//! Per-checkout config option overrides.
//!
//! `gluon.rhai` is the canonical declaration of every config option a
//! project understands, but values can be overridden two ways without
//! editing the script:
//!
//! 1. **Override file** — a key/value file at
//!    `<project_root>/.gluon-config` (or wherever `-C/--config-file`
//!    points). One option per line, `KEY = value` syntax. Useful for
//!    "my dev box always builds with `DEBUG_LOG = true`" without
//!    polluting the committed `gluon.rhai`.
//! 2. **Environment variables** — `GLUON_<NAME>=<value>` in the env at
//!    invocation time. Useful for ad-hoc one-shot overrides
//!    (`GLUON_DEBUG_LOG=true gluon build`) and for CI matrix builds.
//!
//! Both feed [`gluon_model::ConfigValue`] entries into the same
//! `option_overrides` slot consumed by [`crate::config::resolve`]. Env
//! beats file when both supply a key — env is more transient and
//! therefore should be the more "intentional" channel.
//!
//! ### Format
//!
//! The override file is intentionally a tiny grammar, *not* TOML or
//! Rhai. Reasons:
//!
//! - It's per-developer, often hand-edited, and a 30-line file should be
//!   readable at a glance.
//! - The kconfig sub-project (#2.5) will eventually replace this with a
//!   richer parser. Locking in TOML now would create a migration headache.
//! - The values it can express are exactly what `ConfigValue` can hold:
//!   bool, integer, quoted string. No nesting, no arrays — list-typed
//!   options can stay in `gluon.rhai`.
//!
//! Grammar (one entry per line):
//!
//! ```text
//! NAME = value
//! ```
//!
//! - `NAME`: ASCII letters, digits, `_`. Must start with a letter or
//!   `_`. Case-sensitive — matched verbatim against the config option
//!   name in the model.
//! - `value`: one of
//!   - `true` / `false` → `ConfigValue::Bool`
//!   - decimal integer (`0`, `42`, `1234`) → `ConfigValue::U64`
//!   - double-quoted string (`"hello"`) → `ConfigValue::Str` with `\"`
//!     and `\\` escapes
//! - Lines whose first non-whitespace character is `#` are comments.
//! - Blank lines are ignored.
//! - Whitespace around `=` and at line ends is stripped.
//!
//! Anything else is a hard error pointing at the offending line — silent
//! fall-through is exactly the kind of "looks fine but does nothing" bug
//! that costs hours to track down.

use crate::error::{Error, Result};
use gluon_model::ConfigValue;
use std::collections::BTreeMap;
use std::path::Path;

/// Default filename gluon looks for in the project root when no
/// `-C/--config-file` is passed. Picked to match the field already
/// reserved on `ProjectDef::config_override_file` and to mirror cargo's
/// `.cargo/config.toml` convention (a hidden file in the project tree).
pub const DEFAULT_OVERRIDE_FILENAME: &str = ".gluon-config";

/// Default prefix scanned by [`load_env_overrides`]. The CLI passes this
/// directly; tests use it to avoid hard-coding the literal.
pub const DEFAULT_ENV_PREFIX: &str = "GLUON_";

/// Parse an override file at `path`.
///
/// Missing files are *not* an error — they return an empty map. This is
/// the right default because the override file is opt-in: a project
/// without one shouldn't have to acknowledge its absence.
///
/// Returns a `BTreeMap` so iteration order is deterministic, which
/// matters for any downstream consumer that might log overrides or hash
/// them into a cache key.
pub fn load_override_file(path: &Path) -> Result<BTreeMap<String, ConfigValue>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BTreeMap::new());
        }
        Err(e) => {
            return Err(Error::Io {
                path: path.to_path_buf(),
                source: e,
            });
        }
    };

    // The file is human-edited and should be UTF-8. If it isn't, surface
    // the error rather than guessing.
    let text = String::from_utf8(bytes).map_err(|e| {
        Error::Config(format!(
            "override file {} is not valid UTF-8: {e}",
            path.display()
        ))
    })?;

    let mut out = BTreeMap::new();
    for (lineno0, raw) in text.lines().enumerate() {
        let lineno = lineno0 + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = parse_entry(line).map_err(|e| {
            Error::Config(format!(
                "override file {}:{}: {}",
                path.display(),
                lineno,
                e
            ))
        })?;
        if out.insert(key.clone(), value).is_some() {
            return Err(Error::Config(format!(
                "override file {}:{}: duplicate key '{}'",
                path.display(),
                lineno,
                key,
            )));
        }
    }
    Ok(out)
}

