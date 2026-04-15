//! Convert analysis diagnostics to LSP diagnostic format.

use crate::analysis;
use crate::parser::TextRange;
use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

pub fn to_lsp_diagnostics(diagnostics: &[analysis::Diagnostic]) -> Vec<Diagnostic> {
    diagnostics
        .iter()
        .map(|d| Diagnostic {
            range: text_range_to_lsp(d.range),
            severity: Some(match d.severity {
                analysis::Severity::Error => DiagnosticSeverity::ERROR,
                analysis::Severity::Warning => DiagnosticSeverity::WARNING,
            }),
            source: Some("gluon-lsp".to_string()),
            message: d.message.clone(),
            ..Default::default()
        })
        .collect()
}

pub fn text_range_to_lsp(range: TextRange) -> Range {
    Range {
        start: Position {
            line: range.start_line,
            character: range.start_col,
        },
        end: Position {
            line: range.end_line,
            character: range.end_col,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::{Diagnostic as AnalysisDiagnostic, Severity};
    use crate::parser::TextRange;

    #[test]
    fn converts_error_diagnostic() {
        let range = TextRange {
            start_byte: 0,
            end_byte: 6,
            start_line: 3,
            start_col: 4,
            end_line: 3,
            end_col: 10,
        };
        let diags = vec![AnalysisDiagnostic {
            range,
            severity: Severity::Error,
            message: "unknown DSL function `oops`".to_string(),
        }];
        let lsp_diags = to_lsp_diagnostics(&diags);
        assert_eq!(lsp_diags.len(), 1);
        let d = &lsp_diags[0];
        assert_eq!(d.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(d.source.as_deref(), Some("gluon-lsp"));
        assert_eq!(d.range.start.line, 3);
        assert_eq!(d.range.start.character, 4);
        assert_eq!(d.range.end.line, 3);
        assert_eq!(d.range.end.character, 10);
        assert!(d.message.contains("oops"));
    }
}
