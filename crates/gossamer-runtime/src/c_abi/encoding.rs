#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::doc_markdown)]

//! C-ABI shims for `std::encoding::*` free functions so the compiled
//! tier lowers them instead of emitting an undefined `@encoding::*`
//! reference. The pure codec logic is small enough to live here
//! directly - `gossamer-runtime` cannot depend on `gossamer-std`
//! (that would be a dependency cycle), so the bytes mirror
//! `gossamer_std::encoding::{xml,base32}` exactly.

use std::os::raw::c_char;

use super::string::alloc_cstring;

fn err_result(msg: &str) -> i128 {
    let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
    unsafe { super::vec::gos_rt_result_new(1, err as i64) }
}

fn cstr_to_str<'a>(s: *const c_char) -> &'a str {
    // SAFETY: callers pass a Gossamer `String`, read through its length
    // header so interior NUL bytes survive; non-UTF-8 falls back to empty.
    unsafe { crate::c_abi::gos_str_arg_text(s) }
}

/// `encoding::xml::escape(s)` - replaces XML metacharacters with their
/// entity forms. Mirrors `gossamer_std::encoding::xml::escape`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_encoding_xml_escape(s: *const c_char) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let input = cstr_to_str(s);
        let mut out = String::with_capacity(input.len());
        for ch in input.chars() {
            match ch {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' => out.push_str("&quot;"),
                '\'' => out.push_str("&apos;"),
                c => out.push(c),
            }
        }
        alloc_cstring(out.as_bytes())
    })
}

/// Reads a `*mut GosVec` of `u8` into an owned `Vec<u8>`. Canonical byte
/// vectors use packed one-byte elements. The word-stride branch accepts legacy
/// or foreign ABI values at runtime boundaries.
pub(crate) unsafe fn gosvec_u8(v: *const super::vec::GosVec) -> Vec<u8> {
    if v.is_null() {
        return Vec::new();
    }
    let vref = unsafe { &*v };
    if vref.ptr.is_null() || vref.len <= 0 {
        return Vec::new();
    }
    let len = vref.len as usize;
    if vref.elem_bytes == 1 {
        return unsafe { std::slice::from_raw_parts(vref.ptr.as_ptr(), len) }.to_vec();
    }
    let words = unsafe { std::slice::from_raw_parts(vref.ptr.as_ptr().cast::<i64>(), len) };
    words.iter().map(|&w| w as u8).collect()
}

/// Builds a canonical packed Gossamer `Vec<u8>` from raw bytes.
pub(crate) fn bytes_to_gosvec(bytes: &[u8]) -> *mut super::vec::GosVec {
    let v = unsafe { super::vec::gos_rt_vec_with_capacity(1, bytes.len() as i64) };
    if !bytes.is_empty() {
        let vref = unsafe { &mut *v };
        if !vref.ptr.is_null() {
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), vref.ptr.as_ptr(), bytes.len());
            }
            vref.len = bytes.len() as i64;
        }
    }
    v
}

/// `encoding::hex::encode(data)` - lowercase hex of a byte vector.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_encoding_hex_encode(
    data: *const super::vec::GosVec,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let bytes = unsafe { gosvec_u8(data) };
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in &bytes {
            out.push(char::from_digit(u32::from(b >> 4), 16).unwrap_or('0'));
            out.push(char::from_digit(u32::from(b & 0x0f), 16).unwrap_or('0'));
        }
        alloc_cstring(out.as_bytes())
    })
}

/// `encoding::base32::encode(data)` - RFC 4648 of a byte vector.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_encoding_base32_encode(
    data: *const super::vec::GosVec,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let bytes = unsafe { gosvec_u8(data) };
        alloc_cstring(base32_encode(&bytes).as_bytes())
    })
}

/// `encoding::base32::encode_hex(data)` - hex (extended) alphabet
/// Base32 (0-9 A-V) of a byte vector. Companion to `decode_hex`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_encoding_base32_encode_hex(
    data: *const super::vec::GosVec,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let bytes = unsafe { gosvec_u8(data) };
        alloc_cstring(base32_encode_with(&bytes, BASE32_HEX_ALPHA).as_bytes())
    })
}

/// `html::escape(s)` - HTML entity-escapes `& < > " '`. Mirrors
/// `gossamer_std::html::escape`, which uses `&#39;` for the apostrophe
/// (XML uses `&apos;`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_html_escape(s: *const c_char) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let input = cstr_to_str(s);
        let mut out = String::with_capacity(input.len());
        for ch in input.chars() {
            match ch {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' => out.push_str("&quot;"),
                '\'' => out.push_str("&#39;"),
                '/' => out.push_str("&#x2F;"),
                '`' => out.push_str("&#x60;"),
                c => out.push(c),
            }
        }
        alloc_cstring(out.as_bytes())
    })
}

/// `encoding::base64::encode(data)` - RFC 4648 standard alphabet
/// with padding. Mirrors `gossamer_std::encoding::base64::encode`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_encoding_base64_encode(
    data: *const super::vec::GosVec,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let bytes = unsafe { gosvec_u8(data) };
        alloc_cstring(base64_encode(&bytes).as_bytes())
    })
}

