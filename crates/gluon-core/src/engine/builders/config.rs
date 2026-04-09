//! Config option builders: `config_bool`, `config_tristate`, `config_u32`,
//! `config_u64`, `config_str`, `config_choice`, `config_list`, and `preset`.

use super::builder_method;
use crate::engine::EngineState;
use crate::engine::conversions::{
    array_to_string_vec, dynamic_to_config_value, dynamic_to_config_value_best_effort,
};
use crate::error::Diagnostic;
use crate::kconfig::parse_bool_expr;
use gluon_model::{Binding, ConfigOptionDef, ConfigType, ConfigValue, Expr, PresetDef};
use rhai::{Dynamic, Engine, NativeCallContext, Position};

// ---------------------------------------------------------------------------
// Builder types
// ---------------------------------------------------------------------------

/// Chainable builder returned by the `config_*` family of functions.
#[derive(Clone)]
pub struct ConfigOptionBuilder {
    state: EngineState,
    name: String,
    pos: Position,
    /// Captured at construction so `.default()` can type-check the value.
    expected_type: ConfigType,
    /// `true` if this builder was returned from a duplicate definition; when
    /// set, all chained methods no-op (matches the pattern used by the
    /// model builders in Chunk 2).
    is_duplicate: bool,
}

/// Chainable builder returned by `preset("name")`.
#[derive(Clone)]
pub struct PresetBuilder {
    state: EngineState,
    name: String,
    pos: Position,
    is_duplicate: bool,
}

// ---------------------------------------------------------------------------
// Registration entry point
// ---------------------------------------------------------------------------

pub(super) fn register(engine: &mut Engine, state: &EngineState) {
    register_config_ctors(engine, state.clone());
    register_config_methods(engine);
    register_preset(engine, state.clone());
}

// ---------------------------------------------------------------------------
// config_* constructors
// ---------------------------------------------------------------------------

fn register_config_ctors(engine: &mut Engine, state: EngineState) {
    fn make_ctor(engine: &mut Engine, state: EngineState, fn_name: &'static str, ty: ConfigType) {
        let s = state;
        engine.register_fn(
            fn_name,
            move |ctx: NativeCallContext, name: &str| -> ConfigOptionBuilder {
                let pos = ctx.call_position();
                let span = s.span_from(pos);

                let is_duplicate = {
                    let mut model = s.model.borrow_mut();
                    if model.config_options.contains_key(name) {
                        s.push_diagnostic(
                            Diagnostic::error(format!(
                                "config option '{name}' is defined more than once"
                            ))
                            .with_span(span.clone()),
                        );
                        true
                    } else {
                        model.config_options.insert(
                            name.into(),
                            ConfigOptionDef {
                                name: name.into(),
                                ty,
                                default: default_value_for(ty),
                                help: None,
                                selects: Vec::new(),
                                range: None,
                                choices: None,
                                menu: None,
                                bindings: Vec::new(),
                                span: Some(span.clone()),
                                depends_on_expr: None,
                                visible_if_expr: None,
                            },
                        );
                        false
                    }
                };

                ConfigOptionBuilder {
                    state: s.clone(),
                    name: name.into(),
                    pos,
                    expected_type: ty,
                    is_duplicate,
                }
            },
        );
    }

    make_ctor(engine, state.clone(), "config_bool", ConfigType::Bool);
    make_ctor(
        engine,
        state.clone(),
        "config_tristate",
        ConfigType::Tristate,
    );
    make_ctor(engine, state.clone(), "config_u32", ConfigType::U32);
    make_ctor(engine, state.clone(), "config_u64", ConfigType::U64);
    make_ctor(engine, state.clone(), "config_str", ConfigType::Str);
    make_ctor(engine, state.clone(), "config_choice", ConfigType::Choice);
    make_ctor(engine, state, "config_list", ConfigType::List);
}

/// Neutral initial value for each config type. Overridden by `.default(...)`
/// on the builder.
/// AND-merge `added` into an optional `Expr` slot, flattening when the
/// existing value is already an `And(...)` so repeated
/// `.depends_on(...)` / `.depends_on_expr(...)` calls fold into a single
/// level rather than nesting. Matches the cumulative-AND semantics of
/// the legacy flat `Vec<String>` append path.
fn and_into(slot: &mut Option<Expr>, added: Expr) {
    *slot = Some(match slot.take() {
        None => added,
        Some(Expr::And(mut xs)) => {
            xs.push(added);
            Expr::And(xs)
        }
        Some(other) => Expr::And(vec![other, added]),
    });
}

fn default_value_for(ty: ConfigType) -> ConfigValue {
    match ty {
        ConfigType::Bool => ConfigValue::Bool(false),
        ConfigType::Tristate => ConfigValue::Tristate(gluon_model::TristateVal::No),
        ConfigType::U32 => ConfigValue::U32(0),
        ConfigType::U64 => ConfigValue::U64(0),
        ConfigType::Str => ConfigValue::Str(String::new()),
        ConfigType::Choice => ConfigValue::Choice(String::new()),
        ConfigType::List => ConfigValue::List(Vec::new()),
        // Group-typed options never carry a value; pick a harmless placeholder.
        ConfigType::Group => ConfigValue::Bool(false),
    }
}

