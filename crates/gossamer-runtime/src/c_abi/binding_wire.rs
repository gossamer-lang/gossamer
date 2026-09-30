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
    /// A NUL-terminated C string the binding owns.
    pub const STRING: i32 = 4;
}

/// A sequence field, carried as a `GosVec` of widened words.
const WIRE_TAG_VEC: i32 = 5;
/// A nested variant field.
const WIRE_TAG_VARIANT: i32 = 6;
/// A tuple field, which a list and a map both ride on the wire.
const WIRE_TAG_TUPLE: i32 = 7;

/// Element kinds a map's keys and values are converted through.
mod map_kind {
    /// A runtime `String`. Every other kind is an 8-byte word.
    pub(super) const STRING: i64 = 1;
}

/// Builds a `Bytes` wire header for a runtime byte vector. The
/// binding owns the returned header and buffer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_binding_bytes_from_vec(v: *mut GosVec) -> *mut GosBytes {
    ffi_entry!(std::ptr::null_mut(), {
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

/// Materialises a runtime `Vec<i64>` from a `Bytes` a binding
/// returned, widening each byte to the slot the compiled tiers read,
/// and reclaims the wire header and its buffer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_binding_bytes_to_vec(b: *mut GosBytes) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
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
    ffi_entry!(std::ptr::null_mut(), {
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
    ffi_entry!(std::ptr::null_mut(), {
        let key_bytes = 8;
        let out = super::map::gos_rt_map_new(key_bytes, 8);
        if bm.is_null() {
            return out;
        }
        // SAFETY: `bm` is non-null (checked above), a wire map the binding boxed and hands back,
        // which this call consumes (C-ABI contract).
        let wire = unsafe { Box::from_raw(bm) };
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
    ffi_entry!(std::ptr::null_mut(), {
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

/// Writes a tuple a binding returned into a run of 8-byte slots, and
/// reclaims the wire value. `tags` packs one [`wire_tag`] byte per
/// element, least-significant byte first.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_binding_tuple_to_slots(
    t: *mut GosTuple,
    out: *mut i64,
    n: i64,
    tags: i64,
) {
    ffi_entry!((), {
        let count = usize::try_from(n.max(0)).unwrap_or(0);
        if out.is_null() || count == 0 {
            return;
        }
        // SAFETY: `out` is non-null (checked above) and addresses `count` words (C-ABI contract).
        let slots = unsafe { std::slice::from_raw_parts_mut(out, count) };
        for slot in slots.iter_mut() {
            *slot = 0;
        }
        if t.is_null() {
            return;
        }
        // SAFETY: `t` is non-null (checked above), a tuple the binding boxed and hands back,
        // which this call consumes (C-ABI contract).
        let tuple = unsafe { Box::from_raw(t) };
        let available = usize::try_from(tuple.len.max(0)).unwrap_or(0);
        // SAFETY: `tuple.fields` holds `available` fields, and `out` addresses `count` words.
        unsafe { wire_fields_to_slots(tuple.fields, available, out, count, tags) };
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

/// Writes a tagged field array into a run of 8-byte slots, giving each
/// string field a runtime `String` of its own.
unsafe fn wire_fields_to_slots(
    fields: *const GosVariantValue,
    available: usize,
    out: *mut i64,
    count: usize,
    tags: i64,
) {
    if fields.is_null() || out.is_null() {
        return;
    }
    // SAFETY: `out` is non-null (checked above), and this `unsafe fn`'s caller passes it
    // addressing `count` words.
    let slots = unsafe { std::slice::from_raw_parts_mut(out, count) };
    // SAFETY: `fields` is non-null (checked above), and this `unsafe fn`'s caller passes it
    // holding `available` fields; the read stays within both counts.
    let read = unsafe { std::slice::from_raw_parts(fields, available.min(count)) };
    let kinds = packed_tags(tags, count);
    for (index, field) in read.iter().enumerate() {
        let word = field.data as i64;
        slots[index] = if kinds[index] == wire_tag::STRING && word != 0 {
            // HOST-CSTRING: a native Rust binding owns this pointer and
            // publishes it as a NUL-terminated C string, not a Gossamer
            // `String`, so it carries no length header. The slot needs a
            // runtime String of its own.
            // SAFETY: a non-null `STRING` field is a NUL-terminated C string the binding owns for
            // the call.
            let text = unsafe { CStr::from_ptr(word as *const c_char) };
            super::string::alloc_cstring(text.to_bytes()) as i64
        } else {
            word
        };
    }
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
    ffi_entry!(std::ptr::null_mut(), {
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

/// Writes a struct a binding returned into a run of 8-byte slots, and
/// reclaims the wire value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_binding_struct_to_slots(
    v: *mut GosDynVariant,
    out: *mut i64,
    n: i64,
    tags: i64,
) {
    ffi_entry!((), {
        let count = usize::try_from(n.max(0)).unwrap_or(0);
        if out.is_null() || count == 0 {
            return;
        }
        // SAFETY: `out` is non-null (checked above) and addresses `count` words (C-ABI contract).
        let slots = unsafe { std::slice::from_raw_parts_mut(out, count) };
        for slot in slots.iter_mut() {
            *slot = 0;
        }
        if v.is_null() {
            return;
        }
        // SAFETY: `v` is non-null (checked above), a variant the binding boxed and hands back,
        // which this call consumes (C-ABI contract).
        let wire = unsafe { Box::from_raw(v) };
        let available = usize::try_from(wire.payload_len.max(0)).unwrap_or(0);
        // SAFETY: `wire.payload` holds `available` fields, and `out` addresses `count` words.
        unsafe { wire_fields_to_slots(wire.payload, available, out, count, tags) };
    });
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
            // HOST-CSTRING: the binding owns this pointer and publishes it as
            // a NUL-terminated C string with no length header.
            // SAFETY: a non-null `STRING` field is a NUL-terminated C string the binding owns for
            // the call.
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
    // HOST-CSTRING: the binding arena-allocates an arm name as a plain C
    // string, which carries no length header.
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
pub unsafe extern "C" fn gos_rt_dyn_from_binding_variant(v: *const GosDynVariant) -> *mut GosDyn {
    ffi_entry!(std::ptr::null_mut(), {
        // SAFETY: `v` is this shim's argument, live for the call (C-ABI contract) or null, which
        // `dyn_from_wire_variant` accepts.
        GosDyn::into_raw(unsafe { dyn_from_wire_variant(v) })
    })
}
