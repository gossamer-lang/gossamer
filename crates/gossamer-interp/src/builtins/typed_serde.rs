// The VM's half of typed serde: the `__gos_serde_*` steps the decode and
// encode walks that `gossamer-hir`'s serde pass generates call. The reader is
// `gossamer_core`'s, as on the compiled tiers, and the writer appends the
// bytes the runtime's writer appends, so every tier decodes and encodes alike.

/// A typed reader over a VM string it holds a share of.
type Reader = json_std::typed::TypedReader<SmolStr>;

/// Open readers and writers, by handle. A goroutine may resume on another
/// worker thread between two steps, so the tables are process-wide.
struct SerdeHandles {
    readers: Vec<Option<Reader>>,
    writers: Vec<Option<gossamer_runtime::c_abi::typed_serde::SerdeWriter>>,
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
    f: impl FnOnce(&mut Reader) -> T,
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
    with_writer_state(args, |writer| f(&mut writer.bytes));
}

/// Runs `f` on the writer state `args[0]` names.
fn with_writer_state(
    args: &[Value],
    f: impl FnOnce(&mut gossamer_runtime::c_abi::typed_serde::SerdeWriter),
) {
    let Some(slot) = serde_slot(args) else {
        return;
    };
    let mut handles = SERDE_HANDLES.lock();
    if let Some(writer) = handles.writers.get_mut(slot).and_then(Option::as_mut) {
        f(writer);
    }
}

fn serde_handle(slot: usize) -> Value {
    Value::Int(i64::try_from(slot + 1).unwrap_or(0))
}

fn builtin_serde_rd_new(args: &[Value]) -> RuntimeResult<Value> {
    // The reader keeps a share of the program's string rather than a copy.
    let text = match args.first() {
        Some(Value::String(text)) => text.clone(),
        _ => SmolStr::from(serde_text(args, 0)),
    };
    open_reader(Reader::over(text))
}

/// `json::from_value::<T>`'s reader: over the text of the value, scalars and
/// handles alike.
fn builtin_serde_rd_new_value(args: &[Value]) -> RuntimeResult<Value> {
    let text = match args.first() {
        Some(Value::Json(value)) => json_std::encode(&value.as_value()),
        Some(other) => json_std::encode(&crate::builtins::gossamer_to_json_value(other)),
        None => "null".to_owned(),
    };
    open_reader(Reader::over(SmolStr::from(text)))
}

/// A reader over a document converted to JSON text, or one that has failed
/// with the conversion's message.
fn open_converted(converted: Result<String, String>) -> RuntimeResult<Value> {
    open_reader(match converted {
        Ok(json) => Reader::over(SmolStr::from(json)),
        Err(message) => Reader::failing(SmolStr::default(), &message),
    })
}

/// The `json::DuplicateKeys` policy a step's argument `index` names.
fn duplicates_arg(args: &[Value], index: usize) -> json_std::DuplicateKeys {
    match args.get(index) {
        Some(Value::Int(n)) => json_std::DuplicateKeys::from_index(*n),
        _ => json_std::DuplicateKeys::Error,
    }
}

/// `yaml::decode::<T>`'s reader, reading a repeated mapping key as its
/// second argument's policy says.
fn builtin_serde_rd_new_yaml(args: &[Value]) -> RuntimeResult<Value> {
    open_converted(gossamer_std::encoding::yaml::to_json_text_with(
        serde_text(args, 0),
        duplicates_arg(args, 1),
    ))
}

fn builtin_serde_rd_duplicates(args: &[Value]) -> RuntimeResult<Value> {
    let policy = duplicates_arg(args, 1);
    with_reader(args, (), |r| r.set_duplicate_keys(policy));
    Ok(Value::Unit)
}

/// `toml::decode::<T>`'s reader.
fn builtin_serde_rd_new_toml(args: &[Value]) -> RuntimeResult<Value> {
    open_converted(gossamer_std::encoding_toml::to_json_text(serde_text(args, 0)))
}

fn open_reader(reader: Reader) -> RuntimeResult<Value> {
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
    Ok(Value::Bool(with_reader(args, false, Reader::finish)))
}

