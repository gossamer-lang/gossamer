#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::similar_names)]
#![allow(clippy::many_single_char_names)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::same_length_and_capacity)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::cast_ptr_alignment)]
#![allow(clippy::ptr_as_ptr)]
#![allow(static_mut_refs)]
#![allow(unused_unsafe)]
#![allow(clippy::wildcard_imports)]

use std::os::raw::c_char;

use serde::Deserialize;

use super::*;

// Keep compiled JSON's resource limits aligned with the VM standard
// library. `serde_json` otherwise accepts unbounded input and nesting,
// letting an HTTP-facing compiled program allocate or recurse far beyond the
// limits enforced by `gossamer_std::json::parse`.
const JSON_MAX_SIZE: usize = 16 * 1024 * 1024;
const JSON_MAX_DEPTH: usize = 128;

/// Validates a C JSON input once before handing it to `serde_json`. The scan
/// is allocation-free, understands quoted strings/escapes, and rejects only
/// limits that the VM parser already rejects. Syntax remains `serde_json`'s
/// responsibility so its detailed parse diagnostics are preserved.
fn checked_json_text(bytes: &[u8]) -> Result<&str, &'static str> {
    if bytes.len() > JSON_MAX_SIZE {
        return Err("input exceeds max_size (16 MiB)");
    }
    let text = std::str::from_utf8(bytes).map_err(|_| "invalid UTF-8")?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for byte in bytes {
        if in_string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match *byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                if depth > JSON_MAX_DEPTH {
                    return Err("nesting depth exceeds max_depth (128)");
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(text)
}

/// Parses after [`checked_json_text`] has applied Gossamer's explicit depth
/// limit. `serde_json`'s default recursion counter rejects the valid
/// VM-boundary document at depth 128 one level early, so disable only that
/// duplicate guard and keep the bounded preflight as the authority.
fn parse_checked_json(text: &str) -> Result<serde_json::Value, serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    deserializer.disable_recursion_limit();
    let value = serde_json::Value::deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(value)
}

/// Fully validates a document without constructing a DOM. Parsed documents
/// stay in this compact form until an API actually projects a child or value.
fn validate_checked_json(text: &str) -> Result<(), serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    deserializer.disable_recursion_limit();
    serde::de::IgnoredAny::deserialize(&mut deserializer)?;
    deserializer.end()
}

// ---------------------------------------------------------------
// JSON runtime - wraps `serde_json::Value` behind a heap pointer
// so user code can do `json::parse(s)`, `value.field`, and
// `value.as_i64()` from compiled Gossamer. The MIR lowerer
// rewrites field access on a `json::Value` receiver into a
// `gos_rt_json_get(value, "field")` call before the cranelift
// backend sees it.
// ---------------------------------------------------------------

/// Heap-allocated JSON node. The compiled tier shuttles raw
/// `*mut GosJson` pointers through normal i64 slots; the runtime
/// owns every node exclusively (each helper that "returns" a value
/// boxes a fresh node). Lifetime tied to the next
/// `gos_rt_gc_reset` only for the cstring helpers - JSON nodes are
/// Heap-allocated JSON node. The compiled tier shuttles raw
/// `*mut GosJson` pointers through normal i64 slots; each handle
/// carries a shared `Arc<serde_json::Value>` keeping the parsed
/// tree alive plus a stable interior pointer naming the specific
/// sub-node this handle refers to.
///
/// Why this shape: `serde_json::Value::clone()` is O(N) on a
/// nested tree. Previously every `gos_rt_json_get` call deep-cloned
/// the matched child and `Box`-leaked the copy, so a single askq
/// chat round walked a 10-deep delta tree per chunk × 200 chunks
/// = thousands of multi-KB clones leaking permanently. The
/// `Arc<Value>`-shared model bumps a refcount instead of cloning;
/// child views are interior pointers into the same allocation.
/// Tree storage drops when the last GosJson referencing it is
/// freed (or, today, when the GC reclaims its leaked Box).
///
/// **Pointer stability:** `Arc::new(value)` allocates the Value on
/// the heap via the global allocator. The Value's address never
/// moves while any `Arc` referencing it lives, so the
/// `view: *const Value` field is stable for the GosJson's
/// lifetime. This is the same trick `Pin<Arc<T>>` uses;
/// formalising it via `Pin` would not change the layout.
///
/// See `~/dev/contexts/lang/fix_architecture_ownership.md`
/// Stage 2 (final form).
enum JsonTree {
    Value(serde_json::Value),
    Raw {
        text: Box<str>,
        parsed: std::sync::OnceLock<serde_json::Value>,
    },
}

impl JsonTree {
    fn value(&self) -> &serde_json::Value {
        match self {
            Self::Value(value) => value,
            Self::Raw { text, parsed } => parsed
                .get_or_init(|| parse_checked_json(text).expect("validated JSON must reparse")),
        }
    }

    fn raw_text(&self) -> Option<&str> {
        match self {
            Self::Raw { text, parsed } if parsed.get().is_none() => Some(text),
            _ => None,
        }
    }
}

pub struct GosJson {
    /// Owning shared reference to the parsed-once value tree. Kept
    /// alive for the duration of the GosJson; cloning a GosJson
    /// only bumps this refcount (not a deep copy).
    tree: std::sync::Arc<JsonTree>,
    /// View into `tree`'s subtree. Always points to a sub-Value of
    /// `tree`'s root. Stable as long as `tree` is alive.
    view: SyncRawPtr<serde_json::Value>,
}

impl GosJson {
    /// Wraps a fresh `serde_json::Value` as the root of its own
    /// tree. Allocates one `Arc<Value>` and one `Box<GosJson>`.
    pub(crate) fn into_raw(value: serde_json::Value) -> *mut GosJson {
        let tree = std::sync::Arc::new(JsonTree::Value(value));
        let view = std::ptr::from_ref(tree.value());
        Box::into_raw(Box::new(GosJson {
            tree,
            view: SyncRawPtr::new(view.cast_mut()),
        }))
    }

    fn raw(text: &str) -> *mut GosJson {
        Box::into_raw(Box::new(GosJson {
            tree: std::sync::Arc::new(JsonTree::Raw {
                text: text.into(),
                parsed: std::sync::OnceLock::new(),
            }),
            view: SyncRawPtr::NULL,
        }))
    }

