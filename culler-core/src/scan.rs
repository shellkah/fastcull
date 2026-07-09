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
    sidecar: Option<PathBuf>,
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
        } else if ext == "xmp" {
            let stem = sidecar_stem(path);
            groups
                .entry(stem)
                .or_default()
                .sidecar
                .get_or_insert_with(|| path.clone());
        }
        // Everything else (videos, session sidecar, …) is unrecognized → ignored.
    }

    let mut shots: Vec<Shot> = Vec::new();
    let mut raw_only: Vec<PathBuf> = Vec::new();
    for (stem, group) in groups {
        match group.jpeg {
            Some(jpeg) => {
                let capture = read_capture_time(&jpeg);
                shots.push(Shot {
                    stem,
                    jpeg,
                    raw: group.raw,
                    sidecar: group.sidecar,
                    capture,
                });
            }
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

/// The shot stem an `.xmp` sidecar belongs to, handling both conventions:
///  - Adobe:     `IMG_1234.xmp`      → "IMG_1234"
///  - darktable: `IMG_1234.CR3.xmp`  → "IMG_1234" (strip the inner RAW/JPEG ext)
///
/// `file_stem` removes only the trailing ".xmp"; if what remains itself ends in
/// a recognized (case-insensitive) RAW or JPEG extension, that is stripped too.
fn sidecar_stem(path: &Path) -> String {
    let inner = match path.file_stem().and_then(|s| s.to_str()) {
        Some(s) => s,
        None => return String::new(),
    };
    let inner_path = Path::new(inner);
    if let Some(inner_ext) = inner_path.extension().and_then(|e| e.to_str()) {
        let inner_ext = inner_ext.to_ascii_lowercase();
        if RAW_EXTS.contains(&inner_ext.as_str()) || JPEG_EXTS.contains(&inner_ext.as_str()) {
            return inner_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
        }
    }
    inner.to_string()
}

/// Read `DateTimeOriginal` / `SubSecTimeOriginal` from a JPEG's EXIF header.
/// Undecodable or EXIF-less files never fail the scan — they yield a default
/// (empty) `CaptureTime`, so such shots simply sort after all dated ones.
fn read_capture_time(jpeg: &Path) -> CaptureTime {
    let file = match std::fs::File::open(jpeg) {
        Ok(f) => f,
        Err(_) => return CaptureTime::default(),
    };
    let mut reader = std::io::BufReader::new(file);
    let exif = match exif::Reader::new().read_from_container(&mut reader) {
        Ok(e) => e,
        Err(_) => return CaptureTime::default(),
    };
    let datetime = ascii_field(&exif, exif::Tag::DateTimeOriginal);
    let subsec = ascii_field(&exif, exif::Tag::SubSecTimeOriginal).and_then(|s| parse_subsec(&s));
    CaptureTime { datetime, subsec }
}

/// EXIF SubSecTime* is a DECIMAL FRACTION digit string, not an integer:
/// "5" means .5s and "05" means .05s. Normalize to milliseconds by
/// right-padding/truncating the digits to a fixed width of 3, so
/// mixed-width values order correctly ("5"→500, "05"→50, "123456"→123).
fn parse_subsec(s: &str) -> Option<u32> {
    let digits: String = s
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        return None;
    }
    let padded = format!("{digits:0<3}");
    padded[..3].parse::<u32>().ok()
}

/// The first ASCII string of `tag` in the primary IFD, trimmed. `None` when the
/// tag is absent or not an ASCII value. Returns the bytes verbatim (no reformat),
/// so `DateTimeOriginal` stays the lexically-sortable `"YYYY:MM:DD HH:MM:SS"`.
fn ascii_field(exif: &exif::Exif, tag: exif::Tag) -> Option<String> {
    let field = exif.get_field(tag, exif::In::PRIMARY)?;
    match &field.value {
        exif::Value::Ascii(vec) if !vec.is_empty() => {
            Some(String::from_utf8_lossy(&vec[0]).trim().to_string())
        }
        _ => None,
    }
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

    #[test]
    fn adobe_sidecar_is_detected() {
        let dir = unique_temp_dir("adobexmp");
        touch(&dir.join("IMG_0001.JPG"));
        touch(&dir.join("IMG_0001.xmp")); // Adobe convention: stem.xmp

        let shots = scan(&dir).unwrap();
        assert_eq!(shots.len(), 1);
        assert_eq!(shots[0].sidecar, Some(dir.join("IMG_0001.xmp")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn darktable_sidecar_is_detected() {
        let dir = unique_temp_dir("dtxmp");
        touch(&dir.join("IMG_0001.JPG"));
        touch(&dir.join("IMG_0001.CR3"));
        touch(&dir.join("IMG_0001.CR3.xmp")); // darktable convention: file.ext.xmp

        let shots = scan(&dir).unwrap();
        assert_eq!(shots.len(), 1);
        assert_eq!(shots[0].raw, Some(dir.join("IMG_0001.CR3")));
        assert_eq!(shots[0].sidecar, Some(dir.join("IMG_0001.CR3.xmp")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn darktable_sidecar_inner_ext_is_case_insensitive() {
        let dir = unique_temp_dir("dtxmpcase");
        touch(&dir.join("IMG_0001.JPG"));
        touch(&dir.join("IMG_0001.jpg.XMP")); // darktable sidecar for the JPEG, mixed case

        let shots = scan(&dir).unwrap();
        assert_eq!(shots.len(), 1);
        assert_eq!(shots[0].sidecar, Some(dir.join("IMG_0001.jpg.XMP")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn orphan_sidecar_without_jpeg_is_ignored() {
        let dir = unique_temp_dir("orphanxmp");
        touch(&dir.join("IMG_0001.xmp")); // no JPEG, no RAW

        let (shots, raw_only) = scan_report(&dir).unwrap();
        assert!(shots.is_empty());
        assert!(raw_only.is_empty()); // an orphan sidecar is neither a shot nor RAW-only
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Build a minimal but valid JPEG carrying an EXIF APP1 block (big-endian
    /// "MM" TIFF) with DateTimeOriginal and SubSecTimeOriginal. Enough for
    /// kamadak-exif to parse; it is not a real image. `subsec` must be ≤ 3 chars
    /// so the ASCII value (plus NUL) fits inline in the 4-byte IFD value field.
    fn jpeg_with_exif(datetime: &str, subsec: &str) -> Vec<u8> {
        fn be16(v: u16) -> [u8; 2] {
            v.to_be_bytes()
        }
        fn be32(v: u32) -> [u8; 4] {
            v.to_be_bytes()
        }

        let mut dt = datetime.as_bytes().to_vec();
        dt.push(0); // NUL-terminate
        let dt_count = dt.len() as u32;

        let mut ss = subsec.as_bytes().to_vec();
        ss.push(0);
        let ss_count = ss.len() as u32;
        assert!(ss_count <= 4, "subsec must fit inline (<= 3 chars)");

        // TIFF offsets, relative to the "MM" byte:
        //   0  TIFF header (8 bytes) → IFD0 at offset 8
        //   8  IFD0:      2 + 1*12 + 4 = 18 bytes → ends at 26
        //   26 Exif IFD:  2 + 2*12 + 4 = 30 bytes → ends at 56
        //   56 DateTimeOriginal string (dt_count bytes)
        const IFD0_OFF: u32 = 8;
        const EXIF_IFD_OFF: u32 = 26;
        const DT_STR_OFF: u32 = 56;

        let mut tiff: Vec<u8> = Vec::new();
        tiff.extend_from_slice(b"MM"); // big-endian byte order
        tiff.extend_from_slice(&be16(42)); // TIFF magic
        tiff.extend_from_slice(&be32(IFD0_OFF));

        // IFD0: one entry, ExifIFDPointer (0x8769, LONG) → EXIF_IFD_OFF
        tiff.extend_from_slice(&be16(1));
        tiff.extend_from_slice(&be16(0x8769));
        tiff.extend_from_slice(&be16(4)); // LONG
        tiff.extend_from_slice(&be32(1)); // count
        tiff.extend_from_slice(&be32(EXIF_IFD_OFF));
        tiff.extend_from_slice(&be32(0)); // no next IFD

        // Exif IFD: two entries
        tiff.extend_from_slice(&be16(2));
        // DateTimeOriginal (0x9003), ASCII → DT_STR_OFF
        tiff.extend_from_slice(&be16(0x9003));
        tiff.extend_from_slice(&be16(2)); // ASCII
        tiff.extend_from_slice(&be32(dt_count));
        tiff.extend_from_slice(&be32(DT_STR_OFF));
        // SubSecTimeOriginal (0x9291), ASCII, stored inline
        tiff.extend_from_slice(&be16(0x9291));
        tiff.extend_from_slice(&be16(2)); // ASCII
        tiff.extend_from_slice(&be32(ss_count));
        let mut inline = [0u8; 4];
        inline[..ss.len()].copy_from_slice(&ss);
        tiff.extend_from_slice(&inline);
        tiff.extend_from_slice(&be32(0)); // no next IFD

        // DateTimeOriginal string payload lands exactly at DT_STR_OFF.
        assert_eq!(tiff.len() as u32, DT_STR_OFF);
        tiff.extend_from_slice(&dt);

        // Wrap the TIFF in a JPEG APP1 "Exif" segment.
        let mut jpeg: Vec<u8> = Vec::new();
        jpeg.extend_from_slice(&[0xFF, 0xD8]); // SOI
        jpeg.extend_from_slice(&[0xFF, 0xE1]); // APP1
        let seg_len = (2 + 6 + tiff.len()) as u16; // len field + "Exif\0\0" + TIFF
        jpeg.extend_from_slice(&be16(seg_len));
        jpeg.extend_from_slice(b"Exif\0\0");
        jpeg.extend_from_slice(&tiff);
        jpeg.extend_from_slice(&[0xFF, 0xD9]); // EOI
        jpeg
    }

    #[test]
    fn reads_datetime_and_subsec_from_exif() {
        let dir = unique_temp_dir("exif");
        std::fs::write(
            dir.join("IMG_0001.JPG"),
            jpeg_with_exif("2026:07:08 10:11:12", "42"),
        )
        .unwrap();

        let shots = scan(&dir).unwrap();
        assert_eq!(shots.len(), 1);
        assert_eq!(
            shots[0].capture.datetime,
            Some("2026:07:08 10:11:12".to_string())
        );
        // "42" = 0.42s → right-padded to milliseconds: 420 (NOT integer 42).
        assert_eq!(shots[0].capture.subsec, Some(420));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn undecodable_jpeg_scans_with_default_capture() {
        let dir = unique_temp_dir("badexif");
        touch(&dir.join("IMG_0001.JPG")); // empty file → no EXIF, must not fail

        let shots = scan(&dir).unwrap();
        assert_eq!(shots.len(), 1);
        assert_eq!(shots[0].capture, CaptureTime::default());
        std::fs::remove_dir_all(&dir).ok();
    }
}
