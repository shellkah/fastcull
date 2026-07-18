//! CLI parsing, startup scan/reattach, dest==source guard, and crash-journal
//! detection. Wired into the event loop by `main` (Task 11).

use culler_core::apply::{Journal, OpState};
use culler_core::model::{
    BUCKET_BESTS, BUCKET_KEEP, BUCKET_PICKS, BUCKET_REJECTED, BUCKET_REST, JOURNAL_FILE, Session,
    Shot,
};
use std::path::{Path, PathBuf};

#[derive(clap::Parser, Debug)]
#[command(name = "fastcull", about = "FastCull — keyboard-driven photo culling")]
pub struct Cli {
    /// Source folder of shots (scanned flat, non-recursive). Omit to open the
    /// startup screen and pick a folder interactively.
    pub source: Option<PathBuf>,
    /// Disable single-key auto-advance (tiering stays on the current shot).
    #[arg(long)]
    pub no_auto_advance: bool,
    #[arg(long)]
    pub bucket_rejected: Option<String>,
    #[arg(long)]
    pub bucket_rest: Option<String>,
    #[arg(long)]
    pub bucket_keep: Option<String>,
    #[arg(long)]
    pub bucket_picks: Option<String>,
    #[arg(long)]
    pub bucket_bests: Option<String>,
}

/// Bucket names in canonical index order [rejected, rest, keep, picks, bests].
pub fn default_buckets() -> [String; 5] {
    [
        BUCKET_REJECTED,
        BUCKET_REST,
        BUCKET_KEEP,
        BUCKET_PICKS,
        BUCKET_BESTS,
    ]
    .map(String::from)
}

/// Resolve the five working bucket names: a CLI override wins, else the default.
pub fn resolve_buckets(cli: &Cli) -> [String; 5] {
    let d = default_buckets();
    [
        cli.bucket_rejected.clone().unwrap_or_else(|| d[0].clone()),
        cli.bucket_rest.clone().unwrap_or_else(|| d[1].clone()),
        cli.bucket_keep.clone().unwrap_or_else(|| d[2].clone()),
        cli.bucket_picks.clone().unwrap_or_else(|| d[3].clone()),
        cli.bucket_bests.clone().unwrap_or_else(|| d[4].clone()),
    ]
}

/// Build the working session from a fresh scan + any prior decisions (keyed by stem).
/// Prior `current` is clamped into the new shot range; the undo stack is not restored.
pub fn reattach(source: &Path, scanned: Vec<Shot>, prev: Option<Session>) -> Session {
    match prev {
        Some(p) => {
            let current = if scanned.is_empty() {
                0
            } else {
                p.current.min(scanned.len() - 1)
            };
            Session {
                source_dir: source.to_path_buf(),
                shots: scanned,
                decisions: p.decisions, // stem-keyed; survives a rescan
                current,
                pending_apply: p.pending_apply, // crash breadcrumb survives the rescan
                undo: Vec::new(),
            }
        }
        None => Session {
            source_dir: source.to_path_buf(),
            shots: scanned,
            decisions: std::collections::HashMap::new(),
            current: 0,
            pending_apply: None,
            undo: Vec::new(),
        },
    }
}

/// True iff `dest` resolves to the source ROOT itself (which is refused).
/// A source subfolder is allowed. A not-yet-created dest can't be the existing root.
// Consumed by Task 12's apply-destination picker; unit-tested here now.
pub fn dest_is_source_root(source: &Path, dest: &Path) -> bool {
    match (source.canonicalize(), dest.canonicalize()) {
        (Ok(s), Ok(d)) => s == d,
        _ => source == dest,
    }
}

