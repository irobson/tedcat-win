//! System audio: WASAPI loopback on the default render endpoint, converted to
//! 48 kHz stereo 16-bit and written to the sink writer's AAC stream.
//!
//! Loopback delivers no packets while nothing plays, so a second client on the
//! same endpoint renders silence for as long as the capture runs; the engine
//! then keeps producing (silent) loopback packets and the audio track has no
//! holes. If the endpoint goes away or the default output changes (headphones
//! plugged in, Bluetooth connected), capture moves to the new default output;
//! the timeline fills the gap with silence.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use super::pcm::{self, Extensible, LinearResampler, PcmFormat, SubFormat, WaveDescription};
use super::session::Shared;
use super::sys::{describe, now_hns, on_mta_thread, MtaScope};
use super::timing::{frame_span, AudioTimeline};
use super::writer::{Writer, AUDIO_RATE};
use crate::{log_info, log_warn};

/// Shared-mode buffer length requested from WASAPI (200 ms).
const BUFFER_HNS: i64 = 2_000_000;
/// Gaps or overlaps smaller than this are not corrected (40 ms).
const TIMELINE_TOLERANCE_HNS: i64 = 400_000;
/// How long `open` waits for the audio thread to initialize.
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);
/// How often to check whether the default output device changed.
const DEFAULT_CHECK_INTERVAL: Duration = Duration::from_secs(1);
/// Delay between attempts to reopen a lost device.
const REOPEN_INTERVAL: Duration = Duration::from_millis(500);
/// An audio-only recording gives up (and tells the UI) after this long without a device.
const AUDIO_ONLY_REOPEN_LIMIT: Duration = Duration::from_secs(3);
/// Silence is written in chunks of at most one second.
const SILENCE_CHUNK_FRAMES: u64 = AUDIO_RATE as u64;

/// Why audio capture could not start.
#[derive(Clone, Debug)]
pub(super) enum OpenError {
    /// There is no default audio output device.
    NoDevice(String),
    /// The device exists but capture could not be set up.
    Failed(String),
}

impl OpenError {
    pub(super) fn detail(&self) -> &str {
        match self {
            OpenError::NoDevice(detail) | OpenError::Failed(detail) => detail,
        }
    }
}

/// True when a default audio render (output) device exists.
pub(super) fn output_available() -> bool {
    on_mta_thread(|| {
        // SAFETY: COM is initialized on this probe thread by `on_mta_thread`.
        let enumerator: windows::core::Result<IMMDeviceEnumerator> =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) };
        // SAFETY: as above.
        enumerator.is_ok_and(|enumerator| unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }.is_ok())
    })
    .unwrap_or(false)
}

/// The audio capture thread, seen from the recorder thread.
pub(super) struct AudioCapture {
    thread: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    go: Option<SyncSender<Arc<Shared>>>,
}

impl AudioCapture {
    /// Opens loopback capture of the default output on a new thread. Returns
    /// once the device is initialized; packets are only written after [`begin`](Self::begin).
    pub(super) fn open() -> Result<AudioCapture, OpenError> {
        let stop = Arc::new(AtomicBool::new(false));
        let (init_tx, init_rx) = mpsc::sync_channel(1);
        let (go_tx, go_rx) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("TedCat audio".into())
            .spawn({
                let stop = stop.clone();
                move || audio_thread(init_tx, go_rx, stop)
            })
            .map_err(|e| OpenError::Failed(format!("could not start the audio thread: {e}")))?;
        let mut capture = AudioCapture { thread: Some(thread), stop, go: Some(go_tx) };
        match init_rx.recv_timeout(OPEN_TIMEOUT) {
            Ok(Ok(())) => Ok(capture),
            // Dropping `capture` joins the thread, which is already exiting.
            Ok(Err(e)) => Err(e),
            Err(_) => {
                // Do not wait for a stuck driver: detach, the thread exits once it returns.
                capture.thread = None;
                Err(OpenError::Failed("the audio device did not respond".into()))
            }
        }
    }

    /// Starts writing captured audio through `shared`.
    pub(super) fn begin(&mut self, shared: Arc<Shared>) {
        if let Some(go) = self.go.take() {
            let _ = go.send(shared);
        }
    }
}

