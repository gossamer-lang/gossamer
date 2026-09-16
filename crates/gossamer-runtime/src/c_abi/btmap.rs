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

/// The opening bracket a sequence renders under.
///
/// A `Vec` is written `#[..]` and a fixed array or slice `[..]`, and the two
/// share one runtime representation - so the caller's static type is the only
/// thing that knows which spelling a value takes, and it says so here.
fn seq_open(bare: i32) -> &'static str {
    if bare == 0 { "#[" } else { "[" }
}

/// The same choice for a sequence with nothing in it.
fn seq_empty(bare: i32) -> &'static str {
    if bare == 0 { "#[]" } else { "[]" }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_format_i64(v: *const GosVec, bare: i32) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if v.is_null() {
            return alloc_cstring(seq_empty(bare).as_bytes());
        }
        let vec = unsafe { &*v };
        let mut out = String::with_capacity(2 + (vec.len as usize) * 4);
        out.push_str(seq_open(bare));
        for i in 0..vec.len {
            if i > 0 {
                out.push_str(", ");
            }
            // Byte-strided elements (`u8`, `bool`) store one byte per slot,
            // so the value is read at the width the header declares.
            let n = unsafe { crate::c_abi::vec::vec_elem_load_i64(vec, i) };
            out.push_str(&format!("{n}"));
        }
        out.push(']');
        alloc_cstring(out.as_bytes())
    })
}

/// Renders a `u64`-elem `Vec` as `[v0, v1, …]`. A slot holds the value's
/// bits, so an element at or above `i64::MAX` reads as its unsigned decimal
/// rather than the negative the same bits spell. Returns a fresh String
/// pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_format_u64(v: *const GosVec, bare: i32) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if v.is_null() {
            return alloc_cstring(seq_empty(bare).as_bytes());
        }
        let vec = unsafe { &*v };
        let mut out = String::with_capacity(2 + (vec.len as usize) * 4);
        out.push_str(seq_open(bare));
        for i in 0..vec.len {
            if i > 0 {
                out.push_str(", ");
            }
            let n = unsafe { crate::c_abi::vec::vec_elem_load_i64(vec, i) } as u64;
            out.push_str(&format!("{n}"));
        }
        out.push(']');
        alloc_cstring(out.as_bytes())
    })
}

/// Renders an `f64`-elem `Vec` as `[v0, v1, …]`. Returns a fresh
/// String pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_format_f64(v: *const GosVec, bare: i32) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if v.is_null() {
            return alloc_cstring(seq_empty(bare).as_bytes());
        }
        let vec = unsafe { &*v };
        let mut out = String::with_capacity(2 + (vec.len as usize) * 6);
        out.push_str(seq_open(bare));
        for i in 0..vec.len {
            if i > 0 {
                out.push_str(", ");
            }
            let p = unsafe { vec.ptr.add((i as usize) * (vec.elem_bytes as usize)) };
            let n = unsafe { (p as *const f64).read_unaligned() };
            out.push_str(&crate::builtins::format_float_debug(n));
        }
        out.push(']');
        alloc_cstring(out.as_bytes())
    })
}

/// Renders a `bool`-elem `Vec` as `[true, false, …]`. Returns a
/// fresh String pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_format_bool(v: *const GosVec, bare: i32) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if v.is_null() {
            return alloc_cstring(seq_empty(bare).as_bytes());
        }
        let vec = unsafe { &*v };
        let mut out = String::with_capacity(2 + (vec.len as usize) * 6);
        out.push_str(seq_open(bare));
        for i in 0..vec.len {
            if i > 0 {
                out.push_str(", ");
            }
            let p = unsafe { vec.ptr.add((i as usize) * (vec.elem_bytes as usize)) };
            let b = unsafe { *p } != 0;
            out.push_str(if b { "true" } else { "false" });
        }
        out.push(']');
        alloc_cstring(out.as_bytes())
    })
}

/// Renders a `char`-elem `Vec` as `[c0, c1, …]`. Elements occupy a
/// full slot each and hold the scalar value's code point. Returns a
/// fresh String pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_format_char(v: *const GosVec, bare: i32) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if v.is_null() {
            return alloc_cstring(seq_empty(bare).as_bytes());
        }
        let vec = unsafe { &*v };
        let mut out = String::with_capacity(2 + (vec.len as usize) * 3);
        out.push_str(seq_open(bare));
        for i in 0..vec.len {
            if i > 0 {
                out.push_str(", ");
            }
            let word = unsafe { crate::c_abi::vec::vec_elem_load_i64(vec, i) };
            if let Some(c) = char::from_u32(word as u32) {
                out.push(c);
            }
        }
        out.push(']');
        alloc_cstring(out.as_bytes())
    })
}