// ---------------------------------------------------------------------------
// ConfigOptionBuilder chainable methods
// ---------------------------------------------------------------------------

fn register_config_methods(engine: &mut Engine) {
    // `.default_value(value)` — typed against the captured expected_type.
    //
    // Note: the method is named `default_value` rather than the more natural
    // `default` because Rhai reserves `default` as a keyword and forbids it
    // as a property name even when [`Engine::disable_symbol`] is called.
    engine.register_fn(
        "default_value",
        |builder: &mut ConfigOptionBuilder, value: Dynamic| -> ConfigOptionBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            let state = builder.state.clone();
            let name = builder.name.clone();
            let expected = builder.expected_type;
            let pos = builder.pos;
            if let Some(cv) = dynamic_to_config_value(&state, pos, &name, &expected, value) {
                let mut model = state.model.borrow_mut();
                if let Some(opt) = model.config_options.get_mut(&name) {
                    opt.default = cv;
                }
            }
            builder.clone()
        },
    );

    builder_method!(
        engine,
        "help",
        ConfigOptionBuilder,
        |state, model, name, pos, text: &str| {
            let _ = (state, pos);
            if let Some(opt) = model.config_options.get_mut(name) {
                opt.help = Some(text.into());
            }
        }
    );

    // `.description(text)` — the model has no dedicated description field;
    // fold into `help` (prepending, if help is already set).
    builder_method!(
        engine,
        "description",
        ConfigOptionBuilder,
        |state, model, name, pos, text: &str| {
            let _ = (state, pos);
            if let Some(opt) = model.config_options.get_mut(name) {
                opt.help = Some(match opt.help.take() {
                    Some(existing) => format!("{text}\n\n{existing}"),
                    None => text.into(),
                });
            }
        }
    );

    builder_method!(
        engine,
        "menu",
        ConfigOptionBuilder,
        |state, model, name, pos, text: &str| {
            let _ = (state, pos);
            if let Some(opt) = model.config_options.get_mut(name) {
                opt.menu = Some(text.into());
            }
        }
    );

    // depends_on: accept either a single string or an array of strings.
    //
    // Both forms AND-fold into `depends_on_expr`, matching what the
    // `.kconfig` loader produces for an equivalent `depends_on = A && B`
    // clause. For anything beyond AND-of-names (e.g. `A || !B`), use
    // `.depends_on_expr("...")` below.
    engine.register_fn(
        "depends_on",
        |builder: &mut ConfigOptionBuilder, dep: rhai::ImmutableString| -> ConfigOptionBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            let mut model = builder.state.model.borrow_mut();
            if let Some(opt) = model.config_options.get_mut(&builder.name) {
                and_into(&mut opt.depends_on_expr, Expr::Ident(dep.to_string()));
            }
            builder.clone()
        },
    );
    builder_method!(
        engine,
        "depends_on",
        ConfigOptionBuilder,
        |state, model, name, pos, list: rhai::Array| {
            match array_to_string_vec(list) {
                Ok(v) => {
                    if let Some(opt) = model.config_options.get_mut(name) {
                        for dep in v {
                            and_into(&mut opt.depends_on_expr, Expr::Ident(dep));
                        }
                    }
                }
                Err(msg) => state.push_diagnostic(
                    Diagnostic::error(format!("config option '{name}' depends_on: {msg}"))
                        .with_span(state.span_from(pos)),
                ),
            }
        }
    );

    // depends_on_expr: accept a mini boolean-expression string using the
    // same grammar the `.kconfig` parser accepts for `depends_on`/
    // `visible_if` property values (`&&`, `||`, `!`, `(...)`). Reuses
    // `kconfig::parse_bool_expr` so there is exactly one grammar across
    // both declaration surfaces.
    builder_method!(
        engine,
        "depends_on_expr",
        ConfigOptionBuilder,
        |state, model, name, pos, expr_src: rhai::ImmutableString| {
            let origin = state.span_from(pos);
            match parse_bool_expr(expr_src.as_str(), origin) {
                Ok(expr) => {
                    if let Some(opt) = model.config_options.get_mut(name) {
                        and_into(&mut opt.depends_on_expr, expr);
                    }
                }
                Err(d) => state.push_diagnostic(
                    Diagnostic::error(format!(
                        "config option '{name}' depends_on_expr: {}",
                        d.message
                    ))
                    .with_optional_span(d.span),
                ),
            }
        }
    );

    // selects: single or array.
    engine.register_fn(
        "selects",
        |builder: &mut ConfigOptionBuilder, sel: rhai::ImmutableString| -> ConfigOptionBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            let mut model = builder.state.model.borrow_mut();
            if let Some(opt) = model.config_options.get_mut(&builder.name) {
                opt.selects.push(sel.to_string());
            }
            builder.clone()
        },
    );
    builder_method!(
        engine,
        "selects",
        ConfigOptionBuilder,
        |state, model, name, pos, list: rhai::Array| {
            match array_to_string_vec(list) {
                Ok(mut v) => {
                    if let Some(opt) = model.config_options.get_mut(name) {
                        opt.selects.append(&mut v);
                    }
                }
                Err(msg) => state.push_diagnostic(
                    Diagnostic::error(format!("config option '{name}' selects: {msg}"))
                        .with_span(state.span_from(pos)),
                ),
            }
        }
    );

    // .range(min, max) — only meaningful on numeric options.
    builder_method!(
        engine,
        "range",
        ConfigOptionBuilder,
        |state, model, name, pos, min: i64, max: i64| {
            if min < 0 || max < 0 || min > max {
                state.push_diagnostic(
                    Diagnostic::error(format!(
                        "config option '{name}': invalid range ({min}..={max})"
                    ))
                    .with_span(state.span_from(pos)),
                );
            } else if let Some(opt) = model.config_options.get_mut(name) {
                match opt.ty {
                    ConfigType::U32 | ConfigType::U64 => {
                        opt.range = Some((min as u64, max as u64));
                    }
                    _ => state.push_diagnostic(
                        Diagnostic::error(format!(
                            "config option '{name}': .range() only applies to numeric (U32/U64) options"
                        ))
                        .with_span(state.span_from(pos)),
                    ),
                }
            }
        }
    );

    // .choices([...]) — only meaningful on Choice options.
    builder_method!(
        engine,
        "choices",
        ConfigOptionBuilder,
        |state, model, name, pos, list: rhai::Array| {
            match array_to_string_vec(list) {
                Ok(v) => {
                    if let Some(opt) = model.config_options.get_mut(name) {
                        if opt.ty == ConfigType::Choice {
                            opt.choices = Some(v);
                        } else {
                            state.push_diagnostic(
                                Diagnostic::error(format!(
                                    "config option '{name}': .choices() only applies to Choice options"
                                ))
                                .with_span(state.span_from(pos)),
                            );
                        }
                    }
                }
                Err(msg) => state.push_diagnostic(
                    Diagnostic::error(format!("config option '{name}' choices: {msg}"))
                        .with_span(state.span_from(pos)),
                ),
            }
        }
    );

    // Binding setters: zero-arg methods that append a Binding variant to
    // `bindings`. Multiple bindings on one option are allowed by the model.
    fn register_binding(engine: &mut Engine, fn_name: &'static str, binding: Binding) {
        let binding_clone = binding.clone();
        engine.register_fn(
            fn_name,
            move |builder: &mut ConfigOptionBuilder| -> ConfigOptionBuilder {
                if builder.is_duplicate {
                    return builder.clone();
                }
                let mut model = builder.state.model.borrow_mut();
                if let Some(opt) = model.config_options.get_mut(&builder.name) {
                    opt.bindings.push(binding_clone.clone());
                }
                builder.clone()
            },
        );
    }
    register_binding(engine, "binding_cfg", Binding::Cfg);
    register_binding(engine, "binding_cfg_cumulative", Binding::CfgCumulative);
    register_binding(engine, "binding_const", Binding::Const);
    register_binding(engine, "binding_build", Binding::Build);
}

