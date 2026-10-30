//! Pure audio sample handling: mix-format parsing, decoding to float,
//! downmixing to stereo, linear resampling and conversion to 16-bit PCM.
//!
//! The WASAPI shared-mode mix format is usually 32-bit float, but it can be
//! integer PCM, have more than two channels (5.1, 7.1) or run at 44.1, 96 or
//! 192 kHz. The encoder takes 48 kHz stereo 16-bit, so everything is converted
//! here. Nothing in this file touches Win32.

/// `WAVE_FORMAT_PCM`.
const TAG_PCM: u16 = 0x0001;
/// `WAVE_FORMAT_IEEE_FLOAT`.
const TAG_FLOAT: u16 = 0x0003;
/// `WAVE_FORMAT_EXTENSIBLE`.
const TAG_EXTENSIBLE: u16 = 0xFFFE;

// Speaker positions (`dwChannelMask` bits, `SPEAKER_*` in ksmedia.h).
const FRONT_LEFT: u32 = 0x1;
const FRONT_RIGHT: u32 = 0x2;
const FRONT_CENTER: u32 = 0x4;
const LOW_FREQUENCY: u32 = 0x8;
const BACK_LEFT: u32 = 0x10;
const BACK_RIGHT: u32 = 0x20;
const FRONT_LEFT_OF_CENTER: u32 = 0x40;
const FRONT_RIGHT_OF_CENTER: u32 = 0x80;
const BACK_CENTER: u32 = 0x100;
const SIDE_LEFT: u32 = 0x200;
const SIDE_RIGHT: u32 = 0x400;
const TOP_CENTER: u32 = 0x800;
const TOP_FRONT_LEFT: u32 = 0x1000;
const TOP_FRONT_CENTER: u32 = 0x2000;
const TOP_FRONT_RIGHT: u32 = 0x4000;
const TOP_BACK_LEFT: u32 = 0x8000;
const TOP_BACK_CENTER: u32 = 0x10000;
const TOP_BACK_RIGHT: u32 = 0x20000;

/// -3 dB, the usual weight for center and surround channels in a stereo downmix.
const MINUS_3DB: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// The sub-format GUID of a `WAVEFORMATEXTENSIBLE`, reduced to what matters here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubFormat {
    Pcm,
    Float,
    Other,
}

/// The extra fields of a `WAVEFORMATEXTENSIBLE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Extensible {
    pub valid_bits: u16,
    pub channel_mask: u32,
    pub sub_format: SubFormat,
}

/// A `WAVEFORMATEX` (optionally extensible) copied out of Win32 memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WaveDescription {
    pub format_tag: u16,
    pub channels: u16,
    pub rate: u32,
    pub bits: u16,
    pub block_align: u16,
    pub extensible: Option<Extensible>,
}

/// How each sample is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SampleKind {
    /// 32-bit IEEE float, nominal range -1..1.
    F32,
    /// 16-bit signed integer.
    I16,
    /// 24-bit signed integer packed in 3 bytes.
    I24,
    /// 32-bit container: 32-bit integer, or 24/20-bit left-justified.
    I32,
}

impl SampleKind {
    fn bytes(self) -> usize {
        match self {
            SampleKind::I16 => 2,
            SampleKind::I24 => 3,
            SampleKind::F32 | SampleKind::I32 => 4,
        }
    }
}

/// A capture format this module can decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PcmFormat {
    pub rate: u32,
    pub channels: u16,
    pub kind: SampleKind,
    /// Bytes per frame (all channels).
    pub block_align: u16,
    /// Speaker positions; 0 when unknown.
    pub channel_mask: u32,
}

