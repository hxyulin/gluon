//! ESP assembly helper.
//!
//! An [`EspDef`] declares a list of `(source_crate, dest_path)` pairs. At
//! build time, after every referenced source crate has compiled, the
//! `EspBuild` scheduler node calls [`ensure_esp`] to copy each source
//! artifact into the configured destination inside the ESP directory.
//!
//! ### Output layout
//!
//! The ESP directory lives at
//! `<build>/cross/<target>/<profile>/esp/<name>/`, where `<target>` is
//! the profile's cross target. Each entry's `dest_path` is joined into
//! that directory, creating intermediate directories as needed. The
//! result is a valid on-disk FAT layout that QEMU's VVFAT driver can
//! mount via `-drive format=raw,file=fat:rw:<dir>`.
//!
//! ### Freshness
//!
//! Each run writes a stamp file at `<esp_dir>/.esp-stamp.json` recording
//! `(dest_path, source_mtime_ns, source_size)` for every entry. The
//! helper is fresh when every entry's current source mtime+size match
//! the stamp AND every dest file exists on disk. Missing entries are
//! re-copied; unchanged entries are left alone (incremental).
//!
//! This is intentionally a sidecar stamp rather than a `BuildRecord`
//! through the main `Cache` — the cache is rustc-argv-hash-keyed, and
//! there is no rustc invocation here.

use crate::compile::CompileCtx;
use crate::error::{Error, Result};
use gluon_model::{BuildModel, EspDef, Handle, ResolvedConfig};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::compile::ArtifactMap;

/// Stamp entry: per-dest-path fingerprint of the last successful copy.
/// `mtime_ns` is populated from `std::fs::Metadata::modified` on the
/// source file; `size` is its byte length. Together they form a poor
/// man's content hash — a false positive (stale copy with matching
/// metadata) is possible but would require the user to bypass gluon's
/// build graph entirely.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct EspStampEntry {
    dest_path: String,
    mtime_ns: i128,
    size: u64,
}

/// Stamp file format: a sorted map keyed by dest_path.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct EspStamp {
    entries: BTreeMap<String, EspStampEntry>,
}

