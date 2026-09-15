fn completion_item(label: &str, kind: u32) -> Value {
    let mut item = BTreeMap::new();
    item.insert("label".to_string(), Value::String(label.to_string()));
    item.insert("kind".to_string(), Value::Number(f64::from(kind)));
    Value::Object(item)
}

fn completion_item_local(label: &str) -> Value {
    let mut item = BTreeMap::new();
    item.insert("label".to_string(), Value::String(label.to_string()));
    item.insert("kind".to_string(), Value::Number(6.0)); // Variable
    Value::Object(item)
}

fn completion_item_for(doc: &DocumentAnalysis, label: &str, _prefix: &str) -> Value {
    // If the index has a real DefinitionInfo for this name, decorate
    // the completion entry with the kind, signature, and docs so the
    // editor can render a richer popup.
    let mut item = BTreeMap::new();
    item.insert("label".to_string(), Value::String(label.to_string()));
    let mut kind = 3.0; // Function (LSP CompletionItemKind::Function)
    for (_, info) in doc.index_pairs() {
        if info.name == label {
            kind = match info.kind {
                DefKind::Fn => 3.0,
                DefKind::Struct => 22.0,
                DefKind::Enum => 13.0,
                DefKind::Trait => 8.0,
                DefKind::TypeAlias => 25.0,
                DefKind::Const => 21.0,
                DefKind::Static => 6.0,
                DefKind::Mod => 9.0,
                DefKind::Variant => 20.0,
                DefKind::TypeParam => 25.0,
            };
            if !info.signature.is_empty() {
                item.insert("detail".to_string(), Value::String(info.signature.clone()));
            }
            if !info.docs.is_empty() {
                let mut docs = BTreeMap::new();
                docs.insert("kind".to_string(), Value::String("markdown".to_string()));
                docs.insert("value".to_string(), Value::String(info.docs.clone()));
                item.insert("documentation".to_string(), Value::Object(docs));
            }
            break;
        }
    }
    item.insert("kind".to_string(), Value::Number(kind));
    Value::Object(item)
}

fn member_to_completion(spec: &MemberSpec) -> Value {
    let mut item = BTreeMap::new();
    item.insert("label".to_string(), Value::String(spec.name.clone()));
    item.insert("kind".to_string(), Value::Number(f64::from(spec.kind)));
    if let Some(detail) = &spec.detail {
        item.insert("detail".to_string(), Value::String(detail.clone()));
    }
    if let Some(doc) = &spec.doc {
        let mut docs = BTreeMap::new();
        docs.insert("kind".to_string(), Value::String("markdown".to_string()));
        docs.insert("value".to_string(), Value::String(doc.clone()));
        item.insert("documentation".to_string(), Value::Object(docs));
    }
    // Function-like members carry a snippet so the editor opens the
    // parens for the user. Module / type / const stay as bare names.
    if spec.kind == 3 {
        item.insert(
            "insertText".to_string(),
            Value::String(format!("{}($0)", spec.name)),
        );
        item.insert("insertTextFormat".to_string(), Value::Number(2.0));
    }
    Value::Object(item)
}

fn completion_function_item_with_snippet(name: &str) -> Value {
    let mut item = BTreeMap::new();
    item.insert("label".to_string(), Value::String(name.to_string()));
    item.insert("kind".to_string(), Value::Number(3.0));
    item.insert(
        "insertText".to_string(),
        Value::String(format!("{name}($0)")),
    );
    item.insert("insertTextFormat".to_string(), Value::Number(2.0));
    Value::Object(item)
}

/// Receiver-side identification used to look up methods.
#[derive(Debug, Clone)]
struct ReceiverDescriptor {
    /// Builtin classification (`Vec` / `String` / `Map` / `Option` / `Result` / …).
    builtin: BuiltinReceiver,
    /// User-facing type name extracted from `let r: Foo = …` or
    /// `struct Foo { … }`. Used to match `impl Foo` blocks.
    type_name: Option<String>,
    /// Whether method completion may offer an `&mut self` operation.
    writable: bool,
}

