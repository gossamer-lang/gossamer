//! Rendering for [`Diagnostic`] into a rustc/elm-style text frame.
//! Output goes through [`render`] by default. Tests and machine
//! consumers can use [`render_plain`] for a colour-free form that is
//! stable across runs.

use std::fmt::Write;

use gossamer_lex::{FileId, SourceMap};

use crate::{Diagnostic, Label};

/// Style options for [`render`]. Kept small on purpose - callers that
/// want colour should opt in explicitly.
#[derive(Debug, Clone, Copy, Default)]
pub struct RenderOptions {
    /// Emit ANSI colour escapes.
    pub colour: bool,
}

/// Renders `diag` as an `error[GP0001]: …` frame using the supplied
/// source map for resolving file names and line/column info.
#[must_use]
pub fn render(diag: &Diagnostic, map: &SourceMap, options: RenderOptions) -> String {
    let mut out = String::new();
    let severity_colour = if options.colour {
        colour_for(diag.severity)
    } else {
        ""
    };
    let bold_colour = if options.colour { BOLD } else { "" };
    let dim_colour = if options.colour { DIM } else { "" };
    let reset = if options.colour { RESET } else { "" };
    let _ = writeln!(
        out,
        "{severity_colour}{}[{}]{reset}{bold_colour}: {}{reset}",
        diag.severity, diag.code, diag.title,
    );
    for label in &diag.labels {
        render_label(&mut out, label, map, &diag.title, options.colour);
    }
    for note in &diag.notes {
        let _ = writeln!(out, "  {dim_colour}= note:{reset} {note}");
    }
    for help in &diag.helps {
        let _ = writeln!(out, "  {dim_colour}= help:{reset} {help}");
    }
    for suggestion in &diag.suggestions {
        // A message that already quotes its replacement is complete on its
        // own; appending the arrow would print the same text twice.
        if suggestion
            .message
            .contains(&format!("`{}`", suggestion.replacement))
        {
            let _ = writeln!(
                out,
                "  {dim_colour}= suggestion:{reset} {}",
                suggestion.message
            );
        } else {
            let _ = writeln!(
                out,
                "  {dim_colour}= suggestion:{reset} {} → `{}`",
                suggestion.message, suggestion.replacement
            );
        }
    }
    out
}

/// Colour-free one-line form for tests and JSON consumers.
#[must_use]
pub fn render_plain(diag: &Diagnostic) -> String {
    let mut out = format!("{}[{}]: {}", diag.severity, diag.code, diag.title);
    if let Some(primary) = diag.primary_label()
        && let Some(msg) = &primary.message
    {
        out.push_str(" - ");
        out.push_str(msg);
    }
    out
}

/// Renders `diag` as a single-line JSON object, terminated by `\n`.
///
/// Stable schema (versioned at `1`):
///
/// ```json
/// {
///   "schema": 1,
///   "code": "GP0016",
///   "severity": "error",
///   "title": "the `extern` keyword is reserved",
///   "labels": [
///     {"file":"foo.gos","line":1,"column":1,"span_start":0,"span_end":6,"primary":true,"message":"..."}
///   ],
///   "notes": ["..."],
///   "helps": ["..."],
///   "suggestions": [
///     {"file":"foo.gos","line":1,"column":1,"span_start":0,"span_end":6,"message":"...","replacement":"..."}
///   ]
/// }
/// ```
///
/// The shape is intentionally flat per label / suggestion so a
/// consumer can stream individual diagnostics through `jq` or
/// equivalent without paged parsing. The hand-rolled emitter
/// avoids a `serde_json` dependency; the field set is closed and
/// stable so the cost is bounded.
#[must_use]
pub fn render_json(diag: &Diagnostic, map: &SourceMap) -> String {
    let mut out = String::new();
    out.push_str("{\"schema\":1,\"code\":");
    push_json_string(&mut out, diag.code.as_str());
    out.push_str(",\"severity\":");
    push_json_string(&mut out, diag.severity.tag());
    out.push_str(",\"title\":");
    push_json_string(&mut out, &diag.title);
    out.push_str(",\"labels\":[");
    for (i, label) in diag.labels.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let (path, line, column) = resolve(map, label.location.file, label.location.span);
        out.push_str("{\"file\":");
        push_json_string(&mut out, &path);
        out.push_str(",\"line\":");
        out.push_str(&line.to_string());
        out.push_str(",\"column\":");
        out.push_str(&column.to_string());
        out.push_str(",\"span_start\":");
        out.push_str(&label.location.span.start.to_string());
        out.push_str(",\"span_end\":");
        out.push_str(&label.location.span.end.to_string());
        out.push_str(",\"primary\":");
        out.push_str(if label.primary { "true" } else { "false" });
        out.push_str(",\"message\":");
        match &label.message {
            Some(msg) => push_json_string(&mut out, msg),
            None => out.push_str("null"),
        }
        out.push('}');
    }
    out.push_str("],\"notes\":[");
    for (i, note) in diag.notes.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        push_json_string(&mut out, note);
    }
    out.push_str("],\"helps\":[");
    for (i, help) in diag.helps.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        push_json_string(&mut out, help);
    }
    out.push_str("],\"suggestions\":[");
    for (i, suggestion) in diag.suggestions.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let (path, line, column) = resolve(map, suggestion.location.file, suggestion.location.span);
        out.push_str("{\"file\":");
        push_json_string(&mut out, &path);
        out.push_str(",\"line\":");
        out.push_str(&line.to_string());
        out.push_str(",\"column\":");
        out.push_str(&column.to_string());
        out.push_str(",\"span_start\":");
        out.push_str(&suggestion.location.span.start.to_string());
        out.push_str(",\"span_end\":");
        out.push_str(&suggestion.location.span.end.to_string());
        out.push_str(",\"message\":");
        push_json_string(&mut out, &suggestion.message);
        out.push_str(",\"replacement\":");
        push_json_string(&mut out, &suggestion.replacement);
        out.push('}');
    }
    out.push_str("]}\n");
    out
}

