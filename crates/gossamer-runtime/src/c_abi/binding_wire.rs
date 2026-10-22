//! Conversions between the runtime's own value shapes and the wire
//! shapes a `[rust-bindings]` crate exchanges.
//!
//! A binding thunk speaks the `gossamer-binding` ABI: `Bytes` is a
//! packed byte buffer, a `Map<K, V>` is a pair of parallel vectors,
//! and a tuple is a tagged field array. The compiled tiers hold the
//! same values as a runtime `GosVec`, a `GosMap`, and a run of
//! 8-byte slots, so every call across the boundary converts here.
//! `GosVec` itself is layout-identical on both sides and crosses
//! unchanged.

use std::ffi::{CStr, c_char};
use std::sync::Arc;

use super::dynamic::{DynNode, GosDyn};
use super::map::GosMap;
use super::vec::GosVec;

/// Binding-side `Bytes`: a heap header over a packed byte buffer.
#[repr(C)]
pub struct GosBytes {
    /// Byte length.
    pub len: i64,
    /// Allocated capacity.
    pub cap: i64,
    /// Byte buffer.
    pub ptr: *mut u8,
}

/// Binding-side `Map<K, V>`: parallel key and value vectors, paired
/// by index.
#[repr(C)]
pub struct BindingGosMap {
    /// Keys.
    pub keys: *mut GosVec,
    /// Values.
    pub values: *mut GosVec,
}

/// Binding-side dynamic variant: a name plus a tagged field array.
/// A `#[derive(GosStruct)]` struct crosses in this shape, its fields
/// positional and in declaration order.
#[repr(C)]
pub struct GosDynVariant {
    /// Arm name, NUL-terminated.
    pub name: *const c_char,
    /// Field count.
    pub payload_len: i32,
    /// Explicit padding so `payload` lands on its 8-byte offset.
    pub pad: i32,
    /// Field array.
    pub payload: *mut GosVariantValue,
}

/// Binding-side tuple: a field count and a tagged field array.
#[repr(C)]
pub struct GosTuple {
    /// Field count.
    pub len: i32,
    /// Field array.
    pub fields: *mut GosVariantValue,
}

/// One binding-side tuple or variant field: a kind tag and a word.
#[repr(C)]
pub struct GosVariantValue {
    /// Field kind - see [`wire_tag`].
    pub tag: i32,
    /// Explicit padding so `data` lands on its 8-byte offset.
    pub pad: i32,
    /// The field's word: an integer, a bit pattern, or a pointer.
    pub data: u64,
}

/// Field kinds in the binding ABI's tagged word.
pub mod wire_tag {
    /// `i64`, and every narrower integer.
    pub const I64: i32 = 0;
    /// `f64`, carried as its bit pattern.
    pub const F64: i32 = 1;
    /// `bool`.
    pub const BOOL: i32 = 2;
    /// `char`.
    pub const CHAR: i32 = 3;
    /// A runtime string; a value a binding answers hands it over.
    pub const STRING: i32 = 4;
}

/// A sequence field, carried as a `GosVec` of widened words.
const WIRE_TAG_VEC: i32 = 5;
/// A nested variant field.
const WIRE_TAG_VARIANT: i32 = 6;
/// A tuple field, which a list and a map both ride on the wire.
const WIRE_TAG_TUPLE: i32 = 7;

/// Binding-side `Option` / `Result`: a tag, then a tagged field array.
#[repr(C)]
pub struct GosVariant {
    /// `1` for `Some` / `Ok`, `0` for `None` / `Err`.
    pub tag: i32,
    /// Field count.
    pub payload_len: i32,
    /// Field array.
    pub payload: *mut GosVariantValue,
}

/// Frees a field array a binding answered, laid out as the `Box<[T]>` the
/// binding's host allocator made, without freeing what its fields hold.
///
/// # Safety
/// `fields` is null or a field array of `len` values a binding answered,
/// handed back once.
pub(crate) unsafe fn free_wire_fields(fields: *mut GosVariantValue, len: usize) {
    if !fields.is_null() && len > 0 {
        // SAFETY: the caller's contract.
        drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(fields, len)) });
    }
}

