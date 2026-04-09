//! Rule, pipeline, qemu, and bootloader builders.

use super::builder_method;
use crate::engine::EngineState;
use crate::engine::conversions::array_to_string_vec;
use crate::error::Diagnostic;
use gluon_model::{PipelineDef, PipelineStep, RuleDef, RuleHandler};
use rhai::{Dynamic, Engine, NativeCallContext, Position};

/// Default name for the project-level singleton pipeline. The script calls
/// `pipeline()` with no arguments and implicitly refers to this one.
const DEFAULT_PIPELINE_NAME: &str = "main";

// ---------------------------------------------------------------------------
// Builder types
// ---------------------------------------------------------------------------

/// Chainable builder returned by `rule("name")`.
#[derive(Clone)]
pub struct RuleBuilder {
    state: EngineState,
    name: String,
    pos: Position,
    is_duplicate: bool,
}

/// Chainable builder returned by `pipeline()`. The pipeline is a singleton
/// keyed by [`DEFAULT_PIPELINE_NAME`]; calling `pipeline()` more than once
/// returns a builder pointing at the same pipeline (not a duplicate).
#[derive(Clone)]
pub struct PipelineBuilder {
    state: EngineState,
    name: String,
    pos: Position,
    /// Always `false` for the singleton pipeline; retained for macro
    /// compatibility with the `builder_method!` short-circuit.
    is_duplicate: bool,
}

/// Chainable builder returned by `qemu()`. Stub: all values are stored in
/// [`gluon_model::QemuDef::extras`] as stringified entries for later
/// consumption.
#[derive(Clone)]
pub struct QemuBuilder {
    state: EngineState,
    pos: Position,
    is_duplicate: bool,
}

/// Chainable builder returned by `bootloader(kind)`. Stub: values go into
/// [`gluon_model::BootloaderDef::extras`].
#[derive(Clone)]
pub struct BootloaderBuilder {
    state: EngineState,
    pos: Position,
    is_duplicate: bool,
}

// ---------------------------------------------------------------------------
// Registration entry point
// ---------------------------------------------------------------------------

pub(super) fn register(engine: &mut Engine, state: &EngineState) {
    register_rule(engine, state.clone());
    register_pipeline(engine, state.clone());
    register_qemu(engine, state.clone());
    register_bootloader(engine, state.clone());
}

// ---------------------------------------------------------------------------
// rule()
// ---------------------------------------------------------------------------

