# FastCull Phase 4 — Safe-Move Apply Engine — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans. Steps use `- [ ]`. Canonical types in [README.md](README.md). **Depends on Phases 1–3.** This is the most-tested unit — data loss would happen here.

**Goal:** Build `culler-core`'s safe-move `apply` engine: an injectable `FsOps` filesystem trait (`RealFs` via rustix + a fault-injecting `FakeFs` test double), a journal-first crash-recoverable executor that moves each shot's fileset atomically (same-FS `renameat2(NOREPLACE)`, cross-FS copy→fsync file→verify→rename→**fsync dir**→remove), and a **reconciling** `resume()` — never deleting a source except the cross-FS path unlinking its own verified copy, and removing its own journal on full success.

**Architecture:** All dangerous filesystem effects go through the `FsOps` trait so tests inject `EXDEV`, `ENOSPC`, permission errors, and surprise collisions deterministically against an in-memory `FakeFs`; `RealFs` (rustix) is exercised by real temp-dir tests for happy paths. `apply` serializes the plan to a journal **before the first move**, marks each file `Done` incrementally (atomic temp+rename), and stops loudly on the first failure with the journal as the durable stop-of-record; `resume` replays that journal, skipping `Done` moves. There is **no deletion step** — rejects are moved like any bucket.

**Tech Stack:** Rust 2024, rustix 1 (renameat2/statvfs/fsync/stat — dep already added by Phase 3; API verified by probe build 2026-07-09), serde_json (journal), tempfile (dev-dependency, real-FS tests).

## Global Constraints

These bind every task in this phase (copied from [README.md](README.md)):

