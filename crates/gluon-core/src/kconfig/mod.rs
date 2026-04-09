//! `.kconfig` file parser and loader.
//!
//! Gluon config options can be declared in two equivalent ways:
//!
//! 1. **Inline in Rhai** via the `engine::builders::config` module
//!    (`config_bool("NAME", ...)` and friends).
//! 2. **Externally in a `.kconfig` file** loaded from `gluon.rhai` via the
//!    `load_kconfig("./options.kconfig")` Rhai function.
//!
//! Both paths produce the same [`gluon_model::ConfigOptionDef`] and
//! [`gluon_model::PresetDef`] shapes, so downstream resolution and codegen
//! are unaware of the source format. The `.kconfig` form additionally
//! populates [`gluon_model::ConfigOptionDef::depends_on_expr`] and
//! [`gluon_model::ConfigOptionDef::visible_if_expr`] with full boolean
//! expressions (`&&`, `||`, `!`, grouping), which the resolver evaluates
//! semantically rather than as a flat AND-of-symbols.
//!
//! The module is split into four layers:
//!
//! - [`lexer`] — hand-rolled tokenizer producing `Token`s with spans.
//! - `parser` (arrives in chunk K3) — recursive-descent parser over tokens.
//! - `ast` (arrives in chunk K3) — pure data AST shape.
//! - `lower` (arrives in chunk K6) — lowers the AST to model types,
//!   reusing the existing conversion logic where practical.

pub mod ast;
pub mod lexer;
pub mod loader;
pub mod lower;
pub mod parser;

pub use loader::load_kconfig;
pub use lower::Lowered;
