//! Region video: Windows.Graphics.Capture on the region's monitor, cropped on
//! the GPU, read back through a CPU-readable staging texture, throttled to
//! 30 fps and handed to the sink writer as top-down RGB32 frames.
//!
//! A region larger than the encoder accepts (see `VideoEncodingPlan`) is
//! cropped at its own size and scaled down to the encoded size: on the GPU by
//! the D3D11 video processor, or on the CPU (`frame::scale_bgra_into`) when
//! the GPU cannot. The thumbnail and the encoder backlog use the encoded size.
//!
//! The frame pool is free threaded, so `FrameArrived` runs on a capture
//! thread pool thread; all per-frame state lives behind a `Mutex`.
//!
//! The pool is never recreated. When the display's size changes (resolution
//! or rotation), its frames no longer match the pool and the region no longer
//! means the same pixels, so the recording is interrupted; stop then
//! finalizes what was captured before the change.
//!
//! Frames are written one frame late: each frame's duration is the time until
//! the next kept frame, which is exact even though capture only delivers
//! frames when pixels change. The last frame is handed back on stop so the
//! recorder can extend it to the stop time.

use std::mem::ManuallyDrop;
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tedcat_core::VideoEncodingPlan;
use windows::core::{IInspectable, Interface};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{E_FAIL, HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11RenderTargetView, ID3D11Texture2D,
    ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
    D3D11_SDK_VERSION, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT,
    D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_OPTIMAL_QUALITY,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice, DXGI_ERROR_UNSUPPORTED};
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

/// Why a recording stops when the monitor's size changes under it: a
/// sentence (see `messages::sentence`); the UI appends its own after it.
const DISPLAY_CHANGED: &str = "The display resolution changed during the recording.";

/// Opaque black, for the parts of the region outside the captured content.
const BLACK: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

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
    /// CPU-readable texture the picture is read back from: the region at its
    /// source size, or the encoded picture when the GPU scales it.
    staging: ID3D11Texture2D,
    scaling: Scaling,
}

/// How the cropped region becomes the encoded picture.
enum Scaling {
    /// The region is encoded at its own size.
    None,
    /// The D3D11 video processor scales the region on the GPU.
    Gpu(GpuScaler),
    /// The staging texture holds the region at its source size and each frame
    /// is scaled on the CPU, through `scratch` (the region, black-filled).
    Cpu { scratch: Vec<u8> },
}

/// Scales the cropped region to the encoded size with the D3D11 video processor.
struct GpuScaler {
    /// The immediate context's critical section, held for each frame's whole
    /// clear / copy / blit / read-back sequence (see `DeviceSection`).
    lock: Option<ID3D11Multithread>,
    video_context: ID3D11VideoContext,
    processor: ID3D11VideoProcessor,
    /// The region at its source size: the crop lands here.
    region: ID3D11Texture2D,
    /// Render target view of `region`, to black out what the frame does not cover.
    region_target: ID3D11RenderTargetView,
    region_input: ID3D11VideoProcessorInputView,
    /// The encoded picture, copied to the staging texture after each blit.
    output: ID3D11Texture2D,
    output_view: ID3D11VideoProcessorOutputView,
    _enumerator: ID3D11VideoProcessorEnumerator,
}

struct VideoState {
    gpu: Option<Gpu>,
    /// The region, at its source size.
    crop: CropInput,
    /// Size of the encoded picture (equal to the crop size unless scaled).
    encoded: (u32, u32),
    /// The size the frame pool was created with: the monitor's at start.
    pool_size: SizeInt32,
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

// SAFETY: D3D11 interfaces, video processor ones included, are free threaded
// (the immediate context has multithread protection on), and the Media
// Foundation buffer is free threaded too. The state is only touched under its
// Mutex.
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
        let (staging, scaling) = create_scaling(&device, &context, plan).map_err(|e| fail("staging texture", e))?;
        log_info!("frame pool {}x{}", size.Width, size.Height);
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
        if let Err(e) = session.SetIsBorderRequired(false) {
            log_info!("capture border cannot be disabled here: {}", describe(&e));
        }

