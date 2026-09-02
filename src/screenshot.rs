#![allow(dead_code, unused_variables)]

use cosmic::cosmic_config::CosmicConfigEntry;
use cosmic::iced::clipboard::mime::AsMimeTypes;
use cosmic::iced::keyboard::Key;
use cosmic::iced::keyboard::key::Named;
use cosmic::iced::platform_specific::shell::commands::layer_surface::destroy_layer_surface;
use cosmic::iced::runtime::clipboard;
use cosmic::iced::runtime::platform_specific::wayland::layer_surface::{
    IcedOutput, SctkLayerSurfaceSettings,
};
use cosmic::iced::{Length, Limits, window};
use cosmic::widget::space;
use cosmic_client_toolkit::sctk::shell::wlr_layer::{Anchor, KeyboardInteractivity, Layer};
use futures::stream::{FuturesUnordered, StreamExt};
use image::RgbaImage;
use rustix::fd::AsFd;
use std::borrow::Cow;
use std::collections::HashMap;
use std::io;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use wayland_client::protocol::wl_output::WlOutput;

use crate::app::{Message, OutputState, Screenshot as App};
use crate::config::{self, ImageSaveLocation};
use crate::dbus::Request;
use crate::fl;
use crate::wayland::{CaptureSource, ShmImage, WaylandHelper};
use crate::widget::keyboard_wrapper::KeyboardWrapper;
use crate::widget::rectangle_selection::DragState;

#[derive(Clone, Debug)]
pub struct ScreenshotImage {
    pub rgba: RgbaImage,
    pub handle: cosmic::widget::image::Handle,
}

impl ScreenshotImage {
    fn new<T: AsFd>(img: ShmImage<T>) -> anyhow::Result<Self> {
        let rgba = img.image_transformed()?;
        let handle = cosmic::widget::image::Handle::from_rgba(
            rgba.width(),
            rgba.height(),
            rgba.clone().into_vec(),
        );
        Ok(Self { rgba, handle })
    }

    pub fn width(&self) -> u32 {
        self.rgba.width()
    }

    pub fn height(&self) -> u32 {
        self.rgba.height()
    }
}

/// A captured PNG on its way to the clipboard.
struct ScreenshotBytes {
    bytes: Vec<u8>,
}

impl ScreenshotBytes {
    fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }
}

impl AsMimeTypes for ScreenshotBytes {
    fn available(&self) -> std::borrow::Cow<'static, [String]> {
        Cow::Owned(vec!["image/png".to_string()])
    }

    fn as_bytes(&self, mime_type: &str) -> Option<std::borrow::Cow<'static, [u8]>> {
        Some(Cow::Owned(self.bytes.clone()))
    }
}

/// Logical Size and Position of a rectangle
#[derive(Clone, Copy, Debug, Default)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    fn intersect(&self, other: Rect) -> Option<Rect> {
        let left = self.left.max(other.left);
        let top = self.top.max(other.top);
        let right = self.right.min(other.right);
        let bottom = self.bottom.min(other.bottom);
        if left < right && top < bottom {
            Some(Rect {
                left,
                top,
                right,
                bottom,
            })
        } else {
            None
        }
    }

    fn translate(&self, x: i32, y: i32) -> Rect {
        Rect {
            left: self.left + x,
            top: self.top + y,
            right: self.right + x,
            bottom: self.bottom + y,
        }
    }

    fn width(&self) -> i32 {
        self.right - self.left
    }

    fn height(&self) -> i32 {
        self.bottom - self.top
    }

    pub fn dimensions(self) -> Option<RectDimension> {
        let width = NonZeroU32::new((self.width()).unsigned_abs())?;
        let height = NonZeroU32::new((self.height()).unsigned_abs())?;
        Some(RectDimension { width, height })
    }
}

#[derive(Clone, Copy)]
pub struct RectDimension {
    width: NonZeroU32,
    height: NonZeroU32,
}

/// Everything that reaches for pixels. Holds the wayland side and nothing else: what to
/// do with a frame is the overlay's business, not the capturer's.
pub struct Capturer {
    wayland_helper: WaylandHelper,
}

impl Capturer {
    pub fn new(wayland_helper: WaylandHelper) -> Self {
        Self { wayland_helper }
    }

