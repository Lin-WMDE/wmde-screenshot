use async_io::Timer;
use cosmic_client_toolkit::screencopy::{
    CaptureCursorSession, CaptureFrame, CaptureOptions, CaptureSession, Capturer, FailureReason,
    Formats, Frame, ScreencopyFrameData, ScreencopyFrameDataExt, ScreencopyHandler,
    ScreencopySessionData, ScreencopySessionDataExt, ScreencopyState,
};
use cosmic_client_toolkit::sctk::output::{OutputHandler, OutputInfo, OutputState};
use cosmic_client_toolkit::sctk::registry::{ProvidesRegistryState, RegistryState};
use cosmic_client_toolkit::sctk::shm::{Shm, ShmHandler};
use cosmic_client_toolkit::sctk::{self};
use cosmic_client_toolkit::toplevel_info::{ToplevelInfo, ToplevelInfoState};
use cosmic_client_toolkit::workspace::WorkspaceState;
use cosmic_protocols::toplevel_info::v1::client::zcosmic_toplevel_handle_v1;
use futures::channel::oneshot;
use futures::future::{Either, select};
use futures::stream::{FuturesOrdered, Stream, StreamExt};
use std::collections::HashMap;
use std::future::Future;
use std::os::fd::{AsFd, OwnedFd};
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread;
use std::time::Duration;
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_buffer, wl_output, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1;
use wayland_protocols::ext::workspace::v1::client::ext_workspace_handle_v1;

pub use cosmic_client_toolkit::screencopy::{CaptureSource, Rect};

use crate::buffer;

mod toplevel;
mod workspaces;

/// Upper bound on how long a single capture step (waiting for `Formats`, or for a
/// captured frame) may block. A healthy capture completes in well under a second;
/// reaching this means the wayland dispatch loop stalled or died, and we return an
/// error instead of hanging the portal request (and its D-Bus caller) forever.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);

/// Run `fut`, or return `None` if `CAPTURE_TIMEOUT` elapses first. Uses async-io's
/// timer, which drives itself off its own reactor, so this works under any executor
/// (tokio for the Screenshot path, `futures::executor::block_on` for the ScreenCast
/// thread).
async fn with_timeout<T>(fut: impl Future<Output = T>) -> Option<T> {
    let fut = pin!(fut);
    let timer = pin!(Timer::after(CAPTURE_TIMEOUT));
    match select(fut, timer).await {
        Either::Left((out, _)) => Some(out),
        Either::Right(_) => None,
    }
}

fn wl_transform_to_image_orientation(
    transform: wl_output::Transform,
) -> image::metadata::Orientation {
    match transform {
        wl_output::Transform::Normal => image::metadata::Orientation::NoTransforms,
        wl_output::Transform::_90 => image::metadata::Orientation::Rotate90,
        wl_output::Transform::_180 => image::metadata::Orientation::Rotate180,
        wl_output::Transform::_270 => image::metadata::Orientation::Rotate270,
        wl_output::Transform::Flipped => image::metadata::Orientation::FlipHorizontal,
        wl_output::Transform::Flipped90 => image::metadata::Orientation::Rotate90FlipH,
        wl_output::Transform::Flipped180 => image::metadata::Orientation::FlipVertical,
        wl_output::Transform::Flipped270 => image::metadata::Orientation::Rotate270FlipH,
        _ => unreachable!(),
    }
}

struct WaylandHelperInner {
    conn: wayland_client::Connection,
    outputs: Mutex<Vec<wl_output::WlOutput>>,
    output_infos: Mutex<HashMap<wl_output::WlOutput, OutputInfo>>,
    output_toplevels: Mutex<HashMap<wl_output::WlOutput, Vec<ExtForeignToplevelHandleV1>>>,
    toplevels: Mutex<Vec<ToplevelInfo>>,
    qh: QueueHandle<AppData>,
    capturer: Capturer,
    wl_shm: wl_shm::WlShm,
    /// `false` once the dispatch thread has exited (see `WaylandHelper::new`); capture
    /// entry points check this to fail fast instead of awaiting events that will never
    /// be dispatched.
    event_loop_alive: AtomicBool,
}

