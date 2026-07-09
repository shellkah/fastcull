//! `FsOps`: the narrow filesystem surface the apply engine drives, so tests can
//! inject EXDEV / ENOSPC / permission / collision faults deterministically.
//! `RealFs` is the production impl over rustix; `FakeFs` (test-only, Task 2)
//! is the in-memory fault-injection double.

use std::io;
use std::path::Path;

/// Every dangerous filesystem effect the apply engine performs goes through here.
pub trait FsOps {
    /// Recursively create `path` and ancestors (idempotent).
    fn mkdir_p(&self, path: &Path) -> io::Result<()>;
    /// True if `a` and `b` live on the same filesystem (same `st_dev`).
    fn same_filesystem(&self, a: &Path, b: &Path) -> io::Result<bool>;
    /// `renameat2(RENAME_NOREPLACE)`: atomic move that NEVER clobbers — `EEXIST` if `to` exists.
    fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// Create `to` with `O_CREAT|O_EXCL`, stream-copy `from` into it; returns bytes copied.
    fn copy_create_new(&self, from: &Path, to: &Path) -> io::Result<u64>;
    /// fsync the file at `path` (durability of copied data).
    fn fsync_file(&self, path: &Path) -> io::Result<()>;
    /// fsync the directory at `path` (durability of a new directory entry).
    fn fsync_dir(&self, path: &Path) -> io::Result<()>;
    /// Unlink a single file.
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    /// Byte length of the file at `path`.
    fn file_len(&self, path: &Path) -> io::Result<u64>;
    /// Bytes available to an unprivileged user at `path` (`statvfs`: `f_bavail * f_frsize`).
    fn free_space(&self, path: &Path) -> io::Result<u64>;
}

/// Production `FsOps` over the real Linux filesystem via rustix.
pub struct RealFs;

impl FsOps for RealFs {
    fn mkdir_p(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn same_filesystem(&self, a: &Path, b: &Path) -> io::Result<bool> {
        Ok(dev_of(a)? == dev_of(b)?)
    }

    fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
        use rustix::fs::{CWD, RenameFlags, renameat_with};
        renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE).map_err(io::Error::from)
    }

    fn copy_create_new(&self, from: &Path, to: &Path) -> io::Result<u64> {
        let mut src = std::fs::File::open(from)?;
        let mut dst = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true) // O_CREAT | O_EXCL
            .open(to)?;
        std::io::copy(&mut src, &mut dst)
    }

    fn fsync_file(&self, path: &Path) -> io::Result<()> {
        std::fs::File::open(path)?.sync_all()
    }

    fn fsync_dir(&self, path: &Path) -> io::Result<()> {
        // Opening a directory read-only then fsync flushes its entries on Linux.
        std::fs::File::open(path)?.sync_all()
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn file_len(&self, path: &Path) -> io::Result<u64> {
        Ok(std::fs::metadata(path)?.len())
    }

    fn free_space(&self, path: &Path) -> io::Result<u64> {
        let s = rustix::fs::statvfs(path).map_err(io::Error::from)?;
        Ok(s.f_bavail as u64 * s.f_frsize as u64)
    }
}

/// Device id of the filesystem containing `p` (stat its containing directory, per spec §8).
fn dev_of(p: &Path) -> io::Result<u64> {
    let target = p
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(p);
    let st = rustix::fs::stat(target).map_err(io::Error::from)?;
    Ok(st.st_dev as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io;

    #[test]
    fn real_rename_noreplace_moves_then_refuses_to_clobber() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.txt");
        let dst = dir.path().join("b.txt");
        fs::write(&src, b"hello").unwrap();

        let fs_ops = RealFs;

        // Happy path: atomic no-clobber rename onto a free name.
        fs_ops.rename_noreplace(&src, &dst).unwrap();
        assert!(!src.exists(), "source consumed by rename");
        assert_eq!(fs::read(&dst).unwrap(), b"hello");
        assert_eq!(fs_ops.file_len(&dst).unwrap(), 5);

        // Re-create the source and try to clobber the now-existing dst: must fail EEXIST.
        fs::write(&src, b"world").unwrap();
        let err = fs_ops.rename_noreplace(&src, &dst).unwrap_err();
        assert_eq!(
            err.kind(),
            io::ErrorKind::AlreadyExists,
            "NOREPLACE must not clobber"
        );
        assert!(src.exists(), "source untouched on refusal");
        assert_eq!(fs::read(&dst).unwrap(), b"hello", "dst unchanged");
    }

    #[test]
    fn real_same_filesystem_and_free_space() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();
        let fs_ops = RealFs;
        assert!(
            fs_ops.same_filesystem(&a, &b).unwrap(),
            "two files in one dir share a FS"
        );
        assert!(
            fs_ops.free_space(dir.path()).unwrap() > 0,
            "statvfs reports free space"
        );
    }

    #[test]
    fn real_copy_create_new_streams_and_refuses_existing() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        let dst = dir.path().join("dst.bin");
        fs::write(&src, vec![7u8; 4096]).unwrap();
        let fs_ops = RealFs;

        let n = fs_ops.copy_create_new(&src, &dst).unwrap();
        assert_eq!(n, 4096);
        assert_eq!(fs::read(&dst).unwrap().len(), 4096);
        assert!(src.exists(), "copy leaves source in place");

        // O_EXCL: copying onto an existing dst must fail.
        let err = fs_ops.copy_create_new(&src, &dst).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    }
}