impl EspStamp {
    fn load(path: &Path) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn save(&self, path: &Path) -> Result<()> {
        let parent = path.parent().ok_or_else(|| {
            Error::Compile(format!(
                "esp stamp path '{}' has no parent directory",
                path.display()
            ))
        })?;
        std::fs::create_dir_all(parent).map_err(|e| Error::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| {
            Error::Compile(format!("esp stamp serialize failed: {e}"))
        })?;
        // Atomic write: write to temp + rename, so an interrupted run
        // never leaves a half-written JSON file that would crash the
        // next load.
        let tmp = path.with_extension("json.tmp");
        {
            let mut f = std::fs::File::create(&tmp).map_err(|e| Error::Io {
                path: tmp.clone(),
                source: e,
            })?;
            f.write_all(&bytes).map_err(|e| Error::Io {
                path: tmp.clone(),
                source: e,
            })?;
            f.sync_all().ok();
        }
        std::fs::rename(&tmp, path).map_err(|e| Error::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        Ok(())
    }
}

fn fingerprint_source(path: &Path) -> Result<(i128, u64)> {
    let md = std::fs::metadata(path).map_err(|e| Error::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let size = md.len();
    // mtime can be negative on pre-epoch files; i128 covers both signs
    // and the nanosecond precision of SystemTime without risking overflow.
    let mtime_ns = md
        .modified()
        .ok()
        .map(|t| {
            use std::time::UNIX_EPOCH;
            match t.duration_since(UNIX_EPOCH) {
                Ok(d) => d.as_nanos() as i128,
                Err(e) => -(e.duration().as_nanos() as i128),
            }
        })
        .unwrap_or(0);
    Ok((mtime_ns, size))
}

/// Assemble the ESP directory for one [`EspDef`], returning the
/// directory path and a `was_cached` flag indicating whether the
/// previous run's output was reused as-is.
///
/// Caller invariants:
/// - The `EspBuild` DAG node guarantees every `source_crate_handle` in
///   `esp.entries` has already been compiled, so `artifacts.get(h)`
///   must return `Some` for resolvable entries. A missing entry is
///   a scheduler bug and produces a clear error.
/// - `esp.entries` with `source_crate_handle == None` are silently
///   skipped; intern has already pushed a diagnostic for them.
pub fn ensure_esp(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    esp_handle: Handle<EspDef>,
    artifacts: &ArtifactMap,
    stdout: &mut Vec<u8>,
) -> Result<(PathBuf, bool)> {
    let esp = model.esps.get(esp_handle).ok_or_else(|| {
        Error::Compile(format!(
            "scheduler: ESP handle {:?} not found in model.esps",
            esp_handle
        ))
    })?;

    // The ESP directory lives under the profile's cross target, even
    // though individual source crates may target something different
    // (e.g. bootloader = x86_64-unknown-uefi, kernel = x86_64-unknown-none).
    // Keying by profile.target keeps the output path stable.
    let profile = &resolved.profile;
    let target = model.targets.get(profile.target).ok_or_else(|| {
        Error::Compile(format!(
            "scheduler: profile '{}' has target handle {:?} not in model.targets",
            profile.name, profile.target
        ))
    })?;
    let esp_dir = ctx.layout.esp_dir(target, profile, &esp.name);
    let stamp_path = esp_dir.join(".esp-stamp.json");

    // Collect the resolvable entries (those with source_crate_handle set
    // AND whose handle resolves in the ArtifactMap). Entries with
    // source_crate_handle == None were already reported by intern and
    // are silently skipped. Entries with a handle but no artifact are a
    // scheduler bug — we surface them loudly.
    struct ResolvedEntry<'a> {
        dest_path: &'a str,
        source_path: PathBuf,
        source_name: &'a str,
    }
    let mut entries: Vec<ResolvedEntry<'_>> = Vec::with_capacity(esp.entries.len());
    for entry in &esp.entries {
        let Some(h) = entry.source_crate_handle else {
            continue;
        };
        let art = artifacts.get(h).ok_or_else(|| {
            Error::Compile(format!(
                "scheduler: EspBuild('{}') source crate '{}' has no entry in \
                 ArtifactMap (DAG edge missing or ran out of order?)",
                esp.name, entry.source_crate
            ))
        })?;
        entries.push(ResolvedEntry {
            dest_path: &entry.dest_path,
            source_path: art.to_path_buf(),
            source_name: &entry.source_crate,
        });
    }

    // Compute the fresh stamp from the current source file metadata.
    let mut fresh_stamp = EspStamp::default();
    for e in &entries {
        let (mtime_ns, size) = fingerprint_source(&e.source_path)?;
        fresh_stamp.entries.insert(
            e.dest_path.to_string(),
            EspStampEntry {
                dest_path: e.dest_path.to_string(),
                mtime_ns,
                size,
            },
        );
    }

    // Fast path: every entry's source metadata matches the stamp AND
    // every dest file exists. `dest_exists` is the catch for the case
    // where the user `rm -rf`d the ESP directory but left the stamp.
    let prev_stamp = EspStamp::load(&stamp_path);
    let all_dest_files_exist = entries.iter().all(|e| esp_dir.join(e.dest_path).exists());
    let stamp_matches = prev_stamp.as_ref() == Some(&fresh_stamp);
    if stamp_matches && all_dest_files_exist {
        return Ok((esp_dir, true));
    }

    // Slow path: ensure the ESP root exists, then (incrementally) copy
    // every entry whose stamp differs.
    std::fs::create_dir_all(&esp_dir).map_err(|e| Error::Io {
        path: esp_dir.clone(),
        source: e,
    })?;

    let prev = prev_stamp.unwrap_or_default();
    let mut copied = 0usize;
    for e in &entries {
        let dest_abs = esp_dir.join(e.dest_path);
        let needs_copy = !dest_abs.exists()
            || prev.entries.get(e.dest_path) != fresh_stamp.entries.get(e.dest_path);
        if !needs_copy {
            continue;
        }
        if let Some(parent) = dest_abs.parent() {
            std::fs::create_dir_all(parent).map_err(|err| Error::Io {
                path: parent.to_path_buf(),
                source: err,
            })?;
        }
        std::fs::copy(&e.source_path, &dest_abs).map_err(|err| Error::Io {
            path: e.source_path.clone(),
            source: err,
        })?;
        copied += 1;
        let _ = writeln!(
            stdout,
            "    Copied {} -> esp/{}/{}",
            e.source_name, esp.name, e.dest_path
        );
    }

    // Persist the stamp *after* every copy succeeds so an interrupted
    // run doesn't leave us believing a partial ESP is fresh.
    fresh_stamp.save(&stamp_path)?;

    // If nothing changed (e.g. only the stamp was missing), report this
    // as a cache hit so the summary counters stay accurate.
    Ok((esp_dir, copied == 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Cache;
    use crate::compile::{BuildLayout, RustcInfo};
    use gluon_model::{
        BuildModel, CrateDef, EspEntry, ProjectDef, ResolvedConfig, ResolvedProfile, TargetDef,
    };
    use std::sync::Arc;

    fn fake_rustc_info() -> RustcInfo {
        RustcInfo {
            rustc_path: PathBuf::from("/usr/bin/rustc"),
            rustc_arg: "rustc".into(),
            version: "rustc 0.0.0 (test)".into(),
            host_triple: "x86_64-unknown-linux-gnu".into(),
            commit_hash: None,
            release: "0.0.0".into(),
            sysroot: PathBuf::from("/fake-sysroot"),
            rust_src: None,
            mtime_ns: 0,
        }
    }

    fn make_ctx(tmp: &Path) -> CompileCtx {
        let layout = BuildLayout::new(tmp.join("build"), "t");
        std::fs::create_dir_all(tmp.join("build")).unwrap();
        let cache = Cache::load(tmp.join("build/cache-manifest.json")).0;
        CompileCtx::new(layout, Arc::new(fake_rustc_info()), cache)
    }

    fn make_model_with_esp(
        tmp: &Path,
        source_artifact: &Path,
    ) -> (BuildModel, ResolvedConfig, Handle<EspDef>) {
        let mut model = BuildModel::default();
        let (th, _) = model.targets.insert(
            "x86_64-uefi".into(),
            TargetDef {
                name: "x86_64-uefi".into(),
                spec: "x86_64-unknown-uefi".into(),
                builtin: true,
                panic_strategy: None,
                span: None,
            },
        );
        let (ch, _) = model.crates.insert(
            "bootloader".into(),
            CrateDef {
                name: "bootloader".into(),
                path: "crates/bootloader".into(),
                edition: "2021".into(),
                crate_type: gluon_model::CrateType::Bin,
                target: "cross".into(),
                target_handle: Some(th),
                group: "uefi".into(),
                ..Default::default()
            },
        );
        let (eh, _) = model.esps.insert(
            "default".into(),
            EspDef {
                name: "default".into(),
                entries: vec![EspEntry {
                    source_crate: "bootloader".into(),
                    source_crate_handle: Some(ch),
                    dest_path: "EFI/BOOT/BOOTX64.EFI".into(),
                }],
                span: None,
            },
        );

        let _ = source_artifact;

        let resolved = ResolvedConfig {
            project: ProjectDef {
                name: "t".into(),
                version: "0.1.0".into(),
                ..Default::default()
            },
            profile: ResolvedProfile {
                name: "dev".into(),
                target: th,
                opt_level: 0,
                debug_info: false,
                lto: None,
                boot_binary: None,
                qemu_memory: None,
                qemu_cores: None,
                qemu_extra_args: Vec::new(),
                test_timeout: None,
            },
            options: BTreeMap::new(),
            crates: Vec::new(),
            build_dir: tmp.join("build"),
            project_root: tmp.to_path_buf(),
        };

        (model, resolved, eh)
    }

    #[test]
    fn ensure_esp_cold_run_copies_source_artifact_to_dest() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path());

        // Create a source "bootloader binary" on disk.
        let source_dir = tmp.path().join("src-artifacts");
        std::fs::create_dir_all(&source_dir).unwrap();
        let source_path = source_dir.join("bootloader.efi");
        std::fs::write(&source_path, b"EFI\x00bootloader bytes").unwrap();

        let (model, resolved, eh) = make_model_with_esp(tmp.path(), &source_path);

        // Populate ArtifactMap: resolve bootloader's handle to our fake
        // source path.
        let mut artifacts = ArtifactMap::new();
        let bl_h = model.crates.lookup("bootloader").unwrap();
        artifacts.insert(bl_h, source_path.clone());

        let mut out = Vec::<u8>::new();
        let (esp_dir, was_cached) =
            ensure_esp(&ctx, &model, &resolved, eh, &artifacts, &mut out).unwrap();

        assert!(!was_cached, "cold run must not report cached");
        let dest = esp_dir.join("EFI/BOOT/BOOTX64.EFI");
        assert!(dest.exists(), "dest file must exist: {}", dest.display());
        let dest_bytes = std::fs::read(&dest).unwrap();
        assert_eq!(dest_bytes, b"EFI\x00bootloader bytes");
        assert!(esp_dir.join(".esp-stamp.json").exists());
    }

    #[test]
    fn ensure_esp_second_run_is_cached_when_source_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path());
        let source_dir = tmp.path().join("src-artifacts");
        std::fs::create_dir_all(&source_dir).unwrap();
        let source_path = source_dir.join("bootloader.efi");
        std::fs::write(&source_path, b"bytes").unwrap();

