//! Geometry, text and pixel math for the selection overlay, free of Win32 calls
//! so it can be unit tested.

use tedcat_core::{CaptureMode, Codec, RecordingTarget};

use crate::geom::{PointPx, RectPx};

/// Label shown while a recording selection is below the drag threshold.
pub(super) const DRAG_HINT: &str = "● REC  ·  drag to select an area";
/// Gap between the selection and the label, in logical units.
pub(super) const LABEL_GAP_POINTS: f64 = 8.0;
/// Label font size (em height), in logical units.
pub(super) const LABEL_FONT_POINTS: f64 = 13.0;
pub(super) const LABEL_PADDING_X_POINTS: f64 = 9.0;
pub(super) const LABEL_PADDING_Y_POINTS: f64 = 4.0;
pub(super) const LABEL_RADIUS_POINTS: f64 = 6.0;
/// Opacity of the label body, 0..=255.
pub(super) const LABEL_OPACITY: u8 = 240;
/// Total duration of the white flash after a screenshot.
pub(super) const FLASH_MILLIS: u64 = 180;
/// Opacity of the flash when it appears, 0..=255.
pub(super) const FLASH_START_ALPHA: u8 = 190;

/// `points` in logical units to whole physical pixels (at least 1).
pub(super) fn scaled(points: f64, scale: f64) -> i32 {
    let scale = sane_scale(scale);
    ((points * scale).round() as i32).max(1)
}

fn sane_scale(scale: f64) -> f64 {
    if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    }
}

/// Normalizes `rect` and clamps it into `bounds` coordinate by coordinate, so
/// a zero-size selection keeps its position instead of collapsing to (0, 0).
pub(super) fn clamp_rect(rect: RectPx, bounds: RectPx) -> RectPx {
    let clamp_x = |v: i32| v.max(bounds.left).min(bounds.right.max(bounds.left));
    let clamp_y = |v: i32| v.max(bounds.top).min(bounds.bottom.max(bounds.top));
    RectPx::new(
        clamp_x(rect.left.min(rect.right)),
        clamp_y(rect.top.min(rect.bottom)),
        clamp_x(rect.left.max(rect.right)),
        clamp_y(rect.top.max(rect.bottom)),
    )
}

/// Width and height in logical units, rounded.
pub(super) fn logical_size(rect: RectPx, scale: f64) -> (i64, i64) {
    let scale = sane_scale(scale);
    let w = (f64::from(rect.width().max(0)) / scale).round() as i64;
    let h = (f64::from(rect.height().max(0)) / scale).round() as i64;
    (w, h)
}

/// True while a recording selection is below the drag threshold on both axes.
pub(super) fn is_undragged(selection: RectPx, scale: f64) -> bool {
    RecordingTarget::is_undragged(selection.to_logical(sane_scale(scale)))
}

/// The label for a (clamped) selection, or `None` when no label is shown
/// (a screenshot selection that has no area yet).
pub(super) fn label_text(mode: CaptureMode, selection: RectPx, scale: f64) -> Option<String> {
    let scale = sane_scale(scale);
    let (w, h) = logical_size(selection, scale);
    match mode {
        CaptureMode::Screenshot => (!selection.is_empty()).then(|| format!("{w} × {h}")),
        CaptureMode::Recording => {
            let logical = selection.to_logical(scale);
            if RecordingTarget::is_undragged(logical) {
                return Some(DRAG_HINT.to_owned());
            }
            Some(match RecordingTarget::from_selection(logical, scale, Codec::H264) {
                RecordingTarget::TooSmall => format!("{w} × {h}  ·  too small to film"),
                RecordingTarget::Region(_) | RecordingTarget::AudioOnly => format!("● REC  {w} × {h}"),
            })
        }
    }
}

/// Where the label goes: centred below `anchor`, above it if there is no room
/// below, inside it (along its bottom edge) as a last resort, and always
/// entirely within `monitor`.
pub(super) fn label_position(size: (i32, i32), anchor: RectPx, monitor: RectPx, gap: i32) -> PointPx {
    let (w, h) = size;
    let below = anchor.bottom + gap;
    let above = anchor.top - gap - h;
    let y = if below + h <= monitor.bottom {
        below
    } else if above >= monitor.top {
        above
    } else {
        anchor.bottom - gap - h
    };
    let x = anchor.left + anchor.width() / 2 - w / 2;
    PointPx::new(
        x.min(monitor.right - w).max(monitor.left),
        y.min(monitor.bottom - h).max(monitor.top),
    )
}

/// Thickness of the frame ring: 1-2 px white for screenshots, 2 px red for recordings.
pub(super) fn ring_thickness(mode: CaptureMode, scale: f64) -> i32 {
    match mode {
        CaptureMode::Screenshot if sane_scale(scale) < 1.5 => 1,
        CaptureMode::Screenshot | CaptureMode::Recording => 2,
    }
}

