# Gluon

Gluon is a build system for bare-metal Rust kernels. It bypasses Cargo,
drives `rustc` directly, builds custom sysroots, and orchestrates the full
pipeline from configuration through compilation to bootable artifacts and
QEMU runs.

## Why Gluon?

Cargo is excellent for most Rust projects, but bare-metal kernel development
pushes against its assumptions:

- **Custom sysroots.** Bare-metal targets need `core`, `alloc`, and
  `compiler_builtins` compiled from source with specific flags. Cargo's
  `-Zbuild-std` is unstable and gives limited control over sysroot
  compilation flags.
- **Multiple targets in one project.** A UEFI bootloader and a bare-metal
  kernel live in the same repo but compile for different triples. Cargo
  workspaces do not natively support per-crate target overrides.
- **Linker scripts and post-build steps.** Kernels need custom linker
  scripts, binary transformations, and ESP assembly. Cargo has no built-in
  mechanism for these; projects resort to fragile `build.rs` scripts and
  shell wrappers.
- **QEMU integration.** The edit-compile-boot loop is the inner loop of
  kernel development. Gluon makes `gluon run` a single command that
  builds, assembles boot artifacts, and launches QEMU.
- **Compile-time configuration.** Kernels need Kconfig-style option systems
  for conditional compilation across hundreds of features. Cargo features
  are flat and boolean; Gluon provides typed options with dependencies,
  ranges, presets, and override layers.

Gluon replaces this patchwork with a single tool driven by a declarative
Rhai configuration file.

## Quick Start

**Requirements:**

- Rust nightly toolchain (1.85+) with the `rust-src` component
- QEMU (for `gluon run`; not needed for just building)

**Install:**

```sh
git clone https://github.com/hxyulin/gluon.git
cd gluon
cargo install --path crates/gluon-cli
```

**Create `gluon.rhai` in your project root:**

```rhai
project("my-kernel", "0.1.0");

target("x86_64-unknown-none");

profile("dev")
    .target("x86_64-unknown-none")
    .opt_level(0)
    .debug_info(true)
    .boot_binary("kernel");

group("kernel")
    .target("x86_64-unknown-none")
    .edition("2021")
    .add("kernel", "crates/kernel")
        .crate_type("bin")
        .root("src/main.rs")
        .linker_script("crates/kernel/kernel.ld");

qemu("qemu-system-x86_64")
    .machine("q35")
    .memory(128)
    .cores(1)
    .serial_stdio();
```

**Build and run:**

```sh
gluon build              # Compile everything
gluon run                # Build + launch QEMU
gluon check              # Metadata-only check pass
gluon configure          # Generate rust-project.json for rust-analyzer
```

## Features

- **Direct rustc invocation** -- per-crate flag assembly, no Cargo overhead
- **Custom sysroot compilation** -- `core`, `alloc`, `compiler_builtins` built for your target
- **Rhai configuration** -- declarative `gluon.rhai` with projects, targets, profiles, groups, and crates
- **Kconfig-style options** -- typed config options with defaults, ranges, dependencies, and presets
- **Configuration overrides** -- `gluon.rhai` defaults, `.gluon-config` file, and `GLUON_*` environment variables
- **Hybrid build cache** -- two-tier mtime + SHA-256 freshness checking
- **Parallel DAG scheduler** -- dependency-aware build graph with worker pool
- **Dependency vendoring** -- `gluon vendor` with lockfile and fingerprinting
- **QEMU orchestration** -- direct boot, UEFI boot (OVMF), GDB server, timeouts
- **rust-analyzer integration** -- `gluon configure` generates `rust-project.json`
- **Pipeline and rule system** -- user-defined build stages with copy, mkimage, and exec rules
- **ESP assembly** -- automatic EFI System Partition layout for UEFI boot
- **External plugins** -- `gluon foo` dispatches to `gluon-foo` on `$PATH`

## Documentation

Full documentation is available in [`docs/`](docs/) (built with [mdbook](https://rust-lang.github.io/mdBook/)).

```sh
cd docs && mdbook serve
```

## License

Gluon is licensed under the [GNU General Public License v3.0](LICENSE).
