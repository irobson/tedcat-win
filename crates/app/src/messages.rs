//! Messages between threads.
//!
//! Every window and every Win32 UI call lives on the main (UI) thread. Other
//! threads (the input hook thread, recorder threads, worker threads) talk to
//! it only by posting one of the `WM_APP_*` messages below to the main
//! message-only window, with a boxed payload in `LPARAM`. The receiver takes
//! ownership with [`take_payload`].

use std::path::PathBuf;

use tedcat_core::CaptureMode;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use crate::geom::{PointPx, RectPx};

/// Payload: [`GestureEvent`]. Posted by the input hook thread.
pub const WM_APP_GESTURE: u32 = WM_APP + 1;
/// Payload: [`HookStatus`]. Posted by the input hook thread.
pub const WM_APP_HOOK_STATUS: u32 = WM_APP + 2;
/// Payload: [`RecorderEvent`]. Posted by recorder and worker threads.
pub const WM_APP_RECORDER: u32 = WM_APP + 3;
/// Tray icon callback (`uCallbackMessage`); no payload, standard Shell_NotifyIcon semantics.
pub const WM_APP_TRAY: u32 = WM_APP + 4;

/// What the gesture recognizer decided, in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GestureEvent {
    /// The hold timer fired: show the overlay on the monitor containing `origin`.
    Begin { origin: PointPx, mode: CaptureMode },
    /// The selection changed (already normalized; not yet clamped to the monitor).
    Update(RectPx),
    /// The button was released: take the screenshot or start the recording.
    Commit { rect: RectPx, mode: CaptureMode },
    /// Escape, or the hook was reset: close the overlay, do nothing.
    Cancel,
}

/// Health of the input hook, for the tray menu and user notices.
#[derive(Clone, Debug, PartialEq)]
pub enum HookStatus {
    /// Hooks installed and receiving events.
    Active,
    /// Windows refused or removed the hooks; clicks are untouched. The reason is shown to the user.
    Unavailable(String),
    /// Click replay failed (for example `SendInput` refused by UIPI). The hook stopped
    /// holding clicks to fail open, and the gesture is disabled until re-enabled.
    Degraded(String),
    /// The emergency switch (Esc pressed 3 times within 1.5 s) turned the gesture off.
    KillSwitch,
}

/// A small BGRA preview for the saved card (top-down rows, 4 bytes per pixel).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// What a finished recording produced.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordingResult {
    pub path: PathBuf,
    pub duration_secs: f64,
    pub bytes: u64,
    pub audio_only: bool,
    /// False when there was no audio output device or system audio was disabled.
    pub has_audio: bool,
    /// First complete frame of a video recording; `None` for audio-only.
    pub thumbnail: Option<Thumbnail>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RecorderEvent {
    /// Capture stopped on its own (display removed, device lost, writer failure).
    /// The UI must call `Recorder::stop` to finalize whatever was captured.
    Interrupted(String),
    /// The recording was finalized (posted by the worker thread that ran `stop`).
    Finished(Result<RecordingResult, String>),
}

/// A `Send`-able handle to the UI thread's main window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UiHandle(isize);

// SAFETY: an HWND is just an identifier; PostMessageW is thread-safe.
unsafe impl Send for UiHandle {}
unsafe impl Sync for UiHandle {}

impl UiHandle {
    pub fn new(hwnd: HWND) -> Self {
        Self(hwnd.0 as isize)
    }

    pub fn hwnd(&self) -> HWND {
        HWND(self.0 as *mut _)
    }

    /// Posts `payload` to the UI thread. Returns false (and drops the payload)
    /// if the message could not be queued.
    pub fn post<T: Send + 'static>(&self, message: u32, payload: T) -> bool {
        let raw = Box::into_raw(Box::new(payload));
        let posted = unsafe { PostMessageW(Some(self.hwnd()), message, WPARAM(0), LPARAM(raw as isize)) }.is_ok();
        if !posted {
            // SAFETY: the message was not queued, so we still own the box.
            drop(unsafe { Box::from_raw(raw) });
        }
        posted
    }
}

/// Takes ownership of a payload posted with [`UiHandle::post`].
///
/// # Safety
/// `lparam` must come from a `UiHandle::post::<T>` with the same `T`, and be
/// taken exactly once (i.e. only in the window procedure handling that message).
pub unsafe fn take_payload<T>(lparam: LPARAM) -> Box<T> {
    Box::from_raw(lparam.0 as *mut T)
}
