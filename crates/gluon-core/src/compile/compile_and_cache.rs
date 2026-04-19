//! Shared narrow-lock compile-and-cache wrapper.
//!
//! Three places in gluon's compile pipeline — `sysroot::build_sysroot_crate`,
//! `compile::compile_crate::compile`, and
//! `scheduler::helpers::config_crate::ensure_config_crate` — all drive the
//! same freshness → rustc → depfile → mark_built dance around a single
//! [`RustcCommandBuilder`]. Before this helper landed each call site
//! duplicated ~40 lines of cache-locking, process-spawning, and error
//! rendering. Keeping that logic in three places is a correctness hazard:
//! the narrow-lock rule ("never hold the cache lock across a rustc spawn")
//! is subtle, and any one of the sites drifting from the others would be
//! easy to miss in review.
//!
//! `compile_and_cache` centralises the pattern. Each call site now passes:
//!
//! - the already-assembled [`RustcCommandBuilder`] (consumed into a
//!   [`std::process::Command`] internally),
//! - the expected output path and depfile path,
//! - the cache key and precomputed argv hash,
//! - two small closures that build the per-site error diagnostics (rustc
//!   exit failure headline, depfile-parse failure diagnostic).
//!
//! The helper handles:
//!
//! 1. Seeding `sources_for_query` from the previous run's depfile (empty on
//!    cold builds, which is fine — the cache entry also doesn't exist).
//! 2. Narrow-lock freshness check → drop.
//! 3. Early return on a cache hit (no rustc spawn).
//! 4. Rendering the rustc command to a string **before** consuming the
//!    builder, so the error path can include the full invocation.
//! 5. Spawning rustc and surfacing stderr + command in the diagnostic on
//!    failure.
//! 6. Parsing the depfile **before** updating the cache (ordering matters:
//!    if parsing fails, the rlib is on disk but the cache stays stale, so
//!    the next run re-runs rustc rather than silently using a stale entry).
//! 7. Narrow-lock `mark_built` → drop.
//!
//! ## Out-dir creation
//!
//! Callers that need an output directory created on the slow path pass it
//! via `out_dir_to_create`. `sysroot::build_sysroot` creates its
//! `sysroot_lib_dir` once per target before building individual crates, so
//! it passes `None`; `compile_crate` and `ensure_config_crate` pass `Some(..)`
//! because each crate's out-dir is per-crate and only needed on the slow
//! path.
//!
//! ## Why two closures
//!
//! The error headlines differ between call sites (host vs cross naming,
//! sysroot crate vs user crate vs config crate wording), and the depfile-
//! parse failure diagnostic has site-specific notes. Closures let each site
//! keep its existing error text verbatim — behaviour parity with the
//! pre-extraction code was the explicit goal of C4a.

use crate::cache::{BuildRecord, FreshnessQuery, parse_depfile};
use crate::compile::{CompileCtx, RustcCommandBuilder};
use crate::error::{Error, Result};
use std::path::{Path, PathBuf};
use std::process::Output;

/// Shape of the error diagnostic emitted when the slow-path rustc spawn
/// exits with a non-zero status. Called with the full [`Output`] so the
/// caller can decide how much of stderr/status to surface.
pub(crate) type RustcFailDiag = dyn FnOnce(&Output, &str) -> Error;

/// Shape of the error diagnostic emitted when depfile parsing fails after
/// a successful rustc spawn. The caller receives the underlying I/O error
/// so it can render a note pointing at the on-disk path.
pub(crate) type DepfileFailDiag = dyn FnOnce(&crate::error::Error) -> Error;