/// Renders an aggregate-elem `Vec` as `[e0, e1, …]` by calling the element
/// type's derived `fmt` on each element. Elements are stored inline, so
/// element `i` begins at `ptr + i * elem_bytes`. A struct's `fmt` reads its
/// fields from that address (`by_ref`); an inline enum's `fmt` decodes the
/// element word itself, so that word is loaded and passed instead.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_format_adt(
    v: *const GosVec,
    fmt: *const std::ffi::c_void,
    by_ref: i32,
    bare: i32,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if v.is_null() || fmt.is_null() {
            return alloc_cstring(seq_empty(bare).as_bytes());
        }
        let vec = unsafe { &*v };
        let mut out = String::with_capacity(2 + (vec.len as usize) * 16);
        out.push_str(seq_open(bare));
        for i in 0..vec.len {
            if i > 0 {
                out.push_str(", ");
            }
            let slot = unsafe { vec.ptr.add((i as usize) * (vec.elem_bytes as usize)) };
            let arg = if by_ref != 0 {
                slot
            } else {
                unsafe { crate::c_abi::vec::slot_read_word(slot) }.cast_const()
            };
            out.push_str(&unsafe { crate::c_abi::vec::adt_fmt_string(arg, fmt) });
        }
        out.push(']');
        alloc_cstring(out.as_bytes())
    })
}

/// Renders a `Vec` whose elements are described by the descriptor at `desc`
/// inside `tags`, so a nested element shape renders through the same walk.
///
/// # Safety
/// `v` is a live `GosVec` and `tags` addresses a descriptor at `desc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_format_desc(
    v: *const GosVec,
    tags: *const u8,
    desc: i64,
    bare: i32,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if v.is_null() || tags.is_null() {
            return alloc_cstring(seq_empty(bare).as_bytes());
        }
        let tags = unsafe { crate::c_abi::map::DescStream::new(tags) };
        let vec = unsafe { &*v };
        // An aggregate element is stored inline whatever its width; a
        // one-word element that is not one is the value itself or the
        // handle addressing it.
        let storage = if crate::c_abi::vec::vec_elem_is_inline_aggregate(vec) {
            crate::c_abi::map::Storage::Inline
        } else {
            crate::c_abi::map::Storage::ByWord
        };
        let mut out = String::with_capacity(2 + (vec.len as usize) * 16);
        out.push_str(seq_open(bare));
        for i in 0..vec.len {
            if i > 0 {
                out.push_str(", ");
            }
            let slot = unsafe { vec.ptr.add((i as usize) * (vec.elem_bytes as usize)) };
            let mut cursor = desc as usize;
            unsafe {
                crate::c_abi::map::render_desc_storage(&mut out, slot, tags, &mut cursor, storage);
            }
        }
        out.push(']');
        alloc_cstring(out.as_bytes())
    })
}

/// Renders a map-elem `Vec` as `[{k: v}, …]`. Each element is a `GosMap`
/// handle word, rendered by the same formatter a bare `{:?}` on the map uses.
///
/// # Safety
/// `v` is a live `GosVec` whose elements are `GosMap` handles.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_format_map(v: *const GosVec, bare: i32) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if v.is_null() {
            return alloc_cstring(seq_empty(bare).as_bytes());
        }
        let vec = unsafe { &*v };
        let mut out = String::with_capacity(2 + (vec.len as usize) * 16);
        out.push_str(seq_open(bare));
        for i in 0..vec.len {
            if i > 0 {
                out.push_str(", ");
            }
            let slot = unsafe { vec.ptr.add((i as usize) * (vec.elem_bytes as usize)) };
            let child = unsafe { crate::c_abi::vec::slot_read_word(slot) };
            let rendered = unsafe { crate::c_abi::gos_rt_map_format(child.cast_const().cast()) };
            if !rendered.is_null() {
                out.push_str(&unsafe { crate::c_abi::gos_str_arg_lossy(rendered) });
                // The formatter answered a fresh rendering whose bytes are now copied.
                unsafe { crate::c_abi::string::gos_rt_str_free(rendered) };
            }
        }
        out.push(']');
        alloc_cstring(out.as_bytes())
    })
}

