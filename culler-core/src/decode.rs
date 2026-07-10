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

/// Aspect-preserving SIMD downscale of a straight-RGBA8 buffer. No-op if already the target size.
fn resize_rgba(src: DecodedImage, tw: u32, th: u32) -> Result<DecodedImage, DecodeError> {
    use fast_image_resize::images::Image;
    use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};

    if tw == src.w && th == src.h {
        return Ok(src);
    }
    let src_img = Image::from_vec_u8(src.w, src.h, src.rgba, PixelType::U8x4)
        .map_err(|e| DecodeError::Decode(e.to_string()))?;
    let mut dst_img = Image::new(tw, th, PixelType::U8x4);
    let mut resizer = Resizer::new();
    resizer
        .resize(
            &src_img,
            &mut dst_img,
            &ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3)),
        )
        .map_err(|e| DecodeError::Decode(e.to_string()))?;
    Ok(DecodedImage {
        w: tw,
        h: th,
        rgba: dst_img.into_vec(),
    })
}

/// Largest denom in {8,4,2,1} whose 1/denom scaled dims still cover the
/// `bw x bh` box (>= both sides); 1 if none does. Pure — extracted from
/// `decode_fit` so the selection logic is unit-pinnable (the chosen denom
/// is not observable from decode()'s output dims once the resize finishes).
fn fit_denom(w: usize, h: usize, bw: usize, bh: usize) -> u8 {
    for &d in &[8u8, 4, 2, 1] {
        if scaled_dim(w, d) >= bw && scaled_dim(h, d) >= bh {
            return d;
        }
    }
    1
}

/// Decode into a display-space box: if orientation rotates the image (5/6/7/8),
/// the box is swapped so that after `apply_orientation` the result fits `fit_w x fit_h`.
/// Smallest turbojpeg scaled level >= box, then aspect-preserving SIMD downscale. Never upscales.
fn decode_fit(
    data: &[u8],
    fit_w: u32,
    fit_h: u32,
    orientation: u16,
) -> Result<DecodedImage, DecodeError> {
    let mut dec = turbojpeg::Decompressor::new().map_err(|e| DecodeError::Decode(e.to_string()))?;
    let header = dec
        .read_header(data)
        .map_err(|e| DecodeError::Decode(e.to_string()))?;

    // Box in stored orientation: swap when the final image will be rotated 90/270.
    let rotates = matches!(orientation, 5..=8);
    let (bw, bh) = if rotates {
        (fit_h as usize, fit_w as usize)
    } else {
        (fit_w as usize, fit_h as usize)
    };

    let denom = fit_denom(header.width, header.height, bw, bh);

    let decoded = decompress_scaled(data, denom)?;
    let (sw, sh) = (decoded.w as f64, decoded.h as f64);
    let scale = (bw as f64 / sw).min(bh as f64 / sh).min(1.0); // never upscale
    let tw = ((sw * scale).round() as u32).max(1);
    let th = ((sh * scale).round() as u32).max(1);
    resize_rgba(decoded, tw, th)
}

/// Read the EXIF Orientation tag (1..=8) from the already-loaded JPEG bytes —
/// `decode` has the whole file in memory, so no second file read / reopen
/// (kamadak-exif reads from any BufRead+Seek; `Cursor<&[u8]>` qualifies,
/// verified by probe build). Returns 1 (identity) when EXIF is absent or
/// undecodable — orientation reading never fails a decode.
fn read_orientation(data: &[u8]) -> u16 {
    let mut cursor = std::io::Cursor::new(data);
    match exif::Reader::new().read_from_container(&mut cursor) {
        Ok(exif) => exif
            .get_field(exif::Tag::Orientation, exif::In::PRIMARY)
            .and_then(|f| f.value.get_uint(0))
            .map(|v| v as u16)
            .unwrap_or(1),
        Err(_) => 1,
    }
}

/// Decode `path`'s JPEG at/around `target`, apply EXIF orientation, return straight RGBA8.
/// Errors: unreadable file -> `Io`; non-JPEG bytes -> `Unsupported`; corrupt/undecodable
/// JPEG -> `Decode(msg)`. Never panics on bad input.
pub fn decode(path: &Path, target: TargetSize) -> Result<DecodedImage, DecodeError> {
    let data = std::fs::read(path).map_err(DecodeError::Io)?;
    if !is_jpeg(&data) {
        return Err(DecodeError::Unsupported);
    }
    let orientation = read_orientation(&data);
    let decoded = match target {
        TargetSize::Full => decompress_scaled(&data, 1)?,
        TargetSize::Scaled(n) => match n {
            1 | 2 | 4 | 8 => decompress_scaled(&data, n)?,
            _ => return Err(DecodeError::Decode(format!("unsupported scale 1/{n}"))),
        },
        TargetSize::Fit(w, h) => decode_fit(&data, w, h, orientation)?,
    };
    let (rgba, w, h) = apply_orientation(decoded.rgba, decoded.w, decoded.h, orientation);
    Ok(DecodedImage { w, h, rgba })
}

