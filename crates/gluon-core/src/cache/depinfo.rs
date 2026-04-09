//! Minimal parser for rustc-emitted dep-info files (`--emit=dep-info`).
//!
//! Rustc writes a Makefile-style fragment whose first target-line lists the
//! output followed by every source file it compiled. We only care about that
//! first target-line — the subsequent per-prereq stubs and `# env-dep:`
//! comments are informational and rustc regenerates them deterministically.
//! The parser's job is to extract the prereq list, with three edge cases
//! that rustc actually produces:
//!
//! 1. **Backslash-escaped spaces in paths.** A path containing a literal
//!    space is emitted as `foo\ bar.rs`. We decode `\ ` → ` ` on
//!    tokenisation.
//! 2. **Line continuations.** A trailing `\` immediately before a newline
//!    means the logical line continues on the next physical line. We join
//!    them with a single space before tokenising.
//! 3. **Leading blank / `#`-comment lines.** Rustc doesn't currently emit
//!    them at the top, but the Make-ish format allows them and we skip them
//!    defensively.
//!
//! Anything more exotic (`$(VAR)` expansions, `;` recipes, etc.) is outside
//! the subset rustc emits, and we deliberately don't try to handle it. A
//! malformed depfile is a bug in the toolchain, not in gluon — we surface
//! the failure as [`Error::Config`] so the diagnostic points at the file.

use crate::error::{Error, Result};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Parse a rustc dep-info file and return the prereq paths from its first
/// target-line.
///
/// See the module docs for the exact subset of Make syntax we recognise.
///
/// **Encoding**: the parser operates on raw bytes (`&[u8]`), not on a
/// Rust `String`. On Unix, file system paths are arbitrary byte
/// sequences and may not be valid UTF-8 — a project living under
/// `/home/ülrich/…` would be silently corrupted by an earlier
/// `read_to_string` + `b as char` pipeline. We construct each `PathBuf`
/// via [`OsString::from_vec`](std::os::unix::ffi::OsStringExt::from_vec)
/// on Unix and via UTF-8 validation on Windows (where rustc writes
/// depfiles in UTF-8 and the OS path encoding is UTF-16, so a UTF-8
/// round-trip is the only sensible bridge).
pub fn parse_depfile(path: &Path) -> Result<Vec<PathBuf>> {
    let bytes = std::fs::read(path).map_err(|e| Error::Io {
        path: path.to_path_buf(),
        source: e,
    })?;

    // Fold `\<newline>` continuations. We turn each `\<newline>` into a
    // single space so the resulting buffer contains one logical line per
    // physical `\n` — which keeps the outer line walker simple.
    let folded = fold_continuations(&bytes);

    let mut lineno = 0usize;
    for raw in folded.split(|b| *b == b'\n') {
        lineno += 1;
        let line = trim_leading_ascii_whitespace(raw);
        if line.is_empty() || line[0] == b'#' {
            continue;
        }
        // First non-skippable line must be a target-line with `:`.
        let idx = first_unescaped_colon(line).ok_or_else(|| {
            // Truncate to the first 80 bytes so a 4 KB single-line
            // depfile doesn't drown the diagnostic. We render via
            // `from_utf8_lossy` because this is a *diagnostic*, not a
            // path: we'd rather show garbled text than refuse to print.
            let snippet = String::from_utf8_lossy(&line[..line.len().min(80)]).into_owned();
            Error::Config(format!(
                "malformed depfile {}: malformed target-line at line {}: \
                 missing `:` separator (line starts with: {:?})",
                path.display(),
                lineno,
                snippet,
            ))
        })?;
        let prereqs = &line[idx + 1..];
        return tokenise_prereqs(prereqs, path, lineno);
    }

    Err(Error::Config(format!(
        "malformed depfile {}: no target-line found",
        path.display()
    )))
}

/// Strip leading ` ` and `\t` bytes. We avoid `str::trim_start` because
/// the input here is `&[u8]` that may not be valid UTF-8.
fn trim_leading_ascii_whitespace(line: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < line.len() && (line[start] == b' ' || line[start] == b'\t') {
        start += 1;
    }
    // Trim trailing `\r` so a CRLF file doesn't carry the carriage
    // return into a path token.
    let mut end = line.len();
    if end > start && line[end - 1] == b'\r' {
        end -= 1;
    }
    &line[start..end]
}

