//! Typed serde for compiled code: the reader `from_json::<T>` decodes through
//! and the writer `to_json::<T>` encodes into.
//!
//! The decode and encode walks themselves are code the compiler generates per
//! type (`gossamer-hir`'s serde pass). These entries are the steps it calls:
//! [`gossamer_core::json::typed::TypedReader`] behind a reader handle, and a
//! byte buffer behind a writer handle. The VM's builtins of the same names
//! drive the same reader, so every tier decodes alike.

use std::os::raw::c_char;

use gossamer_core::json::typed;

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

/// The text a reader walks: a share of the caller's `String`, held for the
/// walk instead of a copy, or text the reader made itself.
enum ReaderText {
    /// A `String` argument the reader took a share of, released on drop.
    Shared(*const c_char),
    Owned(String),
}

impl AsRef<str> for ReaderText {
    fn as_ref(&self) -> &str {
        match self {
            // SAFETY: the reader holds a share of the string, so its body
            // stays live until the drop below, and `rd_new` checked it is
            // UTF-8 before sharing it.
            Self::Shared(text) => unsafe { super::string::gos_str_arg_text(*text) },
            Self::Owned(text) => text,
        }
    }
}

impl Drop for ReaderText {
    fn drop(&mut self) {
        if let Self::Shared(text) = *self {
            // SAFETY: gives back the share `rd_new` took.
            unsafe { super::string::gos_rt_str_free_typed(text.cast_mut()) };
        }
    }
}

type TypedReader = typed::TypedReader<ReaderText>;

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

/// Opens a reader over `text`, validated as `json::parse` would.
///
/// The reader takes a share of the string rather than copying it: the
/// caller may release its own share once this call is its last use, and the
/// text stays live until `rd_free` gives the reader's share back.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_rd_new(text: *const c_char) -> i64 {
    ffi_entry!({
        let owned = |text: &str| TypedReader::over(ReaderText::Owned(text.to_owned()));
        let reader = if text.is_null() {
            owned("")
        } else {
            // SAFETY: `text` is this shim's String argument, live for the call.
            let bytes = unsafe { gos_str_arg_bytes(text) };
            // SAFETY: as above.
            let known_utf8 = unsafe { super::string::typed_str_known_utf8(text) };
            if known_utf8 || std::str::from_utf8(bytes).is_ok() {
                // SAFETY: as above; the share lives in the reader until
                // `ReaderText`'s drop gives it back.
                unsafe { super::string::gos_rt_str_retain_typed(text) };
                TypedReader::over(ReaderText::Shared(text))
            } else {
                match gossamer_core::json::validate(bytes) {
                    Ok(_) => owned(""),
                    Err(error) => TypedReader::rejected(ReaderText::Owned(String::new()), &error),
                }
            }
        };
        Box::into_raw(Box::new(reader)) as i64
    })
}

/// Opens a reader over the `json::Value` `j`, for `json::from_value::<T>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_rd_new_value(j: *const GosJson) -> i64 {
    ffi_entry!({
        // SAFETY: `j` is this shim's json::Value argument, live for the call.
        let text = unsafe { super::json::json_value_text(j) };
        Box::into_raw(Box::new(TypedReader::over(ReaderText::Owned(text)))) as i64
    })
}

/// A reader over `text` converted to JSON by `convert`, or one that has
/// failed with the conversion's message.
fn converted_reader(converted: Result<String, String>) -> i64 {
    let reader = match converted {
        Ok(json) => TypedReader::over(ReaderText::Owned(json)),
        Err(message) => TypedReader::failing(ReaderText::Owned(String::new()), &message),
    };
    Box::into_raw(Box::new(reader)) as i64
}

/// Opens a reader over a YAML document, for `yaml::decode::<T>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_rd_new_yaml(text: *const c_char) -> i64 {
    ffi_entry!({
        // SAFETY: `text` is this shim's String argument, live for the call or null.
        let text = unsafe { text_arg(text) };
        converted_reader(
            crate::yaml_node::decode_json(
                text,
                crate::yaml_node::max_depth(),
                crate::yaml_node::max_size(),
            )
            .and_then(|value| serde_json::to_string(&value).map_err(|e| e.to_string())),
        )
    })
}