- **v1 performs no deletions of user data.** There is no `unlink` of a source shot anywhere in v1 **except the cross-FS copy path removing its own verified source after the destination copy is fsynced and length-verified**. Rejects are **moved** to `00_rejected`, never deleted. There is no delete step, no "moves-before-deletes" ordering.
- **Atomic writes everywhere:** journal writes use write-temp-then-rename (fsynced at the checkpoints below). File moves use `renameat2(RENAME_NOREPLACE)` (no-clobber) same-FS, and **copy→fsync file→verify→rename(NOREPLACE)→fsync dir→remove source** cross-FS — the dir fsync comes *after* the publish rename (spec §8 rev 3): the source unlink is on a *different filesystem*, so power loss could persist the unlink while an un-fsynced rename is lost, leaving the data reachable only as a hidden `.partial`.
- **A destination file appearing between plan and apply must fail loudly** (NOREPLACE returns `EEXIST` → `ApplyError::Collision`); never silently overwrite. Plan-time collision checks are advisory only. Sidecar writes carry the same guarantee (Phase 3's `write_sidecar` publishes with NOREPLACE) and are **skip-idempotent** here: an already-present sidecar target is skipped on resume, not clobbered and not an error.
- **Journal-first crash recovery:** the serialized plan lands in `dest/.fastcull-apply.json` **before the first move** (fsynced); each file is marked complete as it executes (fsynced every 64th move, on failure, and at stop — `resume()`'s reconciliation makes an unsynced tail harmless, so per-move fsync isn't needed). A crashed/aborted run is resumable via `resume()`, which **reconciles the journal against the disk in both directions** before executing (spec §8 rev 3). **On full success the journal is removed** — it is FastCull's own metadata, not user data; a finished run must never read as a crashed one or hijack a later apply into the same dest.
- **Preflight:** before a cross-filesystem run, check destination free space (`statvfs`) against the plan's total byte count; refuse (`ApplyError::Preflight`) rather than abort halfway.
- **Never add BLAKE3** — cross-FS verification is byte-length only in v1 (BLAKE3 is a spec-Phase-2 paranoia setting). **Never add a delete step for user data.** (Removing FastCull's own retired journal and cleaning its own `.partial`/`.tmp` files is bookkeeping, not a delete step.)
- **Platform:** Linux only. `rustix`/`renameat2`/`statvfs`/`stat` are used directly; no cross-platform abstraction.
- **`culler-core` has zero GUI dependencies.**
- **TDD, DRY, YAGNI, frequent commits.** Every task: failing test → run-it-fails → minimal impl → run-it-passes → commit. Conventional-commit messages (`feat:`, `test:`, `refactor:`, `chore:`).

**Canonical-type note (Phase 3 refinement):** Phase 3 refined `ShotOp.write_sidecar` from the README's `Option<FileMove>` to `Option<SidecarWrite>`, where `pub struct SidecarWrite { pub path: std::path::PathBuf, pub tags: Vec<String>, pub rating: Option<i32> }`. This phase **consumes that refined shape** — a `Some(SidecarWrite)` means "write a fresh sidecar here via `crate::xmp::write_sidecar`". All other `ApplyPlan` / `ShotOp` / `FileMove` fields are exactly as in the README.

---

### Task 1: `FsOps` trait + `RealFs` (rustix), no-clobber rename verified on the real FS

**Files:**
- Create: `culler-core/src/fsops.rs`
- Modify: `culler-core/src/lib.rs` (add `pub mod fsops;`)
- Modify: `culler-core/Cargo.toml` (add `rustix` dep + `tempfile` dev-dep)
- Test: `culler-core/src/fsops.rs` (inline `#[cfg(test)] mod tests`, real temp dir via `tempfile`)

**Interfaces:**
- Consumes: nothing from earlier phases (leaf module). Uses `rustix::fs::{renameat_with, RenameFlags, CWD, statvfs, stat}`.
- Produces:
  - `pub trait FsOps { fn mkdir_p(&self, p: &Path) -> io::Result<()>; fn same_filesystem(&self, a: &Path, b: &Path) -> io::Result<bool>; fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()>; fn copy_create_new(&self, from: &Path, to: &Path) -> io::Result<u64>; fn fsync_file(&self, p: &Path) -> io::Result<()>; fn fsync_dir(&self, p: &Path) -> io::Result<()>; fn remove_file(&self, p: &Path) -> io::Result<()>; fn file_len(&self, p: &Path) -> io::Result<u64>; fn free_space(&self, p: &Path) -> io::Result<u64>; }`
  - `pub struct RealFs;` implementing `FsOps`.

- [ ] **Step 1: Write the failing test**

First ensure dependencies in `culler-core/Cargo.toml` (`rustix` is already present if Phase 3 landed — it added it for the no-clobber sidecar publish):
```toml
[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
rustix = { version = "1", features = ["fs"] }

[dev-dependencies]
tempfile = "3"
```

`culler-core/src/fsops.rs` (test module — trait/impl still absent, so it will not compile → red):
```rust
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
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "NOREPLACE must not clobber");
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
        assert!(fs_ops.same_filesystem(&a, &b).unwrap(), "two files in one dir share a FS");
        assert!(fs_ops.free_space(dir.path()).unwrap() > 0, "statvfs reports free space");
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
```

- [ ] **Step 2: Run to verify it fails**
Run: `cargo test -p culler-core fsops::tests`
Expected: FAIL — `cannot find type RealFs` / `cannot find trait FsOps` (module has no non-test code yet).

- [ ] **Step 3: Minimal implementation**

Prepend to `culler-core/src/fsops.rs` (above the test module):
```rust
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
        use rustix::fs::{renameat_with, RenameFlags, CWD};
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
```

Then add to `culler-core/src/lib.rs`:
```rust
pub mod fsops;
```

- [ ] **Step 4: Run to verify pass**
Run: `cargo test -p culler-core fsops::tests`
Expected: PASS — all three real-FS tests green.

- [ ] **Step 5: Commit**
```bash
git add culler-core/Cargo.toml culler-core/src/fsops.rs culler-core/src/lib.rs
git commit -m "feat(fsops): FsOps trait + RealFs rustix impl with no-clobber rename" -m "Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: `FakeFs` — in-memory fault-injection test double

**Files:**
- Modify: `culler-core/src/fsops.rs` (add `#[cfg(test)] pub(crate) mod fake`)
- Test: `culler-core/src/fsops.rs` (inline test asserting the double's own behavior)

**Interfaces:**
- Consumes: `FsOps` (Task 1).
- Produces (test-only, crate-visible so `apply` tests reuse it): `crate::fsops::fake::FakeFs`, an in-memory model that can inject **EXDEV** on rename (forcing the cross-FS copy path), **ENOSPC** on copy, a **surprise collision** (seed a file at a dest so `rename_noreplace` returns `EEXIST`), **permission errors** (deny rename/remove of a specific path), and a **controllable `free_space`**. It records fsynced files/dirs for assertions **and an ordered `events()` log (`rename:from->to`, `copy:from->to`, `fsync_file:p`, `fsync_dir:p`, `remove:p`) so tests can assert durability ORDERING, not just membership** (the spec §8 rev-3 rename-before-dir-fsync guarantee is an ordering property).

> `FakeFs` is the backbone of every later apply test. It is real committed code under `#[cfg(test)]`; tasks 3–10 reuse it via `use crate::fsops::fake::FakeFs;`. It is shown in full here and never re-pasted.

- [ ] **Step 1: Write the failing test**

Append to `culler-core/src/fsops.rs` (inside the `#[cfg(test)] mod tests` block, or a sibling test fn — the `FakeFs` type it references does not exist yet → red):
```rust
    use super::fake::FakeFs; // NB: `use fake::…` would not resolve inside `mod tests`
    use std::path::PathBuf;

    #[test]
    fn fake_rename_moves_entry_and_injects_faults() {
        let fs = FakeFs::new();
        fs.seed_file("/src/a.jpg", 10);

        // Happy rename moves the entry.
        fs.rename_noreplace(&PathBuf::from("/src/a.jpg"), &PathBuf::from("/dst/a.jpg")).unwrap();
        assert!(!fs.exists(&PathBuf::from("/src/a.jpg")));
        assert_eq!(fs.len_of(&PathBuf::from("/dst/a.jpg")), Some(10));

        // Surprise collision: seeding the target makes rename fail EEXIST.
        fs.seed_file("/src/b.jpg", 3);
        fs.seed_file("/dst/b.jpg", 99);
        let e = fs.rename_noreplace(&PathBuf::from("/src/b.jpg"), &PathBuf::from("/dst/b.jpg")).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);

        // EXDEV injection forces the copy path.
        fs.set_cross_fs(true);
        let e = fs.rename_noreplace(&PathBuf::from("/src/b.jpg"), &PathBuf::from("/other/b.jpg")).unwrap_err();
        assert!(e.raw_os_error().is_some(), "EXDEV carries an errno");
        assert!(!fs.same_filesystem(&PathBuf::from("/src/x"), &PathBuf::from("/dst/x")).unwrap());
        fs.set_cross_fs(false);

        // ENOSPC injection on copy; source untouched, partial not created.
        fs.set_enospc_on_copy(true);
        let e = fs.copy_create_new(&PathBuf::from("/src/b.jpg"), &PathBuf::from("/dst/b.partial")).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(rustix::io::Errno::NOSPC.raw_os_error()));
        assert!(fs.exists(&PathBuf::from("/src/b.jpg")));
        assert!(!fs.exists(&PathBuf::from("/dst/b.partial")));
    }
```

- [ ] **Step 2: Run to verify it fails**
Run: `cargo test -p culler-core fsops::tests::fake_rename_moves_entry_and_injects_faults`
Expected: FAIL — `unresolved import fake::FakeFs` / `module fake not found`.

- [ ] **Step 3: Minimal implementation**

Append to `culler-core/src/fsops.rs`:
```rust
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
        cross_fs: bool,                    // same_filesystem -> false; rename_noreplace -> EXDEV
        enospc_on_copy: bool,              // copy_create_new -> ENOSPC (source untouched)
        deny_rename_from: Option<PathBuf>, // rename_noreplace(from, _) -> EACCES
        deny_remove: Option<PathBuf>,      // remove_file(path) -> EACCES
        fsynced_files: Vec<PathBuf>,
        fsynced_dirs: Vec<PathBuf>,
        events: Vec<String>, // ordered op log — durability ORDERING assertions
    }

    fn eacces() -> io::Error { io::Error::from(io::ErrorKind::PermissionDenied) }
    fn enoent() -> io::Error { io::Error::from(io::ErrorKind::NotFound) }
    fn eexist() -> io::Error { io::Error::from(io::ErrorKind::AlreadyExists) }
    fn exdev() -> io::Error { io::Error::from_raw_os_error(rustix::io::Errno::XDEV.raw_os_error()) }
    fn enospc() -> io::Error { io::Error::from_raw_os_error(rustix::io::Errno::NOSPC.raw_os_error()) }

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
        pub(crate) fn set_free(&self, v: u64) { self.st.borrow_mut().free = v; }
        pub(crate) fn set_cross_fs(&self, v: bool) { self.st.borrow_mut().cross_fs = v; }
        pub(crate) fn set_enospc_on_copy(&self, v: bool) { self.st.borrow_mut().enospc_on_copy = v; }
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
        pub(crate) fn exists(&self, p: &Path) -> bool { self.st.borrow().files.contains_key(p) }
        pub(crate) fn len_of(&self, p: &Path) -> Option<u64> { self.st.borrow().files.get(p).copied() }
        pub(crate) fn dir_exists(&self, p: &Path) -> bool { self.st.borrow().dirs.contains(p) }
        pub(crate) fn fsynced_files(&self) -> Vec<PathBuf> { self.st.borrow().fsynced_files.clone() }
        pub(crate) fn fsynced_dirs(&self) -> Vec<PathBuf> { self.st.borrow().fsynced_dirs.clone() }
        /// Ordered log of successful ops ("rename:a->b", "copy:a->b",
        /// "fsync_file:p", "fsync_dir:p", "remove:p") for ordering assertions.
        pub(crate) fn events(&self) -> Vec<String> { self.st.borrow().events.clone() }
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

        fn same_filesystem(&self, _a: &Path, _b: &Path) -> io::Result<bool> {
            Ok(!self.st.borrow().cross_fs)
        }

        fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
            let mut s = self.st.borrow_mut();
            if s.cross_fs {
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
            s.events.push(format!("rename:{}->{}", from.display(), to.display()));
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
            s.events.push(format!("copy:{}->{}", from.display(), to.display()));
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
```

- [ ] **Step 4: Run to verify pass**
Run: `cargo test -p culler-core fsops::tests`
Expected: PASS — the `FakeFs` self-test and Task-1 real-FS tests all green.

- [ ] **Step 5: Commit**
```bash
git add culler-core/src/fsops.rs
git commit -m "test(fsops): in-memory FakeFs double with EXDEV/ENOSPC/perm/collision injection" -m "Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: `apply` happy path — same-FS group move + journal-first write

**Files:**
- Create: `culler-core/src/apply.rs`
- Modify: `culler-core/src/lib.rs` (add `pub mod apply;`)
- Test: `culler-core/src/apply.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes (Phase 3, exact): `ApplyPlan { dest: PathBuf, buckets: [String;5], ops: Vec<ShotOp>, per_bucket_counts: TierCountsPlan, skipped_sidecar_writes: Vec<String>, stale: Vec<String>, total_bytes: u64 }`; `ShotOp { stem: String, bucket: String, moves: Vec<FileMove>, write_sidecar: Option<SidecarWrite>, suffix: Option<u32> }`; `FileMove { from: PathBuf, to: PathBuf }`; `SidecarWrite { path: PathBuf, tags: Vec<String>, rating: Option<i32> }`. Bucket constants from `crate::model`. `FsOps` (Task 1), `FakeFs` (Task 2).
- Produces: `pub enum OpState { Pending, Done, Failed }`; `pub struct Journal { pub plan: ApplyPlan, pub statuses: Vec<OpState> }`; `pub enum ApplyError { Preflight(String), Fs { path: PathBuf, source: io::Error }, Collision(PathBuf) }`; `pub struct ApplyReport { pub moved_shots: usize, pub moved_files: usize, pub sidecars_written: usize, pub stopped_at: Option<String> }`; `pub fn apply(plan: &ApplyPlan, fs: &dyn FsOps, journal_path: &Path) -> Result<ApplyReport, ApplyError>`.

> **Return-contract decision (resolves a README ambiguity — see notes at end):** `apply`/`resume` return `Ok(ApplyReport { stopped_at: None, .. })` on full completion; **any** per-file failure returns `Err(ApplyError)` with a precise variant. The durable on-disk journal is the stop-of-record (its first non-`Done` op is where the run stopped); `resume` continues from exactly there. `ApplyReport.stopped_at` is reserved for the Phase-6 binary to materialize from a failed run's journal + error; Phase-4 tests assert the stop via the journal (first non-`Done` stem) and the `Err` variant.

- [ ] **Step 1: Write the failing test**

`culler-core/src/apply.rs` (test module — `apply`/`Journal` absent → red):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsops::fake::FakeFs;
    use crate::model::{BUCKET_BESTS, BUCKET_KEEP, BUCKET_PICKS, BUCKET_REJECTED, BUCKET_REST};
    use crate::plan::{ApplyPlan, FileMove, ShotOp, TierCountsPlan};
    use std::path::{Path, PathBuf};

    // ---- test builders (shared across apply tests) ----
    pub(super) fn buckets() -> [String; 5] {
        [
            BUCKET_REJECTED.into(),
            BUCKET_REST.into(),
            BUCKET_KEEP.into(),
            BUCKET_PICKS.into(),
            BUCKET_BESTS.into(),
        ]
    }

    pub(super) fn shot(stem: &str, bucket: &str, srcs: &[(&str, u64)], dest: &Path) -> (ShotOp, u64) {
        let mut moves = Vec::new();
        let mut bytes = 0u64;
        for (name, len) in srcs {
            let from = PathBuf::from(format!("/src/{name}"));
            let to = dest.join(bucket).join(name);
            moves.push(FileMove { from, to });
            bytes += *len;
        }
        (
            ShotOp {
                stem: stem.into(),
                bucket: bucket.into(),
                moves,
                write_sidecar: None, // FakeFs tests never do real xmp I/O; see Task 11
                suffix: None,
            },
            bytes,
        )
    }

    pub(super) fn plan_of(dest: &Path, ops: Vec<ShotOp>, total_bytes: u64) -> ApplyPlan {
        ApplyPlan {
            dest: dest.to_path_buf(),
            buckets: buckets(),
            ops,
            per_bucket_counts: TierCountsPlan::default(),
            skipped_sidecar_writes: Vec::new(),
            stale: Vec::new(),
            total_bytes,
        }
    }

    pub(super) fn seed_sources(fs: &FakeFs, ops: &[ShotOp]) {
        for op in ops {
            for m in &op.moves {
                fs.seed_file(m.from.clone(), 100);
            }
        }
    }

    #[test]
    fn apply_same_fs_moves_every_file_and_journals_all_done() {
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot("IMG_0001", BUCKET_KEEP, &[("IMG_0001.JPG", 100), ("IMG_0001.CR3", 100)], &dest);
        let (s2, b2) = shot("IMG_0002", BUCKET_PICKS, &[("IMG_0002.JPG", 100)], &dest);
        let plan = plan_of(&dest, vec![s1, s2], b1 + b2);

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops);

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");

        let report = apply(&plan, &fs, &jpath).unwrap();

        // Every source moved into its bucket; sources consumed.
        assert!(!fs.exists(&PathBuf::from("/src/IMG_0001.JPG")));
        assert_eq!(fs.len_of(&dest.join(BUCKET_KEEP).join("IMG_0001.JPG")), Some(100));
        assert_eq!(fs.len_of(&dest.join(BUCKET_KEEP).join("IMG_0001.CR3")), Some(100));
        assert_eq!(fs.len_of(&dest.join(BUCKET_PICKS).join("IMG_0002.JPG")), Some(100));

        // Buckets were created.
        assert!(fs.dir_exists(&dest.join(BUCKET_REJECTED)));
        assert!(fs.dir_exists(&dest.join(BUCKET_BESTS)));

        assert_eq!(report.moved_shots, 2);
        assert_eq!(report.moved_files, 3);
        assert_eq!(report.stopped_at, None);

        // Success REMOVES the journal (spec §8 rev 3): a finished run must never
        // read as a crashed one or hijack a later apply into the same dest.
        // (Journal-first existence + incremental Done marking are proven by the
        // failure-path tests in Task 4.)
        assert!(!jpath.exists(), "journal removed on full success");
    }
}
```

- [ ] **Step 2: Run to verify it fails**
Run: `cargo test -p culler-core apply::tests::apply_same_fs_moves_every_file_and_journals_all_done`
Expected: FAIL — `cannot find function apply` / `cannot find type Journal`.

- [ ] **Step 3: Minimal implementation**

Prepend to `culler-core/src/apply.rs` (above the test module):
```rust
//! The safe-move apply engine. Journals the plan before the first move, moves
//! each shot's fileset group-atomically through `FsOps`, and stops loudly on the
//! first failure. No deletion step exists beyond the cross-FS path removing its
//! own verified source (Task 5). Resumable via `resume` (Task 8).

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fsops::FsOps;
use crate::plan::ApplyPlan;

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum OpState {
    Pending,
    Done,
    Failed,
}

/// Serialized alongside the plan in `dest/.fastcull-apply.json`. `statuses` is
/// parallel to the flattened list of file moves (`ops` × `moves`), in order.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Journal {
    pub plan: ApplyPlan,
    pub statuses: Vec<OpState>,
}

