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

use std::ffi::c_char;

use super::*;

// ---------------------------------------------------------------
// container::heap - min-heap operations over `Vec<i64>` (in-place
// sift up/down). Pair with `Vec::len` to detect empty before
// peek/pop; the sentinel return (0) on empty is documented but the
// caller is expected to check length.
// ---------------------------------------------------------------

unsafe fn heap_sift_up_i64(buf: *mut i64, start_i: usize) {
    let mut i = start_i;
    while i > 0 {
        let parent = (i - 1) / 2;
        // SAFETY: this `unsafe fn`'s caller passes `buf` a heap buffer holding at least `start_i
        // + 1` words, and `parent < i <= start_i`.
        let parent_v = unsafe { *buf.add(parent) };
        // SAFETY: this `unsafe fn`'s caller passes `buf` a heap buffer holding at least `start_i
        // + 1` words, and `parent < i <= start_i`.
        let cur_v = unsafe { *buf.add(i) };
        if parent_v > cur_v {
            // SAFETY: this `unsafe fn`'s caller passes `buf` a heap buffer holding at least
            // `start_i + 1` words, and `parent < i <= start_i`.
            unsafe { std::ptr::swap(buf.add(parent), buf.add(i)) };
            i = parent;
        } else {
            break;
        }
    }
}

unsafe fn heap_sift_down_i64(buf: *mut i64, len: usize, start_i: usize) {
    let mut i = start_i;
    loop {
        let l = 2 * i + 1;
        let r = 2 * i + 2;
        let mut smallest = i;
        // SAFETY: this `unsafe fn`'s caller passes `buf` a heap buffer of `len` words; the index
        // is below `len` (checked in the condition).
        if l < len && unsafe { *buf.add(l) } < unsafe { *buf.add(smallest) } {
            smallest = l;
        }
        // SAFETY: this `unsafe fn`'s caller passes `buf` a heap buffer of `len` words; the index
        // is below `len` (checked in the condition).
        if r < len && unsafe { *buf.add(r) } < unsafe { *buf.add(smallest) } {
            smallest = r;
        }
        if smallest == i {
            break;
        }
        // SAFETY: both indices are below `len`, inside the buffer.
        unsafe { std::ptr::swap(buf.add(smallest), buf.add(i)) };
        i = smallest;
    }
}

unsafe fn max_heap_sift_up_i64(buf: *mut i64, start_i: usize) {
    let mut i = start_i;
    while i > 0 {
        let parent = (i - 1) / 2;
        // SAFETY: this `unsafe fn`'s caller passes `buf` a heap buffer holding at least `start_i
        // + 1` words, and `parent < i <= start_i`.
        let parent_v = unsafe { *buf.add(parent) };
        // SAFETY: this `unsafe fn`'s caller passes `buf` a heap buffer holding at least `start_i
        // + 1` words, and `parent < i <= start_i`.
        let cur_v = unsafe { *buf.add(i) };
        if parent_v < cur_v {
            // SAFETY: this `unsafe fn`'s caller passes `buf` a heap buffer holding at least
            // `start_i + 1` words, and `parent < i <= start_i`.
            unsafe { std::ptr::swap(buf.add(parent), buf.add(i)) };
            i = parent;
        } else {
            break;
        }
    }
}

unsafe fn max_heap_sift_down_i64(buf: *mut i64, len: usize, start_i: usize) {
    let mut i = start_i;
    loop {
        let l = 2 * i + 1;
        let r = 2 * i + 2;
        let mut largest = i;
        // SAFETY: this `unsafe fn`'s caller passes `buf` a heap buffer of `len` words; the index
        // is below `len` (checked in the condition).
        if l < len && unsafe { *buf.add(l) } > unsafe { *buf.add(largest) } {
            largest = l;
        }
        // SAFETY: this `unsafe fn`'s caller passes `buf` a heap buffer of `len` words; the index
        // is below `len` (checked in the condition).
        if r < len && unsafe { *buf.add(r) } > unsafe { *buf.add(largest) } {
            largest = r;
        }
        if largest == i {
            break;
        }
        // SAFETY: both indices are below `len`, inside the buffer.
        unsafe { std::ptr::swap(buf.add(largest), buf.add(i)) };
        i = largest;
    }
}

/// Replaces the heap a by-value aggregate field holds with its own store.
///
/// `slot` is the field's storage address. A heap IS a `GosVec`, but its push
/// and pop write the store in place rather than through a copy-on-write, so a
/// copied field takes a store of its own the way a heap binding does rather
/// than a share of the same one. Null-safe.
///
/// # Safety
/// `slot` addresses a `*mut GosVec` field, or is null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_field_clone(slot: *mut *mut GosVec) {
    ffi_entry!({
        if slot.is_null() {
            return;
        }
        // SAFETY: `slot` is this shim's field argument, non-null (checked above), holding a heap
        // handle (C-ABI contract).
        let v = unsafe { slot.read_unaligned() };
        if v.is_null() {
            return;
        }
        // SAFETY: `v` is non-null (checked above) and the field's live heap vec.
        let cloned = unsafe { crate::c_abi::string::gos_rt_vec_clone(v) };
        // SAFETY: `slot` is the field read just above.
        unsafe { slot.write_unaligned(cloned) };
    });
}

/// Frees the heap a by-value aggregate field owns and nulls the slot.
///
/// Nulling makes the release idempotent, so the drop pass may book it at more
/// than one exit edge of the same field without the second booking touching a
/// freed store. Null-safe.
///
/// # Safety
/// `slot` addresses a `*mut GosVec` field, or is null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_field_release(slot: *mut *mut GosVec) {
    ffi_entry!({
        if slot.is_null() {
            return;
        }
        // SAFETY: `slot` is this shim's field argument, non-null (checked above), holding a heap
        // handle (C-ABI contract).
        let v = unsafe { slot.read_unaligned() };
        if v.is_null() {
            return;
        }
        // SAFETY: `slot` is the field read just above.
        unsafe { slot.write_unaligned(std::ptr::null_mut()) };
        // SAFETY: `v` is non-null (checked above), the field's own heap vec, which it held alone.
        unsafe { crate::c_abi::map::gos_rt_vec_free(v) };
    });
}

