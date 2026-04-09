//! Shared command context and plumbing for `gluon-cli` subcommands.
//!
//! Each subcommand needs some subset of the same preamble work: walk
//! up from the current directory to find a `gluon.rhai`, evaluate it,
//! pick a profile, resolve the config, probe rustc, load the cache
//! manifest, and construct a `CompileCtx`. Two entry points are
//! provided here:
//!
//! - [`build_context_at_for_driver`] does the full wiring including the
//!   rustc probe. Used by `build`, `check`, `clippy`, and `configure`
//!   — all of which genuinely need rustc metadata (for sysroot
//!   compilation, the metadata-only check pass, the lint pass, and the
//!   analyzer's `rust_src` field respectively). The driver flag flows
//!   down into [`gluon_core::BuildLayout`] so each command's output
//!   lands in its own subdirectory namespace.
//!
//! - [`build_layout_context_at`] stops just before the rustc probe
//!   and returns only the layout + project root. Used by `clean`,
//!   which only needs to know where `build/` lives. This is
//!   important: `clean` is the subcommand users reach for when their
//!   toolchain is broken, so forcing a rustc probe would make the
//!   tool useless in exactly that situation.
//!
//! Both `_at` variants take the working directory explicitly so unit
//! tests can exercise the full wiring against a tempdir without
//! mutating process-wide `current_dir`. The public [`build_context`]
//! wrapper reads the real cwd and delegates.

pub mod build;
pub mod check;
pub mod clean;
pub mod clippy;
pub mod configure;
pub mod external;
pub mod fmt;

use anyhow::{Context, Result};
use gluon_core::config::{
    DEFAULT_ENV_PREFIX, DEFAULT_OVERRIDE_FILENAME, load_env_overrides, load_override_file,
    merge_overrides,
};
use gluon_core::model::{BuildModel, ResolvedConfig};
use gluon_core::{
    BuildLayout, Cache, CompileCtx, DriverKind, RustcInfo, evaluate, find_project_root,
    resolve_config,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Everything a subcommand needs: the evaluated build model, the
/// resolved config for the active profile, the compile context
/// (layout + rustc info + cache), and where the project lives on disk.
pub struct CmdContext {
    /// The evaluated `gluon.rhai` model, kept around so subcommands can
    /// look up targets, crates, and groups referenced from the resolved
    /// config by handle.
    pub model: BuildModel,
    /// The resolved profile + options for this invocation.
    pub resolved: ResolvedConfig,
    /// Layout, rustc metadata, and cache manifest — everything rustc
    /// invocations need.
    pub ctx: CompileCtx,
    /// The directory containing `gluon.rhai` (the project root).
    ///
    /// Not consumed by today's three subcommands (`build`, `clean`,
    /// `configure`) — they all reach for `resolved.project_root`
    /// instead — but kept on the context so future subcommands and
    /// external plugins can route relative paths without re-walking
    /// from cwd.
    #[allow(dead_code)]
    pub project_root: PathBuf,
}

/// Build a [`CmdContext`] by walking up from the host's current
/// directory. Thin wrapper around [`build_context_at_for_driver`]; the resulting
/// context uses [`DriverKind::Rustc`] (i.e. the historical `gluon build`
/// layout). For `gluon check` / `gluon clippy`, use
/// [`build_context_for_driver`] instead.
pub fn build_context(
    profile: Option<&str>,
    target: Option<&str>,
    config_file: Option<&Path>,
) -> Result<CmdContext> {
    build_context_for_driver(profile, target, config_file, DriverKind::Rustc)
}

/// Build a [`CmdContext`] for an explicit driver. This is the entry
/// point used by `gluon check` and `gluon clippy`: the driver
/// determines whether the layout's user-crate output dirs land under
/// the historical paths or under `tool/check/` / `tool/clippy/`.
pub fn build_context_for_driver(
    profile: Option<&str>,
    target: Option<&str>,
    config_file: Option<&Path>,
    driver: DriverKind,
) -> Result<CmdContext> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    build_context_at_for_driver(&cwd, profile, target, config_file, driver)
}

