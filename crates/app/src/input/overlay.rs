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
//!
//! The label sits below the selection, above it if there is no room, inside as
//! a last resort, always within the monitor.
//!
//! Implementation notes: the dim and frame windows cover the monitor and only
//! change their window region on `update`, so an update costs two region
//! changes and a small label redraw whatever the resolution. The frame ring is
//! drawn just outside the selection, so it never covers selected pixels. The
//! label is a per-pixel-alpha layered window (`UpdateLayeredWindow`) with
//! rounded corners; a screenshot label appears once the selection has an area.

mod layout;

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

use tedcat_core::CaptureMode;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    GetLastError, COLORREF, ERROR_CLASS_ALREADY_EXISTS, HINSTANCE, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CombineRgn, CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateRectRgn, DeleteDC, DeleteObject,
    EndPaint, FillRect, GdiFlush, GetStockObject, GetTextExtentPoint32W, SelectObject, SetBkMode, SetDCBrushColor,
    SetTextColor, SetWindowRgn, TextOutW, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    BLENDFUNCTION, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DC_BRUSH, DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS,
    FF_SWISS, FW_SEMIBOLD, HBRUSH, HDC, HFONT, HGDIOBJ, HRGN, OUT_TT_PRECIS, PAINTSTRUCT, RGN_DIFF, RGN_ERROR,
    TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetWindowLongPtrW, KillTimer, RegisterClassExW,
    SetLayeredWindowAttributes, SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    UpdateLayeredWindow, GWLP_USERDATA, HTTRANSPARENT, HWND_TOPMOST, LWA_ALPHA, MA_NOACTIVATE, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SW_HIDE, ULW_ALPHA, WINDOW_EX_STYLE, WM_ERASEBKGND,
    WM_MOUSEACTIVATE, WM_NCHITTEST, WM_PAINT, WM_TIMER, WNDCLASSEXW, WNDPROC, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use self::layout::{
    LABEL_FONT_POINTS, LABEL_GAP_POINTS, LABEL_OPACITY, LABEL_PADDING_X_POINTS, LABEL_PADDING_Y_POINTS,
    LABEL_RADIUS_POINTS,
};
use crate::geom::{MonitorInfo, PointPx, RectPx};
use crate::util::wide;
use crate::util::win::exclude_from_capture;

const DIM_CLASS: &str = "TedCat.Overlay.Dim";
const FRAME_CLASS: &str = "TedCat.Overlay.Frame";
const LABEL_CLASS: &str = "TedCat.Overlay.Label";
const FLASH_CLASS: &str = "TedCat.Overlay.Flash";

/// Click-through, never activated, not in Alt+Tab, above normal windows.
const OVERLAY_EX_STYLE: WINDOW_EX_STYLE = WINDOW_EX_STYLE(
    WS_EX_LAYERED.0 | WS_EX_TRANSPARENT.0 | WS_EX_TOOLWINDOW.0 | WS_EX_TOPMOST.0 | WS_EX_NOACTIVATE.0,
);

/// 35 % black.
const DIM_ALPHA: u8 = 89;
const BLACK: COLORREF = rgb(0, 0, 0);
const WHITE: COLORREF = rgb(255, 255, 255);
const RECORDING_RED: COLORREF = rgb(255, 59, 48);
/// Label body colour as B, G, R bytes.
const LABEL_BACKGROUND_BGR: [u8; 3] = [32, 30, 30];
const LABEL_TEXT: COLORREF = rgb(255, 255, 255);
const LABEL_DOT: COLORREF = rgb(255, 69, 58);

const FLASH_TIMER_ID: usize = 1;
const FLASH_FRAME_MS: u32 = 15;

/// Registered once per process (window classes are process-wide).
static CLASSES_REGISTERED: AtomicBool = AtomicBool::new(false);

const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}

