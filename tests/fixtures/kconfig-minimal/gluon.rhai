// K9 fixture: a tiny project that loads its config from a .kconfig file
// instead of declaring options inline. The integration test runs this
// alongside `kconfig-rhai-equiv/` and asserts that both produce the
// same resolved option set, proving the loader is on-parity with the
// existing Rhai builder for the simple AND-of-symbols subset.
//
// The kconfig file additionally exercises features the Rhai builder
// cannot express (`||`/`!` in `depends_on`, nested menus, `source`,
// preset overrides), which are tested separately via direct
// resolved-config inspection.

project("kconfig-minimal", "0.1.0");

target("x86_64-unknown-none");

profile("default")
    .target("x86_64-unknown-none")
    .opt_level(0)
    .debug_info(true);

load_kconfig("./options.kconfig");
