//! Intern pass: resolve string cross-references to typed handles.
//!
//! Every arena item in [`BuildModel`] carries both a string-ref field (e.g.
//! `ProfileDef::inherits: Option<String>`) and a sibling handle field (e.g.
//! `ProfileDef::inherits_handle: Option<Handle<ProfileDef>>`) that is left
//! `None` during script evaluation. The intern pass walks every such field
//! and populates the handle sibling by looking the name up in the
//! appropriate arena.
//!
//! Dangling references are reported as diagnostics rather than hard errors
//! so the caller sees every problem at once. Unresolved handles are left
//! `None`; downstream passes must tolerate that or key off the returned
//! diagnostic list.
//!
//! # Borrow discipline
//!
//! Rust's borrow checker won't let us mutate an arena entry while holding
//! a borrow on the same arena (`model.profiles.get_mut(h).inherits_handle =
//! model.profiles.lookup(name)` is two simultaneous borrows). Each helper
//! therefore runs in two passes: the first collects `(handle, resolved)`
//! tuples into a scratch `Vec`, the second walks that `Vec` and performs
//! the mutations. The scratch vectors are short-lived and bounded by the
//! size of the arena being walked.

use crate::error::Diagnostic;
use gluon_model::{
    BuildModel, CrateDef, EspDef, GroupDef, Handle, PipelineDef, ProfileDef, TargetDef,
};

/// Outer per-pipeline, inner per-stage, innermost per-input resolved handle.
type PipelineInputResolution = (Handle<PipelineDef>, Vec<Vec<Option<Handle<GroupDef>>>>);

/// Walk every string-ref field in the model and populate its sibling
/// `Option<Handle<_>>` by looking the name up in the appropriate arena.
///
/// Collects every dangling reference into the returned diagnostic vec so
/// the caller sees all errors at once, not just the first.
pub(crate) fn intern(model: &mut BuildModel) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    intern_profiles(model, &mut diags);
    intern_groups(model, &mut diags);
    intern_crates(model, &mut diags);
    intern_pipelines(model, &mut diags);
    intern_esps(model, &mut diags);
    diags
}

/// Per-profile resolution result. Each inner `Option` records whether the
/// profile had a string ref in that slot at all; the inner `Option<Handle>`
/// is the resolved handle (`None` if the lookup failed — the diagnostic has
/// already been pushed in that case).
type ProfileResolution = (
    Handle<ProfileDef>,
    Option<Option<Handle<ProfileDef>>>,
    Option<Option<Handle<TargetDef>>>,
    Option<Option<Handle<CrateDef>>>,
);

fn intern_profiles(model: &mut BuildModel, diags: &mut Vec<Diagnostic>) {
    let mut updates: Vec<ProfileResolution> = Vec::with_capacity(model.profiles.len());

    for (handle, profile) in model.profiles.iter() {
        let inherits = profile.inherits.as_ref().map(|name| {
            let h = model.profiles.lookup(name);
            if h.is_none() {
                diags.push(
                    Diagnostic::error(format!(
                        "profile '{}' inherits from unknown profile '{}'",
                        profile.name, name
                    ))
                    .with_optional_span(profile.span.clone()),
                );
            }
            h
        });
        let target = profile.target.as_ref().map(|name| {
            let h = model.targets.lookup(name);
            if h.is_none() {
                diags.push(
                    Diagnostic::error(format!(
                        "profile '{}' references unknown target '{}'",
                        profile.name, name
                    ))
                    .with_optional_span(profile.span.clone()),
                );
            }
            h
        });
        let boot_binary = profile.boot_binary.as_ref().map(|name| {
            let h = model.crates.lookup(name);
            if h.is_none() {
                diags.push(
                    Diagnostic::error(format!(
                        "profile '{}' references unknown boot binary crate '{}'",
                        profile.name, name
                    ))
                    .with_optional_span(profile.span.clone()),
                );
            }
            h
        });
        updates.push((handle, inherits, target, boot_binary));
    }

    for (handle, inherits, target, boot_binary) in updates {
        if let Some(profile) = model.profiles.get_mut(handle) {
            if let Some(v) = inherits {
                profile.inherits_handle = v;
            }
            if let Some(v) = target {
                profile.target_handle = v;
            }
            if let Some(v) = boot_binary {
                profile.boot_binary_handle = v;
            }
        }
    }
}

