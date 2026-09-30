//! JSON as the language defines it: the dynamic [`Value`], the grammar and
//! limits a document is read under, and the diagnostic a rejected one
//! reports. The bytecode VM parses into [`Value`] with [`parse`]; native
//! builds check a document with [`validate`] before materializing it, so every
//! tier accepts, rejects, and describes the same documents the same way.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use thiserror::Error;

/// Default cap on parser nesting depth - guards the recursive descent
/// against stack exhaustion from adversarial input.
pub const DEFAULT_MAX_DEPTH: usize = 128;

/// Default cap on document byte length - guards against memory
/// exhaustion from oversized payloads. 16 MiB.
pub const DEFAULT_MAX_SIZE: usize = 16 * 1024 * 1024;

static MAX_DEPTH: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_DEPTH);
static MAX_SIZE: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_SIZE);

/// Overrides the process-wide cap on parser nesting depth.
pub fn set_max_depth(n: usize) {
    MAX_DEPTH.store(n, Ordering::Relaxed);
}

/// Overrides the process-wide cap on input byte length.
pub fn set_max_size(n: usize) {
    MAX_SIZE.store(n, Ordering::Relaxed);
}

/// Returns the current cap on parser nesting depth.
#[must_use]
pub fn max_depth() -> usize {
    MAX_DEPTH.load(Ordering::Relaxed)
}

/// Returns the current cap on input byte length.
#[must_use]
pub fn max_size() -> usize {
    MAX_SIZE.load(Ordering::Relaxed)
}

/// Dynamically typed JSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// `null`.
    Null,
    /// `true` / `false`.
    Bool(bool),
    /// Integer literal that fits an `i64`, preserved exactly. Kept
    /// distinct from `Number` so large integers (above 2^53) round-trip
    /// without the precision loss an `f64` would impose and so `100`
    /// renders without a trailing `.0`, matching `serde_json`.
    Int(i64),
    /// Integer above `i64::MAX` that fits a `u64`, preserved exactly so an
    /// unsigned value renders with its own digits.
    Uint(u64),
    /// Non-integer numeric literal (has a fractional part or exponent),
    /// or an integer too large for `u64`, preserved as `f64`.
    Number(f64),
    /// UTF-8 string.
    String(String),
    /// Ordered array.
    Array(Vec<Value>),
    /// Object keyed by field name, iteration order sorted.
    Object(BTreeMap<String, Value>),
}

/// Error returned by [`parse`]. Carries one-based line and column so
/// downstream tooling can produce pointed diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message} at {line}:{column}")]
pub struct Error {
    /// Human-readable explanation of the failure.
    pub message: String,
    /// One-based line number of the offending character.
    pub line: u32,
    /// One-based column number of the offending character.
    pub column: u32,
}

/// Parses a JSON document into a [`Value`].
pub fn parse(source: &str) -> Result<Value, Error> {
    let cap = max_size();
    if source.len() > cap {
        return Err(Error {
            message: format!("input exceeds max_size ({} > {cap})", source.len()),
            line: 1,
            column: 1,
        });
    }
    let mut parser = Parser::new(source);
    parser.skip_whitespace();
    let value = parser.parse_value()?;
    parser.skip_whitespace();
    if parser.cursor < parser.bytes.len() {
        return Err(parser.error("trailing input"));
    }
    Ok(value)
}

/// Checks that `bytes` is a JSON document under the current limits without
/// building it, answering the document as text, or the error [`parse`] would
/// report for it.
///
/// # Errors
///
/// The first grammar, encoding, or limit violation, with its position.
pub fn validate(bytes: &[u8]) -> Result<&str, Error> {
    let cap = max_size();
    if bytes.len() > cap {
        return Err(Error {
            message: format!("input exceeds max_size ({} > {cap})", bytes.len()),
            line: 1,
            column: 1,
        });
    }
    let source = std::str::from_utf8(bytes).map_err(|error| {
        let prefix = &bytes[..error.valid_up_to()];
        let line = prefix.split(|b| *b == b'\n').count();
        let column = prefix.iter().rev().take_while(|b| **b != b'\n').count() + 1;
        Error {
            message: "invalid UTF-8".to_string(),
            line: u32::try_from(line).unwrap_or(u32::MAX),
            column: u32::try_from(column).unwrap_or(u32::MAX),
        }
    })?;
    let mut parser = Parser::new(source);
    parser.build = false;
    parser.skip_whitespace();
    parser.parse_value()?;
    parser.skip_whitespace();
    if parser.cursor < parser.bytes.len() {
        return Err(parser.error("trailing input"));
    }
    Ok(source)
}

struct Parser<'a> {
    bytes: &'a [u8],
    cursor: usize,
    depth: usize,
    max_depth: usize,
    /// Whether the parse keeps what it reads; [`validate`] walks the same
    /// grammar and keeps nothing.
    build: bool,
}

