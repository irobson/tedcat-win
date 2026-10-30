//! An off-screen 32-bit canvas for layered windows: shapes through
//! [`raster::Pixmap`], text through GDI, shown with `UpdateLayeredWindow`.

use std::cell::OnceCell;

use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontIndirectW, DeleteDC, DeleteObject, DrawTextW, GdiFlush,
    GetStockObject, GetTextExtentPoint32W, SelectObject, SetBkMode, SetTextColor, AC_SRC_ALPHA, AC_SRC_OVER,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, CLEARTYPE_QUALITY, DEFAULT_CHARSET, DEFAULT_GUI_FONT, DIB_RGB_COLORS,
    DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, HBITMAP, HDC, HFONT, HGDIOBJ, LOGFONTW,
    TRANSPARENT,
};
use windows::Win32::UI::HiDpi::SystemParametersInfoForDpi;
use windows::Win32::UI::WindowsAndMessaging::{
    UpdateLayeredWindow, NONCLIENTMETRICSW, SPI_GETNONCLIENTMETRICS, ULW_ALPHA,
};

use crate::geom::{PointPx, RectPx};
use crate::ui::raster::{Color, Pixmap};

/// Font weights used by the pill and the card.
pub(crate) const WEIGHT_REGULAR: i32 = 400;
pub(crate) const WEIGHT_SEMIBOLD: i32 = 600;

thread_local! {
    /// The face of the system message font ("Segoe UI" on English Windows).
    static FACE: OnceCell<[u16; 32]> = const { OnceCell::new() };
}

fn message_font_face() -> [u16; 32] {
    FACE.with(|face| {
        *face.get_or_init(|| {
            let mut metrics =
                NONCLIENTMETRICSW { cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32, ..Default::default() };
            // SAFETY: the buffer is a NONCLIENTMETRICSW with cbSize set, as the call requires.
            let ok = unsafe {
                SystemParametersInfoForDpi(
                    SPI_GETNONCLIENTMETRICS.0,
                    metrics.cbSize,
                    Some(&mut metrics as *mut NONCLIENTMETRICSW as *mut _),
                    0,
                    96,
                )
            }
            .is_ok();
            if ok && metrics.lfMessageFont.lfFaceName[0] != 0 {
                metrics.lfMessageFont.lfFaceName
            } else {
                crate::ui::text::fixed_wide::<32>("Segoe UI")
            }
        })
    })
}

/// A GDI font in the system message face, sized in physical pixels.
pub(crate) struct Font {
    handle: HFONT,
    owned: bool,
}

impl Font {
    /// The message font with an em height of `logical_px * scale` pixels.
    pub(crate) fn new(logical_px: f64, scale: f64, weight: i32) -> Font {
        let face = message_font_face();
        let logfont = LOGFONTW {
            lfHeight: -((logical_px * scale).round() as i32).max(1),
            lfWeight: weight,
            lfQuality: CLEARTYPE_QUALITY,
            lfCharSet: DEFAULT_CHARSET,
            lfFaceName: face,
            ..Default::default()
        };
        // SAFETY: the LOGFONTW is fully initialized.
        let handle = unsafe { CreateFontIndirectW(&logfont) };
        if handle.is_invalid() {
            crate::log_warn!("CreateFontIndirectW failed; using the default GUI font");
            // SAFETY: stock objects are always available and must not be deleted.
            let stock = unsafe { GetStockObject(DEFAULT_GUI_FONT) };
            return Font { handle: HFONT(stock.0), owned: false };
        }
        Font { handle, owned: true }
    }

    fn object(&self) -> HGDIOBJ {
        HGDIOBJ(self.handle.0)
    }
}

impl Drop for Font {
    fn drop(&mut self) {
        if self.owned {
            // SAFETY: we created this font and nothing selects it any more.
            unsafe {
                let _ = DeleteObject(self.object());
            }
        }
    }
}

/// A 32-bit top-down DIB section selected into a memory DC.
pub(crate) struct Canvas {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut u8,
    width: i32,
    height: i32,
}

