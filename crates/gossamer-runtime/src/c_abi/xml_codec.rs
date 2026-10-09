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

use std::collections::BTreeMap;
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
            .map_err(|e| format!("xml: parse: {e}"))?;
        if !matches!(event, Event::Text(_) | Event::GeneralRef(_)) {
            flush_text(&mut text_buf, &mut stack)?;
        }
        match event {
            Event::Start(e) => {
                if stack.len() >= DEFAULT_MAX_DEPTH {
                    return Err(format!(
                        "xml nesting depth exceeds max_depth ({DEFAULT_MAX_DEPTH})"
                    ));
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
                let decoded = e.decode().map_err(|err| format!("xml: {err}"))?;
                text_buf.push_str(&decoded);
            }
            Event::GeneralRef(e) => {
                let name = e.decode().map_err(|err| format!("xml: {err}"))?;
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
    root.ok_or_else(|| "xml: empty document".to_string())
}

/// Resolves the accumulated character data (entity references
/// reconstructed as `&name;`), trims surrounding whitespace, and
/// appends it as a text child of the current element. Whitespace-only
/// runs collapse to nothing, matching the old `trim_text` behaviour.
fn flush_text(
    buf: &mut String,
    stack: &mut [(String, BTreeMap<String, String>, Vec<Node>)],
) -> Result<(), String> {
    if buf.is_empty() {
        return Ok(());
    }
    let resolved = quick_xml::escape::unescape(buf).map_err(|err| format!("xml: {err}"))?;
    let trimmed = resolved.trim().to_owned();
    buf.clear();
    if !trimmed.is_empty() {
        if let Some(parent) = stack.last_mut() {
            parent.2.push(Node::Text(trimmed));
        }
    }
    Ok(())
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
        match value.and_then(json_to_node) {
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
