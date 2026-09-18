#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::similar_names)]
#![allow(clippy::many_single_char_names)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::same_length_and_capacity)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::cast_ptr_alignment)]
#![allow(clippy::ptr_as_ptr)]
#![allow(static_mut_refs)]
#![allow(unused_unsafe)]
#![allow(clippy::wildcard_imports)]

//! C-ABI surface for `std::utf8`. Mirrors the most common helpers
//! exposed by `gossamer_std::utf8` in shapes the compiled tiers
//! can call directly:
//! - bool predicates return `i64` (0/1)
//! - `char` arguments arrive as `u32`
//! - `String` arguments arrive as `*const c_char`
//!
//! The tuple-returning `decode_rune` family returns `(char, i64)`
//! via the by-value-aggregate ABI: a GC-allocated 2-slot heap
//! buffer (codepoint, byte-length) the caller memcpys into its
//! tuple alloca.

use std::os::raw::c_char;

#[inline]
unsafe fn cstr_to_str<'a>(s: *const c_char) -> &'a str {
    if s.is_null() {
        return "";
    }
    unsafe { crate::c_abi::gos_str_arg_text(s) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_utf8_rune_count_in_string(s: *const c_char) -> i64 {
    ffi_entry!(0, {
        let text = unsafe { cstr_to_str(s) };
        text.chars().count() as i64
    })
}

/// `utf8::rune_count(bytes: *mut GosVec) -> i64` - counts code
/// points in a byte buffer. Invalid sequences return 0 for the run
/// they cover (matching Rust's `std::str::from_utf8` failure mode).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_utf8_rune_count_bytes(
    vec: *const crate::c_abi::vec::GosVec,
) -> i64 {
    ffi_entry!(0, {
        if vec.is_null() {
            return 0;
        }
        let bytes = unsafe { crate::c_abi::vec::vec_bytes_cow(vec) };
        std::str::from_utf8(&bytes).map_or(0, |s| s.chars().count() as i64)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_utf8_rune_len(c: u32) -> i64 {
    ffi_entry!(-1, {
        char::from_u32(c).map_or(-1, |ch| ch.len_utf8() as i64)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_utf8_valid_rune(c: u32) -> i64 {
    ffi_entry!(0, { i64::from(char::from_u32(c).is_some()) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_utf8_valid_string(s: *const c_char) -> i64 {
    ffi_entry!(0, {
        if s.is_null() {
            return 1;
        }
        let ok = std::str::from_utf8(unsafe { crate::c_abi::gos_str_arg_bytes(s) }).is_ok();
        i64::from(ok)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_utf8_is_valid(vec: *const crate::c_abi::vec::GosVec) -> i64 {
    ffi_entry!(0, {
        if vec.is_null() {
            return 1;
        }
        let bytes = unsafe { crate::c_abi::vec::vec_bytes_cow(vec) };
        i64::from(std::str::from_utf8(&bytes).is_ok())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_utf8_full_rune_in_string(s: *const c_char) -> i64 {
    ffi_entry!(0, {
        let text = unsafe { cstr_to_str(s) };
        let bytes = text.as_bytes();
        if bytes.is_empty() {
            return 0;
        }
        let first = bytes[0];
        let width = if first < 0x80 {
            1
        } else if first < 0xC0 {
            return 0;
        } else if first < 0xE0 {
            2
        } else if first < 0xF0 {
            3
        } else {
            4
        };
        i64::from(bytes.len() >= width)
    })
}

/// `utf8::full_rune(bytes) -> bool` - whether a byte buffer begins with a
/// complete UTF-8 sequence. The string-taking twin is
/// `gos_rt_utf8_full_rune_in_string`; this is the one a `Vec<u8>` reaches.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_utf8_full_rune(vec: *const crate::c_abi::vec::GosVec) -> i64 {
    ffi_entry!(0, {
        let bytes = unsafe { crate::c_abi::vec::vec_bytes(vec) };
        let Some(&first) = bytes.first() else {
            return 0;
        };
        let width = if first < 0x80 {
            1
        } else if first < 0xC0 {
            return 0;
        } else if first < 0xE0 {
            2
        } else if first < 0xF0 {
            3
        } else {
            4
        };
        i64::from(bytes.len() >= width)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_utf8_rune_start(b: u32) -> i64 {
    ffi_entry!(0, { i64::from((b as u8) & 0xC0 != 0x80) })
}

// ---------------------------------------------------------------
// utf8::decode_rune family + append_rune (0.10.0 cross-tier wiring)
// ---------------------------------------------------------------
// Mirrors gossamer_std::utf8 exactly. The decode_* helpers return a
// `(char, i64)` tuple via the by-value-aggregate ABI (GC-allocated
// 2-slot buffer: slot 0 = codepoint, slot 1 = byte length).

const RUNE_ERROR: char = '\u{FFFD}';

unsafe fn vec_bytes(v: *const super::vec::GosVec) -> Vec<u8> {
    unsafe { crate::c_abi::vec::vec_bytes(v) }
}

fn rune_pair(ch: char, n: usize) -> *mut u8 {
    let p = crate::c_abi::gos_rt_gc_alloc(16);
    if !p.is_null() {
        let slots = p.cast::<i64>();
        unsafe {
            *slots = i64::from(ch as u32);
            *slots.add(1) = n as i64;
        }
    }
    p
}

fn decode_rune_bytes(p: &[u8]) -> (char, usize) {
    if p.is_empty() {
        return (RUNE_ERROR, 0);
    }
    match std::str::from_utf8(p) {
        Ok(s) => s
            .chars()
            .next()
            .map_or((RUNE_ERROR, 1), |ch| (ch, ch.len_utf8())),
        Err(e) => {
            let valid = e.valid_up_to();
            if valid == 0 {
                (RUNE_ERROR, 1)
            } else {
                let ch = std::str::from_utf8(&p[..valid])
                    .ok()
                    .and_then(|s| s.chars().next())
                    .unwrap_or(RUNE_ERROR);
                (ch, ch.len_utf8())
            }
        }
    }
}

/// `utf8::decode_rune(bytes) -> (char, i64)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_utf8_decode_rune(v: *const super::vec::GosVec) -> *mut u8 {
    ffi_entry!(std::ptr::null_mut(), {
        let (ch, n) = decode_rune_bytes(&unsafe { vec_bytes(v) });
        rune_pair(ch, n)
    })
}

/// `utf8::decode_rune_in_string(s) -> (char, i64)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_utf8_decode_rune_in_string(s: *const c_char) -> *mut u8 {
    ffi_entry!(std::ptr::null_mut(), {
        let (ch, n) = match unsafe { cstr_to_str(s) }.chars().next() {
            Some(ch) => (ch, ch.len_utf8()),
            None => (RUNE_ERROR, 0),
        };
        rune_pair(ch, n)
    })
}

/// `utf8::decode_last_rune(bytes) -> (char, i64)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_utf8_decode_last_rune(v: *const super::vec::GosVec) -> *mut u8 {
    ffi_entry!(std::ptr::null_mut(), {
        let p = unsafe { vec_bytes(v) };
        if p.is_empty() {
            return rune_pair(RUNE_ERROR, 0);
        }
        let mut i = p.len();
        while i > 0 && i > p.len().saturating_sub(4) {
            i -= 1;
            if p[i] & 0xC0 != 0x80 {
                break;
            }
        }
        let (ch, size) = decode_rune_bytes(&p[i..]);
        if i + size == p.len() {
            rune_pair(ch, size)
        } else {
            rune_pair(RUNE_ERROR, 1)
        }
    })
}

/// `utf8::decode_last_rune_in_string(s) -> (char, i64)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_utf8_decode_last_rune_in_string(s: *const c_char) -> *mut u8 {
    ffi_entry!(std::ptr::null_mut(), {
        let (ch, n) = match unsafe { cstr_to_str(s) }.chars().next_back() {
            Some(ch) => (ch, ch.len_utf8()),
            None => (RUNE_ERROR, 0),
        };
        rune_pair(ch, n)
    })
}

/// `utf8::append_rune(buf, r) -> [u8]`. Appends the UTF-8 encoding
/// of `r` to a copy of `buf` and returns the new byte vector.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_utf8_append_rune(
    v: *const super::vec::GosVec,
    r: u32,
) -> *mut super::vec::GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let mut buf = unsafe { vec_bytes(v) };
        let ch = char::from_u32(r).unwrap_or('\0');
        let mut tmp = [0u8; 4];
        let n = ch.encode_utf8(&mut tmp).len();
        buf.extend_from_slice(&tmp[..n]);
        super::encoding::bytes_to_gosvec(&buf)
    })
}
