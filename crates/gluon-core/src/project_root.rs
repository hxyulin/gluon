//! Project-root discovery.
//!
//! Walks upward from a starting directory looking for a `gluon.rhai`
//! file and returns its enclosing directory. Lives in `gluon-core` (not
//! `gluon-cli`) so that embedders — editor plugins, LSPs, future
//! `gluon-*` plugin binaries — can reuse the same discovery logic
//! without re-implementing it.

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};

/// Walk upward from `start` looking for a `gluon.rhai` file. Returns the
/// directory that contains it.
///
/// `start` may be either a file or a directory. If it is a file, the
/// walk begins from its parent. The search terminates when either a
/// `gluon.rhai` is found or the filesystem root is reached without
/// finding one.
///
/// ### Why not record the script path itself?
///
/// Callers (CLI, LSP, embedders) want to anchor relative paths against
/// the *project root*, not against the script. Returning the directory
/// keeps the project-root concept opaque to consumers — they don't have
/// to know whether the marker file is `gluon.rhai`, `gluon.toml`, or
/// `.gluonrc` in some future world.
pub fn find_project_root(start: &Path) -> Result<PathBuf> {
    // If `start` is a file, begin from its parent. Symlinks are followed
    // by `metadata` here intentionally — a symlinked checkout should
    // resolve to its physical project tree, not the symlink's parent.
    let mut cursor: PathBuf = if start.is_file() {
        start
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("/"))
    } else {
        start.to_path_buf()
    };

    loop {
        let candidate = cursor.join("gluon.rhai");
        if candidate.is_file() {
            return Ok(cursor);
        }
        // `Path::parent` returns `None` once we hit the filesystem root.
        if !cursor.pop() {
            return Err(Error::Config(format!(
                "no gluon.rhai found in {} or any parent directory",
                start.display()
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn finds_gluon_rhai_in_starting_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        fs::write(tmp.path().join("gluon.rhai"), b"// stub").expect("write script");

        let got = find_project_root(tmp.path()).expect("should find");
        assert_eq!(
            got.canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn errors_when_no_gluon_rhai_anywhere() {
        // Use a child dir under tempdir so we definitely won't accidentally
        // find a real gluon.rhai in the user's filesystem ancestors.
        let tmp = tempfile::tempdir().expect("tempdir");
        let child = tmp.path().join("only-this");
        fs::create_dir_all(&child).expect("mkdir child");

        // Note: this test will spuriously pass if a gluon.rhai exists in
        // /tmp or any ancestor — but in practice tempdir's prefix is
        // always under a controlled location, and CI environments do not
        // have spurious gluon.rhai files at /tmp/.
        let result = find_project_root(&child);
        match result {
            Err(Error::Config(msg)) => {
                assert!(
                    msg.contains("gluon.rhai"),
                    "error message should mention gluon.rhai, got: {msg}"
                );
            }
            Ok(p) => panic!("unexpected success at {p:?}"),
            Err(other) => panic!("expected Error::Config, got {other:?}"),
        }
    }

    #[test]
    fn finds_gluon_rhai_from_nested_subdirectory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        fs::write(tmp.path().join("gluon.rhai"), b"// stub").expect("write script");

        let nested = tmp.path().join("a").join("b").join("c");
        fs::create_dir_all(&nested).expect("mkdir nested");

        let got = find_project_root(&nested).expect("should find via walk");
        assert_eq!(
            got.canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn accepts_file_path_as_start() {
        let tmp = tempfile::tempdir().expect("tempdir");
        fs::write(tmp.path().join("gluon.rhai"), b"// stub").expect("write script");
        let nested = tmp.path().join("a");
        fs::create_dir_all(&nested).expect("mkdir nested");
        let some_file = nested.join("foo.rs");
        fs::write(&some_file, b"fn main(){}").expect("write file");

        let got = find_project_root(&some_file).expect("should find via walk from file");
        assert_eq!(
            got.canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap()
        );
    }
}
