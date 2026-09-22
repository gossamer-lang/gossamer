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

use std::os::raw::c_char;

use super::*;

// ---------------------------------------------------------------
// Concat buffer - backing store for `__concat` / `format!`.
// Thread-local so `go { format!(...) }` calls don't trample
// each other.
// ---------------------------------------------------------------

thread_local! {
    static CONCAT_BUF: std::cell::RefCell<Vec<u8>> = std::cell::RefCell::new(Vec::with_capacity(256));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_concat_init() {
    ffi_entry!((), {
        CONCAT_BUF.with(|b| {
            let mut buf = b.borrow_mut();
            buf.clear();
            // Bound the high-water mark: a one-time large `format!()`
            // result would otherwise pin the buffer's capacity at the
            // peak forever. 4 KiB is plenty for typical concat chains;
            // anything larger reallocates next time and shrinks again
            // here, returning the slack to the allocator.
            if buf.capacity() > 4096 {
                *buf = Vec::with_capacity(256);
            }
        });
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_concat_str(s: *const c_char) {
    ffi_entry!((), {
        if s.is_null() {
            return;
        }
        let bytes = unsafe { crate::c_abi::gos_str_arg_bytes(s) };
        CONCAT_BUF.with(|b| b.borrow_mut().extend_from_slice(bytes));
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_concat_i64(n: i64) {
    ffi_entry!((), {
        use std::io::Write;
        // `Vec<u8>` is an `io::Write` sink, so the digits format straight
        // into the buffer with no intermediate `String` allocation.
        CONCAT_BUF.with(|b| {
            let _ = write!(&mut *b.borrow_mut(), "{n}");
        });
    });
}

/// Appends an *unsigned* 64-bit integer to the concat buffer.
/// Used when the source TyKind is `u8/u16/u32/u64/u128/usize` so
/// values `>= 2^63` print as their true magnitude rather than the
/// sign-flipped two's-complement view a single `i64` printer would
/// produce.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_concat_u64(n: u64) {
    ffi_entry!((), {
        use std::io::Write;
        CONCAT_BUF.with(|b| {
            let _ = write!(&mut *b.borrow_mut(), "{n}");
        });
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_concat_f64(x: f64) {
    ffi_entry!((), {
        let mut text = crate::builtins::FloatText::new();
        let digits = crate::builtins::f64_display(x, &mut text);
        CONCAT_BUF.with(|b| b.borrow_mut().extend_from_slice(digits));
    });
}

/// Appends `x` to the concat buffer in its `{:?}` spelling: an integral
/// value keeps a `.0` and an out-of-window magnitude switches to exponent
/// form, so the text always reads back as a float.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_concat_f64_debug(x: f64) {
    ffi_entry!((), {
        let s = crate::builtins::format_float_debug(x);
        CONCAT_BUF.with(|b| b.borrow_mut().extend_from_slice(s.as_bytes()));
    });
}

/// Appends `x` to the concat buffer with `prec` fractional digits.
/// Used by the `{:.N}` lowering when the surrounding `__concat`
/// pipeline can route the value directly without an intermediate
/// allocation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_concat_f64_prec(x: f64, prec: i64) {
    ffi_entry!((), {
        let prec = prec.clamp(0, 64) as usize;
        let s = format!("{x:.prec$}");
        CONCAT_BUF.with(|b| b.borrow_mut().extend_from_slice(s.as_bytes()));
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_concat_bool(b: i32) {
    ffi_entry!((), {
        let s = if b != 0 { "true" } else { "false" };
        CONCAT_BUF.with(|buf| buf.borrow_mut().extend_from_slice(s.as_bytes()));
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_concat_char(c: i32) {
    ffi_entry!((), {
        let ch = char::from_u32(c as u32).unwrap_or('\u{FFFD}');
        let s = ch.to_string();
        CONCAT_BUF.with(|b| b.borrow_mut().extend_from_slice(s.as_bytes()));
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_concat_finish() -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        CONCAT_BUF.with(|b| {
            let buf = b.borrow();
            alloc_cstring(&buf)
        })
    })
}

/// Returns the cause of `err` wrapped in an `Option<errors::Error>`
/// `GosResult` handle (`disc=0/Some` for non-null, `disc=1/None`
/// for null). Lets the match on `error.cause()` see a real
/// discriminant and terminate the cause-chain walk.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_error_cause(err: *const GosError) -> i128 {
    ffi_entry!(0i128, {
        let cause = if err.is_null() {
            std::ptr::null_mut::<GosError>()
        } else {
            unsafe { (*err).cause.as_ptr() }
        };
        // The `Some` arm borrows the cause the error holds a share of, so a
        // binding that keeps it takes a share of its own.
        let (disc, payload) = if cause.is_null() {
            (1, 0)
        } else {
            (0, cause as i64)
        };
        crate::c_abi::vec::pack_result(disc, payload)
    })
}

/// Substring search over raw bytes, so a message or needle containing an
/// interior NUL still matches on its full content.
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty()
        || (haystack.len() >= needle.len() && haystack.windows(needle.len()).any(|w| w == needle))
}

/// Walks the cause chain looking for a substring match.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_error_is(err: *const GosError, needle: *const c_char) -> i64 {
    ffi_entry!(-1, {
        if err.is_null() || needle.is_null() {
            return 0;
        }
        let needle = unsafe { crate::c_abi::gos_str_arg_bytes(needle) };
        let mut cur = err;
        while !cur.is_null() {
            let m = unsafe { (*cur).message };
            if !m.is_null()
                && contains_bytes(
                    unsafe { crate::c_abi::gos_str_arg_bytes(m.as_ptr()) },
                    needle,
                )
            {
                return 1;
            }
            cur = unsafe { (*cur).cause.as_ptr() };
        }
        0
    })
}

/// Joins every error message in `vec` (a `*mut GosVec` of `*mut GosError`)
/// with "; " and returns `Some(joined_error)` as a `*mut GosResult`.
/// Returns a `None`-shaped `GosResult` when the array is null or empty.
/// `ptr` points directly to the array of `GosError*` elements (stack-allocated
/// fixed-size array from the compiled tier); `len` is the compile-time count.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_errors_join(ptr: *const *mut GosError, len: i64) -> i128 {
    ffi_entry!(0i128, {
        let none = || crate::c_abi::vec::pack_result(1, 0);
        if ptr.is_null() || len <= 0 {
            return none();
        }
        let len = len as usize;
        let mut parts: Vec<String> = Vec::with_capacity(len);
        for i in 0..len {
            let err = unsafe { *ptr.add(i) }; // ptr is the array base from the caller
            if err.is_null() {
                continue;
            }
            let m = unsafe { (*err).message };
            if m.is_null() {
                continue;
            }
            parts.push(unsafe { crate::c_abi::gos_str_arg_string(m.as_ptr()) });
        }
        if parts.is_empty() {
            return none();
        }
        let combined = parts.join("; ");
        let err = super::errors::error_alloc(
            alloc_cstring(combined.as_bytes()),
            std::ptr::null_mut(),
            Vec::new(),
        );
        crate::c_abi::vec::pack_result(0, err as i64)
    })
}

/// Joins every error in `vec` (a `*mut GosVec` of `*mut GosError` elements)
/// with "; " and returns `Some(joined_error)` as a `*mut GosResult`.
/// Returns a None-shaped result when `vec` is null or empty.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_errors_join_vec(vec: *mut GosVec) -> i128 {
    ffi_entry!(0i128, {
        let none = || crate::c_abi::vec::pack_result(1, 0);
        if vec.is_null() {
            return none();
        }
        let len = unsafe { (*vec).len } as usize;
        if len == 0 {
            return none();
        }
        let data = unsafe { (*vec).ptr.as_ptr() } as *const *mut GosError;
        let mut parts: Vec<String> = Vec::with_capacity(len);
        for i in 0..len {
            let err = unsafe { *data.add(i) };
            if err.is_null() {
                continue;
            }
            let m = unsafe { (*err).message };
            if m.is_null() {
                continue;
            }
            parts.push(unsafe { crate::c_abi::gos_str_arg_string(m.as_ptr()) });
        }
        if parts.is_empty() {
            return none();
        }
        let combined = parts.join("; ");
        let err = super::errors::error_alloc(
            alloc_cstring(combined.as_bytes()),
            std::ptr::null_mut(),
            Vec::new(),
        );
        crate::c_abi::vec::pack_result(0, err as i64)
    })
}
