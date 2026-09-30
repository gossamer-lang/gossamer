//! The placeholders of an interpolated string `f"..."`.
//!
//! A placeholder is `{expr}` or `{expr:spec}`, where `expr` is any
//! expression. Its extent follows the expression's own brackets, string and
//! character literals, and `::` path separators, so `{m["k"]}`, `{f(a, b)}`,
//! and `{Type::MAX}` each end at their own closing brace. The first `:` at
//! the expression's top level that is not half of a `::` starts the spec,
//! which runs to the next `}`; an expression that needs a top-level `:` of
//! its own is written in parentheses. In a single-line literal a placeholder
//! ends on its own line, so a `{` left unclosed cannot reach past the
//! literal's closing quote.

use std::ops::Range;

/// One placeholder, with byte ranges relative to its opening `{`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placeholder {
    /// Bytes from the opening `{` through the closing `}`.
    pub len: usize,
    /// The expression's text.
    pub expr: Range<usize>,
    /// The spec's text after the `:`, when there is one.
    pub spec: Option<Range<usize>>,
}

/// A piece of an interpolated string's body, with byte ranges relative to the
/// body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterpolationPiece {
    /// Literal text, its escapes and doubled braces still encoded.
    Text(Range<usize>),
    /// A placeholder, offset to the body.
    Placeholder(Placeholder, usize),
}

/// The placeholder opening at the start of `text`, which begins with `{`;
/// `None` when no closing `}` ends it, on this line unless `multiline`.
#[must_use]
pub fn scan_placeholder(text: &str, multiline: bool) -> Option<Placeholder> {
    debug_assert!(text.starts_with('{'));
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' if !multiline => return None,
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' => depth = depth.saturating_sub(1),
            b'}' if depth == 0 => {
                return Some(Placeholder {
                    len: i + 1,
                    expr: 1..i,
                    spec: None,
                });
            }
            b'}' => depth -= 1,
            b':' if bytes.get(i + 1) == Some(&b':') => i += 1,
            b':' if depth == 0 => {
                let close = i + 1 + text[i + 1..].find('}')?;
                return Some(Placeholder {
                    len: close + 1,
                    expr: 1..i,
                    spec: Some(i + 1..close),
                });
            }
            b'"' => {
                let interpolated = i > 0 && bytes[i - 1] == b'f' && starts_word(bytes, i - 1);
                i = string_end(text, i, interpolated, multiline)?;
                continue;
            }
            b'r' if starts_word(bytes, i) && matches!(bytes.get(i + 1), Some(b'"' | b'#')) => {
                i = raw_string_end(text, i + 1)?;
                continue;
            }
            b'\'' => {
                i = char_literal_end(bytes, i).unwrap_or(i + 1);
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Splits an interpolated string's body - the text between its quotes - into
/// literal text and placeholders. A `{` no `}` closes is left in the text.
/// `multiline` is true for a triple-quoted literal.
#[must_use]
pub fn interpolation_pieces(body: &str, multiline: bool) -> Vec<InterpolationPiece> {
    let bytes = body.as_bytes();
    let mut pieces = Vec::new();
    let mut text_start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i = escape_end(bytes, i),
            b'{' | b'}' if bytes.get(i + 1) == Some(&bytes[i]) => i += 2,
            b'{' => match scan_placeholder(&body[i..], multiline) {
                Some(placeholder) => {
                    if text_start < i {
                        pieces.push(InterpolationPiece::Text(text_start..i));
                    }
                    let start = i;
                    i += placeholder.len;
                    pieces.push(InterpolationPiece::Placeholder(placeholder, start));
                    text_start = i;
                }
                None => i += 1,
            },
            _ => i += 1,
        }
    }
    if text_start < bytes.len() {
        pieces.push(InterpolationPiece::Text(text_start..bytes.len()));
    }
    pieces
}

/// Whether the byte at `at` begins a word rather than continuing one.
fn starts_word(bytes: &[u8], at: usize) -> bool {
    at == 0 || !(bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'_')
}

/// The index just past the escape sequence starting with `\` at `at`.
fn escape_end(bytes: &[u8], at: usize) -> usize {
    match bytes.get(at + 1) {
        Some(b'u') if bytes.get(at + 2) == Some(&b'{') => bytes[at + 3..]
            .iter()
            .position(|&b| b == b'}')
            .map_or(bytes.len(), |off| at + 3 + off + 1),
        Some(_) => at + 2,
        None => at + 1,
    }
}

