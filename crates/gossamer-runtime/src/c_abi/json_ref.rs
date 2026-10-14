//! Reads of a `json::Value`, over either a tree the program built or a parsed
//! document's tape.
//!
//! A parsed document is read where it lies: the walk that checks it records
//! each value's kind, span, and number in a [`Tape`], and every accessor
//! answers from that tape and the document text. Each read answers what the
//! same read of the document's `serde_json::Value` answers: object members in
//! key order with the last of equal keys kept, numbers as serde_json holds
//! them.

use std::borrow::Cow;
use std::sync::OnceLock;

use gossamer_core::json::{Tape, TapeKind, TapeNode};

/// An object with at most this many members is searched in place; a larger
/// one through its sorted member index.
const SCAN_MEMBERS: u32 = 8;

/// The key nodes of an object, in key order, the last of equal keys kept.
type MemberIndex = OnceLock<Box<[u32]>>;

/// A parsed document: its text, its tape, and each object's member index,
/// built on first need.
pub(crate) struct ParsedTape {
    text: Box<str>,
    tape: Tape,
    /// One slot per object, the table itself made by the first read that
    /// needs any index.
    members: OnceLock<Box<[MemberIndex]>>,
}

impl ParsedTape {
    /// The document `text` read into `tape`.
    pub(crate) fn new(text: &str, tape: Tape) -> Self {
        Self {
            text: text.into(),
            tape,
            members: OnceLock::new(),
        }
    }

    fn node(&self, index: u32) -> TapeNode {
        self.tape.node(index)
    }

    /// The text of node `index`.
    fn span(&self, index: u32) -> &str {
        let node = self.node(index);
        &self.text[node.start as usize..node.end() as usize]
    }

    /// The text of the string at node `index`, escapes resolved.
    fn string(&self, index: u32) -> Cow<'_, str> {
        let span = self.span(index);
        let inner = &span[1..span.len() - 1];
        match self.node(index).kind() {
            TapeKind::StrEscaped => Cow::Owned(gossamer_core::json::unescape_string(inner)),
            _ => Cow::Borrowed(inner),
        }
    }

    /// Indices of the elements of the array at `index`, or of the members'
    /// keys of the object at `index` (each value follows its key).
    fn children(&self, index: u32) -> impl Iterator<Item = u32> + '_ {
        let step = if self.node(index).kind() == TapeKind::Object {
            2
        } else {
            1
        };
        // The first child follows the container's word slot.
        let mut next = index + 2;
        (0..self.tape.count(index)).map(move |_| {
            let at = next;
            next = self.tape.extent(at);
            if step == 2 {
                let value = next;
                next = self.tape.extent(value);
            }
            at
        })
    }

    /// The key nodes of the object at `index`, in key order, the last of
    /// equal keys kept.
    fn members(&self, index: u32) -> &[u32] {
        let Ok(ordinal) = self.tape.objects.binary_search(&index) else {
            // Only an object has members.
            return &[];
        };
        let table = self
            .members
            .get_or_init(|| self.tape.objects.iter().map(|_| OnceLock::new()).collect());
        table[ordinal].get_or_init(|| {
            let mut keyed: Vec<(Cow<'_, str>, u32)> = self
                .children(index)
                .map(|key| (self.string(key), key))
                .collect();
            // A stable sort leaves equal keys in document order, so the last
            // of each run is the member a map keeps.
            keyed.sort_by(|a, b| a.0.cmp(&b.0));
            keyed
                .iter()
                .enumerate()
                .filter(|&(i, (key, _))| keyed.get(i + 1).is_none_or(|next| next.0 != *key))
                .map(|(_, &(_, node))| node)
                .collect()
        })
    }

    /// The value of the member `key` of the object at `index`.
    fn member(&self, index: u32, key: &str) -> Option<u32> {
        if self.tape.count(index) <= SCAN_MEMBERS {
            return self
                .children(index)
                .filter(|&k| self.string(k) == key)
                .last()
                .map(|k| k + 1);
        }
        let members = self.members(index);
        members
            .binary_search_by(|&k| self.string(k).as_ref().cmp(key))
            .ok()
            .map(|at| members[at] + 1)
    }
}

/// A JSON number as serde_json holds it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Num {
    Int(i64),
    Uint(u64),
    Float(f64),
}

impl Num {
    fn of(n: &serde_json::Number) -> Self {
        if let Some(u) = n.as_u64() {
            Self::Uint(u)
        } else if let Some(i) = n.as_i64() {
            Self::Int(i)
        } else {
            Self::Float(n.as_f64().unwrap_or(0.0))
        }
    }

    /// The number as an `i64`, when it is an integer that fits one.
    pub(crate) fn as_i64(self) -> Option<i64> {
        match self {
            Self::Int(i) => Some(i),
            Self::Uint(u) => i64::try_from(u).ok(),
            Self::Float(_) => None,
        }
    }

