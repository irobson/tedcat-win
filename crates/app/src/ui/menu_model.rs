//! The tray menu as plain data: what it shows for a given state, and what each
//! item does. `ui::menu` turns it into a Win32 popup menu.

use crate::settings::{Trigger, HOLD_CHOICES};
use crate::ui::text::gesture_hints;

/// Where the recording lifecycle is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Idle,
    Starting,
    Recording,
    Saving,
}

/// A check mark style.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Check {
    None,
    /// A check box item, checked or not.
    Toggle(bool),
    /// A radio item (bullet), selected or not.
    Radio(bool),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MenuItem {
    pub id: u32,
    pub text: String,
    pub enabled: bool,
    pub check: Check,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MenuEntry {
    Item(MenuItem),
    Separator,
    Submenu { text: String, entries: Vec<MenuEntry> },
}

/// What a menu item does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Command {
    RecordAudio,
    StopRecording,
    ToggleEnabled,
    SetTrigger(Trigger),
    SetHoldMillis(u32),
    TogglePlaySounds,
    ToggleLaunchAtLogin,
    ToggleSystemAudio,
    OpenRecordingsFolder,
    ChangeRecordingsFolder,
    OpenLog,
    Quit,
}

/// Ids of the informational (disabled) lines; never returned as commands.
const ID_STATUS: u32 = 1;
const ID_HINT_SCREENSHOT: u32 = 2;
const ID_HINT_RECORDING: u32 = 3;
const ID_TRIGGER_BASE: u32 = 30;
const ID_HOLD_BASE: u32 = 40;

impl Command {
    /// The menu item id (never 0, which `TrackPopupMenu` uses for "nothing chosen").
    pub(crate) fn id(self) -> u32 {
        match self {
            Command::RecordAudio => 10,
            Command::StopRecording => 11,
            Command::ToggleEnabled => 20,
            Command::SetTrigger(trigger) => {
                ID_TRIGGER_BASE + Trigger::ALL.iter().position(|&t| t == trigger).unwrap_or(0) as u32
            }
            Command::SetHoldMillis(ms) => {
                ID_HOLD_BASE + HOLD_CHOICES.iter().position(|&choice| choice == ms).unwrap_or(1) as u32
            }
            Command::TogglePlaySounds => 50,
            Command::ToggleLaunchAtLogin => 51,
            Command::ToggleSystemAudio => 60,
            Command::OpenRecordingsFolder => 61,
            Command::ChangeRecordingsFolder => 62,
            Command::OpenLog => 70,
            Command::Quit => 71,
        }
    }

    /// The command for a menu item id, if it is one.
    pub(crate) fn from_id(id: u32) -> Option<Command> {
        let fixed = [
            Command::RecordAudio,
            Command::StopRecording,
            Command::ToggleEnabled,
            Command::TogglePlaySounds,
            Command::ToggleLaunchAtLogin,
            Command::ToggleSystemAudio,
            Command::OpenRecordingsFolder,
            Command::ChangeRecordingsFolder,
            Command::OpenLog,
            Command::Quit,
        ];
        if let Some(command) = fixed.into_iter().find(|c| c.id() == id) {
            return Some(command);
        }
        if let Some(&trigger) = id.checked_sub(ID_TRIGGER_BASE).and_then(|i| Trigger::ALL.get(i as usize)) {
            return Some(Command::SetTrigger(trigger));
        }
        if let Some(&ms) = id.checked_sub(ID_HOLD_BASE).and_then(|i| HOLD_CHOICES.get(i as usize)) {
            return Some(Command::SetHoldMillis(ms));
        }
        None
    }
}

/// Everything the menu depends on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MenuState {
    pub phase: Phase,
    /// The input hook could not be installed (or Windows removed it).
    pub hook_unavailable: bool,
    /// The gesture is currently active (preference on and not stopped by a fault).
    pub enabled: bool,
    pub trigger: Trigger,
    pub hold_millis: u32,
    pub play_sounds: bool,
    pub launch_at_login: bool,
    pub include_system_audio: bool,
}

