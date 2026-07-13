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
use crate::startup::{dest_is_source_root, find_crashed_apply, journal_counts, journal_report};
use culler_core::apply::{ApplyError, ApplyReport, apply, resume};
use culler_core::fsops::{FsOps, RealFs};
use culler_core::model::{JOURNAL_FILE, SESSION_FILE, Session};
use culler_core::persist::save;
use culler_core::plan::{ApplyPlan, TierCountsPlan, plan};
use slint::ComponentHandle;
use std::cell::{Cell, RefCell};
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

/// C3 fix: `culler_core::plan::plan` is pure I/O-free code — its own doc
/// contract (plan.rs) and spec §10 both say the BINARY existence-checks
/// shots, drops the missing ones before calling `plan`, and reports them as
/// stale. A shot's files are `shot.files()` (jpeg + raw? + sidecar?); if ANY
/// sibling has vanished from disk since scan, the WHOLE shot is dropped — a
/// partial move (e.g. jpeg present, raw gone) would be worse than skipping
/// it outright. Returns a session copy whose `shots` keep only the survivors
/// (the `decisions` map is left untouched — `plan` keys off `shots`, not
/// `decisions` — and `current` is left as-is too: it is an index into the UI
/// filmstrip, irrelevant to planning) plus the SORTED stale stems.
fn partition_stale(session: &Session) -> (Session, Vec<String>) {
    let mut kept = Vec::with_capacity(session.shots.len());
    let mut stale = Vec::new();
    for shot in &session.shots {
        if shot.files().iter().all(|f| f.exists()) {
            kept.push(shot.clone());
        } else {
            stale.push(shot.stem.clone());
        }
    }
    stale.sort();
    let mut filtered = session.clone();
    filtered.shots = kept;
    (filtered, stale)
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
///
/// `stale` is the caller's pre-verification count (C3 fix), NOT
/// `planned.stale.len()`: `plan()` never fills its own `stale` field (it
/// does no I/O — see plan.rs's doc contract), so by the time `planned` gets
/// here it was already built from a session `partition_stale` filtered; the
/// stale count has to be threaded through separately.
pub fn build_preview(
    planned: &ApplyPlan,
    stale: usize,
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
        stale,
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

/// Gather + plan + preview in one step (used by the dialog's "Compute
/// preview"). C3 fix: pre-verifies shot existence first (plan.rs's doc
/// contract / spec §10) — a shot whose files vanished from disk since scan
/// is dropped before `gather`/`plan` ever see it, and its stem is counted in
/// the preview's `stale`.
pub fn compute_preview(
    session: &Session,
    dest: &Path,
    buckets: &[String; 5],
) -> (ApplyPlan, ApplyPreview) {
    let (filtered, stale) = partition_stale(session);
    let (existing, sizes, leftovers) = gather_apply_inputs(&filtered, dest, buckets);
    let planned = plan(&filtered, dest, buckets, &existing, &sizes);
    let fs = RealFs;
    let cross_fs = probe_cross_fs(&fs, &filtered.source_dir, dest);
    let free_bytes = if cross_fs {
        fs.free_space(dest).ok()
    } else {
        None
    };
    let preview = build_preview(&planned, stale.len(), leftovers, cross_fs, free_bytes);
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

/// I3 fix: post-apply tail, extracted so relocation's failure mode is
/// unit-testable in isolation from a real `apply()`/`resume()` call. By the
/// time this runs the photos are ALREADY moved (and the journal already
/// retired) — `relocate_session` failing here (e.g. dest going read-only, or
/// filling up, between the last move and this call) is an audit-copy
/// problem, NOT a lost photo, so it must never be reported as an apply
/// failure. It downgrades to a warning attached to the (otherwise unchanged)
/// success report; the caller must still surface `ok = true`.
fn finish_apply(
    report: ApplyReport,
    session: Session,
    dest: &Path,
) -> (ApplyReport, Option<String>) {
    match relocate_session(&session, dest) {
        Ok(()) => (report, None),
        Err(e) => (
            report,
            Some(format!(
                "Photos moved successfully, but the session audit copy could not be written to \
                 the destination: {e}. Your photos are safe in the destination; only the \
                 .fastcull.json record is missing."
            )),
        ),
    }
}

/// Fresh apply: pre-verify -> gather -> plan -> journaled apply -> clear
/// breadcrumb -> relocate session record. Takes the session BY VALUE — this
/// runs on a worker thread against a snapshot, never on the UI thread.
/// I3 fix: the `Option<String>` alongside the success `ApplyReport` carries a
/// relocation-failure WARNING — the apply itself succeeded (photos moved,
/// journal retired) even when the session's audit copy could not be written
/// to `dest`; only `apply()` failing before that point is a real `Err`.
pub fn run_apply(
    mut session: Session,
    dest: &Path,
    buckets: &[String; 5],
) -> Result<(ApplyReport, Option<String>), String> {
    // C3 fix: re-verify existence AGAIN here, on the confirm-time snapshot —
    // a file can vanish between the dialog's preview and this call. Bail out
    // BEFORE touching disk (no `mkdir_p`, no journal) when every shot is
    // stale: applying an empty plan would still create an empty `dest` and a
    // journal for zero ops, which is pointless, and a shot whose files
    // vanished must never reach `plan`/`apply` (its encoded moves would ENOENT
    // and, worse, jam `resume` on that same ENOENT forever).
    let (filtered, stale) = partition_stale(&session);
    if !session.shots.is_empty() && filtered.shots.is_empty() {
        return Err(format!(
            "Apply refused: nothing left to apply, {} shot{} stale (source files vanished since scan).",
            stale.len(),
            if stale.len() == 1 { "" } else { "s" }
        ));
    }

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

    let (existing, sizes, _leftovers) = gather_apply_inputs(&filtered, dest, buckets);
    let planned = plan(&filtered, dest, buckets, &existing, &sizes);
    let fs = RealFs;
    let journal_path = dest.join(JOURNAL_FILE);
    let report =
        apply(&planned, &fs, &journal_path).map_err(|e| format_apply_error(&e, &journal_path))?;
    // Success: the breadcrumb has served its purpose (spec §6 rev 3) — clear it
    // before the session becomes the immutable audit record in dest.
    session.pending_apply = None;
    Ok(finish_apply(report, session, dest))
}

/// Shared resume executor (Task A2). Runs ON A WORKER THREAD against a
/// session SNAPSHOT — BOTH the Apply dialog's resume arm and the future 2d
/// startup-recovery panel call this, so the healing logic exists exactly
/// once. `resume()` reconciles + completes the interrupted moves and retires
/// the journal on success. `relocate` controls the post-resume session-record
/// handling:
///  - true  — a breadcrumb-detected crash: THIS shoot's own apply is
///    completing, so clear the breadcrumb and relocate the session sidecar
///    into `dest` as the audit record (via `finish_apply`); a relocation
///    failure is a warning.
///  - false — a source-dir-probe crash (the source folder was itself a prior
///    run's DESTINATION): the current session is NOT that apply, so do NOT
///    relocate or retire it — just heal the journal and return.
pub fn resume_on_worker(
    journal: &Path,
    dest: &Path,
    session: Session,
    relocate: bool,
    fs: &dyn FsOps,
) -> Result<(ApplyReport, Option<String>), String> {
    let report = resume(journal, fs).map_err(|e| format_apply_error(&e, journal))?;
    if relocate {
        let mut s = session;
        s.pending_apply = None;
        Ok(finish_apply(report, s, dest))
    } else {
        Ok((report, None))
    }
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
            std::thread::spawn(move || {
                // A2: the dialog path is ALWAYS `relocate: true` — a
                // breadcrumb was recorded by `record_breadcrumb` before this
                // point, so `snapshot.pending_apply` is `Some(dest)` and THIS
                // shoot's own apply is the one completing. The old inlined
                // arm passed `jpath2` (== `dest2.join(JOURNAL_FILE)` == `j`)
                // to `format_apply_error`; `resume_on_worker` uses its
                // `journal` parameter (`j`) for the same call, so the
                // rendered error string is unchanged.
                let result = match resume_journal {
                    Some(j) => resume_on_worker(&j, &dest2, snapshot, true, &RealFs),
                    None => run_apply(snapshot, &dest2, &buckets),
                };
                let (ok, msg) = match result {
                    Ok((report, warning)) => {
                        let success = format!(
                            "Applied: {} shots, {} files moved, {} sidecars written.",
                            report.moved_shots, report.moved_files, report.sidecars_written
                        );
                        let msg = match warning {
                            // I3 fix: photos are safe and moved — say so
                            // alongside the normal success summary rather
                            // than failing the whole apply.
                            Some(w) => format!("{success}\n\nWarning: {w}"),
                            None => success,
                        };
                        (true, msg)
                    }
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

/// Success-message formatting for a completed crash-recovery resume
/// (`wire_crash_recovery`'s worker thread) — extracted from that closure so
/// the completion-branch decision it feeds is unit-testable without a Slint
/// event loop, mirroring `wire_apply_dialog`'s inline success-string build
/// but worded "Recovered" (2d) rather than "Applied" (2b/2c).
pub fn crash_success_message(report: &ApplyReport, warning: &Option<String>) -> String {
    let success = format!(
        "Recovered: {} shots, {} files moved, {} sidecars written.",
        report.moved_shots, report.moved_files, report.sidecars_written
    );
    match warning {
        // I3 fix (mirrors run_apply's arm): a relocation-failure warning is
        // appended, never turned into a failure — the moves already happened.
        Some(w) => format!("{success}\n\nWarning: {w}"),
        None => success,
    }
}

/// Wire the 2d crash-recovery startup screen (Task A3, DESIGN.md §4 2d):
/// show-report / resume / completion. Resume reuses `resume_on_worker` (A2)
/// — the SAME shared executor the Apply dialog's resume arm calls — so the
/// healing logic exists exactly once; this function only adds the 200ms
/// journal-count poll (a thin progress reader) and the completion routing
/// specific to the startup screen (drop into culling vs. stay open on
/// failure, breadcrumb-clear + `applied` guard only when the crash was OUR
/// OWN apply).
pub fn wire_crash_recovery(
    app: &AppWindow,
    session: Rc<RefCell<Session>>,
    applied: Rc<Cell<bool>>,
    crash: crate::startup::CrashInfo,
) {
    // Show report: render `journal_report` on demand (empty until clicked).
    {
        let crash = crash.clone();
        let app_w = app.as_weak();
        app.on_crash_show_report(move || {
            let Some(app) = app_w.upgrade() else { return };
            let report = journal_report(&crash.journal)
                .unwrap_or_else(|e| format!("could not read journal: {e}"));
            app.set_crash_report(report.into());
            app.set_crash_report_open(true);
        });
    }

    // Poll-timer holder, exactly like `wire_apply_dialog`'s `progress_timer`
    // — owned here so the completion sink can drop it (stopping the poll)
    // and the resume handler can (re)start it.
    let poll_timer: Rc<RefCell<Option<slint::Timer>>> = Rc::new(RefCell::new(None));

    // Completion sink — registered on the UI thread, so it may touch Rc
    // state. The worker thread reaches it via `invoke_crash_finished` (Rc/
    // RefCell are not Send; only the AppWindow Weak crosses the thread
    // boundary).
    {
        let session = session.clone();
        let applied = applied.clone();
        let poll_timer = poll_timer.clone();
        let crash = crash.clone();
        let app_w = app.as_weak();
        app.on_crash_finished(move |ok, msg| {
            let Some(app) = app_w.upgrade() else { return };
            poll_timer.borrow_mut().take(); // dropping the Timer stops the polling
            app.set_crash_resuming(false);
            if ok {
                app.set_crash_open(false); // drop into culling
                if crash.from_breadcrumb {
                    // This shoot's own apply just completed: mirror the
                    // on-disk breadcrumb clear in the live session, and — I-1
                    // style — do not let main's autosave timer / exit-flush
                    // resurrect the now-retired source `.fastcull.json`; the
                    // audit record lives in `dest`.
                    session.borrow_mut().pending_apply = None;
                    applied.set(true);
                }
                // NOT from_breadcrumb: a source-dir-probe crash (source was a
                // prior run's DESTINATION) — the current session is not that
                // apply, so `applied` stays false and the user keeps culling
                // the source normally. Toast either way.
                app.invoke_show_toast("✓ recovery complete".into(), 1);
            } else {
                // Keep the panel open so the user sees why it failed and can
                // retry (Resume apply again) or quit.
                app.set_crash_report(msg);
                app.set_crash_report_open(true);
            }
        });
    }

    // Resume: worker-thread `resume_on_worker` call + 200ms journal-count
    // poll for the gold progress bar.
    {
        let session = session.clone();
        let poll_timer = poll_timer.clone();
        let crash = crash.clone();
        let app_w = app.as_weak();
        app.on_crash_resume(move || {
            let Some(app) = app_w.upgrade() else { return };
            app.set_crash_resuming(true);

            let timer = slint::Timer::default();
            {
                let app_w = app_w.clone();
                let journal = crash.journal.clone();
                timer.start(
                    slint::TimerMode::Repeated,
                    std::time::Duration::from_millis(200),
                    move || {
                        // Ignore Err: the journal is retired on success, so a
                        // read failure near completion just means "done" —
                        // the completion hop (not this poll) is what closes
                        // the panel.
                        if let Some(app) = app_w.upgrade()
                            && let Ok((d, t)) = journal_counts(&journal)
                        {
                            app.set_crash_done(d as i32);
                            app.set_crash_total(t as i32);
                        }
                    },
                );
            }
            *poll_timer.borrow_mut() = Some(timer);

            let snapshot = session.borrow().clone();
            let app_w2 = app_w.clone();
            let crash2 = crash.clone();
            std::thread::spawn(move || {
                // A2 shared executor — do NOT re-inline resume/finish_apply.
                let result = resume_on_worker(
                    &crash2.journal,
                    &crash2.dest,
                    snapshot,
                    crash2.from_breadcrumb,
                    &RealFs,
                );
                let (ok, msg) = match result {
                    Ok((report, warning)) => (true, crash_success_message(&report, &warning)),
                    // The Err(String) is already self-descriptive
                    // (format_apply_error's wording) — shown verbatim.
                    Err(e) => (false, e),
                };
                let _ = app_w2.upgrade_in_event_loop(move |app| {
                    app.invoke_crash_finished(ok, msg.into());
                });
            });
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
            jpeg: Some(dir.join(format!("{stem}.JPG"))),
            raw: None,
            sidecar: None,
            capture: CaptureTime::default(),
            exif: None,
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
        let preview = build_preview(&planned, 0, leftovers, false, None);
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
        let ok = build_preview(&planned, 0, 0, true, Some(2_000));
        assert!(ok.cross_fs && ok.enough_space);
        let tight = build_preview(&planned, 0, 0, true, Some(500));
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

        let (report, warning) = run_apply(session, &dest, &buckets)
            .unwrap_or_else(|e| panic!("run_apply must succeed into a fresh subfolder: {e}"));

        assert!(
            warning.is_none(),
            "a writable dest must relocate cleanly with no warning: {warning:?}"
        );
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

    /// C3 (critical defect): a shot whose JPEG vanished from disk after scan
    /// (but is still listed in the session) must be dropped BEFORE `plan`
    /// ever sees it, and counted as stale in the preview — per plan.rs's own
    /// doc contract and spec §10.
    #[test]
    fn preview_reports_vanished_shot_as_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path();
        std::fs::write(src.join("IMG_1.JPG"), b"aaaa").unwrap(); // survives -> Keep
        std::fs::write(src.join("IMG_2.JPG"), b"bbbbbb").unwrap(); // will vanish -> Reject

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
        let dest = src.join("sorted");

        // The JPEG vanishes AFTER the session/shot list was built (e.g.
        // deleted out-of-band between scan and apply) — exactly the gap
        // plan.rs's doc contract requires the BINARY to close.
        std::fs::remove_file(src.join("IMG_2.JPG")).unwrap();

        let (planned, preview) = compute_preview(&session, &dest, &buckets);

        assert_eq!(preview.stale, 1, "vanished shot must be counted as stale");
        assert!(
            planned.ops.iter().all(|o| o.stem != "IMG_2"),
            "the stale stem must not appear in the plan's ops: {:?}",
            planned.ops.iter().map(|o| &o.stem).collect::<Vec<_>>()
        );
        assert!(
            planned.ops.iter().any(|o| o.stem == "IMG_1"),
            "the surviving shot must still be planned"
        );
    }

    /// C3 follow-up: `run_apply` must exclude a vanished shot from the
    /// actual moves and still complete cleanly for the survivors.
    #[test]
    fn run_apply_excludes_vanished_shots_and_completes() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path();
        std::fs::write(src.join("IMG_1.JPG"), b"aaaa").unwrap(); // survives -> Keep
        std::fs::write(src.join("IMG_2.JPG"), b"bbbbbb").unwrap(); // will vanish -> Reject

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
        let dest = src.join("sorted");

        std::fs::remove_file(src.join("IMG_2.JPG")).unwrap();

        let (report, warning) = run_apply(session, &dest, &buckets)
            .unwrap_or_else(|e| panic!("run_apply must succeed excluding the stale shot: {e}"));

        assert!(
            warning.is_none(),
            "a writable dest must relocate cleanly with no warning: {warning:?}"
        );
        assert_eq!(report.moved_shots, 1, "only the surviving shot is moved");
        assert!(
            dest.join(&buckets[2]).join("IMG_1.JPG").exists(),
            "surviving Keep file landed"
        );
        for bucket in &buckets {
            assert!(
                !dest.join(bucket).join("IMG_2.JPG").exists(),
                "the vanished stem must not appear in any bucket"
            );
        }
        assert!(
            !dest.join(JOURNAL_FILE).exists(),
            "journal removed on clean completion"
        );
    }

    /// C3 follow-up: if EVERY shot has vanished by confirm time, applying an
    /// empty plan would silently "succeed" while doing nothing useful and
    /// leaving a pointless empty `dest` behind — `run_apply` must fail
    /// loudly instead, and touch no disk state at all (no journal, no dest).
    #[test]
    fn run_apply_all_stale_fails_loudly() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path();
        std::fs::write(src.join("IMG_1.JPG"), b"aaaa").unwrap();
        std::fs::write(src.join("IMG_2.JPG"), b"bbbbbb").unwrap();

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
        let dest = src.join("sorted");

        std::fs::remove_file(src.join("IMG_1.JPG")).unwrap();
        std::fs::remove_file(src.join("IMG_2.JPG")).unwrap();

        let err = run_apply(session, &dest, &buckets).expect_err(
            "an apply where every shot vanished must fail loudly, not apply an empty plan",
        );
        let lower = err.to_lowercase();
        assert!(
            lower.contains("stale") || lower.contains("nothing"),
            "error must mention stale/nothing to apply: {err}"
        );
        assert!(
            !dest.exists(),
            "no partial destination state must be created: {}",
            dest.display()
        );
    }

    /// C3 follow-up: `partition_stale` treats a missing RAW sibling the same
    /// as a missing JPEG — a shot's files are jpeg + raw? + sidecar?, and a
    /// missing sibling makes the WHOLE shot stale (a partial move would be
    /// worse than dropping it).
    #[test]
    fn partition_stale_missing_raw_sibling_marks_shot_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path();
        std::fs::write(src.join("IMG_1.JPG"), b"aaaa").unwrap();

        let mut shot = mk_shot("IMG_1", src);
        shot.raw = Some(src.join("IMG_1.CR3")); // never written -> missing sibling

        let session = Session {
            source_dir: src.to_path_buf(),
            shots: vec![shot],
            decisions: std::collections::HashMap::new(),
            current: 0,
            pending_apply: None,
            undo: vec![],
        };

        let (filtered, stale) = partition_stale(&session);

        assert!(
            filtered.shots.is_empty(),
            "a shot with any missing sibling file must be dropped"
        );
        assert_eq!(stale, vec!["IMG_1".to_string()]);
    }

    /// I3 (defect): after a fully successful apply (photos moved, journal
    /// retired), relocating the session sidecar into `dest` is an AUDIT
    /// COPY, not the photos themselves. `finish_apply` is the extracted
    /// post-apply tail; when `dest` is writable, relocation must succeed
    /// silently — no warning — and the sidecar must land in `dest`.
    #[test]
    fn finish_apply_relocates_session_when_dest_writable() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dest).unwrap();

        let session = Session {
            source_dir: src.clone(),
            ..Session::default()
        };
        // Pre-apply sidecar, as a real apply would have left behind.
        save(&session, &src.join(SESSION_FILE)).unwrap();

        let report = ApplyReport {
            moved_shots: 2,
            moved_files: 3,
            sidecars_written: 1,
            stopped_at: None,
        };

        let (out_report, warning) = finish_apply(report.clone(), session, &dest);

        assert_eq!(
            out_report, report,
            "a clean relocation must not alter the ApplyReport"
        );
        assert!(warning.is_none(), "warning was: {warning:?}");
        assert!(
            dest.join(SESSION_FILE).exists(),
            ".fastcull.json must be relocated into dest"
        );
    }

    /// A2 shared fixture: the "all-Done heal" scenario `resume_on_worker`
    /// tests exercise on a real FS (`RealFs`) — a completed move whose
    /// journal is still present. `IMG_1.JPG` already sits at
    /// `dest/02_keep/IMG_1.JPG` (source consumed, so `src/IMG_1.JPG` is never
    /// created); a one-op `Journal` with `OpState::Done` records that move;
    /// a pre-apply session sidecar sits at `src/.fastcull.json`. `resume()`
    /// reconciles the Done op (dest present, src absent -> stays done), runs
    /// the durability pass, and retires the journal — returns `(src, dest,
    /// journal_path, session)` for the caller to drive `resume_on_worker`.
    fn mk_resume_fixture(tmp: &std::path::Path) -> (PathBuf, PathBuf, PathBuf, Session) {
        let src = tmp.join("src");
        let dest = tmp.join("dest");
        let bucket_dir = dest.join("02_keep");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&bucket_dir).unwrap();

        // The move already happened: dest file present, source consumed.
        std::fs::write(bucket_dir.join("IMG_1.JPG"), b"aaaa").unwrap();

        let plan = ApplyPlan {
            dest: dest.clone(),
            buckets: crate::startup::default_buckets(),
            ops: vec![culler_core::plan::ShotOp {
                stem: "IMG_1".into(),
                bucket: "02_keep".into(),
                moves: vec![culler_core::plan::FileMove {
                    from: src.join("IMG_1.JPG"),
                    to: bucket_dir.join("IMG_1.JPG"),
                }],
                write_sidecar: None,
                suffix: None,
            }],
            per_bucket_counts: TierCountsPlan::default(),
            skipped_sidecar_writes: vec![],
            stale: vec![],
            total_bytes: 4,
        };
        let journal = culler_core::apply::Journal {
            plan,
            statuses: vec![culler_core::apply::OpState::Done],
        };
        let journal_path = dest.join(JOURNAL_FILE);
        std::fs::write(&journal_path, serde_json::to_vec(&journal).unwrap()).unwrap();

        let session = Session {
            source_dir: src.clone(),
            ..Session::default()
        };
        save(&session, &src.join(SESSION_FILE)).unwrap();

        (src, dest, journal_path, session)
    }

    /// A2: `relocate=true` mirrors the Apply-dialog resume arm (a
    /// breadcrumb-detected crash for THIS shoot's own apply) — the session
    /// record must be relocated into `dest` as the audit record, and the
    /// stale source copy retired, on top of the journal being healed/retired.
    #[test]
    fn resume_on_worker_relocate_true_relocates_session() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dest, journal_path, session) = mk_resume_fixture(tmp.path());

        let (report, warning) =
            resume_on_worker(&journal_path, &dest, session, true, &RealFs).unwrap();
        let _ = report;

        assert!(warning.is_none(), "warning was: {warning:?}");
        assert!(!journal_path.exists(), "journal must be retired");
        assert!(
            dest.join(SESSION_FILE).exists(),
            "session sidecar must be relocated into dest"
        );
        assert!(
            !src.join(SESSION_FILE).exists(),
            "stale source sidecar copy must be retired"
        );
    }

    /// A2: `relocate=false` mirrors a source-dir-probe crash (the source
    /// folder was itself a prior run's DESTINATION) — the current session is
    /// NOT that apply, so `resume_on_worker` must heal the journal only and
    /// leave the session record entirely untouched.
    #[test]
    fn resume_on_worker_relocate_false_leaves_session() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dest, journal_path, session) = mk_resume_fixture(tmp.path());

        let (report, warning) =
            resume_on_worker(&journal_path, &dest, session, false, &RealFs).unwrap();
        let _ = report;

        assert!(warning.is_none(), "warning was: {warning:?}");
        assert!(!journal_path.exists(), "journal must be retired");
        assert!(
            !dest.join(SESSION_FILE).exists(),
            "session sidecar must NOT be relocated when relocate=false"
        );
        assert!(
            src.join(SESSION_FILE).exists(),
            "source sidecar must remain untouched when relocate=false"
        );
    }

    /// I3 follow-up: when relocation FAILS after a successful apply (dest
    /// goes read-only between the last move and the sidecar write —
    /// plausible on flaky removable media), `finish_apply` must downgrade
    /// that into a WARNING, never an error — the photos already moved. The
    /// returned `ApplyReport` must be byte-for-byte unchanged.
    #[test]
    fn finish_apply_downgrades_relocation_failure_to_warning() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dest).unwrap();

        let session = Session {
            source_dir: src.clone(),
            ..Session::default()
        };
        save(&session, &src.join(SESSION_FILE)).unwrap();

        // Photos are already "staged" in dest (mirrors a completed apply)
        // BEFORE dest goes read-only — the failure under test is the
        // relocation step, not the move itself.
        std::fs::write(dest.join("IMG_1.JPG"), b"staged").unwrap();
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o555)).unwrap();

        // Environment probe (mirrors run_apply_uncreatable_dest_errors_loudly
        // / record_breadcrumb_failure_refuses_and_clears above): some
        // sandboxes/CI run as root, or on a filesystem that ignores unix
        // perms, where 0o555 never actually blocks a write — in that case
        // this test cannot exercise EACCES, so skip rather than assert on an
        // impossible-to-produce condition.
        let probe = dest.join(".rw_probe");
        let probe_writable = std::fs::write(&probe, b"x").is_ok();
        let _ = std::fs::remove_file(&probe);
        if probe_writable {
            std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).unwrap();
            eprintln!(
                "skipping finish_apply_downgrades_relocation_failure_to_warning: {} is writable \
                 despite 0o555 (likely running as root) — cannot produce EACCES in this \
                 environment",
                dest.display()
            );
            return;
        }

        let report = ApplyReport {
            moved_shots: 2,
            moved_files: 3,
            sidecars_written: 1,
            stopped_at: None,
        };

        let (out_report, warning) = finish_apply(report.clone(), session, &dest);

        // Restore perms before any assertion panics / before tempdir drop.
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(
            out_report, report,
            "a relocation failure must NOT alter the (already-successful) ApplyReport"
        );
        let warning =
            warning.expect("a relocation failure after a successful apply must produce a warning");
        assert!(
            warning.contains("audit") || warning.contains("safe"),
            "warning must explain the photos are safe / name the audit copy: {warning}"
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

/// Task A3: the one integration-adjacent piece of `wire_crash_recovery` that
/// is testable without a Slint event loop — the success-message formatting
/// fed to `crash-finished`/the "✓ recovery complete" toast.
#[cfg(test)]
mod crash_success_message_tests {
    use super::*;

    fn report(moved_shots: usize, moved_files: usize, sidecars_written: usize) -> ApplyReport {
        ApplyReport {
            moved_shots,
            moved_files,
            sidecars_written,
            stopped_at: None,
        }
    }

    #[test]
    fn formats_recovered_shape_with_no_warning() {
        let msg = crash_success_message(&report(3, 5, 2), &None);
        assert_eq!(
            msg,
            "Recovered: 3 shots, 5 files moved, 2 sidecars written."
        );
    }

    #[test]
    fn appends_warning_when_present() {
        let msg = crash_success_message(&report(1, 1, 1), &Some("dest went read-only".into()));
        assert!(
            msg.starts_with("Recovered: 1 shots, 1 files moved, 1 sidecars written."),
            "message was: {msg}"
        );
        assert!(
            msg.contains("Warning: dest went read-only"),
            "message was: {msg}"
        );
    }
}
