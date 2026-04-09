# Building and Running

This page covers the core CLI commands for compiling, running, and maintaining
your Gluon project.

## Build

```sh
gluon build
```

Compiles every crate in your project. Gluon drives `rustc` directly for each
crate, assembling per-crate flags from your `gluon.rhai` configuration. Custom
sysroots (`core`, `alloc`, `compiler_builtins`) are compiled automatically for
each registered target triple.

Build output goes to the `build/` directory at your project root.

## Run

```sh
gluon run
```

Builds the project (if needed) and launches QEMU using the configuration from
the `qemu()` block in your `gluon.rhai`. This command requires a `qemu()`
declaration in your configuration file.

To pass extra arguments directly to QEMU, place them after `--`:

```sh
gluon run -- -serial mon:stdio -d int
```

## Check

```sh
gluon check
```

Performs a metadata-only check pass, similar to `cargo check`. This runs `rustc`
with the check-only flag, catching type errors and borrow-checker issues without
producing final artifacts. Useful for fast feedback during development.

## Clippy

```sh
gluon clippy
```

Runs `clippy-driver` on your crates, applying the same flag assembly and sysroot
that `gluon build` uses. Requires the `clippy` component in your Rust toolchain.

## Format

```sh
gluon fmt
```

Runs `rustfmt` on your project sources. To verify formatting without modifying
files (useful in CI), pass the check flag:

```sh
gluon fmt --check
```

## Clean

```sh
gluon clean
```

Removes the `build/` directory. To remove build artifacts but preserve the
compiled sysroot (which is expensive to rebuild), use:

```sh
gluon clean --keep-sysroot
```

## Configure

```sh
gluon configure
```

Generates a `rust-project.json` file for rust-analyzer. This gives your editor
full knowledge of the crate graph, sysroot paths, and compiler flags, enabling
accurate completions, diagnostics, and go-to-definition in bare-metal code.

## Common flags

These flags apply to most commands:

| Flag | Description |
|------|-------------|
| `-p <profile>` | Select a build profile (e.g., `-p release`). Defaults to the profile set by `.default_profile()` on `project()`, or `dev` if none is set. |
| `-t <triple>` | Override the target triple for this invocation. |
| `-j <N>` | Set the number of parallel jobs (e.g., `-j 4`). |

### Example: release build

```sh
gluon build -p release
```

### Example: build for a different target

```sh
gluon build -t aarch64-unknown-none
```

## Setting a default profile

Rather than passing `-p` every time, you can set a default in your `gluon.rhai`:

```rhai
project("my-kernel", "0.1.0")
    .default_profile("dev");
```

This is used whenever `-p` is not specified on the command line.
