//! Validate pass: structural checks on the interned model.
//!
//! Runs after [`super::intern::intern`] has resolved every string cross
//! reference to its sibling handle. Checks here are restricted to invariants
//! the type system can't express (cycles, cross-arena consistency, top-level
//! presence) — field-level validation belongs in the builders, and semantic
//! resolution (profile inheritance merging, config selects, etc.) belongs in
//! the forthcoming `config::resolve` pass.

use crate::compile::compile_utils::normalize_crate_name;
use crate::error::Diagnostic;
use gluon_model::{BuildModel, Handle, ProfileDef};
use std::collections::{BTreeMap, BTreeSet};

/// Structural checks that happen after interning.
pub(crate) fn validate(model: &BuildModel) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    check_project_present(model, &mut diags);
    check_default_profile_exists(model, &mut diags);
    check_profile_inheritance_cycles(model, &mut diags);
    check_crate_output_name_uniqueness(model, &mut diags);
    check_pipeline_stage_references(model, &mut diags);
    check_artifact_deps_resolve(model, &mut diags);
    diags
}

/// Reject `CrateDef::artifact_deps` entries that don't name a real crate.
///
/// `artifact_deps` is a list of crate names (not handles — Rhai callers can
/// only type strings) that produce ordering-only DAG edges. Unlike regular
/// `deps`, there's no intern pass that fills in a `crate_handle`, so the
/// first time we'd notice a typo is when `build_dag` silently drops the
/// edge and the bootloader sees stale (or missing) kernel bytes. Catch it
/// at validate time where we can still point at the crate's span.
fn check_artifact_deps_resolve(model: &BuildModel, diags: &mut Vec<Diagnostic>) {
    for (_handle, crate_def) in model.crates.iter() {
        for dep_name in &crate_def.artifact_deps {
            if model.crates.lookup(dep_name).is_none() {
                diags.push(
                    Diagnostic::error(format!(
                        "crate '{}' declares artifact_deps entry '{}', but no crate with that name exists in the model",
                        crate_def.name, dep_name
                    ))
                    .with_optional_span(crate_def.span.clone()),
                );
            }
        }
    }
}

/// Reject `project().default_profile("name")` when "name" doesn't
/// match any declared profile. Runs after interning so
/// `model.profiles.lookup` is authoritative. Without this check a
/// typo in `default_profile` would only be caught at run time when
/// the CLI tried to look up the profile and silently fell back to
/// alphabetical order — the exact footgun the field exists to fix.
fn check_default_profile_exists(model: &BuildModel, diags: &mut Vec<Diagnostic>) {
    let Some(project) = model.project.as_ref() else {
        return;
    };
    let Some(name) = project.default_profile.as_deref() else {
        return;
    };
    if model.profiles.lookup(name).is_none() {
        let known: Vec<&str> = model.profiles.names().map(|(n, _)| n).collect();
        let known_list = if known.is_empty() {
            "no profiles declared".to_string()
        } else {
            known.join(", ")
        };
        diags.push(Diagnostic::error(format!(
            "project().default_profile(\"{name}\") references an unknown profile. Known profiles: {known_list}"
        )));
    }
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
/// rustc (see [`crate::compile::compile_utils::normalize_crate_name`]).
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

    fn make_project_with_default_profile(default: Option<&str>) -> gluon_model::ProjectDef {
        gluon_model::ProjectDef {
            name: "demo".into(),
            version: "0.1.0".into(),
            default_profile: default.map(|s| s.into()),
            ..Default::default()
        }
    }

    fn make_profile(name: &str) -> ProfileDef {
        ProfileDef {
            name: name.into(),
            ..Default::default()
        }
    }

    fn model_with_project(default: Option<&str>) -> BuildModel {
        BuildModel {
            project: Some(make_project_with_default_profile(default)),
            ..Default::default()
        }
    }

    #[test]
    fn default_profile_none_passes() {
        let mut model = model_with_project(None);
        let _ = model.profiles.insert("dev".into(), make_profile("dev"));

        let mut diags = Vec::new();
        check_default_profile_exists(&model, &mut diags);
        assert!(diags.is_empty());
    }

    #[test]
    fn default_profile_existing_name_passes() {
        let mut model = model_with_project(Some("release"));
        let _ = model.profiles.insert("dev".into(), make_profile("dev"));
        let _ = model
            .profiles
            .insert("release".into(), make_profile("release"));

        let mut diags = Vec::new();
        check_default_profile_exists(&model, &mut diags);
        assert!(diags.is_empty(), "no diag expected, got: {diags:?}");
    }

    #[test]
    fn default_profile_unknown_name_errors() {
        let mut model = model_with_project(Some("typo"));
        let _ = model.profiles.insert("dev".into(), make_profile("dev"));
        let _ = model
            .profiles
            .insert("release".into(), make_profile("release"));

        let mut diags = Vec::new();
        check_default_profile_exists(&model, &mut diags);
        assert_eq!(diags.len(), 1);
        let msg = &diags[0].message;
        assert!(msg.contains("typo"), "diag must name the bad value: {msg}");
        // Known list must be included so the user sees what's valid.
        assert!(
            msg.contains("dev") && msg.contains("release"),
            "diag must list known profiles: {msg}"
        );
    }

    #[test]
    fn artifact_deps_unknown_name_errors() {
        let mut model = BuildModel::default();
        let mut consumer = make_crate("consumer");
        consumer.artifact_deps = vec!["ghost".into()];
        let _ = model.crates.insert("consumer".into(), consumer);

        let mut diags = Vec::new();
        check_artifact_deps_resolve(&model, &mut diags);
        assert_eq!(
            diags.len(),
            1,
            "expected one dangling-artifact_dep diagnostic"
        );
        let msg = &diags[0].message;
        assert!(
            msg.contains("consumer"),
            "diag must name the consumer crate: {msg}"
        );
        assert!(
            msg.contains("ghost"),
            "diag must name the missing target: {msg}"
        );
    }

    #[test]
    fn artifact_deps_existing_name_passes() {
        let mut model = BuildModel::default();
        let _ = model.crates.insert("kernel".into(), make_crate("kernel"));
        let mut bootloader = make_crate("bootloader");
        bootloader.artifact_deps = vec!["kernel".into()];
        let _ = model.crates.insert("bootloader".into(), bootloader);

        let mut diags = Vec::new();
        check_artifact_deps_resolve(&model, &mut diags);
        assert!(diags.is_empty(), "no diag expected, got: {diags:?}");
    }

    #[test]
    fn default_profile_with_no_profiles_at_all_errors_clearly() {
        // No profiles registered at all — still an error, but the
        // diagnostic should say so rather than claim the name is in
        // an empty list.
        let model = model_with_project(Some("dev"));

        let mut diags = Vec::new();
        check_default_profile_exists(&model, &mut diags);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("no profiles declared"));
    }
}
