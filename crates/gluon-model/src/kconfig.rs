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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub depends_on: Vec<String>,
    pub selects: Vec<String>,
    pub range: Option<(u64, u64)>,
    pub choices: Option<Vec<String>>,
    /// Menu category for TUI menuconfig grouping.
    pub menu: Option<String>,
    /// Code generation bindings for this option.
    pub bindings: Vec<Binding>,
    /// Symbols that must be enabled for this option to be visible in the TUI.
    #[serde(default)]
    pub visible_if: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

/// A named configuration preset.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PresetDef {
    pub name: String,
    pub inherits: Option<String>,
    pub help: Option<String>,
    pub overrides: BTreeMap<String, ConfigValue>,
}