    fn active_toplevel_index(&self, output: &WlOutput) -> Option<usize> {
        self.wayland_helper.active_toplevel_index(output)
    }

    async fn interactive_toplevel_images(
        &self,
        outputs: &[Output],
    ) -> anyhow::Result<HashMap<String, Vec<ScreenshotImage>>> {
        let wayland_helper = self.wayland_helper.clone();
        Ok(outputs
            .iter()
            .map(move |Output { output, name, .. }| {
                let wayland_helper = wayland_helper.clone();
                async move {
                    let frame = wayland_helper
                        .capture_output_toplevels_shm(output, false)
                        .filter_map(|img| async { ScreenshotImage::new(img).ok() })
                        .collect()
                        .await;
                    (name.clone(), frame)
                }
            })
            .collect::<FuturesUnordered<_>>()
            .collect::<HashMap<String, _>>()
            .await)
    }

    async fn interactive_output_images(
        &self,
        outputs: &[Output],
    ) -> anyhow::Result<HashMap<String, ScreenshotImage>> {
        // collect screenshots from each output

        let wayland_helper = self.wayland_helper.clone();

        let mut map = HashMap::with_capacity(outputs.len());
        for Output {
            output,
            logical_position: (output_x, output_y),
            name,
            ..
        } in outputs
        {
            let frame = wayland_helper
                .capture_source_shm(CaptureSource::Output(output.clone()), false)
                .await
                .ok_or_else(|| anyhow::anyhow!("shm screencopy failed"))?;
            map.insert(name.clone(), ScreenshotImage::new(frame)?);
        }

        Ok(map)
    }

    pub fn save_rgba(img: &RgbaImage, path: Option<&Path>) -> anyhow::Result<Vec<u8>> {
        // Write to the buffer first since the image data will always be copied to the clipboard.
        // This skips encoding the PNG twice.
        let mut buffer = Vec::new();
        write_png(&mut buffer, img)?;

        if let Some(path) = path {
            std::fs::write(path, &buffer)?;
        }

        Ok(buffer)
    }

    pub fn get_img_path(location: ImageSaveLocation, filename: &str) -> Option<PathBuf> {
        let mut path = match location {
            ImageSaveLocation::Pictures => {
                // First check for XDG_SCREENSHOTS_DIR environment variable
                std::env::var_os("XDG_SCREENSHOTS_DIR")
                    .map(PathBuf::from)
                    .filter(|p| p.is_absolute())
                    .or_else(|| {
                        // Fall back to XDG_PICTURES_DIR/Screenshots or ~/Pictures/Screenshots
                        dirs::picture_dir()
                            .or_else(|| dirs::home_dir().map(|h| h.join("Pictures")))
                            .map(|p| p.join("Screenshots"))
                    })
            }
            ImageSaveLocation::Documents => {
                dirs::document_dir().or_else(|| dirs::home_dir().map(|h| h.join("Documents")))
            }
            ImageSaveLocation::Clipboard => None,
        }?;

        // Ensure the directory exists
        if let Err(err) = std::fs::create_dir_all(&path) {
            tracing::error!("Failed to create screenshot directory {:?}: {}", path, err);
            return None;
        }

        path.push(jiff::Zoned::now().strftime(filename).to_string());

        Some(path)
    }

    async fn screenshot_inner(&self, outputs: &[Output], app_id: &str) -> anyhow::Result<PathBuf> {
        let wayland_helper = self.wayland_helper.clone();

        let mut bounds_opt: Option<Rect> = None;
        let mut frames = Vec::with_capacity(outputs.len());
        for Output {
            output,
            logical_position: (output_x, output_y),
            logical_size: (output_w, output_h),
            ..
        } in outputs
        {
            let frame = wayland_helper
                .capture_source_shm(CaptureSource::Output(output.clone()), false)
                .await
                .ok_or_else(|| anyhow::anyhow!("shm screencopy failed"))?;
            let frame_image = frame.image_transformed()?;
            let rect = Rect {
                left: *output_x,
                top: *output_y,
                right: output_x.saturating_add(*output_w),
                bottom: output_y.saturating_add(*output_h),
            };
            bounds_opt = Some(match bounds_opt.take() {
                Some(bounds) => Rect {
                    left: bounds.left.min(rect.left),
                    top: bounds.top.min(rect.top),
                    right: bounds.right.max(rect.right),
                    bottom: bounds.bottom.max(rect.bottom),
                },
                None => rect,
            });
            frames.push((frame_image, rect));
        }

        let (file, path) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let image = combined_image(bounds_opt.unwrap_or_default(), frames);

            let mut file = tempfile::Builder::new()
                .prefix("screenshot-")
                .suffix(".png")
                .tempfile()?;
            {
                write_png(&mut file, &image)?;
            }
            Ok(file.keep()?)
        })
        .await??;

        Ok(path)
    }
}

