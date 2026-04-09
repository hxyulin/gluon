// Path-only variant of the vendor-minimal fixture.
//
// Used by the no-network integration test in
// `crates/gluon-core/tests/vendor_e2e.rs`. Identical to `gluon.rhai`
// except the `bitflags` crates.io dep is omitted, so `vendor_sync`
// does not need to shell out to `cargo vendor`.

project("vendor-minimal", "0.1.0");

target("x86_64-unknown-none");

profile("dev")
    .target("x86_64-unknown-none")
    .opt_level(0)
    .debug_info(true);

dependency("vendor_minimal_helper").path("../vendor-minimal-helper");
