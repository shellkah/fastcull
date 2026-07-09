//! Folder scan: a flat (non-recursive) walk that groups files by filename stem
//! into `Shot`s and sorts them into a stable, capture-time filmstrip order.
//!
//! A shot REQUIRES a JPEG display file in v1: stems with only a RAW are not
//! cullable shots (they are surfaced by `scan_report`, added in a later task).
//! Zero GUI dependencies.

use crate::model::{CaptureTime, JPEG_EXTS, RAW_EXTS, Shot};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum ScanError {
    Io(std::io::Error),
    NotADir(PathBuf),
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScanError::Io(e) => write!(f, "scan I/O error: {e}"),
            ScanError::NotADir(p) => write!(f, "not a directory: {}", p.display()),
        }
    }
}

impl std::error::Error for ScanError {}

/// Files sharing one filename stem, accumulated during the walk.
#[derive(Default)]
struct Group {
    jpeg: Option<PathBuf>,
    raw: Option<PathBuf>,
}

/// Flat (non-recursive) walk of `dir`. Groups files by filename stem, emits a
/// `Shot` for every stem that has a JPEG display file, and returns the RAW paths
/// of stems that have a RAW but **no JPEG** (not cullable shots in v1) so they
/// are never silently dropped.
pub fn scan_report(dir: &Path) -> Result<(Vec<Shot>, Vec<PathBuf>), ScanError> {
    if !dir.is_dir() {
        return Err(ScanError::NotADir(dir.to_path_buf()));
    }

    let mut entries: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(ScanError::Io)? {
        let entry = entry.map_err(ScanError::Io)?;
        if entry.file_type().map_err(ScanError::Io)?.is_dir() {
            continue; // flat walk: subdirectories are ignored entirely
        }
        entries.push(entry.path());
    }
    entries.sort();

    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    for path in &entries {
        let ext = match ext_lower(path) {
            Some(e) => e,
            None => continue, // no extension → unrecognized, ignored
        };
        if JPEG_EXTS.contains(&ext.as_str()) {
            let stem = file_stem_string(path);
            groups
                .entry(stem)
                .or_default()
                .jpeg
                .get_or_insert_with(|| path.clone());
        } else if RAW_EXTS.contains(&ext.as_str()) {
            let stem = file_stem_string(path);
            groups
                .entry(stem)
                .or_default()
                .raw
                .get_or_insert_with(|| path.clone());
        }
        // Everything else (videos, session sidecar, …) is unrecognized → ignored.
    }

    let mut shots: Vec<Shot> = Vec::new();
    let mut raw_only: Vec<PathBuf> = Vec::new();
    for (stem, group) in groups {
        match group.jpeg {
            Some(jpeg) => shots.push(Shot {
                stem,
                jpeg,
                raw: group.raw,
                sidecar: None,
                capture: CaptureTime::default(),
            }),
            None => {
                // No JPEG → not a cullable shot in v1. Report the RAW so a
                // RAW-only stem isn't silently dropped (embedded-preview
                // extraction is a phase-2 item).
                if let Some(raw) = group.raw {
                    raw_only.push(raw);
                }
                // A stem with neither JPEG nor RAW (e.g. an orphan file) is dropped.
            }
        }
    }
    Ok((shots, raw_only))
}

/// Flat walk of `dir` returning only the cullable shots. Thin wrapper over
/// `scan_report` that discards the RAW-only report.
pub fn scan(dir: &Path) -> Result<Vec<Shot>, ScanError> {
    let (shots, _raw_only) = scan_report(dir)?;
    Ok(shots)
}

