//! Minimal LSP wire-format helpers backed by `gossamer_std::json`.
//! The full LSP spec is large; this module implements only the
//! subset Gossamer's first-slice server uses. It hand-writes the
//! `Content-Length: N\r\n\r\n` framing and goes straight to / from
//! [`gossamer_std::json::Value`] without a separate typed DTO layer.

#![forbid(unsafe_code)]

use std::io::{BufRead, Write};

use gossamer_std::json::{self, Value};

/// Wraps stdin + stdout into a framed JSON-RPC transport.
pub(crate) struct Transport<R: BufRead, W: Write> {
    reader: R,
    writer: W,
    buffer: Vec<u8>,
}

impl<R: std::io::Read, W: Write> Transport<std::io::BufReader<R>, W> {
    /// Whether more input has already arrived, so the next read would not
    /// wait for the client.
    pub(crate) fn has_buffered_input(&self) -> bool {
        !self.reader.buffer().is_empty()
    }
}

impl<R: BufRead, W: Write> Transport<R, W> {
    /// Constructs a transport bound to the supplied streams.
    pub(crate) fn new(reader: R, writer: W) -> Self {
        Self {
            reader,
            writer,
            buffer: Vec::new(),
        }
    }

    /// Reads one framed message, returning `None` on clean EOF.
    pub(crate) fn read_message(&mut self) -> std::io::Result<Option<Value>> {
        let mut content_length: Option<usize> = None;
        let mut line = String::new();
        loop {
            line.clear();
            let n = self.reader.read_line(&mut line)?;
            if n == 0 {
                return Ok(None);
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            if let Some((name, value)) = trimmed.split_once(':') {
                if name.eq_ignore_ascii_case("Content-Length") {
                    content_length = value.trim().parse::<usize>().ok();
                }
            }
        }
        let Some(len) = content_length else {
            return Err(std::io::Error::other(
                "LSP frame missing Content-Length header",
            ));
        };
        self.buffer.resize(len, 0);
        self.reader.read_exact(&mut self.buffer)?;
        let text = std::str::from_utf8(&self.buffer).map_err(std::io::Error::other)?;
        let value = json::parse(text).map_err(|e| std::io::Error::other(format!("{e}")))?;
        Ok(Some(value))
    }

    /// Writes one framed message.
    pub(crate) fn write_message(&mut self, value: &Value) -> std::io::Result<()> {
        let payload = json::encode(&lsp_integers(value));
        write!(self.writer, "Content-Length: {}\r\n\r\n", payload.len())?;
        self.writer.write_all(payload.as_bytes())?;
        self.writer.flush()
    }
}

/// Rewrites every integral `Value::Number` in `value` to `Value::Int`.
///
/// The LSP protocol's numeric fields (line, character, kind, severity,
/// semantic-token deltas, ...) are all integers. `gossamer_std::json`
/// stores them as `Value::Number(f64)`, whose encoder renders an integral
/// float as `12.0` to preserve float shape on round-trip. The LSP wire
/// format requires `12`: a conformant client deserializes these fields as
/// integers and rejects (or, in `serde_json`'s case, fails `as_i64`) a
/// float. This adapts gossamer's float-preferring model to the protocol's
/// integer wire format at the single serialization boundary, matching the
/// reference implementation's `JSON.stringify` semantics. Non-integral
/// numbers (none exist in the protocol today) keep their float shape.
fn lsp_integers(value: &Value) -> Value {
    match value {
        Value::Number(n)
            if n.is_finite()
                && n.fract() == 0.0
                && *n >= i64::MIN as f64
                && *n <= i64::MAX as f64 =>
        {
            Value::Int(*n as i64)
        }
        Value::Array(items) => Value::Array(items.iter().map(lsp_integers).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), lsp_integers(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Builds a successful JSON-RPC response.
pub(crate) fn response_ok(id: Value, result: Value) -> Value {
    let mut map = std::collections::BTreeMap::new();
    map.insert("jsonrpc".to_string(), Value::String("2.0".to_string()));
    map.insert("id".to_string(), id);
    map.insert("result".to_string(), result);
    Value::Object(map)
}

/// The JSON-RPC error code for a request the client cancelled before it was
/// answered.
pub(crate) const REQUEST_CANCELLED: i64 = -32800;

/// Builds a JSON-RPC error response.
pub(crate) fn response_error(id: Value, code: i64, message: &str) -> Value {
    let mut error = std::collections::BTreeMap::new();
    error.insert("code".to_string(), Value::Int(code));
    error.insert("message".to_string(), Value::String(message.to_string()));
    let mut map = std::collections::BTreeMap::new();
    map.insert("jsonrpc".to_string(), Value::String("2.0".to_string()));
    map.insert("id".to_string(), id);
    map.insert("error".to_string(), Value::Object(error));
    Value::Object(map)
}

/// Builds a JSON-RPC notification (a message without an `id`).
pub(crate) fn notification(method: &str, params: Value) -> Value {
    let mut map = std::collections::BTreeMap::new();
    map.insert("jsonrpc".to_string(), Value::String("2.0".to_string()));
    map.insert("method".to_string(), Value::String(method.to_string()));
    map.insert("params".to_string(), params);
    Value::Object(map)
}

/// Extracts a field from a JSON object, returning `Value::Null` when
/// absent so callers can unify the empty-field path with the
/// Null-field path.
pub(crate) fn field<'v>(object: &'v Value, key: &str) -> &'v Value {
    if let Value::Object(map) = object {
        map.get(key).unwrap_or(&Value::Null)
    } else {
        &Value::Null
    }
}

/// Extracts a string field from a JSON object.
pub(crate) fn field_str<'v>(object: &'v Value, key: &str) -> Option<&'v str> {
    if let Value::String(s) = field(object, key) {
        Some(s.as_str())
    } else {
        None
    }
}

