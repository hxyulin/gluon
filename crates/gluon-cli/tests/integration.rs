//! End-to-end CLI integration tests for `gluon-cli`.
//!
//! These tests spawn the real `gluon` binary (via `env!("CARGO_BIN_EXE_gluon")`)
//! against a copy of `tests/fixtures/minimal/` that has been recursively
//! copied into a `tempfile::TempDir`. Running against a copy (rather than
//! the on-disk fixture in the source tree) keeps the workspace clean: no
//! `build/` directories, cached manifests, or generated `rust-project.json`
//! files leak back into the repository between test runs.
//!
//! ## What's `#[ignore]`-gated and why
//!
//! Tests that reach `gluon-cli`'s `build_context` path — `gluon build`
//! and `gluon configure` — call `RustcInfo::load_or_probe`, which
//! spawns `rustc -vV`. `gluon build` also needs the `rust-src`
//! component to build the custom sysroot. Spawning rustc (let alone
//! building core/alloc) isn't acceptable in every CI sandbox, so
//! those tests are `#[ignore]`-gated and use `require_rustc_or_skip`
//! to *skip* rather than fail when the toolchain is unavailable.
//! This mirrors the probe-or-skip + `#[ignore]` pattern used by the
//! scheduler end-to-end tests in `gluon-core`.
//!
//! Tests that do **not** require rustc run in the default suite:
//!   - The Rhai-diagnostic test deliberately corrupts the script so
//!     that `evaluate()` fails before any rustc probe.
//!   - The `gluon clean` tests use `build_layout_context`, which
//!     stops short of the rustc probe — `clean` should work on
//!     machines with broken or missing toolchains.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Copy the on-disk minimal fixture into a fresh tempdir and return a
/// handle to the tempdir along with the absolute path of the copied
/// project root (the directory containing the new `gluon.rhai`).
///
/// The returned `TempDir` is kept alive by the caller; dropping it
/// cleans up the entire tree.
fn setup_fixture() -> (TempDir, PathBuf) {
    let src = fixture_source_dir();
    assert!(
        src.join("gluon.rhai").is_file(),
        "fixture missing at {src:?}: check tests/fixtures/minimal/"
    );
    let tmp = tempfile::tempdir().expect("create tempdir");
    let dst = tmp.path().join("minimal");
    copy_dir_all(&src, &dst).expect("copy fixture into tempdir");
    (tmp, dst)
}

/// Resolve the on-disk fixture path: `<workspace>/tests/fixtures/minimal/`.
///
/// `CARGO_MANIFEST_DIR` for this crate is `<workspace>/crates/gluon-cli`,
/// so the fixture lives two directories up.
fn fixture_source_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/gluon-cli has a two-level parent")
        .join("tests")
        .join("fixtures")
        .join("minimal")
}

/// Recursive copy. Deliberately hand-rolled — we don't want to pull in
/// `fs_extra` or similar just for tests.
fn copy_dir_all(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&from, &to)?;
        } else if ty.is_file() {
            fs::copy(&from, &to)?;
        }
        // Symlinks and other special file types aren't used in the
        // fixture, so we silently skip them.
    }
    Ok(())
}

/// Build a `Command` targeting the `gluon` binary produced by cargo for
/// this integration test run, with its working directory set to the
/// fixture copy. Stdout/stderr are piped so tests can inspect them.
fn gluon_command(fixture: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gluon"));
    cmd.current_dir(fixture);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd
}

