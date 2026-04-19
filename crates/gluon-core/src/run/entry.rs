//! `gluon run` top-level entry.
//!
//! Wires together `build()`, resolve, OVMF resolution (when needed),
//! argv assembly, and QEMU spawn.
//!
//! Public surface: [`run`] plus the [`RunOptions`] struct the CLI
//! populates from its clap-parsed args. Keeping everything else in
//! sibling submodules means the whole pipeline stays unit-testable in
//! isolation: `qemu_cmd` is pure, `resolve` is pure, `ovmf` only
//! touches the filesystem behind an injected context, and this file
//! is the one spot where subprocesses get spawned.

use super::ovmf::{OvmfResolveCtx, resolve_ovmf};
use super::qemu_cmd::{QemuInvocation, build_qemu_command};
use super::resolve::resolve_qemu;
use crate::compile::CompileCtx;
use crate::error::{Error, Result};
use crate::{build_with_workers, model::BootMode};
use gluon_model::{BuildModel, EspSource, ResolvedConfig};
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::AtomicI32;
use std::time::{Duration, Instant};

/// Knobs passed into [`run`] by the CLI layer.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// Forces `BootMode::Uefi` / `BootMode::Direct`, overriding whatever
    /// the profile's `qemu().boot_mode(...)` declared. `None` means
    /// "use the profile-level setting, or direct".
    pub boot_mode_override: Option<BootMode>,
    /// Forces a wall-clock timeout, overriding any profile-level
    /// `test_timeout`. `None` means "inherit profile, or no timeout".
    pub timeout_override: Option<Duration>,
    /// CLI pass-through arguments (`gluon run -- -device ...`).
    pub extra_args: Vec<OsString>,
    /// Worker count for the preceding `build()` step. `None` = auto.
    pub workers: Option<usize>,
    /// Print the assembled QEMU command and exit 0 without spawning.
    /// Used by integration tests and for users sanity-checking a
    /// config.
    pub dry_run: bool,
    /// Skip the implicit `build()` step and go straight to the QEMU
    /// spawn. The user is asserting the boot binary on disk is
    /// already current; if it isn't, QEMU will report a clearer
    /// "could not load kernel" error than gluon could fabricate from
    /// the outside. Intended for tight edit/run loops where rerunning
    /// gluon's fingerprint sweep is pure latency.
    ///
    /// Implied by `dry_run` (dry-run already skips the build).
    pub no_build: bool,
    /// Start QEMU with a GDB server on :1234 and halt the CPU before
    /// the first instruction (`-s -S`). A hint line is printed to
    /// stderr before spawn so the user knows where to point their
    /// debugger.
    pub gdb: bool,
    /// Emit an `-device isa-debug-exit,iobase=<port>,iosize=0x04`
    /// entry in the QEMU argv so the future `gluon test` harness can
    /// read an exit code back from the guest via an I/O port write.
    /// The port comes from [`super::ResolvedQemu::test_exit_port`].
    ///
    /// Not wired to a CLI flag today; the future `gluon test`
    /// subcommand will flip this on before invoking the runner.
    pub test_mode: bool,
}

