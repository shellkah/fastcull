# FastCull Phase 1 — Core Domain & Persistence — Implementation Plan

> **STATUS: LANDED on `master`** (fast-forward 42ba998..100daee, 2026-07-08; final review "ready to merge"; 38 tests green). **The code on master is authoritative, not this document.** Two post-plan review fixes are folded into the snippets below so this document no longer teaches the defective versions: `9b4f6b6` (`save` fsyncs the temp file before the rename) and `e1a20b1` (corrupt-session quarantine uses the first FREE numbered `.bad` / `.bad.1` / `.bad.2`… sibling instead of clobbering). Post-landing the workspace was also bumped to **edition 2024 / resolver 3** (2026-07-09). Do not re-execute this plan; use it as the design record.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax. Canonical types/constants are in [README.md](README.md); use them verbatim.

**Goal:** Stand up the Cargo workspace and build `culler-core`'s pure, GUI-free domain engine — bucket/tier model, per-shot decisions, the in-memory `Session` state machine with bounded undo, and an atomic, corruption-safe JSON session sidecar — all fully unit-tested.

**Architecture:** A two-crate workspace: `culler-core` (library, zero GUI deps) holds all domain logic; `culler` (binary) is a compile-only stub in this phase. Phase 1 delivers two `culler-core` modules — `model` (types + pure `Session` transitions) and `persist` (atomic save / load / corrupt-quarantine of the `.fastcull.json` sidecar). No pixels, no file moves; just a resumable, testable state engine.

**Tech Stack:** Rust 2024, serde/serde_json.

## Global Constraints

These bind every task in this phase (copied from [README.md](README.md)):

- **Language / edition:** Rust, edition 2024 (workspace resolver 3). Workspace with two member crates: `culler-core` (lib) and `culler` (bin).
- **`culler-core` has zero GUI dependencies.** No `slint`, no Slint types, in the library.
- **Nothing touches disk until Apply.** All culling decisions live in memory + the autosaved session sidecar. Phase 1 delivers exactly that sidecar; it performs **no file moves** and no destructive I/O.
- **Atomic writes everywhere:** session saves use write-temp-then-**fsync**-then-rename (write `<path>.tmp`, `sync_all`, then `rename` over the target — without the fsync a crash can commit the rename before the data blocks land, publishing a truncated sidecar).
- **Decisions are keyed by filename stem** so resume re-attaches them after a rescan.
- **Corrupt session file → renamed to the first FREE `.fastcull.json.bad` / `.bad.1` / `.bad.2`… sibling**, reported, and a fresh session is started — never silently overwritten, and a later corruption never clobbers earlier quarantined evidence.
- **Undo is bounded** at `UNDO_LIMIT = 200`; oldest entries drop when exceeded. `set_tier` / `add_tag` / `set_tags` push the PREVIOUS decision onto the undo stack; `mark_visited` does NOT. The undo stack is `#[serde(skip)]`.
- **Platform:** Linux only.
- **TDD, DRY, YAGNI, frequent commits.** Every task: failing test → run-it-fails → minimal impl → run-it-passes → commit. Conventional-commit messages (`feat:`, `test:`, `chore:`, `refactor:`).
- **No v1 config file.** Configurable bucket names are the `BUCKET_*` constants (CLI flags override them in later phases, not here).

---

### Task 1: Workspace scaffolding

**Files:**
- Create: `Cargo.toml` (workspace root)
- Create: `.gitignore`
- Create: `culler-core/Cargo.toml`
- Create: `culler-core/src/lib.rs`
- Create: `culler/Cargo.toml`
- Create: `culler/src/main.rs`

**Interfaces:**
- Consumes: none
- Produces: a buildable workspace; `culler-core` lib target and `culler` bin target both compile. Establishes the crate boundary every later task builds inside.

> This task is scaffolding, so it uses build/run/commit rather than the fail-first micro-cycle (there is no domain code to fail yet). The placeholder test proves `cargo test` runs.

- [ ] **Step 1: Create the workspace and crate files**

`Cargo.toml` (workspace root):
```toml
[workspace]
resolver = "2"
members = ["culler-core", "culler"]
```

`.gitignore`:
```gitignore
/target
```

`culler-core/Cargo.toml`:
```toml
[package]
name = "culler-core"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

`culler-core/src/lib.rs`:
```rust
//! culler-core: the pure, GUI-free domain library for FastCull.
//!
//! Modules land as phases progress: `model` + `persist` (phase 1),
//! then `scan`, `xmp`, `plan`, `apply`, `decode` in later phases.
//! Nothing in this crate depends on Slint or any GUI type.

#[cfg(test)]
mod tests {
    #[test]
    fn crate_builds() {
        assert_eq!(2 + 2, 4);
    }
}
```

`culler/Cargo.toml`:
```toml
[package]
name = "culler"
version = "0.1.0"
edition = "2021"

[dependencies]
culler-core = { path = "../culler-core" }
```

`culler/src/main.rs`:
```rust
fn main() {
    // Phase 6 replaces this stub with the Slint app (parse args, build/resume
    // session, spawn decode workers, run the event loop).
    println!("fastcull");
}
```

- [ ] **Step 2: Build the workspace**
Run: `cargo build --workspace`
Expected: PASS — both `culler-core` and `culler` compile with no errors.

- [ ] **Step 3: Run the placeholder test**
Run: `cargo test --workspace -- --nocapture`
Expected: PASS — `culler-core::tests::crate_builds` runs and passes; `culler` has no tests.

- [ ] **Step 4: Commit**
```bash
git add Cargo.toml .gitignore culler-core/Cargo.toml culler-core/src/lib.rs culler/Cargo.toml culler/src/main.rs
git commit -m "chore: scaffold cargo workspace with culler-core lib and culler bin" -m "Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: Bucket / file constants + `Tier`

**Files:**
- Create: `culler-core/src/model.rs`
- Modify: `culler-core/src/lib.rs` (add `pub mod model;`)
- Test: `culler-core/src/model.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: none
- Produces:
  - `pub const BUCKET_REJECTED/BUCKET_REST/BUCKET_KEEP/BUCKET_PICKS/BUCKET_BESTS: &str`
  - `pub const SESSION_FILE/SESSION_BAD_FILE/JOURNAL_FILE: &str`
  - `pub const RAW_EXTS: &[&str]`, `pub const JPEG_EXTS: &[&str]`
  - `pub const UNDO_LIMIT: usize`
  - `pub enum Tier { Reject, Keep, Pick, Best }` with `fn rank(self) -> i8`, `fn bucket(self) -> &'static str`, `fn xmp_rating(self) -> i32`

