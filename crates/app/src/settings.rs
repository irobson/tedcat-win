//! Persisted preferences.
//!
//! CONTRACT (owned by the UI agent): JSON at `util::paths::app_data_dir()\settings.json`,
//! loaded at start (defaults on any error), saved on every change. Launch at
//! login is not stored here but in `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tedcat_core::Modifiers;

/// Which modifier must be held together with the press. Shift is reserved for recording.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Trigger {
    #[default]
    HoldOnly,
    Control,
    Alt,
    Win,
}

impl Trigger {
    /// Every trigger, in menu order.
    pub const ALL: [Trigger; 4] = [Trigger::HoldOnly, Trigger::Control, Trigger::Alt, Trigger::Win];

    /// The modifiers that must be held for the gesture to arm.
    pub fn modifiers(self) -> Modifiers {
        match self {
            Trigger::HoldOnly => Modifiers::NONE,
            Trigger::Control => Modifiers::CONTROL,
            Trigger::Alt => Modifiers::ALT,
            Trigger::Win => Modifiers::WIN,
        }
    }

    /// The menu label: "Hold only", "Ctrl + Hold", ...
    pub fn label(self) -> &'static str {
        match self {
            Trigger::HoldOnly => "Hold only",
            Trigger::Control => "Ctrl + Hold",
            Trigger::Alt => "Alt + Hold",
            Trigger::Win => "Win + Hold",
        }
    }

    /// The key name to put in front of "Hold" in gesture hints, if any.
    pub fn key_name(self) -> Option<&'static str> {
        match self {
            Trigger::HoldOnly => None,
            Trigger::Control => Some("Ctrl"),
            Trigger::Alt => Some("Alt"),
            Trigger::Win => Some("Win"),
        }
    }
}

/// The hold durations offered in the menu, in milliseconds.
pub const HOLD_CHOICES: [u32; 4] = [200, 350, 500, 750];

/// Hold durations outside this range (from a hand-edited file) are clamped into it.
pub const HOLD_RANGE: (u32, u32) = (100, 2000);

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

impl Settings {
    /// `%APPDATA%\TedCat\settings.json`.
    pub fn path() -> PathBuf {
        crate::util::paths::app_data_dir().join("settings.json")
    }

    /// Loads the settings file, falling back to defaults on any error.
    pub fn load() -> Settings {
        Self::load_from(&Self::path())
    }

    /// Loads `path`, falling back to defaults when it is missing or unreadable.
    pub fn load_from(path: &Path) -> Settings {
        match fs::read_to_string(path) {
            Ok(text) => Self::from_json(&text),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Settings::default(),
            Err(error) => {
                crate::log_warn!("could not read {}: {error}; using defaults", path.display());
                Settings::default()
            }
        }
    }

    /// Parses settings JSON leniently: unknown keys are ignored, a missing or
    /// invalid value falls back to its default without discarding the others,
    /// and anything that is not a JSON object gives the defaults.
    pub fn from_json(text: &str) -> Settings {
        if let Ok(settings) = serde_json::from_str::<Settings>(text) {
            return settings.sanitized();
        }
        let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(text) else {
            return Settings::default();
        };
        fn field<T: serde::de::DeserializeOwned>(
            map: &serde_json::Map<String, serde_json::Value>,
            key: &str,
            fallback: T,
        ) -> T {
            map.get(key).and_then(|value| serde_json::from_value(value.clone()).ok()).unwrap_or(fallback)
        }
        let defaults = Settings::default();
        Settings {
            enabled: field(&map, "enabled", defaults.enabled),
            trigger: field(&map, "trigger", defaults.trigger),
            hold_millis: field(&map, "hold_millis", defaults.hold_millis),
            play_sounds: field(&map, "play_sounds", defaults.play_sounds),
            include_system_audio: field(&map, "include_system_audio", defaults.include_system_audio),
            recordings_folder: field(&map, "recordings_folder", defaults.recordings_folder),
        }
        .sanitized()
    }