        let (first_tx, first_rx) = mpsc::sync_channel(1);
        let state = Arc::new(Mutex::new(VideoState {
            gpu: Some(Gpu { context, staging, scaling }),
            crop: CropInput {
                region_left: region.left,
                region_top: region.top,
                monitor_left: monitor.rect.left,
                monitor_top: monitor.rect.top,
                out_width: plan.source_width,
                out_height: plan.source_height,
            },
            encoded: (plan.pixel_width, plan.pixel_height),
            pool_size: size,
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
        // Frames keep arriving in surfaces of the pool's size after the
        // display changed, so the crop would show the wrong area (or black).
        let content = frame.ContentSize().map_err(capture_error)?;
        if content != self.pool_size {
            log_warn!(
                "the display changed from {}x{} to {}x{} during the recording",
                self.pool_size.Width,
                self.pool_size.Height,
                content.Width,
                content.Height
            );
            return Err(DISPLAY_CHANGED.into());
        }
        let time = frame.SystemRelativeTime().map_err(capture_error)?.Duration;
        let t0 = shared.t0_or_init(time);
        let time = time - t0;
        if time < 0 || !self.throttle.accept(time) {
            return Ok(());
        }
        if self.gpu.is_none() || (self.pending.is_some() && self.encoder_is_behind(shared)) {
            return Ok(());
        }
        let Some(gpu) = self.gpu.as_mut() else { return Ok(()) };
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
        let source = (self.crop.out_width, self.crop.out_height);
        let (width, height) = self.encoded;
        let mut thumbnail = None;
        let wants_thumbnail = self.thumbnail.is_none();
        let buffer = gpu
            .picture(&texture, copy, source, self.encoded, |picture| {
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
        let frame_bytes = self.encoded.0 as usize * self.encoded.1 as usize * 4;
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

impl Gpu {
    /// Puts the visible part of the region (`copy`, from the captured
    /// `frame`) into a new media buffer holding the encoded picture, black
    /// where the frame does not cover the region. `inspect` sees the finished
    /// picture. `source` is the region's size, `encoded` the picture's.
    fn picture(
        &mut self,
        frame: &ID3D11Texture2D,
        copy: Option<CropCopy>,
        source: (u32, u32),
        encoded: (u32, u32),
        inspect: impl FnOnce(&[u8]),
    ) -> windows::core::Result<IMFMediaBuffer> {
        let (source_width, source_height) = source;
        let (width, height) = encoded;
        let len = width as usize * height as usize * 4;
        match &mut self.scaling {
            Scaling::None => {
                if let Some(copy) = copy {
                    copy_region(&self.context, &self.staging, frame, copy);
                }
                read_staging(&self.context, &self.staging, width, height, len, |rows, pitch, picture| {
                    frame::copy_picture(rows, pitch, picture, width, height, copy);
                    inspect(picture);
                })
            }
            Scaling::Gpu(scaler) => {
                let _section = scaler.lock.as_ref().map(DeviceSection::enter);
                scaler.scale(&self.context, frame, copy, source)?;
                // SAFETY: the output and staging textures are both `encoded` BGRA.
                unsafe { self.context.CopyResource(&self.staging, &scaler.output) };
                read_staging(&self.context, &self.staging, width, height, len, |rows, pitch, picture| {
                    frame::copy_picture(rows, pitch, picture, width, height, Some(CropCopy::full(width, height)));
                    inspect(picture);
                })
            }
            Scaling::Cpu { scratch } => {
                if let Some(copy) = copy {
                    copy_region(&self.context, &self.staging, frame, copy);
                }
                scratch.resize(source_width as usize * source_height as usize * 4, 0);
                read_staging(&self.context, &self.staging, source_width, source_height, len, |rows, pitch, picture| {
                    frame::copy_picture(rows, pitch, scratch, source_width, source_height, copy);
                    frame::scale_bgra_into(scratch, source_width, source_height, source_width as usize * 4, picture, width, height);
                    inspect(picture);
                })
            }
        }
    }
}

impl GpuScaler {
    /// Sets up the video processor for `source` -> `encoded` BGRA and checks
    /// that it really works; fails when the GPU (or WARP) cannot do it.
    fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        source: (u32, u32),
        encoded: (u32, u32),
    ) -> windows::core::Result<GpuScaler> {
        let video_device: ID3D11VideoDevice = device.cast()?;
        let video_context: ID3D11VideoContext = context.cast()?;
        let rate = DXGI_RATIONAL { Numerator: VideoEncodingPlan::FRAMES_PER_SECOND, Denominator: 1 };
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: source.0,
            InputHeight: source.1,
            OutputFrameRate: rate,
            OutputWidth: encoded.0,
            OutputHeight: encoded.1,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_QUALITY,
        };
        let missing = || windows::core::Error::from_hresult(E_FAIL);
        // SAFETY: D3D11 video calls with valid descriptions and out-parameters,
        // on objects created here from the same device.
        unsafe {
            let enumerator = video_device.CreateVideoProcessorEnumerator(&content)?;
            let needed = (D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 | D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0) as u32;
            if enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM)? & needed != needed {
                return Err(windows::core::Error::new(DXGI_ERROR_UNSUPPORTED, "the video processor cannot convert BGRA to BGRA"));
            }
            let processor = video_device.CreateVideoProcessor(&enumerator, 0)?;
            let region_bind = (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32;
            let region = create_texture(device, source.0, source.1, D3D11_USAGE_DEFAULT, region_bind, 0)?;
            let output = create_texture(device, encoded.0, encoded.1, D3D11_USAGE_DEFAULT, D3D11_BIND_RENDER_TARGET.0 as u32, 0)?;
            let mut region_target = None;
            device.CreateRenderTargetView(&region, None, Some(&mut region_target))?;
            let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
            };
            let mut region_input = None;
            video_device.CreateVideoProcessorInputView(&region, &enumerator, &input_desc, Some(&mut region_input))?;
            let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
            };
            let mut output_view = None;
            video_device.CreateVideoProcessorOutputView(&output, &enumerator, &output_desc, Some(&mut output_view))?;

            // The whole region in, the whole picture out, full-range RGB on
            // both sides, and no driver "enhancements" on screen content.
            let whole_region = RECT { left: 0, top: 0, right: source.0 as i32, bottom: source.1 as i32 };
            let whole_picture = RECT { left: 0, top: 0, right: encoded.0 as i32, bottom: encoded.1 as i32 };
            // All bits zero: playback usage, RGB range 0-255.
            let full_range_rgb = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
            video_context.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            video_context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&whole_region));
            video_context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&whole_picture));
            video_context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&whole_picture));
            video_context.VideoProcessorSetStreamColorSpace(&processor, 0, &full_range_rgb);
            video_context.VideoProcessorSetOutputColorSpace(&processor, &full_range_rgb);
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);

            let scaler = GpuScaler {
                lock: context.cast::<ID3D11Multithread>().ok(),
                video_context,
                processor,
                region,
                region_target: region_target.ok_or_else(missing)?,
                region_input: region_input.ok_or_else(missing)?,
                output,
                output_view: output_view.ok_or_else(missing)?,
                _enumerator: enumerator,
            };
            // Some drivers list the format but fail the blit: find out now,
            // while the CPU can still take over, not on the first frame.
            context.ClearRenderTargetView(&scaler.region_target, &BLACK);
            scaler.blit()?;
            Ok(scaler)
        }
    }

    /// Copies the visible part of the region into `region` (blacking out the
    /// rest when the frame does not cover it all) and scales it into `output`.
    fn scale(
        &self,
        context: &ID3D11DeviceContext,
        frame: &ID3D11Texture2D,
        copy: Option<CropCopy>,
        source: (u32, u32),
    ) -> windows::core::Result<()> {
        if copy.is_none_or(|c| !c.covers(source.0, source.1)) {
            // SAFETY: a render target view of a texture this scaler owns.
            unsafe { context.ClearRenderTargetView(&self.region_target, &BLACK) };
        }
        if let Some(copy) = copy {
            copy_region(context, &self.region, frame, copy);
        }
        self.blit()
    }

    /// Scales `region` into `output`.
    fn blit(&self) -> windows::core::Result<()> {
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(Some(self.region_input.clone())),
            ..Default::default()
        };
        // SAFETY: one enabled stream with a valid input view; the processor and
        // both views come from the same enumerator and device.
        let result = unsafe {
            self.video_context.VideoProcessorBlt(&self.processor, &self.output_view, 0, std::slice::from_ref(&stream))
        };
        // Releases the reference the stream held.
        drop(ManuallyDrop::into_inner(stream.pInputSurface));
        result
    }
}

