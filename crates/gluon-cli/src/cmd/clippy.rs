//! `gluon clippy` subcommand — runs the compile pipeline with the
//! clippy driver.
//!
//! Mirrors `cmd/check.rs` exactly except for two things:
//!
//! 1. The caller (in `main.rs`) constructs the `CmdContext` via
//!    `build_context_for_driver(..., DriverKind::Clippy)`. The layout
//!    routes user-crate output under `tool/clippy/` (so clippy
//!    artifacts cannot collide with build or check artifacts), and the
//!    rustc command builder swaps in `clippy-driver` as the program
//!    path while leaving every other flag identical.
//! 2. The summary verb is "linted" instead of "checked".
//!
//! Sysroot, generated config crate, and cache manifest are shared with
//! `gluon build` and `gluon check`.

use super::CmdContext;
use anyhow::Result;

/// Execute the clippy lint pipeline against the prepared context.
/// The context's layout must have been built with
/// `DriverKind::Clippy`; the lib-level `debug_assert_eq!` in
/// `gluon_core::clippy_with_workers` enforces this.
pub fn run(ctx: CmdContext, jobs: Option<usize>) -> Result<()> {
    let workers = jobs
        .map(Ok)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .or::<std::io::Error>(Ok(1))
        })
        .unwrap_or(1);
    let summary = gluon_core::clippy_with_workers(&ctx.ctx, &ctx.model, &ctx.resolved, workers)?;
    eprintln!("linted {}, cached {}", summary.built, summary.cached);
    Ok(())
}
