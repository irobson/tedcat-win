//! Messages between threads.
//!
//! Every window and every Win32 UI call lives on the main (UI) thread. Other
//! threads (the input hook thread, recorder threads, worker threads) talk to
//! it only by posting one of the `WM_APP_*` messages below with
//! [`UiHandle::post`]: a boxed payload in `LPARAM` and this process's payload
//! cookie in `WPARAM`. The receiver takes ownership with [`take_payload`].
//!
//! The posts go to TedCat's message-only window (class `TedCat.Msg`), which
//! other processes' broadcasts never reach. The cookie is a per-process
//! random number, so a message that some other process posts to that window
//! directly is recognised and ignored instead of being taken for a pointer.
//!
//! The UI thread makes one request of the input hook thread, through
//! [`HookHandle`].
//!
//! Reasons shown to the user (`HookStatus::Unavailable` / `Degraded`,
//! `RecorderEvent::Interrupted`, the errors of `Recorder::start` and `stop`)
//! are complete sentences that end with a period. [`sentence`] is the one
//! place that adds a missing final period: each producer passes its reason
//! through it where the reason leaves its module. The UI shows reasons as they
//! are and only appends whole sentences after them, so a message never ends in
//! ".." or has a sentence wrapped in parentheses.

use std::hash::{BuildHasher, Hasher};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use tedcat_core::CaptureMode;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, PostThreadMessageW, WM_APP};

use crate::geom::{PointPx, RectPx};

/// Payload: [`GestureEvent`]. Posted by the input hook thread.
pub const WM_APP_GESTURE: u32 = WM_APP + 1;
/// Payload: [`HookStatus`]. Posted by the input hook thread.
pub const WM_APP_HOOK_STATUS: u32 = WM_APP + 2;
/// Payload: [`RecorderEvent`]. Posted by recorder and worker threads.
pub const WM_APP_RECORDER: u32 = WM_APP + 3;
/// Tray icon callback (`uCallbackMessage`); no payload, standard Shell_NotifyIcon semantics.
pub const WM_APP_TRAY: u32 = WM_APP + 4;

/// Thread message from the UI thread to the input hook thread: the UI will not
/// show the selection that began at the point packed in `WPARAM` (x) and
/// `LPARAM` (y) (see [`pack_point`]), so the hook must hand the held press back
/// to the app instead of swallowing it. No payload is boxed.
///
/// The hook thread's own thread messages are `WM_APP + 0x100` and `+ 0x101`
/// (`input::hook`); this one follows them.
pub const WM_HOOK_SELECTION_REFUSED: u32 = WM_APP + 0x102;

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
    /// Windows refused or removed the hooks; clicks are untouched. The reason
    /// (a [`sentence`] saying why) is shown to the user.
    Unavailable(String),
    /// Click replay failed (for example `SendInput` refused by UIPI), or the hook
    /// hit an internal error. The hook stopped holding clicks to fail open, and
    /// the gesture is disabled until re-enabled. The reason is a [`sentence`]
    /// saying what went wrong; the UI adds what TedCat did about it.
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
    /// The reason is a [`sentence`].
    Interrupted(String),
    /// The recording was finalized (posted by the worker thread that ran `stop`).
    Finished(Result<RecordingResult, String>),
}

/// `text` as a sentence for the user: trimmed, the first letter capitalized
/// and a final period added unless it already ends with `.`, `!`, `?` or `…`.
/// Idempotent, so passing a reason through it twice changes nothing.
pub fn sentence(text: &str) -> String {
    let text = text.trim();
    let mut chars = text.chars();
    let Some(first) = chars.next() else { return String::new() };
    let mut out: String = first.to_uppercase().chain(chars).collect();
    if !out.ends_with(['.', '!', '?', '\u{2026}']) {
        out.push('.');
    }
    out
}

/// The per-process cookie that [`UiHandle::post`] puts in `WPARAM`. Random
/// (seeded from the operating system's generator through `RandomState`) and
/// never zero, so the most common junk value cannot match it.
fn payload_cookie() -> usize {
    static COOKIE: OnceLock<usize> = OnceLock::new();
    *COOKIE.get_or_init(|| {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u32(std::process::id());
        if let Ok(since_epoch) = SystemTime::now().duration_since(UNIX_EPOCH) {
            hasher.write_u128(since_epoch.as_nanos());
        }
        (hasher.finish() as usize) | 1
    })
}

/// True when `WPARAM`/`LPARAM` of a payload message are what [`UiHandle::post`]
/// sends: this process's cookie and a non-null payload pointer.
pub fn is_own_payload(wparam: WPARAM, lparam: LPARAM) -> bool {
    wparam.0 == payload_cookie() && lparam.0 != 0
}

/// A `Send`-able handle to the UI thread's message-only window.
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

    /// Posts `payload` to the UI thread, with the payload cookie in `WPARAM`.
    /// Returns false (and drops the payload) if the message could not be queued.
    pub fn post<T: Send + 'static>(&self, message: u32, payload: T) -> bool {
        let raw = Box::into_raw(Box::new(payload));
        // SAFETY: posting integers; the receiver takes the box back with `take_payload`.
        let posted =
            unsafe { PostMessageW(Some(self.hwnd()), message, WPARAM(payload_cookie()), LPARAM(raw as isize)) }.is_ok();
        if !posted {
            // SAFETY: the message was not queued, so we still own the box.
            drop(unsafe { Box::from_raw(raw) });
        }
        posted
    }
}

