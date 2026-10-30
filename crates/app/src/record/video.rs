//! Region video: Windows.Graphics.Capture on the region's monitor, cropped on
//! the GPU into a CPU-readable staging texture, throttled to 30 fps and handed
//! to the sink writer as top-down RGB32 frames.
//!
//! The frame pool is free threaded, so `FrameArrived` runs on a capture
//! thread pool thread; all per-frame state lives behind a `Mutex`.
//!
//! Frames are written one frame late: each frame's duration is the time until
//! the next kept frame, which is exact even though capture only delivers
//! frames when pixels change. The last frame is handed back on stop so the
//! recorder can extend it to the stop time.

use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tedcat_core::VideoEncodingPlan;
use windows::core::{IInspectable, Interface};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureAccess, GraphicsCaptureAccessKind,
    GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Security::Authorization::AppCapabilityAccess::AppCapabilityAccessStatus;
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{E_FAIL, HMODULE};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D, D3D11_BOX,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice};
use windows::Win32::Media::MediaFoundation::IMFMediaBuffer;
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

use super::frame::{self, CropCopy, CropInput, THUMBNAIL_MAX_HEIGHT, THUMBNAIL_MAX_WIDTH};
use super::session::Shared;
use super::sys::describe;
use super::timing::FrameThrottle;
use super::writer::new_buffer;
use crate::geom::{MonitorInfo, RectPx};
use crate::messages::Thumbnail;
use crate::{log_info, log_warn};

/// How long `start` waits for the first captured frame.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(3);
/// How long `start` waits for Windows to answer the borderless-capture request.
const BORDERLESS_REQUEST_TIMEOUT: Duration = Duration::from_secs(1);

/// A captured frame waiting to be written (or the last one, at stop).
pub(super) struct PendingFrame {
    pub buffer: IMFMediaBuffer,
    /// 100 ns since the recording start.
    pub time: i64,
}

/// What the video side hands back on stop.
pub(super) struct VideoOutcome {
    /// The last kept frame, not yet written.
    pub last: Option<PendingFrame>,
    pub thumbnail: Option<Thumbnail>,
    /// Nominal frame interval in 100 ns.
    pub interval: i64,
}

/// GPU objects used by the frame callback.
struct Gpu {
    context: ID3D11DeviceContext,
    staging: ID3D11Texture2D,
}

struct VideoState {
    gpu: Option<Gpu>,
    crop: CropInput,
    throttle: FrameThrottle,
    pending: Option<PendingFrame>,
    thumbnail: Option<Thumbnail>,
    first_frame: Option<SyncSender<Result<(), String>>>,
    stopped: bool,
    failed: bool,
    size_logged: bool,
    /// Frames dropped because the encoder fell behind.
    dropped: u64,
}

// SAFETY: D3D11 interfaces are free threaded (the device is created with
// multithread protection on), and the Media Foundation buffer is free threaded
// too. The state is only touched under its Mutex.
unsafe impl Send for VideoState {}

/// A running monitor capture. Lives on the recorder thread.
pub(super) struct VideoCapture {
    state: Arc<Mutex<VideoState>>,
    item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    frame_token: Option<i64>,
    closed_token: Option<i64>,
    _device: ID3D11Device,
    _winrt_device: IDirect3DDevice,
}

impl VideoCapture {
    /// Starts capturing `monitor` and returns once the first frame of `region`
    /// was processed.
    pub(super) fn start(
        monitor: &MonitorInfo,
        region: RectPx,
        plan: &VideoEncodingPlan,
        show_cursor: bool,
        shared: Arc<Shared>,
    ) -> Result<VideoCapture, String> {
        if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
            return Err("Screen recording is not supported on this version of Windows.".into());
        }
        let fail = |what: &str, e: windows::core::Error| format!("Could not start the screen capture ({what}): {}", describe(&e));

