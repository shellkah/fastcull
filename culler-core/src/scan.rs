//! Folder scan: a flat (non-recursive) walk that groups files by filename stem
//! into `Shot`s and sorts them into a stable, capture-time filmstrip order.
//!
//! A shot needs a JPEG or a previewable RAW (Fuji RAF today, via its embedded
//! JPEG). A RAW-only stem with no embedded-preview extractor (CR3/NEF/…) is
//! not a cullable shot — it is surfaced separately by `scan_report`.
//! Zero GUI dependencies.

use crate::model::{CaptureTime, ExifSummary, JPEG_EXTS, RAW_EXTS, Shot};
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
///
/// Entries whose file names are not valid UTF-8 are skipped (stems key
/// decisions; such files surface via the Apply preview's leftover diff).
pub fn scan_report(dir: &Path) -> Result<(Vec<Shot>, Vec<PathBuf>), ScanError> {
    if !dir.is_dir() {
        return Err(ScanError::NotADir(dir.to_path_buf()));
    }

    let mut entries: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(ScanError::Io)? {
        let entry = entry.map_err(ScanError::Io)?;
        let path = entry.path();
        // Keep only regular files (metadata follows symlinks): directories
        // (flat walk), FIFOs, sockets and device nodes must never reach the
        // EXIF reader — opening a FIFO for read would block the scan forever.
        // A per-entry stat error (e.g. a broken symlink) skips that entry
        // rather than failing the whole scan.
        match std::fs::metadata(&path) {
            Ok(m) if m.is_file() => {}
            _ => continue,
        }
        // Grouping is keyed by UTF-8 stems (Decision keys in the session
        // JSON); a file name that is not valid UTF-8 cannot carry a faithful
        // stem key, so the entry is skipped here. Such files are not lost to
        // the user: they surface as generic "stays behind" leftovers in the
        // Apply preview's readdir diff.
        if path.file_name().and_then(|n| n.to_str()).is_none() {
            continue;
        }
        entries.push(path);
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
                let (capture, exif) = read_exif_data(&jpeg);
                shots.push(Shot {
                    stem,
                    jpeg: Some(jpeg),
                    raw: group.raw,
                    sidecar: group.sidecar,
                    capture,
                    exif,
                });
            }
            None => {
                match group.raw {
                    // A previewable RAW-only stem (Fuji RAF) is a cullable shot,
                    // shown via its embedded JPEG. Capture/EXIF come from that JPEG.
                    Some(raw) if raw_ext_supported(&raw) => {
                        let (capture, exif) = read_exif_data_raf(&raw);
                        shots.push(Shot {
                            stem,
                            jpeg: None,
                            raw: Some(raw),
                            sidecar: group.sidecar,
                            capture,
                            exif,
                        });
                    }
                    // Non-previewable RAW (CR3/NEF/…): report it, unchanged.
                    Some(raw) => raw_only.push(raw),
                    // Orphan sidecar with neither JPEG nor RAW — dropped, as before.
                    None => {}
                }
            }
        }
    }
    sort_shots(&mut shots);
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

