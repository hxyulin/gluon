//! Scheduler: DAG of build nodes + worker pool.
//!
//! [`dag`] defines [`DagNode`], [`Dag`], and [`build_dag`] — the types and
//! constructor that translate a [`ResolvedConfig`] + [`BuildModel`] into a
//! dependency graph.
//!
//! [`worker`] defines [`JobDispatcher`] and [`WorkerPool`] — the execution
//! engine that drives a [`Dag`] to completion, either single-threaded (`-j1`)
//! or multi-threaded.
//!
//! Chunk B4 will add `execute_pipeline`, the entry point that wires the
//! scheduler together with `ensure_sysroot`, `compile_crate`, and
//! `RuleRegistry::dispatch`.

pub mod dag;
pub mod worker;

pub use dag::{Dag, DagNode, NodeId, build_dag};
pub use worker::{JobDispatcher, WorkerPool};
