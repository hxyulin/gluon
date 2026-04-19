//! [`ArtifactMap`] — a handle-keyed map of built crate artifact paths.
//!
//! The scheduler populates this as each crate completes; `compile_crate`
//! consults it to wire `--extern` flags for the crate's dependencies.
//!
//! ### Why [`BTreeMap`] not [`HashMap`](std::collections::HashMap)
//!
//! `--extern` flags are pushed onto the rustc argv in the order they are
//! visited. Using a `BTreeMap` guarantees that iteration order is
//! determined by the handle index (a monotonically assigned integer) rather
//! than the random hash seed — so the resulting argv, and therefore the
//! cache key, are identical across runs on any machine. This is mandatory
//! per CLAUDE.md's determinism requirement.

use gluon_model::{CrateDef, Handle, TargetDef};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Map from a crate [`Handle`] to its built artifact path.
///
/// Populated by the scheduler as each crate completes and consumed by
/// `compile_crate` to wire `--extern` for that crate's dependencies.
///
/// The `config_crates` map is kept separate from the handle map because
/// the generated `<project>_config` crates have no [`Handle<CrateDef>`] in
/// the main arena — they are synthesised by the scheduler. One config crate
/// is compiled per distinct cross target, so `compile_crate` can add the
/// implicit `--extern <name>_config=<path>` for cross crates on any target.
#[derive(Debug, Default, Clone)]
pub struct ArtifactMap {
    crates: BTreeMap<Handle<CrateDef>, PathBuf>,
    config_crates: BTreeMap<Handle<TargetDef>, PathBuf>,
}

impl ArtifactMap {
    /// Create an empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `h`'s artifact lives at `path`.
    pub fn insert(&mut self, h: Handle<CrateDef>, path: PathBuf) {
        self.crates.insert(h, path);
    }

    /// Look up the artifact path for `h`, if it has been recorded.
    pub fn get(&self, h: Handle<CrateDef>) -> Option<&Path> {
        self.crates.get(&h).map(PathBuf::as_path)
    }

    /// Return `true` if `h`'s artifact has been recorded.
    pub fn contains(&self, h: Handle<CrateDef>) -> bool {
        self.crates.contains_key(&h)
    }

    /// Iterate over `(handle, path)` pairs in handle-index order.
    ///
    /// The handle index is the insertion order into the crate arena, which
    /// is also the scheduler's topological sort order. Iteration here is
    /// therefore deterministic and stable across runs.
    pub fn iter(&self) -> impl Iterator<Item = (Handle<CrateDef>, &Path)> + '_ {
        self.crates.iter().map(|(h, p)| (*h, p.as_path()))
    }

    /// Number of crate entries (does not count the config-crate slot).
    pub fn len(&self) -> usize {
        self.crates.len()
    }

    /// `true` when no crate entries have been recorded.
    pub fn is_empty(&self) -> bool {
        self.crates.is_empty()
    }

    /// Record the path of the generated config crate rlib for a specific
    /// cross target.
    ///
    /// Called by the scheduler after each per-target config crate is compiled.
    /// `compile_crate` looks up the config crate by the crate's own target
    /// to inject `--extern <name>_config=<path>`.
    pub fn set_config_crate(&mut self, target: Handle<TargetDef>, path: PathBuf) {
        self.config_crates.insert(target, path);
    }

    /// The path of the config crate rlib for `target`, if it has been compiled.
    pub fn config_crate(&self, target: Handle<TargetDef>) -> Option<&Path> {
        self.config_crates.get(&target).map(PathBuf::as_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gluon_model::Handle;

    fn h(index: u32) -> Handle<CrateDef> {
        Handle::new(index)
    }

    #[test]
    fn insert_and_get_round_trip() {
        let mut map = ArtifactMap::new();
        map.insert(h(0), PathBuf::from("/build/libfoo.rlib"));
        assert_eq!(map.get(h(0)), Some(Path::new("/build/libfoo.rlib")));
        assert_eq!(map.get(h(1)), None);
    }

    #[test]
    fn iter_is_deterministic_across_inserts() {
        // Insert in non-sequential order; the BTreeMap must sort by handle
        // index so iteration always yields (0, 1, 2) regardless of the
        // insertion order.
        let mut map = ArtifactMap::new();
        map.insert(h(2), PathBuf::from("/build/libc.rlib"));
        map.insert(h(0), PathBuf::from("/build/liba.rlib"));
        map.insert(h(1), PathBuf::from("/build/libb.rlib"));

        let collected: Vec<(Handle<CrateDef>, PathBuf)> =
            map.iter().map(|(h, p)| (h, p.to_path_buf())).collect();

        // Collect again — must be byte-for-byte identical (determinism).
        let collected2: Vec<(Handle<CrateDef>, PathBuf)> =
            map.iter().map(|(h, p)| (h, p.to_path_buf())).collect();
        assert_eq!(collected, collected2);

        // Order must be by ascending handle index.
        let handles: Vec<u32> = collected.iter().map(|(h, _)| h.index()).collect();
        assert_eq!(handles, vec![0, 1, 2]);

        let paths: Vec<&Path> = collected.iter().map(|(_, p)| p.as_path()).collect();
        assert_eq!(
            paths,
            vec![
                Path::new("/build/liba.rlib"),
                Path::new("/build/libb.rlib"),
                Path::new("/build/libc.rlib"),
            ]
        );
    }

    #[test]
    fn set_config_crate_and_read_back() {
        use gluon_model::Handle;
        let mut map = ArtifactMap::new();
        let t0: Handle<TargetDef> = Handle::new(0);
        let t1: Handle<TargetDef> = Handle::new(1);
        assert_eq!(map.config_crate(t0), None);

        map.set_config_crate(t0, PathBuf::from("/build/libmyproject_config.rlib"));
        assert_eq!(
            map.config_crate(t0),
            Some(Path::new("/build/libmyproject_config.rlib"))
        );
        assert_eq!(
            map.config_crate(t1),
            None,
            "other target has no config crate"
        );

        // Overwrite is supported (e.g. a rebuild of the config crate).
        map.set_config_crate(t0, PathBuf::from("/build/libmyproject_config_v2.rlib"));
        assert_eq!(
            map.config_crate(t0),
            Some(Path::new("/build/libmyproject_config_v2.rlib"))
        );

        // Second target gets its own config crate.
        map.set_config_crate(t1, PathBuf::from("/build/libmyproject_config_t1.rlib"));
        assert_eq!(
            map.config_crate(t1),
            Some(Path::new("/build/libmyproject_config_t1.rlib"))
        );
    }

    #[test]
    fn contains_returns_false_for_absent_handle() {
        let map = ArtifactMap::new();
        assert!(!map.contains(h(0)));
    }

    #[test]
    fn len_and_is_empty() {
        let mut map = ArtifactMap::new();
        assert!(map.is_empty());
        assert_eq!(map.len(), 0);
        map.insert(h(0), PathBuf::from("/a.rlib"));
        assert!(!map.is_empty());
        assert_eq!(map.len(), 1);
    }
}