#[derive(Debug, Default)]
struct LabelState {
    text: Option<String>,
    size: (i32, i32),
    position: Option<PointPx>,
    shown: bool,
    failure_logged: bool,
}

pub struct SelectionOverlay {
    mode: CaptureMode,
    monitor: MonitorInfo,
    /// The press point, for the recording hint.
    origin: PointPx,
    // Handles are kept as integers so the overlay keeps the auto traits of
    // the original contract stub; it is still only used on the UI thread.
    dim: Handle,
    frame: Handle,
    label_window: Handle,
    font: Handle,
    /// The last clamped selection applied.
    selection: Option<RectPx>,
    /// The hole currently cut into the dim window.
    hole: Option<RectPx>,
    frame_shown: bool,
    label: LabelState,
    hidden: AtomicBool,
}

impl SelectionOverlay {
    /// Shows the overlay on the monitor containing `origin` (dimmed, no frame yet).
    pub fn show(origin: PointPx, mode: CaptureMode) -> windows::core::Result<SelectionOverlay> {
        let monitor = MonitorInfo::from_point(origin);
        let instance = module_instance()?;
        register_classes(instance)?;
        // Built first so that `Drop` destroys whatever was created if a later step fails.
        let mut overlay = SelectionOverlay {
            mode,
            monitor,
            origin,
            dim: Handle::default(),
            frame: Handle::default(),
            label_window: Handle::default(),
            font: Handle::default(),
            selection: None,
            hole: None,
            frame_shown: false,
            label: LabelState::default(),
            hidden: AtomicBool::new(false),
        };
        let area = monitor.rect;
        // Creation order is the z-order: dim, then frame, then label on top.
        overlay.dim = Handle::window(create_window(DIM_CLASS, instance, area)?);
        overlay.frame = Handle::window(create_window(FRAME_CLASS, instance, area)?);
        let corner = RectPx::new(area.left, area.top, area.left + 1, area.top + 1);
        overlay.label_window = Handle::window(create_window(LABEL_CLASS, instance, corner)?);
        let frame_color = match mode {
            CaptureMode::Screenshot => WHITE,
            CaptureMode::Recording => RECORDING_RED,
        };
        // SAFETY: the windows were just created on this thread.
        unsafe {
            SetWindowLongPtrW(overlay.dim.hwnd(), GWLP_USERDATA, BLACK.0 as isize);
            SetWindowLongPtrW(overlay.frame.hwnd(), GWLP_USERDATA, frame_color.0 as isize);
            SetLayeredWindowAttributes(overlay.dim.hwnd(), COLORREF(0), DIM_ALPHA, LWA_ALPHA)?;
            SetLayeredWindowAttributes(overlay.frame.hwnd(), COLORREF(0), 255, LWA_ALPHA)?;
        }
        overlay.font = Handle::font(create_label_font(monitor.scale));
        show_on_top(overlay.dim.hwnd());
        if mode == CaptureMode::Recording {
            let press = overlay.press_rect();
            overlay.refresh_label(press, press);
        }
        Ok(overlay)
    }

    /// Updates the selection; it is clamped to the overlay's monitor.
    pub fn update(&mut self, selection: RectPx) {
        let clamped = self.clamp(selection);
        if self.selection == Some(clamped) {
            return;
        }
        self.selection = Some(clamped);
        self.update_hole(clamped);
        self.update_frame(clamped);
        let anchor = if self.mode == CaptureMode::Recording && layout::is_undragged(clamped, self.monitor.scale) {
            self.press_rect()
        } else {
            clamped
        };
        self.refresh_label(clamped, anchor);
    }

    pub fn mode(&self) -> CaptureMode {
        self.mode
    }

    /// The monitor the overlay covers. Selections are clamped to it.
    pub fn monitor(&self) -> MonitorInfo {
        self.monitor
    }

