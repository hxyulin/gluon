# UEFI Boot and ESP Assembly

UEFI boot is the standard boot path for modern x86_64 systems and is
required if your OS uses a UEFI bootloader. Gluon handles the full
pipeline: compiling the bootloader for a UEFI target, assembling the
EFI System Partition, and configuring QEMU with OVMF firmware.

## Overview

UEFI boot requires three components:

1. **OVMF firmware** -- a UEFI implementation for QEMU that replaces the
   default BIOS.
2. **A UEFI application** -- your bootloader, compiled for a UEFI target
   triple like `x86_64-unknown-uefi`.
3. **An EFI System Partition (ESP)** -- a FAT-formatted partition
   containing the UEFI application at the standard path
   (`EFI/BOOT/BOOTX64.EFI` for x86_64).

## ESP Assembly

Declare an ESP layout in `gluon.rhai`:

```rhai
esp("default")
    .add("bootloader", "EFI/BOOT/BOOTX64.EFI");
```

The `.add(crate_name, esp_path)` method copies the named crate's output
binary to the given path inside a staging directory. Gluon assembles the
ESP automatically after compilation completes.

You can add multiple files to the ESP:

```rhai
esp("default")
    .add("bootloader", "EFI/BOOT/BOOTX64.EFI")
    .add("shell", "EFI/tools/Shell.efi");
```

The staging directory is located at
`build/cross/<target>/<profile>/esp/` and is rebuilt whenever any of its
input files change.

## OVMF Firmware

Gluon probes for OVMF firmware in this order:

1. **Environment variables:** `OVMF_CODE` and `OVMF_VARS`
2. **Common system paths** (distribution-dependent)

If auto-detection fails, set the environment variables explicitly:

```sh
export OVMF_CODE=/usr/share/OVMF/OVMF_CODE.fd
export OVMF_VARS=/usr/share/OVMF/OVMF_VARS.fd
```

On macOS with Homebrew:

```sh
export OVMF_CODE="$(brew --prefix)/share/qemu/edk2-x86_64-code.fd"
export OVMF_VARS="$(brew --prefix)/share/qemu/edk2-i386-vars.fd"
```

## Complete Example

The following `gluon.rhai` defines a project with a bare-metal kernel and
a UEFI bootloader. The bootloader embeds the kernel binary at compile time
using `artifact_env`.

```rhai
project("my-os", "0.1.0")
    .default_profile("debug");

target("x86_64-unknown-none");
target("x86_64-unknown-uefi");

profile("debug")
    .target("x86_64-unknown-uefi")
    .opt_level(0)
    .boot_binary("bootloader");

// Kernel -- bare-metal, compiled for x86_64-unknown-none
group("kernel")
    .target("x86_64-unknown-none")
    .edition("2021")
    .add("kernel", "crates/kernel")
        .crate_type("bin")
        .linker_script("crates/kernel/kernel.ld");

// Bootloader -- UEFI application, embeds the kernel binary
group("uefi")
    .target("x86_64-unknown-uefi")
    .edition("2021")
    .add("bootloader", "crates/bootloader")
        .crate_type("bin")
        .artifact_env("KERNEL_PATH", "kernel");

esp("default")
    .add("bootloader", "EFI/BOOT/BOOTX64.EFI");

qemu("qemu-system-x86_64")
    .machine("q35")
    .memory(256)
    .serial_stdio()
    .boot_mode("uefi");
```

### How artifact_env works

The `.artifact_env("KERNEL_PATH", "kernel")` call tells Gluon to set the
`KERNEL_PATH` environment variable to the output path of the `kernel`
crate when compiling the `bootloader` crate. This lets the bootloader
embed the kernel at compile time:

```rust
static KERNEL: &[u8] = include_bytes!(env!("KERNEL_PATH"));
```

Gluon automatically adds a dependency edge from `bootloader` to `kernel`
in the DAG, ensuring the kernel is compiled first.

### Running

```sh
gluon run
```

This compiles the kernel, compiles the bootloader (with the kernel path
injected), assembles the ESP, and launches QEMU with OVMF firmware.
The boot mode is set to UEFI in the `qemu()` configuration, so no
additional flags are needed.
