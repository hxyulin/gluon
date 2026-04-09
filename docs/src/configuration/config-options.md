# Config Options and Kconfig

Gluon supports typed configuration options that become compile-time constants
in an auto-generated Rust crate. This is similar in spirit to Linux's Kconfig
system, adapted for Rust's type system.

## Inline config options

Declare config options directly in `gluon.rhai` using the typed builder
functions:

```rhai
config_bool("LOG_ENABLED")
    .default_value(true)
    .help("Enable runtime logging subsystem");

config_u32("LOG_LEVEL")
    .default_value(3)
    .range(0, 5)
    .depends_on("LOG_ENABLED")
    .help("Verbosity: 0 = off, 5 = trace");

config_str("KERNEL_NAME")
    .default_value("my-kernel")
    .help("Kernel display name");
```

### Available types

| Function | Rust type | Description |
|---|---|---|
| `config_bool(name)` | `bool` | Boolean option |
| `config_u32(name)` | `u32` | Unsigned 32-bit integer |
| `config_str(name)` | `&str` | String constant |

### Common methods

| Method | Description |
|---|---|
| `.default_value(val)` | Default when no override is applied |
| `.help("...")` | Human-readable description |
| `.range(lo, hi)` | Valid range (integers only) |
| `.depends_on("SYMBOL")` | Only active when SYMBOL is true |
| `.depends_on_expr("A && !B")` | Arbitrary boolean expression using `&&`, `\|\|`, `!` |

When a config option has a `.depends_on()` constraint and the dependency
evaluates to false, the option is disabled and uses its default value (or is
omitted entirely, depending on the type).

## Presets

Presets are named bundles of config overrides:

```rhai
preset("verbose")
    .set("LOG_LEVEL", 5)
    .set("DEBUG", true);
```

Apply a preset to a profile:

```rhai
profile("dev")
    .target("x86_64-unknown-none")
    .preset("verbose");
```

Presets are useful for defining common configurations that multiple profiles
can share (e.g., a "verbose" preset for development, a "minimal" preset for
release).

## The generated config crate

Config options are compiled into an auto-generated Rust crate named
`<project>_config` by default. You can customize this name with
`project().config_crate_name()` (see
[Projects, Targets, and Profiles](./projects-targets-profiles.md)).

The generated crate exports each config option as a public constant:

```rust
use my_kernel_config::*;

if LOG_ENABLED {
    set_log_level(LOG_LEVEL);
}
```

Groups with `.config(true)` automatically link against the config crate. For
individual crates, use `.requires_config(true)` to opt in or
`.requires_config(false)` to opt out regardless of the group setting.

## Kconfig files

For projects with many configuration options, you can load them from a
Linux-style Kconfig file instead of (or in addition to) declaring them inline:

```rhai
load_kconfig("./options.kconfig");
```

The path is relative to the `gluon.rhai` file.

### Supported Kconfig syntax

Gluon's Kconfig parser supports the standard constructs:

- **Entries**: `config`, `menuconfig`
- **Grouping**: `menu` / `endmenu`
- **Types**: `bool`, `int`, `string`, `tristate`
- **Properties**: `default`, `depends on`, `select`, `help`
- **Includes**: `source` for pulling in other Kconfig files
- **Expressions**: full `&&`, `||`, `!` grammar in conditions

Kconfig options and inline `config_*()` declarations coexist in the same
namespace. You can declare some options inline and load others from Kconfig
files -- just avoid duplicate names.
