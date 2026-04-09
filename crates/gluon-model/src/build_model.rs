use crate::handle::{Arena, Handle, Named};
use crate::kconfig::{ConfigOptionDef, ConfigValue, PresetDef};
use crate::source::SourceSpan;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// The complete build model produced by evaluating `gluon.rhai`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct BuildModel {
    pub project: Option<ProjectDef>,
    pub targets: Arena<TargetDef>,
    pub profiles: Arena<ProfileDef>,
    pub groups: Arena<GroupDef>,
    pub crates: Arena<CrateDef>,
    pub rules: Arena<RuleDef>,
    pub pipelines: Arena<PipelineDef>,
    pub external_deps: Arena<ExternalDepDef>,
    /// Keyed by option NAME (e.g. `"CONFIG_FOO"`); options are not in an arena
    /// because every reference to them is by string identifier.
    pub config_options: BTreeMap<String, ConfigOptionDef>,
    pub presets: BTreeMap<String, PresetDef>,
    /// Stub bootloader configuration; details land in a later sub-project.
    #[serde(default)]
    pub bootloader: BootloaderDef,
    /// Stub image configuration; details land in a later sub-project.
    #[serde(default)]
    pub image: ImageDef,
    /// Stub QEMU configuration; details land in a later sub-project.
    #[serde(default)]
    pub qemu: QemuDef,
}

/// Project metadata.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ProjectDef {
    pub name: String,
    pub version: String,
    /// Override for the generated config crate's name. Resolved to
    /// `<project>_config` at resolve time when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_crate_name: Option<String>,
    /// Override for the cfg flag prefix. Resolved to a sanitised `<project>`
    /// at resolve time when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cfg_prefix: Option<String>,
    /// Override for the per-developer config file path. Resolved to
    /// `.gluon-config` at resolve time when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_override_file: Option<PathBuf>,
    /// Name of the profile to use when the user did not pass
    /// `-p/--profile` on the command line. When `None`, the CLI falls
    /// back to the first profile in alphabetical order — which is a
    /// footgun for projects with `debug`/`dev`/`release` profiles
    /// because it silently picks `debug`. Setting this makes the
    /// intent explicit and stable across fresh clones.
    ///
    /// Validated at intern time: the named profile must exist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_profile: Option<String>,
}

/// A compilation target definition.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TargetDef {
    pub name: String,
    pub spec: String,
    /// If true, `spec` is a rustc builtin triple (not a file path).
    #[serde(default)]
    pub builtin: bool,
    /// The panic strategy rustc should use for this target. When `Some`,
    /// `-C panic=<strategy>` is passed to every rustc invocation that
    /// builds a crate for this target — including sysroot crates.
    ///
    /// Bare-metal targets almost always want `Some("abort")`. Mixing panic
    /// strategies across sysroot rlibs and downstream crates fails at link
    /// time with `error: the crate ... is compiled with a different panic
    /// strategy`, so this must be consistent across all crates for a target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panic_strategy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

/// Crate output type.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CrateType {
    #[default]
    Lib,
    Bin,
    ProcMacro,
    #[serde(rename = "staticlib")]
    StaticLib,
}

impl CrateType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Lib => "lib",
            Self::Bin => "bin",
            Self::ProcMacro => "proc-macro",
            Self::StaticLib => "staticlib",
        }
    }
}

/// A build profile definition.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileDef {
    pub name: String,
    pub inherits: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherits_handle: Option<Handle<ProfileDef>>,
    /// `None` when the profile inherits a target from its parent via `inherits_handle`.
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_handle: Option<Handle<TargetDef>>,
    pub opt_level: Option<u8>,
    pub debug_info: Option<bool>,
    pub lto: Option<String>,
    pub boot_binary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_binary_handle: Option<Handle<CrateDef>>,
    /// Named preset to apply.
    pub preset: Option<String>,
    pub config: BTreeMap<String, ConfigValue>,
    pub qemu_memory: Option<u32>,
    pub qemu_cores: Option<u32>,
    pub qemu_extra_args: Option<Vec<String>>,
    pub test_timeout: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

/// A group of crates with shared compilation behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupDef {
    pub name: String,
    /// Target for all crates in this group. `"host"` = host triple.
    /// Required at the group level; groups always pin a target triple or `"host"`.
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_handle: Option<Handle<TargetDef>>,
    pub default_edition: String,
    pub crates: Vec<String>,
    pub shared_flags: Vec<String>,
    /// Whether crates in this group are project crates (for clippy linting).
    pub is_project: bool,
    /// Whether crates in this group should be linked with the config crate.
    pub config: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

