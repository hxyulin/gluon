//! Pure QEMU argv assembly.
//!
//! `build_qemu_command` is a deterministic, side-effect-free function
//! that takes a [`ResolvedQemu`] plus the boot-mode inputs and produces
//! a [`QemuInvocation`] (binary + args + timeout). Keeping it pure
//! means:
//!
//! - Unit tests for argv composition never need a real filesystem or a
//!   real OVMF install.
//! - The `gluon run --dry-run` path can print the exact argv that
//!   would be used without needing QEMU at all.
//! - Future `gluon test` can reuse this for per-test argv assembly
//!   while managing the surrounding process lifecycle itself.
//!
//! All "live" work (resolving OVMF paths, copying a writable vars file,
//! spawning) lives in [`super::ovmf`] and [`super`] respectively.

use super::ovmf::ResolvedOvmf;
use super::resolve::ResolvedQemu;
use crate::error::{Error, Result};
use gluon_model::{BootMode, EspSource, SerialMode};
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

/// A complete QEMU command plus execution metadata.
#[derive(Debug, Clone)]
pub struct QemuInvocation {
    pub binary: String,
    pub args: Vec<OsString>,
    pub timeout: Option<Duration>,
}

/// Assemble the final QEMU argv.
///
/// `kernel` is the path that gets fed to `-kernel` in direct mode. In
/// UEFI mode it is unused by QEMU itself (UEFI loads a PE32+ image
/// from the ESP) but we still carry it for diagnostic messages.
///
/// `ovmf` **must** be `Some` when `mode == BootMode::Uefi` and `None`
/// otherwise. The caller is responsible for running
/// [`super::ovmf::resolve_ovmf`] and any vars-writability copy before
/// calling this function.
///
/// Errors:
/// - [`Error::Config`] if `mode == Uefi` but `ovmf` is `None`.
/// - [`Error::EspMissing`] if the ESP source file/directory does not
///   exist on disk. This is the only filesystem touch in this function;
///   it is done because letting QEMU fail 200ms later with a cryptic
///   "no bootable device" is a worse experience than a targeted error.
pub fn build_qemu_command(
    resolved: &ResolvedQemu,
    kernel: &Path,
    mode: BootMode,
    ovmf: Option<&ResolvedOvmf>,
    extra_cli_args: &[OsString],
    test_mode: bool,
    skip_path_checks: bool,
) -> Result<QemuInvocation> {
    if mode == BootMode::Uefi && ovmf.is_none() {
        return Err(Error::Config(
            "internal: build_qemu_command(UEFI) called without a resolved OVMF firmware".into(),
        ));
    }

    let mut args: Vec<OsString> = Vec::new();

    // 1. Machine type.
    args.push("-machine".into());
    args.push(resolved.machine.clone().into());

    // 2. Memory.
    args.push("-m".into());
    args.push(format!("{}M", resolved.memory_mb).into());

    // 3. SMP.
    args.push("-smp".into());
    args.push(resolved.cores.to_string().into());

    // 4. Default `-display none` unless the user set a `-display` flag
    //    themselves. Dry-running tests assert this exact ordering, so
    //    keep the override-check simple and well-commented.
    if !user_has_flag(&resolved.extra_args, extra_cli_args, "-display") {
        args.push("-display".into());
        args.push("none".into());
    }

    // 5. Serial policy. Skip if the user set an explicit `-serial`.
    if !user_has_flag(&resolved.extra_args, extra_cli_args, "-serial") {
        match &resolved.serial {
            SerialMode::Stdio => {
                args.push("-serial".into());
                args.push("stdio".into());
            }
            SerialMode::None => {
                args.push("-serial".into());
                args.push("none".into());
            }
            SerialMode::File(path) => {
                args.push("-serial".into());
                let mut spec = OsString::from("file:");
                spec.push(path.as_os_str());
                args.push(spec);
            }
        }
    }

    // 6. Mode-specific boot wiring.
    match mode {
        BootMode::Direct => {
            args.push("-kernel".into());
            args.push(kernel.as_os_str().to_os_string());
        }
        BootMode::Uefi => {
            // Safety: checked above.
            let ovmf = ovmf.expect("uefi + ovmf invariant");
            push_pflash(&mut args, &ovmf.code, true);
            push_pflash(&mut args, &ovmf.vars, false);

            if let Some(esp) = &resolved.esp {
                // `skip_path_checks` is set by the runner when it's
                // in `--dry-run` or `--no-build` mode. In those modes
                // the ESP path may legitimately not exist yet (dry-run
                // skips the build; `--no-build` trusts the user), and
                // QEMU itself will produce a better error at spawn
                // time than we can from here.
                match esp {
                    EspSource::Dir(dir) => {
                        if !skip_path_checks && !dir.exists() {
                            return Err(Error::EspMissing { path: dir.clone() });
                        }
                        args.push("-drive".into());
                        let mut spec = OsString::from("format=raw,file=fat:rw:");
                        spec.push(dir.as_os_str());
                        args.push(spec);
                    }
                    EspSource::Image(img) => {
                        if !skip_path_checks && !img.exists() {
                            return Err(Error::EspMissing { path: img.clone() });
                        }
                        args.push("-drive".into());
                        let mut spec = OsString::from("format=raw,file=");
                        spec.push(img.as_os_str());
                        args.push(spec);
                    }
                }
            }
        }
    }

    // 7. Test-mode exit device. Wired in here (before user extras)
    //    so the user can still override or suppress via `extra_args`
    //    if they really want to, but positioned ahead of the CLI
    //    pass-through so `gluon test -- <extras>` doesn't clobber
    //    it by accident. The port + iosize combo matches the
    //    convention documented in the Rust embedonomicon for
    //    bare-metal test exit; iosize is
    //    fixed at 0x04 because QEMU's isa-debug-exit device takes
    //    a 32-bit write at that offset.
    if test_mode {
        args.push("-device".into());
        args.push(
            format!(
                "isa-debug-exit,iobase=0x{:x},iosize=0x04",
                resolved.test_exit_port
            )
            .into(),
        );
    }

    // 8. User's qemu.extra_args (from Rhai).
    for a in &resolved.extra_args {
        args.push(a.into());
    }

    // 9. CLI pass-through (`gluon run -- <extra>`).
    args.extend(extra_cli_args.iter().cloned());

    Ok(QemuInvocation {
        binary: resolved.binary.clone(),
        args,
        timeout: resolved.timeout,
    })
}