/// Build a [`CmdContext`] starting the project-root search at `cwd`,
/// using the requested driver flavor.
///
/// Both axes are needed for the check/clippy commands to share
/// behavior with the existing build/configure tests, since the
/// integration tests parameterize on cwd (tempdir) and the new
/// commands parameterize on driver.
pub fn build_context_at_for_driver(
    cwd: &Path,
    profile: Option<&str>,
    target: Option<&str>,
    config_file: Option<&Path>,
    driver: DriverKind,
) -> Result<CmdContext> {
    let LayoutContext {
        model,
        resolved,
        layout,
        project_root,
    } = build_layout_context_at(cwd, profile, target, config_file)?;

    // Re-flavor the layout for the requested driver. The shared
    // `build_layout_context_at` constructs a Rustc-flavored layout by
    // default; for check/clippy we need to swap in a layout whose
    // user-crate path methods route under `tool/<driver>/`. Sysroot,
    // cache manifest, and config crate paths are unchanged because
    // they don't reference the user-crate root — see `BuildLayout`'s
    // doc comment for the rationale.
    let layout = BuildLayout::with_driver(
        layout.root().to_path_buf(),
        resolved.project.name.clone(),
        driver,
    );

    let rustc_info = Arc::new(
        RustcInfo::load_or_probe(&layout).context("failed to load or probe rustc metadata")?,
    );
    let cache = Cache::load(layout.cache_manifest()).context("failed to load cache manifest")?;
    let ctx = CompileCtx::new(layout, rustc_info, cache);

    Ok(CmdContext {
        model,
        resolved,
        ctx,
        project_root,
    })
}

/// Lighter-weight context used by subcommands that don't need rustc.
///
/// Contains everything up to and including the resolved config and
/// build layout, but not a [`CompileCtx`] — that would force a rustc
/// probe, which is exactly what we want to avoid for `clean`.
pub struct LayoutContext {
    pub model: BuildModel,
    pub resolved: ResolvedConfig,
    pub layout: BuildLayout,
    pub project_root: PathBuf,
}

/// Build a [`LayoutContext`] (no rustc probe) starting at `cwd`.
///
/// Used by `clean` so that a user with a broken or missing toolchain
/// can still wipe the build directory. `build` and `configure` go
/// through [`build_context_at_for_driver`] instead because they need the rustc
/// probe — `build` for sysroot compilation, `configure` for the
/// analyzer's `rust_src` field.
pub fn build_layout_context_at(
    cwd: &Path,
    profile: Option<&str>,
    target: Option<&str>,
    config_file: Option<&Path>,
) -> Result<LayoutContext> {
    let project_root = find_project_root(cwd)
        .context("could not find a gluon.rhai in the current directory or any parent")?;
    let script = project_root.join("gluon.rhai");
    let model = evaluate(&script).context("failed to evaluate gluon.rhai")?;

    // Pick a default profile when the user didn't pass `-p`. We use the
    // first profile by name (BTreeMap-backed, so this is deterministic
    // and alphabetical). A more principled default — e.g. a
    // `project.default_profile` field — can come later; for MVP-M the
    // convention is "declare what you want or pass `-p`".
    let default_name;
    let profile_name = match profile {
        Some(p) => p,
        None => {
            default_name = default_profile(&model).map(str::to_owned);
            match default_name.as_deref() {
                Some(n) => n,
                None => {
                    return Err(anyhow::anyhow!(
                        "no profiles declared in gluon.rhai; add at least one `profile(...)` \
                         definition or pass `-p <name>`"
                    ));
                }
            }
        }
    };

    // Per-checkout overrides: load the file (CLI flag → explicit path,
    // otherwise the default `<root>/.gluon-config` if present), then
    // layer environment variables on top. An absent default file is not
    // an error; an absent *explicit* file is. Env beats file on
    // conflicts (see `config::overrides::merge_overrides`).
    let overrides = {
        let file_path = match config_file {
            Some(p) => Some(p.to_path_buf()),
            None => {
                let default = project_root.join(DEFAULT_OVERRIDE_FILENAME);
                if default.exists() {
                    Some(default)
                } else {
                    None
                }
            }
        };
        let file_overrides = match &file_path {
            Some(p) => {
                if config_file.is_some() && !p.exists() {
                    return Err(anyhow::anyhow!(
                        "config override file {} does not exist",
                        p.display()
                    ));
                }
                load_override_file(p).context("failed to read config override file")?
            }
            None => Default::default(),
        };
        let env_overrides = load_env_overrides(DEFAULT_ENV_PREFIX);
        merge_overrides(file_overrides, env_overrides)
    };

    let overrides_arg = if overrides.is_empty() {
        None
    } else {
        Some(&overrides)
    };
    let resolved = resolve_config(&model, profile_name, target, &project_root, overrides_arg)
        .context("failed to resolve config")?;

    let layout = BuildLayout::new(project_root.join("build"), &resolved.project.name);

    Ok(LayoutContext {
        model,
        resolved,
        layout,
        project_root,
    })
}

