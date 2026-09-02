// SPDX-License-Identifier: GPL-3.0-only

//! WMDE screenshot tool.
//!
//! One binary in two shapes. With no arguments the process becomes the service that owns
//! `fun.wmde.Screenshot` and waits; with a subcommand it is a client that hands one
//! request over and exits. The key binding spawns the second, D-Bus starts the first.

mod app;
mod buffer;
mod config;
mod dbus;
mod localize;
mod screenshot;
mod wayland;
mod widget;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "wmde-screenshot", about = "Take a screenshot")]
struct Cli {
    #[command(subcommand)]
    request: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Choose what to capture on screen, then edit it
    Gui,
    /// Every output at once, straight to the configured destination
    Full,
    /// The output under the pointer
    Screen,
    /// The focused window
    Window,
}

impl From<Command> for dbus::Request {
    fn from(command: Command) -> Self {
        match command {
            Command::Gui => Self::Gui,
            Command::Full => Self::Full,
            Command::Screen => Self::Screen,
            Command::Window => Self::Window,
        }
    }
}

fn main() -> cosmic::iced::Result {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    log_panics::init();
    localize::localize();

    match Cli::parse().request {
        Some(command) => {
            request(command.into());
            Ok(())
        }
        None => app::run(),
    }
}

/// The client half: one call, then out. Its own small runtime, because starting the whole
/// toolkit to send four words down a socket would be the delay this design exists to
/// remove.
fn request(request: dbus::Request) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            tracing::error!("no runtime: {err}");
            std::process::exit(1);
        }
    };

    if let Err(err) = runtime.block_on(dbus::call(request)) {
        tracing::error!("{}: {err}", dbus::DBUS_NAME);
        std::process::exit(1);
    }
}
