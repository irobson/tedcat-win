//! The recorder thread: sets up the pipeline, waits for the stop command and
//! finalizes the file. Everything that touches COM, WinRT or Media Foundation
//! is created here or on the audio thread, both in the multithreaded apartment.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tedcat_core::{Codec, VideoEncodingPlan};

use super::audio::{AudioCapture, OpenError};
use super::sys::{delete_output, media_foundation_available, now_hns, MtaScope};
use super::timing;
use super::video::{VideoCapture, VideoOutcome};
use super::writer::{copy_buffer, VideoFormat, Writer, AUDIO_RATE};
use super::{RecorderOptions, Target};
use crate::messages::{RecorderEvent, RecordingResult, UiHandle, WM_APP_RECORDER};
use crate::{log_error, log_info, log_warn};

/// How long an audio-only start waits for the first packet before going on anyway.
const FIRST_AUDIO_TIMEOUT: Duration = Duration::from_secs(2);

/// Caps the encoded video size (`WIDTHxHEIGHT`, in pixels) below the codec's
/// own limit. Only the smoke test sets it, to exercise the downscaling path
/// on a display too small to need it.
const TEST_MAX_VIDEO_SIZE: &str = "TEDCAT_TEST_MAX_VIDEO_SIZE";

/// Commands from the `Recorder` handle.
pub(super) enum Command {
    /// Finalize the file and return the result.
    Stop,
    /// `start` gave up waiting: tear down and delete the file.
    Abort,
}

/// State shared by the recorder, video and audio threads.
pub(super) struct Shared {
    writer: Mutex<Writer>,
    /// QPC time (100 ns) of the recording start; `i64::MIN` until known.
    t0: AtomicI64,
    /// Set once `Interrupted` was posted, or when stopping (no more notices).
    quiet: AtomicBool,
    events: UiHandle,
    /// Audio-only recordings start their clock with the first audio packet.
    pub audio_only: bool,
}

impl Shared {
    fn new(writer: Writer, events: UiHandle, audio_only: bool) -> Self {
        Self {
            writer: Mutex::new(writer),
            t0: AtomicI64::new(i64::MIN),
            quiet: AtomicBool::new(false),
            events,
            audio_only,
        }
    }

    /// Locks the writer (a panic elsewhere never makes it unusable).
    pub(super) fn writer(&self) -> MutexGuard<'_, Writer> {
        self.writer.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The recording start, once the first frame (or audio packet) set it.
    pub(super) fn t0(&self) -> Option<i64> {
        let t0 = self.t0.load(Ordering::Acquire);
        (t0 != i64::MIN).then_some(t0)
    }

    /// Sets the recording start to `candidate` unless it is already set;
    /// returns the start in effect.
    pub(super) fn t0_or_init(&self, candidate: i64) -> i64 {
        match self.t0.compare_exchange(i64::MIN, candidate, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => candidate,
            Err(existing) => existing,
        }
    }

    /// Tells the UI, once, that capture stopped on its own. The UI then calls
    /// `Recorder::stop`, which still finalizes what was captured.
    pub(super) fn interrupt(&self, reason: &str) {
        if self.quiet.swap(true, Ordering::AcqRel) {
            log_warn!("recorder: {reason}");
            return;
        }
        log_error!("recording interrupted: {reason}");
        if !self.events.post(WM_APP_RECORDER, RecorderEvent::Interrupted(reason.to_string())) {
            log_warn!("could not notify the UI that the recording was interrupted");
        }
    }

    fn silence_notices(&self) {
        self.quiet.store(true, Ordering::Release);
    }
}