    /// Clamps a rectangle to the overlay's monitor.
    ///
    /// The result is normalized; a rectangle that does not overlap the monitor
    /// comes back empty, and a zero-size one keeps its (clamped) position.
    pub fn clamp(&self, rect: RectPx) -> RectPx {
        layout::clamp_rect(rect, self.monitor.rect)
    }

    /// Hides every overlay window and waits for the compositor (`DwmFlush`), so a
    /// capture taken right after cannot contain the overlay even where window
    /// display affinity is unsupported.
    pub fn hide_for_capture(&self) {
        self.hidden.store(true, Ordering::Relaxed);
        for hwnd in [self.label_window.hwnd(), self.frame.hwnd(), self.dim.hwnd()] {
            if !hwnd.is_invalid() {
                // SAFETY: a window owned by this thread.
                unsafe {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
            }
        }
        // The first flush may return at the end of a frame that was already
        // being composed with the overlay; the second one is after the hide.
        // SAFETY: no preconditions; failure (composition off) is harmless.
        unsafe {
            let _ = DwmFlush();
            let _ = DwmFlush();
        }
    }

    /// Briefly flashes `rect` white (about 180 ms fade) and then destroys every window.
    pub fn flash_and_close(self, rect: RectPx) {
        let area = self.clamp(rect);
        if !area.is_empty() {
            if let Err(error) = show_flash(area) {
                crate::log_warn!("Could not show the screenshot flash: {}", error.message());
            }
        }
        drop(self);
    }

    /// Destroys every overlay window immediately.
    pub fn close(self) {
        drop(self);
    }

    fn monitor_origin(&self) -> PointPx {
        PointPx::new(self.monitor.rect.left, self.monitor.rect.top)
    }

    /// A zero-size rectangle at the press point, on the monitor.
    fn press_rect(&self) -> RectPx {
        self.clamp(RectPx::new(self.origin.x, self.origin.y, self.origin.x, self.origin.y))
    }

    fn update_hole(&mut self, clamped: RectPx) {
        let hole = (!clamped.is_empty()).then_some(clamped);
        if hole == self.hole {
            return;
        }
        let window = layout::relative(self.monitor.rect, self.monitor_origin());
        let region = match hole {
            Some(hole) => match region_difference(window, layout::relative(hole, self.monitor_origin())) {
                Some(region) => Some(region),
                // Keep the previous shape rather than dimming the selection.
                None => return,
            },
            None => None,
        };
        if set_window_region(self.dim.hwnd(), region) {
            self.hole = hole;
        }
    }

    fn update_frame(&mut self, clamped: RectPx) {
        let inner = layout::relative(clamped, self.monitor_origin());
        let outer = layout::inflate(inner, layout::ring_thickness(self.mode, self.monitor.scale));
        // Without its ring region the frame window would paint the whole
        // monitor, so it is only ever shown with a region in place.
        let shaped = !clamped.is_empty()
            && region_difference(outer, inner).is_some_and(|region| set_window_region(self.frame.hwnd(), Some(region)));
        if !shaped {
            if self.frame_shown {
                hide(self.frame.hwnd());
                self.frame_shown = false;
            }
            return;
        }
        if !self.frame_shown && !self.hidden.load(Ordering::Relaxed) {
            show_on_top(self.frame.hwnd());
            self.frame_shown = true;
            if self.label.shown {
                show_on_top(self.label_window.hwnd());
            }
        }
    }

    fn refresh_label(&mut self, selection: RectPx, anchor: RectPx) {
        let scale = self.monitor.scale;
        let Some(text) = layout::label_text(self.mode, selection, scale) else {
            self.hide_label();
            return;
        };
        let gap = layout::scaled(LABEL_GAP_POINTS, scale);
        let monitor = self.monitor.rect;
        if self.label.text.as_deref() != Some(text.as_str()) {
            let rendered = render_label(self.label_window.hwnd(), self.font.hfont(), &text, scale, |size| {
                layout::label_position(size, anchor, monitor, gap)
            });
            match rendered {
                Ok((size, position)) => {
                    self.label.text = Some(text);
                    self.label.size = size;
                    self.label.position = Some(position);
                }
                Err(error) => {
                    if !std::mem::replace(&mut self.label.failure_logged, true) {
                        crate::log_warn!("Could not draw the selection label: {}", error.message());
                    }
                    self.label.text = None;
                    self.hide_label();
                    return;
                }
            }
        } else {
            let position = layout::label_position(self.label.size, anchor, monitor, gap);
            if self.label.position != Some(position) {
                // SAFETY: a window owned by this thread.
                unsafe {
                    let _ = SetWindowPos(
                        self.label_window.hwnd(),
                        None,
                        position.x,
                        position.y,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
                self.label.position = Some(position);
            }
        }
        if !self.label.shown && !self.hidden.load(Ordering::Relaxed) {
            show_on_top(self.label_window.hwnd());
            self.label.shown = true;
        }
    }

    fn hide_label(&mut self) {
        if self.label.shown {
            hide(self.label_window.hwnd());
            self.label.shown = false;
        }
    }
}

impl Drop for SelectionOverlay {
    fn drop(&mut self) {
        for hwnd in [self.label_window.hwnd(), self.frame.hwnd(), self.dim.hwnd()] {
            if !hwnd.is_invalid() {
                // SAFETY: windows created by this overlay on this thread.
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
            }
        }
        let font = self.font.hfont();
        if !font.is_invalid() {
            // SAFETY: the font was created by this overlay and is not selected anywhere.
            unsafe {
                let _ = DeleteObject(font.into());
            }
        }
    }
}

/// A window or font handle stored as an integer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Handle(isize);

impl Handle {
    fn window(hwnd: HWND) -> Self {
        Self(hwnd.0 as isize)
    }

    fn font(font: HFONT) -> Self {
        Self(font.0 as isize)
    }

    fn hwnd(self) -> HWND {
        HWND(self.0 as *mut c_void)
    }

    fn hfont(self) -> HFONT {
        HFONT(self.0 as *mut c_void)
    }
}

fn module_instance() -> windows::core::Result<HINSTANCE> {
    // SAFETY: querying the handle of the running executable.
    unsafe { GetModuleHandleW(None) }.map(HINSTANCE::from)
}

fn register_classes(instance: HINSTANCE) -> windows::core::Result<()> {
    if CLASSES_REGISTERED.load(Ordering::Acquire) {
        return Ok(());
    }
    // No class brushes: the solid windows paint themselves in WM_PAINT and the
    // label is drawn with UpdateLayeredWindow.
    register_class(instance, DIM_CLASS, Some(solid_proc))?;
    register_class(instance, FRAME_CLASS, Some(solid_proc))?;
    register_class(instance, LABEL_CLASS, Some(passive_proc))?;
    register_class(instance, FLASH_CLASS, Some(flash_proc))?;
    CLASSES_REGISTERED.store(true, Ordering::Release);
    Ok(())
}

fn register_class(instance: HINSTANCE, name: &str, proc: WNDPROC) -> windows::core::Result<()> {
    let class_name = wide(name);
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: proc,
        hInstance: instance,
        lpszClassName: PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };
    // SAFETY: `class` and the name it points to are valid for the call; the
    // system copies the name.
    if unsafe { RegisterClassExW(&class) } == 0 {
        // SAFETY: no preconditions.
        let error = unsafe { GetLastError() };
        if error != ERROR_CLASS_ALREADY_EXISTS {
            return Err(error.into());
        }
    }
    Ok(())
}

/// A hidden overlay window covering `rect`, excluded from capture.
fn create_window(class: &str, instance: HINSTANCE, rect: RectPx) -> windows::core::Result<HWND> {
    let class_name = wide(class);
    // SAFETY: the class is registered and the name outlives the call.
    let hwnd = unsafe {
        CreateWindowExW(
            OVERLAY_EX_STYLE,
            PCWSTR(class_name.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            rect.left,
            rect.top,
            rect.width().max(1),
            rect.height().max(1),
            None,
            None,
            Some(instance),
            None,
        )
    }?;
    exclude_from_capture(hwnd);
    Ok(hwnd)
}

/// Shows `hwnd` without activating it, at the top of the topmost band.
fn show_on_top(hwnd: HWND) {
    // SAFETY: a window owned by this thread.
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    }
}

fn hide(hwnd: HWND) {
    // SAFETY: a window owned by this thread.
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
}

/// `outer` minus `inner` as a new region, or `None` if GDI failed.
fn region_difference(outer: RectPx, inner: RectPx) -> Option<HRGN> {
    // SAFETY: plain GDI region calls; every region is either returned or deleted.
    unsafe {
        let region = CreateRectRgn(outer.left, outer.top, outer.right, outer.bottom);
        if region.is_invalid() {
            return None;
        }
        let cut = CreateRectRgn(inner.left, inner.top, inner.right, inner.bottom);
        if cut.is_invalid() {
            let _ = DeleteObject(region.into());
            return None;
        }
        let combined = CombineRgn(Some(region), Some(region), Some(cut), RGN_DIFF);
        let _ = DeleteObject(cut.into());
        if combined == RGN_ERROR {
            let _ = DeleteObject(region.into());
            return None;
        }
        Some(region)
    }
}

/// Sets (or with `None` removes) a window region. Returns true on success.
fn set_window_region(hwnd: HWND, region: Option<HRGN>) -> bool {
    // SAFETY: on success the system owns the region; on failure it is still ours.
    unsafe {
        if SetWindowRgn(hwnd, region, true) != 0 {
            return true;
        }
        if let Some(region) = region {
            let _ = DeleteObject(region.into());
        }
        false
    }
}

fn create_label_font(scale: f64) -> HFONT {
    let face = wide("Segoe UI");
    // SAFETY: the face name outlives the call. A null font (failure) is handled
    // by the label code, which then draws with the DC's default font.
    unsafe {
        CreateFontW(
            -layout::scaled(LABEL_FONT_POINTS, scale),
            0,
            0,
            0,
            FW_SEMIBOLD.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_TT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            u32::from(DEFAULT_PITCH.0 | FF_SWISS.0),
            PCWSTR(face.as_ptr()),
        )
    }
}

/// A memory DC that restores what it selected and frees what it owns on drop.
struct MemoryDc {
    dc: HDC,
    restore: Vec<HGDIOBJ>,
    owned: Vec<HGDIOBJ>,
}

impl MemoryDc {
    fn new() -> windows::core::Result<Self> {
        // SAFETY: a DC compatible with the screen.
        let dc = unsafe { CreateCompatibleDC(None) };
        if dc.is_invalid() {
            return Err(windows::core::Error::from_thread());
        }
        Ok(Self { dc, restore: Vec::new(), owned: Vec::new() })
    }

    fn select(&mut self, object: HGDIOBJ) {
        // SAFETY: `object` is a live GDI object; the previous one is restored on drop.
        let previous = unsafe { SelectObject(self.dc, object) };
        if !previous.is_invalid() {
            self.restore.push(previous);
        }
    }
}

impl Drop for MemoryDc {
    fn drop(&mut self) {
        // SAFETY: restoring the original objects, then freeing ours once deselected.
        unsafe {
            for previous in self.restore.iter().rev() {
                SelectObject(self.dc, *previous);
            }
            let _ = DeleteDC(self.dc);
            for object in &self.owned {
                let _ = DeleteObject(*object);
            }
        }
    }
}

/// Draws `text` into the label window with `UpdateLayeredWindow` at the
/// position `place` picks for the label's size. Returns the size and position.
fn render_label(
    hwnd: HWND,
    font: HFONT,
    text: &str,
    scale: f64,
    place: impl FnOnce((i32, i32)) -> PointPx,
) -> windows::core::Result<((i32, i32), PointPx)> {
    let runs: Vec<(Vec<u16>, bool)> =
        layout::label_runs(text).into_iter().map(|(run, red)| (run.encode_utf16().collect(), red)).collect();
    let mut dc = MemoryDc::new()?;
    if !font.is_invalid() {
        dc.select(font.into());
    }

    let mut widths = Vec::with_capacity(runs.len());
    let mut text_height = 0;
    for (run, _) in &runs {
        let mut size = SIZE::default();
        // SAFETY: `size` is a valid out pointer.
        unsafe {
            let _ = GetTextExtentPoint32W(dc.dc, run, &mut size);
        }
        widths.push(size.cx);
        text_height = text_height.max(size.cy);
    }
    let pad_x = layout::scaled(LABEL_PADDING_X_POINTS, scale);
    let pad_y = layout::scaled(LABEL_PADDING_Y_POINTS, scale);
    let width = widths.iter().sum::<i32>() + 2 * pad_x;
    let height = text_height + 2 * pad_y;

    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    // SAFETY: `info` describes a 32-bit top-down DIB; `bits` receives its pixels.
    let bitmap = unsafe { CreateDIBSection(Some(dc.dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) }?;
    dc.owned.push(bitmap.into());
    if bits.is_null() {
        return Err(windows::core::Error::from_thread());
    }
    dc.select(bitmap.into());
    let len = width as usize * height as usize * 4;
    // SAFETY: the DIB section holds exactly width × height 32-bit pixels and
    // lives until `dc` is dropped at the end of this function.
    let pixels = unsafe { std::slice::from_raw_parts_mut(bits.cast::<u8>(), len) };
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel[..3].copy_from_slice(&LABEL_BACKGROUND_BGR);
        pixel[3] = 0;
    }

    // SAFETY: drawing into our own memory DC.
    unsafe {
        SetBkMode(dc.dc, TRANSPARENT);
        let mut x = pad_x;
        for ((run, red), run_width) in runs.iter().zip(&widths) {
            SetTextColor(dc.dc, if *red { LABEL_DOT } else { LABEL_TEXT });
            let _ = TextOutW(dc.dc, x, pad_y, run);
            x += run_width;
        }
        let _ = GdiFlush();
    }

    // GDI does not write alpha: shape the body (rounded corners) and premultiply.
    let radius = f64::from(layout::scaled(LABEL_RADIUS_POINTS, scale));
    for y in 0..height {
        for x in 0..width {
            let i = (y as usize * width as usize + x as usize) * 4;
            let coverage = layout::rounded_rect_coverage(x, y, width, height, radius);
            let shaded = layout::premultiply([pixels[i], pixels[i + 1], pixels[i + 2]], coverage, LABEL_OPACITY);
            pixels[i..i + 4].copy_from_slice(&shaded);
        }
    }

    let position = place((width, height));
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    let destination = POINT { x: position.x, y: position.y };
    let size = SIZE { cx: width, cy: height };
    let source = POINT { x: 0, y: 0 };
    // SAFETY: all pointers refer to locals that outlive the call; the DC holds
    // the premultiplied 32-bit bitmap.
    unsafe {
        UpdateLayeredWindow(
            hwnd,
            None,
            Some(&destination),
            Some(&size),
            Some(dc.dc),
            Some(&source),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        )
    }?;
    Ok(((width, height), position))
}

/// Shows a white window over `area` that fades out and destroys itself.
fn show_flash(area: RectPx) -> windows::core::Result<()> {
    let instance = module_instance()?;
    register_classes(instance)?;
    let hwnd = create_window(FLASH_CLASS, instance, area)?;
    // SAFETY: a window just created on this thread; it is destroyed on any failure.
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, GetTickCount64() as isize);
        if let Err(error) = SetLayeredWindowAttributes(hwnd, COLORREF(0), layout::FLASH_START_ALPHA, LWA_ALPHA) {
            let _ = DestroyWindow(hwnd);
            return Err(error);
        }
        if SetTimer(Some(hwnd), FLASH_TIMER_ID, FLASH_FRAME_MS, None) == 0 {
            let error = windows::core::Error::from_thread();
            let _ = DestroyWindow(hwnd);
            return Err(error);
        }
    }
    show_on_top(hwnd);
    Ok(())
}

