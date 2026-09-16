fn builtin_channel_close(args: &[Value]) -> RuntimeResult<Value> {
    let Some(Value::Channel(channel)) = args.first() else {
        return Err(RuntimeError::Type(
            "close: receiver must be a channel".to_string(),
        ));
    };
    if channel.close() {
        Ok(Value::Unit)
    } else {
        // Go semantics: closing an already-closed channel panics.
        Err(RuntimeError::Panic("close of closed channel".to_string()))
    }
}

fn builtin_channel_recv(args: &[Value]) -> RuntimeResult<Value> {
    let Some(Value::Channel(channel)) = args.first() else {
        return Err(RuntimeError::Type(
            "recv: receiver must be a channel".to_string(),
        ));
    };
    match channel.recv() {
        crate::value::RecvOutcome::Value(value) => Ok(some_variant(value)),
        crate::value::RecvOutcome::Closed => Ok(none_variant()),
        crate::value::RecvOutcome::Deadlocked => Err(
            crate::value::deadlock_error("receive"),
        ),
    }
}

fn builtin_channel_try_recv(args: &[Value]) -> RuntimeResult<Value> {
    let Some(Value::Channel(channel)) = args.first() else {
        return Err(RuntimeError::Type(
            "try_recv: receiver must be a channel".to_string(),
        ));
    };
    Ok(match channel.try_recv() {
        Some(value) => some_variant(value),
        None => none_variant(),
    })
}

/// `rx.recv_ctx(&ctx)` in the interpreter. The VM channel and Context use
/// separate wait primitives, so the receive performs bounded condvar waits and
/// checks the Context between them. A context that has already fired answers
/// `None` without consuming a queued value, as it does in the native runtime.
fn builtin_channel_recv_ctx(args: &[Value]) -> RuntimeResult<Value> {
    let Some(Value::Channel(channel)) = args.first() else {
        return Err(RuntimeError::Type(
            "recv_ctx: receiver must be a channel".to_string(),
        ));
    };
    let ctx = args.get(1);
    Ok(
        match channel.recv_with_cancel(|| {
            ctx.is_some_and(crate::stdlib_builtins::context::value_is_cancelled)
        }) {
            Some(value) => some_variant(value),
            None => none_variant(),
        },
    )
}

/// One flag extracted from a `FlagDecl` struct literal.
#[derive(Debug, Clone)]
struct FlagDeclEntry {
    name: String,
    short: Option<char>,
    default: Value,
}

/// Parses an array of `FlagDecl` structs against `PROGRAM_ARGS` and
/// returns a `FlagMap` value. Used by the declarative `flag::parse`
/// API.
fn builtin_flag_parse(args: &[Value]) -> RuntimeResult<Value> {
    let Some(Value::Array(decls)) = args.first() else {
        return Err(RuntimeError::Type(
            "flag::parse: expected array of FlagDecl".to_string(),
        ));
    };
    let program_args = program_args();
    let entries = extract_flag_decls(decls);
    let mut map_fields: Vec<(&'static str, Value)> = Vec::new();
    let mut positional: Vec<Value> = Vec::new();
    let mut idx = 0usize;
    while idx < program_args.len() {
        let arg = &program_args[idx];
        if arg == "--" {
            for rest in &program_args[idx + 1..] {
                positional.push(Value::String(SmolStr::from(rest.clone())));
            }
            break;
        }
        if let Some(rest) = arg.strip_prefix("--") {
            let (name, explicit) = match rest.split_once('=') {
                Some((n, v)) => (n.to_string(), Some(v.to_string())),
                None => (rest.to_string(), None),
            };
            let Some(entry) = entries.iter().find(|d| d.name == name) else {
                idx += 1;
                continue;
            };
            let parsed = if let Some(v) = explicit {
                flag_parse_value(&entry.default, &v)
            } else {
                let Some(next) = program_args.get(idx + 1) else {
                    idx += 1;
                    continue;
                };
                idx += 1;
                flag_parse_value(&entry.default, next)
            };
            map_fields.push((crate::value::intern_type_name(&entry.name), parsed));
            idx += 1;
            continue;
        }
        if let Some(rest) = arg.strip_prefix('-') {
            if rest.is_empty() {
                positional.push(Value::String(SmolStr::from(arg.clone())));
                idx += 1;
                continue;
            }
            let mut chars = rest.chars();
            let first = chars.next().unwrap();
            let remainder = chars.as_str();
            let Some(entry) = entries.iter().find(|d| d.short == Some(first)) else {
                idx += 1;
                continue;
            };
            let explicit = if remainder.is_empty() {
                None
            } else {
                Some(remainder.to_string())
            };
            let parsed = if let Some(v) = explicit {
                flag_parse_value(&entry.default, &v)
            } else {
                let Some(next) = program_args.get(idx + 1) else {
                    idx += 1;
                    continue;
                };
                idx += 1;
                flag_parse_value(&entry.default, next)
            };
            map_fields.push((crate::value::intern_type_name(&entry.name), parsed));
            idx += 1;
            continue;
        }
        positional.push(Value::String(SmolStr::from(arg.clone())));
        idx += 1;
    }

    for entry in &entries {
        if !map_fields.iter().any(|(ident, _)| (*ident) == entry.name) {
            map_fields.push((
                crate::value::intern_type_name(&entry.name),
                entry.default.clone(),
            ));
        }
    }
    map_fields.push(("__positional", Value::Array(Arc::new(positional))));
    Ok(Value::struct_(
        "FlagMap",
        Arc::unwrap_or_clone(Arc::new(map_fields)),
    ))
}

fn extract_flag_decls(values: &[Value]) -> Vec<FlagDeclEntry> {
    let mut out = Vec::new();
    for value in values {
        let Value::Struct(inner) = value else {
            continue;
        };
        if inner.name != "FlagDecl" {
            continue;
        }
        let field_map: std::collections::HashMap<&str, &Value> = inner
            .fields
            .iter()
            .map(|(ident, val)| ((*ident), val))
            .collect();
        let Some(Value::String(flag_name)) = field_map.get("name") else {
            continue;
        };
        let short = field_map.get("short").and_then(|v| match v {
            Value::Char(c) => Some(*c),
            _ => None,
        });
        let default = field_map
            .get("value")
            .copied()
            .cloned()
            .unwrap_or(Value::Unit);
        out.push(FlagDeclEntry {
            name: flag_name.to_string(),
            short,
            default,
        });
    }
    out
}

fn flag_parse_value(default: &Value, raw: &str) -> Value {
    match default {
        Value::Variant(inner) if inner.name == "Int" => {
            let n = raw.parse::<i64>().unwrap_or(0);
            Value::variant("Int", vec![Value::Int(n)])
        }
        Value::Variant(inner) if inner.name == "Str" => {
            Value::variant("Str", vec![Value::String(SmolStr::from(raw.to_string()))])
        }
        Value::Variant(inner) if inner.name == "Bool" => {
            let b = matches!(raw, "true" | "1" | "yes" | "on");
            Value::variant("Bool", vec![Value::Bool(b)])
        }
        _ => Value::String(SmolStr::from(raw.to_string())),
    }
}

/// `FlagMap::get(flag_map, key)` returns `Some(flag_value)` when the
/// key exists in the parsed map, otherwise `None`.
fn builtin_flag_map_get(args: &[Value]) -> RuntimeResult<Value> {
    let (map, key) = match args {
        [Value::Struct(inner), key_value] if inner.name == "FlagMap" => {
            let key_str = match key_value {
                Value::String(s) => s.as_str(),
                _ => "",
            };
            (&inner.fields, key_str)
        }
        _ => {
            return Err(RuntimeError::Type(
                "FlagMap::get: expected FlagMap and key".to_string(),
            ));
        }
    };
    let found = map.iter().find(|(ident, _)| (**ident) == key);
    Ok(match found {
        Some((_, value)) => some_variant(value.clone()),
        None => none_variant(),
    })
}

// ------------------------------------------------------------------
// Sync primitives: I64Vec, WaitGroup, lcg_jump.
//
// The interpreter exposes these as `Value::Struct { name: "I64Vec" }`
// / `Value::Struct { name: "WaitGroup" }` carrying a single
// `__handle: i64` field. The actual mutable state lives in the
// global side tables below, so the `Value` itself stays cheap to
// clone across goroutine boundaries (the closure-call form of
// `go iub_worker(buf, ...)` passes `buf` by value into the
// spawned thread). Shared writes go through `AtomicI64::store`
// without locking; non-overlapping ranges are how `fasta.gos`'s
// fan-out is correct in the first place.

use std::sync::atomic::AtomicI64;

struct WaitGroupCell {
    counter: parking_lot::Mutex<i64>,
    cond: parking_lot::Condvar,
}

static I64VEC_REGISTRY: parking_lot::Mutex<Vec<Option<Arc<Vec<AtomicI64>>>>> =
    parking_lot::Mutex::new(Vec::new());
static WG_REGISTRY: parking_lot::Mutex<Vec<Option<Arc<WaitGroupCell>>>> =
    parking_lot::Mutex::new(Vec::new());

fn i64vec_register(arc: Arc<Vec<AtomicI64>>) -> i64 {
    let mut reg = I64VEC_REGISTRY.lock();
    for (i, slot) in reg.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(arc);
            return i as i64;
        }
    }
    let id = reg.len() as i64;
    reg.push(Some(arc));
    id
}

