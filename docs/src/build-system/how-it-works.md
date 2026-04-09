# How Gluon Builds Your Code

Gluon's build pipeline has six stages. Understanding this pipeline gives you
a mental model for how configuration turns into bootable artifacts, and why
certain things (like sysroot compilation) only happen on the first build.

## The pipeline

### 1. Evaluate

Gluon parses and executes your `gluon.rhai` script. The script declares
projects, targets, crate groups, profiles, rules, pipelines, and QEMU
settings. The result is a **BuildModel** -- a structured, in-memory
representation of everything the build needs to know.

### 2. Resolve

The BuildModel is resolved for the chosen profile. This step flattens
profile inheritance chains, applies config overrides, resolves option
dependencies, and produces a **ResolvedConfig** -- a single, flat
configuration with no ambiguity about what flags, features, or options
apply.

### 3. Sysroot

Bare-metal targets need `core`, `alloc`, and `compiler_builtins` compiled
from source for the target triple. Gluon checks whether a cached sysroot
exists and is still fresh. If the sysroot is missing or stale, Gluon
compiles it before anything else. See [Custom Sysroots](./sysroots.md)
for details.

### 4. DAG construction

Gluon walks the resolved crates and their dependencies to build a
**directed acyclic graph** (DAG) of compilation tasks. The DAG contains
several node types:

- **Crate nodes** -- your source crates, compiled with per-crate flags
- **Sysroot nodes** -- the sysroot compilation tasks
- **ConfigCrate nodes** -- an auto-generated crate that exposes build
  configuration to your code
- **Rule nodes** -- user-defined build steps (copy, exec, mkimage)
- **ESP nodes** -- EFI System Partition assembly tasks
- **Image nodes** -- disk image creation tasks

Edges in the DAG encode "must be built before" relationships. A crate that
depends on another crate cannot start compiling until its dependency
finishes.

### 5. Schedule

The DAG is handed to a parallel worker pool. The pool size is configurable
with the `-j` flag (e.g., `gluon build -j4`). The scheduler picks nodes
whose dependencies are all satisfied, dispatches them to workers, and
continues until the graph is fully executed or an error occurs.

### 6. Per-crate compilation

For each crate node the scheduler dispatches, Gluon:

1. **Checks the build cache.** If the crate's sources, flags, and
   dependencies have not changed, the cached artifact is reused. See
   [Build Cache](./build-cache.md).
2. **Assembles rustc flags.** Each crate gets its own set of flags:
   edition, optimization level, `--cfg` flags, feature flags, extern
   paths, linker script, sysroot path, and more.
3. **Invokes rustc directly.** Gluon calls `rustc` as a subprocess --
   there is no Cargo involved.
4. **Parses the depfile.** Rustc emits a `.d` dependency file listing
   every source file the crate touched. Gluon records these for future
   cache invalidation.
5. **Records the result.** On success, the artifact path and freshness
   metadata are written to the cache manifest.

## Key difference from Cargo

Cargo is a package manager that also builds code. It controls the `rustc`
invocation but limits how much you can customize per-crate flags, sysroot
paths, linker scripts, and cross-target compilation within a single
workspace.

Gluon drives `rustc` directly and assembles flags per-crate from your
`gluon.rhai` configuration. This gives full control over:

- Sysroot paths (custom-compiled `core`/`alloc` for your target)
- Per-crate `--cfg` flags and features
- Linker scripts
- Mixed-target compilation (e.g., a UEFI bootloader and a bare-metal
  kernel in the same build)
- Build rules and pipelines for post-processing steps
