//! Short system sounds for the "Play Sounds" option.

use windows::core::{w, PCWSTR};
use windows::Win32::Media::Audio::{PlaySoundW, SND_ALIAS, SND_ASYNC, SND_NODEFAULT};

/// Something worth a sound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sound {
    ScreenshotTaken,
    RecordingStarted,
    RecordingSaved,
}

impl Sound {
    /// The standard sound-scheme alias played for this event. Users can change
    /// (or silence) these in Control Panel > Sound > Sounds.
    fn alias(self) -> PCWSTR {
        match self {
            Sound::ScreenshotTaken => w!("SystemAsterisk"),
            Sound::RecordingStarted => w!("DeviceConnect"),
            Sound::RecordingSaved => w!("Notification.Default"),
        }
    }
}

/// Plays `sound` asynchronously; stays silent if the alias has no sound.
pub(crate) fn play(sound: Sound) {
    // SAFETY: the alias is a static string; SND_ASYNC returns immediately.
    let ok = unsafe { PlaySoundW(sound.alias(), None, SND_ALIAS | SND_ASYNC | SND_NODEFAULT) };
    if !ok.as_bool() {
        crate::log_info!("no sound played for {sound:?}");
    }
}
