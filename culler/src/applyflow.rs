//! Apply dialog flow (Task 12, spec §6): gather the plan's I/O-derived inputs
//! so `culler_core::plan::plan` itself stays pure, build the on-screen
//! preview, and run confirm/apply/resume on a worker thread while the UI
//! polls the journal for progress. On success the session sidecar is
//! relocated into `dest` as the audit record (Phase 4 left this to `main`).
//!
//! Module-path note (Task 12 corrections): `culler_core::apply::{apply,
//! resume, ApplyReport, ApplyError}`, `culler_core::plan::{plan, ApplyPlan,
//! TierCountsPlan}`, `culler_core::persist::save`,
//! `culler_core::fsops::{FsOps, RealFs}`,
//! `culler_core::model::{Session, JOURNAL_FILE, SESSION_FILE}`.
//! `find_crashed_apply` / `journal_report` already live in `startup` (Task 11).

use crate::AppWindow;
use crate::startup::{dest_is_source_root, find_crashed_apply, journal_report};
use culler_core::apply::{ApplyError, ApplyReport, apply, resume};
use culler_core::fsops::{FsOps, RealFs};
use culler_core::model::{JOURNAL_FILE, SESSION_FILE, Session};
use culler_core::persist::save;
use culler_core::plan::{ApplyPlan, TierCountsPlan, plan};
use slint::ComponentHandle;
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// Everything the preview shows.
pub struct ApplyPreview {
    pub per_bucket: TierCountsPlan,
    pub collisions: usize,       // ops whose whole stem was auto-suffixed
    pub skipped_sidecars: usize, // pre-existing sidecar carried, tag-write skipped
    pub stale: usize,            // stems that vanished from disk
    pub leftovers: usize,        // unrecognized source files that stay behind
    pub total_bytes: u64,
    pub cross_fs: bool,
    // Kept for API completeness / future use (parity with the plan brief's
    // interface) — the dialog only surfaces the derived `enough_space`
    // boolean today, so nothing in this crate reads the raw byte count yet.
    #[allow(dead_code)]
    pub free_bytes: Option<u64>,
    pub enough_space: bool,
}

/// Gather the plan's I/O-derived inputs so `plan` itself stays pure:
///  - `existing`: bucket-relative paths already under the dest buckets (per-directory collision detection, rev 3)
///  - `sizes`: stem -> total bytes of the shot's files (free-space preflight)
///  - leftover count: source files belonging to no shot (they stay behind)
pub fn gather_apply_inputs(
    session: &Session,
    dest: &Path,
    buckets: &[String; 5],
) -> (BTreeSet<String>, HashMap<String, u64>, usize) {
    // `existing` holds BUCKET-RELATIVE paths ("02_keep/IMG_1.JPG") — rev 3:
    // plan()'s collisions are per target directory, so a name occupied in a
    // different bucket must not force a suffix. A crash-leftover hidden
    // ".name.pid.tmp" file may show up here too (it is a dotfile, so it can
    // never collide with a real target name) — the `to_str` filter below
    // silently tolerates any non-UTF-8 entry rather than panicking.
    let mut existing = BTreeSet::new();
    for b in buckets {
        if let Ok(rd) = std::fs::read_dir(dest.join(b)) {
            for e in rd.flatten() {
                if let Some(name) = e.file_name().to_str() {
                    existing.insert(format!("{b}/{name}"));
                }
            }
        }
    }

    let mut sizes = HashMap::new();
    let mut shot_names: BTreeSet<String> = BTreeSet::new();
    for shot in &session.shots {
        let mut total = 0u64;
        for f in shot.files() {
            if let Ok(md) = std::fs::metadata(&f) {
                total += md.len();
            }
            if let Some(name) = f.file_name().and_then(|n| n.to_str()) {
                shot_names.insert(name.to_string());
            }
        }
        sizes.insert(shot.stem.clone(), total);
    }

    // Leftover counting (Task 12 amendment B, controller-mandated
    // carry-forward): an entry counts as a LEFTOVER when
    //  (1) it is NOT a directory (`file_type`, not `is_file()` — a FIFO,
    //      symlink, socket, etc. also stays behind, not just a regular file),
    //  (2) its name does not start with `.` (checked on the raw encoded
    //      bytes so a non-UTF-8 name is never mis-decoded or panics), and
    //  (3) its name is not one of the shot files' names. A name that is not
    //      valid UTF-8 can never equal a shot name (every `Shot` file name
    //      stored in `shot_names` is, by construction, valid UTF-8), so a
    //      non-UTF-8-named non-dotfile always counts here.
    let mut leftovers = 0usize;
    if let Ok(rd) = std::fs::read_dir(&session.source_dir) {
        for e in rd.flatten() {
            let Ok(file_type) = e.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                continue;
            }
            let name = e.file_name();
            if name.as_encoded_bytes().starts_with(b".") {
                continue; // dotfiles, incl. the session sidecar
            }
            let is_shot_file = name
                .to_str()
                .map(|s| shot_names.contains(s))
                .unwrap_or(false);
            if !is_shot_file {
                leftovers += 1;
            }
        }
    }

    (existing, sizes, leftovers)
}

