//! Profile + config option resolution. See [`resolve`].

use crate::config::interpolate::interpolate;
use crate::error::{Diagnostic, Error, Result};
use gluon_model::{
    BuildModel, ConfigOptionDef, ConfigType, ConfigValue, CrateDef, Expr, Handle, ProfileDef,
    ResolvedConfig, ResolvedCrateRef, ResolvedProfile, ResolvedValue, TargetDef, TristateVal,
};
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::path::Path;

/// Maximum number of select/depends iteration passes before declaring
/// non-convergence (almost always indicates a misuse — well-formed
/// configurations converge in 2-3 iterations).
const MAX_FIXEDPOINT_ITERS: usize = 32;

/// Resolve a [`BuildModel`] into a [`ResolvedConfig`] for a specific profile.
///
/// - `profile_name`: the profile to build (e.g. `"default"`).
/// - `target_override`: if `Some`, overrides whatever target the profile
///   declared. Errors if the named target does not exist.
/// - `project_root`: used as the base for `build_dir` (always
///   `project_root.join("build")`).
/// - `option_overrides`: caller-provided per-option values, typically
///   loaded from `.gluon-config`. Applied **after** preset overrides but
///   **before** the selects/depends fixed point. `None` means "no
///   overrides; use the profile's preset (if any) and option defaults".
///
/// Returns a fully validated, flattened [`ResolvedConfig`] ready for the
/// scheduler. On any failure (missing profile, unknown target, range
/// violation, type mismatch, interpolation cycle, etc.) returns
/// `Err(Error::Diagnostics(...))` carrying every problem detected during
/// the pass — matching the intern/validate accumulation pattern.
pub fn resolve(
    model: &BuildModel,
    profile_name: &str,
    target_override: Option<&str>,
    project_root: &Path,
    option_overrides: Option<&BTreeMap<String, ConfigValue>>,
) -> Result<ResolvedConfig> {
    let mut diags: Vec<Diagnostic> = Vec::new();

    // 1. Look up the profile.
    let Some(profile_handle) = model.profiles.lookup(profile_name) else {
        return Err(Error::Diagnostics(vec![Diagnostic::error(format!(
            "unknown profile {profile_name:?}"
        ))]));
    };

    // 2. Walk inheritance and flatten.
    let flattened = match flatten_profile(model, profile_handle) {
        Ok(p) => p,
        Err(d) => {
            diags.push(d);
            return Err(Error::Diagnostics(diags));
        }
    };

    // 3. Resolve effective target.
    let effective_target = match target_override {
        Some(name) => match model.targets.lookup(name) {
            Some(h) => Some(h),
            None => {
                diags.push(Diagnostic::error(format!(
                    "--target override '{name}' does not match any target defined in the build model"
                )));
                None
            }
        },
        None => flattened.target_handle,
    };
    let Some(effective_target) = effective_target else {
        diags.push(Diagnostic::error(format!(
            "profile '{profile_name}' does not specify a target and no target override was provided"
        )));
        return Err(Error::Diagnostics(diags));
    };

    // 4. opt_level / debug_info / lto / boot_binary with defaults.
    let opt_level = flattened.opt_level.unwrap_or(0);
    if opt_level > 3 {
        diags.push(Diagnostic::error(format!(
            "profile '{profile_name}' opt_level {opt_level} out of range (expected 0..=3)"
        )));
    }
    let debug_info = flattened.debug_info.unwrap_or(false);
    let lto = flattened.lto.clone();
    let boot_binary = flattened.boot_binary_handle;

    // 5. Resolve config options (defaults → preset → overrides).
    let mut resolved_options: BTreeMap<String, ResolvedValue> = BTreeMap::new();
    let preset_overrides: Option<&BTreeMap<String, ConfigValue>> = flattened
        .preset
        .as_ref()
        .and_then(|name| model.presets.get(name))
        .map(|p| &p.overrides);

    for (name, opt) in &model.config_options {
        // Skip group structural options — they don't carry values.
        if matches!(opt.ty, ConfigType::Group) {
            continue;
        }

        let mut value =
            coerce(&opt.default, opt, name, &mut diags).unwrap_or_else(|| off_value_for(opt));

        if let Some(preset_map) = preset_overrides {
            if let Some(pv) = preset_map.get(name) {
                if let Some(coerced) = coerce_with_label(
                    pv,
                    opt,
                    name,
                    &mut diags,
                    &format!("preset '{}'", flattened.preset.as_deref().unwrap_or("?")),
                ) {
                    value = coerced;
                }
            }
        }

        if let Some(overrides) = option_overrides {
            if let Some(ov) = overrides.get(name) {
                if let Some(coerced) =
                    coerce_with_label(ov, opt, name, &mut diags, "config override")
                {
                    value = coerced;
                }
            }
        }

        resolved_options.insert(name.clone(), value);
    }

    // 6. Validate ranges.
    for (name, opt) in &model.config_options {
        let Some((lo, hi)) = opt.range else { continue };
        let Some(v) = resolved_options.get(name) else {
            continue;
        };
        let n: u64 = match v {
            ResolvedValue::U32(n) => *n as u64,
            ResolvedValue::U64(n) => *n,
            _ => continue,
        };
        if n < lo || n > hi {
            diags.push(Diagnostic::error(format!(
                "option '{name}' value {n} is outside declared range [{lo}, {hi}]"
            )));
        }
    }

    // 7. Validate choice values.
    for (name, opt) in &model.config_options {
        if !matches!(opt.ty, ConfigType::Choice) {
            continue;
        }
        let Some(choices) = &opt.choices else {
            continue;
        };
        let Some(ResolvedValue::Choice(picked)) = resolved_options.get(name) else {
            continue;
        };
        if !choices.iter().any(|c| c == picked) {
            diags.push(Diagnostic::error(format!(
                "option '{name}' value '{picked}' is not in declared choices [{}]",
                choices.join(", ")
            )));
        }
    }

    // 8. Selects/depends_on fixed point.
    let mut converged = false;
    for _ in 0..MAX_FIXEDPOINT_ITERS {
        let mut changed = false;

        // depends_on: any unsatisfied dependency forces this option off.
        // Collect names first to avoid mutation during iteration.
        let names: Vec<String> = model.config_options.keys().cloned().collect();
        for name in &names {
            let Some(opt) = model.config_options.get(name) else {
                continue;
            };

            // Single path: evaluate `depends_on_expr` if present. Both
            // declaration surfaces — the `.kconfig` loader and the Rhai
            // `.depends_on(...)` / `.depends_on_expr(...)` builders —
            // populate this Expr tree. For the common case of a plain
            // AND-of-idents (which is what `.depends_on([A, B])` and a
            // `.kconfig` `depends_on = A && B` both produce), we walk
            // the tree to name the first unsatisfied ident in the
            // diagnostic so the user isn't left staring at a generic
            // "expression not satisfied" message.
            if let Some(expr) = &opt.depends_on_expr {
                let lookup = |n: &str| resolved_options.get(n).map(is_on);
                if !expr.eval(&lookup) {
                    let off = off_value_for(opt);
                    let cur = resolved_options.get(name);
                    if cur != Some(&off) {
                        let diag = if let Some(dep_name) = first_unsatisfied_ident(expr, &lookup) {
                            Diagnostic::error(format!(
                                "option '{name}' disabled because dependency '{dep_name}' is not satisfied"
                            ))
                            .with_note("depends_on forces an option to its 'off' value when any dependency is off")
                        } else {
                            Diagnostic::error(format!(
                                "option '{name}' disabled because its 'depends_on' expression is not satisfied"
                            ))
                            .with_note("depends_on_expr evaluates with `&&`/`||`/`!` semantics; check that the referenced options are set to the values you expect")
                        };
                        diags.push(diag);
                        resolved_options.insert(name.clone(), off);
                        changed = true;
                    }
                }
            }
        }

        // selects: any "on" option forces its selects to "on".
        for name in &names {
            let Some(opt) = model.config_options.get(name) else {
                continue;
            };
            if opt.selects.is_empty() {
                continue;
            }
            let on = resolved_options.get(name).map(is_on).unwrap_or(false);
            if !on {
                continue;
            }
            for sel in &opt.selects {
                let Some(target_opt) = model.config_options.get(sel) else {
                    diags.push(Diagnostic::error(format!(
                        "option '{name}' selects unknown option '{sel}'"
                    )));
                    continue;
                };
                let on_value = on_value_for(target_opt);
                let cur = resolved_options.get(sel);
                if cur != Some(&on_value) {
                    resolved_options.insert(sel.clone(), on_value);
                    changed = true;
                }
            }
        }

        if !changed {
            converged = true;
            break;
        }
    }
    if !converged {
        diags.push(Diagnostic::error(
            "config option resolution did not converge (likely a selects cycle)",
        ));
    }

    // 9. String interpolation pass.
    let snapshot = resolved_options.clone();
    let mut updates: Vec<(String, ResolvedValue)> = Vec::new();
    for (name, value) in &resolved_options {
        let new = match value {
            ResolvedValue::String(s) if s.contains("${") => {
                let mut visiting = HashSet::new();
                visiting.insert(name.clone());
                match interpolate(s, &snapshot, &mut visiting) {
                    Ok(expanded) => Some(ResolvedValue::String(expanded)),
                    Err(e) => {
                        diags.push(Diagnostic::error(format!(
                            "while interpolating option '{name}': {e}"
                        )));
                        None
                    }
                }
            }
            ResolvedValue::Choice(s) if s.contains("${") => {
                let mut visiting = HashSet::new();
                visiting.insert(name.clone());
                match interpolate(s, &snapshot, &mut visiting) {
                    Ok(expanded) => Some(ResolvedValue::Choice(expanded)),
                    Err(e) => {
                        diags.push(Diagnostic::error(format!(
                            "while interpolating option '{name}': {e}"
                        )));
                        None
                    }
                }
            }
            _ => None,
        };
        if let Some(nv) = new {
            updates.push((name.clone(), nv));
        }
    }
    for (k, v) in updates {
        resolved_options.insert(k, v);
    }

    // 10. Resolve crate list. When boot_binary is set, include only crates
    //     reachable from the boot binary via deps + artifact_deps closure.
    //     This prevents wasteful compilation: a project with profile("x86")
    //     and profile("arm") will only compile crates relevant to the
    //     active profile's boot binary.
    //
    //     When boot_binary is None, fall back to including every crate
    //     (backward compat for simple single-target projects).
    //
    //     Host crates (target == "host") set `host: true` so the
    //     scheduler compiles them for the build machine. All other crates
    //     are cross and receive their group's resolved target handle.
    let reachable = boot_binary.map(|root| reachable_crates(model, root));
    let mut resolved_crates: Vec<ResolvedCrateRef> = Vec::new();
    for (crate_handle, krate) in model.crates.iter() {
        // Filter to reachable crates when boot_binary is set.
        if let Some(ref set) = reachable {
            if !set.contains(&crate_handle) {
                continue;
            }
        }
        let Some(group_h) = krate.group_handle else {
            continue;
        };
        let Some(group) = model.groups.get(group_h) else {
            continue;
        };
        let is_host = group.target == "host";
        // For host crates, the target field is just a placeholder — the
        // scheduler keys off `host: bool`. We use `effective_target` as a
        // safe placeholder so the field is always populated with a valid
        // arena handle.
        let target = if is_host {
            effective_target
        } else {
            krate.target_handle.unwrap_or(effective_target)
        };
        resolved_crates.push(ResolvedCrateRef {
            handle: crate_handle,
            target,
            host: is_host,
        });
    }

    if !diags.is_empty() {
        return Err(Error::Diagnostics(diags));
    }

    // 11. Build the final config.
    let project = model.project.clone().ok_or_else(|| {
        Error::Diagnostics(vec![Diagnostic::error(
            "build model has no project() declaration; cannot resolve",
        )])
    })?;

    Ok(ResolvedConfig {
        project,
        profile: ResolvedProfile {
            name: profile_name.to_string(),
            target: effective_target,
            opt_level,
            debug_info,
            lto,
            boot_binary,
            qemu_memory: flattened.qemu_memory,
            qemu_cores: flattened.qemu_cores,
            qemu_extra_args: flattened.qemu_extra_args.clone().unwrap_or_default(),
            test_timeout: flattened.test_timeout,
        },
        options: resolved_options,
        crates: resolved_crates,
        build_dir: project_root.join("build"),
        project_root: project_root.to_path_buf(),
    })
}

