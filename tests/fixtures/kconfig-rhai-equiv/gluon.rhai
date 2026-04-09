// K9 fixture: the pure-Rhai equivalent of `kconfig-minimal/options.kconfig`
// for the AND-of-symbols subset that both forms can express.
//
// FANCY_FEATURE is intentionally omitted because its `depends_on`
// expression in the .kconfig form (`LOG_ENABLED && !DEBUG`) cannot be
// expressed in the legacy flat-Vec<String> Rhai builder. The K9 parity
// test only compares the options both fixtures declare.

project("kconfig-minimal", "0.1.0");

target("x86_64-unknown-none");

profile("default")
    .target("x86_64-unknown-none")
    .opt_level(0)
    .debug_info(true);

config_bool("LOG_ENABLED")
    .default_value(true)
    .help("Enable runtime logging");

config_u32("LOG_LEVEL")
    .default_value(3)
    .range(0, 5)
    .depends_on("LOG_ENABLED");

config_bool("DEBUG")
    .default_value(false);

preset("verbose")
    .set("LOG_LEVEL", 5)
    .set("DEBUG", true);
