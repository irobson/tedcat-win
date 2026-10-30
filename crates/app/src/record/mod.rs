//! Screen and audio recording.
//!
//! CONTRACT (owned by the recording agent; keep the public API unchanged):
//!
//! Region recordings: Windows.Graphics.Capture on the region's monitor
//! (`IGraphicsCaptureItemInterop::CreateForMonitor`, free-threaded frame pool,
//! BGRA8, cursor captured, capture border disabled when the OS allows it),
//! cropped to the region with `CopySubresourceRegion` into a staging texture,
//! throttled to 30 fps, encoded by a Media Foundation sink writer to MP4:
//! H.264 at the size and bitrate from `tedcat_core::VideoEncodingPlan`
//! (`Codec::H264`, scale = monitor.scale), hardware transforms enabled.
//! TedCat's own windows are excluded from capture by display affinity.
//!
//! System audio: WASAPI loopback on the default render endpoint (shared mode,
//! event driven), converted to 16-bit PCM and encoded as AAC 48 kHz stereo
//! 128 kbps in the same file. Loopback delivers no packets while nothing plays,
//! so keep a silent render stream open on the same endpoint for the duration,
//! which makes loopback produce continuous (silent) packets.
//!
//! Timing: all timestamps come from QPC (frame `SystemRelativeTime`, audio
//! `GetBuffer` QPC position) relative to the first video frame (region) or the
//! first audio packet (audio only); earlier audio is dropped. ScreenCaptureKit
//! and WGC only deliver frames when pixels change, so on stop re-send the last
//! frame stamped "now" so the video lasts as long as the recording.
//!
//! Audio only: no video stream, AAC in an `.m4a` (MPEG-4) container.
//! If there is no audio output device: region recordings continue without
//! audio (`has_audio = false`); audio-only recordings fail with a clear message.
//!
//! Robustness: if capture or writing fails mid-recording, post
//! `RecorderEvent::Interrupted(reason)` to `events` once; `stop` must still
//! finalize what was captured. Never leave an empty or unplayable file: delete
//! the output if nothing was captured or finalizing failed.
//! `thumbnail`: the first complete video frame of the region, scaled to fit
//! 184×128, BGRA top-down.

mod audio;
mod frame;
mod pcm;
mod session;
mod sys;
mod timing;
mod video;
mod writer;

use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::geom::{MonitorInfo, RectPx};
use crate::messages::{RecordingResult, UiHandle};
use crate::{log_error, log_warn};
use session::Command;

/// How long `start` waits for the pipeline before giving up.
const START_TIMEOUT: Duration = Duration::from_secs(10);

/// What to record.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Target {
    /// A region (physical pixels, virtual-screen coordinates) on `monitor`.
    Region { rect: RectPx, monitor: MonitorInfo },
    /// System audio only.
    AudioOnly,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RecorderOptions {
    /// Ignored for audio-only recordings, which always capture system audio.
    pub include_system_audio: bool,
    pub show_cursor: bool,
    /// Full path of the file to create (`.mp4` for regions, `.m4a` for audio only).
    pub output: PathBuf,
}

/// A running recording. Must be `Send`: the UI thread moves it to a worker
/// thread to call the blocking `stop`.
///
/// The capture and encoding objects live on threads the recorder owns (COM
/// multithreaded apartment, Media Foundation started there); this handle only
/// holds a command channel and the recorder thread's join handle.
pub struct Recorder {
    commands: mpsc::Sender<Command>,
    worker: JoinHandle<Result<RecordingResult, String>>,
}

impl Recorder {
    /// Starts capturing. Returns once frames/audio are flowing or start failed.
    pub fn start(target: Target, options: RecorderOptions, events: UiHandle) -> Result<Recorder, String> {
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (commands, command_rx) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("TedCat recorder".into())
            .spawn(move || session::run(target, options, events, ready_tx, command_rx))
            .map_err(|e| format!("Could not start the recorder: {e}"))?;
        match ready_rx.recv_timeout(START_TIMEOUT) {
            Ok(Ok(())) => Ok(Recorder { commands, worker }),
            // The thread is already cleaning up and exits on its own.
            Ok(Err(e)) => Err(e),
            Err(RecvTimeoutError::Timeout) => {
                log_error!("recording did not start within {START_TIMEOUT:?}");
                let _ = commands.send(Command::Abort);
                Err("Recording did not start in time.".into())
            }
            Err(RecvTimeoutError::Disconnected) => {
                log_error!("the recorder thread ended during start");
                Err("The recorder stopped unexpectedly.".into())
            }
        }
    }

    /// Stops capturing and finalizes the file. Blocking; call it off the UI thread.
    pub fn stop(self) -> Result<RecordingResult, String> {
        let _ = self.commands.send(Command::Stop);
        match self.worker.join() {
            Ok(result) => result,
            Err(_) => {
                log_warn!("the recorder thread panicked");
                Err("The recorder stopped unexpectedly.".into())
            }
        }
    }
}

/// True when a default audio render (output) device exists.
pub fn audio_output_available() -> bool {
    audio::output_available()
}

/// `Recorder` must stay `Send` (see its documentation).
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<Recorder>();
};
