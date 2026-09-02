// SPDX-License-Identifier: GPL-3.0-only

//! WMDE: the bus face of the screenshot service.
//!
//! The program is one resident service rather than a process per key press: `Print`
//! reaches something that is already running, so nothing is spent on starting the toolkit
//! between the press and the overlay. The first request of a session starts it through the
//! activation file in `/usr/share/dbus-1/services/`, and it stays up for the rest.

use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot;

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

/// The way back to a caller that is waiting for the file. Empty for a key press: nobody
/// is waiting, and the shot goes wherever the settings say.
///
/// Wrapped rather than held plainly because it travels inside an iced message, and those
/// have to be `Clone` - which a one-shot sender is not, by its nature.
pub type Reply = Arc<Mutex<Option<oneshot::Sender<Result<String, String>>>>>;

/// A request on its way to the application half, and the way back if anyone is waiting.
#[derive(Clone, Debug)]
pub struct Incoming {
    pub request: Request,
    pub reply: Reply,
}

/// Answer the waiting caller, if there is one. Sending twice is impossible: the sender is
/// taken out on the first answer.
pub fn answer(reply: &Reply, result: Result<String, String>) {
    if let Some(tx) = reply.lock().unwrap().take()
        && tx.send(result).is_err()
    {
        tracing::debug!("nobody was left waiting for the screenshot");
    }
}

/// A reply nobody is waiting for.
pub fn no_reply() -> Reply {
    Arc::new(Mutex::new(None))
}

/// The served object. It holds nothing but the way back to the application half: the bus
/// runs on its own task, and everything that draws has to happen on the iced side.
pub struct Service {
    tx: Sender<Incoming>,
}

impl Service {
    pub fn new(tx: Sender<Incoming>) -> Self {
        Self { tx }
    }

    /// A full channel means the previous request has not been picked up yet. Refusing is
    /// better than queueing: a person cannot want four overlays in a row, so the extra
    /// presses are noise, and answering the caller with an error beats making it wait.
    async fn hand_over(&self, request: Request) -> zbus::fdo::Result<()> {
        self.tx
            .try_send(Incoming {
                request,
                reply: no_reply(),
            })
            .map_err(|err| {
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

    /// Take a shot and answer with where it went. This is the one method with a return
    /// value, and it exists for `wmde-portal`: the portal owes its own caller a URI, so
    /// something has to wait for the file to be on disk.
    ///
    /// The file is a temporary one whatever the settings say. The application asked for an
    /// image to read, not for a copy in the person's pictures folder.
    async fn shot(&self, interactive: bool) -> zbus::fdo::Result<String> {
        let (tx, rx) = oneshot::channel();
        let request = if interactive {
            Request::Gui
        } else {
            Request::Full
        };

        self.tx
            .try_send(Incoming {
                request,
                reply: Arc::new(Mutex::new(Some(tx))),
            })
            .map_err(|err| {
                tracing::warn!("dropping {request:?}: {err}");
                zbus::fdo::Error::Failed(format!("{request:?} was not taken"))
            })?;

        match rx.await {
            Ok(Ok(uri)) => Ok(uri),
            Ok(Err(err)) => Err(zbus::fdo::Error::Failed(err)),
            // The application half dropped the sender: the shot was cancelled, or the
            // overlay went away with the session.
            Err(_) => Err(zbus::fdo::Error::Failed("no screenshot was taken".into())),
        }
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
