//! The Media Foundation sink writer: H.264 video and/or AAC audio in an
//! MPEG-4 container (`.mp4` for regions, `.m4a` for audio only).
//!
//! The writer is shared by the video (WGC callback) and audio threads behind a
//! `Mutex` in `session::Shared`, so writes are serialized.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Media::MediaFoundation::{
    eAVEncCommonRateControlMode_UnconstrainedVBR, eAVEncH264VProfile_Main, IMFAttributes, IMFByteStream,
    IMFMediaBuffer, IMFMediaType, IMFSinkWriter, MFAudioFormat_AAC, MFAudioFormat_PCM, MFCreateAttributes,
    MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFCreateSinkWriterFromURL, MFMediaType_Audio,
    MFMediaType_Video, MFTranscodeContainerType_MPEG4, MFVideoFormat_H264, MFVideoFormat_RGB32,
    MFVideoInterlace_Progressive, CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVGOPSize,
    MF_MT_ALL_SAMPLES_INDEPENDENT, MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE,
    MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_AVG_BITRATE,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS,
    MF_SINK_WRITER_DISABLE_THROTTLING, MF_SINK_WRITER_STATISTICS, MF_TRANSCODE_CONTAINERTYPE,
};

use super::sys::{delete_output, describe};
use crate::{log_info, log_warn};

/// Audio is always encoded as 48 kHz stereo.
pub(super) const AUDIO_RATE: u32 = 48_000;
pub(super) const AUDIO_CHANNELS: u32 = 2;
/// 16 000 bytes per second = 128 kbps AAC (one of the four rates the
/// Windows AAC encoder accepts).
const AAC_BYTES_PER_SECOND: u32 = 16_000;

/// The video stream to create.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct VideoFormat {
    pub width: u32,
    pub height: u32,
    pub bitrate: u32,
    pub fps: u32,
}

/// One way of configuring the encoder; tried in order until one works.
#[derive(Clone, Copy, Debug)]
struct Attempt {
    hardware: bool,
    /// Main profile, 2 s GOP and unconstrained VBR at the plan's mean bitrate.
    tuned: bool,
}

/// An open sink writer and what has been written to it.
pub(super) struct Writer {
    sink: Option<IMFSinkWriter>,
    video_stream: Option<u32>,
    audio_stream: Option<u32>,
    failed: bool,
    video_samples: u64,
    audio_samples: u64,
    end_time: i64,
}

// SAFETY: Media Foundation objects are free threaded (they are not bound to an
// apartment). The writer is only used through the Mutex in `session::Shared`,
// so calls are serialized, and only from MTA threads (or WGC callback threads,
// which run in the process MTA kept alive by the recorder thread).
unsafe impl Send for Writer {}

impl Writer {
    /// Creates the file and the streams, ready for samples.
    ///
    /// Tries hardware encoding with tuned settings first and falls back to
    /// software and to encoder defaults, because some hardware encoders reject
    /// small frame sizes or particular settings.
    pub(super) fn create(path: &Path, video: Option<VideoFormat>, audio: bool) -> Result<Writer, String> {
        let attempts: &[Attempt] = if video.is_some() {
            &[
                Attempt { hardware: true, tuned: true },
                Attempt { hardware: true, tuned: false },
                Attempt { hardware: false, tuned: true },
                Attempt { hardware: false, tuned: false },
            ]
        } else {
            &[Attempt { hardware: true, tuned: false }]
        };
        let mut last_error = String::from("no encoder configuration was tried");
        for attempt in attempts {
            match try_create(path, video, audio, *attempt) {
                Ok(writer) => {
                    log_info!(
                        "sink writer ready: video={video:?} audio={audio} hardware={} tuned={}",
                        attempt.hardware,
                        attempt.tuned
                    );
                    return Ok(writer);
                }
                Err(e) => {
                    last_error = describe(&e);
                    log_warn!("sink writer configuration {attempt:?} failed: {last_error}");
                    delete_output(path);
                }
            }
        }
        Err(last_error)
    }

    pub(super) fn has_audio_stream(&self) -> bool {
        self.audio_stream.is_some()
    }

    pub(super) fn video_samples(&self) -> u64 {
        self.video_samples
    }

    pub(super) fn audio_samples(&self) -> u64 {
        self.audio_samples
    }

    /// End of the latest sample written, in 100 ns since the start.
    pub(super) fn end_time(&self) -> i64 {
        self.end_time
    }

    /// Video frames handed to the writer but not yet encoded, when the writer
    /// reports it.
    pub(super) fn video_backlog(&self) -> Option<u64> {
        let (sink, stream) = (self.sink.as_ref()?, self.video_stream?);
        let mut stats = MF_SINK_WRITER_STATISTICS { cb: std::mem::size_of::<MF_SINK_WRITER_STATISTICS>() as u32, ..Default::default() };
        // SAFETY: `stats` is a valid out-parameter with `cb` set.
        unsafe { sink.GetStatistics(stream, &mut stats) }.ok()?;
        Some(stats.qwNumSamplesReceived.saturating_sub(stats.qwNumSamplesEncoded))
    }

