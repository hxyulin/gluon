//! OVMF firmware path resolver.
//!
//! UEFI boot in QEMU needs two firmware files:
//!
//! - **CODE**: the read-only OVMF UEFI image (`OVMF_CODE.fd`). QEMU
//!   mmaps this as the lower pflash bank.
//! - **VARS**: the writable NVRAM store
//!   (`OVMF_VARS.fd`). UEFI updates this on every boot, so QEMU
//!   requires it to be writable.
//!
//! We do not ship OVMF. Users provide paths via (in precedence order):
//!
//! 1. Explicit `.ovmf_code(...)` / `.ovmf_vars(...)` calls in
//!    `gluon.rhai` → populate [`gluon_model::QemuDef::ovmf_code`] and
//!    `ovmf_vars`.
//! 2. `OVMF_CODE` / `OVMF_VARS` environment variables.
//! 3. A small table of well-known system paths per OS.
//!
//! If none of those resolve, we return [`Error::OvmfNotFound`] with a
//! multi-line diagnostic listing every fallback the resolver tried,
//! so the user knows exactly what to install or set.
//!
//! ## Writable vars copy
//!
//! A UEFI firmware image on disk (especially one installed by the
//! system package manager) is usually read-only for non-root users.
//! Since QEMU has to write NVRAM updates back to the vars file,
//! running with a read-only path would fail at boot.
//!
//! The resolver detects this and copies the vars file once to
//! `<build_root>/ovmf_vars-<profile>.fd`, returning the writable copy
//! path instead. Subsequent runs re-use the copy as long as its mtime
//! is ≥ the source mtime; if the source is refreshed (e.g. via a
//! package upgrade), we re-copy.
//!
//! The per-profile suffix exists so two concurrent runs of different
//! profiles don't race on the same NVRAM file — NVRAM state is
//! per-boot-environment and shouldn't leak between profiles.

use crate::error::{Error, Result};
use gluon_model::QemuDef;
use std::path::{Path, PathBuf};

/// OVMF firmware paths ready to pass to QEMU.
///
/// `vars` is always a writable path — the resolver has already made a
/// copy if the source was read-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedOvmf {
    pub code: PathBuf,
    pub vars: PathBuf,
}

/// Well-known OVMF file pairs to probe when no explicit path or env
/// var is set. First match whose *both* files exist wins.
///
/// Ordering is: Linux (most common) → macOS → anything more exotic.
/// Add new entries at the bottom; earlier probes are hot paths.
const SYSTEM_PATHS: &[(&str, &str)] = &[
    // Ubuntu/Debian `ovmf` package.
    (
        "/usr/share/OVMF/OVMF_CODE.fd",
        "/usr/share/OVMF/OVMF_VARS.fd",
    ),
    // Fedora/RHEL `edk2-ovmf` package.
    (
        "/usr/share/edk2-ovmf/x64/OVMF_CODE.fd",
        "/usr/share/edk2-ovmf/x64/OVMF_VARS.fd",
    ),
    // Arch Linux `edk2-ovmf` package.
    (
        "/usr/share/edk2/x64/OVMF_CODE.fd",
        "/usr/share/edk2/x64/OVMF_VARS.fd",
    ),
    // Homebrew (Apple Silicon) `qemu` bottle.
    (
        "/opt/homebrew/share/qemu/edk2-x86_64-code.fd",
        "/opt/homebrew/share/qemu/edk2-i386-vars.fd",
    ),
    // Homebrew (Intel) `qemu` bottle.
    (
        "/usr/local/share/qemu/edk2-x86_64-code.fd",
        "/usr/local/share/qemu/edk2-i386-vars.fd",
    ),
];

/// Context carried into [`resolve_ovmf`] — lets callers override the
/// env and system probe for hermetic testing.
///
/// In production, construct with [`OvmfResolveCtx::from_env`]. In
/// tests, build a context with a fake env lookup and a custom
/// `system_paths` slice pointing at tempdir-backed fixtures.
#[derive(Clone)]
pub struct OvmfResolveCtx<'a> {
    pub build_root: &'a Path,
    pub profile_name: &'a str,
    pub env_lookup: fn(&str) -> Option<String>,
    pub system_paths: &'a [(&'a str, &'a str)],
}

impl<'a> OvmfResolveCtx<'a> {
    /// Production context: real env, hard-coded `SYSTEM_PATHS`.
    pub fn from_env(build_root: &'a Path, profile_name: &'a str) -> Self {
        Self {
            build_root,
            profile_name,
            env_lookup: real_env,
            system_paths: SYSTEM_PATHS,
        }
    }
}

