//! The typed reader `from_json::<T>` decodes through.
//!
//! The document is validated once, as [`super::parse`] would read it, and then
//! walked by byte offset without building a tree: a struct records where each
//! of its keys' values starts and reads them in declaration order, an array
//! records where its elements start, and a scalar is parsed where it stands.
//! The values and failures match what `json::parse` followed by `json::get`
//! and the `json::as_*` accessors report, because the scanning is the same
//! parser and the conversions are the same rules.
//!
//! The reader's error is sticky. The first failure is kept, every later read
//! answers a zero value, and the decoder checks [`TypedReader::failed`] once at
//! the end. Each nested decode that was running when the failure happened adds
//! its context on the way out, so the error reads outermost first, the way
//! nested `errors::wrap` calls render.

use super::{Error, Parser, Value, validate_str_with_containers};

/// A walk over one validated JSON document held as `T`: an owned `String`,
/// or text a caller keeps alive for the walk.
#[derive(Debug)]
pub struct TypedReader<T = String> {
    text: T,
    /// Every object and array: its opening offset and the offset just past
    /// its close, in document order, so stepping over one is a lookup.
    containers: Vec<(usize, usize)>,
    /// The values stepped into, the first `depth` of them live; the rest keep
    /// their buffers for the next step.
    frames: Vec<Frame>,
    depth: usize,
    error: Option<ReadError>,
    /// The last key table `begin_struct` read, and where each name sits in
    /// it: a decode reads the same few tables over and over.
    table: String,
    table_names: Vec<(usize, usize)>,
}

/// The first failure and the contexts the decodes it unwound through added,
/// innermost first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadError {
    /// The failure's own message.
    pub message: String,
    /// One per nested decode that wrapped it, innermost first.
    pub contexts: Vec<String>,
}

