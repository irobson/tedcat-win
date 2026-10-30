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
//! - `handle` gives the UI a `messages::HookHandle`. When the UI will not show
//!   a selection (the overlay failed, or TedCat is quitting) it sends
//!   `WM_HOOK_SELECTION_REFUSED`, and the held press goes back to the app.
//!
//! Implementation notes:
//! - The decisions live in the Win32-free [`logic`] submodule; this file is the
//!   thread, the callbacks and the Win32 calls around it.
//! - Callbacks never log, never touch the disk and never wait: log lines go to
//!   a separate logger thread, replays are sent by the message loop.
//! - A press is also never held over any window of TedCat's own process (tray
//!   menu, dialogs), nor when no window or process can be found under it.
//! - If the hooks cannot be installed, `start` still succeeds: it posts
//!   `Unavailable` and the watchdog retries every 2 s, posting `Active` once
//!   the hooks are in.

mod logic;

use std::cell::{Cell, RefCell};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tedcat_core::Modifiers;
use windows::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel, TOKEN_MANDATORY_LABEL,
    TOKEN_QUERY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::SystemServices::SECURITY_MANDATORY_MEDIUM_RID;
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetCurrentThread, GetCurrentThreadId, OpenProcess, OpenProcessToken,
    SetThreadPriority, PROCESS_QUERY_LIMITED_INFORMATION, THREAD_PRIORITY_HIGHEST,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetLastInputInfo, SendInput, INPUT, INPUT_0, INPUT_MOUSE, LASTINPUTINFO, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
    MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY, VK_CONTROL, VK_ESCAPE, VK_LWIN,
    VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetAncestor, GetClassNameW, GetCursorPos, GetMessageW, GetSystemMetrics,
    GetWindowThreadProcessId, KillTimer, PeekMessageW, PostThreadMessageW, SetTimer, SetWindowsHookExW,
    TranslateMessage, UnhookWindowsHookEx, WindowFromPoint, GA_ROOT, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, MSG,
    MSLLHOOKSTRUCT, PM_NOREMOVE, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    USER_TIMER_MINIMUM, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_APP, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP,
    WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER, WM_USER, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

use self::logic::{
    class_has_prefix, hooks_look_dead, is_pen_or_touch, may_hold_over, modifiers_from_keys, normalize_absolute,
    Command, HookLogic, IntegrityCache, LogLevel, PressContext, ReplayAction, ReplayEvent, Throttled, UpdateThrottle,
};
use crate::geom::{MonitorInfo, PointPx};
use crate::messages::{
    unpack_point, GestureEvent, HookHandle, HookStatus, UiHandle, WM_APP_GESTURE, WM_APP_HOOK_STATUS,
    WM_HOOK_SELECTION_REFUSED,
};
use crate::util::win::CLASS_PREFIX;

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

/// Thread message: replays are queued (no payload).
const WM_HOOK_WAKE: u32 = WM_APP + 0x100;
/// Thread message: remove the hooks and end the thread (no payload).
const WM_HOOK_STOP: u32 = WM_APP + 0x101;

/// Watchdog period.
const WATCHDOG_INTERVAL_MS: u32 = 2_000;
/// `stop` retries posting the stop message this many times, 10 ms apart.
const STOP_POST_ATTEMPTS: u32 = 50;
/// At most one watchdog log line per this interval (the secure desktop, where
/// hooks see nothing, would otherwise log every 2 s).
const WATCHDOG_LOG_INTERVAL: Duration = Duration::from_secs(60);
/// A hold-timer tick this much earlier than its deadline is a stale tick of a
/// recycled timer id, not the hold.
const HOLD_TIMER_SLACK: Duration = Duration::from_millis(30);

/// The running hook thread.
pub struct InputHook {
    thread_id: u32,
    thread: Option<JoinHandle<()>>,
}

impl InputHook {
    /// Installs the hooks on a new thread and starts posting events to `ui`.
    ///
    /// Fails only if the thread cannot be created. If Windows refuses the hooks,
    /// the thread posts `HookStatus::Unavailable` and keeps retrying.
    pub fn start(ui: UiHandle, settings: Arc<GestureSettings>) -> std::io::Result<InputHook> {
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("TedCat input hook".to_owned())
            .spawn(move || run_hook_thread(ui, settings, ready_tx))?;
        match ready_rx.recv() {
            Ok(thread_id) => Ok(InputHook { thread_id, thread: Some(thread) }),
            Err(_) => {
                let _ = thread.join();
                Err(std::io::Error::other("the input hook thread exited during start-up"))
            }
        }
    }

