//! K9 — end-to-end test for the `.kconfig` parser, loader, and resolver.
//!
//! This is the round-trip parity check between the two equivalent
//! fixtures at:
//!
//! - `tests/fixtures/kconfig-minimal/` — declares its options through a
//!   `.kconfig` file loaded via `load_kconfig()`. Also exercises menus,
//!   the `source` directive, and a boolean expression in `depends_on`.
//! - `tests/fixtures/kconfig-rhai-equiv/` — declares the same options
//!   through the inline `config_*` Rhai builders, omitting the
//!   features the legacy builder cannot express (`||`/`!`).
//!
//! For the AND-of-symbols subset both forms can express, every option's
//! resolved value should match. The test loads both fixtures, runs the
//! resolver, and asserts equality option-by-option.
//!
//! # Why this lives in `gluon-core/tests/`
//!
//! It's a pure in-process test of the parse → load → lower → resolve
//! pipeline. It does not spawn the `gluon` binary and does not need
//! rustc — there's no `#[ignore]` gate. The CLI integration tests in
//! `gluon-cli/tests/integration.rs` are reserved for behavior that
//! actually shells out to the binary; this test would be misleading
//! there because nothing about the CLI is being exercised.

use gluon_core::model::ResolvedValue;
use std::path::{Path, PathBuf};

/// Walk up from `CARGO_MANIFEST_DIR` (which is `crates/gluon-core` for
/// this test crate) to the workspace root, then descend into
/// `tests/fixtures/<name>`. Mirrors the helper used by
/// `gluon-cli/tests/integration.rs::fixture_source_dir`.
fn fixture_dir(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/gluon-core has a two-level parent")
        .join("tests")
        .join("fixtures")
        .join(name)
}

#[test]
fn kconfig_loader_round_trips_against_rhai_builder() {
    let kconfig_dir = fixture_dir("kconfig-minimal");
    let rhai_dir = fixture_dir("kconfig-rhai-equiv");
    assert!(
        kconfig_dir.join("gluon.rhai").is_file(),
        "fixture missing at {kconfig_dir:?}"
    );
    assert!(
        rhai_dir.join("gluon.rhai").is_file(),
        "fixture missing at {rhai_dir:?}"
    );

    // Evaluate both scripts. The kconfig fixture's `load_kconfig()`
    // call merges the .kconfig contents into the same model.
    let kconfig_model =
        gluon_core::evaluate(&kconfig_dir.join("gluon.rhai")).expect("evaluate kconfig fixture");
    let rhai_model =
        gluon_core::evaluate(&rhai_dir.join("gluon.rhai")).expect("evaluate rhai fixture");

    // Resolve both with the default profile and no overrides.
    let kconfig_resolved =
        gluon_core::resolve_config(&kconfig_model, "default", None, &kconfig_dir, None)
            .expect("resolve kconfig fixture");
    let rhai_resolved = gluon_core::resolve_config(&rhai_model, "default", None, &rhai_dir, None)
        .expect("resolve rhai fixture");

    // For every option declared in the Rhai equivalent, the kconfig
    // resolved value should match. The kconfig form may declare
    // additional options (e.g. FANCY_FEATURE) that the Rhai form
    // cannot express, so we iterate over the Rhai keys and treat that
    // as the parity baseline.
    for (name, rhai_value) in &rhai_resolved.options {
        let kconfig_value = kconfig_resolved
            .options
            .get(name)
            .unwrap_or_else(|| panic!("kconfig fixture is missing option '{name}'"));
        assert_eq!(
            kconfig_value, rhai_value,
            "option '{name}' resolved differently:\n  kconfig: {kconfig_value:?}\n  rhai:    {rhai_value:?}"
        );
    }
}

