//! Pluggable parser abstraction.
//!
//! Defines the [`Parser`] trait and a concrete [`Node`] enum that the
//! semantic analysis layer works against. The parser implementation
//! (currently tree-sitter-rhai) is swappable — when the custom DSL
//! replaces Rhai, only the parser module changes.

pub mod rhai; // Will be implemented in Task 3

/// A position range in source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextRange {
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
}

/// Concrete AST node. Pattern-matchable, no trait objects.
#[derive(Debug, Clone)]
pub enum Node {
    /// Top-level function call: `project("name", "1.0")`
    FnCall {
        name: String,
        name_range: TextRange,
        args: Vec<Node>,
        range: TextRange,
    },
    /// Method call in a chain: `.target("host")`
    MethodCall {
        receiver: Box<Node>,
        method: String,
        method_range: TextRange,
        args: Vec<Node>,
        range: TextRange,
    },
    /// Bare identifier: `LIB`, `BIN`
    Identifier {
        name: String,
        range: TextRange,
    },
    /// String literal: `"hello"`
    StringLiteral {
        value: String,
        range: TextRange,
    },
    /// Integer literal: `256`
    IntLiteral {
        value: i64,
        range: TextRange,
    },
    /// Boolean literal: `true`, `false`
    BoolLiteral {
        value: bool,
        range: TextRange,
    },
    /// Array literal: `["a", "b"]`
    ArrayLiteral {
        elements: Vec<Node>,
        range: TextRange,
    },
    /// Map literal: `#{ key: value }`
    MapLiteral {
        range: TextRange,
    },
    /// Comment (line or block)
    Comment {
        range: TextRange,
    },
    /// Parse error node — tree-sitter error recovery
    Error {
        range: TextRange,
    },
}

/// The parsed syntax tree for a document.
#[derive(Debug, Clone)]
pub struct SyntaxTree {
    /// Top-level statements.
    pub statements: Vec<Node>,
    /// Byte ranges of parse errors.
    pub errors: Vec<TextRange>,
}

/// Pluggable parser interface.
pub trait Parser {
    fn parse(&self, source: &str) -> SyntaxTree;
}

impl Node {
    /// Get the text range of this node.
    pub fn range(&self) -> TextRange {
        match self {
            Node::FnCall { range, .. }
            | Node::MethodCall { range, .. }
            | Node::Identifier { range, .. }
            | Node::StringLiteral { range, .. }
            | Node::IntLiteral { range, .. }
            | Node::BoolLiteral { range, .. }
            | Node::ArrayLiteral { range, .. }
            | Node::MapLiteral { range, .. }
            | Node::Comment { range, .. }
            | Node::Error { range, .. } => *range,
        }
    }
}