#[derive(Clone)]
pub struct WaylandHelper {
    inner: Arc<WaylandHelperInner>,
}

/// Clears `event_loop_alive` when dropped, so the flag is reset whether the dispatch
/// thread exits cleanly (a dispatch error) or a wayland event handler panics and
/// unwinds the thread. Holds its own `WaylandHelper` clone so it can still reach the
/// flag even after the thread's `AppData` has been dropped.
struct EventLoopGuard(WaylandHelper);

impl Drop for EventLoopGuard {
    fn drop(&mut self) {
        self.0.inner.event_loop_alive.store(false, Ordering::SeqCst);
    }
}

struct AppData {
    wayland_helper: WaylandHelper,
    registry_state: RegistryState,
    screencopy_state: ScreencopyState,
    output_state: OutputState,
    shm_state: Shm,
    toplevel_info_state: ToplevelInfoState,
    workspace_state: WorkspaceState,
}

// NOTE: upstream (pop-os 969f3ca) adds a `Drop` impl for `AppData` here that calls
// `process::exit(1)` when the dispatch thread unwinds, relying on the service being
// restarted. WMDE deliberately does NOT take it: our systemd user unit has no
// `Restart=`, so exiting would take the portal down for the whole session instead of
// degrading. `EventLoopGuard` + `event_loop_alive` above already mark the loop dead so
// capture entry points fail fast while Settings/Access keep working.
// Still applies as of the COSMIC 1.5 merge: upstream 1.5 leaves that `Drop` impl
// unchanged, so our removal of it stands.

impl AppData {
    pub fn update_output_toplevels(&self) {
        let toplevels = self.toplevel_info_state.toplevels();
        let mut guard = self
            .wayland_helper
            .inner
            .as_ref()
            .output_toplevels
            .lock()
            .unwrap();
        *guard = toplevels
            .filter_map(|info| {
                let o = self.workspace_state.workspace_groups().find_map(|wg| {
                    wg.workspaces
                        .iter()
                        .filter_map(|handle| self.workspace_state.workspace_info(handle))
                        .find_map(|w| {
                            info.workspace
                                .iter()
                                .any(|x| {
                                    x == &w.handle
                                        && w.state.contains(ext_workspace_handle_v1::State::Active)
                                })
                                .then(|| info.output.iter().cloned().collect::<Vec<_>>())
                        })
                })?;

                Some((o, info.foreign_toplevel.clone()))
            })
            .fold(
                std::collections::HashMap::new(),
                |mut map, (outputs, toplevel)| {
                    for o in outputs {
                        map.entry(o).or_default().push(toplevel.clone());
                    }
                    map
                },
            );

        *self.wayland_helper.inner.toplevels.lock().unwrap() =
            self.toplevel_info_state.toplevels().cloned().collect();
    }
}

#[derive(Default)]
struct SessionState {
    formats: Option<Formats>,
    stopped: bool,
    wakers: Vec<std::task::Waker>,
}

struct SessionInner {
    wayland_helper: WaylandHelper,
    capture_session: CaptureSession,
    condvar: Condvar,
    state: Mutex<SessionState>,
}

pub struct Session(Arc<SessionInner>);

impl Session {
    pub fn for_session(session: &CaptureSession) -> Option<Self> {
        session.data::<SessionData>()?.session.upgrade().map(Self)
    }

    fn update<F: FnOnce(&mut SessionState)>(&self, f: F) {
        let mut state = self.0.state.lock().unwrap();
        f(&mut state);
        for waker in std::mem::take(&mut state.wakers) {
            waker.wake();
        }
        self.0.condvar.notify_all();
    }

