//! The safe-move apply engine. Journals the plan before the first move, moves
//! each shot's fileset group-atomically through `FsOps`, and stops loudly on the
//! first failure. No deletion step exists beyond the cross-FS path removing its
//! own verified source (Task 5). Resumable via `resume` (Task 8).

use std::collections::BTreeSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fsops::FsOps;
use crate::plan::{ApplyPlan, FileMove};

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum OpState {
    Pending,
    /// Cross-FS only (spec §8 rev 4): the publish rename succeeded — the
    /// destination is definitively ours — but the source unlink is still
    /// owed. Resume completes the unlink rather than re-copying into its own
    /// finished copy; same-FS moves never enter this state. Additive variant:
    /// older journals serialize only `Pending`/`Done`/`Failed` and still
    /// parse (see `journal_with_only_pre_published_variant_names_parses`).
    Published,
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

/// Outcome of `move_one`: whether the move finished immediately (same-FS
/// rename) or only reached the cross-FS "published" checkpoint, where the
/// destination copy is durably renamed into place but the source unlink is
/// still owed (spec §8 rev 4 — see `OpState::Published`). Same-FS moves never
/// produce `Published`; there is no window for it to close.
enum MoveOutcome {
    Moved,
    Published,
}

/// Move one file same-FS (rename), mapping no-clobber `EEXIST` to `Collision`,
/// falling back to the cross-FS publish path (`publish_cross_fs`) on `EXDEV`.
/// A cross-FS move stops at `Published` — the caller (`execute`) journals
/// that checkpoint before finishing the source unlink via `finish_cross_fs`.
fn move_one(fs: &dyn FsOps, from: &Path, to: &Path) -> Result<MoveOutcome, ApplyError> {
    match fs.rename_noreplace(from, to) {
        Ok(()) => Ok(MoveOutcome::Moved),
        Err(e) if is_exdev(&e) => publish_cross_fs(fs, from, to).map(|()| MoveOutcome::Published),
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

/// Cross-filesystem publish: copy source → partial, fsync, verify byte
/// length, then publish `.partial` → final with NOREPLACE. Source is never
/// touched until the destination copy is fully copied, fsynced,
/// length-verified, and atomically published; a mid-copy failure leaves the
/// source untouched and cleans up the partial. Stops as soon as the publish
/// rename succeeds — the caller journals `Published` (dest is definitively
/// ours) before calling `finish_cross_fs` to fsync the directory and remove
/// the source. Splitting the checkpoint out here is what spec §8 rev 4 needs:
/// a crash in the publish→unlink window resumes by finishing the unlink,
/// never by re-copying into this function's own already-published copy.
fn publish_cross_fs(fs: &dyn FsOps, from: &Path, to: &Path) -> Result<(), ApplyError> {
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
    // Publish partial → final (no clobber). Once this succeeds the
    // destination is DEFINITIVELY ours: the caller journals `Published`
    // immediately, then `finish_cross_fs` fsyncs the directory and removes
    // the source (spec §8 rev 3/4 — the dir fsync must not happen before
    // this rename; rev 2 fsynced the dir first, which made the `.partial`
    // entry durable instead of the final one).
    match fs.rename_noreplace(&partial, to) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let _ = fs.remove_file(&partial);
            Err(ApplyError::Collision(to.to_path_buf()))
        }
        Err(e) => {
            let _ = fs.remove_file(&partial);
            Err(ApplyError::Fs {
                path: to.to_path_buf(),
                source: e,
            })
        }
    }
}

/// Finish a durably-`Published` cross-FS move: fsync the destination
/// directory (durability of the publish rename — the source unlink below
/// happens on a *different* filesystem, so this must come first, spec §8 rev
/// 3), then remove the source. A dir-fsync failure stops loudly without
/// touching the source or the destination — worst case a duplicate (source +
/// dest both present), never a loss. A `NotFound` on the source removal is
/// treated as already-completed rather than an error: `reconcile` only ever
/// leaves a `Published` entry for `execute` to finish when the source is
/// still present (source-gone reconciles straight to `Done`), so by the time
/// this runs NotFound means the unlink already happened (e.g. a re-resumed
/// run) — disclosed choice, spec rev 4.
fn finish_cross_fs(fs: &dyn FsOps, from: &Path, to: &Path) -> Result<(), ApplyError> {
    if let Some(dir) = to.parent()
        && let Err(e) = fs.fsync_dir(dir)
    {
        return Err(ApplyError::Fs {
            path: dir.to_path_buf(),
            source: e,
        });
    }
    match fs.remove_file(from) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(ApplyError::Fs {
            path: from.to_path_buf(),
            source: e,
        }),
    }
}

