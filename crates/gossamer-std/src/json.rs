//! Runtime support for `std::encoding::json`.
//! Implements the dynamic [`Value`] type and a hand-written
//! recursive-descent parser + emitter pair. The derive-based
//! `Serialize` / `Deserialize` traits from the SPEC lower to calls
//! into `to_value` / `from_value` once the compiler grows derive
//! support, so the surface here is stable even though the macro layer
//! is not yet present.

#![forbid(unsafe_code)]
#![allow(
    clippy::cast_lossless,
    clippy::needless_continue,
    clippy::too_many_lines,
    clippy::struct_excessive_bools
)]

use std::collections::BTreeMap;
use std::fmt::Write;
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
    /// Non-integer numeric literal (has a fractional part or exponent),
    /// or an integer too large for `i64`, preserved as `f64`.
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

/// Encodes a [`Value`] as a compact UTF-8 JSON string.
#[must_use]
pub fn encode(value: &Value) -> String {
    let mut out = String::with_capacity(encoded_capacity_hint(value));
    write_value(&mut out, value);
    out
}

/// Encodes a [`Value`] with two-space indentation.
#[must_use]
pub fn encode_pretty(value: &Value) -> String {
    // Pretty output is never shorter than compact output. Reserving the
    // compact size avoids geometric growth for the common shallow case;
    // indentation is appended as needed for deeply nested documents.
    let mut out = String::with_capacity(encoded_capacity_hint(value));
    write_pretty(&mut out, value, 0);
    out
}

/// Builds a [`Value::Int`] from an `i64`, preserving it exactly.
#[must_use]
pub fn from_i64(n: i64) -> Value {
    Value::Int(n)
}

/// Retrieves an `i64` from a [`Value::Int`], or from a [`Value::Number`]
/// whose value is integral and within `i64` range.
#[must_use]
pub fn as_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Int(n) => Some(*n),
        Value::Number(n) if n.fract() == 0.0 && *n >= i64::MIN as f64 && *n <= i64::MAX as f64 => {
            Some(*n as i64)
        }
        _ => None,
    }
}

/// Retrieves an `f64` from a [`Value::Number`] or [`Value::Int`].
#[must_use]
pub fn as_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => Some(*n),
        Value::Int(n) => Some(*n as f64),
        _ => None,
    }
}

/// Retrieves a `&str` from a [`Value::String`].
#[must_use]
pub fn as_str(value: &Value) -> Option<&str> {
    if let Value::String(s) = value {
        Some(s)
    } else {
        None
    }
}

/// Retrieves a `bool` from a [`Value::Bool`].
#[must_use]
pub fn as_bool(value: &Value) -> Option<bool> {
    if let Value::Bool(b) = value {
        Some(*b)
    } else {
        None
    }
}

/// Borrows the inner `Vec` of a [`Value::Array`].
#[must_use]
pub fn as_array(value: &Value) -> Option<&Vec<Value>> {
    if let Value::Array(a) = value {
        Some(a)
    } else {
        None
    }
}

/// Borrows the inner `BTreeMap` of a [`Value::Object`].
#[must_use]
pub fn as_object(value: &Value) -> Option<&BTreeMap<String, Value>> {
    if let Value::Object(m) = value {
        Some(m)
    } else {
        None
    }
}

/// Returns `true` for `Value::Null`. Mirrors `serde_json::Value::is_null`.
#[must_use]
pub fn is_null(value: &Value) -> bool {
    matches!(value, Value::Null)
}

/// Number of items in an array, key/value pairs in an object, or
/// bytes in a string. Returns 0 for any other variant.
#[must_use]
pub fn len(value: &Value) -> i64 {
    match value {
        Value::Array(a) => a.len() as i64,
        Value::Object(m) => m.len() as i64,
        Value::String(s) => s.len() as i64,
        _ => 0,
    }
}

/// Looks up `key` in an object (`{...}`). Returns `None` for any
/// non-object value or when the key isn't present. Pair with
/// [`as_str`] / [`as_array`] / [`as_i64`] to drill in.
#[must_use]
pub fn get<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    if let Value::Object(m) = value {
        m.get(key)
    } else {
        None
    }
}