#[derive(Debug)]
pub enum ApplyError {
    /// Free-space preflight refused before any move.
    Preflight(String),
    /// A filesystem operation failed on `path`.
    Fs { path: PathBuf, source: io::Error },
    /// A destination file appeared between plan and apply (NOREPLACE `EEXIST`).
    Collision(PathBuf),
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyError::Preflight(m) => write!(f, "preflight failed: {m}"),
            ApplyError::Fs { path, source } => write!(f, "fs error on {}: {source}", path.display()),
            ApplyError::Collision(p) => write!(f, "collision: {} already exists", p.display()),
        }
    }
}
impl std::error::Error for ApplyError {}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApplyReport {
    pub moved_shots: usize,
    pub moved_files: usize,
    pub sidecars_written: usize,
    pub stopped_at: Option<String>,
}

/// Total number of file moves across all ops (== journal `statuses` length).
fn total_move_count(plan: &ApplyPlan) -> usize {
    plan.ops.iter().map(|o| o.moves.len()).sum()
}

/// Serialize the journal atomically (temp file → optional fsync → rename). Real
/// I/O — the journal must survive a real crash, so it does NOT go through
/// `FsOps`. `sync` fsyncs the temp before publishing: required at checkpoints
/// (journal-first write, failure stop), optional for incremental progress —
/// `resume`'s reconciliation (Task 8) makes an unsynced tail harmless, so
/// per-move fsync (brutal on multi-thousand-file shoots) is unnecessary.
fn write_journal(journal: &Journal, path: &Path, sync: bool) -> Result<(), ApplyError> {
    let bytes = serde_json::to_vec(journal).map_err(|e| ApplyError::Fs {
        path: path.to_path_buf(),
        source: io::Error::other(e),
    })?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "journal".into());
    let tmp = path.with_file_name(format!("{name}.tmp"));
    let mut f = std::fs::File::create(&tmp).map_err(|e| ApplyError::Fs { path: tmp.clone(), source: e })?;
    f.write_all(&bytes).map_err(|e| ApplyError::Fs { path: tmp.clone(), source: e })?;
    if sync {
        f.sync_all().map_err(|e| ApplyError::Fs { path: tmp.clone(), source: e })?;
    }
    drop(f);
    std::fs::rename(&tmp, path).map_err(|e| ApplyError::Fs { path: path.to_path_buf(), source: e })
}

/// Move one file same-FS (rename), mapping no-clobber `EEXIST` to `Collision`.
/// (Cross-FS `EXDEV` handling is added in Task 5.)
fn move_one(fs: &dyn FsOps, from: &Path, to: &Path) -> Result<(), ApplyError> {
    match fs.rename_noreplace(from, to) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(ApplyError::Collision(to.to_path_buf())),
        Err(e) => Err(ApplyError::Fs { path: from.to_path_buf(), source: e }),
    }
}

/// Execute (or resume) a journal: mkdir buckets, move each not-yet-`Done` file,
/// then per shot write a fresh sidecar if requested. Shared by `apply` and `resume`.
fn execute(journal: &mut Journal, fs: &dyn FsOps, journal_path: &Path) -> Result<ApplyReport, ApplyError> {
    let dest = journal.plan.dest.clone();
    let buckets = journal.plan.buckets.clone();
    let ops = journal.plan.ops.clone(); // owned copy so we can mutate journal.statuses freely

    // Create the five bucket dirs (idempotent; safe on resume).
    for bucket in &buckets {
        let dir = dest.join(bucket);
        fs.mkdir_p(&dir).map_err(|e| ApplyError::Fs { path: dir, source: e })?;
    }

    let mut report = ApplyReport::default();
    let mut gidx = 0usize; // global index into journal.statuses

    for op in &ops {
        for mv in &op.moves {
            if journal.statuses[gidx] == OpState::Done {
                gidx += 1; // already moved by an earlier (crashed) run
                continue;
            }
            move_one(fs, &mv.from, &mv.to)?;
            journal.statuses[gidx] = OpState::Done;
            report.moved_files += 1;
            gidx += 1;
        }

        if let Some(sw) = &op.write_sidecar {
            // Skip-idempotent (spec §8 rev 3): sidecars are not journaled, so a
            // resume re-visits every op — an already-written target is skipped,
            // and write_sidecar's NOREPLACE publish turns a lost race into
            // AlreadyExists, which is also a skip. Never a clobber.
            if !sw.path.exists() {
                match crate::xmp::write_sidecar(&sw.path, &sw.tags, sw.rating) {
                    Ok(()) => report.sidecars_written += 1,
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(ApplyError::Fs { path: sw.path.clone(), source: e }),
                }
            }
        }
        report.moved_shots += 1;
    }

    // Full success: the journal has served its purpose — remove it so a later
    // launch or apply into this dest never mistakes a finished run for a
    // crashed one (spec §8 rev 3; the journal is FastCull metadata, not user
    // data — the no-deletion guarantee protects photos, not our bookkeeping).
    let _ = std::fs::remove_file(journal_path);
    Ok(report)
}

/// Journals the plan FIRST, then executes each `ShotOp` group. Same-FS only in
/// Task 3; cross-FS + preflight land in Tasks 5 and 9.
pub fn apply(plan: &ApplyPlan, fs: &dyn FsOps, journal_path: &Path) -> Result<ApplyReport, ApplyError> {
    let mut journal = Journal {
        plan: plan.clone(),
        statuses: vec![OpState::Pending; total_move_count(plan)],
    };
    write_journal(&journal, journal_path, true)?; // JOURNAL FIRST — durable before any move
    execute(&mut journal, fs, journal_path)
}
```

Then add to `culler-core/src/lib.rs`:
```rust
pub mod apply;
```

- [ ] **Step 4: Run to verify pass**
Run: `cargo test -p culler-core apply::tests::apply_same_fs_moves_every_file_and_journals_all_done`
Expected: PASS.

- [ ] **Step 5: Commit**
```bash
git add culler-core/src/apply.rs culler-core/src/lib.rs
git commit -m "feat(apply): same-FS group move engine with journal-first write" -m "Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: Journal-first ordering + incremental `Done` marking