/// Body of the recorder thread. Reports start-up on `ready`, then waits for
/// a command and returns the result of the recording.
pub(super) fn run(
    target: Target,
    options: RecorderOptions,
    events: UiHandle,
    ready: SyncSender<Result<(), String>>,
    commands: Receiver<Command>,
) -> Result<RecordingResult, String> {
    // Before anything calls into the delay-loaded Media Foundation DLLs, and
    // reported as is: the message already says what to do.
    if let Err(e) = media_foundation_available() {
        let _ = ready.send(Err(e.clone()));
        return Err(e);
    }
    let scope = match MtaScope::enter(true) {
        Ok(scope) => scope,
        Err(e) => {
            log_error!("recorder thread setup failed: {e}");
            let message = format!("Could not start recording: {e}");
            let _ = ready.send(Err(message.clone()));
            return Err(message);
        }
    };
    let result = run_in_apartment(target, &options, events, ready, commands);
    drop(scope);
    result
}

fn run_in_apartment(
    target: Target,
    options: &RecorderOptions,
    events: UiHandle,
    ready: SyncSender<Result<(), String>>,
    commands: Receiver<Command>,
) -> Result<RecordingResult, String> {
    let recording = match Recording::start(target, options, events) {
        Ok(recording) => recording,
        Err(e) => {
            log_error!("recording did not start: {e}");
            let _ = ready.send(Err(e.clone()));
            return Err(e);
        }
    };
    let acknowledged = ready.send(Ok(())).is_ok();
    drop(ready);
    // A dropped `Recorder` (no command at all) still finalizes, so the file is never lost.
    let keep = acknowledged && !matches!(commands.recv(), Ok(Command::Abort));
    if keep {
        recording.finish()
    } else {
        log_warn!("recording abandoned before it was reported as started");
        recording.discard();
        Err("The recording was cancelled.".into())
    }
}

/// A running recording on the recorder thread.
struct Recording {
    shared: Arc<Shared>,
    video: Option<VideoCapture>,
    audio: Option<AudioCapture>,
    path: PathBuf,
    audio_only: bool,
}