fn register_rule(engine: &mut Engine, state: EngineState) {
    let s = state;
    engine.register_fn(
        "rule",
        move |ctx: NativeCallContext, name: &str| -> RuleBuilder {
            let pos = ctx.call_position();
            let span = s.span_from(pos);
            let (_, inserted) = s.model.borrow_mut().rules.insert(
                name.into(),
                RuleDef {
                    name: name.into(),
                    inputs: Vec::new(),
                    outputs: Vec::new(),
                    depends_on: Vec::new(),
                    handler: RuleHandler::Builtin("exec".into()),
                    span: Some(span.clone()),
                },
            );
            if !inserted {
                s.push_diagnostic(
                    Diagnostic::error(format!("rule '{name}' is defined more than once"))
                        .with_span(span),
                );
            }
            RuleBuilder {
                state: s.clone(),
                name: name.into(),
                pos,
                is_duplicate: !inserted,
            }
        },
    );

    builder_method!(
        engine,
        "handler",
        RuleBuilder,
        |state, model, name, pos, handler: &str| {
            let _ = (state, pos);
            if let Some(h) = model.rules.lookup(name) {
                if let Some(r) = model.rules.get_mut(h) {
                    r.handler = RuleHandler::Builtin(handler.into());
                }
            }
        }
    );

    builder_method!(
        engine,
        "on_execute",
        RuleBuilder,
        |state, model, name, pos, script: &str| {
            let _ = (state, pos);
            if let Some(h) = model.rules.lookup(name) {
                if let Some(r) = model.rules.get_mut(h) {
                    r.handler = RuleHandler::Script(script.into());
                }
            }
        }
    );

    builder_method!(
        engine,
        "inputs",
        RuleBuilder,
        |state, model, name, pos, list: rhai::Array| {
            match array_to_string_vec(list) {
                Ok(mut v) => {
                    if let Some(h) = model.rules.lookup(name) {
                        if let Some(r) = model.rules.get_mut(h) {
                            r.inputs.append(&mut v);
                        }
                    }
                }
                Err(msg) => state.push_diagnostic(
                    Diagnostic::error(format!("rule '{name}' inputs: {msg}"))
                        .with_span(state.span_from(pos)),
                ),
            }
        }
    );

    builder_method!(
        engine,
        "outputs",
        RuleBuilder,
        |state, model, name, pos, list: rhai::Array| {
            match array_to_string_vec(list) {
                Ok(mut v) => {
                    if let Some(h) = model.rules.lookup(name) {
                        if let Some(r) = model.rules.get_mut(h) {
                            r.outputs.append(&mut v);
                        }
                    }
                }
                Err(msg) => state.push_diagnostic(
                    Diagnostic::error(format!("rule '{name}' outputs: {msg}"))
                        .with_span(state.span_from(pos)),
                ),
            }
        }
    );

    builder_method!(
        engine,
        "output",
        RuleBuilder,
        |state, model, name, pos, single: &str| {
            let _ = (state, pos);
            if let Some(h) = model.rules.lookup(name) {
                if let Some(r) = model.rules.get_mut(h) {
                    r.outputs.push(single.into());
                }
            }
        }
    );

    builder_method!(
        engine,
        "depends_on",
        RuleBuilder,
        |state, model, name, pos, list: rhai::Array| {
            match array_to_string_vec(list) {
                Ok(mut v) => {
                    if let Some(h) = model.rules.lookup(name) {
                        if let Some(r) = model.rules.get_mut(h) {
                            r.depends_on.append(&mut v);
                        }
                    }
                }
                Err(msg) => state.push_diagnostic(
                    Diagnostic::error(format!("rule '{name}' depends_on: {msg}"))
                        .with_span(state.span_from(pos)),
                ),
            }
        }
    );
}

// ---------------------------------------------------------------------------
// pipeline() — singleton
// ---------------------------------------------------------------------------

fn register_pipeline(engine: &mut Engine, state: EngineState) {
    let s = state;
    engine.register_fn(
        "pipeline",
        move |ctx: NativeCallContext| -> PipelineBuilder {
            let pos = ctx.call_position();
            let span = s.span_from(pos);
            {
                let mut model = s.model.borrow_mut();
                if model.pipelines.lookup(DEFAULT_PIPELINE_NAME).is_none() {
                    model.pipelines.insert(
                        DEFAULT_PIPELINE_NAME.into(),
                        PipelineDef {
                            name: DEFAULT_PIPELINE_NAME.into(),
                            stages: Vec::new(),
                            span: Some(span),
                        },
                    );
                }
            }
            PipelineBuilder {
                state: s.clone(),
                name: DEFAULT_PIPELINE_NAME.into(),
                pos,
                is_duplicate: false,
            }
        },
    );

    builder_method!(
        engine,
        "stage",
        PipelineBuilder,
        |state, model, name, pos, stage_name: &str, inputs: rhai::Array| {
            match array_to_string_vec(inputs) {
                Ok(v) => {
                    if let Some(h) = model.pipelines.lookup(name) {
                        if let Some(p) = model.pipelines.get_mut(h) {
                            p.stages.push(PipelineStep {
                                name: stage_name.into(),
                                inputs: v,
                                inputs_handles: Vec::new(),
                                rule: None,
                            });
                        }
                    }
                }
                Err(msg) => state.push_diagnostic(
                    Diagnostic::error(format!("pipeline stage '{stage_name}': {msg}"))
                        .with_span(state.span_from(pos)),
                ),
            }
        }
    );

    builder_method!(
        engine,
        "barrier",
        PipelineBuilder,
        |state, model, name, pos, barrier_name: &str| {
            let _ = (state, pos);
            if let Some(h) = model.pipelines.lookup(name) {
                if let Some(p) = model.pipelines.get_mut(h) {
                    p.stages.push(PipelineStep {
                        name: barrier_name.into(),
                        inputs: Vec::new(),
                        inputs_handles: Vec::new(),
                        rule: None,
                    });
                }
            }
        }
    );
}

// ---------------------------------------------------------------------------
// qemu() — stub, stores values in QemuDef.extras
// ---------------------------------------------------------------------------

