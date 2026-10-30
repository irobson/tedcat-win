//! Pure placement math for TedCat's small windows, in physical pixels.

use crate::geom::{PointPx, RectPx};

/// A corner of a monitor's work area.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// Distance from the work-area edges to the pill and the card, in logical px.
pub(crate) const CORNER_MARGIN: f64 = 16.0;

/// Logical length to physical pixels for a monitor scale.
pub(crate) fn px(logical: f64, scale: f64) -> i32 {
    (logical * scale).round() as i32
}

/// The corner of `area` farthest from the center of `region`.
/// Ties prefer bottom-right, then top-right, bottom-left and top-left.
pub(crate) fn farthest_corner(area: RectPx, region: RectPx) -> Corner {
    let cx = (region.left as i64 + region.right as i64) / 2;
    let cy = (region.top as i64 + region.bottom as i64) / 2;
    let candidates = [
        (Corner::BottomRight, area.right as i64, area.bottom as i64),
        (Corner::TopRight, area.right as i64, area.top as i64),
        (Corner::BottomLeft, area.left as i64, area.bottom as i64),
        (Corner::TopLeft, area.left as i64, area.top as i64),
    ];
    let mut best = candidates[0].0;
    let mut best_distance = -1i64;
    for (corner, x, y) in candidates {
        let distance = (x - cx).pow(2) + (y - cy).pow(2);
        if distance > best_distance {
            best = corner;
            best_distance = distance;
        }
    }
    best
}

/// Top-left position of a `width` x `height` window in `corner` of `area`,
/// `margin` px from both edges, kept inside `area` when it is large enough.
pub(crate) fn place_in_corner(area: RectPx, corner: Corner, width: i32, height: i32, margin: i32) -> PointPx {
    let x = match corner {
        Corner::TopLeft | Corner::BottomLeft => area.left + margin,
        Corner::TopRight | Corner::BottomRight => area.right - margin - width,
    };
    let y = match corner {
        Corner::TopLeft | Corner::TopRight => area.top + margin,
        Corner::BottomLeft | Corner::BottomRight => area.bottom - margin - height,
    };
    let x = x.min(area.right - width).max(area.left);
    let y = y.min(area.bottom - height).max(area.top);
    PointPx::new(x, y)
}

/// The four bars (top, bottom, left, right) of a ring `thickness` px wide just
/// outside `region`. A side whose bar would leave `bounds` (the monitor) is
/// drawn just inside the region instead, so the ring never spills onto a
/// neighbouring display. Bars that end up empty are returned as empty rects.
pub(crate) fn ring_bars(region: RectPx, thickness: i32, bounds: RectPx) -> [RectPx; 4] {
    let t = thickness.max(1);
    // Outer edge of each side: outside when there is room, inside otherwise.
    let top_out = region.top - t >= bounds.top;
    let bottom_out = region.bottom + t <= bounds.bottom;
    let left_out = region.left - t >= bounds.left;
    let right_out = region.right + t <= bounds.right;
    let (top0, top1) = if top_out { (region.top - t, region.top) } else { (region.top, region.top + t) };
    let (bottom0, bottom1) =
        if bottom_out { (region.bottom, region.bottom + t) } else { (region.bottom - t, region.bottom) };
    let (left0, left1) = if left_out { (region.left - t, region.left) } else { (region.left, region.left + t) };
    let (right0, right1) = if right_out { (region.right, region.right + t) } else { (region.right - t, region.right) };
    // Horizontal bars span the full outer width; vertical bars fill the gap between them.
    let outer_left = left0;
    let outer_right = right1;
    let bars = [
        RectPx::new(outer_left, top0, outer_right, top1),
        RectPx::new(outer_left, bottom0, outer_right, bottom1),
        RectPx::new(left0, top1, left1, bottom0),
        RectPx::new(right0, top1, right1, bottom0),
    ];
    bars.map(|bar| bar.intersect(&bounds))
}

