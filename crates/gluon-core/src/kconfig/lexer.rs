//! Hand-rolled tokenizer for `.kconfig` files.
//!
//! Produces a `Vec<Token>` from UTF-8 source. Every token carries a
//! [`SourceSpan`] so downstream parser errors can point at the exact
//! offending byte range. The token stream is terminated with an explicit
//! [`TokenKind::Eof`] sentinel so the parser can uniformly match on
//! "next token" without special-casing end-of-input.
//!
//! # Token philosophy
//!
//! The token set is intentionally small. Only structural punctuation
//! (`{`, `}`, `:`, `=`, `..`, `..=`, etc.), operators (`&&`, `||`, `!`),
//! literals (strings, integers, `true`/`false`), and bare identifiers
//! are distinguished. Grammar keywords like `config`, `menu`, `bool`,
//! `default`, `depends_on`, etc. are lexed as [`TokenKind::Ident`] and
//! interpreted by the parser based on position. This keeps the lexer
//! agnostic to grammar evolution — adding a new config property is a
//! parser-only change.
//!
//! # Improvements over hadron
//!
//! - Newlines are **not** significant (the brace grammar makes them
//!   ordinary whitespace). Hadron's kconfig parser relies on significant
//!   newlines to terminate property lines, which leaks into the parser
//!   as an ambiguity source.
//! - Errors are structured [`Diagnostic`]s with [`SourceSpan`]s, not
//!   stringly-typed `"path:line:col: msg"` formats.

use crate::error::Diagnostic;
use gluon_model::SourceSpan;
use std::path::{Path, PathBuf};

/// A single lexed token with its source location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: SourceSpan,
}

/// Flavor of a lexed token.
///
/// Bare identifiers cover all grammar keywords (`config`, `menu`, `bool`,
/// `default`, `depends_on`, ...). The parser interprets them based on
/// position. Only `true` and `false` are hard keywords, because they are
/// value literals rather than names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    /// A bare identifier. Covers config names (`DEBUG_LOG`), type tags
    /// (`bool`, `u32`), property names (`default`, `depends_on`), and
    /// everything else lexically shaped like `[A-Za-z_][A-Za-z0-9_]*`.
    Ident(String),
    /// A double-quoted string literal. Escape sequences `\"`, `\\`,
    /// `\n`, `\r`, `\t` are decoded into the stored value; any other
    /// `\x` sequence is a lex error.
    String(String),
    /// An unsigned integer literal. Supports decimal and `0x`-prefixed
    /// hexadecimal. Values that overflow `u64` are a lex error.
    Integer(u64),
    /// The literal `true`.
    True,
    /// The literal `false`.
    False,

    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Colon,
    Comma,
    Eq,
    /// `..` — exclusive range operator (used in `range = a..b`).
    DotDot,
    /// `..=` — inclusive range operator.
    DotDotEq,

    /// `&&` — logical AND in `depends_on` / `visible_if` expressions.
    AndAnd,
    /// `||` — logical OR.
    OrOr,
    /// `!` — logical NOT.
    Bang,

    /// End-of-input sentinel. Always the last token in the output stream.
    Eof,
}

/// Tokenize a `.kconfig` source string.
///
/// `file` is the path associated with spans — typically the canonical
/// path to the file being parsed. The lexer itself does not open any
/// files; file I/O happens in the loader layer.
///
/// On success, returns a `Vec<Token>` that always ends with a single
/// [`TokenKind::Eof`] token. On failure, returns one or more
/// [`Diagnostic`]s describing every lex error encountered; the lexer is
/// best-effort and continues past recoverable errors (e.g. unknown
/// characters) so a single run can report as many problems as possible.
pub fn lex(source: &str, file: &Path) -> Result<Vec<Token>, Vec<Diagnostic>> {
    let mut lx = Lexer::new(source, file.to_path_buf());
    lx.run();
    if lx.diagnostics.is_empty() {
        Ok(lx.tokens)
    } else {
        Err(lx.diagnostics)
    }
}

struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    file: PathBuf,
    pos: usize,
    /// 1-based line number of `pos`.
    line: u32,
    /// 1-based column number of `pos`.
    col: u32,
    tokens: Vec<Token>,
    diagnostics: Vec<Diagnostic>,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str, file: PathBuf) -> Self {
        Self {
            src,
            bytes: src.as_bytes(),
            file,
            pos: 0,
            line: 1,
            col: 1,
            tokens: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    fn run(&mut self) {
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            match b {
                // Whitespace.
                b' ' | b'\t' | b'\r' => self.advance(),
                b'\n' => {
                    self.pos += 1;
                    self.line += 1;
                    self.col = 1;
                }
                // Line comment: `#` to end of line.
                b'#' => {
                    while self.pos < self.bytes.len() && self.bytes[self.pos] != b'\n' {
                        self.pos += 1;
                        self.col += 1;
                    }
                }
                // Single-char punctuation.
                b'{' => self.push_simple(TokenKind::LBrace),
                b'}' => self.push_simple(TokenKind::RBrace),
                b'(' => self.push_simple(TokenKind::LParen),
                b')' => self.push_simple(TokenKind::RParen),
                b'[' => self.push_simple(TokenKind::LBracket),
                b']' => self.push_simple(TokenKind::RBracket),
                b':' => self.push_simple(TokenKind::Colon),
                b',' => self.push_simple(TokenKind::Comma),
                b'=' => self.push_simple(TokenKind::Eq),
                // Multi-char operators.
                b'.' => self.lex_dot(),
                b'&' => self.lex_and(),
                b'|' => self.lex_or(),
                b'!' => self.push_simple(TokenKind::Bang),
                // Strings.
                b'"' => self.lex_string(),
                // Numbers.
                b'0'..=b'9' => self.lex_number(),
                // Identifiers and keywords.
                b'a'..=b'z' | b'A'..=b'Z' | b'_' => self.lex_ident(),
                // Anything else is a lex error; skip the byte so we keep
                // making progress and can report multiple errors per run.
                _ => {
                    let start = (self.line, self.col);
                    let ch = self.src[self.pos..]
                        .chars()
                        .next()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| format!("0x{b:02x}"));
                    // Advance by the full UTF-8 char width, not just 1
                    // byte, so non-ASCII garbage doesn't desync spans.
                    let width = self.src[self.pos..]
                        .chars()
                        .next()
                        .map(|c| c.len_utf8())
                        .unwrap_or(1);
                    self.pos += width;
                    self.col += 1;
                    self.diagnostics.push(
                        Diagnostic::error(format!("unexpected character '{ch}' in .kconfig"))
                            .with_span(SourceSpan::range(
                                self.file.clone(),
                                start,
                                (self.line, self.col),
                            )),
                    );
                }
            }
        }

        // Emit a final EOF sentinel. Its span is a zero-width point at
        // the current cursor so the parser can attach "unexpected end of
        // input" errors to a sensible location.
        let eof_span = SourceSpan::point(self.file.clone(), self.line, self.col);
        self.tokens.push(Token {
            kind: TokenKind::Eof,
            span: eof_span,
        });
    }

    /// Advance one byte, tracking column. Caller must only use this for
    /// bytes that are definitely ASCII and definitely not `\n`.
    fn advance(&mut self) {
        self.pos += 1;
        self.col += 1;
    }

    fn push_simple(&mut self, kind: TokenKind) {
        let start = (self.line, self.col);
        self.pos += 1;
        self.col += 1;
        let end = (self.line, self.col);
        self.tokens.push(Token {
            kind,
            span: SourceSpan::range(self.file.clone(), start, end),
        });
    }

    fn lex_dot(&mut self) {
        let start = (self.line, self.col);
        // Already positioned on `.`. Look ahead: `..`, `..=`, or lone
        // `.` (which is a lex error — bare `.` has no meaning in our
        // grammar).
        if self.bytes.get(self.pos + 1) == Some(&b'.') {
            if self.bytes.get(self.pos + 2) == Some(&b'=') {
                self.pos += 3;
                self.col += 3;
                let end = (self.line, self.col);
                self.tokens.push(Token {
                    kind: TokenKind::DotDotEq,
                    span: SourceSpan::range(self.file.clone(), start, end),
                });
            } else {
                self.pos += 2;
                self.col += 2;
                let end = (self.line, self.col);
                self.tokens.push(Token {
                    kind: TokenKind::DotDot,
                    span: SourceSpan::range(self.file.clone(), start, end),
                });
            }
        } else {
            self.pos += 1;
            self.col += 1;
            self.diagnostics.push(
                Diagnostic::error("unexpected '.' (did you mean '..' or '..=')").with_span(
                    SourceSpan::range(self.file.clone(), start, (self.line, self.col)),
                ),
            );
        }
    }

    fn lex_and(&mut self) {
        let start = (self.line, self.col);
        if self.bytes.get(self.pos + 1) == Some(&b'&') {
            self.pos += 2;
            self.col += 2;
            let end = (self.line, self.col);
            self.tokens.push(Token {
                kind: TokenKind::AndAnd,
                span: SourceSpan::range(self.file.clone(), start, end),
            });
        } else {
            self.pos += 1;
            self.col += 1;
            self.diagnostics.push(
                Diagnostic::error("expected '&&', found single '&'").with_span(SourceSpan::range(
                    self.file.clone(),
                    start,
                    (self.line, self.col),
                )),
            );
        }
    }

    fn lex_or(&mut self) {
        let start = (self.line, self.col);
        if self.bytes.get(self.pos + 1) == Some(&b'|') {
            self.pos += 2;
            self.col += 2;
            let end = (self.line, self.col);
            self.tokens.push(Token {
                kind: TokenKind::OrOr,
                span: SourceSpan::range(self.file.clone(), start, end),
            });
        } else {
            self.pos += 1;
            self.col += 1;
            self.diagnostics.push(
                Diagnostic::error("expected '||', found single '|'").with_span(SourceSpan::range(
                    self.file.clone(),
                    start,
                    (self.line, self.col),
                )),
            );
        }
    }

    fn lex_string(&mut self) {
        let start = (self.line, self.col);
        // Skip opening quote.
        self.pos += 1;
        self.col += 1;

        let mut value = String::new();
        loop {
            if self.pos >= self.bytes.len() {
                self.diagnostics
                    .push(Diagnostic::error("unterminated string literal").with_span(
                        SourceSpan::range(self.file.clone(), start, (self.line, self.col)),
                    ));
                return;
            }
            let b = self.bytes[self.pos];
            match b {
                b'"' => {
                    self.pos += 1;
                    self.col += 1;
                    let end = (self.line, self.col);
                    self.tokens.push(Token {
                        kind: TokenKind::String(value),
                        span: SourceSpan::range(self.file.clone(), start, end),
                    });
                    return;
                }
                b'\\' => {
                    self.pos += 1;
                    self.col += 1;
                    if self.pos >= self.bytes.len() {
                        self.diagnostics.push(
                            Diagnostic::error("string ended in the middle of an escape sequence")
                                .with_span(SourceSpan::range(
                                    self.file.clone(),
                                    start,
                                    (self.line, self.col),
                                )),
                        );
                        return;
                    }
                    let esc = self.bytes[self.pos];
                    self.pos += 1;
                    self.col += 1;
                    match esc {
                        b'"' => value.push('"'),
                        b'\\' => value.push('\\'),
                        b'n' => value.push('\n'),
                        b'r' => value.push('\r'),
                        b't' => value.push('\t'),
                        b'0' => value.push('\0'),
                        _ => {
                            let bad_char = esc as char;
                            self.diagnostics.push(
                                Diagnostic::error(format!(
                                    "unknown string escape sequence '\\{bad_char}'"
                                ))
                                .with_span(SourceSpan::range(
                                    self.file.clone(),
                                    start,
                                    (self.line, self.col),
                                ))
                                .with_note("supported escapes: \\\", \\\\, \\n, \\r, \\t, \\0"),
                            );
                        }
                    }
                }
                b'\n' => {
                    // Raw newline inside a string literal is a hard
                    // error. Block/brace grammar treats newlines as
                    // whitespace elsewhere but strings must be closed
                    // on the same line (simpler diagnostics, matches
                    // Rust's own string rules without raw prefix).
                    self.diagnostics.push(
                        Diagnostic::error("unterminated string literal (newline in string)")
                            .with_span(SourceSpan::range(
                                self.file.clone(),
                                start,
                                (self.line, self.col),
                            )),
                    );
                    return;
                }
                _ => {
                    // Consume one UTF-8 char.
                    let ch_len = self.src[self.pos..]
                        .chars()
                        .next()
                        .map(|c| {
                            value.push(c);
                            c.len_utf8()
                        })
                        .unwrap_or(1);
                    self.pos += ch_len;
                    self.col += 1;
                }
            }
        }
    }

    fn lex_number(&mut self) {
        let start = (self.line, self.col);
        let begin = self.pos;

        // `0x` hex prefix is only valid when the next byte is an ASCII
        // hex digit; a bare `0` followed by e.g. `.` is still a decimal
        // integer `0`.
        let is_hex = self.bytes.get(self.pos) == Some(&b'0')
            && matches!(self.bytes.get(self.pos + 1), Some(&b'x') | Some(&b'X'))
            && self
                .bytes
                .get(self.pos + 2)
                .map(|b| b.is_ascii_hexdigit())
                .unwrap_or(false);

        let (radix, digit_start) = if is_hex {
            self.pos += 2;
            self.col += 2;
            (16u32, self.pos)
        } else {
            (10u32, self.pos)
        };

        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            let ok = match radix {
                10 => b.is_ascii_digit() || b == b'_',
                16 => b.is_ascii_hexdigit() || b == b'_',
                _ => unreachable!(),
            };
            if !ok {
                break;
            }
            self.pos += 1;
            self.col += 1;
        }

        if digit_start == self.pos {
            // `0x` with no digits after (shouldn't happen given the
            // lookahead above, but handle defensively).
            self.diagnostics.push(
                Diagnostic::error("integer literal has no digits").with_span(SourceSpan::range(
                    self.file.clone(),
                    start,
                    (self.line, self.col),
                )),
            );
            return;
        }

        let raw = &self.src[digit_start..self.pos];
        let cleaned: String = raw.chars().filter(|c| *c != '_').collect();
        let end = (self.line, self.col);
        match u64::from_str_radix(&cleaned, radix) {
            Ok(v) => {
                self.tokens.push(Token {
                    kind: TokenKind::Integer(v),
                    span: SourceSpan::range(self.file.clone(), start, end),
                });
            }
            Err(_) => {
                // The most common cause here is overflow — `from_str_radix`
                // on clean ASCII digit chars only fails on overflow or
                // empty input, and we already rejected the empty case.
                let full = &self.src[begin..self.pos];
                self.diagnostics.push(
                    Diagnostic::error(format!("integer literal '{full}' is out of range for u64"))
                        .with_span(SourceSpan::range(self.file.clone(), start, end)),
                );
            }
        }
    }

    fn lex_ident(&mut self) {
        let start = (self.line, self.col);
        let begin = self.pos;
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b.is_ascii_alphanumeric() || b == b'_' {
                self.pos += 1;
                self.col += 1;
            } else {
                break;
            }
        }
        let text = &self.src[begin..self.pos];
        let end = (self.line, self.col);
        let kind = match text {
            "true" => TokenKind::True,
            "false" => TokenKind::False,
            _ => TokenKind::Ident(text.to_string()),
        };
        self.tokens.push(Token {
            kind,
            span: SourceSpan::range(self.file.clone(), start, end),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Convenience: lex the input at a dummy path, unwrap, and drop the
    /// trailing EOF so tests can focus on real tokens.
    fn lex_ok(src: &str) -> Vec<Token> {
        let mut toks = lex(src, Path::new("test.kconfig")).expect("lex ok");
        assert!(matches!(toks.last().map(|t| &t.kind), Some(TokenKind::Eof)));
        toks.pop();
        toks
    }

    fn kinds(toks: &[Token]) -> Vec<&TokenKind> {
        toks.iter().map(|t| &t.kind).collect()
    }

    #[test]
    fn empty_input_yields_only_eof() {
        let toks = lex("", Path::new("t.kconfig")).expect("empty ok");
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].kind, TokenKind::Eof);
    }

    #[test]
    fn whitespace_and_newlines_are_skipped() {
        let toks = lex_ok("  \n\t\r\n   ");
        assert!(toks.is_empty());
    }

    #[test]
    fn line_comment_runs_to_newline() {
        let toks = lex_ok("# this is a comment\nfoo");
        assert_eq!(kinds(&toks), vec![&TokenKind::Ident("foo".into())]);
    }

    #[test]
    fn simple_identifier() {
        let toks = lex_ok("DEBUG_LOG");
        assert_eq!(kinds(&toks), vec![&TokenKind::Ident("DEBUG_LOG".into())]);
    }

    #[test]
    fn true_false_are_hard_keywords() {
        let toks = lex_ok("true false");
        assert_eq!(kinds(&toks), vec![&TokenKind::True, &TokenKind::False]);
    }

    #[test]
    fn grammar_keywords_lex_as_idents() {
        // "config" / "bool" / "default" are grammar keywords parsed by
        // the parser, but at the lexer level they are just identifiers.
        // This is a deliberate design decision — it keeps the lexer
        // immune to grammar evolution.
        let toks = lex_ok("config bool default depends_on");
        assert_eq!(
            kinds(&toks),
            vec![
                &TokenKind::Ident("config".into()),
                &TokenKind::Ident("bool".into()),
                &TokenKind::Ident("default".into()),
                &TokenKind::Ident("depends_on".into()),
            ]
        );
    }

    #[test]
    fn punctuation_and_operators() {
        let toks = lex_ok("{}()[]:,=..=..&&||!");
        assert_eq!(
            kinds(&toks),
            vec![
                &TokenKind::LBrace,
                &TokenKind::RBrace,
                &TokenKind::LParen,
                &TokenKind::RParen,
                &TokenKind::LBracket,
                &TokenKind::RBracket,
                &TokenKind::Colon,
                &TokenKind::Comma,
                &TokenKind::Eq,
                &TokenKind::DotDotEq,
                &TokenKind::DotDot,
                &TokenKind::AndAnd,
                &TokenKind::OrOr,
                &TokenKind::Bang,
            ]
        );
    }

    #[test]
    fn string_with_escapes() {
        let toks = lex_ok(r#" "hello\nworld\t\"quoted\"\\" "#);
        assert_eq!(
            kinds(&toks),
            vec![&TokenKind::String("hello\nworld\t\"quoted\"\\".into())]
        );
    }

    #[test]
    fn unterminated_string_is_error() {
        let errs = lex("\"abc", Path::new("t.kconfig")).unwrap_err();
        assert!(errs.iter().any(|d| d.message.contains("unterminated")));
    }

    #[test]
    fn unknown_string_escape_is_error() {
        let errs = lex(r#" "foo\q" "#, Path::new("t.kconfig")).unwrap_err();
        assert!(errs.iter().any(|d| d.message.contains("unknown")));
    }

    #[test]
    fn decimal_and_hex_integers() {
        let toks = lex_ok("0 42 1_000 0xff 0xDEAD_BEEF");
        assert_eq!(
            kinds(&toks),
            vec![
                &TokenKind::Integer(0),
                &TokenKind::Integer(42),
                &TokenKind::Integer(1_000),
                &TokenKind::Integer(0xff),
                &TokenKind::Integer(0xdead_beef),
            ]
        );
    }

    #[test]
    fn integer_overflow_is_error() {
        // 2^64 is one past u64::MAX.
        let errs = lex("18446744073709551616", Path::new("t.kconfig")).unwrap_err();
        assert!(errs.iter().any(|d| d.message.contains("out of range")));
    }

    #[test]
    fn dotdot_eq_is_greedy() {
        // `0..=5` should lex as `0`, `..=`, `5` — not `0`, `..`, `=`, `5`.
        let toks = lex_ok("0..=5");
        assert_eq!(
            kinds(&toks),
            vec![
                &TokenKind::Integer(0),
                &TokenKind::DotDotEq,
                &TokenKind::Integer(5),
            ]
        );
    }

    #[test]
    fn spans_track_line_and_column() {
        let toks = lex_ok("foo\n  bar");
        // `foo` starts at line 1 col 1.
        assert_eq!(toks[0].span.line, 1);
        assert_eq!(toks[0].span.col, 1);
        // `bar` starts at line 2 col 3 (after two spaces).
        assert_eq!(toks[1].span.line, 2);
        assert_eq!(toks[1].span.col, 3);
    }

    #[test]
    fn bang_does_not_swallow_eq() {
        // `!` is always a standalone token; there's no `!=` in the grammar.
        let toks = lex_ok("!A");
        assert_eq!(
            kinds(&toks),
            vec![&TokenKind::Bang, &TokenKind::Ident("A".into())]
        );
    }

    #[test]
    fn single_ampersand_is_error() {
        let errs = lex("A & B", Path::new("t.kconfig")).unwrap_err();
        assert!(errs.iter().any(|d| d.message.contains("&&")));
    }

    #[test]
    fn single_pipe_is_error() {
        let errs = lex("A | B", Path::new("t.kconfig")).unwrap_err();
        assert!(errs.iter().any(|d| d.message.contains("||")));
    }

    #[test]
    fn lone_dot_is_error() {
        let errs = lex(".", Path::new("t.kconfig")).unwrap_err();
        assert!(errs.iter().any(|d| d.message.contains("'..'")));
    }

    #[test]
    fn unknown_character_is_error_and_recovers() {
        // `@` is unknown; lexer should report an error but still return
        // the surrounding tokens in the `Err` path's ignored success.
        let result = lex("foo @ bar", Path::new("t.kconfig"));
        assert!(result.is_err());
        // The lexer's "continue past errors" property is enforced by
        // the error count — there is exactly one unknown character.
        if let Err(errs) = result {
            assert_eq!(errs.len(), 1);
            assert!(errs[0].message.contains("unexpected character"));
        }
    }

    #[test]
    fn realistic_config_block_lexes_cleanly() {
        let src = r#"
            config DEBUG_LOG: bool {
                default = true
                help = "Enable debug logging"
                depends_on = LOG_ENABLED && !QUIET
                range = 0..=5
            }
        "#;
        let toks = lex_ok(src);
        // Sanity-check the start and end of the stream rather than
        // enumerating every token (that would duplicate the parser test
        // in K3). We expect: `config`, `DEBUG_LOG`, `:`, `bool`, `{`, ...
        assert!(matches!(&toks[0].kind, TokenKind::Ident(s) if s == "config"));
        assert!(matches!(&toks[1].kind, TokenKind::Ident(s) if s == "DEBUG_LOG"));
        assert_eq!(toks[2].kind, TokenKind::Colon);
        assert!(matches!(&toks[3].kind, TokenKind::Ident(s) if s == "bool"));
        assert_eq!(toks[4].kind, TokenKind::LBrace);
        assert!(toks.iter().any(|t| t.kind == TokenKind::AndAnd));
        assert!(toks.iter().any(|t| t.kind == TokenKind::Bang));
        assert!(toks.iter().any(|t| t.kind == TokenKind::DotDotEq));
        assert!(toks.iter().any(|t| t.kind == TokenKind::True));
        assert_eq!(toks.last().unwrap().kind, TokenKind::RBrace);
    }
}