impl ReceiverDescriptor {
    fn type_name(&self) -> Option<&str> {
        self.type_name.as_deref()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuiltinReceiver {
    Integer,
    Vec,
    Array,
    Slice,
    String,
    HashMap,
    HashSet,
    BTreeSet,
    VecDeque,
    VecQueue,
    VecStack,
    MaxHeap,
    MinHeap,
    Iterator,
    Option,
    Result,
    Unknown,
}

fn receiver_descriptor(doc: &DocumentAnalysis, offset: u32) -> ReceiverDescriptor {
    // Locate the receiver expression: walk left from the dot in the
    // source, skipping the suffix word the user is typing.
    let bytes = doc.source().as_bytes();
    let mut idx = (offset as usize).min(bytes.len());
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    while idx > 0 && is_word(bytes[idx - 1]) {
        idx -= 1;
    }
    if idx == 0 || bytes[idx - 1] != b'.' {
        return ReceiverDescriptor {
            builtin: BuiltinReceiver::Unknown,
            type_name: None,
            writable: false,
        };
    }
    // Walk left across the receiver expression (very conservative: stop
    // at common statement boundaries / unmatched parens).
    let dot_pos = idx - 1;
    let mut start = dot_pos;
    let mut depth: i32 = 0;
    while start > 0 {
        let b = bytes[start - 1];
        match b {
            b')' | b']' | b'}' => depth += 1,
            b'(' | b'[' | b'{' => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            b';' | b',' | b'\n' if depth == 0 => break,
            _ => {}
        }
        start -= 1;
    }
    let receiver = std::str::from_utf8(&bytes[start..dot_pos])
        .unwrap_or("")
        .trim();
    classify_receiver(doc, receiver)
}

fn classify_receiver(doc: &DocumentAnalysis, expr: &str) -> ReceiverDescriptor {
    let head = expr.trim();
    // Direct string literal.
    if head.starts_with('"') {
        return ReceiverDescriptor {
            builtin: BuiltinReceiver::String,
            type_name: Some("String".to_string()),
            writable: false,
        };
    }
    if head.starts_with("#[") {
        return ReceiverDescriptor {
            builtin: BuiltinReceiver::Vec,
            type_name: None,
            writable: false,
        };
    }
    if head.starts_with("#{") {
        return ReceiverDescriptor {
            builtin: BuiltinReceiver::HashSet,
            type_name: Some("Set".to_string()),
            writable: false,
        };
    }
    if head.starts_with("^[") {
        return ReceiverDescriptor {
            builtin: BuiltinReceiver::MaxHeap,
            type_name: Some("MaxHeap".to_string()),
            writable: false,
        };
    }
    if head.starts_with("_[") {
        return ReceiverDescriptor {
            builtin: BuiltinReceiver::MinHeap,
            type_name: Some("MinHeap".to_string()),
            writable: false,
        };
    }
    if head.starts_with("<[") {
        return ReceiverDescriptor {
            builtin: BuiltinReceiver::VecQueue,
            type_name: Some("Queue".to_string()),
            writable: false,
        };
    }
    if head.starts_with('[') && head.ends_with("]>") {
        return ReceiverDescriptor {
            builtin: BuiltinReceiver::VecStack,
            type_name: Some("Stack".to_string()),
            writable: false,
        };
    }
    if head.starts_with("vec![") {
        return ReceiverDescriptor {
            builtin: BuiltinReceiver::Vec,
            type_name: None,
            writable: false,
        };
    }
    if head.starts_with('{') {
        return ReceiverDescriptor {
            builtin: BuiltinReceiver::HashMap,
            type_name: Some("Map".to_string()),
            writable: false,
        };
    }
    // A plain bracket literal is a fixed array; `#[...]` above is the Vec form.
    if head.starts_with('[') {
        return ReceiverDescriptor {
            builtin: BuiltinReceiver::Array,
            type_name: None,
            writable: false,
        };
    }
    if head.parse::<i64>().is_ok()
        || ["i8", "i16", "i32", "i64", "isize", "u8", "u16", "u32", "u64", "usize"]
            .iter()
            .any(|suffix| {
                head.strip_suffix(suffix)
                    .is_some_and(|number| number.parse::<i64>().is_ok())
            })
    {
        return ReceiverDescriptor {
            builtin: BuiltinReceiver::Integer,
            type_name: Some("i64".to_string()),
            writable: false,
        };
    }
    // Identifier - try resolving via let-binding type annotation.
    if let Some(name) = identifier_token(head) {
        if let Some(ty) = lookup_let_annotation(doc.source(), name) {
            let mut descriptor = classify_type_string(&ty);
            descriptor.writable = ty.trim_start().starts_with("&mut ")
                || lookup_let_binding_is_mutable(doc.source(), name);
            return descriptor;
        }
    }
    ReceiverDescriptor {
        builtin: BuiltinReceiver::Unknown,
        type_name: None,
        writable: false,
    }
}

fn classify_type_string(ty: &str) -> ReceiverDescriptor {
    let ty = ty.trim();
    let head = ty
        .trim_start_matches(['&', '*', ' '])
        .trim_end_matches([',', ';', ' ']);
    let head = head.strip_prefix("mut ").unwrap_or(head);
    if head.starts_with("Vec<") {
        return receiver_desc(BuiltinReceiver::Vec, None);
    }
    if type_name_matches(head, &["Deque"]) {
        return receiver_desc(BuiltinReceiver::VecDeque, Some("Deque"));
    }
    if type_name_matches(head, &["Queue"]) {
        return receiver_desc(BuiltinReceiver::VecQueue, Some("Queue"));
    }
    if type_name_matches(head, &["Stack"]) {
        return receiver_desc(BuiltinReceiver::VecStack, Some("Stack"));
    }
    if type_name_matches(head, &["MaxHeap"]) {
        return receiver_desc(BuiltinReceiver::MaxHeap, Some("MaxHeap"));
    }
    if type_name_matches(head, &["MinHeap"]) {
        return receiver_desc(BuiltinReceiver::MinHeap, Some("MinHeap"));
    }
    if head.starts_with('[') {
        return ReceiverDescriptor {
            builtin: if head.contains(';') {
                BuiltinReceiver::Array
            } else {
                BuiltinReceiver::Slice
            },
            type_name: None,
            writable: false,
        };
    }
    if head.starts_with("Map<") || head.starts_with("HashMap<") {
        return receiver_desc(BuiltinReceiver::HashMap, Some("Map"));
    }
    if head.starts_with("Set<") || head.starts_with("HashSet<") {
        return receiver_desc(BuiltinReceiver::HashSet, Some("Set"));
    }
    if type_name_matches(head, &["BTreeSet"]) {
        return receiver_desc(BuiltinReceiver::BTreeSet, Some("BTreeSet"));
    }
    if head == "String" || head == "&str" || head == "str" {
        return receiver_desc(BuiltinReceiver::String, Some("String"));
    }
    if matches!(
        head,
        "i8" | "i16" | "i32" | "i64" | "isize" | "u8" | "u16" | "u32" | "u64" | "usize"
    ) {
        return receiver_desc(BuiltinReceiver::Integer, Some(head));
    }
    if head.starts_with("Option<") || head == "Option" {
        return receiver_desc(BuiltinReceiver::Option, Some("Option"));
    }
    if head.starts_with("Iterator<") || head == "Iterator" {
        return receiver_desc(BuiltinReceiver::Iterator, Some("Iterator"));
    }
    if head.starts_with("Result<") || head == "Result" {
        return receiver_desc(BuiltinReceiver::Result, Some("Result"));
    }
    let bare = head.split(['<', '[', '(', ' ']).next().unwrap_or(head);
    if bare.is_empty() {
        receiver_desc(BuiltinReceiver::Unknown, None)
    } else {
        receiver_desc(BuiltinReceiver::Unknown, Some(bare))
    }
}

fn receiver_desc(builtin: BuiltinReceiver, type_name: Option<&str>) -> ReceiverDescriptor {
    ReceiverDescriptor {
        builtin,
        type_name: type_name.map(str::to_string),
        writable: false,
    }
}

fn type_name_matches(head: &str, names: &[&str]) -> bool {
    names
        .iter()
        .any(|name| head == *name || head.strip_prefix(name).is_some_and(|rest| rest.starts_with('<')))
}

fn identifier_token(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return None;
    }
    if trimmed.chars().next()?.is_ascii_digit() {
        return None;
    }
    Some(trimmed)
}

/// Looks for a `let <name>: <type> = ...` binding for `name` in the
/// document and returns the rendered type spelling.
fn lookup_let_annotation(source: &str, name: &str) -> Option<String> {
    let needle = format!("let {name}");
    let needle_mut = format!("let mut {name}");
    let mut start = 0usize;
    while start < source.len() {
        let remaining = &source[start..];
        let position = match (remaining.find(&needle), remaining.find(&needle_mut)) {
            (Some(a), Some(b)) => a.min(b),
            (Some(position), None) | (None, Some(position)) => position,
            (None, None) => return None,
        };
        let absolute = start + position;
        let head_ok = absolute == 0
            || !matches!(
                source.as_bytes()[absolute - 1],
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_'
            );
        if !head_ok {
            start = absolute + 1;
            continue;
        }
        // After the `let <name>` (or `let mut <name>`), look for a `:`
        // that starts the type annotation, stopping at `=` or newline.
        let after = if source[absolute..].starts_with(&needle_mut) {
            absolute + needle_mut.len()
        } else {
            absolute + needle.len()
        };
        let tail = &source[after..];
        // Strict word boundary: next char must not be word.
        if tail
            .as_bytes()
            .first()
            .copied()
            .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            start = absolute + 1;
            continue;
        }
        let stripped = tail.trim_start();
        if let Some(rest_with_ws) = stripped.strip_prefix(':') {
            let rest = rest_with_ws.trim_start();
            // Capture until `=` or newline at top depth.
            let mut depth: i32 = 0;
            let mut end = 0usize;
            for (i, ch) in rest.char_indices() {
                match ch {
                    '<' | '(' | '[' => depth += 1,
                    '>' | ')' | ']' => depth -= 1,
                    '=' | '\n' | ';' if depth == 0 => {
                        end = i;
                        break;
                    }
                    _ => {}
                }
            }
            if end == 0 {
                end = rest.len();
            }
            let ty = rest[..end].trim().trim_end_matches(',').trim();
            if !ty.is_empty() {
                return Some(ty.to_string());
            }
        }
        start = absolute + 1;
    }
    None
}

