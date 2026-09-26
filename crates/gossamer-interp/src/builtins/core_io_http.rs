// ---- Stream builtins: io::{stdout, stderr, stdin} + methods ----
//
// An `io::Stream` value is a `Value::Struct` with a single `fd`
// field: 0 = stdin, 1 = stdout, 2 = stderr. The walker's method
// dispatch routes `stream.write_byte(b)` etc. to the handlers
// below based on either the bare method name or the
// `Stream::method` qualified key.

fn stream_of(fd: i64) -> Value {
    Value::struct_("Stream", vec![("fd", Value::Int(fd))])
}

fn stream_fd(value: &Value) -> i64 {
    match value {
        Value::MutCell(cell) => stream_fd(&cell.lock()),
        Value::Struct(inner) if inner.name == "Stream" => {
            for (f_name, f_val) in &inner.fields {
                if (*f_name) == "fd" {
                    if let Value::Int(n) = f_val {
                        return *n;
                    }
                }
            }
            1
        }
        _ => 1,
    }
}

fn math_arg(args: &[Value]) -> f64 {
    match args.first() {
        Some(Value::Float(x)) => *x,
        Some(Value::Uint(n)) => *n as f64,
        other => other.and_then(Value::as_i64).map_or(0.0, |n| n as f64),
    }
}

fn wrapping_int_arg(args: &[Value], index: usize) -> u64 {
    args.get(index).and_then(Value::as_u64).unwrap_or(0)
}

fn normalize_wrapping_int(value: u64, bits: u32, signed: bool) -> i64 {
    let masked = if bits == 64 {
        value
    } else {
        value & ((1_u64 << bits) - 1)
    };
    if signed && bits < 64 && masked & (1_u64 << (bits - 1)) != 0 {
        (masked | (!0_u64 << bits)) as i64
    } else {
        masked as i64
    }
}

macro_rules! wrapping_int_builtins {
    ($add:ident, $sub:ident, $mul:ident, $bits:expr, $signed:expr) => {
        fn $add(args: &[Value]) -> RuntimeResult<Value> {
            let value = wrapping_int_arg(args, 0).wrapping_add(wrapping_int_arg(args, 1));
            Ok(Value::Int(normalize_wrapping_int(value, $bits, $signed)))
        }

        fn $sub(args: &[Value]) -> RuntimeResult<Value> {
            let value = wrapping_int_arg(args, 0).wrapping_sub(wrapping_int_arg(args, 1));
            Ok(Value::Int(normalize_wrapping_int(value, $bits, $signed)))
        }

        fn $mul(args: &[Value]) -> RuntimeResult<Value> {
            let value = wrapping_int_arg(args, 0).wrapping_mul(wrapping_int_arg(args, 1));
            Ok(Value::Int(normalize_wrapping_int(value, $bits, $signed)))
        }
    };
}

wrapping_int_builtins!(
    builtin_i8_wrapping_add,
    builtin_i8_wrapping_sub,
    builtin_i8_wrapping_mul,
    8,
    true
);
wrapping_int_builtins!(
    builtin_i16_wrapping_add,
    builtin_i16_wrapping_sub,
    builtin_i16_wrapping_mul,
    16,
    true
);
wrapping_int_builtins!(
    builtin_i32_wrapping_add,
    builtin_i32_wrapping_sub,
    builtin_i32_wrapping_mul,
    32,
    true
);
wrapping_int_builtins!(
    builtin_i64_wrapping_add,
    builtin_i64_wrapping_sub,
    builtin_i64_wrapping_mul,
    64,
    true
);
wrapping_int_builtins!(
    builtin_u8_wrapping_add,
    builtin_u8_wrapping_sub,
    builtin_u8_wrapping_mul,
    8,
    false
);
wrapping_int_builtins!(
    builtin_u16_wrapping_add,
    builtin_u16_wrapping_sub,
    builtin_u16_wrapping_mul,
    16,
    false
);
wrapping_int_builtins!(
    builtin_u32_wrapping_add,
    builtin_u32_wrapping_sub,
    builtin_u32_wrapping_mul,
    32,
    false
);
wrapping_int_builtins!(
    builtin_u64_wrapping_add,
    builtin_u64_wrapping_sub,
    builtin_u64_wrapping_mul,
    64,
    false
);

/// `f64::to_bits(x) -> u64`: the value's IEEE-754 binary64 encoding.
fn builtin_f64_to_bits(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Int(math_arg(args).to_bits() as i64))
}

/// `f64::from_bits(b) -> f64`: the binary64 value `b` encodes.
fn builtin_f64_from_bits(args: &[Value]) -> RuntimeResult<Value> {
    let bits = args.first().and_then(Value::as_u64).unwrap_or(0);
    Ok(Value::Float(f64::from_bits(bits)))
}

/// `f32::to_bits(x) -> u32`: the binary32 encoding of `x` rounded to
/// single precision, since every float occupies a 64-bit slot.
fn builtin_f32_to_bits(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Int(i64::from((math_arg(args) as f32).to_bits())))
}

/// `f32::from_bits(b) -> f32`: the binary32 value the low 32 bits of `b`
/// encode, widened to the 64-bit float slot.
fn builtin_f32_from_bits(args: &[Value]) -> RuntimeResult<Value> {
    let bits = args.first().and_then(Value::as_u64).unwrap_or(0) as u32;
    Ok(Value::Float(f64::from(f32::from_bits(bits))))
}

fn builtin_math_sqrt(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_arg(args).sqrt()))
}
fn builtin_math_sin(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_arg(args).sin()))
}
fn builtin_math_cos(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_arg(args).cos()))
}
fn builtin_math_exp(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_arg(args).exp()))
}
fn builtin_math_ln(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_arg(args).ln()))
}
fn builtin_math_abs(args: &[Value]) -> RuntimeResult<Value> {
    if let Some(Value::Int(n)) = args.first() {
        return Ok(Value::Int(n.saturating_abs()));
    }
    Ok(Value::Float(math_arg(args).abs()))
}
fn builtin_math_floor(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_arg(args).floor()))
}
fn builtin_math_ceil(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_arg(args).ceil()))
}
fn builtin_math_pow(args: &[Value]) -> RuntimeResult<Value> {
    let x = math_arg(args);
    let y = match args.get(1) {
        Some(Value::Float(v)) => *v,
        Some(Value::Int(n)) => *n as f64,
        Some(Value::Uint(n)) => *n as f64,
        _ => 0.0,
    };
    Ok(Value::Float(x.powf(y)))
}

fn builtin_io_stdout(_: &[Value]) -> RuntimeResult<Value> {
    Ok(stream_of(1))
}
fn builtin_io_stderr(_: &[Value]) -> RuntimeResult<Value> {
    Ok(stream_of(2))
}
fn builtin_io_stdin(_: &[Value]) -> RuntimeResult<Value> {
    Ok(stream_of(0))
}

fn builtin_stream_write_byte(args: &[Value]) -> RuntimeResult<Value> {
    let fd = args.first().map_or(1, stream_fd);
    let b = match args.get(1) {
        Some(Value::Int(n)) => *n,
        _ => return Err(RuntimeError::Type("write_byte: expected i64".to_string())),
    };
    stream_write_one_byte(fd, b);
    Ok(Value::Unit)
}

