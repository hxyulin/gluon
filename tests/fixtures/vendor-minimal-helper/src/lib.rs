// Sibling path-dep target for the `vendor-minimal` fixture.
// Exists only so `vendor-minimal`'s gluon.rhai has a real
// `dependency("helper").version(...)` target with `DepSource::Path`.
// Never compiled by gluon — only read by the vendor-auto-register pass
// which parses `Cargo.toml` for the edition + crate type.

pub const fn hello() -> &'static str {
    "hi from the vendor helper"
}