/// Lower-cased file extension (no leading dot), or `None` when absent.
fn ext_lower(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

/// The filename portion before the final extension, as an owned `String`.
fn file_stem_string(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, unique temp directory for a single test (no `tempfile` dep —
    /// same hand-rolled approach as Phase 1's `persist` tests).
    fn unique_temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "fastcull-scan-{tag}-{}-{nanos}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Create an empty placeholder file. An empty `.jpg` has no decodable EXIF,
    /// so its `CaptureTime` stays default — perfect for pairing/sort fixtures.
    fn touch(path: &Path) {
        std::fs::write(path, b"").unwrap();
    }

    /// The stems of the returned shots, in order.
    fn stems(shots: &[Shot]) -> Vec<String> {
        shots.iter().map(|s| s.stem.clone()).collect()
    }

    #[test]
    fn not_a_dir_path_is_an_error() {
        let dir = unique_temp_dir("notadir");
        let file = dir.join("IMG_0001.JPG");
        touch(&file);
        match scan(&file) {
            Err(ScanError::NotADir(p)) => assert_eq!(p, file),
            other => panic!("expected NotADir, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_dir_yields_no_shots() {
        let dir = unique_temp_dir("empty");
        assert!(scan(&dir).unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn subdirectory_is_ignored() {
        let dir = unique_temp_dir("flatwalk");
        touch(&dir.join("TOP.JPG"));
        let sub = dir.join("nested");
        std::fs::create_dir_all(&sub).unwrap();
        touch(&sub.join("INNER.JPG")); // must NOT appear — flat walk only

        let shots = scan(&dir).unwrap();
        assert_eq!(stems(&shots), vec!["TOP".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn jpeg_files_become_shots_case_insensitive() {
        let dir = unique_temp_dir("jpegs");
        touch(&dir.join("IMG_0001.JPG"));
        touch(&dir.join("IMG_0002.jpeg"));
        touch(&dir.join("IMG_0003.JpG"));

        let shots = scan(&dir).unwrap();
        assert_eq!(
            stems(&shots),
            vec![
                "IMG_0001".to_string(),
                "IMG_0002".to_string(),
                "IMG_0003".to_string()
            ]
        );
        // The display-file path is carried verbatim (on-disk case preserved).
        assert_eq!(shots[0].jpeg, dir.join("IMG_0001.JPG"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn non_image_files_are_ignored() {
        let dir = unique_temp_dir("nonimage");
        touch(&dir.join("IMG_0001.JPG"));
        touch(&dir.join("notes.txt"));
        touch(&dir.join("clip.mov"));
        touch(&dir.join(".fastcull.json")); // the session sidecar is not a shot

        let shots = scan(&dir).unwrap();
        assert_eq!(stems(&shots), vec!["IMG_0001".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn raw_only_stem_is_reported_and_is_not_a_shot() {
        let dir = unique_temp_dir("rawonly");
        touch(&dir.join("IMG_0001.JPG")); // a normal shot
        touch(&dir.join("IMG_0002.CR3")); // RAW with no JPEG sibling

        let (shots, raw_only) = scan_report(&dir).unwrap();
        assert_eq!(stems(&shots), vec!["IMG_0001".to_string()]);
        assert_eq!(raw_only, vec![dir.join("IMG_0002.CR3")]);

        // scan() hides the report and returns only the cullable shots.
        assert_eq!(stems(&scan(&dir).unwrap()), vec!["IMG_0001".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn jpeg_and_raw_sharing_a_stem_are_one_shot() {
        let dir = unique_temp_dir("pair");
        touch(&dir.join("IMG_0001.JPG"));
        touch(&dir.join("IMG_0001.CR3"));

        let (shots, raw_only) = scan_report(&dir).unwrap();
        assert_eq!(stems(&shots), vec!["IMG_0001".to_string()]);
        assert!(raw_only.is_empty()); // the RAW is paired, not orphaned
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn raw_sibling_is_attached_to_its_shot() {
        let dir = unique_temp_dir("rawsibling");
        touch(&dir.join("IMG_0001.JPG"));
        touch(&dir.join("IMG_0001.CR3"));

        let shots = scan(&dir).unwrap();
        assert_eq!(shots.len(), 1);
        assert_eq!(shots[0].raw, Some(dir.join("IMG_0001.CR3")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn raw_sibling_matches_case_insensitively() {
        let dir = unique_temp_dir("rawcase");
        touch(&dir.join("IMG_0001.jpg"));
        touch(&dir.join("IMG_0001.Nef")); // mixed-case RAW extension

        let shots = scan(&dir).unwrap();
        assert_eq!(shots.len(), 1);
        assert_eq!(shots[0].raw, Some(dir.join("IMG_0001.Nef")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn jpeg_without_raw_has_no_sibling() {
        let dir = unique_temp_dir("noraw");
        touch(&dir.join("IMG_0001.JPG"));
        let shots = scan(&dir).unwrap();
        assert_eq!(shots[0].raw, None);
        std::fs::remove_dir_all(&dir).ok();
    }
}
