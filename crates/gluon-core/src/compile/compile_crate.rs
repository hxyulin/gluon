//! Per-crate compilation dispatch.
//!
//! [`compile`] is the single entry point for building a user crate (as
//! opposed to a sysroot crate handled by [`crate::sysroot`]). It mirrors
//! the cache integration pattern of `build_sysroot_crate` in
//! `sysroot/mod.rs` — read that function first if you are new to this
//! module.
//!
//! ### Host vs cross dispatch
//!
//! The `host` flag on [`ResolvedCrateRef`] determines which code path runs:
//!
//! - **Host crates** are compiled for the build machine. They get no
//!   `--sysroot`, no `--target`, and their outputs land in
//!   `<build>/host/<crate>/`.
//! - **Cross crates** are compiled for the profile's target. They get
//!   `--sysroot`, `--target`, implicit `--extern core/alloc/compiler_builtins`
//!   from the sysroot, and outputs under
//!   `<build>/cross/<target>/<profile>/<crate>/`.
//!
//! ### Flag assembly ordering
//!
//! The canonical token order (which determines the cache key hash) is:
//! input → crate-name → crate-type → edition → sysroot? → target? →
//! out-dir → emit → implicit-externs? → explicit-externs → opt-level →
//! debug-info → lto? → cfg-flags → panic-strategy? → extra-rustc-flags →
//! incremental → linker-script? → extra-filename.
//!
//! Keeping this order stable is non-negotiable: any reordering of the
//! flags invalidates every on-disk cache entry.

use crate::cache::{BuildRecord, FreshnessQuery, parse_depfile};
use crate::compile::{ArtifactMap, CompileCtx, Emit, RustcCommandBuilder};
use crate::error::{Diagnostic, Error, Result};
use gluon_model::{BuildModel, CrateDef, CrateType, ResolvedConfig, ResolvedCrateRef};
use std::fs;
use std::path::{Path, PathBuf};

/// Input bundle for [`compile`]. Grouped into a struct so the function
/// signature stays legible as the number of inputs grows.
pub struct CompileCrateInput<'a> {
    /// The full build model (needed to resolve target/dep handles).
    pub model: &'a BuildModel,
    /// The resolved configuration for this build invocation.
    pub resolved: &'a ResolvedConfig,
    /// The specific crate to compile (with its resolved target binding).
    pub crate_ref: &'a ResolvedCrateRef,
    /// Map of already-built crate artifacts. The caller (scheduler) must
    /// have populated every dependency of `crate_ref` before calling.
    pub artifacts: &'a ArtifactMap,
    /// Path to the pre-built sysroot directory for the active target.
    /// **Required** for cross crates; ignored for host crates.
    pub sysroot_dir: Option<&'a Path>,
}

