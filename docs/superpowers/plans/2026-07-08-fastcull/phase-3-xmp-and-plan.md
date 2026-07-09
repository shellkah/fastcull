# FastCull Phase 3 — XMP Writer & Apply Plan — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans. Steps use `- [ ]`. Canonical types in [README.md](README.md); use verbatim. **Depends on Phases 1–2.**

**Goal:** Deliver `culler-core`'s `xmp` module (a hand-written XMP sidecar writer) and its `plan` module (a pure, I/O-free `ApplyPlan` computation) so the binary can render the Apply preview and, later, execute a safe move.

**Architecture:** `xmp` builds a `dc:subject` + `xmp:Rating` XMP document with quick-xml and writes it atomically (temp + rename) — the only disk touch in this phase. `plan` is **pure: it performs no filesystem I/O whatsoever** — it consumes an in-memory `Session` plus caller-gathered facts (`existing` names, `sizes`) and returns an `ApplyPlan` (per-shot moves, collision suffixes, sidecar-write intents, per-bucket counts, total bytes). That `ApplyPlan` powers the preview and is later executed by Phase 4's apply engine; nothing here moves a user file.

**Tech Stack:** Rust 2024, quick-xml 0.41, rustix 1 (`fs` feature — the no-clobber sidecar publish; Phase 4 reuses the dep).

## Global Constraints

Copied from [README.md](README.md); every task inherits these.

- **`plan` is pure and performs no I/O.** All culling decisions live in memory + the autosaved session sidecar; nothing touches disk until Apply. `plan` gathers no facts itself — `existing` (destination file names) and `sizes` (stem → bytes) are handed in by the binary.
- **`culler-core` has zero GUI dependencies.** No `slint` types in the library.
- **v1 performs no deletions of user data.** Rejects are *moved* to `00_rejected`. `plan` never emits an unlink; there is no delete step.
- **Atomic writes everywhere, and NO destination write may clobber (spec §8 rev 3):** `write_sidecar` writes to a temp file, fsyncs, then publishes with `renameat2(RENAME_NOREPLACE)` — an existing file at the target is `ErrorKind::AlreadyExists`, never silently overwritten. (Session saves and journal writes follow the same temp+fsync+rename discipline in other phases; a plain clobbering `rename` was this plan's one unguarded destination write in rev 2.)
- **Pre-existing sidecars are carried untouched, and the skipped tag-write is reported.** When a shot already has a sidecar, `plan` puts that sidecar in `moves` unmodified, writes **no** new one, and records the stem in `ApplyPlan.skipped_sidecar_writes`. Merging tags into an existing XMP is Phase 2 — overwriting someone's edit history is data loss through the front door.
- **Decisions are keyed by filename stem.** `plan` resolves each shot's decision via the session's stem-keyed map.
- **Platform:** Linux only.
- **TDD, DRY, YAGNI, frequent commits.** Every task: failing test → run-it-fails → minimal impl → run-it-passes → commit. Conventional-commit messages (`feat:`, `test:`, `refactor:`).
- **No v1 config file.** Bucket names are passed in as the `buckets: &[String; 5]` argument (CLI-overridable), never read from a config file.

> **⚠️ TYPE REFINEMENT (read before Phase 4 / reconcile with README):** The README declares `ShotOp.write_sidecar: Option<FileMove>`. That type is awkward — a to-be-written sidecar has no meaningful `from` path, and apply needs the tags + rating to regenerate the document. **This phase refines it to `Option<SidecarWrite>`**, and introduces a new public type:
> ```rust
> pub struct SidecarWrite { pub path: std::path::PathBuf, pub tags: Vec<String>, pub rating: Option<i32> }
> ```
> `ShotOp.write_sidecar` therefore becomes `Option<SidecarWrite>`. Phase 4's apply engine calls `xmp::write_sidecar(&sw.path, &sw.tags, sw.rating)` for each `Some`. The orchestrator MUST update the README canonical `ShotOp` and `SidecarWrite` to match. Everything else in the README `ShotOp`/`ApplyPlan`/`FileMove`/`TierCountsPlan` shapes is used verbatim.

> **Note on `stale`:** `plan` cannot detect missing files (it does no I/O), so `ApplyPlan.stale` is initialized **empty**. The binary pre-verifies existence, removes stale shots from the session before calling `plan`, and populates `stale` post-hoc for the preview. This keeps the README `plan` signature unchanged.

---

### Task 1: `build_xmp` — `dc:subject` keyword bag

**Files:**
- Create `culler-core/src/xmp.rs`
- Modify `culler-core/src/lib.rs` (add `pub mod xmp;`)
- Modify `culler-core/Cargo.toml` (add `quick-xml` dependency)
- Test: inline `#[cfg(test)]` module in `culler-core/src/xmp.rs`

**Interfaces:**
- Consumes: nothing from earlier phases.
- Produces: `pub fn build_xmp(tags: &[String], rating: Option<i32>) -> String` (rating unused this task).

- [ ] **Step 1: Write the failing test**
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_xmp_emits_dc_subject_bag() {
        let xml = build_xmp(&["red".to_string(), "sky".to_string()], None);
        assert!(xml.contains("<dc:subject>"), "xml was: {xml}");
        assert!(xml.contains("<rdf:Bag>"), "xml was: {xml}");
        assert!(xml.contains("<rdf:li>red</rdf:li>"), "xml was: {xml}");
        assert!(xml.contains("<rdf:li>sky</rdf:li>"), "xml was: {xml}");

        // empty tags => no dc:subject block at all
        let empty = build_xmp(&[], None);
        assert!(!empty.contains("dc:subject"), "xml was: {empty}");
    }
}
```

- [ ] **Step 2: Run to verify it fails**
Run: `cargo test -p culler-core build_xmp_emits_dc_subject_bag`
Expected: FAIL — compile error `cannot find function \`build_xmp\` in this scope` (module/function not yet defined).

