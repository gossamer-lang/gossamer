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
// Concat buffer - backing store for `__concat` / `format!`.
// Thread-local so `go { format!(...) }` calls don't trample
// each other.
// ---------------------------------------------------------------

thread_local! {
    static CONCAT_BUF: std::cell::RefCell<Vec<u8>> = std::cell::RefCell::new(Vec::with_capacity(256));
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_concat_init() {
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
        // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
        let bytes = unsafe { crate::c_abi::gos_str_arg_bytes(s) };
        CONCAT_BUF.with(|b| b.borrow_mut().extend_from_slice(bytes));
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_concat_i64(n: i64) {
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
pub extern "C" fn gos_rt_concat_u64(n: u64) {
    ffi_entry!((), {
        use std::io::Write;
        CONCAT_BUF.with(|b| {
            let _ = write!(&mut *b.borrow_mut(), "{n}");
        });
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_concat_f64(x: f64) {
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
pub extern "C" fn gos_rt_concat_f64_debug(x: f64) {
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
pub extern "C" fn gos_rt_concat_f64_prec(x: f64, prec: i64) {
    ffi_entry!((), {
        let prec = prec.clamp(0, 64) as usize;
        let s = format!("{x:.prec$}");
        CONCAT_BUF.with(|b| b.borrow_mut().extend_from_slice(s.as_bytes()));
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_concat_bool(b: i32) {
    ffi_entry!((), {
        let s = if b != 0 { "true" } else { "false" };
        CONCAT_BUF.with(|buf| buf.borrow_mut().extend_from_slice(s.as_bytes()));
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_concat_char(c: i32) {
    ffi_entry!((), {
        let ch = char::from_u32(c as u32).unwrap_or('\u{FFFD}');
        let s = ch.to_string();
        CONCAT_BUF.with(|b| b.borrow_mut().extend_from_slice(s.as_bytes()));
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_concat_finish() -> *mut c_char {
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
            // SAFETY: `err` is non-null on this branch and live for the call (C-ABI contract).
            unsafe { (*err).cause.as_ptr() }
        };
        // The `Some` arm borrows the cause the error holds a share of, so a
        // binding that keeps it takes a share of its own.
        let (disc, payload) = if cause.is_null() {
            (1, 0)
        } else {
            (0, cause as i64)
        };
        crate::c_abi::result::pack_result(disc, payload)
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
        // SAFETY: `needle` is a String argument from compiled code, null or a live string body for the whole call.
        let needle = unsafe { crate::c_abi::gos_str_arg_bytes(needle) };
        let mut cur = err;
        while !cur.is_null() {
            // SAFETY: `cur` is non-null (the loop condition) and a live error: `err` per the
            // C-ABI contract, then each cause.
            let m = unsafe { (*cur).message };
            if !m.is_null()
                && contains_bytes(
                    // SAFETY: a non-null message is the runtime string the error owns.
                    unsafe { crate::c_abi::gos_str_arg_bytes(m.as_ptr()) },
                    needle,
                )
            {
                return 1;
            }
            // SAFETY: `cur` is non-null (the loop condition), and a live error's `cause` is null
            // or a share of a live error, so the chain walk only reaches live errors.
            cur = unsafe { (*cur).cause.as_ptr() };
        }
        0
    })
}

/// The top message of the error `err` names, or `None` for a null error or one
/// with no message.
///
/// # Safety
/// `err` is null or a live error.
unsafe fn error_message_of(err: *const GosError) -> Option<String> {
    // SAFETY: this `unsafe fn`'s caller passes `err` null or live.
    let err = unsafe { err.as_ref() }?;
    let message = err.message.as_ptr();
    // SAFETY: a non-null message is the runtime string the live error owns.
    (!message.is_null()).then(|| unsafe { crate::c_abi::gos_str_arg_string(message) })
}

/// `Some` of one error whose message joins `parts` with "; ", or `None` when
/// there is nothing to join.
fn joined_error(parts: &[String]) -> i128 {
    if parts.is_empty() {
        return crate::c_abi::result::pack_result(1, 0);
    }
    let combined = parts.join("; ");
    // SAFETY: the message is a fresh string and there is no cause.
    let err = unsafe {
        super::errors::error_alloc(
            alloc_cstring(combined.as_bytes()),
            std::ptr::null_mut(),
            Vec::new(),
        )
    };
    crate::c_abi::result::pack_result(0, err as i64)
}

/// Joins every error message in the `len` errors at `ptr` with "; " and
/// returns `Some(joined_error)` as a `*mut GosResult`; `None` when the array
/// is null or holds no message. `ptr` is the compiled tier's fixed-size array
/// of `GosError*` elements and `len` its compile-time count.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_errors_join(ptr: *const *mut GosError, len: i64) -> i128 {
    ffi_entry!(0i128, {
        let count = usize::try_from(len).unwrap_or(0);
        if ptr.is_null() || count == 0 {
            return joined_error(&[]);
        }
        // SAFETY: `ptr` is non-null (checked above) and addresses `count` error words (C-ABI
        // contract).
        let errors = unsafe { std::slice::from_raw_parts(ptr, count) };
        let parts: Vec<String> = errors
            .iter()
            // SAFETY: each element is null or a live error (C-ABI contract), which
            // `error_message_of` accepts.
            .filter_map(|&err| unsafe { error_message_of(err) })
            .collect();
        joined_error(&parts)
    })
}

/// Joins every error in `vec` (a `Vec<errors::Error>`) with "; " and returns
/// `Some(joined_error)` as a `*mut GosResult`; `None` when `vec` is null or
/// holds no message.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_errors_join_vec(vec: *mut GosVec) -> i128 {
    ffi_entry!(0i128, {
        // SAFETY: `vec` is this shim's argument, null or a live `Vec<errors::Error>` (C-ABI
        // contract).
        let Some(errors) = (unsafe { crate::c_abi::vec::VecView::of(vec) }) else {
            return joined_error(&[]);
        };
        let parts: Vec<String> = (0..errors.len())
            // SAFETY: each element of a `Vec<errors::Error>` is null or a live error (C-ABI
            // contract), which `error_message_of` accepts.
            .filter_map(|i| unsafe { error_message_of(errors.pointer_at::<GosError>(i)) })
            .collect();
        joined_error(&parts)
    })
}