    /// A handle for the UI thread's requests to the hook thread (see
    /// [`HookHandle`]); valid until [`InputHook::stop`].
    pub fn handle(&self) -> HookHandle {
        HookHandle::new(self.thread_id)
    }

    /// Removes the hooks and joins the thread. Any held press is dropped.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        let Some(thread) = self.thread.take() else { return };
        let mut posted = false;
        for _ in 0..STOP_POST_ATTEMPTS {
            if thread.is_finished() {
                break;
            }
            // SAFETY: posting a payload-free message to a thread id we own.
            if unsafe { PostThreadMessageW(self.thread_id, WM_HOOK_STOP, WPARAM(0), LPARAM(0)) }.is_ok() {
                posted = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if posted || thread.is_finished() {
            if thread.join().is_err() {
                crate::log_error!("The input hook thread panicked.");
            }
        } else {
            crate::log_error!("Could not ask the input hook thread to stop; it ends with the process.");
        }
    }
}

impl Drop for InputHook {
    fn drop(&mut self) {
        self.shutdown();
    }
}

thread_local! {
    /// The hook thread's state. Hook callbacks have no user-data pointer, so
    /// they find it here; it only exists on the hook thread.
    static HOOK: RefCell<Option<HookThread>> = const { RefCell::new(None) };
    /// `GetTickCount` at the last hook callback, for the watchdog. Separate from
    /// `HOOK` so it is updated even if the state is busy.
    static LAST_CALLBACK: Cell<u32> = const { Cell::new(0) };
}

fn run_hook_thread(ui: UiHandle, settings: Arc<GestureSettings>, ready: mpsc::Sender<u32>) {
    let mut msg = MSG::default();
    // SAFETY: plain calls on the current thread. PeekMessageW creates the
    // thread's message queue so PostThreadMessageW works as soon as the id is
    // announced.
    let thread_id = unsafe {
        let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);
        // Hook callbacks must be served promptly even when the CPU is busy.
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
        GetCurrentThreadId()
    };
    if ready.send(thread_id).is_err() {
        return;
    }
    drop(ready);

    HOOK.with(|cell| *cell.borrow_mut() = Some(HookThread::new(ui, settings, thread_id)));
    with_hook(HookThread::start);

