//! Clap command-line definition for the `gluon` binary.
//!
//! The root [`Cli`] struct captures global flags (profile, target, jobs,
//! verbose, quiet) that apply to every subcommand, plus a [`Command`]
//! enum that selects between the `build`, `clean`, `configure`
//! subcommands and the `external_subcommand` plugin arm.
//!
//! Parsing lives here so `main.rs` stays a thin dispatcher; keeping the
//! argv schema in one file makes it easier to unit-test without spinning
//! up the whole command pipeline.

use clap::{Parser, Subcommand};
use std::ffi::OsString;
use std::path::PathBuf;

/// Parse `-j/--jobs`. Rejects zero with a friendly message instead of
/// the numeric-overflow noise rustc would produce later in the
/// scheduler.
fn parse_jobs(s: &str) -> Result<usize, String> {
    let n: usize = s.parse().map_err(|_| format!("'{s}' is not a number"))?;
    if n == 0 {
        return Err("must be at least 1 (use no -j flag for the default)".into());
    }
    Ok(n)
}

/// Top-level `gluon` command-line interface.
#[derive(Parser, Debug)]
#[command(name = "gluon", version, about = "Bare-metal Rust kernel build system")]
pub struct Cli {
    /// Profile name to use (overrides the default in gluon.rhai).
    #[arg(short = 'p', long, global = true)]
    pub profile: Option<String>,

    /// Target name to use (overrides the profile's pinned target).
    #[arg(short = 't', long, global = true)]
    pub target: Option<String>,

    /// Override file with `KEY = value` entries (defaults to
    /// `<project_root>/.gluon-config` when present).
    ///
    /// Values from this file are merged on top of the defaults declared
    /// in `gluon.rhai`. Environment variables prefixed `GLUON_` win over
    /// the file. See `gluon_core::config::overrides` for the grammar.
    #[arg(short = 'C', long = "config-file", global = true)]
    pub config_file: Option<PathBuf>,

    /// Number of parallel compile jobs (defaults to available parallelism).
    ///
    /// Must be at least 1; clap rejects `-j 0` at parse time via
    /// [`parse_jobs`] so the scheduler never sees a worker count that
    /// would deadlock the ready queue.
    #[arg(short = 'j', long, global = true, value_parser = parse_jobs)]
    pub jobs: Option<usize>,

    /// Emit more verbose output.
    #[arg(short = 'v', long, global = true)]
    pub verbose: bool,

    /// Suppress non-error output.
    #[arg(short = 'q', long, global = true)]
    pub quiet: bool,

    /// Which subcommand to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Subcommands dispatched by the `gluon` binary.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Build the project.
    Build(BuildArgs),
    /// Run a metadata-only check pass over every crate in the build
    /// model. Equivalent to `cargo check` but uses gluon's per-crate
    /// flag assembly so the same `--cfg`s, target, sysroot, and
    /// extern deps that `gluon build` would use are applied here too.
    Check(CheckArgs),
    /// Run clippy over every crate in the build model. Same per-crate
    /// flag assembly as `gluon build`, but the program path swaps
    /// `rustc` for `clippy-driver` (resolved via `$CLIPPY_DRIVER`,
    /// then a sibling-of-rustc heuristic, then `$PATH`).
    Clippy(ClippyArgs),
    /// Run `rustfmt` over every crate. Pass `--check` to verify
    /// formatting without rewriting (matches `cargo fmt --check`).
    Fmt(FmtArgs),
    /// Remove the build directory.
    Clean(CleanArgs),
    /// Generate `rust-project.json` for rust-analyzer.
    Configure(ConfigureArgs),
    /// Vendor external dependencies declared in `gluon.rhai`.
    ///
    /// Synthesises a scratch `Cargo.toml`, invokes `cargo vendor` to
    /// populate `./vendor/`, and writes `gluon.lock` pinning the
    /// result. `--check` verifies the existing lock without touching
    /// disk; `--force` bypasses the fingerprint-match fast path.
    Vendor(VendorArgs),
    /// Dispatch to an external `gluon-<name>` binary on `$PATH`.
    ///
    /// Clap's `external_subcommand` captures everything after the
    /// unknown command name, and the first element of the returned
    /// vector is the subcommand name itself (not stripped by clap).
    #[command(external_subcommand)]
    External(Vec<OsString>),
}

