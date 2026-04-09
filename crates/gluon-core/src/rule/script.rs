//! Script-backed rule handler using an embedded Rhai engine.
//!
//! When a rule uses `.on_execute(script)`, the script source is stored in
//! `RuleHandler::Script`. This module evaluates that script in a sandboxed
//! Rhai engine with access to build context variables and helper functions.

use super::RuleCtx;
use crate::error::{Error, Result};
use gluon_model::RuleDef;
use std::process::Command;
use std::sync::{Arc, Mutex};

/// Evaluate a script-backed rule.
///
/// Creates a fresh `rhai::Engine` per invocation (no cross-rule state leakage)
/// with access to:
///
/// - **Scope variables:** `build_dir`, `project_root`, `profile`, `target`,
///   `project_name`, `inputs`, `outputs`
/// - **Functions:** `artifact(name)` → artifact path, `exec(cmd, args)` →
///   exit code, `log(msg)` → writes to stdout buffer
pub fn execute_script(
    ctx: &RuleCtx<'_>,
    rule: &RuleDef,
    inputs: &[String],
    outputs: &[String],
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
) -> Result<()> {
    let script_source = match &rule.handler {
        gluon_model::RuleHandler::Script(src) => src,
        _ => {
            return Err(Error::Compile(format!(
                "rule '{}': execute_script called with non-Script handler",
                rule.name
            )));
        }
    };

    let mut engine = rhai::Engine::new();

    // Sandboxing: cap computation to prevent infinite loops.
    engine.set_max_operations(1_000_000);

    // Resolve target name for scope.
    let target_name = ctx
        .model
        .targets
        .get(ctx.resolved.profile.target)
        .map(|t| t.name.clone())
        .unwrap_or_default();

    // Build scope with context variables.
    let mut scope = rhai::Scope::new();
    scope.push("build_dir", ctx.layout.root().display().to_string());
    scope.push("project_root", ctx.resolved.project_root.display().to_string());
    scope.push("profile", ctx.resolved.profile.name.clone());
    scope.push("target", target_name);
    scope.push("project_name", ctx.resolved.project.name.clone());
    scope.push(
        "inputs",
        inputs
            .iter()
            .map(|s| rhai::Dynamic::from(s.clone()))
            .collect::<rhai::Array>(),
    );
    scope.push(
        "outputs",
        outputs
            .iter()
            .map(|s| rhai::Dynamic::from(s.clone()))
            .collect::<rhai::Array>(),
    );

    // Register `artifact(name)` function.
    let artifacts = ctx.artifacts.clone();
    let model_crates = ctx.model.crates.clone();
    let rule_name_for_artifact = rule.name.clone();
    engine.register_fn("artifact", move |name: &str| -> std::result::Result<String, Box<rhai::EvalAltResult>> {
        let handle = model_crates.lookup(name).ok_or_else(|| {
            Box::new(rhai::EvalAltResult::ErrorRuntime(
                format!(
                    "rule '{}': artifact('{}') — unknown crate",
                    rule_name_for_artifact, name
                )
                .into(),
                rhai::Position::NONE,
            ))
        })?;
        let path = artifacts.get(handle).ok_or_else(|| {
            Box::new(rhai::EvalAltResult::ErrorRuntime(
                format!(
                    "rule '{}': artifact('{}') — crate has no compiled artifact",
                    rule_name_for_artifact, name
                )
                .into(),
                rhai::Position::NONE,
            ))
        })?;
        Ok(path.display().to_string())
    });

    // Register `exec(cmd, args)` function.
    let build_root = ctx.layout.root().to_path_buf();
    let rule_name_for_exec = rule.name.clone();
    let exec_stderr: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let exec_stderr_clone = exec_stderr.clone();
    engine.register_fn(
        "exec",
        move |cmd: &str, args: rhai::Array| -> std::result::Result<i64, Box<rhai::EvalAltResult>> {
            let str_args: Vec<String> = args
                .into_iter()
                .map(|a| a.into_string().unwrap_or_default())
                .collect();
            let output = Command::new(cmd)
                .args(&str_args)
                .current_dir(&build_root)
                .output()
                .map_err(|e| {
                    Box::new(rhai::EvalAltResult::ErrorRuntime(
                        format!(
                            "rule '{}': exec('{}') failed to start: {}",
                            rule_name_for_exec, cmd, e
                        )
                        .into(),
                        rhai::Position::NONE,
                    ))
                })?;
            if !output.stderr.is_empty() {
                if let Ok(mut buf) = exec_stderr_clone.lock() {
                    buf.extend_from_slice(&output.stderr);
                }
            }
            Ok(output.status.code().unwrap_or(-1) as i64)
        },
    );

    // Register `log(msg)` function.
    let log_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let log_buf_clone = log_buf.clone();
    engine.register_fn("log", move |msg: &str| {
        if let Ok(mut buf) = log_buf_clone.lock() {
            buf.extend_from_slice(msg.as_bytes());
            buf.push(b'\n');
        }
    });

    // Execute the script. The return value is ignored — scripts communicate
    // results through side effects (exec, log, file I/O).
    let _ = engine
        .eval_with_scope::<rhai::Dynamic>(&mut scope, script_source)
        .map_err(|e| {
            Error::Compile(format!(
                "rule '{}': script execution failed: {}",
                rule.name, e
            ))
        })?;

    // Flush captured output to the scheduler buffers.
    if let Ok(buf) = log_buf.lock() {
        stdout.extend_from_slice(&buf);
    }
    if let Ok(buf) = exec_stderr.lock() {
        stderr.extend_from_slice(&buf);
    }

    Ok(())
}