/// `rect` relative to `origin` (screen to window coordinates).
pub(super) fn relative(rect: RectPx, origin: PointPx) -> RectPx {
    RectPx::new(rect.left - origin.x, rect.top - origin.y, rect.right - origin.x, rect.bottom - origin.y)
}

/// `rect` grown by `by` on every side.
pub(super) fn inflate(rect: RectPx, by: i32) -> RectPx {
    RectPx::new(rect.left - by, rect.top - by, rect.right + by, rect.bottom + by)
}

/// Splits a label into (text, is_red_dot) runs: a leading "●" is drawn red.
pub(super) fn label_runs(text: &str) -> Vec<(&str, bool)> {
    match text.strip_prefix('●') {
        Some(rest) if !rest.is_empty() => vec![("●", true), (rest, false)],
        Some(_) => vec![("●", true)],
        None => vec![(text, false)],
    }
}

/// Anti-aliased coverage (0..=1) of pixel (`x`, `y`) by a `w` × `h` rounded
/// rectangle with corner radius `radius`.
pub(super) fn rounded_rect_coverage(x: i32, y: i32, w: i32, h: i32, radius: f64) -> f64 {
    let (w, h) = (f64::from(w), f64::from(h));
    let r = radius.max(0.0).min(w / 2.0).min(h / 2.0);
    let (px, py) = (f64::from(x) + 0.5, f64::from(y) + 0.5);
    if px < 0.0 || py < 0.0 || px > w || py > h {
        return 0.0;
    }
    // Signed distance from the pixel centre to the outline (negative inside):
    // to the corner arc in the corner squares, to the nearest edge elsewhere.
    let in_corner = (px < r || px > w - r) && (py < r || py > h - r);
    let distance = if in_corner {
        let cx = px.clamp(r, w - r);
        let cy = py.clamp(r, h - r);
        (px - cx).hypot(py - cy) - r
    } else {
        -px.min(w - px).min(py).min(h - py)
    };
    (0.5 - distance).clamp(0.0, 1.0)
}

/// Premultiplies a BGRA pixel by `coverage` × `opacity`; returns the new pixel.
pub(super) fn premultiply(bgr: [u8; 3], coverage: f64, opacity: u8) -> [u8; 4] {
    let alpha = (coverage.clamp(0.0, 1.0) * f64::from(opacity)).round();
    let mul = |c: u8| ((f64::from(c) * alpha) / 255.0).round() as u8;
    [mul(bgr[0]), mul(bgr[1]), mul(bgr[2]), alpha as u8]
}

