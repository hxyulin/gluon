//! The single point in gluon that assembles a `rustc` invocation.
//!
//! [`RustcCommandBuilder`] is the only place in the entire build system that
//! constructs a [`std::process::Command`] for `rustc`. Every later pipeline
//! stage — sysroot builds, host crates, cross crates — funnels through it.
//! Centralising this matters for two reasons:
//!
//! 1. **Auditability.** There is exactly one canonical encoding of every
//!    rustc flag gluon emits. Diagnostics and tests can rely on the token
//!    forms documented below, and any future change to a flag's spelling is
//!    a one-file edit.
//! 2. **Cache keying.** [`RustcCommandBuilder::hash`] produces a stable
//!    SHA-256 over the rustc binary path, the canonical argv, the env, and
//!    the cwd. Downstream cache logic uses this as the "have we already run
//!    this exact invocation?" key — so the encoding *must* be deterministic
//!    and *must* be domain-separated against other hashes in the system.
//!
//! Setters APPEND to the canonical arg list; they do not deduplicate. Where
//! a flag is naturally one-shot (`--crate-name`, `--target`, `--sysroot`),
//! callers are responsible for calling the setter at most once. Setters that
//! naturally repeat (`extern_crate`, `cfg`, `raw_arg`, `env`) preserve call
//! order in the resulting argv / hash.
//!
//! # Hash stability across platforms
//!
//! [`RustcCommandBuilder::hash`] is stable across runs on the same host
//! platform. The underlying `OsString` encoding differs between Unix (raw
//! bytes) and Windows (WTF-8), so cross-platform hash equality is only
//! guaranteed for inputs that are entirely pure ASCII. As a consequence, a
//! build cache shared between Unix and Windows hosts is not currently a
//! supported configuration.

use gluon_model::CrateType;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Kinds of output rustc can emit. Combined via a slice on
/// [`RustcCommandBuilder::emit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Emit {
    Link,
    Metadata,
    DepInfo,
}

impl Emit {
    pub fn as_str(&self) -> &'static str {
        match self {
            Emit::Link => "link",
            Emit::Metadata => "metadata",
            Emit::DepInfo => "dep-info",
        }
    }
}

/// Builder for a single `rustc` invocation.
///
/// See the module docs for the canonical token forms each setter produces.
/// All setters are infallible and chainable; the builder cannot fail to
/// build a [`std::process::Command`].
pub struct RustcCommandBuilder {
    rustc: PathBuf,
    /// Canonical, order-preserving arg list. Tests and the cache key both
    /// depend on the exact contents of this vector.
    args: Vec<OsString>,
    /// Stored as a `BTreeMap` so iteration order is deterministic regardless
    /// of insertion order — see [`RustcCommandBuilder::hash`].
    env: BTreeMap<OsString, OsString>,
    cwd: Option<PathBuf>,
}

impl RustcCommandBuilder {
    pub fn new(rustc: impl Into<PathBuf>) -> Self {
        Self {
            rustc: rustc.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
        }
    }

    /// Append a positional input path. No flag is emitted; the path is
    /// pushed verbatim and rustc treats trailing positional args as inputs.
    pub fn input(&mut self, path: &Path) -> &mut Self {
        self.args.push(path.as_os_str().to_os_string());
        self
    }

    /// One-shot: calling more than once will cause rustc to reject the
    /// invocation.
    pub fn crate_name(&mut self, name: &str) -> &mut Self {
        self.args.push(OsString::from("--crate-name"));
        self.args.push(OsString::from(name));
        self
    }

    /// One-shot: calling more than once will cause rustc to reject the
    /// invocation.
    pub fn crate_type(&mut self, kind: CrateType) -> &mut Self {
        self.args.push(OsString::from("--crate-type"));
        self.args.push(OsString::from(kind.as_str()));
        self
    }

    /// One-shot: calling more than once will cause rustc to reject the
    /// invocation.
    pub fn edition(&mut self, edition: &str) -> &mut Self {
        let mut s = OsString::from("--edition=");
        s.push(edition);
        self.args.push(s);
        self
    }

