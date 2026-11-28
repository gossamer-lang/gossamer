//! The typed reader `from_json::<T>` decodes through.
//!
//! The document is read in one pass from front to back, as `serde_json` and
//! Zig's `std.json` read into a type: nothing is built besides the decoded
//! value. Each step checks the text it consumes, and a value the walk passes
//! over is checked as it is skipped, so a read that succeeds has seen a
//! well-formed document. A read that fails, or finds text out of shape, is
//! settled against [`super::validate_str`], so a document that does not
//! parse reports the parser's error whatever it holds. A struct walks its object's members in document
//! order and hands each to the field its key names, an array walks its
//! elements, and a scalar is parsed where it stands. The reader keeps a frame
//! per open container with where its walk stands, so a value read leaves the
//! walk just past it and the next member is found without scanning again.
//! The values and failures match what `json::parse` followed by `json::get`
//! and the `json::as_*` accessors report, because the scanning is the same
//! parser and the conversions are the same rules.
//!
//! The reader's error is sticky. The first failure is kept, every later read
//! answers a zero value, and the decoder checks [`TypedReader::failed`] once at
//! the end. Each value the walk steps into remembers the key or index it was
//! reached by, so a failure carries the path to the value it names
//! (`current.items[2]`), spelled with the document's own keys.

use std::collections::BTreeSet;

use super::{Error, Parser, Value, integral_i64, integral_u64, validate_str};

/// A walk over one validated JSON document held as `T`: an owned `String`,
/// or text a caller keeps alive for the walk.
#[derive(Debug)]
pub struct TypedReader<T = String> {
    text: T,
    /// The values stepped into, the first `depth` of them live; the rest keep
    /// their buffers for the next step.
    frames: Vec<Frame>,
    depth: usize,
    error: Option<ReadError>,
    /// The key tables `begin_struct` has read: a decode reads the same few
    /// over and over, and a struct's walk names the one it reads by index.
    tables: Vec<Table>,
    /// What a key an object repeats reads as.
    duplicates: super::DuplicateKeys,
    /// Set when a step found text that is not well-formed JSON, so the end of
    /// the read checks the whole document.
    suspect: std::cell::Cell<bool>,
    /// Whether the document's shape is settled: checked against the parser,
    /// or refused before any of it was read.
    checked: bool,
    /// The stack the walk skipping a value keeps, reused from one to the next.
    stack: std::cell::RefCell<super::walk::Stack>,
}

/// A struct's field names as `begin_struct` takes them, and each name.
#[derive(Debug)]
struct Table {
    text: String,
    names: Vec<Box<[u8]>>,
}

/// The first failure and the path to the value it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadError {
    /// The failure's own message.
    pub message: String,
    /// Where the failure happened (`current.items[2]`), empty at the root.
    pub path: String,
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.path.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(f, "{}: {}", self.path, self.message)
        }
    }
}

/// How the walk reached a value: by the key whose opening quote sits at an
/// offset, or by an element index. The root and a value the document does not
/// have are reached by neither.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Segment {
    #[default]
    None,
    Key(usize),
    Index(usize),
}

/// One value the walk stands on: its byte offset, or `None` for a value the
/// document does not have (an element past the end), which reads as `null`.
#[derive(Debug, Default)]
struct Frame {
    node: Option<usize>,
    segment: Segment,
    /// Where a walk of this container stands: the offset just past the last
    /// member it read, or its opening bracket before the first.
    cursor: usize,
    /// The offset just past this value, once a walk has reached its end.
    end: Option<usize>,
    /// Members the walk has met: a struct's ordinal, an array's index.
    count: usize,
    /// Per declared field of the struct begun here: whether its key has come.
    seen: Vec<bool>,
    /// The field the struct's walk stands on.
    field: usize,
    /// The key table of the struct begun here.
    table: usize,
    /// The keys a map's walk has met, kept where a repeat is refused or
    /// skipped.
    keys: BTreeSet<String>,
    /// The key of the map entry the walk stands on.
    key: String,
    /// A tuple's element offsets.
    elems: Vec<usize>,
}

/// What kind of JSON value starts at an offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Null,
    Bool,
    Number,
    String,
    Array,
    Object,
}

impl TypedReader<String> {
    /// A reader over a copy of `text`. A document that does not validate
    /// leaves the reader failed with the error `json::parse` reports for it.
    #[must_use]
    pub fn new(text: &str) -> Self {
        Self::over(text.to_owned())
    }
}

impl<T: AsRef<str>> TypedReader<T> {
    /// A reader over `text`, validated as `json::parse` validates a document.
    #[must_use]
    pub fn over(text: T) -> Self {
        let mut reader = Self::unread(text);
        if let Err(error) = super::check_size(reader.text.as_ref().as_bytes()) {
            reader.reject(&error);
            return reader;
        }
        let root = skip_ws(reader.text.as_ref().as_bytes(), 0);
        reader.push(Some(root), Segment::None);
        reader
    }

    /// A reader over `text` that has read nothing yet.
    fn unread(text: T) -> Self {
        Self {
            text,
            frames: Vec::with_capacity(8),
            depth: 0,
            error: None,
            tables: Vec::new(),
            duplicates: super::DuplicateKeys::Error,
            suspect: std::cell::Cell::new(false),
            checked: false,
            stack: std::cell::RefCell::default(),
        }
    }

    /// A reader over a document already rejected with `error`, as for text
    /// that is not UTF-8.
    #[must_use]
    pub fn rejected(text: T, error: &Error) -> Self {
        let mut reader = Self::unread(text);
        reader.reject(error);
        reader
    }

    /// A reader that has failed with `message` before reading anything, as
    /// for a YAML or TOML document that did not convert.
    #[must_use]
    pub fn failing(text: T, message: &str) -> Self {
        let mut reader = Self::unread(text);
        reader.error = Some(ReadError {
            message: message.to_owned(),
            path: String::new(),
        });
        reader.checked = true;
        reader.push(None, Segment::None);
        reader
    }

    fn reject(&mut self, error: &Error) {
        self.error = Some(ReadError {
            message: error.to_string(),
            path: String::new(),
        });
        self.checked = true;
        self.push(None, Segment::None);
    }

    fn text(&self) -> &str {
        self.text.as_ref()
    }

    /// Whether a read has failed.
    #[must_use]
    pub fn failed(&self) -> bool {
        self.error.is_some()
    }

    /// The failure, when there is one.
    #[must_use]
    pub fn error(&self) -> Option<&ReadError> {
        self.error.as_ref()
    }

    /// Sets what a key an object repeats reads as: a failure naming it (the
    /// default), its first value, or its last.
    pub fn set_duplicate_keys(&mut self, duplicates: super::DuplicateKeys) {
        self.duplicates = duplicates;
    }

    /// Records `message` as the failure at the current value, unless one is
    /// already recorded.
    pub fn fail(&mut self, message: &str) {
        if self.error.is_none() {
            let path = self.path();
            self.error = Some(ReadError {
                message: message.to_owned(),
                path,
            });
        }
    }

    /// Records that the current value is not the `expected` shape, naming
    /// what it is instead.
    pub fn fail_expected(&mut self, expected: &str) {
        if self.error.is_none() {
            let found = self.found();
            self.fail(&format!("expected {expected}, found {found}"));
        }
    }

    /// Settles a document the walk found out of shape: the error the parser
    /// reports for it replaces any other, as a document that does not parse
    /// is refused whatever it holds. Answers whether the document failed to
    /// parse; a document that parses after all is left as it was.
    fn malformed(&mut self) -> bool {
        if std::mem::replace(&mut self.checked, true) {
            return false;
        }
        match validate_str(self.text()) {
            Ok(()) => false,
            Err(error) => {
                self.error = Some(ReadError {
                    message: error.to_string(),
                    path: String::new(),
                });
                true
            }
        }
    }