/// Returns the i-th element of a [`Value::Array`], if any.
#[must_use]
pub fn at(value: &Value, idx: i64) -> Option<&Value> {
    if let Value::Array(a) = value {
        if idx < 0 {
            return None;
        }
        a.get(idx as usize)
    } else {
        None
    }
}

/// Returns every key of a [`Value::Object`] in sorted order.
#[must_use]
pub fn keys(value: &Value) -> Vec<String> {
    match value {
        Value::Object(m) => m.keys().cloned().collect(),
        _ => Vec::new(),
    }
}

/// Alias so the surface matches the SPEC's `json::decode`.
pub fn decode(source: &str) -> Result<Value, Error> {
    parse(source)
}

/// Alias for `encode`, matching the SPEC's `json::encode`.
#[must_use]
pub fn to_string(value: &Value) -> String {
    encode(value)
}

/// Placeholder derive adapters - until the compiler's derive machinery
/// can target these traits directly, callers hand-implement them by
/// constructing [`Value`]s manually.
pub mod serde_surface {
    use super::Value;

    /// `Serialize`-shaped trait exposed to user code.
    pub trait Serialize {
        /// Converts `self` into a JSON [`Value`].
        fn to_json(&self) -> Value;
    }

    /// `Deserialize`-shaped trait.
    pub trait Deserialize: Sized {
        /// Error type returned on failure.
        type Error;
        /// Builds `Self` from a [`Value`].
        fn from_json(value: &Value) -> Result<Self, Self::Error>;
    }
}

/// Streaming JSON decoder over an [`std::io::Read`] source. Mirrors Go's
/// `json.NewDecoder(r).Decode(&v)` shape: each call to [`Decoder::decode`]
/// returns the next document on the stream as a [`Value`]. Suitable for
/// JSON-Lines / NDJSON workloads where the response body is too large
/// to buffer fully.
///
/// The decoder reads into an internal buffer one chunk at a time, so a
/// caller streaming a 1 GiB response body never holds more than the
/// next document plus the buffer in memory.
pub struct Decoder<R: std::io::Read> {
    reader: R,
    buffer: Vec<u8>,
    cursor: usize,
    eof: bool,
}

