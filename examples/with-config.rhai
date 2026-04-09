// examples/with-config.rhai
//
// Project using Gluon's Kconfig-style configuration system.
//
// Config options become compile-time constants in an auto-generated
// `<project>_config` crate.  They can be declared inline (as below)
// or loaded from a `.kconfig` file via `load_kconfig("path")`.
//
//     gluon build
//     gluon build --preset verbose
//
// This file is illustrative — it is not a runnable project on its own.

// ── Project ──────────────────────────────────────────────────────────
project("configurable_kernel", "0.1.0");

target("x86_64-unknown-none");

profile("default")
    .target("x86_64-unknown-none")
    .opt_level(0)
    .debug_info(true);

// ── Config options ───────────────────────────────────────────────────
// Typed configuration options.  Each becomes a constant in the
// auto-generated `configurable_kernel_config` crate.
//
// Available types:
//   config_bool(name)   — boolean constant
//   config_u32(name)    — unsigned 32-bit integer constant
//   config_str(name)    — string constant  (not shown here)
//
// Chained methods:
//   .default_value(val) — value used when nothing overrides it
//   .help("...")        — human-readable description
//   .range(lo, hi)      — valid range (integers only)
//   .depends_on("SYM")  — option is only active when SYM is true
//   .depends_on_expr("A && !B")  — arbitrary boolean expression

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
    .help("Enable kernel debug assertions and extra checks");

config_bool("FANCY_FEATURE")
    .default_value(true)
    .depends_on_expr("LOG_ENABLED && !DEBUG")
    .help("A feature that requires logging but is disabled in debug mode");

// ── Presets ──────────────────────────────────────────────────────────
// Presets are named bundles of config overrides.  Select one at build
// time with `gluon build --preset verbose`.
//
// `.set(name, value)` overrides the default for the named option.
preset("verbose")
    .set("LOG_LEVEL", 5)
    .set("DEBUG", true);

preset("quiet")
    .set("LOG_ENABLED", false);

// ── Group + crate ────────────────────────────────────────────────────
group("kernel")
    .target("x86_64-unknown-none")
    .edition("2021")
    .add("kernel", "crates/kernel")
        .crate_type("bin")
        .root("src/main.rs");

// ── Using config in Rust ─────────────────────────────────────────────
// The generated config crate is named `<project>_config`, so:
//
//     use configurable_kernel_config::*;
//
//     if LOG_ENABLED {
//         log::set_level(LOG_LEVEL);
//     }
//
// Alternatively, load options from a .kconfig file instead of inline
// declarations:
//
//     load_kconfig("./options.kconfig");
//
// The .kconfig format supports menus, `depends on`, `source`, and the
// full `||`/`&&`/`!` expression grammar.  See the Kconfig
// documentation for details.