/// Run the project defined by `model`/`resolved` under QEMU.
///
/// Steps, in order:
///
/// 1. **Build.** Invokes [`build_with_workers`] so `gluon run` is
///    idempotent with the current project state. A failed build
///    short-circuits without touching QEMU.
/// 2. **Locate boot binary.** The resolved profile must have a
///    `boot_binary` set; otherwise [`Error::NoBootBinary`] is
///    returned. The binary path is
///    `BuildLayout::cross_final_dir(target, profile).join(crate_name)`.
/// 3. **Resolve QEMU.** [`resolve_qemu`] merges profile overrides and
///    fills defaults.
/// 4. **UEFI: resolve OVMF.** Only when `boot_mode == Uefi`. The
///    writable-vars copy (when needed) lands under
///    `build/ovmf_vars-<profile>.fd`.
/// 5. **Build argv.** Pure [`build_qemu_command`] call.
/// 6. **Dry-run short-circuit** or **spawn + wait with timeout**.
pub fn run(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    opts: RunOptions,
) -> Result<ExitStatus> {
    // Step 1: build (skipped in dry-run or when --no-build is set).
    //
    // Dry-run exists to let users sanity-check their QEMU
    // configuration without touching rustc, the cache, or the
    // filesystem. The assembled argv is entirely derivable from the
    // resolved model and the `BuildLayout::cross_final_dir` helper,
    // so we can skip the build wholesale and still produce the
    // correct output. Integration tests rely on this.
    //
    // `--no-build` is the same skip without the spawn skip: the user
    // is telling us "I just rebuilt by hand, don't waste my time
    // re-checking timestamps". If the boot binary actually is stale
    // or missing, QEMU will fail at load time with a clearer
    // diagnostic than gluon would produce by trying to second-guess.
    // Captured if we actually built — carries the ESP output paths
    // used by Step 4 to auto-wire QEMU's `-drive fat:rw:` flag.
    let build_summary = if !opts.dry_run && !opts.no_build {
        let workers = opts.workers.unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        });
        Some(build_with_workers(ctx, model, resolved, workers)?)
    } else {
        None
    };

    // Step 2: locate boot binary.
    let profile = &resolved.profile;
    let boot_handle = profile.boot_binary.ok_or_else(|| Error::NoBootBinary {
        profile: profile.name.clone(),
    })?;
    let krate = model.crates.get(boot_handle).ok_or_else(|| {
        Error::Config(format!(
            "internal: boot_binary handle for profile '{}' does not resolve in the model",
            profile.name
        ))
    })?;
    let target = model.targets.get(profile.target).ok_or_else(|| {
        Error::Config(format!(
            "internal: profile '{}' target handle does not resolve",
            profile.name
        ))
    })?;
    let suffix = crate::compile::compile_utils::exe_suffix_for_target(&target.spec);
    let kernel: PathBuf = ctx.layout.cross_final_dir(target, profile).join(format!(
        "{}{suffix}",
        crate::compile::compile_utils::normalize_crate_name(&krate.name)
    ));

    // Step 3: resolve QEMU config.
    //
    // We pass the target's spec as the triple because for builtin
    // targets it is the triple (`x86_64-unknown-none`), and for custom
    // JSON specs it's the spec path — which users typically name with
    // the arch prefix, so the arch-prefix match in
    // `default_binary_for_target` still has a reasonable shot.
    let mut resolved_qemu = resolve_qemu(
        &model.qemu,
        profile,
        &target.spec,
        opts.boot_mode_override,
        opts.timeout_override,
    )?;

    // Step 3b: auto-wire the ESP directory for UEFI boot.
    //
    // If the final boot mode is UEFI, the user has NOT set an explicit
    // `qemu().esp_dir()` / `esp_image()`, and the project declares
    // exactly one `esp(...)` block, then point QEMU at the build-output
    // ESP directory we just assembled (or, in dry-run / --no-build
    // mode, at the path we *would* have assembled via pure path
    // arithmetic). More than one declared ESP is an error: the user
    // must pick explicitly because we can't guess. Zero declarations
    // falls through — QEMU will simply have no ESP drive, same as
    // before this feature existed.
    if resolved_qemu.boot_mode == BootMode::Uefi && resolved_qemu.esp.is_none() {
        match model.esps.len() {
            0 => { /* no automation requested */ }
            1 => {
                // Prefer the path the build actually produced, so a
                // test-only run (without build) never disagrees with a
                // real build. Fall back to path arithmetic when we
                // didn't build (dry-run / --no-build).
                let esp_handle = model
                    .esps
                    .iter()
                    .next()
                    .map(|(h, _)| h)
                    .expect("model.esps.len() == 1");
                let esp_def = model.esps.get(esp_handle).ok_or_else(|| {
                    Error::Compile("internal: esp handle iterates but does not resolve".to_string())
                })?;
                let esp_path = build_summary
                    .as_ref()
                    .and_then(|s| s.esp_dirs.get(&esp_handle).cloned())
                    .unwrap_or_else(|| ctx.layout.esp_dir(target, profile, &esp_def.name));
                resolved_qemu.esp = Some(EspSource::Dir(esp_path));
            }
            n => {
                return Err(Error::Compile(format!(
                    "profile '{}' uses boot_mode('uefi') and the project declares {} esp(...) blocks; \
                     gluon can't guess which one to mount. Set qemu().esp_dir(...) explicitly, \
                     or remove the extra esp declarations.",
                    profile.name, n
                )));
            }
        }
    }

    // Step 4: OVMF (only when the final boot mode is UEFI).
    let ovmf = if resolved_qemu.boot_mode == BootMode::Uefi {
        let ovmf_ctx = OvmfResolveCtx::from_env(ctx.layout.root(), &profile.name);
        Some(resolve_ovmf(&model.qemu, &ovmf_ctx)?)
    } else {
        None
    };

    // Step 5: assemble argv.
    //
    // `--gdb` is surfaced here by prepending `-s -S` to the CLI
    // pass-through list. This keeps `build_qemu_command` pure — it
    // doesn't need to know anything about gdb, just about argv
    // ordering — and it places the gdb flags *after* the user's
    // Rhai-level `extra_args`, so a user who wants to customize
    // the gdb port (`-gdb tcp::9999`) can do it via their config
    // without being clobbered.
    let extra_args_with_gdb: Vec<OsString> = if opts.gdb {
        let mut v = Vec::with_capacity(opts.extra_args.len() + 2);
        v.push(OsString::from("-s"));
        v.push(OsString::from("-S"));
        v.extend(opts.extra_args.iter().cloned());
        v
    } else {
        opts.extra_args.clone()
    };
    let invocation = build_qemu_command(
        &resolved_qemu,
        &kernel,
        resolved_qemu.boot_mode,
        ovmf.as_ref(),
        &extra_args_with_gdb,
        opts.test_mode,
        // Skip ESP existence checks in dry-run / --no-build. In dry-run
        // we haven't built anything, so the auto-wired ESP path does
        // not exist on disk; in --no-build the user is asserting the
        // build tree is already current, and QEMU will report a better
        // error than we can if they're wrong.
        opts.dry_run || opts.no_build,
    )?;

    // Step 6: dry-run or spawn.
    if opts.dry_run {
        print_invocation(&invocation);
        // Return a synthetic success. We don't spawn QEMU in dry-run,
        // and the CLI only cares about the error path (dry-run
        // failures become Result::Err on the way up).
        return Ok(synthetic_success());
    }

    if opts.gdb {
        // Printed *before* the spawn so the user sees it even if QEMU
        // blocks waiting for a gdb connection. Stderr (not stdout)
        // because QEMU's stdio is on stdout/stderr already — we'd
        // rather not interleave with its serial output.
        let mut err = std::io::stderr().lock();
        let _ = writeln!(
            err,
            "gluon: QEMU halted for GDB on :1234 — connect with `target remote :1234`"
        );
    }

    spawn_and_wait(invocation)
}

