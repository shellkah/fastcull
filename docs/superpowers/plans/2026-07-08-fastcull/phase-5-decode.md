# FastCull Phase 5 — Decode Pipeline — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans. Steps use `- [ ]`. Canonical types in [README.md](README.md). **Depends on Phase 1** (core crate must exist). Emits plain RGBA — zero GUI deps.

**Goal:** Turn a JPEG path plus a `TargetSize` into a straight-RGBA8 `DecodedImage` that is correctly sized (native turbojpeg scaled decode, finished with SIMD resize for `Fit`) and upright (all 8 EXIF orientations applied), plus extract the embedded EXIF thumbnail for instant filmstrip first paint — never panicking on corrupt input.

**Architecture:** `culler-core/src/decode.rs` is the last core module and is logically independent of Phases 1–4. It wraps **turbojpeg** for JPEG decode (using its native 1/1·1/2·1/4·1/8 scaled decode), **fast_image_resize** for the SIMD downscale that finishes a `Fit` request, and **kamadak-exif** for the Orientation tag and the embedded-thumbnail bytes. The pixel-reorientation step is factored into a pure `apply_orientation` fn so the highest-value logic is unit-tested with zero external files; turbojpeg round-trips (compress-then-decode synthetic buffers) and one committed real rotated fixture cover the rest.

**Tech Stack:** Rust 2021, turbojpeg (libjpeg-turbo), fast_image_resize, kamadak-exif.

## Global Constraints

Copied verbatim from [README.md](README.md); every task in this phase implicitly includes them:

- **Language / edition:** Rust, edition 2021. Workspace with two member crates: `culler-core` (lib) and `culler` (bin). This phase touches only `culler-core`.
- **`culler-core` has zero GUI dependencies.** No `slint`, no Slint types, in the library. `decode` emits plain `Vec<u8>` RGBA, never `slint::Image`.
- **Platform:** Linux only. `rustix`/`renameat2`/`statvfs` are fine to use directly; no cross-platform abstraction needed.
- **TDD, DRY, YAGNI, frequent commits.** Every task: failing test → run-it-fails → minimal impl → run-it-passes → commit. Conventional-commit messages (`feat:`, `test:`, `refactor:`).
- **Decode must NEVER panic on corrupt input.** Unreadable file → `DecodeError::Io`; corrupt/undecodable JPEG → `DecodeError::Decode(msg)` (the GUI shows a placeholder tile); non-JPEG → `DecodeError::Unsupported`. Every fallible library call is mapped with `?`/`map_err` — no `unwrap`, no `todo!()`, no panics in library code.
- **Straight (non-premultiplied) RGBA8.** `DecodedImage.rgba` is tightly packed, pitch = `w*4`, alpha uniformly `255` for opaque JPEGs. Because alpha is uniform, `fast_image_resize` needs no premultiply/unpremultiply round-trip (YAGNI).

**Canonical decode types this phase delivers** (verbatim from README — do not invent parallels):

```rust
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TargetSize { Fit(u32, u32), Full, Scaled(u8) } // Scaled(n): 1/n via turbojpeg (n in {1,2,4,8})
pub struct DecodedImage { pub w: u32, pub h: u32, pub rgba: Vec<u8> } // straight RGBA8, NOT premultiplied
#[derive(Debug)]
pub enum DecodeError { Io(std::io::Error), Decode(String), Unsupported }
pub fn decode(path: &std::path::Path, target: TargetSize) -> Result<DecodedImage, DecodeError>;
pub fn embedded_thumbnail(path: &std::path::Path) -> Option<DecodedImage>;
```

---

### Task 1: `apply_orientation` pure fn + all-8-cases unit tests

**Files:**
- Create `culler-core/src/decode.rs` (module skeleton: the three public types + private `apply_orientation` + tests).
- Modify `culler-core/src/lib.rs` (register `pub mod decode;`).

**Interfaces:** Consumes: nothing. Produces: `pub enum TargetSize`, `pub struct DecodedImage`, `pub enum DecodeError`, and the private `fn apply_orientation(rgba: Vec<u8>, w: u32, h: u32, orientation: u16) -> (Vec<u8>, u32, u32)`.

