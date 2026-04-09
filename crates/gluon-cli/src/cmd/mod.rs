//! Shared command context and plumbing for `gluon-cli` subcommands.
//!
//! Each subcommand (`build`, `clean`, `configure`) needs the same set of
//! preamble work: walk up from the current directory to find a
//! `gluon.rhai`, evaluate it, pick a profile, resolve the config, probe
//! rustc, load the cache manifest, and construct a `CompileCtx`. That
//! work lives here in [`build_context_at`] so each subcommand module can
//! stay focused on the *action* rather than the *setup*.
//!
//! `build_context_at(cwd, ...)` takes the working directory explicitly
//! so unit tests can exercise the full wiring against a tempdir without
//! mutating process-wide `current_dir`. The public [`build_context`]
//! wrapper reads the real cwd and delegates.

pub mod build;
pub mod clean;
pub mod configure;
pub mod external;

use anyhow::{Context, Result};
use gluon_core::model::{BuildModel, ResolvedConfig};
use gluon_core::{
    BuildLayout, Cache, CompileCtx, RustcInfo, evaluate, find_project_root, resolve_config,
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
/// directory. Thin wrapper around [`build_context_at`].
pub fn build_context(profile: Option<&str>, target: Option<&str>) -> Result<CmdContext> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    build_context_at(&cwd, profile, target)
}

/// Build a [`CmdContext`] starting the project-root search at `cwd`.
///
/// Split out from [`build_context`] for testability: tests pass a
/// tempdir directly rather than mutating `std::env::set_current_dir`,
/// which would race with other tests in the same process.
pub fn build_context_at(
    cwd: &Path,
    profile: Option<&str>,
    target: Option<&str>,
) -> Result<CmdContext> {
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

    let resolved = resolve_config(&model, profile_name, target, &project_root, None)
        .context("failed to resolve config")?;

    let layout = BuildLayout::new(project_root.join("build"), &resolved.project.name);
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

        let cmd = build_context_at(tmp.path(), None, None).expect("build_context_at");
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
        let cmd = build_context_at(tmp.path(), None, None).expect("build_context_at");
        assert_eq!(cmd.resolved.profile.name, "alpha");
    }
}
