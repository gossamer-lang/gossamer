// Runtime support for `std::encoding::xml` - XML parsing and encoding.
//
// Exposes a simple tree-based XML value type that covers the common
// cases: elements with attributes and mixed text/element children.
// For streaming use, the underlying quick-xml reader is exposed via
// `events()`. All strings are UTF-8.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap};
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
    let mut tree = TreeBuilder::new(max_depth(), cap);
    tree.feed(src, 0).map_err(Error::new)?;
    tree.finish().map_err(Error::new)
}

/// How deeply entity references may nest inside the replacement text of
/// the entities that name them.
const MAX_ENTITY_DEPTH: usize = 16;

/// Builds a [`Node`] tree from a document's events, expanding the general
/// entities its internal DTD subset declares (XML 1.0 section 4.4).
struct TreeBuilder {
    stack: Vec<(String, BTreeMap<String, String>, Vec<Node>)>,
    root: Option<Node>,
    // `quick-xml` reports entity references (`&lt;` etc.) as their own
    // `GeneralRef` events and splits surrounding character data, so text
    // is reassembled here and entity-resolved as one piece at each
    // element boundary.
    text: String,
    /// Each declared entity's replacement text: its literal with character
    /// references expanded and entity references left to expand at use.
    entities: HashMap<String, String>,
    depth_cap: usize,
    /// Bytes of replacement text expansion may still produce, so nested
    /// references cannot multiply a small document past the size cap.
    expansion_budget: usize,
}

impl TreeBuilder {
    fn new(depth_cap: usize, size_cap: usize) -> Self {
        Self {
            stack: Vec::new(),
            root: None,
            text: String::new(),
            entities: HashMap::new(),
            depth_cap,
            expansion_budget: size_cap,
        }
    }

    fn finish(self) -> Result<Node, String> {
        self.root.ok_or_else(|| "xml: empty document".to_string())
    }

