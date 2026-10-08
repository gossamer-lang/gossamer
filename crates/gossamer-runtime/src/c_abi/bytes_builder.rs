#![allow(clippy::missing_safety_doc)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::cast_possible_truncation)]
// `use super::*` pulls in the shared FFI helpers (`alloc_cstring`,
// `gos_rt_result_new`, the `vec` module) the same way every other
// c_abi shim does; the glob is the established convention here.
#![allow(clippy::wildcard_imports)]

//! Runtime support for `std::bytes` - the `Builder` (string assembly)
//! and `Buffer` (byte accumulation) handle types plus the stateless
//! `index_of` / `split` / `replace` helpers.
//!
//! Both handle types are opaque heap `Box`es; compiled tiers carry
//! the pointer as an `i64` and the MIR receiver-kind dispatch tags
//! constructor results `bytes::Builder` / `bytes::Buffer` so method
//! calls route to the helpers below. The handle is never freed (it
//! leaks at process exit), matching `sync::Map` / `math::rand::Rng`:
//! these are long-lived assembly scratch buffers, not graph nodes.
//!
//! `build` / `as_str` / `to_string` are non-destructive: they clone
//! the accumulated contents into a fresh runtime c-string and leave
//! the handle usable, so the receiver pointer the MIR still holds
//! never dangles.
//!
//! The byte helpers mirror `gossamer_std::bytes` exactly (rather than
//! importing it, which would form a `runtime -> std -> runtime`
//! dependency cycle) so the VM and compiled tiers agree bit-for-bit.

use std::os::raw::c_char;

use super::*;

/// # Safety
///
/// `p` is null or a live string body.
unsafe fn cstr_to_string(p: *const c_char) -> String {
    // SAFETY: this function's contract is the one the reader states for `p`.
    unsafe { crate::c_abi::gos_str_arg_string(p) }
}

/// First index at which `needle` appears in `haystack`, or `None`.
/// Bit-identical to `gossamer_std::bytes::index_of`.
fn bytes_index_of(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Splits `haystack` on every `separator` occurrence into owned chunks.
/// Bit-identical to `gossamer_std::bytes::split`.
fn bytes_split(haystack: &[u8], separator: &[u8]) -> Vec<Vec<u8>> {
    if separator.is_empty() {
        return vec![haystack.to_vec()];
    }
    let mut out = Vec::new();
    let mut cursor = 0;
    while cursor <= haystack.len() {
        if let Some(off) = bytes_index_of(&haystack[cursor..], separator) {
            out.push(haystack[cursor..cursor + off].to_vec());
            cursor += off + separator.len();
        } else {
            out.push(haystack[cursor..].to_vec());
            break;
        }
    }
    out
}

/// Replaces every `from` in `haystack` with `to`. Bit-identical to
/// `gossamer_std::bytes::replace`.
fn bytes_replace(haystack: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    if from.is_empty() {
        return haystack.to_vec();
    }
    let parts = bytes_split(haystack, from);
    let mut out = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(to);
        }
        out.extend_from_slice(part);
    }
    out
}

// ---------------------------------------------------------------
// bytes::Builder - incremental String assembly
// ---------------------------------------------------------------

/// Opaque heap handle wrapping the accumulated string.
pub struct GosBytesBuilder {
    inner: String,
}

super::rc::managed_handle!(GosBytesBuilder);

/// Allocate an empty `bytes::Builder`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_bytes_builder_new() -> *mut GosBytesBuilder {
    ffi_entry!(std::ptr::null_mut(), {
        super::rc::alloc_managed(GosBytesBuilder {
            inner: String::new(),
        })
    })
}

/// Allocate a `bytes::Builder` with `n` bytes of reserved capacity.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_bytes_builder_with_capacity(n: i64) -> *mut GosBytesBuilder {
    ffi_entry!(std::ptr::null_mut(), {
        super::rc::alloc_managed(GosBytesBuilder {
            inner: String::with_capacity(n.max(0) as usize),
        })
    })
}

/// Append `text` to the builder.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bytes_builder_write(b: *mut GosBytesBuilder, text: *const c_char) {
    ffi_entry!((), {
        if b.is_null() {
            return;
        }
        // SAFETY: `text` is this shim's argument, as `cstr_to_string` requires (C-ABI contract).
        let s = unsafe { cstr_to_string(text) };
        // SAFETY: `b` is a handle from compiled code, checked non-null above and live for the whole call.
        unsafe { &mut *b }.inner.push_str(&s);
    });
}

