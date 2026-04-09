//! Rustc command assembly for per-crate compilation.
//!
//! This module contains [`build_rustc_command`], which assembles a
//! [`RustcCommandBuilder`] for a single user crate without spawning rustc.
//! It was extracted from `compile_crate.rs` to keep that module focused on
//! cache integration and process management.

use super::compile_crate::CompileCrateInput;
use super::compile_utils::{exe_suffix_for_target, normalize_crate_name, sanitise_crate_name};
use super::{CompileCtx, Emit, RustcCommandBuilder};
use crate::error::{Error, Result};
use gluon_model::{CrateDef, CrateType};
use std::path::PathBuf;

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
    // rustc rejects `-` in `--crate-name`. Cargo silently normalizes
    // `my-crate` → `my_crate`; we mirror that so cargo-style names in
    // `gluon.rhai` Just Work. The original `crate_name` is kept for error
    // messages and cache keys (it's what the user typed). The normalized
    // form (`crate_name_rustc`) is what we hand to rustc and what we
    // splice into on-disk filenames so they line up with what rustc
    // actually writes (rustc derives the filename from `--crate-name` +
    // `extra-filename`). See `engine::validate` for the post-normalization
    // collision check that rejects e.g. both `foo-bar` and `foo_bar` in
    // the same model.
    let crate_name_rustc = normalize_crate_name(crate_name);

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
    let extra_filename = format!("-gluon-{crate_name_rustc}");

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
                out_dir.join(format!("{crate_name_rustc}{extra_filename}"))
            }
            CrateType::ProcMacro => {
                // Proc-macros produce a .so/.dylib. rustc determines the
                // exact extension; for the cache output_path we use the
                // platform DLL extension so the existence check is accurate.
                let ext = std::env::consts::DLL_EXTENSION;
                out_dir.join(format!(
                    "{}{}{}{}",
                    std::env::consts::DLL_PREFIX,
                    crate_name_rustc,
                    extra_filename,
                    if ext.is_empty() {
                        String::new()
                    } else {
                        format!(".{ext}")
                    }
                ))
            }
            _ => out_dir.join(format!("lib{crate_name_rustc}{extra_filename}.rlib")),
        };
        let depfile = out_dir.join(format!("{crate_name_rustc}{extra_filename}.d"));
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
                // For binaries, rustc writes the file at
                // `<--crate-name><exe-suffix>` where exe-suffix is
                // target-specific (empty for bare-metal, `.efi` for UEFI,
                // `.exe` for Windows). We must use the *normalized* name
                // and the correct suffix so the artifact path matches
                // what rustc actually writes on disk.
                let suffix = exe_suffix_for_target(&target.spec);
                let artifact = final_dir.join(format!("{}{suffix}", crate_name_rustc));
                // Depfile: without extra-filename, rustc writes `<name>.d`
                // in the --out-dir (final_dir).
                let depfile = final_dir.join(format!("{crate_name_rustc}.d"));
                // Use final_dir as out_dir so the binary is placed there.
                // The incremental dir is set separately via -C incremental=.
                (final_dir, artifact, depfile)
            }
            _ => {
                let out_dir = layout.cross_artifact_dir(target, &resolved.profile, crate_def);
                let artifact = out_dir.join(format!("lib{crate_name_rustc}{extra_filename}.rlib"));
                let depfile = out_dir.join(format!("{crate_name_rustc}{extra_filename}.d"));
                (out_dir, artifact, depfile)
            }
        }
    };

    // --- Builder assembly ---
    //
    // The program path is selected by `ctx.driver`. For
    // `DriverKind::Rustc` (the historical default) this is just the
    // probed rustc path. For `Check` it is also rustc, but we will
    // override the `--emit` kinds below to suppress codegen. For
    // `Clippy` it resolves `clippy-driver` (env, sibling-of-rustc, or
    // PATH lookup), which is rustc-CLI-compatible so every flag below
    // applies unchanged.
    let program = ctx.driver().program(&ctx.rustc_info);
    let mut builder = RustcCommandBuilder::new(&program);

    // Input source file — positional arg, must come first.
    builder.input(&src_file);

    // Crate identity flags.
    builder
        .crate_name(&crate_name_rustc)
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

        // Inject the generated config crate for this crate's target.
        // Each cross target has its own config crate rlib so there is
        // no target-mismatch risk.
        if let Some(config_path) = artifacts.config_crate(crate_ref.target) {
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

    // Emit flags. The depfile path is threaded in explicitly so rustc writes
    // it to a location we control, rather than deriving it from --out-dir +
    // crate name + extra-filename. This keeps the "where does the .d land?"
    // knowledge in one place (the `depfile_path` bound above).
    //
    // The driver may force a metadata-only emit (`Check` and `Clippy`
    // both do this) — in that case it returns `Some(slice)` from
    // `emit_override` and we use it instead of the per-crate-type
    // selection. The depfile is still emitted so the cache freshness
    // logic continues to track source dependencies; it is correct on a
    // metadata-only build because rustc still produces accurate
    // dep-info even when codegen is suppressed.
    //
    // **Proc-macros are exempt from the override.** A proc-macro crate
    // must produce a real dylib because the dependent crate's compile
    // pass dlopens it at compile time. Forcing `--emit=metadata` here
    // would write only an `.rmeta` and the next crate's rustc would
    // fail with "extern location for X does not exist". `cargo check`
    // has exactly the same behavior — it always builds proc-macros
    // fully, regardless of the check vs build distinction. We mirror
    // that.
    let default_emit: &[Emit] = match crate_def.crate_type {
        CrateType::Bin => &[Emit::Link, Emit::DepInfo],
        _ => &[Emit::Link, Emit::Metadata, Emit::DepInfo],
    };
    let emit_kinds: &[Emit] = if crate_def.crate_type == CrateType::ProcMacro {
        default_emit
    } else {
        ctx.driver().emit_override().unwrap_or(default_emit)
    };
    builder.emit_with_dep_info_path(emit_kinds, &depfile_path);

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

    // Artifact env vars (BTreeMap iteration = deterministic order, so the
    // cache-key hash is stable across runs). Each entry names a sibling
    // crate whose absolute, canonicalised output path gets injected as
    // an env var at rustc spawn time. The calling source can then use
    // `env!("KEY")` at compile time — e.g. `include_bytes!(env!("KERNEL_PATH"))`.
    //
    // This function relies on the scheduler having built the referenced
    // crate already; the DAG edge comes from `CrateDef::artifact_deps`,
    // which the Rhai builder auto-populates alongside `artifact_env`.
    // Missing artifacts here are a scheduler bug (same error shape as
    // the `--extern` loop above).
    for (env_key, dep_name) in &crate_def.artifact_env {
        let dep_handle = model.crates.lookup(dep_name).ok_or_else(|| {
            Error::Compile(format!(
                "crate '{}' has artifact_env entry '{}={}', but no crate named '{}' \
                 exists in the build model (validate pass should have caught this)",
                crate_name, env_key, dep_name, dep_name,
            ))
        })?;
        let artifact_path = artifacts.get(dep_handle).ok_or_else(|| {
            Error::Compile(format!(
                "crate '{}' artifact_env references '{}' but its artifact is not \
                 available in ArtifactMap (scheduler bug — artifact_deps ordering \
                 edge should have built it first)",
                crate_name, dep_name,
            ))
        })?;
        // Canonicalise so `env!` receives a stable absolute path regardless
        // of where the build was invoked from. Fall back to the original
        // path if canonicalisation fails — e.g. inside tests that don't
        // actually write the artifact to disk.
        let abs_path = std::fs::canonicalize(artifact_path)
            .unwrap_or_else(|_| artifact_path.to_path_buf());
        builder.env(env_key.as_str(), abs_path.as_os_str());
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

    // For Check/Clippy on non-proc-macro crates, the artifact path
    // computed above (rlib / bin / dylib) is *never written* — only
    // metadata + dep-info land on disk. The cache freshness check in
    // `compile_and_cache` uses `output_path.exists()` as a final
    // sanity guard, which would fail forever under those drivers and
    // re-run rustc on every invocation.
    //
    // Use the depfile as the cache stamp instead: rustc reliably
    // writes it under both Rustc and Check/Clippy, and its existence
    // is a faithful proxy for "we ran a successful compile". The
    // BuildRecord stores it the same way as a real artifact path; the
    // `compile()` caller's return value is the path that downstream
    // crates use to wire `--extern`, so for proc-macros (which still
    // produce a real artifact under Check) we keep the original
    // dylib path.
    let cache_output_path =
        if ctx.driver().emit_override().is_some() && crate_def.crate_type != CrateType::ProcMacro {
            depfile_path.clone()
        } else {
            output_path.clone()
        };

    Ok((builder, cache_output_path, depfile_path))
}