/// `encoding::base64::decode(s)` - returns `Result<Vec<u8>,
/// errors::Error>`. Ok payload is a `*mut GosVec` of bytes
/// (packed bytes); Err payload is a gos error handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_encoding_base64_decode(s: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        let text = unsafe { crate::c_abi::gos_str_arg_bytes(s) };
        // Decoding straight into the answer's own storage is what keeps the
        // result one allocation: an intermediate `Vec` would be built and
        // then copied whole into it.
        let v = unsafe {
            super::vec::gos_rt_vec_with_capacity(1, base64_decoded_bound(text.len()) as i64)
        };
        let vref = unsafe { &mut *v };
        if vref.ptr.is_null() {
            return unsafe { super::vec::gos_rt_result_new(0, v as i64) };
        }
        // SAFETY: the vec was just built with room for the decoded bound, and
        // its buffer is live and uniquely held here.
        let out = unsafe {
            std::slice::from_raw_parts_mut(vref.ptr.as_ptr(), base64_decoded_bound(text.len()))
        };
        match base64_decode_into(text, out) {
            Ok(written) => {
                vref.len = written as i64;
                unsafe { super::vec::gos_rt_result_new(0, v as i64) }
            }
            Err(e) => {
                unsafe { crate::c_abi::map::gos_rt_vec_free(v) };
                err_result(&e)
            }
        }
    })
}

/// `encoding::hex::decode(s)` - returns `Result<Vec<u8>,
/// errors::Error>` (Ok payload is a byte `Vec`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_encoding_hex_decode(s: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        match hex_decode(cstr_to_str(s)) {
            Ok(bytes) => {
                let v = bytes_to_gosvec(&bytes);
                unsafe { super::vec::gos_rt_result_new(0, v as i64) }
            }
            Err(e) => err_result(&e),
        }
    })
}

/// `html::unescape(s)` - inverse of `gos_rt_html_escape`. Decodes the
/// named entities Gossamer emits (`&amp; &lt; &gt; &quot; &#39;
/// &apos;`) plus decimal / hex numeric character references.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_html_unescape(s: *const c_char) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let input = cstr_to_str(s);
        alloc_cstring(html_unescape(input).as_bytes())
    })
}

/// `html::template::render_json(source, json_data)` - renders the
/// context-aware HTML template `source` against a JSON-encoded data
/// context, returning `Result<String, errors::Error>`. The `GosResult`
/// disc is 0 for `Ok` (the rendered text), 1 for `Err` (the parse /
/// data error message). Calls the leaf `gossamer-template` engine
/// directly, the same code the VM tier reaches through
/// `gossamer_std::html::template`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_html_template_render_json(
    source: *const c_char,
    json_data: *const c_char,
) -> i128 {
    ffi_entry!(0i128, {
        let source = cstr_to_str(source);
        let json_data = cstr_to_str(json_data);
        match gossamer_template::html::render_json(source, json_data) {
            Ok(text) => {
                let p = alloc_cstring(text.as_bytes());
                unsafe { super::vec::gos_rt_result_new(0, p as i64) }
            }
            Err(e) => err_result(&format!("{e}")),
        }
    })
}

const BASE64_ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Sextet value per input byte; `INVALID` for anything outside the alphabet.
/// A table read replaces the range tests the decoder used to run per
/// character.
const BASE64_INVALID: u8 = 0xFF;
static BASE64_VALUES: [u8; 256] = {
    let mut table = [BASE64_INVALID; 256];
    let mut i = 0;
    while i < 64 {
        table[BASE64_ALPHA[i] as usize] = i as u8;
        i += 1;
    }
    table
};

pub(crate) fn base64_encode(data: &[u8]) -> String {
    // The output is pure ASCII from the alphabet, so it is built as bytes at
    // its final length: a `String::push(char)` per character re-encodes each
    // one as UTF-8 and re-checks the capacity four times per three input
    // bytes.
    let mut out = vec![0u8; data.len().div_ceil(3) * 4];
    let mut written = 0usize;
    let mut chunks = data.chunks_exact(3);
    for chunk in &mut chunks {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8) | u32::from(chunk[2]);
        out[written] = BASE64_ALPHA[((n >> 18) & 0x3f) as usize];
        out[written + 1] = BASE64_ALPHA[((n >> 12) & 0x3f) as usize];
        out[written + 2] = BASE64_ALPHA[((n >> 6) & 0x3f) as usize];
        out[written + 3] = BASE64_ALPHA[(n & 0x3f) as usize];
        written += 4;
    }
    let tail = chunks.remainder();
    if !tail.is_empty() {
        let b1 = tail.get(1).copied().unwrap_or(0);
        let n = (u32::from(tail[0]) << 16) | (u32::from(b1) << 8);
        out[written] = BASE64_ALPHA[((n >> 18) & 0x3f) as usize];
        out[written + 1] = BASE64_ALPHA[((n >> 12) & 0x3f) as usize];
        out[written + 2] = if tail.len() > 1 {
            BASE64_ALPHA[((n >> 6) & 0x3f) as usize]
        } else {
            b'='
        };
        out[written + 3] = b'=';
    }
    // SAFETY: every byte written comes from the base64 alphabet or `=`, all
    // of which are ASCII.
    unsafe { String::from_utf8_unchecked(out) }
}