    /// Pretty-printed JSON for the settings file.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_owned())
    }

    /// Writes the settings file (via a temporary file, so a crash never leaves half a file).
    pub fn save(&self) -> io::Result<()> {
        self.save_to(&Self::path())
    }

    /// Writes the settings to `path`, creating its folder if needed.
    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, self.to_json())?;
        fs::rename(&temporary, path)
    }

    /// The same settings with out-of-range values repaired.
    pub fn sanitized(mut self) -> Settings {
        self.hold_millis = self.hold_millis.clamp(HOLD_RANGE.0, HOLD_RANGE.1);
        if self.recordings_folder.as_ref().is_some_and(|folder| folder.as_os_str().is_empty()) {
            self.recordings_folder = None;
        }
        self
    }

    /// The folder recordings are saved to.
    pub fn recordings_dir(&self) -> PathBuf {
        self.recordings_folder.clone().unwrap_or_else(crate::util::paths::default_recordings_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_macos_app() {
        let s = Settings::default();
        assert!(s.enabled);
        assert_eq!(s.trigger, Trigger::HoldOnly);
        assert_eq!(s.hold_millis, 350);
        assert!(!s.play_sounds);
        assert!(s.include_system_audio);
        assert_eq!(s.recordings_folder, None);
    }

    #[test]
    fn json_round_trip() {
        let s = Settings {
            enabled: false,
            trigger: Trigger::Alt,
            hold_millis: 750,
            play_sounds: true,
            include_system_audio: false,
            recordings_folder: Some(PathBuf::from(r"D:\Clips")),
        };
        assert_eq!(Settings::from_json(&s.to_json()), s);
    }

    #[test]
    fn malformed_json_gives_defaults() {
        for text in ["", "not json", "{", "[1, 2]", "42", "null", "\"text\""] {
            assert_eq!(Settings::from_json(text), Settings::default(), "input: {text:?}");
        }
    }

    #[test]
    fn missing_fields_take_their_defaults() {
        let s = Settings::from_json(r#"{ "play_sounds": true }"#);
        assert_eq!(s, Settings { play_sounds: true, ..Settings::default() });
    }

    #[test]
    fn one_bad_field_does_not_discard_the_others() {
        let s = Settings::from_json(r#"{ "enabled": false, "hold_millis": "slow", "trigger": "Shift", "extra": 1 }"#);
        assert_eq!(s, Settings { enabled: false, ..Settings::default() });
    }

    #[test]
    fn out_of_range_values_are_repaired() {
        assert_eq!(Settings::from_json(r#"{ "hold_millis": 5 }"#).hold_millis, HOLD_RANGE.0);
        assert_eq!(Settings::from_json(r#"{ "hold_millis": 99999 }"#).hold_millis, HOLD_RANGE.1);
        assert_eq!(Settings::from_json(r#"{ "recordings_folder": "" }"#).recordings_folder, None);
    }

    #[test]
    fn hold_choices_are_within_range_and_include_the_default() {
        assert!(HOLD_CHOICES.contains(&Settings::default().hold_millis));
        assert!(HOLD_CHOICES.iter().all(|&ms| (HOLD_RANGE.0..=HOLD_RANGE.1).contains(&ms)));
    }

    #[test]
    fn triggers_never_require_shift() {
        for trigger in Trigger::ALL {
            assert!(!trigger.modifiers().contains(Modifiers::SHIFT));
        }
        assert_eq!(Trigger::Control.modifiers(), Modifiers::CONTROL);
        assert_eq!(Trigger::HoldOnly.modifiers(), Modifiers::NONE);
    }

    #[test]
    fn save_and_load_from_disk() {
        let dir = std::env::temp_dir().join(format!("tedcat-settings-test-{}", std::process::id()));
        let path = dir.join("settings.json");
        let s = Settings { trigger: Trigger::Win, hold_millis: 200, ..Settings::default() };
        s.save_to(&path).expect("save");
        assert_eq!(Settings::load_from(&path), s);
        assert_eq!(Settings::load_from(&dir.join("missing.json")), Settings::default());
        let _ = fs::remove_dir_all(&dir);
    }
}
