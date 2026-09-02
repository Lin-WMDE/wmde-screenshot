// SPDX-License-Identifier: GPL-3.0-only

//! WMDE: what the program remembers between shots.
//!
//! Carried over from `cosmic-portal-config`, where these values lived while the portal
//! owned the screenshot. The identifier is ours: the settings belong to the program that
//! takes the shot, not to the portal that forwards the request.

use cosmic_config::CosmicConfigEntry;
use cosmic_config::cosmic_config_derive::CosmicConfigEntry;
use serde::{Deserialize, Serialize};

pub const APP_ID: &str = "fun.wmde.Screenshot";
pub const CONFIG_VERSION: u64 = 1;

#[derive(Debug, Clone, Default, PartialEq, CosmicConfigEntry, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[version = 1]
#[id = "fun.wmde.Screenshot"]
pub struct Config {
    /// What the overlay remembers: the last choice, the last rectangle, where it saves.
    pub screenshot: Screenshot,
}

impl Config {
    pub fn load() -> (Self, Option<cosmic_config::Config>) {
        match cosmic_config::Config::new(APP_ID, CONFIG_VERSION) {
            Ok(handler) => {
                let config = Config::get_entry(&handler)
                    .inspect_err(|(errors, _)| {
                        for err in errors {
                            tracing::error!("{err}");
                        }
                    })
                    .unwrap_or_else(|(_, config)| config);
                (config, Some(handler))
            }
            Err(err) => {
                tracing::error!("no settings for {APP_ID} v{CONFIG_VERSION}: {err}");
                (Config::default(), None)
            }
        }
    }
}

/// Logical coordinates of a rectangle selection
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize, Serialize)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Screenshot {
    pub save_location: ImageSaveLocation,
    pub choice: Choice,
    #[serde(default)]
    pub last_rectangle: Option<Rect>,
    /// Name of the saved file, as a `strftime` pattern in local time. Anything `strftime`
    /// takes works; the extension is part of the pattern, so changing it to `.jpg` would
    /// only rename a PNG.
    #[serde(default = "default_filename")]
    pub filename: String,
}

fn default_filename() -> String {
    "Screenshot_%Y-%m-%d_%H-%M-%S.png".to_string()
}

impl Default for Screenshot {
    fn default() -> Self {
        Self {
            save_location: ImageSaveLocation::default(),
            choice: Choice::default(),
            last_rectangle: None,
            filename: default_filename(),
        }
    }
}

#[derive(Default, Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub enum ImageSaveLocation {
    Clipboard,
    #[default]
    Pictures,
    Documents,
    // Custom(PathBuf), // TODO
}

// TODO: Use type from screenshot directly?
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub enum Choice {
    Output(Option<String>),
    Rectangle,
    Window,
}

impl From<&mut Choice> for Choice {
    fn from(value: &mut Choice) -> Self {
        // Convenience implementation to move Choice so that the borrow checker doesn't complain
        // about partial moves
        match value {
            Choice::Output(output) => Choice::Output(output.take()),
            Choice::Rectangle => Choice::Rectangle,
            Choice::Window => Choice::Window,
        }
    }
}

impl Default for Choice {
    fn default() -> Self {
        Choice::Output(None)
    }
}
