# FastCull Phase 2 — Scan & Ingest — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax. Canonical types are in [README.md](README.md); use them verbatim. **Depends on Phase 1** (`culler-core::model`).

**Goal:** Build `culler-core`'s `scan` module — a flat (non-recursive) folder walk that groups files by filename stem into `Shot`s (JPEG display file + optional RAW sibling + optional pre-existing sidecar), reads EXIF capture time from the JPEG, and returns them in a stable capture-time order.

**Architecture:** `scan` consumes the Phase 1 `model` types verbatim (`Shot`, `CaptureTime`, `RAW_EXTS`, `JPEG_EXTS`) and produces `pub fn scan(dir: &Path) -> Result<Vec<Shot>, ScanError>`. All grouping, RAW/sidecar detection and sorting are pure functions over a directory listing; the only I/O is `read_dir` and reading each JPEG's EXIF header. A shot **requires** a JPEG in v1, so `scan` also exposes `scan_report`, which additionally returns the RAW-only stems that are not cullable shots, so they are never silently dropped.

**Tech Stack:** Rust 2021, kamadak-exif, std::fs.

> **Assumption (spec §5 vs §10/§13):** §5 says the JPEG is "required in v1"; §10 says a RAW-only stem gets a "no preview" placeholder and is "still movable"; §13 defers "RAW-only shots via embedded-preview extraction" to **phase 2 (not v1)**. Resolved for v1: **`scan` produces `Shot`s only for stems that have a JPEG. A stem with only a RAW (no JPEG sibling) is NOT a cullable shot in v1.** To avoid silently dropping such files, `scan_report(dir) -> Result<(Vec<Shot>, Vec<PathBuf> /*raw_only*/), ScanError>` returns the RAW-only paths alongside the shots; `scan` calls `scan_report` and discards the second element. Embedded-preview extraction that would turn RAW-only stems into displayable shots is a phase-2 item.

## Global Constraints

These bind every task in this phase (copied from [README.md](README.md), tailored to `scan`):

- **Language / edition:** Rust, edition 2021. Work inside the existing workspace; all code lands in `culler-core` (lib).
- **`culler-core` has zero GUI dependencies.** No `slint`, no Slint types. `scan` returns plain `model` types only.
- **Nothing touches disk until Apply.** `scan` is **read-only**: it lists the directory and reads each JPEG's EXIF header. It never writes, moves, or deletes a file.
- **Decisions are keyed by filename stem** so resume re-attaches them after a rescan — therefore the shot key is `Shot.stem`, and the stable sort keeps burst order fixed across sessions.
- **Case-insensitive extension matching throughout.** `JPG`/`jpg`/`JpG`, `CR3`/`cr3`, `.xmp`/`.XMP`, and the darktable inner extension are all compared lower-cased.
- **Platform:** Linux only.
- **TDD, DRY, YAGNI, frequent commits.** Every task: failing test → run-it-fails → minimal impl → run-it-passes → commit. Conventional-commit messages (`feat:`, `test:`, `refactor:`).
- **No v1 config file.** Nothing in `scan` is configurable; the recognized extension tables are the `RAW_EXTS` / `JPEG_EXTS` constants from Phase 1.

---

### Task 1: `scan.rs` scaffolding — `ScanError`, flat walk, jpeg-only shots

