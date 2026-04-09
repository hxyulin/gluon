//! Conversions from Rhai dynamic values to model types.
//!
//! Centralises the strict parsing logic so the builder layer stays thin.

use super::EngineState;
use crate::error::Diagnostic;
use gluon_model::{ConfigType, ConfigValue, DepDef, TristateVal};
use rhai::{Dynamic, Map, Position};
use std::collections::BTreeMap;

/// Allowed keys inside an entry of a `.deps(#{ ... })` map.
const ALLOWED_DEP_KEYS: &[&str] = &["crate", "default_features", "features", "optional", "version"];

/// Parse a `.deps(#{ ... })` map strictly.
///
/// Each entry's value must be a `rhai::Map` with at minimum a `crate` key.
/// Unknown keys, missing `crate`, or wrong value types push diagnostics onto
/// `state` and cause the offending entry to be skipped. Well-formed entries
/// are returned keyed by extern name.
pub(super) fn parse_dep_map(
    state: &EngineState,
    pos: Position,
    map: Map,
) -> BTreeMap<String, DepDef> {
    let mut out = BTreeMap::new();
    let span = state.span_from(pos);

    for (extern_name, value) in map {
        let extern_name = extern_name.to_string();

        // Value must itself be a map.
        let inner: Map = match value.try_cast::<Map>() {
            Some(m) => m,
            None => {
                state.push_diagnostic(
                    Diagnostic::error(format!(
                        "dependency '{extern_name}' must be declared as an object map: \
                         #{{ crate: \"<name>\", ... }}"
                    ))
                    .with_span(span.clone()),
                );
                continue;
            }
        };

        let mut crate_name: Option<String> = None;
        let mut features: Vec<String> = Vec::new();
        let mut version: Option<String> = None;
        let mut optional = false;
        let mut default_features = true;
        let mut had_error = false;

        for (k, v) in &inner {
            let ks = k.as_str();
            if !ALLOWED_DEP_KEYS.contains(&ks) {
                state.push_diagnostic(
                    Diagnostic::error(format!(
                        "unknown dep option '{ks}' (allowed: crate, default_features, features, optional, version)"
                    ))
                    .with_span(span.clone()),
                );
                had_error = true;
                continue;
            }
            match ks {
                "crate" => match v.clone().into_string() {
                    Ok(s) => crate_name = Some(s),
                    Err(_) => {
                        state.push_diagnostic(
                            Diagnostic::error(format!(
                                "dep '{extern_name}' field 'crate' must be a string"
                            ))
                            .with_span(span.clone()),
                        );
                        had_error = true;
                    }
                },
                "features" => {
                    if let Some(arr) = v.clone().try_cast::<rhai::Array>() {
                        match array_to_string_vec(arr) {
                            Ok(f) => features = f,
                            Err(msg) => {
                                state.push_diagnostic(
                                    Diagnostic::error(format!(
                                        "dep '{extern_name}' field 'features': {msg}"
                                    ))
                                    .with_span(span.clone()),
                                );
                                had_error = true;
                            }
                        }
                    } else {
                        state.push_diagnostic(
                            Diagnostic::error(format!(
                                "dep '{extern_name}' field 'features' must be an array of strings"
                            ))
                            .with_span(span.clone()),
                        );
                        had_error = true;
                    }
                }
                "version" => match v.clone().into_string() {
                    Ok(s) => version = Some(s),
                    Err(_) => {
                        state.push_diagnostic(
                            Diagnostic::error(format!(
                                "dep '{extern_name}' field 'version' must be a string"
                            ))
                            .with_span(span.clone()),
                        );
                        had_error = true;
                    }
                },
                "optional" => match v.as_bool() {
                    Ok(b) => optional = b,
                    Err(_) => {
                        state.push_diagnostic(
                            Diagnostic::error(format!(
                                "dep '{extern_name}' field 'optional' must be a bool"
                            ))
                            .with_span(span.clone()),
                        );
                        had_error = true;
                    }
                },
                "default_features" => match v.as_bool() {
                    Ok(b) => default_features = b,
                    Err(_) => {
                        state.push_diagnostic(
                            Diagnostic::error(format!(
                                "dep '{extern_name}' field 'default_features' must be a bool"
                            ))
                            .with_span(span.clone()),
                        );
                        had_error = true;
                    }
                },
                _ => unreachable!("ALLOWED_DEP_KEYS check above"),
            }
        }

        let crate_name = match crate_name {
            Some(c) => c,
            None => {
                state.push_diagnostic(
                    Diagnostic::error(format!(
                        "dep '{extern_name}' is missing required 'crate' field"
                    ))
                    .with_span(span.clone()),
                );
                continue;
            }
        };

        if had_error {
            // At least one sub-field was malformed; skip this entry so the
            // intern/validate pass doesn't also complain about it.
            continue;
        }

        out.insert(
            extern_name,
            DepDef {
                crate_name,
                crate_handle: None,
                features,
                version,
                optional,
                default_features,
                span: Some(span.clone()),
            },
        );
    }

    out
}

