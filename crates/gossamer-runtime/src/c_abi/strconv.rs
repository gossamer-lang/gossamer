#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::wildcard_imports)]
#![allow(clippy::single_match_else)]
#![allow(clippy::uninlined_format_args)]

use std::os::raw::c_char;

use super::*;

/// Reads a (possibly null) c-string as UTF-8, defaulting to `""`.
unsafe fn str_of<'a>(s: *const c_char) -> &'a str {
    if s.is_null() {
        ""
    } else {
        unsafe { crate::c_abi::gos_str_arg_text(s) }
    }
}

unsafe fn bytes_of<'a>(ptr: *const u8, len: i64) -> Option<&'a str> {
    if ptr.is_null() || len < 0 {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
    std::str::from_utf8(bytes).ok()
}

unsafe fn str_range_of<'a>(s: *const c_char, start: i64, end: i64) -> Result<&'a str, String> {
    let len = unsafe { crate::c_abi::gos_str_arg_len(s) } as i64;
    if start < 0 || end < 0 || start > end || end > len {
        return Err(format!(
            "slice: range [{start}, {end}) out of bounds for length {len}"
        ));
    }
    let lo = start as usize;
    let hi = end as usize;
    let bytes = if s.is_null() {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(s.cast::<u8>(), len as usize) }
    };
    std::str::from_utf8(&bytes[lo..hi])
        .map_err(|_| format!("slice: range [{start}, {end}) does not fall on UTF-8 boundaries"))
}

/// Renders a `std::num` integer parse failure exactly as
/// `gossamer_std::strconv::ParseError` Displays it, so the compiled
/// tier's error text is byte-identical to `gos` (which formats
/// the `ParseError` returned by `gossamer_std`). `value` is the
/// trimmed input the parser saw.
fn int_err_text(value: &str, err: &std::num::ParseIntError) -> String {
    use std::num::IntErrorKind;
    match err.kind() {
        IntErrorKind::Empty => "empty input".to_string(),
        IntErrorKind::PosOverflow | IntErrorKind::NegOverflow => {
            format!("overflow parsing {value:?}")
        }
        _ => format!("invalid input: {value:?}"),
    }
}

/// `strconv::parse_i64(s) -> Result<i64, errors::Error>`.
/// Result is `*mut GosResult` (disc=0 Ok, disc=1 Err); the Ok
/// payload is the parsed integer, the Err payload is a
/// `*mut GosError` describing the parse failure. Mirrors
/// `gossamer_std::strconv::parse_i64` (trim, decimal `i64`, the
/// `ParseError` Display text) so it agrees with `gos`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_strconv_parse_i64(s: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        let trimmed = unsafe { str_of(s) }.trim();
        if trimmed.is_empty() {
            return unsafe { strconv_err("empty input") };
        }
        match trimmed.parse::<i64>() {
            Ok(n) => unsafe { gos_rt_result_new(0, n) },
            Err(e) => unsafe { strconv_err(&int_err_text(trimmed, &e)) },
        }
    })
}

/// Byte-counted variant of `strconv::parse_i64`.
///
/// The caller passes a borrowed UTF-8 byte slice, avoiding the temporary
/// runtime String that `strings::slice(...)? |> strconv::parse_i64`
/// would otherwise allocate before parsing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_strconv_parse_i64_bytes(ptr: *const u8, len: i64) -> i128 {
    ffi_entry!(0i128, {
        let Some(text) = (unsafe { bytes_of(ptr, len) }) else {
            return unsafe { strconv_err("invalid input: \"<non-utf8>\"") };
        };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return unsafe { strconv_err("empty input") };
        }
        match trimmed.parse::<i64>() {
            Ok(n) => unsafe { gos_rt_result_new(0, n) },
            Err(e) => unsafe { strconv_err(&int_err_text(trimmed, &e)) },
        }
    })
}

