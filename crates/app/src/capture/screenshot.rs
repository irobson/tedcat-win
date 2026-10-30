//! Region screenshot to the clipboard.
//!
//! CONTRACT (owned by the input agent; keep the public API unchanged):
//! Captures `rect` (physical pixels, virtual-screen coordinates) from the screen
//! DC with `BitBlt` (`SRCCOPY | CAPTUREBLT`) into a 32-bit top-down DIB section,
//! then replaces the clipboard contents with both `CF_DIBV5` (alpha forced to
//! 255) and the registered `"PNG"` format (encoded with WIC), so it pastes in
//! classic apps, browsers and chat apps alike. Nothing is written to disk.
//! The caller hides the overlay first (`SelectionOverlay::hide_for_capture`).
//!
//! Implementation notes:
//! - `CF_DIBV5` is written bottom-up with `BI_RGB` and an sRGB colour space,
//!   the layout every reader accepts; Windows synthesizes `CF_DIB` and
//!   `CF_BITMAP` from it on demand. Pixels are opaque, so `BI_RGB` loses nothing
//!   and avoids the readers that misplace `BI_BITFIELDS` masks.
//! - The PNG is 24-bit RGB (32-bit BGRA if WIC insists).
//! - Both blocks are prepared before the clipboard is opened, so it is held
//!   only for a moment. A clipboard held by another app is retried briefly.
//! - If PNG encoding fails, the `CF_DIBV5` copy is still placed and the failure
//!   logged; the call fails only if the bitmap itself cannot be placed.
//! - COM is initialized for the call if the thread has not done it already.

use std::ffi::c_void;
use std::time::Duration;

use windows::core::{w, Error, Result, GUID};
use windows::Win32::Foundation::{
    GlobalFree, E_FAIL, E_INVALIDARG, E_OUTOFMEMORY, HANDLE, HGLOBAL, HWND, RPC_E_CHANGED_MODE,
};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GdiFlush, GetDC, ReleaseDC, SelectObject,
    BITMAPINFO, BITMAPINFOHEADER, BITMAPV5HEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
    LCS_GM_IMAGES, SRCCOPY,
};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatPng, GUID_WICPixelFormat24bppBGR, GUID_WICPixelFormat32bppBGRA,
    IWICBitmapFrameEncode, IWICImagingFactory, WICBitmapEncoderNoCache,
};
use windows::Win32::System::Com::StructuredStorage::{CreateStreamOnHGlobal, GetHGlobalFromStream};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, IStream, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    STREAM_SEEK_END,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_DIBV5;

use crate::geom::RectPx;

/// Attempts to open a clipboard another app is holding, and the pause between them.
const CLIPBOARD_ATTEMPTS: u32 = 8;
const CLIPBOARD_RETRY_DELAY: Duration = Duration::from_millis(15);
/// Rows converted per `WritePixels` call when encoding 24-bit PNG.
const PNG_ROWS_PER_CHUNK: usize = 64;
/// `LCS_sRGB` ('sRGB'); the constant lives in a `windows` feature this crate does not enable.
const LCS_SRGB: u32 = 0x7352_4742;

/// Copies `rect` to the clipboard. `owner` becomes the clipboard owner window.
pub fn copy_region_to_clipboard(rect: RectPx, owner: HWND) -> windows::core::Result<()> {
    if rect.is_empty() {
        return Err(Error::new(E_INVALIDARG, "The screenshot region is empty."));
    }
    let (width, height) = (rect.width() as u32, rect.height() as u32);
    let capture = ScreenCapture::grab(rect)?;
    let dib = dibv5_global(capture.pixels(), width, height)?;
    let png = png_global(capture.pixels(), width, height);
    drop(capture);
    publish(owner, dib, png)
}

/// The 32-bit top-down pixels of a screen region, in a DIB section.
struct ScreenCapture {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut u8,
    len: usize,
}