/// Strict-convert a Rhai [`Dynamic`] into a typed [`ConfigValue`] for the
/// given [`ConfigType`].
///
/// Unlike the upstream fall-through conversion, this is *typed by expectation*:
/// the caller declares which `ConfigType` is expected and the function pushes
/// a diagnostic and returns `None` if the dynamic doesn't match.
pub(super) fn dynamic_to_config_value(
    state: &EngineState,
    pos: Position,
    option_name: &str,
    expected: &ConfigType,
    value: Dynamic,
) -> Option<ConfigValue> {
    let observed = value.type_name();
    let mismatch = |state: &EngineState, expected_str: &str| {
        state.push_diagnostic(
            Diagnostic::error(format!(
                "config option '{option_name}': expected {expected_str} value, got {observed}"
            ))
            .with_span(state.span_from(pos)),
        );
    };

    match expected {
        ConfigType::Bool => match value.as_bool() {
            Ok(b) => Some(ConfigValue::Bool(b)),
            Err(_) => {
                mismatch(state, "bool");
                None
            }
        },
        ConfigType::U32 => match value.as_int() {
            Ok(i) if (0..=u32::MAX as i64).contains(&i) => Some(ConfigValue::U32(i as u32)),
            Ok(i) => {
                state.push_diagnostic(
                    Diagnostic::error(format!(
                        "config option '{option_name}': value {i} out of range for u32"
                    ))
                    .with_span(state.span_from(pos)),
                );
                None
            }
            Err(_) => {
                mismatch(state, "u32");
                None
            }
        },
        ConfigType::U64 => match value.as_int() {
            Ok(i) if i >= 0 => Some(ConfigValue::U64(i as u64)),
            Ok(i) => {
                state.push_diagnostic(
                    Diagnostic::error(format!(
                        "config option '{option_name}': value {i} is negative, expected u64"
                    ))
                    .with_span(state.span_from(pos)),
                );
                None
            }
            Err(_) => {
                mismatch(state, "u64");
                None
            }
        },
        ConfigType::Str => match value.into_string() {
            Ok(s) => Some(ConfigValue::Str(s)),
            Err(_) => {
                mismatch(state, "string");
                None
            }
        },
        ConfigType::Choice => match value.into_string() {
            Ok(s) => Some(ConfigValue::Choice(s)),
            Err(_) => {
                mismatch(state, "string (choice variant name)");
                None
            }
        },
        ConfigType::List => match value.try_cast::<rhai::Array>() {
            Some(arr) => match array_to_string_vec(arr) {
                Ok(v) => Some(ConfigValue::List(v)),
                Err(msg) => {
                    state.push_diagnostic(
                        Diagnostic::error(format!("config option '{option_name}': {msg}"))
                            .with_span(state.span_from(pos)),
                    );
                    None
                }
            },
            None => {
                mismatch(state, "array of strings");
                None
            }
        },
        ConfigType::Tristate => match value.into_string() {
            Ok(s) => match s.as_str() {
                "y" | "yes" => Some(ConfigValue::Tristate(TristateVal::Yes)),
                "n" | "no" => Some(ConfigValue::Tristate(TristateVal::No)),
                "m" | "module" => Some(ConfigValue::Tristate(TristateVal::Module)),
                _ => {
                    state.push_diagnostic(
                        Diagnostic::error(format!(
                            "config option '{option_name}': tristate must be \"y\", \"n\", or \"m\", got \"{s}\""
                        ))
                        .with_span(state.span_from(pos)),
                    );
                    None
                }
            },
            Err(_) => {
                mismatch(state, "tristate string (\"y\"/\"n\"/\"m\")");
                None
            }
        },
        ConfigType::Group => {
            state.push_diagnostic(
                Diagnostic::error(format!(
                    "config option '{option_name}': cannot set a value on a Group-typed option"
                ))
                .with_span(state.span_from(pos)),
            );
            None
        }
    }
}

/// Best-effort conversion from a Rhai [`Dynamic`] to a [`ConfigValue`], used
/// by `preset.set(name, value)` where the expected type is not yet known at
/// the point of assignment. Values are stored using the closest matching
/// variant; the resolve pass later re-types them against the actual option's
/// `ConfigType`.
pub(super) fn dynamic_to_config_value_best_effort(
    state: &EngineState,
    pos: Position,
    preset_name: &str,
    option_name: &str,
    value: Dynamic,
) -> Option<ConfigValue> {
    if let Ok(b) = value.as_bool() {
        return Some(ConfigValue::Bool(b));
    }
    if let Ok(i) = value.as_int() {
        if i >= 0 {
            return Some(ConfigValue::U64(i as u64));
        }
        state.push_diagnostic(
            Diagnostic::error(format!(
                "preset '{preset_name}' option '{option_name}': negative integer {i} not allowed"
            ))
            .with_span(state.span_from(pos)),
        );
        return None;
    }
    if let Some(arr) = value.clone().try_cast::<rhai::Array>() {
        return match array_to_string_vec(arr) {
            Ok(v) => Some(ConfigValue::List(v)),
            Err(msg) => {
                state.push_diagnostic(
                    Diagnostic::error(format!(
                        "preset '{preset_name}' option '{option_name}': {msg}"
                    ))
                    .with_span(state.span_from(pos)),
                );
                None
            }
        };
    }
    let observed = value.type_name();
    match value.into_string() {
        Ok(s) => Some(ConfigValue::Str(s)),
        Err(_) => {
            state.push_diagnostic(
                Diagnostic::error(format!(
                    "preset '{preset_name}' option '{option_name}': unsupported value type {observed}"
                ))
                .with_span(state.span_from(pos)),
            );
            None
        }
    }
}

/// Convert a Rhai `Array` of strings into `Vec<String>`, or an error message
/// naming the first non-string element's type.
pub(super) fn array_to_string_vec(arr: rhai::Array) -> std::result::Result<Vec<String>, String> {
    let mut out = Vec::with_capacity(arr.len());
    for (i, v) in arr.into_iter().enumerate() {
        let type_name = v.type_name();
        match v.into_string() {
            Ok(s) => out.push(s),
            Err(_) => return Err(format!("element {i} is not a string (got {type_name})")),
        }
    }
    Ok(out)
}