    /// Builds a child handle that shares the same tree as `self`
    /// and points at `child` inside it. `child` must be a
    /// reference into `self.tree`'s subtree (the type system
    /// cannot enforce this here because we cross the FFI; every
    /// caller below derives `child` via `serde_json::Value::get`
    /// on `self.view`'s subtree, which is sound).
    fn child(&self, child: &serde_json::Value) -> *mut GosJson {
        Box::into_raw(Box::new(GosJson {
            tree: std::sync::Arc::clone(&self.tree),
            view: SyncRawPtr::new(std::ptr::from_ref(child).cast_mut()),
        }))
    }

    fn value(&self) -> &serde_json::Value {
        if self.view.is_null() {
            self.tree.value()
        } else {
            unsafe { &*self.view.as_const_ptr() }
        }
    }

    fn null_ptr() -> *mut GosJson {
        Self::into_raw(serde_json::Value::Null)
    }
}

unsafe fn json_borrow<'a>(p: *const GosJson) -> Option<&'a serde_json::Value> {
    if p.is_null() {
        return None;
    }
    // Arc<serde_json::Value> pointers are always >> 1 on any real allocator.
    // If the first word is 0 or 1 we received a *mut GosResult (disc + payload)
    // instead of a *const GosJson - unwrap the Option layer transparently.
    let first_word = unsafe { *(p as *const u64) };
    if first_word <= 1 {
        if first_word == 0 {
            // disc=0 (Some): offset-8 holds the inner *mut GosJson as i64.
            let payload = unsafe { *((p as *const u64).add(1)) };
            if payload == 0 {
                return None;
            }
            return unsafe { json_borrow(payload as *const GosJson) };
        }
        // disc=1 (None)
        return None;
    }
    let json = unsafe { &*p };
    // SAFETY: `view` was set by `Self::into_raw` (points at the
    // tree's root) or by `Self::child` (points at a sub-Value of
    // `self.tree`'s subtree). Either way the pointee lives as
    // long as `tree` does, which is at least until this `&GosJson`
    // dies - i.e. at least until this function returns.
    Some(json.value())
}

/// Borrows the `serde_json::Value` a `GosJson` handle views, for
/// sibling runtime modules (e.g. yaml encoding) that project a parsed
/// JSON tree onto another format. `None` for a null/None handle.
pub(crate) unsafe fn json_value_ref<'a>(p: *const GosJson) -> Option<&'a serde_json::Value> {
    unsafe { json_borrow(p) }
}

/// Resolves `p` and returns the GosJson struct itself so the
/// caller can construct child handles via `Self::child`. Returns
/// `None` only for null inputs.
unsafe fn json_handle<'a>(p: *const GosJson) -> Option<&'a GosJson> {
    if p.is_null() {
        return None;
    }
    // Same GosResult-vs-GosJson guard as json_borrow.
    let first_word = unsafe { *(p as *const u64) };
    if first_word <= 1 {
        if first_word == 0 {
            let payload = unsafe { *((p as *const u64).add(1)) };
            if payload == 0 {
                return None;
            }
            return unsafe { json_handle(payload as *const GosJson) };
        }
        return None;
    }
    Some(unsafe { &*p })
}

/// `json::parse(text) -> Result<json::Value, String>` runtime
/// `json::valid(text) -> bool` - true when `text` parses as
/// well-formed JSON. Mirrors the interp `json::valid` builtin.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_valid(text: *const c_char) -> i8 {
    ffi_entry!(0, {
        let bytes: &[u8] = if text.is_null() {
            b""
        } else {
            unsafe { crate::c_abi::gos_str_arg_bytes(text) }
        };
        let ok = checked_json_text(bytes)
            .ok()
            .and_then(|s| validate_checked_json(s).ok())
            .is_some();
        i8::from(ok)
    })
}

/// entry point. Returns a real `GosResult` so `match` and `?`
/// work across function boundaries in compiled code.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_parse(text: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        let bytes: &[u8] = if text.is_null() {
            b""
        } else {
            unsafe { crate::c_abi::gos_str_arg_bytes(text) }
        };
        match checked_json_text(bytes) {
            Ok(s) => match validate_checked_json(s) {
                Ok(()) => {
                    let ptr = GosJson::raw(s);
                    unsafe { gos_rt_result_new(0, ptr as i64) }
                }
                Err(error) => {
                    let message = error.to_string();
                    let err = crate::c_abi::errors::error_new_from_bytes(message.as_bytes());
                    unsafe { gos_rt_result_new(1, err as i64) }
                }
            },
            Err(message) => {
                let err = crate::c_abi::errors::error_new_from_bytes(message.as_bytes());
                unsafe { gos_rt_result_new(1, err as i64) }
            }
        }
    })
}

/// Frees a `GosJson` handle (drops its `Arc` share of the parsed
/// tree; the tree itself dies with its last handle). Null-safe.
/// Emitted by the drop pass for provably single-owner JSON locals.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_free(j: *mut GosJson) {
    if j.is_null() {
        return;
    }
    drop(unsafe { Box::from_raw(j) });
}

/// Takes the value a builder box holds, consuming the box.
///
/// A box the encoder just built owns its whole tree, so the value moves out
/// with no copy. A handle that shares a parsed document, or one viewing a
/// subtree, answers a copy of what it views - the document stays whole.
unsafe fn take_json_value(p: *mut GosJson) -> serde_json::Value {
    if p.is_null() {
        return serde_json::Value::Null;
    }
    let boxed = unsafe { Box::from_raw(p) };
    let views_root = boxed.view.is_null()
        || std::ptr::eq(
            boxed.view.as_const_ptr(),
            std::ptr::from_ref(boxed.tree.value()),
        );
    if !views_root {
        return unsafe { &*boxed.view.as_const_ptr() }.clone();
    }
    match std::sync::Arc::try_unwrap(boxed.tree) {
        Ok(JsonTree::Value(value)) => value,
        Ok(other) => other.value().clone(),
        Err(shared) => shared.value().clone(),
    }
}

/// `json::Value::Array` over a builder vector whose element boxes it consumes.
///
/// The borrowing form copies every child into the array and leaves the boxes
/// to the caller, which is a deep copy of the whole subtree at each level of a
/// nested value. This one moves each child in and frees its box, so building
/// an array of objects costs the objects and nothing more.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_value_array_owned(vec: *mut GosVec) -> *mut GosJson {
    ffi_entry!(std::ptr::null_mut(), {
        let mut out: Vec<serde_json::Value> = Vec::new();
        if !vec.is_null() {
            let header = unsafe { &*vec };
            let len = usize::try_from(header.len.max(0)).unwrap_or(0);
            if !header.ptr.is_null() && len > 0 {
                out.reserve(len);
                let base = header.ptr;
                for i in 0..len {
                    let addr = unsafe { base.add(i * 8).cast::<usize>().read_unaligned() };
                    let elem: *mut GosJson = std::ptr::with_exposed_provenance_mut(addr);
                    out.push(unsafe { take_json_value(elem) });
                }
            }
        }
        unsafe { crate::c_abi::gos_rt_vec_free(vec) };
        GosJson::into_raw(serde_json::Value::Array(out))
    })
}