/// Detect an interrupted apply: any journal left in `dir` by a prior run.
/// Journal PRESENCE = incomplete run (spec §8 rev 4): success removes the
/// journal only after the sidecar-dir durability pass, so even an all-`Done`
/// journal still owes that pass — `resume()` on it re-runs the fsyncs and
/// retires it without re-executing moves; a fresh apply proceeds afterwards
/// (no stale-journal hijack: the retired journal is gone). An unreadable or
/// corrupt journal IS surfaced (returned), not silently ignored —
/// `journal_report` will show the parse failure.
pub fn find_crashed_apply(dir: &Path) -> Option<PathBuf> {
    let j = dir.join(JOURNAL_FILE);
    if j.is_file() { Some(j) } else { None }
}

/// Read and parse the journal at `journal_path`, mapping a parse failure to
/// `io::ErrorKind::InvalidData` (shared by `journal_report`/`journal_counts`).
fn parse_journal(journal_path: &Path) -> std::io::Result<Journal> {
    let bytes = std::fs::read(journal_path)?;
    serde_json::from_slice(&bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Human-readable per-op status summary for the resume-or-report prompt.
pub fn journal_report(journal_path: &Path) -> std::io::Result<String> {
    let journal = parse_journal(journal_path)?;
    let count = |want: OpState| journal.statuses.iter().filter(|s| **s == want).count();
    // AMENDMENT: Published counts as IN-FLIGHT (pending) for progress display —
    // the publish rename landed but the source unlink is still owed (rev 4).
    let done = count(OpState::Done);
    let published = count(OpState::Published);
    let pending = count(OpState::Pending) + published;
    let failed = count(OpState::Failed);
    let mut out = format!(
        "Interrupted apply into {}\n  done: {done}  pending: {pending}  failed: {failed}",
        journal.plan.dest.display()
    );
    if published > 0 {
        out.push_str(&format!(
            "  (of pending, {published} published-awaiting-unlink)"
        ));
    }
    Ok(out)
}

/// True iff `name` is a single plain path component: non-empty, not "." or
/// "..", contains no separator or NUL, and parses to exactly one `Normal`
/// path component equal to `name` itself.
fn is_plain_component(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." {
        return false;
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        return false;
    }
    let mut components = Path::new(name).components();
    matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(c)), None)
            if c == std::ffi::OsStr::new(name)
    )
}

/// Every bucket name must be a single plain path component (non-empty, not
/// "." or "..", no '/' or '\\', no NUL, and exactly one `Normal` `Path`
/// component equal to the name itself) — otherwise a CLI bucket override
/// could escape `dest` (e.g. "../x") or desync the collision-detection keys
/// used elsewhere (which are also bare bucket names).
pub fn validate_buckets(buckets: &[String; 5]) -> Result<(), String> {
    for name in buckets {
        if !is_plain_component(name) {
            return Err(format!(
                "invalid bucket name {name:?}: must be a single plain path component \
                 (non-empty, not \".\"/\"..\", no path separators)"
            ));
        }
    }
    Ok(())
}

/// Outcome of probing the session's crash breadcrumb (`Session.pending_apply`)
/// against the filesystem at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BreadcrumbProbe {
    /// No apply was in flight when the session was last saved.
    NoBreadcrumb,
    /// A journal is present at the breadcrumb's dest — a genuine crashed apply.
    /// The breadcrumb is left in place; the caller still needs it to locate
    /// the journal (Task 12's resume-or-report dialog / Task 13's banner).
    CrashedJournal(PathBuf),
    /// A breadcrumb pointed at `dest`, but no journal exists there (the crash
    /// happened before the first journal write, or `dest` vanished since).
    /// The breadcrumb has already been cleared from `session` — the caller
    /// should autosave and inform the user before proceeding fresh.
    StaleCleared(PathBuf),
}

/// Probe `session.pending_apply` against the filesystem. See `BreadcrumbProbe`
/// for what each outcome means and what has already been mutated on `session`.
pub fn probe_breadcrumb(session: &mut Session) -> BreadcrumbProbe {
    let Some(dest) = session.pending_apply.clone() else {
        return BreadcrumbProbe::NoBreadcrumb;
    };
    match find_crashed_apply(&dest) {
        Some(j) => BreadcrumbProbe::CrashedJournal(j),
        None => {
            // Tolerate a breadcrumb-without-journal (mandated): clear it so a
            // stale pointer never re-surfaces as a phantom crash on the next
            // launch too.
            session.pending_apply = None;
            BreadcrumbProbe::StaleCleared(dest)
        }
    }
}

