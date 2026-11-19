//! The typed reader `from_json::<T>` decodes through.
//!
//! The document is validated once, as [`super::parse`] would read it, by the
//! walk that also reads it into a [`Tape`]: every value in document order,
//! each container with the extent of what it holds and each number already
//! converted. The reader then walks the tape without building a tree: a
//! struct records where each of its keys' values sits and reads them in
//! declaration order, an array records where its elements sit, and a scalar
//! is read from its node. A document too long for a tape is walked by byte
//! offset instead, over the spans of its containers.
//! The values and failures match what `json::parse` followed by `json::get`
//! and the `json::as_*` accessors report, because the scanning is the same
//! parser and the conversions are the same rules.
//!
//! The reader's error is sticky. The first failure is kept, every later read
//! answers a zero value, and the decoder checks [`TypedReader::failed`] once at
//! the end. Each value the walk steps into remembers the key or index it was
//! reached by, so a failure carries the path to the value it names
//! (`current.items[2]`), spelled with the document's own keys.

use super::{
    ContainerSpans, Error, Parser, Tape, TapeKind, Value, integral_i64, integral_u64,
    unescape_string, validate_str_to_tape, validate_str_with_containers,
};

/// What the reader walks a document by. A node is a tape index over a tape,
/// and a byte offset over spans.
#[derive(Debug)]
enum Index {
    /// The document's tape.
    Tape(Tape),
    /// Each object's and array's span, for a document too long for a tape.
    Spans {
        containers: ContainerSpans,
        /// The slot in `containers` just past the last one looked up. A walk
        /// steps over a container's members in document order, so the next
        /// lookup is nearly always at or just after it.
        cursor: std::cell::Cell<usize>,
    },
}

/// A walk over one validated JSON document held as `T`: an owned `String`,
/// or text a caller keeps alive for the walk.
#[derive(Debug)]
pub struct TypedReader<T = String> {
    text: T,
    index: Index,
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
    /// Per field of the struct begun here: its key's offset and its value's.
    fields: Vec<Option<(usize, usize)>>,
    elems: Vec<usize>,
    /// Per map entry: its key, the key's offset, and the value's offset.
    entries: Vec<(MapKey, usize, usize)>,
}