/// `json::Value::Object` over a name/value builder vector whose value boxes it
/// consumes. The name slots are borrowed C strings; only the values move.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_value_object_owned(vec: *mut GosVec) -> *mut GosJson {
    ffi_entry!(std::ptr::null_mut(), {
        let mut out = serde_json::Map::new();
        if !vec.is_null() {
            let header = unsafe { &*vec };
            let raw_len = usize::try_from(header.len.max(0)).unwrap_or(0);
            let elem_bytes = header.elem_bytes as usize;
            let header_looks_valid =
                matches!(elem_bytes, 8 | 16 | 24) && raw_len <= 16 * 1024 * 1024;
            if header_looks_valid && !header.ptr.is_null() && raw_len > 0 {
                let tuple_count = if elem_bytes == 16 {
                    raw_len
                } else {
                    raw_len / 2
                };
                let pairs = unsafe {
                    std::slice::from_raw_parts(header.ptr.cast::<[i64; 2]>(), tuple_count)
                };
                for pair in pairs {
                    let key_ptr = pair[0] as *const c_char;
                    let val_ptr = pair[1] as *mut GosJson;
                    let key = if key_ptr.is_null() {
                        String::new()
                    } else {
                        unsafe { crate::c_abi::gos_str_arg_string(key_ptr) }
                    };
                    out.insert(key, unsafe { take_json_value(val_ptr) });
                }
            }
        }
        unsafe { crate::c_abi::gos_rt_vec_free(vec) };
        GosJson::into_raw(serde_json::Value::Object(out))
    })
}

/// A second handle onto the same document, for a storage that duplicates a
/// slot holding one.
///
/// A handle carries no count of its own - it is a box over a shared tree - so
/// a copied slot takes a box of its own and the tree's `Arc` gains a holder.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_clone_handle(p: *const GosJson) -> *mut GosJson {
    if p.is_null() {
        return std::ptr::null_mut();
    }
    let src = unsafe { &*p };
    Box::into_raw(Box::new(GosJson {
        tree: std::sync::Arc::clone(&src.tree),
        view: SyncRawPtr::new(src.view.as_const_ptr().cast_mut()),
    }))
}

/// Frees the `GosJson` handles a builder vector holds, then the vector.
///
/// A container constructor copies every child it is handed, so the boxes the
/// walk built are the caller's to reclaim. `first` is the index of the first
/// owned slot and `stride` the step between them: a value array owns every
/// slot, an object's name/value pairs own every second one.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_free_slots(vec: *mut GosVec, first: i64, stride: i64) {
    if vec.is_null() {
        return;
    }
    let header = unsafe { &*vec };
    let len = usize::try_from(header.len.max(0)).unwrap_or(0);
    let first = usize::try_from(first.max(0)).unwrap_or(0);
    let stride = usize::try_from(stride.max(1)).unwrap_or(1);
    if !header.ptr.is_null() {
        let base = header.ptr;
        let mut i = first;
        while i < len {
            let addr = unsafe { base.add(i * 8).cast::<usize>().read_unaligned() };
            let child: *mut GosJson = std::ptr::with_exposed_provenance_mut(addr);
            unsafe { gos_rt_json_free(child) };
            i += stride;
        }
    }
    unsafe { crate::c_abi::gos_rt_vec_free(vec) };
}

/// `serde_json::to_writer` sink backed directly by the compiled String ABI.
/// Each write consumes the current unique builder and returns its possibly
/// reallocated pointer. HTML-sensitive characters are escaped inline, so no
/// whole-document replacement buffer is needed afterwards.
struct RuntimeJsonWriter {
    string: *mut c_char,
    len: usize,
    capacity: usize,
}

impl RuntimeJsonWriter {
    fn new(capacity: usize) -> Self {
        let capacity = capacity.min(u32::MAX as usize);
        Self {
            string: gos_rt_str_with_capacity(i64::try_from(capacity).unwrap_or(i64::MAX)),
            len: 0,
            capacity,
        }
    }

    fn append(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let new_len = self.len.saturating_add(bytes.len());
        if new_len <= self.capacity {
            unsafe { str_builder_write_reserved(self.string, self.len, bytes) };
        } else {
            self.string = unsafe {
                gos_rt_str_append_bytes(
                    self.string,
                    bytes.as_ptr(),
                    i64::try_from(bytes.len()).unwrap_or(i64::MAX),
                )
            };
            self.capacity = new_len.saturating_mul(2).max(64).min(u32::MAX as usize);
        }
        self.len = new_len;
    }

    fn finish(mut self) -> *mut c_char {
        let string = self.string;
        self.string = std::ptr::null_mut();
        string
    }
}

impl std::io::Write for RuntimeJsonWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let mut start = 0;
        let mut offset = 0;
        while offset < bytes.len() {
            let (source_len, replacement): (usize, Option<&[u8]>) = match bytes[offset] {
                b'<' => (1, Some(b"\\u003c")),
                b'>' => (1, Some(b"\\u003e")),
                b'&' => (1, Some(b"\\u0026")),
                0xe2 if bytes.get(offset + 1) == Some(&0x80) => match bytes.get(offset + 2) {
                    Some(0xa8) => (3, Some(b"\\u2028")),
                    Some(0xa9) => (3, Some(b"\\u2029")),
                    _ => (1, None),
                },
                _ => (1, None),
            };
            let Some(replacement) = replacement else {
                offset += source_len;
                continue;
            };
            self.append(&bytes[start..offset]);
            self.append(replacement);
            offset += source_len;
            start = offset;
        }
        self.append(&bytes[start..]);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for RuntimeJsonWriter {
    fn drop(&mut self) {
        if !self.string.is_null() {
            unsafe { gos_rt_str_free(self.string) };
        }
    }
}

