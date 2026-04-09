//! `gluon run` — QEMU-based kernel launcher.
//!
//! This module owns the runtime side of `gluon run`: resolving a
//! [`gluon_model::QemuDef`] into a concrete QEMU invocation (with both
//! direct-kernel and UEFI boot modes), spawning the process, and
//! managing timeouts and serial forwarding.
//!
//! Layering:
//!
//! - [`qemu_cmd`] assembles the QEMU argv as a pure function. No
//!   filesystem or environment access — all inputs are explicit.
//! - [`ovmf`] probes for OVMF firmware files (explicit → env → system)
//!   and handles the writable-vars copy. Touches the filesystem.
//! - [`resolve`] merges `QemuDef` + profile-level overrides into a
//!   fully-defaulted [`resolve::ResolvedQemu`].
//! - This file wires those three together behind the public [`run`]
//!   entry point.
//!
//! The future `gluon test` command will reuse everything except
//! [`run`] itself: the test harness will call `build_qemu_command`
//! directly per test binary and interpret exit codes.

mod entry;
pub mod ovmf;
pub mod qemu_cmd;
pub mod resolve;

pub use entry::{RunOptions, run};
pub use ovmf::{ResolvedOvmf, resolve_ovmf};
pub use qemu_cmd::{QemuInvocation, build_qemu_command};
pub use resolve::{ResolvedQemu, resolve_qemu};
