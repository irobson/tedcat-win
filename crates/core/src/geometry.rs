//! Minimal geometry in floating-point logical units.
//!
//! The app converts Windows physical pixels to logical units (pixels divided
//! by the monitor scale) before asking the core anything that depends on size,
//! so thresholds mean the same on every display.

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    /// Euclidean distance to another point.
    pub fn distance(self, other: Point) -> f64 {
        (other.x - self.x).hypot(other.y - self.y)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Size {
    pub width: f64,
    pub height: f64,
}

impl Size {
    pub const fn new(width: f64, height: f64) -> Self {
        Self { width, height }
    }
}

/// An axis-aligned rectangle. `y` grows downward, like Windows screen coordinates.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub const ZERO: Rect = Rect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 };

    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self { x, y, width, height }
    }

    /// Builds a normalized rectangle from two opposite corners, in any order.
    pub fn from_corners(a: Point, b: Point) -> Self {
        Self {
            x: a.x.min(b.x),
            y: a.y.min(b.y),
            width: (b.x - a.x).abs(),
            height: (b.y - a.y).abs(),
        }
    }

    pub fn min_x(&self) -> f64 {
        self.x
    }
    pub fn min_y(&self) -> f64 {
        self.y
    }
    pub fn max_x(&self) -> f64 {
        self.x + self.width
    }
    pub fn max_y(&self) -> f64 {
        self.y + self.height
    }
    pub fn size(&self) -> Size {
        Size::new(self.width, self.height)
    }

    /// True when either side is zero or negative.
    pub fn is_empty(&self) -> bool {
        self.width <= 0.0 || self.height <= 0.0
    }

    /// The overlap of two rectangles, or `None` when they do not overlap.
    pub fn intersection(&self, other: &Rect) -> Option<Rect> {
        let x1 = self.min_x().max(other.min_x());
        let y1 = self.min_y().max(other.min_y());
        let x2 = self.max_x().min(other.max_x());
        let y2 = self.max_y().min(other.max_y());
        if x2 < x1 || y2 < y1 {
            None
        } else {
            Some(Rect::new(x1, y1, x2 - x1, y2 - y1))
        }
    }

    /// Clamps the rectangle so it lies entirely inside `bounds`.
    /// Returns `Rect::ZERO` when there is no overlap.
    pub fn clamped(&self, bounds: &Rect) -> Rect {
        self.intersection(bounds).unwrap_or(Rect::ZERO)
    }

    /// Scales every coordinate, e.g. physical pixels to logical units.
    pub fn scaled(&self, factor: f64) -> Rect {
        Rect::new(self.x * factor, self.y * factor, self.width * factor, self.height * factor)
    }
}
