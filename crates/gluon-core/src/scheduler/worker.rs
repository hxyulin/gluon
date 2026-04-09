//! Worker pool that drives a `Dag` to completion.
//!
//! ### Single-threaded vs multi-threaded paths
//!
//! Two separate code paths exist for `-j1` and `-j>1`:
//!
//! **`workers == 1` (single-threaded fast path)**
//!
//! No threads, no channels, no `mpsc` overhead. The ready set is a
//! `BTreeSet<NodeId>` maintained on the stack. At each step the smallest
//! ready NodeId is popped, dispatched inline, its buffers drained to the
//! sinks, and its dependents' in-degrees decremented. This path exists not
//! just for performance but for test correctness: at `-j1`, execution order
//! is fully deterministic — it always follows the ascending `NodeId` order
//! imposed by `BTreeSet`. Tests that assert a specific ordering (e.g.
//! topological correctness) must use `workers=1` to get reproducible results.
//!
//! **`workers > 1` (multi-threaded path)**
//!
//! Uses `std::thread::scope` so workers can borrow `&dyn JobDispatcher`
//! without needing `Arc`. The main thread owns the ready set, the in-degree
//! bookkeeping, AND is the sole job distributor. Each worker is assigned a
//! fixed index and owns its own `mpsc::Receiver<Option<NodeId>>` (None =
//! shutdown signal). The main thread keeps a set of idle worker indices; when
//! a ready node is available it pops an idle index, looks up that worker's
//! `Sender`, and pushes the job directly to that worker. When the worker
//! finishes it sends back `(worker_index, NodeId, result, stdout, stderr)` on
//! the result channel; the main thread returns the worker index to the idle
//! set and continues scheduling.
//!
//! This design eliminates the Mutex-across-recv anti-pattern: workers never
//! contend for a shared queue; each blocks only on its own private channel.
//! Parallelism is limited only by the number of ready nodes and workers, not
//! by lock contention.
//!
//! ### Per-job output buffering
//!
//! Each job writes into its own `Vec<u8>` stdout and stderr buffers. The main
//! thread flushes these to the sink writers *as each job completes*, rather
//! than inline during execution. This prevents interleaving: when two crates
//! compile in parallel, you never see rustc output from crate A interrupting
//! the output from crate B. The determinism guarantee is at the granularity
//! of individual job buffers — the *order* in which jobs complete is not
//! deterministic under parallelism, but each job's output block is atomic.
//!
//! ### Error handling
//!
//! On the first job failure, the pool stops dispatching NEW jobs. In-flight
//! jobs continue to completion (we cannot cancel them without `unsafe`
//! thread-killing). All results — successes and failures — are drained to the
//! sinks in completion order. The final return is
//! `Err(Error::Diagnostics(diags))` where `diags` collects every error seen,
//! in completion order.

use super::dag::{Dag, NodeId};
use crate::error::{Diagnostic, Error, Result};
use std::collections::BTreeSet;
use std::io::Write;
use std::sync::mpsc;

// ---------------------------------------------------------------------------
// JobDispatcher
// ---------------------------------------------------------------------------

