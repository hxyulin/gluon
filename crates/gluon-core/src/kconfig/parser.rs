//! Recursive-descent parser over the token stream produced by
//! [`crate::kconfig::lexer::lex`].
//!
//! # Error philosophy
//!
//! The parser is **fail-slow** within a single item and **fail-fast** at
//! the item boundary. When a property inside a `config { ... }` block
//! fails to parse, the parser records a diagnostic and resyncs by
//! scanning forward to either the next property-start (a top-level
//! identifier inside the brace) or the closing `}`. When a top-level
//! item fails to parse, the parser skips to the next `config` / `menu`
//! / `preset` / `source` keyword and continues. This maximizes the
//! number of diagnostics reported in a single run.
//!
//! # Grammar coverage (chunks K3–K5)
//!
//! - `config NAME: type { props... }` with `default`, `help`, `range`,
//!   `choices`, `menu`, `binding`, `depends_on`, `visible_if`, `selects`.
//! - Expressions in `depends_on` / `visible_if` with full `&&`, `||`,
//!   `!`, and parenthesized grouping (precedence climbing).
//! - `menu "Title" { items... }` with arbitrary nesting.
//! - `preset "name" { inherits, help, per-option overrides }`.
//! - `source "./path"` top-level file include.

use super::ast::{
    BindingTag, ConfigBlock, ConfigProp, File, Item, Literal, MenuBlock, PresetBlock, PresetProp,
    SourceDecl, TypeTag,
};
use super::lexer::{Token, TokenKind};
use crate::error::Diagnostic;
use gluon_model::{Expr, SourceSpan};

/// Parse a token stream into an [`ast::File`](super::ast::File).
///
/// The returned `File` contains every item that parsed cleanly. If any
/// item failed, the `Err` variant is populated with every diagnostic
/// collected during the run; the partially-parsed `File` is not returned
/// on error — callers needing partial results should extend this API in
/// a later chunk.
pub fn parse(tokens: &[Token]) -> Result<File, Vec<Diagnostic>> {
    let mut p = Parser::new(tokens);
    let file = p.parse_file();
    if p.diagnostics.is_empty() {
        Ok(file)
    } else {
        Err(p.diagnostics)
    }
}

/// Parse a standalone boolean expression (the same grammar accepted by
/// `depends_on`/`visible_if` property values) from a pre-lexed token
/// stream. Used by [`crate::kconfig::parse_bool_expr`] so the Rhai
/// builder can reuse the kconfig expression grammar without duplicating
/// it.
///
/// The token slice must be terminated with [`TokenKind::Eof`] (the lexer
/// always emits one). Trailing tokens beyond a single expression are a
/// parse error.
pub(crate) fn parse_standalone_expr(tokens: &[Token]) -> Result<Expr, Vec<Diagnostic>> {
    let mut p = Parser::new(tokens);
    let Some((expr, _)) = p.parse_expr() else {
        return Err(p.diagnostics);
    };
    if !p.at_eof() {
        p.error_here("unexpected trailing tokens after expression");
    }
    if p.diagnostics.is_empty() {
        Ok(expr)
    } else {
        Err(p.diagnostics)
    }
}

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
    diagnostics: Vec<Diagnostic>,
}

impl<'a> Parser<'a> {
    fn new(tokens: &'a [Token]) -> Self {
        Self {
            tokens,
            pos: 0,
            diagnostics: Vec::new(),
        }
    }

    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    fn peek_kind(&self) -> &TokenKind {
        &self.tokens[self.pos].kind
    }

    fn advance(&mut self) -> &Token {
        let t = &self.tokens[self.pos];
        if !matches!(t.kind, TokenKind::Eof) {
            self.pos += 1;
        }
        t
    }

    fn at_eof(&self) -> bool {
        matches!(self.peek_kind(), TokenKind::Eof)
    }

    /// Push a diagnostic against the current token's span.
    fn error_here(&mut self, msg: impl Into<String>) {
        let span = self.peek().span.clone();
        self.diagnostics
            .push(Diagnostic::error(msg).with_span(span));
    }

    fn error_at(&mut self, msg: impl Into<String>, span: SourceSpan) {
        self.diagnostics
            .push(Diagnostic::error(msg).with_span(span));
    }

    /// Expect a specific token kind; advance and return its span on
    /// success. On mismatch, record a diagnostic and return `None`
    /// without consuming — callers resync.
    fn expect(&mut self, expected: &TokenKind, description: &str) -> Option<SourceSpan> {
        if std::mem::discriminant(self.peek_kind()) == std::mem::discriminant(expected) {
            Some(self.advance().span.clone())
        } else {
            let found = describe_token(self.peek_kind());
            self.error_here(format!("expected {description}, found {found}"));
            None
        }
    }

    fn parse_file(&mut self) -> File {
        let mut items = Vec::new();
        while !self.at_eof() {
            match self.parse_item() {
                Some(item) => items.push(item),
                None => self.resync_to_top_level(),
            }
        }
        File { items }
    }

    /// Advance until we see a plausible top-level keyword start or EOF.
    /// Used as error recovery between items.
    fn resync_to_top_level(&mut self) {
        while !self.at_eof() {
            if let TokenKind::Ident(name) = self.peek_kind()
                && matches!(name.as_str(), "config" | "menu" | "preset" | "source")
            {
                return;
            }
            self.advance();
        }
    }