fn lookup_let_binding_is_mutable(source: &str, name: &str) -> bool {
    let mutable = format!("let mut {name}");
    let immutable = format!("let {name}");
    source.lines().rev().find_map(|line| {
        let line = line.trim_start();
        if line.starts_with(&mutable)
            && line[mutable.len()..]
                .chars()
                .next()
                .is_none_or(|ch| !ch.is_ascii_alphanumeric() && ch != '_')
        {
            Some(true)
        } else if line.starts_with(&immutable)
            && line[immutable.len()..]
                .chars()
                .next()
                .is_none_or(|ch| !ch.is_ascii_alphanumeric() && ch != '_')
        {
            Some(false)
        } else {
            None
        }
    }) == Some(true)
}

#[derive(Debug, Clone, Copy)]
struct BuiltinMethod {
    name: &'static str,
    signature: &'static str,
    doc: &'static str,
    snippet: &'static str,
}

const VEC_METHODS: &[BuiltinMethod] = &[
    BuiltinMethod {
        name: "push",
        signature: "fn push(&mut self, value: T)",
        doc: "Appends `value` to the back of the vec.",
        snippet: "push($0)",
    },
    BuiltinMethod {
        name: "pop",
        signature: "fn pop(&mut self) -> Option<T>",
        doc: "Removes the last element and returns it, or `None` when empty.",
        snippet: "pop()$0",
    },
    BuiltinMethod {
        name: "clear",
        signature: "fn clear(&mut self)",
        doc: "Removes every element, leaving the vec at length 0.",
        snippet: "clear()$0",
    },
    BuiltinMethod {
        name: "clone",
        signature: "fn clone(&self) -> Self",
        doc: "Clones every element into a new vec.",
        snippet: "clone()$0",
    },
    BuiltinMethod {
        name: "insert",
        signature: "fn insert(&mut self, index: i64, value: T) -> Result<(), errors::Error>",
        doc: "Inserts a value at a checked index.",
        snippet: "insert($1, $2)$0",
    },
    BuiltinMethod {
        name: "remove",
        signature: "fn remove(&mut self, index: i64) -> Result<T, errors::Error>",
        doc: "Removes and returns the value at a checked index.",
        snippet: "remove($0)",
    },
    BuiltinMethod {
        name: "extend",
        signature: "fn extend(&mut self, values: Vec<T>)",
        doc: "Appends every value from another vector.",
        snippet: "extend($0)",
    },
    BuiltinMethod {
        name: "extend_from_slice",
        signature: "fn extend_from_slice(&mut self, values: [T])",
        doc: "Appends cloned values from a slice.",
        snippet: "extend_from_slice($0)",
    },
    BuiltinMethod {
        name: "truncate",
        signature: "fn truncate(&mut self, len: i64)",
        doc: "Shortens the vector to at most `len` elements.",
        snippet: "truncate($0)",
    },
    BuiltinMethod {
        name: "reserve",
        signature: "fn reserve(&mut self, capacity: i64)",
        doc: "Ensures at least the requested total capacity.",
        snippet: "reserve($0)",
    },
    BuiltinMethod {
        name: "reserve_exact",
        signature: "fn reserve_exact(&mut self, capacity: i64)",
        doc: "Reserves the requested total capacity without extra growth.",
        snippet: "reserve_exact($0)",
    },
    BuiltinMethod {
        name: "capacity",
        signature: "fn capacity(&self) -> i64",
        doc: "Returns the vector's allocated element capacity.",
        snippet: "capacity()$0",
    },
];

