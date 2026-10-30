//! The UI-thread controller.
//!
//! CONTRACT (owned by the UI agent): `run` initializes COM (apartment threaded)
//! and Media Foundation users may assume COM is initialized on the UI thread;
//! enforces a single instance (named mutex `Local\TedCat.SingleInstance`; a
//! second launch exits quietly); creates two windows: a message-only window
//! (class `TedCat.Msg`) whose procedure handles `WM_APP_GESTURE`,
//! `WM_APP_HOOK_STATUS`, `WM_APP_RECORDER` (the `UiHandle` posts) and
//! `WM_APP_UI`, and a hidden top-level window (class `TedCat.Main`) for the
//! tray icon (`WM_APP_TRAY`), broadcasts and the end of the session; starts
//! the input hook; and runs the message loop until Quit. It returns the
//! process exit code.
//!
//! Payload messages only go to the message-only window, which broadcasts from
//! other processes never reach, and must carry the payload cookie
//! (`messages.rs`); the top-level window ignores `WM_APP_GESTURE`..
//! `WM_APP_RECORDER`, so a foreign `WM_APP + n` is never taken for a pointer.
//!
//! Recordings are never left unfinalized:
//! - On quit, the gesture is switched off first (nothing handles gestures any
//!   more), then the recording is finalized before the hook stops and TedCat
//!   exits.
//! - On `WM_QUERYENDSESSION` (logoff, shutdown) a running recording starts
//!   finalizing at once and a shutdown block reason names it;
//!   `WM_ENDSESSION` returns only once every save is complete (or after
//!   [`QUIT_SAVE_TIMEOUT`]), because Windows may end the process right after.
//! - The running `Recorder` and the saves in flight live in thread-locals
//!   outside the controller, so the session-end path reaches them even while
//!   the controller is borrowed or already quitting.
//!
//! How messages flow: the main window procedure turns every message it cares
//! about into an [`AppMsg`] (taking ownership of boxed payloads at once) and
//! queues it. The queue is drained by whichever call is not nested inside the
//! controller: if the controller is busy (a handler pumped messages, e.g.
//! inside a COM call) the message waits and runs right after the current
//! handler, so the controller is never re-entered. Modal loops (the tray
//! menu, the folder dialog) run outside the controller as [`Followup`]s, so
//! hook and recorder messages keep being handled while a menu is open.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tedcat_core::{recording_file_name, CaptureMode, Codec, RecordingTarget};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, HANDLE, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
use windows::Win32::System::Shutdown::{ShutdownBlockReasonCreate, ShutdownBlockReasonDestroy};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Controls::{InitCommonControlsEx, ICC_WIN95_CLASSES, INITCOMMONCONTROLSEX};
use windows::Win32::UI::Shell::{NINF_KEY, NIN_SELECT};
use windows::Win32::UI::WindowsAndMessaging::{
    ChangeWindowMessageFilterEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    KillTimer, MsgWaitForMultipleObjects, PeekMessageW, PostQuitMessage, RegisterWindowMessageW, SetTimer,
    TranslateMessage, HWND_MESSAGE, MSG, MSGFLT_ALLOW, PM_REMOVE, QS_ALLINPUT, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
    WM_CLOSE, WM_CONTEXTMENU, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_ENDSESSION, WM_QUERYENDSESSION, WM_QUIT,
    WM_SETTINGCHANGE, WM_TIMER, WS_EX_TOOLWINDOW, WS_POPUP,
};

use crate::capture::screenshot;
use crate::geom::{MonitorInfo, PointPx, RectPx};
use crate::input::{GestureSettings, InputHook, SelectionOverlay};
use crate::messages::{
    sentence, take_payload, GestureEvent, HookHandle, HookStatus, RecorderEvent, RecordingResult, UiHandle,
    WM_APP_GESTURE, WM_APP_HOOK_STATUS, WM_APP_RECORDER, WM_APP_TRAY,
};
use crate::record::{Recorder, RecorderOptions, Target};
use crate::settings::Settings;
use crate::ui::card::{CardInfo, SavedCard};
use crate::ui::hud::{Hud, HudMode};
use crate::ui::layout::farthest_corner;
use crate::ui::menu_model::{build_menu, Command, MenuEntry, MenuState, Phase};
use crate::ui::outline::Outline;
use crate::ui::sound::{self, Sound};
use crate::ui::tray::{Balloon, Tray};
use crate::ui::{autostart, dialogs, menu, shell, text, window, Placement, UiAction, WM_APP_UI};
use crate::util::paths;

const MAIN_CLASS: PCWSTR = w!("TedCat.Main");
/// The message-only window that receives every `UiHandle` post.
const MSG_CLASS: PCWSTR = w!("TedCat.Msg");
const MUTEX_NAME: PCWSTR = w!("Local\\TedCat.SingleInstance");

/// Main-window timer that retries adding the tray icon.
const TRAY_RETRY_TIMER: usize = 1;
const TRAY_RETRY_MILLIS: u32 = 2000;
const TRAY_RETRY_LIMIT: u32 = 30;
/// A tray click this soon after the menu closed is the click that closed it.
const MENU_REOPEN_GUARD: Duration = Duration::from_millis(300);
/// How long quitting, or the end of the session, waits for a recording to be finalized.
const QUIT_SAVE_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest single wait while pumping messages for a save.
const PUMP_SLICE: Duration = Duration::from_millis(50);
/// Shown by Windows on its shutdown screen while TedCat finalizes a recording.
const SHUTDOWN_REASON: PCWSTR = w!("Saving a recording");
/// Payload messages that did not come from this process are logged this many times.
const FOREIGN_LOG_LIMIT: u32 = 10;
/// `NIN_KEYSELECT` (Enter or Space on the focused tray icon).
const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;

const STOPPED_TITLE: &str = "TedCat stopped intercepting clicks";

/// The balloon for a hook status: title, text and kind (`None` for `Active`).
/// The reasons are sentences (`messages::sentence`); the text appends whole
/// sentences after them.
fn hook_status_notice(status: &HookStatus) -> Option<(&'static str, String, Balloon)> {
    match status {
        HookStatus::Active => None,
        HookStatus::Unavailable(reason) => Some((
            STOPPED_TITLE,
            format!(
                "Windows did not let TedCat watch the mouse, so clicks are left alone. {reason} \
                 To try again, choose Enabled in the TedCat menu."
            ),
            Balloon::Warning,
        )),
        HookStatus::Degraded(reason) => Some((
            STOPPED_TITLE,
            format!(
                "{reason} TedCat let go of the mouse to keep clicks working. \
                 To turn it back on, choose Enabled in the TedCat menu."
            ),
            Balloon::Warning,
        )),
        HookStatus::KillSwitch => Some((
            "TedCat is paused",
            "Escape was pressed 3 times, so TedCat stopped intercepting clicks. \
             To turn it back on, choose Enabled in the TedCat menu."
                .to_owned(),
            Balloon::Info,
        )),
    }
}

/// The balloon text when a recording stops on its own. `reason` is a sentence.
fn interrupted_notice(reason: &str) -> String {
    format!("{reason} TedCat saves what was captured.")
}

/// A Windows error as a sentence for the user: its message, or its code when
/// Windows has no text for it.
fn error_sentence(error: &windows::core::Error) -> String {
    let message = error.message();
    if message.trim().is_empty() {
        format!("Windows error 0x{:08X}.", error.code().0 as u32)
    } else {
        sentence(&message)
    }
}

/// Runs TedCat until the user quits. Returns the process exit code.
pub fn run() -> i32 {
    crate::log_info!("TedCat {} starting", env!("CARGO_PKG_VERSION"));
    // SAFETY: first COM/OLE initialization on the UI thread, balanced below.
    let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
    if com.is_err() {
        crate::log_error!("CoInitializeEx failed: {}", windows::core::Error::from_hresult(com));
    }
    // OLE (on top of the STA) is needed for drag and drop out of the saved card.
    // SAFETY: as above.
    let ole = unsafe { OleInitialize(None) };
    if let Err(error) = &ole {
        crate::log_error!("OleInitialize failed: {error}");
    }
    let code = run_app();
    // SAFETY: balances the successful initializations above.
    unsafe {
        if ole.is_ok() {
            OleUninitialize();
        }
        if com.is_ok() {
            CoUninitialize();
        }
    }
    code
}

