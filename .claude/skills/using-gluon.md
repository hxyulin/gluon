---
name: using-gluon
description: Use when writing or modifying gluon.rhai configuration files, running gluon CLI commands (build, run, check, clippy, fmt, clean, configure, vendor), setting up a new bare-metal Rust project with gluon, or troubleshooting gluon build issues. Covers the Rhai DSL API, CLI flags, config override precedence, QEMU orchestration, and UEFI boot setup.
---

# Using Gluon

Gluon is a build system for bare-metal Rust kernels. It bypasses Cargo, drives `rustc` directly, compiles custom sysroots, and orchestrates compilation through to bootable artifacts and QEMU runs. Configuration is declared in a `gluon.rhai` script.

## CLI Quick Reference

| Command | Purpose |
|---------|---------|
| `gluon build` | Compile all crates |
| `gluon check` | Type-check only (like cargo check) |
| `gluon clippy` | Lint with clippy-driver |
| `gluon fmt [--check]` | Format with rustfmt |
| `gluon clean [--keep-sysroot]` | Delete build/ (optionally keep sysroot) |
| `gluon configure` | Generate rust-project.json for rust-analyzer |
| `gluon vendor [--check\|--force\|--offline]` | Vendor dependencies, write gluon.lock |
| `gluon run` | Build + launch in QEMU |

**Global flags:** `-p <profile>`, `-t <target>`, `-j <jobs>`, `-C <config-file>`, `-v`, `-q`

**Run flags:** `--uefi`, `--direct`, `-T <secs>` (timeout), `--gdb` (halt + GDB on :1234), `--no-build`, `--dry-run`, `-- <qemu-args>`

## Rhai DSL API

### Project & Target

```rhai
project("name", "0.1.0")
    .default_profile("dev")
    .config_crate_name("my_config");  // default: <project>_config

target("x86_64-unknown-none");              // built-in triple
target("my-target", "my-target-spec.json"); // custom spec
    // optional: .panic_strategy("abort")
```

### Profiles

```rhai
profile("dev")
    .target("x86_64-unknown-none")
    .opt_level(0)           // 0|1|2|3
    .debug_info(true)
    .lto("thin")            // "thin"|"fat"|"off"
    .boot_binary("kernel")  // entry point for gluon run
    .inherits("base")       // inherit from another profile
    .preset("verbose")      // apply named config preset
    .config("LOG_LEVEL", 5) // override config option
    // QEMU overrides per profile:
    .qemu_memory(512)
    .qemu_cores(4)
    .qemu_extra_args(["-display", "none"])
    .test_timeout(60);
```

### Groups & Crates

```rhai
group("kernel")
    .target("x86_64-unknown-none")
    .edition("2021")
    .is_project(true)   // affects clippy
    .config(true)        // link with generated config crate
    .shared_flags(["-Cforce-frame-pointers=yes"])

    .add("kernel", "crates/kernel")
        .crate_type("bin")       // "bin"|"lib"|"proc-macro"|"staticlib"
        .root("src/main.rs")
        .linker_script("crates/kernel/kernel.ld")
        .edition("2024")         // override group edition
        .target("other-target")  // override group target
        .features(["alloc"])
        .cfg_flags(["my_flag"])
        .rustc_flags(["-Copt-level=2"])
        .deps(#{ utils: #{ crate: "utils", features: ["alloc"] } })
        .dev_deps(#{ test_utils: #{ crate: "test_utils" } })
        .artifact_deps(["other_crate"])
        .artifact_env("KERNEL_PATH", "kernel")  // inject artifact path as env var
        .requires_config(false);                 // override group config linking
```

**Artifact env** enables cross-crate embedding: in Rust, `include_bytes!(env!("KERNEL_PATH"))`.

### Dependencies

```rhai
dependency("spin")
    .version("0.9")               // crates.io
    .path("../local-crate")       // local path
    .git("https://...")            // git repo
    .branch("main")               // or .tag("v1") or .rev("abc123")
    .features(["mutex"])
    .default_features(false)
    .optional(true);
```

Link in crate: `.deps(#{ spin: #{ crate: "spin" } })`

Vendor: `gluon vendor` writes `gluon.lock` — commit to VCS.

### Config Options (Kconfig-style)

```rhai
config_bool("LOG_ENABLED").default_value(true).help("Enable logging");
config_u32("LOG_LEVEL").default_value(3).range(0, 5).depends_on("LOG_ENABLED");
config_str("KERNEL_NAME").default_value("mykernel");
config_choice("CPU_TYPE").default_value("x86_64");
config_list("FEATURES").default_value([]);
config_tristate("EXPERIMENTAL").default_value("no");
// All support: .help(), .depends_on("SYM"), .depends_on_expr("A && !B"), .selects(["OPT"])
```

Also: `config_u64`, `load_kconfig("./options.kconfig")`.

**Presets** (named config bundles):
```rhai
preset("verbose").set("LOG_LEVEL", 5).set("DEBUG", true);
// Use: gluon build --preset verbose
```

**Override precedence** (highest → lowest):
1. `GLUON_*` env vars (`GLUON_LOG_LEVEL=5 gluon build`)
2. `.gluon-config` file (per-developer, gitignored)
3. `gluon.rhai` defaults

Generated config crate provides constants: `use my_config::*; if LOG_ENABLED { ... }`

### QEMU

```rhai
qemu("qemu-system-x86_64")
    .machine("q35").memory(256).cores(2)
    .serial_stdio()           // or .serial_none() or .serial_file(path)
    .boot_mode("direct")     // "direct"|"uefi"
    .extra_args(["-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"])
    // UEFI-specific:
    .ovmf_code(path).ovmf_vars(path)  // fallback: OVMF_CODE/OVMF_VARS env vars
    .esp_dir(path).esp_image(path)
    // Testing:
    .test_exit_port(0xf4).test_success_code(0x10);
```

Boot mode precedence: `--uefi/--direct` (CLI) > `qemu().boot_mode()` > direct (default).

### ESP (UEFI boot)

```rhai
esp("default")
    .add("bootloader", "EFI/BOOT/BOOTX64.EFI")
    .add("kernel", "EFI/kernel");
```

### Pipelines & Rules (post-build steps)

```rhai
rule("mkimage")
    .inputs(["kernel"]).outputs(["kernel.uImage"])
    .handler("exec")
    .on_execute("objcopy -O binary kernel kernel.bin");

pipeline()
    .stage("compile", ["kernel"]).rule("copy")
    .stage("image", ["copy"]).rule("mkimage")
    .barrier("done");
```

## Directory Layout

```
project-root/
├── gluon.rhai           # Build configuration
├── .gluon-config        # Per-developer overrides (gitignored)
├── gluon.lock           # Vendored dependency pins (committed)
├── rust-project.json    # Generated by gluon configure
├── vendor/              # Vendored dependencies
└── build/
    ├── sysroot/<target>/   # Custom core/alloc/compiler_builtins
    └── target/<profile>/final/  # Output artifacts
```

## Key Concepts

- **Sysroot**: Auto-compiled core/alloc/compiler_builtins per target. Requires `rustup component add rust-src`. Most expensive part of clean build; use `--keep-sysroot` with clean.
- **Profiles**: Like Cargo's dev/release but with full control. Profiles can inherit from each other.
- **Groups**: Crate collections sharing a target and edition. A project typically has one group per target (e.g., kernel group + UEFI bootloader group).
- **Artifact deps**: Let one crate embed another's binary output via `artifact_env()` + `include_bytes!(env!(...))`.
- **External plugins**: `gluon <name>` dispatches to `gluon-<name>` binary on `$PATH`.
