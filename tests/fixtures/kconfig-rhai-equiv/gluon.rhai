// K9 fixture: the pure-Rhai equivalent of `kconfig-minimal/options.kconfig`.
//
// Including FANCY_FEATURE — which uses the full `&&`/`!` expression
// grammar — is the whole point of the `.depends_on_expr(...)` Rhai
// method: it reuses the `.kconfig` expression parser so this fixture
// can declare the same shape the `.kconfig` file does without a second
// grammar. The K9 parity test compares every option across both
// fixtures; if the resolver produced different values for
// FANCY_FEATURE between the two forms, the round-trip test would
// notice.

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

config_bool("FANCY_FEATURE")
    .default_value(true)
    .depends_on_expr("LOG_ENABLED && !DEBUG");

preset("verbose")
    .set("LOG_LEVEL", 5)
    .set("DEBUG", true);
