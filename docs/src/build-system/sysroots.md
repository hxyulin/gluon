# Custom Sysroots

## What is a sysroot?

Bare-metal targets (`#![no_std]`) cannot use pre-compiled standard library
crates because those crates either do not exist for custom target triples
or were compiled with flags that do not match the target's requirements.

The **sysroot** is the directory where Rust looks for these foundational
crates. For hosted targets (like `x86_64-unknown-linux-gnu`), rustup
provides a pre-compiled sysroot. For bare-metal targets, the sysroot must
be compiled from source.

## How Gluon handles sysroots

Gluon automatically compiles a custom sysroot for each target triple
declared in your build model. No manual configuration is needed -- if your
`gluon.rhai` declares a bare-metal target, Gluon takes care of the rest.

The compiled sysroot lives at:

```
build/sysroot/<target>/
```

### What gets compiled

- **`core`** -- the foundational `#![no_std]` library. Every bare-metal
  crate depends on this.
- **`alloc`** -- heap allocation support (`Box`, `Vec`, `String`, etc.).
  Required if your kernel uses a global allocator.
- **`compiler_builtins`** -- compiler intrinsics such as `memcpy`,
  `memset`, and soft-float routines. Rustc expects these to be available
  for any target.

### When the sysroot is rebuilt

The sysroot is rebuilt when any of the following conditions are met:

- **First build** for a given target triple.
- **Rust toolchain change** -- detected by comparing the `rustc` binary's
  modification time against the cached value.
- **Missing or corrupted sysroot directory** -- if the directory does not
  exist or is incomplete, Gluon rebuilds it from scratch.

In all other cases, the existing sysroot is reused.

## Requirements

The `rust-src` component must be installed for the active toolchain:

```sh
rustup component add rust-src
```

Gluon locates the `rust-src` path by querying the active toolchain's
sysroot (`rustc --print sysroot`) and looking for
`lib/rustlib/src/rust/library/` underneath it.

If `rust-src` is not installed, Gluon will report a clear error pointing
you at the `rustup component add` command.

## Sysroot caching

Compiling the sysroot is the most expensive part of a clean build -- it
takes significantly longer than compiling your own crates. Gluon caches
it aggressively to avoid repeating this work.

The sysroot is **shared across profiles** (debug, release, test, etc.)
and **shared across drivers** (build, check, clippy). There is one sysroot
per target triple, not one per profile.

### Cleaning

- `gluon clean` -- wipes the entire `build/` directory, including the
  sysroot.
- `gluon clean --keep-sysroot` -- removes all build artifacts but
  preserves the sysroot. This is useful when you want a fresh build
  without paying the sysroot compilation cost again.
