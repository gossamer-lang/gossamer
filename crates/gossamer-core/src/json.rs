//! JSON as the language defines it: the dynamic [`Value`], the grammar and
//! limits a document is read under, and the diagnostic a rejected one
//! reports. The bytecode VM parses into [`Value`] with [`parse`]; native
//! builds check a document with [`validate`] before materializing it, so every
//! tier accepts, rejects, and describes the same documents the same way.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use thiserror::Error;

pub mod typed;

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
    validate_with(bytes, None).map(|(source, _)| source)
}

/// Each object and array of a document: its opening offset and the offset
/// just past its close, in document order.
pub type ContainerSpans = Vec<(usize, usize)>;

/// [`validate`], also answering every object and array in the document as
/// its opening offset and the offset just past its close, in document order,
/// so a reader can step over a container without scanning it again.
///
/// # Errors
///
/// The error [`validate`] reports.
pub fn validate_with_containers(bytes: &[u8]) -> Result<(&str, ContainerSpans), Error> {
    validate_with(bytes, Some(Vec::new()))
        .map(|(source, containers)| (source, containers.unwrap_or_default()))
}

/// [`validate_with_containers`] for a document already known to be UTF-8.
///
/// # Errors
///
/// The error [`validate`] reports, apart from an encoding one.
pub fn validate_str_with_containers(source: &str) -> Result<ContainerSpans, Error> {
    check_size(source.as_bytes())?;
    validate_source(source, Some(Vec::new())).map(Option::unwrap_or_default)
}

/// What one [`TapeNode`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TapeKind {
    /// `null`.
    Null,
    /// `false`.
    False,
    /// `true`.
    True,
    /// An integer that fits an `i64`, its bits in [`TapeNode::word`].
    Int,
    /// An integer above `i64::MAX` that fits a `u64`, in [`TapeNode::word`].
    Uint,
    /// Any other number, the bits of its `f64` in [`TapeNode::word`].
    Float,
    /// A string with no escape: its text is the span inside the quotes.
    Str,
    /// A string holding at least one escape.
    StrEscaped,
    /// An array: [`TapeNode::extent`] and [`TapeNode::count`] apply.
    Array,
    /// An object: [`TapeNode::extent`] and [`TapeNode::count`] apply. Its
    /// members follow it as key, value pairs.
    Object,
}

const KINDS: [TapeKind; 10] = [
    TapeKind::Null,
    TapeKind::False,
    TapeKind::True,
    TapeKind::Int,
    TapeKind::Uint,
    TapeKind::Float,
    TapeKind::Str,
    TapeKind::StrEscaped,
    TapeKind::Array,
    TapeKind::Object,
];

/// Bits of [`TapeNode`]'s packed word that hold a value's byte length; the
/// kind sits above them.
const LEN_BITS: u32 = 28;

/// The longest document a [`Tape`] reads: every offset and length fits the
/// node's packed fields.
pub const TAPE_MAX_LEN: usize = (1 << LEN_BITS) - 1;

/// One value of a document read into a [`Tape`], in document order: a
/// container comes before everything it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TapeNode {
    /// Offset of the value's first byte in the document.
    pub start: u32,
    /// The kind (high bits) and the byte length (low [`LEN_BITS`] bits).
    packed: u32,
    /// A number's bits, or a container's extent (low half) and member or
    /// element count (high half).
    pub word: u64,
}

impl TapeNode {
    fn new(kind: TapeKind, start: u32, len: u32, word: u64) -> Self {
        Self {
            start,
            packed: ((kind as u32) << LEN_BITS) | len,
            word,
        }
    }

    /// What the value is.
    #[must_use]
    pub fn kind(&self) -> TapeKind {
        KINDS[(self.packed >> LEN_BITS) as usize]
    }

    /// Offset just past the value's last byte.
    #[must_use]
    pub fn end(&self) -> u32 {
        self.start + (self.packed & ((1 << LEN_BITS) - 1))
    }

