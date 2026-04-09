# Debugging with GDB

Gluon integrates with QEMU's built-in GDB server to provide a
straightforward debugging workflow for bare-metal kernels.

## Starting a debug session

```sh
gluon run --gdb
```

This starts QEMU with two additional flags:

- `-s` -- start a GDB server on TCP port 1234
- `-S` -- halt the CPU before executing the first instruction

QEMU will launch but wait for a GDB client to connect before running
any code.

## Attaching GDB

In a separate terminal, start GDB with your kernel's ELF binary:

```sh
gdb build/cross/<target>/<profile>/deps/kernel
```

Then connect to the QEMU GDB server:

```
(gdb) target remote :1234
(gdb) break _start
(gdb) continue
```

Replace the path with the actual location of your kernel binary. Crate
artifacts live under `build/cross/<target>/<profile>/deps/`.

## Tips

### Load symbols from the ELF binary

Always pass the **unstripped ELF binary** to GDB, not a stripped or
objcopy'd binary. The ELF contains DWARF debug information that GDB
needs for source-level debugging, symbol names, and type information.

If you have a separate debug symbols file, load it with:

```
(gdb) symbol-file path/to/kernel.elf
```

### Set the architecture manually

If GDB does not auto-detect the target architecture, set it explicitly:

```
(gdb) set architecture i386:x86-64
```

For AArch64 targets:

```
(gdb) set architecture aarch64
```

### Use hardware breakpoints for early boot

Software breakpoints work by patching instructions in memory. In early
boot code -- before the MMU is configured or when executing from ROM --
software breakpoints may not work. Use hardware breakpoints instead:

```
(gdb) hbreak _start
```

Hardware breakpoints use the CPU's debug registers and work regardless
of memory configuration. Most x86_64 CPUs support 4 hardware breakpoints
simultaneously.

### Combine with a timeout

When debugging, it is easy to leave QEMU running indefinitely after
detaching GDB. Use a timeout as a safety net:

```sh
gluon run --gdb -T 300
```

This kills QEMU after 5 minutes if you forget to shut it down.

### Useful GDB commands for kernel debugging

| Command                    | Description                              |
|----------------------------|------------------------------------------|
| `info registers`           | Print all CPU registers                  |
| `x/16xg $rsp`             | Examine 16 quad-words at the stack pointer |
| `layout src`               | Show source code in a TUI split          |
| `layout asm`               | Show disassembly in a TUI split          |
| `monitor info mem`         | Query QEMU's memory map (via GDB monitor)|
| `monitor info cpus`        | List QEMU vCPUs and their states         |

The `monitor` prefix sends commands to QEMU's monitor interface through
the GDB connection, giving you access to QEMU introspection without a
separate monitor console.
