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
        let parent_v = unsafe { *buf.add(parent) };
        let cur_v = unsafe { *buf.add(i) };
        if parent_v > cur_v {
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
        if l < len && unsafe { *buf.add(l) } < unsafe { *buf.add(smallest) } {
            smallest = l;
        }
        if r < len && unsafe { *buf.add(r) } < unsafe { *buf.add(smallest) } {
            smallest = r;
        }
        if smallest == i {
            break;
        }
        unsafe { std::ptr::swap(buf.add(smallest), buf.add(i)) };
        i = smallest;
    }
}

unsafe fn max_heap_sift_up_i64(buf: *mut i64, start_i: usize) {
    let mut i = start_i;
    while i > 0 {
        let parent = (i - 1) / 2;
        let parent_v = unsafe { *buf.add(parent) };
        let cur_v = unsafe { *buf.add(i) };
        if parent_v < cur_v {
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
        if l < len && unsafe { *buf.add(l) } > unsafe { *buf.add(largest) } {
            largest = l;
        }
        if r < len && unsafe { *buf.add(r) } > unsafe { *buf.add(largest) } {
            largest = r;
        }
        if largest == i {
            break;
        }
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
    ffi_entry!((), {
        if slot.is_null() {
            return;
        }
        let v = unsafe { slot.read_unaligned() };
        if v.is_null() {
            return;
        }
        let cloned = unsafe { crate::c_abi::string::gos_rt_vec_clone(v) };
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
    ffi_entry!((), {
        if slot.is_null() {
            return;
        }
        let v = unsafe { slot.read_unaligned() };
        if v.is_null() {
            return;
        }
        unsafe { slot.write_unaligned(std::ptr::null_mut()) };
        unsafe { crate::c_abi::map::gos_rt_vec_free(v) };
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_max_new_i64() -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), { unsafe { gos_rt_vec_new(8) } })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_max_from_vec_i64(v: *mut GosVec) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let heap = if v.is_null() {
            unsafe { gos_rt_vec_new(8) }
        } else {
            unsafe { gos_rt_vec_clone(v) }
        };
        let vec = unsafe { &*heap };
        let len = vec.len as usize;
        if len > 1 {
            let buf = vec.ptr.cast::<i64>();
            for i in (0..=(len / 2)).rev() {
                unsafe { max_heap_sift_down_i64(buf, len, i) };
            }
        }
        heap
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_max_push_i64(v: *mut GosVec, value: i64) {
    ffi_entry!((), {
        if v.is_null() {
            return;
        }
        unsafe { gos_rt_vec_push_i64(v, value) };
        let vec = unsafe { &*v };
        let len = vec.len as usize;
        if len > 1 {
            unsafe { max_heap_sift_up_i64(vec.ptr.cast::<i64>(), len - 1) };
        }
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_max_pop_i64(v: *mut GosVec) -> i128 {
    ffi_entry!(super::vec::pack_result(1, 0), {
        if v.is_null() {
            return super::vec::pack_result(1, 0);
        }
        let vec = unsafe { &mut *v };
        if vec.len <= 0 {
            return super::vec::pack_result(1, 0);
        }
        let buf = vec.ptr.cast::<i64>();
        let root = unsafe { *buf };
        let last_idx = (vec.len - 1) as usize;
        if last_idx > 0 {
            unsafe { *buf = *buf.add(last_idx) };
        }
        vec.len -= 1;
        let new_len = vec.len as usize;
        if new_len > 1 {
            unsafe { max_heap_sift_down_i64(buf, new_len, 0) };
        }
        super::vec::pack_result(0, root)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_max_peek_i64(v: *const GosVec) -> i128 {
    ffi_entry!(super::vec::pack_result(1, 0), {
        if v.is_null() {
            return super::vec::pack_result(1, 0);
        }
        let vec = unsafe { &*v };
        if vec.len <= 0 {
            return super::vec::pack_result(1, 0);
        }
        super::vec::pack_result(0, unsafe { *vec.ptr.cast::<i64>() })
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_min_new_i64() -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), { unsafe { gos_rt_vec_new(8) } })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_min_from_vec_i64(v: *mut GosVec) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let heap = if v.is_null() {
            unsafe { gos_rt_vec_new(8) }
        } else {
            unsafe { gos_rt_vec_clone(v) }
        };
        let vec = unsafe { &*heap };
        let len = vec.len as usize;
        if len > 1 {
            let buf = vec.ptr.cast::<i64>();
            for i in (0..=(len / 2)).rev() {
                unsafe { heap_sift_down_i64(buf, len, i) };
            }
        }
        heap
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_min_push_i64(v: *mut GosVec, value: i64) {
    ffi_entry!((), {
        if v.is_null() {
            return;
        }
        unsafe { gos_rt_vec_push_i64(v, value) };
        let vec = unsafe { &*v };
        let len = vec.len as usize;
        if len > 1 {
            unsafe { heap_sift_up_i64(vec.ptr.cast::<i64>(), len - 1) };
        }
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_min_pop_i64(v: *mut GosVec) -> i128 {
    ffi_entry!(super::vec::pack_result(1, 0), {
        if v.is_null() {
            return super::vec::pack_result(1, 0);
        }
        let vec = unsafe { &mut *v };
        if vec.len <= 0 {
            return super::vec::pack_result(1, 0);
        }
        let buf = vec.ptr.cast::<i64>();
        let root = unsafe { *buf };
        let last_idx = (vec.len - 1) as usize;
        if last_idx > 0 {
            unsafe { *buf = *buf.add(last_idx) };
        }
        vec.len -= 1;
        let new_len = vec.len as usize;
        if new_len > 1 {
            unsafe { heap_sift_down_i64(buf, new_len, 0) };
        }
        super::vec::pack_result(0, root)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_min_peek_i64(v: *const GosVec) -> i128 {
    ffi_entry!(super::vec::pack_result(1, 0), {
        if v.is_null() {
            return super::vec::pack_result(1, 0);
        }
        let vec = unsafe { &*v };
        if vec.len <= 0 {
            return super::vec::pack_result(1, 0);
        }
        super::vec::pack_result(0, unsafe { *vec.ptr.cast::<i64>() })
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_is_empty(v: *const GosVec) -> i32 {
    ffi_entry!(1, {
        if v.is_null() {
            return 1;
        }
        i32::from(unsafe { (*v).len <= 0 })
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_clear(v: *mut GosVec) {
    ffi_entry!((), {
        if v.is_null() {
            return;
        }
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
        let vec = unsafe { &*v };
        let buf = vec.ptr.cast::<i64>();
        for i in 0..vec.len.max(0) as usize {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&crate::builtins::format_int(unsafe { *buf.add(i) }));
        }
    }
    out.push(']');
    crate::c_abi::string::alloc_cstring(out.as_bytes())
}

/// Format a `MaxHeap` for `{}` / `{:?}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_max_format(v: *const GosVec) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { bheap_format(v, "MaxHeap") }
    })
}

/// Format a `MinHeap` for `{}` / `{:?}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_min_format(v: *const GosVec) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { bheap_format(v, "MinHeap") }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_len(v: *const GosVec) -> i64 {
    ffi_entry!(0, {
        if v.is_null() {
            return 0;
        }
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
        let parent_v = slot_as_f64(unsafe { *buf.add(parent) });
        let cur_v = slot_as_f64(unsafe { *buf.add(i) });
        if parent_v > cur_v {
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
            && slot_as_f64(unsafe { *buf.add(l) }) < slot_as_f64(unsafe { *buf.add(smallest) })
        {
            smallest = l;
        }
        if r < len
            && slot_as_f64(unsafe { *buf.add(r) }) < slot_as_f64(unsafe { *buf.add(smallest) })
        {
            smallest = r;
        }
        if smallest == i {
            break;
        }
        unsafe { std::ptr::swap(buf.add(smallest), buf.add(i)) };
        i = smallest;
    }
}

unsafe fn max_heap_sift_up_f64(buf: *mut i64, start_i: usize) {
    let mut i = start_i;
    while i > 0 {
        let parent = (i - 1) / 2;
        let parent_v = slot_as_f64(unsafe { *buf.add(parent) });
        let cur_v = slot_as_f64(unsafe { *buf.add(i) });
        if parent_v < cur_v {
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
            && slot_as_f64(unsafe { *buf.add(l) }) > slot_as_f64(unsafe { *buf.add(largest) })
        {
            largest = l;
        }
        if r < len
            && slot_as_f64(unsafe { *buf.add(r) }) > slot_as_f64(unsafe { *buf.add(largest) })
        {
            largest = r;
        }
        if largest == i {
            break;
        }
        unsafe { std::ptr::swap(buf.add(largest), buf.add(i)) };
        i = largest;
    }
}

/// Heapifies a `Vec<f64>` snapshot into a max-heap over the float values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_max_from_vec_f64(v: *mut GosVec) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let heap = if v.is_null() {
            unsafe { gos_rt_vec_new(8) }
        } else {
            unsafe { gos_rt_vec_clone(v) }
        };
        let vec = unsafe { &*heap };
        let len = vec.len as usize;
        if len > 1 {
            let buf = vec.ptr.cast::<i64>();
            for i in (0..=(len / 2)).rev() {
                unsafe { max_heap_sift_down_f64(buf, len, i) };
            }
        }
        heap
    })
}

/// Pushes a float onto a max-heap, keeping the greatest value at the root.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_max_push_f64(v: *mut GosVec, value: f64) {
    ffi_entry!((), {
        if v.is_null() {
            return;
        }
        unsafe { gos_rt_vec_push_i64(v, value.to_bits() as i64) };
        let vec = unsafe { &*v };
        let len = vec.len as usize;
        if len > 1 {
            unsafe { max_heap_sift_up_f64(vec.ptr.cast::<i64>(), len - 1) };
        }
    });
}

/// Removes and returns the greatest float as `Option<f64>` bits packed into
/// an i128 (disc=0 `Some`, disc=1 `None`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_max_pop_f64(v: *mut GosVec) -> i128 {
    ffi_entry!(super::vec::pack_result(1, 0), {
        if v.is_null() {
            return super::vec::pack_result(1, 0);
        }
        let vec = unsafe { &mut *v };
        if vec.len <= 0 {
            return super::vec::pack_result(1, 0);
        }
        let buf = vec.ptr.cast::<i64>();
        let root = unsafe { *buf };
        let last_idx = (vec.len - 1) as usize;
        if last_idx > 0 {
            unsafe { *buf = *buf.add(last_idx) };
        }
        vec.len -= 1;
        let new_len = vec.len as usize;
        if new_len > 1 {
            unsafe { max_heap_sift_down_f64(buf, new_len, 0) };
        }
        super::vec::pack_result(0, root)
    })
}

/// Heapifies a `Vec<f64>` snapshot into a min-heap over the float values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_min_from_vec_f64(v: *mut GosVec) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let heap = if v.is_null() {
            unsafe { gos_rt_vec_new(8) }
        } else {
            unsafe { gos_rt_vec_clone(v) }
        };
        let vec = unsafe { &*heap };
        let len = vec.len as usize;
        if len > 1 {
            let buf = vec.ptr.cast::<i64>();
            for i in (0..=(len / 2)).rev() {
                unsafe { heap_sift_down_f64(buf, len, i) };
            }
        }
        heap
    })
}

/// Pushes a float onto a min-heap, keeping the least value at the root.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_min_push_f64(v: *mut GosVec, value: f64) {
    ffi_entry!((), {
        if v.is_null() {
            return;
        }
        unsafe { gos_rt_vec_push_i64(v, value.to_bits() as i64) };
        let vec = unsafe { &*v };
        let len = vec.len as usize;
        if len > 1 {
            unsafe { heap_sift_up_f64(vec.ptr.cast::<i64>(), len - 1) };
        }
    });
}

/// Removes and returns the least float as `Option<f64>` bits packed into an
/// i128 (disc=0 `Some`, disc=1 `None`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_min_pop_f64(v: *mut GosVec) -> i128 {
    ffi_entry!(super::vec::pack_result(1, 0), {
        if v.is_null() {
            return super::vec::pack_result(1, 0);
        }
        let vec = unsafe { &mut *v };
        if vec.len <= 0 {
            return super::vec::pack_result(1, 0);
        }
        let buf = vec.ptr.cast::<i64>();
        let root = unsafe { *buf };
        let last_idx = (vec.len - 1) as usize;
        if last_idx > 0 {
            unsafe { *buf = *buf.add(last_idx) };
        }
        vec.len -= 1;
        let new_len = vec.len as usize;
        if new_len > 1 {
            unsafe { heap_sift_down_f64(buf, new_len, 0) };
        }
        super::vec::pack_result(0, root)
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
    ffi_entry!(std::ptr::null_mut(), {
        let bytes = if elem_bytes > 0 { elem_bytes } else { 8 };
        unsafe { crate::c_abi::vec::gos_rt_vec_new_typed(bytes as u32, elem_kind) }
    })
}

unsafe fn heap_elem(v: &GosVec, idx: usize) -> *mut u8 {
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
    inline: [u8; HEAP_SWAP_INLINE_BYTES],
    spilled: Vec<u8>,
    stride: usize,
}

impl HeapHole {
    /// Lifts the element at `idx` out of the storage.
    unsafe fn lift(v: &GosVec, idx: usize) -> Self {
        let stride = v.elem_bytes as usize;
        let mut hole = Self {
            inline: [0u8; HEAP_SWAP_INLINE_BYTES],
            spilled: if stride > HEAP_SWAP_INLINE_BYTES {
                vec![0u8; stride]
            } else {
                Vec::new()
            },
            stride,
        };
        unsafe {
            crate::c_abi::string::copy_small_bytes(heap_elem(v, idx), hole.as_mut_ptr(), stride);
        }
        hole
    }

    fn as_ptr(&self) -> *const u8 {
        if self.stride > HEAP_SWAP_INLINE_BYTES {
            self.spilled.as_ptr()
        } else {
            self.inline.as_ptr()
        }
    }

    fn as_mut_ptr(&mut self) -> *mut u8 {
        if self.stride > HEAP_SWAP_INLINE_BYTES {
            self.spilled.as_mut_ptr()
        } else {
            self.inline.as_mut_ptr()
        }
    }

    /// Drops the held element back into the storage at `idx`.
    unsafe fn settle(&self, v: &GosVec, idx: usize) {
        unsafe {
            crate::c_abi::string::copy_small_bytes(self.as_ptr(), heap_elem(v, idx), self.stride);
        }
    }
}

/// Moves the element at `from` to `to`, both inside one heap.
unsafe fn heap_move(v: &GosVec, from: usize, to: usize) {
    let stride = v.elem_bytes as usize;
    unsafe { crate::c_abi::string::copy_small_bytes(heap_elem(v, from), heap_elem(v, to), stride) };
}

unsafe fn heap_swap(v: &GosVec, a: usize, b: usize) {
    if a == b {
        return;
    }
    let stride = v.elem_bytes as usize;
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
    let hole = unsafe { HeapHole::lift(v, start) };
    let held = hole.as_ptr();
    let mut i = start;
    while i > 0 {
        let parent = (i - 1) / 2;
        let ord = cmp(unsafe { heap_elem(v, parent) }, held);
        let outranks = if max { ord < 0 } else { ord > 0 };
        if !outranks {
            break;
        }
        unsafe { heap_move(v, parent, i) };
        i = parent;
    }
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
            let ord = cmp(unsafe { heap_elem(v, left) }, unsafe {
                heap_elem(v, right)
            });
            let right_outranks = if max { ord < 0 } else { ord > 0 };
            if right_outranks {
                best = right;
            }
        }
        let ord = cmp(unsafe { heap_elem(v, best) }, held);
        let outranks = if max { ord > 0 } else { ord < 0 };
        if !outranks {
            break;
        }
        unsafe { heap_move(v, best, i) };
        i = best;
    }
    unsafe { hole.settle(v, i) };
}

/// Runs `sift` with the comparison the element's descriptor settles, decided
/// once per sift so each level costs the comparison itself rather than a
/// walk of the descriptor.
macro_rules! sift_under_plan {
    ($tags:expr, $sift:ident ( $($arg:expr),* )) => {{
        let tags = $tags;
        let plan = unsafe { crate::c_abi::desc_cmp::plan_cmp(tags) };
        use crate::c_abi::desc_cmp::CmpPlan;
        match plan {
            CmpPlan::IntWord => unsafe {
                $sift($($arg),*, |a, b| crate::c_abi::desc_cmp::compare_int_word(a, b))
            },
            CmpPlan::IntTuple(slots) => unsafe {
                $sift($($arg),*, |a, b| {
                    crate::c_abi::desc_cmp::compare_int_slots(slots, a, b)
                })
            },
            CmpPlan::Flat(tag) => unsafe {
                $sift($($arg),*, |a, b| crate::c_abi::desc_cmp::compare_flat(tag, a, b))
            },
            CmpPlan::FlatTuple(fields) => unsafe {
                $sift($($arg),*, |a, b| {
                    crate::c_abi::desc_cmp::compare_flat_slots(fields, a, b)
                })
            },
            CmpPlan::Walk => unsafe {
                $sift($($arg),*, |a, b| crate::c_abi::desc_cmp::compare_whole(a, b, tags))
            },
        }
    }};
}

/// Sifts the element at `start` towards the root while it outranks its
/// parent. `max` selects which end of the ordering the root holds.
unsafe fn heap_sift_up_desc(v: &GosVec, start: usize, tags: *const u8, max: bool) {
    sift_under_plan!(tags, sift_up_by(v, start, max));
}

/// Sifts the element at `start` down while a child outranks it.
unsafe fn heap_sift_down_desc(v: &GosVec, len: usize, start: usize, tags: *const u8, max: bool) {
    sift_under_plan!(tags, sift_down_by(v, len, start, max));
}

unsafe fn bheap_push_desc(v: *mut GosVec, elem: *const u8, tags: *const u8, max: bool) {
    if v.is_null() || elem.is_null() || tags.is_null() {
        return;
    }
    unsafe { crate::c_abi::vec::gos_rt_vec_push(v, elem) };
    let vec = unsafe { &*v };
    let len = vec.len.max(0) as usize;
    if len > 1 {
        unsafe { heap_sift_up_desc(vec, len - 1, tags, max) };
    }
}

unsafe fn bheap_pop_desc(v: *mut GosVec, tags: *const u8, max: bool) -> i128 {
    if v.is_null() || tags.is_null() {
        return unsafe { super::vec::pack_result(1, 0) };
    }
    let vec = unsafe { &mut *v };
    if vec.len <= 0 {
        return unsafe { super::vec::pack_result(1, 0) };
    }
    let last = (vec.len - 1) as usize;
    // The root leaves through the slot just past the new end, which the
    // sift below never touches - the same place a `Vec` pop hands its
    // element back from.
    unsafe { heap_swap(vec, 0, last) };
    vec.len -= 1;
    let new_len = vec.len.max(0) as usize;
    if new_len > 1 {
        unsafe { heap_sift_down_desc(vec, new_len, 0, tags, max) };
    }
    let word = unsafe { crate::c_abi::vec::vec_elem_owned_payload_word(vec, last as i64) };
    unsafe { super::vec::pack_result(0, word) }
}

/// The heap pop whose element the caller already owns storage for: the root
/// leaves through the slot past the new end, and its slots move into `out`.
/// Answers the `Option` discriminant (0 written, 1 empty).
unsafe fn bheap_pop_desc_into(v: *mut GosVec, tags: *const u8, out: *mut u8, max: bool) -> i64 {
    if v.is_null() || tags.is_null() || out.is_null() {
        return 1;
    }
    let vec = unsafe { &mut *v };
    if vec.len <= 0 || vec.ptr.is_null() {
        return 1;
    }
    let last = (vec.len - 1) as usize;
    unsafe { heap_swap(vec, 0, last) };
    vec.len -= 1;
    let new_len = vec.len.max(0) as usize;
    if new_len > 1 {
        unsafe { heap_sift_down_desc(vec, new_len, 0, tags, max) };
    }
    let stride = vec.elem_bytes as usize;
    let src = unsafe { vec.ptr.add(last * stride) };
    unsafe { crate::c_abi::string::copy_small_bytes(src, out, stride) };
    0
}

unsafe fn bheap_from_vec_desc(v: *mut GosVec, tags: *const u8, max: bool) -> *mut GosVec {
    let heap = if v.is_null() {
        unsafe { gos_rt_vec_new(8) }
    } else {
        unsafe { gos_rt_vec_clone(v) }
    };
    if tags.is_null() {
        return heap;
    }
    let vec = unsafe { &*heap };
    let len = vec.len.max(0) as usize;
    if len > 1 {
        for i in (0..len / 2).rev() {
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
    ffi_entry!((), { unsafe { bheap_push_desc(v, elem, tags, true) } });
}

/// Push an element of any orderable type onto a min heap.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_min_push_desc(
    v: *mut GosVec,
    elem: *const u8,
    tags: *const u8,
) {
    ffi_entry!((), { unsafe { bheap_push_desc(v, elem, tags, false) } });
}

/// Remove and return the greatest element as `Option<T>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_max_pop_desc(v: *mut GosVec, tags: *const u8) -> i128 {
    ffi_entry!(super::vec::pack_result(1, 0), {
        unsafe { bheap_pop_desc(v, tags, true) }
    })
}

/// Remove and return the least element as `Option<T>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_min_pop_desc(v: *mut GosVec, tags: *const u8) -> i128 {
    ffi_entry!(super::vec::pack_result(1, 0), {
        unsafe { bheap_pop_desc(v, tags, false) }
    })
}

/// Remove the greatest element into caller-owned storage; see
/// [`bheap_pop_desc_into`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_max_pop_desc_into(
    v: *mut GosVec,
    tags: *const u8,
    out: *mut u8,
) -> i64 {
    ffi_entry!(1, { unsafe { bheap_pop_desc_into(v, tags, out, true) } })
}

/// Remove the least element into caller-owned storage; see
/// [`bheap_pop_desc_into`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_min_pop_desc_into(
    v: *mut GosVec,
    tags: *const u8,
    out: *mut u8,
) -> i64 {
    ffi_entry!(1, { unsafe { bheap_pop_desc_into(v, tags, out, false) } })
}

/// The root element as `Option<T>` without removing it. The payload of a
/// multi-slot element is the address of its slots.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_peek_elem(v: *const GosVec) -> i128 {
    ffi_entry!(super::vec::pack_result(1, 0), {
        if v.is_null() {
            return unsafe { super::vec::pack_result(1, 0) };
        }
        let vec = unsafe { &*v };
        if vec.len <= 0 {
            return unsafe { super::vec::pack_result(1, 0) };
        }
        let word = unsafe { crate::c_abi::vec::vec_elem_shared_payload_word(vec, 0) };
        unsafe { super::vec::pack_result(0, word) }
    })
}

/// Heapify a `Vec` of any orderable element into a max heap.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_max_from_vec_desc(
    v: *mut GosVec,
    tags: *const u8,
) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { bheap_from_vec_desc(v, tags, true) }
    })
}

/// Heapify a `Vec` of any orderable element into a min heap.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_min_from_vec_desc(
    v: *mut GosVec,
    tags: *const u8,
) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { bheap_from_vec_desc(v, tags, false) }
    })
}