/// Walk the `inherits_handle` chain depth-first and produce a synthetic
/// [`ProfileDef`] where each `Option` field is filled by the nearest
/// descendant that set it.
///
/// The validate pass already detects inheritance cycles, but we keep a
/// defensive visited-set here so a buggy validator can't OOM the resolver.
fn flatten_profile(
    model: &BuildModel,
    start: Handle<ProfileDef>,
) -> std::result::Result<ProfileDef, Diagnostic> {
    let mut chain: Vec<Handle<ProfileDef>> = Vec::new();
    let mut visited: HashSet<u32> = HashSet::new();
    let mut cur = Some(start);
    while let Some(h) = cur {
        if !visited.insert(h.index()) {
            let name = model
                .profiles
                .get(h)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| format!("#{}", h.index()));
            return Err(Diagnostic::error(format!(
                "profile inheritance cycle detected at '{name}'"
            )));
        }
        chain.push(h);
        let Some(p) = model.profiles.get(h) else {
            break;
        };
        cur = p.inherits_handle;
    }

    // Walk from root → child so child fields overwrite parent fields.
    let mut acc = ProfileDef::default();
    for h in chain.into_iter().rev() {
        let Some(p) = model.profiles.get(h) else {
            continue;
        };
        if !p.name.is_empty() {
            acc.name = p.name.clone();
        }
        if p.target.is_some() {
            acc.target = p.target.clone();
            acc.target_handle = p.target_handle;
        }
        if p.opt_level.is_some() {
            acc.opt_level = p.opt_level;
        }
        if p.debug_info.is_some() {
            acc.debug_info = p.debug_info;
        }
        if p.lto.is_some() {
            acc.lto = p.lto.clone();
        }
        if p.boot_binary.is_some() {
            acc.boot_binary = p.boot_binary.clone();
            acc.boot_binary_handle = p.boot_binary_handle;
        }
        if p.preset.is_some() {
            acc.preset = p.preset.clone();
        }
        if p.qemu_memory.is_some() {
            acc.qemu_memory = p.qemu_memory;
        }
        if p.qemu_cores.is_some() {
            acc.qemu_cores = p.qemu_cores;
        }
        if p.qemu_extra_args.is_some() {
            acc.qemu_extra_args = p.qemu_extra_args.clone();
        }
        if p.test_timeout.is_some() {
            acc.test_timeout = p.test_timeout;
        }
        for (k, v) in &p.config {
            acc.config.insert(k.clone(), v.clone());
        }
    }
    Ok(acc)
}

