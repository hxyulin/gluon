//! Standalone compile-time utility helpers.
//!
//! These functions are pure transformations with no dependencies on the
//! rest of the compile pipeline. They live here so they can be shared
//! across `compile_crate`, the scheduler, the validation pass, and the
//! run subsystem without creating circular module dependencies.

use std::borrow::Cow;

/// Normalize a user-supplied crate name into the form rustc accepts as
/// `--crate-name`.
///
/// rustc rejects `-` (and a handful of other punctuation) in
/// `--crate-name`. The cargo convention — and what gluon mirrors — is to
/// silently rewrite `-` → `_` so that idiomatic dashed names like
/// `my-kernel-lib` Just Work. Other invalid characters are left alone:
/// rustc will reject them loudly, which is the correct outcome (the user
/// has typed something genuinely wrong, not a stylistic choice).
///
/// Returns `Cow::Borrowed` for the common no-dash case so we don't pay
/// for an allocation on every compile.
///
/// **Important**: this function is *not* idempotent under
/// [`sanitise_crate_name`]. They have different goals — sanitise is for
/// generated identifiers, normalize is for user-typed names. Don't mix
/// them.
pub(crate) fn normalize_crate_name(name: &str) -> Cow<'_, str> {
    if name.contains('-') {
        Cow::Owned(name.replace('-', "_"))
    } else {
        Cow::Borrowed(name)
    }
}

/// Derive the executable suffix for a given target spec.
///
/// rustc appends a platform-specific suffix to `--crate-type bin` outputs
/// depending on the target triple. For most bare-metal targets the suffix
/// is empty; for UEFI targets it is `.efi`; for Windows targets it is
/// `.exe`. This function matches on well-known triple suffixes so gluon
/// can predict the on-disk filename without spawning `rustc --print
/// file-names` (which would add a per-target rustc invocation).
///
/// The match is intentionally conservative: unknown targets get no suffix
/// (the common case for bare-metal). If a custom target spec has an unusual
/// suffix, the user can work around this by adding `-o <name>` to
/// `rustc_flags` in their `gluon.rhai`.
pub(crate) fn exe_suffix_for_target(spec: &str) -> &'static str {
    // UEFI targets produce PE32+ with .efi extension.
    if spec.ends_with("-uefi") || spec.contains("-uefi-") {
        return ".efi";
    }
    // Windows targets produce PE with .exe extension.
    if spec.contains("-windows-") || spec.ends_with("-windows") {
        return ".exe";
    }
    // WebAssembly targets produce .wasm files when compiled as bin.
    if spec.starts_with("wasm32-") || spec.starts_with("wasm64-") {
        return ".wasm";
    }
    // Everything else (Linux, macOS, bare-metal, custom specs) — no suffix.
    ""
}

/// Sanitise an arbitrary string to a valid Rust identifier component.
///
/// Lowercases every character and replaces any byte that is not `[a-z0-9_]`
/// with `_`. The result is suitable as a crate name or the prefix of one.
///
/// `pub(crate)` so that `scheduler::helpers::config_crate` can reuse this
/// logic without duplication — both places derive config-crate names from
/// the project name using the same rules.
pub(crate) fn sanitise_crate_name(s: &str) -> String {
    s.chars()
        .map(|c| {
            let lc = c.to_ascii_lowercase();
            if lc.is_ascii_alphanumeric() || lc == '_' {
                lc
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── sanitise_crate_name ──────────────────────────────────────────

    #[test]
    fn sanitise_alphanumeric_passthrough() {
        assert_eq!(sanitise_crate_name("hello123"), "hello123");
    }

    #[test]
    fn sanitise_uppercase_to_lowercase() {
        assert_eq!(sanitise_crate_name("MyKernel"), "mykernel");
    }

    #[test]
    fn sanitise_dashes_to_underscores() {
        assert_eq!(sanitise_crate_name("my-kernel"), "my_kernel");
    }

    #[test]
    fn sanitise_dots_and_spaces() {
        assert_eq!(sanitise_crate_name("my.kernel lib"), "my_kernel_lib");
    }

    #[test]
    fn sanitise_mixed_invalid_chars() {
        assert_eq!(
            sanitise_crate_name("Hello-World.2024!"),
            "hello_world_2024_"
        );
    }

    #[test]
    fn sanitise_empty_string() {
        assert_eq!(sanitise_crate_name(""), "");
    }

    // ── exe_suffix_for_target ────────────────────────────────────────

    #[test]
    fn suffix_bare_metal() {
        assert_eq!(exe_suffix_for_target("x86_64-unknown-none"), "");
    }

    #[test]
    fn suffix_uefi() {
        assert_eq!(exe_suffix_for_target("x86_64-unknown-uefi"), ".efi");
    }

    #[test]
    fn suffix_uefi_mid_triple() {
        assert_eq!(
            exe_suffix_for_target("aarch64-unknown-uefi-something"),
            ".efi"
        );
    }

    #[test]
    fn suffix_windows() {
        assert_eq!(exe_suffix_for_target("x86_64-pc-windows-msvc"), ".exe");
    }

    #[test]
    fn suffix_wasm32() {
        assert_eq!(exe_suffix_for_target("wasm32-unknown-unknown"), ".wasm");
    }

    #[test]
    fn suffix_wasm64() {
        assert_eq!(exe_suffix_for_target("wasm64-unknown-unknown"), ".wasm");
    }

    #[test]
    fn suffix_linux() {
        assert_eq!(exe_suffix_for_target("x86_64-unknown-linux-gnu"), "");
    }

    #[test]
    fn suffix_macos() {
        assert_eq!(exe_suffix_for_target("aarch64-apple-darwin"), "");
    }

    #[test]
    fn suffix_custom_spec() {
        assert_eq!(exe_suffix_for_target("my-custom-target"), "");
    }

    // ── normalize_crate_name ─────────────────────────────────────────

    #[test]
    fn normalize_no_dashes_borrows() {
        let result = normalize_crate_name("hello");
        assert_eq!(result, "hello");
        assert!(matches!(result, Cow::Borrowed(_)));
    }

    #[test]
    fn normalize_with_dashes_owns() {
        let result = normalize_crate_name("my-kernel");
        assert_eq!(result, "my_kernel");
        assert!(matches!(result, Cow::Owned(_)));
    }

    #[test]
    fn normalize_underscores_untouched_borrows() {
        let result = normalize_crate_name("my_kernel");
        assert_eq!(result, "my_kernel");
        assert!(matches!(result, Cow::Borrowed(_)));
    }
}