/// Renders a tuple-elem `Vec` as `[(a, b), …]`. Each element occupies the
/// Vec's element stride as a flat slot buffer, rendered through the same
/// per-element tag array `gos_rt_tuple_format` takes.
///
/// # Safety
/// `v` is a live `GosVec` whose elements are tuple slot buffers, and `tags`
/// addresses at least the tag bytes those `n` elements describe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_format_tuple(
    v: *const GosVec,
    n: i64,
    tags: *const u8,
    bare: i32,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if v.is_null() || tags.is_null() || n <= 0 {
            return alloc_cstring(seq_empty(bare).as_bytes());
        }
        let vec = unsafe { &*v };
        let mut out = String::with_capacity(2 + (vec.len as usize) * 16);
        out.push_str(seq_open(bare));
        for i in 0..vec.len {
            if i > 0 {
                out.push_str(", ");
            }
            let slot = unsafe { vec.ptr.add((i as usize) * (vec.elem_bytes as usize)) };
            let mut slot_cursor = 0usize;
            let mut tag_cursor = 0usize;
            unsafe {
                crate::c_abi::map::render_tuple_elements(
                    &mut out,
                    slot.cast::<i64>(),
                    crate::c_abi::map::DescStream::bare(tags),
                    n as usize,
                    &mut slot_cursor,
                    &mut tag_cursor,
                );
            }
        }
        out.push(']');
        alloc_cstring(out.as_bytes())
    })
}

/// Renders a `String`-elem `Vec` as `[s0, s1, …]`. Each element
/// in the Vec is a NUL-terminated `*const c_char`; we read it as
/// an 8-byte word and dereference. Returns a fresh String
/// pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_format_string(v: *const GosVec, bare: i32) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if v.is_null() {
            return alloc_cstring(seq_empty(bare).as_bytes());
        }
        let vec = unsafe { &*v };
        let mut out = String::with_capacity(2 + (vec.len as usize) * 8);
        out.push_str(seq_open(bare));
        for i in 0..vec.len {
            if i > 0 {
                out.push_str(", ");
            }
            let p = unsafe { vec.ptr.add((i as usize) * (vec.elem_bytes as usize)) };
            let s_ptr = unsafe {
                crate::c_abi::vec::slot_read_word(p)
                    .cast_const()
                    .cast::<c_char>()
            };
            if !s_ptr.is_null() {
                out.push_str(&unsafe { crate::c_abi::gos_str_arg_lossy(s_ptr) });
            }
        }
        out.push(']');
        alloc_cstring(out.as_bytes())
    })
}

/// Renders a `Vec<Vec<i64>>` as `[[a, b], [c], …]`. Each
/// element is a `*mut GosVec` (8-byte slot); we recursively
/// stringify each inner `Vec<i64>`. Returns a fresh String
/// pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_format_vec_i64(v: *const GosVec, bare: i32) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if v.is_null() {
            return alloc_cstring(seq_empty(bare).as_bytes());
        }
        let vec = unsafe { &*v };
        let mut out = String::with_capacity(2 + (vec.len as usize) * 8);
        out.push_str(seq_open(bare));
        for i in 0..vec.len {
            if i > 0 {
                out.push_str(", ");
            }
            let p = unsafe { vec.ptr.add((i as usize) * (vec.elem_bytes as usize)) };
            let inner_ptr = unsafe {
                crate::c_abi::vec::slot_read_word(p)
                    .cast_const()
                    .cast::<GosVec>()
            };
            if inner_ptr.is_null() {
                out.push_str("#[]");
            } else {
                let rendered = unsafe { gos_rt_vec_format_i64(inner_ptr, 0) };
                if rendered.is_null() {
                    out.push_str("#[]");
                } else {
                    out.push_str(&unsafe { crate::c_abi::gos_str_arg_lossy(rendered) });
                    // The formatter answered a fresh rendering whose bytes are now copied.
                    unsafe { crate::c_abi::string::gos_rt_str_free(rendered) };
                }
            }
        }
        out.push(']');
        alloc_cstring(out.as_bytes())
    })
}