    /// Wait for the `Formats` to be sent from the compositor for the stream, and run
    /// a callback with the state mutex locked.
    ///
    /// If formats has not been sent, this will wait until it is received. It returns
    /// `None` if the server has sent `stopped`.
    pub async fn wait_for_formats<T, F: FnMut(&Formats) -> T>(&self, mut cb: F) -> Option<T> {
        let wait = std::future::poll_fn(|context| {
            let mut state = self.0.state.lock().unwrap();
            if state.stopped {
                std::task::Poll::Ready(None)
            } else if let Some(formats) = &state.formats {
                std::task::Poll::Ready(Some(cb(formats)))
            } else {
                state.wakers.push(context.waker().clone());
                std::task::Poll::Pending
            }
        });
        // Bound the wait: if the event loop stalls or dies, `Formats` never arrives and
        // the waker is never woken - without this the caller would hang forever.
        match with_timeout(wait).await {
            Some(result) => result,
            None => {
                tracing::error!(
                    "timed out waiting for capture formats (wayland event loop stalled?)"
                );
                None
            }
        }
    }

    /// Capture to `wl_buffer`, blocking until capture either succeeds or fails
    pub async fn capture_wl_buffer(
        &self,
        buffer: &wl_buffer::WlBuffer,
        buffer_damage: &[Rect],
    ) -> Result<Frame, WEnum<FailureReason>> {
        let (sender, receiver) = oneshot::channel();
        self.0.capture_session.capture(
            buffer,
            buffer_damage,
            &self.0.wayland_helper.inner.qh,
            FrameData {
                frame_data: Default::default(),
                sender: Mutex::new(Some(sender)),
            },
        );
        if let Err(err) = self.0.wayland_helper.inner.conn.flush() {
            tracing::error!("wayland flush failed while requesting capture: {}", err);
            return Err(WEnum::Value(FailureReason::Stopped));
        }

        // TODO: wait for server to release buffer?
        // Assume stopped if frame is dropped without `ready` or `failed`
        // - This can happen if the session object has already been destroyed
        //   when the frame is created.
        // Bounded wait: a dead event loop would otherwise leave `receiver` pending
        // forever, since its sender is only fired from the dispatch thread.
        match with_timeout(receiver).await {
            Some(result) => result.unwrap_or(Err(WEnum::Value(FailureReason::Stopped))),
            None => {
                tracing::error!(
                    "timed out waiting for captured frame (wayland event loop stalled?)"
                );
                Err(WEnum::Value(FailureReason::Stopped))
            }
        }
    }
}

impl WaylandHelper {
    pub fn new(conn: wayland_client::Connection) -> Self {
        // Fatal on purpose: the portal is D-Bus activated inside a running wayland
        // session, so a failure here means a broken/absent compositor socket. Use a
        // diagnostic panic rather than a bare unwrap. (Serving the Settings interface
        // headless - without wayland - would require decoupling the zbus server from
        // the iced app; tracked separately.)
        let (globals, mut event_queue) = registry_queue_init(&conn)
            .expect("wayland registry init failed - portal requires a running wayland compositor");
        let qh = event_queue.handle();
        let registry_state = RegistryState::new(&globals);
        let screencopy_state = ScreencopyState::new(&globals, &qh);
        let shm_state =
            Shm::bind(&globals, &qh).expect("wl_shm unavailable - required for capture buffers");
        let wayland_helper = WaylandHelper {
            inner: Arc::new(WaylandHelperInner {
                conn,
                outputs: Mutex::new(Vec::new()),
                output_infos: Mutex::new(HashMap::new()),
                output_toplevels: Mutex::new(HashMap::new()),
                toplevels: Mutex::new(Vec::new()),
                qh: qh.clone(),
                capturer: screencopy_state.capturer().clone(),
                wl_shm: shm_state.wl_shm().clone(),
                event_loop_alive: AtomicBool::new(true),
            }),
        };
        let mut data = AppData {
            // XXX must be before workspace and toplevel_info
            output_state: OutputState::new(&globals, &qh),
            shm_state,
            wayland_helper: wayland_helper.clone(),
            screencopy_state,
            // XXX must be before toplevel_info
            workspace_state: WorkspaceState::new(&registry_state, &qh),
            toplevel_info_state: ToplevelInfoState::new(&registry_state, &qh),
            registry_state,
        };
        // A flush/roundtrip error here is a real transport or protocol failure on the
        // wayland connection - wl_shm and the screencopy globals exist regardless of
        // GPU/software-rendering, so this is NOT a normal headless condition. It is not
        // worth aborting startup over (the Settings interface needs no wayland, and a
        // later capture attempt surfaces the failure via CAPTURE_TIMEOUT), but log it
        // honestly as a connection/protocol error.
        if let Err(err) = event_queue.flush() {
            tracing::warn!(
                "initial wayland flush failed (connection/protocol error): {}",
                err
            );
        }

        if let Err(err) = event_queue.roundtrip(&mut data) {
            tracing::warn!(
                "initial wayland roundtrip failed (connection/protocol error): {}",
                err
            );
        }

        thread::spawn(move || {
            // Mark the event loop dead on ANY exit - a clean dispatch-error break OR a
            // handler panic unwinding the thread - so capture entry points fail fast
            // instead of awaiting frames that will never arrive. Dropping `data`/
            // `event_queue` on the way out also cancels any in-flight frame senders.
            let _guard = EventLoopGuard(data.wayland_helper.clone());
            loop {
                if let Err(err) = event_queue.blocking_dispatch(&mut data) {
                    tracing::error!(
                        "wayland event dispatch failed, stopping event loop: {}",
                        err
                    );
                    break;
                }
            }
        });

        wayland_helper
    }

