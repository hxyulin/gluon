//! `gluon build` subcommand — runs the full compile pipeline.

use super::CmdContext;
use anyhow::Result;

/// Execute the build pipeline against the prepared context.
///
/// The `jobs` parameter is parsed from the CLI so `-j N` doesn't error,
/// but it is currently inert: `gluon_core::build` always uses
/// `std::thread::available_parallelism`. Plumbing `-j` through requires
/// a `build_with_workers(...)` variant in gluon-core; that's intentionally
/// deferred until a later chunk so this one stays scoped to CLI glue.
pub fn run(ctx: CmdContext, jobs: Option<usize>) -> Result<()> {
    // TODO: honour `jobs` once gluon_core exposes a worker-count override.
    let _ = jobs;
    let summary = gluon_core::build(&ctx.ctx, &ctx.model, &ctx.resolved)?;
    eprintln!("built {}, cached {}", summary.built, summary.cached);
    Ok(())
}
