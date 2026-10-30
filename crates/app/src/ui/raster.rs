//! A tiny anti-aliased software rasterizer for the pill and the card.
//!
//! Shapes are drawn with signed distance functions into a top-down BGRA
//! buffer (the bits of a DIB section) over an opaque background; GDI then adds
//! the text, and [`Pixmap::finish_rounded`] cuts the window shape and
//! premultiplies the colors for `UpdateLayeredWindow`.

/// A color with straight (not premultiplied) alpha.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub(crate) const fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color { r, g, b, a: 255 }
    }
    pub(crate) const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color {
        Color { r, g, b, a }
    }
    /// The same color with its alpha multiplied by `factor` (0..1).
    pub(crate) fn faded(self, factor: f64) -> Color {
        Color { a: (self.a as f64 * factor.clamp(0.0, 1.0)).round() as u8, ..self }
    }
    /// GDI `COLORREF` layout (0x00BBGGRR).
    pub(crate) fn colorref(self) -> u32 {
        self.r as u32 | (self.g as u32) << 8 | (self.b as u32) << 16
    }
}

/// A rectangle in pixels, as floating point so shapes can sit between pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Area {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Area {
    pub(crate) const fn new(x: f64, y: f64, w: f64, h: f64) -> Area {
        Area { x, y, w, h }
    }
    pub(crate) fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

/// Signed distance from `(px, py)` to a rounded rectangle (negative inside).
fn rounded_rect_distance(px: f64, py: f64, area: &Area, radius: f64) -> f64 {
    let radius = radius.min(area.w / 2.0).min(area.h / 2.0).max(0.0);
    let cx = area.x + area.w / 2.0;
    let cy = area.y + area.h / 2.0;
    let qx = (px - cx).abs() - area.w / 2.0 + radius;
    let qy = (py - cy).abs() - area.h / 2.0 + radius;
    let outside = qx.max(0.0).hypot(qy.max(0.0));
    let inside = qx.max(qy).min(0.0);
    outside + inside - radius
}

/// Signed distance from `(px, py)` to a segment of the given half thickness.
fn segment_distance(px: f64, py: f64, a: (f64, f64), b: (f64, f64), half_thickness: f64) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let length_sq = dx * dx + dy * dy;
    let t = if length_sq > 0.0 { (((px - a.0) * dx + (py - a.1) * dy) / length_sq).clamp(0.0, 1.0) } else { 0.0 };
    let (nx, ny) = (a.0 + t * dx, a.1 + t * dy);
    (px - nx).hypot(py - ny) - half_thickness
}

/// Pixel coverage (0..1) for a signed distance, with a one-pixel soft edge.
fn coverage(distance: f64) -> f64 {
    (0.5 - distance).clamp(0.0, 1.0)
}

