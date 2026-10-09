#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
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
#![allow(clippy::wildcard_imports)]

use std::os::raw::c_char;

use serde::Deserialize;

use super::*;

/// Materializes a document [`gossamer_core::json::validate`] accepted, whose
/// depth it has already bounded. `serde_json`'s own recursion counter rejects
/// the valid document at depth 128 one level early, so that duplicate guard
/// is disabled.
fn parse_checked_json(text: &str) -> Result<serde_json::Value, serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    deserializer.disable_recursion_limit();
    let mut value = serde_json::Value::deserialize(&mut deserializer)?;
    deserializer.end()?;
    narrow_numbers_to_language_range(&mut value);
    Ok(value)
}

/// Rewrites every integer outside the `i64` and `u64` ranges as the `f64`
/// nearest it.
///
/// An integer a program cannot name is one it cannot read back: neither
/// `as_i64` nor `as_u64` answers it and `as_f64` answers the approximation,
/// so holding the exact value would let a document render digits no accessor
/// agrees with. The bytecode VM's parser resolves these to `f64` for the same
/// reason, and keeps a value `as_u64` reads, as this does, so every tier's
/// rendering and accessors are identical.
fn narrow_numbers_to_language_range(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Number(n) => {
            if n.as_i64().is_none()
                && n.as_u64().is_none()
                && let Some(as_float) = n.as_f64()
                && let Some(narrowed) = serde_json::Number::from_f64(as_float)
            {
                *n = narrowed;
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                narrow_numbers_to_language_range(item);
            }
        }
        serde_json::Value::Object(entries) => {
            for (_, entry) in entries.iter_mut() {
                narrow_numbers_to_language_range(entry);
            }
        }
        _ => {}
    }
}

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

    /// The validated text of a document no read has materialized yet.
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
        json_box(GosJson {
            tree,
            view: SyncRawPtr::new(view.cast_mut()),
        })
    }

    fn raw(text: &str) -> *mut GosJson {
        json_box(GosJson {
            tree: std::sync::Arc::new(JsonTree::Raw {
                text: text.into(),
                parsed: std::sync::OnceLock::new(),
            }),
            view: SyncRawPtr::NULL,
        })
    }

    /// Builds a child handle that shares the same tree as `self`
    /// and points at `child` inside it. `child` must be a
    /// reference into `self.tree`'s subtree (the type system
    /// cannot enforce this here because we cross the FFI; every
    /// caller below derives `child` via `serde_json::Value::get`
    /// on `self.view`'s subtree, which is sound).
    fn child(&self, child: &serde_json::Value) -> *mut GosJson {
        json_box(GosJson {
            tree: std::sync::Arc::clone(&self.tree),
            view: SyncRawPtr::new(std::ptr::from_ref(child).cast_mut()),
        })
    }

    fn value(&self) -> &serde_json::Value {
        if self.view.is_null() {
            self.tree.value()
        } else {
            // SAFETY: a non-null `view` points into the subtree of `tree`, which this handle
            // keeps alive.
            unsafe { &*self.view.as_const_ptr() }
        }
    }

    fn null_ptr() -> *mut GosJson {
        Self::into_raw(serde_json::Value::Null)
    }
}

/// Boxes a JSON handle, or places it in the innermost open region, which
/// drops it at its pop.
fn json_box(handle: GosJson) -> *mut GosJson {
    crate::c_abi::rc::region_alloc_handle(handle, finalize_region_json)
        .unwrap_or_else(|handle| Box::into_raw(Box::new(handle)))
}

/// Drops a JSON handle its region placed, giving back its share of the tree.
///
/// # Safety
///
/// `p` is a handle `json_box` placed in a region, finalized once, at its pop.
unsafe fn finalize_region_json(p: *mut u8) {
    // SAFETY: the handle is dropped once, in place; its region reclaims the storage.
    unsafe { std::ptr::drop_in_place(p.cast::<GosJson>()) };
}

pub(crate) unsafe fn json_borrow<'a>(p: *const GosJson) -> Option<&'a serde_json::Value> {
    if p.is_null() {
        return None;
    }
    // SAFETY: `p` is non-null (checked above), and this `unsafe fn`'s caller passes a live JSON
    // value.
    let json = unsafe { &*p };
    // `view` was set by `Self::into_raw` (points at the
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
    // SAFETY: this `unsafe fn`'s caller passes `p` live or null, which `json_borrow` accepts.
    unsafe { json_borrow(p) }
}

/// Resolves `p` and returns the GosJson struct itself so the
/// caller can construct child handles via `Self::child`. Returns
/// `None` only for null inputs.
unsafe fn json_handle<'a>(p: *const GosJson) -> Option<&'a GosJson> {
    if p.is_null() {
        return None;
    }
    // SAFETY: `p` is non-null (checked above), and this `unsafe fn`'s caller passes a live JSON
    // value.
    Some(unsafe { &*p })
}

/// `json::parse(text) -> Result<json::Value, String>` runtime
/// `json::valid(text) -> bool` - true when `text` parses as
/// well-formed JSON. Mirrors the interp `json::valid` builtin.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_valid(text: *const c_char) -> i8 {
    ffi_entry!({
        let bytes: &[u8] = if text.is_null() {
            b""
        } else {
            // SAFETY: `text` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_bytes(text) }
        };
        i8::from(gossamer_core::json::validate(bytes).is_ok())
    })
}

/// entry point. Returns a real `GosResult` so `match` and `?`
/// work across function boundaries in compiled code.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_parse(text: *const c_char) -> i128 {
    ffi_entry!({
        let bytes: &[u8] = if text.is_null() {
            b""
        } else {
            // SAFETY: `text` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_bytes(text) }
        };
        // The same validator the VM's parser is, so every tier accepts,
        // rejects, and describes a document alike.
        match gossamer_core::json::validate(bytes) {
            Ok(s) => {
                let ptr = GosJson::raw(s);
                gos_rt_result_new(0, ptr as i64)
            }
            Err(error) => {
                let message = error.to_string();
                let err = crate::c_abi::errors::error_new_from_bytes(message.as_bytes());
                gos_rt_result_new(1, err as i64)
            }
        }
    })
}

/// Frees a `GosJson` handle (drops its `Arc` share of the parsed
/// tree; the tree itself dies with its last handle). Null-safe.
/// Emitted by the drop pass for provably single-owner JSON locals.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_free(j: *mut GosJson) {
    // A handle in region storage is dropped by its region's pop.
    if j.is_null() || crate::c_abi::rc::in_region_arena(j.cast()) {
        return;
    }
    // SAFETY: `j` is non-null (checked above), a handle a constructor boxed, which this call
    // consumes (C-ABI contract).
    drop(unsafe { Box::from_raw(j) });
}

