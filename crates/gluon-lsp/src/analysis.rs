//! Semantic analysis: walks the parsed AST against the [`DslSchema`]
//! to resolve builder chain types, classify tokens, and emit diagnostics.
//!
//! ## Algorithm
//!
//! The DSL is a chain of constructor calls and builder methods. Each
//! constructor (`group`, `qemu`, ...) returns either `Void` or a named
//! builder type. Each method on a builder returns either the same
//! builder (`SelfType` — chainable), a different builder (cross-builder
//! transition like `GroupBuilder.add() -> CrateBuilder`), or `Void`
//! (terminal — chaining further is an error).
//!
//! [`analyze`] walks the AST in pre-order, threading the resolved
//! builder type up from each receiver to its method call. Tokens are
//! collected during the walk and sorted at the end so the LSP can emit
//! semantic-token deltas (which require strictly increasing positions).
//!
//! Errors are accumulated, never fatal: an unknown method on a known
//! builder still lets us classify subsequent constructors in the same
//! file. This matches IDE expectations — a single typo shouldn't blank
//! the whole document's highlighting.

use crate::parser::{Node, SyntaxTree, TextRange};
use gluon_core::engine::schema::{DslSchema, ReturnType};

/// A classified token for LSP semantic highlighting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticToken {
    pub range: TextRange,
    pub token_type: TokenType,
    pub modifiers: TokenModifiers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenType {
    /// DSL constructor: `project`, `group`, `qemu`
    Function,
    /// Builder method: `.target()`, `.add()`, `.machine()`
    Method,
    /// Global constant: `LIB`, `BIN`
    Variable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenModifiers {
    pub declaration: bool,
    pub readonly: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub range: TextRange,
    pub severity: Severity,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone)]