This is the highest-value pure logic in the whole module — it needs **no JPEG bytes**. EXIF orientation codes 1..=8, with 5/6/7/8 swapping width/height. The forward pixel maps mirror the well-tested `image` crate transforms (rotate90 = clockwise `put_pixel(H-1-y, x)`, rotate270 = CCW `put_pixel(y, W-1-x)`, transpose `(y,x)`, transverse `(H-1-y, W-1-x)`).

- [ ] **Step 1: Write the failing test**

Create `culler-core/src/decode.rs` containing ONLY the tests module below for now (append the rest in Step 3). Paste the whole file:

```rust
//! JPEG decode pipeline: (path, target) -> straight RGBA8, EXIF-oriented.
//! GUI-free: emits plain `Vec<u8>` RGBA, never `slint::Image`.

use std::path::Path;

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
}
```

Then register the module — add this line to `culler-core/src/lib.rs`:

```rust
pub mod decode;
```

- [ ] **Step 2: Run to verify it fails** Run: `cargo test -p culler-core apply_orientation` Expected: FAIL — compile error `cannot find function 'apply_orientation' in this scope`.

- [ ] **Step 3: Minimal implementation**

Insert the three public types and the pure `apply_orientation` fn into `culler-core/src/decode.rs`, above the `#[cfg(test)] mod tests` block:

```rust
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
        5 | 6 | 7 | 8 => (h, w),
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
```

- [ ] **Step 4: Run to verify pass** Run: `cargo test -p culler-core apply_orientation` Expected: PASS (both tests, all 8 orientation cases + identity fallbacks).

- [ ] **Step 5: Commit**

```bash
git add culler-core/src/decode.rs culler-core/src/lib.rs
git commit -m "feat(decode): add TargetSize/DecodedImage/DecodeError + pure apply_orientation with 8-case tests"
```

---

### Task 2: turbojpeg full decode into a straight-RGBA `DecodedImage`

**Files:**
- Modify `culler-core/Cargo.toml` (add `turbojpeg` dependency + `tempfile` dev-dependency).
- Modify `culler-core/src/decode.rs` (add `decompress_scaled` helper, `decode` entry point handling `TargetSize::Full`, and the round-trip test + shared test helpers).

**Interfaces:** Consumes: `DecodedImage`, `DecodeError`, `TargetSize`. Produces: `pub fn decode(path, target)` (Full only for now), private `fn decompress_scaled(jpeg: &[u8], denom: u8) -> Result<DecodedImage, DecodeError>`.

> **System dependency (one-liner):** turbojpeg links libjpeg-turbo. On Debian/Ubuntu install `sudo apt install libturbojpeg0-dev` (or, to let the crate build the lib from source, install `cmake` + `nasm`; set `TURBOJPEG_SOURCE=pkg-config` in the env to link the system lib instead of building). Confirm with `cargo build -p culler-core` before writing code.

> **turbojpeg API note:** this plan uses `turbojpeg::Decompressor::{new, read_header, set_scaling_factor, decompress}`, `turbojpeg::DecompressHeader::scaled(ScalingFactor)`, `turbojpeg::ScalingFactor::new(num, denom)`, and the `turbojpeg::Image<T>` struct (`pixels/width/pitch/height/format`, `.as_deref_mut()`). If the pinned crate version spells any of these differently (e.g. `with_scaling_factor`), run `cargo doc -p turbojpeg --open` and adjust names — the logic is unchanged.

- [ ] **Step 1: Write the failing test**

Add these shared test helpers and the round-trip test **inside** the existing `#[cfg(test)] mod tests` block in `culler-core/src/decode.rs` (later tasks reuse `synth_jpeg` / `write_temp_jpeg`):

```rust
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
        assert!(img.rgba.chunks_exact(4).all(|p| p[3] == 255), "alpha must be opaque 255");
    }
```

- [ ] **Step 2: Run to verify it fails** Run: `cargo test -p culler-core decode_full_roundtrip` Expected: FAIL — compile error `cannot find function 'decode' in this scope` (and `turbojpeg` unresolved until Cargo is updated).

- [ ] **Step 3: Minimal implementation**

Add to `culler-core/Cargo.toml`:

```toml
[dependencies]
turbojpeg = "1"

[dev-dependencies]
tempfile = "3"
```

Add to `culler-core/src/decode.rs` (below the types, above `mod tests`):