    /// Index of the first node past this value and everything it holds.
    #[must_use]
    pub fn extent(&self, index: u32) -> u32 {
        match self.kind() {
            TapeKind::Array | TapeKind::Object => self.word as u32,
            _ => index + 1,
        }
    }

    /// Elements of an array, or members of an object (duplicates included).
    #[must_use]
    pub fn count(&self) -> u32 {
        (self.word >> 32) as u32
    }
}

/// A document's values in document order, built by the walk that checks it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tape {
    /// Every value, the root first.
    pub nodes: Vec<TapeNode>,
    /// The index of every object node, ascending.
    pub objects: Vec<u32>,
}

/// [`validate`], also reading the document into a [`Tape`] in the same walk.
/// A document longer than [`TAPE_MAX_LEN`] bytes answers no tape: its
/// offsets do not fit one.
///
/// # Errors
///
/// The error [`validate`] reports.
pub fn validate_to_tape(bytes: &[u8]) -> Result<(&str, Option<Tape>), Error> {
    check_size(bytes)?;
    let source = utf8_source(bytes)?;
    if source.len() > TAPE_MAX_LEN {
        validate_source(source, None)?;
        return Ok((source, None));
    }
    let mut parser = Parser::new(source);
    parser.build = false;
    // A value takes at least two bytes with its separator, and most
    // documents average several times that; capacity a document does not
    // fill is never touched.
    parser.tape = Some(Tape {
        nodes: Vec::with_capacity(source.len() / 8 + 1),
        objects: Vec::new(),
    });
    parser.skip_whitespace();
    parser.parse_value()?;
    parser.skip_whitespace();
    if parser.cursor < parser.bytes.len() {
        return Err(parser.error("trailing input"));
    }
    Ok((source, parser.tape))
}

/// The text of a string a [`Tape`] read, `raw` being its span inside the
/// quotes, with its escapes resolved.
#[must_use]
pub fn unescape_string(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let resolved = match chars.next() {
            Some('n') => '\n',
            Some('t') => '\t',
            Some('r') => '\r',
            Some('b') => '\u{0008}',
            Some('f') => '\u{000c}',
            Some('u') => {
                let hex4 = |chars: &mut std::str::Chars<'_>| {
                    let digits: String = chars.by_ref().take(4).collect();
                    u32::from_str_radix(&digits, 16).unwrap_or(0)
                };
                let cp = hex4(&mut chars);
                let scalar = if (0xD800..=0xDBFF).contains(&cp) {
                    chars.next();
                    chars.next();
                    let lo = hex4(&mut chars);
                    0x10000 + (((cp - 0xD800) << 10) | (lo.wrapping_sub(0xDC00) & 0x3FF))
                } else {
                    cp
                };
                char::from_u32(scalar).unwrap_or(char::REPLACEMENT_CHARACTER)
            }
            Some(other) => other,
            None => break,
        };
        out.push(resolved);
    }
    out
}

fn check_size(bytes: &[u8]) -> Result<(), Error> {
    let cap = max_size();
    if bytes.len() > cap {
        return Err(Error {
            message: format!("input exceeds max_size ({} > {cap})", bytes.len()),
            line: 1,
            column: 1,
        });
    }
    Ok(())
}

fn validate_with(
    bytes: &[u8],
    containers: Option<ContainerSpans>,
) -> Result<(&str, Option<ContainerSpans>), Error> {
    check_size(bytes)?;
    let source = utf8_source(bytes)?;
    validate_source(source, containers).map(|containers| (source, containers))
}

fn utf8_source(bytes: &[u8]) -> Result<&str, Error> {
    std::str::from_utf8(bytes).map_err(|error| {
        let prefix = &bytes[..error.valid_up_to()];
        let line = prefix.split(|b| *b == b'\n').count();
        let column = prefix.iter().rev().take_while(|b| **b != b'\n').count() + 1;
        Error {
            message: "invalid UTF-8".to_string(),
            line: u32::try_from(line).unwrap_or(u32::MAX),
            column: u32::try_from(column).unwrap_or(u32::MAX),
        }
    })
}