/// Assemble the preview from a computed plan + gathered facts.
pub fn build_preview(
    planned: &ApplyPlan,
    leftovers: usize,
    cross_fs: bool,
    free_bytes: Option<u64>,
) -> ApplyPreview {
    let collisions = planned.ops.iter().filter(|o| o.suffix.is_some()).count();
    let enough_space = match (cross_fs, free_bytes) {
        (true, Some(free)) => free >= planned.total_bytes,
        _ => true, // same-fs (rename) never needs a space gate
    };
    ApplyPreview {
        per_bucket: planned.per_bucket_counts,
        collisions,
        skipped_sidecars: planned.skipped_sidecar_writes.len(),
        stale: planned.stale.len(),
        leftovers,
        total_bytes: planned.total_bytes,
        cross_fs,
        free_bytes,
        enough_space,
    }
}

/// True when the move crosses filesystems (probes the nearest existing ancestor of dest).
fn probe_cross_fs(fs: &RealFs, source: &Path, dest: &Path) -> bool {
    let mut probe = dest;
    while !probe.exists() {
        match probe.parent() {
            Some(p) => probe = p,
            None => return false,
        }
    }
    fs.same_filesystem(source, probe)
        .map(|same| !same)
        .unwrap_or(false)
}

/// Gather + plan + preview in one step (used by the dialog's "Compute preview").
pub fn compute_preview(
    session: &Session,
    dest: &Path,
    buckets: &[String; 5],
) -> (ApplyPlan, ApplyPreview) {
    let (existing, sizes, leftovers) = gather_apply_inputs(session, dest, buckets);
    let planned = plan(session, dest, buckets, &existing, &sizes);
    let fs = RealFs;
    let cross_fs = probe_cross_fs(&fs, &session.source_dir, dest);
    let free_bytes = if cross_fs {
        fs.free_space(dest).ok()
    } else {
        None
    };
    let preview = build_preview(&planned, leftovers, cross_fs, free_bytes);
    (planned, preview)
}

/// Render a worker-thread `ApplyError` for the UI (Task 12 amendment C).
/// Every string the dialog shows after a failed apply/resume routes through
/// here:
///  - `Preflight` messages are shown VERBATIM (the engine already wrote them
///    for a human — the corrupt-journal refusal and the duplicate-aware
///    Pending+both-present stop) — a short label may be prefixed, but the
///    inner text is never altered.
///  - A `Fs` failure whose path is the journal itself or its `.tmp` publish
///    sibling (`{name}.tmp`, e.g. `.fastcull-apply.json.tmp`) is
///    apply-BOOKKEEPING, not a lost photo — worded so it never reads as a
///    failed move; the plan itself can still be re-run.
///  - Any other `Fs` failure names the path that failed to move.
///  - `Collision` names the path and states nothing was overwritten.
pub fn format_apply_error(e: &ApplyError, journal_path: &Path) -> String {
    match e {
        ApplyError::Preflight(msg) => format!("Apply refused: {msg}"),
        ApplyError::Fs { path, source } => {
            let is_bookkeeping = path == journal_path
                || path
                    .file_name()
                    .map(|n| n.to_string_lossy().starts_with(".fastcull-apply"))
                    .unwrap_or(false);
            if is_bookkeeping {
                format!(
                    "Apply bookkeeping (journal) write failed at {}: {source}. \
                     No photo move failed; the plan can be re-run.",
                    path.display()
                )
            } else {
                format!("Move failed at {}: {source}", path.display())
            }
        }
        ApplyError::Collision(path) => format!(
            "Collision: {} already exists and was not overwritten.",
            path.display()
        ),
    }
}

