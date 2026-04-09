//! Pure data types for the Gluon build system.
//!
//! This crate is `serde`-only. No other dependencies. It is safe for
//! embedders (e.g., future menuconfig binary, LSP plugin, CI validator)
//! to depend on without dragging in the build engine.

pub mod build_model;
pub mod handle;
pub mod kconfig;
pub mod resolved;
pub mod source;

pub use build_model::*;
pub use handle::{Arena, Handle};
pub use kconfig::*;
pub use resolved::*;
pub use source::SourceSpan;