/// Writes a single byte to fd `fd` through the bytecode VM's
/// redirectable writer (`STDOUT_WRITER` / `STDERR_WRITER`),
/// matching the public `set_stdout_writer` contract used by
/// tests. Pulled out of `builtin_stream_write_byte` so the
/// `Op::StreamWriteByte` super-instruction can call it without
/// constructing a `Vec<Value>` first - the dominant per-byte cost
/// in `fasta`'s output loop.
pub(crate) fn stream_write_one_byte(fd: i64, byte: i64) {
    let bytes = [(byte & 0xff) as u8];
    let text = std::str::from_utf8(&bytes).unwrap_or("");
    if fd == 2 {
        write_stderr(text);
    } else {
        write_stdout(text);
    }
}

fn builtin_stream_write_str(args: &[Value]) -> RuntimeResult<Value> {
    let fd = args.first().map_or(1, stream_fd);
    let s = args.get(1).map(render_one).unwrap_or_default();
    if fd == 2 {
        write_stderr(&s);
    } else {
        write_stdout(&s);
    }
    Ok(Value::Unit)
}

fn builtin_stream_flush(_args: &[Value]) -> RuntimeResult<Value> {
    // The walker's writers are unbuffered (go straight to the
    // installed closures); flush is a no-op.
    Ok(Value::Unit)
}

fn builtin_stream_read_line(args: &[Value]) -> RuntimeResult<Value> {
    use std::io::BufRead;
    let fd = args.first().map_or(0, stream_fd);
    if args.len() == 1 {
        if fd != 0 {
            return Ok(none_variant());
        }
        let read = gossamer_runtime::sched_global::run_blocking("stdin-read-line", || {
            let stdin = std::io::stdin();
            let mut line = String::new();
            stdin.lock().read_line(&mut line).map(|n| (n, line))
        });
        return match read {
            Ok(Ok((0, _))) => Ok(none_variant()),
            Ok(Ok((_, mut line))) => {
                while line.ends_with('\n') || line.ends_with('\r') {
                    line.pop();
                }
                Ok(some_variant(Value::String(SmolStr::from(line))))
            }
            Ok(Err(_)) | Err(_) => Ok(none_variant()),
        };
    }
    let Some(Value::MutCell(cell)) = args.get(1) else {
        return Ok(err_variant(
            "read_line: expected &mut String buffer".to_string(),
        ));
    };
    if fd != 0 {
        return Ok(ok_variant(Value::Int(0)));
    }
    let read = gossamer_runtime::sched_global::run_blocking("stdin-read-line", || {
        let stdin = std::io::stdin();
        let mut line = String::new();
        stdin.lock().read_line(&mut line).map(|n| (n, line))
    });
    match read {
        Ok(Ok((n, line))) => {
            let mut guard = cell.lock();
            let Some(existing) = as_str(&guard) else {
                return Ok(err_variant(
                    "read_line: expected &mut String buffer".to_string(),
                ));
            };
            let mut out = existing.to_string();
            out.push_str(&line);
            *guard = Value::String(SmolStr::from(out));
            Ok(ok_variant(Value::Int(n as i64)))
        }
        Ok(Err(e)) => Ok(err_variant(format!("read_line: {e}"))),
        Err(e) => Ok(err_variant(e)),
    }
}

fn builtin_stream_read_to_string(args: &[Value]) -> RuntimeResult<Value> {
    use std::io::Read;
    let fd = args.first().map_or(0, stream_fd);
    if fd != 0 {
        return Ok(Value::String(SmolStr::from(String::new())));
    }
    let read = gossamer_runtime::sched_global::run_blocking("stdin-read-to-string", || {
        let stdin = std::io::stdin();
        let mut buf = String::new();
        stdin.lock().read_to_string(&mut buf).map(|_| buf)
    });
    match read {
        Ok(Ok(mut buf)) => {
            buf.shrink_to_fit();
            Ok(Value::String(buf.into()))
        }
        Ok(Err(_)) | Err(_) => Ok(Value::String(SmolStr::from(String::new()))),
    }
}

/// Drains a reader to a fallible String result. Today the only
/// Reader-shaped value the interp surfaces is the stdin Stream.
fn builtin_io_read_all(args: &[Value]) -> RuntimeResult<Value> {
    use std::io::Read;
    let fd = args.first().map_or(0, stream_fd);
    if fd != 0 {
        return Ok(ok_variant(Value::String(SmolStr::from(String::new()))));
    }
    let read = gossamer_runtime::sched_global::run_blocking("stdin-read-all", || {
        let stdin = std::io::stdin();
        let mut buf = String::new();
        stdin.lock().read_to_string(&mut buf).map(|_| buf)
    });
    match read {
        Ok(Ok(mut buf)) => {
            buf.shrink_to_fit();
            Ok(ok_variant(Value::String(buf.into())))
        }
        Ok(Err(e)) => Ok(err_variant(format!("io::ReadAll: {e}"))),
        Err(e) => Ok(err_variant(format!("io::ReadAll: {e}"))),
    }
}

/// `io::Copy(dst, src) -> i64` - drains `src` byte-by-byte into
/// `dst`, returning the byte count copied. Mirrors Go's `io.Copy`.
/// Works on the fd-shaped streams returned by `io::stdin` /
/// `io::stdout` / `io::stderr`: stdin → stdout/stderr is the only
/// pair supported today (other fds map to empty / no-op).
fn builtin_io_copy(args: &[Value]) -> RuntimeResult<Value> {
    use std::io::Read;
    let dst_fd = args.first().map_or(1, stream_fd);
    let src_fd = args.get(1).map_or(0, stream_fd);
    if src_fd != 0 {
        return Ok(Value::Int(0));
    }
    let read = gossamer_runtime::sched_global::run_blocking("stdin-copy", || {
        let stdin = std::io::stdin();
        let mut buf = String::new();
        stdin.lock().read_to_string(&mut buf).map(|n| (n, buf))
    });
    let (n, buf) = match read {
        Ok(Ok((n, mut buf))) => {
            buf.shrink_to_fit();
            (n as i64, buf)
        }
        Ok(Err(_)) | Err(_) => return Ok(Value::Int(0)),
    };
    if dst_fd == 2 {
        write_stderr(&buf);
    } else {
        write_stdout(&buf);
    }
    Ok(Value::Int(n))
}

fn builtin_eprintln(args: &[Value]) -> RuntimeResult<Value> {
    // One call for the line and its terminator, so concurrent writers
    // interleave whole lines rather than splicing one into another.
    let mut rendered = render_args(args);
    rendered.push('\n');
    write_stderr(&rendered);
    Ok(Value::Unit)
}

fn builtin_eprint(args: &[Value]) -> RuntimeResult<Value> {
    write_stderr(&render_args(args));
    Ok(Value::Unit)
}

fn builtin_format(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::String(SmolStr::from(render_args(args))))
}

/// Zero-separator concat. Used by compile-time macro expansion.
fn builtin_concat(args: &[Value]) -> RuntimeResult<Value> {
    let mut out = String::with_capacity(args.len() * 8);
    for arg in args {
        let _ = write!(out, "{arg}");
    }
    Ok(Value::String(out.into()))
}

