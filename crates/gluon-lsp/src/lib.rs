//! Gluon LSP server library.
//!
//! The modules are also used by the binary (`main.rs`). The library
//! target exists so integration tests can exercise the analysis
//! pipeline without starting the LSP transport.

pub mod analysis;
pub mod completion;
pub mod diagnostics;
pub mod hover;
pub mod parser;
pub mod semantic_tokens;
pub mod word;