/// Frees what one field of a dynamic value a binding answered owns: a
/// string, a vector, a nested arm, or a tuple, all the way down.
///
/// # Safety
/// `field` is a field of a value a binding answered that nothing else owns.
unsafe fn free_wire_field(field: &GosVariantValue) {
    let word = field.data as usize;
    if word == 0 {
        return;
    }
    // SAFETY: each pointer kind is the allocation the binding made for it,
    // owned by this field alone.
    unsafe {
        match field.tag {
            wire_tag::STRING => {
                super::string::gos_rt_str_free(std::ptr::with_exposed_provenance_mut(word));
            }
            WIRE_TAG_VEC => {
                super::map::gos_rt_vec_free(std::ptr::with_exposed_provenance_mut(word));
            }
            WIRE_TAG_VARIANT => free_wire_dyn(std::ptr::with_exposed_provenance_mut(word)),
            WIRE_TAG_TUPLE => free_wire_tuple(std::ptr::with_exposed_provenance_mut(word)),
            _ => {}
        }
    }
}

/// Frees every field of `fields[..len]`, then the array.
///
/// # Safety
/// As for [`free_wire_fields`], for fields nothing else owns.
unsafe fn free_wire_fields_deep(fields: *mut GosVariantValue, len: usize) {
    if fields.is_null() {
        return;
    }
    // SAFETY: the caller's contract.
    for field in unsafe { std::slice::from_raw_parts(fields, len) } {
        // SAFETY: as above.
        unsafe { free_wire_field(field) };
    }
    // SAFETY: as above.
    unsafe { free_wire_fields(fields, len) };
}

/// Frees a tuple a binding answered and everything it holds.
///
/// # Safety
/// `t` is null or a tuple a binding answered, handed back once.
unsafe fn free_wire_tuple(t: *mut GosTuple) {
    if t.is_null() {
        return;
    }
    // SAFETY: the caller's contract.
    let tuple = unsafe { Box::from_raw(t) };
    // SAFETY: the tuple's fields are its own.
    unsafe { free_wire_fields_deep(tuple.fields, usize::try_from(tuple.len.max(0)).unwrap_or(0)) };
}

/// Frees a dynamic arm a binding answered: its name and everything its
/// payload holds.
///
/// # Safety
/// `v` is null or an arm a binding answered, handed back once.
unsafe fn free_wire_dyn(v: *mut GosDynVariant) {
    if v.is_null() {
        return;
    }
    // SAFETY: the caller's contract.
    let wire = unsafe { Box::from_raw(v) };
    // SAFETY: the name and the payload are the arm's own.
    unsafe {
        super::string::gos_rt_str_free(wire.name.cast_mut());
        free_wire_fields_deep(
            wire.payload,
            usize::try_from(wire.payload_len.max(0)).unwrap_or(0),
        );
    }
}

/// Element kinds a map's keys and values are converted through.
mod map_kind {
    /// A runtime `String`. Every other kind is an 8-byte word.
    pub(super) const STRING: i64 = 1;
}

/// Builds a `Bytes` wire header for a runtime byte vector. The
/// binding owns the returned header and buffer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_binding_bytes_from_vec(v: *mut GosVec) -> *mut GosBytes {
    ffi_entry!({
        // SAFETY: `v` is this shim's argument, null or live for the call (C-ABI contract), which
        // `vec_bytes` accepts.
        let bytes = unsafe { crate::c_abi::vec::vec_bytes(v) };
        let len = bytes.len() as i64;
        let mut boxed = bytes.into_boxed_slice();
        let ptr = boxed.as_mut_ptr();
        std::mem::forget(boxed);
        Box::into_raw(Box::new(GosBytes { len, cap: len, ptr }))
    })
}

/// Frees a `Bytes` argument [`gos_rt_binding_bytes_from_vec`] built, once
/// the binding call that read it returns; the binding copies what it keeps.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_binding_bytes_free(b: *mut GosBytes) {
    ffi_entry!({
        if b.is_null() {
            return;
        }
        // SAFETY: `b` is the header `gos_rt_binding_bytes_from_vec` boxed, handed back once.
        let header = unsafe { Box::from_raw(b) };
        let len = usize::try_from(header.len.max(0)).unwrap_or(0);
        if !header.ptr.is_null() {
            // SAFETY: the buffer is the boxed slice of `len` bytes built beside the header.
            drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(header.ptr, len)) });
        }
    });
}