impl Canvas {
    pub(crate) fn new(width: i32, height: i32) -> windows::core::Result<Canvas> {
        let (width, height) = (width.max(1), height.max(1));
        // SAFETY: standard DIB section creation; every handle is released in Drop
        // (or right here on failure).
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                return Err(windows::core::Error::from_thread());
            }
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
            let mut bits = std::ptr::null_mut();
            let bitmap = match CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) {
                Ok(bitmap) if !bits.is_null() => bitmap,
                Ok(bitmap) => {
                    let _ = DeleteObject(bitmap.into());
                    let _ = DeleteDC(dc);
                    return Err(windows::core::Error::from_thread());
                }
                Err(error) => {
                    let _ = DeleteDC(dc);
                    return Err(error);
                }
            };
            let previous = SelectObject(dc, bitmap.into());
            SetBkMode(dc, TRANSPARENT);
            Ok(Canvas { dc, bitmap, previous, bits: bits as *mut u8, width, height })
        }
    }

    pub(crate) fn width(&self) -> i32 {
        self.width
    }

    pub(crate) fn height(&self) -> i32 {
        self.height
    }

    /// Direct access to the pixels (flushes pending GDI drawing first).
    pub(crate) fn pixmap(&mut self) -> Pixmap<'_> {
        let len = self.width as usize * self.height as usize * 4;
        // SAFETY: `bits` points to the DIB section's width * height * 4 bytes, which
        // live as long as `self`; GdiFlush makes GDI finish writing to them first.
        unsafe {
            let _ = GdiFlush();
            Pixmap::new(std::slice::from_raw_parts_mut(self.bits, len), self.width as usize, self.height as usize)
        }
    }

    /// Draws one left-aligned line of text vertically centered in `rect`, with
    /// an end ellipsis if it does not fit.
    pub(crate) fn text(&mut self, text: &str, rect: RectPx, font: &Font, color: Color) {
        let mut buffer: Vec<u16> = text.encode_utf16().collect();
        if buffer.is_empty() {
            return;
        }
        let mut r: RECT = rect.to_win();
        // SAFETY: the DC and font are valid; DrawTextW reads `buffer` and writes nothing back
        // (no DT_MODIFYSTRING).
        unsafe {
            let previous = SelectObject(self.dc, font.object());
            SetTextColor(self.dc, COLORREF(color.colorref()));
            DrawTextW(
                self.dc,
                &mut buffer,
                &mut r,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
            );
            SelectObject(self.dc, previous);
        }
    }

    /// The width of `text` in pixels with `font`.
    pub(crate) fn measure(&self, text: &str, font: &Font) -> i32 {
        let buffer: Vec<u16> = text.encode_utf16().collect();
        let mut size = SIZE::default();
        // SAFETY: the DC and font are valid; the size is written to a local.
        unsafe {
            let previous = SelectObject(self.dc, font.object());
            let _ = GetTextExtentPoint32W(self.dc, &buffer, &mut size);
            SelectObject(self.dc, previous);
        }
        size.cx
    }

    /// Shows the canvas as the content of layered window `hwnd` at `at` (screen pixels).
    /// The pixels must already be premultiplied (see [`Pixmap::finish_rounded`]).
    pub(crate) fn present(&self, hwnd: HWND, at: PointPx) -> windows::core::Result<()> {
        let position = POINT { x: at.x, y: at.y };
        let size = SIZE { cx: self.width, cy: self.height };
        let source = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        // SAFETY: every pointer refers to a local that outlives the call.
        unsafe {
            let _ = GdiFlush();
            UpdateLayeredWindow(
                hwnd,
                None,
                Some(&position),
                Some(&size),
                Some(self.dc),
                Some(&source),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            )
        }
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        // SAFETY: restores the DC's original bitmap, then frees what `new` created.
        unsafe {
            SelectObject(self.dc, self.previous);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

/// A screen-space rectangle from logical layout units, relative to a canvas.
pub(crate) fn rect(x: f64, y: f64, w: f64, h: f64, scale: f64) -> RectPx {
    let p = |v: f64| (v * scale).round() as i32;
    RectPx::new(p(x), p(y), p(x + w), p(y + h))
}
