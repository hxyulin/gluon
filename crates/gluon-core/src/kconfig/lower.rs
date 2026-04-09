//! Lower a parsed [`ast::File`] into the `gluon-model` representation
//! consumed by the resolver.
//!
//! This layer mirrors the type-directed coercion done by
//! `engine::conversions::dynamic_to_config_value` but operates on
//! parser literals rather than Rhai `Dynamic` values, so there is no
//! Rhai dependency. Duplicates between the two paths (Rhai builder and
//! `.kconfig` loader) are intentional — the logic is compact, and
//! keeping them separate means the `.kconfig` loader compiles cleanly
//! without pulling in Rhai when used from an LSP or validator.
//!
//! # Passes
//!
//! 1. **Structural lowering.** Walk the AST, flattening nested menus
//!    into dot-path `menu` metadata, converting each `ConfigBlock` to a
//!    [`ConfigOptionDef`], and each `PresetBlock` to a [`PresetDef`].
//!    Type-check default values against the declared type and validate
//!    type-specific properties (e.g. `range` only applies to u32/u64,
//!    `choices` only to choice-typed options).
//! 2. **Cross-reference validation.** Now that the full set of declared
//!    option names is known, walk every `depends_on_expr` /
//!    `visible_if_expr` / `selects` and confirm every identifier refers
//!    to an option declared in this file. Unknown references become
//!    error diagnostics attached to the enclosing property's span.
//!
//! All diagnostics collected in either pass are returned together on
//! error. On success, the `(options, presets)` pair is returned.

use super::ast::{
    self, BindingTag, ConfigBlock, ConfigProp, Item, Literal, MenuBlock, PresetBlock, PresetProp,
    TypeTag,
};
use crate::error::Diagnostic;
use gluon_model::{
    Binding, ConfigOptionDef, ConfigType, ConfigValue, Expr, PresetDef, SourceSpan, TristateVal,
};
use std::collections::BTreeMap;

/// Result of lowering a single `.kconfig` file's AST into model types.
#[derive(Debug, Clone, Default)]
pub struct Lowered {
    pub options: BTreeMap<String, ConfigOptionDef>,
    pub presets: BTreeMap<String, PresetDef>,
}

/// Lower an AST file to the model representation.
///
/// On error, returns every diagnostic collected during the lowering
/// pass — structural errors (wrong default type, range on non-numeric,
/// etc.) and cross-reference errors (unknown option in `depends_on`)
/// are reported together.
pub fn lower(file: &ast::File) -> Result<Lowered, Vec<Diagnostic>> {
    let mut lw = Lowerer::default();
    lw.walk_items(&file.items, &[]);
    lw.validate_cross_references();
    if lw.diagnostics.is_empty() {
        Ok(Lowered {
            options: lw.options,
            presets: lw.presets,
        })
    } else {
        Err(lw.diagnostics)
    }
}

#[derive(Default)]
struct Lowerer {
    options: BTreeMap<String, ConfigOptionDef>,
    presets: BTreeMap<String, PresetDef>,
    diagnostics: Vec<Diagnostic>,
    /// Span where each option was first declared, kept in a side table
    /// (rather than only on `ConfigOptionDef`) so duplicate diagnostics
    /// can cite the prior declaration without poking at serde-skipped
    /// fields on the model type.
    declared_at: BTreeMap<String, SourceSpan>,
}

impl Lowerer {
    fn walk_items(&mut self, items: &[Item], menu_stack: &[&str]) {
        for item in items {
            match item {
                Item::Config(cb) => self.lower_config(cb, menu_stack),
                Item::Menu(mb) => self.lower_menu(mb, menu_stack),
                Item::Preset(pb) => self.lower_preset(pb),
                Item::Source(_) => {
                    // Source directives are resolved in the loader
                    // layer (chunk K7). A `source` item reaching the
                    // lowerer means the loader passed through an
                    // unexpanded file — not an error here, just a
                    // no-op. The loader walks its own tree separately.
                }
            }
        }
    }

