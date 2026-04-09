# Introduction

Gluon is a build system for bare-metal Rust kernels and firmware. It bypasses
Cargo entirely, driving `rustc` directly with per-crate flag assembly,
compiling custom sysroots (`core`, `alloc`, `compiler_builtins`) for arbitrary
target triples, and orchestrating the full pipeline from configuration through
compilation to bootable artifacts and QEMU runs. The entire build is declared
in a single Rhai-scripted configuration file, `gluon.rhai`.

**Why not Cargo?** Cargo is an excellent tool for userland Rust, but bare-metal
kernels push it past its comfort zone. Custom sysroot compilation requires
unstable `-Zbuild-std` flags and fragile workarounds. Multiple target triples
in a single workspace need separate build directories and manual coordination.
Linker scripts, post-build pipelines (objcopy, image assembly, ESP layout),
QEMU integration, and Kconfig-style configuration options all live outside
Cargo's model and end up duct-taped together in Makefiles or shell scripts.
Gluon replaces that entire stack with a single, purpose-built tool that
understands the bare-metal build from end to end.

**Who is Gluon for?** Anyone building a bare-metal Rust kernel, microkernel,
hypervisor, or firmware image. Gluon is kernel-agnostic -- it does not assume a
particular target triple, bootloader, or boot protocol. It is a host-side
userland tool, not a runtime dependency. The project is functional and actively
developed; expect rough edges and evolving APIs.

Ready to try it? Head to [Installation](./getting-started/installation.md) to
get started.
