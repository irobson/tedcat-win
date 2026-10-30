//! Pure picture math for region recordings: where to crop a captured monitor
//! frame, how to copy it into the encoder's buffer, and the thumbnail.
//!
//! All sizes are physical pixels; pictures are BGRA, 4 bytes per pixel,
//! top-down rows. Nothing in this file touches Win32.

/// Largest thumbnail the saved card shows.
pub(crate) const THUMBNAIL_MAX_WIDTH: u32 = 184;
pub(crate) const THUMBNAIL_MAX_HEIGHT: u32 = 128;

/// Where the recorded region sits, in virtual-screen pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CropInput {
    pub region_left: i32,
    pub region_top: i32,
    pub monitor_left: i32,
    pub monitor_top: i32,
    /// Encoded picture size (the even size from the encoding plan).
    pub out_width: u32,
    pub out_height: u32,
}

/// A copy from the captured frame into the output picture: the source box
/// (frame pixels, right/bottom exclusive) lands at (`dst_x`, `dst_y`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CropCopy {
    pub src_left: u32,
    pub src_top: u32,
    pub src_right: u32,
    pub src_bottom: u32,
    pub dst_x: u32,
    pub dst_y: u32,
}

impl CropCopy {
    pub(crate) fn width(&self) -> u32 {
        self.src_right - self.src_left
    }

    pub(crate) fn height(&self) -> u32 {
        self.src_bottom - self.src_top
    }

    /// True when the copy fills the whole `width` x `height` output picture.
    pub(crate) fn covers(&self, width: u32, height: u32) -> bool {
        self.dst_x == 0 && self.dst_y == 0 && self.width() == width && self.height() == height
    }
}

/// Works out the copy for a frame whose usable content is `available_width`
/// x `available_height` pixels (the frame's `ContentSize`, clamped to the
/// texture). The region is converted from virtual-screen to monitor-relative
/// pixels; parts outside the content are left out (they stay black).
/// Returns `None` when nothing of the region is visible.
pub(crate) fn crop_copy(input: &CropInput, available_width: u32, available_height: u32) -> Option<CropCopy> {
    let left = input.region_left as i64 - input.monitor_left as i64;
    let top = input.region_top as i64 - input.monitor_top as i64;
    let right = left + input.out_width as i64;
    let bottom = top + input.out_height as i64;
    let src_left = left.max(0);
    let src_top = top.max(0);
    let src_right = right.min(available_width as i64);
    let src_bottom = bottom.min(available_height as i64);
    if src_right <= src_left || src_bottom <= src_top {
        return None;
    }
    Some(CropCopy {
        src_left: src_left as u32,
        src_top: src_top as u32,
        src_right: src_right as u32,
        src_bottom: src_bottom as u32,
        dst_x: (src_left - left) as u32,
        dst_y: (src_top - top) as u32,
    })
}

/// Copies the output picture out of a mapped staging texture.
///
/// `src` holds `height` rows of `src_pitch` bytes (the last row may be cut
/// short at `width * 4` bytes); `dst` receives tightly packed rows. Only the
/// area `copy` wrote into the staging texture is read; everything else in
/// `dst` is set to black, because the staging texture is reused between frames.
pub(crate) fn copy_picture(src: &[u8], src_pitch: usize, dst: &mut [u8], width: u32, height: u32, copy: Option<CropCopy>) {
    let row = width as usize * 4;
    let height = height as usize;
    let dst = &mut dst[..row * height];
    let Some(copy) = copy else {
        dst.fill(0);
        return;
    };
    if copy.covers(width, height as u32) {
        for (y, out) in dst.chunks_exact_mut(row).enumerate() {
            let start = y * src_pitch;
            out.copy_from_slice(&src[start..start + row]);
        }
        return;
    }
    dst.fill(0);
    let x0 = copy.dst_x as usize * 4;
    let x1 = x0 + copy.width() as usize * 4;
    let y0 = copy.dst_y as usize;
    for y in y0..(y0 + copy.height() as usize).min(height) {
        let start = y * src_pitch;
        dst[y * row + x0..y * row + x1].copy_from_slice(&src[start + x0..start + x1]);
    }
}