/// Move the session sidecar into dest as the audit record (Phase 4 left this
/// to main). NO-CLOBBER like every other destination write (rev 3): if dest
/// already holds a `.fastcull.json` (it was a FastCull folder before), the
/// record lands at the first free numbered sibling instead of overwriting
/// history. Always finishes by writing the passed (breadcrumb-cleared)
/// in-memory state and retiring the stale source copy.
fn relocate_session(session: &Session, dest: &Path) -> std::io::Result<()> {
    let from = session.source_dir.join(SESSION_FILE);
    let to = first_free(dest, SESSION_FILE);
    match RealFs.rename_noreplace(&from, &to) {
        Ok(()) => {
            // The moved file still holds pre-apply state (breadcrumb set);
            // atomically rewrite it with the cleared session.
            save(session, &to).map_err(|e| std::io::Error::other(format!("{e:?}")))
        }
        Err(_) => {
            // Cross-FS or source already gone: write the record fresh, then
            // retire any stale source copy (it would resurrect the breadcrumb
            // on the next launch). Our own metadata — not a user-data delete.
            save(session, &to).map_err(|e| std::io::Error::other(format!("{e:?}")))?;
            let _ = std::fs::remove_file(&from);
            Ok(())
        }
    }
}

/// `base`, then `base.1`, `base.2`, … — first name not present in `dir`.
fn first_free(dir: &Path, base: &str) -> PathBuf {
    let candidate = dir.join(base);
    if !candidate.exists() {
        return candidate;
    }
    let mut n = 1u32;
    loop {
        let c = dir.join(format!("{base}.{n}"));
        if !c.exists() {
            return c;
        }
        n += 1;
    }
}

/// Fresh apply: gather -> plan -> journaled apply -> clear breadcrumb ->
/// relocate session record. Takes the session BY VALUE — this runs on a worker
/// thread against a snapshot, never on the UI thread.
pub fn run_apply(
    mut session: Session,
    dest: &Path,
    buckets: &[String; 5],
) -> Result<ApplyReport, String> {
    // C1 fix: `culler_core::apply::apply()` journals into
    // `dest/.fastcull-apply.json` BEFORE the first move (journal-first
    // durability), and `culler_core` only ever creates the BUCKET
    // subdirectories during execution (apply.rs's `execute`) — it never
    // creates `dest` itself. Spec §6 explicitly allows a fresh, not-yet-
    // created subfolder of the source as the destination (the dialog's own
    // hint says so), so `dest` must exist before `apply()` is invoked, or
    // the journal write hits ENOENT on every single attempt. Loud on
    // failure — no silent tolerance, no worker crash.
    RealFs
        .mkdir_p(dest)
        .map_err(|e| format!("Could not create destination {}: {e}", dest.display()))?;

    let (existing, sizes, _leftovers) = gather_apply_inputs(&session, dest, buckets);
    let planned = plan(&session, dest, buckets, &existing, &sizes);
    let fs = RealFs;
    let journal_path = dest.join(JOURNAL_FILE);
    let report =
        apply(&planned, &fs, &journal_path).map_err(|e| format_apply_error(&e, &journal_path))?;
    // Success: the breadcrumb has served its purpose (spec §6 rev 3) — clear it
    // before the session becomes the immutable audit record in dest.
    session.pending_apply = None;
    relocate_session(&session, dest).map_err(|e| format!("session relocation: {e}"))?;
    Ok(report)
}

/// Spec §6 rev 3: the in-flight `dest` must be recorded on `session` and
/// autosaved BEFORE any move, so a crash mid-apply is detectable on the next
/// launch. C2 fix: if the save itself fails (read-only or full source volume
/// — plausible on SD/USB workflows), the apply must not start at all — a
/// move without a landed breadcrumb is an UNDETECTABLE crash waiting to
/// happen. On failure the in-memory `pending_apply` is cleared back to
/// `None` too, so no phantom breadcrumb (set in memory but never persisted)
/// lingers around to mislead anything that inspects `session` afterward.
fn record_breadcrumb(
    session: &mut Session,
    dest: &Path,
    session_file: &Path,
) -> Result<(), String> {
    session.pending_apply = Some(dest.to_path_buf());
    if let Err(e) = save(session, session_file) {
        session.pending_apply = None;
        return Err(format!(
            "Cannot start apply: failed to record the crash-recovery breadcrumb in {}: {e} — \
             the apply was NOT started. Fix the source volume (read-only/full?) and retry.",
            session_file.display()
        ));
    }
    Ok(())
}

fn to_ui_preview(p: &ApplyPreview) -> crate::ApplyPreviewUi {
    crate::ApplyPreviewUi {
        rejected: p.per_bucket.rejected as i32,
        rest: p.per_bucket.rest as i32,
        keep: p.per_bucket.keep as i32,
        picks: p.per_bucket.picks as i32,
        bests: p.per_bucket.bests as i32,
        collisions: p.collisions as i32,
        skipped_sidecars: p.skipped_sidecars as i32,
        stale: p.stale as i32,
        leftovers: p.leftovers as i32,
        total_mb: (p.total_bytes / (1024 * 1024)) as i32,
        cross_fs: p.cross_fs,
        enough_space: p.enough_space,
    }
}

