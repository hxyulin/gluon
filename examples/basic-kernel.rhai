// examples/basic-kernel.rhai
//
// Minimal bare-metal kernel project.
//
// This is the simplest useful gluon.rhai: one target, one profile, one
// crate compiled into a kernel binary.  Run with:
//
//     gluon build
//     gluon run
//
// This file is illustrative — it is not a runnable project on its own
// (no Rust source or linker script is provided).  Its purpose is to
// document the configuration API.

// ── Project declaration ──────────────────────────────────────────────
// Every gluon.rhai starts with a project name and version.
project("my-kernel", "0.1.0");

// ── Target ───────────────────────────────────────────────────────────
// Declare the target triple the kernel will be compiled for.
//
// The optional second argument is a path (relative to this file) to a
// custom target-spec JSON file.  Omit it when the target is a built-in
// rustc target or is provided by rustup (e.g. x86_64-unknown-none).
target("x86_64-unknown-none");

// ── Profile ──────────────────────────────────────────────────────────
// Profiles control optimisation level, debug info, LTO, and which
// target to build for.  `.boot_binary()` tells `gluon run` which crate
// is the entry-point binary for QEMU.
profile("dev")
    .target("x86_64-unknown-none")
    .opt_level(0)
    .debug_info(true)
    .boot_binary("kernel");

// ── Group + crate ────────────────────────────────────────────────────
// A group collects crates that share a target and edition.  The target
// set here overrides the profile-level default for every crate in the
// group.
//
// `.edition("2021")` sets the Rust edition for all crates in the group.
//
// `.add(name, path)` registers a crate:
//   - `name`  — the rustc crate name (underscores, not dashes).
//   - `path`  — directory relative to the project root.
//
// Chained methods on the crate:
//   .crate_type("bin")             — binary (default is lib)
//   .root("src/main.rs")          — entry-point file
//   .linker_script("kernel.ld")   — linker script path (relative to project root)
group("kernel")
    .target("x86_64-unknown-none")
    .edition("2021")
    .add("kernel", "crates/kernel")
        .crate_type("bin")
        .root("src/main.rs")
        .linker_script("crates/kernel/kernel.ld");

// ── QEMU ─────────────────────────────────────────────────────────────
// Configure `gluon run` to launch QEMU.  The first argument is the
// QEMU executable (must be on $PATH or an absolute path).
//
//   .machine("q35")     — QEMU machine type
//   .memory(128)        — RAM in MiB
//   .cores(1)           — number of virtual CPUs
//   .serial_stdio()     — redirect serial port to the terminal
qemu("qemu-system-x86_64")
    .machine("q35")
    .memory(128)
    .cores(1)
    .serial_stdio();