    /// Emit `--target`. When `builtin` is true, `spec` is a rustc builtin
    /// triple and gets folded into `--target=<spec>` as a single token. When
    /// false, `spec` is treated as a path to a target JSON spec file and is
    /// emitted as two tokens — `--target` then the path — so that paths
    /// containing `=` characters round-trip correctly.
    ///
    /// One-shot: calling more than once will cause rustc to reject the
    /// invocation.
    pub fn target(&mut self, spec: &str, builtin: bool) -> &mut Self {
        if builtin {
            let mut s = OsString::from("--target=");
            s.push(spec);
            self.args.push(s);
        } else {
            self.args.push(OsString::from("--target"));
            self.args.push(OsString::from(spec));
        }
        self
    }

    /// One-shot: calling more than once will cause rustc to reject the
    /// invocation.
    pub fn sysroot(&mut self, sysroot: &Path) -> &mut Self {
        self.args.push(OsString::from("--sysroot"));
        self.args.push(sysroot.as_os_str().to_os_string());
        self
    }

    /// One-shot: calling more than once will cause rustc to reject the
    /// invocation.
    pub fn out_dir(&mut self, dir: &Path) -> &mut Self {
        self.args.push(OsString::from("--out-dir"));
        self.args.push(dir.as_os_str().to_os_string());
        self
    }

    /// Combine the requested emit kinds into a single `--emit=a,b,c` token,
    /// preserving the order of the input slice.
    ///
    /// One-shot: calling more than once will cause rustc to reject the
    /// invocation.
    pub fn emit(&mut self, kinds: &[Emit]) -> &mut Self {
        self.emit_inner(kinds, None)
    }

    /// Like [`RustcCommandBuilder::emit`], but routes the dep-info output to an explicit path via
    /// `--emit=...,dep-info=<path>`. The caller is responsible for ensuring
    /// [`Emit::DepInfo`] appears in `kinds` (debug-asserted).
    ///
    /// Why this exists: rustc otherwise derives the depfile path from
    /// `--out-dir` + the crate name (plus any `-C extra-filename`), which
    /// works but is implicit — the caller has to reconstruct the exact
    /// filename rustc will choose. Making the path explicit eliminates that
    /// coupling and documents where the depfile lands in one place.
    ///
    /// One-shot: calling more than once will cause rustc to reject the
    /// invocation.
    pub fn emit_with_dep_info_path(&mut self, kinds: &[Emit], dep_info: &Path) -> &mut Self {
        debug_assert!(
            kinds.contains(&Emit::DepInfo),
            "emit_with_dep_info_path called without Emit::DepInfo in kinds: {kinds:?}"
        );
        self.emit_inner(kinds, Some(dep_info))
    }

    fn emit_inner(&mut self, kinds: &[Emit], dep_info: Option<&Path>) -> &mut Self {
        let mut s = OsString::from("--emit=");
        let mut first = true;
        for k in kinds {
            if !first {
                s.push(",");
            }
            first = false;
            match (k, dep_info) {
                (Emit::DepInfo, Some(path)) => {
                    s.push("dep-info=");
                    s.push(path.as_os_str());
                }
                _ => s.push(k.as_str()),
            }
        }
        self.args.push(s);
        self
    }

    pub fn extern_crate(&mut self, name: &str, rlib: &Path) -> &mut Self {
        debug_assert!(
            !name.contains('='),
            "extern crate name must not contain '=': {name:?}"
        );
        self.args.push(OsString::from("--extern"));
        let mut s = OsString::from(name);
        s.push("=");
        s.push(rlib.as_os_str());
        self.args.push(s);
        self
    }

    pub fn cfg(&mut self, flag: &str) -> &mut Self {
        self.args.push(OsString::from("--cfg"));
        self.args.push(OsString::from(flag));
        self
    }

    pub fn opt_level(&mut self, level: u8) -> &mut Self {
        self.args.push(OsString::from("-C"));
        self.args.push(OsString::from(format!("opt-level={level}")));
        self
    }

    pub fn debug_info(&mut self, on: bool) -> &mut Self {
        self.args.push(OsString::from("-C"));
        self.args.push(OsString::from(if on {
            "debuginfo=2"
        } else {
            "debuginfo=0"
        }));
        self
    }

    pub fn lto(&mut self, mode: &str) -> &mut Self {
        self.args.push(OsString::from("-C"));
        self.args.push(OsString::from(format!("lto={mode}")));
        self
    }

    pub fn linker_script(&mut self, path: &Path) -> &mut Self {
        self.args.push(OsString::from("-C"));
        let mut s = OsString::from("link-arg=-T");
        s.push(path.as_os_str());
        self.args.push(s);
        self
    }

