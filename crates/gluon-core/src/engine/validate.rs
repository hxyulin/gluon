//! Validate pass: structural checks on the interned model.
//!
//! Runs after [`super::intern::intern`] has resolved every string cross
//! reference to its sibling handle. Checks here are restricted to invariants
//! the type system can't express (cycles, cross-arena consistency, top-level
//! presence) — field-level validation belongs in the builders, and semantic
//! resolution (profile inheritance merging, config selects, etc.) belongs in
//! the forthcoming `config::resolve` pass.

use crate::error::Diagnostic;
use gluon_model::{BuildModel, Handle, ProfileDef};
use std::collections::BTreeSet;

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

/// Two crates with the same name are already rejected by the arena's
/// duplicate-insert check, so this function is intentionally a no-op for
/// MVP-M. If the compile layer later derives output names from something
/// other than the crate name (e.g. `lib.rs` crate renames), this is where
/// the additional check would live.
fn check_crate_output_name_uniqueness(_model: &BuildModel, _diags: &mut Vec<Diagnostic>) {}

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
