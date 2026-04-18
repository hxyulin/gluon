# gluon-lsp

Language server for Gluon's Rhai DSL (`gluon.rhai`).

## Architecture

- **Transport:** stdio only. Editors launch `gluon-lsp` as a child
  process; LSP traffic flows over stdin/stdout. No TCP, no async runtime.
- **Parser:** `tree-sitter-rhai` (vendored at `crates/tree-sitter-rhai`).
- **Semantic layer:** in-tree semantic frontend that consumes the same
  `DslSchema` the real build pipeline uses — so the completion and
  hover surface never drifts from what `gluon build` actually accepts.
  Adding a builder in `gluon-core` makes it available to the LSP on the
  next restart, with no hand-maintained symbol list.
- **Runtime:** single-threaded event loop (`lsp-server`, the same crate
  rust-analyzer uses). Deliberately avoids `tower-lsp` — its async
  requirement is overkill for this workload.

## CLI

```
gluon-lsp --help      # usage
gluon-lsp --version   # version
gluon-lsp             # run as an LSP server over stdio (default)
```

Editors invoke the binary with no arguments. `--help` and `--version`
exist so humans can verify their install.

## How highlighting works

| Layer | What it covers | Provided by |
|-------|---------------|-------------|
| Base Rhai syntax (strings, comments, numbers, keywords, operators) | Tree-sitter grammar | `tree-sitter-rhai` |
| Gluon DSL builtins and builder methods | Semantic token classification | `gluon-lsp` |

The LSP emits semantic tokens for every call-site it recognises — the
editor applies the theme colours automatically, with no query files to
maintain or regenerate.

## Editor setup (Neovim)

1. Install `tree-sitter-rhai` for base Rhai syntax:

   ```
   :TSInstall rhai
   ```

2. Install `gluon-lsp`:

   ```sh
   cargo install --path crates/gluon-lsp
   ```

3. Register `gluon-lsp` with nvim-lspconfig (Neovim 0.9+):

   ```lua
   local lspconfig = require('lspconfig')
   local configs = require('lspconfig.configs')

   if not configs.gluon then
     configs.gluon = {
       default_config = {
         cmd = { 'gluon-lsp' },
         filetypes = { 'rhai' },
         root_dir = lspconfig.util.root_pattern('gluon.rhai'),
       },
     }
   end

   lspconfig.gluon.setup({})
   ```

   Neovim 0.9+ applies semantic tokens automatically; no additional
   plugin is required. DSL builtins will be highlighted as soon as the
   LSP attaches to a `gluon.rhai` buffer.

## Capabilities advertised

- Full-text sync (`textDocument/didOpen`, `didChange`, `didClose`)
- Completion (`textDocument/completion`) with `.` as a trigger character
- Hover (`textDocument/hover`)
- Semantic tokens (`textDocument/semanticTokens/full`)
- Diagnostics (`textDocument/publishDiagnostics`)
