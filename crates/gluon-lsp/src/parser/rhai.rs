//! Tree-sitter-rhai parser implementation.
//!
//! Maps the tree-sitter-rhai CST to the concrete [`Node`] enum used by
//! the semantic analysis layer. The grammar's node taxonomy was
//! discovered empirically (see the `dump_sexps` test) — key shapes:
//!
//! ```text
//! Rhai
//!   Stmt
//!     Item
//!       (Doc (comment_line_doc | comment_block_doc))*  // optional leading docs
//!       Expr  // the actual statement value
//!
//! Expr is one of:
//!   ExprCall { fn_name: Expr, ArgList { args: Expr* } }
//!   ExprDotAccess { Expr, Expr }   // [receiver, member]; positional, no field names
//!   ExprPath { Path { ident+ } }   // bare identifier or dotted path
//!   ExprLit  { Lit  { lit_str | lit_int | lit_float | lit_bool | LitUnit } }
//!   ExprArray { Expr* }
//!   ExprObject { ObjectField* }
//!   ...
//! ```
//!
//! Method chains nest as `ExprCall(fn_name=ExprDotAccess(ExprCall(...),
//! ExprPath(method)), args)`. We unwrap that into our flat
//! `MethodCall { receiver, method, args }` form.

use super::{Node, Parser, SyntaxTree, TextRange};

/// Parser backed by the vendored tree-sitter-rhai grammar.
pub struct RhaiParser;

impl RhaiParser {
    pub fn new() -> Self {
        Self
    }
}

impl Default for RhaiParser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser for RhaiParser {
    fn parse(&self, source: &str) -> SyntaxTree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rhai::LANGUAGE.into())
            .expect("tree-sitter-rhai grammar version compatible with tree-sitter 0.26");
        let tree = parser
            .parse(source, None)
            .expect("tree-sitter parse never returns None on string input without a cancel flag");

        let root = tree.root_node();
        let mut statements = Vec::new();
        let mut errors = Vec::new();

        // Root is `Rhai`. Its named children are `Stmt` nodes.
        let mut cursor = root.walk();
        for stmt in root.named_children(&mut cursor) {
            collect_from_stmt(stmt, source, &mut statements, &mut errors);
        }

        SyntaxTree { statements, errors }
    }
}

fn ts_range(node: &tree_sitter::Node) -> TextRange {
    let start = node.start_position();
    let end = node.end_position();
    TextRange {
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        start_line: start.row as u32,
        start_col: start.column as u32,
        end_line: end.row as u32,
        end_col: end.column as u32,
    }
}

/// Walk a `Stmt` (or any wrapper that may contain an `Item`/`Expr`)
/// and append the lowered top-level statement(s) into `out`.
///
/// We treat each leading `Doc` as its own top-level `Comment` node so
/// the LSP can highlight / hover on doc comments independently of the
/// item they document. The `Expr` child becomes the actual statement.
fn collect_from_stmt(
    node: tree_sitter::Node,
    source: &str,
    out: &mut Vec<Node>,
    errors: &mut Vec<TextRange>,
) {
    // Errors at any level: surface and stop descending — child nodes
    // under an ERROR are unreliable.
    if node.is_error() || node.kind() == "ERROR" {
        let range = ts_range(&node);
        errors.push(range);
        out.push(Node::Error { range });
        return;
    }

    match node.kind() {
        "Stmt" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_from_stmt(child, source, out, errors);
            }
        }
        "Item" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                match child.kind() {
                    "Doc" => out.push(Node::Comment {
                        range: ts_range(&child),
                    }),
                    "Expr" => {
                        if let Some(n) = lower_expr(child, source, errors) {
                            out.push(n);
                        }
                    }
                    _ => {
                        // Defensive: unknown child under Item.
                        if let Some(n) = lower_expr(child, source, errors) {
                            out.push(n);
                        }
                    }
                }
            }
        }
        // Top-level comments not attached to an item (rare, since the
        // grammar prefers to attach them to the following item).
        "comment_line_doc" | "comment_block_doc" | "Doc" => {
            out.push(Node::Comment {
                range: ts_range(&node),
            });
        }
        _ => {
            if let Some(n) = lower_expr(node, source, errors) {
                out.push(n);
            }
        }
    }
}

