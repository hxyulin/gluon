//! `textDocument/hover` handler — type-aware.
//!
//! Walks the parsed AST to find the deepest call node whose name (or
//! method) range contains the cursor, then renders a markdown body
//! with the resolved signature:
//!
//! - **Constructor** (`project`, `group`, ...): `name(params) -> ReturnType`
//!   plus every overload.
//! - **Builder method** (`.target`, `.add`, ...): `BuilderType.method(params)
//!   -> ReturnType`. The receiver type is resolved by walking the chain
//!   the same way `analysis::resolve_type` does.
//! - **Global constant** (`LIB`, `BIN`, ...): `const NAME: i64 = value`.
//!
//! Returns `None` for unknowns — the LSP spec treats `None` as "no
//! hover content", which is what editors display as "nothing here".

use crate::parser::rhai::RhaiParser;
use crate::parser::{Node, Parser as _, TextRange};
use gluon_core::engine::schema::{DslSchema, FnSig, ReturnType};
use lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};

pub fn hover(schema: &DslSchema, parser: &RhaiParser, doc: &str, pos: Position) -> Option<Hover> {
    let tree = parser.parse(doc);

    // Walk every statement; the first hit wins. Statements are
    // disjoint, so there's no ambiguity here.
    for stmt in &tree.statements {
        if let Some(md) = hover_in_node(stmt, schema, pos, None) {
            return Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: md,
                }),
                range: None,
            });
        }
    }
    None
}

/// Recursive search for the cursor.
///
/// `receiver_type` is the builder type that this node is being called
/// against — `Some` only when we recursed into the receiver of a
/// MethodCall to look for a deeper hit. For top-level FnCalls it is
/// always `None`.
fn hover_in_node(
    node: &Node,
    schema: &DslSchema,
    pos: Position,
    _receiver_type: Option<&str>,
) -> Option<String> {
    match node {
        Node::FnCall {
            name,
            name_range,
            args,
            ..
        } => {
            // Try args first so a nested call wins over the outer
            // function name (deepest match semantics).
            for arg in args {
                if let Some(md) = hover_in_node(arg, schema, pos, None) {
                    return Some(md);
                }
            }
            if range_contains(name_range, pos) {
                return render_constructor(schema, name);
            }
            None
        }
        Node::MethodCall {
            receiver,
            method,
            method_range,
            args,
            ..
        } => {
            // Recurse into receiver first — if the cursor is on
            // something earlier in the chain, that's the deeper hit.
            if let Some(md) = hover_in_node(receiver, schema, pos, None) {
                return Some(md);
            }
            for arg in args {
                if let Some(md) = hover_in_node(arg, schema, pos, None) {
                    return Some(md);
                }
            }
            if range_contains(method_range, pos) {
                let receiver_builder = chain_type(receiver, schema)?;
                return render_method(schema, &receiver_builder, method);
            }
            None
        }
        Node::Identifier { name, range } => {
            if range_contains(range, pos) {
                if let Some(value) = schema.global_constants.get(name) {
                    return Some(render_constant(name, *value));
                }
            }
            None
        }
        Node::ArrayLiteral { elements, .. } => {
            for el in elements {
                if let Some(md) = hover_in_node(el, schema, pos, None) {
                    return Some(md);
                }
            }
            None
        }
        Node::StringLiteral { .. }
        | Node::IntLiteral { .. }
        | Node::BoolLiteral { .. }
        | Node::MapLiteral { .. }
        | Node::Comment { .. }
        | Node::Error { .. } => None,
    }
}

/// Resolve the type a chain expression evaluates to. Mirrors
/// `analysis::resolve_type` but doesn't emit tokens or diagnostics.
fn chain_type(node: &Node, schema: &DslSchema) -> Option<String> {
    match node {
        Node::FnCall { name, .. } => match &schema.constructors.get(name)?.returns {
            ReturnType::Builder(b) => Some(b.clone()),
            _ => None,
        },
        Node::MethodCall {
            receiver, method, ..
        } => {
            let recv_builder = chain_type(receiver, schema)?;
            let m = schema
                .builder_types
                .get(&recv_builder)?
                .methods
                .get(method)?;
            match &m.returns {
                ReturnType::SelfType => Some(recv_builder),
                ReturnType::Builder(b) => Some(b.clone()),
                ReturnType::Void => None,
            }
        }
        _ => None,
    }
}

