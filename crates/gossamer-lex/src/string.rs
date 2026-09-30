//! String, character, raw string, and byte literal tokenization helpers.

use crate::cursor::Cursor;
use crate::diagnostic::LexError;
use crate::span::{FileId, Span};
use crate::token::TokenKind;

/// Opening and closing delimiter of a triple-quoted string literal.
pub(crate) const TRIPLE_QUOTE: &str = "\"\"\"";

/// A triple-quoted literal's source text split into its dedented body
/// and the layout facts a formatter needs to re-render it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TripleString {
    /// Content lines with the shared indentation removed, in order.
    /// Escapes are still encoded so a caller decodes them after
    /// dedenting. A literal whose closing delimiter follows the opening
    /// one directly has no content lines at all, which is what keeps a
    /// re-render from inventing one.
    pub lines: Vec<String>,
    /// The whitespace prefix removed from every content line.
    pub indent: String,
    /// `true` when the literal spans more than one physical line.
    pub multiline: bool,
    /// `true` when the closing `"""` sits on a line of its own.
    pub closer_on_own_line: bool,
    /// `true` when non-whitespace text follows the opening `"""`.
    pub opening_line_text: bool,
}

impl TripleString {
    /// The literal's contents with escapes still encoded: the content
    /// lines joined by the newlines that separated them.
    #[must_use]
    pub fn body(&self) -> String {
        self.lines.join("\n")
    }
}

/// Splits a triple-quoted literal's source text, delimiters included,
/// into its dedented body and layout.
///
/// The indentation measure is the longest leading-whitespace prefix
/// shared by every non-blank content line and by the closing
/// delimiter's line when it has one. Measuring the closing line too is
/// what keeps a re-render at a fresh indentation stable under
/// repetition. A whitespace-only line carries no content, so it neither
/// contributes to the measure nor survives into the body.
#[must_use]
pub fn triple_string(raw: &str) -> TripleString {
    let inner = raw
        .strip_prefix(TRIPLE_QUOTE)
        .and_then(|rest| rest.strip_suffix(TRIPLE_QUOTE))
        .unwrap_or(raw);
    let Some((opening, rest)) = inner.split_once('\n') else {
        return TripleString {
            lines: vec![inner.to_string()],
            indent: String::new(),
            multiline: false,
            closer_on_own_line: false,
            opening_line_text: false,
        };
    };
    let opening_line_text = !opening.trim().is_empty();
    let mut lines: Vec<&str> = rest.split('\n').collect();
    let closer_on_own_line = lines.last().is_some_and(|last| last.trim().is_empty());
    let mut measure: Option<&str> = None;
    if closer_on_own_line {
        measure = lines.pop();
    }
    for line in &lines {
        if line.trim().is_empty() {
            continue;
        }
        let lead = &line[..line.len() - line.trim_start().len()];
        measure = Some(match measure {
            Some(current) => shared_prefix(current, lead),
            None => lead,
        });
    }
    let indent = measure.unwrap_or("");
    let dedented = lines
        .iter()
        .map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else {
                line.strip_prefix(indent).unwrap_or(line).to_string()
            }
        })
        .collect();
    TripleString {
        lines: dedented,
        indent: indent.to_string(),
        multiline: true,
        closer_on_own_line,
        opening_line_text,
    }
}

/// Longest leading substring shared by two whitespace runs.
fn shared_prefix<'a>(left: &'a str, right: &str) -> &'a str {
    let mut end = 0usize;
    for ((offset, a), b) in left.char_indices().zip(right.chars()) {
        if a != b {
            break;
        }
        end = offset + a.len_utf8();
    }
    &left[..end]
}

/// Outcome of lexing one of the quoted-literal forms.
pub(crate) struct QuotedOutcome {
    /// Token kind to emit for the literal.
    pub(crate) kind: TokenKind,
    /// Any diagnostics raised while scanning the literal.
    pub(crate) diagnostics: Vec<LexError>,
}

