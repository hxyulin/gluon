//! Rule registry and dispatch for user-defined build rules.
//!
//! A [`RuleRegistry`] holds named [`RuleFn`] implementations. During pipeline
//! execution (Chunk B4), the scheduler calls [`RuleRegistry::dispatch`] for
//! each rule node as it becomes ready. This module is self-contained: it does
//! not yet integrate with the scheduler; Chunk B4 wires that together.
//!
//! ### Design notes
//!
//! **Why `BTreeMap`?** Deterministic iteration order. While handler lookup by
//! name is not order-sensitive, future diagnostics that enumerate registered
//! rules (e.g. "did you mean ...?") must produce stable output. A `HashMap`
//! would produce arbitrarily ordered lists across runs.
//!
//! **Why separate `inputs` and `outputs` slices in [`RuleFn::execute`]?**
//! Merging them into one `Vec` forces each handler to track where inputs end
//! and outputs begin. Keeping them separate makes the contract explicit: the
//! built-in `exec` rule treats inputs as the command + argv and ignores
//! outputs (which the scheduler uses for cache-invalidation bookkeeping).

use crate::compile::{ArtifactMap, BuildLayout};
use crate::error::{Error, Result};
use gluon_model::{BuildModel, CrateDef, ResolvedConfig, RuleDef, RuleHandler, TargetDef};
use std::collections::BTreeMap;

pub mod builtin;
pub mod script;

/// Execution context handed to a [`RuleFn`] when the scheduler invokes a rule.
///
/// Deliberately narrow: rules should not reach into the build cache directly
/// (the scheduler handles rule-level caching at the DAG level). Extend this
/// struct when a new capability is genuinely needed by an in-tree rule.
///
/// `model` is required because [`ResolvedConfig`] holds target handles but
/// not the target names themselves — we need the model to dereference
/// `resolved.profile.target` for `${target}` substitution.
pub struct RuleCtx<'a> {
    pub layout: &'a BuildLayout,
    pub resolved: &'a ResolvedConfig,
    pub model: &'a BuildModel,
    pub artifacts: &'a ArtifactMap,
}

/// A rule handler. Implementations receive pre-substituted argument strings —
/// the registry performs `${var}` expansion before dispatch.
///
/// ### Contract
///
/// - `inputs`: the substituted input strings. For the built-in `exec` rule the
///   first element is the command path and the remainder are its argv.
/// - `outputs`: the substituted output strings. The scheduler uses this list
///   for cache-invalidation bookkeeping. Most handlers can ignore it.
///
/// The two slices are passed separately (rather than concatenated) so handlers
/// can unambiguously distinguish between command arguments and output paths
/// without needing an out-of-band length.
pub trait RuleFn: Send + Sync {
    fn execute(
        &self,
        ctx: &RuleCtx<'_>,
        rule: &RuleDef,
        inputs: &[String],
        outputs: &[String],
        stdout: &mut Vec<u8>,
        stderr: &mut Vec<u8>,
    ) -> Result<()>;
}

/// Registry of rule handlers keyed by name.
///
/// Use [`RuleRegistry::with_builtins`] for normal operation; use
/// [`RuleRegistry::new`] in tests when you want a clean slate.
pub struct RuleRegistry {
    // BTreeMap for deterministic iteration — see module-level note.
    handlers: BTreeMap<String, Box<dyn RuleFn>>,
}

impl Default for RuleRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl RuleRegistry {
    /// Empty registry with no handlers.
    ///
    /// Use for tests or when the caller wants to hand-pick which builtins to
    /// register.
    pub fn new() -> Self {
        Self {
            handlers: BTreeMap::new(),
        }
    }

    /// Registry pre-populated with every in-tree builtin.
    ///
    /// MVP-M registers only `exec`; future chunks may add more.
    pub fn with_builtins() -> Self {
        let mut r = Self::new();
        r.register("exec", Box::new(builtin::ExecRule));
        r
    }

    /// Register a handler under the given name, overwriting any previous one.
    pub fn register(&mut self, name: impl Into<String>, handler: Box<dyn RuleFn>) {
        self.handlers.insert(name.into(), handler);
    }

    /// Returns `true` if a handler with this name has been registered.
    pub fn contains(&self, name: &str) -> bool {
        self.handlers.contains_key(name)
    }