fn i64vec_lookup(handle: i64) -> Option<Arc<Vec<AtomicI64>>> {
    let reg = I64VEC_REGISTRY.lock();
    if handle < 0 {
        return None;
    }
    reg.get(handle as usize).and_then(std::clone::Clone::clone)
}

fn wg_register(arc: Arc<WaitGroupCell>) -> i64 {
    let mut reg = WG_REGISTRY.lock();
    for (i, slot) in reg.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(arc);
            return i as i64;
        }
    }
    let id = reg.len() as i64;
    reg.push(Some(arc));
    id
}

fn wg_lookup(handle: i64) -> Option<Arc<WaitGroupCell>> {
    let reg = WG_REGISTRY.lock();
    if handle < 0 {
        return None;
    }
    reg.get(handle as usize).and_then(std::clone::Clone::clone)
}

fn struct_handle(v: &Value, expected: &str) -> Option<i64> {
    match v {
        Value::Struct(inner) if inner.name == expected => {
            for (ident, val) in &inner.fields {
                if (*ident) == "__handle" {
                    if let Value::Int(n) = val {
                        return Some(*n);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

fn make_handle_struct(name: &str, handle: i64) -> Value {
    Value::struct_(name, vec![("__handle", Value::Int(handle))])
}

/// Snapshots a `U8Vec`'s registry-backed bytes for the JIT trampoline to
/// marshal into a fresh native buffer. `None` if `v` is not a live `U8Vec`.
pub(crate) fn u8vec_snapshot_bytes(v: &Value) -> Option<Vec<u8>> {
    let handle = struct_handle(v, "U8Vec")?;
    let arc = u8vec_lookup(handle)?;
    Some(
        arc.iter()
            .map(|b| b.load(std::sync::atomic::Ordering::Relaxed))
            .collect(),
    )
}

/// Writes `bytes` back into a `U8Vec`'s registry buffer after a JIT body
/// mutated the marshalled copy, so the caller observes in-place mutations.
pub(crate) fn u8vec_write_back(v: &Value, bytes: &[u8]) {
    let Some(handle) = struct_handle(v, "U8Vec") else {
        return;
    };
    let Some(arc) = u8vec_lookup(handle) else {
        return;
    };
    for (slot, &b) in arc.iter().zip(bytes.iter()) {
        slot.store(b, std::sync::atomic::Ordering::Relaxed);
    }
}

fn arg_int(args: &[Value], idx: usize) -> Option<i64> {
    args.get(idx).and_then(Value::as_i64)
}

fn non_negative_arg(args: &[Value], idx: usize, default: i64, label: &str) -> RuntimeResult<usize> {
    let n = arg_int(args, idx).unwrap_or(default);
    if n < 0 {
        return Err(RuntimeError::Type(format!("{label} must be non-negative")));
    }
    usize::try_from(n).map_err(|_| RuntimeError::Type(format!("{label} is too large")))
}

fn positive_arg(args: &[Value], idx: usize, default: i64, label: &str) -> RuntimeResult<usize> {
    let n = arg_int(args, idx).unwrap_or(default);
    if n <= 0 {
        return Err(RuntimeError::Type(format!("{label} must be positive")));
    }
    usize::try_from(n).map_err(|_| RuntimeError::Type(format!("{label} is too large")))
}

fn builtin_i64vec_new(args: &[Value]) -> RuntimeResult<Value> {
    let len = non_negative_arg(args, 0, 0, "I64Vec::new: length")?;
    let mut data: Vec<AtomicI64> = Vec::with_capacity(len);
    for _ in 0..len {
        data.push(AtomicI64::new(0));
    }
    let handle = i64vec_register(Arc::new(data));
    Ok(make_handle_struct("I64Vec", handle))
}

fn builtin_i64vec_set_at(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args
        .first()
        .and_then(|v| struct_handle(v, "I64Vec"))
        .ok_or_else(|| RuntimeError::Type("set_at: receiver must be I64Vec".to_string()))?;
    let vec_arc = i64vec_lookup(handle)
        .ok_or_else(|| RuntimeError::Type("set_at: stale I64Vec handle".to_string()))?;
    let idx = arg_int(args, 1)
        .ok_or_else(|| RuntimeError::Type("set_at: idx must be i64".to_string()))?;
    let val = arg_int(args, 2)
        .ok_or_else(|| RuntimeError::Type("set_at: val must be i64".to_string()))?;
    if idx >= 0 {
        if let Some(slot) = vec_arc.get(idx as usize) {
            slot.store(val, std::sync::atomic::Ordering::Relaxed);
        }
    }
    Ok(Value::Unit)
}

fn builtin_i64vec_get_at(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args
        .first()
        .and_then(|v| struct_handle(v, "I64Vec"))
        .ok_or_else(|| RuntimeError::Type("get_at: receiver must be I64Vec".to_string()))?;
    let vec_arc = i64vec_lookup(handle)
        .ok_or_else(|| RuntimeError::Type("get_at: stale I64Vec handle".to_string()))?;
    let idx = arg_int(args, 1)
        .ok_or_else(|| RuntimeError::Type("get_at: idx must be i64".to_string()))?;
    let v = if idx >= 0 {
        vec_arc
            .get(idx as usize)
            .map_or(0, |s| s.load(std::sync::atomic::Ordering::Relaxed))
    } else {
        0
    };
    Ok(Value::Int(v))
}

fn builtin_i64vec_vec_len(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args
        .first()
        .and_then(|v| struct_handle(v, "I64Vec"))
        .ok_or_else(|| RuntimeError::Type("vec_len: receiver must be I64Vec".to_string()))?;
    let vec_arc = i64vec_lookup(handle)
        .ok_or_else(|| RuntimeError::Type("vec_len: stale I64Vec handle".to_string()))?;
    Ok(Value::Int(vec_arc.len() as i64))
}

fn builtin_i64vec_write_range_to_stdout(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args
        .first()
        .and_then(|v| struct_handle(v, "I64Vec"))
        .ok_or_else(|| {
            RuntimeError::Type("write_range_to_stdout: receiver must be I64Vec".to_string())
        })?;
    let vec_arc = i64vec_lookup(handle).ok_or_else(|| {
        RuntimeError::Type("write_range_to_stdout: stale I64Vec handle".to_string())
    })?;
    let off = non_negative_arg(args, 1, 0, "write_range_to_stdout: offset")?;
    let count = non_negative_arg(args, 2, 0, "write_range_to_stdout: count")?;
    let end = off.saturating_add(count).min(vec_arc.len());
    let mut buf = Vec::with_capacity(end.saturating_sub(off));
    for i in off..end {
        buf.push((vec_arc[i].load(std::sync::atomic::Ordering::Relaxed) & 0xff) as u8);
    }
    write_stdout_bytes(&buf);
    Ok(Value::Unit)
}

fn builtin_i64vec_write_lines_to_stdout(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args
        .first()
        .and_then(|v| struct_handle(v, "I64Vec"))
        .ok_or_else(|| {
            RuntimeError::Type("write_lines_to_stdout: receiver must be I64Vec".to_string())
        })?;
    let vec_arc = i64vec_lookup(handle).ok_or_else(|| {
        RuntimeError::Type("write_lines_to_stdout: stale I64Vec handle".to_string())
    })?;
    let off = non_negative_arg(args, 1, 0, "write_lines_to_stdout: offset")?;
    let count = non_negative_arg(args, 2, 0, "write_lines_to_stdout: count")?;
    let line_len = positive_arg(args, 3, 60, "write_lines_to_stdout: line length")?;
    let end = off.saturating_add(count).min(vec_arc.len());
    let mut buf = Vec::with_capacity(end.saturating_sub(off) + count / line_len + 1);
    let mut i = off;
    while i < end {
        let upper = (i + line_len).min(end);
        for j in i..upper {
            buf.push((vec_arc[j].load(std::sync::atomic::Ordering::Relaxed) & 0xff) as u8);
        }
        buf.push(b'\n');
        i = upper;
    }
    write_stdout_bytes(&buf);
    Ok(Value::Unit)
}

// ------------------------------------------------------------------
// U8Vec - 1-byte-per-element heap vec for fasta scratch buffers.
//
// Same handle-table shape as I64Vec; storage uses `AtomicU8` so
// goroutine workers can write disjoint slices without locks.

static U8VEC_REGISTRY: parking_lot::Mutex<Vec<Option<Arc<Vec<std::sync::atomic::AtomicU8>>>>> =
    parking_lot::Mutex::new(Vec::new());

fn u8vec_register(arc: Arc<Vec<std::sync::atomic::AtomicU8>>) -> i64 {
    let mut reg = U8VEC_REGISTRY.lock();
    for (i, slot) in reg.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(arc);
            return i as i64;
        }
    }
    let id = reg.len() as i64;
    reg.push(Some(arc));
    id
}

thread_local! {
    /// Single-slot per-thread cache for the most recent U8Vec
    /// resolution. Hot byte-scan loops issue millions of
    /// `buf.get_byte(_)` calls against one buffer; a trivial
    /// cache on `(handle, Arc)` skips the global registry
    /// mutex entirely after the first lookup.
    static U8VEC_LAST: std::cell::RefCell<Option<(i64, Arc<Vec<std::sync::atomic::AtomicU8>>)>> =
        const { std::cell::RefCell::new(None) };
}

fn u8vec_lookup(handle: i64) -> Option<Arc<Vec<std::sync::atomic::AtomicU8>>> {
    if handle < 0 {
        return None;
    }
    let cached = U8VEC_LAST.with(|cell| {
        cell.borrow()
            .as_ref()
            .filter(|(h, _)| *h == handle)
            .map(|(_, arc)| Arc::clone(arc))
    });
    if cached.is_some() {
        return cached;
    }
    let reg = U8VEC_REGISTRY.lock();
    let arc = reg.get(handle as usize).and_then(std::clone::Clone::clone);
    if let Some(ref a) = arc {
        let cached = Arc::clone(a);
        U8VEC_LAST.with(|cell| *cell.borrow_mut() = Some((handle, cached)));
    }
    arc
}

/// Inline `set_byte` for the VM's `Op::U8VecSetByte` super-instruction.
/// Skips the `args: &[Value]` round-trip and the per-arg
/// `MapKey`-style discriminant matching that
/// [`builtin_u8vec_set_byte`] does. Returns `true` on success;
/// `false` lets the caller fall back to the generic method
/// dispatch path when the receiver shape doesn't match.
#[inline]
pub(crate) fn u8vec_set_byte_inline(handle: i64, idx: i64, byte: i64) -> bool {
    let Some(arc) = u8vec_lookup(handle) else {
        return false;
    };
    if idx < 0 {
        // Out-of-range writes are silently dropped, matching
        // `builtin_u8vec_set_byte`'s `if let Some(slot)` branch.
        return true;
    }
    if let Some(slot) = arc.get(idx as usize) {
        slot.store(byte as u8, std::sync::atomic::Ordering::Relaxed);
    }
    true
}

/// Inline `get_byte` for the VM's `Op::U8VecGetByte`. Returns
/// `None` when the handle is stale (caller falls back to the
/// generic dispatch path); returns `Some(0)` for out-of-range
/// reads, matching [`builtin_u8vec_get_byte`].
#[inline]
pub(crate) fn u8vec_get_byte_inline(handle: i64, idx: i64) -> Option<i64> {
    let arc = u8vec_lookup(handle)?;
    if idx < 0 {
        return Some(0);
    }
    Some(arc.get(idx as usize).map_or(0, |s| {
        i64::from(s.load(std::sync::atomic::Ordering::Relaxed))
    }))
}

fn builtin_u8vec_new(args: &[Value]) -> RuntimeResult<Value> {
    let len = non_negative_arg(args, 0, 0, "U8Vec::new: length")?;
    let mut data: Vec<std::sync::atomic::AtomicU8> = Vec::with_capacity(len);
    for _ in 0..len {
        data.push(std::sync::atomic::AtomicU8::new(0));
    }
    let handle = u8vec_register(Arc::new(data));
    Ok(make_handle_struct("U8Vec", handle))
}

fn builtin_u8vec_set_byte(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args
        .first()
        .and_then(|v| struct_handle(v, "U8Vec"))
        .ok_or_else(|| RuntimeError::Type("set_byte: receiver must be U8Vec".to_string()))?;
    let vec_arc = u8vec_lookup(handle)
        .ok_or_else(|| RuntimeError::Type("set_byte: stale U8Vec handle".to_string()))?;
    let idx = arg_int(args, 1)
        .ok_or_else(|| RuntimeError::Type("set_byte: idx must be i64".to_string()))?;
    let val = arg_int(args, 2)
        .ok_or_else(|| RuntimeError::Type("set_byte: val must be i64".to_string()))?;
    if idx >= 0 {
        if let Some(slot) = vec_arc.get(idx as usize) {
            slot.store(val as u8, std::sync::atomic::Ordering::Relaxed);
        }
    }
    Ok(Value::Unit)
}

fn builtin_u8vec_get_byte(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args
        .first()
        .and_then(|v| struct_handle(v, "U8Vec"))
        .ok_or_else(|| RuntimeError::Type("get_byte: receiver must be U8Vec".to_string()))?;
    let vec_arc = u8vec_lookup(handle)
        .ok_or_else(|| RuntimeError::Type("get_byte: stale U8Vec handle".to_string()))?;
    let idx = arg_int(args, 1)
        .ok_or_else(|| RuntimeError::Type("get_byte: idx must be i64".to_string()))?;
    let v = if idx >= 0 {
        vec_arc.get(idx as usize).map_or(0, |s| {
            i64::from(s.load(std::sync::atomic::Ordering::Relaxed))
        })
    } else {
        0
    };
    Ok(Value::Int(v))
}

fn builtin_u8vec_count_singles(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args
        .first()
        .and_then(|v| struct_handle(v, "U8Vec"))
        .ok_or_else(|| RuntimeError::Type("count_singles: receiver must be U8Vec".to_string()))?;
    let vec_arc = u8vec_lookup(handle)
        .ok_or_else(|| RuntimeError::Type("count_singles: stale U8Vec handle".to_string()))?;
    let buf_len = non_negative_arg(args, 1, 0, "to_string_left: length")?;
    let len = vec_arc.len().min(buf_len);
    let mut counts = [0i64; 4];
    for slot in &vec_arc[..len] {
        let b = slot.load(std::sync::atomic::Ordering::Relaxed) as usize;
        if b < 4 {
            counts[b] += 1;
        }
    }
    Ok(Value::IntArray(Arc::new(counts.to_vec())))
}

fn builtin_u8vec_count_pairs(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args
        .first()
        .and_then(|v| struct_handle(v, "U8Vec"))
        .ok_or_else(|| RuntimeError::Type("count_pairs: receiver must be U8Vec".to_string()))?;
    let vec_arc = u8vec_lookup(handle)
        .ok_or_else(|| RuntimeError::Type("count_pairs: stale U8Vec handle".to_string()))?;
    let buf_len = non_negative_arg(args, 1, 0, "to_string_center: length")?;
    let len = vec_arc.len().min(buf_len);
    let mut counts = [0i64; 16];
    if len < 2 {
        return Ok(Value::IntArray(Arc::new(counts.to_vec())));
    }
    let stop = len - 1;
    for j in 0..stop {
        let a = vec_arc[j].load(std::sync::atomic::Ordering::Relaxed) as usize;
        let b = vec_arc[j + 1].load(std::sync::atomic::Ordering::Relaxed) as usize;
        let idx = (a << 2) | b;
        if idx < 16 {
            counts[idx] += 1;
        }
    }
    Ok(Value::IntArray(Arc::new(counts.to_vec())))
}

fn builtin_u8vec_count_kmers(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args
        .first()
        .and_then(|v| struct_handle(v, "U8Vec"))
        .ok_or_else(|| RuntimeError::Type("count_kmers: receiver must be U8Vec".to_string()))?;
    let vec_arc = u8vec_lookup(handle)
        .ok_or_else(|| RuntimeError::Type("count_kmers: stale U8Vec handle".to_string()))?;
    let buf_len = non_negative_arg(args, 1, 0, "to_string_right: length")?;
    let k = non_negative_arg(args, 2, 0, "to_string_right: count")?;
    let len = vec_arc.len().min(buf_len);
    let counts = kmer_count(&vec_arc[..len], k);
    Ok(Value::IntMap(Arc::new(parking_lot::Mutex::new(counts))))
}

/// Scans `buf` with a sliding window of size `k`, packing each
/// window into a 2-bit-per-byte `i64` key and accumulating the
/// frequency. Tight C-side loop replacing the `while`-loop +
/// `Op::IntMapInc` chain user code would emit. Pre-allocates
/// the map with a sane capacity (capped well below the worst-
/// case buffer length so a k=18 call does not reserve a large dense
/// table up front. The cap keeps steady-state RSS predictable for
/// the small-k calls.
// Soft cap on the pre-allocated map capacity: 64 K slots
// cap just keeps steady-state RSS predictable for the small-k calls
// without paying catastrophic up-front cost on k=18.
const KMER_CAP_SOFT: usize = 64 * 1024;

#[inline]
fn kmer_count(buf: &[std::sync::atomic::AtomicU8], k: usize) -> DenseMap<i64, i64> {
    let upper_by_alphabet = if k == 0 || k >= 32 {
        usize::MAX
    } else {
        1usize.checked_shl((k as u32) * 2).unwrap_or(usize::MAX)
    };
    let cap = upper_by_alphabet.clamp(64, KMER_CAP_SOFT);
    let mut counts = dense_map_with_capacity(cap);
    if k == 0 || k > buf.len() {
        return counts;
    }
    let stop = buf.len() - k + 1;
    // Rolling key: drop the high 2 bits, shift, OR in the new
    // byte. Keeps the inner loop O(1) per iter regardless of k.
    let mask: i64 = if k >= 32 { -1 } else { (1i64 << (k * 2)) - 1 };
    let mut key: i64 = 0;
    for slot in buf.iter().take(k) {
        let b = slot.load(std::sync::atomic::Ordering::Relaxed);
        key = (key << 2) | i64::from(b);
    }
    *counts.entry(key).or_insert(0) += 1;
    let mut i = 1usize;
    while i < stop {
        let b = buf[i + k - 1].load(std::sync::atomic::Ordering::Relaxed);
        key = ((key << 2) | i64::from(b)) & mask;
        *counts.entry(key).or_insert(0) += 1;
        i += 1;
    }
    counts
}

fn builtin_u8vec_window_key(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args
        .first()
        .and_then(|v| struct_handle(v, "U8Vec"))
        .ok_or_else(|| RuntimeError::Type("window_key: receiver must be U8Vec".to_string()))?;
    let vec_arc = u8vec_lookup(handle)
        .ok_or_else(|| RuntimeError::Type("window_key: stale U8Vec handle".to_string()))?;
    let i = non_negative_arg(args, 1, 0, "increment_sorted: index")?;
    let k = non_negative_arg(args, 2, 0, "increment_sorted: count")?;
    let len = vec_arc.len();
    let mut key: i64 = 0;
    let stop = i.saturating_add(k).min(len);
    for j in i..stop {
        let b = vec_arc[j].load(std::sync::atomic::Ordering::Relaxed);
        key = (key << 2) | i64::from(b);
    }
    // Out-of-range tail: zero-extend remaining slots (matches
    // the by-byte loop's behaviour when `i + k` overshoots).
    let tail = (i + k).saturating_sub(stop);
    for _ in 0..tail {
        key <<= 2;
    }
    Ok(Value::Int(key))
}

fn builtin_u8vec_byte_len(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args
        .first()
        .and_then(|v| struct_handle(v, "U8Vec"))
        .ok_or_else(|| RuntimeError::Type("byte_len: receiver must be U8Vec".to_string()))?;
    let vec_arc = u8vec_lookup(handle)
        .ok_or_else(|| RuntimeError::Type("byte_len: stale U8Vec handle".to_string()))?;
    Ok(Value::Int(vec_arc.len() as i64))
}

/// `Vec::new()` - empty growable array. Used by `let mut v:
/// Vec<T> = Vec::new()` patterns; without this entry the path
/// lookup falls through to the bare `new` global, which is the
/// last-installed module's `new` (currently `HashMap::new`).
fn builtin_vec_new(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::empty_array())
}

/// `Vec::from(array)` converts the fixed-size array value to its growable
/// representation. Both share the VM's array value representation.
fn builtin_vec_from(args: &[Value]) -> RuntimeResult<Value> {
    args.first()
        .cloned()
        .ok_or_else(|| RuntimeError::Type("Vec::from: missing array".to_string()))
}

/// `Vec::with_capacity(n)` - an empty growable array with room for `n` elements.
fn builtin_vec_with_capacity(args: &[Value]) -> RuntimeResult<Value> {
    let storage = crate::value::vec_with_capacity(arg_int(args, 0).unwrap_or(0))?;
    Ok(Value::Array(Arc::new(storage)))
}

/// `buf.to_string(len)` - freezes the first `len` bytes of a
/// `U8Vec` build buffer into an immutable `String`. Mirrors the
/// canonical immutable-string-language idiom: a mutable buffer
/// for incremental construction, an explicit one-shot conversion
/// at the end.
fn builtin_u8vec_to_string(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args
        .first()
        .and_then(|v| struct_handle(v, "U8Vec"))
        .ok_or_else(|| RuntimeError::Type("to_string: receiver must be U8Vec".to_string()))?;
    let vec_arc = u8vec_lookup(handle)
        .ok_or_else(|| RuntimeError::Type("to_string: stale U8Vec handle".to_string()))?;
    let len = match arg_int(args, 1) {
        Some(n) if n < 0 => {
            return Err(RuntimeError::Type(
                "to_string: length must be non-negative".to_string(),
            ));
        }
        Some(n) => usize::try_from(n)
            .map_err(|_| RuntimeError::Type("to_string: length is too large".to_string()))?,
        None => vec_arc.len(),
    };
    let take = len.min(vec_arc.len());
    let mut bytes = Vec::with_capacity(take);
    for slot in vec_arc.iter().take(take) {
        bytes.push(slot.load(std::sync::atomic::Ordering::Relaxed));
    }
    let s = String::from_utf8(bytes)
        .map_err(|_| RuntimeError::Type("to_string: U8Vec contents are not UTF-8".to_string()))?;
    Ok(Value::String(SmolStr::from(s)))
}

fn builtin_u8vec_write_byte_range_to_stdout(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args
        .first()
        .and_then(|v| struct_handle(v, "U8Vec"))
        .ok_or_else(|| {
            RuntimeError::Type("write_byte_range_to_stdout: receiver must be U8Vec".to_string())
        })?;
    let vec_arc = u8vec_lookup(handle).ok_or_else(|| {
        RuntimeError::Type("write_byte_range_to_stdout: stale U8Vec handle".to_string())
    })?;
    let off = non_negative_arg(args, 1, 0, "write_byte_range_to_stdout: offset")?;
    let count = non_negative_arg(args, 2, 0, "write_byte_range_to_stdout: count")?;
    let end = off.saturating_add(count).min(vec_arc.len());
    let mut buf = Vec::with_capacity(end.saturating_sub(off));
    for i in off..end {
        buf.push(vec_arc[i].load(std::sync::atomic::Ordering::Relaxed));
    }
    write_stdout_bytes(&buf);
    Ok(Value::Unit)
}

fn builtin_u8vec_write_byte_lines_to_stdout(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args
        .first()
        .and_then(|v| struct_handle(v, "U8Vec"))
        .ok_or_else(|| {
            RuntimeError::Type("write_byte_lines_to_stdout: receiver must be U8Vec".to_string())
        })?;
    let vec_arc = u8vec_lookup(handle).ok_or_else(|| {
        RuntimeError::Type("write_byte_lines_to_stdout: stale U8Vec handle".to_string())
    })?;
    let off = non_negative_arg(args, 1, 0, "write_byte_lines_to_stdout: offset")?;
    let count = non_negative_arg(args, 2, 0, "write_byte_lines_to_stdout: count")?;
    let line_len = positive_arg(args, 3, 60, "write_byte_lines_to_stdout: line length")?;
    let end = off.saturating_add(count).min(vec_arc.len());
    let mut buf = Vec::with_capacity(end.saturating_sub(off) + (end - off) / line_len + 1);
    let mut i = off;
    while i < end {
        let upper = (i + line_len).min(end);
        for j in i..upper {
            buf.push(vec_arc[j].load(std::sync::atomic::Ordering::Relaxed));
        }
        buf.push(b'\n');
        i = upper;
    }
    write_stdout_bytes(&buf);
    Ok(Value::Unit)
}

fn builtin_waitgroup_new(_args: &[Value]) -> RuntimeResult<Value> {
    let cell = Arc::new(WaitGroupCell {
        counter: parking_lot::Mutex::new(0),
        cond: parking_lot::Condvar::new(),
    });
    let handle = wg_register(cell);
    Ok(make_handle_struct("WaitGroup", handle))
}

fn builtin_waitgroup_add(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args
        .first()
        .and_then(|v| struct_handle(v, "WaitGroup"))
        .ok_or_else(|| {
            RuntimeError::Type("WaitGroup::add: receiver must be WaitGroup".to_string())
        })?;
    let cell = wg_lookup(handle)
        .ok_or_else(|| RuntimeError::Type("WaitGroup::add: stale WaitGroup handle".to_string()))?;
    let n = arg_int(args, 1).unwrap_or(1);
    *cell.counter.lock() += n;
    Ok(Value::Unit)
}

fn builtin_waitgroup_done(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args
        .first()
        .and_then(|v| struct_handle(v, "WaitGroup"))
        .ok_or_else(|| {
            RuntimeError::Type("WaitGroup::done: receiver must be WaitGroup".to_string())
        })?;
    let cell = wg_lookup(handle)
        .ok_or_else(|| RuntimeError::Type("WaitGroup::done: stale WaitGroup handle".to_string()))?;
    let mut count = cell.counter.lock();
    *count -= 1;
    if *count <= 0 {
        cell.cond.notify_all();
    }
    Ok(Value::Unit)
}

fn builtin_waitgroup_wait(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args
        .first()
        .and_then(|v| struct_handle(v, "WaitGroup"))
        .ok_or_else(|| {
            RuntimeError::Type("WaitGroup::wait: receiver must be WaitGroup".to_string())
        })?;
    let cell = wg_lookup(handle)
        .ok_or_else(|| RuntimeError::Type("WaitGroup::wait: stale WaitGroup handle".to_string()))?;
    let mut count = cell.counter.lock();
    if *count > 0 && !gossamer_runtime::platform::CAN_BLOCK {
        return Err(RuntimeError::WouldNeverWake("WaitGroup::wait"));
    }
    while *count > 0 {
        cell.cond.wait(&mut count);
    }
    Ok(Value::Unit)
}

/// `wg.wait_ctx(ctx)` - waits for the counter to reach zero unless the
/// context fires first, answering which of the two happened. The wait is
/// split into short steps so a cancellation raised while waiting is
/// observed promptly.
fn builtin_waitgroup_wait_ctx(args: &[Value]) -> RuntimeResult<Value> {
    let handle = args
        .first()
        .and_then(|v| struct_handle(v, "WaitGroup"))
        .ok_or_else(|| {
            RuntimeError::Type("WaitGroup::wait_ctx: receiver must be WaitGroup".to_string())
        })?;
    let cell = wg_lookup(handle).ok_or_else(|| {
        RuntimeError::Type("WaitGroup::wait_ctx: stale WaitGroup handle".to_string())
    })?;
    let Some(ctx) = args.get(1) else {
        return Ok(Value::Bool(true));
    };
    loop {
        let mut count = cell.counter.lock();
        if *count <= 0 {
            return Ok(Value::Bool(true));
        }
        if crate::stdlib_builtins::context::value_is_cancelled(ctx) {
            return Ok(Value::Bool(false));
        }
        if !gossamer_runtime::platform::CAN_BLOCK {
            return Err(RuntimeError::WouldNeverWake("WaitGroup::wait_ctx"));
        }
        cell.cond
            .wait_for(&mut count, std::time::Duration::from_millis(5));
    }
}

fn builtin_lcg_jump(args: &[Value]) -> RuntimeResult<Value> {
    let state = arg_int(args, 0).unwrap_or(0);
    let ia = arg_int(args, 1).unwrap_or(0);
    let ic = arg_int(args, 2).unwrap_or(0);
    let im = arg_int(args, 3).unwrap_or(1);
    let n = arg_int(args, 4).unwrap_or(0);
    Ok(Value::Int(lcg_jump_compute(state, ia, ic, im, n)))
}

// O(log n) modular exponentiation on the affine LCG transform.
// Mirrors `gos_rt_lcg_jump` in `gossamer-runtime`. Uses i128 internally
// so the intermediate `a * a` and `c * a + c` products do not overflow
// for the bench-game parameter set (im = 139_968).
fn lcg_jump_compute(state: i64, ia: i64, ic: i64, im: i64, n: i64) -> i64 {
    if im <= 0 || n <= 0 {
        return state;
    }
    let modu = i128::from(im);
    let mut a_pow = i128::from(ia).rem_euclid(modu);
    let mut c_pow = i128::from(ic).rem_euclid(modu);
    let mut x = i128::from(state).rem_euclid(modu);
    let mut k = n;
    let mut acc_a: i128 = 1;
    let mut acc_c: i128 = 0;
    while k > 0 {
        if k & 1 == 1 {
            acc_a = (acc_a * a_pow).rem_euclid(modu);
            acc_c = (acc_c * a_pow + c_pow).rem_euclid(modu);
        }
        let a_new = (a_pow * a_pow).rem_euclid(modu);
        let c_new = (c_pow * a_pow + c_pow).rem_euclid(modu);
        a_pow = a_new;
        c_pow = c_new;
        k >>= 1;
    }
    x = (x * acc_a + acc_c).rem_euclid(modu);
    x as i64
}

fn builtin_stream_write_byte_array(args: &[Value]) -> RuntimeResult<Value> {
    let fd = args.first().map_or(1, stream_fd);
    let count = non_negative_arg(args, 2, 0, "sha256_blocks: count")?;
    let mut buf = Vec::with_capacity(count);
    match args.get(1) {
        Some(Value::IntArray(data)) => {
            for &b in data.iter().take(count) {
                buf.push((b & 0xff) as u8);
            }
        }
        Some(Value::Array(arr)) => {
            for v in arr.iter().take(count) {
                if let Value::Int(b) = v {
                    buf.push((*b & 0xff) as u8);
                }
            }
        }
        _ => {}
    }
    if fd == 2 {
        write_stderr_bytes(&buf);
    } else {
        write_stdout_bytes(&buf);
    }
    Ok(Value::Unit)
}

fn write_stdout_bytes(bytes: &[u8]) {
    if let Ok(text) = std::str::from_utf8(bytes) {
        write_stdout(text);
        return;
    }
    // Lossy fallback for sequences that aren't valid UTF-8 - should
    // not happen in fasta-shaped programs, but keeps the writer
    // contract honest.
    let lossy = String::from_utf8_lossy(bytes);
    write_stdout(&lossy);
}

fn write_stderr_bytes(bytes: &[u8]) {
    if let Ok(text) = std::str::from_utf8(bytes) {
        write_stderr(text);
        return;
    }
    let lossy = String::from_utf8_lossy(bytes);
    write_stderr(&lossy);
}

#[cfg(test)]
mod tests {
    use super::*;
    use smallvec::smallvec;

    fn expect_byte_vec(value: Value) -> Vec<u8> {
        match value {
            Value::ByteVec(bytes) => bytes.as_ref().clone(),
            other => panic!("expected ByteVec, got {other:?}"),
        }
    }

    #[test]
    fn hashmap_with_capacity_preserves_string_key_semantics() {
        let map = builtin_map_with_capacity(&[Value::Int(4)]).expect("constructor");
        let key = Value::String(SmolStr::from("present"));
        let inserted =
            builtin_map_insert(&[map.clone(), key.clone(), Value::Int(42)]).expect("insert");
        assert!(matches!(inserted, Value::Variant(ref variant) if variant.name.as_str() == "None"));
        let value = builtin_map_get_or(&[map, key, Value::Int(-1)]).expect("get_or");
        assert!(matches!(value, Value::Int(42)));
    }

    #[test]
    fn push_handles_byte_backed_vectors_on_generic_path() {
        let fixed = Value::ByteArray(Arc::new(vec![3, 43].into()));
        let inline = Value::InlineByteArray(Arc::new(smallvec![3, 43]));
        let growable = Value::ByteVec(Arc::new(vec![3, 43]));

        assert_eq!(
            expect_byte_vec(builtin_push(&[fixed, Value::Int(83)]).unwrap()),
            vec![3, 43, 83]
        );
        assert_eq!(
            expect_byte_vec(builtin_push(&[inline, Value::Int(83)]).unwrap()),
            vec![3, 43, 83]
        );
        assert_eq!(
            expect_byte_vec(builtin_push(&[growable, Value::Int(83)]).unwrap()),
            vec![3, 43, 83]
        );
    }

    #[test]
    fn builtin_split_returns_array_of_segments_for_string_receiver() {
        let args = vec![
            Value::String(SmolStr::from("a/b/c".to_string())),
            Value::String(SmolStr::from("/".to_string())),
        ];
        let Ok(Value::Array(parts)) = builtin_split(&args) else {
            panic!("expected array");
        };
        let texts: Vec<&str> = parts
            .iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["a", "b", "c"]);
    }

    #[test]
    fn builtin_split_handles_char_delimiter_argument() {
        let args = vec![
            Value::String(SmolStr::from("one two three".to_string())),
            Value::Char(' '),
        ];
        let Ok(Value::Array(parts)) = builtin_split(&args) else {
            panic!("expected array");
        };
        assert_eq!(parts.len(), 3);
    }

    #[test]
    fn builtin_trim_strips_ascii_whitespace_on_both_sides() {
        let args = vec![Value::String(SmolStr::from("  hello \t ".to_string()))];
        let Ok(Value::String(out)) = builtin_trim(&args) else {
            panic!("expected string");
        };
        assert_eq!(out.as_str(), "hello");
    }

    fn header_pair(name: &str, value: &str) -> Value {
        Value::Tuple(Arc::from(vec![
            Value::String(SmolStr::from(name.to_string())),
            Value::String(SmolStr::from(value.to_string())),
        ]))
    }

    fn response_value(content_type: Option<&str>, headers: Option<Vec<Value>>) -> Value {
        let mut fields = vec![
            ("status", Value::Int(200)),
            ("body", Value::String(SmolStr::from("ok".to_string()))),
        ];
        if let Some(ct) = content_type {
            fields.push(("content_type", Value::String(SmolStr::from(ct.to_string()))));
        }
        if let Some(items) = headers {
            fields.push(("headers", Value::Array(Arc::new(items))));
        }
        Value::struct_("Response", fields)
    }

    #[test]
    fn value_to_response_explicit_content_type_header_wins_over_field() {
        let value = response_value(
            Some("application/json"),
            Some(vec![header_pair("Content-Type", "text/html")]),
        );
        let response = value_to_response(&value).expect("response");
        assert_eq!(response.headers.get("content-type"), Some("text/html"));
    }

    #[test]
    fn value_to_response_content_type_field_used_when_no_explicit_header() {
        let value = response_value(
            Some("application/json"),
            Some(vec![header_pair("x-a", "1")]),
        );
        let response = value_to_response(&value).expect("response");
        assert_eq!(
            response.headers.get("content-type"),
            Some("application/json")
        );
        assert_eq!(response.headers.get("x-a"), Some("1"));
    }

    #[test]
    fn value_to_response_defaults_to_text_plain_when_neither_set() {
        let value = response_value(None, None);
        let response = value_to_response(&value).expect("response");
        assert_eq!(
            response.headers.get("content-type"),
            Some("text/plain; charset=utf-8")
        );
        assert_eq!(response.headers.get("content-length"), Some("2"));
    }

    #[test]
    fn value_to_response_honors_explicit_content_length_header() {
        let value = response_value(None, Some(vec![header_pair("Content-Length", "99")]));
        let response = value_to_response(&value).expect("response");
        assert_eq!(response.headers.get("content-length"), Some("99"));
    }

    #[test]
    fn with_header_replaces_same_name_case_insensitively_then_pushes() {
        let r0 = response_value(None, None);
        let r1 = builtin_http_response_with_header(&[
            r0,
            Value::String(SmolStr::from("x-a".to_string())),
            Value::String(SmolStr::from("1".to_string())),
        ])
        .expect("with_header");
        let r2 = builtin_http_response_with_header(&[
            r1,
            Value::String(SmolStr::from("X-A".to_string())),
            Value::String(SmolStr::from("2".to_string())),
        ])
        .expect("with_header");
        let Value::Struct(inner) = &r2 else {
            panic!("expected struct");
        };
        let Some((_, Value::Array(items))) = inner
            .fields
            .iter()
            .find(|(ident, _)| (**ident) == "headers")
        else {
            panic!("expected headers array");
        };
        assert_eq!(items.len(), 1, "replace-then-push keeps one entry");
        let Value::Tuple(kv) = &items[0] else {
            panic!("expected tuple");
        };
        let (Some(Value::String(name)), Some(Value::String(value))) = (kv.first(), kv.get(1))
        else {
            panic!("expected string pair");
        };
        assert_eq!(name.as_str(), "X-A");
        assert_eq!(value.as_str(), "2");
    }

    #[test]
    fn with_header_chain_of_three_keeps_last_duplicate_and_distinct_names() {
        let mut response = response_value(None, None);
        for (name, value) in [("x-a", "1"), ("X-A", "2"), ("x-b", "3")] {
            response = builtin_http_response_with_header(&[
                response,
                Value::String(SmolStr::from(name.to_string())),
                Value::String(SmolStr::from(value.to_string())),
            ])
            .expect("with_header");
        }
        let rendered = value_to_response(&response).expect("response");
        assert_eq!(rendered.headers.get("x-a"), Some("2"));
        assert_eq!(rendered.headers.get("x-b"), Some("3"));
        assert_eq!(
            rendered.headers.get("content-type"),
            Some("text/plain; charset=utf-8")
        );
    }

    #[test]
    fn errors_new_displays_message_only() {
        let e = builtin_errors_new(&[Value::String(SmolStr::from("boom"))]).expect("new");
        assert_eq!(format!("{e}"), "boom");
    }

    #[test]
    fn wrapped_error_displays_colon_joined_chain() {
        let root = builtin_errors_new(&[Value::String(SmolStr::from("root"))]).expect("new");
        let mid = builtin_errors_wrap(&[root, Value::String(SmolStr::from("mid"))]).expect("wrap");
        let outer =
            builtin_errors_wrap(&[mid, Value::String(SmolStr::from("outer"))]).expect("wrap");
        assert_eq!(format!("{outer}"), "outer: mid: root");
        let msg = builtin_errors_message(&[outer]).expect("message");
        assert_eq!(format!("{msg}"), "outer", "message() stays top-level-only");
    }
}
