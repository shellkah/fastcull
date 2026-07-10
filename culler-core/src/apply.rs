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

/// Journals the plan FIRST, then executes each `ShotOp` group. Same-FS only in
/// Task 3; cross-FS + preflight land in Tasks 5 and 9.
pub fn apply(
    plan: &ApplyPlan,
    fs: &dyn FsOps,
    journal_path: &Path,
) -> Result<ApplyReport, ApplyError> {
    let mut journal = Journal {
        plan: plan.clone(),
        statuses: vec![OpState::Pending; total_move_count(plan)],
    };
    write_journal(&journal, journal_path, true)?; // JOURNAL FIRST — durable before any move
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
}