        let (device, context) = create_device().map_err(|e| fail("Direct3D device", e))?;
        enable_multithread_protection(&context);
        let winrt_device = winrt_device(&device).map_err(|e| fail("Direct3D interop", e))?;
        let item = capture_item(monitor).map_err(|e| fail("monitor", e))?;
        let size = item.Size().map_err(|e| fail("monitor size", e))?;
        if size.Width != monitor.rect.width() || size.Height != monitor.rect.height() {
            log_warn!(
                "capture item is {}x{} but the monitor rectangle is {}x{}",
                size.Width,
                size.Height,
                monitor.rect.width(),
                monitor.rect.height()
            );
        }
        let staging = create_staging(&device, plan.pixel_width, plan.pixel_height).map_err(|e| fail("staging texture", e))?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )
        .map_err(|e| fail("frame pool", e))?;
        let session = match pool.CreateCaptureSession(&item) {
            Ok(session) => session,
            Err(e) => {
                let _ = pool.Close();
                return Err(fail("session", e));
            }
        };
        if let Err(e) = session.SetIsCursorCaptureEnabled(show_cursor) {
            log_info!("cursor capture setting unavailable: {}", describe(&e));
        }
        // Without the borderless grant Windows accepts `IsBorderRequired(false)`
        // but still draws its yellow border around the whole monitor.
        match request_borderless_capture() {
            Ok(status) if status == AppCapabilityAccessStatus::Allowed => {}
            Ok(status) => log_info!("borderless capture not granted (status {}); Windows shows its capture border", status.0),
            Err(e) => log_info!("borderless capture unavailable ({e}); Windows shows its capture border"),
        }
        if let Err(e) = session.SetIsBorderRequired(false) {
            log_info!("capture border cannot be disabled here: {}", describe(&e));
        }

        let (first_tx, first_rx) = mpsc::sync_channel(1);
        let state = Arc::new(Mutex::new(VideoState {
            gpu: Some(Gpu { context, staging }),
            crop: CropInput {
                region_left: region.left,
                region_top: region.top,
                monitor_left: monitor.rect.left,
                monitor_top: monitor.rect.top,
                out_width: plan.pixel_width,
                out_height: plan.pixel_height,
            },
            throttle: FrameThrottle::new(VideoEncodingPlan::FRAMES_PER_SECOND),
            pending: None,
            thumbnail: None,
            first_frame: Some(first_tx),
            stopped: false,
            failed: false,
            size_logged: false,
            dropped: 0,
        }));
        let mut capture = VideoCapture {
            state: state.clone(),
            item,
            pool,
            session,
            frame_token: None,
            closed_token: None,
            _device: device,
            _winrt_device: winrt_device,
        };

        let on_frame = {
            let shared = shared.clone();
            TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |pool, _| {
                frame_arrived(&state, &shared, pool.as_ref());
                Ok(())
            })
        };
        match capture.pool.FrameArrived(&on_frame) {
            Ok(token) => capture.frame_token = Some(token),
            Err(e) => {
                capture.stop();
                return Err(fail("frame events", e));
            }
        }
        let on_closed = TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
            shared.interrupt("The display being recorded was disconnected.");
            Ok(())
        });
        match capture.item.Closed(&on_closed) {
            Ok(token) => capture.closed_token = Some(token),
            Err(e) => log_warn!("cannot watch for the display going away: {}", describe(&e)),
        }
        if let Err(e) = capture.session.StartCapture() {
            capture.stop();
            return Err(fail("start", e));
        }
        match first_rx.recv_timeout(FIRST_FRAME_TIMEOUT) {
            Ok(Ok(())) => Ok(capture),
            Ok(Err(e)) => {
                capture.stop();
                Err(e)
            }
            Err(_) => {
                capture.stop();
                Err("The screen capture did not deliver any picture.".into())
            }
        }
    }

    /// Stops the capture and returns the frame still held back.
    pub(super) fn stop(self) -> VideoOutcome {
        lock(&self.state).stopped = true;
        if let Some(token) = self.frame_token {
            let _ = self.pool.RemoveFrameArrived(token);
        }
        if let Some(token) = self.closed_token {
            let _ = self.item.RemoveClosed(token);
        }
        let _ = self.session.Close();
        let _ = self.pool.Close();
        // Waits for a callback still in flight, then releases the GPU objects
        // here, on the thread that owns the apartment, even if WinRT keeps the
        // (now empty) handler alive a little longer.
        let mut state = lock(&self.state);
        state.gpu = None;
        state.first_frame = None;
        if state.dropped > 0 {
            log_warn!("{} video frames were dropped because the encoder fell behind", state.dropped);
        }
        VideoOutcome { last: state.pending.take(), thumbnail: state.thumbnail.take(), interval: state.throttle.interval() }
    }
}