    /// Whether the wayland dispatch thread is still running. Once it exits (an
    /// unrecoverable dispatch error) no further wayland events are processed, so
    /// captures must not be attempted.
    pub fn is_event_loop_alive(&self) -> bool {
        self.inner.event_loop_alive.load(Ordering::SeqCst)
    }

    fn set_output_info(&self, output: &wl_output::WlOutput, output_info_opt: Option<OutputInfo>) {
        let mut output_infos = self.inner.output_infos.lock().unwrap();
        match output_info_opt {
            Some(output_info) => {
                output_infos.insert(output.clone(), output_info);
            }
            None => {
                output_infos.remove(output);
            }
        }
    }

    /// Where each window on this output sits on the desktop, in the same order as the
    /// frames `capture_output_toplevels_shm` yields. Marks drawn over a window shot need
    /// it: without a rectangle there is nothing to map desktop coordinates onto.
    pub fn output_toplevel_rects(
        &self,
        output: &wl_output::WlOutput,
    ) -> Vec<Option<crate::screenshot::Rect>> {
        let handles = self
            .inner
            .output_toplevels
            .lock()
            .unwrap()
            .get(output)
            .cloned()
            .unwrap_or_default();
        let toplevels = self.inner.toplevels.lock().unwrap();

        handles
            .iter()
            .map(|handle| {
                let info = toplevels
                    .iter()
                    .find(|info| &info.foreign_toplevel == handle)?;
                let geometry = info.geometry.get(output)?;
                Some(crate::screenshot::Rect {
                    left: geometry.x,
                    top: geometry.y,
                    right: geometry.x + geometry.width,
                    bottom: geometry.y + geometry.height,
                })
            })
            .collect()
    }

    /// Where the focused window sits among the frames `capture_output_toplevels_shm`
    /// yields for this output. Same list in the same order - both walk `output_toplevels` -
    /// so the index addresses the frame of the window that has focus.
    ///
    /// `None` when nothing on this output is focused, which is the honest answer: shooting
    /// "the current window" with no current window is a guess, not a screenshot.
    pub fn active_toplevel_index(&self, output: &wl_output::WlOutput) -> Option<usize> {
        let handles = self
            .inner
            .output_toplevels
            .lock()
            .unwrap()
            .get(output)
            .cloned()?;
        let toplevels = self.inner.toplevels.lock().unwrap();

        handles.iter().position(|handle| {
            toplevels.iter().any(|info| {
                &info.foreign_toplevel == handle
                    && info
                        .state
                        .contains(&zcosmic_toplevel_handle_v1::State::Activated)
            })
        })
    }

