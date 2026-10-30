//! Window plumbing shared by the pill, the card and the outline: class
//! registration, creation with TedCat's styles, per-window state and small
//! mouse helpers.

use std::cell::RefCell;
use std::rc::Rc;

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{GetLastError, ERROR_CLASS_ALREADY_EXISTS, HINSTANCE, HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    TOOLTIPS_CLASSW, TTF_IDISHWND, TTF_SUBCLASS, TTM_ADDTOOLW, TTM_SETMAXTIPWIDTH, TTS_ALWAYSTIP, TTS_NOPREFIX,
    TTTOOLINFOW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, GetCursorPos, GetWindowLongPtrW, GetWindowRect, LoadCursorW, RegisterClassExW,
    SendMessageW, SetCursor, SetWindowLongPtrW, ShowWindow, CW_USEDEFAULT, GWLP_USERDATA, IDC_ARROW, IDC_HAND,
    SW_SHOWNOACTIVATE, WINDOW_EX_STYLE, WINDOW_STYLE, WNDCLASSEXW, WNDPROC, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::geom::{PointPx, RectPx};

/// Extended styles of TedCat's floating windows: layered (per-pixel alpha),
/// never activated, not in Alt+Tab or the taskbar, always on top.
pub(crate) const FLOATING_EX_STYLE: WINDOW_EX_STYLE =
    WINDOW_EX_STYLE(WS_EX_LAYERED.0 | WS_EX_TOOLWINDOW.0 | WS_EX_TOPMOST.0 | WS_EX_NOACTIVATE.0);

/// This module's instance handle.
pub(crate) fn instance() -> HINSTANCE {
    // SAFETY: GetModuleHandleW(None) returns the handle of the running executable.
    unsafe { GetModuleHandleW(None) }.map(HINSTANCE::from).unwrap_or_default()
}

/// Registers a window class (once; registering again is not an error).
pub(crate) fn register_class(class: PCWSTR, proc: WNDPROC, background: Option<HBRUSH>) -> windows::core::Result<()> {
    // SAFETY: the class name is a static string, the procedure a valid extern "system" fn.
    unsafe {
        let class_info = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: proc,
            hInstance: instance(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: background.unwrap_or_default(),
            lpszClassName: class,
            ..Default::default()
        };
        if RegisterClassExW(&class_info) == 0 {
            let error = GetLastError();
            if error != ERROR_CLASS_ALREADY_EXISTS {
                return Err(windows::core::Error::from_hresult(error.to_hresult()));
            }
        }
    }
    Ok(())
}

/// Creates a hidden floating window of `class` covering `rect` and excludes it
/// from capture. Show it with [`show_no_activate`] once it has content.
pub(crate) fn create_floating(
    class: PCWSTR,
    extra_style: WINDOW_EX_STYLE,
    rect: RectPx,
) -> windows::core::Result<HWND> {
    // SAFETY: standard window creation; the class was registered by the caller.
    let hwnd = unsafe {
        CreateWindowExW(
            FLOATING_EX_STYLE | extra_style,
            class,
            w!("TedCat"),
            WS_POPUP,
            rect.left,
            rect.top,
            rect.width().max(1),
            rect.height().max(1),
            None,
            None,
            Some(instance()),
            None,
        )
    }?;
    crate::util::win::exclude_from_capture(hwnd);
    Ok(hwnd)
}

/// Shows a window without activating it.
pub(crate) fn show_no_activate(hwnd: HWND) {
    // SAFETY: plain ShowWindow on a window we own.
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
}

/// Stores `state` in the window (one strong reference, released by [`detach`]).
pub(crate) fn attach<T: 'static>(hwnd: HWND, state: &Rc<RefCell<T>>) {
    let raw = Rc::into_raw(Rc::clone(state));
    // SAFETY: GWLP_USERDATA is free for the window's owner; `detach` reclaims the reference.
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);
    }
}

/// The state stored by [`attach`], if any.
///
/// Each window class must always use the same `T` for its windows.
pub(crate) fn state<T: 'static>(hwnd: HWND) -> Option<Rc<RefCell<T>>> {
    // SAFETY: the pointer was produced by `Rc::into_raw::<RefCell<T>>` in `attach`
    // and stays valid until `detach`, which clears it first.
    unsafe {
        let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<T>;
        if raw.is_null() {
            return None;
        }
        Rc::increment_strong_count(raw);
        Some(Rc::from_raw(raw))
    }
}

