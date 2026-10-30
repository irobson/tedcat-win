//! The UI-thread controller.
//!
//! CONTRACT (owned by the UI agent): `run` initializes COM (apartment threaded)
//! and Media Foundation users may assume COM is initialized on the UI thread;
//! enforces a single instance (named mutex `Local\TedCat.SingleInstance`; a
//! second launch exits quietly); creates the hidden main window (class
//! `TedCat.Main`) whose window procedure handles `WM_APP_GESTURE`,
//! `WM_APP_HOOK_STATUS`, `WM_APP_RECORDER` and `WM_APP_TRAY`; starts the input
//! hook; and runs the message loop until Quit. It returns the process exit code.
//!
//! On quit while recording, it stops and finalizes the recording before exiting.
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
use std::sync::atomic::Ordering;
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
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Controls::{InitCommonControlsEx, ICC_WIN95_CLASSES, INITCOMMONCONTROLSEX};
use windows::Win32::UI::Shell::{NINF_KEY, NIN_SELECT};
use windows::Win32::UI::WindowsAndMessaging::{
    ChangeWindowMessageFilterEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    KillTimer, MsgWaitForMultipleObjects, PeekMessageW, PostQuitMessage, RegisterWindowMessageW, SetTimer,
    TranslateMessage, MSG, MSGFLT_ALLOW, PM_REMOVE, QS_ALLINPUT, WM_CLOSE, WM_CONTEXTMENU, WM_DISPLAYCHANGE,
    WM_DPICHANGED, WM_ENDSESSION, WM_QUERYENDSESSION, WM_QUIT, WM_SETTINGCHANGE, WM_TIMER, WS_EX_TOOLWINDOW, WS_POPUP,
};