/// `__debug(value)` - the `{:?}` rendering channel. It matches Display
/// everywhere the two agree; a bare float keeps a fractional part or an
/// exponent so the text reads back as a float, which is also how a float
/// nested in an aggregate renders.
fn builtin_debug(args: &[Value]) -> RuntimeResult<Value> {
    let mut out = String::with_capacity(args.len() * 8);
    for arg in args {
        if let Some(f) = crate::value::f32_render_slot(arg) {
            out.push_str(&gossamer_runtime::builtins::format_f32_debug(f));
            continue;
        }
        match arg {
            Value::Float(f) => out.push_str(&gossamer_runtime::builtins::format_float_debug(*f)),
            other => {
                let _ = write!(out, "{other}");
            }
        }
    }
    Ok(Value::String(out.into()))
}

/// `__fmt_prec(value, prec)` - format-string `{:.N}` lowering. Returns
/// a `String` containing `value` rendered with `prec` fractional
/// digits. Mirrors the runtime helper `gos_rt_f64_prec_to_str` so
/// interp output matches the compiled tiers bit-for-bit.
fn builtin_fmt_prec(args: &[Value]) -> RuntimeResult<Value> {
    let value = args.first().cloned().unwrap_or(Value::Int(0));
    let prec = args.get(1).and_then(value_to_int).unwrap_or(0);
    if prec < 0 {
        return Err(RuntimeError::Type(
            "__fmt_prec: precision must be non-negative".to_string(),
        ));
    }
    let prec = prec.min(64) as usize;
    let f = match value {
        Value::Float(f) => f,
        Value::Int(n) => n as f64,
        // Precision bounds how much of a value is shown, so on text it
        // takes the first `prec` scalars - the unit a string's length is
        // counted in everywhere else in the language.
        Value::String(text) => {
            let taken: String = text.as_str().chars().take(prec).collect();
            return Ok(Value::String(taken.into()));
        }
        other => {
            return Err(RuntimeError::Type(format!(
                "__fmt_prec expected a numeric or text first argument, got {other}"
            )));
        }
    };
    Ok(Value::String(format!("{f:.prec$}").into()))
}

/// `__fmt_radix(value, base)` - renders an integer in `base` (2..=36).
fn builtin_fmt_radix(args: &[Value]) -> RuntimeResult<Value> {
    let value = args.first().and_then(value_to_int).unwrap_or(0);
    let base = args.get(1).and_then(value_to_int).unwrap_or(10);
    let radix = u32::try_from(base).unwrap_or(10);
    let out = if !(2..=36).contains(&radix) || value == 0 {
        if value == 0 {
            "0".to_string()
        } else {
            value.to_string()
        }
    } else {
        let mut v = u128::from(value as u64);
        let r = u128::from(radix);
        let mut digits = Vec::new();
        while v > 0 {
            let d = (v % r) as u32;
            digits.push(std::char::from_digit(d, radix).unwrap_or('0'));
            v /= r;
        }
        digits.iter().rev().collect()
    };
    Ok(Value::String(out.into()))
}

/// `__fmt_upper(s)` - uppercases the rendered string (for `{:X}`).
fn builtin_fmt_upper(args: &[Value]) -> RuntimeResult<Value> {
    let s = args.first().and_then(as_str).unwrap_or("");
    Ok(Value::String(SmolStr::to_uppercase_from(s)))
}

/// `__fmt_pad(s, width, fill, align)` - pads a rendered string to `width`.
/// `align`: 0 = right (pad left), 1 = left (pad right), 2 = center,
/// 3 = zeros between the number's sign (and radix prefix) and its digits.
fn builtin_fmt_pad(args: &[Value]) -> RuntimeResult<Value> {
    let text = args.first().and_then(as_str).unwrap_or("");
    let width = args.get(1).and_then(value_to_int).unwrap_or(0);
    if width < 0 {
        return Err(RuntimeError::Type(
            "__fmt_pad: width must be non-negative".to_string(),
        ));
    }
    let width = usize::try_from(width)
        .map_err(|_| RuntimeError::Type("__fmt_pad: width is too large".to_string()))?;
    let fill = args
        .get(2)
        .and_then(value_to_int)
        .and_then(|c| u32::try_from(c).ok())
        .and_then(char::from_u32)
        .unwrap_or(' ');
    let align = args.get(3).and_then(value_to_int).unwrap_or(0);
    let count = text.chars().count();
    if count >= width {
        return Ok(Value::String(text.into()));
    }
    let total = width - count;
    if align == gossamer_ast::PAD_ALIGN_SIGN_AWARE_ZERO {
        let split = gossamer_ast::sign_aware_prefix_len(text);
        let mut out = String::with_capacity(text.len() + total);
        out.push_str(&text[..split]);
        out.extend(std::iter::repeat_n('0', total));
        out.push_str(&text[split..]);
        return Ok(Value::String(out.into()));
    }
    let (left, right) = match align {
        1 => (0, total),
        2 => (total / 2, total - total / 2),
        _ => (total, 0),
    };
    let mut out = String::with_capacity(text.len() + total);
    for _ in 0..left {
        out.push(fill);
    }
    out.push_str(text);
    for _ in 0..right {
        out.push(fill);
    }
    Ok(Value::String(out.into()))
}

fn builtin_panic(args: &[Value]) -> RuntimeResult<Value> {
    Err(RuntimeError::Panic(render_args(args)))
}

/// `assert(cond)` / `assert(cond, msg)` - prelude assertion. Panics on a
/// false condition (so a failing test is recorded as a failure); a
/// passing assertion is counted in the test tally. Mirrored on the
/// compiled tiers by lowering to a conditional `gos_rt_panic`.
fn builtin_assert(args: &[Value]) -> RuntimeResult<Value> {
    let cond = matches!(args.first(), Some(Value::Bool(true)));
    if cond {
        observe_assertion(true, "assert".to_string());
        return Ok(Value::Unit);
    }
    // Match the compiled tier (and Rust's `assert!`): a supplied message
    // is the panic text verbatim; the no-message form panics with
    // "assertion failed".
    let detail = match args.get(1).and_then(as_str) {
        Some(m) => m.to_string(),
        None => "assertion failed".to_string(),
    };
    Err(RuntimeError::Panic(detail))
}

/// `assert_eq(a, b)` / `assert_eq(a, b, msg)` - panics unless `a == b`.
fn builtin_assert_eq(args: &[Value]) -> RuntimeResult<Value> {
    let left = args.first().cloned().unwrap_or(Value::Unit);
    let right = args.get(1).cloned().unwrap_or(Value::Unit);
    if values_equal_for_assertion(&left, &right) {
        observe_assertion(true, "assert_eq".to_string());
        return Ok(Value::Unit);
    }
    let suffix = match args.get(2).and_then(as_str) {
        Some(m) => format!(": {m}"),
        None => String::new(),
    };
    Err(RuntimeError::Panic(format!(
        "assertion failed{suffix}: {} != {}",
        render_one(&left),
        render_one(&right)
    )))
}

