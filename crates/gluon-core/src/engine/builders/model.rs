//! Model builders: `project`, `target`, `profile`, `group`, `CrateBuilder`,
//! `dependency`.

use super::builder_method;
use crate::engine::EngineState;
use crate::engine::conversions::{array_to_string_vec, parse_dep_map};
use crate::error::Diagnostic;
use gluon_model::{
    CrateDef, CrateType, DepSource, ExternalDepDef, GroupDef, ProfileDef, ProjectDef, TargetDef,
};
use rhai::{Dynamic, Engine, Map, NativeCallContext, Position};

// ---------------------------------------------------------------------------
// Builder types
// ---------------------------------------------------------------------------

/// Chainable builder returned by `profile("name")`.
#[derive(Clone)]
pub struct ProfileBuilder {
    state: EngineState,
    name: String,
    pos: Position,
    /// `true` if this builder was returned from a duplicate definition.
    /// When set, all chained methods no-op so the second definition cannot
    /// ghost-mutate the first definition's state.
    is_duplicate: bool,
}

/// Chainable builder returned by `group("name")`.
#[derive(Clone)]
pub struct GroupBuilder {
    state: EngineState,
    name: String,
    pos: Position,
    /// See [`ProfileBuilder::is_duplicate`].
    is_duplicate: bool,
}

/// Chainable builder returned by `GroupBuilder::add("name", "path")`.
#[derive(Clone)]
pub struct CrateBuilder {
    state: EngineState,
    name: String,
    pos: Position,
    /// See [`ProfileBuilder::is_duplicate`]. Also set when the parent
    /// `GroupBuilder` was itself a duplicate, so child-crate chained
    /// methods also no-op.
    is_duplicate: bool,
}

/// Chainable builder returned by `dependency("name")`.
#[derive(Clone)]
pub struct DependencyBuilder {
    state: EngineState,
    name: String,
    pos: Position,
    /// See [`ProfileBuilder::is_duplicate`].
    is_duplicate: bool,
}

// ---------------------------------------------------------------------------
// Crate type mapping (matches the constants exposed in `register_all`).
// ---------------------------------------------------------------------------

fn crate_type_from_i64(v: i64) -> Option<CrateType> {
    match v {
        0 => Some(CrateType::Lib),
        1 => Some(CrateType::Bin),
        2 => Some(CrateType::ProcMacro),
        3 => Some(CrateType::StaticLib),
        _ => None,
    }
}

