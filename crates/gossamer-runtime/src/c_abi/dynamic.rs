//! `DynValue` - a value whose shape is decided by the data rather than by a
//! declaration.
//!
//! A decoder reading a wire format, a database column typed by its own
//! metadata, or a Rust binding returning one arm of a set it names at run
//! time all produce a value no declared enum covers. `DynValue` is that
//! value: `Nil | Bool | Int | Float | Char | String | Bytes | List | Map |
//! Tagged { name, payload }`, where a tagged arm's name is a runtime string.
//!
//! The node is shared and immutable, so passing one around costs a refcount
//! bump. Every tier reaches it through this same family, so a construction,
//! an inspection, and a comparison answer the same thing on the bytecode VM
//! as in a native build.

use std::ffi::c_char;
use std::sync::Arc;

use crate::c_abi::string::alloc_cstring;
use crate::c_abi::vec::GosVec;

/// One `DynValue` node. Shared by `Arc`, so a list or a tagged payload holds
/// its children without copying them.
#[derive(Debug, Clone, PartialEq)]
pub enum DynNode {
    /// The absent value.
    Nil,
    /// A boolean.
    Bool(bool),
    /// A signed 64-bit integer.
    Int(i64),
    /// A double.
    Float(f64),
    /// A Unicode scalar.
    Char(char),
    /// Text.
    Str(String),
    /// A byte buffer.
    Bytes(Vec<u8>),
    /// A positional sequence.
    List(Vec<Arc<DynNode>>),
    /// Key/value pairs, in the order they were built.
    Map(Vec<(Arc<DynNode>, Arc<DynNode>)>),
    /// A named arm and its positional payload.
    Tagged {
        /// Arm name, decided at run time.
        name: String,
        /// Positional payload.
        payload: Vec<Arc<DynNode>>,
    },
}

/// The kind tags a program reads a node's shape through. Stable across tiers:
/// the interpreter and both compiled backends answer the same number.
pub mod dyn_kind {
    /// [`super::DynNode::Nil`].
    pub const NIL: i64 = 0;
    /// [`super::DynNode::Bool`].
    pub const BOOL: i64 = 1;
    /// [`super::DynNode::Int`].
    pub const INT: i64 = 2;
    /// [`super::DynNode::Float`].
    pub const FLOAT: i64 = 3;
    /// [`super::DynNode::Char`].
    pub const CHAR: i64 = 4;
    /// [`super::DynNode::Str`].
    pub const STRING: i64 = 5;
    /// [`super::DynNode::Bytes`].
    pub const BYTES: i64 = 6;
    /// [`super::DynNode::List`].
    pub const LIST: i64 = 7;
    /// [`super::DynNode::Map`].
    pub const MAP: i64 = 8;
    /// [`super::DynNode::Tagged`].
    pub const TAGGED: i64 = 9;
}

/// The handle a compiled program holds a `DynValue` by.
pub struct GosDyn {
    node: Arc<DynNode>,
}

impl GosDyn {
    /// Wraps a node as a fresh handle.
    pub(crate) fn into_raw(node: DynNode) -> *mut GosDyn {
        Box::into_raw(Box::new(GosDyn {
            node: Arc::new(node),
        }))
    }

    fn share(node: &Arc<DynNode>) -> *mut GosDyn {
        Box::into_raw(Box::new(GosDyn {
            node: Arc::clone(node),
        }))
    }
}

/// The node a handle names, or `Nil` for a null handle.
unsafe fn node_of<'a>(v: *const GosDyn) -> &'a DynNode {
    static NIL: DynNode = DynNode::Nil;
    if v.is_null() {
        return &NIL;
    }
    unsafe { &(*v).node }
}

/// Reads a `Vec<DynValue>` argument as the nodes its handles name.
unsafe fn children_of(items: *const GosVec) -> Vec<Arc<DynNode>> {
    if items.is_null() {
        return Vec::new();
    }
    let header = unsafe { &*items };
    let len = usize::try_from(header.len.max(0)).unwrap_or(0);
    let data = header.ptr.as_ptr();
    if len == 0 || data.is_null() {
        return Vec::new();
    }
    let words = unsafe { std::slice::from_raw_parts(data.cast::<i64>(), len) };
    words
        .iter()
        .map(|word| {
            let handle: *const GosDyn = std::ptr::with_exposed_provenance(*word as usize);
            Arc::new(unsafe { node_of(handle) }.clone())
        })
        .collect()
}

