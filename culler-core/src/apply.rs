//! The safe-move apply engine. Journals the plan before the first move, moves
//! each shot's fileset group-atomically through `FsOps`, and stops loudly on the
//! first failure. No deletion step exists beyond the cross-FS path removing its
//! own verified source (Task 5). Resumable via `resume` (Task 8).

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fsops::FsOps;
use crate::plan::ApplyPlan;

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum OpState {
    Pending,
    Done,
    Failed,
}

/// Serialized alongside the plan in `dest/.fastcull-apply.json`. `statuses` is
/// parallel to the flattened list of file moves (`ops` × `moves`), in order.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Journal {
    pub plan: ApplyPlan,
    pub statuses: Vec<OpState>,
}

#[derive(Debug)]
pub enum ApplyError {
    /// Free-space preflight refused before any move.
    Preflight(String),
    /// A filesystem operation failed on `path`.
    Fs { path: PathBuf, source: io::Error },
    /// A destination file appeared between plan and apply (NOREPLACE `EEXIST`).
    Collision(PathBuf),
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyError::Preflight(m) => write!(f, "preflight failed: {m}"),
            ApplyError::Fs { path, source } => {
                write!(f, "fs error on {}: {source}", path.display())
            }
            ApplyError::Collision(p) => write!(f, "collision: {} already exists", p.display()),
        }
    }
}
impl std::error::Error for ApplyError {}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApplyReport {
    pub moved_shots: usize,
    pub moved_files: usize,
    pub sidecars_written: usize,
    pub stopped_at: Option<String>,
}

/// Total number of file moves across all ops (== journal `statuses` length).
fn total_move_count(plan: &ApplyPlan) -> usize {
    plan.ops.iter().map(|o| o.moves.len()).sum()
}

/// Serialize the journal atomically (temp file → optional fsync → rename). Real
/// I/O — the journal must survive a real crash, so it does NOT go through
/// `FsOps`. `sync` fsyncs the temp before publishing: required at checkpoints
/// (journal-first write, failure stop), optional for incremental progress —
/// `resume`'s reconciliation (Task 8) makes an unsynced tail harmless, so
/// per-move fsync (brutal on multi-thousand-file shoots) is unnecessary.
fn write_journal(journal: &Journal, path: &Path, sync: bool) -> Result<(), ApplyError> {
    let bytes = serde_json::to_vec(journal).map_err(|e| ApplyError::Fs {
        path: path.to_path_buf(),
        source: io::Error::other(e),
    })?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "journal".into());
    let tmp = path.with_file_name(format!("{name}.tmp"));
    let mut f = std::fs::File::create(&tmp).map_err(|e| ApplyError::Fs {
        path: tmp.clone(),
        source: e,
    })?;
    f.write_all(&bytes).map_err(|e| ApplyError::Fs {
        path: tmp.clone(),
        source: e,
    })?;
    if sync {
        f.sync_all().map_err(|e| ApplyError::Fs {
            path: tmp.clone(),
            source: e,
        })?;
    }
    drop(f);
    std::fs::rename(&tmp, path).map_err(|e| ApplyError::Fs {
        path: path.to_path_buf(),
        source: e,
    })
}

/// Move one file same-FS (rename), mapping no-clobber `EEXIST` to `Collision`,
/// falling back to `move_cross_fs` on `EXDEV`.
fn move_one(fs: &dyn FsOps, from: &Path, to: &Path) -> Result<(), ApplyError> {
    match fs.rename_noreplace(from, to) {
        Ok(()) => Ok(()),
        Err(e) if is_exdev(&e) => move_cross_fs(fs, from, to),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            Err(ApplyError::Collision(to.to_path_buf()))
        }
        Err(e) => Err(ApplyError::Fs {
            path: from.to_path_buf(),
            source: e,
        }),
    }
}

fn is_exdev(e: &io::Error) -> bool {
    e.raw_os_error() == Some(rustix::io::Errno::XDEV.raw_os_error())
}

/// Hidden sibling partial path: `dir/.<name>.partial`.
fn partial_path(to: &Path) -> PathBuf {
    let name = to
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    to.with_file_name(format!(".{name}.partial"))
}

