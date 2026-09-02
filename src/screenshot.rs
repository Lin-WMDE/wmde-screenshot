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

    fn toplevel_rects(&self, output: &WlOutput) -> Vec<Option<Rect>> {
        self.wayland_helper.output_toplevel_rects(output)
    }

    fn active_output(&self) -> Option<WlOutput> {
        self.wayland_helper.active_output()
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

    /// A file that outlives this process but belongs to nobody: what a portal caller is
    /// handed. Kept rather than deleted on drop - the caller reads it after we answer.
    pub fn temp_path() -> Option<PathBuf> {
        tempfile::Builder::new()
            .prefix("screenshot-")
            .suffix(".png")
            .tempfile()
            .and_then(|file| file.keep().map_err(Into::into))
            .inspect_err(|err| tracing::error!("no temporary file: {err}"))
            .ok()
            .map(|(_file, path)| path)
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
    /// The pointer on the drawing layer.
    Draw(crate::widget::annotations::Draw),
    /// Pick a tool, or `None` to go back to moving the selection.
    Tool(Option<crate::editor::Tool>),
    Color(cosmic::iced::Color),
    Undo,
    Redo,
    /// Turn the pointer readout on or off.
    Eyedropper,
    /// Take the shot down and ask where to put it.
    ChooseFolder,
    /// The person picked a file. `None` means they changed their mind.
    SaveTo(Option<PathBuf>),
    /// A character typed into the text being edited.
    Type(char),
    Backspace,
    /// Stop typing, keeping what was typed.
    FinishText,
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
    /// One frame per output, keyed by output name. Pristine: what the screen looked like
    /// at the key press, and what the file is built from.
    pub output_images: HashMap<String, ScreenshotImage>,
    /// The frames the overlay shows, when they differ from the pristine ones - which is
    /// exactly when something has been blurred or pixelated. `None` the rest of the time,
    /// so a shot without those tools carries one copy of the screen and not two.
    pub preview: Option<HashMap<String, ScreenshotImage>>,
    /// Frames of the windows on each output, in stacking order.
    pub toplevel_images: HashMap<String, Vec<ScreenshotImage>>,
    /// Where each of those windows sits on the desktop, in the same order. `None` for a
    /// window the compositor gave no geometry for; marks are simply not baked into that
    /// one rather than landing somewhere arbitrary.
    pub toplevel_rects: HashMap<String, Vec<Option<Rect>>>,
    pub choice: Choice,
    pub location: ImageSaveLocation,
    /// Whether the overlay is put up at all. `full`, `screen` and `window` shoot straight
    /// from the key press; only `gui` asks.
    pub interactive: bool,
    /// Where the answer goes when a caller is waiting for the file.
    pub reply: crate::dbus::Reply,
    /// Save to a temporary file rather than the configured place. An application that
    /// asked the portal for a screenshot wants an image to read, not a copy in the
    /// person's pictures folder.
    pub to_temp: bool,
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

    let frames = args.preview.as_ref().unwrap_or(&args.output_images);
    let Some(img) = frames.get(&output.name) else {
        return space::horizontal().width(Length::Fixed(1.0)).into();
    };
    let theme = app.core.system_theme().cosmic();
    let editing = app.editor.editing();
    KeyboardWrapper::new(
        crate::widget::screenshot::ScreenshotSelection::new(
            args.choice.clone(),
            img,
            Msg::Capture,
            output,
            id,
            Msg::OutputChanged,
            Msg::Choice,
            &args.toplevel_images,
            Msg::WindowChosen,
            theme.spacing,
            i as u128,
            // The drawing layer. It answers to the pointer only while a tool is in hand;
            // otherwise the press belongs to the selection underneath.
            cosmic::Element::<crate::widget::annotations::Draw>::from(
                cosmic::widget::canvas(crate::widget::annotations::Annotations {
                    editor: &app.editor,
                    frame: Some(&img.rgba),
                    origin: cosmic::iced::Vector::new(
                        output.logical_pos.0 as f32,
                        output.logical_pos.1 as f32,
                    ),
                })
                .width(Length::Fill)
                .height(Length::Fill),
            )
            .map(Msg::Draw),
            editor_bar(app, output),
        ),
        move |key, modifiers| {
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
                        } else if value.eq_ignore_ascii_case("z") {
                            return Some(if modifiers.shift() {
                                Msg::Redo
                            } else {
                                Msg::Undo
                            });
                        }
                    }
                    _ => {}
                }
            }

            // While a text is being typed the keyboard belongs to it. Otherwise every
            // letter would change the tool instead of appearing on the shot.
            if editing {
                return match key {
                    Key::Character(ref value) => value.as_str().chars().next().map(Msg::Type),
                    Key::Named(Named::Backspace) => Some(Msg::Backspace),
                    Key::Named(Named::Enter | Named::Escape) => Some(Msg::FinishText),
                    _ => None,
                };
            }

            match key {
                Key::Named(Named::Enter) => Some(Msg::Capture),
                // Escape leaves the tool first and the overlay second: a person who
                // picked the wrong tool should not lose the shot to say so.
                Key::Named(Named::Escape) => Some(Msg::Cancel),
                // One letter per tool, no modifier. The overlay has the keyboard to
                // itself, and reaching for a modifier is a tax on every stroke.
                Key::Character(ref value) => {
                    let c = value.as_str().chars().next()?.to_ascii_lowercase();
                    if c == 'i' {
                        return Some(Msg::Eyedropper);
                    }
                    crate::editor::Tool::from_key(c).map(|tool| Msg::Tool(Some(tool)))
                }
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
/// Where a shot is going.
pub enum Destination {
    /// Wherever the settings say, under the name the settings give.
    Configured,
    /// A temporary file, for a caller that asked the portal for an image.
    Temp,
    /// A file the person picked.
    Path(PathBuf),
}

fn save(
    outputs: &[OutputState],
    args: Args,
    filename: &str,
    objects: &[crate::editor::Object],
    dest: Destination,
) -> Vec<cosmic::Task<cosmic::Action<Message>>> {
    let mut cmds: Vec<cosmic::Task<cosmic::Action<Message>>> = Vec::new();
    let outputs = outputs.to_vec();
    let Args {
        choice,
        output_images: mut images,
        location,
        reply,
        to_temp,
        ..
    } = args;

    let mut success = true;
    let image_path = match dest {
        Destination::Path(path) => Some(path),
        Destination::Temp => Capturer::temp_path(),
        Destination::Configured => Capturer::get_img_path(location, filename),
    };

    match choice {
        Choice::Output(name) => {
            if let Some(mut img) = images.remove(&name) {
                if let Some(output) = outputs.iter().find(|o| o.name == name) {
                    crate::editor::bake(
                        &mut img.rgba,
                        objects,
                        Rect {
                            left: output.logical_pos.0,
                            top: output.logical_pos.1,
                            right: output.logical_pos.0 + output.logical_size.0 as i32,
                            bottom: output.logical_pos.1 + output.logical_size.1 as i32,
                        },
                    );
                }
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
                let mut img = combined_image(r, frames);
                crate::editor::bake(&mut img, objects, r);

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
                // The window's own place on the desktop is what maps a mark onto its
                // frame. Without it the marks are left off rather than guessed at.
                let mut rgba = img.rgba.clone();
                if let Some(Some(rect)) = args
                    .toplevel_rects
                    .get(&output)
                    .and_then(|rects| rects.get(window_i))
                {
                    crate::editor::bake(&mut rgba, objects, *rect);
                }
                let img = ScreenshotImage {
                    handle: img.handle.clone(),
                    rgba,
                };
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

    match (success, image_path.as_deref()) {
        (true, Some(path)) => {
            crate::dbus::answer(&reply, Ok(format!("file://{}", path.display())));
        }
        // Nothing on disk to name. A caller waiting for a file gets the honest answer
        // rather than a URI that leads nowhere.
        (true, None) => {
            crate::dbus::answer(&reply, Err("the screenshot went to the clipboard".into()));
        }
        (false, _) => crate::dbus::answer(&reply, Err("the screenshot failed".into())),
    }

    // A caller that asked for a file has its own way of showing the result; the
    // notification is for the person who pressed a key.
    if !to_temp {
        announce(success, image_path.as_deref());
    }

    cmds
}

pub fn update_msg(app: &mut App, msg: Msg) -> cosmic::Task<cosmic::Action<Message>> {
    match msg {
        Msg::Draw(draw) => {
            use crate::widget::annotations::Draw;
            match draw {
                Draw::Press(at) => app.editor.press(at),
                Draw::Drag(at) => app.editor.drag(at),
                Draw::Release => {
                    app.editor.release();
                    refresh_preview(app);
                }
                Draw::Width(steps) => app.editor.nudge_width(steps),
            }
            cosmic::Task::none()
        }
        Msg::Tool(tool) => {
            app.editor.tool = tool;
            cosmic::Task::none()
        }
        Msg::Color(color) => {
            app.editor.color = color;
            cosmic::Task::none()
        }
        Msg::ChooseFolder => {
            // The overlay comes down first. It holds the keyboard exclusively, so a file
            // dialog opened underneath it could be looked at and not typed into.
            let mut cmds: Vec<cosmic::Task<cosmic::Action<Message>>> = app
                .outputs
                .iter()
                .map(|o| destroy_layer_surface(o.id))
                .collect();

            app.editor.finish_text();
            app.pending_save = app.screenshot_args.take();

            let name = jiff::Zoned::now()
                .strftime(&app.config.screenshot.filename)
                .to_string();
            cmds.push(cosmic::Task::perform(
                async move {
                    cosmic::dialog::file_chooser::save::file(
                        cosmic::dialog::file_chooser::save::Dialog::new()
                            .title(fl!("save-to"))
                            .file_name(name),
                    )
                    .await
                    .ok()
                    .and_then(|response| response.url().and_then(|url| url.to_file_path().ok()))
                },
                |path| cosmic::action::app(Message::Screenshot(Msg::SaveTo(path))),
            ));

            cosmic::Task::batch(cmds)
        }
        Msg::SaveTo(path) => {
            let Some(args) = app.pending_save.take() else {
                tracing::error!("nothing was waiting to be saved");
                return cosmic::Task::none();
            };
            let Some(path) = path else {
                tracing::info!("the save was called off");
                crate::dbus::answer(&args.reply, Err("cancelled".into()));
                app.editor = crate::editor::Editor::default();
                return cosmic::Task::none();
            };

            let objects: Vec<_> = app.editor.objects().cloned().collect();
            let cmds = save(
                &app.outputs,
                args,
                &app.config.screenshot.filename,
                &objects,
                Destination::Path(path),
            );
            app.editor = crate::editor::Editor::default();
            cosmic::Task::batch(cmds)
        }
        Msg::Eyedropper => {
            app.editor.eyedropper = !app.editor.eyedropper;
            cosmic::Task::none()
        }
        Msg::Undo => {
            app.editor.undo();
            refresh_preview(app);
            cosmic::Task::none()
        }
        Msg::Redo => {
            app.editor.redo();
            refresh_preview(app);
            cosmic::Task::none()
        }
        Msg::Type(c) => {
            app.editor.type_char(c);
            cosmic::Task::none()
        }
        Msg::Backspace => {
            app.editor.backspace();
            cosmic::Task::none()
        }
        Msg::FinishText => {
            app.editor.finish_text();
            cosmic::Task::none()
        }
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
            app.editor.finish_text();
            let objects: Vec<_> = app.editor.objects().cloned().collect();
            let dest = if args.to_temp {
                Destination::Temp
            } else {
                Destination::Configured
            };
            cmds.extend(save(
                &app.outputs,
                args,
                &app.config.screenshot.filename,
                &objects,
                dest,
            ));
            app.editor = crate::editor::Editor::default();
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
            match app.screenshot_args.take() {
                // A caller waiting for a file is told the person said no, rather than
                // being left holding a call that never returns.
                Some(args) => crate::dbus::answer(&args.reply, Err("cancelled".into())),
                None => tracing::error!("cancelled with no shot in progress"),
            }

            cosmic::Task::batch(cmds)
        }
        Msg::Choice(c) => {
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
                    // One past the three places: the entry that asks.
                    _ => return cosmic::task::message(Message::Screenshot(Msg::ChooseFolder)),
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
        fl!("choose-folder"),
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
    incoming: crate::dbus::Incoming,
) -> Option<Args> {
    let request = incoming.request;
    // The compositor knows which window has focus, and that is a better answer than
    // whatever the toolkit last reported as the surface under the pointer.
    let active = wayland_helper.active_output().or(active);
    // Asked before the value moves into `Args`: someone waiting for the file is what makes
    // this a temporary one.
    let to_temp = incoming.reply.lock().unwrap().is_some();
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
        // The overlay always opens on a region. The mode buttons are right there for the
        // other two, and a key that sometimes opens a window picker instead of a selection
        // is a key nobody can press without looking.
        Request::Gui => Choice::Rectangle(
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
    };

    let toplevel_rects = outputs
        .iter()
        .map(|output| (output.name.clone(), capturer.toplevel_rects(&output.output)))
        .collect();

    Some(Args {
        output_images,
        toplevel_rects,
        preview: None,
        toplevel_images,
        choice,
        // An instant key puts the picture where it can be pasted. Choosing a folder is
        // what the overlay is for, and these three never show one.
        location: match request {
            Request::Gui => config.save_location,
            _ => ImageSaveLocation::Clipboard,
        },
        interactive: matches!(request, Request::Gui),
        to_temp,
        reply: incoming.reply,
    })
}

/// Shoot without asking. Used by `full`, `screen` and `window`, where the request already
/// says what to take.
pub fn shoot(app: &App, args: Args) -> cosmic::Task<cosmic::Action<Message>> {
    // Nothing was drawn: this path never put an overlay up for anyone to draw on.
    let dest = if args.to_temp {
        Destination::Temp
    } else {
        Destination::Configured
    };
    cosmic::Task::batch(save(
        &app.outputs,
        args,
        &app.config.screenshot.filename,
        &[],
        dest,
    ))
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
        // Nothing to say. The notification exists to answer two questions - where the
        // file went, and whether it failed - and the clipboard answers the first one by
        // itself at the next paste. Three instant keys that each throw a toast are noise.
        (true, None) => {
            tracing::info!("screenshot copied to the clipboard");
            return;
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

/// The drawing half of the toolbar: what to draw with, in what colour, how thick, and the
/// two arrows that take it back.
///
/// Round filled buttons in a single row with nothing behind them, as the reference has
/// them. A panel would put a second rectangle beside the selection and make the strip
/// heavier than the picture it sits on.
fn editor_bar(app: &App, output: &OutputState) -> cosmic::Element<'static, Msg> {
    use crate::editor::Tool;
    use cosmic::iced::{Alignment, Background, Border};
    use cosmic::widget::{button, container, row, space};

    let theme = app.core.system_theme().cosmic();
    let spacing = theme.spacing;
    let accent = cosmic::iced::Color::from(theme.accent_color());

    let tool_button = |tool: Tool, name: &'static str| {
        let chosen = app.editor.tool == Some(tool);
        round_button(name, chosen).on_press(Msg::Tool((!chosen).then_some(tool)))
    };

    let swatch = |color: cosmic::iced::Color| {
        let chosen = color == app.editor.color;
        button::custom(
            container(space::horizontal().width(Length::Fixed(SWATCH)))
                .height(Length::Fixed(SWATCH))
                .class(cosmic::theme::Container::custom(move |_| {
                    cosmic::iced::widget::container::Style {
                        background: Some(Background::Color(color)),
                        border: Border {
                            radius: (SWATCH / 2.0).into(),
                            // The chosen colour is the one with a ring: a swatch cannot
                            // show selection by changing colour.
                            width: if chosen { 2.0 } else { 0.0 },
                            color: cosmic::iced::Color::WHITE,
                        },
                        ..Default::default()
                    }
                })),
        )
        .class(cosmic::theme::Button::Icon)
        .padding(2)
        .on_press(Msg::Color(color))
    };

    let mut bar = row::with_capacity(32)
        .align_y(Alignment::Center)
        .spacing(spacing.space_xxs);

    // What to take, before what to draw on it.
    let has_region = matches!(app.editor.tool, None) && matches!(&app.screenshot_args, Some(_));
    let _ = has_region;
    for (name, chosen, message) in [
        (
            "wmde-shot-region-symbolic",
            matches!(current_choice(app), Some(Choice::Rectangle(..))),
            Msg::Choice(Choice::Rectangle(
                app.prev_rectangle.unwrap_or_default(),
                DragState::None,
            )),
        ),
        (
            "wmde-shot-window-symbolic",
            matches!(current_choice(app), Some(Choice::Window(..))),
            Msg::Choice(Choice::Window(output.name.clone(), None)),
        ),
        (
            "wmde-shot-monitor-symbolic",
            matches!(current_choice(app), Some(Choice::Output(..))),
            Msg::Choice(Choice::Output(output.name.clone())),
        ),
    ] {
        bar = bar.push(round_button(name, chosen).on_press(message));
    }

    for (tool, name) in [
        (Tool::Pencil, "wmde-shot-pencil-symbolic"),
        (Tool::Line, "wmde-shot-line-symbolic"),
        (Tool::Arrow, "wmde-shot-arrow-symbolic"),
        (Tool::Rectangle, "wmde-shot-rectangle-symbolic"),
        (Tool::Marker, "wmde-shot-marker-symbolic"),
        (Tool::Text, "wmde-shot-text-symbolic"),
        (Tool::Counter, "wmde-shot-counter-symbolic"),
        (Tool::Blur, "wmde-shot-blur-symbolic"),
        (Tool::Pixelate, "wmde-shot-pixelate-symbolic"),
        (Tool::Move, "wmde-shot-move-symbolic"),
    ] {
        bar = bar.push(tool_button(tool, name));
    }

    bar = bar.push(
        round_button("wmde-shot-eyedropper-symbolic", app.editor.eyedropper).on_press(Msg::Eyedropper),
    );

    for color in crate::editor::palette(accent) {
        bar = bar.push(swatch(color));
    }

    bar = bar
        .push(round_button("wmde-shot-undo-symbolic", false).on_press(Msg::Undo))
        .push(round_button("wmde-shot-redo-symbolic", false).on_press(Msg::Redo));

    // Where it goes. Three buttons rather than a list to choose from: the reference has
    // copy and save as buttons, and a list is one more click for the two things anyone
    // does with a screenshot.
    let ready =
        !matches!(current_choice(app), Some(Choice::Rectangle(r, _)) if r.dimensions().is_none());

    bar.push(
        round_button("wmde-shot-copy-symbolic", false).on_press_maybe(
            ready.then_some(Msg::CaptureWithLocation(ImageSaveLocation::Clipboard)),
        ),
    )
    .push(
        round_button("wmde-shot-save-symbolic", false)
            .on_press_maybe(ready.then_some(Msg::CaptureWithLocation(ImageSaveLocation::Pictures))),
    )
    .push(
        round_button("wmde-shot-save-as-symbolic", false)
            .on_press_maybe(ready.then_some(Msg::ChooseFolder)),
    )
    .push(round_button("wmde-shot-close-symbolic", false).on_press(Msg::Cancel))
    .into()
}

/// What the overlay is set to take right now.
fn current_choice(app: &App) -> Option<&Choice> {
    app.screenshot_args.as_ref().map(|args| &args.choice)
}

/// Diameter of a toolbar button, and of a colour swatch beside it.
const BUTTON: f32 = 34.0;
const SWATCH: f32 = 18.0;

/// One round button: a filled circle with a white glyph, as the reference draws them. The
/// chosen one is filled with the accent; the rest sit on a dark disc so that a light
/// screenshot behind them cannot swallow the glyph.
fn round_button(name: &'static str, chosen: bool) -> cosmic::widget::Button<'static, Msg> {
    fn face(
        theme: &cosmic::Theme,
        background: cosmic::iced::Color,
    ) -> cosmic::widget::button::Style {
        let mut style = cosmic::widget::button::Style::new();
        style.background = Some(background.into());
        style.icon_color = Some(cosmic::iced::Color::WHITE);
        style.text_color = Some(cosmic::iced::Color::WHITE);
        style.border_radius = (BUTTON / 2.0).into();
        let _ = theme;
        style
    }

    let fill = move |theme: &cosmic::Theme, lighten: f32| {
        let cosmic = theme.cosmic();
        if chosen {
            cosmic::iced::Color::from(cosmic.accent_color())
        } else {
            let base = cosmic::iced::Color::from(cosmic.background(false).base);
            cosmic::iced::Color {
                r: base.r + lighten,
                g: base.g + lighten,
                b: base.b + lighten,
                a: 0.92,
            }
        }
    };

    cosmic::widget::button::custom(
        cosmic::widget::icon::Icon::from(cosmic::widget::icon::from_name(name).size(48))
            .width(Length::Fixed(18.0))
            .height(Length::Fixed(18.0)),
    )
    .width(Length::Fixed(BUTTON))
    .height(Length::Fixed(BUTTON))
    .padding(0)
    .class(cosmic::theme::Button::Custom {
        active: Box::new(move |_focused, theme| face(theme, fill(theme, 0.0))),
        disabled: Box::new(move |theme| face(theme, fill(theme, 0.0))),
        hovered: Box::new(move |_focused, theme| face(theme, fill(theme, 0.10))),
        pressed: Box::new(move |_focused, theme| face(theme, fill(theme, 0.16))),
    })
}

/// Rebuild the frames the overlay shows, when blur or pixelation has changed them.
///
/// Always from the pristine frames, never from the last preview: undo has to be able to
/// take a blur back, and a blur applied to an already blurred frame cannot be undone by
/// applying less of it.
fn refresh_preview(app: &mut App) {
    let Some(args) = app.screenshot_args.as_mut() else {
        return;
    };

    if !app.editor.has_raster() {
        // Nothing changes the picture any more, so the second copy of the screen goes.
        args.preview = None;
        return;
    }

    let objects: Vec<_> = app.editor.objects().cloned().collect();
    let mut preview = HashMap::with_capacity(args.output_images.len());

    for output in &app.outputs {
        let Some(pristine) = args.output_images.get(&output.name) else {
            continue;
        };
        let mut rgba = pristine.rgba.clone();
        crate::editor::apply_raster(
            &mut rgba,
            &objects,
            Rect {
                left: output.logical_pos.0,
                top: output.logical_pos.1,
                right: output.logical_pos.0 + output.logical_size.0 as i32,
                bottom: output.logical_pos.1 + output.logical_size.1 as i32,
            },
        );
        let handle = cosmic::widget::image::Handle::from_rgba(
            rgba.width(),
            rgba.height(),
            rgba.clone().into_vec(),
        );
        preview.insert(output.name.clone(), ScreenshotImage { rgba, handle });
    }

    args.preview = Some(preview);
}