/// Validates a mix format and works out how to decode it.
pub(crate) fn parse_format(wave: &WaveDescription) -> Result<PcmFormat, String> {
    let sub_format = match wave.format_tag {
        TAG_PCM => SubFormat::Pcm,
        TAG_FLOAT => SubFormat::Float,
        TAG_EXTENSIBLE => wave.extensible.map(|e| e.sub_format).unwrap_or(SubFormat::Other),
        _ => SubFormat::Other,
    };
    let unsupported = || {
        format!(
            "unsupported audio format (tag {:#06x}, {} bits, {} channels, {} Hz)",
            wave.format_tag, wave.bits, wave.channels, wave.rate
        )
    };
    if wave.channels == 0 || wave.rate == 0 || wave.block_align == 0 || !wave.block_align.is_multiple_of(wave.channels) {
        return Err(unsupported());
    }
    // The container size comes from the block alignment, which is reliable
    // even when wBitsPerSample describes only the valid bits.
    let container = (wave.block_align / wave.channels) as usize;
    if wave.extensible.is_some_and(|e| e.valid_bits as usize > container * 8) {
        return Err(unsupported());
    }
    let kind = match (sub_format, container) {
        (SubFormat::Float, 4) => SampleKind::F32,
        (SubFormat::Pcm, 2) => SampleKind::I16,
        (SubFormat::Pcm, 3) => SampleKind::I24,
        (SubFormat::Pcm, 4) => SampleKind::I32,
        _ => return Err(unsupported()),
    };
    Ok(PcmFormat {
        rate: wave.rate,
        channels: wave.channels,
        kind,
        block_align: wave.block_align,
        channel_mask: wave.extensible.map(|e| e.channel_mask).unwrap_or(0),
    })
}

/// The usual speaker layout for a channel count, used when the mask is missing.
fn default_mask(channels: u16) -> u32 {
    match channels {
        1 => FRONT_CENTER,
        2 => FRONT_LEFT | FRONT_RIGHT,
        3 => FRONT_LEFT | FRONT_RIGHT | FRONT_CENTER,
        4 => FRONT_LEFT | FRONT_RIGHT | BACK_LEFT | BACK_RIGHT,
        5 => FRONT_LEFT | FRONT_RIGHT | FRONT_CENTER | BACK_LEFT | BACK_RIGHT,
        6 => FRONT_LEFT | FRONT_RIGHT | FRONT_CENTER | LOW_FREQUENCY | BACK_LEFT | BACK_RIGHT,
        7 => FRONT_LEFT | FRONT_RIGHT | FRONT_CENTER | LOW_FREQUENCY | BACK_LEFT | BACK_RIGHT | BACK_CENTER,
        8 => FRONT_LEFT | FRONT_RIGHT | FRONT_CENTER | LOW_FREQUENCY | BACK_LEFT | BACK_RIGHT | SIDE_LEFT | SIDE_RIGHT,
        n => {
            if n >= 32 {
                u32::MAX
            } else {
                (1u32 << n) - 1
            }
        }
    }
}

fn speaker_weights(speaker: u32) -> (f32, f32) {
    match speaker {
        FRONT_LEFT | FRONT_LEFT_OF_CENTER => (1.0, 0.0),
        FRONT_RIGHT | FRONT_RIGHT_OF_CENTER => (0.0, 1.0),
        BACK_LEFT | SIDE_LEFT | TOP_FRONT_LEFT | TOP_BACK_LEFT => (MINUS_3DB, 0.0),
        BACK_RIGHT | SIDE_RIGHT | TOP_FRONT_RIGHT | TOP_BACK_RIGHT => (0.0, MINUS_3DB),
        FRONT_CENTER | BACK_CENTER | TOP_CENTER | TOP_FRONT_CENTER | TOP_BACK_CENTER => (MINUS_3DB, MINUS_3DB),
        _ => (0.0, 0.0), // LFE and unknown positions are left out.
    }
}

/// Per-channel (left, right) gains for a stereo downmix.
///
/// Mono is copied to both sides; stereo passes through; front channels keep
/// full level, center and surround channels are mixed in at -3 dB, LFE is
/// dropped (the usual ITU-style downmix, clipped later by [`f32_to_i16`]).
pub(crate) fn downmix_weights(channels: u16, channel_mask: u32) -> Vec<(f32, f32)> {
    if channels == 1 {
        return vec![(1.0, 1.0)];
    }
    let mask = if channel_mask.count_ones() >= channels as u32 { channel_mask } else { default_mask(channels) };
    let mut speakers = (0..32).map(|bit| 1u32 << bit).filter(|bit| mask & bit != 0);
    (0..channels).map(|_| speakers.next().map(speaker_weights).unwrap_or((0.0, 0.0))).collect()
}

fn read_sample(bytes: &[u8], kind: SampleKind) -> f32 {
    match kind {
        SampleKind::F32 => f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        SampleKind::I16 => i16::from_le_bytes([bytes[0], bytes[1]]) as f32 / 32_768.0,
        SampleKind::I24 => {
            let value = i32::from_le_bytes([0, bytes[0], bytes[1], bytes[2]]) >> 8;
            value as f32 / 8_388_608.0
        }
        SampleKind::I32 => i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f32 / 2_147_483_648.0,
    }
}