/// Range-counted variant of `strconv::parse_i64`.
///
/// This validates `s[start..end]` exactly like `strings::slice`, then parses the
/// borrowed range without allocating a temporary runtime `String`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_strconv_parse_i64_range(
    s: *const c_char,
    start: i64,
    end: i64,
) -> i128 {
    ffi_entry!(0i128, {
        let text = match unsafe { str_range_of(s, start, end) } {
            Ok(text) => text,
            Err(msg) => return unsafe { strconv_err(&msg) },
        };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return unsafe { strconv_err("empty input") };
        }
        match trimmed.parse::<i64>() {
            Ok(n) => unsafe { gos_rt_result_new(0, n) },
            Err(e) => unsafe { strconv_err(&int_err_text(trimmed, &e)) },
        }
    })
}

/// Legacy ABI helper for `strconv::parse_i64(s) -> Result<i64, errors::Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_strconv_atoi(s: *const c_char) -> i128 {
    unsafe { gos_rt_strconv_parse_i64(s) }
}

/// `strconv::parse_u64(s) -> Result<u64, errors::Error>`. Parses an
/// unsigned 64-bit decimal so a leading `-` is rejected and values up
/// to `u64::MAX` are accepted whole. Mirrors `gossamer_std::strconv::
/// parse_u64` so it agrees with `gos`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_strconv_parse_u64(s: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        let trimmed = unsafe { str_of(s) }.trim();
        if trimmed.is_empty() {
            return unsafe { strconv_err("empty input") };
        }
        match trimmed.parse::<u64>() {
            // The payload word carries the `u64`'s bits; the static type
            // reads them back unsigned.
            Ok(n) => unsafe { gos_rt_result_new(0, n.cast_signed()) },
            Err(e) => unsafe { strconv_err(&int_err_text(trimmed, &e)) },
        }
    })
}

/// `strconv::parse_f64(s) -> Result<f64, errors::Error>`.
/// Ok payload is the bit-pattern of the parsed `f64`
/// (`f64::to_bits` as i64); the LLVM lowering reads the i64
/// register and reinterprets as `f64` via `bitcast`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_strconv_parse_f64(s: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        // Mirrors `gossamer_std::strconv::parse_f64`: trim, reject the
        // empty input as `ParseError::Empty`, and surface every other
        // failure as `ParseError::Invalid`.
        let trimmed = unsafe { str_of(s) }.trim();
        if trimmed.is_empty() {
            return unsafe { strconv_err("empty input") };
        }
        match trimmed.parse::<f64>() {
            Ok(x) => unsafe { gos_rt_result_new(0, x.to_bits() as i64) },
            Err(_) => unsafe { strconv_err(&format!("invalid input: {trimmed:?}")) },
        }
    })
}

/// Byte-counted variant of `strconv::parse_f64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_strconv_parse_f64_bytes(ptr: *const u8, len: i64) -> i128 {
    ffi_entry!(0i128, {
        let Some(text) = (unsafe { bytes_of(ptr, len) }) else {
            return unsafe { strconv_err("invalid input: \"<non-utf8>\"") };
        };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return unsafe { strconv_err("empty input") };
        }
        match trimmed.parse::<f64>() {
            Ok(x) => unsafe { gos_rt_result_new(0, x.to_bits() as i64) },
            Err(_) => unsafe { strconv_err(&format!("invalid input: {trimmed:?}")) },
        }
    })
}

/// Range-counted variant of `strconv::parse_f64`.
///
/// This validates `s[start..end]` exactly like `strings::slice`, then parses the
/// borrowed range without allocating a temporary runtime `String`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_strconv_parse_f64_range(
    s: *const c_char,
    start: i64,
    end: i64,
) -> i128 {
    ffi_entry!(0i128, {
        let text = match unsafe { str_range_of(s, start, end) } {
            Ok(text) => text,
            Err(msg) => return unsafe { strconv_err(&msg) },
        };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return unsafe { strconv_err("empty input") };
        }
        match trimmed.parse::<f64>() {
            Ok(x) => unsafe { gos_rt_result_new(0, x.to_bits() as i64) },
            Err(_) => unsafe { strconv_err(&format!("invalid input: {trimmed:?}")) },
        }
    })
}