/// Finish an entry already sitting at `Published` (either just reached this
/// run, or found that way on resume): fsync the dest dir + remove the source
/// via `finish_cross_fs`, then mark `Done` and journal it (batched, same
/// policy as every other Done write) — counted in `moved_files` (spec §8 rev
/// 4: "a move brought to Done this run"). A finish-stage failure re-writes
/// `Published` as the durable stop record (forced sync): `Published` is
/// ALREADY the correct record here (the dest is definitively ours), so
/// downgrading to `Failed` would erase that fact and strand a future resume
/// in the duplicate-stop check.
fn finish_published(
    fs: &dyn FsOps,
    mv: &FileMove,
    journal: &mut Journal,
    gidx: usize,
    journal_path: &Path,
    report: &mut ApplyReport,
) -> Result<(), ApplyError> {
    match finish_cross_fs(fs, &mv.from, &mv.to) {
        Ok(()) => {
            journal.statuses[gidx] = OpState::Done;
            report.moved_files += 1;
            write_journal(journal, journal_path, report.moved_files.is_multiple_of(64))
        }
        Err(e) => {
            let _ = write_journal(journal, journal_path, true); // durable Published stop record
            Err(e)
        }
    }
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
    // Dirs whose sidecar target is PRESENT on disk after this run handled it
    // (spec §8 rev 4, session-scoped per the controller adjudication — the
    // durability guarantee is "durable against a post-success power cut" for
    // the *session*, not just the invocation that happened to write the
    // byte). A BTreeSet dedupes (many shots share a bucket dir) and keeps the
    // durability pass below deterministic. Tracked in ALL THREE outcomes
    // below: a fresh write, a skip because the target already existed (our
    // own unfsynced prior-run write, or a harmless foreign file), or a raced
    // `AlreadyExists` from `write_sidecar` itself. The only op-with-a-sidecar
    // arm that does NOT track is a genuine write failure, which returns
    // before reaching the tracking code. A plan with no `SidecarWrite` at all
    // never enters this block, so it tracks nothing.
    let mut sidecar_dirs: BTreeSet<PathBuf> = BTreeSet::new();

    for op in &ops {
        for mv in &op.moves {
            match journal.statuses[gidx] {
                OpState::Done => {
                    gidx += 1;
                    continue;
                }
                OpState::Published => {
                    // Resumed mid publish→unlink window (spec §8 rev 4): the
                    // destination is already durably ours — finish the source
                    // unlink, do NOT re-copy.
                    finish_published(fs, mv, journal, gidx, journal_path, &mut report)?;
                    gidx += 1;
                    continue;
                }
                OpState::Pending | OpState::Failed => {}
            }
            match move_one(fs, &mv.from, &mv.to) {
                Ok(MoveOutcome::Moved) => {
                    journal.statuses[gidx] = OpState::Done;
                    report.moved_files += 1;
                    // Persist progress incrementally; fsync only every 64th move
                    // (and at checkpoints) — reconciliation (Task 8) makes an
                    // unsynced tail harmless, and this doubles as the progress
                    // feed the Phase-6 UI polls from a worker thread.
                    write_journal(journal, journal_path, report.moved_files.is_multiple_of(64))?;
                }
                Ok(MoveOutcome::Published) => {
                    // Publish rename just succeeded: journal the checkpoint
                    // (existing batched-fsync policy, unchanged) BEFORE
                    // finishing the unlink — spec §8 rev 4.
                    journal.statuses[gidx] = OpState::Published;
                    write_journal(journal, journal_path, report.moved_files.is_multiple_of(64))?;
                    finish_published(fs, mv, journal, gidx, journal_path, &mut report)?;
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
            let mut target_present = sw.path.exists();
            if !target_present {
                match crate::xmp::write_sidecar(&sw.path, &sw.tags, sw.rating) {
                    Ok(()) => {
                        report.sidecars_written += 1;
                        target_present = true;
                    }
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                        target_present = true; // raced with someone else's write
                    }
                    Err(e) => {
                        return Err(ApplyError::Fs {
                            path: sw.path.clone(),
                            source: e,
                        });
                    }
                }
            }
            // Track for the durability pass whenever the target is present —
            // see the `sidecar_dirs` doc comment above for why the skip and
            // already-exists arms count too, not just a fresh write.
            if target_present && let Some(dir) = sw.path.parent() {
                sidecar_dirs.insert(dir.to_path_buf());
            }
        }
        report.moved_shots += 1;
    }

    // End-of-success durability pass (spec §8 rev 4, session-scoped per the
    // controller adjudication): sidecar renames are unjournaled, so on full
    // success — before the journal is removed — every dir holding a sidecar
    // target that is PRESENT on disk this run (see `sidecar_dirs` above) is
    // fsynced. A failure here is a loud stop: return WITHOUT removing the
    // journal, so a post-failure crash still reads as an incomplete run
    // rather than a finished one. This is what makes the retry REAL rather
    // than a false promise: because the journal stays on disk, the next
    // `resume` calls `execute` again from the top, which re-examines the
    // same sidecar target — this time via the `sw.path.exists()` skip arm,
    // since the file itself was already written before the fsync failed —
    // re-inserts its dir into `sidecar_dirs`, and this pass runs again and
    // fsyncs it. So a target left unfsynced by a failed run is retried on
    // the very next successful resume, not silently dropped.
    for dir in &sidecar_dirs {
        fs.fsync_dir(dir).map_err(|e| ApplyError::Fs {
            path: dir.clone(),
            source: e,
        })?;
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

/// Journals the plan FIRST, then executes each `ShotOp` group group-atomically
/// (same-FS rename, or cross-FS copy→publish→`Published`-checkpoint→unlink,
/// spec §8 rev 4). On full success the journal file is removed (FastCull
/// metadata, not user data). See `resume` for how a crashed run picks back up.
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

/// Read `bytes` from disk, deserialize a journal, and validate it is
/// structurally consistent before any caller can index into it.
///
/// A journal that parses as valid JSON but whose `statuses` length disagrees
/// with the plan's flattened move count (hand-edited or corrupted on disk) is
/// refused here with a precise `Preflight` error — spec rev 4 §8: "refused
/// gracefully with a precise error; the recovery path itself must never
/// panic." Both `reconcile` and `execute` index `journal.statuses[gidx]`
/// without bounds checks (that indexing is the load-bearing invariant this
/// function protects); `read_journal` is the single choke point every journal
/// read passes through — `resume` is the only current caller, but any future
/// reader inherits the guard for free by going through this function rather
/// than by re-deriving the check at each call site.
fn read_journal(path: &Path) -> Result<Journal, ApplyError> {
    let bytes = std::fs::read(path).map_err(|e| ApplyError::Fs {
        path: path.to_path_buf(),
        source: e,
    })?;
    let journal: Journal = serde_json::from_slice(&bytes).map_err(|e| ApplyError::Fs {
        path: path.to_path_buf(),
        source: io::Error::new(io::ErrorKind::InvalidData, e),
    })?;
    let expected = total_move_count(&journal.plan);
    let actual = journal.statuses.len();
    if actual != expected {
        return Err(ApplyError::Preflight(format!(
            "corrupt journal at {}: plan expects {expected} move status entries but found {actual}",
            path.display()
        )));
    }
    Ok(journal)
}

/// The crash window can strand the journal on either side of reality (spec §8
/// rev 3/4). Reconcile it against the observable filesystem BEFORE executing:
///  - `Pending`/`Failed` move whose source is GONE and destination EXISTS →
///    the crashed run already did it: mark `Done` (a re-run would fail ENOENT
///    or, worse, surface its own work as a Collision).
///  - `Done` move whose destination is MISSING while the source still exists →
///    the journal outran a lost rename: mark `Pending`, re-execute.
///  - `Published` move whose source is GONE (either destination state) →
///    the crashed run finished the unlink too: mark `Done` (rev 4).
///  - `Published` move whose source is present and destination MISSING →
///    a `Published` record vouching for an observably-absent destination
///    vouches for nothing: demote to `Pending` so it re-copies (the
///    stale-partial pre-clean in `publish_cross_fs` handles any leftover);
///    this is the controller-specified safety guard (rev 4).
///  - `Published` move whose source AND destination are both present is left
///    alone — `execute` finishes it (fsync dir, remove source) without
///    re-copying.
///
/// Anything else is left alone: a `Pending`/`Failed` cell surfaces loudly
/// through the normal NOREPLACE/ENOENT paths (or, for `Pending`, through the
/// duplicate-aware loud stop in `resume` when both source and dest are
/// present); a `Done` entry whose source and destination are both absent
/// stays skipped — nothing recoverable exists on disk.
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
                OpState::Published if !src => {
                    journal.statuses[gidx] = OpState::Done;
                }
                OpState::Published if src && !dst => {
                    journal.statuses[gidx] = OpState::Pending;
                }
                _ => {}
            }
            gidx += 1;
        }
    }
}