/// One value the walk stands on: its byte offset, or `None` for a value the
/// document does not have (an element past the end), which reads as `null`.
#[derive(Debug, Default)]
struct Frame {
    node: Option<usize>,
    fields: Vec<Option<usize>>,
    elems: Vec<usize>,
    entries: Vec<(String, usize)>,
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
        let mut reader = Self {
            text,
            containers: Vec::new(),
            frames: Vec::with_capacity(8),
            depth: 0,
            error: None,
            table: String::new(),
            table_names: Vec::new(),
        };
        match validate_str_with_containers(reader.text.as_ref()) {
            Ok(containers) => {
                reader.containers = containers;
                let start = skip_ws(reader.text.as_ref().as_bytes(), 0);
                reader.push(Some(start));
            }
            Err(error) => reader.reject(&error),
        }
        reader
    }

    /// A reader over a document already rejected with `error`, as for text
    /// that is not UTF-8.
    #[must_use]
    pub fn rejected(text: T, error: &Error) -> Self {
        let mut reader = Self {
            text,
            containers: Vec::new(),
            frames: Vec::with_capacity(1),
            depth: 0,
            error: None,
            table: String::new(),
            table_names: Vec::new(),
        };
        reader.reject(error);
        reader
    }

    fn reject(&mut self, error: &Error) {
        self.error = Some(ReadError {
            message: error.to_string(),
            contexts: Vec::new(),
        });
        self.push(None);
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

    /// Records `message` as the failure, unless one is already recorded.
    pub fn fail(&mut self, message: &str) {
        if self.error.is_none() {
            self.error = Some(ReadError {
                message: message.to_owned(),
                contexts: Vec::new(),
            });
        }
    }

    /// Adds `context` to a failure that happened after a decode started:
    /// `failed_before` is what [`Self::failed`] answered when it began.
    pub fn wrap(&mut self, failed_before: bool, context: &str) {
        if failed_before {
            return;
        }
        if let Some(error) = &mut self.error {
            error.contexts.push(context.to_owned());
        }
    }

    /// Steps out of the value the last `enter_*` stepped into.
    pub fn leave(&mut self) {
        if self.depth > 1 {
            self.depth -= 1;
        }
    }

    /// Whether the current value is `null` or absent.
    #[must_use]
    pub fn is_null(&self) -> bool {
        self.kind() == Kind::Null
    }

    /// Records where each of `keys` (encoded by [`encode_names`]) starts in
    /// the current value. A key the document repeats takes its last value, as
    /// `json::get` reads it; a value that is not an object has none of them.
    pub fn begin_struct(&mut self, keys: &str) {
        if self.table != keys {
            self.table.clear();
            self.table.push_str(keys);
            self.table_names.clear();
            self.table_names.extend(table_ranges(keys));
        }
        let mut fields = std::mem::take(&mut self.top_mut().fields);
        fields.clear();
        fields.resize(self.table_names.len(), None);
        if !self.failed()
            && let Some(start) = self.object_start()
        {
            let table = self.table.as_bytes();
            let names = &self.table_names;
            self.for_each_entry(start, |key, value| {
                if let Some(index) = names.iter().position(|&(a, b)| &table[a..b] == key) {
                    fields[index] = Some(value);
                }
            });
        }
        self.top_mut().fields = fields;
    }

    /// Steps into field `index` of the struct [`Self::begin_struct`] began, and
    /// answers whether the document has it. Nothing is stepped into when it
    /// does not.
    pub fn enter_field(&mut self, index: usize) -> bool {
        let node = self.top().fields.get(index).copied().flatten();
        match node {
            Some(node) if !self.failed() => {
                self.push(Some(node));
                true
            }
            _ => false,
        }
    }

    /// The current value as an `i64`, by `json::as_i64`'s rule, or `0` with
    /// `message` recorded when it is not one.
    pub fn read_i64(&mut self, message: &str) -> i64 {
        let Some(n) = self.scalar().as_ref().and_then(as_i64) else {
            self.fail(message);
            return 0;
        };
        n
    }

    /// The current value as a `u64`, by `json::as_u64`'s rule.
    pub fn read_u64(&mut self, message: &str) -> u64 {
        let Some(n) = self.scalar().as_ref().and_then(as_u64) else {
            self.fail(message);
            return 0;
        };
        n
    }

    /// The current value as an `f64`, by `json::as_f64`'s rule.
    pub fn read_f64(&mut self, message: &str) -> f64 {
        // A number token reads as the nearest `f64` whether it is written
        // as an integer or not, which is what `as_f64` makes of each form.
        let number = if self.kind() == Kind::Number {
            self.number_text().and_then(|text| text.parse::<f64>().ok())
        } else {
            None
        };
        let Some(n) = number else {
            self.fail(message);
            return 0.0;
        };
        n
    }

    fn number_text(&self) -> Option<&str> {
        let start = self.node()?;
        let end = scan_value_end(self.text().as_bytes(), start);
        self.text().get(start..end)
    }

    /// The current value as a `bool`.
    pub fn read_bool(&mut self, message: &str) -> bool {
        let Some(Value::Bool(b)) = self.scalar() else {
            self.fail(message);
            return false;
        };
        b
    }

    /// The current value as a `char`: a string holding exactly one.
    pub fn read_char(&mut self, message: &str) -> char {
        let text = self.read_string(message);
        let mut chars = text.chars();
        let (Some(c), None) = (chars.next(), chars.next()) else {
            self.fail(message);
            return '\0';
        };
        c
    }

    /// The current value as a string.
    pub fn read_string(&mut self, message: &str) -> String {
        let Some(Value::String(s)) = self.scalar() else {
            self.fail(message);
            return String::new();
        };
        s
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

    /// The element count of the current value, recording `message` and
    /// answering `0` when it is not an array.
    pub fn begin_array(&mut self, message: &str) -> usize {
        if self.failed() {
            return 0;
        }
        if self.kind() != Kind::Array {
            self.fail(message);
            return 0;
        }
        let mut elems = std::mem::take(&mut self.top_mut().elems);
        self.current_elems(&mut elems);
        let count = elems.len();
        self.top_mut().elems = elems;
        count
    }

    /// Checks that the current value is an array of exactly `len` elements,
    /// recording `not_array` or `wrong_len` when it is not.
    pub fn begin_tuple(&mut self, len: usize, not_array: &str, wrong_len: &str) {
        if self.failed() {
            return;
        }
        if self.kind() != Kind::Array {
            self.fail(not_array);
            return;
        }
        let mut elems = std::mem::take(&mut self.top_mut().elems);
        self.current_elems(&mut elems);
        let fits = elems.len() == len;
        self.top_mut().elems = elems;
        if !fits {
            self.fail(wrong_len);
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
        self.push(node);
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
        self.push(node);
    }

    /// The entry count of the current value, recording `message` and answering
    /// `0` when it is not an object. Entries are in key order, and a key the
    /// document repeats keeps its last value, as `json::keys` and `json::get`
    /// read them.
    pub fn begin_map(&mut self, message: &str) -> usize {
        if self.failed() {
            return 0;
        }
        let Some(start) = self.object_start() else {
            self.fail(message);
            return 0;
        };
        let mut entries: Vec<(String, usize)> = Vec::new();
        self.for_each_entry(start, |key, value| {
            entries.push((String::from_utf8_lossy(key).into_owned(), value));
        });
        // A stable sort keeps a repeated key's occurrences in document order,
        // so the last of each run is the value `json::get` answers.
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let mut deduped: Vec<(String, usize)> = Vec::with_capacity(entries.len());
        for entry in entries {
            match deduped.last_mut() {
                Some(last) if last.0 == entry.0 => *last = entry,
                _ => deduped.push(entry),
            }
        }
        let count = deduped.len();
        self.top_mut().entries = deduped;
        count
    }

    /// The key of entry `index` of the map [`Self::begin_map`] began.
    #[must_use]
    pub fn map_key(&self, index: usize) -> &str {
        self.top()
            .entries
            .get(index)
            .map_or("", |(key, _)| key.as_str())
    }

    /// Steps into the value of entry `index` of the map [`Self::begin_map`]
    /// began.
    pub fn enter_map_value(&mut self, index: usize) {
        let node = if self.failed() {
            None
        } else {
            self.top().entries.get(index).map(|(_, value)| *value)
        };
        self.push(node);
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
            if let Some(payload) = payload {
                self.push(Some(payload));
            }
            return index;
        }
        if table.get(fallback).is_some_and(|(unit, _)| !unit) {
            self.push(None);
        }
        fallback
    }

    fn select_variant(
        &mut self,
        table: &[(bool, &str)],
        name: &str,
    ) -> Option<(usize, Option<usize>)> {
        match self.kind() {
            Kind::String => {
                let tag = self.read_string("");
                let found = table.iter().position(|(unit, n)| *unit && *n == tag);
                if found.is_none() {
                    self.fail(&format!("unknown variant `{tag}` of `{name}`"));
                }
                found.map(|index| (index, None))
            }
            Kind::Object => {
                let start = self.object_start()?;
                // Distinct keys, each with its last value, as `json::keys`
                // and `json::get` read the object.
                let mut entries: Vec<(String, usize)> = Vec::new();
                self.for_each_entry(start, |key, value| {
                    match entries.iter_mut().find(|(k, _)| k.as_bytes() == key) {
                        Some(slot) => slot.1 = value,
                        None => entries.push((String::from_utf8_lossy(key).into_owned(), value)),
                    }
                });
                if entries.len() != 1 {
                    self.fail(&shape_message(name));
                    return None;
                }
                let (tag, payload) = entries.swap_remove(0);
                let found = table.iter().position(|(unit, n)| !*unit && *n == tag);
                if found.is_none() {
                    self.fail(&format!("unknown variant `{tag}` of `{name}`"));
                }
                found.map(|index| (index, Some(payload)))
            }
            _ => {
                self.fail(&shape_message(name));
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
            self.push(None);
        }
        &mut self.frames[self.depth - 1]
    }

    fn push(&mut self, node: Option<usize>) {
        if let Some(frame) = self.frames.get_mut(self.depth) {
            frame.node = node;
        } else {
            self.frames.push(Frame {
                node,
                ..Frame::default()
            });
        }
        self.depth += 1;
    }

    fn node(&self) -> Option<usize> {
        self.top().node
    }

    fn kind(&self) -> Kind {
        if self.failed() {
            return Kind::Null;
        }
        self.node()
            .map_or(Kind::Null, |start| kind_at(self.text().as_bytes(), start))
    }

    fn object_start(&self) -> Option<usize> {
        (self.kind() == Kind::Object).then(|| self.node()).flatten()
    }

    /// The current scalar, parsed where it stands; `None` for a container or
    /// an absent value.
    fn scalar(&self) -> Option<Value> {
        if self.failed() {
            return None;
        }
        match self.kind() {
            Kind::Null => Some(Value::Null),
            Kind::Array | Kind::Object => None,
            Kind::Number => {
                let start = self.node()?;
                let end = scan_value_end(self.text().as_bytes(), start);
                number_value(&self.text()[start..end])
            }
            Kind::Bool | Kind::String => {
                let start = self.node()?;
                let mut parser = Parser::new(self.text());
                parser.cursor = start;
                parser.parse_value().ok()
            }
        }
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
                _ => return,
            }
        }
    }

    /// Calls `f` with each key of the object at `start` and the offset of its
    /// value, in document order.
    fn for_each_entry(&self, start: usize, mut f: impl FnMut(&[u8], usize)) {
        let bytes = self.text().as_bytes();
        let mut cursor = skip_ws(bytes, start + 1);
        if bytes.get(cursor) == Some(&b'}') {
            return;
        }
        loop {
            let key_start = skip_ws(bytes, cursor);
            let key_end = string_end(bytes, key_start);
            let raw = &bytes[key_start + 1..key_end.saturating_sub(1).max(key_start + 1)];
            // A key without an escape is its own bytes; one with an escape is
            // read the way the parser reads any string.
            let unescaped;
            let key = if raw.contains(&b'\\') {
                let mut parser = Parser::new(self.text());
                parser.cursor = key_start;
                let Ok(text) = parser.parse_string() else {
                    return;
                };
                unescaped = text;
                unescaped.as_bytes()
            } else {
                raw
            };
            // The document validated, so a `:` follows every key.
            cursor = skip_ws(bytes, key_end);
            cursor = skip_ws(bytes, cursor + 1);
            let value = cursor;
            f(key, value);
            cursor = skip_ws(bytes, self.value_end(value));
            match bytes.get(cursor) {
                Some(b',') => cursor += 1,
                _ => return,
            }
        }
    }

    /// The offset just past the value starting at `start`: a container's from
    /// the spans validation recorded, anything else by scanning it.
    fn value_end(&self, start: usize) -> usize {
        let bytes = self.text().as_bytes();
        match bytes.get(start) {
            Some(b'{' | b'[') => self
                .containers
                .binary_search_by_key(&start, |(open, _)| *open)
                .map_or_else(
                    |_| scan_value_end(bytes, start),
                    |slot| self.containers[slot].1,
                ),
            _ => scan_value_end(bytes, start),
        }
    }
}