/// True when `pos` falls inside `range`. LSP positions are zero-based
/// `(line, character)`; tree-sitter `start_col`/`end_col` are the
/// equivalent UTF-8 columns for the ASCII docs Gluon edits. Boundary
/// behavior: an end column equal to `pos.character` is treated as a
/// hit, so cursors immediately after the last char of a token (the
/// LSP "between characters" position) still resolve.
fn range_contains(r: &TextRange, pos: Position) -> bool {
    let line = pos.line;
    let col = pos.character;
    if line < r.start_line || line > r.end_line {
        return false;
    }
    if line == r.start_line && col < r.start_col {
        return false;
    }
    if line == r.end_line && col > r.end_col {
        return false;
    }
    true
}

fn render_constructor(schema: &DslSchema, name: &str) -> Option<String> {
    let ctor = schema.constructors.get(name)?;
    let header = format!(
        "{name} -> {return_type}",
        return_type = render_return(&ctor.returns)
    );
    Some(render_overloads(&header, &ctor.overloads))
}

fn render_method(schema: &DslSchema, builder: &str, method: &str) -> Option<String> {
    let m = schema.builder_types.get(builder)?.methods.get(method)?;
    let header = format!(
        "{builder}.{method} -> {return_type}",
        return_type = render_return(&m.returns)
    );
    Some(render_overloads(&header, &m.overloads))
}

fn render_return(ret: &ReturnType) -> String {
    match ret {
        ReturnType::SelfType => "Self".to_string(),
        ReturnType::Builder(b) => b.clone(),
        ReturnType::Void => "()".to_string(),
    }
}

fn render_overloads(header: &str, overloads: &[FnSig]) -> String {
    let mut out = String::new();
    out.push_str("```rust\n");
    out.push_str(header);
    out.push('\n');
    for sig in overloads {
        out.push_str(&sig.display);
        out.push('\n');
    }
    out.push_str("```");
    out
}

fn render_constant(name: &str, value: i64) -> String {
    format!("```rust\nconst {name}: i64 = {value}\n```")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> DslSchema {
        gluon_core::engine::dsl_schema()
    }

    #[test]
    fn hover_on_constructor_shows_signature() {
        let parser = RhaiParser::new();
        let doc = r#"project("my-project", "0.1.0");"#;
        let pos = Position {
            line: 0,
            character: 3,
        };
        let result = hover(&schema(), &parser, doc, pos);
        let md = match result.unwrap().contents {
            HoverContents::Markup(m) => m.value,
            _ => String::new(),
        };
        assert!(md.contains("project"), "got: {md}");
    }

    #[test]
    fn hover_on_method_shows_builder_context() {
        let parser = RhaiParser::new();
        let doc = r#"group("kernel").target("x86_64-unknown-none");"#;
        let pos = Position {
            line: 0,
            character: 17,
        };
        let result = hover(&schema(), &parser, doc, pos);
        let md = match result.unwrap().contents {
            HoverContents::Markup(m) => m.value,
            _ => String::new(),
        };
        assert!(md.contains("GroupBuilder"), "got: {md}");
        assert!(md.contains("target"));
    }

    #[test]
    fn hover_on_unknown_returns_none() {
        let parser = RhaiParser::new();
        let result = hover(
            &schema(),
            &parser,
            "zzz",
            Position {
                line: 0,
                character: 1,
            },
        );
        assert!(result.is_none());
    }

    #[test]
    fn hover_on_global_constant_shows_value() {
        let parser = RhaiParser::new();
        let doc = "group(\"h\").add(\"c\", \"p\").crate_type(LIB);";
        let col = doc.find("LIB").unwrap() as u32 + 1;
        let result = hover(
            &schema(),
            &parser,
            doc,
            Position {
                line: 0,
                character: col,
            },
        );
        let md = match result.unwrap().contents {
            HoverContents::Markup(m) => m.value,
            _ => String::new(),
        };
        assert!(md.contains("LIB"));
    }

    #[test]
    fn hover_on_whitespace_returns_none() {
        let parser = RhaiParser::new();
        let result = hover(
            &schema(),
            &parser,
            "   group(\"x\")",
            Position {
                line: 0,
                character: 1,
            },
        );
        assert!(result.is_none());
    }
}