/// Window procedure of the label window, and the base of the others: invisible
/// to the mouse and never activated.
unsafe extern "system" fn passive_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        // SAFETY: default handling with the unchanged arguments.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Fills the invalid part of `hwnd` (clipped to its window region) with `color`.
///
/// # Safety
/// Must be called from the window's own WM_PAINT handling.
unsafe fn paint_solid(hwnd: HWND, color: COLORREF) {
    let mut paint = PAINTSTRUCT::default();
    // SAFETY: BeginPaint/EndPaint pair on the window being painted.
    unsafe {
        let hdc = BeginPaint(hwnd, &mut paint);
        if !hdc.is_invalid() {
            SetDCBrushColor(hdc, color);
            FillRect(hdc, &paint.rcPaint, HBRUSH(GetStockObject(DC_BRUSH).0));
        }
        let _ = EndPaint(hwnd, &paint);
    }
}

/// The dim and frame windows: a solid colour (stored in `GWLP_USERDATA`),
/// painted in WM_PAINT so it never depends on the erase flag.
unsafe extern "system" fn solid_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            // SAFETY: our own window, inside its WM_PAINT.
            unsafe {
                let color = COLORREF(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as u32);
                paint_solid(hwnd, color);
            }
            LRESULT(0)
        }
        // SAFETY: forwarding the unchanged arguments.
        _ => unsafe { passive_proc(hwnd, msg, wparam, lparam) },
    }
}