fn intern_groups(model: &mut BuildModel, diags: &mut Vec<Diagnostic>) {
    // Groups always carry a mandatory `target: String`. The literal string
    // "host" is a sentinel meaning "use the host triple" and is not expected
    // to be in `model.targets`; leaving its handle `None` is correct.
    let mut updates: Vec<(Handle<GroupDef>, Option<Handle<TargetDef>>)> =
        Vec::with_capacity(model.groups.len());

    for (handle, group) in model.groups.iter() {
        if group.target == "host" {
            updates.push((handle, None));
            continue;
        }
        let h = model.targets.lookup(&group.target);
        if h.is_none() {
            diags.push(
                Diagnostic::error(format!(
                    "group '{}' references unknown target '{}'",
                    group.name, group.target
                ))
                .with_optional_span(group.span.clone()),
            );
        }
        updates.push((handle, h));
    }

    for (handle, target) in updates {
        if let Some(group) = model.groups.get_mut(handle) {
            group.target_handle = target;
        }
    }
}

/// Per-crate resolution result.
///
/// `deps` is an ordered list of `(dep_key, resolved_handle)` matching the
/// iteration order of `BTreeMap::iter` on the original `crate.deps`. Only
/// project-crate resolutions land as `Some`; vendored external deps resolve
/// to `None` with no diagnostic (that's not an error).
type CrateResolution = (
    Handle<CrateDef>,
    Option<Handle<GroupDef>>,
    Option<Option<Handle<TargetDef>>>,
    Vec<(String, Option<Handle<CrateDef>>)>,
    Vec<(String, Option<Handle<CrateDef>>)>,
);

fn intern_crates(model: &mut BuildModel, diags: &mut Vec<Diagnostic>) {
    let mut updates: Vec<CrateResolution> = Vec::with_capacity(model.crates.len());

    for (handle, krate) in model.crates.iter() {
        // Group ref is mandatory.
        let group_h = model.groups.lookup(&krate.group);
        if group_h.is_none() {
            diags.push(
                Diagnostic::error(format!(
                    "crate '{}' references unknown group '{}'",
                    krate.name, krate.group
                ))
                .with_optional_span(krate.span.clone()),
            );
        }

        // Target ref is optional and uses the "host" sentinel convention.
        let target_h = if krate.target.is_empty() || krate.target == "host" {
            None
        } else {
            let h = model.targets.lookup(&krate.target);
            if h.is_none() {
                diags.push(
                    Diagnostic::error(format!(
                        "crate '{}' references unknown target '{}'",
                        krate.name, krate.target
                    ))
                    .with_optional_span(krate.span.clone()),
                );
            }
            Some(h)
        };

        let deps = resolve_deps(model, krate, &krate.deps, diags, "deps");
        let dev_deps = resolve_deps(model, krate, &krate.dev_deps, diags, "dev-deps");

        updates.push((handle, group_h, target_h, deps, dev_deps));
    }

    for (handle, group_h, target_h, deps, dev_deps) in updates {
        if let Some(krate) = model.crates.get_mut(handle) {
            krate.group_handle = group_h;
            if let Some(v) = target_h {
                krate.target_handle = v;
            }
            for (key, resolved) in deps {
                if let Some(dep) = krate.deps.get_mut(&key) {
                    dep.crate_handle = resolved;
                }
            }
            for (key, resolved) in dev_deps {
                if let Some(dep) = krate.dev_deps.get_mut(&key) {
                    dep.crate_handle = resolved;
                }
            }
        }
    }
}

