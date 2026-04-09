//! Host `rustc` metadata probing and caching.
//!
//! Gluon needs a stable view of the host toolchain to drive sysroot builds
//! and to derive cache keys that invalidate when the compiler changes.
//! [`RustcInfo`] captures that view by spawning `rustc` once per run, and
//! [`RustcInfo::load_or_probe`] persists it to a JSON file under the build
//! layout. The cache is keyed on the rustc binary's mtime, so upgrading a
//! toolchain automatically invalidates every downstream artefact that
//! depends on [`RustcInfo::version_hash`].
//!
//! ### Absolute-path resolution
//!
//! `rustc` is usually on `$PATH` (or shadowed by `rustup`), so we can't
//! assume the caller hands us an absolute path. Rather than pulling in a
//! `which` crate for one call site, we bootstrap via `rustc --print
//! sysroot`: the returned sysroot always contains a `bin/rustc`, and that
//! path is what we stat for mtime. If `$RUSTC` is set to an absolute path
//! we honour it verbatim instead.

use crate::compile::layout::BuildLayout;
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

/// Resolve the rustc command string the current process should invoke.
///
/// Returns the value of `$RUSTC` verbatim if set, else the literal
/// `"rustc"`. This is the *raw, pre-canonicalisation* string — both
/// [`RustcInfo::probe`] and [`RustcInfo::load_or_probe`] consult this so
/// that the cache can be invalidated when the user swaps `$RUSTC` even if
/// the on-disk binary's mtime hasn't moved.
fn resolve_rustc_arg() -> OsString {
    std::env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"))
}

/// Cached metadata about the host `rustc` driving the build.
///
/// All fields are derived from a single probe of the compiler; see
/// [`RustcInfo::probe`]. Serialisable so it can be persisted to a build
/// cache and restored on subsequent runs via [`RustcInfo::load_or_probe`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RustcInfo {
    /// Absolute path to the `rustc` binary used for the probe.
    pub rustc_path: PathBuf,
    /// The raw `$RUSTC` value (or `"rustc"` if unset) at probe time,
    /// stored verbatim — *not* canonicalised. This is the cache-key for
    /// "the user changed which compiler they're invoking" detection: if
    /// the current process's `$RUSTC` differs from this, we re-probe even
    /// when `mtime_ns` would otherwise say the cache is fresh. Stored as a
    /// `String` for clean serde round-tripping; `$RUSTC` and `"rustc"` are
    /// both UTF-8 in every realistic environment.
    pub rustc_arg: String,
    /// Full first line of `rustc -vV`, e.g.
    /// `"rustc 1.82.0 (f6e511eec 2024-10-15)"`.
    pub version: String,
    /// Host triple from the `host:` line of `rustc -vV`.
    pub host_triple: String,
    /// Commit hash from the `commit-hash:` line of `rustc -vV`. Absent on
    /// non-nightly or stripped builds.
    pub commit_hash: Option<String>,
    /// Release string from the `release:` line of `rustc -vV`.
    pub release: String,
    /// Output of `rustc --print sysroot`, trimmed.
    pub sysroot: PathBuf,
    /// `<sysroot>/lib/rustlib/src/rust`, `Some` iff the directory exists.
    /// Required for building custom sysroots (`-Zbuild-std`-style flows).
    pub rust_src: Option<PathBuf>,
    /// Nanoseconds since `UNIX_EPOCH` of `rustc_path` at probe time. Used
    /// as the cache-freshness key.
    pub mtime_ns: u128,
}

