//! `rust-project.json` generation for rust-analyzer.
//!
//! [`generate_rust_project_json`] is a pure function: it consumes a
//! [`BuildModel`] + [`ResolvedConfig`] + [`BuildLayout`] + [`RustcInfo`]
//! and produces a [`serde_json::Value`] in rust-analyzer's
//! `rust-project.json` format. The caller (typically
//! [`crate::configure`]) is responsible for serialising and writing it
//! to disk.
//!
//! ### Why pure?
//!
//! Keeping the generator I/O-free makes it trivially unit-testable
//! against hand-built fixtures and lets non-CLI embedders (LSPs, editor
//! plugins) reuse the same JSON shape without paying the cost of
//! routing through the filesystem.
//!
//! ### Schema reference
//!
//! See <https://rust-analyzer.github.io/manual.html#non-cargo-based-projects>.
//! The fields populated here are the minimum subset rust-analyzer needs
//! to index a non-cargo project: `sysroot_src` (optional) and `crates`
//! (each with `root_module`, `edition`, `deps`, `cfg`, `env`,
//! `is_workspace_member`, `is_proc_macro`, `source`).

use crate::compile::{BuildLayout, RustcInfo};
use gluon_model::{BuildModel, CrateDef, CrateType, Handle, ResolvedConfig};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Generate a `rust-project.json` value for the given build.
///
/// The output always includes one entry per [`gluon_model::ResolvedCrateRef`]
/// in `resolved.crates` (host and cross alike) **plus** an entry for the
/// generated `<project>_config` crate. The config crate is emitted first
/// so its index in the `crates` array is fixed at `0` — downstream cross
/// crates that link against it can reference it by that stable index.
///
/// `sysroot_src` is omitted entirely (rather than set to `null`) when
/// [`RustcInfo::rust_src`] is `None`. rust-analyzer treats absence and
/// `null` differently: absence triggers its built-in fallback, while
/// `null` is a hard error in some versions.
pub fn generate_rust_project_json(
    model: &BuildModel,
    resolved: &ResolvedConfig,
    layout: &BuildLayout,
    rustc_info: &RustcInfo,
) -> Value {
    // Build the crate list in two passes: first assign indices so we
    // know each crate's position before populating its `deps` array,
    // then walk again to emit the JSON entries with resolved deps.
    //
    // Index 0 is reserved for the generated config crate. The first
    // entry in `resolved.crates` therefore lands at index 1.

    const CONFIG_CRATE_INDEX: usize = 0;

    // Pass 1: build the handle → index map.
    //
    // We dedupe by handle (not by name) because host and cross crates
    // may legitimately share a name but must remain distinct rust-
    // analyzer entries.
    let mut handle_to_index: BTreeMap<Handle<CrateDef>, usize> = BTreeMap::new();
    for (i, cref) in resolved.crates.iter().enumerate() {
        // The first .insert call wins. Subsequent ResolvedCrateRefs
        // with the same handle (which would be a resolver bug) are
        // silently coalesced rather than producing duplicate JSON
        // entries.
        handle_to_index.entry(cref.handle).or_insert(i + 1);
    }

    // Emit the config crate at index 0.
    let config_crate_dir = layout.generated_config_crate_dir();
    let config_crate_entry = json!({
        "root_module": config_crate_dir.join("src").join("lib.rs").to_string_lossy(),
        "edition": "2021",
        "deps": [],
        "cfg": [],
        "env": {},
        "is_workspace_member": true,
        "is_proc_macro": false,
        "source": {
            "include_dirs": [config_crate_dir.to_string_lossy()],
            "exclude_dirs": [layout.root().to_string_lossy()],
        },
    });

    let mut crates_array: Vec<Value> = Vec::with_capacity(resolved.crates.len() + 1);
    crates_array.push(config_crate_entry);

    // Pass 2: emit each ResolvedCrateRef as a JSON entry.
    for cref in &resolved.crates {
        let krate: &CrateDef = match model.crates.get(cref.handle) {
            Some(k) => k,
            // Defensive: a ResolvedCrateRef with no matching CrateDef
            // is a resolver bug, not an analyzer bug. Skip it rather
            // than panicking — the user's rust-analyzer view will be
            // partially broken but other crates will still be indexed.
            None => continue,
        };

        let crate_dir = resolved.project_root.join(&krate.path);
        let root_module = if let Some(root) = &krate.root {
            crate_dir.join(root)
        } else {
            match krate.crate_type {
                CrateType::Bin => crate_dir.join("src").join("main.rs"),
                _ => crate_dir.join("src").join("lib.rs"),
            }
        };

        let edition = if krate.edition.is_empty() {
            "2021"
        } else {
            krate.edition.as_str()
        };

        // --- deps ---
        //
        // The map key in `CrateDef::deps` is the *extern* name (i.e.
        // the identifier the consuming crate uses in `extern crate
        // foo;` or `use foo::...`), which may differ from the target
        // crate's own name. We propagate the key, not the target's
        // CrateDef.name.
        //
        // Cross crates also depend on the generated config crate (the
        // DAG mirrors this — see scheduler::dag::build_dag rule 2).
        // Host crates do not.
        let mut deps: Vec<Value> = Vec::new();
        if !cref.host {
            deps.push(json!({
                "name": "config",
                "crate": CONFIG_CRATE_INDEX,
            }));
        }
        for (extern_name, dep) in &krate.deps {
            let Some(target_handle) = dep.crate_handle else {
                // Unresolved dep (resolver did not assign a handle) —
                // probably a vendor dep, out of scope for MVP-M.
                continue;
            };
            let Some(&idx) = handle_to_index.get(&target_handle) else {
                // Resolved handle but the dep is not part of the
                // current build (e.g. vendored crate not in
                // resolved.crates). Skip it — see plan §6 landmines.
                continue;
            };
            deps.push(json!({
                "name": extern_name,
                "crate": idx,
            }));
        }

        // --- cfg ---
        //
        // We emit the crate's own raw `cfg_flags` verbatim. For cross
        // crates we *could* also derive `target_arch`, `target_os`,
        // `target_pointer_width` from `TargetDef::spec`, but that
        // requires either parsing a custom-target spec JSON file or
        // shelling out to `rustc --print=cfg --target=...`. Both are
        // out of scope for the MVP-M analyzer; rust-analyzer will
        // still index the crate, just without target-specific cfgs.
        // TODO: derive target cfgs once a stable spec parser exists.
        let cfg: Vec<String> = krate.cfg_flags.clone();

        let entry = json!({
            "root_module": root_module.to_string_lossy(),
            "edition": edition,
            "deps": deps,
            "cfg": cfg,
            "env": {},
            "is_workspace_member": true,
            "is_proc_macro": krate.crate_type == CrateType::ProcMacro,
            "source": {
                "include_dirs": [crate_dir.to_string_lossy()],
                "exclude_dirs": [layout.root().to_string_lossy()],
            },
        });
        crates_array.push(entry);
    }

    let mut top = serde_json::Map::new();
    if let Some(rust_src) = &rustc_info.rust_src {
        top.insert(
            "sysroot_src".to_string(),
            Value::String(rust_src.to_string_lossy().into_owned()),
        );
    }
    top.insert("crates".to_string(), Value::Array(crates_array));
    Value::Object(top)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gluon_model::{
        BuildModel, CrateDef, CrateType, DepDef, ProjectDef, ResolvedConfig, ResolvedCrateRef,
        ResolvedProfile, TargetDef,
    };
    use std::path::PathBuf;
    use std::sync::Arc;

    fn fake_rustc_info(rust_src: Option<PathBuf>) -> RustcInfo {
        RustcInfo {
            rustc_path: PathBuf::from("/usr/bin/rustc"),
            rustc_arg: "rustc".into(),
            version: "rustc 0.0.0 (test)".into(),
            host_triple: "x86_64-unknown-linux-gnu".into(),
            commit_hash: None,
            release: "0.0.0".into(),
            sysroot: PathBuf::from("/fake-sysroot"),
            rust_src,
            mtime_ns: 0,
        }
    }

    fn make_fixture() -> (BuildModel, Handle<TargetDef>) {
        let mut model = BuildModel::default();
        let (target_handle, _) = model.targets.insert(
            "x86_64-unknown-none".into(),
            TargetDef {
                name: "x86_64-unknown-none".into(),
                spec: "x86_64-unknown-none".into(),
                builtin: true,
                panic_strategy: Some("abort".into()),
                span: None,
            },
        );
        (model, target_handle)
    }

    fn make_resolved(
        target_handle: Handle<TargetDef>,
        refs: Vec<ResolvedCrateRef>,
    ) -> ResolvedConfig {
        ResolvedConfig {
            project: ProjectDef {
                name: "demo".into(),
                version: "0.1.0".into(),
                config_crate_name: None,
                cfg_prefix: None,
                config_override_file: None,
                default_profile: None,
            },
            profile: ResolvedProfile {
                name: "dev".into(),
                target: target_handle,
                opt_level: 0,
                debug_info: false,
                lto: None,
                boot_binary: None,
                qemu_memory: None,
                qemu_cores: None,
                qemu_extra_args: Vec::new(),
                test_timeout: None,
            },
            options: BTreeMap::new(),
            crates: refs,
            build_dir: "/tmp/build".into(),
            project_root: "/tmp/proj".into(),
        }
    }

    fn layout() -> BuildLayout {
        BuildLayout::new("/tmp/build", "demo")
    }

    #[test]
    fn single_cross_crate_emits_config_plus_one() {
        let (mut model, th) = make_fixture();
        let (ch, _) = model.crates.insert(
            "kernel".into(),
            CrateDef {
                name: "kernel".into(),
                path: "crates/kernel".into(),
                edition: "2021".into(),
                crate_type: CrateType::Bin,
                ..Default::default()
            },
        );
        let resolved = make_resolved(
            th,
            vec![ResolvedCrateRef {
                handle: ch,
                target: th,
                host: false,
            }],
        );
        let info = Arc::new(fake_rustc_info(Some(PathBuf::from("/rust-src"))));
        let json = generate_rust_project_json(&model, &resolved, &layout(), &info);

        let crates = json["crates"].as_array().expect("crates array");
        assert_eq!(crates.len(), 2, "config + 1 user crate");
        // Index 0 is the config crate.
        assert!(
            crates[0]["root_module"]
                .as_str()
                .unwrap()
                .contains("demo_config"),
            "first entry must be config crate, got: {:?}",
            crates[0]
        );
        // Cross crate has a dep on the config crate at index 0.
        let kernel = &crates[1];
        assert_eq!(kernel["is_proc_macro"], false);
        let deps = kernel["deps"].as_array().unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0]["crate"], 0);
        assert_eq!(deps[0]["name"], "config");
        // Bin → main.rs.
        assert!(
            kernel["root_module"]
                .as_str()
                .unwrap()
                .ends_with("crates/kernel/src/main.rs")
        );
    }

    #[test]
    fn single_host_crate_has_no_config_dep() {
        let (mut model, th) = make_fixture();
        let (ch, _) = model.crates.insert(
            "build-tool".into(),
            CrateDef {
                name: "build-tool".into(),
                path: "crates/build-tool".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                ..Default::default()
            },
        );
        let resolved = make_resolved(
            th,
            vec![ResolvedCrateRef {
                handle: ch,
                target: th,
                host: true,
            }],
        );
        let info = Arc::new(fake_rustc_info(Some(PathBuf::from("/rust-src"))));
        let json = generate_rust_project_json(&model, &resolved, &layout(), &info);

        let crates = json["crates"].as_array().unwrap();
        assert_eq!(crates.len(), 2);
        let host = &crates[1];
        assert_eq!(host["deps"].as_array().unwrap().len(), 0);
        assert!(
            host["root_module"]
                .as_str()
                .unwrap()
                .ends_with("crates/build-tool/src/lib.rs")
        );
    }

    #[test]
    fn mixed_host_and_cross_both_appear() {
        let (mut model, th) = make_fixture();
        let (host_h, _) = model.crates.insert(
            "host-tool".into(),
            CrateDef {
                name: "host-tool".into(),
                path: "crates/host-tool".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                ..Default::default()
            },
        );
        let (cross_h, _) = model.crates.insert(
            "kernel".into(),
            CrateDef {
                name: "kernel".into(),
                path: "crates/kernel".into(),
                edition: "2021".into(),
                crate_type: CrateType::Bin,
                ..Default::default()
            },
        );
        let resolved = make_resolved(
            th,
            vec![
                ResolvedCrateRef {
                    handle: host_h,
                    target: th,
                    host: true,
                },
                ResolvedCrateRef {
                    handle: cross_h,
                    target: th,
                    host: false,
                },
            ],
        );
        let info = Arc::new(fake_rustc_info(Some(PathBuf::from("/rust-src"))));
        let json = generate_rust_project_json(&model, &resolved, &layout(), &info);
        let crates = json["crates"].as_array().unwrap();
        assert_eq!(crates.len(), 3, "config + host + cross");
        assert_eq!(crates[1]["deps"].as_array().unwrap().len(), 0); // host
        assert_eq!(crates[2]["deps"].as_array().unwrap().len(), 1); // cross
    }

    #[test]
    fn proc_macro_crate_marked_correctly() {
        let (mut model, th) = make_fixture();
        let (ch, _) = model.crates.insert(
            "macros".into(),
            CrateDef {
                name: "macros".into(),
                path: "crates/macros".into(),
                edition: "2021".into(),
                crate_type: CrateType::ProcMacro,
                ..Default::default()
            },
        );
        let resolved = make_resolved(
            th,
            vec![ResolvedCrateRef {
                handle: ch,
                target: th,
                host: true,
            }],
        );
        let info = Arc::new(fake_rustc_info(None));
        let json = generate_rust_project_json(&model, &resolved, &layout(), &info);
        assert_eq!(json["crates"][1]["is_proc_macro"], true);
    }

    #[test]
    fn missing_rust_src_omits_sysroot_src_key() {
        let (model, _th) = make_fixture();
        let resolved = make_resolved(_th, Vec::new());
        let info = Arc::new(fake_rustc_info(None));
        let json = generate_rust_project_json(&model, &resolved, &layout(), &info);
        assert!(
            json.get("sysroot_src").is_none(),
            "sysroot_src must be absent (not null) when rust_src is None, got: {json}"
        );
    }

    #[test]
    fn rust_src_present_emits_sysroot_src_key() {
        let (model, _th) = make_fixture();
        let resolved = make_resolved(_th, Vec::new());
        let info = Arc::new(fake_rustc_info(Some(PathBuf::from("/path/to/rust-src"))));
        let json = generate_rust_project_json(&model, &resolved, &layout(), &info);
        assert_eq!(json["sysroot_src"], "/path/to/rust-src");
    }

    #[test]
    fn dep_map_key_used_as_extern_name_not_crate_name() {
        let (mut model, th) = make_fixture();
        let (target_ch, _) = model.crates.insert(
            "real-name".into(),
            CrateDef {
                name: "real-name".into(),
                path: "crates/real-name".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                ..Default::default()
            },
        );
        // Consumer crate has a dep keyed by "alias_name" pointing at
        // "real-name"'s handle.
        let mut deps = BTreeMap::new();
        deps.insert(
            "alias_name".to_string(),
            DepDef {
                crate_name: "real-name".into(),
                crate_handle: Some(target_ch),
                features: Vec::new(),
                version: None,
                span: None,
            },
        );
        let (consumer_ch, _) = model.crates.insert(
            "consumer".into(),
            CrateDef {
                name: "consumer".into(),
                path: "crates/consumer".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                deps,
                ..Default::default()
            },
        );
        let resolved = make_resolved(
            th,
            vec![
                ResolvedCrateRef {
                    handle: target_ch,
                    target: th,
                    host: true,
                },
                ResolvedCrateRef {
                    handle: consumer_ch,
                    target: th,
                    host: true,
                },
            ],
        );
        let info = Arc::new(fake_rustc_info(None));
        let json = generate_rust_project_json(&model, &resolved, &layout(), &info);
        // consumer is the second user crate → index 2.
        let consumer = &json["crates"][2];
        let deps = consumer["deps"].as_array().unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(
            deps[0]["name"], "alias_name",
            "extern name must be the dep map key, not the target crate's name"
        );
        assert_eq!(deps[0]["crate"], 1);
    }

    #[test]
    fn vendor_dep_with_no_resolved_target_is_skipped() {
        let (mut model, th) = make_fixture();
        // Insert a "phantom" CrateDef so we can borrow its handle for
        // the dep, but DO NOT add it to resolved.crates — simulating
        // a vendor dep not yet wired up.
        let (phantom, _) = model.crates.insert(
            "phantom".into(),
            CrateDef {
                name: "phantom".into(),
                path: "vendor/phantom".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                ..Default::default()
            },
        );
        let mut deps = BTreeMap::new();
        deps.insert(
            "phantom".to_string(),
            DepDef {
                crate_name: "phantom".into(),
                crate_handle: Some(phantom),
                features: Vec::new(),
                version: None,
                span: None,
            },
        );
        let (consumer_ch, _) = model.crates.insert(
            "consumer".into(),
            CrateDef {
                name: "consumer".into(),
                path: "crates/consumer".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                deps,
                ..Default::default()
            },
        );
        let resolved = make_resolved(
            th,
            vec![ResolvedCrateRef {
                handle: consumer_ch,
                target: th,
                host: true,
            }],
        );
        let info = Arc::new(fake_rustc_info(None));
        let json = generate_rust_project_json(&model, &resolved, &layout(), &info);
        // Consumer is at index 1; its deps array must be empty
        // because the phantom dep was filtered out (not erroring).
        let consumer = &json["crates"][1];
        assert!(
            consumer["deps"].as_array().unwrap().is_empty(),
            "vendor dep must be silently skipped, got: {}",
            consumer["deps"]
        );
    }
}