**Files:**
- Modify: `culler-core/src/apply.rs` (`execute` persists after each move + on failure)
- Test: `culler-core/src/apply.rs` (inline)

**Interfaces:**
- Consumes: everything from Task 3 + `FakeFs::deny_rename_from`.
- Produces: the crash-safety guarantee — a failure mid-run leaves a durable journal whose statuses are exactly `[Done*, Failed, Pending*]`, and the journal exists even when the very first move fails (proving journal-first).

- [ ] **Step 1: Write the failing test**

Append inside `apply::tests`:
```rust
    #[test]
    fn journal_persists_incrementally_and_before_first_move() {
        let dest = PathBuf::from("/dst");
        // One shot, three files; fail the SECOND move (index 1).
        let (s1, b1) = shot(
            "IMG_0007",
            BUCKET_KEEP,
            &[("IMG_0007.JPG", 100), ("IMG_0007.CR3", 100), ("IMG_0007.xmp", 100)],
            &dest,
        );
        let plan = plan_of(&dest, vec![s1], b1);

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops);
        fs.deny_rename_from("/src/IMG_0007.CR3"); // second move fails with EACCES

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");

        let err = apply(&plan, &fs, &jpath).unwrap_err();
        assert!(matches!(err, ApplyError::Fs { .. }));

        // Durable journal reflects incremental progress: [Done, Failed, Pending].
        let j: Journal = serde_json::from_slice(&std::fs::read(&jpath).unwrap()).unwrap();
        assert_eq!(j.statuses, vec![OpState::Done, OpState::Failed, OpState::Pending]);

        // First file really moved; the failing file's source is untouched.
        assert!(!fs.exists(&PathBuf::from("/src/IMG_0007.JPG")));
        assert!(fs.exists(&PathBuf::from("/src/IMG_0007.CR3")));
        assert!(fs.exists(&PathBuf::from("/src/IMG_0007.xmp")));
    }

    #[test]
    fn journal_exists_even_when_the_very_first_move_fails() {
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot("IMG_0009", BUCKET_KEEP, &[("IMG_0009.JPG", 100)], &dest);
        let plan = plan_of(&dest, vec![s1], b1);

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops);
        fs.deny_rename_from("/src/IMG_0009.JPG"); // first move fails

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");

        let _ = apply(&plan, &fs, &jpath).unwrap_err();
        assert!(jpath.exists(), "journal was written before the first move");
        let j: Journal = serde_json::from_slice(&std::fs::read(&jpath).unwrap()).unwrap();
        assert_eq!(j.statuses, vec![OpState::Failed]);
    }
```

- [ ] **Step 2: Run to verify it fails**
Run: `cargo test -p culler-core apply::tests::journal_persists_incrementally_and_before_first_move`
Expected: FAIL — Task-3 `execute` only writes the journal at the end, so on failure the on-disk statuses are all `Pending` (not `[Done, Failed, Pending]`).

- [ ] **Step 3: Minimal implementation**

In `execute`, replace the move loop body so each move is persisted immediately and failures record `Failed`. Replace the inner `for mv` block and drop the trailing single end-write:
```rust
    for op in &ops {
        for mv in &op.moves {
            if journal.statuses[gidx] == OpState::Done {
                gidx += 1;
                continue;
            }
            match move_one(fs, &mv.from, &mv.to) {
                Ok(()) => {
                    journal.statuses[gidx] = OpState::Done;
                    report.moved_files += 1;
                    // Persist progress incrementally; fsync only every 64th move
                    // (and at checkpoints) — reconciliation (Task 8) makes an
                    // unsynced tail harmless, and this doubles as the progress
                    // feed the Phase-6 UI polls from a worker thread.
                    write_journal(journal, journal_path, report.moved_files % 64 == 0)?;
                }
                Err(e) => {
                    journal.statuses[gidx] = OpState::Failed;
                    let _ = write_journal(journal, journal_path, true); // durable stop record
                    return Err(e);
                }
            }
            gidx += 1;
        }

        if let Some(sw) = &op.write_sidecar {
            // Skip-idempotent; NOREPLACE inside write_sidecar — see Task 3.
            if !sw.path.exists() {
                match crate::xmp::write_sidecar(&sw.path, &sw.tags, sw.rating) {
                    Ok(()) => report.sidecars_written += 1,
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(ApplyError::Fs { path: sw.path.clone(), source: e }),
                }
            }
        }
        report.moved_shots += 1;
    }

    let _ = std::fs::remove_file(journal_path); // success: journal retired (spec §8 rev 3)
    Ok(report)
```
(The former end-of-run `write_journal` tail is replaced by the journal removal — on success there is nothing left to resume, and a lingering all-`Done` journal would read as a crash or hijack a later apply into the same dest.)

- [ ] **Step 4: Run to verify pass**
Run: `cargo test -p culler-core apply::tests`
Expected: PASS — incremental + journal-first tests green; Task-3 happy path still green.

- [ ] **Step 5: Commit**
```bash
git add culler-core/src/apply.rs
git commit -m "feat(apply): incremental Done journaling + durable stop-of-record on failure" -m "Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: Cross-FS `EXDEV` path — copy → fsync → verify → rename → fsync_dir → remove

**Files:**
- Modify: `culler-core/src/apply.rs` (`move_one` EXDEV branch + `move_cross_fs`)
- Test: `culler-core/src/apply.rs` (inline, `FakeFs::set_cross_fs(true)`)

**Interfaces:**
- Consumes: Task-3 `move_one`, `FakeFs` cross-FS injection + fsync recorders.
- Produces: `move_cross_fs(fs, from, to)` — copies to a hidden `.<name>.partial`, fsyncs it, verifies `file_len(dest) == file_len(src)`, publishes with NOREPLACE, **fsyncs the bucket dir (after the publish — spec §8 rev 3)**, and **only then** removes the source. A mid-copy failure leaves the source untouched and cleans the partial.

- [ ] **Step 1: Write the failing test**

Append inside `apply::tests`:
```rust
    #[test]
    fn apply_cross_fs_copies_verifies_then_removes_source() {
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot("IMG_0100", BUCKET_BESTS, &[("IMG_0100.JPG", 100), ("IMG_0100.CR3", 100)], &dest);
        let plan = plan_of(&dest, vec![s1], b1);

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops);
        fs.set_free(u64::MAX);   // preflight (Task 9) will pass
        fs.set_cross_fs(true);   // rename returns EXDEV → copy path

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");

        let report = apply(&plan, &fs, &jpath).unwrap();
        assert_eq!(report.moved_files, 2);

        let jpg_final = dest.join(BUCKET_BESTS).join("IMG_0100.JPG");
        let jpg_partial = dest.join(BUCKET_BESTS).join(".IMG_0100.JPG.partial");

        // Final present + correct length; source removed; partial cleaned up.
        assert_eq!(fs.len_of(&jpg_final), Some(100));
        assert!(!fs.exists(&PathBuf::from("/src/IMG_0100.JPG")), "verified source removed");
        assert!(!fs.exists(&jpg_partial), "partial published, not left behind");

        // Durability ordering was exercised: partial fsynced, bucket dir fsynced.
        assert!(fs.fsynced_files().contains(&jpg_partial));
        assert!(fs.fsynced_dirs().contains(&dest.join(BUCKET_BESTS)));

        // ORDER (spec §8 rev 3), asserted on the event log, not just membership:
        // publish rename BEFORE the dir fsync, source unlink strictly last.
        let ev = fs.events();
        let pos = |needle: &str| {
            ev.iter()
                .position(|e| e == needle)
                .unwrap_or_else(|| panic!("missing {needle} in {ev:?}"))
        };
        let publish = pos(&format!("rename:{}->{}", jpg_partial.display(), jpg_final.display()));
        let dirsync = pos(&format!("fsync_dir:{}", dest.join(BUCKET_BESTS).display()));
        let unlink = pos("remove:/src/IMG_0100.JPG");
        assert!(publish < dirsync, "dir fsync must FOLLOW the publish rename: {ev:?}");
        assert!(dirsync < unlink, "source unlink must follow the dir fsync: {ev:?}");
    }
```

- [ ] **Step 2: Run to verify it fails**
Run: `cargo test -p culler-core apply::tests::apply_cross_fs_copies_verifies_then_removes_source`
Expected: FAIL — Task-3 `move_one` maps `EXDEV` to `ApplyError::Fs` (no copy fallback), so `apply` errors instead of moving.

- [ ] **Step 3: Minimal implementation**

Add the EXDEV branch to `move_one`:
```rust
fn move_one(fs: &dyn FsOps, from: &Path, to: &Path) -> Result<(), ApplyError> {
    match fs.rename_noreplace(from, to) {
        Ok(()) => Ok(()),
        Err(e) if is_exdev(&e) => move_cross_fs(fs, from, to),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(ApplyError::Collision(to.to_path_buf())),
        Err(e) => Err(ApplyError::Fs { path: from.to_path_buf(), source: e }),
    }
}