use crate::capture::screenshot;
use crate::geom::{MonitorInfo, PointPx, RectPx};
use crate::input::{GestureSettings, InputHook, SelectionOverlay};
use crate::messages::{
    take_payload, GestureEvent, HookStatus, RecorderEvent, RecordingResult, UiHandle, WM_APP_GESTURE,
    WM_APP_HOOK_STATUS, WM_APP_RECORDER, WM_APP_TRAY,
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
const MUTEX_NAME: PCWSTR = w!("Local\\TedCat.SingleInstance");

/// Main-window timer that retries adding the tray icon.
const TRAY_RETRY_TIMER: usize = 1;
const TRAY_RETRY_MILLIS: u32 = 2000;
const TRAY_RETRY_LIMIT: u32 = 30;
/// A tray click this soon after the menu closed is the click that closed it.
const MENU_REOPEN_GUARD: Duration = Duration::from_millis(300);
/// How long quitting waits for a recording to be finalized.
const QUIT_SAVE_TIMEOUT: Duration = Duration::from_secs(30);
/// `NIN_KEYSELECT` (Enter or Space on the focused tray icon).
const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;

const STOPPED_TITLE: &str = "TedCat stopped intercepting clicks";

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
    // SAFETY: registering a well-known message name; allowing it through UIPI
    // matters only when TedCat runs elevated and Explorer does not.
    unsafe {
        let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
        TASKBAR_CREATED.set(taskbar_created);
        if taskbar_created != 0 {
            let _ = ChangeWindowMessageFilterEx(main, taskbar_created, MSGFLT_ALLOW, None);
        }
    }

    let controller = Controller::new(main);
    CONTROLLER.with(|slot| *slot.borrow_mut() = Some(controller));
    drain();

    let code = message_loop();

    let controller = CONTROLLER.with(|slot| slot.try_borrow_mut().ok().and_then(|mut slot| slot.take()));
    if let Some(controller) = controller {
        controller.shutdown();
    }
    QUEUE.with(|queue| queue.borrow_mut().clear());
    // SAFETY: destroying our own window after the loop ended.
    unsafe {
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
/// receives broadcasts such as `TaskbarCreated` and `WM_SETTINGCHANGE`.
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

/// Waits for `worker` while still dispatching window messages (so COM calls
/// that need this STA thread cannot deadlock), for at most `timeout`.
fn join_pumping<T>(worker: JoinHandle<T>, timeout: Duration) -> Option<T> {
    let deadline = Instant::now() + timeout;
    let mut quit_code = None;
    while !worker.is_finished() {
        if Instant::now() >= deadline {
            crate::log_warn!("gave up waiting for a worker thread");
            if let Some(code) = quit_code {
                // SAFETY: re-posts the WM_QUIT we consumed while pumping.
                unsafe { PostQuitMessage(code) };
            }
            return None;
        }
        // SAFETY: waits for input or 50 ms; then dispatches what arrived.
        unsafe {
            MsgWaitForMultipleObjects(None, false, 50, QS_ALLINPUT);
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
    }
    if let Some(code) = quit_code {
        // SAFETY: as above.
        unsafe { PostQuitMessage(code) };
    }
    worker.join().ok()
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
            Ok(mut slot) => slot.as_mut().and_then(|controller| controller.handle(message)),
            Err(_) => None,
        });
        if let Some(followup) = followup {
            run_followup(followup);
        }
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

extern "system" fn main_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let decoded = match message {
        // SAFETY (payloads): these messages are only posted with `UiHandle::post`
        // and the matching payload type; each is taken exactly once, here.
        WM_APP_GESTURE if lparam.0 != 0 => AppMsg::Gesture(*unsafe { take_payload::<GestureEvent>(lparam) }),
        WM_APP_HOOK_STATUS if lparam.0 != 0 => AppMsg::Hook(*unsafe { take_payload::<HookStatus>(lparam) }),
        WM_APP_RECORDER if lparam.0 != 0 => AppMsg::Recorder(*unsafe { take_payload::<RecorderEvent>(lparam) }),
        // NOTIFYICON_VERSION_4: LOWORD(lParam) is the event, wParam the anchor point.
        WM_APP_TRAY => AppMsg::Tray { event: (lparam.0 & 0xFFFF) as u32, anchor: packed_point(wparam) },
        WM_APP_UI => match UiAction::from_code(wparam.0) {
            Some(action) => AppMsg::Ui { action, generation: lparam.0 as u32 },
            None => return LRESULT(0),
        },
        WM_SETTINGCHANGE | WM_DPICHANGED | WM_DISPLAYCHANGE => AppMsg::Environment,
        WM_TIMER => AppMsg::Timer(wparam.0),
        WM_CLOSE => AppMsg::Close,
        WM_QUERYENDSESSION => return LRESULT(1),
        WM_ENDSESSION => {
            if wparam.0 != 0 {
                // The session is ending: Windows may end the process as soon as
                // this returns, so finalize a running recording right now.
                crate::log_info!("session ending");
                let controller = CONTROLLER.with(|slot| slot.try_borrow_mut().ok().and_then(|mut slot| slot.take()));
                if let Some(controller) = controller {
                    controller.shutdown();
                }
                // SAFETY: ends the message loop of this thread.
                unsafe { PostQuitMessage(0) };
            }
            return LRESULT(0);
        }
        _ if message != 0 && message == TASKBAR_CREATED.get() => AppMsg::TaskbarCreated,
        // SAFETY: default handling for everything else.
        _ => return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    };
    dispatch(decoded);
    LRESULT(0)
}

// ---------------------------------------------------------------------------
// The controller

type StopResult = Result<RecordingResult, String>;

/// Where the recording lifecycle is, with what each phase owns.
enum Recording {
    Idle,
    /// `Recorder::start` is running (synchronously, on this thread).
    Starting,
    Running {
        recorder: Recorder,
        audio_only: bool,
        placement: Placement,
    },
    /// A worker thread is finalizing the file; it posts `RecorderEvent::Finished`.
    Saving {
        worker: Option<JoinHandle<Option<StopResult>>>,
        audio_only: bool,
        placement: Placement,
    },
}

/// Runs `Recorder::stop` on a worker thread. With `notify`, the result is
/// posted to the UI thread as `RecorderEvent::Finished`; without, the thread
/// returns it. If no thread can be started, stops right here and returns the
/// result as the error value.
fn spawn_stop(recorder: Recorder, notify: Option<UiHandle>) -> Result<JoinHandle<Option<StopResult>>, StopResult> {
    let slot = Arc::new(Mutex::new(Some(recorder)));
    let worker_slot = Arc::clone(&slot);
    let spawned = std::thread::Builder::new().name("TedCat recorder stop".to_owned()).spawn(move || {
        let recorder = worker_slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take()?;
        let result = recorder.stop();
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
        Ok(worker) => Ok(worker),
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
    main: HWND,
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
    fn new(main: HWND) -> Controller {
        let first_run = !Settings::path().exists();
        let settings = Settings::load();
        crate::log_info!("settings: {settings:?}");
        let gesture = GestureSettings::new(settings.enabled, settings.hold_millis, settings.trigger.modifiers());
        let mut controller = Controller {
            main,
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
        match InputHook::start(UiHandle::new(main), Arc::clone(&controller.gesture)) {
            Ok(hook) => controller.hook = Some(hook),
            Err(error) => {
                crate::log_error!("input hook could not start: {error}");
                controller.on_hook_status(HookStatus::Unavailable(error.to_string()));
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
            Recording::Running { .. } => Phase::Recording,
            Recording::Saving { .. } => Phase::Saving,
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
                    Err(error) => crate::log_error!("could not show the selection overlay: {error}"),
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
                self.notice("Screenshot failed", &error.message(), Balloon::Error);
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
        std::fs::create_dir_all(&folder)
            .map_err(|error| format!("Could not create the recordings folder {}: {error}", folder.display()))?;
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
            Recorder::start(target, options, UiHandle::new(self.main))
        });
        match started {
            Ok(recorder) => {
                self.recording = Recording::Running { recorder, audio_only, placement };
                if let Target::Region { rect, monitor } = target {
                    match Outline::show(rect, &monitor) {
                        Ok(outline) => self.outline = Some(outline),
                        Err(error) => crate::log_warn!("could not show the region outline: {error}"),
                    }
                }
                let generation = self.generation();
                let mode = HudMode::Recording { started: Instant::now() };
                match Hud::show(self.main, generation, audio_only, placement, mode) {
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
        match Hud::show(self.main, generation, audio_only, placement, mode) {
            Ok(hud) => self.hud = Some(hud),
            Err(error) => crate::log_warn!("could not show the failed pill: {error}"),
        }
    }

    fn stop_recording(&mut self) {
        let (recorder, audio_only, placement) = match std::mem::replace(&mut self.recording, Recording::Idle) {
            Recording::Running { recorder, audio_only, placement } => (recorder, audio_only, placement),
            other => {
                self.recording = other;
                return;
            }
        };
        crate::log_info!("stopping the recording");
        if let Some(hud) = &self.hud {
            hud.set_mode(HudMode::Saving);
        }
        match spawn_stop(recorder, Some(UiHandle::new(self.main))) {
            Ok(worker) => {
                self.recording = Recording::Saving { worker: Some(worker), audio_only, placement };
                self.sync_recording_flags();
            }
            Err(result) => {
                self.recording = Recording::Saving { worker: None, audio_only, placement };
                self.finish_recording(result);
            }
        }
    }

    fn on_recorder(&mut self, event: RecorderEvent) {
        match event {
            RecorderEvent::Interrupted(reason) => {
                crate::log_warn!("recording interrupted: {reason}");
                if matches!(self.recording, Recording::Running { .. }) {
                    self.notice(
                        "Recording stopped",
                        &format!("{reason}. TedCat saves what was captured."),
                        Balloon::Warning,
                    );
                    self.stop_recording();
                }
            }
            RecorderEvent::Finished(result) => self.finish_recording(result),
        }
    }

    fn finish_recording(&mut self, result: StopResult) {
        let (audio_only, placement) = match std::mem::replace(&mut self.recording, Recording::Idle) {
            Recording::Saving { worker, audio_only, placement } => {
                if let Some(worker) = worker {
                    // It posted the result as its last step, so this returns at once.
                    let _ = worker.join();
                }
                (audio_only, placement)
            }
            other => {
                self.recording = other;
                crate::log_warn!("ignoring a recording result that nobody waited for");
                return;
            }
        };
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
        match SavedCard::show(self.main, generation, info, placement) {
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
        match &status {
            HookStatus::Active => {}
            HookStatus::Unavailable(reason) if changed => self.notice(
                STOPPED_TITLE,
                &format!(
                    "Windows did not let TedCat watch the mouse ({reason}), so clicks are left alone. \
                     To try again, choose Enabled in the TedCat menu."
                ),
                Balloon::Warning,
            ),
            HookStatus::Degraded(reason) if changed => self.notice(
                STOPPED_TITLE,
                &format!(
                    "Replaying a click failed ({reason}), so TedCat let go of the mouse to keep clicks working. \
                     To turn it back on, choose Enabled in the TedCat menu."
                ),
                Balloon::Warning,
            ),
            HookStatus::KillSwitch => self.notice(
                "TedCat is paused",
                "Escape was pressed 3 times, so TedCat stopped intercepting clicks. \
                 To turn it back on, choose Enabled in the TedCat menu.",
                Balloon::Info,
            ),
            _ => {}
        }
        self.hook_status = status;
    }

    fn restart_hook(&mut self) {
        if let Some(hook) = self.hook.take() {
            hook.stop();
        }
        match InputHook::start(UiHandle::new(self.main), Arc::clone(&self.gesture)) {
            Ok(hook) => {
                self.hook = Some(hook);
                self.hook_status = HookStatus::Active;
            }
            Err(error) => {
                crate::log_error!("input hook could not restart: {error}");
                // Make sure the user hears about it again: they asked for this.
                self.hook_status = HookStatus::Active;
                self.on_hook_status(HookStatus::Unavailable(error.to_string()));
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

    /// Finalizes a recording, stops the hook and removes every window and the tray icon.
    fn shutdown(mut self) {
        crate::log_info!("TedCat is quitting");
        // SAFETY: our own timer on our own window.
        unsafe {
            let _ = KillTimer(Some(self.main), TRAY_RETRY_TIMER);
        }
        if let Some(overlay) = self.overlay.take() {
            overlay.close();
        }
        match std::mem::replace(&mut self.recording, Recording::Idle) {
            Recording::Running { recorder, .. } => {
                crate::log_info!("finalizing the recording before quitting");
                if let Some(hud) = &self.hud {
                    hud.set_mode(HudMode::Saving);
                }
                let result = match spawn_stop(recorder, None) {
                    Ok(worker) => join_pumping(worker, QUIT_SAVE_TIMEOUT).flatten(),
                    Err(result) => Some(result),
                };
                match result {
                    Some(Ok(recording)) => crate::log_info!("saved {} before quitting", recording.path.display()),
                    Some(Err(reason)) => crate::log_error!("recording failed while quitting: {reason}"),
                    None => crate::log_error!("the recording did not finish in time"),
                }
            }
            Recording::Saving { worker: Some(worker), .. } => {
                crate::log_info!("waiting for the recording to be saved");
                if join_pumping(worker, QUIT_SAVE_TIMEOUT).is_none() {
                    crate::log_error!("the recording did not finish in time");
                }
            }
            Recording::Saving { worker: None, .. } | Recording::Starting | Recording::Idle => {}
        }
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
}