/// Compute the transitive closure of crates reachable from `root` via
/// `deps` and `artifact_deps` edges. Used when a profile declares a
/// `boot_binary` to restrict the build to only the crates that are actually
/// needed — preventing wasteful cross-target compilation in multi-profile
/// projects.
///
/// Also expands ESP entries: if any source crate in an `EspDef` is reachable,
/// all source crates in that ESP are included (an ESP is an atomic unit that
/// must be assembled whole).
fn reachable_crates(model: &BuildModel, root: Handle<CrateDef>) -> BTreeSet<Handle<CrateDef>> {
    let mut visited = BTreeSet::new();
    let mut queue = VecDeque::new();
    visited.insert(root);
    queue.push_back(root);

    while let Some(h) = queue.pop_front() {
        let Some(krate) = model.crates.get(h) else {
            continue;
        };
        // Follow regular deps (--extern edges).
        for dep in krate.deps.values() {
            if let Some(dh) = dep.crate_handle {
                if visited.insert(dh) {
                    queue.push_back(dh);
                }
            }
        }
        // Follow artifact_deps (ordering-only edges, e.g. bootloader → kernel).
        for dep_name in &krate.artifact_deps {
            if let Some(dh) = model.crates.lookup(dep_name) {
                if visited.insert(dh) {
                    queue.push_back(dh);
                }
            }
        }
    }

    // ESP expansion: if any entry's source crate is reachable, pull in all
    // entries of that ESP. An ESP is assembled as a unit — partial inclusion
    // would produce a broken boot partition.
    for (_h, esp) in model.esps.iter() {
        let any_reachable = esp
            .entries
            .iter()
            .any(|e| e.source_crate_handle.is_some_and(|sh| visited.contains(&sh)));
        if any_reachable {
            for entry in &esp.entries {
                if let Some(sh) = entry.source_crate_handle {
                    // No need to BFS from these — ESP source crates are leaves
                    // in the dependency sense (they produce artifacts, not deps).
                    visited.insert(sh);
                }
            }
        }
    }

    visited
}