impl ScreenCapture {
    fn grab(rect: RectPx) -> Result<Self> {
        let (width, height) = (rect.width(), rect.height());
        let len = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .filter(|&len| len <= i32::MAX as usize)
            .ok_or_else(|| Error::new(E_OUTOFMEMORY, "The screenshot region is too large."))?;
        let screen = ScreenDc::get()?;
        // SAFETY: a memory DC compatible with the screen; deleted by `Drop` or below.
        let dc = unsafe { CreateCompatibleDC(Some(screen.0)) };
        if dc.is_invalid() {
            return Err(Error::from_thread());
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
        let mut bits: *mut c_void = std::ptr::null_mut();
        // SAFETY: `info` describes a 32-bit top-down DIB; `bits` receives its pixels.
        let bitmap = match unsafe { CreateDIBSection(Some(screen.0), &info, DIB_RGB_COLORS, &mut bits, None, 0) } {
            Ok(bitmap) if !bits.is_null() => bitmap,
            Ok(bitmap) => {
                // SAFETY: freeing what was just created.
                unsafe {
                    let _ = DeleteObject(bitmap.into());
                    let _ = DeleteDC(dc);
                }
                return Err(Error::new(E_FAIL, "The screenshot bitmap has no pixels."));
            }
            Err(error) => {
                // SAFETY: freeing the DC created above.
                unsafe {
                    let _ = DeleteDC(dc);
                }
                return Err(error);
            }
        };
        // SAFETY: selecting our bitmap into our DC; restored by `Drop`.
        let previous = unsafe { SelectObject(dc, bitmap.into()) };
        let mut capture = ScreenCapture { dc, bitmap, previous, bits: bits.cast(), len };
        // SAFETY: both DCs are valid; the source rectangle is in screen
        // coordinates, which for the screen DC are virtual-screen coordinates.
        unsafe {
            BitBlt(dc, 0, 0, width, height, Some(screen.0), rect.left, rect.top, SRCCOPY | CAPTUREBLT)?;
            let _ = GdiFlush();
        }
        force_opaque(capture.pixels_mut());
        Ok(capture)
    }

    fn pixels(&self) -> &[u8] {
        // SAFETY: the DIB section holds `len` bytes and lives as long as `self`.
        unsafe { std::slice::from_raw_parts(self.bits, self.len) }
    }

    fn pixels_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, and `&mut self` makes the access exclusive.
        unsafe { std::slice::from_raw_parts_mut(self.bits, self.len) }
    }
}

impl Drop for ScreenCapture {
    fn drop(&mut self) {
        // SAFETY: deselect, then free the bitmap and the DC created in `grab`.
        unsafe {
            SelectObject(self.dc, self.previous);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

/// The screen DC, released on drop.
struct ScreenDc(HDC);

impl ScreenDc {
    fn get() -> Result<Self> {
        // SAFETY: the DC of the whole virtual screen; released by `Drop`.
        let dc = unsafe { GetDC(None) };
        if dc.is_invalid() {
            Err(Error::from_thread())
        } else {
            Ok(Self(dc))
        }
    }
}

impl Drop for ScreenDc {
    fn drop(&mut self) {
        // SAFETY: releasing the DC obtained in `get`.
        unsafe {
            ReleaseDC(None, self.0);
        }
    }
}

/// A movable global memory block, freed on drop unless handed to the clipboard.
struct OwnedGlobal {
    handle: Option<HGLOBAL>,
    len: usize,
}

impl OwnedGlobal {
    fn alloc(len: usize) -> Result<Self> {
        // SAFETY: a plain allocation, freed by `Drop` unless released.
        let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, len.max(1)) }?;
        Ok(Self { handle: Some(handle), len })
    }

    fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let global = Self::alloc(bytes.len())?;
        global.write(|dst| dst.copy_from_slice(bytes))?;
        Ok(global)
    }

    /// Locks the block and lets `fill` write all of it.
    fn write(&self, fill: impl FnOnce(&mut [u8])) -> Result<()> {
        let handle = self.handle.ok_or_else(|| Error::from_hresult(E_FAIL))?;
        // SAFETY: the block is at least `len` bytes; it stays locked until the
        // unlock below and nothing else references it.
        unsafe {
            let ptr = GlobalLock(handle);
            if ptr.is_null() {
                return Err(Error::from_thread());
            }
            fill(std::slice::from_raw_parts_mut(ptr.cast::<u8>(), self.len));
            let _ = GlobalUnlock(handle);
        }
        Ok(())
    }