    fn lower_menu(&mut self, mb: &MenuBlock, menu_stack: &[&str]) {
        let mut stack: Vec<&str> = menu_stack.to_vec();
        stack.push(mb.title.as_str());
        self.walk_items(&mb.items, &stack);
    }

    fn lower_config(&mut self, cb: &ConfigBlock, menu_stack: &[&str]) {
        // Duplicate check.
        if let Some(prior) = self.declared_at.get(&cb.name) {
            self.diagnostics.push(
                Diagnostic::error(format!(
                    "config option '{}' is declared more than once",
                    cb.name
                ))
                .with_span(cb.name_span.clone())
                .with_note(format!("previous declaration at {prior}")),
            );
            return;
        }

        let ty = map_type_tag(cb.ty);
        let mut opt = ConfigOptionDef {
            name: cb.name.clone(),
            ty,
            default: neutral_default(ty),
            help: None,
            depends_on: Vec::new(),
            selects: Vec::new(),
            range: None,
            choices: None,
            menu: if menu_stack.is_empty() {
                None
            } else {
                Some(menu_stack.join("."))
            },
            bindings: Vec::new(),
            visible_if: Vec::new(),
            span: Some(cb.span.clone()),
            depends_on_expr: None,
            visible_if_expr: None,
        };

        // Apply each property with type-directed validation.
        for prop in &cb.props {
            self.apply_prop(&mut opt, prop);
        }

        // If it's a Choice and no variants were declared, that is an
        // error (choice with no variants is meaningless). Same for an
        // empty `choices` list.
        if ty == ConfigType::Choice {
            match &opt.choices {
                None => {
                    self.diagnostics.push(
                        Diagnostic::error(format!(
                            "choice option '{}' declares no variants — add 'choices = [...]'",
                            cb.name
                        ))
                        .with_span(cb.span.clone()),
                    );
                }
                Some(v) if v.is_empty() => {
                    self.diagnostics.push(
                        Diagnostic::error(format!(
                            "choice option '{}' has an empty choices list",
                            cb.name
                        ))
                        .with_span(cb.span.clone()),
                    );
                }
                _ => {}
            }
        }

        self.declared_at
            .insert(cb.name.clone(), cb.name_span.clone());
        self.options.insert(cb.name.clone(), opt);
    }

    fn apply_prop(&mut self, opt: &mut ConfigOptionDef, prop: &ConfigProp) {
        match prop {
            ConfigProp::Default { value, span } => {
                match literal_to_config_value(&opt.name, opt.ty, value, span.clone()) {
                    Ok(v) => opt.default = v,
                    Err(d) => self.diagnostics.push(d),
                }
            }
            ConfigProp::Help { text, .. } => {
                opt.help = Some(text.clone());
            }
            ConfigProp::Range {
                low,
                high,
                inclusive,
                span,
            } => {
                if !matches!(opt.ty, ConfigType::U32 | ConfigType::U64) {
                    self.diagnostics.push(
                        Diagnostic::error(format!(
                            "'range' only applies to u32 or u64 options, but '{}' is {:?}",
                            opt.name, opt.ty
                        ))
                        .with_span(span.clone()),
                    );
                    return;
                }
                // Normalize exclusive `a..b` to inclusive `a..=b-1` so
                // the existing resolver range check (which is inclusive
                // on both ends) sees a single shape. The inclusivity
                // flag is purely a parser-level nicety.
                let high_incl = if *inclusive {
                    *high
                } else if *high == 0 {
                    // Degenerate: `0..0` is empty — nothing satisfies it.
                    self.diagnostics.push(
                        Diagnostic::error(format!(
                            "empty exclusive range on option '{}'",
                            opt.name
                        ))
                        .with_span(span.clone()),
                    );
                    return;
                } else {
                    *high - 1
                };
                opt.range = Some((*low, high_incl));
            }
            ConfigProp::Choices { variants, span } => {
                if opt.ty != ConfigType::Choice {
                    self.diagnostics.push(
                        Diagnostic::error(format!(
                            "'choices' only applies to choice-typed options, but '{}' is {:?}",
                            opt.name, opt.ty
                        ))
                        .with_span(span.clone()),
                    );
                    return;
                }
                opt.choices = Some(variants.clone());
            }
            ConfigProp::MenuLabel { label, .. } => {
                // A per-option menu label overrides any enclosing
                // `menu { ... }` stack. This matches the Rhai builder's
                // `.menu("X")` behavior where the user wins if they
                // explicitly set a label.
                opt.menu = Some(label.clone());
            }
            ConfigProp::Binding { tag, .. } => {
                opt.bindings.push(map_binding(*tag));
            }
            ConfigProp::DependsOn { expr, .. } => {
                // Populate both the expression form (for the resolver
                // to evaluate semantically) and the flat Vec<String>
                // (for any consumer that still walks the old form).
                let mut idents = Vec::new();
                expr.referenced_idents(&mut idents);
                opt.depends_on = idents.into_iter().map(|s| s.to_string()).collect();
                opt.depends_on_expr = Some(expr.clone());
            }
            ConfigProp::VisibleIf { expr, .. } => {
                let mut idents = Vec::new();
                expr.referenced_idents(&mut idents);
                opt.visible_if = idents.into_iter().map(|s| s.to_string()).collect();
                opt.visible_if_expr = Some(expr.clone());
            }
            ConfigProp::Selects { names, .. } => {
                opt.selects = names.clone();
            }
        }
    }