/// Holds a device's multithread critical section until dropped. WGC uses the
/// device from its own threads; holding the section keeps those calls from
/// interleaving with a frame's video context work. The section is recursive,
/// so the runtime's own per-call locking inside it is fine.
struct DeviceSection<'a>(&'a ID3D11Multithread);

impl<'a> DeviceSection<'a> {
    fn enter(lock: &'a ID3D11Multithread) -> DeviceSection<'a> {
        // SAFETY: balanced by Leave in Drop, on the same thread.
        unsafe { lock.Enter() };
        DeviceSection(lock)
    }
}

impl Drop for DeviceSection<'_> {
    fn drop(&mut self) {
        // SAFETY: this guard entered the section in `enter`.
        unsafe { self.0.Leave() };
    }
}

/// Copies the visible part of the region (`copy`) from the captured `frame`
/// into `target`, a texture of the region's size.
fn copy_region(context: &ID3D11DeviceContext, target: &ID3D11Texture2D, frame: &ID3D11Texture2D, copy: CropCopy) {
    let source_box = D3D11_BOX {
        left: copy.src_left,
        top: copy.src_top,
        front: 0,
        right: copy.src_right,
        bottom: copy.src_bottom,
        back: 1,
    };
    // SAFETY: the box lies inside the frame texture and the destination
    // offset plus box size fits `target` (see `crop_copy`).
    unsafe { context.CopySubresourceRegion(target, 0, copy.dst_x, copy.dst_y, 0, frame, 0, Some(&source_box)) };
}

