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

use crate::compile::BuildLayout;
use crate::error::{Error, Result};
use gluon_model::{BuildModel, ResolvedConfig, RuleDef, RuleHandler, TargetDef};
use std::collections::BTreeMap;

pub mod builtin;

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
    pub fn dispatch(&self, ctx: &RuleCtx<'_>, rule: &RuleDef) -> Result<()> {
        // Script handlers are deferred to a post-MVP-M chunk.
        let builtin_name = match &rule.handler {
            RuleHandler::Script(fn_name) => {
                return Err(Error::Compile(format!(
                    "rule '{}': script-backed rules (handler: '{}') are not implemented \
                     in MVP-M — this feature is deferred to a later chunk",
                    rule.name, fn_name
                )));
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

        handler.execute(ctx, rule, &inputs, &outputs)
    }
}

// ---------------------------------------------------------------------------
// ${var} substitution
// ---------------------------------------------------------------------------

/// The set of variable names recognised by the substitution engine. Kept as a
/// sorted slice so error messages listing known vars are deterministic.
const KNOWN_VARS: &[&str] = &["build_dir", "profile", "project_name", "target"];

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
/// - A bare `$` not followed by `{` is emitted literally. Escape sequences
///   (`$${...}`) are **not** supported in MVP-M; document this clearly so
///   callers are not surprised.
/// - `${build_dir}` → `ctx.layout.root().display().to_string()`
/// - `${project_name}` → `ctx.resolved.project.name.clone()`
/// - `${profile}` → `ctx.resolved.profile.name.clone()`
/// - `${target}` → the `name` of the target referenced by
///   `ctx.resolved.profile.target` via `ctx.model.targets`. A missing
///   target is treated as an internal bug (it should never happen after
///   the intern pass) and surfaces a clear error rather than a panic.
fn substitute_one(ctx: &RuleCtx<'_>, rule: &RuleDef, s: &str) -> Result<String> {
    // Fast path: no substitution needed.
    if !s.contains("${") {
        return Ok(s.to_owned());
    }

    let mut out = String::with_capacity(s.len() + 32);
    let bytes = s.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        // Look for `$`. If it's not followed by `{`, emit it literally and
        // advance. Supporting `$${` escapes is intentionally left out of
        // MVP-M — callers that need a literal `${` must restructure their
        // command rather than relying on escaping.
        if bytes[i] == b'$' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'{' {
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
/// Returns `Error::Compile` if `var_name` is not in [`KNOWN_VARS`], naming
/// the rule and listing all known variable names for discoverability.
fn resolve_var(ctx: &RuleCtx<'_>, rule: &RuleDef, var_name: &str) -> Result<String> {
    match var_name {
        "build_dir" => Ok(ctx.layout.root().display().to_string()),
        "project_name" => Ok(ctx.resolved.project.name.clone()),
        "profile" => Ok(ctx.resolved.profile.name.clone()),
        "target" => {
            // Dereference the profile's target handle via the build model.
            // This should never fail after the intern pass completes — if it
            // does, it is an internal bug rather than a user error, but we
            // surface it with a clear message rather than panicking.
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
             known variables: [{}]",
            rule.name,
            unknown,
            KNOWN_VARS.join(", ")
        ))),
    }
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
        use crate::compile::BuildLayout;
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
                },
                profile: make_profile(target_handle),
                options: Default::default(),
                crates: Vec::new(),
                build_dir: build_dir.clone(),
                project_root: build_dir.parent().unwrap_or(&build_dir).to_path_buf(),
            }
        }

        /// Build a `(BuildLayout, BuildModel, ResolvedConfig)` triple for testing.
        ///
        /// `tmp` should be a `tempfile::TempDir` base; the build dir is
        /// `tmp/build`.
        pub fn make_ctx_parts(tmp: &std::path::Path) -> (BuildLayout, BuildModel, ResolvedConfig) {
            let build_dir = tmp.join("build");
            std::fs::create_dir_all(&build_dir).unwrap();
            let (model, target_handle) = make_model_with_target("x86_64-test");
            let resolved = make_resolved(target_handle, build_dir.clone());
            let layout = BuildLayout::new(build_dir, "testproject");
            (layout, model, resolved)
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
            ) -> Result<()> {
                Ok(())
            }
        }

        let mut registry = RuleRegistry::new();
        registry.register("fake", Box::new(FakeRule));

        let tmp = tempfile::tempdir().unwrap();
        let (layout, model, resolved) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
        };
        let rule = RuleDef {
            name: "myrule".into(),
            inputs: vec![],
            outputs: vec![],
            depends_on: vec![],
            handler: RuleHandler::Builtin("nope".into()),
            span: None,
        };

        let err = registry.dispatch(&ctx, &rule).unwrap_err();
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
    fn dispatch_script_handler_returns_not_implemented_error() {
        let registry = RuleRegistry::new();
        let tmp = tempfile::tempdir().unwrap();
        let (layout, model, resolved) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
        };
        let rule = RuleDef {
            name: "scriptrule".into(),
            inputs: vec![],
            outputs: vec![],
            depends_on: vec![],
            handler: RuleHandler::Script("some_fn".into()),
            span: None,
        };

        let err = registry.dispatch(&ctx, &rule).unwrap_err();
        match err {
            Error::Compile(msg) => {
                assert!(
                    msg.to_lowercase().contains("script"),
                    "error should mention 'script': {msg}"
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
        let (layout, model, resolved) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
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
            span: None,
        };

        registry.dispatch(&ctx, &rule).unwrap();

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
        let (layout, model, resolved) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
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
            span: None,
        };

        registry.dispatch(&ctx, &rule).unwrap();
        let args = captured_inputs.lock().unwrap().clone();
        assert_eq!(args, vec!["testproject-debug-x86_64-test".to_string()]);
    }

    /// Regression guard: non-ASCII characters must survive the slow
    /// path. Earlier drafts used `bytes[i] as char` which silently split
    /// multi-byte UTF-8 sequences into invalid scalar values.
    #[test]
    fn substitution_preserves_non_ascii_literal_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, model, resolved) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
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
            span: None,
        };

        registry.dispatch(&ctx, &rule).unwrap();
        let args = captured_inputs.lock().unwrap().clone();
        assert_eq!(args, vec!["café-debug-naïve".to_string()]);
    }

    #[test]
    fn substitution_unknown_var_errors_with_rule_name_and_var_name() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, model, resolved) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
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
            span: None,
        };

        let err = registry.dispatch(&ctx, &rule).unwrap_err();
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
        let (layout, model, resolved) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
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
            span: None,
        };

        let err = registry.dispatch(&ctx, &rule).unwrap_err();
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
        let (layout, model, resolved) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
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
            span: None,
        };

        registry.dispatch(&ctx, &rule).unwrap();

        let args = captured_inputs.lock().unwrap().clone();
        assert_eq!(
            args[0], "$HOME/no_brace",
            "literal $ must pass through unchanged"
        );
    }

    #[test]
    fn substitution_handles_outputs_too() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, model, resolved) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
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
            span: None,
        };

        registry.dispatch(&ctx, &rule).unwrap();

        let out_args = captured_outputs.lock().unwrap().clone();
        assert_eq!(out_args.len(), 1);
        assert_eq!(
            out_args[0],
            format!("{}/out", tmp.path().join("build").display())
        );
    }
}