/// Flash opacity `elapsed_ms` after it appeared, or `None` once it is over.
/// Eases out (quadratic), from `FLASH_START_ALPHA` to 0.
pub(super) fn flash_alpha(elapsed_ms: u64) -> Option<u8> {
    if elapsed_ms >= FLASH_MILLIS {
        return None;
    }
    let remaining = 1.0 - elapsed_ms as f64 / FLASH_MILLIS as f64;
    Some((f64::from(FLASH_START_ALPHA) * remaining * remaining).round() as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MONITOR: RectPx = RectPx::new(0, 0, 1920, 1080);

    #[test]
    fn screenshot_label_shows_logical_size() {
        let rect = RectPx::new(10, 10, 650, 410);
        assert_eq!(label_text(CaptureMode::Screenshot, rect, 1.0).as_deref(), Some("640 × 400"));
        let retina = RectPx::new(0, 0, 1280, 800);
        assert_eq!(label_text(CaptureMode::Screenshot, retina, 2.0).as_deref(), Some("640 × 400"));
        let laptop = RectPx::new(0, 0, 801, 500);
        assert_eq!(label_text(CaptureMode::Screenshot, laptop, 1.25).as_deref(), Some("641 × 400"));
        assert_eq!(label_text(CaptureMode::Screenshot, RectPx::new(5, 5, 5, 5), 1.0), None);
    }

    #[test]
    fn recording_label_follows_the_recording_target() {
        let press = RectPx::new(100, 100, 100, 100);
        assert_eq!(label_text(CaptureMode::Recording, press, 1.0).as_deref(), Some(DRAG_HINT));
        assert_eq!(DRAG_HINT, "● REC  ·  drag to select an area");
        let small = RectPx::new(100, 100, 109, 108);
        assert_eq!(label_text(CaptureMode::Recording, small, 1.0).as_deref(), Some(DRAG_HINT));
        let thin = RectPx::new(0, 0, 640, 4);
        assert_eq!(label_text(CaptureMode::Recording, thin, 1.0).as_deref(), Some("640 × 4  ·  too small to film"));
        let region = RectPx::new(0, 0, 640, 400);
        assert_eq!(label_text(CaptureMode::Recording, region, 1.0).as_deref(), Some("● REC  640 × 400"));
        let retina = RectPx::new(0, 0, 1280, 800);
        assert_eq!(label_text(CaptureMode::Recording, retina, 2.0).as_deref(), Some("● REC  640 × 400"));
    }

    #[test]
    fn label_goes_below_then_above_then_inside() {
        let size = (120, 24);
        let below = label_position(size, RectPx::new(100, 100, 300, 200), MONITOR, 8);
        assert_eq!(below, PointPx::new(140, 208));
        let near_bottom = RectPx::new(100, 900, 300, 1070);
        assert_eq!(label_position(size, near_bottom, MONITOR, 8), PointPx::new(140, 868));
        let full_height = RectPx::new(100, 0, 300, 1080);
        assert_eq!(label_position(size, full_height, MONITOR, 8), PointPx::new(140, 1048));
    }

    #[test]
    fn label_stays_on_the_monitor() {
        let size = (200, 24);
        let left_edge = label_position(size, RectPx::new(0, 100, 20, 120), MONITOR, 8);
        assert_eq!(left_edge.x, 0);
        let right_edge = label_position(size, RectPx::new(1900, 100, 1920, 120), MONITOR, 8);
        assert_eq!(right_edge.x, 1720);
        let second = RectPx::new(1920, -200, 3840, 880);
        let pos = label_position(size, RectPx::new(1925, -195, 1930, -190), second, 8);
        assert!(pos.x >= second.left && pos.x + size.0 <= second.right);
        assert!(pos.y >= second.top && pos.y + size.1 <= second.bottom);
    }

    #[test]
    fn clamping_keeps_zero_size_selections_in_place() {
        assert_eq!(clamp_rect(RectPx::new(-10, 1000, 30, 1200), MONITOR), RectPx::new(0, 1000, 30, 1080));
        assert_eq!(clamp_rect(RectPx::new(50, 60, 50, 60), MONITOR), RectPx::new(50, 60, 50, 60));
        assert_eq!(clamp_rect(RectPx::new(300, 200, 100, 100), MONITOR), RectPx::new(100, 100, 300, 200));
        assert!(clamp_rect(RectPx::new(3000, 50, 3100, 80), MONITOR).is_empty());
    }

    #[test]
    fn ring_and_relative_geometry() {
        assert_eq!(ring_thickness(CaptureMode::Screenshot, 1.0), 1);
        assert_eq!(ring_thickness(CaptureMode::Screenshot, 2.0), 2);
        assert_eq!(ring_thickness(CaptureMode::Recording, 1.0), 2);
        let r = RectPx::new(2000, 100, 2100, 200);
        assert_eq!(relative(r, PointPx::new(1920, 0)), RectPx::new(80, 100, 180, 200));
        assert_eq!(inflate(r, 2), RectPx::new(1998, 98, 2102, 202));
        assert_eq!(scaled(13.0, 1.5), 20);
        assert_eq!(scaled(0.2, 1.0), 1);
    }

    #[test]
    fn red_dot_is_split_off() {
        assert_eq!(label_runs("● REC  640 × 400"), vec![("●", true), (" REC  640 × 400", false)]);
        assert_eq!(label_runs("640 × 400"), vec![("640 × 400", false)]);
    }

    #[test]
    fn rounded_corners_are_transparent_and_the_body_opaque() {
        let (w, h) = (100, 24);
        assert_eq!(rounded_rect_coverage(0, 0, w, h, 6.0), 0.0);
        assert_eq!(rounded_rect_coverage(50, 12, w, h, 6.0), 1.0);
        assert_eq!(rounded_rect_coverage(50, 0, w, h, 6.0), 1.0);
        assert_eq!(rounded_rect_coverage(0, 12, w, h, 6.0), 1.0);
        let edge = rounded_rect_coverage(1, 2, w, h, 6.0);
        assert!(edge > 0.0 && edge < 1.0, "corner edge pixel is partially covered: {edge}");
        assert_eq!(rounded_rect_coverage(100, 12, w, h, 6.0), 0.0);
    }

    #[test]
    fn premultiplied_pixels() {
        assert_eq!(premultiply([255, 255, 255], 1.0, 255), [255, 255, 255, 255]);
        assert_eq!(premultiply([255, 128, 0], 0.0, 255), [0, 0, 0, 0]);
        let half = premultiply([200, 100, 50], 0.5, 255);
        assert_eq!(half[3], 128);
        assert!(half[0] <= half[3]);
    }

    #[test]
    fn flash_fades_out_in_180_ms() {
        assert_eq!(flash_alpha(0), Some(FLASH_START_ALPHA));
        let mut last = FLASH_START_ALPHA;
        for t in (15..FLASH_MILLIS).step_by(15) {
            let a = flash_alpha(t).unwrap();
            assert!(a <= last);
            last = a;
        }
        assert_eq!(flash_alpha(FLASH_MILLIS), None);
        assert_eq!(flash_alpha(10_000), None);
    }
}