fn run_app() -> i32 {
    let Some(_instance) = SingleInstance::acquire() else {
        crate::log_info!("another TedCat is already running; exiting");
        return 0;
    };
    let controls =
        INITCOMMONCONTROLSEX { dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_WIN95_CLASSES };
    // SAFETY: registers the common control classes (tooltips) for this process.
    unsafe {
        let _ = InitCommonControlsEx(&controls);
    }
    let main = match create_main_window() {
        Ok(main) => main,
        Err(error) => {
            crate::log_error!("could not create the main window: {error}");
            return 1;
        }
    };
    let msg_window = match create_message_window() {
        Ok(msg_window) => msg_window,
        Err(error) => {
            crate::log_error!("could not create the message window: {error}");
            // SAFETY: destroying the window created above.
            unsafe {
                let _ = DestroyWindow(main);
            }
            return 1;
        }
    };
    UI.set(Some(UiHandle::new(msg_window)));
    // SAFETY: registering a well-known message name; allowing it through UIPI
    // matters only when TedCat runs elevated and Explorer does not.
    unsafe {
        let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
        TASKBAR_CREATED.set(taskbar_created);
        if taskbar_created != 0 {
            let _ = ChangeWindowMessageFilterEx(main, taskbar_created, MSGFLT_ALLOW, None);
        }
    }

    let controller = Controller::new(main, msg_window);
    CONTROLLER.with(|slot| *slot.borrow_mut() = Some(controller));
    drain();

    let code = message_loop();

    let controller = CONTROLLER.with(|slot| slot.try_borrow_mut().ok().and_then(|mut slot| slot.take()));
    if let Some(controller) = controller {
        controller.shutdown();
    }
    QUEUE.with(|queue| queue.borrow_mut().clear());
    UI.set(None);
    // SAFETY: destroying our own windows after the loop ended.
    unsafe {
        let _ = DestroyWindow(msg_window);
        let _ = DestroyWindow(main);
    }
    crate::log_info!("TedCat stopped");
    code
}

/// Holds the single-instance mutex for the life of the process.
struct SingleInstance(HANDLE);

impl SingleInstance {
    /// `None` when another TedCat already holds the mutex.
    fn acquire() -> Option<SingleInstance> {
        // SAFETY: creates or opens a named mutex; GetLastError is read right after.
        unsafe {
            match CreateMutexW(None, false, MUTEX_NAME) {
                Ok(handle) => {
                    if GetLastError() == ERROR_ALREADY_EXISTS {
                        let _ = CloseHandle(handle);
                        None
                    } else {
                        Some(SingleInstance(handle))
                    }
                }
                // Held by an instance we may not open (e.g. an elevated one).
                Err(error) if error.code() == ERROR_ACCESS_DENIED.to_hresult() => None,
                Err(error) => {
                    crate::log_warn!("single-instance mutex unavailable ({error}); continuing");
                    Some(SingleInstance(HANDLE::default()))
                }
            }
        }
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: closes the mutex handle we own.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// The hidden main window: a real top-level window (not message-only), so it
/// receives broadcasts such as `TaskbarCreated` and `WM_SETTINGCHANGE`, and
/// `WM_QUERYENDSESSION` / `WM_ENDSESSION`. Being top-level, it also receives
/// other processes' `HWND_BROADCAST` posts, which is why payload messages go
/// to the message-only window instead.
fn create_main_window() -> windows::core::Result<HWND> {
    window::register_class(MAIN_CLASS, Some(main_proc), None)?;
    // SAFETY: standard window creation; the window is never shown.
    unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            MAIN_CLASS,
            w!("TedCat"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(window::instance()),
            None,
        )
    }
}

/// The message-only window (parent `HWND_MESSAGE`): it receives the other
/// threads' payload posts and the pill's and card's actions, and never sees
/// broadcasts.
fn create_message_window() -> windows::core::Result<HWND> {
    window::register_class(MSG_CLASS, Some(msg_proc), None)?;
    // SAFETY: standard window creation with the message-only parent.
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            MSG_CLASS,
            w!("TedCat"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(window::instance()),
            None,
        )
    }
}

