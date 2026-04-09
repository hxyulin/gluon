use crate::source::SourceSpan;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Configuration option type tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfigType {
    Bool,
    /// Tristate: yes/module/no.
    Tristate,
    U32,
    U64,
    Str,
    /// Dedicated enum type with a fixed set of named variants.
    Choice,
    /// Ordered list of strings.
    List,
    /// Nested config group using flat dot-notation keys (e.g. `uart.baud`).
    Group,
}

/// Tristate value: yes, module, or no.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TristateVal {
    Yes,
    Module,
    No,
}

/// Values that a config option can hold. Note: `ConfigType::Group` is purely
/// structural — group-typed options never carry a value directly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfigValue {
    Bool(bool),
    Tristate(TristateVal),
    U32(u32),
    U64(u64),
    Str(String),
    /// Selected variant name for a `ConfigType::Choice` option.
    Choice(String),
    /// Ordered list of string items for a `ConfigType::List` option.
    List(Vec<String>),
}

impl Default for ConfigValue {
    fn default() -> Self {
        Self::Bool(false)
    }
}

/// How a config option maps to generated code or build flags.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Binding {
    /// Emit a `--cfg <prefix>_<name>` flag (boolean) or value form.
    Cfg,
    /// Emit `--cfg` for all values up to the configured one (ordered choices).
    CfgCumulative,
    /// Emit a `pub const NAME: Type = value;` in the generated config crate.
    Const,
    /// Available to gluon for crate-gating decisions (no codegen).
    Build,
}

/// A typed configuration option (Kconfig-style).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigOptionDef {
    /// Redundant with map key; kept for validation error messages.
    pub name: String,
    pub ty: ConfigType,
    pub default: ConfigValue,
    pub help: Option<String>,
    pub selects: Vec<String>,
    pub range: Option<(u64, u64)>,
    pub choices: Option<Vec<String>>,
    /// Menu category for TUI menuconfig grouping.
    pub menu: Option<String>,
    /// Code generation bindings for this option.
    pub bindings: Vec<Binding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
    /// Parsed boolean expression form of `depends_on`. Both declaration
    /// surfaces — the `.kconfig` loader and the Rhai `config_*` builder
    /// — populate this field; there is no other encoding. The resolver
    /// evaluates it semantically with full `&&` / `||` / `!` / grouping
    /// support. See [`Expr::eval`].
    ///
    /// `None` means "no `depends_on` was declared", which is distinct
    /// from `Some(Expr::And(vec![]))` (vacuously true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depends_on_expr: Option<Expr>,
    /// Parsed boolean expression form of `visible_if`. TUI-only today —
    /// validated for undeclared references but not evaluated by the
    /// resolver. Populated by the `.kconfig` loader (the Rhai surface
    /// has no equivalent method yet).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_if_expr: Option<Expr>,
}

/// A named configuration preset.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PresetDef {
    pub name: String,
    pub inherits: Option<String>,
    pub help: Option<String>,
    pub overrides: BTreeMap<String, ConfigValue>,
}

/// A boolean expression over config option identifiers.
///
/// This is the sole encoding for `depends_on` and `visible_if` clauses
/// in [`ConfigOptionDef`]. Both declaration surfaces — the `.kconfig`
/// loader (`A && !B`-style source grammar) and the Rhai `config_*`
/// builder (`.depends_on([A, B])` and `.depends_on_expr("A || B")`) —
/// lower into this enum before the resolver sees it, so there is
/// exactly one path through `config::resolve`. At resolve time an
/// `Ident` is "true" iff the referenced option is enabled (per the
/// `is_on` predicate).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Expr {
    /// Reference to another option by name. Evaluates to the value of
    /// that option's `is_on` predicate; missing options evaluate to false.
    Ident(String),
    /// Literal boolean. Parseable from `true` / `false` in `.kconfig`.
    Const(bool),
    Not(Box<Expr>),
    /// Conjunction. `And(vec![])` is vacuously true.
    And(Vec<Expr>),
    /// Disjunction. `Or(vec![])` is vacuously false.
    Or(Vec<Expr>),
}

impl Expr {
    /// Evaluate the expression against a lookup of "is this option on?".
    ///
    /// `lookup` returns `Some(true)` if the option is enabled,
    /// `Some(false)` if it is declared but off, and `None` if it is not
    /// declared at all. Missing options are treated as off — matching the
    /// conservative semantics of the flat `Vec<String>` depends path in
    /// `config::resolve`.
    pub fn eval<F>(&self, lookup: &F) -> bool
    where
        F: Fn(&str) -> Option<bool>,
    {
        match self {
            Expr::Ident(name) => lookup(name).unwrap_or(false),
            Expr::Const(b) => *b,
            Expr::Not(inner) => !inner.eval(lookup),
            Expr::And(xs) => xs.iter().all(|x| x.eval(lookup)),
            Expr::Or(xs) => xs.iter().any(|x| x.eval(lookup)),
        }
    }

