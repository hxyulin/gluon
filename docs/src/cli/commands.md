# Commands and Flags

Complete CLI reference for Gluon.

## Global Flags

These flags apply to all commands.

| Flag | Short | Description |
|------|-------|-------------|
| `--profile <NAME>` | `-p` | Profile to use (overrides default) |
| `--target <NAME>` | `-t` | Target triple (overrides profile's target) |
| `--config-file <PATH>` | `-C` | Override file path (default: `.gluon-config`) |
| `--jobs <N>` | `-j` | Parallel compile jobs (default: host parallelism) |
| `--verbose` | `-v` | Emit more verbose output |
| `--quiet` | `-q` | Suppress non-error output |

## Commands

### `gluon build`

Build the project. Compiles all crates in the build model using the resolved
profile. This includes compiling a custom sysroot (`core`, `alloc`,
`compiler_builtins`) if one does not already exist for the target, then
compiling each crate with per-crate flag assembly.

```sh
gluon build
gluon build -p release
gluon build -t aarch64-unknown-none
```

### `gluon check`

Run a metadata-only check pass over every crate. Equivalent to `cargo check`
but uses Gluon's per-crate flag assembly. This is faster than a full build
because it skips code generation. Output goes to `build/tool/check/`.

```sh
gluon check
```

### `gluon clippy`

Run clippy over every crate. Uses `clippy-driver`, resolved in the following
order:

1. The `$CLIPPY_DRIVER` environment variable
2. A `clippy-driver` binary next to the active `rustc`
3. `clippy-driver` on `$PATH`

Output goes to `build/tool/clippy/`.

```sh
gluon clippy
```

### `gluon fmt [--check]`

Run `rustfmt` over every crate in the build model.

| Flag | Description |
|------|-------------|
| `--check` | Verify formatting without rewriting files. Exits non-zero if any file is unformatted. |

```sh
gluon fmt
gluon fmt --check
```

### `gluon clean [--keep-sysroot]`

Remove the `build/` directory.

| Flag | Description |
|------|-------------|
| `--keep-sysroot` | Preserve the sysroot directory, avoiding expensive recompilation of `core`, `alloc`, and `compiler_builtins` on the next build. |

```sh
gluon clean
gluon clean --keep-sysroot
```

### `gluon configure [--output PATH]`

Generate a `rust-project.json` file for rust-analyzer. This gives your editor
a complete project model even though Gluon does not use `Cargo.toml`.

| Flag | Short | Description |
|------|-------|-------------|
| `--output` | `-o` | Output path (default: `<project_root>/rust-project.json`) |

```sh
gluon configure
gluon configure -o ./ide/rust-project.json
```

See [rust-analyzer](../tooling/rust-analyzer.md) for details on editor
integration.

### `gluon vendor [--check] [--force] [--offline]`

Vendor external dependencies into the project.

| Flag | Description |
|------|-------------|
| `--check` | Verify vendor tree integrity without modifying anything. Exits non-zero if the vendor directory has drifted from the lockfile. |
| `--force` | Re-run `cargo vendor` unconditionally, even if the vendor directory appears fresh. |
| `--offline` | No network access. The lockfile must already exist. |

```sh
gluon vendor
gluon vendor --check
gluon vendor --force
```

### `gluon run [flags] [-- QEMU_ARGS...]`

Build the project and launch it in QEMU. Arguments after `--` are passed
directly to QEMU.

| Flag | Short | Description |
|------|-------|-------------|
| `--uefi` | | Force UEFI boot (conflicts with `--direct`) |
| `--direct` | | Force direct kernel boot (conflicts with `--uefi`) |
| `--timeout <SECS>` | `-T` | Kill QEMU after N seconds |
| `--dry-run` | | Print the QEMU command without executing it |
| `--no-build` | | Skip the build step (use existing artifacts) |
| `--gdb` | | Start with a GDB server, halting at the first instruction |

```sh
gluon run
gluon run -p release
gluon run --uefi --timeout 30
gluon run --gdb -- -d int
gluon run --dry-run
```

## External Subcommands

Unknown subcommands are dispatched to `gluon-<name>` binaries on `$PATH`. For
example:

```sh
gluon foo bar
```

This runs `gluon-foo bar`, passing all remaining arguments. This mechanism
enables plugin development without modifying Gluon itself.
