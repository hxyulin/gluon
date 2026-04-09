//! Hybrid mtime + content-hash build cache.
//!
//! This module is the load-bearing correctness layer for gluon's
//! incrementality story. Every build artefact that can be skipped must
//! first be proved fresh by [`Cache::is_fresh`]; every artefact that's
//! actually (re)built gets recorded via [`Cache::mark_built`]. The rule
//! that governs both sides is simple: when in doubt, invalidate.
//!
//! ### Freshness algorithm
//!
//! For a given [`FreshnessQuery`] we consider an entry fresh iff:
//!
//! 1. The entry exists in the manifest under `key`.
//! 2. The recorded `argv_hash` matches the query (same rustc flags,
//!    environment, cwd).
//! 3. The declared `output_path` still exists on disk.
//! 4. The recorded source set exactly matches the query's source set —
//!    adding or removing a source file is always a change, even if every
//!    remaining file's hash is unchanged.
//! 5. Every source file passes the two-tier freshness check:
//!    - **Fast path:** recorded `mtime_ns` + `size` match the filesystem.
//!    - **Slow path (fallback):** when the fast path disagrees, compute
//!      the current SHA-256 and compare to the recorded one. If they
//!      match, the mtime is a false positive (e.g. `touch`, CI checkout,
//!      `git restore`); we refresh the mtime in place and keep the cache.
//!      If they differ, the content genuinely changed → stale.
//!
//! The slow path is what makes the cache robust against build systems
//! and CI setups that churn mtimes without changing content. It only
//! triggers when the fast path already disagrees, so the common happy
//! case still runs in O(n) stats with no hashing.
//!
//! ### Eager fingerprint population
//!
//! On [`Cache::mark_built`] we compute and store the SHA-256 for every
//! source file immediately — not lazily on first mtime-mismatch. Lazy
//! population would mean "first touch triggers a rebuild that still
//! doesn't populate the hash, so the second touch also rebuilds": the
//! fallback would never actually kick in. Eager hashing costs one full
//! read per build, which is dwarfed by the compile itself.

pub mod depinfo;
pub mod hash;
pub mod lock;
pub mod manifest;

pub use depinfo::parse_depfile;
pub use hash::{hash_argv, sha256_bytes, sha256_file};
pub use lock::CacheLock;
pub use manifest::{CacheEntry, CacheManifest, FileFingerprint};

use crate::error::{Error, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// In-memory view of a single on-disk manifest plus a dirty bit. Drop or
/// explicitly call [`Cache::save`] to persist changes.
pub struct Cache {
    manifest_path: PathBuf,
    manifest: CacheManifest,
    dirty: bool,
}

/// Input for [`Cache::is_fresh`]. Grouped into a struct because the four
/// fields must stay in sync — passing them positionally invited bugs.
#[derive(Debug)]
pub struct FreshnessQuery<'a> {
    /// Caller-chosen manifest key (typically the crate id in the sysroot
    /// builder from chunk A4).
    pub key: &'a str,
    /// The SHA-256 produced by [`hash_argv`] for the rustc invocation
    /// the caller is about to run.
    pub argv_hash: [u8; 32],
    /// Full source set. Order is irrelevant for freshness (we compare as
    /// a set), but the slice's iteration order is used when we refresh
    /// fingerprints on the slow path.
    pub sources: &'a [PathBuf],
    /// Artefact the caller expects to find; absence ⇒ stale.
    pub output_path: &'a Path,
}

/// Input for [`Cache::mark_built`]. Same rationale as [`FreshnessQuery`].
#[derive(Debug)]
pub struct BuildRecord {
    pub key: String,
    pub argv_hash: [u8; 32],
    pub sources: Vec<PathBuf>,
    pub output_path: PathBuf,
}

/// Read a directory's mtime as nanoseconds since `UNIX_EPOCH`.
/// Returns `None` on any I/O or time-conversion failure.
fn dir_mtime_ns(path: &Path) -> Option<u128> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    Some(mtime.duration_since(UNIX_EPOCH).ok()?.as_nanos())
}

impl Cache {
    /// Load a cache from disk. A missing or corrupted manifest silently
    /// degrades to an empty cache — see [`CacheManifest::load`] for the
    /// policy and rationale. Returns any non-fatal warnings the manifest
    /// loader produced (e.g. version mismatch, corruption).
    pub fn load(manifest_path: impl Into<PathBuf>) -> (Self, Vec<crate::error::Diagnostic>) {
        let manifest_path = manifest_path.into();
        let (manifest, warnings) = CacheManifest::load(&manifest_path);
        (
            Self {
                manifest_path,
                manifest,
                dirty: false,
            },
            warnings,
        )
    }

