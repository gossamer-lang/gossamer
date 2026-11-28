//! A parsed document read where it lies: the walk that checks it records
//! each value's kind, span, and number in a [`Tape`], and every read answers
//! from that tape and the document text what the same read of the document's
//! [`Value`] tree answers: object members in key order with the last of
//! equal keys kept, numbers as the tree holds them.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::OnceLock;

use super::{Tape, TapeKind, TapeNode, Value};

/// An object with at most this many members is searched in place; a larger
/// one through its sorted member index.
const SCAN_MEMBERS: u32 = 8;

/// The key nodes of an object, in key order, the last of equal keys kept.
type MemberIndex = OnceLock<Box<[u32]>>;

/// An element below this index is reached by stepping from the first; one at
/// or past it through the array's element index.
const SCAN_ELEMENTS: u32 = 8;

/// The element nodes of an array, in document order.
type ElementIndex = OnceLock<Box<[u32]>>;

/// A parsed document: its text, its tape, and each object's member index and
/// each array's element index, built on first need.
#[derive(Debug)]
pub struct ParsedTape {
    text: Box<str>,
    tape: Tape,
    /// One slot per object, the table itself made by the first read that
    /// needs any index.
    members: OnceLock<Box<[MemberIndex]>>,
    /// One slot per array, made like `members`.
    elements: OnceLock<Box<[ElementIndex]>>,
}

impl ParsedTape {
    /// The document `text` read into `tape`.
    #[must_use]
    pub fn new(text: &str, tape: Tape) -> Self {
        Self {
            text: text.into(),
            tape,
            members: OnceLock::new(),
            elements: OnceLock::new(),
        }
    }

    /// The tape the document was read into.
    #[must_use]
    pub fn tape(&self) -> &Tape {
        &self.tape
    }

    /// Node `index` of the tape.
    #[must_use]
    pub fn node(&self, index: u32) -> TapeNode {
        self.tape.node(index)
    }

    /// The text of node `index`.
    #[must_use]
    pub fn span(&self, index: u32) -> &str {
        let node = self.node(index);
        &self.text[node.start as usize..node.end() as usize]
    }

    /// The text of the string at node `index`, escapes resolved.
    #[must_use]
    pub fn string(&self, index: u32) -> Cow<'_, str> {
        let span = self.span(index);
        let inner = &span[1..span.len() - 1];
        match self.node(index).kind() {
            TapeKind::StrEscaped => Cow::Owned(super::unescape_string(inner)),
            _ => Cow::Borrowed(inner),
        }
    }

    /// Indices of the elements of the array at `index`, or of the members'
    /// keys of the object at `index` (each value follows its key).
    #[must_use]
    pub fn children(&self, index: u32) -> Children<'_> {
        Children {
            tape: &self.tape,
            // The first child follows the container's word slot.
            next: index + 2,
            left: self.tape.count(index),
            object: self.node(index).kind() == TapeKind::Object,
        }
    }

    /// The key nodes of the object at `index`, in key order, the last of
    /// equal keys kept.
    pub fn members(&self, index: u32) -> &[u32] {
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

    /// The node of element `at` of the array at `index`, which has more than
    /// `at` elements.
    #[must_use]
    pub fn element(&self, index: u32, at: u32) -> Option<u32> {
        if at < SCAN_ELEMENTS {
            return self.children(index).nth(at as usize);
        }
        let ordinal = self.tape.arrays.binary_search(&index).ok()?;
        let table = self
            .elements
            .get_or_init(|| self.tape.arrays.iter().map(|_| OnceLock::new()).collect());
        table[ordinal]
            .get_or_init(|| self.children(index).collect())
            .get(at as usize)
            .copied()
    }

    /// The value of the member `key` of the object at `index`.
    #[must_use]
    pub fn member(&self, index: u32, key: &str) -> Option<u32> {
        if self.tape.count(index) <= SCAN_MEMBERS {
            // The last of equal keys is the member, so every key is read.
            let mut found = None;
            for k in self.children(index) {
                if self.string(k) == key {
                    found = Some(k + 1);
                }
            }
            return found;
        }
        let members = self.members(index);
        members
            .binary_search_by(|&k| self.string(k).as_ref().cmp(key))
            .ok()
            .map(|at| members[at] + 1)
    }
}

/// The children [`ParsedTape::children`] walks: each element of an array, or
/// each key of an object, skipping the value after it.
pub struct Children<'t> {
    tape: &'t Tape,
    next: u32,
    left: u32,
    object: bool,
}

impl Iterator for Children<'_> {
    type Item = u32;

    // A step is two tape reads; a call per member would cost more than the
    // step, and every object read walks its members.
    #[inline]
    fn next(&mut self) -> Option<u32> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        let at = self.next;
        self.next = self.tape.extent(at);
        if self.object {
            self.next = self.tape.extent(self.next);
        }
        Some(at)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left as usize, Some(self.left as usize))
    }
}

