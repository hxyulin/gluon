//! DAG of build nodes and the `build_dag` constructor.
//!
//! The DAG is generic in the sense that the scheduler never looks inside
//! a `DagNode` — it only tracks identities and edges. The pipeline
//! dispatcher (Chunk B4) is the only code that matches on `DagNode` variant
//! and calls the corresponding work function.
//!
//! ### Why `BTreeMap`/`BTreeSet` everywhere?
//!
//! Determinism is non-negotiable (see `CLAUDE.md` §3). Build outputs must
//! be reproducible: two runs of `gluon build` with identical input must
//! produce byte-identical artifacts. Determinism collapses at the first
//! `HashMap` — its iteration order is randomised across runs. Every
//! adjacency structure in this module therefore uses `BTreeMap` or
//! `BTreeSet`.
//!
//! ### Why `BTreeSet` for `edges_out` values instead of `Vec`?
//!
//! `BTreeSet` provides automatic deduplication of edges. A previous
//! implementation used `Vec` and suffered a real scheduler deadlock: when
//! `add_edge` was called twice for the same (from, to) pair (a common
//! occurrence because multiple rules in a stage can reference the same
//! group), the in-degree of `to` was incremented twice but the edge was
//! only conceptually present once. The result was a node whose in-degree
//! could never reach zero — a permanent deadlock. `BTreeSet` prevents
//! this by construction: inserting the same edge twice is a no-op, and
//! `add_edge` increments in-degree only when the set actually grows.
//!
//! **Do not change `edges_out` to `Vec` under any circumstances.**

use crate::error::{Error, Result};
use gluon_model::{BuildModel, CrateDef, EspDef, Handle, ResolvedConfig, RuleDef, TargetDef};
use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------
// DagNode
// ---------------------------------------------------------------------------

/// A work node in the scheduler DAG. Four kinds map onto the four kinds
/// of work the MVP-M pipeline performs.
///
/// The scheduler treats these as opaque identities — it only tracks edges and
/// in-degrees. The pipeline dispatcher (Chunk B4) matches on the variant to
/// decide what work to perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub enum DagNode {
    /// Build the custom sysroot for a specific target triple.
    Sysroot(Handle<TargetDef>),
    /// Generate + compile the `<project>_config` crate for a specific target.
    /// One per distinct cross target in `resolved.crates` — each target gets
    /// its own config crate rlib so all cross crates can `--extern` it
    /// regardless of which target they compile for.
    ConfigCrate(Handle<TargetDef>),
    /// Compile a user-declared crate (host or cross).
    Crate(Handle<CrateDef>),
    /// Run a user-declared rule via the rule registry.
    Rule(Handle<RuleDef>),
    /// Assemble an EFI System Partition directory from the primary build
    /// outputs of one or more sibling crates. Depends on every source
    /// crate referenced by the ESP's entries. See [`EspDef`].
    Esp(Handle<EspDef>),
}

// ---------------------------------------------------------------------------
// NodeId
// ---------------------------------------------------------------------------

/// Compact index into `Dag::nodes`. Cheap to copy; used as the key for all
/// adjacency structures.
pub type NodeId = u32;

// ---------------------------------------------------------------------------
// Dag
// ---------------------------------------------------------------------------

/// A directed acyclic graph of work nodes with deterministic ordering.
///
/// All adjacency structures use `BTreeMap`/`BTreeSet` for stable iteration
/// across runs — see module-level doc for the full rationale.
pub struct Dag {
    /// All nodes, indexed by `NodeId`.
    pub nodes: Vec<DagNode>,
    /// Adjacency list: `edges_out[from]` = set of NodeIds that depend on `from`.
    /// `BTreeSet` ensures edge deduplication and deterministic iteration.
    /// See module-level note on why `Vec` is explicitly forbidden here.
    pub edges_out: BTreeMap<NodeId, BTreeSet<NodeId>>,
    /// In-degree for each node. Nodes with in-degree 0 are immediately ready.
    pub in_degree: BTreeMap<NodeId, u32>,
    /// Reverse lookup: `DagNode` → its `NodeId`. Ensures `insert_node` is
    /// idempotent — same logical node always maps to the same `NodeId`.
    pub node_index: BTreeMap<DagNode, NodeId>,
}

impl Dag {
    /// Create an empty DAG.
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            edges_out: BTreeMap::new(),
            in_degree: BTreeMap::new(),
            node_index: BTreeMap::new(),
        }
    }

    /// Insert a node, returning its `NodeId`.
    ///
    /// Idempotent: the same `DagNode` inserted twice returns the same `NodeId`
    /// without any side effects. This makes it safe to call `insert_node`
    /// speculatively for dependency nodes without checking for prior insertion.
    pub fn insert_node(&mut self, node: DagNode) -> NodeId {
        if let Some(&id) = self.node_index.get(&node) {
            return id;
        }
        let id = self.nodes.len() as NodeId;
        self.nodes.push(node);
        self.node_index.insert(node, id);
        // Initialise in-degree to 0. This entry must exist for every node so
        // that `ready()` can iterate `in_degree` to find zero-degree nodes.
        self.in_degree.insert(id, 0);
        id
    }

    /// Add an edge: `to` depends on `from`.
    ///
    /// Idempotent via `BTreeSet`: inserting the same edge a second time is a
    /// no-op and does **not** increment the in-degree of `to` a second time.
    /// This is the critical invariant that prevents scheduler deadlocks — see
    /// the module-level doc for the historical bug this guards against.
    pub fn add_edge(&mut self, from: NodeId, to: NodeId) {
        // edges_out[from] is the set of nodes that depend on `from`.
        let inserted = self.edges_out.entry(from).or_default().insert(to);
        // Only increment in-degree when the edge is genuinely new.
        if inserted {
            *self.in_degree.entry(to).or_insert(0) += 1;
        }
    }

    /// Return all `NodeId`s with in-degree 0 at the moment of the call.
    ///
    /// Results are in ascending `NodeId` order (deterministic via `BTreeMap`
    /// key ordering). The caller (worker pool) uses this to seed the initial
    /// ready queue and to determine which nodes become unblocked after a
    /// predecessor completes.
    pub fn ready(&self) -> Vec<NodeId> {
        self.in_degree
            .iter()
            .filter(|&(_, &deg)| deg == 0)
            .map(|(&id, _)| id)
            .collect()
    }

    /// Iterate over the successor `NodeId`s of `from` — i.e., nodes that have
    /// `from` as a dependency and become candidates for unblocking when `from`
    /// completes.
    pub fn dependents(&self, from: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        self.edges_out
            .get(&from)
            .into_iter()
            .flat_map(|set| set.iter().copied())
    }

    /// Total number of nodes in the DAG.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// True when the DAG contains no nodes.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

