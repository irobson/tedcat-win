//! Persisted preferences.
//!
//! CONTRACT (owned by the UI agent): JSON at `util::paths::app_data_dir()\settings.json`,
//! loaded at start (defaults on any error), saved on every change. Launch at
//! login is not stored here but in `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Which modifier must be held together with the press. Shift is reserved for recording.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Trigger {
    #[default]
    HoldOnly,
    Control,
    Alt,
    Win,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub enabled: bool,
    pub trigger: Trigger,
    pub hold_millis: u32,
    pub play_sounds: bool,
    pub include_system_audio: bool,
    /// `None` means the default `Videos\TedCat`.
    pub recordings_folder: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            trigger: Trigger::HoldOnly,
            hold_millis: 350,
            play_sounds: false,
            include_system_audio: true,
            recordings_folder: None,
        }
    }
}