pub(crate) fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    base64_decode_bytes(s.as_bytes())
}

/// The decoded bytes of base64 text.
///
/// The alphabet, the padding and the whitespace it skips are all ASCII, so
/// the encoding of the surrounding text decides nothing here: reading the
/// bytes answers the same result without validating them as UTF-8 first.
pub(crate) fn base64_decode_bytes(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = vec![0u8; base64_decoded_bound(bytes.len())];
    let written = base64_decode_into(bytes, &mut out)?;
    out.truncate(written);
    Ok(out)
}

/// Bytes `base64_decode_into` may write for an input of this length.
///
/// Every character carries six bits, so the output is at most three quarters
/// of the input; the spare rounds the last partial group up.
pub(crate) const fn base64_decoded_bound(input_len: usize) -> usize {
    input_len / 4 * 3 + 3
}

/// Decodes base64 `bytes` into `out`, answering how many bytes it wrote.
///
/// Four characters carry exactly three bytes, so a group whose characters are
/// all in the alphabet is one 24-bit assemble and three stores. A group
/// holding padding or whitespace - and the tail - goes through the
/// character-at-a-time accumulator beside it, which is also what keeps the
/// group path entered only on a byte boundary.
///
/// # Panics
/// If `out` is shorter than [`base64_decoded_bound`] of the input length.
pub(crate) fn base64_decode_into(bytes: &[u8], out: &mut [u8]) -> Result<usize, String> {
    assert!(
        out.len() >= base64_decoded_bound(bytes.len()),
        "base64 output buffer is shorter than the decoded bound"
    );
    let mut bits: u32 = 0;
    let mut nbits = 0u32;
    let mut i = 0usize;
    let mut written = 0usize;
    while i < bytes.len() {
        if nbits == 0
            && i + 4 <= bytes.len()
            && let a = u32::from(BASE64_VALUES[bytes[i] as usize])
            && let b = u32::from(BASE64_VALUES[bytes[i + 1] as usize])
            && let c = u32::from(BASE64_VALUES[bytes[i + 2] as usize])
            && let d = u32::from(BASE64_VALUES[bytes[i + 3] as usize])
            // Every alphabet value is below 64 and the refusal marker is
            // 0xFF, so one compare of the union rejects the whole group.
            && (a | b | c | d) < 64
        {
            let n = (a << 18) | (b << 12) | (c << 6) | d;
            out[written] = (n >> 16) as u8;
            out[written + 1] = (n >> 8) as u8;
            out[written + 2] = n as u8;
            written += 3;
            i += 4;
            continue;
        }
        let ch = bytes[i];
        i += 1;
        let val = BASE64_VALUES[ch as usize];
        if val == BASE64_INVALID {
            if ch == b'=' || ch.is_ascii_whitespace() {
                continue;
            }
            return Err(format!("base64: invalid character '{}'", ch as char));
        }
        bits = (bits << 6) | u32::from(val);
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out[written] = (bits >> nbits) as u8;
            written += 1;
        }
    }
    Ok(written)
}

fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    let trimmed: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if !trimmed.len().is_multiple_of(2) {
        return Err("hex: odd number of digits".to_string());
    }
    let nibble = |c: u8| -> Result<u8, String> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            b'A'..=b'F' => Ok(c - b'A' + 10),
            _ => Err(format!("hex: invalid character '{}'", c as char)),
        }
    };
    let mut out = Vec::with_capacity(trimmed.len() / 2);
    for pair in trimmed.chunks(2) {
        out.push((nibble(pair[0])? << 4) | nibble(pair[1])?);
    }
    Ok(out)
}

fn html_unescape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'&' {
            // Copy the full UTF-8 char starting at i.
            let ch = input[i..].chars().next().unwrap_or('\u{fffd}');
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        if let Some(semi) = input[i..].find(';') {
            let entity = &input[i + 1..i + semi];
            let decoded = match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" | "#39" => Some('\''),
                _ => {
                    if let Some(hex) = entity
                        .strip_prefix("#x")
                        .or_else(|| entity.strip_prefix("#X"))
                    {
                        u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
                    } else if let Some(dec) = entity.strip_prefix('#') {
                        dec.parse::<u32>().ok().and_then(char::from_u32)
                    } else {
                        None
                    }
                }
            };
            if let Some(c) = decoded {
                out.push(c);
                i += semi + 1;
                continue;
            }
        }
        out.push('&');
        i += 1;
    }
    out
}

const BASE32_ALPHA: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

fn base32_encode(data: &[u8]) -> String {
    base32_encode_with(data, BASE32_ALPHA)
}

