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

    // External subcommands dispatch before building the context — they
    // may be invoked outside any gluon project, so they don't need a
    // project root or an evaluated build model.
    if let cli::Command::External(args) = cli.command {
        return cmd::external::run(args);
    }

    let ctx = cmd::build_context(cli.profile.as_deref(), cli.target.as_deref())?;

    match cli.command {
        cli::Command::Build(_) => cmd::build::run(ctx, cli.jobs),
        cli::Command::Clean(args) => cmd::clean::run(ctx, args.keep_sysroot),
        cli::Command::Configure(args) => cmd::configure::run(ctx, args.output),
        cli::Command::External(_) => unreachable!("handled above"),
    }
}
