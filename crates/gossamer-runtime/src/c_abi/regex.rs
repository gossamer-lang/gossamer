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
// regex module - wraps the host `regex` crate with a c-ABI shim.
// Patterns compile lazily via `gos_rt_regex_compile`; matches /
// captures / replacements operate on `*const Regex` handles
// returned to user code as opaque `*mut GosRegex`.
// ---------------------------------------------------------------

#[repr(transparent)]
pub struct GosRegex {
    inner: ::regex::Regex,
}

/// Slot layout of `gos_rt_regex_find_all` elements: 24-byte
/// `(start i64, end i64, text String)` tuples - the owned match text
/// lives at word 2, unconditionally.
static FIND_ALL_SLOT_CHILDREN: [crate::c_abi::vec::VecSlotChild; 1] =
    [crate::c_abi::vec::VecSlotChild {
        gate: -1,
        disc_word: 0,
        word: 2,
        kind: crate::c_abi::vec::vec_elem_kind::STRING,
    }];

/// Slot layout of the capture-group vecs (`gos_rt_regex_captures` /
/// `gos_rt_regex_captures_all` inner rows): 16-byte by-value
/// `Option<String>` slots - the owned group text at word 1 is live
/// only when the discriminant word 0 reads `0` (Some).
static CAPTURE_SLOT_CHILDREN: [crate::c_abi::vec::VecSlotChild; 1] =
    [crate::c_abi::vec::VecSlotChild {
        gate: 0,
        disc_word: 0,
        word: 1,
        kind: crate::c_abi::vec::vec_elem_kind::STRING,
    }];

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_regex_compile(pat: *const c_char) -> *mut GosRegex {
    ffi_entry!(std::ptr::null_mut(), {
        if pat.is_null() {
            return std::ptr::null_mut();
        }
        let s = unsafe { crate::c_abi::gos_str_arg_text(pat) };
        match ::regex::Regex::new(s) {
            Ok(re) => Box::into_raw(Box::new(GosRegex { inner: re })),
            Err(_) => std::ptr::null_mut(),
        }
    })
}

/// `regex::compile(pattern) -> Result<Pattern, String>`. Ok payload
/// is the `*mut GosRegex` handle; Err payload is the diagnostic
/// string, formatted identically to the VM tier's
/// `regex_std::RegexError::InvalidPattern` so all tiers print the
/// same message.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_regex_compile_result(pat: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        if pat.is_null() {
            let msg = alloc_cstring(b"regex: invalid pattern ``: null pattern");
            return gos_rt_result_new(1, msg as i64);
        }
        let s = unsafe { crate::c_abi::gos_str_arg_text(pat) };
        match ::regex::Regex::new(s) {
            Ok(re) => {
                let handle = Box::into_raw(Box::new(GosRegex { inner: re }));
                gos_rt_result_new(0, handle as i64)
            }
            Err(e) => {
                let msg = format!("regex: invalid pattern `{s}`: {e}");
                gos_rt_result_new(1, alloc_cstring(msg.as_bytes()) as i64)
            }
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_regex_is_match(re: *const GosRegex, text: *const c_char) -> i64 {
    ffi_entry!(-1, {
        if re.is_null() || text.is_null() {
            return 0;
        }
        let s = unsafe { crate::c_abi::gos_str_arg_text(text) };
        i64::from(unsafe { (*re).inner.is_match(s) })
    })
}

/// Counts the non-overlapping matches without building them.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_regex_count(re: *const GosRegex, text: *const c_char) -> i64 {
    ffi_entry!(-1, {
        if re.is_null() || text.is_null() {
            return 0;
        }
        let s = unsafe { crate::c_abi::gos_str_arg_text(text) };
        i64::try_from(unsafe { (*re).inner.find_iter(s).count() }).unwrap_or(i64::MAX)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_regex_find(
    re: *const GosRegex,
    text: *const c_char,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if re.is_null() || text.is_null() {
            return alloc_cstring(b"");
        }
        let s = unsafe { crate::c_abi::gos_str_arg_text(text) };
        match unsafe { (*re).inner.find(s) } {
            Some(m) => alloc_cstring(m.as_str().as_bytes()),
            None => alloc_cstring(b""),
        }
    })
}

/// Returns `Option<(start, end, text)>` as a `*mut GosResult`.
/// disc=0 → Some, disc=1 → None. The payload is a heap-allocated
/// `{start: i64, end: i64, text: *mut c_char}` triple.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_regex_find_opt(re: *const GosRegex, text: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        if re.is_null() || text.is_null() {
            return gos_rt_result_new(1, 0);
        }
        let s = unsafe { crate::c_abi::gos_str_arg_text(text) };
        match unsafe { (*re).inner.find(s) } {
            None => gos_rt_result_new(1, 0),
            Some(m) => {
                #[repr(C)]
                struct Triple {
                    start: i64,
                    end: i64,
                    text: i64,
                }
                let cstr = alloc_cstring(m.as_str().as_bytes());
                let triple = Box::into_raw(Box::new(Triple {
                    start: m.start() as i64,
                    end: m.end() as i64,
                    text: cstr as i64,
                }));
                gos_rt_result_new(0, triple as i64)
            }
        }
    })
}