impl<R: std::io::Read> Decoder<R> {
    /// Constructs a decoder reading from `reader`.
    #[must_use]
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            buffer: Vec::new(),
            cursor: 0,
            eof: false,
        }
    }

    /// Decodes the next JSON document from the stream.
    /// Returns `Ok(None)` when the stream has been fully consumed
    /// (whitespace-only tail).
    pub fn decode(&mut self) -> Result<Option<Value>, Error> {
        self.skip_whitespace_buffered()?;
        if self.cursor >= self.buffer.len() && self.eof {
            return Ok(None);
        }
        let (start, end) = self.read_one_document()?;
        let text = std::str::from_utf8(&self.buffer[start..end]).map_err(|_| Error {
            message: "invalid UTF-8 in stream".into(),
            line: 0,
            column: 0,
        })?;
        let value = parse(text)?;
        self.discard_consumed();
        Ok(Some(value))
    }

    /// Drains every remaining document.
    pub fn decode_all(&mut self) -> Result<Vec<Value>, Error> {
        let mut out = Vec::new();
        while let Some(v) = self.decode()? {
            out.push(v);
        }
        Ok(out)
    }

    fn fill_more(&mut self) -> Result<bool, Error> {
        if self.eof {
            return Ok(false);
        }
        let mut chunk = [0u8; 4096];
        match self.reader.read(&mut chunk) {
            Ok(0) => {
                self.eof = true;
                Ok(false)
            }
            Ok(n) => {
                self.buffer.extend_from_slice(&chunk[..n]);
                Ok(true)
            }
            Err(e) => Err(Error {
                message: format!("io: {e}"),
                line: 0,
                column: 0,
            }),
        }
    }

    fn skip_whitespace_buffered(&mut self) -> Result<(), Error> {
        loop {
            while self.cursor < self.buffer.len()
                && matches!(self.buffer[self.cursor], b' ' | b'\t' | b'\n' | b'\r')
            {
                self.cursor += 1;
            }
            if self.cursor < self.buffer.len() {
                return Ok(());
            }
            // Do not retain an unbounded whitespace-only tail while waiting
            // for the next document (or EOF). All buffered bytes have been
            // consumed at this point, so compaction cannot discard input the
            // caller might still decode.
            self.discard_consumed();
            if !self.fill_more()? {
                return Ok(());
            }
        }
    }

    /// Returns the byte range of one document in `buffer`. Keeping the
    /// document in place avoids a full temporary copy before `parse` builds
    /// the value tree. `decode` discards the consumed prefix immediately
    /// after parsing, so prior documents never accumulate in the buffer.
    fn read_one_document(&mut self) -> Result<(usize, usize), Error> {
        let start = self.cursor;
        let first = self.peek_byte_buffered()?;
        match first {
            b'{' => self.read_balanced(b'{', b'}'),
            b'[' => self.read_balanced(b'[', b']'),
            b'"' => {
                self.cursor += 1;
                self.read_until_string_end(start)?;
                Ok((start, self.cursor))
            }
            _ => self.read_scalar(),
        }
    }

    fn peek_byte_buffered(&mut self) -> Result<u8, Error> {
        loop {
            if self.cursor < self.buffer.len() {
                return Ok(self.buffer[self.cursor]);
            }
            if !self.fill_more()? {
                return Err(Error {
                    message: "unexpected end of stream".into(),
                    line: 0,
                    column: 0,
                });
            }
        }
    }

    fn read_balanced(&mut self, open: u8, close: u8) -> Result<(usize, usize), Error> {
        let start = self.cursor;
        let mut depth = 0i64;
        let mut in_string = false;
        let mut escape = false;
        loop {
            while self.cursor < self.buffer.len() {
                let b = self.buffer[self.cursor];
                self.cursor += 1;
                self.ensure_document_limit(start)?;
                if in_string {
                    if escape {
                        escape = false;
                    } else if b == b'\\' {
                        escape = true;
                    } else if b == b'"' {
                        in_string = false;
                    }
                    continue;
                }
                if b == b'"' {
                    in_string = true;
                } else if b == open {
                    depth += 1;
                } else if b == close {
                    depth -= 1;
                    if depth == 0 {
                        return Ok((start, self.cursor));
                    }
                }
            }
            if !self.fill_more()? {
                return Err(Error {
                    message: "unterminated JSON document".into(),
                    line: 0,
                    column: 0,
                });
            }
        }
    }

    fn read_until_string_end(&mut self, start: usize) -> Result<(), Error> {
        let mut escape = false;
        loop {
            while self.cursor < self.buffer.len() {
                let b = self.buffer[self.cursor];
                self.cursor += 1;
                self.ensure_document_limit(start)?;
                if escape {
                    escape = false;
                } else if b == b'\\' {
                    escape = true;
                } else if b == b'"' {
                    return Ok(());
                }
            }
            if !self.fill_more()? {
                return Err(Error {
                    message: "unterminated string in stream".into(),
                    line: 0,
                    column: 0,
                });
            }
        }
    }

    fn read_scalar(&mut self) -> Result<(usize, usize), Error> {
        let start = self.cursor;
        loop {
            while self.cursor < self.buffer.len() {
                let b = self.buffer[self.cursor];
                if matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b',' | b']' | b'}') {
                    return Ok((start, self.cursor));
                }
                self.cursor += 1;
                self.ensure_document_limit(start)?;
            }
            if !self.fill_more()? {
                return Ok((start, self.cursor));
            }
        }
    }

    fn ensure_document_limit(&self, start: usize) -> Result<(), Error> {
        let len = self.cursor.saturating_sub(start);
        let cap = max_size();
        if len > cap {
            return Err(Error {
                message: format!("input exceeds max_size ({len} > {cap})"),
                line: 0,
                column: 0,
            });
        }
        Ok(())
    }

    /// Drops consumed bytes while retaining an allocation suitable for the
    /// next small document. A one-off large document must not permanently
    /// pin its input buffer for a long-lived streaming decoder.
    fn discard_consumed(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let unread = self.buffer.len() - self.cursor;
        if self.buffer.capacity() > 16 * 1024 && unread <= 4096 {
            let mut next = Vec::with_capacity(4096);
            next.extend_from_slice(&self.buffer[self.cursor..]);
            self.buffer = next;
        } else {
            self.buffer.copy_within(self.cursor.., 0);
            self.buffer.truncate(unread);
        }
        self.cursor = 0;
    }
}