/// Done/total op counts for the crash-recovery panel. Kept consistent with
/// `journal_report`: `Published` is NOT counted as done (its source unlink is
/// still owed), so an all-`Done` journal is the only way to see `done == total`.
pub fn journal_counts(journal_path: &Path) -> std::io::Result<(usize, usize)> {
    let journal = parse_journal(journal_path)?;
    let done = journal
        .statuses
        .iter()
        .filter(|s| **s == OpState::Done)
        .count();
    let total = journal.statuses.len();
    Ok((done, total))
}

/// Everything the crash-recovery startup screen (Task A3) needs to locate and
/// describe an interrupted apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashInfo {
    /// The `.fastcull-apply.json` path.
    pub journal: PathBuf,
    /// The destination dir that owns the journal.
    pub dest: PathBuf,
    /// `true` when found via `session.pending_apply` (our own apply crashed);
    /// `false` when found by probing `source` (the source was itself a prior
    /// run's destination).
    pub from_breadcrumb: bool,
}

/// Result of the two INDEPENDENT startup crash-detection channels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupScan {
    /// A stale `pending_apply` breadcrumb was cleared in-memory (its dest had no
    /// journal). The caller must persist the cleared session + log. Orthogonal to
    /// `crash` — a stale breadcrumb can co-occur with a real source-dir journal.
    pub stale_cleared: Option<PathBuf>,
    /// A journal to surface in the 2d screen, if any. Breadcrumb-detected takes
    /// priority (`from_breadcrumb: true`); else the source-dir probe
    /// (`from_breadcrumb: false`).
    pub crash: Option<CrashInfo>,
}

/// Single entry point routing both crash-detection channels the spec
/// requires: the breadcrumb (`session.pending_apply`, the primary
/// "detected on next launch" mechanism) and the source-dir probe (for when
/// `source` was itself a prior run's destination). The breadcrumb channel
/// takes priority when both point at a live journal. The two channels are
/// INDEPENDENT: a stale-breadcrumb clear never short-circuits the source-dir
/// probe, since `source` can itself be a former apply destination with its
/// own leftover journal (F1 fix — the two used to be mutually exclusive).
pub fn find_startup_crash(session: &mut Session, source: &Path) -> StartupScan {
    let bc_dest = session.pending_apply.clone();
    match probe_breadcrumb(session) {
        BreadcrumbProbe::CrashedJournal(j) => StartupScan {
            stale_cleared: None,
            crash: Some(CrashInfo {
                dest: bc_dest.expect("breadcrumb present on CrashedJournal"),
                journal: j,
                from_breadcrumb: true,
            }),
        },
        BreadcrumbProbe::StaleCleared(d) => StartupScan {
            stale_cleared: Some(d),
            // INDEPENDENT source-dir probe still runs.
            crash: find_crashed_apply(source).map(|j| CrashInfo {
                dest: source.to_path_buf(),
                journal: j,
                from_breadcrumb: false,
            }),
        },
        BreadcrumbProbe::NoBreadcrumb => StartupScan {
            stale_cleared: None,
            crash: find_crashed_apply(source).map(|j| CrashInfo {
                dest: source.to_path_buf(),
                journal: j,
                from_breadcrumb: false,
            }),
        },
    }
}

/// Task A1: `journal_counts` (done/total for the crash-recovery panel) and
/// `find_startup_crash` (routes the breadcrumb + source-probe crash-detection
/// channels into a `StartupScan` outcome for the caller — the two channels
/// are independent, see F1 fix round 2).
#[cfg(test)]
mod crash_info_tests {
    use super::*;
    use culler_core::apply::{Journal, OpState};
    use culler_core::model::{JOURNAL_FILE, Session};
    use culler_core::plan::{ApplyPlan, TierCountsPlan};