impl Drop for AudioCapture {
    /// Stops capture, writing what was captured up to now, and joins the thread.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.go = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn audio_thread(init_tx: SyncSender<Result<(), OpenError>>, go_rx: Receiver<Arc<Shared>>, stop: Arc<AtomicBool>) {
    let scope = match MtaScope::enter(true) {
        Ok(scope) => scope,
        Err(e) => {
            let _ = init_tx.send(Err(OpenError::Failed(e)));
            return;
        }
    };
    capture_loop(init_tx, go_rx, &stop);
    drop(scope);
}

fn capture_loop(init_tx: SyncSender<Result<(), OpenError>>, go_rx: Receiver<Arc<Shared>>, stop: &AtomicBool) {
    // SAFETY: COM is initialized on this thread (MtaScope).
    let enumerator: IMMDeviceEnumerator = match unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) } {
        Ok(enumerator) => enumerator,
        Err(e) => {
            let _ = init_tx.send(Err(OpenError::Failed(describe(&e))));
            return;
        }
    };
    let mut loopback = match Loopback::open(&enumerator) {
        Ok(loopback) => loopback,
        Err(e) => {
            let _ = init_tx.send(Err(e));
            return;
        }
    };
    if init_tx.send(Ok(())).is_err() {
        return;
    }
    drop(init_tx);
    let Ok(shared) = go_rx.recv() else { return };
    let mut timeline = AudioTimeline::new(AUDIO_RATE, TIMELINE_TOLERANCE_HNS);
    loop {
        match loopback.pump(&enumerator, &shared, &mut timeline, stop) {
            PumpEnd::Stopped | PumpEnd::WriterFailed => return,
            PumpEnd::DefaultChanged => log_info!("default audio output changed; following it"),
            PumpEnd::Lost(reason) => log_warn!("audio capture lost ({reason}); reopening"),
        }
        drop(loopback);
        match reopen(&enumerator, &shared, stop) {
            Some(next) => loopback = next,
            None => return,
        }
    }
}