/// A word-wide copy of `v`'s elements, the layout the scalar heaps sift over.
/// A byte-packed `Vec<u8>` or `Vec<bool>` widens each element to a word.
///
/// # Safety
/// `v` is null or a live `GosVec`.
unsafe fn heap_words_from(v: *mut GosVec) -> *mut GosVec {
    // SAFETY: this `unsafe fn`'s caller passes `v` null or a live `Vec`.
    let Some(src) = (unsafe { crate::c_abi::vec::VecView::of(v) }) else {
        return gos_rt_vec_new(8);
    };
    if src.width() == 8 {
        // SAFETY: `v` is non-null here and live (this `unsafe fn`'s contract).
        return unsafe { gos_rt_vec_clone(v) };
    }
    let out = crate::c_abi::vec::gos_rt_vec_with_capacity(8, src.header().len.max(0));
    for word in src.words() {
        // SAFETY: `out` is the fresh word vec made above, or null, which `gos_rt_vec_push_i64`
        // accepts.
        unsafe { crate::c_abi::vec::gos_rt_vec_push_i64(out, word) };
    }
    out
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_max_new_i64() -> *mut GosVec {
    ffi_entry!({ gos_rt_vec_new(8) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_max_from_vec_i64(v: *mut GosVec) -> *mut GosVec {
    ffi_entry!({
        // SAFETY: `v` is this shim's argument, null or live for the call (C-ABI contract).
        let heap = unsafe { heap_words_from(v) };
        // SAFETY: `heap` is the fresh word vec `heap_words_from` made.
        let vec = unsafe { &*heap };
        let len = vec.len as usize;
        if len > 1 {
            let buf = vec.ptr.cast::<i64>();
            for i in (0..=(len / 2)).rev() {
                // SAFETY: `buf` holds `len` words, and `i <= len / 2` is below `len`.
                unsafe { max_heap_sift_down_i64(buf, len, i) };
            }
        }
        heap
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_max_push_i64(v: *mut GosVec, value: i64) {
    ffi_entry!({
        if v.is_null() {
            return;
        }
        // SAFETY: `v` is non-null (checked above) and this shim's live heap argument (C-ABI contract).
        unsafe { gos_rt_vec_push_i64(v, value) };
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        let vec = unsafe { &*v };
        let len = vec.len as usize;
        if len > 1 {
            // SAFETY: `vec` holds `len` words after the push, and `len - 1` is the new element.
            unsafe { max_heap_sift_up_i64(vec.ptr.cast::<i64>(), len - 1) };
        }
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_max_pop_i64(v: *mut GosVec) -> i128 {
    ffi_entry!({
        if v.is_null() {
            return super::result::pack_result(1, 0);
        }
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        let vec = unsafe { &mut *v };
        if vec.len <= 0 {
            return super::result::pack_result(1, 0);
        }
        let buf = vec.ptr.cast::<i64>();
        // SAFETY: `vec` is non-null with `len > 0` (checked above), so the buffer holds its root
        // word.
        let root = unsafe { *buf };
        let last_idx = (vec.len - 1) as usize;
        if last_idx > 0 {
            // SAFETY: `last_idx` is below the length, inside the buffer.
            unsafe { *buf = *buf.add(last_idx) };
        }
        vec.len -= 1;
        let new_len = vec.len as usize;
        if new_len > 1 {
            // SAFETY: `buf` holds `new_len` words.
            unsafe { max_heap_sift_down_i64(buf, new_len, 0) };
        }
        super::result::pack_result(0, root)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_max_peek_i64(v: *const GosVec) -> i128 {
    ffi_entry!({
        if v.is_null() {
            return super::result::pack_result(1, 0);
        }
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        let vec = unsafe { &*v };
        if vec.len <= 0 {
            return super::result::pack_result(1, 0);
        }
        // SAFETY: `vec` is non-null with `len > 0` (checked above), so its buffer holds the root
        // word.
        super::result::pack_result(0, unsafe { *vec.ptr.cast::<i64>() })
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_min_new_i64() -> *mut GosVec {
    ffi_entry!({ gos_rt_vec_new(8) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_min_from_vec_i64(v: *mut GosVec) -> *mut GosVec {
    ffi_entry!({
        // SAFETY: `v` is this shim's argument, null or live for the call (C-ABI contract).
        let heap = unsafe { heap_words_from(v) };
        // SAFETY: `heap` is the fresh word vec `heap_words_from` made.
        let vec = unsafe { &*heap };
        let len = vec.len as usize;
        if len > 1 {
            let buf = vec.ptr.cast::<i64>();
            for i in (0..=(len / 2)).rev() {
                // SAFETY: `buf` holds `len` words, and `i <= len / 2` is below `len`.
                unsafe { heap_sift_down_i64(buf, len, i) };
            }
        }
        heap
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_min_push_i64(v: *mut GosVec, value: i64) {
    ffi_entry!({
        if v.is_null() {
            return;
        }
        // SAFETY: `v` is non-null (checked above) and this shim's live heap argument (C-ABI
        // contract).
        unsafe { gos_rt_vec_push_i64(v, value) };
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        let vec = unsafe { &*v };
        let len = vec.len as usize;
        if len > 1 {
            // SAFETY: `vec` holds `len` words after the push, and `len - 1` is the new element.
            unsafe { heap_sift_up_i64(vec.ptr.cast::<i64>(), len - 1) };
        }
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_min_pop_i64(v: *mut GosVec) -> i128 {
    ffi_entry!({
        if v.is_null() {
            return super::result::pack_result(1, 0);
        }
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        let vec = unsafe { &mut *v };
        if vec.len <= 0 {
            return super::result::pack_result(1, 0);
        }
        let buf = vec.ptr.cast::<i64>();
        // SAFETY: `vec` is non-null with `len > 0` (checked above), so the buffer holds its root
        // word.
        let root = unsafe { *buf };
        let last_idx = (vec.len - 1) as usize;
        if last_idx > 0 {
            // SAFETY: `last_idx` is below the length, inside the buffer.
            unsafe { *buf = *buf.add(last_idx) };
        }
        vec.len -= 1;
        let new_len = vec.len as usize;
        if new_len > 1 {
            // SAFETY: `buf` holds `new_len` words.
            unsafe { heap_sift_down_i64(buf, new_len, 0) };
        }
        super::result::pack_result(0, root)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_min_peek_i64(v: *const GosVec) -> i128 {
    ffi_entry!({
        if v.is_null() {
            return super::result::pack_result(1, 0);
        }
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        let vec = unsafe { &*v };
        if vec.len <= 0 {
            return super::result::pack_result(1, 0);
        }
        // SAFETY: `vec` is non-null with `len > 0` (checked above), so its buffer holds the root
        // word.
        super::result::pack_result(0, unsafe { *vec.ptr.cast::<i64>() })
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_is_empty(v: *const GosVec) -> i32 {
    ffi_entry!({
        if v.is_null() {
            return 1;
        }
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        i32::from(unsafe { (*v).len <= 0 })
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_clear(v: *mut GosVec) {
    ffi_entry!({
        if v.is_null() {
            return;
        }
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        unsafe { (*v).len = 0 };
    });
}

/// Renders `owner [a, b, c]` over the heap array's own order. Both tiers
/// run the same sift routines, so that order is the same sequence of
/// pushes and pops on either.
unsafe fn bheap_format(v: *const GosVec, owner: &str) -> *mut c_char {
    let mut out = String::from(owner);
    out.push_str(" [");
    if !v.is_null() {
        // SAFETY: `v` is non-null (checked above) and, per this `unsafe fn`'s caller, a live heap
        // vec.
        let vec = unsafe { &*v };
        let buf = vec.ptr.cast::<i64>();
        for i in 0..vec.len.max(0) as usize {
            if i > 0 {
                out.push_str(", ");
            }
            // SAFETY: `i` is below the vec's length.
            out.push_str(&crate::builtins::format_int(unsafe { *buf.add(i) }));
        }
    }
    out.push(']');
    crate::c_abi::string::alloc_cstring(out.as_bytes())
}

/// Format a `MaxHeap` for `{}` / `{:?}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_max_format(v: *const GosVec) -> *mut c_char {
    ffi_entry!({
        // SAFETY: `v` is this shim's heap argument, null or live (C-ABI contract).
        unsafe { bheap_format(v, "MaxHeap") }
    })
}

/// Format a `MinHeap` for `{}` / `{:?}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_min_format(v: *const GosVec) -> *mut c_char {
    ffi_entry!({
        // SAFETY: `v` is this shim's heap argument, null or live (C-ABI contract).
        unsafe { bheap_format(v, "MinHeap") }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_len(v: *const GosVec) -> i64 {
    ffi_entry!({
        if v.is_null() {
            return 0;
        }
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        unsafe { (*v).len }
    })
}

// ---------------------------------------------------------------
// Float elements. A slot holds the `f64`'s bit pattern, so the sift
// compares the value those bits spell rather than the bits' integer
// order (which reverses across the sign bit). Peek reads the root
// without comparing, so the integer entry points serve both.
// ---------------------------------------------------------------

fn slot_as_f64(bits: i64) -> f64 {
    f64::from_bits(bits as u64)
}

unsafe fn heap_sift_up_f64(buf: *mut i64, start_i: usize) {
    let mut i = start_i;
    while i > 0 {
        let parent = (i - 1) / 2;
        // SAFETY: this `unsafe fn`'s caller passes `buf` a heap buffer holding at least `start_i
        // + 1` words, and `parent < i <= start_i`.
        let parent_v = slot_as_f64(unsafe { *buf.add(parent) });
        // SAFETY: this `unsafe fn`'s caller passes `buf` a heap buffer holding at least `start_i
        // + 1` words, and `parent < i <= start_i`.
        let cur_v = slot_as_f64(unsafe { *buf.add(i) });
        if parent_v > cur_v {
            // SAFETY: this `unsafe fn`'s caller passes `buf` a heap buffer holding at least
            // `start_i + 1` words, and `parent < i <= start_i`.
            unsafe { std::ptr::swap(buf.add(parent), buf.add(i)) };
            i = parent;
        } else {
            break;
        }
    }
}

unsafe fn heap_sift_down_f64(buf: *mut i64, len: usize, start_i: usize) {
    let mut i = start_i;
    loop {
        let l = 2 * i + 1;
        let r = 2 * i + 2;
        let mut smallest = i;
        if l < len
            // SAFETY: this `unsafe fn`'s caller passes `buf` a heap buffer of `len` words; the
            // index is below `len` (checked in the condition).
            && slot_as_f64(unsafe { *buf.add(l) }) < slot_as_f64(unsafe { *buf.add(smallest) })
        {
            smallest = l;
        }
        if r < len
            // SAFETY: this `unsafe fn`'s caller passes `buf` a heap buffer of `len` words; the
            // index is below `len` (checked in the condition).
            && slot_as_f64(unsafe { *buf.add(r) }) < slot_as_f64(unsafe { *buf.add(smallest) })
        {
            smallest = r;
        }
        if smallest == i {
            break;
        }
        // SAFETY: both indices are below `len`, inside the buffer.
        unsafe { std::ptr::swap(buf.add(smallest), buf.add(i)) };
        i = smallest;
    }
}

unsafe fn max_heap_sift_up_f64(buf: *mut i64, start_i: usize) {
    let mut i = start_i;
    while i > 0 {
        let parent = (i - 1) / 2;
        // SAFETY: this `unsafe fn`'s caller passes `buf` a heap buffer holding at least `start_i
        // + 1` words, and `parent < i <= start_i`.
        let parent_v = slot_as_f64(unsafe { *buf.add(parent) });
        // SAFETY: this `unsafe fn`'s caller passes `buf` a heap buffer holding at least `start_i
        // + 1` words, and `parent < i <= start_i`.
        let cur_v = slot_as_f64(unsafe { *buf.add(i) });
        if parent_v < cur_v {
            // SAFETY: this `unsafe fn`'s caller passes `buf` a heap buffer holding at least
            // `start_i + 1` words, and `parent < i <= start_i`.
            unsafe { std::ptr::swap(buf.add(parent), buf.add(i)) };
            i = parent;
        } else {
            break;
        }
    }
}

unsafe fn max_heap_sift_down_f64(buf: *mut i64, len: usize, start_i: usize) {
    let mut i = start_i;
    loop {
        let l = 2 * i + 1;
        let r = 2 * i + 2;
        let mut largest = i;
        if l < len
            // SAFETY: this `unsafe fn`'s caller passes `buf` a heap buffer of `len` words; the
            // index is below `len` (checked in the condition).
            && slot_as_f64(unsafe { *buf.add(l) }) > slot_as_f64(unsafe { *buf.add(largest) })
        {
            largest = l;
        }
        if r < len
            // SAFETY: this `unsafe fn`'s caller passes `buf` a heap buffer of `len` words; the
            // index is below `len` (checked in the condition).
            && slot_as_f64(unsafe { *buf.add(r) }) > slot_as_f64(unsafe { *buf.add(largest) })
        {
            largest = r;
        }
        if largest == i {
            break;
        }
        // SAFETY: both indices are below `len`, inside the buffer.
        unsafe { std::ptr::swap(buf.add(largest), buf.add(i)) };
        i = largest;
    }
}

/// Heapifies a `Vec<f64>` snapshot into a max-heap over the float values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_max_from_vec_f64(v: *mut GosVec) -> *mut GosVec {
    ffi_entry!({
        let heap = if v.is_null() {
            gos_rt_vec_new(8)
        } else {
            // SAFETY: `v` is this shim's argument, live for the call (C-ABI contract) or null,
            // which `gos_rt_vec_clone` accepts.
            unsafe { gos_rt_vec_clone(v) }
        };
        // SAFETY: `heap` is the fresh vec made above: a copy of `v`, or empty.
        let vec = unsafe { &*heap };
        let len = vec.len as usize;
        if len > 1 {
            let buf = vec.ptr.cast::<i64>();
            for i in (0..=(len / 2)).rev() {
                // SAFETY: `buf` holds `len` words, and `i <= len / 2` is below `len`.
                unsafe { max_heap_sift_down_f64(buf, len, i) };
            }
        }
        heap
    })
}

/// Pushes a float onto a max-heap, keeping the greatest value at the root.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_max_push_f64(v: *mut GosVec, value: f64) {
    ffi_entry!({
        if v.is_null() {
            return;
        }
        // SAFETY: `v` is non-null (checked above) and this shim's live heap argument (C-ABI
        // contract).
        unsafe { gos_rt_vec_push_i64(v, value.to_bits() as i64) };
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        let vec = unsafe { &*v };
        let len = vec.len as usize;
        if len > 1 {
            // SAFETY: `vec` holds `len` words after the push, and `len - 1` is the new element.
            unsafe { max_heap_sift_up_f64(vec.ptr.cast::<i64>(), len - 1) };
        }
    });
}

/// Removes and returns the greatest float as `Option<f64>` bits packed into
/// an i128 (disc=0 `Some`, disc=1 `None`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_max_pop_f64(v: *mut GosVec) -> i128 {
    ffi_entry!({
        if v.is_null() {
            return super::result::pack_result(1, 0);
        }
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        let vec = unsafe { &mut *v };
        if vec.len <= 0 {
            return super::result::pack_result(1, 0);
        }
        let buf = vec.ptr.cast::<i64>();
        // SAFETY: `vec` is non-null with `len > 0` (checked above), so the buffer holds its root
        // word.
        let root = unsafe { *buf };
        let last_idx = (vec.len - 1) as usize;
        if last_idx > 0 {
            // SAFETY: `last_idx` is below the length, inside the buffer.
            unsafe { *buf = *buf.add(last_idx) };
        }
        vec.len -= 1;
        let new_len = vec.len as usize;
        if new_len > 1 {
            // SAFETY: `buf` holds `new_len` words.
            unsafe { max_heap_sift_down_f64(buf, new_len, 0) };
        }
        super::result::pack_result(0, root)
    })
}

/// Heapifies a `Vec<f64>` snapshot into a min-heap over the float values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_min_from_vec_f64(v: *mut GosVec) -> *mut GosVec {
    ffi_entry!({
        let heap = if v.is_null() {
            gos_rt_vec_new(8)
        } else {
            // SAFETY: `v` is this shim's argument, live for the call (C-ABI contract) or null,
            // which `gos_rt_vec_clone` accepts.
            unsafe { gos_rt_vec_clone(v) }
        };
        // SAFETY: `heap` is the fresh vec made above: a copy of `v`, or empty.
        let vec = unsafe { &*heap };
        let len = vec.len as usize;
        if len > 1 {
            let buf = vec.ptr.cast::<i64>();
            for i in (0..=(len / 2)).rev() {
                // SAFETY: `buf` holds `len` words, and `i <= len / 2` is below `len`.
                unsafe { heap_sift_down_f64(buf, len, i) };
            }
        }
        heap
    })
}

/// Pushes a float onto a min-heap, keeping the least value at the root.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_min_push_f64(v: *mut GosVec, value: f64) {
    ffi_entry!({
        if v.is_null() {
            return;
        }
        // SAFETY: `v` is non-null (checked above) and this shim's live heap argument (C-ABI
        // contract).
        unsafe { gos_rt_vec_push_i64(v, value.to_bits() as i64) };
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        let vec = unsafe { &*v };
        let len = vec.len as usize;
        if len > 1 {
            // SAFETY: `vec` holds `len` words after the push, and `len - 1` is the new element.
            unsafe { heap_sift_up_f64(vec.ptr.cast::<i64>(), len - 1) };
        }
    });
}

/// Removes and returns the least float as `Option<f64>` bits packed into an
/// i128 (disc=0 `Some`, disc=1 `None`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_min_pop_f64(v: *mut GosVec) -> i128 {
    ffi_entry!({
        if v.is_null() {
            return super::result::pack_result(1, 0);
        }
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        let vec = unsafe { &mut *v };
        if vec.len <= 0 {
            return super::result::pack_result(1, 0);
        }
        let buf = vec.ptr.cast::<i64>();
        // SAFETY: `vec` is non-null with `len > 0` (checked above), so the buffer holds its root
        // word.
        let root = unsafe { *buf };
        let last_idx = (vec.len - 1) as usize;
        if last_idx > 0 {
            // SAFETY: `last_idx` is below the length, inside the buffer.
            unsafe { *buf = *buf.add(last_idx) };
        }
        vec.len -= 1;
        let new_len = vec.len as usize;
        if new_len > 1 {
            // SAFETY: `buf` holds `new_len` words.
            unsafe { heap_sift_down_f64(buf, new_len, 0) };
        }
        super::result::pack_result(0, root)
    })
}

// ---------------------------------------------------------------
// Elements of any orderable type. The heap is the same element store
// a `Vec<T>` uses - one element of the store's own stride per slot -
// and the sift compares two elements through the ordering descriptor
// the call site hands over, so a struct, tuple, `String`, sequence,
// `Option`, or enum orders exactly as the language orders it.
// ---------------------------------------------------------------

/// Create an empty heap holding elements of `elem_bytes` bytes, owned per
/// the `Vec` element-kind tag.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_new_typed(elem_bytes: i32, elem_kind: u8) -> *mut GosVec {
    ffi_entry!({
        let bytes = if elem_bytes > 0 { elem_bytes } else { 8 };
        crate::c_abi::vec::gos_rt_vec_new_typed(bytes as u32, elem_kind)
    })
}

unsafe fn heap_elem(v: &GosVec, idx: usize) -> *mut u8 {
    // SAFETY: this `unsafe fn`'s caller passes `idx` below the vec's length.
    unsafe { v.ptr.add(idx * (v.elem_bytes as usize)) }
}

/// Element widths a sift's scratch buffer holds without touching the
/// allocator. A heap element is a scalar, a handle, or a flat slot slab, so
/// every ordinary one fits.
const HEAP_SWAP_INLINE_BYTES: usize = 64;

/// A sift's held element: the one the sift is placing, kept out of the
/// storage while the elements it passes move up or down into the hole it
/// leaves. One element move per level, where an exchange per level costs
/// three.
struct HeapHole {
    inline: std::mem::MaybeUninit<[u8; HEAP_SWAP_INLINE_BYTES]>,
    spilled: Vec<u8>,
    stride: usize,
}

impl HeapHole {
    /// Lifts the element at `idx` out of the storage.
    unsafe fn lift(v: &GosVec, idx: usize) -> Self {
        let stride = v.elem_bytes as usize;
        let mut hole = Self {
            inline: std::mem::MaybeUninit::uninit(),
            spilled: if stride > HEAP_SWAP_INLINE_BYTES {
                vec![0u8; stride]
            } else {
                Vec::new()
            },
            stride,
        };
        // SAFETY: this `unsafe fn`'s caller passes `idx` below the vec's length, and the hole
        // holds `stride` bytes.
        unsafe {
            crate::c_abi::string::copy_small_bytes(heap_elem(v, idx), hole.as_mut_ptr(), stride);
        }
        hole
    }

    fn as_ptr(&self) -> *const u8 {
        if self.stride > HEAP_SWAP_INLINE_BYTES {
            self.spilled.as_ptr()
        } else {
            self.inline.as_ptr().cast::<u8>()
        }
    }

    fn as_mut_ptr(&mut self) -> *mut u8 {
        if self.stride > HEAP_SWAP_INLINE_BYTES {
            self.spilled.as_mut_ptr()
        } else {
            self.inline.as_mut_ptr().cast::<u8>()
        }
    }

    /// Drops the held element back into the storage at `idx`.
    unsafe fn settle(&self, v: &GosVec, idx: usize) {
        // SAFETY: this `unsafe fn`'s caller passes `idx` below the vec's length, and the hole
        // holds `stride` bytes.
        unsafe {
            crate::c_abi::string::copy_small_bytes(self.as_ptr(), heap_elem(v, idx), self.stride);
        }
    }
}

/// Moves the element at `from` to `to`, both inside one heap.
unsafe fn heap_move(v: &GosVec, from: usize, to: usize) {
    let stride = v.elem_bytes as usize;
    // SAFETY: this `unsafe fn`'s caller passes `from` and `to` below the vec's length.
    unsafe { crate::c_abi::string::copy_small_bytes(heap_elem(v, from), heap_elem(v, to), stride) };
}

unsafe fn heap_swap(v: &GosVec, a: usize, b: usize) {
    if a == b {
        return;
    }
    let stride = v.elem_bytes as usize;
    // SAFETY: this `unsafe fn`'s caller passes `a` and `b` below the vec's length, and `a != b`
    // (checked above).
    unsafe { std::ptr::swap_nonoverlapping(heap_elem(v, a), heap_elem(v, b), stride) };
}

/// Sifts the element at `start` towards the root while it outranks its
/// parent, under a comparison the caller has already specialised.
unsafe fn sift_up_by(
    v: &GosVec,
    start: usize,
    max: bool,
    cmp: impl Fn(*const u8, *const u8) -> i64,
) {
    if start == 0 {
        return;
    }
    // SAFETY: this `unsafe fn`'s caller passes `start` below the vec's length.
    let hole = unsafe { HeapHole::lift(v, start) };
    let held = hole.as_ptr();
    let mut i = start;
    while i > 0 {
        let parent = (i - 1) / 2;
        // SAFETY: `parent < start` is below the vec's length.
        let ord = cmp(unsafe { heap_elem(v, parent) }, held);
        let outranks = if max { ord < 0 } else { ord > 0 };
        if !outranks {
            break;
        }
        // SAFETY: `parent` and `i` are at most `start`, below the vec's length.
        unsafe { heap_move(v, parent, i) };
        i = parent;
    }
    // SAFETY: `i` is at most `start`, below the vec's length.
    unsafe { hole.settle(v, i) };
}

/// Sifts the element at `start` down while a child outranks it, under a
/// comparison the caller has already specialised.
unsafe fn sift_down_by(
    v: &GosVec,
    len: usize,
    start: usize,
    max: bool,
    cmp: impl Fn(*const u8, *const u8) -> i64,
) {
    // SAFETY: this `unsafe fn`'s caller passes `start` below `len`, the vec's length.
    let hole = unsafe { HeapHole::lift(v, start) };
    let held = hole.as_ptr();
    let mut i = start;
    loop {
        let left = 2 * i + 1;
        if left >= len {
            break;
        }
        let right = left + 1;
        let mut best = left;
        if right < len {
            // SAFETY: `left` and `right` are below `len` (checked above).
            let ord = cmp(unsafe { heap_elem(v, left) }, unsafe {
                heap_elem(v, right)
            });
            let right_outranks = if max { ord < 0 } else { ord > 0 };
            if right_outranks {
                best = right;
            }
        }
        // SAFETY: `best` is below `len`.
        let ord = cmp(unsafe { heap_elem(v, best) }, held);
        let outranks = if max { ord > 0 } else { ord < 0 };
        if !outranks {
            break;
        }
        // SAFETY: `best` and `i` are below `len`.
        unsafe { heap_move(v, best, i) };
        i = best;
    }
    // SAFETY: `i` is below `len`.
    unsafe { hole.settle(v, i) };
}

/// Orders two elements of `W` signed machine words lexicographically.
#[inline]
fn compare_int_words<const W: usize>(a: &[i64; W], b: &[i64; W]) -> i64 {
    for (&x, &y) in a.iter().zip(b) {
        if x != y {
            return if x < y { -1 } else { 1 };
        }
    }
    0
}

/// [`sift_up_by`] for an element of `W` whole signed words, with the
/// element width and the heap's direction fixed where the sift is compiled.
unsafe fn sift_up_int_words<const W: usize, const MAX: bool>(v: &GosVec, start: usize) {
    let base = v.ptr.as_ptr();
    // SAFETY: this `unsafe fn`'s caller passes a vec of `W`-word elements, and every index the
    // sift takes is below its length.
    let at = |i: usize| unsafe { base.add(i * W * 8) };
    // SAFETY: every index below `len` addresses one whole element of W words.
    let held = unsafe { at(start).cast::<[i64; W]>().read_unaligned() };
    let mut i = start;
    while i > 0 {
        let parent = (i - 1) / 2;
        // SAFETY: `parent` is below `start`, so inside the heap.
        let above = unsafe { at(parent).cast::<[i64; W]>().read_unaligned() };
        let ord = compare_int_words(&above, &held);
        let outranks = if MAX { ord < 0 } else { ord > 0 };
        if !outranks {
            break;
        }
        // SAFETY: `i` is inside the heap.
        unsafe { at(i).cast::<[i64; W]>().write_unaligned(above) };
        i = parent;
    }
    // SAFETY: `i` is inside the heap.
    unsafe { at(i).cast::<[i64; W]>().write_unaligned(held) };
}

/// [`sift_down_by`] for an element of `W` whole signed words.
unsafe fn sift_down_int_words<const W: usize, const MAX: bool>(
    v: &GosVec,
    len: usize,
    start: usize,
) {
    let base = v.ptr.as_ptr();
    // SAFETY: this `unsafe fn`'s caller passes a vec of `W`-word elements, and every index the
    // sift takes is below `len`.
    let at = |i: usize| unsafe { base.add(i * W * 8) };
    // SAFETY: every index below `len` addresses one whole element of W words.
    let read = |i: usize| unsafe { at(i).cast::<[i64; W]>().read_unaligned() };
    let held = read(start);
    let mut i = start;
    loop {
        let left = 2 * i + 1;
        if left >= len {
            break;
        }
        let right = left + 1;
        let mut best = left;
        let mut best_val = read(left);
        if right < len {
            let right_val = read(right);
            let ord = compare_int_words(&best_val, &right_val);
            let right_outranks = if MAX { ord < 0 } else { ord > 0 };
            if right_outranks {
                best = right;
                best_val = right_val;
            }
        }
        let ord = compare_int_words(&best_val, &held);
        let outranks = if MAX { ord > 0 } else { ord < 0 };
        if !outranks {
            break;
        }
        // SAFETY: `i` is inside the heap.
        unsafe { at(i).cast::<[i64; W]>().write_unaligned(best_val) };
        i = best;
    }
    // SAFETY: `i` is inside the heap.
    unsafe { at(i).cast::<[i64; W]>().write_unaligned(held) };
}

/// Runs the word-specialised sift for an element of one to four signed
/// words whose store stride is exactly those words, answering whether it
/// applied. Priority queues order integers and tuples of integers far more
/// often than anything else, and a sift whose width and direction are
/// compile-time constants moves and compares each element as whole words.
macro_rules! sift_int_words {
    ($plan:expr, $v:expr, $max:expr, $sift:ident ( $($arg:expr),* )) => {{
        use crate::c_abi::desc_cmp::CmpPlan;
        let words = match $plan {
            CmpPlan::IntWord => Some(1),
            CmpPlan::IntTuple(n) => Some(n),
            _ => None,
        };
        match words.filter(|&w| $v.elem_bytes as usize == w * 8) {
            Some(1) if $max => {
                // SAFETY: the plan names 1-word elements, which the vec's width matches
                // (checked above).
                unsafe { $sift::<1, true>($v, $($arg),*) };
                true
            }
            Some(1) => {
                // SAFETY: the plan names 1-word elements, which the vec's width matches
                // (checked above).
                unsafe { $sift::<1, false>($v, $($arg),*) };
                true
            }
            Some(2) if $max => {
                // SAFETY: the plan names 2-word elements, which the vec's width matches
                // (checked above).
                unsafe { $sift::<2, true>($v, $($arg),*) };
                true
            }
            Some(2) => {
                // SAFETY: the plan names 2-word elements, which the vec's width matches
                // (checked above).
                unsafe { $sift::<2, false>($v, $($arg),*) };
                true
            }
            Some(3) if $max => {
                // SAFETY: the plan names 3-word elements, which the vec's width matches
                // (checked above).
                unsafe { $sift::<3, true>($v, $($arg),*) };
                true
            }
            Some(3) => {
                // SAFETY: the plan names 3-word elements, which the vec's width matches
                // (checked above).
                unsafe { $sift::<3, false>($v, $($arg),*) };
                true
            }
            Some(4) if $max => {
                // SAFETY: the plan names 4-word elements, which the vec's width matches
                // (checked above).
                unsafe { $sift::<4, true>($v, $($arg),*) };
                true
            }
            Some(4) => {
                // SAFETY: the plan names 4-word elements, which the vec's width matches
                // (checked above).
                unsafe { $sift::<4, false>($v, $($arg),*) };
                true
            }
            _ => false,
        }
    }};
}

/// Runs `sift` with the comparison the element's descriptor settles, decided
/// once per sift so each level costs the comparison itself rather than a
/// walk of the descriptor.
macro_rules! sift_under_plan {
    ($tags:expr, $sift:ident ( $($arg:expr),* )) => {{
        let tags = $tags;
        // SAFETY: the macro's caller passes `tags` a compiler-emitted ordering descriptor.
        let plan = unsafe { crate::c_abi::desc_cmp::plan_cmp(tags) };
        use crate::c_abi::desc_cmp::CmpPlan;
        match plan {
            // SAFETY: the plan found integer-word elements, which the comparator reads.
            CmpPlan::IntWord => unsafe {
                $sift($($arg),*, |a, b| crate::c_abi::desc_cmp::compare_int_word(a, b))
            },
            // SAFETY: the plan found integer-tuple elements of `slots` words, which the
            // comparator reads.
            CmpPlan::IntTuple(slots) => unsafe {
                $sift($($arg),*, |a, b| {
                    crate::c_abi::desc_cmp::compare_int_slots(slots, a, b)
                })
            },
            // SAFETY: the plan found flat elements of kind `tag`, which the comparator reads.
            CmpPlan::Flat(tag) => unsafe {
                $sift($($arg),*, |a, b| crate::c_abi::desc_cmp::compare_flat(tag, a, b))
            },
            // SAFETY: the plan found flat tuple elements laid out as `fields` describes, which
            // the comparator reads.
            CmpPlan::FlatTuple(fields) => unsafe {
                $sift($($arg),*, |a, b| {
                    crate::c_abi::desc_cmp::compare_flat_slots(fields, a, b)
                })
            },
            // SAFETY: elements are laid out as `tags` describes, which the descriptor walk reads.
            CmpPlan::Walk => unsafe {
                $sift($($arg),*, |a, b| crate::c_abi::desc_cmp::compare_whole(a, b, tags))
            },
        }
    }};
}

/// Sifts the element at `start` towards the root while it outranks its
/// parent. `max` selects which end of the ordering the root holds.
unsafe fn heap_sift_up_desc(v: &GosVec, start: usize, tags: *const u8, max: bool) {
    // SAFETY: this `unsafe fn`'s caller passes `tags` live or null, which `plan_cmp` accepts.
    let plan = unsafe { crate::c_abi::desc_cmp::plan_cmp(tags) };
    if sift_int_words!(plan, v, max, sift_up_int_words(start)) {
        return;
    }
    sift_under_plan!(tags, sift_up_by(v, start, max));
}

/// Sifts the element at `start` down while a child outranks it.
unsafe fn heap_sift_down_desc(v: &GosVec, len: usize, start: usize, tags: *const u8, max: bool) {
    // SAFETY: this `unsafe fn`'s caller passes `tags` live or null, which `plan_cmp` accepts.
    let plan = unsafe { crate::c_abi::desc_cmp::plan_cmp(tags) };
    if sift_int_words!(plan, v, max, sift_down_int_words(len, start)) {
        return;
    }
    sift_under_plan!(tags, sift_down_by(v, len, start, max));
}

unsafe fn bheap_push_desc(v: *mut GosVec, elem: *const u8, tags: *const u8, max: bool) {
    if v.is_null() || elem.is_null() || tags.is_null() {
        return;
    }
    // SAFETY: this `unsafe fn`'s caller passes `v`, `elem` live; each is checked above or
    // accepted null by `gos_rt_vec_push`.
    unsafe { crate::c_abi::vec::gos_rt_vec_push(v, elem) };
    // SAFETY: `v` is non-null (checked above), and this `unsafe fn`'s caller passes a live `Vec`.
    let vec = unsafe { &*v };
    let len = vec.len.max(0) as usize;
    if len > 1 {
        // SAFETY: `vec` holds `len` elements laid out as `tags` describes, and `len - 1` is the
        // new element.
        unsafe { heap_sift_up_desc(vec, len - 1, tags, max) };
    }
}

unsafe fn bheap_pop_desc(v: *mut GosVec, tags: *const u8, max: bool) -> i128 {
    if v.is_null() || tags.is_null() {
        return super::result::pack_result(1, 0);
    }
    // SAFETY: `v` is non-null (checked above), and this `unsafe fn`'s caller passes a live `Vec`
    // not otherwise accessed during the call.
    let vec = unsafe { &mut *v };
    if vec.len <= 0 {
        return super::result::pack_result(1, 0);
    }
    let last = (vec.len - 1) as usize;
    // The root leaves through the slot just past the new end, which the
    // sift below never touches - the same place a `Vec` pop hands its
    // element back from.
    // SAFETY: `last` is below the vec's length.
    unsafe { heap_swap(vec, 0, last) };
    vec.len -= 1;
    let new_len = vec.len.max(0) as usize;
    if new_len > 1 {
        // SAFETY: `vec` holds `new_len` elements laid out as `tags` describes.
        unsafe { heap_sift_down_desc(vec, new_len, 0, tags, max) };
    }
    // SAFETY: `last` is the slot just past the new length, which still holds the popped element.
    let word = unsafe { crate::c_abi::vec::vec_elem_owned_payload_word(vec, last as i64) };
    super::result::pack_result(0, word)
}

/// The heap pop whose element the caller already owns storage for: the root
/// leaves through the slot past the new end, and its slots move into `out`.
/// Answers the `Option` discriminant (0 written, 1 empty).
unsafe fn bheap_pop_desc_into(v: *mut GosVec, tags: *const u8, out: *mut u8, max: bool) -> i64 {
    if v.is_null() || tags.is_null() || out.is_null() {
        return 1;
    }
    // SAFETY: `v` is non-null (checked above), and this `unsafe fn`'s caller passes a live `Vec`
    // not otherwise accessed during the call.
    let vec = unsafe { &mut *v };
    if vec.len <= 0 || vec.ptr.is_null() {
        return 1;
    }
    let last = (vec.len - 1) as usize;
    // SAFETY: `last` is below the vec's length.
    unsafe { heap_swap(vec, 0, last) };
    vec.len -= 1;
    let new_len = vec.len.max(0) as usize;
    if new_len > 1 {
        // SAFETY: `vec` holds `new_len` elements laid out as `tags` describes.
        unsafe { heap_sift_down_desc(vec, new_len, 0, tags, max) };
    }
    let stride = vec.elem_bytes as usize;
    // SAFETY: `last` is the slot just past the new length, which still holds the popped element.
    let src = unsafe { vec.ptr.add(last * stride) };
    // SAFETY: `out` is the caller's slot of one element's width (this `unsafe fn`'s caller).
    unsafe { crate::c_abi::string::copy_small_bytes(src, out, stride) };
    0
}

unsafe fn bheap_from_vec_desc(v: *mut GosVec, tags: *const u8, max: bool) -> *mut GosVec {
    let heap = if v.is_null() {
        gos_rt_vec_new(8)
    } else {
        // SAFETY: this `unsafe fn`'s caller passes `v` live or null, which `gos_rt_vec_clone`
        // accepts.
        unsafe { gos_rt_vec_clone(v) }
    };
    if tags.is_null() {
        return heap;
    }
    // SAFETY: `heap` is the fresh vec made above.
    let vec = unsafe { &*heap };
    let len = vec.len.max(0) as usize;
    if len > 1 {
        for i in (0..len / 2).rev() {
            // SAFETY: `vec` holds `len` elements laid out as `tags` describes, and `i < len / 2`.
            unsafe { heap_sift_down_desc(vec, len, i, tags, max) };
        }
    }
    heap
}

/// Push an element of any orderable type onto a max heap.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_max_push_desc(
    v: *mut GosVec,
    elem: *const u8,
    tags: *const u8,
) {
    // SAFETY: `v`, `elem`, `tags` are this shim's arguments, live for the call (C-ABI contract)
    // or null, which `bheap_push_desc` accepts.
    ffi_entry!({ unsafe { bheap_push_desc(v, elem, tags, true) } });
}

/// Push an element of any orderable type onto a min heap.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_min_push_desc(
    v: *mut GosVec,
    elem: *const u8,
    tags: *const u8,
) {
    // SAFETY: `v`, `elem`, `tags` are this shim's arguments, live for the call (C-ABI contract)
    // or null, which `bheap_push_desc` accepts.
    ffi_entry!({ unsafe { bheap_push_desc(v, elem, tags, false) } });
}

/// Remove and return the greatest element as `Option<T>`.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_bheap_max_pop_desc(v: *mut GosVec, tags: *const u8) -> i128 {
    ffi_entry_passthrough!({
        // SAFETY: `v`, `tags` are this shim's arguments, live for the call (C-ABI contract) or
        // null, which `bheap_pop_desc` accepts.
        unsafe { bheap_pop_desc(v, tags, true) }
    })
}

/// Remove and return the least element as `Option<T>`.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_bheap_min_pop_desc(v: *mut GosVec, tags: *const u8) -> i128 {
    ffi_entry_passthrough!({
        // SAFETY: `v`, `tags` are this shim's arguments, live for the call (C-ABI contract) or
        // null, which `bheap_pop_desc` accepts.
        unsafe { bheap_pop_desc(v, tags, false) }
    })
}

/// Remove the greatest element into caller-owned storage; see
/// `bheap_pop_desc_into`.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_bheap_max_pop_desc_into(
    v: *mut GosVec,
    tags: *const u8,
    out: *mut u8,
) -> i64 {
    // SAFETY: `v`, `tags`, `out` are this shim's arguments, live for the call (C-ABI contract) or
    // null, which `bheap_pop_desc_into` accepts.
    ffi_entry_passthrough!({ unsafe { bheap_pop_desc_into(v, tags, out, true) } })
}

/// Remove the least element into caller-owned storage; see
/// `bheap_pop_desc_into`.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_bheap_min_pop_desc_into(
    v: *mut GosVec,
    tags: *const u8,
    out: *mut u8,
) -> i64 {
    // SAFETY: `v`, `tags`, `out` are this shim's arguments, live for the call (C-ABI contract) or
    // null, which `bheap_pop_desc_into` accepts.
    ffi_entry_passthrough!({ unsafe { bheap_pop_desc_into(v, tags, out, false) } })
}

/// The root element as `Option<T>` without removing it. The payload of a
/// multi-slot element is the address of its slots.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_peek_elem(v: *const GosVec) -> i128 {
    ffi_entry!({
        if v.is_null() {
            return super::result::pack_result(1, 0);
        }
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        let vec = unsafe { &*v };
        if vec.len <= 0 {
            return super::result::pack_result(1, 0);
        }
        // SAFETY: `vec` is non-null with `len > 0` (checked above).
        let word = unsafe { crate::c_abi::vec::vec_elem_shared_payload_word(vec, 0) };
        super::result::pack_result(0, word)
    })
}

/// Heapify a `Vec` of any orderable element into a max heap.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_bheap_max_from_vec_desc(
    v: *mut GosVec,
    tags: *const u8,
) -> *mut GosVec {
    ffi_entry_passthrough!({
        // SAFETY: `v`, `tags` are this shim's arguments, live for the call (C-ABI contract) or
        // null, which `bheap_from_vec_desc` accepts.
        unsafe { bheap_from_vec_desc(v, tags, true) }
    })
}

/// Heapify a `Vec` of any orderable element into a min heap.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_bheap_min_from_vec_desc(
    v: *mut GosVec,
    tags: *const u8,
) -> *mut GosVec {
    ffi_entry_passthrough!({
        // SAFETY: `v`, `tags` are this shim's arguments, live for the call (C-ABI contract) or
        // null, which `bheap_from_vec_desc` accepts.
        unsafe { bheap_from_vec_desc(v, tags, false) }
    })
}

/// Renders `owner [a, b, c]` over the heap's array order, reading each
/// element through the rendering descriptor `tags`.
unsafe fn bheap_format_desc(v: *const GosVec, owner: &str, tags: *const u8) -> *mut c_char {
    if v.is_null() || tags.is_null() {
        return crate::c_abi::string::alloc_cstring(format!("{owner} []").as_bytes());
    }
    // SAFETY: this `unsafe fn`'s caller passes `tags` live; non-null, checked above.
    let stream = unsafe { crate::c_abi::desc_format::DescStream::new(tags) };
    // SAFETY: `v` is live or null (this `unsafe fn`'s caller), and `stream` walks the element
    // descriptor.
    let text = unsafe { bheap_format_at(v, owner, stream, 0) };
    crate::c_abi::string::alloc_cstring(text.as_bytes())
}

/// `owner [a, b]` reading each element at `elem_desc` in `tags`, so a heap
/// nested in another shape renders through the same stream.
///
/// # Safety
/// `v` is a live element store and `elem_desc` indexes `tags`.
pub(crate) unsafe fn bheap_format_at(
    v: *const GosVec,
    owner: &str,
    tags: crate::c_abi::desc_format::DescStream,
    elem_desc: usize,
) -> String {
    let mut out = String::from(owner);
    out.push_str(" [");
    if !v.is_null() {
        // SAFETY: `v` is non-null (checked above) and, per this `unsafe fn`'s caller, a live heap
        // vec.
        let vec = unsafe { &*v };
        for i in 0..vec.len.max(0) {
            if i > 0 {
                out.push_str(", ");
            }
            // SAFETY: `i` is below the vec's length.
            let slot = unsafe { heap_elem(vec, i as usize) };
            let mut cursor = elem_desc;
            // SAFETY: `slot` is one element, laid out as the descriptor at `elem_desc` describes.
            unsafe {
                crate::c_abi::desc_format::render_desc_value(&mut out, slot, tags, &mut cursor);
            }
        }
    }
    out.push(']');
    out
}

/// Format a `MaxHeap` whose elements are described by `tags`.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_bheap_max_format_desc(
    v: *const GosVec,
    tags: *const u8,
) -> *mut c_char {
    ffi_entry_passthrough!({
        // SAFETY: `v` and `tags` are this shim's arguments, each null or live (C-ABI contract).
        unsafe { bheap_format_desc(v, "MaxHeap", tags) }
    })
}

/// Format a `MinHeap` whose elements are described by `tags`.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_bheap_min_format_desc(
    v: *const GosVec,
    tags: *const u8,
) -> *mut c_char {
    ffi_entry_passthrough!({
        // SAFETY: `v` and `tags` are this shim's arguments, each null or live (C-ABI contract).
        unsafe { bheap_format_desc(v, "MinHeap", tags) }
    })
}
