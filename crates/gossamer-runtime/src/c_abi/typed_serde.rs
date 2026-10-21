//! Typed serde for compiled code: the reader `from_json::<T>` decodes through
//! and the writer `to_json::<T>` encodes into.
//!
//! The decode and encode walks themselves are code the compiler generates per
//! type (`gossamer-hir`'s serde pass). These entries are the steps it calls:
//! [`gossamer_core::json::typed::TypedReader`] behind a reader handle, and a
//! byte buffer behind a writer handle. The VM's builtins of the same names
//! drive the same reader, so every tier decodes alike.

use std::os::raw::c_char;

use gossamer_core::json::typed::TypedReader;

use super::errors::GosError;
use super::json::GosJson;
use super::{alloc_cstring, gos_str_arg_bytes};

/// The text of a String argument, empty for null or for bytes that are not
/// UTF-8.
///
/// # Safety
///
/// `s` is null or a live string body for the whole call.
unsafe fn text_arg<'a>(s: *const c_char) -> &'a str {
    if s.is_null() {
        return "";
    }
    // SAFETY: the caller passes a live string body.
    unsafe { super::string::gos_str_arg_text(s) }
}

/// Runs `f` on the reader handle `r` names, or answers `fallback` for none.
///
/// # Safety
///
/// `r` is zero or a handle `gos_rt_serde_rd_new` answered that nothing else
/// uses for the call.
unsafe fn with_reader<T>(r: i64, fallback: T, f: impl FnOnce(&mut TypedReader) -> T) -> T {
    // SAFETY: the caller provides the same reader-handle contract.
    unsafe { with_reader_or_else(r, || fallback, f) }
}

/// Like `with_reader`, but only constructs an owned fallback for a missing reader.
///
/// # Safety
///
/// The reader-handle contract is the same as `with_reader`.
unsafe fn with_reader_or_else<T>(
    r: i64,
    fallback: impl FnOnce() -> T,
    f: impl FnOnce(&mut TypedReader) -> T,
) -> T {
    if r == 0 {
        return fallback();
    }
    // SAFETY: a nonzero handle is the address `gos_rt_serde_rd_new` leaked,
    // live until `gos_rt_serde_rd_free`, and the caller holds it alone.
    let reader = unsafe { &mut *(r as *mut TypedReader) };
    f(reader)
}

/// Runs `f` on the bytes the writer handle `w` names.
///
/// # Safety
///
/// `w` is zero or a handle `gos_rt_serde_wr_new` answered that nothing else
/// uses for the call.
unsafe fn with_writer(w: i64, f: impl FnOnce(&mut Vec<u8>)) {
    if w == 0 {
        return;
    }
    // SAFETY: a nonzero handle is the address `gos_rt_serde_wr_new` leaked,
    // live until `gos_rt_serde_wr_finish`, and the caller holds it alone.
    let out = unsafe { &mut *(w as *mut Vec<u8>) };
    f(out);
}

fn index_arg(index: i64) -> usize {
    usize::try_from(index).unwrap_or(usize::MAX)
}

/// Opens a reader over a copy of `text`, validated as `json::parse` would.
///
/// The reader keeps its own copy: the text's last use is this call, so the
/// caller may release it before the walk ends.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_rd_new(text: *const c_char) -> i64 {
    ffi_entry!({
        let reader = if text.is_null() {
            TypedReader::new("")
        } else {
            // SAFETY: `text` is this shim's String argument, live for the call.
            let bytes = unsafe { gos_str_arg_bytes(text) };
            // SAFETY: as above.
            let known_utf8 = unsafe { super::string::typed_str_known_utf8(text) };
            if known_utf8 {
                // SAFETY: as above, and its index footer says it is UTF-8.
                TypedReader::new(unsafe { super::string::gos_str_arg_text(text) })
            } else {
                match std::str::from_utf8(bytes) {
                    Ok(source) => TypedReader::new(source),
                    Err(_) => match gossamer_core::json::validate(bytes) {
                        Ok(_) => TypedReader::new(""),
                        Err(error) => TypedReader::rejected(String::new(), &error),
                    },
                }
            }
        };
        Box::into_raw(Box::new(reader)) as i64
    })
}

/// Releases a reader.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_rd_free(r: i64) {
    ffi_entry!({
        if r != 0 {
            // SAFETY: the handle is the address `gos_rt_serde_rd_new` leaked,
            // released once.
            drop(unsafe { Box::from_raw(r as *mut TypedReader) });
        }
    });
}

/// Whether a read has failed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_rd_failed(r: i64) -> i8 {
    ffi_entry!({
        // SAFETY: `r` is this shim's reader handle.
        i8::from(unsafe { with_reader(r, false, |reader| reader.failed()) })
    })
}