// ---------------------------------------------------------------------------
// preset()
// ---------------------------------------------------------------------------

fn register_preset(engine: &mut Engine, state: EngineState) {
    let s = state;
    engine.register_fn(
        "preset",
        move |ctx: NativeCallContext, name: &str| -> PresetBuilder {
            let pos = ctx.call_position();
            let span = s.span_from(pos);

            let is_duplicate = {
                let mut model = s.model.borrow_mut();
                if model.presets.contains_key(name) {
                    s.push_diagnostic(
                        Diagnostic::error(format!("preset '{name}' is defined more than once"))
                            .with_span(span),
                    );
                    true
                } else {
                    model.presets.insert(
                        name.into(),
                        PresetDef {
                            name: name.into(),
                            inherits: None,
                            help: None,
                            overrides: Default::default(),
                        },
                    );
                    false
                }
            };

            PresetBuilder {
                state: s.clone(),
                name: name.into(),
                pos,
                is_duplicate,
            }
        },
    );

    // .set(option_name, value) — value is best-effort typed; resolve re-types.
    engine.register_fn(
        "set",
        |builder: &mut PresetBuilder, option_name: &str, value: Dynamic| -> PresetBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            let state = builder.state.clone();
            if let Some(cv) = dynamic_to_config_value_best_effort(
                &state,
                builder.pos,
                &builder.name,
                option_name,
                value,
            ) {
                let mut model = state.model.borrow_mut();
                if let Some(p) = model.presets.get_mut(&builder.name) {
                    p.overrides.insert(option_name.into(), cv);
                }
            }
            builder.clone()
        },
    );

    builder_method!(
        engine,
        "inherits",
        PresetBuilder,
        |state, model, name, pos, parent: &str| {
            let _ = (state, pos);
            if let Some(p) = model.presets.get_mut(name) {
                p.inherits = Some(parent.into());
            }
        }
    );

    builder_method!(
        engine,
        "help",
        PresetBuilder,
        |state, model, name, pos, text: &str| {
            let _ = (state, pos);
            if let Some(p) = model.presets.get_mut(name) {
                p.help = Some(text.into());
            }
        }
    );
}