impl RustcInfo {
    /// Spawn the host `rustc` and collect every field in this struct.
    ///
    /// Uses `$RUSTC` if set, else the literal `"rustc"` on `$PATH`. Fails
    /// with [`Error::Config`] on any spawn or parse error — we want the
    /// diagnostic to point at the toolchain, not be hidden behind an
    /// anonymous I/O error.
    pub fn probe() -> Result<Self> {
        // Resolve via the shared helper so `load_or_probe` can compare
        // its cache against the *same* string this function will use.
        let rustc_arg_os = resolve_rustc_arg();
        // Lossy is fine: $RUSTC and "rustc" are UTF-8 on every realistic
        // host, and `run_rustc` already takes `&str`. If a user ever
        // points $RUSTC at a non-UTF-8 path, the spawn would have to be
        // reworked anyway.
        let rustc_arg = rustc_arg_os.to_string_lossy().into_owned();
        let rustc_cmd = rustc_arg.as_str();

        // 1. `rustc -vV` — parse first-line version + key:value pairs.
        let vv = run_rustc(rustc_cmd, &["-vV"])?;
        let mut lines = vv.lines();
        let version = lines
            .next()
            .ok_or_else(|| Error::Config("empty output from `rustc -vV`".into()))?
            .trim()
            .to_string();

        let mut host_triple: Option<String> = None;
        let mut commit_hash: Option<String> = None;
        let mut release: Option<String> = None;
        for line in lines {
            if let Some(rest) = line.strip_prefix("host:") {
                host_triple = Some(rest.trim().to_string());
            } else if let Some(rest) = line.strip_prefix("commit-hash:") {
                let v = rest.trim();
                // Stripped builds emit `commit-hash: unknown`; treat as absent.
                if !v.is_empty() && v != "unknown" {
                    commit_hash = Some(v.to_string());
                }
            } else if let Some(rest) = line.strip_prefix("release:") {
                release = Some(rest.trim().to_string());
            }
        }

        let host_triple =
            host_triple.ok_or_else(|| Error::Config("`rustc -vV` missing `host:` line".into()))?;
        let release =
            release.ok_or_else(|| Error::Config("`rustc -vV` missing `release:` line".into()))?;

        // 2. `rustc --print sysroot` — trimmed.
        let sysroot_raw = run_rustc(rustc_cmd, &["--print", "sysroot"])?;
        let sysroot = PathBuf::from(sysroot_raw.trim());

        // 3. Derive the absolute rustc path. Honour $RUSTC when it's
        //    already absolute; otherwise use `<sysroot>/bin/rustc`.
        //    TODO(windows): append `.exe` on Windows hosts.
        let rustc_path = {
            let p = PathBuf::from(rustc_cmd);
            if p.is_absolute() {
                p
            } else {
                sysroot.join("bin").join("rustc")
            }
        };

        // 4. Optional rust source tree (present in toolchains that
        //    installed the `rust-src` component).
        let rust_src_candidate = sysroot.join("lib").join("rustlib").join("src").join("rust");
        let rust_src = if rust_src_candidate.exists() {
            Some(rust_src_candidate)
        } else {
            None
        };

        // 5. mtime of rustc_path as cache key.
        let mtime_ns = mtime_ns_of(&rustc_path)?;

        Ok(Self {
            rustc_path,
            rustc_arg,
            version,
            host_triple,
            commit_hash,
            release,
            sysroot,
            rust_src,
            mtime_ns,
        })
    }

    /// Load a cached `RustcInfo` from `layout.rustc_info_cache()`, falling
    /// back to [`RustcInfo::probe`] if the cache is missing or stale.
    ///
    /// Staleness is determined by two checks, in order: (1) the current
    /// process's `$RUSTC` (or `"rustc"` if unset) must match the cached
    /// `rustc_arg`, otherwise the user has switched compilers and the
    /// cached info points at the wrong binary; (2) the current mtime of
    /// the cached `rustc_path` must equal `mtime_ns`. Any I/O or parse
    /// failure while reading the cache is treated as "not cached" — we
    /// reprobe.
    ///
    /// Cache *writes* that fail are swallowed: the probe succeeded and
    /// the caller should be able to proceed, even if we couldn't persist
    /// the result (e.g. a read-only build directory). A stale cache is a
    /// performance issue, never a correctness issue.
    pub fn load_or_probe(layout: &BuildLayout) -> Result<Self> {
        let cache_path = layout.rustc_info_cache();
        let current_arg = resolve_rustc_arg();

        if let Ok(bytes) = std::fs::read(&cache_path)
            && let Ok(cached) = serde_json::from_slice::<RustcInfo>(&bytes)
            // Footgun guard: if the user changed `$RUSTC` between runs,
            // the on-disk binary the cache points at may still have the
            // same mtime, so an mtime-only check would silently keep
            // using stale info for the wrong compiler. Compare the raw
            // pre-canonicalisation arg first.
            && OsString::from(&cached.rustc_arg) == current_arg
            && let Ok(current_mtime) = mtime_ns_of(&cached.rustc_path)
            // Exact-equals on mtime_ns is deliberate: any change (newer OR
            // older, e.g. a toolchain swap) invalidates the cache. We
            // assume the filesystem reports a stable mtime value for a
            // given binary — true for every filesystem gluon is
            // realistically run on.
            && current_mtime == cached.mtime_ns
        {
            return Ok(cached);
        }

        let info = Self::probe()?;

        // Best-effort write. See the doc comment above — a failure here
        // must not fail the overall operation.
        if let Some(parent) = cache_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_vec_pretty(&info) {
            let _ = std::fs::write(&cache_path, json);
        }

        Ok(info)
    }

