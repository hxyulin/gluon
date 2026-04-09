# Build Cache

Gluon uses a hybrid two-tier freshness model to avoid unnecessary
recompilation. The goal is to never recompile a crate whose inputs have
not changed, while also never silently using a stale artifact.

## How it works

### Fast path: mtime + size

For each source file, Gluon compares the last-modified time and file size
against the values recorded in the cache manifest. If both match, the file
is considered fresh. This check is nearly free -- it requires only a
`stat()` call per file.

### Slow path: SHA-256

When the mtime differs but the file might not have actually changed, Gluon
falls back to comparing SHA-256 content hashes. This happens in situations
like:

- `git checkout` or `git rebase` that updates mtimes without changing
  content
- CI cache restores that do not preserve timestamps
- Editors that write-then-rename (updating mtime even for no-op saves)

If the hashes match, the mtime was a false positive. Gluon refreshes the
cached mtime to avoid repeating the hash computation on the next build,
and skips recompilation.

## What triggers recompilation

A crate is recompiled when **any** of the following change:

- **Source files** -- content change detected via the two-tier model
  described above.
- **Rustc flags** -- the assembled command-line flags for the crate are
  hashed into an `argv_hash`. If the flags change (e.g., a new `--cfg`
  flag or a different optimization level), the crate is recompiled even
  if no source files changed.
- **Dependencies** -- if a dependency was recompiled (producing a new
  artifact), all of its dependents are invalidated. This is transitive:
  a change in `core` invalidates everything.

## Cache manifest

Freshness records are persisted in `build/cache-manifest.json`. This file
is written after each successful build and loaded at the start of the next
build. It contains, for each crate:

- Source file paths with their cached mtime, size, and SHA-256 hash
- The `argv_hash` of the rustc invocation
- The output artifact path

If the manifest is missing or corrupt, Gluon treats all crates as stale
and performs a full rebuild.

## Build output layout

```
build/
  sysroot/<target>/           # Custom sysroot (shared across profiles)
  cross/<target>/<profile>/
    deps/                     # Cross-compiled crate artifacts
    esp/                      # EFI partitions
  host/                       # Host-compiled crates (proc macros, etc.)
  tool/check/                 # Output from gluon check
  tool/clippy/                # Output from gluon clippy
  cache-manifest.json         # Freshness records
```

Key layout decisions:

- The **sysroot** is shared across profiles and drivers (build, check,
  clippy). Compiling it once per target triple is sufficient.
- **Cross-compiled artifacts** are separated by target and profile. A
  debug build and a release build never share artifacts.
- The **`tool/`** subdirectories ensure that `check` and `clippy` outputs
  do not collide with `build` artifacts. Running `gluon check` will not
  invalidate your last `gluon build`.
- **Host-compiled crates** (such as procedural macros) live in a separate
  `host/` directory because they are compiled for the host triple, not the
  target triple.