    loop {
        // SAFETY: `msg` is a valid MSG buffer. The hook callbacks run inside this call.
        let result = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if result.0 == 0 {
            break;
        }
        if result.0 == -1 {
            with_hook(HookThread::loop_failed);
            break;
        }
        if msg.hwnd.is_invalid() {
            if msg.message == WM_HOOK_STOP {
                break;
            }
            with_hook(|hook| hook.on_thread_message(&msg));
        } else {
            // This thread owns no windows, but dispatch anything addressed to one.
            // SAFETY: `msg` was just filled in by GetMessageW.
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    with_hook(HookThread::shutdown);
    let state = HOOK.with(|cell| cell.borrow_mut().take());
    drop(state);
}

/// Runs `f` on the hook state. Returns `None` when the state is missing or
/// already borrowed (a re-entrant callback), or when `f` panicked; in the last
/// case the hook fails open.
fn with_hook<R>(f: impl FnOnce(&mut HookThread) -> R) -> Option<R> {
    HOOK.try_with(|cell| {
        let mut guard = cell.try_borrow_mut().ok()?;
        let hook = guard.as_mut()?;
        match panic::catch_unwind(AssertUnwindSafe(|| f(&mut *hook))) {
            Ok(result) => Some(result),
            Err(_) => {
                hook.internal_failure();
                None
            }
        }
    })
    .ok()
    .flatten()
}

fn note_callback() {
    // SAFETY: GetTickCount has no preconditions.
    let now = unsafe { GetTickCount() };
    let _ = LAST_CALLBACK.try_with(|last| last.set(now));
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && lparam.0 != 0 {
        note_callback();
        // SAFETY: for WH_MOUSE_LL with HC_ACTION, lparam points to an
        // MSLLHOOKSTRUCT that stays valid for the duration of this call.
        let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        if with_hook(|hook| hook.mouse_event(wparam.0 as u32, info)).unwrap_or(false) {
            // Swallow: do not call the next hook.
            return LRESULT(1);
        }
    }
    // SAFETY: forwarding the unchanged hook arguments.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && lparam.0 != 0 {
        note_callback();
        // SAFETY: for WH_KEYBOARD_LL with HC_ACTION, lparam points to a
        // KBDLLHOOKSTRUCT that stays valid for the duration of this call.
        let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        if with_hook(|hook| hook.key_event(wparam.0 as u32, info)).unwrap_or(false) {
            return LRESULT(1);
        }
    }
    // SAFETY: forwarding the unchanged hook arguments.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

struct Hooks {
    mouse: HHOOK,
    keyboard: HHOOK,
}

/// Everything the hook thread owns. Lives in the `HOOK` thread-local.
struct HookThread {
    ui: UiHandle,
    settings: Arc<GestureSettings>,
    thread_id: u32,
    logic: HookLogic,
    /// Reused buffer for executing commands.
    commands: Vec<Command>,
    hooks: Option<Hooks>,
    /// `Unavailable` was posted and `Active` has not been posted since.
    reported_unavailable: bool,
    hold_timer: usize,
    /// When the armed hold timer is due.
    hold_deadline: Option<Instant>,
    update_timer: usize,
    watchdog_timer: usize,
    throttle: UpdateThrottle,
    probe: TargetProbe,
    log: HookLog,
    clock: Instant,
    last_watchdog_log: Option<Instant>,
    suppressed_watchdog_logs: u32,
}

impl HookThread {
    fn new(ui: UiHandle, settings: Arc<GestureSettings>, thread_id: u32) -> Self {
        Self {
            ui,
            settings,
            thread_id,
            logic: HookLogic::new(),
            commands: Vec::with_capacity(8),
            hooks: None,
            reported_unavailable: false,
            hold_timer: 0,
            hold_deadline: None,
            update_timer: 0,
            watchdog_timer: 0,
            throttle: UpdateThrottle::default(),
            probe: TargetProbe::new(),
            log: HookLog::start(),
            clock: Instant::now(),
            last_watchdog_log: None,
            suppressed_watchdog_logs: 0,
        }
    }

    fn start(&mut self) {
        match self.install() {
            Ok(()) => {
                self.post_status(HookStatus::Active);
                self.log.write(LogLevel::Info, "Input hooks installed.".to_owned());
            }
            Err(reason) => self.report_unavailable(reason),
        }
        // SAFETY: a thread timer on the current thread.
        self.watchdog_timer = unsafe { SetTimer(None, 0, WATCHDOG_INTERVAL_MS, None) };
        if self.watchdog_timer == 0 {
            self.log.write(LogLevel::Warn, "The input hook watchdog timer could not be started.".to_owned());
        }
    }

    fn shutdown(&mut self) {
        self.uninstall();
        self.kill_hold_timer();
        self.kill_update_timer();
        if self.watchdog_timer != 0 {
            // SAFETY: a thread timer created on this thread.
            unsafe {
                let _ = KillTimer(None, self.watchdog_timer);
            }
            self.watchdog_timer = 0;
        }
        self.logic.shutdown();
        self.run_commands();
        self.log.write(LogLevel::Info, "Input hooks removed.".to_owned());
    }

    fn loop_failed(&mut self) {
        // SAFETY: GetLastError has no preconditions.
        let error = unsafe { GetLastError() };
        self.log.write(LogLevel::Error, format!("The input hook message loop failed (error {}).", error.0));
        self.post_status(HookStatus::Unavailable("TedCat's input thread stopped unexpectedly.".to_owned()));
    }

    fn internal_failure(&mut self) {
        self.logic.internal_failure();
        self.run_commands();
    }

    fn install(&mut self) -> Result<(), String> {
        // SAFETY: querying the handle of the running executable.
        let module = unsafe { GetModuleHandleW(None) }.ok().map(HINSTANCE::from);
        // SAFETY: the callbacks are `extern "system"` functions with the HOOKPROC
        // signature that live for the whole program.
        let mouse = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), module, 0) }
            .map_err(|e| format!("Windows refused the mouse hook: {}", e.message()))?;
        // SAFETY: as above.
        let keyboard = match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), module, 0) } {
            Ok(hook) => hook,
            Err(e) => {
                // Without the keyboard hook there is no Escape and no emergency
                // switch: do not run with the mouse hook alone.
                // SAFETY: `mouse` was installed above by this thread.
                unsafe {
                    let _ = UnhookWindowsHookEx(mouse);
                }
                return Err(format!("Windows refused the keyboard hook: {}", e.message()));
            }
        };
        self.hooks = Some(Hooks { mouse, keyboard });
        note_callback();
        Ok(())
    }

    fn uninstall(&mut self) {
        if let Some(hooks) = self.hooks.take() {
            // SAFETY: both hooks were installed by this thread; failure (for
            // example a hook Windows already removed) is harmless.
            unsafe {
                let _ = UnhookWindowsHookEx(hooks.mouse);
                let _ = UnhookWindowsHookEx(hooks.keyboard);
            }
        }
    }

    fn report_unavailable(&mut self, reason: String) {
        self.log.write(LogLevel::Error, format!("Input hooks unavailable: {reason}"));
        self.reported_unavailable = true;
        self.post_status(HookStatus::Unavailable(reason));
    }

    /// A mouse hook event. Returns true to swallow it.
    fn mouse_event(&mut self, message: u32, info: &MSLLHOOKSTRUCT) -> bool {
        if info.dwExtraInfo == TEDCAT_INPUT_MARKER || is_pen_or_touch(info.dwExtraInfo) {
            return false;
        }
        let point = PointPx::new(info.pt.x, info.pt.y);
        let swallow = match message {
            WM_MOUSEMOVE => {
                self.logic.moved(point);
                false
            }
            WM_LBUTTONDOWN => {
                let ctx = self.press_context(point);
                self.logic.left_down(point, &ctx)
            }
            WM_LBUTTONUP => self.logic.left_up(point),
            other => match other_action(other, info.mouseData) {
                Some(action) => self.logic.other_button(ReplayEvent::new(point, action)),
                None => false,
            },
        };
        self.run_commands();
        swallow
    }

    /// A keyboard hook event. Only Escape matters. Returns true to swallow it.
    fn key_event(&mut self, message: u32, info: &KBDLLHOOKSTRUCT) -> bool {
        if info.vkCode != u32::from(VK_ESCAPE.0) || info.dwExtraInfo == TEDCAT_INPUT_MARKER {
            return false;
        }
        let down = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
        if !down && !matches!(message, WM_KEYUP | WM_SYSKEYUP) {
            return false;
        }
        // SAFETY: GetTickCount has no preconditions.
        let now = unsafe { GetTickCount() };
        let swallow = self.logic.escape(down, now);
        self.run_commands();
        swallow
    }

    /// The settings and environment for a left-button press at `point`.
    fn press_context(&mut self, point: PointPx) -> PressContext {
        let settings = &self.settings;
        let mut ctx = PressContext {
            enabled: settings.enabled.load(Ordering::SeqCst),
            eligible: false,
            scale: 1.0,
            modifiers: Modifiers::NONE,
            hold_millis: settings.hold_millis.load(Ordering::Relaxed),
            required: settings.required(),
            allows_recording: settings.allows_recording.load(Ordering::Relaxed),
        };
        if ctx.enabled {
            let now_ms = self.clock.elapsed().as_millis() as u64;
            ctx.eligible = self.probe.may_hold_at(point, now_ms);
            if ctx.eligible {
                ctx.scale = MonitorInfo::from_point(point).scale;
                ctx.modifiers = current_modifiers();
            }
        }
        ctx
    }

    fn on_thread_message(&mut self, msg: &MSG) {
        if msg.message == WM_TIMER {
            let id = msg.wParam.0;
            if id != 0 && id == self.hold_timer {
                let early = self.hold_deadline.is_some_and(|due| Instant::now() + HOLD_TIMER_SLACK < due);
                // KillTimer does not remove a WM_TIMER already queued, and ids are
                // recycled: an early tick belongs to an earlier press. The real
                // timer is periodic and still fires on time.
                if !early {
                    self.kill_hold_timer();
                    self.logic.hold_timeout(self.settings.enabled.load(Ordering::SeqCst));
                }
            } else if id != 0 && id == self.update_timer {
                self.kill_update_timer();
                let now_us = self.clock.elapsed().as_micros() as u64;
                if let Some(rect) = self.throttle.flush(now_us) {
                    let _ = self.ui.post(WM_APP_GESTURE, GestureEvent::Update(rect));
                }
            } else if id != 0 && id == self.watchdog_timer {
                self.watchdog();
            } else if id != 0 {
                // A timer this thread no longer tracks.
                // SAFETY: killing a thread timer of this thread; failure is harmless.
                unsafe {
                    let _ = KillTimer(None, id);
                }
            }
        } else if msg.message == WM_HOOK_SELECTION_REFUSED {
            // The UI will not show this selection (overlay failed, or TedCat is
            // quitting): hand the press back instead of swallowing the release.
            let origin = unpack_point(msg.wParam, msg.lParam);
            self.logic.selection_refused(origin, cursor_position().unwrap_or(origin));
        }
        // WM_HOOK_WAKE needs nothing more than this.
        self.run_commands();
        self.send_replays();
        self.run_commands();
    }

    fn watchdog(&mut self) {
        if self.hooks.is_none() {
            if self.install().is_ok() {
                self.log.write(LogLevel::Info, "Input hooks installed after an earlier failure.".to_owned());
                if self.reported_unavailable {
                    self.reported_unavailable = false;
                    self.post_status(HookStatus::Active);
                }
            }
            return;
        }
        let mut info = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
        // SAFETY: `info` is a valid LASTINPUTINFO with cbSize set.
        if !unsafe { GetLastInputInfo(&mut info) }.as_bool() {
            return;
        }
        let last_callback = LAST_CALLBACK.with(Cell::get);
        if hooks_look_dead(info.dwTime, last_callback) {
            self.reinstall(info.dwTime.wrapping_sub(last_callback));
        }
    }

    fn reinstall(&mut self, stalled_ms: u32) {
        self.uninstall();
        self.kill_hold_timer();
        self.kill_update_timer();
        self.throttle.reset();
        self.logic.hooks_reset();
        self.run_commands();
        match self.install() {
            Ok(()) => {
                let due = self.last_watchdog_log.is_none_or(|at| at.elapsed() >= WATCHDOG_LOG_INTERVAL);
                if due {
                    let suppressed = std::mem::take(&mut self.suppressed_watchdog_logs);
                    self.last_watchdog_log = Some(Instant::now());
                    self.log.write(
                        LogLevel::Warn,
                        format!(
                            "Input arrived {stalled_ms} ms after the last hook callback; hooks reinstalled \
                             ({suppressed} similar reinstalls not logged)."
                        ),
                    );
                } else {
                    self.suppressed_watchdog_logs += 1;
                }
            }
            Err(reason) => self.report_unavailable(reason),
        }
    }

    /// Executes the commands the logic emitted, including any they lead to.
    fn run_commands(&mut self) {
        let mut batch = std::mem::take(&mut self.commands);
        // Commands can produce follow-up commands (a timer that cannot be
        // armed, a post that fails); a few rounds always settle them.
        for _ in 0..8 {
            self.logic.drain_commands_into(&mut batch);
            if batch.is_empty() {
                break;
            }
            for command in batch.drain(..) {
                self.execute(command);
            }
        }
        batch.clear();
        self.commands = batch;
    }

    fn execute(&mut self, command: Command) {
        match command {
            Command::Gesture(GestureEvent::Update(rect)) => {
                let now_us = self.clock.elapsed().as_micros() as u64;
                match self.throttle.offer(rect, now_us) {
                    Throttled::Send(rect) => {
                        let _ = self.ui.post(WM_APP_GESTURE, GestureEvent::Update(rect));
                    }
                    Throttled::Deferred if self.update_timer == 0 => {
                        // SAFETY: a thread timer on the current thread.
                        self.update_timer = unsafe { SetTimer(None, 0, USER_TIMER_MINIMUM, None) };
                        if self.update_timer == 0 {
                            if let Some(rect) = self.throttle.flush(now_us) {
                                let _ = self.ui.post(WM_APP_GESTURE, GestureEvent::Update(rect));
                            }
                        }
                    }
                    Throttled::Deferred | Throttled::Unchanged => {}
                }
            }
            Command::Gesture(event) => {
                // Begin, Commit and Cancel carry everything: drop pending updates.
                self.throttle.reset();
                self.kill_update_timer();
                let begin = matches!(event, GestureEvent::Begin { .. });
                if !self.ui.post(WM_APP_GESTURE, event) {
                    if begin {
                        self.logic.begin_failed();
                    } else {
                        self.log.write(LogLevel::Warn, format!("Could not post {event:?} to the UI."));
                    }
                }
            }
            Command::ArmHoldTimer(millis) => {
                self.kill_hold_timer();
                let millis = millis.max(USER_TIMER_MINIMUM);
                // SAFETY: a thread timer on the current thread.
                self.hold_timer = unsafe { SetTimer(None, 0, millis, None) };
                self.hold_deadline = Some(Instant::now() + Duration::from_millis(u64::from(millis)));
                if self.hold_timer == 0 {
                    self.logic.hold_timer_failed();
                }
            }
            Command::DisarmHoldTimer => self.kill_hold_timer(),
            Command::Wake => {
                // SAFETY: posting a payload-free message to this very thread.
                let posted = unsafe { PostThreadMessageW(self.thread_id, WM_HOOK_WAKE, WPARAM(0), LPARAM(0)) };
                if posted.is_err() {
                    // The queue is full: sending late from here beats never sending.
                    self.send_replays();
                }
            }
            Command::Disable { status, always_report } => {
                let was_enabled = self.settings.enabled.swap(false, Ordering::SeqCst);
                if was_enabled || always_report {
                    self.post_status(status);
                }
            }
            Command::Log(level, message) => self.log.write(level, message),
        }
    }

    /// Injects the queued replays with one `SendInput` call; fails open if
    /// Windows inserts fewer events than requested.
    fn send_replays(&mut self) {
        let events = self.logic.take_replays();
        if events.is_empty() {
            return;
        }
        let screen = VirtualScreen::current();
        let inputs: Vec<INPUT> = events.iter().map(|event| mouse_input(event, &screen)).collect();
        // SAFETY: `inputs` is a valid slice of INPUT structures of the given size.
        let inserted = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
        if inserted as usize != inputs.len() {
            // SAFETY: GetLastError has no preconditions.
            let error = unsafe { GetLastError() };
            self.logic.replay_failed(format!(
                "SendInput inserted {inserted} of {} events, error {}",
                inputs.len(),
                error.0
            ));
        }
    }

    fn post_status(&mut self, status: HookStatus) {
        if !self.ui.post(WM_APP_HOOK_STATUS, status) {
            self.log.write(LogLevel::Warn, "Could not post the input hook status to the UI.".to_owned());
        }
    }

    fn kill_hold_timer(&mut self) {
        self.hold_deadline = None;
        if self.hold_timer != 0 {
            // SAFETY: a thread timer created on this thread.
            unsafe {
                let _ = KillTimer(None, self.hold_timer);
            }
            self.hold_timer = 0;
        }
    }

    fn kill_update_timer(&mut self) {
        if self.update_timer != 0 {
            // SAFETY: a thread timer created on this thread.
            unsafe {
                let _ = KillTimer(None, self.update_timer);
            }
            self.update_timer = 0;
        }
    }
}

