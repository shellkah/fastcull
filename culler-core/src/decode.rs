//! JPEG decode pipeline: (path, target) -> straight RGBA8, EXIF-oriented.
//! GUI-free: emits plain `Vec<u8>` RGBA, never `slint::Image`.

use std::path::Path;

/// Target decode size. `Scaled(n)` = 1/n via turbojpeg native scaled decode (n in {1,2,4,8}).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TargetSize {
    Fit(u32, u32),
    Full,
    Scaled(u8),
}

/// Decoded frame. Straight (non-premultiplied) RGBA8, tightly packed (pitch = w*4).
pub struct DecodedImage {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

/// Decode failures. Corrupt input NEVER panics — it returns `Decode`.
#[derive(Debug)]
pub enum DecodeError {
    Io(std::io::Error),
    Decode(String),
    Unsupported,
}

/// Reorient a straight-RGBA8 buffer per an EXIF Orientation code (1..=8).
/// Pure: no I/O, no external deps. Codes 5/6/7/8 (90/270 rotations + diagonal
/// flips) swap width and height. Unknown/absent orientation (0 or >8) = identity.
#[allow(dead_code)]
fn apply_orientation(rgba: Vec<u8>, w: u32, h: u32, orientation: u16) -> (Vec<u8>, u32, u32) {
    if orientation <= 1 || orientation > 8 {
        return (rgba, w, h);
    }
    let (ow, oh) = match orientation {
        5..=8 => (h, w),
        _ => (w, h),
    };
    let (wu, hu, owu) = (w as usize, h as usize, ow as usize);
    let mut out = vec![0u8; owu * oh as usize * 4];
    for y in 0..hu {
        for x in 0..wu {
            // forward map: source (x,y) -> destination (ox,oy)
            let (ox, oy) = match orientation {
                2 => (wu - 1 - x, y),
                3 => (wu - 1 - x, hu - 1 - y),
                4 => (x, hu - 1 - y),
                5 => (y, x),
                6 => (hu - 1 - y, x),
                7 => (hu - 1 - y, wu - 1 - x),
                8 => (y, wu - 1 - x),
                _ => (x, y),
            };
            let si = (y * wu + x) * 4;
            let di = (oy * owu + ox) * 4;
            out[di..di + 4].copy_from_slice(&rgba[si..si + 4]);
        }
    }
    (out, ow, oh)
}

/// TurboJPEG scaled dimension: ceil(dim / denom) — the same rounding as the C
/// library's TJSCALED macro with numerator 1, so requesting exactly these
/// dimensions makes tjDecompress2 decode at exactly 1/denom.
fn scaled_dim(dim: usize, denom: u8) -> usize {
    dim.div_ceil(denom as usize)
}

/// Decompress a JPEG at native scale 1/`denom` (denom in {1,2,4,8}) into straight RGBA8.
/// No orientation applied here. Any turbojpeg failure maps to `DecodeError::Decode`.
///
/// turbojpeg 0.5 (legacy TurboJPEG API) has no explicit scaling-factor call:
/// `tjDecompress2` decodes at the largest scaling factor that fits the CALLER-
/// SUPPLIED output dimensions, and `Decompressor::decompress` passes our
/// `Image` dims straight through — so allocating the buffer at the TJSCALED
/// 1/denom dimensions selects exactly the native 1/denom DCT-scaled decode.
fn decompress_scaled(jpeg: &[u8], denom: u8) -> Result<DecodedImage, DecodeError> {
    let mut dec = turbojpeg::Decompressor::new().map_err(|e| DecodeError::Decode(e.to_string()))?;
    let header = dec
        .read_header(jpeg)
        .map_err(|e| DecodeError::Decode(e.to_string()))?;
    let (w, h) = (
        scaled_dim(header.width, denom),
        scaled_dim(header.height, denom),
    );
    let mut image = turbojpeg::Image {
        pixels: vec![0u8; w * h * 4],
        width: w,
        pitch: w * 4,
        height: h,
        format: turbojpeg::PixelFormat::RGBA,
    };
    dec.decompress(jpeg, image.as_deref_mut())
        .map_err(|e| DecodeError::Decode(e.to_string()))?;
    Ok(DecodedImage {
        w: w as u32,
        h: h as u32,
        rgba: image.pixels,
    })
}

/// Decode `path`'s JPEG at/around `target`, returning straight RGBA8.
/// (Fit and EXIF orientation land in later tasks.)
pub fn decode(path: &Path, target: TargetSize) -> Result<DecodedImage, DecodeError> {
    let data = std::fs::read(path).map_err(DecodeError::Io)?;
    match target {
        TargetSize::Full => decompress_scaled(&data, 1),
        TargetSize::Scaled(n) => match n {
            1 | 2 | 4 | 8 => decompress_scaled(&data, n),
            _ => Err(DecodeError::Decode(format!("unsupported scale 1/{n}"))),
        },
        TargetSize::Fit(_, _) => Err(DecodeError::Decode("Fit not yet implemented".to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an RGBA buffer where each pixel's R=G=B is taken from `r` (row-major), A=255.
    fn buf(r: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(r.len() * 4);
        for &x in r {
            v.extend_from_slice(&[x, x, x, 255]);
        }
        v
    }

    #[test]
    fn apply_orientation_all_eight_cases() {
        // 2x3 asymmetric source, R channel = row-major index 0..6:
        //   (0,0)=0 (1,0)=1
        //   (0,1)=2 (1,1)=3
        //   (0,2)=4 (1,2)=5
        let src = buf(&[0, 1, 2, 3, 4, 5]);
        // (orientation, expected_w, expected_h, expected_row_major_R)
        let cases: &[(u16, u32, u32, &[u8])] = &[
            (1, 2, 3, &[0, 1, 2, 3, 4, 5]), // identity
            (2, 2, 3, &[1, 0, 3, 2, 5, 4]), // mirror horizontal
            (3, 2, 3, &[5, 4, 3, 2, 1, 0]), // rotate 180
            (4, 2, 3, &[4, 5, 2, 3, 0, 1]), // mirror vertical
            (5, 3, 2, &[0, 2, 4, 1, 3, 5]), // transpose (main diagonal)
            (6, 3, 2, &[4, 2, 0, 5, 3, 1]), // rotate 90 CW
            (7, 3, 2, &[5, 3, 1, 4, 2, 0]), // transverse (anti-diagonal)
            (8, 3, 2, &[1, 3, 5, 0, 2, 4]), // rotate 90 CCW
        ];
        for &(o, ew, eh, er) in cases {
            let (out, w, h) = apply_orientation(src.clone(), 2, 3, o);
            assert_eq!((w, h), (ew, eh), "orientation {o} dims");
            assert_eq!(out, buf(er), "orientation {o} pixels");
        }
    }

    #[test]
    fn apply_orientation_unknown_is_identity() {
        let src = buf(&[0, 1, 2, 3, 4, 5]);
        let (out, w, h) = apply_orientation(src.clone(), 2, 3, 0);
        assert_eq!((w, h), (2, 3));
        assert_eq!(out, src);
        let (out9, w9, h9) = apply_orientation(src.clone(), 2, 3, 9);
        assert_eq!((w9, h9), (2, 3));
        assert_eq!(out9, src);
    }

    /// Compress a synthetic RGBA gradient to JPEG bytes (no EXIF, orientation absent).
    fn synth_jpeg(w: usize, h: usize) -> Vec<u8> {
        let mut px = vec![0u8; w * h * 4];
        for i in 0..w * h {
            px[i * 4] = (i % 256) as u8;
            px[i * 4 + 1] = 128;
            px[i * 4 + 2] = 64;
            px[i * 4 + 3] = 255;
        }
        let image = turbojpeg::Image {
            pixels: px.as_slice(),
            width: w,
            pitch: w * 4,
            height: h,
            format: turbojpeg::PixelFormat::RGBA,
        };
        let jpeg = turbojpeg::compress(image, 95, turbojpeg::Subsamp::Sub2x2).unwrap();
        jpeg[..].to_vec()
    }

    /// Write bytes to a temp file; keep the returned TempDir alive for the test's lifetime.
    fn write_temp_jpeg(bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("synthetic.jpg");
        std::fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    #[test]
    fn decode_full_roundtrip() {
        let jpeg = synth_jpeg(64, 48);
        let (_dir, path) = write_temp_jpeg(&jpeg);
        let img = decode(&path, TargetSize::Full).expect("decode Full");
        assert_eq!((img.w, img.h), (64, 48));
        assert_eq!(img.rgba.len(), 64 * 48 * 4);
        assert!(
            img.rgba.chunks_exact(4).all(|p| p[3] == 255),
            "alpha must be opaque 255"
        );
    }

    #[test]
    fn decode_scaled_halves_dimensions() {
        let jpeg = synth_jpeg(64, 48);
        let (_dir, path) = write_temp_jpeg(&jpeg);

        let half = decode(&path, TargetSize::Scaled(2)).expect("scaled 1/2");
        assert_eq!((half.w, half.h), (32, 24));
        assert_eq!(half.rgba.len(), 32 * 24 * 4);
        assert!(
            half.rgba.chunks_exact(4).all(|p| p[3] == 255),
            "1/2 scaled decode must write every pixel (alpha 255 across the zero-initialized buffer)"
        );

        let quarter = decode(&path, TargetSize::Scaled(4)).expect("scaled 1/4");
        assert_eq!((quarter.w, quarter.h), (16, 12));
        assert!(
            quarter.rgba.chunks_exact(4).all(|p| p[3] == 255),
            "1/4 scaled decode must write every pixel (alpha 255 across the zero-initialized buffer)"
        );

        let full = decode(&path, TargetSize::Scaled(1)).expect("scaled 1/1");
        assert_eq!((full.w, full.h), (64, 48));

        // Unsupported scaling factor -> Decode error, no panic.
        assert!(matches!(
            decode(&path, TargetSize::Scaled(3)),
            Err(DecodeError::Decode(_))
        ));
    }
}