fn builtin_http_response_text(args: &[Value]) -> RuntimeResult<Value> {
    // Method call: response.text() - receiver is a Response struct.
    if let Some(Value::Struct(inner)) = args.first() {
        if inner.name == "Response" {
            let body = inner
                .fields
                .iter()
                .find(|(ident, _)| (**ident) == "body")
                .and_then(|(_, v)| as_str(v))
                .unwrap_or_default();
            return Ok(ok_variant(Value::String(SmolStr::from(body.to_string()))));
        }
    }
    // Constructor: Response::text(status, body).
    let status = args.first().and_then(value_to_int).unwrap_or(200);
    let body = args.get(1).map(render_one).unwrap_or_default();
    Ok(response_struct(status, body, "text/plain; charset=utf-8"))
}

fn builtin_http_response_json(args: &[Value]) -> RuntimeResult<Value> {
    let status = args.first().and_then(value_to_int).unwrap_or(200);
    let body = args.get(1).map(render_one).unwrap_or_default();
    Ok(response_struct(status, body, "application/json"))
}

/// `Response::stream(status, content_type, rs) -> Response` - wraps a
/// live `ResponseStream` so the server drains it to the client as
/// chunked frames (proxy passthrough). Construction CONSUMES the
/// stream: the handle is moved out of the client registry, so a later
/// `next_chunk` / `next_line` on the same `ResponseStream` yields
/// `None`, and a second `Response::stream` over the same value serves
/// an empty chunked body. Mirrors the compiled tier's
/// `gos_rt_http_response_stream_new` exactly.
#[cfg(not(target_arch = "wasm32"))]
fn builtin_http_response_stream(args: &[Value]) -> RuntimeResult<Value> {
    let status = args.first().and_then(value_to_int).unwrap_or(200);
    let content_type = args.get(1).map(render_one).unwrap_or_default();
    let Some(handle) = args
        .get(2)
        .and_then(crate::http_client_builtins::response_stream_handle)
    else {
        return Err(RuntimeError::Type(
            "Response::stream expects a ResponseStream as its third argument".to_string(),
        ));
    };
    crate::http_client_builtins::stream_consume_for_response(handle);
    let fields = vec![
        ("status", Value::Int(status)),
        ("body", Value::String(SmolStr::default())),
        ("content_type", Value::String(SmolStr::from(content_type))),
        ("__stream_handle", Value::Int(handle)),
    ];
    Ok(Value::struct_("Response", fields))
}

/// `resp.with_header(name, value) -> Response` - chainable header
/// attach. Replace-then-push, mirroring the compiled tier's
/// `gos_rt_http_response_with_header`: any prior header with the
/// same case-insensitive name is dropped, then the new pair is
/// appended, so the last `with_header` for a name wins.
fn builtin_http_response_with_header(args: &[Value]) -> RuntimeResult<Value> {
    let Some(Value::Struct(inner)) = args.first() else {
        return Err(RuntimeError::Type(
            "Response::with_header expects a Response receiver".to_string(),
        ));
    };
    let name = args.get(1).map(render_one).unwrap_or_default();
    let value = args.get(2).map(render_one).unwrap_or_default();
    let pair = Value::Tuple(Arc::from(vec![
        Value::String(SmolStr::from(name.clone())),
        Value::String(SmolStr::from(value)),
    ]));
    let mut fields = inner.fields.to_vec();
    if let Some((_, slot)) = fields.iter_mut().find(|(ident, _)| (*ident) == "headers") {
        let mut items = match slot {
            Value::Array(existing) => existing.as_ref().clone(),
            _ => Vec::new(),
        };
        items.retain(|item| {
            !matches!(item, Value::Tuple(kv)
                if matches!(kv.first(), Some(Value::String(k))
                    if k.as_str().eq_ignore_ascii_case(&name)))
        });
        items.push(pair);
        *slot = Value::Array(Arc::new(items));
    } else {
        fields.push(("headers", Value::Array(Arc::new(vec![pair]))));
    }
    Ok(Value::struct_(inner.name.clone(), fields))
}

#[cfg(not(target_arch = "wasm32"))]
fn builtin_http2_config_default(_args: &[Value]) -> RuntimeResult<Value> {
    let c = gossamer_std::http_h2::Config::default();
    let fields = vec![
        (
            "max_concurrent_streams",
            Value::Int(i64::from(c.max_concurrent_streams)),
        ),
        (
            "initial_window_size",
            Value::Int(i64::from(c.initial_window_size)),
        ),
        (
            "initial_connection_window_size",
            Value::Int(i64::from(c.initial_connection_window_size)),
        ),
        ("max_frame_size", Value::Int(i64::from(c.max_frame_size))),
        (
            "max_header_list_size",
            Value::Int(i64::from(c.max_header_list_size)),
        ),
    ];
    Ok(Value::struct_(
        "Config",
        Arc::unwrap_or_clone(Arc::new(fields)),
    ))
}

fn response_struct(status: i64, body: String, content_type: &str) -> Value {
    let fields = vec![
        ("status", Value::Int(status)),
        ("body", Value::String(body.into())),
        (
            "content_type",
            Value::String(SmolStr::from(content_type.to_string())),
        ),
    ];
    Value::struct_("Response", Arc::unwrap_or_clone(Arc::new(fields)))
}

pub(crate) fn value_to_int(value: &Value) -> Option<i64> {
    match value {
        Value::Int(n) => Some(*n),
        // `as u64` / `as usize` answer an unsigned shape carrying the same
        // 64 bits. Every caller wants those bits, and a `None` here reaches
        // an `unwrap_or(0)` that would silently substitute zero.
        Value::Uint(n) => Some(*n as i64),
        _ => None,
    }
}

fn render_one(value: &Value) -> String {
    match value {
        Value::String(s) => s.as_str().to_string(),
        other => format!("{other}"),
    }
}

fn render_args(args: &[Value]) -> String {
    let mut out = String::new();
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let _ = write!(out, "{arg}");
    }
    out
}

/// Turns one handler outcome into the response the wire gets, reporting a
/// fault through the `slog` record shape every tier's server path uses.
///
/// A handler that panics, answers `Err`, or answers something that is not
/// an `http::Response` is a server fault: the client gets a bare 500 and
/// the operator gets the message together with the request that provoked
/// it.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn handler_outcome_to_response(
    outcome: RuntimeResult<Value>,
    method: &str,
    path: &str,
) -> http_std::Response {
    let fault = match outcome {
        Ok(value) => match value_to_response(&value) {
            Some(response) => return response,
            // An `Err` handler reports its own error; anything else is a
            // handler that answered something that is not a response.
            None => match &value {
                Value::Variant(v) if v.name == "Err" => v
                    .fields
                    .first()
                    .map_or_else(|| "handler returned Err".to_string(), |e| format!("{e}")),
                _ => "handler did not return http::Response".to_string(),
            },
        },
        // A panic's bare text, so the record reads the same on every tier.
        Err(RuntimeError::Panic(message)) => message,
        Err(err) => format!("{err}"),
    };
    slog_record(
        gossamer_std::slog::Level::Error,
        "http: handler failed",
        &[
            ("method".to_string(), method.to_string()),
            ("path".to_string(), path.to_string()),
            ("status".to_string(), "500".to_string()),
            ("error".to_string(), fault),
        ],
    );
    http_std::Response::text(
        http_std::StatusCode::INTERNAL_SERVER_ERROR,
        "internal server error",
    )
}