impl<'a> Parser<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            bytes: source.as_bytes(),
            cursor: 0,
            depth: 0,
            max_depth: max_depth(),
            build: true,
        }
    }

    fn enter(&mut self) -> Result<(), Error> {
        if self.depth >= self.max_depth {
            return Err(self.error(format!(
                "nesting depth exceeds max_depth ({})",
                self.max_depth
            )));
        }
        self.depth += 1;
        Ok(())
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    /// An error at the cursor. The position is derived from the bytes read so
    /// far only when one is reported, so reading a document costs nothing for
    /// it: the line counts newlines, the column bytes since the last one.
    fn error(&self, message: impl Into<String>) -> Error {
        let read = &self.bytes[..self.cursor.min(self.bytes.len())];
        let line = read.split(|b| *b == b'\n').count();
        let column = read.iter().rev().take_while(|b| **b != b'\n').count() + 1;
        Error {
            message: message.into(),
            line: u32::try_from(line).unwrap_or(u32::MAX),
            column: u32::try_from(column).unwrap_or(u32::MAX),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.cursor).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.cursor += 1;
        Some(b)
    }

    fn skip_whitespace(&mut self) {
        while let Some(b) = self.peek() {
            if matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
                self.bump();
            } else {
                break;
            }
        }
    }

    fn parse_value(&mut self) -> Result<Value, Error> {
        self.skip_whitespace();
        match self
            .peek()
            .ok_or_else(|| self.error("unexpected end of input"))?
        {
            b'{' => self.parse_object(),
            b'[' => self.parse_array(),
            b'"' => self.parse_string().map(Value::String),
            b't' | b'f' => self.parse_bool(),
            b'n' => self.parse_null(),
            b'-' | b'0'..=b'9' => self.parse_number(),
            other => Err(self.error(format!("unexpected byte {other:#x}"))),
        }
    }

    fn parse_object(&mut self) -> Result<Value, Error> {
        self.enter()?;
        self.bump();
        let mut map = BTreeMap::new();
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.bump();
            self.leave();
            return Ok(Value::Object(map));
        }
        loop {
            self.skip_whitespace();
            let key = self.parse_string()?;
            self.skip_whitespace();
            if self.bump() != Some(b':') {
                return Err(self.error("expected `:`"));
            }
            let value = self.parse_value()?;
            if self.build {
                map.insert(key, value);
            }
            self.skip_whitespace();
            match self.bump() {
                Some(b',') => {}
                Some(b'}') => {
                    self.leave();
                    return Ok(Value::Object(map));
                }
                _ => return Err(self.error("expected `,` or `}` in object")),
            }
        }
    }

    fn parse_array(&mut self) -> Result<Value, Error> {
        self.enter()?;
        self.bump();
        let mut out = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.bump();
            self.leave();
            return Ok(Value::Array(out));
        }
        loop {
            let value = self.parse_value()?;
            if self.build {
                out.push(value);
            }
            self.skip_whitespace();
            match self.bump() {
                Some(b',') => {}
                Some(b']') => {
                    self.leave();
                    return Ok(Value::Array(out));
                }
                _ => return Err(self.error("expected `,` or `]` in array")),
            }
        }
    }

    fn parse_string(&mut self) -> Result<String, Error> {
        self.skip_whitespace();
        if self.bump() != Some(b'"') {
            return Err(self.error("expected string"));
        }
        let mut out = String::new();
        loop {
            let Some(byte) = self.peek() else {
                return Err(self.error("unterminated string"));
            };
            match byte {
                b'"' => {
                    self.bump();
                    return Ok(out);
                }
                b'\\' => {
                    self.bump();
                    let Some(escape) = self.bump() else {
                        return Err(self.error("unterminated escape"));
                    };
                    match escape {
                        b'"' => self.put(&mut out, '"'),
                        b'\\' => self.put(&mut out, '\\'),
                        b'/' => self.put(&mut out, '/'),
                        b'n' => self.put(&mut out, '\n'),
                        b't' => self.put(&mut out, '\t'),
                        b'r' => self.put(&mut out, '\r'),
                        b'b' => self.put(&mut out, '\u{0008}'),
                        b'f' => self.put(&mut out, '\u{000c}'),
                        b'u' => {
                            let cp = self.parse_hex4()?;
                            if (0xD800..=0xDBFF).contains(&cp) {
                                if self.bump() != Some(b'\\') || self.bump() != Some(b'u') {
                                    return Err(self.error("expected low surrogate after high"));
                                }
                                let lo = self.parse_hex4()?;
                                if !(0xDC00..=0xDFFF).contains(&lo) {
                                    return Err(self.error("invalid low surrogate"));
                                }
                                let scalar = 0x10000 + (((cp - 0xD800) << 10) | (lo - 0xDC00));
                                match char::from_u32(scalar) {
                                    Some(c) => self.put(&mut out, c),
                                    None => return Err(self.error("invalid surrogate pair")),
                                }
                            } else if (0xDC00..=0xDFFF).contains(&cp) {
                                return Err(self.error("unpaired low surrogate"));
                            } else {
                                match char::from_u32(cp) {
                                    Some(c) => self.put(&mut out, c),
                                    None => return Err(self.error("invalid unicode escape")),
                                }
                            }
                        }
                        other => return Err(self.error(format!("unknown escape {other:#x}"))),
                    }
                }
                byte if byte < 0x20 => {
                    return Err(self.error("control character in string"));
                }
                _ => {
                    // A run of plain bytes up to the next quote, escape, or
                    // control byte. The source is a `str` and those three are
                    // ASCII, so the run ends on a character boundary.
                    let start = self.cursor;
                    let run = self.bytes[start..]
                        .iter()
                        .position(|b| matches!(b, b'"' | b'\\') || *b < 0x20)
                        .unwrap_or(self.bytes.len() - start);
                    self.cursor += run;
                    if self.build {
                        let text = std::str::from_utf8(&self.bytes[start..self.cursor])
                            .map_err(|_| self.error("invalid UTF-8 in string"))?;
                        out.push_str(text);
                    }
                }
            }
        }
    }

    /// Appends `c` to a string being read, when the parse keeps it.
    fn put(&self, out: &mut String, c: char) {
        if self.build {
            out.push(c);
        }
    }

    fn parse_hex4(&mut self) -> Result<u32, Error> {
        let mut acc: u32 = 0;
        for _ in 0..4 {
            let b = self
                .bump()
                .ok_or_else(|| self.error("truncated unicode escape"))?;
            let digit = match b {
                b'0'..=b'9' => u32::from(b - b'0'),
                b'a'..=b'f' => u32::from(b - b'a') + 10,
                b'A'..=b'F' => u32::from(b - b'A') + 10,
                _ => return Err(self.error("invalid hex digit in unicode escape")),
            };
            acc = (acc << 4) | digit;
        }
        Ok(acc)
    }

    fn parse_bool(&mut self) -> Result<Value, Error> {
        if self.bytes[self.cursor..].starts_with(b"true") {
            for _ in 0..4 {
                self.bump();
            }
            Ok(Value::Bool(true))
        } else if self.bytes[self.cursor..].starts_with(b"false") {
            for _ in 0..5 {
                self.bump();
            }
            Ok(Value::Bool(false))
        } else {
            Err(self.error("expected `true` or `false`"))
        }
    }

    fn parse_null(&mut self) -> Result<Value, Error> {
        if self.bytes[self.cursor..].starts_with(b"null") {
            for _ in 0..4 {
                self.bump();
            }
            Ok(Value::Null)
        } else {
            Err(self.error("expected `null`"))
        }
    }

    fn parse_number(&mut self) -> Result<Value, Error> {
        // RFC 8259: `-? (0 | [1-9][0-9]*) (. [0-9]+)? ([eE] [+-]? [0-9]+)?`.
        let start = self.cursor;
        if self.peek() == Some(b'-') {
            self.bump();
        }
        match self.peek() {
            Some(b'0') => {
                self.bump();
            }
            Some(b'1'..=b'9') => self.bump_digits(),
            _ => return Err(self.error("invalid number: expected a digit")),
        }
        if self.peek() == Some(b'.') {
            self.bump();
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.error("invalid number: expected a digit after `.`"));
            }
            self.bump_digits();
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.bump();
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.bump();
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.error("invalid number: expected a digit in the exponent"));
            }
            self.bump_digits();
        }
        let text = std::str::from_utf8(&self.bytes[start..self.cursor])
            .map_err(|_| self.error("invalid UTF-8 in number"))?;
        // Integer-form tokens that fit `i64` are kept as `Int` so large
        // integers round-trip exactly and render without a trailing `.0`
        // (matching serde_json's number handling on the compiled tier).
        // An integer above `i64::MAX` that fits `u64` stays exact as `Uint`.
        // Anything with a fractional part or exponent - or too large for
        // `u64` - falls back to `f64`.
        let is_float = text.bytes().any(|b| matches!(b, b'.' | b'e' | b'E'));
        // Eighteen digits always fit an `i64`, so validation has nothing to
        // check in a short integer.
        if !self.build && !is_float && text.len() <= 18 {
            return Ok(Value::Null);
        }
        if !is_float {
            // `-0` keeps its sign, which only a float can, as serde_json and
            // the compiled tiers read it.
            if text == "-0" {
                return Ok(Value::Number(-0.0));
            }
            if let Ok(n) = text.parse::<i64>() {
                return Ok(Value::Int(n));
            }
            if let Ok(n) = text.parse::<u64>() {
                return Ok(Value::Uint(n));
            }
        }
        // A number too large for an `f64` is rejected rather than read as an
        // infinity no JSON document can spell back.
        match text.parse::<f64>() {
            Ok(value) if value.is_finite() => Ok(Value::Number(value)),
            Ok(_) => Err(self.error(format!("number out of range {text:?}"))),
            Err(_) => Err(self.error(format!("invalid number {text:?}"))),
        }
    }
}

impl Parser<'_> {
    /// Consumes a run of ASCII digits.
    fn bump_digits(&mut self) {
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.bump();
        }
    }
}