fn register_qemu(engine: &mut Engine, state: EngineState) {
    let s = state;
    engine.register_fn("qemu", move |ctx: NativeCallContext| -> QemuBuilder {
        let pos = ctx.call_position();
        let span = s.span_from(pos);
        let mut defined = s.qemu_defined.borrow_mut();
        let is_duplicate = *defined;
        if is_duplicate {
            s.push_diagnostic(
                Diagnostic::error("qemu is defined more than once".to_string()).with_span(span),
            );
        } else {
            *defined = true;
        }
        QemuBuilder {
            state: s.clone(),
            pos,
            is_duplicate,
        }
    });

    fn set_extra(builder: &QemuBuilder, key: &str, val: String) {
        let mut model = builder.state.model.borrow_mut();
        model.qemu.extras.insert(key.into(), val);
    }

    engine.register_fn(
        "machine",
        |builder: &mut QemuBuilder, machine: &str| -> QemuBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            set_extra(builder, "machine", machine.into());
            builder.clone()
        },
    );
    engine.register_fn(
        "memory",
        |builder: &mut QemuBuilder, mb: i64| -> QemuBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            set_extra(builder, "memory", mb.to_string());
            builder.clone()
        },
    );
    engine.register_fn(
        "cores",
        |builder: &mut QemuBuilder, n: i64| -> QemuBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            set_extra(builder, "cores", n.to_string());
            builder.clone()
        },
    );
    engine.register_fn(
        "serial",
        |builder: &mut QemuBuilder, mode: &str| -> QemuBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            set_extra(builder, "serial", mode.into());
            builder.clone()
        },
    );
    engine.register_fn(
        "test_success",
        |builder: &mut QemuBuilder, value: &str| -> QemuBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            set_extra(builder, "test_success", value.into());
            builder.clone()
        },
    );
    engine.register_fn(
        "extra_args",
        |builder: &mut QemuBuilder, list: rhai::Array| -> QemuBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            match array_to_string_vec(list) {
                Ok(v) => set_extra(builder, "extra_args", v.join(" ")),
                Err(msg) => builder.state.push_diagnostic(
                    Diagnostic::error(format!("qemu.extra_args: {msg}"))
                        .with_span(builder.state.span_from(builder.pos)),
                ),
            }
            builder.clone()
        },
    );
}

// ---------------------------------------------------------------------------
// bootloader() — stub
// ---------------------------------------------------------------------------

fn register_bootloader(engine: &mut Engine, state: EngineState) {
    let s = state;
    engine.register_fn(
        "bootloader",
        move |ctx: NativeCallContext, kind: &str| -> BootloaderBuilder {
            let pos = ctx.call_position();
            let span = s.span_from(pos);
            let mut defined = s.bootloader_defined.borrow_mut();
            let is_duplicate = *defined;
            if is_duplicate {
                s.push_diagnostic(
                    Diagnostic::error("bootloader is defined more than once".to_string())
                        .with_span(span),
                );
            } else {
                *defined = true;
                let mut model = s.model.borrow_mut();
                model.bootloader = gluon_model::BootloaderDef {
                    kind: kind.into(),
                    extras: std::collections::BTreeMap::new(),
                };
            }
            BootloaderBuilder {
                state: s.clone(),
                pos,
                is_duplicate,
            }
        },
    );

    fn set_extra(builder: &BootloaderBuilder, key: &str, val: String) {
        let mut model = builder.state.model.borrow_mut();
        model.bootloader.extras.insert(key.into(), val);
    }

    engine.register_fn(
        "config_file",
        |builder: &mut BootloaderBuilder, file: &str| -> BootloaderBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            set_extra(builder, "config_file", file.into());
            builder.clone()
        },
    );

    // Generic string-valued .set(key, value) escape hatch for the stub.
    engine.register_fn(
        "set",
        |builder: &mut BootloaderBuilder, key: &str, value: Dynamic| -> BootloaderBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            let observed = value.type_name();
            match value.into_string() {
                Ok(s) => set_extra(builder, key, s),
                Err(_) => builder.state.push_diagnostic(
                    Diagnostic::error(format!(
                        "bootloader.set('{key}'): value must be a string, got {observed}"
                    ))
                    .with_span(builder.state.span_from(builder.pos)),
                ),
            }
            builder.clone()
        },
    );
}