const ARRAY_SLICE_METHODS: &[BuiltinMethod] = &[
    BuiltinMethod {
        name: "len",
        signature: "fn len(&self) -> usize",
        doc: "Returns the fixed number of elements in the sequence view.",
        snippet: "len()$0",
    },
    BuiltinMethod {
        name: "is_empty",
        signature: "fn is_empty(&self) -> bool",
        doc: "Returns true when the slice has no elements.",
        snippet: "is_empty()$0",
    },
    BuiltinMethod {
        name: "slice",
        signature: "fn slice(&self, start: i64, end: i64) -> Result<Vec<T>, errors::Error>",
        doc: "Returns a checked copy of the selected range.",
        snippet: "slice($1, $2)$0",
    },
    BuiltinMethod {
        name: "first",
        signature: "fn first(&self) -> Option<T>",
        doc: "Returns the first element, or `None` when empty.",
        snippet: "first()$0",
    },
    BuiltinMethod {
        name: "last",
        signature: "fn last(&self) -> Option<T>",
        doc: "Returns the last element, or `None` when empty.",
        snippet: "last()$0",
    },
    BuiltinMethod {
        name: "get",
        signature: "fn get(&self, index: i64) -> Option<T>",
        doc: "Returns the element at `index`, or `None` when out of bounds.",
        snippet: "get($0)",
    },
    BuiltinMethod {
        name: "iter",
        signature: "fn iter(&self) -> Iter<T>",
        doc: "Returns an iterator over borrowed elements.",
        snippet: "iter()$0",
    },
    BuiltinMethod {
        name: "contains",
        signature: "fn contains(&self, value: T) -> bool",
        doc: "Returns true when the sequence contains an equal element.",
        snippet: "contains($0)",
    },
    BuiltinMethod {
        name: "index_of",
        signature: "fn index_of(&self, value: T) -> Option<i64>",
        doc: "Returns the first matching index when present.",
        snippet: "index_of($0)",
    },
    BuiltinMethod {
        name: "count_of",
        signature: "fn count_of(&self, value: T) -> i64",
        doc: "Counts elements equal to `value`.",
        snippet: "count_of($0)",
    },
    BuiltinMethod {
        name: "sort",
        signature: "fn sort(&mut self)",
        doc: "Sorts existing elements in place without resizing.",
        snippet: "sort()$0",
    },
    BuiltinMethod {
        name: "sort_by",
        signature: "fn sort_by(&mut self, cmp: fn(T, T) -> i64)",
        doc: "Sorts existing elements in place with a comparator.",
        snippet: "sort_by($0)",
    },
    BuiltinMethod {
        name: "sort_by_key",
        signature: "fn sort_by_key<K>(&mut self, key: fn(T) -> K)",
        doc: "Sorts existing elements in place by a derived key.",
        snippet: "sort_by_key($0)",
    },
    BuiltinMethod {
        name: "reverse",
        signature: "fn reverse(&mut self)",
        doc: "Reverses existing elements in place without resizing.",
        snippet: "reverse()$0",
    },
    BuiltinMethod {
        name: "swap",
        signature: "fn swap(&mut self, a: i64, b: i64)",
        doc: "Swaps two existing elements; an index outside [0, len) panics.",
        snippet: "swap($1, $2)$0",
    },
    BuiltinMethod {
        name: "fill",
        signature: "fn fill(&mut self, value: T)",
        doc: "Clones a value into every existing element without resizing.",
        snippet: "fill($0)",
    },
    BuiltinMethod {
        name: "windows",
        signature: "fn windows(&self, size: i64) -> Vec<Vec<T>>",
        doc: "Returns overlapping windows of `size` elements.",
        snippet: "windows($0)",
    },
    BuiltinMethod {
        name: "chunks",
        signature: "fn chunks(&self, size: i64) -> Vec<Vec<T>>",
        doc: "Returns consecutive chunks of at most `size` elements.",
        snippet: "chunks($0)",
    },
    BuiltinMethod {
        name: "join",
        signature: "fn join(&self, separator: String) -> String",
        doc: "Joins displayable elements with a separator.",
        snippet: "join($0)",
    },
    BuiltinMethod {
        name: "to_vec",
        signature: "fn to_vec(&self) -> Vec<T>",
        doc: "Copies the elements into a new vector.",
        snippet: "to_vec()$0",
    },
];

const STRING_METHODS: &[BuiltinMethod] = &[
    BuiltinMethod {
        name: "len",
        signature: "fn len(&self) -> usize",
        doc: "Length of the string in bytes.",
        snippet: "len()$0",
    },
    BuiltinMethod {
        name: "is_empty",
        signature: "fn is_empty(&self) -> bool",
        doc: "Returns `true` for the empty string.",
        snippet: "is_empty()$0",
    },
    BuiltinMethod {
        name: "to_uppercase",
        signature: "fn to_uppercase(&self) -> String",
        doc: "Returns the upper-cased clone of the string.",
        snippet: "to_uppercase()$0",
    },
    BuiltinMethod {
        name: "to_lowercase",
        signature: "fn to_lowercase(&self) -> String",
        doc: "Returns the lower-cased clone of the string.",
        snippet: "to_lowercase()$0",
    },
    BuiltinMethod {
        name: "trim",
        signature: "fn trim(&self) -> String",
        doc: "Returns the string with leading + trailing whitespace stripped.",
        snippet: "trim()$0",
    },
    BuiltinMethod {
        name: "split",
        signature: "fn split(&self, sep: String) -> Vec<String>",
        doc: "Splits on every occurrence of `sep`.",
        snippet: "split(\"$0\")",
    },
    BuiltinMethod {
        name: "lines",
        signature: "fn lines(&self) -> Vec<String>",
        doc: "Splits the string on `\\n`.",
        snippet: "lines()$0",
    },
    BuiltinMethod {
        name: "starts_with",
        signature: "fn starts_with(&self, prefix: String) -> bool",
        doc: "True when the string begins with `prefix`.",
        snippet: "starts_with(\"$0\")",
    },
    BuiltinMethod {
        name: "ends_with",
        signature: "fn ends_with(&self, suffix: String) -> bool",
        doc: "True when the string ends with `suffix`.",
        snippet: "ends_with(\"$0\")",
    },
    BuiltinMethod {
        name: "contains",
        signature: "fn contains(&self, needle: String) -> bool",
        doc: "True when `needle` appears anywhere in the string.",
        snippet: "contains(\"$0\")",
    },
    BuiltinMethod {
        name: "repeat",
        signature: "fn repeat(&self, n: i64) -> String",
        doc: "Returns the string repeated `n` times.",
        snippet: "repeat($0)",
    },
    BuiltinMethod {
        name: "to_string",
        signature: "fn to_string(&self) -> String",
        doc: "Returns a fresh owned copy.",
        snippet: "to_string()$0",
    },
    BuiltinMethod {
        name: "as_bytes",
        signature: "fn as_bytes(&self) -> Vec<u8>",
        doc: "Materializes the UTF-8 bytes in a vector.",
        snippet: "as_bytes()$0",
    },
];

const HASHMAP_METHODS: &[BuiltinMethod] = &[
    BuiltinMethod {
        name: "insert",
        signature: "fn insert(&mut self, key: K, value: V) -> Option<V>",
        doc: "Inserts a key/value pair, returning the previous value (if any).",
        snippet: "insert($1, $2)$0",
    },
    BuiltinMethod {
        name: "get",
        signature: "fn get(&self, key: K) -> Option<V>",
        doc: "Looks up `key`.",
        snippet: "get($0)",
    },
    BuiltinMethod {
        name: "get_or",
        signature: "fn get_or(&self, key: K, default: V) -> V",
        doc: "Looks up `key`, returning `default` when absent.",
        snippet: "get_or($1, $2)$0",
    },
    BuiltinMethod {
        name: "remove",
        signature: "fn remove(&mut self, key: K) -> Option<V>",
        doc: "Removes `key`'s entry, returning the removed value.",
        snippet: "remove($0)",
    },
    BuiltinMethod {
        name: "len",
        signature: "fn len(&self) -> usize",
        doc: "Number of entries.",
        snippet: "len()$0",
    },
    BuiltinMethod {
        name: "is_empty",
        signature: "fn is_empty(&self) -> bool",
        doc: "Returns `true` when there are no entries.",
        snippet: "is_empty()$0",
    },
    BuiltinMethod {
        name: "contains_key",
        signature: "fn contains_key(&self, key: K) -> bool",
        doc: "Returns `true` when an entry for `key` exists.",
        snippet: "contains_key($0)",
    },
    BuiltinMethod {
        name: "clear",
        signature: "fn clear(&mut self)",
        doc: "Removes every entry.",
        snippet: "clear()$0",
    },
    BuiltinMethod {
        name: "keys",
        signature: "fn keys(&self) -> Iter<K>",
        doc: "Iterator over keys.",
        snippet: "keys()$0",
    },
    BuiltinMethod {
        name: "values",
        signature: "fn values(&self) -> Iter<V>",
        doc: "Iterator over values.",
        snippet: "values()$0",
    },
];