/// Decides whether a press may be held over the window under the pointer.
struct TargetProbe {
    own_pid: u32,
    own_integrity: u32,
    cache: IntegrityCache,
}

impl TargetProbe {
    fn new() -> Self {
        // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no closing.
        let own_integrity = unsafe { token_integrity(GetCurrentProcess()) };
        Self {
            // SAFETY: no preconditions.
            own_pid: unsafe { GetCurrentProcessId() },
            own_integrity: own_integrity.unwrap_or(SECURITY_MANDATORY_MEDIUM_RID as u32),
            cache: IntegrityCache::default(),
        }
    }

    /// Called from the mouse hook: only calls that read window-manager and
    /// process state, none of which send messages or wait on other threads.
    fn may_hold_at(&mut self, point: PointPx, now_ms: u64) -> bool {
        // SAFETY: WindowFromPoint only sends WM_NCHITTEST to windows of the
        // calling thread, and this thread has none.
        let hwnd = unsafe { WindowFromPoint(POINT { x: point.x, y: point.y }) };
        if hwnd.is_invalid() {
            return false;
        }
        let mut pid = 0u32;
        // SAFETY: `pid` is a valid out pointer.
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        let tedcat_class = is_tedcat_class(hwnd) || {
            // SAFETY: GetAncestor tolerates any window handle.
            let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
            !root.is_invalid() && root != hwnd && is_tedcat_class(root)
        };
        let integrity = if pid == 0 || pid == self.own_pid || tedcat_class {
            None
        } else {
            match self.cache.get(pid, now_ms) {
                Some(level) => level,
                None => {
                    let level = process_integrity(pid);
                    self.cache.insert(pid, level, now_ms);
                    level
                }
            }
        };
        may_hold_over(pid, tedcat_class, integrity, self.own_pid, self.own_integrity)
    }
}

