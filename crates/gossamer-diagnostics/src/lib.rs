//! Structured diagnostics for the Gossamer compiler.
//! Every front-end crate constructs [`Diagnostic`] values; the CLI
//! renders them through [`render()`] with a rustc/elm-style frame,
//! primary and secondary labels, notes, helps, and optional
//! structured fix-it suggestions the formatter can apply.
//! The shape is deliberately small: severity, stable error code, one
//! summary line, and a list of labels / notes / suggestions.
//! Downstream lint crates compose the same primitives so the CLI
//! prints one uniform diagnostic shape for parse, resolve, type,
//! exhaustiveness, and lint errors.

#![forbid(unsafe_code)]

use std::fmt;

use gossamer_lex::{FileId, Span};

pub mod code_registry;
pub mod render;

pub use code_registry::{REGISTRY, codes, explain};
pub use render::{RenderOptions, render, render_json, render_plain};

/// Severity of a diagnostic. Mirrors the standard four-level scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Severity {
    /// Hard error; compilation cannot produce a usable artifact.
    Error,
    /// Warning; compilation continues but the code is suspicious.
    Warning,
    /// Informational note; used mostly by lints.
    Note,
    /// Help text; attached to another diagnostic.
    Help,
}

impl Severity {
    /// Short textual tag (`error`, `warning`, `note`, `help`).
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Note => "note",
            Self::Help => "help",
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(self.tag())
    }
}

/// Stable, human-searchable identifier for a diagnostic.
///
/// Codes are a four-character prefix plus a four-digit number. The
/// prefix denotes the phase:
/// - `GP` - parser / lexer.
/// - `GR` - name resolution.
/// - `GT` - type checker.
/// - `GM` - match exhaustiveness.
/// - `GL` - lint framework.
/// - `GK` - package manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Code(pub &'static str);

impl Code {
    /// Returns the code's textual form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl serde::Serialize for Code {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0)
    }
}

// A code read back resolves to the registry's own spelling, so only a code
// the registry knows deserializes.
impl<'de> serde::Deserialize<'de> for Code {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let spelled = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        code_registry::codes()
            .find(|code| *code == spelled)
            .map(Code)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown diagnostic code `{spelled}`")))
    }
}

impl fmt::Display for Code {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(self.0)
    }
}

/// Anchor for a label: a file id plus a byte-range span.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Location {
    /// File the label refers to.
    pub file: FileId,
    /// Byte range within that file.
    pub span: Span,
}

impl Location {
    /// Convenience constructor.
    #[must_use]
    pub const fn new(file: FileId, span: Span) -> Self {
        Self { file, span }
    }
}

/// One label attached to a diagnostic.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Label {
    /// Where the label points.
    pub location: Location,
    /// `true` for the diagnostic's primary span.
    pub primary: bool,
    /// Label text, or `None` for bare highlighting.
    pub message: Option<String>,
}

impl Label {
    /// Primary label with the supplied message.
    #[must_use]
    pub fn primary(location: Location, message: impl Into<String>) -> Self {
        Self {
            location,
            primary: true,
            message: Some(message.into()),
        }
    }

    /// Secondary label with the supplied message.
    #[must_use]
    pub fn secondary(location: Location, message: impl Into<String>) -> Self {
        Self {
            location,
            primary: false,
            message: Some(message.into()),
        }
    }
}

/// A structured fix-it the formatter (or `gos lint --fix`) can apply.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Suggestion {
    /// Where the replacement applies.
    pub location: Location,
    /// Human-readable description of the fix.
    pub message: String,
    /// Text that should replace `location`.
    pub replacement: String,
}

impl Suggestion {
    /// Replacement constructor.
    #[must_use]
    pub fn replacement(
        location: Location,
        message: impl Into<String>,
        replacement: impl Into<String>,
    ) -> Self {
        Self {
            location,
            message: message.into(),
            replacement: replacement.into(),
        }
    }
}

/// One diagnostic emitted by the compiler.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Diagnostic {
    /// Severity of the diagnostic.
    pub severity: Severity,
    /// Stable code identifying the diagnostic.
    pub code: Code,
    /// One-line summary.
    pub title: String,
    /// Labels attached to the diagnostic.
    pub labels: Vec<Label>,
    /// Longer explanation bullets (`note: ...`).
    pub notes: Vec<String>,
    /// Inline fix suggestions (`help: ...`).
    pub helps: Vec<String>,
    /// Machine-applicable fixes.
    pub suggestions: Vec<Suggestion>,
}