/// Frees a tuple argument [`gos_rt_binding_tuple_from_slots`] built, once
/// the binding call that read it returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_binding_tuple_free(t: *mut GosTuple) {
    ffi_entry!({
        if t.is_null() {
            return;
        }
        // SAFETY: `t` is the tuple `gos_rt_binding_tuple_from_slots` boxed, handed back once.
        let tuple = unsafe { Box::from_raw(t) };
        let len = usize::try_from(tuple.len.max(0)).unwrap_or(0);
        if !tuple.fields.is_null() {
            // SAFETY: the fields are the boxed slice of `len` values built beside the tuple.
            drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(tuple.fields, len)) });
        }
    });
}

/// Frees a struct argument [`gos_rt_binding_struct_from_slots`] built, once
/// the binding call that read it returns. The name it borrows stays the
/// caller's.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_binding_struct_free(v: *mut GosDynVariant) {
    ffi_entry!({
        if v.is_null() {
            return;
        }
        // SAFETY: `v` is the variant `gos_rt_binding_struct_from_slots` boxed, handed back once.
        let wire = unsafe { Box::from_raw(v) };
        let len = usize::try_from(wire.payload_len.max(0)).unwrap_or(0);
        if !wire.payload.is_null() {
            // SAFETY: the payload is the boxed slice of `len` fields `wire_fields_from_slots` built.
            drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(wire.payload, len)) });
        }
    });
}

/// Materialises a runtime `Vec<i64>` from a `Bytes` a binding
/// returned, widening each byte to the slot the compiled tiers read,
/// and reclaims the wire header and its buffer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_binding_bytes_to_vec(b: *mut GosBytes) -> *mut GosVec {
    ffi_entry!({
        let out = super::vec::gos_rt_vec_new(8);
        if b.is_null() {
            return out;
        }
        // SAFETY: `b` is non-null (checked above), a `Bytes` header the binding boxed and hands
        // back, which this call consumes (C-ABI contract).
        let header = unsafe { Box::from_raw(b) };
        let len = usize::try_from(header.len.max(0)).unwrap_or(0);
        if !header.ptr.is_null() && len > 0 {
            // SAFETY: the header's buffer is non-null (checked above) and holds `len` bytes.
            let bytes = unsafe { std::slice::from_raw_parts(header.ptr, len) };
            for byte in bytes {
                // SAFETY: `out` is the fresh vec made above, or null, which `gos_rt_vec_push_i64`
                // accepts.
                unsafe { super::vec::gos_rt_vec_push_i64(out, i64::from(*byte)) };
            }
            let cap = usize::try_from(header.cap.max(header.len)).unwrap_or(len);
            // SAFETY: the buffer is the binding's `Vec<u8>` allocation of `len` bytes within
            // `cap`, owned by the consumed header.
            drop(unsafe { Vec::from_raw_parts(header.ptr, len, cap) });
        }
        out
    })
}

/// Snapshots a runtime map into the parallel-vector wire shape a
/// binding parameter reads.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_binding_map_from_map(
    m: *mut GosMap,
    key_kind: i64,
    value_kind: i64,
) -> *mut BindingGosMap {
    ffi_entry!({
        let keys = if key_kind == map_kind::STRING {
            // SAFETY: `m` is this shim's argument, live for the call (C-ABI contract) or null,
            // which `gos_rt_map_keys_str` accepts.
            unsafe { super::map::gos_rt_map_keys_str(m) }
        } else {
            // SAFETY: `m` is this shim's argument, null or live for the call (C-ABI contract),
            // which `gos_rt_map_keys_i64` accepts.
            unsafe { super::map::gos_rt_map_keys_i64(m) }
        };
        let values = if value_kind == map_kind::STRING {
            // SAFETY: `m` is this shim's argument, live for the call (C-ABI contract) or null,
            // which `gos_rt_map_values_str` accepts.
            unsafe { super::map::gos_rt_map_values_str(m) }
        } else {
            // SAFETY: `m` is this shim's argument, null or live for the call (C-ABI contract),
            // which `gos_rt_map_values_i64` accepts.
            unsafe { super::map::gos_rt_map_values_i64(m) }
        };
        Box::into_raw(Box::new(BindingGosMap { keys, values }))
    })
}

