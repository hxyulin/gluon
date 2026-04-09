//! End-to-end CLI integration tests for `gluon run`.
//!
//! These tests drive the real `gluon` binary against the `minimal`
//! and `minimal-uefi` fixtures in `--dry-run` mode. Dry-run skips
//! both the build step and the QEMU spawn, returning only the
//! assembled argv to stdout — so these tests:
//!
//! - do **not** require a working rustc toolchain,
//! - do **not** require QEMU to be installed,
//! - do **not** require a real OVMF firmware (we point at tempfile
//!   stubs via `OVMF_CODE` / `OVMF_VARS`),
//!
//! and therefore run in the default test set without an `#[ignore]`
//! gate. A separate `spawn_real_qemu_smoke` test (gated behind the
//! `GLUON_TEST_QEMU=1` env var *and* `#[ignore]`) exercises the
//! actual spawn path on a dev box.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn workspace_fixture_dir(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/gluon-cli has a two-level parent")
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn setup_fixture(name: &str) -> (TempDir, PathBuf) {
    let src = workspace_fixture_dir(name);
    assert!(
        src.join("gluon.rhai").is_file(),
        "fixture missing at {src:?}"
    );
    let tmp = tempfile::tempdir().expect("create tempdir");
    let dst = tmp.path().join(name);
    copy_dir_all(&src, &dst).expect("copy fixture into tempdir");
    (tmp, dst)
}

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
    }
    Ok(())
}

fn gluon_command(fixture: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gluon"));
    cmd.current_dir(fixture);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    // Scrub inherited OVMF env vars so tests are hermetic by default.
    // Individual tests that need them will re-add them explicitly.
    cmd.env_remove("OVMF_CODE");
    cmd.env_remove("OVMF_VARS");
    cmd
}

/// Parse a `gluon run --dry-run` stdout line into `(binary, args)`.
///
/// The dry-run printer emits a single shell-quoted line:
/// `qemu-system-x86_64 -machine q35 -m 256M ...`. This helper
/// splits on whitespace which is good enough for our tests because
/// all paths in the fixtures are alphanumeric + `-`/`_`/`.`/`/`.
fn parse_dry_run(stdout: &str) -> (String, Vec<String>) {
    let line = stdout
        .lines()
        .find(|l| !l.trim().is_empty())
        .expect("at least one non-empty stdout line");
    let mut parts = line.split_whitespace();
    let binary = parts.next().expect("binary").to_string();
    let args: Vec<String> = parts.map(|s| s.to_string()).collect();
    (binary, args)
}