/// Assemble a [`RustcCommandBuilder`] for the given crate without spawning
/// rustc. Also returns the expected output artifact path and the depfile
/// path.
///
/// Exposed as `pub(crate)` so tests can inspect the assembled flags without
/// needing a real rustc binary. Production code calls [`compile`], which
/// wraps this with cache checking and process spawning.
///
/// Returns `(builder, output_path, depfile_path)`.
pub(crate) fn build_rustc_command(
    ctx: &CompileCtx,
    input: &CompileCrateInput<'_>,
) -> Result<(RustcCommandBuilder, PathBuf, PathBuf)> {
    let layout = &ctx.layout;
    let model = input.model;
    let resolved = input.resolved;
    let crate_ref = input.crate_ref;
    let artifacts = input.artifacts;

    let crate_def: &CrateDef = model.crates.get(crate_ref.handle).ok_or_else(|| {
        Error::Compile(format!(
            "crate handle {:?} not found in build model",
            crate_ref.handle
        ))
    })?;

    let crate_name = &crate_def.name;

    // Resolve the source input file. `CrateDef::path` is a directory path
    // relative to the project root; `CrateDef::root` optionally overrides
    // the entry-point file inside that directory.
    let project_root = &resolved.project_root;
    let crate_dir = project_root.join(&crate_def.path);
    let src_file = if let Some(root_override) = &crate_def.root {
        crate_dir.join(root_override)
    } else {
        match crate_def.crate_type {
            CrateType::Bin => crate_dir.join("src/main.rs"),
            _ => crate_dir.join("src/lib.rs"),
        }
    };

    // Determine the output directory and artifact path. We use
    // `-C extra-filename=-gluon-<name>` so rustc writes a deterministic
    // name instead of the default hash-suffixed form — this lets us know
    // the output path without globbing post-spawn.
    //
    // Exception: cross Bin crates do NOT use extra-filename. Rustc never
    // appends a hash suffix to binary outputs (unlike rlibs), and adding
    // extra-filename to a binary produces an ugly `hello-gluon-hello` name
    // in the final output directory. The binary is placed directly into
    // `cross_final_dir` via `--out-dir`; its canonical name is just
    // `<crate_name>` with no suffix. Dep-info for bins is therefore also
    // suffix-free: `<crate_name>.d` in `cross_final_dir`.
    let extra_filename = format!("-gluon-{crate_name}");

    let (out_dir, output_path, depfile_path) = if crate_ref.host {
        // Host crates: output under build/host/<crate>/.
        let out_dir = layout.host_artifact_dir(crate_def);
        // Host proc-macros and libs both produce an rlib or .so/.dylib;
        // for cache purposes we track the rlib (lib crates always produce
        // one; proc-macros produce a dylib but also an .rmeta; the
        // out-dir is what matters for --extern resolution).
        let artifact = match crate_def.crate_type {
            CrateType::Bin => {
                // Bins on host are unusual but supported; name has no lib prefix.
                out_dir.join(format!("{crate_name}{extra_filename}"))
            }
            CrateType::ProcMacro => {
                // Proc-macros produce a .so/.dylib. rustc determines the
                // exact extension; for the cache output_path we use the
                // platform DLL extension so the existence check is accurate.
                let ext = std::env::consts::DLL_EXTENSION;
                out_dir.join(format!(
                    "{}{}{}{}",
                    std::env::consts::DLL_PREFIX,
                    crate_name,
                    extra_filename,
                    if ext.is_empty() {
                        String::new()
                    } else {
                        format!(".{ext}")
                    }
                ))
            }
            _ => out_dir.join(format!("lib{crate_name}{extra_filename}.rlib")),
        };
        let depfile = out_dir.join(format!("{crate_name}{extra_filename}.d"));
        (out_dir, artifact, depfile)
    } else {
        // Cross crates: dispatch on crate type for output location.
        let target_handle = crate_ref.target;
        let target = model.targets.get(target_handle).ok_or_else(|| {
            Error::Compile(format!(
                "crate '{}': target handle {:?} not found in build model",
                crate_name, target_handle
            ))
        })?;

        match crate_def.crate_type {
            CrateType::Bin => {
                // Cross binaries land directly in `cross_final_dir`. We set
                // `--out-dir` to `final_dir` (no `-o` override) and do NOT
                // use `-C extra-filename` for Bin crates. Reasons:
                //   1. rustc never hash-suffixes binary names, so no
                //      extra-filename is needed to make the name deterministic.
                //   2. `-C extra-filename` with `--out-dir` would produce
                //      `hello-gluon-hello`, which is ugly for a final binary.
                //   3. Omitting extra-filename keeps the canonical output name
                //      equal to the crate name, matching user expectations.
                // The artifact_dir (cross_artifact_dir) is kept for incremental
                // state; out_dir points to final_dir so the binary lands there.
                let final_dir = layout.cross_final_dir(target, &resolved.profile);
                let artifact = final_dir.join(crate_name.as_str());
                // Depfile: without extra-filename, rustc writes `<name>.d`
                // in the --out-dir (final_dir).
                let depfile = final_dir.join(format!("{crate_name}.d"));
                // Use final_dir as out_dir so the binary is placed there.
                // The incremental dir is set separately via -C incremental=.
                (final_dir, artifact, depfile)
            }
            _ => {
                let out_dir = layout.cross_artifact_dir(target, &resolved.profile, crate_def);
                let artifact = out_dir.join(format!("lib{crate_name}{extra_filename}.rlib"));
                let depfile = out_dir.join(format!("{crate_name}{extra_filename}.d"));
                (out_dir, artifact, depfile)
            }
        }
    };

    // --- Builder assembly ---
    let mut builder = RustcCommandBuilder::new(&ctx.rustc_info.rustc_path);

    // Input source file — positional arg, must come first.
    builder.input(&src_file);

    // Crate identity flags.
    builder
        .crate_name(crate_name)
        .crate_type(crate_def.crate_type)
        .edition(&crate_def.edition);

    if crate_ref.host {
        // Host crates: no sysroot, no target. rustc uses its built-in sysroot.
        builder.out_dir(&out_dir);
    } else {
        // Cross crates: sysroot and target are required.
        let sysroot = input.sysroot_dir.ok_or_else(|| {
            Error::Compile(format!(
                "crate '{}' is a cross crate but no sysroot_dir was provided",
                crate_name
            ))
        })?;

        let target_handle = crate_ref.target;
        let target = model.targets.get(target_handle).ok_or_else(|| {
            Error::Compile(format!(
                "crate '{}': target handle {:?} not found in build model",
                crate_name, target_handle
            ))
        })?;

        builder
            .sysroot(sysroot)
            .target(&target.spec, target.builtin)
            .out_dir(&out_dir);

        // Inject implicit sysroot crates. These are the crates that rustc
        // normally injects from its own built-in sysroot; since we are
        // using a custom sysroot we must wire them explicitly. The names
        // must match what `build_sysroot_crate` writes (same extra-filename
        // convention: `-gluon-<name>`).
        let sysroot_lib = layout.sysroot_lib_dir(target);
        builder
            .extern_crate("core", &sysroot_lib.join("libcore-gluon-core.rlib"))
            .extern_crate(
                "compiler_builtins",
                &sysroot_lib.join("libcompiler_builtins-gluon-compiler_builtins.rlib"),
            )
            .extern_crate("alloc", &sysroot_lib.join("liballoc-gluon-alloc.rlib"));

        // Inject the generated config crate if it is available. This is
        // populated by chunk B4 after the config crate is compiled.
        if let Some(config_path) = artifacts.config_crate() {
            // The config crate name is `<project>_config` by default, or the
            // override in `ProjectDef::config_crate_name`. We sanitise the
            // project name (lowercase, non-[a-z0-9_] replaced by `_`) and
            // append `_config`.
            let config_extern_name = resolved
                .project
                .config_crate_name
                .as_deref()
                .map(|s| s.to_string())
                .unwrap_or_else(|| sanitise_crate_name(&resolved.project.name) + "_config");
            builder.extern_crate(&config_extern_name, config_path);
        }
    }

    // Emit flags.
    let emit_kinds: &[Emit] = match crate_def.crate_type {
        CrateType::Bin => &[Emit::Link, Emit::DepInfo],
        _ => &[Emit::Link, Emit::Metadata, Emit::DepInfo],
    };
    builder.emit(emit_kinds);

    // Explicit dependencies (BTreeMap order = deterministic argv order).
    for (extern_name, dep_def) in &crate_def.deps {
        let dep_handle = dep_def.crate_handle.ok_or_else(|| {
            Error::Compile(format!(
                "crate '{}' has dep '{}' (crate_name='{}') with no resolved handle; \
                 the intern/resolve pass should have populated crate_handle before compile",
                crate_name, extern_name, dep_def.crate_name,
            ))
        })?;
        let artifact_path = artifacts.get(dep_handle).ok_or_else(|| {
            Error::Compile(format!(
                "crate '{}' depends on '{}' but its artifact is not yet available in \
                 ArtifactMap (scheduler bug — dep must be built before depender)",
                crate_name, extern_name,
            ))
        })?;
        builder.extern_crate(extern_name, artifact_path);
    }

    // Profile-driven code-generation knobs.
    builder
        .opt_level(resolved.profile.opt_level)
        .debug_info(resolved.profile.debug_info);
    if let Some(mode) = &resolved.profile.lto {
        builder.lto(mode);
    }

    // Per-crate cfg flags (Vec order is preserved for determinism; the
    // Rhai evaluator always produces them in declaration order).
    for flag in &crate_def.cfg_flags {
        builder.cfg(flag);
    }

    // Panic strategy (cross crates only) — must be consistent across
    // sysroot + user crates for the same target, otherwise rustc rejects
    // the link with a "different panic strategy" error. Emitted at this
    // position to match the canonical token order documented at the top
    // of this module.
    if !crate_ref.host {
        let target = model.targets.get(crate_ref.target).ok_or_else(|| {
            Error::Compile(format!(
                "crate '{}': target handle {:?} not found in build model",
                crate_name, crate_ref.target
            ))
        })?;
        if let Some(s) = &target.panic_strategy {
            builder.raw_arg("-C").raw_arg(format!("panic={s}"));
        }
    }

    // Extra rustc flags — appended last (before extra-filename) so they
    // cannot accidentally interleave with the structured flags above.
    for flag in &crate_def.rustc_flags {
        builder.raw_arg(flag.as_str());
    }

    // Incremental compilation directory.
    builder.incremental(&layout.incremental_dir(crate_def));

    // Linker script (Bin crates only). Resolved relative to the project root.
    if crate_def.crate_type == CrateType::Bin {
        if let Some(ls) = &crate_def.linker_script {
            builder.linker_script(&project_root.join(ls));
        }
        // Note: we do NOT pass `-o` here for cross Bin crates. We use
        // `--out-dir=cross_final_dir` instead, and omit `-C extra-filename`
        // for Bin crates (see the output-path derivation above for rationale).
        // For host Bin crates (unusual), `-o` is also omitted; they use the
        // host_artifact_dir as --out-dir with extra-filename.
    }

    // Append -C extra-filename LAST (mirrors sysroot convention). This must
    // come after all other flags so the canonical token order (and therefore
    // the cache hash) stays stable.
    //
    // Exception: cross Bin crates skip extra-filename entirely. See the
    // output-path derivation comment above for the full rationale. The `if`
    // here ensures the cache key is still stable — the absence of the flag for
    // Bin crates is part of the canonical token order.
    if crate_def.crate_type != CrateType::Bin || crate_ref.host {
        builder
            .raw_arg("-C")
            .raw_arg(format!("extra-filename={extra_filename}"));
    }

    Ok((builder, output_path, depfile_path))
}

