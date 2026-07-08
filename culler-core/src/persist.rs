//! Session persistence: atomic JSON sidecar read/write for `.fastcull.json`.

use crate::model::Session;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum PersistError {
    Io(std::io::Error),
    Corrupt(serde_json::Error),
}

impl std::fmt::Display for PersistError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PersistError::Io(e) => write!(f, "session I/O error: {e}"),
            PersistError::Corrupt(e) => write!(f, "corrupt session file: {e}"),
        }
    }
}

impl std::error::Error for PersistError {}

/// Serialize `session` to `path` atomically: write `<path>.tmp` in the same
/// directory, then `rename` it over `path` (rename is atomic on the same FS).
pub fn save(session: &Session, path: &Path) -> Result<(), PersistError> {
    let json = serde_json::to_vec_pretty(session).map_err(PersistError::Corrupt)?;
    let tmp = tmp_path(path);
    std::fs::write(&tmp, &json).map_err(PersistError::Io)?;
    std::fs::rename(&tmp, path).map_err(PersistError::Io)?;
    Ok(())
}

/// Read and deserialize a `Session` from `path`. A missing file surfaces as
/// `PersistError::Io`; malformed JSON as `PersistError::Corrupt`.
pub fn load(path: &Path) -> Result<Session, PersistError> {
    let bytes = std::fs::read(path).map_err(PersistError::Io)?;
    serde_json::from_slice(&bytes).map_err(PersistError::Corrupt)
}

/// `<path>.tmp` in the same directory, so the subsequent rename stays same-FS.
fn tmp_path(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_owned();
    os.push(".tmp");
    PathBuf::from(os)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CaptureTime, Decision, Session, Shot, Tier, SESSION_FILE};
    use std::path::PathBuf;

    /// A fresh, unique temp directory for a single test.
    fn unique_temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "fastcull-{tag}-{}-{nanos}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = unique_temp_dir("roundtrip");
        let path = dir.join(SESSION_FILE);

        let mut session = Session {
            source_dir: dir.clone(),
            shots: vec![Shot {
                stem: "IMG_0001".to_string(),
                jpeg: dir.join("IMG_0001.JPG"),
                raw: Some(dir.join("IMG_0001.CR3")),
                sidecar: None,
                capture: CaptureTime {
                    datetime: Some("2026:07:08 10:00:00".to_string()),
                    subsec: Some(1),
                },
            }],
            decisions: std::collections::HashMap::new(),
            current: 0,
            undo: Vec::new(),
        };
        session.decisions.insert(
            "IMG_0001".to_string(),
            Decision {
                tier: Some(Tier::Pick),
                tags: vec!["portrait".to_string()],
                visited: true,
            },
        );

        save(&session, &path).unwrap();
        assert!(path.exists());
        // Atomic write leaves no dangling temp file behind.
        assert!(!dir.join(".fastcull.json.tmp").exists());

        let loaded = load(&path).unwrap();
        assert_eq!(loaded.source_dir, session.source_dir);
        assert_eq!(loaded.shots, session.shots);
        assert_eq!(loaded.decisions, session.decisions);
        assert_eq!(loaded.current, 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_reports_corrupt_json_as_corrupt_error() {
        let dir = unique_temp_dir("loadcorrupt");
        let path = dir.join(SESSION_FILE);
        std::fs::write(&path, b"{ not valid json ").unwrap();

        match load(&path) {
            Err(PersistError::Corrupt(_)) => {}
            other => panic!("expected Corrupt, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_missing_file_is_an_io_error() {
        let dir = unique_temp_dir("loadmissing");
        let path = dir.join(SESSION_FILE);
        match load(&path) {
            Err(PersistError::Io(e)) => {
                assert_eq!(e.kind(), std::io::ErrorKind::NotFound);
            }
            other => panic!("expected Io(NotFound), got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