- [ ] **Step 3: Minimal implementation**
First add the dependencies and wire the module (quick-xml 0.41 API verified by probe build 2026-07-09; rustix is used by Task 3's no-clobber publish and reused by Phase 4):
```bash
cargo add quick-xml@0.41 -p culler-core
cargo add rustix@1 --features fs -p culler-core
```
Add to `culler-core/src/lib.rs`:
```rust
pub mod xmp;
```
Create `culler-core/src/xmp.rs`:
```rust
use std::io::{self, Write};
use std::path::Path;

use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use quick_xml::writer::Writer;

/// Build an XMP sidecar document string: a `dc:subject` `rdf:Bag` of keywords
/// (one `rdf:li` per tag). Wrapped in the conventional `xpacket` envelope so
/// Lightroom / darktable / Bridge import it. `rating` is handled in a later task.
pub fn build_xmp(tags: &[String], _rating: Option<i32>) -> String {
    let mut w = Writer::new_with_indent(Vec::new(), b' ', 1);

    let mut meta = BytesStart::new("x:xmpmeta");
    meta.push_attribute(("xmlns:x", "adobe:ns:meta/"));
    w.write_event(Event::Start(meta)).expect("write xmpmeta");

    let mut rdf = BytesStart::new("rdf:RDF");
    rdf.push_attribute(("xmlns:rdf", "http://www.w3.org/1999/02/22-rdf-syntax-ns#"));
    w.write_event(Event::Start(rdf)).expect("write rdf");

    let mut desc = BytesStart::new("rdf:Description");
    desc.push_attribute(("rdf:about", ""));
    desc.push_attribute(("xmlns:dc", "http://purl.org/dc/elements/1.1/"));
    desc.push_attribute(("xmlns:xmp", "http://ns.adobe.com/xap/1.0/"));
    w.write_event(Event::Start(desc)).expect("write description");

    if !tags.is_empty() {
        w.write_event(Event::Start(BytesStart::new("dc:subject"))).expect("write");
        w.write_event(Event::Start(BytesStart::new("rdf:Bag"))).expect("write");
        for tag in tags {
            w.write_event(Event::Start(BytesStart::new("rdf:li"))).expect("write");
            w.write_event(Event::Text(BytesText::new(tag))).expect("write");
            w.write_event(Event::End(BytesEnd::new("rdf:li"))).expect("write");
        }
        w.write_event(Event::End(BytesEnd::new("rdf:Bag"))).expect("write");
        w.write_event(Event::End(BytesEnd::new("dc:subject"))).expect("write");
    }

    w.write_event(Event::End(BytesEnd::new("rdf:Description"))).expect("write");
    w.write_event(Event::End(BytesEnd::new("rdf:RDF"))).expect("write");
    w.write_event(Event::End(BytesEnd::new("x:xmpmeta"))).expect("write");

    let body = String::from_utf8(w.into_inner()).expect("xmp is valid utf8");
    format!(
        "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n{body}\n<?xpacket end=\"w\"?>\n"
    )
}
```

- [ ] **Step 4: Run to verify pass**
Run: `cargo test -p culler-core build_xmp_emits_dc_subject_bag`
Expected: PASS

- [ ] **Step 5: Commit**
```bash
git add culler-core/src/xmp.rs culler-core/src/lib.rs culler-core/Cargo.toml Cargo.lock
git commit -m "feat(xmp): build_xmp emits dc:subject keyword bag"
```

---

### Task 2: `build_xmp` — `xmp:Rating` + quick-xml round-trip

**Files:**
- Modify `culler-core/src/xmp.rs` (emit `xmp:Rating`; add round-trip test)

**Interfaces:**
- Consumes: `build_xmp(tags, rating)` from Task 1.
- Produces: `build_xmp` now emits `<xmp:Rating>` when `rating` is `Some`.

- [ ] **Step 1: Write the failing test** (add inside the existing `mod tests`)
```rust
#[test]
fn build_xmp_round_trips_rating_and_tags() {
    fn parse_xmp(xml: &str) -> (Vec<String>, Option<i32>) {
        use quick_xml::events::Event;
        use quick_xml::Reader;
        let mut reader = Reader::from_str(xml);
        let mut tags = Vec::new();
        let mut rating = None;
        let mut in_li = false;
        let mut in_rating = false;
        loop {
            match reader.read_event() {
                Ok(Event::Start(e)) => match e.name().as_ref() {
                    b"rdf:li" => in_li = true,
                    b"xmp:Rating" => in_rating = true,
                    _ => {}
                },
                Ok(Event::End(e)) => match e.name().as_ref() {
                    b"rdf:li" => in_li = false,
                    b"xmp:Rating" => in_rating = false,
                    _ => {}
                },
                Ok(Event::Text(t)) => {
                    let txt = t.unescape().expect("unescape").into_owned();
                    if in_li {
                        tags.push(txt);
                    } else if in_rating {
                        rating = txt.trim().parse::<i32>().ok();
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => panic!("parse error: {e}"),
                _ => {}
            }
        }
        (tags, rating)
    }

    let xml = build_xmp(&["red".to_string(), "sky".to_string()], Some(4));
    assert!(xml.contains("<xmp:Rating>4</xmp:Rating>"), "xml was: {xml}");
    let (tags, rating) = parse_xmp(&xml);
    assert_eq!(tags, vec!["red".to_string(), "sky".to_string()]);
    assert_eq!(rating, Some(4));

    // reject rating (-1, the Bridge/darktable convention) survives
    let (_, r) = parse_xmp(&build_xmp(&[], Some(-1)));
    assert_eq!(r, Some(-1));

    // rating None => no xmp:Rating element at all
    let none = build_xmp(&["x".to_string()], None);
    assert!(!none.contains("xmp:Rating"), "xml was: {none}");
    let (t2, r2) = parse_xmp(&none);
    assert_eq!(t2, vec!["x".to_string()]);
    assert_eq!(r2, None);
}
```

- [ ] **Step 2: Run to verify it fails**
Run: `cargo test -p culler-core build_xmp_round_trips_rating_and_tags`
Expected: FAIL — assertion `xml.contains("<xmp:Rating>4</xmp:Rating>")` fails (Task 1's `build_xmp` ignores `rating` and emits no rating element).

- [ ] **Step 3: Minimal implementation** — replace `build_xmp` in `culler-core/src/xmp.rs` with the version below (renames `_rating` → `rating`, emits `xmp:Rating` before `dc:subject`):
```rust
/// Build an XMP sidecar document string: `xmp:Rating` from `rating` (when Some)
/// and a `dc:subject` `rdf:Bag` of keywords (one `rdf:li` per tag). Wrapped in
/// the conventional `xpacket` envelope so Lightroom / darktable / Bridge import it.
pub fn build_xmp(tags: &[String], rating: Option<i32>) -> String {
    let mut w = Writer::new_with_indent(Vec::new(), b' ', 1);

    let mut meta = BytesStart::new("x:xmpmeta");
    meta.push_attribute(("xmlns:x", "adobe:ns:meta/"));
    w.write_event(Event::Start(meta)).expect("write xmpmeta");

    let mut rdf = BytesStart::new("rdf:RDF");
    rdf.push_attribute(("xmlns:rdf", "http://www.w3.org/1999/02/22-rdf-syntax-ns#"));
    w.write_event(Event::Start(rdf)).expect("write rdf");

    let mut desc = BytesStart::new("rdf:Description");
    desc.push_attribute(("rdf:about", ""));
    desc.push_attribute(("xmlns:dc", "http://purl.org/dc/elements/1.1/"));
    desc.push_attribute(("xmlns:xmp", "http://ns.adobe.com/xap/1.0/"));
    w.write_event(Event::Start(desc)).expect("write description");

    if let Some(r) = rating {
        w.write_event(Event::Start(BytesStart::new("xmp:Rating"))).expect("write");
        w.write_event(Event::Text(BytesText::new(&r.to_string()))).expect("write");
        w.write_event(Event::End(BytesEnd::new("xmp:Rating"))).expect("write");
    }

    if !tags.is_empty() {
        w.write_event(Event::Start(BytesStart::new("dc:subject"))).expect("write");
        w.write_event(Event::Start(BytesStart::new("rdf:Bag"))).expect("write");
        for tag in tags {
            w.write_event(Event::Start(BytesStart::new("rdf:li"))).expect("write");
            w.write_event(Event::Text(BytesText::new(tag))).expect("write");
            w.write_event(Event::End(BytesEnd::new("rdf:li"))).expect("write");
        }
        w.write_event(Event::End(BytesEnd::new("rdf:Bag"))).expect("write");
        w.write_event(Event::End(BytesEnd::new("dc:subject"))).expect("write");
    }

    w.write_event(Event::End(BytesEnd::new("rdf:Description"))).expect("write");
    w.write_event(Event::End(BytesEnd::new("rdf:RDF"))).expect("write");
    w.write_event(Event::End(BytesEnd::new("x:xmpmeta"))).expect("write");

    let body = String::from_utf8(w.into_inner()).expect("xmp is valid utf8");
    format!(
        "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n{body}\n<?xpacket end=\"w\"?>\n"
    )
}
```

- [ ] **Step 4: Run to verify pass**
Run: `cargo test -p culler-core build_xmp`
Expected: PASS (both `build_xmp_emits_dc_subject_bag` and `build_xmp_round_trips_rating_and_tags`).

- [ ] **Step 5: Commit**
```bash
git add culler-core/src/xmp.rs
git commit -m "feat(xmp): emit xmp:Rating and round-trip tags+rating"
```

---

### Task 3: `write_sidecar` — atomic temp + rename

**Files:**
- Modify `culler-core/src/xmp.rs` (add `write_sidecar`; add atomic-write test)

**Interfaces:**
- Consumes: `build_xmp(tags, rating)` from Task 2; `rustix::fs::{renameat_with, RenameFlags, CWD}`.
- Produces: `pub fn write_sidecar(path: &std::path::Path, tags: &[String], rating: Option<i32>) -> std::io::Result<()>` — atomic (temp + fsync + rename) **and no-clobber** (`RENAME_NOREPLACE`; an existing target is `ErrorKind::AlreadyExists`). Phase 4's apply relies on the no-clobber error to make resume-time sidecar re-runs skip-idempotent.

- [ ] **Step 1: Write the failing test** (add inside `mod tests`)
```rust
#[test]
fn write_sidecar_writes_atomically_and_parses_back() {
    fn unique_tmp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut p = std::env::temp_dir();
        p.push(format!("fastcull-xmp-{}-{}-{}", tag, std::process::id(), nanos));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    let dir = unique_tmp_dir("sidecar");
    let path = dir.join("IMG_1234.xmp");
    write_sidecar(&path, &["red".to_string()], Some(5)).expect("write_sidecar");

    let content = std::fs::read_to_string(&path).expect("read back");
    assert!(content.contains("<rdf:li>red</rdf:li>"), "content: {content}");
    assert!(content.contains("<xmp:Rating>5</xmp:Rating>"), "content: {content}");

    // atomic write leaves no temp file behind: only the final sidecar remains
    let mut entries: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    assert_eq!(entries, vec!["IMG_1234.xmp".to_string()], "leftover files: {entries:?}");

    // NO-CLOBBER (spec §8 rev 3): a second write onto the same path must fail
    // AlreadyExists, leave the original byte-for-byte intact, and clean its temp.
    let before = std::fs::read(&path).unwrap();
    let err = write_sidecar(&path, &["other".to_string()], Some(1)).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(&path).unwrap(), before, "existing sidecar untouched");
    let count = std::fs::read_dir(&dir).unwrap().count();
    assert_eq!(count, 1, "refused publish leaves no temp litter");

    std::fs::remove_dir_all(&dir).ok();
}
```

- [ ] **Step 2: Run to verify it fails**
Run: `cargo test -p culler-core write_sidecar_writes_atomically_and_parses_back`
Expected: FAIL — compile error `cannot find function \`write_sidecar\` in this scope`.

- [ ] **Step 3: Minimal implementation** — append to `culler-core/src/xmp.rs`:
```rust
/// Write `build_xmp(tags, rating)` to `path` atomically AND no-clobber: content
/// goes to a sibling temp file, is fsynced, then published with
/// `renameat2(RENAME_NOREPLACE)`. An existing file at `path` yields
/// `ErrorKind::AlreadyExists` and is never overwritten — the same guarantee
/// every file move has (spec §8 rev 3); a plain `rename` here was the one
/// destination write that could silently clobber. Caller chooses the path
/// (`<stem>.xmp`, Adobe style).
pub fn write_sidecar(path: &Path, tags: &[String], rating: Option<i32>) -> io::Result<()> {
    let content = build_xmp(tags, rating);
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "sidecar path has no file name"))?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id()
    ));

    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
    }
    use rustix::fs::{renameat_with, RenameFlags, CWD};
    if let Err(e) = renameat_with(CWD, &tmp, CWD, path, RenameFlags::NOREPLACE) {
        let _ = std::fs::remove_file(&tmp); // refused publish leaves no litter
        return Err(io::Error::from(e));
    }
    Ok(())
}
```

- [ ] **Step 4: Run to verify pass**
Run: `cargo test -p culler-core xmp`
Expected: PASS (all three xmp tests).

- [ ] **Step 5: Commit**
```bash
git add culler-core/src/xmp.rs
git commit -m "feat(xmp): write_sidecar atomic temp+rename"
```

---

### Task 4: `plan` — bucket assignment + per-shot moves (no collisions)

**Files:**
- Create `culler-core/src/plan.rs`
- Modify `culler-core/src/lib.rs` (add `pub mod plan;`)
- Test: inline `#[cfg(test)]` module in `culler-core/src/plan.rs`

**Interfaces:**
- Consumes (from Phases 1–2): `Session`, `Shot`, `Decision`, `Tier`, `Shot::files() -> Vec<PathBuf>`, `Session::decision(index) -> &Decision`, `Decision::xmp_rating() -> Option<i32>`, `CaptureTime`, and `BUCKET_*` constants.
- Produces:
  - `pub struct FileMove { pub from: PathBuf, pub to: PathBuf }`
  - `pub struct SidecarWrite { pub path: PathBuf, pub tags: Vec<String>, pub rating: Option<i32> }` **(new type — README refinement, see Global Constraints)**
  - `pub struct ShotOp { pub stem: String, pub bucket: String, pub moves: Vec<FileMove>, pub write_sidecar: Option<SidecarWrite>, pub suffix: Option<u32> }`
  - `pub struct TierCountsPlan { pub rejected: usize, pub rest: usize, pub keep: usize, pub picks: usize, pub bests: usize }`
  - `pub struct ApplyPlan { pub dest, pub buckets: [String;5], pub ops, pub per_bucket_counts, pub skipped_sidecar_writes, pub stale, pub total_bytes }`
  - `pub fn plan(session: &Session, dest: &Path, buckets: &[String; 5], existing: &BTreeSet<String>, sizes: &HashMap<String, u64>) -> ApplyPlan`

- [ ] **Step 1: Write the failing test**
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        CaptureTime, Decision, Session, Shot, Tier, BUCKET_BESTS, BUCKET_KEEP, BUCKET_PICKS,
        BUCKET_REJECTED, BUCKET_REST,
    };
    use std::collections::{BTreeSet, HashMap};
    use std::path::{Path, PathBuf};

    fn default_buckets() -> [String; 5] {
        [
            BUCKET_REJECTED.to_string(),
            BUCKET_REST.to_string(),
            BUCKET_KEEP.to_string(),
            BUCKET_PICKS.to_string(),
            BUCKET_BESTS.to_string(),
        ]
    }

    fn shot(stem: &str, ext: &str, raw: Option<&str>, sidecar: Option<&str>) -> Shot {
        Shot {
            stem: stem.to_string(),
            jpeg: PathBuf::from(format!("/src/{stem}.{ext}")),
            raw: raw.map(|e| PathBuf::from(format!("/src/{stem}.{e}"))),
            sidecar: sidecar.map(PathBuf::from),
            capture: CaptureTime::default(),
        }
    }

    #[test]
    fn plan_assigns_buckets_and_builds_moves() {
        let buckets = default_buckets();
        let shots = vec![
            shot("IMG_0001", "JPG", Some("CR3"), None),
            shot("IMG_0002", "JPG", None, None),
        ];
        let mut decisions = HashMap::new();
        decisions.insert(
            "IMG_0001".to_string(),
            Decision { tier: Some(Tier::Best), tags: vec![], visited: true },
        );
        // IMG_0002 has no decision entry => undecided => 01_rest
        let session = Session { shots, decisions, ..Default::default() };

        let p = plan(&session, Path::new("/dest"), &buckets, &BTreeSet::new(), &HashMap::new());

        assert_eq!(p.ops.len(), 2);
        assert_eq!(p.dest, PathBuf::from("/dest"));
        assert_eq!(p.buckets, buckets);

        let op0 = &p.ops[0];
        assert_eq!(op0.stem, "IMG_0001");
        assert_eq!(op0.bucket, "04_bests");
        assert_eq!(op0.suffix, None);
        assert_eq!(
            op0.moves,
            vec![
                FileMove {
                    from: PathBuf::from("/src/IMG_0001.JPG"),
                    to: PathBuf::from("/dest/04_bests/IMG_0001.JPG"),
                },
                FileMove {
                    from: PathBuf::from("/src/IMG_0001.CR3"),
                    to: PathBuf::from("/dest/04_bests/IMG_0001.CR3"),
                },
            ],
        );

        let op1 = &p.ops[1];
        assert_eq!(op1.stem, "IMG_0002");
        assert_eq!(op1.bucket, "01_rest");
        assert_eq!(
            op1.moves,
            vec![FileMove {
                from: PathBuf::from("/src/IMG_0002.JPG"),
                to: PathBuf::from("/dest/01_rest/IMG_0002.JPG"),
            }],
        );

        assert!(p.stale.is_empty());
    }
}
```

- [ ] **Step 2: Run to verify it fails**
Run: `cargo test -p culler-core plan_assigns_buckets_and_builds_moves`
Expected: FAIL — compile error `cannot find function \`plan\`` / unresolved module `plan`.

