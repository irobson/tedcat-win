//! Win32 conventions every TedCat window follows.

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE};

/// Every TedCat window class name starts with this prefix. The input hook
/// uses it to recognise TedCat's own windows and never hold clicks on them.
pub const CLASS_PREFIX: &str = "TedCat.";

/// Keeps a window out of screenshots and recordings (Windows 10 2004+).
/// Every visible TedCat window must call this right after creation.
pub fn exclude_from_capture(hwnd: HWND) {
    unsafe {
        let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
    }
}
