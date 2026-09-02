// SPDX-License-Identifier: GPL-3.0-only

//! WMDE: the resident half - an iced application that owns no window until asked.

use crate::wayland::WaylandHelper;
use crate::{config, dbus, screenshot};
use cosmic::app;
use cosmic::cctk::sctk::shell::wlr_layer;
use cosmic::iced::core::event::wayland::OutputEvent;
use cosmic::iced::platform_specific::shell::commands::layer_surface::get_layer_surface;
use cosmic::iced::runtime::platform_specific::wayland::layer_surface::{
    IcedMargin, IcedOutput, SctkLayerSurfaceSettings,
};
use cosmic::iced::{Length, Limits, Subscription, event, window};
use cosmic::widget;
use futures::SinkExt;
use futures::channel::mpsc;
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::{Connection, Proxy};

pub fn run() -> cosmic::iced::Result {
    // Two halves of the same decision. `no_main_window` because the program draws only
    // when a request arrives, and `exit_on_close` because the overlay is destroyed after
    // every shot - a service that quit with its last surface would be a process per key
    // press again, which is what the bus was chosen to avoid.
    let settings = cosmic::app::Settings::default()
        .no_main_window(true)
        .exit_on_close(false);

    let (config, config_handler) = config::Config::load();

    cosmic::app::run::<Screenshot>(
        settings,
        Flags {
            config,
            config_handler,
        },
    )
}

pub struct Flags {
    pub config: config::Config,
    pub config_handler: Option<cosmic::cosmic_config::Config>,
}

/// One output as the overlay needs to know it: where it sits in the layout, which surface
/// covers it, and what is behind it.
#[derive(Debug, Clone)]
pub struct OutputState {
    pub output: WlOutput,
    pub id: window::Id,
    pub name: String,
    pub logical_size: (u32, u32),
    pub logical_pos: (i32, i32),
    pub has_pointer: bool,
    pub bg_source: Option<cosmic_bg_config::Source>,
}

pub struct Screenshot {
    pub core: app::Core,
    pub config: config::Config,
    config_handler: Option<cosmic::cosmic_config::Config>,
    /// Every output the compositor has told us about, in no particular order.
    pub outputs: Vec<OutputState>,
    pub active_output: Option<WlOutput>,
    /// Built from the first output event: the wayland connection the toolkit already has
    /// is the one the capture goes through, so there is no second connection to keep.
    pub wayland_helper: Option<WaylandHelper>,
    /// Frames in hand and the current choice. `Some` exactly while the overlay is up.
    pub screenshot_args: Option<screenshot::Args>,
    pub location_options: Vec<String>,
    /// Last rectangle, so a second shot of the same place starts where the first ended.
    pub prev_rectangle: Option<screenshot::Rect>,
    /// A request that arrived before the wayland side was up. See [`Screenshot::request`].
    pending: Option<dbus::Incoming>,
    /// What has been drawn on the shot in hand. Cleared with every new overlay.
    pub editor: crate::editor::Editor,
}

#[derive(Clone, Debug)]
pub enum Message {
    /// A request arrived on the bus.
    Request(dbus::Incoming),
    /// The overlay talking to itself.
    Screenshot(screenshot::Msg),
    /// An output appeared, moved, resized or went away.
    Output(OutputEvent, WlOutput),
    /// Frames are in hand; put the overlay up. Boxed because `Args` carries whole images
    /// and every other variant would have to be as large as this one.
    Captured(Box<screenshot::Args>),
    /// The overlay changed something worth remembering.
    ConfigSet(config::Screenshot),
    /// The outputs have had a moment to arrive; run whatever was held.
    RunPending,
}

impl cosmic::Application for Screenshot {
    type Executor = cosmic::executor::Default;
    type Flags = Flags;
    type Message = Message;

    const APP_ID: &'static str = dbus::DBUS_NAME;

    fn core(&self) -> &app::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut app::Core {
        &mut self.core
    }

    fn init(mut core: app::Core, flags: Flags) -> (Self, cosmic::Task<cosmic::Action<Message>>) {
        core.set_app_type(cosmic::core::AppType::System);

        (
            Self {
                core,
                config: flags.config,
                config_handler: flags.config_handler,
                outputs: Vec::new(),
                active_output: None,
                wayland_helper: None,
                screenshot_args: None,
                location_options: Vec::new(),
                prev_rectangle: None,
                pending: None,
                editor: crate::editor::Editor::default(),
            },
            dummy_surface(window::Id::unique()),
        )
    }

