//! Pattern parsing (SPEC §5).

#![forbid(unsafe_code)]

use gossamer_ast::{
    FieldPattern, Ident, Literal, Mutability, Pattern, PatternKind, RangeKind, TypePath,
};
use gossamer_lex::{Keyword, Punct, TokenKind};

use crate::diagnostic::ParseError;
use crate::parser::Parser;

impl Parser<'_> {
    /// Parses a pattern that may include top-level `|` alternatives.
    pub(crate) fn parse_pattern(&mut self) -> Pattern {
        self.enter_pattern_pipe();
        let first = self.parse_pattern_no_or();
        if !self.at_punct(Punct::Pipe) {
            self.leave_pattern_pipe();
            return first;
        }
        let mut alternatives = vec![first];
        while self.eat_punct(Punct::Pipe) {
            alternatives.push(self.parse_pattern_no_or());
        }
        self.leave_pattern_pipe();
        let start_span = alternatives
            .first()
            .map_or_else(|| self.peek_span(), |pattern| pattern.span);
        let end_span = alternatives
            .last()
            .map_or_else(|| self.peek_span(), |pattern| pattern.span);
        let span = self.join(start_span, end_span);
        let id = self.alloc_id();
        Pattern::new(id, span, PatternKind::Or(alternatives))
    }

    /// Parses a pattern that never accepts a top-level `|`.
    pub(crate) fn parse_pattern_no_or(&mut self) -> Pattern {
        let start_span = self.peek_span();
        if self.enter_recursion(start_span).is_err() {
            let id = self.alloc_id();
            if !self.at_eof() {
                self.bump();
            }
            return Pattern::new(id, start_span, PatternKind::Error);
        }
        let kind = self.parse_pattern_kind();
        let end_span = self.last_span();
        let span = self.join(start_span, end_span);
        let id = self.alloc_id();
        self.leave_recursion();
        Pattern::new(id, span, kind)
    }

    fn parse_pattern_kind(&mut self) -> PatternKind {
        if self.at_punct(Punct::DotDot) || self.at_punct(Punct::DotDotEq) {
            return self.parse_range_pattern_or_rest();
        }
        if self.eat_punct(Punct::Amp) {
            let mutability = if self.eat_keyword(Keyword::Mut) {
                Mutability::Mutable
            } else {
                Mutability::Immutable
            };
            let inner = self.parse_pattern_no_or();
            return PatternKind::Ref {
                mutability,
                inner: Box::new(inner),
            };
        }
        if self.eat_punct(Punct::LParen) {
            return self.parse_tuple_pattern();
        }
        if self.eat_punct(Punct::LBracket) {
            return self.parse_slice_pattern();
        }
        if self.eat_keyword(Keyword::Mut) {
            return self.parse_ident_pattern(Mutability::Mutable);
        }
        if let Some(literal) = self.try_parse_range_bound() {
            return self.maybe_range_pattern(literal);
        }
        if matches!(self.peek().kind, TokenKind::Ident)
            && is_wildcard_ident(self.slice(self.peek_span()))
        {
            self.bump();
            return PatternKind::Wildcard;
        }
        if self.is_path_start() {
            return self.parse_path_pattern();
        }
        self.record(
            ParseError::unexpected("a pattern", self.peek_text()),
            self.peek_span(),
        );
        self.bump();
        PatternKind::Error
    }

    /// Parses a leading `..` / `..=`. Followed by a literal it is an
    /// open-start range pattern (`..hi` / `..=hi`); a bare `..` is the
    /// rest pattern. A bare `..=` with no upper bound is rejected.
    fn parse_range_pattern_or_rest(&mut self) -> PatternKind {
        let inclusive = self.at_punct(Punct::DotDotEq);
        self.bump();
        let kind = if inclusive {
            RangeKind::Inclusive
        } else {
            RangeKind::Exclusive
        };
        if let Some(hi) = self.try_parse_range_bound() {
            return PatternKind::Range {
                lo: None,
                hi: Some(hi),
                kind,
            };
        }
        if inclusive && self.is_path_start() {
            return self.reject_path_range_bound();
        }
        if inclusive {
            self.record(ParseError::InclusiveRangeMissingEnd, self.last_span());
            return PatternKind::Error;
        }
        PatternKind::Rest
    }

    fn parse_tuple_pattern(&mut self) -> PatternKind {
        if self.eat_punct(Punct::RParen) {
            return PatternKind::Literal(Literal::Unit);
        }
        let mut elements = Vec::new();
        elements.push(self.parse_pattern());
        // Only a written comma marks the one-element tuple `(p,)`; a newline
        // separator leaves `(\n p \n)` the parenthesised pattern it reads as.
        let mut saw_comma = false;
        loop {
            if self.eat_punct(Punct::Comma) {
                saw_comma = true;
            } else if self.at_eof() || !self.newline_before_peek() {
                // A newline is not a token, so it separates elements only
                // while one could still follow. At the end of input it would
                // keep asking for another element that never arrives.
                break;
            }
            if self.at_punct(Punct::RParen) || self.at_eof() {
                break;
            }
            let before = self.tokens.checkpoint();
            elements.push(self.parse_pattern());
            // A pattern that consumed nothing cannot be followed by one that
            // does; stop rather than collect the same failure forever.
            if self.tokens.checkpoint() == before {
                break;
            }
        }
        self.expect_punct(Punct::RParen, "to close tuple pattern");
        if elements.len() == 1 && !saw_comma {
            return elements.pop().expect("single-element tuple").kind;
        }
        PatternKind::Tuple(elements)
    }

    /// Parses a slice pattern `[p1, ..rest, pN]`. The leading `[` has
    /// already been consumed. A single `..` (optionally binding a
    /// sub-slice, e.g. `..rest`) splits the elements into a prefix and
    /// suffix; without `..` the pattern matches a fixed length.
    fn parse_slice_pattern(&mut self) -> PatternKind {
        let mut prefix = Vec::new();
        let mut suffix = Vec::new();
        let mut rest: Option<Box<Pattern>> = None;
        let mut seen_rest = false;
        while !self.at_punct(Punct::RBracket) && !self.at_eof() {
            if self.at_punct(Punct::DotDot) {
                let rest_span = self.peek_span();
                self.bump();
                let binding = if self.at_punct(Punct::Comma) || self.at_punct(Punct::RBracket) {
                    let id = self.alloc_id();
                    Box::new(Pattern::new(id, rest_span, PatternKind::Wildcard))
                } else {
                    Box::new(self.parse_pattern_no_or())
                };
                if seen_rest {
                    self.record(ParseError::SlicePatternExtraRest, rest_span);
                } else {
                    seen_rest = true;
                    rest = Some(binding);
                }
            } else {
                let pattern = self.parse_pattern();
                if seen_rest {
                    suffix.push(pattern);
                } else {
                    prefix.push(pattern);
                }
            }
            if !self.eat_list_separator() {
                break;
            }
        }
        self.expect_punct(Punct::RBracket, "to close slice pattern");
        PatternKind::Slice {
            prefix,
            rest,
            suffix,
        }
    }

    fn parse_ident_pattern(&mut self, mutability: Mutability) -> PatternKind {
        let token = self.peek();
        if !matches!(token.kind, TokenKind::Ident) {
            self.record(
                ParseError::unexpected_help(
                    "an identifier after `mut`",
                    self.peek_text(),
                    "a reference pattern is written `&mut name`, not `mut &name`",
                ),
                token.span,
            );
            return PatternKind::Error;
        }
        self.bump();
        let name = Ident::new(self.slice(token.span));
        let subpattern = if self.eat_punct(Punct::At) {
            Some(Box::new(self.parse_pattern_no_or()))
        } else {
            None
        };
        PatternKind::Ident {
            mutability,
            name,
            subpattern,
        }
    }

    fn try_parse_literal_pattern(&mut self) -> Option<Literal> {
        let token = self.peek();
        match token.kind {
            TokenKind::IntLit => {
                self.bump();
                Some(Literal::Int(self.slice(token.span).to_string()))
            }
            TokenKind::FloatLit => {
                self.bump();
                Some(Literal::Float(self.slice(token.span).to_string()))
            }
            TokenKind::StringLit | TokenKind::RawStringLit { .. } | TokenKind::TripleStringLit => {
                self.bump();
                Some(Literal::String(string_literal_value(
                    self.slice(token.span),
                )))
            }
            TokenKind::CharLit => {
                self.bump();
                Some(Literal::Char(char_literal_value(self.slice(token.span))))
            }
            TokenKind::ByteLit => {
                self.bump();
                Some(Literal::Byte(byte_literal_value(self.slice(token.span))))
            }
            TokenKind::Keyword(Keyword::True) => {
                self.bump();
                Some(Literal::Bool(true))
            }
            TokenKind::Keyword(Keyword::False) => {
                self.bump();
                Some(Literal::Bool(false))
            }
            TokenKind::Punct(Punct::Minus) => {
                if matches!(
                    self.peek_nth(1).kind,
                    TokenKind::IntLit | TokenKind::FloatLit
                ) {
                    self.bump();
                    let number = self.peek();
                    self.bump();
                    let spelling = format!("-{}", self.slice(number.span));
                    if matches!(number.kind, TokenKind::IntLit) {
                        return Some(Literal::Int(spelling));
                    }
                    return Some(Literal::Float(spelling));
                }
                None
            }
            _ => None,
        }
    }

    fn maybe_range_pattern(&mut self, lo: Literal) -> PatternKind {
        if self.at_punct(Punct::DotDot) || self.at_punct(Punct::DotDotEq) {
            let kind = if self.eat_punct(Punct::DotDotEq) {
                RangeKind::Inclusive
            } else {
                self.bump();
                RangeKind::Exclusive
            };
            // `lo..hi` / `lo..=hi` when a bound follows; otherwise `lo..`
            // is an open-end range up to the type maximum. `lo..=` has an
            // inclusive marker without an upper bound and is invalid.
            let hi = self.try_parse_range_bound();
            if hi.is_none() && self.is_path_start() {
                return self.reject_path_range_bound();
            }
            if hi.is_none() && kind == RangeKind::Inclusive {
                self.record(ParseError::InclusiveRangeMissingEnd, self.last_span());
                return PatternKind::Error;
            }
            return PatternKind::Range {
                lo: Some(lo),
                hi,
                kind,
            };
        }
        PatternKind::Literal(lo)
    }

    /// A literal, or a primitive integer limit such as `i64::MIN` or
    /// `u8::MAX`, which the language defines as that literal.
    fn try_parse_range_bound(&mut self) -> Option<Literal> {
        if let Some(literal) = self.try_parse_literal_pattern() {
            return Some(literal);
        }
        if !matches!(self.peek().kind, TokenKind::Ident)
            || !self.peek_nth_is_punct(1, Punct::ColonColon)
            || !matches!(self.peek_nth(2).kind, TokenKind::Ident)
        {
            return None;
        }
        let ty = self.slice(self.peek_span());
        let limit = self.slice(self.peek_nth(2).span);
        let value = primitive_int_limit(ty, limit)?;
        // A path that goes on (`i64::MAX::x`) or calls (`i64::MAX(..)`) is
        // not the limit, so it stays a path pattern.
        if self.peek_nth_is_punct(3, Punct::ColonColon) || self.peek_nth_is_punct(3, Punct::LParen)
        {
            return None;
        }
        self.bump();
        self.bump();
        self.bump();
        Some(Literal::Int(value.to_string()))
    }

    /// Reports a range pattern bound written as a path, consuming the path.
    fn reject_path_range_bound(&mut self) -> PatternKind {
        let start = self.peek_span();
        let path = self.parse_type_path();
        let span = self.join(start, self.last_span());
        let text = path_text(&path);
        self.record(ParseError::RangePatternBoundNotLiteral { text }, span);
        PatternKind::Error
    }

    fn is_path_start(&self) -> bool {
        matches!(
            self.peek().kind,
            TokenKind::Ident
                | TokenKind::Keyword(
                    Keyword::SelfUpper | Keyword::SelfLower | Keyword::Super | Keyword::Crate
                )
        )
    }

    fn parse_path_pattern(&mut self) -> PatternKind {
        let start_span = self.peek_span();
        let path = self.parse_type_path();
        if self.at_punct(Punct::DotDot) || self.at_punct(Punct::DotDotEq) {
            let span = self.join(start_span, self.last_span());
            let text = path_text(&path);
            self.record(ParseError::RangePatternBoundNotLiteral { text }, span);
            self.bump();
            if self.try_parse_range_bound().is_none() && self.is_path_start() {
                let _ = self.parse_type_path();
            }
            return PatternKind::Error;
        }
        let is_single_ident = path.segments.len() == 1 && path.segments[0].generics.is_empty();
        if self.eat_punct(Punct::LParen) {
            let mut elements = Vec::new();
            while !self.at_punct(Punct::RParen) && !self.at_eof() {
                elements.push(self.parse_pattern());
                if !self.eat_list_separator() {
                    break;
                }
            }
            self.expect_punct(Punct::RParen, "to close tuple-struct pattern");
            return PatternKind::TupleStruct {
                path,
                elems: elements,
            };
        }
        if self.eat_punct(Punct::LBrace) {
            let (fields, rest) = self.parse_struct_pattern_fields();
            return PatternKind::Struct { path, fields, rest };
        }
        if is_single_ident {
            let name_text = path.segments[0].name.name.clone();
            if starts_with_uppercase(&name_text) {
                return PatternKind::Path(path);
            }
            if self.eat_punct(Punct::At) {
                let subpattern = Some(Box::new(self.parse_pattern_no_or()));
                return PatternKind::Ident {
                    mutability: Mutability::Immutable,
                    name: Ident::new(name_text),
                    subpattern,
                };
            }
            let _ = start_span;
            return PatternKind::Ident {
                mutability: Mutability::Immutable,
                name: Ident::new(name_text),
                subpattern: None,
            };
        }
        PatternKind::Path(path)
    }

    fn parse_struct_pattern_fields(&mut self) -> (Vec<FieldPattern>, bool) {
        let mut fields = Vec::new();
        let mut rest = false;
        while !self.at_punct(Punct::RBrace) && !self.at_eof() {
            if self.eat_punct(Punct::DotDot) {
                rest = true;
                break;
            }
            let name_span = self.peek_span();
            if !matches!(self.peek().kind, TokenKind::Ident) {
                self.record(
                    ParseError::unexpected("a field name", self.peek_text()),
                    name_span,
                );
                self.bump();
                break;
            }
            let name = self.slice(name_span).to_string();
            self.bump();
            let pattern = if self.eat_punct(Punct::Colon) {
                Some(self.parse_pattern())
            } else {
                None
            };
            fields.push(FieldPattern {
                name: Ident::new(name),
                pattern,
            });
            if !self.eat_list_separator() {
                break;
            }
        }
        self.expect_punct(Punct::RBrace, "to close struct pattern");
        (fields, rest)
    }
}