fn is_tedcat_class(hwnd: HWND) -> bool {
    let mut name = [0u16; 64];
    // SAFETY: the buffer is valid for its length.
    let len = unsafe { GetClassNameW(hwnd, &mut name) };
    len > 0 && class_has_prefix(&name[..len as usize], CLASS_PREFIX)
}

/// Integrity level RID of process `pid`, or `None` if it cannot be read.
fn process_integrity(pid: u32) -> Option<u32> {
    // SAFETY: the handle is closed below; PROCESS_QUERY_LIMITED_INFORMATION is
    // the least access that allows opening the token.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    // SAFETY: `process` is a valid process handle.
    let level = unsafe { token_integrity(process) };
    // SAFETY: closing the handle opened above.
    unsafe {
        let _ = CloseHandle(process);
    }
    level
}

/// Integrity level RID from the token of `process`.
///
/// # Safety
/// `process` must be a valid process handle (or pseudo-handle).
unsafe fn token_integrity(process: HANDLE) -> Option<u32> {
    let mut token = HANDLE::default();
    // SAFETY: `token` is a valid out pointer.
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }.ok()?;
    // TOKEN_MANDATORY_LABEL plus its SID: well under 128 bytes; u64 for alignment.
    let mut buffer = [0u64; 16];
    let mut needed = 0u32;
    // SAFETY: the buffer is valid for the size given.
    let queried = unsafe {
        GetTokenInformation(
            token,
            TokenIntegrityLevel,
            Some(buffer.as_mut_ptr().cast()),
            std::mem::size_of_val(&buffer) as u32,
            &mut needed,
        )
    };
    // SAFETY: closing the token opened above.
    unsafe {
        let _ = CloseHandle(token);
    }
    queried.ok()?;
    // SAFETY: on success the buffer starts with a TOKEN_MANDATORY_LABEL whose SID
    // points inside the buffer, which is still alive.
    unsafe {
        let label = &*(buffer.as_ptr() as *const TOKEN_MANDATORY_LABEL);
        let sid = label.Label.Sid;
        if sid.is_invalid() {
            return None;
        }
        let count = GetSidSubAuthorityCount(sid);
        if count.is_null() || *count == 0 {
            return None;
        }
        let rid = GetSidSubAuthority(sid, u32::from(*count) - 1);
        if rid.is_null() {
            None
        } else {
            Some(*rid)
        }
    }
}

