//! C-ABI dispatch shims for the stateless string/crypto surface of
//! `std::http::cookie`, `std::http::csrf`, and `std::http::session`.
//! These mirror the bytecode-VM builtins in
//! `gossamer-interp/src/stdlib_builtins/http_security.rs` so the
//! compiled (Cranelift / LLVM) tier resolves the same calls natively
//! instead of failing to link.
//!
//! Only the request/response-free core is wired here - the parts that
//! are pure functions of their string / byte arguments:
//!
//! - `cookie::parse_cookie_header(header) -> [(String, String)]` and
//!   `cookie::serialize(name, value) -> String` (bare `name=value`
//!   render with RFC 6265 sanitisation).
//! - `csrf::issue_token(key) -> Result<String, _>` and
//!   `csrf::verify_token(cookie_token, supplied_token, key) ->
//!   Result<(), _>`. Wire shape:
//!   `base64url(nonce) . base64url(hmac_sha256(key, nonce))`.
//! - `session::sign(payload, key) -> String` and
//!   `session::verify(cookie, key) -> Result<String, _>` for the
//!   `SignedCookieStore` SignedOnly mode. Wire shape:
//!   `base64url(payload) . base64url(hmac_sha256(key, base64url(payload)))`.
//!
//! The HMAC-SHA256 construction, base64url alphabet, constant-time
//! compare, and cookie parse/encode rules are reimplemented inline to
//! match `gossamer-std/src/http_{cookie,csrf,session}.rs` and
//! `gossamer-std/src/crypto.rs` byte-for-byte (the runtime crate
//! cannot depend on `gossamer-std`). `gossamer_pkg::sha256::digest`
//! is the shared SHA-256 primitive, identical to the one
//! `crypto::hmac::sha256_mac` builds on.

#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_sign_loss)]

use std::os::raw::c_char;

use super::result::gos_rt_result_new;
use super::string::alloc_cstring;
use super::vec::{
    GosVec, VecSlotChild, gos_rt_vec_push, gos_rt_vec_with_capacity, vec_elem_kind,
    vec_set_slot_children,
};

// -- base64url (RFC 4648 §5, no padding) ---------------------------------

const B64URL_ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn b64url_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= input.len() {
        let n =
            (u32::from(input[i]) << 16) | (u32::from(input[i + 1]) << 8) | u32::from(input[i + 2]);
        out.push(B64URL_ALPHA[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64URL_ALPHA[((n >> 12) & 0x3f) as usize] as char);
        out.push(B64URL_ALPHA[((n >> 6) & 0x3f) as usize] as char);
        out.push(B64URL_ALPHA[(n & 0x3f) as usize] as char);
        i += 3;
    }
    let remaining = input.len() - i;
    if remaining == 1 {
        let n = u32::from(input[i]) << 16;
        out.push(B64URL_ALPHA[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64URL_ALPHA[((n >> 12) & 0x3f) as usize] as char);
    } else if remaining == 2 {
        let n = (u32::from(input[i]) << 16) | (u32::from(input[i + 1]) << 8);
        out.push(B64URL_ALPHA[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64URL_ALPHA[((n >> 12) & 0x3f) as usize] as char);
        out.push(B64URL_ALPHA[((n >> 6) & 0x3f) as usize] as char);
    }
    out
}

fn decode_b64url_char(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'-' => Some(62),
        b'_' => Some(63),
        _ => None,
    }
}

fn b64url_decode(input: &str) -> Option<Vec<u8>> {
    let trimmed = input.trim_end_matches('=');
    let bytes = trimmed.as_bytes();
    let n = bytes.len();
    let out_len = match n % 4 {
        0 => n / 4 * 3,
        2 => n / 4 * 3 + 1,
        3 => n / 4 * 3 + 2,
        _ => return None,
    };
    let mut out = Vec::with_capacity(out_len);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for &b in bytes {
        let v = decode_b64url_char(b)?;
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    if bits > 0 && (acc & ((1u32 << bits) - 1)) != 0 {
        return None;
    }
    Some(out)
}

// -- HMAC-SHA256 (RFC 2104) - mirrors crypto::hmac::sha256_mac ------------

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&gossamer_pkg::sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner_key = [0u8; BLOCK];
    let mut outer_key = [0u8; BLOCK];
    for i in 0..BLOCK {
        inner_key[i] = block[i] ^ 0x36;
        outer_key[i] = block[i] ^ 0x5c;
    }
    let mut inner_input = Vec::with_capacity(BLOCK + message.len());
    inner_input.extend_from_slice(&inner_key);
    inner_input.extend_from_slice(message);
    let inner_hash = gossamer_pkg::sha256::digest(&inner_input);
    let mut outer_input = Vec::with_capacity(BLOCK + 32);
    outer_input.extend_from_slice(&outer_key);
    outer_input.extend_from_slice(&inner_hash);
    gossamer_pkg::sha256::digest(&outer_input)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Packs an `Err(errors::Error)` for the Result-returning shims.
fn sec_err(msg: &str) -> i128 {
    let err = super::errors::error_new_from_bytes(msg.as_bytes());
    gos_rt_result_new(1, err as i64)
}

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

// -- csrf ----------------------------------------------------------------

const MIN_CSRF_KEY_BYTES: usize = 32;

fn csrf_key_is_strong_enough(key: &[u8]) -> bool {
    key.len() >= MIN_CSRF_KEY_BYTES
}

fn split_token(token: &str) -> Option<(&str, &str)> {
    let (a, b) = token.split_once('.')?;
    if a.is_empty() || b.is_empty() {
        return None;
    }
    Some((a, b))
}

/// `csrf::issue_token(key) -> Result<String, errors::Error>` - a fresh
/// signed double-submit token `base64url(nonce).base64url(hmac(key, nonce))`.
/// Mirrors `gossamer_std::http_csrf::issue_token`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_csrf_issue_token(key: *const GosVec) -> i128 {
    ffi_entry!({
        // SAFETY: `key` is this shim's argument, live for the call (C-ABI contract) or null,
        // which `vec_bytes` accepts.
        let key = unsafe { crate::c_abi::vec::vec_bytes(key) };
        if !csrf_key_is_strong_enough(&key) {
            return sec_err("csrf: key must be at least 32 bytes");
        }
        let mut nonce = [0u8; 32];
        if getrandom::fill(&mut nonce).is_err() {
            return sec_err("csrf: rng failure");
        }
        let mac = hmac_sha256(&key, &nonce);
        let token = format!("{}.{}", b64url_encode(&nonce), b64url_encode(&mac));
        gos_rt_result_new(0, alloc_cstring(token.as_bytes()) as i64)
    })
}

/// `csrf::verify_token(cookie_token, supplied_token, key) ->
/// Result<(), errors::Error>`. `Ok(())` when both presentations share
/// the same nonce and the HMAC over that nonce verifies under `key`.
/// Mirrors `gossamer_std::http_csrf::verify_token`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_csrf_verify_token(
    cookie_token: *const c_char,
    supplied_token: *const c_char,
    key: *const GosVec,
) -> i128 {
    ffi_entry!({
        // SAFETY: `cookie_token` is this shim's argument, null or a live string body for the call
        // (C-ABI contract), which `cstr_str` accepts.
        let cookie_token = unsafe { cstr_str(cookie_token) };
        // SAFETY: `supplied_token` is this shim's argument, null or a live string body for the
        // call (C-ABI contract), which `cstr_str` accepts.
        let supplied_token = unsafe { cstr_str(supplied_token) };
        // SAFETY: `key` is this shim's argument, live for the call (C-ABI contract) or null,
        // which `vec_bytes` accepts.
        let key = unsafe { crate::c_abi::vec::vec_bytes(key) };
        if !csrf_key_is_strong_enough(&key) {
            return sec_err("csrf: key must be at least 32 bytes");
        }

        let Some((cookie_nonce_b64, cookie_sig_b64)) = split_token(cookie_token) else {
            return sec_err("csrf: token missing separator");
        };
        let Some((header_nonce_b64, _)) = split_token(supplied_token) else {
            return sec_err("csrf: token missing separator");
        };
        if !constant_time_eq(cookie_nonce_b64.as_bytes(), header_nonce_b64.as_bytes()) {
            return sec_err("csrf: token mismatch");
        }
        let Some(nonce) = b64url_decode(cookie_nonce_b64) else {
            return sec_err("csrf: cookie nonce decode");
        };
        let Some(sig) = b64url_decode(cookie_sig_b64) else {
            return sec_err("csrf: cookie signature decode");
        };
        let expected = hmac_sha256(&key, &nonce);
        if !constant_time_eq(&sig, &expected) {
            return sec_err("csrf: signature mismatch");
        }
        gos_rt_result_new(0, 0)
    })
}

