//! Built-in rule handlers shipped with gluon-core.
//!
//! MVP-M ships only `exec`. Future chunks may add image generation, linker
//! orchestration, etc. Each builtin is a zero-size struct implementing
//! [`RuleFn`].

use super::{RuleCtx, RuleFn};
use crate::error::{Diagnostic, Error, Result};
use gluon_model::RuleDef;
use std::path::PathBuf;
use std::process::Command;

/// Built-in `exec` rule handler.
///
/// Runs an arbitrary command with the substituted input args as argv.
///
/// ### Argument contract
///
/// - `inputs[0]` is the command path (or name looked up via `PATH`).
/// - `inputs[1..]` are its arguments.
/// - `outputs` is ignored by the handler itself; the scheduler uses it for
///   cache-invalidation bookkeeping.
///
/// ### Working directory
///
/// The command is run with `ctx.layout.root()` as the current directory so
/// that relative paths in substituted args resolve against the build root
/// rather than whatever directory the gluon process happens to be running in.
/// This makes rules portable: a rule that references `./sentinel` always means
/// `<build_dir>/sentinel` regardless of how the user invoked `gluon`.
///
/// ### Stdout / stderr
///
/// Both streams are forwarded into the per-job buffers the scheduler hands
/// us; the worker pool flushes them to the user's stdout/stderr atomically
/// per job, so output from parallel rule runs never interleaves.
pub struct ExecRule;

impl RuleFn for ExecRule {
    fn execute(
        &self,
        ctx: &RuleCtx<'_>,
        rule: &RuleDef,
        inputs: &[String],
        _outputs: &[String],
        stdout: &mut Vec<u8>,
        stderr: &mut Vec<u8>,
    ) -> Result<()> {
        // Guard: exec requires at least one input (the command).
        if inputs.is_empty() {
            return Err(Error::Compile(format!(
                "rule '{}': exec requires at least one input arg (the command)",
                rule.name
            )));
        }

        // `split_first` is safe here because we checked `is_empty` above.
        let (cmd, args) = inputs.split_first().unwrap();

        // Resolve working directory: default to build root, but allow
        // "project_root" to run from the project root.
        let cwd = match rule.working_dir.as_deref() {
            Some("project_root") => ctx.resolved.project_root.clone(),
            Some(other) => {
                return Err(Error::Compile(format!(
                    "rule '{}': unknown working_dir '{}'; expected 'project_root' or omit for build dir",
                    rule.name, other
                )));
            }
            None => ctx.layout.root().to_path_buf(),
        };

        let output = Command::new(cmd)
            .args(args)
            .current_dir(&cwd)
            .output()
            .map_err(|e| Error::Io {
                path: PathBuf::from(cmd),
                source: e,
            })?;

        if !output.status.success() {
            let cmd_stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Diagnostics(vec![
                Diagnostic::error(format!(
                    "rule '{}': exec command '{}' failed: exit={:?}",
                    rule.name,
                    cmd,
                    output.status.code()
                ))
                .with_note(format!("stderr:\n{}", cmd_stderr))
                .with_note(format!("command: {} {:?}", cmd, args)),
            ]));
        }

        // Forward captured output to the scheduler's per-job buffers so
        // rule output is visible in the build log.
        stdout.extend_from_slice(&output.stdout);
        stderr.extend_from_slice(&output.stderr);

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use crate::error::Error;
    use crate::rule::tests::test_support::make_ctx_parts;
    use crate::rule::{RuleCtx, RuleRegistry};
    use gluon_model::{RuleDef, RuleHandler};

    fn make_exec_rule(name: &str, inputs: Vec<String>) -> RuleDef {
        RuleDef {
            name: name.into(),
            inputs,
            outputs: vec![],
            depends_on: vec![],
            handler: RuleHandler::Builtin("exec".into()),
            working_dir: None,
            span: None,
        }
    }

    #[test]
    fn exec_empty_inputs_returns_compile_error() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, model, resolved, artifacts) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
            artifacts: &artifacts,
        };
        let registry = RuleRegistry::with_builtins();
        let rule = make_exec_rule("myrule", vec![]);

        let err = registry
            .dispatch(&ctx, &rule, &mut Vec::new(), &mut Vec::new())
            .unwrap_err();
        match err {
            Error::Compile(msg) => {
                assert!(msg.contains("exec"), "should mention exec: {msg}");
                assert!(msg.contains("myrule"), "should name rule: {msg}");
            }
            other => panic!("expected Error::Compile, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn exec_runs_command_and_produces_output_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, model, resolved, artifacts) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
            artifacts: &artifacts,
        };
        let registry = RuleRegistry::with_builtins();

        // Use `sh -c 'touch "$1"' -- <path>` to create a sentinel file.
        // `${build_dir}/sentinel` will be substituted to the real build dir.
        let sentinel_path = format!("{}/sentinel", tmp.path().join("build").display());
        let rule = RuleDef {
            name: "touchrule".into(),
            inputs: vec![
                "sh".into(),
                "-c".into(),
                "touch \"$1\"".into(),
                "--".into(),
                "${build_dir}/sentinel".into(),
            ],
            outputs: vec![],
            depends_on: vec![],
            handler: RuleHandler::Builtin("exec".into()),
            working_dir: None,
            span: None,
        };

        registry
            .dispatch(&ctx, &rule, &mut Vec::new(), &mut Vec::new())
            .unwrap();

        assert!(
            std::path::Path::new(&sentinel_path).exists(),
            "sentinel file should have been created at {sentinel_path}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn exec_nonzero_exit_returns_diagnostic_with_stderr() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, model, resolved, artifacts) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
            artifacts: &artifacts,
        };
        let registry = RuleRegistry::with_builtins();

        let rule = RuleDef {
            name: "failrule".into(),
            inputs: vec!["sh".into(), "-c".into(), "echo boom >&2; exit 1".into()],
            outputs: vec![],
            depends_on: vec![],
            handler: RuleHandler::Builtin("exec".into()),
            working_dir: None,
            span: None,
        };

        let err = registry
            .dispatch(&ctx, &rule, &mut Vec::new(), &mut Vec::new())
            .unwrap_err();
        match err {
            Error::Diagnostics(diags) => {
                assert!(!diags.is_empty(), "should have at least one diagnostic");
                let diag = &diags[0];
                assert!(
                    diag.message.contains("failrule"),
                    "headline should name rule: {}",
                    diag.message
                );
                assert!(
                    diag.message.contains("exit="),
                    "headline should mention exit code: {}",
                    diag.message
                );
                let notes_combined = diag.notes.join("\n");
                assert!(
                    notes_combined.contains("boom"),
                    "notes should include stderr output: {notes_combined}"
                );
            }
            other => panic!("expected Error::Diagnostics, got {other:?}"),
        }
    }
}
