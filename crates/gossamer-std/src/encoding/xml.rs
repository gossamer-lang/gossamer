// Runtime support for `std::encoding::xml` - XML parsing and encoding.
//
// Exposes a simple tree-based XML value type that covers the common
// cases: elements with attributes and mixed text/element children.
// For streaming use, the underlying quick-xml reader is exposed via
// `events()`. All strings are UTF-8.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use quick_xml::Reader;
use quick_xml::events::Event;
use quick_xml::writer::Writer;

use crate::errors::Error;

const DEFAULT_MAX_DEPTH: usize = 128;
const DEFAULT_MAX_SIZE: usize = 16 * 1024 * 1024;

static MAX_DEPTH: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_DEPTH);
static MAX_SIZE: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_SIZE);

/// Overrides the process-wide cap on parser nesting depth.
pub fn set_max_depth(n: usize) {
    MAX_DEPTH.store(n, Ordering::Relaxed);
}

/// Overrides the process-wide cap on parser input bytes.
pub fn set_max_size(n: usize) {
    MAX_SIZE.store(n, Ordering::Relaxed);
}

/// Current cap on parser nesting depth.
#[must_use]
pub fn max_depth() -> usize {
    MAX_DEPTH.load(Ordering::Relaxed)
}

/// Current cap on parser input bytes.
#[must_use]
pub fn max_size() -> usize {
    MAX_SIZE.load(Ordering::Relaxed)
}

/// A single attribute name → value pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attr {
    /// Attribute name.
    pub name: String,
    /// Attribute value.
    pub value: String,
}

/// A node in an XML tree.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    /// An element with a tag name, attributes, and child nodes.
    Element {
        /// Tag name.
        name: String,
        /// Ordered attribute map.
        attrs: BTreeMap<String, String>,
        /// Child nodes.
        children: Vec<Node>,
    },
    /// A text node.
    Text(String),
}

impl Node {
    /// Returns the tag name if this is an element.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Element { name, .. } => Some(name),
            Self::Text(_) => None,
        }
    }

    /// Returns the text content if this is a `Text` node.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Text(s) => Some(s),
            Self::Element { .. } => None,
        }
    }

    /// Collects the text of all direct `Text` children.
    #[must_use]
    pub fn inner_text(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Element { children, .. } => children
                .iter()
                .filter_map(|c| {
                    if let Self::Text(s) = c {
                        Some(s.as_str())
                    } else {
                        None
                    }
                })
                .collect(),
        }
    }

    /// Returns child elements with the given tag name.
    #[must_use]
    pub fn children_named<'a>(&'a self, tag: &str) -> Vec<&'a Node> {
        match self {
            Self::Element { children, .. } => {
                children.iter().filter(|c| c.name() == Some(tag)).collect()
            }
            Self::Text(_) => vec![],
        }
    }

    /// Returns the value of an attribute.
    #[must_use]
    pub fn attr(&self, name: &str) -> Option<&str> {
        match self {
            Self::Element { attrs, .. } => attrs.get(name).map(String::as_str),
            Self::Text(_) => None,
        }
    }
}