impl QuotedOutcome {
    /// Returns an outcome with no diagnostics attached.
    const fn ok(kind: TokenKind) -> Self {
        Self {
            kind,
            diagnostics: Vec::new(),
        }
    }
}

/// Lexes a double-quoted string literal beginning at the current cursor
/// position (which points at the opening `"`).
pub(crate) fn lex_string(
    cursor: &mut Cursor<'_>,
    file: FileId,
    literal_start: u32,
) -> QuotedOutcome {
    debug_assert_eq!(cursor.peek(), '"');
    cursor.bump();
    let mut diagnostics = Vec::new();
    loop {
        match cursor.peek() {
            '\0' if cursor.is_eof() => {
                diagnostics.push(LexError::UnterminatedString {
                    span: span_to_here(file, literal_start, cursor),
                });
                return QuotedOutcome {
                    kind: TokenKind::StringLit,
                    diagnostics,
                };
            }
            '"' => {
                cursor.bump();
                return QuotedOutcome {
                    kind: TokenKind::StringLit,
                    diagnostics,
                };
            }
            '\\' => consume_escape(cursor, file, &mut diagnostics),
            _ => {
                cursor.bump();
            }
        }
    }
}

/// Lexes a triple-quoted string literal beginning at the opening `"""`.
///
/// The body may span lines and carries the same escape sequences an
/// ordinary string literal does, so the literal ends at the first `"""`
/// that an escape has not already consumed.
pub(crate) fn lex_triple_string(
    cursor: &mut Cursor<'_>,
    file: FileId,
    literal_start: u32,
) -> QuotedOutcome {
    debug_assert!(cursor.rest().starts_with(TRIPLE_QUOTE));
    for _ in 0..TRIPLE_QUOTE.len() {
        cursor.bump();
    }
    let mut diagnostics = Vec::new();
    loop {
        if cursor.is_eof() {
            diagnostics.push(LexError::UnterminatedTripleString {
                span: span_to_here(file, literal_start, cursor),
            });
            return QuotedOutcome {
                kind: TokenKind::TripleStringLit,
                diagnostics,
            };
        }
        if cursor.rest().starts_with(TRIPLE_QUOTE) {
            for _ in 0..TRIPLE_QUOTE.len() {
                cursor.bump();
            }
            return QuotedOutcome {
                kind: TokenKind::TripleStringLit,
                diagnostics,
            };
        }
        if cursor.peek() == '\\' {
            consume_escape(cursor, file, &mut diagnostics);
        } else {
            cursor.bump();
        }
    }
}

/// Lexes an interpolated string literal beginning at its opening `"` or
/// `"""` (the `f` prefix already consumed). A placeholder is skipped whole,
/// so the quotes, braces, and escapes its expression holds are the
/// expression's own; escapes in the literal text are checked as in any
/// string.
pub(crate) fn lex_interpolated_string(
    cursor: &mut Cursor<'_>,
    file: FileId,
    literal_start: u32,
) -> QuotedOutcome {
    let triple = cursor.rest().starts_with(TRIPLE_QUOTE);
    let (kind, delimiter) = if triple {
        (TokenKind::TripleStringLit, TRIPLE_QUOTE)
    } else {
        (TokenKind::StringLit, "\"")
    };
    for _ in 0..delimiter.len() {
        cursor.bump();
    }
    let mut diagnostics = Vec::new();
    loop {
        if cursor.is_eof() {
            diagnostics.push(if triple {
                LexError::UnterminatedTripleString {
                    span: span_to_here(file, literal_start, cursor),
                }
            } else {
                LexError::UnterminatedString {
                    span: span_to_here(file, literal_start, cursor),
                }
            });
            return QuotedOutcome { kind, diagnostics };
        }
        let rest = cursor.rest();
        if rest.starts_with(delimiter) {
            for _ in 0..delimiter.len() {
                cursor.bump();
            }
            return QuotedOutcome { kind, diagnostics };
        }
        if rest.starts_with("{{") || rest.starts_with("}}") {
            cursor.bump();
            cursor.bump();
        } else if rest.starts_with('{') {
            match crate::interpolation::scan_placeholder(rest, triple) {
                Some(placeholder) => {
                    for _ in rest[..placeholder.len].chars() {
                        cursor.bump();
                    }
                }
                None => {
                    cursor.bump();
                }
            }
        } else if cursor.peek() == '\\' {
            consume_escape(cursor, file, &mut diagnostics);
        } else {
            cursor.bump();
        }
    }
}