    /// Ends the read: the document must hold nothing past its value, and a
    /// read that failed is checked against the whole document, so a
    /// document that does not parse reports the parser's error rather than
    /// the first value that did not fit. Answers whether the read failed.
    pub fn finish(&mut self) -> bool {
        let trailing = self.frames.first().and_then(|root| {
            let end = root
                .end
                .or_else(|| root.node.map(|start| self.value_end(start)))?;
            let bytes = self.text().as_bytes();
            Some(skip_ws(bytes, end) != bytes.len())
        });
        if self.failed() || self.suspect.get() || trailing != Some(false) {
            let malformed = self.malformed();
            if !malformed && trailing == Some(true) && !self.failed() {
                self.fail("trailing characters after the value");
            }
        }
        self.failed()
    }

    /// The current value as a failure names it: a number by its text, any
    /// other value by its kind.
    fn found(&self) -> String {
        match self.kind() {
            Kind::Null => "null".to_owned(),
            Kind::Bool => "bool".to_owned(),
            Kind::String => "string".to_owned(),
            Kind::Array => "array".to_owned(),
            Kind::Object => "object".to_owned(),
            Kind::Number => self.number_text().unwrap_or("number").to_owned(),
        }
    }

    /// The keys and indices from the root to the current value.
    fn path(&self) -> String {
        let mut path = String::new();
        for frame in &self.frames[..self.depth] {
            match frame.segment {
                Segment::None => {}
                Segment::Index(index) => {
                    let _ = std::fmt::Write::write_fmt(&mut path, format_args!("[{index}]"));
                }
                Segment::Key(offset) => {
                    let key = self.key_at(offset);
                    let plain = !key.is_empty()
                        && key
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
                    if plain {
                        if !path.is_empty() {
                            path.push('.');
                        }
                        path.push_str(&key);
                    } else {
                        path.push('[');
                        write_quoted(&mut path, &key);
                        path.push(']');
                    }
                }
            }
        }
        path
    }

