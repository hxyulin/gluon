//! `textDocument/hover` handler.
//!
//! Looks up the word at the cursor in the [`DslIndex`] and returns
//! every overload as a single Markdown code block. Returns `None`
//! when the cursor is on whitespace, inside a comment line, or on an
//! identifier not known to the index (script-defined helpers,
//! variables, etc.) — the LSP protocol expects `None` to mean "no
//! hover content" rather than an empty response.

use crate::index::DslIndex;
use crate::word::word_at;
use lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};

pub fn hover(index: &DslIndex, doc: &str, pos: Position) -> Option<Hover> {
    let word = word_at(doc, pos)?;
    let sym = index.get(&word)?;
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: sym.hover_markdown(),
        }),
        range: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_index() -> DslIndex {
        DslIndex::from_signatures(vec![
            "target(_: string)".to_string(),
            "target(_: string, _: string)".to_string(),
            "group(_: string) -> GroupBuilder".to_string(),
        ])
    }

    fn pos(line: u32, col: u32) -> Position {
        Position {
            line,
            character: col,
        }
    }

    #[test]
    fn hovers_on_known_identifier() {
        let h = hover(&sample_index(), "target(\"foo\")", pos(0, 3)).expect("some hover");
        let HoverContents::Markup(md) = h.contents else {
            panic!("expected markup");
        };
        assert!(md.value.contains("target(_: string)"));
        assert!(md.value.contains("target(_: string, _: string)"));
    }

    #[test]
    fn returns_none_on_unknown_identifier() {
        assert!(hover(&sample_index(), "zzz", pos(0, 1)).is_none());
    }

    #[test]
    fn returns_none_on_whitespace() {
        assert!(hover(&sample_index(), "   target", pos(0, 1)).is_none());
    }
}