    fn empty_plan(dest: &std::path::Path) -> ApplyPlan {
        ApplyPlan {
            dest: dest.to_path_buf(),
            buckets: default_buckets(),
            ops: Vec::new(),
            per_bucket_counts: TierCountsPlan::default(),
            skipped_sidecar_writes: Vec::new(),
            stale: Vec::new(),
            total_bytes: 0,
        }
    }

    fn write_journal_fixture(dest: &std::path::Path, statuses: Vec<OpState>) {
        let journal = Journal {
            plan: empty_plan(dest),
            statuses,
        };
        std::fs::write(
            dest.join(JOURNAL_FILE),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
    }

    fn session_with(dir: &std::path::Path, pending: Option<std::path::PathBuf>) -> Session {
        Session {
            source_dir: dir.to_path_buf(),
            shots: Vec::new(),
            decisions: std::collections::HashMap::new(),
            current: 0,
            pending_apply: pending,
            undo: Vec::new(),
        }
    }

    #[test]
    fn journal_counts_done_and_total() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path();
        write_journal_fixture(dest, vec![OpState::Done, OpState::Done, OpState::Pending]);
        let j = find_crashed_apply(dest).expect("journal present");
        assert_eq!(journal_counts(&j).unwrap(), (2, 3));
    }

    #[test]
    fn journal_counts_published_not_counted_as_done() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path();
        write_journal_fixture(
            dest,
            vec![OpState::Done, OpState::Published, OpState::Pending],
        );
        let j = find_crashed_apply(dest).expect("journal present");
        assert_eq!(journal_counts(&j).unwrap(), (1, 3));
    }

    #[test]
    fn journal_counts_all_done_is_full() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path();
        write_journal_fixture(dest, vec![OpState::Done, OpState::Done]);
        let j = find_crashed_apply(dest).expect("journal present");
        assert_eq!(journal_counts(&j).unwrap(), (2, 2));
    }

    #[test]
    fn journal_counts_rejects_corrupt_journal() {
        let tmp = tempfile::tempdir().unwrap();
        let journal_path = tmp.path().join(JOURNAL_FILE);
        std::fs::write(&journal_path, b"{ not json").unwrap();
        assert!(journal_counts(&journal_path).is_err());
    }

    #[test]
    fn find_startup_crash_none() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path();
        let mut session = session_with(source, None);
        let scan = find_startup_crash(&mut session, source);
        assert_eq!(scan.stale_cleared, None);
        assert_eq!(scan.crash, None);
    }

    #[test]
    fn find_startup_crash_from_breadcrumb() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        let dest = tmp.path().join("sorted");
        std::fs::create_dir_all(&dest).unwrap();
        write_journal_fixture(&dest, vec![OpState::Pending]);

        let mut session = session_with(&source, Some(dest.clone()));
        let scan = find_startup_crash(&mut session, &source);
        let info = scan.crash.expect("crash detected");
        assert!(info.from_breadcrumb);
        assert_eq!(info.dest, dest);
        assert_eq!(info.journal, dest.join(JOURNAL_FILE));
        assert_eq!(scan.stale_cleared, None);
        // Breadcrumb kept — the caller still needs it (spec: A3 uses it).
        assert_eq!(session.pending_apply, Some(dest));
    }

    #[test]
    fn find_startup_crash_stale_breadcrumb() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        let dest = tmp.path().join("sorted"); // never created / no journal there

        let mut session = session_with(&source, Some(dest.clone()));
        let scan = find_startup_crash(&mut session, &source);
        assert_eq!(scan.stale_cleared, Some(dest));
        assert_eq!(scan.crash, None);
        assert_eq!(session.pending_apply, None);
    }

    #[test]
    fn find_startup_crash_from_source_probe() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        write_journal_fixture(&source, vec![OpState::Pending]);

        let mut session = session_with(&source, None);
        let scan = find_startup_crash(&mut session, &source);
        let info = scan.crash.expect("crash detected");
        assert!(!info.from_breadcrumb);
        assert_eq!(info.dest, source);
        assert_eq!(info.journal, source.join(JOURNAL_FILE));
        assert_eq!(scan.stale_cleared, None);
    }

    #[test]
    fn find_startup_crash_breadcrumb_takes_priority() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        write_journal_fixture(&source, vec![OpState::Pending]); // journal in source too

        let dest = tmp.path().join("sorted");
        std::fs::create_dir_all(&dest).unwrap();
        write_journal_fixture(&dest, vec![OpState::Pending]);

        let mut session = session_with(&source, Some(dest.clone()));
        let scan = find_startup_crash(&mut session, &source);
        let info = scan.crash.expect("crash detected");
        assert!(info.from_breadcrumb);
        assert_eq!(info.dest, dest);
        assert_eq!(scan.stale_cleared, None);
    }

    /// F1 (adversarial fix): the breadcrumb and source-dir probe channels
    /// are INDEPENDENT — a stale breadcrumb clear must not short-circuit the
    /// source-dir probe. Here `source` is itself a former apply DESTINATION
    /// with a leftover journal, while the session ALSO carries a stale
    /// breadcrumb pointing elsewhere (with no journal there).
    #[test]
    fn stale_breadcrumb_and_live_source_journal_shows_2d() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        write_journal_fixture(&source, vec![OpState::Pending]); // real journal IN source

        let dest_without_journal = tmp.path().join("stale-dest"); // never created
        let mut session = session_with(&source, Some(dest_without_journal.clone()));

        let scan = find_startup_crash(&mut session, &source);
        assert_eq!(scan.stale_cleared, Some(dest_without_journal));
        assert_eq!(
            scan.crash,
            Some(CrashInfo {
                dest: source.clone(),
                journal: source.join(JOURNAL_FILE),
                from_breadcrumb: false,
            })
        );
        assert_eq!(session.pending_apply, None); // breadcrumb cleared
    }

    /// F1 companion case: stale breadcrumb, but no journal in `source` either
    /// — the independent probe correctly reports no crash and the caller
    /// proceeds straight into culling.
    #[test]
    fn stale_breadcrumb_and_no_source_journal_is_cull() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        // No journal written into `source`.

        let dest = tmp.path().join("stale-dest"); // never created / no journal there
        let mut session = session_with(&source, Some(dest.clone()));

        let scan = find_startup_crash(&mut session, &source);
        assert_eq!(scan.stale_cleared, Some(dest));
        assert_eq!(scan.crash, None);
        assert_eq!(session.pending_apply, None);
    }
}