    /// The text of the key at node `offset`, escapes resolved.
    fn key_at(&self, offset: usize) -> std::borrow::Cow<'_, str> {
        let bytes = self.text().as_bytes();
        let end = string_end(bytes, offset).min(bytes.len());
        let raw = &self.text()[offset + 1..end.saturating_sub(1).max(offset + 1)];
        if raw.contains('\\') {
            let mut parser = Parser::new(self.text());
            parser.cursor = offset;
            parser
                .parse_string()
                .map_or(std::borrow::Cow::Borrowed(raw), std::borrow::Cow::Owned)
        } else {
            std::borrow::Cow::Borrowed(raw)
        }
    }

    /// Steps out of the value the last step stepped into, leaving the walk of
    /// the container that held it just past it.
    pub fn leave(&mut self) {
        if self.depth > 1 {
            let child = &self.frames[self.depth - 1];
            let end = match (child.end, child.node) {
                (Some(end), _) => Some(end),
                (None, Some(start)) if !self.failed() => Some(self.value_end(start)),
                _ => None,
            };
            self.depth -= 1;
            if let Some(end) = end {
                self.frames[self.depth - 1].cursor = end;
            }
        }
    }

    /// Whether the current value is `null` or absent.
    #[must_use]
    pub fn is_null(&self) -> bool {
        self.kind() == Kind::Null
    }

    /// Begins a walk of the current object for the fields `keys` names
    /// (encoded by [`encode_names`]). A value that is not an object is a
    /// failure.
    pub fn begin_struct(&mut self, keys: &str) {
        if self.failed() {
            return;
        }
        let Some(start) = self.object_start() else {
            self.fail_expected("object");
            return;
        };
        if !self.deep_enough() {
            return;
        }
        let table = if let Some(table) = self.tables.iter().rposition(|table| table.text == keys) {
            table
        } else {
            self.tables.push(Table {
                text: keys.to_owned(),
                names: table_ranges(keys)
                    .map(|(a, b)| keys.as_bytes()[a..b].into())
                    .collect(),
            });
            self.tables.len() - 1
        };
        let fields = self.tables[table].names.len();
        let top = self.top_mut();
        top.table = table;
        top.cursor = start + 1;
        top.count = 0;
        top.end = None;
        // No field read yet: the next one checked first is the first.
        top.field = usize::MAX;
        top.seen.clear();
        top.seen.resize(fields, false);
    }

    /// Steps into the next member of the struct [`Self::begin_struct`] began
    /// whose key names one of its fields, answering whether there is one; a
    /// member no field names is passed over. [`Self::field_index`] names the
    /// field. A key the object repeats reads by the duplicate-key policy: a
    /// failure, or its first or its last value.
    pub fn next_field(&mut self) -> bool {
        loop {
            if self.failed() {
                return false;
            }
            let Some(key_at) = self.next_member(b'}') else {
                return false;
            };
            // Fields are usually written in declaration order, so the field
            // after the last one read is checked where its key stands first.
            let ordinal = self.top().field.wrapping_add(1);
            let bytes = self.text().as_bytes();
            let expected = self.tables[self.top().table].names.get(ordinal);
            let (index, key_end) = match expected {
                Some(name)
                    if bytes.get(key_at + 1..key_at + 1 + name.len()) == Some(&name[..])
                        && bytes.get(key_at + 1 + name.len()) == Some(&b'"') =>
                {
                    (Some(ordinal), Some(key_at + name.len() + 2))
                }
                _ => (None, None),
            };
            let Some((key_end, value)) = self.key_and_value(key_at, key_end) else {
                return false;
            };
            let index = index.or_else(|| self.field_at(key_at, key_end, ordinal));
            self.top_mut().count += 1;
            let Some(index) = index else {
                let end = self.value_end(value);
                self.top_mut().cursor = end;
                continue;
            };
            if self.top().seen[index] {
                match self.duplicates {
                    super::DuplicateKeys::UseLast => {}
                    super::DuplicateKeys::UseFirst => {
                        let end = self.value_end(value);
                        self.top_mut().cursor = end;
                        continue;
                    }
                    super::DuplicateKeys::Error => {
                        self.fail_duplicate(index);
                        return false;
                    }
                }
            }
            let top = self.top_mut();
            top.seen[index] = true;
            top.field = index;
            self.push(Some(value), Segment::Key(key_at));
            return true;
        }
    }

    /// The field [`Self::next_field`] stepped into, by its place in the
    /// struct's keys.
    #[must_use]
    pub fn field_index(&self) -> usize {
        self.depth
            .checked_sub(2)
            .and_then(|at| self.frames.get(at))
            .map_or(0, |frame| frame.field)
    }

    /// Which of the begun struct's fields the key `key_at..key_end` names,
    /// when it is not the one written where the next field's stands.
    #[cold]
    #[inline(never)]
    fn field_at(&self, key_at: usize, key_end: usize, ordinal: usize) -> Option<usize> {
        let raw = &self.text().as_bytes()[key_at + 1..key_end - 1];
        if raw.contains(&b'\\') {
            let key = self.key_at(key_at);
            self.field_named(key.as_bytes(), ordinal)
        } else {
            self.field_named(raw, ordinal)
        }
    }

    /// Records the failure for a repeat of the begun struct's field `index`.
    #[cold]
    #[inline(never)]
    fn fail_duplicate(&mut self, index: usize) {
        let table = &self.tables[self.top().table];
        let name = String::from_utf8_lossy(&table.names[index]).into_owned();
        self.fail(&format!("duplicate key `{name}`"));
    }

    /// Which of the begun struct's fields `key` names. Members are usually
    /// written in declaration order, so the name at the member's own position
    /// is tried first.
    fn field_named(&self, key: &[u8], ordinal: usize) -> Option<usize> {
        let names = &self.tables[self.top().table].names;
        if names.get(ordinal).is_some_and(|name| **name == *key) {
            return Some(ordinal);
        }
        names.iter().position(|name| **name == *key)
    }

    /// The current value as an `i64`, by `json::as_i64`'s rule, or `0` with
    /// a failure naming `expected` recorded when it is not one.
    pub fn read_i64(&mut self, expected: &str) -> i64 {
        let Some(n) = self.scalar().as_ref().and_then(as_i64) else {
            self.fail_expected(expected);
            return 0;
        };
        n
    }

    /// The current value as an integer within `min..=max`, by
    /// `json::as_i64`'s rule, for a field narrower than `i64`.
    pub fn read_int(&mut self, expected: &str, min: i64, max: i64) -> i64 {
        match self.scalar().as_ref().and_then(as_i64) {
            Some(n) if (min..=max).contains(&n) => n,
            _ => {
                self.fail_expected(expected);
                0
            }
        }
    }

    /// The current value as a `u64`, by `json::as_u64`'s rule.
    pub fn read_u64(&mut self, expected: &str) -> u64 {
        let Some(n) = self.scalar().as_ref().and_then(as_u64) else {
            self.fail_expected(expected);
            return 0;
        };
        n
    }

    /// The current value as the bits of a 128-bit integer: a number, or a
    /// string of decimal digits, read as a `u128` when `unsigned` and an
    /// `i128` otherwise. `0` with a failure naming `expected` recorded when it
    /// is neither, or does not fit.
    pub fn read_wide(&mut self, expected: &str, unsigned: bool) -> u128 {
        let text = match self.kind() {
            Kind::Number => self.number_text().map(str::to_owned),
            Kind::String => match self.scalar() {
                Some(Value::String(s)) => Some(s),
                _ => None,
            },
            _ => None,
        };
        let bits = text.and_then(|t| {
            if unsigned {
                t.parse::<u128>().ok()
            } else {
                t.parse::<i128>().ok().map(i128::cast_unsigned)
            }
        });
        bits.unwrap_or_else(|| {
            self.fail_expected(expected);
            0
        })
    }

    /// The current value as an `f64`, by `json::as_f64`'s rule.
    pub fn read_f64(&mut self, expected: &str) -> f64 {
        // A number token reads as the nearest `f64` whether it is written
        // as an integer or not, which is what `as_f64` makes of each form.
        let number = match self.node() {
            _ if self.failed() => None,
            Some(start) if self.kind() == Kind::Number => {
                // Each form converts to its nearest `f64`, as parsing the
                // token's text as one does.
                match super::walk::number_at(self.text(), start) {
                    Some((kind, word, end)) => {
                        self.top_mut().end = Some(end);
                        Some(match kind {
                            super::TapeKind::Int => word.cast_signed() as f64,
                            super::TapeKind::Uint => word as f64,
                            _ => f64::from_bits(word),
                        })
                    }
                    None if self.malformed() => None,
                    None => self.number_text().and_then(|text| text.parse::<f64>().ok()),
                }
            }
            _ => None,
        };
        let Some(n) = number else {
            self.fail_expected(expected);
            return 0.0;
        };
        n
    }

    /// The text of the number token at the current value. A token the fast
    /// reader does not take whole leaves the read to be checked against the
    /// parser at its end.
    fn number_text(&self) -> Option<&str> {
        let start = self.node()?;
        let end = scan_value_end(self.text().as_bytes(), start);
        if super::walk::number_at(self.text(), start).map(|(_, _, at)| at) != Some(end) {
            self.suspect.set(true);
        }
        self.text().get(start..end)
    }

    /// The current value as a `bool`.
    pub fn read_bool(&mut self, expected: &str) -> bool {
        let Some(Value::Bool(b)) = self.scalar() else {
            self.fail_expected(expected);
            return false;
        };
        b
    }

    /// The current value as a `char`: a string holding exactly one.
    pub fn read_char(&mut self, expected: &str) -> char {
        let text = self.read_string(expected);
        let mut chars = text.chars();
        let (Some(c), None) = (chars.next(), chars.next()) else {
            if !self.failed() {
                let count = text.chars().count();
                self.fail(&format!(
                    "expected {expected}, found {count}-character string"
                ));
            }
            return '\0';
        };
        c
    }

    /// The current value as a string.
    pub fn read_string(&mut self, expected: &str) -> String {
        self.read_str(expected).into_owned()
    }

    /// [`Self::read_string`] without a copy for a string that has no escape:
    /// its text is the span of the document between its quotes.
    pub fn read_str(&mut self, expected: &str) -> std::borrow::Cow<'_, str> {
        if self.kind() != Kind::String {
            self.fail_expected(expected);
            return std::borrow::Cow::Borrowed("");
        }
        let Some(start) = self.node() else {
            return std::borrow::Cow::Borrowed("");
        };
        let Some(end) = super::walk::string_end(self.text(), start) else {
            self.suspect.set(true);
            return std::borrow::Cow::Borrowed("");
        };
        self.top_mut().end = Some(end);
        let raw = &self.text()[start + 1..end - 1];
        if !raw.contains('\\') {
            return std::borrow::Cow::Borrowed(raw);
        }
        let mut parser = Parser::new(self.text());
        parser.cursor = start;
        std::borrow::Cow::Owned(parser.parse_string().unwrap_or_default())
    }

    /// The text of the current value, `null` when it is absent, for a field
    /// that keeps the document as `json::Value`.
    #[must_use]
    pub fn raw_value(&self) -> &str {
        match self.node() {
            Some(start) if !self.failed() => {
                let end = self.value_end(start);
                &self.text()[start..end]
            }
            _ => "null",
        }
    }

    /// Begins a walk of the current array's elements, recording a failure
    /// naming `expected` when it is not an array.
    pub fn begin_array(&mut self, expected: &str) {
        if self.failed() {
            return;
        }
        if self.kind() != Kind::Array {
            self.fail_expected(expected);
            return;
        }
        let Some(start) = self.node() else {
            return;
        };
        if !self.deep_enough() {
            return;
        }
        let top = self.top_mut();
        top.cursor = start + 1;
        top.count = 0;
        top.end = None;
    }

    /// Steps into the next element of the array [`Self::begin_array`] began,
    /// answering whether there is one.
    pub fn next_elem(&mut self) -> bool {
        if self.failed() {
            return false;
        }
        let Some(at) = self.next_member(b']') else {
            return false;
        };
        let top = self.top_mut();
        let index = top.count;
        top.count += 1;
        top.cursor = at;
        self.push(Some(at), Segment::Index(index));
        true
    }

    /// Checks that the current value is an array of exactly `len` elements,
    /// recording a failure naming `not_array` when it is not an array and
    /// `wrong_len` when it has another length.
    pub fn begin_tuple(&mut self, len: usize, not_array: &str, wrong_len: &str) {
        if self.failed() {
            return;
        }
        if self.kind() != Kind::Array {
            self.fail_expected(not_array);
            return;
        }
        let mut elems = std::mem::take(&mut self.top_mut().elems);
        self.current_elems(&mut elems);
        let found = elems.len();
        self.top_mut().elems = elems;
        if found != len {
            self.fail(&format!(
                "expected {wrong_len}, found {found}-element array"
            ));
        }
    }

    /// Steps into element `index` of the array [`Self::begin_array`] or
    /// [`Self::begin_tuple`] began.
    pub fn enter_elem(&mut self, index: usize) {
        let node = if self.failed() {
            None
        } else {
            self.top().elems.get(index).copied()
        };
        self.push(node, Segment::Index(index));
    }

    /// Steps into element `index` of the current value, or into `null` when
    /// it is not an array or is shorter, as `json::at` reads it.
    pub fn enter_at(&mut self, index: usize) {
        let node = if !self.failed() && self.kind() == Kind::Array {
            let mut elems = Vec::new();
            self.current_elems(&mut elems);
            elems.get(index).copied()
        } else {
            None
        };
        self.push(node, Segment::Index(index));
    }

    /// Begins a walk of the current object's entries, recording a failure
    /// naming `expected` when it is not an object.
    pub fn begin_map(&mut self, expected: &str) {
        if self.failed() {
            return;
        }
        let Some(start) = self.object_start() else {
            self.fail_expected(expected);
            return;
        };
        if !self.deep_enough() {
            return;
        }
        let top = self.top_mut();
        top.cursor = start + 1;
        top.count = 0;
        top.end = None;
        top.keys.clear();
    }

    /// Steps into the value of the next entry of the map [`Self::begin_map`]
    /// began, answering whether there is one; [`Self::entry_key`] is its key.
    /// Entries come in document order. A key the object repeats reads by the
    /// duplicate-key policy: a failure, its first value, or its last, which a
    /// later insert of the same key leaves in place.
    pub fn next_entry(&mut self) -> bool {
        loop {
            if self.failed() {
                return false;
            }
            let Some(key_at) = self.next_member(b'}') else {
                return false;
            };
            let Some((_, value)) = self.key_and_value(key_at, None) else {
                return false;
            };
            self.top_mut().count += 1;
            let key = self.key_at(key_at).into_owned();
            if self.duplicates != super::DuplicateKeys::UseLast
                && !self.top_mut().keys.insert(key.clone())
            {
                if self.duplicates == super::DuplicateKeys::Error {
                    self.fail(&format!("duplicate key `{key}`"));
                    return false;
                }
                let end = self.value_end(value);
                self.top_mut().cursor = end;
                continue;
            }
            self.top_mut().key = key;
            self.push(Some(value), Segment::Key(key_at));
            return true;
        }
    }

    /// The key of the entry [`Self::next_entry`] stepped into.
    #[must_use]
    pub fn entry_key(&self) -> &str {
        self.depth
            .checked_sub(2)
            .and_then(|at| self.frames.get(at))
            .map_or("", |frame| frame.key.as_str())
    }

    /// Selects the variant of an externally tagged enum. `variants` lists
    /// every variant in declaration order (encoded by [`encode_variants`]),
    /// `name` is the enum's name for the messages, and `fallback` is the
    /// variant to answer once the reader has failed.
    ///
    /// A string names a unit variant. An object with one key names a variant
    /// with a payload, and the reader steps into that payload; it steps into
    /// nothing for a unit variant. Every other shape, and a name the enum does
    /// not have in that form, is a failure.
    pub fn begin_enum(&mut self, variants: &str, name: &str, fallback: usize) -> usize {
        let table = decode_variants(variants);
        let chosen = if self.failed() {
            None
        } else {
            self.select_variant(&table, name)
        };
        if let Some((index, payload)) = chosen {
            if let Some((key, payload)) = payload {
                self.push(Some(payload), Segment::Key(key));
            }
            return index;
        }
        if table.get(fallback).is_some_and(|(unit, _)| !unit) {
            self.push(None, Segment::None);
        }
        fallback
    }

    /// Selects the variant of an internally tagged enum: the current value is
    /// an object whose `tag` member names the variant, and the variant's own
    /// fields sit beside it in the same object. Nothing is stepped into.
    /// `variants`, `name`, and `fallback` are as [`Self::begin_enum`] takes
    /// them.
    pub fn begin_tagged(
        &mut self,
        variants: &str,
        tag: &str,
        name: &str,
        fallback: usize,
    ) -> usize {
        if self.failed() {
            return fallback;
        }
        let Some(start) = self.object_start() else {
            self.fail_expected(&format!("an object naming a variant of `{name}`"));
            return fallback;
        };
        let mut member = None;
        self.for_each_entry(start, |key, key_at, value| {
            if key == tag.as_bytes() {
                member = Some((key_at, value));
            }
        });
        let Some((key_at, value)) = member else {
            self.fail(&format!("missing field `{tag}`"));
            return fallback;
        };
        self.push(Some(value), Segment::Key(key_at));
        let found = self.read_string("a variant name");
        let table = decode_variants(variants);
        let index = if self.failed() {
            None
        } else {
            let index = table.iter().position(|(_, n)| *n == found);
            if index.is_none() {
                self.fail(&unknown_variant(&found, name, &table, |_| true));
            }
            index
        };
        self.leave();
        index.unwrap_or(fallback)
    }

    fn select_variant(
        &mut self,
        table: &[(bool, &str)],
        name: &str,
    ) -> Option<(usize, Option<(usize, usize)>)> {
        match self.kind() {
            Kind::String => {
                let tag = self.read_string("");
                let found = table.iter().position(|(unit, n)| *unit && *n == tag);
                if found.is_none() {
                    self.fail(&unknown_variant(&tag, name, table, |unit| unit));
                }
                found.map(|index| (index, None))
            }
            Kind::Object => {
                let start = self.object_start()?;
                // Distinct keys, each with its last value, as `json::keys`
                // and `json::get` read the object.
                let mut entries: Vec<(String, usize, usize)> = Vec::new();
                self.for_each_entry(start, |key, key_at, value| {
                    match entries.iter_mut().find(|(k, _, _)| k.as_bytes() == key) {
                        Some(slot) => (slot.1, slot.2) = (key_at, value),
                        None => {
                            entries.push((
                                String::from_utf8_lossy(key).into_owned(),
                                key_at,
                                value,
                            ));
                        }
                    }
                });
                if entries.len() != 1 {
                    self.fail_expected(&shape_message(name));
                    return None;
                }
                let (tag, key_at, payload) = entries.swap_remove(0);
                let found = table.iter().position(|(unit, n)| !*unit && *n == tag);
                if found.is_none() {
                    self.fail(&unknown_variant(&tag, name, table, |unit| !unit));
                }
                found.map(|index| (index, Some((key_at, payload))))
            }
            _ => {
                self.fail_expected(&shape_message(name));
                None
            }
        }
    }

    fn top(&self) -> &Frame {
        // `over` and `rejected` push the root frame, and `leave` never steps
        // out of it.
        self.frames[..self.depth].last().unwrap_or(&ROOT_ABSENT)
    }

    fn top_mut(&mut self) -> &mut Frame {
        if self.depth == 0 {
            self.push(None, Segment::None);
        }
        &mut self.frames[self.depth - 1]
    }

    fn push(&mut self, node: Option<usize>, segment: Segment) {
        if let Some(frame) = self.frames.get_mut(self.depth) {
            frame.node = node;
            frame.segment = segment;
            frame.end = None;
        } else {
            self.push_new_frame(node, segment);
        }
        self.depth += 1;
    }

    /// Grows the frame stack by one, which a walk does once per depth it
    /// first reaches.
    #[cold]
    #[inline(never)]
    fn push_new_frame(&mut self, node: Option<usize>, segment: Segment) {
        self.frames.push(Frame {
            node,
            segment,
            ..Frame::default()
        });
    }

    fn node(&self) -> Option<usize> {
        self.top().node
    }

    fn kind(&self) -> Kind {
        if self.failed() {
            return Kind::Null;
        }
        let Some(node) = self.node() else {
            return Kind::Null;
        };
        kind_at(self.text().as_bytes(), node)
    }

    fn object_start(&self) -> Option<usize> {
        (self.kind() == Kind::Object).then(|| self.node()).flatten()
    }

    /// The current scalar, parsed where it stands; `None` for a container or
    /// an absent value. A scalar read records where it ends, so stepping out
    /// of it scans nothing again.
    fn scalar(&mut self) -> Option<Value> {
        if self.failed() {
            return None;
        }
        let (value, end) = match self.kind() {
            Kind::Null => {
                let start = self.node();
                if let Some(start) = start
                    && self.text().as_bytes().get(start..start + 4) != Some(b"null")
                {
                    self.suspect.set(true);
                    return None;
                }
                (Some(Value::Null), start.map(|start| start + 4))
            }
            Kind::Array | Kind::Object => return None,
            Kind::Number => {
                let start = self.node()?;
                match super::walk::number_at(self.text(), start) {
                    Some((super::TapeKind::Int, word, end)) => {
                        (Some(Value::Int(word.cast_signed())), Some(end))
                    }
                    Some((super::TapeKind::Uint, word, end)) => {
                        (Some(Value::Uint(word)), Some(end))
                    }
                    Some((_, word, end)) => (Some(Value::Number(f64::from_bits(word))), Some(end)),
                    // The fast reader declines a number out of its shape or
                    // range; one the document holds after all is read from
                    // its text.
                    None => {
                        if self.malformed() {
                            return None;
                        }
                        let end = scan_value_end(self.text().as_bytes(), start);
                        (number_value(&self.text()[start..end]), Some(end))
                    }
                }
            }
            Kind::Bool | Kind::String => {
                let start = self.node()?;
                let mut parser = Parser::new(self.text());
                parser.cursor = start;
                let Ok(value) = parser.parse_value() else {
                    self.suspect.set(true);
                    return None;
                };
                (Some(value), Some(parser.cursor))
            }
        };
        if end.is_some() {
            self.top_mut().end = end;
        }
        value
    }

    /// The element offsets of the current array, into `elems`.
    fn current_elems(&self, elems: &mut Vec<usize>) {
        elems.clear();
        let Some(start) = self.node() else {
            return;
        };
        let bytes = self.text().as_bytes();
        let mut cursor = skip_ws(bytes, start + 1);
        if bytes.get(cursor) == Some(&b']') {
            return;
        }
        loop {
            cursor = skip_ws(bytes, cursor);
            elems.push(cursor);
            cursor = skip_ws(bytes, self.value_end(cursor));
            match bytes.get(cursor) {
                Some(b',') => cursor += 1,
                Some(b']') => return,
                _ => {
                    self.suspect.set(true);
                    return;
                }
            }
        }
    }

    /// Calls `f` with each key of the object at `start`, the offset of the
    /// key's opening quote, and the offset of its value, in document order.
    fn for_each_entry(&self, start: usize, mut f: impl FnMut(&[u8], usize, usize)) {
        let bytes = self.text().as_bytes();
        let mut cursor = skip_ws(bytes, start + 1);
        if bytes.get(cursor) == Some(&b'}') {
            return;
        }
        loop {
            let key_start = skip_ws(bytes, cursor);
            let Some((key_end, value)) = self.key_and_value(key_start, None) else {
                return;
            };
            let raw = &bytes[key_start + 1..key_end - 1];
            // A key without an escape is its own bytes; one with an escape is
            // read the way the parser reads any string.
            let unescaped;
            let key = if raw.contains(&b'\\') {
                let mut parser = Parser::new(self.text());
                parser.cursor = key_start;
                let Ok(text) = parser.parse_string() else {
                    self.suspect.set(true);
                    return;
                };
                unescaped = text;
                unescaped.as_bytes()
            } else {
                raw
            };
            f(key, key_start, value);
            cursor = skip_ws(bytes, self.value_end(value));
            match bytes.get(cursor) {
                Some(b',') => cursor += 1,
                Some(b'}') => return,
                _ => {
                    self.suspect.set(true);
                    return;
                }
            }
        }
    }

    /// The offset just past the value starting at `start`, found by scanning
    /// it.
    fn value_end(&self, start: usize) -> usize {
        let mut stack = self.stack.borrow_mut();
        super::walk::value_end(self.text(), start, super::max_depth(), &mut stack).unwrap_or_else(
            || {
                self.suspect.set(true);
                self.text().len()
            },
        )
    }

    /// Where the next member of the container being walked starts, or `None`
    /// at its end, `close`, which the walk steps past; a comma separates
    /// members and none trails the last. Text out of that shape ends the walk
    /// and leaves the read to be checked whole.
    fn next_member(&mut self, close: u8) -> Option<usize> {
        let bytes = self.text().as_bytes();
        let top = self.top();
        let mut at = skip_ws(bytes, top.cursor);
        if top.count > 0 {
            match bytes.get(at) {
                Some(b',') => at = skip_ws(bytes, at + 1),
                Some(&byte) if byte == close => {
                    self.top_mut().end = Some(at + 1);
                    return None;
                }
                _ => {
                    self.suspect.set(true);
                    return None;
                }
            }
        } else if bytes.get(at) == Some(&close) {
            self.top_mut().end = Some(at + 1);
            return None;
        }
        if at >= bytes.len() || bytes[at] == close {
            self.suspect.set(true);
            return None;
        }
        Some(at)
    }

    /// The offset just past the key whose quote opens at `key_at`, and the
    /// offset of its value after the `:`; `None` for text out of that shape.
    fn key_and_value(&self, key_at: usize, key_end: Option<usize>) -> Option<(usize, usize)> {
        let bytes = self.text().as_bytes();
        if bytes.get(key_at) != Some(&b'"') {
            self.suspect.set(true);
            return None;
        }
        let key_end = key_end.or_else(|| super::walk::string_end(self.text(), key_at));
        let Some(key_end) = key_end else {
            self.suspect.set(true);
            return None;
        };
        let colon = skip_ws(bytes, key_end);
        if bytes.get(colon) != Some(&b':') {
            self.suspect.set(true);
            return None;
        }
        Some((key_end, skip_ws(bytes, colon + 1)))
    }

    /// Whether a container may open at the current depth.
    fn deep_enough(&self) -> bool {
        if self.depth > super::max_depth() {
            self.suspect.set(true);
            return false;
        }
        true
    }
}

