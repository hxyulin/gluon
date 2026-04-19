//! Word-at-cursor extraction from an in-memory buffer.
//!
//! The LSP doesn't yet parse `gluon.rhai` files into a real AST; for
//! MVP hover and completion we only need to know the identifier under
//! the cursor (or, for completion, the prefix being typed). This is a
//! deliberately minimal character-class scan — anything smarter should
//! grow into a proper Rhai parse step, not layers of regex.

use lsp_types::Position;

/// Identifier character for Rhai: ASCII alphanumeric or underscore.
/// Rhai accepts some additional Unicode in identifiers, but every
/// Gluon DSL function name in practice is ASCII, so the narrower
/// definition gives us a cleaner word boundary without cutting real
/// uses.
fn is_id(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Return the full identifier token containing or immediately
/// preceding the cursor position. Returns `None` if the cursor is
/// outside the buffer, on a blank line, or not adjacent to an
/// identifier character.
///
/// "Immediately preceding" matters because LSP cursor positions are
/// *between* characters — after typing `target`, the cursor is at
/// column 6, past the last `t`, so we must look one column to the
/// left.
pub fn word_at(doc: &str, pos: Position) -> Option<String> {
    let line = doc.lines().nth(pos.line as usize)?;
    let chars: Vec<char> = line.chars().collect();
    let col = pos.character as usize;
    // Clamp: cursor past end-of-line means "at end of line" in LSP.
    let col = col.min(chars.len());

    // Walk left from cursor while we're inside an identifier.
    let mut start = col;
    while start > 0 && is_id(chars[start - 1]) {
        start -= 1;
    }
    // Walk right from cursor through identifier chars.
    let mut end = col;
    while end < chars.len() && is_id(chars[end]) {
        end += 1;
    }
    if start == end {
        return None;
    }
    Some(chars[start..end].iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(line: u32, col: u32) -> Position {
        Position {
            line,
            character: col,
        }
    }

    #[test]
    fn word_at_picks_up_identifier_end() {
        // Cursor after the last `t` in "target"
        assert_eq!(
            word_at("target(\"foo\")", pos(0, 6)).as_deref(),
            Some("target")
        );
    }

    #[test]
    fn word_at_picks_up_identifier_middle() {
        assert_eq!(word_at("group(\"k\")", pos(0, 3)).as_deref(), Some("group"));
    }

    #[test]
    fn word_at_picks_up_identifier_start() {
        assert_eq!(word_at("foo", pos(0, 0)).as_deref(), Some("foo"));
    }

    #[test]
    fn word_at_returns_none_in_whitespace() {
        assert_eq!(word_at("  target", pos(0, 0)), None);
        assert_eq!(word_at("target  other", pos(0, 7)), None);
    }

    #[test]
    fn word_at_handles_out_of_range() {
        assert_eq!(word_at("short", pos(0, 999)).as_deref(), Some("short"));
        assert_eq!(word_at("short", pos(5, 0)), None);
    }

    #[test]
    fn word_at_respects_underscores() {
        assert_eq!(
            word_at("config_option(\"X\")", pos(0, 5)).as_deref(),
            Some("config_option")
        );
    }
}