/// Returns the decoded value of a double-quoted string literal. For now
/// the parser accepts the raw body between the quotes verbatim; future
/// phases may implement full escape decoding.
pub(crate) fn string_literal_value(source: &str) -> String {
    // A triple-quoted literal also opens and closes with `"`, so its
    // dedent must be recognised before the ordinary-string strip.
    if source.len() >= 6 && source.starts_with("\"\"\"") && source.ends_with("\"\"\"") {
        return decode_string_escapes(&gossamer_lex::triple_string(source).body());
    }
    if let Some(stripped) = source
        .strip_prefix('"')
        .and_then(|text| text.strip_suffix('"'))
    {
        return decode_string_escapes(stripped);
    }
    if let Some(stripped) = source
        .strip_prefix("r\"")
        .and_then(|text| text.strip_suffix('"'))
    {
        return stripped.to_string();
    }
    source.to_string()
}

fn decode_string_escapes(body: &str) -> String {
    let mut output = String::with_capacity(body.len());
    let mut chars = body.chars().peekable();
    while let Some(current) = chars.next() {
        if current != '\\' {
            output.push(current);
            continue;
        }
        match chars.next() {
            Some('n') => output.push('\n'),
            Some('r') => output.push('\r'),
            Some('t') => output.push('\t'),
            Some('\\') => output.push('\\'),
            Some('\'') => output.push('\''),
            Some('"') => output.push('"'),
            Some('0') => output.push('\0'),
            Some('u') => decode_unicode_escape(&mut chars, &mut output),
            Some('x') => decode_hex_escape(&mut chars, &mut output),
            Some(other) => {
                output.push('\\');
                output.push(other);
            }
            None => output.push('\\'),
        }
    }
    output
}

