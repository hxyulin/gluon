# Gluon

> Gluon is a build system for bare-metal Rust kernels. It bypasses Cargo,
> drives `rustc` directly, builds custom sysroots, and orchestrates the
> full pipeline from configuration through compilation to bootable
> artifacts and QEMU runs.

## Status

Gluon is functional and actively developed. Current capabilities:

- **Build / Check / Clippy / Fmt** — full compile pipeline driving `rustc` directly, with metadata-only check and lint passes
- **Custom sysroots** — compiles `core`, `alloc`, and `compiler_builtins` for arbitrary target triples
- **Hybrid build cache** — two-tier mtime + SHA-256 freshness with parent-directory tracking
- **DAG scheduler** — parallel, dependency-aware build graph with worker pool
- **Rhai configuration** — declarative `gluon.rhai` build scripts with projects, targets, profiles, groups, and crates
- **Kconfig support** — full lexer/parser/lowerer for Linux-style Kconfig files
- **Dependency vendoring** — `gluon vendor` wrapping `cargo vendor` with lockfile and fingerprinting
- **QEMU orchestration** — `gluon run` with direct and UEFI boot, GDB server, timeouts, and signal handling
- **rust-analyzer integration** — `gluon configure` generates `rust-project.json`
- **Pipeline and rule system** — user-defined build stages with built-in rules (copy, mkimage, exec)

## License

Gluon is licensed under the GNU General Public License v3.0. See [LICENSE](LICENSE) for more details.