/// One value of a JSON document, borrowed from the tree or the parsed
/// document that holds it.
#[derive(Debug, Clone, Copy)]
pub enum ValueRef<'a> {
    /// A node of a [`Value`] tree.
    Value(&'a Value),
    /// Node `.1` of a parsed document.
    Tape(&'a ParsedTape, u32),
}

impl<'a> ValueRef<'a> {
    fn kind(self) -> Option<TapeKind> {
        match self {
            Self::Value(_) => None,
            Self::Tape(t, i) => Some(t.node(i).kind()),
        }
    }

    /// Whether this is `null`.
    #[must_use]
    pub fn is_null(self) -> bool {
        match self {
            Self::Value(v) => matches!(v, Value::Null),
            Self::Tape(..) => self.kind() == Some(TapeKind::Null),
        }
    }

    /// Whether this is an array.
    #[must_use]
    pub fn is_array(self) -> bool {
        match self {
            Self::Value(v) => matches!(v, Value::Array(_)),
            Self::Tape(..) => self.kind() == Some(TapeKind::Array),
        }
    }

    /// Whether this is an object.
    #[must_use]
    pub fn is_object(self) -> bool {
        match self {
            Self::Value(v) => matches!(v, Value::Object(_)),
            Self::Tape(..) => self.kind() == Some(TapeKind::Object),
        }
    }

    /// The value of a boolean.
    #[must_use]
    pub fn as_bool(self) -> Option<bool> {
        match self {
            Self::Value(Value::Bool(b)) => Some(*b),
            Self::Value(_) => None,
            Self::Tape(..) => match self.kind() {
                Some(TapeKind::True) => Some(true),
                Some(TapeKind::False) => Some(false),
                _ => None,
            },
        }
    }

    /// The value of a number, as the tree holds it: `Int`, `Uint`, or
    /// `Number`.
    #[must_use]
    pub fn number(self) -> Option<Value> {
        match self {
            Self::Value(v @ (Value::Int(_) | Value::Uint(_) | Value::Number(_))) => Some(v.clone()),
            Self::Value(_) => None,
            Self::Tape(t, i) => match t.node(i).kind() {
                TapeKind::Int => Some(Value::Int(t.tape().word(i).cast_signed())),
                TapeKind::Uint => Some(Value::Uint(t.tape().word(i))),
                TapeKind::Float => Some(Value::Number(f64::from_bits(t.tape().word(i)))),
                _ => None,
            },
        }
    }

    /// The text of a string.
    #[must_use]
    pub fn string(self) -> Option<Cow<'a, str>> {
        match self {
            Self::Value(Value::String(s)) => Some(Cow::Borrowed(s)),
            Self::Value(_) => None,
            Self::Tape(t, i) => matches!(t.node(i).kind(), TapeKind::Str | TapeKind::StrEscaped)
                .then(|| t.string(i)),
        }
    }

    /// The number of elements of an array or members of an object; zero for
    /// anything else.
    #[must_use]
    pub fn len(self) -> usize {
        match self {
            Self::Value(Value::Array(a)) => a.len(),
            Self::Value(Value::Object(o)) => o.len(),
            Self::Value(_) => 0,
            Self::Tape(t, i) => match t.node(i).kind() {
                TapeKind::Array => t.tape().count(i) as usize,
                TapeKind::Object if t.tape().count(i) <= 1 => t.tape().count(i) as usize,
                TapeKind::Object => t.members(i).len(),
                _ => 0,
            },
        }
    }

    /// Whether [`Self::len`] is zero.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// The member `key` of an object.
    #[must_use]
    pub fn get(self, key: &str) -> Option<Self> {
        match self {
            Self::Value(Value::Object(entries)) => entries.get(key).map(Self::Value),
            Self::Value(_) => None,
            Self::Tape(t, i) if t.node(i).kind() == TapeKind::Object => {
                t.member(i, key).map(|value| Self::Tape(t, value))
            }
            Self::Tape(..) => None,
        }
    }

    /// Element `index` of an array.
    #[must_use]
    pub fn at(self, index: usize) -> Option<Self> {
        match self {
            Self::Value(Value::Array(items)) => items.get(index).map(Self::Value),
            Self::Value(_) => None,
            Self::Tape(t, i) if t.node(i).kind() == TapeKind::Array => {
                let index = u32::try_from(index).ok()?;
                if index >= t.tape().count(i) {
                    return None;
                }
                t.element(i, index).map(|at| Self::Tape(t, at))
            }
            Self::Tape(..) => None,
        }
    }

    /// The value an RFC 6901 JSON Pointer names below this one.
    #[must_use]
    pub fn pointer(self, pointer: &str) -> Option<Self> {
        super::pointer_tokens(pointer)?
            .iter()
            .try_fold(self, |at, token| {
                if at.is_array() {
                    at.at(super::pointer_index(token)?)
                } else {
                    at.get(token)
                }
            })
    }

    /// The elements of an array.
    #[must_use]
    pub fn items(self) -> Option<Vec<Self>> {
        match self {
            Self::Value(Value::Array(items)) => Some(items.iter().map(Self::Value).collect()),
            Self::Value(_) => None,
            Self::Tape(t, i) if t.node(i).kind() == TapeKind::Array => {
                Some(t.children(i).map(|at| Self::Tape(t, at)).collect())
            }
            Self::Tape(..) => None,
        }
    }

    /// The members of an object in key order, the last of equal keys kept.
    #[must_use]
    pub fn entries(self) -> Option<Vec<(Cow<'a, str>, Self)>> {
        match self {
            Self::Value(Value::Object(entries)) => Some(
                entries
                    .iter()
                    .map(|(k, v)| (Cow::Borrowed(k.as_str()), Self::Value(v)))
                    .collect(),
            ),
            Self::Value(_) => None,
            Self::Tape(t, i) if t.node(i).kind() == TapeKind::Object => Some(
                t.members(i)
                    .iter()
                    .map(|&k| (t.string(k), Self::Tape(t, k + 1)))
                    .collect(),
            ),
            Self::Tape(..) => None,
        }
    }

    /// The value as a tree of its own.
    #[must_use]
    pub fn to_value(self) -> Value {
        match self {
            Self::Value(v) => v.clone(),
            Self::Tape(t, i) => match t.node(i).kind() {
                TapeKind::Null => Value::Null,
                TapeKind::True => Value::Bool(true),
                TapeKind::False => Value::Bool(false),
                TapeKind::Int | TapeKind::Uint | TapeKind::Float => {
                    self.number().unwrap_or(Value::Null)
                }
                TapeKind::Str | TapeKind::StrEscaped => Value::String(t.string(i).into_owned()),
                TapeKind::Array => Value::Array(
                    t.children(i)
                        .map(|at| Self::Tape(t, at).to_value())
                        .collect(),
                ),
                // A later key replaces an earlier equal one, as the tree keeps
                // the last.
                TapeKind::Object => Value::Object(
                    t.children(i)
                        .map(|k| (t.string(k).into_owned(), Self::Tape(t, k + 1).to_value()))
                        .collect::<BTreeMap<_, _>>(),
                ),
            },
        }
    }

    /// The value as a tree, borrowed when it is one.
    #[must_use]
    pub fn as_value(self) -> Cow<'a, Value> {
        match self {
            Self::Value(v) => Cow::Borrowed(v),
            Self::Tape(..) => Cow::Owned(self.to_value()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ParsedTape, ValueRef};

    fn reads_alike(text: &str) {
        let (source, tape) = super::super::validate_to_tape(text.as_bytes()).unwrap();
        let doc = ParsedTape::new(source, tape.unwrap());
        let tree = super::super::parse(text).unwrap();
        assert_eq!(ValueRef::Tape(&doc, 0).to_value(), tree, "{text}");
        let (a, b) = (ValueRef::Tape(&doc, 0), ValueRef::Value(&tree));
        assert_eq!(a.len(), b.len(), "{text}");
        assert_eq!(
            a.entries().map(|e| e
                .into_iter()
                .map(|(k, v)| (k.into_owned(), v.to_value()))
                .collect::<Vec<_>>()),
            b.entries().map(|e| e
                .into_iter()
                .map(|(k, v)| (k.into_owned(), v.to_value()))
                .collect::<Vec<_>>()),
            "{text}"
        );
    }

    #[test]
    fn a_tape_reads_as_the_tree_of_the_same_text() {
        for text in [
            r#"{"b": 1, "a": [true, false, null], "c": {"x": "y\né"}}"#,
            "[1, -2, 18446744073709551615, 1.5, -0, 1e3, 123456789012345678901234567890]",
            r#""😀 and \"quoted\"""#,
            "[]",
            "{}",
            r#"{"k1": 1, "k2": 2, "k3": 3, "k4": 4, "k5": 5, "k6": 6, "k7": 7, "k8": 8, "k9": 9, "k0": 0}"#,
        ] {
            reads_alike(text);
        }
    }

    #[test]
    fn reads_reach_members_elements_and_pointers() {
        let text = r#"{"a": [10, {"b": "x"}], "n": null}"#;
        let (source, tape) = super::super::validate_to_tape(text.as_bytes()).unwrap();
        let doc = ParsedTape::new(source, tape.unwrap());
        let root = ValueRef::Tape(&doc, 0);
        assert!(root.get("n").is_some_and(ValueRef::is_null));
        assert_eq!(
            root.pointer("/a/1/b").and_then(ValueRef::string).as_deref(),
            Some("x")
        );
        assert_eq!(
            root.get("a")
                .and_then(|a| a.at(0))
                .and_then(ValueRef::number),
            Some(super::Value::Int(10))
        );
        assert!(root.get("a").and_then(|a| a.at(2)).is_none());
        assert!(root.get("zz").is_none());
    }
}