const HASHSET_METHODS: &[BuiltinMethod] = &[
    BuiltinMethod {
        name: "insert",
        signature: "fn insert(&mut self, value: T) -> bool",
        doc: "Adds a value and reports whether it was newly inserted.",
        snippet: "insert($0)",
    },
    BuiltinMethod {
        name: "remove",
        signature: "fn remove(&mut self, value: T) -> bool",
        doc: "Removes a value and reports whether it was present.",
        snippet: "remove($0)",
    },
    BuiltinMethod {
        name: "contains",
        signature: "fn contains(&self, value: T) -> bool",
        doc: "Reports whether the set contains a value.",
        snippet: "contains($0)",
    },
    BuiltinMethod {
        name: "union",
        signature: "fn union(&self, other: Set<T>) -> Set<T>",
        doc: "Returns values present in either set.",
        snippet: "union($0)",
    },
    BuiltinMethod {
        name: "intersection",
        signature: "fn intersection(&self, other: Set<T>) -> Set<T>",
        doc: "Returns values present in both sets.",
        snippet: "intersection($0)",
    },
    BuiltinMethod {
        name: "difference",
        signature: "fn difference(&self, other: Set<T>) -> Set<T>",
        doc: "Returns values present only in this set.",
        snippet: "difference($0)",
    },
    BuiltinMethod {
        name: "symmetric_difference",
        signature: "fn symmetric_difference(&self, other: Set<T>) -> Set<T>",
        doc: "Returns values present in exactly one set.",
        snippet: "symmetric_difference($0)",
    },
    BuiltinMethod {
        name: "len",
        signature: "fn len(&self) -> i64",
        doc: "Returns the number of values.",
        snippet: "len()$0",
    },
    BuiltinMethod {
        name: "is_empty",
        signature: "fn is_empty(&self) -> bool",
        doc: "Reports whether the set has no values.",
        snippet: "is_empty()$0",
    },
    BuiltinMethod {
        name: "clear",
        signature: "fn clear(&mut self)",
        doc: "Removes every value.",
        snippet: "clear()$0",
    },
    BuiltinMethod {
        name: "iter",
        signature: "fn iter(&self) -> Iterator<T>",
        doc: "Walks the values; every sequence operation on a set starts here.",
        snippet: "iter()$0",
    },
    BuiltinMethod {
        name: "to_vec",
        signature: "fn to_vec(&self) -> Vec<T>",
        doc: "Returns the set values as a vector, in the same unordered walk.",
        snippet: "to_vec()$0",
    },
    BuiltinMethod {
        name: "is_subset",
        signature: "fn is_subset(&self, other: Set<T>) -> bool",
        doc: "Reports whether every value is in the other set.",
        snippet: "is_subset($0)",
    },
    BuiltinMethod {
        name: "is_superset",
        signature: "fn is_superset(&self, other: Set<T>) -> bool",
        doc: "Reports whether this set contains every value in the other set.",
        snippet: "is_superset($0)",
    },
    BuiltinMethod {
        name: "is_disjoint",
        signature: "fn is_disjoint(&self, other: Set<T>) -> bool",
        doc: "Reports whether the sets share no values.",
        snippet: "is_disjoint($0)",
    },
];

const VECDEQUE_METHODS: &[BuiltinMethod] = &[
    BuiltinMethod {
        name: "push_back",
        signature: "fn push_back(&mut self, value: i64)",
        doc: "Appends a value to the back.",
        snippet: "push_back($0)",
    },
    BuiltinMethod {
        name: "push_front",
        signature: "fn push_front(&mut self, value: i64)",
        doc: "Appends a value to the front.",
        snippet: "push_front($0)",
    },
    BuiltinMethod {
        name: "pop_front",
        signature: "fn pop_front(&mut self) -> Option<i64>",
        doc: "Removes and returns the front value when present.",
        snippet: "pop_front()$0",
    },
    BuiltinMethod {
        name: "pop_back",
        signature: "fn pop_back(&mut self) -> Option<i64>",
        doc: "Removes and returns the back value when present.",
        snippet: "pop_back()$0",
    },
    BuiltinMethod {
        name: "peek_front",
        signature: "fn peek_front(&self) -> Option<i64>",
        doc: "Returns the front value without removing it.",
        snippet: "peek_front()$0",
    },
    BuiltinMethod {
        name: "peek_back",
        signature: "fn peek_back(&self) -> Option<i64>",
        doc: "Returns the back value without removing it.",
        snippet: "peek_back()$0",
    },
    BuiltinMethod {
        name: "len",
        signature: "fn len(&self) -> i64",
        doc: "Returns the number of values.",
        snippet: "len()$0",
    },
    BuiltinMethod {
        name: "is_empty",
        signature: "fn is_empty(&self) -> bool",
        doc: "Returns true when the deque has no values.",
        snippet: "is_empty()$0",
    },
    BuiltinMethod {
        name: "clear",
        signature: "fn clear(&mut self)",
        doc: "Removes all values.",
        snippet: "clear()$0",
    },
];

const VEC_QUEUE_STACK_METHODS: &[BuiltinMethod] = &[
    BuiltinMethod {
        name: "push",
        signature: "fn push(&mut self, value: i64)",
        doc: "Pushes a value.",
        snippet: "push($0)",
    },
    BuiltinMethod {
        name: "pop",
        signature: "fn pop(&mut self) -> Option<i64>",
        doc: "Removes and returns a value when present.",
        snippet: "pop()$0",
    },
    BuiltinMethod {
        name: "peek",
        signature: "fn peek(&self) -> Option<i64>",
        doc: "Returns the next value without removing it.",
        snippet: "peek()$0",
    },
    BuiltinMethod {
        name: "len",
        signature: "fn len(&self) -> i64",
        doc: "Returns the number of values.",
        snippet: "len()$0",
    },
    BuiltinMethod {
        name: "is_empty",
        signature: "fn is_empty(&self) -> bool",
        doc: "Returns true when the collection has no values.",
        snippet: "is_empty()$0",
    },
    BuiltinMethod {
        name: "clear",
        signature: "fn clear(&mut self)",
        doc: "Removes all values.",
        snippet: "clear()$0",
    },
];

