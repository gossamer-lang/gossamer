//! `{}` and `{:?}` rendering of VM values.

use super::{DenseMap, MapKey, SmolStr, StructInner, Value, dense_map, native_enum_to_variant};

use std::fmt;
use std::sync::Arc;

impl fmt::Display for Value {
    #[allow(
        clippy::too_many_lines,
        reason = "one match whose length is the value set"
    )]
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Primitive formatting delegates to the shared runtime
        // helpers so the interpreter and the native backend produce
        // byte-identical text.
        match self {
            Self::NativeEnum(o) => fmt::Display::fmt(&native_enum_to_variant(o), out),
            // Cells render as their inner value - they are a call-
            // protocol artifact, never a user-visible shape.
            Self::MutCell(c) => {
                let inner = c.lock().clone();
                fmt::Display::fmt(&inner, out)
            }
            Self::CaptureCell(c) => {
                let inner = c.lock().clone();
                fmt::Display::fmt(&inner, out)
            }
            Self::Unit => out.write_str(gossamer_runtime::builtins::format_unit()),
            Self::Bool(b) => out.write_str(gossamer_runtime::builtins::format_bool(*b)),
            Self::Int(i) => out.write_str(&gossamer_runtime::builtins::format_int(*i)),
            Self::Float(f) => out.write_str(&gossamer_runtime::builtins::format_float(*f)),
            Self::Char(c) => write!(out, "{c}"),
            Self::String(s) => out.write_str(s),
            Self::Json(value) => out.write_str(&gossamer_std::json::encode(value.as_value())),
            Self::Tuple(parts) => write_tuple(out, parts),
            Self::Array(parts) => write_array(out, parts),
            Self::FloatArray(_) => write_array(out, &self.float_array_elems()),
            Self::IntArray(data) => {
                let elems: Vec<Value> = data.iter().copied().map(Value::Int).collect();
                write_array(out, &elems)
            }
            Self::ByteArray(data) => {
                let elems: Vec<Value> = data
                    .iter()
                    .copied()
                    .map(|value| Value::Int(i64::from(value)))
                    .collect();
                write_array(out, &elems)
            }
            Self::InlineByteArray(data) => {
                let elems: Vec<Value> = data
                    .iter()
                    .copied()
                    .map(|value| Value::Int(i64::from(value)))
                    .collect();
                write_array(out, &elems)
            }
            Self::ByteVec(data) => {
                let elems: Vec<Value> = data
                    .iter()
                    .copied()
                    .map(|value| Value::Int(i64::from(value)))
                    .collect();
                write_array(out, &elems)
            }
            Self::FloatVec(data) => {
                let elems: Vec<Value> = data.iter().copied().map(Value::Float).collect();
                write_array(out, &elems)
            }
            Self::LazyIter(id) => match crate::stdlib_builtins::iter::lazy_iter_repr(id.id()) {
                Some(range) => out.write_str(&range),
                None => out.write_str("<iterator>"),
            },
            Self::Variant(inner) => write_variant(out, inner.name.as_str(), &inner.fields),
            Self::Struct(inner) => {
                // Placeholder expressions evaluate to this sentinel in the VM;
                // the compiled tiers emit "<value>" for the same cases.
                if inner.name == "<stub>" {
                    return out.write_str("<value>");
                }
                if matches!(inner.name.as_str(), "bytes::Buffer" | "bytes::Builder") {
                    return out.write_str(inner.name.as_str());
                }
                if let Some(f) = f32_render_slot(self) {
                    return out.write_str(&gossamer_runtime::builtins::format_f32(f));
                }
                if let Some(items) = vec_render_items(inner) {
                    return out.write_str(&repr_vec(items));
                }
                if is_set_struct_name(inner.name.as_str()) {
                    return out.write_str(&repr_set(self));
                }
                if is_deque_struct_name(inner.name.as_str()) {
                    return out.write_str(&repr_deque(self, inner.name.as_str()));
                }
                if is_heap_struct_name(inner.name.as_str()) {
                    return out.write_str(&repr_binary_heap(self, inner.name.as_str()));
                }
                // `errors::Error` prints Go-style as its colon-joined
                // cause chain ("outer: mid: root") so `format!("{}", e)`
                // and `?`-surfaced errors match the compiled tiers'
                // `gos_rt_error_display` path. `.message()` stays
                // top-level-only. Other structs keep the default
                // `Name { f: v, … }` shape used everywhere else.
                if let Some(text) = error_chain_text(self) {
                    return out.write_str(&text);
                }
                write_struct(
                    out,
                    source_facing_nested_item_name(inner.name.as_str()),
                    &inner.fields.to_vec(),
                )
            }
            Self::Closure(_) => out.write_str("<closure>"),
            Self::Builtin(inner) => write!(out, "<builtin {}>", inner.name),
            Self::Native(inner) => write!(out, "<native {}>", inner.name),
            Self::Channel(ch) => write!(out, "{ch:?}"),
            Self::Opaque(_) => out.write_str("<opaque>"),
            Self::Map(map) => write_map(out, &map.lock()),
            Self::IntMap(map) => write_int_map(out, &map.lock()),
            Self::StrIntMap(map) => write_str_int_map(out, &map.lock()),
            Self::Uint(n) => write!(out, "{n}"),
            Self::Weak(_) => out.write_str("<weak>"),
            Self::Void => out.write_str("<void>"),
        }
    }
}