    /// Writes one video frame (a top-down RGB32 buffer of the stream's size).
    pub(super) fn write_video(&mut self, buffer: &IMFMediaBuffer, time: i64, duration: i64) -> Result<(), String> {
        let Some(stream) = self.video_stream else { return Ok(()) };
        self.write(stream, buffer, time, duration)?;
        self.video_samples += 1;
        Ok(())
    }

    /// Writes interleaved 48 kHz stereo 16-bit samples.
    pub(super) fn write_audio(&mut self, pcm: &[i16], time: i64, duration: i64) -> Result<(), String> {
        let Some(stream) = self.audio_stream else { return Ok(()) };
        if pcm.is_empty() {
            return Ok(());
        }
        let bytes = pcm.len() * 2;
        let buffer = new_buffer(bytes, |out| {
            for (chunk, sample) in out.as_chunks_mut::<2>().0.iter_mut().zip(pcm) {
                *chunk = sample.to_le_bytes();
            }
        })
        .map_err(|e| self.fail(&e))?;
        self.write(stream, &buffer, time, duration)?;
        self.audio_samples += 1;
        Ok(())
    }

    fn write(&mut self, stream: u32, buffer: &IMFMediaBuffer, time: i64, duration: i64) -> Result<(), String> {
        if self.failed {
            return Err("the recording file could not be written".into());
        }
        let Some(sink) = self.sink.as_ref() else {
            return Err("the recording file is already closed".into());
        };
        // SAFETY: plain Media Foundation calls on objects this writer owns.
        let result = unsafe {
            MFCreateSample().and_then(|sample| {
                sample.AddBuffer(buffer)?;
                sample.SetSampleTime(time)?;
                sample.SetSampleDuration(duration.max(1))?;
                sink.WriteSample(stream, &sample)
            })
        };
        match result {
            Ok(()) => {
                self.end_time = self.end_time.max(time + duration.max(1));
                Ok(())
            }
            Err(e) => Err(self.fail(&e)),
        }
    }

    fn fail(&mut self, error: &windows::core::Error) -> String {
        self.failed = true;
        describe(error)
    }

    /// Completes the file (writes the MP4 index). Releases the sink writer
    /// whatever the outcome; later writes fail.
    pub(super) fn finalize(&mut self) -> Result<(), String> {
        let Some(sink) = self.sink.take() else {
            return Err("the recording file is already closed".into());
        };
        // SAFETY: Finalize blocks until every queued sample is encoded and written.
        unsafe { sink.Finalize() }.map_err(|e| describe(&e))
    }

    /// Releases the sink writer without completing the file (the caller deletes it).
    pub(super) fn abandon(&mut self) {
        self.sink = None;
    }
}

/// Creates a memory buffer of `len` bytes filled by `fill`.
pub(super) fn new_buffer(len: usize, fill: impl FnOnce(&mut [u8])) -> windows::core::Result<IMFMediaBuffer> {
    let size = u32::try_from(len).map_err(|_| windows::core::Error::from_hresult(E_FAIL))?;
    // SAFETY: the buffer is locked for exactly `size` bytes (MFCreateMemoryBuffer
    // allocates at least that much) and unlocked before it is returned.
    unsafe {
        let buffer = MFCreateMemoryBuffer(size)?;
        let mut data = std::ptr::null_mut();
        buffer.Lock(&mut data, None, None)?;
        if data.is_null() {
            let _ = buffer.Unlock();
            return Err(windows::core::Error::from_hresult(E_FAIL));
        }
        fill(std::slice::from_raw_parts_mut(data, len));
        buffer.Unlock()?;
        buffer.SetCurrentLength(size)?;
        Ok(buffer)
    }
}

/// A copy of a media buffer's current contents.
pub(super) fn copy_buffer(source: &IMFMediaBuffer) -> windows::core::Result<IMFMediaBuffer> {
    // SAFETY: the source is locked only while its bytes are copied.
    unsafe {
        let mut data = std::ptr::null_mut();
        let mut length = 0u32;
        source.Lock(&mut data, None, Some(&mut length))?;
        let copy = if data.is_null() {
            Err(windows::core::Error::from_hresult(E_FAIL))
        } else {
            let bytes = std::slice::from_raw_parts(data, length as usize);
            new_buffer(bytes.len(), |out| out.copy_from_slice(bytes))
        };
        let _ = source.Unlock();
        copy
    }
}

