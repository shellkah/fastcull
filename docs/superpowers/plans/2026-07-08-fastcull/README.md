# FastCull — Implementation Plan (Phase Index)

> **For agentic workers:** This directory splits the [FastCull design spec](../../../specs/2026-07-08-fastcull-design.md) into **6 sequential phases**. Each phase is a self-contained plan that produces working, unit-tested software on its own. Implement them **in order** — later phases consume the exact interfaces earlier phases produce.
>
> **REQUIRED SUB-SKILL:** Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement each phase task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build FastCull v1 — a fast, keyboard-driven photo culling tool that sorts a folder of shots into quality tiers and safely reorganizes them into a destination folder structure on Apply, never deleting anything.

**Architecture:** Two Cargo crates in a workspace. `culler-core` is a pure, GUI-free library holding all domain logic (model, scan, decode, persist, xmp, plan, apply) so it is fully unit-testable. `culler` is the Slint binary (pipeline, input, ui, main) that renders the core and is deliberately swappable.

**Tech Stack:** Rust (2024 edition), Slint 1.17 (Skia), turbojpeg 1, fast_image_resize 6, kamadak-exif 0.6, rustix 1, serde/serde_json, quick-xml 0.41. *(BLAKE3 verification belongs to the spec's post-v1 "Phase 2" — an unrelated numbering to these plan phases. Do **not** add the `blake3` dep anywhere in v1.)*

---

## Phase map

Build in this order. Phases 1–4 yield a fully headless, data-safe core (you could drive Apply from a test harness before any pixels render). Phase 5 adds decoding. Phase 6 adds the GUI.

| Phase | File | Scope (`culler-core` unless noted) | Deliverable |
|---|---|---|---|
| **1** | [`phase-1-core-and-persistence.md`](phase-1-core-and-persistence.md) | workspace scaffolding, `model`, `persist` | Pure state engine + resumable session sidecar, fully unit-tested. |
| **2** | [`phase-2-scan.md`](phase-2-scan.md) | `scan` | Folder → sorted `Vec<Shot>`: pairing, RAW + both sidecar conventions, stable capture-time sort. |
| **3** | [`phase-3-xmp-and-plan.md`](phase-3-xmp-and-plan.md) | `xmp`, `plan` | XMP sidecar writer + pure `ApplyPlan` computation (powers the preview). |
| **4** | [`phase-4-apply-engine.md`](phase-4-apply-engine.md) | `apply`, `FsOps`, journal | The safe-move engine — same/cross-FS moves, NOREPLACE, journal, crash recovery. Most-tested unit. |
| **5** | [`phase-5-decode.md`](phase-5-decode.md) | `decode` | `(path, target) → DecodedImage`, EXIF-oriented; embedded-thumbnail extraction. |
| **6** | [`phase-6-gui.md`](phase-6-gui.md) | `culler` binary: `pipeline`, `input`, `ui`, `main` | The running app: filmstrip, loupe, HUD, keymap, Apply dialog. |

> **Phase 6 visual design (authoritative):** the GUI's look is fixed by the imported Claude Design project, vendored in **[`docs/design/`](../../../design/)** — design tokens, per-screen anatomy, and HTML→Slint translation notes in [`DESIGN.md`](../../../design/DESIGN.md); pixel references in [`screens/`](../../../design/screens/). `phase-6-gui.md` builds a `theme.slint` token global (Task 1b) from it and verifies the running app against the screenshots (Task 13). These tokens override any placeholder colors sketched in the plan.

Each phase file repeats the plan header, its own goal, and per-task `Interfaces: Consumes / Produces` blocks. Types named below are **canonical** — phases must use these exact names and signatures, never invent parallel ones.

---

## Global constraints (apply to every task in every phase)

Copied verbatim from the spec. Every task's requirements implicitly include this section.

- **Language / edition:** Rust, edition **2024** (workspace `resolver = "3"`). Workspace with two member crates: `culler-core` (lib) and `culler` (bin).
- **`culler-core` has zero GUI dependencies.** No `slint`, no Slint types, in the library. `decode` emits plain `Vec<u8>` RGBA, never `slint::Image`.
- **v1 performs no deletions of user data.** There is no `unlink` of a source shot anywhere in v1 except the *cross-FS copy path removing its own verified source after the destination copy is fsynced and length-verified*. Rejects are **moved** to `00_rejected`, never deleted. There is no delete step, no "moves-before-deletes" ordering.
- **Nothing touches disk until Apply.** All culling decisions live in memory + the autosaved session sidecar. `plan` is pure and performs **no I/O**.
- **Atomic writes everywhere:** session saves and journal writes use write-temp-then-**fsync**-then-rename. File moves use `renameat2(RENAME_NOREPLACE)` (no-clobber) same-FS, and copy→fsync file→verify→rename(NOREPLACE)→**fsync dir**→remove source cross-FS (spec §8 rev 3: the dir fsync comes *after* the publish rename).
- **A destination file appearing between plan and apply must fail loudly** (NOREPLACE returns `EEXIST`); never silently overwrite. **This includes fresh `.xmp` sidecar writes** — `write_sidecar` publishes with NOREPLACE too; on resume an already-present sidecar target is skipped, not clobbered and not an error.
- **Journal lifecycle (spec §8 rev 4):** the session records the in-flight destination (`Session.pending_apply`) before the first move — that breadcrumb is what makes next-launch crash detection work, since the journal lives in *dest* while launches open *source*. On full success the sidecar bucket dirs are fsynced (durability pass), THEN the journal is **removed** and the breadcrumb cleared (the journal is FastCull's own metadata — the no-deletion guarantee protects user photos, not our bookkeeping). **Journal PRESENCE = incomplete run (rev 4):** even an all-`Done` journal still owes its durability pass — resuming it re-runs the fsyncs and retires it (cheap heal, never re-executes moves), after which a fresh apply proceeds; there is no stale-journal hijack because the retired journal is gone. Crash detection keys on journal presence, NEVER on "has a non-`Done` entry" (the rev-3 rule — it would silently drop the owed fsyncs).
- **Decisions are keyed by filename stem** so resume re-attaches them after a rescan. Corrupt session file → renamed to the first free `.fastcull.json.bad` / `.bad.1` / `.bad.2` … sibling (evidence never clobbered), reported, fresh session started.
- **Platform:** Linux only. `rustix`/`renameat2`/`statvfs` are fine to use directly; no cross-platform abstraction needed.
- **TDD, DRY, YAGNI, frequent commits.** Every task: failing test → run-it-fails → minimal impl → run-it-passes → commit. Conventional-commit messages (`feat:`, `test:`, `refactor:`).
- **No v1 config file.** All configurable names/behaviors are CLI flags (bucket names, `--no-auto-advance`, destination).

---

## Canonical constants

```rust
// culler-core/src/model.rs — bucket layout (defaults; binary may override names via CLI)
pub const BUCKET_REJECTED: &str = "00_rejected";
pub const BUCKET_REST:     &str = "01_rest";
pub const BUCKET_KEEP:     &str = "02_keep";
pub const BUCKET_PICKS:    &str = "03_picks";
pub const BUCKET_BESTS:    &str = "04_bests";

pub const SESSION_FILE:      &str = ".fastcull.json";       // in source dir
pub const SESSION_BAD_FILE:  &str = ".fastcull.json.bad";   // first quarantine name; later corruptions get .bad.1, .bad.2, …
pub const JOURNAL_FILE:      &str = ".fastcull-apply.json"; // in dest dir; removed on successful apply

// RAW extensions recognized as siblings (compared case-insensitively, no leading dot)
pub const RAW_EXTS: &[&str] = &["cr3", "cr2", "nef", "arw", "raf", "rw2", "orf", "dng", "pef", "srw"];
pub const JPEG_EXTS: &[&str] = &["jpg", "jpeg"];
```

## Canonical types (`culler-core`)

These are the load-bearing signatures every phase shares. Field/method names are fixed.

```rust
// ---- model ----
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Tier { Reject, Keep, Pick, Best }

impl Tier {
    /// Ordering on the quality ladder Reject < Rest(None) < Keep < Pick < Best.
    pub fn rank(self) -> i8;             // Reject=-1, Keep=1, Pick=2, Best=3  (Rest/None = 0, handled at call site)
    pub fn bucket(self) -> &'static str; // maps to BUCKET_* constant
    pub fn xmp_rating(self) -> i32;      // Reject=-1, Keep=3, Pick=4, Best=5
}

#[derive(Clone, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Decision {
    pub tier: Option<Tier>,   // None = undecided/Rest → 01_rest on Apply
    pub tags: Vec<String>,
    pub visited: bool,
}
impl Decision {
    pub fn bucket(&self) -> &'static str;   // tier.map(Tier::bucket).unwrap_or(BUCKET_REST)
    pub fn xmp_rating(&self) -> Option<i32>; // tier.map(Tier::xmp_rating)
    pub fn is_undecided(&self) -> bool;      // tier.is_none()
}

/// A capture instant, string-comparable straight from EXIF. Pure model type (no exif dep).
#[derive(Clone, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct CaptureTime {
    pub datetime: Option<String>, // "YYYY:MM:DD HH:MM:SS" exactly as EXIF stores it (lexically sortable)
    pub subsec: Option<u32>,      // SubSecTimeOriginal: first 3 digits RIGHT-PADDED to milliseconds
                                  // ("5"→500, "05"→50, "123456"→123) — EXIF subsec is a decimal
                                  // FRACTION, so integer-parsing raw digits would misorder mixed widths
}

/// One shot = all files sharing a filename stem. Produced by `scan`.
#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct Shot {
    pub stem: String,               // the shot key, e.g. "IMG_1234" (as it appears on disk, case preserved)
    pub jpeg: std::path::PathBuf,   // display file, required in v1
    pub raw: Option<std::path::PathBuf>,
    pub sidecar: Option<std::path::PathBuf>, // pre-existing xmp (either convention), carried untouched
    pub capture: CaptureTime,
}
impl Shot {
    /// All on-disk files belonging to this shot, in move order: jpeg, raw?, sidecar?.
    pub fn files(&self) -> Vec<std::path::PathBuf>;
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UndoEntry { pub stem: String, pub previous: Decision }

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Session {
    pub source_dir: std::path::PathBuf,
    pub shots: Vec<Shot>,
    pub decisions: std::collections::HashMap<String, Decision>, // keyed by Shot.stem
    pub current: usize,                                          // index into shots
    /// In-flight Apply destination (crash-detection breadcrumb, spec §6/§8 rev 3).
    /// Set + autosaved just before an apply's first move; cleared on success.
    /// `#[serde(default)]` so pre-rev-3 session files still load.
    /// (Added by Phase 6 Task 12 in a small TDD step against the landed model.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_apply: Option<std::path::PathBuf>,
    #[serde(skip)]
    pub undo: Vec<UndoEntry>,                                    // bounded, most-recent last
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct TierCounts { pub rejected: usize, pub rest: usize, pub keep: usize, pub picks: usize, pub bests: usize }

// Key Session methods (pure state transitions; each mutating tier/tag op pushes onto `undo`):
//   fn decision(&self, index: usize) -> &Decision                 (returns a default-Decision view if absent)
//   fn set_tier(&mut self, index: usize, tier: Option<Tier>)      // records undo; does NOT auto-advance (caller/input does)
//   fn add_tag(&mut self, index: usize, tag: String)              // records undo; dedupes
//   fn set_tags(&mut self, index: usize, tags: Vec<String>)       // records undo
//   fn mark_visited(&mut self, index: usize)                      // idempotent; no undo entry
//   fn undo(&mut self) -> bool                                    // pops last UndoEntry, restores; false if empty
//   fn counts(&self) -> TierCounts
//   fn visited_count(&self) -> usize
//   fn next_unvisited(&self, from: usize) -> Option<usize>
//   fn all_tags(&self) -> Vec<String>                             // sorted unique, for autocomplete
// UNDO_LIMIT: usize = 200 (bounded stack)

// ---- persist ----  (LANDED on master; these comments match the shipped code)
#[derive(Debug)]
pub enum PersistError { Io(std::io::Error), Corrupt(serde_json::Error) }
pub fn save(session: &Session, path: &std::path::Path) -> Result<(), PersistError>; // atomic temp + fsync + rename
pub fn load(path: &std::path::Path) -> Result<Session, PersistError>;
/// Loads if present & valid; on Corrupt renames path to the FIRST FREE quarantine
/// sibling (.bad, .bad.1, .bad.2, …) so earlier evidence is never clobbered, and returns Ok(None).
pub fn load_or_fresh(source_dir: &std::path::Path) -> Result<Option<Session>, PersistError>;

// ---- scan ----
#[derive(Debug)]
pub enum ScanError { Io(std::io::Error), NotADir(std::path::PathBuf) }
/// Flat (non-recursive) walk of `dir`; groups by stem, pairs RAW + both sidecar conventions,
/// reads EXIF capture time, returns shots sorted by (capture.datetime, capture.subsec, jpeg filename).
/// Shots with no EXIF datetime sort AFTER dated shots, then by filename.
/// v1 requires a JPEG per shot; a stem with only a RAW (no JPEG) is NOT a cullable shot in v1
/// (RAW-only via embedded preview is phase 2 — see Phase 2's assumption note).
pub fn scan(dir: &std::path::Path) -> Result<Vec<Shot>, ScanError>;
/// Same walk, but also returns the RAW-only stems' paths (second element) so they are never
/// silently dropped. `scan` delegates to this and discards the report. Note: the binary counts
/// generic "stays behind" leftovers itself via a readdir diff (Phase 6 `gather_apply_inputs`);
/// this list exists so the preview can additionally name the RAW-only files as a distinct line.
pub fn scan_report(dir: &std::path::Path)
    -> Result<(Vec<Shot>, Vec<std::path::PathBuf>), ScanError>;

// ---- xmp ----
/// Build an XMP sidecar document string: `dc:subject` bag from tags, `xmp:Rating` from rating.
pub fn build_xmp(tags: &[String], rating: Option<i32>) -> String;
/// Write build_xmp(..) to `path` atomically AND no-clobber: temp file → fsync →
/// rename with RENAME_NOREPLACE. An existing `path` yields ErrorKind::AlreadyExists
/// (never silently overwritten — same guarantee as file moves; spec §8 rev 3).
/// Caller decides the path (`stem.xmp`, Adobe style).
pub fn write_sidecar(path: &std::path::Path, tags: &[String], rating: Option<i32>) -> std::io::Result<()>;

// ---- plan (PURE, no I/O) ----
#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct FileMove { pub from: std::path::PathBuf, pub to: std::path::PathBuf }

/// A fresh sidecar to be WRITTEN during apply (not moved). Carries what
/// `xmp::write_sidecar` needs — a to-be-written file has no meaningful `from`.
/// (Refinement introduced in Phase 3; consumed by Phase 4 apply.)
#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct SidecarWrite {
    pub path: std::path::PathBuf,   // target .xmp path in the destination bucket
    pub tags: Vec<String>,
    pub rating: Option<i32>,
}

#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct ShotOp {
    pub stem: String,
    pub bucket: String,                       // one of BUCKET_* (resolved name)
    pub moves: Vec<FileMove>,                 // jpeg, raw?, sidecar? — into `bucket`, suffix-consistent
    pub write_sidecar: Option<SidecarWrite>,  // Some if a NEW sidecar must be written; None if pre-existing carried or no tier/tags
    pub suffix: Option<u32>,                  // collision auto-suffix applied to the whole stem, if any
}

