//! JPEG decode pipeline: (path, target) -> straight RGBA8, EXIF-oriented.
//! GUI-free: emits plain `Vec<u8>` RGBA, never `slint::Image`.

use std::path::Path;

/// Hard cap on total pixels (width * height) a header may declare before we
/// allocate a decode buffer. A crafted/corrupt JPEG can declare arbitrary SOF
/// dimensions (up to 65535x65535) — allocating `w * h * 4` bytes for that BEFORE
/// any scan data is validated is a decompression-bomb vector: under
/// `vm.overcommit_memory=2` the allocator's `handle_alloc_error` aborts the
/// whole process (SIGABRT), which is uncatchable and would crash the Phase 6
/// GUI worker thread on a single corrupt file. 300 MP is comfortably above any
/// real camera sensor (current high-end sensors top out well under 200 MP)
/// and far below allocation-abort territory (300M * 4 bytes = 1.2 GB).
const MAX_DECODE_PIXELS: usize = 300_000_000;

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
    debug_assert!(
        matches!(denom, 1 | 2 | 4 | 8),
        "decompress_scaled denom must be one of {{1,2,4,8}} (0 would panic scaled_dim's div_ceil; \
         other values aren't a real turbojpeg scaling factor), got {denom}"
    );
    let mut dec = turbojpeg::Decompressor::new().map_err(|e| DecodeError::Decode(e.to_string()))?;
    let header = dec
        .read_header(jpeg)
        .map_err(|e| DecodeError::Decode(e.to_string()))?;

    // Reject pathological header-declared dimensions BEFORE any allocation.
    // Check the FULL (unscaled) dims: the scaled buffer we're about to
    // allocate is always <= the full one, so guarding the full dims also
    // caps every scaled request derived from this header.
    match header.width.checked_mul(header.height) {
        Some(pixels) if pixels <= MAX_DECODE_PIXELS => {}
        _ => {
            return Err(DecodeError::Decode(format!(
                "image dimensions {}x{} exceed the decode limit ({MAX_DECODE_PIXELS} pixels)",
                header.width, header.height
            )));
        }
    }

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

/// Read the EXIF Orientation tag (1..=8) from an already-parsed `exif::Exif`,
/// range-checking the raw value BEFORE narrowing to `u16`. `get_uint` widens
/// whatever integer type the tag was actually encoded as — kamadak-exif does
/// not enforce the spec's SHORT type for Orientation, so a crafted/corrupt
/// LONG-typed value like 65542 (0x00010006) must be rejected by range rather
/// than truncated (a bare `as u16` narrow would silently yield 6 = rotate
/// 90, defeating the identity fallback this function promises for invalid
/// orientation). Out-of-range or absent -> 1 (identity); `apply_orientation`
/// already treats 0 and >8 as identity too, so in-range-but-invalid values
/// still fall back correctly downstream.
fn orientation_from(exif: &exif::Exif) -> u16 {
    exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)
        .and_then(|f| f.value.get_uint(0))
        .and_then(|v| u16::try_from(v).ok())
        .unwrap_or(1)
}

/// Read the EXIF Orientation tag from the already-loaded JPEG bytes —
/// `decode` has the whole file in memory, so no second file read / reopen
/// (kamadak-exif reads from any BufRead+Seek; `Cursor<&[u8]>` qualifies,
/// verified by probe build). Returns 1 (identity) when EXIF is absent or
/// undecodable — orientation reading never fails a decode.
fn read_orientation(data: &[u8]) -> u16 {
    let mut cursor = std::io::Cursor::new(data);
    match exif::Reader::new().read_from_container(&mut cursor) {
        Ok(exif) => orientation_from(&exif),
        Err(_) => 1,
    }
}

