//! `gluon vendor` subcommand — thin CLI wrapper over
//! [`gluon_core::vendor::vendor_sync`] and
//! [`gluon_core::vendor::vendor_check`].
//!
//! The heavy lifting lives in `gluon_core::vendor`; this file is
//! responsible for:
//!
//! - routing `--check` / `--force` / `--offline` into the right core
//!   entry point,
//! - printing human-friendly progress and summary lines,
//! - translating a non-clean [`gluon_core::vendor::VendorCheckReport`]
//!   into a non-zero process exit code via
//!   [`std::process::exit`].
//!
//! Uses the lighter-weight [`LayoutContext`] (no rustc probe) because
//! vendoring does not need the toolchain. The caller in `main.rs`
//! constructs the context with
//! [`super::LayoutContextOpts::skip_vendor_autoreg`] set to `true` —
//! see that call site for why.

use super::LayoutContext;
use anyhow::Result;
use gluon_core::vendor::{self, VendorOptions};

/// Entry point called from `main.rs`.
///
/// `check_mode` mirrors `gluon vendor --check`: verify-only, no
/// disk mutation. `force` bypasses the fingerprint fast path.
/// `offline` passes `--offline`/`--frozen` through to cargo.
pub fn run(ctx: LayoutContext, check_mode: bool, force: bool, offline: bool) -> Result<()> {
    let LayoutContext {
        model,
        resolved: _, // vendor doesn't need the resolved profile
        layout,
        project_root,
    } = ctx;

    if check_mode {
        let report = vendor::vendor_check(&model, &layout, &project_root)?;
        if report.is_clean() {
            println!("vendor state is up to date");
            return Ok(());
        }
        // Non-clean: print every diagnostic and exit 1. We use
        // `process::exit` rather than returning an error so the CLI
        // doesn't double-print via the top-level `Error:` prefix.
        for diag in report.to_diagnostics() {
            eprintln!("{diag}");
        }
        std::process::exit(1);
    }

    let opts = VendorOptions { force, offline };
    let lock = vendor::vendor_sync(&model, &layout, &project_root, opts)?;
    if lock.packages.is_empty() {
        println!("no external dependencies declared — wrote empty gluon.lock");
    } else {
        println!(
            "vendored {} crate(s); see {}",
            lock.packages.len(),
            layout.vendor_dir(&project_root).display()
        );
    }
    Ok(())
}
