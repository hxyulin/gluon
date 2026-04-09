//! Validate pass: structural checks on the interned model.
//!
//! Runs after [`super::intern::intern`] has resolved every string cross
//! reference to its sibling handle. Checks here are restricted to invariants
//! the type system can't express (cycles, cross-arena consistency, top-level
//! presence) — field-level validation belongs in the builders, and semantic
//! resolution (profile inheritance merging, config selects, etc.) belongs in
//! the forthcoming `config::resolve` pass.

use crate::compile::compile_crate::normalize_crate_name;
use crate::error::Diagnostic;
use gluon_model::{BuildModel, Handle, ProfileDef};
use std::collections::{BTreeMap, BTreeSet};

/// Structural checks that happen after interning.
pub(crate) fn validate(model: &BuildModel) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    check_project_present(model, &mut diags);
    check_profile_inheritance_cycles(model, &mut diags);
    check_crate_output_name_uniqueness(model, &mut diags);
    check_pipeline_stage_references(model, &mut diags);
    diags
}

fn check_project_present(model: &BuildModel, diags: &mut Vec<Diagnostic>) {
    if model.project.is_none() {
        diags.push(Diagnostic::error(
            "gluon.rhai must declare a project(name, version)",
        ));
    }
}

fn check_profile_inheritance_cycles(model: &BuildModel, diags: &mut Vec<Diagnostic>) {
    // Walk the inherits chain from each profile. A cycle shows up as a
    // revisit of a handle already in the current path set.
    for (start, _) in model.profiles.iter() {
        let mut path: Vec<Handle<ProfileDef>> = Vec::new();
        let mut visited: BTreeSet<Handle<ProfileDef>> = BTreeSet::new();
        let mut current = Some(start);
        while let Some(h) = current {
            if !visited.insert(h) {
                // Cycle: truncate path to the repeated handle.
                if let Some(pos) = path.iter().position(|p| *p == h) {
                    let cycle_names: Vec<String> = path[pos..]
                        .iter()
                        .filter_map(|p| model.profiles.get(*p).map(|pr| pr.name.clone()))
                        .collect();
                    // Only report the cycle once, starting from the
                    // lexicographically first handle in the cycle, so that
                    // walking from other entry points doesn't duplicate it.
                    let min_in_cycle = path[pos..].iter().min().copied();
                    if min_in_cycle == Some(start) {
                        let start_profile = model.profiles.get(start);
                        diags.push(
                            Diagnostic::error(format!(
                                "profile inheritance cycle: {}",
                                cycle_names.join(" -> ")
                            ))
                            .with_optional_span(start_profile.and_then(|p| p.span.clone())),
                        );
                    }
                }
                break;
            }
            path.push(h);
            current = model.profiles.get(h).and_then(|p| p.inherits_handle);
        }
    }
}

/// Reject crates whose names collapse to the same `--crate-name` after
/// dash-normalization.
///
/// The arena already rejects two crates with the *exact* same name, but
/// the compile layer normalizes `-` → `_` before handing the name to
/// rustc (see [`crate::compile::compile_crate::normalize_crate_name`]).
/// So `foo-bar` and `foo_bar` are distinct in the arena but indistinguishable
/// to rustc — they would clobber each other's `.rlib` on disk and produce
/// confusing "duplicate crate" errors at link time. We catch the collision
/// here, where we can still point at both source spans.
fn check_crate_output_name_uniqueness(model: &BuildModel, diags: &mut Vec<Diagnostic>) {
    // Group every crate by its normalized name. A group of size > 1 is a
    // collision worth reporting.
    let mut by_normalized: BTreeMap<String, Vec<Handle<gluon_model::CrateDef>>> = BTreeMap::new();
    for (handle, crate_def) in model.crates.iter() {
        let normalized = normalize_crate_name(&crate_def.name).into_owned();
        by_normalized.entry(normalized).or_default().push(handle);
    }

    for (normalized, handles) in by_normalized {
        if handles.len() < 2 {
            continue;
        }
        // Build a list of the user-typed names for the error message,
        // sorted for stable output. Spans are attached to the first
        // colliding crate (the diagnostic renderer only carries one
        // primary span; the message lists every offender by name).
        let names: Vec<String> = handles
            .iter()
            .filter_map(|h| model.crates.get(*h).map(|c| c.name.clone()))
            .collect();
        let primary_span = handles
            .first()
            .and_then(|h| model.crates.get(*h))
            .and_then(|c| c.span.clone());
        diags.push(
            Diagnostic::error(format!(
                "crates {} all normalize to '{}' for rustc; \
                 rename one so they don't collide on disk",
                names
                    .iter()
                    .map(|n| format!("'{n}'"))
                    .collect::<Vec<_>>()
                    .join(", "),
                normalized,
            ))
            .with_optional_span(primary_span),
        );
    }
}

fn check_pipeline_stage_references(model: &BuildModel, diags: &mut Vec<Diagnostic>) {
    for (_, pipeline) in model.pipelines.iter() {
        for stage in &pipeline.stages {
            if stage.inputs.len() != stage.inputs_handles.len() {
                diags.push(Diagnostic::error(format!(
                    "pipeline '{}' stage '{}' has {} inputs but {} resolved handles (intern bug)",
                    pipeline.name,
                    stage.name,
                    stage.inputs.len(),
                    stage.inputs_handles.len()
                )));
            }
            if let Some(rule_name) = &stage.rule
                && model.rules.lookup(rule_name).is_none()
            {
                diags.push(Diagnostic::error(format!(
                    "pipeline '{}' stage '{}' references unknown rule '{}'",
                    pipeline.name, stage.name, rule_name
                )));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gluon_model::{BuildModel, CrateDef, CrateType};

    fn make_crate(name: &str) -> CrateDef {
        CrateDef {
            name: name.into(),
            path: format!("crates/{name}"),
            edition: "2021".into(),
            crate_type: CrateType::Lib,
            ..Default::default()
        }
    }

    #[test]
    fn dash_underscore_collision_is_reported() {
        // `foo-bar` and `foo_bar` are two distinct entries in the arena
        // (the user typed two distinct names), but they collide once the
        // compile layer normalizes dashes for rustc. validate must catch
        // this with a diagnostic naming both crates.
        let mut model = BuildModel::default();
        let _ = model.crates.insert("foo-bar".into(), make_crate("foo-bar"));
        let _ = model.crates.insert("foo_bar".into(), make_crate("foo_bar"));

        let mut diags = Vec::new();
        check_crate_output_name_uniqueness(&model, &mut diags);

        assert_eq!(diags.len(), 1, "expected exactly one collision diagnostic");
        let msg = &diags[0].message;
        assert!(msg.contains("'foo-bar'"), "diag must name foo-bar: {msg}");
        assert!(msg.contains("'foo_bar'"), "diag must name foo_bar: {msg}");
        assert!(
            msg.contains("foo_bar"),
            "diag must mention normalized form: {msg}"
        );
    }

    #[test]
    fn distinct_normalized_names_pass() {
        let mut model = BuildModel::default();
        let _ = model.crates.insert("foo".into(), make_crate("foo"));
        let _ = model.crates.insert("bar".into(), make_crate("bar"));
        let _ = model.crates.insert("baz-qux".into(), make_crate("baz-qux"));

        let mut diags = Vec::new();
        check_crate_output_name_uniqueness(&model, &mut diags);
        assert!(diags.is_empty(), "no collision expected, got: {diags:?}");
    }
}