/// Reopens the default output until it works, stop is requested, or (audio
/// only) the device stays missing for too long.
fn reopen(enumerator: &IMMDeviceEnumerator, shared: &Shared, stop: &AtomicBool) -> Option<Loopback> {
    let started = Instant::now();
    let mut logged = false;
    loop {
        if stop.load(Ordering::Acquire) {
            return None;
        }
        match Loopback::open(enumerator) {
            Ok(loopback) => {
                log_info!("audio capture resumed on the default output ({})", loopback.describe_format());
                return Some(loopback);
            }
            Err(e) => {
                if !logged {
                    log_warn!("cannot reopen audio capture yet: {}", e.detail());
                    logged = true;
                }
                if shared.audio_only && started.elapsed() >= AUDIO_ONLY_REOPEN_LIMIT {
                    shared.interrupt("The audio output device was disconnected.");
                    return None;
                }
            }
        }
        let wait_until = Instant::now() + REOPEN_INTERVAL;
        while Instant::now() < wait_until && !stop.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

enum PumpEnd {
    Stopped,
    WriterFailed,
    DefaultChanged,
    Lost(String),
}

enum DrainError {
    Device(windows::core::Error),
    Writer(String),
}

/// An auto-reset event handle, closed on drop.
struct Event(HANDLE);

impl Event {
    fn new() -> windows::core::Result<Event> {
        // SAFETY: unnamed auto-reset event with default security.
        unsafe { CreateEventW(None, false, false, PCWSTR::null()) }.map(Event)
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by this value.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// The mix format returned by `GetMixFormat`, freed on drop.
struct MixFormat(*mut WAVEFORMATEX);

impl MixFormat {
    fn of(client: &IAudioClient) -> windows::core::Result<MixFormat> {
        // SAFETY: returns a CoTaskMemAlloc'ed WAVEFORMATEX(TENSIBLE), owned by the result.
        let format = unsafe { client.GetMixFormat()? };
        if format.is_null() {
            return Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_POINTER));
        }
        Ok(MixFormat(format))
    }

    fn as_ptr(&self) -> *const WAVEFORMATEX {
        self.0
    }

    fn describe(&self) -> WaveDescription {
        // SAFETY: GetMixFormat returned a valid WAVEFORMATEX, followed by the
        // WAVEFORMATEXTENSIBLE fields when the tag says so and cbSize allows
        // it. The structs are packed, so they are read unaligned, by value.
        let base = unsafe { std::ptr::read_unaligned(self.0) };
        let extensible_size = (std::mem::size_of::<WAVEFORMATEXTENSIBLE>() - std::mem::size_of::<WAVEFORMATEX>()) as u16;
        let format_tag = base.wFormatTag;
        let extensible = if format_tag as u32 == WAVE_FORMAT_EXTENSIBLE && base.cbSize >= extensible_size {
            let ext = unsafe { std::ptr::read_unaligned(self.0 as *const WAVEFORMATEXTENSIBLE) };
            let sub_format = ext.SubFormat;
            let samples = ext.Samples;
            // SAFETY: every member of the union is a u16.
            let valid_bits = unsafe { samples.wValidBitsPerSample };
            Some(Extensible {
                valid_bits,
                channel_mask: ext.dwChannelMask,
                sub_format: if sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                    SubFormat::Float
                } else if sub_format == KSDATAFORMAT_SUBTYPE_PCM {
                    SubFormat::Pcm
                } else {
                    SubFormat::Other
                },
            })
        } else {
            None
        };
        WaveDescription {
            format_tag,
            channels: base.nChannels,
            rate: base.nSamplesPerSec,
            bits: base.wBitsPerSample,
            block_align: base.nBlockAlign,
            extensible,
        }
    }
}

impl Drop for MixFormat {
    fn drop(&mut self) {
        // SAFETY: allocated by GetMixFormat with CoTaskMemAlloc.
        unsafe { CoTaskMemFree(Some(self.0 as *const _)) };
    }
}

/// A render stream that plays silence so loopback keeps delivering packets.
struct SilentRender {
    client: IAudioClient,
    render: IAudioRenderClient,
    frames: u32,
}

impl SilentRender {
    fn start(device: &IMMDevice, format: &MixFormat) -> windows::core::Result<SilentRender> {
        // SAFETY: standard shared-mode render setup with the device's own mix format.
        unsafe {
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, BUFFER_HNS, 0, format.as_ptr(), None)?;
            let frames = client.GetBufferSize()?;
            let render: IAudioRenderClient = client.GetService()?;
            let silent = SilentRender { client, render, frames };
            silent.top_up()?;
            silent.client.Start()?;
            Ok(silent)
        }
    }

    /// Fills the free part of the render buffer with silence.
    fn top_up(&self) -> windows::core::Result<()> {
        // SAFETY: requests at most the free space; the buffer is released
        // immediately with the SILENT flag, so its contents are never read.
        unsafe {
            let padding = self.client.GetCurrentPadding()?;
            let free = self.frames.saturating_sub(padding);
            if free > 0 {
                self.render.GetBuffer(free)?;
                self.render.ReleaseBuffer(free, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
            }
        }
        Ok(())
    }
}

impl Drop for SilentRender {
    fn drop(&mut self) {
        // SAFETY: stopping a started client; errors are irrelevant here.
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

/// One loopback capture session on one endpoint.
struct Loopback {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    keep_alive: Option<SilentRender>,
    event: Event,
    event_driven: bool,
    device_id: Option<String>,
    format: PcmFormat,
    weights: Vec<(f32, f32)>,
    resampler: LinearResampler,
    stereo: Vec<f32>,
    resampled: Vec<f32>,
    pcm: Vec<i16>,
    silence: Vec<i16>,
}

impl Loopback {
    fn open(enumerator: &IMMDeviceEnumerator) -> Result<Loopback, OpenError> {
        let failed = |e: windows::core::Error| OpenError::Failed(describe(&e));
        // SAFETY: COM calls on valid interfaces from this MTA thread.
        let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
            .map_err(|e| OpenError::NoDevice(describe(&e)))?;
        let device_id = endpoint_id(&device);
        // SAFETY: as above.
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(failed)?;
        let mix = MixFormat::of(&client).map_err(failed)?;
        let format = pcm::parse_format(&mix.describe()).map_err(OpenError::Failed)?;
        let event = Event::new().map_err(failed)?;

        let (client, event_driven) = match initialize_loopback(&client, &mix, true) {
            Ok(()) => (client, true),
            Err(e) => {
                log_warn!("event-driven loopback refused ({}); polling instead", describe(&e));
                // A client whose Initialize failed cannot be initialized again.
                // SAFETY: as above.
                let retry: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(failed)?;
                initialize_loopback(&retry, &mix, false).map_err(failed)?;
                (retry, false)
            }
        };
        // SAFETY: the event outlives the client (both are fields of Loopback,
        // and the client is stopped before the event is closed).
        unsafe {
            if event_driven {
                client.SetEventHandle(event.0).map_err(failed)?;
            }
        }
        // SAFETY: as above.
        let capture: IAudioCaptureClient = unsafe { client.GetService() }.map_err(failed)?;
        let keep_alive = match SilentRender::start(&device, &mix) {
            Ok(silent) => Some(silent),
            Err(e) => {
                log_warn!("silent keep-alive stream unavailable ({}); loopback may pause in silence", describe(&e));
                None
            }
        };
        // SAFETY: as above.
        unsafe { client.Start() }.map_err(failed)?;
        let loopback = Loopback {
            client,
            capture,
            keep_alive,
            event,
            event_driven,
            device_id,
            format,
            weights: pcm::downmix_weights(format.channels, format.channel_mask),
            resampler: LinearResampler::new(format.rate, AUDIO_RATE),
            stereo: Vec::new(),
            resampled: Vec::new(),
            pcm: Vec::new(),
            silence: Vec::new(),
        };
        log_info!("loopback capture open: {}", loopback.describe_format());
        Ok(loopback)
    }

    fn describe_format(&self) -> String {
        format!(
            "{} Hz, {} ch, {:?}, mask {:#x}, {}",
            self.format.rate,
            self.format.channels,
            self.format.kind,
            self.format.channel_mask,
            if self.event_driven { "event driven" } else { "polled" }
        )
    }

    /// Captures until stop is requested or the session ends.
    fn pump(&mut self, enumerator: &IMMDeviceEnumerator, shared: &Shared, timeline: &mut AudioTimeline, stop: &AtomicBool) -> PumpEnd {
        let wait_ms = if self.event_driven { 50 } else { 10 };
        let mut last_default_check = Instant::now();
        loop {
            // SAFETY: waiting on an event handle this session owns.
            let _ = unsafe { WaitForSingleObject(self.event.0, wait_ms) };
            if let Some(silent) = self.keep_alive.as_ref() {
                if let Err(e) = silent.top_up() {
                    log_warn!("silent keep-alive stream failed: {}", describe(&e));
                    self.keep_alive = None;
                }
            }
            match self.drain(shared, timeline) {
                Ok(()) => {}
                Err(DrainError::Device(e)) => return PumpEnd::Lost(describe(&e)),
                Err(DrainError::Writer(reason)) => {
                    shared.interrupt(&format!("Could not write the recording: {reason}"));
                    return PumpEnd::WriterFailed;
                }
            }
            if stop.load(Ordering::Acquire) {
                return PumpEnd::Stopped;
            }
            if last_default_check.elapsed() >= DEFAULT_CHECK_INTERVAL {
                last_default_check = Instant::now();
                if self.default_changed(enumerator) {
                    return PumpEnd::DefaultChanged;
                }
            }
        }
    }

    fn default_changed(&self, enumerator: &IMMDeviceEnumerator) -> bool {
        // SAFETY: COM call from this MTA thread.
        let Ok(device) = (unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }) else {
            return false; // No default right now: capture errors will tell if ours is gone.
        };
        match (endpoint_id(&device), self.device_id.as_ref()) {
            (Some(current), Some(ours)) => &current != ours,
            _ => false,
        }
    }

    /// Reads every packet available now.
    fn drain(&mut self, shared: &Shared, timeline: &mut AudioTimeline) -> Result<(), DrainError> {
        loop {
            // SAFETY: COM calls on this session's capture client.
            let next = unsafe { self.capture.GetNextPacketSize() }.map_err(DrainError::Device)?;
            if next == 0 {
                return Ok(());
            }
            let mut data = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            let mut qpc = 0u64;
            // SAFETY: valid out-parameters; the buffer is released below.
            unsafe { self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc)) }
                .map_err(DrainError::Device)?;
            self.decode(data, frames as usize, flags);
            // SAFETY: releases exactly the packet obtained above.
            unsafe { self.capture.ReleaseBuffer(frames) }.map_err(DrainError::Device)?;
            self.emit(shared, timeline, frames as usize, flags, qpc).map_err(DrainError::Writer)?;
        }
    }

    /// Converts the packet to interleaved stereo floats in `self.stereo`.
    fn decode(&mut self, data: *const u8, frames: usize, flags: u32) {
        self.stereo.clear();
        if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
            self.stereo.resize(frames * 2, 0.0);
            return;
        }
        let len = frames * self.format.block_align as usize;
        // SAFETY: GetBuffer returned `frames` frames of `block_align` bytes,
        // valid until ReleaseBuffer, which is called after this copy.
        let bytes = unsafe { std::slice::from_raw_parts(data, len) };
        pcm::decode_to_stereo(bytes, &self.format, &self.weights, &mut self.stereo);
    }

    /// Places the decoded packet on the timeline and writes it.
    fn emit(&mut self, shared: &Shared, timeline: &mut AudioTimeline, frames: usize, flags: u32, qpc: u64) -> Result<(), String> {
        if frames == 0 {
            return Ok(());
        }
        let timestamp_ok = flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 == 0 && qpc != 0;
        let packet_time = if timestamp_ok { qpc as i64 } else { now_hns() };
        let t0 = match shared.t0() {
            Some(t0) => t0,
            // Audio only: the first packet starts the clock.
            None if shared.audio_only => shared.t0_or_init(packet_time),
            // Region: wait for the first video frame; earlier audio is dropped.
            None => return Ok(()),
        };
        let time = timestamp_ok.then_some(packet_time - t0);
        let Some(placement) = timeline.place(time, frames, self.format.rate) else { return Ok(()) };
        let skip = placement.skip_input_frames.min(frames) * 2;
        self.resampled.clear();
        self.resampler.process(&self.stereo[skip.min(self.stereo.len())..], &mut self.resampled);
        pcm::f32_to_i16(&self.resampled, &mut self.pcm);

        let mut writer = shared.writer();
        if placement.silence_frames > 0 {
            write_silence(&mut writer, timeline, placement.silence_frames, &mut self.silence)?;
        }
        let count = (self.pcm.len() / 2) as u64;
        if count > 0 {
            let (time, duration) = frame_span(timeline.position(), count, AUDIO_RATE);
            writer.write_audio(&self.pcm, time, duration)?;
            timeline.advance(count);
        }
        Ok(())
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        // SAFETY: stopping a started client before its event handle is closed.
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

fn initialize_loopback(client: &IAudioClient, mix: &MixFormat, event_driven: bool) -> windows::core::Result<()> {
    let flags = AUDCLNT_STREAMFLAGS_LOOPBACK | if event_driven { AUDCLNT_STREAMFLAGS_EVENTCALLBACK } else { 0 };
    // SAFETY: shared-mode loopback with the endpoint's own mix format.
    unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, BUFFER_HNS, 0, mix.as_ptr(), None) }
}

fn write_silence(writer: &mut Writer, timeline: &mut AudioTimeline, frames: u64, zeros: &mut Vec<i16>) -> Result<(), String> {
    let chunk = frames.min(SILENCE_CHUNK_FRAMES);
    zeros.resize(chunk as usize * 2, 0);
    let mut remaining = frames;
    while remaining > 0 {
        let count = remaining.min(SILENCE_CHUNK_FRAMES);
        let (time, duration) = frame_span(timeline.position(), count, AUDIO_RATE);
        writer.write_audio(&zeros[..count as usize * 2], time, duration)?;
        timeline.advance(count);
        remaining -= count;
    }
    Ok(())
}

/// The endpoint ID string, used to notice default device changes.
fn endpoint_id(device: &IMMDevice) -> Option<String> {
    // SAFETY: GetId returns a CoTaskMemAlloc'ed string, freed right after copying.
    unsafe {
        let id = device.GetId().ok()?;
        let text = id.to_string().ok();
        CoTaskMemFree(Some(id.0 as *const _));
        text
    }
}