- [ ] **Step 3: Minimal implementation**
Add to `culler-core/src/lib.rs`:
```rust
pub mod plan;
```
Create `culler-core/src/plan.rs`:
```rust
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::model::{Session, Tier};

#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct FileMove {
    pub from: PathBuf,
    pub to: PathBuf,
}

/// A fresh XMP sidecar the apply engine must write. Refinement of the README's
/// `ShotOp.write_sidecar: Option<FileMove>` — carries what `xmp::write_sidecar`
/// needs (target path + tags + rating) instead of a meaningless `from`.
#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct SidecarWrite {
    pub path: PathBuf,
    pub tags: Vec<String>,
    pub rating: Option<i32>,
}

#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct ShotOp {
    pub stem: String,
    pub bucket: String,
    pub moves: Vec<FileMove>,
    pub write_sidecar: Option<SidecarWrite>,
    pub suffix: Option<u32>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct TierCountsPlan {
    pub rejected: usize,
    pub rest: usize,
    pub keep: usize,
    pub picks: usize,
    pub bests: usize,
}

#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct ApplyPlan {
    pub dest: PathBuf,
    pub buckets: [String; 5],
    pub ops: Vec<ShotOp>,
    pub per_bucket_counts: TierCountsPlan,
    pub skipped_sidecar_writes: Vec<String>,
    pub stale: Vec<String>,
    pub total_bytes: u64,
}

/// Index into `buckets` (order: [rejected, rest, keep, picks, bests]) for a tier.
/// Undecided/None => rest.
fn bucket_index(tier: Option<Tier>) -> usize {
    match tier {
        None => 1,
        Some(Tier::Reject) => 0,
        Some(Tier::Keep) => 2,
        Some(Tier::Pick) => 3,
        Some(Tier::Best) => 4,
    }
}

/// The filename portion after the shot's stem, e.g. "IMG_1234.JPG" => ".JPG",
/// darktable "IMG_1234.CR3.xmp" => ".CR3.xmp". Files of one shot share the stem.
fn rest_after_stem(file_name: &str, stem: &str) -> String {
    file_name.get(stem.len()..).unwrap_or_default().to_string()
}

/// PURE — no filesystem I/O. `existing` = BUCKET-RELATIVE destination paths
/// ("02_keep/IMG_1234.JPG") already on disk (gathered by the binary via one
/// readdir per bucket). Collisions are PER TARGET DIRECTORY — the same name in
/// a different bucket must not force a suffix (rev 3). `sizes` = stem → total
/// bytes. `buckets` = resolved bucket names, order [rejected, rest, keep, picks, bests].
pub fn plan(
    session: &Session,
    dest: &Path,
    buckets: &[String; 5],
    _existing: &BTreeSet<String>,
    _sizes: &HashMap<String, u64>,
) -> ApplyPlan {
    let mut ops = Vec::with_capacity(session.shots.len());

    for (i, shot) in session.shots.iter().enumerate() {
        let decision = session.decision(i);
        let idx = bucket_index(decision.tier);
        let bucket = &buckets[idx];
        let dest_dir = dest.join(bucket);

        let files = shot.files();
        let moves: Vec<FileMove> = files
            .iter()
            .map(|from| {
                let name = from.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                let rest = rest_after_stem(name, &shot.stem);
                FileMove {
                    from: from.clone(),
                    to: dest_dir.join(format!("{}{}", shot.stem, rest)),
                }
            })
            .collect();

        ops.push(ShotOp {
            stem: shot.stem.clone(),
            bucket: bucket.clone(),
            moves,
            write_sidecar: None,
            suffix: None,
        });
    }

    ApplyPlan {
        dest: dest.to_path_buf(),
        buckets: buckets.clone(),
        ops,
        per_bucket_counts: TierCountsPlan::default(),
        skipped_sidecar_writes: Vec::new(),
        stale: Vec::new(),
        total_bytes: 0,
    }
}
```