/// Decodes a `\u{...}` escape. Accepts 1 to 6 hex digits, rejecting
/// surrogate code points (0xD800..=0xDFFF), values past 0x10FFFF, and
/// malformed body shapes. On rejection the literal is preserved as
/// `\u{...}` so the typechecker can emit a structured diagnostic
/// from the same surface text.
fn decode_unicode_escape<I: Iterator<Item = char>>(
    chars: &mut std::iter::Peekable<I>,
    output: &mut String,
) {
    if chars.peek() != Some(&'{') {
        output.push_str("\\u");
        return;
    }
    chars.next();
    let mut digits = String::with_capacity(6);
    while let Some(&c) = chars.peek() {
        if c == '}' {
            break;
        }
        if !c.is_ascii_hexdigit() || digits.len() >= 6 {
            break;
        }
        digits.push(c);
        chars.next();
    }
    let closed = chars.peek() == Some(&'}');
    if closed {
        chars.next();
    }
    let value = u32::from_str_radix(&digits, 16).ok();
    let valid = match value {
        Some(v) if v <= 0x10FFFF && !(0xD800..=0xDFFF).contains(&v) => char::from_u32(v),
        _ => None,
    };
    match valid {
        Some(c) if closed && !digits.is_empty() => output.push(c),
        _ => {
            output.push_str("\\u{");
            output.push_str(&digits);
            if closed {
                output.push('}');
            }
        }
    }
}

