# Gluon

## Overview

Gluon is a build system for bare-metal Rust kernels. It bypasses Cargo,
invokes `rustc` directly with per-crate flag assembly, compiles custom
sysroots (`core`, `alloc`, `compiler_builtins`) for arbitrary target
triples, and orchestrates the full pipeline from configuration through
compilation to bootable artifacts and QEMU runs. The build model is
declared in a Rhai-scripted configuration file.

Gluon is intended to be **kernel-agnostic** and reusable across bare-metal
Rust projects. It is a host-side userland tool, not a kernel — so it has
no `no_std` or zero-dependency constraints; mature crates are used where
they pull their weight.

## Collaboration Instructions

1. **Always ask for clarification rather than assuming intent.** If a
   request is vague, ambiguous, or appears incorrect, stop and ask
   before acting.
2. **Consider multiple approaches.** When responding to a request, think
   through alternatives and surface better options if they exist —
   explain the trade-offs rather than silently picking one.
3. **Be educational.** Offer suggestions, guidelines, and brief
   explanations of *why* an approach is preferred. Build-system design
   has a lot of subtle trade-offs (incrementality, parallelism, cache
   invalidation, dependency resolution); treat each interaction as a
   chance to make those trade-offs explicit.
4. **Plan before you build.** Every feature — no matter how small it
   seems — must be planned thoroughly before any code is written.
   Discuss the design, edge cases, and integration points with the user
   first, and only begin implementation once the plan is agreed upon.

## Engineering Guidelines

1. **Stay kernel-agnostic.** Gluon is a *general-purpose* build system
   for bare-metal Rust. Do not bake assumptions about a specific kernel,
   target triple, bootloader, or boot protocol into the core. Such
   behavior belongs in user configuration (`gluon.rhai`), not in code.
2. **Pragmatic dependency policy.** External crates are allowed and
   encouraged where they pull their weight (`clap`, `serde`, `rhai`,
   `ratatui`, etc.). Prefer mature, widely-used crates over hand-rolling.
   Avoid pulling in trivial micro-crates or overlapping ecosystems.
3. **Determinism and reproducibility first.** Build outputs must be
   reproducible: stable iteration order, deterministic hashing, no
   reliance on filesystem ordering or HashMap iteration. A given
   `gluon.rhai` + source tree must always produce the same artifacts.
4. **Correct cache invalidation is non-negotiable.** A build system that
   silently uses stale outputs is worse than one that's slow. When in
   doubt, invalidate. Document the freshness model wherever it's checked.
5. **Minimize `unsafe`.** Gluon is a host tool — there are very few
   legitimate reasons for `unsafe` here. If one arises, isolate it,
   wrap it in a safe API, and document the invariants.
6. **Surface errors with context.** Build failures must point the user
   at the offending crate, file, rustc invocation, or config option.
   Anonymous errors are bugs.
7. **Document intent, not mechanics.** Comments should explain *why*
   code exists or *why* an approach was chosen — especially around
   caching, scheduling, and rustc flag assembly, where the "what" is
   often obvious but the "why" is not.