- [ ] **Step 4: Run to verify pass**
Run: `cargo test -p culler-core plan_assigns_buckets_and_builds_moves`
Expected: PASS

- [ ] **Step 5: Commit**
```bash
git add culler-core/src/plan.rs culler-core/src/lib.rs
git commit -m "feat(plan): bucket assignment and per-shot move list"
```

---

### Task 5: `plan` — collision auto-suffix (existing + intra-plan)

**Files:**
- Modify `culler-core/src/plan.rs` (add deterministic whole-stem suffixing; add collision test)

**Interfaces:**
- Consumes: `plan(...)`, `ShotOp`, `FileMove` from Task 4.
- Produces: `plan` now uses `existing` and prior ops (`claimed`) to auto-suffix colliding stems consistently; `ShotOp.suffix` records the applied suffix.

- [ ] **Step 1: Write the failing test** (add inside `mod tests`)
```rust
#[test]
fn plan_auto_suffixes_existing_and_intra_plan() {
    let buckets = default_buckets();
    // Realistic intra-plan collision: IMG_0002 gets suffixed to IMG_0002-1 by
    // `existing`, which then collides with the REAL stem IMG_0002-1. (Two shots
    // sharing one stem is impossible — scan groups by stem and decisions are
    // stem-keyed — so the old duplicate-stem fixture modeled an unreachable state.)
    let shots = vec![
        shot("IMG_0001", "JPG", Some("CR3"), None), // both files collide with `existing`
        shot("IMG_0002", "JPG", None, None),        // suffixed to -1 by `existing`…
        shot("IMG_0002-1", "JPG", None, None),      // …colliding with that claimed name
    ];
    // all undecided => all land in 01_rest
    let session = Session { shots, decisions: HashMap::new(), ..Default::default() };

    // `existing` holds BUCKET-RELATIVE paths (rev 3).
    let mut existing = BTreeSet::new();
    existing.insert("01_rest/IMG_0001.JPG".to_string());
    existing.insert("01_rest/IMG_0001.CR3".to_string());
    existing.insert("01_rest/IMG_0002.JPG".to_string());

    let p = plan(&session, Path::new("/dest"), &buckets, &existing, &HashMap::new());

    // existing collision suffixes the WHOLE stem, keeping jpeg + raw matched
    assert_eq!(p.ops[0].suffix, Some(1));
    assert_eq!(
        p.ops[0].moves,
        vec![
            FileMove {
                from: PathBuf::from("/src/IMG_0001.JPG"),
                to: PathBuf::from("/dest/01_rest/IMG_0001-1.JPG"),
            },
            FileMove {
                from: PathBuf::from("/src/IMG_0001.CR3"),
                to: PathBuf::from("/dest/01_rest/IMG_0001-1.CR3"),
            },
        ],
    );

    // IMG_0002 is taken in 01_rest => suffixed to IMG_0002-1
    assert_eq!(p.ops[1].suffix, Some(1));
    assert_eq!(p.ops[1].moves[0].to, PathBuf::from("/dest/01_rest/IMG_0002-1.JPG"));

    // the real stem IMG_0002-1 collides with the name op[1] claimed => IMG_0002-1-1
    assert_eq!(p.ops[2].suffix, Some(1));
    assert_eq!(p.ops[2].moves[0].to, PathBuf::from("/dest/01_rest/IMG_0002-1-1.JPG"));
}

#[test]
fn plan_ignores_same_name_in_a_different_bucket() {
    let buckets = default_buckets();
    let shots = vec![shot("IMG_0009", "JPG", None, None)]; // undecided -> 01_rest
    let session = Session { shots, decisions: HashMap::new(), ..Default::default() };

    let mut existing = BTreeSet::new();
    existing.insert("02_keep/IMG_0009.JPG".to_string()); // same NAME, different bucket

    let p = plan(&session, Path::new("/dest"), &buckets, &existing, &HashMap::new());
    // rev 3: collisions are per target directory — no spurious rename.
    assert_eq!(p.ops[0].suffix, None);
    assert_eq!(p.ops[0].moves[0].to, PathBuf::from("/dest/01_rest/IMG_0009.JPG"));
}
```

