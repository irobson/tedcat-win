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

    /// Largest frame the encoders accept, as (long side, short side) in pixels.
    ///
    /// H.264: 4096 x 2304, the maximum frame size of levels 5.1 and 5.2
    /// (36 864 macroblocks). The Microsoft software encoder stops there and
    /// hardware encoders stop at 4096 wide. HEVC: 8192 x 4320 (level 6).
    pub const fn max_frame_sides(self) -> (u32, u32) {
        match self {
            Codec::H264 => (4096, 2304),
            Codec::Hevc => (8192, 4320),
        }
    }
}

/// Output size and bitrate for a region recording.
///
/// Tuned for screen content that must stay legible (code, documents, slides,
/// subtitles) while keeping files small: native pixel density, 30 fps, and a
/// bitrate proportional to the pixel rate, clamped to a sane range.
///
/// A region larger than the codec accepts (a full-width selection on a
/// 5120 x 1440 super-ultrawide or a 5K display, say) is scaled down to fit,
/// keeping its aspect ratio: `source_*` is what is cropped from the display,
/// `pixel_*` what is encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoEncodingPlan {
    /// Width of the captured region in physical pixels (even).
    pub source_width: u32,
    /// Height of the captured region in physical pixels (even).
    pub source_height: u32,
    /// Width of the encoded video (even): the source width, unless scaled down.
    pub pixel_width: u32,
    /// Height of the encoded video (even): the source height, unless scaled down.
    pub pixel_height: u32,
    /// For the encoded size.
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
        // The level limit is a frame size, so a portrait picture may be as
        // tall as a landscape one is wide (see `landscape_fallback`).
        let (long, short) = codec.max_frame_sides();
        let (max_width, max_height) = if height > width { (short, long) } else { (long, short) };
        Some(Self::fitted(width, height, max_width, max_height, codec))
    }

    /// True when the region is scaled down before it is encoded.
    pub fn is_scaled(&self) -> bool {
        (self.pixel_width, self.pixel_height) != (self.source_width, self.source_height)
    }

    /// This plan with the encoded size also fitted within `max_width` x
    /// `max_height` (never larger than it already is, never below
    /// `MINIMUM_PIXEL_SIDE`), keeping the source's aspect ratio. The bitrate
    /// follows the new size.
    pub fn fit_within(&self, max_width: u32, max_height: u32) -> Self {
        let max_width = max_width.min(self.pixel_width).max(Self::MINIMUM_PIXEL_SIDE);
        let max_height = max_height.min(self.pixel_height).max(Self::MINIMUM_PIXEL_SIDE);
        Self::fitted(self.source_width, self.source_height, max_width, max_height, self.codec)
    }

    /// The plan to retry with when no encoder accepts this one: some
    /// encoders compare a portrait frame with the landscape limit side by
    /// side (refusing 2160 x 3840 for H.264), so this fits the picture within
    /// the landscape limit whatever its orientation. `None` when this plan
    /// already fits it.
    pub fn landscape_fallback(&self) -> Option<Self> {
        let (long, short) = self.codec.max_frame_sides();
        let fallback = self.fit_within(long, short);
        (fallback != *self).then_some(fallback)
    }

    /// The plan for an even `source_width` x `source_height` region, scaled
    /// down uniformly when needed to fit `max_width` x `max_height`.
    fn fitted(source_width: u32, source_height: u32, max_width: u32, max_height: u32, codec: Codec) -> Self {
        let scale = (max_width as f64 / source_width as f64)
            .min(max_height as f64 / source_height as f64)
            .min(1.0);
        let (width, height) = if scale < 1.0 {
            (scaled_side(source_width, scale, max_width), scaled_side(source_height, scale, max_height))
        } else {
            (source_width, source_height)
        };
        let raw = (width as f64) * (height as f64) * (Self::FRAMES_PER_SECOND as f64) * codec.bits_per_pixel();
        let bitrate = (raw as u32).clamp(codec.minimum_bitrate(), codec.maximum_bitrate());
        Self { source_width, source_height, pixel_width: width, pixel_height: height, average_bitrate: bitrate, codec }
    }
}

