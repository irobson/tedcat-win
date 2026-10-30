//! What a recording captures, and how it is encoded.

use crate::geometry::{Rect, Size};

/// Video codec. Windows uses H.264, because the HEVC encoder is an optional,
/// sometimes paid, extension there; the macOS app uses HEVC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
}

impl Codec {
    /// Bits per pixel per frame. Screen content stays sharp well below 0.1.
    pub const fn bits_per_pixel(self) -> f64 {
        match self {
            Codec::H264 => 0.06,
            Codec::Hevc => 0.04,
        }
    }
    pub const fn minimum_bitrate(self) -> u32 {
        match self {
            Codec::H264 => 2_000_000,
            Codec::Hevc => 1_500_000,
        }
    }
    pub const fn maximum_bitrate(self) -> u32 {
        match self {
            Codec::H264 => 9_000_000,
            Codec::Hevc => 6_000_000,
        }
    }
}

/// Output size and bitrate for a region recording.
///
/// Tuned for screen content that must stay legible (code, documents, slides,
/// subtitles) while keeping files small: native pixel density, 30 fps, and a
/// bitrate proportional to the pixel rate, clamped to a sane range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoEncodingPlan {
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub average_bitrate: u32,
    pub codec: Codec,
}

impl VideoEncodingPlan {
    pub const FRAMES_PER_SECOND: u32 = 30;
    /// Encoders need a few macroblocks to work with.
    pub const MINIMUM_PIXEL_SIDE: u32 = 32;

    /// - `point_size`: region size in logical units (physical pixels / scale).
    /// - `scale`: monitor scale factor (1.0 at 96 DPI, 1.5 at 144 DPI, 2.0 at 192 DPI).
    ///
    /// Returns `None` when the region is too small to encode.
    pub fn new(point_size: Size, scale: f64, codec: Codec) -> Option<Self> {
        // H.264/HEVC need even dimensions (4:2:0 chroma subsampling).
        let width = even(point_size.width * scale);
        let height = even(point_size.height * scale);
        if width < Self::MINIMUM_PIXEL_SIDE || height < Self::MINIMUM_PIXEL_SIDE {
            return None;
        }
        let raw = (width as f64) * (height as f64) * (Self::FRAMES_PER_SECOND as f64) * codec.bits_per_pixel();
        let bitrate = (raw as u32).clamp(codec.minimum_bitrate(), codec.maximum_bitrate());
        Some(Self { pixel_width: width, pixel_height: height, average_bitrate: bitrate, codec })
    }
}

fn even(value: f64) -> u32 {
    let rounded = value.round().max(0.0) as u32;
    rounded - rounded % 2
}

/// What a recording captures.
///
/// The Shift + hold gesture only ever records a dragged region; audio-only
/// recordings are started from the tray menu. A drag that is too small or too
/// thin for the video encoder is reported as such, so the release can be
/// ignored instead of failing later.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RecordingTarget {
    /// System audio only, saved as M4A. Started from the menu, never by the gesture.
    AudioOnly,
    /// Video of the region plus system audio, saved as MP4.
    Region(Rect),
    /// The selection is below what the encoder accepts (including no drag at all).
    TooSmall,
}

impl RecordingTarget {
    /// Below this size on both axes, in logical units, the user has not really
    /// dragged yet. The overlay shows a hint instead of "too small".
    pub const DRAG_THRESHOLD: f64 = 10.0;

    /// The target for a dragged selection: a region if it can be filmed, else `TooSmall`.
    /// - `selection`: in logical units.
    /// - `scale`: monitor scale factor.
    pub fn from_selection(selection: Rect, scale: f64, codec: Codec) -> Self {
        if VideoEncodingPlan::new(selection.size(), scale, codec).is_some() {
            RecordingTarget::Region(selection)
        } else {
            RecordingTarget::TooSmall
        }
    }

