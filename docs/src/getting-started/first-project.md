# Your First Project

This page walks through creating a minimal bare-metal kernel project with
Gluon. By the end you will have a `gluon.rhai` configuration file and a
directory layout ready to build.

## Directory layout

```
my-kernel/
  gluon.rhai
  crates/kernel/
    src/main.rs
    kernel.ld
```

`gluon.rhai` is the single configuration file that declares your entire build.
It lives at the root of your project.

## Writing gluon.rhai

A minimal configuration needs four declarations: a project, a target, a
profile, and a group containing at least one crate.

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
```

### What each declaration does

- **`project(name, version)`** -- declares the project name and version. This
  is metadata used in build output paths and diagnostics.

- **`target(triple)`** -- registers a rustc target triple. Gluon will compile a
  custom sysroot for each registered target. You can register multiple targets
  if your project needs them (e.g., one for the kernel, another for a
  bootloader).

- **`profile(name)`** -- defines a compilation profile (analogous to Cargo's
  `dev` / `release` profiles). `.target()` sets which target triple this
  profile compiles for, `.opt_level()` and `.debug_info()` control optimization
  and debug symbols, and `.boot_binary()` names the crate whose output is the
  final bootable artifact.

- **`group(name)`** -- collects one or more crates that share a target and
  edition. `.add(name, path)` registers a crate within the group, returning a
  crate builder where you set the crate type, source root, linker script, and
  other per-crate options.

## Adding QEMU configuration

If you want to launch your kernel in QEMU with `gluon run`, add a QEMU block:

```rhai
qemu("qemu-system-x86_64")
    .machine("q35")
    .memory(128)
    .cores(1)
    .serial_stdio();
```

- **`qemu(binary)`** -- the QEMU system emulator binary to invoke.
- **`.machine(type)`** -- the machine/board model (`q35`, `virt`, etc.).
- **`.memory(mb)`** -- guest RAM in megabytes.
- **`.cores(n)`** -- number of virtual CPU cores.
- **`.serial_stdio()`** -- redirects the guest serial port to the host terminal,
  so `print!` / serial output appears in your shell.

## Next steps

With your `gluon.rhai` and source files in place, head to
[Building and Running](./building-and-running.md) to compile and launch your
kernel.
