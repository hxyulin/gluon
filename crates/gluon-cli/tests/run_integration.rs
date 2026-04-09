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

// ---------------------------------------------------------------------------
// uefi-bootloader-kernel fixture — Step 6 of the UEFI end-to-end work.
// Exercises the artifact_env + auto-ESP + QEMU auto-wire chain against
// a two-target project (x86_64-unknown-uefi bootloader embedding an
// x86_64-unknown-none kernel).
// ---------------------------------------------------------------------------

/// Probe rustc up-front; skip if unavailable or missing `rust-src`.
/// The build-path tests use this because they need a real sysroot for
/// both x86_64-unknown-none and x86_64-unknown-uefi.
fn require_rustc_or_skip(test_name: &str) -> Option<()> {
    match gluon_core::RustcInfo::probe() {
        Ok(info) if info.rust_src.is_some() => Some(()),
        Ok(_) => {
            eprintln!(
                "{test_name}: skipped — rustc found but `rust-src` component missing \
                 (install with: rustup component add rust-src)"
            );
            None
        }
        Err(e) => {
            eprintln!("{test_name}: skipped — rustc probe failed: {e}");
            None
        }
    }
}

#[test]
fn gluon_run_uefi_bootloader_kernel_dry_run_emits_auto_esp() {
    // With no explicit `qemu().esp_dir(...)` and exactly one
    // `esp("default")` declaration, dry-run must emit the path-arithmetic
    // ESP directory (build/cross/x86_64-unknown-uefi/dev/esp/default)
    // as the fat:rw: drive target.
    let (_tmp, fixture) = setup_fixture("uefi-bootloader-kernel");
    let code = fixture.join("OVMF_CODE.fd");
    let vars = fixture.join("OVMF_VARS.fd");
    fs::write(&code, b"fake code").unwrap();
    fs::write(&vars, b"fake vars").unwrap();

    let output = gluon_command(&fixture)
        .env("OVMF_CODE", &code)
        .env("OVMF_VARS", &vars)
        .args(["run", "--dry-run"])
        .output()
        .expect("spawn gluon run --dry-run");

    assert!(
        output.status.success(),
        "gluon run --dry-run failed: status={:?}, stderr=\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let (_, args) = parse_dry_run(&stdout);

    // Pflash drives present: UEFI mode is wired.
    let pflash_count = args
        .windows(2)
        .filter(|w| w[0] == "-drive" && w[1].contains("if=pflash"))
        .count();
    assert_eq!(
        pflash_count, 2,
        "expected 2 pflash drives in UEFI mode, got {args:?}"
    );

    // Auto-wired ESP: -drive format=raw,file=fat:rw:<esp_dir>
    let fat_drives: Vec<_> = args
        .windows(2)
        .filter(|w| w[0] == "-drive" && w[1].contains("fat:rw:"))
        .map(|w| w[1].clone())
        .collect();
    assert_eq!(
        fat_drives.len(),
        1,
        "expected exactly one fat:rw: drive (auto-wired ESP), got {args:?}"
    );

    // The path must resolve to the build-output ESP directory under
    // the profile's cross target. We don't require the directory to
    // exist — dry-run skips the build, so it won't.
    let fat_drive = &fat_drives[0];
    let expected_suffix = "build/cross/x86_64-unknown-uefi/dev/esp/default";
    assert!(
        fat_drive.contains(expected_suffix),
        "auto-wired ESP path should end in '{expected_suffix}', got: {fat_drive}"
    );

    // No direct -kernel: UEFI mode routes through OVMF, not QEMU's
    // direct loader.
    assert!(
        !args.iter().any(|a| a == "-kernel"),
        "UEFI mode must not emit -kernel, got {args:?}"
    );
}

#[test]
#[ignore]
fn gluon_build_uefi_bootloader_kernel_assembles_esp() {
    // Full build path: compile both the kernel (x86_64-unknown-none)
    // and the bootloader (x86_64-unknown-uefi), run the EspBuild node,
    // and verify the on-disk ESP layout.
    //
    // Gated by #[ignore] + real-rustc probe because it needs
    // `rust-src` for both targets. Run with:
    //   cargo test -p gluon-cli --test run_integration -- --ignored \
    //     gluon_build_uefi_bootloader_kernel_assembles_esp
    let Some(()) = require_rustc_or_skip("gluon_build_uefi_bootloader_kernel_assembles_esp") else {
        return;
    };

    let (_tmp, fixture) = setup_fixture("uefi-bootloader-kernel");

    let output = gluon_command(&fixture)
        .args(["build"])
        .output()
        .expect("spawn gluon build");

    assert!(
        output.status.success(),
        "gluon build failed: status={:?}, stderr=\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let esp_dir = fixture
        .join("build")
        .join("cross")
        .join("x86_64-unknown-uefi")
        .join("dev")
        .join("esp")
        .join("default");
    assert!(
        esp_dir.is_dir(),
        "ESP directory should exist at {esp_dir:?}"
    );

    let boot_x64 = esp_dir.join("EFI").join("BOOT").join("BOOTX64.EFI");
    assert!(
        boot_x64.is_file(),
        "bootloader should have been copied to {boot_x64:?}"
    );
    let boot_bytes = fs::read(&boot_x64).expect("read BOOTX64.EFI");
    assert!(
        !boot_bytes.is_empty(),
        "BOOTX64.EFI must not be empty"
    );
    // PE32+ header magic: MZ at offset 0.
    assert_eq!(
        &boot_bytes[..2],
        b"MZ",
        "BOOTX64.EFI should be a PE binary (starts with MZ), got first 2 bytes: {:?}",
        &boot_bytes[..2]
    );

    // The kernel ELF should NOT be in the ESP (it's embedded in the
    // bootloader via include_bytes!, not shipped as a separate file).
    let esp_kernel_elf = esp_dir.join("kernel");
    assert!(
        !esp_kernel_elf.exists(),
        "kernel must not appear in ESP — the bootloader embeds it"
    );

    // Sanity: the standalone kernel ELF should exist at its own path
    // (proves the cross-group build actually ran both targets).
    let kernel_elf = fixture
        .join("build")
        .join("cross")
        .join("x86_64-unknown-none")
        .join("dev")
        .join("final")
        .join("kernel");
    assert!(
        kernel_elf.is_file(),
        "kernel ELF should exist at {kernel_elf:?}"
    );
}

#[test]
#[ignore]
fn gluon_run_uefi_bootloader_kernel_real_qemu_debugcon() {
    // Full spawn path: builds + boots the bootloader under QEMU/OVMF
    // and asserts on debugcon output. Gated by GLUON_TEST_QEMU=1
    // because it needs both a real rustc toolchain AND a working
    // qemu-system-x86_64 binary with OVMF firmware discoverable.
    if std::env::var("GLUON_TEST_QEMU").ok().as_deref() != Some("1") {
        eprintln!(
            "gluon_run_uefi_bootloader_kernel_real_qemu_debugcon: \
             skipped — set GLUON_TEST_QEMU=1 to enable"
        );
        return;
    }
    let Some(()) =
        require_rustc_or_skip("gluon_run_uefi_bootloader_kernel_real_qemu_debugcon")
    else {
        return;
    };

    let (_tmp, fixture) = setup_fixture("uefi-bootloader-kernel");
    let debugcon_path = fixture.join("debugcon.out");
    // Best-effort: create an empty file the bootloader can append to.
    fs::write(&debugcon_path, b"").ok();

    let debugcon_arg = format!("file:{}", debugcon_path.display());

    let output = gluon_command(&fixture)
        .args([
            "run",
            "-T",
            "10",
            "--",
            "-debugcon",
            &debugcon_arg,
            "-display",
            "none",
            "-no-reboot",
        ])
        .output()
        .expect("spawn gluon run");

    // The bootloader halts in an hlt loop, so we expect the timeout
    // to fire. Both a clean exit (unlikely) and a timeout-induced
    // non-zero status are acceptable; what matters is the captured
    // debugcon output.
    let _ = output;

    let captured = fs::read(&debugcon_path).unwrap_or_default();
    assert!(
        captured.starts_with(b"K"),
        "debugcon must start with 'K' (kernel non-empty marker), got: {:?}",
        String::from_utf8_lossy(&captured)
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

    // The stub kernel loops forever, so the timeout should fire and
    // gluon reports QemuTimeout as a non-zero exit. However, newer QEMU
    // versions (≥ 7.2) reject bare ELF kernels without a PVH ELF note
    // in direct-boot mode, exiting immediately with status 1 and an
    // "Error loading uncompressed kernel" message. Both outcomes are
    // valid — the test's purpose is to verify that gluon's runner
    // correctly spawns QEMU and reports the result, not that the
    // minimal fixture's kernel actually boots.
    assert!(!output.status.success(), "expected non-zero exit");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let is_timeout = stderr.to_lowercase().contains("timeout");
    let is_qemu_load_error = stderr.contains("QEMU exited with status");
    assert!(
        is_timeout || is_qemu_load_error,
        "expected either a timeout or a QEMU load error in stderr, got: {stderr}"
    );
}