- [ ] **Step 2: Run to verify it fails**
Run: `cargo test -p culler-core plan_auto_suffixes_existing_and_intra_plan`
Expected: FAIL — `assert_eq!(p.ops[0].suffix, Some(1))` gets `None` (Task 4 ignores `existing` and never suffixes).

- [ ] **Step 3: Minimal implementation** — replace `plan` (and add the `suffixed_stem` helper) in `culler-core/src/plan.rs` with:
```rust
/// Apply the whole-stem collision suffix: None => "IMG_1234", Some(1) => "IMG_1234-1".
fn suffixed_stem(stem: &str, suffix: Option<u32>) -> String {
    match suffix {
        None => stem.to_string(),
        Some(n) => format!("{stem}-{n}"),
    }
}

/// PURE — no filesystem I/O. `existing` = BUCKET-RELATIVE destination paths
/// ("02_keep/IMG_1234.JPG") already on disk (gathered by the binary via one
/// readdir per bucket). Collisions are PER TARGET DIRECTORY — the same name in
/// a different bucket must not force a suffix (rev 3). `sizes` = stem → total
/// bytes. `buckets` = resolved bucket names, order [rejected, rest, keep, picks, bests].
pub fn plan(
    session: &Session,
    dest: &Path,
    buckets: &[String; 5],
    existing: &BTreeSet<String>,
    _sizes: &HashMap<String, u64>,
) -> ApplyPlan {
    let mut ops = Vec::with_capacity(session.shots.len());
    // Names this plan has already claimed in the destination (across all ops).
    let mut claimed: BTreeSet<String> = BTreeSet::new();

    for (i, shot) in session.shots.iter().enumerate() {
        let decision = session.decision(i);
        let idx = bucket_index(decision.tier);
        let bucket = &buckets[idx];
        let dest_dir = dest.join(bucket);

        let files = shot.files();
        let rests: Vec<String> = files
            .iter()
            .map(|f| {
                let name = f.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                rest_after_stem(name, &shot.stem)
            })
            .collect();

        // Resolve a whole-stem suffix so no BUCKET-RELATIVE target path
        // ("01_rest/IMG_1234.JPG") collides with the destination (`existing`)
        // or with a path already claimed by this plan. Per-directory keys mean
        // the same name in a DIFFERENT bucket never forces a suffix (rev 3).
        let mut suffix: Option<u32> = None;
        let names = loop {
            let new_stem = suffixed_stem(&shot.stem, suffix);
            let candidate: Vec<String> =
                rests.iter().map(|rest| format!("{bucket}/{new_stem}{rest}")).collect();
            if candidate
                .iter()
                .all(|n| !existing.contains(n) && !claimed.contains(n))
            {
                break candidate;
            }
            suffix = Some(suffix.map_or(1, |s| s + 1));
        };
        for n in &names {
            claimed.insert(n.clone());
        }
        let new_stem = suffixed_stem(&shot.stem, suffix);

        let moves: Vec<FileMove> = files
            .iter()
            .zip(rests.iter())
            .map(|(from, rest)| FileMove {
                from: from.clone(),
                to: dest_dir.join(format!("{new_stem}{rest}")),
            })
            .collect();

        ops.push(ShotOp {
            stem: shot.stem.clone(),
            bucket: bucket.clone(),
            moves,
            write_sidecar: None,
            suffix,
        });
    }

    ApplyPlan {
        dest: dest.to_path_buf(),
        buckets: buckets.clone(),
        ops,
        per_bucket_counts: TierCountsPlan::default(),
        skipped_sidecar_writes: Vec::new(),
        stale: Vec::new(),
        total_bytes: 0,
    }
}
```