const HEAP_METHODS: &[BuiltinMethod] = &[
    BuiltinMethod {
        name: "push",
        signature: "fn push(&mut self, value: T)",
        doc: "Pushes a value onto the heap.",
        snippet: "push($0)",
    },
    BuiltinMethod {
        name: "pop",
        signature: "fn pop(&mut self) -> Option<T>",
        doc: "Removes and returns the root value when present.",
        snippet: "pop()$0",
    },
    BuiltinMethod {
        name: "peek",
        signature: "fn peek(&self) -> Option<T>",
        doc: "Returns the root value without removing it.",
        snippet: "peek()$0",
    },
    BuiltinMethod {
        name: "len",
        signature: "fn len(&self) -> i64",
        doc: "Returns the number of values.",
        snippet: "len()$0",
    },
    BuiltinMethod {
        name: "is_empty",
        signature: "fn is_empty(&self) -> bool",
        doc: "Returns true when the heap has no values.",
        snippet: "is_empty()$0",
    },
    BuiltinMethod {
        name: "clear",
        signature: "fn clear(&mut self)",
        doc: "Removes all values.",
        snippet: "clear()$0",
    },
];

const OPTION_METHODS: &[BuiltinMethod] = &[
    BuiltinMethod {
        name: "and_then",
        signature: "fn and_then<U>(self, f: fn(T) -> Option<U>) -> Option<U>",
        doc: "Chains the contained value through an Option-returning function.",
        snippet: "and_then(|value| $0)",
    },
    BuiltinMethod {
        name: "expect",
        signature: "fn expect(self, message: String) -> T",
        doc: "Returns the contained value or panics with the supplied message.",
        snippet: "expect($0)",
    },
    BuiltinMethod {
        name: "filter",
        signature: "fn filter(self, predicate: fn(T) -> bool) -> Option<T>",
        doc: "Keeps the value only when the predicate accepts it.",
        snippet: "filter(|value| $0)",
    },
    BuiltinMethod {
        name: "flatten",
        signature: "fn flatten(self) -> Option<T>",
        doc: "Flattens one nested Option level.",
        snippet: "flatten()$0",
    },
    BuiltinMethod {
        name: "is_some",
        signature: "fn is_some(&self) -> bool",
        doc: "Returns `true` when the option is `Some`.",
        snippet: "is_some()$0",
    },
    BuiltinMethod {
        name: "is_none",
        signature: "fn is_none(&self) -> bool",
        doc: "Returns `true` when the option is `None`.",
        snippet: "is_none()$0",
    },
    BuiltinMethod {
        name: "unwrap_or",
        signature: "fn unwrap_or(self, default: T) -> T",
        doc: "Returns the contained value, or `default` if `None`.",
        snippet: "unwrap_or($0)",
    },
    BuiltinMethod {
        name: "iter",
        signature: "fn iter(self) -> Vec<T>",
        doc: "Returns a zero-or-one element sequence.",
        snippet: "iter()$0",
    },
    BuiltinMethod {
        name: "map",
        signature: "fn map<U>(self, f: fn(T) -> U) -> Option<U>",
        doc: "Maps the contained value through `f`.",
        snippet: "map(|x| $0)",
    },
    BuiltinMethod {
        name: "ok_or",
        signature: "fn ok_or<E>(self, err: E) -> Result<T, E>",
        doc: "Converts `Some` to `Ok` and `None` to the supplied error.",
        snippet: "ok_or($0)",
    },
    BuiltinMethod {
        name: "ok_or_else",
        signature: "fn ok_or_else<E>(self, err: fn() -> E) -> Result<T, E>",
        doc: "Converts `Some` to `Ok` and computes an error only for `None`.",
        snippet: "ok_or_else(|| $0)",
    },
    BuiltinMethod {
        name: "or",
        signature: "fn or(self, fallback: Option<T>) -> Option<T>",
        doc: "Returns this option when present, otherwise the fallback.",
        snippet: "or($0)",
    },
    BuiltinMethod {
        name: "or_else",
        signature: "fn or_else(self, fallback: fn() -> Option<T>) -> Option<T>",
        doc: "Computes a fallback only when this option is None.",
        snippet: "or_else(|| $0)",
    },
    BuiltinMethod {
        name: "unwrap",
        signature: "fn unwrap(self) -> T",
        doc: "Returns the contained value or panics if the option is None.",
        snippet: "unwrap()$0",
    },
    BuiltinMethod {
        name: "unwrap_or_else",
        signature: "fn unwrap_or_else(self, fallback: fn() -> T) -> T",
        doc: "Computes a fallback only when this option is None.",
        snippet: "unwrap_or_else(|| $0)",
    },
    BuiltinMethod {
        name: "zip",
        signature: "fn zip<U>(self, other: Option<U>) -> Option<(T, U)>",
        doc: "Pairs the values when both options are present.",
        snippet: "zip($0)",
    },
];