    fn parse_item(&mut self) -> Option<Item> {
        let TokenKind::Ident(keyword) = self.peek_kind() else {
            let found = describe_token(self.peek_kind());
            self.error_here(format!("expected top-level item, found {found}"));
            return None;
        };
        match keyword.as_str() {
            "config" => self.parse_config_block().map(Item::Config),
            "menu" => self.parse_menu_block().map(Item::Menu),
            "preset" => self.parse_preset_block().map(Item::Preset),
            "source" => self.parse_source_decl().map(Item::Source),
            other => {
                let span = self.peek().span.clone();
                self.error_at(
                    format!(
                        "unexpected top-level keyword '{other}' (expected 'config', 'menu', 'preset', or 'source')"
                    ),
                    span,
                );
                None
            }
        }
    }

    fn parse_menu_block(&mut self) -> Option<MenuBlock> {
        let keyword_span = self.advance().span.clone(); // consume `menu`

        // Menu title is a required string literal.
        let (title, title_span) = match self.peek_kind() {
            TokenKind::String(s) => {
                let t = s.clone();
                let sp = self.advance().span.clone();
                (t, sp)
            }
            _ => {
                self.error_here("expected string literal for menu title");
                return None;
            }
        };

        self.expect(&TokenKind::LBrace, "'{' to begin menu block")?;

        let mut items = Vec::new();
        loop {
            if matches!(self.peek_kind(), TokenKind::RBrace | TokenKind::Eof) {
                break;
            }
            match self.parse_item() {
                Some(item) => items.push(item),
                // On error inside a menu, skip forward until we see
                // either a plausible next item start or the closing `}`.
                None => self.resync_inside_menu(),
            }
        }
        let close_span = self.expect(&TokenKind::RBrace, "'}' to end menu block")?;

        Some(MenuBlock {
            title,
            title_span,
            items,
            span: merge_spans(&keyword_span, &close_span),
        })
    }

    fn resync_inside_menu(&mut self) {
        while !self.at_eof() {
            match self.peek_kind() {
                TokenKind::RBrace => return,
                TokenKind::Ident(n)
                    if matches!(n.as_str(), "config" | "menu" | "preset" | "source") =>
                {
                    return;
                }
                _ => {
                    self.advance();
                }
            }
        }
    }

    fn parse_preset_block(&mut self) -> Option<PresetBlock> {
        let keyword_span = self.advance().span.clone(); // consume `preset`

        let (name, name_span) = match self.peek_kind() {
            TokenKind::String(s) => {
                let n = s.clone();
                let sp = self.advance().span.clone();
                (n, sp)
            }
            _ => {
                self.error_here("expected string literal for preset name");
                return None;
            }
        };

        self.expect(&TokenKind::LBrace, "'{' to begin preset block")?;

        let mut props = Vec::new();
        loop {
            if matches!(self.peek_kind(), TokenKind::RBrace | TokenKind::Eof) {
                break;
            }
            match self.parse_preset_prop() {
                Some(p) => props.push(p),
                None => self.resync_inside_config_block(),
            }
        }
        let close_span = self.expect(&TokenKind::RBrace, "'}' to end preset block")?;

        Some(PresetBlock {
            name,
            name_span,
            props,
            span: merge_spans(&keyword_span, &close_span),
        })
    }

    fn parse_preset_prop(&mut self) -> Option<PresetProp> {
        let TokenKind::Ident(name) = self.peek_kind() else {
            self.error_here("expected property name or option name inside preset block");
            return None;
        };
        let name = name.clone();
        let start = self.peek().span.clone();
        self.advance();

        self.expect(&TokenKind::Eq, "'=' after preset property name")?;

        match name.as_str() {
            "inherits" => {
                // `inherits = "parent"`
                let TokenKind::String(parent) = self.peek_kind() else {
                    self.error_here("expected string literal for 'inherits'");
                    return None;
                };
                let parent = parent.clone();
                let end = self.advance().span.clone();
                Some(PresetProp::Inherits {
                    parent,
                    span: merge_spans(&start, &end),
                })
            }
            "help" => {
                let TokenKind::String(text) = self.peek_kind() else {
                    self.error_here("expected string literal for 'help'");
                    return None;
                };
                let text = text.clone();
                let end = self.advance().span.clone();
                Some(PresetProp::Help {
                    text,
                    span: merge_spans(&start, &end),
                })
            }
            // Anything else is an option override. The identifier is
            // the option name and the RHS is a literal value. Type
            // checking against the referenced option happens during
            // lowering (K6), not here — the parser does not have access
            // to the set of declared options.
            _ => {
                let (value, end) = self.parse_literal()?;
                Some(PresetProp::Override {
                    option: name,
                    value,
                    span: merge_spans(&start, &end),
                })
            }
        }
    }

    fn parse_source_decl(&mut self) -> Option<SourceDecl> {
        let keyword_span = self.advance().span.clone(); // consume `source`
        match self.peek_kind() {
            TokenKind::String(s) => {
                let path = s.clone();
                let end = self.advance().span.clone();
                Some(SourceDecl {
                    path,
                    span: merge_spans(&keyword_span, &end),
                })
            }
            _ => {
                self.error_here("expected string literal for source path");
                None
            }
        }
    }