/// The flash window fades on its timer (start tick in `GWLP_USERDATA`) and
/// destroys itself at the end.
unsafe extern "system" fn flash_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_ERASEBKGND {
        return LRESULT(1);
    }
    if msg == WM_PAINT {
        // SAFETY: our own window, inside its WM_PAINT.
        unsafe { paint_solid(hwnd, WHITE) };
        return LRESULT(0);
    }
    if msg == WM_TIMER && wparam.0 == FLASH_TIMER_ID {
        // SAFETY: our own window, on its own thread.
        unsafe {
            let start = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as u64;
            match layout::flash_alpha(GetTickCount64().saturating_sub(start)) {
                Some(alpha) => {
                    let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA);
                }
                None => {
                    let _ = KillTimer(Some(hwnd), FLASH_TIMER_ID);
                    let _ = DestroyWindow(hwnd);
                }
            }
        }
        return LRESULT(0);
    }
    // SAFETY: forwarding the unchanged arguments.
    unsafe { passive_proc(hwnd, msg, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::win::CLASS_PREFIX;

    #[test]
    fn every_class_is_a_tedcat_class() {
        for class in [DIM_CLASS, FRAME_CLASS, LABEL_CLASS, FLASH_CLASS] {
            assert!(class.starts_with(CLASS_PREFIX), "{class}");
        }
    }

    #[test]
    fn overlay_windows_are_click_through_and_never_activate() {
        for style in [WS_EX_LAYERED, WS_EX_TRANSPARENT, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_NOACTIVATE] {
            assert_eq!(OVERLAY_EX_STYLE.0 & style.0, style.0);
        }
    }

    #[test]
    fn overlay_keeps_the_auto_traits_of_the_contract_stub() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SelectionOverlay>();
    }

    #[test]
    fn colors_are_bgr_colorrefs() {
        assert_eq!(rgb(0x11, 0x22, 0x33).0, 0x0033_2211);
        assert_eq!(DIM_ALPHA, (0.35f64 * 255.0).round() as u8);
    }
}