/// Renders a `Vec<Vec<f64>>` as `[[a, b], [c], …]`. Each element is a
/// `*mut GosVec` (8-byte slot) whose rows render through the `f64` element
/// formatter. Returns a fresh String pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_format_vec_f64(v: *const GosVec, bare: i32) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if v.is_null() {
            return alloc_cstring(seq_empty(bare).as_bytes());
        }
        let vec = unsafe { &*v };
        let mut out = String::with_capacity(2 + (vec.len as usize) * 8);
        out.push_str(seq_open(bare));
        for i in 0..vec.len {
            if i > 0 {
                out.push_str(", ");
            }
            let p = unsafe { vec.ptr.add((i as usize) * (vec.elem_bytes as usize)) };
            let inner_ptr = unsafe {
                crate::c_abi::vec::slot_read_word(p)
                    .cast_const()
                    .cast::<GosVec>()
            };
            if inner_ptr.is_null() {
                out.push_str("#[]");
            } else {
                let rendered = unsafe { gos_rt_vec_format_f64(inner_ptr, 0) };
                if rendered.is_null() {
                    out.push_str("#[]");
                } else {
                    out.push_str(&unsafe { crate::c_abi::gos_str_arg_lossy(rendered) });
                    // The formatter answered a fresh rendering whose bytes are now copied.
                    unsafe { crate::c_abi::string::gos_rt_str_free(rendered) };
                }
            }
        }
        out.push(']');
        alloc_cstring(out.as_bytes())
    })
}

/// Renders a `Vec<Vec<String>>` as `[[s0, s1], [s2], …]`. Each
/// element is a `*mut GosVec` (8-byte slot); we recursively
/// stringify each inner `Vec<String>`. Returns a fresh String
/// pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_format_vec_string(v: *const GosVec, bare: i32) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if v.is_null() {
            return alloc_cstring(seq_empty(bare).as_bytes());
        }
        let vec = unsafe { &*v };
        let mut out = String::with_capacity(2 + (vec.len as usize) * 8);
        out.push_str(seq_open(bare));
        for i in 0..vec.len {
            if i > 0 {
                out.push_str(", ");
            }
            let p = unsafe { vec.ptr.add((i as usize) * (vec.elem_bytes as usize)) };
            let inner_ptr = unsafe {
                crate::c_abi::vec::slot_read_word(p)
                    .cast_const()
                    .cast::<GosVec>()
            };
            if inner_ptr.is_null() {
                out.push_str("#[]");
            } else {
                let rendered = unsafe { gos_rt_vec_format_string(inner_ptr, 0) };
                if rendered.is_null() {
                    out.push_str("#[]");
                } else {
                    out.push_str(&unsafe { crate::c_abi::gos_str_arg_lossy(rendered) });
                    // The formatter answered a fresh rendering whose bytes are now copied.
                    unsafe { crate::c_abi::string::gos_rt_str_free(rendered) };
                }
            }
        }
        out.push(']');
        alloc_cstring(out.as_bytes())
    })
}

/// Renders a flat `[u8; N]` raw buffer as `[v0, v1, …]`. A `u8` array
/// is byte-packed rather than slot-per-element, so it reads with a
/// stride of one and cannot share [`gos_rt_arr_format_i64`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_arr_format_u8(p: *const u8, len: i64) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if p.is_null() || len <= 0 {
            return alloc_cstring(b"[]");
        }
        let len_usize = len.max(0) as usize;
        let mut out = String::with_capacity(2 + len_usize * 4);
        out.push('[');
        for i in 0..len_usize {
            if i > 0 {
                out.push_str(", ");
            }
            let n = unsafe { p.add(i).read_unaligned() };
            out.push_str(&format!("{n}"));
        }
        out.push(']');
        alloc_cstring(out.as_bytes())
    })
}

/// Renders a flat `[i64; N]` raw buffer as `[v0, v1, …]`. Used by
/// the print/format dispatch for fixed-size array literals
/// (`let xs = [a, b, c]`) whose storage is a flat heap blob, not a
/// `GosVec` with a header. Each element occupies one i64 slot
/// regardless of platform pointer width; a `[u8; N]` is byte-packed
/// instead and goes through [`gos_rt_arr_format_u8`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_arr_format_i64(p: *const i64, len: i64) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if p.is_null() || len <= 0 {
            return alloc_cstring(b"[]");
        }
        let len_usize = len.max(0) as usize;
        let mut out = String::with_capacity(2 + len_usize * 4);
        out.push('[');
        for i in 0..len_usize {
            if i > 0 {
                out.push_str(", ");
            }
            let n = unsafe { p.add(i).read_unaligned() };
            out.push_str(&format!("{n}"));
        }
        out.push(']');
        alloc_cstring(out.as_bytes())
    })
}

