# Groups and Crates

Groups collect crates that share a target and Rust edition. Every crate in the
build model belongs to exactly one group.

## Groups

```rhai
group("kernel")
    .target("x86_64-unknown-none")
    .edition("2021")
    .is_project(true)
    .config(true)
    .add("my_crate", "crates/my-crate")
        // crate methods chained after .add()
```

### Group methods

| Method | Description |
|---|---|
| `.target(name)` | All crates in this group compile for this target |
| `.edition("2021")` | Rust edition for all crates in the group |
| `.is_project(true)` | Marks these as project crates (affects clippy linting) |
| `.config(true)` | Link crates in this group with the generated config crate |
| `.shared_flags([...])` | Rustc flags applied to every crate in the group |

A project typically has one group per target triple. For example, a kernel
project with a UEFI bootloader might have a `"kernel"` group targeting
`x86_64-unknown-none` and a `"uefi"` group targeting `x86_64-unknown-uefi`.

## Crates

Crates are added to a group via `.add(name, path)`. The name is used as the
crate identifier throughout the build model, and the path is relative to the
project root. After `.add()`, you chain crate-specific methods:

```rhai
group("kernel")
    .target("x86_64-unknown-none")
    .edition("2021")
    .add("kernel", "crates/kernel")
        .crate_type("bin")
        .root("src/main.rs")
        .linker_script("crates/kernel/kernel.ld")
        .dep("log", "log")
        .features(["alloc"])
        .cfg_flags(["my_flag"])
        .rustc_flags(["-C", "link-arg=-Tkernel.ld"]);
```

### Crate methods

| Method | Description |
|---|---|
| `.crate_type(type)` | `"bin"`, `"lib"` (default), `"proc-macro"`, `"staticlib"` |
| `.root(path)` | Entry point file (default: `src/lib.rs` for lib, `src/main.rs` for bin) |
| `.linker_script(path)` | Linker script path, relative to the project root |
| `.dep(alias, crate_name)` | Add a dependency on another crate in the build model |
| `.dev_dep(alias, crate_name)` | Dev dependency (test/bench only) |
| `.features(list)` | Cargo-style feature flags |
| `.cfg_flags(list)` | `--cfg` flags passed to rustc |
| `.rustc_flags(list)` | Arbitrary additional rustc flags |
| `.requires_config(bool)` | Link with the generated config crate (overrides group setting) |

The `.dep()` method takes two arguments: the alias (used in `extern crate` or
`use` statements) and the crate name as declared in the build model. For
external dependencies, the crate name must match a `dependency()` declaration.
For project crates, it must match the name passed to `.add()`.

## Artifact environment variables

The `.artifact_env()` method enables cross-crate artifact embedding -- a common
pattern in bootloader/kernel setups.

```rhai
group("uefi")
    .target("x86_64-unknown-uefi")
    .edition("2021")
    .add("bootloader", "crates/bootloader")
        .crate_type("bin")
        .artifact_env("KERNEL_PATH", "kernel");
```

**`.artifact_env(env_var, crate_name)`** does two things:

1. Injects an environment variable at compile time containing the absolute path
   to the named crate's output artifact.
2. Automatically adds a build-order dependency so the referenced crate compiles
   first.

This enables patterns like embedding the kernel binary inside the bootloader:

```rust
// In the bootloader crate
static KERNEL: &[u8] = include_bytes!(env!("KERNEL_PATH"));
```

The bootloader does not need to know where the kernel artifact lives on disk --
Gluon resolves the path and injects it as an environment variable before
compiling the bootloader.
