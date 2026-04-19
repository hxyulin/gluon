//! `textDocument/completion` handler — scope-aware.
//!
//! The completion set is chosen based on a coarse classification of the
//! cursor context:
//!
//! - **Top level / general expression position**: every constructor in
//!   the schema plus every global constant. This is the "what can I
//!   write here" set — `project`, `group`, `qemu`, `LIB`, ...
//! - **After a `.` in a method chain** (`group("k").`): only the methods
//!   of whatever builder the receiver evaluates to. We synthesize a
//!   complete statement out of the text-before-dot, parse it, walk the
//!   resulting AST to resolve the receiver's builder type, and look up
//!   that builder's methods in the schema. If we can't resolve a type,
//!   we return nothing rather than mislead the user with the top-level
//!   set.
//! - **Inside a `//` line comment**: nothing. Block comments are
//!   intentionally ignored — detecting them needs a real lexer pass and
//!   the false-positive cost is low (user dismisses the popup).
//!
//! The "synthesize and reparse" approach for `AfterDot` is deliberately
//! simple: it costs one extra tree-sitter parse per keystroke after a
//! dot, which is well under a millisecond on the buffers Gluon edits.
//! A more incremental approach would mean threading position info
//! through the parser; not worth it until we see a real perf problem.

use crate::parser::rhai::RhaiParser;
use crate::parser::{Node, Parser as _, SyntaxTree};
use gluon_core::engine::schema::{DslSchema, FnSig, ReturnType};
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionResponse, Documentation, InsertTextFormat,
    MarkupContent, MarkupKind, Position,
};

/// Build the completion response for a cursor position in `doc`.
pub fn complete(
    schema: &DslSchema,
    parser: &RhaiParser,
    doc: &str,
    pos: Position,
) -> CompletionResponse {
    match cursor_context(doc, pos) {
        CompletionContext::InComment => CompletionResponse::Array(Vec::new()),
        CompletionContext::TopLevel => CompletionResponse::Array(top_level_items(schema)),
        CompletionContext::AfterDot { text_before_dot } => {
            let items = match resolve_chain_builder(parser, schema, &text_before_dot) {
                Some(builder) => builder_method_items(schema, &builder),
                // Receiver type unknown — better to return nothing than
                // dump an irrelevant top-level list. The user still has
                // Ctrl-Space to retry / explore.
                None => Vec::new(),
            };
            CompletionResponse::Array(items)
        }
    }
}

/// Where the cursor is sitting, classified for completion purposes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CompletionContext {
    TopLevel,
    AfterDot { text_before_dot: String },
    InComment,
}

/// Decide which completion set applies at `pos`. See module docs for
/// the precedence rules.
fn cursor_context(doc: &str, pos: Position) -> CompletionContext {
    let line = doc.lines().nth(pos.line as usize).unwrap_or("");
    // Clamp using bytes, since LSP positions for ASCII docs are bytes.
    let col = (pos.character as usize).min(line.len());
    let prefix = &line[..col];

    if prefix.contains("//") {
        return CompletionContext::InComment;
    }

    // Strip any in-progress identifier the user is typing after the dot
    // (`group("k").tar` -> `group("k").`), then any whitespace; if what
    // remains ends with `.`, we're completing a method.
    let trimmed = prefix.trim_end_matches(|c: char| c.is_alphanumeric() || c == '_');
    let trimmed = trimmed.trim_end();
    if let Some(stripped) = trimmed.strip_suffix('.') {
        // Reconstruct the full prefix lines so multi-line chains parse.
        // For now the dot-stripped current-line prefix is enough — the
        // common case is the dot at the end of a single-line chain.
        return CompletionContext::AfterDot {
            text_before_dot: stripped.to_string(),
        };
    }
    CompletionContext::TopLevel
}

/// Build the list of every constructor + every global constant.
fn top_level_items(schema: &DslSchema) -> Vec<CompletionItem> {
    let mut out = Vec::with_capacity(schema.constructors.len() + schema.global_constants.len());
    for ctor in schema.constructors.values() {
        out.push(function_item(
            &ctor.name,
            &ctor.overloads,
            CompletionItemKind::FUNCTION,
        ));
    }
    for (name, value) in &schema.global_constants {
        out.push(constant_item(name, *value));
    }
    out
}