/// Maps `staging` (`width` x `height` pixels) and fills a new `len`-byte media
/// buffer from it with `fill(rows, row_pitch, buffer)`.
fn read_staging(
    context: &ID3D11DeviceContext,
    staging: &ID3D11Texture2D,
    width: u32,
    height: u32,
    len: usize,
    fill: impl FnOnce(&[u8], usize, &mut [u8]),
) -> windows::core::Result<IMFMediaBuffer> {
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    // SAFETY: the staging texture was created with CPU read access; it is
    // unmapped below on every path.
    unsafe { context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))? };
    let result = if mapped.pData.is_null() || height == 0 {
        Err(windows::core::Error::from_hresult(E_FAIL))
    } else {
        let pitch = mapped.RowPitch as usize;
        let src_len = pitch * (height as usize - 1) + width as usize * 4;
        // SAFETY: a mapped staging texture of `height` rows holds at least
        // `RowPitch * (height - 1) + width * 4` readable bytes.
        let src = unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, src_len) };
        new_buffer(len, |dst| fill(src, pitch, dst))
    };
    // SAFETY: balances the successful Map above.
    unsafe { context.Unmap(staging, 0) };
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

/// The staging texture and the scaling `plan` needs: none when the region is
/// encoded at its own size, else the GPU video processor, or the CPU when the
/// GPU cannot scale.
fn create_scaling(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    plan: &VideoEncodingPlan,
) -> windows::core::Result<(ID3D11Texture2D, Scaling)> {
    let source = (plan.source_width, plan.source_height);
    let encoded = (plan.pixel_width, plan.pixel_height);
    if !plan.is_scaled() {
        return Ok((create_staging(device, source.0, source.1)?, Scaling::None));
    }
    match GpuScaler::new(device, context, source, encoded) {
        Ok(scaler) => {
            log_info!("scaling the {}x{} region to {}x{} on the GPU", source.0, source.1, encoded.0, encoded.1);
            Ok((create_staging(device, encoded.0, encoded.1)?, Scaling::Gpu(scaler)))
        }
        Err(e) => {
            log_warn!(
                "scaling the {}x{} region to {}x{} on the CPU: the GPU video processor is unavailable ({})",
                source.0,
                source.1,
                encoded.0,
                encoded.1,
                describe(&e)
            );
            Ok((create_staging(device, source.0, source.1)?, Scaling::Cpu { scratch: Vec::new() }))
        }
    }
}

fn create_staging(device: &ID3D11Device, width: u32, height: u32) -> windows::core::Result<ID3D11Texture2D> {
    create_texture(device, width, height, D3D11_USAGE_STAGING, 0, D3D11_CPU_ACCESS_READ.0 as u32)
}

/// A single BGRA texture without mipmaps.
fn create_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    usage: D3D11_USAGE,
    bind_flags: u32,
    cpu_access: u32,
) -> windows::core::Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: usage,
        BindFlags: bind_flags,
        CPUAccessFlags: cpu_access,
        MiscFlags: 0,
    };
    let mut texture = None;
    // SAFETY: `desc` describes a valid texture; no initial data.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture))? };
    texture.ok_or_else(|| windows::core::Error::from_hresult(E_FAIL))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_display_change_reason_is_a_sentence() {
        assert_eq!(crate::messages::sentence(DISPLAY_CHANGED), DISPLAY_CHANGED);
    }
}