fn render_json_direct(value: &serde_json::Value, pretty: bool) -> *mut c_char {
    // A recursive exact-size pass formats every number and walks the complete
    // tree before serde immediately repeats the work. Start large enough to
    // avoid churn for ordinary documents and let the builder double for large
    // payloads. Peak growth stays bounded while serialization remains one pass.
    let mut writer = RuntimeJsonWriter::new(64 * 1024);
    let result = if pretty {
        serde_json::to_writer_pretty(&mut writer, value)
    } else {
        serde_json::to_writer(&mut writer, value)
    };
    if result.is_err() {
        return alloc_cstring(b"");
    }
    writer.finish()
}

fn render_json_raw(text: &str) -> *mut c_char {
    use std::io::Write as _;

    let mut writer = RuntimeJsonWriter::new(text.len().max(64 * 1024));
    let bytes = text.as_bytes();
    let mut in_string = false;
    let mut escaped = false;
    let mut start = 0usize;
    let mut offset = 0usize;
    while offset < bytes.len() {
        let byte = bytes[offset];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            offset += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            offset += 1;
            continue;
        }
        if byte.is_ascii_whitespace() {
            let _ = writer.write_all(&bytes[start..offset]);
            offset += 1;
            while offset < bytes.len() && bytes[offset].is_ascii_whitespace() {
                offset += 1;
            }
            start = offset;
        } else {
            offset += 1;
        }
    }
    let _ = writer.write_all(&bytes[start..]);
    writer.finish()
}

fn render_json_handle(json: &GosJson, pretty: bool) -> *mut c_char {
    if !pretty
        && json.view.is_null()
        && let Some(text) = json.tree.raw_text()
    {
        return render_json_raw(text);
    }
    render_json_direct(json.value(), pretty)
}

/// `json::render(value) -> String`. Always returns a non-null
/// C-string (empty on null input) into the GC arena.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_render(j: *const GosJson) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let Some(json) = (unsafe { json_handle(j) }) else {
            return alloc_cstring(b"");
        };
        render_json_handle(json, false)
    })
}

/// `json::encode_pretty(value) -> String`. Two-space indented form of
/// `gos_rt_json_render`; the same HTML-safe escaping applies. Always
/// returns a non-null C-string (empty on null input) into the GC arena.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_render_pretty(j: *const GosJson) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let Some(json) = (unsafe { json_handle(j) }) else {
            return alloc_cstring(b"");
        };
        render_json_handle(json, true)
    })
}

/// Display form of a `json::Value` for `println!("{}", val)`.
/// Strings are shown without JSON quotes; all other values use
/// their JSON representation so they stay machine-readable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_display(j: *const GosJson) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let Some(v) = (unsafe { json_borrow(j) }) else {
            return alloc_cstring(b"null");
        };
        match v {
            serde_json::Value::String(s) => alloc_cstring(s.as_bytes()),
            other => render_json_direct(other, false),
        }
    })
}

/// `value.get(key) -> json::Value`. Returns a fresh `GosJson*`
/// holding the field's value, or a JSON-null node when the
/// receiver is not an object or the field is missing. Nested
/// chains (`root.latency.low_ms`) work because each call returns
/// a real handle the next call can dereference. The child handle
/// shares the parent's `Arc<Value>` tree (no deep clone).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_get(j: *const GosJson, key: *const c_char) -> *mut GosJson {
    ffi_entry!(std::ptr::null_mut(), {
        let Some(parent) = (unsafe { json_handle(j) }) else {
            return GosJson::null_ptr();
        };
        // SAFETY: `parent.view` is a stable interior pointer into
        // `parent.tree`'s allocation; see `GosJson` doc. The
        // dereference produces a borrow that lives only inside this
        // function call.
        let v = parent.value();
        let key_bytes: &[u8] = if key.is_null() {
            b""
        } else {
            unsafe { crate::c_abi::gos_str_arg_bytes(key) }
        };
        let Ok(key_str) = std::str::from_utf8(key_bytes) else {
            return GosJson::null_ptr();
        };
        match v.get(key_str) {
            Some(child) => parent.child(child),
            None => GosJson::null_ptr(),
        }
    })
}

/// `value.at(idx) -> json::Value`. Sub-array index; child handle
/// shares the parent's tree.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_at(j: *const GosJson, idx: i64) -> *mut GosJson {
    ffi_entry!(std::ptr::null_mut(), {
        let Some(parent) = (unsafe { json_handle(j) }) else {
            return GosJson::null_ptr();
        };
        if idx < 0 {
            return GosJson::null_ptr();
        }
        let v = parent.value();
        match v.get(idx as usize) {
            Some(child) => parent.child(child),
            None => GosJson::null_ptr(),
        }
    })
}

/// `value.len() -> i64` for arrays and objects; 0 elsewhere.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_len(j: *const GosJson) -> i64 {
    ffi_entry!(-1, {
        let Some(v) = (unsafe { json_borrow(j) }) else {
            return 0;
        };
        match v {
            serde_json::Value::Array(a) => a.len() as i64,
            serde_json::Value::Object(o) => o.len() as i64,
            serde_json::Value::String(s) => s.len() as i64,
            _ => 0,
        }
    })
}

/// `value.is_null() -> bool` (returns 1/0 i32, the codegen ABI).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_is_null(j: *const GosJson) -> i32 {
    ffi_entry!(-1, {
        match unsafe { json_borrow(j) } {
            Some(serde_json::Value::Null) | None => 1,
            Some(_) => 0,
        }
    })
}

/// `value.as_i64() -> i64`. JSON numbers convert; everything else
/// returns 0 (matches the interpreter's `unwrap_or(0)` shape).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_as_i64(j: *const GosJson) -> i64 {
    ffi_entry!(-1, {
        let Some(v) = (unsafe { json_borrow(j) }) else {
            return 0;
        };
        match v {
            serde_json::Value::Number(n) => n
                .as_i64()
                .unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64),
            serde_json::Value::Bool(b) => i64::from(*b),
            serde_json::Value::String(s) => s.parse::<i64>().unwrap_or(0),
            _ => 0,
        }
    })
}

/// `value.as_f64() -> f64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_as_f64(j: *const GosJson) -> f64 {
    ffi_entry!(f64::NAN, {
        let Some(v) = (unsafe { json_borrow(j) }) else {
            return 0.0;
        };
        match v {
            serde_json::Value::Number(n) => n.as_f64().unwrap_or(0.0),
            serde_json::Value::Bool(true) => 1.0,
            serde_json::Value::Bool(false) => 0.0,
            serde_json::Value::String(s) => s.parse::<f64>().unwrap_or(0.0),
            _ => 0.0,
        }
    })
}