static ROOT_ABSENT: Frame = Frame {
    node: None,
    fields: Vec::new(),
    elems: Vec::new(),
    entries: Vec::new(),
};

fn shape_message(name: &str) -> String {
    format!("expected a variant of `{name}`: a name, or an object with one key")
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

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "json::as_i64 accepts an integral float inside i64's range, and this is that rule"
)]
fn as_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Int(n) => Some(*n),
        Value::Number(n) if n.fract() == 0.0 && *n >= i64::MIN as f64 && *n <= i64::MAX as f64 => {
            Some(*n as i64)
        }
        _ => None,
    }
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "json::as_u64 accepts an integral float inside u64's range, and this is that rule"
)]
fn as_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Int(n) => u64::try_from(*n).ok(),
        Value::Uint(n) => Some(*n),
        Value::Number(n) if n.fract() == 0.0 && *n >= 0.0 && *n <= u64::MAX as f64 => {
            Some(*n as u64)
        }
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

#[cfg(test)]
mod tests {
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
        reader.error().map(|e| {
            let mut text = e.message.clone();
            for context in &e.contexts {
                text = format!("{context}: {text}");
            }
            text
        })
    }

    #[test]
    fn a_struct_reads_fields_in_declaration_order_with_the_last_duplicate() {
        let mut r = TypedReader::new(r#"{"b": "x", "a": 1, "a": 2, "zz": [1, {"q": null}]}"#);
        r.begin_struct(&encode_names(["a", "b", "c"]));
        assert!(r.enter_field(0));
        assert_eq!(r.read_i64("a"), 2);
        r.leave();
        assert!(r.enter_field(1));
        assert_eq!(r.read_string("b"), "x");
        r.leave();
        assert!(!r.enter_field(2));
        assert!(!r.failed());
    }

    #[test]
    fn the_first_failure_is_kept_and_wrapped_by_the_decodes_it_unwinds() {
        let mut r = TypedReader::new(r#"{"inner": {"n": "x"}, "m": true}"#);
        r.begin_struct(&encode_names(["inner", "m"]));
        assert!(r.enter_field(0));
        let before = r.failed();
        r.begin_struct(&encode_names(["n"]));
        assert!(r.enter_field(0));
        assert_eq!(r.read_i64("field `n`: expected i64"), 0);
        r.leave();
        r.wrap(before, "field `inner`");
        r.leave();
        assert!(r.enter_field(1) || r.failed());
        assert_eq!(r.read_i64("field `m`: expected i64"), 0);
        assert_eq!(
            message(&r).as_deref(),
            Some("field `inner`: field `n`: expected i64")
        );
    }

    #[test]
    fn an_invalid_document_fails_with_the_parse_error() {
        let r = TypedReader::new(r#"{"n": 1,"#);
        assert_eq!(message(&r).as_deref(), Some("expected string at 1:9"));
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
    fn maps_answer_sorted_keys_with_the_last_value_of_a_repeat() {
        let mut r = TypedReader::new(r#"{"q": 2, "b": 1, "q": 5}"#);
        assert_eq!(r.begin_map("m"), 2);
        assert_eq!(r.map_key(0), "b");
        assert_eq!(r.map_key(1), "q");
        r.enter_map_value(1);
        assert_eq!(r.read_i64("v"), 5);
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
            Some("unknown variant `Empty` of `Shape`")
        );
        let mut r = TypedReader::new("[1]");
        r.begin_enum(&table, "Shape", 0);
        assert_eq!(
            message(&r).as_deref(),
            Some("expected a variant of `Shape`: a name, or an object with one key")
        );
    }

    #[test]
    fn json_at_reads_null_past_the_end() {
        let mut r = TypedReader::new("[1]");
        r.enter_at(1);
        assert!(r.is_null());
        assert_eq!(r.read_i64("expected i64"), 0);
        assert!(r.failed());
    }
}