/// Decode `path`'s JPEG at/around `target`, apply EXIF orientation, return straight RGBA8.
/// Errors: unreadable file -> `Io`; non-JPEG bytes -> `Unsupported`; corrupt/undecodable
/// JPEG -> `Decode(msg)`. Never panics on bad input.
pub fn decode(path: &Path, target: TargetSize) -> Result<DecodedImage, DecodeError> {
    let data = std::fs::read(path).map_err(DecodeError::Io)?;
    // The JPEG bytes to decode: the file itself if it's a JPEG, else a RAW's
    // embedded JPEG preview (Fuji RAF today). Neither branch copies.
    let jpeg: &[u8] = if is_jpeg(&data) {
        &data
    } else if let Some(slice) = crate::raw::embedded_jpeg(&data) {
        slice
    } else {
        return Err(DecodeError::Unsupported);
    };
    let orientation = read_orientation(jpeg);
    let decoded = match target {
        TargetSize::Full => decompress_scaled(jpeg, 1)?,
        TargetSize::Scaled(n) => match n {
            1 | 2 | 4 | 8 => decompress_scaled(jpeg, n)?,
            _ => return Err(DecodeError::Decode(format!("unsupported scale 1/{n}"))),
        },
        TargetSize::Fit(w, h) => decode_fit(jpeg, w, h, orientation)?,
    };
    let (rgba, w, h) = apply_orientation(decoded.rgba, decoded.w, decoded.h, orientation);
    Ok(DecodedImage { w, h, rgba })
}

/// True if `data` starts with the JPEG SOI + marker magic (FF D8 FF).
fn is_jpeg(data: &[u8]) -> bool {
    data.len() >= 3 && data[0] == 0xFF && data[1] == 0xD8 && data[2] == 0xFF
}

/// Extract the embedded EXIF thumbnail (fast filmstrip first paint), oriented
/// like the primary image. For a previewable RAW (Fuji RAF), the EXIF+thumbnail
/// live inside the embedded JPEG (kamadak-exif can't parse the RAF container),
/// so extract that first. Returns `None` if absent, unreadable, or undecodable.
pub fn embedded_thumbnail(path: &Path) -> Option<DecodedImage> {
    let is_raw_preview = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
        .is_some_and(crate::raw::preview_supported);

    if is_raw_preview {
        // RAF: read the file, pull the embedded JPEG, read its EXIF from memory.
        let data = std::fs::read(path).ok()?;
        let jpeg = crate::raw::embedded_jpeg(&data)?;
        let mut cursor = std::io::Cursor::new(jpeg);
        let exif = exif::Reader::new().read_from_container(&mut cursor).ok()?;
        thumbnail_from_exif(&exif)
    } else {
        // JPEG: stream the EXIF straight from the file (partial read, no full load).
        let file = std::fs::File::open(path).ok()?;
        let mut reader = std::io::BufReader::new(file);
        let exif = exif::Reader::new().read_from_container(&mut reader).ok()?;
        thumbnail_from_exif(&exif)
    }
}