#[test]
fn kconfig_loader_resolves_boolean_expression_form() {
    // FANCY_FEATURE is declared in extras.kconfig with
    //     depends_on = LOG_ENABLED && !DEBUG
    // The defaults are LOG_ENABLED=true, DEBUG=false → expression is
    // true → FANCY_FEATURE keeps its `default = true`. This is the
    // behavior that distinguishes true semantic evaluation from
    // hadron-style `flatten_symbols()`: the flat form would treat the
    // referenced symbols as an implicit AND of two requirements
    // (`LOG_ENABLED` AND `DEBUG`), wrongly disabling FANCY_FEATURE.
    let kconfig_dir = fixture_dir("kconfig-minimal");
    let model =
        gluon_core::evaluate(&kconfig_dir.join("gluon.rhai")).expect("evaluate kconfig fixture");
    let resolved =
        gluon_core::resolve_config(&model, "default", None, &kconfig_dir, None).expect("resolve");
    // Pin the intermediate values explicitly so that editing the
    // fixture defaults flips this test loudly instead of letting the
    // expression silently become vacuous (see the comment on
    // FANCY_FEATURE in `extras.kconfig`).
    assert_eq!(
        resolved.options.get("LOG_ENABLED"),
        Some(&ResolvedValue::Bool(true)),
        "fixture invariant: LOG_ENABLED defaults to true"
    );
    assert_eq!(
        resolved.options.get("DEBUG"),
        Some(&ResolvedValue::Bool(false)),
        "fixture invariant: DEBUG defaults to false"
    );
    assert_eq!(
        resolved.options.get("FANCY_FEATURE"),
        Some(&ResolvedValue::Bool(true)),
        "FANCY_FEATURE should be enabled because LOG_ENABLED && !DEBUG holds at default"
    );
}

#[test]
fn rhai_depends_on_expr_honours_or_semantics() {
    // Parity check for `.depends_on_expr(...)` on the Rhai side.
    //
    // Defaults: A=true, B=false. The expression `A || B` is true
    // under the *semantic* Expr evaluation path (reuses the kconfig
    // grammar) so DEP keeps its default of true. If the Rhai builder
    // were still routing this through the legacy flat `Vec<String>`
    // AND-of-names path, it would treat `A` and `B` as two separate
    // required symbols, see `B` is off, and force DEP to false.
    // Therefore this test's pass/fail cleanly distinguishes the two
    // encodings — it is the smoking gun proving the Rhai surface
    // reuses the `.kconfig` expression grammar, not a second parser.
    let tmp = tempfile::tempdir().expect("tempdir");
    let script = r#"
        project("expr-parity", "0.1.0");
        target("x86_64-unknown-none");
        profile("default").target("x86_64-unknown-none");
        config_bool("A").default_value(true);
        config_bool("B").default_value(false);
        config_bool("DEP").default_value(true).depends_on_expr("A || B");
    "#;
    std::fs::write(tmp.path().join("gluon.rhai"), script).unwrap();

    let model = gluon_core::evaluate(&tmp.path().join("gluon.rhai")).expect("evaluate");
    let resolved =
        gluon_core::resolve_config(&model, "default", None, tmp.path(), None).expect("resolve");

    assert_eq!(
        resolved.options.get("A"),
        Some(&ResolvedValue::Bool(true)),
        "sanity check: A should default to true"
    );
    assert_eq!(
        resolved.options.get("B"),
        Some(&ResolvedValue::Bool(false)),
        "sanity check: B should default to false"
    );
    assert_eq!(
        resolved.options.get("DEP"),
        Some(&ResolvedValue::Bool(true)),
        "DEP should stay on because A || B holds (A is on)"
    );
}

#[test]
fn kconfig_source_directive_pulls_extras_into_model() {
    // FANCY_FEATURE only exists in extras.kconfig, which options.kconfig
    // pulls in via `source "./extras.kconfig"`. If the loader didn't
    // recurse into source directives this option would be missing.
    let kconfig_dir = fixture_dir("kconfig-minimal");
    let model =
        gluon_core::evaluate(&kconfig_dir.join("gluon.rhai")).expect("evaluate kconfig fixture");
    assert!(
        model.config_options.contains_key("FANCY_FEATURE"),
        "source directive failed to pull in FANCY_FEATURE from extras.kconfig"
    );
}