/// Aggregate counts produced by [`crate::scheduler::execute_pipeline`] and
/// surfaced through [`crate::build`].
///
/// - `built` counts nodes that ran their underlying action (rustc, etc.) to
///   completion this build.
/// - `cached` counts nodes whose freshness check reported fresh, so the
///   action was skipped entirely.
///
/// Counts are accumulated across **all** pipeline node kinds that go through
/// a freshness check (sysroot crates, the generated config crate, and
/// user-declared crates). `Rule` nodes are **not** counted — they are not
/// cacheable in MVP-M.
///
/// This exists so that the CLI and integration tests can rely on a single
/// concrete cache-hit signal rather than scraping per-job stdout — the
/// alternative we explicitly rejected because it is fragile.
///
/// `esp_dirs` carries the assembled-ESP output paths produced by any
/// `DagNode::Esp` nodes in this build. It is keyed by
/// [`gluon_model::Handle<EspDef>`] and consumed by `run::entry` to
/// auto-wire the ESP into QEMU's `-drive fat:rw:` flag. The map is
/// empty for builds that do not declare any `esp(...)` blocks.
///
/// (`Copy` was dropped when `esp_dirs` was added — call sites clone
/// explicitly. There are no hot paths relying on a bitwise copy.)
#[derive(Debug, Clone, Default)]
pub struct BuildSummary {
    /// Number of cacheable steps that ran rustc to completion this build.
    pub built: usize,
    /// Number of cacheable steps that were already fresh in the cache and
    /// therefore skipped rustc entirely.
    pub cached: usize,
    /// Assembled-ESP directories keyed by [`gluon_model::EspDef`] handle.
    pub esp_dirs: std::collections::BTreeMap<gluon_model::Handle<gluon_model::EspDef>, std::path::PathBuf>,
}

/// Per-node execution closure.
///
/// Each call receives the `NodeId` (the dispatcher looks up the `DagNode`
/// variant in the `Dag` itself) and two output buffers. Writing to these
/// buffers instead of directly to stdout/stderr prevents parallel jobs from
/// interleaving their compiler messages — the worker pool drains the buffers
/// to the sinks as each job completes.
///
/// `JobDispatcher: Sync` because `thread::scope` workers borrow a shared
/// `&dyn JobDispatcher` immutably from multiple threads concurrently. The
/// implementor is responsible for internal synchronisation (typically via
/// `Mutex` or atomic operations if state is needed).
pub trait JobDispatcher: Sync {
    fn dispatch(&self, node: NodeId, stdout: &mut Vec<u8>, stderr: &mut Vec<u8>) -> Result<()>;
}

// ---------------------------------------------------------------------------
// WorkerPool
// ---------------------------------------------------------------------------

/// A worker pool that drives a `Dag` to completion using a `JobDispatcher`.
pub struct WorkerPool {
    workers: usize,
}

impl WorkerPool {
    /// Create a new `WorkerPool`. `workers` is clamped to at least 1.
    pub fn new(workers: usize) -> Self {
        Self {
            workers: workers.max(1),
        }
    }

    /// The configured worker count.
    pub fn workers(&self) -> usize {
        self.workers
    }