/// Renders `owner [a, b, c]` over the heap's array order, reading each
/// element through the rendering descriptor `tags`.
unsafe fn bheap_format_desc(v: *const GosVec, owner: &str, tags: *const u8) -> *mut c_char {
    if v.is_null() || tags.is_null() {
        return crate::c_abi::string::alloc_cstring(format!("{owner} []").as_bytes());
    }
    let stream = unsafe { crate::c_abi::map::DescStream::new(tags) };
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
    tags: crate::c_abi::map::DescStream,
    elem_desc: usize,
) -> String {
    let mut out = String::from(owner);
    out.push_str(" [");
    if !v.is_null() {
        let vec = unsafe { &*v };
        for i in 0..vec.len.max(0) {
            if i > 0 {
                out.push_str(", ");
            }
            let slot = unsafe { heap_elem(vec, i as usize) };
            let mut cursor = elem_desc;
            unsafe { crate::c_abi::map::render_desc_value(&mut out, slot, tags, &mut cursor) };
        }
    }
    out.push(']');
    out
}

/// Format a `MaxHeap` whose elements are described by `tags`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_max_format_desc(
    v: *const GosVec,
    tags: *const u8,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { bheap_format_desc(v, "MaxHeap", tags) }
    })
}

/// Format a `MinHeap` whose elements are described by `tags`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_bheap_min_format_desc(
    v: *const GosVec,
    tags: *const u8,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { bheap_format_desc(v, "MinHeap", tags) }
    })
}