/// Arguments for `gluon build`.
#[derive(clap::Args, Debug)]
pub struct BuildArgs {}

/// Arguments for `gluon check`. Currently empty — the same global
/// `-p`/`-t`/`-j`/`-C` flags that apply to `build` apply here.
#[derive(clap::Args, Debug)]
pub struct CheckArgs {}

/// Arguments for `gluon clippy`. Currently empty.
#[derive(clap::Args, Debug)]
pub struct ClippyArgs {}

/// Arguments for `gluon fmt`.
#[derive(clap::Args, Debug)]
pub struct FmtArgs {
    /// Verify formatting without rewriting files (mirrors
    /// `cargo fmt --check`). Exit code is non-zero if any crate has
    /// unformatted files.
    #[arg(long)]
    pub check: bool,
}

/// Arguments for `gluon clean`.
#[derive(clap::Args, Debug)]
pub struct CleanArgs {
    /// Keep the custom sysroot directory (default: wipe the whole build directory).
    #[arg(long)]
    pub keep_sysroot: bool,
}

/// Arguments for `gluon configure`.
#[derive(clap::Args, Debug)]
pub struct ConfigureArgs {
    /// Output path for `rust-project.json` (default: `<project_root>/rust-project.json`).
    #[arg(short, long)]
    pub output: Option<PathBuf>,
}

/// Arguments for `gluon vendor`.
#[derive(clap::Args, Debug)]
pub struct VendorArgs {
    /// Verify the existing vendor tree without modifying anything.
    ///
    /// Exits with a non-zero status if `gluon.lock` is missing, its
    /// fingerprint disagrees with the declared deps, or any vendored
    /// crate's on-disk checksum has drifted.
    #[arg(long)]
    pub check: bool,

    /// Ignore the fingerprint-match fast path and re-run `cargo
    /// vendor` unconditionally. Useful after hand-editing `vendor/`.
    #[arg(long)]
    pub force: bool,

    /// Pass `--offline` / `--frozen` through to cargo. Forbids any
    /// network access; expects the lockfile to already be up to date.
    #[arg(long)]
    pub offline: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_bare_build() {
        let cli = Cli::try_parse_from(["gluon", "build"]).expect("parse");
        assert!(matches!(cli.command, Command::Build(_)));
        assert_eq!(cli.profile, None);
    }

    #[test]
    fn parses_bare_check() {
        let cli = Cli::try_parse_from(["gluon", "check"]).expect("parse");
        assert!(matches!(cli.command, Command::Check(_)));
    }

    #[test]
    fn parses_check_with_profile_and_jobs() {
        let cli = Cli::try_parse_from(["gluon", "-p", "dev", "-j", "2", "check"]).expect("parse");
        assert_eq!(cli.profile.as_deref(), Some("dev"));
        assert_eq!(cli.jobs, Some(2));
        assert!(matches!(cli.command, Command::Check(_)));
    }

    #[test]
    fn parses_bare_clippy() {
        let cli = Cli::try_parse_from(["gluon", "clippy"]).expect("parse");
        assert!(matches!(cli.command, Command::Clippy(_)));
    }

    #[test]
    fn parses_fmt_default() {
        let cli = Cli::try_parse_from(["gluon", "fmt"]).expect("parse");
        match cli.command {
            Command::Fmt(a) => assert!(!a.check),
            other => panic!("expected Fmt, got {other:?}"),
        }
    }

    #[test]
    fn parses_fmt_check() {
        let cli = Cli::try_parse_from(["gluon", "fmt", "--check"]).expect("parse");
        match cli.command {
            Command::Fmt(a) => assert!(a.check),
            other => panic!("expected Fmt, got {other:?}"),
        }
    }

    #[test]
    fn parses_short_profile() {
        let cli = Cli::try_parse_from(["gluon", "-p", "release", "build"]).expect("parse");
        assert_eq!(cli.profile.as_deref(), Some("release"));
        assert!(matches!(cli.command, Command::Build(_)));
    }