/// Scan the process environment for `<prefix><NAME>=<value>` entries.
///
/// Each matching variable becomes one `ConfigValue` entry keyed on
/// `<NAME>` (the prefix is stripped). Values are parsed with the same
/// grammar as the override file: `true`/`false` → bool, decimal integer
/// → u64, anything else → string. Quoting is *not* required for env
/// values — `GLUON_FOO=hello world` becomes `ConfigValue::Str("hello world")`
/// — because shells already handle quoting at the env-var boundary.
pub fn load_env_overrides(prefix: &str) -> BTreeMap<String, ConfigValue> {
    let mut out = BTreeMap::new();
    for (k, v) in std::env::vars() {
        if let Some(name) = k.strip_prefix(prefix) {
            // Skip the prefix-only case (`GLUON_=...`) — that's clearly
            // a typo, not a config option.
            if name.is_empty() {
                continue;
            }
            out.insert(name.to_string(), parse_env_value(&v));
        }
    }
    out
}

/// Combine file and env overrides into a single map. Env wins on
/// conflicts: env is more transient and therefore the more
/// "intentional" channel for the user (they typed it on this exact
/// invocation), while the file is a long-lived default.
pub fn merge_overrides(
    file: BTreeMap<String, ConfigValue>,
    env: BTreeMap<String, ConfigValue>,
) -> BTreeMap<String, ConfigValue> {
    let mut out = file;
    for (k, v) in env {
        out.insert(k, v);
    }
    out
}

// --- internals -------------------------------------------------------------

fn parse_entry(line: &str) -> std::result::Result<(String, ConfigValue), String> {
    let eq = line
        .find('=')
        .ok_or_else(|| "expected `KEY = value` (missing `=`)".to_string())?;
    let key = line[..eq].trim();
    let value = line[eq + 1..].trim();

    if key.is_empty() {
        return Err("empty key before `=`".into());
    }
    validate_key(key)?;

    // Strict-typed parse. We never silently coerce — a typo'd value is
    // exactly what we want to error on.
    let parsed = if value == "true" {
        ConfigValue::Bool(true)
    } else if value == "false" {
        ConfigValue::Bool(false)
    } else if let Some(stripped) = strip_quotes(value) {
        ConfigValue::Str(unescape_string(stripped)?)
    } else if let Ok(n) = value.parse::<u64>() {
        ConfigValue::U64(n)
    } else {
        return Err(format!(
            "value '{value}' is not a recognised override (expected true/false, \
             a non-negative integer, or a double-quoted string)",
        ));
    };

    Ok((key.to_string(), parsed))
}

fn validate_key(key: &str) -> std::result::Result<(), String> {
    let mut chars = key.chars();
    let first = chars.next().expect("validated non-empty above");
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(format!(
            "key '{key}' must start with an ASCII letter or underscore",
        ));
    }
    for c in chars {
        if !(c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!(
                "key '{key}' contains invalid character '{c}' \
                 (only ASCII letters, digits, and `_` allowed)",
            ));
        }
    }
    Ok(())
}

fn strip_quotes(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"' {
        Some(&s[1..s.len() - 1])
    } else {
        None
    }
}

fn unescape_string(s: &str) -> std::result::Result<String, String> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => {
                    return Err(format!("unknown escape sequence `\\{other}` in string"));
                }
                None => return Err("trailing backslash in quoted string".into()),
            }
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

fn parse_env_value(v: &str) -> ConfigValue {
    if v == "true" {
        ConfigValue::Bool(true)
    } else if v == "false" {
        ConfigValue::Bool(false)
    } else if let Ok(n) = v.parse::<u64>() {
        ConfigValue::U64(n)
    } else {
        ConfigValue::Str(v.to_string())
    }
}