/// Builds a runtime map from the parallel-vector wire shape a
/// binding returned, and reclaims the wire header.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_binding_map_to_map(
    bm: *mut BindingGosMap,
    key_kind: i64,
    value_kind: i64,
) -> *mut GosMap {
    ffi_entry!({
        let key_bytes = 8;
        let out = super::map::gos_rt_map_new(key_bytes, 8);
        if bm.is_null() {
            return out;
        }
        // SAFETY: `bm` is non-null (checked above), a wire map the binding answered, which this
        // call reads and then frees (C-ABI contract).
        let wire = unsafe { &*bm };
        // SAFETY: the wire map's key vector is null or live, which `gos_rt_vec_len` accepts.
        let count = unsafe { super::vec::gos_rt_vec_len(wire.keys) }
            // SAFETY: the wire map's value vector is null or live, which `gos_rt_vec_len`
            // accepts.
            .min(unsafe { super::vec::gos_rt_vec_len(wire.values) });
        for index in 0..count {
            // SAFETY: `index` is below both vectors' lengths, a key or value word is a string
            // body or a scalar as its kind names, and `out` is the fresh map made above.
            unsafe {
                let key_word = super::signal::gos_rt_vec_get_i64(wire.keys, index);
                let value_word = super::signal::gos_rt_vec_get_i64(wire.values, index);
                match (key_kind, value_kind) {
                    (k, v) if k == map_kind::STRING && v == map_kind::STRING => {
                        super::map::gos_rt_map_insert_str_str(
                            out,
                            key_word as *const c_char,
                            value_word as *const c_char,
                        );
                    }
                    (k, _) if k == map_kind::STRING => {
                        super::map::gos_rt_map_insert_str_i64(
                            out,
                            key_word as *const c_char,
                            value_word,
                        );
                    }
                    (_, v) if v == map_kind::STRING => {
                        super::map::gos_rt_map_insert_i64_str(
                            out,
                            key_word,
                            value_word as *const c_char,
                        );
                    }
                    _ => super::map::gos_rt_map_insert_i64_i64(out, key_word, value_word),
                }
            }
        }
        // The map copied every key and value, so the wire map and the strings
        // its halves own go.
        // SAFETY: `bm` is the wire map read above, not read again.
        unsafe { super::map::gos_rt_binding_map_free(bm.cast()) };
        out
    })
}

/// One field kind per element, unpacked from the byte-per-element
/// stream the compiled tiers pass as a single word.
fn packed_tags(tags: i64, n: usize) -> Vec<i32> {
    (0..n)
        .map(|index| ((tags >> (index * 8)) & 0xff) as i32)
        .collect()
}

/// Builds a tuple wire value from a run of 8-byte slots. `tags` packs
/// one [`wire_tag`] byte per element, least-significant byte first.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_binding_tuple_from_slots(
    slots: *const i64,
    n: i64,
    tags: i64,
) -> *mut GosTuple {
    ffi_entry!({
        let count = usize::try_from(n.max(0)).unwrap_or(0);
        if slots.is_null() || count == 0 {
            return Box::into_raw(Box::new(GosTuple {
                len: 0,
                fields: std::ptr::null_mut(),
            }));
        }
        // SAFETY: `slots` is non-null (checked above) and addresses `count` words (C-ABI
        // contract).
        let words = unsafe { std::slice::from_raw_parts(slots, count) };
        let kinds = packed_tags(tags, count);
        let mut fields: Vec<GosVariantValue> = Vec::with_capacity(count);
        for (word, kind) in words.iter().zip(kinds) {
            // A runtime String is already NUL-terminated, so a string
            // field crosses as the pointer the slot holds; the binding
            // reads it to its terminator.
            fields.push(GosVariantValue {
                tag: kind,
                pad: 0,
                data: *word as u64,
            });
        }
        let mut boxed = fields.into_boxed_slice();
        let fields_ptr = boxed.as_mut_ptr();
        std::mem::forget(boxed);
        Box::into_raw(Box::new(GosTuple {
            len: count as i32,
            fields: fields_ptr,
        }))
    })
}

