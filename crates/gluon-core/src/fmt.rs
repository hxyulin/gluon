//! `gluon fmt` — invoke `rustfmt` over every crate in the build model.
//!
//! Unlike `gluon check` and `gluon clippy`, `fmt` does **not** reuse
//! the per-crate `RustcCommandBuilder` flag assembly. Rustfmt has zero
//! flag overlap with rustc — it takes a list of source files plus an
//! `--edition` flag, nothing else gluon would care about. Reusing the
//! builder for fmt would be machinery for its own sake.
//!
//! Instead this module:
//!
//! 1. Iterates `resolved.crates` in deterministic order.
//! 2. For each crate, walks its source root recursively to enumerate
//!    every `.rs` file (sorted, so the rustfmt invocation is
//!    deterministic).
//! 3. Spawns `rustfmt --edition=<e> [--check] file1 file2 …`.
//!
//! The rustfmt binary is resolved via `$RUSTFMT` first, then bare
//! `rustfmt` on `$PATH`. Resolving via the rustup-managed sibling of
//! `rustc` would require a rustc probe (which `gluon fmt` otherwise
//! avoids), so we punt that to a future improvement.
//!
//! Project-level `rustfmt.toml` files are picked up automatically by
//! rustfmt itself when invoked from the project root.

use crate::error::{Diagnostic, Error, Result};
use gluon_model::{BuildModel, CrateType, ResolvedConfig};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Outcome of a `gluon fmt` run.
#[derive(Debug, Clone, Default)]
pub struct FmtSummary {
    /// Number of crates that rustfmt processed cleanly. In `check`
    /// mode this counts crates that were already formatted.
    pub formatted: usize,
    /// Crate names that were skipped because they had no `.rs` files
    /// (typically a misconfigured `path =`).
    pub skipped: Vec<String>,
    /// Crate names whose files were unformatted (only populated in
    /// `check` mode — in non-check mode rustfmt rewrites them in
    /// place and the run is still considered "formatted").
    pub unformatted: Vec<String>,
}