/// Wire the Apply dialog callbacks: dest validation + crash-journal probe,
/// preview, confirm (worker-thread apply-or-resume with breadcrumb + journal
/// polling), completion sink, cancel.
pub fn wire_apply_dialog(
    app: &AppWindow,
    session: Rc<RefCell<Session>>,
    buckets: [String; 5],
    applied: std::rc::Rc<std::cell::Cell<bool>>,
) {
    // Feed the real resolved bucket names to the per-bucket table (index
    // order [rejected, rest, keep, picks, bests]) — the CLI may override
    // names, so the dialog must never hardcode "00_rejected" etc.
    {
        let names: Vec<slint::SharedString> = buckets.iter().map(|b| b.clone().into()).collect();
        app.set_bucket_names(Rc::new(slint::VecModel::from(names)).into());
    }

    // Destination typed: guard dest==source root, probe for a crashed journal.
    {
        let session = session.clone();
        let app_w = app.as_weak();
        app.on_dest_changed(move |dest_str| {
            let Some(app) = app_w.upgrade() else { return };
            let dest = PathBuf::from(dest_str.to_string());
            let source = session.borrow().source_dir.clone();
            app.set_preview_ready(false);
            if dest.as_os_str().is_empty() {
                app.set_dest_error("".into());
                app.set_resume_mode(false);
                return;
            }
            if dest_is_source_root(&source, &dest) {
                app.set_dest_error(
                    "Destination cannot be the source folder itself (a subfolder is fine).".into(),
                );
                app.set_resume_mode(false);
                return;
            }
            if let Some(j) = find_crashed_apply(&dest) {
                let report = journal_report(&j).unwrap_or_default();
                app.set_dest_error(
                    format!("{report}\nConfirm to RESUME this interrupted apply.").into(),
                );
                app.set_resume_mode(true);
            } else {
                app.set_dest_error("".into());
                app.set_resume_mode(false);
            }
        });
    }

    // Compute preview.
    {
        let session = session.clone();
        let buckets = buckets.clone();
        let app_w = app.as_weak();
        app.on_apply_refresh(move || {
            let Some(app) = app_w.upgrade() else { return };
            let dest = PathBuf::from(app.get_dest_path().to_string());
            let source = session.borrow().source_dir.clone();
            if dest.as_os_str().is_empty() || dest_is_source_root(&source, &dest) {
                app.set_preview_ready(false);
                return;
            }
            let (_planned, preview) = compute_preview(&session.borrow(), &dest, &buckets);
            app.set_preview(to_ui_preview(&preview));
            app.set_preview_ready(true);
        });
    }

    // Completion sink — registered on the UI thread, so it may touch Rc state.
    // The worker thread reaches it via `invoke_apply_finished` (Rc/RefCell are
    // not Send; only the AppWindow Weak crosses the thread boundary).
    let progress_timer: Rc<RefCell<Option<slint::Timer>>> = Rc::new(RefCell::new(None));
    {
        let session = session.clone();
        let progress_timer = progress_timer.clone();
        let applied = applied.clone();
        let app_w = app.as_weak();
        app.on_apply_finished(move |ok, msg| {
            let Some(app) = app_w.upgrade() else { return };
            progress_timer.borrow_mut().take(); // dropping the Timer stops the polling
            app.set_apply_running(false);
            app.set_dest_error(msg);
            if ok {
                // Mirror the on-disk clear in the live session.
                session.borrow_mut().pending_apply = None;
                app.set_apply_open(false);
                // M-2: a completed apply is never a resume target — don't
                // stick in resume mode if the dialog is reopened.
                app.set_resume_mode(false);
                // I-1: post-apply, the session record lives in dest; do not
                // let main's autosave timer / exit-flush resurrect the
                // retired source .fastcull.json.
                applied.set(true);
                // Toast (Task 9b, DESIGN §4 2g): shortened per the task brief
                // rather than echoing the full "Applied: N shots, ..." message.
                // tier-keep (code 1) is the closest available "confirm" dot —
                // Toast only supports a leading dot, not 2g's per-message
                // border tinting (kept out to stay a single reusable pill).
                app.invoke_show_toast("✓ apply complete".into(), 1);
            }
        });
    }

    // Confirm: resume any run whose journal is still present (rev 4: presence
    // = incomplete; resuming an all-Done journal just re-runs the owed
    // durability fsyncs and retires it — no moves re-executed, no hijack of a
    // fresh apply, which proceeds after the heal), else fresh apply. Always on
    // a worker thread: a 2k-file move must not freeze the window. Progress =
    // polling the journal the engine rewrites per move.
    {
        let session = session.clone();
        let buckets = buckets.clone();
        let app_w = app.as_weak();
        let progress_timer = progress_timer.clone();
        app.on_apply_confirmed(move || {
            let Some(app) = app_w.upgrade() else { return };
            let dest = PathBuf::from(app.get_dest_path().to_string());
            let source = session.borrow().source_dir.clone();
            if dest_is_source_root(&source, &dest) {
                return;
            }

            // Breadcrumb FIRST (spec §6 rev 3): record + autosave the in-flight
            // dest BEFORE any move, so a crash is detectable on next launch.
            // C2 fix: if that save fails, refuse to start the apply — bail
            // out here, before apply_running/the poll timer/the worker
            // thread are ever touched below.
            {
                let mut s = session.borrow_mut();
                let session_file = source.join(SESSION_FILE);
                if let Err(e) = record_breadcrumb(&mut s, &dest, &session_file) {
                    drop(s);
                    app.set_dest_error(e.into());
                    return;
                }
            }

            // C1 audit: `find_crashed_apply` only returns `Some` when
            // `dest.join(JOURNAL_FILE)` is an existing FILE (startup.rs) —
            // that can only be true if `dest` itself already exists as a
            // directory, so the resume branch below needs no `mkdir_p`
            // (unlike `run_apply`'s fresh-apply path, which must create a
            // not-yet-existing subfolder). Only the `None` arm below ever
            // calls `run_apply`, which does its own dest creation.
            let resume_journal = find_crashed_apply(&dest);
            let snapshot = session.borrow().clone(); // worker-thread copy
            let buckets = buckets.clone();
            let jpath = dest.join(JOURNAL_FILE);
            app.set_apply_running(true);

            // 2c progress screen: poll the journal at 200 ms while the worker runs.
            let timer = slint::Timer::default();
            {
                let app_w = app_w.clone();
                let jp = jpath.clone();
                timer.start(
                    slint::TimerMode::Repeated,
                    std::time::Duration::from_millis(200),
                    move || {
                        if let Some(app) = app_w.upgrade()
                            && let Ok(r) = journal_report(&jp)
                        {
                            app.set_apply_progress(r.into());
                        }
                    },
                );
            }
            *progress_timer.borrow_mut() = Some(timer);

            let app_w2 = app_w.clone();
            let dest2 = dest.clone();
            let jpath2 = jpath.clone();
            std::thread::spawn(move || {
                let result = match resume_journal {
                    Some(j) => resume(&j, &RealFs)
                        .map_err(|e| format_apply_error(&e, &jpath2))
                        .and_then(|r| {
                            // Post-resume housekeeping mirrors run_apply's tail.
                            let mut s = snapshot;
                            s.pending_apply = None;
                            relocate_session(&s, &dest2)
                                .map(|_| r)
                                .map_err(|e| format!("session relocation: {e}"))
                        }),
                    None => run_apply(snapshot, &dest2, &buckets),
                };
                let (ok, msg) = match result {
                    Ok(report) => (
                        true,
                        format!(
                            "Applied: {} shots, {} files moved, {} sidecars written.",
                            report.moved_shots, report.moved_files, report.sidecars_written
                        ),
                    ),
                    // M-5: `e` is already self-descriptive (format_apply_error's
                    // "Apply refused: ...", "Move failed at ...", the
                    // relocation-failure string, etc.) — no extra prefix.
                    Err(e) => (false, e),
                };
                let _ = app_w2.upgrade_in_event_loop(move |app| {
                    app.invoke_apply_finished(ok, msg.into());
                });
            });
        });
    }

    // Cancel.
    {
        let app_w = app.as_weak();
        app.on_apply_cancelled(move || {
            if let Some(a) = app_w.upgrade() {
                a.set_apply_open(false);
            }
        });
    }
}