    /// Reads `src`, the document itself at `entity_depth` 0 and an entity's
    /// replacement text below it, into the tree.
    fn feed(&mut self, src: &str, entity_depth: usize) -> Result<(), String> {
        let mut reader = Reader::from_str(src);
        loop {
            let event = reader
                .read_event()
                .map_err(|e| format!("xml: parse: {e}"))?;
            // The end of a replacement text is not a boundary: its character
            // data runs on into the text after the reference.
            let continues_text = matches!(event, Event::Text(_) | Event::GeneralRef(_))
                || (entity_depth > 0 && matches!(event, Event::Eof));
            if !continues_text {
                self.flush_text()?;
            }
            match event {
                Event::DocType(e) if entity_depth == 0 => {
                    let decl = e.decode().map_err(|err| format!("xml: {err}"))?;
                    self.entities = declared_entities(&decl);
                }
                Event::Start(e) => {
                    if self.stack.len() >= self.depth_cap {
                        return Err(format!(
                            "xml nesting depth exceeds max_depth ({})",
                            self.depth_cap
                        ));
                    }
                    let (name, attrs) = self.element(&e)?;
                    self.stack.push((name, attrs, Vec::new()));
                }
                Event::Empty(e) => {
                    let (name, attrs) = self.element(&e)?;
                    self.attach(Node::Element {
                        name,
                        attrs,
                        children: vec![],
                    });
                }
                Event::End(_) => {
                    if let Some((name, attrs, children)) = self.stack.pop() {
                        self.attach(Node::Element {
                            name,
                            attrs,
                            children,
                        });
                    }
                }
                Event::Text(e) => {
                    let decoded = e.decode().map_err(|err| format!("xml: {err}"))?;
                    self.text.push_str(&decoded);
                }
                Event::GeneralRef(e) => {
                    let name = e.decode().map_err(|err| format!("xml: {err}"))?;
                    // The replacement text is content of its own: its
                    // character data joins the text around the reference and
                    // its markup becomes elements.
                    if let Some(text) = self.replacement(&name, entity_depth)? {
                        self.feed(&text, entity_depth + 1)?;
                    } else {
                        self.text.push('&');
                        self.text.push_str(&name);
                        self.text.push(';');
                    }
                }
                Event::CData(e) => {
                    let text = String::from_utf8_lossy(e.as_ref()).into_owned();
                    if let Some(parent) = self.stack.last_mut() {
                        parent.2.push(Node::Text(text));
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }
        Ok(())
    }

    /// The replacement text of the declared entity `name`, or `None` for a
    /// character reference, a predefined entity, or an undeclared name,
    /// which the text's own unescaping resolves or reports.
    fn replacement(&mut self, name: &str, entity_depth: usize) -> Result<Option<String>, String> {
        if name.starts_with('#') || quick_xml::escape::resolve_xml_entity(name).is_some() {
            return Ok(None);
        }
        let Some(text) = self.entities.get(name) else {
            return Ok(None);
        };
        if entity_depth >= MAX_ENTITY_DEPTH {
            return Err(format!(
                "xml: entity `{name}` nests deeper than {MAX_ENTITY_DEPTH} references"
            ));
        }
        if text.len() > self.expansion_budget {
            return Err("xml: entity expansion exceeds max_size".to_string());
        }
        self.expansion_budget -= text.len();
        Ok(Some(text.clone()))
    }

    /// An attribute's value with its references expanded; the replacement
    /// text of an entity an attribute names may not contain `<`.
    fn attribute_value(&mut self, raw: &str, entity_depth: usize) -> Result<String, String> {
        let mut out = String::with_capacity(raw.len());
        let mut rest = raw;
        while let Some(amp) = rest.find('&') {
            out.push_str(&rest[..amp]);
            let after = &rest[amp + 1..];
            let Some(semi) = after.find(';') else {
                return Err("xml: unterminated reference in an attribute value".to_string());
            };
            let name = &after[..semi];
            if let Some(text) = self.replacement(name, entity_depth)? {
                if text.contains('<') {
                    return Err(format!(
                        "xml: entity `{name}` puts `<` in an attribute value"
                    ));
                }
                let expanded = self.attribute_value(&text, entity_depth + 1)?;
                out.push_str(&expanded);
            } else {
                let reference = &rest[amp..amp + semi + 2];
                let resolved =
                    quick_xml::escape::unescape(reference).map_err(|err| escape_error(&err))?;
                out.push_str(&resolved);
            }
            rest = &after[semi + 1..];
        }
        out.push_str(rest);
        Ok(out)
    }

    fn element(
        &mut self,
        e: &quick_xml::events::BytesStart<'_>,
    ) -> Result<(String, BTreeMap<String, String>), String> {
        let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
        let mut attrs = BTreeMap::new();
        for attr in e.attributes().flatten() {
            let key = String::from_utf8_lossy(attr.key.local_name().as_ref()).into_owned();
            let raw = String::from_utf8_lossy(&attr.value).into_owned();
            let value = self.attribute_value(&raw, 0)?;
            attrs.insert(key, value);
        }
        Ok((name, attrs))
    }

    fn attach(&mut self, node: Node) {
        if let Some(parent) = self.stack.last_mut() {
            parent.2.push(node);
        } else {
            self.root = Some(node);
        }
    }

    /// Resolves the accumulated character data (entity references
    /// reconstructed as `&name;`), trims surrounding whitespace, and
    /// appends it as a text child of the current element. Whitespace-only
    /// runs collapse to nothing, matching the old `trim_text` behaviour.
    fn flush_text(&mut self) -> Result<(), String> {
        if self.text.is_empty() {
            return Ok(());
        }
        let trimmed = quick_xml::escape::unescape(&self.text)
            .map_err(|err| escape_error(&err))?
            .trim()
            .to_owned();
        self.text.clear();
        if !trimmed.is_empty()
            && let Some(parent) = self.stack.last_mut()
        {
            parent.2.push(Node::Text(trimmed));
        }
        Ok(())
    }
}

/// The general entities a DOCTYPE's internal subset declares, by name, each
/// mapped to its literal with character references expanded. The first
/// declaration of a name binds it; parameter and external entities are not
/// read.
fn declared_entities(doctype: &str) -> HashMap<String, String> {
    let mut entities = HashMap::new();
    let Some(open) = outside_quotes(doctype, b'[') else {
        return entities;
    };
    let mut rest = &doctype[open + 1..];
    while let Some(c) = rest.chars().next() {
        if let Some(after) = rest.strip_prefix("<!--") {
            rest = after.find("-->").map_or("", |end| &after[end + 3..]);
        } else if let Some(after) = rest.strip_prefix("<?") {
            rest = after.find("?>").map_or("", |end| &after[end + 2..]);
        } else if let Some(after) = rest.strip_prefix("<!ENTITY") {
            let end = outside_quotes(after, b'>').map_or(after.len(), |end| end + 1);
            if let Some((name, literal)) = entity_declaration(&after[..end]) {
                entities.entry(name).or_insert(literal);
            }
            rest = &after[end..];
        } else if c == '"' || c == '\'' {
            let after = &rest[1..];
            rest = after.find(c).map_or("", |end| &after[end + 1..]);
        } else if c == ']' {
            break;
        } else {
            rest = &rest[c.len_utf8()..];
        }
    }
    entities
}

/// The name and replacement text of one `<!ENTITY ...>` declaration's body,
/// when it declares an internal general entity.
fn entity_declaration(body: &str) -> Option<(String, String)> {
    let body = body.trim_start();
    if body.starts_with('%') {
        return None;
    }
    let name_end = body.find(char::is_whitespace)?;
    let name = &body[..name_end];
    let value = body[name_end..].trim_start();
    let quote = value.chars().next().filter(|q| *q == '"' || *q == '\'')?;
    let literal = &value[1..];
    let literal = &literal[..literal.find(quote)?];
    Some((name.to_string(), expand_char_refs(literal)))
}

/// `literal` with each character reference (`&#38;`, `&#x3C;`) replaced by
/// its character, as an entity's literal is at its declaration.
fn expand_char_refs(literal: &str) -> String {
    let mut out = String::with_capacity(literal.len());
    let mut rest = literal;
    while let Some(at) = rest.find("&#") {
        out.push_str(&rest[..at]);
        let after = &rest[at..];
        let Some(reference) = after.find(';').map(|semi| &after[..=semi]) else {
            out.push_str(after);
            rest = "";
            break;
        };
        match quick_xml::escape::unescape(reference) {
            Ok(c) => out.push_str(&c),
            Err(_) => out.push_str(reference),
        }
        rest = &after[reference.len()..];
    }
    out.push_str(rest);
    out
}

/// The message for a reference unescaping could not resolve. An undeclared
/// entity is named without the offset, which counts from the start of the
/// reassembled text rather than the document.
fn escape_error(err: &quick_xml::escape::EscapeError) -> String {
    match err {
        quick_xml::escape::EscapeError::UnrecognizedEntity(_, name) => {
            format!("xml: unrecognized entity `{name}`")
        }
        other => format!("xml: {other}"),
    }
}

/// The byte offset of the first `byte` in `text` outside a quoted string.
fn outside_quotes(text: &str, byte: u8) -> Option<usize> {
    let mut quote: Option<u8> = None;
    for (i, b) in text.bytes().enumerate() {
        match quote {
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None if b == b'"' || b == b'\'' => quote = Some(b),
            None if b == byte => return Some(i),
            None => {}
        }
    }
    None
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
    gossamer_runtime::codec::xml::escape(s)
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