/// Takes the value a builder box holds, consuming the box.
///
/// A box the encoder just built owns its whole tree, so the value moves out
/// with no copy. A handle that shares a parsed document, or one viewing a
/// subtree, answers a copy of what it views - the document stays whole.
///
/// # Safety
/// `p` is null or a builder box the caller hands over.
unsafe fn take_json_value(p: *mut GosJson) -> serde_json::Value {
    if p.is_null() {
        return serde_json::Value::Null;
    }
    if crate::c_abi::rc::in_region_arena(p.cast()) {
        // Its region drops the handle at pop; the value is read, not moved.
        // SAFETY: `p` is a live handle in region storage.
        return unsafe { &*p }.value().clone();
    }
    // SAFETY: `p` is non-null (checked above), a builder box this `unsafe fn`'s caller hands
    // over.
    let boxed = unsafe { Box::from_raw(p) };
    let views_root = boxed.view.is_null()
        || std::ptr::eq(
            boxed.view.as_const_ptr(),
            std::ptr::from_ref(boxed.tree.value()),
        );
    if !views_root {
        // SAFETY: a `view` that is not the root points into the subtree of `tree`, which `boxed`
        // keeps alive here.
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
    ffi_entry!({
        let mut out: Vec<serde_json::Value> = Vec::new();
        if !vec.is_null() {
            // SAFETY: `vec` is non-null (checked above) and live for the call (C-ABI contract).
            let header = unsafe { &*vec };
            let len = usize::try_from(header.len.max(0)).unwrap_or(0);
            if !header.ptr.is_null() && len > 0 {
                out.reserve(len);
                let base = header.ptr;
                for i in 0..len {
                    // SAFETY: `i` is below the vec's length, and a builder vector holds one
                    // 8-byte handle per element.
                    let elem = unsafe { crate::c_abi::vec::slot_read_word(base.add(i * 8)) }
                        .cast::<GosJson>();
                    // SAFETY: each element is a builder box the vector hands over with it (this
                    // shim's contract).
                    out.push(unsafe { take_json_value(elem) });
                }
            }
        }
        // SAFETY: `vec` is this shim's argument, consumed here with the boxes already taken, or
        // null, which `gos_rt_vec_free` accepts.
        unsafe { crate::c_abi::gos_rt_vec_free(vec) };
        GosJson::into_raw(serde_json::Value::Array(out))
    })
}

/// The name/value pairs a builder vector holds: a vector of `(name, value)`
/// tuples stores one pair per 16-byte element, and the flat vector the
/// encoder builds stores a name word and a value word in alternate 8-byte
/// elements.
fn object_pair_count(header: &GosVec) -> usize {
    let len = usize::try_from(header.len.max(0)).unwrap_or(0);
    if header.ptr.is_null() {
        return 0;
    }
    match header.elem_bytes {
        16 => len,
        8 => len / 2,
        _ => 0,
    }
}

/// `json::Value::Object` over a name/value builder vector whose value boxes it
/// consumes. The name slots are borrowed C strings; only the values move.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_value_object_owned(vec: *mut GosVec) -> *mut GosJson {
    // SAFETY: `vec` is this shim's argument, live for the call (C-ABI contract) or null, which
    // `gos_rt_json_value_object_owned_keyed` accepts.
    unsafe { gos_rt_json_value_object_owned_keyed(vec, 0) }
}

/// [`gos_rt_json_value_object_owned`] over pairs whose key words are of the
/// kind `object_member_name` names.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_value_object_owned_keyed(
    vec: *mut GosVec,
    key_kind: i64,
) -> *mut GosJson {
    ffi_entry!({
        let mut out = serde_json::Map::new();
        if !vec.is_null() {
            // SAFETY: `vec` is non-null (checked above) and live for the call (C-ABI contract).
            let header = unsafe { &*vec };
            let pair_count = object_pair_count(header);
            if pair_count > 0 {
                // SAFETY: `object_pair_count` answers a non-zero count only for a non-null
                // buffer holding that many name/value word pairs.
                let pairs = unsafe {
                    std::slice::from_raw_parts(header.ptr.cast::<[i64; 2]>(), pair_count)
                };
                for pair in pairs {
                    let val_ptr = pair[1] as *mut GosJson;
                    // SAFETY: a pair's name word is of the kind `key_kind` names (this shim's
                    // contract).
                    let key = unsafe { object_member_name(pair[0], key_kind) };
                    // SAFETY: each value word is a builder box the vector hands over with it
                    // (this shim's contract).
                    out.insert(key, unsafe { take_json_value(val_ptr) });
                }
            }
        }
        // SAFETY: `vec` is this shim's argument, consumed here with the boxes already taken, or
        // null, which `gos_rt_vec_free` accepts.
        unsafe { crate::c_abi::gos_rt_vec_free(vec) };
        GosJson::into_raw(serde_json::Value::Object(out))
    })
}

/// A second handle onto the same document, for a storage that duplicates a
/// slot holding one.
///
/// A handle carries no count of its own - it is a box over a shared tree - so
/// a copied slot takes a box of its own and the tree's `Arc` gains a holder.
pub(crate) unsafe fn json_clone_handle(p: *const GosJson) -> *mut GosJson {
    if p.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: `p` is non-null (checked above), and this `unsafe fn`'s caller passes a live JSON
    // value.
    let src = unsafe { &*p };
    json_box(GosJson {
        tree: std::sync::Arc::clone(&src.tree),
        view: SyncRawPtr::new(src.view.as_const_ptr().cast_mut()),
    })
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
    // SAFETY: `vec` is a handle from compiled code, checked non-null above and live for the whole call.
    let header = unsafe { &*vec };
    let len = usize::try_from(header.len.max(0)).unwrap_or(0);
    let first = usize::try_from(first.max(0)).unwrap_or(0);
    let stride = usize::try_from(stride.max(1)).unwrap_or(1);
    if !header.ptr.is_null() {
        let base = header.ptr;
        let mut i = first;
        while i < len {
            let child =
                // SAFETY: `i` is below the vec's length, and a builder vector holds one 8-byte
                // handle per element.
                unsafe { crate::c_abi::vec::slot_read_word(base.add(i * 8)) }.cast::<GosJson>();
            // SAFETY: each owned slot holds a handle the walk built, null or live, which
            // `gos_rt_json_free` accepts.
            unsafe { gos_rt_json_free(child) };
            i += stride;
        }
    }
    // SAFETY: `vec` is non-null (checked above), the builder vector this call consumes (C-ABI
    // contract).
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
            // SAFETY: `string` is this writer's own builder, `len` its length, and `new_len` fits
            // its capacity (checked above).
            unsafe { str_builder_write_reserved(self.string, self.len, bytes) };
        } else {
            // SAFETY: `string` is this writer's own builder, whose share the append consumes and
            // answers.
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

/// Offset of the first byte that may need a JSON escape sequence: one of the
/// three HTML-significant ASCII bytes, or the lead byte of U+2028 / U+2029.
#[inline]
fn first_escape_candidate(bytes: &[u8]) -> Option<usize> {
    bytes
        .iter()
        .position(|&b| matches!(b, b'<' | b'>' | b'&' | 0xe2))
}

impl std::io::Write for RuntimeJsonWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.write_escaped(bytes);
        Ok(bytes.len())
    }

    // Every write takes all of its bytes, so the default loop that retries a
    // short write has nothing to do; serde_json writes through this.
    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.write_escaped(bytes);
        Ok(())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl RuntimeJsonWriter {
    /// Appends `bytes`, escaping the characters JSON output keeps HTML-safe.
    fn write_escaped(&mut self, bytes: &[u8]) {
        write_html_safe(bytes, |part| self.append(part));
    }
}

/// Hands `bytes` to `append` in pieces, with the characters JSON output keeps
/// HTML-safe (`<`, `>`, `&`, U+2028, U+2029) replaced by their `\u` escapes.
fn write_html_safe(bytes: &[u8], mut append: impl FnMut(&[u8])) {
    let Some(first) = first_escape_candidate(bytes) else {
        append(bytes);
        return;
    };
    let mut start = 0;
    let mut offset = first;
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
            let rest = offset + source_len;
            offset = match first_escape_candidate(&bytes[rest..]) {
                Some(next) => rest + next,
                None => bytes.len(),
            };
            continue;
        };
        append(&bytes[start..offset]);
        append(replacement);
        offset += source_len;
        start = offset;
    }
    append(&bytes[start..]);
}

impl Drop for RuntimeJsonWriter {
    fn drop(&mut self) {
        if !self.string.is_null() {
            // SAFETY: `string` is the non-null builder this writer still owns.
            unsafe { gos_rt_str_free(self.string) };
        }
    }
}

/// Writes a JSON float the way the language writes the same value, so a
/// number inside a document reads as the one `{}` shows for it. Rust's
/// shortest-round-trip float writer would spell large and small magnitudes in
/// exponent form, which no other rendering in the language uses. A value with
/// no fractional part keeps a `.0` so it stays a float on the way back in, and
/// NaN or an infinity, which JSON cannot spell, is written `null`.
fn write_language_float<W: std::io::Write + ?Sized>(
    writer: &mut W,
    value: f64,
) -> std::io::Result<()> {
    if !value.is_finite() {
        return writer.write_all(b"null");
    }
    if value.fract() == 0.0 {
        write!(writer, "{value}.0")
    } else {
        write!(writer, "{value}")
    }
}