/// The tuple (`shape` 0) or struct (`shape` 1) a binding answered, as its
/// field array and field count.
///
/// # Safety
/// `wire` is null or a value of `shape` a binding answered, not yet freed.
unsafe fn wire_fields(wire: *mut u8, shape: i64) -> (*mut GosVariantValue, usize) {
    if wire.is_null() {
        return (std::ptr::null_mut(), 0);
    }
    // SAFETY: the caller's contract names the layout.
    let (fields, len) = unsafe {
        if shape == 0 {
            let tuple = &*wire.cast::<GosTuple>();
            (tuple.fields, tuple.len)
        } else {
            let arm = &*wire.cast::<GosDynVariant>();
            (arm.payload, arm.payload_len)
        }
    };
    (fields, usize::try_from(len.max(0)).unwrap_or(0))
}

/// The word of field `index`, or `None` past the fields.
///
/// # Safety
/// As for [`wire_fields`].
unsafe fn wire_field_word(wire: *mut u8, shape: i64, index: i64) -> Option<u64> {
    // SAFETY: the caller's contract.
    let (fields, len) = unsafe { wire_fields(wire, shape) };
    let index = usize::try_from(index).ok().filter(|i| *i < len)?;
    // SAFETY: `index` is below the field count.
    Some(unsafe { (*fields.add(index)).data })
}

/// The word field `index` of the tuple (`shape` 0) or struct (`shape` 1) a
/// binding answered holds: an integer or a `char`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_binding_field_word(wire: *mut u8, shape: i64, index: i64) -> i64 {
    // SAFETY: `wire` is a value of `shape` a binding answered (C-ABI contract).
    ffi_entry!({ unsafe { wire_field_word(wire, shape, index) }.map_or(0, |w| w as i64) })
}

/// [`gos_rt_binding_field_word`] for a `String` field: the runtime string the
/// binding answered, whose share the caller takes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_binding_field_str(
    wire: *mut u8,
    shape: i64,
    index: i64,
) -> *mut c_char {
    ffi_entry!({
        // SAFETY: as for `gos_rt_binding_field_word`.
        unsafe { wire_field_word(wire, shape, index) }.map_or(std::ptr::null_mut(), |w| {
            std::ptr::with_exposed_provenance_mut(w as usize)
        })
    })
}

/// [`gos_rt_binding_field_word`] for an `f64` field.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_binding_field_f64(wire: *mut u8, shape: i64, index: i64) -> f64 {
    // SAFETY: as for `gos_rt_binding_field_word`.
    ffi_entry!({ unsafe { wire_field_word(wire, shape, index) }.map_or(0.0, f64::from_bits) })
}

/// [`gos_rt_binding_field_word`] for a `bool` field, `1` for true. The wire
/// writes one byte for a boolean, and the rest of the word is undefined.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_binding_field_bool(wire: *mut u8, shape: i64, index: i64) -> i8 {
    ffi_entry!({
        // SAFETY: as for `gos_rt_binding_field_word`.
        i8::from(unsafe { wire_field_word(wire, shape, index) }.is_some_and(|w| w & 0xff != 0))
    })
}

/// Frees the tuple (`shape` 0) or struct (`shape` 1) a binding answered once
/// the caller took its first `taken` fields: what any later field owns, the
/// field array, the struct's name, and the value itself.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_binding_fields_free(wire: *mut u8, shape: i64, taken: i64) {
    ffi_entry!({
        if wire.is_null() {
            return;
        }
        // SAFETY: `wire` is a value of `shape` a binding answered, handed back once (C-ABI
        // contract); the caller owns the fields it took, and the rest are the value's own.
        unsafe {
            let (fields, len) = wire_fields(wire, shape);
            let taken = usize::try_from(taken.max(0)).unwrap_or(0).min(len);
            if !fields.is_null() {
                for field in std::slice::from_raw_parts(fields.add(taken), len - taken) {
                    free_wire_field(field);
                }
            }
            free_wire_fields(fields, len);
            if shape == 0 {
                drop(Box::from_raw(wire.cast::<GosTuple>()));
            } else {
                let arm = Box::from_raw(wire.cast::<GosDynVariant>());
                super::string::gos_rt_str_free(arm.name.cast_mut());
            }
        }
    });
}