#[cfg(test)]
mod startup_tests {
    use super::*;
    use culler_core::model::{CaptureTime, Decision, Session, Shot, Tier};

    fn shot(stem: &str, dir: &std::path::Path) -> Shot {
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
    fn dest_equal_source_root_is_refused_subfolder_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path();
        assert!(dest_is_source_root(src, src)); // exact root -> refused
        let sub = src.join("sorted");
        assert!(!dest_is_source_root(src, &sub)); // subfolder -> allowed
        let elsewhere = tmp.path().parent().unwrap();
        assert!(!dest_is_source_root(src, elsewhere));
    }

    #[test]
    fn reattach_keeps_prior_decisions_by_stem_and_clamps_current() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let mut prev_decisions = std::collections::HashMap::new();
        prev_decisions.insert(
            "IMG_0001".to_string(),
            Decision {
                tier: Some(Tier::Best),
                tags: vec!["hero".into()],
                visited: true,
            },
        );
        let prev = Session {
            source_dir: dir.to_path_buf(),
            shots: vec![shot("IMG_0001", dir), shot("IMG_0002", dir)],
            decisions: prev_decisions,
            current: 5, // stale index from before a rescan removed shots
            pending_apply: None,
            undo: vec![],
        };
        let scanned = vec![shot("IMG_0001", dir)]; // only one shot remains on disk
        let s = reattach(dir, scanned, Some(prev));
        assert_eq!(s.shots.len(), 1);
        assert_eq!(s.current, 0); // clamped into range
        assert_eq!(s.decision(0).tier, Some(Tier::Best)); // re-attached by stem
        assert_eq!(s.decision(0).tags, vec!["hero".to_string()]);
        assert!(s.undo.is_empty()); // undo not restored across sessions
    }

    #[test]
    fn reattach_carries_the_crash_breadcrumb() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let prev = Session {
            source_dir: dir.to_path_buf(),
            shots: vec![shot("IMG_0001", dir)],
            decisions: std::collections::HashMap::new(),
            current: 0,
            pending_apply: Some(dir.join("sorted")), // crashed mid-apply last run
            undo: vec![],
        };
        let s = reattach(dir, vec![shot("IMG_0001", dir)], Some(prev));
        // The breadcrumb must survive the rescan — it is how the next launch
        // finds the dest journal (spec §6 rev 3).
        assert_eq!(s.pending_apply, Some(dir.join("sorted")));
    }

    #[test]
    fn reattach_none_starts_fresh() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let s = reattach(dir, vec![shot("IMG_0001", dir)], None);
        assert_eq!(s.current, 0);
        assert!(s.decisions.is_empty());
        assert_eq!(s.source_dir, dir);
    }

    #[test]
    fn resolve_buckets_defaults_and_overrides() {
        let d = default_buckets();
        assert_eq!(
            d,
            ["00_rejected", "01_rest", "02_keep", "03_picks", "04_bests"].map(String::from)
        );
        let cli = Cli {
            source: Some("/x".into()),
            no_auto_advance: false,
            bucket_rejected: Some("trash".into()),
            bucket_rest: None,
            bucket_keep: None,
            bucket_picks: None,
            bucket_bests: None,
        };
        let b = resolve_buckets(&cli);
        assert_eq!(b[0], "trash");
        assert_eq!(b[1], "01_rest");
    }
}