/// How to align a popup menu opened at `point` on a monitor: returns
/// `(right_align, bottom_align)` so the menu opens towards the screen center
/// (up and to the left for a tray at the bottom right).
pub(crate) fn menu_alignment(point: PointPx, monitor: RectPx) -> (bool, bool) {
    let cx = (monitor.left as i64 + monitor.right as i64) / 2;
    let cy = (monitor.top as i64 + monitor.bottom as i64) / 2;
    ((point.x as i64) > cx, (point.y as i64) > cy)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: RectPx = RectPx::new(0, 0, 1920, 1040);

    #[test]
    fn the_pill_goes_to_the_far_corner() {
        assert_eq!(farthest_corner(AREA, RectPx::new(10, 10, 200, 200)), Corner::BottomRight);
        assert_eq!(farthest_corner(AREA, RectPx::new(1700, 900, 1900, 1000)), Corner::TopLeft);
        assert_eq!(farthest_corner(AREA, RectPx::new(1700, 10, 1900, 200)), Corner::BottomLeft);
        assert_eq!(farthest_corner(AREA, RectPx::new(10, 900, 200, 1000)), Corner::TopRight);
    }

    #[test]
    fn a_centered_region_prefers_bottom_right() {
        assert_eq!(farthest_corner(AREA, RectPx::new(860, 420, 1060, 620)), Corner::BottomRight);
        assert_eq!(farthest_corner(AREA, AREA), Corner::BottomRight);
    }

    #[test]
    fn corners_work_on_secondary_monitors() {
        let left_monitor = RectPx::new(-2560, -200, 0, 1240);
        assert_eq!(farthest_corner(left_monitor, RectPx::new(-2500, -150, -2000, 300)), Corner::BottomRight);
    }

    #[test]
    fn placement_keeps_the_margin() {
        assert_eq!(
            place_in_corner(AREA, Corner::BottomRight, 132, 34, 16),
            PointPx::new(1920 - 16 - 132, 1040 - 16 - 34)
        );
        assert_eq!(place_in_corner(AREA, Corner::TopLeft, 132, 34, 16), PointPx::new(16, 16));
        assert_eq!(place_in_corner(AREA, Corner::TopRight, 132, 34, 16), PointPx::new(1920 - 16 - 132, 16));
        assert_eq!(place_in_corner(AREA, Corner::BottomLeft, 132, 34, 16), PointPx::new(16, 1040 - 16 - 34));
        let offset = RectPx::new(1920, 100, 3840, 1180);
        assert_eq!(place_in_corner(offset, Corner::TopLeft, 10, 10, 20), PointPx::new(1940, 120));
    }

    #[test]
    fn placement_stays_inside_tiny_areas() {
        let tiny = RectPx::new(0, 0, 100, 40);
        assert_eq!(place_in_corner(tiny, Corner::BottomRight, 90, 34, 16), PointPx::new(0, 0));
        assert_eq!(place_in_corner(tiny, Corner::TopLeft, 90, 34, 16), PointPx::new(10, 6));
    }

    #[test]
    fn ring_sits_just_outside_the_region() {
        let monitor = RectPx::new(0, 0, 1920, 1080);
        let region = RectPx::new(100, 100, 300, 200);
        let [top, bottom, left, right] = ring_bars(region, 2, monitor);
        assert_eq!(top, RectPx::new(98, 98, 302, 100));
        assert_eq!(bottom, RectPx::new(98, 200, 302, 202));
        assert_eq!(left, RectPx::new(98, 100, 100, 200));
        assert_eq!(right, RectPx::new(300, 100, 302, 200));
    }

    #[test]
    fn ring_moves_inside_at_monitor_edges() {
        let monitor = RectPx::new(0, 0, 1920, 1080);
        let [top, bottom, left, right] = ring_bars(monitor, 2, monitor);
        assert_eq!(top, RectPx::new(0, 0, 1920, 2));
        assert_eq!(bottom, RectPx::new(0, 1078, 1920, 1080));
        assert_eq!(left, RectPx::new(0, 2, 2, 1078));
        assert_eq!(right, RectPx::new(1918, 2, 1920, 1078));
        // Only the left side touches the edge here.
        let region = RectPx::new(0, 100, 300, 200);
        let [top, _, left, right] = ring_bars(region, 3, monitor);
        assert_eq!(left, RectPx::new(0, 100, 3, 200));
        assert_eq!(top, RectPx::new(0, 97, 303, 100));
        assert_eq!(right, RectPx::new(300, 100, 303, 200));
    }

    #[test]
    fn menus_open_towards_the_screen_center() {
        let monitor = RectPx::new(0, 0, 1920, 1080);
        assert_eq!(menu_alignment(PointPx::new(1850, 1060), monitor), (true, true));
        assert_eq!(menu_alignment(PointPx::new(1850, 10), monitor), (true, false));
        assert_eq!(menu_alignment(PointPx::new(10, 1060), monitor), (false, true));
    }

    #[test]
    fn logical_lengths_scale_and_round() {
        assert_eq!(px(132.0, 1.0), 132);
        assert_eq!(px(132.0, 1.25), 165);
        assert_eq!(px(34.0, 1.5), 51);
        assert_eq!(px(2.0, 1.75), 4);
    }
}
