//! Region screenshot to the clipboard.
//!
//! CONTRACT (owned by the input agent; keep the public API unchanged):
//! Captures `rect` (physical pixels, virtual-screen coordinates) from the screen
//! DC with `BitBlt` (`SRCCOPY | CAPTUREBLT`) into a 32-bit top-down DIB section,
//! then replaces the clipboard contents with both `CF_DIBV5` (alpha forced to
//! 255) and the registered `"PNG"` format (encoded with WIC), so it pastes in
//! classic apps, browsers and chat apps alike. Nothing is written to disk.
//! The caller hides the overlay first (`SelectionOverlay::hide_for_capture`).

use windows::Win32::Foundation::HWND;

use crate::geom::RectPx;

/// Copies `rect` to the clipboard. `owner` becomes the clipboard owner window.
pub fn copy_region_to_clipboard(rect: RectPx, owner: HWND) -> windows::core::Result<()> {
    let _ = (rect, owner);
    todo!("input agent")
}
