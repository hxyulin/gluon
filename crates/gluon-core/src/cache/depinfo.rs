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
use std::path::{Path, PathBuf};

/// Parse a rustc dep-info file and return the prereq paths from its first
/// target-line.
///
/// See the module docs for the exact subset of Make syntax we recognise.
pub fn parse_depfile(path: &Path) -> Result<Vec<PathBuf>> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::Io {
        path: path.to_path_buf(),
        source: e,
    })?;

    // Fold `\<newline>` continuations. We turn each `\<newline>` into a
    // single space so the resulting string contains one logical line per
    // physical `\n` — which keeps the outer line walker simple.
    //
    // We iterate byte-by-byte to avoid UTF-8 boundary headaches; all the
    // characters we care about (`\`, `\n`, `#`, `:`, space, tab) are ASCII
    // and safe to match on bytes.
    let folded = fold_continuations(&text);

    for (lineno, raw) in folded.lines().enumerate() {
        let line = raw.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // First non-skippable line must be a target-line with `:`.
        let idx = first_unescaped_colon(line).ok_or_else(|| {
            // Truncate to the first 80 chars so a 4 KB single-line
            // depfile doesn't drown the diagnostic.
            let snippet: String = line.chars().take(80).collect();
            Error::Config(format!(
                "malformed depfile {}: malformed target-line at line {}: \
                 missing `:` separator (line starts with: {:?})",
                path.display(),
                lineno + 1,
                snippet,
            ))
        })?;
        let prereqs = &line[idx + 1..];
        return tokenise_prereqs(prereqs, path, lineno + 1);
    }

    Err(Error::Config(format!(
        "malformed depfile {}: no target-line found",
        path.display()
    )))
}

// TODO(session-B): this parser treats depfile content as ASCII. `read_to_string`
// already rejects non-UTF-8 input, but a UTF-8 depfile with non-ASCII path
// components would be corrupted by the per-byte `b as char` conversion here.
// Fix by tracking byte offsets through the original &[u8] and building
// PathBufs via OsString::from_vec on unix.
fn fold_continuations(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
            out.push(' ');
            i += 2;
            continue;
        }
        // Handle `\r\n` continuations too, just to be forgiving of
        // CRLF-normalised checkouts. Not strictly needed for rustc.
        if b == b'\\' && i + 2 < bytes.len() && bytes[i + 1] == b'\r' && bytes[i + 2] == b'\n' {
            out.push(' ');
            i += 3;
            continue;
        }
        out.push(b as char);
        i += 1;
    }
    out
}

/// Return the index of the first `:` in `line` that is not preceded by an
/// unescaped backslash. Backslashes can legitimately escape `:` in Make
/// syntax, though rustc itself doesn't emit escaped colons today.
fn first_unescaped_colon(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if i + 1 < bytes.len() => {
                // Skip the escape and the escaped byte.
                i += 2;
            }
            b':' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// Tokenise a prereq string: split on unescaped whitespace, decoding `\ `
/// into a literal space.
fn tokenise_prereqs(s: &str, depfile: &Path, lineno: usize) -> Result<Vec<PathBuf>> {
    let bytes = s.as_bytes();
    let mut out: Vec<PathBuf> = Vec::new();
    let mut cur = String::new();
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
                    cur.push(' ');
                } else {
                    cur.push(next as char);
                }
                i += 2;
            }
            b' ' | b'\t' => {
                if !cur.is_empty() {
                    out.push(PathBuf::from(std::mem::take(&mut cur)));
                }
                i += 1;
            }
            _ => {
                cur.push(b as char);
                i += 1;
            }
        }
    }
    if !cur.is_empty() {
        out.push(PathBuf::from(cur));
    }
    Ok(out)
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