/// Descriptor bytes naming where a rendered value's integers were declared
/// unsigned. The compiler builds one per rendered argument whose type holds
/// such an integer; [`uint_leaves`] walks the value alongside it.
pub(crate) mod uint_desc {
    /// Nothing under this position is unsigned.
    pub(crate) const NONE: u8 = b'.';
    /// This integer position is unsigned.
    pub(crate) const UINT: u8 = b'u';
    /// This float position is an `f32`, which renders with the digits of its
    /// single-precision value. Only a render descriptor carries it.
    pub(crate) const F32: u8 = b'f';
    /// A fixed array or a slice; the element's own descriptor follows.
    /// Renders in bare brackets, which is how both are written.
    pub(crate) const SEQ: u8 = b'v';
    /// A `Vec`; the element's own descriptor follows. Renders in the
    /// `Vec` literal's own spelling, `#[..]`, which is what tells it
    /// apart from a fixed array at every depth - the two share one
    /// runtime representation, so only the descriptor knows.
    pub(crate) const VEC: u8 = b'V';
    /// A map; the key's descriptor follows, then the value's.
    pub(crate) const MAP: u8 = b'm';
    /// A tuple; its arity follows as one byte, then that many descriptors.
    pub(crate) const TUPLE: u8 = b't';
    /// An `Option`; the payload's descriptor follows.
    pub(crate) const OPTION: u8 = b'o';
    /// A `Result`; the `Ok` descriptor follows, then the `Err` one.
    pub(crate) const RESULT: u8 = b'r';
    /// A `Set` / `BTreeSet` handle; the element's own descriptor follows.
    /// The elements live in a runtime registry, so the descriptor rides
    /// on the handle the renderer copies.
    pub(crate) const SET: u8 = b's';
    /// A `Deque` / `Queue` / `Stack` / heap handle; the element's own
    /// descriptor follows. The elements live in a runtime registry, so
    /// the descriptor rides on the handle the renderer copies.
    pub(crate) const CONTAINER: u8 = b'c';
    /// A struct; its field count follows as one byte, then that many
    /// descriptors, in declaration order. Only the REPL builds this:
    /// a program renders a struct through the `to_string` synthesized
    /// for its type, which describes each field where it formats it.
    pub(crate) const ADT: u8 = b'A';
}

/// Name of the one-field wrapper [`uint_leaves`] puts a `Vec` in so the
/// renderer knows to spell it `#[..]`. A `Vec` and a fixed array share
/// one runtime representation, so the descriptor built from the static
/// type is the only thing that can tell them apart; the wrapper carries
/// that answer on the renderer's private copy and nowhere else.
pub(crate) const VEC_RENDER_NAME: &str = "__vec";

/// Name of the one-field wrapper [`uint_leaves`] puts an `f32` in so the
/// renderer spells it with single-precision digits. The slot holds the value
/// at double width, so the static type is the only thing that says which
/// digits read back as it.
pub(crate) const F32_RENDER_NAME: &str = "__f32";

/// Field a rendered container handle carries to describe its elements,
/// for the containers whose elements the renderer reads out of a
/// registry rather than out of the value. Only [`uint_leaves`] adds it.
pub(crate) const ELEM_DESC_MARKER: &str = "__elemdesc";

/// Returns `value` with the integers the descriptor names re-boxed as
/// [`Value::Uint`], so a `u64` at or above `i64::MAX` renders as its own
/// decimal instead of the negative the same bits spell. The copy is the
/// renderer's alone, so the source keeps its own representation.
#[must_use]
pub fn uint_leaves(value: &Value, desc: &[u8]) -> Value {
    let mut cursor = 0usize;
    convert_uint(value, desc, &mut cursor)
}