/// The pointer position in physical pixels (the process is per-monitor DPI aware).
fn cursor_position() -> Option<PointPx> {
    let mut point = POINT::default();
    // SAFETY: writes into a local POINT.
    unsafe { GetCursorPos(&mut point) }.ok().map(|()| PointPx::new(point.x, point.y))
}

fn key_down(key: VIRTUAL_KEY) -> bool {
    // SAFETY: no preconditions. The high bit means "currently down".
    (unsafe { GetAsyncKeyState(i32::from(key.0)) } as u16 & 0x8000) != 0
}

fn current_modifiers() -> Modifiers {
    modifiers_from_keys(
        key_down(VK_SHIFT),
        key_down(VK_CONTROL),
        key_down(VK_MENU),
        key_down(VK_LWIN) || key_down(VK_RWIN),
    )
}

/// The replayable action for a mouse message other than move and left button.
fn other_action(message: u32, mouse_data: u32) -> Option<ReplayAction> {
    let high = (mouse_data >> 16) as u16;
    Some(match message {
        WM_RBUTTONDOWN => ReplayAction::RightDown,
        WM_RBUTTONUP => ReplayAction::RightUp,
        WM_MBUTTONDOWN => ReplayAction::MiddleDown,
        WM_MBUTTONUP => ReplayAction::MiddleUp,
        WM_XBUTTONDOWN => ReplayAction::XDown(high),
        WM_XBUTTONUP => ReplayAction::XUp(high),
        WM_MOUSEWHEEL => ReplayAction::Wheel(high as i16),
        WM_MOUSEHWHEEL => ReplayAction::HWheel(high as i16),
        _ => return None,
    })
}