fn builtin_serde_rd_error(args: &[Value]) -> RuntimeResult<Value> {
    let error = with_reader(args, None, |r| r.error().cloned());
    let text = error.map(|e| e.to_string()).unwrap_or_default();
    Ok(make_error_value(&text))
}

fn builtin_serde_rd_fail(args: &[Value]) -> RuntimeResult<Value> {
    let message = serde_text(args, 1).to_owned();
    with_reader(args, (), |r| r.fail(&message));
    Ok(Value::Unit)
}

fn builtin_serde_rd_int(args: &[Value]) -> RuntimeResult<Value> {
    let expected = serde_text(args, 1).to_owned();
    let bound = |index: usize| match args.get(index) {
        Some(Value::Int(n)) => *n,
        _ => 0,
    };
    let (min, max) = (bound(2), bound(3));
    Ok(Value::Int(with_reader(args, 0, |r| {
        r.read_int(&expected, min, max)
    })))
}

fn builtin_serde_rd_leave(args: &[Value]) -> RuntimeResult<Value> {
    with_reader(args, (), Reader::leave);
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

fn builtin_serde_rd_next_field(args: &[Value]) -> RuntimeResult<Value> {
    let index = with_reader(args, -1, |r| {
        if r.next_field() {
            i64::try_from(r.field_index()).unwrap_or(-1)
        } else {
            -1
        }
    });
    Ok(Value::Int(index))
}

fn builtin_serde_rd_i64(args: &[Value]) -> RuntimeResult<Value> {
    let message = serde_text(args, 1).to_owned();
    Ok(Value::Int(with_reader(args, 0, |r| r.read_i64(&message))))
}

fn builtin_serde_rd_i128(args: &[Value]) -> RuntimeResult<Value> {
    let message = serde_text(args, 1).to_owned();
    let bits = with_reader(args, 0, |r| r.read_wide(&message, false));
    Ok(Value::from_i128(bits.cast_signed()))
}

fn builtin_serde_rd_u128(args: &[Value]) -> RuntimeResult<Value> {
    let message = serde_text(args, 1).to_owned();
    Ok(Value::from_u128(with_reader(args, 0, |r| r.read_wide(&message, true))))
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

fn builtin_serde_rd_char(args: &[Value]) -> RuntimeResult<Value> {
    let message = serde_text(args, 1).to_owned();
    Ok(Value::Char(with_reader(args, '\0', |r| r.read_char(&message))))
}

fn builtin_serde_rd_str(args: &[Value]) -> RuntimeResult<Value> {
    let message = serde_text(args, 1).to_owned();
    let text = with_reader(args, SmolStr::default(), |r| SmolStr::from(&*r.read_str(&message)));
    Ok(Value::String(text))
}

fn builtin_serde_rd_json(args: &[Value]) -> RuntimeResult<Value> {
    let raw = with_reader(args, String::from("null"), |r| r.raw_value().to_owned());
    let value = json_std::parse(&raw).unwrap_or(json_std::Value::Null);
    Ok(Value::Json(Arc::new(JsonInner::new(value))))
}

fn builtin_serde_rd_array(args: &[Value]) -> RuntimeResult<Value> {
    let message = serde_text(args, 1).to_owned();
    with_reader(args, (), |r| r.begin_array(&message));
    Ok(Value::Unit)
}

fn builtin_serde_rd_next_elem(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Bool(with_reader(args, false, Reader::next_elem)))
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
    with_reader(args, (), |r| r.begin_map(&message));
    Ok(Value::Unit)
}

fn builtin_serde_rd_next_entry(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Bool(with_reader(args, false, Reader::next_entry)))
}

fn builtin_serde_rd_entry_key(args: &[Value]) -> RuntimeResult<Value> {
    let key = with_reader(args, String::new(), |r| r.entry_key().to_owned());
    Ok(Value::String(SmolStr::from(key)))
}

