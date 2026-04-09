//! File-system loader for `.kconfig` files.
//!
//! Resolves `source "./path"` directives recursively, lexes and parses
//! every file once, then lowers the merged AST into the model
//! representation. Each file is loaded at most once — both cycles
//! (`A` sources `A`) and diamonds (`A` sources `B` and `C`, both of
//! which source `D`) collapse to a single load. This is simpler and
//! safer than tracking active-loading vs. fully-loaded states, and
//! diamonds are common enough in real `.kconfig` trees that
//! double-loading would surface as confusing duplicate-name errors.

use super::ast::{File as AstFile, Item};
use super::lexer::lex;
use super::lower::{Lowered, lower};
use super::parser::parse;
use crate::error::Diagnostic;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Load a `.kconfig` file and every file it transitively `source`s,
/// returning the lowered options + presets on success.
///
/// `root` is the path to the entry-point file. Relative paths are
/// resolved relative to the file containing the `source` directive.
///
/// On error, the returned vector contains every diagnostic collected
/// across the lex / parse / lower passes for the entire file tree.
pub fn load_kconfig(root: &Path) -> Result<Lowered, Vec<Diagnostic>> {
    let mut visited: BTreeSet<PathBuf> = BTreeSet::new();
    let mut all_items: Vec<Item> = Vec::new();
    let mut diags: Vec<Diagnostic> = Vec::new();
    load_recursive(root, &mut visited, &mut all_items, &mut diags);
    if !diags.is_empty() {
        return Err(diags);
    }
    let merged = AstFile { items: all_items };
    lower(&merged)
}

fn load_recursive(
    path: &Path,
    visited: &mut BTreeSet<PathBuf>,
    out: &mut Vec<Item>,
    diags: &mut Vec<Diagnostic>,
) {
    let canonical = match path.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            diags.push(Diagnostic::error(format!(
                "could not open .kconfig file '{}': {e}",
                path.display()
            )));
            return;
        }
    };

    // Diamond / cycle protection: load each canonical path at most once.
    if !visited.insert(canonical.clone()) {
        return;
    }

    let source = match std::fs::read_to_string(&canonical) {
        Ok(s) => s,
        Err(e) => {
            diags.push(Diagnostic::error(format!(
                "could not read .kconfig file '{}': {e}",
                canonical.display()
            )));
            return;
        }
    };

    let toks = match lex(&source, &canonical) {
        Ok(t) => t,
        Err(errs) => {
            diags.extend(errs);
            return;
        }
    };

    let parsed = match parse(&toks) {
        Ok(f) => f,
        Err(errs) => {
            diags.extend(errs);
            return;
        }
    };

    let parent = canonical.parent().map(Path::to_path_buf);
    for item in parsed.items {
        match item {
            Item::Source(decl) => {
                let resolved = if let Some(parent) = &parent {
                    parent.join(&decl.path)
                } else {
                    PathBuf::from(&decl.path)
                };
                load_recursive(&resolved, visited, out, diags);
            }
            other => out.push(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn loads_single_file() {
        let dir = tempdir().expect("tempdir");
        let kconfig = dir.path().join("options.kconfig");
        fs::write(&kconfig, "config DEBUG: bool { default = true }").expect("write");

        let lw = load_kconfig(&kconfig).expect("load ok");
        assert_eq!(lw.options.len(), 1);
        assert!(lw.options.contains_key("DEBUG"));
    }

    #[test]
    fn source_directive_pulls_in_subfile() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join("root.kconfig"),
            r#"
            source "./sub.kconfig"
            config TOPLEVEL: bool {}
            "#,
        )
        .expect("write root");
        fs::write(
            dir.path().join("sub.kconfig"),
            "config FROM_SUB: bool { default = true }",
        )
        .expect("write sub");

        let lw = load_kconfig(&dir.path().join("root.kconfig")).expect("load ok");
        assert_eq!(lw.options.len(), 2);
        assert!(lw.options.contains_key("TOPLEVEL"));
        assert!(lw.options.contains_key("FROM_SUB"));
    }

    #[test]
    fn diamond_includes_load_each_file_once() {
        // root sources A and B; both A and B source common.kconfig.
        // common defines OPT — should appear exactly once and not
        // trigger a duplicate-declaration error.
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join("root.kconfig"),
            r#"
            source "./a.kconfig"
            source "./b.kconfig"
            "#,
        )
        .expect("write");
        fs::write(dir.path().join("a.kconfig"), r#"source "./common.kconfig""#).expect("write");
        fs::write(dir.path().join("b.kconfig"), r#"source "./common.kconfig""#).expect("write");
        fs::write(dir.path().join("common.kconfig"), "config OPT: bool {}").expect("write");

        let lw = load_kconfig(&dir.path().join("root.kconfig")).expect("diamond load ok");
        assert_eq!(lw.options.len(), 1);
        assert!(lw.options.contains_key("OPT"));
    }

    #[test]
    fn cycle_does_not_infinite_loop() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join("a.kconfig"),
            r#"
            source "./b.kconfig"
            config A: bool {}
            "#,
        )
        .expect("write");
        fs::write(
            dir.path().join("b.kconfig"),
            r#"
            source "./a.kconfig"
            config B: bool {}
            "#,
        )
        .expect("write");

        let lw = load_kconfig(&dir.path().join("a.kconfig")).expect("cycle load ok");
        assert_eq!(lw.options.len(), 2);
    }

    #[test]
    fn missing_file_reports_diagnostic() {
        let dir = tempdir().expect("tempdir");
        let missing = dir.path().join("does_not_exist.kconfig");
        let diags = load_kconfig(&missing).expect_err("should fail");
        assert!(diags.iter().any(|d| d.message.contains("could not open")));
    }

    #[test]
    fn missing_sourced_file_reports_diagnostic() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join("root.kconfig"),
            r#"source "./nope.kconfig""#,
        )
        .expect("write");
        let diags = load_kconfig(&dir.path().join("root.kconfig")).expect_err("should fail");
        assert!(diags.iter().any(|d| d.message.contains("could not open")));
    }

    #[test]
    fn parse_error_in_sub_propagates() {
        let dir = tempdir().expect("tempdir");
        fs::write(dir.path().join("root.kconfig"), r#"source "./bad.kconfig""#).expect("write");
        fs::write(dir.path().join("bad.kconfig"), "config X bool {}").expect("write");
        let diags = load_kconfig(&dir.path().join("root.kconfig")).expect_err("should fail");
        // Missing colon parse error.
        assert!(diags.iter().any(|d| d.message.contains("':'")));
    }

    #[test]
    fn cross_file_references_validate() {
        // depends_on can reference an option declared in a sourced file.
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join("root.kconfig"),
            r#"
            source "./base.kconfig"
            config FEATURE: bool { depends_on = BASE }
            "#,
        )
        .expect("write");
        fs::write(dir.path().join("base.kconfig"), "config BASE: bool {}").expect("write");

        let lw = load_kconfig(&dir.path().join("root.kconfig")).expect("load ok");
        assert!(lw.options.contains_key("FEATURE"));
        assert!(lw.options.contains_key("BASE"));
    }
}
