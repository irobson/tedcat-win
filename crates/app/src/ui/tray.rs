//! The notification-area icon and its balloons.

use windows::core::w;
use windows::Win32::Foundation::{ERROR_SUCCESS, HWND};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::UI::HiDpi::GetSystemMetricsForDpi;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_ERROR, NIIF_INFO, NIIF_WARNING,
    NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NOTIFYICONDATAW, NOTIFYICONDATAW_0, NOTIFYICON_VERSION_4,
};
use windows::Win32::UI::WindowsAndMessaging::SM_CXSMICON;

use crate::geom::{MonitorInfo, PointPx};
use crate::messages::WM_APP_TRAY;
use crate::ui::icons::{icon_from_png, tray_png, Icon};
use crate::ui::text::fixed_wide;

/// The one icon TedCat shows (`uID`).
const TRAY_ID: u32 = 1;
const TOOLTIP: &str = "TedCat";

/// The kind of balloon, which picks the system icon shown in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Balloon {
    Info,
    Warning,
    Error,
}

/// Which artwork is current, so the icon is only rebuilt when it changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IconKey {
    dark_taskbar: bool,
    recording: bool,
    size: u32,
}

/// The tray icon of the main window.
pub(crate) struct Tray {
    hwnd: HWND,
    added: bool,
    recording: bool,
    key: Option<IconKey>,
    icon: Option<Icon>,
}

/// True when the taskbar uses the dark theme
/// (`HKCU\...\Themes\Personalize\SystemUsesLightTheme` is 0 or missing).
pub(crate) fn taskbar_is_dark() -> bool {
    let mut value: u32 = 1;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: reads a DWORD into a local of the right size.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    // Windows 10 before 1903 has no light taskbar and no such value.
    status != ERROR_SUCCESS || value == 0
}

/// The small-icon size for the primary monitor's DPI (where the notification area lives).
fn small_icon_size() -> u32 {
    let primary = MonitorInfo::from_point(PointPx::new(0, 0));
    let dpi = (primary.scale * 96.0).round() as u32;
    // SAFETY: a plain metrics query.
    let size = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, dpi) };
    if size > 0 {
        size as u32
    } else {
        ((16.0 * primary.scale).round() as u32).max(16)
    }
}

impl Tray {
    /// A tray icon for `hwnd`, not yet added (call [`Tray::ensure_added`]).
    pub(crate) fn new(hwnd: HWND) -> Tray {
        Tray { hwnd, added: false, recording: false, key: None, icon: None }
    }

    fn data(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: TRAY_ID,
            ..Default::default()
        }
    }

    /// Rebuilds the icon if the theme, DPI or recording state changed. Returns true if it changed.
    fn update_icon(&mut self) -> bool {
        let key = IconKey { dark_taskbar: taskbar_is_dark(), recording: self.recording, size: small_icon_size() };
        if self.key == Some(key) && self.icon.is_some() {
            return false;
        }
        match icon_from_png(tray_png(key.dark_taskbar, key.recording), key.size) {
            Ok(icon) => {
                self.icon = Some(icon);
                self.key = Some(key);
                true
            }
            Err(error) => {
                crate::log_error!("could not build the tray icon: {error}");
                false
            }
        }
    }

    /// Adds the icon if it is not in the notification area yet (also after
    /// Explorer restarted). Returns false if the shell refused; retry later.
    pub(crate) fn ensure_added(&mut self) -> bool {
        self.update_icon();
        let mut data = self.data();
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        data.uCallbackMessage = WM_APP_TRAY;
        data.hIcon = self.icon.as_ref().map(Icon::handle).unwrap_or_default();
        data.szTip = fixed_wide(TOOLTIP);
        // SAFETY: `data` is a fully initialized NOTIFYICONDATAW for our window.
        let notify = |message| unsafe { Shell_NotifyIconW(message, &data) }.as_bool();
        // An icon that is already there (for example after a NIM_ADD that timed
        // out but did succeed) refuses NIM_ADD and accepts NIM_MODIFY.
        let (first, second) = if self.added { (NIM_MODIFY, NIM_ADD) } else { (NIM_ADD, NIM_MODIFY) };
        let ok = notify(first) || notify(second);
        if !ok {
            crate::log_warn!("Shell_NotifyIcon could not add the tray icon (will retry)");
            self.added = false;
            return false;
        }
        data.Anonymous = NOTIFYICONDATAW_0 { uVersion: NOTIFYICON_VERSION_4 };
        // SAFETY: as above.
        if !unsafe { Shell_NotifyIconW(NIM_SETVERSION, &data) }.as_bool() {
            crate::log_warn!("NIM_SETVERSION 4 was refused");
        }
        self.added = true;
        true
    }

    /// Explorer restarted (`TaskbarCreated`): the old icon is gone, add it again.
    pub(crate) fn readd(&mut self) -> bool {
        self.added = false;
        self.ensure_added()
    }

    /// Re-reads the theme and DPI and swaps the icon if needed.
    pub(crate) fn refresh(&mut self) {
        if self.update_icon() && self.added {
            self.modify_icon();
        }
    }

    /// Switches between the idle and the recording artwork.
    pub(crate) fn set_recording(&mut self, recording: bool) {
        if self.recording != recording {
            self.recording = recording;
            self.refresh();
        }
    }

    fn modify_icon(&self) {
        let mut data = self.data();
        data.uFlags = NIF_ICON;
        data.hIcon = self.icon.as_ref().map(Icon::handle).unwrap_or_default();
        // SAFETY: fully initialized data for our icon.
        if !unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) }.as_bool() {
            crate::log_warn!("could not update the tray icon");
        }
    }

    /// Shows a balloon (a toast on Windows 10 and 11) from the tray icon.
    pub(crate) fn balloon(&self, title: &str, text: &str, kind: Balloon) {
        crate::log_info!("notice: {title}: {text}");
        if !self.added {
            return;
        }
        let mut data = self.data();
        data.uFlags = NIF_INFO;
        data.szInfoTitle = fixed_wide(title);
        data.szInfo = fixed_wide(text);
        data.dwInfoFlags = match kind {
            Balloon::Info => NIIF_INFO,
            Balloon::Warning => NIIF_WARNING,
            Balloon::Error => NIIF_ERROR,
        };
        // SAFETY: fully initialized data for our icon.
        if !unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) }.as_bool() {
            crate::log_warn!("could not show a balloon");
        }
    }

    /// Removes the icon from the notification area.
    pub(crate) fn remove(&mut self) {
        if self.added {
            let data = self.data();
            // SAFETY: identifies our icon by window and id.
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &data);
            }
            self.added = false;
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.remove();
    }
}