static ROOT_ABSENT: Frame = Frame {
    node: None,
    segment: Segment::None,
    cursor: 0,
    end: None,
    count: 0,
    seen: Vec::new(),
    field: 0,
    table: 0,
    keys: BTreeSet::new(),
    key: String::new(),
    elems: Vec::new(),
};

fn shape_message(name: &str) -> String {
    format!("a variant of `{name}` (a name, or an object with one key)")
}

/// The failure for a variant name `tag` that `name` does not have in the form
/// `form` accepts, listing the names it does.
fn unknown_variant(
    tag: &str,
    name: &str,
    table: &[(bool, &str)],
    form: impl Fn(bool) -> bool,
) -> String {
    let known: Vec<String> = table
        .iter()
        .filter(|(unit, _)| form(*unit))
        .map(|(_, n)| format!("`{n}`"))
        .collect();
    if known.is_empty() {
        format!("unknown variant `{tag}` of `{name}`")
    } else {
        format!(
            "unknown variant `{tag}` of `{name}`, expected one of {}",
            known.join(", ")
        )
    }
}

/// `compact` (JSON text with no whitespace outside strings, as the typed
/// writer writes it) laid out as `json::encode_pretty` lays out a document:
/// two spaces per level, `": "` after a key, and `[]` / `{}` for an empty
/// container. Members keep their order.
#[must_use]
pub fn indent_text(compact: &str) -> String {
    let bytes = compact.as_bytes();
    let mut out = String::with_capacity(compact.len() * 2);
    let mut depth = 0usize;
    let mut at = 0;
    let newline = |out: &mut String, depth: usize| {
        out.push('\n');
        for _ in 0..depth {
            out.push_str("  ");
        }
    };
    while at < bytes.len() {
        match bytes[at] {
            b'"' => {
                let end = string_end(bytes, at);
                out.push_str(&compact[at..end]);
                at = end;
                continue;
            }
            open @ (b'{' | b'[') => {
                let close = if open == b'{' { b'}' } else { b']' };
                if bytes.get(at + 1) == Some(&close) {
                    out.push(char::from(open));
                    out.push(char::from(close));
                    at += 2;
                    continue;
                }
                out.push(char::from(open));
                depth += 1;
                newline(&mut out, depth);
            }
            close @ (b'}' | b']') => {
                depth = depth.saturating_sub(1);
                newline(&mut out, depth);
                out.push(char::from(close));
            }
            b',' => {
                out.push(',');
                newline(&mut out, depth);
            }
            b':' => out.push_str(": "),
            _ => {
                let end = scan_value_end(bytes, at);
                out.push_str(&compact[at..end]);
                at = end;
                continue;
            }
        }
        at += 1;
    }
    out
}