/// `value.as_str() -> String`. Strings round-trip; non-string
/// values render through serde_json::to_string so users can still
/// log them.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_as_str(j: *const GosJson) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let Some(v) = (unsafe { json_borrow(j) }) else {
            return alloc_cstring(b"");
        };
        match v {
            serde_json::Value::String(s) => alloc_cstring(s.as_bytes()),
            other => {
                let rendered = serde_json::to_string(other).unwrap_or_default();
                alloc_cstring(rendered.as_bytes())
            }
        }
    })
}

/// `value.as_i64() -> Option<i64>` - strict: `Some` only for a JSON
/// integer (or integer-valued number), `None` otherwise. This is the
/// shape the auto-derived `from_json` relies on (`match json::as_i64(x)
/// { Some(v) => v, None => Err }`); a coercing i64 return made every
/// non-integer field silently parse, so type validation never failed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_as_i64_opt(j: *const GosJson) -> i128 {
    ffi_entry!(0i128, {
        match unsafe { json_borrow(j) } {
            Some(serde_json::Value::Number(n)) => {
                if let Some(i) = n.as_i64() {
                    unsafe { gos_rt_result_new(0, i) }
                } else if let Some(f) = n.as_f64() {
                    if f.fract() == 0.0 && f >= i64::MIN as f64 && f <= i64::MAX as f64 {
                        unsafe { gos_rt_result_new(0, f as i64) }
                    } else {
                        unsafe { gos_rt_result_new(1, 0) }
                    }
                } else {
                    unsafe { gos_rt_result_new(1, 0) }
                }
            }
            _ => unsafe { gos_rt_result_new(1, 0) },
        }
    })
}

/// `value.as_f64() -> Option<f64>` - `Some` for any JSON number,
/// `None` otherwise.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_as_f64_opt(j: *const GosJson) -> i128 {
    ffi_entry!(0i128, {
        match unsafe { json_borrow(j) } {
            Some(serde_json::Value::Number(n)) => unsafe {
                gos_rt_result_new_f64(0, n.as_f64().unwrap_or(0.0))
            },
            _ => unsafe { gos_rt_result_new(1, 0) },
        }
    })
}

/// `value.as_str() -> Option<String>` - `Some` only for a JSON
/// string, `None` otherwise.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_as_str_opt(j: *const GosJson) -> i128 {
    ffi_entry!(0i128, {
        match unsafe { json_borrow(j) } {
            Some(serde_json::Value::String(s)) => {
                let cs = alloc_cstring(s.as_bytes());
                unsafe { gos_rt_result_new(0, cs as i64) }
            }
            _ => unsafe { gos_rt_result_new(1, 0) },
        }
    })
}

/// `value.as_bool() -> bool`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_as_bool(j: *const GosJson) -> i32 {
    ffi_entry!(-1, {
        match unsafe { json_borrow(j) } {
            Some(serde_json::Value::Bool(true)) => 1,
            Some(serde_json::Value::Number(n)) if n.as_f64().unwrap_or(0.0) != 0.0 => 1,
            Some(serde_json::Value::String(s)) if !s.is_empty() => 1,
            _ => 0,
        }
    })
}

/// `json::as_bool(value) -> Option<bool>` - `Some(b)` only when the
/// value is a JSON boolean, else `None`. Result-shaped (disc 0 =
/// Some, disc 1 = None) to match the bytecode VM's `Option<bool>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_as_bool_opt(j: *const GosJson) -> i128 {
    ffi_entry!(0i128, {
        match unsafe { json_borrow(j) } {
            Some(serde_json::Value::Bool(b)) => unsafe { gos_rt_result_new(0, i64::from(*b)) },
            _ => unsafe { gos_rt_result_new(1, 0) },
        }
    })
}

/// Identity helper for `json::as_array` / similar type
/// assertions - the runtime doesn't keep separate array vs
/// object handles, so the as_* coercions just thread the
/// receiver through unchanged. Lets MIR lowering route these
/// names without special-casing them at the call site.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_identity(j: *mut GosJson) -> *mut GosJson {
    ffi_entry!(std::ptr::null_mut(), { j })
}

/// `json::get(value, key) -> Option<json::Value>`. Wraps
/// `gos_rt_json_get`'s null-on-miss result in the standard
/// `*mut GosResult` Option shape (`disc 0 = Some, disc 1 = None`)
/// so user-level `match` / `if let` / `is_some` reads the right
/// discriminant. The bare `gos_rt_json_get` survives for the MIR
/// field-access lowering of `root.a.b.c`, which threads raw
/// `*mut GosJson` pointers through chained calls.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_get_opt(j: *const GosJson, key: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        let Some(parent) = (unsafe { json_handle(j) }) else {
            return gos_rt_result_new(1, 0);
        };
        let key_bytes: &[u8] = if key.is_null() {
            b""
        } else {
            unsafe { crate::c_abi::gos_str_arg_bytes(key) }
        };
        let Ok(key_str) = std::str::from_utf8(key_bytes) else {
            return gos_rt_result_new(1, 0);
        };
        let v = parent.value();
        match v.get(key_str) {
            Some(child) => gos_rt_result_new(0, parent.child(child) as i64),
            None => gos_rt_result_new(1, 0),
        }
    })
}

/// `json::keys(value) -> Option<[String]>`. Returns `Some(vec)`
/// for objects (keys in declaration order), `None` for any other
/// shape - pinned by `malformed_json_returns_none_not_segfault`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_keys_opt(j: *const GosJson) -> i128 {
    ffi_entry!(0i128, {
        let Some(v) = (unsafe { json_borrow(j) }) else {
            return unsafe { gos_rt_result_new(1, 0) };
        };
        match v {
            serde_json::Value::Object(map) => {
                // STRING-typed 8-byte slots (cstring pointers): the vec
                // owns each fresh key string, so `gos_rt_vec_free`
                // reclaims them even when a consumer loop breaks early.
                // `serde_json::Map::len` is exact, so build the runtime
                // vector at its final capacity. This avoids repeated copies
                // of key pointers and extra arena/global allocations for
                // object-heavy JSON responses.
                let vec_ptr = unsafe {
                    crate::c_abi::vec::gos_rt_vec_with_capacity_typed(
                        8,
                        map.len().min(i64::MAX as usize) as i64,
                        crate::c_abi::vec::vec_elem_kind::STRING,
                    )
                };
                for k in map.keys() {
                    let cs = alloc_cstring(k.as_bytes()) as i64;
                    unsafe {
                        gos_rt_vec_push(vec_ptr, std::ptr::addr_of!(cs).cast::<u8>());
                    }
                }
                unsafe { gos_rt_result_new(0, vec_ptr as i64) }
            }
            _ => unsafe { gos_rt_result_new(1, 0) },
        }
    })
}