/// After reconciliation, refuse a resume that would unlink a source without a
/// durable record vouching for its destination (spec §8 rev 4): any
/// still-`Pending` move whose source AND destination BOTH exist is the one
/// state reconciliation cannot disambiguate (a foreign file appeared, or a
/// `Published` journal record was lost to the batched-fsync window) — it is a
/// loud stop naming the destination as a possible prior-run completed copy,
/// returned WITHOUT executing anything. `Failed` entries are not scanned
/// here: only `Pending` (untried-this-run) carries this specific ambiguity.
fn check_no_unvouched_duplicates(journal: &Journal, fs: &dyn FsOps) -> Result<(), ApplyError> {
    let mut gidx = 0usize;
    for op in &journal.plan.ops {
        for mv in &op.moves {
            if journal.statuses[gidx] == OpState::Pending
                && fs.file_len(&mv.from).is_ok()
                && fs.file_len(&mv.to).is_ok()
            {
                return Err(ApplyError::Preflight(format!(
                    "resume refused: {} already exists and its source is still present — \
                     this may be a completed copy from the prior run; verify and resolve \
                     manually before retrying (resume never unlinks a source without a \
                     durable record vouching for its destination)",
                    mv.to.display()
                )));
            }
            gidx += 1;
        }
    }
    Ok(())
}