#[cfg(test)]
mod applyflow_tests {
    use super::*;
    use culler_core::model::{CaptureTime, Decision, Session, Shot, Tier};

    fn mk_shot(stem: &str, dir: &std::path::Path) -> Shot {
        Shot {
            stem: stem.into(),
            jpeg: dir.join(format!("{stem}.JPG")),
            raw: None,
            sidecar: None,
            capture: CaptureTime::default(),
        }
    }

    #[test]
    fn gather_and_preview_counts_leftovers_bytes_and_buckets() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path();
        std::fs::write(src.join("IMG_1.JPG"), b"aaaa").unwrap(); // 4 bytes -> Keep
        std::fs::write(src.join("IMG_2.JPG"), b"bbbbbb").unwrap(); // 6 bytes -> Reject
        std::fs::write(src.join("clip.MOV"), b"zz").unwrap(); // unrecognized -> leftover

        let mut decisions = std::collections::HashMap::new();
        decisions.insert(
            "IMG_1".to_string(),
            Decision {
                tier: Some(Tier::Keep),
                tags: vec![],
                visited: true,
            },
        );
        decisions.insert(
            "IMG_2".to_string(),
            Decision {
                tier: Some(Tier::Reject),
                tags: vec![],
                visited: true,
            },
        );
        let session = Session {
            source_dir: src.to_path_buf(),
            shots: vec![mk_shot("IMG_1", src), mk_shot("IMG_2", src)],
            decisions,
            current: 0,
            pending_apply: None,
            undo: vec![],
        };
        let buckets = crate::startup::default_buckets();
        let dest = src.join("sorted"); // a source subfolder is allowed; not created yet