- [ ] **Step 1: Write the failing test**

Add `pub mod model;` as the first line of `culler-core/src/lib.rs` (above the `#[cfg(test)]` block), then create `culler-core/src/model.rs` containing ONLY the tests module (implementation comes in Step 3):
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_constants_have_expected_names() {
        assert_eq!(BUCKET_REJECTED, "00_rejected");
        assert_eq!(BUCKET_REST, "01_rest");
        assert_eq!(BUCKET_KEEP, "02_keep");
        assert_eq!(BUCKET_PICKS, "03_picks");
        assert_eq!(BUCKET_BESTS, "04_bests");
    }

    #[test]
    fn sidecar_and_journal_file_names() {
        assert_eq!(SESSION_FILE, ".fastcull.json");
        assert_eq!(SESSION_BAD_FILE, ".fastcull.json.bad");
        assert_eq!(JOURNAL_FILE, ".fastcull-apply.json");
    }

    #[test]
    fn extension_tables_are_lowercase_no_dot() {
        assert!(RAW_EXTS.contains(&"cr3"));
        assert!(RAW_EXTS.contains(&"dng"));
        assert!(JPEG_EXTS.contains(&"jpg"));
        assert!(JPEG_EXTS.contains(&"jpeg"));
        assert!(RAW_EXTS.iter().all(|e| e == &e.to_lowercase() && !e.starts_with('.')));
    }

    #[test]
    fn undo_limit_is_two_hundred() {
        assert_eq!(UNDO_LIMIT, 200);
    }

    #[test]
    fn tier_rank_orders_quality_ladder() {
        assert_eq!(Tier::Reject.rank(), -1);
        assert_eq!(Tier::Keep.rank(), 1);
        assert_eq!(Tier::Pick.rank(), 2);
        assert_eq!(Tier::Best.rank(), 3);
        // Reject < Rest(0) < Keep < Pick < Best
        assert!(Tier::Reject.rank() < 0);
        assert!(Tier::Keep.rank() > 0);
        assert!(Tier::Keep.rank() < Tier::Pick.rank());
        assert!(Tier::Pick.rank() < Tier::Best.rank());
    }

    #[test]
    fn tier_bucket_maps_to_constants() {
        assert_eq!(Tier::Reject.bucket(), BUCKET_REJECTED);
        assert_eq!(Tier::Keep.bucket(), BUCKET_KEEP);
        assert_eq!(Tier::Pick.bucket(), BUCKET_PICKS);
        assert_eq!(Tier::Best.bucket(), BUCKET_BESTS);
    }

    #[test]
    fn tier_xmp_rating_matches_spec() {
        assert_eq!(Tier::Reject.xmp_rating(), -1);
        assert_eq!(Tier::Keep.xmp_rating(), 3);
        assert_eq!(Tier::Pick.xmp_rating(), 4);
        assert_eq!(Tier::Best.xmp_rating(), 5);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**
Run: `cargo test -p culler-core`
Expected: FAIL — compile error, e.g. `error[E0425]: cannot find value \`BUCKET_REJECTED\` in this scope` and `error[E0433]: failed to resolve: use of undeclared type \`Tier\``.

- [ ] **Step 3: Write minimal implementation**

Insert this ABOVE the `#[cfg(test)]` module in `culler-core/src/model.rs`:
```rust
//! Core domain model: bucket layout, tiers, decisions, shots, and the pure
//! in-memory `Session` state engine. Zero GUI dependencies.

// ---- Bucket layout (defaults; the binary may override names via CLI later) ----
pub const BUCKET_REJECTED: &str = "00_rejected";
pub const BUCKET_REST: &str = "01_rest";
pub const BUCKET_KEEP: &str = "02_keep";
pub const BUCKET_PICKS: &str = "03_picks";
pub const BUCKET_BESTS: &str = "04_bests";

// ---- Session / journal sidecar file names ----
pub const SESSION_FILE: &str = ".fastcull.json"; // in source dir
pub const SESSION_BAD_FILE: &str = ".fastcull.json.bad"; // corrupt-session rename target
pub const JOURNAL_FILE: &str = ".fastcull-apply.json"; // in dest dir (used in phase 4)

// ---- Recognized extensions (compared case-insensitively, no leading dot) ----
pub const RAW_EXTS: &[&str] =
    &["cr3", "cr2", "nef", "arw", "raf", "rw2", "orf", "dng", "pef", "srw"];
pub const JPEG_EXTS: &[&str] = &["jpg", "jpeg"];

// ---- Bounded undo stack limit ----
pub const UNDO_LIMIT: usize = 200;

#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Tier {
    Reject,
    Keep,
    Pick,
    Best,
}

impl Tier {
    /// Ordering on the quality ladder Reject < Rest(None) < Keep < Pick < Best.
    /// Rest/None = 0 and is handled at the call site.
    pub fn rank(self) -> i8 {
        match self {
            Tier::Reject => -1,
            Tier::Keep => 1,
            Tier::Pick => 2,
            Tier::Best => 3,
        }
    }

    /// Destination bucket name for this tier.
    pub fn bucket(self) -> &'static str {
        match self {
            Tier::Reject => BUCKET_REJECTED,
            Tier::Keep => BUCKET_KEEP,
            Tier::Pick => BUCKET_PICKS,
            Tier::Best => BUCKET_BESTS,
        }
    }

    /// XMP rating written on Apply (Bridge/darktable convention: reject = -1).
    pub fn xmp_rating(self) -> i32 {
        match self {
            Tier::Reject => -1,
            Tier::Keep => 3,
            Tier::Pick => 4,
            Tier::Best => 5,
        }
    }
}
```

- [ ] **Step 4: Run test to verify it passes**
Run: `cargo test -p culler-core`
Expected: PASS — all seven tests green.

- [ ] **Step 5: Commit**
```bash
git add culler-core/src/lib.rs culler-core/src/model.rs
git commit -m "feat(model): add bucket constants and Tier with rank/bucket/xmp_rating" -m "Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: `Decision` + helpers

**Files:**
- Modify: `culler-core/src/model.rs`
- Test: `culler-core/src/model.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `Tier` (Task 2); `BUCKET_REST`, `BUCKET_*` (Task 2)
- Produces:
  - `pub struct Decision { pub tier: Option<Tier>, pub tags: Vec<String>, pub visited: bool }` (derives `Default`)
  - `fn bucket(&self) -> &'static str`, `fn xmp_rating(&self) -> Option<i32>`, `fn is_undecided(&self) -> bool`

- [ ] **Step 1: Write the failing test**

Add these test functions inside `model.rs`'s `#[cfg(test)] mod tests` block (before its closing brace):
```rust
    #[test]
    fn decision_default_is_undecided_rest() {
        let d = Decision::default();
        assert!(d.is_undecided());
        assert_eq!(d.bucket(), BUCKET_REST);
        assert_eq!(d.xmp_rating(), None);
        assert!(d.tags.is_empty());
        assert!(!d.visited);
    }

    #[test]
    fn decision_bucket_and_rating_follow_tier() {
        let pick = Decision {
            tier: Some(Tier::Pick),
            tags: vec![],
            visited: true,
        };
        assert!(!pick.is_undecided());
        assert_eq!(pick.bucket(), BUCKET_PICKS);
        assert_eq!(pick.xmp_rating(), Some(4));

        let reject = Decision {
            tier: Some(Tier::Reject),
            tags: vec![],
            visited: true,
        };
        assert_eq!(reject.bucket(), BUCKET_REJECTED);
        assert_eq!(reject.xmp_rating(), Some(-1));
    }
```

- [ ] **Step 2: Run test to verify it fails**
Run: `cargo test -p culler-core decision_`
Expected: FAIL — compile error `error[E0433]: failed to resolve: use of undeclared type \`Decision\``.

- [ ] **Step 3: Write minimal implementation**

Insert ABOVE the `#[cfg(test)]` module in `model.rs`:
```rust
#[derive(Clone, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Decision {
    pub tier: Option<Tier>, // None = undecided/Rest → 01_rest on Apply
    pub tags: Vec<String>,
    pub visited: bool, // set the first time the shot is shown in the loupe
}

impl Decision {
    /// Destination bucket: the tier's bucket, or `BUCKET_REST` when undecided.
    pub fn bucket(&self) -> &'static str {
        self.tier.map(Tier::bucket).unwrap_or(BUCKET_REST)
    }

    /// XMP rating for this decision, or `None` when undecided.
    pub fn xmp_rating(&self) -> Option<i32> {
        self.tier.map(Tier::xmp_rating)
    }

    /// True when no tier has been assigned (undecided / residual Rest).
    pub fn is_undecided(&self) -> bool {
        self.tier.is_none()
    }
}
```

- [ ] **Step 4: Run test to verify it passes**
Run: `cargo test -p culler-core decision_`
Expected: PASS.

- [ ] **Step 5: Commit**
```bash
git add culler-core/src/model.rs
git commit -m "feat(model): add Decision with bucket/xmp_rating/is_undecided" -m "Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: `CaptureTime` + `Shot` + `files()`

**Files:**
- Modify: `culler-core/src/model.rs`
- Test: `culler-core/src/model.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: none new (self-contained pure model types)
- Produces:
  - `pub struct CaptureTime { pub datetime: Option<String>, pub subsec: Option<u32> }` (derives `Default`)
  - `pub struct Shot { pub stem: String, pub jpeg: PathBuf, pub raw: Option<PathBuf>, pub sidecar: Option<PathBuf>, pub capture: CaptureTime }`
  - `fn files(&self) -> Vec<PathBuf>` (jpeg, raw?, sidecar? — in that move order)

- [ ] **Step 1: Write the failing test**

Add inside `model.rs`'s `#[cfg(test)] mod tests` block:
```rust
    #[test]
    fn shot_files_lists_jpeg_only_when_no_siblings() {
        let shot = Shot {
            stem: "IMG_1234".to_string(),
            jpeg: std::path::PathBuf::from("/src/IMG_1234.JPG"),
            raw: None,
            sidecar: None,
            capture: CaptureTime::default(),
        };
        assert_eq!(
            shot.files(),
            vec![std::path::PathBuf::from("/src/IMG_1234.JPG")]
        );
    }

    #[test]
    fn shot_files_orders_jpeg_then_raw_then_sidecar() {
        let shot = Shot {
            stem: "IMG_1234".to_string(),
            jpeg: std::path::PathBuf::from("/src/IMG_1234.JPG"),
            raw: Some(std::path::PathBuf::from("/src/IMG_1234.CR3")),
            sidecar: Some(std::path::PathBuf::from("/src/IMG_1234.xmp")),
            capture: CaptureTime {
                datetime: Some("2026:07:08 10:11:12".to_string()),
                subsec: Some(42),
            },
        };
        assert_eq!(
            shot.files(),
            vec![
                std::path::PathBuf::from("/src/IMG_1234.JPG"),
                std::path::PathBuf::from("/src/IMG_1234.CR3"),
                std::path::PathBuf::from("/src/IMG_1234.xmp"),
            ]
        );
    }

    #[test]
    fn capture_time_default_is_empty() {
        let c = CaptureTime::default();
        assert_eq!(c.datetime, None);
        assert_eq!(c.subsec, None);
    }
```

- [ ] **Step 2: Run test to verify it fails**
Run: `cargo test -p culler-core shot_files`
Expected: FAIL — compile error `error[E0433]: failed to resolve: use of undeclared type \`Shot\`` (and `CaptureTime`).

- [ ] **Step 3: Write minimal implementation**

Insert ABOVE the `#[cfg(test)]` module in `model.rs`:
```rust
/// A capture instant, string-comparable straight from EXIF. Pure model type
/// (no exif dependency — `scan` fills it in a later phase).
#[derive(Clone, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct CaptureTime {
    /// "YYYY:MM:DD HH:MM:SS" exactly as EXIF stores it (lexically sortable).
    pub datetime: Option<String>,
    /// SubSecTimeOriginal parsed to a number.
    pub subsec: Option<u32>,
}

/// One shot = all files sharing a filename stem. Produced by `scan`.
#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct Shot {
    pub stem: String, // the shot key, e.g. "IMG_1234" (case preserved as on disk)
    pub jpeg: std::path::PathBuf, // display file, required in v1
    pub raw: Option<std::path::PathBuf>,
    pub sidecar: Option<std::path::PathBuf>, // pre-existing xmp (either convention)
    pub capture: CaptureTime,
}

impl Shot {
    /// All on-disk files belonging to this shot, in move order: jpeg, raw?, sidecar?.
    pub fn files(&self) -> Vec<std::path::PathBuf> {
        let mut out = Vec::with_capacity(3);
        out.push(self.jpeg.clone());
        if let Some(raw) = &self.raw {
            out.push(raw.clone());
        }
        if let Some(sidecar) = &self.sidecar {
            out.push(sidecar.clone());
        }
        out
    }
}
```

- [ ] **Step 4: Run test to verify it passes**
Run: `cargo test -p culler-core`
Expected: PASS — all model tests so far are green.

- [ ] **Step 5: Commit**
```bash
git add culler-core/src/model.rs
git commit -m "feat(model): add CaptureTime and Shot with files()" -m "Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: `Session` skeleton + `UndoEntry` + `TierCounts` + `decision()` + serde

**Files:**
- Modify: `culler-core/src/model.rs`
- Test: `culler-core/src/model.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `Decision` (Task 3), `Shot` (Task 4)
- Produces:
  - `pub struct UndoEntry { pub stem: String, pub previous: Decision }`
  - `pub struct Session { pub source_dir: PathBuf, pub shots: Vec<Shot>, pub decisions: HashMap<String, Decision>, pub current: usize, #[serde(skip)] pub undo: Vec<UndoEntry> }` (derives `Default`)
  - `pub struct TierCounts { pub rejected, rest, keep, picks, bests: usize }` (derives `Default`, `Copy`)
  - `fn decision(&self, index: usize) -> &Decision` (returns a reference to a stored default `Decision` when the shot has none / index is out of range)

- [ ] **Step 1: Write the failing test**

Add inside `model.rs`'s `#[cfg(test)] mod tests` block:
```rust
    #[test]
    fn session_serde_round_trips_and_skips_undo() {
        let mut session = Session {
            source_dir: std::path::PathBuf::from("/src"),
            shots: vec![Shot {
                stem: "IMG_0001".to_string(),
                jpeg: std::path::PathBuf::from("/src/IMG_0001.JPG"),
                raw: None,
                sidecar: None,
                capture: CaptureTime::default(),
            }],
            decisions: std::collections::HashMap::new(),
            current: 0,
            undo: vec![UndoEntry {
                stem: "IMG_0001".to_string(),
                previous: Decision::default(),
            }],
        };
        session.decisions.insert(
            "IMG_0001".to_string(),
            Decision {
                tier: Some(Tier::Keep),
                tags: vec!["sky".to_string()],
                visited: true,
            },
        );

        let json = serde_json::to_string(&session).unwrap();
        // #[serde(skip)] means the undo stack is never serialized.
        assert!(!json.contains("undo"));

        let restored: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.source_dir, session.source_dir);
        assert_eq!(restored.shots, session.shots);
        assert_eq!(restored.decisions, session.decisions);
        assert_eq!(restored.current, 0);
        assert!(restored.undo.is_empty()); // skipped on ser, defaults on de
    }

    #[test]
    fn session_decision_returns_stored_default_when_absent() {
        let session = Session {
            source_dir: std::path::PathBuf::from("/src"),
            shots: vec![Shot {
                stem: "IMG_0001".to_string(),
                jpeg: std::path::PathBuf::from("/src/IMG_0001.JPG"),
                raw: None,
                sidecar: None,
                capture: CaptureTime::default(),
            }],
            decisions: std::collections::HashMap::new(),
            current: 0,
            undo: Vec::new(),
        };
        let d = session.decision(0);
        assert!(d.is_undecided());
        assert_eq!(d, &Decision::default());
        // Out-of-range index is also a default view, never a panic.
        assert_eq!(session.decision(99), &Decision::default());
    }

    #[test]
    fn session_decision_returns_stored_value_when_present() {
        let mut session = Session::default();
        session.shots.push(Shot {
            stem: "IMG_0001".to_string(),
            jpeg: std::path::PathBuf::from("/src/IMG_0001.JPG"),
            raw: None,
            sidecar: None,
            capture: CaptureTime::default(),
        });
        session.decisions.insert(
            "IMG_0001".to_string(),
            Decision {
                tier: Some(Tier::Best),
                tags: vec![],
                visited: true,
            },
        );
        assert_eq!(session.decision(0).tier, Some(Tier::Best));
        assert!(session.decision(0).visited);
    }

    #[test]
    fn tier_counts_default_is_all_zero() {
        assert_eq!(
            TierCounts::default(),
            TierCounts {
                rejected: 0,
                rest: 0,
                keep: 0,
                picks: 0,
                bests: 0
            }
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**
Run: `cargo test -p culler-core session_`
Expected: FAIL — compile error `error[E0433]: failed to resolve: use of undeclared type \`Session\`` (and `UndoEntry`, `TierCounts`).

- [ ] **Step 3: Write minimal implementation**

Insert ABOVE the `#[cfg(test)]` module in `model.rs`:
```rust
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UndoEntry {
    pub stem: String,
    pub previous: Decision,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Session {
    pub source_dir: std::path::PathBuf,
    pub shots: Vec<Shot>,
    /// Keyed by `Shot.stem` so resume re-attaches decisions after a rescan.
    pub decisions: std::collections::HashMap<String, Decision>,
    pub current: usize, // index into `shots`
    #[serde(skip)]
    pub undo: Vec<UndoEntry>, // bounded (UNDO_LIMIT), most-recent last
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct TierCounts {
    pub rejected: usize,
    pub rest: usize,
    pub keep: usize,
    pub picks: usize,
    pub bests: usize,
}

impl Session {
    /// The decision for `shots[index]` (keyed by its stem), or a reference to a
    /// stored default `Decision` when the shot has no recorded decision or the
    /// index is out of range. Never panics.
    pub fn decision(&self, index: usize) -> &Decision {
        static DEFAULT: Decision = Decision {
            tier: None,
            tags: Vec::new(),
            visited: false,
        };
        self.shots
            .get(index)
            .and_then(|shot| self.decisions.get(&shot.stem))
            .unwrap_or(&DEFAULT)
    }
}
```

- [ ] **Step 4: Run test to verify it passes**
Run: `cargo test -p culler-core`
Expected: PASS.

- [ ] **Step 5: Commit**
```bash
git add culler-core/src/model.rs
git commit -m "feat(model): add Session, UndoEntry, TierCounts and decision()" -m "Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 6: `Session` tier/tag transitions + bounded undo

**Files:**
- Modify: `culler-core/src/model.rs`
- Test: `culler-core/src/model.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `Session`, `Decision`, `UndoEntry`, `Tier`, `UNDO_LIMIT` (Tasks 2–5)
- Produces:
  - `fn set_tier(&mut self, index: usize, tier: Option<Tier>)` — records undo; does NOT auto-advance
  - `fn add_tag(&mut self, index: usize, tag: String)` — records undo; dedupes
  - `fn set_tags(&mut self, index: usize, tags: Vec<String>)` — records undo; dedupes (first occurrence wins)
  - `fn mark_visited(&mut self, index: usize)` — idempotent; NO undo entry
  - `fn undo(&mut self) -> bool` — pops last `UndoEntry`, restores decision, `false` if empty
  - **Private helper (new, phase-1-local):** `fn record_undo(&mut self, stem: String, previous: Decision)` — pushes onto `undo` and drops the oldest when `undo.len() > UNDO_LIMIT`. Not in the README; internal to this phase.

- [ ] **Step 1: Write the failing test**

Add inside `model.rs`'s `#[cfg(test)] mod tests` block. First add a small fixture helper (used here and in Task 7), then the tests:
```rust
    /// Build a session of undecided shots with the given stems.
    fn fixture_session(stems: &[&str]) -> Session {
        let mut session = Session::default();
        for stem in stems {
            session.shots.push(Shot {
                stem: (*stem).to_string(),
                jpeg: std::path::PathBuf::from(format!("/src/{stem}.JPG")),
                raw: None,
                sidecar: None,
                capture: CaptureTime::default(),
            });
        }
        session
    }

    #[test]
    fn set_tier_records_undo_and_undo_restores_stepwise() {
        let mut session = fixture_session(&["A"]);

        session.set_tier(0, Some(Tier::Keep));
        assert_eq!(session.decision(0).tier, Some(Tier::Keep));
        assert_eq!(session.undo.len(), 1);

        session.set_tier(0, Some(Tier::Best));
        assert_eq!(session.decision(0).tier, Some(Tier::Best));
        assert_eq!(session.undo.len(), 2);

        assert!(session.undo());
        assert_eq!(session.decision(0).tier, Some(Tier::Keep));
        assert!(session.undo());
        assert_eq!(session.decision(0).tier, None); // back to undecided
        assert!(!session.undo()); // stack empty
    }

    #[test]
    fn set_tier_preserves_existing_tags_and_visited() {
        let mut session = fixture_session(&["A"]);
        session.mark_visited(0);
        session.add_tag(0, "sky".to_string());
        session.set_tier(0, Some(Tier::Pick));
        assert_eq!(session.decision(0).tier, Some(Tier::Pick));
        assert_eq!(session.decision(0).tags, vec!["sky".to_string()]);
        assert!(session.decision(0).visited);
    }

    #[test]
    fn add_tag_dedupes_and_records_undo() {
        let mut session = fixture_session(&["A"]);
        session.add_tag(0, "sky".to_string());
        session.add_tag(0, "sky".to_string()); // duplicate ignored
        session.add_tag(0, "tree".to_string());
        assert_eq!(
            session.decision(0).tags,
            vec!["sky".to_string(), "tree".to_string()]
        );

        assert!(session.undo()); // reverts the "tree" add
        assert_eq!(session.decision(0).tags, vec!["sky".to_string()]);
    }

    #[test]
    fn set_tags_replaces_and_dedupes_preserving_order() {
        let mut session = fixture_session(&["A"]);
        session.set_tags(
            0,
            vec!["a".to_string(), "b".to_string(), "a".to_string()],
        );
        assert_eq!(
            session.decision(0).tags,
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn mark_visited_is_idempotent_and_records_no_undo() {
        let mut session = fixture_session(&["A"]);
        session.mark_visited(0);
        session.mark_visited(0);
        assert!(session.decision(0).visited);
        assert!(session.undo.is_empty());
    }

    #[test]
    fn undo_stack_is_bounded_at_limit() {
        let mut session = fixture_session(&["A"]);
        for _ in 0..(UNDO_LIMIT + 50) {
            session.add_tag(0, "x".to_string());
        }
        assert_eq!(session.undo.len(), UNDO_LIMIT);
    }

    #[test]
    fn transitions_on_out_of_range_index_are_no_ops() {
        let mut session = fixture_session(&["A"]);
        session.set_tier(99, Some(Tier::Keep));
        session.add_tag(99, "x".to_string());
        session.set_tags(99, vec!["y".to_string()]);
        session.mark_visited(99);
        assert!(session.undo.is_empty());
        assert!(session.decisions.is_empty());
    }
```

- [ ] **Step 2: Run test to verify it fails**
Run: `cargo test -p culler-core set_tier`
Expected: FAIL — compile error `error[E0599]: no method named \`set_tier\` found for struct \`Session\``.

- [ ] **Step 3: Write minimal implementation**

Insert a NEW `impl Session { ... }` block ABOVE the `#[cfg(test)]` module in `model.rs`:
```rust
impl Session {
    /// Assign (or clear, with `None`) the tier for `shots[index]`. Records the
    /// previous decision on the undo stack. Does NOT auto-advance — the input
    /// layer owns navigation. No-op if `index` is out of range.
    pub fn set_tier(&mut self, index: usize, tier: Option<Tier>) {
        let stem = match self.shots.get(index) {
            Some(shot) => shot.stem.clone(),
            None => return,
        };
        let previous = self.decisions.get(&stem).cloned().unwrap_or_default();
        self.record_undo(stem.clone(), previous);
        self.decisions.entry(stem).or_default().tier = tier;
    }

    /// Add a single tag to `shots[index]`, ignoring duplicates. Records undo.
    /// No-op if `index` is out of range.
    pub fn add_tag(&mut self, index: usize, tag: String) {
        let stem = match self.shots.get(index) {
            Some(shot) => shot.stem.clone(),
            None => return,
        };
        let previous = self.decisions.get(&stem).cloned().unwrap_or_default();
        self.record_undo(stem.clone(), previous);
        let entry = self.decisions.entry(stem).or_default();
        if !entry.tags.contains(&tag) {
            entry.tags.push(tag);
        }
    }

    /// Replace all tags on `shots[index]` with `tags`, dropping duplicates and
    /// keeping first-occurrence order. Records undo. No-op if out of range.
    pub fn set_tags(&mut self, index: usize, tags: Vec<String>) {
        let stem = match self.shots.get(index) {
            Some(shot) => shot.stem.clone(),
            None => return,
        };
        let previous = self.decisions.get(&stem).cloned().unwrap_or_default();
        self.record_undo(stem.clone(), previous);
        let mut deduped: Vec<String> = Vec::with_capacity(tags.len());
        for t in tags {
            if !deduped.contains(&t) {
                deduped.push(t);
            }
        }
        self.decisions.entry(stem).or_default().tags = deduped;
    }

    /// Mark `shots[index]` as seen. Idempotent; records NO undo entry.
    /// No-op if `index` is out of range.
    pub fn mark_visited(&mut self, index: usize) {
        let stem = match self.shots.get(index) {
            Some(shot) => shot.stem.clone(),
            None => return,
        };
        self.decisions.entry(stem).or_default().visited = true;
    }

    /// Revert the most recent tier/tag change. Returns `false` if the stack is
    /// empty. Restoring a previously-absent decision stores its default value,
    /// which is equivalent to absence for every read path (counts, tags, etc.).
    pub fn undo(&mut self) -> bool {
        match self.undo.pop() {
            Some(entry) => {
                self.decisions.insert(entry.stem, entry.previous);
                true
            }
            None => false,
        }
    }

    /// Push a previous decision onto the bounded undo stack, dropping the oldest
    /// entry once `UNDO_LIMIT` is exceeded.
    fn record_undo(&mut self, stem: String, previous: Decision) {
        self.undo.push(UndoEntry { stem, previous });
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
    }
}
```

- [ ] **Step 4: Run test to verify it passes**
Run: `cargo test -p culler-core`
Expected: PASS.

- [ ] **Step 5: Commit**
```bash
git add culler-core/src/model.rs
git commit -m "feat(model): add Session tier/tag transitions with bounded undo" -m "Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 7: `Session` counts, navigation, and tag aggregation

**Files:**
- Modify: `culler-core/src/model.rs`
- Test: `culler-core/src/model.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `Session`, `TierCounts`, `Tier` (Tasks 2, 5); `fixture_session` test helper (Task 6)
- Produces:
  - `fn counts(&self) -> TierCounts` — every shot buckets by its decision; undecided → `rest`
  - `fn visited_count(&self) -> usize`
  - `fn next_unvisited(&self, from: usize) -> Option<usize>` — first unvisited index `i >= from` (inclusive), or `None`
  - `fn all_tags(&self) -> Vec<String>` — sorted, unique (for autocomplete)

- [ ] **Step 1: Write the failing test**

Add inside `model.rs`'s `#[cfg(test)] mod tests` block (reuses the `fixture_session` helper from Task 6):
```rust
    #[test]
    fn counts_buckets_every_shot_undecided_as_rest() {
        let mut session = fixture_session(&["A", "B", "C", "D", "E"]);
        session.set_tier(0, Some(Tier::Keep));
        session.set_tier(1, Some(Tier::Pick));
        session.set_tier(2, Some(Tier::Best));
        session.set_tier(3, Some(Tier::Reject));
        // index 4 ("E") left undecided → rest
        assert_eq!(
            session.counts(),
            TierCounts {
                rejected: 1,
                rest: 1,
                keep: 1,
                picks: 1,
                bests: 1
            }
        );
    }

    #[test]
    fn counts_treats_cleared_tier_as_rest() {
        let mut session = fixture_session(&["A", "B"]);
        session.set_tier(0, Some(Tier::Keep));
        session.set_tier(0, None); // explicitly cleared back to Rest
        assert_eq!(
            session.counts(),
            TierCounts {
                rejected: 0,
                rest: 2,
                keep: 0,
                picks: 0,
                bests: 0
            }
        );
    }

    #[test]
    fn visited_count_counts_only_visited() {
        let mut session = fixture_session(&["A", "B", "C"]);
        session.mark_visited(0);
        session.mark_visited(2);
        assert_eq!(session.visited_count(), 2);
    }

    #[test]
    fn next_unvisited_finds_first_from_index_inclusive() {
        let mut session = fixture_session(&["A", "B", "C"]);
        session.mark_visited(0);
        assert_eq!(session.next_unvisited(0), Some(1));
        assert_eq!(session.next_unvisited(1), Some(1)); // inclusive of `from`
        session.mark_visited(1);
        session.mark_visited(2);
        assert_eq!(session.next_unvisited(0), None);
    }

    #[test]
    fn next_unvisited_past_end_is_none() {
        let session = fixture_session(&["A", "B"]);
        assert_eq!(session.next_unvisited(0), Some(0));
        assert_eq!(session.next_unvisited(5), None);
    }

    #[test]
    fn all_tags_are_sorted_and_unique() {
        let mut session = fixture_session(&["A", "B"]);
        session.set_tags(0, vec!["sky".to_string(), "tree".to_string()]);
        session.set_tags(1, vec!["tree".to_string(), "beach".to_string()]);
        assert_eq!(
            session.all_tags(),
            vec!["beach".to_string(), "sky".to_string(), "tree".to_string()]
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**
Run: `cargo test -p culler-core counts_`
Expected: FAIL — compile error `error[E0599]: no method named \`counts\` found for struct \`Session\``.

- [ ] **Step 3: Write minimal implementation**

Insert a NEW `impl Session { ... }` block ABOVE the `#[cfg(test)]` module in `model.rs`:
```rust
impl Session {
    /// Per-bucket counts over ALL shots. A shot with no decision (or a cleared
    /// tier) counts as `rest` — the destination it would land in on Apply.
    pub fn counts(&self) -> TierCounts {
        let mut c = TierCounts::default();
        for shot in &self.shots {
            let tier = self.decisions.get(&shot.stem).and_then(|d| d.tier);
            match tier {
                Some(Tier::Reject) => c.rejected += 1,
                Some(Tier::Keep) => c.keep += 1,
                Some(Tier::Pick) => c.picks += 1,
                Some(Tier::Best) => c.bests += 1,
                None => c.rest += 1,
            }
        }
        c
    }

    /// How many shots have been seen in the loupe (real completion progress).
    pub fn visited_count(&self) -> usize {
        self.shots
            .iter()
            .filter(|shot| {
                self.decisions
                    .get(&shot.stem)
                    .map(|d| d.visited)
                    .unwrap_or(false)
            })
            .count()
    }

    /// First unvisited shot at index `>= from` (inclusive), or `None`.
    pub fn next_unvisited(&self, from: usize) -> Option<usize> {
        (from..self.shots.len()).find(|&i| {
            !self
                .decisions
                .get(&self.shots[i].stem)
                .map(|d| d.visited)
                .unwrap_or(false)
        })
    }

    /// All tags used across the session, sorted and de-duplicated (autocomplete).
    pub fn all_tags(&self) -> Vec<String> {
        let mut set = std::collections::BTreeSet::new();
        for decision in self.decisions.values() {
            for tag in &decision.tags {
                set.insert(tag.clone());
            }
        }
        set.into_iter().collect()
    }
}
```

- [ ] **Step 4: Run test to verify it passes**
Run: `cargo test -p culler-core`
Expected: PASS — the full `model` suite is green.

- [ ] **Step 5: Commit**
```bash
git add culler-core/src/model.rs
git commit -m "feat(model): add counts, navigation and tag aggregation" -m "Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 8: `persist` atomic save + load round-trip

**Files:**
- Create: `culler-core/src/persist.rs`
- Modify: `culler-core/src/lib.rs` (add `pub mod persist;`)
- Test: `culler-core/src/persist.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `Session`, `Shot`, `Decision`, `Tier`, `CaptureTime`, `SESSION_FILE` (model, Tasks 2–5)
- Produces:
  - `pub enum PersistError { Io(std::io::Error), Corrupt(serde_json::Error) }` (impls `Display` + `std::error::Error`)
  - `pub fn save(session: &Session, path: &Path) -> Result<(), PersistError>` — atomic write-temp-then-rename
  - `pub fn load(path: &Path) -> Result<Session, PersistError>`
  - **Private helpers (new, phase-1-local):** `fn tmp_path(path: &Path) -> PathBuf` (appends `.tmp`); **test-only** `fn unique_temp_dir(tag: &str) -> PathBuf`. Neither is in the README.

- [ ] **Step 1: Write the failing test**

Add `pub mod persist;` to `culler-core/src/lib.rs` (below `pub mod model;`), then create `culler-core/src/persist.rs` containing ONLY the tests module (implementation comes in Step 3):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CaptureTime, Decision, Session, Shot, Tier, SESSION_FILE};
    use std::path::PathBuf;

    /// A fresh, unique temp directory for a single test.
    fn unique_temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "fastcull-{tag}-{}-{nanos}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = unique_temp_dir("roundtrip");
        let path = dir.join(SESSION_FILE);

        let mut session = Session {
            source_dir: dir.clone(),
            shots: vec![Shot {
                stem: "IMG_0001".to_string(),
                jpeg: dir.join("IMG_0001.JPG"),
                raw: Some(dir.join("IMG_0001.CR3")),
                sidecar: None,
                capture: CaptureTime {
                    datetime: Some("2026:07:08 10:00:00".to_string()),
                    subsec: Some(1),
                },
            }],
            decisions: std::collections::HashMap::new(),
            current: 0,
            undo: Vec::new(),
        };
        session.decisions.insert(
            "IMG_0001".to_string(),
            Decision {
                tier: Some(Tier::Pick),
                tags: vec!["portrait".to_string()],
                visited: true,
            },
        );

        save(&session, &path).unwrap();
        assert!(path.exists());
        // Atomic write leaves no dangling temp file behind.
        assert!(!dir.join(".fastcull.json.tmp").exists());

        let loaded = load(&path).unwrap();
        assert_eq!(loaded.source_dir, session.source_dir);
        assert_eq!(loaded.shots, session.shots);
        assert_eq!(loaded.decisions, session.decisions);
        assert_eq!(loaded.current, 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_reports_corrupt_json_as_corrupt_error() {
        let dir = unique_temp_dir("loadcorrupt");
        let path = dir.join(SESSION_FILE);
        std::fs::write(&path, b"{ not valid json ").unwrap();

        match load(&path) {
            Err(PersistError::Corrupt(_)) => {}
            other => panic!("expected Corrupt, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_missing_file_is_an_io_error() {
        let dir = unique_temp_dir("loadmissing");
        let path = dir.join(SESSION_FILE);
        match load(&path) {
            Err(PersistError::Io(e)) => {
                assert_eq!(e.kind(), std::io::ErrorKind::NotFound);
            }
            other => panic!("expected Io(NotFound), got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
```

- [ ] **Step 2: Run test to verify it fails**
Run: `cargo test -p culler-core --lib persist`
Expected: FAIL — compile error `error[E0425]: cannot find function \`save\` in this scope` (and `load`, `PersistError`).

- [ ] **Step 3: Write minimal implementation**

Insert ABOVE the `#[cfg(test)]` module in `culler-core/src/persist.rs`:
```rust
//! Session persistence: atomic JSON sidecar read/write for `.fastcull.json`.

use crate::model::Session;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum PersistError {
    Io(std::io::Error),
    Corrupt(serde_json::Error),
}

impl std::fmt::Display for PersistError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PersistError::Io(e) => write!(f, "session I/O error: {e}"),
            PersistError::Corrupt(e) => write!(f, "corrupt session file: {e}"),
        }
    }
}

impl std::error::Error for PersistError {}

/// Serialize `session` to `path` atomically: write `<path>.tmp` in the same
/// directory, fsync it, then `rename` it over `path` (rename is atomic on the
/// same FS). *(rev: fsync added by post-review fix 9b4f6b6 — without it a crash
/// can commit the rename before the data blocks reach disk, publishing a
/// truncated sidecar.)*
pub fn save(session: &Session, path: &Path) -> Result<(), PersistError> {
    let json = serde_json::to_vec_pretty(session).map_err(PersistError::Corrupt)?;
    let tmp = tmp_path(path);
    let mut file = std::fs::File::create(&tmp).map_err(PersistError::Io)?;
    std::io::Write::write_all(&mut file, &json).map_err(PersistError::Io)?;
    file.sync_all().map_err(PersistError::Io)?;
    drop(file);
    std::fs::rename(&tmp, path).map_err(PersistError::Io)?;
    Ok(())
}

/// Read and deserialize a `Session` from `path`. A missing file surfaces as
/// `PersistError::Io`; malformed JSON as `PersistError::Corrupt`.
pub fn load(path: &Path) -> Result<Session, PersistError> {
    let bytes = std::fs::read(path).map_err(PersistError::Io)?;
    serde_json::from_slice(&bytes).map_err(PersistError::Corrupt)
}

/// `<path>.tmp` in the same directory, so the subsequent rename stays same-FS.
fn tmp_path(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_owned();
    os.push(".tmp");
    PathBuf::from(os)
}
```

- [ ] **Step 4: Run test to verify it passes**
Run: `cargo test -p culler-core --lib persist`
Expected: PASS — all three persist tests green.

- [ ] **Step 5: Commit**
```bash
git add culler-core/src/lib.rs culler-core/src/persist.rs
git commit -m "feat(persist): add atomic save and load" -m "Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 9: `load_or_fresh` + corrupt-session quarantine

**Files:**
- Modify: `culler-core/src/persist.rs`
- Test: `culler-core/src/persist.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `Session` (model), `SESSION_FILE`, `SESSION_BAD_FILE` (model, Task 2); `save`, `load`, `PersistError`, `unique_temp_dir` test helper (Task 8)
- Produces:
  - `pub fn load_or_fresh(source_dir: &Path) -> Result<Option<Session>, PersistError>` — reads `source_dir/SESSION_FILE`; missing → `Ok(None)`; valid → `Ok(Some(session))`; corrupt → renames the file to the `SESSION_BAD_FILE` sibling (never overwriting it) and returns `Ok(None)`.

- [ ] **Step 1: Write the failing test**

First update the model import line at the top of `persist.rs`'s `mod tests` to add `SESSION_BAD_FILE`:
```rust
    use crate::model::{CaptureTime, Decision, Session, Shot, Tier, SESSION_BAD_FILE, SESSION_FILE};
```
Then add these tests inside `persist.rs`'s `#[cfg(test)] mod tests` block:
```rust
    #[test]
    fn load_or_fresh_missing_returns_none() {
        let dir = unique_temp_dir("lofmissing");
        assert!(load_or_fresh(&dir).unwrap().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_or_fresh_valid_returns_session() {
        let dir = unique_temp_dir("lofvalid");
        let path = dir.join(SESSION_FILE);
        let session = Session {
            source_dir: dir.clone(),
            ..Session::default()
        };
        save(&session, &path).unwrap();

        let loaded = load_or_fresh(&dir).unwrap().expect("session present");
        assert_eq!(loaded.source_dir, dir);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_or_fresh_corrupt_renames_to_bad_and_returns_none() {
        let dir = unique_temp_dir("lofcorrupt");
        let path = dir.join(SESSION_FILE);
        std::fs::write(&path, b"{ this is not valid json ").unwrap();

        let result = load_or_fresh(&dir).unwrap();
        assert!(result.is_none());
        // The original is renamed aside, not overwritten or deleted.
        assert!(!path.exists());
        let bad = dir.join(SESSION_BAD_FILE);
        assert!(bad.exists());
        // Evidence is preserved verbatim.
        assert_eq!(
            std::fs::read(&bad).unwrap(),
            b"{ this is not valid json ".to_vec()
        );
        std::fs::remove_dir_all(&dir).ok();
    }
```

- [ ] **Step 2: Run test to verify it fails**
Run: `cargo test -p culler-core --lib load_or_fresh`
Expected: FAIL — compile error `error[E0425]: cannot find function \`load_or_fresh\` in this scope`.

- [ ] **Step 3: Write minimal implementation**

Update the `use` line at the top of `persist.rs` to pull in the two file-name constants:
```rust
use crate::model::{Session, SESSION_BAD_FILE, SESSION_FILE};
```
Then add this function to `persist.rs` (below `load`, above the `#[cfg(test)]` module):
```rust
/// Load the session sidecar from `source_dir/SESSION_FILE`.
/// - Missing file → `Ok(None)` (start a fresh session).
/// - Valid file → `Ok(Some(session))`.
/// - Corrupt file → rename it to the first free `SESSION_BAD_FILE` sibling
///   (`.bad`, then `.bad.1`, `.bad.2`, …) so every corruption's evidence is
///   preserved, and return `Ok(None)`. *(rev: numbered siblings added by
///   post-review fix e1a20b1 — a plain rename onto `.bad` clobbered the
///   evidence of an earlier corruption.)*
/// - Other I/O errors → `Err(PersistError::Io)`.
pub fn load_or_fresh(source_dir: &Path) -> Result<Option<Session>, PersistError> {
    let path = source_dir.join(SESSION_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(PersistError::Io(e)),
    };
    match serde_json::from_slice::<Session>(&bytes) {
        Ok(session) => Ok(Some(session)),
        Err(_) => {
            let bad = quarantine_path(source_dir);
            std::fs::rename(&path, &bad).map_err(PersistError::Io)?;
            Ok(None)
        }
    }
}

/// First quarantine name with no existing file: `SESSION_BAD_FILE`, then
/// numbered `.1`, `.2`, … siblings, so a rename there never destroys evidence
/// from an earlier corruption.
fn quarantine_path(source_dir: &Path) -> PathBuf {
    let bad = source_dir.join(SESSION_BAD_FILE);
    if !bad.exists() {
        return bad;
    }
    let mut n: u32 = 1;
    loop {
        let candidate = source_dir.join(format!("{}.{}", SESSION_BAD_FILE, n));
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}
```

- [ ] **Step 4: Run test to verify it passes**
Run: `cargo test -p culler-core --lib`
Expected: PASS — the whole `culler-core` unit suite (model + persist) is green.

- [ ] **Step 5: Commit**
```bash
git add culler-core/src/persist.rs
git commit -m "feat(persist): add load_or_fresh with corrupt-session quarantine" -m "Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Phase 1 done — verification

Run the full workspace suite and build to confirm the phase deliverable (a fully unit-tested pure state engine + resumable session sidecar):

```bash
cargo test --workspace
cargo build --workspace
```

Expected: all tests green; both crates compile. `culler-core` carries no GUI dependency; `culler` is a compile-only stub. Phases 2–6 consume the exact symbols listed in each task's **Produces** block (canonical signatures in [README.md](README.md)).
