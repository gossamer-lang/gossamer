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
// uuid - v4 (random) and v7 (timestamp-ordered) UUID generation,
// parsing, and normalization. Logic lives in the runtime crate
// (compiled tier links against `libgossamer_runtime.a` directly);
// `gossamer-std::uuid` is a thin facade that re-exports these
// functions for the interpreter.
// ---------------------------------------------------------------

/// Generates a fresh v4 (random) UUID and returns the canonical
/// hyphenated form as a heap-owned c-string.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_uuid_v4() -> *mut c_char {
    ffi_entry!({
        let s = ::uuid::Uuid::new_v4().hyphenated().to_string();
        alloc_cstring(s.as_bytes())
    })
}

/// Generates a fresh v7 (timestamp-ordered) UUID.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_uuid_v7() -> *mut c_char {
    ffi_entry!({
        let s = ::uuid::Uuid::now_v7().hyphenated().to_string();
        alloc_cstring(s.as_bytes())
    })
}

/// Returns 1 iff `s` parses as a canonical UUID.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_uuid_is_valid(s: *const c_char) -> i64 {
    ffi_entry!({
        if s.is_null() {
            return 0;
        }
        // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
        let s = unsafe { crate::c_abi::gos_str_arg_text(s) };
        i64::from(::uuid::Uuid::parse_str(s).is_ok())
    })
}

/// Returns the lowercase canonical form of `s` if it parses, else the empty string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_uuid_normalize(s: *const c_char) -> *mut c_char {
    ffi_entry!({
        if s.is_null() {
            return alloc_cstring(b"");
        }
        // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
        let s = unsafe { crate::c_abi::gos_str_arg_text(s) };
        let out = match ::uuid::Uuid::parse_str(s) {
            Ok(u) => u.hyphenated().to_string(),
            Err(_) => String::new(),
        };
        alloc_cstring(out.as_bytes())
    })
}

/// Returns the 32-char unhyphenated form of `s`, else the empty string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_uuid_simple(s: *const c_char) -> *mut c_char {
    ffi_entry!({
        if s.is_null() {
            return alloc_cstring(b"");
        }
        // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
        let s = unsafe { crate::c_abi::gos_str_arg_text(s) };
        let out = match ::uuid::Uuid::parse_str(s) {
            Ok(u) => u.simple().to_string(),
            Err(_) => String::new(),
        };
        alloc_cstring(out.as_bytes())
    })
}