/// Extracts a `u32` field from a JSON object (LSP uses non-negative
/// integers for line/character positions).
///
/// Accepts both `Value::Int` (an integer literal from the client, the
/// common case) and an integral non-negative `Value::Number` (a float
/// literal whose value is a whole number), so a client that sends `0`
/// or `0.0` for a position both parse.
pub(crate) fn field_u32(object: &Value, key: &str) -> Option<u32> {
    match field(object, key) {
        Value::Int(n) => u32::try_from(*n).ok(),
        Value::Number(n) if n.is_finite() && *n >= 0.0 && n.fract() == 0.0 => Some(*n as u32),
        _ => None,
    }
}

#[cfg(test)]
mod protocol_tests {
    use super::*;
    use std::collections::BTreeMap;

    fn position(line: f64, character: f64) -> Value {
        let mut p = BTreeMap::new();
        p.insert("line".to_string(), Value::Number(line));
        p.insert("character".to_string(), Value::Number(character));
        Value::Object(p)
    }

    #[test]
    fn integral_numbers_encode_without_trailing_zero() {
        // The position fields enter as `Value::Number(12.0)`; the wire
        // form must be `12`, not `12.0`, or a conformant client's
        // integer parse drops the value (and silently reads 0).
        let encoded = json::encode(&lsp_integers(&position(12.0, 5.0)));
        assert!(
            encoded.contains("\"line\":12") && !encoded.contains("12.0"),
            "line must serialize as an integer: {encoded}"
        );
        assert!(
            encoded.contains("\"character\":5") && !encoded.contains("5.0"),
            "character must serialize as an integer: {encoded}"
        );
    }

    #[test]
    fn nested_arrays_and_objects_are_normalized() {
        // Semantic-token data is an integer array nested under an object.
        let mut obj = BTreeMap::new();
        obj.insert(
            "data".to_string(),
            Value::Array(vec![Value::Number(2.0), Value::Number(0.0)]),
        );
        let encoded = json::encode(&lsp_integers(&Value::Object(obj)));
        assert_eq!(encoded, "{\"data\":[2,0]}");
    }

    #[test]
    fn write_message_frames_positions_as_integers() {
        // End-to-end through the transport: a response carrying a
        // `Value::Number(12.0)` position must reach the wire as `12`.
        let mut sink: Vec<u8> = Vec::new();
        {
            let mut transport = Transport::new(&b""[..], &mut sink);
            let response = response_ok(Value::Int(1), position(12.0, 5.0));
            transport.write_message(&response).unwrap();
        }
        let framed = String::from_utf8(sink).unwrap();
        let body = framed
            .split("\r\n\r\n")
            .nth(1)
            .expect("framed message has a body");
        assert!(
            body.contains("\"line\":12") && !body.contains("12.0"),
            "wire body must carry an integer line: {body}"
        );
    }

    #[test]
    fn field_u32_reads_integer_and_float_literals() {
        // A client integer parses as `Value::Int`; a float literal
        // parses as `Value::Number`. Both name a valid position.
        let from_int = json::parse("{\"line\": 7}").unwrap();
        let from_float = json::parse("{\"line\": 7.0}").unwrap();
        assert_eq!(field_u32(&from_int, "line"), Some(7));
        assert_eq!(field_u32(&from_float, "line"), Some(7));
        assert_eq!(field_u32(&from_int, "missing"), None);
    }

    #[test]
    fn content_length_header_is_case_insensitive() {
        let body = "{}";
        let frame = format!("content-length: {}\r\n\r\n{body}", body.len());
        let mut transport = Transport::new(frame.as_bytes(), Vec::<u8>::new());
        assert_eq!(
            transport.read_message().unwrap(),
            Some(Value::Object(BTreeMap::default()))
        );
    }
}