/// The status line at the top of the menu.
pub(crate) fn status_text(phase: Phase, hook_unavailable: bool, enabled: bool) -> &'static str {
    match phase {
        Phase::Starting => "Starting recording…",
        Phase::Recording => "Recording…",
        Phase::Saving => "Saving recording…",
        Phase::Idle if hook_unavailable => "Input hook unavailable",
        Phase::Idle if !enabled => "TedCat is paused",
        Phase::Idle => "TedCat is active",
    }
}

fn info(id: u32, text: impl Into<String>) -> MenuEntry {
    MenuEntry::Item(MenuItem { id, text: text.into(), enabled: false, check: Check::None })
}

fn action(command: Command, text: impl Into<String>, enabled: bool, check: Check) -> MenuEntry {
    MenuEntry::Item(MenuItem { id: command.id(), text: text.into(), enabled, check })
}

/// The tray menu for `state`.
pub(crate) fn build_menu(state: &MenuState) -> Vec<MenuEntry> {
    let [hint_screenshot, hint_recording] = gesture_hints(state.trigger);
    let mut entries = vec![
        info(ID_STATUS, status_text(state.phase, state.hook_unavailable, state.enabled)),
        info(ID_HINT_SCREENSHOT, hint_screenshot),
        info(ID_HINT_RECORDING, hint_recording),
        MenuEntry::Separator,
    ];
    match state.phase {
        Phase::Idle => entries.push(action(Command::RecordAudio, "Record Audio Only", true, Check::None)),
        Phase::Recording => entries.push(action(Command::StopRecording, "Stop Recording", true, Check::None)),
        Phase::Starting | Phase::Saving => {
            entries.push(action(Command::StopRecording, "Stop Recording", false, Check::None))
        }
    }
    entries.push(MenuEntry::Separator);
    entries.push(action(Command::ToggleEnabled, "Enabled", true, Check::Toggle(state.enabled)));
    entries.push(MenuEntry::Submenu {
        text: "Trigger".to_owned(),
        entries: Trigger::ALL
            .iter()
            .map(|&t| action(Command::SetTrigger(t), t.label(), true, Check::Radio(t == state.trigger)))
            .collect(),
    });
    entries.push(MenuEntry::Submenu {
        text: "Hold Duration".to_owned(),
        entries: HOLD_CHOICES
            .iter()
            .map(|&ms| {
                action(Command::SetHoldMillis(ms), format!("{ms} ms"), true, Check::Radio(ms == state.hold_millis))
            })
            .collect(),
    });
    entries.push(action(Command::TogglePlaySounds, "Play Sounds", true, Check::Toggle(state.play_sounds)));
    entries.push(action(Command::ToggleLaunchAtLogin, "Launch at Login", true, Check::Toggle(state.launch_at_login)));
    entries.push(MenuEntry::Separator);
    entries.push(action(
        Command::ToggleSystemAudio,
        "Include System Audio in Videos",
        true,
        Check::Toggle(state.include_system_audio),
    ));
    entries.push(action(Command::OpenRecordingsFolder, "Open Recordings Folder", true, Check::None));
    entries.push(action(Command::ChangeRecordingsFolder, "Change Recordings Folder…", true, Check::None));
    entries.push(MenuEntry::Separator);
    entries.push(action(Command::OpenLog, "Open Log File", true, Check::None));
    entries.push(action(Command::Quit, "Quit TedCat", true, Check::None));
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(phase: Phase) -> MenuState {
        MenuState {
            phase,
            hook_unavailable: false,
            enabled: true,
            trigger: Trigger::HoldOnly,
            hold_millis: 350,
            play_sounds: false,
            launch_at_login: true,
            include_system_audio: true,
        }
    }

    fn items(entries: &[MenuEntry]) -> Vec<MenuItem> {
        let mut out = Vec::new();
        for entry in entries {
            match entry {
                MenuEntry::Item(item) => out.push(item.clone()),
                MenuEntry::Submenu { entries, .. } => out.extend(items(entries)),
                MenuEntry::Separator => {}
            }
        }
        out
    }

    fn find(entries: &[MenuEntry], text: &str) -> Option<MenuItem> {
        items(entries).into_iter().find(|item| item.text == text)
    }

    #[test]
    fn every_command_id_round_trips() {
        let mut all = vec![
            Command::RecordAudio,
            Command::StopRecording,
            Command::ToggleEnabled,
            Command::TogglePlaySounds,
            Command::ToggleLaunchAtLogin,
            Command::ToggleSystemAudio,
            Command::OpenRecordingsFolder,
            Command::ChangeRecordingsFolder,
            Command::OpenLog,
            Command::Quit,
        ];
        all.extend(Trigger::ALL.iter().map(|&t| Command::SetTrigger(t)));
        all.extend(HOLD_CHOICES.iter().map(|&ms| Command::SetHoldMillis(ms)));
        let mut ids: Vec<u32> = all.iter().map(|c| c.id()).collect();
        for command in &all {
            assert_eq!(Command::from_id(command.id()), Some(*command));
        }
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), all.len(), "ids are unique");
        assert!(!ids.contains(&0));
        for info_id in [0, ID_STATUS, ID_HINT_SCREENSHOT, ID_HINT_RECORDING, 999] {
            assert_eq!(Command::from_id(info_id), None);
        }
    }

    #[test]
    fn status_line_reflects_state() {
        assert_eq!(status_text(Phase::Idle, false, true), "TedCat is active");
        assert_eq!(status_text(Phase::Idle, false, false), "TedCat is paused");
        assert_eq!(status_text(Phase::Idle, true, true), "Input hook unavailable");
        assert_eq!(status_text(Phase::Starting, true, false), "Starting recording…");
        assert_eq!(status_text(Phase::Recording, false, true), "Recording…");
        assert_eq!(status_text(Phase::Saving, false, true), "Saving recording…");
    }

    #[test]
    fn idle_menu_offers_audio_recording() {
        let menu = build_menu(&state(Phase::Idle));
        assert!(find(&menu, "Record Audio Only").is_some_and(|item| item.enabled));
        assert!(find(&menu, "Stop Recording").is_none());
        let first = items(&menu);
        assert_eq!(first[0].text, "TedCat is active");
        assert!(!first[0].enabled && !first[1].enabled && !first[2].enabled);
        assert_eq!(first[1].text, "Hold, drag: screenshot to clipboard");
    }

    #[test]
    fn recording_menu_offers_stop() {
        let menu = build_menu(&state(Phase::Recording));
        assert!(find(&menu, "Record Audio Only").is_none());
        assert!(find(&menu, "Stop Recording").is_some_and(|item| item.enabled));
        let saving = build_menu(&state(Phase::Saving));
        assert!(find(&saving, "Stop Recording").is_some_and(|item| !item.enabled));
    }

    #[test]
    fn checks_and_radios_follow_settings() {
        let mut s = state(Phase::Idle);
        s.trigger = Trigger::Alt;
        s.hold_millis = 500;
        s.enabled = false;
        let menu = build_menu(&s);
        assert_eq!(find(&menu, "Enabled").map(|i| i.check), Some(Check::Toggle(false)));
        assert_eq!(find(&menu, "Alt + Hold").map(|i| i.check), Some(Check::Radio(true)));
        assert_eq!(find(&menu, "Hold only").map(|i| i.check), Some(Check::Radio(false)));
        assert_eq!(find(&menu, "500 ms").map(|i| i.check), Some(Check::Radio(true)));
        assert_eq!(find(&menu, "Launch at Login").map(|i| i.check), Some(Check::Toggle(true)));
        assert_eq!(find(&menu, "Play Sounds").map(|i| i.check), Some(Check::Toggle(false)));
        assert_eq!(items(&menu)[1].text, "Alt + Hold, drag: screenshot to clipboard");
        assert_eq!(items(&menu)[0].text, "TedCat is paused");
    }

    #[test]
    fn unlisted_hold_durations_select_nothing() {
        let mut s = state(Phase::Idle);
        s.hold_millis = 420;
        let menu = build_menu(&s);
        assert!(items(&menu).iter().filter(|i| i.text.ends_with(" ms")).all(|i| i.check == Check::Radio(false)));
    }

    #[test]
    fn menu_ends_with_quit_and_has_no_double_separators() {
        for phase in [Phase::Idle, Phase::Starting, Phase::Recording, Phase::Saving] {
            let menu = build_menu(&state(phase));
            assert_eq!(items(&menu).last().map(|i| i.text.clone()), Some("Quit TedCat".to_owned()));
            assert!(menu.windows(2).all(|w| !(w[0] == MenuEntry::Separator && w[1] == MenuEntry::Separator)));
        }
    }
}