    /// Collect every option identifier referenced anywhere in the
    /// expression tree, in left-to-right traversal order. Used by the
    /// loader's validation pass to check that every symbol a
    /// `depends_on` / `visible_if` mentions is actually declared.
    ///
    /// Duplicates are preserved — callers that want a set should
    /// deduplicate themselves.
    pub fn referenced_idents<'a>(&'a self, out: &mut Vec<&'a str>) {
        match self {
            Expr::Ident(name) => out.push(name.as_str()),
            Expr::Const(_) => {}
            Expr::Not(inner) => inner.referenced_idents(out),
            Expr::And(xs) | Expr::Or(xs) => {
                for x in xs {
                    x.referenced_idents(out);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a lookup closure from a slice of (name, on) pairs. Missing
    /// names return `None`, matching how resolver-state lookups behave.
    fn lookup_from(pairs: &[(&str, bool)]) -> impl Fn(&str) -> Option<bool> {
        let map: BTreeMap<String, bool> =
            pairs.iter().map(|(k, v)| ((*k).to_string(), *v)).collect();
        move |name: &str| map.get(name).copied()
    }

    #[test]
    fn expr_ident_reads_lookup() {
        let l = lookup_from(&[("A", true), ("B", false)]);
        assert!(Expr::Ident("A".into()).eval(&l));
        assert!(!Expr::Ident("B".into()).eval(&l));
        // Missing identifier is treated as off, not an error — this
        // matches the existing flat-Vec<String> depends_on semantics at
        // config::resolve::resolve where `.unwrap_or(true)` on a missing
        // option treats it as unsatisfied.
        assert!(!Expr::Ident("MISSING".into()).eval(&l));
    }

    #[test]
    fn expr_const_is_literal() {
        let l = lookup_from(&[]);
        assert!(Expr::Const(true).eval(&l));
        assert!(!Expr::Const(false).eval(&l));
    }

    #[test]
    fn expr_not_inverts() {
        let l = lookup_from(&[("A", true)]);
        assert!(!Expr::Not(Box::new(Expr::Ident("A".into()))).eval(&l));
        assert!(Expr::Not(Box::new(Expr::Ident("MISSING".into()))).eval(&l));
    }

    #[test]
    fn expr_and_short_circuits() {
        let l = lookup_from(&[("A", true), ("B", true), ("C", false)]);
        assert!(Expr::And(vec![Expr::Ident("A".into()), Expr::Ident("B".into())]).eval(&l));
        assert!(!Expr::And(vec![Expr::Ident("A".into()), Expr::Ident("C".into())]).eval(&l));
        // Empty AND is vacuously true — matches mathematical convention
        // and means `depends_on = []` imposes no constraint.
        assert!(Expr::And(vec![]).eval(&l));
    }

    #[test]
    fn expr_or_short_circuits() {
        let l = lookup_from(&[("A", false), ("B", true), ("C", false)]);
        assert!(Expr::Or(vec![Expr::Ident("A".into()), Expr::Ident("B".into())]).eval(&l));
        assert!(!Expr::Or(vec![Expr::Ident("A".into()), Expr::Ident("C".into())]).eval(&l));
        // Empty OR is vacuously false.
        assert!(!Expr::Or(vec![]).eval(&l));
    }

    #[test]
    fn expr_semantics_differ_from_flatten() {
        // This is the whole reason we went with true semantic evaluation
        // instead of hadron's flatten_symbols() approach: `A || B` and
        // `A && B` must produce different answers when only one of A/B
        // is on. A flatten-based implementation would see the same
        // {A, B} symbol set in both and resolve them identically.
        let l = lookup_from(&[("A", true), ("B", false)]);
        let a_or_b = Expr::Or(vec![Expr::Ident("A".into()), Expr::Ident("B".into())]);
        let a_and_b = Expr::And(vec![Expr::Ident("A".into()), Expr::Ident("B".into())]);
        assert!(a_or_b.eval(&l));
        assert!(!a_and_b.eval(&l));
    }

    #[test]
    fn expr_nested_not_and_or() {
        // !A && (B || C): with A=false, B=false, C=true → true.
        let l = lookup_from(&[("A", false), ("B", false), ("C", true)]);
        let e = Expr::And(vec![
            Expr::Not(Box::new(Expr::Ident("A".into()))),
            Expr::Or(vec![Expr::Ident("B".into()), Expr::Ident("C".into())]),
        ]);
        assert!(e.eval(&l));
    }

    #[test]
    fn referenced_idents_walks_tree() {
        // !A && (B || C) should yield [A, B, C] in traversal order.
        let e = Expr::And(vec![
            Expr::Not(Box::new(Expr::Ident("A".into()))),
            Expr::Or(vec![Expr::Ident("B".into()), Expr::Ident("C".into())]),
        ]);
        let mut out = Vec::new();
        e.referenced_idents(&mut out);
        assert_eq!(out, vec!["A", "B", "C"]);
    }

    #[test]
    fn referenced_idents_skips_const() {
        // Const literals contribute no identifiers.
        let e = Expr::Or(vec![Expr::Const(true), Expr::Ident("X".into())]);
        let mut out = Vec::new();
        e.referenced_idents(&mut out);
        assert_eq!(out, vec!["X"]);
    }

    #[test]
    fn config_option_def_defaults_to_empty_expr_fields() {
        // A freshly-constructed `ConfigOptionDef` has `None` for both
        // `depends_on_expr` and `visible_if_expr` — meaning "no clause
        // declared", as opposed to `Some(Expr::And(vec![]))` which is
        // vacuously true.
        let opt = ConfigOptionDef {
            name: "X".into(),
            ty: ConfigType::Bool,
            default: ConfigValue::Bool(false),
            help: None,
            selects: vec![],
            range: None,
            choices: None,
            menu: None,
            bindings: vec![],
            span: None,
            depends_on_expr: None,
            visible_if_expr: None,
        };
        assert!(opt.depends_on_expr.is_none());
        assert!(opt.visible_if_expr.is_none());
    }
}