fn is_exdev(e: &io::Error) -> bool {
    e.raw_os_error() == Some(rustix::io::Errno::XDEV.raw_os_error())
}

/// Hidden sibling partial path: `dir/.<name>.partial`.
fn partial_path(to: &Path) -> PathBuf {
    let name = to
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    to.with_file_name(format!(".{name}.partial"))
}

/// Cross-filesystem move. Source is never touched until the destination copy is
/// fully copied, fsynced, length-verified, and atomically published.
fn move_cross_fs(fs: &dyn FsOps, from: &Path, to: &Path) -> Result<(), ApplyError> {
    let partial = partial_path(to);
    let _ = fs.remove_file(&partial); // clear a stale partial from a prior crash

    // Copy source → partial (O_EXCL). Any error here leaves the SOURCE untouched.
    let copied = match fs.copy_create_new(from, &partial) {
        Ok(n) => n,
        Err(e) => {
            let _ = fs.remove_file(&partial);
            return Err(ApplyError::Fs { path: partial, source: e });
        }
    };
    if let Err(e) = fs.fsync_file(&partial) {
        let _ = fs.remove_file(&partial);
        return Err(ApplyError::Fs { path: partial, source: e });
    }
    // Verify byte length: file_len(dest) == file_len(src) (BLAKE3 is phase-2).
    let dest_len = match fs.file_len(&partial) {
        Ok(n) => n,
        Err(e) => {
            let _ = fs.remove_file(&partial);
            return Err(ApplyError::Fs { path: partial, source: e });
        }
    };
    let src_len = match fs.file_len(from) {
        Ok(n) => n,
        Err(e) => {
            let _ = fs.remove_file(&partial);
            return Err(ApplyError::Fs { path: from.to_path_buf(), source: e });
        }
    };
    if dest_len != src_len || copied != src_len {
        let _ = fs.remove_file(&partial);
        return Err(ApplyError::Fs {
            path: partial,
            source: io::Error::new(io::ErrorKind::InvalidData, format!("short copy: {dest_len} of {src_len} bytes")),
        });
    }
    // Publish partial → final (no clobber), THEN make the rename durable.
    // ORDER MATTERS (spec §8 rev 3): the source unlink below happens on a
    // DIFFERENT filesystem, so the rename's directory entry must be durable
    // before the source disappears — power loss could otherwise persist the
    // unlink while the rename is lost, leaving the data reachable only as a
    // hidden `.partial`. (rev 2 fsynced the dir BEFORE the rename, which made
    // the `.partial` entry durable instead of the final one.)
    match fs.rename_noreplace(&partial, to) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let _ = fs.remove_file(&partial);
            return Err(ApplyError::Collision(to.to_path_buf()));
        }
        Err(e) => {
            let _ = fs.remove_file(&partial);
            return Err(ApplyError::Fs { path: to.to_path_buf(), source: e });
        }
    }
    if let Some(dir) = to.parent() {
        if let Err(e) = fs.fsync_dir(dir) {
            // The final is already published — do NOT touch it or the source if
            // durability can't be proven. Stop loudly; worst case a duplicate
            // (source + dest both present), never a loss.
            return Err(ApplyError::Fs { path: dir.to_path_buf(), source: e });
        }
    }
    // ONLY NOW remove the verified, durably-published source (the sole unlink in v1).
    fs.remove_file(from).map_err(|e| ApplyError::Fs { path: from.to_path_buf(), source: e })
}
```

- [ ] **Step 4: Run to verify pass**
Run: `cargo test -p culler-core apply::tests`
Expected: PASS.

- [ ] **Step 5: Commit**
```bash
git add culler-core/src/apply.rs
git commit -m "feat(apply): cross-FS EXDEV copy-fsync-verify-rename-remove path" -m "Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 6: Collision at apply time — NOREPLACE fails loudly

**Files:**
- Test only: `culler-core/src/apply.rs` (inline) — `move_one` already maps `EEXIST` → `Collision`.

**Interfaces:**
- Consumes: Task-3/5 `move_one` (`EEXIST` → `ApplyError::Collision`), `FakeFs::seed_file` to place a surprise destination file.
- Produces: verified guarantee that a destination file appearing between plan and apply is never overwritten — the run stops with `ApplyError::Collision(path)` and the source stays put.

> This behaviour was implemented in Task 3; this task adds the dedicated §11 "collision appearing between plan and apply" test. If it passes immediately, that is expected — do not weaken it. (Confirm it exercises a genuinely new path by first asserting it FAILS against a deliberately-broken `move_one` in Step 2's note, then reverting.)

- [ ] **Step 1: Write the failing test**

Append inside `apply::tests`:
```rust
    #[test]
    fn apply_collision_between_plan_and_apply_fails_loudly() {
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot("IMG_0200", BUCKET_KEEP, &[("IMG_0200.JPG", 100)], &dest);
        let plan = plan_of(&dest, vec![s1], b1);

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops);
        // A file materialized at the destination AFTER planning.
        let target = dest.join(BUCKET_KEEP).join("IMG_0200.JPG");
        fs.seed_file(target.clone(), 999);

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");

        let err = apply(&plan, &fs, &jpath).unwrap_err();
        match err {
            ApplyError::Collision(p) => assert_eq!(p, target),
            other => panic!("expected Collision, got {other:?}"),
        }
        // NEVER overwritten; source stays put.
        assert_eq!(fs.len_of(&target), Some(999), "existing dest file untouched");
        assert!(fs.exists(&PathBuf::from("/src/IMG_0200.JPG")), "source not moved");
    }
```

- [ ] **Step 2: Run to verify it fails**
Run: `cargo test -p culler-core apply::tests::apply_collision_between_plan_and_apply_fails_loudly`
Expected: PASS (behaviour landed in Task 3). To prove the test has teeth, temporarily change `move_one`'s `AlreadyExists` arm to `Ok(())` and re-run → it FAILS ("expected Collision"); then revert. Document this check in the commit body.

- [ ] **Step 3: Minimal implementation**
No production change — the `EEXIST → Collision` mapping already exists in `move_one` (both the same-FS rename and the cross-FS publish rename). Verify by re-reading `move_one` / `move_cross_fs`.

- [ ] **Step 4: Run to verify pass**
Run: `cargo test -p culler-core apply::tests`
Expected: PASS.

- [ ] **Step 5: Commit**
```bash
git add culler-core/src/apply.rs
git commit -m "test(apply): NOREPLACE collision at apply time fails loudly, no clobber" -m "Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 7: Group atomicity — later file fails → stop + precise report

**Files:**
- Test only: `culler-core/src/apply.rs` (inline).

**Interfaces:**
- Consumes: Task-4 incremental journaling + `FakeFs::deny_rename_from`.
- Produces: verified group atomicity — when a later file in a shot fails, the already-moved files stay recorded `Done` in the journal, the run stops with a precise `ApplyError`, and the durable journal identifies the stopped stem (its first non-`Done` op).

- [ ] **Step 1: Write the failing test**

Append inside `apply::tests`:
```rust
    /// Stem of the shot where the run stopped: the op owning the first non-`Done` move.
    fn stopped_stem(j: &Journal) -> Option<String> {
        let mut gidx = 0usize;
        for op in &j.plan.ops {
            for _ in &op.moves {
                if j.statuses[gidx] != OpState::Done {
                    return Some(op.stem.clone());
                }
                gidx += 1;
            }
        }
        None
    }

    #[test]
    fn apply_group_atomicity_stops_and_records_partial() {
        let dest = PathBuf::from("/dst");
        // shot A completes; shot B fails on its RAW (second file) → stop at B.
        let (a, ba) = shot("IMG_0300", BUCKET_KEEP, &[("IMG_0300.JPG", 100)], &dest);
        let (b, bb) = shot("IMG_0301", BUCKET_PICKS, &[("IMG_0301.JPG", 100), ("IMG_0301.CR3", 100), ("IMG_0301.xmp", 100)], &dest);
        let plan = plan_of(&dest, vec![a, b], ba + bb);

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops);
        fs.deny_rename_from("/src/IMG_0301.CR3"); // later file in shot B

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");

        let err = apply(&plan, &fs, &jpath).unwrap_err();
        assert!(matches!(err, ApplyError::Fs { .. }));

        // Shot A fully moved; shot B's first file moved, its RAW + xmp still at source.
        assert!(!fs.exists(&PathBuf::from("/src/IMG_0300.JPG")));
        assert!(!fs.exists(&PathBuf::from("/src/IMG_0301.JPG")));
        assert!(fs.exists(&PathBuf::from("/src/IMG_0301.CR3")));
        assert!(fs.exists(&PathBuf::from("/src/IMG_0301.xmp")));

        // Durable journal is the stop-of-record.
        let j: Journal = serde_json::from_slice(&std::fs::read(&jpath).unwrap()).unwrap();
        assert_eq!(j.statuses, vec![OpState::Done, OpState::Done, OpState::Failed, OpState::Pending]);
        assert_eq!(stopped_stem(&j), Some("IMG_0301".to_string())); // ApplyReport.stopped_at equivalent
    }
