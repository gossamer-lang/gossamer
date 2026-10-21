#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::doc_markdown)]

//! C-ABI shims for `std::encoding::xml::{parse, encode}` so the
//! compiled tier lowers them to direct runtime calls instead of
//! emitting undefined `@encoding::xml::parse` / `@encoding::xml::encode`
//! references. `gossamer-runtime` cannot depend on `gossamer-std`
//! (that would be a dependency cycle), so the parse/encode logic is
//! reimplemented here against the same `quick-xml 0.37` the VM tier
//! uses - the bytes mirror `gossamer_std::encoding::xml` exactly, so
//! a parse->encode round-trip is bit-identical across tiers.
//!
//! The parsed tree is a `json::Value` on every tier, in the shape the VM
//! builds: an element is `{"__xml_type": "element", "attrs": {..},
//! "children": [..], "name": ..}` and a text node is
//! `{"__xml_type": "text", "value": ..}`. `parse` answers a `GosJson`
//! handle, so it renders, navigates, and is released as any other
//! `json::Value`; `encode` reads one back.

use std::collections::{BTreeMap, HashMap};
use std::os::raw::c_char;

use quick_xml::Reader;
use quick_xml::events::Event;
use quick_xml::writer::Writer;

use super::string::alloc_cstring;

/// Mirrors `gossamer_std::encoding::xml`'s default parser caps so the
/// error path is byte-identical to the VM on oversize / over-deep
/// input. The VM's `set_max_*` setters are not part of the compiled
/// surface, so these are fixed at the same defaults.
const DEFAULT_MAX_DEPTH: usize = 128;
const DEFAULT_MAX_SIZE: usize = 16 * 1024 * 1024;

/// A node in the parsed XML tree. Mirrors
/// `gossamer_std::encoding::xml::Node`; attributes are ordered
/// (`BTreeMap`) so attribute emission is deterministic and matches
/// the VM tier.
enum Node {
    Element {
        name: String,
        attrs: BTreeMap<String, String>,
        children: Vec<Node>,
    },
    Text(String),
}

/// # Safety
///
/// `s` is null or a live string body that outlives the returned borrow.
unsafe fn cstr_to_str<'a>(s: *const c_char) -> &'a str {
    // SAFETY: callers pass a Gossamer `String`, read through its length
    // header so interior NUL bytes survive; non-UTF-8 falls back to empty.
    unsafe { crate::c_abi::gos_str_arg_text(s) }
}

fn err_result(msg: &str) -> i128 {
    let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
    super::result::gos_rt_result_new(1, err as i64)
}

/// Parses an XML document into a tree, returning the root element.
/// Byte-for-byte mirror of `gossamer_std::encoding::xml::parse`.
fn parse(src: &str) -> Result<Node, String> {
    if src.len() > DEFAULT_MAX_SIZE {
        return Err(format!(
            "xml input exceeds max_size ({} > {DEFAULT_MAX_SIZE})",
            src.len()
        ));
    }
    let mut tree = TreeBuilder::new(DEFAULT_MAX_DEPTH, DEFAULT_MAX_SIZE);
    tree.feed(src, 0)?;
    tree.finish()
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

/// Serialises a node tree to XML. Byte-for-byte mirror of
/// `gossamer_std::encoding::xml::encode`; emits no XML declaration.
fn encode(node: &Node) -> String {
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

/// The `json::Value` shape of `node`, keyed as the VM keys it.
fn node_to_json(node: &Node) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    match node {
        Node::Text(text) => {
            map.insert("__xml_type".into(), "text".into());
            map.insert("value".into(), text.as_str().into());
        }
        Node::Element {
            name,
            attrs,
            children,
        } => {
            map.insert("__xml_type".into(), "element".into());
            let attrs = attrs
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::from(v.as_str())))
                .collect();
            map.insert("attrs".into(), serde_json::Value::Object(attrs));
            map.insert(
                "children".into(),
                serde_json::Value::Array(children.iter().map(node_to_json).collect()),
            );
            map.insert("name".into(), name.as_str().into());
        }
    }
    serde_json::Value::Object(map)
}

/// The node a `json::Value` of the [`node_to_json`] shape describes;
/// `None` for any other value, as the VM answers.
fn json_to_node(value: &serde_json::Value) -> Option<Node> {
    let map = value.as_object()?;
    match map.get("__xml_type")?.as_str()? {
        "text" => Some(Node::Text(
            map.get("value")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string(),
        )),
        "element" => {
            let name = map.get("name")?.as_str()?.to_string();
            let attrs = map
                .get("attrs")
                .and_then(serde_json::Value::as_object)
                .map(|attrs| {
                    attrs
                        .iter()
                        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            let children = map
                .get("children")
                .and_then(serde_json::Value::as_array)
                .map(|children| children.iter().filter_map(json_to_node).collect())
                .unwrap_or_default();
            Some(Node::Element {
                name,
                attrs,
                children,
            })
        }
        _ => None,
    }
}

/// `encoding::xml::parse(s) -> Result<json::Value, errors::Error>`,
/// packed as a `GosResult` i128 (disc 0 = Ok with a `GosJson` handle,
/// disc 1 = Err with an error handle).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_xml_parse(s: *const c_char) -> i128 {
    ffi_entry!({
        // SAFETY: `s` is this shim's argument, as `cstr_to_str` requires (C-ABI contract).
        match parse(unsafe { cstr_to_str(s) }) {
            Ok(node) => {
                let handle = super::json::GosJson::into_raw(node_to_json(&node));
                super::result::gos_rt_result_new(0, handle as i64)
            }
            Err(e) => err_result(&e),
        }
    })
}

/// `encoding::xml::encode(value) -> String`. Borrows the `json::Value`
/// handle; a value that is not an XML node encodes as the empty string,
/// as on the VM.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_xml_encode(node: i64) -> *mut c_char {
    ffi_entry!({
        // SAFETY: `node` is a `json::Value` handle (or null), which is what
        // the checker admits for `encode`'s parameter.
        let value = unsafe { super::json::json_borrow(node as *const super::json::GosJson) };
        match value.and_then(|value| json_to_node(&value.as_value())) {
            Some(node) => alloc_cstring(encode(&node).as_bytes()),
            None => alloc_cstring(b""),
        }
    })
}

#[cfg(test)]
mod xml_codec_tests {
    use super::*;

    #[test]
    fn roundtrip_matches_quick_xml_bytes() {
        let src = "<note id=\"7\"><to>Tove</to><from>Jani</from>\
                   <body>Don't &lt;forget&gt; me</body><empty/></note>";
        let node = parse(src).expect("parse");
        let out = encode(&node);
        assert_eq!(
            out,
            "<note id=\"7\"><to>Tove</to><from>Jani</from>\
             <body>Don&apos;t &lt;forget&gt; me</body><empty/></note>"
        );
    }

    #[test]
    fn empty_document_is_an_error() {
        assert!(parse("   ").is_err());
    }
}