    fn update(&mut self, message: Message) -> cosmic::Task<cosmic::Action<Message>> {
        match message {
            Message::Request(request) => self.request(request),
            Message::RunPending => match self.pending.take() {
                Some(incoming) => self.request(incoming),
                None => cosmic::Task::none(),
            },
            Message::Captured(args) => {
                if args.interactive {
                    screenshot::update_args(self, *args)
                } else {
                    screenshot::shoot(self, *args)
                }
            }
            Message::Screenshot(msg) => screenshot::update_msg(self, msg),
            Message::Output(event, output) => self.output_event(event, output),
            Message::ConfigSet(screenshot) => {
                match &mut self.config_handler {
                    Some(handler) => {
                        if let Err(err) = self.config.set_screenshot(handler, screenshot) {
                            tracing::error!("cannot save the screenshot settings: {err}");
                        }
                    }
                    None => tracing::error!("cannot save the screenshot settings: no handler"),
                }
                cosmic::Task::none()
            }
        }
    }

    /// Never reached: there is no main window. Answering with nothing is cheaper than a
    /// panic if some path in the toolkit ever asks anyway.
    fn view(&self) -> cosmic::Element<'_, Message> {
        widget::Space::new()
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn view_window(&self, id: window::Id) -> cosmic::Element<'_, Message> {
        if self.outputs.iter().any(|output| output.id == id) {
            screenshot::view(self, id).map(Message::Screenshot)
        } else {
            widget::Space::new()
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            Subscription::run(bus),
            // Output geometry is not asked for, it arrives: the toolkit reports every
            // output as the compositor announces it, and the overlay needs all of them
            // before it can cover the desktop.
            event::listen_with(|event, _, _| match event {
                cosmic::iced::Event::PlatformSpecific(event::PlatformSpecific::Wayland(
                    event::wayland::Event::Output(output_event, output),
                )) => Some(Message::Output(output_event, output)),
                _ => None,
            }),
        ])
    }
}

impl Screenshot {
    /// Take the frames a request needs, then hand them to the overlay. Capture happens
    /// before anything is drawn: the shot has to show the desktop as it was at the key
    /// press, not the overlay that came up to ask about it.
    fn request(&mut self, incoming: dbus::Incoming) -> cosmic::Task<cosmic::Action<Message>> {
        let request = incoming.request;

        if self.screenshot_args.is_some() {
            tracing::info!("{request:?} ignored: the overlay is already up");
            dbus::answer(
                &incoming.reply,
                Err("a screenshot is already in progress".into()),
            );
            return cosmic::Task::none();
        }

        // The first request of a session arrives while D-Bus is still starting this
        // process, which is before the toolkit has reported a single output. Holding it
        // is the whole reason `pending` exists: dropping it would mean the first press of
        // every session does nothing, and only the first.
        if self.wayland_helper.is_none() || self.outputs.is_empty() {
            tracing::info!("{request:?} held until the outputs arrive");
            self.pending = Some(incoming);
            return cosmic::Task::none();
        }

        let wayland_helper = self.wayland_helper.clone().unwrap();

        let outputs = self.outputs.clone();
        let config = self.config.screenshot.clone();
        let active = self.active_output.clone();

        cosmic::Task::perform(
            screenshot::capture(wayland_helper, outputs, config, active, incoming),
            |args| match args {
                Some(args) => cosmic::action::app(Message::Captured(Box::new(args))),
                None => cosmic::action::none(),
            },
        )
    }