    /// Gives up ownership (the clipboard took it).
    fn release(mut self) {
        self.handle = None;
    }
}

impl Drop for OwnedGlobal {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            // SAFETY: we still own the block.
            unsafe {
                let _ = GlobalFree(Some(handle));
            }
        }
    }
}

/// The `CF_DIBV5` header for an opaque 32-bit bottom-up image.
fn dibv5_header(width: u32, height: u32) -> BITMAPV5HEADER {
    BITMAPV5HEADER {
        bV5Size: std::mem::size_of::<BITMAPV5HEADER>() as u32,
        bV5Width: width as i32,
        bV5Height: height as i32,
        bV5Planes: 1,
        bV5BitCount: 32,
        bV5Compression: BI_RGB,
        bV5SizeImage: width * height * 4,
        bV5CSType: LCS_SRGB,
        bV5Intent: LCS_GM_IMAGES as u32,
        ..Default::default()
    }
}

/// A `CF_DIBV5` block for top-down BGRA `pixels`.
fn dibv5_global(pixels: &[u8], width: u32, height: u32) -> Result<OwnedGlobal> {
    let header = dibv5_header(width, height);
    let header_len = std::mem::size_of::<BITMAPV5HEADER>();
    let global = OwnedGlobal::alloc(header_len + pixels.len())?;
    global.write(|block| {
        let (head, body) = block.split_at_mut(header_len);
        // SAFETY: BITMAPV5HEADER is plain old data of exactly `header_len` bytes.
        let header_bytes = unsafe { std::slice::from_raw_parts((&header as *const BITMAPV5HEADER).cast::<u8>(), header_len) };
        head.copy_from_slice(header_bytes);
        flip_rows(pixels, width as usize * 4, body);
    })?;
    Ok(global)
}

/// The PNG block, or `None` (logged) when the format or the encoder is unavailable.
fn png_global(pixels: &[u8], width: u32, height: u32) -> Option<(u32, OwnedGlobal)> {
    // SAFETY: a null-terminated literal.
    let format = unsafe { RegisterClipboardFormatW(w!("PNG")) };
    if format == 0 {
        crate::log_warn!("The PNG clipboard format could not be registered; copying the bitmap only.");
        return None;
    }
    match encode_png(pixels, width, height).and_then(|bytes| OwnedGlobal::from_bytes(&bytes)) {
        Ok(global) => Some((format, global)),
        Err(error) => {
            crate::log_warn!("PNG encoding failed ({}); copying the bitmap only.", error.message());
            None
        }
    }
}

/// Replaces the clipboard contents with the bitmap and, if available, the PNG.
fn publish(owner: HWND, dib: OwnedGlobal, png: Option<(u32, OwnedGlobal)>) -> Result<()> {
    let _clipboard = ClipboardSession::open(owner)?;
    // SAFETY: the clipboard is open by this thread.
    unsafe { EmptyClipboard() }?;
    set_clipboard(u32::from(CF_DIBV5.0), dib)?;
    if let Some((format, png)) = png {
        if let Err(error) = set_clipboard(format, png) {
            crate::log_warn!("Could not place the PNG on the clipboard: {}", error.message());
        }
    }
    Ok(())
}

fn set_clipboard(format: u32, data: OwnedGlobal) -> Result<()> {
    let handle = data.handle.ok_or_else(|| Error::from_hresult(E_FAIL))?;
    // SAFETY: the clipboard is open and emptied by this thread; `handle` is an
    // unlocked GMEM_MOVEABLE block. On success the system owns it.
    unsafe { SetClipboardData(format, Some(HANDLE(handle.0))) }?;
    data.release();
    Ok(())
}

/// An open clipboard, closed on drop.
struct ClipboardSession;