#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct ApplyPlan {
    pub dest: std::path::PathBuf,
    pub buckets: [String; 5],              // resolved bucket names, index order [rejected,rest,keep,picks,bests]
    pub ops: Vec<ShotOp>,
    pub per_bucket_counts: TierCountsPlan,  // counts per destination bucket for the preview
    pub skipped_sidecar_writes: Vec<String>,// stems whose new tags were skipped (pre-existing sidecar) — reported
    pub stale: Vec<String>,                // stems in session no longer on disk (re-verified by binary before plan)
    pub total_bytes: u64,                  // sum of file sizes to move (for free-space preflight)
}
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct TierCountsPlan { pub rejected: usize, pub rest: usize, pub keep: usize, pub picks: usize, pub bests: usize }

/// Pure. `existing` = set of BUCKET-RELATIVE paths ("02_keep/IMG_1234.JPG") already present in the
/// destination (gathered by the binary via readdir, one prefix per bucket). Collisions are per
/// target directory — a name occupied in a *different* bucket must NOT force a suffix (rev 3;
/// a flat name-set caused spurious renames). `sizes` = map stem→total bytes (gathered by binary;
/// plan stays I/O-free). Bucket names come from `buckets`.
pub fn plan(
    session: &Session,
    dest: &std::path::Path,
    buckets: &[String; 5],
    existing: &std::collections::BTreeSet<String>,
    sizes: &std::collections::HashMap<String, u64>,
) -> ApplyPlan;

