//! Conversions from Rhai dynamic values to model types.
//!
//! Centralises the strict parsing logic so the builder layer stays thin.

use super::EngineState;
use crate::error::Diagnostic;
use gluon_model::DepDef;
use rhai::{Map, Position};
use std::collections::BTreeMap;

// TODO: restore `optional` and `default_features` to the allowed keys
// when DepDef gains fields for them (likely when vendor resolution lands
// in sub-project #3).
/// Allowed keys inside an entry of a `.deps(#{ ... })` map.
const ALLOWED_DEP_KEYS: &[&str] = &["crate", "features", "version"];

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
        let mut had_error = false;

        for (k, v) in &inner {
            let ks = k.as_str();
            if !ALLOWED_DEP_KEYS.contains(&ks) {
                state.push_diagnostic(
                    Diagnostic::error(format!(
                        "unknown dep option '{ks}' (allowed: crate, features, version)"
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
                span: Some(span.clone()),
            },
        );
    }

    out
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