    /// Persist the manifest via [`CacheManifest::save_atomic`]. No-op
    /// when no change has been made since the last load or save — this
    /// matters for CI runs where the cache is fresh across every crate
    /// and we'd otherwise rewrite the manifest on every invocation.
    pub fn save(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        self.manifest.save_atomic(&self.manifest_path)?;
        self.dirty = false;
        Ok(())
    }

    /// Check whether the cached entry (if any) for `q.key` is still valid
    /// given the current filesystem and the query's expected argv/sources.
    ///
    /// Takes `&mut self` because the slow-path SHA-256 check can refresh
    /// a source's recorded mtime in place, marking the cache dirty.
    pub fn is_fresh(&mut self, q: &FreshnessQuery<'_>) -> bool {
        let Some(entry) = self.manifest.entries.get(q.key) else {
            return false;
        };

        if entry.argv_hash != q.argv_hash {
            return false;
        }
        if !q.output_path.exists() {
            return false;
        }

        // Source set comparison as sets. Adding OR removing a source
        // file must invalidate; hash equality on the intersection isn't
        // enough because new files can change the build output.
        //
        // Canonicalise the query into a BTreeSet first: callers may pass
        // duplicate paths (e.g. a depfile listing the same file twice),
        // and a naive `len()` comparison against `entry.source_files`
        // would silently admit a smaller real set as fresh. Comparing
        // sets makes the check insensitive to query-side duplication.
        let query_set: BTreeSet<&Path> = q.sources.iter().map(|p| p.as_path()).collect();
        if query_set.len() != entry.source_files.len() {
            return false;
        }
        for src in &query_set {
            if !entry.source_files.contains_key(*src) {
                return false;
            }
        }

        // Per-file freshness with slow-path fallback. Accumulate mtime
        // refreshes into a side-buffer so we don't mutate `self` while
        // borrowing the entry immutably. Iterating `query_set` (not
        // `q.sources`) avoids stat'ing/hashing the same file twice when
        // the caller passed duplicates.
        let mut refreshes: Vec<(PathBuf, u128, u64)> = Vec::new();
        for &src in &query_set {
            let meta = match std::fs::metadata(src) {
                Ok(m) => m,
                Err(_) => return false,
            };
            let Some(fp) = entry.source_files.get(src) else {
                // Defensive: covered by the set check above, but guards
                // against future refactors that might drop that check.
                return false;
            };
            let cur_size = meta.len();
            let cur_mtime = match meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            {
                Some(d) => d.as_nanos(),
                None => return false,
            };
            if cur_mtime == fp.mtime_ns && cur_size == fp.size {
                continue;
            }

            // Fast path disagreed — fall back to content hash. If we
            // don't have a stored hash we can't prove the content is
            // unchanged, so we must treat it as stale. In practice
            // `mark_built` always populates the hash, so this branch is
            // only hit by manifests from older gluon versions.
            let Some(stored_hash) = fp.sha256 else {
                return false;
            };
            let cur_hash = match sha256_file(src) {
                Ok(h) => h,
                Err(_) => return false,
            };
            if cur_hash != stored_hash {
                return false;
            }
            refreshes.push((src.to_path_buf(), cur_mtime, cur_size));
        }

        // Verify parent-directory mtimes. A directory's mtime changes
        // when files are added or removed — this catches new source
        // files that wouldn't appear in the per-file check above.
        for (dir, &recorded_mtime) in &entry.parent_dir_mtimes {
            let current_mtime = match dir_mtime_ns(dir) {
                Some(m) => m,
                None => return false,
            };
            if current_mtime != recorded_mtime {
                return false;
            }
        }

        if !refreshes.is_empty()
            && let Some(entry_mut) = self.manifest.entries.get_mut(q.key)
        {
            for (src, mtime_ns, size) in refreshes {
                if let Some(fp) = entry_mut.source_files.get_mut(&src) {
                    fp.mtime_ns = mtime_ns;
                    fp.size = size;
                }
            }
            self.dirty = true;
        }

        true
    }

