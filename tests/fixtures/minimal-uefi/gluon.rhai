// Minimal UEFI-flavored gluon fixture. Self-contained (duplicates the
// `minimal` fixture's tiny crate sources) so integration tests can
// copy this directory alone into a tempdir without a sibling
// dependency. Dry-run tests never compile anything, so source content
// is essentially ceremony.

project("minimal-uefi", "0.1.0");

target("x86_64-unknown-none");

profile("dev")
    .target("x86_64-unknown-none")
    .opt_level(0)
    .debug_info(true)
    .boot_binary("kernel");

group("host_tools")
    .target("host")
    .edition("2021")
    .add("minimal_derive", "crates/proc_macro")
        .crate_type("proc-macro")
        .root("src/lib.rs");

group("kernel")
    .target("x86_64-unknown-none")
    .edition("2021")
    .add("kernel", "crates/kernel")
        .crate_type("bin")
        .root("src/main.rs")
        .linker_script("crates/kernel/kernel.ld")
        .deps(#{ minimal_derive: #{ crate: "minimal_derive" } });

// UEFI boot mode: OVMF pflash + ESP directory mounted via VVFAT.
// OVMF paths are left unset here — tests inject them via the
// OVMF_CODE / OVMF_VARS env vars so the fixture stays hermetic.
qemu("qemu-system-x86_64")
    .machine("q35")
    .memory(512)
    .cores(2)
    .serial_stdio()
    .boot_mode("uefi")
    .esp_dir("./esp");