/// Renders a node the way `{}` and `{:?}` show it. A tagged arm reads as its
/// name, with its payload in parentheses when it carries one.
fn render(node: &DynNode, out: &mut String) {
    use std::fmt::Write as _;
    match node {
        // The absent value reads as the unit the interpreter shows for it.
        DynNode::Nil => out.push_str("()"),
        DynNode::Bool(b) => {
            let _ = write!(out, "{b}");
        }
        DynNode::Int(n) => {
            let _ = write!(out, "{n}");
        }
        DynNode::Float(f) => {
            let _ = write!(out, "{}", crate::builtins::format_float_debug(*f));
        }
        DynNode::Char(c) => {
            let _ = write!(out, "{c}");
        }
        // The VM's Debug channel quotes strings (the spelling that
        // builds them), so every tier's `{:?}` of a `DynValue` quotes
        // its string payloads.
        DynNode::Str(s) => crate::c_abi::map::push_quoted_str(out, s),
        DynNode::Bytes(bytes) => {
            out.push('[');
            for (index, byte) in bytes.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                let _ = write!(out, "{byte}");
            }
            out.push(']');
        }
        DynNode::List(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                render(item, out);
            }
            out.push(']');
        }
        DynNode::Map(entries) => {
            out.push('{');
            for (index, (key, value)) in entries.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                render(key, out);
                out.push_str(": ");
                render(value, out);
            }
            out.push('}');
        }
        DynNode::Tagged { name, payload } => {
            out.push_str(name);
            if payload.is_empty() {
                return;
            }
            out.push('(');
            for (index, item) in payload.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                render(item, out);
            }
            out.push(')');
        }
    }
}

// --- construction ------------------------------------------------

/// `DynValue::nil()`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_dyn_nil() -> *mut GosDyn {
    ffi_entry!(std::ptr::null_mut(), { GosDyn::into_raw(DynNode::Nil) })
}

/// `DynValue::bool(b)`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_dyn_bool(value: i32) -> *mut GosDyn {
    ffi_entry!(std::ptr::null_mut(), {
        GosDyn::into_raw(DynNode::Bool(value != 0))
    })
}

/// `DynValue::int(n)`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_dyn_int(value: i64) -> *mut GosDyn {
    ffi_entry!(std::ptr::null_mut(), {
        GosDyn::into_raw(DynNode::Int(value))
    })
}

/// `DynValue::float(f)`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_dyn_float(value: f64) -> *mut GosDyn {
    ffi_entry!(std::ptr::null_mut(), {
        GosDyn::into_raw(DynNode::Float(value))
    })
}

/// `DynValue::char(c)`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_dyn_char(value: i32) -> *mut GosDyn {
    ffi_entry!(std::ptr::null_mut(), {
        let scalar = u32::try_from(value).ok().and_then(char::from_u32);
        GosDyn::into_raw(scalar.map_or(DynNode::Nil, DynNode::Char))
    })
}

/// `DynValue::string(s)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_dyn_string(text: *const c_char) -> *mut GosDyn {
    ffi_entry!(std::ptr::null_mut(), {
        let bytes = unsafe { crate::c_abi::gos_str_arg_bytes(text) };
        GosDyn::into_raw(DynNode::Str(String::from_utf8_lossy(bytes).into_owned()))
    })
}

/// `DynValue::bytes(buf)`, over a `Vec` whose elements are byte values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_dyn_bytes(items: *const GosVec) -> *mut GosDyn {
    ffi_entry!(std::ptr::null_mut(), {
        let mut out: Vec<u8> = Vec::new();
        if !items.is_null() {
            let header = unsafe { &*items };
            let len = usize::try_from(header.len.max(0)).unwrap_or(0);
            let data = header.ptr.as_ptr();
            if len > 0 && !data.is_null() {
                if header.elem_bytes == 1 {
                    out.extend_from_slice(unsafe { std::slice::from_raw_parts(data, len) });
                } else {
                    let words = unsafe { std::slice::from_raw_parts(data.cast::<i64>(), len) };
                    out.extend(words.iter().map(|w| (*w & 0xff) as u8));
                }
            }
        }
        GosDyn::into_raw(DynNode::Bytes(out))
    })
}