fn real_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Resolve OVMF CODE+VARS into paths usable by QEMU.
///
/// Three-stage fallback: explicit → env → system probe. Returns an
/// [`Error::OvmfNotFound`] whose message lists every fallback it
/// tried.
pub fn resolve_ovmf(qemu: &QemuDef, ctx: &OvmfResolveCtx<'_>) -> Result<ResolvedOvmf> {
    // ---- stage 1: explicit paths from `.ovmf_code()`/`.ovmf_vars()`
    let explicit_code = qemu.ovmf_code.clone();
    let explicit_vars = qemu.ovmf_vars.clone();

    // ---- stage 2: env
    let env_code = (ctx.env_lookup)("OVMF_CODE").map(PathBuf::from);
    let env_vars = (ctx.env_lookup)("OVMF_VARS").map(PathBuf::from);

    // Either stage can contribute one of the two paths; if at least
    // one is set, we consume *both* of its values and never fall
    // through to the system probe. Mixing (explicit code + env vars)
    // is intentional — it's occasionally useful to override only one.
    let code = explicit_code.clone().or(env_code.clone());
    let vars = explicit_vars.clone().or(env_vars.clone());

    let (code, vars) = match (code, vars) {
        (Some(c), Some(v)) => (c, v),
        _ => {
            // ---- stage 3: system probe
            match probe_system(ctx.system_paths) {
                Some(pair) => pair,
                None => {
                    return Err(Error::OvmfNotFound {
                        attempts: render_attempts(
                            &explicit_code,
                            &explicit_vars,
                            &env_code,
                            &env_vars,
                            ctx.system_paths,
                        ),
                    });
                }
            }
        }
    };

    // Validate presence — catching a bad explicit/env path here gives
    // a clearer error than letting QEMU fail with "Could not open
    // '/path': No such file or directory".
    if !code.exists() {
        return Err(Error::OvmfNotFound {
            attempts: format!(
                "OVMF CODE path does not exist: {}\n\
                 (source: {}).",
                code.display(),
                source_label(&explicit_code, &env_code)
            ),
        });
    }
    if !vars.exists() {
        return Err(Error::OvmfNotFound {
            attempts: format!(
                "OVMF VARS path does not exist: {}\n\
                 (source: {}).",
                vars.display(),
                source_label(&explicit_vars, &env_vars)
            ),
        });
    }

    // Copy vars to a writable location if the source isn't writable,
    // or if the source lives outside the build dir (system paths are
    // typically read-only to non-root users anyway, so we always copy
    // from SYSTEM_PATHS hits).
    let writable_vars = ensure_writable_vars(&vars, ctx.build_root, ctx.profile_name)?;

    Ok(ResolvedOvmf {
        code,
        vars: writable_vars,
    })
}

/// First `(code, vars)` pair from the table whose both files exist.
fn probe_system(paths: &[(&str, &str)]) -> Option<(PathBuf, PathBuf)> {
    for (c, v) in paths {
        let cp = PathBuf::from(c);
        let vp = PathBuf::from(v);
        if cp.exists() && vp.exists() {
            return Some((cp, vp));
        }
    }
    None
}

/// Render a diagnostic describing every fallback layer we tried.
fn render_attempts(
    explicit_code: &Option<PathBuf>,
    explicit_vars: &Option<PathBuf>,
    env_code: &Option<PathBuf>,
    env_vars: &Option<PathBuf>,
    system_paths: &[(&str, &str)],
) -> String {
    let mut out = String::from("Tried, in order:\n");
    out.push_str("  1. Explicit: ");
    match (explicit_code, explicit_vars) {
        (Some(c), Some(v)) => {
            out.push_str(&format!("code={}, vars={}\n", c.display(), v.display()))
        }
        (Some(c), None) => out.push_str(&format!("code={} (vars unset)\n", c.display())),
        (None, Some(v)) => out.push_str(&format!("vars={} (code unset)\n", v.display())),
        (None, None) => out.push_str("(.ovmf_code / .ovmf_vars not called)\n"),
    }
    out.push_str("  2. Env: ");
    match (env_code, env_vars) {
        (Some(c), Some(v)) => out.push_str(&format!(
            "OVMF_CODE={}, OVMF_VARS={}\n",
            c.display(),
            v.display()
        )),
        (Some(c), None) => out.push_str(&format!("OVMF_CODE={} (OVMF_VARS unset)\n", c.display())),
        (None, Some(v)) => out.push_str(&format!("OVMF_VARS={} (OVMF_CODE unset)\n", v.display())),
        (None, None) => out.push_str("OVMF_CODE / OVMF_VARS unset\n"),
    }
    out.push_str("  3. System paths (first existing pair wins):\n");
    for (c, v) in system_paths {
        out.push_str(&format!("       {c} + {v}\n"));
    }
    out.push_str(
        "\nInstall OVMF (apt install ovmf / brew install qemu / pacman -S edk2-ovmf),\n\
         or set OVMF_CODE and OVMF_VARS, or declare explicit paths in gluon.rhai via\n\
         qemu().ovmf_code(\"...\").ovmf_vars(\"...\").",
    );
    out
}

