# tree-sitter-gluon

> **This directory no longer ships query files.**
>
> DSL-specific highlighting (Gluon builtins such as `project`, `target`,
> `group`, `config_option`, `qemu`, and all builder methods) is now
> delivered by **gluon-lsp** via LSP semantic tokens. There is no
> `highlights.scm` to copy.

## How highlighting works now

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