/// Streaming JSON encoder writing one document per [`Encoder::encode`]
/// call into an [`std::io::Write`] sink.
pub struct Encoder<W: std::io::Write> {
    writer: W,
}

impl<W: std::io::Write> Encoder<W> {
    /// Constructs a streaming encoder.
    pub fn new(writer: W) -> Self {
        Self { writer }
    }

    /// Writes `value` followed by a newline.
    pub fn encode(&mut self, value: &Value) -> std::io::Result<()> {
        // Buffer small punctuation/string writes so streaming to an
        // unbuffered file or socket does not turn one JSON character into
        // one system call. The buffer is fixed-size, unlike the former
        // `encode(value)` temporary which grew with the whole document.
        let mut writer = std::io::BufWriter::new(&mut self.writer);
        write_value_to(&mut writer, value)?;
        std::io::Write::write_all(&mut writer, b"\n")?;
        std::io::Write::flush(&mut writer)
    }

    /// Returns the underlying writer.
    pub fn into_inner(self) -> W {
        self.writer
    }
}

/// Field-tag descriptor. The compiler's derive machinery (deferred -
/// see [`serde_surface`]) walks a struct's tags to know how to map
/// JSON keys onto Rust fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldTag {
    /// Source-side identifier (`Rust`-side struct field).
    pub field: &'static str,
    /// JSON-side name (`json("name")`).
    pub json_name: &'static str,
    /// Whether the field is omitted when its value is the zero value.
    pub omit_empty: bool,
}

impl FieldTag {
    /// Convenience constructor for field tag tables.
    #[must_use]
    pub const fn new(field: &'static str, json_name: &'static str) -> Self {
        Self {
            field,
            json_name,
            omit_empty: false,
        }
    }

    /// Marks the tag with `omit_empty`.
    #[must_use]
    pub const fn omit_empty(mut self) -> Self {
        self.omit_empty = true;
        self
    }
}

struct Parser<'a> {
    bytes: &'a [u8],
    cursor: usize,
    line: u32,
    column: u32,
    depth: usize,
    max_depth: usize,
}

impl<'a> Parser<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            bytes: source.as_bytes(),
            cursor: 0,
            line: 1,
            column: 1,
            depth: 0,
            max_depth: max_depth(),
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

    fn error(&self, message: impl Into<String>) -> Error {
        Error {
            message: message.into(),
            line: self.line,
            column: self.column,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.cursor).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.cursor += 1;
        if b == b'\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
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
            map.insert(key, value);
            self.skip_whitespace();
            match self.bump() {
                Some(b',') => continue,
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
            out.push(value);
            self.skip_whitespace();
            match self.bump() {
                Some(b',') => continue,
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
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'n' => out.push('\n'),
                        b't' => out.push('\t'),
                        b'r' => out.push('\r'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000c}'),
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
                                    Some(c) => out.push(c),
                                    None => return Err(self.error("invalid surrogate pair")),
                                }
                            } else if (0xDC00..=0xDFFF).contains(&cp) {
                                return Err(self.error("unpaired low surrogate"));
                            } else {
                                match char::from_u32(cp) {
                                    Some(c) => out.push(c),
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
                    let cp_len = utf8_first_byte_len(byte);
                    if self.cursor + cp_len > self.bytes.len() {
                        return Err(self.error("truncated UTF-8 in string"));
                    }
                    let slice = &self.bytes[self.cursor..self.cursor + cp_len];
                    let text = std::str::from_utf8(slice)
                        .map_err(|_| self.error("invalid UTF-8 in string"))?;
                    out.push_str(text);
                    for _ in 0..cp_len {
                        self.bump();
                    }
                }
            }
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
        let start = self.cursor;
        if self.peek() == Some(b'-') {
            self.bump();
        }
        while matches!(
            self.peek(),
            Some(b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
        ) {
            self.bump();
        }
        let text = std::str::from_utf8(&self.bytes[start..self.cursor])
            .map_err(|_| self.error("invalid UTF-8 in number"))?;
        // Integer-form tokens that fit `i64` are kept as `Int` so large
        // integers round-trip exactly and render without a trailing `.0`
        // (matching serde_json's number handling on the compiled tier).
        // Anything with a fractional part or exponent - or too large for
        // `i64` - falls back to `f64`.
        let is_float = text.bytes().any(|b| matches!(b, b'.' | b'e' | b'E'));
        if !is_float {
            if let Ok(n) = text.parse::<i64>() {
                return Ok(Value::Int(n));
            }
        }
        text.parse::<f64>()
            .map(Value::Number)
            .map_err(|_| self.error(format!("invalid number {text:?}")))
    }
}