/// The index just past the string literal whose opening `"` is at `at`,
/// triple-quoted or not, following placeholders when it is interpolated.
fn string_end(text: &str, at: usize, interpolated: bool, multiline: bool) -> Option<usize> {
    let bytes = text.as_bytes();
    let triple = text[at..].starts_with("\"\"\"");
    let mut i = at + if triple { 3 } else { 1 };
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i = escape_end(bytes, i),
            b'\n' if !triple && !multiline => return None,
            b'"' if !triple => return Some(i + 1),
            b'"' if text[i..].starts_with("\"\"\"") => return Some(i + 3),
            b'{' | b'}' if interpolated && bytes.get(i + 1) == Some(&bytes[i]) => i += 2,
            b'{' if interpolated => i += scan_placeholder(&text[i..], multiline || triple)?.len,
            _ => i += 1,
        }
    }
    None
}

/// The index just past the raw string whose `#`s or opening `"` start at `at`.
fn raw_string_end(text: &str, at: usize) -> Option<usize> {
    let hashes = text[at..].bytes().take_while(|&b| b == b'#').count();
    let open = at + hashes;
    if text.as_bytes().get(open) != Some(&b'"') {
        return None;
    }
    let closing = format!("\"{}", "#".repeat(hashes));
    text[open + 1..]
        .find(&closing)
        .map(|off| open + 1 + off + closing.len())
}

/// The index just past a character literal opening at `at`, or `None` when
/// the `'` starts a label instead.
fn char_literal_end(bytes: &[u8], at: usize) -> Option<usize> {
    if bytes.get(at + 1) == Some(&b'\\') {
        let after = escape_end(bytes, at + 1);
        return (bytes.get(after) == Some(&b'\'')).then_some(after + 1);
    }
    let width = utf8_width(*bytes.get(at + 1)?);
    (bytes.get(at + 1 + width) == Some(&b'\'')).then_some(at + 2 + width)
}

/// Bytes in the UTF-8 sequence whose first byte is `lead`.
fn utf8_width(lead: u8) -> usize {
    match lead {
        0xF0..=0xFF => 4,
        0xE0..=0xEF => 3,
        0xC0..=0xDF => 2,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::{InterpolationPiece, Placeholder, interpolation_pieces, scan_placeholder};

    fn expr_of(text: &str) -> &str {
        let placeholder = scan_placeholder(text, false).expect("a closed placeholder");
        &text[placeholder.expr]
    }

    #[test]
    fn a_placeholder_ends_at_its_own_closing_brace() {
        assert_eq!(expr_of("{x + 1} tail"), "x + 1");
        assert_eq!(expr_of("{m[\"}\"]}"), "m[\"}\"]");
        assert_eq!(expr_of("{if a { 1 } else { 2 }}"), "if a { 1 } else { 2 }");
        assert_eq!(expr_of("{f\"{inner}\"}"), "f\"{inner}\"");
        assert_eq!(expr_of("{c == '}'}"), "c == '}'");
    }

    #[test]
    fn a_top_level_single_colon_starts_the_spec() {
        let text = "{ratio * 100.0:.2}";
        let placeholder = scan_placeholder(text, false).expect("closed");
        assert_eq!(&text[placeholder.expr.clone()], "ratio * 100.0");
        assert_eq!(placeholder.spec.map(|spec| &text[spec]), Some(".2"));
        assert_eq!(expr_of("{i64::MAX}"), "i64::MAX");
        assert_eq!(expr_of("{Point { x: 1 }}"), "Point { x: 1 }");
    }

    #[test]
    fn an_unclosed_placeholder_has_no_extent() {
        assert_eq!(scan_placeholder("{x + 1", false), None);
        assert_eq!(scan_placeholder("{x\n}", false), None);
        assert!(scan_placeholder("{x\n}", true).is_some());
    }

    #[test]
    fn a_body_splits_into_text_and_placeholders() {
        let pieces = interpolation_pieces("a {{b}} {c + 1:>4} \\u{41}", false);
        assert_eq!(
            pieces,
            vec![
                InterpolationPiece::Text(0..8),
                InterpolationPiece::Placeholder(
                    Placeholder {
                        len: 10,
                        expr: 1..6,
                        spec: Some(7..9),
                    },
                    8,
                ),
                InterpolationPiece::Text(18..25),
            ]
        );
    }
}