/// `http::serve(addr: String, handler: Value) -> Result<(), Error>`.
///
/// Binds a TCP listener on `addr` and serves HTTP/1.1 traffic. Each
/// accepted connection is parsed into a [`Request`][http_std::Request]
/// shaped `Value::Struct`, then dispatched by calling the user's
/// `serve` method with `[handler, request_value]`. The returned
/// response value is serialised back to the wire.
///
/// Graceful shutdown is driven by the `GOSSAMER_HTTP_MAX_REQUESTS`
/// environment variable (stop after N requests) or by the process
/// receiving SIGINT.
#[cfg(not(target_arch = "wasm32"))]
fn native_http_serve(dispatch: &mut dyn NativeDispatch, args: &[Value]) -> RuntimeResult<Value> {
    if args.len() < 2 {
        return Err(RuntimeError::Arity {
            expected: 2,
            found: args.len(),
        });
    }
    let addr: String = match &args[0] {
        Value::String(s) => s.as_str().to_string(),
        other => {
            return Err(RuntimeError::Type(format!(
                "expected address string, got {other}"
            )));
        }
    };
    let handler = args[1].clone();

    let mut config = http_std::server::Config::default();
    let override_max = HTTP_MAX_REQUESTS_OVERRIDE.load(Ordering::SeqCst);
    if override_max > 0 {
        config.max_requests = Some(override_max);
    } else if let Ok(raw) = std::env::var("GOSSAMER_HTTP_MAX_REQUESTS") {
        if let Ok(n) = raw.parse::<u64>() {
            config.max_requests = Some(n);
            eprintln!(
                "http::serve: GOSSAMER_HTTP_MAX_REQUESTS={n} - server will exit after {n} request(s). Unset this env var to run forever."
            );
        }
    }
    let shutdown = Arc::clone(&config.shutdown);
    install_http_shutdown_handler(shutdown);

    let (target, leading) = crate::value::SpawnTarget::for_handler(&handler);
    let result = http_std::server::bind_and_run_dispatch(&addr, &config, |request, sink| {
        // Each request answers on its own goroutine, so the accept loop
        // takes the next one while this handler runs and N requests are
        // served N-ways concurrently.
        let method = request.method.as_str().to_string();
        let path = request.path.clone();
        let (context, context_id) =
            crate::stdlib_builtins::context::request_context(0, Some(request.context.clone()));
        let mut args = leading.clone();
        args.push(request_to_value_with_context(&request, context));
        dispatch.spawn_with_outcome(
            target.clone(),
            args,
            Box::new(move |outcome| {
                sink.send(handler_outcome_to_response(outcome, &method, &path));
                // The request is over: anything the handler started under
                // its context stops with it.
                crate::stdlib_builtins::context::cancel_request_context(context_id);
            }),
        );
    });

    match result {
        Ok(()) => Ok(Value::variant("Ok", vec![Value::Unit])),
        // `http::serve` is `Result<(), Error>` in Gossamer - a bind
        // failure is an `Err` value for the caller's match, not a
        // panic (native-tier parity).
        Err(err) => Ok(err_variant(format!("http::serve: {err}"))),
    }
}

/// `httptest::record(handler, method, path, body)
/// -> Result<Response, errors::Error>` - calls `handler` with a request
/// built in memory and answers its response.
///
/// No socket, no port, no accept loop: a handler is a function from a
/// request to a response, and a test that only wants to know what it
/// answers should not have to run a server to find out.
#[cfg(not(target_arch = "wasm32"))]
fn native_httptest_record(
    dispatch: &mut dyn NativeDispatch,
    args: &[Value],
) -> RuntimeResult<Value> {
    if args.len() < 4 {
        return Err(RuntimeError::Arity {
            expected: 4,
            found: args.len(),
        });
    }
    let text = |value: &Value| match value {
        Value::String(s) => s.as_str().to_string(),
        other => format!("{other}"),
    };
    let (path, query) = match text(&args[2]).split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (text(&args[2]), String::new()),
    };
    let request = http_std::Request {
        method: http_std::Method::parse(&text(&args[1])).unwrap_or(http_std::Method::Get),
        path,
        query,
        headers: http_std::Headers::new(),
        body: text(&args[3]).into_bytes(),
        context: gossamer_std::context::Context::background(),
        trailers: None,
        peer_addr: String::new(),
    };
    let (context, context_id) = crate::stdlib_builtins::context::request_context(0, None);
    let value = request_to_value_with_context(&request, context);
    let outcome = crate::value::dispatch_request(dispatch, &args[0], value);
    crate::stdlib_builtins::context::cancel_request_context(context_id);
    match outcome {
        Ok(answer) => Ok(answer),
        Err(err) => Ok(err_variant(format!("httptest::record: {err}"))),
    }
}

/// `httptest::server(status, body) -> String` starts a detached loopback
/// static responder. Keeping it callback-free lets the interpreter release
/// its mutable dispatcher before returning the URL, matching compiled tiers.
#[cfg(not(target_arch = "wasm32"))]
fn native_httptest_server(
    _dispatch: &mut dyn NativeDispatch,
    args: &[Value],
) -> RuntimeResult<Value> {
    if args.len() != 2 {
        return Err(RuntimeError::Arity {
            expected: 2,
            found: args.len(),
        });
    }
    let status = match args[0] {
        Value::Int(value) if (100..=599).contains(&value) => value,
        Value::Int(_) => 500,
        ref value => {
            return Err(RuntimeError::Type(format!(
                "expected HTTP status integer, got {value}"
            )));
        }
    };
    let body = match &args[1] {
        Value::String(body) => body.as_str().to_string(),
        value => {
            return Err(RuntimeError::Type(format!(
                "expected response body string, got {value}"
            )));
        }
    };
    let url = gossamer_runtime::c_abi::testing::httptest_server(status, &body)
        .map_err(|error| RuntimeError::Panic(format!("httptest::server: {error}")))?;
    Ok(Value::String(url.into()))
}

