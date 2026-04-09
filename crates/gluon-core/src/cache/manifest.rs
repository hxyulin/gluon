//! On-disk cache manifest types and atomic persistence.
//!
//! The manifest is a JSON document describing, for every previously-built
//! cache entry, the rustc argv digest, the set of source files that went
//! into the build (with mtime/size/content-hash fingerprints), the
//! declared output path, and a build timestamp. It is the single source of
//! truth for "have we already built this?" — see [`crate::cache::Cache`]
//! for the freshness algorithm that reads it.
//!
//! ### Corruption policy
//!
//! A corrupted or version-mismatched manifest is *always* treated as a
//! cold cache, never as a build failure. The rationale is the same as for
//! `RustcInfo` in chunk A1: a cache that refuses to load stops a build
//! that was otherwise going to succeed, which is strictly worse than a
//! cache that forces a rebuild. We log a warning to stderr so the user
//! isn't silently paying the re-build cost, but we never return an error.
//!
//! ### Atomic writes
//!
//! [`CacheManifest::save_atomic`] writes to `<path>.tmp.<pid>.<nonce>` +
//! fsync + rename so a crash mid-write can never leave a truncated manifest
//! on disk. The `<pid>.<nonce>` suffix prevents concurrent gluon processes
//! that share the same build directory from clobbering each other's temp
//! files — `<pid>` alone is not collision-free across PID namespaces (two
//! containers can legitimately share a PID), so we append a nanosecond
//! nonce drawn from the wall clock. (Two processes writing the *final*
//! path concurrently is still a race, but a last-writer-wins rename is
//! acceptable: both writers are working from an internally-consistent
//! snapshot.)
//!
//! On any failure during the write/fsync/rename sequence, the temp file
//! is removed before the error is propagated. Without this cleanup, a
//! cache directory would slowly accumulate `.tmp.*` orphans across
//! repeated failed builds.

use crate::error::{Diagnostic, Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

/// Per-source-file fingerprint. The fast path is mtime + size; the
/// content hash is only consulted when the fast path disagrees, but we
/// populate it eagerly on [`crate::cache::Cache::mark_built`] so the
/// fallback actually has something to compare against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFingerprint {
    /// Last-modification time in nanoseconds since `UNIX_EPOCH`.
    pub mtime_ns: u128,
    /// File size in bytes. Included alongside `mtime_ns` because some
    /// filesystems round mtime to the second — two edits within the same
    /// second that change size will still invalidate the cache.
    pub size: u64,
    /// Content hash used when the mtime+size check fails. Populated on the
    /// first build; `Option` rather than a mandatory field so older
    /// manifests that predate eager hashing (and any future lazy-fill
    /// upgrade path) can still deserialise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<[u8; 32]>,
}

/// One cached build. Keyed in the manifest by a caller-chosen string
/// (typically the crate id in chunk A4's sysroot builder).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheEntry {
    /// SHA-256 of the rustc invocation that produced `output_path` — see
    /// [`crate::cache::hash::hash_argv`]. Mismatch ⇒ stale.
    pub argv_hash: [u8; 32],
    /// Source files consumed by the build, with per-file fingerprints.
    /// `BTreeMap` for deterministic iteration / serialisation order.
    pub source_files: BTreeMap<PathBuf, FileFingerprint>,
    /// Mtimes (nanoseconds since `UNIX_EPOCH`) of the direct parent
    /// directories of all source files. A change in directory mtime
    /// indicates that files were added or removed — catching new-file
    /// additions that per-file tracking alone would miss.
    ///
    /// Backwards-compatible: older manifests deserialise with an empty
    /// map via `#[serde(default)]`; newer manifests read by older gluon
    /// versions silently ignore the populated field (the old code had
    /// `let _ = &entry.parent_dir_mtimes`). Both directions degrade to
    /// a more conservative freshness check, never to incorrect caching.
    #[serde(default)]
    pub parent_dir_mtimes: BTreeMap<PathBuf, u128>,
    /// Path to the artefact this entry declares as built. `is_fresh`
    /// requires this file to exist for the entry to be considered fresh.
    pub output_path: PathBuf,
    /// Wall-clock time of the build in nanoseconds since `UNIX_EPOCH`.
    /// Informational only — not used in freshness checks.
    pub built_at: u128,
}

/// Top-level manifest persisted as JSON. Keys are strings so the file is
/// stable across [`BTreeMap`] internal representation changes.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CacheManifest {
    pub version: u32,
    pub entries: BTreeMap<String, CacheEntry>,
}

impl CacheManifest {
    /// The only version gluon currently understands. Bump (and add a
    /// migration path) if the on-disk layout ever changes.
    pub const CURRENT_VERSION: u32 = 1;