/// Builds the tagged field array shared by the tuple and struct wire
/// shapes from a run of 8-byte slots.
unsafe fn wire_fields_from_slots(
    slots: *const i64,
    count: usize,
    tags: i64,
) -> *mut GosVariantValue {
    if slots.is_null() || count == 0 {
        return std::ptr::null_mut();
    }
    // SAFETY: `slots` is non-null (checked above), and this `unsafe fn`'s caller passes it
    // addressing `count` words.
    let words = unsafe { std::slice::from_raw_parts(slots, count) };
    let kinds = packed_tags(tags, count);
    let fields: Vec<GosVariantValue> = words
        .iter()
        .zip(kinds)
        .map(|(word, kind)| GosVariantValue {
            tag: kind,
            pad: 0,
            // A runtime String is already NUL-terminated, so a string
            // field crosses as the pointer the slot holds.
            data: *word as u64,
        })
        .collect();
    let mut boxed = fields.into_boxed_slice();
    let ptr = boxed.as_mut_ptr();
    std::mem::forget(boxed);
    ptr
}

/// Builds the wire value a `#[derive(GosStruct)]` parameter reads from
/// a struct's run of 8-byte slots. `name` is the struct's own name, as
/// a runtime `String`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_binding_struct_from_slots(
    name: *const c_char,
    slots: *const i64,
    n: i64,
    tags: i64,
) -> *mut GosDynVariant {
    ffi_entry!({
        let count = usize::try_from(n.max(0)).unwrap_or(0);
        // SAFETY: this shim's arguments, under the C-ABI contract, give `slots` null or
        // addressing `count` words.
        let payload = unsafe { wire_fields_from_slots(slots, count, tags) };
        Box::into_raw(Box::new(GosDynVariant {
            name,
            payload_len: count as i32,
            pad: 0,
            payload,
        }))
    })
}

/// Reads one binding-side payload field as the dynamic value it stands for,
/// mirroring the reader the interpreter uses so both tiers rebuild the same
/// value from the same wire bytes.
unsafe fn dyn_from_wire_field(field: &GosVariantValue) -> DynNode {
    let word = field.data as i64;
    match field.tag {
        wire_tag::I64 => DynNode::Int(word),
        wire_tag::F64 => DynNode::Float(f64::from_bits(field.data)),
        // The wire union writes one byte for a boolean; the rest of the
        // word is whatever the union's widest member left there.
        wire_tag::BOOL => DynNode::Bool(field.data & 0xff != 0),
        wire_tag::CHAR => char::from_u32(field.data as u32).map_or(DynNode::Nil, DynNode::Char),
        wire_tag::STRING => {
            if word == 0 {
                return DynNode::Str(String::new());
            }
            // The node takes a copy; the string goes with the wire value.
            // SAFETY: a non-null `STRING` field is a NUL-terminated runtime string.
            let text = unsafe { CStr::from_ptr(word as *const c_char) };
            DynNode::Str(text.to_string_lossy().into_owned())
        }
        // A sequence field carries widened bytes. All-in-range reads as the
        // byte buffer it stands for, exactly as the interpreter reads it.
        WIRE_TAG_VEC => {
            let vec: *const GosVec = std::ptr::with_exposed_provenance(word as usize);
            // SAFETY: a `VEC` field's word is a live `GosVec` or null, which `vec_words` accepts.
            let words = unsafe { vec_words(vec) };
            if words.iter().all(|w| (0..=255).contains(w)) {
                DynNode::Bytes(words.iter().map(|w| *w as u8).collect())
            } else {
                DynNode::List(
                    words
                        .into_iter()
                        .map(|w| Arc::new(DynNode::Int(w)))
                        .collect(),
                )
            }
        }
        WIRE_TAG_VARIANT => {
            let nested: *const GosDynVariant = std::ptr::with_exposed_provenance(word as usize);
            // SAFETY: a `VARIANT` field's word is a live wire variant or null, which
            // `dyn_from_wire_variant` accepts.
            unsafe { dyn_from_wire_variant(nested) }
        }
        WIRE_TAG_TUPLE => {
            let tuple: *const GosTuple = std::ptr::with_exposed_provenance(word as usize);
            // SAFETY: a `TUPLE` field's word is a live wire tuple or null, which
            // `dyn_from_wire_tuple` accepts.
            DynNode::List(unsafe { dyn_from_wire_tuple(tuple) })
        }
        _ => DynNode::Nil,
    }
}