    fn parse_config_block(&mut self) -> Option<ConfigBlock> {
        let keyword_span = self.advance().span.clone(); // consume `config`

        // NAME
        let (name, name_span) = match self.peek_kind() {
            TokenKind::Ident(n) => {
                let n = n.clone();
                let s = self.advance().span.clone();
                (n, s)
            }
            _ => {
                self.error_here("expected config option name after 'config'");
                return None;
            }
        };

        // `:`
        self.expect(&TokenKind::Colon, "':' after config name")?;

        // type tag
        let (ty, ty_span) = self.parse_type_tag()?;

        // `{`
        self.expect(&TokenKind::LBrace, "'{' to begin config block")?;

        // props... until `}`
        let mut props = Vec::new();
        loop {
            if matches!(self.peek_kind(), TokenKind::RBrace | TokenKind::Eof) {
                break;
            }
            match self.parse_config_prop() {
                Some(p) => props.push(p),
                None => self.resync_inside_config_block(),
            }
        }

        let close_span = self.expect(&TokenKind::RBrace, "'}' to end config block")?;

        let span = merge_spans(&keyword_span, &close_span);
        Some(ConfigBlock {
            name,
            name_span,
            ty,
            ty_span,
            props,
            span,
        })
    }

    /// Inside a config block, skip until we find either a `}` or the
    /// start of a plausible next property (a bare identifier).
    fn resync_inside_config_block(&mut self) {
        while !self.at_eof() {
            match self.peek_kind() {
                TokenKind::RBrace => return,
                TokenKind::Ident(_) => return,
                _ => {
                    self.advance();
                }
            }
        }
    }

    fn parse_type_tag(&mut self) -> Option<(TypeTag, SourceSpan)> {
        let TokenKind::Ident(name) = self.peek_kind() else {
            self.error_here("expected type tag (e.g. 'bool', 'u32', 'str')");
            return None;
        };
        let tag = match name.as_str() {
            "bool" => TypeTag::Bool,
            "tristate" => TypeTag::Tristate,
            "u32" => TypeTag::U32,
            "u64" => TypeTag::U64,
            "str" => TypeTag::Str,
            "choice" => TypeTag::Choice,
            "list" => TypeTag::List,
            "group" => TypeTag::Group,
            other => {
                let span = self.peek().span.clone();
                self.error_at(
                    format!("unknown type tag '{other}' (expected bool, tristate, u32, u64, str, choice, list, group)"),
                    span,
                );
                return None;
            }
        };
        let span = self.advance().span.clone();
        Some((tag, span))
    }

    fn parse_config_prop(&mut self) -> Option<ConfigProp> {
        let TokenKind::Ident(name) = self.peek_kind() else {
            self.error_here("expected property name inside config block");
            return None;
        };
        let name = name.clone();
        let prop_start = self.peek().span.clone();
        self.advance(); // consume property name

        // All K3 properties use `=` as the separator.
        self.expect(&TokenKind::Eq, "'=' after property name")?;

        match name.as_str() {
            "default" => self.parse_default_prop(prop_start),
            "help" => self.parse_help_prop(prop_start),
            "range" => self.parse_range_prop(prop_start),
            "choices" => self.parse_choices_prop(prop_start),
            "menu" => self.parse_menu_prop(prop_start),
            "binding" => self.parse_binding_prop(prop_start),
            "depends_on" => self.parse_depends_on_prop(prop_start),
            "visible_if" => self.parse_visible_if_prop(prop_start),
            "selects" => self.parse_selects_prop(prop_start),
            other => {
                let span = prop_start.clone();
                self.error_at(format!("unknown config property '{other}'"), span);
                None
            }
        }
    }

    fn parse_depends_on_prop(&mut self, start: SourceSpan) -> Option<ConfigProp> {
        let (expr, end) = self.parse_expr()?;
        Some(ConfigProp::DependsOn {
            expr,
            span: merge_spans(&start, &end),
        })
    }

    fn parse_visible_if_prop(&mut self, start: SourceSpan) -> Option<ConfigProp> {
        let (expr, end) = self.parse_expr()?;
        Some(ConfigProp::VisibleIf {
            expr,
            span: merge_spans(&start, &end),
        })
    }

    fn parse_selects_prop(&mut self, start: SourceSpan) -> Option<ConfigProp> {
        // `selects = [A, B, C]` — flat identifier list. No expression
        // form; selects semantics are inherently AND-of-union.
        self.expect(&TokenKind::LBracket, "'[' to begin selects list")?;
        let mut names = Vec::new();
        loop {
            match self.peek_kind() {
                TokenKind::RBracket => break,
                TokenKind::Ident(n) => {
                    names.push(n.clone());
                    self.advance();
                }
                _ => {
                    self.error_here("expected identifier in 'selects' list");
                    return None;
                }
            }
            match self.peek_kind() {
                TokenKind::Comma => {
                    self.advance();
                }
                TokenKind::RBracket => {}
                _ => {
                    self.error_here("expected ',' or ']' in 'selects' list");
                    return None;
                }
            }
        }
        let end = self.expect(&TokenKind::RBracket, "']' to end selects list")?;
        Some(ConfigProp::Selects {
            names,
            span: merge_spans(&start, &end),
        })
    }

    // --- Expression grammar (precedence climbing). ---
    //
    // expr    -> or_expr
    // or_expr -> and_expr ( '||' and_expr )*
    // and_expr-> unary    ( '&&' unary )*
    // unary   -> '!' unary | primary
    // primary -> IDENT | 'true' | 'false' | '(' expr ')'
    //
    // Returns the parsed expression plus the span of its last token,
    // so the caller can merge with its own starting span.

    fn parse_expr(&mut self) -> Option<(Expr, SourceSpan)> {
        self.parse_or_expr()
    }

