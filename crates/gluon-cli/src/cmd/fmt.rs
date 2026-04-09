//! `gluon fmt` subcommand — invoke `rustfmt` over every crate.
//!
//! Unlike `build`/`check`/`clippy`, this command takes a
//! [`LayoutContext`] (no rustc probe) because rustfmt has nothing to
//! do with rustc — finding it via `$RUSTFMT` or `$PATH` is enough.
//! That makes `gluon fmt` work on machines with a broken or missing
//! rustc, just like `gluon clean`.

use super::LayoutContext;
use anyhow::{Context, Result};

/// Run `rustfmt` over every crate in the build model.
///
/// `check_mode` mirrors `cargo fmt --check`: when set, rustfmt is
/// invoked with `--check` and the function returns a non-zero exit
/// (via `Err`) if any crate has unformatted files.
pub fn run(ctx: LayoutContext, check_mode: bool) -> Result<()> {
    let summary =
        gluon_core::fmt::run_fmt(&ctx.model, &ctx.resolved, &ctx.project_root, check_mode)
            .context("rustfmt invocation failed")?;

    eprintln!(
        "fmt: {} crate{} processed{}",
        summary.formatted,
        if summary.formatted == 1 { "" } else { "s" },
        if summary.skipped.is_empty() {
            String::new()
        } else {
            format!(", {} skipped (no .rs files)", summary.skipped.len())
        }
    );

    if !summary.unformatted.is_empty() {
        eprintln!("unformatted crates:");
        for name in &summary.unformatted {
            eprintln!("  - {name}");
        }
        // Match the exit-code semantics of `cargo fmt --check`: a
        // non-empty unformatted list is a non-zero exit so CI gates
        // can rely on it. Returning an anyhow Error here will cause
        // main.rs to print a (now somewhat redundant) "Error:" prefix
        // — that's still better than silently exiting 0 from a check
        // that found drift.
        return Err(anyhow::anyhow!(
            "{} crate(s) need formatting; run `gluon fmt` (without --check) to fix",
            summary.unformatted.len()
        ));
    }

    Ok(())
}