    /// Record a completed build. Eagerly computes the SHA-256 of every
    /// source file — see the module-level "eager fingerprint population"
    /// note for the reasoning. Returns [`Error::Io`] if any source file
    /// is unreadable; callers should only invoke this after a successful
    /// rustc run, when every source is guaranteed to exist.
    ///
    /// On error, the entry under `rec.key` is left unchanged —
    /// `mark_built` is all-or-nothing from the cache's perspective. The
    /// fingerprint map is built up locally and only inserted into the
    /// manifest after every source has been successfully fingerprinted,
    /// so a mid-loop failure can never leave a half-populated entry
    /// behind. Callers should only invoke this after a successful rustc
    /// run, when every source path is guaranteed to exist.
    pub fn mark_built(&mut self, rec: BuildRecord) -> Result<()> {
        let mut source_files: BTreeMap<PathBuf, FileFingerprint> = BTreeMap::new();
        for src in &rec.sources {
            let meta = std::fs::metadata(src).map_err(|e| Error::Io {
                path: src.clone(),
                source: e,
            })?;
            let mtime = meta.modified().map_err(|e| Error::Io {
                path: src.clone(),
                source: e,
            })?;
            let mtime_ns = mtime
                .duration_since(UNIX_EPOCH)
                .map_err(|e| {
                    Error::Config(format!(
                        "mtime of {} predates UNIX_EPOCH: {}",
                        src.display(),
                        e
                    ))
                })?
                .as_nanos();
            let sha = sha256_file(src)?;
            source_files.insert(
                src.clone(),
                FileFingerprint {
                    mtime_ns,
                    size: meta.len(),
                    sha256: Some(sha),
                },
            );
        }

        // Collect mtimes for direct parent directories of all source
        // files. Directory mtime changes when files are added or removed,
        // so this catches new-file additions that per-file tracking misses.
        let mut parent_dir_mtimes: BTreeMap<PathBuf, u128> = BTreeMap::new();
        for src in &rec.sources {
            if let Some(parent) = src.parent() {
                if !parent_dir_mtimes.contains_key(parent) {
                    if let Some(mtime) = dir_mtime_ns(parent) {
                        parent_dir_mtimes.insert(parent.to_path_buf(), mtime);
                    }
                }
            }
        }

        let built_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);