fn utf8_first_byte_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b & 0xE0 == 0xC0 {
        2
    } else if b & 0xF0 == 0xE0 {
        3
    } else if b & 0xF8 == 0xF0 {
        4
    } else {
        1
    }
}

/// A close upper bound for compact JSON output. This moves the normal
/// `encode` path to one String allocation without changing rendering or the
/// public `Value` representation. Number bounds deliberately over-reserve
/// a few bytes; that is cheaper than formatting each number twice merely to
/// determine its exact length.
fn encoded_capacity_hint(value: &Value) -> usize {
    match value {
        Value::Null => 4,
        Value::Bool(false) => 5,
        Value::Bool(true) => 4,
        Value::Int(_) => 20,
        Value::Number(_) => 32,
        Value::String(s) => escaped_string_len(s),
        Value::Array(values) => values.iter().fold(2usize, |size, value| {
            size.saturating_add(encoded_capacity_hint(value))
                .saturating_add(1)
        }),
        Value::Object(map) => map.iter().fold(2usize, |size, (key, value)| {
            size.saturating_add(escaped_string_len(key))
                .saturating_add(1)
                .saturating_add(encoded_capacity_hint(value))
                .saturating_add(1)
        }),
    }
}

fn escaped_string_len(text: &str) -> usize {
    text.chars().fold(2usize, |size, ch| {
        let width = match ch {
            '"' | '\\' | '\n' | '\t' | '\r' | '\u{0008}' | '\u{000c}' => 2,
            '<' | '>' | '&' | '\u{2028}' | '\u{2029}' => 6,
            c if (c as u32) < 0x20 => 6,
            c => c.len_utf8(),
        };
        size.saturating_add(width)
    })
}

fn write_value(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(n) => {
            let _ = write!(out, "{n}");
        }
        Value::Number(n) => {
            // An integer-valued `f64` renders with a trailing `.0` so it
            // round-trips as a float (matching serde_json); a `Number`
            // reaching here always had a fractional part or exponent in the
            // source, so preserving the float shape is correct.
            if n.is_finite() && n.fract() == 0.0 {
                let _ = write!(out, "{n}.0");
            } else {
                let _ = write!(out, "{n}");
            }
        }
        Value::String(s) => write_string(out, s),
        Value::Array(values) => {
            out.push('[');
            for (i, v) in values.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, v);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (k, v)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, k);
                out.push(':');
                write_value(out, v);
            }
            out.push('}');
        }
    }
}