/// Lexes a raw string literal beginning at `r` or `br` (after any `b`
/// prefix has already been consumed by `lex_ident_or_prefix`).
///
/// Expects the cursor to be positioned at the `r` of `r"..."` /
/// `r#"..."#`.
pub(crate) fn lex_raw_string(
    cursor: &mut Cursor<'_>,
    file: FileId,
    literal_start: u32,
    byte_flavor: bool,
) -> QuotedOutcome {
    debug_assert_eq!(cursor.peek(), 'r');
    cursor.bump();
    let hashes = consume_opening_hashes(cursor);
    if !cursor.bump_if(|character| character == '"') {
        let span = span_to_here(file, literal_start, cursor);
        return QuotedOutcome {
            kind: if byte_flavor {
                TokenKind::RawByteStringLit { hashes }
            } else {
                TokenKind::RawStringLit { hashes }
            },
            diagnostics: vec![LexError::UnterminatedRawString { span }],
        };
    }
    let terminated = consume_raw_body(cursor, hashes);
    let kind = if byte_flavor {
        TokenKind::RawByteStringLit { hashes }
    } else {
        TokenKind::RawStringLit { hashes }
    };
    if terminated {
        QuotedOutcome::ok(kind)
    } else {
        QuotedOutcome {
            kind,
            diagnostics: vec![LexError::UnterminatedRawString {
                span: span_to_here(file, literal_start, cursor),
            }],
        }
    }
}

/// Lexes a character literal (`'x'`) starting at the current cursor.
pub(crate) fn lex_char(cursor: &mut Cursor<'_>, file: FileId, literal_start: u32) -> QuotedOutcome {
    debug_assert_eq!(cursor.peek(), '\'');
    cursor.bump();
    let mut diagnostics = Vec::new();
    let mut char_count = 0usize;
    loop {
        match cursor.peek() {
            '\0' if cursor.is_eof() => {
                diagnostics.push(LexError::UnterminatedChar {
                    span: span_to_here(file, literal_start, cursor),
                });
                return QuotedOutcome {
                    kind: TokenKind::CharLit,
                    diagnostics,
                };
            }
            '\n' => {
                diagnostics.push(LexError::UnterminatedChar {
                    span: span_to_here(file, literal_start, cursor),
                });
                return QuotedOutcome {
                    kind: TokenKind::CharLit,
                    diagnostics,
                };
            }
            '\'' => {
                cursor.bump();
                if char_count != 1 {
                    diagnostics.push(LexError::BadCharLiteralLength {
                        span: span_to_here(file, literal_start, cursor),
                    });
                }
                return QuotedOutcome {
                    kind: TokenKind::CharLit,
                    diagnostics,
                };
            }
            '\\' => {
                consume_escape(cursor, file, &mut diagnostics);
                char_count += 1;
            }
            _ => {
                cursor.bump();
                char_count += 1;
            }
        }
    }
}

/// Lexes a `b'...'` byte literal.
pub(crate) fn lex_byte(cursor: &mut Cursor<'_>, file: FileId, literal_start: u32) -> QuotedOutcome {
    debug_assert_eq!(cursor.peek(), '\'');
    let inner = lex_char(cursor, file, literal_start);
    QuotedOutcome {
        kind: TokenKind::ByteLit,
        diagnostics: inner.diagnostics,
    }
}

