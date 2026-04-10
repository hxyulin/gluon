# tree-sitter-gluon

Editor query overrides that teach your Rhai tree-sitter grammar about
Gluon's DSL surface — `project`, `target`, `group`, `config_option`,
`qemu`, and all the builder methods — so they get highlighted as
builtins instead of plain identifiers.

> **This is not a fork of `tree-sitter-rhai`.** There is no grammar
> here. We use the upstream Rhai grammar unchanged and layer these
> queries on top. The Rhai DSL's syntax is plain Rhai.

## What this pack gives you

- **`queries/highlights.scm`** — recolors every function that Gluon
  registers at runtime as `@function.builtin`. Generated from a live
  Gluon engine via `gluon internal dump-dsl`, so it can never drift
  out of sync with the `register_fn` calls in
  `crates/gluon-core/src/engine/builders/`.

## What this pack does **not** give you

- **Semantic autocomplete.** Tree-sitter is a parser — it has no
  concept of types, function signatures, or scope. If you want to see
  what methods exist on a `CrateBuilder` after `.add(...)`, you need
  an LSP. That is what `crates/gluon-lsp/` exists to provide. Install
  both for the full experience.
- **Locals / indent / folds for base Rhai.** Install upstream
  [`tree-sitter-rhai`](https://github.com/lf-lang/tree-sitter-rhai)
  first; these queries layer on top of what it already provides.

## Install

### Neovim (with nvim-treesitter)

1. Install `tree-sitter-rhai` per `:TSInstall rhai` (requires a parser
   registered for the `rhai` filetype).
2. Drop the file into your runtime queries path as an override:

   ```sh
   mkdir -p ~/.config/nvim/after/queries/rhai
   cp editor/tree-sitter-gluon/queries/highlights.scm \
      ~/.config/nvim/after/queries/rhai/highlights.scm
   ```

   Files under `after/queries/<lang>/` are loaded *after* the base
   grammar's queries, so Gluon's builtins take precedence over any
   generic `@function` capture.

### Helix

1. Ensure Rhai is in your `languages.toml` or that Helix ships a Rhai
   grammar in your runtime.
2. Copy the highlights into Helix's runtime queries directory:

   ```sh
   mkdir -p ~/.config/helix/runtime/queries/rhai
   cp editor/tree-sitter-gluon/queries/highlights.scm \
      ~/.config/helix/runtime/queries/rhai/highlights.scm
   ```

### Zed

At the time of writing, Zed's Rhai support is community-driven. Point
a local extension at this queries directory, or copy the file into the
extension's `languages/rhai/` folder.

## Regenerating `highlights.scm`

Whenever gluon's DSL surface changes (a new `register_fn` in
`crates/gluon-core/src/engine/builders/`), refresh the query list:

```sh
./scripts/regen-dsl-queries.sh
```

The script invokes `cargo run --bin gluon -- internal dump-dsl` and
rewrites `queries/highlights.scm`. Review the diff, commit both the
engine change and the regenerated queries in the same commit.