- [ ] **Step 4: Run to verify pass**
Run: `cargo test -p culler-core plan_`
Expected: PASS (`plan_assigns_buckets_and_builds_moves`, `plan_auto_suffixes_existing_and_intra_plan`, `plan_ignores_same_name_in_a_different_bucket`).

- [ ] **Step 5: Commit**
```bash
git add culler-core/src/plan.rs
git commit -m "feat(plan): deterministic whole-stem collision auto-suffix"
```

---

### Task 6: `plan` — sidecar writes vs pre-existing carry + skip report

**Files:**
- Modify `culler-core/src/plan.rs` (emit `SidecarWrite`; report skips; include the new `.xmp` in collision checks)

**Interfaces:**
- Consumes: `plan(...)`, `SidecarWrite`, `Decision::xmp_rating()`, `Shot.sidecar`.
- Produces: `plan` now sets `ShotOp.write_sidecar = Some(SidecarWrite{..})` for shots with a tier or tags and **no** pre-existing sidecar; carries pre-existing sidecars in `moves` and records their stems in `ApplyPlan.skipped_sidecar_writes`; the fresh `.xmp` participates in collision resolution.

- [ ] **Step 1: Write the failing test** (add inside `mod tests`)
```rust
#[test]
fn plan_writes_new_sidecar_and_skips_preexisting() {
    let buckets = default_buckets();
    let shots = vec![
        shot("A", "JPG", None, None),                      // Keep tier => write new sidecar (rating 3)
        shot("B", "JPG", None, None),                      // tags only => write new sidecar (rating None)
        shot("C", "JPG", Some("CR3"), Some("/src/C.xmp")), // pre-existing sidecar => skip + carry
        shot("D", "JPG", None, None),                      // no tier, no tags => no sidecar
    ];
    let mut decisions = HashMap::new();
    decisions.insert(
        "A".to_string(),
        Decision { tier: Some(Tier::Keep), tags: vec![], visited: true },
    );
    decisions.insert(
        "B".to_string(),
        Decision { tier: None, tags: vec!["sky".to_string()], visited: true },
    );
    decisions.insert(
        "C".to_string(),
        Decision { tier: Some(Tier::Pick), tags: vec!["hero".to_string()], visited: true },
    );
    // D: no entry
    let session = Session { shots, decisions, ..Default::default() };

    let p = plan(&session, Path::new("/dest"), &buckets, &BTreeSet::new(), &HashMap::new());

    // A: Keep => 02_keep, fresh sidecar with rating 3, no tags
    assert_eq!(
        p.ops[0].write_sidecar,
        Some(SidecarWrite {
            path: PathBuf::from("/dest/02_keep/A.xmp"),
            tags: vec![],
            rating: Some(3),
        })
    );
    // B: tags only => 01_rest, fresh sidecar, rating None
    assert_eq!(
        p.ops[1].write_sidecar,
        Some(SidecarWrite {
            path: PathBuf::from("/dest/01_rest/B.xmp"),
            tags: vec!["sky".to_string()],
            rating: None,
        })
    );
    // C: pre-existing sidecar carried in moves, no new write, reported as skipped
    assert_eq!(p.ops[2].write_sidecar, None);
    assert!(p.ops[2]
        .moves
        .iter()
        .any(|m| m.to == PathBuf::from("/dest/03_picks/C.xmp")));
    assert_eq!(p.skipped_sidecar_writes, vec!["C".to_string()]);
    // D: nothing to write
    assert_eq!(p.ops[3].write_sidecar, None);
}
```