/// Compact JSON with the language's own float spelling. Every method but the
/// float writers is serde's compact default.
struct LanguageFloatsCompact;

impl serde_json::ser::Formatter for LanguageFloatsCompact {
    fn write_f32<W: std::io::Write + ?Sized>(
        &mut self,
        writer: &mut W,
        value: f32,
    ) -> std::io::Result<()> {
        write_language_float(writer, f64::from(value))
    }

    fn write_f64<W: std::io::Write + ?Sized>(
        &mut self,
        writer: &mut W,
        value: f64,
    ) -> std::io::Result<()> {
        write_language_float(writer, value)
    }
}

/// Indented JSON with the language's own float spelling: serde's pretty
/// layout for structure, [`write_language_float`] for numbers.
struct LanguageFloatsPretty<'a>(serde_json::ser::PrettyFormatter<'a>);

impl serde_json::ser::Formatter for LanguageFloatsPretty<'_> {
    fn write_f32<W: std::io::Write + ?Sized>(
        &mut self,
        writer: &mut W,
        value: f32,
    ) -> std::io::Result<()> {
        write_language_float(writer, f64::from(value))
    }

    fn write_f64<W: std::io::Write + ?Sized>(
        &mut self,
        writer: &mut W,
        value: f64,
    ) -> std::io::Result<()> {
        write_language_float(writer, value)
    }

    fn begin_array<W: std::io::Write + ?Sized>(&mut self, writer: &mut W) -> std::io::Result<()> {
        self.0.begin_array(writer)
    }

    fn end_array<W: std::io::Write + ?Sized>(&mut self, writer: &mut W) -> std::io::Result<()> {
        self.0.end_array(writer)
    }

    fn begin_array_value<W: std::io::Write + ?Sized>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> std::io::Result<()> {
        self.0.begin_array_value(writer, first)
    }

    fn end_array_value<W: std::io::Write + ?Sized>(
        &mut self,
        writer: &mut W,
    ) -> std::io::Result<()> {
        self.0.end_array_value(writer)
    }

    fn begin_object<W: std::io::Write + ?Sized>(&mut self, writer: &mut W) -> std::io::Result<()> {
        self.0.begin_object(writer)
    }

    fn end_object<W: std::io::Write + ?Sized>(&mut self, writer: &mut W) -> std::io::Result<()> {
        self.0.end_object(writer)
    }

    fn begin_object_key<W: std::io::Write + ?Sized>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> std::io::Result<()> {
        self.0.begin_object_key(writer, first)
    }

    fn begin_object_value<W: std::io::Write + ?Sized>(
        &mut self,
        writer: &mut W,
    ) -> std::io::Result<()> {
        self.0.begin_object_value(writer)
    }

    fn end_object_value<W: std::io::Write + ?Sized>(
        &mut self,
        writer: &mut W,
    ) -> std::io::Result<()> {
        self.0.end_object_value(writer)
    }
}

/// Serializes `value` into `sink` in the language's number spelling.
fn serialize_language_json<W: std::io::Write>(
    sink: W,
    value: &serde_json::Value,
    pretty: bool,
) -> Result<(), serde_json::Error> {
    use serde::Serialize as _;
    if pretty {
        let formatter = LanguageFloatsPretty(serde_json::ser::PrettyFormatter::new());
        let mut serializer = serde_json::Serializer::with_formatter(sink, formatter);
        value.serialize(&mut serializer)
    } else {
        let mut serializer = serde_json::Serializer::with_formatter(sink, LanguageFloatsCompact);
        value.serialize(&mut serializer)
    }
}

fn render_json_direct(value: &serde_json::Value, pretty: bool) -> *mut c_char {
    // A recursive exact-size pass formats every number and walks the complete
    // tree before serde immediately repeats the work. Start large enough to
    // avoid churn for ordinary documents and let the builder double for large
    // payloads. Peak growth stays bounded while serialization remains one pass.
    let mut writer = RuntimeJsonWriter::new(64 * 1024);
    let result = serialize_language_json(&mut writer, value, pretty);
    if result.is_err() {
        return alloc_cstring(b"");
    }
    writer.finish()
}

/// Renders a document in the language's one JSON form - keys in order,
/// numbers and strings as `json::encode` writes them - whatever text it was
/// parsed from, so every tier renders a parsed document identically.
fn render_json_handle(json: &GosJson, pretty: bool) -> *mut c_char {
    if json.view.is_null()
        && let Some(text) = json.tree.raw_text()
    {
        let rendered = if pretty {
            transcode_canonical(
                text,
                LanguageFloatsPretty(serde_json::ser::PrettyFormatter::new()),
            )
        } else {
            transcode_canonical(text, LanguageFloatsCompact)
        };
        return match rendered {
            Ok(bytes) => alloc_cstring(&bytes),
            Err(_) => alloc_cstring(b""),
        };
    }
    render_json_direct(json.value(), pretty)
}

/// Renders validated JSON `text` in the form serializing its
/// `serde_json::Value` writes - keys sorted with the last duplicate kept,
/// numbers and strings as that value holds them - streaming from the text
/// instead of building the tree, so rendering costs the output, not a tree
/// several times the document's size.
fn transcode_canonical<F: serde_json::ser::Formatter>(
    text: &str,
    formatter: F,
) -> Result<Vec<u8>, serde_json::Error> {
    use serde::de::DeserializeSeed as _;

    let mut sink = CanonicalSink {
        out: Vec::with_capacity(text.len()),
        formatter,
    };
    let mut deserializer = serde_json::Deserializer::from_str(text);
    deserializer.disable_recursion_limit();
    Transcode(&mut sink).deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(sink.out)
}

/// Output of [`transcode_canonical`] and the formatter whose state (the
/// pretty form's indentation) spans the whole document.
struct CanonicalSink<F> {
    out: Vec<u8>,
    formatter: F,
}

/// `io::Write` over the output that keeps it HTML-safe, as
/// [`RuntimeJsonWriter`] does for a tree's rendering.
struct HtmlSafeVec<'a>(&'a mut Vec<u8>);

impl std::io::Write for HtmlSafeVec<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        write_html_safe(bytes, |part| self.0.extend_from_slice(part));
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<F: serde_json::ser::Formatter> CanonicalSink<F> {
    /// Applies one formatter step to the output.
    fn emit(
        &mut self,
        step: impl FnOnce(&mut F, &mut HtmlSafeVec<'_>) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        step(&mut self.formatter, &mut HtmlSafeVec(&mut self.out))
    }

    /// Writes `value` as a JSON string. Every formatter here escapes strings
    /// with serde_json's defaults, which a compact serializer applies.
    fn string(&mut self, value: &str) -> Result<(), serde_json::Error> {
        use serde::Serializer as _;
        (&mut serde_json::Serializer::new(HtmlSafeVec(&mut self.out))).serialize_str(value)
    }
}

/// One object member already written: its key, and the byte range of its
/// `key: value` text in the output.
struct Member<'de> {
    key: std::borrow::Cow<'de, str>,
    start: usize,
    end: usize,
}

/// Rewrites the members of the object whose text ends the output in key
/// order, keeping the last of equal keys, as a `serde_json::Map` holds them.
fn sort_members(out: &mut Vec<u8>, mut members: Vec<Member<'_>>) {
    let in_order = members.windows(2).all(|pair| pair[0].key < pair[1].key);
    if in_order {
        return;
    }
    let base = members[0].start;
    let separator = out[members[0].end..members[1].start].to_vec();
    let region = out.split_off(base);
    members.sort_by(|a, b| a.key.cmp(&b.key));
    let last_of_each = members
        .iter()
        .enumerate()
        .filter(|&(i, m)| members.get(i + 1).is_none_or(|next| next.key != m.key))
        .map(|(_, m)| m);
    for (i, member) in last_of_each.enumerate() {
        if i > 0 {
            out.extend_from_slice(&separator);
        }
        out.extend_from_slice(&region[member.start - base..member.end - base]);
    }
}