/// Coerce a [`ConfigValue`] into the [`ResolvedValue`] type required by an
/// option, returning `None` (and pushing a diagnostic) on type mismatch.
fn coerce(
    value: &ConfigValue,
    opt: &ConfigOptionDef,
    name: &str,
    diags: &mut Vec<Diagnostic>,
) -> Option<ResolvedValue> {
    coerce_with_label(value, opt, name, diags, "default")
}

fn coerce_with_label(
    value: &ConfigValue,
    opt: &ConfigOptionDef,
    name: &str,
    diags: &mut Vec<Diagnostic>,
    label: &str,
) -> Option<ResolvedValue> {
    match (opt.ty, value) {
        (ConfigType::Bool, ConfigValue::Bool(b)) => Some(ResolvedValue::Bool(*b)),
        (ConfigType::Tristate, ConfigValue::Tristate(t)) => Some(ResolvedValue::Tristate(*t)),
        // Tristate accepts a Bool too: true → Yes, false → No.
        (ConfigType::Tristate, ConfigValue::Bool(b)) => Some(ResolvedValue::Tristate(if *b {
            TristateVal::Yes
        } else {
            TristateVal::No
        })),
        (ConfigType::U32, ConfigValue::U32(n)) => Some(ResolvedValue::U32(*n)),
        (ConfigType::U32, ConfigValue::U64(n)) => {
            if *n <= u32::MAX as u64 {
                Some(ResolvedValue::U32(*n as u32))
            } else {
                diags.push(Diagnostic::error(format!(
                    "{label} sets option '{name}' to {n}, which overflows u32"
                )));
                None
            }
        }
        (ConfigType::U64, ConfigValue::U64(n)) => Some(ResolvedValue::U64(*n)),
        (ConfigType::U64, ConfigValue::U32(n)) => Some(ResolvedValue::U64(*n as u64)),
        (ConfigType::Str, ConfigValue::Str(s)) => Some(ResolvedValue::String(s.clone())),
        (ConfigType::Choice, ConfigValue::Choice(s)) => Some(ResolvedValue::Choice(s.clone())),
        (ConfigType::Choice, ConfigValue::Str(s)) => Some(ResolvedValue::Choice(s.clone())),
        (ConfigType::List, ConfigValue::List(items)) => Some(ResolvedValue::List(items.clone())),
        _ => {
            diags.push(Diagnostic::error(format!(
                "{label} sets option '{name}' to a value of type {} but the option is declared as {}",
                value_type_name(value),
                config_type_name(opt.ty),
            )));
            None
        }
    }
}

fn value_type_name(v: &ConfigValue) -> &'static str {
    match v {
        ConfigValue::Bool(_) => "bool",
        ConfigValue::Tristate(_) => "tristate",
        ConfigValue::U32(_) => "u32",
        ConfigValue::U64(_) => "u64",
        ConfigValue::Str(_) => "str",
        ConfigValue::Choice(_) => "choice",
        ConfigValue::List(_) => "list",
    }
}

fn config_type_name(t: ConfigType) -> &'static str {
    match t {
        ConfigType::Bool => "bool",
        ConfigType::Tristate => "tristate",
        ConfigType::U32 => "u32",
        ConfigType::U64 => "u64",
        ConfigType::Str => "str",
        ConfigType::Choice => "choice",
        ConfigType::List => "list",
        ConfigType::Group => "group",
    }
}

/// If `expr` is (recursively) an `And` of `Ident`s that evaluates false,
/// return the name of the first ident in left-to-right order whose
/// referenced option is off under `lookup`. For any more complex shape
/// (`Or`, `Not`, `Const`, or a mixed `And` containing non-ident
/// children), return `None` — the caller falls back to a generic
/// "expression not satisfied" diagnostic, since there is no single
/// "responsible" dependency to name.
///
/// This exists purely to preserve the legacy-path diagnostic wording
/// for the common case of `.depends_on([A, B])` — it is not load-bearing
/// for correctness.
fn first_unsatisfied_ident<F>(expr: &Expr, lookup: &F) -> Option<String>
where
    F: Fn(&str) -> Option<bool>,
{
    match expr {
        Expr::Ident(name) => {
            if lookup(name).unwrap_or(false) {
                None
            } else {
                Some(name.clone())
            }
        }
        Expr::And(xs) => {
            // Every child must also be Ident-only for the simple
            // diagnostic to apply; any `Or`/`Not`/`Const`/nested `And`
            // rejects the whole traversal so we fall back to the
            // generic message.
            for child in xs {
                match child {
                    Expr::Ident(name) => {
                        if !lookup(name).unwrap_or(false) {
                            return Some(name.clone());
                        }
                    }
                    _ => return None,
                }
            }
            None
        }
        _ => None,
    }
}

/// Returns true when a resolved value should be considered "on" for the
/// purposes of `selects`/`depends_on`. Tristate is treated as a bool:
/// only [`TristateVal::Yes`] counts as on; `Module` and `No` are off.
fn is_on(v: &ResolvedValue) -> bool {
    match v {
        ResolvedValue::Bool(b) => *b,
        ResolvedValue::Tristate(t) => matches!(t, TristateVal::Yes),
        ResolvedValue::U32(n) => *n != 0,
        ResolvedValue::U64(n) => *n != 0,
        ResolvedValue::String(s) => !s.is_empty(),
        ResolvedValue::Choice(s) => !s.is_empty(),
        ResolvedValue::List(items) => !items.is_empty(),
    }
}

/// The "off" value for a given option type. Used by `depends_on` to force
/// an option to its disabled state when a dependency is unsatisfied.
fn off_value_for(opt: &ConfigOptionDef) -> ResolvedValue {
    match opt.ty {
        ConfigType::Bool => ResolvedValue::Bool(false),
        ConfigType::Tristate => ResolvedValue::Tristate(TristateVal::No),
        ConfigType::U32 => ResolvedValue::U32(0),
        ConfigType::U64 => ResolvedValue::U64(0),
        ConfigType::Str => ResolvedValue::String(String::new()),
        ConfigType::Choice => ResolvedValue::Choice(String::new()),
        ConfigType::List => ResolvedValue::List(Vec::new()),
        ConfigType::Group => ResolvedValue::Bool(false),
    }
}