/// A map entry's key: the span of the document that spells it, or its text
/// with escapes resolved when it has any.
#[derive(Debug)]
enum MapKey {
    Span(usize, usize),
    Owned(String),
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
            index: Index::Tape(Tape::default()),
            frames: Vec::with_capacity(8),
            depth: 0,
            error: None,
            table: String::new(),
            table_names: Vec::new(),
        };
        let indexed = match validate_str_to_tape(reader.text.as_ref()) {
            Ok(Some(tape)) => Ok((Index::Tape(tape), 0)),
            Ok(None) => validate_str_with_containers(reader.text.as_ref()).map(|containers| {
                let root = skip_ws(reader.text.as_ref().as_bytes(), 0);
                let cursor = std::cell::Cell::new(0);
                (Index::Spans { containers, cursor }, root)
            }),
            Err(error) => Err(error),
        };
        match indexed {
            Ok((index, root)) => {
                reader.index = index;
                reader.push(Some(root), Segment::None);
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
            index: Index::Tape(Tape::default()),
            frames: Vec::with_capacity(1),
            depth: 0,
            error: None,
            table: String::new(),
            table_names: Vec::new(),
        };
        reader.reject(error);
        reader
    }

    /// A reader that has failed with `message` before reading anything, as
    /// for a YAML or TOML document that did not convert.
    #[must_use]
    pub fn failing(text: T, message: &str) -> Self {
        let mut reader = Self {
            text,
            index: Index::Tape(Tape::default()),
            frames: Vec::with_capacity(1),
            depth: 0,
            error: None,
            table: String::new(),
            table_names: Vec::new(),
        };
        reader.error = Some(ReadError {
            message: message.to_owned(),
            path: String::new(),
        });
        reader.push(None, Segment::None);
        reader
    }

    fn reject(&mut self, error: &Error) {
        self.error = Some(ReadError {
            message: error.to_string(),
            path: String::new(),
        });
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
        if let Index::Tape(tape) = &self.index {
            return self.tape_string(tape, offset);
        }
        let bytes = self.text().as_bytes();
        let end = string_end(bytes, offset);
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
    /// `json::get` reads it. A value that is not an object is a failure.
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
        if !self.failed() {
            if let Some(start) = self.object_start() {
                let table = self.table.as_bytes();
                let names = &self.table_names;
                let field_of = |key: &[u8], ordinal: usize| {
                    // Members are usually written in declaration order, so
                    // the name at the member's own position is tried first.
                    match names.get(ordinal) {
                        Some(&(a, b)) if &table[a..b] == key => Some(ordinal),
                        _ => names.iter().position(|&(a, b)| &table[a..b] == key),
                    }
                };
                if let Index::Tape(tape) = &self.index {
                    let bytes = self.text().as_bytes();
                    let object = start as u32;
                    let mut key = object + 2;
                    for ordinal in 0..tape.count(object) as usize {
                        let node = tape.node(key);
                        let spelled = &bytes[node.start as usize + 1..node.end() as usize - 1];
                        let index = if node.kind() == TapeKind::Str {
                            field_of(spelled, ordinal)
                        } else {
                            field_of(self.tape_string(tape, key as usize).as_bytes(), ordinal)
                        };
                        if let Some(index) = index {
                            fields[index] = Some((key as usize, key as usize + 1));
                        }
                        key = tape.extent(key + 1);
                    }
                } else {
                    let mut ordinal = 0;
                    self.for_each_entry(start, |key, key_at, value| {
                        if let Some(index) = field_of(key, ordinal) {
                            fields[index] = Some((key_at, value));
                        }
                        ordinal += 1;
                    });
                }
            } else {
                self.top_mut().fields = fields;
                self.fail_expected("object");
                return;
            }
        }
        self.top_mut().fields = fields;
    }

    /// Steps into field `index` of the struct [`Self::begin_struct`] began, and
    /// answers whether the document has it. Nothing is stepped into when it
    /// does not.
    pub fn enter_field(&mut self, index: usize) -> bool {
        let node = self.top().fields.get(index).copied().flatten();
        match node {
            Some((key, node)) if !self.failed() => {
                self.push(Some(node), Segment::Key(key));
                true
            }
            _ => false,
        }
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

    /// The current value as an `f64`, by `json::as_f64`'s rule.
    pub fn read_f64(&mut self, expected: &str) -> f64 {
        // A number token reads as the nearest `f64` whether it is written
        // as an integer or not, which is what `as_f64` makes of each form.
        let number = match (&self.index, self.node()) {
            _ if self.failed() => None,
            (Index::Tape(tape), Some(node)) => {
                let index = node as u32;
                // Each form converts to its nearest `f64`, as parsing the
                // token's text as one does.
                match tape.node(index).kind() {
                    TapeKind::Float => Some(f64::from_bits(tape.word(index))),
                    TapeKind::Int => Some(tape.word(index) as i64 as f64),
                    TapeKind::Uint => Some(tape.word(index) as f64),
                    _ => None,
                }
            }
            _ if self.kind() == Kind::Number => {
                self.number_text().and_then(|text| text.parse::<f64>().ok())
            }
            _ => None,
        };
        let Some(n) = number else {
            self.fail_expected(expected);
            return 0.0;
        };
        n
    }

    fn number_text(&self) -> Option<&str> {
        let node = self.node()?;
        let (start, end) = match &self.index {
            Index::Tape(tape) => {
                let at = tape.node(node as u32);
                (at.start as usize, at.end() as usize)
            }
            Index::Spans { .. } => (node, scan_value_end(self.text().as_bytes(), node)),
        };
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
        let Some(Value::String(s)) = self.scalar() else {
            self.fail_expected(expected);
            return String::new();
        };
        s
    }

    /// The text of the current value, `null` when it is absent, for a field
    /// that keeps the document as `json::Value`.
    #[must_use]
    pub fn raw_value(&self) -> &str {
        match (self.node(), &self.index) {
            (Some(node), Index::Tape(tape)) if !self.failed() => {
                let at = tape.node(node as u32);
                &self.text()[at.start as usize..at.end() as usize]
            }
            (Some(start), Index::Spans { .. }) if !self.failed() => {
                let end = self.value_end(start);
                &self.text()[start..end]
            }
            _ => "null",
        }
    }

    /// The element count of the current value, recording a failure naming
    /// `expected` and answering `0` when it is not an array.
    pub fn begin_array(&mut self, expected: &str) -> usize {
        if self.failed() {
            return 0;
        }
        if self.kind() != Kind::Array {
            self.fail_expected(expected);
            return 0;
        }
        let mut elems = std::mem::take(&mut self.top_mut().elems);
        self.current_elems(&mut elems);
        let count = elems.len();
        self.top_mut().elems = elems;
        count
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

    /// The entry count of the current value, recording a failure naming
    /// `expected` and answering `0` when it is not an object. Entries are in
    /// key order, and a key the document repeats keeps its last value, as
    /// `json::keys` and `json::get` read them.
    pub fn begin_map(&mut self, expected: &str) -> usize {
        if self.failed() {
            return 0;
        }
        let Some(start) = self.object_start() else {
            self.fail_expected(expected);
            return 0;
        };
        let mut entries = std::mem::take(&mut self.top_mut().entries);
        entries.clear();
        let text = self.text().as_ptr() as usize;
        self.for_each_entry(start, |_, key_at, value| {
            let key = match self.key_at(key_at) {
                std::borrow::Cow::Borrowed(spelled) => {
                    let offset = spelled.as_ptr() as usize - text;
                    MapKey::Span(offset, offset + spelled.len())
                }
                std::borrow::Cow::Owned(resolved) => MapKey::Owned(resolved),
            };
            entries.push((key, key_at, value));
        });
        if entries.len() > 1 {
            // A stable sort keeps a repeated key's occurrences in document
            // order, so the last of each run is the value `json::get` answers.
            entries.sort_by(|a, b| self.entry_key(&a.0).cmp(self.entry_key(&b.0)));
            entries.dedup_by(|later, kept| {
                let repeated = self.entry_key(&later.0) == self.entry_key(&kept.0);
                if repeated {
                    std::mem::swap(later, kept);
                }
                repeated
            });
        }
        let count = entries.len();
        self.top_mut().entries = entries;
        count
    }

    fn entry_key<'k>(&'k self, key: &'k MapKey) -> &'k str {
        match key {
            MapKey::Span(start, end) => &self.text()[*start..*end],
            MapKey::Owned(text) => text,
        }
    }

    /// The key of entry `index` of the map [`Self::begin_map`] began.
    #[must_use]
    pub fn map_key(&self, index: usize) -> &str {
        self.top()
            .entries
            .get(index)
            .map_or("", |(key, _, _)| self.entry_key(key))
    }

    /// Steps into the value of entry `index` of the map [`Self::begin_map`]
    /// began.
    pub fn enter_map_value(&mut self, index: usize) {
        let entry = if self.failed() {
            None
        } else {
            self.top()
                .entries
                .get(index)
                .map(|&(_, key, value)| (key, value))
        };
        match entry {
            Some((key, value)) => self.push(Some(value), Segment::Key(key)),
            None => self.push(None, Segment::None),
        }
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
        } else {
            self.frames.push(Frame {
                node,
                segment,
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
        let Some(node) = self.node() else {
            return Kind::Null;
        };
        match &self.index {
            Index::Tape(tape) => match tape.node(node as u32).kind() {
                TapeKind::Null => Kind::Null,
                TapeKind::False | TapeKind::True => Kind::Bool,
                TapeKind::Int | TapeKind::Uint | TapeKind::Float => Kind::Number,
                TapeKind::Str | TapeKind::StrEscaped => Kind::String,
                TapeKind::Array => Kind::Array,
                TapeKind::Object => Kind::Object,
            },
            Index::Spans { .. } => kind_at(self.text().as_bytes(), node),
        }
    }

    /// The text of the string at tape node `node`, escapes resolved.
    fn tape_string(&self, tape: &Tape, node: usize) -> std::borrow::Cow<'_, str> {
        let at = tape.node(node as u32);
        let raw = &self.text()[at.start as usize + 1..at.end() as usize - 1];
        if at.kind() == TapeKind::StrEscaped {
            std::borrow::Cow::Owned(unescape_string(raw))
        } else {
            std::borrow::Cow::Borrowed(raw)
        }
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
        if let (Index::Tape(tape), Some(node)) = (&self.index, self.node()) {
            let index = node as u32;
            let word = || tape.word(index);
            return match tape.node(index).kind() {
                TapeKind::Null => Some(Value::Null),
                TapeKind::False => Some(Value::Bool(false)),
                TapeKind::True => Some(Value::Bool(true)),
                TapeKind::Int => Some(Value::Int(word() as i64)),
                TapeKind::Uint => Some(Value::Uint(word())),
                TapeKind::Float => Some(Value::Number(f64::from_bits(word()))),
                TapeKind::Str | TapeKind::StrEscaped => {
                    Some(Value::String(self.tape_string(tape, node).into_owned()))
                }
                TapeKind::Array | TapeKind::Object => None,
            };
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
        if let Index::Tape(tape) = &self.index {
            let array = start as u32;
            let mut at = array + 2;
            for _ in 0..tape.count(array) {
                elems.push(at as usize);
                at = tape.extent(at);
            }
            return;
        }
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

    /// Calls `f` with each key of the object at `start`, the offset of the
    /// key's opening quote, and the offset of its value, in document order.
    fn for_each_entry(&self, start: usize, mut f: impl FnMut(&[u8], usize, usize)) {
        if let Index::Tape(tape) = &self.index {
            let object = start as u32;
            let mut key = object + 2;
            for _ in 0..tape.count(object) {
                let value = key + 1;
                let text = self.tape_string(tape, key as usize);
                f(text.as_bytes(), key as usize, value as usize);
                key = tape.extent(value);
            }
            return;
        }
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
            f(key, key_start, value);
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
            Some(b'{' | b'[') => match (&self.index, self.container_slot(start)) {
                (Index::Spans { containers, .. }, Some(slot)) => containers[slot].1,
                _ => scan_value_end(bytes, start),
            },
            _ => scan_value_end(bytes, start),
        }
    }

    /// The slot of the container opening at `start`: found by stepping
    /// forward from the last lookup, which a walk in document order reaches
    /// in a step or two, else by a search of the whole table.
    fn container_slot(&self, start: usize) -> Option<usize> {
        let Index::Spans { containers, cursor } = &self.index else {
            return None;
        };
        let from = cursor.get();
        let ahead = containers
            .get(from..)
            .unwrap_or_default()
            .iter()
            .take(4)
            .position(|(open, _)| *open == start)
            .map(|offset| from + offset);
        let slot = ahead.or_else(|| {
            containers
                .binary_search_by_key(&start, |(open, _)| *open)
                .ok()
        })?;
        cursor.set(slot + 1);
        Some(slot)
    }
}

static ROOT_ABSENT: Frame = Frame {
    node: None,
    segment: Segment::None,
    fields: Vec::new(),
    elems: Vec::new(),
    entries: Vec::new(),
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
        reader.error().map(ToString::to_string)
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
    fn the_first_failure_is_kept_with_the_path_to_its_value() {
        let mut r = TypedReader::new(r#"{"inner": {"n": "x"}, "m": true}"#);
        r.begin_struct(&encode_names(["inner", "m"]));
        assert!(r.enter_field(0));
        r.begin_struct(&encode_names(["n"]));
        assert!(r.enter_field(0));
        assert_eq!(r.read_i64("i64"), 0);
        r.leave();
        r.leave();
        assert!(r.enter_field(1) || r.failed());
        assert_eq!(r.read_i64("i64"), 0);
        assert_eq!(
            message(&r).as_deref(),
            Some("inner.n: expected i64, found string")
        );
    }

    #[test]
    fn a_path_names_elements_by_index_and_odd_keys_in_quotes() {
        let mut r = TypedReader::new(r#"{"a b": [1, {"q\"": 2.5}]}"#);
        r.begin_struct(&encode_names(["a b"]));
        assert!(r.enter_field(0));
        assert_eq!(r.begin_array("array"), 2);
        r.enter_elem(1);
        r.begin_struct(&encode_names(["q\""]));
        assert!(r.enter_field(0));
        r.read_i64("i64");
        assert_eq!(
            message(&r).as_deref(),
            Some(r#"["a b"][1]["q\""]: expected i64, found 2.5"#)
        );
    }

    #[test]
    fn a_struct_from_a_value_that_is_not_an_object_fails() {
        let mut r = TypedReader::new(r#"{"inner": [1]}"#);
        r.begin_struct(&encode_names(["inner"]));
        assert!(r.enter_field(0));
        r.begin_struct(&encode_names(["n"]));
        assert!(!r.enter_field(0));
        assert_eq!(
            message(&r).as_deref(),
            Some("inner: expected object, found array")
        );
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
        assert!(r.enter_field(0));
        assert_eq!(r.read_i64("i64"), 2);
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