        let (model, resolved, eh) = make_model_with_esp(tmp.path(), &source_path);
        let bl_h = model.crates.lookup("bootloader").unwrap();
        let mut artifacts = ArtifactMap::new();
        artifacts.insert(bl_h, source_path.clone());

        let mut out = Vec::<u8>::new();
        // First run: cold
        let (_dir, cached1) =
            ensure_esp(&ctx, &model, &resolved, eh, &artifacts, &mut out).unwrap();
        assert!(!cached1);

        // Second run: source unchanged → cached
        let (_dir, cached2) =
            ensure_esp(&ctx, &model, &resolved, eh, &artifacts, &mut out).unwrap();
        assert!(cached2, "second run with unchanged source must be cached");
    }

    #[test]
    fn ensure_esp_recopies_when_source_content_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path());
        let source_dir = tmp.path().join("src-artifacts");
        std::fs::create_dir_all(&source_dir).unwrap();
        let source_path = source_dir.join("bootloader.efi");
        std::fs::write(&source_path, b"v1").unwrap();

        let (model, resolved, eh) = make_model_with_esp(tmp.path(), &source_path);
        let bl_h = model.crates.lookup("bootloader").unwrap();
        let mut artifacts = ArtifactMap::new();
        artifacts.insert(bl_h, source_path.clone());

        let mut out = Vec::<u8>::new();
        let (esp_dir, _) = ensure_esp(&ctx, &model, &resolved, eh, &artifacts, &mut out).unwrap();

        // Mutate source content AND bump mtime. Filesystems can report
        // the same mtime for two writes within the same second, so we
        // also change the file size — the stamp keys on both.
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(&source_path, b"v2-with-more-bytes").unwrap();

        let (_, cached) = ensure_esp(&ctx, &model, &resolved, eh, &artifacts, &mut out).unwrap();
        assert!(!cached, "changed source must trigger a re-copy");
        let dest_bytes = std::fs::read(esp_dir.join("EFI/BOOT/BOOTX64.EFI")).unwrap();
        assert_eq!(dest_bytes, b"v2-with-more-bytes");
    }

    #[test]
    fn ensure_esp_missing_source_handle_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path());
        let source_path = tmp.path().join("bootloader.efi");
        std::fs::write(&source_path, b"x").unwrap();

        let (model, resolved, eh) = make_model_with_esp(tmp.path(), &source_path);
        // EMPTY artifact map — simulates a scheduler bug where the
        // bootloader node didn't run first.
        let artifacts = ArtifactMap::new();

        let mut out = Vec::<u8>::new();
        let err = ensure_esp(&ctx, &model, &resolved, eh, &artifacts, &mut out).unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("ArtifactMap") || msg.contains("DAG edge"),
            "error must mention the missing-artifact condition: {msg}"
        );
    }
}
