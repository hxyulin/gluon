# Editor Setup

Before configuring your editor, run `gluon configure` to generate the
`rust-project.json` file. Without it, rust-analyzer has no project model to
work with.

## VS Code

Install the
[rust-analyzer extension](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer).

After running `gluon configure`, VS Code should automatically detect the
`rust-project.json` in the project root. If it does not, add the following to
`.vscode/settings.json`:

```json
{
    "rust-analyzer.linkedProjects": ["rust-project.json"]
}
```

## Neovim

If using [nvim-lspconfig](https://github.com/neovim/nvim-lspconfig), configure
rust-analyzer to use the project file:

```lua
require('lspconfig').rust_analyzer.setup {
    settings = {
        ['rust-analyzer'] = {
            linkedProjects = { 'rust-project.json' },
        },
    },
}
```

If you use a different LSP client, the key setting is the same: point
`linkedProjects` at your `rust-project.json` path.

### Gluon DSL highlighting (gluon-lsp)

For rich editing of `gluon.rhai` — completions, hover signatures,
diagnostics, and DSL-specific syntax highlighting — install and register
`gluon-lsp`:

1. Install the binary:

   ```sh
   cargo install --path crates/gluon-lsp
   ```

2. Install `tree-sitter-rhai` for base Rhai syntax (strings, comments,
   numbers, keywords):

   ```
   :TSInstall rhai
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

   DSL-specific highlighting (Gluon builtins and builder methods) is
   delivered automatically via LSP semantic tokens — no query files to
   copy or maintain. Neovim 0.9+ applies semantic tokens out of the box.

## Helix

Add the following to `.helix/languages.toml` in your project root:

```toml
[language-server.rust-analyzer.config]
linkedProjects = ["rust-project.json"]
```

## Zed

Zed supports rust-analyzer natively. Add to your project's `.zed/settings.json`:

```json
{
    "lsp": {
        "rust-analyzer": {
            "initialization_options": {
                "linkedProjects": ["rust-project.json"]
            }
        }
    }
}
```

## General notes

- Run `gluon configure` **before** opening the editor so the JSON file exists
  when rust-analyzer starts.
- If rust-analyzer shows errors about missing crates, try re-running `gluon
  configure` followed by restarting the language server (most editors have a
  "Restart Server" command).
- The generated config crate gets a stub `lib.rs` during `gluon configure`.
  This stub is replaced with real generated content on the next `gluon build`.
  After building, re-run `gluon configure` and restart the language server to
  pick up the real constants.
- If you change the output path with `gluon configure --output`, update your
  editor's `linkedProjects` setting to match.