/// Run `rustfmt` over every crate in `resolved.crates`.
///
/// `check_mode` mirrors `cargo fmt --check`: pass `--check` to
/// rustfmt and surface unformatted files as a non-success summary
/// (the function still returns `Ok(summary)` so callers can render
/// per-crate output; the CLI layer is responsible for translating a
/// non-empty `unformatted` list into a non-zero exit code).
pub fn run_fmt(
    model: &BuildModel,
    resolved: &ResolvedConfig,
    project_root: &Path,
    check_mode: bool,
) -> Result<FmtSummary> {
    let rustfmt = resolve_rustfmt();
    let mut summary = FmtSummary::default();

    for crate_ref in &resolved.crates {
        let crate_def = model.crates.get(crate_ref.handle).ok_or_else(|| {
            Error::Compile(format!(
                "crate handle {:?} not found in build model",
                crate_ref.handle
            ))
        })?;

        // The crate root: directory the user wrote in `path =`. Source
        // files live somewhere under here. We don't reuse the per-
        // crate-type `src/main.rs` / `src/lib.rs` heuristic because we
        // want to format every .rs file the user has, not just the
        // entry point.
        let crate_dir = project_root.join(&crate_def.path);
        if !crate_dir.is_dir() {
            return Err(Error::Compile(format!(
                "crate '{}': source path {} does not exist",
                crate_def.name,
                crate_dir.display()
            )));
        }

        // Find every .rs file under the crate dir. Deterministic
        // sorted order so the rustfmt argv (and any error message
        // referencing it) is reproducible across runs.
        let mut files = collect_rs_files(&crate_dir);
        files.sort();
        if files.is_empty() {
            summary.skipped.push(crate_def.name.clone());
            continue;
        }

        let mut cmd = Command::new(&rustfmt);
        cmd.current_dir(project_root)
            .arg("--edition")
            .arg(&crate_def.edition);
        if check_mode {
            cmd.arg("--check");
        }
        // Proc-macro crates and bins format the same way as libs;
        // crate_type doesn't affect rustfmt invocation. We keep the
        // crate_type read for documentation only.
        let _ = (CrateType::Lib, crate_def.crate_type);

        for f in &files {
            cmd.arg(f);
        }

        let output = cmd.output().map_err(|e| {
            Error::Diagnostics(vec![
                Diagnostic::error(format!(
                    "failed to spawn rustfmt for crate '{}': {e}",
                    crate_def.name
                ))
                .with_note(format!(
                    "tried to invoke {} (set $RUSTFMT to override)",
                    rustfmt.display()
                )),
            ])
        })?;

        if output.status.success() {
            summary.formatted += 1;
        } else if check_mode {
            // In --check mode, rustfmt exits non-zero when files are
            // unformatted. That's a normal "needs fixing" outcome, not
            // an error — record it and continue so we report every
            // unformatted crate in one run.
            summary.unformatted.push(crate_def.name.clone());
        } else {
            // Non-check mode: rustfmt failure is a hard error (e.g.
            // syntax error in a source file).
            return Err(Error::Diagnostics(vec![
                Diagnostic::error(format!(
                    "rustfmt failed on crate '{}': exit={:?}",
                    crate_def.name,
                    output.status.code()
                ))
                .with_note(format!(
                    "stderr:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                )),
            ]));
        }
    }

    Ok(summary)
}

/// Resolve which `rustfmt` binary to invoke. Honors `$RUSTFMT` first
/// (matching cargo's convention) and falls back to bare `rustfmt` on
/// `$PATH`. The bare-name fallback relies on the OS to perform `$PATH`
/// lookup at spawn time.
fn resolve_rustfmt() -> PathBuf {
    if let Some(p) = std::env::var_os("RUSTFMT") {
        return PathBuf::from(p);
    }
    PathBuf::from("rustfmt")
}

/// Recursively collect every `.rs` file under `dir`. Hand-rolled
/// rather than pulling in `walkdir` because the traversal here is
/// trivial and we don't otherwise depend on it. Hidden directories
/// (those starting with `.`) and `target/` are skipped — they
/// shouldn't contain user-authored sources.
fn collect_rs_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(dir, &mut out);
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        // Skip hidden dirs/files and `target/` so we don't try to
        // rustfmt our own build output if it lands inside the source
        // tree.
        if name_str.starts_with('.') || name_str == "target" {
            continue;
        }
        let ty = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ty.is_dir() {
            walk(&path, out);
        } else if ty.is_file() && path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn collect_rs_files_finds_nested_sources() {
        let dir = tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("src/inner")).unwrap();
        fs::write(dir.path().join("src/lib.rs"), "// a").unwrap();
        fs::write(dir.path().join("src/inner/mod.rs"), "// b").unwrap();
        fs::write(dir.path().join("src/inner/util.rs"), "// c").unwrap();
        fs::write(dir.path().join("README.md"), "ignored").unwrap();
        let mut files = collect_rs_files(dir.path());
        files.sort();
        // Sorted by full path, not basename: `src/inner/...` comes
        // before `src/lib.rs` because "inner" < "lib" lexically.
        // Strip the tempdir prefix for a stable comparison.
        let rels: Vec<String> = files
            .iter()
            .map(|p| {
                p.strip_prefix(dir.path())
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(
            rels,
            vec!["src/inner/mod.rs", "src/inner/util.rs", "src/lib.rs"]
        );
    }

    #[test]
    fn collect_rs_files_skips_hidden_and_target_dirs() {
        let dir = tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("target/debug")).unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join("target/debug/leftover.rs"), "skip").unwrap();
        fs::write(dir.path().join(".git/HEAD.rs"), "skip").unwrap();
        fs::write(dir.path().join("real.rs"), "keep").unwrap();
        let files = collect_rs_files(dir.path());
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["real.rs"]);
    }

    #[test]
    fn resolve_rustfmt_env_var_then_default() {
        // Combined into a single test because the two cases share
        // process-global env state — running them as separate tests
        // would race under cargo's parallel runner.
        // SAFETY: this is the only test in the binary that reads or
        // writes RUSTFMT. We set, observe, then unset, and observe
        // the unset behavior — all in one serialized test body.
        let sentinel = "/totally/not/real/rustfmt-sentinel";
        unsafe { std::env::set_var("RUSTFMT", sentinel) };
        assert_eq!(resolve_rustfmt(), PathBuf::from(sentinel));
        unsafe { std::env::remove_var("RUSTFMT") };
        assert_eq!(resolve_rustfmt(), PathBuf::from("rustfmt"));
    }
}
