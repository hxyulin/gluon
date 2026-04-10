//! `textDocument/completion` handler.
//!
//! Phase 1 scope: return every symbol in the [`DslIndex`] as a
//! completion item, with the detail line showing the first overload's
//! signature and the documentation showing every overload. The client
//! does the prefix filtering against `label`, so we do not need to
//! parse the buffer at all for this step — we only look at the buffer
//! to decide whether the cursor is inside a comment or string, which
//! is where completions would be actively wrong.
//!
//! AST-aware filtering (e.g. "after `group(\"x\").`, offer only
//! GroupBuilder methods") is explicit Phase 2 work and deliberately
//! NOT done here.

use crate::index::DslIndex;
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionResponse, Documentation, InsertTextFormat,
    MarkupContent, MarkupKind, Position,
};

/// Build the completion response for a cursor position in `doc`.
///
/// `doc` and `pos` are currently unused beyond a comment-suppression
/// check; they are in the signature so Phase 2 (AST-aware completion)
/// can implement real filtering without reshuffling call sites.
pub fn complete(index: &DslIndex, doc: &str, pos: Position) -> CompletionResponse {
    if inside_line_comment(doc, pos) {
        return CompletionResponse::Array(Vec::new());
    }

    let items: Vec<CompletionItem> = index
        .iter()
        .map(|sym| CompletionItem {
            label: sym.name.clone(),
            kind: Some(CompletionItemKind::FUNCTION),
            detail: Some(sym.completion_detail()),
            documentation: Some(Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: sym.hover_markdown(),
            })),
            // `$0` is a snippet tab-stop: the user's cursor lands
            // inside the parens after the completion is inserted, so
            // they can type arguments immediately.
            insert_text: Some(format!("{}($0)", sym.name)),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        })
        .collect();
    CompletionResponse::Array(items)
}

/// True if the cursor is after a `//` on the same line. Rhai uses
/// `//` for line comments and `/* */` for block comments; we only
/// suppress line-comment contexts here because block-comment detection
/// would need a real lexer pass. The cost of occasionally offering
/// completions inside a `/* */` is low — the user just dismisses them.
fn inside_line_comment(doc: &str, pos: Position) -> bool {
    let Some(line) = doc.lines().nth(pos.line as usize) else {
        return false;
    };
    let col = (pos.character as usize).min(line.chars().count());
    let prefix: String = line.chars().take(col).collect();
    prefix.contains("//")
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
    fn returns_every_symbol() {
        let idx = sample_index();
        let resp = complete(&idx, "", pos(0, 0));
        match resp {
            CompletionResponse::Array(items) => {
                let labels: Vec<_> = items.iter().map(|i| i.label.clone()).collect();
                assert!(labels.contains(&"target".to_string()));
                assert!(labels.contains(&"group".to_string()));
                assert_eq!(items.len(), 2, "should dedupe overloads per name");
            }
            _ => panic!("expected array response"),
        }
    }

    #[test]
    fn every_item_is_a_snippet() {
        let resp = complete(&sample_index(), "", pos(0, 0));
        let CompletionResponse::Array(items) = resp else {
            panic!("expected array");
        };
        for item in items {
            assert_eq!(item.insert_text_format, Some(InsertTextFormat::SNIPPET));
            assert!(
                item.insert_text.as_deref().unwrap_or("").contains("$0"),
                "insert_text should contain a snippet tab-stop"
            );
        }
    }

    #[test]
    fn overload_count_visible_in_detail() {
        let resp = complete(&sample_index(), "", pos(0, 0));
        let CompletionResponse::Array(items) = resp else {
            panic!("expected array");
        };
        let target = items
            .iter()
            .find(|i| i.label == "target")
            .expect("target");
        assert!(
            target.detail.as_deref().unwrap_or("").contains("+1 more"),
            "detail should note the extra overload: {:?}",
            target.detail
        );
    }

    #[test]
    fn suppresses_completion_after_line_comment() {
        let resp = complete(&sample_index(), "// thoughts here", pos(0, 16));
        match resp {
            CompletionResponse::Array(items) => assert!(items.is_empty()),
            _ => panic!("expected array"),
        }
    }

    #[test]
    fn offers_completion_before_line_comment_start() {
        let resp = complete(&sample_index(), "target // trailing", pos(0, 6));
        match resp {
            CompletionResponse::Array(items) => assert!(!items.is_empty()),
            _ => panic!("expected array"),
        }
    }
}
