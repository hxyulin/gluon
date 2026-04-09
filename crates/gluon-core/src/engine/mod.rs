//! Rhai scripting engine for `gluon.rhai`.
//!
//! This module wires a [`rhai::Engine`] up with the gluon builder API and
//! evaluates a script into a [`BuildModel`]. It is single-threaded by design
//! (Rhai evaluation is single-threaded), so the model is shared via
//! `Rc<RefCell<_>>` rather than `Arc<Mutex<_>>`.
//!
//! Only the **model builders** (`project`, `target`, `profile`, `group`,
//! `group.add` → `CrateBuilder`, `dependency`) are registered in this chunk;
//! config, pipeline, rule, qemu, bootloader, image, and per-crate script
//! loading all land in later chunks.

use crate::error::{Diagnostic, Error, Result};
use gluon_model::{BuildModel, SourceSpan};
use rhai::{Engine, Position};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

mod builders;
mod conversions;
pub(crate) mod intern;
pub(crate) mod validate;

/// Engine state shared by all builders during a single script evaluation.
///
/// All fields are cheap to clone — every builder owns a full `EngineState`
/// clone, which lets methods mutate the model and push diagnostics from a
/// single captured handle.
#[derive(Clone)]
pub(crate) struct EngineState {
    pub(crate) model: Rc<RefCell<BuildModel>>,
    pub(crate) diagnostics: Rc<RefCell<Vec<Diagnostic>>>,
    pub(crate) script_file: Rc<PathBuf>,
    /// Sidecar singleton flag for `qemu()`. `BuildModel::qemu` is a
    /// non-`Option` `QemuDef` with a default value, so we can't tell from
    /// the model alone whether the script has called `qemu()` already.
    pub(crate) qemu_defined: Rc<RefCell<bool>>,
    /// Sidecar singleton flag for `bootloader()`; same rationale as
    /// [`Self::qemu_defined`].
    pub(crate) bootloader_defined: Rc<RefCell<bool>>,
}

impl EngineState {
    pub(crate) fn new(script_file: PathBuf) -> Self {
        Self {
            model: Rc::new(RefCell::new(BuildModel::default())),
            diagnostics: Rc::new(RefCell::new(Vec::new())),
            script_file: Rc::new(script_file),
            qemu_defined: Rc::new(RefCell::new(false)),
            bootloader_defined: Rc::new(RefCell::new(false)),
        }
    }

    /// Push a diagnostic onto the shared channel.
    pub(crate) fn push_diagnostic(&self, d: Diagnostic) {
        self.diagnostics.borrow_mut().push(d);
    }

    /// Convert a Rhai [`Position`] into a [`SourceSpan`] anchored at the
    /// script file currently being evaluated.
    pub(crate) fn span_from(&self, pos: Position) -> SourceSpan {
        pos_to_span((*self.script_file).clone(), pos)
    }
}

/// Convert a Rhai [`Position`] into a point [`SourceSpan`] for the given file.
///
/// Rhai positions are 1-based line, 1-based position-in-line. A `NONE` position
/// (line/position `None`) maps to `(0, 0)`.
pub(crate) fn pos_to_span(file: impl Into<PathBuf>, pos: Position) -> SourceSpan {
    SourceSpan::point(
        file,
        pos.line().unwrap_or(0) as u32,
        pos.position().unwrap_or(0) as u32,
    )
}

/// Parse and evaluate a `gluon.rhai` file, returning the resulting
/// [`BuildModel`].
///
/// Rhai parse/eval failures become [`Error::Script`]. Builder-level errors
/// (strict dep parsing, duplicate definitions, etc.) are collected into a
/// single [`Error::Diagnostics`] so the caller sees every problem at once.
pub fn evaluate_script(path: impl AsRef<Path>) -> Result<BuildModel> {
    let (model, diags) = evaluate_script_raw(path)?;
    if !diags.is_empty() {
        return Err(Error::Diagnostics(diags));
    }
    Ok(model)
}