/// Appends an object member's key text to `out`, after a `,` unless the
/// member is the first of the object being written.
pub fn write_key(out: &mut Vec<u8>, text: &[u8]) {
    if out.last().is_some_and(|&b| b != b'{') {
        out.push(b',');
    }
    out.extend_from_slice(text);
}

/// Appends `text` as a JSON string literal.
fn write_quoted(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => {
                let _ = std::fmt::Write::write_fmt(out, format_args!("\\u{:04x}", u32::from(c)));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn kind_at(bytes: &[u8], start: usize) -> Kind {
    match bytes.get(start) {
        Some(b'{') => Kind::Object,
        Some(b'[') => Kind::Array,
        Some(b'"') => Kind::String,
        Some(b't' | b'f') => Kind::Bool,
        Some(b'n') | None => Kind::Null,
        Some(_) => Kind::Number,
    }
}

fn skip_ws(bytes: &[u8], mut cursor: usize) -> usize {
    while matches!(bytes.get(cursor), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        cursor += 1;
    }
    cursor
}

/// The offset just past the value starting at `start` in a validated
/// document, found by scanning it.
fn scan_value_end(bytes: &[u8], start: usize) -> usize {
    match bytes.get(start) {
        Some(b'"') => string_end(bytes, start),
        Some(open @ (b'{' | b'[')) => {
            let close = if *open == b'{' { b'}' } else { b']' };
            let mut depth = 0usize;
            let mut cursor = start;
            loop {
                cursor = super::scan::find(
                    bytes,
                    cursor,
                    |w| {
                        super::scan::equal(w, b'"')
                            | super::scan::equal(w, b'[')
                            | super::scan::equal(w, b'{')
                            | super::scan::equal(w, b']')
                            | super::scan::equal(w, b'}')
                    },
                    |b| matches!(b, b'"' | b'{' | b'[' | b'}' | b']'),
                );
                let Some(&byte) = bytes.get(cursor) else {
                    return cursor;
                };
                match byte {
                    b'"' => {
                        cursor = string_end(bytes, cursor);
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            debug_assert_eq!(byte, close);
                            return cursor + 1;
                        }
                    }
                    _ => {}
                }
                cursor += 1;
            }
        }
        _ => super::scan::find(
            bytes,
            start,
            |w| {
                super::scan::equal(w, b',')
                    | super::scan::equal(w, b'}')
                    | super::scan::equal(w, b']')
                    | super::scan::equal(w, b' ')
                    | super::scan::equal(w, b'\t')
                    | super::scan::equal(w, b'\n')
                    | super::scan::equal(w, b'\r')
            },
            |b| matches!(b, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r'),
        ),
    }
}

/// The offset just past the string whose opening quote is at `start`.
fn string_end(bytes: &[u8], start: usize) -> usize {
    let mut cursor = start + 1;
    loop {
        cursor = super::scan::find(
            bytes,
            cursor,
            |w| super::scan::equal(w, b'"') | super::scan::equal(w, b'\\'),
            |b| matches!(b, b'"' | b'\\'),
        );
        let Some(&byte) = bytes.get(cursor) else {
            return cursor;
        };
        if byte == b'"' {
            return cursor + 1;
        }
        // An escape: the byte after it is never the closing quote.
        cursor += 2;
    }
}

fn as_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Int(n) => Some(*n),
        Value::Number(n) => integral_i64(*n),
        _ => None,
    }
}

