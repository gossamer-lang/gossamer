// The VM's half of typed serde: the `__gos_serde_*` steps the decode and
// encode walks that `gossamer-hir`'s serde pass generates call. The reader is
// `gossamer_core`'s, as on the compiled tiers, and the writer appends the
// bytes the runtime's writer appends, so every tier decodes and encodes alike.

/// Open readers and writers, by handle. A goroutine may resume on another
/// worker thread between two steps, so the tables are process-wide.
struct SerdeHandles {
    readers: Vec<Option<json_std::typed::TypedReader>>,
    writers: Vec<Option<Vec<u8>>>,
}

static SERDE_HANDLES: parking_lot::Mutex<SerdeHandles> = parking_lot::Mutex::new(SerdeHandles {
    readers: Vec::new(),
    writers: Vec::new(),
});

/// The slot a handle names: handles count from 1, so 0 names nothing.
fn serde_slot(args: &[Value]) -> Option<usize> {
    match args.first() {
        Some(Value::Int(n)) if *n > 0 => usize::try_from(*n - 1).ok(),
        _ => None,
    }
}

fn serde_text(args: &[Value], index: usize) -> &str {
    args.get(index).and_then(as_str).unwrap_or("")
}

fn serde_index(args: &[Value], index: usize) -> usize {
    match args.get(index) {
        Some(Value::Int(n)) => usize::try_from(*n).unwrap_or(usize::MAX),
        _ => usize::MAX,
    }
}

/// Runs `f` on the reader `args[0]` names, answering `fallback` for none.
fn with_reader<T>(
    args: &[Value],
    fallback: T,
    f: impl FnOnce(&mut json_std::typed::TypedReader) -> T,
) -> T {
    let Some(slot) = serde_slot(args) else {
        return fallback;
    };
    let mut handles = SERDE_HANDLES.lock();
    match handles.readers.get_mut(slot).and_then(Option::as_mut) {
        Some(reader) => f(reader),
        None => fallback,
    }
}

/// Runs `f` on the writer `args[0]` names.
fn with_writer(args: &[Value], f: impl FnOnce(&mut Vec<u8>)) {
    let Some(slot) = serde_slot(args) else {
        return;
    };
    let mut handles = SERDE_HANDLES.lock();
    if let Some(out) = handles.writers.get_mut(slot).and_then(Option::as_mut) {
        f(out);
    }
}

fn serde_handle(slot: usize) -> Value {
    Value::Int(i64::try_from(slot + 1).unwrap_or(0))
}

fn builtin_serde_rd_new(args: &[Value]) -> RuntimeResult<Value> {
    let reader = json_std::typed::TypedReader::new(serde_text(args, 0));
    let mut handles = SERDE_HANDLES.lock();
    let slot = if let Some(free) = handles.readers.iter().position(Option::is_none) {
        handles.readers[free] = Some(reader);
        free
    } else {
        handles.readers.push(Some(reader));
        handles.readers.len() - 1
    };
    Ok(serde_handle(slot))
}

fn builtin_serde_rd_free(args: &[Value]) -> RuntimeResult<Value> {
    if let Some(slot) = serde_slot(args) {
        let mut handles = SERDE_HANDLES.lock();
        if let Some(entry) = handles.readers.get_mut(slot) {
            *entry = None;
        }
    }
    Ok(Value::Unit)
}

fn builtin_serde_rd_failed(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Bool(with_reader(args, false, |r| r.failed())))
}

fn builtin_serde_rd_error(args: &[Value]) -> RuntimeResult<Value> {
    let error = with_reader(args, None, |r| r.error().cloned());
    let Some(error) = error else {
        return Ok(make_error_value(""));
    };
    let mut value = make_error_value(&error.message);
    for context in error.contexts {
        value = errors_struct(context, Value::variant("Some", vec![value]));
    }
    Ok(value)
}

fn builtin_serde_rd_fail(args: &[Value]) -> RuntimeResult<Value> {
    let message = serde_text(args, 1).to_owned();
    with_reader(args, (), |r| r.fail(&message));
    Ok(Value::Unit)
}

fn builtin_serde_rd_mark(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Int(i64::from(with_reader(args, false, |r| r.failed()))))
}

