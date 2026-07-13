//! Luma histogram: a pure, GUI-free bucket count over an image's Rec.601 luma,
//! sampled for speed and normalized for direct HUD/overlay rendering.

use crate::decode::DecodedImage;

/// Visit at most roughly this many pixels; larger images are strided down to
/// this budget so the histogram stays cheap regardless of decode resolution.
const MAX_SAMPLED_PIXELS: usize = 200_000;

/// Rec.601 luma histogram of `img`, binned into `bins` buckets over 0..=255
/// (`bucket = luma * bins / 256`, clamped to `bins - 1`) and normalized so the
/// tallest bucket is exactly `1.0` (every element in `[0, 1]`).
///
/// Samples with a stride so at most ~200k pixels are ever visited, regardless
/// of image size: `stride = max(1, w*h / 200_000)`.
///
/// `bins == 0` returns an empty vec. An empty image (zero pixels), or one
/// whose sampled counts are all zero, returns `bins` zeros.
///
/// Defensive against a short/truncated `rgba` buffer: only complete RGBA
/// pixels actually present in the buffer are read, so a buffer shorter than
/// `w * h * 4` never indexes out of bounds.
pub fn luma_histogram(img: &DecodedImage, bins: usize) -> Vec<f32> {
    if bins == 0 {
        return Vec::new();
    }
    let mut counts = vec![0u32; bins];

    let total_px = img.w as usize * img.h as usize;
    let complete_px = img.rgba.len() / 4; // defensive: never read a partial trailing pixel
    let visitable_px = total_px.min(complete_px);
    let stride = (total_px / MAX_SAMPLED_PIXELS).max(1);

    let mut i = 0usize;
    while i < visitable_px {
        let o = i * 4;
        let luma = 0.299 * img.rgba[o] as f32
            + 0.587 * img.rgba[o + 1] as f32
            + 0.114 * img.rgba[o + 2] as f32;
        let bucket = ((luma * bins as f32 / 256.0) as usize).min(bins - 1);
        counts[bucket] += 1;
        i += stride;
    }

    let max = counts.iter().copied().max().unwrap_or(0);
    if max == 0 {
        return vec![0.0; bins];
    }
    counts.into_iter().map(|c| c as f32 / max as f32).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `w x h` image where every pixel is the same opaque gray value.
    fn solid(w: u32, h: u32, gray: u8) -> DecodedImage {
        let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
        for _ in 0..(w as usize * h as usize) {
            rgba.extend_from_slice(&[gray, gray, gray, 255]);
        }
        DecodedImage { w, h, rgba }
    }

    #[test]
    fn all_black_puts_all_weight_in_bucket_zero() {
        let img = solid(8, 8, 0);
        let hist = luma_histogram(&img, 16);
        assert_eq!(hist.len(), 16);
        assert_eq!(hist[0], 1.0);
        assert!(hist[1..].iter().all(|&v| v == 0.0));
    }

    #[test]
    fn all_white_puts_all_weight_in_last_bucket() {
        let img = solid(8, 8, 255);
        let hist = luma_histogram(&img, 16);
        assert_eq!(hist.len(), 16);
        assert_eq!(hist[15], 1.0);
        assert!(hist[..15].iter().all(|&v| v == 0.0));
    }

    #[test]
    fn horizontal_gradient_spreads_across_buckets() {
        let (w, h) = (256u32, 4u32);
        let mut rgba = Vec::with_capacity((w * h) as usize * 4);
        for _y in 0..h {
            for x in 0..w {
                let v = x as u8; // horizontal gradient, 0..255 left to right
                rgba.extend_from_slice(&[v, v, v, 255]);
            }
        }
        let img = DecodedImage { w, h, rgba };
        let hist = luma_histogram(&img, 16);
        assert_eq!(hist.len(), 16);
        let nonzero = hist.iter().filter(|&&v| v > 0.0).count();
        assert!(
            nonzero > 1,
            "a full-range gradient must spread across more than one bucket, got {hist:?}"
        );
    }

    #[test]
    fn length_always_equals_bins_and_max_is_one_when_nonempty() {
        for bins in [1usize, 2, 16, 30] {
            let img = solid(4, 4, 128);
            let hist = luma_histogram(&img, bins);
            assert_eq!(hist.len(), bins, "bins={bins}");
            let max = hist.iter().cloned().fold(0.0f32, f32::max);
            assert_eq!(max, 1.0, "tallest bucket must normalize to 1.0, bins={bins}");
        }
    }

    #[test]
    fn bins_zero_is_empty() {
        let img = solid(4, 4, 128);
        assert!(luma_histogram(&img, 0).is_empty());
    }

    #[test]
    fn empty_image_returns_all_zero_bins() {
        let img = DecodedImage {
            w: 0,
            h: 0,
            rgba: Vec::new(),
        };
        let hist = luma_histogram(&img, 30);
        assert_eq!(hist.len(), 30);
        assert!(hist.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn truncated_buffer_never_panics_and_ignores_the_partial_pixel() {
        let mut img = solid(8, 8, 200); // 64 complete pixels
        img.rgba.truncate(img.rgba.len() - 2); // chop off half of the last pixel
        let hist = luma_histogram(&img, 30);
        assert_eq!(hist.len(), 30);
        // All 63 remaining complete pixels are gray=200 -> bucket (200*30/256)=23.
        assert_eq!(hist[23], 1.0);
        assert!(
            hist.iter().enumerate().all(|(i, &v)| i == 23 || v == 0.0),
            "only bucket 23 should carry weight, got {hist:?}"
        );
    }

    #[test]
    fn bins_30_real_call_is_well_formed() {
        let img = solid(64, 48, 90);
        let hist = luma_histogram(&img, 30);
        assert_eq!(hist.len(), 30);
        assert_eq!(hist.iter().cloned().fold(0.0f32, f32::max), 1.0);
        assert!(hist.iter().all(|&v| (0.0..=1.0).contains(&v)));
    }
}