/// Renders a flat `[f64; N]` raw buffer. Layout: each element is
/// stored at an 8-byte stride; we read the raw word as f64.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_arr_format_f64(p: *const f64, len: i64) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if p.is_null() || len <= 0 {
            return alloc_cstring(b"[]");
        }
        let len_usize = len.max(0) as usize;
        let mut out = String::with_capacity(2 + len_usize * 6);
        out.push('[');
        for i in 0..len_usize {
            if i > 0 {
                out.push_str(", ");
            }
            let n = unsafe { p.add(i).read_unaligned() };
            out.push_str(&crate::builtins::format_float_debug(n));
        }
        out.push(']');
        alloc_cstring(out.as_bytes())
    })
}

/// Renders a flat `[bool; N]` raw buffer. Each element is one
/// 8-byte slot; the low byte is the bool.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_arr_format_bool(p: *const i64, len: i64) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if p.is_null() || len <= 0 {
            return alloc_cstring(b"[]");
        }
        let len_usize = len.max(0) as usize;
        let mut out = String::with_capacity(2 + len_usize * 6);
        out.push('[');
        for i in 0..len_usize {
            if i > 0 {
                out.push_str(", ");
            }
            let raw = unsafe { p.add(i).read_unaligned() };
            out.push_str(if raw & 1 != 0 { "true" } else { "false" });
        }
        out.push(']');
        alloc_cstring(out.as_bytes())
    })
}

/// Renders a flat `[char; N]` raw buffer. Each element is one 8-byte slot
/// holding the scalar value's code point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_arr_format_char(p: *const i64, len: i64) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if p.is_null() || len <= 0 {
            return alloc_cstring(b"[]");
        }
        let len_usize = len.max(0) as usize;
        let mut out = String::with_capacity(2 + len_usize * 3);
        out.push('[');
        for i in 0..len_usize {
            if i > 0 {
                out.push_str(", ");
            }
            let raw = unsafe { p.add(i).read_unaligned() };
            if let Some(c) = char::from_u32(raw as u32) {
                out.push(c);
            }
        }
        out.push(']');
        alloc_cstring(out.as_bytes())
    })
}

/// Renders a flat `[Adt; N]` raw buffer by calling the element type's derived
/// `fmt` on each element. Rows are inline at `stride` bytes apart; `by_ref`
/// distinguishes a struct's slot address from an enum's element word exactly
/// as in [`gos_rt_vec_format_adt`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_arr_format_adt(
    p: *const u8,
    len: i64,
    stride: i64,
    fmt: *const std::ffi::c_void,
    by_ref: i32,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if p.is_null() || len <= 0 || stride <= 0 || fmt.is_null() {
            return alloc_cstring(b"[]");
        }
        let len_usize = len as usize;
        let mut out = String::with_capacity(2 + len_usize * 16);
        out.push('[');
        for i in 0..len_usize {
            if i > 0 {
                out.push_str(", ");
            }
            let slot = unsafe { p.add(i * (stride as usize)) };
            let arg = if by_ref != 0 {
                slot
            } else {
                unsafe { crate::c_abi::vec::slot_read_word(slot) }.cast_const()
            };
            out.push_str(&unsafe { crate::c_abi::vec::adt_fmt_string(arg, fmt) });
        }
        out.push(']');
        alloc_cstring(out.as_bytes())
    })
}

/// Renders a flat `[String; N]` raw buffer. Each element is a
/// pointer to a NUL-terminated c-string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_arr_format_string(
    p: *const *const c_char,
    len: i64,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if p.is_null() || len <= 0 {
            return alloc_cstring(b"[]");
        }
        let len_usize = len.max(0) as usize;
        let mut out = String::with_capacity(2 + len_usize * 8);
        out.push('[');
        for i in 0..len_usize {
            if i > 0 {
                out.push_str(", ");
            }
            let s_ptr = unsafe { p.add(i).read_unaligned() };
            if !s_ptr.is_null() {
                out.push_str(&unsafe { crate::c_abi::gos_str_arg_lossy(s_ptr) });
            }
        }
        out.push(']');
        alloc_cstring(out.as_bytes())
    })
}