fn push_pflash(args: &mut Vec<OsString>, file: &Path, readonly: bool) {
    args.push("-drive".into());
    let mut spec = OsString::from(if readonly {
        "if=pflash,format=raw,readonly=on,file="
    } else {
        "if=pflash,format=raw,file="
    });
    spec.push(file.as_os_str());
    args.push(spec);
}

/// Whether any of the user-provided argument lists contain `flag`.
///
/// Checks both the Rhai-level `extra_args` and the `gluon run -- ...`
/// CLI pass-through. Used by the default `-display none` / `-serial
/// stdio` insertion logic so users can override either with a bare
/// `-display gtk` in their config or on the command line.
fn user_has_flag(rhai_extras: &[String], cli_extras: &[OsString], flag: &str) -> bool {
    rhai_extras.iter().any(|a| a == flag) || cli_extras.iter().any(|a| a.to_string_lossy() == flag)
}

/// Suggestive test helper: a `ResolvedQemu` that the caller customises.
#[cfg(test)]
fn stub_resolved() -> ResolvedQemu {
    ResolvedQemu {
        binary: "qemu-system-x86_64".into(),
        machine: "q35".into(),
        memory_mb: 256,
        cores: 1,
        serial: SerialMode::Stdio,
        extra_args: Vec::new(),
        boot_mode: BootMode::Direct,
        ovmf_code: None,
        ovmf_vars: None,
        esp: None,
        test_exit_port: 0xf4,
        test_success_code: 0x10,
        timeout: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn os(s: &str) -> OsString {
        OsString::from(s)
    }

    fn args_to_strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn direct_mode_basic_argv() {
        let resolved = stub_resolved();
        let kernel = PathBuf::from("/tmp/build/kernel");
        let inv =
            build_qemu_command(&resolved, &kernel, BootMode::Direct, None, &[], false, false).unwrap();
        let args = args_to_strings(&inv.args);
        assert_eq!(
            args,
            vec![
                "-machine",
                "q35",
                "-m",
                "256M",
                "-smp",
                "1",
                "-display",
                "none",
                "-serial",
                "stdio",
                "-kernel",
                "/tmp/build/kernel",
            ]
        );
        assert_eq!(inv.binary, "qemu-system-x86_64");
    }

    #[test]
    fn direct_mode_custom_memory_and_cores() {
        let mut r = stub_resolved();
        r.memory_mb = 1024;
        r.cores = 4;
        let inv =
            build_qemu_command(&r, Path::new("/k"), BootMode::Direct, None, &[], false, false).unwrap();
        let a = args_to_strings(&inv.args);
        assert!(a.windows(2).any(|w| w == ["-m", "1024M"]));
        assert!(a.windows(2).any(|w| w == ["-smp", "4"]));
    }

    #[test]
    fn uefi_mode_emits_pflash_pair() {
        let mut r = stub_resolved();
        r.boot_mode = BootMode::Uefi;
        let ovmf = ResolvedOvmf {
            code: PathBuf::from("/opt/ovmf/CODE.fd"),
            vars: PathBuf::from("/tmp/build/VARS.fd"),
        };
        let inv = build_qemu_command(
            &r,
            Path::new("/ignored"),
            BootMode::Uefi,
            Some(&ovmf),
            &[],
            false,
            false,
        )
        .unwrap();
        let a = args_to_strings(&inv.args);
        let code_arg = "if=pflash,format=raw,readonly=on,file=/opt/ovmf/CODE.fd";
        let vars_arg = "if=pflash,format=raw,file=/tmp/build/VARS.fd";
        assert!(
            a.windows(2).any(|w| w[0] == "-drive" && w[1] == code_arg),
            "expected readonly code pflash, got {a:?}"
        );
        assert!(
            a.windows(2).any(|w| w[0] == "-drive" && w[1] == vars_arg),
            "expected writable vars pflash, got {a:?}"
        );
        // Direct-mode -kernel flag must NOT appear in UEFI mode.
        assert!(!a.iter().any(|s| s == "-kernel"));
    }

    #[test]
    fn uefi_mode_requires_ovmf() {
        let mut r = stub_resolved();
        r.boot_mode = BootMode::Uefi;
        let err =
            build_qemu_command(&r, Path::new("/k"), BootMode::Uefi, None, &[], false, false).unwrap_err();
        assert!(matches!(err, Error::Config(_)));
    }

    #[test]
    fn uefi_mode_with_esp_dir_mounts_fat_rw() {
        let tmp = tempfile::tempdir().unwrap();
        let esp = tmp.path().join("esp");
        std::fs::create_dir_all(&esp).unwrap();

        let mut r = stub_resolved();
        r.boot_mode = BootMode::Uefi;
        r.esp = Some(EspSource::Dir(esp.clone()));
        let ovmf = ResolvedOvmf {
            code: PathBuf::from("/a"),
            vars: PathBuf::from("/b"),
        };
        let inv = build_qemu_command(&r, Path::new("/k"), BootMode::Uefi, Some(&ovmf), &[], false, false)
            .unwrap();
        let a = args_to_strings(&inv.args);
        let expected = format!("format=raw,file=fat:rw:{}", esp.display());
        assert!(
            a.windows(2).any(|w| w[0] == "-drive" && w[1] == expected),
            "expected fat:rw drive for {}, got {:?}",
            esp.display(),
            a
        );
    }

    #[test]
    fn uefi_mode_with_esp_image_mounts_raw() {
        let tmp = tempfile::tempdir().unwrap();
        let img = tmp.path().join("disk.img");
        std::fs::write(&img, b"stub").unwrap();

        let mut r = stub_resolved();
        r.boot_mode = BootMode::Uefi;
        r.esp = Some(EspSource::Image(img.clone()));
        let ovmf = ResolvedOvmf {
            code: PathBuf::from("/a"),
            vars: PathBuf::from("/b"),
        };
        let inv = build_qemu_command(&r, Path::new("/k"), BootMode::Uefi, Some(&ovmf), &[], false, false)
            .unwrap();
        let a = args_to_strings(&inv.args);
        let expected = format!("format=raw,file={}", img.display());
        assert!(
            a.windows(2).any(|w| w[0] == "-drive" && w[1] == expected),
            "expected raw image drive, got {:?}",
            a
        );
    }

    #[test]
    fn esp_dir_missing_errors() {
        let mut r = stub_resolved();
        r.boot_mode = BootMode::Uefi;
        r.esp = Some(EspSource::Dir(PathBuf::from("/nonexistent/esp/abc123")));
        let ovmf = ResolvedOvmf {
            code: PathBuf::from("/a"),
            vars: PathBuf::from("/b"),
        };
        let err = build_qemu_command(&r, Path::new("/k"), BootMode::Uefi, Some(&ovmf), &[], false, false)
            .unwrap_err();
        assert!(matches!(err, Error::EspMissing { .. }));
    }

    #[test]
    fn user_display_override_skips_default_display_none() {
        let mut r = stub_resolved();
        r.extra_args = vec!["-display".into(), "gtk".into()];
        let inv =
            build_qemu_command(&r, Path::new("/k"), BootMode::Direct, None, &[], false, false).unwrap();
        let a = args_to_strings(&inv.args);
        // Default "none" must not be injected when user specified -display.
        let idx = a.iter().position(|s| s == "-display").unwrap();
        assert_eq!(a[idx + 1], "gtk");
        assert!(!a.iter().any(|s| s == "none"), "unexpected: {a:?}");
    }

    #[test]
    fn user_cli_display_override_also_skips_default() {
        let r = stub_resolved();
        let extra = [os("-display"), os("curses")];
        let inv =
            build_qemu_command(&r, Path::new("/k"), BootMode::Direct, None, &extra, false, false).unwrap();
        let a = args_to_strings(&inv.args);
        // Default -display none should NOT appear; the user's -display
        // curses is appended at the end via cli_extras.
        let display_positions: Vec<_> = a
            .iter()
            .enumerate()
            .filter(|(_, s)| *s == "-display")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(display_positions.len(), 1, "{a:?}");
    }

    #[test]
    fn user_extra_args_ordering() {
        let mut r = stub_resolved();
        r.extra_args = vec!["-d".into(), "int".into()];
        let extra = [os("-no-reboot")];
        let inv =
            build_qemu_command(&r, Path::new("/k"), BootMode::Direct, None, &extra, false, false).unwrap();
        let a = args_to_strings(&inv.args);
        // The kernel flag is part of gluon's managed section;
        // user args (rhai then cli) come after it in order.
        let k = a.iter().position(|s| s == "-kernel").unwrap();
        let d = a.iter().position(|s| s == "-d").unwrap();
        let nr = a.iter().position(|s| s == "-no-reboot").unwrap();
        assert!(k < d && d < nr, "order broken: {a:?}");
    }

    #[test]
    fn serial_none_emits_none() {
        let mut r = stub_resolved();
        r.serial = SerialMode::None;
        let inv =
            build_qemu_command(&r, Path::new("/k"), BootMode::Direct, None, &[], false, false).unwrap();
        let a = args_to_strings(&inv.args);
        let idx = a.iter().position(|s| s == "-serial").unwrap();
        assert_eq!(a[idx + 1], "none");
    }

    #[test]
    fn serial_file_emits_file_spec() {
        let mut r = stub_resolved();
        r.serial = SerialMode::File(PathBuf::from("/tmp/out.log"));
        let inv =
            build_qemu_command(&r, Path::new("/k"), BootMode::Direct, None, &[], false, false).unwrap();
        let a = args_to_strings(&inv.args);
        let idx = a.iter().position(|s| s == "-serial").unwrap();
        assert_eq!(a[idx + 1], "file:/tmp/out.log");
    }

    #[test]
    fn timeout_propagates_to_invocation() {
        let mut r = stub_resolved();
        r.timeout = Some(Duration::from_secs(10));
        let inv =
            build_qemu_command(&r, Path::new("/k"), BootMode::Direct, None, &[], false, false).unwrap();
        assert_eq!(inv.timeout, Some(Duration::from_secs(10)));
    }

    #[test]
    fn test_mode_off_omits_isa_debug_exit() {
        let r = stub_resolved();
        let inv =
            build_qemu_command(&r, Path::new("/k"), BootMode::Direct, None, &[], false, false).unwrap();
        let a = args_to_strings(&inv.args);
        assert!(
            !a.iter().any(|s| s.contains("isa-debug-exit")),
            "test_mode=false must not emit isa-debug-exit, got: {a:?}"
        );
    }

    #[test]
    fn test_mode_on_emits_isa_debug_exit_with_resolved_port() {
        let mut r = stub_resolved();
        r.test_exit_port = 0xf4;
        let inv =
            build_qemu_command(&r, Path::new("/k"), BootMode::Direct, None, &[], true, false).unwrap();
        let a = args_to_strings(&inv.args);
        // Expect the pair `-device isa-debug-exit,iobase=0xf4,iosize=0x04`.
        let pair: Vec<_> = a
            .windows(2)
            .filter(|w| w[0] == "-device" && w[1].starts_with("isa-debug-exit"))
            .collect();
        assert_eq!(
            pair.len(),
            1,
            "expected exactly one -device entry, got {a:?}"
        );
        assert_eq!(
            pair[0][1], "isa-debug-exit,iobase=0xf4,iosize=0x04",
            "exit device spec mismatch"
        );
    }

    #[test]
    fn test_mode_honours_custom_exit_port() {
        let mut r = stub_resolved();
        r.test_exit_port = 0x501;
        let inv =
            build_qemu_command(&r, Path::new("/k"), BootMode::Direct, None, &[], true, false).unwrap();
        let a = args_to_strings(&inv.args);
        assert!(
            a.iter()
                .any(|s| s == "isa-debug-exit,iobase=0x501,iosize=0x04"),
            "custom port must appear in the spec, got {a:?}"
        );
    }

    #[test]
    fn test_mode_device_precedes_user_extra_args() {
        // The managed isa-debug-exit entry must come BEFORE the
        // user's Rhai-level extra_args, so a user who wants to
        // override or augment the test device can append another
        // -device entry and trust the order.
        let mut r = stub_resolved();
        r.extra_args = vec!["-d".into(), "int".into()];
        let inv =
            build_qemu_command(&r, Path::new("/k"), BootMode::Direct, None, &[], true, false).unwrap();
        let a = args_to_strings(&inv.args);
        let pos_device = a
            .iter()
            .position(|s| s.starts_with("isa-debug-exit"))
            .unwrap();
        let pos_d = a.iter().position(|s| s == "-d").unwrap();
        assert!(
            pos_device < pos_d,
            "managed -device must precede user extras: {a:?}"
        );
    }
}