fn builtin_serde_rd_wrap(args: &[Value]) -> RuntimeResult<Value> {
    let mark = matches!(args.get(1), Some(Value::Int(n)) if *n != 0);
    let context = serde_text(args, 2).to_owned();
    with_reader(args, (), |r| r.wrap(mark, &context));
    Ok(Value::Unit)
}

fn builtin_serde_rd_leave(args: &[Value]) -> RuntimeResult<Value> {
    with_reader(args, (), json_std::typed::TypedReader::leave);
    Ok(Value::Unit)
}

fn builtin_serde_rd_is_null(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Bool(with_reader(args, true, |r| r.is_null())))
}

fn builtin_serde_rd_struct(args: &[Value]) -> RuntimeResult<Value> {
    let keys = serde_text(args, 1).to_owned();
    with_reader(args, (), |r| r.begin_struct(&keys));
    Ok(Value::Unit)
}

fn builtin_serde_rd_field(args: &[Value]) -> RuntimeResult<Value> {
    let index = serde_index(args, 1);
    Ok(Value::Bool(with_reader(args, false, |r| r.enter_field(index))))
}

fn builtin_serde_rd_i64(args: &[Value]) -> RuntimeResult<Value> {
    let message = serde_text(args, 1).to_owned();
    Ok(Value::Int(with_reader(args, 0, |r| r.read_i64(&message))))
}

fn builtin_serde_rd_u64(args: &[Value]) -> RuntimeResult<Value> {
    let message = serde_text(args, 1).to_owned();
    Ok(Value::Uint(with_reader(args, 0, |r| r.read_u64(&message))))
}

fn builtin_serde_rd_f64(args: &[Value]) -> RuntimeResult<Value> {
    let message = serde_text(args, 1).to_owned();
    Ok(Value::Float(with_reader(args, 0.0, |r| r.read_f64(&message))))
}

fn builtin_serde_rd_bool(args: &[Value]) -> RuntimeResult<Value> {
    let message = serde_text(args, 1).to_owned();
    Ok(Value::Bool(with_reader(args, false, |r| r.read_bool(&message))))
}

fn builtin_serde_rd_str(args: &[Value]) -> RuntimeResult<Value> {
    let message = serde_text(args, 1).to_owned();
    let text = with_reader(args, String::new(), |r| r.read_string(&message));
    Ok(Value::String(SmolStr::from(text)))
}

fn builtin_serde_rd_json(args: &[Value]) -> RuntimeResult<Value> {
    let raw = with_reader(args, String::from("null"), |r| r.raw_value().to_owned());
    let value = json_std::parse(&raw).unwrap_or(json_std::Value::Null);
    Ok(Value::Json(Arc::new(JsonInner::new(value))))
}

fn builtin_serde_rd_array(args: &[Value]) -> RuntimeResult<Value> {
    let message = serde_text(args, 1).to_owned();
    let count = with_reader(args, 0, |r| r.begin_array(&message));
    Ok(Value::Int(i64::try_from(count).unwrap_or(i64::MAX)))
}

fn builtin_serde_rd_tuple(args: &[Value]) -> RuntimeResult<Value> {
    let len = serde_index(args, 1);
    let not_array = serde_text(args, 2).to_owned();
    let wrong_len = serde_text(args, 3).to_owned();
    with_reader(args, (), |r| r.begin_tuple(len, &not_array, &wrong_len));
    Ok(Value::Unit)
}

fn builtin_serde_rd_elem(args: &[Value]) -> RuntimeResult<Value> {
    let index = serde_index(args, 1);
    with_reader(args, (), |r| r.enter_elem(index));
    Ok(Value::Unit)
}

fn builtin_serde_rd_at(args: &[Value]) -> RuntimeResult<Value> {
    let index = serde_index(args, 1);
    with_reader(args, (), |r| r.enter_at(index));
    Ok(Value::Unit)
}

fn builtin_serde_rd_map(args: &[Value]) -> RuntimeResult<Value> {
    let message = serde_text(args, 1).to_owned();
    let count = with_reader(args, 0, |r| r.begin_map(&message));
    Ok(Value::Int(i64::try_from(count).unwrap_or(i64::MAX)))
}