- [ ] **Step 2: Run to verify it fails**
Run: `cargo test -p culler-core plan_writes_new_sidecar_and_skips_preexisting`
Expected: FAIL — `assert_eq!(p.ops[0].write_sidecar, Some(..))` gets `None` (Task 5 always sets `write_sidecar: None`).

- [ ] **Step 3: Minimal implementation** — replace `plan` in `culler-core/src/plan.rs` with (the `bucket_index`, `rest_after_stem`, and `suffixed_stem` helpers are unchanged):
```rust
/// PURE — no filesystem I/O. `existing` = BUCKET-RELATIVE destination paths
/// ("02_keep/IMG_1234.JPG") already on disk (gathered by the binary via one
/// readdir per bucket). Collisions are PER TARGET DIRECTORY — the same name in
/// a different bucket must not force a suffix (rev 3). `sizes` = stem → total
/// bytes. `buckets` = resolved bucket names, order [rejected, rest, keep, picks, bests].
pub fn plan(
    session: &Session,
    dest: &Path,
    buckets: &[String; 5],
    existing: &BTreeSet<String>,
    _sizes: &HashMap<String, u64>,
) -> ApplyPlan {
    let mut ops = Vec::with_capacity(session.shots.len());
    let mut skipped_sidecar_writes = Vec::new();
    let mut claimed: BTreeSet<String> = BTreeSet::new();

    for (i, shot) in session.shots.iter().enumerate() {
        let decision = session.decision(i);
        let idx = bucket_index(decision.tier);
        let bucket = &buckets[idx];
        let dest_dir = dest.join(bucket);

        let files = shot.files();
        let rests: Vec<String> = files
            .iter()
            .map(|f| {
                let name = f.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                rest_after_stem(name, &shot.stem)
            })
            .collect();

        // A fresh sidecar is written only when the shot has a tier or tags AND
        // has no pre-existing sidecar (which we carry untouched instead).
        let has_content = decision.tier.is_some() || !decision.tags.is_empty();
        let write_new_sidecar = shot.sidecar.is_none() && has_content;

        // Resolve a whole-stem suffix so no target name — including the fresh
        // `.xmp` — collides with `existing` or with a name this plan claimed.
        let mut suffix: Option<u32> = None;
        let names = loop {
            let new_stem = suffixed_stem(&shot.stem, suffix);
            let mut candidate: Vec<String> =
                rests.iter().map(|rest| format!("{bucket}/{new_stem}{rest}")).collect();
            if write_new_sidecar {
                candidate.push(format!("{bucket}/{new_stem}.xmp"));
            }
            if candidate
                .iter()
                .all(|n| !existing.contains(n) && !claimed.contains(n))
            {
                break candidate;
            }
            suffix = Some(suffix.map_or(1, |s| s + 1));
        };
        for n in &names {
            claimed.insert(n.clone());
        }
        let new_stem = suffixed_stem(&shot.stem, suffix);

        // Moves: jpeg, raw?, and a pre-existing sidecar? — carried untouched.
        let moves: Vec<FileMove> = files
            .iter()
            .zip(rests.iter())
            .map(|(from, rest)| FileMove {
                from: from.clone(),
                to: dest_dir.join(format!("{new_stem}{rest}")),
            })
            .collect();

        let write_sidecar = if write_new_sidecar {
            Some(SidecarWrite {
                path: dest_dir.join(format!("{new_stem}.xmp")),
                tags: decision.tags.clone(),
                rating: decision.xmp_rating(),
            })
        } else {
            None
        };
        // Report a skipped tag-write only when there was something to write.
        if shot.sidecar.is_some() && has_content {
            skipped_sidecar_writes.push(shot.stem.clone());
        }

        ops.push(ShotOp {
            stem: shot.stem.clone(),
            bucket: bucket.clone(),
            moves,
            write_sidecar,
            suffix,
        });
    }

    ApplyPlan {
        dest: dest.to_path_buf(),
        buckets: buckets.clone(),
        ops,
        per_bucket_counts: TierCountsPlan::default(),
        skipped_sidecar_writes,
        stale: Vec::new(),
        total_bytes: 0,
    }
}
```

- [ ] **Step 4: Run to verify pass**
Run: `cargo test -p culler-core plan_`
Expected: PASS (all three `plan_*` tests).

- [ ] **Step 5: Commit**
```bash
git add culler-core/src/plan.rs
git commit -m "feat(plan): sidecar-write intents and pre-existing-sidecar skip report"
```

---

### Task 7: `plan` — per-bucket counts + total bytes

**Files:**
- Modify `culler-core/src/plan.rs` (tally `per_bucket_counts`; sum `total_bytes`; add counts/bytes test)

**Interfaces:**
- Consumes: `plan(...)`, `TierCountsPlan`, `sizes: &HashMap<String, u64>`.
- Produces: `plan` now fills `ApplyPlan.per_bucket_counts` (shots landing per bucket) and `ApplyPlan.total_bytes` (`sum(sizes[stem])` over moved shots, for the Phase 4 free-space preflight). `ApplyPlan.stale` remains empty (binary-populated).

- [ ] **Step 1: Write the failing test** (add inside `mod tests`)
```rust
#[test]
fn plan_counts_buckets_and_sums_bytes() {
    let buckets = default_buckets();
    let shots = vec![
        shot("R", "JPG", None, None), // Reject => 00_rejected
        shot("K", "JPG", None, None), // Keep    => 02_keep
        shot("P", "JPG", None, None), // Pick    => 03_picks
        shot("B", "JPG", None, None), // Best    => 04_bests
        shot("Z", "JPG", None, None), // undecided => 01_rest
    ];
    let mut decisions = HashMap::new();
    decisions.insert("R".to_string(), Decision { tier: Some(Tier::Reject), ..Default::default() });
    decisions.insert("K".to_string(), Decision { tier: Some(Tier::Keep), ..Default::default() });
    decisions.insert("P".to_string(), Decision { tier: Some(Tier::Pick), ..Default::default() });
    decisions.insert("B".to_string(), Decision { tier: Some(Tier::Best), ..Default::default() });
    // Z: no entry
    let session = Session { shots, decisions, ..Default::default() };

    let mut sizes = HashMap::new();
    sizes.insert("R".to_string(), 10u64);
    sizes.insert("K".to_string(), 20u64);
    sizes.insert("P".to_string(), 30u64);
    sizes.insert("B".to_string(), 40u64);
    sizes.insert("Z".to_string(), 5u64);
    // a stem with no size entry contributes 0 (defensive)

    let p = plan(&session, Path::new("/dest"), &buckets, &BTreeSet::new(), &sizes);

    assert_eq!(
        p.per_bucket_counts,
        TierCountsPlan { rejected: 1, rest: 1, keep: 1, picks: 1, bests: 1 }
    );
    assert_eq!(p.total_bytes, 105);
    assert!(p.stale.is_empty());
}
```