/// The failure as an `errors::Error`, wrapped once per context the nested
/// decodes added, outermost last.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_rd_error(r: i64) -> *mut GosError {
    ffi_entry!({
        // SAFETY: `r` is this shim's reader handle.
        let error = unsafe { with_reader(r, None, |reader| reader.error().cloned()) };
        let Some(error) = error else {
            return super::errors::error_new_from_bytes(b"");
        };
        let mut err = super::errors::error_new_from_bytes(error.message.as_bytes());
        for context in &error.contexts {
            // SAFETY: the message is a fresh string, and `err` is the cell built
            // above, whose only share the new cell takes over.
            err = unsafe {
                super::errors::error_alloc(alloc_cstring(context.as_bytes()), err, Vec::new())
            };
        }
        err
    })
}

/// Records `message` as the failure unless one is recorded.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_rd_fail(r: i64, message: *const c_char) {
    ffi_entry!({
        // SAFETY: `message` is this shim's String argument.
        let message = unsafe { text_arg(message) };
        // SAFETY: `r` is this shim's reader handle.
        unsafe { with_reader(r, (), |reader| reader.fail(message)) };
    });
}

/// `1` when the reader has failed, taken before a nested decode so
/// `gos_rt_serde_rd_wrap` can tell whether the decode failed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_rd_mark(r: i64) -> i64 {
    ffi_entry!({
        // SAFETY: `r` is this shim's reader handle.
        i64::from(unsafe { with_reader(r, false, |reader| reader.failed()) })
    })
}

/// Adds `context` to a failure that happened after `mark` was taken.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_rd_wrap(r: i64, mark: i64, context: *const c_char) {
    ffi_entry!({
        // SAFETY: `context` is this shim's String argument.
        let context = unsafe { text_arg(context) };
        // SAFETY: `r` is this shim's reader handle.
        unsafe { with_reader(r, (), |reader| reader.wrap(mark != 0, context)) };
    });
}

/// Steps out of the value the last step went into.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_rd_leave(r: i64) {
    ffi_entry!({
        // SAFETY: `r` is this shim's reader handle.
        unsafe { with_reader(r, (), TypedReader::leave) };
    });
}

/// Whether the current value is `null` or absent.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_rd_is_null(r: i64) -> i8 {
    ffi_entry!({
        // SAFETY: `r` is this shim's reader handle.
        i8::from(unsafe { with_reader(r, true, |reader| reader.is_null()) })
    })
}

/// Records where each encoded key's value starts in the current object.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_rd_struct(r: i64, keys: *const c_char) {
    ffi_entry!({
        // SAFETY: `keys` is this shim's String argument.
        let keys = unsafe { text_arg(keys) };
        // SAFETY: `r` is this shim's reader handle.
        unsafe { with_reader(r, (), |reader| reader.begin_struct(keys)) };
    });
}

/// Steps into field `index`, answering whether the document has it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_rd_field(r: i64, index: i64) -> i8 {
    ffi_entry!({
        // SAFETY: `r` is this shim's reader handle.
        i8::from(unsafe { with_reader(r, false, |reader| reader.enter_field(index_arg(index))) })
    })
}

/// The current value as an `i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_rd_i64(r: i64, message: *const c_char) -> i64 {
    ffi_entry!({
        // SAFETY: `message` is this shim's String argument.
        let message = unsafe { text_arg(message) };
        // SAFETY: `r` is this shim's reader handle.
        unsafe { with_reader(r, 0, |reader| reader.read_i64(message)) }
    })
}

/// The current value as a `u64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_rd_u64(r: i64, message: *const c_char) -> u64 {
    ffi_entry!({
        // SAFETY: `message` is this shim's String argument.
        let message = unsafe { text_arg(message) };
        // SAFETY: `r` is this shim's reader handle.
        unsafe { with_reader(r, 0, |reader| reader.read_u64(message)) }
    })
}

/// The current value as an `f64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_rd_f64(r: i64, message: *const c_char) -> f64 {
    ffi_entry!({
        // SAFETY: `message` is this shim's String argument.
        let message = unsafe { text_arg(message) };
        // SAFETY: `r` is this shim's reader handle.
        unsafe { with_reader(r, 0.0, |reader| reader.read_f64(message)) }
    })
}

/// The current value as a `bool`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_rd_bool(r: i64, message: *const c_char) -> i8 {
    ffi_entry!({
        // SAFETY: `message` is this shim's String argument.
        let message = unsafe { text_arg(message) };
        // SAFETY: `r` is this shim's reader handle.
        i8::from(unsafe { with_reader(r, false, |reader| reader.read_bool(message)) })
    })
}

