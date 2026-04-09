//! Cross-process advisory lock around a project's build cache.
//!
//! Two `gluon build` invocations against the same project share the
//! same `cache-manifest.json`. Without coordination they race
//! catastrophically: each one reads the manifest at startup, mutates
//! its in-memory copy, and writes it back at the end. The atomic
//! write-rename in `manifest::save_atomic` keeps the file from being
//! corrupted at the byte level, but it can't stop "last writer wins"
//! from silently dropping the other build's freshness records.
//!
//! [`CacheLock`] is the fix: a `flock(2)`-style advisory exclusive lock
//! held on a sibling lockfile (`cache-manifest.json.lock`) for the
//! entire duration of a build. Acquire it before reading the manifest
//! and drop it after the final save; concurrent builds in the same
//! project then serialise on the lock instead of clobbering each other.
//!
//! ### Why advisory and not the manifest itself?
//!
//! - We need to lock *across* the read and the write, with the manifest
//!   file being deleted/recreated by the atomic-rename in between. A
//!   lock on the manifest itself would be invalidated the moment
//!   `save_atomic` did its rename.
//! - A separate lockfile is also a good place to add liveness tags
//!   later (PID, hostname, "this build is still running") if we ever
//!   want richer wait diagnostics. For MVP-M the file is just a
//!   coordination point.
//!
//! ### Why not a mutex?
//!
//! In-process mutexes don't help across `gluon` invocations from a
//! script-on-save editor + a terminal rebuild + a CI pre-commit hook —
//! every common multi-build scenario is multi-process.
//!
//! ### Build vs clean
//!
//! Both [`crate::build`] and [`crate::clean`] take this lock. Clean
//! racing a build is just as dangerous as build-vs-build: clean would
//! `rm -rf` files the builder is in the middle of writing.

use crate::compile::BuildLayout;
use crate::error::{Error, Result};
use fs2::FileExt;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

/// RAII guard holding an exclusive advisory lock on the project's cache
/// lockfile. The lock is released when the guard is dropped.
///
/// The held [`File`] is intentionally private — there's nothing to read
/// or write inside it; the file exists purely as a stable inode for the
/// kernel's lock table.
#[must_use = "dropping the CacheLock immediately releases the lock"]
pub struct CacheLock {
    // Held to keep the OS lock alive. Dropping releases it via the
    // platform-specific unlock_exclusive in `Drop`.
    file: File,
    path: PathBuf,
}

impl CacheLock {
    /// Acquire the cache lock for the given build layout.
    ///
    /// The lockfile lives at `<build_root>/cache-manifest.json.lock`.
    /// `<build_root>` is created if it doesn't already exist — `build`
    /// is going to need it anyway, and `clean` short-circuits before it
    /// would call this on a missing build dir, so creating it here is
    /// always safe.
    ///
    /// Acquisition strategy: try a non-blocking lock first; if that
    /// fails (another gluon process is holding it), print a one-line
    /// notice to stderr and fall back to a blocking acquisition. The
    /// notice prevents the user from staring at a frozen terminal
    /// wondering why nothing is happening — the most common confusing
    /// failure mode of locking systems.
    pub fn acquire(layout: &BuildLayout) -> Result<Self> {
        let lock_path = lockfile_path(layout);
        // Make sure the parent dir exists. The build path itself is
        // about to need it; doing it here keeps the lock acquisition
        // self-contained.
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Io {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| Error::Io {
                path: lock_path.clone(),
                source: e,
            })?;

        // Try a non-blocking acquisition first so the common single-process
        // case is silent. fs2 returns WouldBlock when another process
        // holds the lock; everything else is a real error.
        match file.try_lock_exclusive() {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                eprintln!(
                    "gluon: waiting for another build in {}",
                    layout.root().display()
                );
                file.lock_exclusive().map_err(|e| Error::Io {
                    path: lock_path.clone(),
                    source: e,
                })?;
            }
            Err(e) => {
                return Err(Error::Io {
                    path: lock_path,
                    source: e,
                });
            }
        }

        Ok(Self {
            file,
            path: lock_path,
        })
    }

    /// Path of the lockfile this guard is holding. Mostly useful for
    /// diagnostics and tests.
    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for CacheLock {
    fn drop(&mut self) {
        // Best-effort unlock. fs2 also drops the lock when the file
        // descriptor closes (which is about to happen as `file` itself
        // drops), so a failure here is harmless — we just won't get a
        // log line for it.
        let _ = FileExt::unlock(&self.file);
    }
}

/// Compute the lockfile path for a layout. Defined as
/// `<cache-manifest>.lock` so it's obviously a sibling and `gluon
/// clean` (which removes the build dir) takes it down too.
fn lockfile_path(layout: &BuildLayout) -> PathBuf {
    let manifest = layout.cache_manifest();
    let mut name = manifest
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_else(|| "cache-manifest.json".into());
    name.push(".lock");
    manifest.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_layout(tmp: &Path) -> BuildLayout {
        BuildLayout::new(tmp.join("build"), "testproject")
    }

    #[test]
    fn lockfile_path_is_sibling_of_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = make_layout(tmp.path());
        let lock = lockfile_path(&layout);
        let manifest = layout.cache_manifest();
        assert_eq!(lock.parent(), manifest.parent());
        assert_eq!(
            lock.file_name().unwrap().to_str().unwrap(),
            "cache-manifest.json.lock"
        );
    }

    #[test]
    fn acquire_creates_build_dir_and_lockfile() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = make_layout(tmp.path());
        // build_dir does not exist yet — acquire must create it.
        assert!(!layout.root().exists());

        let guard = CacheLock::acquire(&layout).expect("acquire");
        assert!(layout.root().exists());
        assert!(guard.path().exists());
        drop(guard);
    }

    #[test]
    fn drop_releases_lock_so_reacquisition_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = make_layout(tmp.path());

        let g1 = CacheLock::acquire(&layout).expect("first acquire");
        drop(g1);
        // After drop, a second acquisition must succeed without
        // blocking. We use a tight loop with try_lock_exclusive on a
        // raw file to verify that — the second `acquire` would
        // otherwise hang the test if drop didn't release.
        let _g2 = CacheLock::acquire(&layout).expect("second acquire");
    }

    #[test]
    fn try_lock_blocks_on_held_lock_in_separate_process_via_fs2() {
        // We simulate cross-process contention with a second `File`
        // handle pointing at the same lockfile inode. fs2's lock
        // semantics on POSIX (flock) and Windows (LockFileEx) reject a
        // second exclusive lock from a different file descriptor even
        // within the same process. This makes the assertion below a
        // valid stand-in for a true subprocess test, without the cost
        // and flakiness of spawning `gluon` itself.
        let tmp = tempfile::tempdir().unwrap();
        let layout = make_layout(tmp.path());
        let _g1 = CacheLock::acquire(&layout).expect("first acquire");

        let lock_path = lockfile_path(&layout);
        let f2 = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .expect("open second handle");
        // The second handle's try-lock must report WouldBlock.
        match f2.try_lock_exclusive() {
            Ok(()) => panic!("expected WouldBlock, but second try_lock succeeded"),
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::WouldBlock),
        }
    }
}