pub struct AnalysisResult {
    pub tokens: Vec<SemanticToken>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Walk the syntax tree and produce semantic tokens + diagnostics.
///
/// Tokens are returned sorted by `(start_line, start_col)` so the LSP
/// can directly delta-encode them.
pub fn analyze(tree: &SyntaxTree, schema: &DslSchema) -> AnalysisResult {
    let mut tokens = Vec::new();
    let mut diagnostics = Vec::new();

    for stmt in &tree.statements {
        resolve_type(stmt, schema, &mut tokens, &mut diagnostics);
    }

    // Semantic-token delta encoding (the LSP wire format) requires
    // strictly increasing positions. Source-order traversal already
    // mostly produces that, but recursive arg analysis can interleave —
    // e.g. a constant identifier inside an argument is visited before
    // we get back to the outer method's name range. Sort at the end so
    // callers don't have to think about it.
    tokens.sort_by_key(|t| (t.range.start_line, t.range.start_col));

    AnalysisResult {
        tokens,
        diagnostics,
    }
}

/// Resolve a node's static "DSL type" and emit tokens / diagnostics
/// along the way.
///
/// Returns `Some(builder_name)` when the node evaluates to a known
/// builder type, `None` for `Void`, unknowns, or non-builder values
/// (literals, plain identifiers).
fn resolve_type(
    node: &Node,
    schema: &DslSchema,
    tokens: &mut Vec<SemanticToken>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<String> {
    match node {
        Node::FnCall {
            name,
            name_range,
            args,
            ..
        } => {
            if let Some(ctor) = schema.constructors.get(name) {
                tokens.push(SemanticToken {
                    range: *name_range,
                    token_type: TokenType::Function,
                    modifiers: TokenModifiers::default(),
                });
                for arg in args {
                    resolve_type(arg, schema, tokens, diagnostics);
                }
                match &ctor.returns {
                    ReturnType::Builder(name) => Some(name.clone()),
                    // SelfType on a top-level constructor is nonsensical
                    // (no `self` to refer to); treat as Void to be safe.
                    ReturnType::SelfType | ReturnType::Void => None,
                }
            } else {
                diagnostics.push(Diagnostic {
                    range: *name_range,
                    severity: Severity::Error,
                    message: format!("unknown DSL function `{name}`"),
                });
                // Still walk arguments — they may contain valid
                // identifiers/constants worth classifying.
                for arg in args {
                    resolve_type(arg, schema, tokens, diagnostics);
                }
                None
            }
        }
        Node::MethodCall {
            receiver,
            method,
            method_range,
            args,
            ..
        } => {
            let receiver_type = resolve_type(receiver, schema, tokens, diagnostics);
            match receiver_type {
                Some(builder_name) => {
                    let builder = schema.builder_types.get(&builder_name);
                    let method_info = builder.and_then(|b| b.methods.get(method));
                    if let Some(m) = method_info {
                        tokens.push(SemanticToken {
                            range: *method_range,
                            token_type: TokenType::Method,
                            modifiers: TokenModifiers::default(),
                        });
                        for arg in args {
                            resolve_type(arg, schema, tokens, diagnostics);
                        }
                        match &m.returns {
                            ReturnType::SelfType => Some(builder_name),
                            ReturnType::Builder(other) => Some(other.clone()),
                            ReturnType::Void => None,
                        }
                    } else {
                        diagnostics.push(Diagnostic {
                            range: *method_range,
                            severity: Severity::Error,
                            message: format!(
                                "`{method}` is not a method on `{builder_name}`"
                            ),
                        });
                        for arg in args {
                            resolve_type(arg, schema, tokens, diagnostics);
                        }
                        None
                    }
                }
                None => {
                    diagnostics.push(Diagnostic {
                        range: *method_range,
                        severity: Severity::Error,
                        message: format!(
                            "cannot call `.{method}()` \u{2014} receiver is not a builder"
                        ),
                    });
                    for arg in args {
                        resolve_type(arg, schema, tokens, diagnostics);
                    }
                    None
                }
            }
        }
        Node::Identifier { name, range } => {
            if schema.global_constants.contains_key(name) {
                tokens.push(SemanticToken {
                    range: *range,
                    token_type: TokenType::Variable,
                    modifiers: TokenModifiers {
                        declaration: false,
                        readonly: true,
                    },
                });
            }
            None
        }
        Node::ArrayLiteral { elements, .. } => {
            // Walk into arrays so identifiers like `[LIB, BIN]` get
            // classified. Arrays themselves carry no builder type.
            for el in elements {
                resolve_type(el, schema, tokens, diagnostics);
            }
            None
        }
        // Literals, comments, parse errors, and map literals carry no
        // type information and contain no further nodes worth walking.
        Node::StringLiteral { .. }
        | Node::IntLiteral { .. }
        | Node::BoolLiteral { .. }
        | Node::MapLiteral { .. }
        | Node::Comment { .. }
        | Node::Error { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::Parser;
    use crate::parser::rhai::RhaiParser;

    fn analyze_src(src: &str) -> AnalysisResult {
        let parser = RhaiParser::new();
        let tree = parser.parse(src);
        let schema = gluon_core::engine::dsl_schema();
        analyze(&tree, &schema)
    }

    #[test]
    fn constructor_classified_as_function() {
        let result = analyze_src(r#"project("name", "0.1.0");"#);
        let func_tokens: Vec<_> = result
            .tokens
            .iter()
            .filter(|t| t.token_type == TokenType::Function)
            .collect();
        assert_eq!(func_tokens.len(), 1);
    }

    #[test]
    fn builder_method_classified_as_method() {
        let result = analyze_src(r#"group("kernel").target("x86_64-unknown-none");"#);
        let method_tokens: Vec<_> = result
            .tokens
            .iter()
            .filter(|t| t.token_type == TokenType::Method)
            .collect();
        assert_eq!(method_tokens.len(), 1);
    }

    #[test]
    fn global_constant_classified_as_variable() {
        let result = analyze_src(r#"group("host").add("c", "p").crate_type(LIB);"#);
        let var_tokens: Vec<_> = result
            .tokens
            .iter()
            .filter(|t| t.token_type == TokenType::Variable)
            .collect();
        assert_eq!(var_tokens.len(), 1);
    }

    #[test]
    fn unknown_constructor_produces_diagnostic() {
        let result = analyze_src(r#"foobar("x");"#);
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].severity, Severity::Error);
        assert!(result.diagnostics[0].message.contains("unknown"));
    }

    #[test]
    fn invalid_method_produces_diagnostic() {
        let result = analyze_src(r#"group("x").memory(256);"#);
        let errors: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        assert!(!errors.is_empty());
        assert!(errors[0].message.contains("GroupBuilder"));
    }

    #[test]
    fn valid_chain_produces_no_diagnostics() {
        let result = analyze_src(
            r#"group("kernel").target("x86_64-unknown-none").edition("2021");"#,
        );
        let errors: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    }

    #[test]
    fn cross_builder_chain_resolves_types() {
        // group().add() returns CrateBuilder; .crate_type() lives on CrateBuilder.
        let result = analyze_src(r#"group("host").add("c", "path").crate_type(LIB);"#);
        let errors: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert!(
            result
                .tokens
                .iter()
                .any(|t| t.token_type == TokenType::Function)
        );
        assert!(
            result
                .tokens
                .iter()
                .filter(|t| t.token_type == TokenType::Method)
                .count()
                >= 2
        );
        assert!(
            result
                .tokens
                .iter()
                .any(|t| t.token_type == TokenType::Variable)
        );
    }

    #[test]
    fn void_constructor_no_methods() {
        // target() returns Void — chaining a method onto it must error.
        let result = analyze_src(r#"target("x86_64-unknown-none").edition("2021");"#);
        let errors: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        assert!(!errors.is_empty());
    }
}
