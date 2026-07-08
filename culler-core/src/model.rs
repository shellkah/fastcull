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
}