fn as_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Int(n) => u64::try_from(*n).ok(),
        Value::Uint(n) => Some(*n),
        Value::Number(n) => integral_u64(*n),
        _ => None,
    }
}

/// Encodes `names` as one string a decoder passes to [`TypedReader::begin_struct`]:
/// each name as its byte length, a `:`, and its bytes.
#[must_use]
pub fn encode_names<'a>(names: impl IntoIterator<Item = &'a str>) -> String {
    let mut out = String::new();
    for name in names {
        out.push_str(&name.len().to_string());
        out.push(':');
        out.push_str(name);
    }
    out
}

/// Encodes an enum's variants for [`TypedReader::begin_enum`]: each as `u` for
/// a unit variant or `p` for one with a payload, then its name as
/// [`encode_names`] spells one.
#[must_use]
pub fn encode_variants<'a>(variants: impl IntoIterator<Item = (bool, &'a str)>) -> String {
    let mut out = String::new();
    for (unit, name) in variants {
        out.push(if unit { 'u' } else { 'p' });
        out.push_str(&name.len().to_string());
        out.push(':');
        out.push_str(name);
    }
    out
}

/// Where each name sits in a table [`encode_names`] wrote, in order.
fn table_ranges(encoded: &str) -> impl Iterator<Item = (usize, usize)> + '_ {
    let mut cursor = 0usize;
    std::iter::from_fn(move || {
        let rest = encoded.get(cursor..)?;
        let (len, _) = rest.split_once(':')?;
        let width = len.len() + 1;
        let len = len.parse::<usize>().ok()?;
        let start = cursor + width;
        let end = start + len;
        encoded.get(start..end)?;
        cursor = end;
        Some((start, end))
    })
}

/// A number token as the parser reads one: an `i64` when it is a whole
/// number that fits, a `u64` above that, and an `f64` otherwise.
fn number_value(text: &str) -> Option<Value> {
    let is_float = text.bytes().any(|b| matches!(b, b'.' | b'e' | b'E'));
    if !is_float {
        // `-0` keeps its sign, which only a float can.
        if text == "-0" {
            return Some(Value::Number(-0.0));
        }
        if let Ok(n) = text.parse::<i64>() {
            return Some(Value::Int(n));
        }
        if let Ok(n) = text.parse::<u64>() {
            return Some(Value::Uint(n));
        }
    }
    text.parse::<f64>()
        .ok()
        .filter(|n| n.is_finite())
        .map(Value::Number)
}

fn decode_variants(encoded: &str) -> Vec<(bool, &str)> {
    let mut variants = Vec::new();
    let mut rest = encoded;
    while let Some(tag) = rest.chars().next() {
        let unit = tag == 'u';
        let Some((len, tail)) = rest[1..].split_once(':') else {
            break;
        };
        let Ok(len) = len.parse::<usize>() else {
            break;
        };
        let Some(name) = tail.get(..len) else {
            break;
        };
        variants.push((unit, name));
        rest = &tail[len..];
    }
    variants
}