impl Default for GroupDef {
    fn default() -> Self {
        Self {
            name: String::new(),
            target: "host".into(),
            target_handle: None,
            default_edition: "2024".into(),
            crates: Vec::new(),
            shared_flags: Vec::new(),
            is_project: true,
            config: false,
            span: None,
        }
    }
}

/// A crate definition.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CrateDef {
    pub name: String,
    pub path: String,
    pub edition: String,
    pub crate_type: CrateType,
    /// Target for this crate (inherited from group). `"host"` = host triple.
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_handle: Option<Handle<TargetDef>>,
    pub deps: BTreeMap<String, DepDef>,
    pub dev_deps: BTreeMap<String, DepDef>,
    pub features: Vec<String>,
    pub root: Option<String>,
    /// Per-crate linker script.
    pub linker_script: Option<String>,
    /// The group this crate belongs to.
    pub group: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_handle: Option<Handle<GroupDef>>,
    /// Whether this crate is a project crate (for clippy linting).
    pub is_project_crate: bool,
    /// Extra `--cfg` flags for this crate.
    pub cfg_flags: Vec<String>,
    /// Extra `rustc` flags for this crate.
    pub rustc_flags: Vec<String>,
    /// Config options that must be enabled for this crate to be compiled.
    pub requires_config: Vec<String>,
    /// Ordering-only dependencies on other crates. Creates DAG edges
    /// without `--extern` flags.
    #[serde(default)]
    pub artifact_deps: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

/// A dependency specification within a crate definition.
///
/// This is the strict single-form representation. The map key on
/// `CrateDef::deps` is the extern name; this struct holds the referenced
/// crate name and resolution metadata.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DepDef {
    pub crate_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crate_handle: Option<Handle<CrateDef>>,
    pub features: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

/// Source location for an external dependency.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DepSource {
    /// crates.io with exact version.
    CratesIo { version: String },
    /// Git repository.
    Git { url: String, reference: GitRef },
    /// Local path (not vendored, used in-place).
    Path { path: String },
}

/// Git reference type for git-sourced dependencies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GitRef {
    Rev(String),
    Tag(String),
    Branch(String),
    Default,
}

/// An external dependency declaration from `gluon.rhai`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalDepDef {
    pub name: String,
    pub source: DepSource,
    pub features: Vec<String>,
    pub default_features: bool,
    /// Extra `--cfg` flags to pass when compiling this dependency.
    pub cfg_flags: Vec<String>,
    /// Extra `rustc` flags to pass when compiling this dependency.
    pub rustc_flags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

/// A rule for custom artifact generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleDef {
    pub name: String,
    /// Inputs are dispatched by string at runtime; no handles.
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub depends_on: Vec<String>,
    pub handler: RuleHandler,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

/// How a rule's artifact is generated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RuleHandler {
    /// A built-in Rust function identified by name.
    Builtin(String),
    /// Script source code for user-defined rule callbacks.
    Script(String),
}

/// A build pipeline definition.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PipelineDef {
    pub name: String,
    pub stages: Vec<PipelineStep>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

/// A single step in a build pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineStep {
    pub name: String,
    /// String references to group names.
    pub inputs: Vec<String>,
    /// Sibling resolved handles, populated by the intern pass.
    #[serde(default)]
    pub inputs_handles: Vec<Option<Handle<GroupDef>>>,
    /// Optional rule to run for this stage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
}

/// Stub bootloader configuration. Will be expanded in a later sub-project.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BootloaderDef {
    pub kind: String,
    #[serde(default)]
    pub extras: BTreeMap<String, String>,
}

/// Stub image configuration. Will be expanded in a later sub-project.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImageDef {
    #[serde(default)]
    pub extras: BTreeMap<String, String>,
}