/// The "on" value used by `selects` to force a target option enabled.
/// For non-bool/tristate options, the existing default is left in place
/// where possible — for now we only need a sentinel "on" representation.
fn on_value_for(opt: &ConfigOptionDef) -> ResolvedValue {
    match opt.ty {
        ConfigType::Bool => ResolvedValue::Bool(true),
        ConfigType::Tristate => ResolvedValue::Tristate(TristateVal::Yes),
        // For numeric / string / list / choice types, "selects" pushes a
        // truthy sentinel: 1 for numeric, a single placeholder for string,
        // first choice for choice, single empty-marker for list. The
        // intended use of `selects` is bool/tristate; using it on a
        // numeric option is an unusual pattern, but we still produce a
        // value `is_on` will accept so the fixed point converges.
        ConfigType::U32 => ResolvedValue::U32(1),
        ConfigType::U64 => ResolvedValue::U64(1),
        ConfigType::Str => ResolvedValue::String("y".into()),
        ConfigType::Choice => opt
            .choices
            .as_ref()
            .and_then(|c| c.first())
            .map(|s| ResolvedValue::Choice(s.clone()))
            .unwrap_or_else(|| ResolvedValue::Choice("y".into())),
        ConfigType::List => ResolvedValue::List(vec!["y".into()]),
        ConfigType::Group => ResolvedValue::Bool(true),
    }
}