/// `http::serve_tls(addr, cert_pem, key_pem, handler) -> Result<(), Error>`.
///
/// TLS-terminating variant of [`native_http_serve`]: builds a rustls
/// server config from the PEM-encoded certificate chain and private
/// key, then dispatches each request through the same handler contract
/// after TLS termination - so HTTPS handlers behave identically to the
/// plaintext path on every tier.
#[cfg(not(target_arch = "wasm32"))]
fn native_http_serve_tls(
    dispatch: &mut dyn NativeDispatch,
    args: &[Value],
) -> RuntimeResult<Value> {
    if args.len() < 4 {
        return Err(RuntimeError::Arity {
            expected: 4,
            found: args.len(),
        });
    }
    let addr = match &args[0] {
        Value::String(s) => s.as_str().to_string(),
        other => {
            return Err(RuntimeError::Type(format!(
                "expected address string, got {other}"
            )));
        }
    };
    let cert_pem = match &args[1] {
        Value::String(s) => s.as_str().to_string(),
        other => {
            return Err(RuntimeError::Type(format!(
                "expected cert PEM string, got {other}"
            )));
        }
    };
    let key_pem = match &args[2] {
        Value::String(s) => s.as_str().to_string(),
        other => {
            return Err(RuntimeError::Type(format!(
                "expected key PEM string, got {other}"
            )));
        }
    };
    let handler = args[3].clone();

    let tls_config = match gossamer_std::tls::server_config(gossamer_std::tls::CertKey {
        cert_pem: cert_pem.into_bytes(),
        key_pem: key_pem.into_bytes(),
    }) {
        Ok(c) => c,
        Err(e) => return Ok(err_variant(format!("http::serve_tls: {e}"))),
    };

    let mut config = http_std::server::Config::default();
    let override_max = HTTP_MAX_REQUESTS_OVERRIDE.load(Ordering::SeqCst);
    if override_max > 0 {
        config.max_requests = Some(override_max);
    } else if let Ok(raw) = std::env::var("GOSSAMER_HTTP_MAX_REQUESTS") {
        if let Ok(n) = raw.parse::<u64>() {
            config.max_requests = Some(n);
        }
    }
    let shutdown = Arc::clone(&config.shutdown);
    install_http_shutdown_handler(shutdown);

    let (target, leading) = crate::value::SpawnTarget::for_handler(&handler);
    let result = http_std::server::bind_and_run_tls_dispatch(
        &addr,
        &tls_config,
        &config,
        |request, sink| {
            let method = request.method.as_str().to_string();
            let path = request.path.clone();
            let (context, context_id) = crate::stdlib_builtins::context::request_context(
                0,
                Some(request.context.clone()),
            );
            let mut args = leading.clone();
            args.push(request_to_value_with_context(&request, context));
            dispatch.spawn_with_outcome(
                target.clone(),
                args,
                Box::new(move |outcome| {
                    sink.send(handler_outcome_to_response(outcome, &method, &path));
                    crate::stdlib_builtins::context::cancel_request_context(context_id);
                }),
            );
        },
    );

    match result {
        Ok(()) => Ok(Value::variant("Ok", vec![Value::Unit])),
        Err(err) => Ok(err_variant(format!("http::serve_tls: {err}"))),
    }
}