/// The current value as a String.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_rd_str(r: i64, message: *const c_char) -> *mut c_char {
    ffi_entry!({
        // SAFETY: `message` is this shim's String argument.
        let message = unsafe { text_arg(message) };
        // SAFETY: `r` is this shim's reader handle.
        let text = unsafe { with_reader(r, String::new(), |reader| reader.read_string(message)) };
        alloc_cstring(text.as_bytes())
    })
}

/// The current value as a `json::Value`, `null` when it is absent.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_rd_json(r: i64) -> *mut GosJson {
    ffi_entry!({
        // SAFETY: `r` is this shim's reader handle.
        unsafe {
            with_reader_or_else(
                r,
                || super::json::json_raw_handle("null"),
                |reader| super::json::json_raw_handle(reader.raw_value()),
            )
        }
    })
}

/// The element count of the current array.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_rd_array(r: i64, message: *const c_char) -> i64 {
    ffi_entry!({
        // SAFETY: `message` is this shim's String argument.
        let message = unsafe { text_arg(message) };
        // SAFETY: `r` is this shim's reader handle.
        let count = unsafe { with_reader(r, 0, |reader| reader.begin_array(message)) };
        i64::try_from(count).unwrap_or(i64::MAX)
    })
}

/// Checks the current value is an array of exactly `len` elements.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_rd_tuple(
    r: i64,
    len: i64,
    not_array: *const c_char,
    wrong_len: *const c_char,
) {
    ffi_entry!({
        // SAFETY: `not_array` is this shim's String argument.
        let not_array = unsafe { text_arg(not_array) };
        // SAFETY: `wrong_len` is this shim's String argument.
        let wrong_len = unsafe { text_arg(wrong_len) };
        let len = usize::try_from(len).unwrap_or(0);
        // SAFETY: `r` is this shim's reader handle.
        unsafe {
            with_reader(r, (), |reader| {
                reader.begin_tuple(len, not_array, wrong_len);
            });
        }
    });
}

/// Steps into element `index` of the array or tuple just begun.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_rd_elem(r: i64, index: i64) {
    ffi_entry!({
        // SAFETY: `r` is this shim's reader handle.
        unsafe { with_reader(r, (), |reader| reader.enter_elem(index_arg(index))) };
    });
}

/// Steps into element `index` of the current value, as `json::at` reads it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_rd_at(r: i64, index: i64) {
    ffi_entry!({
        // SAFETY: `r` is this shim's reader handle.
        unsafe { with_reader(r, (), |reader| reader.enter_at(index_arg(index))) };
    });
}

/// The entry count of the current object, read as a map.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_rd_map(r: i64, message: *const c_char) -> i64 {
    ffi_entry!({
        // SAFETY: `message` is this shim's String argument.
        let message = unsafe { text_arg(message) };
        // SAFETY: `r` is this shim's reader handle.
        let count = unsafe { with_reader(r, 0, |reader| reader.begin_map(message)) };
        i64::try_from(count).unwrap_or(i64::MAX)
    })
}

/// The key of map entry `index`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_rd_map_key(r: i64, index: i64) -> *mut c_char {
    ffi_entry!({
        // SAFETY: `r` is this shim's reader handle.
        unsafe {
            with_reader_or_else(
                r,
                || alloc_cstring(b""),
                |reader| alloc_cstring(reader.map_key(index_arg(index)).as_bytes()),
            )
        }
    })
}

/// Steps into the value of map entry `index`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_rd_map_value(r: i64, index: i64) {
    ffi_entry!({
        // SAFETY: `r` is this shim's reader handle.
        unsafe { with_reader(r, (), |reader| reader.enter_map_value(index_arg(index))) };
    });
}

/// Selects an enum variant, stepping into its payload when it has one.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_rd_enum(
    r: i64,
    variants: *const c_char,
    name: *const c_char,
    fallback: i64,
) -> i64 {
    ffi_entry!({
        // SAFETY: `variants` is this shim's String argument.
        let variants = unsafe { text_arg(variants) };
        // SAFETY: `name` is this shim's String argument.
        let name = unsafe { text_arg(name) };
        let fallback_index = usize::try_from(fallback).unwrap_or(0);
        // SAFETY: `r` is this shim's reader handle.
        let chosen = unsafe {
            with_reader(r, fallback_index, |reader| {
                reader.begin_enum(variants, name, fallback_index)
            })
        };
        i64::try_from(chosen).unwrap_or(fallback)
    })
}

/// Opens a writer.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_serde_wr_new() -> i64 {
    ffi_entry!({ Box::into_raw(Box::new(Vec::<u8>::with_capacity(256))) as i64 })
}

/// Releases a writer, answering the text written into it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_wr_finish(w: i64) -> *mut c_char {
    ffi_entry!({
        if w == 0 {
            return alloc_cstring(b"");
        }
        // SAFETY: the handle is the address `gos_rt_serde_wr_new` leaked,
        // released once.
        let bytes = unsafe { Box::from_raw(w as *mut Vec<u8>) };
        alloc_cstring(&bytes)
    })
}