    /// The number as a `u64`, when it is a non-negative integer.
    pub(crate) fn as_u64(self) -> Option<u64> {
        match self {
            Self::Int(i) => u64::try_from(i).ok(),
            Self::Uint(u) => Some(u),
            Self::Float(_) => None,
        }
    }

    /// The number as the nearest `f64`.
    pub(crate) fn as_f64(self) -> f64 {
        match self {
            Self::Int(i) => i as f64,
            Self::Uint(u) => u as f64,
            Self::Float(f) => f,
        }
    }
}

/// One value of a `json::Value`, borrowed from the tree or document that
/// holds it.
#[derive(Clone, Copy)]
pub(crate) enum JsonRef<'a> {
    Value(&'a serde_json::Value),
    Tape(&'a ParsedTape, u32),
}

impl<'a> JsonRef<'a> {
    pub(crate) fn is_null(self) -> bool {
        match self {
            Self::Value(v) => v.is_null(),
            Self::Tape(t, i) => t.node(i).kind() == TapeKind::Null,
        }
    }

    /// The value of a boolean.
    pub(crate) fn as_bool(self) -> Option<bool> {
        match self {
            Self::Value(v) => v.as_bool(),
            Self::Tape(t, i) => match t.node(i).kind() {
                TapeKind::True => Some(true),
                TapeKind::False => Some(false),
                _ => None,
            },
        }
    }

    /// The value of a number.
    pub(crate) fn number(self) -> Option<Num> {
        match self {
            Self::Value(serde_json::Value::Number(n)) => Some(Num::of(n)),
            Self::Value(_) => None,
            Self::Tape(t, i) => {
                let node = t.node(i);
                match node.kind() {
                    TapeKind::Int => Some(Num::Int(t.tape.word(i) as i64)),
                    TapeKind::Uint => Some(Num::Uint(t.tape.word(i))),
                    TapeKind::Float => Some(Num::Float(f64::from_bits(t.tape.word(i)))),
                    _ => None,
                }
            }
        }
    }

    /// The text of a string.
    pub(crate) fn string(self) -> Option<Cow<'a, str>> {
        match self {
            Self::Value(serde_json::Value::String(s)) => Some(Cow::Borrowed(s)),
            Self::Value(_) => None,
            Self::Tape(t, i) => matches!(t.node(i).kind(), TapeKind::Str | TapeKind::StrEscaped)
                .then(|| t.string(i)),
        }
    }

    /// Whether a string's text is empty.
    pub(crate) fn string_is_empty(self) -> Option<bool> {
        match self {
            Self::Tape(t, i)
                if matches!(t.node(i).kind(), TapeKind::Str | TapeKind::StrEscaped) =>
            {
                // Every escape spells at least one character.
                let node = t.node(i);
                Some(node.end() - node.start == 2)
            }
            _ => self.string().map(|s| s.is_empty()),
        }
    }

    /// Elements of an array, members of an object, bytes of a string, and
    /// zero for anything else.
    pub(crate) fn len(self) -> usize {
        match self {
            Self::Value(serde_json::Value::Array(a)) => a.len(),
            Self::Value(serde_json::Value::Object(o)) => o.len(),
            Self::Value(serde_json::Value::String(s)) => s.len(),
            Self::Value(_) => 0,
            Self::Tape(t, i) => match t.node(i).kind() {
                TapeKind::Array => t.tape.count(i) as usize,
                TapeKind::Object if t.tape.count(i) <= 1 => t.tape.count(i) as usize,
                TapeKind::Object => t.members(i).len(),
                TapeKind::Str => t.span(i).len() - 2,
                TapeKind::StrEscaped => t.string(i).len(),
                _ => 0,
            },
        }
    }

    /// The member `key` of an object.
    pub(crate) fn get(self, key: &str) -> Option<Self> {
        match self {
            Self::Value(v) => v.get(key).map(Self::Value),
            Self::Tape(t, i) if t.node(i).kind() == TapeKind::Object => {
                t.member(i, key).map(|value| Self::Tape(t, value))
            }
            Self::Tape(..) => None,
        }
    }

    /// Element `index` of an array.
    pub(crate) fn at(self, index: usize) -> Option<Self> {
        match self {
            Self::Value(v) => v.get(index).map(Self::Value),
            Self::Tape(t, i) if t.node(i).kind() == TapeKind::Array => {
                let index = u32::try_from(index).ok()?;
                if index >= t.tape.count(i) {
                    return None;
                }
                t.children(i)
                    .nth(index as usize)
                    .map(|at| Self::Tape(t, at))
            }
            Self::Tape(..) => None,
        }
    }

    /// The elements of an array.
    pub(crate) fn items(self) -> Option<Vec<Self>> {
        match self {
            Self::Value(serde_json::Value::Array(items)) => {
                Some(items.iter().map(Self::Value).collect())
            }
            Self::Value(_) => None,
            Self::Tape(t, i) if t.node(i).kind() == TapeKind::Array => {
                Some(t.children(i).map(|at| Self::Tape(t, at)).collect())
            }
            Self::Tape(..) => None,
        }
    }

    /// The keys of an object, in order.
    pub(crate) fn keys(self) -> Option<Vec<Cow<'a, str>>> {
        match self {
            Self::Value(serde_json::Value::Object(map)) => {
                Some(map.keys().map(|k| Cow::Borrowed(k.as_str())).collect())
            }
            Self::Value(_) => None,
            Self::Tape(t, i) if t.node(i).kind() == TapeKind::Object => {
                Some(t.members(i).iter().map(|&k| t.string(k)).collect())
            }
            Self::Tape(..) => None,
        }
    }

    /// The value's document text, for a value read from one.
    pub(crate) fn source_text(self) -> Option<&'a str> {
        match self {
            Self::Value(_) => None,
            Self::Tape(t, i) => Some(t.span(i)),
        }
    }

    /// The value as a tree of its own.
    pub(crate) fn to_value(self) -> serde_json::Value {
        match self {
            Self::Value(v) => v.clone(),
            Self::Tape(t, i) => {
                let node = t.node(i);
                match node.kind() {
                    TapeKind::Null => serde_json::Value::Null,
                    TapeKind::True => serde_json::Value::Bool(true),
                    TapeKind::False => serde_json::Value::Bool(false),
                    TapeKind::Int => serde_json::Value::from(t.tape.word(i) as i64),
                    TapeKind::Uint => serde_json::Value::from(t.tape.word(i)),
                    TapeKind::Float => serde_json::Number::from_f64(f64::from_bits(t.tape.word(i)))
                        .map_or(serde_json::Value::Null, serde_json::Value::Number),
                    TapeKind::Str | TapeKind::StrEscaped => {
                        serde_json::Value::String(t.string(i).into_owned())
                    }
                    TapeKind::Array => serde_json::Value::Array(
                        t.children(i)
                            .map(|at| Self::Tape(t, at).to_value())
                            .collect(),
                    ),
                    TapeKind::Object => serde_json::Value::Object(
                        t.children(i)
                            .map(|k| (t.string(k).into_owned(), Self::Tape(t, k + 1).to_value()))
                            .collect(),
                    ),
                }
            }
        }
    }

    /// The value as a tree, borrowed when it is one.
    pub(crate) fn as_value(self) -> Cow<'a, serde_json::Value> {
        match self {
            Self::Value(v) => Cow::Borrowed(v),
            Self::Tape(..) => Cow::Owned(self.to_value()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> ParsedTape {
        let (source, tape) = gossamer_core::json::validate_to_tape(text.as_bytes()).unwrap();
        ParsedTape::new(source, tape.unwrap())
    }

    fn reads_alike(text: &str) {
        let doc = parsed(text);
        let tape_value = JsonRef::Tape(&doc, 0).to_value();
        let serde_value: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(tape_value, serde_value, "{text}");
    }

    #[test]
    fn a_tape_reads_as_the_serde_tree_of_the_same_text() {
        for text in [
            r#"{"b": 1, "a": [true, false, null], "c": {"x": "y\né"}}"#,
            "[1, -2, 18446744073709551615, 1.5, -0, 1e3, 123456789012345678901234567890]",
            r#"{"k": 1, "k": 2, "j": 3}"#,
            r#""😀 and \"quoted\"""#,
            "[]",
            "{}",
        ] {
            reads_alike(text);
        }
    }

    #[test]
    fn a_large_object_keeps_the_last_of_equal_keys_and_orders_its_keys() {
        let mut text = String::from("{");
        for i in (0..20).rev() {
            text += &format!("\"k{i:02}\": {i}, ");
        }
        text += "\"k05\": 99, \"z\\u0041\": 7}";
        let doc = parsed(&text);
        let root = JsonRef::Tape(&doc, 0);
        assert_eq!(root.len(), 21);
        assert_eq!(
            root.get("k05").and_then(JsonRef::number),
            Some(Num::Int(99))
        );
        assert_eq!(root.get("zA").and_then(JsonRef::number), Some(Num::Int(7)));
        assert!(root.get("k20").is_none());
        let keys = root.keys().unwrap();
        assert_eq!(keys.first().map(AsRef::as_ref), Some("k00"));
        assert_eq!(keys.last().map(AsRef::as_ref), Some("zA"));
        reads_alike(&text);
    }

    #[test]
    fn array_elements_step_over_nested_values() {
        let doc = parsed(r#"[[1, [2]], {"a": [3, 4]}, "s", 5]"#);
        let root = JsonRef::Tape(&doc, 0);
        assert_eq!(root.len(), 4);
        assert_eq!(root.at(2).and_then(JsonRef::string).as_deref(), Some("s"));
        assert_eq!(root.at(3).and_then(JsonRef::number), Some(Num::Int(5)));
        assert!(root.at(4).is_none());
        let a = root.at(1).and_then(|o| o.get("a")).unwrap();
        assert_eq!(a.at(1).and_then(JsonRef::number), Some(Num::Int(4)));
    }
}
