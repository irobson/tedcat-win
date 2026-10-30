//! Low-level mouse and keyboard hooks implementing hold-and-replay.
//!
//! CONTRACT (owned by the input agent; keep the public API unchanged):
//!
//! `InputHook::start` spawns a dedicated thread that installs `WH_MOUSE_LL` and
//! `WH_KEYBOARD_LL` and runs its own message loop. The hook callbacks run on
//! that thread and must return quickly (Windows silently removes hooks whose
//! callback exceeds `LowLevelHooksTimeout`). They drive a
//! `tedcat_core::HoldGestureRecognizer` (coordinates in physical pixels; set
//! `move_tolerance` to `4.0 * monitor.scale` at each mouse-down) and post every
//! `Effect` that concerns the UI to `ui` as `WM_APP_GESTURE` / `GestureEvent`.
//!
//! Required behaviour:
//! - Left button only. Right/middle buttons, wheel and plain moves are never
//!   swallowed (swallowing `WM_MOUSEMOVE` freezes the cursor). While pending,
//!   moves within tolerance pass through but are still fed to the recognizer.
//! - Hold: swallow the left-down and remember it. Release: re-inject the held
//!   down and then the current event with one `SendInput` call (absolute,
//!   virtual-desk coordinates, `dwExtraInfo = TEDCAT_INPUT_MARKER`), from the
//!   hook thread but outside the hook callback (post a thread message to
//!   yourself), preserving order.
//! - Ignore events whose `dwExtraInfo == TEDCAT_INPUT_MARKER` (our own replays).
//!   Injected events from other processes are treated like real input, so
//!   automation (and the CI smoke test) can drive the gesture.
//! - Modifiers at mouse-down from `GetAsyncKeyState` (Shift, Ctrl, Alt, Win)
//!   mapped to `tedcat_core::Modifiers`; required modifiers from settings.
//! - Never hold a press (pass it straight through) when: settings are
//!   disabled; the window under the pointer belongs to TedCat (class name
//!   starts with `util::win::CLASS_PREFIX`); or the window under the pointer
//!   belongs to a process with a higher integrity level than ours (UIPI would
//!   block the replay).
//! - Fail open: if `SendInput` inserts fewer events than requested, stop
//!   holding presses at once (set `settings.enabled` false) and post
//!   `HookStatus::Degraded(reason)`.
//! - Escape during a selection: cancel (swallow that Escape). Escape pressed 3
//!   times within 1.5 s in any state: emergency switch, set `enabled` false
//!   and post `HookStatus::KillSwitch` (do not swallow those keys otherwise).
//! - Watchdog: every 2 s compare `GetLastInputInfo` with the time of the last
//!   hook callback; if input happened more than 2 s after the last callback,
//!   reinstall both hooks, reset the recognizer (drop any held press, cancel
//!   any selection) and log it.
//! - Post `HookStatus::Active` once installed, or `Unavailable` on failure.
//! - `stop` unhooks, ends the thread and joins it.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;

use tedcat_core::Modifiers;

use crate::messages::UiHandle;

/// Marks every input event TedCat injects ("TCAT").
pub const TEDCAT_INPUT_MARKER: usize = 0x5443_4154;

/// Gesture settings shared between the UI thread (writer) and the hook thread (reader).
#[derive(Debug)]
pub struct GestureSettings {
    pub enabled: AtomicBool,
    pub hold_millis: AtomicU32,
    /// `tedcat_core::Modifiers` bits that must be held (never includes Shift).
    pub required_modifiers: AtomicU8,
    /// False while a recording is running: Shift + hold then passes through.
    pub allows_recording: AtomicBool,
}

impl GestureSettings {
    pub fn new(enabled: bool, hold_millis: u32, required: Modifiers) -> Arc<Self> {
        Arc::new(Self {
            enabled: AtomicBool::new(enabled),
            hold_millis: AtomicU32::new(hold_millis),
            required_modifiers: AtomicU8::new(required.without(Modifiers::SHIFT).0),
            allows_recording: AtomicBool::new(true),
        })
    }

    pub fn required(&self) -> Modifiers {
        Modifiers(self.required_modifiers.load(Ordering::Relaxed))
    }
}

/// The running hook thread.
pub struct InputHook {
    _private: (),
}

impl InputHook {
    /// Installs the hooks on a new thread and starts posting events to `ui`.
    pub fn start(ui: UiHandle, settings: Arc<GestureSettings>) -> std::io::Result<InputHook> {
        let _ = (ui, settings);
        todo!("input agent")
    }

    /// Removes the hooks and joins the thread. Any held press is dropped.
    pub fn stop(self) {
        todo!("input agent")
    }
}