/// `json::as_array(value) -> Option<[json::Value]>`. Returns
/// `Some(vec)` of element-pointers for an array node, `None`
/// otherwise. Each element is materialised as a fresh `GosJson*`
/// so the receiver can be dropped independently.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_as_array_opt(j: *const GosJson) -> i128 {
    ffi_entry!(0i128, {
        let Some(parent) = (unsafe { json_handle(j) }) else {
            return gos_rt_result_new(1, 0);
        };
        let v = parent.value();
        match v {
            serde_json::Value::Array(items) => {
                // Each returned child handle needs one pointer slot. Reserve
                // once from the source array's exact length instead of
                // growing through every capacity tier.
                let vec_ptr = unsafe {
                    crate::c_abi::vec::gos_rt_vec_with_capacity_typed(
                        8,
                        items.len().min(i64::MAX as usize) as i64,
                        crate::c_abi::vec::vec_elem_kind::JSON,
                    )
                };
                for item in items {
                    // Each element shares the parent's `Arc<Value>`
                    // tree - no deep clone, no per-element leak of a
                    // freshly-boxed Value.
                    let elem = parent.child(item) as i64;
                    unsafe {
                        gos_rt_vec_push(vec_ptr, std::ptr::addr_of!(elem).cast::<u8>());
                    }
                }
                gos_rt_result_new(0, vec_ptr as i64)
            }
            _ => unsafe { gos_rt_result_new(1, 0) },
        }
    })
}

/// `json::Value::String(s)` constructor.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_value_string(s: *const c_char) -> *mut GosJson {
    ffi_entry!(std::ptr::null_mut(), {
        let text = if s.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(s) }
        };
        GosJson::into_raw(serde_json::Value::String(text))
    })
}

/// `json::Value::Int(n)` constructor.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_value_int(n: i64) -> *mut GosJson {
    ffi_entry!(std::ptr::null_mut(), {
        GosJson::into_raw(serde_json::Value::Number(n.into()))
    })
}

/// `json::Value::Bool(b)` constructor.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_value_bool(b: i32) -> *mut GosJson {
    ffi_entry!(std::ptr::null_mut(), {
        GosJson::into_raw(serde_json::Value::Bool(b != 0))
    })
}

/// `json::Value::Float(x)` constructor used by `json::render` on
/// struct fields of type `f64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_value_float(x: f64) -> *mut GosJson {
    ffi_entry!(std::ptr::null_mut(), {
        let n = serde_json::Number::from_f64(x).unwrap_or_else(|| serde_json::Number::from(0));
        GosJson::into_raw(serde_json::Value::Number(n))
    })
}

/// `json::Value::Null` constructor.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_value_null() -> *mut GosJson {
    ffi_entry!(std::ptr::null_mut(), { GosJson::null_ptr() })
}

/// `json::Value::Array(vec)` constructor. Takes a `*mut GosVec` of
/// `*mut GosJson` element pointers and rebuilds a real
/// `serde_json::Value::Array`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_value_array(vec: *const GosVec) -> *mut GosJson {
    ffi_entry!(std::ptr::null_mut(), {
        let mut out: Vec<serde_json::Value> = Vec::new();
        if !vec.is_null() {
            let header = unsafe { &*vec };
            let len = usize::try_from(header.len.max(0)).unwrap_or(0);
            if !header.ptr.is_null() && len > 0 {
                out.reserve(len);
                let base = header.ptr;
                for i in 0..len {
                    // Slots hold child pointers exposed as integers by the
                    // flat-slot ABI in an unaligned byte buffer; read
                    // unaligned and recover provenance.
                    let addr = unsafe { base.add(i * 8).cast::<usize>().read_unaligned() };
                    let elem: *const GosJson = std::ptr::with_exposed_provenance(addr);
                    if let Some(v) = unsafe { json_borrow(elem) } {
                        out.push(v.clone());
                    } else {
                        out.push(serde_json::Value::Null);
                    }
                }
            }
        }
        GosJson::into_raw(serde_json::Value::Array(out))
    })
}

/// Builds a `json::Value::Array` from a Gossamer `Vec` of scalar
/// elements. `kind` selects how each 8-byte slot is read:
/// 0 = i64, 1 = f64 (bit pattern), 2 = String (`*const c_char`),
/// 3 = bool. Used by `json::encode([…])` on a scalar array, where
/// the MIR has a typed scalar `*GosVec` rather than a Vec of
/// pre-boxed `*GosJson` pointers (the shape `gos_rt_json_value_array`
/// expects).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_array_from_scalar_vec(
    vec: *const GosVec,
    kind: i64,
) -> *mut GosJson {
    ffi_entry!(std::ptr::null_mut(), {
        let mut out: Vec<serde_json::Value> = Vec::new();
        if !vec.is_null() {
            let header = unsafe { &*vec };
            let len = usize::try_from(header.len.max(0)).unwrap_or(0);
            if !header.ptr.is_null() && len > 0 {
                out.reserve(len);
                let words = unsafe { std::slice::from_raw_parts(header.ptr.cast::<i64>(), len) };
                for &w in words {
                    let v = match kind {
                        1 => serde_json::Number::from_f64(f64::from_bits(w as u64))
                            .map_or(serde_json::Value::Null, serde_json::Value::Number),
                        2 => {
                            let p = w as *const c_char;
                            if p.is_null() {
                                serde_json::Value::String(String::new())
                            } else {
                                serde_json::Value::String(unsafe {
                                    crate::c_abi::gos_str_arg_string(p)
                                })
                            }
                        }
                        3 => serde_json::Value::Bool(w != 0),
                        _ => serde_json::Value::Number(w.into()),
                    };
                    out.push(v);
                }
            }
        }
        GosJson::into_raw(serde_json::Value::Array(out))
    })
}

