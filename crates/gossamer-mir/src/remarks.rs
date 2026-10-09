//! Optimisation remarks: why a loop was or was not auto-regioned, and which
//! index checks a body keeps. Each family is opted into by its own variable
//! and written to standard error, one line per site.

use gossamer_lex::Span;

/// The optimisation a remark reports on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemarkKind {
    /// Automatic arena regions (`GOS_ARENA_TRACE`).
    Arena,
    /// Index bounds checks (`GOS_BOUNDS_REMARKS`).
    Bounds,
}

impl RemarkKind {
    fn tag(self) -> &'static str {
        match self {
            Self::Arena => "arena",
            Self::Bounds => "bounds",
        }
    }

    fn variable(self) -> &'static str {
        match self {
            Self::Arena => "GOS_ARENA_TRACE",
            Self::Bounds => "GOS_BOUNDS_REMARKS",
        }
    }

    /// Whether remarks of this family were requested for this process.
    #[must_use]
    pub fn enabled(self) -> bool {
        std::env::var_os(self.variable()).is_some()
    }
}

/// Whether any remark family was requested, which makes a build a report
/// about its own compile rather than a request for an artifact a cache could
/// stand in for.
#[must_use]
pub fn any_enabled() -> bool {
    RemarkKind::Arena.enabled() || RemarkKind::Bounds.enabled()
}

/// Where `span` was written: `file:line:col` through the registered position
/// table, or the raw unit offsets when no table was registered (an in-process
/// JIT compile has none).
#[must_use]
pub fn describe_span(span: Span) -> String {
    match gossamer_lex::source_position(span.start) {
        Some((file, line, column)) => format!("{file}:{line}:{column}"),
        None => format!(
            "file {} bytes {}..{}",
            span.file.as_u32(),
            span.start,
            span.end
        ),
    }
}

/// Writes one remark of `kind` for `span` when that family is enabled.
pub(crate) fn emit(kind: RemarkKind, span: Span, message: impl std::fmt::Display) {
    if kind.enabled() {
        eprintln!("[{}] {}: {message}", kind.tag(), describe_span(span));
    }
}
