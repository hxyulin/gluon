//! DSL symbol index built from [`gluon_core::engine::dsl_signatures`].
//!
//! The LSP keeps one [`DslIndex`] for the lifetime of the server; it
//! is constructed once at startup (during `initialize`) and never
//! mutated afterward. Every request — completion, hover — reads from
//! this index.
//!
//! **Design: why a flat list with dedup-by-name.** Rhai reports each
//! overload of a function separately (e.g. `target(_: string)` and
//! `target(_: string, _: string)` are two entries). For completion we
//! only want one item per *name*, with all overloads stacked in the
//! `detail` field so the user sees every arity. For hover we want the
//! same — a single response listing every variant. Deduping on insert
//! lets both paths read from one index without re-aggregating per
//! request.
//!
//! The parser is deliberately permissive: Rhai signature strings have
//! a stable-ish shape (`name(params) -> Return`), but the param list
//! can include complex Rust type names with `<>`, `::`, `&mut`, and
//! `, ` separators. Rather than write a real parser, we split on the
//! first `(` to get the name and keep the rest as an opaque display
//! string. That's enough for MVP: the name drives completion, the
//! full signature drives hover.

use std::collections::BTreeMap;

/// One DSL symbol. Groups every overload Rhai reported for a given
/// name — there is a single [`DslSymbol`] per `name`, not per arity.
#[derive(Debug, Clone)]
pub struct DslSymbol {
    /// Bare function name (the token before `(`).
    pub name: String,
    /// Every signature string for this name, in the order Rhai
    /// returned them (sorted, because [`gluon_core::engine::dsl_signatures`]
    /// sorts its output). Used verbatim as the hover body.
    pub overloads: Vec<String>,
}

impl DslSymbol {
    /// A single-line display suitable for LSP completion `detail`.
    /// Shows the first overload plus an `(+N more)` suffix when more
    /// than one exists — rust-analyzer uses a similar convention.
    pub fn completion_detail(&self) -> String {
        match self.overloads.len() {
            0 => self.name.clone(),
            1 => self.overloads[0].clone(),
            n => format!("{} (+{} more)", self.overloads[0], n - 1),
        }
    }

    /// Multi-line markdown body suitable for an LSP hover: every
    /// overload in a Rust code block.
    pub fn hover_markdown(&self) -> String {
        let mut out = String::from("```rust\n");
        for sig in &self.overloads {
            out.push_str(sig);
            out.push('\n');
        }
        out.push_str("```");
        out
    }
}

/// The entire DSL symbol table. Keyed by name so hover lookups are
/// O(log N); the ordered iteration of `BTreeMap` also gives
/// deterministic completion output without a separate sort step.
#[derive(Debug, Default)]
pub struct DslIndex {
    pub symbols: BTreeMap<String, DslSymbol>,
}

impl DslIndex {
    /// Build an index by introspecting a fresh Gluon engine. This is
    /// the canonical entry point — the LSP calls it exactly once, at
    /// `initialize`.
    pub fn from_engine() -> Self {
        Self::from_signatures(gluon_core::engine::dsl_signatures())
    }

    /// Parse a list of Rhai signature strings into the index. Split
    /// out from [`Self::from_engine`] so unit tests can supply
    /// hand-written fixtures without spinning up a real engine.
    pub fn from_signatures(sigs: Vec<String>) -> Self {
        let mut symbols: BTreeMap<String, DslSymbol> = BTreeMap::new();
        for sig in sigs {
            let name = parse_name(&sig);
            // Skip anything that doesn't look like a function
            // registration. Defensive: Rhai's metadata output is
            // well-formed today, but we'd rather drop garbage than
            // surface empty completion items.
            if name.is_empty() {
                continue;
            }
            symbols
                .entry(name.clone())
                .or_insert_with(|| DslSymbol {
                    name,
                    overloads: Vec::new(),
                })
                .overloads
                .push(sig);
        }
        Self { symbols }
    }

    pub fn get(&self, name: &str) -> Option<&DslSymbol> {
        self.symbols.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &DslSymbol> {
        self.symbols.values()
    }
}

/// Extract the function name — everything up to the first `(`.
/// Trimmed to handle any leading whitespace Rhai may introduce.
fn parse_name(sig: &str) -> String {
    sig.split_once('(')
        .map(|(n, _)| n.trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_name() {
        assert_eq!(parse_name("target(_: string)"), "target");
        assert_eq!(
            parse_name("qemu() -> gluon_core::engine::builders::pipeline::QemuBuilder"),
            "qemu"
        );
    }

    #[test]
    fn parse_name_handles_garbage() {
        assert_eq!(parse_name(""), "");
        assert_eq!(parse_name("no_paren"), "");
    }

    #[test]
    fn dedupes_overloads_under_one_symbol() {
        let sigs = vec![
            "target(_: string)".to_string(),
            "target(_: string, _: string)".to_string(),
            "group(_: string) -> GroupBuilder".to_string(),
        ];
        let idx = DslIndex::from_signatures(sigs);
        assert_eq!(idx.symbols.len(), 2);
        let target = idx.get("target").expect("target symbol");
        assert_eq!(target.overloads.len(), 2);
        assert_eq!(idx.get("group").expect("group").overloads.len(), 1);
    }

    #[test]
    fn completion_detail_shows_overload_count() {
        let idx = DslIndex::from_signatures(vec![
            "x(_: i64)".to_string(),
            "x(_: string)".to_string(),
            "x(_: bool)".to_string(),
        ]);
        let detail = idx.get("x").unwrap().completion_detail();
        assert!(detail.contains("+2 more"), "got: {detail}");
    }

    #[test]
    fn single_overload_completion_detail_is_raw_signature() {
        let idx = DslIndex::from_signatures(vec!["y(_: i64) -> Z".to_string()]);
        let detail = idx.get("y").unwrap().completion_detail();
        assert_eq!(detail, "y(_: i64) -> Z");
    }

    #[test]
    fn hover_markdown_contains_every_overload() {
        let idx = DslIndex::from_signatures(vec![
            "z(_: i64)".to_string(),
            "z(_: string)".to_string(),
        ]);
        let md = idx.get("z").unwrap().hover_markdown();
        assert!(md.contains("z(_: i64)"));
        assert!(md.contains("z(_: string)"));
        assert!(md.starts_with("```rust\n"));
        assert!(md.ends_with("```"));
    }

    #[test]
    fn from_engine_includes_well_known_entry_points() {
        // Ties the LSP's symbol index to gluon-core's engine surface.
        // If a future refactor silently drops `target`/`group` from
        // the engine, this fails loudly instead of the LSP quietly
        // returning no completions.
        let idx = DslIndex::from_engine();
        for name in ["project", "target", "group", "profile", "pipeline", "qemu"] {
            assert!(
                idx.get(name).is_some(),
                "expected `{name}` in index, available: {:?}",
                idx.symbols.keys().collect::<Vec<_>>()
            );
        }
    }
}