/// Appends `text` as it is: punctuation and the keys the compiler spelled.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_wr_raw(w: i64, text: *const c_char) {
    ffi_entry!({
        // SAFETY: `text` is this shim's String argument.
        let text = unsafe { text_arg(text) };
        // SAFETY: `w` is this shim's writer handle.
        unsafe { with_writer(w, |out| out.extend_from_slice(text.as_bytes())) };
    });
}

/// Appends the `,` that separates element `index` from the one before it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_wr_sep(w: i64, index: i64) {
    ffi_entry!({
        if index > 0 {
            // SAFETY: `w` is this shim's writer handle.
            unsafe { with_writer(w, |out| out.push(b',')) };
        }
    });
}

/// Appends an integer as `{}` renders it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_wr_i64(w: i64, value: i64) {
    ffi_entry!({
        // SAFETY: `w` is this shim's writer handle.
        unsafe {
            with_writer(w, |out| {
                out.extend_from_slice(itoa::Buffer::new().format(value).as_bytes());
            });
        }
    });
}

/// Appends an unsigned integer as `{}` renders it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_wr_u64(w: i64, value: u64) {
    ffi_entry!({
        // SAFETY: `w` is this shim's writer handle.
        unsafe {
            with_writer(w, |out| {
                out.extend_from_slice(itoa::Buffer::new().format(value).as_bytes());
            });
        }
    });
}

/// Appends an `f64` as `{}` renders it, or `null` for NaN or an infinity.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_wr_f64(w: i64, value: f64) {
    ffi_entry!({
        // SAFETY: `w` is this shim's writer handle.
        unsafe {
            with_writer(w, |out| {
                if value.is_finite() {
                    let mut text = crate::builtins::FloatText::new();
                    out.extend_from_slice(crate::builtins::f64_display(value, &mut text));
                } else {
                    out.extend_from_slice(b"null");
                }
            });
        }
    });
}

/// Appends an `f32`, carried widened, as `{}` renders it, or `null` for NaN
/// or an infinity.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_wr_f32(w: i64, value: f64) {
    ffi_entry!({
        // SAFETY: `w` is this shim's writer handle.
        unsafe {
            with_writer(w, |out| {
                if value.is_finite() {
                    out.extend_from_slice(crate::builtins::format_f32(value).as_bytes());
                } else {
                    out.extend_from_slice(b"null");
                }
            });
        }
    });
}

/// Appends `true` or `false`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_wr_bool(w: i64, value: i32) {
    ffi_entry!({
        let text: &[u8] = if value != 0 { b"true" } else { b"false" };
        // SAFETY: `w` is this shim's writer handle.
        unsafe { with_writer(w, |out| out.extend_from_slice(text)) };
    });
}

/// Appends a String as `json::encode` writes one: quoted, escaped, and
/// HTML-safe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_wr_str(w: i64, text: *const c_char) {
    ffi_entry!({
        let bytes: &[u8] = if text.is_null() {
            b""
        } else {
            // SAFETY: `text` is this shim's String argument.
            unsafe { gos_str_arg_bytes(text) }
        };
        // SAFETY: `w` is this shim's writer handle.
        unsafe {
            with_writer(w, |out| {
                out.push(b'"');
                super::json_escape::json_escape_into(bytes, out);
                out.push(b'"');
            });
        }
    });
}

/// Appends a `json::Value` as `json::render` writes it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_wr_json(w: i64, value: *const GosJson) {
    ffi_entry!({
        let rendered = if value.is_null() {
            b"null".to_vec()
        } else {
            // SAFETY: `value` is this shim's JSON argument, non-null and live.
            super::json::render_json_bytes(unsafe { &*value }, false)
        };
        // SAFETY: `w` is this shim's writer handle.
        unsafe { with_writer(w, |out| out.extend_from_slice(&rendered)) };
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_fallback_is_constructed_only_for_a_missing_handle() {
        let calls = std::cell::Cell::new(0);
        let fallback = || {
            calls.set(calls.get() + 1);
            String::from("fallback")
        };
        let mut reader = TypedReader::new("42");
        let handle = (&raw mut reader) as i64;
        // SAFETY: the stack reader stays live and exclusively borrowed for this call.
        let value = unsafe {
            with_reader_or_else(handle, fallback, |reader| reader.raw_value().to_owned())
        };
        assert_eq!(value, "42");
        assert_eq!(
            calls.get(),
            0,
            "live readers must not allocate unused fallbacks"
        );
        // SAFETY: zero denotes a missing reader.
        let value = unsafe { with_reader_or_else(0, fallback, |_| panic!("missing reader")) };
        assert_eq!(value, "fallback");
        assert_eq!(calls.get(), 1);
    }
}
