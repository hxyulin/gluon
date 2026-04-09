# QEMU Orchestration

Gluon can build your project and launch it in QEMU in a single command.
The QEMU configuration lives in your `gluon.rhai` alongside the rest of
the build model, so the run environment is versioned with your source code.

## Configuring QEMU

Add a `qemu()` block to your `gluon.rhai`:

```rhai
qemu("qemu-system-x86_64")
    .machine("q35")
    .memory(256)
    .cores(2)
    .serial_stdio()
    .boot_mode("direct");
```

The first argument is the QEMU executable name. It must be on your `$PATH`
or specified as an absolute path.

### Builder methods

| Method              | Description                                     |
|---------------------|-------------------------------------------------|
| `.machine(type)`    | QEMU machine type (e.g., `"q35"`, `"virt"`)    |
| `.memory(mib)`      | RAM in MiB                                      |
| `.cores(n)`         | Number of virtual CPUs                          |
| `.serial_stdio()`   | Redirect the serial port to the terminal        |
| `.boot_mode(mode)`  | `"direct"` (default) or `"uefi"`                |
| `.extra_args(list)` | Additional QEMU arguments passed verbatim       |

## Running

```sh
gluon run                 # Build + launch QEMU
gluon run --no-build      # Skip build, use existing artifacts
gluon run --dry-run       # Print the QEMU command without executing
gluon run -- -serial mon:stdio  # Pass extra args to QEMU after --
```

The `--` separator passes all subsequent arguments directly to QEMU,
overriding or supplementing the configured arguments.

## Boot mode selection

Gluon resolves the boot mode using the following precedence (highest to
lowest):

1. **CLI flags:** `--uefi` or `--direct`
2. **Profile settings:** `qemu().boot_mode("uefi")` in `gluon.rhai`
3. **Default:** direct kernel boot

### Direct boot

QEMU loads the kernel ELF directly via `-kernel <path>`. This is the
simplest mode and requires no firmware. The kernel binary must be a
valid ELF that QEMU can load.

### UEFI boot

Uses OVMF firmware with an EFI System Partition. QEMU boots into UEFI,
which loads the bootloader from the ESP. See
[UEFI Boot and ESP Assembly](./uefi-boot.md) for setup details.

## Timeouts

```sh
gluon run -T 30           # Kill QEMU after 30 seconds
```

The timeout is useful for automated testing: QEMU is terminated after the
specified number of seconds, and Gluon reports whether the process exited
cleanly.

The timeout can also be set per-profile in `gluon.rhai`:

```rhai
profile("test")
    .inherits("dev")
    .test_timeout(60);
```

## Profile overrides

Profiles can override QEMU settings to tailor the run environment for
different use cases:

```rhai
profile("test")
    .inherits("dev")
    .qemu_memory(512)
    .qemu_cores(4)
    .qemu_extra_args(["-display", "none"])
    .test_timeout(60);
```

This is particularly useful for CI, where you might want to disable the
display and set a hard timeout, or for performance testing, where you
might want more cores and memory than the default.