/// Build the list of every method on `builder_name`.
fn builder_method_items(schema: &DslSchema, builder_name: &str) -> Vec<CompletionItem> {
    let Some(builder) = schema.builder_types.get(builder_name) else {
        return Vec::new();
    };
    builder
        .methods
        .values()
        .map(|m| function_item(&m.name, &m.overloads, CompletionItemKind::METHOD))
        .collect()
}

/// One CompletionItem for a function or method, with a `name($0)`
/// snippet so the cursor lands inside the argument list.
fn function_item(name: &str, overloads: &[FnSig], kind: CompletionItemKind) -> CompletionItem {
    CompletionItem {
        label: name.to_string(),
        kind: Some(kind),
        detail: overloads.first().map(|o| o.display.clone()),
        documentation: Some(Documentation::MarkupContent(MarkupContent {
            kind: MarkupKind::Markdown,
            value: render_overloads_markdown(overloads),
        })),
        insert_text: Some(format!("{name}($0)")),
        insert_text_format: Some(InsertTextFormat::SNIPPET),
        ..Default::default()
    }
}

/// One CompletionItem for a global constant. Inserts the bare name
/// (no parens).
fn constant_item(name: &str, value: i64) -> CompletionItem {
    let body = format!("```rust\nconst {name}: i64 = {value}\n```");
    CompletionItem {
        label: name.to_string(),
        kind: Some(CompletionItemKind::VARIABLE),
        detail: Some(format!("const {name}: i64 = {value}")),
        documentation: Some(Documentation::MarkupContent(MarkupContent {
            kind: MarkupKind::Markdown,
            value: body,
        })),
        insert_text: Some(name.to_string()),
        insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
        ..Default::default()
    }
}

fn render_overloads_markdown(overloads: &[FnSig]) -> String {
    let mut out = String::from("```rust\n");
    for sig in overloads {
        out.push_str(&sig.display);
        out.push('\n');
    }
    out.push_str("```");
    out
}

/// Parse `text_before_dot` as a complete statement and return the
/// builder type the receiver evaluates to, or `None` if the chain
/// doesn't resolve to a known builder.
fn resolve_chain_builder(
    parser: &RhaiParser,
    schema: &DslSchema,
    text_before_dot: &str,
) -> Option<String> {
    let synthesized = format!("{text_before_dot};");
    let tree = parser.parse(&synthesized);
    let stmt = pick_chain_root(&tree)?;
    receiver_builder_type(stmt, schema)
}

/// Pick the rightmost FnCall/MethodCall in the parsed statements —
/// that's the expression whose type we need to resolve. If parsing
/// produced multiple statements (rare; the user's prefix included a
/// `;` mid-line), the last one is the chain we care about.
fn pick_chain_root(tree: &SyntaxTree) -> Option<&Node> {
    tree.statements
        .iter()
        .rev()
        .find(|n| matches!(n, Node::FnCall { .. } | Node::MethodCall { .. }))
}

