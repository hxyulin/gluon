//! `gluon clean` subcommand — removes the build directory.

use super::LayoutContext;
use anyhow::Result;

/// Run `gluon clean`, removing the build directory.
///
/// When `keep_sysroot` is `true`, the custom sysroot subtree is
/// preserved so the next build can skip re-materialising `core`,
/// `alloc`, and `compiler_builtins`.
///
/// Takes a [`LayoutContext`] rather than a full [`super::CmdContext`]
/// so it runs without probing rustc. That matters because `clean` is
/// the subcommand users reach for when their toolchain is broken,
/// and a mandatory rustc probe would make the command useless in
/// exactly that situation.
pub fn run(ctx: LayoutContext, keep_sysroot: bool) -> Result<()> {
    gluon_core::clean(&ctx.layout, keep_sysroot)?;
    Ok(())
}