/// Advances `cursor` past one descriptor without converting anything.
fn skip_uint_desc(desc: &[u8], cursor: &mut usize) {
    let tag = desc.get(*cursor).copied().unwrap_or(uint_desc::NONE);
    *cursor += 1;
    match tag {
        uint_desc::SEQ
        | uint_desc::VEC
        | uint_desc::OPTION
        | uint_desc::CONTAINER
        | uint_desc::SET => {
            skip_uint_desc(desc, cursor);
        }
        uint_desc::MAP | uint_desc::RESULT => {
            skip_uint_desc(desc, cursor);
            skip_uint_desc(desc, cursor);
        }
        uint_desc::TUPLE | uint_desc::ADT => {
            let arity = desc.get(*cursor).copied().unwrap_or(0) as usize;
            *cursor += 1;
            for _ in 0..arity {
                skip_uint_desc(desc, cursor);
            }
        }
        _ => {}
    }
}

fn convert_uint(value: &Value, desc: &[u8], cursor: &mut usize) -> Value {
    let tag = desc.get(*cursor).copied().unwrap_or(uint_desc::NONE);
    *cursor += 1;
    match tag {
        uint_desc::UINT => match value {
            Value::Int(n) => Value::Uint(*n as u64),
            other => other.clone(),
        },
        uint_desc::F32 => match value {
            Value::Float(f) => Value::struct_(F32_RENDER_NAME, vec![("value", Value::Float(*f))]),
            other => other.clone(),
        },
        uint_desc::SEQ => convert_uint_sequence(value, desc, cursor),
        uint_desc::VEC => {
            // A value already carrying the `Vec` spelling reaches a second
            // format site inside the rendering that describes it again.
            if let Value::Struct(inner) = value
                && vec_render_items(inner).is_some()
            {
                skip_uint_desc(desc, cursor);
                return value.clone();
            }
            let converted = convert_uint_sequence(value, desc, cursor);
            Value::struct_(VEC_RENDER_NAME, vec![("items", converted)])
        }
        uint_desc::TUPLE => {
            let arity = desc.get(*cursor).copied().unwrap_or(0) as usize;
            *cursor += 1;
            let Value::Tuple(parts) = value else {
                for _ in 0..arity {
                    skip_uint_desc(desc, cursor);
                }
                return value.clone();
            };
            let mut out = Vec::with_capacity(parts.len());
            for i in 0..arity {
                match parts.get(i) {
                    Some(part) => out.push(convert_uint(part, desc, cursor)),
                    None => skip_uint_desc(desc, cursor),
                }
            }
            out.extend(parts.iter().skip(arity).cloned());
            Value::Tuple(Arc::new(out))
        }
        uint_desc::ADT => {
            let count = desc.get(*cursor).copied().unwrap_or(0) as usize;
            *cursor += 1;
            let Value::Struct(inner) = value else {
                for _ in 0..count {
                    skip_uint_desc(desc, cursor);
                }
                return value.clone();
            };
            let mut fields = inner.fields.to_vec();
            for i in 0..count {
                match fields.get_mut(i) {
                    Some((_, field)) => *field = convert_uint(&field.clone(), desc, cursor),
                    None => skip_uint_desc(desc, cursor),
                }
            }
            Value::struct_(inner.name.as_str(), fields)
        }
        uint_desc::OPTION | uint_desc::RESULT => convert_uint_variant(value, desc, cursor, tag),
        uint_desc::MAP => convert_uint_map(value, desc, cursor),
        uint_desc::CONTAINER | uint_desc::SET => {
            let elem_at = *cursor;
            skip_uint_desc(desc, cursor);
            match value {
                Value::Struct(inner)
                    if desc[elem_at..*cursor].iter().any(|b| *b != uint_desc::NONE) =>
                {
                    let elem_desc: String =
                        desc[elem_at..*cursor].iter().map(|b| *b as char).collect();
                    let mut fields = inner.fields.to_vec();
                    fields.push((ELEM_DESC_MARKER, Value::String(elem_desc.as_str().into())));
                    Value::struct_(inner.name.as_str(), fields)
                }
                other => other.clone(),
            }
        }
        _ => value.clone(),
    }
}

fn convert_uint_sequence(value: &Value, desc: &[u8], cursor: &mut usize) -> Value {
    let elem_at = *cursor;
    skip_uint_desc(desc, cursor);
    let convert = |v: &Value| {
        let mut elem_cursor = elem_at;
        convert_uint(v, desc, &mut elem_cursor)
    };
    match value {
        Value::Array(items) => Value::Array(Arc::new(items.iter().map(convert).collect())),
        Value::IntArray(items) => Value::Array(Arc::new(
            items.iter().map(|n| convert(&Value::Int(*n))).collect(),
        )),
        Value::FloatVec(items) => Value::Array(Arc::new(
            items.iter().map(|f| convert(&Value::Float(*f))).collect(),
        )),
        Value::FloatArray(_) => Value::Array(Arc::new(
            value.float_array_elems().iter().map(convert).collect(),
        )),
        other => other.clone(),
    }
}