fn base32_encode_with(data: &[u8], alpha: &[u8; 32]) -> String {
    let mut out = String::new();
    for chunk in data.chunks(5) {
        let mut buf = [0u8; 5];
        buf[..chunk.len()].copy_from_slice(chunk);
        let n = u64::from(buf[0]) << 32
            | u64::from(buf[1]) << 24
            | u64::from(buf[2]) << 16
            | u64::from(buf[3]) << 8
            | u64::from(buf[4]);
        let chars = [
            (n >> 35) & 0x1f,
            (n >> 30) & 0x1f,
            (n >> 25) & 0x1f,
            (n >> 20) & 0x1f,
            (n >> 15) & 0x1f,
            (n >> 10) & 0x1f,
            (n >> 5) & 0x1f,
            n & 0x1f,
        ];
        // Number of output chars depends on the chunk byte count.
        let keep = match chunk.len() {
            1 => 2,
            2 => 4,
            3 => 5,
            4 => 7,
            _ => 8,
        };
        for (i, c) in chars.iter().enumerate() {
            if i < keep {
                out.push(alpha[*c as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

const BASE32_HEX_ALPHA: &[u8; 32] = b"0123456789ABCDEFGHIJKLMNOPQRSTUV";

fn base32_decode_inner(s: &str, alpha: &[u8; 32]) -> Result<Vec<u8>, String> {
    let mut bits: u64 = 0;
    let mut nbits = 0u32;
    let mut out = Vec::new();
    for ch in s.chars() {
        if ch == '=' {
            break;
        }
        let up = ch.to_ascii_uppercase();
        let val = alpha
            .iter()
            .position(|&a| a == up as u8)
            .ok_or_else(|| format!("base32: invalid character '{ch}'"))?;
        bits = (bits << 5) | val as u64;
        nbits += 5;
        if nbits >= 8 {
            nbits -= 8;
            out.push((bits >> nbits) as u8);
        }
    }
    Ok(out)
}

fn base32_decode(s: &str) -> Result<Vec<u8>, String> {
    base32_decode_inner(s, BASE32_ALPHA)
}

fn base32_decode_hex(s: &str) -> Result<Vec<u8>, String> {
    base32_decode_inner(s, BASE32_HEX_ALPHA)
}

/// `encoding::base32::encode_string(s)` - RFC 4648 standard alphabet.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_encoding_base32_encode_string(s: *const c_char) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let encoded = base32_encode(cstr_to_str(s).as_bytes());
        alloc_cstring(encoded.as_bytes())
    })
}

/// `encoding::base32::decode_string(s)` - returns `Result<String,
/// errors::Error>`. The `GosResult` disc is 0 for `Ok`, 1 for `Err`;
/// the payload is a c-string pointer (decoded text or error message).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_encoding_base32_decode_string(s: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        match base32_decode(cstr_to_str(s)) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => {
                    let p = alloc_cstring(text.as_bytes());
                    unsafe { super::vec::gos_rt_result_new(0, p as i64) }
                }
                Err(e) => err_result(&format!("base32: {e}")),
            },
            Err(e) => err_result(&e),
        }
    })
}

/// `encoding::base32::decode(s)` - standard RFC 4648 alphabet,
/// returns `Result<Vec<u8>, errors::Error>` (Ok payload a byte `Vec`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_encoding_base32_decode(s: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        match base32_decode(cstr_to_str(s)) {
            Ok(bytes) => {
                let v = bytes_to_gosvec(&bytes);
                unsafe { super::vec::gos_rt_result_new(0, v as i64) }
            }
            Err(e) => err_result(&e),
        }
    })
}

/// `encoding::base32::decode_hex(s)` - hex (extended) RFC 4648
/// alphabet (0-9 A-V), returns `Result<Vec<u8>, errors::Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_encoding_base32_decode_hex(s: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        match base32_decode_hex(cstr_to_str(s)) {
            Ok(bytes) => {
                let v = bytes_to_gosvec(&bytes);
                unsafe { super::vec::gos_rt_result_new(0, v as i64) }
            }
            Err(e) => err_result(&e),
        }
    })
}

// ---------------------------------------------------------------
// encoding::ascii85 (Adobe ASCII85 / btoa) - mirrors
// gossamer_std::encoding::ascii85 exactly.
// ---------------------------------------------------------------

fn ascii85_encode(data: &[u8]) -> String {
    let mut out = String::from("<~");
    let mut i = 0;
    while i < data.len() {
        let remaining = data.len() - i;
        let chunk = &data[i..i + remaining.min(4)];
        let mut group = [0u8; 4];
        group[..chunk.len()].copy_from_slice(chunk);
        let val = u32::from_be_bytes(group);
        if chunk.len() == 4 && val == 0 {
            out.push('z');
        } else {
            let mut digits = [0u8; 5];
            let mut v = val;
            for d in digits.iter_mut().rev() {
                *d = (v % 85) as u8 + b'!';
                v /= 85;
            }
            for &d in &digits[..=chunk.len()] {
                out.push(char::from(d));
            }
        }
        i += chunk.len();
    }
    out.push_str("~>");
    out
}