/// Print the command in a copy-pasteable form. Used by `--dry-run`.
fn print_invocation(inv: &QemuInvocation) {
    let mut line = String::new();
    line.push_str(&shell_quote(&inv.binary));
    for a in &inv.args {
        line.push(' ');
        line.push_str(&shell_quote(&a.to_string_lossy()));
    }
    // Write through stdout explicitly so the ordering is predictable
    // relative to other print! calls in the process.
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
}

/// Minimal POSIX-shell quoting suitable for human consumption. We
/// wrap anything with whitespace or shell metacharacters in single
/// quotes; everything else is printed verbatim.
fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    if s.chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '=' | ','))
    {
        return s.to_string();
    }
    let mut out = String::from("'");
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Spawn the QEMU binary and wait for it, enforcing `invocation.timeout`
/// and a SIGINT/SIGTERM trap that forwards the signal to the child.
///
/// Stdio is inherited so the user sees serial output live. If the
/// caller set `SerialMode::File`, QEMU is the one writing the file;
/// we still inherit our stdio so diagnostic output ("QEMU:
/// warning:...") reaches the terminal.
///
/// On Unix, we register SIGINT + SIGTERM handlers via `signal-hook`
/// that flip an `AtomicBool`. The try_wait loop polls that flag and,
/// when set, kills the child cleanly, reaps it, and returns
/// [`Error::KilledBySignal`]. This prevents the common failure mode
/// where the user hits Ctrl-C and the shell's SIGINT gets delivered
/// to both gluon and QEMU — gluon returning while QEMU keeps running
/// as an orphan with a dangling stdio inheritance. We'd rather eat
/// the signal, tear the child down, and tell the user what happened.
///
/// On Windows the signal handler is a no-op; the Windows console
/// uses `SetConsoleCtrlHandler` which is a different mechanism and
/// the runner is Unix-centric today. Wiring it up can come later if
/// someone needs it.
fn spawn_and_wait(invocation: QemuInvocation) -> Result<ExitStatus> {
    let mut cmd = Command::new(&invocation.binary);
    cmd.args(&invocation.args);
    cmd.stdin(Stdio::inherit());
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::inherit());

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::QemuBinaryNotFound {
                binary: invocation.binary.clone(),
            }
        } else {
            Error::QemuSpawnFailed {
                binary: invocation.binary.clone(),
                source: e,
            }
        }
    })?;

    // Install signal trap only after we have a live child — before
    // this point there's nothing to tear down if a signal arrives,
    // and the default signal disposition (exit) is fine.
    let signal_flag = install_signal_trap();

    let deadline = invocation.timeout.map(|d| Instant::now() + d);
    loop {
        match child.try_wait().map_err(|e| Error::QemuSpawnFailed {
            binary: invocation.binary.clone(),
            source: e,
        })? {
            Some(status) => return Ok(status),
            None => {
                // Signal check first: a user hitting Ctrl-C expects
                // immediate teardown, even if we're a few polls away
                // from the timeout.
                if let Some(sig) = check_signal(&signal_flag) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Error::KilledBySignal { signal: sig });
                }
                if let Some(dl) = deadline {
                    if Instant::now() >= dl {
                        // Kill + reap. Ignore errors: the child may
                        // have exited on its own between the try_wait
                        // and the kill, which is harmless.
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(Error::QemuTimeout);
                    }
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Install a SIGINT + SIGTERM handler that stores the received signal
/// number in a shared `AtomicI32`. Returns the shared flag — zero means
/// "no signal yet", any other value is the received signal number.
///
/// On non-Unix this is a no-op: the returned flag will always read 0,
/// which means the runner falls back to its previous behaviour
/// (timeout-only teardown).
#[cfg(unix)]
fn install_signal_trap() -> Arc<AtomicI32> {
    let flag = Arc::new(AtomicI32::new(0));
    // signal-hook's `low_level::register` installs a handler that
    // runs async-signal-safe code (atomic store) — no allocation, no
    // Rust panic unwinding through the signal frame. Ignore
    // registration errors: if we can't install the handler (for
    // example because one's already installed by the host program)
    // we fall back to the default signal disposition, which is
    // "exit the process" — no worse than the pre-signal-hook
    // behaviour.
    //
    // SAFETY: signal-hook's `low_level::register` is `unsafe` because
    // the closure runs in a signal context; we use only atomic stores
    // (async-signal-safe) inside it and capture by `move`, satisfying
    // the API's contract.
    for &sig in &[signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        let flag_clone = flag.clone();
        #[allow(unsafe_code)]
        let _ = unsafe {
            signal_hook::low_level::register(sig, move || {
                flag_clone.store(sig, std::sync::atomic::Ordering::SeqCst);
            })
        };
    }
    flag
}

#[cfg(not(unix))]
fn install_signal_trap() -> Arc<AtomicI32> {
    Arc::new(AtomicI32::new(0))
}

fn check_signal(flag: &Arc<AtomicI32>) -> Option<i32> {
    let v = flag.load(std::sync::atomic::Ordering::SeqCst);
    if v == 0 { None } else { Some(v) }
}

/// Fabricate an `ExitStatus(0)` without spawning a child. Used by the
/// `--dry-run` path so the return type stays symmetric with the real
/// spawn path.
#[cfg(unix)]
fn synthetic_success() -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    ExitStatus::from_raw(0)
}

#[cfg(windows)]
fn synthetic_success() -> ExitStatus {
    use std::os::windows::process::ExitStatusExt;
    ExitStatus::from_raw(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_passes_plain_strings() {
        assert_eq!(shell_quote("q35"), "q35");
        assert_eq!(shell_quote("/path/to/file.fd"), "/path/to/file.fd");
        assert_eq!(shell_quote("format=raw,file=foo"), "format=raw,file=foo");
    }

    #[test]
    fn shell_quote_wraps_strings_with_spaces() {
        assert_eq!(shell_quote("hello world"), "'hello world'");
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn shell_quote_empty_string() {
        assert_eq!(shell_quote(""), "''");
    }
}