fn validate_source(
    source: &str,
    containers: Option<ContainerSpans>,
) -> Result<Option<ContainerSpans>, Error> {
    let mut parser = Parser::new(source);
    parser.build = false;
    parser.containers = containers;
    parser.skip_whitespace();
    parser.parse_value()?;
    parser.skip_whitespace();
    if parser.cursor < parser.bytes.len() {
        return Err(parser.error("trailing input"));
    }
    Ok(parser.containers)
}

struct Parser<'a> {
    bytes: &'a [u8],
    cursor: usize,
    depth: usize,
    max_depth: usize,
    /// Whether the parse keeps what it reads; [`validate`] walks the same
    /// grammar and keeps nothing.
    build: bool,
    /// When set, every object and array the walk reads, as its opening
    /// offset and the offset just past its close, in document order.
    containers: Option<ContainerSpans>,
    /// When set, every value the walk reads, in document order.
    tape: Option<Tape>,
    /// Whether the string the walk read last held an escape.
    escaped: bool,
}

impl<'a> Parser<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            bytes: source.as_bytes(),
            cursor: 0,
            depth: 0,
            max_depth: max_depth(),
            build: true,
            containers: None,
            tape: None,
            escaped: false,
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

    /// Records the container opening at the cursor, answering its slot.
    fn open_container(&mut self) -> Option<usize> {
        let containers = self.containers.as_mut()?;
        containers.push((self.cursor, self.cursor));
        Some(containers.len() - 1)
    }

    /// Records where the container in `slot` ended: just before the cursor.
    fn close_container(&mut self, slot: Option<usize>) {
        if let (Some(slot), Some(containers)) = (slot, self.containers.as_mut()) {
            containers[slot].1 = self.cursor;
        }
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
        let start = self.cursor;
        match self
            .peek()
            .ok_or_else(|| self.error("unexpected end of input"))?
        {
            b'{' => self.parse_object(),
            b'[' => self.parse_array(),
            b'"' => {
                let text = self.parse_string()?;
                self.record_string(start);
                Ok(Value::String(text))
            }
            b't' | b'f' => {
                let value = self.parse_bool()?;
                let kind = if value == Value::Bool(true) {
                    TapeKind::True
                } else {
                    TapeKind::False
                };
                self.record(kind, start, 0);
                Ok(value)
            }
            b'n' => {
                let value = self.parse_null()?;
                self.record(TapeKind::Null, start, 0);
                Ok(value)
            }
            b'-' | b'0'..=b'9' => self.parse_number(),
            other => Err(self.error(format!("unexpected byte {other:#x}"))),
        }
    }

    /// Records a value that ends at the cursor, when the walk keeps a tape,
    /// answering its index.
    fn record(&mut self, kind: TapeKind, start: usize, word: u64) -> Option<usize> {
        let end = self.cursor;
        let tape = self.tape.as_mut()?;
        let index = tape.nodes.len();
        if kind == TapeKind::Object {
            tape.objects.push(index as u32);
        }
        // `validate_to_tape` keeps a tape only for a document whose offsets
        // and lengths fit a node.
        tape.nodes.push(TapeNode::new(
            kind,
            start as u32,
            (end - start) as u32,
            word,
        ));
        Some(index)
    }

    /// Records the string just read, which opened at `start`.
    fn record_string(&mut self, start: usize) {
        let kind = if self.escaped {
            TapeKind::StrEscaped
        } else {
            TapeKind::Str
        };
        self.record(kind, start, 0);
    }

    /// Closes the container recorded at `slot`, which holds `count` members
    /// or elements and ends at the cursor.
    fn close_recorded(&mut self, slot: Option<usize>, count: u32) {
        let end = self.cursor as u32;
        if let (Some(slot), Some(tape)) = (slot, self.tape.as_mut()) {
            let extent = tape.nodes.len() as u64;
            let node = tape.nodes[slot];
            tape.nodes[slot] = TapeNode::new(
                node.kind(),
                node.start,
                end - node.start,
                extent | (u64::from(count) << 32),
            );
        }
    }

    fn parse_object(&mut self) -> Result<Value, Error> {
        self.enter()?;
        let recorded = self.open_container();
        let slot = self.record(TapeKind::Object, self.cursor, 0);
        self.bump();
        let mut map = BTreeMap::new();
        let mut count = 0u32;
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.bump();
            self.close_container(recorded);
            self.close_recorded(slot, count);
            self.leave();
            return Ok(Value::Object(map));
        }
        loop {
            self.skip_whitespace();
            let key_start = self.cursor;
            let key = self.parse_string()?;
            self.record_string(key_start);
            self.skip_whitespace();
            if self.bump() != Some(b':') {
                return Err(self.error("expected `:`"));
            }
            let value = self.parse_value()?;
            if self.build {
                map.insert(key, value);
            }
            count += 1;
            self.skip_whitespace();
            match self.bump() {
                Some(b',') => {}
                Some(b'}') => {
                    self.close_container(recorded);
                    self.close_recorded(slot, count);
                    self.leave();
                    return Ok(Value::Object(map));
                }
                _ => return Err(self.error("expected `,` or `}` in object")),
            }
        }
    }

    fn parse_array(&mut self) -> Result<Value, Error> {
        self.enter()?;
        let recorded = self.open_container();
        let slot = self.record(TapeKind::Array, self.cursor, 0);
        self.bump();
        let mut out = Vec::new();
        let mut count = 0u32;
        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.bump();
            self.close_container(recorded);
            self.close_recorded(slot, count);
            self.leave();
            return Ok(Value::Array(out));
        }
        loop {
            let value = self.parse_value()?;
            if self.build {
                out.push(value);
            }
            count += 1;
            self.skip_whitespace();
            match self.bump() {
                Some(b',') => {}
                Some(b']') => {
                    self.close_container(recorded);
                    self.close_recorded(slot, count);
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
        self.escaped = false;
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
                    self.escaped = true;
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
        if !self.build && self.tape.is_none() {
            // A number without an exponent and with at most 308 digits before
            // its point is below `f64::MAX`, so a validating walk has nothing
            // more to check in it.
            let token = &self.bytes[start..self.cursor];
            let has_exponent = token.iter().any(|b| matches!(b, b'e' | b'E'));
            let whole_digits = token
                .iter()
                .filter(|b| **b != b'-')
                .take_while(|b| **b != b'.')
                .count();
            if !has_exponent && whole_digits <= 308 {
                return Ok(Value::Null);
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.cursor])
            .map_err(|_| self.error("invalid UTF-8 in number"))?;
        // Integer-form tokens that fit `i64` are kept as `Int` so large
        // integers round-trip exactly and render without a trailing `.0`
        // (matching serde_json's number handling on the compiled tier).
        // An integer above `i64::MAX` that fits `u64` stays exact as `Uint`.
        // Anything with a fractional part or exponent - or too large for
        // `u64` - falls back to `f64`.
        let value = self.number_value(text)?;
        let (kind, word) = match value {
            Value::Int(n) => (TapeKind::Int, n as u64),
            Value::Uint(n) => (TapeKind::Uint, n),
            Value::Number(n) => (TapeKind::Float, n.to_bits()),
            _ => (TapeKind::Null, 0),
        };
        self.record(kind, start, word);
        Ok(value)
    }

    /// The value of the number token `text`.
    fn number_value(&self, text: &str) -> Result<Value, Error> {
        let is_float = text.bytes().any(|b| matches!(b, b'.' | b'e' | b'E'));
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