/// Cross-filesystem move. Source is never touched until the destination copy is
/// fully copied, fsynced, length-verified, and atomically published.
fn move_cross_fs(fs: &dyn FsOps, from: &Path, to: &Path) -> Result<(), ApplyError> {
    let partial = partial_path(to);
    let _ = fs.remove_file(&partial); // clear a stale partial from a prior crash

    // Copy source → partial (O_EXCL). Any error here leaves the SOURCE untouched.
    let copied = match fs.copy_create_new(from, &partial) {
        Ok(n) => n,
        Err(e) => {
            let _ = fs.remove_file(&partial);
            return Err(ApplyError::Fs {
                path: partial,
                source: e,
            });
        }
    };
    if let Err(e) = fs.fsync_file(&partial) {
        let _ = fs.remove_file(&partial);
        return Err(ApplyError::Fs {
            path: partial,
            source: e,
        });
    }
    // Verify byte length: file_len(dest) == file_len(src) (BLAKE3 is phase-2).
    let dest_len = match fs.file_len(&partial) {
        Ok(n) => n,
        Err(e) => {
            let _ = fs.remove_file(&partial);
            return Err(ApplyError::Fs {
                path: partial,
                source: e,
            });
        }
    };
    let src_len = match fs.file_len(from) {
        Ok(n) => n,
        Err(e) => {
            let _ = fs.remove_file(&partial);
            return Err(ApplyError::Fs {
                path: from.to_path_buf(),
                source: e,
            });
        }
    };
    if dest_len != src_len || copied != src_len {
        let _ = fs.remove_file(&partial);
        return Err(ApplyError::Fs {
            path: partial,
            source: io::Error::new(
                io::ErrorKind::InvalidData,
                format!("short copy: {dest_len} of {src_len} bytes"),
            ),
        });
    }
    // Publish partial → final (no clobber), THEN make the rename durable.
    // ORDER MATTERS (spec §8 rev 3): the source unlink below happens on a
    // DIFFERENT filesystem, so the rename's directory entry must be durable
    // before the source disappears — power loss could otherwise persist the
    // unlink while the rename is lost, leaving the data reachable only as a
    // hidden `.partial`. (rev 2 fsynced the dir BEFORE the rename, which made
    // the `.partial` entry durable instead of the final one.)
    match fs.rename_noreplace(&partial, to) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let _ = fs.remove_file(&partial);
            return Err(ApplyError::Collision(to.to_path_buf()));
        }
        Err(e) => {
            let _ = fs.remove_file(&partial);
            return Err(ApplyError::Fs {
                path: to.to_path_buf(),
                source: e,
            });
        }
    }
    if let Some(dir) = to.parent()
        && let Err(e) = fs.fsync_dir(dir)
    {
        // The final is already published — do NOT touch it or the source if
        // durability can't be proven. Stop loudly; worst case a duplicate
        // (source + dest both present), never a loss.
        return Err(ApplyError::Fs {
            path: dir.to_path_buf(),
            source: e,
        });
    }
    // ONLY NOW remove the verified, durably-published source (the sole unlink in v1).
    fs.remove_file(from).map_err(|e| ApplyError::Fs {
        path: from.to_path_buf(),
        source: e,
    })
}

/// Execute (or resume) a journal: mkdir buckets, move each not-yet-`Done` file,
/// then per shot write a fresh sidecar if requested. Shared by `apply` and `resume`.
fn execute(
    journal: &mut Journal,
    fs: &dyn FsOps,
    journal_path: &Path,
) -> Result<ApplyReport, ApplyError> {
    let dest = journal.plan.dest.clone();
    let buckets = journal.plan.buckets.clone();
    let ops = journal.plan.ops.clone(); // owned copy so we can mutate journal.statuses freely

    // Create the five bucket dirs (idempotent; safe on resume).
    for bucket in &buckets {
        let dir = dest.join(bucket);
        fs.mkdir_p(&dir).map_err(|e| ApplyError::Fs {
            path: dir,
            source: e,
        })?;
    }

    let mut report = ApplyReport::default();
    let mut gidx = 0usize; // global index into journal.statuses

    for op in &ops {
        for mv in &op.moves {
            if journal.statuses[gidx] == OpState::Done {
                gidx += 1;
                continue;
            }
            match move_one(fs, &mv.from, &mv.to) {
                Ok(()) => {
                    journal.statuses[gidx] = OpState::Done;
                    report.moved_files += 1;
                    // Persist progress incrementally; fsync only every 64th move
                    // (and at checkpoints) — reconciliation (Task 8) makes an
                    // unsynced tail harmless, and this doubles as the progress
                    // feed the Phase-6 UI polls from a worker thread.
                    write_journal(journal, journal_path, report.moved_files % 64 == 0)?;
                }
                Err(e) => {
                    journal.statuses[gidx] = OpState::Failed;
                    let _ = write_journal(journal, journal_path, true); // durable stop record
                    return Err(e);
                }
            }
            gidx += 1;
        }

        if let Some(sw) = &op.write_sidecar {
            // Skip-idempotent; NOREPLACE inside write_sidecar — see Task 3.
            if !sw.path.exists() {
                match crate::xmp::write_sidecar(&sw.path, &sw.tags, sw.rating) {
                    Ok(()) => report.sidecars_written += 1,
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(e) => {
                        return Err(ApplyError::Fs {
                            path: sw.path.clone(),
                            source: e,
                        });
                    }
                }
            }
        }
        report.moved_shots += 1;
    }

    let _ = std::fs::remove_file(journal_path); // success: journal retired (spec §8 rev 3)
    Ok(report)
}

/// Refuse a cross-filesystem run that cannot fit. Same-FS runs move no bytes and
/// are never gated. Uses the first source file to decide FS-crossing vs `dest`.
fn preflight(plan: &ApplyPlan, fs: &dyn FsOps) -> Result<(), ApplyError> {
    let first_from = plan
        .ops
        .iter()
        .flat_map(|o| o.moves.iter())
        .map(|m| &m.from)
        .next();
    if let Some(src) = first_from {
        let same = fs
            .same_filesystem(src, &plan.dest)
            .map_err(|e| ApplyError::Fs {
                path: plan.dest.clone(),
                source: e,
            })?;
        if !same {
            let avail = fs.free_space(&plan.dest).map_err(|e| ApplyError::Fs {
                path: plan.dest.clone(),
                source: e,
            })?;
            if avail < plan.total_bytes {
                return Err(ApplyError::Preflight(format!(
                    "insufficient free space at {}: need {} bytes, {} available",
                    plan.dest.display(),
                    plan.total_bytes,
                    avail
                )));
            }
        }
    }
    Ok(())
}

