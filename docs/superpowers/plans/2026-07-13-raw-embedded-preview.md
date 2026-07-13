# RAW embedded-preview (Fuji RAF) + JPEG⇄RAW toggle — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the culler preview Fuji RAF files by decoding the full-size JPEG embedded in the RAF header through the existing JPEG pipeline — making RAF-only shots cullable and adding an `r` key that toggles JPEG⇄RAW in a pair.

**Architecture:** A new pure `culler-core::raw` module locates the embedded JPEG (byte parsing, zero-copy); `decode()` gains a 3-way dispatch (JPEG / RAF-embedded / unsupported) and reuses every existing internal unchanged. `Shot.jpeg` becomes `Option`; `scan` promotes previewable RAW-only stems; the `culler` glue adds an `r` action, path routing, warning toasts, and a 3-state HUD "RAW" badge.

**Tech Stack:** Rust (edition 2024, workspace resolver 3), `turbojpeg` (system-linked, unchanged), `kamadak-exif 0.6`, Slint 1.17 (winit + Skia).

**Spec:** [`docs/superpowers/specs/2026-07-13-raw-embedded-preview-design.md`](../specs/2026-07-13-raw-embedded-preview-design.md)

## Global Constraints

- **No new dependencies.** RAF parsing is pure Rust byte reading. Do NOT add `libraw`/`rawloader`/`quickraw`/`rawler`. Decode stays on the existing `turbojpeg`.
- **`culler-core` stays GUI-free** — no `slint` import may enter it.
- **Preserve all existing behavior** — file-move/apply/session semantics unchanged beyond `Shot.jpeg` becoming optional. All existing tests (221) must stay green.
- **Sticky toggle** — `r` sets a global "prefer RAW" flag that persists across navigation (like `Z` zoom), NOT per-shot.
- **Toast `code` −1** = no color dot (all RAW toasts use −1).
- **Format gate** — only `"raf"` is previewable today; the `raw::preview_supported` predicate is the single place new formats plug in.
- **Commits** — one per task, conventional-commit style matching the repo (`feat(core): …`, `feat(ui): …`). End each commit message body with the trailer `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`.

---

### Task 1: `culler-core::raw` — RAF embedded-JPEG extraction

**Files:**
- Create: `culler-core/src/raw.rs`
- Modify: `culler-core/src/lib.rs` (add `pub mod raw;`)
- Test: inline `#[cfg(test)] mod tests` in `culler-core/src/raw.rs`

**Interfaces:**
- Produces: `culler_core::raw::preview_supported(ext_lower: &str) -> bool`; `culler_core::raw::embedded_jpeg(data: &[u8]) -> Option<&[u8]>`.

- [ ] **Step 1: Write the failing tests**