impl Diagnostic {
    /// Error constructor.
    #[must_use]
    pub fn error(code: Code, title: impl Into<String>) -> Self {
        Self::new(Severity::Error, code, title)
    }

    /// Warning constructor.
    #[must_use]
    pub fn warning(code: Code, title: impl Into<String>) -> Self {
        Self::new(Severity::Warning, code, title)
    }

    /// Note constructor.
    #[must_use]
    pub fn note(code: Code, title: impl Into<String>) -> Self {
        Self::new(Severity::Note, code, title)
    }

    fn new(severity: Severity, code: Code, title: impl Into<String>) -> Self {
        Self {
            severity,
            code,
            title: title.into(),
            labels: Vec::new(),
            notes: Vec::new(),
            helps: Vec::new(),
            suggestions: Vec::new(),
        }
    }

    /// Attaches a label and returns `self` for chaining.
    #[must_use]
    pub fn with_label(mut self, label: Label) -> Self {
        self.labels.push(label);
        self
    }

    /// Attaches a primary label and returns `self` for chaining.
    #[must_use]
    pub fn with_primary(self, location: Location, message: impl Into<String>) -> Self {
        self.with_label(Label::primary(location, message))
    }

    /// Attaches a secondary label and returns `self` for chaining.
    #[must_use]
    pub fn with_secondary(self, location: Location, message: impl Into<String>) -> Self {
        self.with_label(Label::secondary(location, message))
    }

    /// Appends a `note:` line.
    #[must_use]
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }

    /// Appends a `help:` line.
    #[must_use]
    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.helps.push(help.into());
        self
    }

    /// Appends a machine-applicable suggestion.
    #[must_use]
    pub fn with_suggestion(mut self, suggestion: Suggestion) -> Self {
        self.suggestions.push(suggestion);
        self
    }

    /// Returns the first primary label, if any.
    #[must_use]
    pub fn primary_label(&self) -> Option<&Label> {
        self.labels.iter().find(|l| l.primary)
    }
}

/// Collects diagnostics produced by a single compilation pass.
#[derive(Debug, Clone, Default)]
pub struct Sink {
    diagnostics: Vec<Diagnostic>,
}

impl Sink {
    /// Empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a diagnostic.
    pub fn push(&mut self, diagnostic: Diagnostic) {
        self.diagnostics.push(diagnostic);
    }

    /// Drains this sink and returns its contents.
    pub fn drain(&mut self) -> Vec<Diagnostic> {
        std::mem::take(&mut self.diagnostics)
    }

    /// Returns the number of diagnostics stored.
    #[must_use]
    pub fn len(&self) -> usize {
        self.diagnostics.len()
    }

    /// Whether the sink holds no diagnostics.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.diagnostics.is_empty()
    }

    /// Count of errors, ignoring warnings / notes / helps.
    #[must_use]
    pub fn error_count(&self) -> usize {
        self.diagnostics
            .iter()
            .filter(|d| matches!(d.severity, Severity::Error))
            .count()
    }

    /// View the raw slice of diagnostics.
    #[must_use]
    pub fn as_slice(&self) -> &[Diagnostic] {
        &self.diagnostics
    }
}

/// Computes the Levenshtein distance between `a` and `b`. Used by
/// the "did you mean" helper below.
#[must_use]
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut curr: Vec<usize> = vec![0; m + 1];
    for i in 1..=n {
        curr[0] = i;
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[m]
}

/// Returns the best "did you mean" candidate for `target` from
/// `candidates` under the given edit-distance budget.
///
/// A candidate must share more with `target` than it differs: the edit
/// distance is also capped below `target`'s length, so a short or empty
/// target cannot match an unrelated name. A candidate equal to `target`
/// is never suggested, since replacing a name with itself is no fix.
#[must_use]
pub fn suggest<'a, I>(target: &str, candidates: I, max_distance: usize) -> Option<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    let budget = max_distance.min(target.chars().count().saturating_sub(1));
    if budget == 0 {
        return None;
    }
    let mut best: Option<(&str, usize)> = None;
    for candidate in candidates {
        if candidate == target {
            continue;
        }
        let distance = edit_distance(target, candidate);
        if distance > budget {
            continue;
        }
        match best {
            None => best = Some((candidate, distance)),
            Some((_, prev)) if distance < prev => best = Some((candidate, distance)),
            _ => {}
        }
    }
    best.map(|(name, _)| name)
}