/// Lower an `Expr` (or any node that *unwraps to* an Expr) into our
/// `Node`. Returns `None` for purely structural nodes with no useful
/// content — but in practice we try hard to descend into something
/// recognisable.
fn lower_expr(
    node: tree_sitter::Node,
    source: &str,
    errors: &mut Vec<TextRange>,
) -> Option<Node> {
    if node.is_error() || node.kind() == "ERROR" {
        let range = ts_range(&node);
        errors.push(range);
        return Some(Node::Error { range });
    }

    match node.kind() {
        // `Expr` is a transparent wrapper — descend to its single named child.
        "Expr" => first_named(&node).and_then(|c| lower_expr(c, source, errors)),

        // ExprCall { fn_name: Expr, ArgList }
        "ExprCall" => lower_call(node, source, errors),

        // ExprDotAccess at expression position (not as a callee) is a
        // field access. We don't have a `FieldAccess` variant, so we
        // collapse `recv.field` to whichever side is more useful — for
        // our DSL surface, this only happens inside argument positions
        // (rare), so we lower the receiver and ignore the field. This
        // is a known simplification; the semantic layer doesn't yet
        // care about non-call dot-access.
        "ExprDotAccess" => first_named(&node).and_then(|c| lower_expr(c, source, errors)),

        // Bare path / identifier: `LIB`, `foo::bar`. We surface the
        // full source text as the identifier name; multi-segment paths
        // are rare in the gluon DSL, but if they appear the semantic
        // layer can split on `::`.
        "ExprPath" | "Path" => Some(Node::Identifier {
            name: text_of(&node, source).to_string(),
            range: ts_range(&node),
        }),
        "ExprIdent" | "ident" => Some(Node::Identifier {
            name: text_of(&node, source).to_string(),
            range: ts_range(&node),
        }),

        // Literals: ExprLit > Lit > (lit_str | lit_int | lit_bool | lit_float | LitUnit)
        "ExprLit" | "Lit" => first_named(&node).and_then(|c| lower_expr(c, source, errors)),
        "lit_str" => {
            let raw = text_of(&node, source);
            Some(Node::StringLiteral {
                value: strip_string_quotes(raw).to_string(),
                range: ts_range(&node),
            })
        }
        "lit_int" => {
            let text = text_of(&node, source);
            let cleaned: String = text.chars().filter(|c| *c != '_').collect();
            let value = parse_int(&cleaned).unwrap_or(0);
            Some(Node::IntLiteral {
                value,
                range: ts_range(&node),
            })
        }
        "lit_float" => {
            // No dedicated Float variant in our AST; surface as IntLiteral 0
            // so the position is preserved. The semantic layer does not
            // currently distinguish numeric subtypes for DSL builders.
            Some(Node::IntLiteral {
                value: 0,
                range: ts_range(&node),
            })
        }
        "lit_bool" => Some(Node::BoolLiteral {
            value: text_of(&node, source) == "true",
            range: ts_range(&node),
        }),
        "LitUnit" => None,

        // Arrays
        "ExprArray" => {
            let mut elements = Vec::new();
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if let Some(n) = lower_expr(child, source, errors) {
                    elements.push(n);
                }
            }
            Some(Node::ArrayLiteral {
                elements,
                range: ts_range(&node),
            })
        }

        // Maps
        "ExprObject" => Some(Node::MapLiteral {
            range: ts_range(&node),
        }),

        // Parenthesised expression — descend.
        "ExprParen" => first_named(&node).and_then(|c| lower_expr(c, source, errors)),

        // Comments encountered at expression position (shouldn't
        // really happen, but be robust).
        "Doc" | "comment_line_doc" | "comment_block_doc" => Some(Node::Comment {
            range: ts_range(&node),
        }),

        // Anything else: try to descend through the first named child
        // so we don't silently drop something we just haven't enumerated.
        _ => first_named(&node).and_then(|c| lower_expr(c, source, errors)),
    }
}

/// Lower an `ExprCall` to either `FnCall` or `MethodCall`.
///
/// Shape: `ExprCall { fn_name: Expr, ArgList { args: Expr* } }`.
/// If `fn_name` resolves through to `ExprDotAccess(receiver_expr,
/// member_path_expr)`, it's a method call.
fn lower_call(
    node: tree_sitter::Node,
    source: &str,
    errors: &mut Vec<TextRange>,
) -> Option<Node> {
    let fn_name = node.child_by_field_name("fn_name")?;
    let arg_list = find_child_by_kind(&node, "ArgList");
    let args = collect_args(arg_list, source, errors);
    let range = ts_range(&node);

    // `fn_name` is always wrapped in `Expr`; unwrap one level.
    let callee = if fn_name.kind() == "Expr" {
        first_named(&fn_name).unwrap_or(fn_name)
    } else {
        fn_name
    };

    if callee.kind() == "ExprDotAccess" {
        // ExprDotAccess has two named Expr children: [receiver, member].
        let mut cursor = callee.walk();
        let mut named = callee.named_children(&mut cursor);
        let recv_expr = named.next()?;
        let member_expr = named.next()?;

        let receiver_node = lower_expr(recv_expr, source, errors)?;

        // The member is `Expr > ExprPath > Path > ident`. Drill down
        // to find the method name string + range.
        let (method_name, method_range) = extract_method_name(member_expr, source)?;

        return Some(Node::MethodCall {
            receiver: Box::new(receiver_node),
            method: method_name,
            method_range,
            args,
            range,
        });
    }

    // Plain function call. The callee should be an ExprPath identifying
    // the function name.
    let (name, name_range) = extract_method_name(callee, source)
        .unwrap_or_else(|| (text_of(&callee, source).to_string(), ts_range(&callee)));

    Some(Node::FnCall {
        name,
        name_range,
        args,
        range,
    })
}

