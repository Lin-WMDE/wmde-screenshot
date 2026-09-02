// SPDX-License-Identifier: GPL-3.0-only

//! WMDE: the resident half - an iced application that owns no window until asked.

use crate::dbus;
use cosmic::app;
use cosmic::cctk::sctk::shell::wlr_layer;
use cosmic::iced::platform_specific::shell::commands::layer_surface::get_layer_surface;
use cosmic::iced::runtime::platform_specific::wayland::layer_surface::{
    IcedMargin, IcedOutput, SctkLayerSurfaceSettings,
};
use cosmic::iced::{Length, Limits, Subscription, window};
use cosmic::widget;
use futures::SinkExt;
use futures::channel::mpsc;

pub fn run() -> cosmic::iced::Result {
    // Two halves of the same decision. `no_main_window` because the program draws only
    // when a request arrives, and `exit_on_close` because the overlay is destroyed after
    // every shot - a service that quit with its last surface would be a process per key
    // press again, which is what the bus was chosen to avoid.
    let settings = cosmic::app::Settings::default()
        .no_main_window(true)
        .exit_on_close(false);

    cosmic::app::run::<Screenshot>(settings, ())
}

pub struct Screenshot {
    core: app::Core,
}

#[derive(Clone, Debug)]
pub enum Message {
    /// A request arrived on the bus.
    Request(dbus::Request),
}

impl cosmic::Application for Screenshot {
    type Executor = cosmic::executor::Default;
    type Flags = ();
    type Message = Message;

    const APP_ID: &'static str = dbus::DBUS_NAME;

    fn core(&self) -> &app::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut app::Core {
        &mut self.core
    }

    fn init(mut core: app::Core, _flags: ()) -> (Self, cosmic::Task<cosmic::Action<Message>>) {
        core.set_app_type(cosmic::core::AppType::System);

        (Self { core }, dummy_surface(window::Id::unique()))
    }

    fn update(&mut self, message: Message) -> cosmic::Task<cosmic::Action<Message>> {
        match message {
            // Everything this program is for hangs off this arm; the capture and the
            // editor land here in the stages after this one.
            Message::Request(request) => {
                tracing::info!("{request:?}: not built yet");
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

    fn view_window(&self, _id: window::Id) -> cosmic::Element<'_, Message> {
        widget::Space::new()
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::run(bus)
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

        while let Some(request) = rx.recv().await {
            if output.send(Message::Request(request)).await.is_err() {
                return;
            }
        }
    })
}

async fn serve(tx: tokio::sync::mpsc::Sender<dbus::Request>) -> zbus::Result<zbus::Connection> {
    let connection = zbus::connection::Builder::session()?
        .serve_at(dbus::DBUS_PATH, dbus::Service::new(tx))?
        .build()
        .await?;

    connection.request_name(dbus::DBUS_NAME).await?;

    Ok(connection)
}
