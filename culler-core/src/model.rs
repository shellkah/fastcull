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
pub const RAW_EXTS: &[&str] = &[
    "cr3", "cr2", "nef", "arw", "raf", "rw2", "orf", "dng", "pef", "srw",
];
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
        assert!(
            RAW_EXTS
                .iter()
                .all(|e| e == &e.to_lowercase() && !e.starts_with('.'))
        );
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
        session.set_tags(0, vec!["a".to_string(), "b".to_string(), "a".to_string()]);
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
}