/// QEMU configuration for `gluon run` (and future `gluon test`).
///
/// All fields are optional at the model level — defaults are filled in by
/// [`resolve_qemu`](../../gluon_core/run/resolve/fn.resolve_qemu.html) at
/// runtime. Profile-level overrides (`ProfileDef::qemu_memory`,
/// `qemu_cores`, `qemu_extra_args`, `test_timeout`) take precedence over
/// the values here.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QemuDef {
    /// QEMU binary to invoke (e.g. `"qemu-system-x86_64"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
    /// Machine type (`-machine <...>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    /// Memory in MiB (`-m <...>M`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u32>,
    /// Core count (`-smp <...>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cores: Option<u32>,
    /// Serial policy. `Stdio` is the default applied at resolve time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<SerialMode>,
    /// Extra QEMU arguments appended after every gluon-managed flag.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
    /// Default boot mode for this profile. `None` means direct kernel boot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_mode: Option<BootMode>,
    /// Explicit OVMF CODE firmware path. Overrides env/system fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ovmf_code: Option<PathBuf>,
    /// Explicit OVMF VARS firmware path. Overrides env/system fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ovmf_vars: Option<PathBuf>,
    /// EFI System Partition source. Mutually exclusive variants.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub esp: Option<EspSource>,
    /// `isa-debug-exit` I/O port (for the future test harness). Defaults
    /// to `0xf4` at resolve time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_exit_port: Option<u16>,
    /// Success exit code written by the kernel to `test_exit_port`.
    /// Defaults to `0x10` at resolve time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_success_code: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

/// Boot method selected for `gluon run`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BootMode {
    /// Direct kernel boot via `-kernel <path>`. QEMU parses the ELF
    /// and jumps to its entry point. Works for any kernel QEMU's
    /// direct loader understands (multiboot, ELF64 with a plain
    /// entry point, etc.).
    Direct,
    /// UEFI boot via OVMF pflash firmware. An optional ESP source
    /// provides the bootable `EFI/BOOT/BOOTX64.EFI` (or equivalent).
    Uefi,
}

/// Serial output policy for QEMU.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SerialMode {
    /// Forward QEMU's first serial port to the host stdio (default).
    Stdio,
    /// Disable serial (`-serial none`).
    None,
    /// Write serial output to a file (`-serial file:<path>`).
    File(PathBuf),
}

/// Source for the EFI System Partition in UEFI boot mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EspSource {
    /// Local directory mounted via QEMU's VVFAT driver
    /// (`-drive format=raw,file=fat:rw:<dir>`).
    Dir(PathBuf),
    /// Pre-built raw disk image
    /// (`-drive format=raw,file=<img>`).
    Image(PathBuf),
}

impl Named for TargetDef {
    fn name(&self) -> &str {
        &self.name
    }
}

impl Named for ProfileDef {
    fn name(&self) -> &str {
        &self.name
    }
}

impl Named for GroupDef {
    fn name(&self) -> &str {
        &self.name
    }
}

impl Named for CrateDef {
    fn name(&self) -> &str {
        &self.name
    }
}

impl Named for RuleDef {
    fn name(&self) -> &str {
        &self.name
    }
}

impl Named for PipelineDef {
    fn name(&self) -> &str {
        &self.name
    }
}

impl Named for ExternalDepDef {
    fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_build_model_is_empty() {
        let m = BuildModel::default();
        assert!(m.project.is_none());
        assert!(m.targets.is_empty());
        assert!(m.profiles.is_empty());
        assert!(m.groups.is_empty());
        assert!(m.crates.is_empty());
        assert!(m.rules.is_empty());
        assert!(m.pipelines.is_empty());
        assert!(m.external_deps.is_empty());
        assert!(m.config_options.is_empty());
        assert!(m.presets.is_empty());
    }

    #[test]
    fn build_model_json_round_trip() {
        let mut m = BuildModel {
            project: Some(ProjectDef {
                name: "demo".into(),
                version: "0.1.0".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let (_, inserted) = m.targets.insert(
            "x86_64".into(),
            TargetDef {
                name: "x86_64".into(),
                spec: "x86_64-unknown-none".into(),
                builtin: true,
                panic_strategy: None,
                span: None,
            },
        );
        assert!(inserted);

        let json = serde_json::to_string(&m).unwrap();
        let de: BuildModel = serde_json::from_str(&json).unwrap();
        assert_eq!(de.project.as_ref().unwrap().name, "demo");
        assert_eq!(de.targets.len(), 1);
        let h = de.targets.lookup("x86_64").expect("name index rebuilt");
        assert_eq!(de.targets.get(h).unwrap().spec, "x86_64-unknown-none");
    }

    #[test]
    fn crate_type_serde_matches_as_str() {
        for t in [
            CrateType::Lib,
            CrateType::Bin,
            CrateType::ProcMacro,
            CrateType::StaticLib,
        ] {
            let json = serde_json::to_string(&t).unwrap();
            let trimmed = json.trim_matches('"');
            assert_eq!(
                trimmed,
                t.as_str(),
                "serde form must match as_str for {t:?}"
            );
        }
    }
}