/// The JSON text of a 128-bit integer's `bits` (a `u128` when `unsigned`):
/// a number when the value fits 64 bits, which every JSON reader holds
/// exactly, and a string of its decimal digits otherwise.
#[must_use]
pub fn wide_json_text(bits: u128, unsigned: bool) -> String {
    if unsigned {
        if u64::try_from(bits).is_ok() {
            bits.to_string()
        } else {
            format!("\"{bits}\"")
        }
    } else {
        let value = bits.cast_signed();
        if i64::try_from(value).is_ok() {
            value.to_string()
        } else {
            format!("\"{value}\"")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::DuplicateKeys;
    use super::{TypedReader, encode_names, encode_variants, scan_value_end};

    /// `scan_value_end` read byte by byte, for comparison with the scan that
    /// skips ahead.
    fn value_end_bytewise(bytes: &[u8], start: usize) -> usize {
        fn string_end(bytes: &[u8], start: usize) -> usize {
            let mut cursor = start + 1;
            while let Some(&byte) = bytes.get(cursor) {
                match byte {
                    b'"' => return cursor + 1,
                    b'\\' => cursor += 2,
                    _ => cursor += 1,
                }
            }
            cursor
        }
        match bytes.get(start) {
            Some(b'"') => string_end(bytes, start),
            Some(b'{' | b'[') => {
                let mut depth = 0usize;
                let mut cursor = start;
                while let Some(&byte) = bytes.get(cursor) {
                    match byte {
                        b'"' => {
                            cursor = string_end(bytes, cursor);
                            continue;
                        }
                        b'{' | b'[' => depth += 1,
                        b'}' | b']' => {
                            depth -= 1;
                            if depth == 0 {
                                return cursor + 1;
                            }
                        }
                        _ => {}
                    }
                    cursor += 1;
                }
                cursor
            }
            _ => {
                let mut cursor = start;
                while let Some(&byte) = bytes.get(cursor) {
                    if matches!(byte, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r') {
                        break;
                    }
                    cursor += 1;
                }
                cursor
            }
        }
    }

    #[test]
    fn the_skipping_scan_ends_values_where_the_bytewise_one_does() {
        const ALPHABET: &[u8] = b"[]{}\"\\,: \t\n\r1a_\x7f";
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        for length in 1..40 {
            for _ in 0..300 {
                let mut text: Vec<u8> = (0..length)
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        ALPHABET[(state % ALPHABET.len() as u64) as usize]
                    })
                    .collect();
                // Every value kind starts the text in turn. A checked document
                // closes each container with its own bracket, so one kind of
                // bracket is used throughout.
                text[0] = [b'[', b'{', b'"', b'1'][length % 4];
                let (open, close) = if text[0] == b'{' {
                    (b'{', b'}')
                } else {
                    (b'[', b']')
                };
                for byte in &mut text[1..] {
                    match *byte {
                        b'[' | b'{' => *byte = open,
                        b']' | b'}' => *byte = close,
                        _ => {}
                    }
                }
                // Unbalanced brackets would underflow the depth, which a
                // checked document never has; the reference shares that.
                let opens = text.iter().filter(|b| matches!(b, b'[' | b'{')).count();
                let closes = text.iter().filter(|b| matches!(b, b']' | b'}')).count();
                if closes > opens {
                    continue;
                }
                assert_eq!(
                    scan_value_end(&text, 0),
                    value_end_bytewise(&text, 0),
                    "{:?}",
                    String::from_utf8_lossy(&text)
                );
            }
        }
    }

    fn message(reader: &TypedReader) -> Option<String> {
        reader.error().map(ToString::to_string)
    }

    /// The fields `next_field` steps into, by index, each read with `read`.
    fn fields(r: &mut TypedReader, mut read: impl FnMut(&mut TypedReader, usize)) -> Vec<usize> {
        let mut seen = Vec::new();
        while r.next_field() {
            let index = r.field_index();
            seen.push(index);
            read(r, index);
            r.leave();
        }
        seen
    }

    #[test]
    fn a_struct_reads_its_fields_in_document_order_with_the_chosen_duplicate() {
        let text = r#"{"b": "x", "a": 1, "a": 2, "zz": [1, {"q": null}]}"#;
        for (policy, order, expected) in [
            (DuplicateKeys::UseFirst, vec![1, 0], 1),
            (DuplicateKeys::UseLast, vec![1, 0, 0], 2),
        ] {
            let mut r = TypedReader::new(text);
            r.set_duplicate_keys(policy);
            r.begin_struct(&encode_names(["a", "b", "c"]));
            let mut a = 0;
            let mut b = String::new();
            let seen = fields(&mut r, |r, index| match index {
                0 => a = r.read_i64("a"),
                _ => b = r.read_string("b"),
            });
            assert_eq!(seen, order);
            assert_eq!((a, b.as_str()), (expected, "x"));
            assert!(!r.finish());
        }
    }

    #[test]
    fn a_struct_refuses_a_repeated_field_key_by_default() {
        let mut r = TypedReader::new(r#"{"b": "x", "a": 1, "a": 2}"#);
        r.begin_struct(&encode_names(["a", "b"]));
        fields(&mut r, |_, _| {});
        assert!(r.finish());
        assert_eq!(message(&r).as_deref(), Some("duplicate key `a`"));
        // A repeated key no field reads is no field's value.
        let mut r = TypedReader::new(r#"{"a": 1, "zz": 1, "zz": 2}"#);
        r.begin_struct(&encode_names(["a"]));
        assert_eq!(fields(&mut r, |r, _| assert_eq!(r.read_i64("a"), 1)), [0]);
        assert!(!r.finish());
    }

    #[test]
    fn the_first_failure_is_kept_with_the_path_to_its_value() {
        let mut r = TypedReader::new(r#"{"inner": {"n": "x"}, "m": true}"#);
        r.begin_struct(&encode_names(["inner", "m"]));
        assert!(r.next_field());
        r.begin_struct(&encode_names(["n"]));
        assert!(r.next_field());
        assert_eq!(r.read_i64("i64"), 0);
        r.leave();
        assert!(!r.next_field());
        r.leave();
        assert!(!r.next_field());
        assert!(r.finish());
        assert_eq!(
            message(&r).as_deref(),
            Some("inner.n: expected i64, found string")
        );
    }

    #[test]
    fn a_path_names_elements_by_index_and_odd_keys_in_quotes() {
        let mut r = TypedReader::new(r#"{"a b": [1, {"q\"": 2.5}]}"#);
        r.begin_struct(&encode_names(["a b"]));
        assert!(r.next_field());
        r.begin_array("array");
        assert!(r.next_elem());
        assert_eq!(r.read_i64("i64"), 1);
        r.leave();
        assert!(r.next_elem());
        r.begin_struct(&encode_names(["q\""]));
        assert!(r.next_field());
        r.read_i64("i64");
        assert_eq!(
            message(&r).as_deref(),
            Some(r#"["a b"][1]["q\""]: expected i64, found 2.5"#)
        );
    }

    #[test]
    fn a_nested_struct_keeps_its_own_fields_and_path() {
        let mut r = TypedReader::new(r#"{"current": {"list": []}, "after": 1}"#);
        r.begin_struct(&encode_names(["current", "after"]));
        assert!(r.next_field());
        assert_eq!(r.field_index(), 0);
        r.begin_struct(&encode_names(["temperature", "list"]));
        assert!(r.next_field());
        assert_eq!(r.field_index(), 1);
        r.begin_array("array");
        assert!(!r.next_elem());
        r.leave();
        assert!(!r.next_field());
        r.fail("missing field `temperature`");
        assert_eq!(
            message(&r).as_deref(),
            Some("current: missing field `temperature`")
        );
    }

    #[test]
    fn a_struct_from_a_value_that_is_not_an_object_fails() {
        let mut r = TypedReader::new(r#"{"inner": [1]}"#);
        r.begin_struct(&encode_names(["inner"]));
        assert!(r.next_field());
        r.begin_struct(&encode_names(["n"]));
        assert!(!r.next_field());
        assert_eq!(
            message(&r).as_deref(),
            Some("inner: expected object, found array")
        );
    }

    #[test]
    fn an_array_walks_its_elements_and_ends_at_its_close() {
        let mut r = TypedReader::new(" [ 1 , [2, 3] , \"x\" ] ");
        r.begin_array("array");
        let mut raw = Vec::new();
        while r.next_elem() {
            raw.push(r.raw_value().to_owned());
            r.leave();
        }
        assert_eq!(raw, ["1", "[2, 3]", "\"x\""]);
        assert!(!r.finish());
        let mut r = TypedReader::new("[]");
        r.begin_array("array");
        assert!(!r.next_elem());
        assert!(!r.finish());
    }

    #[test]
    fn a_narrow_integer_out_of_its_range_fails() {
        let mut r = TypedReader::new("300");
        assert_eq!(r.read_int("u8", 0, 255), 0);
        assert_eq!(message(&r).as_deref(), Some("expected u8, found 300"));
        let mut r = TypedReader::new("-128");
        assert_eq!(r.read_int("i8", -128, 127), -128);
        assert!(!r.failed());
    }

    #[test]
    fn a_tagged_enum_reads_its_tag_beside_the_fields() {
        let table = encode_variants([(true, "Empty"), (false, "Circle")]);
        let mut r = TypedReader::new(r#"{"r": 2, "type": "Circle"}"#);
        assert_eq!(r.begin_tagged(&table, "type", "Shape", 0), 1);
        r.begin_struct(&encode_names(["r"]));
        assert_eq!(fields(&mut r, |r, _| assert_eq!(r.read_i64("i64"), 2)), [0]);
        assert!(!r.finish());
        let mut r = TypedReader::new(r#"{"r": 2}"#);
        r.begin_tagged(&table, "type", "Shape", 0);
        assert_eq!(message(&r).as_deref(), Some("missing field `type`"));
        let mut r = TypedReader::new(r#"{"type": "Square"}"#);
        r.begin_tagged(&table, "type", "Shape", 0);
        assert_eq!(
            message(&r).as_deref(),
            Some("type: unknown variant `Square` of `Shape`, expected one of `Empty`, `Circle`")
        );
    }

    #[test]
    fn an_invalid_document_fails_with_the_parse_error() {
        // However far the walk gets, a document that does not parse reports
        // the parser's error: one cut short, one with a bad separator, one
        // whose bad text is a value no field reads, and one with text after
        // its value.
        for text in [
            r#"{"n": 1,"#,
            r#"{"n": 1 "m": 2}"#,
            r#"{"zz": [1 2], "n": 1}"#,
            r#"{"n": 1} x"#,
            r#"{"n": 01}"#,
        ] {
            let expected = super::super::validate_str(text).unwrap_err().to_string();
            let mut r = TypedReader::new(text);
            r.begin_struct(&encode_names(["n"]));
            fields(&mut r, |r, _| {
                r.read_i64("i64");
            });
            assert!(r.finish(), "{text}");
            assert_eq!(message(&r), Some(expected), "{text}");
        }
        assert_eq!(
            super::super::validate_str(r#"{"n": 1,"#)
                .unwrap_err()
                .to_string(),
            "expected string at 1:9"
        );
        let mut r = TypedReader::new("[1, 2 3]");
        r.begin_tuple(2, "array", "2-tuple");
        r.enter_elem(0);
        r.read_i64("i64");
        r.leave();
        assert!(r.finish());
        let mut r = TypedReader::new(r#"{"a":1,"#);
        r.begin_map("map");
        while r.next_entry() {
            r.leave();
        }
        assert!(r.finish());
    }

    #[test]
    fn numbers_follow_the_json_accessor_rules() {
        let read = |text: &str| {
            let i = TypedReader::new(text).read_i64("i");
            let u = TypedReader::new(text).read_u64("u");
            let f = TypedReader::new(text).read_f64("f");
            format!("{i} {u} {f}")
        };
        assert_eq!(read("3.0"), "3 3 3");
        assert_eq!(read("1.5"), "0 0 1.5");
        assert_eq!(read("-1"), "-1 0 -1");
        assert_eq!(
            read("18446744073709551615"),
            format!("0 {} {}", u64::MAX, u64::MAX as f64)
        );
        assert_eq!(read("-0"), "0 0 -0");
        assert_eq!(read("1e30"), "0 0 1000000000000000000000000000000");
    }

    #[test]
    fn maps_answer_keys_in_document_order_with_the_chosen_value_of_a_repeat() {
        let text = r#"{"q": 2, "b": 1, "q": 5}"#;
        for (policy, expected) in [
            (DuplicateKeys::UseFirst, vec![("q", 2), ("b", 1)]),
            (DuplicateKeys::UseLast, vec![("q", 2), ("b", 1), ("q", 5)]),
        ] {
            let mut r = TypedReader::new(text);
            r.set_duplicate_keys(policy);
            r.begin_map("m");
            let mut entries = Vec::new();
            while r.next_entry() {
                let key = r.entry_key().to_owned();
                entries.push((key, r.read_i64("v")));
                r.leave();
            }
            let entries: Vec<_> = entries.iter().map(|(k, v)| (k.as_str(), *v)).collect();
            assert_eq!(entries, expected);
            assert!(!r.finish());
        }
        let mut r = TypedReader::new(text);
        r.begin_map("m");
        while r.next_entry() {
            r.leave();
        }
        assert!(r.finish());
        assert_eq!(message(&r).as_deref(), Some("duplicate key `q`"));
    }

    #[test]
    fn an_enum_selects_its_variant_by_name_or_single_key() {
        let table = encode_variants([(true, "Empty"), (false, "Circle"), (false, "Rect")]);
        let mut r = TypedReader::new(r#""Empty""#);
        assert_eq!(r.begin_enum(&table, "Shape", 0), 0);
        let mut r = TypedReader::new(r#"{"Circle": 1.5}"#);
        assert_eq!(r.begin_enum(&table, "Shape", 0), 1);
        assert!((r.read_f64("c") - 1.5).abs() < f64::EPSILON);
        let mut r = TypedReader::new(r#"{"Empty": null}"#);
        r.begin_enum(&table, "Shape", 0);
        assert_eq!(
            message(&r).as_deref(),
            Some("unknown variant `Empty` of `Shape`, expected one of `Circle`, `Rect`")
        );
        let mut r = TypedReader::new("[1]");
        r.begin_enum(&table, "Shape", 0);
        assert_eq!(
            message(&r).as_deref(),
            Some("expected a variant of `Shape` (a name, or an object with one key), found array")
        );
    }

    #[test]
    fn indent_text_lays_out_as_the_pretty_renderer() {
        let compact = r#"{"z":[1,{"a":"x,y:[]"},[]],"e":{},"n":null}"#;
        let pretty = super::indent_text(compact);
        let tree = crate::json::parse(compact).unwrap();
        assert_eq!(
            pretty,
            "{\n  \"z\": [\n    1,\n    {\n      \"a\": \"x,y:[]\"\n    },\n    []\n  ],\n  \"e\": {},\n  \"n\": null\n}"
        );
        assert_eq!(crate::json::parse(&pretty).unwrap(), tree);
        assert_eq!(super::indent_text("[]"), "[]");
        assert_eq!(super::indent_text("1.5"), "1.5");
    }

    #[test]
    fn json_at_reads_null_past_the_end() {
        let mut r = TypedReader::new("[1]");
        r.enter_at(1);
        assert!(r.is_null());
        assert_eq!(r.read_i64("i64"), 0);
        assert_eq!(
            message(&r).as_deref(),
            Some("[1]: expected i64, found null")
        );
    }
}