/// `json::Value::object(n, pairs_ptr)` - fan-out constructor
/// that takes the pair count and a flat `[k0, v0, k1, v1, …]`
/// arena buffer. Lets the MIR lowerer materialise an array
/// literal of `(String, json::Value)` pairs into a 16-B-strided
/// buffer without going through `gos_rt_vec_push` (which
/// truncates at 8 bytes today). The legacy
/// `gos_rt_json_value_object(*mut GosVec)` survives for runner
/// builds that still pass a real `GosVec`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_value_object_n(n: i64, pairs: *const i64) -> *mut GosJson {
    ffi_entry!(std::ptr::null_mut(), {
        let mut out = serde_json::Map::new();
        let n = usize::try_from(n.max(0)).unwrap_or(0);
        if !pairs.is_null() && n > 0 {
            let slice = unsafe { std::slice::from_raw_parts(pairs, n * 2) };
            for chunk in slice.chunks_exact(2) {
                let key_ptr = chunk[0] as *const c_char;
                let val_ptr = chunk[1] as *mut GosJson;
                let key = if key_ptr.is_null() {
                    String::new()
                } else {
                    unsafe { crate::c_abi::gos_str_arg_string(key_ptr) }
                };
                let v = if let Some(v) = unsafe { json_borrow(val_ptr) } {
                    v.clone()
                } else {
                    serde_json::Value::Null
                };
                out.insert(key, v);
            }
        }
        GosJson::into_raw(serde_json::Value::Object(out))
    })
}

/// `json::Value::object([(k, v), ...])` constructor. Takes a
/// `*mut GosVec` of `(String, *mut GosJson)` tuple pointers.
/// Used by the runner-build path; the compiled tier prefers
/// `gos_rt_json_value_object_n` to dodge `*mut GosVec` plumbing
/// for the array-literal-of-pairs shape.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_value_object(vec: *const GosVec) -> *mut GosJson {
    ffi_entry!(std::ptr::null_mut(), {
        let mut out = serde_json::Map::new();
        if !vec.is_null() {
            let header = unsafe { &*vec };
            let raw_len = usize::try_from(header.len.max(0)).unwrap_or(0);
            let elem_bytes = header.elem_bytes as usize;
            // The compiled tier passes raw stack-arrays where the
            // call site expected a `*mut GosVec`; in that case the
            // first 8 bytes the runtime reads as `header.len` are
            // actually the first key's c_char pointer (huge value),
            // and following the bogus length crashes on the next
            // strlen. Bail early when the header doesn't look like
            // a GosVec we built (`elem_bytes` is one of the small
            // shapes we hand out, the length is plausible).
            let header_looks_valid =
                matches!(elem_bytes, 8 | 16 | 24) && raw_len <= 16 * 1024 * 1024;
            if header_looks_valid && !header.ptr.is_null() && raw_len > 0 {
                // Tuples in the compiled tier currently get pushed as
                // flat 8-byte slots - `[("k", v), ("k2", v2)]` lands
                // as `len = 4` of i64 slots, not `len = 2` of 16-byte
                // pairs. Detect this by `elem_bytes`: if it's 8, treat
                // `len` as half the tuple count and stride 8; if it's
                // 16, treat `len` as the tuple count and stride 16.
                let tuple_count = if elem_bytes == 16 {
                    raw_len
                } else {
                    raw_len / 2
                };
                let pairs = unsafe {
                    std::slice::from_raw_parts(header.ptr.cast::<[i64; 2]>(), tuple_count)
                };
                for pair in pairs {
                    let key_ptr = pair[0] as *const c_char;
                    let val_ptr = pair[1] as *mut GosJson;
                    let key = if key_ptr.is_null() {
                        String::new()
                    } else {
                        unsafe { crate::c_abi::gos_str_arg_string(key_ptr) }
                    };
                    let v = if let Some(v) = unsafe { json_borrow(val_ptr) } {
                        v.clone()
                    } else {
                        serde_json::Value::Null
                    };
                    out.insert(key, v);
                }
            }
        }
        GosJson::into_raw(serde_json::Value::Object(out))
    })
}