/// Decodes a `\xNN` escape into an ASCII char (0x00..=0x7F). Per Rust
/// convention non-ASCII bytes are rejected; the literal is preserved
/// verbatim so a downstream diagnostic can flag it.
fn decode_hex_escape<I: Iterator<Item = char>>(
    chars: &mut std::iter::Peekable<I>,
    output: &mut String,
) {
    let mut digits = String::with_capacity(2);
    for _ in 0..2 {
        if let Some(&c) = chars.peek() {
            if c.is_ascii_hexdigit() {
                digits.push(c);
                chars.next();
                continue;
            }
        }
        break;
    }
    let value = if digits.len() == 2 {
        u8::from_str_radix(&digits, 16).ok()
    } else {
        None
    };
    match value {
        Some(byte) if byte <= 0x7F => output.push(byte as char),
        _ => {
            output.push_str("\\x");
            output.push_str(&digits);
        }
    }
}

/// Returns the decoded char value for a `'x'` literal.
pub(crate) fn char_literal_value(source: &str) -> char {
    // Exactly one delimiter comes off each end: `'\''` carries an escaped
    // quote whose own `'` would otherwise be eaten as a repeated delimiter,
    // leaving a lone backslash behind.
    let body = source.strip_prefix('\'').unwrap_or(source);
    let body = body.strip_suffix('\'').unwrap_or(body);
    let decoded = decode_string_escapes(body);
    decoded.chars().next().unwrap_or('\0')
}

