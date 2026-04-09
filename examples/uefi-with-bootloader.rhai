// examples/uefi-with-bootloader.rhai
//
// UEFI project with a bootloader that embeds a kernel binary.
//
// This example shows the two-group pattern used when a UEFI bootloader
// needs to include_bytes! a bare-metal kernel ELF.  Gluon's
// `artifact_env` mechanism injects the kernel's output path into the
// bootloader's compile-time environment, and automatically adds a build
// dependency so the kernel compiles first.
//
//     gluon build
//     gluon run --uefi
//
// This file is illustrative — it is not a runnable project on its own.

// ── Project ──────────────────────────────────────────────────────────
// `.default_profile("debug")` selects the profile used when `gluon
// build` is invoked without `--profile`.
project("my-os", "0.1.0")
    .default_profile("debug");

// ── Targets ──────────────────────────────────────────────────────────
// Two targets: bare-metal for the kernel, UEFI for the bootloader.
// Both are built-in rustc targets so no JSON spec is needed.
target("x86_64-unknown-none");
target("x86_64-unknown-uefi");

// ── Profiles ─────────────────────────────────────────────────────────
// `.boot_binary("bootloader")` tells `gluon run` that the bootloader
// (not the kernel) is the artifact QEMU should boot.
profile("debug")
    .target("x86_64-unknown-uefi")
    .opt_level(0)
    .debug_info(true)
    .boot_binary("bootloader");

profile("release")
    .target("x86_64-unknown-uefi")
    .opt_level(2)
    .debug_info(false)
    .lto("thin")
    .boot_binary("bootloader");

// ── Kernel group ─────────────────────────────────────────────────────
// Compiled for bare-metal.  The linker script places the kernel at
// the address the bootloader expects.
group("kernel")
    .target("x86_64-unknown-none")
    .edition("2021")
    .add("kernel", "crates/kernel")
        .crate_type("bin")
        .root("src/main.rs")
        .linker_script("crates/kernel/kernel.ld");

// ── Bootloader group ─────────────────────────────────────────────────
// Compiled for UEFI.
//
// `.artifact_env("KERNEL_PATH", "kernel")` does two things:
//   1. Passes `--env KERNEL_PATH=<absolute path to kernel binary>` to
//      rustc so the bootloader can use:
//          static KERNEL: &[u8] = include_bytes!(env!("KERNEL_PATH"));
//   2. Auto-adds a build-order dependency: the kernel group compiles
//      before the bootloader group.
group("uefi")
    .target("x86_64-unknown-uefi")
    .edition("2021")
    .add("bootloader", "crates/bootloader")
        .crate_type("bin")
        .root("src/main.rs")
        .artifact_env("KERNEL_PATH", "kernel");

// ── ESP assembly ─────────────────────────────────────────────────────
// `esp("default")` declares an EFI System Partition layout.  After the
// bootloader compiles, Gluon copies its binary to the given path inside
// a staging directory.
//
// `gluon run --uefi` auto-detects this ESP and passes it to QEMU via
// the VVFAT driver (`-drive fat:rw:<esp_dir>`).
esp("default")
    .add("bootloader", "EFI/BOOT/BOOTX64.EFI");

// ── QEMU ─────────────────────────────────────────────────────────────
// `.boot_mode("uefi")` tells the runner to use OVMF firmware.  Gluon
// probes the `OVMF_CODE` and `OVMF_VARS` environment variables for
// firmware paths at launch time.
qemu("qemu-system-x86_64")
    .machine("q35")
    .memory(256)
    .cores(1)
    .serial_stdio()
    .boot_mode("uefi");