/// Pushes `value` onto `buf` as a JSON-escaped double-quoted
/// string. Supports the minimum set of escapes required by
/// RFC 8259: `\"`, `\\`, `\b`, `\f`, `\n`, `\r`, `\t`, and
/// `\u00XX` for the remaining control characters.
fn push_json_string(buf: &mut String, value: &str) {
    buf.push('"');
    for ch in value.chars() {
        match ch {
            '"' => buf.push_str("\\\""),
            '\\' => buf.push_str("\\\\"),
            '\u{08}' => buf.push_str("\\b"),
            '\u{0c}' => buf.push_str("\\f"),
            '\n' => buf.push_str("\\n"),
            '\r' => buf.push_str("\\r"),
            '\t' => buf.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write as _;
                let _ = write!(buf, "\\u{:04x}", c as u32);
            }
            c => buf.push(c),
        }
    }
    buf.push('"');
}

fn render_label(
    out: &mut String,
    label: &Label,
    map: &SourceMap,
    header_title: &str,
    colour: bool,
) {
    let (origin, _, line_col) = resolve_in_origin(map, label.location.file, label.location.span);
    let (path, line, column) = (
        map.file_name(origin).to_string(),
        line_col.line,
        line_col.column,
    );
    let prefix = if label.primary { "-->" } else { "::>" };
    let cyan = if colour { CYAN } else { "" };
    let red = if colour { RED } else { "" };
    let dim = if colour { DIM } else { "" };
    let reset = if colour { RESET } else { "" };
    let _ = writeln!(out, "  {cyan}{prefix}{reset} {path}:{line}:{column}");
    if let Some(source_line) = source_line_of(map, origin, line) {
        let gutter = format!("{line:>4}");
        let _ = writeln!(out, "  {dim}{gutter} |{reset} {source_line}");
        let padding = " ".repeat(column.saturating_sub(1) as usize);
        let span_len = label
            .location
            .span
            .end
            .saturating_sub(label.location.span.start)
            .max(1) as usize;
        // A span may run past the line under it - an `if / else if` chain or a
        // block-bodied match arm covers several lines. The caret marks what
        // the reader can see, so it stops at the end of the printed line.
        let visible = source_line_of(map, origin, line)
            .map_or(span_len, |text| {
                text.chars()
                    .count()
                    .saturating_sub(column.saturating_sub(1) as usize)
                    .max(1)
            })
            .min(span_len);
        let caret = "^".repeat(visible);
        let caret_colour = if label.primary { red } else { cyan };
        let _ = writeln!(
            out,
            "       {dim}|{reset} {padding}{caret_colour}{caret}{reset}",
        );
    }
    if let Some(msg) = &label.message {
        let is_redundant = label.primary && msg == header_title;
        if !is_redundant {
            let tag = if label.primary { "error" } else { "note" };
            let tag_colour = if label.primary { red } else { cyan };
            let _ = writeln!(out, "     {tag_colour}{tag}{reset}: {msg}");
        }
    }
}

fn source_line_of(map: &SourceMap, file: FileId, line: u32) -> Option<String> {
    if line == 0 {
        return None;
    }
    let source = map.source(file);
    source
        .lines()
        .nth(line.saturating_sub(1) as usize)
        .map(std::string::ToString::to_string)
}

/// Location a span points at, reported against the file the bytes were
/// read from rather than the assembled compilation unit they were
/// checked in.
fn resolve(map: &SourceMap, file: FileId, span: gossamer_lex::Span) -> (String, u32, u32) {
    let (origin, _, line_col) = resolve_in_origin(map, file, span);
    (
        map.file_name(origin).to_string(),
        line_col.line,
        line_col.column,
    )
}

fn resolve_in_origin(
    map: &SourceMap,
    file: FileId,
    span: gossamer_lex::Span,
) -> (FileId, u32, gossamer_lex::LineCol) {
    let (origin, offset) = map.origin_of(file, span.start);
    (origin, offset, map.line_col(origin, offset))
}

const RESET: &str = "\x1b[0m";
const RED: &str = "\x1b[31m";
const CYAN: &str = "\x1b[36m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";

const fn colour_for(severity: crate::Severity) -> &'static str {
    match severity {
        crate::Severity::Error => "\x1b[31;1m",
        crate::Severity::Warning => "\x1b[33;1m",
        crate::Severity::Note => "\x1b[36m",
        crate::Severity::Help => "\x1b[32m",
    }
}
