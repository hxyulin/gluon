# Kernel with Config Options

Gluon includes a Kconfig-style configuration system that generates a Rust crate
of typed constants. This lets you define build-time options -- feature toggles,
numeric tuning knobs, string paths -- and access them as normal Rust constants
with full type safety.

## gluon.rhai

```rhai
project("configurable_kernel", "0.1.0");

target("x86_64-unknown-none");

profile("default")
    .target("x86_64-unknown-none")
    .opt_level(0)
    .debug_info(true);

// Typed configuration options
config_bool("LOG_ENABLED")
    .default_value(true)
    .help("Enable runtime logging subsystem");

config_u32("LOG_LEVEL")
    .default_value(3)
    .range(0, 5)
    .depends_on("LOG_ENABLED")
    .help("Verbosity: 0 = off, 5 = trace");

config_bool("DEBUG")
    .default_value(false)
    .help("Enable kernel debug assertions");

// Presets bundle config overrides
preset("verbose")
    .set("LOG_LEVEL", 5)
    .set("DEBUG", true);

preset("quiet")
    .set("LOG_ENABLED", false);

group("kernel")
    .target("x86_64-unknown-none")
    .edition("2021")
    .config(true)    // Link with the generated config crate
    .add("kernel", "crates/kernel")
        .crate_type("bin")
        .root("src/main.rs");
```

## Using config values in Rust

The generated config crate is named `<project>_config`. Import it like any
other crate:

```rust
// In crates/kernel/src/main.rs
use configurable_kernel_config::*;

fn init_logging() {
    if LOG_ENABLED {
        set_log_level(LOG_LEVEL);
    }
}
```

Because these are `const` values, the compiler eliminates dead branches. When
`LOG_ENABLED` is `false`, the body of the `if` block is removed entirely -- no
runtime cost.

## Overriding values

There are several ways to override the defaults declared in `gluon.rhai`.

### Per-developer file (`.gluon-config`)

Create a `.gluon-config` file in your project root (typically gitignored):

```
LOG_LEVEL = 4
DEBUG = true
```

### Environment variables

Prefix the option name with `GLUON_`:

```sh
GLUON_LOG_LEVEL=5 gluon build
```

Environment variables take precedence over `.gluon-config`.

### Via preset (in a profile)

Presets bundle multiple overrides into a named set. Apply them in a profile:

```rhai
profile("verbose-dev")
    .inherits("default")
    .preset("verbose");
```

### Precedence order

From lowest to highest priority:

1. `default_value()` in `gluon.rhai`
2. Preset applied by the active profile
3. `.gluon-config` file
4. `--config-file` flag (overrides the `.gluon-config` path)
5. `GLUON_*` environment variables

## Using Kconfig files instead

If you prefer the traditional Kconfig format, replace inline declarations with
a `.kconfig` file:

```rhai
load_kconfig("./options.kconfig");
```

```kconfig
# options.kconfig
config LOG_ENABLED
    bool "Enable logging"
    default y

config LOG_LEVEL
    int "Log verbosity (0-5)"
    default 3
    range 0 5
    depends on LOG_ENABLED
```

Both approaches produce the same generated config crate. Use whichever fits
your project better -- inline declarations for small projects, Kconfig files
for larger ones with many options.

This example is based on `examples/with-config.rhai` in the Gluon repository.