fn combined_image(bounds: Rect, frames: Vec<(RgbaImage, Rect)>) -> RgbaImage {
    // If we have only one image, crop without scaling
    if frames.len() == 1 {
        let (frame_image, rect) = &frames[0];

        // TODO Don't have explicit scale factor; how to ensure pixel perfect scaling?
        let width_scale = frame_image.width() as f64 / rect.width() as f64;
        let height_scale = frame_image.height() as f64 / rect.height() as f64;

        let width = (bounds.width() as f64 * width_scale).max(0.) as u32;
        let height = (bounds.height() as f64 * height_scale).max(0.) as u32;
        let x = ((bounds.left - rect.left) as f64 * width_scale).max(0.) as u32;
        let y = ((bounds.top - rect.top) as f64 * height_scale).max(0.) as u32;

        return image::imageops::crop_imm(frame_image, x, y, width, height).to_image();
    }

    let width = bounds
        .right
        .saturating_sub(bounds.left)
        .try_into()
        .unwrap_or_default();
    let height = bounds
        .bottom
        .saturating_sub(bounds.top)
        .try_into()
        .unwrap_or_default();
    let mut image = image::RgbaImage::new(width, height);
    for (mut frame_image, rect) in frames {
        let width = rect.width() as u32;
        let height = rect.height() as u32;
        if frame_image.dimensions() != (width, height) {
            frame_image = image::imageops::resize(
                &frame_image,
                width,
                height,
                image::imageops::FilterType::Lanczos3,
            );
        };
        let x = i64::from(rect.left) - i64::from(bounds.left);
        let y = i64::from(rect.top) - i64::from(bounds.top);
        image::imageops::overlay(&mut image, &frame_image, x, y);
    }
    image
}

fn write_png<W: io::Write>(w: W, image: &RgbaImage) -> Result<(), png::EncodingError> {
    let mut encoder = png::Encoder::new(w, image.width(), image.height());
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(image.as_raw())
}

#[derive(Debug, Clone)]
pub enum Msg {
    Capture,
    CaptureWithLocation(ImageSaveLocation),
    Cancel,
    Choice(Choice),
    OutputChanged(WlOutput),
    WindowChosen(String, usize),
    Location(usize),
}

#[derive(Debug, Clone)]
pub enum Choice {
    Output(String),
    Rectangle(Rect, DragState),
    Window(String, Option<usize>),
}