/// Takes ownership of a payload posted with [`UiHandle::post`]. Returns `None`
/// (and touches nothing) when the message did not come from `post` in this
/// process: a wrong cookie in `wparam`, or no payload.
///
/// # Safety
/// Call it only from the window procedure that handles `message` for the
/// window `post` targets, with `T` the payload type that `post` uses for that
/// message, and at most once per message. With the right cookie, `lparam` is
/// then a `Box<T>` that nobody else owns.
pub unsafe fn take_payload<T>(wparam: WPARAM, lparam: LPARAM) -> Option<Box<T>> {
    if !is_own_payload(wparam, lparam) {
        return None;
    }
    // SAFETY: see the function contract; the cookie proves `post::<T>` sent it.
    Some(unsafe { Box::from_raw(lparam.0 as *mut T) })
}

/// A point as the `WPARAM` (x) / `LPARAM` (y) pair of [`WM_HOOK_SELECTION_REFUSED`].
pub fn pack_point(point: PointPx) -> (WPARAM, LPARAM) {
    (WPARAM(point.x as u32 as usize), LPARAM(point.y as isize))
}

/// The point packed by [`pack_point`].
pub fn unpack_point(wparam: WPARAM, lparam: LPARAM) -> PointPx {
    PointPx::new(wparam.0 as u32 as i32, lparam.0 as i32)
}

/// A `Send`-able handle to the input hook thread (from `InputHook::handle`),
/// for the requests the UI thread makes of it. It stays valid as long as that
/// `InputHook` runs; drop it before stopping the hook.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HookHandle(u32);

impl HookHandle {
    pub fn new(thread_id: u32) -> Self {
        Self(thread_id)
    }

    /// Asks the hook to hand back the press of the selection that began at
    /// `origin`, because the UI will not show it ([`WM_HOOK_SELECTION_REFUSED`]).
    /// The hook ignores it unless that selection is still in progress.
    /// Returns false if the request could not be queued.
    pub fn refuse_selection(&self, origin: PointPx) -> bool {
        let (wparam, lparam) = pack_point(origin);
        // SAFETY: posting integers to a thread id; failure is reported to the caller.
        unsafe { PostThreadMessageW(self.0, WM_HOOK_SELECTION_REFUSED, wparam, lparam) }.is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cookie_is_stable_and_never_zero() {
        assert_ne!(payload_cookie(), 0);
        assert_eq!(payload_cookie(), payload_cookie());
    }

    #[test]
    fn only_our_cookie_with_a_payload_is_taken() {
        let cookie = payload_cookie();
        assert!(is_own_payload(WPARAM(cookie), LPARAM(0x1000)));
        assert!(!is_own_payload(WPARAM(cookie), LPARAM(0)));
        assert!(!is_own_payload(WPARAM(0), LPARAM(0x4141_4141)));
        assert!(!is_own_payload(WPARAM(cookie ^ 1), LPARAM(0x4141_4141)));
        // SAFETY: a foreign message: nothing is dereferenced or freed.
        assert!(unsafe { take_payload::<GestureEvent>(WPARAM(cookie ^ 2), LPARAM(0x4141_4141)) }.is_none());
    }

    #[test]
    fn a_taken_payload_is_the_one_posted() {
        let raw = Box::into_raw(Box::new(GestureEvent::Cancel));
        // SAFETY: built exactly like `UiHandle::post` builds a message.
        let taken = unsafe { take_payload::<GestureEvent>(WPARAM(payload_cookie()), LPARAM(raw as isize)) };
        assert_eq!(taken.as_deref(), Some(&GestureEvent::Cancel));
    }

    #[test]
    fn sentences_get_exactly_one_final_period() {
        assert_eq!(sentence("The audio output device was disconnected."), "The audio output device was disconnected.");
        assert_eq!(sentence("The display resolution changed"), "The display resolution changed.");
        assert_eq!(sentence("  could not write the Run key (error 5) "), "Could not write the Run key (error 5).");
        assert_eq!(
            sentence("Screen capture stopped: The device was removed. (0x887A0005)"),
            "Screen capture stopped: The device was removed. (0x887A0005)."
        );
        assert_eq!(sentence("Really?"), "Really?");
        assert_eq!(sentence("\u{e9}t\u{e9}"), "\u{c9}t\u{e9}.");
        assert_eq!(sentence(""), "");
        assert_eq!(sentence("   "), "");
        for text in ["x", "Nothing was recorded.", "A (b)", "c\u{2026}"] {
            assert_eq!(sentence(&sentence(text)), sentence(text), "{text}");
            assert!(!sentence(text).ends_with(".."), "{text}");
        }
    }

    #[test]
    fn points_survive_packing() {
        let points = [PointPx::new(0, 0), PointPx::new(-1920, -1080), PointPx::new(7680, 4320), PointPx::new(i32::MIN, i32::MAX)];
        for point in points {
            let (wparam, lparam) = pack_point(point);
            assert_eq!(unpack_point(wparam, lparam), point);
        }
    }
}
