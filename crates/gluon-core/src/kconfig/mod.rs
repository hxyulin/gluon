//! `.kconfig` file parser and loader.
//!
//! Gluon config options can be declared in two equivalent ways:
//!
//! 1. **Inline in Rhai** via the `engine::builders::config` module
//!    (`config_bool("NAME", ...)` and friends).
//! 2. **Externally in a `.kconfig` file** loaded from `gluon.rhai` via the
//!    `load_kconfig("./options.kconfig")` Rhai function.
//!
//! Both paths produce the same [`gluon_model::ConfigOptionDef`] and
//! [`gluon_model::PresetDef`] shapes, so downstream resolution and codegen
//! are unaware of the source format. The `.kconfig` form additionally
//! populates [`gluon_model::ConfigOptionDef::depends_on_expr`] and
//! [`gluon_model::ConfigOptionDef::visible_if_expr`] with full boolean
//! expressions (`&&`, `||`, `!`, grouping), which the resolver evaluates
//! semantically rather than as a flat AND-of-symbols.
//!
//! The module is split into four layers:
//!
//! - [`lexer`] — hand-rolled tokenizer producing `Token`s with spans.
//! - `parser` (arrives in chunk K3) — recursive-descent parser over tokens.
//! - `ast` (arrives in chunk K3) — pure data AST shape.
//! - `lower` (arrives in chunk K6) — lowers the AST to model types,
//!   reusing the existing conversion logic where practical.

pub mod ast;
pub mod lexer;
pub mod loader;
pub mod lower;
pub mod parser;

pub use loader::load_kconfig;
pub use lower::Lowered;

use crate::error::Diagnostic;
use gluon_model::{Expr, SourceSpan};
use std::path::Path;

/// Parse a standalone boolean expression string using the same grammar
/// the `.kconfig` parser accepts for `depends_on` / `visible_if`
/// property values (full `&&`, `||`, `!`, `(...)` precedence).
///
/// This is how the Rhai builder exposes boolean-expression dependencies
/// (`.depends_on_expr("A && !B")`) without duplicating the grammar.
///
/// On success, returns the parsed [`Expr`]. On failure, returns a single
/// [`Diagnostic`] attributed to `origin` — the source location of the
/// Rhai call site — so the user sees the error pointing into their
/// `gluon.rhai` rather than into a synthetic internal buffer. The lexer
/// and parser both emit their own spans against an internal
/// `<depends_on_expr>` pseudo-path; we deliberately discard those and
/// re-attach `origin` because per-character offsets inside a Rhai string
/// literal are not visible to us at this layer (Rhai only gives us the
/// line/column of the function call itself).
pub fn parse_bool_expr(input: &str, origin: SourceSpan) -> Result<Expr, Diagnostic> {
    let pseudo_path = Path::new("<depends_on_expr>");
    let tokens =
        lexer::lex(input, pseudo_path).map_err(|diags| first_or_default(diags, &origin))?;
    parser::parse_standalone_expr(&tokens).map_err(|diags| first_or_default(diags, &origin))
}

fn first_or_default(diags: Vec<Diagnostic>, origin: &SourceSpan) -> Diagnostic {
    let msg = diags
        .into_iter()
        .next()
        .map(|d| d.message)
        .unwrap_or_else(|| "failed to parse boolean expression".to_string());
    Diagnostic::error(format!("invalid boolean expression: {msg}")).with_span(origin.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gluon_model::Expr;

    fn origin() -> SourceSpan {
        SourceSpan::point("test.rhai", 1, 1)
    }

    fn parse(s: &str) -> Result<Expr, Diagnostic> {
        parse_bool_expr(s, origin())
    }

    #[test]
    fn parses_bare_ident() {
        assert_eq!(parse("A").unwrap(), Expr::Ident("A".to_string()));
    }

    #[test]
    fn parses_and() {
        assert_eq!(
            parse("A && B").unwrap(),
            Expr::And(vec![
                Expr::Ident("A".to_string()),
                Expr::Ident("B".to_string())
            ])
        );
    }

    #[test]
    fn parses_or_with_not() {
        assert_eq!(
            parse("A || !B").unwrap(),
            Expr::Or(vec![
                Expr::Ident("A".to_string()),
                Expr::Not(Box::new(Expr::Ident("B".to_string()))),
            ])
        );
    }

    #[test]
    fn parses_grouped() {
        assert_eq!(
            parse("(A || B) && !C").unwrap(),
            Expr::And(vec![
                Expr::Or(vec![
                    Expr::Ident("A".to_string()),
                    Expr::Ident("B".to_string())
                ]),
                Expr::Not(Box::new(Expr::Ident("C".to_string()))),
            ])
        );
    }

    #[test]
    fn parses_literal_true_false() {
        assert_eq!(parse("true").unwrap(), Expr::Const(true));
        assert_eq!(parse("false").unwrap(), Expr::Const(false));
    }

    #[test]
    fn error_on_empty() {
        let d = parse("").unwrap_err();
        assert_eq!(d.span.as_ref().unwrap().file, origin().file);
        assert!(d.message.contains("invalid boolean expression"));
    }

    #[test]
    fn error_on_dangling_and() {
        assert!(parse("A &&").is_err());
    }

    #[test]
    fn error_on_trailing_tokens() {
        assert!(parse("A B").is_err());
    }

    #[test]
    fn error_span_points_to_origin() {
        let o = SourceSpan::point("my.rhai", 42, 7);
        let d = parse_bool_expr("A && ", o.clone()).unwrap_err();
        assert_eq!(d.span.unwrap(), o);
    }
}