/// `http2::bind_and_run_h2c(addr: String, handler) -> Result<(), Error>`.
///
/// Boots an HTTP/2 cleartext server. The handler is a struct
/// with a `serve(request) -> Response` method (or any callable
/// value with that shape). Connections run in goroutines; each
/// request is dispatched back to the main thread via a channel
/// so the interpreter's `NativeDispatch` (which is not Send) can
/// invoke the handler on the calling thread.
#[allow(
    clippy::too_many_lines,
    clippy::items_after_statements,
    clippy::needless_continue
)]
#[cfg(not(target_arch = "wasm32"))]
fn native_http2_bind_and_run_h2c(
    dispatch: &mut dyn NativeDispatch,
    args: &[Value],
) -> RuntimeResult<Value> {
    if args.len() < 2 {
        return Err(RuntimeError::Arity {
            expected: 2,
            found: args.len(),
        });
    }
    let addr: String = match &args[0] {
        Value::String(s) => s.as_str().to_string(),
        other => {
            return Err(RuntimeError::Type(format!(
                "expected address string, got {other}"
            )));
        }
    };
    let handler = args[1].clone();

    use std::sync::mpsc;
    let (req_tx, req_rx) = mpsc::channel::<(http_std::Request, mpsc::Sender<http_std::Response>)>();

    let shutdown = Arc::new(AtomicBool::new(false));
    install_http_shutdown_handler(Arc::clone(&shutdown));

    let max_requests = HTTP_MAX_REQUESTS_OVERRIDE.load(Ordering::SeqCst);

    // Bind synchronously so a bind failure is the caller's `Err`
    // value (native-tier parity), then hand the listener to the
    // accept thread.
    let listener = match gossamer_runtime::listen::bind_tcp(&addr) {
        Ok(l) => l,
        Err(e) => return Ok(err_variant(format!("http::serve_h2c: {e}"))),
    };

    let shutdown_for_server = Arc::clone(&shutdown);
    let req_tx_for_server = req_tx.clone();
    std::thread::Builder::new()
        .name("gossamer-http2-accept".to_string())
        .spawn(move || {
            let _ = gossamer_std::http_h2::run_h2c(
                listener,
                move |req: http_std::Request| -> http_std::Response {
                    if shutdown_for_server.load(Ordering::Acquire) {
                        return http_std::Response {
                            status: http_std::StatusCode(503),
                            headers: http_std::Headers::new(),
                            body: b"shutting down".to_vec(),
                            raw_header_pairs: Vec::new(),
                            body_stream: None,
                        };
                    }
                    let (resp_tx, resp_rx) = mpsc::channel();
                    if req_tx_for_server.send((req, resp_tx)).is_err() {
                        return http_std::Response {
                            status: http_std::StatusCode(500),
                            headers: http_std::Headers::new(),
                            body: b"dispatch channel closed".to_vec(),
                            raw_header_pairs: Vec::new(),
                            body_stream: None,
                        };
                    }
                    match resp_rx.recv_timeout(Duration::from_secs(30)) {
                        Ok(r) => r,
                        Err(_) => http_std::Response {
                            status: http_std::StatusCode(504),
                            headers: http_std::Headers::new(),
                            body: b"handler timeout".to_vec(),
                            raw_header_pairs: Vec::new(),
                            body_stream: None,
                        },
                    }
                },
                gossamer_std::http_h2::Config::default(),
            );
        })
        .map_err(|e| RuntimeError::Panic(format!("http2::bind_and_run_h2c spawn: {e}")))?;
    drop(req_tx);

    let (target, leading) = crate::value::SpawnTarget::for_handler(&handler);
    let mut served: u64 = 0;
    loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        match req_rx.recv_timeout(Duration::from_millis(50)) {
            Ok((req, resp_tx)) => {
                let method = req.method.as_str().to_string();
                let path = req.path.clone();
                let (context, context_id) = crate::stdlib_builtins::context::request_context(
                    0,
                    Some(req.context.clone()),
                );
                let mut args = leading.clone();
                args.push(request_to_value_with_context(&req, context));
                dispatch.spawn_with_outcome(
                    target.clone(),
                    args,
                    Box::new(move |outcome| {
                        let _ =
                            resp_tx.send(handler_outcome_to_response(outcome, &method, &path));
                        crate::stdlib_builtins::context::cancel_request_context(context_id);
                    }),
                );
                served = served.saturating_add(1);
                if max_requests > 0 && served >= max_requests {
                    shutdown.store(true, Ordering::Release);
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    Ok(Value::variant("Ok", vec![Value::Unit]))
}

/// `http_h3::serve(addr, cert_path, key_path, handler) ->
/// Result<(), Error>`.
///
/// Boots a QUIC + HTTP/3 server through the shared
/// [`gossamer_std::http_h3`] adapter. The engine drives its handler
/// closure on a private tokio runtime thread, so - exactly like the
/// h2 builtin - each request is marshalled over an mpsc channel back
/// to the interpreter thread, where the not-`Send` `NativeDispatch`
/// invokes the user handler.
#[allow(
    clippy::too_many_lines,
    clippy::items_after_statements,
    clippy::needless_continue
)]
#[cfg(not(target_arch = "wasm32"))]
fn native_http3_serve(dispatch: &mut dyn NativeDispatch, args: &[Value]) -> RuntimeResult<Value> {
    if args.len() < 4 {
        return Err(RuntimeError::Arity {
            expected: 4,
            found: args.len(),
        });
    }
    let str_arg = |v: &Value, what: &str| -> RuntimeResult<String> {
        match v {
            Value::String(s) => Ok(s.as_str().to_string()),
            other => Err(RuntimeError::Type(format!(
                "expected {what} string, got {other}"
            ))),
        }
    };
    let addr = str_arg(&args[0], "address")?;
    let cert_path = str_arg(&args[1], "cert path")?;
    let key_path = str_arg(&args[2], "key path")?;
    let handler = args[3].clone();

    let (target, leading) = crate::value::SpawnTarget::for_handler(&handler);

    use std::sync::mpsc;
    let (req_tx, req_rx) = mpsc::channel::<(http_std::Request, mpsc::Sender<http_std::Response>)>();

    let shutdown = Arc::new(AtomicBool::new(false));
    install_http_shutdown_handler(Arc::clone(&shutdown));
    let max_requests = HTTP_MAX_REQUESTS_OVERRIDE.load(Ordering::SeqCst);

    let shutdown_for_server = Arc::clone(&shutdown);
    let req_tx_for_server = req_tx.clone();
    let bind_addr = addr.clone();
    let (boot_tx, boot_rx) = mpsc::channel::<Result<(), String>>();
    std::thread::Builder::new()
        .name("gossamer-http3-accept".to_string())
        .spawn(move || {
            let handler_fn = move |req: http_std::Request| -> http_std::Response {
                if shutdown_for_server.load(Ordering::Acquire) {
                    return http_std::Response {
                        status: http_std::StatusCode(503),
                        headers: http_std::Headers::new(),
                        body: b"shutting down".to_vec(),
                        raw_header_pairs: Vec::new(),
                        body_stream: None,
                    };
                }
                let (resp_tx, resp_rx) = mpsc::channel();
                if req_tx_for_server.send((req, resp_tx)).is_err() {
                    return http_std::Response {
                        status: http_std::StatusCode(500),
                        headers: http_std::Headers::new(),
                        body: b"dispatch channel closed".to_vec(),
                        raw_header_pairs: Vec::new(),
                        body_stream: None,
                    };
                }
                match resp_rx.recv_timeout(Duration::from_secs(30)) {
                    Ok(r) => r,
                    Err(_) => http_std::Response {
                        status: http_std::StatusCode(504),
                        headers: http_std::Headers::new(),
                        body: b"handler timeout".to_vec(),
                        raw_header_pairs: Vec::new(),
                        body_stream: None,
                    },
                }
            };
            // `gossamer_std::http_h3::serve` validates the address,
            // reads the keypair, and binds before its accept loop
            // runs - so a synchronous `Err` is a startup failure the
            // caller must see. Forward the bind outcome over `boot`
            // and, on success, block here driving the endpoint.
            match gossamer_std::http_h3::serve(&bind_addr, &cert_path, &key_path, handler_fn) {
                Ok(()) => {
                    let _ = boot_tx.send(Ok(()));
                }
                Err(e) => {
                    let _ = boot_tx.send(Err(e.to_string()));
                }
            }
        })
        .map_err(|e| RuntimeError::Panic(format!("http_h3::serve spawn: {e}")))?;
    drop(req_tx);

    let mut served: u64 = 0;
    loop {
        // A startup failure (bad address, unreadable cert/key, bind
        // error) arrives on `boot` before any request - surface it as
        // the caller's `Err`, matching the native tier.
        if let Ok(Err(msg)) = boot_rx.try_recv() {
            return Ok(err_variant(format!("http_h3::serve: {msg}")));
        }
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        match req_rx.recv_timeout(Duration::from_millis(50)) {
            Ok((req, resp_tx)) => {
                let method = req.method.as_str().to_string();
                let path = req.path.clone();
                let (context, context_id) = crate::stdlib_builtins::context::request_context(
                    0,
                    Some(req.context.clone()),
                );
                let mut args = leading.clone();
                args.push(request_to_value_with_context(&req, context));
                dispatch.spawn_with_outcome(
                    target.clone(),
                    args,
                    Box::new(move |outcome| {
                        let _ =
                            resp_tx.send(handler_outcome_to_response(outcome, &method, &path));
                        crate::stdlib_builtins::context::cancel_request_context(context_id);
                    }),
                );
                served = served.saturating_add(1);
                if max_requests > 0 && served >= max_requests {
                    shutdown.store(true, Ordering::Release);
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // The server thread ended before any request - the only
                // pre-shutdown exit is a startup failure (bad address,
                // unreadable cert/key, bind error). Its outcome is now
                // definitively on `boot`; surface an `Err` so the caller
                // sees the same failure value the native tier returns.
                if let Ok(Err(msg)) = boot_rx.recv() {
                    return Ok(err_variant(format!("http_h3::serve: {msg}")));
                }
                break;
            }
        }
    }

    Ok(Value::variant("Ok", vec![Value::Unit]))
}

/// [`request_to_value`] with the request-scoped context a handler reads
/// off `request.context`.
pub(crate) fn request_to_value_with_context(
    request: &http_std::Request,
    context: Value,
) -> Value {
    let Value::Struct(inner) = request_to_value(request) else {
        return request_to_value(request);
    };
    let mut fields: Vec<(&'static str, Value)> = inner
        .fields
        .iter()
        .map(|(name, value)| (*name, value.clone()))
        .collect();
    fields.push(("context", context));
    Value::struct_("Request", fields)
}

pub(crate) fn request_to_value(request: &http_std::Request) -> Value {
    // Path and query are split at the ABI level since 0.4.
    let bare_path = request.path.clone();
    let query_string = request.query.clone();
    let headers: Vec<Value> = request
        .headers
        .iter()
        .map(|(name, value)| {
            Value::Tuple(Arc::from(vec![
                Value::String(SmolStr::from(name.to_string())),
                Value::String(SmolStr::from(value.to_string())),
            ]))
        })
        .collect();
    // Through the runtime's own parser, which is what the compiled
    // tiers read this field with: splitting here without decoding
    // answered `hello+world` where they answer `hello world`.
    let query_pairs: Vec<Value> = gossamer_runtime::c_abi::http_query::parse_query_pairs(
        query_string.as_str(),
    )
    .into_iter()
    .map(|(name, value)| {
        Value::Tuple(Arc::from(vec![
            Value::String(SmolStr::from(name)),
            Value::String(SmolStr::from(value)),
        ]))
    })
    .collect();
    let body_text = String::from_utf8_lossy(&request.body).into_owned();
    // Binary-safe sibling of the UTF-8-lossy `body` field - one
    // `Value::Int` per byte, matching the `resp.raw_bytes` and
    // `fs::read` byte-array shape.
    let raw_body: Vec<Value> = request
        .body
        .iter()
        .map(|b| Value::Int(i64::from(*b)))
        .collect();
    let fields = vec![
        (
            "method",
            Value::String(SmolStr::from(request.method.as_str().to_string())),
        ),
        ("path", Value::String(bare_path.into())),
        ("query", Value::String(query_string.into())),
        ("query_pairs", Value::Array(Arc::new(query_pairs))),
        ("headers", Value::Array(Arc::new(headers))),
        ("body", Value::String(body_text.into())),
        ("raw_body", Value::Array(Arc::new(raw_body))),
        (
            "peer_addr",
            Value::String(SmolStr::from(request.peer_addr.as_str())),
        ),
    ];
    Value::struct_("Request", Arc::unwrap_or_clone(Arc::new(fields)))
}

#[cfg(not(target_arch = "wasm32"))]
fn value_to_response(value: &Value) -> Option<http_std::Response> {
    let unwrapped = unwrap_result(value);
    let Value::Struct(struct_inner) = unwrapped else {
        return None;
    };
    let fields = &struct_inner.fields;
    let mut status: u16 = 200;
    let mut body: Vec<u8> = Vec::new();
    let mut content_type = String::new();
    let mut header_pairs: Vec<(String, String)> = Vec::new();
    let mut stream_handle: Option<i64> = None;
    for (ident, v) in fields {
        match *ident {
            "__stream_handle" => {
                if let Value::Int(h) = v {
                    stream_handle = Some(*h);
                }
            }
            "status" => {
                status = match v {
                    Value::Int(n) => u16::try_from(*n).unwrap_or(500),
                    Value::Variant(var_inner) if !var_inner.fields.is_empty() => {
                        match &var_inner.fields[0] {
                            Value::Int(n) => u16::try_from(*n).unwrap_or(500),
                            _ => 200,
                        }
                    }
                    _ => 200,
                };
            }
            "body" => body = v.bytes_or_empty(),
            "content_type" => {
                if let Value::String(s) = v {
                    content_type.clear();
                    content_type.push_str(s.as_str());
                }
            }
            "headers" => {
                if let Value::Array(items) = v {
                    for item in items.iter() {
                        if let Value::Tuple(kv) = item
                            && let (Some(Value::String(k)), Some(Value::String(val))) =
                                (kv.first(), kv.get(1))
                        {
                            header_pairs.push((k.to_string(), val.to_string()));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    // A `__stream_handle` field marks a `Response::stream` value:
    // take the live stream out of the pending registry (one-shot -
    // a second serve of the same handle drains nothing and answers
    // an empty chunked body, matching the compiled tier).
    let body_stream = stream_handle.map(|h| {
        crate::http_client_builtins::stream_take_for_serve(h).map_or_else(
            || http_std::BodyStream(Box::new(std::io::empty())),
            |arc| http_std::BodyStream(Box::new(crate::http_client_builtins::StreamBody(arc))),
        )
    });
    let streamed = body_stream.is_some();
    let mut response = http_std::Response {
        status: http_std::StatusCode(status),
        headers: http_std::Headers::new(),
        body,
        body_stream,
        raw_header_pairs: Vec::new(),
    };
    // Handler-set headers go in first; `Headers::insert` keys by
    // lowercased name, so a later same-name pair replaces the
    // earlier one - matching the compiled tier's replace-then-push.
    let mut has_content_type = false;
    let mut has_content_length = false;
    for (name, value) in &header_pairs {
        if name.eq_ignore_ascii_case("content-type") {
            has_content_type = true;
        }
        if name.eq_ignore_ascii_case("content-length") {
            has_content_length = true;
        }
        response.headers.insert(name, value);
    }
    // Precedence (mirrors the compiled tier's
    // `extract_response_into`): explicit content-type header >
    // `content_type` field > text/plain default.
    if !has_content_type {
        if content_type.is_empty() {
            content_type.push_str("text/plain; charset=utf-8");
        }
        response.headers.insert("content-type", &content_type);
    }
    // Streamed responses are framed as Transfer-Encoding: chunked by
    // the server writer; a Content-Length would violate RFC 7230
    // §3.3.3, so it is only synthesized for buffered bodies.
    if !has_content_length && !streamed {
        response
            .headers
            .insert("content-length", &response.body.len().to_string());
    }
    Some(response)
}

fn unwrap_result(value: &Value) -> &Value {
    match value {
        Value::Variant(inner) if inner.name == "Ok" && !inner.fields.is_empty() => &inner.fields[0],
        other => other,
    }
}

fn install_http_shutdown_handler(shutdown: Arc<AtomicBool>) {
    // `lifecycle::shutdown()` from Gossamer code stops this server too:
    // the runtime's process lifecycle owns the one shutdown decision, and
    // this server's own flag follows it.
    gossamer_runtime::c_abi::lifecycle::register_shutdown_flag(&shutdown);
    register_http_shutdown(shutdown);
}

#[cfg(not(target_arch = "wasm32"))]
fn register_http_shutdown(shutdown: Arc<AtomicBool>) {
    use gossamer_std::signal::{self, sigs};
    use std::sync::OnceLock;

    static HTTP_SHUTDOWNS: OnceLock<parking_lot::Mutex<Vec<Arc<AtomicBool>>>> = OnceLock::new();
    static HTTP_SIGNAL_HOOKED: AtomicBool = AtomicBool::new(false);

    let registry = HTTP_SHUTDOWNS.get_or_init(|| parking_lot::Mutex::new(Vec::new()));
    registry.lock().push(shutdown);

    if HTTP_SIGNAL_HOOKED.swap(true, Ordering::SeqCst) {
        return;
    }

    let sigint = signal::on(sigs::SIGINT);
    let sigterm = signal::on(sigs::SIGTERM);
    std::thread::Builder::new()
        .name("gossamer-http-shutdown".to_string())
        .spawn(move || {
            loop {
                if sigint.wait_with_timeout(Duration::from_millis(50))
                    || sigterm.wait_with_timeout(Duration::ZERO)
                {
                    if let Some(registry) = HTTP_SHUTDOWNS.get() {
                        registry.lock().retain(|flag| {
                            flag.store(true, Ordering::Release);
                            Arc::strong_count(flag) > 1
                        });
                    }
                }
            }
        })
        .ok();
}

#[cfg(target_arch = "wasm32")]
fn register_http_shutdown(_shutdown: Arc<AtomicBool>) {}

static SIGINT_HOOKED: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
mod http_shutdown_tests {
    use super::*;

    #[test]
    fn sigterm_marks_registered_http_shutdown_flags() {
        let first = Arc::new(AtomicBool::new(false));
        let second = Arc::new(AtomicBool::new(false));
        install_http_shutdown_handler(Arc::clone(&first));
        install_http_shutdown_handler(Arc::clone(&second));

        gossamer_std::signal::deliver(gossamer_std::signal::sigs::SIGTERM);

        let deadline = gossamer_runtime::platform::Instant::now() + Duration::from_secs(1);
        while gossamer_runtime::platform::Instant::now() < deadline {
            if first.load(Ordering::Acquire) && second.load(Ordering::Acquire) {
                return;
            }
            gossamer_runtime::platform::sleep(Duration::from_millis(10));
        }

        assert!(first.load(Ordering::Acquire));
        assert!(second.load(Ordering::Acquire));
    }

    #[test]
    fn an_unsigned_value_reads_as_the_integer_it_carries() {
        // `as u64` / `as usize` answer an unsigned shape. A builtin that took
        // `None` here substituted a zero for whatever the caller passed.
        assert_eq!(super::value_to_int(&Value::Int(7)), Some(7));
        assert_eq!(super::value_to_int(&Value::Uint(7)), Some(7));
        assert_eq!(
            super::value_to_int(&Value::Uint(u64::MAX)),
            Some(-1),
            "a u64 carries the same 64 bits an i64 does"
        );
        assert_eq!(super::value_to_int(&Value::Unit), None);
    }
}