/// Journals the plan FIRST, then executes each `ShotOp` group. Same-FS only in
/// Task 3; cross-FS + preflight land in Tasks 5 and 9.
pub fn apply(
    plan: &ApplyPlan,
    fs: &dyn FsOps,
    journal_path: &Path,
) -> Result<ApplyReport, ApplyError> {
    preflight(plan, fs)?; // refuse rather than abort halfway
    let mut journal = Journal {
        plan: plan.clone(),
        statuses: vec![OpState::Pending; total_move_count(plan)],
    };
    write_journal(&journal, journal_path, true)?; // JOURNAL FIRST — durable before any move
    execute(&mut journal, fs, journal_path)
}

/// Read `bytes` from disk and deserialize a journal.
fn read_journal(path: &Path) -> Result<Journal, ApplyError> {
    let bytes = std::fs::read(path).map_err(|e| ApplyError::Fs {
        path: path.to_path_buf(),
        source: e,
    })?;
    serde_json::from_slice(&bytes).map_err(|e| ApplyError::Fs {
        path: path.to_path_buf(),
        source: io::Error::new(io::ErrorKind::InvalidData, e),
    })
}

/// The crash window can strand the journal on either side of reality (spec §8
/// rev 3). Reconcile it against the observable filesystem BEFORE executing:
///  - `Pending`/`Failed` move whose source is GONE and destination EXISTS →
///    the crashed run already did it: mark `Done` (a re-run would fail ENOENT
///    or, worse, surface its own work as a Collision).
///  - `Done` move whose destination is MISSING while the source still exists →
///    the journal outran a lost rename: mark `Pending`, re-execute.
///
/// Anything else (both present, both absent) is left alone and will surface
/// loudly through the normal NOREPLACE/ENOENT paths.
fn reconcile(journal: &mut Journal, fs: &dyn FsOps) {
    let mut gidx = 0usize;
    let ops = journal.plan.ops.clone();
    for op in &ops {
        for mv in &op.moves {
            let src = fs.file_len(&mv.from).is_ok();
            let dst = fs.file_len(&mv.to).is_ok();
            match journal.statuses[gidx] {
                OpState::Pending | OpState::Failed if !src && dst => {
                    journal.statuses[gidx] = OpState::Done;
                }
                OpState::Done if !dst && src => {
                    journal.statuses[gidx] = OpState::Pending;
                }
                _ => {}
            }
            gidx += 1;
        }
    }
}