impl Default for Dag {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// build_dag
// ---------------------------------------------------------------------------

/// Build the scheduler DAG from a resolved configuration and the underlying
/// build model.
///
/// ### Edge rules
///
/// 1. **Cross crate → Sysroot**: Every cross crate depends on the sysroot for
///    its target. The sysroot must be built before any cross compilation can
///    start.
/// 2. **Cross crate → ConfigCrate**: Every cross crate depends on the config
///    crate (the generated `<project>_config` rlib). The config crate exposes
///    the resolved `CONFIG_*` flags as constants — downstream cross crates
///    `--extern` it.
/// 3. **ConfigCrate → Sysroot**: The config crate is compiled for the
///    profile's cross target, so it also depends on the sysroot.
/// 4. **Crate → Crate (deps)**: Every crate depends on every entry in its
///    `CrateDef::deps` map whose `crate_handle` is `Some` AND whose
///    referenced crate appears in `resolved.crates`. Dangling handles are
///    silently skipped — this function's contract is to build edges for
///    resolvable deps; surfacing missing handles is the resolver's job.
///    Both host→host and cross→cross (and host→cross for proc-macros) are
///    modelled here; cross→host is architecturally nonsensical but not
///    explicitly rejected.
/// 4a. **Crate → Crate (artifact_deps)**: Every crate depends on every entry
///     in its `CrateDef::artifact_deps` list that resolves to a crate present
///     in `resolved.crates`. Unlike regular `deps`, artifact deps do not
///     produce `--extern` flags — they exist purely to enforce build ordering
///     across crates that reference each other's build outputs (e.g. a
///     bootloader that `include_bytes!`es a kernel ELF via an `artifact_env`
///     injected `KERNEL_PATH`). Edges are group- and target-agnostic; a
///     `x86_64-unknown-uefi` bootloader can artifact-depend on a
///     `x86_64-unknown-none` kernel and the cross-group edge is added here.
///     Dangling names (no matching crate in the model) are a resolver-level
///     error surfaced by `validate::check_artifact_deps_resolve`; this
///     function silently skips them as a belt-and-braces backstop.
/// 5. **Pipeline stage barrier**: Each stage in each pipeline becomes one or
///    more rule nodes. Every node in stage N depends on every node in
///    stage N−1 (bipartite barrier). In MVP-M each stage has at most one rule
///    node (`PipelineStep::rule`); stages with `rule: None` are skipped.
/// 6. **Rule → group crates**: For each `Some(group_handle)` in a stage's
///    `PipelineStep::inputs_handles`, the rule node depends on every crate
///    in `resolved.crates` whose `CrateDef::group_handle` matches.
///
/// ### Empty crates list
///
/// Legal: `build_dag` constructs the DAG with only a `Sysroot` and
/// `ConfigCrate` node when `resolved.crates` is empty.
///
/// ### Cycle detection
///
/// NOT performed here. Malformed models that contain cycles will simply fail
/// to make progress at scheduling time (the worker pool detects deadlock and
/// returns an error).
pub fn build_dag(resolved: &ResolvedConfig, model: &BuildModel) -> Result<Dag> {
    let mut dag = Dag::new();

    // -----------------------------------------------------------------------
    // Step 1: Insert cross Sysroot nodes for every target in use.
    // -----------------------------------------------------------------------
    // Each distinct target used by at least one resolved cross crate gets
    // its own Sysroot node. A project with two targets (e.g. x86_64-unknown-uefi
    // for the bootloader and x86_64-unknown-none for the kernel) produces
    // two independent sysroot builds. `insert_node` is idempotent so
    // inserting the same target twice is a no-op.
    //
    // The profile's target always gets a sysroot node (for the config crate);
    // additional targets appear if any resolved crate references them.
    dag.insert_node(DagNode::Sysroot(resolved.profile.target));
    for crate_ref in &resolved.crates {
        if !crate_ref.host {
            dag.insert_node(DagNode::Sysroot(crate_ref.target));
        }
    }

    // -----------------------------------------------------------------------
    // Step 2: Insert one ConfigCrate node per distinct cross target.
    // Each ConfigCrate depends on its own target's sysroot.
    // -----------------------------------------------------------------------
    // Collect distinct cross targets. The profile target always gets one
    // (even if no crate uses it — the config crate is unconditional);
    // additional targets appear from resolved.crates.
    let mut config_targets: BTreeSet<Handle<TargetDef>> = BTreeSet::new();
    config_targets.insert(resolved.profile.target);
    for crate_ref in &resolved.crates {
        if !crate_ref.host {
            config_targets.insert(crate_ref.target);
        }
    }
    for &target in &config_targets {
        let sysroot_id = dag.insert_node(DagNode::Sysroot(target));
        let config_id = dag.insert_node(DagNode::ConfigCrate(target));
        dag.add_edge(sysroot_id, config_id);
    }

    // -----------------------------------------------------------------------
    // Step 3: Insert every crate in resolved.crates.
    // -----------------------------------------------------------------------
    // Collect the set of handles present in resolved.crates so we can skip
    // dangling dep handles later (rule 4).
    let resolved_handles: BTreeSet<Handle<CrateDef>> =
        resolved.crates.iter().map(|cr| cr.handle).collect();

    for crate_ref in &resolved.crates {
        let crate_id = dag.insert_node(DagNode::Crate(crate_ref.handle));

        // Edge rule 1 + 2: cross crates depend on Sysroot and ConfigCrate.
        // Host crates (proc-macros, build scripts) are compiled for the build
        // machine and do not use the custom sysroot or config crate.
        if !crate_ref.host {
            // Rule 1: cross crate → sysroot FOR ITS OWN TARGET (not
            // necessarily the profile target — multi-target projects have
            // independent sysroots per target).
            let crate_sysroot_id = dag.insert_node(DagNode::Sysroot(crate_ref.target));
            dag.add_edge(crate_sysroot_id, crate_id);
            // Rule 2: cross crate → config crate for its own target.
            // Each target has its own ConfigCrate node so there is no
            // target-mismatch risk.
            let config_id = dag.insert_node(DagNode::ConfigCrate(crate_ref.target));
            dag.add_edge(config_id, crate_id);
        }

        // Edge rule 4: crate → its dependency crates.
        let crate_def: &CrateDef = model.crates.get(crate_ref.handle).ok_or_else(|| {
            Error::Compile(format!(
                "build_dag: crate handle {:?} present in resolved.crates but not in \
                 build model (internal consistency error — this is a resolver bug)",
                crate_ref.handle
            ))
        })?;

        for dep in crate_def.deps.values() {
            let dep_handle = match dep.crate_handle {
                Some(h) => h,
                None => continue, // unresolved dep — resolver bug, not our problem
            };
            // Skip deps that are not part of this build (dangling handles).
            if !resolved_handles.contains(&dep_handle) {
                continue;
            }
            let dep_id = dag.insert_node(DagNode::Crate(dep_handle));
            // The dep must complete before this crate can compile.
            dag.add_edge(dep_id, crate_id);
        }

        // Edge rule 4a: crate → its artifact-dep crates (ordering-only,
        // no --extern). Unknown names are skipped silently; validate.rs
        // is responsible for surfacing them as a diagnostic before we
        // get here.
        for dep_name in &crate_def.artifact_deps {
            let dep_handle = match model.crates.lookup(dep_name) {
                Some(h) => h,
                None => continue, // dangling — validate surfaces this
            };
            if !resolved_handles.contains(&dep_handle) {
                continue;
            }
            let dep_id = dag.insert_node(DagNode::Crate(dep_handle));
            dag.add_edge(dep_id, crate_id);
        }
    }

    // -----------------------------------------------------------------------
    // Step 3b: EspBuild nodes.
    // -----------------------------------------------------------------------
    // Each declared ESP becomes one node. The node depends on every source
    // crate referenced by its entries. Entries whose source crate does not
    // resolve (missing `source_crate_handle` because intern failed) are
    // skipped — validate has already pushed a diagnostic for those.
    // Entries whose source crate exists but is not in `resolved.crates`
    // (e.g. gated behind a disabled config option) are also skipped, same
    // policy as regular deps. An ESP with zero resolvable entries still
    // gets a node — the helper will produce an empty directory, which
    // is harmless and makes the failure mode observable.
    for (esp_handle, esp) in model.esps.iter() {
        let esp_id = dag.insert_node(DagNode::Esp(esp_handle));
        for entry in &esp.entries {
            let Some(src_handle) = entry.source_crate_handle else {
                continue; // intern-time dangling ref — diagnostic already pushed
            };
            if !resolved_handles.contains(&src_handle) {
                continue;
            }
            let src_id = dag.insert_node(DagNode::Crate(src_handle));
            dag.add_edge(src_id, esp_id);
        }
    }

    // -----------------------------------------------------------------------
    // Step 4: Pipeline stage barriers and rule → group edges.
    // -----------------------------------------------------------------------
    for (_pipeline_handle, pipeline) in model.pipelines.iter() {
        // Track the rule NodeIds from the previous stage so we can wire the
        // bipartite barrier between consecutive stages.
        let mut prev_stage_rule_ids: Vec<NodeId> = Vec::new();

        for step in &pipeline.stages {
            // Stages without a rule are skipped (MVP-M invariant: each
            // participating stage names exactly one rule).
            let rule_name = match &step.rule {
                Some(name) => name,
                None => {
                    // This stage produces no rule node. Leave prev_stage_rule_ids
                    // unchanged so the next non-None stage inherits the same
                    // predecessor set — the barrier is preserved across skipped
                    // stages. MVP-M doesn't exercise None stages, but the safer
                    // behaviour is the one a future contributor would expect.
                    continue;
                }
            };

            let rule_handle = model.rules.lookup(rule_name).ok_or_else(|| {
                Error::Compile(format!(
                    "build_dag: pipeline '{}' stage '{}' references unknown rule '{}'",
                    pipeline.name, step.name, rule_name
                ))
            })?;
            let rule_id = dag.insert_node(DagNode::Rule(rule_handle));

            // Edge rule 5: bipartite barrier — rule depends on all rules in
            // the previous stage.
            for &prev_id in &prev_stage_rule_ids {
                dag.add_edge(prev_id, rule_id);
            }

            // Edge rule 6: rule depends on every crate in each referenced group.
            for maybe_group in &step.inputs_handles {
                let group_handle = match maybe_group {
                    Some(h) => *h,
                    None => continue,
                };
                for crate_ref in &resolved.crates {
                    let crate_def = model.crates.get(crate_ref.handle).ok_or_else(|| {
                        Error::Compile(format!(
                            "build_dag: crate handle {:?} present in resolved.crates but \
                             not in build model (internal consistency error)",
                            crate_ref.handle
                        ))
                    })?;
                    if crate_def.group_handle == Some(group_handle) {
                        let crate_id = dag.insert_node(DagNode::Crate(crate_ref.handle));
                        dag.add_edge(crate_id, rule_id);
                    }
                }
            }

            prev_stage_rule_ids = vec![rule_id];
        }
    }

    Ok(dag)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use gluon_model::{
        BuildModel, CrateDef, CrateType, DepDef, GroupDef, PipelineDef, PipelineStep, ProjectDef,
        ResolvedCrateRef, RuleDef, RuleHandler, TargetDef,
    };

    // -----------------------------------------------------------------------
    // Test-support helpers
    // -----------------------------------------------------------------------

    #[cfg(test)]
    mod test_support {
        use super::*;
        use gluon_model::{ResolvedConfig, ResolvedProfile};
        use std::path::PathBuf;

        pub fn make_target(model: &mut BuildModel, name: &str) -> Handle<TargetDef> {
            let (h, _) = model.targets.insert(
                name.into(),
                TargetDef {
                    name: name.into(),
                    spec: format!("{}-unknown-none", name),
                    builtin: true,
                    panic_strategy: None,
                    span: None,
                },
            );
            h
        }

        pub fn make_profile(target: Handle<TargetDef>) -> ResolvedProfile {
            ResolvedProfile {
                name: "debug".into(),
                target,
                opt_level: 0,
                debug_info: false,
                lto: None,
                boot_binary: None,
                qemu_memory: None,
                qemu_cores: None,
                qemu_extra_args: Vec::new(),
                test_timeout: None,
            }
        }

        pub fn make_resolved(
            target_handle: Handle<TargetDef>,
            crates: Vec<ResolvedCrateRef>,
        ) -> ResolvedConfig {
            ResolvedConfig {
                project: ProjectDef {
                    name: "testproject".into(),
                    version: "0.1.0".into(),
                    config_crate_name: None,
                    cfg_prefix: None,
                    config_override_file: None,
                    default_profile: None,
                },
                profile: make_profile(target_handle),
                options: Default::default(),
                crates,
                build_dir: PathBuf::from("/tmp/test-build"),
                project_root: PathBuf::from("/tmp/test"),
            }
        }

        pub fn make_cross_crate(
            model: &mut BuildModel,
            name: &str,
            target_handle: Handle<TargetDef>,
        ) -> Handle<CrateDef> {
            let (h, _) = model.crates.insert(
                name.into(),
                CrateDef {
                    name: name.into(),
                    path: name.into(),
                    edition: "2021".into(),
                    crate_type: CrateType::Lib,
                    target: "cross".into(),
                    target_handle: Some(target_handle),
                    group: "default".into(),
                    ..Default::default()
                },
            );
            h
        }

        pub fn make_host_crate(model: &mut BuildModel, name: &str) -> Handle<CrateDef> {
            let (h, _) = model.crates.insert(
                name.into(),
                CrateDef {
                    name: name.into(),
                    path: name.into(),
                    edition: "2021".into(),
                    crate_type: CrateType::ProcMacro,
                    target: "host".into(),
                    group: "host-group".into(),
                    ..Default::default()
                },
            );
            h
        }
    }

    use test_support::*;

    // -----------------------------------------------------------------------
    // Test 1: insert_node_dedups
    // -----------------------------------------------------------------------
    #[test]
    fn insert_node_dedups() {
        let mut dag = Dag::new();
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-test");
        let node = DagNode::Sysroot(th);

        let id1 = dag.insert_node(node);
        let id2 = dag.insert_node(node);
        assert_eq!(
            id1, id2,
            "same DagNode inserted twice must yield same NodeId"
        );
        assert_eq!(dag.len(), 1, "DAG must have exactly one node after dedup");
    }

    // -----------------------------------------------------------------------
    // Test 2: add_edge_is_idempotent_and_increments_in_degree_once
    // -----------------------------------------------------------------------
    #[test]
    fn add_edge_is_idempotent_and_increments_in_degree_once() {
        let mut dag = Dag::new();
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-test");

        let a = dag.insert_node(DagNode::Sysroot(th));
        let b = dag.insert_node(DagNode::ConfigCrate(th));

        // First insertion: in-degree of b becomes 1.
        dag.add_edge(a, b);
        assert_eq!(
            *dag.in_degree.get(&b).unwrap(),
            1,
            "first add_edge must set in-degree to 1"
        );

        // Second insertion: must be a no-op — in-degree stays 1.
        dag.add_edge(a, b);
        assert_eq!(
            *dag.in_degree.get(&b).unwrap(),
            1,
            "duplicate add_edge must NOT increment in-degree a second time"
        );

        // The edge set has exactly one entry.
        let out = dag.edges_out.get(&a).unwrap();
        assert_eq!(
            out.len(),
            1,
            "BTreeSet must contain exactly one edge after two identical inserts"
        );
    }

    // -----------------------------------------------------------------------
    // Test 3: ready_returns_zero_in_degree_nodes_in_ascending_order
    // -----------------------------------------------------------------------
    #[test]
    fn ready_returns_zero_in_degree_nodes_in_ascending_order() {
        let mut dag = Dag::new();
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-test");
        let ch1 = test_support::make_cross_crate(&mut model, "crate-a", th);
        let ch2 = test_support::make_cross_crate(&mut model, "crate-b", th);

        // Insert three nodes with no edges — all have in-degree 0.
        let id0 = dag.insert_node(DagNode::Sysroot(th));
        let id1 = dag.insert_node(DagNode::Crate(ch1));
        let id2 = dag.insert_node(DagNode::Crate(ch2));

        let ready = dag.ready();
        assert_eq!(
            ready,
            vec![id0, id1, id2],
            "ready() must return in ascending NodeId order"
        );
    }

    // -----------------------------------------------------------------------
    // Test 4: ready_excludes_nodes_with_nonzero_in_degree
    // -----------------------------------------------------------------------
    #[test]
    fn ready_excludes_nodes_with_nonzero_in_degree() {
        let mut dag = Dag::new();
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-test");

        let sysroot = dag.insert_node(DagNode::Sysroot(th));
        let config = dag.insert_node(DagNode::ConfigCrate(th));
        dag.add_edge(sysroot, config);

        let ready = dag.ready();
        assert_eq!(
            ready,
            vec![sysroot],
            "only the sysroot node has in-degree 0"
        );
        assert!(
            !ready.contains(&config),
            "config depends on sysroot and must not be ready"
        );
    }

    // -----------------------------------------------------------------------
    // Test 5: build_dag_minimal_project
    // -----------------------------------------------------------------------
    #[test]
    fn build_dag_minimal_project() {
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-test");
        let ch = make_cross_crate(&mut model, "my-crate", th);

        let resolved = make_resolved(
            th,
            vec![ResolvedCrateRef {
                handle: ch,
                target: th,
                host: false,
            }],
        );

        let dag = build_dag(&resolved, &model).unwrap();

        // All three node kinds must be present.
        let sysroot_id = *dag
            .node_index
            .get(&DagNode::Sysroot(th))
            .expect("Sysroot node");
        let config_id = *dag
            .node_index
            .get(&DagNode::ConfigCrate(th))
            .expect("ConfigCrate node");
        let crate_id = *dag.node_index.get(&DagNode::Crate(ch)).expect("Crate node");

        assert_eq!(dag.len(), 3);

        // Edge: Sysroot → ConfigCrate.
        let sysroot_outs = dag.edges_out.get(&sysroot_id).expect("sysroot has outs");
        assert!(
            sysroot_outs.contains(&config_id),
            "Sysroot → ConfigCrate edge missing"
        );
        // Edge: Sysroot → Crate.
        assert!(
            sysroot_outs.contains(&crate_id),
            "Sysroot → Crate edge missing"
        );

        // Edge: ConfigCrate → Crate.
        let config_outs = dag.edges_out.get(&config_id).expect("config has outs");
        assert!(
            config_outs.contains(&crate_id),
            "ConfigCrate → Crate edge missing"
        );

        // In-degrees.
        assert_eq!(*dag.in_degree.get(&sysroot_id).unwrap(), 0);
        assert_eq!(*dag.in_degree.get(&config_id).unwrap(), 1);
        assert_eq!(*dag.in_degree.get(&crate_id).unwrap(), 2);
    }

    // -----------------------------------------------------------------------
    // Test 6: build_dag_with_host_proc_macro
    // -----------------------------------------------------------------------
    #[test]
    fn build_dag_with_host_proc_macro() {
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-test");

        // Host proc-macro crate.
        let host_h = make_host_crate(&mut model, "my-proc-macro");
        // Cross crate that depends on the proc-macro.
        let cross_h = model
            .crates
            .insert(
                "my-cross-crate".into(),
                CrateDef {
                    name: "my-cross-crate".into(),
                    path: "my-cross-crate".into(),
                    edition: "2021".into(),
                    crate_type: CrateType::Lib,
                    target: "cross".into(),
                    target_handle: Some(th),
                    group: "default".into(),
                    deps: {
                        let mut m = BTreeMap::new();
                        m.insert(
                            "my_proc_macro".into(),
                            DepDef {
                                crate_name: "my-proc-macro".into(),
                                crate_handle: Some(host_h),
                                ..Default::default()
                            },
                        );
                        m
                    },
                    ..Default::default()
                },
            )
            .0;

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

        let dag = build_dag(&resolved, &model).unwrap();

        let sysroot_id = *dag.node_index.get(&DagNode::Sysroot(th)).unwrap();
        let config_id = *dag.node_index.get(&DagNode::ConfigCrate(th)).unwrap();
        let host_id = *dag.node_index.get(&DagNode::Crate(host_h)).unwrap();
        let cross_id = *dag.node_index.get(&DagNode::Crate(cross_h)).unwrap();

        // Host crate must NOT depend on Sysroot or ConfigCrate.
        let sysroot_outs = dag.edges_out.get(&sysroot_id).unwrap();
        assert!(
            !sysroot_outs.contains(&host_id),
            "host proc-macro must NOT depend on the cross sysroot"
        );
        let config_outs = dag.edges_out.get(&config_id).unwrap();
        assert!(
            !config_outs.contains(&host_id),
            "host proc-macro must NOT depend on ConfigCrate"
        );

        // Cross crate depends on Sysroot, ConfigCrate, and the host crate.
        assert!(
            sysroot_outs.contains(&cross_id),
            "cross crate must depend on Sysroot"
        );
        assert!(
            config_outs.contains(&cross_id),
            "cross crate must depend on ConfigCrate"
        );
        let host_outs = dag
            .edges_out
            .get(&host_id)
            .expect("host proc-macro has outs");
        assert!(
            host_outs.contains(&cross_id),
            "cross crate must depend on host proc-macro"
        );

        // Sanity: host crate's in-degree is 0 (no predecessors).
        assert_eq!(*dag.in_degree.get(&host_id).unwrap(), 0);
        // Cross crate's in-degree: Sysroot + ConfigCrate + host = 3.
        assert_eq!(*dag.in_degree.get(&cross_id).unwrap(), 3);
    }

    // -----------------------------------------------------------------------
    // Test 7: build_dag_with_intra_chain_dep
    // -----------------------------------------------------------------------
    #[test]
    fn build_dag_with_intra_chain_dep() {
        // A depends on B depends on C (all cross). No transitive shortcut.
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-test");

        let c_h = make_cross_crate(&mut model, "crate-c", th);
        let b_h = model
            .crates
            .insert(
                "crate-b".into(),
                CrateDef {
                    name: "crate-b".into(),
                    path: "crate-b".into(),
                    edition: "2021".into(),
                    crate_type: CrateType::Lib,
                    target: "cross".into(),
                    target_handle: Some(th),
                    group: "default".into(),
                    deps: {
                        let mut m = BTreeMap::new();
                        m.insert(
                            "crate_c".into(),
                            DepDef {
                                crate_name: "crate-c".into(),
                                crate_handle: Some(c_h),
                                ..Default::default()
                            },
                        );
                        m
                    },
                    ..Default::default()
                },
            )
            .0;
        let a_h = model
            .crates
            .insert(
                "crate-a".into(),
                CrateDef {
                    name: "crate-a".into(),
                    path: "crate-a".into(),
                    edition: "2021".into(),
                    crate_type: CrateType::Lib,
                    target: "cross".into(),
                    target_handle: Some(th),
                    group: "default".into(),
                    deps: {
                        let mut m = BTreeMap::new();
                        m.insert(
                            "crate_b".into(),
                            DepDef {
                                crate_name: "crate-b".into(),
                                crate_handle: Some(b_h),
                                ..Default::default()
                            },
                        );
                        m
                    },
                    ..Default::default()
                },
            )
            .0;

        let resolved = make_resolved(
            th,
            vec![
                ResolvedCrateRef {
                    handle: c_h,
                    target: th,
                    host: false,
                },
                ResolvedCrateRef {
                    handle: b_h,
                    target: th,
                    host: false,
                },
                ResolvedCrateRef {
                    handle: a_h,
                    target: th,
                    host: false,
                },
            ],
        );

        let dag = build_dag(&resolved, &model).unwrap();

        let c_id = *dag.node_index.get(&DagNode::Crate(c_h)).unwrap();
        let b_id = *dag.node_index.get(&DagNode::Crate(b_h)).unwrap();
        let a_id = *dag.node_index.get(&DagNode::Crate(a_h)).unwrap();

        // C → B edge.
        let c_outs = dag.edges_out.get(&c_id).unwrap();
        assert!(c_outs.contains(&b_id), "C → B edge missing");
        // B → A edge.
        let b_outs = dag.edges_out.get(&b_id).unwrap();
        assert!(b_outs.contains(&a_id), "B → A edge missing");
        // No shortcut C → A.
        assert!(!c_outs.contains(&a_id), "unexpected shortcut C → A");
    }

    // -----------------------------------------------------------------------
    // Test 8: build_dag_with_pipeline_stage_barrier
    // -----------------------------------------------------------------------
    #[test]
    fn build_dag_with_pipeline_stage_barrier() {
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-test");

        // Two rules.
        let (rule0_h, _) = model.rules.insert(
            "stage0-rule".into(),
            RuleDef {
                name: "stage0-rule".into(),
                inputs: vec![],
                outputs: vec![],
                depends_on: vec![],
                handler: RuleHandler::Builtin("exec".into()),
                span: None,
            },
        );
        let (rule1_h, _) = model.rules.insert(
            "stage1-rule".into(),
            RuleDef {
                name: "stage1-rule".into(),
                inputs: vec![],
                outputs: vec![],
                depends_on: vec![],
                handler: RuleHandler::Builtin("exec".into()),
                span: None,
            },
        );

        // Pipeline with 2 stages.
        model.pipelines.insert(
            "my-pipeline".into(),
            PipelineDef {
                name: "my-pipeline".into(),
                stages: vec![
                    PipelineStep {
                        name: "stage0".into(),
                        inputs: vec![],
                        inputs_handles: vec![],
                        rule: Some("stage0-rule".into()),
                    },
                    PipelineStep {
                        name: "stage1".into(),
                        inputs: vec![],
                        inputs_handles: vec![],
                        rule: Some("stage1-rule".into()),
                    },
                ],
                span: None,
            },
        );

        let resolved = make_resolved(th, vec![]);
        let dag = build_dag(&resolved, &model).unwrap();

        let rule0_id = *dag.node_index.get(&DagNode::Rule(rule0_h)).unwrap();
        let rule1_id = *dag.node_index.get(&DagNode::Rule(rule1_h)).unwrap();

        // Stage-1 rule depends on stage-0 rule.
        let rule0_outs = dag
            .edges_out
            .get(&rule0_id)
            .expect("stage0 rule has successors");
        assert!(
            rule0_outs.contains(&rule1_id),
            "stage-1 rule must depend on stage-0 rule"
        );
        // In-degree of stage-0 rule: 0.
        assert_eq!(*dag.in_degree.get(&rule0_id).unwrap(), 0);
        // In-degree of stage-1 rule: 1 (from stage-0 rule).
        assert_eq!(*dag.in_degree.get(&rule1_id).unwrap(), 1);
    }

    // -----------------------------------------------------------------------
    // Test 9: build_dag_rule_depends_on_group_crates
    // -----------------------------------------------------------------------
    #[test]
    fn build_dag_rule_depends_on_group_crates() {
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-test");

        // A group with two crates.
        let (group_h, _) = model.groups.insert(
            "kernel-group".into(),
            GroupDef {
                name: "kernel-group".into(),
                target: "cross".into(),
                target_handle: Some(th),
                default_edition: "2021".into(),
                crates: vec!["crate-x".into(), "crate-y".into()],
                ..Default::default()
            },
        );

        // Two cross crates with group_handle set.
        let (cx_h, _) = model.crates.insert(
            "crate-x".into(),
            CrateDef {
                name: "crate-x".into(),
                path: "crate-x".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                target: "cross".into(),
                target_handle: Some(th),
                group: "kernel-group".into(),
                group_handle: Some(group_h),
                ..Default::default()
            },
        );
        let (cy_h, _) = model.crates.insert(
            "crate-y".into(),
            CrateDef {
                name: "crate-y".into(),
                path: "crate-y".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                target: "cross".into(),
                target_handle: Some(th),
                group: "kernel-group".into(),
                group_handle: Some(group_h),
                ..Default::default()
            },
        );

        // A rule.
        let (rule_h, _) = model.rules.insert(
            "link-rule".into(),
            RuleDef {
                name: "link-rule".into(),
                inputs: vec![],
                outputs: vec![],
                depends_on: vec![],
                handler: RuleHandler::Builtin("exec".into()),
                span: None,
            },
        );

        // Pipeline with one stage that references the group.
        model.pipelines.insert(
            "link-pipeline".into(),
            PipelineDef {
                name: "link-pipeline".into(),
                stages: vec![PipelineStep {
                    name: "link".into(),
                    inputs: vec!["kernel-group".into()],
                    inputs_handles: vec![Some(group_h)],
                    rule: Some("link-rule".into()),
                }],
                span: None,
            },
        );

        let resolved = make_resolved(
            th,
            vec![
                ResolvedCrateRef {
                    handle: cx_h,
                    target: th,
                    host: false,
                },
                ResolvedCrateRef {
                    handle: cy_h,
                    target: th,
                    host: false,
                },
            ],
        );

        let dag = build_dag(&resolved, &model).unwrap();

        let rule_id = *dag.node_index.get(&DagNode::Rule(rule_h)).unwrap();
        let cx_id = *dag.node_index.get(&DagNode::Crate(cx_h)).unwrap();
        let cy_id = *dag.node_index.get(&DagNode::Crate(cy_h)).unwrap();

        // Both crates' outs must contain the rule.
        let cx_outs = dag.edges_out.get(&cx_id).expect("crate-x has outs");
        assert!(cx_outs.contains(&rule_id), "rule must depend on crate-x");
        let cy_outs = dag.edges_out.get(&cy_id).expect("crate-y has outs");
        assert!(cy_outs.contains(&rule_id), "rule must depend on crate-y");

        // In-degree of rule: both crates feed into it (plus 0 from stage barrier).
        assert_eq!(*dag.in_degree.get(&rule_id).unwrap(), 2);
    }

    // -----------------------------------------------------------------------
    // Test 10: artifact_dep_across_groups_and_targets
    // -----------------------------------------------------------------------
    //
    // A bootloader crate (x86_64-unknown-uefi, "uefi-group") has an
    // `artifact_deps` entry pointing at a kernel crate (x86_64-unknown-none,
    // "kernel-group"). The DAG builder must emit a cross-group, cross-target
    // ordering edge from kernel → bootloader even though the two crates
    // share no regular `deps` relationship.
    //
    // This is the load-bearing case for `gluon run`-time artifact embedding
    // (a bootloader that `include_bytes!`es a kernel via a `KERNEL_PATH`
    // env var injected at rustc invocation time). If the edge is missing,
    // the scheduler may run the bootloader node before the kernel is built,
    // and `include_bytes!` will either fail or silently embed stale bytes.
    #[test]
    fn artifact_dep_across_groups_and_targets() {
        let mut model = BuildModel::default();
        let kernel_target = make_target(&mut model, "x86_64-kernel");
        let uefi_target = make_target(&mut model, "x86_64-uefi");

        // Kernel crate in kernel-group / x86_64-kernel target.
        let (kernel_h, _) = model.crates.insert(
            "kernel".into(),
            CrateDef {
                name: "kernel".into(),
                path: "crates/kernel".into(),
                edition: "2021".into(),
                crate_type: CrateType::Bin,
                target: "cross".into(),
                target_handle: Some(kernel_target),
                group: "kernel-group".into(),
                ..Default::default()
            },
        );

        // Bootloader crate in uefi-group / x86_64-uefi target, with an
        // artifact_dep on the kernel (no regular `deps` entry).
        let (bootloader_h, _) = model.crates.insert(
            "bootloader".into(),
            CrateDef {
                name: "bootloader".into(),
                path: "crates/bootloader".into(),
                edition: "2021".into(),
                crate_type: CrateType::Bin,
                target: "cross".into(),
                target_handle: Some(uefi_target),
                group: "uefi-group".into(),
                artifact_deps: vec!["kernel".into()],
                ..Default::default()
            },
        );

        // Resolved config pins the profile to the uefi target (the
        // "running" target) but still lists both crates — the scheduler
        // builds both regardless of which target the profile points at.
        let resolved = make_resolved(
            uefi_target,
            vec![
                ResolvedCrateRef {
                    handle: kernel_h,
                    target: kernel_target,
                    host: false,
                },
                ResolvedCrateRef {
                    handle: bootloader_h,
                    target: uefi_target,
                    host: false,
                },
            ],
        );

        let dag = build_dag(&resolved, &model).unwrap();

        let kernel_id = *dag
            .node_index
            .get(&DagNode::Crate(kernel_h))
            .expect("kernel node present");
        let bootloader_id = *dag
            .node_index
            .get(&DagNode::Crate(bootloader_h))
            .expect("bootloader node present");

        // The critical assertion: kernel → bootloader edge exists.
        let kernel_outs = dag
            .edges_out
            .get(&kernel_id)
            .expect("kernel has outgoing edges (at least to bootloader)");
        assert!(
            kernel_outs.contains(&bootloader_id),
            "artifact_deps must produce an ordering edge: kernel → bootloader"
        );

        // Bootloader's in-degree accounts for: Sysroot + ConfigCrate + kernel = 3.
        assert_eq!(
            *dag.in_degree.get(&bootloader_id).unwrap(),
            3,
            "bootloader in-degree must include the artifact-dep edge from kernel"
        );
    }

    // -----------------------------------------------------------------------
    // Test: build_dag_inserts_esp_nodes_with_source_crate_edges
    // -----------------------------------------------------------------------
    //
    // A single EspDef with one entry (source_crate = "bootloader") must
    // produce a `DagNode::Esp` node with an incoming edge from the
    // bootloader crate's `DagNode::Crate` node. The esp's `entries`
    // must have been interned with a `source_crate_handle` before
    // `build_dag` runs — we simulate that by setting it explicitly.
    #[test]
    fn build_dag_inserts_esp_nodes_with_source_crate_edges() {
        use gluon_model::{EspDef, EspEntry};

        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-uefi");

        let (bl_h, _) = model.crates.insert(
            "bootloader".into(),
            CrateDef {
                name: "bootloader".into(),
                path: "crates/bootloader".into(),
                edition: "2021".into(),
                crate_type: CrateType::Bin,
                target: "cross".into(),
                target_handle: Some(th),
                group: "uefi".into(),
                ..Default::default()
            },
        );

        // Mirror what `intern_esps` would do: resolve the source_crate
        // name to a handle before calling build_dag.
        let (esp_h, _) = model.esps.insert(
            "default".into(),
            EspDef {
                name: "default".into(),
                entries: vec![EspEntry {
                    source_crate: "bootloader".into(),
                    source_crate_handle: Some(bl_h),
                    dest_path: "EFI/BOOT/BOOTX64.EFI".into(),
                }],
                span: None,
            },
        );

        let resolved = make_resolved(
            th,
            vec![ResolvedCrateRef {
                handle: bl_h,
                target: th,
                host: false,
            }],
        );

        let dag = build_dag(&resolved, &model).unwrap();

        let bl_id = *dag.node_index.get(&DagNode::Crate(bl_h)).unwrap();
        let esp_id = *dag
            .node_index
            .get(&DagNode::Esp(esp_h))
            .expect("Esp node must be present");

        // bootloader → esp edge.
        let bl_outs = dag.edges_out.get(&bl_id).unwrap();
        assert!(
            bl_outs.contains(&esp_id),
            "bootloader → esp edge missing — ESP would run before the bootloader is built"
        );

        // ESP's in-degree: 1 (the bootloader). Sysroot/ConfigCrate feed
        // the bootloader, not the ESP directly.
        assert_eq!(*dag.in_degree.get(&esp_id).unwrap(), 1);
    }

    // -----------------------------------------------------------------------
    // Test 11: artifact_dep_unknown_name_is_silently_skipped
    // -----------------------------------------------------------------------
    //
    // A dangling `artifact_deps` entry (pointing at a crate name that does
    // not exist in the model) must NOT cause `build_dag` to error. Surfacing
    // that diagnostic is validate.rs's job — this function's contract is
    // to build edges for resolvable entries and skip the rest.
    #[test]
    fn artifact_dep_unknown_name_is_silently_skipped() {
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-test");
        let (consumer_h, _) = model.crates.insert(
            "consumer".into(),
            CrateDef {
                name: "consumer".into(),
                path: "consumer".into(),
                edition: "2021".into(),
                crate_type: CrateType::Bin,
                target: "cross".into(),
                target_handle: Some(th),
                group: "default".into(),
                artifact_deps: vec!["ghost".into()],
                ..Default::default()
            },
        );

        let resolved = make_resolved(
            th,
            vec![ResolvedCrateRef {
                handle: consumer_h,
                target: th,
                host: false,
            }],
        );

        // Must not return Err.
        let dag = build_dag(&resolved, &model).expect("unknown artifact_dep must not abort");
        // Consumer's in-degree: Sysroot + ConfigCrate only (no artifact edge).
        let consumer_id = *dag.node_index.get(&DagNode::Crate(consumer_h)).unwrap();
        assert_eq!(*dag.in_degree.get(&consumer_id).unwrap(), 2);
    }

    // -----------------------------------------------------------------------
    // Test: two cross targets produce two ConfigCrate nodes
    // -----------------------------------------------------------------------
    #[test]
    fn build_dag_two_targets_two_config_crates() {
        let mut model = BuildModel::default();
        let t1 = make_target(&mut model, "target-one");
        let t2 = make_target(&mut model, "target-two");

        let c1 = make_cross_crate(&mut model, "crate-one", t1);
        let c2 = make_cross_crate(&mut model, "crate-two", t2);

        let resolved = make_resolved(
            t1,
            vec![
                ResolvedCrateRef {
                    handle: c1,
                    target: t1,
                    host: false,
                },
                ResolvedCrateRef {
                    handle: c2,
                    target: t2,
                    host: false,
                },
            ],
        );

        let dag = build_dag(&resolved, &model).unwrap();

        // Two distinct ConfigCrate nodes.
        let cfg1_id = *dag
            .node_index
            .get(&DagNode::ConfigCrate(t1))
            .expect("ConfigCrate(t1) must exist");
        let cfg2_id = *dag
            .node_index
            .get(&DagNode::ConfigCrate(t2))
            .expect("ConfigCrate(t2) must exist");
        assert_ne!(cfg1_id, cfg2_id, "different targets → different config nodes");

        // Each cross crate depends on its own ConfigCrate.
        let c1_id = *dag.node_index.get(&DagNode::Crate(c1)).unwrap();
        let c2_id = *dag.node_index.get(&DagNode::Crate(c2)).unwrap();

        let cfg1_outs = dag.edges_out.get(&cfg1_id).unwrap();
        assert!(cfg1_outs.contains(&c1_id), "ConfigCrate(t1) → crate-one");
        assert!(
            !cfg1_outs.contains(&c2_id),
            "ConfigCrate(t1) must NOT → crate-two"
        );

        let cfg2_outs = dag.edges_out.get(&cfg2_id).unwrap();
        assert!(cfg2_outs.contains(&c2_id), "ConfigCrate(t2) → crate-two");
        assert!(
            !cfg2_outs.contains(&c1_id),
            "ConfigCrate(t2) must NOT → crate-one"
        );
    }
}