/// Probe rustc up-front; if it's not available or `rust-src` is missing,
/// print a skip notice and return `None`. Tests use
/// `let Some(()) = require_rustc_or_skip("test_name") else { return; };`
/// to bail out cleanly.
fn require_rustc_or_skip(test_name: &str) -> Option<()> {
    match gluon_core::RustcInfo::probe() {
        Ok(info) if info.rust_src.is_some() => Some(()),
        Ok(_) => {
            eprintln!("{test_name}: skipped — rust-src component not installed");
            None
        }
        Err(e) => {
            eprintln!("{test_name}: skipped — rustc probe failed: {e}");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// A Rhai parse error in `gluon.rhai` must be reported to stderr with the
/// file name somewhere in the message, and the CLI must exit non-zero.
///
/// This test deliberately induces a Rhai *parse* failure — not a semantic
/// one — so `evaluate()` fails before `build_context` ever reaches the
/// rustc probe. That's what lets this single test run in the default
/// (non-`--ignored`) test set without a working toolchain.
#[test]
fn gluon_build_rhai_typo_diagnostic() {
    let (_tmp, fixture) = setup_fixture();

    // Clobber the script with something Rhai's parser will reject.
    // A stray unbalanced `}` at top level is a hard parse error.
    let broken = r#"project("minimal", "0.1.0");
this_is_a_syntax_error }
"#;
    fs::write(fixture.join("gluon.rhai"), broken).expect("rewrite gluon.rhai");

    let output = gluon_command(&fixture)
        .arg("build")
        .output()
        .expect("spawn gluon build");

    assert!(
        !output.status.success(),
        "gluon build should fail on a broken gluon.rhai, got status {:?}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_lowercase().contains("error"),
        "stderr should mention 'error', got: {stderr}"
    );
    // The anyhow context wrapper in build_context prints "failed to
    // evaluate gluon.rhai", which is a reliable substring to assert on
    // because it names the file.
    assert!(
        stderr.contains("gluon.rhai"),
        "stderr should mention gluon.rhai, got: {stderr}"
    );
}

/// Cold build, then a second build that hits the cache.
///
/// Asserts:
///   1. First `gluon build` exits 0 and its "built N, cached M" summary
///      line has `cached 0` (everything was compiled fresh).
///   2. The sysroot stamp exists under `build/sysroot/<target>/stamp`.
///   3. The kernel binary exists under
///      `build/cross/<target>/<profile>/final/kernel`.
///   4. Second `gluon build` exits 0 and its summary has `built 0`
///      (nothing was rebuilt; everything was cached).
#[test]
#[ignore]
fn gluon_build_cold_then_cache_hit() {
    let Some(()) = require_rustc_or_skip("gluon_build_cold_then_cache_hit") else {
        return;
    };
    let (_tmp, fixture) = setup_fixture();

    // --- First run: cold build ---
    let first = gluon_command(&fixture)
        .arg("build")
        .output()
        .expect("spawn first gluon build");
    let first_stderr = String::from_utf8_lossy(&first.stderr);
    assert!(
        first.status.success(),
        "first build failed: status={:?}, stderr=\n{first_stderr}",
        first.status
    );
    assert!(
        first_stderr.contains("built ") && first_stderr.contains("cached 0"),
        "first build summary should be 'built N, cached 0', got: {first_stderr}"
    );

    // Sysroot stamp should exist after a successful cold build.
    let stamp = fixture
        .join("build")
        .join("sysroot")
        .join("x86_64-unknown-none")
        .join("stamp");
    assert!(
        stamp.is_file(),
        "sysroot stamp should exist at {stamp:?} after cold build"
    );

    // Kernel binary should exist at cross/<target>/<profile>/final/kernel.
    let kernel_bin = fixture
        .join("build")
        .join("cross")
        .join("x86_64-unknown-none")
        .join("dev")
        .join("final")
        .join("kernel");
    assert!(
        kernel_bin.is_file(),
        "expected kernel binary at {kernel_bin:?} after cold build, stderr:\n{first_stderr}"
    );

    // --- Second run: everything should be cached ---
    let second = gluon_command(&fixture)
        .arg("build")
        .output()
        .expect("spawn second gluon build");
    let second_stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        second.status.success(),
        "second build failed: status={:?}, stderr=\n{second_stderr}",
        second.status
    );
    assert!(
        second_stderr.contains("built 0"),
        "second build summary should report 'built 0', got: {second_stderr}"
    );
    assert!(
        second_stderr.contains("cached ") && !second_stderr.contains("cached 0"),
        "second build summary should report a non-zero 'cached N', got: {second_stderr}"
    );
}

/// After a cold build, touch `crates/kernel/src/main.rs` and re-run the
/// build. At least one crate should be rebuilt (kernel itself, and
/// possibly others that depend on it — the exact count depends on rebuild
/// semantics, so we only assert `built > 0`).
#[test]
#[ignore]
fn gluon_build_source_touch_rebuilds_one() {
    let Some(()) = require_rustc_or_skip("gluon_build_source_touch_rebuilds_one") else {
        return;
    };
    let (_tmp, fixture) = setup_fixture();

    // Cold build first.
    let first = gluon_command(&fixture)
        .arg("build")
        .output()
        .expect("first build");
    assert!(
        first.status.success(),
        "cold build failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );

    // Modify main.rs in-place. We rewrite rather than just `touch` so
    // the content hash changes regardless of what freshness model the
    // cache uses (content-hash *or* mtime).
    let main_path = fixture.join("crates/kernel/src/main.rs");
    let mut src = fs::read_to_string(&main_path).expect("read main.rs");
    src.push_str("\n// touched by gluon_build_source_touch_rebuilds_one\n");
    fs::write(&main_path, src).expect("rewrite main.rs");

    let second = gluon_command(&fixture)
        .arg("build")
        .output()
        .expect("second build");
    let second_stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        second.status.success(),
        "touched rebuild failed: {second_stderr}"
    );
    // We only assert "not zero" because whether the rebuild count is
    // 1, 2, or more depends on how many dependents the scheduler
    // recompiles for a touched leaf crate.
    assert!(
        !second_stderr.contains("built 0"),
        "touched build should have rebuilt at least one crate, got: {second_stderr}"
    );
}

/// `gluon configure` must write a parseable `rust-project.json` at the
/// default location, containing the config crate plus one entry per
/// resolved crate in the fixture (minimal_derive + kernel = 3 total
/// including the config crate).
#[test]
#[ignore]
fn gluon_configure_emits_valid_json() {
    let Some(()) = require_rustc_or_skip("gluon_configure_emits_valid_json") else {
        return;
    };
    let (_tmp, fixture) = setup_fixture();

    let output = gluon_command(&fixture)
        .arg("configure")
        .output()
        .expect("spawn gluon configure");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "gluon configure failed: status={:?}, stderr={stderr}",
        output.status
    );

    let rp_path = fixture.join("rust-project.json");
    assert!(
        rp_path.is_file(),
        "rust-project.json should exist at {rp_path:?}"
    );
    let body = fs::read_to_string(&rp_path).expect("read rust-project.json");

    // serde_json is intentionally NOT a dev-dep of gluon-cli (scope
    // constraint for this chunk), so we do lightweight substring-level
    // structure checks instead of full deserialization. The file must
    // start with `{`, declare the top-level `crates` key, mention the
    // proc-macro crate type via `is_proc_macro`, and reference both
    // source-tree directories the resolver should have emitted entries
    // for.
    assert!(
        body.trim_start().starts_with('{'),
        "rust-project.json must start with '{{': {body}"
    );
    assert!(
        body.contains("\"crates\""),
        "rust-project.json must declare a 'crates' key: {body}"
    );
    assert!(
        body.contains("\"is_proc_macro\": true") || body.contains("\"is_proc_macro\":true"),
        "rust-project.json must mark the proc-macro crate with is_proc_macro=true: {body}"
    );
    assert!(
        body.contains("crates/kernel"),
        "rust-project.json must reference crates/kernel: {body}"
    );
    assert!(
        body.contains("crates/proc_macro"),
        "rust-project.json must reference crates/proc_macro: {body}"
    );
    // The generated `<project>_config` crate always lives under
    // `<build>/generated/minimal_config/`.
    assert!(
        body.contains("minimal_config"),
        "rust-project.json must reference the generated minimal_config crate: {body}"
    );
}