/// Asks Windows to let TedCat capture without the yellow capture border.
/// Unpackaged apps get the grant without a prompt; the wait is bounded so a
/// prompt or a slow broker can never hold up the start of a recording.
fn request_borderless_capture() -> Result<AppCapabilityAccessStatus, String> {
    let request =
        GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless).map_err(|e| describe(&e))?;
    let deadline = Instant::now() + BORDERLESS_REQUEST_TIMEOUT;
    loop {
        let status = request.Status().map_err(|e| describe(&e))?;
        if status == windows_future::AsyncStatus::Completed {
            return request.GetResults().map_err(|e| describe(&e));
        }
        if status != windows_future::AsyncStatus::Started {
            return Err(format!("the request ended with status {}", status.0));
        }
        if Instant::now() >= deadline {
            let _ = request.Cancel();
            return Err("no answer in time".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn lock(state: &Mutex<VideoState>) -> MutexGuard<'_, VideoState> {
    state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `FrameArrived` handler body. Never panics and never returns an error to
/// WinRT; failures are reported through `Shared::interrupt` (or to `start`).
fn frame_arrived(state: &Mutex<VideoState>, shared: &Shared, pool: Option<&Direct3D11CaptureFramePool>) {
    let Some(pool) = pool else { return };
    let Ok(frame) = pool.TryGetNextFrame() else { return };
    {
        let mut state = lock(state);
        if !state.stopped && !state.failed {
            if let Err(reason) = state.process(&frame, shared) {
                state.failed = true;
                match state.first_frame.take() {
                    Some(first) => {
                        let _ = first.send(Err(reason));
                    }
                    None => shared.interrupt(&reason),
                }
            }
        }
    }
    let _ = frame.Close();
}

impl VideoState {
    fn process(&mut self, frame: &Direct3D11CaptureFrame, shared: &Shared) -> Result<(), String> {
        let capture_error = |e: windows::core::Error| format!("Screen capture stopped: {}", describe(&e));
        let time = frame.SystemRelativeTime().map_err(capture_error)?.Duration;
        let t0 = shared.t0_or_init(time);
        let time = time - t0;
        if time < 0 || !self.throttle.accept(time) {
            return Ok(());
        }
        if self.gpu.is_none() || (self.pending.is_some() && self.encoder_is_behind(shared)) {
            return Ok(());
        }
        let Some(gpu) = self.gpu.as_ref() else { return Ok(()) };
        let content = frame.ContentSize().map_err(capture_error)?;
        let texture: ID3D11Texture2D = frame
            .Surface()
            .and_then(|surface| surface.cast::<IDirect3DDxgiInterfaceAccess>())
            // SAFETY: GetInterface returns a new reference to the frame's texture.
            .and_then(|access| unsafe { access.GetInterface::<ID3D11Texture2D>() })
            .map_err(capture_error)?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `desc` is a valid out-parameter.
        unsafe { texture.GetDesc(&mut desc) };
        let available_width = (content.Width.max(0) as u32).min(desc.Width);
        let available_height = (content.Height.max(0) as u32).min(desc.Height);
        let copy = frame::crop_copy(&self.crop, available_width, available_height);
        if !self.size_logged && copy.is_none_or(|c| !c.covers(self.crop.out_width, self.crop.out_height)) {
            self.size_logged = true;
            log_warn!(
                "region only partly inside the captured content ({available_width}x{available_height}); filling with black"
            );
        }
        if let Some(copy) = copy {
            let source_box = D3D11_BOX {
                left: copy.src_left,
                top: copy.src_top,
                front: 0,
                right: copy.src_right,
                bottom: copy.src_bottom,
                back: 1,
            };
            // SAFETY: the box lies inside the source texture and the destination
            // offset plus box size fits the staging texture (see `crop_copy`).
            unsafe {
                gpu.context.CopySubresourceRegion(&gpu.staging, 0, copy.dst_x, copy.dst_y, 0, &texture, 0, Some(&source_box))
            };
        }
        let (width, height) = (self.crop.out_width, self.crop.out_height);
        let mut thumbnail = None;
        let wants_thumbnail = self.thumbnail.is_none();
        let buffer = read_staging(gpu, width, height, copy, |picture| {
            if wants_thumbnail {
                thumbnail = Some(make_thumbnail(picture, width, height));
            }
        })
        .map_err(capture_error)?;
        if thumbnail.is_some() {
            self.thumbnail = thumbnail;
        }

        if let Some(previous) = self.pending.take() {
            shared
                .writer()
                .write_video(&previous.buffer, previous.time, time - previous.time)
                .map_err(|e| format!("Could not write the recording: {e}"))?;
        }
        self.pending = Some(PendingFrame { buffer, time });
        if let Some(first) = self.first_frame.take() {
            let _ = first.send(Ok(()));
        }
        Ok(())
    }
}

impl VideoState {
    /// True when too many raw frames already wait for the encoder; the frame
    /// is then skipped (the previous one simply stays on screen longer).
    fn encoder_is_behind(&mut self, shared: &Shared) -> bool {
        let frame_bytes = self.crop.out_width as usize * self.crop.out_height as usize * 4;
        let behind = shared.writer().video_backlog().is_some_and(|backlog| backlog > frame::max_video_backlog(frame_bytes));
        if behind {
            if self.dropped == 0 {
                log_warn!("the video encoder is falling behind; dropping frames until it catches up");
            }
            self.dropped += 1;
        }
        behind
    }
}

/// Maps the staging texture and copies the picture into a new media buffer.
fn read_staging(
    gpu: &Gpu,
    width: u32,
    height: u32,
    copy: Option<CropCopy>,
    inspect: impl FnOnce(&[u8]),
) -> windows::core::Result<IMFMediaBuffer> {
    let len = width as usize * height as usize * 4;
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    // SAFETY: the staging texture was created with CPU read access; it is
    // unmapped below on every path.
    unsafe { gpu.context.Map(&gpu.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))? };
    let result = if mapped.pData.is_null() || height == 0 {
        Err(windows::core::Error::from_hresult(E_FAIL))
    } else {
        let pitch = mapped.RowPitch as usize;
        let src_len = pitch * (height as usize - 1) + width as usize * 4;
        // SAFETY: a mapped staging texture of `height` rows holds at least
        // `RowPitch * (height - 1) + width * 4` readable bytes.
        let src = unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, src_len) };
        new_buffer(len, |dst| {
            frame::copy_picture(src, pitch, dst, width, height, copy);
            inspect(dst);
        })
    };
    // SAFETY: balances the successful Map above.
    unsafe { gpu.context.Unmap(&gpu.staging, 0) };
    result
}