/// Renders a flat `[[i64; M]; N]` raw buffer as `[[..], [..], …]`.
/// The nested repeat/literal layout is `N * M` contiguous 8-byte
/// slots (inner arrays inline, no per-row header), so the row at
/// index `i` starts at slot `i * inner`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_arr_format_arr_i64(
    p: *const i64,
    outer: i64,
    inner: i64,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if p.is_null() || outer <= 0 || inner <= 0 {
            return alloc_cstring(b"[]");
        }
        let (outer, inner) = (outer as usize, inner as usize);
        let mut out = String::with_capacity(2 + outer * (2 + inner * 4));
        out.push('[');
        for i in 0..outer {
            if i > 0 {
                out.push_str(", ");
            }
            out.push('[');
            for j in 0..inner {
                if j > 0 {
                    out.push_str(", ");
                }
                let n = unsafe { p.add(i * inner + j).read_unaligned() };
                out.push_str(&format!("{n}"));
            }
            out.push(']');
        }
        out.push(']');
        alloc_cstring(out.as_bytes())
    })
}

/// Renders a flat `[[f64; M]; N]` raw buffer; same layout contract
/// as the i64 variant, reading each slot as an f64.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_arr_format_arr_f64(
    p: *const f64,
    outer: i64,
    inner: i64,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if p.is_null() || outer <= 0 || inner <= 0 {
            return alloc_cstring(b"[]");
        }
        let (outer, inner) = (outer as usize, inner as usize);
        let mut out = String::with_capacity(2 + outer * (2 + inner * 6));
        out.push('[');
        for i in 0..outer {
            if i > 0 {
                out.push_str(", ");
            }
            out.push('[');
            for j in 0..inner {
                if j > 0 {
                    out.push_str(", ");
                }
                let n = unsafe { p.add(i * inner + j).read_unaligned() };
                out.push_str(&crate::builtins::format_float_debug(n));
            }
            out.push(']');
        }
        out.push(']');
        alloc_cstring(out.as_bytes())
    })
}

/// Renders a flat `[[bool; M]; N]` raw buffer; same layout contract
/// as the i64 variant, each slot's low bit is the bool.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_arr_format_arr_bool(
    p: *const i64,
    outer: i64,
    inner: i64,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if p.is_null() || outer <= 0 || inner <= 0 {
            return alloc_cstring(b"[]");
        }
        let (outer, inner) = (outer as usize, inner as usize);
        let mut out = String::with_capacity(2 + outer * (2 + inner * 7));
        out.push('[');
        for i in 0..outer {
            if i > 0 {
                out.push_str(", ");
            }
            out.push('[');
            for j in 0..inner {
                if j > 0 {
                    out.push_str(", ");
                }
                let raw = unsafe { p.add(i * inner + j).read_unaligned() };
                out.push_str(if raw & 1 != 0 { "true" } else { "false" });
            }
            out.push(']');
        }
        out.push(']');
        alloc_cstring(out.as_bytes())
    })
}

/// `os::set_env(name, value) -> Result<(), errors::Error>`.
///
/// Mutates the calling process's environment so subsequently
/// spawned children inherit the new value. Routes through
/// `safe_env::set_env`, which serializes the POSIX `setenv`
/// against the rest of the runtime so concurrent goroutines
/// can't race on the env block.
///
/// MIR-side dispatch routes `os::set_env(...)` here so the
/// compiled tier matches the VM's behaviour. Without this binding
/// `os::set_env` lowered to a generic call against a non-existent
/// symbol - the compiled tier silently no-op'd, and downstream
/// `os::env(name)` returned the old value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_os_set_env(name: *const c_char, value: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        if name.is_null() {
            let err = crate::c_abi::errors::error_new_from_bytes(b"os::set_env: name is null");
            return unsafe { gos_rt_result_new(1, err as i64) };
        }
        let name_str = unsafe { crate::c_abi::gos_str_arg_string(name) };
        let value_str = if value.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(value) }
        };
        crate::safe_env::set_env(&name_str, &value_str);
        unsafe { gos_rt_result_new(0, 0) }
    })
}

/// `os::unset_env(name)` - companion to `gos_rt_os_set_env`.
/// Returns unit; failures (e.g. name with `=`) are silently
/// dropped to match the VM's lenient behaviour.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_os_unset_env(name: *const c_char) {
    ffi_entry!((), {
        if name.is_null() {
            return;
        }
        let name_str = unsafe { crate::c_abi::gos_str_arg_string(name) };
        crate::safe_env::unset_env(&name_str);
    });
}