**Files:**
- Create: `culler-core/src/scan.rs`
- Modify: `culler-core/src/lib.rs` (add `pub mod scan;`)
- Test: `culler-core/src/scan.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: Phase 1 `model::{Shot, CaptureTime}`, `model::JPEG_EXTS`
- Produces:
  - `pub enum ScanError { Io(std::io::Error), NotADir(std::path::PathBuf) }` (impls `Display` + `std::error::Error`)
  - `pub fn scan(dir: &Path) -> Result<Vec<Shot>, ScanError>` — validates the directory, walks it **flat** (subdirectories ignored), and (this first cut) emits one `Shot` per JPEG display file. RAW/sidecar/EXIF/sort arrive in later tasks.
  - **Private helpers (phase-2-local):** `fn ext_lower(path) -> Option<String>`, `fn file_stem_string(path) -> String`.

- [ ] **Step 1: Write the failing test**

Add `pub mod scan;` to `culler-core/src/lib.rs` (below `pub mod persist;`), then create `culler-core/src/scan.rs` containing ONLY the tests module (implementation comes in Step 3):
```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, unique temp directory for a single test (no `tempfile` dep —
    /// same hand-rolled approach as Phase 1's `persist` tests).
    fn unique_temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "fastcull-scan-{tag}-{}-{nanos}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Create an empty placeholder file. An empty `.jpg` has no decodable EXIF,
    /// so its `CaptureTime` stays default — perfect for pairing/sort fixtures.
    fn touch(path: &Path) {
        std::fs::write(path, b"").unwrap();
    }

    /// The stems of the returned shots, in order.
    fn stems(shots: &[Shot]) -> Vec<String> {
        shots.iter().map(|s| s.stem.clone()).collect()
    }

    #[test]
    fn not_a_dir_path_is_an_error() {
        let dir = unique_temp_dir("notadir");
        let file = dir.join("IMG_0001.JPG");
        touch(&file);
        match scan(&file) {
            Err(ScanError::NotADir(p)) => assert_eq!(p, file),
            other => panic!("expected NotADir, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_dir_yields_no_shots() {
        let dir = unique_temp_dir("empty");
        assert!(scan(&dir).unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn subdirectory_is_ignored() {
        let dir = unique_temp_dir("flatwalk");
        touch(&dir.join("TOP.JPG"));
        let sub = dir.join("nested");
        std::fs::create_dir_all(&sub).unwrap();
        touch(&sub.join("INNER.JPG")); // must NOT appear — flat walk only

        let shots = scan(&dir).unwrap();
        assert_eq!(stems(&shots), vec!["TOP".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn jpeg_files_become_shots_case_insensitive() {
        let dir = unique_temp_dir("jpegs");
        touch(&dir.join("IMG_0001.JPG"));
        touch(&dir.join("IMG_0002.jpeg"));
        touch(&dir.join("IMG_0003.JpG"));

        let shots = scan(&dir).unwrap();
        assert_eq!(
            stems(&shots),
            vec![
                "IMG_0001".to_string(),
                "IMG_0002".to_string(),
                "IMG_0003".to_string()
            ]
        );
        // The display-file path is carried verbatim (on-disk case preserved).
        assert_eq!(shots[0].jpeg, dir.join("IMG_0001.JPG"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn non_image_files_are_ignored() {
        let dir = unique_temp_dir("nonimage");
        touch(&dir.join("IMG_0001.JPG"));
        touch(&dir.join("notes.txt"));
        touch(&dir.join("clip.mov"));
        touch(&dir.join(".fastcull.json")); // the session sidecar is not a shot

        let shots = scan(&dir).unwrap();
        assert_eq!(stems(&shots), vec!["IMG_0001".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
```

- [ ] **Step 2: Run test to verify it fails**
Run: `cargo test -p culler-core scan::tests::not_a_dir_path_is_an_error`
Expected: FAIL — compile error `error[E0425]: cannot find function \`scan\` in this scope` and `error[E0433]: failed to resolve: use of undeclared type \`ScanError\``.

- [ ] **Step 3: Write minimal implementation**

Insert ABOVE the `#[cfg(test)]` module in `culler-core/src/scan.rs`:
```rust
//! Folder scan: a flat (non-recursive) walk that groups files by filename stem
//! into `Shot`s and sorts them into a stable, capture-time filmstrip order.
//!
//! A shot REQUIRES a JPEG display file in v1: stems with only a RAW are not
//! cullable shots (they are surfaced by `scan_report`, added in a later task).
//! Zero GUI dependencies.

use crate::model::{CaptureTime, Shot, JPEG_EXTS};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum ScanError {
    Io(std::io::Error),
    NotADir(PathBuf),
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScanError::Io(e) => write!(f, "scan I/O error: {e}"),
            ScanError::NotADir(p) => write!(f, "not a directory: {}", p.display()),
        }
    }
}

impl std::error::Error for ScanError {}

/// Files sharing one filename stem, accumulated during the walk.
#[derive(Default)]
struct Group {
    jpeg: Option<PathBuf>,
}

/// Flat (non-recursive) walk of `dir`. In this first cut only JPEG display files
/// are recognized; each becomes a one-file `Shot`. RAW siblings, sidecars, EXIF
/// capture time and stable sorting arrive in later tasks.
pub fn scan(dir: &Path) -> Result<Vec<Shot>, ScanError> {
    if !dir.is_dir() {
        return Err(ScanError::NotADir(dir.to_path_buf()));
    }

    // Collect entries first and sort by path so grouping is deterministic.
    let mut entries: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(ScanError::Io)? {
        let entry = entry.map_err(ScanError::Io)?;
        if entry.file_type().map_err(ScanError::Io)?.is_dir() {
            continue; // flat walk: subdirectories are ignored entirely
        }
        entries.push(entry.path());
    }
    entries.sort();

    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    for path in &entries {
        let ext = match ext_lower(path) {
            Some(e) => e,
            None => continue, // no extension → unrecognized, ignored
        };
        if JPEG_EXTS.contains(&ext.as_str()) {
            let stem = file_stem_string(path);
            groups
                .entry(stem)
                .or_default()
                .jpeg
                .get_or_insert_with(|| path.clone());
        }
        // Everything else is ignored for now.
    }

    let mut shots: Vec<Shot> = Vec::new();
    for (stem, group) in groups {
        if let Some(jpeg) = group.jpeg {
            shots.push(Shot {
                stem,
                jpeg,
                raw: None,
                sidecar: None,
                capture: CaptureTime::default(),
            });
        }
    }
    Ok(shots)
}

/// Lower-cased file extension (no leading dot), or `None` when absent.
fn ext_lower(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

/// The filename portion before the final extension, as an owned `String`.
fn file_stem_string(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string()
}
```

- [ ] **Step 4: Run test to verify it passes**
Run: `cargo test -p culler-core scan::`
Expected: PASS — all five Task-1 tests green.

- [ ] **Step 5: Commit**
```bash
git add culler-core/src/lib.rs culler-core/src/scan.rs
git commit -m "feat(scan): add ScanError, flat walk, and jpeg-only shot scaffolding" -m "Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: `scan_report` — stem grouping, JPEG requirement, RAW-only reporting

**Files:**
- Modify: `culler-core/src/scan.rs`
- Test: `culler-core/src/scan.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: Phase 1 `model::{Shot, CaptureTime}`, `model::JPEG_EXTS`, `model::RAW_EXTS`; `ext_lower`, `file_stem_string`, `Group` (Task 1)
- Produces:
  - `pub fn scan_report(dir: &Path) -> Result<(Vec<Shot>, Vec<PathBuf>), ScanError>` — flat walk; groups by stem; emits a `Shot` for every stem that has a JPEG; a stem with a RAW but **no JPEG** produces no shot and its RAW path is returned in the second element (`raw_only`). `scan` now delegates to `scan_report` and discards the report.
  - `Group` gains a `raw` field. (Shot's `raw` field is still `None` here — the RAW is only used for the report; it is wired into `Shot.raw` in Task 3.)

- [ ] **Step 1: Write the failing test**

Add inside `scan.rs`'s `#[cfg(test)] mod tests` block (reuses `unique_temp_dir`, `touch`, `stems` from Task 1):
```rust
    #[test]
    fn raw_only_stem_is_reported_and_is_not_a_shot() {
        let dir = unique_temp_dir("rawonly");
        touch(&dir.join("IMG_0001.JPG")); // a normal shot
        touch(&dir.join("IMG_0002.CR3")); // RAW with no JPEG sibling

        let (shots, raw_only) = scan_report(&dir).unwrap();
        assert_eq!(stems(&shots), vec!["IMG_0001".to_string()]);
        assert_eq!(raw_only, vec![dir.join("IMG_0002.CR3")]);

        // scan() hides the report and returns only the cullable shots.
        assert_eq!(stems(&scan(&dir).unwrap()), vec!["IMG_0001".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn jpeg_and_raw_sharing_a_stem_are_one_shot() {
        let dir = unique_temp_dir("pair");
        touch(&dir.join("IMG_0001.JPG"));
        touch(&dir.join("IMG_0001.CR3"));

        let (shots, raw_only) = scan_report(&dir).unwrap();
        assert_eq!(stems(&shots), vec!["IMG_0001".to_string()]);
        assert!(raw_only.is_empty()); // the RAW is paired, not orphaned
        std::fs::remove_dir_all(&dir).ok();
    }
```

- [ ] **Step 2: Run test to verify it fails**
Run: `cargo test -p culler-core scan::tests::raw_only_stem_is_reported_and_is_not_a_shot`
Expected: FAIL — compile error `error[E0425]: cannot find function \`scan_report\` in this scope`.

- [ ] **Step 3: Write minimal implementation**

In `culler-core/src/scan.rs`, add `RAW_EXTS` to the model import:
```rust
use crate::model::{CaptureTime, Shot, JPEG_EXTS, RAW_EXTS};
```
Add the `raw` field to `Group`:
```rust
/// Files sharing one filename stem, accumulated during the walk.
#[derive(Default)]
struct Group {
    jpeg: Option<PathBuf>,
    raw: Option<PathBuf>,
}
```
Replace the entire `pub fn scan` with `scan_report` + a thin `scan` delegate:
```rust
/// Flat (non-recursive) walk of `dir`. Groups files by filename stem, emits a
/// `Shot` for every stem that has a JPEG display file, and returns the RAW paths
/// of stems that have a RAW but **no JPEG** (not cullable shots in v1) so they
/// are never silently dropped.
pub fn scan_report(dir: &Path) -> Result<(Vec<Shot>, Vec<PathBuf>), ScanError> {
    if !dir.is_dir() {
        return Err(ScanError::NotADir(dir.to_path_buf()));
    }

    let mut entries: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(ScanError::Io)? {
        let entry = entry.map_err(ScanError::Io)?;
        if entry.file_type().map_err(ScanError::Io)?.is_dir() {
            continue; // flat walk: subdirectories are ignored entirely
        }
        entries.push(entry.path());
    }
    entries.sort();

    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    for path in &entries {
        let ext = match ext_lower(path) {
            Some(e) => e,
            None => continue, // no extension → unrecognized, ignored
        };
        if JPEG_EXTS.contains(&ext.as_str()) {
            let stem = file_stem_string(path);
            groups
                .entry(stem)
                .or_default()
                .jpeg
                .get_or_insert_with(|| path.clone());
        } else if RAW_EXTS.contains(&ext.as_str()) {
            let stem = file_stem_string(path);
            groups
                .entry(stem)
                .or_default()
                .raw
                .get_or_insert_with(|| path.clone());
        }
        // Everything else (videos, session sidecar, …) is unrecognized → ignored.
    }

    let mut shots: Vec<Shot> = Vec::new();
    let mut raw_only: Vec<PathBuf> = Vec::new();
    for (stem, group) in groups {
        match group.jpeg {
            Some(jpeg) => shots.push(Shot {
                stem,
                jpeg,
                raw: None,
                sidecar: None,
                capture: CaptureTime::default(),
            }),
            None => {
                // No JPEG → not a cullable shot in v1. Report the RAW so a
                // RAW-only stem isn't silently dropped (embedded-preview
                // extraction is a phase-2 item).
                if let Some(raw) = group.raw {
                    raw_only.push(raw);
                }
                // A stem with neither JPEG nor RAW (e.g. an orphan file) is dropped.
            }
        }
    }
    Ok((shots, raw_only))
}

/// Flat walk of `dir` returning only the cullable shots. Thin wrapper over
/// `scan_report` that discards the RAW-only report.
pub fn scan(dir: &Path) -> Result<Vec<Shot>, ScanError> {
    let (shots, _raw_only) = scan_report(dir)?;
    Ok(shots)
}
```

- [ ] **Step 4: Run test to verify it passes**
Run: `cargo test -p culler-core scan::`
Expected: PASS — Task-1 and Task-2 tests all green.

- [ ] **Step 5: Commit**
```bash
git add culler-core/src/scan.rs
git commit -m "feat(scan): add scan_report with stem grouping, JPEG requirement, raw-only report" -m "Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: RAW sibling detection (case-insensitive)

**Files:**
- Modify: `culler-core/src/scan.rs`
- Test: `culler-core/src/scan.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `scan_report`, `Group.raw` (Task 2)
- Produces: the JPEG-bearing shot now carries its RAW sibling in `Shot.raw` (case-insensitive extension match). No new public symbols; the only change is `raw: None` → `raw: group.raw` in the shot-construction arm.

- [ ] **Step 1: Write the failing test**

Add inside `scan.rs`'s `#[cfg(test)] mod tests` block:
```rust
    #[test]
    fn raw_sibling_is_attached_to_its_shot() {
        let dir = unique_temp_dir("rawsibling");
        touch(&dir.join("IMG_0001.JPG"));
        touch(&dir.join("IMG_0001.CR3"));

        let shots = scan(&dir).unwrap();
        assert_eq!(shots.len(), 1);
        assert_eq!(shots[0].raw, Some(dir.join("IMG_0001.CR3")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn raw_sibling_matches_case_insensitively() {
        let dir = unique_temp_dir("rawcase");
        touch(&dir.join("IMG_0001.jpg"));
        touch(&dir.join("IMG_0001.Nef")); // mixed-case RAW extension

        let shots = scan(&dir).unwrap();
        assert_eq!(shots.len(), 1);
        assert_eq!(shots[0].raw, Some(dir.join("IMG_0001.Nef")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn jpeg_without_raw_has_no_sibling() {
        let dir = unique_temp_dir("noraw");
        touch(&dir.join("IMG_0001.JPG"));
        let shots = scan(&dir).unwrap();
        assert_eq!(shots[0].raw, None);
        std::fs::remove_dir_all(&dir).ok();
    }
```

- [ ] **Step 2: Run test to verify it fails**
Run: `cargo test -p culler-core scan::tests::raw_sibling_is_attached_to_its_shot`
Expected: FAIL — runtime assertion `assertion \`left == right\` failed` (left: `None`, right: `Some(".../IMG_0001.CR3")`), because the shot-construction arm still sets `raw: None`.

- [ ] **Step 3: Write minimal implementation**

In `scan_report`, change the JPEG-bearing arm so the shot carries the grouped RAW. Replace the `Some(jpeg) => shots.push(Shot { ... })` arm with:
```rust
            Some(jpeg) => shots.push(Shot {
                stem,
                jpeg,
                raw: group.raw,
                sidecar: None,
                capture: CaptureTime::default(),
            }),
```
(The `RAW_EXTS` extension match in the grouping loop is already case-insensitive because `ext_lower` lower-cases the extension before comparing.)

- [ ] **Step 4: Run test to verify it passes**
Run: `cargo test -p culler-core scan::`
Expected: PASS — Tasks 1–3 all green.

- [ ] **Step 5: Commit**
```bash
git add culler-core/src/scan.rs
git commit -m "feat(scan): detect RAW siblings case-insensitively" -m "Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: Sidecar detection — both Adobe and darktable conventions

**Files:**
- Modify: `culler-core/src/scan.rs`
- Test: `culler-core/src/scan.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `scan_report`, `Group` (Tasks 2–3); `RAW_EXTS`, `JPEG_EXTS`
- Produces:
  - `Group` gains a `sidecar` field; the grouping loop recognizes `.xmp` files under **both** conventions and the shot carries `Shot.sidecar`.
  - **Private helper (phase-2-local):** `fn sidecar_stem(path) -> String` — maps `IMG_1234.xmp` (Adobe) and `IMG_1234.CR3.xmp` (darktable) both to shot stem `IMG_1234`, stripping a case-insensitive inner RAW/JPEG extension when present.

- [ ] **Step 1: Write the failing test**

Add inside `scan.rs`'s `#[cfg(test)] mod tests` block:
```rust
    #[test]
    fn adobe_sidecar_is_detected() {
        let dir = unique_temp_dir("adobexmp");
        touch(&dir.join("IMG_0001.JPG"));
        touch(&dir.join("IMG_0001.xmp")); // Adobe convention: stem.xmp

        let shots = scan(&dir).unwrap();
        assert_eq!(shots.len(), 1);
        assert_eq!(shots[0].sidecar, Some(dir.join("IMG_0001.xmp")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn darktable_sidecar_is_detected() {
        let dir = unique_temp_dir("dtxmp");
        touch(&dir.join("IMG_0001.JPG"));
        touch(&dir.join("IMG_0001.CR3"));
        touch(&dir.join("IMG_0001.CR3.xmp")); // darktable convention: file.ext.xmp

        let shots = scan(&dir).unwrap();
        assert_eq!(shots.len(), 1);
        assert_eq!(shots[0].raw, Some(dir.join("IMG_0001.CR3")));
        assert_eq!(shots[0].sidecar, Some(dir.join("IMG_0001.CR3.xmp")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn darktable_sidecar_inner_ext_is_case_insensitive() {
        let dir = unique_temp_dir("dtxmpcase");
        touch(&dir.join("IMG_0001.JPG"));
        touch(&dir.join("IMG_0001.jpg.XMP")); // darktable sidecar for the JPEG, mixed case

        let shots = scan(&dir).unwrap();
        assert_eq!(shots.len(), 1);
        assert_eq!(shots[0].sidecar, Some(dir.join("IMG_0001.jpg.XMP")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn orphan_sidecar_without_jpeg_is_ignored() {
        let dir = unique_temp_dir("orphanxmp");
        touch(&dir.join("IMG_0001.xmp")); // no JPEG, no RAW

        let (shots, raw_only) = scan_report(&dir).unwrap();
        assert!(shots.is_empty());
        assert!(raw_only.is_empty()); // an orphan sidecar is neither a shot nor RAW-only
        std::fs::remove_dir_all(&dir).ok();
    }
```

- [ ] **Step 2: Run test to verify it fails**
Run: `cargo test -p culler-core scan::tests::adobe_sidecar_is_detected`
Expected: FAIL — runtime assertion `assertion \`left == right\` failed` (left: `None`, right: `Some(".../IMG_0001.xmp")`), because `.xmp` files are not yet grouped and the shot sets `sidecar: None`.

- [ ] **Step 3: Write minimal implementation**

Add the `sidecar` field to `Group`:
```rust
/// Files sharing one filename stem, accumulated during the walk.
#[derive(Default)]
struct Group {
    jpeg: Option<PathBuf>,
    raw: Option<PathBuf>,
    sidecar: Option<PathBuf>,
}
```
In `scan_report`'s grouping loop, add an `.xmp` branch after the `RAW_EXTS` branch:
```rust
        } else if ext == "xmp" {
            let stem = sidecar_stem(path);
            groups
                .entry(stem)
                .or_default()
                .sidecar
                .get_or_insert_with(|| path.clone());
        }
```
Change the JPEG-bearing arm so the shot carries the grouped sidecar:
```rust
            Some(jpeg) => shots.push(Shot {
                stem,
                jpeg,
                raw: group.raw,
                sidecar: group.sidecar,
                capture: CaptureTime::default(),
            }),
```
Add the `sidecar_stem` helper (below `file_stem_string`, above the `#[cfg(test)]` module):
```rust
/// The shot stem an `.xmp` sidecar belongs to, handling both conventions:
///  - Adobe:     `IMG_1234.xmp`      → "IMG_1234"
///  - darktable: `IMG_1234.CR3.xmp`  → "IMG_1234" (strip the inner RAW/JPEG ext)
///
/// `file_stem` removes only the trailing ".xmp"; if what remains itself ends in
/// a recognized (case-insensitive) RAW or JPEG extension, that is stripped too.
fn sidecar_stem(path: &Path) -> String {
    let inner = match path.file_stem().and_then(|s| s.to_str()) {
        Some(s) => s,
        None => return String::new(),
    };
    let inner_path = Path::new(inner);
    if let Some(inner_ext) = inner_path.extension().and_then(|e| e.to_str()) {
        let inner_ext = inner_ext.to_ascii_lowercase();
        if RAW_EXTS.contains(&inner_ext.as_str()) || JPEG_EXTS.contains(&inner_ext.as_str()) {
            return inner_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
        }
    }
    inner.to_string()
}
```

- [ ] **Step 4: Run test to verify it passes**
Run: `cargo test -p culler-core scan::`
Expected: PASS — Tasks 1–4 all green.

- [ ] **Step 5: Commit**
```bash
git add culler-core/src/scan.rs
git commit -m "feat(scan): detect Adobe and darktable sidecar conventions" -m "Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: EXIF capture time from the display JPEG

**Files:**
- Modify: `culler-core/Cargo.toml` (add `kamadak-exif`)
- Modify: `culler-core/src/scan.rs`
- Test: `culler-core/src/scan.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `scan_report` (Task 4); Phase 1 `model::CaptureTime`
- Produces:
  - each shot's `capture` is read from its JPEG's EXIF: `DateTimeOriginal` → `CaptureTime.datetime` (the raw `"YYYY:MM:DD HH:MM:SS"` ASCII, verbatim), `SubSecTimeOriginal` → `CaptureTime.subsec` (parsed to `u32`). Undecodable/absent EXIF must **not** fail the scan → `CaptureTime::default()`.
  - **Private helpers (phase-2-local):** `fn read_capture_time(jpeg) -> CaptureTime`, `fn ascii_field(exif, tag) -> Option<String>`.

**Fixture strategy:** No committed binaries. The test builds a minimal valid JPEG in memory (`jpeg_with_exif`) carrying an EXIF APP1 block with a big-endian TIFF holding `DateTimeOriginal` + `SubSecTimeOriginal` — full byte-builder shown, no hand-waving.

- [ ] **Step 1: Write the failing test**

Add inside `scan.rs`'s `#[cfg(test)] mod tests` block (the `jpeg_with_exif` helper is reused in Task 6):
```rust
    /// Build a minimal but valid JPEG carrying an EXIF APP1 block (big-endian
    /// "MM" TIFF) with DateTimeOriginal and SubSecTimeOriginal. Enough for
    /// kamadak-exif to parse; it is not a real image. `subsec` must be ≤ 3 chars
    /// so the ASCII value (plus NUL) fits inline in the 4-byte IFD value field.
    fn jpeg_with_exif(datetime: &str, subsec: &str) -> Vec<u8> {
        fn be16(v: u16) -> [u8; 2] {
            v.to_be_bytes()
        }
        fn be32(v: u32) -> [u8; 4] {
            v.to_be_bytes()
        }

        let mut dt = datetime.as_bytes().to_vec();
        dt.push(0); // NUL-terminate
        let dt_count = dt.len() as u32;

        let mut ss = subsec.as_bytes().to_vec();
        ss.push(0);
        let ss_count = ss.len() as u32;
        assert!(ss_count <= 4, "subsec must fit inline (<= 3 chars)");

        // TIFF offsets, relative to the "MM" byte:
        //   0  TIFF header (8 bytes) → IFD0 at offset 8
        //   8  IFD0:      2 + 1*12 + 4 = 18 bytes → ends at 26
        //   26 Exif IFD:  2 + 2*12 + 4 = 30 bytes → ends at 56
        //   56 DateTimeOriginal string (dt_count bytes)
        const IFD0_OFF: u32 = 8;
        const EXIF_IFD_OFF: u32 = 26;
        const DT_STR_OFF: u32 = 56;

        let mut tiff: Vec<u8> = Vec::new();
        tiff.extend_from_slice(b"MM"); // big-endian byte order
        tiff.extend_from_slice(&be16(42)); // TIFF magic
        tiff.extend_from_slice(&be32(IFD0_OFF));

        // IFD0: one entry, ExifIFDPointer (0x8769, LONG) → EXIF_IFD_OFF
        tiff.extend_from_slice(&be16(1));
        tiff.extend_from_slice(&be16(0x8769));
        tiff.extend_from_slice(&be16(4)); // LONG
        tiff.extend_from_slice(&be32(1)); // count
        tiff.extend_from_slice(&be32(EXIF_IFD_OFF));
        tiff.extend_from_slice(&be32(0)); // no next IFD

        // Exif IFD: two entries
        tiff.extend_from_slice(&be16(2));
        // DateTimeOriginal (0x9003), ASCII → DT_STR_OFF
        tiff.extend_from_slice(&be16(0x9003));
        tiff.extend_from_slice(&be16(2)); // ASCII
        tiff.extend_from_slice(&be32(dt_count));
        tiff.extend_from_slice(&be32(DT_STR_OFF));
        // SubSecTimeOriginal (0x9291), ASCII, stored inline
        tiff.extend_from_slice(&be16(0x9291));
        tiff.extend_from_slice(&be16(2)); // ASCII
        tiff.extend_from_slice(&be32(ss_count));
        let mut inline = [0u8; 4];
        inline[..ss.len()].copy_from_slice(&ss);
        tiff.extend_from_slice(&inline);
        tiff.extend_from_slice(&be32(0)); // no next IFD

        // DateTimeOriginal string payload lands exactly at DT_STR_OFF.
        assert_eq!(tiff.len() as u32, DT_STR_OFF);
        tiff.extend_from_slice(&dt);

        // Wrap the TIFF in a JPEG APP1 "Exif" segment.
        let mut jpeg: Vec<u8> = Vec::new();
        jpeg.extend_from_slice(&[0xFF, 0xD8]); // SOI
        jpeg.extend_from_slice(&[0xFF, 0xE1]); // APP1
        let seg_len = (2 + 6 + tiff.len()) as u16; // len field + "Exif\0\0" + TIFF
        jpeg.extend_from_slice(&be16(seg_len));
        jpeg.extend_from_slice(b"Exif\0\0");
        jpeg.extend_from_slice(&tiff);
        jpeg.extend_from_slice(&[0xFF, 0xD9]); // EOI
        jpeg
    }

    #[test]
    fn reads_datetime_and_subsec_from_exif() {
        let dir = unique_temp_dir("exif");
        std::fs::write(
            dir.join("IMG_0001.JPG"),
            jpeg_with_exif("2026:07:08 10:11:12", "42"),
        )
        .unwrap();

        let shots = scan(&dir).unwrap();
        assert_eq!(shots.len(), 1);
        assert_eq!(
            shots[0].capture.datetime,
            Some("2026:07:08 10:11:12".to_string())
        );
        assert_eq!(shots[0].capture.subsec, Some(42));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn undecodable_jpeg_scans_with_default_capture() {
        let dir = unique_temp_dir("badexif");
        touch(&dir.join("IMG_0001.JPG")); // empty file → no EXIF, must not fail

        let shots = scan(&dir).unwrap();
        assert_eq!(shots.len(), 1);
        assert_eq!(shots[0].capture, CaptureTime::default());
        std::fs::remove_dir_all(&dir).ok();
    }
```

- [ ] **Step 2: Run test to verify it fails**
Run: `cargo test -p culler-core scan::tests::reads_datetime_and_subsec_from_exif`
Expected: FAIL — runtime assertion `assertion \`left == right\` failed` (left: `None`, right: `Some("2026:07:08 10:11:12")`), because the shot still sets `capture: CaptureTime::default()`.

- [ ] **Step 3: Write minimal implementation**

Add the EXIF dependency to `culler-core/Cargo.toml` (package name `kamadak-exif`; the crate is imported as `exif`):
```toml
[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
kamadak-exif = "0.5"
```
In `scan_report`, change the JPEG-bearing arm to read capture time before moving `jpeg` into the shot:
```rust
            Some(jpeg) => {
                let capture = read_capture_time(&jpeg);
                shots.push(Shot {
                    stem,
                    jpeg,
                    raw: group.raw,
                    sidecar: group.sidecar,
                    capture,
                });
            }
```
Add the EXIF readers (below `sidecar_stem`, above the `#[cfg(test)]` module):
```rust
/// Read `DateTimeOriginal` / `SubSecTimeOriginal` from a JPEG's EXIF header.
/// Undecodable or EXIF-less files never fail the scan — they yield a default
/// (empty) `CaptureTime`, so such shots simply sort after all dated ones.
fn read_capture_time(jpeg: &Path) -> CaptureTime {
    let file = match std::fs::File::open(jpeg) {
        Ok(f) => f,
        Err(_) => return CaptureTime::default(),
    };
    let mut reader = std::io::BufReader::new(file);
    let exif = match exif::Reader::new().read_from_container(&mut reader) {
        Ok(e) => e,
        Err(_) => return CaptureTime::default(),
    };
    let datetime = ascii_field(&exif, exif::Tag::DateTimeOriginal);
    let subsec =
        ascii_field(&exif, exif::Tag::SubSecTimeOriginal).and_then(|s| s.trim().parse::<u32>().ok());
    CaptureTime { datetime, subsec }
}

/// The first ASCII string of `tag` in the primary IFD, trimmed. `None` when the
/// tag is absent or not an ASCII value. Returns the bytes verbatim (no reformat),
/// so `DateTimeOriginal` stays the lexically-sortable `"YYYY:MM:DD HH:MM:SS"`.
fn ascii_field(exif: &exif::Exif, tag: exif::Tag) -> Option<String> {
    let field = exif.get_field(tag, exif::In::PRIMARY)?;
    match &field.value {
        exif::Value::Ascii(vec) if !vec.is_empty() => {
            Some(String::from_utf8_lossy(&vec[0]).trim().to_string())
        }
        _ => None,
    }
}
```

- [ ] **Step 4: Run test to verify it passes**
Run: `cargo test -p culler-core scan::`
Expected: PASS — Tasks 1–5 all green (EXIF read, and the empty-file shot keeps a default capture).

- [ ] **Step 5: Commit**
```bash
git add culler-core/Cargo.toml culler-core/src/scan.rs
git commit -m "feat(scan): read EXIF capture time from the display JPEG" -m "Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 6: Stable capture-time sort (undated last)

**Files:**
- Modify: `culler-core/src/scan.rs`
- Test: `culler-core/src/scan.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `scan_report` (Task 5); `jpeg_with_exif`, `unique_temp_dir`, `touch`, `stems` test helpers (Tasks 1, 5)
- Produces: shots returned in `(capture.datetime, capture.subsec, jpeg filename)` order; shots with `datetime == None` sort **after** all dated shots, then by filename — keeping burst order stable across sessions. No new public symbols.
  - **Private helpers (phase-2-local):** `fn sort_shots(shots: &mut Vec<Shot>)`, `fn sort_key(shot) -> (bool, Option<String>, Option<u32>, String)`, `fn file_name_string(path) -> String`.

- [ ] **Step 1: Write the failing test**

Add inside `scan.rs`'s `#[cfg(test)] mod tests` block:
```rust
    #[test]
    fn shots_sort_by_capture_time_with_stable_tiebreakers() {
        let dir = unique_temp_dir("sort");

        // Same datetime AND subsec → filename tiebreak (a_same < b_same).
        std::fs::write(
            dir.join("a_same.jpg"),
            jpeg_with_exif("2026:07:08 09:00:00", "20"),
        )
        .unwrap();
        std::fs::write(
            dir.join("b_same.jpg"),
            jpeg_with_exif("2026:07:08 09:00:00", "20"),
        )
        .unwrap();
        // Same datetime, different subsec → subsec tiebreak (lo < hi).
        std::fs::write(
            dir.join("sub_hi.jpg"),
            jpeg_with_exif("2026:07:08 09:00:01", "50"),
        )
        .unwrap();
        std::fs::write(
            dir.join("sub_lo.jpg"),
            jpeg_with_exif("2026:07:08 09:00:01", "05"),
        )
        .unwrap();
        // Latest dated shot.
        std::fs::write(
            dir.join("late.jpg"),
            jpeg_with_exif("2026:07:08 10:00:00", "00"),
        )
        .unwrap();
        // Undated shots → after all dated, then by filename.
        touch(&dir.join("undated_a.jpg"));
        touch(&dir.join("undated_b.jpg"));

        let shots = scan(&dir).unwrap();
        assert_eq!(
            stems(&shots),
            vec![
                "a_same".to_string(),
                "b_same".to_string(),
                "sub_lo".to_string(),
                "sub_hi".to_string(),
                "late".to_string(),
                "undated_a".to_string(),
                "undated_b".to_string(),
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn undated_shots_alone_sort_by_filename() {
        let dir = unique_temp_dir("undated");
        touch(&dir.join("IMG_0003.JPG"));
        touch(&dir.join("IMG_0001.JPG"));
        touch(&dir.join("IMG_0002.JPG"));

        let shots = scan(&dir).unwrap();
        assert_eq!(
            stems(&shots),
            vec![
                "IMG_0001".to_string(),
                "IMG_0002".to_string(),
                "IMG_0003".to_string()
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }
```

- [ ] **Step 2: Run test to verify it fails**
Run: `cargo test -p culler-core scan::tests::shots_sort_by_capture_time_with_stable_tiebreakers`
Expected: FAIL — runtime assertion on the stem order: without sorting, shots come out in BTreeMap stem order (`a_same, b_same, late, sub_hi, sub_lo, undated_a, undated_b`), not capture-time order.

- [ ] **Step 3: Write minimal implementation**

In `scan_report`, sort the shots just before returning — replace `Ok((shots, raw_only))` with:
```rust
    sort_shots(&mut shots);
    Ok((shots, raw_only))
```
Add the sort helpers (below `ascii_field`, above the `#[cfg(test)]` module):
```rust
/// Stable filmstrip order: by (capture.datetime, capture.subsec, jpeg filename).
/// Shots with no datetime sort AFTER all dated shots, then by filename, so burst
/// order stays put across sessions.
fn sort_shots(shots: &mut Vec<Shot>) {
    shots.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
}

/// Ordering key. The leading bool puts dated shots (`false`) before undated
/// (`true`); `Option<u32>`/`String` then order within each group.
fn sort_key(shot: &Shot) -> (bool, Option<String>, Option<u32>, String) {
    (
        shot.capture.datetime.is_none(),
        shot.capture.datetime.clone(),
        shot.capture.subsec,
        file_name_string(&shot.jpeg),
    )
}

/// The final path component (with extension) as an owned `String`.
fn file_name_string(path: &Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string()
}
```

- [ ] **Step 4: Run test to verify it passes**
Run: `cargo test -p culler-core scan::`
Expected: PASS — the full `scan` suite (Tasks 1–6) is green.

- [ ] **Step 5: Commit**
```bash
git add culler-core/src/scan.rs
git commit -m "feat(scan): sort shots by capture time with undated-last tiebreakers" -m "Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Phase 2 done — verification

Run the whole workspace suite and build to confirm the phase deliverable (folder → sorted `Vec<Shot>` with pairing, RAW + both sidecar conventions, EXIF capture-time sort, RAW-only reporting):

```bash
cargo test --workspace
cargo build --workspace
```

Expected: all tests green; both crates compile. `culler-core` still carries no GUI dependency (it now also depends on `kamadak-exif`). Phase 3 consumes `scan`/`scan_report` and the Phase 1 `model` types (canonical signatures in [README.md](README.md)).