/// Returns `Option<Vec<String>>` as a `*mut GosResult`.
/// disc=0 → Some(captures), disc=1 → None. Group 0 = full match.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_regex_captures(re: *const GosRegex, text: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        if re.is_null() || text.is_null() {
            return gos_rt_result_new(1, 0);
        }
        let s = unsafe { crate::c_abi::gos_str_arg_text(text) };
        match unsafe { (*re).inner.captures(s) } {
            None => gos_rt_result_new(1, 0),
            Some(caps) => {
                // `Option<String>` is the 2-word by-value `i128` (16-byte)
                // representation, so the inner Vec stores 16-byte elements.
                let inner = unsafe { gos_rt_vec_new(16) };
                for i in 0..caps.len() {
                    let opt: i128 = match caps.get(i) {
                        Some(m) => {
                            gos_rt_result_new(0, alloc_cstring(m.as_str().as_bytes()) as i64)
                        }
                        None => gos_rt_result_new(1, 0),
                    };
                    unsafe { gos_rt_vec_push(inner, std::ptr::addr_of!(opt).cast::<u8>()) };
                }
                // Tagged after the pushes: the vec owns the fresh group
                // strings, so `gos_rt_vec_free` reclaims them even when
                // the consumer never iterates every slot.
                crate::c_abi::vec::vec_set_slot_children(inner, &CAPTURE_SLOT_CHILDREN);
                gos_rt_result_new(0, inner as i64)
            }
        }
    })
}

/// Finds every non-overlapping match of `re` in `text` and returns
/// a `Vec<String>` of the matched substrings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_regex_find_all(
    re: *const GosRegex,
    text: *const c_char,
) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        // Each element is a 24-byte `(i64 start, i64 end, *c_char text)`
        // tuple. The previous 8-byte-per-element shape only stored the
        // matched text, leaving `hit.0` / `hit.1` reading garbage and
        // `hit.2` indexing past the end of the buffer (which the
        // example then printed as an empty string).
        let vec = unsafe { gos_rt_vec_new(24) };
        if re.is_null() || text.is_null() {
            return vec;
        }
        let s = unsafe { crate::c_abi::gos_str_arg_text(text) };
        for m in unsafe { (*re).inner.find_iter(s) } {
            let cstr = alloc_cstring(m.as_str().as_bytes());
            #[repr(C)]
            struct Tup {
                start: i64,
                end: i64,
                text: i64,
            }
            let entry = Tup {
                start: m.start() as i64,
                end: m.end() as i64,
                text: cstr as i64,
            };
            unsafe {
                gos_rt_vec_push(vec, std::ptr::addr_of!(entry).cast::<u8>());
            }
        }
        // Tagged after the pushes: the vec owns the fresh match-text
        // strings; `gos_rt_vec_free` reclaims unvisited slots (the
        // early-`break` path) instead of leaking them.
        crate::c_abi::vec::vec_set_slot_children(vec, &FIND_ALL_SLOT_CHILDREN);
        vec
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_regex_replace_all(
    re: *const GosRegex,
    text: *const c_char,
    repl: *const c_char,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if re.is_null() || text.is_null() {
            return alloc_cstring(b"");
        }
        let s = unsafe { crate::c_abi::gos_str_arg_text(text) };
        let r = if repl.is_null() {
            ""
        } else {
            unsafe { crate::c_abi::gos_str_arg_text(repl) }
        };
        alloc_cstring(unsafe { (*re).inner.replace_all(s, r) }.as_bytes())
    })
}