/// True when `path`'s extension has an embedded-preview extractor (Fuji RAF today).
fn raw_ext_supported(path: &Path) -> bool {
    ext_lower(path)
        .as_deref()
        .is_some_and(crate::raw::preview_supported)
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

/// Read capture time + HUD summary from a JPEG file's EXIF (single parse).
/// Undecodable/EXIF-less files never fail the scan — default `CaptureTime` + `None`.
fn read_exif_data(jpeg: &Path) -> (CaptureTime, Option<ExifSummary>) {
    let file = match std::fs::File::open(jpeg) {
        Ok(f) => f,
        Err(_) => return (CaptureTime::default(), None),
    };
    let mut reader = std::io::BufReader::new(file);
    match exif::Reader::new().read_from_container(&mut reader) {
        Ok(exif) => capture_and_summary(&exif),
        Err(_) => (CaptureTime::default(), None),
    }
}

/// Same as `read_exif_data` but for a Fuji RAF: the EXIF lives inside the
/// embedded JPEG (kamadak-exif can't read the RAF container), so extract that
/// first. Any failure degrades to default `CaptureTime` + `None`.
fn read_exif_data_raf(raf: &Path) -> (CaptureTime, Option<ExifSummary>) {
    let data = match std::fs::read(raf) {
        Ok(d) => d,
        Err(_) => return (CaptureTime::default(), None),
    };
    let jpeg = match crate::raw::embedded_jpeg(&data) {
        Some(j) => j,
        None => return (CaptureTime::default(), None),
    };
    let mut cursor = std::io::Cursor::new(jpeg);
    match exif::Reader::new().read_from_container(&mut cursor) {
        Ok(exif) => capture_and_summary(&exif),
        Err(_) => (CaptureTime::default(), None),
    }
}

/// Capture time + `ExifSummary` from an already-parsed `Exif` (shared by the
/// JPEG and RAF readers — one parse each).
fn capture_and_summary(exif: &exif::Exif) -> (CaptureTime, Option<ExifSummary>) {
    let datetime = ascii_field(exif, exif::Tag::DateTimeOriginal);
    let subsec = ascii_field(exif, exif::Tag::SubSecTimeOriginal).and_then(|s| parse_subsec(&s));
    (CaptureTime { datetime, subsec }, read_exif_summary(exif))
}

/// Build the loupe HUD's `ExifSummary` from an already-parsed `exif::Exif`
/// (shared with capture-time parsing above — no second file read/parse).
/// `None` when none of the four tags parsed (e.g. a JPEG whose EXIF block
/// carries no exposure data at all, or a JPEG with no EXIF, which never
/// reaches this function in the first place). Never panics on a missing or
/// oddly-typed tag — each field independently degrades to `None`.
fn read_exif_summary(exif: &exif::Exif) -> Option<ExifSummary> {
    let exposure = rational_field(exif, exif::Tag::ExposureTime);
    let f_number = rational_field_f32(exif, exif::Tag::FNumber);
    let iso = uint_field(exif, exif::Tag::PhotographicSensitivity);
    let focal_length_mm = rational_field_f32(exif, exif::Tag::FocalLength);

    if exposure.is_none() && f_number.is_none() && iso.is_none() && focal_length_mm.is_none() {
        None
    } else {
        Some(ExifSummary {
            exposure,
            f_number,
            iso,
            focal_length_mm,
        })
    }
}

/// The first RATIONAL value of `tag` in the primary IFD, as the raw
/// `(numerator, denominator)` pair. `None` when the tag is absent, not a
/// `Value::Rational`, or the vector is empty.
fn rational_field(exif: &exif::Exif, tag: exif::Tag) -> Option<(u32, u32)> {
    let field = exif.get_field(tag, exif::In::PRIMARY)?;
    match &field.value {
        exif::Value::Rational(v) => v.first().map(|r| (r.num, r.denom)),
        _ => None,
    }
}

/// `rational_field` narrowed to `f32`, for tags the HUD renders as a decimal
/// (FNumber, FocalLength). A zero denominator (an odd/malformed EXIF field)
/// is treated as absent rather than dividing into an infinite or NaN value.
fn rational_field_f32(exif: &exif::Exif, tag: exif::Tag) -> Option<f32> {
    rational_field(exif, tag).and_then(|(num, den)| {
        if den == 0 {
            None
        } else {
            Some(num as f32 / den as f32)
        }
    })
}

/// The first unsigned-integer value (BYTE/SHORT/LONG) of `tag` in the primary
/// IFD, widened to `u32`. `None` when the tag is absent or not one of those
/// integer types (mirrors `decode::orientation_from`'s use of `get_uint`).
fn uint_field(exif: &exif::Exif, tag: exif::Tag) -> Option<u32> {
    exif.get_field(tag, exif::In::PRIMARY)?.value.get_uint(0)
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

/// Stable filmstrip order: by (capture.datetime, capture.subsec, jpeg filename).
/// Shots with no datetime sort AFTER all dated shots, then by filename, so burst
/// order stays put across sessions.
fn sort_shots(shots: &mut [Shot]) {
    shots.sort_by_key(sort_key);
}

/// Ordering key. The leading bool puts dated shots (`false`) before undated
/// (`true`); `Option<u32>`/`String` then order within each group.
fn sort_key(shot: &Shot) -> (bool, Option<String>, Option<u32>, String) {
    (
        shot.capture.datetime.is_none(),
        shot.capture.datetime.clone(),
        shot.capture.subsec,
        file_name_string(shot.display_path()),
    )
}

/// The final path component (with extension) as an owned `String`.
fn file_name_string(path: &Path) -> String {
    path.file_name()
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

    #[cfg(unix)]
    #[test]
    fn fifo_named_like_a_jpeg_is_skipped_not_opened() {
        let dir = unique_temp_dir("fifo");
        touch(&dir.join("IMG_0001.JPG"));
        let fifo = dir.join("IMG_0002.jpg");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo must run");
        assert!(status.success());

        // Must return promptly: opening the FIFO for read would block forever.
        let shots = scan(&dir).unwrap();
        assert_eq!(stems(&shots), vec!["IMG_0001".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_file_names_are_skipped_without_contaminating_shots() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let dir = unique_temp_dir("nonutf8");
        touch(&dir.join("IMG_0001.JPG")); // a normal shot
        // Files whose stems are NOT valid UTF-8 (0xFF byte).
        for name in [
            b"IMG_\xffA.jpg".to_vec(),
            b"IMG_\xffB.jpg".to_vec(),
            b"IMG_\xffC.cr3".to_vec(),
        ] {
            touch(&dir.join(OsString::from_vec(name)));
        }

        let (shots, raw_only) = scan_report(&dir).unwrap();
        // No shot may be produced from the non-UTF-8 names — in particular no
        // empty-stem Shot swallowing them all; the valid shot is untouched.
        assert_eq!(stems(&shots), vec!["IMG_0001".to_string()]);
        assert!(raw_only.is_empty());
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
    fn duplicate_stem_display_files_keep_first_in_path_order() {
        let dir = unique_temp_dir("dupstem");
        touch(&dir.join("IMG_0001.jpg"));
        touch(&dir.join("IMG_0001.jpeg")); // sorts before IMG_0001.jpg

        let shots = scan(&dir).unwrap();
        assert_eq!(shots.len(), 1);
        // Deterministic first-wins (plan's documented Known edge): the
        // sorted-path-order winner claims the display slot regardless of
        // readdir order.
        assert_eq!(shots[0].jpeg, Some(dir.join("IMG_0001.jpeg")));
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
        assert_eq!(shots[0].jpeg, Some(dir.join("IMG_0001.JPG")));
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

    /// Wrap JPEG bytes (e.g. from `jpeg_with_exif`) in a minimal Fuji RAF container.
    fn wrap_raf(jpeg: &[u8]) -> Vec<u8> {
        let off = 128usize;
        let mut raf = Vec::new();
        raf.extend_from_slice(b"FUJIFILMCCD-RAW ");
        raf.resize(off, 0);
        raf[84..88].copy_from_slice(&(off as u32).to_be_bytes());
        raf[88..92].copy_from_slice(&(jpeg.len() as u32).to_be_bytes());
        raf.extend_from_slice(jpeg);
        raf
    }

    #[test]
    fn raf_only_stem_is_promoted_with_embedded_capture_time() {
        let dir = unique_temp_dir("rafonly");
        std::fs::write(
            dir.join("DSCF0001.RAF"),
            wrap_raf(&jpeg_with_exif("2026:07:08 10:11:12", "42")),
        )
        .unwrap();

        let (shots, raw_only) = scan_report(&dir).unwrap();
        assert_eq!(stems(&shots), vec!["DSCF0001".to_string()]);
        assert!(
            raw_only.is_empty(),
            "a previewable RAF-only stem is a shot, not raw_only"
        );
        assert_eq!(shots[0].jpeg, None);
        assert_eq!(shots[0].raw, Some(dir.join("DSCF0001.RAF")));
        assert_eq!(
            shots[0].capture.datetime,
            Some("2026:07:08 10:11:12".to_string())
        );
        assert_eq!(shots[0].capture.subsec, Some(420));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cr3_only_stem_stays_raw_only_not_a_shot() {
        let dir = unique_temp_dir("cr3only");
        touch(&dir.join("IMG_0001.CR3")); // no previewable extractor -> unchanged behavior
        let (shots, raw_only) = scan_report(&dir).unwrap();
        assert!(shots.is_empty());
        assert_eq!(raw_only, vec![dir.join("IMG_0001.CR3")]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn jpeg_and_raf_sharing_a_stem_are_one_shot() {
        let dir = unique_temp_dir("rafpair");
        touch(&dir.join("DSCF0002.JPG"));
        std::fs::write(
            dir.join("DSCF0002.RAF"),
            wrap_raf(&jpeg_with_exif("2026:07:08 09:00:00", "10")),
        )
        .unwrap();
        let (shots, raw_only) = scan_report(&dir).unwrap();
        assert_eq!(stems(&shots), vec!["DSCF0002".to_string()]);
        assert!(raw_only.is_empty());
        assert_eq!(shots[0].raw, Some(dir.join("DSCF0002.RAF")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn raf_only_shot_sorts_by_embedded_capture_time() {
        let dir = unique_temp_dir("rafsort");
        std::fs::write(
            dir.join("B_late.RAF"),
            wrap_raf(&jpeg_with_exif("2026:07:08 10:00:00", "00")),
        )
        .unwrap();
        std::fs::write(
            dir.join("A_early.RAF"),
            wrap_raf(&jpeg_with_exif("2026:07:08 09:00:00", "00")),
        )
        .unwrap();
        let shots = scan(&dir).unwrap();
        assert_eq!(
            stems(&shots),
            vec!["A_early".to_string(), "B_late".to_string()]
        );
        std::fs::remove_dir_all(&dir).ok();
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
        // No EXIF at all -> no exif summary either.
        assert_eq!(shots[0].exif, None);
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- ExifSummary (exposure / f-number / ISO / focal length) ----

    /// One Exif-IFD entry in TIFF wire format: `data.len()` must equal
    /// `count * unit_size(type_code)`.
    struct RawEntry {
        tag: u16,
        type_code: u16,
        count: u32,
        data: Vec<u8>,
    }

    fn ascii_entry(tag: u16, s: &str) -> RawEntry {
        let mut data = s.as_bytes().to_vec();
        data.push(0); // NUL-terminate
        RawEntry {
            tag,
            type_code: 2, // ASCII
            count: data.len() as u32,
            data,
        }
    }

    fn short_entry(tag: u16, v: u16) -> RawEntry {
        RawEntry {
            tag,
            type_code: 3, // SHORT
            count: 1,
            data: v.to_be_bytes().to_vec(),
        }
    }

    fn rational_entry(tag: u16, num: u32, denom: u32) -> RawEntry {
        let mut data = num.to_be_bytes().to_vec();
        data.extend_from_slice(&denom.to_be_bytes());
        RawEntry {
            tag,
            type_code: 5, // RATIONAL
            count: 1,
            data,
        }
    }

    /// Build a minimal big-endian TIFF/EXIF blob: IFD0 holds a single
    /// `ExifIFDPointer` entry pointing at an Exif IFD containing `entries`
    /// (caller supplies them in ascending tag order, matching a real
    /// encoder — kamadak-exif itself does not require this on read, but this
    /// keeps the fixture realistic). Entries whose data is `<= 4` bytes are
    /// stored inline (left-justified, zero-padded) in the IFD entry itself,
    /// exactly like the TIFF spec's Value/Offset field; larger entries
    /// (RATIONAL's 8 bytes) are placed in an external block right after the
    /// Exif IFD, with their offsets computed automatically — no hand-derived
    /// magic numbers to keep in sync when the entry set changes.
    fn build_tiff(entries: &[RawEntry]) -> Vec<u8> {
        fn be16(v: u16) -> [u8; 2] {
            v.to_be_bytes()
        }
        fn be32(v: u32) -> [u8; 4] {
            v.to_be_bytes()
        }

        const IFD0_OFF: u32 = 8;
        let exif_ifd_off: u32 = IFD0_OFF + 2 + 12 + 4; // IFD0: 1 entry, no next-IFD data
        let exif_ifd_len: u32 = 2 + 12 * entries.len() as u32 + 4;
        let external_start: u32 = exif_ifd_off + exif_ifd_len;

        // First pass: assign every >4-byte entry an offset in the external block.
        let mut external: Vec<u8> = Vec::new();
        let mut offsets: Vec<u32> = Vec::with_capacity(entries.len());
        for e in entries {
            if e.data.len() <= 4 {
                offsets.push(0); // unused for inline entries
            } else {
                offsets.push(external_start + external.len() as u32);
                external.extend_from_slice(&e.data);
            }
        }

        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"MM"); // big-endian byte order
        tiff.extend_from_slice(&be16(42)); // TIFF magic
        tiff.extend_from_slice(&be32(IFD0_OFF));

        // IFD0: one entry, ExifIFDPointer (0x8769, LONG) -> exif_ifd_off.
        tiff.extend_from_slice(&be16(1));
        tiff.extend_from_slice(&be16(0x8769));
        tiff.extend_from_slice(&be16(4)); // LONG
        tiff.extend_from_slice(&be32(1)); // count
        tiff.extend_from_slice(&be32(exif_ifd_off));
        tiff.extend_from_slice(&be32(0)); // no next IFD
        assert_eq!(tiff.len() as u32, exif_ifd_off, "IFD0 layout drifted");

        // Exif IFD.
        tiff.extend_from_slice(&be16(entries.len() as u16));
        for (e, &off) in entries.iter().zip(&offsets) {
            tiff.extend_from_slice(&be16(e.tag));
            tiff.extend_from_slice(&be16(e.type_code));
            tiff.extend_from_slice(&be32(e.count));
            if e.data.len() <= 4 {
                let mut inline = [0u8; 4];
                inline[..e.data.len()].copy_from_slice(&e.data);
                tiff.extend_from_slice(&inline);
            } else {
                tiff.extend_from_slice(&be32(off));
            }
        }
        tiff.extend_from_slice(&be32(0)); // no next IFD
        assert_eq!(tiff.len() as u32, external_start, "Exif IFD layout drifted");

        tiff.extend_from_slice(&external);
        tiff
    }

    /// Wrap a TIFF/EXIF blob in a JPEG SOI + APP1 "Exif" segment + EOI.
    fn wrap_jpeg_exif(tiff: &[u8]) -> Vec<u8> {
        let mut jpeg: Vec<u8> = Vec::new();
        jpeg.extend_from_slice(&[0xFF, 0xD8]); // SOI
        jpeg.extend_from_slice(&[0xFF, 0xE1]); // APP1
        let seg_len = (2 + 6 + tiff.len()) as u16;
        jpeg.extend_from_slice(&seg_len.to_be_bytes());
        jpeg.extend_from_slice(b"Exif\0\0");
        jpeg.extend_from_slice(tiff);
        jpeg.extend_from_slice(&[0xFF, 0xD9]); // EOI
        jpeg
    }

    #[test]
    fn exif_summary_parses_all_four_fields() {
        let dir = unique_temp_dir("exifsummary");
        let entries = vec![
            rational_entry(0x829a, 1, 250),   // ExposureTime = 1/250s
            rational_entry(0x829d, 28, 10),   // FNumber = 2.8
            short_entry(0x8827, 400),         // PhotographicSensitivity = ISO 400
            rational_entry(0x920a, 850, 10),  // FocalLength = 85.0mm
        ];
        std::fs::write(dir.join("IMG_0001.JPG"), wrap_jpeg_exif(&build_tiff(&entries))).unwrap();

        let shots = scan(&dir).unwrap();
        assert_eq!(shots.len(), 1);
        assert_eq!(
            shots[0].exif,
            Some(crate::model::ExifSummary {
                exposure: Some((1, 250)),
                f_number: Some(2.8),
                iso: Some(400),
                focal_length_mm: Some(85.0),
            })
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn exif_summary_partial_fields_present() {
        let dir = unique_temp_dir("exifpartial");
        // Only FNumber and ISO present; ExposureTime and FocalLength absent.
        let entries = vec![rational_entry(0x829d, 18, 10), short_entry(0x8827, 100)];
        std::fs::write(dir.join("IMG_0001.JPG"), wrap_jpeg_exif(&build_tiff(&entries))).unwrap();

        let shots = scan(&dir).unwrap();
        assert_eq!(
            shots[0].exif,
            Some(crate::model::ExifSummary {
                exposure: None,
                f_number: Some(1.8),
                iso: Some(100),
                focal_length_mm: None,
            })
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn exif_summary_coexists_with_capture_time_from_one_parse() {
        let dir = unique_temp_dir("exifboth");
        let entries = vec![
            rational_entry(0x829a, 1, 125), // ExposureTime = 1/125s
            short_entry(0x8827, 200),       // ISO 200
            ascii_entry(0x9003, "2026:07:08 10:11:12"), // DateTimeOriginal
        ];
        std::fs::write(dir.join("IMG_0001.JPG"), wrap_jpeg_exif(&build_tiff(&entries))).unwrap();

        let shots = scan(&dir).unwrap();
        assert_eq!(
            shots[0].capture.datetime,
            Some("2026:07:08 10:11:12".to_string())
        );
        assert_eq!(
            shots[0].exif,
            Some(crate::model::ExifSummary {
                exposure: Some((1, 125)),
                f_number: None,
                iso: Some(200),
                focal_length_mm: None,
            })
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn exif_summary_none_when_exif_present_but_no_relevant_tags() {
        let dir = unique_temp_dir("exifirrelevant");
        // A real EXIF block, but none of the four summary tags — just a
        // DateTimeOriginal (already covered by `reads_datetime_and_subsec_from_exif`;
        // the point here is that `exif` on the Shot stays `None`).
        let entries = vec![ascii_entry(0x9003, "2026:07:08 10:11:12")];
        std::fs::write(dir.join("IMG_0001.JPG"), wrap_jpeg_exif(&build_tiff(&entries))).unwrap();

        let shots = scan(&dir).unwrap();
        assert_eq!(
            shots[0].capture.datetime,
            Some("2026:07:08 10:11:12".to_string())
        );
        assert_eq!(shots[0].exif, None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn exif_summary_odd_typed_tag_degrades_to_none_without_panicking() {
        let dir = unique_temp_dir("exifoddtype");
        // ExposureTime encoded as ASCII instead of RATIONAL (malformed/odd
        // EXIF) alongside a well-formed ISO — the odd tag must degrade to
        // `None` for that one field, not panic and not poison the rest.
        let entries = vec![ascii_entry(0x829a, "not-a-rational"), short_entry(0x8827, 800)];
        std::fs::write(dir.join("IMG_0001.JPG"), wrap_jpeg_exif(&build_tiff(&entries))).unwrap();

        let shots = scan(&dir).unwrap();
        assert_eq!(
            shots[0].exif,
            Some(crate::model::ExifSummary {
                exposure: None,
                f_number: None,
                iso: Some(800),
                focal_length_mm: None,
            })
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn exif_summary_zero_denominator_rational_degrades_to_none() {
        let dir = unique_temp_dir("exifzeroden");
        // A malformed FNumber with denominator 0 must not panic or produce
        // an infinite/NaN f32 — it degrades to `None` for that field.
        let entries = vec![rational_entry(0x829d, 5, 0), short_entry(0x8827, 100)];
        std::fs::write(dir.join("IMG_0001.JPG"), wrap_jpeg_exif(&build_tiff(&entries))).unwrap();

        let shots = scan(&dir).unwrap();
        assert_eq!(
            shots[0].exif,
            Some(crate::model::ExifSummary {
                exposure: None,
                f_number: None,
                iso: Some(100),
                focal_length_mm: None,
            })
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_subsec_normalizes_fraction_digits() {
        // EXIF subsec is a decimal FRACTION: right-pad/truncate to milliseconds.
        assert_eq!(parse_subsec("5"), Some(500));
        assert_eq!(parse_subsec("05"), Some(50));
        assert_eq!(parse_subsec("42"), Some(420));
        assert_eq!(parse_subsec("123"), Some(123));
        assert_eq!(parse_subsec("123456"), Some(123));
        // "9" = 0.9s must order AFTER "10" = 0.10s once normalized.
        assert!(parse_subsec("9") > parse_subsec("10"));
        // Non-digit / empty inputs yield None; leading digits before junk still parse.
        assert_eq!(parse_subsec(""), None);
        assert_eq!(parse_subsec("abc"), None);
        assert_eq!(parse_subsec(" 42 "), Some(420));
    }

    #[test]
    fn shots_sort_by_capture_time_with_stable_tiebreakers() {
        let dir = unique_temp_dir("sort");

        // Same datetime AND subsec → filename tiebreak (a_same < b_same).
        std::fs::write(
            dir.join("a_same.jpg"),
            jpeg_with_exif("2026:07:08 09:00:00", "20"),
        )
        .unwrap();
        std::fs::write(
            dir.join("b_same.jpg"),
            jpeg_with_exif("2026:07:08 09:00:00", "20"),
        )
        .unwrap();
        // Same datetime, different subsec → subsec tiebreak (lo < hi).
        std::fs::write(
            dir.join("sub_hi.jpg"),
            jpeg_with_exif("2026:07:08 09:00:01", "50"),
        )
        .unwrap();
        std::fs::write(
            dir.join("sub_lo.jpg"),
            jpeg_with_exif("2026:07:08 09:00:01", "05"),
        )
        .unwrap();
        // Latest dated shot.
        std::fs::write(
            dir.join("late.jpg"),
            jpeg_with_exif("2026:07:08 10:00:00", "00"),
        )
        .unwrap();
        // Undated shots → after all dated, then by filename.
        touch(&dir.join("undated_a.jpg"));
        touch(&dir.join("undated_b.jpg"));

        let shots = scan(&dir).unwrap();
        assert_eq!(
            stems(&shots),
            vec![
                "a_same".to_string(),
                "b_same".to_string(),
                "sub_lo".to_string(),
                "sub_hi".to_string(),
                "late".to_string(),
                "undated_a".to_string(),
                "undated_b".to_string(),
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_subsec_sorts_before_present_subsec_at_same_datetime() {
        let dir = unique_temp_dir("subsecnone");
        // Same datetime; "z_nosub" has NO usable subsec (empty SubSecTimeOriginal
        // → None), "a_sub" has one. Option<u32> orders None < Some, so z_nosub
        // must come FIRST despite its later filename — proving the subsec field
        // (not the filename) decides this tiebreak, deterministically.
        std::fs::write(
            dir.join("z_nosub.jpg"),
            jpeg_with_exif("2026:07:08 09:00:00", ""),
        )
        .unwrap();
        std::fs::write(
            dir.join("a_sub.jpg"),
            jpeg_with_exif("2026:07:08 09:00:00", "10"),
        )
        .unwrap();

        let shots = scan(&dir).unwrap();
        assert_eq!(shots[0].capture.subsec, None);
        assert_eq!(shots[1].capture.subsec, Some(100));
        assert_eq!(
            stems(&shots),
            vec!["z_nosub".to_string(), "a_sub".to_string()]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_report_itself_returns_shots_in_capture_time_order() {
        let dir = unique_temp_dir("reportsort");
        // BTreeMap stem order would be (a_late, b_early); capture order is the
        // reverse — this must hold for scan_report itself, not just scan().
        std::fs::write(
            dir.join("a_late.jpg"),
            jpeg_with_exif("2026:07:08 10:00:00", "00"),
        )
        .unwrap();
        std::fs::write(
            dir.join("b_early.jpg"),
            jpeg_with_exif("2026:07:08 09:00:00", "00"),
        )
        .unwrap();

        let (shots, _raw_only) = scan_report(&dir).unwrap();
        assert_eq!(
            stems(&shots),
            vec!["b_early".to_string(), "a_late".to_string()]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn undated_shots_alone_sort_by_filename() {
        let dir = unique_temp_dir("undated");
        touch(&dir.join("IMG_0003.JPG"));
        touch(&dir.join("IMG_0001.JPG"));
        touch(&dir.join("IMG_0002.JPG"));

        let shots = scan(&dir).unwrap();
        assert_eq!(
            stems(&shots),
            vec![
                "IMG_0001".to_string(),
                "IMG_0002".to_string(),
                "IMG_0003".to_string()
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