/// Pulled forward from Task 12 (crash-journal detection): the brief's main()
/// references `find_crashed_apply`/`journal_report` at launch.
#[cfg(test)]
mod crash_tests {
    use super::*;
    use culler_core::apply::{Journal, OpState};
    use culler_core::model::JOURNAL_FILE;
    use culler_core::plan::{ApplyPlan, TierCountsPlan};

    fn empty_plan(dest: &std::path::Path) -> ApplyPlan {
        ApplyPlan {
            dest: dest.to_path_buf(),
            buckets: default_buckets(),
            ops: Vec::new(),
            per_bucket_counts: TierCountsPlan::default(),
            skipped_sidecar_writes: Vec::new(),
            stale: Vec::new(),
            total_bytes: 0,
        }
    }

    fn write_journal_fixture(dest: &std::path::Path, statuses: Vec<OpState>) {
        let journal = Journal {
            plan: empty_plan(dest),
            statuses,
        };
        std::fs::write(
            dest.join(JOURNAL_FILE),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn find_crashed_apply_detects_and_report_summarizes_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path();
        write_journal_fixture(dest, vec![OpState::Done, OpState::Pending]);

        let found = find_crashed_apply(dest).expect("journal present");
        let report = journal_report(&found).unwrap();
        assert!(report.contains("done: 1"), "report was: {report}");
        assert!(report.contains("pending: 1"), "report was: {report}");
        assert!(report.contains("failed: 0"), "report was: {report}");
    }

    #[test]
    fn find_crashed_apply_still_some_on_all_done_journal() {
        // Presence-keyed (rev 4): success only removes the journal AFTER the
        // sidecar-dir durability pass, so an all-Done journal left on disk is
        // still an incomplete run.
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path();
        write_journal_fixture(dest, vec![OpState::Done, OpState::Done]);

        assert!(find_crashed_apply(dest).is_some());
    }

    #[test]
    fn journal_report_counts_published_as_pending_awaiting_unlink() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path();
        write_journal_fixture(
            dest,
            vec![OpState::Done, OpState::Published, OpState::Pending],
        );

        let found = find_crashed_apply(dest).expect("journal present");
        let report = journal_report(&found).unwrap();
        assert!(report.contains("done: 1"), "report was: {report}");
        assert!(report.contains("pending: 2"), "report was: {report}");
        assert!(report.contains("failed: 0"), "report was: {report}");
        assert!(
            report.contains("published-awaiting-unlink"),
            "report was: {report}"
        );
    }
}

