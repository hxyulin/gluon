# Pipelines and Rules

Compilation is only part of a bare-metal build. After compiling, you often
need to copy binaries, strip symbols, create disk images, or assemble
boot partitions. Gluon handles these post-processing steps through
**rules** and **pipelines**.

## Rules

A rule is a custom build step with inputs, outputs, and a handler that
defines what the step does. Rules participate in the build DAG just like
crate compilations -- they respect dependency ordering and are subject to
caching.

### Built-in rule types

Gluon provides three built-in handlers:

**copy** -- copy a file from one location to another:

```rhai
rule("copy-kernel")
    .handler("copy")
    .input("${build_dir}/kernel")
    .output("${build_dir}/boot/kernel.elf");
```

**exec** -- run an arbitrary command:

```rhai
rule("strip-kernel")
    .handler("exec")
    .command("llvm-objcopy")
    .args(["-O", "binary", "${input}", "${output}"])
    .input("${build_dir}/kernel")
    .output("${build_dir}/kernel.bin");
```

**mkimage** -- create disk images (the exact behavior is
implementation-dependent and may vary by target and boot protocol).

### Variable substitution

Rule arguments support variable substitution using `${...}` syntax. The
following variables are available:

| Variable       | Description                                      |
|----------------|--------------------------------------------------|
| `${build_dir}` | The build output directory for the current target and profile |
| `${target}`    | The target triple                                |
| `${profile}`   | The active profile name                          |
| `${input}`     | The rule's input path (resolved)                 |
| `${output}`    | The rule's output path (resolved)                |

Variables are resolved at execution time, so they always reflect the
current build context.

## Pipelines

A pipeline defines an ordered sequence of build stages. Each stage
references either a crate group (for compilation) or a rule (for
post-processing).

```rhai
pipeline("default")
    .stage("compile", "kernel")
    .stage("post", "copy-kernel")
    .stage("post", "strip-kernel");
```

### Stage types

- **`"compile"`** -- compile the named crate group. All crates in the
  group and their dependencies are compiled in parallel according to
  the DAG.
- **`"post"`** -- execute the named rule. Post-processing stages run
  after all compilation stages they depend on have completed.

### Execution order

Stages execute in declaration order. Within a stage, the DAG scheduler
ensures all dependencies are satisfied before a task starts. This means:

1. The `"compile"` stage for `"kernel"` runs first, compiling all crates
   in the kernel group.
2. The `"post"` stage for `"copy-kernel"` runs next, but only after the
   kernel binary exists.
3. The `"post"` stage for `"strip-kernel"` runs last.

If two stages have no dependency relationship, the scheduler may execute
them in parallel.

### Multiple pipelines

You can define multiple pipelines for different purposes:

```rhai
pipeline("default")
    .stage("compile", "kernel")
    .stage("post", "copy-kernel");

pipeline("release")
    .stage("compile", "kernel")
    .stage("post", "strip-kernel")
    .stage("post", "mkimage-disk");
```

Select a pipeline at build time with `gluon build --pipeline release`.
