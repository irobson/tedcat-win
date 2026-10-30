//! The red ring around a region while it is being recorded.
//!
//! Four thin click-through bars instead of one window with a hole: each bar
//! is tiny, so nothing large is allocated however big the region is. The ring
//! sits just outside the region (so it never appears in the video, though the
//! bars are also excluded from capture) and moves inside at monitor edges.

use std::sync::OnceLock;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateSolidBrush, HBRUSH};
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, DestroyWindow, SetLayeredWindowAttributes, HTTRANSPARENT, LWA_ALPHA, MA_NOACTIVATE,
    WM_MOUSEACTIVATE, WM_NCHITTEST, WS_EX_TRANSPARENT,
};

use crate::geom::{MonitorInfo, RectPx};
use crate::ui::layout::{px, ring_bars};
use crate::ui::raster::Color;
use crate::ui::window;

const CLASS: PCWSTR = w!("TedCat.Outline");

/// Recording red (the same as the overlay frame).
pub(crate) const RECORDING_RED: Color = Color::rgb(255, 59, 48);
/// Ring thickness in logical px.
const THICKNESS: f64 = 2.0;

/// Raw brush handle, kept as an integer so it can live in a static.
static BRUSH: OnceLock<isize> = OnceLock::new();

fn brush() -> HBRUSH {
    // SAFETY: CreateSolidBrush has no preconditions; the class keeps using the
    // brush for the life of the process, so it is never deleted.
    let raw = *BRUSH.get_or_init(|| unsafe { CreateSolidBrush(COLORREF(RECORDING_RED.colorref())) }.0 as isize);
    HBRUSH(raw as *mut _)
}

extern "system" fn outline_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        // SAFETY: default handling (the class brush paints the bar).
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

/// The ring; dropping it removes it.
pub(crate) struct Outline {
    bars: Vec<HWND>,
}

impl Outline {
    /// Shows the ring around `region` (physical pixels) on `monitor`.
    pub(crate) fn show(region: RectPx, monitor: &MonitorInfo) -> windows::core::Result<Outline> {
        window::register_class(CLASS, Some(outline_proc), Some(brush()))?;
        let thickness = px(THICKNESS, monitor.scale).max(2);
        let mut outline = Outline { bars: Vec::with_capacity(4) };
        for bar in ring_bars(region, thickness, monitor.rect) {
            if bar.is_empty() {
                continue;
            }
            let hwnd = window::create_floating(CLASS, WS_EX_TRANSPARENT, bar)?;
            outline.bars.push(hwnd);
            // SAFETY: a layered window needs its attributes set once to become visible.
            unsafe { SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA) }?;
            window::show_no_activate(hwnd);
        }
        Ok(outline)
    }
}

impl Drop for Outline {
    fn drop(&mut self) {
        for hwnd in self.bars.drain(..) {
            // SAFETY: destroying windows we created, on the UI thread.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_uses_the_tedcat_prefix() {
        // SAFETY: CLASS is a static, null-terminated string.
        let name = unsafe { CLASS.to_string() }.unwrap_or_default();
        assert!(name.starts_with(crate::util::win::CLASS_PREFIX), "{name}");
    }
}