/// Controller-mandated carry-forward: bucket-name validation guards against a
/// CLI override escaping dest or desyncing collision keys.
#[cfg(test)]
mod validate_bucket_tests {
    use super::*;

    #[test]
    fn default_buckets_pass_validation() {
        assert!(validate_buckets(&default_buckets()).is_ok());
    }

    #[test]
    fn rejects_parent_dir_escape() {
        let mut b = default_buckets();
        b[0] = "../x".to_string();
        let err = validate_buckets(&b).unwrap_err();
        assert!(err.contains("../x"), "error was: {err}");
    }

    #[test]
    fn rejects_nested_path() {
        let mut b = default_buckets();
        b[0] = "a/b".to_string();
        let err = validate_buckets(&b).unwrap_err();
        assert!(err.contains("a/b"), "error was: {err}");
    }

    #[test]
    fn rejects_empty_name() {
        let mut b = default_buckets();
        b[0] = "".to_string();
        assert!(validate_buckets(&b).is_err());
    }

    #[test]
    fn rejects_absolute_path() {
        let mut b = default_buckets();
        b[0] = "/abs".to_string();
        let err = validate_buckets(&b).unwrap_err();
        assert!(err.contains("/abs"), "error was: {err}");
    }

    #[test]
    fn rejects_current_dir() {
        let mut b = default_buckets();
        b[0] = ".".to_string();
        assert!(validate_buckets(&b).is_err());
    }
}

/// Controller-mandated carry-forward: tolerate a breadcrumb-without-journal
/// (crash before the first journal write, or a vanished dest) by clearing it
/// rather than getting stuck offering to resume a journal that doesn't exist.
#[cfg(test)]
mod breadcrumb_tests {
    use super::*;
    use culler_core::model::Session;

    fn session_with(dir: &std::path::Path, pending: Option<std::path::PathBuf>) -> Session {
        Session {
            source_dir: dir.to_path_buf(),
            shots: Vec::new(),
            decisions: std::collections::HashMap::new(),
            current: 0,
            pending_apply: pending,
            undo: Vec::new(),
        }
    }

    #[test]
    fn no_breadcrumb_when_pending_apply_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = session_with(tmp.path(), None);
        assert!(matches!(
            probe_breadcrumb(&mut s),
            BreadcrumbProbe::NoBreadcrumb
        ));
        assert_eq!(s.pending_apply, None);
    }

    #[test]
    fn crashed_journal_kept_when_dest_has_a_journal() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("sorted");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join(culler_core::model::JOURNAL_FILE), b"{}").unwrap();

        let mut s = session_with(tmp.path(), Some(dest.clone()));
        match probe_breadcrumb(&mut s) {
            BreadcrumbProbe::CrashedJournal(j) => {
                assert_eq!(j, dest.join(culler_core::model::JOURNAL_FILE))
            }
            other => panic!("expected CrashedJournal, got {other:?}"),
        }
        // Breadcrumb kept — the caller still needs it to locate the journal.
        assert_eq!(s.pending_apply, Some(dest));
    }

    #[test]
    fn stale_breadcrumb_cleared_when_no_journal_at_dest() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("sorted"); // never created / no journal there
        let mut s = session_with(tmp.path(), Some(dest.clone()));
        match probe_breadcrumb(&mut s) {
            BreadcrumbProbe::StaleCleared(d) => assert_eq!(d, dest),
            other => panic!("expected StaleCleared, got {other:?}"),
        }
        assert!(s.pending_apply.is_none());
    }
}
