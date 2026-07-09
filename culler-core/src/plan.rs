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
                let name = from
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default();
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
}