// -- session (SignedCookieStore, SignedOnly mode) ------------------------

/// `session::sign(payload, key) -> String` - frames a JSON payload as a
/// signed cookie value `base64url(payload).base64url(hmac(key,
/// base64url(payload)))`. Mirrors `SignedCookieStore::encode` for
/// `SerializationMode::SignedOnly`. The signature covers the
/// base64url-encoded payload string, exactly as the store does.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_session_sign(
    payload: *const c_char,
    key: *const GosVec,
) -> *mut c_char {
    ffi_entry!({
        // SAFETY: `payload` is this shim's argument, null or a live string body for the call
        // (C-ABI contract), which `cstr_bytes` accepts.
        let payload = unsafe { cstr_bytes(payload) };
        // SAFETY: `key` is this shim's argument, live for the call (C-ABI contract) or null,
        // which `vec_bytes` accepts.
        let key = unsafe { crate::c_abi::vec::vec_bytes(key) };
        let b64_payload = b64url_encode(payload);
        let mac = hmac_sha256(&key, b64_payload.as_bytes());
        let wire = format!("{b64_payload}.{}", b64url_encode(&mac));
        alloc_cstring(wire.as_bytes())
    })
}

/// `session::verify(cookie, key) -> Result<String, errors::Error>` -
/// validates the HMAC and returns the decoded JSON payload text.
/// Mirrors `SignedCookieStore::decode` for `SignedOnly`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_session_verify(
    cookie: *const c_char,
    key: *const GosVec,
) -> i128 {
    ffi_entry!({
        // SAFETY: `cookie` is this shim's argument, null or a live string body for the call
        // (C-ABI contract), which `cstr_str` accepts.
        let cookie = unsafe { cstr_str(cookie) };
        // SAFETY: `key` is this shim's argument, live for the call (C-ABI contract) or null,
        // which `vec_bytes` accepts.
        let key = unsafe { crate::c_abi::vec::vec_bytes(key) };
        let Some((left, right)) = cookie.split_once('.') else {
            return sec_err("session: missing separator");
        };
        let Some(sig) = b64url_decode(right) else {
            return sec_err("session: signature decode");
        };
        let mac = hmac_sha256(&key, left.as_bytes());
        if !constant_time_eq(&mac, &sig) {
            return sec_err("session: bad signature");
        }
        let Some(payload) = b64url_decode(left) else {
            return sec_err("session: payload decode");
        };
        match String::from_utf8(payload) {
            Ok(s) => gos_rt_result_new(0, alloc_cstring(s.as_bytes()) as i64),
            Err(_) => sec_err("session: payload not utf-8"),
        }
    })
}