fn builtin_serde_rd_enum(args: &[Value]) -> RuntimeResult<Value> {
    let variants = serde_text(args, 1).to_owned();
    let name = serde_text(args, 2).to_owned();
    let fallback = serde_index(args, 3);
    let chosen = with_reader(args, fallback, |r| r.begin_enum(&variants, &name, fallback));
    Ok(Value::Int(i64::try_from(chosen).unwrap_or(0)))
}

fn builtin_serde_rd_tagged(args: &[Value]) -> RuntimeResult<Value> {
    let variants = serde_text(args, 1).to_owned();
    let tag = serde_text(args, 2).to_owned();
    let name = serde_text(args, 3).to_owned();
    let fallback = serde_index(args, 4);
    let chosen = with_reader(args, fallback, |r| {
        r.begin_tagged(&variants, &tag, &name, fallback)
    });
    Ok(Value::Int(i64::try_from(chosen).unwrap_or(0)))
}

fn builtin_serde_wr_new(_args: &[Value]) -> RuntimeResult<Value> {
    let mut handles = SERDE_HANDLES.lock();
    let slot = if let Some(free) = handles.writers.iter().position(Option::is_none) {
        handles.writers[free] = Some(gossamer_runtime::c_abi::typed_serde::SerdeWriter {
            bytes: Vec::with_capacity(gossamer_runtime::c_abi::typed_serde::SERDE_WRITER_CAPACITY),
            non_finite: None,
        });
        free
    } else {
        handles.writers.push(Some(gossamer_runtime::c_abi::typed_serde::SerdeWriter {
            bytes: Vec::with_capacity(gossamer_runtime::c_abi::typed_serde::SERDE_WRITER_CAPACITY),
            non_finite: None,
        }));
        handles.writers.len() - 1
    };
    Ok(serde_handle(slot))
}

fn builtin_serde_wr_finish(args: &[Value]) -> RuntimeResult<Value> {
    let bytes = serde_slot(args)
        .and_then(|slot| SERDE_HANDLES.lock().writers.get_mut(slot).and_then(Option::take))
        .map(|writer| writer.bytes)
        .unwrap_or_default();
    Ok(Value::String(SmolStr::from(
        String::from_utf8_lossy(&bytes).into_owned(),
    )))
}

/// `json::encode`'s finish: the text, or the error for a NaN or an infinity
/// the walk met.
fn builtin_serde_wr_finish_json(args: &[Value]) -> RuntimeResult<Value> {
    finish_json(args, false)
}

/// `json::encode_pretty`'s finish.
fn builtin_serde_wr_finish_json_pretty(args: &[Value]) -> RuntimeResult<Value> {
    finish_json(args, true)
}

fn finish_json(args: &[Value], pretty: bool) -> RuntimeResult<Value> {
    let writer = serde_slot(args)
        .and_then(|slot| SERDE_HANDLES.lock().writers.get_mut(slot).and_then(Option::take))
        .unwrap_or_default();
    if let Some(value) = writer.non_finite {
        return Ok(crate::builtins::err_variant(
            gossamer_runtime::c_abi::typed_serde::non_finite_message(value),
        ));
    }
    let compact = String::from_utf8_lossy(&writer.bytes);
    let text = if pretty {
        json_std::typed::indent_text(&compact)
    } else {
        compact.into_owned()
    };
    Ok(converted_result(Ok(text)))
}

fn builtin_serde_wr_finish_pretty(args: &[Value]) -> RuntimeResult<Value> {
    let bytes = serde_slot(args)
        .and_then(|slot| SERDE_HANDLES.lock().writers.get_mut(slot).and_then(Option::take))
        .map(|writer| writer.bytes)
        .unwrap_or_default();
    let compact = String::from_utf8_lossy(&bytes);
    Ok(Value::String(SmolStr::from(
        json_std::typed::indent_text(&compact),
    )))
}