/// `MOUSEEVENTF_*` button flags and `mouseData` for an action.
fn action_flags(action: ReplayAction) -> (MOUSE_EVENT_FLAGS, u32) {
    match action {
        ReplayAction::Move => (MOUSE_EVENT_FLAGS(0), 0),
        ReplayAction::LeftDown => (MOUSEEVENTF_LEFTDOWN, 0),
        ReplayAction::LeftUp => (MOUSEEVENTF_LEFTUP, 0),
        ReplayAction::RightDown => (MOUSEEVENTF_RIGHTDOWN, 0),
        ReplayAction::RightUp => (MOUSEEVENTF_RIGHTUP, 0),
        ReplayAction::MiddleDown => (MOUSEEVENTF_MIDDLEDOWN, 0),
        ReplayAction::MiddleUp => (MOUSEEVENTF_MIDDLEUP, 0),
        ReplayAction::XDown(button) => (MOUSEEVENTF_XDOWN, u32::from(button)),
        ReplayAction::XUp(button) => (MOUSEEVENTF_XUP, u32::from(button)),
        ReplayAction::Wheel(delta) => (MOUSEEVENTF_WHEEL, i32::from(delta) as u32),
        ReplayAction::HWheel(delta) => (MOUSEEVENTF_HWHEEL, i32::from(delta) as u32),
    }
}

/// The virtual screen in physical pixels (the process is per-monitor DPI aware).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VirtualScreen {
    left: i32,
    top: i32,
    width: i32,
    height: i32,
}