fn try_create(path: &Path, video: Option<VideoFormat>, audio: bool, attempt: Attempt) -> windows::core::Result<Writer> {
    let url: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    // SAFETY: Media Foundation setup calls; `url` outlives the call that reads it.
    unsafe {
        let attributes = new_attributes(3)?;
        attributes.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, attempt.hardware as u32)?;
        attributes.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
        // Pick the container explicitly: the extension (.m4a) is not always mapped.
        attributes.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
        let sink = MFCreateSinkWriterFromURL(PCWSTR(url.as_ptr()), None::<&IMFByteStream>, &attributes)?;

        let mut video_stream = None;
        if let Some(format) = video {
            let stream = sink.AddStream(&h264_type(&format, attempt.tuned)?)?;
            let parameters = if attempt.tuned { Some(encoder_parameters(&format)?) } else { None };
            sink.SetInputMediaType(stream, &rgb32_type(&format)?, parameters.as_ref())?;
            video_stream = Some(stream);
        }
        let mut audio_stream = None;
        if audio {
            let stream = sink.AddStream(&aac_type()?)?;
            sink.SetInputMediaType(stream, &pcm_type()?, None::<&IMFAttributes>)?;
            audio_stream = Some(stream);
        }
        sink.BeginWriting()?;
        Ok(Writer {
            sink: Some(sink),
            video_stream,
            audio_stream,
            failed: false,
            video_samples: 0,
            audio_samples: 0,
            end_time: 0,
        })
    }
}

// The `unsafe fn` helpers below only require that Media Foundation was started
// on the calling thread (`sys::MtaScope::enter(true)`).

unsafe fn new_attributes(capacity: u32) -> windows::core::Result<IMFAttributes> {
    let mut attributes = None;
    MFCreateAttributes(&mut attributes, capacity)?;
    attributes.ok_or_else(|| windows::core::Error::from_hresult(E_FAIL))
}

fn pack(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | low as u64
}

unsafe fn media_type(major: &GUID, subtype: &GUID) -> windows::core::Result<IMFMediaType> {
    let media_type = MFCreateMediaType()?;
    media_type.SetGUID(&MF_MT_MAJOR_TYPE, major)?;
    media_type.SetGUID(&MF_MT_SUBTYPE, subtype)?;
    Ok(media_type)
}

unsafe fn set_video_geometry(media_type: &IMFMediaType, format: &VideoFormat) -> windows::core::Result<()> {
    media_type.SetUINT64(&MF_MT_FRAME_SIZE, pack(format.width, format.height))?;
    media_type.SetUINT64(&MF_MT_FRAME_RATE, pack(format.fps, 1))?;
    media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
    media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
}

unsafe fn h264_type(format: &VideoFormat, tuned: bool) -> windows::core::Result<IMFMediaType> {
    let media_type = media_type(&MFMediaType_Video, &MFVideoFormat_H264)?;
    media_type.SetUINT32(&MF_MT_AVG_BITRATE, format.bitrate)?;
    set_video_geometry(&media_type, format)?;
    if tuned {
        media_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
    }
    Ok(media_type)
}

unsafe fn rgb32_type(format: &VideoFormat) -> windows::core::Result<IMFMediaType> {
    let media_type = media_type(&MFMediaType_Video, &MFVideoFormat_RGB32)?;
    set_video_geometry(&media_type, format)?;
    // A positive stride means top-down rows, which is how frames are copied.
    media_type.SetUINT32(&MF_MT_DEFAULT_STRIDE, format.width * 4)?;
    media_type.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
    Ok(media_type)
}

/// Encoder properties (ICodecAPI keys) passed through `SetInputMediaType`.
unsafe fn encoder_parameters(format: &VideoFormat) -> windows::core::Result<IMFAttributes> {
    let parameters = new_attributes(2)?;
    // A keyframe every 2 s keeps seeking responsive in players.
    parameters.SetUINT32(&CODECAPI_AVEncMPVGOPSize, format.fps * 2)?;
    // Screen content is mostly static: let quiet stretches use fewer bits.
    parameters.SetUINT32(&CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_UnconstrainedVBR.0 as u32)?;
    Ok(parameters)
}

unsafe fn aac_type() -> windows::core::Result<IMFMediaType> {
    let media_type = media_type(&MFMediaType_Audio, &MFAudioFormat_AAC)?;
    media_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
    media_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_RATE)?;
    media_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, AUDIO_CHANNELS)?;
    media_type.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AAC_BYTES_PER_SECOND)?;
    Ok(media_type)
}

unsafe fn pcm_type() -> windows::core::Result<IMFMediaType> {
    let media_type = media_type(&MFMediaType_Audio, &MFAudioFormat_PCM)?;
    let block_align = AUDIO_CHANNELS * 2;
    media_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
    media_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_RATE)?;
    media_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, AUDIO_CHANNELS)?;
    media_type.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, block_align)?;
    media_type.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AUDIO_RATE * block_align)?;
    media_type.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
    Ok(media_type)
}