fn fold_continuations(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
            out.push(b' ');
            i += 2;
            continue;
        }
        // Handle `\r\n` continuations too, just to be forgiving of
        // CRLF-normalised checkouts. Not strictly needed for rustc.
        if b == b'\\' && i + 2 < bytes.len() && bytes[i + 1] == b'\r' && bytes[i + 2] == b'\n' {
            out.push(b' ');
            i += 3;
            continue;
        }
        out.push(b);
        i += 1;
    }
    out
}

/// Return the index of the first `:` in `line` that is not preceded by an
/// unescaped backslash. Backslashes can legitimately escape `:` in Make
/// syntax, though rustc itself doesn't emit escaped colons today.
fn first_unescaped_colon(line: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i < line.len() {
        match line[i] {
            b'\\' if i + 1 < line.len() => {
                // Skip the escape and the escaped byte.
                i += 2;
            }
            b':' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// Tokenise a prereq byte slice: split on unescaped whitespace, decoding
/// `\ ` into a literal space, and convert each token into a [`PathBuf`]
/// preserving the original bytes verbatim.
fn tokenise_prereqs(bytes: &[u8], depfile: &Path, lineno: usize) -> Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'\\' => {
                if i + 1 >= bytes.len() {
                    return Err(Error::Config(format!(
                        "malformed depfile {}: unterminated backslash escape \
                         at end of prereq list (line {})",
                        depfile.display(),
                        lineno,
                    )));
                }
                let next = bytes[i + 1];
                // `\ ` → space. Any other escape is passed through verbatim
                // (rustc only emits `\ ` today, but be forgiving).
                if next == b' ' {
                    cur.push(b' ');
                } else {
                    cur.push(next);
                }
                i += 2;
            }
            b' ' | b'\t' => {
                if !cur.is_empty() {
                    out.push(bytes_to_pathbuf(std::mem::take(&mut cur), depfile)?);
                }
                i += 1;
            }
            _ => {
                cur.push(b);
                i += 1;
            }
        }
    }
    if !cur.is_empty() {
        out.push(bytes_to_pathbuf(cur, depfile)?);
    }
    Ok(out)
}

/// Convert a raw byte sequence into a `PathBuf` without going through
/// `String`.
///
/// On Unix the OS path encoding is "arbitrary bytes", so we use
/// [`OsString::from_vec`](std::os::unix::ffi::OsStringExt::from_vec)
/// — this preserves bytes that aren't valid UTF-8 (e.g. ISO-8859-1
/// filesystems, mojibake left over from a migration). On Windows the OS
/// path encoding is UTF-16; the rustc-written depfile is UTF-8, so we
/// validate as UTF-8 and let `OsString` perform the WTF-8 → UTF-16
/// bridge. Invalid UTF-8 on Windows is reported as a config error
/// rather than silently corrupted, since silently building from a path
/// that can't be expressed in the OS encoding will fail downstream
/// anyway.
#[cfg(unix)]
fn bytes_to_pathbuf(bytes: Vec<u8>, _depfile: &Path) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

