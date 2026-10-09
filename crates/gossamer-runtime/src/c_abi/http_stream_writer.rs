#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]

//! `http::ResponseStream::new()` - a response body a handler writes as it
//! goes.
//!
//! `Response::text` and `Response::json` need the whole body in memory
//! before the first byte reaches the wire, which rules out server-sent
//! events, a large download, a progressive render, and a log tail. A
//! response stream is the other half: the handler answers immediately with
//! the stream as its body, and whatever writes into it is framed to the
//! client as it arrives.
//!
//! ```text
//! fn events(_r: http::Request) -> Result<http::Response, errors::Error> {
//!     let body = http::ResponseStream::new()
//!     go feed(body)
//!     Ok(http::Response::stream(200, "text/event-stream", body))
//! }
//! ```
//!
//! The writer and the reader are the two ends of one queue: a write hands
//! the bytes over and returns, and the server's chunked drain takes them.
//! Closing ends the body; dropping the last writer does too, so a producer
//! that panics cannot leave the client waiting forever.

use std::os::raw::c_char;
use std::sync::mpsc::{Receiver, Sender};

use parking_lot::Mutex;

use super::GosVec;

/// The reading end of a response stream: whatever a handler wrote, in
/// order, then EOF once every writer is gone.
struct QueueReader {
    /// Behind a mutex so the reader is `Sync`: the registry stores it as
    /// a shared `Read`, and only the connection draining this body ever
    /// reads from it.
    rx: Mutex<Receiver<Vec<u8>>>,
    /// Bytes taken from the queue but not yet handed to the caller's
    /// buffer. A `read` that cannot fit a whole chunk keeps the tail here.
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

/// Writers by stream handle. An entry is removed by `close`, and by the
/// last writer going away, which is what ends the body.
fn writers() -> &'static Mutex<std::collections::HashMap<i64, Sender<Vec<u8>>>> {
    static WRITERS: std::sync::OnceLock<Mutex<std::collections::HashMap<i64, Sender<Vec<u8>>>>> =
        std::sync::OnceLock::new();
    WRITERS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// The handle a `ResponseStream` blob carries in its first slot.
///
/// # Safety
///
/// `rs` is null or addresses a live response-stream cell.
unsafe fn handle_of(rs: *const i64) -> i64 {
    if rs.is_null() {
        return -1;
    }
    // SAFETY: `rs` is non-null (checked above) and, per this `unsafe fn`'s contract, a live
    // response-stream cell.
    unsafe { *rs }
}

/// Hands `bytes` to the stream's reader. Answers how many bytes were
/// queued, or `-1` when the stream is closed - which is also what a
/// client that hung up looks like, so a producer can stop.
///
/// # Safety
///
/// `rs` is null or addresses a live response-stream cell.
unsafe fn push(rs: *const i64, bytes: Vec<u8>) -> i64 {
    // SAFETY: this function's contract covers `rs`, as `handle_of` requires.
    let handle = unsafe { handle_of(rs) };
    let len = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
    let guard = writers().lock();
    match guard.get(&handle) {
        Some(tx) if tx.send(bytes).is_ok() => len,
        _ => -1,
    }
}

/// `http::ResponseStream::new() -> ResponseStream` - a body the handler
/// writes as it goes.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_http_response_stream_open() -> *mut i64 {
    ffi_entry!({
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let reader = QueueReader {
            rx: Mutex::new(rx),
            pending: Vec::new(),
            consumed: 0,
        };
        let boxed: Box<dyn std::io::Read + Send + Sync> = Box::new(reader);
        let handle = super::http_client::stream_registry_register(std::io::BufReader::new(boxed));
        writers().lock().insert(handle, tx);
        super::http_client::alloc_response_stream_blob_public(handle, 200, "")
    })
}

/// `stream.write(text) -> i64` - queues `text`, answering the byte count
/// or `-1` once the stream is closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_response_stream_write(
    rs: *const i64,
    text: *const c_char,
) -> i64 {
    ffi_entry!({
        if text.is_null() {
            return 0;
        }
        // SAFETY: `text` is a String argument from compiled code, null or a live string body for the whole call.
        let bytes = unsafe { crate::c_abi::gos_str_arg_bytes(text) }.to_vec();
        // SAFETY: `rs` is this shim's argument, null or a live response-stream cell (C-ABI
        // contract).
        unsafe { push(rs, bytes) }
    })
}

/// `stream.write_bytes(bytes) -> i64` - the binary counterpart, for a
/// download that is not text.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_response_stream_write_bytes(
    rs: *const i64,
    bytes: *const GosVec,
) -> i64 {
    ffi_entry!({
        if bytes.is_null() {
            return 0;
        }
        // SAFETY: `bytes` is a handle from compiled code, checked non-null above and live for the whole call.
        let vec = unsafe { &*bytes };
        let len = vec.len.max(0) as usize;
        let elem = vec.elem_bytes.max(1) as usize;
        let mut out = Vec::with_capacity(len);
        for i in 0..len {
            // A byte vector's slot is one byte wide; a wider element means
            // the caller passed something that is not `Vec<u8>`.
            // SAFETY: `i` counts below the vec's length, so the element's slot lies inside its
            // buffer.
            let slot = unsafe { vec.ptr.add(i * elem) };
            // SAFETY: `slot` addresses the first byte of an element, which a byte vector stores
            // at any width.
            out.push(unsafe { slot.read() });
        }
        // SAFETY: `rs` is this shim's argument, null or a live response-stream cell (C-ABI
        // contract).
        unsafe { push(rs, out) }
    })
}

/// `stream.close()` - ends the body. Idempotent.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_response_stream_close(rs: *const i64) {
    ffi_entry!({
        // SAFETY: `rs` is this shim's argument, as `handle_of` requires (C-ABI contract).
        writers().lock().remove(&unsafe { handle_of(rs) });
    });
}

/// Whether the stream is still open, so a producer can stop when the
/// client has gone.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_response_stream_is_open(rs: *const i64) -> i64 {
    ffi_entry!({
        // SAFETY: `rs` is this shim's argument, as `handle_of` requires (C-ABI contract).
        i64::from(writers().lock().contains_key(&unsafe { handle_of(rs) }))
    })
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;

    use super::*;

    #[test]
    fn a_reader_sees_writes_in_order_then_eof() {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let mut reader = QueueReader {
            rx: Mutex::new(rx),
            pending: Vec::new(),
            consumed: 0,
        };
        tx.send(b"one".to_vec()).unwrap();
        tx.send(b"two".to_vec()).unwrap();
        drop(tx);
        let mut seen = Vec::new();
        reader.read_to_end(&mut seen).unwrap();
        assert_eq!(seen, b"onetwo");
    }

    #[test]
    fn a_read_smaller_than_a_chunk_keeps_the_tail() {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let mut reader = QueueReader {
            rx: Mutex::new(rx),
            pending: Vec::new(),
            consumed: 0,
        };
        tx.send(b"abcdef".to_vec()).unwrap();
        drop(tx);
        let mut small = [0u8; 2];
        assert_eq!(reader.read(&mut small).unwrap(), 2);
        assert_eq!(&small, b"ab");
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"cdef");
    }
}