```rust
/// Decompress a JPEG at native scale 1/`denom` (denom in {1,2,4,8}) into straight RGBA8.
/// No orientation applied here. Any turbojpeg failure maps to `DecodeError::Decode`.
fn decompress_scaled(jpeg: &[u8], denom: u8) -> Result<DecodedImage, DecodeError> {
    let mut dec = turbojpeg::Decompressor::new().map_err(|e| DecodeError::Decode(e.to_string()))?;
    let header = dec.read_header(jpeg).map_err(|e| DecodeError::Decode(e.to_string()))?;
    let sf = turbojpeg::ScalingFactor::new(1, denom as usize);
    dec.set_scaling_factor(sf).map_err(|e| DecodeError::Decode(e.to_string()))?;
    let scaled = header.scaled(sf);
    let (w, h) = (scaled.width, scaled.height);
    let mut image = turbojpeg::Image {
        pixels: vec![0u8; w * h * 4],
        width: w,
        pitch: w * 4,
        height: h,
        format: turbojpeg::PixelFormat::RGBA,
    };
    dec.decompress(jpeg, image.as_deref_mut())
        .map_err(|e| DecodeError::Decode(e.to_string()))?;
    Ok(DecodedImage { w: w as u32, h: h as u32, rgba: image.pixels })
}

/// Decode `path`'s JPEG at/around `target`, returning straight RGBA8.
/// (Full only in this task; Scaled/Fit and EXIF orientation land in later tasks.)
pub fn decode(path: &Path, target: TargetSize) -> Result<DecodedImage, DecodeError> {
    let data = std::fs::read(path).map_err(DecodeError::Io)?;
    match target {
        TargetSize::Full => decompress_scaled(&data, 1),
        TargetSize::Scaled(_) | TargetSize::Fit(_, _) => {
            Err(DecodeError::Decode("target not yet implemented".to_string()))
        }
    }
}
```

- [ ] **Step 4: Run to verify pass** Run: `cargo test -p culler-core decode_full_roundtrip` Expected: PASS (dimensions 64×48, buffer length 12288, alpha opaque).

- [ ] **Step 5: Commit**

```bash
git add culler-core/Cargo.toml culler-core/src/decode.rs
git commit -m "feat(decode): turbojpeg full decode into straight RGBA DecodedImage"
```

---

### Task 3: native scaled decode for `TargetSize::Scaled(n)`