/// The text a writer holds, releasing the writer.
fn take_writer(args: &[Value]) -> String {
    let bytes = serde_slot(args)
        .and_then(|slot| SERDE_HANDLES.lock().writers.get_mut(slot).and_then(Option::take))
        .map(|writer| writer.bytes)
        .unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// A conversion's text as `Ok`, or its message as an `Err`.
fn converted_result(converted: Result<String, String>) -> Value {
    match converted {
        Ok(text) => crate::builtins::ok_variant(Value::String(SmolStr::from(text))),
        Err(message) => crate::builtins::err_variant(message),
    }
}

/// `to_yaml::<T>`'s finish: the walk's JSON text as YAML, members as written.
fn builtin_serde_wr_finish_yaml(args: &[Value]) -> RuntimeResult<Value> {
    let text = take_writer(args);
    Ok(converted_result(gossamer_runtime::codec::json_convert::to_yaml(
        &text,
        gossamer_runtime::codec::json_convert::Members::AsWritten,
    )))
}

/// `to_toml::<T>`'s finish: the walk's JSON text as TOML, members as written.
fn builtin_serde_wr_finish_toml(args: &[Value]) -> RuntimeResult<Value> {
    let text = take_writer(args);
    Ok(converted_result(gossamer_runtime::codec::json_convert::to_toml(
        &text,
        gossamer_runtime::codec::json_convert::Members::AsWritten,
    )))
}

fn builtin_serde_wr_raw(args: &[Value]) -> RuntimeResult<Value> {
    let text = serde_text(args, 1).to_owned();
    with_writer(args, |out| out.extend_from_slice(text.as_bytes()));
    Ok(Value::Unit)
}

fn builtin_serde_wr_key(args: &[Value]) -> RuntimeResult<Value> {
    let text = serde_text(args, 1).to_owned();
    with_writer(args, |out| json_std::typed::write_key(out, text.as_bytes()));
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

/// Appends a 128-bit integer: a number when it fits 64 bits, else a string.
fn write_wide(args: &[Value], unsigned: bool) -> RuntimeResult<Value> {
    let bits = args
        .get(1)
        .and_then(if unsigned { Value::as_u128 } else { |v: &Value| v.as_i128().map(i128::cast_unsigned) })
        .unwrap_or(0);
    let text = json_std::typed::wide_json_text(bits, unsigned);
    with_writer(args, |out| out.extend_from_slice(text.as_bytes()));
    Ok(Value::Unit)
}

fn builtin_serde_wr_i128(args: &[Value]) -> RuntimeResult<Value> {
    write_wide(args, false)
}

fn builtin_serde_wr_u128(args: &[Value]) -> RuntimeResult<Value> {
    write_wide(args, true)
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

fn builtin_serde_wr_f64_tree(args: &[Value]) -> RuntimeResult<Value> {
    let value = serde_float_arg(args);
    with_writer_state(args, |writer| {
        writer.note_float(value);
        gossamer_runtime::builtins::write_json_tree_float(&mut writer.bytes, value);
    });
    Ok(Value::Unit)
}

fn builtin_serde_wr_f32_tree(args: &[Value]) -> RuntimeResult<Value> {
    let value = gossamer_runtime::builtins::f32_as_decimal_double(serde_float_arg(args));
    with_writer_state(args, |writer| {
        writer.note_float(value);
        gossamer_runtime::builtins::write_json_tree_float(&mut writer.bytes, value);
    });
    Ok(Value::Unit)
}

fn builtin_serde_wr_bool(args: &[Value]) -> RuntimeResult<Value> {
    let value = matches!(args.get(1), Some(Value::Bool(true)));
    with_writer(args, |out| {
        out.extend_from_slice(if value { b"true" } else { b"false" });
    });
    Ok(Value::Unit)
}

fn builtin_serde_wr_char(args: &[Value]) -> RuntimeResult<Value> {
    let c = match args.get(1) {
        Some(Value::Char(c)) => *c,
        _ => '\u{FFFD}',
    };
    let mut buf = [0u8; 4];
    let text = c.encode_utf8(&mut buf);
    with_writer(args, |out| {
        out.push(b'"');
        gossamer_runtime::c_abi::json_escape::json_escape_into(text.as_bytes(), out);
        out.push(b'"');
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
    ("__gos_serde_rd_next_elem", builtin_serde_rd_next_elem),
    ("__gos_serde_rd_at", builtin_serde_rd_at),
    ("__gos_serde_rd_bool", builtin_serde_rd_bool),
    ("__gos_serde_rd_char", builtin_serde_rd_char),
    ("__gos_serde_rd_elem", builtin_serde_rd_elem),
    ("__gos_serde_rd_enum", builtin_serde_rd_enum),
    ("__gos_serde_rd_error", builtin_serde_rd_error),
    ("__gos_serde_rd_f64", builtin_serde_rd_f64),
    ("__gos_serde_rd_fail", builtin_serde_rd_fail),
    ("__gos_serde_rd_failed", builtin_serde_rd_failed),
    ("__gos_serde_rd_next_field", builtin_serde_rd_next_field),
    ("__gos_serde_rd_free", builtin_serde_rd_free),
    ("__gos_serde_rd_i128", builtin_serde_rd_i128),
    ("__gos_serde_rd_i64", builtin_serde_rd_i64),
    ("__gos_serde_rd_duplicates", builtin_serde_rd_duplicates),
    ("__gos_serde_rd_int", builtin_serde_rd_int),
    ("__gos_serde_rd_is_null", builtin_serde_rd_is_null),
    ("__gos_serde_rd_json", builtin_serde_rd_json),
    ("__gos_serde_rd_leave", builtin_serde_rd_leave),
    ("__gos_serde_rd_map", builtin_serde_rd_map),
    ("__gos_serde_rd_next_entry", builtin_serde_rd_next_entry),
    ("__gos_serde_rd_entry_key", builtin_serde_rd_entry_key),
    ("__gos_serde_rd_new", builtin_serde_rd_new),
    ("__gos_serde_rd_new_toml", builtin_serde_rd_new_toml),
    ("__gos_serde_rd_new_value", builtin_serde_rd_new_value),
    ("__gos_serde_rd_new_yaml", builtin_serde_rd_new_yaml),
    ("__gos_serde_rd_str", builtin_serde_rd_str),
    ("__gos_serde_rd_struct", builtin_serde_rd_struct),
    ("__gos_serde_rd_tagged", builtin_serde_rd_tagged),
    ("__gos_serde_rd_tuple", builtin_serde_rd_tuple),
    ("__gos_serde_rd_u128", builtin_serde_rd_u128),
    ("__gos_serde_rd_u64", builtin_serde_rd_u64),
    ("__gos_serde_wr_bool", builtin_serde_wr_bool),
    ("__gos_serde_wr_char", builtin_serde_wr_char),
    ("__gos_serde_wr_f32", builtin_serde_wr_f32),
    ("__gos_serde_wr_f32_tree", builtin_serde_wr_f32_tree),
    ("__gos_serde_wr_f64", builtin_serde_wr_f64),
    ("__gos_serde_wr_f64_tree", builtin_serde_wr_f64_tree),
    ("__gos_serde_wr_finish", builtin_serde_wr_finish),
    ("__gos_serde_wr_finish_json", builtin_serde_wr_finish_json),
    ("__gos_serde_wr_finish_json_pretty", builtin_serde_wr_finish_json_pretty),
    ("__gos_serde_wr_finish_pretty", builtin_serde_wr_finish_pretty),
    ("__gos_serde_wr_finish_toml", builtin_serde_wr_finish_toml),
    ("__gos_serde_wr_finish_yaml", builtin_serde_wr_finish_yaml),
    ("__gos_serde_wr_i128", builtin_serde_wr_i128),
    ("__gos_serde_wr_i64", builtin_serde_wr_i64),
    ("__gos_serde_wr_json", builtin_serde_wr_json),
    ("__gos_serde_wr_key", builtin_serde_wr_key),
    ("__gos_serde_wr_new", builtin_serde_wr_new),
    ("__gos_serde_wr_raw", builtin_serde_wr_raw),
    ("__gos_serde_wr_sep", builtin_serde_wr_sep),
    ("__gos_serde_wr_str", builtin_serde_wr_str),
    ("__gos_serde_wr_u128", builtin_serde_wr_u128),
    ("__gos_serde_wr_u64", builtin_serde_wr_u64),
];