/// Releases the state stored by [`attach`]. Call from `WM_NCDESTROY`.
pub(crate) fn detach<T: 'static>(hwnd: HWND) {
    // SAFETY: see `state`; the slot is cleared before the reference is released.
    unsafe {
        let raw = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) as *const RefCell<T>;
        if !raw.is_null() {
            drop(Rc::from_raw(raw));
        }
    }
}

/// Client coordinates from a mouse message's `LPARAM` (signed, like `GET_X_LPARAM`).
pub(crate) fn lparam_point(lparam: LPARAM) -> (i32, i32) {
    let x = (lparam.0 & 0xFFFF) as u16 as i16 as i32;
    let y = ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
    (x, y)
}

/// The pointer position in screen coordinates.
pub(crate) fn cursor_pos() -> Option<PointPx> {
    let mut point = POINT::default();
    // SAFETY: writes into a local POINT.
    unsafe { GetCursorPos(&mut point) }.ok().map(|_| PointPx::new(point.x, point.y))
}

/// The window's rectangle in screen coordinates.
pub(crate) fn window_rect(hwnd: HWND) -> Option<RectPx> {
    let mut rect = RECT::default();
    // SAFETY: writes into a local RECT.
    unsafe { GetWindowRect(hwnd, &mut rect) }.ok().map(|_| RectPx::from_win(rect))
}

/// True when the pointer is over the window.
pub(crate) fn cursor_inside(hwnd: HWND) -> bool {
    match (cursor_pos(), window_rect(hwnd)) {
        (Some(point), Some(rect)) => rect.contains(point),
        _ => false,
    }
}

/// Asks for one `WM_MOUSELEAVE` when the pointer leaves the window.
pub(crate) fn track_leave(hwnd: HWND) -> bool {
    let mut track = TRACKMOUSEEVENT {
        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
        dwFlags: TME_LEAVE,
        hwndTrack: hwnd,
        dwHoverTime: 0,
    };
    // SAFETY: the struct is fully initialized and outlives the call.
    unsafe { TrackMouseEvent(&mut track) }.is_ok()
}

/// Shows the hand cursor (for `WM_SETCURSOR`).
pub(crate) fn set_hand_cursor() {
    // SAFETY: loading a system cursor and setting it is always valid on the UI thread.
    unsafe {
        if let Ok(cursor) = LoadCursorW(None, IDC_HAND) {
            SetCursor(Some(cursor));
        }
    }
}

/// A standard tooltip showing `text` while the pointer rests on a window.
pub(crate) struct Tooltip {
    hwnd: HWND,
}

impl Tooltip {
    /// Attaches a tooltip to `owner` (the whole window is the tool).
    pub(crate) fn new(owner: HWND, text: &str) -> Option<Tooltip> {
        let style = WINDOW_STYLE(WS_POPUP.0 | TTS_ALWAYSTIP | TTS_NOPREFIX);
        // SAFETY: creates a common-controls tooltip owned by `owner` (comctl32 v6 is in
        // the manifest and initialized in app::run).
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(WS_EX_TOPMOST.0 | WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0),
                TOOLTIPS_CLASSW,
                PCWSTR::null(),
                style,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                Some(owner),
                None,
                Some(instance()),
                None,
            )
        }
        .map_err(|error| crate::log_warn!("could not create a tooltip: {error}"))
        .ok()?;
        crate::util::win::exclude_from_capture(hwnd);
        let mut wide_text = crate::util::wide(text);
        let info = TTTOOLINFOW {
            cbSize: std::mem::size_of::<TTTOOLINFOW>() as u32,
            uFlags: TTF_IDISHWND | TTF_SUBCLASS,
            hwnd: owner,
            uId: owner.0 as usize,
            lpszText: PWSTR(wide_text.as_mut_ptr()),
            ..Default::default()
        };
        // SAFETY: TTM_ADDTOOLW copies the tool info and its text before returning.
        unsafe {
            SendMessageW(hwnd, TTM_SETMAXTIPWIDTH, None, Some(LPARAM(420)));
            SendMessageW(hwnd, TTM_ADDTOOLW, None, Some(LPARAM(&info as *const TTTOOLINFOW as isize)));
        }
        Some(Tooltip { hwnd })
    }
}

impl Drop for Tooltip {
    fn drop(&mut self) {
        // SAFETY: destroys the tooltip window we created.
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_coordinates_are_signed() {
        assert_eq!(lparam_point(LPARAM(0x0020_0010)), (16, 32));
        assert_eq!(lparam_point(LPARAM(0xFFFE_FFFF)), (-1, -2));
    }
}