impl Recording {
    fn start(target: Target, options: &RecorderOptions, events: UiHandle) -> Result<Recording, String> {
        let path = options.output.clone();
        if let Some(folder) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(folder) {
                log_warn!("could not create {}: {e}", folder.display());
            }
        }
        match target {
            Target::Region { rect, monitor } => {
                let plan = VideoEncodingPlan::new(rect.to_logical(monitor.scale).size(), monitor.scale, Codec::H264)
                    .ok_or_else(|| String::from("The selected area is too small to record."))?;
                let plan = apply_test_size_limit(plan);
                log_info!(
                    "starting region recording {rect:?} on monitor {:?} (scale {}): {} at {} bps -> {}",
                    monitor.rect,
                    monitor.scale,
                    describe_size(&plan),
                    plan.average_bitrate,
                    path.display()
                );
                let mut audio = if options.include_system_audio {
                    match AudioCapture::open() {
                        Ok(audio) => Some(audio),
                        Err(e) => {
                            log_warn!("recording without system audio: {}", e.detail());
                            None
                        }
                    }
                } else {
                    None
                };
                let (writer, plan) = create_video_writer(&path, plan, audio.is_some()).map_err(|e| {
                    drop(audio.take());
                    delete_output(&path);
                    format!("Could not create the video file: {e}")
                })?;
                let shared = Arc::new(Shared::new(writer, events, false));
                if let Some(audio) = audio.as_mut() {
                    audio.begin(shared.clone());
                }
                let video = match VideoCapture::start(&monitor, rect, &plan, options.show_cursor, shared.clone()) {
                    Ok(video) => video,
                    Err(e) => {
                        drop(audio);
                        shared.writer().abandon();
                        delete_output(&path);
                        return Err(e);
                    }
                };
                Ok(Recording { shared, video: Some(video), audio, path, audio_only: false })
            }
            Target::AudioOnly => {
                log_info!("starting audio-only recording -> {}", path.display());
                let mut audio = AudioCapture::open().map_err(|e| match e {
                    OpenError::NoDevice(_) => String::from("No audio output device is available."),
                    OpenError::Failed(detail) => format!("Could not capture the computer's audio: {detail}"),
                })?;
                let writer = Writer::create(&path, None, true).map_err(|e| {
                    delete_output(&path);
                    format!("Could not create the audio file: {e}")
                })?;
                let shared = Arc::new(Shared::new(writer, events, true));
                audio.begin(shared.clone());
                let started = Instant::now();
                while shared.t0().is_none() && started.elapsed() < FIRST_AUDIO_TIMEOUT {
                    std::thread::sleep(Duration::from_millis(10));
                }
                if shared.t0().is_none() {
                    log_warn!("no audio packet within {FIRST_AUDIO_TIMEOUT:?}; recording anyway");
                }
                Ok(Recording { shared, video: None, audio: Some(audio), path, audio_only: true })
            }
        }
    }

    /// Stops capture, completes the file and describes it.
    fn finish(mut self) -> Result<RecordingResult, String> {
        // The moment the user stopped; the video is extended up to it.
        let stop_time = self.shared.t0().map(|t0| now_hns() - t0);
        self.shared.silence_notices();
        let outcome = self.video.take().map(VideoCapture::stop);
        drop(self.audio.take());
        let thumbnail = outcome.as_ref().and_then(|o| o.thumbnail.clone());

        let mut writer = self.shared.writer();
        if let (Some(outcome), Some(stop_time)) = (outcome, stop_time) {
            write_video_tail(&mut writer, outcome, stop_time);
        }
        let has_audio = writer.has_audio_stream();
        if has_audio && writer.audio_samples() == 0 && writer.video_samples() > 0 {
            // An audio track without samples can make the MP4 unplayable.
            let frames = AUDIO_RATE as u64 / 10;
            let (_, duration) = timing::frame_span(0, frames, AUDIO_RATE);
            if let Err(e) = writer.write_audio(&vec![0; frames as usize * 2], 0, duration) {
                log_warn!("could not write filler audio: {e}");
            }
        }
        let (video_samples, audio_samples, end_time) = (writer.video_samples(), writer.audio_samples(), writer.end_time());
        if video_samples + audio_samples == 0 {
            writer.abandon();
            drop(writer);
            delete_output(&self.path);
            return Err("Nothing was recorded.".into());
        }
        let finalized = writer.finalize();
        drop(writer);
        if let Err(e) = finalized {
            log_error!("finalizing {} failed: {e}", self.path.display());
            delete_output(&self.path);
            return Err(format!("Could not finish the recording: {e}"));
        }
        let bytes = file_size(&self.path);
        let duration_secs = end_time as f64 / timing::HNS_PER_SECOND as f64;
        log_info!(
            "recording saved: {} ({duration_secs:.1} s, {bytes} bytes, {video_samples} video / {audio_samples} audio samples)",
            self.path.display()
        );
        Ok(RecordingResult {
            path: self.path.clone(),
            duration_secs,
            bytes,
            audio_only: self.audio_only,
            has_audio,
            thumbnail: if self.audio_only { None } else { thumbnail },
        })
    }

    /// Stops capture and deletes the file.
    fn discard(mut self) {
        self.shared.silence_notices();
        drop(self.video.take().map(VideoCapture::stop));
        drop(self.audio.take());
        self.shared.writer().abandon();
        delete_output(&self.path);
    }
}

/// The sink writer's video stream for `plan`.
fn video_format(plan: &VideoEncodingPlan) -> VideoFormat {
    VideoFormat {
        width: plan.pixel_width,
        height: plan.pixel_height,
        bitrate: plan.average_bitrate,
        fps: VideoEncodingPlan::FRAMES_PER_SECOND,
    }
}

