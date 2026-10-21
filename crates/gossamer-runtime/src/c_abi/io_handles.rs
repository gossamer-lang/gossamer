#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::wildcard_imports)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_sign_loss)]

//! Compiled-tier `std::io` stream adapters. Behavioural mirror of
//! `gossamer_std::io_handles`: every reader / writer the language
//! exposes is a counted row of one process-wide registry, and adapters
//! compose by holding references to their sources' rows. The two copies must
//! stay identical - the crate graph keeps `gossamer-std` out of the
//! runtime, so the logic is duplicated rather than shared.

use std::os::raw::c_char;

use super::*;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use parking_lot::Mutex;

/// Shared byte buffer behind a `pipe()` pair.
#[derive(Debug, Default)]
pub struct PipeBuffer {
    bytes: Mutex<Vec<u8>>,
    closed: AtomicBool,
}

/// One registry row: what a handle actually is.
#[derive(Debug)]
enum Node {
    /// In-memory reader over a fixed byte buffer.
    Memory { data: Vec<u8>, pos: usize },
    /// In-memory writer collecting everything written to it.
    Buffer(Vec<u8>),
    /// Reader yielding at most `remaining` more bytes from `src`.
    Limit { src: i64, remaining: i64 },
    /// Reader forwarding every byte read from `src` into `sink`.
    Tee { src: i64, sink: i64 },
    /// Reader draining each source in turn.
    Multi { sources: Vec<i64>, index: usize },
    /// One half of a `pipe()` pair.
    Pipe(Arc<PipeBuffer>),
}

/// A row and the references held to it: the `io::MemoryStream` handle naming it
/// and every adapter reading from it.
#[derive(Debug)]
struct Row {
    refs: usize,
    node: Node,
}

static NEXT_ID: AtomicI64 = AtomicI64::new(1);
static REGISTRY: Mutex<Option<HashMap<i64, Row>>> = Mutex::new(None);

/// Registers `node` with the one reference its handle holds.
fn insert(node: Node) -> i64 {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let mut guard = REGISTRY.lock();
    guard
        .get_or_insert_with(HashMap::new)
        .insert(id, Row { refs: 1, node });
    id
}

/// Takes a reference to each row an adapter reads from.
fn share(ids: &[i64]) {
    let mut guard = REGISTRY.lock();
    if let Some(table) = guard.as_mut() {
        for id in ids {
            if let Some(row) = table.get_mut(id) {
                row.refs += 1;
            }
        }
    }
}

/// Gives back one reference to row `id`; the last one removes the row and
/// gives back the references it held to the rows it reads from.
pub(crate) fn retire(id: i64) {
    let mut pending = vec![id];
    while let Some(id) = pending.pop() {
        let mut guard = REGISTRY.lock();
        let Some(table) = guard.as_mut() else {
            return;
        };
        let Some(row) = table.get_mut(&id) else {
            continue;
        };
        row.refs = row.refs.saturating_sub(1);
        if row.refs > 0 {
            continue;
        }
        if let Some(row) = table.remove(&id) {
            match row.node {
                Node::Limit { src, .. } => pending.push(src),
                Node::Tee { src, sink } => pending.extend([src, sink]),
                Node::Multi { sources, .. } => pending.extend(sources),
                Node::Memory { .. } | Node::Buffer(_) | Node::Pipe(_) => {}
            }
        }
    }
}

/// Registers an in-memory reader over `text`; returns its handle.
#[must_use]
pub fn string_reader(text: &str) -> i64 {
    insert(Node::Memory {
        data: text.as_bytes().to_vec(),
        pos: 0,
    })
}

/// Registers an in-memory writer; returns its handle.
#[must_use]
pub fn buffer_writer() -> i64 {
    insert(Node::Buffer(Vec::new()))
}

/// Registers a reader yielding at most `limit` bytes from `src`.
#[must_use]
pub fn limit_reader(src: i64, limit: i64) -> i64 {
    share(&[src]);
    insert(Node::Limit {
        src,
        remaining: limit.max(0),
    })
}

/// Registers a reader that mirrors everything read from `src` into
/// `sink` before returning it.
#[must_use]
pub fn tee_reader(src: i64, sink: i64) -> i64 {
    share(&[src, sink]);
    insert(Node::Tee { src, sink })
}

/// Registers a reader that drains each source in turn.
#[must_use]
pub fn multi_reader(sources: Vec<i64>) -> i64 {
    share(&sources);
    insert(Node::Multi { sources, index: 0 })
}

/// Registers a connected reader / writer pair sharing one buffer.
#[must_use]
pub fn pipe() -> (i64, i64) {
    let shared = Arc::new(PipeBuffer::default());
    let reader = insert(Node::Pipe(Arc::clone(&shared)));
    let writer = insert(Node::Pipe(shared));
    (reader, writer)
}