fn ascii85_decode(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim();
    let s = s.strip_prefix("<~").unwrap_or(s);
    let s = s.strip_suffix("~>").unwrap_or(s);
    let mut out = Vec::new();
    let mut group = [0u8; 5];
    let mut count = 0usize;
    for ch in s.chars() {
        if ch.is_ascii_whitespace() {
            continue;
        }
        if ch == 'z' {
            if count != 0 {
                return Err("ascii85: z inside group".to_string());
            }
            out.extend_from_slice(&[0u8; 4]);
            continue;
        }
        if !(b'!'..=b'u').contains(&(ch as u8)) {
            return Err(format!("ascii85: invalid character '{ch}'"));
        }
        group[count] = ch as u8 - b'!';
        count += 1;
        if count == 5 {
            let val: u32 = u32::from(group[0]) * 52_200_625
                + u32::from(group[1]) * 614_125
                + u32::from(group[2]) * 7_225
                + u32::from(group[3]) * 85
                + u32::from(group[4]);
            out.extend_from_slice(&val.to_be_bytes());
            count = 0;
        }
    }
    if count > 0 {
        if count == 1 {
            return Err("ascii85: trailing single digit".to_string());
        }
        let padding = 5 - count;
        for slot in group.iter_mut().skip(count) {
            *slot = b'u' - b'!';
        }
        let val: u32 = u32::from(group[0]) * 52_200_625
            + u32::from(group[1]) * 614_125
            + u32::from(group[2]) * 7_225
            + u32::from(group[3]) * 85
            + u32::from(group[4]);
        let bytes = val.to_be_bytes();
        out.extend_from_slice(&bytes[..4 - padding]);
    }
    Ok(out)
}

/// `encoding::ascii85::encode(data) -> String`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_encoding_ascii85_encode(
    data: *const super::vec::GosVec,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let bytes = unsafe { gosvec_u8(data) };
        alloc_cstring(ascii85_encode(&bytes).as_bytes())
    })
}

/// `encoding::ascii85::decode(s) -> Result<Vec<u8>, errors::Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_encoding_ascii85_decode(s: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        match ascii85_decode(cstr_to_str(s)) {
            Ok(bytes) => {
                let v = bytes_to_gosvec(&bytes);
                unsafe { super::vec::gos_rt_result_new(0, v as i64) }
            }
            Err(e) => err_result(&e),
        }
    })
}

// ---------------------------------------------------------------
// encoding::binary - fixed-width put/get + varint decode.
// The `put_*` shims mirror the VM builtin shape: the first
// argument (a buffer) is ignored; a fresh byte vector is returned.
// ---------------------------------------------------------------

macro_rules! put_fixed {
    ($name:ident, $ty:ty, $to:ident, $n:expr) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(
            _buf: *const super::vec::GosVec,
            v: i64,
        ) -> *mut super::vec::GosVec {
            ffi_entry!(std::ptr::null_mut(), {
                let bytes = (v as $ty).$to();
                bytes_to_gosvec(&bytes[..$n])
            })
        }
    };
}

put_fixed!(gos_rt_bin_put_u8, u8, to_be_bytes, 1);
put_fixed!(gos_rt_bin_put_u16_be, u16, to_be_bytes, 2);
put_fixed!(gos_rt_bin_put_u16_le, u16, to_le_bytes, 2);
put_fixed!(gos_rt_bin_put_u32_be, u32, to_be_bytes, 4);
put_fixed!(gos_rt_bin_put_u32_le, u32, to_le_bytes, 4);
put_fixed!(gos_rt_bin_put_u64_be, u64, to_be_bytes, 8);
put_fixed!(gos_rt_bin_put_u64_le, u64, to_le_bytes, 8);

macro_rules! get_fixed {
    ($name:ident, $ty:ty, $from:ident, $n:expr) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(data: *const super::vec::GosVec) -> i128 {
            ffi_entry!(0i128, {
                let bytes = unsafe { gosvec_u8(data) };
                if bytes.len() < $n {
                    return err_result("binary: buffer too short");
                }
                let mut arr = [0u8; $n];
                arr.copy_from_slice(&bytes[..$n]);
                let v = <$ty>::$from(arr);
                unsafe { super::vec::gos_rt_result_new(0, i64::from(v)) }
            })
        }
    };
}

get_fixed!(gos_rt_bin_get_u8, u8, from_be_bytes, 1);
get_fixed!(gos_rt_bin_get_u16_be, u16, from_be_bytes, 2);
get_fixed!(gos_rt_bin_get_u16_le, u16, from_le_bytes, 2);
get_fixed!(gos_rt_bin_get_u32_be, u32, from_be_bytes, 4);
get_fixed!(gos_rt_bin_get_u32_le, u32, from_le_bytes, 4);

// u64 needs the value reinterpreted (i64::from would reject u64).
macro_rules! get_u64 {
    ($name:ident, $from:ident) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(data: *const super::vec::GosVec) -> i128 {
            ffi_entry!(0i128, {
                let bytes = unsafe { gosvec_u8(data) };
                if bytes.len() < 8 {
                    return err_result("binary: buffer too short");
                }
                let mut arr = [0u8; 8];
                arr.copy_from_slice(&bytes[..8]);
                let v = u64::$from(arr);
                unsafe { super::vec::gos_rt_result_new(0, v as i64) }
            })
        }
    };
}

get_u64!(gos_rt_bin_get_u64_be, from_be_bytes);
get_u64!(gos_rt_bin_get_u64_le, from_le_bytes);

// ---------------------------------------------------------------
// encoding::binary - offset-taking in-place accessors.
// A pager reads a u16 at offset 5 of a 4 KiB page and writes a u32
// at offset 8, so the `_at` family reads and writes through the
// caller's own buffer instead of returning a fresh one. An offset
// plus width that runs past the end is an `Err`, never a zero-fill.
// ---------------------------------------------------------------

