//! Gluon command-line entry point.
//!
//! Parses argv via clap, dispatches to the appropriate subcommand
//! module, and converts any returned error into a non-zero exit status.
//! The actual command logic lives in `cmd::{build,clean,configure,external}`;
//! this file is intentionally thin so it's easy to audit.

mod cli;
mod cmd;

use anyhow::Result;
use clap::Parser;

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {e:?}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = cli::Cli::parse();

    // External subcommands dispatch before building any context — they
    // may be invoked outside any gluon project, so they don't need a
    // project root or an evaluated build model.
    if let cli::Command::External(args) = cli.command {
        return cmd::external::run(args);
    }

    // `clean` takes the lighter layout-only context (no rustc probe)
    // so it still works when the toolchain is broken or missing.
    if let cli::Command::Clean(args) = &cli.command {
        let ctx = cmd::build_layout_context(
            cli.profile.as_deref(),
            cli.target.as_deref(),
            cli.config_file.as_deref(),
        )?;
        return cmd::clean::run(ctx, args.keep_sysroot);
    }

    let ctx = cmd::build_context(
        cli.profile.as_deref(),
        cli.target.as_deref(),
        cli.config_file.as_deref(),
    )?;

    match cli.command {
        cli::Command::Build(_) => cmd::build::run(ctx, cli.jobs),
        cli::Command::Configure(args) => cmd::configure::run(ctx, args.output),
        cli::Command::Clean(_) | cli::Command::External(_) => {
            unreachable!("handled above")
        }
    }
}