    fn parse_or_expr(&mut self) -> Option<(Expr, SourceSpan)> {
        let (mut lhs, mut last_span) = self.parse_and_expr()?;
        while matches!(self.peek_kind(), TokenKind::OrOr) {
            self.advance();
            let (rhs, rhs_span) = self.parse_and_expr()?;
            last_span = rhs_span;
            lhs = match lhs {
                // Fold consecutive `||` into a single flat vector for
                // cheaper evaluation and a cleaner AST.
                Expr::Or(mut xs) => {
                    xs.push(rhs);
                    Expr::Or(xs)
                }
                other => Expr::Or(vec![other, rhs]),
            };
        }
        Some((lhs, last_span))
    }

    fn parse_and_expr(&mut self) -> Option<(Expr, SourceSpan)> {
        let (mut lhs, mut last_span) = self.parse_unary_expr()?;
        while matches!(self.peek_kind(), TokenKind::AndAnd) {
            self.advance();
            let (rhs, rhs_span) = self.parse_unary_expr()?;
            last_span = rhs_span;
            lhs = match lhs {
                Expr::And(mut xs) => {
                    xs.push(rhs);
                    Expr::And(xs)
                }
                other => Expr::And(vec![other, rhs]),
            };
        }
        Some((lhs, last_span))
    }

    fn parse_unary_expr(&mut self) -> Option<(Expr, SourceSpan)> {
        if matches!(self.peek_kind(), TokenKind::Bang) {
            self.advance();
            let (inner, span) = self.parse_unary_expr()?;
            Some((Expr::Not(Box::new(inner)), span))
        } else {
            self.parse_primary_expr()
        }
    }

    fn parse_primary_expr(&mut self) -> Option<(Expr, SourceSpan)> {
        match self.peek_kind() {
            TokenKind::Ident(name) => {
                let name = name.clone();
                let span = self.advance().span.clone();
                Some((Expr::Ident(name), span))
            }
            TokenKind::True => {
                let span = self.advance().span.clone();
                Some((Expr::Const(true), span))
            }
            TokenKind::False => {
                let span = self.advance().span.clone();
                Some((Expr::Const(false), span))
            }
            TokenKind::LParen => {
                self.advance();
                let (inner, _) = self.parse_expr()?;
                let end = self.expect(&TokenKind::RParen, "')' to close grouped expression")?;
                Some((inner, end))
            }
            _ => {
                self.error_here("expected expression (identifier, 'true', 'false', '!', or '(')");
                None
            }
        }
    }

    fn parse_default_prop(&mut self, start: SourceSpan) -> Option<ConfigProp> {
        let (value, value_span) = self.parse_literal()?;
        let span = merge_spans(&start, &value_span);
        Some(ConfigProp::Default { value, span })
    }

    fn parse_help_prop(&mut self, start: SourceSpan) -> Option<ConfigProp> {
        match self.peek_kind() {
            TokenKind::String(s) => {
                let text = s.clone();
                let end = self.advance().span.clone();
                Some(ConfigProp::Help {
                    text,
                    span: merge_spans(&start, &end),
                })
            }
            _ => {
                self.error_here("expected string literal for 'help'");
                None
            }
        }
    }

    fn parse_range_prop(&mut self, start: SourceSpan) -> Option<ConfigProp> {
        let low = match self.peek_kind() {
            TokenKind::Integer(v) => {
                let v = *v;
                self.advance();
                v
            }
            _ => {
                self.error_here("expected integer at start of 'range'");
                return None;
            }
        };
        let inclusive = match self.peek_kind() {
            TokenKind::DotDotEq => {
                self.advance();
                true
            }
            TokenKind::DotDot => {
                self.advance();
                false
            }
            _ => {
                self.error_here("expected '..' or '..=' in 'range'");
                return None;
            }
        };
        let (high, high_span) = match self.peek_kind() {
            TokenKind::Integer(v) => {
                let v = *v;
                let s = self.advance().span.clone();
                (v, s)
            }
            _ => {
                self.error_here("expected integer at end of 'range'");
                return None;
            }
        };
        if low > high {
            self.error_at(
                format!("range lower bound {low} is greater than upper bound {high}"),
                merge_spans(&start, &high_span),
            );
            return None;
        }
        Some(ConfigProp::Range {
            low,
            high,
            inclusive,
            span: merge_spans(&start, &high_span),
        })
    }

    fn parse_choices_prop(&mut self, start: SourceSpan) -> Option<ConfigProp> {
        self.expect(&TokenKind::LBracket, "'[' to begin choices list")?;
        let mut variants = Vec::new();
        // Allow trailing comma and empty list.
        loop {
            match self.peek_kind() {
                TokenKind::RBracket => break,
                TokenKind::String(s) => {
                    variants.push(s.clone());
                    self.advance();
                }
                _ => {
                    self.error_here("expected string literal in 'choices' list");
                    return None;
                }
            }
            match self.peek_kind() {
                TokenKind::Comma => {
                    self.advance();
                }
                TokenKind::RBracket => {}
                _ => {
                    self.error_here("expected ',' or ']' in 'choices' list");
                    return None;
                }
            }
        }
        let end = self
            .expect(&TokenKind::RBracket, "']' to end choices list")?
            .clone();
        Some(ConfigProp::Choices {
            variants,
            span: merge_spans(&start, &end),
        })
    }

    fn parse_menu_prop(&mut self, start: SourceSpan) -> Option<ConfigProp> {
        match self.peek_kind() {
            TokenKind::String(s) => {
                let label = s.clone();
                let end = self.advance().span.clone();
                Some(ConfigProp::MenuLabel {
                    label,
                    span: merge_spans(&start, &end),
                })
            }
            _ => {
                self.error_here("expected string literal for 'menu'");
                None
            }
        }
    }

