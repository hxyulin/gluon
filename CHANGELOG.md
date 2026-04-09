# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [0.1.0] - 2026-04-09

### Added

- **Rhai-scripted build configuration** — declare projects, targets,
  profiles, groups, and crates in `gluon.rhai`
- **Direct `rustc` invocation** — bypass Cargo, assemble per-crate
  flags, and compile custom sysroots (`core`, `alloc`,
  `compiler_builtins`) for arbitrary target triples
- **Build cache** — hybrid mtime + SHA-256 freshness checking with
  depfile tracking and file-level locking
- **DAG scheduler** — parallel worker pool with dependency-ordered
  compilation
- **Pipelines and rules** — user-definable build rules with an `exec`
  builtin for custom build steps
- **Config resolution** — interpolation, per-target overrides, and
  profile-based filtering
- **Kconfig-style typed configuration** — `.kconfig` file loader with
  bool/int/string/hex options, defaults, dependencies, and `select`
- **`gluon build`** — full compilation pipeline from config evaluation
  through sysroot and crate compilation
- **`gluon run`** — QEMU boot for direct and UEFI targets with
  `--no-build`, `--gdb`, signal forwarding, per-target QEMU args, and
  test mode
- **`gluon vendor`** — vendor external crate dependencies with lockfile
  tracking, checksums, and fingerprint-based staleness detection
- **`gluon check` / `clippy` / `fmt`** — forward to `rustc`/`clippy`/
  `rustfmt` with correct sysroot and flag assembly
- **UEFI bootloader+kernel support** — `artifact_env` for cross-crate
  artifact paths, ESP directory assembly, multi-target builds
- **Per-profile filtering** — select which crates and groups apply to
  each profile
- **`depends_on` expressions** — declare inter-crate build dependencies
  in Rhai config
- **CI pipeline** — GitHub Actions with fmt, clippy, test, vendor e2e,
  and dogfood (`gluon fmt --check`) jobs
- **mdbook documentation** — 26 pages covering getting started,
  configuration, build system internals, running/debugging, CLI
  reference, tooling integration, and cookbook recipes
- **Example `gluon.rhai` scripts** — basic kernel, UEFI bootloader, and
  Kconfig-style configuration examples
- **rust-analyzer integration** — `rust-project.json` generation for
  IDE support in bare-metal projects

[unreleased]: https://github.com/hxyulin/gluon/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/hxyulin/gluon/releases/tag/v0.1.0