#[cfg(not(unix))]
fn bytes_to_pathbuf(bytes: Vec<u8>, depfile: &Path) -> Result<PathBuf> {
    let s = String::from_utf8(bytes).map_err(|e| {
        Error::Config(format!(
            "malformed depfile {}: prereq path is not valid UTF-8: {e}",
            depfile.display(),
        ))
    })?;
    Ok(PathBuf::from(OsString::from(s)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tmp(content: &str) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("foo.d");
        std::fs::write(&p, content).expect("write");
        (tmp, p)
    }

    #[test]
    fn simple_three_prereqs() {
        let (_tmp, p) = write_tmp("out/foo.rlib: src/a.rs src/b.rs src/c.rs\n");
        let got = parse_depfile(&p).expect("parse");
        assert_eq!(
            got,
            vec![
                PathBuf::from("src/a.rs"),
                PathBuf::from("src/b.rs"),
                PathBuf::from("src/c.rs"),
            ]
        );
    }

    #[test]
    fn escaped_spaces_in_paths() {
        let (_tmp, p) = write_tmp("out/foo.rlib: path/with\\ space/a.rs path/b.rs\n");
        let got = parse_depfile(&p).expect("parse");
        assert_eq!(
            got,
            vec![
                PathBuf::from("path/with space/a.rs"),
                PathBuf::from("path/b.rs"),
            ]
        );
    }

    #[test]
    fn line_continuation_joined() {
        let (_tmp, p) = write_tmp("out/foo.rlib: src/a.rs \\\n src/b.rs src/c.rs\n");
        let got = parse_depfile(&p).expect("parse");
        assert_eq!(
            got,
            vec![
                PathBuf::from("src/a.rs"),
                PathBuf::from("src/b.rs"),
                PathBuf::from("src/c.rs"),
            ]
        );
    }

    #[test]
    fn leading_blank_and_comment_lines_skipped() {
        let (_tmp, p) = write_tmp("\n# comment line\n\nout/foo.rlib: src/a.rs\n");
        let got = parse_depfile(&p).expect("parse");
        assert_eq!(got, vec![PathBuf::from("src/a.rs")]);
    }

    #[test]
    fn per_prereq_stub_lines_ignored() {
        // rustc emits stub `src/a.rs:` lines after the main target-line.
        // We must stop at the first target-line and never see these.
        let content = "out/foo.rlib: src/a.rs src/b.rs\nsrc/a.rs:\nsrc/b.rs:\n";
        let (_tmp, p) = write_tmp(content);
        let got = parse_depfile(&p).expect("parse");
        assert_eq!(
            got,
            vec![PathBuf::from("src/a.rs"), PathBuf::from("src/b.rs")]
        );
    }

    #[test]
    fn missing_file_returns_io_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("nope.d");
        match parse_depfile(&p) {
            Err(Error::Io { path, .. }) => assert_eq!(path, p),
            other => panic!("expected Error::Io, got {:?}", other),
        }
    }

    #[test]
    fn empty_file_returns_config_error() {
        let (_tmp, p) = write_tmp("");
        match parse_depfile(&p) {
            Err(Error::Config(msg)) => assert!(msg.contains("no target-line")),
            other => panic!("expected Error::Config, got {:?}", other),
        }
    }

    #[test]
    fn only_blank_and_comment_lines_returns_config_error() {
        let (_tmp, p) = write_tmp("\n\n# only comments\n#and more\n\n");
        match parse_depfile(&p) {
            Err(Error::Config(msg)) => assert!(msg.contains("no target-line")),
            other => panic!("expected Error::Config, got {:?}", other),
        }
    }

    #[test]
    fn depfile_with_non_ascii_path_round_trips() {
        // A real-world UTF-8 path under a Unicode-named directory must
        // round-trip byte-exactly. Before the byte-clean parser this case
        // got corrupted by a `b as char` cast that truncated multi-byte
        // codepoints to U+00xx.
        let (_tmp, p) = write_tmp("out/foo.rlib: /tmp/ülrich/src/main.rs /tmp/a.rs\n");
        let got = parse_depfile(&p).expect("parse");
        assert_eq!(
            got,
            vec![
                PathBuf::from("/tmp/ülrich/src/main.rs"),
                PathBuf::from("/tmp/a.rs"),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn depfile_with_non_utf8_path_preserved_on_unix() {
        // On Unix, paths are arbitrary bytes; rustc may emit a depfile
        // referencing a path that isn't valid UTF-8 (e.g. an old
        // ISO-8859-1 filesystem). Make sure those bytes survive the
        // parser unchanged so the cache lookup matches the actual file.
        use std::os::unix::ffi::OsStrExt;
        let mut content = Vec::new();
        content.extend_from_slice(b"out/foo.rlib: /tmp/");
        // 0xFF is the canonical "not valid UTF-8" byte.
        content.push(0xFF);
        content.extend_from_slice(b"name/main.rs\n");
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("foo.d");
        std::fs::write(&p, &content).unwrap();

        let got = parse_depfile(&p).expect("parse");
        assert_eq!(got.len(), 1);
        let parsed_bytes = got[0].as_os_str().as_bytes();
        assert_eq!(parsed_bytes, b"/tmp/\xFFname/main.rs");
    }

    #[test]
    fn trailing_backslash_in_prereqs_is_error() {
        // A lone `\` at EOF with no escaped char following is malformed.
        // Note: a trailing `\\\n` would be folded into a space by
        // `fold_continuations`, so we test the no-newline variant.
        let (_tmp, p) = write_tmp("out: a.rs \\");
        match parse_depfile(&p) {
            Err(Error::Config(msg)) => {
                assert!(
                    msg.contains("unterminated backslash escape"),
                    "unexpected message: {msg}"
                );
            }
            other => panic!("expected Error::Config, got {:?}", other),
        }
    }
}