/// Lexes a `b"..."` byte string literal.
pub(crate) fn lex_byte_string(
    cursor: &mut Cursor<'_>,
    file: FileId,
    literal_start: u32,
) -> QuotedOutcome {
    debug_assert_eq!(cursor.peek(), '"');
    let inner = lex_string(cursor, file, literal_start);
    QuotedOutcome {
        kind: TokenKind::ByteStringLit,
        diagnostics: inner.diagnostics,
    }
}

/// Consumes opening `#` characters preceding the `"` of a raw string
/// and returns their count, saturating at `u8::MAX`.
fn consume_opening_hashes(cursor: &mut Cursor<'_>) -> u8 {
    let mut hashes: u16 = 0;
    while cursor.peek() == '#' && hashes < u16::from(u8::MAX) {
        cursor.bump();
        hashes += 1;
    }
    u8::try_from(hashes).unwrap_or(u8::MAX)
}

/// Consumes the body of a raw string up to (and including) a closing
/// `"` followed by the expected number of `#` characters.
fn consume_raw_body(cursor: &mut Cursor<'_>, hashes: u8) -> bool {
    while !cursor.is_eof() {
        let character = cursor.peek();
        if character == '"' && lookahead_matches_closing(cursor, hashes) {
            cursor.bump();
            for _ in 0..hashes {
                cursor.bump();
            }
            return true;
        }
        cursor.bump();
    }
    false
}

/// Returns `true` when the cursor sits at `"` followed by `hashes` more
/// `#` characters - the valid raw-string closing sequence.
fn lookahead_matches_closing(cursor: &Cursor<'_>, hashes: u8) -> bool {
    debug_assert_eq!(cursor.peek(), '"');
    let rest = &cursor.rest()[1..];
    rest.bytes().take(hashes as usize).all(|byte| byte == b'#') && rest.len() >= hashes as usize
}

/// Consumes an escape sequence beginning at the current `\`.
///
/// Reports diagnostics for malformed escapes but always advances past
/// the escape so lexing can continue.
fn consume_escape(cursor: &mut Cursor<'_>, file: FileId, diagnostics: &mut Vec<LexError>) {
    let escape_start = cursor.offset();
    debug_assert_eq!(cursor.peek(), '\\');
    cursor.bump();
    match cursor.peek() {
        'n' | 't' | 'r' | '\\' | '\'' | '"' | '0' => {
            cursor.bump();
        }
        'x' => {
            cursor.bump();
            let ok = consume_hex_digits(cursor, 2);
            if !ok {
                diagnostics.push(LexError::BadEscape {
                    span: span_from_offset(file, escape_start, cursor),
                });
            }
        }
        'u' => {
            cursor.bump();
            if !consume_unicode_escape(cursor) {
                diagnostics.push(LexError::BadUnicodeEscape {
                    span: span_from_offset(file, escape_start, cursor),
                });
            }
        }
        _ => {
            cursor.bump();
            diagnostics.push(LexError::BadEscape {
                span: span_from_offset(file, escape_start, cursor),
            });
        }
    }
}

/// Consumes exactly `count` hex digits. Returns `false` if fewer than
/// `count` are available.
fn consume_hex_digits(cursor: &mut Cursor<'_>, count: usize) -> bool {
    for _ in 0..count {
        if !cursor.peek().is_ascii_hexdigit() {
            return false;
        }
        cursor.bump();
    }
    true
}

/// Consumes a `{XXXX}` unicode escape body. Returns `false` on malformed input.
fn consume_unicode_escape(cursor: &mut Cursor<'_>) -> bool {
    if !cursor.bump_if(|character| character == '{') {
        return false;
    }
    let mut digit_count = 0usize;
    while cursor.peek().is_ascii_hexdigit() && digit_count < 6 {
        cursor.bump();
        digit_count += 1;
    }
    if digit_count == 0 {
        return false;
    }
    cursor.bump_if(|character| character == '}')
}

