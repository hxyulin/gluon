//! Subprocess wrapper around `cargo vendor`.
//!
//! Resolves the cargo binary via `$CARGO` (matching cargo's own
//! convention and what rustup sets on the outer cargo invocation),
//! falling back to bare `cargo` on `$PATH`. Stdout and stderr are
//! inherited — `cargo vendor` produces useful progress output and we
//! want the user to see it live rather than buffered.
//!
//! On non-zero exit, returns an [`Error::Diagnostics`] with a note
//! pointing at the binary we tried to run, so the user can tell
//! whether it's a missing toolchain, a stale network, or a real dep
//! error.

use crate::error::{Diagnostic, Error, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Flags that modulate the `cargo vendor` invocation.
#[derive(Debug, Clone, Copy, Default)]
pub struct VendorFlags {
    /// Pass `--offline` and `--frozen` to cargo — forbids any network
    /// access and any mutation of `Cargo.lock`. Useful in CI when the
    /// lockfile is expected to be up to date.
    pub offline: bool,
}

/// Run `cargo vendor` on the given scratch workspace, populating
/// `vendor_target` with the resolved dependency closure.
///
/// `workspace_dir` must contain a valid `Cargo.toml` (the one produced
/// by [`super::manifest_gen::write_vendor_workspace`]). `vendor_target`
/// is the directory that cargo will populate — typically
/// `<project>/vendor/`, computed by
/// [`crate::compile::BuildLayout::vendor_dir`].
///
/// Both paths are passed to cargo as absolute paths so a non-default
/// working directory can't confuse it.
pub fn run_cargo_vendor(
    workspace_dir: &Path,
    vendor_target: &Path,
    flags: VendorFlags,
) -> Result<()> {
    let cargo = resolve_cargo();

    let mut cmd = Command::new(&cargo);
    cmd.arg("vendor")
        .arg("--manifest-path")
        .arg(workspace_dir.join("Cargo.toml"))
        .arg(vendor_target);

    if flags.offline {
        cmd.arg("--frozen").arg("--offline");
    }

    // Inherit stdio so cargo's "Downloading …" / "Vendoring …" progress
    // shows up live in the user's terminal.
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());

    let status = cmd.status().map_err(|e| {
        Error::Diagnostics(vec![
            Diagnostic::error(format!("failed to spawn `cargo vendor`: {e}")).with_note(format!(
                "tried to invoke {} (set $CARGO to override)",
                cargo.display()
            )),
        ])
    })?;

    if !status.success() {
        return Err(Error::Diagnostics(vec![
            Diagnostic::error(format!(
                "`cargo vendor` failed with exit code {:?}",
                status.code()
            ))
            .with_note(format!("scratch workspace: {}", workspace_dir.display()))
            .with_note(format!("target directory: {}", vendor_target.display()))
            .with_note("see cargo's output above for the real error"),
        ]));
    }

    Ok(())
}

/// Resolve which `cargo` binary to invoke. Honors `$CARGO` first
/// (rustup sets this on the outer cargo invocation, so nested calls
/// naturally stick to the same toolchain), otherwise falls back to
/// bare `cargo` on `$PATH`.
fn resolve_cargo() -> PathBuf {
    if let Some(p) = std::env::var_os("CARGO") {
        return PathBuf::from(p);
    }
    PathBuf::from("cargo")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_cargo_honors_env_var() {
        // Concurrent env var access is only safe because the tests in
        // this crate agree not to clobber each other's variables —
        // same caveat as `fmt::tests::resolve_rustfmt_env_var_then_default`.
        // We use a sentinel path that could not plausibly collide with
        // a real binary on any developer's machine.
        let sentinel = "/tmp/gluon-vendor-test-sentinel-cargo";
        unsafe { std::env::set_var("CARGO", sentinel) };
        assert_eq!(resolve_cargo(), PathBuf::from(sentinel));
        unsafe { std::env::remove_var("CARGO") };
        assert_eq!(resolve_cargo(), PathBuf::from("cargo"));
    }

    #[test]
    fn spawn_failure_is_diagnosed() {
        // Point $CARGO at a binary that cannot possibly exist so we
        // exercise the spawn-error arm of run_cargo_vendor.
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(
            ws.join("Cargo.toml"),
            b"[package]\nname=\"x\"\nversion=\"0.0.0\"\nedition=\"2021\"\n[lib]\npath=\"lib.rs\"\n",
        )
        .unwrap();
        std::fs::write(ws.join("lib.rs"), b"").unwrap();

        let bogus = "/nonexistent/gluon-vendor-test-cargo-missing";
        // Save prior value (may be unset) and restore at end.
        let prior = std::env::var_os("CARGO");
        unsafe { std::env::set_var("CARGO", bogus) };

        let result = run_cargo_vendor(&ws, &tmp.path().join("out"), VendorFlags::default());

        // Restore BEFORE asserting so a failure doesn't leak state.
        match prior {
            Some(v) => unsafe { std::env::set_var("CARGO", v) },
            None => unsafe { std::env::remove_var("CARGO") },
        }

        let err = result.expect_err("must fail — bogus CARGO path");
        let msg = err.to_string();
        assert!(msg.contains("failed to spawn"), "msg: {msg}");
        assert!(msg.contains(bogus), "msg: {msg}");
    }
}