```

- [ ] **Step 2: Run to verify it fails**
Run: `cargo test -p culler-core apply::tests::apply_group_atomicity_stops_and_records_partial`
Expected: PASS (group-atomic stop + incremental journaling already implemented in Tasks 3–4). Prove teeth: temporarily make the `Err` arm in `execute` `continue` instead of `return Err(e)` → the run would keep going and the journal would not stop at B; re-run FAILS; then revert. Note the check in the commit.

- [ ] **Step 3: Minimal implementation**
No production change — the `execute` loop returns on the first failing move, leaving prior moves `Done` and the rest `Pending`/`Failed`. `stopped_stem` is a test helper mirroring how the Phase-6 binary materializes `ApplyReport.stopped_at` from the journal.

- [ ] **Step 4: Run to verify pass**
Run: `cargo test -p culler-core apply::tests`
Expected: PASS.

- [ ] **Step 5: Commit**
```bash
git add culler-core/src/apply.rs
git commit -m "test(apply): group atomicity stops at failing shot with durable partial journal" -m "Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 8: Crash mid-apply → `resume()` recovery

**Files:**
- Modify: `culler-core/src/apply.rs` (add `resume`)
- Test: `culler-core/src/apply.rs` (inline)

**Interfaces:**
- Consumes: `execute` (skips `Done`), `write_journal`, `FakeFs::{clear_faults, seed_file}`.
- Produces: `pub fn resume(journal_path: &Path, fs: &dyn FsOps) -> Result<ApplyReport, ApplyError>` — reads the on-disk journal, **reconciles it against the disk in both directions (spec §8 rev 3)**, skips `Done` moves, continues the rest to completion, and removes the journal on success. Private `fn reconcile(&mut Journal, &dyn FsOps)`.

> **Why reconciliation is load-bearing:** a crash can land *between* a move and
> its journal update (move done, journal still `Pending`) or — with batched
> fsyncs — the reverse (journal says `Done`, the rename never became durable).
> Without reconciliation, resume re-runs a completed move and dies on a bogus
> `ENOENT` (source gone) or `EEXIST`-as-`Collision` (dest occupied by its own
> work) — precisely the "forensic mystery" the spec forbids. Reconciliation is
> also what makes the Task-4 fsync batching safe.

- [ ] **Step 1: Write the failing test**

Append inside `apply::tests`:
```rust
    #[test]
    fn resume_continues_a_crashed_run_from_the_journal() {
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot(
            "IMG_0400",
            BUCKET_KEEP,
            &[("IMG_0400.JPG", 100), ("IMG_0400.CR3", 100), ("IMG_0400.xmp", 100)],
            &dest,
        );
        let plan = plan_of(&dest, vec![s1], b1);

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops);
        fs.deny_rename_from("/src/IMG_0400.CR3"); // crash on the second file

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");

        // First run stops at the RAW; JPEG already moved.
        let _ = apply(&plan, &fs, &jpath).unwrap_err();
        assert!(!fs.exists(&PathBuf::from("/src/IMG_0400.JPG")));
        assert!(fs.exists(&PathBuf::from("/src/IMG_0400.CR3")));

        // The fault clears (e.g. permissions fixed); resume from the same journal.
        fs.clear_faults();
        let report = resume(&jpath, &fs).unwrap();

        // Only the not-yet-done files moved this run; JPEG skipped (already Done).
        assert_eq!(report.moved_files, 2);
        assert!(!fs.exists(&PathBuf::from("/src/IMG_0400.CR3")));
        assert!(!fs.exists(&PathBuf::from("/src/IMG_0400.xmp")));
        assert_eq!(fs.len_of(&dest.join(BUCKET_KEEP).join("IMG_0400.CR3")), Some(100));

        // Success retires the journal (spec §8 rev 3).
        assert!(!jpath.exists(), "journal removed once the resume completes");
    }

    #[test]
    fn resume_reconciles_crash_between_move_and_journal_update() {
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot("IMG_0410", BUCKET_KEEP, &[("IMG_0410.JPG", 100), ("IMG_0410.CR3", 100)], &dest);
        let plan = plan_of(&dest, vec![s1], b1);

        let fs = FakeFs::new();
        // The crashed run moved the JPG but died BEFORE journaling it:
        fs.seed_file(dest.join(BUCKET_KEEP).join("IMG_0410.JPG"), 100);
        fs.seed_file("/src/IMG_0410.CR3", 100);

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");
        let stale = Journal { plan: plan.clone(), statuses: vec![OpState::Pending, OpState::Pending] };
        std::fs::write(&jpath, serde_json::to_vec(&stale).unwrap()).unwrap();

        // rev 3: reconciliation sees from-gone + to-present ⇒ Done, so resume
        // completes instead of dying on ENOENT / EEXIST-as-Collision.
        let report = resume(&jpath, &fs).unwrap();
        assert_eq!(report.moved_files, 1, "only the CR3 actually moved this run");
        assert_eq!(fs.len_of(&dest.join(BUCKET_KEEP).join("IMG_0410.CR3")), Some(100));
        assert!(!jpath.exists(), "journal removed on success");
    }

    #[test]
    fn resume_reexecutes_a_done_move_the_disk_never_saw() {
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot("IMG_0420", BUCKET_KEEP, &[("IMG_0420.JPG", 100)], &dest);
        let plan = plan_of(&dest, vec![s1], b1);

        let fs = FakeFs::new();
        // Journal was fsynced Done, but the rename itself was lost to the crash:
        fs.seed_file("/src/IMG_0420.JPG", 100);

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");
        let stale = Journal { plan: plan.clone(), statuses: vec![OpState::Done] };
        std::fs::write(&jpath, serde_json::to_vec(&stale).unwrap()).unwrap();

        // rev 3: Done + dest-missing + source-present ⇒ re-execute, not skip —
        // otherwise the shot is silently left behind while the run reports success.
        let report = resume(&jpath, &fs).unwrap();
        assert_eq!(report.moved_files, 1);
        assert_eq!(fs.len_of(&dest.join(BUCKET_KEEP).join("IMG_0420.JPG")), Some(100));
        assert!(!fs.exists(&PathBuf::from("/src/IMG_0420.JPG")));
    }
```

- [ ] **Step 2: Run to verify it fails**
Run: `cargo test -p culler-core apply::tests::resume_continues_a_crashed_run_from_the_journal`
Expected: FAIL — `cannot find function resume`.

- [ ] **Step 3: Minimal implementation**

Add to `culler-core/src/apply.rs`:
```rust
/// Read `bytes` from disk and deserialize a journal.
fn read_journal(path: &Path) -> Result<Journal, ApplyError> {
    let bytes = std::fs::read(path).map_err(|e| ApplyError::Fs { path: path.to_path_buf(), source: e })?;
    serde_json::from_slice(&bytes).map_err(|e| ApplyError::Fs {
        path: path.to_path_buf(),
        source: io::Error::new(io::ErrorKind::InvalidData, e),
    })
}

/// The crash window can strand the journal on either side of reality (spec §8
/// rev 3). Reconcile it against the observable filesystem BEFORE executing:
///  - `Pending`/`Failed` move whose source is GONE and destination EXISTS →
///    the crashed run already did it: mark `Done` (a re-run would fail ENOENT
///    or, worse, surface its own work as a Collision).
///  - `Done` move whose destination is MISSING while the source still exists →
///    the journal outran a lost rename: mark `Pending`, re-execute.
/// Anything else (both present, both absent) is left alone and will surface
/// loudly through the normal NOREPLACE/ENOENT paths.
fn reconcile(journal: &mut Journal, fs: &dyn FsOps) {
    let mut gidx = 0usize;
    let ops = journal.plan.ops.clone();
    for op in &ops {
        for mv in &op.moves {
            let src = fs.file_len(&mv.from).is_ok();
            let dst = fs.file_len(&mv.to).is_ok();
            match journal.statuses[gidx] {
                OpState::Pending | OpState::Failed if !src && dst => {
                    journal.statuses[gidx] = OpState::Done;
                }
                OpState::Done if !dst && src => {
                    journal.statuses[gidx] = OpState::Pending;
                }
                _ => {}
            }
            gidx += 1;
        }
    }
}

/// Resume a crashed/aborted run from its journal: reconcile against the disk
/// (rev 3), skip `Done` moves, continue the rest; the journal is removed on
/// success by `execute`. Detected + offered on next launch (the offer UX is
/// Phase 6). Does not re-run the free-space preflight — the journal is trusted
/// as the source of truth for WHAT to do; the disk for what already happened.
pub fn resume(journal_path: &Path, fs: &dyn FsOps) -> Result<ApplyReport, ApplyError> {
    let mut journal = read_journal(journal_path)?;
    reconcile(&mut journal, fs);
    execute(&mut journal, fs, journal_path)
}
```