fn convert_uint_variant(value: &Value, desc: &[u8], cursor: &mut usize, tag: u8) -> Value {
    let ok_at = *cursor;
    skip_uint_desc(desc, cursor);
    let err_at = *cursor;
    if tag == uint_desc::RESULT {
        skip_uint_desc(desc, cursor);
    }
    let Value::Variant(variant) = value else {
        return value.clone();
    };
    let arm_at = match variant.name.as_str() {
        "Some" | "Ok" => ok_at,
        "Err" if tag == uint_desc::RESULT => err_at,
        _ => return value.clone(),
    };
    let fields = variant
        .fields
        .iter()
        .map(|field| {
            let mut field_cursor = arm_at;
            convert_uint(field, desc, &mut field_cursor)
        })
        .collect();
    Value::variant(variant.name.as_str(), fields)
}

fn convert_uint_map(value: &Value, desc: &[u8], cursor: &mut usize) -> Value {
    let key_at = *cursor;
    skip_uint_desc(desc, cursor);
    let val_at = *cursor;
    skip_uint_desc(desc, cursor);
    let key_described = desc[key_at..val_at].iter().any(|b| *b != uint_desc::NONE);
    // A key renders from the `MapKey` the copy stores, so a key the type
    // describes - an unsigned integer, a `Vec`, or an aggregate holding
    // either - is rebuilt from its described value.
    let convert_key = |key: &MapKey| {
        if key_described {
            let mut key_cursor = key_at;
            MapKey::rendered(&convert_uint(&key.to_value(), desc, &mut key_cursor))
        } else {
            key.clone()
        }
    };
    let convert_value = |v: &Value| {
        let mut value_cursor = val_at;
        convert_uint(v, desc, &mut value_cursor)
    };
    let mut out: DenseMap<MapKey, Value> = dense_map();
    match value {
        Value::Map(map) => {
            for (key, entry) in map.lock().iter() {
                out.insert(convert_key(key), convert_value(entry));
            }
        }
        Value::IntMap(map) => {
            for (key, entry) in map.lock().iter() {
                out.insert(
                    convert_key(&MapKey::Int(*key)),
                    convert_value(&Value::Int(*entry)),
                );
            }
        }
        Value::StrIntMap(map) => {
            for (key, entry) in map.lock().iter() {
                out.insert(MapKey::Str(key.clone()), convert_value(&Value::Int(*entry)));
            }
        }
        other => return other.clone(),
    }
    Value::Map(Arc::new(parking_lot::Mutex::new(out.into())))
}

/// Renders one struct field, reading it as unsigned when the declaration
/// named it `u64` / `usize`, which is what the derived `fmt` the compiled
/// tiers call does.
fn uint_aware_field(struct_name: &str, field_name: &str, field: &Value) -> String {
    match field {
        Value::Int(n) if crate::builtins::struct_field_is_uint(struct_name, field_name) => {
            format!("{}", *n as u64)
        }
        other => repr_value(other),
    }
}

/// A map key's text: a `String` key reads quoted, and every other key reads
/// the way the same value reads anywhere else, which is what both compiled
/// tiers render from the key's tag.
fn map_key_text(key: &Value) -> String {
    if let Some(f) = f32_render_slot(key) {
        return gossamer_runtime::builtins::format_f32_debug(f);
    }
    match key {
        Value::String(text) => format!("{:?}", text.as_str()),
        Value::Char(ch) => format!("{ch:?}"),
        other => other.to_string(),
    }
}