/// Resume a crashed/aborted run from its journal: reconcile against the disk
/// (rev 3), skip `Done` moves, continue the rest; the journal is removed on
/// success by `execute`. Detected + offered on next launch (the offer UX is
/// Phase 6). Does not re-run the free-space preflight — the journal is trusted
/// as the source of truth for WHAT to do; the disk for what already happened.
pub fn resume(journal_path: &Path, fs: &dyn FsOps) -> Result<ApplyReport, ApplyError> {
    let mut journal = read_journal(journal_path)?;
    reconcile(&mut journal, fs);
    execute(&mut journal, fs, journal_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsops::fake::FakeFs;
    use crate::model::{BUCKET_BESTS, BUCKET_KEEP, BUCKET_PICKS, BUCKET_REJECTED, BUCKET_REST};
    use crate::plan::{ApplyPlan, FileMove, ShotOp, SidecarWrite, TierCountsPlan};
    use std::path::{Path, PathBuf};

    // ---- test builders (shared across apply tests) ----
    pub(super) fn buckets() -> [String; 5] {
        [
            BUCKET_REJECTED.into(),
            BUCKET_REST.into(),
            BUCKET_KEEP.into(),
            BUCKET_PICKS.into(),
            BUCKET_BESTS.into(),
        ]
    }

    pub(super) fn shot(
        stem: &str,
        bucket: &str,
        srcs: &[(&str, u64)],
        dest: &Path,
    ) -> (ShotOp, u64) {
        let mut moves = Vec::new();
        let mut bytes = 0u64;
        for (name, len) in srcs {
            let from = PathBuf::from(format!("/src/{name}"));
            let to = dest.join(bucket).join(name);
            moves.push(FileMove { from, to });
            bytes += *len;
        }
        (
            ShotOp {
                stem: stem.into(),
                bucket: bucket.into(),
                moves,
                write_sidecar: None, // FakeFs tests never do real xmp I/O; see Task 11
                suffix: None,
            },
            bytes,
        )
    }

    pub(super) fn plan_of(dest: &Path, ops: Vec<ShotOp>, total_bytes: u64) -> ApplyPlan {
        ApplyPlan {
            dest: dest.to_path_buf(),
            buckets: buckets(),
            ops,
            per_bucket_counts: TierCountsPlan::default(),
            skipped_sidecar_writes: Vec::new(),
            stale: Vec::new(),
            total_bytes,
        }
    }

    pub(super) fn seed_sources(fs: &FakeFs, ops: &[ShotOp]) {
        for op in ops {
            for m in &op.moves {
                fs.seed_file(m.from.clone(), 100);
            }
        }
    }

    #[test]
    fn apply_same_fs_moves_every_file_and_journals_all_done() {
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot(
            "IMG_0001",
            BUCKET_KEEP,
            &[("IMG_0001.JPG", 100), ("IMG_0001.CR3", 100)],
            &dest,
        );
        let (s2, b2) = shot("IMG_0002", BUCKET_PICKS, &[("IMG_0002.JPG", 100)], &dest);
        let plan = plan_of(&dest, vec![s1, s2], b1 + b2);

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops);

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");

        let report = apply(&plan, &fs, &jpath).unwrap();

        // Every source moved into its bucket; sources consumed.
        assert!(!fs.exists(&PathBuf::from("/src/IMG_0001.JPG")));
        assert_eq!(
            fs.len_of(&dest.join(BUCKET_KEEP).join("IMG_0001.JPG")),
            Some(100)
        );
        assert_eq!(
            fs.len_of(&dest.join(BUCKET_KEEP).join("IMG_0001.CR3")),
            Some(100)
        );
        assert_eq!(
            fs.len_of(&dest.join(BUCKET_PICKS).join("IMG_0002.JPG")),
            Some(100)
        );

        // Buckets were created.
        assert!(fs.dir_exists(&dest.join(BUCKET_REJECTED)));
        assert!(fs.dir_exists(&dest.join(BUCKET_BESTS)));

        assert_eq!(report.moved_shots, 2);
        assert_eq!(report.moved_files, 3);
        assert_eq!(report.stopped_at, None);

        // Success REMOVES the journal (spec §8 rev 3): a finished run must never
        // read as a crashed one or hijack a later apply into the same dest.
        // (Journal-first existence + incremental Done marking are proven by the
        // failure-path tests in Task 4.)
        assert!(!jpath.exists(), "journal removed on full success");
    }

    // Controller-added: carry-forward from the Phase 3 final review — a serde
    // derive drift on `Journal` (or any type it embeds) would silently corrupt
    // the crash-recovery record, so pin an exact round-trip on a non-trivial
    // plan (a move + a sidecar write, non-empty skip/stale lists, nonzero
    // bytes) alongside a mixed `statuses` vec.
    #[test]
    fn journal_serde_round_trip() {
        let dest = PathBuf::from("/dst");
        let (mut s1, b1) = shot(
            "IMG_0001",
            BUCKET_KEEP,
            &[("IMG_0001.JPG", 100), ("IMG_0001.CR3", 100)],
            &dest,
        );
        s1.write_sidecar = Some(SidecarWrite {
            path: dest.join(BUCKET_KEEP).join("IMG_0001.xmp"),
            tags: vec!["landscape".into(), "sunset".into()],
            rating: Some(5),
        });
        let mut plan = plan_of(&dest, vec![s1], b1);
        plan.skipped_sidecar_writes = vec!["IMG_0002".into()];
        plan.stale = vec!["IMG_0003".into()];

        let journal = Journal {
            plan,
            statuses: vec![OpState::Done, OpState::Pending, OpState::Failed],
        };

        let bytes = serde_json::to_vec(&journal).unwrap();
        let round_tripped: Journal = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(round_tripped, journal);
    }

    #[test]
    fn journal_persists_incrementally_and_before_first_move() {
        let dest = PathBuf::from("/dst");
        // One shot, three files; fail the SECOND move (index 1).
        let (s1, b1) = shot(
            "IMG_0007",
            BUCKET_KEEP,
            &[
                ("IMG_0007.JPG", 100),
                ("IMG_0007.CR3", 100),
                ("IMG_0007.xmp", 100),
            ],
            &dest,
        );
        let plan = plan_of(&dest, vec![s1], b1);

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops);
        fs.deny_rename_from("/src/IMG_0007.CR3"); // second move fails with EACCES

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");

        let err = apply(&plan, &fs, &jpath).unwrap_err();
        assert!(matches!(err, ApplyError::Fs { .. }));

        // Durable journal reflects incremental progress: [Done, Failed, Pending].
        let j: Journal = serde_json::from_slice(&std::fs::read(&jpath).unwrap()).unwrap();
        assert_eq!(
            j.statuses,
            vec![OpState::Done, OpState::Failed, OpState::Pending]
        );

        // First file really moved; the failing file's source is untouched.
        assert!(!fs.exists(&PathBuf::from("/src/IMG_0007.JPG")));
        assert!(fs.exists(&PathBuf::from("/src/IMG_0007.CR3")));
        assert!(fs.exists(&PathBuf::from("/src/IMG_0007.xmp")));
    }

    #[test]
    fn journal_exists_even_when_the_very_first_move_fails() {
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot("IMG_0009", BUCKET_KEEP, &[("IMG_0009.JPG", 100)], &dest);
        let plan = plan_of(&dest, vec![s1], b1);

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops);
        fs.deny_rename_from("/src/IMG_0009.JPG"); // first move fails

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");

        let _ = apply(&plan, &fs, &jpath).unwrap_err();
        assert!(jpath.exists(), "journal was written before the first move");
        let j: Journal = serde_json::from_slice(&std::fs::read(&jpath).unwrap()).unwrap();
        assert_eq!(j.statuses, vec![OpState::Failed]);
    }

    #[test]
    fn apply_cross_fs_copies_verifies_then_removes_source() {
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot(
            "IMG_0100",
            BUCKET_BESTS,
            &[("IMG_0100.JPG", 100), ("IMG_0100.CR3", 100)],
            &dest,
        );
        let plan = plan_of(&dest, vec![s1], b1);

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops);
        fs.set_free(u64::MAX); // preflight (Task 9) will pass
        fs.set_cross_fs(true); // rename returns EXDEV → copy path

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");

        let report = apply(&plan, &fs, &jpath).unwrap();
        assert_eq!(report.moved_files, 2);

        let jpg_final = dest.join(BUCKET_BESTS).join("IMG_0100.JPG");
        let jpg_partial = dest.join(BUCKET_BESTS).join(".IMG_0100.JPG.partial");

        // Final present + correct length; source removed; partial cleaned up.
        assert_eq!(fs.len_of(&jpg_final), Some(100));
        assert!(
            !fs.exists(&PathBuf::from("/src/IMG_0100.JPG")),
            "verified source removed"
        );
        assert!(
            !fs.exists(&jpg_partial),
            "partial published, not left behind"
        );

        // Durability ordering was exercised: partial fsynced, bucket dir fsynced.
        assert!(fs.fsynced_files().contains(&jpg_partial));
        assert!(fs.fsynced_dirs().contains(&dest.join(BUCKET_BESTS)));

        // ORDER (spec §8 rev 3), asserted on the event log, not just membership:
        // publish rename BEFORE the dir fsync, source unlink strictly last.
        let ev = fs.events();
        let pos = |needle: &str| {
            ev.iter()
                .position(|e| e == needle)
                .unwrap_or_else(|| panic!("missing {needle} in {ev:?}"))
        };
        let publish = pos(&format!(
            "rename:{}->{}",
            jpg_partial.display(),
            jpg_final.display()
        ));
        let dirsync = pos(&format!("fsync_dir:{}", dest.join(BUCKET_BESTS).display()));
        let unlink = pos("remove:/src/IMG_0100.JPG");
        assert!(
            publish < dirsync,
            "dir fsync must FOLLOW the publish rename: {ev:?}"
        );
        assert!(
            dirsync < unlink,
            "source unlink must follow the dir fsync: {ev:?}"
        );
    }

    #[test]
    fn apply_collision_between_plan_and_apply_fails_loudly() {
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot("IMG_0200", BUCKET_KEEP, &[("IMG_0200.JPG", 100)], &dest);
        let plan = plan_of(&dest, vec![s1], b1);

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops);
        // A file materialized at the destination AFTER planning.
        let target = dest.join(BUCKET_KEEP).join("IMG_0200.JPG");
        fs.seed_file(target.clone(), 999);

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");

        let err = apply(&plan, &fs, &jpath).unwrap_err();
        match err {
            ApplyError::Collision(p) => assert_eq!(p, target),
            other => panic!("expected Collision, got {other:?}"),
        }
        // NEVER overwritten; source stays put.
        assert_eq!(
            fs.len_of(&target),
            Some(999),
            "existing dest file untouched"
        );
        assert!(
            fs.exists(&PathBuf::from("/src/IMG_0200.JPG")),
            "source not moved"
        );
    }

    /// Stem of the shot where the run stopped: the op owning the first non-`Done` move.
    fn stopped_stem(j: &Journal) -> Option<String> {
        let mut gidx = 0usize;
        for op in &j.plan.ops {
            for _ in &op.moves {
                if j.statuses[gidx] != OpState::Done {
                    return Some(op.stem.clone());
                }
                gidx += 1;
            }
        }
        None
    }

    #[test]
    fn apply_group_atomicity_stops_and_records_partial() {
        let dest = PathBuf::from("/dst");
        // shot A completes; shot B fails on its RAW (second file) → stop at B.
        let (a, ba) = shot("IMG_0300", BUCKET_KEEP, &[("IMG_0300.JPG", 100)], &dest);
        let (b, bb) = shot(
            "IMG_0301",
            BUCKET_PICKS,
            &[
                ("IMG_0301.JPG", 100),
                ("IMG_0301.CR3", 100),
                ("IMG_0301.xmp", 100),
            ],
            &dest,
        );
        let plan = plan_of(&dest, vec![a, b], ba + bb);

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops);
        fs.deny_rename_from("/src/IMG_0301.CR3"); // later file in shot B

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");

        let err = apply(&plan, &fs, &jpath).unwrap_err();
        assert!(matches!(err, ApplyError::Fs { .. }));

        // Shot A fully moved; shot B's first file moved, its RAW + xmp still at source.
        assert!(!fs.exists(&PathBuf::from("/src/IMG_0300.JPG")));
        assert!(!fs.exists(&PathBuf::from("/src/IMG_0301.JPG")));
        assert!(fs.exists(&PathBuf::from("/src/IMG_0301.CR3")));
        assert!(fs.exists(&PathBuf::from("/src/IMG_0301.xmp")));

        // Durable journal is the stop-of-record.
        let j: Journal = serde_json::from_slice(&std::fs::read(&jpath).unwrap()).unwrap();
        assert_eq!(
            j.statuses,
            vec![
                OpState::Done,
                OpState::Done,
                OpState::Failed,
                OpState::Pending
            ]
        );
        assert_eq!(stopped_stem(&j), Some("IMG_0301".to_string())); // ApplyReport.stopped_at equivalent
    }

    #[test]
    fn resume_continues_a_crashed_run_from_the_journal() {
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot(
            "IMG_0400",
            BUCKET_KEEP,
            &[
                ("IMG_0400.JPG", 100),
                ("IMG_0400.CR3", 100),
                ("IMG_0400.xmp", 100),
            ],
            &dest,
        );
        let plan = plan_of(&dest, vec![s1], b1);

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops);
        fs.deny_rename_from("/src/IMG_0400.CR3"); // crash on the second file

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");

        // First run stops at the RAW; JPEG already moved.
        let _ = apply(&plan, &fs, &jpath).unwrap_err();
        assert!(!fs.exists(&PathBuf::from("/src/IMG_0400.JPG")));
        assert!(fs.exists(&PathBuf::from("/src/IMG_0400.CR3")));

        // The fault clears (e.g. permissions fixed); resume from the same journal.
        fs.clear_faults();
        let report = resume(&jpath, &fs).unwrap();

        // Only the not-yet-done files moved this run; JPEG skipped (already Done).
        assert_eq!(report.moved_files, 2);
        assert!(!fs.exists(&PathBuf::from("/src/IMG_0400.CR3")));
        assert!(!fs.exists(&PathBuf::from("/src/IMG_0400.xmp")));
        assert_eq!(
            fs.len_of(&dest.join(BUCKET_KEEP).join("IMG_0400.CR3")),
            Some(100)
        );

        // Success retires the journal (spec §8 rev 3).
        assert!(!jpath.exists(), "journal removed once the resume completes");
    }

    #[test]
    fn resume_reconciles_crash_between_move_and_journal_update() {
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot(
            "IMG_0410",
            BUCKET_KEEP,
            &[("IMG_0410.JPG", 100), ("IMG_0410.CR3", 100)],
            &dest,
        );
        let plan = plan_of(&dest, vec![s1], b1);

        let fs = FakeFs::new();
        // The crashed run moved the JPG but died BEFORE journaling it:
        fs.seed_file(dest.join(BUCKET_KEEP).join("IMG_0410.JPG"), 100);
        fs.seed_file("/src/IMG_0410.CR3", 100);

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");
        let stale = Journal {
            plan: plan.clone(),
            statuses: vec![OpState::Pending, OpState::Pending],
        };
        std::fs::write(&jpath, serde_json::to_vec(&stale).unwrap()).unwrap();

        // rev 3: reconciliation sees from-gone + to-present ⇒ Done, so resume
        // completes instead of dying on ENOENT / EEXIST-as-Collision.
        let report = resume(&jpath, &fs).unwrap();
        assert_eq!(
            report.moved_files, 1,
            "only the CR3 actually moved this run"
        );
        assert_eq!(
            fs.len_of(&dest.join(BUCKET_KEEP).join("IMG_0410.CR3")),
            Some(100)
        );
        assert!(!jpath.exists(), "journal removed on success");
    }

    #[test]
    fn resume_reexecutes_a_done_move_the_disk_never_saw() {
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot("IMG_0420", BUCKET_KEEP, &[("IMG_0420.JPG", 100)], &dest);
        let plan = plan_of(&dest, vec![s1], b1);

        let fs = FakeFs::new();
        // Journal was fsynced Done, but the rename itself was lost to the crash:
        fs.seed_file("/src/IMG_0420.JPG", 100);

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");
        let stale = Journal {
            plan: plan.clone(),
            statuses: vec![OpState::Done],
        };
        std::fs::write(&jpath, serde_json::to_vec(&stale).unwrap()).unwrap();

        // rev 3: Done + dest-missing + source-present ⇒ re-execute, not skip —
        // otherwise the shot is silently left behind while the run reports success.
        let report = resume(&jpath, &fs).unwrap();
        assert_eq!(report.moved_files, 1);
        assert_eq!(
            fs.len_of(&dest.join(BUCKET_KEEP).join("IMG_0420.JPG")),
            Some(100)
        );
        assert!(!fs.exists(&PathBuf::from("/src/IMG_0420.JPG")));
    }

    // Post-final-review addition: pins spec §8 rev 3's "an all-Done journal is
    // never re-executed" property. Disk state models a crashed run that fully
    // completed and journaled Done for every move but died before the journal
    // could be retired — only the DEST files exist, sources are already gone.
    // Phase 6's `find_crashed_apply` gate relies on `resume` treating this as
    // a safe no-op rather than attempting (and failing) to re-move vanished
    // sources.
    #[test]
    fn resume_on_all_done_journal_is_a_safe_noop() {
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot(
            "IMG_0900",
            BUCKET_KEEP,
            &[("IMG_0900.JPG", 100), ("IMG_0900.CR3", 100)],
            &dest,
        );
        let plan = plan_of(&dest, vec![s1], b1);

        let fs = FakeFs::new();
        // The moves already happened: dest files present, sources NOT seeded.
        fs.seed_file(dest.join(BUCKET_KEEP).join("IMG_0900.JPG"), 100);
        fs.seed_file(dest.join(BUCKET_KEEP).join("IMG_0900.CR3"), 100);

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");
        let done = Journal {
            plan: plan.clone(),
            statuses: vec![OpState::Done, OpState::Done],
        };
        std::fs::write(&jpath, serde_json::to_vec(&done).unwrap()).unwrap();

        let report = resume(&jpath, &fs).unwrap();

        // Nothing re-executed — a re-move would have failed ENOENT (no source).
        assert_eq!(report.moved_files, 0, "an all-Done journal replays nothing");
        assert_eq!(
            fs.len_of(&dest.join(BUCKET_KEEP).join("IMG_0900.JPG")),
            Some(100)
        );
        assert_eq!(
            fs.len_of(&dest.join(BUCKET_KEEP).join("IMG_0900.CR3")),
            Some(100)
        );
        assert!(!jpath.exists(), "journal retired even though nothing ran");
    }

    #[test]
    fn preflight_refuses_when_cross_fs_and_not_enough_space() {
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot(
            "IMG_0500",
            BUCKET_KEEP,
            &[("IMG_0500.JPG", 100), ("IMG_0500.CR3", 100)],
            &dest,
        );
        let plan = plan_of(&dest, vec![s1], b1); // total_bytes = 200

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops);
        fs.set_cross_fs(true);
        fs.set_free(150); // < 200

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");

        let err = apply(&plan, &fs, &jpath).unwrap_err();
        assert!(matches!(err, ApplyError::Preflight(_)));
        // Refused BEFORE any move — sources all intact, no journal, no buckets.
        assert!(fs.exists(&PathBuf::from("/src/IMG_0500.JPG")));
        assert!(fs.exists(&PathBuf::from("/src/IMG_0500.CR3")));
        assert!(!jpath.exists(), "no journal written when preflight refuses");
    }

    #[test]
    fn same_fs_never_free_space_gated() {
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot("IMG_0501", BUCKET_KEEP, &[("IMG_0501.JPG", 100)], &dest);
        let plan = plan_of(&dest, vec![s1], b1);

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops);
        fs.set_free(0); // irrelevant: same-FS rename moves no bytes

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");
        apply(&plan, &fs, &jpath).unwrap(); // succeeds despite free==0
        assert!(!fs.exists(&PathBuf::from("/src/IMG_0501.JPG")));
    }

    #[test]
    fn mid_copy_enospc_leaves_source_untouched() {
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot("IMG_0502", BUCKET_KEEP, &[("IMG_0502.JPG", 100)], &dest);
        let plan = plan_of(&dest, vec![s1], b1);

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops);
        fs.set_cross_fs(true);
        fs.set_free(u64::MAX); // preflight passes
        fs.set_enospc_on_copy(true); // but the copy itself fails ENOSPC

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");

        let err = apply(&plan, &fs, &jpath).unwrap_err();
        assert!(matches!(err, ApplyError::Fs { .. }));
        // Source intact, no final, no leftover partial.
        assert!(fs.exists(&PathBuf::from("/src/IMG_0502.JPG")));
        assert!(!fs.exists(&dest.join(BUCKET_KEEP).join("IMG_0502.JPG")));
        assert!(!fs.exists(&dest.join(BUCKET_KEEP).join(".IMG_0502.JPG.partial")));
    }

    #[test]
    fn permission_error_is_surfaced_per_file() {
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot("IMG_0600", BUCKET_KEEP, &[("IMG_0600.JPG", 100)], &dest);
        let plan = plan_of(&dest, vec![s1], b1);

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops);
        fs.deny_rename_from("/src/IMG_0600.JPG");

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");

        let err = apply(&plan, &fs, &jpath).unwrap_err();
        match err {
            ApplyError::Fs { path, source } => {
                assert_eq!(path, PathBuf::from("/src/IMG_0600.JPG"));
                assert_eq!(source.kind(), std::io::ErrorKind::PermissionDenied);
            }
            other => panic!("expected Fs error, got {other:?}"),
        }
        assert!(
            fs.exists(&PathBuf::from("/src/IMG_0600.JPG")),
            "no data lost"
        );
    }

    #[test]
    fn cross_fs_permission_on_source_remove_surfaces_but_copy_is_published() {
        // The verified copy is already published to dest before source removal;
        // a permission error on the final unlink surfaces the source path.
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot("IMG_0601", BUCKET_KEEP, &[("IMG_0601.JPG", 100)], &dest);
        let plan = plan_of(&dest, vec![s1], b1);

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops);
        fs.set_cross_fs(true);
        fs.set_free(u64::MAX);
        fs.deny_remove("/src/IMG_0601.JPG"); // final unlink denied

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");

        let err = apply(&plan, &fs, &jpath).unwrap_err();
        match err {
            ApplyError::Fs { path, source } => {
                assert_eq!(path, PathBuf::from("/src/IMG_0601.JPG"));
                assert_eq!(source.kind(), std::io::ErrorKind::PermissionDenied);
            }
            other => panic!("expected Fs error, got {other:?}"),
        }
        // Copy is durably published; source lingers (a duplicate, never a loss).
        assert_eq!(
            fs.len_of(&dest.join(BUCKET_KEEP).join("IMG_0601.JPG")),
            Some(100)
        );
        assert!(fs.exists(&PathBuf::from("/src/IMG_0601.JPG")));
    }

    // Task 11: sidecar writes are real filesystem I/O (they do NOT route
    // through `FsOps`), so this test uses `RealFs` + a temp dir — mixing
    // `FakeFs` (in-memory) with a real `xmp` write is meaningless. All
    // `FakeFs` apply tests keep `write_sidecar: None`; here is where the
    // sidecar path is exercised end-to-end.
    //
    // NOTE: `SidecarWrite` is already imported at the top of this `mod
    // tests` (see the `use crate::plan::{ .. SidecarWrite .. }` above);
    // re-importing it here would be a duplicate `use` of the same name in
    // the same scope (rustc E0252), so only `RealFs` is a new import.
    use crate::fsops::RealFs;

    #[test]
    fn apply_writes_fresh_sidecar_into_bucket_realfs() {
        let root = tempfile::tempdir().unwrap();
        let src_dir = root.path().join("src");
        let dest = root.path().join("dst");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::create_dir_all(&dest).unwrap();

        let jpg = src_dir.join("IMG_0700.JPG");
        std::fs::write(&jpg, vec![1u8; 128]).unwrap();

        let bucket = crate::model::BUCKET_KEEP;
        let sidecar_path = dest.join(bucket).join("IMG_0700.xmp");
        let op = ShotOp {
            stem: "IMG_0700".into(),
            bucket: bucket.into(),
            moves: vec![FileMove {
                from: jpg.clone(),
                to: dest.join(bucket).join("IMG_0700.JPG"),
            }],
            write_sidecar: Some(SidecarWrite {
                path: sidecar_path.clone(),
                tags: vec!["portrait".into(), "golden-hour".into()],
                rating: Some(3), // Keep → 3
            }),
            suffix: None,
        };
        let plan = plan_of(&dest, vec![op], 128);

        let jpath = dest.join(".fastcull-apply.json");
        let report = apply(&plan, &RealFs, &jpath).unwrap();

        // File moved; fresh sidecar written into the bucket and counted.
        assert!(dest.join(bucket).join("IMG_0700.JPG").exists());
        assert!(!jpg.exists());
        assert_eq!(report.sidecars_written, 1);
        assert!(sidecar_path.exists());
        let xmp = std::fs::read_to_string(&sidecar_path).unwrap();
        assert!(xmp.contains("portrait"), "dc:subject keyword present");
        assert!(xmp.contains("golden-hour"));
        assert!(xmp.contains("3"), "xmp:Rating present");
    }

    // Controller-added (checklist: T11 must cover "sidecar write +
    // skip-idempotent re-run"). Pins spec §8 rev-3: "on resume an
    // already-present sidecar target is skipped, not clobbered and not an
    // error." The pre-seeded sentinel file here stands in for the disk state
    // a resumed run would find (a sidecar already published by an earlier,
    // interrupted apply) — `apply` must treat it as a no-op skip, not a
    // clobber and not a failure.
    #[test]
    fn apply_skips_existing_sidecar_target_never_clobbers_realfs() {
        let root = tempfile::tempdir().unwrap();
        let src_dir = root.path().join("src");
        let dest = root.path().join("dst");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::create_dir_all(&dest).unwrap();

        let jpg = src_dir.join("IMG_0701.JPG");
        std::fs::write(&jpg, vec![2u8; 128]).unwrap();

        let bucket = crate::model::BUCKET_KEEP;
        let bucket_dir = dest.join(bucket);
        std::fs::create_dir_all(&bucket_dir).unwrap();
        let sidecar_path = bucket_dir.join("IMG_0701.xmp");
        let sentinel = "SENTINEL - user's existing sidecar";
        std::fs::write(&sidecar_path, sentinel).unwrap();

        let op = ShotOp {
            stem: "IMG_0701".into(),
            bucket: bucket.into(),
            moves: vec![FileMove {
                from: jpg.clone(),
                to: dest.join(bucket).join("IMG_0701.JPG"),
            }],
            write_sidecar: Some(SidecarWrite {
                path: sidecar_path.clone(),
                tags: vec!["portrait".into()],
                rating: Some(3),
            }),
            suffix: None,
        };
        let plan = plan_of(&dest, vec![op], 128);

        let jpath = dest.join(".fastcull-apply.json");
        let report = apply(&plan, &RealFs, &jpath).unwrap();

        // An existing sidecar target is a SKIP, not an error.
        assert_eq!(report.sidecars_written, 0);
        // Never clobbered: byte-for-byte the sentinel the "user" already had.
        let content = std::fs::read_to_string(&sidecar_path).unwrap();
        assert_eq!(content, sentinel, "existing sidecar content untouched");
        // The jpeg move itself still completed.
        assert!(dest.join(bucket).join("IMG_0701.JPG").exists());
        assert!(!jpg.exists());
    }
}