/// Walk through `Expr > ExprPath > Path > ident` (or any prefix of
/// that) and return the identifier text + range. Used for both
/// function names and method names.
fn extract_method_name(
    mut node: tree_sitter::Node,
    source: &str,
) -> Option<(String, TextRange)> {
    // Unwrap up to a few layers; bail if we don't find an `ident`.
    for _ in 0..6 {
        match node.kind() {
            "ident" => {
                return Some((text_of(&node, source).to_string(), ts_range(&node)));
            }
            "Path" => {
                // Multi-segment paths: take the last ident as the
                // "method name" but report the full path range so
                // diagnostics underline the whole reference.
                let mut cursor = node.walk();
                let last_ident = node
                    .named_children(&mut cursor)
                    .filter(|c| c.kind() == "ident")
                    .last();
                if let Some(i) = last_ident {
                    let full_range = ts_range(&node);
                    return Some((text_of(&i, source).to_string(), full_range));
                }
                return None;
            }
            _ => {
                let next = first_named(&node)?;
                node = next;
            }
        }
    }
    None
}

fn collect_args(
    arg_list: Option<tree_sitter::Node>,
    source: &str,
    errors: &mut Vec<TextRange>,
) -> Vec<Node> {
    let Some(args_node) = arg_list else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = args_node.walk();
    for child in args_node.named_children(&mut cursor) {
        if let Some(n) = lower_expr(child, source, errors) {
            out.push(n);
        }
    }
    out
}

fn first_named<'a>(node: &tree_sitter::Node<'a>) -> Option<tree_sitter::Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).next()
}

fn find_child_by_kind<'a>(
    node: &tree_sitter::Node<'a>,
    kind: &str,
) -> Option<tree_sitter::Node<'a>> {
    let mut cursor = node.walk();
    node.children(&mut cursor).find(|c| c.kind() == kind)
}

fn text_of<'a>(node: &tree_sitter::Node<'_>, source: &'a str) -> &'a str {
    &source[node.start_byte()..node.end_byte()]
}

fn strip_string_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &s[1..s.len() - 1];
        }
    }
    s
}