        let (existing, sizes, leftovers) = gather_apply_inputs(&session, &dest, &buckets);
        assert!(existing.is_empty()); // dest buckets do not exist yet
        assert_eq!(sizes["IMG_1"], 4);
        assert_eq!(sizes["IMG_2"], 6);
        assert_eq!(leftovers, 1); // clip.MOV stays behind

        let planned = plan(&session, &dest, &buckets, &existing, &sizes);
        let preview = build_preview(&planned, leftovers, false, None);
        assert_eq!(preview.per_bucket.keep, 1);
        assert_eq!(preview.per_bucket.rejected, 1);
        assert_eq!(preview.leftovers, 1);
        assert_eq!(preview.total_bytes, 10);
        assert_eq!(preview.collisions, 0);
        assert!(preview.enough_space); // same-fs -> no space gate
    }

    #[test]
    fn build_preview_gates_on_free_space_when_cross_fs() {
        let planned = ApplyPlan {
            dest: "/mnt/other/sorted".into(),
            buckets: crate::startup::default_buckets(),
            ops: vec![],
            per_bucket_counts: TierCountsPlan::default(),
            skipped_sidecar_writes: vec![],
            stale: vec![],
            total_bytes: 1_000,
        };
        let ok = build_preview(&planned, 0, true, Some(2_000));
        assert!(ok.cross_fs && ok.enough_space);
        let tight = build_preview(&planned, 0, true, Some(500));
        assert!(tight.cross_fs && !tight.enough_space); // 500 < 1000
    }

    /// Task 12 amendment B: a source-dir file whose name is not valid UTF-8
    /// can never match a (necessarily UTF-8) shot file name, so it must be
    /// counted as a leftover rather than silently skipped.
    #[test]
    fn gather_apply_inputs_counts_non_utf8_named_file_as_leftover() {
        use std::os::unix::ffi::OsStrExt;

        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path();
        std::fs::write(src.join("IMG_1.JPG"), b"aaaa").unwrap();
        let bad_name = std::ffi::OsStr::from_bytes(b"IMG_\xFF.JPG");
        std::fs::write(src.join(bad_name), b"bad").unwrap();

        let mut decisions = std::collections::HashMap::new();
        decisions.insert(
            "IMG_1".to_string(),
            Decision {
                tier: Some(Tier::Keep),
                tags: vec![],
                visited: true,
            },
        );
        let session = Session {
            source_dir: src.to_path_buf(),
            shots: vec![mk_shot("IMG_1", src)],
            decisions,
            current: 0,
            pending_apply: None,
            undo: vec![],
        };
        let buckets = crate::startup::default_buckets();
        let dest = src.join("sorted");

        let (_existing, _sizes, leftovers) = gather_apply_inputs(&session, &dest, &buckets);
        assert_eq!(
            leftovers, 1,
            "the non-UTF-8-named file must count as a leftover"
        );
    }

    /// C1 (critical defect): apply into a NOT-YET-CREATED destination
    /// subfolder is the primary documented workflow (spec §6: "a fresh
    /// subfolder of the source is allowed"; the dialog's own green hint says
    /// the same). `culler_core::apply::apply()` journals into
    /// `dest/.fastcull-apply.json` BEFORE any move, and `culler_core` only
    /// ever creates the BUCKET dirs during execution — never `dest` itself —
    /// so the journal write hits ENOENT whenever `dest` doesn't exist yet.
    /// `run_apply` must create `dest` first.
    #[test]
    fn run_apply_creates_missing_dest_subfolder() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path();
        std::fs::write(src.join("IMG_1.JPG"), b"aaaa").unwrap(); // -> Keep
        std::fs::write(src.join("IMG_2.JPG"), b"bbbbbb").unwrap(); // -> Reject

        let mut decisions = std::collections::HashMap::new();
        decisions.insert(
            "IMG_1".to_string(),
            Decision {
                tier: Some(Tier::Keep),
                tags: vec![],
                visited: true,
            },
        );
        decisions.insert(
            "IMG_2".to_string(),
            Decision {
                tier: Some(Tier::Reject),
                tags: vec![],
                visited: true,
            },
        );
        let session = Session {
            source_dir: src.to_path_buf(),
            shots: vec![mk_shot("IMG_1", src), mk_shot("IMG_2", src)],
            decisions,
            current: 0,
            pending_apply: None,
            undo: vec![],
        };
        let buckets = crate::startup::default_buckets();
        let dest = src.join("sorted"); // a source subfolder is allowed; NOT created

        assert!(!dest.exists(), "precondition: dest must not exist yet");

        let report = run_apply(session, &dest, &buckets)
            .unwrap_or_else(|e| panic!("run_apply must succeed into a fresh subfolder: {e}"));

        assert_eq!(report.moved_shots, 2);
        assert!(
            dest.join(&buckets[2]).join("IMG_1.JPG").exists(),
            "Keep file landed"
        );
        assert!(
            dest.join(&buckets[0]).join("IMG_2.JPG").exists(),
            "Reject file landed"
        );
        assert!(
            !dest.join(JOURNAL_FILE).exists(),
            "journal removed on success"
        );
        assert!(
            dest.join(SESSION_FILE).exists(),
            "session sidecar relocated into dest"
        );
        assert!(
            !src.join(SESSION_FILE).exists(),
            "stale source sidecar retired"
        );
    }

    /// C1 follow-up: when `dest` truly cannot be created (e.g. a read-only
    /// parent), `run_apply` must fail LOUDLY and name the destination — never
    /// a worker crash, never silent tolerance.
    #[test]
    fn run_apply_uncreatable_dest_errors_loudly() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("IMG_1.JPG"), b"aaaa").unwrap();

        let mut decisions = std::collections::HashMap::new();
        decisions.insert(
            "IMG_1".to_string(),
            Decision {
                tier: Some(Tier::Keep),
                tags: vec![],
                visited: true,
            },
        );
        let session = Session {
            source_dir: src.clone(),
            shots: vec![mk_shot("IMG_1", &src)],
            decisions,
            current: 0,
            pending_apply: None,
            undo: vec![],
        };
        let buckets = crate::startup::default_buckets();

        let parent = tmp.path().join("readonly_parent");
        std::fs::create_dir_all(&parent).unwrap();
        let dest = parent.join("sorted");

        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555)).unwrap();

        // Environment probe: some sandboxes/CI run as root (or under a
        // filesystem that ignores unix perms), where 0o555 never actually
        // blocks a create — in that case this test cannot exercise EACCES,
        // so skip rather than assert on an impossible-to-produce condition.
        let probe = parent.join(".rw_probe");
        let probe_writable = std::fs::write(&probe, b"x").is_ok();
        let _ = std::fs::remove_file(&probe);
        if probe_writable {
            std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
            eprintln!(
                "skipping run_apply_uncreatable_dest_errors_loudly: {} is writable despite 0o555 \
                 (likely running as root) — cannot produce EACCES in this environment",
                parent.display()
            );
            return;
        }

        let result = run_apply(session, &dest, &buckets);

        // Restore perms before any assertion panics / before tempdir drop.
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();

        let err = result.expect_err("dest creation under a read-only parent must fail loudly");
        assert!(
            err.contains("Could not create destination"),
            "error must name the failure: {err}"
        );
        assert!(
            err.contains(&dest.display().to_string()),
            "error must name the dest: {err}"
        );
    }

    /// C2 (critical defect): the crash-detection breadcrumb (`pending_apply`
    /// and its autosave) must land on disk BEFORE any move happens — spec §6
    /// rev 3. `record_breadcrumb` is the pure(ish) core the confirm handler
    /// delegates to, so the save-then-refuse contract is unit-testable
    /// without a Slint event loop.
    #[test]
    fn record_breadcrumb_persists_before_apply() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path();
        let mut session = Session {
            source_dir: src.to_path_buf(),
            ..Session::default()
        };
        let dest = src.join("sorted");
        let session_file = src.join(SESSION_FILE);

        record_breadcrumb(&mut session, &dest, &session_file)
            .expect("breadcrumb save must succeed into a writable tempdir");

        assert_eq!(session.pending_apply, Some(dest.clone()));

        let on_disk = std::fs::read_to_string(&session_file).unwrap();
        assert!(
            on_disk.contains("pending_apply"),
            "sidecar must record the breadcrumb: {on_disk}"
        );
        assert!(
            on_disk.contains(&dest.display().to_string()),
            "sidecar must name the in-flight dest: {on_disk}"
        );
    }

    /// C2 follow-up: when the breadcrumb save itself fails (read-only or
    /// full source volume — plausible on SD/USB workflows), the apply must
    /// be refused, and no phantom breadcrumb may linger in memory once it
    /// never made it to disk.
    #[test]
    fn record_breadcrumb_failure_refuses_and_clears() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();

        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o555)).unwrap();

        // Environment probe (mirrors run_apply_uncreatable_dest_errors_loudly
        // above): some sandboxes/CI run as root, or on a filesystem that
        // ignores unix perms, where 0o555 never actually blocks a write — in
        // that case this test cannot exercise EACCES, so skip rather than
        // assert on an impossible-to-produce condition.
        let probe = src.join(".rw_probe");
        let probe_writable = std::fs::write(&probe, b"x").is_ok();
        let _ = std::fs::remove_file(&probe);
        if probe_writable {
            std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o755)).unwrap();
            eprintln!(
                "skipping record_breadcrumb_failure_refuses_and_clears: {} is writable despite \
                 0o555 (likely running as root) — cannot produce EACCES in this environment",
                src.display()
            );
            return;
        }

        let mut session = Session {
            source_dir: src.clone(),
            ..Session::default()
        };
        let dest = src.join("sorted");
        let session_file = src.join(SESSION_FILE);

        let result = record_breadcrumb(&mut session, &dest, &session_file);

        // Restore perms before any assertion panics / before tempdir drop.
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o755)).unwrap();

        let err = result.expect_err("a failed breadcrumb save must refuse the apply");
        assert!(
            err.contains(&session_file.display().to_string()),
            "error must name the session file: {err}"
        );
        assert_eq!(
            session.pending_apply, None,
            "in-memory breadcrumb must be cleared, not left phantom on a failed save"
        );
        assert!(
            !session_file.exists(),
            "no sidecar should land on disk when the save fails"
        );
    }
}

