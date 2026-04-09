# Contributing to Gluon

## Getting started

```bash
# Clone and enter the repo
git clone https://github.com/hxyulin/gluon.git
cd gluon

# The nightly toolchain + components are pinned in rust-toolchain.toml —
# rustup will install them automatically on the first cargo invocation.

# Run the full test suite
cargo test --workspace

# Run fmt and clippy (CI enforces both)
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

## Project structure

| Crate | Purpose |
|-------|---------|
| `gluon-model` | Data types shared across the build pipeline |
| `gluon-core` | Engine, compiler, scheduler, cache, vendor, kconfig |
| `gluon-cli` | `clap`-based CLI (`gluon build`, `run`, `vendor`, ...) |

## Commit messages

Follow [Conventional Commits](https://www.conventionalcommits.org/):

```
<type>(<scope>): <short description>
```

- **Types:** `feat`, `fix`, `refactor`, `test`, `docs`, `build`, `chore`
- **Scope:** optional but encouraged (`compile`, `engine`, `cli`, ...)
- Imperative mood, lowercase, no trailing period, max ~72 chars
- No `Co-Authored-By` or other footers
- Default to subject-only; use 1-2 bullet points in the body only when
  the subject line is not self-explanatory

## Pull requests

- Keep PRs focused — one logical change per PR
- Ensure `cargo test --workspace`, `cargo fmt --check`, and
  `cargo clippy -- -D warnings` all pass before opening
- Add or update tests for any new or changed behavior
- Update documentation (mdbook pages, doc comments) if the user-facing
  surface changes

## Design guidelines

Gluon is a **kernel-agnostic** build system. Contributions should:

- Not bake assumptions about a specific kernel, target triple, or
  bootloader into core code — that belongs in `gluon.rhai`
- Prefer deterministic, reproducible behavior (stable ordering,
  no reliance on `HashMap` iteration order)
- Surface errors with context — build failures should point the user at
  the offending crate, file, or config option
- Use `unsafe` only when strictly necessary, isolated behind safe APIs

See [CLAUDE.md](CLAUDE.md) for the full engineering guidelines.

## Documentation

The mdbook documentation lives in `docs/src/`. To preview locally:

```bash
# Install mdbook if needed
cargo install mdbook

# Build and serve with live reload
cd docs
mdbook serve --open
```

## License

By contributing, you agree that your contributions will be licensed
under the [GPL-3.0-or-later](LICENSE) license.
