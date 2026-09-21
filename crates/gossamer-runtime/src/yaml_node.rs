//! One YAML document tree, decoded the same way for every tier.
//!
//! `serde_norway::Value` rejects an integer wider than 64 bits, which the
//! parser hands to `visit_i128` / `visit_u128`. This tree keeps every integer
//! that fits `i64` or `u64` exact and turns a wider one into the nearest
//! `f64`, as a JSON decoder does.

use std::fmt;

use serde::de::{self, Deserialize, Deserializer, EnumAccess, MapAccess, SeqAccess, Visitor};

/// A decoded YAML value with tags dropped and mapping order preserved.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    /// `null` / `~` / an empty scalar.
    Null,
    /// `true` / `false`.
    Bool(bool),
    /// An integer in the `i64` range.
    Int(i64),
    /// An integer above `i64::MAX` that fits `u64`.
    UInt(u64),
    /// A float, or an integer too wide for 64 bits.
    Float(f64),
    /// A string scalar.
    String(String),
    /// A sequence.
    Seq(Vec<Node>),
    /// A mapping, in document order.
    Map(Vec<(Node, Node)>),
}

impl Node {
    /// Nesting depth of the deepest sequence or mapping, counting this node.
    #[must_use]
    pub fn depth(&self) -> usize {
        match self {
            Node::Seq(items) => 1 + items.iter().map(Node::depth).max().unwrap_or(0),
            Node::Map(entries) => {
                1 + entries
                    .iter()
                    .map(|(k, v)| k.depth().max(v.depth()))
                    .max()
                    .unwrap_or(0)
            }
            _ => 0,
        }
    }

    /// The same document as a JSON value; a non-string mapping key is
    /// rendered as its scalar text.
    #[must_use]
    pub fn into_json(self) -> serde_json::Value {
        match self {
            Node::Null => serde_json::Value::Null,
            Node::Bool(b) => serde_json::Value::Bool(b),
            Node::Int(n) => serde_json::Value::Number(n.into()),
            Node::UInt(n) => serde_json::Value::Number(n.into()),
            Node::Float(f) => serde_json::Number::from_f64(f)
                .map_or(serde_json::Value::Null, serde_json::Value::Number),
            Node::String(s) => serde_json::Value::String(s),
            Node::Seq(items) => {
                serde_json::Value::Array(items.into_iter().map(Node::into_json).collect())
            }
            Node::Map(entries) => serde_json::Value::Object(
                entries
                    .into_iter()
                    .map(|(k, v)| (k.key_text(), v.into_json()))
                    .collect(),
            ),
        }
    }

    fn key_text(self) -> String {
        match self {
            Node::String(s) => s,
            Node::Null => "null".to_string(),
            Node::Bool(b) => b.to_string(),
            Node::Int(n) => n.to_string(),
            Node::UInt(n) => n.to_string(),
            Node::Float(f) => f.to_string(),
            other => other.into_json().to_string(),
        }
    }
}

/// Decodes one YAML document.
///
/// # Errors
/// Returns the parser's message when `text` is not a single YAML document.
pub fn parse(text: &str) -> Result<Node, serde_norway::Error> {
    serde_norway::from_str(text)
}

/// Decodes every document of a multi-document YAML stream.
///
/// # Errors
/// Returns the parser's message for the first document that does not decode.
pub fn parse_all(text: &str) -> Result<Vec<Node>, serde_norway::Error> {
    serde_norway::Deserializer::from_str(text)
        .map(Node::deserialize)
        .collect()
}

impl<'de> Deserialize<'de> for Node {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(NodeVisitor)
    }
}

struct NodeVisitor;

