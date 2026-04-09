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
}

impl EngineState {
    pub(crate) fn new(script_file: PathBuf) -> Self {
        Self {
            model: Rc::new(RefCell::new(BuildModel::default())),
            diagnostics: Rc::new(RefCell::new(Vec::new())),
            script_file: Rc::new(script_file),
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

    let diags = std::mem::take(&mut *state.diagnostics.borrow_mut());

    // Drop the engine so any closures holding `EngineState` clones release
    // their `Rc` references. Without this, `Rc::try_unwrap` below can't
    // succeed and we'd silently clone the model.
    drop(engine);

    let model = Rc::try_unwrap(state.model)
        .map(|rc| rc.into_inner())
        .unwrap_or_else(|rc| rc.borrow().clone());
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
}