/// Resume a crashed/aborted run from its journal: reconcile against the disk
/// (rev 3/4 — see `reconcile`), refuse loudly on an unvouched duplicate (see
/// `check_no_unvouched_duplicates`), then skip `Done` moves and continue the
/// rest; the journal is removed on success by `execute`. Detected + offered
/// on next launch (the offer UX is Phase 6). Does not re-run the free-space
/// preflight — the journal is trusted as the source of truth for WHAT to do;
/// the disk for what already happened. A fresh `apply()` never reaches this
/// scan — a fresh-run collision still surfaces as `ApplyError::Collision` via
/// NOREPLACE.
pub fn resume(journal_path: &Path, fs: &dyn FsOps) -> Result<ApplyReport, ApplyError> {
    let mut journal = read_journal(journal_path)?;
    reconcile(&mut journal, fs);
    check_no_unvouched_duplicates(&journal, fs)?;
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

    // Supervisor-verified finding (Phase 4 follow-up F1): a journal that parses
    // as valid JSON but whose `statuses` length disagrees with the plan's move
    // count (hand-edited or corrupted on disk) must be refused gracefully
    // (spec rev 4 §8: "refused gracefully with a precise error; the recovery
    // path itself must never panic"), not index out of bounds in `reconcile`/
    // `execute`. These three pin empty, short, and over-long `statuses` vecs.
    #[test]
    fn resume_refuses_empty_statuses_against_one_move_plan() {
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot("IMG_1000", BUCKET_KEEP, &[("IMG_1000.JPG", 100)], &dest);
        let plan = plan_of(&dest, vec![s1], b1); // 1 move total

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops);

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");
        let corrupt = Journal {
            plan: plan.clone(),
            statuses: vec![], // expected 1, found 0
        };
        std::fs::write(&jpath, serde_json::to_vec(&corrupt).unwrap()).unwrap();

        let err = resume(&jpath, &fs).unwrap_err();
        match err {
            ApplyError::Preflight(msg) => {
                assert!(
                    msg.contains(&jpath.display().to_string()),
                    "message names the journal path: {msg}"
                );
                assert!(
                    msg.contains("expected 1") || msg.contains("expects 1"),
                    "message names the expected count: {msg}"
                );
                assert!(msg.contains('0'), "message names the actual count: {msg}");
            }
            other => panic!("expected ApplyError::Preflight, got {other:?}"),
        }

        // Disk untouched: no move executed, no reconcile, journal left in place.
        assert!(
            fs.exists(&PathBuf::from("/src/IMG_1000.JPG")),
            "source untouched"
        );
        assert!(
            !fs.exists(&dest.join(BUCKET_KEEP).join("IMG_1000.JPG")),
            "dest not created"
        );
        assert!(jpath.exists(), "corrupt journal left on disk as evidence");
    }

    #[test]
    fn resume_refuses_short_statuses_against_two_move_plan() {
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot(
            "IMG_1001",
            BUCKET_KEEP,
            &[("IMG_1001.JPG", 100), ("IMG_1001.CR3", 100)],
            &dest,
        );
        let plan = plan_of(&dest, vec![s1], b1); // 2 moves total

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops);

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");
        let corrupt = Journal {
            plan: plan.clone(),
            statuses: vec![OpState::Pending], // expected 2, found 1
        };
        std::fs::write(&jpath, serde_json::to_vec(&corrupt).unwrap()).unwrap();

        let err = resume(&jpath, &fs).unwrap_err();
        match err {
            ApplyError::Preflight(msg) => {
                assert!(msg.contains(&jpath.display().to_string()));
                assert!(msg.contains("expected 2") || msg.contains("expects 2"));
                assert!(msg.contains('1'));
            }
            other => panic!("expected ApplyError::Preflight, got {other:?}"),
        }

        assert!(
            fs.exists(&PathBuf::from("/src/IMG_1001.JPG")),
            "source untouched"
        );
        assert!(
            fs.exists(&PathBuf::from("/src/IMG_1001.CR3")),
            "source untouched"
        );
        assert!(!fs.exists(&dest.join(BUCKET_KEEP).join("IMG_1001.JPG")));
        assert!(!fs.exists(&dest.join(BUCKET_KEEP).join("IMG_1001.CR3")));
        assert!(jpath.exists(), "corrupt journal left on disk as evidence");
    }

    #[test]
    fn resume_refuses_overlong_statuses_against_one_move_plan() {
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot("IMG_1002", BUCKET_KEEP, &[("IMG_1002.JPG", 100)], &dest);
        let plan = plan_of(&dest, vec![s1], b1); // 1 move total

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops);

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");
        let corrupt = Journal {
            plan: plan.clone(),
            statuses: vec![OpState::Done, OpState::Pending, OpState::Failed], // expected 1, found 3
        };
        std::fs::write(&jpath, serde_json::to_vec(&corrupt).unwrap()).unwrap();

        let err = resume(&jpath, &fs).unwrap_err();
        match err {
            ApplyError::Preflight(msg) => {
                assert!(msg.contains(&jpath.display().to_string()));
                assert!(msg.contains("expected 1") || msg.contains("expects 1"));
                assert!(msg.contains('3'));
            }
            other => panic!("expected ApplyError::Preflight, got {other:?}"),
        }

        assert!(
            fs.exists(&PathBuf::from("/src/IMG_1002.JPG")),
            "source untouched"
        );
        assert!(!fs.exists(&dest.join(BUCKET_KEEP).join("IMG_1002.JPG")));
        assert!(jpath.exists(), "corrupt journal left on disk as evidence");
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

    // ---- F2 (Phase 4 follow-up): the cross-FS length-verify guard in
    // `publish_cross_fs` (`dest_len != src_len || copied != src_len`) was a
    // surviving mutant — `FakeFs::copy_create_new` always copied exactly the
    // source length, so nothing ever exercised a real short copy. These pin
    // both operands via `FakeFs::set_short_copy` / `set_lying_copy_return`. ----

    #[test]
    fn cross_fs_short_copy_is_rejected_and_source_kept_intact() {
        // set_short_copy moves BOTH `dest_len` and `copied` together (a real
        // short copy affects the bytes actually on disk AND what the copy
        // call reports), so this test kills the guard's whole-check mutant
        // and, jointly, both its operands.
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot("IMG_0700", BUCKET_KEEP, &[("IMG_0700.JPG", 100)], &dest);
        let plan = plan_of(&dest, vec![s1], b1);

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops); // seeds a 100-byte source
        fs.set_cross_fs(true); // force the copy path
        fs.set_free(u64::MAX);
        fs.set_short_copy("/src/IMG_0700.JPG", 60); // silently truncates to 60 of 100 bytes

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");

        let err = apply(&plan, &fs, &jpath).unwrap_err();
        let partial = dest.join(BUCKET_KEEP).join(".IMG_0700.JPG.partial");
        match err {
            ApplyError::Fs { path, source } => {
                assert_eq!(path, partial);
                assert!(
                    source.to_string().contains("short copy: 60 of 100 bytes"),
                    "error should name the short copy: {source}"
                );
            }
            other => panic!("expected Fs error, got {other:?}"),
        }

        // Partial cleaned up; source untouched; nothing published to dest.
        assert!(!fs.exists(&partial), "partial cleaned up, not left behind");
        assert!(
            fs.exists(&PathBuf::from("/src/IMG_0700.JPG")),
            "source intact — never touched until a verified publish"
        );
        assert!(
            !fs.exists(&dest.join(BUCKET_KEEP).join("IMG_0700.JPG")),
            "no final published"
        );
    }

    #[test]
    fn cross_fs_lying_copy_return_is_rejected_independently_of_dest_len() {
        // The dest entry is recorded at its correct/full length here (unlike
        // the sibling test above) — only the reported bytes-copied count
        // lies short. This isolates the `copied != src_len` operand: if it
        // were ever dropped from the guard while `dest_len != src_len`
        // stayed, this exact fault shape would slip through unnoticed.
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot("IMG_0701", BUCKET_KEEP, &[("IMG_0701.JPG", 100)], &dest);
        let plan = plan_of(&dest, vec![s1], b1);

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops); // seeds a 100-byte source
        fs.set_cross_fs(true);
        fs.set_free(u64::MAX);
        fs.set_lying_copy_return("/src/IMG_0701.JPG", 60); // dest recorded at 100; return lies at 60

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");

        let err = apply(&plan, &fs, &jpath).unwrap_err();
        let partial = dest.join(BUCKET_KEEP).join(".IMG_0701.JPG.partial");
        match err {
            ApplyError::Fs { path, source } => {
                assert_eq!(path, partial);
                // dest_len reads back correct (100 of 100) — only `copied`
                // (not rendered in the message) disagrees; the guard still
                // must reject on that operand alone.
                assert!(
                    source.to_string().contains("short copy: 100 of 100 bytes"),
                    "dest_len side of the message reads correct: {source}"
                );
            }
            other => panic!("expected Fs error, got {other:?}"),
        }

        assert!(!fs.exists(&partial), "partial cleaned up, not left behind");
        assert!(
            fs.exists(&PathBuf::from("/src/IMG_0701.JPG")),
            "source intact"
        );
        assert!(
            !fs.exists(&dest.join(BUCKET_KEEP).join("IMG_0701.JPG")),
            "no final published"
        );
    }

    // ---- F3 (Phase 4 follow-up): OpState::Published closes the cross-FS
    // publish→unlink resume window (spec rev 4). ----

    // Extends the scenario in `cross_fs_permission_on_source_remove_surfaces_
    // but_copy_is_published` (kept untouched above): now that the finish
    // stage durably records `Published` (not `Failed`) as the stop record,
    // resuming from it must heal by finishing the unlink WITHOUT re-copying —
    // this is the exact healing the rev-4 finding motivated.
    #[test]
    fn cross_fs_permission_on_source_remove_leaves_published_stop_record_and_resume_heals() {
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot("IMG_0805", BUCKET_KEEP, &[("IMG_0805.JPG", 100)], &dest);
        let plan = plan_of(&dest, vec![s1], b1);

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops);
        fs.set_cross_fs(true);
        fs.set_free(u64::MAX);
        fs.deny_remove("/src/IMG_0805.JPG"); // finish-stage unlink denied

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");

        let err = apply(&plan, &fs, &jpath).unwrap_err();
        assert!(matches!(err, ApplyError::Fs { .. }));

        // The durable stop record is Published, NOT Failed: the dest is
        // already ours, so downgrading to Failed here would erase that fact
        // and strand resume in the duplicate-stop check.
        let j: Journal = serde_json::from_slice(&std::fs::read(&jpath).unwrap()).unwrap();
        assert_eq!(j.statuses, vec![OpState::Published]);

        // Clear the fault and resume: heals by finishing the unlink only —
        // no re-copy, no re-rename. Snapshot the event count first — the
        // initial (crashed) run already logged its own copy/rename, and this
        // assertion is about what the RESUME does, not the whole history.
        let events_before_resume = fs.events().len();
        fs.clear_faults();
        let report = resume(&jpath, &fs).unwrap();
        assert_eq!(report.moved_files, 1);
        assert!(!fs.exists(&PathBuf::from("/src/IMG_0805.JPG")));
        assert_eq!(
            fs.len_of(&dest.join(BUCKET_KEEP).join("IMG_0805.JPG")),
            Some(100)
        );
        assert!(!jpath.exists(), "journal retired once the resume completes");

        let ev = fs.events();
        let resume_ev = &ev[events_before_resume..];
        assert!(
            !resume_ev
                .iter()
                .any(|e| e.starts_with("copy:") || e.starts_with("rename:")),
            "resume must not re-copy or re-rename a Published move: {resume_ev:?}"
        );
    }

    #[test]
    fn resume_completes_a_published_cross_fs_move_without_recopying() {
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot("IMG_0800", BUCKET_KEEP, &[("IMG_0800.JPG", 100)], &dest);
        let plan = plan_of(&dest, vec![s1], b1);

        let fs = FakeFs::new();
        // Crashed exactly in the publish→unlink window: both source and
        // destination are present, and the journal (written raw, as a crash
        // would leave it) already recorded Published.
        fs.seed_file("/src/IMG_0800.JPG", 100);
        fs.seed_file(dest.join(BUCKET_KEEP).join("IMG_0800.JPG"), 100);

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");
        let stale = Journal {
            plan: plan.clone(),
            statuses: vec![OpState::Published],
        };
        std::fs::write(&jpath, serde_json::to_vec(&stale).unwrap()).unwrap();

        let report = resume(&jpath, &fs).unwrap();

        assert_eq!(report.moved_files, 1);
        assert!(
            !fs.exists(&PathBuf::from("/src/IMG_0800.JPG")),
            "source removed to finish the publish"
        );
        assert_eq!(
            fs.len_of(&dest.join(BUCKET_KEEP).join("IMG_0800.JPG")),
            Some(100),
            "dest intact at its seeded length — never re-copied"
        );
        assert!(!jpath.exists(), "journal retired on success");

        // No copy/rename happened this run — only the finish-stage ops.
        let ev = fs.events();
        assert!(
            !ev.iter()
                .any(|e| e.starts_with("copy:") || e.starts_with("rename:")),
            "must not re-copy or re-rename: {ev:?}"
        );
        assert!(ev.iter().any(|e| e.starts_with("fsync_dir:")));
        assert!(ev.iter().any(|e| e.starts_with("remove:")));
    }

    #[test]
    fn published_with_source_already_gone_marks_done_without_touching_disk() {
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot("IMG_0801", BUCKET_KEEP, &[("IMG_0801.JPG", 100)], &dest);
        let plan = plan_of(&dest, vec![s1], b1);

        let fs = FakeFs::new();
        // Source already gone (the crashed run's unlink DID land); dest present.
        fs.seed_file(dest.join(BUCKET_KEEP).join("IMG_0801.JPG"), 100);

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");
        let stale = Journal {
            plan: plan.clone(),
            statuses: vec![OpState::Published],
        };
        std::fs::write(&jpath, serde_json::to_vec(&stale).unwrap()).unwrap();

        let report = resume(&jpath, &fs).unwrap();

        assert_eq!(
            report.moved_files, 0,
            "already-completed work is not recounted"
        );
        assert!(
            fs.events().is_empty(),
            "reconcile marks it Done directly — no disk operation needed"
        );
        assert!(!jpath.exists());
    }

    #[test]
    fn published_with_dest_missing_reexecutes_the_copy_safety_guard() {
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot("IMG_0802", BUCKET_KEEP, &[("IMG_0802.JPG", 100)], &dest);
        let plan = plan_of(&dest, vec![s1], b1);

        let fs = FakeFs::new();
        // Source present, dest MISSING: a Published record vouching for a
        // destination that isn't observably there vouches for nothing.
        fs.seed_file("/src/IMG_0802.JPG", 100);
        fs.set_cross_fs(true); // a Published record only ever originates cross-FS
        fs.set_free(u64::MAX);

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");
        let stale = Journal {
            plan: plan.clone(),
            statuses: vec![OpState::Published],
        };
        std::fs::write(&jpath, serde_json::to_vec(&stale).unwrap()).unwrap();

        let report = resume(&jpath, &fs).unwrap();

        assert_eq!(report.moved_files, 1, "re-executed as a fresh copy");
        assert!(
            !fs.exists(&PathBuf::from("/src/IMG_0802.JPG")),
            "source consumed by the re-executed copy"
        );
        assert_eq!(
            fs.len_of(&dest.join(BUCKET_KEEP).join("IMG_0802.JPG")),
            Some(100),
            "dest recreated"
        );
        assert!(!jpath.exists());
    }

    #[test]
    fn resume_refuses_pending_move_with_both_source_and_dest_present() {
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot("IMG_0803", BUCKET_KEEP, &[("IMG_0803.JPG", 100)], &dest);
        let plan = plan_of(&dest, vec![s1], b1);

        let fs = FakeFs::new();
        fs.seed_file("/src/IMG_0803.JPG", 100);
        let target = dest.join(BUCKET_KEEP).join("IMG_0803.JPG");
        fs.seed_file(target.clone(), 100);

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");
        let stale = Journal {
            plan: plan.clone(),
            statuses: vec![OpState::Pending],
        };
        std::fs::write(&jpath, serde_json::to_vec(&stale).unwrap()).unwrap();

        let err = resume(&jpath, &fs).unwrap_err();
        match err {
            ApplyError::Preflight(msg) => {
                assert!(
                    msg.contains(&target.display().to_string()),
                    "message names the dest path: {msg}"
                );
                assert!(
                    msg.contains("prior run") && msg.contains("verify"),
                    "message directs the user to verify/resolve a possible prior-run copy: {msg}"
                );
            }
            other => panic!("expected ApplyError::Preflight, got {other:?}"),
        }

        assert!(
            fs.events().is_empty(),
            "nothing executed once the duplicate is detected"
        );
        let on_disk: Journal = serde_json::from_slice(&std::fs::read(&jpath).unwrap()).unwrap();
        assert_eq!(
            on_disk.statuses,
            vec![OpState::Pending],
            "journal left untouched on disk"
        );
    }

    #[test]
    fn journal_with_only_pre_published_variant_names_parses() {
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot(
            "IMG_0804",
            BUCKET_KEEP,
            &[
                ("IMG_0804.JPG", 100),
                ("IMG_0804.CR3", 100),
                ("IMG_0804.xmp", 100),
            ],
            &dest,
        );
        let plan = plan_of(&dest, vec![s1], b1); // 3 moves total

        // The exact on-disk shape a pre-rev-4 journal would have: only the
        // three old variant names, never "Published".
        let plan_json = serde_json::to_value(&plan).unwrap();
        let raw = serde_json::json!({
            "plan": plan_json,
            "statuses": ["Pending", "Done", "Failed"],
        });
        let parsed: Journal = serde_json::from_value(raw).unwrap();
        assert_eq!(
            parsed.statuses,
            vec![OpState::Pending, OpState::Done, OpState::Failed]
        );
    }

    // ---- F4 (Phase 4 follow-up): end-of-success durability pass for
    // sidecar publishes (spec §8 rev 4) — "on full success, before the
    // journal is removed, every bucket directory that received a sidecar
    // publish is fsynced". Sidecar writes are real filesystem I/O (never
    // routed through `FsOps`), so — like Task 11 below — these mix `FakeFs`
    // moves with a REAL tempdir for the sidecar target; only the `fsync_dir`
    // call the durability pass makes goes through `FsOps` (and is therefore
    // observable via `fs.fsynced_dirs()`), even though the write itself does not. ----

    #[test]
    fn full_success_fsyncs_sidecar_dirs_before_journal_retirement_deduped() {
        let dest = PathBuf::from("/dst");
        let sidecar_dir = tempfile::tempdir().unwrap(); // REAL tempdir: sidecar parent

        let (mut s1, b1) = shot("IMG_2000", BUCKET_KEEP, &[("IMG_2000.JPG", 100)], &dest);
        s1.write_sidecar = Some(SidecarWrite {
            path: sidecar_dir.path().join("IMG_2000.xmp"),
            tags: vec!["a".into()],
            rating: Some(3),
        });
        // Second shot's sidecar lands in the SAME real dir — pins the dedupe.
        let (mut s2, b2) = shot("IMG_2001", BUCKET_PICKS, &[("IMG_2001.JPG", 100)], &dest);
        s2.write_sidecar = Some(SidecarWrite {
            path: sidecar_dir.path().join("IMG_2001.xmp"),
            tags: vec!["b".into()],
            rating: Some(4),
        });
        let plan = plan_of(&dest, vec![s1, s2], b1 + b2);

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops); // moves stay on the fake /src -> /dst paths

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");

        let report = apply(&plan, &fs, &jpath).unwrap();
        assert_eq!(report.sidecars_written, 2);

        let fsynced = fs.fsynced_dirs();
        let real_dir = sidecar_dir.path().to_path_buf();
        assert_eq!(
            fsynced.iter().filter(|d| **d == real_dir).count(),
            1,
            "sidecar dir fsynced exactly once despite two sidecar publishes in it: {fsynced:?}"
        );
    }

    // Pins the loud-stop half of the contract: a durability-pass fsync
    // failure must propagate as `ApplyError::Fs { path: dir, .. }` and leave
    // the journal ON DISK (not retired) — the run must never read as
    // complete when the sidecar publish it made durable a promise about
    // isn't actually durable yet. Everything up to the fsync itself (the
    // move, the sidecar write) already succeeded; only the trailing
    // dir-fsync fails.
    #[test]
    fn durability_pass_failure_leaves_journal_on_disk_and_stops_loudly() {
        let dest = PathBuf::from("/dst");
        let sidecar_dir = tempfile::tempdir().unwrap();
        let sidecar_path = sidecar_dir.path().join("IMG_2300.xmp");

        let (mut s1, b1) = shot("IMG_2300", BUCKET_KEEP, &[("IMG_2300.JPG", 100)], &dest);
        s1.write_sidecar = Some(SidecarWrite {
            path: sidecar_path.clone(),
            tags: vec!["x".into()],
            rating: Some(3),
        });
        let plan = plan_of(&dest, vec![s1], b1);

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops);
        fs.deny_fsync_dir(sidecar_dir.path()); // the durability pass itself fails

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");

        let err = apply(&plan, &fs, &jpath).unwrap_err();
        match err {
            ApplyError::Fs { path, source } => {
                assert_eq!(path, sidecar_dir.path());
                assert_eq!(source.kind(), io::ErrorKind::PermissionDenied);
            }
            other => panic!("expected ApplyError::Fs, got {other:?}"),
        }

        assert!(
            jpath.exists(),
            "journal must REMAIN after a durability-pass failure — the run \
             is not allowed to read as complete"
        );
        // The move and the sidecar write themselves already landed; only the
        // trailing fsync failed.
        assert!(sidecar_path.exists());
        assert_eq!(
            fs.len_of(&dest.join(BUCKET_KEEP).join("IMG_2300.JPG")),
            Some(100)
        );
    }

    #[test]
    fn full_success_with_no_sidecar_writes_fsyncs_no_dirs() {
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot("IMG_2100", BUCKET_KEEP, &[("IMG_2100.JPG", 100)], &dest);
        let plan = plan_of(&dest, vec![s1], b1); // write_sidecar: None (see `shot`)

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops);

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");

        apply(&plan, &fs, &jpath).unwrap();
        assert!(
            fs.fsynced_dirs().is_empty(),
            "a same-FS-only plan with no sidecar writes must not fsync any dir"
        );
    }

    // Controller-adjudicated semantic flip (Critical fix, F4 durability-pass
    // follow-up): this test used to pin the OPPOSITE — that a pre-existing
    // sidecar target's dir is NOT fsynced. That was the bug: a target already
    // present on disk when `execute` runs is either a foreign file (fsyncing
    // its dir is harmless) OR our own prior-run write that was never fsynced
    // (the durability guarantee is session-scoped, not invocation-scoped — a
    // subsequent `resume` must still make it durable). See
    // `durability_pass_retries_on_resume_after_fsync_failure` below for the
    // resume-retry case this flip exists to cover.
    #[test]
    fn full_success_fsyncs_dir_of_preexisting_sidecar_target_too() {
        let dest = PathBuf::from("/dst");
        let sidecar_dir = tempfile::tempdir().unwrap();
        let sidecar_path = sidecar_dir.path().join("IMG_2200.xmp");
        std::fs::write(&sidecar_path, "SENTINEL").unwrap(); // pre-seeded: write is a skip

        let (mut s1, b1) = shot("IMG_2200", BUCKET_KEEP, &[("IMG_2200.JPG", 100)], &dest);
        s1.write_sidecar = Some(SidecarWrite {
            path: sidecar_path.clone(),
            tags: vec!["x".into()],
            rating: Some(3),
        });
        let plan = plan_of(&dest, vec![s1], b1);

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops);

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");

        let report = apply(&plan, &fs, &jpath).unwrap();
        assert_eq!(report.sidecars_written, 0, "pre-existing sidecar is a skip");
        assert!(
            fs.fsynced_dirs()
                .contains(&sidecar_dir.path().to_path_buf()),
            "a pre-existing sidecar target's dir must STILL be fsynced: either it's a \
             foreign file (harmless, cheap) or our own prior-run write that still owes \
             a durability fsync"
        );
        assert_eq!(
            std::fs::read_to_string(&sidecar_path).unwrap(),
            "SENTINEL",
            "skip must not clobber"
        );
    }

    // Critical finding (F4 durability-pass review): `execute` used to track a
    // sidecar's parent dir ONLY when `write_sidecar` returned `Ok` THIS
    // invocation. If the durability pass's trailing fsync failed (journal
    // correctly left on disk, all moves + the sidecar write itself already
    // Done/landed), a subsequent `resume` would see `sw.path.exists()` and
    // take the skip branch — tracking nothing, fsyncing nothing, then
    // retiring the journal and reporting success even though the sidecar
    // dirent was never made durable. This pins the fix: the durability
    // guarantee is session-scoped, so the skip branch on resume must ALSO
    // track the dir, giving the fsync a real retry.
    #[test]
    fn durability_pass_retries_on_resume_after_fsync_failure() {
        let dest = PathBuf::from("/dst");
        let sidecar_dir = tempfile::tempdir().unwrap(); // REAL tempdir: sidecar parent
        let sidecar_path = sidecar_dir.path().join("IMG_2400.xmp");

        let (mut s1, b1) = shot("IMG_2400", BUCKET_KEEP, &[("IMG_2400.JPG", 100)], &dest);
        s1.write_sidecar = Some(SidecarWrite {
            path: sidecar_path.clone(),
            tags: vec!["x".into()],
            rating: Some(3),
        });
        let plan = plan_of(&dest, vec![s1], b1);

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops);
        fs.deny_fsync_dir(sidecar_dir.path()); // durability pass fails THIS run

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");

        // First run: move + sidecar write land, but the trailing fsync fails.
        let err = apply(&plan, &fs, &jpath).unwrap_err();
        assert!(matches!(err, ApplyError::Fs { .. }));
        assert!(
            jpath.exists(),
            "journal left on disk after the fsync failure"
        );
        assert!(sidecar_path.exists(), "sidecar was actually written");

        // Snapshot fsynced-dirs BEFORE resume: the failed fsync above never
        // actually recorded (FakeFs only records on success), so this is empty.
        let before = fs.fsynced_dirs().len();

        // Fault clears (e.g. the mount that held the sidecar dir is writable
        // again); resume from the same journal.
        fs.clear_faults();
        let report = resume(&jpath, &fs).unwrap();

        // The Critical-path assertion: resume must re-reach the durability
        // pass and fsync the dir, even though the sidecar file itself was
        // already present (and therefore skipped, not rewritten).
        assert_eq!(
            report.sidecars_written, 0,
            "sidecar already present: a skip on resume"
        );
        let after = fs.fsynced_dirs();
        assert_eq!(
            after.len(),
            before + 1,
            "resume must fsync the sidecar dir exactly once more: {after:?}"
        );
        assert!(after.contains(&sidecar_dir.path().to_path_buf()));

        assert!(!jpath.exists(), "journal retired once the resume succeeds");
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