/// `DynValue::list(items)`, over a `Vec<DynValue>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_dyn_list(items: *const GosVec) -> *mut GosDyn {
    ffi_entry!(std::ptr::null_mut(), {
        GosDyn::into_raw(DynNode::List(unsafe { children_of(items) }))
    })
}

/// `DynValue::map(keys, values)`, over two parallel `Vec<DynValue>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_dyn_map(keys: *const GosVec, values: *const GosVec) -> *mut GosDyn {
    ffi_entry!(std::ptr::null_mut(), {
        let ks = unsafe { children_of(keys) };
        let vs = unsafe { children_of(values) };
        let entries = ks.into_iter().zip(vs).collect();
        GosDyn::into_raw(DynNode::Map(entries))
    })
}

/// `DynValue::tagged(name, payload)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_dyn_tagged(
    name: *const c_char,
    payload: *const GosVec,
) -> *mut GosDyn {
    ffi_entry!(std::ptr::null_mut(), {
        let bytes = unsafe { crate::c_abi::gos_str_arg_bytes(name) };
        GosDyn::into_raw(DynNode::Tagged {
            name: String::from_utf8_lossy(bytes).into_owned(),
            payload: unsafe { children_of(payload) },
        })
    })
}

// --- inspection --------------------------------------------------

/// The node's kind, per [`dyn_kind`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_dyn_kind(v: *const GosDyn) -> i64 {
    ffi_entry!(dyn_kind::NIL, {
        match unsafe { node_of(v) } {
            DynNode::Nil => dyn_kind::NIL,
            DynNode::Bool(_) => dyn_kind::BOOL,
            DynNode::Int(_) => dyn_kind::INT,
            DynNode::Float(_) => dyn_kind::FLOAT,
            DynNode::Char(_) => dyn_kind::CHAR,
            DynNode::Str(_) => dyn_kind::STRING,
            DynNode::Bytes(_) => dyn_kind::BYTES,
            DynNode::List(_) => dyn_kind::LIST,
            DynNode::Map(_) => dyn_kind::MAP,
            DynNode::Tagged { .. } => dyn_kind::TAGGED,
        }
    })
}

/// The value's kind as its own name: `nil`, `bool`, `int`, `float`, `char`,
/// `string`, `bytes`, `list`, `map`, `tagged`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_dyn_kind_name(v: *const GosDyn) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let name: &[u8] = match unsafe { node_of(v) } {
            DynNode::Nil => b"nil",
            DynNode::Bool(_) => b"bool",
            DynNode::Int(_) => b"int",
            DynNode::Float(_) => b"float",
            DynNode::Char(_) => b"char",
            DynNode::Str(_) => b"string",
            DynNode::Bytes(_) => b"bytes",
            DynNode::List(_) => b"list",
            DynNode::Map(_) => b"map",
            DynNode::Tagged { .. } => b"tagged",
        };
        alloc_cstring(name)
    })
}

/// A tagged arm's name, or the empty string for every other kind.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_dyn_name(v: *const GosDyn) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        match unsafe { node_of(v) } {
            DynNode::Tagged { name, .. } => alloc_cstring(name.as_bytes()),
            _ => alloc_cstring(b""),
        }
    })
}

/// How many values the node holds: a payload, a list's elements, a map's
/// entries, a byte buffer's bytes. Zero for every scalar.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_dyn_len(v: *const GosDyn) -> i64 {
    ffi_entry!(0, {
        let len = match unsafe { node_of(v) } {
            // Text counts its own scalars, the same length a `String`
            // reports; every other shape counts the values it holds.
            DynNode::Str(s) => s.chars().count(),
            DynNode::Bytes(bytes) => bytes.len(),
            DynNode::List(items) => items.len(),
            DynNode::Map(entries) => entries.len(),
            DynNode::Tagged { payload, .. } => payload.len(),
            _ => 0,
        };
        i64::try_from(len).unwrap_or(i64::MAX)
    })
}

