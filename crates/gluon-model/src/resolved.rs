use crate::build_model::{CrateDef, ProjectDef, TargetDef};
use crate::handle::Handle;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// A value for a config option resolved to its final typed form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ResolvedValue {
    Bool(bool),
    Tristate(crate::kconfig::TristateVal),
    U32(u32),
    U64(u64),
    String(String),
    Choice(String),
    List(Vec<String>),
}

/// A profile after inheritance has been applied.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedProfile {
    pub name: String,
    pub target: Handle<TargetDef>,
    pub opt_level: u8,
    pub debug_info: bool,
    pub lto: Option<String>,
    pub boot_binary: Option<Handle<CrateDef>>,
    // Leave profile extras minimal — later chunks add qemu/preset/etc. when they need them.
}

/// A crate selected for inclusion in the build, with resolved target binding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedCrateRef {
    pub handle: Handle<CrateDef>,
    /// Always equals the crate's own `target_handle` after resolution; carried
    /// here so the scheduler can avoid the indirection. For host crates this
    /// is a placeholder (the resolved target of the active profile) and the
    /// `host` flag should be consulted instead.
    pub target: Handle<TargetDef>,
    /// True when this crate's group targets the literal `"host"` triple.
    /// Host crates are built for the build machine and are not subject to
    /// the profile's target.
    #[serde(default)]
    pub host: bool,
}

/// A fully-resolved build configuration, consumed by scheduler/compile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedConfig {
    pub project: ProjectDef,
    pub profile: ResolvedProfile,
    pub options: BTreeMap<String, ResolvedValue>,
    pub crates: Vec<ResolvedCrateRef>,
    pub build_dir: PathBuf,
    /// Absolute path to the project root (the directory containing
    /// `gluon.rhai`). Used by `compile_crate` to resolve `CrateDef::path`
    /// and `CrateDef::linker_script` against the project tree.
    pub project_root: PathBuf,
}