impl From<&Choice> for config::Choice {
    // Using a reference here to avoid requiring a temporary `Choice` that's only consumed
    fn from(value: &Choice) -> Self {
        match value {
            Choice::Window(..) => config::Choice::Window,
            Choice::Rectangle(..) => config::Choice::Rectangle,
            Choice::Output(output) => config::Choice::Output(Some(output.clone())),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub enum Action {
    #[default]
    ReturnPath,
    SaveToClipboard,
    SaveToPictures,
    SaveToDocuments,
    ChooseFolder, // TODO use document portal to choose folder
    Choice(Choice),
}

#[derive(Clone, Debug)]
pub struct Args {
    /// One frame per output, keyed by output name.
    pub output_images: HashMap<String, ScreenshotImage>,
    /// Frames of the windows on each output, in stacking order.
    pub toplevel_images: HashMap<String, Vec<ScreenshotImage>>,
    pub choice: Choice,
    pub location: ImageSaveLocation,
    /// Whether the overlay is put up at all. `full`, `screen` and `window` shoot straight
    /// from the key press; only `gui` asks.
    pub interactive: bool,
}

struct Output {
    output: WlOutput,
    logical_position: (i32, i32),
    logical_size: (i32, i32),
    name: String,
}

pub(crate) fn view(app: &App, id: window::Id) -> cosmic::Element<'_, Msg> {
    let Some((i, output)) = app.outputs.iter().enumerate().find(|(i, o)| o.id == id) else {
        return space::horizontal().width(Length::Fixed(1.0)).into();
    };
    let Some(args) = app.screenshot_args.as_ref() else {
        return space::horizontal().width(Length::Fixed(1.0)).into();
    };

    let Some(img) = args.output_images.get(&output.name) else {
        return space::horizontal().width(Length::Fixed(1.0)).into();
    };
    let theme = app.core.system_theme().cosmic();
    KeyboardWrapper::new(
        crate::widget::screenshot::ScreenshotSelection::new(
            args.choice.clone(),
            img,
            Msg::Capture,
            Msg::Cancel,
            output,
            id,
            Msg::OutputChanged,
            Msg::Choice,
            &args.toplevel_images,
            Msg::WindowChosen,
            &app.location_options,
            args.location as usize,
            Msg::Location,
            theme.spacing,
            i as u128,
        ),
        |key, modifiers| {
            if modifiers.control() {
                match key {
                    Key::Named(Named::Copy) => {
                        return Some(Msg::CaptureWithLocation(ImageSaveLocation::Clipboard));
                    }
                    Key::Named(Named::Save) => {
                        return Some(Msg::CaptureWithLocation(ImageSaveLocation::Pictures));
                    }
                    Key::Character(ref value) => {
                        let value = value.as_str();
                        if value.eq_ignore_ascii_case("c") {
                            return Some(Msg::CaptureWithLocation(ImageSaveLocation::Clipboard));
                        } else if value.eq_ignore_ascii_case("s") {
                            return Some(Msg::CaptureWithLocation(ImageSaveLocation::Pictures));
                        }
                    }
                    _ => {}
                }
            }

            match key {
                Key::Named(Named::Enter) => Some(Msg::Capture),
                Key::Named(Named::Escape) => Some(Msg::Cancel),
                _ => None,
            }
        },
    )
    .into()
}

/// Turn frames plus a choice into a file on disk, bytes on the clipboard, or both. The
/// returned tasks are the clipboard writes - the only part of saving that iced has to do.
///
/// Shared by both ways in: the overlay's capture button, and the requests that never put an
/// overlay up at all.
fn save(
    outputs: &[OutputState],
    args: Args,
    filename: &str,
) -> Vec<cosmic::Task<cosmic::Action<Message>>> {
    let mut cmds: Vec<cosmic::Task<cosmic::Action<Message>>> = Vec::new();
    let outputs = outputs.to_vec();
    let Args {
        choice,
        output_images: mut images,
        location,
        ..
    } = args;

    let mut success = true;
    let image_path = Capturer::get_img_path(location, filename);

    match choice {
        Choice::Output(name) => {
            if let Some(img) = images.remove(&name) {
                if let Ok(buffer) = Capturer::save_rgba(&img.rgba, image_path.as_deref())
                    .inspect_err(|err| {
                        tracing::error!("Failed to capture screenshot: {:?}", err);
                        success = false;
                    })
                {
                    cmds.push(clipboard::write_data(ScreenshotBytes::new(buffer)));
                }
            } else {
                tracing::error!("Failed to find output {}", name);
                success = false;
            }
        }
        Choice::Rectangle(r, s) => {
            if let Some(RectDimension { width, height }) = r.dimensions() {
                // Construct Rgba image with size of rect
                // then overlay the part of each image that intersects with the rect
                //let mut img = RgbaImage::new(width.get(), height.get());

                let frames = images
                    .into_iter()
                    .filter_map(|(name, raw_img)| {
                        let output = outputs.iter().find(|o| o.name == name)?;
                        let pos = output.logical_pos;
                        let output_rect = Rect {
                            left: pos.0,
                            top: pos.1,
                            right: pos.0 + output.logical_size.0 as i32,
                            bottom: pos.1 + output.logical_size.1 as i32,
                        };

                        let intersect = r.intersect(output_rect)?;

                        Some((raw_img.rgba, output_rect))
                    })
                    .collect::<Vec<_>>();
                let img = combined_image(r, frames);

                if let Ok(buffer) =
                    Capturer::save_rgba(&img, image_path.as_deref()).inspect_err(|err| {
                        tracing::error!("Failed to capture screenshot: {:?}", err);
                        success = false;
                    })
                {
                    cmds.push(clipboard::write_data(ScreenshotBytes::new(buffer)));
                }
            } else {
                success = false;
            }
        }
        Choice::Window(output, Some(window_i)) => {
            if let Some(img) = args
                .toplevel_images
                .get(&output)
                .and_then(|imgs| imgs.get(window_i))
            {
                if let Ok(buffer) = Capturer::save_rgba(&img.rgba, image_path.as_deref())
                    .inspect_err(|err| {
                        tracing::error!("Failed to capture screenshot: {:?}", err);
                        success = false;
                    })
                {
                    cmds.push(clipboard::write_data(ScreenshotBytes::new(buffer)));
                }
            } else {
                success = false;
            }
        }
        _ => {
            success = false;
        }
    }

    announce(success, image_path.as_deref());

    cmds
}

pub fn update_msg(app: &mut App, msg: Msg) -> cosmic::Task<cosmic::Action<Message>> {
    match msg {
        Msg::Capture => {
            let mut cmds: Vec<cosmic::Task<cosmic::Action<Message>>> = app
                .outputs
                .iter()
                .map(|o| destroy_layer_surface(o.id))
                .collect();
            let Some(args) = app.screenshot_args.take() else {
                tracing::error!("Failed to find screenshot Args for Capture message.");
                return cosmic::Task::batch(cmds);
            };
            cmds.extend(save(&app.outputs, args, &app.config.screenshot.filename));
            cosmic::Task::batch(cmds)
        }
        Msg::CaptureWithLocation(location) => {
            if let Some(args) = app.screenshot_args.as_mut() {
                args.location = location;
            } else {
                tracing::error!("Failed to find screenshot Args for CaptureWithLocation message.");
                return cosmic::Task::none();
            }
            update_msg(app, Msg::Capture)
        }
        Msg::Cancel => {
            let cmds = app.outputs.iter().map(|o| destroy_layer_surface(o.id));
            if app.screenshot_args.take().is_none() {
                tracing::error!("cancelled with no shot in progress");
            }

            cosmic::Task::batch(cmds)
        }
        Msg::Choice(c) => {
            let choice = (&c).into();
            // Only save config when drag is finished to avoid disk writes on every mouse motion
            let should_save_config =
                !matches!(&c, Choice::Rectangle(_, s) if *s != DragState::None);
            let last_rect = if let Choice::Rectangle(r, _) = &c {
                app.prev_rectangle = Some(*r);
                Some(config::Rect {
                    left: r.left,
                    top: r.top,
                    right: r.right,
                    bottom: r.bottom,
                })
            } else {
                app.config.screenshot.last_rectangle
            };

            if let Some(args) = app.screenshot_args.as_mut() {
                args.choice = c;
            } else {
                tracing::error!("Failed to find screenshot Args for Choice message.");
            }
            if should_save_config {
                cosmic::task::message(Message::ConfigSet(config::Screenshot {
                    choice,
                    last_rectangle: last_rect,
                    ..app.config.screenshot.clone()
                }))
            } else {
                cosmic::Task::none()
            }
        }
        Msg::OutputChanged(wl_output) => {
            if let (Some(args), Some(o)) = (
                app.screenshot_args.as_mut(),
                app.outputs
                    .iter()
                    .find(|o| o.output == wl_output)
                    .map(|o| o.name.clone()),
            ) {
                args.choice = Choice::Output(o);
            } else {
                tracing::error!(
                    "Failed to find output for OutputChange message: {:?}",
                    wl_output
                );
            }
            app.active_output = Some(wl_output);
            cosmic::Task::none()
        }
        Msg::WindowChosen(name, i) => {
            if let Some(args) = app.screenshot_args.as_mut() {
                args.choice = Choice::Window(name, Some(i));
            } else {
                tracing::error!("Failed to find screenshot Args for WindowChosen message.");
            }
            update_msg(app, Msg::Capture)
        }
        Msg::Location(loc) => {
            if let Some(args) = app.screenshot_args.as_mut() {
                let loc = match loc {
                    loc if loc == ImageSaveLocation::Clipboard as usize => {
                        ImageSaveLocation::Clipboard
                    }
                    loc if loc == ImageSaveLocation::Pictures as usize => {
                        ImageSaveLocation::Pictures
                    }
                    loc if loc == ImageSaveLocation::Documents as usize => {
                        ImageSaveLocation::Documents
                    }
                    _ => args.location,
                };
                args.location = loc;
                cosmic::task::message(Message::ConfigSet(config::Screenshot {
                    save_location: loc,
                    ..app.config.screenshot.clone()
                }))
            } else {
                tracing::error!("Failed to find screenshot Args for Location message.");
                cosmic::Task::none()
            }
        }
    }
}

pub fn update_args(app: &mut App, args: Args) -> cosmic::Task<cosmic::Action<Message>> {
    let Args {
        output_images: images,
        ..
    } = &args;

    if app.outputs.len() != images.len() {
        tracing::error!(
            "Screenshot output count mismatch: {} != {}",
            app.outputs.len(),
            images.len()
        );
        tracing::warn!("Screenshot outputs: {:?}", app.outputs);
        tracing::warn!("Screenshot images: {:?}", images.keys().collect::<Vec<_>>());
        return cosmic::Task::none();
    }

    // update output bg sources
    if let Ok(c) = cosmic::cosmic_config::Config::new_state(
        cosmic_bg_config::NAME,
        cosmic_bg_config::state::State::version(),
    ) {
        let bg_state = match cosmic_bg_config::state::State::get_entry(&c) {
            Ok(state) => state,
            Err((err, s)) => {
                tracing::error!("Failed to get bg config state: {:?}", err);
                s
            }
        };
        for o in &mut app.outputs {
            let source = bg_state.wallpapers.iter().find(|s| s.0 == o.name);
            o.bg_source = Some(source.cloned().map(|s| s.1).unwrap_or_else(|| {
                cosmic_bg_config::Source::Path(
                    "/usr/share/backgrounds/wmde/wmde-default.jpg".into(),
                )
            }));
        }
    } else {
        tracing::error!("Failed to get bg config state");
        for o in &mut app.outputs {
            o.bg_source = Some(cosmic_bg_config::Source::Path(
                "/usr/share/backgrounds/wmde/wmde-default.jpg".into(),
            ));
        }
    }
    app.location_options = vec![
        fl!("save-to", "clipboard"),
        fl!("save-to", "pictures"),
        fl!("save-to", "documents"),
    ];

    if app.screenshot_args.replace(args).is_none() {
        // iterate over outputs and create a layer surface for each
        let cmds: Vec<_> = app
            .outputs
            .iter()
            .map(|state: &OutputState| {
                let id = state.id;
                let output = state.output.clone();
                let name = state.name.clone();
                cosmic::surface::surface_task::<Message>(
                    cosmic::surface::action::simple_layer_shell::<Message>(
                        Default::default,
                        move || SctkLayerSurfaceSettings {
                            id,
                            layer: Layer::Overlay,
                            keyboard_interactivity: KeyboardInteractivity::Exclusive,
                            input_zone: None,
                            anchor: Anchor::all(),
                            output: IcedOutput::Output(output.clone()),
                            namespace: "screenshot".to_string(),
                            size: Some((None, None)),
                            exclusive_zone: -1,
                            size_limits: Limits::NONE.min_height(1.0).min_width(1.0),
                            ..Default::default()
                        },
                        None::<fn() -> cosmic::Element<'static, cosmic::Action<Message>>>,
                    ),
                )
            })
            .collect();
        cosmic::Task::batch(cmds)
    } else {
        tracing::info!("Existing screenshot args updated");
        cosmic::Task::none()
    }
}

/// Take the frames one request needs. `None` means nothing was captured and there is
/// nothing to show: the caller logs it and drops the request.
///
/// Capture happens before any surface is created, so the frames hold the desktop as it
/// was at the key press rather than the overlay that came up to ask about it.
pub async fn capture(
    wayland_helper: WaylandHelper,
    outputs: Vec<OutputState>,
    config: config::Screenshot,
    active: Option<WlOutput>,
    request: Request,
) -> Option<Args> {
    let capturer = Capturer::new(wayland_helper);
    let outputs: Vec<Output> = outputs
        .iter()
        .map(|state| Output {
            output: state.output.clone(),
            logical_position: state.logical_pos,
            logical_size: (state.logical_size.0 as i32, state.logical_size.1 as i32),
            name: state.name.clone(),
        })
        .collect();

    let output_images = capturer
        .interactive_output_images(&outputs)
        .await
        .inspect_err(|err| tracing::error!("cannot capture the outputs: {err}"))
        .ok()?;

    // Windows are only worth the extra captures when something can pick one.
    let toplevel_images = match request {
        Request::Gui | Request::Window => capturer
            .interactive_toplevel_images(&outputs)
            .await
            .inspect_err(|err| tracing::error!("cannot capture the windows: {err}"))
            .unwrap_or_default(),
        _ => HashMap::new(),
    };

    let first = outputs.first()?;
    let active_name = active
        .and_then(|active| outputs.iter().find(|o| o.output == active))
        .map(|o| o.name.clone());

    // What the overlay opens on. A named request says it outright; `Gui` resumes whatever
    // the last shot chose, because taking two shots of the same thing is the common case.
    let choice = match request {
        Request::Screen => Choice::Output(active_name.unwrap_or_else(|| first.name.clone())),
        Request::Full => Choice::Rectangle(desktop_rect(&outputs), DragState::None),
        // The window in focus, by index into the frames just taken of that output. No
        // focused window means nothing to shoot, and the request is dropped rather than
        // guessed at.
        Request::Window => {
            let name = active_name.clone().unwrap_or_else(|| first.name.clone());
            let output = outputs.iter().find(|o| o.name == name)?;
            let index = capturer.active_toplevel_index(&output.output)?;
            Choice::Window(name, Some(index))
        }
        Request::Gui => match &config.choice {
            config::Choice::Output(Some(name)) if outputs.iter().any(|o| &o.name == name) => {
                Choice::Output(name.clone())
            }
            config::Choice::Output(_) => {
                Choice::Output(active_name.clone().unwrap_or_else(|| first.name.clone()))
            }
            // No index: the overlay opens on the window picker and the person chooses.
            config::Choice::Window => Choice::Window(
                active_name.clone().unwrap_or_else(|| first.name.clone()),
                None,
            ),
            config::Choice::Rectangle => Choice::Rectangle(
                config
                    .last_rectangle
                    .map(|r| Rect {
                        left: r.left,
                        top: r.top,
                        right: r.right,
                        bottom: r.bottom,
                    })
                    .unwrap_or_else(|| desktop_rect(&outputs)),
                DragState::None,
            ),
        },
    };

    Some(Args {
        output_images,
        toplevel_images,
        choice,
        location: config.save_location,
        interactive: matches!(request, Request::Gui),
    })
}

/// Shoot without asking. Used by `full`, `screen` and `window`, where the request already
/// says what to take.
pub fn shoot(app: &App, args: Args) -> cosmic::Task<cosmic::Action<Message>> {
    cosmic::Task::batch(save(&app.outputs, args, &app.config.screenshot.filename))
}

/// The rectangle that covers every output.
fn desktop_rect(outputs: &[Output]) -> Rect {
    let mut rect = Rect::default();
    for output in outputs {
        let (x, y) = output.logical_position;
        let (w, h) = output.logical_size;
        rect.left = rect.left.min(x);
        rect.top = rect.top.min(y);
        rect.right = rect.right.max(x + w);
        rect.bottom = rect.bottom.max(y + h);
    }
    rect
}

/// Tell the person what happened. A shot saved to a file with no word about it is
/// indistinguishable from a shot that failed - both leave the screen exactly as it was.
fn announce(success: bool, path: Option<&Path>) {
    let (summary, body) = match (success, path) {
        (true, Some(path)) => {
            tracing::info!("screenshot saved to {}", path.display());
            (fl!("saved"), path.display().to_string())
        }
        (true, None) => {
            tracing::info!("screenshot copied to the clipboard");
            (fl!("copied"), String::new())
        }
        (false, _) => {
            tracing::error!("screenshot failed");
            (fl!("failed"), String::new())
        }
    };

    // Blocking: notify-rust talks to the notification daemon over D-Bus, and the overlay
    // is being torn down on this thread.
    tokio::task::spawn_blocking(move || {
        if let Err(err) = notify_rust::Notification::new()
            .appname("WMDE")
            .summary(&summary)
            .body(&body)
            .icon("applets-screenshooter")
            .show()
        {
            tracing::warn!("cannot show the notification: {err}");
        }
    });
}