    /// Dispatch a rule: validate the handler kind, substitute `${var}` tokens,
    /// and call the selected handler.
    ///
    /// ### Error cases
    ///
    /// - `rule.handler` is `RuleHandler::Script(_)` — script-backed rules are
    ///   not implemented in MVP-M; returns a contextual `Error::Compile`.
    /// - `rule.handler` is `RuleHandler::Builtin(name)` and `name` is not
    ///   registered — the error message names the rule, the requested builtin,
    ///   and the sorted list of registered builtin names.
    /// - Substitution encounters an unknown `${var}` — message names the rule
    ///   and the unknown variable.
    /// - Substitution encounters an unterminated `${...}` — message names the
    ///   rule and the offending token.
    pub fn dispatch(
        &self,
        ctx: &RuleCtx<'_>,
        rule: &RuleDef,
        stdout: &mut Vec<u8>,
        stderr: &mut Vec<u8>,
    ) -> Result<()> {
        // Script handlers delegate to the embedded Rhai engine.
        let builtin_name = match &rule.handler {
            RuleHandler::Script(_) => {
                let inputs = substitute_args(ctx, rule, &rule.inputs)?;
                let outputs = substitute_args(ctx, rule, &rule.outputs)?;
                return script::execute_script(ctx, rule, &inputs, &outputs, stdout, stderr);
            }
            RuleHandler::Builtin(name) => name,
        };

        let handler = self.handlers.get(builtin_name.as_str()).ok_or_else(|| {
            let available: Vec<&str> = self.handlers.keys().map(|s| s.as_str()).collect();
            Error::Compile(format!(
                "rule '{}': unknown builtin '{}'; registered builtins: [{}]",
                rule.name,
                builtin_name,
                available.join(", ")
            ))
        })?;

        // Substitute ${var} tokens in inputs and outputs separately so the
        // handler can tell them apart (see RuleFn doc comment).
        let inputs = substitute_args(ctx, rule, &rule.inputs)?;
        let outputs = substitute_args(ctx, rule, &rule.outputs)?;

        handler.execute(ctx, rule, &inputs, &outputs, stdout, stderr)
    }
}

// ---------------------------------------------------------------------------
// ${var} substitution
// ---------------------------------------------------------------------------

/// The set of simple variable names recognised by the substitution engine.
/// Kept as a sorted slice so error messages are deterministic.
///
/// In addition to these, the prefixed form `${artifact:<crate_name>}` resolves
/// to the output path of a compiled crate.
const KNOWN_VARS: &[&str] = &["build_dir", "profile", "project_name", "project_root", "target"];

/// Substitute all `${var}` tokens in a list of argument strings.
///
/// Returns a fresh `Vec<String>` — one element per input string — with all
/// recognised `${var}` tokens replaced. Unknown variables and unterminated
/// `${...}` sequences are reported as `Error::Compile` naming the offending
/// rule.
fn substitute_args(ctx: &RuleCtx<'_>, rule: &RuleDef, args: &[String]) -> Result<Vec<String>> {
    args.iter()
        .map(|arg| substitute_one(ctx, rule, arg))
        .collect()
}