    fn lower_preset(&mut self, pb: &PresetBlock) {
        if self.presets.contains_key(&pb.name) {
            // PresetDef does not carry a span, so we cannot cite the
            // prior declaration here. Pointing at the duplicate is
            // still actionable; the user can grep for the name.
            self.diagnostics.push(
                Diagnostic::error(format!("preset '{}' is declared more than once", pb.name))
                    .with_span(pb.name_span.clone()),
            );
            return;
        }

        let mut preset = PresetDef {
            name: pb.name.clone(),
            inherits: None,
            help: None,
            overrides: BTreeMap::new(),
        };

        for prop in &pb.props {
            match prop {
                PresetProp::Inherits { parent, .. } => {
                    preset.inherits = Some(parent.clone());
                }
                PresetProp::Help { text, .. } => {
                    preset.help = Some(text.clone());
                }
                PresetProp::Override {
                    option,
                    value,
                    span,
                } => {
                    // At this point the target option may not yet be
                    // declared (it could live in a later `source`-d
                    // file). Use the best-effort converter: we don't
                    // know the expected type, so coerce as generously
                    // as possible and defer strict type checking to
                    // the resolver's preset-application pass, which
                    // already handles type mismatches via
                    // `dynamic_to_config_value_best_effort`.
                    let value = literal_to_config_value_best_effort(value);
                    match value {
                        Some(v) => {
                            preset.overrides.insert(option.clone(), v);
                        }
                        None => {
                            self.diagnostics.push(
                                Diagnostic::error(format!(
                                    "preset '{}': could not interpret value for option '{}'",
                                    pb.name, option
                                ))
                                .with_span(span.clone()),
                            );
                        }
                    }
                }
            }
        }

        self.presets.insert(pb.name.clone(), preset);
    }