// ---- apply (the dangerous unit) ----
pub trait FsOps {
    fn mkdir_p(&self, path: &std::path::Path) -> std::io::Result<()>;
    fn same_filesystem(&self, a: &std::path::Path, b: &std::path::Path) -> std::io::Result<bool>;
    fn rename_noreplace(&self, from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()>; // EEXIST if `to` exists
    fn copy_create_new(&self, from: &std::path::Path, to: &std::path::Path) -> std::io::Result<u64>; // O_EXCL; returns bytes
    fn fsync_file(&self, path: &std::path::Path) -> std::io::Result<()>;
    fn fsync_dir(&self, path: &std::path::Path) -> std::io::Result<()>;
    fn remove_file(&self, path: &std::path::Path) -> std::io::Result<()>;
    fn file_len(&self, path: &std::path::Path) -> std::io::Result<u64>;
    fn free_space(&self, path: &std::path::Path) -> std::io::Result<u64>; // statvfs f_bavail * f_frsize
}
pub struct RealFs; // impl FsOps via rustix

#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum OpState { Pending, Published, Done, Failed }
// Published (rev 4): cross-FS only — the publish rename succeeded, the source unlink is still owed.
// Resume completes the unlink (dest is definitively ours); same-FS moves never enter this state.
#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct Journal { pub plan: ApplyPlan, pub statuses: Vec<OpState> } // statuses parallel to the flattened
// file-move list (ops × moves, in order). Sidecar writes are NOT journaled: they are idempotent —
// no-clobber NOREPLACE publish, and an already-present target is skipped on resume.
// A journal whose statuses length disagrees with the plan's flattened move count is structurally
// corrupt: apply/resume refuse it gracefully (ApplyError::Preflight), never index into it (rev 4).

#[derive(Debug)]
pub enum ApplyError { Preflight(String), Fs { path: std::path::PathBuf, source: std::io::Error }, Collision(std::path::PathBuf) }

pub struct ApplyReport { pub moved_shots: usize, pub moved_files: usize, pub sidecars_written: usize, pub stopped_at: Option<String> }

/// Journals first, then executes each ShotOp group atomically. Resumable from an existing journal.
/// On FULL SUCCESS every bucket dir that received a sidecar publish is fsynced, THEN the journal
/// file is removed (FastCull metadata, not user data). `resume` RECONCILES the journal against
/// the disk before executing (Pending + source-gone + dest-exists ⇒ done; Done + dest-missing +
/// source-present ⇒ re-execute; Published ⇒ dest is ours, finish the source unlink — rev 4) so a
/// crash between a move and its journal update never strands the run. Pending + BOTH source and
/// dest present stays a loud stop whose message names the possible prior-run completed copy
/// (rev 4); a source is never unlinked without a durable Published/Done record. Progress
/// reporting: the journal is rewritten as moves complete, so a UI runs apply/resume on a worker
/// thread and POLLS the journal for done/total — no progress callback in the signature (kept stable).
pub fn apply(plan: &ApplyPlan, fs: &dyn FsOps, journal_path: &std::path::Path) -> Result<ApplyReport, ApplyError>;
pub fn resume(journal_path: &std::path::Path, fs: &dyn FsOps) -> Result<ApplyReport, ApplyError>;

// ---- decode (culler-core, but binary-facing) ----
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TargetSize { Fit(u32, u32), Full, Scaled(u8) } // Scaled(n): 1/n via turbojpeg (n in {1,2,4,8})
pub struct DecodedImage { pub w: u32, pub h: u32, pub rgba: Vec<u8> } // premultiplied? NO — straight RGBA8
#[derive(Debug)]
pub enum DecodeError { Io(std::io::Error), Decode(String), Unsupported }
/// Decodes `path`'s JPEG at/around target, applies EXIF orientation, returns straight RGBA8.
pub fn decode(path: &std::path::Path, target: TargetSize) -> Result<DecodedImage, DecodeError>;
/// Extracts the embedded EXIF thumbnail (fast filmstrip first paint); None if absent.
pub fn embedded_thumbnail(path: &std::path::Path) -> Option<DecodedImage>;
```

## Canonical keymap (Phase 6)

| Key | Action | Auto-advance |
|---|---|---|
| `←/→`, `space`/`backspace` | prev / next | — |
| `1` / `2` / `3` | Keep / Pick / Best | yes (default on; `--no-auto-advance` off) |
| `X` | Reject | yes |
| `` ` `` / `0` | clear → undecided (Rest) | no |
| `U` | undo last tier/tag change | — |
| `T` | tag entry (autocomplete; comma-separates) | — |
| `Z` | toggle 1:1 zoom (zoom + pan persist across prev/next) | — |
| `F` | cycle filter: All → ≥Keep → ≥Pick → ≥Best → Rejects | — |
| `Tab` | jump to next unvisited shot | — |
| `A` | open Apply dialog (destination + preview + confirm) | — |
| `Ctrl+S` | force-save session | — |

---

## How to execute this plan

Work one phase file at a time, top to bottom. Do not start Phase N+1 until Phase N's tests are green and committed. Within a phase, each task is `- [ ]` checkboxed with a strict TDD micro-cycle. When a phase file says "Consumes" a type, that type's canonical signature is in this README and was delivered by an earlier phase.