/// Run the freshness → rustc → depfile → mark_built sequence for a single
/// rustc invocation. See the module docs for the full contract.
///
/// `extra_sources` is a list of files that must be considered "inputs"
/// of this compile even though rustc's depfile won't list them. The
/// canonical use case is `artifact_env`: a bootloader crate that injects
/// `KERNEL_PATH=<kernel.efi>` needs the kernel binary's mtime to
/// participate in freshness checks, so that rebuilding the kernel
/// invalidates the bootloader's cache. Cross Bin crates have stable
/// output paths (no extra-filename hash), so the rustc argv hash alone
/// would miss a kernel content change. Pass an empty slice for callers
/// with no out-of-band inputs (sysroot, config crate, crates without
/// `artifact_env`).
///
/// Returns `(output_path, was_cached)`:
/// - `was_cached = true` ⇒ the freshness check short-circuited rustc and
///   no new process was spawned.
/// - `was_cached = false` ⇒ rustc ran and the cache was updated.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compile_and_cache(
    ctx: &CompileCtx,
    builder: RustcCommandBuilder,
    argv_hash: [u8; 32],
    cache_key: String,
    output_path: PathBuf,
    depfile_path: PathBuf,
    out_dir_to_create: Option<&Path>,
    extra_sources: Vec<PathBuf>,
    rustc_fail: Box<RustcFailDiag>,
    depfile_fail: Box<DepfileFailDiag>,
    stderr_sink: &mut Vec<u8>,
) -> Result<(PathBuf, bool)> {
    // Seed the source list from the previous run's depfile if it exists.
    // On a cold build this is empty — the cache entry is also missing,
    // so `is_fresh` returns false regardless.
    //
    // `extra_sources` is appended unconditionally — these are always
    // known to the caller and are not tracked by rustc's depfile.
    let sources_for_query: Vec<PathBuf> = {
        let mut v = if depfile_path.exists() {
            parse_depfile(&depfile_path).unwrap_or_default()
        } else {
            Vec::new()
        };
        v.extend(extra_sources.iter().cloned());
        v
    };

    // --- Narrow cache-lock acquisition (read) ---
    let is_fresh = {
        let mut cache = ctx
            .cache
            .lock()
            .map_err(|_| Error::Config("cache mutex poisoned".into()))?;
        cache.is_fresh(&FreshnessQuery {
            key: &cache_key,
            argv_hash,
            sources: &sources_for_query,
            output_path: &output_path,
        })
    };

    if is_fresh && output_path.exists() {
        return Ok((output_path, true));
    }

    // Slow path: optionally create the output directory, then spawn rustc.
    if let Some(dir) = out_dir_to_create {
        std::fs::create_dir_all(dir).map_err(|e| Error::Io {
            path: dir.to_path_buf(),
            source: e,
        })?;
    }

    // Render the argv to a shell-friendly string BEFORE consuming the
    // builder with `into_command()`, so the error path has the full rustc
    // invocation available without needing to reconstruct it. Allocating
    // here is unconditional (the snapshot is cheap), but the string is
    // only formatted on the failure path inside `rustc_fail`.
    let rustc_path = ctx.rustc_info.rustc_path.clone();
    let argv_snapshot: Vec<std::ffi::OsString> = builder.args().to_vec();
    let render_cmd = || -> String {
        let mut s = rustc_path.display().to_string();
        for arg in &argv_snapshot {
            s.push(' ');
            // Debug formatting gives robust cross-platform quoting of
            // OsStr values that may contain spaces or shell metachars.
            s.push_str(&format!("{arg:?}"));
        }
        s
    };

    let mut cmd = builder.into_command();
    let output = cmd.output().map_err(|e| Error::Io {
        path: ctx.rustc_info.rustc_path.clone(),
        source: e,
    })?;
    if !output.status.success() {
        let rendered = render_cmd();
        return Err(rustc_fail(&output, &rendered));
    }
    // Surface rustc's stderr (warnings, lint messages) to the per-job
    // buffer. The worker pool drains this to the user's stderr sink as
    // soon as the job completes, atomically per job — so warnings on
    // parallel compiles never interleave. The failure path above embeds
    // stderr into the diagnostic instead, so we only forward on success.
    stderr_sink.extend_from_slice(&output.stderr);

    // Parse the depfile BEFORE calling `mark_built` so the cache is only
    // updated once we have the source list in hand. If parsing fails,
    // the rlib is on disk but the cache stays stale — the next run will
    // miss the cache and re-spawn rustc. That's merely wasteful, not
    // incorrect.
    //
    // Append `extra_sources` so they participate in future freshness
    // checks exactly as they did in this run's query. Without this,
    // a kernel rebuild would invalidate the bootloader cache on the
    // first invocation (because the query included the kernel) but be
    // silently forgotten on the next (because the stored record didn't).
    let sources = match parse_depfile(&depfile_path) {
        Ok(mut s) => {
            s.extend(extra_sources.iter().cloned());
            s
        }
        Err(e) => return Err(depfile_fail(&e)),
    };

    // --- Narrow cache-lock acquisition (write) ---
    {
        let mut cache = ctx
            .cache
            .lock()
            .map_err(|_| Error::Config("cache mutex poisoned".into()))?;
        cache.mark_built(BuildRecord {
            key: cache_key,
            argv_hash,
            sources,
            output_path: output_path.clone(),
        })?;
    }

    Ok((output_path, false))
}