impl<'de> Visitor<'de> for NodeVisitor {
    type Value = Node;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any YAML value")
    }

    fn visit_unit<E>(self) -> Result<Node, E> {
        Ok(Node::Null)
    }

    fn visit_none<E>(self) -> Result<Node, E> {
        Ok(Node::Null)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Node, D::Error> {
        Node::deserialize(deserializer)
    }

    fn visit_bool<E>(self, b: bool) -> Result<Node, E> {
        Ok(Node::Bool(b))
    }

    fn visit_i64<E>(self, n: i64) -> Result<Node, E> {
        Ok(Node::Int(n))
    }

    fn visit_u64<E>(self, n: u64) -> Result<Node, E> {
        Ok(i64::try_from(n).map_or(Node::UInt(n), Node::Int))
    }

    #[expect(
        clippy::cast_precision_loss,
        reason = "an integer wider than 64 bits is kept as its nearest f64"
    )]
    fn visit_i128<E>(self, n: i128) -> Result<Node, E> {
        if let Ok(small) = i64::try_from(n) {
            return Ok(Node::Int(small));
        }
        if let Ok(unsigned) = u64::try_from(n) {
            return Ok(Node::UInt(unsigned));
        }
        Ok(Node::Float(n as f64))
    }

    #[expect(
        clippy::cast_precision_loss,
        reason = "an integer wider than 64 bits is kept as its nearest f64"
    )]
    fn visit_u128<E>(self, n: u128) -> Result<Node, E> {
        Ok(match u64::try_from(n) {
            Ok(fits) => i64::try_from(fits).map_or(Node::UInt(fits), Node::Int),
            Err(_) => Node::Float(n as f64),
        })
    }

    fn visit_f64<E>(self, f: f64) -> Result<Node, E> {
        Ok(Node::Float(f))
    }

    fn visit_str<E>(self, s: &str) -> Result<Node, E> {
        Ok(Node::String(s.to_string()))
    }

    fn visit_string<E>(self, s: String) -> Result<Node, E> {
        Ok(Node::String(s))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Node, A::Error> {
        let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0));
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(Node::Seq(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Node, A::Error> {
        let mut entries = Vec::with_capacity(map.size_hint().unwrap_or(0));
        while let Some(entry) = map.next_entry()? {
            entries.push(entry);
        }
        Ok(Node::Map(entries))
    }

    // A `!tag value` arrives as an enum whose variant is the tag; the tag
    // carries no meaning here, so the value stands for itself.
    fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<Node, A::Error> {
        let (_tag, variant) = data.variant::<String>()?;
        de::VariantAccess::newtype_variant::<Node>(variant)
    }
}

#[cfg(test)]
mod tests {
    use super::{Node, parse, parse_all};

    #[test]
    fn integers_that_fit_64_bits_stay_exact() {
        assert_eq!(parse("-9223372036854775808").unwrap(), Node::Int(i64::MIN));
        assert_eq!(parse("18446744073709551615").unwrap(), Node::UInt(u64::MAX));
    }

    #[test]
    fn wider_integers_become_the_nearest_float() {
        assert_eq!(
            parse("-9223372036854776000").unwrap(),
            Node::Float(-9_223_372_036_854_776_000.0)
        );
        assert_eq!(
            parse("x: 99999999999999999999999").unwrap(),
            Node::Map(vec![(
                Node::String("x".to_string()),
                Node::Float(99_999_999_999_999_999_999_999.0)
            )])
        );
    }

    #[test]
    fn tags_are_dropped_and_mapping_order_kept() {
        let doc = parse("b: !custom 1\na: [x, ~]\n").unwrap();
        assert_eq!(
            doc,
            Node::Map(vec![
                (Node::String("b".to_string()), Node::Int(1)),
                (
                    Node::String("a".to_string()),
                    Node::Seq(vec![Node::String("x".to_string()), Node::Null])
                ),
            ])
        );
    }

    #[test]
    fn every_document_of_a_stream_decodes() {
        let docs = parse_all("a: 1\n---\n-9223372036854776000\n").unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[1], Node::Float(-9_223_372_036_854_776_000.0));
    }
}