/// Creates the sink writer for `plan`. When no encoder configuration accepts
/// it and the plan has a landscape fallback (a tall portrait picture, which
/// some encoders measure against the landscape limit), tries again with that.
/// Returns the plan the writer was created for.
fn create_video_writer(path: &Path, plan: VideoEncodingPlan, audio: bool) -> Result<(Writer, VideoEncodingPlan), String> {
    let error = match Writer::create(path, Some(video_format(&plan)), audio) {
        Ok(writer) => return Ok((writer, plan)),
        Err(error) => error,
    };
    let Some(fallback) = plan.landscape_fallback() else { return Err(error) };
    log_warn!(
        "no encoder accepted {}x{} ({error}); trying {}x{}",
        plan.pixel_width,
        plan.pixel_height,
        fallback.pixel_width,
        fallback.pixel_height
    );
    let writer = Writer::create(path, Some(video_format(&fallback)), audio)?;
    Ok((writer, fallback))
}

/// "480x320", or "5120x1440 encoded as 4096x1152" when the plan scales down.
fn describe_size(plan: &VideoEncodingPlan) -> String {
    if plan.is_scaled() {
        format!(
            "{}x{} encoded as {}x{}",
            plan.source_width, plan.source_height, plan.pixel_width, plan.pixel_height
        )
    } else {
        format!("{}x{}", plan.pixel_width, plan.pixel_height)
    }
}

/// `plan`, further limited by `TEST_MAX_VIDEO_SIZE` when that is set.
fn apply_test_size_limit(plan: VideoEncodingPlan) -> VideoEncodingPlan {
    let Some(value) = std::env::var_os(TEST_MAX_VIDEO_SIZE) else { return plan };
    match value.to_str().and_then(parse_size) {
        Some((width, height)) => {
            log_warn!("{TEST_MAX_VIDEO_SIZE} is set: encoding at most {width}x{height}");
            plan.fit_within(width, height)
        }
        None => {
            log_warn!("ignoring {TEST_MAX_VIDEO_SIZE}={value:?}: expected WIDTHxHEIGHT");
            plan
        }
    }
}

/// Parses `WIDTHxHEIGHT` (positive integers, `x` or `X`, spaces allowed).
fn parse_size(text: &str) -> Option<(u32, u32)> {
    let (width, height) = text.split_once(['x', 'X'])?;
    let width: u32 = width.trim().parse().ok()?;
    let height: u32 = height.trim().parse().ok()?;
    (width > 0 && height > 0).then_some((width, height))
}

/// Writes the held-back last frame up to the stop time, repeating it at the
/// stop time if it is old (capture only delivers frames when pixels change).
fn write_video_tail(writer: &mut Writer, outcome: VideoOutcome, stop_time: i64) {
    let Some(last) = outcome.last else { return };
    let tail = timing::tail(last.time, stop_time, outcome.interval);
    if let Err(e) = writer.write_video(&last.buffer, last.time, tail.last_duration) {
        log_warn!("could not write the last video frame: {e}");
        return;
    }
    if let Some(at) = tail.repeat_at {
        match copy_buffer(&last.buffer) {
            Ok(copy) => {
                if let Err(e) = writer.write_video(&copy, at, outcome.interval) {
                    log_warn!("could not repeat the last video frame: {e}");
                }
            }
            Err(e) => log_warn!("could not copy the last video frame: {}", super::sys::describe(&e)),
        }
    }
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_size_limits_are_parsed() {
        assert_eq!(parse_size("240x240"), Some((240, 240)));
        assert_eq!(parse_size(" 1280 X 720 "), Some((1280, 720)));
        for bad in ["", "240", "x240", "240x", "0x240", "240x0", "-1x240", "240x240x2", "ax b"] {
            assert_eq!(parse_size(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn scaled_plans_say_both_sizes() {
        let plan = VideoEncodingPlan::new(tedcat_core::Size::new(5120.0, 1440.0), 1.0, Codec::H264).unwrap();
        assert_eq!(describe_size(&plan), "5120x1440 encoded as 4096x1152");
        let plan = VideoEncodingPlan::new(tedcat_core::Size::new(480.0, 320.0), 1.0, Codec::H264).unwrap();
        assert_eq!(describe_size(&plan), "480x320");
    }
}