/// Thin wrapper around [`build_layout_context_at`] that reads cwd.
pub fn build_layout_context(
    profile: Option<&str>,
    target: Option<&str>,
    config_file: Option<&Path>,
) -> Result<LayoutContext> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    build_layout_context_at(&cwd, profile, target, config_file)
}

/// Return the first profile name defined in the model, or `None` if the
/// model declares no profiles at all. `Arena::names` iterates in
/// BTreeMap (alphabetical) order, so this is deterministic.
fn default_profile(model: &BuildModel) -> Option<&str> {
    model.profiles.names().next().map(|(name, _)| name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Write the smallest possible valid `gluon.rhai` that evaluates
    /// and resolves cleanly — a project, a builtin target, and a
    /// profile pinned to that target. No crates or groups required.
    fn write_min_script(dir: &Path) {
        let script = r#"
            project("demo", "0.1.0");
            target("x86_64-unknown-none");
            profile("dev")
                .target("x86_64-unknown-none")
                .opt_level(0)
                .debug_info(true);
        "#;
        fs::write(dir.join("gluon.rhai"), script).expect("write gluon.rhai");
    }

    // These tests exercise the full context-building pipeline, which
    // calls `RustcInfo::load_or_probe` and therefore spawns `rustc`.
    // They follow the same probe-or-skip + `#[ignore]` pattern as the
    // scheduler e2e tests so they stay opt-in in sandboxed CI.

    #[test]
    #[ignore]
    fn build_context_at_wires_up_a_tempdir_project() {
        if gluon_core::RustcInfo::probe().is_err() {
            eprintln!("cli e2e test skipped: rustc probe failed");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        write_min_script(tmp.path());

        let cmd = build_context_at_for_driver(
            tmp.path(),
            None,
            None,
            None,
            gluon_core::DriverKind::Rustc,
        )
        .expect("build_context_at_for_driver");
        assert_eq!(cmd.resolved.project.name, "demo");
        assert_eq!(cmd.resolved.profile.name, "dev");
        // project_root should be the tempdir (canonicalized by find_project_root).
        assert!(
            cmd.project_root.canonicalize().ok() == tmp.path().canonicalize().ok()
                || cmd.project_root == tmp.path()
        );
    }

    #[test]
    #[ignore]
    fn default_profile_picks_first_alphabetically() {
        if gluon_core::RustcInfo::probe().is_err() {
            eprintln!("cli e2e test skipped: rustc probe failed");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let script = r#"
            project("demo", "0.1.0");
            target("x86_64-unknown-none");
            profile("zeta").target("x86_64-unknown-none");
            profile("alpha").target("x86_64-unknown-none");
        "#;
        fs::write(tmp.path().join("gluon.rhai"), script).expect("write");
        let cmd = build_context_at_for_driver(
            tmp.path(),
            None,
            None,
            None,
            gluon_core::DriverKind::Rustc,
        )
        .expect("build_context_at_for_driver");
        assert_eq!(cmd.resolved.profile.name, "alpha");
    }
}
