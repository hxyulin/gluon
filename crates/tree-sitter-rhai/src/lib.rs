//! Rust binding for the vendored tree-sitter-rhai grammar.
//!
//! Exposes a single [`LANGUAGE`] constant compatible with tree-sitter
//! 0.26's `Language::from(LANGUAGE)` pattern. The C symbol
//! `tree_sitter_rhai` is defined by `grammar/parser.c`, compiled by
//! `build.rs` into a static library.

use tree_sitter_language::LanguageFn;

unsafe extern "C" {
    fn tree_sitter_rhai() -> *const ();
}

/// Tree-sitter language for the Rhai scripting language.
///
/// Convert into a `tree_sitter::Language` with `LANGUAGE.into()` or
/// `Language::from(LANGUAGE)`. (We don't depend on `tree-sitter`
/// directly — only `tree-sitter-language` — so the `Language` type is
/// referenced as plain code rather than an intra-doc link.)
pub const LANGUAGE: LanguageFn = unsafe { LanguageFn::from_raw(tree_sitter_rhai) };

/// Source of the highlight queries (empty placeholder — grammar's
/// `queries/highlights.scm` is not vendored because the LSP doesn't
/// use tree-sitter for highlighting).
pub const HIGHLIGHTS_QUERY: &str = "";

#[cfg(test)]
mod tests {
    use super::LANGUAGE;

    #[test]
    fn loads_grammar() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&LANGUAGE.into())
            .expect("language version compatible");
        let tree = parser.parse("let x = 1;", None).expect("parse succeeds");
        assert!(!tree.root_node().has_error());
    }
}