    /// Execute the DAG to completion.
    ///
    /// See the module-level doc for the full semantics. In brief:
    ///
    /// - `workers == 1`: single-threaded, fully deterministic, no channels.
    /// - `workers > 1`: `thread::scope` + `mpsc` channels; the main thread
    ///   distributes jobs to per-worker channels so workers never contend on
    ///   a shared queue.
    /// - On first job failure: stop dispatching, wait for in-flight, collect
    ///   all errors, return `Err(Error::Diagnostics(...))`.
    /// - Deadlock: if the ready queue empties with unfinished nodes remaining,
    ///   return `Err(Error::Compile("scheduler deadlock: ..."))`.
    ///
    /// **Does not mutate `dag.in_degree`** — a working copy is kept separately
    /// so the same `Dag` can be executed multiple times (important for tests).
    pub fn execute(
        &self,
        dag: &Dag,
        dispatcher: &dyn JobDispatcher,
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> Result<()> {
        if dag.is_empty() {
            return Ok(());
        }

        if self.workers == 1 {
            execute_single(dag, dispatcher, stdout, stderr)
        } else {
            execute_parallel(dag, self.workers, dispatcher, stdout, stderr)
        }
    }
}

// ---------------------------------------------------------------------------
// Single-threaded execution
// ---------------------------------------------------------------------------

/// Single-threaded execution path.
///
/// Uses no threads or channels. The ready set is a `BTreeSet<NodeId>` on
/// the stack; the smallest NodeId is always dispatched next, giving a
/// fully deterministic execution order. This makes `-j1` suitable for
/// tests that assert topological ordering.
fn execute_single(
    dag: &Dag,
    dispatcher: &dyn JobDispatcher,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<()> {
    // Working copy of in-degrees so we do not mutate dag.in_degree.
    let mut in_degree: std::collections::BTreeMap<NodeId, u32> = dag.in_degree.clone();

    // Seed the ready set with all nodes that have in-degree 0 at the start.
    let mut ready: BTreeSet<NodeId> = in_degree
        .iter()
        .filter(|&(_, &deg)| deg == 0)
        .map(|(&id, _)| id)
        .collect();

    let total = dag.len();
    let mut finished = 0usize;
    let mut errors: Vec<Diagnostic> = Vec::new();

    while !ready.is_empty() {
        // Once an error has occurred, stop dispatching new jobs. There are no
        // in-flight jobs on the single-threaded path, so we break immediately.
        if !errors.is_empty() {
            break;
        }

        // invariant: the loop condition `while !ready.is_empty()` guarantees
        // at least one element, so `.next()` cannot return None.
        let node_id = *ready.iter().next().unwrap();
        ready.remove(&node_id);

        let mut job_stdout: Vec<u8> = Vec::new();
        let mut job_stderr: Vec<u8> = Vec::new();
        let result = dispatcher.dispatch(node_id, &mut job_stdout, &mut job_stderr);

        // Always drain the output buffers to the sinks, even on failure.
        // Propagate sink I/O errors; on failure we have nowhere useful to
        // write anyway, so bailing immediately is correct.
        stdout.write_all(&job_stdout).map_err(|e| Error::Io {
            path: std::path::PathBuf::from("<stdout>"),
            source: e,
        })?;
        stderr.write_all(&job_stderr).map_err(|e| Error::Io {
            path: std::path::PathBuf::from("<stderr>"),
            source: e,
        })?;

        finished += 1;

        match result {
            Ok(()) => {
                // Decrement in-degrees of dependents and enqueue newly-ready nodes.
                for dep in dag.dependents(node_id) {
                    let deg = in_degree.entry(dep).or_insert(0);
                    *deg = deg.saturating_sub(1);
                    if *deg == 0 {
                        ready.insert(dep);
                    }
                }
            }
            Err(e) => {
                errors.push(Diagnostic::error(format!(
                    "node {node_id}: {}",
                    error_message(&e)
                )));
                // Do not propagate dependents — they can never run.
            }
        }
    }

    // Deadlock detection: if we finished fewer nodes than the DAG contains
    // AND no error caused us to stop early, there must be a cycle.
    if errors.is_empty() && finished < total {
        return Err(render_deadlock_error(dag, &in_degree, total - finished));
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(Error::Diagnostics(errors))
    }
}

// ---------------------------------------------------------------------------
// Multi-threaded execution
// ---------------------------------------------------------------------------

/// Multi-threaded execution path using `std::thread::scope`.
///
/// ### Scheduling model
///
/// Each worker has a fixed index `[0, workers)` and owns a private
/// `mpsc::Receiver<Option<NodeId>>`. The main thread holds a corresponding
/// `Vec` of `Sender`s and a `BTreeSet<usize>` of currently-idle worker
/// indices. Scheduling proceeds as follows:
///
/// 1. While there are ready nodes AND idle workers, pop one of each and send
///    the `NodeId` directly to that worker's channel.
/// 2. Block on the result channel until any worker completes a job.
/// 3. Return that worker's index to the idle set, update in-degrees, and
///    repeat.
///
/// When in-flight reaches zero (ready queue empty and no jobs running), the
/// main thread sends `None` to all worker channels and the scope exits.
///
/// Workers borrow `dispatcher` via the scope's lifetime, so no `Arc` is
/// needed for the dispatcher itself.
fn execute_parallel(
    dag: &Dag,
    workers: usize,
    dispatcher: &dyn JobDispatcher,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<()> {
    // Working copy of in-degrees — do not mutate dag.in_degree.
    let mut in_degree: std::collections::BTreeMap<NodeId, u32> = dag.in_degree.clone();

    let mut ready: BTreeSet<NodeId> = in_degree
        .iter()
        .filter(|&(_, &deg)| deg == 0)
        .map(|(&id, _)| id)
        .collect();

    let total = dag.len();
    let mut finished = 0usize;
    let mut errors: Vec<Diagnostic> = Vec::new();
    let mut errored = false;
    let mut in_flight: usize = 0;

    // Result channel: workers send (worker_index, NodeId, Result, stdout_buf, stderr_buf).
    // Including worker_index allows the main thread to return the worker to the
    // idle set without any shared mutable state.
    let (result_tx, result_rx) = mpsc::channel::<(usize, NodeId, Result<()>, Vec<u8>, Vec<u8>)>();

    std::thread::scope(|scope| -> Result<()> {
        // Per-worker job channels. main thread → worker: Option<NodeId>.
        // None is the shutdown signal.
        let mut job_txs: Vec<mpsc::Sender<Option<NodeId>>> = Vec::with_capacity(workers);

        for worker_idx in 0..workers {
            let (job_tx, job_rx) = mpsc::channel::<Option<NodeId>>();
            job_txs.push(job_tx);
            let result_tx_clone = result_tx.clone();
            scope.spawn(move || {
                // Each worker blocks on its own private channel — no mutex, no
                // contention with other workers. `None` signals clean shutdown;
                // a channel error (sender dropped) is also treated as shutdown.
                while let Ok(Some(node_id)) = job_rx.recv() {
                    let mut job_stdout: Vec<u8> = Vec::new();
                    let mut job_stderr: Vec<u8> = Vec::new();
                    let result = dispatcher.dispatch(node_id, &mut job_stdout, &mut job_stderr);
                    // The main thread is always alive when workers run (scope
                    // lifetime), so send errors are unexpected; ignore them.
                    result_tx_clone
                        .send((worker_idx, node_id, result, job_stdout, job_stderr))
                        .ok();
                }
            });
        }
        // Drop the scope-local result_tx so result_rx detects channel close
        // once all worker clones are also dropped (i.e., all workers exit).
        drop(result_tx);

        // All workers start idle.
        // BTreeSet for deterministic ordering (lowest-index worker preferred).
        let mut idle: BTreeSet<usize> = (0..workers).collect();

        // Main scheduling loop.
        loop {
            // Dispatch as many ready nodes as possible to idle workers.
            // Stop dispatching new jobs once an error has been seen.
            if !errored {
                while let Some(&node_id) = ready.iter().next() {
                    if let Some(&worker_idx) = idle.iter().next() {
                        ready.remove(&node_id);
                        idle.remove(&worker_idx);
                        job_txs[worker_idx]
                            .send(Some(node_id))
                            .expect("worker job channel must be open during dispatch");
                        in_flight += 1;
                    } else {
                        // All workers busy — wait for a completion below.
                        break;
                    }
                }
            }

            // If nothing is in flight, we are done (or deadlocked — checked
            // after the scope exits).
            if in_flight == 0 {
                break;
            }

            // Block until any worker completes a job.
            let (worker_idx, node_id, result, job_stdout, job_stderr) = match result_rx.recv() {
                Ok(r) => r,
                Err(_) => break, // all workers exited unexpectedly
            };

            // Return this worker to the idle pool so the next scheduling pass
            // can assign it another job.
            idle.insert(worker_idx);
            in_flight -= 1;
            finished += 1;

            // Drain output buffers to the sinks atomically per job.
            // The main thread processes completions serially, so no two jobs'
            // output blocks can interleave here even though workers ran in
            // parallel.
            //
            // Collect sink I/O errors into `errors` rather than bailing
            // immediately, so in-flight jobs can still complete and the scope
            // exits cleanly.
            if let Err(e) = stdout.write_all(&job_stdout) {
                errors.push(Diagnostic::error(format!("sink I/O error (stdout): {e}")));
                errored = true;
            }
            if let Err(e) = stderr.write_all(&job_stderr) {
                errors.push(Diagnostic::error(format!("sink I/O error (stderr): {e}")));
                errored = true;
            }

            match result {
                Ok(()) => {
                    for dep in dag.dependents(node_id) {
                        let deg = in_degree.entry(dep).or_insert(0);
                        *deg = deg.saturating_sub(1);
                        if *deg == 0 && !errored {
                            ready.insert(dep);
                        }
                    }
                }
                Err(e) => {
                    errors.push(Diagnostic::error(format!(
                        "node {node_id}: {}",
                        error_message(&e)
                    )));
                    errored = true;
                    // Dependents cannot run — do not enqueue them.
                }
            }
        }

        // Send shutdown signal to all idle workers (those waiting for a job).
        // In-flight workers will receive their shutdown signal after they finish
        // their current job and loop back to recv on their channel — but at this
        // point in_flight == 0, so all workers are idle.
        for tx in &job_txs {
            let _ = tx.send(None);
        }

        Ok(())
    })?;

    // Deadlock detection.
    if errors.is_empty() && finished < total {
        return Err(render_deadlock_error(dag, &in_degree, total - finished));
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(Error::Diagnostics(errors))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract a displayable message from an `Error`.
fn error_message(e: &Error) -> String {
    match e {
        Error::Compile(msg) | Error::Config(msg) | Error::Script(msg) => msg.clone(),
        // Render each diagnostic in full via its Display impl so notes
        // (rustc stderr, command, etc.) survive the node wrapping. Joining
        // with "\n" keeps multi-diagnostic errors readable.
        Error::Diagnostics(diags) => diags
            .iter()
            .map(|d| d.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        Error::Io { path, source } => format!("I/O error at {}: {}", path.display(), source),
        // Run-path errors never reach the scheduler today; fall back to the
        // Display impl so they still render sensibly if they ever do.
        other @ (Error::NoBootBinary { .. }
        | Error::QemuBinaryNotFound { .. }
        | Error::OvmfNotFound { .. }
        | Error::EspMissing { .. }
        | Error::QemuTimeout
        | Error::QemuSpawnFailed { .. }
        | Error::UnknownQemuTarget { .. }
        | Error::KilledBySignal { .. }) => other.to_string(),
    }
}

/// Build a rich deadlock error enumerating up to 8 stuck nodes.
///
/// Called when the ready queue drains with unfinished nodes remaining,
/// indicating a cycle or unsatisfiable dependency. The error names specific
/// stuck nodes (with their `DagNode` variant and remaining in-degree) so the
/// user can identify the offending DAG edge quickly.
fn render_deadlock_error(
    dag: &Dag,
    in_degree: &std::collections::BTreeMap<NodeId, u32>,
    unfinished: usize,
) -> Error {
    let stuck: Vec<String> = in_degree
        .iter()
        .filter(|&(_, &d)| d > 0)
        .take(8)
        .map(|(&id, deg)| format!("node {id} ({:?}, in_degree={deg})", dag.nodes[id as usize]))
        .collect();
    let suffix = if stuck.len() == unfinished {
        String::new()
    } else {
        format!(" (and {} more)", unfinished - stuck.len())
    };
    Error::Compile(format!(
        "scheduler deadlock: {unfinished} nodes stuck in a cycle or on unsatisfiable \
         dependencies. First stuck: [{}]{suffix}",
        stuck.join(", "),
    ))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use crate::scheduler::dag::DagNode;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    // -----------------------------------------------------------------------
    // Test dispatcher implementations
    // -----------------------------------------------------------------------

    /// Behaviour for a single node in the `TestDispatcher`.
    #[allow(dead_code)] // Record is the explicit default; naming it clarifies intent
    enum NodeBehavior {
        /// Record the call and succeed (default).
        Record,
        /// Block on a `Barrier` before succeeding. Proves parallelism.
        Barrier(Arc<std::sync::Barrier>),
        /// Return an error with the given message.
        Fail(String),
        /// Write a deterministic byte pattern into stdout.
        Write,
    }

    /// Flexible test dispatcher with per-node configurable behaviour.
    ///
    /// Nodes without an explicit entry default to `Record` behaviour.
    struct TestDispatcher {
        /// Order in which dispatch was called.
        record: Arc<Mutex<Vec<NodeId>>>,
        /// Per-node overrides.
        behaviors: BTreeMap<NodeId, NodeBehavior>,
    }

    impl TestDispatcher {
        fn new() -> Self {
            Self {
                record: Arc::new(Mutex::new(Vec::new())),
                behaviors: BTreeMap::new(),
            }
        }

        fn with_barrier(mut self, node_id: NodeId, barrier: Arc<std::sync::Barrier>) -> Self {
            self.behaviors
                .insert(node_id, NodeBehavior::Barrier(barrier));
            self
        }

        fn with_fail(mut self, node_id: NodeId, msg: impl Into<String>) -> Self {
            self.behaviors
                .insert(node_id, NodeBehavior::Fail(msg.into()));
            self
        }

        fn with_write(mut self, node_id: NodeId) -> Self {
            self.behaviors.insert(node_id, NodeBehavior::Write);
            self
        }

        fn record(&self) -> Arc<Mutex<Vec<NodeId>>> {
            Arc::clone(&self.record)
        }
    }

    impl JobDispatcher for TestDispatcher {
        fn dispatch(
            &self,
            node: NodeId,
            stdout: &mut Vec<u8>,
            _stderr: &mut Vec<u8>,
        ) -> Result<()> {
            self.record.lock().unwrap().push(node);
            match self.behaviors.get(&node) {
                None | Some(NodeBehavior::Record) => Ok(()),
                Some(NodeBehavior::Barrier(b)) => {
                    b.wait();
                    Ok(())
                }
                Some(NodeBehavior::Fail(msg)) => Err(Error::Compile(msg.clone())),
                Some(NodeBehavior::Write) => {
                    write!(stdout, "NODE-{node}-START\nDATA\nNODE-{node}-END\n").unwrap();
                    Ok(())
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // DAG helpers
    // -----------------------------------------------------------------------

    /// Build a DAG with `n` independent nodes (no edges).
    fn independent_dag(n: u32) -> Dag {
        use gluon_model::CrateDef;
        let mut dag = Dag::new();
        for i in 0..n {
            dag.insert_node(DagNode::Crate(gluon_model::Handle::<CrateDef>::new(i)));
        }
        dag
    }

    /// Build a chain A → B → C (topological order: A first, C last).
    fn chain_dag() -> (Dag, NodeId, NodeId, NodeId) {
        use gluon_model::CrateDef;
        let mut dag = Dag::new();
        let a = dag.insert_node(DagNode::Crate(gluon_model::Handle::<CrateDef>::new(0)));
        let b = dag.insert_node(DagNode::Crate(gluon_model::Handle::<CrateDef>::new(1)));
        let c = dag.insert_node(DagNode::Crate(gluon_model::Handle::<CrateDef>::new(2)));
        dag.add_edge(a, b); // b depends on a
        dag.add_edge(b, c); // c depends on b
        (dag, a, b, c)
    }

    // -----------------------------------------------------------------------
    // Test 10: execute_empty_dag_is_noop
    // -----------------------------------------------------------------------
    #[test]
    fn execute_empty_dag_is_noop() {
        let pool = WorkerPool::new(1);
        let dispatcher = TestDispatcher::new();
        let dag = Dag::new();
        let mut out = Vec::<u8>::new();
        let mut err = Vec::<u8>::new();
        pool.execute(&dag, &dispatcher, &mut out, &mut err).unwrap();
        assert!(out.is_empty());
        assert!(err.is_empty());
        assert!(dispatcher.record.lock().unwrap().is_empty());
    }

    // -----------------------------------------------------------------------
    // Test 11: execute_single_node_dag_calls_dispatcher_once
    // -----------------------------------------------------------------------
    #[test]
    fn execute_single_node_dag_calls_dispatcher_once() {
        let pool = WorkerPool::new(1);
        let dispatcher = TestDispatcher::new();
        let record = dispatcher.record();
        let dag = independent_dag(1);
        let mut out = Vec::<u8>::new();
        let mut err = Vec::<u8>::new();
        pool.execute(&dag, &dispatcher, &mut out, &mut err).unwrap();
        let calls = record.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], 0);
    }

    // -----------------------------------------------------------------------
    // Test 12: execute_respects_topological_order
    // -----------------------------------------------------------------------
    #[test]
    fn execute_respects_topological_order() {
        let pool = WorkerPool::new(1);
        let dispatcher = TestDispatcher::new();
        let record = dispatcher.record();
        let (dag, a, b, c) = chain_dag();
        let mut out = Vec::<u8>::new();
        let mut err = Vec::<u8>::new();
        pool.execute(&dag, &dispatcher, &mut out, &mut err).unwrap();
        let calls = record.lock().unwrap().clone();
        assert_eq!(calls, vec![a, b, c], "topological order must be A → B → C");
    }

    // -----------------------------------------------------------------------
    // Test 13: execute_j1_fast_path_runs_in_ready_queue_order
    // -----------------------------------------------------------------------
    #[test]
    fn execute_j1_fast_path_runs_in_ready_queue_order() {
        let pool = WorkerPool::new(1);
        let dispatcher = TestDispatcher::new();
        let record = dispatcher.record();
        let dag = independent_dag(3);
        let mut out = Vec::<u8>::new();
        let mut err = Vec::<u8>::new();
        pool.execute(&dag, &dispatcher, &mut out, &mut err).unwrap();
        let calls = record.lock().unwrap().clone();
        // 3 independent nodes with NodeIds 0, 1, 2 must run in ascending order.
        assert_eq!(calls, vec![0, 1, 2]);
    }

    // -----------------------------------------------------------------------
    // Test 14: execute_multi_worker_runs_independent_nodes_in_parallel
    // -----------------------------------------------------------------------
    #[test]
    fn execute_multi_worker_runs_independent_nodes_in_parallel() {
        use std::time::Duration;

        const N: usize = 4;
        // The barrier requires all N workers to rendezvous before any proceeds.
        // If execution is truly parallel, all N dispatches happen concurrently
        // and the barrier is satisfied. If the pool runs serially, the first
        // worker blocks forever — caught by the recv_timeout below.
        let barrier = Arc::new(std::sync::Barrier::new(N));

        let pool = WorkerPool::new(N);
        let mut dispatcher = TestDispatcher::new();
        for i in 0..N as u32 {
            dispatcher = dispatcher.with_barrier(i, Arc::clone(&barrier));
        }

        // Wrap in recv_timeout so a parallelism regression fails with a clear
        // message rather than hanging the test runner indefinitely.
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let dag = independent_dag(N as u32);
            let mut out = Vec::<u8>::new();
            let mut err = Vec::<u8>::new();
            let result = pool.execute(&dag, &dispatcher, &mut out, &mut err);
            let _ = tx.send(result);
        });
        let result = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("parallel execution timed out — scheduler may be running serially");
        result.expect("parallel execution must succeed");
    }

    // -----------------------------------------------------------------------
    // Test 15: execute_per_job_stdout_is_not_interleaved
    // -----------------------------------------------------------------------
    #[test]
    fn execute_per_job_stdout_is_not_interleaved() {
        const N: u32 = 4;
        let pool = WorkerPool::new(N as usize);
        let mut dispatcher = TestDispatcher::new();
        for i in 0..N {
            dispatcher = dispatcher.with_write(i);
        }

        let dag = independent_dag(N);
        let mut out = Vec::<u8>::new();
        let mut err = Vec::<u8>::new();
        pool.execute(&dag, &dispatcher, &mut out, &mut err).unwrap();

        let output = String::from_utf8(out).unwrap();
        // Each node's output block must appear as a contiguous substring.
        for i in 0..N {
            let block = format!("NODE-{i}-START\nDATA\nNODE-{i}-END\n");
            assert!(
                output.contains(&block),
                "node {i} output block is missing or was interleaved:\n{output}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Test 16: execute_j1_deterministic_output_interleaving_across_runs
    // -----------------------------------------------------------------------
    #[test]
    fn execute_j1_deterministic_output_interleaving_across_runs() {
        let pool = WorkerPool::new(1);

        let run = || {
            let mut dispatcher = TestDispatcher::new();
            for i in 0..5 {
                dispatcher = dispatcher.with_write(i);
            }
            let dag = independent_dag(5);
            let mut out = Vec::<u8>::new();
            let mut err = Vec::<u8>::new();
            pool.execute(&dag, &dispatcher, &mut out, &mut err).unwrap();
            out
        };

        let first = run();
        let second = run();
        assert_eq!(
            first, second,
            "j1 output must be byte-identical across runs"
        );
    }

    // -----------------------------------------------------------------------
    // Test 17: execute_reports_first_error_and_waits_for_in_flight
    // -----------------------------------------------------------------------
    #[test]
    fn execute_reports_first_error_and_waits_for_in_flight() {
        const N: u32 = 4;
        let pool = WorkerPool::new(N as usize);
        let dispatcher = TestDispatcher::new().with_fail(2, "boom-2");
        let record = dispatcher.record();

        let dag = independent_dag(N);
        let mut out = Vec::<u8>::new();
        let mut err = Vec::<u8>::new();
        let result = pool.execute(&dag, &dispatcher, &mut out, &mut err);

        // Must return an error.
        match result {
            Err(Error::Diagnostics(diags)) => {
                let combined: String = diags
                    .iter()
                    .map(|d| d.message.clone())
                    .collect::<Vec<_>>()
                    .join(" ");
                assert!(
                    combined.contains("boom-2"),
                    "error message must mention 'boom-2': {combined}"
                );
            }
            other => panic!("expected Err(Diagnostics), got {other:?}"),
        }

        // All 4 nodes must have been dispatched and completed (in-flight jobs
        // run to completion even after an error).
        let calls = record.lock().unwrap();
        assert_eq!(
            calls.len(),
            4,
            "all 4 dispatches must complete before the pool returns: got {}",
            calls.len()
        );
    }

    // -----------------------------------------------------------------------
    // Test 18: execute_detects_deadlock
    // -----------------------------------------------------------------------
    #[test]
    fn execute_detects_deadlock() {
        use gluon_model::CrateDef;

        // Both workers=1 and workers=2 should detect the deadlock.
        for workers in [1usize, 2] {
            let pool = WorkerPool::new(workers);
            let dispatcher = TestDispatcher::new();

            // Build a cyclic DAG: 0 → 1 → 0.
            let mut dag = Dag::new();
            let a = dag.insert_node(DagNode::Crate(gluon_model::Handle::<CrateDef>::new(0)));
            let b = dag.insert_node(DagNode::Crate(gluon_model::Handle::<CrateDef>::new(1)));
            dag.add_edge(a, b);
            dag.add_edge(b, a);

            let mut out = Vec::<u8>::new();
            let mut err = Vec::<u8>::new();
            let result = pool.execute(&dag, &dispatcher, &mut out, &mut err);
            match result {
                Err(Error::Compile(msg)) => {
                    assert!(
                        msg.contains("deadlock"),
                        "error must mention 'deadlock' (workers={workers}): {msg}"
                    );
                    // The enhanced error must name at least one stuck node id.
                    assert!(
                        msg.contains("node 0") || msg.contains("node 1"),
                        "error must reference stuck node ids (workers={workers}): {msg}"
                    );
                }
                other => {
                    panic!("expected deadlock Err(Compile), got {other:?} (workers={workers})")
                }
            }
        }
    }
}
