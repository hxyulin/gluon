//! Centralised path arithmetic for the gluon build tree.
//!
//! `BuildLayout` owns the mapping from logical build artefacts (a crate's
//! incremental directory, a target's sysroot, the cache manifest) to the
//! concrete `PathBuf` they live at on disk. It performs **no I/O**: every
//! method is a pure function of the layout's root plus its arguments.
//!
//! Routing every `build/...` path through this type keeps the directory
//! layout in one place, so later reorganisation (e.g. sharding by target,
//! adding a content-addressed store) only has to touch one file.

use gluon_model::{CrateDef, ResolvedProfile, TargetDef};
use std::path::{Path, PathBuf};

/// Pure path-arithmetic view of the gluon build directory.
///
/// All getters return computed paths; none of them touch the filesystem.
/// Callers are responsible for creating directories on demand (typically
/// the compile step just before it writes into a given path).
#[derive(Debug, Clone)]
pub struct BuildLayout {
    /// Absolute or relative root under which every derived path lives.
    /// Usually `<project>/build`, but this type treats it opaquely.
    root: PathBuf,
    /// Project name used to derive the generated config crate directory.
    project_name: String,
}

impl BuildLayout {
    /// Construct a new layout rooted at `root`.
    ///
    /// `root` is the directory every derived path is relative to — this
    /// type does **not** implicitly append `build/`. Pass whatever root the
    /// caller wants artefacts to live under (e.g. `<project>/build`).
    pub fn new(root: impl Into<PathBuf>, project_name: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            project_name: project_name.into(),
        }
    }

    /// The build root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path to the top-level cache manifest (`<root>/cache-manifest.json`).
    pub fn cache_manifest(&self) -> PathBuf {
        self.root.join("cache-manifest.json")
    }

    /// Per-target sysroot directory (`<root>/sysroot/<target>/`).
    ///
    /// Keyed by [`TargetDef::name`] (the gluon target name), not the
    /// underlying spec/triple — two targets with the same triple but
    /// different specs must not share a sysroot directory.
    pub fn sysroot_dir(&self, target: &TargetDef) -> PathBuf {
        self.root.join("sysroot").join(&target.name)
    }

    /// The directory rustc expects sysroot libs in
    /// (`<sysroot>/lib/rustlib/<target>/lib`).
    pub fn sysroot_lib_dir(&self, target: &TargetDef) -> PathBuf {
        self.sysroot_dir(target)
            .join("lib")
            .join("rustlib")
            .join(&target.name)
            .join("lib")
    }

    /// Stamp file marking a successful sysroot build
    /// (`<sysroot>/stamp`).
    pub fn sysroot_stamp(&self, target: &TargetDef) -> PathBuf {
        self.sysroot_dir(target).join("stamp")
    }

    /// Per-host-crate artifact directory (`<root>/host/<crate>/`).
    pub fn host_artifact_dir(&self, krate: &CrateDef) -> PathBuf {
        self.root.join("host").join(&krate.name)
    }

    /// Per-cross-crate artifact directory
    /// (`<root>/cross/<target>/<profile>/<crate>/`).
    pub fn cross_artifact_dir(
        &self,
        target: &TargetDef,
        profile: &ResolvedProfile,
        krate: &CrateDef,
    ) -> PathBuf {
        self.root
            .join("cross")
            .join(&target.name)
            .join(&profile.name)
            .join(&krate.name)
    }

    /// Final link/image output directory for a (target, profile) pair
    /// (`<root>/cross/<target>/<profile>/final/`).
    pub fn cross_final_dir(&self, target: &TargetDef, profile: &ResolvedProfile) -> PathBuf {
        self.root
            .join("cross")
            .join(&target.name)
            .join(&profile.name)
            .join("final")
    }

    /// Directory containing the generated config crate
    /// (`<root>/generated/<project>_config/`).
    ///
    /// The `_config` suffix is the MVP-M default. Overriding the name via
    /// `ProjectDef::config_crate_name` is out of scope for this chunk.
    pub fn generated_config_crate_dir(&self) -> PathBuf {
        self.root
            .join("generated")
            .join(format!("{}_config", self.project_name))
    }

    /// Per-crate incremental compilation directory
    /// (`<root>/incremental/<crate>/`).
    pub fn incremental_dir(&self, krate: &CrateDef) -> PathBuf {
        self.root.join("incremental").join(&krate.name)
    }

    /// Cache file for the probed [`super::RustcInfo`]
    /// (`<root>/.rustc-info.json`).
    pub fn rustc_info_cache(&self) -> PathBuf {
        self.root.join(".rustc-info.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gluon_model::Handle;

    fn layout() -> BuildLayout {
        BuildLayout::new("/tmp/fake", "demo")
    }

    fn target() -> TargetDef {
        TargetDef {
            name: "x86_64-test".into(),
            spec: "x86_64-unknown-none".into(),
            builtin: true,
            span: None,
        }
    }

    fn profile() -> ResolvedProfile {
        ResolvedProfile {
            name: "debug".into(),
            target: Handle::new(0),
            opt_level: 0,
            debug_info: true,
            lto: None,
            boot_binary: None,
        }
    }

    fn krate() -> CrateDef {
        CrateDef {
            name: "kernel".into(),
            ..Default::default()
        }
    }

    #[test]
    fn root_returned_verbatim() {
        assert_eq!(layout().root(), Path::new("/tmp/fake"));
    }

    #[test]
    fn cache_manifest_path() {
        assert_eq!(
            layout().cache_manifest(),
            PathBuf::from("/tmp/fake/cache-manifest.json")
        );
    }

    #[test]
    fn sysroot_paths() {
        let l = layout();
        let t = target();
        assert_eq!(
            l.sysroot_dir(&t),
            PathBuf::from("/tmp/fake/sysroot/x86_64-test")
        );
        assert_eq!(
            l.sysroot_lib_dir(&t),
            PathBuf::from("/tmp/fake/sysroot/x86_64-test/lib/rustlib/x86_64-test/lib")
        );
        assert_eq!(
            l.sysroot_stamp(&t),
            PathBuf::from("/tmp/fake/sysroot/x86_64-test/stamp")
        );
    }

    #[test]
    fn host_artifact_path() {
        assert_eq!(
            layout().host_artifact_dir(&krate()),
            PathBuf::from("/tmp/fake/host/kernel")
        );
    }

    #[test]
    fn cross_artifact_path() {
        assert_eq!(
            layout().cross_artifact_dir(&target(), &profile(), &krate()),
            PathBuf::from("/tmp/fake/cross/x86_64-test/debug/kernel")
        );
    }

    #[test]
    fn cross_final_path() {
        assert_eq!(
            layout().cross_final_dir(&target(), &profile()),
            PathBuf::from("/tmp/fake/cross/x86_64-test/debug/final")
        );
    }

    #[test]
    fn generated_config_crate_path() {
        assert_eq!(
            layout().generated_config_crate_dir(),
            PathBuf::from("/tmp/fake/generated/demo_config")
        );
    }

    #[test]
    fn incremental_path() {
        assert_eq!(
            layout().incremental_dir(&krate()),
            PathBuf::from("/tmp/fake/incremental/kernel")
        );
    }

    #[test]
    fn rustc_info_cache_path() {
        assert_eq!(
            layout().rustc_info_cache(),
            PathBuf::from("/tmp/fake/.rustc-info.json")
        );
    }

    #[test]
    fn targets_with_same_triple_different_names_do_not_collide() {
        let l = layout();
        let t1 = TargetDef {
            name: "x86_64-a".into(),
            spec: "x86_64-unknown-none".into(),
            builtin: true,
            span: None,
        };
        let t2 = TargetDef {
            name: "x86_64-b".into(),
            spec: "x86_64-unknown-none".into(),
            builtin: true,
            span: None,
        };
        assert_ne!(l.sysroot_dir(&t1), l.sysroot_dir(&t2));
    }
}
