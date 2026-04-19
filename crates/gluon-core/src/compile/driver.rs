//! Driver kind selection for the compile pipeline.
//!
//! Gluon's per-crate flag assembly in [`super::rustc::RustcCommandBuilder`]
//! is binary-agnostic — it accepts any `PathBuf` as the program to spawn
//! and emits its arguments in a fixed, cache-keyed order. That makes it
//! cheap to repurpose the same pipeline for `gluon check` and
//! `gluon clippy` by swapping which binary the builder targets and which
//! `--emit` flags it produces.
//!
//! [`DriverKind`] enumerates the supported drivers. Each variant knows:
//!
//! 1. **Which program to invoke.** For `Rustc` and `Check` that's just
//!    the configured rustc path; for `Clippy` it's `clippy-driver`,
//!    found via `$CLIPPY_DRIVER`, then a sibling-of-rustc heuristic,
//!    then plain `clippy-driver` on `$PATH`.
//! 2. **Which `--emit` kinds to produce.** `Rustc` runs the full normal
//!    build pipeline. `Check` and `Clippy` only need metadata; suppressing
//!    codegen reuses the same dependency graph but skips the slow parts
//!    (LLVM, link), giving us `cargo check` semantics for free.
//!
//! The compile pipeline (chunk T2) parameterizes
//! [`super::compile_crate::compile`] on a `DriverKind` so the existing
//! `gluon build` flow becomes `DriverKind::Rustc` and the new
//! `gluon check` / `gluon clippy` flows reuse every line of flag
//! assembly without forking the codepath.

use super::{Emit, RustcInfo};
use std::path::PathBuf;

/// Which compiler driver the pipeline should invoke for this run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverKind {
    /// Normal rustc build. Produces full codegen output (rlib, bin, etc.)
    /// matching whatever `--emit` kinds the compile step requested.
    /// This is the default — `gluon build` uses it.
    Rustc,
    /// `cargo check`-equivalent: invoke rustc but suppress codegen by
    /// emitting only `metadata`. Used by `gluon check`.
    Check,
    /// Clippy lint pass: swap the binary path to `clippy-driver` (which
    /// is rustc-CLI-compatible) and run with `--emit=metadata`. Used by
    /// `gluon clippy`.
    Clippy,
}

impl DriverKind {
    /// Resolve the binary path that this driver should spawn.
    ///
    /// `Rustc` and `Check` both use the configured rustc binary
    /// (`RustcInfo::rustc_path`, which honours `$RUSTC`). `Clippy`
    /// follows the same precedence as cargo's clippy: explicit
    /// `$CLIPPY_DRIVER` first, then a `clippy-driver` binary in the
    /// same directory as the resolved rustc, then bare `clippy-driver`
    /// on `$PATH`. The bare-name fallback relies on the OS to perform
    /// `$PATH` lookup at spawn time.
    pub fn program(self, rustc_info: &RustcInfo) -> PathBuf {
        match self {
            DriverKind::Rustc | DriverKind::Check => rustc_info.rustc_path.clone(),
            DriverKind::Clippy => resolve_clippy_driver(rustc_info),
        }
    }

    /// Returns `Some(slice)` if this driver overrides the requested
    /// `--emit` kinds (forcing metadata-only + dep-info), or `None` to
    /// use the per-step kinds the caller specified.
    ///
    /// Note: returning `None` for `Rustc` matters — the build pipeline
    /// requests different emit kinds per crate (rlib for libs, link for
    /// bins, etc.), and the driver must not stomp on that. Returning a
    /// fixed slice for `Check`/`Clippy` collapses every step to
    /// metadata-only, which is exactly what we want.
    ///
    /// `Emit::DepInfo` is included even on the metadata-only paths
    /// because the cache freshness logic in
    /// [`super::compile_crate::compile`] reads the depfile to track
    /// source dependencies. Suppressing dep-info would force every
    /// `gluon check` invocation to re-run rustc on every crate. Rustc
    /// produces accurate dep-info even when codegen is suppressed, so
    /// keeping it has no observable cost.
    pub fn emit_override(self) -> Option<&'static [Emit]> {
        match self {
            DriverKind::Rustc => None,
            DriverKind::Check | DriverKind::Clippy => Some(&[Emit::Metadata, Emit::DepInfo]),
        }
    }

    /// Human-readable name used in CLI summary lines like
    /// "checked N, cached M".
    pub fn verb_past(self) -> &'static str {
        match self {
            DriverKind::Rustc => "built",
            DriverKind::Check => "checked",
            DriverKind::Clippy => "linted",
        }
    }
}

fn resolve_clippy_driver(rustc_info: &RustcInfo) -> PathBuf {
    // 1. Explicit override via env var (matches cargo's behavior).
    if let Some(p) = std::env::var_os("CLIPPY_DRIVER") {
        return PathBuf::from(p);
    }

    // 2. Sibling of the resolved rustc binary. With rustup, both
    //    `rustc` and `clippy-driver` live in the same toolchain bin
    //    directory, so this is the common case.
    if let Some(parent) = rustc_info.rustc_path.parent() {
        let candidate = parent.join(driver_filename());
        if candidate.is_file() {
            return candidate;
        }
    }

    // 3. Bare name; rely on $PATH lookup at spawn time.
    PathBuf::from(driver_filename())
}

#[cfg(windows)]
fn driver_filename() -> &'static str {
    "clippy-driver.exe"
}

#[cfg(not(windows))]
fn driver_filename() -> &'static str {
    "clippy-driver"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rustc_and_check_have_no_emit_override() {
        // Rustc must let the per-crate emit selection through.
        assert!(DriverKind::Rustc.emit_override().is_none());
    }

    #[test]
    fn check_and_clippy_force_metadata_plus_depinfo_emit() {
        // DepInfo is required for the cache freshness path; see
        // emit_override doc for the rationale.
        let check = DriverKind::Check.emit_override().unwrap();
        let clippy = DriverKind::Clippy.emit_override().unwrap();
        assert_eq!(check, &[Emit::Metadata, Emit::DepInfo]);
        assert_eq!(clippy, &[Emit::Metadata, Emit::DepInfo]);
    }

    #[test]
    fn verb_past_is_distinct_per_kind() {
        // Used in CLI summary output; each driver should produce a
        // distinct verb so users can tell what just ran.
        assert_eq!(DriverKind::Rustc.verb_past(), "built");
        assert_eq!(DriverKind::Check.verb_past(), "checked");
        assert_eq!(DriverKind::Clippy.verb_past(), "linted");
    }

    #[test]
    fn clippy_program_honors_env_override() {
        // `temp_env::with_var` serializes env mutation behind a global
        // mutex and restores the prior value on guard drop, so this is
        // safe even under cargo's parallel test runner.
        let sentinel = "/definitely/not/a/real/clippy-driver-sentinel";
        temp_env::with_var("CLIPPY_DRIVER", Some(sentinel), || {
            let info = RustcInfo {
                rustc_path: PathBuf::from("/usr/bin/rustc"),
                rustc_arg: "rustc".into(),
                version: "rustc 1.80.0 (test)".into(),
                host_triple: "x86_64-unknown-linux-gnu".into(),
                commit_hash: None,
                release: "1.80.0".into(),
                sysroot: PathBuf::from("/usr/lib"),
                rust_src: None,
                mtime_ns: 0,
            };
            let p = DriverKind::Clippy.program(&info);
            assert_eq!(p, PathBuf::from(sentinel));
        });
    }
}