/// `gluon clean` (default: no `--keep-sysroot`) must remove the entire
/// `build/` directory. We pre-populate it with a sentinel file so we can
/// confirm the sweep actually ran. Runs without rustc because `clean`
/// uses the layout-only context.
#[test]
fn gluon_clean_removes_build_dir() {
    let (_tmp, fixture) = setup_fixture();

    let build_dir = fixture.join("build");
    fs::create_dir_all(&build_dir).expect("mkdir build");
    let sentinel = build_dir.join("sentinel");
    fs::write(&sentinel, b"hello").expect("write sentinel");

    let output = gluon_command(&fixture)
        .arg("clean")
        .output()
        .expect("spawn gluon clean");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "gluon clean failed: status={:?}, stderr={stderr}",
        output.status
    );

    assert!(
        !build_dir.exists(),
        "build/ should have been removed by gluon clean"
    );
}

/// `gluon clean --keep-sysroot` must remove build state but leave the
/// sysroot subtree intact. Runs without rustc because `clean` uses
/// the layout-only context.
#[test]
fn gluon_clean_keep_sysroot_preserves_sysroot() {
    let (_tmp, fixture) = setup_fixture();

    // Fabricate a fake pre-built state: a sysroot stamp plus some other
    // build artifact. The sysroot subtree should survive; the other
    // artifact should be deleted.
    let build_dir = fixture.join("build");
    let sysroot_dir = build_dir.join("sysroot").join("x86_64-unknown-none");
    fs::create_dir_all(&sysroot_dir).expect("mkdir sysroot");
    let stamp = sysroot_dir.join("stamp");
    fs::write(&stamp, b"stamp").expect("write stamp");

    let other = build_dir.join("cross").join("leftover");
    fs::create_dir_all(other.parent().unwrap()).expect("mkdir cross");
    fs::write(&other, b"junk").expect("write junk");

    let output = gluon_command(&fixture)
        .arg("clean")
        .arg("--keep-sysroot")
        .output()
        .expect("spawn gluon clean --keep-sysroot");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "gluon clean --keep-sysroot failed: status={:?}, stderr={stderr}",
        output.status
    );

    assert!(
        stamp.is_file(),
        "sysroot stamp should be preserved by --keep-sysroot"
    );
    assert!(
        !other.exists(),
        "non-sysroot build state should have been removed"
    );
}