/// The value at `index`: a payload field, a list element, or a map value.
/// Out of range answers `Nil`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_dyn_at(v: *const GosDyn, index: i64) -> *mut GosDyn {
    ffi_entry!(std::ptr::null_mut(), {
        let index = usize::try_from(index).unwrap_or(usize::MAX);
        let child = match unsafe { node_of(v) } {
            DynNode::List(items) => items.get(index),
            DynNode::Tagged { payload, .. } => payload.get(index),
            DynNode::Map(entries) => entries.get(index).map(|(_, value)| value),
            DynNode::Bytes(bytes) => {
                return bytes.get(index).map_or_else(
                    || GosDyn::into_raw(DynNode::Nil),
                    |byte| GosDyn::into_raw(DynNode::Int(i64::from(*byte))),
                );
            }
            _ => None,
        };
        child.map_or_else(|| GosDyn::into_raw(DynNode::Nil), GosDyn::share)
    })
}

/// A map's key at `index`; `Nil` for every other kind.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_dyn_key_at(v: *const GosDyn, index: i64) -> *mut GosDyn {
    ffi_entry!(std::ptr::null_mut(), {
        let index = usize::try_from(index).unwrap_or(usize::MAX);
        match unsafe { node_of(v) } {
            DynNode::Map(entries) => entries.get(index).map_or_else(
                || GosDyn::into_raw(DynNode::Nil),
                |(key, _)| GosDyn::share(key),
            ),
            _ => GosDyn::into_raw(DynNode::Nil),
        }
    })
}

/// The integer a node holds, as `Option<i64>` in the runtime's packed shape.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_dyn_as_i64(v: *const GosDyn) -> i128 {
    ffi_entry!(0, {
        match unsafe { node_of(v) } {
            DynNode::Int(n) => crate::c_abi::gos_rt_result_new(0, *n),
            _ => crate::c_abi::gos_rt_result_new(1, 0),
        }
    })
}

/// The float a node holds, as `Option<f64>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_dyn_as_f64(v: *const GosDyn) -> i128 {
    ffi_entry!(0, {
        match unsafe { node_of(v) } {
            DynNode::Float(f) => crate::c_abi::gos_rt_result_new_f64(0, *f),
            _ => crate::c_abi::gos_rt_result_new_f64(1, 0.0),
        }
    })
}

/// The boolean a node holds, as `Option<bool>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_dyn_as_bool(v: *const GosDyn) -> i128 {
    ffi_entry!(0, {
        match unsafe { node_of(v) } {
            DynNode::Bool(b) => crate::c_abi::gos_rt_result_new(0, i64::from(*b)),
            _ => crate::c_abi::gos_rt_result_new(1, 0),
        }
    })
}

/// The Unicode scalar a node holds, as `Option<char>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_dyn_as_char(v: *const GosDyn) -> i128 {
    ffi_entry!(0, {
        match unsafe { node_of(v) } {
            DynNode::Char(c) => crate::c_abi::gos_rt_result_new(0, i64::from(*c as u32)),
            _ => crate::c_abi::gos_rt_result_new(1, 0),
        }
    })
}

/// The text a node holds, as `Option<String>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_dyn_as_str(v: *const GosDyn) -> i128 {
    ffi_entry!(0, {
        match unsafe { node_of(v) } {
            DynNode::Str(s) => {
                crate::c_abi::gos_rt_result_new(0, alloc_cstring(s.as_bytes()) as i64)
            }
            _ => crate::c_abi::gos_rt_result_new(1, 0),
        }
    })
}

/// The bytes a node holds, as a `Vec` of byte values; empty for every other
/// kind.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_dyn_as_bytes(v: *const GosDyn) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let out = unsafe { crate::c_abi::vec::gos_rt_vec_new(8) };
        if let DynNode::Bytes(bytes) = unsafe { node_of(v) } {
            for byte in bytes {
                unsafe { crate::c_abi::vec::gos_rt_vec_push_i64(out, i64::from(*byte)) };
            }
        }
        out
    })
}