- [ ] **Step 2: Run to verify it fails**
Run: `cargo test -p culler-core plan_counts_buckets_and_sums_bytes`
Expected: FAIL — `assert_eq!(p.per_bucket_counts, ..)` gets all-zero counts and `p.total_bytes` is `0` (Task 6 leaves both defaulted).

- [ ] **Step 3: Minimal implementation** — replace `plan` in `culler-core/src/plan.rs` with the final version (helpers unchanged):
```rust
/// PURE — no filesystem I/O. `existing` = BUCKET-RELATIVE destination paths
/// ("02_keep/IMG_1234.JPG") already on disk (gathered by the binary via one
/// readdir per bucket). Collisions are PER TARGET DIRECTORY — the same name in
/// a different bucket must not force a suffix (rev 3). `sizes` = stem → total
/// bytes. `buckets` = resolved bucket names, order [rejected, rest, keep, picks, bests].
///
/// `stale` is left empty: `plan` does no I/O, so the binary pre-verifies file
/// existence, drops missing shots before calling `plan`, and fills `stale` for
/// the preview post-hoc.
pub fn plan(
    session: &Session,
    dest: &Path,
    buckets: &[String; 5],
    existing: &BTreeSet<String>,
    sizes: &HashMap<String, u64>,
) -> ApplyPlan {
    let mut ops = Vec::with_capacity(session.shots.len());
    let mut counts = TierCountsPlan::default();
    let mut skipped_sidecar_writes = Vec::new();
    let mut claimed: BTreeSet<String> = BTreeSet::new();
    let mut total_bytes: u64 = 0;

    for (i, shot) in session.shots.iter().enumerate() {
        let decision = session.decision(i);
        let idx = bucket_index(decision.tier);
        let bucket = &buckets[idx];
        let dest_dir = dest.join(bucket);

        let files = shot.files();
        let rests: Vec<String> = files
            .iter()
            .map(|f| {
                let name = f.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                rest_after_stem(name, &shot.stem)
            })
            .collect();

        let has_content = decision.tier.is_some() || !decision.tags.is_empty();
        let write_new_sidecar = shot.sidecar.is_none() && has_content;

        let mut suffix: Option<u32> = None;
        let names = loop {
            let new_stem = suffixed_stem(&shot.stem, suffix);
            let mut candidate: Vec<String> =
                rests.iter().map(|rest| format!("{bucket}/{new_stem}{rest}")).collect();
            if write_new_sidecar {
                candidate.push(format!("{bucket}/{new_stem}.xmp"));
            }
            if candidate
                .iter()
                .all(|n| !existing.contains(n) && !claimed.contains(n))
            {
                break candidate;
            }
            suffix = Some(suffix.map_or(1, |s| s + 1));
        };
        for n in &names {
            claimed.insert(n.clone());
        }
        let new_stem = suffixed_stem(&shot.stem, suffix);

        let moves: Vec<FileMove> = files
            .iter()
            .zip(rests.iter())
            .map(|(from, rest)| FileMove {
                from: from.clone(),
                to: dest_dir.join(format!("{new_stem}{rest}")),
            })
            .collect();

        let write_sidecar = if write_new_sidecar {
            Some(SidecarWrite {
                path: dest_dir.join(format!("{new_stem}.xmp")),
                tags: decision.tags.clone(),
                rating: decision.xmp_rating(),
            })
        } else {
            None
        };
        if shot.sidecar.is_some() && has_content {
            skipped_sidecar_writes.push(shot.stem.clone());
        }

        match idx {
            0 => counts.rejected += 1,
            1 => counts.rest += 1,
            2 => counts.keep += 1,
            3 => counts.picks += 1,
            _ => counts.bests += 1,
        }
        total_bytes += sizes.get(&shot.stem).copied().unwrap_or(0);

        ops.push(ShotOp {
            stem: shot.stem.clone(),
            bucket: bucket.clone(),
            moves,
            write_sidecar,
            suffix,
        });
    }

    ApplyPlan {
        dest: dest.to_path_buf(),
        buckets: buckets.clone(),
        ops,
        per_bucket_counts: counts,
        skipped_sidecar_writes,
        stale: Vec::new(),
        total_bytes,
    }
}
```

- [ ] **Step 4: Run to verify pass**
Run: `cargo test -p culler-core`
Expected: PASS (all `xmp` and `plan` tests: 3 + 5 = 8 new tests, plus Phases 1–2).

- [ ] **Step 5: Commit**
```bash
git add culler-core/src/plan.rs
git commit -m "feat(plan): per-bucket counts and total-byte preflight sum"
```

---

## Phase 3 done — definition of done

- `culler-core/src/xmp.rs`: `build_xmp` (dc:subject bag + xmp:Rating, xpacket-wrapped, round-trips) and `write_sidecar` (atomic temp + fsync + **NOREPLACE** rename — no-clobber, `AlreadyExists` on an occupied target), fully unit-tested.
- `culler-core/src/plan.rs`: pure `plan(...)` → `ApplyPlan` — bucket assignment, deterministic whole-stem collision suffixing over **bucket-relative** paths (existing + intra-plan; a name in a different bucket never forces a suffix), fresh-sidecar intents vs pre-existing carry + skip report, per-bucket counts, total bytes; `stale` empty (binary-populated). **No filesystem I/O in `plan`.**
- `culler-core/src/lib.rs`: `pub mod xmp;` and `pub mod plan;`.
- New public types: `FileMove`, `SidecarWrite` (README refinement), `ShotOp` (with `write_sidecar: Option<SidecarWrite>`), `TierCountsPlan`, `ApplyPlan`.
- `cargo test -p culler-core` green; each task committed with a conventional-commit message.
- **Reconcile README:** update canonical `ShotOp.write_sidecar` to `Option<SidecarWrite>` and add the `SidecarWrite` type before Phase 4.
