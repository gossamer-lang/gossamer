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

/// Writes every element of `vec` with `render`, between the brackets `bare`
/// selects; a null vec writes as empty.
fn write_elems(
    vec: Option<crate::c_abi::vec::VecView<'_>>,
    bare: i32,
    out: &mut String,
    render: impl Fn(crate::c_abi::vec::VecView<'_>, usize, &mut String),
) {
    let Some(vec) = vec else {
        out.push_str(seq_empty(bare));
        return;
    };
    out.push_str(seq_open(bare));
    for i in 0..vec.len() {
        if i > 0 {
            out.push_str(", ");
        }
        render(vec, i, out);
    }
    out.push(']');
}

/// Renders every element of the `Vec` `v` names with `render`, between the
/// brackets `bare` selects.
///
/// # Safety
/// `v` is null or a live `Vec`.
unsafe fn format_elems(
    v: *const GosVec,
    bare: i32,
    render: impl Fn(crate::c_abi::vec::VecView<'_>, usize, &mut String),
) -> *mut c_char {
    let mut out = String::new();
    // SAFETY: this `unsafe fn`'s caller passes `v` null or a live `Vec`.
    write_elems(
        unsafe { crate::c_abi::vec::VecView::of(v) },
        bare,
        &mut out,
        render,
    );
    alloc_cstring(out.as_bytes())
}

/// Writes one integer element, read at the width its vec declares.
fn int_elem(vec: crate::c_abi::vec::VecView<'_>, i: usize, out: &mut String) {
    out.push_str(&crate::builtins::format_int(vec.word(i)));
}

/// Writes one `f64` element from the bits its slot holds.
fn float_elem(vec: crate::c_abi::vec::VecView<'_>, i: usize, out: &mut String) {
    out.push_str(&crate::builtins::format_float_debug(f64::from_bits(
        vec.word(i) as u64,
    )));
}

/// Writes every element of a `Vec<String>` quoted, between the brackets `bare`
/// selects; a null vec writes as empty and an empty slot writes nothing.
fn write_strings(vec: Option<crate::c_abi::vec::StrVecView<'_>>, bare: i32, out: &mut String) {
    let Some(vec) = vec else {
        out.push_str(seq_empty(bare));
        return;
    };
    out.push_str(seq_open(bare));
    for i in 0..vec.len() {
        if i > 0 {
            out.push_str(", ");
        }
        if !vec.is_null(i) {
            crate::c_abi::desc_format::push_quoted_str(out, &vec.text(i));
        }
    }
    out.push(']');
}

/// Renders an integer `Vec` as `[v0, v1, …]`, each element read at the width
/// the header declares. Returns a fresh String pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_format_i64(v: *const GosVec, bare: i32) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        // SAFETY: `v` is this shim's argument, null or a live `Vec` (C-ABI contract).
        unsafe { format_elems(v, bare, int_elem) }
    })
}

/// Renders a `u64`-elem `Vec` as `[v0, v1, …]`. A slot holds the value's
/// bits, so an element at or above `i64::MAX` reads as its unsigned decimal
/// rather than the negative the same bits spell. Returns a fresh String
/// pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_format_u64(v: *const GosVec, bare: i32) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        // SAFETY: `v` is this shim's argument, null or a live `Vec` (C-ABI contract).
        unsafe {
            format_elems(v, bare, |vec, i, out| {
                out.push_str(&crate::builtins::format_uint(vec.word(i) as u64));
            })
        }
    })
}

/// Renders an `f64`-elem `Vec` as `[v0, v1, …]`. Returns a fresh
/// String pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_format_f64(v: *const GosVec, bare: i32) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        // SAFETY: `v` is this shim's argument, null or a live `Vec` (C-ABI contract).
        unsafe { format_elems(v, bare, float_elem) }
    })
}

/// Renders a `bool`-elem `Vec` as `[true, false, …]`. Returns a
/// fresh String pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_format_bool(v: *const GosVec, bare: i32) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        // SAFETY: `v` is this shim's argument, null or a live `Vec` (C-ABI contract).
        unsafe {
            format_elems(v, bare, |vec, i, out| {
                out.push_str(if vec.elem(i)[0] != 0 { "true" } else { "false" });
            })
        }
    })
}