/// Replaces only the first match of `re` in `text` with `repl`.
/// Companion to [`gos_rt_regex_replace_all`] - separate symbol so
/// the codegen dispatch tables can pick the right semantics.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_regex_replace(
    re: *const GosRegex,
    text: *const c_char,
    repl: *const c_char,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if re.is_null() || text.is_null() {
            return alloc_cstring(b"");
        }
        let s = unsafe { crate::c_abi::gos_str_arg_text(text) };
        let r = if repl.is_null() {
            ""
        } else {
            unsafe { crate::c_abi::gos_str_arg_text(repl) }
        };
        alloc_cstring(unsafe { (*re).inner.replace(s, r) }.as_bytes())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_regex_split(
    re: *const GosRegex,
    text: *const c_char,
) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        // STRING-typed so `gos_rt_vec_free` deep-frees each owned piece
        // (consumer loops borrow the slot strings; see the unicode
        // `alloc_string_vec` template).
        let vec = unsafe {
            crate::c_abi::vec::gos_rt_vec_new_typed(8, crate::c_abi::vec::vec_elem_kind::STRING)
        };
        if re.is_null() || text.is_null() {
            return vec;
        }
        let s = unsafe { crate::c_abi::gos_str_arg_text(text) };
        for piece in unsafe { (*re).inner.split(s) } {
            let cstr = alloc_cstring(piece.as_bytes());
            let ptr_val = cstr as i64;
            unsafe {
                gos_rt_vec_push(vec, std::ptr::addr_of!(ptr_val).cast::<u8>());
            }
        }
        vec
    })
}

/// Returns `Vec<Vec<*c_char>>` - outer Vec has one entry per
/// match, inner Vec has one entry per group (group 0 = full
/// match, group 1+ = sub-captures). Missing groups are NULL
/// (which user code can pattern-match as `Option::None` because
/// the runtime treats null pointers as the zero discriminant).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_regex_captures_all(
    re: *const GosRegex,
    text: *const c_char,
) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        // VEC-typed outer: freeing it recursively frees each inner row,
        // whose own slot meta then reclaims the Some-group strings.
        let outer = unsafe {
            crate::c_abi::vec::gos_rt_vec_new_typed(8, crate::c_abi::vec::vec_elem_kind::VEC)
        };
        if re.is_null() || text.is_null() {
            return outer;
        }
        let s = unsafe { crate::c_abi::gos_str_arg_text(text) };
        for caps in unsafe { (*re).inner.captures_iter(s) } {
            // Each group is an `Option<String>` - the 2-word by-value `i128`
            // (16-byte) representation, so the inner Vec stores 16-byte
            // elements (disc 0 = Some, disc 1 = None).
            let inner = unsafe { gos_rt_vec_new(16) };
            for i in 0..caps.len() {
                let opt: i128 = match caps.get(i) {
                    Some(m) => gos_rt_result_new(0, alloc_cstring(m.as_str().as_bytes()) as i64),
                    None => gos_rt_result_new(1, 0),
                };
                unsafe {
                    gos_rt_vec_push(inner, std::ptr::addr_of!(opt).cast::<u8>());
                }
            }
            crate::c_abi::vec::vec_set_slot_children(inner, &CAPTURE_SLOT_CHILDREN);
            let inner_val = inner as i64;
            unsafe {
                gos_rt_vec_push(outer, std::ptr::addr_of!(inner_val).cast::<u8>());
            }
        }
        outer
    })
}