    /// Load a manifest from disk.
    ///
    /// Returns `(manifest, warnings)` — the manifest is always usable
    /// (cold-cache fallback on any corruption), and warnings are
    /// [`Diagnostic`]s the caller can surface or suppress. On any of:
    /// file missing, file unreadable, JSON parse failure, or version
    /// mismatch, the returned manifest is a fresh default.
    pub fn load(path: &Path) -> (Self, Vec<Diagnostic>) {
        let mut warnings = Vec::new();

        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return (Self::fresh(), warnings);
            }
            Err(_) => {
                warnings.push(
                    Diagnostic::warning(format!(
                        "cache manifest at {} is unreadable; treating as cold cache",
                        path.display()
                    ))
                    .with_note("Next build will rebuild from scratch. Delete the file to silence this warning."),
                );
                return (Self::fresh(), warnings);
            }
        };
        match serde_json::from_slice::<CacheManifest>(&bytes) {
            Ok(m) if m.version == Self::CURRENT_VERSION => (m, warnings),
            Ok(m) => {
                warnings.push(
                    Diagnostic::warning(format!(
                        "cache manifest at {} is version {}; treating as cold cache",
                        path.display(),
                        m.version
                    ))
                    .with_note("Next build will rebuild from scratch. Delete the file to silence this warning."),
                );
                (Self::fresh(), warnings)
            }
            Err(_) => {
                warnings.push(
                    Diagnostic::warning(format!(
                        "cache manifest at {} is corrupted; treating as cold cache",
                        path.display()
                    ))
                    .with_note("Next build will rebuild from scratch. Delete the file to silence this warning."),
                );
                (Self::fresh(), warnings)
            }
        }
    }

    /// Write the manifest to `path` atomically.
    ///
    /// Writes to `<path>.tmp.<pid>.<nonce>` with pretty-printed JSON,
    /// fsyncs the file, then renames into place. Creates the parent
    /// directory if it doesn't already exist. Always stamps
    /// `version = CURRENT_VERSION` so a stale in-memory value can't be
    /// persisted. On any failure path the temp file is removed before
    /// the error is returned, so failed builds don't accumulate orphans
    /// in the cache directory.
    pub fn save_atomic(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|e| Error::Io {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }

        // Stamp the current version regardless of whatever the caller
        // left in `self.version`. Construct a shallow view rather than
        // mutating `self`: the public API is `&self` and we don't want
        // hidden side-effects.
        let to_write = CacheManifest {
            version: Self::CURRENT_VERSION,
            entries: self.entries.clone(),
        };

        // Build `<path>.tmp.<pid>.<nonce>`. We avoid `with_extension`
        // because it mangles extensionless paths (turning `cache` into
        // `cache.tmp.123.456` is fine, but `with_extension` on a path
        // whose final component is `foo` would replace, not append).
        // `with_file_name` + `format!` is unambiguous.
        let file_name = path.file_name().ok_or_else(|| {
            Error::Config(format!("cache manifest path has no filename: {path:?}"))
        })?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let tmp_path = path.with_file_name(format!(
            "{}.tmp.{}.{}",
            file_name.to_string_lossy(),
            std::process::id(),
            nonce,
        ));

        // Wrap the write/fsync/rename body in an immediately-invoked
        // closure so we have a single point at which we can clean up the
        // temp file before propagating any error. Without this, a
        // failure inside `to_writer_pretty`, `into_inner`, `sync_all`,
        // or `rename` would leave `<path>.tmp.<pid>.<nonce>` on disk and
        // the build directory would accumulate orphans across failed
        // runs.
        let tmp_for_body = tmp_path.clone();
        let result: Result<()> = (|| {
            // Scoped so the BufWriter (and underlying File) is dropped
            // before the rename, which would otherwise leave a dangling
            // handle on Windows.
            {
                let file = File::create(&tmp_for_body).map_err(|e| Error::Io {
                    path: tmp_for_body.clone(),
                    source: e,
                })?;
                let mut writer = BufWriter::new(file);
                serde_json::to_writer_pretty(&mut writer, &to_write).map_err(|e| Error::Io {
                    path: tmp_for_body.clone(),
                    source: std::io::Error::other(e),
                })?;
                let file = writer.into_inner().map_err(|e| Error::Io {
                    path: tmp_for_body.clone(),
                    source: std::io::Error::other(e.to_string()),
                })?;
                file.sync_all().map_err(|e| Error::Io {
                    path: tmp_for_body.clone(),
                    source: e,
                })?;
            }

            fs::rename(&tmp_for_body, path).map_err(|e| Error::Io {
                path: path.to_path_buf(),
                source: e,
            })?;
            Ok(())
        })();

        if result.is_err() {
            // Best-effort cleanup. If the temp file was never created
            // (e.g. File::create failed) this is a no-op; if the rename
            // succeeded the temp file no longer exists and this is also
            // a no-op. We deliberately swallow the remove error: the
            // caller already has a more useful error to propagate.
            let _ = fs::remove_file(&tmp_path);
        }

        result
    }

    fn fresh() -> Self {
        Self {
            version: Self::CURRENT_VERSION,
            entries: BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry() -> CacheEntry {
        let mut files = BTreeMap::new();
        files.insert(
            PathBuf::from("src/a.rs"),
            FileFingerprint {
                mtime_ns: 1_000,
                size: 42,
                sha256: Some([0x11; 32]),
            },
        );
        CacheEntry {
            argv_hash: [0xAB; 32],
            source_files: files,
            parent_dir_mtimes: BTreeMap::new(),
            output_path: PathBuf::from("build/foo.rlib"),
            built_at: 999_999,
        }
    }

    #[test]
    fn round_trip_single_entry() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("cache-manifest.json");

        let mut m = CacheManifest::fresh();
        m.entries.insert("foo".into(), sample_entry());
        m.save_atomic(&p).expect("save");

        let loaded = CacheManifest::load(&p).0;
        assert_eq!(loaded.version, CacheManifest::CURRENT_VERSION);
        assert_eq!(loaded.entries.get("foo"), Some(&sample_entry()));
    }

    #[test]
    fn missing_file_returns_default() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("nope.json");
        let loaded = CacheManifest::load(&p).0;
        assert_eq!(loaded.version, CacheManifest::CURRENT_VERSION);
        assert!(loaded.entries.is_empty());
    }

    #[test]
    fn corrupted_json_returns_default_with_warning() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("bad.json");
        std::fs::write(&p, b"not json at all").expect("seed");
        let (loaded, warnings) = CacheManifest::load(&p);
        assert!(loaded.entries.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].message.contains("corrupted"));
    }

    #[test]
    fn wrong_version_returns_default_with_warning() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("v99.json");
        std::fs::write(&p, br#"{"version": 99, "entries": {}}"#).expect("seed");
        let (loaded, warnings) = CacheManifest::load(&p);
        assert!(loaded.entries.is_empty());
        assert_eq!(loaded.version, CacheManifest::CURRENT_VERSION);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].message.contains("version"));
    }

    #[test]
    fn save_always_stamps_current_version() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("cache.json");

        // Construct with a bogus version; save should overwrite it.
        let m = CacheManifest {
            version: 42,
            entries: BTreeMap::new(),
        };
        m.save_atomic(&p).expect("save");

        let bytes = std::fs::read(&p).expect("read");
        let parsed: CacheManifest = serde_json::from_slice(&bytes).expect("parse");
        assert_eq!(parsed.version, CacheManifest::CURRENT_VERSION);
    }

    #[test]
    fn save_creates_missing_parent_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("nested/dir/cache.json");
        let m = CacheManifest::fresh();
        m.save_atomic(&p).expect("save");
        assert!(p.exists());
    }

    #[test]
    fn save_leaves_no_temp_files_behind() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("cache-manifest.json");
        let m = CacheManifest::fresh();
        m.save_atomic(&p).expect("save");

        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("readdir")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains("cache-manifest.json.tmp")
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "unexpected leftover temp files: {:?}",
            leftovers.iter().map(|e| e.path()).collect::<Vec<_>>()
        );
    }

    #[cfg(unix)]
    #[test]
    fn save_atomic_cleans_up_tmp_file_on_failure() {
        // Force `fs::rename` (or an earlier write step) to fail by making
        // the parent directory read+execute-only, then assert no
        // `.tmp.<pid>.<nonce>` orphan is left behind. We restore the
        // permissions at the end so the tempdir can clean itself up.
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("ro");
        std::fs::create_dir(&dir).expect("mkdir");

        // Create a placeholder so the parent directory exists from
        // save_atomic's point of view (it'll skip create_dir_all because
        // the parent already exists).
        let target = dir.join("cache-manifest.json");

        // Read+execute only — no writes allowed in this directory.
        let original_perms = std::fs::metadata(&dir).expect("meta").permissions();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).expect("chmod ro");

        let m = CacheManifest::fresh();
        let result = m.save_atomic(&target);

        // Restore permissions before any assertion that might panic, so
        // the tempdir cleanup at end-of-scope still succeeds.
        std::fs::set_permissions(&dir, original_perms).expect("restore perms");

        assert!(
            result.is_err(),
            "expected save_atomic to fail in a read-only directory"
        );

        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("readdir")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains("cache-manifest.json.tmp")
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "save_atomic leaked temp files on failure: {:?}",
            leftovers.iter().map(|e| e.path()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn entries_iterate_in_sorted_order() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("cache.json");

        let mut m = CacheManifest::fresh();
        for k in ["z", "a", "m"] {
            m.entries.insert(k.into(), sample_entry());
        }
        m.save_atomic(&p).expect("save");

        let loaded = CacheManifest::load(&p).0;
        let keys: Vec<_> = loaded.entries.keys().cloned().collect();
        assert_eq!(keys, vec!["a".to_string(), "m".into(), "z".into()]);
    }
}