    pub fn capture_output_toplevels_shm<'a>(
        &'a self,
        output: &wl_output::WlOutput,
        overlay_cursor: bool,
    ) -> impl Stream<Item = ShmImage<OwnedFd>> + 'a {
        // get the active workspace for this output
        // get the toplevels for that workspace
        // capture each toplevel

        let toplevels = self
            .inner
            .output_toplevels
            .lock()
            .unwrap()
            .get(output)
            .cloned()
            .unwrap_or_default();

        toplevels
            .into_iter()
            .map(|foreign_toplevel| {
                let source = CaptureSource::Toplevel(foreign_toplevel.clone());
                self.capture_source_shm(source, overlay_cursor)
            })
            .collect::<FuturesOrdered<_>>()
            .filter_map(|x| async { x })
    }

    pub fn capture_source_session(&self, source: CaptureSource, overlay_cursor: bool) -> Session {
        Session(Arc::new_cyclic(|weak_session| {
            let options = if overlay_cursor {
                CaptureOptions::PaintCursors
            } else {
                CaptureOptions::empty()
            };
            // Unwrap: cosmic-comp should always support this capture
            let capture_session = self
                .inner
                .capturer
                .create_session(
                    &source,
                    options,
                    &self.inner.qh,
                    SessionData {
                        session: weak_session.clone(),
                        session_data: Default::default(),
                    },
                )
                .unwrap();

            if let Err(err) = self.inner.conn.flush() {
                tracing::warn!(
                    "wayland flush failed while creating capture session: {}",
                    err
                );
            }

            SessionInner {
                wayland_helper: self.clone(),
                capture_session,
                condvar: Condvar::new(),
                state: Default::default(),
            }
        }))
    }

    pub async fn capture_source_shm(
        &self,
        source: CaptureSource,
        overlay_cursor: bool,
    ) -> Option<ShmImage<OwnedFd>> {
        // XXX error type?

        // Fail fast if the event loop is already dead - no point setting up a capture
        // whose frames will never be dispatched.
        if !self.is_event_loop_alive() {
            tracing::error!("wayland event loop is not running; cannot capture output");
            return None;
        }

        let session = self.capture_source_session(source, overlay_cursor);

        // TODO: Check that format has been advertised in `Formats`
        let (width, height) = session
            .wait_for_formats(|formats| formats.buffer_size)
            .await?;

        let fd = buffer::create_memfd(width, height);
        let buffer =
            self.create_shm_buffer(&fd, width, height, width * 4, wl_shm::Format::Abgr8888);

        let damage = &[Rect {
            x: 0,
            y: 0,
            width: width as i32,
            height: height as i32,
        }];
        let res = session.capture_wl_buffer(&buffer, damage).await;
        buffer.destroy();

        if let Ok(frame) = res {
            let transform = match frame.transform {
                WEnum::Value(value) => value,
                WEnum::Unknown(value) => panic!("invalid capture transform: {}", value),
            };
            Some(ShmImage {
                fd,
                width,
                height,
                transform,
            })
        } else {
            None
        }
    }

    pub fn create_shm_buffer<Fd: AsFd>(
        &self,
        fd: &Fd,
        width: u32,
        height: u32,
        stride: u32,
        format: wl_shm::Format,
    ) -> wl_buffer::WlBuffer {
        let pool = self.inner.wl_shm.create_pool(
            fd.as_fd(),
            stride as i32 * height as i32,
            &self.inner.qh,
            (),
        );
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            format,
            &self.inner.qh,
            (),
        );

        pool.destroy();

        buffer
    }
}

pub struct ShmImage<T: AsFd> {
    fd: T,
    pub width: u32,
    pub height: u32,
    pub transform: wl_output::Transform,
}

impl<T: AsFd> ShmImage<T> {
    pub fn image(&self) -> anyhow::Result<image::RgbaImage> {
        let mmap = unsafe { memmap2::Mmap::map(&self.fd.as_fd())? };
        image::RgbaImage::from_raw(self.width, self.height, mmap.to_vec())
            .ok_or_else(|| anyhow::anyhow!("ShmImage had incorrect size"))
    }

    pub fn image_transformed(&self) -> anyhow::Result<image::RgbaImage> {
        let mut image = image::DynamicImage::from(self.image()?);
        image.apply_orientation(wl_transform_to_image_orientation(self.transform));
        match image {
            image::DynamicImage::ImageRgba8(image) => Ok(image),
            _ => unreachable!(),
        }
    }
}