const ITERATOR_METHODS: &[BuiltinMethod] = &[
    BuiltinMethod { name: "map", signature: "fn map<U>(self, f: fn(T) -> U) -> Iterator<U>", doc: "Transforms each item.", snippet: "map(|value| $0)" },
    BuiltinMethod { name: "filter", signature: "fn filter(self, predicate: fn(T) -> bool) -> Iterator<T>", doc: "Keeps accepted items.", snippet: "filter(|value| $0)" },
    BuiltinMethod { name: "fold", signature: "fn fold<U>(self, initial: U, f: fn(U, T) -> U) -> U", doc: "Folds items into one value.", snippet: "fold($1, |acc, value| $0)" },
    BuiltinMethod { name: "collect", signature: "fn collect(self) -> Vec<T>", doc: "Materializes all items.", snippet: "collect()$0" },
    BuiltinMethod { name: "count", signature: "fn count(self) -> i64", doc: "Counts all items.", snippet: "count()$0" },
    BuiltinMethod { name: "sum", signature: "fn sum(self) -> T", doc: "Sums all items.", snippet: "sum()$0" },
    BuiltinMethod { name: "product", signature: "fn product(self) -> T", doc: "Multiplies all items.", snippet: "product()$0" },
    BuiltinMethod { name: "min", signature: "fn min(self) -> Option<T>", doc: "Returns the minimum item.", snippet: "min()$0" },
    BuiltinMethod { name: "max", signature: "fn max(self) -> Option<T>", doc: "Returns the maximum item.", snippet: "max()$0" },
    BuiltinMethod { name: "any", signature: "fn any(self, predicate: fn(T) -> bool) -> bool", doc: "Tests whether any item is accepted.", snippet: "any(|value| $0)" },
    BuiltinMethod { name: "all", signature: "fn all(self, predicate: fn(T) -> bool) -> bool", doc: "Tests whether every item is accepted.", snippet: "all(|value| $0)" },
    BuiltinMethod { name: "find", signature: "fn find(self, predicate: fn(T) -> bool) -> Option<T>", doc: "Returns the first accepted item.", snippet: "find(|value| $0)" },
    BuiltinMethod { name: "take", signature: "fn take(self, count: i64) -> Iterator<T>", doc: "Yields at most count items.", snippet: "take($0)" },
    BuiltinMethod { name: "skip", signature: "fn skip(self, count: i64) -> Iterator<T>", doc: "Skips the first count items.", snippet: "skip($0)" },
    BuiltinMethod { name: "step_by", signature: "fn step_by(self, step: i64) -> Iterator<T>", doc: "Yields every step-th item.", snippet: "step_by($0)" },
    BuiltinMethod { name: "enumerate", signature: "fn enumerate(self) -> Iterator<(i64, T)>", doc: "Pairs items with indexes.", snippet: "enumerate()$0" },
    BuiltinMethod { name: "chain", signature: "fn chain(self, other: Iterator<T>) -> Iterator<T>", doc: "Yields this iterator then another.", snippet: "chain($0)" },
    BuiltinMethod { name: "zip", signature: "fn zip<U>(self, other: Iterator<U>) -> Iterator<(T, U)>", doc: "Pairs items from two iterators.", snippet: "zip($0)" },
    BuiltinMethod { name: "dedup", signature: "fn dedup(self) -> Vec<T>", doc: "Collects adjacent distinct items.", snippet: "dedup()$0" },
    BuiltinMethod { name: "flatten", signature: "fn flatten<U>(self: Iterator<Vec<U>>) -> Vec<U>", doc: "Collects one flattened nesting level.", snippet: "flatten()$0" },
    BuiltinMethod { name: "pairwise", signature: "fn pairwise(self) -> Vec<(T, T)>", doc: "Collects adjacent item pairs.", snippet: "pairwise()$0" },
    BuiltinMethod { name: "windows", signature: "fn windows(self, size: i64) -> Vec<Vec<T>>", doc: "Collects overlapping windows.", snippet: "windows($0)" },
    BuiltinMethod { name: "chunks", signature: "fn chunks(self, size: i64) -> Vec<Vec<T>>", doc: "Collects fixed-size chunks.", snippet: "chunks($0)" },
    BuiltinMethod { name: "rev", signature: "fn rev(self) -> Iterator<T>", doc: "Reverses iteration order.", snippet: "rev()$0" },
];

const RESULT_METHODS: &[BuiltinMethod] = &[
    BuiltinMethod {
        name: "is_ok",
        signature: "fn is_ok(&self) -> bool",
        doc: "Returns `true` when the result is `Ok`.",
        snippet: "is_ok()$0",
    },
    BuiltinMethod {
        name: "is_err",
        signature: "fn is_err(&self) -> bool",
        doc: "Returns `true` when the result is `Err`.",
        snippet: "is_err()$0",
    },
    BuiltinMethod {
        name: "unwrap",
        signature: "fn unwrap(self) -> T",
        doc: "Returns the `Ok` value, panicking on `Err`.",
        snippet: "unwrap()$0",
    },
    BuiltinMethod {
        name: "unwrap_or",
        signature: "fn unwrap_or(self, default: T) -> T",
        doc: "Returns the `Ok` value, or `default` on `Err`.",
        snippet: "unwrap_or($0)",
    },
    BuiltinMethod {
        name: "map",
        signature: "fn map<U>(self, f: fn(T) -> U) -> Result<U, E>",
        doc: "Maps the `Ok` value.",
        snippet: "map(|x| $0)",
    },
    BuiltinMethod {
        name: "map_err",
        signature: "fn map_err<F>(self, f: fn(E) -> F) -> Result<T, F>",
        doc: "Maps the `Err` value.",
        snippet: "map_err(|e| $0)",
    },
];

const ALL_BUILTIN_METHODS: &[BuiltinMethod] = &[
    BuiltinMethod {
        name: "to_string",
        signature: "fn to_string(&self) -> String",
        doc: "Default ToString rendering.",
        snippet: "to_string()$0",
    },
    BuiltinMethod {
        name: "clone",
        signature: "fn clone(&self) -> Self",
        doc: "Clones the receiver.",
        snippet: "clone()$0",
    },
];

const INTEGER_METHODS: &[BuiltinMethod] = &[];

fn builtin_methods_for(receiver: &ReceiverDescriptor) -> Vec<&'static BuiltinMethod> {
    let methods: Vec<_> = match receiver.builtin {
        BuiltinReceiver::Integer => INTEGER_METHODS.iter().collect(),
        BuiltinReceiver::Vec => VEC_METHODS.iter().chain(ARRAY_SLICE_METHODS).collect(),
        BuiltinReceiver::Array | BuiltinReceiver::Slice => ARRAY_SLICE_METHODS.iter().collect(),
        BuiltinReceiver::String => STRING_METHODS.iter().collect(),
        BuiltinReceiver::HashMap => HASHMAP_METHODS.iter().collect(),
        BuiltinReceiver::HashSet => HASHSET_METHODS.iter().collect(),
        BuiltinReceiver::BTreeSet => HASHSET_METHODS.iter().collect(),
        BuiltinReceiver::VecDeque => VECDEQUE_METHODS.iter().collect(),
        BuiltinReceiver::VecQueue | BuiltinReceiver::VecStack => {
            VEC_QUEUE_STACK_METHODS.iter().collect()
        }
        BuiltinReceiver::MaxHeap | BuiltinReceiver::MinHeap => HEAP_METHODS.iter().collect(),
        BuiltinReceiver::Iterator => ITERATOR_METHODS.iter().collect(),
        BuiltinReceiver::Option => OPTION_METHODS.iter().collect(),
        BuiltinReceiver::Result => RESULT_METHODS.iter().collect(),
        BuiltinReceiver::Unknown => Vec::new(),
    };
    methods
        .into_iter()
        .filter(|method| receiver.writable || !method.signature.contains("&mut self"))
        .collect()
}

