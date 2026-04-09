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
pub mod sysroot;

pub use cache::{BuildRecord, Cache, CacheManifest, FreshnessQuery};
pub use compile::{
    ArtifactMap, BuildLayout, CompileCrateInput, CompileCtx, Emit, RustcCommandBuilder, RustcInfo,
    compile_crate,
};
pub use config::resolve;
pub use engine::evaluate_script;
pub use error::{Diagnostic, Error, Level, Result};
pub use sysroot::ensure_sysroot;

// Re-export the model crate for convenience — embedders can use either
// `gluon_core::model::BuildModel` or `gluon_model::BuildModel`.
pub use gluon_model as model;
