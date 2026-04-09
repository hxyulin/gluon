//! SHA-256 checksum over a vendored crate directory.
//!
//! The checksum is written into [`super::lockfile::VendorLockPackage::checksum`]
//! so `gluon vendor --check` can detect hand-edits of `vendor/`
//! without re-running `cargo vendor`.
//!
//! # Determinism
//!
//! We walk the directory with `walkdir`, collect every file, sort by
//! relative path, and hash `(relpath, contents)` tuples in order.
//! Sorting makes the hash invariant to filesystem iteration order
//! (which is not guaranteed stable across OSes or filesystems) and
//! the length-prefixed encoding prevents `("foo", "bar")` from
//! colliding with `("fooba", "r")` — the same trick
//! [`crate::cache::hash::hash_argv`] uses.
//!
//! # What's hashed
//!
//! Every regular file under `dir`, recursively. Symlinks are
//! followed (cargo vendor doesn't produce them for crates.io deps;
//! for git deps it strips `.git/` already, so there is nothing
//! intentionally symlinked to worry about). Directories and
//! non-regular entries are ignored. We deliberately include files
//! cargo writes like `.cargo-checksum.json` — they are part of the
//! cargo-managed contract and anyone editing them has broken the
//! vendor directory in a way we want to catch.

use crate::error::{Error, Result};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

const DOMAIN: &str = "gluon.vendor.checksum.v1";
const READ_CHUNK: usize = 64 * 1024;