    /// True while the selection is still below the drag threshold on both axes.
    pub fn is_undragged(selection: Rect) -> bool {
        selection.width < Self::DRAG_THRESHOLD && selection.height < Self::DRAG_THRESHOLD
    }

    pub fn is_audio_only(&self) -> bool {
        matches!(self, RecordingTarget::AudioOnly)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_keeps_native_density_and_even_dimensions() {
        let plan = VideoEncodingPlan::new(Size::new(401.3, 300.7), 2.0, Codec::Hevc).unwrap();
        assert_eq!((plan.pixel_width, plan.pixel_height), (802, 600));
    }

    #[test]
    fn odd_pixel_sizes_round_down_to_even() {
        let plan = VideoEncodingPlan::new(Size::new(101.0, 77.0), 1.0, Codec::H264).unwrap();
        assert_eq!((plan.pixel_width, plan.pixel_height), (100, 76));
    }

    #[test]
    fn fractional_scales_are_supported() {
        // 125% and 150% are common on Windows laptops.
        let plan = VideoEncodingPlan::new(Size::new(400.0, 300.0), 1.5, Codec::H264).unwrap();
        assert_eq!((plan.pixel_width, plan.pixel_height), (600, 450));
    }

    #[test]
    fn bitrate_scales_with_area_and_stays_within_bounds() {
        for codec in [Codec::H264, Codec::Hevc] {
            let tiny = VideoEncodingPlan::new(Size::new(40.0, 40.0), 1.0, codec).unwrap();
            assert_eq!(tiny.average_bitrate, codec.minimum_bitrate());
            let huge = VideoEncodingPlan::new(Size::new(3000.0, 2000.0), 2.0, codec).unwrap();
            assert_eq!(huge.average_bitrate, codec.maximum_bitrate());
            let mid = VideoEncodingPlan::new(Size::new(1600.0, 900.0), 1.0, codec).unwrap();
            assert!(mid.average_bitrate > codec.minimum_bitrate() && mid.average_bitrate < codec.maximum_bitrate());
        }
    }

    #[test]
    fn regions_too_small_to_encode_are_rejected() {
        assert!(VideoEncodingPlan::new(Size::new(10.0, 200.0), 2.0, Codec::H264).is_none());
    }

    #[test]
    fn releasing_without_dragging_records_nothing() {
        let press = Rect::new(100.0, 100.0, 0.0, 0.0);
        assert_eq!(RecordingTarget::from_selection(press, 2.0, Codec::H264), RecordingTarget::TooSmall);
        assert!(RecordingTarget::is_undragged(press));
    }

    #[test]
    fn a_real_drag_films_the_region() {
        let rect = Rect::new(10.0, 20.0, 400.0, 300.0);
        assert_eq!(RecordingTarget::from_selection(rect, 1.0, Codec::H264), RecordingTarget::Region(rect));
        assert!(!RecordingTarget::is_undragged(rect));
    }

    #[test]
    fn a_thin_drag_is_too_small_to_film() {
        let thin = Rect::new(0.0, 0.0, 200.0, 4.0);
        assert_eq!(RecordingTarget::from_selection(thin, 2.0, Codec::H264), RecordingTarget::TooSmall);
    }

    #[test]
    fn the_minimum_filmable_size_depends_on_scale() {
        let rect = Rect::new(0.0, 0.0, 20.0, 20.0);
        assert_eq!(RecordingTarget::from_selection(rect, 2.0, Codec::H264), RecordingTarget::Region(rect));
        assert_eq!(RecordingTarget::from_selection(rect, 1.0, Codec::H264), RecordingTarget::TooSmall);
    }

    #[test]
    fn the_gesture_never_produces_audio_only() {
        for (w, h) in [(0.0, 0.0), (5.0, 5.0), (9.5, 0.0)] {
            assert!(!RecordingTarget::from_selection(Rect::new(0.0, 0.0, w, h), 2.0, Codec::H264).is_audio_only());
        }
    }
}
