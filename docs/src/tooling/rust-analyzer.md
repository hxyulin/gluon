# rust-analyzer

## Why is this needed?

Gluon does not use `Cargo.toml`, so rust-analyzer has no project model out of
the box. Without a project model, the language server cannot resolve crates,
dependencies, or target-specific `cfg` flags -- meaning no completions, no
go-to-definition, and spurious error highlights everywhere.

The `gluon configure` command generates a `rust-project.json` file that gives
rust-analyzer everything it needs to understand your project.

## Generating rust-project.json

```sh
gluon configure
```

This creates `rust-project.json` in your project root. To write it elsewhere:

```sh
gluon configure --output ./ide/rust-project.json
```

Re-run `gluon configure` whenever you:

- Add or remove crates in `gluon.rhai`
- Change dependencies between crates
- Change target triples or `cfg` flags
- Modify features or config options

## What it includes

The generated `rust-project.json` contains:

- **Every crate** in your build model with its root module path
- **Dependency relationships** between crates (including the generated config
  crate if `.config(true)` is set on a group)
- **The custom sysroot path** so rust-analyzer resolves `core`, `alloc`, and
  `compiler_builtins` correctly for bare-metal targets
- **`--cfg` flags and features** for each crate, matching what `gluon build`
  would pass to `rustc`
- **The auto-generated config crate** -- if it has not been built yet, `gluon
  configure` writes a stub `lib.rs` so the language server can at least parse
  the dependency graph

## Editor configuration

Most editors with rust-analyzer support will automatically detect
`rust-project.json` in the project root. If your editor does not pick it up
automatically, see [Editor Setup](./editor-setup.md) for per-editor
instructions.

## Troubleshooting

**rust-analyzer shows "failed to find sysroot"** -- Run `gluon build` at least
once so the sysroot is compiled, then re-run `gluon configure`.

**Completions are missing for config constants** -- The config crate is
generated during `gluon build`. After building, re-run `gluon configure` and
restart the language server.

**Changes to `gluon.rhai` are not reflected** -- `rust-project.json` is a
snapshot. Re-run `gluon configure` after editing the build script, then restart
the language server.