/// Renders a `char`-elem `Vec` as `[c0, c1, …]`. Elements occupy a
/// full slot each and hold the scalar value's code point. Returns a
/// fresh String pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_format_char(v: *const GosVec, bare: i32) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        // SAFETY: `v` is this shim's argument, null or a live `Vec` (C-ABI contract).
        unsafe {
            format_elems(v, bare, |vec, i, out| {
                crate::c_abi::desc_format::push_quoted_char(out, vec.word(i));
            })
        }
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
        if fmt.is_null() {
            return alloc_cstring(seq_empty(bare).as_bytes());
        }
        let mut out = String::new();
        // SAFETY: `v` is this shim's argument, null or a live `Vec` (C-ABI contract).
        let elems = unsafe { crate::c_abi::vec::VecView::of(v) };
        write_elems(elems, bare, &mut out, |elems, i, out| {
            let arg = if by_ref == 0 {
                elems.pointer_at::<u8>(i)
            } else {
                elems.elem(i).as_ptr()
            };
            // SAFETY: `fmt` is non-null (checked above) and the compiled formatter of the element
            // type, and `arg` is the element in the form `by_ref` names (C-ABI contract).
            out.push_str(&unsafe { crate::c_abi::result::adt_fmt_string(arg, fmt) });
        });
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
        if tags.is_null() {
            return alloc_cstring(seq_empty(bare).as_bytes());
        }
        // SAFETY: `tags` is this shim's argument, live for the call (C-ABI contract); non-null,
        // checked above.
        let tags = unsafe { crate::c_abi::desc_format::DescStream::new(tags) };
        // SAFETY: `v` is this shim's argument, null or a live `Vec` (C-ABI contract).
        let elems = unsafe { crate::c_abi::vec::VecView::of(v) };
        let mut out = String::new();
        write_elems(elems, bare, &mut out, |elems, i, out| {
            // An aggregate element is stored inline whatever its width; a one-word element that
            // is not one is the value itself or the handle addressing it.
            let storage = if crate::c_abi::vec::vec_elem_is_inline_aggregate(elems.header()) {
                crate::c_abi::desc_format::Storage::Inline
            } else {
                crate::c_abi::desc_format::Storage::ByWord
            };
            let mut cursor = desc as usize;
            // SAFETY: the element is stored as `storage` names and laid out as the descriptor at
            // `desc` describes (C-ABI contract).
            unsafe {
                crate::c_abi::desc_format::render_desc_storage(
                    out,
                    elems.elem(i).as_ptr(),
                    tags,
                    &mut cursor,
                    storage,
                );
            }
        });
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
        // SAFETY: `v` is this shim's argument, null or a live `Vec<Map>` (C-ABI contract).
        let elems = unsafe { crate::c_abi::vec::VecView::of(v) };
        let mut out = String::new();
        write_elems(elems, bare, &mut out, |elems, i, out| {
            let child = elems.pointer_at::<crate::c_abi::GosMap>(i);
            // SAFETY: a `Vec<Map>` element is null or a live map (C-ABI contract), which
            // `gos_rt_map_format` accepts.
            let rendered = unsafe { crate::c_abi::gos_rt_map_format(child) };
            if !rendered.is_null() {
                // SAFETY: the formatter answered a fresh non-null rendering (checked above).
                out.push_str(&unsafe { crate::c_abi::gos_str_arg_lossy(rendered) });
                // SAFETY: the rendering is owned here, its text copied, and not read again.
                unsafe { crate::c_abi::string::gos_rt_str_free(rendered) };
            }
        });
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
        if tags.is_null() || n <= 0 {
            return alloc_cstring(seq_empty(bare).as_bytes());
        }
        // SAFETY: `v` is this shim's argument, null or a live `Vec` (C-ABI contract).
        let elems = unsafe { crate::c_abi::vec::VecView::of(v) };
        let mut out = String::new();
        write_elems(elems, bare, &mut out, |elems, i, out| {
            let mut slot_cursor = 0usize;
            let mut tag_cursor = 0usize;
            // SAFETY: the element is a tuple of `n` fields laid out as `tags` describes (C-ABI
            // contract).
            unsafe {
                crate::c_abi::desc_format::render_tuple_elements(
                    out,
                    elems.elem(i).as_ptr().cast::<i64>(),
                    crate::c_abi::desc_format::DescStream::bare(tags),
                    n as usize,
                    &mut slot_cursor,
                    &mut tag_cursor,
                );
            }
        });
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
        let mut out = String::new();
        // SAFETY: `v` is this shim's argument, null or a live `Vec<String>` (C-ABI contract).
        write_strings(
            unsafe { crate::c_abi::vec::StrVecView::of(v) },
            bare,
            &mut out,
        );
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
        let mut out = String::new();
        // SAFETY: `v` is this shim's argument, null or a live `Vec` whose elements are `Vec`s
        // (C-ABI contract).
        let rows = unsafe { crate::c_abi::vec::VecView::of(v) };
        write_elems(rows, bare, &mut out, |rows, i, out| {
            let row = rows.pointer_at::<GosVec>(i);
            // SAFETY: an element of a `Vec<Vec<i64>>` is null or a live `Vec` (C-ABI contract).
            write_elems(
                unsafe { crate::c_abi::vec::VecView::of(row) },
                0,
                out,
                int_elem,
            );
        });
        alloc_cstring(out.as_bytes())
    })
}