    #[test]
    fn parses_long_profile() {
        let cli = Cli::try_parse_from(["gluon", "--profile", "release", "build"]).expect("parse");
        assert_eq!(cli.profile.as_deref(), Some("release"));
        assert!(matches!(cli.command, Command::Build(_)));
    }

    #[test]
    fn parses_clean_with_keep_sysroot() {
        let cli = Cli::try_parse_from(["gluon", "clean", "--keep-sysroot"]).expect("parse");
        match cli.command {
            Command::Clean(a) => assert!(a.keep_sysroot),
            other => panic!("expected Clean, got {other:?}"),
        }
    }

    #[test]
    fn parses_clean_default() {
        let cli = Cli::try_parse_from(["gluon", "clean"]).expect("parse");
        match cli.command {
            Command::Clean(a) => assert!(!a.keep_sysroot),
            other => panic!("expected Clean, got {other:?}"),
        }
    }

    #[test]
    fn parses_configure_with_output() {
        let cli =
            Cli::try_parse_from(["gluon", "configure", "--output", "/tmp/rp.json"]).expect("parse");
        match cli.command {
            Command::Configure(a) => {
                assert_eq!(
                    a.output.as_deref(),
                    Some(std::path::Path::new("/tmp/rp.json"))
                );
            }
            other => panic!("expected Configure, got {other:?}"),
        }
    }

    #[test]
    fn parses_bare_vendor() {
        let cli = Cli::try_parse_from(["gluon", "vendor"]).expect("parse");
        match cli.command {
            Command::Vendor(a) => {
                assert!(!a.check);
                assert!(!a.force);
                assert!(!a.offline);
            }
            other => panic!("expected Vendor, got {other:?}"),
        }
    }

    #[test]
    fn parses_vendor_check_and_force() {
        let cli = Cli::try_parse_from(["gluon", "vendor", "--check", "--force"]).expect("parse");
        match cli.command {
            Command::Vendor(a) => {
                assert!(a.check);
                assert!(a.force);
            }
            other => panic!("expected Vendor, got {other:?}"),
        }
    }

    #[test]
    fn parses_vendor_offline() {
        let cli = Cli::try_parse_from(["gluon", "vendor", "--offline"]).expect("parse");
        match cli.command {
            Command::Vendor(a) => assert!(a.offline),
            other => panic!("expected Vendor, got {other:?}"),
        }
    }

    #[test]
    fn parses_external_subcommand() {
        let cli = Cli::try_parse_from(["gluon", "foo", "bar", "baz"]).expect("parse");
        match cli.command {
            Command::External(args) => {
                let as_str: Vec<_> = args
                    .iter()
                    .map(|s| s.to_string_lossy().into_owned())
                    .collect();
                assert_eq!(as_str, vec!["foo", "bar", "baz"]);
            }
            other => panic!("expected External, got {other:?}"),
        }
    }

    #[test]
    fn parses_jobs_flag() {
        let cli = Cli::try_parse_from(["gluon", "-j", "4", "build"]).expect("parse");
        assert_eq!(cli.jobs, Some(4));
    }

    #[test]
    fn rejects_jobs_zero() {
        // -j 0 is meaningless and would deadlock the scheduler. Rejected
        // at parse time so the user gets a friendly error instead of a
        // confusing scheduler stall.
        let err =
            Cli::try_parse_from(["gluon", "-j", "0", "build"]).expect_err("-j 0 must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("at least 1"),
            "error must explain the constraint, got: {msg}"
        );
    }

    #[test]
    fn rejects_jobs_non_numeric() {
        let err = Cli::try_parse_from(["gluon", "-j", "abc", "build"])
            .expect_err("-j abc must be rejected");
        let msg = err.to_string();
        assert!(msg.contains("not a number"), "got: {msg}");
    }

    #[test]
    fn parses_verbose_and_quiet_together() {
        // Clap allows both — the app decides which wins.
        let cli = Cli::try_parse_from(["gluon", "--verbose", "--quiet", "build"]).expect("parse");
        assert!(cli.verbose);
        assert!(cli.quiet);
    }
}