fn resolve_deps(
    model: &BuildModel,
    krate: &CrateDef,
    deps: &std::collections::BTreeMap<String, gluon_model::DepDef>,
    diags: &mut Vec<Diagnostic>,
    kind: &str,
) -> Vec<(String, Option<Handle<CrateDef>>)> {
    let mut out = Vec::with_capacity(deps.len());
    for (extern_name, dep) in deps.iter() {
        // 1. Project-crate lookup.
        if let Some(h) = model.crates.lookup(&dep.crate_name) {
            out.push((extern_name.clone(), Some(h)));
            continue;
        }
        // 2. External dep fallback — no handle populated, and no diagnostic.
        if model.external_deps.lookup(&dep.crate_name).is_some() {
            out.push((extern_name.clone(), None));
            continue;
        }
        // 3. Dangling.
        diags.push(
            Diagnostic::error(format!(
                "dep '{}' in crate '{}' ({}) references unknown crate '{}'",
                extern_name, krate.name, kind, dep.crate_name
            ))
            .with_optional_span(dep.span.clone().or_else(|| krate.span.clone())),
        );
        out.push((extern_name.clone(), None));
    }
    out
}

fn intern_pipelines(model: &mut BuildModel, diags: &mut Vec<Diagnostic>) {
    // For each pipeline, build a parallel Vec<Vec<Option<Handle>>>: outer
    // per stage, inner per input.
    let mut updates: Vec<PipelineInputResolution> = Vec::with_capacity(model.pipelines.len());

    for (handle, pipeline) in model.pipelines.iter() {
        let mut per_stage = Vec::with_capacity(pipeline.stages.len());
        for stage in &pipeline.stages {
            let mut resolved = Vec::with_capacity(stage.inputs.len());
            for input in &stage.inputs {
                let h = model.groups.lookup(input);
                if h.is_none() {
                    diags.push(Diagnostic::error(format!(
                        "pipeline '{}' stage '{}' references unknown group '{}'",
                        pipeline.name, stage.name, input
                    )));
                }
                resolved.push(h);
            }
            per_stage.push(resolved);
        }
        updates.push((handle, per_stage));
    }

    for (handle, per_stage) in updates {
        if let Some(pipeline) = model.pipelines.get_mut(handle) {
            for (stage, resolved) in pipeline.stages.iter_mut().zip(per_stage) {
                stage.inputs_handles = resolved;
            }
        }
    }
}

/// Resolve `EspEntry::source_crate` names to crate handles. Dangling
/// references push a diagnostic and leave the handle `None` — the DAG
/// builder will then skip that entry, so a missing crate name is
/// non-fatal (diagnostic-only) at this layer.
fn intern_esps(model: &mut BuildModel, diags: &mut Vec<Diagnostic>) {
    type EspResolution = (Handle<EspDef>, Vec<Option<Handle<CrateDef>>>);
    let mut updates: Vec<EspResolution> = Vec::with_capacity(model.esps.len());

    for (handle, esp) in model.esps.iter() {
        let mut resolved = Vec::with_capacity(esp.entries.len());
        for entry in &esp.entries {
            let h = model.crates.lookup(&entry.source_crate);
            if h.is_none() {
                diags.push(
                    Diagnostic::error(format!(
                        "esp(\"{}\").add: source crate '{}' does not exist in the model",
                        esp.name, entry.source_crate
                    ))
                    .with_optional_span(esp.span.clone()),
                );
            }
            resolved.push(h);
        }
        updates.push((handle, resolved));
    }

    for (handle, resolved) in updates {
        if let Some(esp) = model.esps.get_mut(handle) {
            for (entry, handle) in esp.entries.iter_mut().zip(resolved) {
                entry.source_crate_handle = handle;
            }
        }
    }
}