/// Deserializes one value from the text and writes it to the sink.
struct Transcode<'s, F>(&'s mut CanonicalSink<F>);

impl<'de, F: serde_json::ser::Formatter> serde::de::DeserializeSeed<'de> for Transcode<'_, F> {
    type Value = ();

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

/// An object key, borrowed from the text unless it holds an escape.
struct KeySeed;

impl<'de> serde::de::DeserializeSeed<'de> for KeySeed {
    type Value = std::borrow::Cow<'de, str>;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_str(self)
    }
}

impl<'de> serde::de::Visitor<'de> for KeySeed {
    type Value = std::borrow::Cow<'de, str>;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("an object key")
    }

    fn visit_borrowed_str<E>(self, v: &'de str) -> Result<Self::Value, E> {
        Ok(std::borrow::Cow::Borrowed(v))
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
        Ok(std::borrow::Cow::Owned(v.to_owned()))
    }
}

impl<'de, F: serde_json::ser::Formatter> serde::de::Visitor<'de> for Transcode<'_, F> {
    type Value = ();

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<(), E> {
        self.0.emit(|f, w| f.write_null(w)).map_err(E::custom)
    }

    fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<(), E> {
        self.0.emit(|f, w| f.write_bool(w, v)).map_err(E::custom)
    }

    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<(), E> {
        self.0.emit(|f, w| f.write_i64(w, v)).map_err(E::custom)
    }

    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<(), E> {
        self.0.emit(|f, w| f.write_u64(w, v)).map_err(E::custom)
    }

    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<(), E> {
        self.0.emit(|f, w| f.write_f64(w, v)).map_err(E::custom)
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<(), E> {
        self.0.string(v).map_err(E::custom)
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        use serde::de::Error as _;
        let sink = self.0;
        sink.emit(|f, w| f.begin_array(w))
            .map_err(A::Error::custom)?;
        let mut first = true;
        loop {
            // The separator goes out before the element is known to exist;
            // formatters keep no state for it, so an end takes it back.
            let separator_at = sink.out.len();
            sink.emit(|f, w| f.begin_array_value(w, first))
                .map_err(A::Error::custom)?;
            if seq.next_element_seed(Transcode(&mut *sink))?.is_none() {
                sink.out.truncate(separator_at);
                break;
            }
            sink.emit(|f, w| f.end_array_value(w))
                .map_err(A::Error::custom)?;
            first = false;
        }
        sink.emit(|f, w| f.end_array(w)).map_err(A::Error::custom)
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        use serde::de::Error as _;
        let sink = self.0;
        sink.emit(|f, w| f.begin_object(w))
            .map_err(A::Error::custom)?;
        let mut members = Vec::new();
        loop {
            let separator_at = sink.out.len();
            sink.emit(|f, w| f.begin_object_key(w, members.is_empty()))
                .map_err(A::Error::custom)?;
            let start = sink.out.len();
            let Some(key) = map.next_key_seed(KeySeed)? else {
                sink.out.truncate(separator_at);
                break;
            };
            sink.string(&key).map_err(A::Error::custom)?;
            sink.emit(|f, w| f.end_object_key(w))
                .map_err(A::Error::custom)?;
            sink.emit(|f, w| f.begin_object_value(w))
                .map_err(A::Error::custom)?;
            map.next_value_seed(Transcode(&mut *sink))?;
            sink.emit(|f, w| f.end_object_value(w))
                .map_err(A::Error::custom)?;
            members.push(Member {
                key,
                start,
                end: sink.out.len(),
            });
        }
        if members.len() > 1 {
            sort_members(&mut sink.out, members);
        }
        sink.emit(|f, w| f.end_object(w)).map_err(A::Error::custom)
    }
}