/// `strconv::parse_bool(s) -> Result<bool, errors::Error>`. Mirrors
/// `gossamer_std::strconv::parse_bool`: accepts only the exact,
/// untrimmed literals `"true"` and `"false"`; anything else is an
/// `Err` whose message matches the `ParseError::Invalid` Display.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_strconv_parse_bool(s: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        let text = unsafe { str_of(s) };
        match text {
            "true" => unsafe { gos_rt_result_new(0, 1) },
            "false" => unsafe { gos_rt_result_new(0, 0) },
            other => unsafe { strconv_err(&format!("invalid input: {other:?}")) },
        }
    })
}

/// `strconv::format_i64(n) -> String` - alias for `i64_to_str`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_strconv_format_i64(n: i64) -> *mut c_char {
    unsafe { gos_rt_i64_to_str(n) }
}

/// Legacy ABI helper for `strconv::format_i64(n) -> String`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_strconv_itoa(n: i64) -> *mut c_char {
    unsafe { gos_rt_i64_to_str(n) }
}

/// `strconv::format_f64(x) -> String` - alias for `f64_to_str`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_strconv_format_f64(x: f64) -> *mut c_char {
    unsafe { gos_rt_f64_to_str(x) }
}

/// `strconv::format_bool(b) -> String` - alias for `bool_to_str`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_strconv_format_bool(b: i32) -> *mut c_char {
    unsafe { gos_rt_bool_to_str(b) }
}

/// Packs an `Err(errors::Error)` result carrying `msg`.
unsafe fn strconv_err(msg: &str) -> i128 {
    let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
    unsafe { gos_rt_result_new(1, err as i64) }
}

/// `strconv::parse_i64_radix(s, base) -> Result<i64, errors::Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_strconv_parse_i64_radix(s: *const c_char, base: i64) -> i128 {
    ffi_entry!(0i128, {
        // Mirrors `gossamer_std::strconv::parse_i64_radix` exactly,
        // including the check order (empty before base) and the
        // `ParseError` Display text. The interpreter converts the base
        // with `u32::try_from(base).unwrap_or(0)`, so an out-of-range
        // base reports the converted radix (0 for negatives) and the
        // message is itself wrapped as `ParseError::Invalid`.
        let trimmed = unsafe { str_of(s) }.trim();
        if trimmed.is_empty() {
            return unsafe { strconv_err("empty input") };
        }
        let radix = u32::try_from(base).unwrap_or(0);
        if !(2..=36).contains(&radix) {
            let inner = format!("invalid base {radix}");
            return unsafe { strconv_err(&format!("invalid input: {inner:?}")) };
        }
        match i64::from_str_radix(trimmed, radix) {
            Ok(n) => unsafe { gos_rt_result_new(0, n) },
            Err(e) => unsafe { strconv_err(&int_err_text(trimmed, &e)) },
        }
    })
}

/// `strconv::format_i64_radix(n, base) -> String`. Out-of-range bases fall
/// back to decimal; digits a-z are lowercase.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_strconv_format_i64_radix(n: i64, base: i64) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let radix = u32::try_from(base).unwrap_or(10);
        let out = if !(2..=36).contains(&radix) || n == 0 {
            if n == 0 {
                "0".to_string()
            } else {
                n.to_string()
            }
        } else {
            let negative = n < 0;
            let mut v = i128::from(n).unsigned_abs();
            let r = u128::from(radix);
            let mut digits = Vec::new();
            while v > 0 {
                let d = (v % r) as u32;
                digits.push(std::char::from_digit(d, radix).unwrap_or('0'));
                v /= r;
            }
            if negative {
                digits.push('-');
            }
            digits.iter().rev().collect()
        };
        alloc_cstring(out.as_bytes())
    })
}