impl ClipboardSession {
    fn open(owner: HWND) -> Result<Self> {
        let owner = (!owner.is_invalid()).then_some(owner);
        let mut last_error = Error::from_hresult(E_FAIL);
        for attempt in 0..CLIPBOARD_ATTEMPTS {
            // SAFETY: `owner` is a window of this thread (or none).
            match unsafe { OpenClipboard(owner) } {
                Ok(()) => return Ok(Self),
                Err(error) => last_error = error,
            }
            if attempt + 1 < CLIPBOARD_ATTEMPTS {
                std::thread::sleep(CLIPBOARD_RETRY_DELAY);
            }
        }
        crate::log_warn!("The clipboard stayed busy: {}", last_error.message());
        Err(last_error)
    }
}

impl Drop for ClipboardSession {
    fn drop(&mut self) {
        // SAFETY: the clipboard was opened by this thread.
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

/// Initializes COM for the current scope if the thread has not already.
struct ComScope {
    uninitialize: bool,
}

impl ComScope {
    fn enter() -> Self {
        // SAFETY: balanced by `Drop` when it succeeds (S_OK or S_FALSE).
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        // RPC_E_CHANGED_MODE means the thread is already in the MTA, which
        // works just as well; any other failure surfaces at CoCreateInstance.
        if hr.is_err() && hr != RPC_E_CHANGED_MODE {
            crate::log_warn!("CoInitializeEx failed: {}", hr.message());
        }
        Self { uninitialize: hr.is_ok() }
    }
}

impl Drop for ComScope {
    fn drop(&mut self) {
        if self.uninitialize {
            // SAFETY: balances the successful CoInitializeEx in `enter`.
            unsafe { CoUninitialize() };
        }
    }
}

/// Encodes top-down BGRA `pixels` as PNG bytes with WIC.
fn encode_png(pixels: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let _com = ComScope::enter();
    // Every COM object is created and released inside, before `_com` drops.
    encode_png_with_com(pixels, width, height)
}

fn encode_png_with_com(pixels: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    // SAFETY: standard WIC encoding sequence on objects owned by this function.
    unsafe {
        let factory: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let stream = CreateStreamOnHGlobal(HGLOBAL::default(), true)?;
        let encoder = factory.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
        encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;
        let mut frame = None;
        let mut options = None;
        encoder.CreateNewFrame(&mut frame, &mut options)?;
        let frame = frame.ok_or_else(|| Error::new(E_FAIL, "WIC returned no PNG frame."))?;
        frame.Initialize(options.as_ref())?;
        frame.SetSize(width, height)?;
        let mut format: GUID = GUID_WICPixelFormat24bppBGR;
        frame.SetPixelFormat(&mut format)?;
        if format == GUID_WICPixelFormat24bppBGR {
            write_bgr(&frame, pixels, width as usize, height as usize)?;
        } else if format == GUID_WICPixelFormat32bppBGRA {
            frame.WritePixels(height, width * 4, pixels)?;
        } else {
            return Err(Error::new(E_FAIL, "The PNG encoder offered an unexpected pixel format."));
        }
        frame.Commit()?;
        encoder.Commit()?;
        stream_bytes(&stream)
    }
}

/// Writes BGRA rows to a 24-bit frame, a chunk of rows at a time.
///
/// # Safety
/// `frame` must be an initialized frame of `width` × `height` pixels in 24bppBGR.
unsafe fn write_bgr(frame: &IWICBitmapFrameEncode, pixels: &[u8], width: usize, height: usize) -> Result<()> {
    let stride = bgr_stride(width);
    let mut buffer = Vec::with_capacity(stride * PNG_ROWS_PER_CHUNK.min(height));
    for rows in pixels.chunks(width * 4 * PNG_ROWS_PER_CHUNK) {
        let count = rows.len() / (width * 4);
        bgra_to_bgr(rows, width, &mut buffer);
        // SAFETY: `buffer` holds `count` rows of `stride` bytes.
        unsafe { frame.WritePixels(count as u32, stride as u32, &buffer) }?;
    }
    Ok(())
}

/// Copies the encoded bytes out of an HGLOBAL-backed stream.
///
/// # Safety
/// `stream` must come from `CreateStreamOnHGlobal`.
unsafe fn stream_bytes(stream: &IStream) -> Result<Vec<u8>> {
    let mut size = 0u64;
    // SAFETY: `size` is a valid out pointer.
    unsafe { stream.Seek(0, STREAM_SEEK_END, Some(&mut size)) }?;
    let len = usize::try_from(size).map_err(|_| Error::from_hresult(E_OUTOFMEMORY))?;
    // SAFETY: the stream is backed by an HGLOBAL that it keeps alive.
    unsafe {
        let global = GetHGlobalFromStream(stream)?;
        if GlobalSize(global) < len {
            return Err(Error::new(E_FAIL, "The PNG stream is shorter than reported."));
        }
        let ptr = GlobalLock(global);
        if ptr.is_null() {
            return Err(Error::from_thread());
        }
        let bytes = std::slice::from_raw_parts(ptr.cast::<u8>(), len).to_vec();
        let _ = GlobalUnlock(global);
        Ok(bytes)
    }
}

/// Sets the alpha byte of every BGRA pixel to 255 (GDI leaves it undefined).
fn force_opaque(pixels: &mut [u8]) {
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel[3] = 255;
    }
}

/// Copies top-down rows of `stride` bytes into `dst` in bottom-up order.
fn flip_rows(src: &[u8], stride: usize, dst: &mut [u8]) {
    if stride == 0 {
        return;
    }
    for (src_row, dst_row) in src.chunks_exact(stride).zip(dst.chunks_exact_mut(stride).rev()) {
        dst_row.copy_from_slice(src_row);
    }
}

/// Bytes per 24-bit row, padded to 4 bytes.
fn bgr_stride(width: usize) -> usize {
    (width * 3 + 3) & !3
}

/// Converts whole BGRA rows to padded BGR rows in `out` (replacing its contents).
fn bgra_to_bgr(rows: &[u8], width: usize, out: &mut Vec<u8>) {
    let stride = bgr_stride(width);
    out.clear();
    for row in rows.chunks_exact(width * 4) {
        let start = out.len();
        for pixel in row.as_chunks::<4>().0 {
            out.extend_from_slice(&pixel[..3]);
        }
        out.resize(start + stride, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpha_is_forced_to_opaque() {
        let mut pixels = vec![1, 2, 3, 0, 4, 5, 6, 7];
        force_opaque(&mut pixels);
        assert_eq!(pixels, vec![1, 2, 3, 255, 4, 5, 6, 255]);
    }

    #[test]
    fn rows_are_flipped_for_a_bottom_up_dib() {
        let src = [1u8, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3];
        let mut dst = [0u8; 12];
        flip_rows(&src, 4, &mut dst);
        assert_eq!(dst, [3, 3, 3, 3, 2, 2, 2, 2, 1, 1, 1, 1]);
    }

    #[test]
    fn bgr_rows_are_padded_to_four_bytes() {
        assert_eq!(bgr_stride(1), 4);
        assert_eq!(bgr_stride(4), 12);
        assert_eq!(bgr_stride(5), 16);
        let rows = [10u8, 20, 30, 255, 40, 50, 60, 255, 1, 2, 3, 255, 4, 5, 6, 255];
        let mut out = Vec::new();
        bgra_to_bgr(&rows, 2, &mut out);
        assert_eq!(out, vec![10, 20, 30, 40, 50, 60, 0, 0, 1, 2, 3, 4, 5, 6, 0, 0]);
    }

    #[test]
    fn dibv5_header_describes_an_opaque_bottom_up_srgb_image() {
        let header = dibv5_header(640, 400);
        assert_eq!(header.bV5Size, 124);
        assert_eq!((header.bV5Width, header.bV5Height), (640, 400));
        assert_eq!(header.bV5BitCount, 32);
        assert_eq!(header.bV5Compression, BI_RGB);
        assert_eq!(header.bV5SizeImage, 640 * 400 * 4);
        assert_eq!(header.bV5CSType, LCS_SRGB);
        assert_eq!(&LCS_SRGB.to_be_bytes(), b"sRGB");
    }
}
