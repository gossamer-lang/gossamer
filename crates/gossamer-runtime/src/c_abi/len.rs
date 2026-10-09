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

use std::sync::atomic::Ordering;

use super::*;

// ---------------------------------------------------------------
// Array/Vec/Generic len - first i64 of the passed buffer is len
// ---------------------------------------------------------------

/// Reads the leading i64 of a len-prefixed pointer.
///
/// Special cases:
/// - NULL returns 0.
/// - The exact pointer returned by `gos_rt_os_args` returns
///   `argc - 1` (the args-list length) instead of whatever the
///   first argv entry happens to look like when dereferenced.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_arr_len(p: *const i64) -> i64 {
    ffi_entry!({
        if p.is_null() {
            return 0;
        }
        if (p as usize) == ARGS_PTR.load(Ordering::SeqCst) && p as usize != 0 {
            return ARGS_LEN.load(Ordering::SeqCst);
        }
        // SAFETY: callers guarantee the pointer is a len-prefixed
        // buffer, the args sentinel, or NULL.
        unsafe { *p }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_len(p: *const i64) -> i64 {
    // SAFETY: `p` is this shim's argument, live for the call (C-ABI contract) or null, which
    // `gos_rt_arr_len` accepts.
    ffi_entry!({ unsafe { gos_rt_arr_len(p) } })
}
