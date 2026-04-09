# The gluon.rhai File

`gluon.rhai` is the single configuration file that declares your entire build.
It is a [Rhai](https://rhai.rs/) script that Gluon evaluates once at build time
to produce a build model -- the complete description of targets, profiles,
crates, dependencies, config options, and pipelines that Gluon needs to compile
your project.

## How Gluon finds it

Gluon searches upward from the current working directory until it finds a file
named `gluon.rhai`, much like Cargo searches for `Cargo.toml`. This means you
can invoke `gluon build` from any subdirectory of your project and it will find
the right configuration file.

## Builder/chaining pattern

Most declarations in `gluon.rhai` follow a builder pattern where a top-level
function returns an object, and you chain method calls to configure it:

```rhai
profile("dev")
    .target("x86_64-unknown-none")
    .opt_level(0)
    .debug_info(true)
    .boot_binary("kernel");
```

Each top-level call registers an entity in the build model. The chained methods
set properties on that entity. Statements are terminated with semicolons.

## Top-level builder functions

| Function | Purpose |
|---|---|
| `project(name, version)` | Project metadata (name, version, default profile) |
| `target(triple)` | Register a compilation target |
| `profile(name)` | Define a build profile (optimization, target, LTO, etc.) |
| `group(name)` | Group of crates sharing a target and edition |
| `dependency(name)` | External crate dependency |
| `qemu(executable)` | QEMU configuration for `gluon run` |
| `esp(name)` | EFI System Partition layout |
| `pipeline(name)` | Build pipeline with ordered stages |
| `rule(name)` | Custom build rule |
| `config_bool(name)` | Typed config option (boolean) |
| `config_u32(name)` | Typed config option (unsigned 32-bit integer) |
| `config_str(name)` | Typed config option (string) |
| `preset(name)` | Named bundle of config overrides |
| `load_kconfig(path)` | Load a Linux-style Kconfig file |
| `bootloader()` | Bootloader configuration |
| `image(name)` | Disk image definition |

Each of these is covered in detail in the following pages of the Configuration
Guide.

## Rhai scripting capabilities

Because `gluon.rhai` is a full Rhai script, you are not limited to flat
declarations. You can use variables, conditionals, loops, and string
interpolation for advanced or dynamic configurations:

```rhai
// Variables
let arch = "x86_64";
let triple = `${arch}-unknown-none`;

target(triple);

// Conditionals
if arch == "x86_64" {
    qemu("qemu-system-x86_64")
        .machine("q35")
        .memory(256);
} else if arch == "aarch64" {
    qemu("qemu-system-aarch64")
        .machine("virt")
        .memory(256);
}

// Loops
let crates = ["hal", "drivers", "kernel"];
for name in crates {
    // ... dynamic crate registration
}
```

That said, most projects do not need scripting -- the declarative builders are
sufficient. Use scripting when you have genuinely dynamic requirements, not as a
default style.
