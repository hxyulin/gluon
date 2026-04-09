//! Test-only fake `qemu-system-*` binary.
//!
//! This is a tiny stand-in for QEMU, used by `gluon-cli`'s integration
//! tests to exercise the runner's spawn path without needing a real
//! QEMU install on the host. Cargo automatically exposes the built
//! path to integration tests as `CARGO_BIN_EXE_fake-qemu` (the dash
//! is preserved in the env var).
//!
//! Behaviour:
//!
//! - If `FAKE_QEMU_ARGV_FILE` is set, every command-line argument
//!   (excluding `argv[0]`) is written to that file, one per line.
//!   This lets tests round-trip the assembled QEMU argv and assert
//!   on its contents — useful for items #5 (`--gdb`) and #6
//!   (`-device isa-debug-exit`) of the polish pass, which both
//!   change the argv but not the gluon side-effects.
//! - If `FAKE_QEMU_EXIT_CODE` is set to a parseable `i32`, the
//!   process exits with that code. This lets tests simulate QEMU
//!   failure paths without actually crashing anything.
//! - Otherwise the process exits 0.
//!
//! Kept dependency-free on purpose — adding clap or anyhow to a
//! test-only shim is pure overhead.

use std::env;
use std::fs;
use std::io::Write;
use std::process::ExitCode;

fn main() -> ExitCode {
    if let Ok(path) = env::var("FAKE_QEMU_ARGV_FILE") {
        // Best-effort: failing to record argv must not block the test.
        // If the test cares about the argv, it will notice the missing
        // file and fail with a clearer message than we could produce
        // from inside the shim.
        if let Ok(mut f) = fs::File::create(&path) {
            for arg in env::args().skip(1) {
                let _ = writeln!(f, "{arg}");
            }
        }
    }

    let code: i32 = env::var("FAKE_QEMU_EXIT_CODE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    // ExitCode only takes u8 directly; non-zero codes from envs are
    // clamped into a u8 the same way std::process::exit would
    // truncate them on Unix.
    ExitCode::from(code as u8)
}