fn method_completion_item(method: &BuiltinMethod) -> Value {
    let mut item = BTreeMap::new();
    item.insert("label".to_string(), Value::String(method.name.to_string()));
    item.insert("kind".to_string(), Value::Number(2.0)); // Method
    item.insert(
        "detail".to_string(),
        Value::String(method.signature.to_string()),
    );
    let mut docs = BTreeMap::new();
    docs.insert("kind".to_string(), Value::String("markdown".to_string()));
    docs.insert("value".to_string(), Value::String(method.doc.to_string()));
    item.insert("documentation".to_string(), Value::Object(docs));
    item.insert(
        "insertText".to_string(),
        Value::String(method.snippet.to_string()),
    );
    item.insert("insertTextFormat".to_string(), Value::Number(2.0));
    Value::Object(item)
}

#[derive(Debug, Clone)]
struct UserMethod {
    name: String,
    signature: String,
    doc: String,
    is_associated: bool,
}

fn user_methods_for(doc: &DocumentAnalysis, type_name: &str) -> Vec<UserMethod> {
    collect_impl_items(doc, type_name, false)
}

fn user_associated_items(doc: &DocumentAnalysis, type_name: &str) -> Vec<UserMethod> {
    let mut out = collect_impl_items(doc, type_name, true);
    // Add enum variants of `type_name` if any.
    out.extend(enum_variants_for(doc, type_name));
    out
}

fn collect_impl_items(
    doc: &DocumentAnalysis,
    type_name: &str,
    want_associated: bool,
) -> Vec<UserMethod> {
    use gossamer_ast::{FnParam, ImplItem, ItemKind, TypeKind};
    let mut out: Vec<UserMethod> = Vec::new();
    for item in &doc.sf.items {
        let ItemKind::Impl(decl) = &item.kind else {
            continue;
        };
        let TypeKind::Path(path) = &decl.self_ty.kind else {
            continue;
        };
        let Some(seg) = path.segments.last() else {
            continue;
        };
        if seg.name.name != type_name {
            continue;
        }
        for impl_item in &decl.items {
            let ImplItem::Fn(fn_decl) = impl_item else {
                continue;
            };
            let has_receiver = fn_decl
                .params
                .first()
                .is_some_and(|p| matches!(p, FnParam::Receiver(_)));
            let is_associated = !has_receiver;
            if is_associated != want_associated {
                continue;
            }
            let signature = render_user_signature(fn_decl);
            out.push(UserMethod {
                name: fn_decl.name.name.clone(),
                signature,
                doc: String::new(),
                is_associated,
            });
        }
    }
    out
}

fn enum_variants_for(doc: &DocumentAnalysis, type_name: &str) -> Vec<UserMethod> {
    use gossamer_ast::ItemKind;
    let mut out: Vec<UserMethod> = Vec::new();
    for item in &doc.sf.items {
        let ItemKind::Enum(decl) = &item.kind else {
            continue;
        };
        if decl.name.name != type_name {
            continue;
        }
        for variant in &decl.variants {
            out.push(UserMethod {
                name: variant.name.name.clone(),
                signature: format!("{}::{}", type_name, variant.name.name),
                doc: String::new(),
                is_associated: true,
            });
        }
    }
    out
}

fn render_user_signature(decl: &gossamer_ast::FnDecl) -> String {
    use gossamer_ast::FnParam;
    let mut out = String::new();
    out.push_str("fn ");
    out.push_str(&decl.name.name);
    out.push('(');
    let mut first = true;
    for param in &decl.params {
        if !first {
            out.push_str(", ");
        }
        first = false;
        match param {
            FnParam::Receiver(receiver) => out.push_str(receiver.as_str()),
            FnParam::Typed { pattern, ty, .. } => {
                let mut printer = gossamer_ast::Printer::new();
                printer.print_type(ty);
                out.push_str(&pattern_label(pattern));
                out.push_str(": ");
                out.push_str(&printer.finish());
            }
        }
    }
    out.push(')');
    if let Some(ret) = &decl.ret {
        out.push_str(" -> ");
        let mut printer = gossamer_ast::Printer::new();
        printer.print_type(ret);
        out.push_str(&printer.finish());
    }
    out
}

fn pattern_label(pattern: &gossamer_ast::Pattern) -> String {
    use gossamer_ast::PatternKind;
    match &pattern.kind {
        PatternKind::Ident { name, .. } => name.name.clone(),
        _ => "_".to_string(),
    }
}

fn user_method_completion_item(method: &UserMethod) -> Value {
    let mut item = BTreeMap::new();
    item.insert("label".to_string(), Value::String(method.name.clone()));
    let kind = if method.is_associated { 3.0 } else { 2.0 };
    item.insert("kind".to_string(), Value::Number(kind));
    item.insert(
        "detail".to_string(),
        Value::String(method.signature.clone()),
    );
    if !method.doc.is_empty() {
        let mut docs = BTreeMap::new();
        docs.insert("kind".to_string(), Value::String("markdown".to_string()));
        docs.insert("value".to_string(), Value::String(method.doc.clone()));
        item.insert("documentation".to_string(), Value::Object(docs));
    }
    item.insert(
        "insertText".to_string(),
        Value::String(format!("{}($0)", method.name)),
    );
    item.insert("insertTextFormat".to_string(), Value::Number(2.0));
    Value::Object(item)
}

fn workspace_completion_item(item: &WorkspaceItem) -> Value {
    let mut entry = BTreeMap::new();
    entry.insert("label".to_string(), Value::String(item.name.clone()));
    let kind = match item.kind {
        DefKind::Fn => 3.0,
        DefKind::Struct => 22.0,
        DefKind::Enum => 13.0,
        DefKind::Trait => 8.0,
        DefKind::TypeAlias => 25.0,
        DefKind::Const => 21.0,
        DefKind::Static => 6.0,
        DefKind::Mod => 9.0,
        DefKind::Variant => 20.0,
        DefKind::TypeParam => 25.0,
    };
    entry.insert("kind".to_string(), Value::Number(kind));
    if !item.signature.is_empty() {
        entry.insert(
            "detail".to_string(),
            Value::String(format!("{}  // {}", item.signature, short_uri(&item.uri))),
        );
    }
    if !item.doc.is_empty() {
        let mut docs = BTreeMap::new();
        docs.insert("kind".to_string(), Value::String("markdown".to_string()));
        docs.insert("value".to_string(), Value::String(item.doc.clone()));
        entry.insert("documentation".to_string(), Value::Object(docs));
    }
    if matches!(item.kind, DefKind::Fn) {
        entry.insert(
            "insertText".to_string(),
            Value::String(format!("{}($0)", item.name)),
        );
        entry.insert("insertTextFormat".to_string(), Value::Number(2.0));
    }
    Value::Object(entry)
}

fn short_uri(uri: &str) -> String {
    uri.rsplit('/').next().unwrap_or(uri).to_string()
}