        let entry = CacheEntry {
            argv_hash: rec.argv_hash,
            source_files,
            parent_dir_mtimes,
            output_path: rec.output_path,
            built_at,
        };
        self.manifest.entries.insert(rec.key, entry);
        self.dirty = true;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn entry(&self, key: &str) -> Option<&CacheEntry> {
        self.manifest.entries.get(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::Write;
    use std::time::Duration;

    struct Fixture {
        _tmp: tempfile::TempDir,
        /// Directory for source files. Separate from `build_dir` so
        /// manifest writes don't change the source directory's mtime.
        src_dir: PathBuf,
        /// Directory for build outputs and the cache manifest.
        build_dir: PathBuf,
        manifest_path: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().expect("tempdir");
            let src_dir = tmp.path().join("src");
            let build_dir = tmp.path().join("build");
            fs::create_dir_all(&src_dir).expect("mkdir src");
            fs::create_dir_all(&build_dir).expect("mkdir build");
            let manifest_path = build_dir.join("cache-manifest.json");
            Self {
                _tmp: tmp,
                src_dir,
                build_dir,
                manifest_path,
            }
        }

        fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let p = self.src_dir.join(name);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).expect("mkdir parent");
            }
            fs::write(&p, bytes).expect("write source");
            p
        }

        fn touch_output(&self, name: &str) -> PathBuf {
            let p = self.build_dir.join(name);
            File::create(&p).expect("create output");
            p
        }
    }

    fn basic_query<'a>(
        key: &'a str,
        argv_hash: [u8; 32],
        sources: &'a [PathBuf],
        output: &'a Path,
    ) -> FreshnessQuery<'a> {
        FreshnessQuery {
            key,
            argv_hash,
            sources,
            output_path: output,
        }
    }

    fn record(
        key: &str,
        argv_hash: [u8; 32],
        sources: Vec<PathBuf>,
        output: PathBuf,
    ) -> BuildRecord {
        BuildRecord {
            key: key.into(),
            argv_hash,
            sources,
            output_path: output,
        }
    }

    #[test]
    fn cold_miss_then_fresh_after_mark_built() {
        let fx = Fixture::new();
        let a = fx.write("a.rs", b"fn main() {}");
        let out = fx.touch_output("a.rlib");
        let argv = [0x11u8; 32];

        let mut cache = Cache::load(&fx.manifest_path).0;
        let q_sources = vec![a.clone()];
        assert!(!cache.is_fresh(&basic_query("k", argv, &q_sources, &out)));

        cache
            .mark_built(record("k", argv, vec![a.clone()], out.clone()))
            .expect("mark");
        cache.save().expect("save");

        let mut cache2 = Cache::load(&fx.manifest_path).0;
        assert!(cache2.is_fresh(&basic_query("k", argv, &q_sources, &out)));
    }

    #[test]
    fn argv_hash_change_invalidates() {
        let fx = Fixture::new();
        let a = fx.write("a.rs", b"x");
        let out = fx.touch_output("a.rlib");

        let mut cache = Cache::load(&fx.manifest_path).0;
        cache
            .mark_built(record("k", [1u8; 32], vec![a.clone()], out.clone()))
            .expect("mark");

        let sources = vec![a];
        assert!(!cache.is_fresh(&basic_query("k", [2u8; 32], &sources, &out)));
    }

    #[test]
    fn missing_output_invalidates() {
        let fx = Fixture::new();
        let a = fx.write("a.rs", b"x");
        let out = fx.touch_output("a.rlib");
        let argv = [3u8; 32];

        let mut cache = Cache::load(&fx.manifest_path).0;
        cache
            .mark_built(record("k", argv, vec![a.clone()], out.clone()))
            .expect("mark");

        fs::remove_file(&out).expect("rm output");
        let sources = vec![a];
        assert!(!cache.is_fresh(&basic_query("k", argv, &sources, &out)));
    }

    #[test]
    fn added_source_invalidates() {
        let fx = Fixture::new();
        let a = fx.write("a.rs", b"x");
        let b = fx.write("b.rs", b"y");
        let out = fx.touch_output("a.rlib");
        let argv = [4u8; 32];

        let mut cache = Cache::load(&fx.manifest_path).0;
        cache
            .mark_built(record("k", argv, vec![a.clone(), b.clone()], out.clone()))
            .expect("mark");

        let c = fx.write("c.rs", b"z");
        let sources = vec![a, b, c];
        assert!(!cache.is_fresh(&basic_query("k", argv, &sources, &out)));
    }

    #[test]
    fn removed_source_invalidates() {
        let fx = Fixture::new();
        let a = fx.write("a.rs", b"x");
        let b = fx.write("b.rs", b"y");
        let out = fx.touch_output("a.rlib");
        let argv = [5u8; 32];

        let mut cache = Cache::load(&fx.manifest_path).0;
        cache
            .mark_built(record("k", argv, vec![a.clone(), b], out.clone()))
            .expect("mark");

        let sources = vec![a];
        assert!(!cache.is_fresh(&basic_query("k", argv, &sources, &out)));
    }

    #[test]
    fn mtime_changed_same_content_stays_fresh() {
        let fx = Fixture::new();
        let a = fx.write("a.rs", b"same content");
        let out = fx.touch_output("a.rlib");
        let argv = [6u8; 32];

        let mut cache = Cache::load(&fx.manifest_path).0;
        cache
            .mark_built(record("k", argv, vec![a.clone()], out.clone()))
            .expect("mark");

        // Bump mtime to "now + 10s" via std::fs::File::set_modified
        // (Rust 1.75+). Content is untouched, so the slow-path SHA
        // fallback should confirm freshness.
        let new_mtime = SystemTime::now() + Duration::from_secs(10);
        let f = File::options().write(true).open(&a).expect("reopen");
        f.set_modified(new_mtime).expect("set mtime");
        drop(f);

        let sources = vec![a.clone()];
        assert!(cache.is_fresh(&basic_query("k", argv, &sources, &out)));
        // The cache should have recorded the new mtime in place.
        let stored = cache.entry("k").expect("entry").source_files[&a].mtime_ns;
        let expected = new_mtime
            .duration_since(UNIX_EPOCH)
            .expect("since epoch")
            .as_nanos();
        assert_eq!(stored, expected);
    }

    #[test]
    fn content_changed_invalidates() {
        let fx = Fixture::new();
        let a = fx.write("a.rs", b"original");
        let out = fx.touch_output("a.rlib");
        let argv = [7u8; 32];

        let mut cache = Cache::load(&fx.manifest_path).0;
        cache
            .mark_built(record("k", argv, vec![a.clone()], out.clone()))
            .expect("mark");

        // Overwrite with different bytes. We also bump the mtime so the
        // fast path definitely disagrees and we exercise the slow path
        // detecting a real content change.
        let mut f = File::create(&a).expect("reopen");
        f.write_all(b"different").expect("rewrite");
        f.sync_all().expect("sync");
        drop(f);

        let sources = vec![a];
        assert!(!cache.is_fresh(&basic_query("k", argv, &sources, &out)));
    }

    #[test]
    fn persistence_across_reload() {
        let fx = Fixture::new();
        let a = fx.write("a.rs", b"hello");
        let out = fx.touch_output("a.rlib");
        let argv = [8u8; 32];

        {
            let mut cache = Cache::load(&fx.manifest_path).0;
            cache
                .mark_built(record("k", argv, vec![a.clone()], out.clone()))
                .expect("mark");
            cache.save().expect("save");
        }

        let mut cache = Cache::load(&fx.manifest_path).0;
        let sources = vec![a];
        assert!(cache.is_fresh(&basic_query("k", argv, &sources, &out)));
    }

    #[test]
    fn save_is_noop_when_not_dirty() {
        let fx = Fixture::new();
        let a = fx.write("a.rs", b"hello");
        let out = fx.touch_output("a.rlib");
        let argv = [9u8; 32];

        let mut cache = Cache::load(&fx.manifest_path).0;
        cache
            .mark_built(record("k", argv, vec![a], out))
            .expect("mark");
        cache.save().expect("save");

        let mtime_before = fs::metadata(&fx.manifest_path)
            .expect("meta")
            .modified()
            .expect("mtime");

        // Second save with no intervening change: the dirty bit is
        // cleared, so the file should not be touched.
        cache.save().expect("noop save");
        let mtime_after = fs::metadata(&fx.manifest_path)
            .expect("meta")
            .modified()
            .expect("mtime");
        assert_eq!(mtime_before, mtime_after);
    }

    #[test]
    fn duplicate_sources_do_not_admit_smaller_entry() {
        // Regression: a query whose source list contains duplicates would
        // previously match an entry built from a *strictly larger* set,
        // because both sides had the same `len()` and every duplicate
        // element trivially passed the contains_key check.
        let fx = Fixture::new();
        let a = fx.write("a.rs", b"alpha");
        let b = fx.write("b.rs", b"beta");
        let out = fx.touch_output("ab.rlib");
        let argv = [11u8; 32];

        let mut cache = Cache::load(&fx.manifest_path).0;
        cache
            .mark_built(record("k", argv, vec![a.clone(), b.clone()], out.clone()))
            .expect("mark");

        // Query with two entries — both `a` — so the underlying set is
        // `{a}`, which is strictly smaller than the recorded `{a, b}`.
        let sources = vec![a.clone(), a];
        assert!(!cache.is_fresh(&basic_query("k", argv, &sources, &out)));
    }

    #[test]
    fn is_fresh_ignores_duplicate_query_entries_when_set_matches() {
        // Symmetric case: a query with duplicates whose canonicalised set
        // exactly matches the recorded set should still be considered
        // fresh. This guarantees the dedup logic isn't over-eager.
        let fx = Fixture::new();
        let a = fx.write("a.rs", b"alpha");
        let out = fx.touch_output("a.rlib");
        let argv = [12u8; 32];

        let mut cache = Cache::load(&fx.manifest_path).0;
        cache
            .mark_built(record("k", argv, vec![a.clone()], out.clone()))
            .expect("mark");

        let sources = vec![a.clone(), a];
        assert!(cache.is_fresh(&basic_query("k", argv, &sources, &out)));
    }

    #[test]
    fn save_leaves_no_temp_files_behind() {
        let fx = Fixture::new();
        let a = fx.write("a.rs", b"hello");
        let out = fx.touch_output("a.rlib");
        let argv = [10u8; 32];

        let mut cache = Cache::load(&fx.manifest_path).0;
        cache
            .mark_built(record("k", argv, vec![a], out))
            .expect("mark");
        cache.save().expect("save");

        let leftovers: Vec<_> = fs::read_dir(&fx.build_dir)
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
            "unexpected temp files: {:?}",
            leftovers.iter().map(|e| e.path()).collect::<Vec<_>>()
        );
    }
}