#[allow(unused)]
fn _crate_handle_marker(_: Handle<CrateDef>, _: Handle<TargetDef>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::evaluate_script;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_script(contents: &str) -> NamedTempFile {
        let mut f = tempfile::Builder::new()
            .prefix("gluon-resolve-test-")
            .suffix(".rhai")
            .tempfile()
            .expect("create temp file");
        f.write_all(contents.as_bytes()).expect("write");
        f.flush().expect("flush");
        f
    }

    fn resolve_default(model: &BuildModel) -> Result<ResolvedConfig> {
        resolve(model, "default", None, Path::new("/tmp/proj"), None)
    }

    #[test]
    fn resolve_basic_profile() {
        let f = write_script(
            r#"
            project("t","1");
            target("x", "t.json");
            profile("default").target("x").opt_level(2).debug_info(true);
            "#,
        );
        let model = evaluate_script(f.path()).expect("evaluate");
        let cfg = resolve_default(&model).expect("resolve");
        assert_eq!(cfg.profile.opt_level, 2);
        assert!(cfg.profile.debug_info);
        assert_eq!(cfg.profile.target, model.targets.lookup("x").unwrap());
        assert_eq!(cfg.build_dir, Path::new("/tmp/proj/build"));
    }

    #[test]
    fn resolve_profile_inheritance() {
        let f = write_script(
            r#"
            project("t","1");
            target("x", "t.json");
            profile("base").target("x").opt_level(2);
            profile("dev").inherits("base").debug_info(true);
            "#,
        );
        let model = evaluate_script(f.path()).expect("evaluate");
        let cfg = resolve(&model, "dev", None, Path::new("/tmp"), None).expect("resolve");
        assert_eq!(
            cfg.profile.opt_level, 2,
            "should inherit opt_level from base"
        );
        assert!(cfg.profile.debug_info, "child sets debug_info");
        assert_eq!(cfg.profile.target, model.targets.lookup("x").unwrap());
    }

    #[test]
    fn resolve_target_override() {
        let f = write_script(
            r#"
            project("t","1");
            target("a", "a.json");
            target("b", "b.json");
            profile("default").target("a");
            "#,
        );
        let model = evaluate_script(f.path()).expect("evaluate");
        let cfg = resolve(&model, "default", Some("b"), Path::new("/tmp"), None).expect("resolve");
        assert_eq!(cfg.profile.target, model.targets.lookup("b").unwrap());
    }

    #[test]
    fn resolve_unknown_profile_error() {
        let f = write_script(
            r#"
            project("t","1");
            target("x","t.json");
            profile("default").target("x");
            "#,
        );
        let model = evaluate_script(f.path()).expect("evaluate");
        let err =
            resolve(&model, "missing", None, Path::new("/tmp"), None).expect_err("should fail");
        match err {
            Error::Diagnostics(v) => {
                assert!(v.iter().any(|d| d.message.contains("missing")));
            }
            other => panic!("expected Diagnostics, got {other:?}"),
        }
    }

    #[test]
    fn resolve_profile_without_target_error() {
        let f = write_script(
            r#"
            project("t","1");
            profile("default").opt_level(0);
            "#,
        );
        let model = evaluate_script(f.path()).expect("evaluate");
        let err = resolve_default(&model).expect_err("should fail");
        match err {
            Error::Diagnostics(v) => {
                assert!(
                    v.iter()
                        .any(|d| d.message.contains("does not specify a target"))
                );
            }
            other => panic!("expected Diagnostics, got {other:?}"),
        }
    }

    #[test]
    fn resolve_config_defaults() {
        let f = write_script(
            r#"
            project("t","1");
            target("x", "t.json");
            profile("default").target("x");
            config_u32("SIZE").default_value(4096);
            config_bool("DEBUG").default_value(true);
            "#,
        );
        let model = evaluate_script(f.path()).expect("evaluate");
        let cfg = resolve_default(&model).expect("resolve");
        assert_eq!(cfg.options.get("SIZE"), Some(&ResolvedValue::U32(4096)));
        assert_eq!(cfg.options.get("DEBUG"), Some(&ResolvedValue::Bool(true)));
    }

    #[test]
    fn resolve_config_preset_override() {
        let f = write_script(
            r#"
            project("t","1");
            target("x","t.json");
            config_u32("SIZE").default_value(4096);
            preset("big").set("SIZE", 8192);
            profile("default").target("x").preset("big");
            "#,
        );
        let model = evaluate_script(f.path()).expect("evaluate");
        let cfg = resolve_default(&model).expect("resolve");
        // Note: preset.set with an integer literal lands as U64, but the
        // option is declared U32; coercion narrows it.
        assert_eq!(cfg.options.get("SIZE"), Some(&ResolvedValue::U32(8192)));
    }

    #[test]
    fn resolve_config_external_override() {
        let f = write_script(
            r#"
            project("t","1");
            target("x","t.json");
            config_u32("SIZE").default_value(4096);
            preset("big").set("SIZE", 8192);
            profile("default").target("x").preset("big");
            "#,
        );
        let model = evaluate_script(f.path()).expect("evaluate");
        let mut overrides = BTreeMap::new();
        overrides.insert("SIZE".to_string(), ConfigValue::U32(16384));
        let cfg =
            resolve(&model, "default", None, Path::new("/tmp"), Some(&overrides)).expect("resolve");
        assert_eq!(cfg.options.get("SIZE"), Some(&ResolvedValue::U32(16384)));
    }

    #[test]
    fn resolve_config_range_violation() {
        let f = write_script(
            r#"
            project("t","1");
            target("x","t.json");
            profile("default").target("x");
            config_u32("SIZE").default_value(999999).range(0, 65536);
            "#,
        );
        let model = evaluate_script(f.path()).expect("evaluate");
        let err = resolve_default(&model).expect_err("should fail");
        match err {
            Error::Diagnostics(v) => {
                assert!(
                    v.iter()
                        .any(|d| d.message.contains("outside declared range")),
                    "expected range diagnostic, got: {v:?}"
                );
            }
            other => panic!("expected Diagnostics, got {other:?}"),
        }
    }

    /// Helper for the K8 expression-form depends_on tests: write a
    /// `gluon.rhai` + sibling `options.kconfig` into a tempdir, evaluate
    /// the script, and resolve the default profile. Returns the
    /// resolved config so each test can assert on the option states.
    ///
    /// The .kconfig path is what populates `depends_on_expr` — the Rhai
    /// builder still produces only `Vec<String>` form. So this helper
    /// is the only way to drive the new resolver branch end-to-end.
    fn resolve_kconfig_pair(rhai: &str, kconfig: &str) -> Result<ResolvedConfig> {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("gluon.rhai");
        std::fs::write(&script, rhai).expect("write rhai");
        std::fs::write(dir.path().join("options.kconfig"), kconfig).expect("write kconfig");
        let model = evaluate_script(&script).expect("evaluate");
        resolve(&model, "default", None, dir.path(), None)
    }

    #[test]
    fn resolve_config_depends_on_expr_and_satisfied() {
        // A && B with both A and B on → X stays at its default (true).
        let cfg = resolve_kconfig_pair(
            r#"
            project("t","1");
            target("x","t.json");
            profile("default").target("x");
            load_kconfig("./options.kconfig");
            "#,
            r#"
            config A: bool { default = true }
            config B: bool { default = true }
            config X: bool { default = true depends_on = A && B }
            "#,
        )
        .expect("resolve");
        assert_eq!(cfg.options.get("X"), Some(&ResolvedValue::Bool(true)));
    }

    #[test]
    fn resolve_config_depends_on_expr_and_unsatisfied() {
        // A && B but B is off → X must be forced off.
        let cfg = resolve_kconfig_pair(
            r#"
            project("t","1");
            target("x","t.json");
            profile("default").target("x");
            load_kconfig("./options.kconfig");
            "#,
            r#"
            config A: bool { default = true }
            config B: bool { default = false }
            config X: bool { default = true depends_on = A && B }
            "#,
        );
        // The resolver pushes a diagnostic on the disable, so the
        // result is Err with a Diagnostics payload — not a hard
        // failure, but the resolver still flips X off in the model.
        // The test just confirms the diagnostic appears.
        //
        // For a plain `And(Ident, Ident)` expression the collapsed
        // resolver walks the tree to name the first unsatisfied ident
        // (B), matching the legacy dep-name wording. The generic
        // "expression is not satisfied" phrasing is reserved for shapes
        // `first_unsatisfied_ident` can't simplify (`Or`, `Not`, mixed).
        match cfg {
            Err(Error::Diagnostics(v)) => {
                assert!(
                    v.iter().any(|d| d.message.contains("dependency 'B'")),
                    "expected diagnostic naming B as the unsatisfied dep, got: {v:?}"
                );
            }
            other => panic!("expected Diagnostics about depends_on expression, got {other:?}"),
        }
    }

    #[test]
    fn resolve_config_depends_on_expr_or_unsatisfied_uses_generic_message() {
        // For `A || B` with both A and B off, the expression doesn't
        // reduce to a single responsible dep, so the resolver falls
        // back to the generic "expression is not satisfied" message.
        let cfg = resolve_kconfig_pair(
            r#"
            project("t","1");
            target("x","t.json");
            profile("default").target("x");
            load_kconfig("./options.kconfig");
            "#,
            r#"
            config A: bool { default = false }
            config B: bool { default = false }
            config X: bool { default = true depends_on = A || B }
            "#,
        );
        match cfg {
            Err(Error::Diagnostics(v)) => {
                assert!(
                    v.iter()
                        .any(|d| d.message.contains("'depends_on' expression")),
                    "expected generic expression-not-satisfied diagnostic, got: {v:?}"
                );
            }
            other => panic!("expected Diagnostics, got {other:?}"),
        }
    }

    #[test]
    fn resolve_config_depends_on_expr_or_one_satisfied() {
        // A || B with only A on → X stays satisfied. This is the case
        // where flatten-based eval would WRONGLY disable X (because B
        // is off and the flat form treats it as required).
        let cfg = resolve_kconfig_pair(
            r#"
            project("t","1");
            target("x","t.json");
            profile("default").target("x");
            load_kconfig("./options.kconfig");
            "#,
            r#"
            config A: bool { default = true }
            config B: bool { default = false }
            config X: bool { default = true depends_on = A || B }
            "#,
        )
        .expect("resolve");
        assert_eq!(cfg.options.get("X"), Some(&ResolvedValue::Bool(true)));
    }

    #[test]
    fn resolve_config_depends_on_expr_not_inverts() {
        // !DISABLED with DISABLED off → expression is true → X enabled.
        let cfg = resolve_kconfig_pair(
            r#"
            project("t","1");
            target("x","t.json");
            profile("default").target("x");
            load_kconfig("./options.kconfig");
            "#,
            r#"
            config DISABLED: bool { default = false }
            config X: bool { default = true depends_on = !DISABLED }
            "#,
        )
        .expect("resolve");
        assert_eq!(cfg.options.get("X"), Some(&ResolvedValue::Bool(true)));
    }

    #[test]
    fn resolve_config_selects() {
        let f = write_script(
            r#"
            project("t","1");
            target("x","t.json");
            profile("default").target("x");
            config_bool("A").default_value(true).selects(["B"]);
            config_bool("B").default_value(false);
            "#,
        );
        let model = evaluate_script(f.path()).expect("evaluate");
        let cfg = resolve_default(&model).expect("resolve");
        assert_eq!(cfg.options.get("A"), Some(&ResolvedValue::Bool(true)));
        assert_eq!(
            cfg.options.get("B"),
            Some(&ResolvedValue::Bool(true)),
            "B should be forced on by A's selects"
        );
    }

    #[test]
    fn resolve_interpolation() {
        let f = write_script(
            r#"
            project("t","1");
            target("x","t.json");
            profile("default").target("x");
            config_str("NAME").default_value("foo");
            config_str("MSG").default_value("hello ${NAME}");
            "#,
        );
        let model = evaluate_script(f.path()).expect("evaluate");
        let cfg = resolve_default(&model).expect("resolve");
        assert_eq!(
            cfg.options.get("MSG"),
            Some(&ResolvedValue::String("hello foo".into()))
        );
    }

    #[test]
    fn resolve_interpolation_cycle_is_error() {
        let f = write_script(
            r#"
            project("t","1");
            target("x","t.json");
            profile("default").target("x");
            config_str("A").default_value("${B}");
            config_str("B").default_value("${A}");
            "#,
        );
        let model = evaluate_script(f.path()).expect("evaluate");
        let err = resolve_default(&model).expect_err("should fail");
        match err {
            Error::Diagnostics(v) => {
                assert!(
                    v.iter().any(|d| d.message.contains("cycle")),
                    "expected cycle diagnostic, got: {v:?}"
                );
            }
            other => panic!("expected Diagnostics, got {other:?}"),
        }
    }

    #[test]
    fn resolve_crates_list() {
        let f = write_script(
            r#"
            project("t","1");
            target("x","t.json");
            profile("default").target("x");
            let k = group("kernel").target("x");
            k.add("kfoo", "crates/kfoo");
            let h = group("tools").target("host");
            h.add("hbar", "crates/hbar");
            "#,
        );
        let model = evaluate_script(f.path()).expect("evaluate");
        let cfg = resolve_default(&model).expect("resolve");
        let names: Vec<&str> = cfg
            .crates
            .iter()
            .map(|c| model.crates.get(c.handle).unwrap().name.as_str())
            .collect();
        assert!(
            names.contains(&"kfoo"),
            "kernel crate present, got {names:?}"
        );
        assert!(names.contains(&"hbar"), "host crate present, got {names:?}");
        let host_entry = cfg
            .crates
            .iter()
            .find(|c| model.crates.get(c.handle).unwrap().name == "hbar")
            .unwrap();
        assert!(host_entry.host, "host crate flagged correctly");
    }

    // -----------------------------------------------------------------------
    // Per-profile crate filtering (reachable_crates)
    // -----------------------------------------------------------------------

    /// Helper: resolve a named profile and return the set of crate names.
    fn crate_names_for_profile(
        model: &BuildModel,
        profile: &str,
    ) -> Vec<String> {
        let cfg = resolve(model, profile, None, Path::new("/tmp/proj"), None)
            .expect("resolve");
        cfg.crates
            .iter()
            .map(|c| model.crates.get(c.handle).unwrap().name.clone())
            .collect()
    }

    #[test]
    fn filter_no_boot_binary_includes_all() {
        let f = write_script(
            r#"
            project("t","1");
            target("x","t.json");
            target("y","y.json");
            profile("default").target("x");
            let g1 = group("a").target("x");
            g1.add("crate_a", "crates/a");
            let g2 = group("b").target("y");
            g2.add("crate_b", "crates/b");
            "#,
        );
        let model = evaluate_script(f.path()).expect("evaluate");
        let names = crate_names_for_profile(&model, "default");
        assert!(names.contains(&"crate_a".into()), "got {names:?}");
        assert!(
            names.contains(&"crate_b".into()),
            "no boot_binary → all crates included, got {names:?}"
        );
    }

    #[test]
    fn filter_boot_binary_restricts_to_reachable() {
        let f = write_script(
            r#"
            project("t","1");
            target("x","t.json");
            target("y","y.json");
            profile("default").target("x").boot_binary("crate_a");
            let g1 = group("a").target("x");
            g1.add("crate_a", "crates/a").crate_type("bin");
            let g2 = group("b").target("y");
            g2.add("crate_b", "crates/b").crate_type("bin");
            "#,
        );
        let model = evaluate_script(f.path()).expect("evaluate");
        let names = crate_names_for_profile(&model, "default");
        assert!(names.contains(&"crate_a".into()), "boot_binary is reachable");
        assert!(
            !names.contains(&"crate_b".into()),
            "crate_b is unreachable from crate_a, got {names:?}"
        );
    }

    #[test]
    fn filter_artifact_deps_cross_target_included() {
        // Bootloader on target Y depends on kernel on target X via artifact_env.
        let f = write_script(
            r#"
            project("t","1");
            target("x","t.json");
            target("y","y.json");
            profile("default").target("y").boot_binary("bootloader");
            let g1 = group("kernel").target("x");
            g1.add("kernel", "crates/kernel").crate_type("bin");
            let g2 = group("boot").target("y");
            g2.add("bootloader", "crates/boot")
                .crate_type("bin")
                .artifact_env("KERNEL_PATH", "kernel");
            "#,
        );
        let model = evaluate_script(f.path()).expect("evaluate");
        let names = crate_names_for_profile(&model, "default");
        assert!(
            names.contains(&"bootloader".into()),
            "boot_binary itself, got {names:?}"
        );
        assert!(
            names.contains(&"kernel".into()),
            "kernel reachable via artifact_deps, got {names:?}"
        );
    }

    #[test]
    fn filter_deps_host_crate_included() {
        // Cross crate depends on a host proc-macro — both should be included.
        let f = write_script(
            r#"
            project("t","1");
            target("x","t.json");
            profile("default").target("x").boot_binary("kernel");
            let h = group("host_tools").target("host");
            h.add("my_derive", "crates/derive").crate_type("proc-macro");
            let k = group("kernel").target("x");
            k.add("kernel", "crates/kernel")
                .crate_type("bin")
                .deps(#{ my_derive: #{ crate: "my_derive" } });
            "#,
        );
        let model = evaluate_script(f.path()).expect("evaluate");
        let names = crate_names_for_profile(&model, "default");
        assert!(names.contains(&"kernel".into()), "got {names:?}");
        assert!(
            names.contains(&"my_derive".into()),
            "host crate reachable via deps, got {names:?}"
        );
    }

    #[test]
    fn filter_two_profiles_disjoint_sets() {
        // Two profiles with different boot_binary should produce disjoint crate sets.
        let f = write_script(
            r#"
            project("t","1");
            target("x","t.json");
            target("y","y.json");
            profile("prof_x").target("x").boot_binary("crate_x");
            profile("prof_y").target("y").boot_binary("crate_y");
            let g1 = group("gx").target("x");
            g1.add("crate_x", "crates/x").crate_type("bin");
            let g2 = group("gy").target("y");
            g2.add("crate_y", "crates/y").crate_type("bin");
            "#,
        );
        let model = evaluate_script(f.path()).expect("evaluate");
        let names_x = crate_names_for_profile(&model, "prof_x");
        let names_y = crate_names_for_profile(&model, "prof_y");
        assert_eq!(names_x, vec!["crate_x"], "prof_x only builds crate_x");
        assert_eq!(names_y, vec!["crate_y"], "prof_y only builds crate_y");
    }

    #[test]
    fn filter_esp_expansion() {
        // ESP source crate is pulled in even if not directly reachable,
        // as long as another entry in the same ESP is reachable.
        let f = write_script(
            r#"
            project("t","1");
            target("x","t.json");
            target("y","y.json");
            profile("default").target("y").boot_binary("bootloader");
            let g1 = group("kernel").target("x");
            g1.add("kernel", "crates/kernel").crate_type("bin");
            let g2 = group("boot").target("y");
            g2.add("bootloader", "crates/boot")
                .crate_type("bin")
                .artifact_env("KERNEL_PATH", "kernel");
            esp("default")
                .add("bootloader", "EFI/BOOT/BOOTX64.EFI");
            "#,
        );
        let model = evaluate_script(f.path()).expect("evaluate");
        let names = crate_names_for_profile(&model, "default");
        assert!(
            names.contains(&"bootloader".into()),
            "ESP source crate included, got {names:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Rhai builder tests for BootloaderDef and ImageDef
    // -----------------------------------------------------------------------

    #[test]
    fn bootloader_builder_populates_typed_fields() {
        let f = write_script(
            r#"
            project("t","1");
            target("x","t.json");
            profile("default").target("x");
            bootloader("uefi")
                .entry_crate("boot")
                .protocol("gop");
            "#,
        );
        let model = evaluate_script(f.path()).expect("evaluate");
        assert_eq!(model.bootloader.kind, "uefi");
        assert_eq!(model.bootloader.entry_crate.as_deref(), Some("boot"));
        assert_eq!(model.bootloader.protocol.as_deref(), Some("gop"));
    }

    #[test]
    fn image_builder_populates_model() {
        let f = write_script(
            r#"
            project("t","1");
            target("x","t.json");
            profile("default").target("x");
            image("disk")
                .format("fat32")
                .size(64)
                .add_crate("bootloader", "EFI/BOOT/BOOTX64.EFI")
                .add_file("splash.bmp", "boot/splash.bmp")
                .add_esp("default", "/");
            "#,
        );
        let model = evaluate_script(f.path()).expect("evaluate");
        let h = model.images.lookup("disk").expect("image must exist");
        let img = model.images.get(h).unwrap();
        assert_eq!(img.format.as_deref(), Some("fat32"));
        assert_eq!(img.size_mb, Some(64));
        assert_eq!(img.entries.len(), 3);
    }
}
