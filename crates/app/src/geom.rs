//! Physical-pixel geometry and monitor lookup.
//!
//! TedCat is per-monitor DPI aware (v2), so every coordinate the hook, the
//! windows and the capture APIs exchange is in physical pixels of the virtual
//! screen. The core crate works in logical units; convert with the monitor's
//! `scale` (DPI / 96) at the boundary.

use tedcat_core::{Point, Rect};
use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromPoint, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PointPx {
    pub x: i32,
    pub y: i32,
}

impl PointPx {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
    pub fn to_core(self) -> Point {
        Point::new(self.x as f64, self.y as f64)
    }
}

/// A rectangle in physical pixels: `left`/`top` inclusive, `right`/`bottom` exclusive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RectPx {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl RectPx {
    pub const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self { left, top, right, bottom }
    }
    pub fn width(&self) -> i32 {
        self.right - self.left
    }
    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }
    pub fn is_empty(&self) -> bool {
        self.width() <= 0 || self.height() <= 0
    }
    pub fn contains(&self, p: PointPx) -> bool {
        p.x >= self.left && p.x < self.right && p.y >= self.top && p.y < self.bottom
    }
    pub fn intersect(&self, other: &RectPx) -> RectPx {
        let r = RectPx::new(
            self.left.max(other.left),
            self.top.max(other.top),
            self.right.min(other.right),
            self.bottom.min(other.bottom),
        );
        if r.is_empty() {
            RectPx::default()
        } else {
            r
        }
    }
    /// Pixel rectangle covering a core rectangle given in physical pixels (rounded outward).
    pub fn from_core(rect: Rect) -> RectPx {
        RectPx::new(
            rect.min_x().floor() as i32,
            rect.min_y().floor() as i32,
            rect.max_x().ceil() as i32,
            rect.max_y().ceil() as i32,
        )
    }
    pub fn to_core(&self) -> Rect {
        Rect::new(self.left as f64, self.top as f64, self.width() as f64, self.height() as f64)
    }
    /// The same rectangle in logical units for the given monitor scale.
    pub fn to_logical(&self, scale: f64) -> Rect {
        self.to_core().scaled(1.0 / scale)
    }
    pub fn to_win(&self) -> RECT {
        RECT { left: self.left, top: self.top, right: self.right, bottom: self.bottom }
    }
    pub fn from_win(r: RECT) -> RectPx {
        RectPx::new(r.left, r.top, r.right, r.bottom)
    }
}

/// A display, in physical pixels of the virtual screen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MonitorInfo {
    /// Raw `HMONITOR` value (kept as an integer so the struct is `Send`).
    pub handle: isize,
    pub rect: RectPx,
    /// The monitor minus the taskbar and docked toolbars.
    pub work_area: RectPx,
    /// Effective DPI / 96: 1.0, 1.25, 1.5, 2.0, ...
    pub scale: f64,
}

impl MonitorInfo {
    pub fn hmonitor(&self) -> HMONITOR {
        HMONITOR(self.handle as *mut _)
    }

    /// The monitor containing (or nearest to) `point`.
    pub fn from_point(point: PointPx) -> MonitorInfo {
        unsafe {
            let monitor = MonitorFromPoint(POINT { x: point.x, y: point.y }, MONITOR_DEFAULTTONEAREST);
            Self::from_hmonitor(monitor)
        }
    }

    pub fn from_hmonitor(monitor: HMONITOR) -> MonitorInfo {
        unsafe {
            let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            let _ = GetMonitorInfoW(monitor, &mut info);
            let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
            let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
            MonitorInfo {
                handle: monitor.0 as isize,
                rect: RectPx::from_win(info.rcMonitor),
                work_area: RectPx::from_win(info.rcWork),
                scale: (dpi_x.max(96) as f64) / 96.0,
            }
        }
    }
}