Create `culler-core/src/raw.rs` with the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal synthetic RAF: 16-byte magic, header zero-padded to `jpeg_off`,
    /// then `jpeg`; the offset(@84)/length(@88) fields point at it (big-endian).
    fn synth_raf(jpeg: &[u8], jpeg_off: usize) -> Vec<u8> {
        assert!(jpeg_off >= 92, "the offset/length header fields occupy bytes 84..92");
        let mut raf = Vec::new();
        raf.extend_from_slice(RAF_MAGIC);
        raf.resize(jpeg_off, 0);
        raf[84..88].copy_from_slice(&(jpeg_off as u32).to_be_bytes());
        raf[88..92].copy_from_slice(&(jpeg.len() as u32).to_be_bytes());
        raf.extend_from_slice(jpeg);
        raf
    }

    fn fake_jpeg() -> Vec<u8> {
        vec![0xFF, 0xD8, 0xFF, 0xEE, 0x11, 0x22, 0x33, 0xFF, 0xD9]
    }

    #[test]
    fn preview_supported_is_raf_only() {
        assert!(preview_supported("raf"));
        assert!(!preview_supported("cr3"));
        assert!(!preview_supported("nef"));
        assert!(!preview_supported("jpg"));
        assert!(!preview_supported(""));
    }

    #[test]
    fn embedded_jpeg_extracts_the_slice() {
        let jpeg = fake_jpeg();
        let raf = synth_raf(&jpeg, 128);
        assert_eq!(embedded_jpeg(&raf), Some(jpeg.as_slice()));
    }

    #[test]
    fn embedded_jpeg_rejects_non_raf_and_short_inputs() {
        assert_eq!(embedded_jpeg(b"not a raf file at all, but long enough........................................................"), None);
        assert_eq!(embedded_jpeg(b"FUJIFILMCCD-RAW "), None); // magic only, no header fields
        assert_eq!(embedded_jpeg(&[]), None);
    }

    #[test]
    fn embedded_jpeg_rejects_bad_pointers() {
        let jpeg = fake_jpeg();
        // Zero length.
        let mut zero = synth_raf(&jpeg, 128);
        zero[88..92].copy_from_slice(&0u32.to_be_bytes());
        assert_eq!(embedded_jpeg(&zero), None);
        // Offset+length past EOF.
        let mut over = synth_raf(&jpeg, 128);
        over[88..92].copy_from_slice(&9999u32.to_be_bytes());
        assert_eq!(embedded_jpeg(&over), None);
    }

    #[test]
    fn embedded_jpeg_rejects_non_jpeg_slice() {
        // Slice bytes present and in-bounds, but not a JPEG SOI.
        let not_jpeg = vec![0x00, 0x01, 0x02, 0x03];
        let raf = synth_raf(&not_jpeg, 128);
        assert_eq!(embedded_jpeg(&raf), None);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p culler-core --lib raw::`
Expected: FAIL to compile — `RAF_MAGIC`, `preview_supported`, `embedded_jpeg` not defined.

- [ ] **Step 3: Write the module**

Prepend to `culler-core/src/raw.rs` (above the test module):

```rust
//! RAW container parsing: locate the full-size JPEG preview a RAW file embeds,
//! so the existing JPEG decode path can render it. Pure, GUI-free, dependency-
//! free — byte parsing only. Fuji RAF is the only format supported today;
//! `preview_supported` is the seam where more plug in.

/// True if a RAW file with this lower-cased, no-dot extension has an embedded-
/// preview extractor. Extension-gated because `scan` groups by extension and
/// must decide cullability without opening every file; the real decode still
/// content-dispatches through `embedded_jpeg`, so a mislabeled file fails closed.
pub fn preview_supported(ext_lower: &str) -> bool {
    matches!(ext_lower, "raf")
}

/// The 16-byte magic every Fuji RAF starts with.
const RAF_MAGIC: &[u8; 16] = b"FUJIFILMCCD-RAW ";
/// Big-endian u32 embedded-JPEG *offset* field position in the RAF header.
const RAF_JPEG_OFFSET_POS: usize = 84;
/// Big-endian u32 embedded-JPEG *length* field position.
const RAF_JPEG_LENGTH_POS: usize = 88;

/// The full-size embedded JPEG a Fuji RAF carries, as a zero-copy slice of
/// `data`. `None` (never a panic) for a non-RAF file, a too-short header, an
/// out-of-bounds or zero-length preview pointer, or a slice that doesn't itself
/// start with a JPEG SOI. The returned slice is a complete JPEG (its own EXIF /
/// orientation intact), ready for `decode`'s existing JPEG internals.
pub fn embedded_jpeg(data: &[u8]) -> Option<&[u8]> {
    // Need through byte 91 for both header fields; the magic check then can't panic.
    if data.len() < RAF_JPEG_LENGTH_POS + 4 || &data[..16] != RAF_MAGIC {
        return None;
    }
    let off = u32::from_be_bytes(
        data[RAF_JPEG_OFFSET_POS..RAF_JPEG_OFFSET_POS + 4].try_into().ok()?,
    ) as usize;
    let len = u32::from_be_bytes(
        data[RAF_JPEG_LENGTH_POS..RAF_JPEG_LENGTH_POS + 4].try_into().ok()?,
    ) as usize;
    if len == 0 {
        return None;
    }
    let end = off.checked_add(len)?;
    let slice = data.get(off..end)?;
    // The embedded preview must itself be a JPEG (SOI FF D8 FF).
    if slice.len() >= 3 && slice[0] == 0xFF && slice[1] == 0xD8 && slice[2] == 0xFF {
        Some(slice)
    } else {
        None
    }
}
```

Add to `culler-core/src/lib.rs` beside the other `pub mod` lines:

```rust
pub mod raw;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p culler-core --lib raw::`
Expected: PASS (5 tests).

- [ ] **Step 5: Commit**

```bash
git add culler-core/src/raw.rs culler-core/src/lib.rs
git commit -m "feat(core): add raw module — Fuji RAF embedded-JPEG extraction"
```

---

### Task 2: `decode()` RAF dispatch + `embedded_thumbnail` RAF path

**Files:**
- Modify: `culler-core/src/decode.rs` (`decode` ~238–254; `embedded_thumbnail` ~263–288)
- Test: inline tests in `culler-core/src/decode.rs`

**Interfaces:**
- Consumes: `crate::raw::embedded_jpeg` (Task 1).
- Produces: `decode(path, target)` now accepts a Fuji RAF and returns its embedded preview as `DecodedImage`; `embedded_thumbnail(path)` handles RAF.

- [ ] **Step 1: Write the failing tests**

Add to `decode.rs`'s `#[cfg(test)] mod tests`. Reuse the existing `synth_jpeg` / `write_temp_named` helpers already in that module:

```rust
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
    assert!(matches!(decode(&path, TargetSize::Full), Err(DecodeError::Unsupported)));
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p culler-core --lib decode::tests::decode_reads_raf_embedded_preview`
Expected: FAIL — `.raf` currently returns `DecodeError::Unsupported`.

- [ ] **Step 3: Modify `decode()`**

Replace the body of `decode` (the `is_jpeg` gate + the `match target`) so it dispatches:

```rust
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
```

- [ ] **Step 4: Refactor `embedded_thumbnail` for the RAF path**

Replace `embedded_thumbnail` and factor its EXIF→thumbnail tail into a helper:

```rust
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
```

- [ ] **Step 5: Run the whole decode suite**

Run: `cargo test -p culler-core --lib decode::`
Expected: PASS — the three new RAF tests plus every existing decode test (JPEG behavior unchanged).

- [ ] **Step 6: Commit**

```bash
git add culler-core/src/decode.rs
git commit -m "feat(core): decode Fuji RAF via its embedded JPEG preview"
```

---

### Task 3: `Shot.jpeg` becomes `Option` + display helpers (workspace-wide)

This task changes a widely-used type, so it lands atomically: change the field, add helpers, and fix every construction/consumer so the whole workspace compiles and every existing test passes with **behavior unchanged**. Promotion of RAW-only stems is Task 4.

**Files:**
- Modify: `culler-core/src/model.rs` (`Shot` struct ~178–191; `files()` ~195–205; helpers; test fixtures)
- Modify: `culler-core/src/scan.rs` (production `Shot` push ~111; `sort_key` ~303)
- Modify: `culler-core/src/persist.rs` (test fixture ~127)
- Modify: `culler/src/ui.rs` (`hud_text` filename ~192–196; test fixtures ~269, ~453)
- Modify: `culler/src/input.rs` (test fixtures ~373, ~586)
- Modify: `culler/src/startup.rs` (test fixture ~508)
- Modify: `culler/src/applyflow.rs` (test fixture ~830)
- Modify: `culler/src/main.rs` (`request_current` enqueues ~259, ~269)

**Interfaces:**
- Produces: `Shot.jpeg: Option<PathBuf>`; `Shot::display_path(&self) -> &Path`; `Shot::is_raw_only(&self) -> bool`. Invariant (enforced by `scan`): at least one of `jpeg`/`raw` is `Some`.

- [ ] **Step 1: Write the failing tests (model.rs)**

Add to `model.rs`'s `#[cfg(test)] mod tests`:

```rust
#[test]
fn display_path_prefers_jpeg_then_raw() {
    let pair = Shot {
        stem: "IMG".into(),
        jpeg: Some("/s/IMG.JPG".into()),
        raw: Some("/s/IMG.RAF".into()),
        sidecar: None,
        capture: CaptureTime::default(),
        exif: None,
    };
    assert_eq!(pair.display_path(), std::path::Path::new("/s/IMG.JPG"));
    assert!(!pair.is_raw_only());

    let raw_only = Shot {
        stem: "IMG".into(),
        jpeg: None,
        raw: Some("/s/IMG.RAF".into()),
        sidecar: None,
        capture: CaptureTime::default(),
        exif: None,
    };
    assert_eq!(raw_only.display_path(), std::path::Path::new("/s/IMG.RAF"));
    assert!(raw_only.is_raw_only());
}

#[test]
fn files_lists_raw_first_when_no_jpeg() {
    let raw_only = Shot {
        stem: "IMG".into(),
        jpeg: None,
        raw: Some("/s/IMG.RAF".into()),
        sidecar: Some("/s/IMG.RAF.xmp".into()),
        capture: CaptureTime::default(),
        exif: None,
    };
    assert_eq!(
        raw_only.files(),
        vec![
            std::path::PathBuf::from("/s/IMG.RAF"),
            std::path::PathBuf::from("/s/IMG.RAF.xmp"),
        ]
    );
}

#[test]
fn shot_jpeg_none_serde_round_trips() {
    let shot = Shot {
        stem: "IMG".into(),
        jpeg: None,
        raw: Some("/s/IMG.RAF".into()),
        sidecar: None,
        capture: CaptureTime::default(),
        exif: None,
    };
    let json = serde_json::to_string(&shot).unwrap();
    let back: Shot = serde_json::from_str(&json).unwrap();
    assert_eq!(back, shot);
    // An old session with a present jpeg string still loads as Some.
    let old = r#"{"stem":"IMG","jpeg":"/s/IMG.JPG","raw":null,"sidecar":null,"capture":{"datetime":null,"subsec":null}}"#;
    let s: Shot = serde_json::from_str(old).unwrap();
    assert_eq!(s.jpeg, Some(std::path::PathBuf::from("/s/IMG.JPG")));
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p culler-core --lib model::tests::display_path_prefers_jpeg_then_raw`
Expected: FAIL to compile — `jpeg: Some(...)`, `display_path`, `is_raw_only` don't exist yet.

- [ ] **Step 3: Change the field + add helpers (model.rs)**

In the `Shot` struct, change the `jpeg` field and add `#[serde(default)]`:

```rust
    #[serde(default)]
    pub jpeg: Option<std::path::PathBuf>, // display JPEG, if the stem has one
```

Update `files()` to push `jpeg` only when present:

```rust
    pub fn files(&self) -> Vec<std::path::PathBuf> {
        let mut out = Vec::with_capacity(3);
        if let Some(jpeg) = &self.jpeg {
            out.push(jpeg.clone());
        }
        if let Some(raw) = &self.raw {
            out.push(raw.clone());
        }
        if let Some(sidecar) = &self.sidecar {
            out.push(sidecar.clone());
        }
        out
    }

    /// The file the loupe decodes by default: the JPEG if present, else the RAW.
    /// Never panics — `scan` guarantees at least one of jpeg/raw is present.
    pub fn display_path(&self) -> &std::path::Path {
        self.jpeg
            .as_deref()
            .or(self.raw.as_deref())
            .expect("Shot invariant: at least one of jpeg/raw is present")
    }

    /// True when this shot has no JPEG (shown via its RAW's embedded preview).
    pub fn is_raw_only(&self) -> bool {
        self.jpeg.is_none()
    }
```

- [ ] **Step 4: Wrap every `Shot { jpeg: … }` construction in `Some(…)`**

These are all fixtures/production sites where `jpeg:` takes a `PathBuf`. Wrap each RHS in `Some(…)`. Exact sites:

- `culler-core/src/model.rs` tests: `shot_files_lists_jpeg_only_when_no_siblings`, `shot_files_orders_jpeg_then_raw_then_sidecar`, `session_serde_round_trips_and_skips_undo`, `session_decision_returns_stored_default_when_absent`, `session_decision_returns_stored_value_when_present`, `fixture_session`. Example — `fixture_session`:
  ```rust
  jpeg: Some(std::path::PathBuf::from(format!("/src/{stem}.JPG"))),
  ```
- `culler-core/src/scan.rs` **production** — the `Some(jpeg)` match arm's push (~111):
  ```rust
  shots.push(Shot { stem, jpeg: Some(jpeg), raw: group.raw, sidecar: group.sidecar, capture, exif });
  ```
- `culler-core/src/persist.rs` ~127: `jpeg: Some(dir.join("IMG_0001.JPG")),`
- `culler/src/ui.rs` tests `mk` (~269) and `mk` (~453): `jpeg: Some(format!("/s/{stem}.JPG").into()),`
- `culler/src/input.rs` tests `mk_session` (~373) and `mk_session` (~586): `jpeg: Some(std::path::PathBuf::from(format!("/src/{stem}.JPG"))),`
- `culler/src/startup.rs` ~508: `jpeg: Some(dir.join(format!("{stem}.JPG"))),`
- `culler/src/applyflow.rs` ~830: `jpeg: Some(dir.join(format!("{stem}.JPG"))),`

- [ ] **Step 5: Fix the production consumers to use `display_path()`**

- `culler-core/src/scan.rs` `sort_key` (~303): `file_name_string(&shot.jpeg)` → `file_name_string(shot.display_path())`.
- `culler/src/ui.rs` `hud_text` filename (~192–195):
  ```rust
  let filename = current_shot
      .map(|s| s.display_path())
      .and_then(|p| p.file_name())
      .map(|n| n.to_string_lossy().into_owned())
      .unwrap_or_default();
  ```
- `culler/src/main.rs` `request_current` — both enqueues (~259 and ~269):
  ```rust
  pipeline.enqueue(cur, s.shots[cur].display_path().to_path_buf(), z.target(fw, fh), false);
  ```
  ```rust
  pipeline.enqueue(idx, s.shots[idx].display_path().to_path_buf(), TargetSize::Fit(fw, fh), true);
  ```

- [ ] **Step 6: Run the whole workspace test suite**

Run: `cargo test`
Expected: PASS — all existing tests plus the 3 new model tests. (If anything fails to compile, it's a missed `Some(…)` wrap — the compiler names the file:line.)

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "feat(core): make Shot.jpeg optional with display_path()/is_raw_only()"
```

---

### Task 4: `scan` — promote previewable RAW-only stems + RAF capture-time/EXIF

**Files:**
- Modify: `culler-core/src/scan.rs` (`scan_report` `None` arm ~120–128; `read_exif_data` ~188–202; add helpers)
- Test: inline tests in `culler-core/src/scan.rs`

**Interfaces:**
- Consumes: `crate::raw::preview_supported`, `crate::raw::embedded_jpeg` (Task 1); `Shot { jpeg: None, … }` (Task 3).
- Produces: RAF-only stems become `Shot`s (`jpeg: None`), with capture time + `ExifSummary` read from the embedded JPEG. Non-previewable RAW-only stems still report via `raw_only`.

- [ ] **Step 1: Write the failing tests**

Add to `scan.rs`'s `#[cfg(test)] mod tests`. It already has `jpeg_with_exif(datetime, subsec)`; add a RAF wrapper beside it:

```rust
/// Wrap JPEG bytes (e.g. from `jpeg_with_exif`) in a minimal Fuji RAF container.
fn wrap_raf(jpeg: &[u8]) -> Vec<u8> {
    let off = 128usize;
    let mut raf = Vec::new();
    raf.extend_from_slice(b"FUJIFILMCCD-RAW ");
    raf.resize(off, 0);
    raf[84..88].copy_from_slice(&(off as u32).to_be_bytes());
    raf[88..92].copy_from_slice(&(jpeg.len() as u32).to_be_bytes());
    raf.extend_from_slice(jpeg);
    raf
}

#[test]
fn raf_only_stem_is_promoted_with_embedded_capture_time() {
    let dir = unique_temp_dir("rafonly");
    std::fs::write(
        dir.join("DSCF0001.RAF"),
        wrap_raf(&jpeg_with_exif("2026:07:08 10:11:12", "42")),
    )
    .unwrap();

    let (shots, raw_only) = scan_report(&dir).unwrap();
    assert_eq!(stems(&shots), vec!["DSCF0001".to_string()]);
    assert!(raw_only.is_empty(), "a previewable RAF-only stem is a shot, not raw_only");
    assert_eq!(shots[0].jpeg, None);
    assert_eq!(shots[0].raw, Some(dir.join("DSCF0001.RAF")));
    assert_eq!(shots[0].capture.datetime, Some("2026:07:08 10:11:12".to_string()));
    assert_eq!(shots[0].capture.subsec, Some(420));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn cr3_only_stem_stays_raw_only_not_a_shot() {
    let dir = unique_temp_dir("cr3only");
    touch(&dir.join("IMG_0001.CR3")); // no previewable extractor -> unchanged behavior
    let (shots, raw_only) = scan_report(&dir).unwrap();
    assert!(shots.is_empty());
    assert_eq!(raw_only, vec![dir.join("IMG_0001.CR3")]);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn jpeg_and_raf_sharing_a_stem_are_one_shot() {
    let dir = unique_temp_dir("rafpair");
    touch(&dir.join("DSCF0002.JPG"));
    std::fs::write(dir.join("DSCF0002.RAF"), wrap_raf(&jpeg_with_exif("2026:07:08 09:00:00", "10"))).unwrap();
    let (shots, raw_only) = scan_report(&dir).unwrap();
    assert_eq!(stems(&shots), vec!["DSCF0002".to_string()]);
    assert!(raw_only.is_empty());
    assert_eq!(shots[0].raw, Some(dir.join("DSCF0002.RAF")));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn raf_only_shot_sorts_by_embedded_capture_time() {
    let dir = unique_temp_dir("rafsort");
    std::fs::write(dir.join("B_late.RAF"), wrap_raf(&jpeg_with_exif("2026:07:08 10:00:00", "00"))).unwrap();
    std::fs::write(dir.join("A_early.RAF"), wrap_raf(&jpeg_with_exif("2026:07:08 09:00:00", "00"))).unwrap();
    let shots = scan(&dir).unwrap();
    assert_eq!(stems(&shots), vec!["A_early".to_string(), "B_late".to_string()]);
    std::fs::remove_dir_all(&dir).ok();
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p culler-core --lib scan::tests::raf_only_stem_is_promoted_with_embedded_capture_time`
Expected: FAIL — the RAF-only stem is still dropped into `raw_only`, so `shots` is empty.

- [ ] **Step 3: Factor the shared EXIF→capture helper**

In `scan.rs`, extract the tail of `read_exif_data` into a helper both paths share, then add the RAF reader. Replace `read_exif_data` and add below it:

```rust
/// Read capture time + HUD summary from a JPEG file's EXIF (single parse).
/// Undecodable/EXIF-less files never fail the scan — default `CaptureTime` + `None`.
fn read_exif_data(jpeg: &Path) -> (CaptureTime, Option<ExifSummary>) {
    let file = match std::fs::File::open(jpeg) {
        Ok(f) => f,
        Err(_) => return (CaptureTime::default(), None),
    };
    let mut reader = std::io::BufReader::new(file);
    match exif::Reader::new().read_from_container(&mut reader) {
        Ok(exif) => capture_and_summary(&exif),
        Err(_) => (CaptureTime::default(), None),
    }
}

/// Same as `read_exif_data` but for a Fuji RAF: the EXIF lives inside the
/// embedded JPEG (kamadak-exif can't read the RAF container), so extract that
/// first. Any failure degrades to default `CaptureTime` + `None`.
fn read_exif_data_raf(raf: &Path) -> (CaptureTime, Option<ExifSummary>) {
    let data = match std::fs::read(raf) {
        Ok(d) => d,
        Err(_) => return (CaptureTime::default(), None),
    };
    let jpeg = match crate::raw::embedded_jpeg(&data) {
        Some(j) => j,
        None => return (CaptureTime::default(), None),
    };
    let mut cursor = std::io::Cursor::new(jpeg);
    match exif::Reader::new().read_from_container(&mut cursor) {
        Ok(exif) => capture_and_summary(&exif),
        Err(_) => (CaptureTime::default(), None),
    }
}

/// Capture time + `ExifSummary` from an already-parsed `Exif` (shared by the
/// JPEG and RAF readers — one parse each).
fn capture_and_summary(exif: &exif::Exif) -> (CaptureTime, Option<ExifSummary>) {
    let datetime = ascii_field(exif, exif::Tag::DateTimeOriginal);
    let subsec = ascii_field(exif, exif::Tag::SubSecTimeOriginal).and_then(|s| parse_subsec(&s));
    (CaptureTime { datetime, subsec }, read_exif_summary(exif))
}
```

- [ ] **Step 4: Promote previewable RAW-only stems**

Replace the `None` arm of the `match group.jpeg` loop in `scan_report`:

```rust
            None => {
                match group.raw {
                    // A previewable RAW-only stem (Fuji RAF) is a cullable shot,
                    // shown via its embedded JPEG. Capture/EXIF come from that JPEG.
                    Some(raw) if raw_ext_supported(&raw) => {
                        let (capture, exif) = read_exif_data_raf(&raw);
                        shots.push(Shot {
                            stem,
                            jpeg: None,
                            raw: Some(raw),
                            sidecar: group.sidecar,
                            capture,
                            exif,
                        });
                    }
                    // Non-previewable RAW (CR3/NEF/…): report it, unchanged.
                    Some(raw) => raw_only.push(raw),
                    // Orphan sidecar with neither JPEG nor RAW — dropped, as before.
                    None => {}
                }
            }
```

Add the extension predicate near `ext_lower`:

```rust
/// True when `path`'s extension has an embedded-preview extractor (Fuji RAF today).
fn raw_ext_supported(path: &Path) -> bool {
    ext_lower(path)
        .as_deref()
        .is_some_and(crate::raw::preview_supported)
}
```

- [ ] **Step 5: Run the scan + workspace suites**

Run: `cargo test -p culler-core --lib scan::` then `cargo test`
Expected: PASS — the 4 new tests plus every existing scan test (the `raw_only_stem_is_reported_and_is_not_a_shot` test uses a `.CR3`, so it still lands in `raw_only`).

- [ ] **Step 6: Commit**

```bash
git add culler-core/src/scan.rs
git commit -m "feat(core): promote Fuji RAF-only stems to cullable shots"
```

---

### Task 5: `input` — the `r` action

**Files:**
- Modify: `culler/src/input.rs` (`Action` enum ~39–53; `key_to_action` ~83–101; `apply_action` no-op arm ~564–571)
- Test: inline tests in `culler/src/input.rs`

**Interfaces:**
- Produces: `Action::ToggleRawPreview`, bound to `r`/`R` in `InputContext::Loupe`.

- [ ] **Step 1: Write the failing tests**

Add to `input.rs`'s `key_tests` and `action_tests`:

```rust
#[test]
fn r_key_toggles_raw_preview_in_loupe() {
    assert_eq!(key_to_action(Key::Char('r'), m(), LOUPE), Some(Action::ToggleRawPreview));
    assert_eq!(key_to_action(Key::Char('R'), m(), LOUPE), Some(Action::ToggleRawPreview));
    // Inert while a modal owns the keyboard.
    assert_eq!(key_to_action(Key::Char('r'), m(), InputContext::TagEntry), None);
}

#[test]
fn toggle_raw_preview_does_not_mutate_model() {
    let mut s = mk_session(&[None, None]);
    apply_action(Action::ToggleRawPreview, &mut s, true, Filter::All);
    assert_eq!(s.current, 0);
    assert_eq!(s.decision(0), &Decision::default());
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p culler --lib input::key_tests::r_key_toggles_raw_preview_in_loupe`
Expected: FAIL to compile — `Action::ToggleRawPreview` doesn't exist.

- [ ] **Step 3: Add the action + keymap + no-op**

In the `Action` enum, add (near `ToggleZoom`):

```rust
    ToggleRawPreview, // r: sticky JPEG<->RAW display source for pairs (UI-only)
```

In `key_to_action`'s printable match (near the `z` line):

```rust
        Key::Char('r') | Key::Char('R') => Some(Action::ToggleRawPreview),
```

In `apply_action`, add `ToggleRawPreview` to the UI-only no-op arm:

```rust
        Action::OpenTagEntry
        | Action::ToggleZoom
        | Action::ToggleRawPreview
        | Action::CycleFilter
        | Action::OpenApply
        | Action::ForceSave
        | Action::ToggleHelp
        | Action::ToggleFullscreen
        | Action::ToggleFocus => {}
```

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p culler --lib input::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add culler/src/input.rs
git commit -m "feat(ui): add r key -> Action::ToggleRawPreview"
```

---

### Task 6: display-source helpers + `hud_text` extension (pure)

**Files:**
- Modify: `culler/src/ui.rs` (add `Showing`, `showing`, `decode_path`, `raw_previewable`, `on_raw_key`, `RawKeyOutcome`; extend `HudText` + `hud_text`)
- Modify: `culler/src/main.rs` (the single `hud_text` call in `refresh_view` ~287 — pass `false` for now)
- Test: inline tests in `culler/src/ui.rs`

**Interfaces:**
- Consumes: `culler_core::raw::preview_supported`; `Shot::display_path`/`is_raw_only` (Task 3).
- Produces: `ui::Showing`, `ui::decode_path(&Shot, bool) -> &Path`, `ui::on_raw_key(&Shot, bool) -> ui::RawKeyOutcome { flip: bool, toast: String }`, and `HudText { …, has_raw, raw_only, showing_raw }` with `hud_text(session, filter, show_raw)`.

- [ ] **Step 1: Write the failing tests**

Add a new test module to `ui.rs`:

```rust
#[cfg(test)]
mod raw_source_tests {
    use super::*;
    use culler_core::model::{CaptureTime, Shot};

    fn shot(jpeg: Option<&str>, raw: Option<&str>) -> Shot {
        Shot {
            stem: "S".into(),
            jpeg: jpeg.map(Into::into),
            raw: raw.map(Into::into),
            sidecar: None,
            capture: CaptureTime::default(),
            exif: None,
        }
    }

    #[test]
    fn showing_resolves_every_case() {
        // JPEG-only: always JPEG.
        assert_eq!(showing(&shot(Some("/s/S.JPG"), None), true), Showing::Jpeg);
        // RAF-only: always RAW.
        assert_eq!(showing(&shot(None, Some("/s/S.RAF")), false), Showing::Raw);
        // RAF pair: follows prefer_raw.
        assert_eq!(showing(&shot(Some("/s/S.JPG"), Some("/s/S.RAF")), false), Showing::Jpeg);
        assert_eq!(showing(&shot(Some("/s/S.JPG"), Some("/s/S.RAF")), true), Showing::Raw);
        // CR3 pair: not previewable, stays JPEG even when prefer_raw.
        assert_eq!(showing(&shot(Some("/s/S.JPG"), Some("/s/S.CR3")), true), Showing::Jpeg);
    }

    #[test]
    fn decode_path_matches_showing() {
        let pair = shot(Some("/s/S.JPG"), Some("/s/S.RAF"));
        assert_eq!(decode_path(&pair, false), std::path::Path::new("/s/S.JPG"));
        assert_eq!(decode_path(&pair, true), std::path::Path::new("/s/S.RAF"));
        assert_eq!(decode_path(&shot(None, Some("/s/S.RAF")), false), std::path::Path::new("/s/S.RAF"));
    }

    #[test]
    fn on_raw_key_covers_four_cases() {
        // Previewable pair: flip; toast names the post-flip state.
        let o = on_raw_key(&shot(Some("/s/S.JPG"), Some("/s/S.RAF")), false);
        assert!(o.flip);
        assert_eq!(o.toast, "showing RAW");
        let o2 = on_raw_key(&shot(Some("/s/S.JPG"), Some("/s/S.RAF")), true);
        assert!(o2.flip);
        assert_eq!(o2.toast, "showing JPEG");
        // Non-previewable RAW in a pair: no flip, names the extension.
        let o3 = on_raw_key(&shot(Some("/s/S.JPG"), Some("/s/S.CR3")), false);
        assert!(!o3.flip);
        assert_eq!(o3.toast, "RAW preview unsupported (.cr3)");
        // JPEG-only: no flip.
        let o4 = on_raw_key(&shot(Some("/s/S.JPG"), None), false);
        assert!(!o4.flip);
        assert_eq!(o4.toast, "no RAW for this shot");
        // RAW-only: no flip.
        let o5 = on_raw_key(&shot(None, Some("/s/S.RAF")), false);
        assert!(!o5.flip);
        assert_eq!(o5.toast, "RAW only — no JPEG to switch to");
    }

    fn one_shot_session(s: Shot) -> Session {
        Session {
            source_dir: "/s".into(),
            shots: vec![s],
            decisions: std::collections::HashMap::new(),
            current: 0,
            pending_apply: None,
            undo: Vec::new(),
        }
    }

    #[test]
    fn hud_text_reports_raw_state_and_shown_filename() {
        // RAF-only: raw_only + showing_raw, filename is the .RAF.
        let h = hud_text(&one_shot_session(shot(None, Some("/s/DSCF1.RAF"))), Filter::All, false);
        assert!(h.has_raw && h.raw_only && h.showing_raw);
        assert_eq!(h.filename, "DSCF1.RAF");
        // Pair, prefer JPEG: showing JPEG, filename the .JPG.
        let hp = hud_text(&one_shot_session(shot(Some("/s/DSCF2.JPG"), Some("/s/DSCF2.RAF"))), Filter::All, false);
        assert!(hp.has_raw && !hp.raw_only && !hp.showing_raw);
        assert_eq!(hp.filename, "DSCF2.JPG");
        // Same pair, prefer RAW: showing RAW, filename the .RAF.
        let hr = hud_text(&one_shot_session(shot(Some("/s/DSCF2.JPG"), Some("/s/DSCF2.RAF"))), Filter::All, true);
        assert!(hr.showing_raw);
        assert_eq!(hr.filename, "DSCF2.RAF");
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p culler --lib raw_source_tests`
Expected: FAIL to compile — helpers, `Showing`, and the new `HudText` fields don't exist.

- [ ] **Step 3: Add the helpers (ui.rs)**

Add near the top of `ui.rs` (after the imports):

```rust
/// What the loupe is actually displaying for a shot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Showing {
    Jpeg,
    Raw,
}

/// True if the shot's RAW sibling has an embedded-preview extractor (Fuji RAF).
fn raw_previewable(shot: &culler_core::model::Shot) -> bool {
    shot.raw
        .as_deref()
        .and_then(|r| r.extension())
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
        .is_some_and(culler_core::raw::preview_supported)
}

/// Resolve which source the loupe shows for `shot` under the sticky prefer-RAW
/// flag: a JPEG-less shot always shows RAW; a JPEG-only shot always JPEG; a pair
/// shows RAW only when `prefer_raw` AND the RAW is previewable.
pub fn showing(shot: &culler_core::model::Shot, prefer_raw: bool) -> Showing {
    if shot.jpeg.is_none() {
        Showing::Raw
    } else if prefer_raw && raw_previewable(shot) {
        Showing::Raw
    } else {
        Showing::Jpeg
    }
}

/// The path the loupe should decode for `shot` under `prefer_raw`.
pub fn decode_path(shot: &culler_core::model::Shot, prefer_raw: bool) -> &std::path::Path {
    match showing(shot, prefer_raw) {
        Showing::Raw => shot.raw.as_deref().unwrap_or_else(|| shot.display_path()),
        Showing::Jpeg => shot.jpeg.as_deref().unwrap_or_else(|| shot.display_path()),
    }
}

/// The outcome of pressing `r` on `shot`, given the prefer-RAW flag BEFORE the press.
pub struct RawKeyOutcome {
    /// Whether the caller should flip the sticky prefer-RAW flag.
    pub flip: bool,
    /// The transient toast to show (always non-empty).
    pub toast: String,
}

/// Decide what pressing `r` does on `shot`. Flips only for a previewable pair;
/// otherwise it's a no-op with an explanatory toast.
pub fn on_raw_key(shot: &culler_core::model::Shot, prefer_raw_before: bool) -> RawKeyOutcome {
    if shot.jpeg.is_none() {
        RawKeyOutcome { flip: false, toast: "RAW only — no JPEG to switch to".into() }
    } else if shot.raw.is_none() {
        RawKeyOutcome { flip: false, toast: "no RAW for this shot".into() }
    } else if !raw_previewable(shot) {
        let ext = shot
            .raw
            .as_deref()
            .and_then(|r| r.extension())
            .and_then(|e| e.to_str())
            .unwrap_or("raw")
            .to_ascii_lowercase();
        RawKeyOutcome { flip: false, toast: format!("RAW preview unsupported (.{ext})") }
    } else {
        let showing_raw_after = !prefer_raw_before;
        let toast = if showing_raw_after { "showing RAW" } else { "showing JPEG" };
        RawKeyOutcome { flip: true, toast: toast.into() }
    }
}
```

- [ ] **Step 4: Extend `HudText` + `hud_text`**

Add three fields to the `HudText` struct:

```rust
    /// Whether the current shot has a RAW sibling (drives the badge presence).
    pub has_raw: bool,
    /// Whether the current shot has NO JPEG (drives the amber "RAW ONLY" badge).
    pub raw_only: bool,
    /// Whether the loupe is currently showing the RAW (drives the accent badge).
    pub showing_raw: bool,
```

Change `hud_text`'s signature to `pub fn hud_text(session: &Session, filter: Filter, show_raw: bool) -> HudText` and replace the `filename`/`has_raw` derivation block with:

```rust
    let current_shot = session.shots.get(session.current);
    let (filename, has_raw, raw_only, showing_raw) = match current_shot {
        Some(s) => {
            let showing = showing(s, show_raw);
            let shown = match showing {
                Showing::Raw => s.raw.as_deref().unwrap_or_else(|| s.display_path()),
                Showing::Jpeg => s.jpeg.as_deref().unwrap_or_else(|| s.display_path()),
            };
            let filename = shown
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            (filename, s.raw.is_some(), s.is_raw_only(), matches!(showing, Showing::Raw))
        }
        None => (String::new(), false, false, false),
    };
```

Add the three fields to the returned `HudText { … }` literal (`has_raw, raw_only, showing_raw`). Update the two existing `hud_text` assertions in `hud_tests` (`hud_text_reports_tier_counts_and_progress`, `hud_text_shows_rest_for_undecided_current`, `hud_text_empty_session_is_safe`) to call `hud_text(&s, filter, false)`.

- [ ] **Step 5: Fix the single call site in main.rs (temporary `false`)**

In `culler/src/main.rs` `refresh_view` (~287): `let h = ui::hud_text(&s, *filter.borrow(), false);` — Task 7 replaces `false` with the real flag.

- [ ] **Step 6: Run to verify they pass**

Run: `cargo test -p culler --lib` then `cargo build`
Expected: PASS + clean build.

- [ ] **Step 7: Commit**

```bash
git add culler/src/ui.rs culler/src/main.rs
git commit -m "feat(ui): display-source helpers + hud_text RAW state"
```

---

### Task 7: wire the toggle — main.rs state/routing, AppWindow props, HUD badge

**Files:**
- Modify: `culler/src/main.rs` (add `show_raw` state; `request_current` routing; `ToggleRawPreview` handler; `refresh_view` prop wiring)
- Modify: `culler/ui/app.slint` (2 props ~47; pass to `HudTopLeft` ~183–188)
- Modify: `culler/ui/hud.slint` (`HudTopLeft` 3-state badge ~47–101)

**Interfaces:**
- Consumes: `ui::decode_path`, `ui::on_raw_key`, `HudText.{has_raw,raw_only,showing_raw}` (Task 6); `Action::ToggleRawPreview` (Task 5).

This task is verified by build + manual run (event-loop wiring; the pure logic it calls is already unit-tested in Task 6).

- [ ] **Step 1: Add the `show_raw` state (main.rs)**

Beside `let zoom = …` (~145):

```rust
    // Sticky "prefer RAW" display source (r): persists across navigation like zoom.
    let show_raw = Rc::new(std::cell::Cell::new(false));
```

- [ ] **Step 2: Route the current-shot decode through it**

In `request_current`, capture `show_raw` (add `let show_raw = show_raw.clone();` in the closure's capture block) and change the current-shot enqueue (~259) to:

```rust
            pipeline.enqueue(
                cur,
                ui::decode_path(&s.shots[cur], show_raw.get()).to_path_buf(),
                z.target(fw, fh),
                false,
            );
```

Leave the neighbor prefetch on `display_path()` (neighbors always show their default source).

- [ ] **Step 3: Handle the `r` action**

In `on_key_pressed`, capture `show_raw` (add `let show_raw = show_raw.clone();` in that closure's capture block) and add an arm beside `Action::ToggleZoom`:

```rust
                Action::ToggleRawPreview => {
                    let outcome = {
                        let s = session.borrow();
                        s.shots.get(s.current).map(|shot| ui::on_raw_key(shot, show_raw.get()))
                    };
                    if let Some(o) = outcome {
                        if o.flip {
                            show_raw.set(!show_raw.get());
                            request_current(); // re-decode current at the new source
                            refresh_view(); // repaint the badge + filename now
                        }
                        show_toast(o.toast, -1);
                    }
                }
```

- [ ] **Step 4: Push the new HUD props in refresh_view**

In `refresh_view`, capture `show_raw` (add `let show_raw = show_raw.clone();`), change the `hud_text` call to pass the flag, and set the two new props (beside `app.set_hud_has_raw(h.has_raw);` ~295):

```rust
            let h = ui::hud_text(&s, *filter.borrow(), show_raw.get());
```
```rust
            app.set_hud_has_raw(h.has_raw);
            app.set_hud_showing_raw(h.showing_raw);
            app.set_hud_raw_only(h.raw_only);
```

- [ ] **Step 5: Declare the AppWindow properties (app.slint)**

Beside `in property <bool> hud-has-raw: false;` (~47):

```slint
    in property <bool> hud-showing-raw: false;
    in property <bool> hud-raw-only: false;
```

Pass them into the `HudTopLeft` instance (~183):

```slint
                HudTopLeft {
                    visible: !root.focus-mode;
                    filename: root.hud-filename;
                    has-raw: root.hud-has-raw;
                    showing-raw: root.hud-showing-raw;
                    raw-only: root.hud-raw-only;
                    position: root.hud-position;
                }
```

- [ ] **Step 6: Make the badge 3-state (hud.slint)**

In `HudTopLeft`, add the two input properties beside `in property <bool> has-raw;`:

```slint
    in property <bool> showing-raw: false;
    in property <bool> raw-only: false;
```

Replace the `if root.has-raw: Rectangle { … }` badge block with:

```slint
            // RAW badge — three states:
            //   raw-only    → amber "RAW ONLY" (no JPEG exists; standing warning)
            //   showing-raw → accent-filled "RAW" (pair currently viewing the RAW)
            //   has-raw     → dim outlined "RAW" (a RAW sibling exists, JPEG on screen)
            if root.has-raw || root.raw-only: Rectangle {
                border-radius: Theme.r-sm;
                border-width: 1px;
                border-color: root.raw-only
                    ? Theme.accent-warn
                    : (root.showing-raw ? Theme.accent-primary : #45484e);
                background: root.raw-only
                    ? Theme.accent-warn.with-alpha(0.16)
                    : (root.showing-raw ? Theme.accent-primary.with-alpha(0.22) : transparent);
                HorizontalLayout {
                    padding: 3px;
                    Text {
                        text: root.raw-only ? "RAW ONLY" : "RAW";
                        font-family: Theme.font-mono;
                        font-weight: 600;
                        font-size: 9px;
                        color: root.raw-only ? Theme.accent-warn : Theme.text;
                        vertical-alignment: center;
                    }
                }
            }
```

- [ ] **Step 7: Build**

Run: `cargo build`
Expected: clean build (Slint compiles the new properties; no Rust errors).

- [ ] **Step 8: Manual smoke (real app)**

Run: `cargo run -p culler -- <a folder with a jpeg+raf pair AND a raf-only file>`
Verify:
- A jpeg+raf pair shows a dim "RAW" badge; press `r` → image reloads, badge turns accent-blue, filename flips to `.RAF`, toast "showing RAW"; press `r` again → back to JPEG, toast "showing JPEG".
- Navigate to the raf-only file → amber "RAW ONLY" badge shows, image is the embedded preview, filename is the `.RAF`.
- Press `r` on a JPEG-only shot → toast "no RAW for this shot", nothing else changes.

- [ ] **Step 9: Commit**

```bash
git add culler/src/main.rs culler/ui/app.slint culler/ui/hud.slint
git commit -m "feat(ui): wire r toggle — routing, RAW-state HUD badge, warning toasts"
```

---

### Task 8: KeySheet entry + final verification

**Files:**
- Modify: `culler/ui/keysheet.slint` (add the `r` row)
- Test: full workspace + manual

- [ ] **Step 1: Add `r` to the KeySheet**

In `culler/ui/keysheet.slint`, add a `KeyRow` to the **VIEW & TAGS** `KeyGroup`, right after the `Z` (zoom) row (~167):

```slint
                        KeyRow { keycap: "r"; desc: "RAW / JPEG preview toggle"; }
```

That group grows from 5 rows to 6, so bump the fixed `panel` height (~88) by one row (~32px) to keep it from clipping — the existing comment there documents exactly this sizing reasoning:

```slint
        height: 532px;
```

- [ ] **Step 2: Full test suite**

Run: `cargo test`
Expected: PASS — entire workspace (existing 221 + the new `raw`, `decode`, `model`, `scan`, `input`, `ui` tests).

- [ ] **Step 3: Clippy + build**

Run: `cargo clippy --all-targets -- -D warnings && cargo build`
Expected: no warnings, clean build.

- [ ] **Step 4: Manual verification checklist (real app)**

Run: `cargo run -p culler -- <folder with jpeg+raf pairs, a raf-only file, a jpeg-only file>`
Confirm each:
- [ ] raf-only file is a cullable shot (appears in the filmstrip in capture-time order), shows its embedded preview + amber "RAW ONLY" badge, and can be tiered/applied (the `.RAF` moves into the bucket).
- [ ] `r` on a jpeg+raf pair toggles the loupe image + badge + filename; sticky across navigation.
- [ ] `r` warnings fire correctly for jpeg-only and raf-only shots.
- [ ] `?` KeySheet lists the `r` binding.
- [ ] A corrupt/preview-less `.raf` shows the placeholder tile, not a crash.

- [ ] **Step 5: Commit**

```bash
git add culler/ui/keysheet.slint
git commit -m "feat(ui): document r (RAW/JPEG toggle) in the KeySheet"
```

---

## Self-review notes

- **Spec coverage:** `raw` module (T1) · decode dispatch + thumbnail (T2) · `Shot.jpeg` optional + helpers (T3) · scan promotion + RAF EXIF (T4) · `r` action (T5) · display-source helpers + `hud_text` (T6) · routing + badge + toasts (T7) · KeySheet + verification (T8). Apply/move of RAF-only shots is covered by `files()` (T3) and manually verified (T8). Portability constraint (no new deps) is enforced in Global Constraints.
- **Sticky toggle** is realized as one `Cell<bool>` never reset on navigation (T7), matching the spec.
- **Type consistency:** `preview_supported`/`embedded_jpeg` (T1) are the only cross-module raw calls; `display_path`/`is_raw_only` (T3) and `Showing`/`decode_path`/`on_raw_key`/`hud_text(…, show_raw)` (T6) are used verbatim in T7.