/// One side scaled by `scale`: even, at most `max`, and at least the
/// encoder's minimum (an extreme sliver is then stretched a little rather
/// than refused).
fn scaled_side(side: u32, scale: f64, max: u32) -> u32 {
    let max = max - max % 2;
    even(side as f64 * scale).min(max).max(VideoEncodingPlan::MINIMUM_PIXEL_SIDE)
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
        assert_eq!((plan.source_width, plan.source_height), (802, 600));
        assert!(!plan.is_scaled());
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

    fn h264(width: f64, height: f64) -> VideoEncodingPlan {
        VideoEncodingPlan::new(Size::new(width, height), 1.0, Codec::H264).unwrap()
    }

    fn encoded(plan: &VideoEncodingPlan) -> (u32, u32) {
        (plan.pixel_width, plan.pixel_height)
    }

    #[test]
    fn sizes_the_encoder_accepts_are_not_scaled() {
        for (w, h) in [(3840.0, 2160.0), (4096.0, 2304.0), (2160.0, 3840.0), (2304.0, 4096.0), (2304.0, 2304.0)] {
            let plan = h264(w, h);
            assert_eq!(encoded(&plan), (w as u32, h as u32));
            assert!(!plan.is_scaled());
        }
    }

    #[test]
    fn a_super_ultrawide_full_width_region_is_scaled_to_4096_wide() {
        let plan = h264(5120.0, 1440.0);
        assert_eq!((plan.source_width, plan.source_height), (5120, 1440));
        assert_eq!(encoded(&plan), (4096, 1152));
        assert!(plan.is_scaled());
    }

    #[test]
    fn a_5k_full_screen_region_is_scaled_to_the_level_limit() {
        assert_eq!(encoded(&h264(5120.0, 2880.0)), (4096, 2304));
        // Same at 200 %: the limit is in pixels, not points.
        let plan = VideoEncodingPlan::new(Size::new(2560.0, 1440.0), 2.0, Codec::H264).unwrap();
        assert_eq!((plan.source_width, plan.source_height), (5120, 2880));
        assert_eq!(encoded(&plan), (4096, 2304));
    }

    #[test]
    fn a_wide_region_that_is_only_too_tall_is_scaled_by_its_height() {
        // 3000 x 2400 fits 4096 wide but not 2304 high.
        assert_eq!(encoded(&h264(3000.0, 2400.0)), (2880, 2304));
    }

    #[test]
    fn portrait_regions_may_be_as_tall_as_landscape_ones_are_wide() {
        // 5K portrait: 2880 x 5120 -> 2304 x 4096.
        assert_eq!(encoded(&h264(2880.0, 5120.0)), (2304, 4096));
    }

    #[test]
    fn scaling_keeps_the_aspect_ratio_and_even_sides() {
        for (w, h) in [(5120.0, 1440.0), (5120.0, 2160.0), (7680.0, 4320.0), (4500.0, 2531.0), (6016.0, 3384.0), (2531.0, 4500.0)] {
            let plan = h264(w, h);
            let (long, short) = Codec::H264.max_frame_sides();
            let (ew, eh) = encoded(&plan);
            assert!(ew % 2 == 0 && eh % 2 == 0, "{ew}x{eh} is odd");
            assert!(ew.max(eh) <= long && ew.min(eh) <= short, "{ew}x{eh} exceeds the limit");
            let source_ratio = plan.source_width as f64 / plan.source_height as f64;
            let ratio = ew as f64 / eh as f64;
            // Rounding each side to an even number moves it by under 2 px.
            assert!((ratio / source_ratio - 1.0).abs() < 2.0 / eh.min(ew) as f64, "{w}x{h} -> {ew}x{eh}");
        }
    }

    #[test]
    fn an_extreme_sliver_is_never_scaled_below_the_encoder_minimum() {
        let plan = h264(7680.0, 34.0);
        assert_eq!(encoded(&plan), (4096, VideoEncodingPlan::MINIMUM_PIXEL_SIDE));
    }

    #[test]
    fn the_bitrate_follows_the_encoded_size() {
        // 5120 x 400 is encoded as 4096 x 320, a mid-range bitrate.
        let plan = h264(5120.0, 400.0);
        assert_eq!(encoded(&plan), (4096, 320));
        let expected = (4096.0 * 320.0 * 30.0 * Codec::H264.bits_per_pixel()) as u32;
        assert_eq!(plan.average_bitrate, expected);
    }

    #[test]
    fn hevc_has_its_own_limit() {
        let plan = VideoEncodingPlan::new(Size::new(5120.0, 2880.0), 1.0, Codec::Hevc).unwrap();
        assert!(!plan.is_scaled());
        let plan = VideoEncodingPlan::new(Size::new(10240.0, 4320.0), 1.0, Codec::Hevc).unwrap();
        assert_eq!(encoded(&plan), (8192, 3456));
    }

    #[test]
    fn portrait_plans_have_a_landscape_fallback() {
        // 4K portrait, full height: kept as is, but some encoders refuse
        // anything taller than 2304, so there is a fallback.
        let plan = h264(2160.0, 3840.0);
        let fallback = plan.landscape_fallback().unwrap();
        assert_eq!((fallback.source_width, fallback.source_height), (2160, 3840));
        assert_eq!(encoded(&fallback), (1296, 2304));
        assert!(fallback.is_scaled());
        assert_eq!(fallback.landscape_fallback(), None);
        // Already scaled portrait plans fall back from the source too.
        let fallback = h264(2880.0, 5120.0).landscape_fallback().unwrap();
        assert_eq!(encoded(&fallback), (1296, 2304));
    }

    #[test]
    fn plans_that_fit_the_landscape_limit_have_no_fallback() {
        for (w, h) in [(400.0, 300.0), (5120.0, 1440.0), (2000.0, 2300.0), (300.0, 400.0)] {
            assert_eq!(h264(w, h).landscape_fallback(), None, "{w}x{h}");
        }
    }

    #[test]
    fn fit_within_only_ever_shrinks() {
        let plan = h264(480.0, 320.0);
        let smaller = plan.fit_within(240, 240);
        assert_eq!((smaller.source_width, smaller.source_height), (480, 320));
        assert_eq!(encoded(&smaller), (240, 160));
        assert_eq!(plan.fit_within(10_000, 10_000), plan);
        // Limits below the encoder minimum are raised to it.
        let tiny = plan.fit_within(2, 2);
        assert!(tiny.pixel_width >= VideoEncodingPlan::MINIMUM_PIXEL_SIDE && tiny.pixel_height >= VideoEncodingPlan::MINIMUM_PIXEL_SIDE);
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