/// Append a single Unicode scalar `ch` to the builder. An invalid
/// scalar value is substituted with U+FFFD rather than unwinding.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bytes_builder_write_char(b: *mut GosBytesBuilder, ch: i32) {
    ffi_entry!((), {
        if b.is_null() {
            return;
        }
        let c = char::from_u32(ch as u32).unwrap_or('\u{FFFD}');
        // SAFETY: `b` is a handle from compiled code, checked non-null above and live for the whole call.
        unsafe { &mut *b }.inner.push(c);
    });
}

/// Return the accumulated string as a fresh runtime c-string. Does
/// not consume the handle (see module note).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bytes_builder_build(b: *mut GosBytesBuilder) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if b.is_null() {
            return alloc_cstring(b"");
        }
        // SAFETY: `b` is a handle from compiled code, checked non-null above and live for the whole call.
        alloc_cstring(unsafe { &*b }.inner.as_bytes())
    })
}

/// Borrowed view of the accumulated string as a fresh runtime
/// c-string (identical contents to `build`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bytes_builder_as_str(b: *mut GosBytesBuilder) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if b.is_null() {
            return alloc_cstring(b"");
        }
        // SAFETY: `b` is a handle from compiled code, checked non-null above and live for the whole call.
        alloc_cstring(unsafe { &*b }.inner.as_bytes())
    })
}

/// Byte length of the accumulated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bytes_builder_len(b: *mut GosBytesBuilder) -> i64 {
    ffi_entry!(0, {
        if b.is_null() {
            return 0;
        }
        // SAFETY: `b` is a handle from compiled code, checked non-null above and live for the whole call.
        unsafe { &*b }.inner.len() as i64
    })
}

// ---------------------------------------------------------------
// bytes::Buffer - byte accumulation (string-oriented surface)
// ---------------------------------------------------------------

/// Opaque heap handle wrapping the accumulated bytes.
pub struct GosBytesBuffer {
    inner: Vec<u8>,
}

super::rc::managed_handle!(GosBytesBuffer);

/// Allocate an empty `bytes::Buffer`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_bytes_buffer_new() -> *mut GosBytesBuffer {
    ffi_entry!(std::ptr::null_mut(), {
        super::rc::alloc_managed(GosBytesBuffer { inner: Vec::new() })
    })
}

/// Allocate a `bytes::Buffer` with `n` bytes of reserved capacity.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_bytes_buffer_with_capacity(n: i64) -> *mut GosBytesBuffer {
    ffi_entry!(std::ptr::null_mut(), {
        super::rc::alloc_managed(GosBytesBuffer {
            inner: Vec::with_capacity(n.max(0) as usize),
        })
    })
}

/// Append `text`'s UTF-8 bytes to the buffer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bytes_buffer_write_str(
    buf: *mut GosBytesBuffer,
    text: *const c_char,
) {
    ffi_entry!((), {
        if buf.is_null() {
            return;
        }
        // SAFETY: `text` is this shim's argument, as `cstr_to_string` requires (C-ABI contract).
        let s = unsafe { cstr_to_string(text) };
        // SAFETY: `buf` is a handle from compiled code, checked non-null above and live for the whole call.
        unsafe { &mut *buf }.inner.extend_from_slice(s.as_bytes());
    });
}

/// Append one byte (low 8 bits of `byte`) to the buffer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bytes_buffer_push(buf: *mut GosBytesBuffer, byte: i64) {
    ffi_entry!((), {
        if buf.is_null() {
            return;
        }
        // SAFETY: `buf` is a handle from compiled code, checked non-null above and live for the whole call.
        unsafe { &mut *buf }.inner.push(byte as u8);
    });
}

/// Current byte length of the buffer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bytes_buffer_len(buf: *mut GosBytesBuffer) -> i64 {
    ffi_entry!(0, {
        if buf.is_null() {
            return 0;
        }
        // SAFETY: `buf` is a handle from compiled code, checked non-null above and live for the whole call.
        unsafe { &*buf }.inner.len() as i64
    })
}

/// `1` when the buffer is empty, else `0`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bytes_buffer_is_empty(buf: *mut GosBytesBuffer) -> i64 {
    ffi_entry!(1, {
        if buf.is_null() {
            return 1;
        }
        // SAFETY: `buf` is a handle from compiled code, checked non-null above and live for the whole call.
        i64::from(unsafe { &*buf }.inner.is_empty())
    })
}