    /// Walk every expression and `selects` list on every declared
    /// option and confirm each referenced name is the name of a
    /// declared option (in *this* file — cross-file references are the
    /// loader's responsibility in chunk K7 once it merges per-file
    /// results).
    fn validate_cross_references(&mut self) {
        // Collect owned copies up-front so the subsequent mutation of
        // `self.diagnostics` does not conflict with an iterator
        // borrowing `self.options`.
        let declared: std::collections::BTreeSet<String> = self.options.keys().cloned().collect();

        // Snapshot of the per-option fields needed by the validator.
        // Cloning avoids a borrow-checker fight when pushing diagnostics
        // back into `self`. The set of options is small (one per declared
        // config) so the clone cost is negligible.
        struct OptionSnapshot {
            name: String,
            depends_on_expr: Option<Expr>,
            visible_if_expr: Option<Expr>,
            selects: Vec<String>,
            span: Option<SourceSpan>,
        }
        let options: Vec<OptionSnapshot> = self
            .options
            .iter()
            .map(|(name, opt)| OptionSnapshot {
                name: name.clone(),
                depends_on_expr: opt.depends_on_expr.clone(),
                visible_if_expr: opt.visible_if_expr.clone(),
                selects: opt.selects.clone(),
                span: opt.span.clone(),
            })
            .collect();

        for OptionSnapshot {
            name,
            depends_on_expr: dep,
            visible_if_expr: vis,
            selects,
            span,
        } in options
        {
            if let Some(expr) = dep {
                let mut refs = Vec::new();
                expr.referenced_idents(&mut refs);
                for r in refs {
                    if !declared.contains(r) {
                        self.push_unknown_ref(&name, "depends_on", r, span.clone());
                    }
                }
            }
            if let Some(expr) = vis {
                let mut refs = Vec::new();
                expr.referenced_idents(&mut refs);
                for r in refs {
                    if !declared.contains(r) {
                        self.push_unknown_ref(&name, "visible_if", r, span.clone());
                    }
                }
            }
            for sel in &selects {
                if !declared.contains(sel.as_str()) {
                    self.push_unknown_ref(&name, "selects", sel, span.clone());
                }
            }
        }
    }

    fn push_unknown_ref(
        &mut self,
        option: &str,
        prop: &str,
        referenced: &str,
        span: Option<SourceSpan>,
    ) {
        let mut d = Diagnostic::error(format!(
            "option '{option}' references undeclared option '{referenced}' in '{prop}'"
        ));
        if let Some(s) = span {
            d = d.with_span(s);
        }
        self.diagnostics.push(d);
    }
}

fn map_type_tag(tag: TypeTag) -> ConfigType {
    match tag {
        TypeTag::Bool => ConfigType::Bool,
        TypeTag::Tristate => ConfigType::Tristate,
        TypeTag::U32 => ConfigType::U32,
        TypeTag::U64 => ConfigType::U64,
        TypeTag::Str => ConfigType::Str,
        TypeTag::Choice => ConfigType::Choice,
        TypeTag::List => ConfigType::List,
        TypeTag::Group => ConfigType::Group,
    }
}

fn map_binding(tag: BindingTag) -> Binding {
    match tag {
        BindingTag::Cfg => Binding::Cfg,
        BindingTag::CfgCumulative => Binding::CfgCumulative,
        BindingTag::Const => Binding::Const,
        BindingTag::Build => Binding::Build,
    }
}

fn neutral_default(ty: ConfigType) -> ConfigValue {
    match ty {
        ConfigType::Bool => ConfigValue::Bool(false),
        ConfigType::Tristate => ConfigValue::Tristate(TristateVal::No),
        ConfigType::U32 => ConfigValue::U32(0),
        ConfigType::U64 => ConfigValue::U64(0),
        ConfigType::Str => ConfigValue::Str(String::new()),
        ConfigType::Choice => ConfigValue::Choice(String::new()),
        ConfigType::List => ConfigValue::List(Vec::new()),
        // Group-typed options never carry a value; match the Rhai
        // builder's neutral placeholder at
        // `engine/builders/config.rs:default_value_for`.
        ConfigType::Group => ConfigValue::Bool(false),
    }
}

