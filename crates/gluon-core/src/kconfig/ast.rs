//! Pure-data AST for a parsed `.kconfig` file.
//!
//! The AST is intentionally dumb: no validation, no semantic checks, no
//! behavior. It's a one-to-one structural mirror of what the parser saw,
//! annotated with spans so later passes (lowering, validation, error
//! rendering) can point at the source.
//!
//! Lowering to [`gluon_model::ConfigOptionDef`] and
//! [`gluon_model::PresetDef`] happens in the `lower` module (chunk K6).

use gluon_model::{Expr, SourceSpan};

/// Root of a parsed `.kconfig` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    pub items: Vec<Item>,
}

/// A top-level declaration in a `.kconfig` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Config(ConfigBlock),
    /// `menu "Title" { items... }` — visual grouping used by TUI
    /// menuconfig. Menus can nest arbitrarily. Lowering flattens them:
    /// each inner `ConfigBlock` has its enclosing menu titles composed
    /// into [`gluon_model::ConfigOptionDef::menu`] as a dot-path
    /// (`"Outer.Inner"`), rather than surfacing a hierarchical type in
    /// the model.
    Menu(MenuBlock),
    /// `preset "name" { inherits, help, per-option overrides }`.
    Preset(PresetBlock),
    /// `source "./path"` — include another `.kconfig` file relative to
    /// the current file. The parser only records the string; the loader
    /// does file I/O and cycle detection.
    Source(SourceDecl),
}

/// `menu "Title" { items... }`.
///
/// Items inside a menu can themselves be configs, nested menus, presets,
/// or sources — the AST is recursive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuBlock {
    pub title: String,
    pub title_span: SourceSpan,
    pub items: Vec<Item>,
    /// Span from the `menu` keyword through the closing `}`.
    pub span: SourceSpan,
}

/// `preset "name" { ... }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetBlock {
    pub name: String,
    pub name_span: SourceSpan,
    pub props: Vec<PresetProp>,
    pub span: SourceSpan,
}

/// A single property inside a `preset { ... }` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresetProp {
    /// `inherits = "parent_preset_name"`.
    Inherits { parent: String, span: SourceSpan },
    /// `help = "..."`.
    Help { text: String, span: SourceSpan },
    /// `OPTION_NAME = value` — an override for one config option.
    Override {
        option: String,
        value: Literal,
        span: SourceSpan,
    },
}

/// `source "./path"` top-level directive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceDecl {
    /// Raw path string as written in the source. The loader is
    /// responsible for relative-path resolution and cycle detection.
    pub path: String,
    pub span: SourceSpan,
}

/// `config NAME: type { props... }` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigBlock {
    pub name: String,
    pub name_span: SourceSpan,
    pub ty: TypeTag,
    pub ty_span: SourceSpan,
    pub props: Vec<ConfigProp>,
    /// Span of the whole block from `config` through `}`.
    pub span: SourceSpan,
}

/// Type tag from `config NAME: TAG { ... }`. Mirrors
/// [`gluon_model::ConfigType`] but lives at the parser layer so we can
/// cleanly reject unknown tags with a span-aware diagnostic without
/// coupling to the model enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeTag {
    Bool,
    Tristate,
    U32,
    U64,
    Str,
    Choice,
    List,
    Group,
}

/// A single property inside a `config { ... }` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigProp {
    Default {
        value: Literal,
        span: SourceSpan,
    },
    Help {
        text: String,
        span: SourceSpan,
    },
    Range {
        low: u64,
        high: u64,
        /// `true` for `..=`, `false` for `..`.
        inclusive: bool,
        span: SourceSpan,
    },
    Choices {
        variants: Vec<String>,
        span: SourceSpan,
    },
    /// Label used by TUI menuconfig to group options visually.
    MenuLabel {
        label: String,
        span: SourceSpan,
    },
    Binding {
        tag: BindingTag,
        span: SourceSpan,
    },
    /// Boolean expression over option identifiers. The resolver
    /// evaluates this semantically (`&&`, `||`, `!`, grouping) rather
    /// than flattening to a symbol set, so `A && B` and `A || B`
    /// produce different answers.
    DependsOn {
        expr: Expr,
        span: SourceSpan,
    },
    /// Boolean expression governing TUI visibility. Same grammar as
    /// `depends_on` but with no runtime effect on resolution today.
    VisibleIf {
        expr: Expr,
        span: SourceSpan,
    },
    /// Flat list of option names that should be forced on when this
    /// option is enabled. Unlike `depends_on`, selects has always been
    /// an implicit union so there is no expression form.
    Selects {
        names: Vec<String>,
        span: SourceSpan,
    },
}

/// Value literal used in `default = ...` and `choices = [...]` and
/// preset overrides. Parser-level only; lowering maps this onto
/// [`gluon_model::ConfigValue`] with type-directed coercion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Literal {
    Bool(bool),
    /// Unsigned integer. Negative integers are not part of the grammar.
    Int(u64),
    String(String),
    /// Bare identifier used for tristate values (`yes`/`no`/`module`)
    /// and for choice variant selection. The lowerer disambiguates
    /// based on the enclosing option's declared type.
    Ident(String),
}

/// Binding flavor selected in `binding = ...`. Mirrors
/// [`gluon_model::Binding`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingTag {
    Cfg,
    CfgCumulative,
    Const,
    Build,
}
