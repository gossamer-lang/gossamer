//! C-ABI shims for `std::http::cookie`, matching the bytecode-VM builtins
//! in `gossamer-interp/src/stdlib_builtins/http_security.rs` so every tier
//! answers the same bytes:
//!
//! - `cookie::parse_cookie_header(header) -> [(String, String)]`.
//! - `cookie::serialize(name, value) -> String`, the bare `name=value` with
//!   RFC 6265 sanitisation.

#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_sign_loss)]

use std::os::raw::c_char;

use super::string::alloc_cstring;
use super::vec::{
    GosVec, VecSlotChild, gos_rt_vec_push, gos_rt_vec_with_capacity, vec_elem_kind,
    vec_set_slot_children,
};

unsafe fn cstr_bytes<'a>(p: *const c_char) -> &'a [u8] {
    // SAFETY: this `unsafe fn`'s caller passes `p` live or null, which `gos_str_arg_bytes`
    // accepts.
    unsafe { crate::c_abi::gos_str_arg_bytes(p) }
}

unsafe fn cstr_str<'a>(p: *const c_char) -> &'a str {
    // SAFETY: this `unsafe fn`'s caller passes `p` null or live, which `cstr_bytes` accepts.
    std::str::from_utf8(unsafe { cstr_bytes(p) }).unwrap_or("")
}

// -- cookie --------------------------------------------------------------

/// Slot layout of the `[(String, String)]` vec returned by
/// [`gos_rt_http_cookie_parse_header`]: 16-byte `(name, value)` tuples
/// whose two words both own a fresh c-string unconditionally.
static COOKIE_PAIR_SLOT_CHILDREN: [VecSlotChild; 2] = [
    VecSlotChild {
        gate: -1,
        disc_word: 0,
        word: 0,
        kind: vec_elem_kind::STRING,
    },
    VecSlotChild {
        gate: -1,
        disc_word: 0,
        word: 1,
        kind: vec_elem_kind::STRING,
    },
];

// Strips a surrounding pair of `"` and unescapes `\\` / `\"` sequences.
// Mirrors `http_cookie::unquote`.
fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"' {
        let inner = &value[1..value.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut escaped = false;
        for ch in inner.chars() {
            if escaped {
                out.push(ch);
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else {
                out.push(ch);
            }
        }
        if escaped {
            out.push('\\');
        }
        out
    } else {
        value.to_string()
    }
}

// Mirrors `http_cookie::encode_value`.
fn encode_value(value: &str) -> String {
    let sanitized: String = value
        .bytes()
        .filter(|&b| (0x20..=0x7e).contains(&b) && b != b'"' && b != b';' && b != b'\\')
        .map(char::from)
        .collect();
    if sanitized.bytes().any(|b| b == b' ' || b == b',') {
        format!("\"{sanitized}\"")
    } else {
        sanitized
    }
}

// Mirrors `http_cookie::sanitize_cookie_name`.
fn sanitize_cookie_name(name: &str) -> String {
    name.chars()
        .filter(|&c| c != '\r' && c != '\n' && c != '\0')
        .collect()
}

/// `cookie::parse_cookie_header(header) -> [(String, String)]` - splits
/// a `Cookie:` request header into ordered `(name, value)` pairs.
/// Lenient: malformed pairs are skipped. Mirrors
/// `gossamer_std::http_cookie::parse_cookie_header`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_cookie_parse_header(header: *const c_char) -> *mut GosVec {
    ffi_entry!({
        // SAFETY: `header` is this shim's argument, null or a live string body for the call
        // (C-ABI contract), which `cstr_str` accepts.
        let header = unsafe { cstr_str(header) };
        #[repr(C)]
        struct Pair {
            name: i64,
            value: i64,
        }
        let v = gos_rt_vec_with_capacity(16, 0);
        for raw in header.split(';') {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Some(eq) = trimmed.find('=') else {
                continue;
            };
            let name = trimmed[..eq].trim();
            let value = trimmed[eq + 1..].trim();
            if name.is_empty() {
                continue;
            }
            let value = unquote(value);
            let entry = Pair {
                name: alloc_cstring(name.as_bytes()) as i64,
                value: alloc_cstring(value.as_bytes()) as i64,
            };
            // SAFETY: `v` is the fresh vec made above, or null, which `gos_rt_vec_push` accepts,
            // and `entry` is one 16-byte element.
            unsafe {
                gos_rt_vec_push(v, std::ptr::addr_of!(entry).cast::<u8>());
            }
        }
        // Tagged after the pushes - the vec owns the fresh strings.
        // SAFETY: `v` is the live vec built above.
        unsafe { vec_set_slot_children(v, &COOKIE_PAIR_SLOT_CHILDREN) };
        v
    })
}

/// `cookie::serialize(name, value) -> String` - renders a bare
/// `name=value` `Set-Cookie` value with RFC 6265 sanitisation (no
/// attributes). Mirrors `Cookie::new(name, value).to_header_value()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_cookie_serialize(
    name: *const c_char,
    value: *const c_char,
) -> *mut c_char {
    ffi_entry!({
        // SAFETY: `name` is this shim's argument, null or a live string body for the call (C-ABI
        // contract), which `cstr_str` accepts.
        let name = unsafe { cstr_str(name) };
        // SAFETY: `value` is this shim's argument, null or a live string body for the call (C-ABI
        // contract), which `cstr_str` accepts.
        let value = unsafe { cstr_str(value) };
        let mut out = String::with_capacity(name.len() + value.len() + 1);
        out.push_str(&sanitize_cookie_name(name));
        out.push('=');
        out.push_str(&encode_value(value));
        alloc_cstring(out.as_bytes())
    })
}