/// Bytes `[offset, offset + width)` of `data`, or the diagnostic when
/// that window is not entirely inside the buffer.
unsafe fn read_window(
    data: *const super::vec::GosVec,
    offset: i64,
    out: &mut [u8],
) -> Result<(), i128> {
    if offset < 0 {
        return Err(err_result("binary: offset must be non-negative"));
    }
    if data.is_null() {
        return Err(err_result("binary: read past the end of the buffer"));
    }
    // The window is read straight out of the caller's buffer: materialising
    // the whole buffer to take a few bytes from it would make every fixed
    // width read cost the buffer's length.
    let vref = unsafe { &*data };
    let len = if vref.ptr.is_null() || vref.len <= 0 {
        0
    } else {
        vref.len as usize
    };
    let start = offset as usize;
    let end = start
        .checked_add(out.len())
        .ok_or_else(|| err_result("binary: offset overflows the buffer"))?;
    if end > len {
        return Err(err_result("binary: read past the end of the buffer"));
    }
    if vref.elem_bytes == 1 {
        // SAFETY: `start + out.len() <= len`, so the window is inside the
        // buffer's single-byte elements.
        let bytes = unsafe { std::slice::from_raw_parts(vref.ptr.as_ptr().add(start), out.len()) };
        out.copy_from_slice(bytes);
    } else {
        // SAFETY: same bound over the buffer's machine-word elements.
        let words = unsafe {
            std::slice::from_raw_parts(vref.ptr.as_ptr().cast::<i64>().add(start), out.len())
        };
        for (slot, &word) in out.iter_mut().zip(words) {
            *slot = word as u8;
        }
    }
    Ok(())
}

/// Writes `bytes` into `[offset, offset + bytes.len())` of a GosVec whose
/// elements are single bytes, or answers the diagnostic when that window
/// is not entirely inside the buffer.
unsafe fn write_window(
    data: *mut super::vec::GosVec,
    offset: i64,
    bytes: &[u8],
) -> Result<(), i128> {
    if offset < 0 {
        return Err(err_result("binary: offset must be non-negative"));
    }
    if data.is_null() {
        return Err(err_result("binary: null buffer"));
    }
    let header = unsafe { &*data };
    if header.elem_bytes != 1 || header.ptr.is_null() {
        return Err(err_result("binary: buffer is not a byte sequence"));
    }
    let len = header.len.max(0) as usize;
    let start = offset as usize;
    let end = start
        .checked_add(bytes.len())
        .ok_or_else(|| err_result("binary: offset overflows the buffer"))?;
    if end > len {
        return Err(err_result("binary: write past the end of the buffer"));
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), header.ptr.as_ptr().add(start), bytes.len());
    }
    Ok(())
}

/// `binary::get_<width>_<endian>_at(bytes, offset) -> Result<T, Error>`:
/// reads the fixed-width integer at a byte offset of the caller's buffer.
macro_rules! get_fixed_at {
    ($name:ident, $ty:ty, $from:ident, $n:expr) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(data: *const super::vec::GosVec, offset: i64) -> i128 {
            ffi_entry!(0i128, {
                let mut arr = [0u8; $n];
                if let Err(packed) = unsafe { read_window(data, offset, &mut arr) } {
                    return packed;
                }
                unsafe { super::vec::gos_rt_result_new(0, <$ty>::$from(arr) as i64) }
            })
        }
    };
}

get_fixed_at!(gos_rt_bin_get_u16_be_at, u16, from_be_bytes, 2);
get_fixed_at!(gos_rt_bin_get_u16_le_at, u16, from_le_bytes, 2);
get_fixed_at!(gos_rt_bin_get_u32_be_at, u32, from_be_bytes, 4);
get_fixed_at!(gos_rt_bin_get_u32_le_at, u32, from_le_bytes, 4);
get_fixed_at!(gos_rt_bin_get_u64_be_at, u64, from_be_bytes, 8);
get_fixed_at!(gos_rt_bin_get_u64_le_at, u64, from_le_bytes, 8);

/// `binary::put_<width>_<endian>_at(buf, offset, value) -> Result<(), Error>`:
/// writes the fixed-width integer in place at a byte offset of the caller's
/// buffer.
macro_rules! put_fixed_at {
    ($name:ident, $ty:ty, $to:ident) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(
            buf: *mut super::vec::GosVec,
            offset: i64,
            value: i64,
        ) -> i128 {
            ffi_entry!(0i128, {
                let bytes = (value as $ty).$to();
                match unsafe { write_window(buf, offset, &bytes) } {
                    Ok(()) => unsafe { super::vec::gos_rt_result_new(0, 0) },
                    Err(packed) => packed,
                }
            })
        }
    };
}

put_fixed_at!(gos_rt_bin_put_u16_be_at, u16, to_be_bytes);
put_fixed_at!(gos_rt_bin_put_u16_le_at, u16, to_le_bytes);
put_fixed_at!(gos_rt_bin_put_u32_be_at, u32, to_be_bytes);
put_fixed_at!(gos_rt_bin_put_u32_le_at, u32, to_le_bytes);
put_fixed_at!(gos_rt_bin_put_u64_be_at, u64, to_be_bytes);
put_fixed_at!(gos_rt_bin_put_u64_le_at, u64, to_le_bytes);

