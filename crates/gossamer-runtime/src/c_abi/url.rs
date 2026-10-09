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
use crate::codec::percent::{self, Component};

// ---------------------------------------------------------------
// net::url - percent-encoding helpers (query_escape / path_escape /
// query_unescape / path_unescape). RFC 3986 unreserved set is
// preserved; everything else encodes to %HH.
// ---------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_url_query_escape(s: *const c_char) -> *mut c_char {
    ffi_entry!({
        if s.is_null() {
            return alloc_cstring(b"");
        }
        // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
        let s = unsafe { crate::c_abi::gos_str_arg_text(s) };
        alloc_cstring(percent::encode(s, Component::Query).as_bytes())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_url_path_escape(s: *const c_char) -> *mut c_char {
    ffi_entry!({
        if s.is_null() {
            return alloc_cstring(b"");
        }
        // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
        let s = unsafe { crate::c_abi::gos_str_arg_text(s) };
        alloc_cstring(percent::encode(s, Component::Path).as_bytes())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_url_query_unescape(s: *const c_char) -> *mut c_char {
    ffi_entry!({
        if s.is_null() {
            return alloc_cstring(b"");
        }
        // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
        let s = unsafe { crate::c_abi::gos_str_arg_text(s) };
        alloc_cstring(percent::decode(s, Component::Query).as_bytes())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_url_path_unescape(s: *const c_char) -> *mut c_char {
    ffi_entry!({
        if s.is_null() {
            return alloc_cstring(b"");
        }
        // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
        let s = unsafe { crate::c_abi::gos_str_arg_text(s) };
        alloc_cstring(percent::decode(s, Component::Path).as_bytes())
    })
}
