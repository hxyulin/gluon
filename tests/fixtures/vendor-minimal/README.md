# vendor-minimal — sub-project #3 fixture

Exercises the `gluon vendor` pipeline end-to-end. Declares two external
dependencies:

1. `vendor_minimal_helper` via `.path("../vendor-minimal-helper")` — a
   `DepSource::Path` dep whose target lives in a sibling fixture
   directory. No network access, always runnable.

2. `bitflags` via `.version("2.11")` — a `DepSource::CratesIo` dep.
   Exercises the real `cargo vendor` shell-out. Gated behind
   `--features gluon-core/network-tests` in the integration test.

`gluon.path-only.rhai` is a variant that omits the crates.io dep and
is used by the no-network integration test (it is copied over
`gluon.rhai` inside the per-test tempdir, so the checked-in tree is
never mutated).

## Vendor directory policy

`.gitignore` ignores `/vendor/` by default — the vendored crate
sources are regenerated on demand by `gluon vendor`. The
authoritative pin lives in:

- `gluon.lock` (checked in)
- `build/vendor-workspace/Cargo.lock` (checked in via a `.gitignore`
  carveout — not yet enabled in this fixture; see
  `~/.claude/plans/proud-zooming-boole.md` §8)

To switch to the "committed vendor" mode (fully offline fresh-clone
builds), delete the `/vendor/` line from `.gitignore` and commit the
directory.

## Running the end-to-end tests

Path-only (always):

```
cargo test -p gluon-core --test vendor_e2e
```

Network-gated (pulls `bitflags` from crates.io):

```
cargo test -p gluon-core --test vendor_e2e --features gluon-core/network-tests
```