impl VirtualScreen {
    fn current() -> Self {
        // SAFETY: GetSystemMetrics has no preconditions.
        unsafe {
            Self {
                left: GetSystemMetrics(SM_XVIRTUALSCREEN),
                top: GetSystemMetrics(SM_YVIRTUALSCREEN),
                width: GetSystemMetrics(SM_CXVIRTUALSCREEN),
                height: GetSystemMetrics(SM_CYVIRTUALSCREEN),
            }
        }
    }
}

/// One marked, absolute `SendInput` event. Every event carries `MOUSEEVENTF_MOVE`
/// so it happens at its own point.
fn mouse_input(event: &ReplayEvent, screen: &VirtualScreen) -> INPUT {
    let (buttons, data) = action_flags(event.action);
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: normalize_absolute(event.point.x, screen.left, screen.width),
                dy: normalize_absolute(event.point.y, screen.top, screen.height),
                mouseData: data,
                dwFlags: buttons | MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                time: 0,
                dwExtraInfo: TEDCAT_INPUT_MARKER,
            },
        },
    }
}

/// Log lines from the hook thread go through a separate thread, so a slow disk
/// can never delay the hook callbacks.
struct HookLog {
    sender: Option<mpsc::Sender<(LogLevel, String)>>,
}

impl HookLog {
    fn start() -> Self {
        let (sender, receiver) = mpsc::channel::<(LogLevel, String)>();
        let spawned = std::thread::Builder::new().name("TedCat hook log".to_owned()).spawn(move || {
            for (level, message) in receiver {
                write_log(level, &message);
            }
        });
        Self { sender: spawned.ok().map(|_| sender) }
    }

    fn write(&self, level: LogLevel, message: String) {
        match &self.sender {
            Some(sender) => {
                if let Err(mpsc::SendError((level, message))) = sender.send((level, message)) {
                    write_log(level, &message);
                }
            }
            // No logger thread: these lines are rare, write them directly.
            None => write_log(level, &message),
        }
    }
}

fn write_log(level: LogLevel, message: &str) {
    match level {
        LogLevel::Info => crate::log_info!("[input] {message}"),
        LogLevel::Warn => crate::log_warn!("[input] {message}"),
        LogLevel::Error => crate::log_error!("[input] {message}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_hook_keeps_the_auto_traits_of_the_contract_stub() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<InputHook>();
        assert_send_sync::<GestureSettings>();
    }

    #[test]
    fn marker_spells_tcat() {
        assert_eq!(&TEDCAT_INPUT_MARKER.to_be_bytes()[std::mem::size_of::<usize>() - 4..], b"TCAT");
    }

    #[test]
    fn other_buttons_and_wheels_are_replayable() {
        assert_eq!(other_action(WM_RBUTTONDOWN, 0), Some(ReplayAction::RightDown));
        assert_eq!(other_action(WM_MBUTTONUP, 0), Some(ReplayAction::MiddleUp));
        assert_eq!(other_action(WM_XBUTTONDOWN, 2 << 16), Some(ReplayAction::XDown(2)));
        let down_notch = ((-120i16) as u16 as u32) << 16;
        assert_eq!(other_action(WM_MOUSEWHEEL, down_notch), Some(ReplayAction::Wheel(-120)));
        assert_eq!(other_action(WM_MOUSEHWHEEL, 120 << 16), Some(ReplayAction::HWheel(120)));
        assert_eq!(other_action(WM_MOUSEMOVE, 0), None);
    }

    #[test]
    fn replays_are_absolute_marked_and_carry_wheel_data() {
        let screen = VirtualScreen { left: -1920, top: 0, width: 3840, height: 1080 };
        let input = mouse_input(&ReplayEvent::new(PointPx::new(-1920, 0), ReplayAction::LeftDown), &screen);
        assert_eq!(input.r#type, INPUT_MOUSE);
        // SAFETY: the union was initialized as `mi`.
        let mi = unsafe { input.Anonymous.mi };
        assert_eq!(mi.dwExtraInfo, TEDCAT_INPUT_MARKER);
        assert_eq!(
            mi.dwFlags,
            MOUSEEVENTF_LEFTDOWN | MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK
        );
        assert!(mi.dx < 20 && mi.dy < 40);

        let wheel = mouse_input(&ReplayEvent::new(PointPx::new(0, 0), ReplayAction::Wheel(-120)), &screen);
        // SAFETY: as above.
        let mi = unsafe { wheel.Anonymous.mi };
        assert_eq!(mi.mouseData as i32, -120);
        assert!(mi.dwFlags.contains(MOUSEEVENTF_WHEEL));
    }
}
