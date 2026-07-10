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

    use super::fake::FakeFs; // NB: `use fake::…` would not resolve inside `mod tests`
    use std::path::PathBuf;

    #[test]
    fn fake_rename_moves_entry_and_injects_faults() {
        let fs = FakeFs::new();
        fs.seed_file("/src/a.jpg", 10);

        // Happy rename moves the entry.
        fs.rename_noreplace(&PathBuf::from("/src/a.jpg"), &PathBuf::from("/dst/a.jpg"))
            .unwrap();
        assert!(!fs.exists(&PathBuf::from("/src/a.jpg")));
        assert_eq!(fs.len_of(&PathBuf::from("/dst/a.jpg")), Some(10));

        // Surprise collision: seeding the target makes rename fail EEXIST.
        fs.seed_file("/src/b.jpg", 3);
        fs.seed_file("/dst/b.jpg", 99);
        let e = fs
            .rename_noreplace(&PathBuf::from("/src/b.jpg"), &PathBuf::from("/dst/b.jpg"))
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);

        // EXDEV injection forces the copy path.
        fs.set_cross_fs(true);
        let e = fs
            .rename_noreplace(&PathBuf::from("/src/b.jpg"), &PathBuf::from("/other/b.jpg"))
            .unwrap_err();
        assert!(e.raw_os_error().is_some(), "EXDEV carries an errno");
        assert!(
            !fs.same_filesystem(&PathBuf::from("/src/x"), &PathBuf::from("/dst/x"))
                .unwrap()
        );
        fs.set_cross_fs(false);

        // ENOSPC injection on copy; source untouched, partial not created.
        fs.set_enospc_on_copy(true);
        let e = fs
            .copy_create_new(
                &PathBuf::from("/src/b.jpg"),
                &PathBuf::from("/dst/b.partial"),
            )
            .unwrap_err();
        assert_eq!(
            e.raw_os_error(),
            Some(rustix::io::Errno::NOSPC.raw_os_error())
        );
        assert!(fs.exists(&PathBuf::from("/src/b.jpg")));
        assert!(!fs.exists(&PathBuf::from("/dst/b.partial")));
    }

    /// Pins the device-aware cross-FS model (fix for the Task-2/Task-5 plan
    /// defect): `cross_fs` injection models per-root "devices", not a single
    /// global EXDEV switch, so a same-root publish rename (`.partial` -> final,
    /// both inside one dest bucket dir) succeeds even while cross-FS faults
    /// are armed for the initial source->dest hop.
    #[test]
    fn fake_cross_fs_models_per_root_devices() {
        let fs = FakeFs::new();
        fs.seed_file("/src/a.jpg", 5);
        fs.seed_file("/dst/x.partial", 7);
        fs.set_cross_fs(true);

        // (a) Rename across roots still fails EXDEV, source untouched.
        let e = fs
            .rename_noreplace(&PathBuf::from("/src/a.jpg"), &PathBuf::from("/other/a.jpg"))
            .unwrap_err();
        assert_eq!(
            e.raw_os_error(),
            Some(rustix::io::Errno::XDEV.raw_os_error())
        );
        assert!(
            fs.exists(&PathBuf::from("/src/a.jpg")),
            "source untouched on EXDEV"
        );

        // (b) Rename WITHIN the same root (dest-internal publish rename) succeeds.
        fs.rename_noreplace(
            &PathBuf::from("/dst/x.partial"),
            &PathBuf::from("/dst/y.jpg"),
        )
        .unwrap();
        assert!(!fs.exists(&PathBuf::from("/dst/x.partial")));
        assert_eq!(fs.len_of(&PathBuf::from("/dst/y.jpg")), Some(7));

        // (c) same_filesystem follows the same per-root device model.
        assert!(
            fs.same_filesystem(&PathBuf::from("/dst/a"), &PathBuf::from("/dst/b"))
                .unwrap(),
            "same root => same device"
        );
        assert!(
            !fs.same_filesystem(&PathBuf::from("/src/a"), &PathBuf::from("/dst/b"))
                .unwrap(),
            "different roots => different device"
        );
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! In-memory `FsOps` for deterministic fault injection in apply tests.
    use super::FsOps;
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};
    use std::io;
    use std::path::{Path, PathBuf};

    pub(crate) struct FakeFs {
        st: RefCell<State>,
    }

    struct State {
        files: BTreeMap<PathBuf, u64>, // path -> byte length
        dirs: BTreeSet<PathBuf>,
        free: u64,
        cross_fs: bool,       // same_filesystem -> false; rename_noreplace -> EXDEV
        enospc_on_copy: bool, // copy_create_new -> ENOSPC (source untouched)
        deny_rename_from: Option<PathBuf>, // rename_noreplace(from, _) -> EACCES
        deny_remove: Option<PathBuf>, // remove_file(path) -> EACCES
        fsynced_files: Vec<PathBuf>,
        fsynced_dirs: Vec<PathBuf>,
        events: Vec<String>, // ordered op log — durability ORDERING assertions
    }

    fn eacces() -> io::Error {
        io::Error::from(io::ErrorKind::PermissionDenied)
    }
    fn enoent() -> io::Error {
        io::Error::from(io::ErrorKind::NotFound)
    }
    fn eexist() -> io::Error {
        io::Error::from(io::ErrorKind::AlreadyExists)
    }
    fn exdev() -> io::Error {
        io::Error::from_raw_os_error(rustix::io::Errno::XDEV.raw_os_error())
    }
    fn enospc() -> io::Error {
        io::Error::from_raw_os_error(rustix::io::Errno::NOSPC.raw_os_error())
    }

    /// The "device" of a path in this fake's cross-FS model: its first
    /// non-root path component (e.g. `/src/a.jpg` -> `src`, `/dst/04_bests/x`
    /// -> `dst`). Two paths sharing a device are modeled as same-filesystem;
    /// this lets a rename *within* a dest bucket dir succeed even while
    /// `cross_fs` injection is active, matching real filesystems where the
    /// EXDEV hazard is between distinct mounts, not within one.
    fn device_of(p: &Path) -> Option<std::ffi::OsString> {
        p.components().find_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_os_string()),
            _ => None,
        })
    }

    impl FakeFs {
        pub(crate) fn new() -> Self {
            FakeFs {
                st: RefCell::new(State {
                    files: BTreeMap::new(),
                    dirs: BTreeSet::new(),
                    free: u64::MAX,
                    cross_fs: false,
                    enospc_on_copy: false,
                    deny_rename_from: None,
                    deny_remove: None,
                    fsynced_files: Vec::new(),
                    fsynced_dirs: Vec::new(),
                    events: Vec::new(),
                }),
            }
        }

        // ---- fixture builders / injectors ----
        pub(crate) fn seed_file(&self, path: impl Into<PathBuf>, len: u64) {
            self.st.borrow_mut().files.insert(path.into(), len);
        }
        pub(crate) fn set_free(&self, v: u64) {
            self.st.borrow_mut().free = v;
        }
        pub(crate) fn set_cross_fs(&self, v: bool) {
            self.st.borrow_mut().cross_fs = v;
        }
        pub(crate) fn set_enospc_on_copy(&self, v: bool) {
            self.st.borrow_mut().enospc_on_copy = v;
        }
        pub(crate) fn deny_rename_from(&self, p: impl Into<PathBuf>) {
            self.st.borrow_mut().deny_rename_from = Some(p.into());
        }
        pub(crate) fn deny_remove(&self, p: impl Into<PathBuf>) {
            self.st.borrow_mut().deny_remove = Some(p.into());
        }
        pub(crate) fn clear_faults(&self) {
            let mut s = self.st.borrow_mut();
            s.cross_fs = false;
            s.enospc_on_copy = false;
            s.deny_rename_from = None;
            s.deny_remove = None;
        }

        // ---- assertions ----
        pub(crate) fn exists(&self, p: &Path) -> bool {
            self.st.borrow().files.contains_key(p)
        }
        pub(crate) fn len_of(&self, p: &Path) -> Option<u64> {
            self.st.borrow().files.get(p).copied()
        }
        pub(crate) fn dir_exists(&self, p: &Path) -> bool {
            self.st.borrow().dirs.contains(p)
        }
        pub(crate) fn fsynced_files(&self) -> Vec<PathBuf> {
            self.st.borrow().fsynced_files.clone()
        }
        pub(crate) fn fsynced_dirs(&self) -> Vec<PathBuf> {
            self.st.borrow().fsynced_dirs.clone()
        }
        /// Ordered log of successful ops ("rename:a->b", "copy:a->b",
        /// "fsync_file:p", "fsync_dir:p", "remove:p") for ordering assertions.
        pub(crate) fn events(&self) -> Vec<String> {
            self.st.borrow().events.clone()
        }
    }

    impl FsOps for FakeFs {
        fn mkdir_p(&self, path: &Path) -> io::Result<()> {
            let mut s = self.st.borrow_mut();
            let mut cur = PathBuf::new();
            for comp in path.components() {
                cur.push(comp);
                s.dirs.insert(cur.clone());
            }
            Ok(())
        }

        fn same_filesystem(&self, a: &Path, b: &Path) -> io::Result<bool> {
            let s = self.st.borrow();
            if s.cross_fs {
                Ok(device_of(a) == device_of(b))
            } else {
                Ok(true)
            }
        }

        fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
            let mut s = self.st.borrow_mut();
            if s.cross_fs && device_of(from) != device_of(to) {
                return Err(exdev());
            }
            if s.deny_rename_from.as_deref() == Some(from) {
                return Err(eacces());
            }
            if !s.files.contains_key(from) {
                return Err(enoent());
            }
            if s.files.contains_key(to) {
                return Err(eexist()); // no-clobber
            }
            let len = s.files.remove(from).unwrap();
            s.files.insert(to.to_path_buf(), len);
            s.events
                .push(format!("rename:{}->{}", from.display(), to.display()));
            Ok(())
        }

        fn copy_create_new(&self, from: &Path, to: &Path) -> io::Result<u64> {
            let mut s = self.st.borrow_mut();
            if !s.files.contains_key(from) {
                return Err(enoent());
            }
            if s.files.contains_key(to) {
                return Err(eexist()); // O_EXCL
            }
            if s.enospc_on_copy {
                return Err(enospc()); // source untouched, nothing created
            }
            let len = *s.files.get(from).unwrap();
            if len > s.free {
                return Err(enospc());
            }
            s.files.insert(to.to_path_buf(), len);
            s.free -= len;
            s.events
                .push(format!("copy:{}->{}", from.display(), to.display()));
            Ok(len)
        }

        fn fsync_file(&self, path: &Path) -> io::Result<()> {
            let mut s = self.st.borrow_mut();
            if !s.files.contains_key(path) {
                return Err(enoent());
            }
            s.fsynced_files.push(path.to_path_buf());
            s.events.push(format!("fsync_file:{}", path.display()));
            Ok(())
        }

        fn fsync_dir(&self, path: &Path) -> io::Result<()> {
            let mut s = self.st.borrow_mut();
            s.fsynced_dirs.push(path.to_path_buf());
            s.events.push(format!("fsync_dir:{}", path.display()));
            Ok(())
        }

        fn remove_file(&self, path: &Path) -> io::Result<()> {
            let mut s = self.st.borrow_mut();
            if s.deny_remove.as_deref() == Some(path) {
                return Err(eacces());
            }
            if s.files.remove(path).is_none() {
                return Err(enoent());
            }
            s.events.push(format!("remove:{}", path.display()));
            Ok(())
        }

        fn file_len(&self, path: &Path) -> io::Result<u64> {
            self.st.borrow().files.get(path).copied().ok_or_else(enoent)
        }

        fn free_space(&self, _path: &Path) -> io::Result<u64> {
            Ok(self.st.borrow().free)
        }
    }
}