/// What a handle needs from the registry to serve one read, taken as a
/// snapshot so nested reads never hold the registry lock.
enum Step {
    Direct(Vec<u8>),
    Limit { src: i64, take: i64 },
    Tee { src: i64, sink: i64 },
    Multi { source: i64, index: usize },
    Exhausted,
}

fn step(id: i64, max: usize) -> Step {
    let mut guard = REGISTRY.lock();
    let Some(table) = guard.as_mut() else {
        return Step::Exhausted;
    };
    match table.get_mut(&id).map(|row| &mut row.node) {
        Some(Node::Memory { data, pos }) => {
            let end = (*pos + max).min(data.len());
            let out = data[*pos..end].to_vec();
            *pos = end;
            Step::Direct(out)
        }
        Some(Node::Pipe(shared)) => {
            let shared = Arc::clone(shared);
            drop(guard);
            let mut bytes = shared.bytes.lock();
            let take = max.min(bytes.len());
            Step::Direct(bytes.drain(..take).collect())
        }
        Some(Node::Limit { src, remaining }) => {
            let take = (*remaining).min(max as i64);
            Step::Limit { src: *src, take }
        }
        Some(Node::Tee { src, sink }) => Step::Tee {
            src: *src,
            sink: *sink,
        },
        Some(Node::Multi { sources, index }) => match sources.get(*index) {
            Some(source) => Step::Multi {
                source: *source,
                index: *index,
            },
            None => Step::Exhausted,
        },
        Some(Node::Buffer(_)) | None => Step::Exhausted,
    }
}

fn consume_limit(id: i64, n: i64) {
    let mut guard = REGISTRY.lock();
    if let Some(Node::Limit { remaining, .. }) = guard
        .as_mut()
        .and_then(|t| t.get_mut(&id))
        .map(|row| &mut row.node)
    {
        *remaining = (*remaining - n).max(0);
    }
}

fn advance_multi(id: i64, from: usize) {
    let mut guard = REGISTRY.lock();
    if let Some(Node::Multi { index, .. }) = guard
        .as_mut()
        .and_then(|t| t.get_mut(&id))
        .map(|row| &mut row.node)
        && *index == from
    {
        *index += 1;
    }
}

/// Reads up to `max` bytes from the reader handle `id`. An empty
/// result means end of stream.
#[must_use]
pub fn read(id: i64, max: usize) -> Vec<u8> {
    if max == 0 {
        return Vec::new();
    }
    match step(id, max) {
        Step::Direct(bytes) => bytes,
        Step::Limit { src, take } => {
            if take <= 0 {
                return Vec::new();
            }
            let out = read(src, take as usize);
            consume_limit(id, out.len() as i64);
            out
        }
        Step::Tee { src, sink } => {
            let out = read(src, max);
            if !out.is_empty() {
                write(sink, &out);
            }
            out
        }
        Step::Multi { source, index } => {
            let out = read(source, max);
            if out.is_empty() {
                advance_multi(id, index);
                return read(id, max);
            }
            out
        }
        Step::Exhausted => Vec::new(),
    }
}

/// Appends `bytes` to the writer handle `id`; returns the number of
/// bytes accepted (zero for an unknown or closed handle).
pub fn write(id: i64, bytes: &[u8]) -> usize {
    let mut guard = REGISTRY.lock();
    let Some(table) = guard.as_mut() else {
        return 0;
    };
    match table.get_mut(&id).map(|row| &mut row.node) {
        Some(Node::Buffer(buf)) => {
            buf.extend_from_slice(bytes);
            bytes.len()
        }
        Some(Node::Pipe(shared)) => {
            let shared = Arc::clone(shared);
            drop(guard);
            if shared.closed.load(Ordering::Acquire) {
                return 0;
            }
            shared.bytes.lock().extend_from_slice(bytes);
            bytes.len()
        }
        _ => 0,
    }
}

/// Copies at most `n` bytes from `src` to `dst`; returns the count
/// actually transferred.
pub fn copy_n(dst: i64, src: i64, n: i64) -> i64 {
    let mut moved = 0i64;
    while moved < n {
        let want = ((n - moved) as usize).min(8192);
        let chunk = read(src, want);
        if chunk.is_empty() {
            break;
        }
        write(dst, &chunk);
        moved += chunk.len() as i64;
    }
    moved
}