/// The embedded IFD1 JPEG thumbnail from an already-parsed `Exif`, decoded and
/// EXIF-oriented. Shared by the JPEG and RAF paths of `embedded_thumbnail`.
fn thumbnail_from_exif(exif: &exif::Exif) -> Option<DecodedImage> {
    let offset = exif
        .get_field(exif::Tag::JPEGInterchangeFormat, exif::In::THUMBNAIL)?
        .value
        .get_uint(0)? as usize;
    let length = exif
        .get_field(exif::Tag::JPEGInterchangeFormatLength, exif::In::THUMBNAIL)?
        .value
        .get_uint(0)? as usize;
    let end = offset.checked_add(length)?;
    let thumb = exif.buf().get(offset..end)?;
    if !is_jpeg(thumb) {
        return None;
    }
    let decoded = decompress_scaled(thumb, 1).ok()?;
    let orientation = orientation_from(exif);
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

    /// Direct unit pin on `scaled_dim`'s ceiling rounding with odd (non-power-of-2,
    /// non-divisible) inputs, plus exact-division cases for contrast. `div_ceil` and
    /// floor `/` only diverge when there's a remainder, so exact divisions alone
    /// (as in `decode_scaled_halves_dimensions` with 64x48) can't distinguish them.
    #[test]
    fn scaled_dim_rounds_up_on_remainder() {
        assert_eq!(scaled_dim(65, 2), 33); // 32.5 -> 33, not 32
        assert_eq!(scaled_dim(47, 2), 24); // 23.5 -> 24
        assert_eq!(scaled_dim(65, 4), 17); // 16.25 -> 17
        assert_eq!(scaled_dim(47, 4), 12); // 11.75 -> 12
        assert_eq!(scaled_dim(65, 8), 9); // 8.125 -> 9
        assert_eq!(scaled_dim(47, 8), 6); // 5.875 -> 6
        assert_eq!(scaled_dim(1, 8), 1); // 0.125 -> 1, never 0
        // Exact divisions still land on the same value either way (sanity, not a
        // discriminator between div_ceil and floor division).
        assert_eq!(scaled_dim(64, 2), 32);
        assert_eq!(scaled_dim(64, 8), 8);
    }

    /// End-to-end pin: a JPEG whose dimensions (65x47) are divisible by neither 2,
    /// 4, nor 8, decoded at every scaling factor. This is the only way to observe
    /// `scaled_dim`'s ceil rounding through the real FFI path: `decompress_scaled`
    /// allocates the output buffer at exactly `scaled_dim(w, denom) x scaled_dim(h,
    /// denom)`, and turbojpeg's `tjDecompress2` picks its native DCT scaling factor
    /// from the CALLER-supplied buffer size — so a too-small (floor-rounded) buffer
    /// would make it select a coarser factor than 1/denom, and the alpha-coverage
    /// assertion (every pixel written, none left at the zero-initialized default)
    /// proves the buffer was actually filled edge-to-edge at the ceil-rounded size.
    #[test]
    fn decode_scaled_non_divisible_dimensions_use_ceil() {
        let jpeg = synth_jpeg(65, 47);
        let (_dir, path) = write_temp_jpeg(&jpeg);

        let cases: &[(u8, u32, u32)] = &[(2, 33, 24), (4, 17, 12), (8, 9, 6)];
        for &(denom, ew, eh) in cases {
            let img = decode(&path, TargetSize::Scaled(denom)).expect("scaled decode");
            assert_eq!(
                (img.w, img.h),
                (ew, eh),
                "1/{denom} scaled dims of 65x47 must be ceil-rounded"
            );
            assert_eq!(img.rgba.len(), ew as usize * eh as usize * 4);
            assert!(
                img.rgba.chunks_exact(4).all(|p| p[3] == 255),
                "1/{denom} scaled decode must fill the whole ceil-sized buffer \
                 (proves tjDecompress2 actually selected 1/{denom}, not a coarser factor)"
            );
        }
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

    /// `embedded_thumbnail` must return `None` (never panic) when the EXIF-declared
    /// thumbnail slice does not itself start with a JPEG SOI, e.g. a crafted/corrupt
    /// file whose IFD1 offset/length pointers are intact but whose thumbnail bytes
    /// were truncated, zeroed, or otherwise corrupted independently of the pointers.
    ///
    /// HONESTY NOTE on what this test does and does not prove: it pins the
    /// None-on-non-JPEG-thumbnail CONTRACT (the public behavior), not the specific
    /// `is_jpeg(thumb)` guard inside `embedded_thumbnail`. Deleting that guard
    /// would NOT make this test fail: `decompress_scaled(thumb, 1)` would then be
    /// called on the zeroed bytes, fail on the missing SOI marker, and the
    /// trailing `.ok()?` would still produce `None`. The guard is defense-in-depth
    /// at the FFI boundary (skip turbojpeg entirely on obviously-non-JPEG bytes
    /// rather than relying on it to fail closed), and this test cannot distinguish
    /// "guard fired" from "guard absent, downstream decode failed" — both paths
    /// converge on the same observable `None`. A mutation run that deleted the
    /// guard was confirmed to leave this test green (see commit body).
    #[test]
    fn embedded_thumbnail_none_on_non_jpeg_thumbnail_bytes() {
        let fx = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/orientation_6.jpg");
        let mut patched = std::fs::read(&fx).expect("read fixture");

        // Locate the embedded thumbnail's JPEG SOI. It is the SECOND `FF D8 FF`
        // occurrence in the file (the first, at offset 0, is the main image's
        // own SOI) — confirmed against the EXIF-declared offset/length via
        // kamadak-exif: JPEGInterchangeFormat is TIFF-relative (56 bytes past
        // the "Exif\0\0" marker's TIFF header, which itself starts at file
        // offset 30), landing at absolute file offset 86, which is exactly
        // where the second `FF D8 FF` sits.
        let soi_positions: Vec<usize> = patched
            .windows(3)
            .enumerate()
            .filter(|(_, w)| *w == [0xFF, 0xD8, 0xFF])
            .map(|(i, _)| i)
            .collect();
        assert!(
            soi_positions.len() >= 2,
            "fixture must contain at least two JPEG SOI markers (main image + thumbnail), \
             found {}",
            soi_positions.len()
        );
        let thumb_soi = soi_positions[1];
        assert_eq!(
            soi_positions[0], 0,
            "the first SOI must be the main image's, at file offset 0"
        );
        assert!(
            thumb_soi > 30,
            "the thumbnail SOI (offset {thumb_soi}) must land inside the EXIF APP1 \
             segment (which starts after the main image's own markers), not coincide \
             with the main image's SOI at offset 0"
        );

        // Cross-check against the EXIF-declared thumbnail pointers directly, so this
        // test doesn't rely solely on "second SOI in the file" coinciding with the
        // real EXIF-declared thumbnail by luck.
        let mut cursor = std::io::Cursor::new(&patched);
        let exif = exif::Reader::new()
            .read_from_container(&mut cursor)
            .expect("fixture must parse as a container with EXIF");
        let tiff_relative_offset =
            exif.get_field(exif::Tag::JPEGInterchangeFormat, exif::In::THUMBNAIL)
                .expect("fixture must declare a thumbnail offset")
                .value
                .get_uint(0)
                .expect("thumbnail offset must be an unsigned int") as usize;
        let needle = b"Exif\0\0";
        let exif_pos = patched
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("fixture must contain an Exif APP1 segment");
        let tiff_start = exif_pos + needle.len();
        assert_eq!(
            tiff_start + tiff_relative_offset,
            thumb_soi,
            "the second SOI in the file must be exactly where EXIF's own \
             JPEGInterchangeFormat offset points"
        );

        // Surgical patch: stomp the thumbnail's SOI + first marker byte (the bytes
        // is_jpeg() inspects) with zeros. Leaves the main image (bytes 0..86) and
        // the EXIF IFD pointers themselves completely untouched.
        patched[thumb_soi] = 0x00;
        patched[thumb_soi + 1] = 0x00;
        patched[thumb_soi + 2] = 0x00;

        let (_dir, path) = write_temp_named("non_jpeg_thumbnail.jpg", &patched);

        // The EXIF pointers must still be intact after patching (implicitly proven
        // by embedded_thumbnail reaching its is_jpeg check rather than bailing out
        // earlier on a missing/unparseable offset — a vacuous None from a broken
        // pointer would make the next assertion meaningless).
        let mut cursor2 = std::io::Cursor::new(&patched);
        let exif2 = exif::Reader::new()
            .read_from_container(&mut cursor2)
            .expect("patched bytes must still parse as a container with EXIF");
        assert!(
            exif2
                .get_field(exif::Tag::JPEGInterchangeFormat, exif::In::THUMBNAIL)
                .is_some(),
            "thumbnail offset pointer must survive the patch"
        );

        // The actual contract pin: non-JPEG thumbnail bytes -> None, no panic, no abort.
        assert!(
            embedded_thumbnail(&path).is_none(),
            "a thumbnail slice that doesn't start with a JPEG SOI must yield None"
        );

        // Proves the patch was surgical: the main image is untouched and still decodes.
        assert!(
            decode(&path, TargetSize::Full).is_ok(),
            "the main image must still decode after patching only the thumbnail bytes"
        );
    }

    fn write_temp_named(name: &str, bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    /// Binary-patch a JPEG's EXIF IFD0 Orientation entry (tag 0x0112) in place:
    /// rewrite its 12-byte IFD entry's field-type from SHORT(3) to LONG(4) and
    /// its value to 65542 (0x00010006), simulating a corrupt/crafted EXIF blob
    /// whose Orientation is nonconformingly LONG-typed. `kamadak-exif`'s
    /// `get_uint` happily widens whatever integer type the tag was actually
    /// encoded as, so a naive `as u16` narrow on this value truncates
    /// 65542 -> 6 (rotate 90), instead of the identity fallback the module
    /// contract promises for out-of-range orientation.
    ///
    /// Locates the TIFF header via the "Exif\0\0" marker inside APP1 and reads
    /// ITS OWN declared byte order ("II" little-endian vs "MM" big-endian)
    /// rather than assuming — mirrors what `kamadak-exif` itself does, so this
    /// helper stays correct regardless of which byte order a given fixture
    /// (or future replacement fixture) was encoded with.
    ///
    /// IFD entry layout (12 bytes): tag(2) type(2) count(4) value/offset(4).
    /// For type=LONG(4) count=1, the 4-byte value fits directly in the
    /// value/offset slot (no indirection) — so only the type and value fields
    /// need rewriting; tag and count are untouched.
    fn patch_orientation_to_overflowing_long(mut jpeg: Vec<u8>) -> Vec<u8> {
        const NEEDLE: &[u8; 6] = b"Exif\0\0";
        let exif_pos = jpeg
            .windows(NEEDLE.len())
            .position(|w| w == NEEDLE)
            .expect("fixture must contain an Exif APP1 segment");
        let tiff_start = exif_pos + NEEDLE.len();

        let le = match &jpeg[tiff_start..tiff_start + 2] {
            b"II" => true,
            b"MM" => false,
            other => panic!("unrecognized TIFF byte order marker: {other:?}"),
        };
        let ru16 = |b: &[u8]| -> u16 {
            let a: [u8; 2] = b[..2].try_into().unwrap();
            if le {
                u16::from_le_bytes(a)
            } else {
                u16::from_be_bytes(a)
            }
        };
        let ru32 = |b: &[u8]| -> u32 {
            let a: [u8; 4] = b[..4].try_into().unwrap();
            if le {
                u32::from_le_bytes(a)
            } else {
                u32::from_be_bytes(a)
            }
        };

        let ifd0_start = tiff_start + ru32(&jpeg[tiff_start + 4..]) as usize;
        let num_entries = ru16(&jpeg[ifd0_start..]) as usize;

        for i in 0..num_entries {
            let entry = ifd0_start + 2 + i * 12;
            if ru16(&jpeg[entry..]) == 0x0112 {
                let type_off = entry + 2;
                let value_off = entry + 8;
                jpeg[type_off..type_off + 2].copy_from_slice(&if le {
                    4u16.to_le_bytes()
                } else {
                    4u16.to_be_bytes()
                });
                jpeg[value_off..value_off + 4].copy_from_slice(&if le {
                    65542u32.to_le_bytes()
                } else {
                    65542u32.to_be_bytes()
                });
                return jpeg;
            }
        }
        panic!("Orientation tag (0x0112) not found in fixture's IFD0");
    }

    #[test]
    fn decode_and_thumbnail_reject_overflowing_long_orientation() {
        let fx = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/orientation_6.jpg");
        let original = std::fs::read(&fx).expect("read fixture");
        let patched = patch_orientation_to_overflowing_long(original);

        // Sanity: confirm the patch actually landed — kamadak-exif must read
        // back a raw (pre-narrowing) Orientation value of 65542, proving the
        // IFD entry is genuinely LONG-typed now and not still SHORT-typed 6.
        let mut cursor = std::io::Cursor::new(&patched);
        let raw_exif = exif::Reader::new()
            .read_from_container(&mut cursor)
            .expect("patched bytes must still parse as a container with EXIF");
        let raw_value = raw_exif
            .get_field(exif::Tag::Orientation, exif::In::PRIMARY)
            .and_then(|f| f.value.get_uint(0))
            .expect("Orientation field must still be present after patching");
        assert_eq!(
            raw_value, 65542,
            "patch must produce an overflowing LONG Orientation value (0x00010006)"
        );

        let (_dir, path) = write_temp_named("overflow_orientation.jpg", &patched);

        // decode(): a LONG Orientation of 65542 must NOT truncate (65542 &
        // 0xFFFF == 6) and rotate the image. It must fall back to identity,
        // returning the fixture's RAW stored dims (120x80, landscape).
        let img = decode(&path, TargetSize::Full).expect("decode patched fixture");
        assert_eq!(
            (img.w, img.h),
            (120, 80),
            "overflowing orientation must fall back to identity (raw stored dims), \
             not truncate 65542 -> 6 and rotate to 80x120"
        );
        assert!(img.w > img.h, "identity fallback must stay landscape");

        // embedded_thumbnail(): the duplicated inline lookup must reject the
        // same overflowing value, returning the thumbnail's RAW stored dims
        // (30x20, landscape), not rotate to 20x30.
        let thumb = embedded_thumbnail(&path).expect("thumbnail must still decode");
        assert_eq!(
            (thumb.w, thumb.h),
            (30, 20),
            "overflowing orientation must fall back to identity for the thumbnail too, \
             not truncate 65542 -> 6 and rotate to 20x30"
        );
        assert!(thumb.w > thumb.h, "identity fallback must stay landscape");
    }

    /// Patch a synthetic JPEG's SOF0-declared dimensions to `w`x`h`, simulating a
    /// crafted/corrupt header that lies about image size while leaving the real
    /// (small) encoded scan data untouched. Scans only up to the first SOS marker
    /// (0xFF 0xDA) so an incidental 0xFF 0xC0 byte pair inside entropy-coded scan
    /// data can never be mistaken for the SOF0 marker — `synth_jpeg`'s deterministic
    /// baseline encode always places SOF0 well before SOS, so this never false-positives.
    fn patch_sof0_dims(mut jpeg: Vec<u8>, w: u16, h: u16) -> Vec<u8> {
        let mut i = 2; // skip the SOI marker (FF D8)
        loop {
            assert!(i + 8 < jpeg.len(), "SOF0 marker not found before EOF");
            assert!(
                !(jpeg[i] == 0xFF && jpeg[i + 1] == 0xDA),
                "hit SOS before finding SOF0 marker"
            );
            if jpeg[i] == 0xFF && jpeg[i + 1] == 0xC0 {
                let (hb, wb) = (h.to_be_bytes(), w.to_be_bytes());
                jpeg[i + 5] = hb[0];
                jpeg[i + 6] = hb[1];
                jpeg[i + 7] = wb[0];
                jpeg[i + 8] = wb[1];
                return jpeg;
            }
            i += 1;
        }
    }

    #[test]
    fn decode_rejects_pathological_header_dimensions() {
        // Take a real, valid synthetic JPEG and patch just its SOF0-declared
        // dimensions to 65500x65500 — libjpeg-turbo's own per-axis ceiling
        // (JPEG_MAX_DIMENSION), so `read_header` itself accepts these dims
        // (a naive per-axis-only check would let this through). The PRODUCT
        // is ~4.29 billion pixels (~17GB RGBA at full decode), simulating a
        // crafted/corrupt header lying about image size. The actual scan data
        // is untouched and tiny (encoded for 64x48), so without our total-
        // pixel guard this would sail past `read_header` and attempt a ~17GB
        // allocation before tjDecompress2 eventually fails on the scan/
        // dimension mismatch.
        let jpeg = patch_sof0_dims(synth_jpeg(64, 48), 65500, 65500);
        let (_dir, path) = write_temp_named("bomb.jpg", &jpeg);

        // The MESSAGE matters here, not just the Err: under default Linux
        // overcommit, `vec![0u8; w*h*4]` for ~17GB succeeds as a virtual
        // mapping (no physical pages touched), so without the guard decode
        // still errors — but from tjDecompress2 AFTER the huge allocation,
        // with an unrelated message. Asserting "decode limit" pins that our
        // guard fired first, before any allocation was attempted.
        let result = decode(&path, TargetSize::Full);
        assert!(
            matches!(result, Err(DecodeError::Decode(_))),
            "expected DecodeError::Decode with a decode-limit message"
        );
        if let Err(DecodeError::Decode(msg)) = result {
            assert!(
                msg.contains("decode limit"),
                "expected the dimension guard's message, got: {msg}"
            );
            assert!(
                msg.contains("65500"),
                "message should name the offending dims: {msg}"
            );
        }

        assert!(
            decode(&path, TargetSize::Scaled(8)).is_err(),
            "must error, not crash"
        );
        assert!(
            decode(&path, TargetSize::Fit(64, 64)).is_err(),
            "must error, not crash"
        );
    }

    /// Reject side of the MAX_DECODE_PIXELS boundary: 20000 x 15001 =
    /// 300_020_000, exactly 20_000 pixels over the 300_000_000 cap. Both axes
    /// stay well under libjpeg-turbo's own per-axis ceiling of 65500 (the value
    /// `decode_rejects_pathological_header_dimensions` uses to prove read_header
    /// itself accepts dims that large), so read_header accepts this header and
    /// ONLY our guard stands between it and the allocation.
    #[test]
    fn decode_rejects_dimensions_one_pixel_over_cap() {
        let jpeg = patch_sof0_dims(synth_jpeg(64, 48), 20000, 15001);
        let (_dir, path) = write_temp_named("over_cap.jpg", &jpeg);

        match decode(&path, TargetSize::Full) {
            Err(DecodeError::Decode(msg)) => {
                assert!(
                    msg.contains("decode limit"),
                    "300_020_000 pixels (just over the cap) must trip the guard, got: {msg}"
                );
            }
            Ok(_) => panic!("expected an error for a 300_020_000-pixel header, got Ok"),
            Err(other) => {
                panic!("expected DecodeError::Decode with a decode-limit message, got {other:?}")
            }
        }
    }

    /// Accept side of the MAX_DECODE_PIXELS boundary: 20000 x 15000 =
    /// 300_000_000, exactly AT the cap. The guard's condition is
    /// `pixels <= MAX_DECODE_PIXELS`, so this must NOT trip it.
    ///
    /// The synthetic scan data is only encoded for 64x48 pixels, so decode
    /// still fails downstream once tjDecompress2 hits the header/scan
    /// mismatch — this test can't observe a successful decode at this size.
    /// What distinguishes "guard fired" from "guard passed, something else
    /// failed" is the ERROR MESSAGE: a guard-fired error contains "decode
    /// limit" (see the reject-side test above); a downstream turbojpeg
    /// failure does not. Asserting the message does NOT contain that phrase
    /// is therefore the only way to pin that the guard let this exact-cap
    /// value through.
    ///
    /// Uses `TargetSize::Scaled(8)`, not `Full`: `decompress_scaled` allocates
    /// its OUTPUT buffer at the scaled dims derived from the header
    /// (2500x1875 here, ~18.75 MB), not the full 20000x15000 dims (~1.2 GB).
    /// Full would still be guarded correctly, but there's no reason for this
    /// test's own memory footprint to be 1.2 GB when 1/8 scale exercises the
    /// exact same guard (which checks the FULL unscaled header product either
    /// way, per decompress_scaled's doc comment).
    #[test]
    fn decode_accepts_dimensions_exactly_at_cap() {
        let jpeg = patch_sof0_dims(synth_jpeg(64, 48), 20000, 15000);
        let (_dir, path) = write_temp_named("at_cap.jpg", &jpeg);

        match decode(&path, TargetSize::Scaled(8)) {
            Err(DecodeError::Decode(msg)) => {
                assert!(
                    !msg.contains("decode limit"),
                    "300_000_000 pixels (exactly at the cap, pixels <= cap) must NOT \
                     trip the guard, but got a decode-limit message: {msg}"
                );
            }
            Ok(_) => panic!(
                "expected a downstream Decode error from the header/scan size mismatch \
                 (64x48 scan data can't satisfy a 20000x15000 header), got Ok"
            ),
            Err(other) => panic!(
                "expected a downstream Decode error (header/scan mismatch, not a \
                 decode-limit rejection), got {other:?}"
            ),
        }
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

    /// Regression pin (not a RED/GREEN TDD test — the behavior under test is
    /// already correct): `decode_fit`'s box-swap-for-rotation composes correctly
    /// with EXIF orientation end to end. `orientation_6.jpg` is stored landscape
    /// 120x80 with Orientation=6, which displays upright portrait 80x120.
    /// Fitting into a 30x60 box: because orientation rotates (5..=8), `decode_fit`
    /// swaps the box to (60, 30) in STORED space before picking a turbojpeg scale
    /// denom and resizing, then `decode()` applies the orientation rotation
    /// afterward, swapping w/h back. Equivalently, in DISPLAY space: scale =
    /// min(30/80, 60/120) = 0.375 applied to the oriented 80x120 frame -> 30x45.
    #[test]
    fn fit_box_swap_composes_with_orientation_e2e() {
        let fx = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/orientation_6.jpg");
        assert!(
            fx.exists(),
            "commit culler-core/tests/fixtures/orientation_6.jpg — a 120x80 stored-landscape \
             JPEG with EXIF Orientation=6 (see fixtures/README.md)"
        );

        let img = decode(&fx, TargetSize::Fit(30, 60)).expect("decode fixture at Fit(30, 60)");
        assert_eq!(
            (img.w, img.h),
            (30, 45),
            "Fit(30, 60) on the oriented 80x120 frame must scale by 0.375 -> 30x45"
        );
        assert_eq!(img.rgba.len(), 30 * 45 * 4);
    }

    /// Wrap JPEG bytes in a minimal Fuji RAF container (magic + offset/length @84/88).
    fn wrap_raf(jpeg: &[u8]) -> Vec<u8> {
        let jpeg_off = 128usize;
        let mut raf = Vec::new();
        raf.extend_from_slice(b"FUJIFILMCCD-RAW ");
        raf.resize(jpeg_off, 0);
        raf[84..88].copy_from_slice(&(jpeg_off as u32).to_be_bytes());
        raf[88..92].copy_from_slice(&(jpeg.len() as u32).to_be_bytes());
        raf.extend_from_slice(jpeg);
        raf
    }

    #[test]
    fn decode_reads_raf_embedded_preview() {
        let jpeg = synth_jpeg(64, 48);
        let raf = wrap_raf(&jpeg);
        let (_dir, path) = write_temp_named("shot.raf", &raf);

        let full = decode(&path, TargetSize::Full).expect("decode RAF Full");
        assert_eq!((full.w, full.h), (64, 48));
        assert!(full.rgba.chunks_exact(4).all(|p| p[3] == 255));

        let fit = decode(&path, TargetSize::Fit(32, 32)).expect("decode RAF Fit");
        assert_eq!((fit.w, fit.h), (32, 24));

        let half = decode(&path, TargetSize::Scaled(2)).expect("decode RAF 1/2");
        assert_eq!((half.w, half.h), (32, 24));
    }

    #[test]
    fn decode_raf_with_no_embedded_jpeg_is_unsupported() {
        // A RAF whose preview pointer is zero-length -> embedded_jpeg None -> Unsupported.
        let jpeg = synth_jpeg(16, 16);
        let mut raf = wrap_raf(&jpeg);
        raf[88..92].copy_from_slice(&0u32.to_be_bytes());
        let (_dir, path) = write_temp_named("empty.raf", &raf);
        assert!(matches!(
            decode(&path, TargetSize::Full),
            Err(DecodeError::Unsupported)
        ));
    }

    #[test]
    fn decode_bomb_guard_fires_through_raf() {
        // The dimension guard must protect the RAF path too: a patched embedded JPEG
        // header declaring 65500x65500 must be rejected before allocation.
        let jpeg = patch_sof0_dims(synth_jpeg(64, 48), 65500, 65500);
        let raf = wrap_raf(&jpeg);
        let (_dir, path) = write_temp_named("bomb.raf", &raf);
        let result = decode(&path, TargetSize::Full);
        assert!(matches!(result, Err(DecodeError::Decode(_))));
        if let Err(DecodeError::Decode(msg)) = result {
            assert!(msg.contains("decode limit"), "guard should fire: {msg}");
        }
    }
}