/// Format-spec intrinsic for `{:b}` / `{:o}` / `{:x}`-style integer
/// rendering. Negative values are rendered as their 64-bit two's-complement
/// bit pattern, matching Rust's radix formatters.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_fmt_radix_i64(n: i64, base: i64) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let radix = u32::try_from(base).unwrap_or(10);
        if !(2..=36).contains(&radix) {
            return alloc_cstring(n.to_string().as_bytes());
        }
        let mut v = u128::from(n as u64);
        if v == 0 {
            return alloc_cstring(b"0");
        }
        let r = u128::from(radix);
        let mut digits = Vec::new();
        while v > 0 {
            let d = (v % r) as u32;
            digits.push(std::char::from_digit(d, radix).unwrap_or('0'));
            v /= r;
        }
        let out: String = digits.iter().rev().collect();
        alloc_cstring(out.as_bytes())
    })
}

/// `strconv::quote(s) -> String` - double-quotes `s`, escaping `"`, `\`, and
/// control characters so [`gos_rt_strconv_unquote`] reverses it exactly.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_strconv_quote(s: *const c_char) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let text = if s.is_null() {
            ""
        } else {
            unsafe { crate::c_abi::gos_str_arg_text(s) }
        };
        let mut out = String::with_capacity(text.len() + 2);
        out.push('"');
        for c in text.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\t' => out.push_str("\\t"),
                '\r' => out.push_str("\\r"),
                c if c.is_control() => out.push_str(&format!("\\u{{{:04x}}}", c as u32)),
                c => out.push(c),
            }
        }
        out.push('"');
        alloc_cstring(out.as_bytes())
    })
}

/// `{:?}` of a `String`: the string between double quotes, escaped as Rust's
/// `Debug` escapes it, the same text a string nested in a container shows.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_debug_quote_str(s: *const c_char) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let text = if s.is_null() {
            ""
        } else {
            unsafe { crate::c_abi::gos_str_arg_text(s) }
        };
        alloc_cstring(format!("{text:?}").as_bytes())
    })
}

/// `{:?}` of a `char`: the character between single quotes, escaped as
/// Rust's `Debug` escapes it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_debug_quote_char(c: i64) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let ch = u32::try_from(c)
            .ok()
            .and_then(char::from_u32)
            .unwrap_or('\u{FFFD}');
        alloc_cstring(format!("{ch:?}").as_bytes())
    })
}

/// `strconv::unquote(s) -> Result<String, errors::Error>` - reverses
/// [`gos_rt_strconv_quote`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_strconv_unquote(s: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        // Mirrors `gossamer_std::strconv::unquote`: every failure mode
        // surfaces as `ParseError::Invalid(original_input)`, so the
        // error text matches `gos` byte-for-byte.
        let text = unsafe { str_of(s) };
        if text.len() < 2 || !text.starts_with('"') || !text.ends_with('"') {
            return unsafe { strconv_err(&format!("invalid input: {text:?}")) };
        }
        let inner = &text[1..text.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        let mut bad = false;
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('u') => {
                    if chars.next() != Some('{') {
                        bad = true;
                        break;
                    }
                    let mut hex = String::new();
                    for hc in chars.by_ref() {
                        if hc == '}' {
                            break;
                        }
                        hex.push(hc);
                    }
                    match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                        Some(ch) => out.push(ch),
                        None => {
                            bad = true;
                            break;
                        }
                    }
                }
                _ => {
                    bad = true;
                    break;
                }
            }
        }
        if bad {
            return unsafe { strconv_err(&format!("invalid input: {text:?}")) };
        }
        let ptr = alloc_cstring(out.as_bytes());
        unsafe { gos_rt_result_new(0, ptr as i64) }
    })
}