/// Type-directed conversion from a parser literal to a model
/// [`ConfigValue`]. Mirrors the Rhai-side
/// `dynamic_to_config_value` but takes a plain AST node.
fn literal_to_config_value(
    option_name: &str,
    expected: ConfigType,
    lit: &Literal,
    span: SourceSpan,
) -> Result<ConfigValue, Diagnostic> {
    let mismatch = |expected_str: &str, found: &str| {
        Diagnostic::error(format!(
            "config option '{option_name}': expected {expected_str} default, found {found}"
        ))
        .with_span(span.clone())
    };

    match (expected, lit) {
        (ConfigType::Bool, Literal::Bool(b)) => Ok(ConfigValue::Bool(*b)),
        (ConfigType::Bool, other) => Err(mismatch("bool", describe_literal(other))),

        (ConfigType::U32, Literal::Int(i)) => {
            if *i <= u32::MAX as u64 {
                Ok(ConfigValue::U32(*i as u32))
            } else {
                Err(Diagnostic::error(format!(
                    "config option '{option_name}': value {i} out of range for u32"
                ))
                .with_span(span))
            }
        }
        (ConfigType::U32, other) => Err(mismatch("u32 integer", describe_literal(other))),

        (ConfigType::U64, Literal::Int(i)) => Ok(ConfigValue::U64(*i)),
        (ConfigType::U64, other) => Err(mismatch("u64 integer", describe_literal(other))),

        (ConfigType::Str, Literal::String(s)) => Ok(ConfigValue::Str(s.clone())),
        (ConfigType::Str, other) => Err(mismatch("string", describe_literal(other))),

        (ConfigType::Choice, Literal::String(s)) => Ok(ConfigValue::Choice(s.clone())),
        (ConfigType::Choice, Literal::Ident(s)) => Ok(ConfigValue::Choice(s.clone())),
        (ConfigType::Choice, other) => Err(mismatch(
            "string or identifier (choice variant)",
            describe_literal(other),
        )),

        (ConfigType::Tristate, Literal::Ident(s)) | (ConfigType::Tristate, Literal::String(s)) => {
            match s.as_str() {
                "y" | "yes" => Ok(ConfigValue::Tristate(TristateVal::Yes)),
                "n" | "no" => Ok(ConfigValue::Tristate(TristateVal::No)),
                "m" | "module" => Ok(ConfigValue::Tristate(TristateVal::Module)),
                _ => Err(Diagnostic::error(format!(
                    "config option '{option_name}': tristate must be one of 'y', 'n', 'm' (or yes/no/module), got '{s}'"
                ))
                .with_span(span)),
            }
        }
        (ConfigType::Tristate, other) => {
            Err(mismatch("tristate (y/n/m)", describe_literal(other)))
        }

        // List defaults aren't representable in our literal grammar;
        // users who need a non-empty list default can declare it from
        // Rhai instead. Keep the error message actionable.
        (ConfigType::List, _) => Err(Diagnostic::error(format!(
            "config option '{option_name}': default values for list-typed options are not \
             supported in .kconfig — declare the default from gluon.rhai instead"
        ))
        .with_span(span)),

        (ConfigType::Group, _) => Err(Diagnostic::error(format!(
            "config option '{option_name}': group-typed options cannot carry a default value"
        ))
        .with_span(span)),
    }
}

/// Best-effort literal-to-ConfigValue conversion for preset overrides,
/// where the expected type is unknown at parse time.
///
/// Mirrors the intent of
/// `conversions::dynamic_to_config_value_best_effort`: pick the most
/// natural model form for the given literal shape and let the resolver
/// apply strict type checking when the preset is actually merged.
fn literal_to_config_value_best_effort(lit: &Literal) -> Option<ConfigValue> {
    match lit {
        Literal::Bool(b) => Some(ConfigValue::Bool(*b)),
        Literal::Int(i) => Some(ConfigValue::U64(*i)),
        Literal::String(s) => Some(ConfigValue::Str(s.clone())),
        // A bare identifier in a preset override is most likely a
        // choice variant name or a tristate keyword. Both are stored as
        // strings in the ConfigValue shape, so `Str` is a safe carrier
        // that the resolver can downcast later.
        Literal::Ident(s) => Some(ConfigValue::Str(s.clone())),
    }
}