    /// Deterministic, paths-free 32-byte fingerprint of the toolchain
    /// identity. Used as a cache-key ingredient by downstream compile
    /// steps: two `RustcInfo` values that compile to the same artefacts
    /// must produce equal hashes, and two that might not must differ.
    ///
    /// Inputs are `version`, `commit_hash`, and `host_triple`, with a
    /// domain separator so gluon's hash space doesn't collide with any
    /// other SHA-256 over the same fields.
    pub fn version_hash(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(b"gluon.rustc-info.v1\0");
        hasher.update(self.version.as_bytes());
        hasher.update(b"\0");
        hasher.update(self.commit_hash.as_deref().unwrap_or("").as_bytes());
        hasher.update(b"\0");
        hasher.update(self.host_triple.as_bytes());
        hasher.finalize().into()
    }
}

/// Run `rustc <args>` and return its stdout as a UTF-8 string.
fn run_rustc(rustc: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(rustc).args(args).output().map_err(|e| {
        Error::Config(format!(
            "failed to spawn `{} {}`: {}",
            rustc,
            args.join(" "),
            e
        ))
    })?;
    if !output.status.success() {
        return Err(Error::Config(format!(
            "`{} {}` exited with {}: {}",
            rustc,
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    String::from_utf8(output.stdout).map_err(|e| {
        Error::Config(format!(
            "`{} {}` produced non-UTF-8 output: {}",
            rustc,
            args.join(" "),
            e
        ))
    })
}

/// Read the mtime of `path` as nanoseconds since `UNIX_EPOCH`.
fn mtime_ns_of(path: &Path) -> Result<u128> {
    let meta = std::fs::metadata(path).map_err(|e| Error::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let mtime = meta.modified().map_err(|e| Error::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let dur = mtime.duration_since(UNIX_EPOCH).map_err(|e| {
        Error::Config(format!(
            "mtime of {} predates UNIX_EPOCH: {}",
            path.display(),
            e
        ))
    })?;
    Ok(dur.as_nanos())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RustcInfo {
        RustcInfo {
            rustc_path: PathBuf::from("/opt/rust/bin/rustc"),
            rustc_arg: "rustc".into(),
            version: "rustc 1.82.0 (f6e511eec 2024-10-15)".into(),
            host_triple: "x86_64-unknown-linux-gnu".into(),
            commit_hash: Some("f6e511eec2dc1".into()),
            release: "1.82.0".into(),
            sysroot: PathBuf::from("/opt/rust"),
            rust_src: None,
            mtime_ns: 1_700_000_000_000_000_000,
        }
    }

    #[test]
    fn probe_against_host_rustc() {
        let info = RustcInfo::probe().expect("host rustc must be reachable");
        assert!(
            info.version.starts_with("rustc "),
            "version should start with 'rustc ', got {:?}",
            info.version
        );
        assert!(!info.host_triple.is_empty());
        assert!(info.sysroot.exists(), "sysroot path should exist on disk");
        assert_eq!(info.version_hash().len(), 32);
    }

    #[test]
    fn load_or_probe_round_trip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let layout = BuildLayout::new(tmp.path(), "demo");

        let first = RustcInfo::load_or_probe(&layout).expect("first probe");
        // Cache file should now exist.
        assert!(layout.rustc_info_cache().exists());

        let second = RustcInfo::load_or_probe(&layout).expect("second probe");
        assert_eq!(first, second);
    }

    #[test]
    fn version_hash_deterministic() {
        let a = sample();
        let b = sample();
        assert_eq!(a.version_hash(), b.version_hash());
    }

    #[test]
    fn version_hash_changes_with_version() {
        let a = sample();
        let mut b = sample();
        b.version = "rustc 1.83.0 (aaaaaaaaa 2025-01-01)".into();
        assert_ne!(a.version_hash(), b.version_hash());
    }

    #[test]
    fn version_hash_changes_with_commit_hash() {
        let a = sample();
        let mut b = sample();
        b.commit_hash = Some("deadbeefcafe".into());
        assert_ne!(a.version_hash(), b.version_hash());
    }

    #[test]
    fn version_hash_changes_with_host_triple() {
        let a = sample();
        let mut b = sample();
        b.host_triple = "aarch64-apple-darwin".into();
        assert_ne!(a.version_hash(), b.version_hash());
    }

    #[test]
    fn version_hash_ignores_paths_and_mtime() {
        let a = sample();
        let mut b = sample();
        b.rustc_path = PathBuf::from("/usr/local/bin/rustc");
        b.rustc_arg = "/opt/nightly/bin/rustc".into();
        b.sysroot = PathBuf::from("/usr/local");
        b.rust_src = Some(PathBuf::from("/usr/local/lib/rustlib/src/rust"));
        b.mtime_ns = 42;
        b.release = "different".into(); // release is also ignored by design
        assert_eq!(a.version_hash(), b.version_hash());
    }

    /// A cache file containing garbage bytes must be silently recovered
    /// from: `load_or_probe` should reprobe and overwrite the file with
    /// valid JSON, never propagate the parse failure as a build error.
    #[test]
    fn load_or_probe_recovers_from_corrupted_cache() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let layout = BuildLayout::new(tmp.path(), "demo");
        let cache_path = layout.rustc_info_cache();

        if let Some(parent) = cache_path.parent() {
            std::fs::create_dir_all(parent).expect("create cache parent");
        }
        std::fs::write(&cache_path, b"not json at all").expect("seed garbage");

        let info = RustcInfo::load_or_probe(&layout).expect("recover from corruption");
        assert!(info.version.starts_with("rustc "));

        // The cache must now contain valid JSON we can re-parse.
        let bytes = std::fs::read(&cache_path).expect("re-read cache");
        let parsed: RustcInfo =
            serde_json::from_slice(&bytes).expect("rewritten cache is valid JSON");
        assert_eq!(parsed, info);
    }

    /// Mutating `mtime_ns` in the on-disk cache must trigger a re-probe;
    /// the returned value should reflect the *real* binary's mtime, not
    /// the bumped value we wrote.
    #[test]
    fn load_or_probe_reprobes_on_stale_mtime() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let layout = BuildLayout::new(tmp.path(), "demo");

        let first = RustcInfo::load_or_probe(&layout).expect("populate cache");
        let real_mtime = first.mtime_ns;

        let bytes = std::fs::read(layout.rustc_info_cache()).expect("read cache");
        let mut tampered: RustcInfo = serde_json::from_slice(&bytes).expect("parse cache");
        let bumped = tampered.mtime_ns.wrapping_add(1);
        tampered.mtime_ns = bumped;
        let json = serde_json::to_vec_pretty(&tampered).expect("re-serialise");
        std::fs::write(layout.rustc_info_cache(), json).expect("write tampered cache");

        let second = RustcInfo::load_or_probe(&layout).expect("re-probe after tamper");
        assert_eq!(
            second.mtime_ns, real_mtime,
            "expected re-probe to restore real mtime",
        );
        assert_ne!(
            second.mtime_ns, bumped,
            "expected re-probe, not the tampered value",
        );
    }

    /// If the cache's `rustc_arg` doesn't match what `resolve_rustc_arg()`
    /// returns in the current process, the cache must be invalidated even
    /// when the file mtime would otherwise look fresh. We avoid mutating
    /// `$RUSTC` here (tests run in parallel) by seeding the cache with a
    /// nonsense `rustc_arg` and asserting `load_or_probe` doesn't return
    /// it verbatim.
    #[test]
    fn load_or_probe_invalidates_on_rustc_arg_mismatch() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let layout = BuildLayout::new(tmp.path(), "demo");
        let cache_path = layout.rustc_info_cache();

        // Write a syntactically valid cache that points at a bogus
        // `rustc_arg`. We give it a real-looking `rustc_path` (the host
        // rustc, via a one-shot probe) so the mtime check would *also*
        // succeed if we hadn't fixed the footgun — that's the whole
        // point of this test.
        let real = RustcInfo::probe().expect("host probe");
        let seeded = RustcInfo {
            rustc_arg: "some-nonexistent-path-xyz".into(),
            ..real.clone()
        };
        if let Some(parent) = cache_path.parent() {
            std::fs::create_dir_all(parent).expect("create cache parent");
        }
        std::fs::write(&cache_path, serde_json::to_vec_pretty(&seeded).unwrap())
            .expect("seed cache");

        let loaded = RustcInfo::load_or_probe(&layout).expect("re-probe on arg mismatch");
        assert_ne!(
            loaded.rustc_arg, "some-nonexistent-path-xyz",
            "cache should have been invalidated",
        );
    }
}