// --- tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tmp(content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join(".gluon-config");
        let mut f = std::fs::File::create(&p).expect("create");
        f.write_all(content.as_bytes()).expect("write");
        (tmp, p)
    }

    fn assert_bool(map: &BTreeMap<String, ConfigValue>, k: &str, want: bool) {
        match map.get(k) {
            Some(ConfigValue::Bool(b)) => assert_eq!(*b, want, "key {k}"),
            other => panic!("expected bool for {k}, got {other:?}"),
        }
    }

    fn assert_u64(map: &BTreeMap<String, ConfigValue>, k: &str, want: u64) {
        match map.get(k) {
            Some(ConfigValue::U64(n)) => assert_eq!(*n, want, "key {k}"),
            other => panic!("expected u64 for {k}, got {other:?}"),
        }
    }

    fn assert_str(map: &BTreeMap<String, ConfigValue>, k: &str, want: &str) {
        match map.get(k) {
            Some(ConfigValue::Str(s)) => assert_eq!(s, want, "key {k}"),
            other => panic!("expected string for {k}, got {other:?}"),
        }
    }

    #[test]
    fn missing_file_returns_empty_map() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("nope");
        let map = load_override_file(&p).expect("missing file is OK");
        assert!(map.is_empty());
    }

    #[test]
    fn parses_bool_int_string_and_skips_blanks_and_comments() {
        let (_t, p) = write_tmp(
            r#"
# This is a comment
DEBUG_LOG = true
WORKERS = 4
GREETING = "hi there"

# trailing comment
ENABLED = false
"#,
        );
        let map = load_override_file(&p).expect("parse");
        assert_bool(&map, "DEBUG_LOG", true);
        assert_u64(&map, "WORKERS", 4);
        assert_str(&map, "GREETING", "hi there");
        assert_bool(&map, "ENABLED", false);
        assert_eq!(map.len(), 4);
    }

    #[test]
    fn quoted_string_supports_escapes() {
        let (_t, p) = write_tmp(r#"MSG = "line\none\ttab\"quote\\back""#);
        let map = load_override_file(&p).expect("parse");
        assert_str(&map, "MSG", "line\none\ttab\"quote\\back");
    }

    #[test]
    fn missing_equals_is_error_with_lineno() {
        let (_t, p) = write_tmp("# header\nFOO bar\n");
        let err = load_override_file(&p).expect_err("must error");
        let msg = match err {
            Error::Config(s) => s,
            other => panic!("expected Config error, got {other:?}"),
        };
        assert!(msg.contains(":2:"), "msg should mention line 2: {msg}");
        assert!(msg.contains("missing `=`"), "{msg}");
    }

    #[test]
    fn invalid_value_is_error() {
        let (_t, p) = write_tmp("FOO = bareword\n");
        let err = load_override_file(&p).expect_err("must error");
        let msg = match err {
            Error::Config(s) => s,
            other => panic!("expected Config error, got {other:?}"),
        };
        assert!(msg.contains("not a recognised override"), "{msg}");
    }

    #[test]
    fn invalid_key_is_error() {
        let (_t, p) = write_tmp("9oops = true\n");
        let err = load_override_file(&p).expect_err("must error");
        let msg = match err {
            Error::Config(s) => s,
            other => panic!("expected Config error, got {other:?}"),
        };
        assert!(msg.contains("must start with"), "{msg}");
    }

    #[test]
    fn duplicate_key_is_error() {
        let (_t, p) = write_tmp("FOO = true\nFOO = false\n");
        let err = load_override_file(&p).expect_err("must error");
        let msg = match err {
            Error::Config(s) => s,
            other => panic!("expected Config error, got {other:?}"),
        };
        assert!(msg.contains("duplicate key"), "{msg}");
    }

    #[test]
    fn env_overrides_picks_up_prefixed_vars_only() {
        // We can't safely set process-wide env vars in a parallel test
        // run without isolation. Use a unique prefix per test instead so
        // we don't see leakage from neighbours.
        let prefix = "GLUON_TEST_OV_";
        // SAFETY: setting env vars from tests; we use a unique prefix.
        unsafe {
            std::env::set_var("GLUON_TEST_OV_FLAG", "true");
            std::env::set_var("GLUON_TEST_OV_NUM", "42");
            std::env::set_var("GLUON_TEST_OV_NAME", "kernel");
            std::env::set_var("UNRELATED_FLAG", "true");
        }

        let map = load_env_overrides(prefix);
        assert_bool(&map, "FLAG", true);
        assert_u64(&map, "NUM", 42);
        assert_str(&map, "NAME", "kernel");
        assert!(!map.contains_key("UNRELATED_FLAG"));
        assert_eq!(map.len(), 3);

        unsafe {
            std::env::remove_var("GLUON_TEST_OV_FLAG");
            std::env::remove_var("GLUON_TEST_OV_NUM");
            std::env::remove_var("GLUON_TEST_OV_NAME");
            std::env::remove_var("UNRELATED_FLAG");
        }
    }

    #[test]
    fn merge_lets_env_win_over_file() {
        let mut file = BTreeMap::new();
        file.insert("X".into(), ConfigValue::Bool(false));
        file.insert("Y".into(), ConfigValue::U64(1));
        let mut env = BTreeMap::new();
        env.insert("X".into(), ConfigValue::Bool(true));
        env.insert("Z".into(), ConfigValue::Str("hi".into()));

        let merged = merge_overrides(file, env);
        assert_bool(&merged, "X", true); // env wins
        assert_u64(&merged, "Y", 1); // file-only kept
        assert_str(&merged, "Z", "hi"); // env-only added
    }
}