fn write_pretty(out: &mut String, value: &Value, indent: usize) {
    match value {
        Value::Array(values) if !values.is_empty() => {
            out.push('[');
            out.push('\n');
            for (i, v) in values.iter().enumerate() {
                push_indent(out, indent + 1);
                write_pretty(out, v, indent + 1);
                if i + 1 < values.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            push_indent(out, indent);
            out.push(']');
        }
        Value::Object(map) if !map.is_empty() => {
            out.push('{');
            out.push('\n');
            // BTreeMap already provides stable sorted iteration. Avoid
            // materialising every borrowed entry just to enumerate it while
            // rendering pretty JSON.
            for (i, (k, v)) in map.iter().enumerate() {
                push_indent(out, indent + 1);
                write_string(out, k);
                out.push_str(": ");
                write_pretty(out, v, indent + 1);
                if i + 1 < map.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            push_indent(out, indent);
            out.push('}');
        }
        _ => write_value(out, value),
    }
}

fn push_indent(out: &mut String, indent: usize) {
    for _ in 0..indent {
        out.push_str("  ");
    }
}

fn write_string(out: &mut String, text: &str) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000c}' => out.push_str("\\f"),
            // Escape the bytes that would let a JSON string break out of
            // a surrounding HTML `<script>` block (`</script>`) or be
            // read as a line terminator by a JavaScript parser
            // (U+2028/U+2029). All are valid JSON escapes that decode
            // back to the original character, so round-trips are exact.
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn write_value_to<W: std::io::Write>(out: &mut W, value: &Value) -> std::io::Result<()> {
    match value {
        Value::Null => out.write_all(b"null"),
        Value::Bool(b) => {
            if *b {
                out.write_all(b"true")
            } else {
                out.write_all(b"false")
            }
        }
        Value::Int(n) => write!(out, "{n}"),
        // A non-finite float has no JSON spelling, so it renders as the zero
        // the boxed form stores for it.
        Value::Number(n) if !n.is_finite() => write!(out, "0.0"),
        Value::Number(n) if n.fract() == 0.0 => write!(out, "{n}.0"),
        Value::Number(n) => write!(out, "{n}"),
        Value::String(s) => write_string_to(out, s),
        Value::Array(values) => {
            out.write_all(b"[")?;
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    out.write_all(b",")?;
                }
                write_value_to(out, value)?;
            }
            out.write_all(b"]")
        }
        Value::Object(map) => {
            out.write_all(b"{")?;
            for (index, (key, value)) in map.iter().enumerate() {
                if index != 0 {
                    out.write_all(b",")?;
                }
                write_string_to(out, key)?;
                out.write_all(b":")?;
                write_value_to(out, value)?;
            }
            out.write_all(b"}")
        }
    }
}