fn builtin_serde_rd_map_key(args: &[Value]) -> RuntimeResult<Value> {
    let index = serde_index(args, 1);
    let key = with_reader(args, String::new(), |r| r.map_key(index).to_owned());
    Ok(Value::String(SmolStr::from(key)))
}

fn builtin_serde_rd_map_value(args: &[Value]) -> RuntimeResult<Value> {
    let index = serde_index(args, 1);
    with_reader(args, (), |r| r.enter_map_value(index));
    Ok(Value::Unit)
}

fn builtin_serde_rd_enum(args: &[Value]) -> RuntimeResult<Value> {
    let variants = serde_text(args, 1).to_owned();
    let name = serde_text(args, 2).to_owned();
    let fallback = serde_index(args, 3);
    let chosen = with_reader(args, fallback, |r| r.begin_enum(&variants, &name, fallback));
    Ok(Value::Int(i64::try_from(chosen).unwrap_or(0)))
}

fn builtin_serde_wr_new(_args: &[Value]) -> RuntimeResult<Value> {
    let mut handles = SERDE_HANDLES.lock();
    let slot = if let Some(free) = handles.writers.iter().position(Option::is_none) {
        handles.writers[free] = Some(Vec::with_capacity(256));
        free
    } else {
        handles.writers.push(Some(Vec::with_capacity(256)));
        handles.writers.len() - 1
    };
    Ok(serde_handle(slot))
}

fn builtin_serde_wr_finish(args: &[Value]) -> RuntimeResult<Value> {
    let bytes = serde_slot(args)
        .and_then(|slot| SERDE_HANDLES.lock().writers.get_mut(slot).and_then(Option::take))
        .unwrap_or_default();
    Ok(Value::String(SmolStr::from(
        String::from_utf8_lossy(&bytes).into_owned(),
    )))
}

fn builtin_serde_wr_raw(args: &[Value]) -> RuntimeResult<Value> {
    let text = serde_text(args, 1).to_owned();
    with_writer(args, |out| out.extend_from_slice(text.as_bytes()));
    Ok(Value::Unit)
}

fn builtin_serde_wr_sep(args: &[Value]) -> RuntimeResult<Value> {
    if matches!(args.get(1), Some(Value::Int(n)) if *n > 0) {
        with_writer(args, |out| out.push(b','));
    }
    Ok(Value::Unit)
}

fn builtin_serde_wr_i64(args: &[Value]) -> RuntimeResult<Value> {
    let text = match args.get(1) {
        Some(Value::Int(n)) => n.to_string(),
        Some(Value::Uint(n)) => n.to_string(),
        _ => String::from("0"),
    };
    with_writer(args, |out| out.extend_from_slice(text.as_bytes()));
    Ok(Value::Unit)
}

fn builtin_serde_wr_u64(args: &[Value]) -> RuntimeResult<Value> {
    let text = match args.get(1) {
        Some(Value::Uint(n)) => n.to_string(),
        #[allow(
            clippy::cast_sign_loss,
            reason = "a u64 the VM holds in an Int slot is its two's-complement bits"
        )]
        Some(Value::Int(n)) => (*n as u64).to_string(),
        _ => String::from("0"),
    };
    with_writer(args, |out| out.extend_from_slice(text.as_bytes()));
    Ok(Value::Unit)
}

fn serde_float_arg(args: &[Value]) -> f64 {
    match args.get(1) {
        Some(Value::Float(f)) => *f,
        Some(other) => crate::value::f32_render_slot(other).unwrap_or(0.0),
        None => 0.0,
    }
}

fn builtin_serde_wr_f64(args: &[Value]) -> RuntimeResult<Value> {
    let value = serde_float_arg(args);
    let text = if value.is_finite() {
        gossamer_runtime::builtins::format_float(value)
    } else {
        String::from("null")
    };
    with_writer(args, |out| out.extend_from_slice(text.as_bytes()));
    Ok(Value::Unit)
}

fn builtin_serde_wr_f32(args: &[Value]) -> RuntimeResult<Value> {
    let value = serde_float_arg(args);
    let text = if value.is_finite() {
        gossamer_runtime::builtins::format_f32(value)
    } else {
        String::from("null")
    };
    with_writer(args, |out| out.extend_from_slice(text.as_bytes()));
    Ok(Value::Unit)
}