    fn parse_binding_prop(&mut self, start: SourceSpan) -> Option<ConfigProp> {
        let TokenKind::Ident(name) = self.peek_kind() else {
            self.error_here("expected binding tag (cfg, cfg_cumulative, const, build)");
            return None;
        };
        let tag = match name.as_str() {
            "cfg" => BindingTag::Cfg,
            "cfg_cumulative" => BindingTag::CfgCumulative,
            "const" => BindingTag::Const,
            "build" => BindingTag::Build,
            other => {
                let span = self.peek().span.clone();
                self.error_at(
                    format!(
                        "unknown binding '{other}' (expected cfg, cfg_cumulative, const, build)"
                    ),
                    span,
                );
                return None;
            }
        };
        let end = self.advance().span.clone();
        Some(ConfigProp::Binding {
            tag,
            span: merge_spans(&start, &end),
        })
    }

    fn parse_literal(&mut self) -> Option<(Literal, SourceSpan)> {
        match self.peek_kind() {
            TokenKind::True => {
                let s = self.advance().span.clone();
                Some((Literal::Bool(true), s))
            }
            TokenKind::False => {
                let s = self.advance().span.clone();
                Some((Literal::Bool(false), s))
            }
            TokenKind::Integer(v) => {
                let v = *v;
                let s = self.advance().span.clone();
                Some((Literal::Int(v), s))
            }
            TokenKind::String(v) => {
                let v = v.clone();
                let s = self.advance().span.clone();
                Some((Literal::String(v), s))
            }
            TokenKind::Ident(v) => {
                let v = v.clone();
                let s = self.advance().span.clone();
                Some((Literal::Ident(v), s))
            }
            _ => {
                self.error_here("expected literal value (bool, integer, string, or identifier)");
                None
            }
        }
    }
}

fn describe_token(kind: &TokenKind) -> String {
    match kind {
        TokenKind::Ident(n) => format!("identifier '{n}'"),
        TokenKind::String(_) => "string literal".to_string(),
        TokenKind::Integer(_) => "integer literal".to_string(),
        TokenKind::True => "'true'".to_string(),
        TokenKind::False => "'false'".to_string(),
        TokenKind::LBrace => "'{'".to_string(),
        TokenKind::RBrace => "'}'".to_string(),
        TokenKind::LParen => "'('".to_string(),
        TokenKind::RParen => "')'".to_string(),
        TokenKind::LBracket => "'['".to_string(),
        TokenKind::RBracket => "']'".to_string(),
        TokenKind::Colon => "':'".to_string(),
        TokenKind::Comma => "','".to_string(),
        TokenKind::Eq => "'='".to_string(),
        TokenKind::DotDot => "'..'".to_string(),
        TokenKind::DotDotEq => "'..='".to_string(),
        TokenKind::AndAnd => "'&&'".to_string(),
        TokenKind::OrOr => "'||'".to_string(),
        TokenKind::Bang => "'!'".to_string(),
        TokenKind::Eof => "end of input".to_string(),
    }
}

