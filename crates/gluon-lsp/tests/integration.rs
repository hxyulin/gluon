//! Integration test: parse a real gluon.rhai fixture through the
//! full analysis pipeline and verify tokens + diagnostics.

use gluon_lsp::analysis::{self, Severity, TokenType};
use gluon_lsp::parser::{rhai::RhaiParser, Parser};

#[test]
fn analyze_minimal_uefi_fixture() {
    let source = include_str!("../../../tests/fixtures/minimal-uefi/gluon.rhai");
    let schema = gluon_core::engine::dsl_schema();
    let parser = RhaiParser::new();
    let tree = parser.parse(source);
    let result = analysis::analyze(&tree, &schema);

    // No semantic errors expected on a valid fixture
    let errors: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .collect();
    assert!(
        errors.is_empty(),
        "unexpected errors analyzing valid fixture:\n{:#?}",
        errors
    );

    // Should produce semantic tokens
    assert!(!result.tokens.is_empty(), "expected semantic tokens");

    // Verify we see constructor tokens: project, target, profile, group (x2), qemu
    let functions: Vec<_> = result
        .tokens
        .iter()
        .filter(|t| t.token_type == TokenType::Function)
        .collect();
    assert!(
        functions.len() >= 5,
        "expected at least 5 function tokens in the fixture, got {}: {:#?}",
        functions.len(),
        functions
    );

    // Verify we see many method tokens (builder chain methods)
    let methods: Vec<_> = result
        .tokens
        .iter()
        .filter(|t| t.token_type == TokenType::Method)
        .collect();
    assert!(
        methods.len() >= 10,
        "expected at least 10 method tokens in the fixture, got {}",
        methods.len()
    );
}

#[test]
fn analyze_detects_invalid_method() {
    // Synthetic broken case: call a QemuBuilder method on a GroupBuilder
    let source = r#"group("kernel").memory(256);"#;
    let schema = gluon_core::engine::dsl_schema();
    let parser = RhaiParser::new();
    let tree = parser.parse(source);
    let result = analysis::analyze(&tree, &schema);

    let errors: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .collect();
    assert!(!errors.is_empty(), "expected an error for invalid method");
    assert!(
        errors[0].message.contains("memory") || errors[0].message.contains("GroupBuilder"),
        "error should mention the method or builder: {:?}",
        errors[0].message
    );
}
