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

    // `internal` subcommands are introspection/maintenance tools. None
    // of them read a `gluon.rhai` off disk — dumping the DSL function
    // list, for instance, only needs the in-memory engine registration
    // — so they must short-circuit ahead of the context builders. That
    // also means they can run from any directory.
    if let cli::Command::Internal(sub) = &cli.command {
        return match sub {
            cli::InternalCommand::DumpDsl => cmd::internal::run_dump_dsl(),
        };
    }

    // `clean` and `fmt` take the lighter layout-only context (no
    // rustc probe) so they still work when the toolchain is broken
    // or missing. `fmt` only needs `rustfmt`, which it resolves
    // separately via $RUSTFMT or $PATH.
    if let cli::Command::Clean(args) = &cli.command {
        let ctx = cmd::build_layout_context(
            cli.profile.as_deref(),
            cli.target.as_deref(),
            cli.config_file.as_deref(),
        )?;
        return cmd::clean::run(ctx, args.keep_sysroot);
    }
    if let cli::Command::Fmt(args) = &cli.command {
        let ctx = cmd::build_layout_context(
            cli.profile.as_deref(),
            cli.target.as_deref(),
            cli.config_file.as_deref(),
        )?;
        return cmd::fmt::run(ctx, args.check);
    }
    // `vendor` skips the vendor-autoregister step in the context
    // builder because it *is* the thing that produces / fixes the
    // lock file — registering against a stale lock would abort
    // before we could repair it.
    if let cli::Command::Vendor(args) = &cli.command {
        let cwd = std::env::current_dir()?;
        let ctx = cmd::build_layout_context_at_with_opts(
            &cwd,
            cli.profile.as_deref(),
            cli.target.as_deref(),
            cli.config_file.as_deref(),
            cmd::LayoutContextOpts {
                skip_vendor_autoreg: true,
            },
        )?;
        return cmd::vendor::run(ctx, args.check, args.force, args.offline);
    }

    // `check` builds a context whose layout is flavored for the check
    // driver — that's what routes user-crate output under
    // `build/tool/check/` so it can't clobber `gluon build` artifacts.
    // Same rustc probe path otherwise; check still needs the host
    // toolchain to invoke `--emit=metadata`.
    // `check` and `clippy` build a context whose layout is flavored
    // for the relevant driver — that's what routes user-crate output
    // under `build/tool/<driver>/` so it can't clobber `gluon build`
    // artifacts. Same rustc probe path otherwise; both still need the
    // host toolchain.
    if matches!(&cli.command, cli::Command::Check(_)) {
        let ctx = cmd::build_context_for_driver(
            cli.profile.as_deref(),
            cli.target.as_deref(),
            cli.config_file.as_deref(),
            gluon_core::DriverKind::Check,
        )?;
        return cmd::check::run(ctx, cli.jobs);
    }
    if matches!(&cli.command, cli::Command::Clippy(_)) {
        let ctx = cmd::build_context_for_driver(
            cli.profile.as_deref(),
            cli.target.as_deref(),
            cli.config_file.as_deref(),
            gluon_core::DriverKind::Clippy,
        )?;
        return cmd::clippy::run(ctx, cli.jobs);
    }

    let ctx = cmd::build_context(
        cli.profile.as_deref(),
        cli.target.as_deref(),
        cli.config_file.as_deref(),
    )?;

    match cli.command {
        cli::Command::Build(_) => cmd::build::run(ctx, cli.jobs),
        cli::Command::Configure(args) => cmd::configure::run(ctx, args.output),
        cli::Command::Run(args) => cmd::run::run(ctx, cli.jobs, args),
        cli::Command::Check(_)
        | cli::Command::Clippy(_)
        | cli::Command::Clean(_)
        | cli::Command::Fmt(_)
        | cli::Command::Vendor(_)
        | cli::Command::Internal(_)
        | cli::Command::External(_) => {
            unreachable!("handled above")
        }
    }
}
