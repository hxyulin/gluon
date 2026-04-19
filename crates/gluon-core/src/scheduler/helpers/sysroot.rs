//! Scheduler-side sysroot helper.

use crate::compile::CompileCtx;
use crate::error::{Error, Result};
use crate::sysroot;
use gluon_model::{BuildModel, Handle, TargetDef};
use std::path::PathBuf;

/// Scheduler-side wrapper around [`sysroot::ensure_sysroot`]. Looks up
/// the target by handle and delegates. Lives in `scheduler::helpers`
/// rather than calling `ensure_sysroot` directly from the pipeline
/// dispatcher so that every per-node operation has a consistent "helper"
/// home — makes the `execute_pipeline` dispatch loop a flat match.
///
/// `stderr_sink` receives rustc's stderr (warnings) from each of the three
/// sysroot crate compiles; the worker pool flushes it to the user's
/// stderr atomically per job. The `_stdout` parameter remains unused —
/// the sysroot pipeline produces nothing of interest on stdout.
pub fn ensure_sysroot_for_node(
    ctx: &CompileCtx,
    model: &BuildModel,
    target: Handle<TargetDef>,
    _stdout: &mut Vec<u8>,
    stderr_sink: &mut Vec<u8>,
) -> Result<(PathBuf, bool)> {
    let target_def = model.targets.get(target).ok_or_else(|| {
        Error::Compile(format!(
            "scheduler: Sysroot node references target handle {target:?} not found in build model"
        ))
    })?;
    sysroot::ensure_sysroot(ctx, target_def, stderr_sink)
}
