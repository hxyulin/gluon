//! Compile primitives used by later pipeline stages.
//!
//! This module exposes the building blocks the rest of gluon's pipeline
//! funnels through:
//!
//! - [`BuildLayout`] — pure path arithmetic for every `build/...` directory
//!   gluon touches. All other modules must route through this type rather
//!   than hardcoding relative `build/...` strings.
//! - [`RustcInfo`] — cached host `rustc` metadata, probed once per run and
//!   persisted to disk, invalidated by the rustc binary's mtime.
//! - [`RustcCommandBuilder`] / [`Emit`] — the single point in gluon that
//!   assembles a rustc invocation, producing both a `Command` and a stable
//!   cache-key hash.
//! - [`CompileCtx`] — the triple of layout + rustc info + cache that every
//!   compile step (sysroot, host crates, cross crates) threads through.

pub mod layout;
pub mod rustc;
pub mod rustc_info;

pub use layout::BuildLayout;
pub use rustc::{Emit, RustcCommandBuilder};
pub use rustc_info::RustcInfo;

use crate::cache::Cache;
use std::sync::{Arc, Mutex};

/// The shared context threaded through every compile step.
///
/// Holds:
/// - a [`BuildLayout`] for path arithmetic,
/// - an `Arc<RustcInfo>` so the probed host-toolchain metadata can be
///   cheaply cloned into worker threads without reprobing,
/// - a [`Cache`] wrapped in a `Mutex` for mutation under shared references.
///
/// ### Why `Mutex` rather than `RefCell`
///
/// `RefCell` would be sufficient for the single-threaded A4 callers, but
/// session B will share `CompileCtx` across worker threads from the
/// scheduler. Introducing the right synchronisation primitive here avoids
/// a later rework and, in the single-threaded case, the mutex is
/// effectively free (an uncontended lock-acquire).
///
/// Callers that need to mutate the cache should acquire the lock as
/// narrowly as possible — in particular, **never hold the cache lock
/// across a rustc spawn**, or parallel workers will serialise on it.
pub struct CompileCtx {
    pub layout: BuildLayout,
    pub rustc_info: Arc<RustcInfo>,
    pub cache: Mutex<Cache>,
}

impl CompileCtx {
    pub fn new(layout: BuildLayout, rustc_info: Arc<RustcInfo>, cache: Cache) -> Self {
        Self {
            layout,
            rustc_info,
            cache: Mutex::new(cache),
        }
    }
}
