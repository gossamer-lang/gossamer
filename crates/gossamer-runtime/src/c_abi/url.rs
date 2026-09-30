#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
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
#![allow(clippy::wildcard_imports)]

use std::os::raw::c_char;

use super::*;

// ---------------------------------------------------------------
// net::url - percent-encoding helpers (query_escape / path_escape /
// query_unescape / path_unescape). RFC 3986 unreserved set is
// preserved; everything else encodes to %HH.
// ---------------------------------------------------------------

fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~')
}

fn percent_encode(input: &str, query_mode: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        if is_unreserved(b) {
            out.push(b as char);
        } else if query_mode && b == b' ' {
            out.push('+');
        } else if !query_mode && matches!(b, b'/' | b':' | b'@') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

pub(crate) fn percent_decode(input: &str, query_mode: bool) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' && i + 2 < bytes.len() {
            let h1 = (bytes[i + 1] as char).to_digit(16);
            let h2 = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h1), Some(h2)) = (h1, h2) {
                out.push((h1 * 16 + h2) as u8);
                i += 3;
                continue;
            }
        }
        if query_mode && b == b'+' {
            out.push(b' ');
        } else {
            out.push(b);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_url_query_escape(s: *const c_char) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if s.is_null() {
            return alloc_cstring(b"");
        }
        // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
        let s = unsafe { crate::c_abi::gos_str_arg_text(s) };
        alloc_cstring(percent_encode(s, true).as_bytes())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_url_path_escape(s: *const c_char) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if s.is_null() {
            return alloc_cstring(b"");
        }
        // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
        let s = unsafe { crate::c_abi::gos_str_arg_text(s) };
        alloc_cstring(percent_encode(s, false).as_bytes())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_url_query_unescape(s: *const c_char) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if s.is_null() {
            return alloc_cstring(b"");
        }
        // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
        let s = unsafe { crate::c_abi::gos_str_arg_text(s) };
        alloc_cstring(percent_decode(s, true).as_bytes())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_url_path_unescape(s: *const c_char) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if s.is_null() {
            return alloc_cstring(b"");
        }
        // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
        let s = unsafe { crate::c_abi::gos_str_arg_text(s) };
        alloc_cstring(percent_decode(s, false).as_bytes())
    })
}