/// Compute the deterministic checksum of every regular file under
/// `dir`.
///
/// Returns a `"sha256:<64 hex chars>"` string on success.
///
/// Errors on I/O failures (missing directory, unreadable file).
/// Missing-directory is an error rather than "empty checksum"
/// because callers asking for a checksum always expect the
/// directory to exist — if it doesn't, something upstream went
/// wrong and we want to see the original error, not a silent
/// hash-of-nothing.
pub fn checksum_vendored_dir(dir: &Path) -> Result<String> {
    if !dir.exists() {
        return Err(Error::Io {
            path: dir.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "vendor directory does not exist",
            ),
        });
    }
    if !dir.is_dir() {
        return Err(Error::Io {
            path: dir.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "vendor path is not a directory",
            ),
        });
    }

    // Gather (relative_path, absolute_path) for every regular file.
    // `walkdir` yields entries in an unspecified order; we sort
    // before hashing to get a stable digest. We also sort by the
    // *byte* representation of the relative path so case-folding and
    // locale-aware collation cannot introduce platform-specific
    // drift.
    let mut files: Vec<(PathBuf, PathBuf)> = Vec::new();
    for entry in walkdir::WalkDir::new(dir).sort_by_file_name() {
        let entry = entry.map_err(|e| {
            let path = e
                .path()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| dir.to_path_buf());
            Error::Io {
                path,
                source: e
                    .into_io_error()
                    .unwrap_or_else(|| std::io::Error::other("walkdir returned a non-IO error")),
            }
        })?;
        if !entry.file_type().is_file() {
            continue;
        }
        let abs = entry.path().to_path_buf();
        let rel = abs
            .strip_prefix(dir)
            .expect("walkdir entry under root")
            .to_path_buf();
        files.push((rel, abs));
    }
    files.sort_by(|(a, _), (b, _)| {
        // Compare byte-wise. `as_encoded_bytes` on `OsStr` gives us a
        // platform-stable ordering that matches the on-disk layout.
        a.as_os_str()
            .as_encoded_bytes()
            .cmp(b.as_os_str().as_encoded_bytes())
    });

    let mut hasher = Sha256::new();
    hasher.update(DOMAIN.as_bytes());
    hasher.update(b"\0");
    hasher.update((files.len() as u64).to_le_bytes());

    let mut buf = vec![0u8; READ_CHUNK];
    for (rel, abs) in &files {
        let rel_bytes = rel.as_os_str().as_encoded_bytes();
        hasher.update((rel_bytes.len() as u64).to_le_bytes());
        hasher.update(rel_bytes);

        let meta = std::fs::metadata(abs).map_err(|e| Error::Io {
            path: abs.clone(),
            source: e,
        })?;
        hasher.update((meta.len() as u64).to_le_bytes());

        let mut file = File::open(abs).map_err(|e| Error::Io {
            path: abs.clone(),
            source: e,
        })?;
        loop {
            let n = file.read(&mut buf).map_err(|e| Error::Io {
                path: abs.clone(),
                source: e,
            })?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
    }

    let digest: [u8; 32] = hasher.finalize().into();
    let mut hex = String::with_capacity(64);
    for b in digest {
        hex.push_str(&format!("{:02x}", b));
    }
    Ok(format!("sha256:{hex}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(dir: &Path, files: &[(&str, &[u8])]) {
        for (rel, body) in files {
            let p = dir.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, body).unwrap();
        }
    }

    #[test]
    fn identical_trees_hash_identically() {
        let t1 = tempfile::tempdir().unwrap();
        let t2 = tempfile::tempdir().unwrap();
        seed(t1.path(), &[("a.txt", b"alpha"), ("sub/b.txt", b"beta")]);
        seed(t2.path(), &[("a.txt", b"alpha"), ("sub/b.txt", b"beta")]);
        assert_eq!(
            checksum_vendored_dir(t1.path()).unwrap(),
            checksum_vendored_dir(t2.path()).unwrap()
        );
    }

    #[test]
    fn file_content_change_perturbs_hash() {
        let t = tempfile::tempdir().unwrap();
        seed(t.path(), &[("a.txt", b"alpha")]);
        let h1 = checksum_vendored_dir(t.path()).unwrap();
        seed(t.path(), &[("a.txt", b"ALPHA")]);
        let h2 = checksum_vendored_dir(t.path()).unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn adding_a_file_perturbs_hash() {
        let t = tempfile::tempdir().unwrap();
        seed(t.path(), &[("a.txt", b"alpha")]);
        let h1 = checksum_vendored_dir(t.path()).unwrap();
        seed(t.path(), &[("b.txt", b"new")]);
        let h2 = checksum_vendored_dir(t.path()).unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn renaming_a_file_perturbs_hash() {
        let t = tempfile::tempdir().unwrap();
        seed(t.path(), &[("a.txt", b"alpha")]);
        let h1 = checksum_vendored_dir(t.path()).unwrap();
        std::fs::rename(t.path().join("a.txt"), t.path().join("b.txt")).unwrap();
        let h2 = checksum_vendored_dir(t.path()).unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn mtime_alone_does_not_perturb_hash() {
        // Touch file contents to the same bytes but with a new mtime.
        // The checksum only looks at relative path + size + body, so
        // hashes must match.
        let t = tempfile::tempdir().unwrap();
        seed(t.path(), &[("a.txt", b"alpha")]);
        let h1 = checksum_vendored_dir(t.path()).unwrap();
        // Rewrite the same body.
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(t.path().join("a.txt"), b"alpha").unwrap();
        let h2 = checksum_vendored_dir(t.path()).unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn empty_dir_has_stable_hash() {
        let t = tempfile::tempdir().unwrap();
        let h = checksum_vendored_dir(t.path()).unwrap();
        assert!(h.starts_with("sha256:"));
        assert_eq!(h.len(), "sha256:".len() + 64);
    }

    #[test]
    fn missing_dir_errors() {
        let t = tempfile::tempdir().unwrap();
        let missing = t.path().join("does-not-exist");
        match checksum_vendored_dir(&missing) {
            Err(Error::Io { path, .. }) => assert_eq!(path, missing),
            other => panic!("expected Error::Io, got {other:?}"),
        }
    }

    #[test]
    fn non_directory_errors() {
        let t = tempfile::tempdir().unwrap();
        let file = t.path().join("not-a-dir.txt");
        std::fs::write(&file, b"x").unwrap();
        assert!(checksum_vendored_dir(&file).is_err());
    }

    #[test]
    fn length_prefix_prevents_collisions() {
        // The classic length-prefix test: ("foo", "bar") must not
        // hash the same as ("fooba", "r").
        let t1 = tempfile::tempdir().unwrap();
        let t2 = tempfile::tempdir().unwrap();
        seed(t1.path(), &[("foo", b"bar")]);
        seed(t2.path(), &[("fooba", b"r")]);
        assert_ne!(
            checksum_vendored_dir(t1.path()).unwrap(),
            checksum_vendored_dir(t2.path()).unwrap()
        );
    }
}