/// Walk a chain expression, threading the receiver builder type
/// forward, and return the type of the *whole* expression. This is a
/// schema-only mirror of `analysis::resolve_type` that doesn't emit
/// tokens or diagnostics — we just want the type.
fn receiver_builder_type(node: &Node, schema: &DslSchema) -> Option<String> {
    match node {
        Node::FnCall { name, .. } => match &schema.constructors.get(name)?.returns {
            ReturnType::Builder(b) => Some(b.clone()),
            // Top-level constructors don't have a `self`, so SelfType
            // can't apply here; treat as no-builder.
            ReturnType::SelfType | ReturnType::Void => None,
        },
        Node::MethodCall {
            receiver, method, ..
        } => {
            let receiver_builder = receiver_builder_type(receiver, schema)?;
            let builder = schema.builder_types.get(&receiver_builder)?;
            let method_info = builder.methods.get(method)?;
            match &method_info.returns {
                ReturnType::SelfType => Some(receiver_builder),
                ReturnType::Builder(b) => Some(b.clone()),
                ReturnType::Void => None,
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> DslSchema {
        gluon_core::engine::dsl_schema()
    }

    #[test]
    fn completes_group_builder_methods_after_dot() {
        let schema = schema();
        let parser = RhaiParser::new();
        let doc = r#"group("kernel")."#;
        let pos = Position {
            line: 0,
            character: 16,
        };
        let result = complete(&schema, &parser, doc, pos);
        let labels: Vec<String> = match result {
            CompletionResponse::Array(items) => items.iter().map(|i| i.label.clone()).collect(),
            _ => vec![],
        };
        assert!(labels.contains(&"target".to_string()));
        assert!(labels.contains(&"add".to_string()));
        assert!(!labels.contains(&"memory".to_string()), "got: {labels:?}");
    }

    #[test]
    fn completes_all_constructors_at_top_level() {
        let schema = schema();
        let parser = RhaiParser::new();
        let result = complete(
            &schema,
            &parser,
            "",
            Position {
                line: 0,
                character: 0,
            },
        );
        let labels: Vec<String> = match result {
            CompletionResponse::Array(items) => items.iter().map(|i| i.label.clone()).collect(),
            _ => vec![],
        };
        assert!(labels.contains(&"project".to_string()));
        assert!(labels.contains(&"group".to_string()));
        assert!(labels.contains(&"qemu".to_string()));
    }

    #[test]
    fn top_level_includes_global_constants() {
        let schema = schema();
        let parser = RhaiParser::new();
        let result = complete(
            &schema,
            &parser,
            "",
            Position {
                line: 0,
                character: 0,
            },
        );
        let labels: Vec<String> = match result {
            CompletionResponse::Array(items) => items.iter().map(|i| i.label.clone()).collect(),
            _ => vec![],
        };
        assert!(labels.contains(&"LIB".to_string()));
        assert!(labels.contains(&"BIN".to_string()));
    }

    #[test]
    fn suppresses_completion_in_line_comment() {
        let schema = schema();
        let parser = RhaiParser::new();
        let result = complete(
            &schema,
            &parser,
            "// thoughts",
            Position {
                line: 0,
                character: 11,
            },
        );
        match result {
            CompletionResponse::Array(items) => assert!(items.is_empty()),
            _ => panic!("expected array"),
        }
    }

    #[test]
    fn cross_builder_chain_offers_crate_builder_methods() {
        let schema = schema();
        let parser = RhaiParser::new();
        let doc = r#"group("host").add("c", "path")."#;
        let col = doc.len() as u32;
        let result = complete(
            &schema,
            &parser,
            doc,
            Position {
                line: 0,
                character: col,
            },
        );
        let labels: Vec<String> = match result {
            CompletionResponse::Array(items) => items.iter().map(|i| i.label.clone()).collect(),
            _ => vec![],
        };
        assert!(
            labels.contains(&"crate_type".to_string()),
            "got: {labels:?}"
        );
        assert!(labels.contains(&"edition".to_string()));
    }

    #[test]
    fn after_dot_with_partial_method_name_still_works() {
        // Cursor in middle of a partial method name after the dot —
        // the trim should peel off the partial identifier.
        let schema = schema();
        let parser = RhaiParser::new();
        let doc = r#"group("k").tar"#;
        let result = complete(
            &schema,
            &parser,
            doc,
            Position {
                line: 0,
                character: doc.len() as u32,
            },
        );
        let labels: Vec<String> = match result {
            CompletionResponse::Array(items) => items.iter().map(|i| i.label.clone()).collect(),
            _ => vec![],
        };
        assert!(labels.contains(&"target".to_string()), "got: {labels:?}");
    }

    #[test]
    fn every_function_item_is_a_snippet() {
        let resp = complete(
            &schema(),
            &RhaiParser::new(),
            "",
            Position {
                line: 0,
                character: 0,
            },
        );
        let CompletionResponse::Array(items) = resp else {
            panic!("expected array");
        };
        for item in items {
            if item.kind == Some(CompletionItemKind::FUNCTION) {
                assert_eq!(item.insert_text_format, Some(InsertTextFormat::SNIPPET));
                assert!(item.insert_text.as_deref().unwrap_or("").contains("$0"));
            }
        }
    }
}
