// Multi-profile fixture exercising features not covered by the minimal fixture:
//   - Multiple profiles (debug + release) with different settings
//   - default_profile
//   - cfg_flags on a crate
//   - Dependency with features
//   - Two groups (host + cross)

project("multiprofile_test", "0.1.0")
    .default_profile("debug");

target("x86_64-unknown-test", "targets/x86_64-unknown-test.json");

profile("debug")
    .target("x86_64-unknown-test")
    .opt_level(0)
    .debug_info(true);

profile("release")
    .target("x86_64-unknown-test")
    .opt_level(2)
    .debug_info(false)
    .lto("thin");

group("host_tools")
    .target("host")
    .edition("2021");

let cross = group("kernel")
    .target("x86_64-unknown-test")
    .edition("2021");

cross.add("mylib", "crates/mylib");

cross.add("app", "crates/app")
    .crate_type("bin")
    .cfg_flags(["my_feature", "test_mode"])
    .deps(#{ mylib: #{ crate: "mylib", features: ["logging"] } });