/// True if `data` starts with the JPEG SOI + marker magic (FF D8 FF).
fn is_jpeg(data: &[u8]) -> bool {
    data.len() >= 3 && data[0] == 0xFF && data[1] == 0xD8 && data[2] == 0xFF
}

/// Extract the embedded EXIF thumbnail (fast filmstrip first paint), oriented like the
/// primary image. Returns `None` if absent, unreadable, or the thumbnail won't decode.
pub fn embedded_thumbnail(path: &Path) -> Option<DecodedImage> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let exif = exif::Reader::new().read_from_container(&mut reader).ok()?;

    let offset = exif
        .get_field(exif::Tag::JPEGInterchangeFormat, exif::In::THUMBNAIL)?
        .value
        .get_uint(0)? as usize;
    let length = exif
        .get_field(exif::Tag::JPEGInterchangeFormatLength, exif::In::THUMBNAIL)?
        .value
        .get_uint(0)? as usize;

    // Thumbnail offsets are relative to the TIFF buffer returned by `Exif::buf()`.
    let end = offset.checked_add(length)?;
    let thumb = exif.buf().get(offset..end)?;
    if !is_jpeg(thumb) {
        return None;
    }

    let decoded = decompress_scaled(thumb, 1).ok()?;
    let orientation = exif
        .get_field(exif::Tag::Orientation, exif::In::PRIMARY)
        .and_then(|f| f.value.get_uint(0))
        .map(|v| v as u16)
        .unwrap_or(1);
    let (rgba, w, h) = apply_orientation(decoded.rgba, decoded.w, decoded.h, orientation);
    Some(DecodedImage { w, h, rgba })
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

    #[test]
    fn fit_denom_pins_every_branch() {
        // d=2 scaled dims are (32,24): width 32>=32 covers, height 24>=32
        // does not -> AND binds and rejects d=2; only d=1 (64,48) covers
        // both -> denom=1. Pins the AND in the predicate.
        assert_eq!(fit_denom(64, 48, 32, 32), 1);
        // d=4 scaled dims (32,24) fail height like above; d=2 scaled dims
        // (64,48) cover both -> denom=2. Pins the d=2 branch.
        assert_eq!(fit_denom(128, 96, 32, 32), 2);
        // d=8 scaled dims (32,24) fail height; d=4 scaled dims (64,48)
        // cover both -> denom=4. Pins the d=4 branch.
        assert_eq!(fit_denom(256, 192, 32, 32), 4);
        // d=8 scaled dims (64,48) cover both on the first iteration ->
        // denom=8. Pins the d=8 branch (first-match wins, largest denom).
        assert_eq!(fit_denom(512, 384, 32, 32), 8);
        // d=8 scaled dims are exactly (32,32) == the box -> covers via
        // >=, not >. Pins the boundary condition at exact equality.
        assert_eq!(fit_denom(256, 256, 32, 32), 8);
        // Box (200,200) is larger than the image at every denom -> no
        // branch matches -> falls through to the trailing default of 1.
        assert_eq!(fit_denom(64, 48, 200, 200), 1);
    }

    #[test]
    fn decode_fit_downscales_within_box() {
        let jpeg = synth_jpeg(64, 48);
        let (_dir, path) = write_temp_jpeg(&jpeg);
        // 64x48 into a 32x32 box: aspect-preserving scale = 0.5 -> 32x24.
        let img = decode(&path, TargetSize::Fit(32, 32)).expect("fit");
        assert_eq!((img.w, img.h), (32, 24));
        assert!(img.w <= 32 && img.h <= 32, "must fit inside the box");
        assert_eq!(img.rgba.len(), 32 * 24 * 4);

        // A box bigger than the image never upscales.
        let big = decode(&path, TargetSize::Fit(200, 200)).expect("fit big");
        assert_eq!((big.w, big.h), (64, 48));
    }

    /// Belt-and-suspenders for `fit_denom_pins_every_branch`: that unit test is
    /// the actual mutation-killer for the denom-selection loop (the chosen
    /// denom is not observable from final dims once resize_rgba finishes —
    /// both denom=1 and denom=4 land on the same 32x24 output here). This
    /// test instead exercises the real FFI path (read_header + decompress_scaled
    /// at denom=4 + resize_rgba) end to end, so a regression in the turbojpeg
    /// wiring around the reduced-denom branches still shows up somewhere.
    #[test]
    fn decode_fit_reduced_denom_path() {
        let jpeg = synth_jpeg(256, 192);
        let (_dir, path) = write_temp_jpeg(&jpeg);
        // 256x192 into a 32x32 box: fit_denom picks d=4 (scaled 64x48 covers
        // the box; d=8's 32x24 fails height), then resize 64x48 -> 32x24.
        let img = decode(&path, TargetSize::Fit(32, 32)).expect("fit reduced denom");
        assert_eq!((img.w, img.h), (32, 24));
        assert_eq!(img.rgba.len(), 32 * 24 * 4);
        assert!(
            img.rgba.chunks_exact(4).all(|p| p[3] == 255),
            "alpha must be opaque 255 across the full resized buffer"
        );
    }

    #[test]
    fn decode_applies_exif_orientation() {
        let rotated = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/orientation_6.jpg");
        assert!(
            rotated.exists(),
            "commit a real orientation-6 portrait JPEG at \
             culler-core/tests/fixtures/orientation_6.jpg (see fixtures/README.md)"
        );

        // Raw stored pixels (orientation NOT applied): a portrait shot tagged 6 is
        // stored landscape, so raw w > h.
        let raw = decompress_scaled(&std::fs::read(&rotated).unwrap(), 1).unwrap();
        assert!(
            raw.w > raw.h,
            "orientation-6 fixture is stored landscape (w > h)"
        );

        // decode() applies EXIF orientation -> dims swapped, portrait upright (h > w).
        let img = decode(&rotated, TargetSize::Full).expect("decode rotated");
        assert_eq!((img.w, img.h), (raw.h, raw.w), "orientation-6 swaps w/h");
        assert!(img.h > img.w, "portrait must come back upright");
        assert_eq!(img.rgba.len(), img.w as usize * img.h as usize * 4);
    }

    #[test]
    fn embedded_thumbnail_none_when_absent() {
        // Synthetic JPEG has no EXIF and therefore no embedded thumbnail.
        let jpeg = synth_jpeg(64, 48);
        let (_dir, path) = write_temp_jpeg(&jpeg);
        assert!(embedded_thumbnail(&path).is_none());
        // Missing file -> None (never panics).
        assert!(embedded_thumbnail(std::path::Path::new("/nope/missing.jpg")).is_none());
    }

    #[test]
    fn embedded_thumbnail_extracts_from_fixture() {
        let fx = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/orientation_6.jpg");
        assert!(
            fx.exists(),
            "commit culler-core/tests/fixtures/orientation_6.jpg — phone/camera JPEGs \
             carry an embedded EXIF thumbnail (see fixtures/README.md)"
        );
        let thumb = embedded_thumbnail(&fx).expect("fixture must carry an embedded EXIF thumbnail");
        assert!(thumb.w > 0 && thumb.h > 0);
        assert!(
            thumb.w <= 1024 && thumb.h <= 1024,
            "embedded thumbnails are small"
        );
        assert_eq!(thumb.rgba.len(), thumb.w as usize * thumb.h as usize * 4);
        // Oriented like the main image: an orientation-6 portrait thumbnail is upright (h > w).
        assert!(thumb.h > thumb.w, "thumbnail must be oriented upright");
    }

    fn write_temp_named(name: &str, bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    #[test]
    fn decode_errors_map_correctly() {
        // Missing / unreadable file -> Io
        let missing = std::path::Path::new("/nonexistent/does/not/exist.jpg");
        assert!(matches!(
            decode(missing, TargetSize::Full),
            Err(DecodeError::Io(_))
        ));

        // Non-JPEG bytes (PNG magic) -> Unsupported
        let (_d1, p1) = write_temp_named("not.jpg", b"\x89PNG\r\n\x1a\n definitely not a jpeg");
        assert!(matches!(
            decode(&p1, TargetSize::Full),
            Err(DecodeError::Unsupported)
        ));

        // Corrupt JPEG (valid SOI magic, garbage body) -> Decode, and it must NOT panic.
        let (_d2, p2) = write_temp_named(
            "corrupt.jpg",
            b"\xFF\xD8\xFF\xEE\x00\x10 garbage not decodable",
        );
        assert!(matches!(
            decode(&p2, TargetSize::Full),
            Err(DecodeError::Decode(_))
        ));
        // Corrupt input through every target arm still returns cleanly (no panic).
        assert!(decode(&p2, TargetSize::Scaled(2)).is_err());
        assert!(decode(&p2, TargetSize::Fit(64, 64)).is_err());
    }
}
