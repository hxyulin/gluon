//! LSP semantic token encoding.
//!
//! Converts [`SemanticToken`] from the analysis layer into the delta-encoded
//! format LSP clients expect from `textDocument/semanticTokens/full`.

use crate::analysis::{SemanticToken, TokenModifiers, TokenType};
use lsp_types::{
    SemanticToken as LspToken, SemanticTokenModifier, SemanticTokenType, SemanticTokensLegend,
};

pub const TOKEN_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::FUNCTION, // index 0
    SemanticTokenType::METHOD,   // index 1
    SemanticTokenType::VARIABLE, // index 2
];

pub const TOKEN_MODIFIERS: &[SemanticTokenModifier] = &[
    SemanticTokenModifier::DECLARATION, // bit 0
    SemanticTokenModifier::READONLY,    // bit 1
];

pub fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: TOKEN_TYPES.to_vec(),
        token_modifiers: TOKEN_MODIFIERS.to_vec(),
    }
}

/// Encode tokens into LSP delta format.
///
/// LSP uses relative positioning: each token's line/col is relative to the
/// previous token. The first token is relative to the start of the file
/// (line 0, col 0). Tokens on the same line encode `delta_line = 0` and
/// `delta_start` relative to the previous token's start column; tokens on a
/// new line encode the absolute column as `delta_start`.
pub fn encode(tokens: &[SemanticToken]) -> Vec<LspToken> {
    let mut result = Vec::with_capacity(tokens.len());
    let mut prev_line = 0u32;
    let mut prev_col = 0u32;
    for token in tokens {
        let line = token.range.start_line;
        let col = token.range.start_col;
        let length = (token.range.end_byte - token.range.start_byte) as u32;
        let delta_line = line - prev_line;
        let delta_start = if delta_line == 0 { col - prev_col } else { col };
        result.push(LspToken {
            delta_line,
            delta_start,
            length,
            token_type: token_type_index(token.token_type),
            token_modifiers_bitset: modifier_bits(token.modifiers),
        });
        prev_line = line;
        prev_col = col;
    }
    result
}

fn token_type_index(tt: TokenType) -> u32 {
    match tt {
        TokenType::Function => 0,
        TokenType::Method => 1,
        TokenType::Variable => 2,
    }
}

fn modifier_bits(mods: TokenModifiers) -> u32 {
    let mut bits = 0u32;
    if mods.declaration {
        bits |= 1 << 0;
    }
    if mods.readonly {
        bits |= 1 << 1;
    }
    bits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::{SemanticToken, TokenModifiers, TokenType};
    use crate::parser::TextRange;

    fn range(start_line: u32, start_col: u32, end_col: u32) -> TextRange {
        TextRange {
            start_byte: start_col as usize,
            end_byte: end_col as usize,
            start_line,
            start_col,
            end_line: start_line,
            end_col,
        }
    }

    #[test]
    fn encodes_single_token() {
        // "project" at (0,0), length 7, Function type, no modifiers.
        let tokens = vec![SemanticToken {
            range: range(0, 0, 7),
            token_type: TokenType::Function,
            modifiers: TokenModifiers::default(),
        }];
        let encoded = encode(&tokens);
        assert_eq!(encoded.len(), 1);
        let t = &encoded[0];
        assert_eq!(t.delta_line, 0);
        assert_eq!(t.delta_start, 0);
        assert_eq!(t.length, 7);
        assert_eq!(t.token_type, 0); // Function
        assert_eq!(t.token_modifiers_bitset, 0);
    }

    #[test]
    fn encodes_delta_positions() {
        // Two tokens on the same line: "group" at col 0, "target" at col 10.
        // The second token's delta_start should be 10 - 0 = 10.
        let tokens = vec![
            SemanticToken {
                range: range(0, 0, 5),
                token_type: TokenType::Function,
                modifiers: TokenModifiers::default(),
            },
            SemanticToken {
                range: range(0, 10, 16),
                token_type: TokenType::Method,
                modifiers: TokenModifiers::default(),
            },
        ];
        let encoded = encode(&tokens);
        assert_eq!(encoded.len(), 2);
        assert_eq!(encoded[0].delta_line, 0);
        assert_eq!(encoded[0].delta_start, 0);
        assert_eq!(encoded[1].delta_line, 0);
        assert_eq!(encoded[1].delta_start, 10); // relative to previous col 0
    }

    #[test]
    fn encodes_multiline_delta() {
        // Token at (0, 5) then token at (2, 8).
        // Second token: delta_line = 2, delta_start = 8 (absolute col, not relative).
        let tokens = vec![
            SemanticToken {
                range: range(0, 5, 10),
                token_type: TokenType::Function,
                modifiers: TokenModifiers::default(),
            },
            SemanticToken {
                range: range(2, 8, 14),
                token_type: TokenType::Method,
                modifiers: TokenModifiers::default(),
            },
        ];
        let encoded = encode(&tokens);
        assert_eq!(encoded.len(), 2);
        assert_eq!(encoded[1].delta_line, 2);
        assert_eq!(encoded[1].delta_start, 8); // absolute col because delta_line != 0
    }

    #[test]
    fn encodes_readonly_modifier() {
        // Variable token with readonly=true should have bitset = 0b10 (bit 1 set).
        let tokens = vec![SemanticToken {
            range: range(0, 0, 3),
            token_type: TokenType::Variable,
            modifiers: TokenModifiers {
                declaration: false,
                readonly: true,
            },
        }];
        let encoded = encode(&tokens);
        assert_eq!(encoded.len(), 1);
        assert_eq!(encoded[0].token_modifiers_bitset, 0b10);
    }
}
