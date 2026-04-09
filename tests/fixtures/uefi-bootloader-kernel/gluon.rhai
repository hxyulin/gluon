// End-to-end fixture: x86_64 UEFI bootloader with an embedded kernel.
//
// This fixture exercises the full cross-target build chain that none of
// the other fixtures cover:
//
//   - Two targets in one project (`x86_64-unknown-uefi` for the
//     bootloader, `x86_64-unknown-none` for the kernel).
//   - Two build groups, each pinning its own target.
//   - `artifact_env` injection: the bootloader crate receives
//     `KERNEL_PATH` at rustc invocation time and uses `include_bytes!`
//     to embed the compiled kernel ELF.
//   - Build ordering across groups: the kernel must compile first
//     (enforced by the auto-populated `artifact_deps` edge that
//     `.artifact_env(...)` adds).
//   - Auto-assembled ESP: `esp("default")` declares which artifact
//     lands where inside a FAT directory; the `EspBuild` scheduler
//     node populates it from the built bootloader binary.
//   - `gluon run --uefi` auto-wires the assembled ESP into QEMU's
//     `-drive fat:rw:` flag, so the user runs it without any manual
//     file copying.
//
// The bootloader deliberately does NOT hand off to the kernel at
// runtime (no ELF parsing, no exit-boot-services, no long-mode
// transition). The fixture's job is to prove the build pipeline
// works end-to-end; a real handoff would balloon this into a hobby
// OS. Runtime verification is via a single byte written to QEMU's
// debugcon port — see crates/bootloader/src/main.rs.

project("uefi-bootloader-kernel", "0.1.0");

target("x86_64-unknown-none");
target("x86_64-unknown-uefi");

profile("dev")
    .target("x86_64-unknown-uefi")
    .opt_level(0)
    .debug_info(true)
    .boot_binary("bootloader");

// Kernel is compiled for a bare-metal target with a custom linker
// script. The bootloader will `include_bytes!` its output.
group("kernel")
    .target("x86_64-unknown-none")
    .edition("2021")
    .add("kernel", "crates/kernel")
        .crate_type("bin")
        .root("src/main.rs")
        .linker_script("crates/kernel/kernel.ld");

// Bootloader is a UEFI application. `artifact_env("KERNEL_PATH",
// "kernel")` injects the absolute path of the kernel's build output
// into rustc's environment at spawn time, and auto-adds an
// `artifact_deps` edge so the DAG compiles the kernel first.
group("uefi")
    .target("x86_64-unknown-uefi")
    .edition("2021")
    .add("bootloader", "crates/bootloader")
        .crate_type("bin")
        .root("src/main.rs")
        .artifact_env("KERNEL_PATH", "kernel");

// Auto-populated ESP directory. The build pipeline will copy the
// compiled bootloader to EFI/BOOT/BOOTX64.EFI after it compiles,
// and `gluon run --uefi` will mount this directory as the ESP via
// QEMU's VVFAT driver.
esp("default")
    .add("bootloader", "EFI/BOOT/BOOTX64.EFI");

// QEMU config. OVMF paths are intentionally omitted — the runner
// probes OVMF_CODE / OVMF_VARS env vars at spawn time so the fixture
// stays hermetic. Tests that actually spawn QEMU (gated on
// GLUON_TEST_QEMU=1) inject the OVMF paths themselves.
//
// Note: no `.esp_dir(...)` — Step 5's auto-wire logic picks up the
// single `esp("default")` declaration above.
qemu("qemu-system-x86_64")
    .machine("q35")
    .memory(256)
    .cores(1)
    .serial_stdio()
    .boot_mode("uefi");