/// Drains the reader handle `id` to end of stream as UTF-8 text;
/// invalid sequences are replaced.
#[must_use]
pub fn drain(id: i64) -> String {
    let mut out: Vec<u8> = Vec::new();
    loop {
        let chunk = read(id, 8192);
        if chunk.is_empty() {
            break;
        }
        out.extend_from_slice(&chunk);
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Everything written to the writer handle `id`, as UTF-8 text.
#[must_use]
pub fn contents(id: i64) -> String {
    let guard = REGISTRY.lock();
    let bytes = match guard.as_ref().and_then(|t| t.get(&id)).map(|row| &row.node) {
        Some(Node::Buffer(buf)) => buf.clone(),
        Some(Node::Pipe(shared)) => shared.bytes.lock().clone(),
        _ => Vec::new(),
    };
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Marks a pipe writer closed so its reader stops accepting writes.
pub fn close_writer(id: i64) {
    let guard = REGISTRY.lock();
    if let Some(Node::Pipe(shared)) = guard.as_ref().and_then(|t| t.get(&id)).map(|row| &row.node) {
        shared.closed.store(true, Ordering::Release);
    }
}

// --- C-ABI surface -------------------------------------------------

unsafe fn borrowed(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: this `unsafe fn`'s caller passes `p` live; non-null, checked above.
    unsafe { crate::c_abi::gos_str_arg_string(p) }
}

/// The handle compiled code holds for the new row `id`: a counted node
/// whose last share gives the row's reference back.
fn stream(id: i64) -> i64 {
    super::registry_key::key_handle(id, retire)
}

/// The row a handle from compiled code names.
fn row(h: i64) -> i64 {
    super::registry_key::key_id(h)
}

/// `io::string_reader(text) -> io::MemoryStream`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_io_string_reader(text: *const c_char) -> i64 {
    // SAFETY: `text` is this shim's argument, live for the call (C-ABI contract) or null, which
    // `borrowed` accepts.
    ffi_entry!({ stream(string_reader(&unsafe { borrowed(text) })) })
}

/// `io::buffer_writer() -> io::MemoryStream`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_io_buffer_writer() -> i64 {
    ffi_entry!({ stream(buffer_writer()) })
}

/// `io::limit_reader(src, limit) -> io::MemoryStream`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_io_limit_reader(src: i64, limit: i64) -> i64 {
    ffi_entry!({ stream(limit_reader(row(src), limit)) })
}

/// `io::tee_reader(src, sink) -> io::MemoryStream`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_io_tee_reader(src: i64, sink: i64) -> i64 {
    ffi_entry!({ stream(tee_reader(row(src), row(sink))) })
}

/// `io::multi_reader(sources) -> io::MemoryStream` over a `Vec<io::MemoryStream>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_io_multi_reader(sources: *const GosVec) -> i64 {
    ffi_entry!({
        // SAFETY: `sources` is this shim's argument, null or a live `Vec` for the call (C-ABI
        // contract).
        let ids: Vec<i64> = unsafe { sources.as_ref() }.map_or_else(Vec::new, |header| {
            (0..header.len)
                // SAFETY: `i` counts below the vec's length.
                .map(|i| row(unsafe { crate::c_abi::vec::vec_elem_load_i64(header, i) }))
                .collect()
        });
        stream(multi_reader(ids))
    })
}

thread_local! {
    static PIPE_ANSWER: std::cell::Cell<[i64; 2]> = const { std::cell::Cell::new([0; 2]) };
}

/// `io::pipe() -> (io::MemoryStream, io::MemoryStream)` - the pair as a 2-slot tuple, each
/// handle holding its share for the caller.
///
/// The answer points at this thread's answer slot, which compiled code copies
/// into its own tuple as the call returns, before it can call again.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_io_pipe() -> *mut i64 {
    ffi_entry!({
        let (reader, writer) = pipe();
        PIPE_ANSWER.with(|slot| {
            slot.set([stream(reader), stream(writer)]);
            slot.as_ptr().cast::<i64>()
        })
    })
}

/// `io::copy_n(dst, src, n) -> Result<i64, errors::Error>`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_io_copy_n(dst: i64, src: i64, n: i64) -> i128 {
    ffi_entry!({
        if n < 0 {
            let err =
                crate::c_abi::errors::error_new_from_bytes(b"io::copy_n: negative byte count");
            return gos_rt_result_new(1, err as i64);
        }
        gos_rt_result_new(0, copy_n(row(dst), row(src), n))
    })
}

/// `io::drain(src) -> String` - reads the stream to its end.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_io_drain(src: i64) -> *mut c_char {
    ffi_entry!({ alloc_cstring(drain(row(src)).as_bytes()) })
}

/// `io::contents(writer) -> String` - everything written so far.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_io_contents(writer: i64) -> *mut c_char {
    ffi_entry!({ alloc_cstring(contents(row(writer)).as_bytes()) })
}

/// `io::write(writer, text) -> i64` - bytes accepted by the writer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_io_write_str(writer: i64, text: *const c_char) -> i64 {
    ffi_entry!({
        // SAFETY: `text` is this shim's argument, live for the call (C-ABI contract) or null,
        // which `borrowed` accepts.
        write(row(writer), unsafe { borrowed(text) }.as_bytes()) as i64
    })
}

/// `io::close_writer(writer)` - signals end of stream on a pipe.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_io_close_writer(writer: i64) {
    ffi_entry!({ close_writer(row(writer)) });
}