fn message_loop() -> i32 {
    let mut message = MSG::default();
    loop {
        // SAFETY: the standard message loop on the UI thread.
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        match result.0 {
            0 => return message.wParam.0 as i32,
            -1 => {
                crate::log_error!("GetMessageW failed: {}", windows::core::Error::from_thread());
                return 1;
            }
            _ => {
                // SAFETY: as above.
                unsafe {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
        }
    }
}

/// Dispatches window messages until `finished` returns true or `timeout`
/// passes, so COM calls and posts that need this STA thread keep working
/// meanwhile (and a nested `WM_ENDSESSION` is answered). A `WM_QUIT` taken
/// while pumping is posted again before returning. Returns whether `finished`
/// became true.
fn pump_until(mut finished: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let mut quit_code = None;
    let done = loop {
        if finished() {
            break true;
        }
        let now = Instant::now();
        if now >= deadline {
            break false;
        }
        let wait = (deadline - now).min(PUMP_SLICE).as_millis().max(1) as u32;
        // SAFETY: waits for input or `wait` ms on this thread; then dispatches what arrived.
        unsafe {
            MsgWaitForMultipleObjects(None, false, wait, QS_ALLINPUT);
            let mut message = MSG::default();
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                if message.message == WM_QUIT {
                    quit_code = Some(message.wParam.0 as i32);
                    continue;
                }
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    };
    if let Some(code) = quit_code {
        // SAFETY: re-posts the WM_QUIT consumed while pumping.
        unsafe { PostQuitMessage(code) };
    }
    done
}

// ---------------------------------------------------------------------------
// Message plumbing

/// A message for the controller, already decoded (payloads owned).
enum AppMsg {
    Gesture(GestureEvent),
    Hook(HookStatus),
    Recorder(RecorderEvent),
    Tray {
        event: u32,
        anchor: PointPx,
    },
    Ui {
        action: UiAction,
        generation: u32,
    },
    TaskbarCreated,
    /// Theme, DPI, display or work-area change.
    Environment,
    Timer(usize),
    MenuCommand(u32),
    FolderPicked(PathBuf),
    Close,
}

/// Work that runs a modal loop, so it runs outside the controller.
enum Followup {
    ShowMenu { owner: HWND, entries: Vec<MenuEntry>, at: PointPx },
    PickFolder { initial: PathBuf },
    Quit,
}

thread_local! {
    static CONTROLLER: RefCell<Option<Controller>> = const { RefCell::new(None) };
    static QUEUE: RefCell<VecDeque<AppMsg>> = const { RefCell::new(VecDeque::new()) };
    /// True while a menu or dialog runs its modal loop.
    static MODAL: Cell<bool> = const { Cell::new(false) };
    static MENU_CLOSED_AT: Cell<Option<Instant>> = const { Cell::new(None) };
    static TASKBAR_CREATED: Cell<u32> = const { Cell::new(0) };
    /// The message-only window, for posts made outside the controller.
    static UI: Cell<Option<UiHandle>> = const { Cell::new(None) };
    /// The running input hook, for requests made outside the controller.
    static HOOK_REQUESTS: Cell<Option<HookHandle>> = const { Cell::new(None) };
    /// The gesture settings the hook reads (the controller's), so the end of
    /// the session can stop holding presses even while the controller is borrowed.
    static GESTURE: RefCell<Option<Arc<GestureSettings>>> = const { RefCell::new(None) };
    /// The running recorder. Outside the controller, so the end of the session
    /// can always finalize it, even while the controller is borrowed.
    static RECORDER: RefCell<Option<Recorder>> = const { RefCell::new(None) };
    /// Recordings being finalized on worker threads (at most one in practice).
    static SAVES: RefCell<Vec<SaveJob>> = const { RefCell::new(Vec::new()) };
    /// The main window holds a shutdown block reason.
    static SHUTDOWN_BLOCKED: Cell<bool> = const { Cell::new(false) };
    /// Payload messages ignored because they did not come from this process.
    static FOREIGN_MESSAGES: Cell<u32> = const { Cell::new(0) };
}

fn enqueue(message: AppMsg) {
    QUEUE.with(|queue| queue.borrow_mut().push_back(message));
}

fn dispatch(message: AppMsg) {
    enqueue(message);
    drain();
}

/// Handles queued messages until the queue is empty. Returns at once when
/// called from inside a controller handler; that outer drain continues.
fn drain() {
    loop {
        let busy = CONTROLLER.with(|slot| slot.try_borrow_mut().is_err());
        if busy {
            return;
        }
        let Some(message) = QUEUE.with(|queue| queue.borrow_mut().pop_front()) else { return };
        let followup = CONTROLLER.with(|slot| match slot.try_borrow_mut() {
            Ok(mut slot) => match slot.as_mut() {
                Some(controller) => controller.handle(message),
                None => {
                    handle_orphan(message);
                    None
                }
            },
            Err(_) => None,
        });
        if let Some(followup) = followup {
            run_followup(followup);
        }
    }
}

/// Runs `f` on the controller when it exists and is not borrowed by a handler
/// further up the stack. `None` otherwise.
fn with_free_controller<R>(f: impl FnOnce(&mut Controller) -> R) -> Option<R> {
    CONTROLLER.with(|slot| {
        let mut slot = slot.try_borrow_mut().ok()?;
        slot.as_mut().map(f)
    })
}

/// A message that arrived while there is no controller (TedCat is quitting,
/// or the session is ending).
fn handle_orphan(message: AppMsg) {
    match message {
        // Nobody shows this selection: the press goes back to the app.
        AppMsg::Gesture(GestureEvent::Begin { origin, .. }) => refuse_selection(origin),
        AppMsg::Recorder(RecorderEvent::Finished(Ok(recording))) => crate::log_info!(
            "saved {} ({} bytes, {:.1} s, audio: {}) while quitting",
            recording.path.display(),
            recording.bytes,
            recording.duration_secs,
            recording.has_audio
        ),
        AppMsg::Recorder(RecorderEvent::Finished(Err(reason))) => {
            crate::log_error!("recording failed while quitting: {reason}")
        }
        _ => {}
    }
}

/// The session is ending while the controller is out of reach: nothing will
/// handle gestures any more, so a press held now would be swallowed. Tells the
/// hook to stop holding presses, as `Controller::shutdown` does; a press
/// already pending is handed back when its hold timer fires. Only the flag the
/// hook reads changes, never the saved setting, so the gesture is on again at
/// the next start.
fn stop_holding_presses() {
    GESTURE.with(|gesture| {
        if let Some(gesture) = gesture.borrow().as_ref() {
            gesture.enabled.store(false, Ordering::SeqCst);
        }
    });
}

/// Asks the input hook to hand back the press of the selection that began at
/// `origin`, because no overlay will be shown for it.
fn refuse_selection(origin: PointPx) {
    match HOOK_REQUESTS.get() {
        Some(hook) if hook.refuse_selection(origin) => {
            crate::log_warn!("no selection overlay for the press at {},{}; it goes back to the app", origin.x, origin.y)
        }
        Some(_) => crate::log_error!("could not ask the input hook to give the press back"),
        None => {}
    }
}

/// Logs a payload message that did not come from this process (a broadcast
/// on the main window, or a post with the wrong cookie), the first
/// [`FOREIGN_LOG_LIMIT`] times.
fn ignore_foreign_message(window: &str, message: u32, wparam: WPARAM, lparam: LPARAM) {
    let count = FOREIGN_MESSAGES.get().saturating_add(1);
    FOREIGN_MESSAGES.set(count);
    if count <= FOREIGN_LOG_LIMIT {
        crate::log_warn!(
            "ignored WM_APP+{} on the {window} window that TedCat did not post (wParam {:#x}, lParam {:#x})",
            message.wrapping_sub(WM_APP),
            wparam.0,
            lparam.0
        );
    }
    if count == FOREIGN_LOG_LIMIT {
        crate::log_warn!("further foreign messages are not logged");
    }
}

fn run_followup(followup: Followup) {
    match followup {
        Followup::ShowMenu { owner, entries, at } => {
            if MODAL.replace(true) {
                return;
            }
            let chosen = menu::show_popup(owner, &entries, at);
            MODAL.set(false);
            MENU_CLOSED_AT.set(Some(Instant::now()));
            if let Some(id) = chosen {
                enqueue(AppMsg::MenuCommand(id));
            }
        }
        Followup::PickFolder { initial } => {
            if MODAL.replace(true) {
                return;
            }
            let picked = dialogs::pick_folder(None, &initial, "Choose Recordings Folder");
            MODAL.set(false);
            if let Some(folder) = picked {
                enqueue(AppMsg::FolderPicked(folder));
            }
        }
        // SAFETY: ends the message loop of this (the UI) thread.
        Followup::Quit => unsafe { PostQuitMessage(0) },
    }
}

/// Signed (x, y) packed in the low and high words of a `WPARAM`.
fn packed_point(wparam: WPARAM) -> PointPx {
    let (x, y) = window::lparam_point(LPARAM(wparam.0 as isize));
    PointPx::new(x, y)
}

/// The top-level window: tray callbacks, broadcasts, quit and session end.
extern "system" fn main_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let decoded = match message {
        // Payloads are only ever posted to the message-only window. Here they
        // can only be other processes' broadcasts: never touch their LPARAM.
        WM_APP_GESTURE | WM_APP_HOOK_STATUS | WM_APP_RECORDER => {
            ignore_foreign_message("main", message, wparam, lparam);
            return LRESULT(0);
        }
        // NOTIFYICON_VERSION_4: LOWORD(lParam) is the event, wParam the anchor point.
        WM_APP_TRAY => AppMsg::Tray { event: (lparam.0 & 0xFFFF) as u32, anchor: packed_point(wparam) },
        WM_SETTINGCHANGE | WM_DPICHANGED | WM_DISPLAYCHANGE => AppMsg::Environment,
        WM_TIMER => AppMsg::Timer(wparam.0),
        WM_CLOSE => AppMsg::Close,
        WM_QUERYENDSESSION => {
            query_end_session(hwnd);
            // TedCat never vetoes the end of the session.
            return LRESULT(1);
        }
        WM_ENDSESSION => {
            if wparam.0 != 0 {
                end_session(hwnd);
            }
            // Otherwise the end was cancelled: a save started by
            // WM_QUERYENDSESSION goes on and releases the block when done.
            return LRESULT(0);
        }
        _ if message != 0 && message == TASKBAR_CREATED.get() => AppMsg::TaskbarCreated,
        // SAFETY: default handling for everything else.
        _ => return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    };
    dispatch(decoded);
    LRESULT(0)
}

/// The message-only window: the other threads' payload posts and the
/// actions of TedCat's own windows (pill, card).
extern "system" fn msg_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY (payloads): `take_payload` only takes messages that carry this
    // process's cookie, which only `UiHandle::post` sends, always with the
    // payload type that belongs to the message id; each is taken once, here.
    let payload = match message {
        WM_APP_GESTURE => unsafe { take_payload::<GestureEvent>(wparam, lparam) }.map(|event| AppMsg::Gesture(*event)),
        WM_APP_HOOK_STATUS => unsafe { take_payload::<HookStatus>(wparam, lparam) }.map(|status| AppMsg::Hook(*status)),
        WM_APP_RECORDER => {
            unsafe { take_payload::<RecorderEvent>(wparam, lparam) }.map(|event| AppMsg::Recorder(*event))
        }
        WM_APP_UI => {
            if let Some(action) = UiAction::from_code(wparam.0) {
                dispatch(AppMsg::Ui { action, generation: lparam.0 as u32 });
            }
            return LRESULT(0);
        }
        // SAFETY: default handling for everything else.
        _ => return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    };
    match payload {
        Some(decoded) => dispatch(decoded),
        None => ignore_foreign_message("message", message, wparam, lparam),
    }
    LRESULT(0)
}

// ---------------------------------------------------------------------------
// Recorder ownership, saves and the end of the session

/// True while a recorder is capturing (not yet handed to a save).
fn recorder_running() -> bool {
    RECORDER.with(|slot| slot.borrow().is_some())
}

/// A recording being finalized on a worker thread.
struct SaveJob {
    /// Turns true once `Recorder::stop` returned: the file is complete (or was
    /// deleted because it could not be finalized).
    done: Arc<AtomicBool>,
    /// Returns the result when the save was started without `notify`.
    worker: JoinHandle<Option<StopResult>>,
}

impl SaveJob {
    fn is_done(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }
}

/// Sets the flag when dropped: at the end of the save, early return or panic.
struct SetOnDrop(Arc<AtomicBool>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// True while a save has not finished.
fn saves_pending() -> bool {
    SAVES.with(|saves| saves.borrow().iter().any(|job| !job.is_done()))
}

/// Pumps messages until every save in flight is done, for at most `timeout`.
/// Returns false on timeout.
fn wait_for_saves(timeout: Duration) -> bool {
    pump_until(|| !saves_pending(), timeout)
}

/// Joins the finished saves and returns their results (`Some` only for saves
/// started without `notify`; the others posted theirs).
fn reap_saves() -> Vec<Option<StopResult>> {
    let finished: Vec<SaveJob> = SAVES.with(|saves| {
        let mut saves = saves.borrow_mut();
        let (finished, pending) = std::mem::take(&mut *saves).into_iter().partition(SaveJob::is_done);
        *saves = pending;
        finished
    });
    finished
        .into_iter()
        .map(|job| {
            job.worker.join().unwrap_or_else(|_| {
                crate::log_error!("the recorder stop thread panicked");
                None
            })
        })
        .collect()
}

/// What [`begin_save`] did.
enum SaveStart {
    /// No recorder was running.
    Nothing,
    /// A worker thread is finalizing it (see [`SAVES`]).
    Started,
    /// No thread could be started, so it was finalized right here.
    Stopped(StopResult),
}

/// Starts finalizing the running recorder, if any, on a worker thread and
/// records the save in [`SAVES`]. With `notify`, the result is posted as
/// `RecorderEvent::Finished`; without, [`reap_saves`] returns it.
fn begin_save(notify: Option<UiHandle>) -> SaveStart {
    let Some(recorder) = RECORDER.with(|slot| slot.borrow_mut().take()) else { return SaveStart::Nothing };
    match spawn_stop(recorder, notify) {
        Ok(job) => {
            SAVES.with(|saves| saves.borrow_mut().push(job));
            SaveStart::Started
        }
        Err(result) => SaveStart::Stopped(result),
    }
}

/// Asks Windows to name TedCat on its shutdown screen while a recording is saved.
fn block_shutdown(main: HWND) {
    if SHUTDOWN_BLOCKED.get() {
        return;
    }
    // SAFETY: `main` is a top-level window created by this (its owning) thread.
    match unsafe { ShutdownBlockReasonCreate(main, SHUTDOWN_REASON) } {
        Ok(()) => {
            SHUTDOWN_BLOCKED.set(true);
            crate::log_info!("shutdown block reason set while the recording is saved");
        }
        Err(error) => crate::log_warn!("could not set a shutdown block reason: {error}"),
    }
}

/// Removes the shutdown block reason once nothing is recording or saving.
fn release_shutdown_block(main: HWND) {
    if !SHUTDOWN_BLOCKED.get() || recorder_running() || saves_pending() {
        return;
    }
    SHUTDOWN_BLOCKED.set(false);
    // SAFETY: removes the reason this thread set on its own window.
    if let Err(error) = unsafe { ShutdownBlockReasonDestroy(main) } {
        crate::log_warn!("could not remove the shutdown block reason: {error}");
    }
}

/// `WM_QUERYENDSESSION`: TedCat never vetoes, but a running recording starts
/// finalizing now, so the file is complete as early as possible, and the
/// shutdown screen names TedCat while it saves.
fn query_end_session(main: HWND) {
    if !recorder_running() && !saves_pending() {
        return;
    }
    crate::log_info!("the session is ending while a recording is active; saving it now");
    block_shutdown(main);
    if recorder_running() {
        // Through the controller when it is free, so the pill shows "Saving".
        let _ = with_free_controller(Controller::stop_recording);
    }
    if recorder_running() {
        // The controller is busy further up the stack: stop from here; it gets
        // the result as RecorderEvent::Finished like after any stop.
        crate::log_info!("stopping the recording");
        let ui = UI.get();
        if let SaveStart::Stopped(result) = begin_save(ui) {
            if !ui.is_some_and(|ui| ui.post(WM_APP_RECORDER, RecorderEvent::Finished(result))) {
                crate::log_error!("could not report the stopped recording to the UI");
            }
        }
    }
}

/// `WM_ENDSESSION` (ending): Windows may end the process as soon as this
/// returns, so it returns only once every recording is finalized.
fn end_session(main: HWND) {
    crate::log_info!("session ending");
    let controller = CONTROLLER.with(|slot| slot.try_borrow_mut().ok().and_then(|mut slot| slot.take()));
    match controller {
        // Finalizes the recording and waits for every save itself.
        Some(controller) => controller.shutdown(),
        // A quit already in progress (it owns the controller), or a controller
        // borrowed further up the stack: finalize from here. Presses pass
        // straight through while this waits (the quit already did this).
        None => {
            stop_holding_presses();
            block_shutdown(main);
            match begin_save(None) {
                SaveStart::Started => crate::log_info!("finalizing the recording before the session ends"),
                SaveStart::Stopped(result) => log_final_result(Some(result)),
                SaveStart::Nothing => {}
            }
            if saves_pending() {
                crate::log_info!("waiting for the recording to be saved before the session ends");
                if !wait_for_saves(QUIT_SAVE_TIMEOUT) {
                    crate::log_error!("the recording did not finish in time");
                }
            }
            for result in reap_saves() {
                log_final_result(result);
            }
        }
    }
    release_shutdown_block(main);
    // SAFETY: ends the message loop of this thread.
    unsafe { PostQuitMessage(0) };
}

/// Logs the result of a save that nobody else reports (quit or session end).
fn log_final_result(result: Option<StopResult>) {
    match result {
        Some(Ok(recording)) => crate::log_info!(
            "saved {} ({} bytes, {:.1} s) before quitting",
            recording.path.display(),
            recording.bytes,
            recording.duration_secs
        ),
        Some(Err(reason)) => crate::log_error!("recording failed while quitting: {reason}"),
        // Posted as RecorderEvent::Finished instead.
        None => {}
    }
}

// ---------------------------------------------------------------------------
// The controller

type StopResult = Result<RecordingResult, String>;

/// Where the recording lifecycle is. The recorder itself lives in [`RECORDER`]
/// and the save in [`SAVES`], outside the controller.
enum Recording {
    Idle,
    /// `Recorder::start` is running (synchronously, on this thread).
    Starting,
    /// Capturing. If the end of the session already started the save from
    /// outside the controller, [`RECORDER`] is empty and `Finished` follows.
    Running { audio_only: bool, placement: Placement },
    /// A worker thread is finalizing the file; `RecorderEvent::Finished` follows.
    Saving { audio_only: bool, placement: Placement },
}

/// Runs `Recorder::stop` on a worker thread. With `notify`, the result is
/// posted to the UI thread as `RecorderEvent::Finished`; without, the thread
/// returns it. The job's `done` flag turns true as soon as `stop` returns. If
/// no thread can be started, stops right here and returns the result as the
/// error value.
fn spawn_stop(recorder: Recorder, notify: Option<UiHandle>) -> Result<SaveJob, StopResult> {
    let slot = Arc::new(Mutex::new(Some(recorder)));
    let worker_slot = Arc::clone(&slot);
    let done = Arc::new(AtomicBool::new(false));
    let finished = SetOnDrop(Arc::clone(&done));
    let spawned = std::thread::Builder::new().name("TedCat recorder stop".to_owned()).spawn(move || {
        let recorder = worker_slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take()?;
        let result = recorder.stop();
        // The file is final: a session end waiting for it may let Windows go on.
        drop(finished);
        match notify {
            Some(ui) => {
                if !ui.post(WM_APP_RECORDER, RecorderEvent::Finished(result)) {
                    crate::log_error!("could not report the finished recording to the UI");
                }
                None
            }
            None => Some(result),
        }
    });
    match spawned {
        Ok(worker) => Ok(SaveJob { done, worker }),
        Err(error) => {
            crate::log_error!("could not start the stop thread ({error}); stopping on the UI thread");
            let recorder = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take();
            Err(match recorder {
                Some(recorder) => recorder.stop(),
                None => Err("The recording could not be stopped.".to_owned()),
            })
        }
    }
}

fn center(rect: RectPx) -> PointPx {
    PointPx::new(
        ((rect.left as i64 + rect.right as i64) / 2) as i32,
        ((rect.top as i64 + rect.bottom as i64) / 2) as i32,
    )
}

/// Windows paths compare case-insensitively.
fn same_folder(a: &std::path::Path, b: &std::path::Path) -> bool {
    let normalize = |p: &std::path::Path| p.display().to_string().trim_end_matches(['\\', '/']).to_lowercase();
    normalize(a) == normalize(b)
}

struct Controller {
    /// The hidden top-level window (tray icon, clipboard owner, menu owner).
    main: HWND,
    /// The message-only window: target of every `UiHandle` post and of the
    /// pill's and card's actions.
    msg_window: HWND,
    settings: Settings,
    gesture: Arc<GestureSettings>,
    hook: Option<InputHook>,
    hook_status: HookStatus,
    tray: Tray,
    tray_retries: u32,
    overlay: Option<SelectionOverlay>,
    recording: Recording,
    hud: Option<Hud>,
    outline: Option<Outline>,
    card: Option<SavedCard>,
    next_generation: u32,
}

impl Controller {
    fn new(main: HWND, msg_window: HWND) -> Controller {
        let first_run = !Settings::path().exists();
        let settings = Settings::load();
        crate::log_info!("settings: {settings:?}");
        let gesture = GestureSettings::new(settings.enabled, settings.hold_millis, settings.trigger.modifiers());
        GESTURE.with(|slot| *slot.borrow_mut() = Some(Arc::clone(&gesture)));
        let mut controller = Controller {
            main,
            msg_window,
            settings,
            gesture,
            hook: None,
            hook_status: HookStatus::Active,
            tray: Tray::new(main),
            tray_retries: 0,
            overlay: None,
            recording: Recording::Idle,
            hud: None,
            outline: None,
            card: None,
            next_generation: 1,
        };
        if !controller.tray.ensure_added() {
            controller.schedule_tray_retry();
        }
        match InputHook::start(controller.ui(), Arc::clone(&controller.gesture)) {
            Ok(hook) => {
                HOOK_REQUESTS.set(Some(hook.handle()));
                controller.hook = Some(hook);
            }
            Err(error) => {
                crate::log_error!("input hook could not start: {error}");
                controller.on_hook_status(HookStatus::Unavailable(sentence(&error.to_string())));
            }
        }
        if first_run {
            controller.save_settings();
            let [screenshot_hint, recording_hint] = text::gesture_hints(controller.settings.trigger);
            controller.notice(
                "TedCat is running",
                &format!("{screenshot_hint}. {recording_hint}. The TedCat menu is in the notification area."),
                Balloon::Info,
            );
        }
        controller
    }

    fn handle(&mut self, message: AppMsg) -> Option<Followup> {
        match message {
            AppMsg::Gesture(event) => self.on_gesture(event),
            AppMsg::Hook(status) => self.on_hook_status(status),
            AppMsg::Recorder(event) => self.on_recorder(event),
            AppMsg::Tray { event, anchor } => return self.on_tray(event, anchor),
            AppMsg::Ui { action, generation } => self.on_ui(action, generation),
            AppMsg::TaskbarCreated => {
                crate::log_info!("taskbar (re)created");
                if !self.tray.readd() {
                    self.schedule_tray_retry();
                }
            }
            AppMsg::Environment => self.tray.refresh(),
            AppMsg::Timer(TRAY_RETRY_TIMER) => {
                self.tray_retries += 1;
                if self.tray.ensure_added() || self.tray_retries >= TRAY_RETRY_LIMIT {
                    // SAFETY: our own timer on our own window.
                    unsafe {
                        let _ = KillTimer(Some(self.main), TRAY_RETRY_TIMER);
                    }
                }
            }
            AppMsg::Timer(_) => {}
            AppMsg::MenuCommand(id) => match Command::from_id(id) {
                Some(command) => return self.on_command(command),
                None => crate::log_warn!("unknown menu id {id}"),
            },
            AppMsg::FolderPicked(folder) => self.on_folder_picked(folder),
            AppMsg::Close => return Some(Followup::Quit),
        }
        None
    }

    /// Where the other threads post to.
    fn ui(&self) -> UiHandle {
        UiHandle::new(self.msg_window)
    }

    fn schedule_tray_retry(&mut self) {
        self.tray_retries = 0;
        // SAFETY: a timer on our own window.
        unsafe {
            SetTimer(Some(self.main), TRAY_RETRY_TIMER, TRAY_RETRY_MILLIS, None);
        }
    }

    fn generation(&mut self) -> u32 {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        generation
    }

    fn notice(&self, title: &str, text: &str, kind: Balloon) {
        self.tray.balloon(title, text, kind);
    }

    fn save_settings(&self) {
        if let Err(error) = self.settings.save() {
            crate::log_error!("could not save settings: {error}");
        }
    }

    fn phase(&self) -> Phase {
        match self.recording {
            Recording::Idle => Phase::Idle,
            Recording::Starting => Phase::Starting,
            Recording::Running { .. } if recorder_running() => Phase::Recording,
            // The end of the session already started saving it.
            Recording::Running { .. } | Recording::Saving { .. } => Phase::Saving,
        }
    }

    /// The gesture is live: enabled and the hook is installed.
    fn gesture_active(&self) -> bool {
        self.gesture.enabled.load(Ordering::SeqCst) && !matches!(self.hook_status, HookStatus::Unavailable(_))
    }

    /// Keeps the hook (Shift + hold passes through while busy) and the tray icon in step.
    fn sync_recording_flags(&mut self) {
        let busy = !matches!(self.recording, Recording::Idle);
        self.gesture.allows_recording.store(!busy, Ordering::SeqCst);
        self.tray.set_recording(busy);
    }

    // --- gestures ---------------------------------------------------------

    fn on_gesture(&mut self, event: GestureEvent) {
        match event {
            GestureEvent::Begin { origin, mode } => {
                if let Some(stale) = self.overlay.take() {
                    stale.close();
                }
                match SelectionOverlay::show(origin, mode) {
                    Ok(overlay) => self.overlay = Some(overlay),
                    Err(error) => {
                        // Without an overlay the user sees nothing happen, so the
                        // press must reach the app instead of being swallowed.
                        crate::log_error!("could not show the selection overlay: {error}");
                        refuse_selection(origin);
                    }
                }
            }
            GestureEvent::Update(rect) => {
                if let Some(overlay) = self.overlay.as_mut() {
                    overlay.update(rect);
                }
            }
            GestureEvent::Commit { rect, mode: CaptureMode::Screenshot } => self.take_screenshot(rect),
            GestureEvent::Commit { rect, mode: CaptureMode::Recording } => self.commit_recording(rect),
            GestureEvent::Cancel => {
                if let Some(overlay) = self.overlay.take() {
                    overlay.close();
                }
            }
        }
    }

    /// The selection clamped to its monitor, and that monitor.
    fn clamp_selection(overlay: Option<&SelectionOverlay>, rect: RectPx) -> (RectPx, MonitorInfo) {
        match overlay {
            Some(overlay) => (overlay.clamp(rect), overlay.monitor()),
            None => {
                let monitor = MonitorInfo::from_point(center(rect));
                (rect.intersect(&monitor.rect), monitor)
            }
        }
    }

    fn take_screenshot(&mut self, rect: RectPx) {
        let overlay = self.overlay.take();
        let (clamped, _) = Self::clamp_selection(overlay.as_ref(), rect);
        if clamped.is_empty() {
            if let Some(overlay) = overlay {
                overlay.close();
            }
            return;
        }
        if let Some(overlay) = &overlay {
            overlay.hide_for_capture();
        }
        match screenshot::copy_region_to_clipboard(clamped, self.main) {
            Ok(()) => {
                if let Some(overlay) = overlay {
                    overlay.flash_and_close(clamped);
                }
                crate::log_info!("screenshot {}x{} copied", clamped.width(), clamped.height());
                if self.settings.play_sounds {
                    sound::play(Sound::ScreenshotTaken);
                }
            }
            Err(error) => {
                if let Some(overlay) = overlay {
                    overlay.close();
                }
                crate::log_error!("screenshot failed: {error}");
                self.notice("Screenshot failed", &error_sentence(&error), Balloon::Error);
            }
        }
    }

    fn commit_recording(&mut self, rect: RectPx) {
        let overlay = self.overlay.take();
        let (clamped, monitor) = Self::clamp_selection(overlay.as_ref(), rect);
        if let Some(overlay) = overlay {
            overlay.close();
        }
        if !matches!(self.recording, Recording::Idle) {
            crate::log_warn!("ignoring a recording gesture while another recording is active");
            return;
        }
        match RecordingTarget::from_selection(clamped.to_logical(monitor.scale), monitor.scale, Codec::H264) {
            RecordingTarget::Region(_) => {
                let placement =
                    Placement { anchor: center(clamped), corner: farthest_corner(monitor.work_area, clamped) };
                self.start_recording(Target::Region { rect: clamped, monitor }, placement);
            }
            RecordingTarget::TooSmall | RecordingTarget::AudioOnly => {
                crate::log_info!("selection {}x{} is too small to record", clamped.width(), clamped.height());
            }
        }
    }

    // --- recording --------------------------------------------------------

    /// A free file name in the recordings folder (created if missing).
    fn output_path(&self, extension: &str) -> Result<PathBuf, String> {
        let folder = self.settings.recordings_dir();
        std::fs::create_dir_all(&folder).map_err(|error| {
            sentence(&format!("Could not create the recordings folder {}: {error}", folder.display()))
        })?;
        let stamp = paths::local_time();
        let name = text::unique_name(
            |attempt| recording_file_name(stamp, extension, attempt),
            |name| folder.join(name).exists(),
        );
        Ok(folder.join(name))
    }

    fn start_recording(&mut self, target: Target, placement: Placement) {
        if !matches!(self.recording, Recording::Idle) {
            crate::log_warn!("a recording is already active");
            return;
        }
        let audio_only = matches!(target, Target::AudioOnly);
        // A new recording replaces the previous card and any failed pill.
        self.card = None;
        self.hud = None;
        self.recording = Recording::Starting;
        self.sync_recording_flags();

        let started = self.output_path(if audio_only { "m4a" } else { "mp4" }).and_then(|output| {
            crate::log_info!("starting recording {target:?} to {}", output.display());
            let options =
                RecorderOptions { include_system_audio: self.settings.include_system_audio, show_cursor: true, output };
            Recorder::start(target, options, self.ui())
        });
        match started {
            Ok(recorder) => {
                RECORDER.with(|slot| *slot.borrow_mut() = Some(recorder));
                self.recording = Recording::Running { audio_only, placement };
                if let Target::Region { rect, monitor } = target {
                    match Outline::show(rect, &monitor) {
                        Ok(outline) => self.outline = Some(outline),
                        Err(error) => crate::log_warn!("could not show the region outline: {error}"),
                    }
                }
                let generation = self.generation();
                let mode = HudMode::Recording { started: Instant::now() };
                match Hud::show(self.msg_window, generation, audio_only, placement, mode) {
                    Ok(hud) => self.hud = Some(hud),
                    Err(error) => crate::log_warn!("could not show the recording pill: {error}"),
                }
                if self.settings.play_sounds {
                    sound::play(Sound::RecordingStarted);
                }
            }
            Err(reason) => {
                crate::log_error!("recording could not start: {reason}");
                self.recording = Recording::Idle;
                self.show_failed_hud(audio_only, placement, &reason);
                self.notice("Could not start recording", &reason, Balloon::Error);
            }
        }
        self.sync_recording_flags();
    }

    fn show_failed_hud(&mut self, audio_only: bool, placement: Placement, reason: &str) {
        self.hud = None;
        let generation = self.generation();
        let mode = HudMode::Failed { reason: reason.to_owned() };
        match Hud::show(self.msg_window, generation, audio_only, placement, mode) {
            Ok(hud) => self.hud = Some(hud),
            Err(error) => crate::log_warn!("could not show the failed pill: {error}"),
        }
    }

    fn stop_recording(&mut self) {
        let Recording::Running { audio_only, placement } = self.recording else { return };
        crate::log_info!("stopping the recording");
        if let Some(hud) = &self.hud {
            hud.set_mode(HudMode::Saving);
        }
        self.recording = Recording::Saving { audio_only, placement };
        self.sync_recording_flags();
        match begin_save(Some(self.ui())) {
            SaveStart::Started => {}
            // The end of the session already started saving it: `Finished` follows.
            SaveStart::Nothing => {}
            SaveStart::Stopped(result) => self.finish_recording(result),
        }
    }

    fn on_recorder(&mut self, event: RecorderEvent) {
        match event {
            RecorderEvent::Interrupted(reason) => {
                crate::log_warn!("recording interrupted: {reason}");
                if matches!(self.recording, Recording::Running { .. }) {
                    self.notice("Recording stopped", &interrupted_notice(&reason), Balloon::Warning);
                    self.stop_recording();
                }
            }
            RecorderEvent::Finished(result) => self.finish_recording(result),
        }
    }

    fn finish_recording(&mut self, result: StopResult) {
        let (audio_only, placement) = match std::mem::replace(&mut self.recording, Recording::Idle) {
            Recording::Saving { audio_only, placement } => (audio_only, placement),
            // Saved from outside the controller (the session is ending).
            Recording::Running { audio_only, placement } if !recorder_running() => (audio_only, placement),
            other => {
                self.recording = other;
                crate::log_warn!("ignoring a recording result that nobody waited for");
                return;
            }
        };
        // The worker posted the result as its last step, so this returns at once.
        let _ = reap_saves();
        release_shutdown_block(self.main);
        self.sync_recording_flags();
        self.outline = None;
        match result {
            Ok(recording) => {
                crate::log_info!(
                    "saved {} ({} bytes, {:.1} s, audio: {})",
                    recording.path.display(),
                    recording.bytes,
                    recording.duration_secs,
                    recording.has_audio
                );
                self.hud = None;
                self.show_card(recording, placement);
                if self.settings.play_sounds {
                    sound::play(Sound::RecordingSaved);
                }
            }
            Err(reason) => {
                crate::log_error!("recording failed: {reason}");
                match &self.hud {
                    Some(hud) => hud.set_mode(HudMode::Failed { reason: reason.clone() }),
                    None => self.show_failed_hud(audio_only, placement, &reason),
                }
                self.notice("Recording failed", &reason, Balloon::Error);
            }
        }
    }

    fn show_card(&mut self, recording: RecordingResult, placement: Placement) {
        self.card = None;
        let info = CardInfo {
            path: recording.path,
            audio_only: recording.audio_only,
            bytes: recording.bytes,
            duration_secs: recording.duration_secs,
            thumbnail: recording.thumbnail,
        };
        let path = info.path.clone();
        let generation = self.generation();
        match SavedCard::show(self.msg_window, generation, info, placement) {
            Ok(card) => self.card = Some(card),
            Err(error) => {
                crate::log_warn!("could not show the saved card: {error}");
                self.notice("Recording saved", &path.display().to_string(), Balloon::Info);
            }
        }
    }

    fn on_ui(&mut self, action: UiAction, generation: u32) {
        let hud_matches = self.hud.as_ref().is_some_and(|hud| hud.generation() == generation);
        let card_path = self.card.as_ref().filter(|card| card.generation() == generation).map(SavedCard::path);
        match action {
            UiAction::HudClicked if hud_matches => match self.recording {
                Recording::Running { .. } => self.stop_recording(),
                Recording::Idle => self.hud = None,
                Recording::Starting | Recording::Saving { .. } => {}
            },
            UiAction::HudExpired if hud_matches && matches!(self.recording, Recording::Idle) => self.hud = None,
            UiAction::CardOpen | UiAction::CardReveal | UiAction::CardClose | UiAction::CardExpired => {
                let Some(path) = card_path else { return };
                self.card = None;
                let opened = match action {
                    UiAction::CardOpen => shell::open(&path),
                    UiAction::CardReveal => shell::reveal(&path),
                    _ => Ok(()),
                };
                if let Err(error) = opened {
                    crate::log_warn!("{error}");
                    self.notice("TedCat", &error, Balloon::Warning);
                }
            }
            _ => {}
        }
    }

    // --- hook -------------------------------------------------------------

    fn on_hook_status(&mut self, status: HookStatus) {
        crate::log_info!("input hook status: {status:?}");
        let changed = std::mem::discriminant(&status) != std::mem::discriminant(&self.hook_status);
        if !matches!(status, HookStatus::Active) {
            if let Some(overlay) = self.overlay.take() {
                overlay.close();
            }
        }
        // Unavailable and Degraded are told once until the hook recovers; the
        // emergency switch every time, since the user just pressed it.
        let tell = changed || matches!(status, HookStatus::KillSwitch);
        if let Some((title, text, kind)) = hook_status_notice(&status).filter(|_| tell) {
            self.notice(title, &text, kind);
        }
        self.hook_status = status;
    }

    fn restart_hook(&mut self) {
        if let Some(hook) = self.hook.take() {
            HOOK_REQUESTS.set(None);
            hook.stop();
        }
        match InputHook::start(self.ui(), Arc::clone(&self.gesture)) {
            Ok(hook) => {
                HOOK_REQUESTS.set(Some(hook.handle()));
                self.hook = Some(hook);
                self.hook_status = HookStatus::Active;
            }
            Err(error) => {
                crate::log_error!("input hook could not restart: {error}");
                // Make sure the user hears about it again: they asked for this.
                self.hook_status = HookStatus::Active;
                self.on_hook_status(HookStatus::Unavailable(sentence(&error.to_string())));
            }
        }
    }

    fn toggle_enabled(&mut self) {
        if self.gesture_active() {
            self.settings.enabled = false;
            self.gesture.enabled.store(false, Ordering::SeqCst);
        } else {
            self.settings.enabled = true;
            self.gesture.enabled.store(true, Ordering::SeqCst);
            match self.hook_status {
                HookStatus::Unavailable(_) => self.restart_hook(),
                HookStatus::Degraded(_) | HookStatus::KillSwitch => self.hook_status = HookStatus::Active,
                HookStatus::Active => {}
            }
        }
        crate::log_info!("gesture enabled: {}", self.gesture_active());
        self.save_settings();
    }

    // --- tray menu --------------------------------------------------------

    fn menu_state(&self) -> MenuState {
        MenuState {
            phase: self.phase(),
            hook_unavailable: matches!(self.hook_status, HookStatus::Unavailable(_)),
            enabled: self.gesture_active(),
            trigger: self.settings.trigger,
            hold_millis: self.settings.hold_millis,
            play_sounds: self.settings.play_sounds,
            launch_at_login: autostart::is_enabled(),
            include_system_audio: self.settings.include_system_audio,
        }
    }

    fn on_tray(&mut self, event: u32, anchor: PointPx) -> Option<Followup> {
        if !matches!(event, NIN_SELECT | NIN_KEYSELECT | WM_CONTEXTMENU) {
            return None;
        }
        if MODAL.get() || MENU_CLOSED_AT.get().is_some_and(|closed| closed.elapsed() < MENU_REOPEN_GUARD) {
            return None;
        }
        let at = if anchor == PointPx::default() { window::cursor_pos().unwrap_or_default() } else { anchor };
        Some(Followup::ShowMenu { owner: self.main, entries: build_menu(&self.menu_state()), at })
    }

    fn on_command(&mut self, command: Command) -> Option<Followup> {
        crate::log_info!("menu: {command:?}");
        match command {
            Command::RecordAudio => self.start_recording(Target::AudioOnly, Placement::primary_bottom_right()),
            Command::StopRecording => self.stop_recording(),
            Command::ToggleEnabled => self.toggle_enabled(),
            Command::SetTrigger(trigger) => {
                self.settings.trigger = trigger;
                let required = trigger.modifiers().without(tedcat_core::Modifiers::SHIFT);
                self.gesture.required_modifiers.store(required.0, Ordering::SeqCst);
                self.save_settings();
            }
            Command::SetHoldMillis(millis) => {
                self.settings.hold_millis = millis;
                self.gesture.hold_millis.store(millis, Ordering::SeqCst);
                self.save_settings();
            }
            Command::TogglePlaySounds => {
                self.settings.play_sounds = !self.settings.play_sounds;
                self.save_settings();
            }
            Command::ToggleLaunchAtLogin => {
                let enable = !autostart::is_enabled();
                if let Err(error) = autostart::set_enabled(enable) {
                    crate::log_error!("launch at login: {error}");
                    self.notice("Could not change Launch at Login", &error, Balloon::Warning);
                }
            }
            Command::ToggleSystemAudio => {
                self.settings.include_system_audio = !self.settings.include_system_audio;
                self.save_settings();
            }
            Command::OpenRecordingsFolder => {
                let folder = self.settings.recordings_dir();
                if let Err(error) = std::fs::create_dir_all(&folder) {
                    crate::log_warn!("could not create {}: {error}", folder.display());
                }
                if let Err(error) = shell::open(&folder) {
                    self.notice("TedCat", &error, Balloon::Warning);
                }
            }
            Command::ChangeRecordingsFolder => {
                return Some(Followup::PickFolder { initial: self.settings.recordings_dir() });
            }
            Command::OpenLog => {
                crate::log_info!("opening the log file");
                match crate::util::log::log_file() {
                    Some(path) => {
                        if let Err(error) = shell::open_text(&path) {
                            self.notice("TedCat", &error, Balloon::Warning);
                        }
                    }
                    None => self.notice("TedCat", "There is no log file (LOCALAPPDATA is not set).", Balloon::Warning),
                }
            }
            Command::Quit => return Some(Followup::Quit),
        }
        None
    }

    fn on_folder_picked(&mut self, folder: PathBuf) {
        crate::log_info!("recordings folder: {}", folder.display());
        self.settings.recordings_folder =
            if same_folder(&folder, &paths::default_recordings_dir()) { None } else { Some(folder) };
        self.save_settings();
    }

    // --- quitting ---------------------------------------------------------

    /// Finalizes a recording, stops the hook and removes every window and the
    /// tray icon. Used by Quit (after the message loop) and by `WM_ENDSESSION`.
    fn shutdown(mut self) {
        // First, stop holding presses: from here on nothing handles gestures, so
        // a press held during the wait below would be swallowed. A press that is
        // already pending is handed back when its hold timer fires. Not saved:
        // the gesture is on again at the next start.
        self.gesture.enabled.store(false, Ordering::SeqCst);
        crate::log_info!("TedCat is quitting; presses are no longer held");
        // SAFETY: our own timer on our own window.
        unsafe {
            let _ = KillTimer(Some(self.main), TRAY_RETRY_TIMER);
        }
        // A selection in progress will never be committed: close its overlay
        // and hand the held press back to the app, as when the overlay could
        // not be shown. The hook gets the request before the stop below.
        if let Some(overlay) = self.overlay.take() {
            let origin = overlay.origin();
            overlay.close();
            refuse_selection(origin);
        }
        if matches!(self.recording, Recording::Running { .. }) {
            if let Some(hud) = &self.hud {
                hud.set_mode(HudMode::Saving);
            }
        }
        self.recording = Recording::Idle;
        match begin_save(None) {
            SaveStart::Started => crate::log_info!("finalizing the recording before quitting"),
            SaveStart::Stopped(result) => log_final_result(Some(result)),
            SaveStart::Nothing if saves_pending() => crate::log_info!("waiting for the recording to be saved"),
            SaveStart::Nothing => {}
        }
        if saves_pending() && !wait_for_saves(QUIT_SAVE_TIMEOUT) {
            crate::log_error!("the recording did not finish in time");
        }
        for result in reap_saves() {
            log_final_result(result);
        }
        release_shutdown_block(self.main);
        HOOK_REQUESTS.set(None);
        if let Some(hook) = self.hook.take() {
            hook.stop();
        }
        self.hud = None;
        self.outline = None;
        self.card = None;
        self.tray.remove();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_class_uses_the_tedcat_prefix() {
        // SAFETY: MAIN_CLASS is a static, null-terminated string.
        let name = unsafe { MAIN_CLASS.to_string() }.unwrap_or_default();
        assert_eq!(name, "TedCat.Main");
        assert!(name.starts_with(crate::util::win::CLASS_PREFIX));
    }

    #[test]
    fn tray_anchor_is_signed() {
        assert_eq!(packed_point(WPARAM(0x0400_0780)), PointPx::new(1920, 1024));
        assert_eq!(packed_point(WPARAM(0x0010_FF00)), PointPx::new(-256, 16));
    }

    #[test]
    fn centers_do_not_overflow() {
        assert_eq!(center(RectPx::new(0, 0, 100, 50)), PointPx::new(50, 25));
        assert_eq!(center(RectPx::new(-200, -100, 0, 0)), PointPx::new(-100, -50));
        assert_eq!(center(RectPx::new(i32::MAX - 2, 0, i32::MAX, 2)), PointPx::new(i32::MAX - 1, 1));
    }

    #[test]
    fn folders_compare_like_windows() {
        assert!(same_folder(
            std::path::Path::new(r"C:\Users\Rob\Videos\TedCat"),
            std::path::Path::new(r"c:\users\rob\videos\tedcat\")
        ));
        assert!(!same_folder(std::path::Path::new(r"D:\Clips"), std::path::Path::new(r"C:\Clips")));
    }

    #[test]
    fn key_select_is_select_plus_key_flag() {
        assert_eq!(NIN_KEYSELECT, 0x401);
    }

    #[test]
    fn message_class_uses_the_tedcat_prefix() {
        // SAFETY: MSG_CLASS is a static, null-terminated string.
        let name = unsafe { MSG_CLASS.to_string() }.unwrap_or_default();
        assert!(name.starts_with(crate::util::win::CLASS_PREFIX));
        assert_ne!(name, "TedCat.Main");
    }

    #[test]
    fn pumping_ends_when_the_condition_holds_or_the_time_is_up() {
        let started = Instant::now();
        let mut checks = 0;
        assert!(pump_until(
            || {
                checks += 1;
                checks >= 3
            },
            Duration::from_secs(10)
        ));
        assert!(started.elapsed() < Duration::from_secs(5));

        let started = Instant::now();
        assert!(!pump_until(|| false, Duration::from_millis(120)));
        assert!(started.elapsed() >= Duration::from_millis(120));
    }

    #[test]
    fn pumping_keeps_a_quit_request_for_the_message_loop() {
        // SAFETY: posts WM_QUIT to this test's own thread.
        unsafe { PostQuitMessage(7) };
        assert!(!pump_until(|| false, Duration::from_millis(80)));
        let mut message = MSG::default();
        // SAFETY: reads this thread's own queue.
        let found = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool();
        assert!(found);
        assert_eq!(message.message, WM_QUIT);
        assert_eq!(message.wParam.0, 7);
    }

    /// A save whose worker finishes when `release` is dropped or sent to.
    fn save_job(result: Option<StopResult>) -> (SaveJob, std::sync::mpsc::Sender<()>) {
        let (release, released) = std::sync::mpsc::channel::<()>();
        let done = Arc::new(AtomicBool::new(false));
        let finished = SetOnDrop(Arc::clone(&done));
        let worker = std::thread::spawn(move || {
            let _ = released.recv();
            drop(finished);
            result
        });
        (SaveJob { done, worker }, release)
    }

    #[test]
    fn waiting_for_saves_lasts_until_the_file_is_final() {
        let (job, release) = save_job(Some(Err("disk full".to_owned())));
        SAVES.with(|saves| saves.borrow_mut().push(job));
        assert!(saves_pending());
        assert!(!wait_for_saves(Duration::from_millis(100)), "the save is still blocked");
        assert!(reap_saves().is_empty(), "an unfinished save is not reaped");

        drop(release);
        assert!(wait_for_saves(Duration::from_secs(10)));
        assert!(!saves_pending());
        assert_eq!(reap_saves(), vec![Some(Err("disk full".to_owned()))]);
        assert!(SAVES.with(|saves| saves.borrow().is_empty()));
    }

    #[test]
    fn saves_that_posted_their_result_reap_to_nothing() {
        let (job, release) = save_job(None);
        SAVES.with(|saves| saves.borrow_mut().push(job));
        release.send(()).expect("the worker is waiting");
        assert!(wait_for_saves(Duration::from_secs(10)));
        assert_eq!(reap_saves(), vec![None]);
    }

    #[test]
    fn nothing_to_save_without_a_recorder() {
        assert!(!recorder_running());
        assert!(matches!(begin_save(None), SaveStart::Nothing));
        assert!(!saves_pending());
        assert!(wait_for_saves(Duration::ZERO));
    }

    #[test]
    fn the_end_of_the_session_stops_holding_presses_while_the_controller_is_out_of_reach() {
        // No controller on this thread: the branch taken while a quit owns it
        // or a handler further up the stack borrows it.
        assert!(CONTROLLER.with(|slot| slot.borrow().is_none()));
        let gesture = GestureSettings::new(true, 350, tedcat_core::Modifiers::NONE);
        GESTURE.with(|slot| *slot.borrow_mut() = Some(Arc::clone(&gesture)));
        let settings_before = std::fs::metadata(Settings::path()).and_then(|m| m.modified()).ok();
        end_session(HWND::default());
        assert!(!gesture.enabled.load(Ordering::SeqCst), "presses must pass straight through while TedCat ends");
        let settings_after = std::fs::metadata(Settings::path()).and_then(|m| m.modified()).ok();
        assert_eq!(settings_before, settings_after, "the switch-off is never saved");
        let mut message = MSG::default();
        // SAFETY: reads this thread's own queue: end_session ends its message loop.
        let quit = unsafe { PeekMessageW(&mut message, None, WM_QUIT, WM_QUIT, PM_REMOVE) }.as_bool();
        assert!(quit && message.message == WM_QUIT);
        GESTURE.with(|slot| slot.borrow_mut().take());
    }

    #[test]
    fn notices_append_whole_sentences_to_the_reasons() {
        let reasons = [
            sentence("The audio output device was disconnected."),
            sentence("The display resolution changed during the recording"),
            sentence("Screen capture stopped: The device was removed. (0x887A0005)"),
        ];
        assert_eq!(
            interrupted_notice(&reasons[0]),
            "The audio output device was disconnected. TedCat saves what was captured."
        );
        let statuses = [
            HookStatus::Unavailable(sentence("Windows refused the mouse hook: Access is denied.")),
            HookStatus::Degraded(sentence(
                "Windows refused to replay a click (SendInput inserted 1 of 2 events, error 5)",
            )),
            HookStatus::KillSwitch,
        ];
        let mut texts: Vec<String> = reasons.iter().map(|reason| interrupted_notice(reason)).collect();
        for status in &statuses {
            let (title, text, _) = hook_status_notice(status).expect("every non-active status is told");
            assert!(!title.ends_with('.'), "{title}");
            texts.push(text);
        }
        assert_eq!(
            texts[4],
            "Windows refused to replay a click (SendInput inserted 1 of 2 events, error 5). TedCat let go of the \
             mouse to keep clicks working. To turn it back on, choose Enabled in the TedCat menu."
        );
        for text in &texts {
            assert!(text.ends_with('.') && !text.contains(".."), "{text}");
            assert!(!text.contains(".)") && !text.contains(". ."), "{text}");
            // A balloon holds 255 characters.
            assert!(text.chars().count() <= 255, "{text}");
        }
        assert_eq!(hook_status_notice(&HookStatus::Active), None);
    }

    #[test]
    fn windows_errors_read_as_sentences() {
        let described =
            windows::core::Error::new(windows::Win32::Foundation::E_FAIL, "The screenshot region is empty.");
        assert_eq!(error_sentence(&described), "The screenshot region is empty.");
        let bare = windows::core::Error::new(windows::core::HRESULT(0x8899_0001_u32 as i32), "");
        let text = error_sentence(&bare);
        assert!(text.ends_with('.') && !text.contains(".."), "{text}");
    }
}