#[cfg(test)]
mod format_apply_error_tests {
    use super::*;

    #[test]
    fn preflight_message_appears_verbatim() {
        let journal_path = Path::new("/dest/.fastcull-apply.json");
        let distinctive = "zebra hallway umbrella distinctive phrase 12345";
        let err = ApplyError::Preflight(distinctive.to_string());
        let msg = format_apply_error(&err, journal_path);
        assert!(msg.contains(distinctive), "message was: {msg}");
    }

    #[test]
    fn journal_and_tmp_sibling_fs_errors_are_bookkeeping_not_move_failed() {
        let journal_path = Path::new("/dest/.fastcull-apply.json");
        let tmp_sibling = Path::new("/dest/.fastcull-apply.json.tmp");

        let e1 = ApplyError::Fs {
            path: journal_path.to_path_buf(),
            source: std::io::Error::other("disk full"),
        };
        let m1 = format_apply_error(&e1, journal_path);
        assert!(m1.contains("bookkeeping"), "message was: {m1}");
        assert!(!m1.contains("Move failed"), "message was: {m1}");

        let e2 = ApplyError::Fs {
            path: tmp_sibling.to_path_buf(),
            source: std::io::Error::other("disk full"),
        };
        let m2 = format_apply_error(&e2, journal_path);
        assert!(m2.contains("bookkeeping"), "message was: {m2}");
        assert!(!m2.contains("Move failed"), "message was: {m2}");
    }

    #[test]
    fn photo_path_fs_error_says_move_failed() {
        let journal_path = Path::new("/dest/.fastcull-apply.json");
        let photo_path = Path::new("/dest/02_keep/IMG_0001.JPG");
        let e = ApplyError::Fs {
            path: photo_path.to_path_buf(),
            source: std::io::Error::other("EACCES"),
        };
        let msg = format_apply_error(&e, journal_path);
        assert!(msg.contains("Move failed at"), "message was: {msg}");
    }

    #[test]
    fn collision_names_path_and_says_not_overwritten() {
        let journal_path = Path::new("/dest/.fastcull-apply.json");
        let collided = PathBuf::from("/dest/02_keep/IMG_0002.JPG");
        let e = ApplyError::Collision(collided.clone());
        let msg = format_apply_error(&e, journal_path);
        assert!(
            msg.contains(&collided.display().to_string()),
            "message was: {msg}"
        );
        assert!(msg.contains("not overwritten"), "message was: {msg}");
    }
}