fn make_thumbnail(picture: &[u8], width: u32, height: u32) -> Thumbnail {
    let (w, h) = frame::fit_within(width, height, THUMBNAIL_MAX_WIDTH, THUMBNAIL_MAX_HEIGHT);
    Thumbnail { width: w, height: h, bgra: frame::scale_bgra(picture, width, height, width as usize * 4, w, h) }
}

/// A D3D11 device with BGRA support: the hardware adapter, or WARP if that fails.
fn create_device() -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut last_error = windows::core::Error::from_hresult(E_FAIL);
    for driver in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
        match create_device_of(driver) {
            Ok(pair) => {
                if driver == D3D_DRIVER_TYPE_WARP {
                    log_warn!("using the WARP software renderer for screen capture");
                }
                return Ok(pair);
            }
            Err(e) => {
                log_warn!("D3D11CreateDevice({driver:?}) failed: {}", describe(&e));
                last_error = e;
            }
        }
    }
    Err(last_error)
}

fn create_device_of(driver: D3D_DRIVER_TYPE) -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    // SAFETY: out-parameters are valid; no adapter means the default one.
    unsafe {
        D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            driver,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    match (device, context) {
        (Some(device), Some(context)) => Ok((device, context)),
        _ => Err(windows::core::Error::from_hresult(E_FAIL)),
    }
}

/// WGC uses the device from its own threads; make the immediate context safe for that.
fn enable_multithread_protection(context: &ID3D11DeviceContext) {
    match context.cast::<ID3D11Multithread>() {
        // SAFETY: plain setter on a valid interface.
        Ok(multithread) => {
            let _ = unsafe { multithread.SetMultithreadProtected(true) };
        }
        Err(e) => log_warn!("ID3D11Multithread unavailable: {}", describe(&e)),
    }
}

fn winrt_device(device: &ID3D11Device) -> windows::core::Result<IDirect3DDevice> {
    let dxgi: IDXGIDevice = device.cast()?;
    // SAFETY: wraps a valid DXGI device in its WinRT projection.
    let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? };
    inspectable.cast()
}

fn capture_item(monitor: &MonitorInfo) -> windows::core::Result<GraphicsCaptureItem> {
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    // SAFETY: the HMONITOR comes from MonitorFromPoint/GetMonitorInfo; a stale
    // handle makes the call fail rather than misbehave.
    unsafe { interop.CreateForMonitor(monitor.hmonitor()) }
}

fn create_staging(device: &ID3D11Device, width: u32, height: u32) -> windows::core::Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut texture = None;
    // SAFETY: `desc` describes a valid staging texture; no initial data.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture))? };
    texture.ok_or_else(|| windows::core::Error::from_hresult(E_FAIL))
}