/// `json::render(value) -> String`. Always returns a non-null
/// C-string (empty on null input) into the GC arena.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_render(j: *const GosJson) -> *mut c_char {
    ffi_entry!({
        // SAFETY: `j` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `json_handle` accepts.
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
    ffi_entry!({
        // SAFETY: `j` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `json_handle` accepts.
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
    ffi_entry!({
        // SAFETY: `j` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `json_borrow` accepts.
        let Some(v) = (unsafe { json_borrow(j) }) else {
            return alloc_cstring(b"null");
        };
        match v {
            serde_json::Value::String(s) => alloc_cstring(s.as_bytes()),
            other => render_json_direct(other, false),
        }
    })
}

/// Debug form of a `json::Value` for `{:?}` inside an `Option`/`Result`
/// payload: strings keep their JSON quotes, matching the VM's Debug
/// rendering, where Display strips them from a top-level string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_debug(j: *const GosJson) -> *mut c_char {
    ffi_entry!({
        // SAFETY: `j` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `json_borrow` accepts.
        let Some(v) = (unsafe { json_borrow(j) }) else {
            return alloc_cstring(b"null");
        };
        render_json_direct(v, false)
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
    ffi_entry!({
        // SAFETY: `j` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `json_handle` accepts.
        let Some(parent) = (unsafe { json_handle(j) }) else {
            return GosJson::null_ptr();
        };
        // `parent.view` is a stable interior pointer into
        // `parent.tree`'s allocation; see `GosJson` doc. The
        // dereference produces a borrow that lives only inside this
        // function call.
        let v = parent.value();
        let key_bytes: &[u8] = if key.is_null() {
            b""
        } else {
            // SAFETY: `key` is a String argument from compiled code, null or a live string body for the whole call.
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
    ffi_entry!({
        // SAFETY: `j` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `json_handle` accepts.
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
    ffi_entry!({
        // SAFETY: `j` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `json_borrow` accepts.
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
    ffi_entry!({
        // SAFETY: `j` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `json_borrow` accepts.
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
    ffi_entry!({
        // SAFETY: `j` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `json_borrow` accepts.
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
    ffi_entry!({
        // SAFETY: `j` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `json_borrow` accepts.
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
    ffi_entry!({
        // SAFETY: `j` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `json_borrow` accepts.
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
    ffi_entry!({
        // SAFETY: `j` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `json_borrow` accepts.
        match unsafe { json_borrow(j) } {
            Some(serde_json::Value::Number(n)) => {
                if let Some(i) = n.as_i64() {
                    gos_rt_result_new(0, i)
                } else if let Some(f) = n.as_f64() {
                    if f.fract() == 0.0 && f >= i64::MIN as f64 && f <= i64::MAX as f64 {
                        gos_rt_result_new(0, f as i64)
                    } else {
                        gos_rt_result_new(1, 0)
                    }
                } else {
                    gos_rt_result_new(1, 0)
                }
            }
            _ => gos_rt_result_new(1, 0),
        }
    })
}

/// `json::as_u64(value) -> Option<u64>`: the integer when it is non-negative
/// and fits a `u64`, the payload word holding its bits.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_as_u64_opt(j: *const GosJson) -> i128 {
    ffi_entry!({
        // SAFETY: `j` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `json_borrow` accepts.
        let n = match unsafe { json_borrow(j) } {
            Some(serde_json::Value::Number(n)) => n.as_u64().or_else(|| {
                n.as_f64()
                    .filter(|f| f.fract() == 0.0 && *f >= 0.0 && *f <= u64::MAX as f64)
                    .map(|f| f as u64)
            }),
            _ => None,
        };
        match n {
            Some(n) => gos_rt_result_new(0, n as i64),
            None => gos_rt_result_new(1, 0),
        }
    })
}

/// `value.as_f64() -> Option<f64>` - `Some` for any JSON number,
/// `None` otherwise.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_as_f64_opt(j: *const GosJson) -> i128 {
    ffi_entry!({
        // SAFETY: `j` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `json_borrow` accepts.
        match unsafe { json_borrow(j) } {
            Some(serde_json::Value::Number(n)) => {
                gos_rt_result_new_f64(0, n.as_f64().unwrap_or(0.0))
            }
            _ => gos_rt_result_new(1, 0),
        }
    })
}

/// `value.as_str() -> Option<String>` - `Some` only for a JSON
/// string, `None` otherwise.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_as_str_opt(j: *const GosJson) -> i128 {
    ffi_entry!({
        // SAFETY: `j` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `json_borrow` accepts.
        match unsafe { json_borrow(j) } {
            Some(serde_json::Value::String(s)) => {
                let cs = alloc_cstring(s.as_bytes());
                gos_rt_result_new(0, cs as i64)
            }
            _ => gos_rt_result_new(1, 0),
        }
    })
}

/// `value.as_bool() -> bool`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_as_bool(j: *const GosJson) -> i32 {
    ffi_entry!({
        // SAFETY: `j` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `json_borrow` accepts.
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
    ffi_entry!({
        // SAFETY: `j` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `json_borrow` accepts.
        match unsafe { json_borrow(j) } {
            Some(serde_json::Value::Bool(b)) => gos_rt_result_new(0, i64::from(*b)),
            _ => gos_rt_result_new(1, 0),
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
    ffi_entry!({ j })
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
    ffi_entry!({
        // SAFETY: `j` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `json_handle` accepts.
        let Some(parent) = (unsafe { json_handle(j) }) else {
            return gos_rt_result_new(1, 0);
        };
        let key_bytes: &[u8] = if key.is_null() {
            b""
        } else {
            // SAFETY: `key` is a String argument from compiled code, null or a live string body for the whole call.
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
    ffi_entry!({
        // SAFETY: `j` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `json_borrow` accepts.
        let Some(v) = (unsafe { json_borrow(j) }) else {
            return gos_rt_result_new(1, 0);
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
                let vec_ptr = {
                    crate::c_abi::vec::gos_rt_vec_with_capacity_typed(
                        8,
                        map.len().min(i64::MAX as usize) as i64,
                        crate::c_abi::vec::vec_elem_kind::STRING,
                    )
                };
                for k in map.keys() {
                    let cs = alloc_cstring(k.as_bytes()) as i64;
                    // SAFETY: `vec_ptr` is the fresh key vector made above, or null, which
                    // `gos_rt_vec_push` accepts, and `cs` is one 8-byte element.
                    unsafe {
                        gos_rt_vec_push(vec_ptr, std::ptr::addr_of!(cs).cast::<u8>());
                    }
                }
                gos_rt_result_new(0, vec_ptr as i64)
            }
            _ => gos_rt_result_new(1, 0),
        }
    })
}

/// `json::as_array(value) -> Option<[json::Value]>`. Returns
/// `Some(vec)` of element-pointers for an array node, `None`
/// otherwise. Each element is materialised as a fresh `GosJson*`
/// so the receiver can be dropped independently.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_as_array_opt(j: *const GosJson) -> i128 {
    ffi_entry!({
        // SAFETY: `j` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `json_handle` accepts.
        let Some(parent) = (unsafe { json_handle(j) }) else {
            return gos_rt_result_new(1, 0);
        };
        let v = parent.value();
        match v {
            serde_json::Value::Array(items) => {
                // Each returned child handle needs one pointer slot. Reserve
                // once from the source array's exact length instead of
                // growing through every capacity tier.
                let vec_ptr = {
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
                    // SAFETY: `vec_ptr` is the fresh element vector made above, or null, which
                    // `gos_rt_vec_push` accepts, and `elem` is one 8-byte element.
                    unsafe {
                        gos_rt_vec_push(vec_ptr, std::ptr::addr_of!(elem).cast::<u8>());
                    }
                }
                gos_rt_result_new(0, vec_ptr as i64)
            }
            _ => gos_rt_result_new(1, 0),
        }
    })
}

/// `json::Value::String(s)` constructor.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_value_string(s: *const c_char) -> *mut GosJson {
    ffi_entry!({
        let text = if s.is_null() {
            String::new()
        } else {
            // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_string(s) }
        };
        GosJson::into_raw(serde_json::Value::String(text))
    })
}

/// `json::Value::Int(n)` constructor.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_json_value_int(n: i64) -> *mut GosJson {
    ffi_entry!({ GosJson::into_raw(serde_json::Value::Number(n.into())) })
}

/// `json::Value` integer constructor for a word declared `u64` / `usize`,
/// which reads as unsigned.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_json_value_uint(n: i64) -> *mut GosJson {
    ffi_entry!({ GosJson::into_raw(serde_json::Value::Number((n as u64).into())) })
}

/// `json::Value::Bool(b)` constructor.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_json_value_bool(b: i32) -> *mut GosJson {
    ffi_entry!({ GosJson::into_raw(serde_json::Value::Bool(b != 0)) })
}

/// `json::Value::Float(x)` constructor used by `json::render` on
/// struct fields of type `f64`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_json_value_float(x: f64) -> *mut GosJson {
    ffi_entry!({
        // JSON has no NaN or infinity, so such a float is `null`.
        let value = serde_json::Number::from_f64(x)
            .map_or(serde_json::Value::Null, serde_json::Value::Number);
        GosJson::into_raw(value)
    })
}

/// A JSON number from an `f32` held at double width, spelled with the
/// single-precision value's digits.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_value_float32(x: f64) -> *mut GosJson {
    gos_rt_json_value_float(crate::builtins::f32_as_decimal_double(x))
}

/// `json::Value::Null` constructor.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_json_value_null() -> *mut GosJson {
    ffi_entry!({ GosJson::null_ptr() })
}

/// `json::Value::Array(vec)` constructor. Takes a `*mut GosVec` of
/// `*mut GosJson` element pointers and rebuilds a real
/// `serde_json::Value::Array`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_value_array(vec: *const GosVec) -> *mut GosJson {
    ffi_entry!({
        // SAFETY: `vec` is this shim's argument, null or a live `Vec<json::Value>` (C-ABI
        // contract).
        let items = unsafe { crate::c_abi::vec::VecView::of(vec) };
        let out: Vec<serde_json::Value> = items.map_or_else(Vec::new, |items| {
            (0..items.len())
                .map(|i| {
                    // SAFETY: each element of a `Vec<json::Value>` is a live handle or null
                    // (C-ABI contract), which `json_borrow` accepts.
                    unsafe { json_borrow(items.pointer_at::<GosJson>(i)) }
                        .map_or(serde_json::Value::Null, Clone::clone)
                })
                .collect()
        });
        GosJson::into_raw(serde_json::Value::Array(out))
    })
}

/// Builds a `json::Value::Array` from a Gossamer `Vec` of scalar
/// elements. `kind` selects how each 8-byte slot is read:
/// 0 = i64, 1 = f64 (bit pattern), 2 = String (`*const c_char`),
/// 3 = bool, 4 = an integer declared `u64` / `usize`, 5 = an `f32` at double
/// width. Used by `json::encode([…])` on a scalar array, where
/// the MIR has a typed scalar `*GosVec` rather than a Vec of
/// pre-boxed `*GosJson` pointers (the shape `gos_rt_json_value_array`
/// expects).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_array_from_scalar_vec(
    vec: *const GosVec,
    kind: i64,
) -> *mut GosJson {
    ffi_entry!({
        let out: Vec<serde_json::Value> = if kind == 2 {
            // SAFETY: a kind-2 `vec` is this shim's argument, null or a live `Vec<String>` (C-ABI
            // contract).
            unsafe { crate::c_abi::vec::StrVecView::of(vec) }.map_or_else(Vec::new, |items| {
                items.texts().map(serde_json::Value::String).collect()
            })
        } else {
            // SAFETY: `vec` is this shim's argument, null or a live `Vec` (C-ABI contract). Each
            // element reads at the width its header declares, so a byte-packed `Vec<u8>` or
            // `Vec<bool>` answers its own values.
            unsafe { crate::c_abi::vec::VecView::of(vec) }.map_or_else(Vec::new, |items| {
                items.words().map(|word| scalar_json(kind, word)).collect()
            })
        };
        GosJson::into_raw(serde_json::Value::Array(out))
    })
}

/// The JSON value a scalar slot word spells under `kind`: `1` an `f64`'s bits,
/// `3` a `bool`, `4` a `u64`, `5` an `f32` at double width, anything else an
/// `i64`.
fn scalar_json(kind: i64, word: i64) -> serde_json::Value {
    match kind {
        1 => serde_json::Number::from_f64(f64::from_bits(word as u64))
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        3 => serde_json::Value::Bool(word != 0),
        4 => serde_json::Value::Number((word as u64).into()),
        5 => serde_json::Number::from_f64(crate::builtins::f32_as_decimal_double(f64::from_bits(
            word as u64,
        )))
        .map_or(serde_json::Value::Null, serde_json::Value::Number),
        _ => serde_json::Value::Number(word.into()),
    }
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
    ffi_entry!({
        let mut out = serde_json::Map::new();
        let n = usize::try_from(n.max(0)).unwrap_or(0);
        if !pairs.is_null() && n > 0 {
            // SAFETY: `pairs` is non-null (checked above) and addresses `n` name/value word pairs
            // (C-ABI contract).
            let slice = unsafe { std::slice::from_raw_parts(pairs, n * 2) };
            for chunk in slice.chunks_exact(2) {
                let key_ptr = chunk[0] as *const c_char;
                let val_ptr = chunk[1] as *mut GosJson;
                let key = if key_ptr.is_null() {
                    String::new()
                } else {
                    // SAFETY: `key_ptr` is a non-null name word, a live string body (C-ABI
                    // contract).
                    unsafe { crate::c_abi::gos_str_arg_string(key_ptr) }
                };
                // SAFETY: each value word is a live handle or null (C-ABI contract), which
                // `json_borrow` accepts.
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
    // SAFETY: `vec` is this shim's argument, live for the call (C-ABI contract) or null, which
    // `gos_rt_json_value_object_keyed` accepts.
    unsafe { gos_rt_json_value_object_keyed(vec, 0) }
}

/// Spells a builder pair's key word as an object member name, the way a map
/// of that key type renders its keys: `0` a borrowed C string, `1` an `i64`,
/// `2` a word declared `u64` / `usize`, `3` a `bool`, `4` a `char`.
///
/// # Safety
/// For kind `0`, `word` must be null or a live NUL-terminated string.
unsafe fn object_member_name(word: i64, key_kind: i64) -> String {
    match key_kind {
        1 => word.to_string(),
        2 => (word as u64).to_string(),
        3 => (word != 0).to_string(),
        4 => u32::try_from(word)
            .ok()
            .and_then(char::from_u32)
            .map_or_else(String::new, String::from),
        _ => {
            let key_ptr = word as *const c_char;
            if key_ptr.is_null() {
                String::new()
            } else {
                // SAFETY: `key_ptr` is non-null (checked above), and this `unsafe fn`'s caller
                // passes a live string for kind `0`.
                unsafe { crate::c_abi::gos_str_arg_string(key_ptr) }
            }
        }
    }
}

/// [`gos_rt_json_value_object`] over pairs whose key words are of the kind
/// `object_member_name` names.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_value_object_keyed(
    vec: *const GosVec,
    key_kind: i64,
) -> *mut GosJson {
    ffi_entry!({
        let mut out = serde_json::Map::new();
        if !vec.is_null() {
            // SAFETY: `vec` is non-null (checked above) and live for the call (C-ABI contract).
            let header = unsafe { &*vec };
            let pair_count = object_pair_count(header);
            if pair_count > 0 {
                // SAFETY: `object_pair_count` answers a non-zero count only for a non-null
                // buffer holding that many name/value word pairs.
                let pairs = unsafe {
                    std::slice::from_raw_parts(header.ptr.cast::<[i64; 2]>(), pair_count)
                };
                for pair in pairs {
                    let val_ptr = pair[1] as *mut GosJson;
                    // SAFETY: a pair's name word is of the kind `key_kind` names (this shim's
                    // contract).
                    let key = unsafe { object_member_name(pair[0], key_kind) };
                    // SAFETY: each value word is a live handle or null (C-ABI contract), which
                    // `json_borrow` accepts.
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
    ffi_entry!({
        // SAFETY: `obj` is this shim's argument, live for the call (C-ABI contract) or null,
        // which `json_handle` accepts.
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
            // SAFETY: `key` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_string(key) }
        };
        // SAFETY: `val` is this shim's argument, live for the call (C-ABI contract) or null,
        // which `json_borrow` accepts.
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

/// A compact JSON document written one token at a time by the compiled
/// encoder of a typed value: the sink and escaping are the tree renderer's,
/// so the bytes are the ones `gos_rt_json_render` answers for the same
/// value, and the open containers are tracked here so each token knows
/// whether a comma precedes it.
pub struct JsonTokenWriter {
    sink: RuntimeJsonWriter,
    /// One entry per open container: whether it is an array, and whether its
    /// next member is the first.
    open: Vec<(bool, bool)>,
    /// Indented form, matching `serde_json::to_writer_pretty`: two spaces per
    /// level, a newline before every member, and an empty container written
    /// without one.
    pretty: bool,
}

impl JsonTokenWriter {
    /// Writes the newline and indent that precede a member at the current
    /// depth. Nothing in compact form.
    fn newline_indent(&mut self) {
        if !self.pretty {
            return;
        }
        self.sink.append(b"\n");
        for _ in 0..self.open.len() {
            self.sink.append(b"  ");
        }
    }

    /// Separates a value from the one before it inside an array. An object
    /// member's separator is written with its key.
    fn begin_value(&mut self) {
        if let Some((true, first)) = self.open.last_mut() {
            let separate = !*first;
            *first = false;
            if separate {
                self.sink.append(b",");
            }
            self.newline_indent();
        }
    }

    /// Closes the innermost container: an empty one keeps its delimiters
    /// together, a populated one puts the closer on its own line.
    fn end_container(&mut self, close: &[u8]) {
        let was_empty = matches!(self.open.last(), Some(&(_, first)) if first);
        self.open.pop();
        if self.pretty && !was_empty {
            self.sink.append(b"\n");
            for _ in 0..self.open.len() {
                self.sink.append(b"  ");
            }
        }
        self.sink.append(close);
    }

    fn write_escaped(&mut self, text: &str) {
        self.sink.append(b"\"");
        crate::c_abi::json_escape::json_escape_with(text.as_bytes(), |run| self.sink.append(run));
        self.sink.append(b"\"");
    }
}

/// The writer `w` names, or `None` for null.
///
/// # Safety
/// `w` is null or a live writer nothing else accesses for `'a`.
unsafe fn token_writer<'a>(w: *mut JsonTokenWriter) -> Option<&'a mut JsonTokenWriter> {
    if w.is_null() {
        None
    } else {
        // SAFETY: `w` is non-null (checked above), and this `unsafe fn`'s caller passes a live
        // writer nothing else accesses meanwhile.
        Some(unsafe { &mut *w })
    }
}

/// Opens a compact JSON document for the token writers below; closed by
/// `gos_rt_json_writer_finish`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_json_writer_new() -> *mut JsonTokenWriter {
    ffi_entry!({
        Box::into_raw(Box::new(JsonTokenWriter {
            sink: RuntimeJsonWriter::new(64 * 1024),
            open: Vec::new(),
            pretty: false,
        }))
    })
}

/// Opens an indented JSON document, in the form
/// `gos_rt_json_render_pretty` answers.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_json_writer_new_pretty() -> *mut JsonTokenWriter {
    ffi_entry!({
        Box::into_raw(Box::new(JsonTokenWriter {
            sink: RuntimeJsonWriter::new(64 * 1024),
            open: Vec::new(),
            pretty: true,
        }))
    })
}

/// Writes `{` as the next value and opens the object.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_writer_begin_object(w: *mut JsonTokenWriter) {
    ffi_entry!({
        // SAFETY: `w` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `token_writer` accepts.
        if let Some(writer) = unsafe { token_writer(w) } {
            writer.begin_value();
            writer.sink.append(b"{");
            writer.open.push((false, true));
        }
    });
}

/// Closes the innermost object with `}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_writer_end_object(w: *mut JsonTokenWriter) {
    ffi_entry!({
        // SAFETY: `w` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `token_writer` accepts.
        if let Some(writer) = unsafe { token_writer(w) } {
            writer.end_container(b"}");
        }
    });
}

/// Writes `[` as the next value and opens the array.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_writer_begin_array(w: *mut JsonTokenWriter) {
    ffi_entry!({
        // SAFETY: `w` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `token_writer` accepts.
        if let Some(writer) = unsafe { token_writer(w) } {
            writer.begin_value();
            writer.sink.append(b"[");
            writer.open.push((true, true));
        }
    });
}

/// Closes the innermost array with `]`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_writer_end_array(w: *mut JsonTokenWriter) {
    ffi_entry!({
        // SAFETY: `w` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `token_writer` accepts.
        if let Some(writer) = unsafe { token_writer(w) } {
            writer.end_container(b"]");
        }
    });
}

/// Writes the name of the next member of the innermost object, with the
/// comma that separates it from the member before.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_writer_key(w: *mut JsonTokenWriter, key: *const c_char) {
    ffi_entry!({
        // SAFETY: `w` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `token_writer` accepts.
        let Some(writer) = (unsafe { token_writer(w) }) else {
            return;
        };
        if let Some((false, first)) = writer.open.last_mut() {
            let separate = !*first;
            *first = false;
            if separate {
                writer.sink.append(b",");
            }
            writer.newline_indent();
        }
        // SAFETY: `key` is a String argument from compiled code, null or a live string body for the whole call.
        let text = unsafe { crate::c_abi::gos_str_arg_lossy(key) };
        writer.write_escaped(&text);
        writer.sink.append(if writer.pretty { b": " } else { b":" });
    });
}

/// Writes a string value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_writer_str(w: *mut JsonTokenWriter, s: *const c_char) {
    ffi_entry!({
        // SAFETY: `w` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `token_writer` accepts.
        if let Some(writer) = unsafe { token_writer(w) } {
            writer.begin_value();
            // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
            let text = unsafe { crate::c_abi::gos_str_arg_lossy(s) };
            writer.write_escaped(&text);
        }
    });
}

/// Writes an integer value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_writer_i64(w: *mut JsonTokenWriter, n: i64) {
    ffi_entry!({
        // SAFETY: `w` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `token_writer` accepts.
        if let Some(writer) = unsafe { token_writer(w) } {
            writer.begin_value();
            let _ = serde_json::to_writer(&mut writer.sink, &n);
        }
    });
}

/// Writes an integer declared `u64` / `usize`, whose word reads as unsigned.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_writer_u64(w: *mut JsonTokenWriter, n: i64) {
    ffi_entry!({
        // SAFETY: `w` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `token_writer` accepts.
        if let Some(writer) = unsafe { token_writer(w) } {
            writer.begin_value();
            let _ = serde_json::to_writer(&mut writer.sink, &(n as u64));
        }
    });
}

/// Writes a float value; a non-finite one renders as `0`, as the tree
/// constructor stores it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_writer_f64(w: *mut JsonTokenWriter, x: f64) {
    ffi_entry!({
        // SAFETY: `w` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `token_writer` accepts.
        if let Some(writer) = unsafe { token_writer(w) } {
            writer.begin_value();
            let _ = write_language_float(&mut writer.sink, x);
        }
    });
}

/// Writes an `f32` held at double width as its single-precision digits.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_writer_f32(w: *mut JsonTokenWriter, x: f64) {
    // SAFETY: `w` is this shim's argument, null or a live writer (C-ABI contract), which
    // `gos_rt_json_writer_f64` accepts.
    unsafe { gos_rt_json_writer_f64(w, crate::builtins::f32_as_decimal_double(x)) }
}

/// Writes a boolean value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_writer_bool(w: *mut JsonTokenWriter, b: i32) {
    ffi_entry!({
        // SAFETY: `w` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `token_writer` accepts.
        if let Some(writer) = unsafe { token_writer(w) } {
            writer.begin_value();
            writer.sink.append(if b != 0 { b"true" } else { b"false" });
        }
    });
}

/// Writes `null`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_writer_null(w: *mut JsonTokenWriter) {
    ffi_entry!({
        // SAFETY: `w` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `token_writer` accepts.
        if let Some(writer) = unsafe { token_writer(w) } {
            writer.begin_value();
            writer.sink.append(b"null");
        }
    });
}

/// Writes a `json::Value` the caller keeps, as the tree renderer would
/// render it in place.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_writer_value(w: *mut JsonTokenWriter, j: *const GosJson) {
    ffi_entry!({
        // SAFETY: `w` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `token_writer` accepts.
        if let Some(writer) = unsafe { token_writer(w) } {
            writer.begin_value();
            // SAFETY: `j` is this shim's argument, live for the call (C-ABI contract) or null,
            // which `json_handle` accepts.
            match unsafe { json_handle(j) } {
                Some(json) if writer.pretty => {
                    // `to_writer_pretty` always indents from column zero, so
                    // the rendered lines are shifted to the depth this value
                    // sits at before they are appended.
                    let mut nested = Vec::new();
                    if serialize_language_json(&mut nested, json.value(), true).is_ok() {
                        let depth = writer.open.len();
                        let text = String::from_utf8_lossy(&nested).into_owned();
                        for (i, line) in text.split('\n').enumerate() {
                            if i > 0 {
                                writer.sink.append(b"\n");
                                for _ in 0..depth {
                                    writer.sink.append(b"  ");
                                }
                            }
                            writer.sink.append(line.as_bytes());
                        }
                    }
                }
                Some(json) => {
                    let _ = serialize_language_json(&mut writer.sink, json.value(), false);
                }
                None => writer.sink.append(b"null"),
            }
        }
    });
}

/// Closes the document and answers its text as a `String`; the writer is
/// released.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_json_writer_finish(w: *mut JsonTokenWriter) -> *mut c_char {
    ffi_entry!({
        if w.is_null() {
            return alloc_cstring(b"");
        }
        // SAFETY: `w` is non-null (checked above), the writer `gos_rt_json_writer_new` boxed,
        // which this call consumes (C-ABI contract).
        let writer = unsafe { Box::from_raw(w) };
        writer.sink.finish()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    /// Refcount word of an `alloc_cstring` builder-layout string:
    /// `[rc:u32][cap:u32][len:u32][tag][content][NUL]`, body at +13.
    unsafe fn str_rc(s: *const c_char) -> u32 {
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let hdr = unsafe { s.cast::<u8>().sub(13) };
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        u32::from_le_bytes(unsafe { [*hdr, *hdr.add(1), *hdr.add(2), *hdr.add(3)] })
    }

    #[test]
    fn json_keys_vec_is_string_typed_and_deep_frees_unvisited_keys() {
        let text = crate::c_abi::string::test_gos_str(r#"{"alpha":1,"beta":2}"#);
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let pr = unsafe { gos_rt_json_parse(text) };
        assert_eq!(crate::c_abi::result::gos_rt_result_disc(pr), 0);
        let j = crate::c_abi::result::gos_rt_result_payload(pr) as *mut GosJson;
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let kr = unsafe { gos_rt_json_keys_opt(j) };
        assert_eq!(crate::c_abi::result::gos_rt_result_disc(kr), 0);
        let v = crate::c_abi::result::gos_rt_result_payload(kr) as *mut crate::c_abi::vec::GosVec;
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let vec = unsafe { &*v };
        assert_eq!(vec.len, 2);
        assert_eq!(vec.elem_kind, crate::c_abi::vec::vec_elem_kind::STRING);
        // Probe-share key 0, free the vec WITHOUT iterating (the
        // early-break consumer shape): deep-free must release exactly
        // the vec's share - rc 2 -> 1, not 2 (leak), not 0 (double free).
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let k0 = unsafe { crate::c_abi::vec::slot_read_word(vec.ptr.as_ptr()).cast::<c_char>() };
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe { crate::c_abi::string::gos_rt_str_retain(k0) };
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        assert_eq!(unsafe { str_rc(k0) }, 2);
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe { crate::c_abi::map::gos_rt_vec_free(v) };
        assert_eq!(
            // SAFETY: every pointer argument is a value this test built above and still holds
            // live; a null one is accepted by the callee.
            unsafe { str_rc(k0) },
            1,
            "deep-free must release the vec's share once"
        );
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        assert_eq!(unsafe { CStr::from_ptr(k0) }.to_str().unwrap(), "alpha");
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe { crate::c_abi::string::gos_rt_str_free(k0) };
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe { gos_rt_json_free(j) };
    }

    #[test]
    fn json_input_limits_match_vm_defaults_and_ignore_string_brackets() {
        let depth = gossamer_core::json::DEFAULT_MAX_DEPTH;
        assert!(gossamer_core::json::validate(br#"{"brackets":"[{]}"}"#).is_ok());
        let at_limit = format!("{}0{}", "[".repeat(depth), "]".repeat(depth));
        let at_limit = crate::c_abi::string::test_gos_str(&at_limit);
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let parsed = unsafe { gos_rt_json_parse(at_limit) };
        assert_eq!(crate::c_abi::result::gos_rt_result_disc(parsed), 0);
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            gos_rt_json_free(crate::c_abi::result::gos_rt_result_payload(parsed) as *mut GosJson);
        }
        let nested = format!("{}0{}", "[".repeat(depth + 1), "]".repeat(depth + 1));
        let refused = gossamer_core::json::validate(nested.as_bytes()).unwrap_err();
        assert_eq!(refused.message, "nesting depth exceeds max_depth (128)");
        let large = vec![b' '; gossamer_core::json::DEFAULT_MAX_SIZE + 1];
        let refused = gossamer_core::json::validate(&large).unwrap_err();
        assert!(refused.message.starts_with("input exceeds max_size"));
    }

    #[test]
    fn streamed_render_matches_the_tree_render() {
        let documents = [
            r#"{"b":1,"a":[true,null,{"z":"<&>","y":{}}],"c":[]}"#,
            r#"{"k":1,"k":2,"a":0,"k":3}"#,
            r#"{"a\"b":"x\u2028y","a":"\u00e9\n","\u0041":1.50,"n":-0}"#,
            r"[1e3,18446744073709551615,-9223372036854775808,99999999999999999999,0.1]",
            r#"  { "nested" : { "b" : [ { "d":1, "c":2 } ], "a" : [ ] } }  "#,
            r#""top""#,
            "{}",
        ];
        for text in documents {
            let tree = parse_checked_json(text).unwrap();
            let expected_compact = {
                let mut out = Vec::new();
                serialize_language_json(HtmlSafeVec(&mut out), &tree, false).unwrap();
                out
            };
            let expected_pretty = {
                let mut out = Vec::new();
                serialize_language_json(HtmlSafeVec(&mut out), &tree, true).unwrap();
                out
            };
            let compact = transcode_canonical(text, LanguageFloatsCompact).unwrap();
            let pretty = transcode_canonical(
                text,
                LanguageFloatsPretty(serde_json::ser::PrettyFormatter::new()),
            )
            .unwrap();
            assert_eq!(
                String::from_utf8(compact).unwrap(),
                String::from_utf8(expected_compact).unwrap()
            );
            assert_eq!(
                String::from_utf8(pretty).unwrap(),
                String::from_utf8(expected_pretty).unwrap()
            );
        }
    }

    #[test]
    fn json_render_keeps_every_parsed_double() {
        let text =
            crate::c_abi::string::test_gos_str(r#"{"score":12.100000000000001,"short":20.9}"#);
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let parsed = unsafe { gos_rt_json_parse(text) };
        assert_eq!(crate::c_abi::result::gos_rt_result_disc(parsed), 0);
        let json = crate::c_abi::result::gos_rt_result_payload(parsed) as *mut GosJson;
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let rendered_ptr = unsafe { gos_rt_json_render(json) };
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let rendered = unsafe { CStr::from_ptr(rendered_ptr) }.to_str().unwrap();
        assert!(
            rendered.contains("\"score\":12.100000000000001"),
            "a parsed number renders as the double it parsed to: {rendered}"
        );
        assert!(rendered.contains("\"short\":20.9"));
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe { crate::c_abi::string::gos_rt_str_free(rendered_ptr) };
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe { gos_rt_json_free(json) };
    }

    #[test]
    fn a_parsed_document_is_materialised_on_first_use_and_renders_canonically() {
        let text = crate::c_abi::string::test_gos_str(" { \"value\" : 7 } ");
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let parsed = unsafe { gos_rt_json_parse(text) };
        let json = crate::c_abi::result::gos_rt_result_payload(parsed) as *mut GosJson;
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let handle = unsafe { &*json };
        assert!(matches!(
            &*handle.tree,
            JsonTree::Raw { parsed, .. } if parsed.get().is_none()
        ));
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let rendered_ptr = unsafe { gos_rt_json_render(json) };
        assert_eq!(
            // SAFETY: every pointer argument is a value this test built above and still holds
            // live; a null one is accepted by the callee.
            unsafe { CStr::from_ptr(rendered_ptr) }.to_bytes(),
            br#"{"value":7}"#
        );
        let key = c"value";
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let child = unsafe { gos_rt_json_get(json, crate::c_abi::string::test_gos_ptr(key)) };
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        assert_eq!(unsafe { gos_rt_json_as_i64(child) }, 7);
        assert!(matches!(
            &*handle.tree,
            JsonTree::Raw { parsed, .. } if parsed.get().is_some()
        ));
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            gos_rt_str_free(rendered_ptr);
            gos_rt_json_free(child);
            gos_rt_json_free(json);
        }
    }

    #[test]
    fn direct_json_render_keeps_html_safe_escaping() {
        let text = crate::c_abi::string::test_gos_str(r#"{"x":"<>&\u2028\u2029"}"#);
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let parsed = unsafe { gos_rt_json_parse(text) };
        let json = crate::c_abi::result::gos_rt_result_payload(parsed) as *mut GosJson;
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let rendered_ptr = unsafe { gos_rt_json_render(json) };
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let rendered = unsafe { CStr::from_ptr(rendered_ptr) }.to_str().unwrap();
        assert_eq!(rendered, r#"{"x":"\u003c\u003e\u0026\u2028\u2029"}"#);
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe { crate::c_abi::string::gos_rt_str_free(rendered_ptr) };
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe { gos_rt_json_free(json) };
    }

    #[test]
    fn json_collection_projections_reserve_the_source_length() {
        let text = crate::c_abi::string::test_gos_str(
            r#"{"k0":0,"k1":1,"k2":2,"k3":3,"k4":4,"k5":5,"k6":6,"k7":7,"k8":8}"#,
        );
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let parsed = unsafe { gos_rt_json_parse(text) };
        let json = crate::c_abi::result::gos_rt_result_payload(parsed) as *mut GosJson;

        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let keys = unsafe { gos_rt_json_keys_opt(json) };
        let keys = crate::c_abi::result::gos_rt_result_payload(keys) as *mut GosVec;
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        assert_eq!(unsafe { (*keys).len }, 9);
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        assert!(unsafe { (*keys).cap } >= 9);

        let array_text = crate::c_abi::string::test_gos_str("[0,1,2,3,4,5,6,7,8]");
        let array_result =
            // SAFETY: every pointer argument is a value this test built above and still holds
            // live; a null one is accepted by the callee.
            unsafe { gos_rt_json_parse(array_text) };
        let array = crate::c_abi::result::gos_rt_result_payload(array_result) as *mut GosJson;
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let items = unsafe { gos_rt_json_as_array_opt(array) };
        let items = crate::c_abi::result::gos_rt_result_payload(items) as *mut GosVec;
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        assert_eq!(unsafe { (*items).len }, 9);
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        assert!(unsafe { (*items).cap } >= 9);

        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe { crate::c_abi::map::gos_rt_vec_free(keys) };
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe { crate::c_abi::map::gos_rt_vec_free(items) };
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe { gos_rt_json_free(json) };
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe { gos_rt_json_free(array) };
    }
}