/// Decodes whole frames from `bytes` and appends them to `out` as
/// interleaved stereo floats, using `weights` from [`downmix_weights`].
/// A trailing partial frame is ignored.
pub(crate) fn decode_to_stereo(bytes: &[u8], format: &PcmFormat, weights: &[(f32, f32)], out: &mut Vec<f32>) {
    let block = format.block_align as usize;
    let sample = format.kind.bytes();
    let channels = (format.channels as usize).min(weights.len());
    if block == 0 || sample * channels > block {
        return;
    }
    out.reserve(bytes.len() / block * 2);
    for frame in bytes.chunks_exact(block) {
        let (mut left, mut right) = (0.0f32, 0.0f32);
        for (channel, &(to_left, to_right)) in weights.iter().enumerate().take(channels) {
            let value = read_sample(&frame[channel * sample..], format.kind);
            left += value * to_left;
            right += value * to_right;
        }
        out.push(left);
        out.push(right);
    }
}

/// Converts floats (nominal range -1..1) to 16-bit PCM, clipping overs.
/// `out` is cleared first.
pub(crate) fn f32_to_i16(input: &[f32], out: &mut Vec<i16>) {
    out.clear();
    out.extend(input.iter().map(|&x| {
        let x = if x.is_nan() { 0.0 } else { x.clamp(-1.0, 1.0) };
        (x * 32_767.0).round() as i16
    }));
}

/// A streaming linear-interpolation resampler for interleaved stereo.
///
/// Good enough for system audio going into a 128 kbps AAC encoder; most mix
/// formats already run at 48 kHz, in which case samples pass through untouched.
/// State carries over between calls, so splitting the input into packets gives
/// the same output as processing it in one go.
#[derive(Clone, Debug)]
pub(crate) struct LinearResampler {
    step: f64,
    passthrough: bool,
    /// Position of the next output sample, in input frames relative to the
    /// first frame of the next input block (-1.0 means the remembered `previous`).
    position: f64,
    previous: Option<[f32; 2]>,
}

impl LinearResampler {
    pub(crate) fn new(input_rate: u32, output_rate: u32) -> Self {
        Self {
            step: input_rate.max(1) as f64 / output_rate.max(1) as f64,
            passthrough: input_rate == output_rate,
            position: 0.0,
            previous: None,
        }
    }