fn builtin_serde_wr_bool(args: &[Value]) -> RuntimeResult<Value> {
    let value = matches!(args.get(1), Some(Value::Bool(true)));
    with_writer(args, |out| {
        out.extend_from_slice(if value { b"true" } else { b"false" });
    });
    Ok(Value::Unit)
}

fn builtin_serde_wr_str(args: &[Value]) -> RuntimeResult<Value> {
    let text = serde_text(args, 1).to_owned();
    with_writer(args, |out| {
        out.push(b'"');
        gossamer_runtime::c_abi::json_escape::json_escape_into(text.as_bytes(), out);
        out.push(b'"');
    });
    Ok(Value::Unit)
}

fn builtin_serde_wr_json(args: &[Value]) -> RuntimeResult<Value> {
    let rendered = match args.get(1) {
        Some(value) => match builtin_json_render(std::slice::from_ref(value))? {
            Value::String(s) => s.as_str().to_owned(),
            _ => String::from("null"),
        },
        None => String::from("null"),
    };
    with_writer(args, |out| out.extend_from_slice(rendered.as_bytes()));
    Ok(Value::Unit)
}

/// A builtin's entry point.
type SerdeBuiltin = fn(&[Value]) -> RuntimeResult<Value>;

/// The `__gos_serde_*` builtins, by the names the serde pass calls.
const SERDE_BUILTINS: &[(&str, SerdeBuiltin)] = &[
    ("__gos_serde_rd_array", builtin_serde_rd_array),
    ("__gos_serde_rd_at", builtin_serde_rd_at),
    ("__gos_serde_rd_bool", builtin_serde_rd_bool),
    ("__gos_serde_rd_elem", builtin_serde_rd_elem),
    ("__gos_serde_rd_enum", builtin_serde_rd_enum),
    ("__gos_serde_rd_error", builtin_serde_rd_error),
    ("__gos_serde_rd_f64", builtin_serde_rd_f64),
    ("__gos_serde_rd_fail", builtin_serde_rd_fail),
    ("__gos_serde_rd_failed", builtin_serde_rd_failed),
    ("__gos_serde_rd_field", builtin_serde_rd_field),
    ("__gos_serde_rd_free", builtin_serde_rd_free),
    ("__gos_serde_rd_i64", builtin_serde_rd_i64),
    ("__gos_serde_rd_is_null", builtin_serde_rd_is_null),
    ("__gos_serde_rd_json", builtin_serde_rd_json),
    ("__gos_serde_rd_leave", builtin_serde_rd_leave),
    ("__gos_serde_rd_map", builtin_serde_rd_map),
    ("__gos_serde_rd_map_key", builtin_serde_rd_map_key),
    ("__gos_serde_rd_map_value", builtin_serde_rd_map_value),
    ("__gos_serde_rd_mark", builtin_serde_rd_mark),
    ("__gos_serde_rd_new", builtin_serde_rd_new),
    ("__gos_serde_rd_str", builtin_serde_rd_str),
    ("__gos_serde_rd_struct", builtin_serde_rd_struct),
    ("__gos_serde_rd_tuple", builtin_serde_rd_tuple),
    ("__gos_serde_rd_u64", builtin_serde_rd_u64),
    ("__gos_serde_rd_wrap", builtin_serde_rd_wrap),
    ("__gos_serde_wr_bool", builtin_serde_wr_bool),
    ("__gos_serde_wr_f32", builtin_serde_wr_f32),
    ("__gos_serde_wr_f64", builtin_serde_wr_f64),
    ("__gos_serde_wr_finish", builtin_serde_wr_finish),
    ("__gos_serde_wr_i64", builtin_serde_wr_i64),
    ("__gos_serde_wr_json", builtin_serde_wr_json),
    ("__gos_serde_wr_new", builtin_serde_wr_new),
    ("__gos_serde_wr_raw", builtin_serde_wr_raw),
    ("__gos_serde_wr_sep", builtin_serde_wr_sep),
    ("__gos_serde_wr_str", builtin_serde_wr_str),
    ("__gos_serde_wr_u64", builtin_serde_wr_u64),
];