/// Parses an XML document into a tree of [`Node`]s. Returns the root element.
pub fn parse(src: &str) -> Result<Node, Error> {
    let cap = max_size();
    if src.len() > cap {
        return Err(Error::new(format!(
            "xml input exceeds max_size ({} > {cap})",
            src.len()
        )));
    }
    let depth_cap = max_depth();
    let mut reader = Reader::from_str(src);
    let mut stack: Vec<(String, BTreeMap<String, String>, Vec<Node>)> = Vec::new();
    let mut root: Option<Node> = None;
    // `quick-xml` reports entity references (`&lt;` etc.) as their own
    // `GeneralRef` events and splits surrounding character data, so text
    // is reassembled here and entity-resolved as one piece at each
    // element boundary.
    let mut text_buf = String::new();
    loop {
        let event = reader
            .read_event()
            .map_err(|e| Error::new(format!("xml: parse: {e}")))?;
        if !matches!(event, Event::Text(_) | Event::GeneralRef(_)) {
            flush_text(&mut text_buf, &mut stack)?;
        }
        match event {
            Event::Start(e) => {
                if stack.len() >= depth_cap {
                    return Err(Error::new(format!(
                        "xml nesting depth exceeds max_depth ({depth_cap})"
                    )));
                }
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                let mut attrs = BTreeMap::new();
                for attr in e.attributes().flatten() {
                    let key = String::from_utf8_lossy(attr.key.local_name().as_ref()).into_owned();
                    let val = String::from_utf8_lossy(&attr.value).into_owned();
                    attrs.insert(key, val);
                }
                stack.push((name, attrs, Vec::new()));
            }
            Event::Empty(e) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                let mut attrs = BTreeMap::new();
                for attr in e.attributes().flatten() {
                    let key = String::from_utf8_lossy(attr.key.local_name().as_ref()).into_owned();
                    let val = String::from_utf8_lossy(&attr.value).into_owned();
                    attrs.insert(key, val);
                }
                let node = Node::Element {
                    name,
                    attrs,
                    children: vec![],
                };
                if let Some(parent) = stack.last_mut() {
                    parent.2.push(node);
                } else {
                    root = Some(node);
                }
            }
            Event::End(_) => {
                if let Some((name, attrs, children)) = stack.pop() {
                    let node = Node::Element {
                        name,
                        attrs,
                        children,
                    };
                    if let Some(parent) = stack.last_mut() {
                        parent.2.push(node);
                    } else {
                        root = Some(node);
                    }
                }
            }
            Event::Text(e) => {
                let decoded = e
                    .decode()
                    .map_err(|err| Error::new(format!("xml: {err}")))?;
                text_buf.push_str(&decoded);
            }
            Event::GeneralRef(e) => {
                let name = e
                    .decode()
                    .map_err(|err| Error::new(format!("xml: {err}")))?;
                text_buf.push('&');
                text_buf.push_str(&name);
                text_buf.push(';');
            }
            Event::CData(e) => {
                let text = String::from_utf8_lossy(e.as_ref()).into_owned();
                if let Some(parent) = stack.last_mut() {
                    parent.2.push(Node::Text(text));
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    root.ok_or_else(|| Error::new("xml: empty document"))
}

/// Resolves the accumulated character data (entity references
/// reconstructed as `&name;`), trims surrounding whitespace, and
/// appends it as a text child of the current element. Whitespace-only
/// runs collapse to nothing, matching the old `trim_text` behaviour.
fn flush_text(
    buf: &mut String,
    stack: &mut [(String, BTreeMap<String, String>, Vec<Node>)],
) -> Result<(), Error> {
    if buf.is_empty() {
        return Ok(());
    }
    let resolved =
        quick_xml::escape::unescape(buf).map_err(|err| Error::new(format!("xml: {err}")))?;
    let trimmed = resolved.trim().to_owned();
    buf.clear();
    if !trimmed.is_empty() {
        if let Some(parent) = stack.last_mut() {
            parent.2.push(Node::Text(trimmed));
        }
    }
    Ok(())
}

/// Serialises a [`Node`] tree to an XML string. Does not emit an XML declaration.
#[must_use]
pub fn encode(node: &Node) -> String {
    let mut buf = Vec::new();
    let mut writer = Writer::new(&mut buf);
    write_node(&mut writer, node);
    String::from_utf8(buf).unwrap_or_default()
}

fn write_node(w: &mut Writer<impl std::io::Write>, node: &Node) {
    match node {
        Node::Text(s) => {
            let _ = w.write_event(Event::Text(quick_xml::events::BytesText::new(s)));
        }
        Node::Element {
            name,
            attrs,
            children,
        } => {
            let mut elem = quick_xml::events::BytesStart::new(name.as_str());
            for (k, v) in attrs {
                elem.push_attribute((k.as_str(), v.as_str()));
            }
            if children.is_empty() {
                let _ = w.write_event(Event::Empty(elem));
            } else {
                let _ = w.write_event(Event::Start(elem));
                for child in children {
                    write_node(w, child);
                }
                let _ = w.write_event(Event::End(quick_xml::events::BytesEnd::new(name.as_str())));
            }
        }
    }
}

/// The `json::Value` a Gossamer program sees for `node`: an element is
/// `{"__xml_type": "element", "attrs": {..}, "children": [..], "name": ..}`
/// and a text node is `{"__xml_type": "text", "value": ..}`.
#[must_use]
pub fn to_json(node: &Node) -> crate::json::Value {
    use crate::json::Value;
    let mut map = BTreeMap::new();
    match node {
        Node::Text(text) => {
            map.insert("__xml_type".to_string(), Value::String("text".into()));
            map.insert("value".to_string(), Value::String(text.clone()));
        }
        Node::Element {
            name,
            attrs,
            children,
        } => {
            map.insert("__xml_type".to_string(), Value::String("element".into()));
            let attrs = attrs
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect();
            map.insert("attrs".to_string(), Value::Object(attrs));
            map.insert(
                "children".to_string(),
                Value::Array(children.iter().map(to_json).collect()),
            );
            map.insert("name".to_string(), Value::String(name.clone()));
        }
    }
    Value::Object(map)
}

/// The node a value of the [`to_json`] shape describes; `None` for any
/// other value.
#[must_use]
pub fn from_json(value: &crate::json::Value) -> Option<Node> {
    use crate::json::Value;
    let Value::Object(map) = value else {
        return None;
    };
    let text_of = |key: &str| match map.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    };
    match text_of("__xml_type")?.as_str() {
        "text" => Some(Node::Text(text_of("value").unwrap_or_default())),
        "element" => {
            let attrs = match map.get("attrs") {
                Some(Value::Object(attrs)) => attrs
                    .iter()
                    .filter_map(|(k, v)| match v {
                        Value::String(v) => Some((k.clone(), v.clone())),
                        _ => None,
                    })
                    .collect(),
                _ => BTreeMap::new(),
            };
            let children = match map.get("children") {
                Some(Value::Array(children)) => children.iter().filter_map(from_json).collect(),
                _ => Vec::new(),
            };
            Some(Node::Element {
                name: text_of("name")?,
                attrs,
                children,
            })
        }
        _ => None,
    }
}

/// Escapes XML special characters in `s` (`&`, `<`, `>`, `"`, `'`).
#[must_use]
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_element() {
        let doc = r#"<root><child name="hello">world</child></root>"#;
        let root = parse(doc).unwrap();
        assert_eq!(root.name(), Some("root"));
        let children = root.children_named("child");
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].attr("name"), Some("hello"));
        assert_eq!(children[0].inner_text(), "world");
    }

    #[test]
    fn parse_self_closing() {
        let doc = "<root><empty/></root>";
        let root = parse(doc).unwrap();
        let children = root.children_named("empty");
        assert_eq!(children.len(), 1);
    }

    #[test]
    fn encode_round_trips() {
        use std::collections::BTreeMap;
        let mut attrs = BTreeMap::new();
        attrs.insert("lang".to_string(), "gos".to_string());
        let node = Node::Element {
            name: "root".to_string(),
            attrs,
            children: vec![Node::Text("hello".to_string())],
        };
        let encoded = encode(&node);
        let decoded = parse(&encoded).unwrap();
        assert_eq!(decoded.name(), Some("root"));
        assert_eq!(decoded.inner_text(), "hello");
    }

    #[test]
    fn escape_special_chars() {
        assert_eq!(
            escape("<b>Hello & 'World'</b>"),
            "&lt;b&gt;Hello &amp; &apos;World&apos;&lt;/b&gt;"
        );
    }
}