/// Parse and evaluate a `gluon.rhai` file, returning the resulting
/// [`BuildModel`] together with **all** accumulated diagnostics, regardless
/// of whether any diagnostics were pushed.
///
/// This lower-level entry point is useful for tools that want to surface
/// warnings without discarding the partial model, and for tests that need
/// to inspect the model even when diagnostics were emitted.
///
/// Rhai parse/eval failures still produce [`Error::Script`], because at that
/// point no meaningful model exists.
pub fn evaluate_script_raw(path: impl AsRef<Path>) -> Result<(BuildModel, Vec<Diagnostic>)> {
    let path = path.as_ref().to_path_buf();
    let state = EngineState::new(path.clone());

    let mut engine = Engine::new();
    builders::register_all(&mut engine, &state);

    engine
        .run_file(path.clone())
        .map_err(|e| Error::Script(e.to_string()))?;

    let mut diags = std::mem::take(&mut *state.diagnostics.borrow_mut());

    // Drop the engine so any closures holding `EngineState` clones release
    // their `Rc` references. Without this, `Rc::try_unwrap` below can't
    // succeed and we'd silently clone the model.
    drop(engine);

    let mut model = Rc::try_unwrap(state.model)
        .map(|rc| rc.into_inner())
        .unwrap_or_else(|rc| rc.borrow().clone());

    // Intern pass: resolve string cross-refs to typed handles. Errors are
    // accumulated rather than short-circuiting so the caller sees every
    // dangling reference at once.
    diags.extend(intern::intern(&mut model));

    // Validate pass: structural checks that only make sense on an interned
    // model (cycle detection, pipeline stage sanity, top-level presence).
    diags.extend(validate::validate(&model));

    Ok((model, diags))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gluon_model::CrateType;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_script(contents: &str) -> NamedTempFile {
        let mut f = tempfile::Builder::new()
            .prefix("gluon-test-")
            .suffix(".rhai")
            .tempfile()
            .expect("create temp file");
        f.write_all(contents.as_bytes()).expect("write script");
        f.flush().expect("flush script");
        f
    }

    #[test]
    fn evaluates_empty_script() {
        let f = write_script(r#"project("test", "0.1.0");"#);
        let model = evaluate_script(f.path()).expect("script must evaluate");
        let project = model.project.expect("project set");
        assert_eq!(project.name, "test");
        assert_eq!(project.version, "0.1.0");
    }

    #[test]
    fn evaluates_target_and_profile() {
        let f = write_script(
            r#"
            project("test", "0.1.0");
            target("x86_64-unknown-test", "targets/x86_64-unknown-test.json");
            profile("default")
                .target("x86_64-unknown-test")
                .opt_level(0)
                .debug_info(true);
            "#,
        );
        let model = evaluate_script(f.path()).expect("script must evaluate");
        assert_eq!(model.targets.len(), 1);
        let th = model
            .targets
            .lookup("x86_64-unknown-test")
            .expect("target exists");
        let t = model.targets.get(th).unwrap();
        assert_eq!(t.spec, "targets/x86_64-unknown-test.json");
        assert!(!t.builtin);

        assert_eq!(model.profiles.len(), 1);
        let ph = model.profiles.lookup("default").expect("profile exists");
        let p = model.profiles.get(ph).unwrap();
        assert_eq!(p.target.as_deref(), Some("x86_64-unknown-test"));
        assert_eq!(p.opt_level, Some(0));
        assert_eq!(p.debug_info, Some(true));
    }

    #[test]
    fn evaluates_group_and_crates() {
        let f = write_script(
            r#"
            project("test", "0.1.0");
            target("x86_64-unknown-test", "targets/x.json");
            let g = group("kernel").target("x86_64-unknown-test").edition("2024");
            g.add("foo", "crates/foo");
            g.add("bar", "crates/bar")
                .deps(#{
                    foo: #{ crate: "foo" },
                });
            "#,
        );
        let model = evaluate_script(f.path()).expect("script must evaluate");
        assert_eq!(model.crates.len(), 2);
        let bh = model.crates.lookup("bar").expect("bar crate exists");
        let bar = model.crates.get(bh).unwrap();
        assert_eq!(bar.deps.len(), 1);
        let dep = bar.deps.get("foo").expect("foo dep");
        assert_eq!(dep.crate_name, "foo");
        assert_eq!(bar.crate_type, CrateType::Lib);
        assert_eq!(bar.edition, "2024");
        assert_eq!(bar.target, "x86_64-unknown-test");
        assert_eq!(bar.group, "kernel");
    }

    #[test]
    fn duplicate_dep_key_is_diagnostic() {
        let f = write_script(
            r#"
            project("test", "0.1.0");
            let g = group("kernel");
            g.add("foo", "crates/foo")
                .deps(#{
                    bar: #{ crate: "bar", flargh: "wrong" },
                });
            "#,
        );
        let err = evaluate_script(f.path()).expect_err("should fail");
        match err {
            Error::Diagnostics(v) => {
                assert!(!v.is_empty());
                assert!(
                    v.iter()
                        .any(|d| d.message.contains("unknown dep option 'flargh'")),
                    "expected 'unknown dep option flargh' diagnostic, got: {v:?}"
                );
            }
            other => panic!("expected Error::Diagnostics, got {other:?}"),
        }
    }

    #[test]
    fn missing_crate_key_is_diagnostic() {
        let f = write_script(
            r#"
            project("test", "0.1.0");
            let g = group("kernel");
            g.add("foo", "crates/foo")
                .deps(#{
                    bar: #{ features: [] },
                });
            "#,
        );
        let err = evaluate_script(f.path()).expect_err("should fail");
        match err {
            Error::Diagnostics(v) => {
                assert!(
                    v.iter()
                        .any(|d| d.message.contains("missing required 'crate' field")),
                    "expected missing crate diagnostic, got: {v:?}"
                );
            }
            other => panic!("expected Error::Diagnostics, got {other:?}"),
        }
    }

    #[test]
    fn dependency_builder_works() {
        let f = write_script(
            r#"
            project("test", "0.1.0");
            dependency("log").version("0.4").features(["std"]).no_default_features();
            "#,
        );
        let model = evaluate_script(f.path()).expect("script must evaluate");
        let h = model.external_deps.lookup("log").expect("log dep exists");
        let dep = model.external_deps.get(h).unwrap();
        match &dep.source {
            gluon_model::DepSource::CratesIo { version } => assert_eq!(version, "0.4"),
            other => panic!("expected CratesIo source, got {other:?}"),
        }
        assert_eq!(dep.features, vec!["std".to_string()]);
        assert!(!dep.default_features);
    }

    #[test]
    fn rejects_default_features_key() {
        let f = write_script(
            r#"
            project("test", "0.1.0");
            target("x86_64-unknown-test", "targets/x.json");
            let g = group("k").target("x86_64-unknown-test");
            g.add("foo", "crates/foo")
                .deps(#{ bar: #{ crate: "bar", default_features: false } });
            "#,
        );
        let err = evaluate_script(f.path()).expect_err("should fail");
        match err {
            Error::Diagnostics(v) => {
                assert!(
                    v.iter()
                        .any(|d| d.message.contains("unknown dep option 'default_features'")),
                    "expected 'unknown dep option default_features' diagnostic, got: {v:?}"
                );
            }
            other => panic!("expected Error::Diagnostics, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_group_does_not_mutate_first() {
        let f = write_script(
            r#"
            project("test", "0.1.0");
            target("x86_64-unknown-test", "targets/x.json");
            group("kernel").target("x86_64-unknown-test").edition("2024");
            group("kernel").target("host").edition("2021");
            "#,
        );
        let (model, diags) = evaluate_script_raw(f.path()).expect("script runs");
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("group") && d.message.contains("more than once")),
            "expected duplicate group diagnostic, got: {diags:#?}"
        );
        let kh = model.groups.lookup("kernel").expect("group exists");
        let kernel = model.groups.get(kh).unwrap();
        assert_eq!(
            kernel.target, "x86_64-unknown-test",
            "first definition's target should be preserved"
        );
        assert_eq!(
            kernel.default_edition, "2024",
            "first definition's edition should be preserved"
        );
    }

    // -----------------------------------------------------------------
    // Chunk 3: config + pipeline + rule + qemu + bootloader builders
    // -----------------------------------------------------------------

    #[test]
    fn config_bool_basic() {
        let f = write_script(
            r#"
            project("t", "0.1.0");
            config_bool("CONFIG_FOO").default_value(true).help("helpful");
            "#,
        );
        let model = evaluate_script(f.path()).expect("script must evaluate");
        let opt = model
            .config_options
            .get("CONFIG_FOO")
            .expect("option registered");
        assert_eq!(opt.ty, gluon_model::ConfigType::Bool);
        match opt.default {
            gluon_model::ConfigValue::Bool(true) => {}
            ref other => panic!("expected Bool(true), got {other:?}"),
        }
        assert_eq!(opt.help.as_deref(), Some("helpful"));
    }

    #[test]
    fn config_u32_range() {
        let f = write_script(
            r#"
            project("t", "0.1.0");
            config_u32("CONFIG_SIZE").default_value(4096).range(0, 65536);
            "#,
        );
        let model = evaluate_script(f.path()).expect("script must evaluate");
        let opt = model
            .config_options
            .get("CONFIG_SIZE")
            .expect("option registered");
        assert_eq!(opt.ty, gluon_model::ConfigType::U32);
        match opt.default {
            gluon_model::ConfigValue::U32(4096) => {}
            ref other => panic!("expected U32(4096), got {other:?}"),
        }
        assert_eq!(opt.range, Some((0, 65536)));
    }

    #[test]
    fn config_choice() {
        let f = write_script(
            r#"
            project("t", "0.1.0");
            config_choice("CONFIG_MODE")
                .choices(["debug", "release"])
                .default_value("debug");
            "#,
        );
        let model = evaluate_script(f.path()).expect("script must evaluate");
        let opt = model
            .config_options
            .get("CONFIG_MODE")
            .expect("option registered");
        assert_eq!(opt.ty, gluon_model::ConfigType::Choice);
        match &opt.default {
            gluon_model::ConfigValue::Choice(s) if s == "debug" => {}
            other => panic!("expected Choice(\"debug\"), got {other:?}"),
        }
        assert_eq!(
            opt.choices.as_deref(),
            Some(&["debug".to_string(), "release".to_string()][..])
        );
    }

    #[test]
    fn config_type_mismatch_is_diagnostic() {
        let f = write_script(
            r#"
            project("t", "0.1.0");
            config_bool("CONFIG_FOO").default_value(42);
            "#,
        );
        let err = evaluate_script(f.path()).expect_err("should fail");
        match err {
            Error::Diagnostics(v) => {
                assert!(
                    v.iter()
                        .any(|d| d.message.contains("CONFIG_FOO") && d.message.contains("bool")),
                    "expected type-mismatch diagnostic, got: {v:?}"
                );
            }
            other => panic!("expected Error::Diagnostics, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_config_is_diagnostic() {
        let f = write_script(
            r#"
            project("t", "0.1.0");
            config_bool("CONFIG_FOO").help("first");
            config_bool("CONFIG_FOO").help("second");
            "#,
        );
        let (model, diags) = evaluate_script_raw(f.path()).expect("script runs");
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("CONFIG_FOO") && d.message.contains("more than once")),
            "expected duplicate-config diagnostic, got: {diags:#?}"
        );
        let opt = model
            .config_options
            .get("CONFIG_FOO")
            .expect("first definition preserved");
        assert_eq!(
            opt.help.as_deref(),
            Some("first"),
            "first definition's help should be preserved"
        );
    }

    #[test]
    fn preset_basic() {
        let f = write_script(
            r#"
            project("t", "0.1.0");
            preset("debug").set("CONFIG_FOO", true).set("CONFIG_SIZE", 4096);
            "#,
        );
        let model = evaluate_script(f.path()).expect("script must evaluate");
        let p = model.presets.get("debug").expect("preset registered");
        assert_eq!(p.overrides.len(), 2);
        match p.overrides.get("CONFIG_FOO") {
            Some(gluon_model::ConfigValue::Bool(true)) => {}
            other => panic!("expected Bool(true), got {other:?}"),
        }
        match p.overrides.get("CONFIG_SIZE") {
            Some(gluon_model::ConfigValue::U64(4096)) => {}
            other => panic!("expected U64(4096), got {other:?}"),
        }
    }

    #[test]
    fn rule_basic() {
        let f = write_script(
            r#"
            project("t", "0.1.0");
            rule("my_rule")
                .handler("exec")
                .inputs(["a", "b"])
                .outputs(["out.bin"]);
            "#,
        );
        let model = evaluate_script(f.path()).expect("script must evaluate");
        let h = model.rules.lookup("my_rule").expect("rule exists");
        let r = model.rules.get(h).unwrap();
        match &r.handler {
            gluon_model::RuleHandler::Builtin(s) if s == "exec" => {}
            other => panic!("expected Builtin(\"exec\"), got {other:?}"),
        }
        assert_eq!(r.inputs, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(r.outputs, vec!["out.bin".to_string()]);
    }

    #[test]
    fn pipeline_stages() {
        let f = write_script(
            r#"
            project("t", "1");
            target("x", "t.json");
            let g = group("kernel").target("x");
            g.add("k", "crates/k");
            pipeline().stage("kernel", ["kernel"]);
            "#,
        );
        let model = evaluate_script(f.path()).expect("script must evaluate");
        assert_eq!(model.pipelines.len(), 1);
        let h = model
            .pipelines
            .lookup("main")
            .expect("main pipeline exists");
        let p = model.pipelines.get(h).unwrap();
        assert_eq!(p.stages.len(), 1);
        assert_eq!(p.stages[0].name, "kernel");
        assert_eq!(p.stages[0].inputs, vec!["kernel".to_string()]);
    }

    #[test]
    fn qemu_stub() {
        let f = write_script(
            r#"
            project("t", "0.1.0");
            qemu().machine("q35").memory(512).cores(4);
            "#,
        );
        let model = evaluate_script(f.path()).expect("script must evaluate");
        assert_eq!(
            model.qemu.extras.get("machine").map(String::as_str),
            Some("q35")
        );
        assert_eq!(
            model.qemu.extras.get("memory").map(String::as_str),
            Some("512")
        );
        assert_eq!(
            model.qemu.extras.get("cores").map(String::as_str),
            Some("4")
        );
    }

    #[test]
    fn bootloader_stub() {
        let f = write_script(
            r#"
            project("t", "0.1.0");
            bootloader("generic-uefi");
            "#,
        );
        let model = evaluate_script(f.path()).expect("script must evaluate");
        assert_eq!(model.bootloader.kind, "generic-uefi");
    }

    #[test]
    fn duplicate_qemu_is_diagnostic() {
        let f = write_script(
            r#"
            project("test", "0.1.0");
            qemu().machine("q35");
            qemu().machine("pc");
            "#,
        );
        let (model, diags) = evaluate_script_raw(f.path()).expect("script runs");
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("qemu") && d.message.contains("more than once")),
            "expected duplicate qemu diagnostic, got: {diags:#?}"
        );
        assert_eq!(
            model.qemu.extras.get("machine").map(String::as_str),
            Some("q35"),
            "first qemu definition's machine should be preserved"
        );
    }

    #[test]
    fn duplicate_bootloader_is_diagnostic() {
        let f = write_script(
            r#"
            project("test", "0.1.0");
            bootloader("foo");
            bootloader("bar");
            "#,
        );
        let (model, diags) = evaluate_script_raw(f.path()).expect("script runs");
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("bootloader") && d.message.contains("more than once")),
            "expected duplicate bootloader diagnostic, got: {diags:#?}"
        );
        assert_eq!(
            model.bootloader.kind, "foo",
            "first bootloader definition's kind should be preserved"
        );
    }

    #[test]
    fn duplicate_target_is_diagnostic() {
        let f = write_script(
            r#"
            project("test", "0.1.0");
            target("foo", "a.json");
            target("foo", "b.json");
            "#,
        );
        let err = evaluate_script(f.path()).expect_err("should fail");
        match err {
            Error::Diagnostics(v) => {
                assert!(
                    v.iter().any(|d| d.message.contains("target \"foo\"")
                        && d.message.contains("defined more than once")),
                    "expected duplicate target diagnostic, got: {v:?}"
                );
            }
            other => panic!("expected Error::Diagnostics, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // Chunk 4: intern + validate passes
    // -----------------------------------------------------------------

    #[test]
    fn intern_resolves_profile_target() {
        let f = write_script(
            r#"
            project("t","1");
            target("x86_64-unknown-test", "t.json");
            profile("default").target("x86_64-unknown-test");
            "#,
        );
        let model = evaluate_script(f.path()).expect("script must evaluate");
        let ph = model.profiles.lookup("default").expect("profile exists");
        let p = model.profiles.get(ph).unwrap();
        assert_eq!(p.target_handle, model.targets.lookup("x86_64-unknown-test"));
        assert!(p.target_handle.is_some());
    }

    #[test]
    fn intern_resolves_profile_inherits() {
        let f = write_script(
            r#"
            project("t","1");
            profile("base").opt_level(0);
            profile("debug").inherits("base").opt_level(0);
            "#,
        );
        let model = evaluate_script(f.path()).expect("script must evaluate");
        let dh = model
            .profiles
            .lookup("debug")
            .expect("debug profile exists");
        let d = model.profiles.get(dh).unwrap();
        assert_eq!(d.inherits_handle, model.profiles.lookup("base"));
        assert!(d.inherits_handle.is_some());
    }

    #[test]
    fn intern_unknown_target_is_diagnostic() {
        let f = write_script(
            r#"
            project("t","1");
            profile("default").target("nonexistent");
            "#,
        );
        let err = evaluate_script(f.path()).expect_err("should fail");
        match err {
            Error::Diagnostics(v) => {
                assert!(
                    v.iter()
                        .any(|d| d.message.contains("unknown target 'nonexistent'")
                            && d.message.contains("profile 'default'")),
                    "expected unknown-target diagnostic, got: {v:?}"
                );
            }
            other => panic!("expected Error::Diagnostics, got {other:?}"),
        }
    }

    #[test]
    fn intern_unknown_inherits_is_diagnostic() {
        let f = write_script(
            r#"
            project("t","1");
            profile("default").inherits("nope");
            "#,
        );
        let err = evaluate_script(f.path()).expect_err("should fail");
        match err {
            Error::Diagnostics(v) => {
                assert!(
                    v.iter()
                        .any(|d| d.message.contains("unknown profile 'nope'")
                            && d.message.contains("profile 'default'")),
                    "expected unknown-inherits diagnostic, got: {v:?}"
                );
            }
            other => panic!("expected Error::Diagnostics, got {other:?}"),
        }
    }

    #[test]
    fn intern_resolves_crate_group_and_target() {
        let f = write_script(
            r#"
            project("t","1");
            target("x", "t.json");
            let g = group("kernel").target("x");
            g.add("foo", "crates/foo");
            "#,
        );
        let model = evaluate_script(f.path()).expect("script must evaluate");
        let fh = model.crates.lookup("foo").expect("foo crate exists");
        let foo = model.crates.get(fh).unwrap();
        assert_eq!(foo.group_handle, model.groups.lookup("kernel"));
        assert!(foo.group_handle.is_some());

        let kh = model.groups.lookup("kernel").expect("kernel group exists");
        let kernel = model.groups.get(kh).unwrap();
        assert_eq!(kernel.target_handle, model.targets.lookup("x"));
        assert!(kernel.target_handle.is_some());
    }

    #[test]
    fn intern_resolves_dep_to_project_crate() {
        let f = write_script(
            r#"
            project("t","1");
            target("x", "t.json");
            let g = group("k").target("x");
            g.add("lib", "crates/lib");
            g.add("app", "crates/app").deps(#{ lib: #{ crate: "lib" } });
            "#,
        );
        let model = evaluate_script(f.path()).expect("script must evaluate");
        let ah = model.crates.lookup("app").expect("app crate exists");
        let app = model.crates.get(ah).unwrap();
        let dep = app.deps.get("lib").expect("lib dep exists");
        assert_eq!(dep.crate_handle, model.crates.lookup("lib"));
        assert!(dep.crate_handle.is_some());
    }

    #[test]
    fn intern_resolves_dep_to_external() {
        let f = write_script(
            r#"
            project("t","1");
            target("x", "t.json");
            dependency("log").version("0.4");
            let g = group("k").target("x");
            g.add("app", "crates/app").deps(#{ log: #{ crate: "log" } });
            "#,
        );
        let (model, diags) = evaluate_script_raw(f.path()).expect("script runs");
        let ah = model.crates.lookup("app").expect("app crate exists");
        let app = model.crates.get(ah).unwrap();
        let dep = app.deps.get("log").expect("log dep exists");
        assert!(
            dep.crate_handle.is_none(),
            "external dep should leave crate_handle as None, got {:?}",
            dep.crate_handle
        );
        assert!(
            !diags.iter().any(|d| d.message.contains("unknown crate")),
            "external dep should not produce a dangling-dep diagnostic, got: {diags:#?}"
        );
    }

    #[test]
    fn intern_unknown_dep_is_diagnostic() {
        let f = write_script(
            r#"
            project("t","1");
            target("x", "t.json");
            let g = group("k").target("x");
            g.add("app", "crates/app").deps(#{ foo: #{ crate: "nonexistent" } });
            "#,
        );
        let err = evaluate_script(f.path()).expect_err("should fail");
        match err {
            Error::Diagnostics(v) => {
                assert!(
                    v.iter()
                        .any(|d| d.message.contains("unknown crate 'nonexistent'")
                            && d.message.contains("app")),
                    "expected unknown-crate diagnostic mentioning app, got: {v:?}"
                );
            }
            other => panic!("expected Error::Diagnostics, got {other:?}"),
        }
    }

    #[test]
    fn intern_resolves_pipeline_stage_inputs() {
        let f = write_script(
            r#"
            project("t","1");
            target("x", "t.json");
            let g = group("k").target("x");
            g.add("a", "crates/a");
            pipeline().stage("k", ["k"]);
            "#,
        );
        let model = evaluate_script(f.path()).expect("script must evaluate");
        let ph = model
            .pipelines
            .lookup("main")
            .expect("main pipeline exists");
        let p = model.pipelines.get(ph).unwrap();
        assert_eq!(p.stages.len(), 1);
        assert_eq!(p.stages[0].inputs_handles.len(), 1);
        assert_eq!(p.stages[0].inputs_handles[0], model.groups.lookup("k"));
        assert!(p.stages[0].inputs_handles[0].is_some());
    }

    #[test]
    fn validate_detects_profile_cycle() {
        let f = write_script(
            r#"
            project("t","1");
            profile("a").inherits("b");
            profile("b").inherits("a");
            "#,
        );
        let err = evaluate_script(f.path()).expect_err("should fail");
        match err {
            Error::Diagnostics(v) => {
                assert!(
                    v.iter().any(|d| d.message.contains("cycle")
                        && d.message.contains("a")
                        && d.message.contains("b")),
                    "expected profile cycle diagnostic, got: {v:?}"
                );
            }
            other => panic!("expected Error::Diagnostics, got {other:?}"),
        }
    }

    #[test]
    fn validate_requires_project() {
        // Note: the script still has to parse, so we use a no-op statement.
        let f = write_script(
            r#"
            target("x", "t.json");
            "#,
        );
        let err = evaluate_script(f.path()).expect_err("should fail");
        match err {
            Error::Diagnostics(v) => {
                assert!(
                    v.iter().any(
                        |d| d.message.contains("project") && d.message.contains("must declare")
                    ),
                    "expected project-required diagnostic, got: {v:?}"
                );
            }
            other => panic!("expected Error::Diagnostics, got {other:?}"),
        }
    }

    // TODO: test pipeline rule resolution when PipelineStep.rule has a
    // script-facing setter (currently the rhai pipeline builder does not
    // expose one, so there's no way to construct an invalid reference).
}
