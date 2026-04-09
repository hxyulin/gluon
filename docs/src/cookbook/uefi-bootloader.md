# UEFI Bootloader + Kernel

This is the most advanced cookbook example: a UEFI bootloader that embeds a
bare-metal kernel binary, with ESP assembly and QEMU integration. It
demonstrates multi-target builds, cross-crate artifact dependencies, and UEFI
boot configuration.

## gluon.rhai

```rhai
project("my-os", "0.1.0")
    .default_profile("debug");

// Two targets: bare-metal for kernel, UEFI for bootloader
target("x86_64-unknown-none");
target("x86_64-unknown-uefi");

profile("debug")
    .target("x86_64-unknown-uefi")
    .opt_level(0)
    .debug_info(true)
    .boot_binary("bootloader");

profile("release")
    .target("x86_64-unknown-uefi")
    .opt_level(2)
    .lto("thin")
    .boot_binary("bootloader");

// Kernel group -- compiled for bare-metal
group("kernel")
    .target("x86_64-unknown-none")
    .edition("2021")
    .add("kernel", "crates/kernel")
        .crate_type("bin")
        .root("src/main.rs")
        .linker_script("crates/kernel/kernel.ld");

// Bootloader group -- compiled for UEFI
// artifact_env injects the kernel's output path and adds a build dependency
group("uefi")
    .target("x86_64-unknown-uefi")
    .edition("2021")
    .add("bootloader", "crates/bootloader")
        .crate_type("bin")
        .root("src/main.rs")
        .artifact_env("KERNEL_PATH", "kernel");

// Assemble an EFI System Partition
esp("default")
    .add("bootloader", "EFI/BOOT/BOOTX64.EFI");

// QEMU with UEFI boot (requires OVMF)
qemu("qemu-system-x86_64")
    .machine("q35")
    .memory(256)
    .cores(1)
    .serial_stdio()
    .boot_mode("uefi");
```

## Directory layout

```
my-os/
  gluon.rhai
  crates/
    kernel/
      src/main.rs
      kernel.ld
    bootloader/
      src/main.rs
```

## How it works

The build has four stages:

### 1. Kernel compiles first

The `artifact_env("KERNEL_PATH", "kernel")` declaration on the bootloader crate
does two things: it creates an automatic build dependency so the kernel is
always compiled before the bootloader, and it sets the `KERNEL_PATH` environment
variable to the absolute path of the kernel binary during bootloader
compilation.

### 2. Bootloader embeds the kernel

At compile time, the bootloader uses `env!()` and `include_bytes!()` to embed
the kernel binary directly:

```rust
static KERNEL: &[u8] = include_bytes!(env!("KERNEL_PATH"));
```

This means the bootloader EFI binary is self-contained -- it carries the kernel
payload inside itself.

### 3. ESP assembly

The `esp()` block tells Gluon to copy the compiled bootloader binary into an
EFI System Partition layout. The `.add("bootloader", "EFI/BOOT/BOOTX64.EFI")`
line places it at the standard UEFI boot path. Gluon creates a FAT filesystem
image from this staging directory.

### 4. QEMU launches with UEFI

The `.boot_mode("uefi")` setting tells Gluon to pass OVMF firmware flags to
QEMU. OVMF loads the bootloader from the ESP, which then loads the embedded
kernel into memory and transfers control to it.

## Running

```sh
gluon run                 # Build + UEFI boot in QEMU
gluon run -p release      # Use release profile
gluon run --gdb           # Start with GDB server for debugging
gluon run --dry-run       # Print QEMU command without executing
```

## OVMF setup

Gluon attempts to auto-detect OVMF firmware files. If detection fails, set
the paths explicitly via environment variables:

```sh
export OVMF_CODE=/usr/share/OVMF/OVMF_CODE.fd
export OVMF_VARS=/usr/share/OVMF/OVMF_VARS.fd
```

Common locations by distribution:

| Distribution | Typical path |
|-------------|-------------|
| Ubuntu/Debian | `/usr/share/OVMF/` |
| Fedora | `/usr/share/edk2/ovmf/` |
| Arch Linux | `/usr/share/edk2-ovmf/x64/` |
| macOS (Homebrew) | `$(brew --prefix)/share/qemu/` |

## Multi-target builds

This example uses two target triples in the same build. Gluon compiles a
separate sysroot for each target and routes crates to the correct one based on
their group's `.target()` setting. The dependency graph spans target boundaries
-- the bootloader (UEFI target) depends on the kernel (bare-metal target) via
`artifact_env`.

This example is based on `examples/uefi-with-bootloader.rhai` in the Gluon
repository.
