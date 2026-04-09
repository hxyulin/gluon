//! Per-node work helpers for the pipeline dispatcher.
//!
//! Each submodule handles one `DagNode` variant:
//!
//! - [`sysroot`] — wraps [`crate::sysroot::ensure_sysroot`] for the DAG-node case.
//! - [`config_crate`] — generates + compiles the `<project>_config` rlib.
//! - [`esp`] — assembles an EFI System Partition directory from built artifacts.
//!
//! Separating the helpers from the dispatch loop in `execute_pipeline` keeps
//! the top-level match flat and each unit of work independently testable.

pub mod config_crate;
pub mod esp;
pub mod sysroot;