pub(super) fn repr_value(value: &Value) -> String {
    if let Some(f) = f32_render_slot(value) {
        return gossamer_runtime::builtins::format_f32_debug(f);
    }
    match value {
        Value::Float(number) => repr_float(*number),
        Value::String(text) => format!("{:?}", text.as_str()),
        Value::Char(ch) => format!("{ch:?}"),
        Value::Tuple(parts) => {
            let mut rendered: Vec<String> = parts.iter().map(repr_value).collect();
            if rendered.len() == 1 {
                rendered[0].push(',');
            }
            format!("({})", rendered.join(", "))
        }
        Value::Array(parts) => format!(
            "[{}]",
            parts.iter().map(repr_value).collect::<Vec<_>>().join(", ")
        ),
        Value::FloatArray(_) => repr_value(&Value::Array(Arc::new(value.float_array_elems()))),
        Value::IntArray(data) => format!("{:?}", data.as_slice()),
        Value::ByteArray(data) => format!("{:?}", &data[..]),
        Value::InlineByteArray(data) => format!("{:?}", data.as_slice()),
        Value::ByteVec(data) => format!("{:?}", data.as_slice()),
        Value::FloatVec(data) => format!(
            "[{}]",
            data.iter()
                .map(|number| repr_float(*number))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::LazyIter(id) => crate::stdlib_builtins::iter::lazy_iter_repr(id.id())
            .unwrap_or_else(|| "<iterator>".to_string()),
        Value::Variant(inner) => {
            let fields = inner.fields.iter().map(repr_value).collect::<Vec<_>>();
            if fields.is_empty() {
                inner.name.as_str().to_string()
            } else {
                format!("{}({})", inner.name.as_str(), fields.join(", "))
            }
        }
        Value::Struct(inner)
            if matches!(inner.name.as_str(), "bytes::Buffer" | "bytes::Builder") =>
        {
            inner.name.as_str().to_string()
        }
        Value::Struct(inner) if vec_render_items(inner).is_some() => {
            repr_vec_debug(vec_render_items(inner).unwrap_or(value))
        }
        Value::Struct(inner) if is_set_struct_name(inner.name.as_str()) => repr_set(value),
        Value::Struct(inner) if is_deque_struct_name(inner.name.as_str()) => {
            repr_deque(value, inner.name.as_str())
        }
        Value::Struct(inner) if is_heap_struct_name(inner.name.as_str()) => {
            repr_binary_heap(value, inner.name.as_str())
        }
        Value::Struct(inner) => repr_struct(
            source_facing_nested_item_name(inner.name.as_str()),
            &inner.fields.to_vec(),
        ),
        Value::Map(map) => {
            let map = map.lock();
            let entries = map.sorted();
            format!(
                "{{{}}}",
                entries
                    .iter()
                    .map(|(key, item)| format!(
                        "{}: {}",
                        map_key_text(&key.to_value()),
                        repr_value(item)
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        Value::StrIntMap(map) => {
            let map = map.lock();
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
            format!(
                "{{{}}}",
                entries
                    .iter()
                    .map(|(key, item)| format!("{:?}: {item}", key.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        Value::MutCell(cell) => repr_value(&cell.lock()),
        Value::NativeEnum(owner) => repr_value(&native_enum_to_variant(owner)),
        _ => value.to_string(),
    }
}

/// Renders `owner {a, b}` over the set's elements, sorted so printed output is
/// stable whatever order the elements went in. Elements print in their Display
/// form, matching `gos_rt_set_format_*` and the way a sequence of the same
/// elements prints.
fn repr_set(value: &Value) -> String {
    let values = crate::stdlib_builtins::set::set_display_snapshot(value).unwrap_or_default();
    // A set renders in its own literal spelling, the same one a program
    // writes to build it, at every depth and on every tier. `Set` and
    // `BTreeSet` are both written `#{..}`. The rendered copy of a set whose
    // element type names an unsigned integer or a `Vec` carries that
    // element's descriptor, so each element reads the way its type spells it.
    format!("#{{{}}}", render_elements(value, &values))
}

/// A set's elements in the order its element type gives them. The snapshot
/// arrives ordered by stored key, which is the language's order for every
/// element except one holding a `u64` / `usize`, whose bits order unsigned;
/// the rendered copy's descriptor names those.
pub(crate) fn described_set_order(handle: &Value, mut values: Vec<Value>) -> Vec<Value> {
    let Some(desc) = elem_desc_of(handle).filter(|desc| desc.as_bytes().contains(&uint_desc::UINT))
    else {
        return values;
    };
    let mut keyed: Vec<(Value, Value)> = values
        .drain(..)
        .map(|value| (uint_leaves(&value, desc.as_bytes()), value))
        .collect();
    keyed.sort_by(|a, b| crate::stdlib_builtins::iter::compare_values_total(&a.0, &b.0));
    keyed.into_iter().map(|(_, value)| value).collect()
}

fn repr_deque(value: &Value, owner: &str) -> String {
    let values = crate::stdlib_builtins::deque::deque_snapshot(value).unwrap_or_default();
    // Elements read the way a sequence's do - `Deque [a, b]` for the same
    // elements a `Vec` prints as `[a, b]`, which is what both compiled tiers
    // render from the element descriptor.
    format!("{owner} [{}]", render_elements(value, &values))
}

/// The elements of a registry-backed container, each rendered under the
/// descriptor the handle carries, comma-joined.
fn render_elements(handle: &Value, values: &[Value]) -> String {
    let desc = elem_desc_of(handle);
    values
        .iter()
        .map(|element| match &desc {
            Some(desc) => render_element(&uint_leaves(element, desc.as_bytes())),
            None => render_element(element),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The element descriptor a rendered container handle carries, if any.
pub(crate) fn elem_desc_of(handle: &Value) -> Option<String> {
    let Value::Struct(inner) = handle else {
        return None;
    };
    inner
        .fields
        .iter()
        .find_map(|(name, value)| (*name == ELEM_DESC_MARKER).then(|| value.to_string()))
}

fn is_set_struct_name(name: &str) -> bool {
    matches!(name, "Set" | "BTreeSet")
}

/// The value an [`F32_RENDER_NAME`] wrapper carries, when `value` is one.
pub(crate) fn f32_render_slot(value: &Value) -> Option<f64> {
    let Value::Struct(inner) = value else {
        return None;
    };
    if inner.name.as_str() != F32_RENDER_NAME {
        return None;
    }
    match inner.fields.get(0) {
        Some((_, Value::Float(f))) => Some(*f),
        _ => None,
    }
}

/// Whether `inner` is the render-only wrapper that says "this sequence
/// is a `Vec`", and the sequence it carries.
pub(crate) fn vec_render_items(inner: &StructInner) -> Option<&Value> {
    if inner.name.as_str() != VEC_RENDER_NAME {
        return None;
    }
    inner.fields.get(0).map(|(_, items)| items)
}

/// One element of a sequence as its own text. A float inside a sequence
/// reads in the form that shows it is one, which is what
/// [`write_element`] writes wherever a sequence is rendered.
fn render_element(value: &Value) -> String {
    if let Some(f) = f32_render_slot(value) {
        return gossamer_runtime::builtins::format_f32_debug(f);
    }
    match value {
        Value::Float(number) => repr_float(*number),
        Value::String(text) => format!("{:?}", text.as_str()),
        Value::Char(ch) => format!("{ch:?}"),
        other => other.to_string(),
    }
}

/// A `Vec` in its own literal spelling for the `repr` channel, where a
/// nested value reads as the source that would build it - a `String`
/// element in quotes, exactly as a fixed array's element does.
fn repr_vec_debug(items: &Value) -> String {
    match items.as_value_slice() {
        Some(parts) => format!(
            "#[{}]",
            parts.iter().map(repr_value).collect::<Vec<_>>().join(", ")
        ),
        None => repr_vec(items),
    }
}

/// A `Vec` in its own literal spelling, elements rendered as they are
/// anywhere else.
pub(crate) fn vec_render_text(items: &Value) -> String {
    repr_vec(items)
}

/// A `Vec` in its own literal spelling, elements rendered as they are
/// anywhere else.
fn repr_vec(items: &Value) -> String {
    if let Some(parts) = items.as_value_slice() {
        return format!(
            "#[{}]",
            parts
                .iter()
                .map(render_element)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    // A flat typed storage - an `IntArray`, a `FloatVec`, a byte buffer -
    // renders its own elements in bare brackets, and the `Vec` spelling
    // is that same text under the `Vec` opening bracket.
    let rendered = repr_value(items);
    match rendered.strip_prefix('[') {
        Some(rest) => format!("#[{rest}"),
        None => format!("#[{rendered}]"),
    }
}

fn is_deque_struct_name(name: &str) -> bool {
    matches!(name, "Deque" | "Queue" | "Stack")
}

fn is_heap_struct_name(name: &str) -> bool {
    matches!(name, "MaxHeap" | "MinHeap")
}

fn repr_binary_heap(value: &Value, owner: &str) -> String {
    let values =
        crate::stdlib_builtins::container_heap::binary_heap_snapshot(value).unwrap_or_default();
    format!("{owner} [{}]", render_elements(value, &values))
}

fn repr_float(number: f64) -> String {
    gossamer_runtime::builtins::format_float_debug(number)
}

fn repr_struct(name: &str, fields: &[(&'static str, Value)]) -> String {
    // A field declared `u64` / `usize` reads as unsigned, matching the
    // derived `fmt` the compiled tiers call.
    let render_field = |field_name: &str, field: &Value| uint_aware_field(name, field_name, field);
    let is_tuple_struct = !fields.is_empty()
        && fields
            .iter()
            .enumerate()
            .all(|(i, (n, _))| n.parse::<usize>() == Ok(i));
    if is_tuple_struct {
        let fields = fields
            .iter()
            .map(|(field_name, field)| render_field(field_name, field))
            .collect::<Vec<_>>()
            .join(", ");
        return format!("{name}({fields})");
    }
    format!(
        "{name} {{ {} }}",
        fields
            .iter()
            .map(|(field_name, field)| format!("{field_name}: {}", render_field(field_name, field)))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn source_facing_nested_item_name(name: &str) -> &str {
    // A user type's identity carries the modules containing it, so two
    // modules may declare the same name; a rendered value shows the name as
    // declared. A stdlib type whose own name is a path (`errors::Error`,
    // `bytes::Builder`) keeps every segment - its prefix names a stdlib
    // module, which no user module path does.
    let name = match name.split_once("::") {
        Some((head, _)) if gossamer_resolve::STDLIB_MODULES.contains(&head) => name,
        Some(_) => name.rsplit("::").next().unwrap_or(name),
        None => name,
    };
    let Some(rest) = name.strip_prefix("__gos_nested_") else {
        return name;
    };
    let Some((def, source_name)) = rest.split_once('_') else {
        return name;
    };
    if def.bytes().all(|byte| byte.is_ascii_digit()) && !source_name.is_empty() {
        source_name
    } else {
        name
    }
}

/// The colon-joined cause chain an `errors::Error` renders as ("outer: mid:
/// root"), or `None` for any other value.
///
/// `{}` and `{:?}` both answer this text on every tier, so the plain
/// formatter and the walk that renders operands carrying their own
/// rendering share one definition of it. `.message()` stays top-level-only.
pub(crate) fn error_chain_text(value: &Value) -> Option<String> {
    let Value::Struct(inner) = value else {
        return None;
    };
    if inner.name != "errors::Error" {
        return None;
    }
    let message = inner
        .fields
        .iter()
        .find(|(n, _)| (**n) == "message")
        .map(|(_, v)| v.clone())?;
    let mut text = format!("{message}");
    let mut cursor = inner
        .fields
        .iter()
        .find(|(n, _)| (**n) == "cause")
        .map(|(_, v)| v.clone());
    while let Some(Value::Variant(link)) = cursor {
        if link.name != "Some" || link.fields.is_empty() {
            break;
        }
        let Value::Struct(cause) = &link.fields[0] else {
            break;
        };
        if cause.name != "errors::Error" {
            break;
        }
        let Some((_, m)) = cause.fields.iter().find(|(n, _)| (**n) == "message") else {
            break;
        };
        text.push_str(": ");
        text.push_str(&m.to_string());
        cursor = cause
            .fields
            .iter()
            .find(|(n, _)| (**n) == "cause")
            .map(|(_, v)| v.clone());
    }
    Some(text)
}

/// Renders one element of an aggregate. A float always keeps a fractional
/// part or an exponent so the text reads back as a float, matching how a
/// struct field renders; every other value keeps its Display text.
fn write_element(out: &mut fmt::Formatter<'_>, value: &Value) -> fmt::Result {
    if let Some(f) = f32_render_slot(value) {
        return out.write_str(&gossamer_runtime::builtins::format_f32_debug(f));
    }
    match value {
        Value::Float(f) => out.write_str(&gossamer_runtime::builtins::format_float_debug(*f)),
        // A nested string or char renders in the spelling that builds it.
        Value::String(text) => write!(out, "{:?}", text.as_str()),
        Value::Char(ch) => write!(out, "{ch:?}"),
        other => write!(out, "{other}"),
    }
}

fn write_tuple(out: &mut fmt::Formatter<'_>, parts: &[Value]) -> fmt::Result {
    out.write_str("(")?;
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            out.write_str(", ")?;
        }
        write_element(out, part)?;
    }
    if parts.len() == 1 {
        out.write_str(",")?;
    }
    out.write_str(")")
}

/// Renders a `HashMap` as `{k: v, …}` with entries sorted by key so
/// the output is deterministic and byte-identical to the compiled
/// tiers' `gos_rt_map_format` (native map storage has its own
/// implementation-defined order).
fn write_map(out: &mut fmt::Formatter<'_>, map: &crate::vm_map::VmMap) -> fmt::Result {
    out.write_str("{")?;
    let entries = map.sorted();
    for (i, (k, v)) in entries.iter().enumerate() {
        if i > 0 {
            out.write_str(", ")?;
        }
        write!(out, "{}: ", map_key_text(&k.to_value()))?;
        write_element(out, v)?;
    }
    out.write_str("}")
}

/// Key-sorted rendering of an `i64`-keyed, `i64`-valued map. Mirrors
/// [`write_map`] for the [`Value::IntMap`] storage shape.
fn write_int_map(out: &mut fmt::Formatter<'_>, map: &DenseMap<i64, i64>) -> fmt::Result {
    out.write_str("{")?;
    let mut entries: Vec<(&i64, &i64)> = map.iter().collect();
    entries.sort_by_key(|&(k, _)| *k);
    for (i, (k, v)) in entries.iter().enumerate() {
        if i > 0 {
            out.write_str(", ")?;
        }
        write!(out, "{k}: {v}")?;
    }
    out.write_str("}")
}

/// Key-sorted rendering of a `String`-keyed, `i64`-valued map.
/// Mirrors [`write_map`] for the [`Value::StrIntMap`] storage shape,
/// quoting keys exactly as the generic map's string keys render.
fn write_str_int_map(out: &mut fmt::Formatter<'_>, map: &DenseMap<SmolStr, i64>) -> fmt::Result {
    out.write_str("{")?;
    let mut entries: Vec<(&SmolStr, &i64)> = map.iter().collect();
    entries.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    for (i, (k, v)) in entries.iter().enumerate() {
        if i > 0 {
            out.write_str(", ")?;
        }
        write!(out, "{:?}: {v}", k.as_str())?;
    }
    out.write_str("}")
}

fn write_array(out: &mut fmt::Formatter<'_>, parts: &[Value]) -> fmt::Result {
    out.write_str("[")?;
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            out.write_str(", ")?;
        }
        write_element(out, part)?;
    }
    out.write_str("]")
}

fn write_variant(out: &mut fmt::Formatter<'_>, name: &str, fields: &[Value]) -> fmt::Result {
    out.write_str(name)?;
    if fields.is_empty() {
        return Ok(());
    }
    out.write_str("(")?;
    for (i, field) in fields.iter().enumerate() {
        if i > 0 {
            out.write_str(", ")?;
        }
        write_element(out, field)?;
    }
    out.write_str(")")
}

fn write_struct(
    out: &mut fmt::Formatter<'_>,
    name: &str,
    fields: &[(&'static str, Value)],
) -> fmt::Result {
    out.write_str(name)?;
    // A tuple struct's fields are named "0".."N-1"; render it as
    // `Name(v0, v1)` to match the derived `fmt` on the compiled tiers.
    let is_tuple_struct = !fields.is_empty()
        && fields
            .iter()
            .enumerate()
            .all(|(i, (n, _))| n.parse::<usize>() == Ok(i));
    if is_tuple_struct {
        out.write_str("(")?;
        for (i, (ident, value)) in fields.iter().enumerate() {
            if i > 0 {
                out.write_str(", ")?;
            }
            out.write_str(&uint_aware_field(name, ident, value))?;
        }
        return out.write_str(")");
    }
    out.write_str(" { ")?;
    for (i, (ident, value)) in fields.iter().enumerate() {
        if i > 0 {
            out.write_str(", ")?;
        }
        write!(
            out,
            "{}: {}",
            (*ident),
            uint_aware_field(name, ident, value)
        )?;
    }
    out.write_str(" }")
}

#[cfg(test)]
mod repr_tests {
    use std::sync::Arc;

    use smallvec::smallvec;

    use super::{StructInner, Value};
    use crate::value::{StructFields, VariantInner, intern_type_tag};

    #[test]
    fn repr_quotes_strings_and_chars_recursively() {
        let list = Value::Array(Arc::new(vec![
            Value::String("wow".into()),
            Value::Char('a'),
        ]));
        let variant = Value::Variant(Arc::new(VariantInner {
            name: intern_type_tag("Ok"),
            fields: smallvec![list],
        }));
        let record = Value::Struct(Arc::new(StructInner {
            name: intern_type_tag("Message"),
            fields: StructFields::new(vec![
                ("text", Value::String("hello".into())),
                ("value", variant),
            ]),
        }));

        assert_eq!(
            record.repr(),
            "Message { text: \"hello\", value: Ok([\"wow\", 'a']) }"
        );
        assert_eq!(Value::String("wow".into()).to_string(), "wow");
    }

    #[test]
    fn repr_keeps_integral_floats_source_like_recursively() {
        let record = Value::Struct(Arc::new(StructInner {
            name: intern_type_tag("Point"),
            fields: StructFields::new(vec![("x", Value::Float(1.0)), ("y", Value::Float(4.0))]),
        }));

        assert_eq!(record.repr(), "Point { x: 1.0, y: 4.0 }");
        assert_eq!(Value::Float(2.0).repr(), "2.0");
        assert_eq!(Value::Float(2.3).repr(), "2.3");
    }
}