- [ ] **Step 4: Run to verify pass**
Run: `cargo test -p culler-core apply::tests`
Expected: PASS.

- [ ] **Step 5: Commit**
```bash
git add culler-core/src/apply.rs
git commit -m "feat(apply): reconciling resume() heals journal-disk drift, skips Done moves" -m "Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 9: Free-space preflight refuse + mid-copy ENOSPC leaves source intact

**Files:**
- Modify: `culler-core/src/apply.rs` (add `preflight`, wire into `apply`)
- Test: `culler-core/src/apply.rs` (inline)

**Interfaces:**
- Consumes: `FsOps::{same_filesystem, free_space}`, `ApplyPlan::total_bytes`, `FakeFs::{set_free, set_cross_fs, set_enospc_on_copy}`.
- Produces: `preflight(plan, fs)` — when the move crosses filesystems and `free_space(dest) < total_bytes`, returns `ApplyError::Preflight(..)` **before any move**; the mid-copy ENOSPC path (space passes preflight but a copy fails) leaves the source untouched.

- [ ] **Step 1: Write the failing test**

Append inside `apply::tests`:
```rust
    #[test]
    fn preflight_refuses_when_cross_fs_and_not_enough_space() {
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot("IMG_0500", BUCKET_KEEP, &[("IMG_0500.JPG", 100), ("IMG_0500.CR3", 100)], &dest);
        let plan = plan_of(&dest, vec![s1], b1); // total_bytes = 200

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops);
        fs.set_cross_fs(true);
        fs.set_free(150); // < 200

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");

        let err = apply(&plan, &fs, &jpath).unwrap_err();
        assert!(matches!(err, ApplyError::Preflight(_)));
        // Refused BEFORE any move — sources all intact, no journal, no buckets.
        assert!(fs.exists(&PathBuf::from("/src/IMG_0500.JPG")));
        assert!(fs.exists(&PathBuf::from("/src/IMG_0500.CR3")));
        assert!(!jpath.exists(), "no journal written when preflight refuses");
    }

    #[test]
    fn same_fs_never_free_space_gated() {
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot("IMG_0501", BUCKET_KEEP, &[("IMG_0501.JPG", 100)], &dest);
        let plan = plan_of(&dest, vec![s1], b1);

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops);
        fs.set_free(0); // irrelevant: same-FS rename moves no bytes

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");
        apply(&plan, &fs, &jpath).unwrap(); // succeeds despite free==0
        assert!(!fs.exists(&PathBuf::from("/src/IMG_0501.JPG")));
    }

    #[test]
    fn mid_copy_enospc_leaves_source_untouched() {
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot("IMG_0502", BUCKET_KEEP, &[("IMG_0502.JPG", 100)], &dest);
        let plan = plan_of(&dest, vec![s1], b1);

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops);
        fs.set_cross_fs(true);
        fs.set_free(u64::MAX);       // preflight passes
        fs.set_enospc_on_copy(true); // but the copy itself fails ENOSPC

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");

        let err = apply(&plan, &fs, &jpath).unwrap_err();
        assert!(matches!(err, ApplyError::Fs { .. }));
        // Source intact, no final, no leftover partial.
        assert!(fs.exists(&PathBuf::from("/src/IMG_0502.JPG")));
        assert!(!fs.exists(&dest.join(BUCKET_KEEP).join("IMG_0502.JPG")));
        assert!(!fs.exists(&dest.join(BUCKET_KEEP).join(".IMG_0502.JPG.partial")));
    }
```

- [ ] **Step 2: Run to verify it fails**
Run: `cargo test -p culler-core apply::tests::preflight_refuses_when_cross_fs_and_not_enough_space`
Expected: FAIL — `apply` has no preflight yet, so it proceeds and the cross-FS copy path drains `free` until it hits ENOSPC (or otherwise) instead of refusing up front (`ApplyError::Preflight`), and a journal gets written.

- [ ] **Step 3: Minimal implementation**

Add `preflight` and call it first in `apply`:
```rust
/// Refuse a cross-filesystem run that cannot fit. Same-FS runs move no bytes and
/// are never gated. Uses the first source file to decide FS-crossing vs `dest`.
fn preflight(plan: &ApplyPlan, fs: &dyn FsOps) -> Result<(), ApplyError> {
    let first_from = plan.ops.iter().flat_map(|o| o.moves.iter()).map(|m| &m.from).next();
    if let Some(src) = first_from {
        let same = fs
            .same_filesystem(src, &plan.dest)
            .map_err(|e| ApplyError::Fs { path: plan.dest.clone(), source: e })?;
        if !same {
            let avail = fs
                .free_space(&plan.dest)
                .map_err(|e| ApplyError::Fs { path: plan.dest.clone(), source: e })?;
            if avail < plan.total_bytes {
                return Err(ApplyError::Preflight(format!(
                    "insufficient free space at {}: need {} bytes, {} available",
                    plan.dest.display(),
                    plan.total_bytes,
                    avail
                )));
            }
        }
    }
    Ok(())
}
```
Update `apply` to run preflight before journalling:
```rust
pub fn apply(plan: &ApplyPlan, fs: &dyn FsOps, journal_path: &Path) -> Result<ApplyReport, ApplyError> {
    preflight(plan, fs)?; // refuse rather than abort halfway
    let mut journal = Journal {
        plan: plan.clone(),
        statuses: vec![OpState::Pending; total_move_count(plan)],
    };
    write_journal(&journal, journal_path, true)?; // JOURNAL FIRST — durable before any move
    execute(&mut journal, fs, journal_path)
}
```

- [ ] **Step 4: Run to verify pass**
Run: `cargo test -p culler-core apply::tests`
Expected: PASS — preflight-refuse, same-FS-not-gated, and mid-copy-ENOSPC-source-intact all green.

- [ ] **Step 5: Commit**
```bash
git add culler-core/src/apply.rs
git commit -m "feat(apply): cross-FS free-space preflight refuse; ENOSPC leaves source intact" -m "Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 10: Permission error surfaced per file

**Files:**
- Test only: `culler-core/src/apply.rs` (inline).

**Interfaces:**
- Consumes: `FakeFs::{deny_rename_from, deny_remove}`, `ApplyError::Fs`.
- Produces: verified per-file surfacing — a permission failure stops the run with `ApplyError::Fs { path, source }` naming the exact file, `source.kind() == PermissionDenied`, and no data loss.

- [ ] **Step 1: Write the failing test**

Append inside `apply::tests`:
```rust
    #[test]
    fn permission_error_is_surfaced_per_file() {
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot("IMG_0600", BUCKET_KEEP, &[("IMG_0600.JPG", 100)], &dest);
        let plan = plan_of(&dest, vec![s1], b1);

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops);
        fs.deny_rename_from("/src/IMG_0600.JPG");

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");

        let err = apply(&plan, &fs, &jpath).unwrap_err();
        match err {
            ApplyError::Fs { path, source } => {
                assert_eq!(path, PathBuf::from("/src/IMG_0600.JPG"));
                assert_eq!(source.kind(), std::io::ErrorKind::PermissionDenied);
            }
            other => panic!("expected Fs error, got {other:?}"),
        }
        assert!(fs.exists(&PathBuf::from("/src/IMG_0600.JPG")), "no data lost");
    }

    #[test]
    fn cross_fs_permission_on_source_remove_surfaces_but_copy_is_published() {
        // The verified copy is already published to dest before source removal;
        // a permission error on the final unlink surfaces the source path.
        let dest = PathBuf::from("/dst");
        let (s1, b1) = shot("IMG_0601", BUCKET_KEEP, &[("IMG_0601.JPG", 100)], &dest);
        let plan = plan_of(&dest, vec![s1], b1);

        let fs = FakeFs::new();
        seed_sources(&fs, &plan.ops);
        fs.set_cross_fs(true);
        fs.set_free(u64::MAX);
        fs.deny_remove("/src/IMG_0601.JPG"); // final unlink denied

        let journal = tempfile::tempdir().unwrap();
        let jpath = journal.path().join(".fastcull-apply.json");

        let err = apply(&plan, &fs, &jpath).unwrap_err();
        match err {
            ApplyError::Fs { path, source } => {
                assert_eq!(path, PathBuf::from("/src/IMG_0601.JPG"));
                assert_eq!(source.kind(), std::io::ErrorKind::PermissionDenied);
            }
            other => panic!("expected Fs error, got {other:?}"),
        }
        // Copy is durably published; source lingers (a duplicate, never a loss).
        assert_eq!(fs.len_of(&dest.join(BUCKET_KEEP).join("IMG_0601.JPG")), Some(100));
        assert!(fs.exists(&PathBuf::from("/src/IMG_0601.JPG")));
    }
```

- [ ] **Step 2: Run to verify it fails**
Run: `cargo test -p culler-core apply::tests::permission_error_is_surfaced_per_file`
Expected: PASS (surfacing landed in Tasks 3/5). Prove teeth by temporarily mapping `move_one`'s catch-all `Err` arm to `Ok(())` → the permission test FAILS; then revert. Note in the commit body.