impl ProvidesRegistryState for AppData {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    sctk::registry_handlers!(OutputState);
}

impl ShmHandler for AppData {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm_state
    }
}

impl OutputHandler for AppData {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        let output_info_opt = self.output_state.info(&output);
        self.wayland_helper
            .set_output_info(&output, output_info_opt);

        self.wayland_helper
            .inner
            .outputs
            .lock()
            .unwrap()
            .push(output);
        self.update_output_toplevels();
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        let output_info_opt = self.output_state.info(&output);
        self.wayland_helper
            .set_output_info(&output, output_info_opt);
        self.update_output_toplevels();
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.wayland_helper.set_output_info(&output, None);

        let mut outputs = self.wayland_helper.inner.outputs.lock().unwrap();
        if let Some(idx) = outputs.iter().position(|x| x == &output) {
            outputs.remove(idx);
        } else {
            // Protocol edge case: destroyed an output we were not tracking. Skip rather
            // than panic (a panic here would kill the whole dispatch thread).
            tracing::warn!("output_destroyed for an untracked output; ignoring");
        }
        self.update_output_toplevels();
    }
}

impl ScreencopyHandler for AppData {
    fn screencopy_state(&mut self) -> &mut ScreencopyState {
        &mut self.screencopy_state
    }

    fn init_done(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        session: &CaptureSession,
        formats: &Formats,
    ) {
        if let Some(session) = Session::for_session(session) {
            session.update(|data| {
                data.formats = Some(formats.clone());
            });
        }
    }

    fn stopped(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, session: &CaptureSession) {
        if let Some(session) = Session::for_session(session) {
            session.update(|data| {
                data.stopped = true;
            });
        }
        // TODO signal users of session in some way?
    }

    fn ready(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        screencopy_frame: &CaptureFrame,
        frame: Frame,
    ) {
        if let Some(sender) = screencopy_frame
            .data::<FrameData>()
            .and_then(|data| data.sender.lock().unwrap().take())
        {
            let _ = sender.send(Ok(frame));
        }
    }

    fn failed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        screencopy_frame: &CaptureFrame,
        reason: WEnum<FailureReason>,
    ) {
        if let Some(sender) = screencopy_frame
            .data::<FrameData>()
            .and_then(|data| data.sender.lock().unwrap().take())
        {
            let _ = sender.send(Err(reason));
        }
    }

    // Required by `ScreencopyHandler` and dead here on purpose: a cursor session is what
    // a screencast needs to draw a pointer into a stream. A screenshot either paints the
    // cursor into the frame itself or leaves it out.
    fn cursor_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _session: &CaptureCursorSession,
    ) {
    }

    fn cursor_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _session: &CaptureCursorSession,
    ) {
    }

    fn cursor_position(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _session: &CaptureCursorSession,
        _x: i32,
        _y: i32,
    ) {
    }

    fn cursor_hotspot(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _session: &CaptureCursorSession,
        _x: i32,
        _y: i32,
    ) {
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for AppData {
    fn event(
        _app_data: &mut Self,
        _buffer: &wl_shm_pool::WlShmPool,
        _event: wl_shm_pool::Event,
        _: &(),
        _: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for AppData {
    fn event(
        _app_data: &mut Self,
        _buffer: &wl_buffer::WlBuffer,
        _event: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

struct SessionData {
    session: Weak<SessionInner>,
    session_data: ScreencopySessionData,
}

impl ScreencopySessionDataExt for SessionData {
    fn screencopy_session_data(&self) -> &ScreencopySessionData {
        &self.session_data
    }
}

struct FrameData {
    frame_data: ScreencopyFrameData,
    #[allow(clippy::type_complexity)]
    sender: Mutex<Option<oneshot::Sender<Result<Frame, WEnum<FailureReason>>>>>,
}

impl ScreencopyFrameDataExt for FrameData {
    fn screencopy_frame_data(&self) -> &ScreencopyFrameData {
        &self.frame_data
    }
}

sctk::delegate_shm!(AppData);
sctk::delegate_registry!(AppData);
sctk::delegate_output!(AppData);
cosmic_client_toolkit::delegate_screencopy!(AppData);
