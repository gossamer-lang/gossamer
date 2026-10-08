//! `http::ResponseStream::new()` builtins - a response body a handler
//! writes as it goes.
//!
//! The framing a client sees is the same one the compiled tiers produce.
//! The handle holds the stream's slot, which keeps the writing end, so the
//! body ends at `close` or when the last copy of the handle is gone.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

use gossamer_std::http::{StatusCode, StreamResponse};
use parking_lot::Mutex;

use crate::builtins::{BuiltinFnPub, as_str, value_to_int};
use crate::value::{RuntimeResult, Value};

/// The reading end of a response stream: whatever a handler wrote, in
/// order, then EOF once every writer is gone.
struct QueueReader {
    rx: Mutex<Receiver<Vec<u8>>>,
    pending: Vec<u8>,
    consumed: usize,
}

impl std::io::Read for QueueReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.consumed >= self.pending.len() {
            match self.rx.lock().recv() {
                Ok(chunk) => {
                    self.pending = chunk;
                    self.consumed = 0;
                }
                // Every writer is gone: the body is complete.
                Err(_) => return Ok(0),
            }
        }
        let available = &self.pending[self.consumed..];
        let take = available.len().min(out.len());
        out[..take].copy_from_slice(&available[..take]);
        self.consumed += take;
        Ok(take)
    }
}

fn slot_of(args: &[Value]) -> Option<Arc<crate::http_client_builtins::StreamSlot>> {
    args.first()
        .and_then(crate::http_client_builtins::response_stream_slot)
}

/// Hands `bytes` to the stream's reader. Answers how many bytes were
/// queued, or `-1` when the stream is closed - which is also what a
/// client that hung up looks like, so a producer can stop.
fn push(args: &[Value], bytes: Vec<u8>) -> i64 {
    slot_of(args).map_or(-1, |slot| slot.push(bytes))
}

/// Registers the `http::ResponseStream` builtins.
pub(crate) fn install_http_response_stream(globals: &mut Vec<(&'static str, Value)>) {
    let methods: &[(&str, BuiltinFnPub)] = &[
        ("new", builtin_new),
        ("write", builtin_write),
        ("write_bytes", builtin_write_bytes),
        ("close", builtin_close),
        ("is_open", builtin_is_open),
    ];
    for &(method, call) in methods {
        for key in [
            Box::leak(format!("ResponseStream::{method}").into_boxed_str()),
            Box::leak(format!("http::ResponseStream::{method}").into_boxed_str()),
        ] {
            globals.push((key, crate::builtins::builtin_pub(key, call)));
        }
    }
}

fn builtin_new(_args: &[Value]) -> RuntimeResult<Value> {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let reader = QueueReader {
        rx: Mutex::new(rx),
        pending: Vec::new(),
        consumed: 0,
    };
    let boxed: Box<dyn std::io::Read + Send + Sync + 'static> = Box::new(reader);
    let handle = crate::http_client_builtins::stream_handle_value(
        StreamResponse::from_reader(StatusCode::OK, boxed),
        Some(tx),
    );
    Ok(Value::struct_(
        "ResponseStream",
        Arc::unwrap_or_clone(Arc::new(vec![
            ("__handle", handle),
            ("status", Value::Int(200)),
            ("content_type", Value::String("".into())),
        ])),
    ))
}

fn builtin_write(args: &[Value]) -> RuntimeResult<Value> {
    let text = as_str(args.get(1).unwrap_or(&Value::Unit))
        .unwrap_or("")
        .as_bytes()
        .to_vec();
    Ok(Value::Int(push(args, text)))
}

fn builtin_write_bytes(args: &[Value]) -> RuntimeResult<Value> {
    let bytes = match args.get(1) {
        Some(Value::ByteVec(b)) => b.as_ref().clone(),
        Some(Value::ByteArray(b)) => b.to_vec(),
        Some(Value::InlineByteArray(b)) => b.as_ref().to_vec(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| value_to_int(v).map(|n| n as u8))
            .collect(),
        _ => Vec::new(),
    };
    Ok(Value::Int(push(args, bytes)))
}

fn builtin_close(args: &[Value]) -> RuntimeResult<Value> {
    if let Some(slot) = slot_of(args) {
        slot.close();
    }
    Ok(Value::Unit)
}

fn builtin_is_open(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Bool(
        slot_of(args).is_some_and(|slot| slot.is_open()),
    ))
}