/// Opens a reader over a TOML document, for `toml::decode::<T>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_rd_new_toml(text: *const c_char) -> i64 {
    ffi_entry!({
        // SAFETY: `text` is this shim's String argument, live for the call or null.
        let text = unsafe { text_arg(text) };
        converted_reader(crate::codec::json_convert::toml_to_json(text))
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

/// The failure as an `errors::Error`: its path, then its message.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_rd_error(r: i64) -> *mut GosError {
    ffi_entry!({
        // SAFETY: `r` is this shim's reader handle.
        let error = unsafe { with_reader(r, None, |reader| reader.error().cloned()) };
        let text = error.map(|e| e.to_string()).unwrap_or_default();
        super::errors::error_new_from_bytes(text.as_bytes())
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

/// The current value as an integer within `min..=max`, for a field narrower
/// than `i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_rd_int(
    r: i64,
    expected: *const c_char,
    min: i64,
    max: i64,
) -> i64 {
    ffi_entry!({
        // SAFETY: `expected` is this shim's String argument.
        let expected = unsafe { text_arg(expected) };
        // SAFETY: `r` is this shim's reader handle.
        unsafe { with_reader(r, 0, |reader| reader.read_int(expected, min, max)) }
    })
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

/// The current value as a `char`, its Unicode scalar value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_rd_char(r: i64, message: *const c_char) -> i32 {
    ffi_entry!({
        // SAFETY: `message` is this shim's String argument.
        let message = unsafe { text_arg(message) };
        // SAFETY: `r` is this shim's reader handle.
        let c = unsafe { with_reader(r, '\0', |reader| reader.read_char(message)) };
        c as i32
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

/// Selects an internally tagged enum variant by the `tag` member of the
/// current object.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_rd_tagged(
    r: i64,
    variants: *const c_char,
    tag: *const c_char,
    name: *const c_char,
    fallback: i64,
) -> i64 {
    ffi_entry!({
        // SAFETY: `variants` is this shim's String argument.
        let variants = unsafe { text_arg(variants) };
        // SAFETY: `tag` is this shim's String argument.
        let tag = unsafe { text_arg(tag) };
        // SAFETY: `name` is this shim's String argument.
        let name = unsafe { text_arg(name) };
        let fallback_index = usize::try_from(fallback).unwrap_or(0);
        // SAFETY: `r` is this shim's reader handle.
        let chosen = unsafe {
            with_reader(r, fallback_index, |reader| {
                reader.begin_tagged(variants, tag, name, fallback_index)
            })
        };
        i64::try_from(chosen).unwrap_or(fallback)
    })
}

/// Bytes a serde writer's buffer starts with, past the allocator's small size
/// classes: growing a large document from a few bytes would leave a partly
/// used page behind in each class it passes through, on every encode.
pub const SERDE_WRITER_CAPACITY: usize = 64 * 1024;

/// Opens a writer.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_serde_wr_new() -> i64 {
    ffi_entry!({ Box::into_raw(Box::new(Vec::<u8>::with_capacity(SERDE_WRITER_CAPACITY))) as i64 })
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

/// Releases a writer, answering its text laid out as `json::encode_pretty`
/// lays out a document.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_wr_finish_pretty(w: i64) -> *mut c_char {
    ffi_entry!({
        if w == 0 {
            return alloc_cstring(b"");
        }
        // SAFETY: the handle is the address `gos_rt_serde_wr_new` leaked,
        // released once.
        let bytes = unsafe { Box::from_raw(w as *mut Vec<u8>) };
        let compact = String::from_utf8_lossy(&bytes);
        alloc_cstring(gossamer_core::json::typed::indent_text(&compact).as_bytes())
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

/// Releases a writer, answering its JSON text as YAML, members in the order
/// the walk wrote them: `Result<String, errors::Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_wr_finish_yaml(w: i64) -> i128 {
    ffi_entry!({
        // SAFETY: `w` is this shim's writer handle, released here.
        let text = unsafe { take_writer(w) };
        converted_result(crate::codec::json_convert::to_yaml(
            &text,
            crate::codec::json_convert::Members::AsWritten,
        ))
    })
}

/// Releases a writer, answering its JSON text as TOML, members in the order
/// the walk wrote them: `Result<String, errors::Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_wr_finish_toml(w: i64) -> i128 {
    ffi_entry!({
        // SAFETY: `w` is this shim's writer handle, released here.
        let text = unsafe { take_writer(w) };
        converted_result(crate::codec::json_convert::to_toml(
            &text,
            crate::codec::json_convert::Members::AsWritten,
        ))
    })
}

/// The text a writer holds, releasing the writer.
///
/// # Safety
///
/// `w` is zero or a handle `gos_rt_serde_wr_new` answered, released once.
unsafe fn take_writer(w: i64) -> String {
    if w == 0 {
        return String::new();
    }
    // SAFETY: the caller's contract.
    let bytes = unsafe { Box::from_raw(w as *mut Vec<u8>) };
    String::from_utf8_lossy(&bytes).into_owned()
}

/// A conversion's text as `Ok`, or its message as an `Err`.
fn converted_result(converted: Result<String, String>) -> i128 {
    match converted {
        Ok(text) => super::result::gos_rt_result_new(0, alloc_cstring(text.as_bytes()) as i64),
        Err(message) => super::result::gos_rt_result_new(
            1,
            super::errors::error_new_from_bytes(message.as_bytes()) as i64,
        ),
    }
}

/// Appends an object member's key text, after a `,` unless the member is the
/// object's first.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_wr_key(w: i64, text: *const c_char) {
    ffi_entry!({
        // SAFETY: `text` is this shim's String argument.
        let text = unsafe { text_arg(text) };
        // SAFETY: `w` is this shim's writer handle.
        unsafe {
            with_writer(w, |out| {
                gossamer_core::json::typed::write_key(out, text);
            });
        };
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

/// Appends an `f64` as a `json::Value` tree writes it, the spelling
/// `json::encode` gives every float.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_wr_f64_tree(w: i64, value: f64) {
    ffi_entry!({
        // SAFETY: `w` is this shim's writer handle.
        unsafe { with_writer(w, |out| crate::builtins::write_json_tree_float(out, value)) };
    });
}

/// Appends an `f32`, carried widened, as a `json::Value` tree writes it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_wr_f32_tree(w: i64, value: f64) {
    ffi_entry!({
        let value = crate::builtins::f32_as_decimal_double(value);
        // SAFETY: `w` is this shim's writer handle.
        unsafe { with_writer(w, |out| crate::builtins::write_json_tree_float(out, value)) };
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

/// Appends a `char` as the one-character string `json::encode` writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_serde_wr_char(w: i64, value: i32) {
    ffi_entry!({
        let c = u32::try_from(value)
            .ok()
            .and_then(char::from_u32)
            .unwrap_or('\u{FFFD}');
        let mut buf = [0u8; 4];
        let text = c.encode_utf8(&mut buf);
        // SAFETY: `w` is this shim's writer handle.
        unsafe {
            with_writer(w, |out| {
                out.push(b'"');
                super::json_escape::json_escape_into(text.as_bytes(), out);
                out.push(b'"');
            });
        };
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
        let mut reader = TypedReader::over(ReaderText::Owned("42".to_owned()));
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
