//! `gluon configure` subcommand — emits `rust-project.json` for rust-analyzer.

use super::CmdContext;
use anyhow::Result;
use std::path::PathBuf;

/// Run `gluon configure`, writing `rust-project.json` to disk.
///
/// When `output` is `None`, the file is written to
/// `<project_root>/rust-project.json` (the conventional location
/// rust-analyzer discovers automatically).
pub fn run(ctx: CmdContext, output: Option<PathBuf>) -> Result<()> {
    gluon_core::configure(&ctx.ctx, &ctx.model, &ctx.resolved, output.as_deref())?;
    Ok(())
}