/// Builds a span from `literal_start` to the cursor's current offset.
fn span_to_here(file: FileId, literal_start: u32, cursor: &Cursor<'_>) -> Span {
    let end = u32::try_from(cursor.offset()).unwrap_or(u32::MAX);
    Span::new(file, literal_start, end)
}

/// Builds a span from `start` to the cursor's current offset.
fn span_from_offset(file: FileId, start: usize, cursor: &Cursor<'_>) -> Span {
    let start_u32 = u32::try_from(start).unwrap_or(u32::MAX);
    let end_u32 = u32::try_from(cursor.offset()).unwrap_or(u32::MAX);
    Span::new(file, start_u32, end_u32)
}

#[cfg(test)]
mod tests {
    use super::triple_string;

    #[test]
    fn strips_the_indentation_shared_with_the_closing_delimiter() {
        let raw = "\"\"\"\n    <html>\n        <body>\n    </html>\n    \"\"\"";
        let parsed = triple_string(raw);
        assert_eq!(parsed.body(), "<html>\n    <body>\n</html>");
        assert_eq!(parsed.indent, "    ");
        assert!(parsed.multiline);
        assert!(parsed.closer_on_own_line);
        assert!(!parsed.opening_line_text);
    }

    #[test]
    fn closing_delimiter_on_its_own_line_drops_the_final_newline() {
        assert_eq!(triple_string("\"\"\"\nabc\n\"\"\"").body(), "abc");
        assert_eq!(triple_string("\"\"\"\nabc\n\n\"\"\"").body(), "abc\n");
    }

    #[test]
    fn an_empty_body_yields_an_empty_value() {
        assert_eq!(triple_string("\"\"\"\n\"\"\"").body(), "");
    }

    #[test]
    fn content_hugging_the_closing_delimiter_keeps_its_own_measure() {
        let parsed = triple_string("\"\"\"\n    abc\"\"\"");
        assert_eq!(parsed.body(), "abc");
        assert_eq!(parsed.indent, "    ");
        assert!(!parsed.closer_on_own_line);
    }

    #[test]
    fn blank_lines_become_empty_and_do_not_lower_the_measure() {
        let raw = "\"\"\"\n    a\n\n  \n    b\n    \"\"\"";
        assert_eq!(triple_string(raw).body(), "a\n\n\nb");
        assert_eq!(triple_string(raw).indent, "    ");
    }

    #[test]
    fn a_line_indented_less_than_the_closer_shortens_the_measure() {
        let raw = "\"\"\"\n  a\n      b\n    \"\"\"";
        assert_eq!(triple_string(raw).body(), "a\n    b");
        assert_eq!(triple_string(raw).indent, "  ");
    }

    #[test]
    fn tabs_and_spaces_never_mix_into_a_shared_measure() {
        let raw = "\"\"\"\n\ta\n    b\n\"\"\"";
        assert_eq!(triple_string(raw).indent, "");
        assert_eq!(triple_string(raw).body(), "\ta\n    b");
    }

    #[test]
    fn a_single_line_literal_is_taken_verbatim() {
        let parsed = triple_string("\"\"\"abc\"\"\"");
        assert_eq!(parsed.body(), "abc");
        assert!(!parsed.multiline);
        assert!(!parsed.closer_on_own_line);
    }

    #[test]
    fn text_after_the_opening_delimiter_is_flagged() {
        assert!(triple_string("\"\"\"oops\n    a\n    \"\"\"").opening_line_text);
        assert!(!triple_string("\"\"\"   \n    a\n    \"\"\"").opening_line_text);
    }

    #[test]
    fn an_escaped_newline_sequence_is_not_a_line_break() {
        let parsed = triple_string("\"\"\"\n    a\\nb\n    \"\"\"");
        assert_eq!(parsed.body(), "a\\nb");
    }

    #[test]
    fn trailing_whitespace_on_a_content_line_survives() {
        let parsed = triple_string("\"\"\"\n    a  \n    \"\"\"");
        assert_eq!(parsed.body(), "a  ");
    }
}