/// Merge two spans assumed to originate from the same file. The result
/// spans from the start of `a` to the end of `b`.
fn merge_spans(a: &SourceSpan, b: &SourceSpan) -> SourceSpan {
    SourceSpan {
        file: a.file.clone(),
        line: a.line,
        col: a.col,
        end_line: b.end_line,
        end_col: b.end_col,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kconfig::lexer::lex;
    use std::path::Path;

    fn parse_ok(src: &str) -> File {
        let toks = lex(src, Path::new("t.kconfig")).expect("lex ok");
        parse(&toks).unwrap_or_else(|diags| {
            panic!(
                "expected clean parse, got diagnostics: {:#?}",
                diags.iter().map(|d| &d.message).collect::<Vec<_>>()
            )
        })
    }

    fn parse_err(src: &str) -> Vec<Diagnostic> {
        let toks = lex(src, Path::new("t.kconfig")).expect("lex ok");
        parse(&toks).expect_err("expected parse error")
    }

    fn single_config(file: &File) -> &ConfigBlock {
        assert_eq!(file.items.len(), 1);
        match &file.items[0] {
            Item::Config(cb) => cb,
            other => panic!("expected Item::Config, got {other:?}"),
        }
    }

    #[test]
    fn empty_file_parses_to_empty_items() {
        let file = parse_ok("");
        assert!(file.items.is_empty());
    }

    #[test]
    fn minimal_bool_config() {
        let file = parse_ok("config DEBUG_LOG: bool {}");
        let cb = single_config(&file);
        assert_eq!(cb.name, "DEBUG_LOG");
        assert_eq!(cb.ty, TypeTag::Bool);
        assert!(cb.props.is_empty());
    }

    #[test]
    fn bool_with_default_and_help() {
        let file = parse_ok(
            r#"
            config DEBUG_LOG: bool {
                default = true
                help = "Enable debug logging"
            }
        "#,
        );
        let cb = single_config(&file);
        assert_eq!(cb.props.len(), 2);
        match &cb.props[0] {
            ConfigProp::Default { value, .. } => assert_eq!(*value, Literal::Bool(true)),
            other => panic!("expected Default, got {other:?}"),
        }
        match &cb.props[1] {
            ConfigProp::Help { text, .. } => assert_eq!(text, "Enable debug logging"),
            other => panic!("expected Help, got {other:?}"),
        }
    }

    #[test]
    fn u32_with_range_inclusive() {
        let file = parse_ok(
            r#"
            config LOG_LEVEL: u32 {
                default = 3
                range = 0..=5
            }
        "#,
        );
        let cb = single_config(&file);
        assert_eq!(cb.ty, TypeTag::U32);
        let range = cb
            .props
            .iter()
            .find_map(|p| match p {
                ConfigProp::Range {
                    low,
                    high,
                    inclusive,
                    ..
                } => Some((*low, *high, *inclusive)),
                _ => None,
            })
            .expect("range prop");
        assert_eq!(range, (0, 5, true));
    }

    #[test]
    fn u32_with_range_exclusive() {
        let file = parse_ok("config X: u32 { range = 0..100 }");
        let cb = single_config(&file);
        let range = cb.props.iter().find_map(|p| match p {
            ConfigProp::Range { inclusive, .. } => Some(*inclusive),
            _ => None,
        });
        assert_eq!(range, Some(false));
    }

    #[test]
    fn range_low_greater_than_high_rejected() {
        let diags = parse_err("config X: u32 { range = 10..5 }");
        assert!(diags.iter().any(|d| d.message.contains("lower bound")));
    }

    #[test]
    fn choices_list_parses() {
        let file = parse_ok(r#"config MODE: choice { choices = ["debug", "release", "profile"] }"#);
        let cb = single_config(&file);
        match cb.props.first().unwrap() {
            ConfigProp::Choices { variants, .. } => {
                assert_eq!(variants, &vec!["debug", "release", "profile"]);
            }
            other => panic!("expected Choices, got {other:?}"),
        }
    }

    #[test]
    fn choices_empty_list_allowed() {
        // Empty list is a degenerate but legal form. Lowerer will reject
        // it with a model-level error; parser accepts it.
        let file = parse_ok(r#"config MODE: choice { choices = [] }"#);
        let cb = single_config(&file);
        match cb.props.first().unwrap() {
            ConfigProp::Choices { variants, .. } => assert!(variants.is_empty()),
            other => panic!("expected Choices, got {other:?}"),
        }
    }

    #[test]
    fn choices_trailing_comma_allowed() {
        let file = parse_ok(r#"config MODE: choice { choices = ["a", "b",] }"#);
        let cb = single_config(&file);
        match cb.props.first().unwrap() {
            ConfigProp::Choices { variants, .. } => assert_eq!(variants.len(), 2),
            other => panic!("expected Choices, got {other:?}"),
        }
    }

    #[test]
    fn menu_label_and_bindings_parse() {
        let file = parse_ok(
            r#"
            config DEBUG_LOG: bool {
                menu = "Logging"
                binding = cfg
                binding = const
            }
        "#,
        );
        let cb = single_config(&file);
        let labels: Vec<&str> = cb
            .props
            .iter()
            .filter_map(|p| match p {
                ConfigProp::MenuLabel { label, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(labels, vec!["Logging"]);
        let bindings: Vec<BindingTag> = cb
            .props
            .iter()
            .filter_map(|p| match p {
                ConfigProp::Binding { tag, .. } => Some(*tag),
                _ => None,
            })
            .collect();
        assert_eq!(bindings, vec![BindingTag::Cfg, BindingTag::Const]);
    }

    #[test]
    fn unknown_type_tag_rejected() {
        let diags = parse_err("config X: nope {}");
        assert!(diags.iter().any(|d| d.message.contains("unknown type tag")));
    }

    #[test]
    fn unknown_binding_rejected() {
        let diags = parse_err("config X: bool { binding = weird }");
        assert!(diags.iter().any(|d| d.message.contains("unknown binding")));
    }

    #[test]
    fn unknown_property_rejected() {
        let diags = parse_err("config X: bool { floob = 1 }");
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("unknown config property"))
        );
    }

    #[test]
    fn missing_colon_rejected() {
        let diags = parse_err("config X bool {}");
        assert!(diags.iter().any(|d| d.message.contains("':'")));
    }

    fn depends_of(cb: &ConfigBlock) -> Option<&Expr> {
        cb.props.iter().find_map(|p| match p {
            ConfigProp::DependsOn { expr, .. } => Some(expr),
            _ => None,
        })
    }

    #[test]
    fn depends_on_single_ident() {
        let file = parse_ok("config X: bool { depends_on = Y }");
        let cb = single_config(&file);
        assert_eq!(depends_of(cb), Some(&Expr::Ident("Y".into())));
    }

    #[test]
    fn depends_on_and_flattens() {
        let file = parse_ok("config X: bool { depends_on = A && B && C }");
        let cb = single_config(&file);
        // Consecutive `&&` should fold into a single flat `And` vec
        // for a cleaner AST and faster evaluation.
        match depends_of(cb).unwrap() {
            Expr::And(xs) => {
                let names: Vec<&str> = xs
                    .iter()
                    .map(|x| match x {
                        Expr::Ident(n) => n.as_str(),
                        _ => panic!("expected ident, got {x:?}"),
                    })
                    .collect();
                assert_eq!(names, vec!["A", "B", "C"]);
            }
            other => panic!("expected And, got {other:?}"),
        }
    }

    #[test]
    fn depends_on_or_flattens() {
        let file = parse_ok("config X: bool { depends_on = A || B || C }");
        let cb = single_config(&file);
        match depends_of(cb).unwrap() {
            Expr::Or(xs) => assert_eq!(xs.len(), 3),
            other => panic!("expected Or, got {other:?}"),
        }
    }

    #[test]
    fn depends_on_and_binds_tighter_than_or() {
        // `A || B && C` must parse as `A || (B && C)`, not `(A || B) && C`.
        let file = parse_ok("config X: bool { depends_on = A || B && C }");
        let cb = single_config(&file);
        match depends_of(cb).unwrap() {
            Expr::Or(xs) => {
                assert_eq!(xs.len(), 2);
                assert_eq!(xs[0], Expr::Ident("A".into()));
                match &xs[1] {
                    Expr::And(ys) => assert_eq!(ys.len(), 2),
                    other => panic!("expected And on rhs, got {other:?}"),
                }
            }
            other => panic!("expected Or at top, got {other:?}"),
        }
    }

    #[test]
    fn depends_on_not_binds_tightest() {
        // `!A && B` must parse as `(!A) && B`.
        let file = parse_ok("config X: bool { depends_on = !A && B }");
        let cb = single_config(&file);
        match depends_of(cb).unwrap() {
            Expr::And(xs) => {
                assert_eq!(xs.len(), 2);
                match &xs[0] {
                    Expr::Not(inner) => assert_eq!(**inner, Expr::Ident("A".into())),
                    other => panic!("expected Not on lhs, got {other:?}"),
                }
            }
            other => panic!("expected And, got {other:?}"),
        }
    }

    #[test]
    fn depends_on_parens_override_precedence() {
        // `(A || B) && C` must parse as `And(Or(A, B), C)`.
        let file = parse_ok("config X: bool { depends_on = (A || B) && C }");
        let cb = single_config(&file);
        match depends_of(cb).unwrap() {
            Expr::And(xs) => {
                assert_eq!(xs.len(), 2);
                match &xs[0] {
                    Expr::Or(ys) => assert_eq!(ys.len(), 2),
                    other => panic!("expected Or inside parens, got {other:?}"),
                }
                assert_eq!(xs[1], Expr::Ident("C".into()));
            }
            other => panic!("expected And, got {other:?}"),
        }
    }

    #[test]
    fn depends_on_true_false_literals_allowed() {
        let file = parse_ok("config X: bool { depends_on = true && !false }");
        let cb = single_config(&file);
        match depends_of(cb).unwrap() {
            Expr::And(xs) => {
                assert_eq!(xs[0], Expr::Const(true));
                match &xs[1] {
                    Expr::Not(inner) => assert_eq!(**inner, Expr::Const(false)),
                    other => panic!("expected Not(Const), got {other:?}"),
                }
            }
            other => panic!("expected And, got {other:?}"),
        }
    }

    #[test]
    fn depends_on_eval_differs_between_and_and_or() {
        // The whole point of K4 over hadron-style flatten: A && B
        // and A || B must resolve differently when only one is on.
        let file_and = parse_ok("config X: bool { depends_on = A && B }");
        let file_or = parse_ok("config Y: bool { depends_on = A || B }");
        let and_expr = depends_of(single_config(&file_and)).unwrap().clone();
        let or_expr = depends_of(single_config(&file_or)).unwrap().clone();
        let lookup = |name: &str| match name {
            "A" => Some(true),
            "B" => Some(false),
            _ => None,
        };
        assert!(!and_expr.eval(&lookup));
        assert!(or_expr.eval(&lookup));
    }

    #[test]
    fn visible_if_accepts_expression() {
        let file = parse_ok("config X: bool { visible_if = !DISABLED }");
        let cb = single_config(&file);
        let expr = cb.props.iter().find_map(|p| match p {
            ConfigProp::VisibleIf { expr, .. } => Some(expr),
            _ => None,
        });
        match expr {
            Some(Expr::Not(inner)) => assert_eq!(**inner, Expr::Ident("DISABLED".into())),
            other => panic!("expected Not(Ident), got {other:?}"),
        }
    }

    #[test]
    fn selects_is_flat_ident_list() {
        let file = parse_ok("config X: bool { selects = [A, B, C] }");
        let cb = single_config(&file);
        let names = cb.props.iter().find_map(|p| match p {
            ConfigProp::Selects { names, .. } => Some(names),
            _ => None,
        });
        assert_eq!(
            names.cloned(),
            Some(vec!["A".to_string(), "B".into(), "C".into()])
        );
    }

    #[test]
    fn selects_rejects_expression_form() {
        // `selects` is deliberately flat — expression form would be a
        // semantic error. `&&` inside selects should produce a parse
        // error (expected ',' or ']' after the first ident).
        let diags = parse_err("config X: bool { selects = [A && B] }");
        assert!(diags.iter().any(|d| d.message.contains("',' or ']'")));
    }

    #[test]
    fn unmatched_paren_in_expression_rejected() {
        let diags = parse_err("config X: bool { depends_on = (A && B }");
        assert!(diags.iter().any(|d| d.message.contains("')'")));
    }

    #[test]
    fn empty_expression_after_not_rejected() {
        let diags = parse_err("config X: bool { depends_on = ! }");
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("expected expression"))
        );
    }

    #[test]
    fn multiple_configs_in_one_file() {
        let file = parse_ok(
            r#"
            config A: bool {}
            config B: u32 { default = 7 }
            config C: str { default = "x" }
        "#,
        );
        assert_eq!(file.items.len(), 3);
        let names: Vec<&str> = file
            .items
            .iter()
            .map(|i| match i {
                Item::Config(cb) => cb.name.as_str(),
                other => panic!("expected Config, got {other:?}"),
            })
            .collect();
        assert_eq!(names, vec!["A", "B", "C"]);
    }

    #[test]
    fn error_recovery_continues_after_bad_item() {
        // First item has an unknown type tag and should be skipped;
        // second item should still parse. Two diagnostics expected:
        // the unknown tag + the unrecoverable item.
        let toks = lex(
            r#"
            config A: wat {}
            config B: bool {}
        "#,
            Path::new("t.kconfig"),
        )
        .unwrap();
        let err = parse(&toks).expect_err("should error");
        assert!(err.iter().any(|d| d.message.contains("unknown type tag")));
    }

    #[test]
    fn span_covers_whole_block() {
        let src = "config X: bool {}";
        let file = parse_ok(src);
        let cb = single_config(&file);
        // Block starts at col 1 (on `config`) and ends at col 18 (after `}`).
        assert_eq!(cb.span.line, 1);
        assert_eq!(cb.span.col, 1);
        assert_eq!(cb.span.end_line, 1);
        assert_eq!(cb.span.end_col, 18);
    }

    // --- K5: menu / preset / source tests ---

    #[test]
    fn simple_menu_with_one_config() {
        let file = parse_ok(
            r#"
            menu "Logging" {
                config LOG_LEVEL: u32 { default = 3 }
            }
        "#,
        );
        assert_eq!(file.items.len(), 1);
        match &file.items[0] {
            Item::Menu(mb) => {
                assert_eq!(mb.title, "Logging");
                assert_eq!(mb.items.len(), 1);
                match &mb.items[0] {
                    Item::Config(cb) => assert_eq!(cb.name, "LOG_LEVEL"),
                    other => panic!("expected Config in menu, got {other:?}"),
                }
            }
            other => panic!("expected Menu, got {other:?}"),
        }
    }

    #[test]
    fn nested_menus() {
        // Menus should recurse: `menu { menu { config } }`.
        let file = parse_ok(
            r#"
            menu "Outer" {
                menu "Inner" {
                    config A: bool {}
                }
            }
        "#,
        );
        let outer = match &file.items[0] {
            Item::Menu(m) => m,
            other => panic!("expected outer menu, got {other:?}"),
        };
        let inner = match &outer.items[0] {
            Item::Menu(m) => m,
            other => panic!("expected inner menu, got {other:?}"),
        };
        assert_eq!(inner.title, "Inner");
        match &inner.items[0] {
            Item::Config(cb) => assert_eq!(cb.name, "A"),
            other => panic!("expected config in inner menu, got {other:?}"),
        }
    }

    #[test]
    fn menu_without_title_rejected() {
        let diags = parse_err("menu { config A: bool {} }");
        assert!(diags.iter().any(|d| d.message.contains("menu title")));
    }

    #[test]
    fn preset_with_inherits_and_overrides() {
        let file = parse_ok(
            r#"
            preset "dev" {
                inherits = "base"
                help = "Developer defaults"
                DEBUG_LOG = true
                LOG_LEVEL = 4
                NAME = "foo"
            }
        "#,
        );
        let pb = match &file.items[0] {
            Item::Preset(p) => p,
            other => panic!("expected preset, got {other:?}"),
        };
        assert_eq!(pb.name, "dev");
        assert_eq!(pb.props.len(), 5);
        match &pb.props[0] {
            PresetProp::Inherits { parent, .. } => assert_eq!(parent, "base"),
            other => panic!("expected Inherits, got {other:?}"),
        }
        match &pb.props[1] {
            PresetProp::Help { text, .. } => assert_eq!(text, "Developer defaults"),
            other => panic!("expected Help, got {other:?}"),
        }
        match &pb.props[2] {
            PresetProp::Override { option, value, .. } => {
                assert_eq!(option, "DEBUG_LOG");
                assert_eq!(*value, Literal::Bool(true));
            }
            other => panic!("expected Override, got {other:?}"),
        }
        match &pb.props[3] {
            PresetProp::Override { option, value, .. } => {
                assert_eq!(option, "LOG_LEVEL");
                assert_eq!(*value, Literal::Int(4));
            }
            other => panic!("expected Override, got {other:?}"),
        }
        match &pb.props[4] {
            PresetProp::Override { option, value, .. } => {
                assert_eq!(option, "NAME");
                assert_eq!(*value, Literal::String("foo".into()));
            }
            other => panic!("expected Override, got {other:?}"),
        }
    }

    #[test]
    fn source_directive_captures_path() {
        let file = parse_ok(r#"source "./sub/options.kconfig""#);
        match &file.items[0] {
            Item::Source(s) => assert_eq!(s.path, "./sub/options.kconfig"),
            other => panic!("expected source, got {other:?}"),
        }
    }

    #[test]
    fn source_without_path_rejected() {
        let diags = parse_err("source");
        assert!(diags.iter().any(|d| d.message.contains("source path")));
    }

    #[test]
    fn mixed_file_with_sources_menus_presets_configs() {
        let file = parse_ok(
            r#"
            source "./base.kconfig"
            config TOPLEVEL: bool { default = false }
            menu "Group" {
                config INSIDE: u32 { default = 1 }
            }
            preset "dev" {
                TOPLEVEL = true
                INSIDE = 7
            }
        "#,
        );
        assert_eq!(file.items.len(), 4);
        assert!(matches!(&file.items[0], Item::Source(_)));
        assert!(matches!(&file.items[1], Item::Config(_)));
        assert!(matches!(&file.items[2], Item::Menu(_)));
        assert!(matches!(&file.items[3], Item::Preset(_)));
    }
}