/// `exec::spawn(prog, args) -> Result<i64, errors::Error>`.
///
/// Non-blocking sibling of `exec::run`: launches `prog` with
/// `args` in the background, redirects stdin/stdout/stderr to
/// `/dev/null` so the child detaches from the calling tty, and
/// returns the child PID immediately. Wait/kill is the caller's
/// responsibility (see `gos_rt_exec_kill`). Used by long-running
/// daemon launches (e.g. an LLM-server program a tool spawns
/// before issuing HTTP requests against it).
///
/// Ok payload is the PID as `i64`; Err payload is a `*mut
/// GosError`. The Result aggregate matches the `Result<i64,
/// errors::Error>` shape MIR pins via the sentinel-DefId Adt.
#[unsafe(no_mangle)]
#[cfg_attr(target_arch = "wasm32", allow(clippy::forget_non_drop))]
pub unsafe extern "C" fn gos_rt_exec_spawn(prog: *const c_char, args: *mut GosVec) -> i128 {
    ffi_entry!(0i128, {
        let prog_str = if prog.is_null() {
            let err = crate::c_abi::errors::error_new_from_bytes(b"exec::spawn: program is null");
            return unsafe { gos_rt_result_new(1, err as i64) };
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(prog) }
        };
        let mut cmd_args: Vec<String> = Vec::new();
        if !args.is_null() {
            let v = unsafe { &*args };
            let elem_bytes = v.elem_bytes as usize;
            if elem_bytes != 0 && !v.ptr.is_null() {
                for i in 0..v.len {
                    let slot = unsafe { v.ptr.add((i as usize) * elem_bytes) };
                    let cstr_ptr = unsafe { crate::c_abi::vec::slot_read_word(slot) }
                        .cast_const()
                        .cast::<c_char>();
                    if cstr_ptr.is_null() {
                        cmd_args.push(String::new());
                        continue;
                    }
                    let arg_str = unsafe { crate::c_abi::gos_str_arg_string(cstr_ptr) };
                    cmd_args.push(arg_str);
                }
            }
        }
        let mut command = std::process::Command::new(&prog_str);
        command.args(&cmd_args);
        command.stdin(std::process::Stdio::null());
        command.stdout(std::process::Stdio::null());
        command.stderr(std::process::Stdio::null());
        match command.spawn() {
            Ok(child) => {
                let pid = i64::from(child.id());
                // Detach: forget the Child handle so its Drop doesn't
                // wait. The user shells the kill via `gos_rt_exec_kill`
                // (or leaves the daemon running for the parent's
                // lifetime).
                std::mem::forget(child);
                unsafe { gos_rt_result_new(0, pid) }
            }
            Err(e) => {
                let msg = format!("exec::spawn({prog_str}): {e}");
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                unsafe { gos_rt_result_new(1, err as i64) }
            }
        }
    })
}

/// Sends SIGTERM (Unix) / TerminateProcess (Windows) to the PID
/// returned by `gos_rt_exec_spawn`. Companion to
/// `gos_rt_exec_spawn` for stop_server-style teardown paths.
/// Returns `true` on success, `false` if the kill syscall failed
/// (e.g. the process already exited, EPERM).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_exec_kill(pid: i64) -> i64 {
    ffi_entry!(-1, {
        if pid <= 0 {
            return 0;
        }
        #[cfg(unix)]
        {
            // SAFETY: libc::kill is safe to call with any pid /
            // signal; the kernel returns EINVAL / EPERM on failure
            // rather than crashing the caller.
            let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
            i64::from(rc == 0)
        }
        #[cfg(windows)]
        {
            // SAFETY: Win32 OpenProcess/TerminateProcess/CloseHandle.
            // CloseHandle is always called to prevent a handle leak.
            unsafe extern "system" {
                fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> isize;
                fn TerminateProcess(process: isize, exit_code: u32) -> i32;
                fn CloseHandle(object: isize) -> i32;
            }
            const PROCESS_TERMINATE: u32 = 0x0001;
            let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid as u32) };
            if handle == 0 {
                return 0;
            }
            let ok = unsafe { TerminateProcess(handle, 1) };
            unsafe { CloseHandle(handle) };
            i64::from(ok != 0)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = pid;
            0
        }
    })
}