/// A sequence field's elements as words.
unsafe fn vec_words(v: *const GosVec) -> Vec<i64> {
    // SAFETY: this `unsafe fn`'s caller passes `v` null or a live `Vec`.
    unsafe { crate::c_abi::vec::VecView::of(v) }.map_or_else(Vec::new, |vec| vec.words().collect())
}

unsafe fn dyn_from_wire_tuple(t: *const GosTuple) -> Vec<Arc<DynNode>> {
    if t.is_null() {
        return Vec::new();
    }
    // SAFETY: `t` is non-null (checked above), and this `unsafe fn`'s caller passes a live
    // `GosTuple`.
    let tuple = unsafe { &*t };
    let len = usize::try_from(tuple.len.max(0)).unwrap_or(0);
    if tuple.fields.is_null() || len == 0 {
        return Vec::new();
    }
    // SAFETY: `tuple.fields` is non-null (checked above) and holds `len` fields.
    let fields = unsafe { std::slice::from_raw_parts(tuple.fields, len) };
    fields
        .iter()
        // SAFETY: each field of a live wire tuple holds a word of the kind its tag names.
        .map(|field| Arc::new(unsafe { dyn_from_wire_field(field) }))
        .collect()
}

unsafe fn dyn_from_wire_variant(v: *const GosDynVariant) -> DynNode {
    if v.is_null() {
        return DynNode::Nil;
    }
    // SAFETY: `v` is non-null (checked above), and this `unsafe fn`'s caller passes a live
    // `GosDynVariant`.
    let wire = unsafe { &*v };
    // The arm name is a runtime string, NUL-terminated.
    let name = if wire.name.is_null() {
        String::new()
    } else {
        // SAFETY: a non-null arm name is a NUL-terminated C string the binding owns.
        unsafe { CStr::from_ptr(wire.name) }
            .to_string_lossy()
            .into_owned()
    };
    let len = usize::try_from(wire.payload_len.max(0)).unwrap_or(0);
    let payload: Vec<Arc<DynNode>> = if wire.payload.is_null() || len == 0 {
        Vec::new()
    } else {
        // SAFETY: `wire.payload` is non-null (checked above) and holds `len` fields.
        let fields = unsafe { std::slice::from_raw_parts(wire.payload, len) };
        fields
            .iter()
            // SAFETY: each field of a live wire variant holds a word of the kind its tag names.
            .map(|field| Arc::new(unsafe { dyn_from_wire_field(field) }))
            .collect()
    };
    unbare_wire_arm(&name, payload)
}

/// Gives back the value a `$`-headed wire arm stands for, and every other arm
/// as the named arm it is. A value that is not a named arm crosses under one
/// of these names - `$` is not an identifier character, so a declared arm can
/// never collide with one - and the reader has to agree with the writer.
fn unbare_wire_arm(name: &str, mut payload: Vec<Arc<DynNode>>) -> DynNode {
    let first = |payload: &mut Vec<Arc<DynNode>>| {
        if payload.is_empty() {
            DynNode::Nil
        } else {
            (*payload.remove(0)).clone()
        }
    };
    match name {
        "$Nil" => DynNode::Nil,
        "$Bool" | "$Int" | "$Float" | "$Char" | "$String" | "$Bytes" | "$Map" => {
            first(&mut payload)
        }
        "$List" => DynNode::List(payload),
        _ => DynNode::Tagged {
            name: name.to_string(),
            payload,
        },
    }
}

/// Builds the `DynValue` a binding returned from its wire variant, so a
/// compiled build reads the value the interpreter reads rather than the
/// pointer that reaches it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_dyn_from_binding_variant(v: *mut GosDynVariant) -> *mut GosDyn {
    ffi_entry!({
        // SAFETY: `v` is this shim's argument, a value the binding answered or null, which
        // `dyn_from_wire_variant` reads; the value read is a copy, so the wire value goes.
        unsafe {
            let read = dyn_from_wire_variant(v);
            free_wire_dyn(v);
            GosDyn::into_raw(read)
        }
    })
}