fn describe_literal(lit: &Literal) -> &'static str {
    match lit {
        Literal::Bool(_) => "bool",
        Literal::Int(_) => "integer",
        Literal::String(_) => "string",
        Literal::Ident(_) => "identifier",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kconfig::lexer::lex;
    use crate::kconfig::parser::parse;
    use std::path::Path;

    fn lower_ok(src: &str) -> Lowered {
        let toks = lex(src, Path::new("t.kconfig")).expect("lex ok");
        let ast = parse(&toks).expect("parse ok");
        lower(&ast).unwrap_or_else(|diags| {
            panic!(
                "expected clean lower, got: {:#?}",
                diags.iter().map(|d| &d.message).collect::<Vec<_>>()
            )
        })
    }

    fn lower_err(src: &str) -> Vec<Diagnostic> {
        let toks = lex(src, Path::new("t.kconfig")).expect("lex ok");
        let ast = parse(&toks).expect("parse ok");
        lower(&ast).expect_err("expected lowering error")
    }

    #[test]
    fn lowers_minimal_bool() {
        let lw = lower_ok("config X: bool { default = true }");
        let opt = lw.options.get("X").expect("option X");
        assert_eq!(opt.ty, ConfigType::Bool);
        assert_eq!(opt.default, ConfigValue::Bool(true));
        assert!(opt.depends_on_expr.is_none());
        assert!(opt.menu.is_none());
    }

    #[test]
    fn default_type_mismatch_reports_error() {
        let diags = lower_err(r#"config X: bool { default = 42 }"#);
        assert!(diags.iter().any(|d| d.message.contains("expected bool")));
    }

    #[test]
    fn u32_range_inclusive_is_stored_as_inclusive() {
        let lw = lower_ok("config X: u32 { range = 0..=100 }");
        let opt = lw.options.get("X").unwrap();
        assert_eq!(opt.range, Some((0, 100)));
    }

    #[test]
    fn u32_range_exclusive_is_normalized_to_inclusive() {
        // `0..10` has 10 elements 0..=9 — should normalize to (0, 9).
        let lw = lower_ok("config X: u32 { range = 0..10 }");
        let opt = lw.options.get("X").unwrap();
        assert_eq!(opt.range, Some((0, 9)));
    }

    #[test]
    fn range_on_non_numeric_rejected() {
        let diags = lower_err("config X: bool { range = 0..=5 }");
        assert!(diags.iter().any(|d| d.message.contains("'range'")));
    }

    #[test]
    fn choice_requires_variants() {
        let diags = lower_err("config X: choice { }");
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("declares no variants"))
        );
    }

    #[test]
    fn choice_with_variants_lowers() {
        let lw = lower_ok(r#"config MODE: choice { choices = ["debug", "release"] }"#);
        let opt = lw.options.get("MODE").unwrap();
        assert_eq!(
            opt.choices,
            Some(vec!["debug".to_string(), "release".into()])
        );
    }

    #[test]
    fn duplicate_config_name_rejected() {
        let diags = lower_err(
            r#"
            config X: bool {}
            config X: u32 {}
        "#,
        );
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("declared more than once"))
        );
    }

    #[test]
    fn menu_stack_flattens_to_dot_path() {
        let lw = lower_ok(
            r#"
            menu "Outer" {
                menu "Inner" {
                    config A: bool {}
                }
                config B: bool {}
            }
            config C: bool {}
        "#,
        );
        assert_eq!(
            lw.options.get("A").unwrap().menu.as_deref(),
            Some("Outer.Inner")
        );
        assert_eq!(lw.options.get("B").unwrap().menu.as_deref(), Some("Outer"));
        assert_eq!(lw.options.get("C").unwrap().menu, None);
    }

    #[test]
    fn explicit_menu_prop_overrides_menu_stack() {
        let lw = lower_ok(
            r#"
            menu "Outer" {
                config A: bool { menu = "Custom" }
            }
        "#,
        );
        assert_eq!(lw.options.get("A").unwrap().menu.as_deref(), Some("Custom"));
    }

    #[test]
    fn depends_on_expr_populates_both_flat_and_expr_forms() {
        let lw = lower_ok(
            r#"
            config A: bool {}
            config B: bool {}
            config X: bool { depends_on = A && !B }
        "#,
        );
        let opt = lw.options.get("X").unwrap();
        assert!(opt.depends_on_expr.is_some());
        // Flat form should contain both referenced idents.
        let mut flat = opt.depends_on.clone();
        flat.sort();
        assert_eq!(flat, vec!["A", "B"]);
    }

    #[test]
    fn depends_on_unknown_ident_rejected() {
        let diags = lower_err(
            r#"
            config X: bool { depends_on = MISSING }
        "#,
        );
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("undeclared option 'MISSING'"))
        );
    }

    #[test]
    fn selects_unknown_ident_rejected() {
        let diags = lower_err(
            r#"
            config X: bool { selects = [MISSING] }
        "#,
        );
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("undeclared option 'MISSING'"))
        );
    }

    #[test]
    fn tristate_accepts_yes_no_module() {
        let lw = lower_ok(
            r#"
            config A: tristate { default = yes }
            config B: tristate { default = n }
            config C: tristate { default = module }
        "#,
        );
        assert_eq!(
            lw.options.get("A").unwrap().default,
            ConfigValue::Tristate(TristateVal::Yes)
        );
        assert_eq!(
            lw.options.get("B").unwrap().default,
            ConfigValue::Tristate(TristateVal::No)
        );
        assert_eq!(
            lw.options.get("C").unwrap().default,
            ConfigValue::Tristate(TristateVal::Module)
        );
    }

    #[test]
    fn tristate_rejects_bogus_value() {
        let diags = lower_err("config A: tristate { default = maybe }");
        assert!(diags.iter().any(|d| d.message.contains("tristate")));
    }

    #[test]
    fn bindings_accumulate_in_order() {
        let lw = lower_ok(
            r#"
            config X: bool {
                binding = cfg
                binding = const
                binding = build
            }
        "#,
        );
        assert_eq!(
            lw.options.get("X").unwrap().bindings,
            vec![Binding::Cfg, Binding::Const, Binding::Build]
        );
    }

    #[test]
    fn preset_with_inherits_and_overrides_lowers() {
        let lw = lower_ok(
            r#"
            config DEBUG_LOG: bool {}
            config LOG_LEVEL: u32 {}
            preset "dev" {
                inherits = "base"
                help = "Developer defaults"
                DEBUG_LOG = true
                LOG_LEVEL = 4
            }
        "#,
        );
        let p = lw.presets.get("dev").expect("preset dev");
        assert_eq!(p.inherits.as_deref(), Some("base"));
        assert_eq!(p.help.as_deref(), Some("Developer defaults"));
        assert_eq!(p.overrides.len(), 2);
        assert_eq!(p.overrides.get("DEBUG_LOG"), Some(&ConfigValue::Bool(true)));
        assert_eq!(p.overrides.get("LOG_LEVEL"), Some(&ConfigValue::U64(4)));
    }

    #[test]
    fn realistic_file_lowers_cleanly() {
        let lw = lower_ok(
            r#"
            menu "Logging" {
                config LOG_ENABLED: bool { default = true }
                config LOG_LEVEL: u32 {
                    default = 3
                    range = 0..=5
                    depends_on = LOG_ENABLED
                }
            }
            config DEBUG: bool {
                default = false
                visible_if = LOG_ENABLED && !LOG_LEVEL
                selects = [LOG_ENABLED]
            }
            preset "verbose" {
                LOG_LEVEL = 5
                DEBUG = true
            }
        "#,
        );
        assert_eq!(lw.options.len(), 3);
        assert_eq!(lw.presets.len(), 1);
        let debug = lw.options.get("DEBUG").unwrap();
        assert!(debug.visible_if_expr.is_some());
        assert_eq!(debug.selects, vec!["LOG_ENABLED"]);
    }
}