    /// Resamples interleaved stereo `input` and appends the result to `out`.
    pub(crate) fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let frames = input.len() / 2;
        if frames == 0 {
            return;
        }
        if self.passthrough {
            out.extend_from_slice(&input[..frames * 2]);
            return;
        }
        let previous = self.previous.unwrap_or([input[0], input[1]]);
        let at = |index: isize| -> [f32; 2] {
            if index < 0 {
                previous
            } else {
                let i = index as usize * 2;
                [input[i], input[i + 1]]
            }
        };
        out.reserve((frames as f64 / self.step) as usize + 2);
        let last = (frames - 1) as f64;
        while self.position < last {
            let base = self.position.floor();
            let fraction = (self.position - base) as f32;
            let a = at(base as isize);
            let b = at(base as isize + 1);
            out.push(a[0] + (b[0] - a[0]) * fraction);
            out.push(a[1] + (b[1] - a[1]) * fraction);
            self.position += self.step;
        }
        self.position -= frames as f64;
        self.previous = Some(at(frames as isize - 1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn float_stereo_48k() -> WaveDescription {
        WaveDescription {
            format_tag: TAG_EXTENSIBLE,
            channels: 2,
            rate: 48_000,
            bits: 32,
            block_align: 8,
            extensible: Some(Extensible {
                valid_bits: 32,
                channel_mask: FRONT_LEFT | FRONT_RIGHT,
                sub_format: SubFormat::Float,
            }),
        }
    }

    #[test]
    fn parses_the_common_float_mix_format() {
        let format = parse_format(&float_stereo_48k()).unwrap();
        assert_eq!(
            format,
            PcmFormat { rate: 48_000, channels: 2, kind: SampleKind::F32, block_align: 8, channel_mask: 3 }
        );
    }

    #[test]
    fn parses_integer_formats_by_container_size() {
        let plain16 = WaveDescription {
            format_tag: TAG_PCM,
            channels: 2,
            rate: 44_100,
            bits: 16,
            block_align: 4,
            extensible: None,
        };
        assert_eq!(parse_format(&plain16).unwrap().kind, SampleKind::I16);
        let packed24 = WaveDescription { bits: 24, block_align: 6, ..plain16 };
        assert_eq!(parse_format(&packed24).unwrap().kind, SampleKind::I24);
        let in32 = WaveDescription {
            format_tag: TAG_EXTENSIBLE,
            bits: 32,
            block_align: 8,
            extensible: Some(Extensible { valid_bits: 24, channel_mask: 3, sub_format: SubFormat::Pcm }),
            ..plain16
        };
        assert_eq!(parse_format(&in32).unwrap().kind, SampleKind::I32);
        let plain_float = WaveDescription { format_tag: TAG_FLOAT, bits: 32, block_align: 8, ..plain16 };
        assert_eq!(parse_format(&plain_float).unwrap().kind, SampleKind::F32);
    }

    #[test]
    fn rejects_formats_it_cannot_decode() {
        let base = float_stereo_48k();
        let double = WaveDescription { bits: 64, block_align: 16, ..base };
        assert!(parse_format(&double).is_err());
        let eight_bit = WaveDescription { format_tag: TAG_PCM, bits: 8, block_align: 2, extensible: None, ..base };
        assert!(parse_format(&eight_bit).is_err());
        let other = WaveDescription {
            extensible: Some(Extensible { valid_bits: 16, channel_mask: 3, sub_format: SubFormat::Other }),
            ..base
        };
        assert!(parse_format(&other).is_err());
        assert!(parse_format(&WaveDescription { channels: 0, ..base }).is_err());
        assert!(parse_format(&WaveDescription { block_align: 7, ..base }).is_err());
    }

    #[test]
    fn decodes_float_stereo_unchanged() {
        let format = parse_format(&float_stereo_48k()).unwrap();
        let samples = [0.5f32, -0.25, 1.0, -1.0];
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut out = Vec::new();
        decode_to_stereo(&bytes, &format, &downmix_weights(2, 3), &mut out);
        assert_eq!(out, samples);
    }

    #[test]
    fn decodes_integer_samples_to_unit_range() {
        let i16_format = PcmFormat { rate: 48_000, channels: 2, kind: SampleKind::I16, block_align: 4, channel_mask: 0 };
        let bytes: Vec<u8> = [16_384i16, -32_768].iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut out = Vec::new();
        decode_to_stereo(&bytes, &i16_format, &downmix_weights(2, 0), &mut out);
        assert_eq!(out, [0.5, -1.0]);

        let i24_format = PcmFormat { kind: SampleKind::I24, block_align: 6, ..i16_format };
        // +0.5 (0x400000) and -0.5 (0xC00000), little endian.
        let bytes = [0x00, 0x00, 0x40, 0x00, 0x00, 0xC0];
        out.clear();
        decode_to_stereo(&bytes, &i24_format, &downmix_weights(2, 0), &mut out);
        assert_eq!(out, [0.5, -0.5]);

        let i32_format = PcmFormat { kind: SampleKind::I32, block_align: 8, ..i16_format };
        let bytes: Vec<u8> = [i32::MIN, 1 << 30].iter().flat_map(|s| s.to_le_bytes()).collect();
        out.clear();
        decode_to_stereo(&bytes, &i32_format, &downmix_weights(2, 0), &mut out);
        assert_eq!(out, [-1.0, 0.5]);
    }

    #[test]
    fn mono_is_copied_to_both_sides() {
        let format = PcmFormat { rate: 48_000, channels: 1, kind: SampleKind::F32, block_align: 4, channel_mask: 4 };
        let bytes: Vec<u8> = [0.25f32, -0.5].iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut out = Vec::new();
        decode_to_stereo(&bytes, &format, &downmix_weights(1, 4), &mut out);
        assert_eq!(out, [0.25, 0.25, -0.5, -0.5]);
    }

    #[test]
    fn surround_downmix_keeps_fronts_splits_center_and_drops_lfe() {
        let weights = downmix_weights(6, 0x3F); // 5.1: FL FR FC LFE BL BR
        assert_eq!(weights[0], (1.0, 0.0));
        assert_eq!(weights[1], (0.0, 1.0));
        assert_eq!(weights[2], (MINUS_3DB, MINUS_3DB));
        assert_eq!(weights[3], (0.0, 0.0));
        assert_eq!(weights[4], (MINUS_3DB, 0.0));
        assert_eq!(weights[5], (0.0, MINUS_3DB));
        // A missing mask falls back to the standard 5.1 layout.
        assert_eq!(downmix_weights(6, 0), weights);
        // 7.1 uses side channels too.
        let seven = downmix_weights(8, 0);
        assert_eq!(seven[6], (MINUS_3DB, 0.0));
        assert_eq!(seven[7], (0.0, MINUS_3DB));

        let format = PcmFormat { rate: 48_000, channels: 6, kind: SampleKind::F32, block_align: 24, channel_mask: 0x3F };
        let frame = [0.5f32, 0.0, 0.2, 1.0, 0.0, 0.0];
        let bytes: Vec<u8> = frame.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut out = Vec::new();
        decode_to_stereo(&bytes, &format, &weights, &mut out);
        assert!((out[0] - (0.5 + 0.2 * MINUS_3DB)).abs() < 1e-6);
        assert!((out[1] - 0.2 * MINUS_3DB).abs() < 1e-6);
    }

    #[test]
    fn partial_frames_are_ignored() {
        let format = parse_format(&float_stereo_48k()).unwrap();
        let mut out = Vec::new();
        decode_to_stereo(&[0u8; 12], &format, &downmix_weights(2, 3), &mut out);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn float_to_i16_rounds_and_clips() {
        let mut out = Vec::new();
        f32_to_i16(&[0.0, 0.5, -0.5, 1.0, -1.0, 1.7, -3.0, f32::NAN], &mut out);
        assert_eq!(out, [0, 16_384, -16_384, 32_767, -32_767, 32_767, -32_767, 0]);
    }

    fn ramp(frames: usize) -> Vec<f32> {
        (0..frames).flat_map(|i| [i as f32, -(i as f32)]).collect()
    }

    #[test]
    fn same_rate_passes_through() {
        let mut resampler = LinearResampler::new(48_000, 48_000);
        let input = ramp(10);
        let mut out = Vec::new();
        resampler.process(&input, &mut out);
        assert_eq!(out, input);
    }

    #[test]
    fn upsampling_by_two_interpolates_midpoints() {
        let mut resampler = LinearResampler::new(24_000, 48_000);
        let mut out = Vec::new();
        resampler.process(&ramp(4), &mut out);
        let left: Vec<f32> = out.iter().step_by(2).copied().collect();
        assert_eq!(left, [0.0, 0.5, 1.0, 1.5, 2.0, 2.5]);
        assert!(out.iter().skip(1).step_by(2).zip(&left).all(|(r, l)| *r == -*l));
    }

    #[test]
    fn downsampling_by_two_keeps_every_other_frame() {
        let mut resampler = LinearResampler::new(96_000, 48_000);
        let mut out = Vec::new();
        resampler.process(&ramp(8), &mut out);
        let left: Vec<f32> = out.iter().step_by(2).copied().collect();
        assert_eq!(left, [0.0, 2.0, 4.0, 6.0]);
    }

    #[test]
    fn splitting_the_input_does_not_change_the_output() {
        let input = ramp(1_000);
        let mut whole = Vec::new();
        LinearResampler::new(44_100, 48_000).process(&input, &mut whole);

        let mut chunked = Vec::new();
        let mut resampler = LinearResampler::new(44_100, 48_000);
        for chunk in input.chunks(2 * 37) {
            resampler.process(chunk, &mut chunked);
        }
        assert_eq!(whole.len(), chunked.len());
        assert!(whole.iter().zip(&chunked).all(|(a, b)| (a - b).abs() < 1e-3));
    }

    #[test]
    fn output_length_follows_the_rate_ratio() {
        let mut resampler = LinearResampler::new(44_100, 48_000);
        let mut out = Vec::new();
        for _ in 0..100 {
            resampler.process(&vec![0.0; 441 * 2], &mut out);
        }
        // 44 100 input frames -> 48 000 output frames, give or take one.
        let frames = out.len() / 2;
        assert!((47_999..=48_001).contains(&frames), "{frames}");
    }
}