/// Reset the buffer to empty without releasing capacity.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bytes_buffer_clear(buf: *mut GosBytesBuffer) {
    ffi_entry!((), {
        if buf.is_null() {
            return;
        }
        // SAFETY: `buf` is a handle from compiled code, checked non-null above and live for the whole call.
        unsafe { &mut *buf }.inner.clear();
    });
}

/// Lossy-UTF-8 view of the buffer contents as a fresh runtime
/// c-string. Invalid sequences become U+FFFD.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bytes_buffer_to_string(buf: *mut GosBytesBuffer) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if buf.is_null() {
            return alloc_cstring(b"");
        }
        // SAFETY: `buf` is a handle from compiled code, checked non-null above and live for the whole call.
        let owned = String::from_utf8_lossy(&unsafe { &*buf }.inner).into_owned();
        alloc_cstring(owned.as_bytes())
    })
}

// ---------------------------------------------------------------
// Stateless helpers - operate on a string's UTF-8 bytes
// ---------------------------------------------------------------

/// `bytes::index_of(haystack, needle)` - byte index of the first
/// occurrence as `Option<i64>` in the 2-word `i128` Result ABI.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bytes_index_of(
    haystack: *const crate::c_abi::vec::GosVec,
    needle: *const crate::c_abi::vec::GosVec,
) -> i128 {
    ffi_entry!(0i128, {
        // SAFETY: `haystack` is this shim's argument, null or a live `Vec` for the call (C-ABI
        // contract), which `vec_bytes` accepts.
        let h = unsafe { crate::c_abi::vec::vec_bytes(haystack) };
        // SAFETY: `needle` is this shim's argument, null or a live `Vec` for the call (C-ABI
        // contract), which `vec_bytes` accepts.
        let n = unsafe { crate::c_abi::vec::vec_bytes(needle) };
        match bytes_index_of(&h, &n) {
            Some(i) => gos_rt_result_new(0, i as i64),
            None => gos_rt_result_new(1, 0),
        }
    })
}

/// `bytes::split(haystack, sep)` - chunks as a `Vec<String>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bytes_split(
    haystack: *const crate::c_abi::vec::GosVec,
    sep: *const crate::c_abi::vec::GosVec,
) -> *mut crate::c_abi::vec::GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let v =
            { crate::c_abi::vec::gos_rt_vec_new_typed(8, crate::c_abi::vec::vec_elem_kind::VEC) };
        // SAFETY: `haystack` is this shim's argument, null or a live `Vec` for the call (C-ABI
        // contract), which `vec_bytes` accepts.
        let h = unsafe { crate::c_abi::vec::vec_bytes(haystack) };
        // SAFETY: `sep` is this shim's argument, null or a live `Vec` for the call (C-ABI
        // contract), which `vec_bytes` accepts.
        let s = unsafe { crate::c_abi::vec::vec_bytes(sep) };
        for chunk in bytes_split(&h, &s) {
            let chunk_i64 = crate::c_abi::encoding::bytes_to_gosvec(&chunk) as i64;
            // SAFETY: `v` is the fresh vec made above, or null, which `gos_rt_vec_push` accepts,
            // and `chunk_i64` is one 8-byte element.
            unsafe {
                crate::c_abi::vec::gos_rt_vec_push(v, std::ptr::addr_of!(chunk_i64).cast::<u8>());
            }
        }
        v
    })
}

/// `bytes::replace(haystack, from, to)` - every occurrence rewritten;
/// returns a fresh runtime c-string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bytes_replace(
    haystack: *const crate::c_abi::vec::GosVec,
    from: *const crate::c_abi::vec::GosVec,
    to: *const crate::c_abi::vec::GosVec,
) -> *mut crate::c_abi::vec::GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        // SAFETY: `haystack` is this shim's argument, null or a live `Vec` for the call (C-ABI
        // contract), which `vec_bytes` accepts.
        let h = unsafe { crate::c_abi::vec::vec_bytes(haystack) };
        // SAFETY: `from` is this shim's argument, null or a live `Vec` for the call (C-ABI
        // contract), which `vec_bytes` accepts.
        let f = unsafe { crate::c_abi::vec::vec_bytes(from) };
        // SAFETY: `to` is this shim's argument, null or a live `Vec` for the call (C-ABI
        // contract), which `vec_bytes` accepts.
        let t = unsafe { crate::c_abi::vec::vec_bytes(to) };
        crate::c_abi::encoding::bytes_to_gosvec(&bytes_replace(&h, &f, &t))
    })
}
