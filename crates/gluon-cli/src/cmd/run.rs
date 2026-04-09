//! `gluon run` subcommand — builds the project and launches QEMU.
//!
//! See [`gluon_core::run`] for the actual pipeline; this module is a
//! thin adapter between [`crate::cli::RunArgs`] and
//! [`gluon_core::RunOptions`].

use super::CmdContext;
use crate::cli::RunArgs;
use anyhow::{Result, bail};
use gluon_core::model::BootMode;
use std::time::Duration;

/// Execute the `gluon run` pipeline against the prepared context.
pub fn run(ctx: CmdContext, jobs: Option<usize>, args: RunArgs) -> Result<()> {
    // CLI boot-mode override. `--uefi` and `--direct` are mutually
    // exclusive in the clap schema; if neither is set, we pass `None`
    // so the profile's own `qemu().boot_mode(...)` wins.
    let boot_mode_override = match (args.uefi, args.direct) {
        (true, false) => Some(BootMode::Uefi),
        (false, true) => Some(BootMode::Direct),
        (false, false) => None,
        (true, true) => unreachable!("clap enforces mutual exclusion"),
    };

    let opts = gluon_core::RunOptions {
        boot_mode_override,
        timeout_override: args.timeout.map(Duration::from_secs),
        extra_args: args.extra,
        workers: jobs,
        dry_run: args.dry_run,
        no_build: args.no_build,
        gdb: args.gdb,
        // Not exposed to `gluon run` — reserved for the future
        // `gluon test` subcommand.
        test_mode: false,
    };

    let status = gluon_core::run(&ctx.ctx, &ctx.model, &ctx.resolved, opts)?;

    if args.dry_run || status.success() {
        return Ok(());
    }

    // Distinguish three non-success cases so the user gets a
    // diagnostic that tells them what actually happened:
    //
    // 1. Signal-terminated QEMU (e.g. user hit Ctrl-C and SIGINT
    //    made it to QEMU directly rather than to gluon). On Unix,
    //    `ExitStatus::signal()` returns `Some(n)` in this case and
    //    `code()` returns `None`; on Windows the concept doesn't
    //    exist and this branch is skipped.
    // 2. QEMU exited with a numeric code — pass it through verbatim.
    // 3. Neither of the above (shouldn't happen on a well-formed
    //    wait result, but we handle it defensively).
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            bail!("QEMU killed by signal {sig}");
        }
    }
    let code = status.code().unwrap_or(-1);
    bail!("QEMU exited with status {code}");
}
