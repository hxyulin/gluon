//! Compile primitives used by later pipeline stages.
//!
//! This module currently exposes two building blocks:
//!
//! - [`BuildLayout`] — pure path arithmetic for every `build/...` directory
//!   gluon touches. All other modules must route through this type rather
//!   than hardcoding relative `build/...` strings.
//! - [`RustcInfo`] — cached host `rustc` metadata, probed once per run and
//!   persisted to disk, invalidated by the rustc binary's mtime.
//!
//! The `CompileCtx` that threads these together lands in a later chunk.

pub mod layout;
pub mod rustc_info;

pub use layout::BuildLayout;
pub use rustc_info::RustcInfo;