fn parse_int(s: &str) -> Option<i64> {
    if let Some(rest) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        i64::from_str_radix(rest, 16).ok()
    } else if let Some(rest) = s.strip_prefix("0o").or_else(|| s.strip_prefix("0O")) {
        i64::from_str_radix(rest, 8).ok()
    } else if let Some(rest) = s.strip_prefix("0b").or_else(|| s.strip_prefix("0B")) {
        i64::from_str_radix(rest, 2).ok()
    } else {
        s.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Print raw S-expressions for discovery / debugging.
    /// Run with: `cargo test -p gluon-lsp parser::rhai::tests::dump_sexps -- --nocapture`
    #[test]
    fn dump_sexps() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rhai::LANGUAGE.into())
            .unwrap();
        for src in &[
            "project(\"my-project\", \"0.1.0\");",
            "group(\"kernel\").target(\"x86_64-unknown-none\").edition(\"2021\");",
            "LIB;",
            "qemu().memory(256);",
            "profile(\"dev\").debug_info(true);",
            "project(\"name\",);",
            "// hello\nproject(\"x\");",
        ] {
            let tree = parser.parse(*src, None).unwrap();
            eprintln!("SRC: {}\nSEXP: {}\n", src, tree.root_node().to_sexp());
        }
    }

    fn parse(src: &str) -> SyntaxTree {
        RhaiParser::new().parse(src)
    }

    #[test]
    fn parses_simple_fn_call() {
        let tree = parse("project(\"my-project\", \"0.1.0\");");
        assert_eq!(tree.statements.len(), 1, "tree: {:?}", tree);
        match &tree.statements[0] {
            Node::FnCall { name, args, .. } => {
                assert_eq!(name, "project");
                assert_eq!(args.len(), 2);
                match &args[0] {
                    Node::StringLiteral { value, .. } => assert_eq!(value, "my-project"),
                    other => panic!("arg 0 not string: {:?}", other),
                }
                match &args[1] {
                    Node::StringLiteral { value, .. } => assert_eq!(value, "0.1.0"),
                    other => panic!("arg 1 not string: {:?}", other),
                }
            }
            other => panic!("not FnCall: {:?}", other),
        }
    }

    #[test]
    fn parses_method_chain() {
        let tree =
            parse("group(\"kernel\").target(\"x86_64-unknown-none\").edition(\"2021\");");
        assert_eq!(tree.statements.len(), 1, "tree: {:?}", tree);

        // Outermost: .edition("2021"). Receiver: .target(...). Receiver
        // of that: group("kernel").
        let edition = &tree.statements[0];
        let target = match edition {
            Node::MethodCall {
                method,
                args,
                receiver,
                ..
            } => {
                assert_eq!(method, "edition");
                assert_eq!(args.len(), 1);
                receiver.as_ref()
            }
            other => panic!("not MethodCall: {:?}", other),
        };
        let group = match target {
            Node::MethodCall {
                method,
                receiver,
                args,
                ..
            } => {
                assert_eq!(method, "target");
                assert_eq!(args.len(), 1);
                receiver.as_ref()
            }
            other => panic!("inner not MethodCall: {:?}", other),
        };
        match group {
            Node::FnCall { name, args, .. } => {
                assert_eq!(name, "group");
                assert_eq!(args.len(), 1);
            }
            other => panic!("base not FnCall: {:?}", other),
        }
    }

    #[test]
    fn parses_bare_identifier() {
        let tree = parse("LIB;");
        assert_eq!(tree.statements.len(), 1, "tree: {:?}", tree);
        match &tree.statements[0] {
            Node::Identifier { name, .. } => assert_eq!(name, "LIB"),
            other => panic!("not Identifier: {:?}", other),
        }
    }

    #[test]
    fn parses_int_literal_arg() {
        let tree = parse("qemu().memory(256);");
        assert_eq!(tree.statements.len(), 1, "tree: {:?}", tree);
        match &tree.statements[0] {
            Node::MethodCall { method, args, .. } => {
                assert_eq!(method, "memory");
                assert_eq!(args.len(), 1);
                match &args[0] {
                    Node::IntLiteral { value, .. } => assert_eq!(*value, 256),
                    other => panic!("arg not IntLiteral: {:?}", other),
                }
            }
            other => panic!("not MethodCall: {:?}", other),
        }
    }

    #[test]
    fn parses_bool_literal_arg() {
        let tree = parse("profile(\"dev\").debug_info(true);");
        assert_eq!(tree.statements.len(), 1, "tree: {:?}", tree);
        match &tree.statements[0] {
            Node::MethodCall { method, args, .. } => {
                assert_eq!(method, "debug_info");
                assert_eq!(args.len(), 1);
                match &args[0] {
                    Node::BoolLiteral { value, .. } => assert!(*value),
                    other => panic!("arg not BoolLiteral: {:?}", other),
                }
            }
            other => panic!("not MethodCall: {:?}", other),
        }
    }

    #[test]
    fn error_recovery_on_broken_input() {
        // tree-sitter is generous and may parse trailing-comma calls
        // either as a clean call or with an inner error. The contract
        // is: no panic, deterministic output, and either we still see
        // the call OR we see a parse error.
        let tree = parse("project(\"name\",);");
        let has_call = tree
            .statements
            .iter()
            .any(|n| matches!(n, Node::FnCall { name, .. } if name == "project"));
        let has_error = !tree.errors.is_empty()
            || tree
                .statements
                .iter()
                .any(|n| matches!(n, Node::Error { .. }));
        assert!(
            has_call || has_error,
            "expected call or error; got: {:?}",
            tree
        );
    }

    #[test]
    fn parses_comments() {
        let tree = parse("// leading\nproject(\"x\");");
        let has_comment = tree
            .statements
            .iter()
            .any(|n| matches!(n, Node::Comment { .. }));
        let has_call = tree
            .statements
            .iter()
            .any(|n| matches!(n, Node::FnCall { name, .. } if name == "project"));
        assert!(has_comment, "no comment surfaced; tree: {:?}", tree);
        assert!(has_call, "call missing; tree: {:?}", tree);
    }
}