    /// One-shot: calling more than once will cause rustc to reject the
    /// invocation.
    pub fn incremental(&mut self, dir: &Path) -> &mut Self {
        self.args.push(OsString::from("-C"));
        let mut s = OsString::from("incremental=");
        s.push(dir.as_os_str());
        self.args.push(s);
        self
    }

    pub fn raw_arg(&mut self, arg: impl Into<OsString>) -> &mut Self {
        self.args.push(arg.into());
        self
    }

    /// Add an environment variable to the spawned command. Values set here
    /// are merged on top of the inherited parent environment when the
    /// resulting [`std::process::Command`] is spawned.
    pub fn env(&mut self, key: impl Into<OsString>, val: impl Into<OsString>) -> &mut Self {
        self.env.insert(key.into(), val.into());
        self
    }

    pub fn cwd(&mut self, dir: impl Into<PathBuf>) -> &mut Self {
        self.cwd = Some(dir.into());
        self
    }

    /// Borrow the canonical arg list. Useful for tests and diagnostics that
    /// want to assert on the exact tokens that will be passed to rustc.
    pub fn args(&self) -> &[OsString] {
        &self.args
    }

    pub fn rustc_path(&self) -> &Path {
        &self.rustc
    }

    /// Materialise a [`std::process::Command`] ready to spawn.
    ///
    /// `env()` values are merged on top of the inherited environment when
    /// the command is spawned — gluon does not strip the parent environment.
    pub fn into_command(self) -> std::process::Command {
        let mut cmd = std::process::Command::new(&self.rustc);
        cmd.args(&self.args);
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        cmd
    }

