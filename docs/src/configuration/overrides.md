# Configuration Overrides

Config option values are resolved through a three-layer precedence system.
From highest to lowest priority:

1. **`GLUON_*` environment variables** (highest priority)
2. **`.gluon-config` file** (per-developer overrides)
3. **`gluon.rhai` defaults** (lowest priority)

This design lets the `gluon.rhai` file declare sensible defaults, individual
developers can override them locally without modifying the checked-in config,
and CI or one-off builds can override everything via environment variables.

## The .gluon-config file

A simple `KEY = value` file placed in the project root:

```
# Per-developer overrides
LOG_ENABLED = true
LOG_LEVEL = 5
KERNEL_NAME = "debug-kernel"
```

### Syntax rules

- Lines starting with `#` are comments.
- Boolean values: `true`, `false`.
- Integer values: decimal numbers.
- String values: quoted with double quotes.
- Whitespace around `=` is optional.

The `.gluon-config` file is typically added to `.gitignore` so each developer
can maintain their own overrides without creating merge conflicts.

The file path can be customized in the project declaration or overridden on the
command line with `-C` / `--config-file`.

## Environment variables

Prefix any config option name with `GLUON_` to override it via the environment:

```sh
GLUON_LOG_LEVEL=5 gluon build
GLUON_DEBUG=true gluon build
```

Environment variables always take the highest precedence. This is particularly
useful in CI pipelines where you want to force specific settings without
modifying any files.

## Where presets fit in

Presets (applied via `profile().preset()`) sit between the `gluon.rhai`
defaults and the `.gluon-config` overrides. The full precedence order is:

1. `GLUON_*` environment variables
2. `.gluon-config` file
3. Profile-level `.config(key, value)` calls
4. Preset values (via `profile().preset()`)
5. `gluon.rhai` `config_*().default_value()` declarations

## Precedence example

Given these three layers:

```rhai
// gluon.rhai
config_u32("LOG_LEVEL").default_value(3);
```

```
# .gluon-config
LOG_LEVEL = 4
```

```sh
# Environment
GLUON_LOG_LEVEL=5 gluon build
```

The resolved value of `LOG_LEVEL` is **5** -- the environment variable wins.

Remove the environment variable, and the value becomes **4** (from
`.gluon-config`). Remove the `.gluon-config` entry too, and it falls back to
**3** (the default declared in `gluon.rhai`).