fn write_string_to<W: std::io::Write>(out: &mut W, text: &str) -> std::io::Result<()> {
    out.write_all(b"\"")?;
    for ch in text.chars() {
        match ch {
            '"' => out.write_all(b"\\\"")?,
            '\\' => out.write_all(b"\\\\")?,
            '\n' => out.write_all(b"\\n")?,
            '\t' => out.write_all(b"\\t")?,
            '\r' => out.write_all(b"\\r")?,
            '\u{0008}' => out.write_all(b"\\b")?,
            '\u{000c}' => out.write_all(b"\\f")?,
            '<' => out.write_all(b"\\u003c")?,
            '>' => out.write_all(b"\\u003e")?,
            '&' => out.write_all(b"\\u0026")?,
            '\u{2028}' => out.write_all(b"\\u2028")?,
            '\u{2029}' => out.write_all(b"\\u2029")?,
            c if (c as u32) < 0x20 => write!(out, "\\u{:04x}", c as u32)?,
            c => {
                let mut bytes = [0u8; 4];
                out.write_all(c.encode_utf8(&mut bytes).as_bytes())?;
            }
        }
    }
    out.write_all(b"\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_object() {
        let value = parse(r#"{"name":"gossamer","stars":42}"#).unwrap();
        assert_eq!(get(&value, "name").and_then(as_str), Some("gossamer"));
        assert_eq!(get(&value, "stars").and_then(as_i64), Some(42));
        let back = encode(&value);
        let again = parse(&back).unwrap();
        assert_eq!(value, again);
    }

    #[test]
    fn encode_escapes_script_breakout_and_round_trips() {
        let value = Value::String("</script><img src=x onerror=alert(1)>&".into());
        let encoded = encode(&value);
        // The script-terminating bytes are escaped, so the string is
        // safe to embed inside an inline <script> block.
        assert!(!encoded.contains('<'), "got {encoded}");
        assert!(!encoded.contains('>'), "got {encoded}");
        assert!(
            !encoded.contains('&') || encoded.contains("\\u0026"),
            "got {encoded}"
        );
        assert!(encoded.contains("\\u003c/script\\u003e"), "got {encoded}");
        // The escapes decode back to the original string exactly.
        assert_eq!(parse(&encoded).unwrap(), value);
    }

    #[test]
    fn encode_escapes_js_line_terminators() {
        let value = Value::String("a\u{2028}b\u{2029}c".into());
        let encoded = encode(&value);
        assert!(
            encoded.contains("\\u2028") && encoded.contains("\\u2029"),
            "got {encoded}"
        );
        assert_eq!(parse(&encoded).unwrap(), value);
    }

    #[test]
    fn streaming_decoder_yields_each_document() {
        let stream = b"{\"a\":1}\n{\"a\":2}\n{\"a\":3}\n".as_slice();
        let mut dec = Decoder::new(stream);
        let one = dec.decode().unwrap().unwrap();
        let two = dec.decode().unwrap().unwrap();
        let three = dec.decode().unwrap().unwrap();
        assert!(dec.decode().unwrap().is_none());
        assert_eq!(get(&one, "a").and_then(as_i64), Some(1));
        assert_eq!(get(&two, "a").and_then(as_i64), Some(2));
        assert_eq!(get(&three, "a").and_then(as_i64), Some(3));
    }

    #[test]
    fn streaming_decoder_handles_arrays_and_strings() {
        let stream = b"[1,2,3] \"hello\" 42 true null".as_slice();
        let mut dec = Decoder::new(stream);
        let arr = dec.decode().unwrap().unwrap();
        assert_eq!(as_array(&arr).unwrap().len(), 3);
        assert_eq!(dec.decode().unwrap(), Some(Value::String("hello".into())));
        assert_eq!(dec.decode().unwrap(), Some(Value::Int(42)));
        assert_eq!(dec.decode().unwrap(), Some(Value::Bool(true)));
        assert_eq!(dec.decode().unwrap(), Some(Value::Null));
        assert!(dec.decode().unwrap().is_none());
    }

    #[test]
    fn streaming_encoder_writes_ndjson() {
        let mut buf = Vec::new();
        {
            let mut enc = Encoder::new(&mut buf);
            enc.encode(&Value::Int(1)).unwrap();
            enc.encode(&Value::String("two".into())).unwrap();
        }
        assert_eq!(buf.as_slice(), b"1\n\"two\"\n".as_slice());
    }

    #[test]
    fn streaming_encoder_matches_compact_encoder_without_document_buffer() {
        let value = Value::Object(BTreeMap::from([
            ("unsafe".into(), Value::String("</script>&".into())),
            (
                "items".into(),
                Value::Array(vec![Value::Int(1), Value::Bool(false)]),
            ),
        ]));
        let mut bytes = Vec::new();
        Encoder::new(&mut bytes).encode(&value).unwrap();
        assert_eq!(bytes, format!("{}\n", encode(&value)).into_bytes());
    }

    #[test]
    fn streaming_decoder_reclaims_consumed_large_document_buffer() {
        let large = format!("\"{}\"\n1", "x".repeat(20 * 1024));
        let mut decoder = Decoder::new(large.as_bytes());
        assert_eq!(
            decoder.decode().unwrap(),
            Some(Value::String("x".repeat(20 * 1024)))
        );
        assert!(
            decoder.buffer.capacity() <= 4096,
            "consumed document buffer remained allocated: {} bytes",
            decoder.buffer.capacity()
        );
        assert_eq!(decoder.decode().unwrap(), Some(Value::Int(1)));
        assert!(decoder.decode().unwrap().is_none());
    }

    #[test]
    fn streaming_decoder_does_not_retain_whitespace_only_tail() {
        let whitespace = " \n\t\r".repeat(10 * 1024);
        let mut decoder = Decoder::new(whitespace.as_bytes());
        assert!(decoder.decode().unwrap().is_none());
        assert!(
            decoder.buffer.capacity() <= 4096,
            "whitespace tail remained allocated: {} bytes",
            decoder.buffer.capacity()
        );
    }

    #[test]
    fn field_tag_omit_empty_builder() {
        let tag = FieldTag::new("user_id", "userId").omit_empty();
        assert!(tag.omit_empty);
        assert_eq!(tag.json_name, "userId");
    }
}