fn uvarint_decode(buf: &[u8]) -> Result<(u64, usize), String> {
    let mut x = 0u64;
    let mut s = 0u32;
    for (i, &b) in buf.iter().enumerate() {
        if i == 10 {
            return Err("varint overflows u64".to_string());
        }
        if b < 0x80 {
            if i == 9 && b > 1 {
                return Err("varint overflows u64".to_string());
            }
            return Ok((x | (u64::from(b) << s), i + 1));
        }
        x |= u64::from(b & 0x7f) << s;
        s += 7;
    }
    Err("varint: buffer too small".to_string())
}

/// Packs `Ok((a, b))` as a GosResult whose payload points to a
/// GC-allocated 2-slot tuple.
fn ok_pair(a: i64, b: i64) -> i128 {
    let p = crate::c_abi::gos_rt_gc_alloc(16);
    if !p.is_null() {
        let slots = p.cast::<i64>();
        unsafe {
            *slots = a;
            *slots.add(1) = b;
        }
    }
    unsafe { super::vec::gos_rt_result_new(0, p as i64) }
}

/// `encoding::binary::uvarint(bytes) -> Result<(i64, i64), Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bin_uvarint(data: *const super::vec::GosVec) -> i128 {
    ffi_entry!(0i128, {
        match uvarint_decode(&unsafe { gosvec_u8(data) }) {
            Ok((v, n)) => ok_pair(v as i64, n as i64),
            Err(e) => err_result(&e),
        }
    })
}

/// `encoding::binary::varint(bytes) -> Result<(i64, i64), Error>`
/// (zigzag-decoded).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bin_varint(data: *const super::vec::GosVec) -> i128 {
    ffi_entry!(0i128, {
        match uvarint_decode(&unsafe { gosvec_u8(data) }) {
            Ok((ux, n)) => {
                let x = if ux & 1 == 0 {
                    (ux >> 1) as i64
                } else {
                    !((ux >> 1) as i64)
                };
                ok_pair(x, n as i64)
            }
            Err(e) => err_result(&e),
        }
    })
}

fn uvarint_encode(mut x: u64) -> Vec<u8> {
    let mut out = Vec::new();
    while x >= 0x80 {
        out.push((x as u8) | 0x80);
        x >>= 7;
    }
    out.push(x as u8);
    out
}

/// `encoding::binary::put_uvarint(buf, x) -> [u8]` - LEB128 encoding of
/// an unsigned varint. Mirrors the `put_*` shape: the buffer argument is
/// ignored and a fresh byte vector is returned.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bin_put_uvarint(
    _buf: *const super::vec::GosVec,
    v: i64,
) -> *mut super::vec::GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        bytes_to_gosvec(&uvarint_encode(v as u64))
    })
}

/// `encoding::binary::put_varint(buf, x) -> [u8]` - zigzag + LEB128
/// encoding of a signed varint.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bin_put_varint(
    _buf: *const super::vec::GosVec,
    v: i64,
) -> *mut super::vec::GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let ux = ((v << 1) ^ (v >> 63)) as u64;
        bytes_to_gosvec(&uvarint_encode(ux))
    })
}

// ---------------------------------------------------------------
// encoding::pem leaf intrinsics. These return only tuples / bytes /
// strings (the proven by-value ABIs); the user-facing `Block`
// struct + decode/encode/decode_all wrappers are injected Gossamer
// source (gossamer-parse autoderive) that builds real structs from
// these. The runtime crate cannot dep on gossamer-std, so the PEM
// parse/format logic (base64 + headers) is reimplemented inline.
// ---------------------------------------------------------------

fn pem_decode_one(input: &str) -> Result<(String, Vec<u8>), String> {
    let begin = input.find("-----BEGIN ").ok_or("pem: no PEM data found")?;
    let rest = &input[begin + 11..];
    let end_label = rest.find("-----").ok_or("pem: malformed BEGIN line")?;
    let label = rest[..end_label].to_string();
    let after_begin = &rest[end_label + 5..];
    let end_marker = format!("-----END {label}-----");
    let end_pos = after_begin
        .find(end_marker.as_str())
        .ok_or_else(|| format!("pem: missing END {label}"))?;
    let b64: String = after_begin[..end_pos]
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("");
    let bytes = base64_decode(&b64).map_err(|e| format!("pem: base64 decode: {e}"))?;
    Ok((label, bytes))
}

/// `__gos_pem_decode_raw(s) -> Result<(String, [u8]), errors::Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_pem_decode_raw(s: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        match pem_decode_one(cstr_to_str(s)) {
            Ok((label, bytes)) => {
                let t = alloc_cstring(label.as_bytes()) as i64;
                let b = bytes_to_gosvec(&bytes) as i64;
                ok_pair(t, b)
            }
            Err(e) => err_result(&e),
        }
    })
}

