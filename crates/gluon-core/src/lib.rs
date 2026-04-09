//! Gluon core library.
//!
//! Host-side build-system primitives consumed by the `gluon-cli` binary
//! and external embedders. Currently contains error/diagnostic types;
//! engine, scheduler, compile, cache, and sysroot modules will be added
//! in subsequent implementation chunks.

pub mod error;

pub use error::{Diagnostic, Error, Level, Result};

// Re-export the model crate for convenience — embedders can use either
// `gluon_core::model::BuildModel` or `gluon_model::BuildModel`.
pub use gluon_model as model;