/// `json::set(obj, key, val) -> json::Value`. Returns a new JSON
/// object with `key` updated to `val`. Appends when the key is new.
/// If `obj` is not an object, returns `obj` unchanged.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_set(
    obj: *const GosJson,
    key: *const c_char,
    val: *const GosJson,
) -> *mut GosJson {
    ffi_entry!(std::ptr::null_mut(), {
        let Some(parent) = (unsafe { json_handle(obj) }) else {
            return GosJson::null_ptr();
        };
        let v = parent.value();
        let serde_json::Value::Object(existing) = v else {
            return parent.child(v);
        };
        let key_str = if key.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(key) }
        };
        let new_val = if let Some(child) = unsafe { json_borrow(val) } {
            child.clone()
        } else {
            serde_json::Value::Null
        };
        let mut out = existing.clone();
        out.insert(key_str, new_val);
        GosJson::into_raw(serde_json::Value::Object(out))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    /// Refcount word of an `alloc_cstring` builder-layout string:
    /// `[rc:u32][cap:u32][len:u32][tag][content][NUL]`, body at +13.
    unsafe fn str_rc(s: *const c_char) -> u32 {
        let hdr = unsafe { s.cast::<u8>().sub(13) };
        u32::from_le_bytes(unsafe { [*hdr, *hdr.add(1), *hdr.add(2), *hdr.add(3)] })
    }

    #[test]
    fn json_keys_vec_is_string_typed_and_deep_frees_unvisited_keys() {
        let text = std::ffi::CString::new(r#"{"alpha":1,"beta":2}"#).unwrap();
        let pr = unsafe { gos_rt_json_parse(crate::c_abi::string::test_gos_ptr(&text)) };
        assert_eq!(crate::c_abi::vec::gos_rt_result_disc(pr), 0);
        let j = crate::c_abi::vec::gos_rt_result_payload(pr) as *mut GosJson;
        let kr = unsafe { gos_rt_json_keys_opt(j) };
        assert_eq!(crate::c_abi::vec::gos_rt_result_disc(kr), 0);
        let v = crate::c_abi::vec::gos_rt_result_payload(kr) as *mut crate::c_abi::vec::GosVec;
        let vec = unsafe { &*v };
        assert_eq!(vec.len, 2);
        assert_eq!(vec.elem_kind, crate::c_abi::vec::vec_elem_kind::STRING);
        // Probe-share key 0, free the vec WITHOUT iterating (the
        // early-break consumer shape): deep-free must release exactly
        // the vec's share - rc 2 -> 1, not 2 (leak), not 0 (double free).
        let k0 = unsafe {
            std::ptr::with_exposed_provenance_mut::<c_char>(
                (vec.ptr.as_ptr() as *const usize).read_unaligned(),
            )
        };
        unsafe { crate::c_abi::string::gos_rt_str_retain(k0) };
        assert_eq!(unsafe { str_rc(k0) }, 2);
        unsafe { crate::c_abi::map::gos_rt_vec_free(v) };
        assert_eq!(
            unsafe { str_rc(k0) },
            1,
            "deep-free must release the vec's share once"
        );
        assert_eq!(unsafe { CStr::from_ptr(k0) }.to_str().unwrap(), "alpha");
        unsafe { crate::c_abi::string::gos_rt_str_free(k0) };
        unsafe { gos_rt_json_free(j) };
    }

    #[test]
    fn json_input_limits_match_vm_defaults_and_ignore_string_brackets() {
        assert!(checked_json_text(br#"{"brackets":"[{]}"}"#).is_ok());
        let at_limit = format!(
            "{}0{}",
            "[".repeat(JSON_MAX_DEPTH),
            "]".repeat(JSON_MAX_DEPTH)
        );
        let at_limit = std::ffi::CString::new(at_limit).unwrap();
        let parsed = unsafe { gos_rt_json_parse(crate::c_abi::string::test_gos_ptr(&at_limit)) };
        assert_eq!(crate::c_abi::vec::gos_rt_result_disc(parsed), 0);
        unsafe {
            gos_rt_json_free(crate::c_abi::vec::gos_rt_result_payload(parsed) as *mut GosJson);
        }
        let nested = format!(
            "{}0{}",
            "[".repeat(JSON_MAX_DEPTH + 1),
            "]".repeat(JSON_MAX_DEPTH + 1)
        );
        assert_eq!(
            checked_json_text(nested.as_bytes()),
            Err("nesting depth exceeds max_depth (128)")
        );
        let large = vec![b' '; JSON_MAX_SIZE + 1];
        assert_eq!(
            checked_json_text(&large),
            Err("input exceeds max_size (16 MiB)")
        );
    }

    #[test]
    fn json_render_preserves_parsed_number_spelling() {
        let text = std::ffi::CString::new(r#"{"score":12.100000000000001,"short":20.9}"#).unwrap();
        let parsed = unsafe { gos_rt_json_parse(crate::c_abi::string::test_gos_ptr(&text)) };
        assert_eq!(crate::c_abi::vec::gos_rt_result_disc(parsed), 0);
        let json = crate::c_abi::vec::gos_rt_result_payload(parsed) as *mut GosJson;
        let rendered_ptr = unsafe { gos_rt_json_render(json) };
        let rendered = unsafe { CStr::from_ptr(rendered_ptr) }.to_str().unwrap();
        assert!(
            rendered.contains("\"score\":12.100000000000001"),
            "rendered JSON must retain the original numeric spelling: {rendered}"
        );
        assert!(rendered.contains("\"short\":20.9"));
        unsafe { crate::c_abi::string::gos_rt_str_free(rendered_ptr) };
        unsafe { gos_rt_json_free(json) };
    }

    #[test]
    fn untouched_json_renders_without_materialising_the_dom() {
        let text = std::ffi::CString::new(" { \"value\" : 7 } ").unwrap();
        let parsed = unsafe { gos_rt_json_parse(crate::c_abi::string::test_gos_ptr(&text)) };
        let json = crate::c_abi::vec::gos_rt_result_payload(parsed) as *mut GosJson;
        let handle = unsafe { &*json };
        assert!(matches!(
            &*handle.tree,
            JsonTree::Raw { parsed, .. } if parsed.get().is_none()
        ));
        let rendered_ptr = unsafe { gos_rt_json_render(json) };
        assert_eq!(
            unsafe { CStr::from_ptr(rendered_ptr) }.to_bytes(),
            br#"{"value":7}"#
        );
        assert!(matches!(
            &*handle.tree,
            JsonTree::Raw { parsed, .. } if parsed.get().is_none()
        ));
        let key = c"value";
        let child = unsafe { gos_rt_json_get(json, crate::c_abi::string::test_gos_ptr(key)) };
        assert_eq!(unsafe { gos_rt_json_as_i64(child) }, 7);
        assert!(matches!(
            &*handle.tree,
            JsonTree::Raw { parsed, .. } if parsed.get().is_some()
        ));
        unsafe {
            gos_rt_str_free(rendered_ptr);
            gos_rt_json_free(child);
            gos_rt_json_free(json);
        }
    }

    #[test]
    fn direct_json_render_keeps_html_safe_escaping() {
        let text = std::ffi::CString::new(r#"{"x":"<>&\u2028\u2029"}"#).unwrap();
        let parsed = unsafe { gos_rt_json_parse(crate::c_abi::string::test_gos_ptr(&text)) };
        let json = crate::c_abi::vec::gos_rt_result_payload(parsed) as *mut GosJson;
        let rendered_ptr = unsafe { gos_rt_json_render(json) };
        let rendered = unsafe { CStr::from_ptr(rendered_ptr) }.to_str().unwrap();
        assert_eq!(rendered, r#"{"x":"\u003c\u003e\u0026\u2028\u2029"}"#);
        unsafe { crate::c_abi::string::gos_rt_str_free(rendered_ptr) };
        unsafe { gos_rt_json_free(json) };
    }

    #[test]
    fn json_collection_projections_reserve_the_source_length() {
        let text = std::ffi::CString::new(
            r#"{"k0":0,"k1":1,"k2":2,"k3":3,"k4":4,"k5":5,"k6":6,"k7":7,"k8":8}"#,
        )
        .unwrap();
        let parsed = unsafe { gos_rt_json_parse(crate::c_abi::string::test_gos_ptr(&text)) };
        let json = crate::c_abi::vec::gos_rt_result_payload(parsed) as *mut GosJson;

        let keys = unsafe { gos_rt_json_keys_opt(json) };
        let keys = crate::c_abi::vec::gos_rt_result_payload(keys) as *mut GosVec;
        assert_eq!(unsafe { (*keys).len }, 9);
        assert!(unsafe { (*keys).cap } >= 9);

        let array_text = std::ffi::CString::new("[0,1,2,3,4,5,6,7,8]").unwrap();
        let array_result =
            unsafe { gos_rt_json_parse(crate::c_abi::string::test_gos_ptr(&array_text)) };
        let array = crate::c_abi::vec::gos_rt_result_payload(array_result) as *mut GosJson;
        let items = unsafe { gos_rt_json_as_array_opt(array) };
        let items = crate::c_abi::vec::gos_rt_result_payload(items) as *mut GosVec;
        assert_eq!(unsafe { (*items).len }, 9);
        assert!(unsafe { (*items).cap } >= 9);

        unsafe { crate::c_abi::map::gos_rt_vec_free(keys) };
        unsafe { crate::c_abi::map::gos_rt_vec_free(items) };
        unsafe { gos_rt_json_free(json) };
        unsafe { gos_rt_json_free(array) };
    }
}
