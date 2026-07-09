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
///
/// `stale` is left empty: `plan` does no I/O, so the binary pre-verifies file
/// existence, drops missing shots before calling `plan`, and fills `stale` for
/// the preview post-hoc.
///
/// The carried sidecar is the one in `Shot.sidecar`: when both the Adobe
/// (`stem.xmp`) and darktable (`stem.ext.xmp`) conventions existed on disk,
/// scan chose the darktable one and left the Adobe file as an untracked leftover.
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
            let mut candidate: Vec<String> = rests
                .iter()
                .map(|rest| format!("{bucket}/{new_stem}{rest}"))
                .collect();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        BUCKET_BESTS, BUCKET_KEEP, BUCKET_PICKS, BUCKET_REJECTED, BUCKET_REST, CaptureTime,
        Decision, Session, Shot, Tier,
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
            Decision {
                tier: Some(Tier::Best),
                tags: vec![],
                visited: true,
            },
        );
        // IMG_0002 has no decision entry => undecided => 01_rest
        let session = Session {
            shots,
            decisions,
            ..Default::default()
        };

        let p = plan(
            &session,
            Path::new("/dest"),
            &buckets,
            &BTreeSet::new(),
            &HashMap::new(),
        );

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
        let session = Session {
            shots,
            decisions: HashMap::new(),
            ..Default::default()
        };

        // `existing` holds BUCKET-RELATIVE paths (rev 3).
        let mut existing = BTreeSet::new();
        existing.insert("01_rest/IMG_0001.JPG".to_string());
        existing.insert("01_rest/IMG_0001.CR3".to_string());
        existing.insert("01_rest/IMG_0002.JPG".to_string());

        let p = plan(
            &session,
            Path::new("/dest"),
            &buckets,
            &existing,
            &HashMap::new(),
        );

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
        assert_eq!(
            p.ops[1].moves[0].to,
            PathBuf::from("/dest/01_rest/IMG_0002-1.JPG")
        );

        // the real stem IMG_0002-1 collides with the name op[1] claimed => IMG_0002-1-1
        assert_eq!(p.ops[2].suffix, Some(1));
        assert_eq!(
            p.ops[2].moves[0].to,
            PathBuf::from("/dest/01_rest/IMG_0002-1-1.JPG")
        );
    }

    #[test]
    fn plan_ignores_same_name_in_a_different_bucket() {
        let buckets = default_buckets();
        let shots = vec![shot("IMG_0009", "JPG", None, None)]; // undecided -> 01_rest
        let session = Session {
            shots,
            decisions: HashMap::new(),
            ..Default::default()
        };

        let mut existing = BTreeSet::new();
        existing.insert("02_keep/IMG_0009.JPG".to_string()); // same NAME, different bucket

        let p = plan(
            &session,
            Path::new("/dest"),
            &buckets,
            &existing,
            &HashMap::new(),
        );
        // rev 3: collisions are per target directory — no spurious rename.
        assert_eq!(p.ops[0].suffix, None);
        assert_eq!(
            p.ops[0].moves[0].to,
            PathBuf::from("/dest/01_rest/IMG_0009.JPG")
        );
    }

    #[test]
    fn plan_writes_new_sidecar_and_skips_preexisting() {
        let buckets = default_buckets();
        let shots = vec![
            shot("A", "JPG", None, None), // Keep tier => write new sidecar (rating 3)
            shot("B", "JPG", None, None), // tags only => write new sidecar (rating None)
            shot("C", "JPG", Some("CR3"), Some("/src/C.xmp")), // pre-existing sidecar => skip + carry
            shot("D", "JPG", None, None),                      // no tier, no tags => no sidecar
        ];
        let mut decisions = HashMap::new();
        decisions.insert(
            "A".to_string(),
            Decision {
                tier: Some(Tier::Keep),
                tags: vec![],
                visited: true,
            },
        );
        decisions.insert(
            "B".to_string(),
            Decision {
                tier: None,
                tags: vec!["sky".to_string()],
                visited: true,
            },
        );
        decisions.insert(
            "C".to_string(),
            Decision {
                tier: Some(Tier::Pick),
                tags: vec!["hero".to_string()],
                visited: true,
            },
        );
        // D: no entry
        let session = Session {
            shots,
            decisions,
            ..Default::default()
        };

        let p = plan(
            &session,
            Path::new("/dest"),
            &buckets,
            &BTreeSet::new(),
            &HashMap::new(),
        );

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
        assert!(
            p.ops[2]
                .moves
                .iter()
                .any(|m| m.to == Path::new("/dest/03_picks/C.xmp"))
        );
        assert_eq!(p.skipped_sidecar_writes, vec!["C".to_string()]);
        // D: nothing to write
        assert_eq!(p.ops[3].write_sidecar, None);
    }

    /// Pins the sidecar-carry contract at plan level: the carried sidecar is
    /// exactly the file in `Shot.sidecar`. Background (scan, already landed):
    /// when both the Adobe (`stem.xmp`) and darktable (`stem.ext.xmp`)
    /// conventions exist on disk for one stem, the darktable one wins and is
    /// stored in `Shot.sidecar`; the Adobe file stays on disk as an untracked
    /// leftover. `plan` must emit exactly one sidecar move — for the scanned
    /// file, full after-stem suffix preserved — and never touch the other name.
    #[test]
    fn plan_carries_exactly_the_scanned_sidecar() {
        let buckets = default_buckets();
        let shots = vec![shot("E", "JPG", Some("CR3"), Some("/src/E.CR3.xmp"))];
        let mut decisions = HashMap::new();
        decisions.insert(
            "E".to_string(),
            Decision {
                tier: Some(Tier::Keep),
                tags: vec![],
                visited: true,
            },
        );
        let session = Session {
            shots,
            decisions,
            ..Default::default()
        };

        let p = plan(
            &session,
            Path::new("/dest"),
            &buckets,
            &BTreeSet::new(),
            &HashMap::new(),
        );

        // The darktable-convention name is preserved via the after-stem rest.
        assert!(
            p.ops[0]
                .moves
                .iter()
                .any(|m| m.to == Path::new("/dest/02_keep/E.CR3.xmp"))
        );
        assert_eq!(p.ops[0].write_sidecar, None);
        assert_eq!(p.skipped_sidecar_writes, vec!["E".to_string()]);
        // Neither a move nor a fresh SidecarWrite ever targets the Adobe name —
        // plan only knows about the sidecar scan chose (`Shot.sidecar`).
        assert!(
            !p.ops[0]
                .moves
                .iter()
                .any(|m| m.to == Path::new("/dest/02_keep/E.xmp"))
        );
    }

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
        decisions.insert(
            "R".to_string(),
            Decision {
                tier: Some(Tier::Reject),
                ..Default::default()
            },
        );
        decisions.insert(
            "K".to_string(),
            Decision {
                tier: Some(Tier::Keep),
                ..Default::default()
            },
        );
        decisions.insert(
            "P".to_string(),
            Decision {
                tier: Some(Tier::Pick),
                ..Default::default()
            },
        );
        decisions.insert(
            "B".to_string(),
            Decision {
                tier: Some(Tier::Best),
                ..Default::default()
            },
        );
        // Z: no entry
        let session = Session {
            shots,
            decisions,
            ..Default::default()
        };

        let mut sizes = HashMap::new();
        sizes.insert("R".to_string(), 10u64);
        sizes.insert("K".to_string(), 20u64);
        sizes.insert("P".to_string(), 30u64);
        sizes.insert("B".to_string(), 40u64);
        sizes.insert("Z".to_string(), 5u64);
        // a stem with no size entry contributes 0 (defensive)

        let p = plan(
            &session,
            Path::new("/dest"),
            &buckets,
            &BTreeSet::new(),
            &sizes,
        );

        assert_eq!(
            p.per_bucket_counts,
            TierCountsPlan {
                rejected: 1,
                rest: 1,
                keep: 1,
                picks: 1,
                bests: 1
            }
        );
        assert_eq!(p.total_bytes, 105);
        assert!(p.stale.is_empty());
    }

    #[test]
    fn plan_reports_all_skipped_sidecar_writes() {
        let buckets = default_buckets();
        let shots = vec![
            shot("Skip1", "JPG", None, Some("/src/Skip1.xmp")), // has sidecar + tags => report
            shot("Skip2", "JPG", None, Some("/src/Skip2.xmp")), // has sidecar + tier => report
            shot("NoReport", "JPG", None, Some("/src/NoReport.xmp")), // has sidecar but no content => do NOT report
        ];
        let mut decisions = HashMap::new();
        decisions.insert(
            "Skip1".to_string(),
            Decision {
                tier: None,
                tags: vec!["hero".to_string()],
                visited: true,
            },
        );
        decisions.insert(
            "Skip2".to_string(),
            Decision {
                tier: Some(Tier::Keep),
                tags: vec![],
                visited: true,
            },
        );
        // NoReport: no decision entry => no tier, no tags => no content => not reported
        let session = Session {
            shots,
            decisions,
            ..Default::default()
        };

        let p = plan(
            &session,
            Path::new("/dest"),
            &buckets,
            &BTreeSet::new(),
            &HashMap::new(),
        );

        assert_eq!(p.skipped_sidecar_writes, vec!["Skip1".to_string(), "Skip2".to_string()]);
    }

    #[test]
    fn plan_claimed_names_are_scoped_per_bucket() {
        let buckets = default_buckets();

        // Case (a): different bucket — no collision with intra-plan claim
        {
            let shots = vec![
                shot("M", "JPG", None, None),     // Keep tier (02_keep), collides with existing
                shot("M-1", "JPG", None, None),   // Pick tier (03_picks), should NOT collide
            ];
            let mut decisions = HashMap::new();
            decisions.insert(
                "M".to_string(),
                Decision {
                    tier: Some(Tier::Keep),
                    tags: vec![],
                    visited: true,
                },
            );
            decisions.insert(
                "M-1".to_string(),
                Decision {
                    tier: Some(Tier::Pick),
                    tags: vec![],
                    visited: true,
                },
            );
            let session = Session {
                shots,
                decisions,
                ..Default::default()
            };

            let mut existing = BTreeSet::new();
            existing.insert("02_keep/M.JPG".to_string()); // collides with shot M

            let p = plan(
                &session,
                Path::new("/dest"),
                &buckets,
                &existing,
                &HashMap::new(),
            );

            // Shot M in 02_keep collides with existing, gets suffix
            assert_eq!(p.ops[0].stem, "M");
            assert_eq!(p.ops[0].bucket, "02_keep");
            assert_eq!(p.ops[0].suffix, Some(1));
            assert_eq!(
                p.ops[0].moves[0].to,
                PathBuf::from("/dest/02_keep/M-1.JPG")
            );

            // Shot M-1 in 03_picks does NOT collide with the claim in 02_keep (different bucket)
            assert_eq!(p.ops[1].stem, "M-1");
            assert_eq!(p.ops[1].bucket, "03_picks");
            assert_eq!(p.ops[1].suffix, None);
            assert_eq!(
                p.ops[1].moves[0].to,
                PathBuf::from("/dest/03_picks/M-1.JPG")
            );
        }

        // Case (b): same bucket — DOES collide with intra-plan claim
        {
            let shots = vec![
                shot("M", "JPG", None, None),     // Keep tier (02_keep), collides with existing
                shot("M-1", "JPG", None, None),   // Keep tier (02_keep), collides with M's claim
            ];
            let mut decisions = HashMap::new();
            decisions.insert(
                "M".to_string(),
                Decision {
                    tier: Some(Tier::Keep),
                    tags: vec![],
                    visited: true,
                },
            );
            decisions.insert(
                "M-1".to_string(),
                Decision {
                    tier: Some(Tier::Keep),
                    tags: vec![],
                    visited: true,
                },
            );
            let session = Session {
                shots,
                decisions,
                ..Default::default()
            };

            let mut existing = BTreeSet::new();
            existing.insert("02_keep/M.JPG".to_string()); // collides with shot M

            let p = plan(
                &session,
                Path::new("/dest"),
                &buckets,
                &existing,
                &HashMap::new(),
            );

            // Shot M in 02_keep collides with existing, gets suffix
            assert_eq!(p.ops[0].stem, "M");
            assert_eq!(p.ops[0].bucket, "02_keep");
            assert_eq!(p.ops[0].suffix, Some(1));
            assert_eq!(
                p.ops[0].moves[0].to,
                PathBuf::from("/dest/02_keep/M-1.JPG")
            );

            // Shot M-1 in 02_keep DOES collide with M's claim (same bucket), gets suffix
            assert_eq!(p.ops[1].stem, "M-1");
            assert_eq!(p.ops[1].bucket, "02_keep");
            assert_eq!(p.ops[1].suffix, Some(1));
            assert_eq!(
                p.ops[1].moves[0].to,
                PathBuf::from("/dest/02_keep/M-1-1.JPG")
            );
        }
    }
}
