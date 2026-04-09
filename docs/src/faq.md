# FAQ

## Why not just use Cargo?

Cargo is excellent for most Rust projects, but bare-metal kernel development
pushes against several of its assumptions:

- **Custom sysroot compilation** requires the unstable `-Zbuild-std` flag with
  limited control over compiler flags.
- **Multiple target triples** in one project require separate Cargo invocations
  or workspace hacks.
- **Per-crate compiler flags** (different optimization levels, `cfg` flags, or
  linker scripts for different crates) are difficult to express in Cargo.
- **Linker scripts, post-build artifact assembly, and QEMU integration** need
  external scripting.

Gluon handles all of this in a single tool with a declarative configuration
file. It invokes `rustc` directly with per-crate flag assembly, giving you full
control over every aspect of the compilation pipeline.

## What Rust toolchain do I need?

A nightly toolchain with the `rust-src` component. Gluon requires Rust 1.85 or
later. If your project includes a `rust-toolchain.toml`, rustup will
automatically install and use the correct version.

```sh
rustup component add rust-src
```

## How do I add a new target?

Add a `target()` declaration in `gluon.rhai`:

```rhai
target("aarch64-unknown-none");
```

For custom target specs, provide a path to the JSON file:

```rhai
target("my-custom-target", "./targets/my-custom-target.json");
```

Gluon will compile a separate sysroot for each declared target.

## Where are build artifacts?

In the `build/` directory under your project root:

| Path | Contents |
|------|----------|
| `build/cross/<target>/<profile>/deps/` | Compiled crate outputs |
| `build/sysroot/<target>/` | Custom sysroot (`core`, `alloc`, `compiler_builtins`) |
| `build/tool/check/` | `gluon check` output |
| `build/tool/clippy/` | `gluon clippy` output |

## How do I use Gluon with an existing kernel project?

Create a `gluon.rhai` file in your project root. Declare your existing crates,
targets, and dependencies. Gluon does not require any changes to your Rust
source code -- it only needs to know where your crates are and how to compile
them. You can migrate incrementally: start with one crate and expand from
there.

## Can I use Gluon for non-kernel bare-metal projects?

Yes. Gluon is kernel-agnostic. It works for any bare-metal Rust project:
firmware, bootloaders, embedded systems, or anything that needs custom sysroots
and direct `rustc` control.

## How do I integrate with CI?

Gluon is a single binary. Install it in CI the same way as locally:

```sh
cargo install --path crates/gluon-cli
gluon build -j 1    # Deterministic single-threaded for reproducibility
```

Useful CI checks:

- `gluon fmt --check` -- enforce formatting
- `gluon clippy` -- lint all crates
- `gluon vendor --check` -- verify vendored dependencies have not drifted
- `gluon check` -- fast metadata-only build verification

## How do I vendor dependencies?

```sh
gluon vendor           # Populate ./vendor/ and write gluon.lock
```

Commit both `vendor/` and `gluon.lock` to version control. Subsequent builds
auto-detect vendored crates. Use `gluon vendor --check` in CI to ensure the
vendor tree stays in sync. See [Dependencies](./configuration/dependencies.md)
for details.

## How do I debug my kernel in QEMU?

Use the `--gdb` flag:

```sh
gluon run --gdb
```

This starts QEMU with a GDB server, halted at the first instruction. Connect
from another terminal:

```sh
gdb -ex "target remote :1234" build/cross/<target>/<profile>/deps/kernel
```

## How do I pass extra flags to QEMU?

Arguments after `--` are forwarded to QEMU verbatim:

```sh
gluon run -- -d int -no-reboot
```

## How do I see what rustc commands Gluon runs?

Use the `--verbose` flag:

```sh
gluon build --verbose
```

This prints each `rustc` invocation with its full argument list.
