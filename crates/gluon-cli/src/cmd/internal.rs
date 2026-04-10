//! Hidden `gluon internal …` maintenance subcommands.
//!
//! These commands exist for in-tree tooling that needs to introspect
//! the Gluon engine without rebuilding the registration logic: the
//! tree-sitter query regenerator reads `dump-dsl` output to refresh
//! `highlights.scm`, and the `gluon-lsp` smoke tests use it as an
//! oracle. They are deliberately undocumented at the user level and
//! hidden from `--help` — the output format is not a stable API.

use anyhow::Result;
use serde_json::json;

/// Print the registered Rhai DSL function list as a JSON array of
/// signature strings on stdout. See
/// [`gluon_core::engine::dsl_signatures`] for the exact shape; every
/// entry looks like `name(params) -> ReturnType`, the list is sorted,
/// and the output is deterministic across runs for the same gluon
/// binary.
///
/// Intentional coupling: the regen script and LSP treat this output
/// as authoritative. If the format ever needs to change, bump a
/// version wrapper around the payload (`{"version": 1, "signatures":
/// [...]}`) rather than breaking consumers silently.
pub fn run_dump_dsl() -> Result<()> {
    let sigs = gluon_core::engine::dsl_signatures();
    let doc = json!({
        "version": 1,
        "signatures": sigs,
    });
    // Pretty-printed so diffs against a checked-in fixture are
    // reviewable. The size is small (~100 entries), so the prettiness
    // cost is negligible.
    println!("{}", serde_json::to_string_pretty(&doc)?);
    Ok(())
}