fn crate_type_from_str(s: &str) -> Option<CrateType> {
    match s {
        "lib" => Some(CrateType::Lib),
        "bin" => Some(CrateType::Bin),
        "proc-macro" | "proc_macro" => Some(CrateType::ProcMacro),
        "staticlib" => Some(CrateType::StaticLib),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Registration entry point
// ---------------------------------------------------------------------------

pub(super) fn register(engine: &mut Engine, state: &EngineState) {
    register_project(engine, state.clone());
    register_target(engine, state.clone());
    register_profile(engine, state.clone());
    register_group(engine, state.clone());
    register_crate_methods(engine);
    register_dependency(engine, state.clone());
}

// ---------------------------------------------------------------------------
// project()
// ---------------------------------------------------------------------------

fn register_project(engine: &mut Engine, state: EngineState) {
    engine.register_fn(
        "project",
        move |ctx: NativeCallContext, name: &str, version: &str| {
            let _pos = ctx.call_position(); // reserved for future diagnostics
            let mut model = state.model.borrow_mut();
            model.project = Some(ProjectDef {
                name: name.into(),
                version: version.into(),
                ..Default::default()
            });
        },
    );
}

// ---------------------------------------------------------------------------
// target()
// ---------------------------------------------------------------------------

fn register_target(engine: &mut Engine, state: EngineState) {
    // Two-arg form: target("name", "spec.json") — custom spec file.
    let s = state.clone();
    engine.register_fn(
        "target",
        move |ctx: NativeCallContext, name: &str, spec: &str| {
            let pos = ctx.call_position();
            let span = s.span_from(pos);
            let (_, inserted) = s.model.borrow_mut().targets.insert(
                name.into(),
                TargetDef {
                    name: name.into(),
                    spec: spec.into(),
                    builtin: false,
                    span: Some(span.clone()),
                },
            );
            if !inserted {
                s.push_diagnostic(
                    Diagnostic::error(format!("target \"{name}\" is defined more than once"))
                        .with_span(span),
                );
            }
        },
    );

    // One-arg form: target("triple") — builtin rustup target.
    let s = state;
    engine.register_fn("target", move |ctx: NativeCallContext, name: &str| {
        let pos = ctx.call_position();
        let span = s.span_from(pos);
        let (_, inserted) = s.model.borrow_mut().targets.insert(
            name.into(),
            TargetDef {
                name: name.into(),
                spec: name.into(),
                builtin: true,
                span: Some(span.clone()),
            },
        );
        if !inserted {
            s.push_diagnostic(
                Diagnostic::error(format!("target \"{name}\" is defined more than once"))
                    .with_span(span),
            );
        }
    });
}

// ---------------------------------------------------------------------------
// profile()
// ---------------------------------------------------------------------------

fn register_profile(engine: &mut Engine, state: EngineState) {
    let s = state;
    engine.register_fn(
        "profile",
        move |ctx: NativeCallContext, name: &str| -> ProfileBuilder {
            let pos = ctx.call_position();
            let span = s.span_from(pos);
            let (_, inserted) = s.model.borrow_mut().profiles.insert(
                name.into(),
                ProfileDef {
                    name: name.into(),
                    span: Some(span.clone()),
                    ..Default::default()
                },
            );
            if !inserted {
                s.push_diagnostic(
                    Diagnostic::error(format!("profile \"{name}\" is defined more than once"))
                        .with_span(span),
                );
            }
            ProfileBuilder {
                state: s.clone(),
                name: name.into(),
                pos,
                is_duplicate: !inserted,
            }
        },
    );

    builder_method!(
        engine,
        "target",
        ProfileBuilder,
        |state, model, name, pos, target: &str| {
            let _ = (state, pos);
            if let Some(h) = model.profiles.lookup(name) {
                if let Some(p) = model.profiles.get_mut(h) {
                    p.target = Some(target.into());
                }
            }
        }
    );

    builder_method!(
        engine,
        "inherits",
        ProfileBuilder,
        |state, model, name, pos, parent: &str| {
            let _ = (state, pos);
            if let Some(h) = model.profiles.lookup(name) {
                if let Some(p) = model.profiles.get_mut(h) {
                    p.inherits = Some(parent.into());
                }
            }
        }
    );

    builder_method!(
        engine,
        "opt_level",
        ProfileBuilder,
        |state, model, name, pos, level: i64| {
            let _ = (state, pos);
            if let Some(h) = model.profiles.lookup(name) {
                if let Some(p) = model.profiles.get_mut(h) {
                    p.opt_level = Some(level as u8);
                }
            }
        }
    );

    builder_method!(
        engine,
        "debug_info",
        ProfileBuilder,
        |state, model, name, pos, enabled: bool| {
            let _ = (state, pos);
            if let Some(h) = model.profiles.lookup(name) {
                if let Some(p) = model.profiles.get_mut(h) {
                    p.debug_info = Some(enabled);
                }
            }
        }
    );

    builder_method!(
        engine,
        "lto",
        ProfileBuilder,
        |state, model, name, pos, mode: &str| {
            let _ = (state, pos);
            if let Some(h) = model.profiles.lookup(name) {
                if let Some(p) = model.profiles.get_mut(h) {
                    p.lto = Some(mode.into());
                }
            }
        }
    );

    builder_method!(
        engine,
        "boot_binary",
        ProfileBuilder,
        |state, model, name, pos, bin: &str| {
            let _ = (state, pos);
            if let Some(h) = model.profiles.lookup(name) {
                if let Some(p) = model.profiles.get_mut(h) {
                    p.boot_binary = Some(bin.into());
                }
            }
        }
    );

    builder_method!(
        engine,
        "preset",
        ProfileBuilder,
        |state, model, name, pos, preset: &str| {
            let _ = (state, pos);
            if let Some(h) = model.profiles.lookup(name) {
                if let Some(p) = model.profiles.get_mut(h) {
                    p.preset = Some(preset.into());
                }
            }
        }
    );
}

// ---------------------------------------------------------------------------
// group() and GroupBuilder
// ---------------------------------------------------------------------------

fn register_group(engine: &mut Engine, state: EngineState) {
    let s = state;
    engine.register_fn(
        "group",
        move |ctx: NativeCallContext, name: &str| -> GroupBuilder {
            let pos = ctx.call_position();
            let span = s.span_from(pos);
            let (_, inserted) = s.model.borrow_mut().groups.insert(
                name.into(),
                GroupDef {
                    name: name.into(),
                    span: Some(span.clone()),
                    ..Default::default()
                },
            );
            if !inserted {
                s.push_diagnostic(
                    Diagnostic::error(format!("group \"{name}\" is defined more than once"))
                        .with_span(span),
                );
            }
            GroupBuilder {
                state: s.clone(),
                name: name.into(),
                pos,
                is_duplicate: !inserted,
            }
        },
    );

    builder_method!(
        engine,
        "target",
        GroupBuilder,
        |state, model, name, pos, target: &str| {
            let _ = (state, pos);
            if let Some(h) = model.groups.lookup(name) {
                if let Some(g) = model.groups.get_mut(h) {
                    g.target = target.into();
                }
            }
        }
    );

    builder_method!(
        engine,
        "edition",
        GroupBuilder,
        |state, model, name, pos, edition: &str| {
            let _ = (state, pos);
            if let Some(h) = model.groups.lookup(name) {
                if let Some(g) = model.groups.get_mut(h) {
                    g.default_edition = edition.into();
                }
            }
        }
    );

    builder_method!(
        engine,
        "project",
        GroupBuilder,
        |state, model, name, pos, is_project: bool| {
            let _ = (state, pos);
            if let Some(h) = model.groups.lookup(name) {
                if let Some(g) = model.groups.get_mut(h) {
                    g.is_project = is_project;
                }
            }
        }
    );

    builder_method!(
        engine,
        "config",
        GroupBuilder,
        |state, model, name, pos, has_config: bool| {
            let _ = (state, pos);
            if let Some(h) = model.groups.lookup(name) {
                if let Some(g) = model.groups.get_mut(h) {
                    g.config = has_config;
                }
            }
        }
    );

    // group.add(name, path) → CrateBuilder. Not macro-eligible because it
    // returns a different builder type.
    engine.register_fn(
        "add",
        |ctx: NativeCallContext,
         builder: &mut GroupBuilder,
         name: &str,
         path: &str|
         -> CrateBuilder {
            let pos = ctx.call_position();
            // If the parent group was a duplicate, propagate the flag so any
            // chained methods on the returned CrateBuilder also no-op. We do
            // not insert the crate either, since the group is not the one the
            // script author intended.
            if builder.is_duplicate {
                return CrateBuilder {
                    state: builder.state.clone(),
                    name: name.into(),
                    pos,
                    is_duplicate: true,
                };
            }
            let span = builder.state.span_from(pos);
            let state = builder.state.clone();

            // Resolve group-level defaults we need for CrateDef fields.
            let (edition, target, is_project, group_name) = {
                let model = state.model.borrow();
                let g = model
                    .groups
                    .lookup(&builder.name)
                    .and_then(|h| model.groups.get(h));
                match g {
                    Some(g) => (
                        g.default_edition.clone(),
                        g.target.clone(),
                        g.is_project,
                        g.name.clone(),
                    ),
                    None => (
                        "2024".to_string(),
                        "host".to_string(),
                        true,
                        builder.name.clone(),
                    ),
                }
            };

            let new_crate = CrateDef {
                name: name.into(),
                path: path.into(),
                edition,
                crate_type: CrateType::Lib,
                target,
                target_handle: None,
                deps: Default::default(),
                dev_deps: Default::default(),
                features: Vec::new(),
                root: None,
                linker_script: None,
                group: group_name.clone(),
                group_handle: None,
                is_project_crate: is_project,
                cfg_flags: Vec::new(),
                rustc_flags: Vec::new(),
                requires_config: Vec::new(),
                artifact_deps: Vec::new(),
                span: Some(span.clone()),
            };

            let (_, inserted) = state
                .model
                .borrow_mut()
                .crates
                .insert(name.into(), new_crate);
            let crate_is_duplicate = !inserted;
            if !inserted {
                state.push_diagnostic(
                    Diagnostic::error(format!("crate \"{name}\" is defined more than once"))
                        .with_span(span),
                );
            } else {
                // Record the crate under its group for later resolution.
                let mut model = state.model.borrow_mut();
                if let Some(h) = model.groups.lookup(&group_name) {
                    if let Some(g) = model.groups.get_mut(h) {
                        g.crates.push(name.to_string());
                    }
                }
            }

            CrateBuilder {
                state,
                name: name.into(),
                pos,
                is_duplicate: crate_is_duplicate,
            }
        },
    );
}

// ---------------------------------------------------------------------------
// CrateBuilder methods
// ---------------------------------------------------------------------------

fn register_crate_methods(engine: &mut Engine) {
    builder_method!(
        engine,
        "edition",
        CrateBuilder,
        |state, model, name, pos, edition: &str| {
            let _ = (state, pos);
            if let Some(h) = model.crates.lookup(name) {
                if let Some(k) = model.crates.get_mut(h) {
                    k.edition = edition.into();
                }
            }
        }
    );

    builder_method!(
        engine,
        "root",
        CrateBuilder,
        |state, model, name, pos, root: &str| {
            let _ = (state, pos);
            if let Some(h) = model.crates.lookup(name) {
                if let Some(k) = model.crates.get_mut(h) {
                    k.root = Some(root.into());
                }
            }
        }
    );

    builder_method!(
        engine,
        "target",
        CrateBuilder,
        |state, model, name, pos, target: &str| {
            let _ = (state, pos);
            if let Some(h) = model.crates.lookup(name) {
                if let Some(k) = model.crates.get_mut(h) {
                    k.target = target.into();
                }
            }
        }
    );

    builder_method!(
        engine,
        "linker_script",
        CrateBuilder,
        |state, model, name, pos, script: &str| {
            let _ = (state, pos);
            if let Some(h) = model.crates.lookup(name) {
                if let Some(k) = model.crates.get_mut(h) {
                    k.linker_script = Some(script.into());
                }
            }
        }
    );

    builder_method!(
        engine,
        "requires_config",
        CrateBuilder,
        |state, model, name, pos, cfg: &str| {
            let _ = (state, pos);
            if let Some(h) = model.crates.lookup(name) {
                if let Some(k) = model.crates.get_mut(h) {
                    k.requires_config.push(cfg.into());
                }
            }
        }
    );

    builder_method!(
        engine,
        "features",
        CrateBuilder,
        |state, model, name, pos, list: rhai::Array| {
            let _ = pos;
            match array_to_string_vec(list) {
                Ok(v) => {
                    if let Some(h) = model.crates.lookup(name) {
                        if let Some(k) = model.crates.get_mut(h) {
                            k.features = v;
                        }
                    }
                }
                Err(msg) => state.push_diagnostic(
                    Diagnostic::error(format!("crate '{name}' features: {msg}"))
                        .with_span(state.span_from(pos)),
                ),
            }
        }
    );

    builder_method!(
        engine,
        "rustc_flags",
        CrateBuilder,
        |state, model, name, pos, list: rhai::Array| {
            let _ = pos;
            match array_to_string_vec(list) {
                Ok(v) => {
                    if let Some(h) = model.crates.lookup(name) {
                        if let Some(k) = model.crates.get_mut(h) {
                            k.rustc_flags = v;
                        }
                    }
                }
                Err(msg) => state.push_diagnostic(
                    Diagnostic::error(format!("crate '{name}' rustc_flags: {msg}"))
                        .with_span(state.span_from(pos)),
                ),
            }
        }
    );

    builder_method!(
        engine,
        "cfg_flags",
        CrateBuilder,
        |state, model, name, pos, list: rhai::Array| {
            let _ = pos;
            match array_to_string_vec(list) {
                Ok(v) => {
                    if let Some(h) = model.crates.lookup(name) {
                        if let Some(k) = model.crates.get_mut(h) {
                            k.cfg_flags = v;
                        }
                    }
                }
                Err(msg) => state.push_diagnostic(
                    Diagnostic::error(format!("crate '{name}' cfg_flags: {msg}"))
                        .with_span(state.span_from(pos)),
                ),
            }
        }
    );

    builder_method!(
        engine,
        "artifact_deps",
        CrateBuilder,
        |state, model, name, pos, list: rhai::Array| {
            let _ = pos;
            match array_to_string_vec(list) {
                Ok(v) => {
                    if let Some(h) = model.crates.lookup(name) {
                        if let Some(k) = model.crates.get_mut(h) {
                            k.artifact_deps = v;
                        }
                    }
                }
                Err(msg) => state.push_diagnostic(
                    Diagnostic::error(format!("crate '{name}' artifact_deps: {msg}"))
                        .with_span(state.span_from(pos)),
                ),
            }
        }
    );

    builder_method!(
        engine,
        "crate_type",
        CrateBuilder,
        |state, model, name, pos, ty: Dynamic| {
            let parsed = if let Some(i) = ty.clone().try_cast::<i64>() {
                crate_type_from_i64(i)
            } else if let Ok(s) = ty.into_string() {
                crate_type_from_str(&s)
            } else {
                None
            };
            match parsed {
                Some(ct) => {
                    if let Some(h) = model.crates.lookup(name) {
                        if let Some(k) = model.crates.get_mut(h) {
                            k.crate_type = ct;
                        }
                    }
                }
                None => state.push_diagnostic(
                    Diagnostic::error(format!(
                        "crate '{name}' crate_type must be one of LIB/BIN/PROC_MACRO/STATICLIB \
                     or a string \"lib\"/\"bin\"/\"proc-macro\"/\"staticlib\""
                    ))
                    .with_span(state.span_from(pos)),
                ),
            }
        }
    );

    builder_method!(
        engine,
        "deps",
        CrateBuilder,
        |state, model, name, pos, map: Map| {
            let parsed = parse_dep_map(state, pos, map);
            if let Some(h) = model.crates.lookup(name) {
                if let Some(k) = model.crates.get_mut(h) {
                    for (extern_name, dep) in parsed {
                        k.deps.insert(extern_name, dep);
                    }
                }
            }
        }
    );

    builder_method!(
        engine,
        "dev_deps",
        CrateBuilder,
        |state, model, name, pos, map: Map| {
            let parsed = parse_dep_map(state, pos, map);
            if let Some(h) = model.crates.lookup(name) {
                if let Some(k) = model.crates.get_mut(h) {
                    for (extern_name, dep) in parsed {
                        k.dev_deps.insert(extern_name, dep);
                    }
                }
            }
        }
    );
}

// ---------------------------------------------------------------------------
// dependency()
// ---------------------------------------------------------------------------

fn register_dependency(engine: &mut Engine, state: EngineState) {
    let s = state;
    engine.register_fn(
        "dependency",
        move |ctx: NativeCallContext, name: &str| -> DependencyBuilder {
            let pos = ctx.call_position();
            let span = s.span_from(pos);
            let (_, inserted) = s.model.borrow_mut().external_deps.insert(
                name.into(),
                ExternalDepDef {
                    name: name.into(),
                    source: DepSource::CratesIo {
                        version: String::new(),
                    },
                    features: Vec::new(),
                    default_features: true,
                    cfg_flags: Vec::new(),
                    rustc_flags: Vec::new(),
                    span: Some(span.clone()),
                },
            );
            if !inserted {
                s.push_diagnostic(
                    Diagnostic::error(format!("dependency \"{name}\" is defined more than once"))
                        .with_span(span),
                );
            }
            DependencyBuilder {
                state: s.clone(),
                name: name.into(),
                pos,
                is_duplicate: !inserted,
            }
        },
    );

    builder_method!(
        engine,
        "version",
        DependencyBuilder,
        |state, model, name, pos, version: &str| {
            let _ = (state, pos);
            if let Some(h) = model.external_deps.lookup(name) {
                if let Some(d) = model.external_deps.get_mut(h) {
                    d.source = DepSource::CratesIo {
                        version: version.into(),
                    };
                }
            }
        }
    );

    builder_method!(
        engine,
        "features",
        DependencyBuilder,
        |state, model, name, pos, list: rhai::Array| {
            match array_to_string_vec(list) {
                Ok(v) => {
                    if let Some(h) = model.external_deps.lookup(name) {
                        if let Some(d) = model.external_deps.get_mut(h) {
                            d.features = v;
                        }
                    }
                }
                Err(msg) => state.push_diagnostic(
                    Diagnostic::error(format!("dependency '{name}' features: {msg}"))
                        .with_span(state.span_from(pos)),
                ),
            }
        }
    );

    builder_method!(
        engine,
        "no_default_features",
        DependencyBuilder,
        |state, model, name, pos| {
            let _ = (state, pos);
            if let Some(h) = model.external_deps.lookup(name) {
                if let Some(d) = model.external_deps.get_mut(h) {
                    d.default_features = false;
                }
            }
        }
    );

    builder_method!(
        engine,
        "cfg_flags",
        DependencyBuilder,
        |state, model, name, pos, list: rhai::Array| {
            match array_to_string_vec(list) {
                Ok(v) => {
                    if let Some(h) = model.external_deps.lookup(name) {
                        if let Some(d) = model.external_deps.get_mut(h) {
                            d.cfg_flags = v;
                        }
                    }
                }
                Err(msg) => state.push_diagnostic(
                    Diagnostic::error(format!("dependency '{name}' cfg_flags: {msg}"))
                        .with_span(state.span_from(pos)),
                ),
            }
        }
    );

    builder_method!(
        engine,
        "rustc_flags",
        DependencyBuilder,
        |state, model, name, pos, list: rhai::Array| {
            match array_to_string_vec(list) {
                Ok(v) => {
                    if let Some(h) = model.external_deps.lookup(name) {
                        if let Some(d) = model.external_deps.get_mut(h) {
                            d.rustc_flags = v;
                        }
                    }
                }
                Err(msg) => state.push_diagnostic(
                    Diagnostic::error(format!("dependency '{name}' rustc_flags: {msg}"))
                        .with_span(state.span_from(pos)),
                ),
            }
        }
    );
}