- [ ] **Step 3: Minimal implementation**
No production change — `move_one` already returns `ApplyError::Fs { path: from, source }` for non-EEXIST/EXDEV rename errors, and `move_cross_fs` surfaces the source path on the final `remove_file` failure.

- [ ] **Step 4: Run to verify pass**
Run: `cargo test -p culler-core apply::tests`
Expected: PASS.

- [ ] **Step 5: Commit**
```bash
git add culler-core/src/apply.rs
git commit -m "test(apply): permission failures surfaced per file with no data loss" -m "Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 11: Sidecar written during apply (real FS)

**Files:**
- Test only: `culler-core/src/apply.rs` (inline, `RealFs` + `tempfile`).

**Interfaces:**
- Consumes: `RealFs` (Task 1), `crate::xmp::write_sidecar` (Phase 3), `ShotOp::write_sidecar: Option<SidecarWrite>` (Phase 3 refined shape).
- Produces: verified integration — after a shot's files move, a `Some(SidecarWrite)` writes a fresh `.xmp` into the bucket (real I/O via `xmp::write_sidecar`) and is counted in `ApplyReport::sidecars_written`.

> Sidecar writes are real filesystem I/O (they do NOT route through `FsOps`), so this test uses `RealFs` + a temp dir — mixing `FakeFs` (in-memory) with a real `xmp` write is meaningless. All `FakeFs` apply tests keep `write_sidecar: None`; this task is where the sidecar path is exercised end-to-end.

- [ ] **Step 1: Write the failing test**

Append inside `apply::tests`:
```rust
    use crate::fsops::RealFs;
    use crate::plan::SidecarWrite;

    #[test]
    fn apply_writes_fresh_sidecar_into_bucket_realfs() {
        let root = tempfile::tempdir().unwrap();
        let src_dir = root.path().join("src");
        let dest = root.path().join("dst");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::create_dir_all(&dest).unwrap();

        let jpg = src_dir.join("IMG_0700.JPG");
        std::fs::write(&jpg, vec![1u8; 128]).unwrap();

        let bucket = crate::model::BUCKET_KEEP;
        let sidecar_path = dest.join(bucket).join("IMG_0700.xmp");
        let op = ShotOp {
            stem: "IMG_0700".into(),
            bucket: bucket.into(),
            moves: vec![FileMove { from: jpg.clone(), to: dest.join(bucket).join("IMG_0700.JPG") }],
            write_sidecar: Some(SidecarWrite {
                path: sidecar_path.clone(),
                tags: vec!["portrait".into(), "golden-hour".into()],
                rating: Some(3), // Keep → 3
            }),
            suffix: None,
        };
        let plan = plan_of(&dest, vec![op], 128);

        let jpath = dest.join(".fastcull-apply.json");
        let report = apply(&plan, &RealFs, &jpath).unwrap();

        // File moved; fresh sidecar written into the bucket and counted.
        assert!(dest.join(bucket).join("IMG_0700.JPG").exists());
        assert!(!jpg.exists());
        assert_eq!(report.sidecars_written, 1);
        assert!(sidecar_path.exists());
        let xmp = std::fs::read_to_string(&sidecar_path).unwrap();
        assert!(xmp.contains("portrait"), "dc:subject keyword present");
        assert!(xmp.contains("golden-hour"));
        assert!(xmp.contains("3"), "xmp:Rating present");
    }
```
(Reuse the `buckets()` / `plan_of()` helpers from Task 3; only `SidecarWrite` and `RealFs` imports are new.)

- [ ] **Step 2: Run to verify it fails**
Run: `cargo test -p culler-core apply::tests::apply_writes_fresh_sidecar_into_bucket_realfs`
Expected: PASS (sidecar write landed in Task 3's `execute`). To prove teeth, temporarily comment out the `write_sidecar` block in `execute` → the test FAILS (`sidecars_written == 0`, sidecar missing); then revert. Note in the commit body.

- [ ] **Step 3: Minimal implementation**
No production change — `execute` already calls `crate::xmp::write_sidecar(&sw.path, &sw.tags, sw.rating)` per shot when `write_sidecar` is `Some` and increments `sidecars_written`.

- [ ] **Step 4: Run to verify pass**
Run: `cargo test -p culler-core apply`
Expected: PASS — the full apply test matrix (Tasks 3–11) is green.

- [ ] **Step 5: Commit**
```bash
git add culler-core/src/apply.rs
git commit -m "test(apply): fresh XMP sidecar written into bucket during apply (RealFs)" -m "Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Phase 4 completion checklist

- [ ] `culler-core/src/fsops.rs`: `FsOps` trait, `RealFs` (rustix), `#[cfg(test)] FakeFs` double.
- [ ] `culler-core/src/apply.rs`: `OpState`, `Journal`, `ApplyError`, `ApplyReport`, `apply`, `resume`.
- [ ] `lib.rs` exports `pub mod fsops;` and `pub mod apply;`.
- [ ] `Cargo.toml`: `rustix` (features `["fs"]`) dependency, `tempfile` dev-dependency.
- [ ] §11 highest-value matrix covered: same-FS rename (T3, +RealFs T1), injected-EXDEV copy-verify **with rename-before-dir-fsync ORDER asserted on the event log** (T5), collision between plan and apply fails loudly (T6), group atomicity stop+report (T7), crash-mid-apply → resume **including both reconciliation directions** (T8), preflight refuse + mid-copy ENOSPC source-intact (T9), permission failures (T10), sidecar write **+ skip-idempotent re-run** (T11), journal-first + incremental Done + **journal removed on success** (T3/T4).
- [ ] `cargo test -p culler-core` fully green; **no BLAKE3, no delete step for user data** anywhere (journal/partial/tmp cleanup is bookkeeping, not deletion).

## Notes for the executor — spec ambiguities & how they were resolved

1. **`ApplyReport.stopped_at` vs. the `Result<_, ApplyError>` signature (the one real ambiguity).** The README pairs an error-returning signature with a `stopped_at` report field; a single call cannot return both an `Err` and a populated report. **Resolution:** every per-file failure returns `Err(ApplyError)` (precise variant — honoring "fail loudly" / "precise ApplyError" / "surfaced per file"), and the durable on-disk journal is the stop-of-record (its first non-`Done` op is the stopped stem). `resume` continues from exactly there. `ApplyReport.stopped_at` is `None` on all Phase-4 `Ok` returns; it is reserved for the Phase-6 binary to materialize when rendering a failed run's journal + error. Tests assert the stop via the journal (`stopped_stem` helper) plus the `Err` variant. If the executor prefers `stopped_at` be populated in Phase 4, the minimal change is to add it to the report the binary builds — not to `apply`/`resume`, whose README signatures are kept stable.

2. **`ShotOp.write_sidecar` shape.** The README shows `Option<FileMove>`; Phase 3 refined it to `Option<SidecarWrite>` (`{ path, tags, rating }`). This phase consumes the **refined** shape and calls `crate::xmp::write_sidecar(&sw.path, &sw.tags, sw.rating)`. If Phase 3's committed type differs, adjust the two `write_sidecar` call sites and the Task-11 test only.

3. **Session-file relocation ownership (spec §6 "on success, the session file moves into the destination").** Deliberately **out of scope for Phase 4** and NOT wired into `apply`, to keep the README `apply`/`resume` signatures stable (they take only `plan`, `fs`, `journal_path`). The session-file handoff is left to the **Phase 6 binary/main**, which knows the concrete `source/.fastcull.json` and dest paths and can move it with `RealFs` after `apply` returns `Ok`. No `apply_with_session` wrapper is introduced (YAGNI). This is noted here so Phase 6 owns it.

4. **Journal I/O is real, not `FsOps`.** The journal must survive a real power-loss crash, so `write_journal`/`read_journal` use `std::fs` (temp → `sync_all` → atomic rename) directly rather than the injectable `FsOps`. FakeFs apply tests therefore pass a **real** temp `journal_path` while their file moves stay in-memory — this is intentional and lets the crash-recovery test (T8) round-trip a real journal.

5. **Rev-3 hardening summary (2026-07-09 plan audit).** Four changes relative to the original rev of this phase: (a) cross-FS dir fsync moved *after* the publish rename (a spec §8 defect this plan had faithfully copied); (b) `resume` reconciles journal↔disk in both directions, which also licenses (c) batched journal fsyncs (every 64th move instead of every move — a 4k-file shoot no longer pays 4k fsyncs of a plan-sized JSON); (d) the journal is removed on success and sidecar writes are skip-idempotent no-clobber, so a finished run can never masquerade as a crashed one, hijack a later apply, or overwrite anything.

6. **`same_filesystem` heuristic for preflight.** `RealFs::same_filesystem` compares `st_dev` of each path's containing directory (spec §8). For a top-level distinct-mount dest this can misjudge in a rare edge case, but preflight is only an optimization to fail fast on space — the per-file move path handles `EXDEV` correctly regardless, so a wrong preflight verdict never causes data loss (worst case: a cross-FS run isn't space-pre-checked and instead surfaces `ENOSPC` mid-copy with the source left intact, per T9).