/// Compile a single crate. Returns the absolute path of the produced
/// artifact and a boolean indicating whether the result came from the
/// cache (`true` = freshness check short-circuited rustc). Cached via
/// `ctx.cache` following the narrow-lock pattern established by
/// `sysroot::build_sysroot_crate`.
///
/// ### Cache contract
///
/// - Acquire cache lock → call `is_fresh` → drop immediately.
/// - If fresh and output exists: return early without spawning rustc.
/// - Else: spawn rustc, parse depfile, re-acquire lock → `mark_built` → drop.
///
/// **Never hold the cache lock across a rustc spawn** — doing so would
/// serialise parallel compilation on the cache mutex.
pub fn compile(ctx: &CompileCtx, input: CompileCrateInput<'_>) -> Result<(PathBuf, bool)> {
    let model = input.model;
    let resolved = input.resolved;
    let crate_ref = input.crate_ref;

    let crate_def: &CrateDef = model.crates.get(crate_ref.handle).ok_or_else(|| {
        Error::Compile(format!(
            "crate handle {:?} not found in build model",
            crate_ref.handle
        ))
    })?;

    let crate_name = &crate_def.name;

    let (builder, output_path, depfile_path) = build_rustc_command(ctx, &input)?;

    let argv_hash = builder.hash();

    // Derive the cache key. The key encodes enough context to avoid
    // collisions between crates with the same name in different targets or
    // profiles.
    let cache_key = if crate_ref.host {
        format!("crate:host:{crate_name}")
    } else {
        let target = model.targets.get(crate_ref.target).ok_or_else(|| {
            Error::Compile(format!(
                "crate '{}': target handle {:?} not found in build model",
                crate_name, crate_ref.target
            ))
        })?;
        format!(
            "crate:cross:{}:{}:{}",
            crate_name, resolved.profile.name, target.name
        )
    };

    // Seed the source list from the previous run's depfile if it exists.
    // On a cold build this will be empty (the depfile doesn't exist yet),
    // which is fine — the cache entry doesn't exist either so `is_fresh`
    // returns false regardless.
    let sources_for_query: Vec<PathBuf> = if depfile_path.exists() {
        parse_depfile(&depfile_path).unwrap_or_default()
    } else {
        Vec::new()
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

    // Slow path: create the output directory and spawn rustc.
    //
    // We render the argv to a string BEFORE consuming the builder with
    // `into_command()`, because on the error path we want to include the
    // full rustc invocation in the diagnostic so flag-assembly bugs are
    // easy to debug. The closure defers the allocation so the common
    // success path pays nothing for this.
    let out_dir = if crate_ref.host {
        ctx.layout.host_artifact_dir(crate_def)
    } else if crate_def.crate_type == CrateType::Bin {
        // Cross Bin crates use cross_final_dir as --out-dir (see
        // build_rustc_command comment). Create it here before spawning rustc.
        let target = model.targets.get(crate_ref.target).ok_or_else(|| {
            Error::Compile(format!(
                "crate '{}': target handle not found when creating output directory",
                crate_name
            ))
        })?;
        ctx.layout.cross_final_dir(target, &resolved.profile)
    } else {
        let target = model.targets.get(crate_ref.target).ok_or_else(|| {
            Error::Compile(format!(
                "crate '{}': target handle not found when creating output directory",
                crate_name
            ))
        })?;
        ctx.layout
            .cross_artifact_dir(target, &resolved.profile, crate_def)
    };
    fs::create_dir_all(&out_dir).map_err(|e| Error::Io {
        path: out_dir.clone(),
        source: e,
    })?;

    let rustc_path = ctx.rustc_info.rustc_path.clone();
    let argv_snapshot: Vec<std::ffi::OsString> = builder.args().to_vec();
    let render_cmd = || -> String {
        let mut s = rustc_path.display().to_string();
        for arg in &argv_snapshot {
            s.push(' ');
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
        // For cross crates we name the target in the headline so
        // multi-target builds don't force the user to dig through the
        // command to see which rustc invocation actually failed.
        let headline = if crate_ref.host {
            format!(
                "rustc failed while building host crate '{}': exit={:?}",
                crate_name,
                output.status.code(),
            )
        } else {
            let target_name = model
                .targets
                .get(crate_ref.target)
                .map(|t| t.name.as_str())
                .unwrap_or("<unknown>");
            format!(
                "rustc failed while building crate '{}' for target '{}': exit={:?}",
                crate_name,
                target_name,
                output.status.code(),
            )
        };
        return Err(Error::Diagnostics(vec![
            Diagnostic::error(headline)
                .with_note(format!(
                    "stderr:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                ))
                .with_note(format!("command: {}", render_cmd())),
        ]));
    }

    // Parse the depfile for the source set to record. If parsing fails,
    // the artifact is already on disk but the cache has no entry for it —
    // the next run will miss the cache and re-spawn rustc. That's merely
    // wasteful, not incorrect, so we surface a clear diagnostic rather
    // than silently ignoring the error.
    let sources = parse_depfile(&depfile_path).map_err(|e| {
        Error::Diagnostics(vec![
            Diagnostic::error(format!(
                "built crate '{}' but its depfile at {} could not be read",
                crate_name,
                depfile_path.display(),
            ))
            .with_note(format!("underlying error: {e}"))
            .with_note(
                "the artifact is on disk but the cache was not updated; \
                 the next run will re-spawn rustc for this crate",
            ),
        ])
    })?;

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

/// Sanitise an arbitrary string to a valid Rust identifier component.
///
/// Lowercases every character and replaces any byte that is not `[a-z0-9_]`
/// with `_`. The result is suitable as a crate name or the prefix of one.
///
/// `pub(crate)` so that `scheduler::helpers::config_crate` can reuse this
/// logic without duplication — both places derive config-crate names from
/// the project name using the same rules.
pub(crate) fn sanitise_crate_name(s: &str) -> String {
    s.chars()
        .map(|c| {
            let lc = c.to_ascii_lowercase();
            if lc.is_ascii_alphanumeric() || lc == '_' {
                lc
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Cache;
    use crate::compile::{ArtifactMap, BuildLayout, RustcInfo};
    use gluon_model::{
        BuildModel, CrateDef, CrateType, DepDef, Handle, ProjectDef, ResolvedConfig,
        ResolvedCrateRef, ResolvedProfile, TargetDef,
    };
    use std::path::Path;
    use std::sync::Arc;

    // ---------------------------------------------------------------------------
    // Test fixtures
    // ---------------------------------------------------------------------------

    fn fake_rustc_info(rustc_path: impl Into<PathBuf>) -> RustcInfo {
        RustcInfo {
            rustc_path: rustc_path.into(),
            rustc_arg: "rustc".into(),
            version: "rustc 0.0.0 (test 2020-01-01)".into(),
            host_triple: "x86_64-unknown-linux-gnu".into(),
            commit_hash: Some("deadbeef".into()),
            release: "0.0.0".into(),
            sysroot: PathBuf::from("/fake-sysroot"),
            rust_src: None,
            mtime_ns: 0,
        }
    }

    /// Build a minimal `CompileCtx` backed by a fresh temp-dir cache.
    fn make_ctx(tmp: &Path, rustc_path: impl Into<PathBuf>) -> CompileCtx {
        let layout = BuildLayout::new(tmp.join("build"), "testproject");
        let info = fake_rustc_info(rustc_path);
        // Create the build dir before loading the cache so a later
        // cache.save() can atomically write-rename into it. Cache::load
        // tolerates a missing manifest file, so the order there doesn't
        // matter — but save() does not tolerate a missing parent dir.
        std::fs::create_dir_all(tmp.join("build")).unwrap();
        let cache = Cache::load(tmp.join("build/cache-manifest.json")).expect("load cache");
        CompileCtx::new(layout, Arc::new(info), cache)
    }

    fn make_target(name: &str, spec: &str, builtin: bool) -> TargetDef {
        TargetDef {
            name: name.into(),
            spec: spec.into(),
            builtin,
            panic_strategy: None,
            span: None,
        }
    }

    fn make_target_with_panic(name: &str, spec: &str, strategy: &str) -> TargetDef {
        TargetDef {
            name: name.into(),
            spec: spec.into(),
            builtin: true,
            panic_strategy: Some(strategy.into()),
            span: None,
        }
    }

    fn make_profile(target: Handle<TargetDef>) -> ResolvedProfile {
        ResolvedProfile {
            name: "debug".into(),
            target,
            opt_level: 0,
            debug_info: false,
            lto: None,
            boot_binary: None,
        }
    }

    /// Insert a target into `model.targets` and return its handle.
    fn insert_target(model: &mut BuildModel, t: TargetDef) -> Handle<TargetDef> {
        let name = t.name.clone();
        let (h, _) = model.targets.insert(name, t);
        h
    }

    /// Insert a crate into `model.crates` and return its handle.
    fn insert_crate(model: &mut BuildModel, c: CrateDef) -> Handle<CrateDef> {
        let name = c.name.clone();
        let (h, _) = model.crates.insert(name, c);
        h
    }

    /// Make a minimal `ResolvedConfig` for the given project root.
    fn make_resolved(target_handle: Handle<TargetDef>, project_root: PathBuf) -> ResolvedConfig {
        ResolvedConfig {
            project: ProjectDef {
                name: "testproject".into(),
                version: "0.1.0".into(),
                config_crate_name: None,
                cfg_prefix: None,
                config_override_file: None,
            },
            profile: make_profile(target_handle),
            options: Default::default(),
            crates: Vec::new(),
            build_dir: project_root.join("build"),
            project_root,
        }
    }

    /// Make a minimal host-crate `ResolvedCrateRef`.
    fn host_ref(handle: Handle<CrateDef>, target: Handle<TargetDef>) -> ResolvedCrateRef {
        ResolvedCrateRef {
            handle,
            target,
            host: true,
        }
    }

    /// Make a minimal cross-crate `ResolvedCrateRef`.
    fn cross_ref(handle: Handle<CrateDef>, target: Handle<TargetDef>) -> ResolvedCrateRef {
        ResolvedCrateRef {
            handle,
            target,
            host: false,
        }
    }

    fn args_as_strings(builder: &RustcCommandBuilder) -> Vec<String> {
        builder
            .args()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    // ---------------------------------------------------------------------------
    // Flag assembly tests
    // ---------------------------------------------------------------------------

    #[test]
    fn flag_assembly_host_crate_has_no_sysroot_or_target() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path(), "/usr/bin/rustc");

        let mut model = BuildModel::default();
        let th = insert_target(&mut model, make_target("x86", "x86_64-unknown-none", true));
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "mylib".into(),
                path: "crates/mylib".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                ..Default::default()
            },
        );
        let resolved = make_resolved(th, tmp.path().to_path_buf());
        let crate_ref = host_ref(ch, th);
        let artifacts = ArtifactMap::new();

        let (builder, _, _) = build_rustc_command(
            &ctx,
            &CompileCrateInput {
                model: &model,
                resolved: &resolved,
                crate_ref: &crate_ref,
                artifacts: &artifacts,
                sysroot_dir: None,
            },
        )
        .unwrap();

        let args = args_as_strings(&builder);
        assert!(
            !args.iter().any(|a| a == "--sysroot"),
            "--sysroot must be absent for host crates, got: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a.starts_with("--target")),
            "--target must be absent for host crates, got: {args:?}"
        );
    }

    #[test]
    fn flag_assembly_cross_crate_has_sysroot_and_target() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path(), "/usr/bin/rustc");
        let sysroot_dir = tmp.path().join("sysroot");

        let mut model = BuildModel::default();
        let th = insert_target(&mut model, make_target("x86", "x86_64-unknown-none", true));
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "kernel".into(),
                path: "crates/kernel".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                ..Default::default()
            },
        );
        let resolved = make_resolved(th, tmp.path().to_path_buf());
        let crate_ref = cross_ref(ch, th);
        let artifacts = ArtifactMap::new();

        let (builder, _, _) = build_rustc_command(
            &ctx,
            &CompileCrateInput {
                model: &model,
                resolved: &resolved,
                crate_ref: &crate_ref,
                artifacts: &artifacts,
                sysroot_dir: Some(&sysroot_dir),
            },
        )
        .unwrap();

        let args = args_as_strings(&builder);
        assert!(
            args.iter().any(|a| a == "--sysroot"),
            "--sysroot must be present for cross crates, got: {args:?}"
        );
        assert!(
            args.iter().any(|a| a.starts_with("--target")),
            "--target must be present for cross crates, got: {args:?}"
        );
        // Implicit sysroot crates.
        assert!(
            args.iter().any(|a| a.starts_with("core=")),
            "--extern core= must be present for cross crates, got: {args:?}"
        );
        assert!(
            args.iter().any(|a| a.starts_with("alloc=")),
            "--extern alloc= must be present for cross crates, got: {args:?}"
        );
        assert!(
            args.iter().any(|a| a.starts_with("compiler_builtins=")),
            "--extern compiler_builtins= must be present for cross crates, got: {args:?}"
        );
    }

    #[test]
    fn flag_assembly_proc_macro_emits_proc_macro_crate_type() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path(), "/usr/bin/rustc");

        let mut model = BuildModel::default();
        let th = insert_target(
            &mut model,
            make_target("host", "x86_64-unknown-linux-gnu", true),
        );
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "mymacro".into(),
                path: "crates/mymacro".into(),
                edition: "2021".into(),
                crate_type: CrateType::ProcMacro,
                ..Default::default()
            },
        );
        let resolved = make_resolved(th, tmp.path().to_path_buf());
        let crate_ref = host_ref(ch, th);
        let artifacts = ArtifactMap::new();

        let (builder, _, _) = build_rustc_command(
            &ctx,
            &CompileCrateInput {
                model: &model,
                resolved: &resolved,
                crate_ref: &crate_ref,
                artifacts: &artifacts,
                sysroot_dir: None,
            },
        )
        .unwrap();

        let args = args_as_strings(&builder);
        // --crate-type proc-macro is emitted as two tokens.
        let crate_type_pos = args.iter().position(|a| a == "--crate-type");
        assert!(
            crate_type_pos.is_some(),
            "--crate-type missing, got: {args:?}"
        );
        let next = args.get(crate_type_pos.unwrap() + 1);
        assert_eq!(
            next.map(|s| s.as_str()),
            Some("proc-macro"),
            "--crate-type must be proc-macro, got: {args:?}"
        );
    }

    #[test]
    fn flag_assembly_respects_explicit_deps() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path(), "/usr/bin/rustc");

        let mut model = BuildModel::default();
        let th = insert_target(&mut model, make_target("x86", "x86_64-unknown-none", true));
        // Insert dep crate first so it gets handle 0.
        let dep_handle = insert_crate(
            &mut model,
            CrateDef {
                name: "myutil".into(),
                path: "crates/myutil".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                ..Default::default()
            },
        );

        let mut deps = std::collections::BTreeMap::new();
        deps.insert(
            "myutil".to_string(),
            DepDef {
                crate_name: "myutil".into(),
                crate_handle: Some(dep_handle),
                ..Default::default()
            },
        );
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "kernel".into(),
                path: "crates/kernel".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                deps,
                ..Default::default()
            },
        );

        let resolved = make_resolved(th, tmp.path().to_path_buf());
        let crate_ref = host_ref(ch, th);

        let dep_rlib = PathBuf::from("/build/libmyutil-gluon-myutil.rlib");
        let mut artifacts = ArtifactMap::new();
        artifacts.insert(dep_handle, dep_rlib.clone());

        let (builder, _, _) = build_rustc_command(
            &ctx,
            &CompileCrateInput {
                model: &model,
                resolved: &resolved,
                crate_ref: &crate_ref,
                artifacts: &artifacts,
                sysroot_dir: None,
            },
        )
        .unwrap();

        let args = args_as_strings(&builder);
        let extern_val = args
            .iter()
            .zip(args.iter().skip(1))
            .find(|(a, _)| a.as_str() == "--extern")
            .map(|(_, v)| v.as_str());
        assert_eq!(
            extern_val,
            Some("myutil=/build/libmyutil-gluon-myutil.rlib"),
            "--extern myutil=... must be present, got: {args:?}"
        );
    }

    #[test]
    fn missing_dep_in_artifact_map_returns_compile_error() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path(), "/usr/bin/rustc");

        let mut model = BuildModel::default();
        let th = insert_target(&mut model, make_target("x86", "x86_64-unknown-none", true));
        let dep_handle = insert_crate(
            &mut model,
            CrateDef {
                name: "missing_dep".into(),
                ..Default::default()
            },
        );

        let mut deps = std::collections::BTreeMap::new();
        deps.insert(
            "missing_dep".to_string(),
            DepDef {
                crate_name: "missing_dep".into(),
                crate_handle: Some(dep_handle),
                ..Default::default()
            },
        );
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "kernel".into(),
                path: "crates/kernel".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                deps,
                ..Default::default()
            },
        );

        let resolved = make_resolved(th, tmp.path().to_path_buf());
        let crate_ref = host_ref(ch, th);
        let artifacts = ArtifactMap::new(); // empty — dep not built

        let result = build_rustc_command(
            &ctx,
            &CompileCrateInput {
                model: &model,
                resolved: &resolved,
                crate_ref: &crate_ref,
                artifacts: &artifacts,
                sysroot_dir: None,
            },
        );

        match result {
            Err(Error::Compile(msg)) => {
                assert!(
                    msg.contains("missing_dep"),
                    "error message must name the missing dep, got: {msg}"
                );
            }
            Ok(_) => panic!("expected Err(Error::Compile), got Ok"),
            Err(e) => panic!("expected Err(Error::Compile), got Err({e})"),
        }
    }

    #[test]
    fn flag_assembly_cross_crate_passes_panic_strategy_when_set() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path(), "/usr/bin/rustc");
        let sysroot_dir = tmp.path().join("sysroot");

        let mut model = BuildModel::default();
        let th = insert_target(
            &mut model,
            make_target_with_panic("x86", "x86_64-unknown-none", "abort"),
        );
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "kernel".into(),
                path: "crates/kernel".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                ..Default::default()
            },
        );
        let resolved = make_resolved(th, tmp.path().to_path_buf());
        let crate_ref = cross_ref(ch, th);
        let artifacts = ArtifactMap::new();

        let (builder, _, _) = build_rustc_command(
            &ctx,
            &CompileCrateInput {
                model: &model,
                resolved: &resolved,
                crate_ref: &crate_ref,
                artifacts: &artifacts,
                sysroot_dir: Some(&sysroot_dir),
            },
        )
        .unwrap();

        let args = args_as_strings(&builder);
        // Scan all -C <val> pairs to find panic=abort.
        let found_panic = args
            .windows(2)
            .any(|w| w[0] == "-C" && w[1] == "panic=abort");
        assert!(
            found_panic,
            "-C panic=abort must be present when panic_strategy=Some(\"abort\"), got: {args:?}"
        );
    }

    #[test]
    fn flag_assembly_cross_crate_omits_panic_strategy_when_unset() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path(), "/usr/bin/rustc");
        let sysroot_dir = tmp.path().join("sysroot");

        let mut model = BuildModel::default();
        let th = insert_target(&mut model, make_target("x86", "x86_64-unknown-none", true));
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "kernel".into(),
                path: "crates/kernel".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                ..Default::default()
            },
        );
        let resolved = make_resolved(th, tmp.path().to_path_buf());
        let crate_ref = cross_ref(ch, th);
        let artifacts = ArtifactMap::new();

        let (builder, _, _) = build_rustc_command(
            &ctx,
            &CompileCrateInput {
                model: &model,
                resolved: &resolved,
                crate_ref: &crate_ref,
                artifacts: &artifacts,
                sysroot_dir: Some(&sysroot_dir),
            },
        )
        .unwrap();

        let args = args_as_strings(&builder);
        let found_panic = args
            .windows(2)
            .any(|w| w[0] == "-C" && w[1].starts_with("panic="));
        assert!(
            !found_panic,
            "no panic= flag must be present when panic_strategy=None, got: {args:?}"
        );
    }

    #[test]
    fn flag_assembly_cross_lib_adds_config_crate_extern_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path(), "/usr/bin/rustc");
        let sysroot_dir = tmp.path().join("sysroot");
        let config_rlib = PathBuf::from("/build/libtestproject_config.rlib");

        let mut model = BuildModel::default();
        let th = insert_target(&mut model, make_target("x86", "x86_64-unknown-none", true));
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "kernel".into(),
                path: "crates/kernel".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                ..Default::default()
            },
        );
        let resolved = make_resolved(th, tmp.path().to_path_buf());
        let crate_ref = cross_ref(ch, th);

        let mut artifacts = ArtifactMap::new();
        artifacts.set_config_crate(config_rlib.clone());

        let (builder, _, _) = build_rustc_command(
            &ctx,
            &CompileCrateInput {
                model: &model,
                resolved: &resolved,
                crate_ref: &crate_ref,
                artifacts: &artifacts,
                sysroot_dir: Some(&sysroot_dir),
            },
        )
        .unwrap();

        let args = args_as_strings(&builder);
        // The config crate extern is: --extern testproject_config=<path>
        let found_config = args
            .windows(2)
            .any(|w| w[0] == "--extern" && w[1].starts_with("testproject_config="));
        assert!(
            found_config,
            "--extern testproject_config=... must be present when config_crate is set, got: {args:?}"
        );
    }

    #[test]
    fn cache_hit_skips_rustc_spawn() {
        // Arrange: pre-populate the cache with a matching BuildRecord and
        // write a zero-byte file at the expected output path. Point rustc at
        // a bogus path — any real spawn would return an I/O error, proving
        // the fast path was taken.
        //
        // The cache key and argv_hash are derived from the builder assembled
        // with a known rustc path. We pre-populate the manifest on disk,
        // then construct a fresh CompileCtx (pointing at a BOGUS rustc) that
        // loads the pre-populated manifest. If the cache hit is taken, compile
        // returns Ok without ever spawning the bogus rustc.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("build")).unwrap();

        let mut model = BuildModel::default();
        let th = insert_target(&mut model, make_target("x86", "x86_64-unknown-none", true));
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "cached_crate".into(),
                path: "crates/cached_crate".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                ..Default::default()
            },
        );
        let resolved = make_resolved(th, tmp.path().to_path_buf());
        let crate_ref = host_ref(ch, th);
        let artifacts = ArtifactMap::new();

        // The bogus rustc path that the final ctx will use. We pre-compute
        // the argv_hash using this SAME path so the BuildRecord we write
        // matches what compile() will compute.
        let bogus_rustc = PathBuf::from("/dev/null/bogus-rustc");
        let manifest_path = tmp.path().join("build/cache-manifest.json");

        // Use a temporary ctx (same bogus path) just to compute the hash
        // and output/depfile paths — this ctx is discarded before calling
        // compile().
        let ctx_for_hash = make_ctx(tmp.path(), &bogus_rustc);
        let (builder, output_path, _depfile_path) = build_rustc_command(
            &ctx_for_hash,
            &CompileCrateInput {
                model: &model,
                resolved: &resolved,
                crate_ref: &crate_ref,
                artifacts: &artifacts,
                sysroot_dir: None,
            },
        )
        .unwrap();
        let argv_hash = builder.hash();
        drop(ctx_for_hash); // release the in-memory cache

        // Create the output file so output_path.exists() is true in is_fresh.
        std::fs::create_dir_all(output_path.parent().unwrap()).unwrap();
        std::fs::write(&output_path, b"").unwrap();

        // Pre-populate the manifest on disk with a matching BuildRecord.
        // Sources is empty because no depfile exists → sources_for_query is
        // also empty, so the set-comparison in is_fresh passes trivially.
        let cache_key = "crate:host:cached_crate".to_string();
        {
            let mut cache = Cache::load(&manifest_path).expect("load");
            cache
                .mark_built(BuildRecord {
                    key: cache_key,
                    argv_hash,
                    sources: Vec::new(),
                    output_path: output_path.clone(),
                })
                .expect("mark_built");
            cache.save().expect("save");
        }

        // Now build a FRESH ctx (same bogus rustc path so hash matches the
        // pre-populated record). This ctx loads the manifest we just wrote.
        let ctx = make_ctx(tmp.path(), &bogus_rustc);

        // compile() must hit the cache and return Ok(output_path) without
        // ever attempting to spawn /dev/null/bogus-rustc (which would fail
        // with an I/O error on any OS).
        let result = compile(
            &ctx,
            CompileCrateInput {
                model: &model,
                resolved: &resolved,
                crate_ref: &crate_ref,
                artifacts: &artifacts,
                sysroot_dir: None,
            },
        );
        let (got_path, was_cached) = result.unwrap();
        assert_eq!(got_path, output_path, "cache hit must return output_path");
        assert!(was_cached, "cache hit must report was_cached=true");
    }

    /// Regression guard: cross Bin crates must NOT emit `-C extra-filename` or
    /// `-o`, and the output path must end with just the crate name (no
    /// `-gluon-<name>` suffix). This exercises the special-case code in
    /// `build_rustc_command` that keeps cross binary names clean.
    #[test]
    fn flag_assembly_cross_bin_omits_extra_filename_and_uses_out_dir_in_final() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path(), "/usr/bin/rustc");
        let sysroot_dir = tmp.path().join("sysroot");

        let mut model = BuildModel::default();
        let th = insert_target(&mut model, make_target("x86", "x86_64-unknown-none", true));
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "hello".into(),
                path: "crates/hello".into(),
                edition: "2021".into(),
                crate_type: CrateType::Bin,
                ..Default::default()
            },
        );
        let resolved = make_resolved(th, tmp.path().to_path_buf());
        let crate_ref = cross_ref(ch, th);
        let artifacts = ArtifactMap::new();

        let (builder, output_path, _depfile) = build_rustc_command(
            &ctx,
            &CompileCrateInput {
                model: &model,
                resolved: &resolved,
                crate_ref: &crate_ref,
                artifacts: &artifacts,
                sysroot_dir: Some(&sysroot_dir),
            },
        )
        .unwrap();

        let args: Vec<String> = builder
            .args()
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert!(
            !args
                .windows(2)
                .any(|w| w[0] == "-C" && w[1].contains("extra-filename")),
            "cross Bin must not emit -C extra-filename, got: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a == "-o"),
            "cross Bin must not emit -o, got: {args:?}"
        );

        // Output should be at <cross_final_dir>/hello, not <...>/hello-gluon-hello.
        assert!(
            output_path.ends_with("hello"),
            "output path should end with crate name, got: {output_path:?}"
        );
        assert!(
            !output_path.to_string_lossy().contains("-gluon-"),
            "output path should not contain -gluon- suffix, got: {output_path:?}"
        );
    }

    /// End-to-end compilation of a trivial host lib crate against the real
    /// toolchain. Gated with `#[ignore]` because it spawns rustc — run with:
    ///   cargo test -p gluon-core -- --ignored e2e_compile_trivial_host_lib_crate
    #[test]
    #[ignore]
    fn e2e_compile_trivial_host_lib_crate() {
        let tmp = tempfile::tempdir().unwrap();

        // Write a trivial source file.
        let src_dir = tmp.path().join("crates/trivial/src");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(src_dir.join("lib.rs"), b"pub fn hi() {}").unwrap();

        let rustc_info = RustcInfo::probe().expect("probe rustc");
        let layout = BuildLayout::new(tmp.path().join("build"), "e2e");
        let cache = Cache::load(layout.cache_manifest()).expect("load cache");
        std::fs::create_dir_all(layout.root()).unwrap();
        let ctx = CompileCtx::new(layout, Arc::new(rustc_info), cache);

        let mut model = BuildModel::default();
        let th = insert_target(
            &mut model,
            make_target("host", "x86_64-unknown-linux-gnu", true),
        );
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "trivial".into(),
                path: "crates/trivial".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                ..Default::default()
            },
        );
        let resolved = make_resolved(th, tmp.path().to_path_buf());
        let crate_ref = host_ref(ch, th);
        let artifacts = ArtifactMap::new();

        let result = compile(
            &ctx,
            CompileCrateInput {
                model: &model,
                resolved: &resolved,
                crate_ref: &crate_ref,
                artifacts: &artifacts,
                sysroot_dir: None,
            },
        );

        let (output_path, was_cached) = result.expect("compile should succeed");
        assert!(!was_cached, "first build must not report cached");
        assert!(output_path.exists(), "rlib must exist at {output_path:?}");
        assert!(
            std::fs::metadata(&output_path).unwrap().len() > 0,
            "rlib must be non-empty"
        );
    }
}