/// Slot layout of the [`gos_rt_pem_decode_all_raw`] Ok-payload vec:
/// the block label string at word 0 and the body byte-vec at word 1,
/// both owned by the vec.
static PEM_SLOT_CHILDREN: [crate::c_abi::vec::VecSlotChild; 2] = [
    crate::c_abi::vec::VecSlotChild {
        gate: -1,
        disc_word: 0,
        word: 0,
        kind: crate::c_abi::vec::vec_elem_kind::STRING,
    },
    crate::c_abi::vec::VecSlotChild {
        gate: -1,
        disc_word: 0,
        word: 1,
        kind: crate::c_abi::vec::vec_elem_kind::VEC,
    },
];

/// `__gos_pem_decode_all_raw(s) -> Result<[(String, [u8])], errors::Error>`.
/// The Ok payload is a `GosVec` of inline 2-slot `(String, [u8])`
/// tuples (16 bytes each: `[label_ptr, bytes_vec_ptr]`); the vec owns
/// both children per slot (slot-children layout registered after the
/// pushes), so `gos_rt_vec_free` deep-frees them.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_pem_decode_all_raw(s: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        let mut remaining = cstr_to_str(s);
        let out = unsafe { super::vec::gos_rt_vec_with_capacity(16, 0) };
        while let Some(begin) = remaining.find("-----BEGIN ") {
            let rest = &remaining[begin + 11..];
            let Some(end_label) = rest.find("-----") else {
                return err_result("pem: malformed BEGIN line");
            };
            let label = rest[..end_label].to_string();
            let after_begin = &rest[end_label + 5..];
            let end_marker = format!("-----END {label}-----");
            let Some(end_pos) = after_begin.find(end_marker.as_str()) else {
                return err_result(&format!("pem: missing END {label}"));
            };
            let b64: String = after_begin[..end_pos]
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with("-----"))
                .collect::<Vec<_>>()
                .join("");
            let bytes = match base64_decode(&b64) {
                Ok(b) => b,
                Err(e) => return err_result(&format!("pem: base64 decode: {e}")),
            };
            let pair: [i64; 2] = [
                alloc_cstring(label.as_bytes()) as i64,
                bytes_to_gosvec(&bytes) as i64,
            ];
            unsafe { super::vec::gos_rt_vec_push(out, pair.as_ptr().cast::<u8>()) };
            let consumed = begin + 11 + end_label + 5 + end_pos + end_marker.len();
            if consumed >= remaining.len() {
                break;
            }
            remaining = &remaining[consumed..];
        }
        super::vec::vec_set_slot_children(out, &PEM_SLOT_CHILDREN);
        unsafe { super::vec::gos_rt_result_new(0, out as i64) }
    })
}

/// `__gos_pem_encode_raw(block_type, bytes) -> String`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_pem_encode_raw(
    block_type: *const c_char,
    bytes: *const super::vec::GosVec,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let label = cstr_to_str(block_type);
        let data = unsafe { gosvec_u8(bytes) };
        let b64 = base64_encode(&data);
        let mut out = format!("-----BEGIN {label}-----\n");
        for chunk in b64.as_bytes().chunks(64) {
            out.push_str(std::str::from_utf8(chunk).unwrap_or(""));
            out.push('\n');
        }
        out.push_str(&format!("-----END {label}-----\n"));
        alloc_cstring(out.as_bytes())
    })
}

#[cfg(test)]
mod base64_roundtrip_tests {
    use super::{base64_decode, base64_encode};

    /// Every byte value and every padding length survives a round trip.
    #[test]
    fn base64_round_trips_every_byte_and_padding_length() {
        let all: Vec<u8> = (0..=255u8).collect();
        for len in 0..=all.len() {
            let data = &all[..len];
            let text = base64_encode(data);
            assert_eq!(text.len(), len.div_ceil(3) * 4, "length for {len} bytes");
            assert_eq!(
                base64_decode(&text).expect("decodes"),
                data,
                "round trip for {len} bytes"
            );
        }
    }

    /// The padding characters land where the tail length says.
    #[test]
    fn base64_pads_the_tail_it_has() {
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
    }

    /// Whitespace and padding are skipped; anything else is named.
    #[test]
    fn base64_decode_reports_the_character_it_refuses() {
        assert_eq!(base64_decode(" Zm9v \n").expect("decodes"), b"foo");
        let err = base64_decode("Zm9v*").expect_err("refuses");
        assert!(err.contains('*'), "{err}");
    }

    /// The four-character group path and the character-at-a-time path agree.
    ///
    /// Whitespace at every offset of a document pushes a group off its byte
    /// boundary, which is the one condition that decides which path a group
    /// takes; the answer may not depend on it.
    #[test]
    fn base64_decode_is_the_same_wherever_whitespace_falls() {
        let all: Vec<u8> = (0..=255u8).collect();
        for len in 0..=64usize {
            let data = &all[..len];
            let text = base64_encode(data);
            for cut in 0..=text.len() {
                let mut spaced = String::with_capacity(text.len() + 2);
                spaced.push_str(&text[..cut]);
                spaced.push_str(" \n");
                spaced.push_str(&text[cut..]);
                assert_eq!(
                    base64_decode(&spaced).expect("decodes"),
                    data,
                    "{len} bytes with whitespace at {cut}"
                );
            }
        }
    }
}
