// SPDX-License-Identifier: GPL-3.0-only

//! WMDE: the bus face of the screenshot service.
//!
//! The program is one resident service rather than a process per key press: `Print`
//! reaches something that is already running, so nothing is spent on starting the toolkit
//! between the press and the overlay. The first request of a session starts it through the
//! activation file in `/usr/share/dbus-1/services/`, and it stays up for the rest.

use tokio::sync::mpsc::Sender;

/// Well-known name this service owns.
pub const DBUS_NAME: &str = "fun.wmde.Screenshot";

/// The single object every request arrives at.
pub const DBUS_PATH: &str = "/fun/wmde/Screenshot";

/// What the caller asked for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Request {
    /// Choose what to capture on screen, then edit it.
    Gui,
    /// Every output at once, no interface at all.
    Full,
    /// The output holding the pointer, no interface.
    Screen,
    /// The focused window cropped to its frame, no interface.
    Window,
}

impl Request {
    /// Name of the bus method that asks for this. zbus derives the exported names from
    /// the methods in the interface block below, so these two lists are one list and have
    /// to move together.
    pub const fn method(self) -> &'static str {
        match self {
            Self::Gui => "Gui",
            Self::Full => "Full",
            Self::Screen => "Screen",
            Self::Window => "Window",
        }
    }
}

/// The served object. It holds nothing but the way back to the application half: the bus
/// runs on its own task, and everything that draws has to happen on the iced side.
pub struct Service {
    tx: Sender<Request>,
}

impl Service {
    pub fn new(tx: Sender<Request>) -> Self {
        Self { tx }
    }

    /// A full channel means the previous request has not been picked up yet. Refusing is
    /// better than queueing: a person cannot want four overlays in a row, so the extra
    /// presses are noise, and answering the caller with an error beats making it wait.
    async fn hand_over(&self, request: Request) -> zbus::fdo::Result<()> {
        self.tx.try_send(request).map_err(|err| {
            tracing::warn!("dropping {request:?}: {err}");
            zbus::fdo::Error::Failed(format!("{request:?} was not taken"))
        })
    }
}

#[zbus::interface(name = "fun.wmde.Screenshot")]
impl Service {
    async fn gui(&self) -> zbus::fdo::Result<()> {
        self.hand_over(Request::Gui).await
    }

    async fn full(&self) -> zbus::fdo::Result<()> {
        self.hand_over(Request::Full).await
    }

    async fn screen(&self) -> zbus::fdo::Result<()> {
        self.hand_over(Request::Screen).await
    }

    async fn window(&self) -> zbus::fdo::Result<()> {
        self.hand_over(Request::Window).await
    }
}

/// Ask the running service for a shot, from a second copy of this binary started by the
/// key binding. Waits for the reply rather than firing and forgetting: on the first
/// request of a session the reply is what proves D-Bus finished starting the service, and
/// the client must not exit before that.
pub async fn call(request: Request) -> zbus::Result<()> {
    let connection = zbus::Connection::session().await?;
    let proxy = zbus::Proxy::new(&connection, DBUS_NAME, DBUS_PATH, DBUS_NAME).await?;
    proxy.call(request.method(), &()).await
}