/// Substitute `${var}` tokens in a single string.
///
/// ### Substitution behaviour
///
/// - `$$` produces a literal `$` (escape mechanism).
/// - `$${foo}` produces a literal `${foo}`.
/// - A bare `$` not followed by `$` or `{` is emitted literally.
/// - `${build_dir}` → the build output directory
/// - `${project_name}` → the project name from config
/// - `${project_root}` → the project root directory
/// - `${profile}` → the active profile name
/// - `${target}` → the target triple name
/// - `${artifact:<name>}` → the compiled output path for the named crate
fn substitute_one(ctx: &RuleCtx<'_>, rule: &RuleDef, s: &str) -> Result<String> {
    // Fast path: no substitution needed.
    if !s.contains('$') {
        return Ok(s.to_owned());
    }

    let mut out = String::with_capacity(s.len() + 32);
    let bytes = s.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'$' {
            // `$$` → literal `$` (escape).
            if i + 1 < bytes.len() && bytes[i + 1] == b'$' {
                out.push('$');
                i += 2;
            } else if i + 1 < bytes.len() && bytes[i + 1] == b'{' {
                // Opening `${` found — scan forward for `}`.
                let var_start = i + 2; // first byte of variable name
                let var_end = bytes[var_start..]
                    .iter()
                    .position(|&b| b == b'}')
                    .map(|pos| var_start + pos)
                    .ok_or_else(|| {
                        Error::Compile(format!(
                            "rule '{}': unterminated '${{...}}' sequence in argument: '{}'",
                            rule.name, s
                        ))
                    })?;

                let var_name = &s[var_start..var_end];
                let replacement = resolve_var(ctx, rule, var_name)?;
                out.push_str(&replacement);
                // Advance past the closing `}`.
                i = var_end + 1;
            } else {
                // Bare `$` not followed by `{` — emit literally.
                out.push('$');
                i += 1;
            }
        } else {
            // Decode the next UTF-8 codepoint at `i`. A naive `bytes[i] as
            // char` cast would silently split multi-byte characters (e.g.
            // 'é' = [0xC3, 0xA9]) into two invalid chars. `{` / `}` / `$`
            // are all ASCII so the brace-scanning branches above are safe
            // on byte indices; only this literal-copy branch needs to
            // advance by a full codepoint's worth of bytes.
            let ch = s[i..]
                .chars()
                .next()
                .expect("index is within &str bounds so at least one char follows");
            out.push(ch);
            i += ch.len_utf8();
        }
    }

    Ok(out)
}

/// Resolve a single variable name to its string value.
///
/// Handles simple variables (`build_dir`, `profile`, etc.) and the prefixed
/// form `artifact:<crate_name>` which resolves to a compiled artifact path.
///
/// Returns `Error::Compile` for unknown variables, naming the rule and
/// listing known variables for discoverability.
fn resolve_var(ctx: &RuleCtx<'_>, rule: &RuleDef, var_name: &str) -> Result<String> {
    // Prefixed variables: ${artifact:<crate_name>}
    if let Some(crate_name) = var_name.strip_prefix("artifact:") {
        return resolve_artifact(ctx, rule, crate_name);
    }

    match var_name {
        "build_dir" => Ok(ctx.layout.root().display().to_string()),
        "project_name" => Ok(ctx.resolved.project.name.clone()),
        "project_root" => Ok(ctx.resolved.project_root.display().to_string()),
        "profile" => Ok(ctx.resolved.profile.name.clone()),
        "target" => {
            let target: &TargetDef = ctx
                .model
                .targets
                .get(ctx.resolved.profile.target)
                .ok_or_else(|| {
                    Error::Compile(format!(
                        "rule '{}': internal error — target handle {:?} not found in \
                             build model (this is a bug in gluon, not in your configuration)",
                        rule.name, ctx.resolved.profile.target
                    ))
                })?;
            Ok(target.name.clone())
        }
        unknown => Err(Error::Compile(format!(
            "rule '{}': unknown substitution variable '${{{}}}'; \
             known variables: [{}], or use ${{artifact:<crate_name>}}",
            rule.name,
            unknown,
            KNOWN_VARS.join(", ")
        ))),
    }
}

