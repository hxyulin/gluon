//! `gluon check` subcommand — runs the compile pipeline with the
//! check driver (`--emit=metadata` only, no codegen).
//!
//! Mirrors `cmd/build.rs` exactly except for two things:
//!
//! 1. The caller (in `main.rs`) constructs the `CmdContext` via
//!    `build_context_for_driver(..., DriverKind::Check)` so the layout's
//!    user-crate output dirs land under `tool/check/` instead of
//!    clobbering `gluon build` artifacts.
//! 2. The summary line uses the per-driver verb ("checked" vs "built")
//!    so users can tell which command just ran.

use super::CmdContext;
use anyhow::Result;

/// Execute the metadata-only check pipeline against the prepared
/// context. The context's layout must have been built with
/// `DriverKind::Check`; otherwise the lib-level `debug_assert_eq!`
/// in `gluon_core::check_with_workers` will fire.
pub fn run(ctx: CmdContext, jobs: Option<usize>) -> Result<()> {
    let workers = jobs
        .map(Ok)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .or::<std::io::Error>(Ok(1))
        })
        .unwrap_or(1);
    let summary = gluon_core::check_with_workers(&ctx.ctx, &ctx.model, &ctx.resolved, workers)?;
    eprintln!("checked {}, cached {}", summary.built, summary.cached);
    Ok(())
}