fn source_label(explicit: &Option<PathBuf>, env: &Option<PathBuf>) -> &'static str {
    match (explicit, env) {
        (Some(_), _) => "explicit .ovmf_* setting in gluon.rhai",
        (None, Some(_)) => "OVMF_CODE / OVMF_VARS env var",
        (None, None) => "system probe",
    }
}

/// Ensure the vars file is writable.
///
/// If the source file is already writable, return its path as-is.
/// Otherwise copy it to `<build_root>/ovmf_vars-<profile>.fd` and
/// return the copy. Re-copy if the source mtime is newer than the
/// cached copy.
fn ensure_writable_vars(source: &Path, build_root: &Path, profile_name: &str) -> Result<PathBuf> {
    if is_writable(source) {
        return Ok(source.to_path_buf());
    }

    // Derive a deterministic cached copy path. Sanitising the profile
    // name keeps weird characters from breaking filesystems; we only
    // strip `/` and `\` since those are the dangerous ones.
    let safe_profile: String = profile_name
        .chars()
        .map(|c| if matches!(c, '/' | '\\') { '_' } else { c })
        .collect();
    let cached = build_root.join(format!("ovmf_vars-{safe_profile}.fd"));

    // Create build_root if missing; the first `gluon run` on a fresh
    // tree might be called before `gluon build` has ever populated it.
    if let Some(parent) = cached.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }

    let needs_copy = match (std::fs::metadata(&cached), std::fs::metadata(source)) {
        (Ok(c_meta), Ok(s_meta)) => {
            let c_mtime = c_meta.modified().ok();
            let s_mtime = s_meta.modified().ok();
            match (c_mtime, s_mtime) {
                (Some(c), Some(s)) => s > c,
                _ => true,
            }
        }
        (Err(_), _) => true,
        _ => true,
    };

    if needs_copy {
        std::fs::copy(source, &cached).map_err(|e| Error::Io {
            path: cached.clone(),
            source: e,
        })?;
        // Clear the read-only bit on Unix — `std::fs::copy` preserves
        // permissions, so a 0444 source produces a 0444 dest. QEMU
        // needs write, so force 0644.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = std::fs::metadata(&cached)
                .map_err(|e| Error::Io {
                    path: cached.clone(),
                    source: e,
                })?
                .permissions();
            perm.set_mode(0o644);
            std::fs::set_permissions(&cached, perm).map_err(|e| Error::Io {
                path: cached.clone(),
                source: e,
            })?;
        }
    }

    Ok(cached)
}

