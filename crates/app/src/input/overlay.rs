//! The selection overlay: dimmed screen, selection frame and size label.
//!
//! CONTRACT (owned by the input agent; keep the public API unchanged):
//! UI thread only. Windows use class names starting with `util::win::CLASS_PREFIX`,
//! the extended styles `WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW |
//! WS_EX_TOPMOST | WS_EX_NOACTIVATE` (click-through, never activates, not in
//! Alt+Tab), and `util::win::exclude_from_capture`.
//! Suggested implementation, cheap at any resolution: one black window over the
//! monitor at 35 % alpha whose window region is the monitor minus the selection;
//! one opaque frame window whose region is a 1-2 px ring around the selection
//! (white for screenshots, red and 2 px for recordings); one small label window.
//!
//! Label text (sizes in logical units, i.e. pixels / monitor scale):
//! - Screenshot: `"640 × 400"`.
//! - Recording, before the pointer moved `RecordingTarget::DRAG_THRESHOLD` on
//!   either axis: `"● REC  ·  drag to select an area"` at the press point.
//! - Recording, `RecordingTarget::from_selection(.., Codec::H264)` is TooSmall:
//!   `"640 × 4  ·  too small to film"`; Region: `"● REC  640 × 400"`.
//! The label sits below the selection, above it if there is no room, inside as
//! a last resort, always within the monitor.

use tedcat_core::CaptureMode;

use crate::geom::{MonitorInfo, PointPx, RectPx};

pub struct SelectionOverlay {
    _private: (),
}

impl SelectionOverlay {
    /// Shows the overlay on the monitor containing `origin` (dimmed, no frame yet).
    pub fn show(origin: PointPx, mode: CaptureMode) -> windows::core::Result<SelectionOverlay> {
        let _ = (origin, mode);
        todo!("input agent")
    }

    /// Updates the selection; it is clamped to the overlay's monitor.
    pub fn update(&mut self, selection: RectPx) {
        let _ = selection;
        todo!("input agent")
    }

    pub fn mode(&self) -> CaptureMode {
        todo!("input agent")
    }

    /// The monitor the overlay covers. Selections are clamped to it.
    pub fn monitor(&self) -> MonitorInfo {
        todo!("input agent")
    }

    /// Clamps a rectangle to the overlay's monitor.
    pub fn clamp(&self, rect: RectPx) -> RectPx {
        let _ = rect;
        todo!("input agent")
    }

    /// Hides every overlay window and waits for the compositor (`DwmFlush`), so a
    /// capture taken right after cannot contain the overlay even where window
    /// display affinity is unsupported.
    pub fn hide_for_capture(&self) {
        todo!("input agent")
    }

    /// Briefly flashes `rect` white (about 180 ms fade) and then destroys every window.
    pub fn flash_and_close(self, rect: RectPx) {
        let _ = rect;
        todo!("input agent")
    }

    /// Destroys every overlay window immediately.
    pub fn close(self) {
        todo!("input agent")
    }
}