**Files:** Modify `culler-core/src/decode.rs` (extend `decode`'s `Scaled` arm; add test).

**Interfaces:** Consumes: `decompress_scaled`, `TargetSize::Scaled`. Produces: `decode(path, Scaled(n))` for `n ∈ {1,2,4,8}`; invalid `n` → `DecodeError::Decode`.

- [ ] **Step 1: Write the failing test**

Add inside `#[cfg(test)] mod tests`:

```rust
    #[test]
    fn decode_scaled_halves_dimensions() {
        let jpeg = synth_jpeg(64, 48);
        let (_dir, path) = write_temp_jpeg(&jpeg);

        let half = decode(&path, TargetSize::Scaled(2)).expect("scaled 1/2");
        assert_eq!((half.w, half.h), (32, 24));
        assert_eq!(half.rgba.len(), 32 * 24 * 4);

        let quarter = decode(&path, TargetSize::Scaled(4)).expect("scaled 1/4");
        assert_eq!((quarter.w, quarter.h), (16, 12));

        let full = decode(&path, TargetSize::Scaled(1)).expect("scaled 1/1");
        assert_eq!((full.w, full.h), (64, 48));

        // Unsupported scaling factor -> Decode error, no panic.
        assert!(matches!(decode(&path, TargetSize::Scaled(3)), Err(DecodeError::Decode(_))));
    }
```

- [ ] **Step 2: Run to verify it fails** Run: `cargo test -p culler-core decode_scaled_halves_dimensions` Expected: FAIL — `assertion failed` / returns `Err(Decode("target not yet implemented"))` for the `Scaled(2)` call.

- [ ] **Step 3: Minimal implementation**

Replace the whole `decode` fn in `culler-core/src/decode.rs` with:

```rust
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
```

- [ ] **Step 4: Run to verify pass** Run: `cargo test -p culler-core decode_scaled_halves_dimensions` Expected: PASS (1/2 → 32×24, 1/4 → 16×12, 1/1 → 64×48, 1/3 → Decode error).

- [ ] **Step 5: Commit**

```bash
git add culler-core/src/decode.rs
git commit -m "feat(decode): native scaled decode for TargetSize::Scaled(n)"
```

---

### Task 4: `Fit(w,h)` — scaled decode then `fast_image_resize` finish

**Files:**
- Modify `culler-core/Cargo.toml` (add `fast_image_resize`).
- Modify `culler-core/src/decode.rs` (add `resize_rgba` + `decode_fit`; wire the `Fit` arm; add test).

**Interfaces:** Consumes: `decompress_scaled`, `DecodedImage`, `TargetSize::Fit`. Produces: private `fn decode_fit(data: &[u8], fit_w: u32, fit_h: u32) -> Result<DecodedImage, DecodeError>`, private `fn resize_rgba(src: DecodedImage, tw: u32, th: u32) -> Result<DecodedImage, DecodeError>`.

Strategy: read the header, pick the **smallest** turbojpeg scaled level whose dims are still **≥ the fit box** (largest denom that still covers the box; never below full), decode at that level, then SIMD-downscale (aspect-preserving, never upscale) to fit inside the box. Alpha is uniform `255`, so straight (non-premultiplied) resize is correct.

- [ ] **Step 1: Write the failing test**

Add inside `#[cfg(test)] mod tests`:

```rust
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
```

- [ ] **Step 2: Run to verify it fails** Run: `cargo test -p culler-core decode_fit_downscales_within_box` Expected: FAIL — returns `Err(Decode("Fit not yet implemented"))`.

- [ ] **Step 3: Minimal implementation**

Add to `culler-core/Cargo.toml` under `[dependencies]`:

```toml
fast_image_resize = "5"
```

Add to `culler-core/src/decode.rs` (above `mod tests`):

```rust
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
    Ok(DecodedImage { w: tw, h: th, rgba: dst_img.into_vec() })
}

/// Decode into a box: smallest turbojpeg scaled level >= box, then aspect-preserving
/// SIMD downscale to fit. Never upscales. Orientation is applied by the caller.
fn decode_fit(data: &[u8], fit_w: u32, fit_h: u32) -> Result<DecodedImage, DecodeError> {
    let mut dec = turbojpeg::Decompressor::new().map_err(|e| DecodeError::Decode(e.to_string()))?;
    let header = dec.read_header(data).map_err(|e| DecodeError::Decode(e.to_string()))?;
    let (bw, bh) = (fit_w as usize, fit_h as usize);

    // Largest denom (smallest decoded image) whose scaled dims still cover the box; else full.
    let mut denom = 1u8;
    for &d in &[8u8, 4, 2, 1] {
        let s = header.scaled(turbojpeg::ScalingFactor::new(1, d as usize));
        if s.width >= bw && s.height >= bh {
            denom = d;
            break;
        }
    }

    let decoded = decompress_scaled(data, denom)?;
    let (sw, sh) = (decoded.w as f64, decoded.h as f64);
    let scale = (bw as f64 / sw).min(bh as f64 / sh).min(1.0); // never upscale
    let tw = ((sw * scale).round() as u32).max(1);
    let th = ((sh * scale).round() as u32).max(1);
    resize_rgba(decoded, tw, th)
}
```

Replace the `Fit` arm of `decode` so the match reads:

```rust
        TargetSize::Fit(w, h) => decode_fit(&data, w, h),
```

- [ ] **Step 4: Run to verify pass** Run: `cargo test -p culler-core decode_fit_downscales_within_box` Expected: PASS (Fit(32,32) → 32×24; Fit(200,200) → 64×48 unchanged).

- [ ] **Step 5: Commit**

```bash
git add culler-core/Cargo.toml culler-core/src/decode.rs
git commit -m "feat(decode): Fit target via scaled decode + fast_image_resize finish"
```

---

### Task 5: wire EXIF orientation into `decode` + real-fixture upright test

**Files:**
- Modify `culler-core/Cargo.toml` (add `kamadak-exif`).
- Create `culler-core/tests/fixtures/orientation_6.jpg` **(human/agent must supply — a real, tiny rotated JPEG; see Step 0)**.
- Create `culler-core/tests/fixtures/README.md` (provenance note).
- Modify `culler-core/src/decode.rs` (add `read_orientation`; apply orientation in `decode`'s tail; give `decode_fit` an orientation-aware box; add fixture test).

**Interfaces:** Consumes: `apply_orientation`, `decompress_scaled`, `decode_fit`. Produces: private `fn read_orientation(path: &Path) -> u16`; `decode` output is now upright for all 8 EXIF orientations.

> **kamadak-exif note:** the crate is named `kamadak-exif` in `Cargo.toml` but imported as `exif`. API used: `exif::Reader::new().read_from_container(&mut BufReader)?` → `exif::Exif`; `exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)`; `field.value.get_uint(0) -> Option<u32>`.

- [ ] **Step 0 (REQUIRED, non-code — supply the fixture; do NOT fake it):**
  Obtain **one real, tiny (<50 KB) JPEG shot in portrait with EXIF `Orientation = 6`** (a phone/camera portrait: stored landscape `w > h`, tagged 6). Verify its tag with `exiftool -Orientation -ImageWidth -ImageHeight orientation_6.jpg` (expect `Rotate 90 CW`, width > height). Commit it as `culler-core/tests/fixtures/orientation_6.jpg`. Optionally add `orientation_1.jpg` (a normal upright JPEG) for future coverage.
  Create `culler-core/tests/fixtures/README.md` documenting provenance, e.g.:

  ```
  # Test fixtures
  orientation_6.jpg — tiny portrait JPEG, EXIF Orientation=6 (stored landscape,
    displays upright rotated 90 CW). Source: <camera/phone model, own photo>.
    Downscaled to <50 KB with `magick input.jpg -resize 200x -strip-none out.jpg`
    (EXIF preserved). Used by decode.rs orientation + embedded-thumbnail tests.
  ```
  If the fixture is absent the tests in Task 5/6 fail loudly with an actionable message — that is intended; they must not silently pass.

- [ ] **Step 1: Write the failing test**

Add inside `#[cfg(test)] mod tests`:

```rust
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
        assert!(raw.w > raw.h, "orientation-6 fixture is stored landscape (w > h)");

        // decode() applies EXIF orientation -> dims swapped, portrait upright (h > w).
        let img = decode(&rotated, TargetSize::Full).expect("decode rotated");
        assert_eq!((img.w, img.h), (raw.h, raw.w), "orientation-6 swaps w/h");
        assert!(img.h > img.w, "portrait must come back upright");
        assert_eq!(img.rgba.len(), img.w as usize * img.h as usize * 4);
    }
```

- [ ] **Step 2: Run to verify it fails** Run: `cargo test -p culler-core decode_applies_exif_orientation` Expected: FAIL — dims come back landscape (`img.w == raw.w`) because `decode` does not yet apply orientation, so `assert_eq!((img.w, img.h), (raw.h, raw.w))` fails.

- [ ] **Step 3: Minimal implementation**

Add to `culler-core/Cargo.toml` under `[dependencies]`:

```toml
kamadak-exif = "0.5"
```

Add to `culler-core/src/decode.rs` (above `mod tests`):

```rust
/// Read the EXIF Orientation tag (1..=8). Returns 1 (identity) if the file is
/// unreadable or has no EXIF — orientation reading never fails a decode.
fn read_orientation(path: &Path) -> u16 {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return 1,
    };
    let mut reader = std::io::BufReader::new(file);
    match exif::Reader::new().read_from_container(&mut reader) {
        Ok(exif) => exif
            .get_field(exif::Tag::Orientation, exif::In::PRIMARY)
            .and_then(|f| f.value.get_uint(0))
            .map(|v| v as u16)
            .unwrap_or(1),
        Err(_) => 1,
    }
}
```

Replace the whole `decode` fn with the orientation-aware version (note `decode_fit` now takes the orientation so the box is measured in display space):

```rust
/// Decode `path`'s JPEG at/around `target`, apply EXIF orientation, return straight RGBA8.
pub fn decode(path: &Path, target: TargetSize) -> Result<DecodedImage, DecodeError> {
    let data = std::fs::read(path).map_err(DecodeError::Io)?;
    let orientation = read_orientation(path);
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
```

Replace `decode_fit`'s signature and box computation so the fit box is expressed in **display** orientation (swap it before decoding when the image will be rotated 90/270). The body is otherwise unchanged:

```rust
/// Decode into a display-space box: if orientation rotates the image (5/6/7/8),
/// the box is swapped so that after `apply_orientation` the result fits `fit_w x fit_h`.
/// Smallest turbojpeg scaled level >= box, then aspect-preserving SIMD downscale. Never upscales.
fn decode_fit(data: &[u8], fit_w: u32, fit_h: u32, orientation: u16) -> Result<DecodedImage, DecodeError> {
    let mut dec = turbojpeg::Decompressor::new().map_err(|e| DecodeError::Decode(e.to_string()))?;
    let header = dec.read_header(data).map_err(|e| DecodeError::Decode(e.to_string()))?;

    // Box in stored orientation: swap when the final image will be rotated 90/270.
    let rotates = matches!(orientation, 5 | 6 | 7 | 8);
    let (bw, bh) = if rotates {
        (fit_h as usize, fit_w as usize)
    } else {
        (fit_w as usize, fit_h as usize)
    };

    let mut denom = 1u8;
    for &d in &[8u8, 4, 2, 1] {
        let s = header.scaled(turbojpeg::ScalingFactor::new(1, d as usize));
        if s.width >= bw && s.height >= bh {
            denom = d;
            break;
        }
    }

    let decoded = decompress_scaled(data, denom)?;
    let (sw, sh) = (decoded.w as f64, decoded.h as f64);
    let scale = (bw as f64 / sw).min(bh as f64 / sh).min(1.0);
    let tw = ((sw * scale).round() as u32).max(1);
    let th = ((sh * scale).round() as u32).max(1);
    resize_rgba(decoded, tw, th)
}
```

(The Task-4 `decode_fit_downscales_within_box` test still passes: synthetic JPEGs carry no EXIF, so `orientation == 1`, `rotates == false`, and the box/dimension math is identical.)

- [ ] **Step 4: Run to verify pass** Run: `cargo test -p culler-core decode` Expected: PASS — `decode_applies_exif_orientation` (orientation-6 → upright, swapped w/h) plus all earlier `decode*` tests stay green.

- [ ] **Step 5: Commit**

```bash
git add culler-core/Cargo.toml culler-core/src/decode.rs culler-core/tests/fixtures/orientation_6.jpg culler-core/tests/fixtures/README.md
git commit -m "feat(decode): apply EXIF orientation so portraits decode upright"
```

---

### Task 6: `embedded_thumbnail` extraction

**Files:** Modify `culler-core/src/decode.rs` (add `is_jpeg` helper, `embedded_thumbnail`; add tests).

**Interfaces:** Consumes: `decompress_scaled`, `apply_orientation`, kamadak-exif. Produces: `pub fn embedded_thumbnail(path: &Path) -> Option<DecodedImage>`, private `fn is_jpeg(data: &[u8]) -> bool`.

The embedded EXIF thumbnail lives in the first few KB (IFD1 / `In::THUMBNAIL`): `JPEGInterchangeFormat` (offset into the TIFF buffer) + `JPEGInterchangeFormatLength`. It is stored in the sensor's raw orientation, so we orient it with the primary image's Orientation tag. Any absence/failure → `None` (spec §12).

- [ ] **Step 1: Write the failing test**

Add inside `#[cfg(test)] mod tests`:

```rust
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
        assert!(thumb.w <= 1024 && thumb.h <= 1024, "embedded thumbnails are small");
        assert_eq!(thumb.rgba.len(), thumb.w as usize * thumb.h as usize * 4);
        // Oriented like the main image: an orientation-6 portrait thumbnail is upright (h > w).
        assert!(thumb.h > thumb.w, "thumbnail must be oriented upright");
    }
```

- [ ] **Step 2: Run to verify it fails** Run: `cargo test -p culler-core embedded_thumbnail` Expected: FAIL — compile error `cannot find function 'embedded_thumbnail' in this scope`.

- [ ] **Step 3: Minimal implementation**

Add to `culler-core/src/decode.rs` (above `mod tests`):

```rust
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
```

> **kamadak-exif note:** `exif::Exif::buf()` returns the continuous TIFF-format byte buffer that `JPEGInterchangeFormat` offsets reference. If the pinned version exposes the thumbnail differently, `cargo doc -p kamadak-exif` — the slice-from-`buf()` approach is the documented one.

- [ ] **Step 4: Run to verify pass** Run: `cargo test -p culler-core embedded_thumbnail` Expected: PASS — synthetic/missing → `None`; fixture → `Some` small upright thumbnail.

- [ ] **Step 5: Commit**

```bash
git add culler-core/src/decode.rs
git commit -m "feat(decode): extract embedded EXIF thumbnail for instant filmstrip paint"
```

---

### Task 7: error handling — corrupt → Decode, non-JPEG → Unsupported, missing → Io (never panics)

**Files:** Modify `culler-core/src/decode.rs` (guard `decode` with the `is_jpeg` magic check; add error tests).

**Interfaces:** Consumes: `is_jpeg` (from Task 6), `DecodeError`. Produces: `decode` returns `Io` for unreadable files, `Unsupported` for non-JPEG bytes, `Decode(msg)` for corrupt JPEGs — and never panics.

- [ ] **Step 1: Write the failing test**

Add inside `#[cfg(test)] mod tests`:

```rust
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
        assert!(matches!(decode(missing, TargetSize::Full), Err(DecodeError::Io(_))));

        // Non-JPEG bytes (PNG magic) -> Unsupported
        let (_d1, p1) = write_temp_named("not.jpg", b"\x89PNG\r\n\x1a\n definitely not a jpeg");
        assert!(matches!(decode(&p1, TargetSize::Full), Err(DecodeError::Unsupported)));

        // Corrupt JPEG (valid SOI magic, garbage body) -> Decode, and it must NOT panic.
        let (_d2, p2) = write_temp_named("corrupt.jpg", b"\xFF\xD8\xFF\xEE\x00\x10 garbage not decodable");
        assert!(matches!(decode(&p2, TargetSize::Full), Err(DecodeError::Decode(_))));
        // Corrupt input through every target arm still returns cleanly (no panic).
        assert!(decode(&p2, TargetSize::Scaled(2)).is_err());
        assert!(decode(&p2, TargetSize::Fit(64, 64)).is_err());
    }
```

- [ ] **Step 2: Run to verify it fails** Run: `cargo test -p culler-core decode_errors_map_correctly` Expected: FAIL — the non-JPEG case returns `Err(Decode(_))` (turbojpeg's message) instead of the required `Err(Unsupported)`.

- [ ] **Step 3: Minimal implementation**

Replace the whole `decode` fn in `culler-core/src/decode.rs` with the final version that rejects non-JPEG bytes up front:

```rust
/// Decode `path`'s JPEG at/around `target`, apply EXIF orientation, return straight RGBA8.
/// Errors: unreadable file -> `Io`; non-JPEG bytes -> `Unsupported`; corrupt/undecodable
/// JPEG -> `Decode(msg)`. Never panics on bad input.
pub fn decode(path: &Path, target: TargetSize) -> Result<DecodedImage, DecodeError> {
    let data = std::fs::read(path).map_err(DecodeError::Io)?;
    if !is_jpeg(&data) {
        return Err(DecodeError::Unsupported);
    }
    let orientation = read_orientation(path);
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
```

- [ ] **Step 4: Run to verify pass** Run: `cargo test -p culler-core decode` Expected: PASS — `decode_errors_map_correctly` (Io/Unsupported/Decode all matched, no panic) plus every earlier `decode*` / `apply_orientation` / `embedded_thumbnail` test green.

- [ ] **Step 5: Commit**

```bash
git add culler-core/src/decode.rs
git commit -m "feat(decode): map errors (Io/Unsupported/Decode) and never panic on corrupt input"
```

---

## Phase 5 done — definition of complete

- `cargo test -p culler-core decode` and `cargo test -p culler-core apply_orientation` both green (assuming the `orientation_6.jpg` fixture is committed).
- Public surface delivered exactly per README: `TargetSize`, `DecodedImage`, `DecodeError`, `decode()`, `embedded_thumbnail()`.
- Straight RGBA8 out, EXIF-oriented, GUI-free (no `slint::Image`, no Slint dep).
- Corrupt/non-JPEG/missing inputs return typed errors; decode never panics.
- System dependency documented (libjpeg-turbo / `libturbojpeg0-dev`); one real rotated fixture + provenance README committed.