/// Best-effort check for write access. On Unix we look at the owner
/// write bit against our EUID; on other platforms we fall back to
/// trying to open the file for append (cheapest portable check) and
/// bouncing the result.
fn is_writable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mode = meta.permissions().mode();
            // Owner write bit. Not perfect (ignores group/other and
            // ACLs) but good enough to short-circuit the common case
            // where the user's own files are rw and system files are
            // 0444.
            return mode & 0o200 != 0;
        }
        false
    }
    #[cfg(not(unix))]
    {
        std::fs::OpenOptions::new().append(true).open(path).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::{Mutex, OnceLock};

    fn test_env_empty(_: &str) -> Option<String> {
        None
    }

    // Shared state for env-override tests (serialise so one test's
    // set_var doesn't leak into another's lookup).
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn explicit_paths_win() {
        let tmp = tempfile::tempdir().unwrap();
        let code = tmp.path().join("CODE.fd");
        let vars = tmp.path().join("VARS.fd");
        fs::write(&code, b"code").unwrap();
        fs::write(&vars, b"vars").unwrap();

        let qemu = QemuDef {
            ovmf_code: Some(code.clone()),
            ovmf_vars: Some(vars.clone()),
            ..Default::default()
        };
        let build = tmp.path().join("build");
        fs::create_dir_all(&build).unwrap();
        let ctx = OvmfResolveCtx {
            build_root: &build,
            profile_name: "dev",
            env_lookup: test_env_empty,
            system_paths: &[],
        };
        let resolved = resolve_ovmf(&qemu, &ctx).unwrap();
        assert_eq!(resolved.code, code);
        // vars is writable so should not be copied.
        assert_eq!(resolved.vars, vars);
    }

    #[test]
    fn system_probe_falls_back_when_nothing_explicit() {
        let tmp = tempfile::tempdir().unwrap();
        let code = tmp.path().join("sys-code.fd");
        let vars = tmp.path().join("sys-vars.fd");
        fs::write(&code, b"code").unwrap();
        fs::write(&vars, b"vars").unwrap();

        let code_s = code.to_str().unwrap();
        let vars_s = vars.to_str().unwrap();
        let system = &[(code_s, vars_s)];

        let build = tmp.path().join("build");
        fs::create_dir_all(&build).unwrap();
        let ctx = OvmfResolveCtx {
            build_root: &build,
            profile_name: "dev",
            env_lookup: test_env_empty,
            system_paths: system,
        };
        let resolved = resolve_ovmf(&QemuDef::default(), &ctx).unwrap();
        assert_eq!(resolved.code, code);
    }

    #[test]
    fn missing_everything_produces_rich_error() {
        let tmp = tempfile::tempdir().unwrap();
        let build = tmp.path().join("build");
        fs::create_dir_all(&build).unwrap();
        let ctx = OvmfResolveCtx {
            build_root: &build,
            profile_name: "dev",
            env_lookup: test_env_empty,
            system_paths: &[],
        };
        let err = resolve_ovmf(&QemuDef::default(), &ctx).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no OVMF firmware found"));
        assert!(msg.contains("Explicit"));
        assert!(msg.contains("Env"));
        assert!(msg.contains("System paths"));
    }

    #[cfg(unix)]
    #[test]
    fn readonly_vars_gets_copied_into_build_dir() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let code = tmp.path().join("CODE.fd");
        let src_vars = tmp.path().join("SRC_VARS.fd");
        fs::write(&code, b"code").unwrap();
        fs::write(&src_vars, b"vars").unwrap();
        let mut perm = fs::metadata(&src_vars).unwrap().permissions();
        perm.set_mode(0o444);
        fs::set_permissions(&src_vars, perm).unwrap();

        let qemu = QemuDef {
            ovmf_code: Some(code.clone()),
            ovmf_vars: Some(src_vars.clone()),
            ..Default::default()
        };
        let build = tmp.path().join("build");
        fs::create_dir_all(&build).unwrap();
        let ctx = OvmfResolveCtx {
            build_root: &build,
            profile_name: "dev",
            env_lookup: test_env_empty,
            system_paths: &[],
        };
        let resolved = resolve_ovmf(&qemu, &ctx).unwrap();
        assert_ne!(resolved.vars, src_vars);
        let cached = build.join("ovmf_vars-dev.fd");
        assert_eq!(resolved.vars, cached);
        assert!(cached.exists());
        // Writable (owner bit set).
        let mode = fs::metadata(&cached).unwrap().permissions().mode();
        assert!(mode & 0o200 != 0);
        // Content preserved.
        assert_eq!(fs::read(&cached).unwrap(), b"vars");
    }

    #[test]
    fn env_vars_consulted_when_explicit_missing() {
        // Use a test-local env_lookup so we don't touch process env.
        let tmp = tempfile::tempdir().unwrap();
        let code = tmp.path().join("env-code.fd");
        let vars = tmp.path().join("env-vars.fd");
        fs::write(&code, b"code").unwrap();
        fs::write(&vars, b"vars").unwrap();

        // Stash paths in a static so the fn ptr can find them.
        static STASH: OnceLock<(String, String)> = OnceLock::new();
        let _ = STASH.set((
            code.to_string_lossy().into_owned(),
            vars.to_string_lossy().into_owned(),
        ));
        fn lookup(name: &str) -> Option<String> {
            let (c, v) = STASH.get()?;
            match name {
                "OVMF_CODE" => Some(c.clone()),
                "OVMF_VARS" => Some(v.clone()),
                _ => None,
            }
        }

        let build = tmp.path().join("build");
        fs::create_dir_all(&build).unwrap();
        let ctx = OvmfResolveCtx {
            build_root: &build,
            profile_name: "dev",
            env_lookup: lookup,
            system_paths: &[],
        };
        let _lock = env_lock().lock().unwrap();
        let resolved = resolve_ovmf(&QemuDef::default(), &ctx).unwrap();
        assert_eq!(resolved.code, code);
    }
}
