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

    /// Number of parallel compile jobs (defaults to available parallelism).
    #[arg(short = 'j', long, global = true)]
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
    /// Remove the build directory.
    Clean(CleanArgs),
    /// Generate `rust-project.json` for rust-analyzer.
    Configure(ConfigureArgs),
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
    fn parses_verbose_and_quiet_together() {
        // Clap allows both — the app decides which wins.
        let cli = Cli::try_parse_from(["gluon", "--verbose", "--quiet", "build"]).expect("parse");
        assert!(cli.verbose);
        assert!(cli.quiet);
    }
}