/// Renders a `Vec<Vec<f64>>` as `[[a, b], [c], …]`. Each element is a
/// `*mut GosVec` (8-byte slot) whose rows render through the `f64` element
/// formatter. Returns a fresh String pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_format_vec_f64(v: *const GosVec, bare: i32) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let mut out = String::new();
        // SAFETY: `v` is this shim's argument, null or a live `Vec` whose elements are `Vec`s
        // (C-ABI contract).
        let rows = unsafe { crate::c_abi::vec::VecView::of(v) };
        write_elems(rows, bare, &mut out, |rows, i, out| {
            let row = rows.pointer_at::<GosVec>(i);
            // SAFETY: an element of a `Vec<Vec<f64>>` is null or a live `Vec` (C-ABI contract).
            write_elems(
                unsafe { crate::c_abi::vec::VecView::of(row) },
                0,
                out,
                float_elem,
            );
        });
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
        let mut out = String::new();
        // SAFETY: `v` is this shim's argument, null or a live `Vec` whose elements are `Vec`s
        // (C-ABI contract).
        let rows = unsafe { crate::c_abi::vec::VecView::of(v) };
        write_elems(rows, bare, &mut out, |rows, i, out| {
            let row = rows.pointer_at::<GosVec>(i);
            // SAFETY: an element of a `Vec<Vec<String>>` is null or a live `Vec<String>` (C-ABI
            // contract).
            write_strings(unsafe { crate::c_abi::vec::StrVecView::of(row) }, 0, out);
        });
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
            // SAFETY: `p` is non-null (checked above) and addresses `len` bytes (C-ABI contract),
            // and `i` is below `len`.
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
            // SAFETY: `p` is non-null (checked above) and addresses `len` words (C-ABI contract),
            // and `i` is below `len`.
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
            // SAFETY: `p` is non-null (checked above) and addresses `len` floats (C-ABI
            // contract), and `i` is below `len`.
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
            // SAFETY: `p` is non-null (checked above) and addresses `len` words (C-ABI contract),
            // and `i` is below `len`.
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
            // SAFETY: `p` is non-null (checked above) and addresses `len` words (C-ABI contract),
            // and `i` is below `len`.
            let raw = unsafe { p.add(i).read_unaligned() };
            crate::c_abi::desc_format::push_quoted_char(&mut out, raw);
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
            // SAFETY: `p` is non-null (checked above) and addresses `len` elements of `stride`
            // bytes (C-ABI contract), and `i` is below `len`.
            let slot = unsafe { p.add(i * (stride as usize)) };
            let arg = if by_ref != 0 {
                slot
            } else {
                // SAFETY: `slot` addresses a one-word element of the array.
                unsafe { crate::c_abi::vec::slot_read_word(slot) }.cast_const()
            };
            // SAFETY: `fmt` is non-null (checked above) and the compiled formatter of the element
            // type, and `arg` is the element in the form `by_ref` names (C-ABI contract).
            out.push_str(&unsafe { crate::c_abi::result::adt_fmt_string(arg, fmt) });
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
            // SAFETY: `p` is non-null (checked above) and addresses `len` words (C-ABI contract),
            // and `i` is below `len`.
            let s_ptr = unsafe { p.add(i).read_unaligned() };
            if !s_ptr.is_null() {
                // SAFETY: `s_ptr` is a non-null slot of a `String` array, a live string body
                // (C-ABI contract).
                crate::c_abi::desc_format::push_quoted_str(&mut out, &unsafe {
                    crate::c_abi::gos_str_arg_lossy(s_ptr)
                });
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
                // SAFETY: `p` is non-null (checked above) and addresses `outer * inner` words
                // (C-ABI contract), and `i * inner + j` is below that.
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
                // SAFETY: `p` is non-null (checked above) and addresses `outer * inner` floats
                // (C-ABI contract), and `i * inner + j` is below that.
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
                // SAFETY: `p` is non-null (checked above) and addresses `outer * inner` words
                // (C-ABI contract), and `i * inner + j` is below that.
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
            return gos_rt_result_new(1, err as i64);
        }
        // SAFETY: `name` is a String argument from compiled code, null or a live string body for the whole call.
        let name_str = unsafe { crate::c_abi::gos_str_arg_string(name) };
        let value_str = if value.is_null() {
            String::new()
        } else {
            // SAFETY: `value` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_string(value) }
        };
        crate::safe_env::set_env(&name_str, &value_str);
        gos_rt_result_new(0, 0)
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
        // SAFETY: `name` is a String argument from compiled code, null or a live string body for the whole call.
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
            return gos_rt_result_new(1, err as i64);
        } else {
            // SAFETY: `prog` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_string(prog) }
        };
        // SAFETY: `args` is this shim's argument, null or a live `Vec<String>` for the call
        // (C-ABI contract), which `argv_strings` accepts.
        let cmd_args = unsafe { crate::c_abi::exec::argv_strings(args) };
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
                gos_rt_result_new(0, pid)
            }
            Err(e) => {
                let msg = format!("exec::spawn({prog_str}): {e}");
                let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
                gos_rt_result_new(1, err as i64)
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
            unsafe extern "system" {
                fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> isize;
                fn TerminateProcess(process: isize, exit_code: u32) -> i32;
                fn CloseHandle(object: isize) -> i32;
            }
            const PROCESS_TERMINATE: u32 = 0x0001;
            // SAFETY: `OpenProcess` takes plain values and answers a handle or zero.
            let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid as u32) };
            if handle == 0 {
                return 0;
            }
            // SAFETY: `handle` is the non-zero process handle opened above.
            let ok = unsafe { TerminateProcess(handle, 1) };
            // SAFETY: `handle` is the process handle opened above, closed once here.
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
