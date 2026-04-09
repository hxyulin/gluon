//! Gluon core library.
//!
//! Host-side build-system primitives consumed by the `gluon-cli` binary
//! and external embedders. Currently contains error/diagnostic types;
//! engine, scheduler, compile, cache, and sysroot modules will be added
//! in subsequent implementation chunks.

pub mod cache;
pub mod compile;
pub mod config;
pub mod engine;
pub mod error;
pub mod rule;
pub mod scheduler;
pub mod sysroot;

pub use cache::{BuildRecord, Cache, CacheManifest, FreshnessQuery};
pub use compile::{
    ArtifactMap, BuildLayout, CompileCrateInput, CompileCtx, Emit, RustcCommandBuilder, RustcInfo,
    compile_crate,
};
pub use config::resolve;
pub use engine::evaluate_script;
pub use error::{Diagnostic, Error, Level, Result};
pub use rule::builtin::ExecRule;
pub use rule::{RuleCtx, RuleFn, RuleRegistry};
pub use scheduler::{Dag, DagNode, JobDispatcher, NodeId, WorkerPool, build_dag, execute_pipeline};
pub use sysroot::ensure_sysroot;

// Re-export the model crate for convenience — embedders can use either
// `gluon_core::model::BuildModel` or `gluon_model::BuildModel`.
pub use gluon_model as model;

/// Top-level build entry point. Builds the DAG, runs the scheduler,
/// and persists the cache manifest on success. Uses the default set of
/// built-in rules (`RuleRegistry::with_builtins`) and the host's
/// parallelism from `std::thread::available_parallelism` (fallback 1).
pub fn build(
    ctx: &CompileCtx,
    model: &gluon_model::BuildModel,
    resolved: &gluon_model::ResolvedConfig,
) -> Result<()> {
    let rules = rule::RuleRegistry::with_builtins();
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    scheduler::execute_pipeline(
        ctx,
        model,
        resolved,
        &rules,
        workers,
        &mut stdout,
        &mut stderr,
    )?;
    // Persist the cache on success so the next run benefits from this build's
    // freshness records. On failure we intentionally skip the save — a partial
    // build's cache entries are written eagerly inside the per-node helpers, so
    // any nodes that succeeded are already recorded.
    ctx.cache
        .lock()
        .map_err(|_| Error::Config("cache mutex poisoned".into()))?
        .save()?;
    Ok(())
}
