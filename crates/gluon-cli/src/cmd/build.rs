//! `gluon build` subcommand — runs the full compile pipeline.

use super::CmdContext;
use anyhow::Result;

/// Execute the build pipeline against the prepared context.
///
/// When `jobs` is `Some(n)`, dispatches to
/// [`gluon_core::build_with_workers`] with exactly `n` workers. When
/// `None`, falls back to [`gluon_core::build`], which probes the host
/// for parallelism. The CLI layer (`cli::parse_jobs`) has already
/// rejected `-j 0`, so any `Some(n)` here is guaranteed `n >= 1`.
pub fn run(ctx: CmdContext, jobs: Option<usize>) -> Result<()> {
    let summary = match jobs {
        Some(n) => gluon_core::build_with_workers(&ctx.ctx, &ctx.model, &ctx.resolved, n)?,
        None => gluon_core::build(&ctx.ctx, &ctx.model, &ctx.resolved)?,
    };
    eprintln!("built {}, cached {}", summary.built, summary.cached);
    Ok(())
}