/// How many raw frames may wait in front of the encoder before new frames are
/// dropped. The sink writer runs unthrottled (throttling could block one
/// stream's writer while it waits for the other stream), so a software
/// encoder that cannot keep up with a large region would otherwise queue
/// frames without bound. The limit targets about 512 MB of queued pictures,
/// but never goes below 30 frames, well above any encoder's internal
/// pipeline depth, so the encoder can always make progress.
pub(crate) fn max_video_backlog(frame_bytes: usize) -> u64 {
    const BUDGET: usize = 512 * 1024 * 1024;
    (BUDGET / frame_bytes.max(1)).clamp(30, 90) as u64
}

/// The largest size with the aspect ratio of `width` x `height` that fits in
/// `max_width` x `max_height` (scaling up or down), at least 1 x 1.
pub(crate) fn fit_within(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (0, 0);
    }
    let scale = (max_width as f64 / width as f64).min(max_height as f64 / height as f64);
    let w = ((width as f64 * scale).round() as u32).clamp(1, max_width.max(1));
    let h = ((height as f64 * scale).round() as u32).clamp(1, max_height.max(1));
    (w, h)
}

/// Resizes a BGRA picture with a box filter (area average when shrinking,
/// nearest pixel when enlarging). Alpha is forced to 255.
pub(crate) fn scale_bgra(src: &[u8], src_width: u32, src_height: u32, src_stride: usize, width: u32, height: u32) -> Vec<u8> {
    let mut out = vec![0u8; width as usize * height as usize * 4];
    if src_width == 0 || src_height == 0 || width == 0 || height == 0 {
        return out;
    }
    let span = |i: u32, from: u32, to: u32| -> (usize, usize) {
        let start = (i as u64 * from as u64 / to as u64) as usize;
        let end = (((i as u64 + 1) * from as u64 / to as u64) as usize).max(start + 1).min(from as usize);
        (start, end)
    };
    for y in 0..height {
        let (sy0, sy1) = span(y, src_height, height);
        for x in 0..width {
            let (sx0, sx1) = span(x, src_width, width);
            let mut sum = [0u64; 3];
            for sy in sy0..sy1 {
                let row = &src[sy * src_stride..];
                for sx in sx0..sx1 {
                    let p = &row[sx * 4..sx * 4 + 3];
                    sum[0] += p[0] as u64;
                    sum[1] += p[1] as u64;
                    sum[2] += p[2] as u64;
                }
            }
            let count = ((sy1 - sy0) * (sx1 - sx0)) as u64;
            let o = (y as usize * width as usize + x as usize) * 4;
            for c in 0..3 {
                out[o + c] = ((sum[c] + count / 2) / count) as u8;
            }
            out[o + 3] = 255;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(region_left: i32, region_top: i32, out_width: u32, out_height: u32) -> CropInput {
        CropInput { region_left, region_top, monitor_left: 1920, monitor_top: -200, out_width, out_height }
    }

    #[test]
    fn region_is_converted_to_monitor_pixels() {
        // Second monitor to the right of the primary, positioned higher.
        let copy = crop_copy(&input(2020, -150, 400, 300), 2560, 1440).unwrap();
        assert_eq!(
            copy,
            CropCopy { src_left: 100, src_top: 50, src_right: 500, src_bottom: 350, dst_x: 0, dst_y: 0 }
        );
        assert!(copy.covers(400, 300));
    }

    #[test]
    fn region_is_clamped_to_the_frame_content() {
        // Hangs off the right and bottom edges.
        let copy = crop_copy(&input(1920 + 2400, -200 + 1400, 400, 300), 2560, 1440).unwrap();
        assert_eq!(
            copy,
            CropCopy { src_left: 2400, src_top: 1400, src_right: 2560, src_bottom: 1440, dst_x: 0, dst_y: 0 }
        );
        assert!(!copy.covers(400, 300));
        // Hangs off the left and top edges: the visible part lands offset.
        let copy = crop_copy(&input(1900, -210, 100, 50), 2560, 1440).unwrap();
        assert_eq!(copy, CropCopy { src_left: 0, src_top: 0, src_right: 80, src_bottom: 40, dst_x: 20, dst_y: 10 });
        // A smaller content size (e.g. the display mode changed) clamps too.
        let copy = crop_copy(&input(2020, -150, 400, 300), 300, 200).unwrap();
        assert_eq!((copy.src_right, copy.src_bottom), (300, 200));
    }

    #[test]
    fn region_outside_the_frame_copies_nothing() {
        assert_eq!(crop_copy(&input(100, 100, 400, 300), 2560, 1440), None);
        assert_eq!(crop_copy(&input(2020, -150, 400, 300), 0, 0), None);
    }

    fn picture(width: usize, height: usize, pitch: usize) -> Vec<u8> {
        let mut src = vec![0xEE; pitch * height];
        for y in 0..height {
            for x in 0..width * 4 {
                src[y * pitch + x] = (y * 16 + x) as u8;
            }
        }
        src
    }

    #[test]
    fn full_copy_drops_the_row_padding() {
        let src = picture(2, 3, 16);
        let mut dst = vec![0xAA; 2 * 3 * 4];
        let copy = CropCopy { src_left: 0, src_top: 0, src_right: 2, src_bottom: 3, dst_x: 0, dst_y: 0 };
        copy_picture(&src[..16 * 2 + 8], 16, &mut dst, 2, 3, Some(copy));
        for y in 0..3 {
            assert_eq!(&dst[y * 8..y * 8 + 8], &src[y * 16..y * 16 + 8]);
        }
    }

    #[test]
    fn partial_copy_blacks_out_the_rest() {
        let src = picture(3, 2, 12);
        let mut dst = vec![0xAA; 3 * 2 * 4];
        let copy = CropCopy { src_left: 0, src_top: 0, src_right: 1, src_bottom: 1, dst_x: 2, dst_y: 1 };
        copy_picture(&src, 12, &mut dst, 3, 2, Some(copy));
        let mut expected = vec![0u8; 24];
        expected[12 + 8..12 + 12].copy_from_slice(&src[12 + 8..12 + 12]);
        assert_eq!(dst, expected);

        copy_picture(&src, 12, &mut dst, 3, 2, None);
        assert!(dst.iter().all(|&b| b == 0));
    }

    #[test]
    fn backlog_limit_bounds_memory_but_never_starves_the_encoder() {
        assert_eq!(max_video_backlog(64 * 64 * 4), 90);
        assert_eq!(max_video_backlog(1920 * 1080 * 4), 64);
        assert_eq!(max_video_backlog(3840 * 2160 * 4), 30);
        assert_eq!(max_video_backlog(7680 * 4320 * 4), 30);
        assert_eq!(max_video_backlog(0), 90);
    }

    #[test]
    fn thumbnail_fits_the_card_and_keeps_the_aspect_ratio() {
        assert_eq!(fit_within(1920, 1080, 184, 128), (184, 104));
        assert_eq!(fit_within(1080, 1920, 184, 128), (72, 128));
        assert_eq!(fit_within(368, 256, 184, 128), (184, 128));
        assert_eq!(fit_within(64, 32, 184, 128), (184, 92));
        assert_eq!(fit_within(4000, 32, 184, 128), (184, 1));
        assert_eq!(fit_within(0, 32, 184, 128), (0, 0));
    }

    #[test]
    fn shrinking_averages_and_forces_opaque_alpha() {
        // 2x2 -> 1x1: average of four pixels.
        let src = [
            0, 0, 0, 0, /**/ 100, 100, 100, 0, //
            200, 200, 200, 0, /**/ 100, 40, 20, 0,
        ];
        assert_eq!(scale_bgra(&src, 2, 2, 8, 1, 1), vec![100, 85, 80, 255]);
    }

    #[test]
    fn enlarging_repeats_pixels() {
        let src = [10, 20, 30, 0, 40, 50, 60, 0];
        let out = scale_bgra(&src, 2, 1, 8, 4, 2);
        let px = |i: usize| &out[i * 4..i * 4 + 4];
        assert_eq!(px(0), [10, 20, 30, 255]);
        assert_eq!(px(1), [10, 20, 30, 255]);
        assert_eq!(px(2), [40, 50, 60, 255]);
        assert_eq!(px(7), [40, 50, 60, 255]);
    }

    #[test]
    fn scaling_respects_the_source_stride() {
        let mut src = vec![0xFF; 16 * 2];
        src[..4].copy_from_slice(&[1, 2, 3, 4]);
        src[16..20].copy_from_slice(&[5, 6, 7, 8]);
        assert_eq!(scale_bgra(&src, 1, 2, 16, 1, 2), vec![1, 2, 3, 255, 5, 6, 7, 255]);
    }
}