fn contains_pair(args: &[String], flag: &str, value: &str) -> bool {
    args.windows(2).any(|w| w[0] == flag && w[1] == value)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn gluon_run_direct_dry_run_against_minimal() {
    let (_tmp, fixture) = setup_fixture("minimal");

    let output = gluon_command(&fixture)
        .args(["run", "--dry-run"])
        .output()
        .expect("spawn gluon run");

    assert!(
        output.status.success(),
        "gluon run --dry-run failed: status={:?}, stderr=\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let (binary, args) = parse_dry_run(&stdout);
    assert_eq!(binary, "qemu-system-x86_64");
    assert!(
        contains_pair(&args, "-machine", "q35"),
        "expected -machine q35, got {args:?}"
    );
    assert!(
        contains_pair(&args, "-m", "256M"),
        "expected -m 256M, got {args:?}"
    );
    assert!(
        contains_pair(&args, "-smp", "1"),
        "expected -smp 1, got {args:?}"
    );
    assert!(
        args.iter().any(|a| a == "-kernel"),
        "expected direct-mode -kernel flag, got {args:?}"
    );
    // Direct mode must NOT emit any pflash drives.
    assert!(
        !args.iter().any(|a| a.contains("pflash")),
        "direct mode must not emit pflash drives, got {args:?}"
    );
    // The boot binary path must end in cross/.../dev/final/kernel.
    let kernel_idx = args.iter().position(|a| a == "-kernel").unwrap();
    let kernel_path = &args[kernel_idx + 1];
    assert!(
        kernel_path.ends_with("cross/x86_64-unknown-none/dev/final/kernel"),
        "kernel path looks wrong: {kernel_path}"
    );
}

#[test]
fn gluon_run_uefi_dry_run_against_minimal_uefi() {
    let (_tmp, fixture) = setup_fixture("minimal-uefi");

    // Materialise fake OVMF files inside the tempdir so the resolver
    // can validate their existence. We only care that the argv points
    // at them; we never actually read their bytes.
    let code = fixture.join("OVMF_CODE.fd");
    let vars = fixture.join("OVMF_VARS.fd");
    fs::write(&code, b"fake code").unwrap();
    fs::write(&vars, b"fake vars").unwrap();

    let output = gluon_command(&fixture)
        .env("OVMF_CODE", &code)
        .env("OVMF_VARS", &vars)
        .args(["run", "--dry-run"])
        .output()
        .expect("spawn gluon run --uefi-from-profile");

    assert!(
        output.status.success(),
        "gluon run --dry-run (uefi) failed: status={:?}, stderr=\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let (_, args) = parse_dry_run(&stdout);

    // Two pflash drives, readonly code + writable vars.
    let pflash: Vec<_> = args
        .windows(2)
        .filter(|w| w[0] == "-drive" && w[1].contains("if=pflash"))
        .map(|w| w[1].clone())
        .collect();
    assert_eq!(pflash.len(), 2, "expected 2 pflash drives, got {pflash:?}");
    assert!(
        pflash.iter().any(|d| d.contains("readonly=on")),
        "one pflash drive must be readonly, got {pflash:?}"
    );
    assert!(
        pflash
            .iter()
            .any(|d| !d.contains("readonly=on") && d.contains("format=raw")),
        "one pflash drive must be writable, got {pflash:?}"
    );

    // ESP dir mounted via VVFAT.
    assert!(
        args.iter().any(|a| a.contains("fat:rw:")),
        "expected fat:rw: ESP drive, got {args:?}"
    );

    // No direct -kernel flag in UEFI mode.
    assert!(
        !args.iter().any(|a| a == "-kernel"),
        "UEFI mode must not emit -kernel, got {args:?}"
    );

    // Profile-level memory (512) overrode the default.
    assert!(
        contains_pair(&args, "-m", "512M"),
        "expected profile's -m 512M, got {args:?}"
    );
    assert!(
        contains_pair(&args, "-smp", "2"),
        "expected profile's -smp 2, got {args:?}"
    );
}

#[test]
fn gluon_run_cli_uefi_flag_overrides_direct_profile() {
    // Start from the `minimal` fixture (direct mode) and force UEFI
    // via the CLI flag, supplying OVMF via env vars. Should produce
    // a UEFI argv even though the fixture's qemu() block never
    // called .boot_mode("uefi").
    let (_tmp, fixture) = setup_fixture("minimal");
    let code = fixture.join("OVMF_CODE.fd");
    let vars = fixture.join("OVMF_VARS.fd");
    fs::write(&code, b"fake code").unwrap();
    fs::write(&vars, b"fake vars").unwrap();

    let output = gluon_command(&fixture)
        .env("OVMF_CODE", &code)
        .env("OVMF_VARS", &vars)
        .args(["run", "--uefi", "--dry-run"])
        .output()
        .expect("spawn gluon run --uefi");

    assert!(
        output.status.success(),
        "gluon run --uefi --dry-run failed: status={:?}, stderr=\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let (_, args) = parse_dry_run(&stdout);
    assert!(
        args.iter().any(|a| a.contains("if=pflash")),
        "expected pflash drives after --uefi, got {args:?}"
    );
    assert!(
        !args.iter().any(|a| a == "-kernel"),
        "CLI --uefi should suppress -kernel, got {args:?}"
    );
}

#[test]
fn gluon_run_uefi_without_ovmf_errors_loudly() {
    // `minimal-uefi` declares boot_mode uefi but its qemu() block
    // does not set explicit ovmf paths. With env vars scrubbed
    // (gluon_command already does this) and no system OVMF, the
    // resolver should fail with the three-layer diagnostic.
    //
    // On a developer box with system OVMF installed, the resolver
    // might actually succeed — in that case we expect the argv path,
    // not the error. We detect that case and skip the error-path
    // assertion.

    let (_tmp, fixture) = setup_fixture("minimal-uefi");

    let output = gluon_command(&fixture)
        .args(["run", "--dry-run"])
        .output()
        .expect("spawn gluon run");

    if output.status.success() {
        // System OVMF was found — confirm the argv is a UEFI one and move on.
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("if=pflash"),
            "system OVMF was found but argv is not UEFI: {stdout}"
        );
        return;
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no OVMF firmware found"),
        "stderr should describe the OVMF resolver failure, got: {stderr}"
    );
    // All three fallback layers should be mentioned in the diagnostic.
    assert!(
        stderr.contains("Explicit") && stderr.contains("Env") && stderr.contains("System paths"),
        "resolver diagnostic should list all three fallback layers, got: {stderr}"
    );
}

#[test]
fn gluon_run_gdb_appends_dash_s_upper_s() {
    // `--gdb` must inject `-s -S` into the argv. We verify via the
    // dry-run path so the test is hermetic (no QEMU spawn). The two
    // flags should appear *after* the managed gluon section (machine,
    // memory, kernel) but before any user-provided `-- extras`.
    let (_tmp, fixture) = setup_fixture("minimal");

    let output = gluon_command(&fixture)
        .args(["run", "--gdb", "--dry-run"])
        .output()
        .expect("spawn gluon run --gdb --dry-run");

    assert!(
        output.status.success(),
        "gluon run --gdb --dry-run failed: stderr=\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let (_, args) = parse_dry_run(&stdout);

    assert!(
        args.iter().any(|a| a == "-s"),
        "expected -s in argv, got {args:?}"
    );
    assert!(
        args.iter().any(|a| a == "-S"),
        "expected -S in argv, got {args:?}"
    );
    // -s must come after -kernel (the managed section); this protects
    // the invariant that user-visible gluon flags don't break the
    // "managed first, user overrides second" layout.
    let pos_kernel = args.iter().position(|a| a == "-kernel").unwrap();
    let pos_s = args.iter().position(|a| a == "-s").unwrap();
    assert!(pos_kernel < pos_s, "-s should follow -kernel, got {args:?}");
}

#[test]
fn gluon_run_extra_args_pass_through() {
    let (_tmp, fixture) = setup_fixture("minimal");

    let output = gluon_command(&fixture)
        .args(["run", "--dry-run", "--", "-no-reboot", "-d", "int"])
        .output()
        .expect("spawn gluon run -- ...");

    assert!(
        output.status.success(),
        "gluon run --dry-run -- ... failed: status={:?}, stderr=\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let (_, args) = parse_dry_run(&stdout);
    // Extra args appear after the managed gluon section.
    let pos_no_reboot = args.iter().position(|a| a == "-no-reboot").unwrap();
    let pos_d = args.iter().position(|a| a == "-d").unwrap();
    let pos_kernel = args.iter().position(|a| a == "-kernel").unwrap();
    assert!(pos_kernel < pos_no_reboot && pos_no_reboot < pos_d);
    assert!(args.iter().any(|a| a == "int"));
}

// ---------------------------------------------------------------------------
// --no-build path
// ---------------------------------------------------------------------------

/// Rewrite the fixture's `qemu("...")` binary to point at a different
/// path. Used by tests that want to spawn the runner against a fake
/// QEMU binary instead of the real one.
///
/// Relies on the literal `"qemu-system-x86_64"` appearing in the
/// fixture's `gluon.rhai`. If the fixture is refactored to use a
/// different name, this helper will silently no-op and the calling
/// test will fail loudly when its spawn returns the wrong exit
/// status — which is the right failure mode (better than a quiet
/// drift).
fn rewrite_qemu_binary(fixture: &Path, new_binary: &str) {
    let path = fixture.join("gluon.rhai");
    let original = fs::read_to_string(&path).expect("read gluon.rhai");
    let rewritten = original.replace("qemu-system-x86_64", new_binary);
    assert_ne!(
        original, rewritten,
        "expected fixture to contain 'qemu-system-x86_64' literal so the \
         test can rewrite it; check tests/fixtures/minimal/gluon.rhai"
    );
    fs::write(&path, rewritten).expect("write gluon.rhai");
}

#[test]
fn gluon_run_no_build_skips_build_and_spawns() {
    // Two assertions, both load-bearing:
    //   1. fake-qemu actually got spawned (exit 0 from the runner).
    //   2. the build step was actually skipped — verified by the
    //      absence of any compiled artifact under build/cross/, which
    //      would otherwise have been produced by build_with_workers.
    //
    // The second assertion is what distinguishes "the flag is wired
    // through" from "the flag is wired but the build runs anyway".
    let (_tmp, fixture) = setup_fixture("minimal");
    let fake_qemu = env!("CARGO_BIN_EXE_fake-qemu");
    rewrite_qemu_binary(&fixture, fake_qemu);

    let output = gluon_command(&fixture)
        .args(["run", "--no-build"])
        .output()
        .expect("spawn gluon run --no-build");

    assert!(
        output.status.success(),
        "gluon run --no-build failed: status={:?}, stderr=\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    // The build step would have populated build/cross/.../final/
    // with the kernel binary. With --no-build, that directory must
    // not exist. (We tolerate the build/ root itself existing in
    // case some other layer touches it; what we care about is that
    // no compilation happened.)
    let cross_dir = fixture.join("build").join("cross");
    assert!(
        !cross_dir.exists(),
        "build/cross/ should not exist after --no-build, but found {cross_dir:?}"
    );
}

#[test]
fn gluon_run_no_build_passes_kernel_path_to_qemu() {
    // Belt-and-braces: also assert the fake-qemu binary saw a
    // -kernel argument referencing the would-be boot binary path.
    // This catches the regression where --no-build skips the build
    // *and* skips populating the QEMU argv.
    let (_tmp, fixture) = setup_fixture("minimal");
    let fake_qemu = env!("CARGO_BIN_EXE_fake-qemu");
    rewrite_qemu_binary(&fixture, fake_qemu);

    let argv_file = fixture.join("fake-qemu-argv.txt");

    let output = gluon_command(&fixture)
        .env("FAKE_QEMU_ARGV_FILE", &argv_file)
        .args(["run", "--no-build"])
        .output()
        .expect("spawn gluon run --no-build");

    assert!(
        output.status.success(),
        "gluon run --no-build failed: status={:?}, stderr=\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let argv = fs::read_to_string(&argv_file).expect("fake-qemu wrote argv");
    let lines: Vec<&str> = argv.lines().collect();
    let kernel_idx = lines
        .iter()
        .position(|l| *l == "-kernel")
        .unwrap_or_else(|| panic!("expected -kernel in fake-qemu argv:\n{argv}"));
    let kernel_path = lines
        .get(kernel_idx + 1)
        .expect("kernel path follows -kernel");
    assert!(
        kernel_path.ends_with("cross/x86_64-unknown-none/dev/final/kernel"),
        "kernel path looks wrong: {kernel_path}"
    );
}

// --- Real QEMU smoke test (opt-in) ---

/// Actually spawn QEMU against a direct-mode kernel ELF. Gated behind
/// `GLUON_TEST_QEMU=1` *and* `#[ignore]` because it needs both `rustc`
/// (to build the kernel) and `qemu-system-x86_64` installed.
#[test]
#[ignore]
fn spawn_real_qemu_smoke() {
    if std::env::var("GLUON_TEST_QEMU").ok().as_deref() != Some("1") {
        eprintln!("spawn_real_qemu_smoke: skipped — set GLUON_TEST_QEMU=1 to enable");
        return;
    }
    if gluon_core::RustcInfo::probe().is_err() {
        eprintln!("spawn_real_qemu_smoke: skipped — rustc probe failed");
        return;
    }
    let (_tmp, fixture) = setup_fixture("minimal");

    let output = gluon_command(&fixture)
        .args(["run", "-T", "5"])
        .output()
        .expect("spawn gluon run");

    // The stub kernel loops forever, so the timeout must fire.
    // gluon reports QemuTimeout as a non-zero exit.
    assert!(!output.status.success(), "expected non-zero from timeout");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_lowercase().contains("timeout"),
        "expected timeout error in stderr, got: {stderr}"
    );
}
