use gluon_model::SourceSpan;
use std::path::PathBuf;
use thiserror::Error;

/// Severity level of a diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Error,
    Warning,
    Note,
}

/// A single diagnostic produced somewhere in the gluon pipeline.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub level: Level,
    pub message: String,
    pub span: Option<SourceSpan>,
    /// Secondary "note: ..." lines rendered beneath the primary message.
    pub notes: Vec<String>,
}

impl Diagnostic {
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            level: Level::Error,
            message: message.into(),
            span: None,
            notes: Vec::new(),
        }
    }

    pub fn warning(message: impl Into<String>) -> Self {
        Self {
            level: Level::Warning,
            message: message.into(),
            span: None,
            notes: Vec::new(),
        }
    }

    pub fn with_span(mut self, span: SourceSpan) -> Self {
        self.span = Some(span);
        self
    }

    /// Attach a span if `Some`, otherwise leave the diagnostic unchanged.
    ///
    /// Useful when the caller has an `Option<SourceSpan>` field (as most
    /// model items do) and wants to forward it without a manual `match`.
    pub fn with_optional_span(mut self, span: Option<SourceSpan>) -> Self {
        if let Some(s) = span {
            self.span = Some(s);
        }
        self
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }
}

/// Simple `file:line:col: level: msg` renderer. A richer renderer (e.g.
/// `ariadne` or `codespan`) can be slotted in later.
impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let level = match self.level {
            Level::Error => "error",
            Level::Warning => "warning",
            Level::Note => "note",
        };
        if let Some(span) = &self.span {
            write!(f, "{}: {}: {}", span, level, self.message)?;
        } else {
            write!(f, "{}: {}", level, self.message)?;
        }
        for note in &self.notes {
            write!(f, "\n    note: {}", note)?;
        }
        Ok(())
    }
}

/// Top-level error type for `gluon-core`.
#[derive(Debug, Error)]
pub enum Error {
    #[error("script error: {0}")]
    Script(String),

    #[error("{}", render_diagnostics(.0))]
    Diagnostics(Vec<Diagnostic>),

    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("configuration error: {0}")]
    Config(String),

    #[error("compile error: {0}")]
    Compile(String),

    #[error("profile '{profile}' has no boot_binary set — required by `gluon run`")]
    NoBootBinary { profile: String },

    #[error(
        "QEMU binary '{binary}' not found on PATH — install QEMU or set a different binary via `qemu(\"<path>\")`"
    )]
    QemuBinaryNotFound { binary: String },

    #[error("no OVMF firmware found.\n{attempts}")]
    OvmfNotFound { attempts: String },

    #[error("ESP source path does not exist: {path}")]
    EspMissing { path: PathBuf },

    #[error("QEMU run exceeded timeout and was killed")]
    QemuTimeout,

    #[error("failed to spawn QEMU binary '{binary}': {source}")]
    QemuSpawnFailed {
        binary: String,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "cannot pick a default QEMU binary for target '{triple}': unknown architecture.\nSet one explicitly with `qemu(\"qemu-system-<arch>\")` in your gluon.rhai."
    )]
    UnknownQemuTarget { triple: String },

    #[error("QEMU run killed by signal {signal} (cleanly torn down by gluon)")]
    KilledBySignal { signal: i32 },
}

fn render_diagnostics(diags: &[Diagnostic]) -> String {
    diags
        .iter()
        .map(|d| d.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_error_no_span() {
        let d = Diagnostic::error("msg");
        assert_eq!(d.to_string(), "error: msg");
    }

    #[test]
    fn diagnostic_error_with_span() {
        let d = Diagnostic::error("msg").with_span(SourceSpan::point("foo.rhai", 10, 3));
        assert_eq!(d.to_string(), "foo.rhai:10:3: error: msg");
    }

    #[test]
    fn diagnostic_with_note() {
        let d = Diagnostic::error("msg").with_note("hint");
        assert_eq!(d.to_string(), "error: msg\n    note: hint");
    }

    #[test]
    fn error_diagnostics_joins_with_newlines() {
        let e = Error::Diagnostics(vec![
            Diagnostic::error("first"),
            Diagnostic::warning("second"),
        ]);
        assert_eq!(e.to_string(), "error: first\nwarning: second");
    }
}