// --- lifecycle, comparison, rendering ----------------------------

/// Another handle onto the same node.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_dyn_clone(v: *const GosDyn) -> *mut GosDyn {
    ffi_entry!(std::ptr::null_mut(), {
        if v.is_null() {
            return GosDyn::into_raw(DynNode::Nil);
        }
        GosDyn::share(unsafe { &(*v).node })
    })
}

/// Drops a handle, releasing its share of the node.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_dyn_free(v: *mut GosDyn) {
    ffi_entry!((), {
        if v.is_null() {
            return;
        }
        drop(unsafe { Box::from_raw(v) });
    });
}

/// Structural equality: two values are equal when they hold the same
/// contents, a tagged arm matching on its name and every payload field.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_dyn_eq(a: *const GosDyn, b: *const GosDyn) -> i64 {
    ffi_entry!(0, {
        i64::from(unsafe { node_of(a) } == unsafe { node_of(b) })
    })
}

/// The text `{}` and `{:?}` show for a value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_dyn_format(v: *const GosDyn) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let mut out = String::new();
        render(unsafe { node_of(v) }, &mut out);
        alloc_cstring(out.as_bytes())
    })
}

/// The index of a tagged value's arm within `names`, a `|`-separated list in
/// discriminant order, or `-1` when the value is not that arm set's.
///
/// A binding that declares its arms names a Gossamer enum; this is how the
/// compiled tiers turn the wire's runtime arm name back into the enum's own
/// discriminant, so a `match` reads the same arm it reads on the interpreter.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_dyn_arm_index(v: *const GosDyn, names: *const c_char) -> i64 {
    ffi_entry!(-1, {
        let DynNode::Tagged { name, .. } = (unsafe { node_of(v) }) else {
            return -1;
        };
        if names.is_null() {
            return -1;
        }
        let declared = unsafe { crate::c_abi::gos_str_arg_bytes(names) };
        let Ok(declared) = std::str::from_utf8(declared) else {
            return -1;
        };
        declared
            .split('|')
            .position(|arm| arm == name)
            .and_then(|index| i64::try_from(index).ok())
            .unwrap_or(-1)
    })
}

/// One payload field, read as the type the arm declares it. A field the value
/// does not carry, or carries as another shape, reads as that type's zero.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_dyn_field_i64(v: *const GosDyn, index: i64) -> i64 {
    ffi_entry!(0, {
        match unsafe { payload_field(v, index) } {
            Some(DynNode::Int(n)) => *n,
            Some(DynNode::Bool(b)) => i64::from(*b),
            Some(DynNode::Char(c)) => i64::from(*c as u32),
            _ => 0,
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_dyn_field_f64(v: *const GosDyn, index: i64) -> f64 {
    ffi_entry!(0.0, {
        match unsafe { payload_field(v, index) } {
            Some(DynNode::Float(f)) => *f,
            Some(DynNode::Int(n)) => *n as f64,
            _ => 0.0,
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_dyn_field_str(v: *const GosDyn, index: i64) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        match unsafe { payload_field(v, index) } {
            Some(DynNode::Str(s)) => alloc_cstring(s.as_bytes()),
            _ => alloc_cstring(b""),
        }
    })
}

/// One payload field as a dynamic value of its own, for an arm whose field is
/// itself open.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_dyn_field_dyn(v: *const GosDyn, index: i64) -> *mut GosDyn {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { payload_field(v, index) }.map_or_else(
            || GosDyn::into_raw(DynNode::Nil),
            |node| GosDyn::into_raw(node.clone()),
        )
    })
}

/// The payload field at `index` of a tagged value.
unsafe fn payload_field<'a>(v: *const GosDyn, index: i64) -> Option<&'a DynNode> {
    let DynNode::Tagged { payload, .. } = (unsafe { node_of(v) }) else {
        return None;
    };
    let index = usize::try_from(index).ok()?;
    payload.get(index).map(std::convert::AsRef::as_ref)
}
