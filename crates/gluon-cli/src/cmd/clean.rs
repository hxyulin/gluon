//! `gluon clean` subcommand — removes the build directory.

use super::CmdContext;
use anyhow::Result;

/// Run `gluon clean`, removing the build directory.
///
/// When `keep_sysroot` is `true`, the custom sysroot subtree is
/// preserved so the next build can skip re-materialising `core`,
/// `alloc`, and `compiler_builtins`.
pub fn run(ctx: CmdContext, keep_sysroot: bool) -> Result<()> {
    gluon_core::clean(&ctx.ctx.layout, keep_sysroot)?;
    Ok(())
}