    /// Stable SHA-256 over the rustc path, canonical argv, env, and cwd.
    ///
    /// This is the cache key for "has this exact invocation already been
    /// run?". The hash is domain-separated with a `gluon.rustc.v1` prefix
    /// so it cannot collide with any other SHA-256 in the build system.
    /// Every variable-length input is length-prefixed to defeat
    /// concatenation ambiguity (so `"foo" + "bar"` and `"foob" + "ar"`
    /// hash to different values).
    ///
    /// The hash is stable across runs on the same host platform only. The
    /// `OsString` encoding differs between Unix (raw bytes) and Windows
    /// (WTF-8), so cross-platform hash equality only holds for pure-ASCII
    /// inputs — a Unix/Windows shared build cache is not supported today.
    /// See the module-level docs for the full caveat.
    pub fn hash(&self) -> [u8; 32] {
        // Delegate to the canonical implementation in `cache::hash`. The
        // algorithm was moved there in chunk A3 so both the cache layer
        // and the builder share one byte-for-byte identical digest.
        crate::cache::hash::hash_argv(&self.rustc, &self.args, &self.env, self.cwd.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn b(rustc: &str) -> RustcCommandBuilder {
        RustcCommandBuilder::new(PathBuf::from(rustc))
    }

    fn os(s: &str) -> OsString {
        OsString::from(s)
    }

    #[test]
    fn input_appends_positional_path() {
        let mut bld = b("/usr/bin/rustc");
        bld.input(&PathBuf::from("/tmp/foo.rs"));
        assert_eq!(bld.args(), &[os("/tmp/foo.rs")]);
    }

    #[test]
    fn crate_name_emits_two_tokens() {
        let mut bld = b("/usr/bin/rustc");
        bld.crate_name("kernel");
        assert_eq!(bld.args(), &[os("--crate-name"), os("kernel")]);
    }

    #[test]
    fn crate_type_uses_as_str_for_each_variant() {
        for (kind, expected) in [
            (CrateType::Lib, "lib"),
            (CrateType::Bin, "bin"),
            (CrateType::ProcMacro, "proc-macro"),
            (CrateType::StaticLib, "staticlib"),
        ] {
            let mut bld = b("/usr/bin/rustc");
            bld.crate_type(kind);
            assert_eq!(bld.args(), &[os("--crate-type"), os(expected)]);
        }
    }

    #[test]
    fn edition_is_single_equals_token() {
        let mut bld = b("/usr/bin/rustc");
        bld.edition("2024");
        assert_eq!(bld.args(), &[os("--edition=2024")]);
    }

    #[test]
    fn target_builtin_is_single_token() {
        let mut bld = b("/usr/bin/rustc");
        bld.target("x86_64-unknown-none", true);
        assert_eq!(bld.args(), &[os("--target=x86_64-unknown-none")]);
    }

    #[test]
    fn target_custom_spec_is_two_tokens() {
        let mut bld = b("/usr/bin/rustc");
        bld.target("/tmp/specs/custom.json", false);
        assert_eq!(bld.args(), &[os("--target"), os("/tmp/specs/custom.json")]);
    }

    #[test]
    fn sysroot_out_dir_and_paths() {
        let mut bld = b("/usr/bin/rustc");
        bld.sysroot(&PathBuf::from("/tmp/sysroot"))
            .out_dir(&PathBuf::from("/tmp/out"));
        assert_eq!(
            bld.args(),
            &[
                os("--sysroot"),
                os("/tmp/sysroot"),
                os("--out-dir"),
                os("/tmp/out"),
            ]
        );
    }

    #[test]
    fn emit_comma_joins_in_slice_order() {
        let mut bld = b("/usr/bin/rustc");
        bld.emit(&[Emit::Link, Emit::DepInfo, Emit::Metadata]);
        assert_eq!(bld.args(), &[os("--emit=link,dep-info,metadata")]);
    }

    #[test]
    fn emit_with_dep_info_path_inlines_path_into_single_token() {
        let mut bld = b("/usr/bin/rustc");
        bld.emit_with_dep_info_path(
            &[Emit::Link, Emit::Metadata, Emit::DepInfo],
            Path::new("/tmp/out/crate.d"),
        );
        assert_eq!(
            bld.args(),
            &[os("--emit=link,metadata,dep-info=/tmp/out/crate.d")]
        );
    }

    #[test]
    #[should_panic(expected = "emit_with_dep_info_path called without Emit::DepInfo")]
    fn emit_with_dep_info_path_requires_dep_info_kind() {
        let mut bld = b("/usr/bin/rustc");
        bld.emit_with_dep_info_path(&[Emit::Link, Emit::Metadata], Path::new("/tmp/foo.d"));
    }

    #[test]
    fn extern_crate_concatenates_and_preserves_order() {
        let mut bld = b("/usr/bin/rustc");
        bld.extern_crate("core", &PathBuf::from("/tmp/libcore.rlib"))
            .extern_crate("alloc", &PathBuf::from("/tmp/liballoc.rlib"));
        assert_eq!(
            bld.args(),
            &[
                os("--extern"),
                os("core=/tmp/libcore.rlib"),
                os("--extern"),
                os("alloc=/tmp/liballoc.rlib"),
            ]
        );
    }

    #[test]
    fn codegen_setters_emit_dash_c_pairs() {
        let mut bld = b("/usr/bin/rustc");
        bld.opt_level(3)
            .debug_info(true)
            .debug_info(false)
            .lto("fat")
            .linker_script(&PathBuf::from("/tmp/link.ld"))
            .incremental(&PathBuf::from("/tmp/inc"));
        assert_eq!(
            bld.args(),
            &[
                os("-C"),
                os("opt-level=3"),
                os("-C"),
                os("debuginfo=2"),
                os("-C"),
                os("debuginfo=0"),
                os("-C"),
                os("lto=fat"),
                os("-C"),
                os("link-arg=-T/tmp/link.ld"),
                os("-C"),
                os("incremental=/tmp/inc"),
            ]
        );
    }

    #[test]
    fn cfg_and_raw_arg_appended_in_call_order() {
        let mut bld = b("/usr/bin/rustc");
        bld.cfg("feature=\"foo\"")
            .raw_arg("-Zsome-flag")
            .cfg("target_os=\"none\"");
        assert_eq!(
            bld.args(),
            &[
                os("--cfg"),
                os("feature=\"foo\""),
                os("-Zsome-flag"),
                os("--cfg"),
                os("target_os=\"none\""),
            ]
        );
    }

    #[test]
    fn hash_is_stable_for_identical_builders() {
        let build = || {
            let mut bld = b("/usr/bin/rustc");
            bld.crate_name("k")
                .crate_type(CrateType::Lib)
                .edition("2024")
                .target("x86_64-unknown-none", true)
                .cfg("foo")
                .env("CARGO", "/bin/cargo")
                .cwd("/tmp");
            bld
        };
        assert_eq!(build().hash(), build().hash());
    }

    #[test]
    fn hash_changes_with_each_input_dimension() {
        let base = || {
            let mut bld = b("/usr/bin/rustc");
            bld.crate_name("k").env("A", "1").cwd("/tmp");
            bld
        };
        let baseline = base().hash();

        // Different rustc path.
        let mut alt_path = b("/opt/rust/bin/rustc");
        alt_path.crate_name("k").env("A", "1").cwd("/tmp");
        assert_ne!(alt_path.hash(), baseline);

        // Different arg.
        let mut alt_arg = base();
        alt_arg.cfg("foo");
        assert_ne!(alt_arg.hash(), baseline);

        // Different env value.
        let mut alt_env = b("/usr/bin/rustc");
        alt_env.crate_name("k").env("A", "2").cwd("/tmp");
        assert_ne!(alt_env.hash(), baseline);

        // Different cwd.
        let mut alt_cwd = b("/usr/bin/rustc");
        alt_cwd.crate_name("k").env("A", "1").cwd("/other");
        assert_ne!(alt_cwd.hash(), baseline);

        // No cwd at all (Some vs None must differ thanks to the tag byte).
        let mut no_cwd = b("/usr/bin/rustc");
        no_cwd.crate_name("k").env("A", "1");
        assert_ne!(no_cwd.hash(), baseline);
    }

    #[test]
    fn hash_env_order_independent() {
        let mut a = b("/usr/bin/rustc");
        a.env("A", "1").env("B", "2");
        let mut b2 = b("/usr/bin/rustc");
        b2.env("B", "2").env("A", "1");
        assert_eq!(a.hash(), b2.hash());
    }

    #[test]
    fn into_command_round_trip() {
        let mut bld = b("/usr/bin/rustc");
        bld.crate_name("k")
            .crate_type(CrateType::Lib)
            .input(&PathBuf::from("/tmp/lib.rs"))
            .env("RUST_LOG", "info")
            .cwd("/tmp");
        let arg_count = bld.args().len();
        let cmd = bld.into_command();
        assert_eq!(cmd.get_program(), OsStr::new("/usr/bin/rustc"));
        assert_eq!(cmd.get_args().count(), arg_count);
    }

    #[test]
    fn hash_length_prefix_collision_resistance() {
        let mut a = b("/usr/bin/rustc");
        a.raw_arg("foo").raw_arg("bar");
        let mut c = b("/usr/bin/rustc");
        c.raw_arg("foobar");
        assert_ne!(a.hash(), c.hash());
    }

    #[test]
    #[should_panic(expected = "extern crate name must not contain '='")]
    fn extern_crate_debug_asserts_on_equals_in_name() {
        let mut bld = b("/usr/bin/rustc");
        bld.extern_crate("bad=name", Path::new("/tmp/lib.rlib"));
    }

    /// Locks in the chunk A3 migration: `RustcCommandBuilder::hash()` must
    /// produce byte-for-byte the same digest as calling
    /// `cache::hash::hash_argv` directly with the builder's fields. If
    /// this ever drifts, every on-disk cache would silently invalidate.
    #[test]
    fn hash_matches_cache_hash_argv_delegation() {
        let mut bld = b("/usr/bin/rustc");
        bld.crate_name("k")
            .crate_type(CrateType::Lib)
            .edition("2024")
            .target("x86_64-unknown-none", true)
            .cfg("foo")
            .env("CARGO", "/bin/cargo")
            .cwd("/tmp");

        let builder_hash = bld.hash();

        // Mirror the builder's fields into the free function. We pull
        // them out of the builder via its accessors where possible; the
        // rest we reconstruct exactly as the setters above did.
        let mut env: BTreeMap<OsString, OsString> = BTreeMap::new();
        env.insert(OsString::from("CARGO"), OsString::from("/bin/cargo"));
        let direct_hash = crate::cache::hash::hash_argv(
            bld.rustc_path(),
            bld.args(),
            &env,
            Some(Path::new("/tmp")),
        );
        assert_eq!(builder_hash, direct_hash);
    }

    #[test]
    fn rustc_path_accessor_returns_input() {
        let bld = b("/opt/rust/bin/rustc");
        assert_eq!(bld.rustc_path(), Path::new("/opt/rust/bin/rustc"));
    }
}
