//! Rhai integration for the `.kconfig` file loader.
//!
//! Exposes a single function: `load_kconfig("./path")`. The path is
//! resolved relative to the directory containing the `gluon.rhai`
//! script that called the function. Loaded options and presets are
//! merged into the same `BuildModel` maps that the inline `config_*`
//! and `preset(...)` builders populate, so downstream resolution is
//! oblivious to the source format.
//!
//! Cross-source duplicate detection: if a `.kconfig` file declares an
//! option (or preset) with the same name as one already declared by an
//! earlier `config_*` call or by a previously-loaded `.kconfig`, the
//! second declaration is rejected with a diagnostic and the first
//! declaration wins. This matches the duplicate-detection behavior of
//! the inline `config_*` builders in `engine/builders/config.rs`.

use crate::engine::EngineState;
use crate::error::Diagnostic;
use crate::kconfig::load_kconfig as load_kconfig_file;
use rhai::{Engine, NativeCallContext, Position};
use std::path::PathBuf;

pub(super) fn register(engine: &mut Engine, state: &EngineState) {
    let captured = state.clone();
    engine.register_fn(
        "load_kconfig",
        move |ctx: NativeCallContext, path: &str| -> () {
            let pos = ctx.call_position();
            load_kconfig_into_state(&captured, path, pos);
        },
    );
}

fn load_kconfig_into_state(state: &EngineState, raw_path: &str, pos: Position) {
    let span = state.span_from(pos);

    // Resolve relative paths against the directory of the calling
    // gluon.rhai. Absolute paths pass through unchanged.
    let path = PathBuf::from(raw_path);
    let resolved = if path.is_absolute() {
        path
    } else {
        let script_dir = state
            .script_file
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        script_dir.join(path)
    };

    let lowered = match load_kconfig_file(&resolved) {
        Ok(lw) => lw,
        Err(diags) => {
            // Forward the loader's diagnostics verbatim — they already
            // carry per-file spans pointing at the offending tokens.
            // Prepend a single contextual diagnostic so the user sees
            // which Rhai call triggered the load.
            state.push_diagnostic(
                Diagnostic::error(format!("load_kconfig(\"{raw_path}\") failed"))
                    .with_span(span.clone()),
            );
            for d in diags {
                state.push_diagnostic(d);
            }
            return;
        }
    };

    // Merge into the build model with cross-source duplicate detection.
    let mut model = state.model.borrow_mut();
    for (name, opt) in lowered.options {
        if model.config_options.contains_key(&name) {
            state.push_diagnostic(
                Diagnostic::error(format!(
                    "config option '{name}' from .kconfig conflicts with an existing declaration"
                ))
                .with_span(span.clone())
                .with_note(
                    "the earlier declaration wins — remove one of the two definitions or rename one of them"
                ),
            );
            continue;
        }
        model.config_options.insert(name, opt);
    }
    for (name, preset) in lowered.presets {
        if model.presets.contains_key(&name) {
            state.push_diagnostic(
                Diagnostic::error(format!(
                    "preset '{name}' from .kconfig conflicts with an existing declaration"
                ))
                .with_span(span.clone()),
            );
            continue;
        }
        model.presets.insert(name, preset);
    }
}