    fn output_event(
        &mut self,
        event: OutputEvent,
        output: WlOutput,
    ) -> cosmic::Task<cosmic::Action<Message>> {
        // The first output event is also where the wayland connection comes from: the
        // toolkit already has one open, and opening a second would mean a second event
        // loop watching the same compositor.
        if self.wayland_helper.is_none()
            && let Some(backend) = output.backend().upgrade()
        {
            self.wayland_helper = Some(WaylandHelper::new(Connection::from_backend(backend)));
        }

        match event {
            OutputEvent::Created(Some(info))
                if info.name.is_some()
                    && info.logical_size.is_some()
                    && info.logical_position.is_some() =>
            {
                self.outputs.push(OutputState {
                    output,
                    id: window::Id::unique(),
                    name: info.name.unwrap(),
                    logical_size: info
                        .logical_size
                        .map(|(w, h)| (w as u32, h as u32))
                        .unwrap(),
                    logical_pos: info.logical_position.unwrap(),
                    has_pointer: false,
                    bg_source: None,
                });
            }
            OutputEvent::Removed => self.outputs.retain(|o| o.output != output),
            OutputEvent::InfoUpdate(info)
                if info.name.is_some()
                    && info.logical_size.is_some()
                    && info.logical_position.is_some() =>
            {
                let size = info
                    .logical_size
                    .map(|(w, h)| (w as u32, h as u32))
                    .unwrap();
                let pos = info.logical_position.unwrap();
                let name = info.name.unwrap();
                if let Some(state) = self.outputs.iter_mut().find(|o| o.output == output) {
                    state.name = name;
                    state.logical_size = size;
                    state.logical_pos = pos;
                } else {
                    self.outputs.push(OutputState {
                        output,
                        id: window::Id::unique(),
                        name,
                        logical_size: size,
                        logical_pos: pos,
                        has_pointer: false,
                        bg_source: None,
                    });
                }
            }
            other => tracing::debug!("unhandled output event: {other:?}"),
        }

        // Default selection is the whole desktop, so the first shot after a login has a
        // rectangle to start from rather than a zero-sized one.
        if self.prev_rectangle.is_none() && !self.outputs.is_empty() {
            let mut rect = screenshot::Rect::default();
            for output in &self.outputs {
                rect.left = rect.left.min(output.logical_pos.0);
                rect.top = rect.top.min(output.logical_pos.1);
                rect.right = rect
                    .right
                    .max(output.logical_pos.0 + output.logical_size.0 as i32);
                rect.bottom = rect
                    .bottom
                    .max(output.logical_pos.1 + output.logical_size.1 as i32);
            }
            self.prev_rectangle = Some(rect);
        }

        // Outputs are announced one at a time and nothing says "that was the last one",
        // so a held request waits a moment more rather than shooting the first monitor
        // that turns up and calling that the desktop. Several of these may be scheduled;
        // only the first finds anything to take.
        if self.pending.is_some() && self.wayland_helper.is_some() && !self.outputs.is_empty() {
            return cosmic::Task::perform(
                tokio::time::sleep(std::time::Duration::from_millis(200)),
                |()| cosmic::action::app(Message::RunPending),
            );
        }

        cosmic::Task::none()
    }
}

/// A six-pixel surface with an empty input zone, taken once and never let go. The toolkit
/// needs one surface to keep the wayland side alive between shots, and an empty input zone
/// is what stops those six pixels from taking a click away from anything. wmde-portal
/// holds the same thing for the same reason.
fn dummy_surface(id: window::Id) -> cosmic::Task<cosmic::Action<Message>> {
    get_layer_surface(SctkLayerSurfaceSettings {
        id,
        layer: wlr_layer::Layer::Bottom,
        keyboard_interactivity: wlr_layer::KeyboardInteractivity::None,
        input_zone: Some(Vec::new()),
        anchor: wlr_layer::Anchor::empty(),
        output: IcedOutput::All,
        namespace: "wmde_screenshot_dummy".into(),
        margin: IcedMargin::default(),
        size: Some((Some(6), Some(6))),
        exclusive_zone: -1,
        size_limits: Limits::NONE,
    })
}

/// Own the bus name and turn every call into a message. Runs for the life of the process.
fn bus() -> impl futures::Stream<Item = Message> {
    cosmic::iced::stream::channel(4, |mut output: mpsc::Sender<Message>| async move {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);

        // Bound for the whole stream on purpose: dropping the connection drops the name,
        // and the next `Print` would start a second copy of this program.
        let _connection = match serve(tx).await {
            Ok(connection) => connection,
            Err(err) => {
                tracing::error!("cannot own {}: {err}", dbus::DBUS_NAME);
                // Stalling rather than returning: a subscription that ends is restarted,
                // and retrying a taken name in a tight loop helps nobody.
                futures::future::pending::<()>().await;
                return;
            }
        };

        while let Some(incoming) = rx.recv().await {
            if output.send(Message::Request(incoming)).await.is_err() {
                return;
            }
        }
    })
}

async fn serve(tx: tokio::sync::mpsc::Sender<dbus::Incoming>) -> zbus::Result<zbus::Connection> {
    let connection = zbus::connection::Builder::session()?
        .serve_at(dbus::DBUS_PATH, dbus::Service::new(tx))?
        .build()
        .await?;

    connection.request_name(dbus::DBUS_NAME).await?;

    Ok(connection)
}