/// Returns the decoded byte value for a `b'x'` literal.
pub(crate) fn byte_literal_value(source: &str) -> u8 {
    let body = source.strip_prefix("b'").unwrap_or(source);
    let body = body.strip_suffix('\'').unwrap_or(body);
    let decoded = decode_string_escapes(body);
    decoded.bytes().next().unwrap_or(0)
}

/// Returns the decoded byte vector for a `b"..."` literal.
pub(crate) fn byte_string_literal_value(source: &str) -> Vec<u8> {
    let body = source.strip_prefix("b\"").unwrap_or(source);
    let body = body.strip_suffix('"').unwrap_or(body);
    decode_string_escapes(body).into_bytes()
}

fn is_wildcard_ident(text: &str) -> bool {
    text == "_"
}

fn starts_with_uppercase(text: &str) -> bool {
    text.chars()
        .next()
        .is_some_and(|character| character.is_ascii_uppercase())
}

/// The literal spelling of a primitive integer type's `MIN` or `MAX`.
fn primitive_int_limit(ty: &str, limit: &str) -> Option<i128> {
    let (min, max): (i128, i128) = match ty {
        "i8" => (i8::MIN.into(), i8::MAX.into()),
        "i16" => (i16::MIN.into(), i16::MAX.into()),
        "i32" => (i32::MIN.into(), i32::MAX.into()),
        "i64" | "isize" => (i64::MIN.into(), i64::MAX.into()),
        "u8" => (0, u8::MAX.into()),
        "u16" => (0, u16::MAX.into()),
        "u32" => (0, u32::MAX.into()),
        "u64" | "usize" => (0, u64::MAX.into()),
        _ => return None,
    };
    match limit {
        "MIN" => Some(min),
        "MAX" => Some(max),
        _ => None,
    }
}

fn path_text(path: &TypePath) -> String {
    path.segments
        .iter()
        .map(|segment| segment.name.name.as_str())
        .collect::<Vec<_>>()
        .join("::")
}