/// Undoes premultiplied alpha in place (BGRA): icons expect straight alpha.
pub(crate) fn unpremultiply(bgra: &mut [u8]) {
    for px in bgra.as_chunks_mut::<4>().0 {
        let a = px[3] as u32;
        if a == 0 {
            px[0] = 0;
            px[1] = 0;
            px[2] = 0;
        } else if a < 255 {
            for c in &mut px[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
}

/// The largest `(w, h)` with the aspect ratio of `src` that fits in `box_w` x `box_h`.
pub(crate) fn fit_inside(src_w: u32, src_h: u32, box_w: f64, box_h: f64) -> (f64, f64) {
    if src_w == 0 || src_h == 0 || box_w <= 0.0 || box_h <= 0.0 {
        return (0.0, 0.0);
    }
    let scale = (box_w / src_w as f64).min(box_h / src_h as f64);
    (src_w as f64 * scale, src_h as f64 * scale)
}

/// A mutable view of a top-down BGRA buffer.
pub(crate) struct Pixmap<'a> {
    data: &'a mut [u8],
    width: usize,
    height: usize,
}

impl<'a> Pixmap<'a> {
    /// Wraps `data`, which must hold at least `width * height * 4` bytes.
    pub(crate) fn new(data: &'a mut [u8], width: usize, height: usize) -> Pixmap<'a> {
        let height = (data.len() / 4).checked_div(width).map_or(0, |rows| height.min(rows));
        Pixmap { data, width, height }
    }

    #[cfg(test)]
    pub(crate) fn width(&self) -> usize {
        self.width
    }

    #[cfg(test)]
    pub(crate) fn height(&self) -> usize {
        self.height
    }

    /// The BGRA bytes of pixel `(x, y)`.
    #[cfg(test)]
    pub(crate) fn pixel(&self, x: usize, y: usize) -> [u8; 4] {
        let i = (y * self.width + x) * 4;
        [self.data[i], self.data[i + 1], self.data[i + 2], self.data[i + 3]]
    }

    /// Fills everything with an opaque color.
    pub(crate) fn fill(&mut self, color: Color) {
        for px in self.data[..self.width * self.height * 4].as_chunks_mut::<4>().0 {
            px.copy_from_slice(&[color.b, color.g, color.r, 255]);
        }
    }

    /// Blends `color` over pixel `(x, y)` with the given coverage (the result stays opaque).
    fn blend(&mut self, x: usize, y: usize, color: Color, cover: f64) {
        let alpha = cover * color.a as f64 / 255.0;
        if alpha <= 0.0 {
            return;
        }
        let i = (y * self.width + x) * 4;
        let mix = |dst: u8, src: u8| (src as f64 * alpha + dst as f64 * (1.0 - alpha)).round() as u8;
        self.data[i] = mix(self.data[i], color.b);
        self.data[i + 1] = mix(self.data[i + 1], color.g);
        self.data[i + 2] = mix(self.data[i + 2], color.r);
        self.data[i + 3] = 255;
    }

    /// Integer pixel bounds covering `area` grown by `pad`, clipped to the pixmap.
    fn bounds(&self, area: &Area, pad: f64) -> (usize, usize, usize, usize) {
        let x0 = (area.x - pad).floor().max(0.0) as usize;
        let y0 = (area.y - pad).floor().max(0.0) as usize;
        let x1 = ((area.x + area.w + pad).ceil().max(0.0) as usize).min(self.width);
        let y1 = ((area.y + area.h + pad).ceil().max(0.0) as usize).min(self.height);
        (x0, y0, x1, y1)
    }

    /// Draws any shape given by its signed distance function, limited to `area` (+1 px).
    fn shape(&mut self, area: Area, color: Color, distance: impl Fn(f64, f64) -> f64) {
        let (x0, y0, x1, y1) = self.bounds(&area, 1.0);
        for y in y0..y1 {
            for x in x0..x1 {
                let cover = coverage(distance(x as f64 + 0.5, y as f64 + 0.5));
                if cover > 0.0 {
                    self.blend(x, y, color, cover);
                }
            }
        }
    }

    pub(crate) fn fill_rounded_rect(&mut self, area: Area, radius: f64, color: Color) {
        self.shape(area, color, |x, y| rounded_rect_distance(x, y, &area, radius));
    }

    /// A rounded-rectangle outline of `thickness` px, drawn inside `area`.
    pub(crate) fn stroke_rounded_rect(&mut self, area: Area, radius: f64, thickness: f64, color: Color) {
        self.shape(area, color, |x, y| {
            let d = rounded_rect_distance(x, y, &area, radius);
            // Band between the edge and `thickness` inside it.
            d.max(-d - thickness)
        });
    }

    pub(crate) fn fill_circle(&mut self, cx: f64, cy: f64, radius: f64, color: Color) {
        let area = Area::new(cx - radius, cy - radius, radius * 2.0, radius * 2.0);
        self.shape(area, color, |x, y| (x - cx).hypot(y - cy) - radius);
    }

    /// A line with round caps.
    pub(crate) fn stroke_line(&mut self, a: (f64, f64), b: (f64, f64), thickness: f64, color: Color) {
        let half = thickness / 2.0;
        let area = Area::new(
            a.0.min(b.0) - half,
            a.1.min(b.1) - half,
            (a.0 - b.0).abs() + thickness,
            (a.1 - b.1).abs() + thickness,
        );
        self.shape(area, color, |x, y| segment_distance(x, y, a, b, half));
    }

    /// Draws a BGRA image (treated as opaque) scaled into `dst`, with rounded corners.
    /// Downscaling averages the source pixels under each target pixel; upscaling is bilinear.
    pub(crate) fn draw_image(&mut self, dst: Area, radius: f64, src: &[u8], src_w: usize, src_h: usize) {
        if src_w == 0 || src_h == 0 || src.len() < src_w * src_h * 4 || dst.w <= 0.0 || dst.h <= 0.0 {
            return;
        }
        let (x0, y0, x1, y1) = self.bounds(&dst, 0.0);
        let sx = src_w as f64 / dst.w;
        let sy = src_h as f64 / dst.h;
        let sample = |x: usize, y: usize| -> [f64; 3] {
            let i = (y.min(src_h - 1) * src_w + x.min(src_w - 1)) * 4;
            [src[i] as f64, src[i + 1] as f64, src[i + 2] as f64]
        };
        for y in y0..y1 {
            for x in x0..x1 {
                let (px, py) = (x as f64 + 0.5, y as f64 + 0.5);
                let cover = coverage(rounded_rect_distance(px, py, &dst, radius));
                if cover <= 0.0 {
                    continue;
                }
                // Source footprint of this target pixel.
                let fx0 = (px - 0.5 - dst.x) * sx;
                let fy0 = (py - 0.5 - dst.y) * sy;
                let rgb = if sx > 1.0 || sy > 1.0 {
                    let (ix0, iy0) = (fx0.floor().max(0.0) as usize, fy0.floor().max(0.0) as usize);
                    let ix1 = ((fx0 + sx).ceil() as usize).clamp(ix0 + 1, src_w.max(ix0 + 1));
                    let iy1 = ((fy0 + sy).ceil() as usize).clamp(iy0 + 1, src_h.max(iy0 + 1));
                    let mut sum = [0.0; 3];
                    let mut n = 0.0;
                    for yy in iy0..iy1 {
                        for xx in ix0..ix1 {
                            let s = sample(xx, yy);
                            sum[0] += s[0];
                            sum[1] += s[1];
                            sum[2] += s[2];
                            n += 1.0;
                        }
                    }
                    [sum[0] / n, sum[1] / n, sum[2] / n]
                } else {
                    let fx = ((px - dst.x) * sx - 0.5).max(0.0);
                    let fy = ((py - dst.y) * sy - 0.5).max(0.0);
                    let (ix, iy) = (fx.floor() as usize, fy.floor() as usize);
                    let (tx, ty) = (fx - ix as f64, fy - iy as f64);
                    let (a, b, c, d) = (sample(ix, iy), sample(ix + 1, iy), sample(ix, iy + 1), sample(ix + 1, iy + 1));
                    let mut out = [0.0; 3];
                    for k in 0..3 {
                        let top = a[k] * (1.0 - tx) + b[k] * tx;
                        let bottom = c[k] * (1.0 - tx) + d[k] * tx;
                        out[k] = top * (1.0 - ty) + bottom * ty;
                    }
                    out
                };
                let color = Color::rgb(rgb[2].round() as u8, rgb[1].round() as u8, rgb[0].round() as u8);
                self.blend(x, y, color, cover);
            }
        }
    }

    /// Final pass: everything outside a rounded rectangle covering the whole
    /// pixmap becomes transparent, edges are anti-aliased, and colors are
    /// premultiplied by alpha as `UpdateLayeredWindow` requires.
    pub(crate) fn finish_rounded(&mut self, radius: f64) {
        let area = Area::new(0.0, 0.0, self.width as f64, self.height as f64);
        for y in 0..self.height {
            for x in 0..self.width {
                let cover = coverage(rounded_rect_distance(x as f64 + 0.5, y as f64 + 0.5, &area, radius));
                let a = (cover * 255.0).round() as u32;
                let i = (y * self.width + x) * 4;
                for c in &mut self.data[i..i + 3] {
                    *c = ((*c as u32 * a + 127) / 255) as u8;
                }
                self.data[i + 3] = a as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canvas(w: usize, h: usize) -> Vec<u8> {
        vec![0u8; w * h * 4]
    }

    #[test]
    fn fill_is_opaque_bgra() {
        let mut buf = canvas(2, 2);
        let mut p = Pixmap::new(&mut buf, 2, 2);
        p.fill(Color::rgb(10, 20, 30));
        assert_eq!(p.pixel(1, 1), [30, 20, 10, 255]);
    }

    #[test]
    fn circles_are_solid_inside_and_soft_at_the_edge() {
        let mut buf = canvas(20, 20);
        let mut p = Pixmap::new(&mut buf, 20, 20);
        p.fill(Color::rgb(0, 0, 0));
        p.fill_circle(10.0, 10.0, 5.0, Color::rgb(255, 0, 0));
        assert_eq!(p.pixel(10, 10), [0, 0, 255, 255]);
        assert_eq!(p.pixel(0, 0), [0, 0, 0, 255]);
        // A pixel on the rim is partially covered.
        let rim = p.pixel(14, 10)[2];
        assert!(rim > 200, "rim {rim}");
        let edge = p.pixel(14, 12)[2];
        assert!(edge > 0 && edge < 255, "edge {edge}");
    }

    #[test]
    fn shapes_are_clipped_to_the_pixmap() {
        let mut buf = canvas(4, 4);
        let mut p = Pixmap::new(&mut buf, 4, 4);
        p.fill_rounded_rect(Area::new(-10.0, -10.0, 100.0, 100.0), 3.0, Color::rgb(1, 2, 3));
        p.stroke_line((-5.0, -5.0), (50.0, 50.0), 2.0, Color::rgb(9, 9, 9));
        assert_eq!(p.pixel(3, 3)[3], 255);
    }

    #[test]
    fn stroke_leaves_the_middle_untouched() {
        let mut buf = canvas(20, 20);
        let mut p = Pixmap::new(&mut buf, 20, 20);
        p.fill(Color::rgb(0, 0, 0));
        p.stroke_rounded_rect(Area::new(0.0, 0.0, 20.0, 20.0), 4.0, 1.0, Color::rgb(255, 255, 255));
        assert_eq!(p.pixel(10, 10), [0, 0, 0, 255]);
        assert_eq!(p.pixel(10, 0), [255, 255, 255, 255]);
    }

    #[test]
    fn finishing_cuts_corners_and_premultiplies() {
        let mut buf = canvas(20, 10);
        let mut p = Pixmap::new(&mut buf, 20, 10);
        p.fill(Color::rgb(200, 100, 50));
        p.finish_rounded(5.0);
        assert_eq!(p.pixel(0, 0)[3], 0);
        assert_eq!(p.pixel(0, 0)[..3], [0, 0, 0]);
        assert_eq!(p.pixel(10, 5), [50, 100, 200, 255]);
        let [b, g, r, a] = p.pixel(1, 1);
        assert!(a > 0 && a < 255);
        assert!(r <= a && g <= a && b <= a, "premultiplied: {r} {g} {b} <= {a}");
    }

    #[test]
    fn faded_colors_blend_partially() {
        let mut buf = canvas(1, 1);
        let mut p = Pixmap::new(&mut buf, 1, 1);
        p.fill(Color::rgb(0, 0, 0));
        p.fill_rounded_rect(Area::new(-1.0, -1.0, 3.0, 3.0), 0.0, Color::rgb(200, 200, 200).faded(0.5));
        assert_eq!(p.pixel(0, 0), [100, 100, 100, 255]);
    }

    #[test]
    fn images_downscale_by_averaging() {
        // 4x2 source: left half black, right half white.
        let mut src = Vec::new();
        for _y in 0..2 {
            for x in 0..4 {
                let v = if x < 2 { 0 } else { 255 };
                src.extend_from_slice(&[v, v, v, 255]);
            }
        }
        let mut buf = canvas(2, 1);
        let mut p = Pixmap::new(&mut buf, 2, 1);
        p.fill(Color::rgb(50, 50, 50));
        p.draw_image(Area::new(0.0, 0.0, 2.0, 1.0), 0.0, &src, 4, 2);
        assert_eq!(p.pixel(0, 0), [0, 0, 0, 255]);
        assert_eq!(p.pixel(1, 0), [255, 255, 255, 255]);
    }

    #[test]
    fn images_upscale_smoothly() {
        let src = [0, 0, 0, 255, 255, 255, 255, 255];
        let mut buf = canvas(4, 1);
        let mut p = Pixmap::new(&mut buf, 4, 1);
        p.draw_image(Area::new(0.0, 0.0, 4.0, 1.0), 0.0, &src, 2, 1);
        assert_eq!(p.pixel(0, 0)[0], 0);
        assert_eq!(p.pixel(3, 0)[0], 255);
        let mid = p.pixel(1, 0)[0];
        assert!(mid > 0 && mid < 255);
    }

    #[test]
    fn bad_images_are_ignored() {
        let mut buf = canvas(2, 2);
        let mut p = Pixmap::new(&mut buf, 2, 2);
        p.draw_image(Area::new(0.0, 0.0, 2.0, 2.0), 0.0, &[1, 2, 3], 2, 2);
        assert_eq!(p.pixel(0, 0), [0, 0, 0, 0]);
    }

    #[test]
    fn unpremultiply_restores_straight_alpha() {
        let mut px = [64, 32, 16, 128, 9, 9, 9, 0, 10, 20, 30, 255];
        unpremultiply(&mut px);
        assert_eq!(px, [128, 64, 32, 128, 0, 0, 0, 0, 10, 20, 30, 255]);
    }

    #[test]
    fn fit_inside_keeps_the_aspect_ratio() {
        assert_eq!(fit_inside(184, 128, 92.0, 64.0), (92.0, 64.0));
        assert_eq!(fit_inside(200, 100, 92.0, 64.0), (92.0, 46.0));
        assert_eq!(fit_inside(100, 200, 92.0, 64.0), (32.0, 64.0));
        assert_eq!(fit_inside(0, 10, 92.0, 64.0), (0.0, 0.0));
    }

    #[test]
    fn colorref_is_bgr() {
        assert_eq!(Color::rgb(0x11, 0x22, 0x33).colorref(), 0x332211);
    }

    #[test]
    fn short_buffers_are_not_overrun() {
        let mut buf = vec![0u8; 10];
        let p = Pixmap::new(&mut buf, 2, 5);
        assert_eq!(p.height(), 1);
        assert_eq!(p.width(), 2);
    }
}