/// Resolve `${artifact:<crate_name>}` to the compiled output path.
///
/// Looks up the crate by name in the build model, then retrieves its artifact
/// path from the snapshot. Returns clear errors if the crate doesn't exist or
/// hasn't been compiled yet.
fn resolve_artifact(ctx: &RuleCtx<'_>, rule: &RuleDef, crate_name: &str) -> Result<String> {
    if crate_name.is_empty() {
        return Err(Error::Compile(format!(
            "rule '{}': empty crate name in ${{artifact:}}",
            rule.name
        )));
    }

    let handle = ctx.model.crates.lookup(crate_name).ok_or_else(|| {
        let available: Vec<&str> = ctx
            .model
            .crates
            .iter()
            .map(|(_, c): (_, &CrateDef)| c.name.as_str())
            .collect();
        Error::Compile(format!(
            "rule '{}': ${{artifact:{}}} references unknown crate; \
             available crates: [{}]",
            rule.name,
            crate_name,
            available.join(", ")
        ))
    })?;

    let path = ctx.artifacts.get(handle).ok_or_else(|| {
        Error::Compile(format!(
            "rule '{}': ${{artifact:{}}} — crate exists but has no compiled \
             artifact (ensure the rule's pipeline stage depends on the crate's group)",
            rule.name, crate_name
        ))
    })?;

    Ok(path.display().to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use gluon_model::{BuildModel, Handle, ProjectDef, TargetDef};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    // -----------------------------------------------------------------------
    // Test fixtures
    // -----------------------------------------------------------------------

    /// Shared helper module so builtin.rs tests can `use super::test_support::*`.
    pub(crate) mod test_support {
        use super::*;
        use crate::compile::{ArtifactMap, BuildLayout};
        use gluon_model::{ResolvedConfig, ResolvedProfile};

        pub fn make_target(name: &str) -> TargetDef {
            TargetDef {
                name: name.into(),
                spec: format!("{}-unknown-none", name),
                builtin: true,
                panic_strategy: None,
                span: None,
            }
        }

        pub fn make_model_with_target(target_name: &str) -> (BuildModel, Handle<TargetDef>) {
            let mut model = BuildModel::default();
            let t = make_target(target_name);
            let (handle, _) = model.targets.insert(target_name.into(), t);
            (model, handle)
        }

        pub fn make_profile(target: Handle<TargetDef>) -> ResolvedProfile {
            ResolvedProfile {
                name: "debug".into(),
                target,
                opt_level: 0,
                debug_info: false,
                lto: None,
                boot_binary: None,
                qemu_memory: None,
                qemu_cores: None,
                qemu_extra_args: Vec::new(),
                test_timeout: None,
            }
        }

        pub fn make_resolved(
            target_handle: Handle<TargetDef>,
            build_dir: PathBuf,
        ) -> ResolvedConfig {
            ResolvedConfig {
                project: ProjectDef {
                    name: "testproject".into(),
                    version: "0.1.0".into(),
                    config_crate_name: None,
                    cfg_prefix: None,
                    config_override_file: None,
                    default_profile: None,
                },
                profile: make_profile(target_handle),
                options: Default::default(),
                crates: Vec::new(),
                build_dir: build_dir.clone(),
                project_root: build_dir.parent().unwrap_or(&build_dir).to_path_buf(),
            }
        }

        /// Build a `(BuildLayout, BuildModel, ResolvedConfig, ArtifactMap)` tuple
        /// for testing.
        ///
        /// `tmp` should be a `tempfile::TempDir` base; the build dir is
        /// `tmp/build`.
        pub fn make_ctx_parts(
            tmp: &std::path::Path,
        ) -> (BuildLayout, BuildModel, ResolvedConfig, ArtifactMap) {
            let build_dir = tmp.join("build");
            std::fs::create_dir_all(&build_dir).unwrap();
            let (model, target_handle) = make_model_with_target("x86_64-test");
            let resolved = make_resolved(target_handle, build_dir.clone());
            let layout = BuildLayout::new(build_dir, "testproject");
            let artifacts = ArtifactMap::new();
            (layout, model, resolved, artifacts)
        }
    }

    use test_support::*;

    // -----------------------------------------------------------------------
    // Registry tests
    // -----------------------------------------------------------------------

    #[test]
    fn empty_registry_has_no_handlers() {
        assert!(!RuleRegistry::new().contains("exec"));
    }

    #[test]
    fn with_builtins_registers_exec() {
        assert!(RuleRegistry::with_builtins().contains("exec"));
    }

    #[test]
    fn dispatch_unknown_builtin_returns_compile_error_with_available_list() {
        // A capturing dummy handler so we have at least one registered name.
        struct FakeRule;
        impl RuleFn for FakeRule {
            fn execute(
                &self,
                _ctx: &RuleCtx<'_>,
                _rule: &RuleDef,
                _inputs: &[String],
                _outputs: &[String],
                _stdout: &mut Vec<u8>,
                _stderr: &mut Vec<u8>,
            ) -> Result<()> {
                Ok(())
            }
        }

        let mut registry = RuleRegistry::new();
        registry.register("fake", Box::new(FakeRule));

        let tmp = tempfile::tempdir().unwrap();
        let (layout, model, resolved, artifacts) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
            artifacts: &artifacts,
        };
        let rule = RuleDef {
            name: "myrule".into(),
            inputs: vec![],
            outputs: vec![],
            depends_on: vec![],
            handler: RuleHandler::Builtin("nope".into()),
            working_dir: None,
            span: None,
        };

        let err = registry.dispatch(&ctx, &rule, &mut Vec::new(), &mut Vec::new()).unwrap_err();
        match err {
            Error::Compile(msg) => {
                assert!(
                    msg.contains("nope"),
                    "error should mention the unknown builtin: {msg}"
                );
                assert!(
                    msg.contains("fake"),
                    "error should list registered builtins: {msg}"
                );
                assert!(msg.contains("myrule"), "error should name the rule: {msg}");
            }
            other => panic!("expected Error::Compile, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_script_handler_executes_rhai_script() {
        let registry = RuleRegistry::new();
        let tmp = tempfile::tempdir().unwrap();
        let (layout, model, resolved, artifacts) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
            artifacts: &artifacts,
        };
        let rule = RuleDef {
            name: "scriptrule".into(),
            inputs: vec![],
            outputs: vec![],
            depends_on: vec![],
            handler: RuleHandler::Script(r#"log("hello from script");"#.into()),
            working_dir: None,
            span: None,
        };

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        registry
            .dispatch(&ctx, &rule, &mut stdout, &mut stderr)
            .unwrap();
        let output = String::from_utf8_lossy(&stdout);
        assert!(
            output.contains("hello from script"),
            "script log should appear in stdout: {output}"
        );
    }

    #[test]
    fn dispatch_script_handler_syntax_error_returns_compile_error() {
        let registry = RuleRegistry::new();
        let tmp = tempfile::tempdir().unwrap();
        let (layout, model, resolved, artifacts) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
            artifacts: &artifacts,
        };
        let rule = RuleDef {
            name: "badsyntax".into(),
            inputs: vec![],
            outputs: vec![],
            depends_on: vec![],
            handler: RuleHandler::Script("let x = ;".into()),
            working_dir: None,
            span: None,
        };

        let err = registry
            .dispatch(&ctx, &rule, &mut Vec::new(), &mut Vec::new())
            .unwrap_err();
        match err {
            Error::Compile(msg) => {
                assert!(msg.contains("badsyntax"), "should name rule: {msg}");
                assert!(
                    msg.contains("script execution failed"),
                    "should mention script failure: {msg}"
                );
            }
            other => panic!("expected Error::Compile, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Substitution tests
    // -----------------------------------------------------------------------

    /// A capturing handler that stores the substituted args in a shared vec.
    struct CapturingRule {
        captured_inputs: Arc<Mutex<Vec<String>>>,
        captured_outputs: Arc<Mutex<Vec<String>>>,
    }

    impl RuleFn for CapturingRule {
        fn execute(
            &self,
            _ctx: &RuleCtx<'_>,
            _rule: &RuleDef,
            inputs: &[String],
            outputs: &[String],
            _stdout: &mut Vec<u8>,
            _stderr: &mut Vec<u8>,
        ) -> Result<()> {
            *self.captured_inputs.lock().unwrap() = inputs.to_vec();
            *self.captured_outputs.lock().unwrap() = outputs.to_vec();
            Ok(())
        }
    }

    type CaptureHandle = Arc<Mutex<Vec<String>>>;
    type CapturingRuleOutput = (CaptureHandle, CaptureHandle, Box<dyn RuleFn>);

    fn make_capturing_rule() -> CapturingRuleOutput {
        let captured_inputs = Arc::new(Mutex::new(Vec::new()));
        let captured_outputs = Arc::new(Mutex::new(Vec::new()));
        let rule = Box::new(CapturingRule {
            captured_inputs: captured_inputs.clone(),
            captured_outputs: captured_outputs.clone(),
        });
        (captured_inputs, captured_outputs, rule)
    }

    #[test]
    fn substitution_expands_known_vars() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, model, resolved, artifacts) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
            artifacts: &artifacts,
        };

        let (captured_inputs, _captured_outputs, handler) = make_capturing_rule();
        let mut registry = RuleRegistry::new();
        registry.register("capture", handler);

        let rule = RuleDef {
            name: "testrule".into(),
            inputs: vec![
                "${build_dir}/foo".into(),
                "${project_name}".into(),
                "${profile}".into(),
                "${target}".into(),
            ],
            outputs: vec![],
            depends_on: vec![],
            handler: RuleHandler::Builtin("capture".into()),
            working_dir: None,
            span: None,
        };

        registry.dispatch(&ctx, &rule, &mut Vec::new(), &mut Vec::new()).unwrap();

        let args = captured_inputs.lock().unwrap().clone();
        assert_eq!(args.len(), 4);
        assert_eq!(
            args[0],
            format!("{}/foo", tmp.path().join("build").display())
        );
        assert_eq!(args[1], "testproject");
        assert_eq!(args[2], "debug");
        assert_eq!(args[3], "x86_64-test");
    }

    /// Regression guard: two `${...}` tokens in a single argument must
    /// both substitute correctly. A broken loop-advance after the first
    /// substitution would either duplicate or drop bytes around the
    /// second token.
    #[test]
    fn substitution_expands_multiple_vars_in_one_arg() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, model, resolved, artifacts) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
            artifacts: &artifacts,
        };

        let (captured_inputs, _captured_outputs, handler) = make_capturing_rule();
        let mut registry = RuleRegistry::new();
        registry.register("capture", handler);

        let rule = RuleDef {
            name: "testrule".into(),
            inputs: vec!["${project_name}-${profile}-${target}".into()],
            outputs: vec![],
            depends_on: vec![],
            handler: RuleHandler::Builtin("capture".into()),
            working_dir: None,
            span: None,
        };

        registry.dispatch(&ctx, &rule, &mut Vec::new(), &mut Vec::new()).unwrap();
        let args = captured_inputs.lock().unwrap().clone();
        assert_eq!(args, vec!["testproject-debug-x86_64-test".to_string()]);
    }

    /// Regression guard: non-ASCII characters must survive the slow
    /// path. Earlier drafts used `bytes[i] as char` which silently split
    /// multi-byte UTF-8 sequences into invalid scalar values.
    #[test]
    fn substitution_preserves_non_ascii_literal_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, model, resolved, artifacts) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
            artifacts: &artifacts,
        };

        let (captured_inputs, _captured_outputs, handler) = make_capturing_rule();
        let mut registry = RuleRegistry::new();
        registry.register("capture", handler);

        let rule = RuleDef {
            name: "testrule".into(),
            inputs: vec!["café-${profile}-naïve".into()],
            outputs: vec![],
            depends_on: vec![],
            handler: RuleHandler::Builtin("capture".into()),
            working_dir: None,
            span: None,
        };

        registry.dispatch(&ctx, &rule, &mut Vec::new(), &mut Vec::new()).unwrap();
        let args = captured_inputs.lock().unwrap().clone();
        assert_eq!(args, vec!["café-debug-naïve".to_string()]);
    }

    #[test]
    fn substitution_unknown_var_errors_with_rule_name_and_var_name() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, model, resolved, artifacts) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
            artifacts: &artifacts,
        };

        let (_, _, handler) = make_capturing_rule();
        let mut registry = RuleRegistry::new();
        registry.register("capture", handler);

        let rule = RuleDef {
            name: "badrule".into(),
            inputs: vec!["${nope}".into()],
            outputs: vec![],
            depends_on: vec![],
            handler: RuleHandler::Builtin("capture".into()),
            working_dir: None,
            span: None,
        };

        let err = registry.dispatch(&ctx, &rule, &mut Vec::new(), &mut Vec::new()).unwrap_err();
        match err {
            Error::Compile(msg) => {
                assert!(msg.contains("badrule"), "should name rule: {msg}");
                assert!(msg.contains("nope"), "should name unknown var: {msg}");
                // Should also mention the list of known vars.
                assert!(msg.contains("build_dir"), "should list known vars: {msg}");
            }
            other => panic!("expected Error::Compile, got {other:?}"),
        }
    }

    #[test]
    fn substitution_unterminated_brace_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, model, resolved, artifacts) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
            artifacts: &artifacts,
        };

        let (_, _, handler) = make_capturing_rule();
        let mut registry = RuleRegistry::new();
        registry.register("capture", handler);

        let rule = RuleDef {
            name: "badrule".into(),
            inputs: vec!["${unterminated".into()],
            outputs: vec![],
            depends_on: vec![],
            handler: RuleHandler::Builtin("capture".into()),
            working_dir: None,
            span: None,
        };

        let err = registry.dispatch(&ctx, &rule, &mut Vec::new(), &mut Vec::new()).unwrap_err();
        match err {
            Error::Compile(msg) => {
                assert!(msg.contains("badrule"), "should name rule: {msg}");
                // The message should hint about the unterminated sequence.
                assert!(
                    msg.contains("unterminated") || msg.contains("${"),
                    "should hint at unterminated sequence: {msg}"
                );
            }
            other => panic!("expected Error::Compile, got {other:?}"),
        }
    }

    #[test]
    fn substitution_literal_dollar_is_preserved() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, model, resolved, artifacts) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
            artifacts: &artifacts,
        };

        let (captured_inputs, _, handler) = make_capturing_rule();
        let mut registry = RuleRegistry::new();
        registry.register("capture", handler);

        let rule = RuleDef {
            name: "testrule".into(),
            inputs: vec!["$HOME/no_brace".into()],
            outputs: vec![],
            depends_on: vec![],
            handler: RuleHandler::Builtin("capture".into()),
            working_dir: None,
            span: None,
        };

        registry.dispatch(&ctx, &rule, &mut Vec::new(), &mut Vec::new()).unwrap();

        let args = captured_inputs.lock().unwrap().clone();
        assert_eq!(
            args[0], "$HOME/no_brace",
            "literal $ must pass through unchanged"
        );
    }

    #[test]
    fn substitution_handles_outputs_too() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, model, resolved, artifacts) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
            artifacts: &artifacts,
        };

        let (_, captured_outputs, handler) = make_capturing_rule();
        let mut registry = RuleRegistry::new();
        registry.register("capture", handler);

        let rule = RuleDef {
            name: "testrule".into(),
            inputs: vec!["cmd".into()],
            outputs: vec!["${build_dir}/out".into()],
            depends_on: vec![],
            handler: RuleHandler::Builtin("capture".into()),
            working_dir: None,
            span: None,
        };

        registry.dispatch(&ctx, &rule, &mut Vec::new(), &mut Vec::new()).unwrap();

        let out_args = captured_outputs.lock().unwrap().clone();
        assert_eq!(out_args.len(), 1);
        assert_eq!(
            out_args[0],
            format!("{}/out", tmp.path().join("build").display())
        );
    }

    // -----------------------------------------------------------------------
    // Artifact substitution tests
    // -----------------------------------------------------------------------

    #[test]
    fn substitution_resolves_artifact_by_crate_name() {
        use gluon_model::CrateDef;

        let tmp = tempfile::tempdir().unwrap();
        let (layout, mut model, resolved, mut artifacts) = make_ctx_parts(tmp.path());

        // Register a crate in the model and record its artifact.
        let crate_def = CrateDef {
            name: "kernel".into(),
            path: "crates/kernel".into(),
            edition: "2021".into(),
            crate_type: gluon_model::CrateType::Bin,
            target: "x86_64-test".into(),
            target_handle: None,
            deps: Default::default(),
            dev_deps: Default::default(),
            features: vec![],
            root: None,
            linker_script: None,
            group: "kernel_group".into(),
            group_handle: None,
            is_project_crate: true,
            cfg_flags: vec![],
            rustc_flags: vec![],
            requires_config: vec![],
            artifact_deps: vec![],
            artifact_env: Default::default(),
            span: None,
        };
        let (handle, _) = model.crates.insert("kernel".into(), crate_def);
        let artifact_path = tmp.path().join("build/cross/x86_64-test/debug/kernel");
        artifacts.insert(handle, artifact_path.clone());

        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
            artifacts: &artifacts,
        };

        let (captured_inputs, _, handler) = make_capturing_rule();
        let mut registry = RuleRegistry::new();
        registry.register("capture", handler);

        let rule = RuleDef {
            name: "strip".into(),
            inputs: vec!["objcopy".into(), "${artifact:kernel}".into()],
            outputs: vec![],
            depends_on: vec![],
            handler: RuleHandler::Builtin("capture".into()),
            working_dir: None,
            span: None,
        };

        registry.dispatch(&ctx, &rule, &mut Vec::new(), &mut Vec::new()).unwrap();
        let args = captured_inputs.lock().unwrap().clone();
        assert_eq!(args[1], artifact_path.display().to_string());
    }

    #[test]
    fn substitution_artifact_unknown_crate_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, model, resolved, artifacts) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
            artifacts: &artifacts,
        };

        let (_, _, handler) = make_capturing_rule();
        let mut registry = RuleRegistry::new();
        registry.register("capture", handler);

        let rule = RuleDef {
            name: "badrule".into(),
            inputs: vec!["${artifact:nonexistent}".into()],
            outputs: vec![],
            depends_on: vec![],
            handler: RuleHandler::Builtin("capture".into()),
            working_dir: None,
            span: None,
        };

        let err = registry.dispatch(&ctx, &rule, &mut Vec::new(), &mut Vec::new()).unwrap_err();
        match err {
            Error::Compile(msg) => {
                assert!(msg.contains("nonexistent"), "should name crate: {msg}");
                assert!(msg.contains("badrule"), "should name rule: {msg}");
            }
            other => panic!("expected Error::Compile, got {other:?}"),
        }
    }

    #[test]
    fn substitution_artifact_empty_name_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, model, resolved, artifacts) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
            artifacts: &artifacts,
        };

        let (_, _, handler) = make_capturing_rule();
        let mut registry = RuleRegistry::new();
        registry.register("capture", handler);

        let rule = RuleDef {
            name: "badrule".into(),
            inputs: vec!["${artifact:}".into()],
            outputs: vec![],
            depends_on: vec![],
            handler: RuleHandler::Builtin("capture".into()),
            working_dir: None,
            span: None,
        };

        let err = registry.dispatch(&ctx, &rule, &mut Vec::new(), &mut Vec::new()).unwrap_err();
        match err {
            Error::Compile(msg) => {
                assert!(msg.contains("empty"), "should mention empty: {msg}");
            }
            other => panic!("expected Error::Compile, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // project_root and $$ escape tests
    // -----------------------------------------------------------------------

    #[test]
    fn substitution_expands_project_root() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, model, resolved, artifacts) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
            artifacts: &artifacts,
        };

        let (captured_inputs, _, handler) = make_capturing_rule();
        let mut registry = RuleRegistry::new();
        registry.register("capture", handler);

        let rule = RuleDef {
            name: "testrule".into(),
            inputs: vec!["${project_root}/keys/sign.key".into()],
            outputs: vec![],
            depends_on: vec![],
            handler: RuleHandler::Builtin("capture".into()),
            working_dir: None,
            span: None,
        };

        registry.dispatch(&ctx, &rule, &mut Vec::new(), &mut Vec::new()).unwrap();
        let args = captured_inputs.lock().unwrap().clone();
        assert_eq!(
            args[0],
            format!("{}/keys/sign.key", resolved.project_root.display())
        );
    }

    #[test]
    fn substitution_double_dollar_produces_literal_dollar() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, model, resolved, artifacts) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
            artifacts: &artifacts,
        };

        let (captured_inputs, _, handler) = make_capturing_rule();
        let mut registry = RuleRegistry::new();
        registry.register("capture", handler);

        let rule = RuleDef {
            name: "testrule".into(),
            inputs: vec!["echo $${HOME}".into()],
            outputs: vec![],
            depends_on: vec![],
            handler: RuleHandler::Builtin("capture".into()),
            working_dir: None,
            span: None,
        };

        registry.dispatch(&ctx, &rule, &mut Vec::new(), &mut Vec::new()).unwrap();
        let args = captured_inputs.lock().unwrap().clone();
        assert_eq!(args[0], "echo ${HOME}");
    }

    #[test]
    fn substitution_double_dollar_standalone() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, model, resolved, artifacts) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
            artifacts: &artifacts,
        };

        let (captured_inputs, _, handler) = make_capturing_rule();
        let mut registry = RuleRegistry::new();
        registry.register("capture", handler);

        let rule = RuleDef {
            name: "testrule".into(),
            inputs: vec!["cost: $$5".into()],
            outputs: vec![],
            depends_on: vec![],
            handler: RuleHandler::Builtin("capture".into()),
            working_dir: None,
            span: None,
        };

        registry.dispatch(&ctx, &rule, &mut Vec::new(), &mut Vec::new()).unwrap();
        let args = captured_inputs.lock().unwrap().clone();
        assert_eq!(args[0], "cost: $5");
    }
}
