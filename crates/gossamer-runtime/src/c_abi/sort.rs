#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::wildcard_imports)]
#![allow(clippy::cast_ptr_alignment)]
#![allow(clippy::ptr_as_ptr)]

//! Compiled-tier `std::sort` - the explicit stable-order and
//! sorted-sequence search primitives. `Vec`'s inherent `sort` is
//! unstable; these shims are the deliberate stable / search half of
//! the same surface. Element access follows the flat 8-byte slot ABI:
//! an `i64` Vec stores values, a `String` Vec stores `*mut c_char`.

use std::os::raw::c_char;

use super::*;

/// The language's order of two floats, the one sorting, `min`, `max`, and a
/// structural comparison use: IEEE total order (`-0.0` just below `0.0`),
/// with every NaN one value above `+inf`. A NaN's sign bit depends on how it
/// was made - hardware division gives one sign, a folded constant another -
/// so it takes no part in the order.
#[must_use]
pub fn float_order(a: f64, b: f64) -> std::cmp::Ordering {
    let canonical = |x: f64| if x.is_nan() { f64::NAN } else { x };
    canonical(a).total_cmp(&canonical(b))
}

/// Read the `i64` slots of a Vec whose elements are 8-byte primitives.
unsafe fn i64_slots<'a>(v: *const GosVec) -> &'a [i64] {
    if v.is_null() {
        return &[];
    }
    let header = unsafe { &*v };
    let len = header.len.max(0) as usize;
    if len == 0 || header.elem_bytes != 8 {
        return &[];
    }
    unsafe { std::slice::from_raw_parts(header.ptr.cast::<i64>(), len) }
}

/// Borrow slot `i` of a `String` Vec as a `&str`; an empty string for
/// a null slot or non-UTF-8 content.
unsafe fn str_slot<'a>(slots: &[i64], i: usize) -> &'a str {
    let raw = slots[i];
    if raw == 0 {
        return "";
    }
    unsafe { crate::c_abi::gos_str_arg_text(raw as *const c_char) }
}

unsafe fn borrowed_str<'a>(p: *const c_char) -> &'a str {
    if p.is_null() {
        return "";
    }
    unsafe { crate::c_abi::gos_str_arg_text(p) }
}

/// Index of the first element not ordered before `pivot` (C++
/// `lower_bound`). The sequence must already be sorted ascending.
fn lower_bound<F: Fn(usize) -> std::cmp::Ordering>(len: usize, cmp: F) -> usize {
    let (mut lo, mut hi) = (0usize, len);
    while lo < hi {
        let mid = usize::midpoint(lo, hi);
        if cmp(mid) == std::cmp::Ordering::Less {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

/// Copy `src` into a fresh Vec sharing ownership of every element,
/// mirroring the sharing contract of `gos_rt_vec_reversed`.
unsafe fn clone_vec(src: *const GosVec, order: &[usize]) -> *mut GosVec {
    let header = unsafe { &*src };
    let elem_bytes = header.elem_bytes;
    let out = unsafe { gos_rt_vec_with_capacity(elem_bytes, order.len() as i64) };
    if out.is_null() {
        return out;
    }
    for &from in order {
        let elem = unsafe { header.ptr.add(from * (elem_bytes as usize)) };
        unsafe { gos_rt_vec_push(out, elem) };
    }
    unsafe { crate::c_abi::vec::vec_share_owned_elements(src, out) };
    out
}

/// `sort::sort_stable(xs) -> Vec<i64>` - ascending, equal elements
/// keep their input order.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sort_stable_i64(v: *const GosVec) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        if v.is_null() {
            return unsafe { gos_rt_vec_new(8) };
        }
        let slots = unsafe { i64_slots(v) };
        let mut order: Vec<usize> = (0..slots.len()).collect();
        order.sort_by_key(|&i| slots[i]);
        unsafe { clone_vec(v, &order) }
    })
}

/// `sort::sort_stable(xs) -> Vec<String>` - ascending by byte order,
/// equal elements keep their input order.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sort_stable_str(v: *const GosVec) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        if v.is_null() {
            return unsafe { gos_rt_vec_new(8) };
        }
        let slots = unsafe { i64_slots(v) };
        let mut order: Vec<usize> = (0..slots.len()).collect();
        order.sort_by(|&a, &b| unsafe { str_slot(slots, a).cmp(str_slot(slots, b)) });
        unsafe { clone_vec(v, &order) }
    })
}

/// `sort::binary_search(xs, target) -> Option<i64>` over a sorted
/// `Vec<i64>`; `Some(index)` of a matching element, else `None`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sort_binary_search_i64(v: *const GosVec, target: i64) -> i128 {
    ffi_entry!(unsafe { gos_rt_result_new(1, 0) }, {
        let slots = unsafe { i64_slots(v) };
        let at = lower_bound(slots.len(), |mid| slots[mid].cmp(&target));
        if at < slots.len() && slots[at] == target {
            unsafe { gos_rt_result_new(0, at as i64) }
        } else {
            unsafe { gos_rt_result_new(1, 0) }
        }
    })
}

/// `sort::binary_search(xs, target) -> Option<i64>` over a sorted
/// `Vec<String>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sort_binary_search_str(
    v: *const GosVec,
    target: *const c_char,
) -> i128 {
    ffi_entry!(unsafe { gos_rt_result_new(1, 0) }, {
        let slots = unsafe { i64_slots(v) };
        let needle = unsafe { borrowed_str(target) };
        let at = lower_bound(slots.len(), |mid| unsafe {
            str_slot(slots, mid).cmp(needle)
        });
        if at < slots.len() && unsafe { str_slot(slots, at) } == needle {
            unsafe { gos_rt_result_new(0, at as i64) }
        } else {
            unsafe { gos_rt_result_new(1, 0) }
        }
    })
}

/// `sort::partition_point(xs, pivot) -> i64` over a sorted
/// `Vec<i64>`: the count of elements strictly less than `pivot`, which
/// is also the insertion index that keeps `xs` sorted.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sort_partition_point_i64(v: *const GosVec, pivot: i64) -> i64 {
    ffi_entry!(0, {
        let slots = unsafe { i64_slots(v) };
        lower_bound(slots.len(), |mid| slots[mid].cmp(&pivot)) as i64
    })
}

/// `sort::partition_point(xs, pivot) -> i64` over a sorted
/// `Vec<String>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sort_partition_point_str(
    v: *const GosVec,
    pivot: *const c_char,
) -> i64 {
    ffi_entry!(0, {
        let slots = unsafe { i64_slots(v) };
        let needle = unsafe { borrowed_str(pivot) };
        lower_bound(slots.len(), |mid| unsafe {
            str_slot(slots, mid).cmp(needle)
        }) as i64
    })
}

/// Read the `f64` slots of a Vec whose elements are 8-byte floats.
unsafe fn f64_slots<'a>(v: *const GosVec) -> &'a [f64] {
    if v.is_null() {
        return &[];
    }
    let header = unsafe { &*v };
    let len = header.len.max(0) as usize;
    if len == 0 || header.elem_bytes != 8 {
        return &[];
    }
    unsafe { std::slice::from_raw_parts(header.ptr.cast::<f64>(), len) }
}

/// `sort::sort_stable(xs) -> Vec<f64>` - ascending by total order, equal
/// elements keep their input order.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sort_stable_f64(v: *const GosVec) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        if v.is_null() {
            return unsafe { gos_rt_vec_new(8) };
        }
        let slots = unsafe { f64_slots(v) };
        let mut order: Vec<usize> = (0..slots.len()).collect();
        order.sort_by(|&a, &b| float_order(slots[a], slots[b]));
        unsafe { clone_vec(v, &order) }
    })
}

/// `sort::binary_search(xs, target) -> Option<i64>` over a sorted
/// `Vec<f64>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sort_binary_search_f64(v: *const GosVec, target: f64) -> i128 {
    ffi_entry!(unsafe { gos_rt_result_new(1, 0) }, {
        let slots = unsafe { f64_slots(v) };
        let at = lower_bound(slots.len(), |mid| float_order(slots[mid], target));
        if at < slots.len() && float_order(slots[at], target) == std::cmp::Ordering::Equal {
            unsafe { gos_rt_result_new(0, at as i64) }
        } else {
            unsafe { gos_rt_result_new(1, 0) }
        }
    })
}

/// `sort::partition_point(xs, pivot) -> i64` over a sorted `Vec<f64>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sort_partition_point_f64(v: *const GosVec, pivot: f64) -> i64 {
    ffi_entry!(0, {
        let slots = unsafe { f64_slots(v) };
        lower_bound(slots.len(), |mid| float_order(slots[mid], pivot)) as i64
    })
}

/// `xs.binary_search(needle) -> Result<i64, i64>` over a sorted
/// `Vec<i64>`: `Ok(index)` of a matching element, `Err(index)` of the
/// position an insert would keep sorted.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_binary_search_i64(v: *const GosVec, target: i64) -> i128 {
    ffi_entry!(unsafe { gos_rt_result_new(1, 0) }, {
        let slots = unsafe { i64_slots(v) };
        let at = lower_bound(slots.len(), |mid| slots[mid].cmp(&target));
        let found = at < slots.len() && slots[at] == target;
        unsafe { gos_rt_result_new(i64::from(!found), at as i64) }
    })
}

/// `xs.binary_search(needle) -> Result<i64, i64>` over a sorted
/// `Vec<f64>`, ordered by [`float_order`] like every other float sort.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_binary_search_f64(v: *const GosVec, target: f64) -> i128 {
    ffi_entry!(unsafe { gos_rt_result_new(1, 0) }, {
        let slots = unsafe { f64_slots(v) };
        let at = lower_bound(slots.len(), |mid| float_order(slots[mid], target));
        let found = at < slots.len() && float_order(slots[at], target) == std::cmp::Ordering::Equal;
        unsafe { gos_rt_result_new(i64::from(!found), at as i64) }
    })
}

/// `xs.binary_search(needle) -> Result<i64, i64>` over a sorted
/// `Vec<String>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_binary_search_str(
    v: *const GosVec,
    target: *const c_char,
) -> i128 {
    ffi_entry!(unsafe { gos_rt_result_new(1, 0) }, {
        let slots = unsafe { i64_slots(v) };
        let needle = unsafe { borrowed_str(target) };
        let at = lower_bound(slots.len(), |mid| unsafe {
            str_slot(slots, mid).cmp(needle)
        });
        let found = at < slots.len() && unsafe { str_slot(slots, at) } == needle;
        unsafe { gos_rt_result_new(i64::from(!found), at as i64) }
    })
}

/// Element pointer and stride of a Vec whose elements are multi-slot
/// aggregates, or `None` for a Vec with no elements to read.
unsafe fn aggregate_elems(v: *const GosVec) -> Option<(*const u8, usize, usize)> {
    if v.is_null() {
        return None;
    }
    let header = unsafe { &*v };
    let len = header.len.max(0) as usize;
    let stride = i64::from(header.elem_bytes).max(0) as usize;
    if len == 0 || stride == 0 || header.ptr.is_null() {
        return None;
    }
    Some((header.ptr.as_ptr().cast_const(), len, stride))
}

/// Compares element `i` of an aggregate Vec against `target`, per the
/// `n`-element `tags` stream describing the element's field layout.
unsafe fn aggregate_cmp(
    base: *const u8,
    stride: usize,
    i: usize,
    target: *const u8,
    n: i64,
    tags: *const u8,
) -> std::cmp::Ordering {
    let elem = unsafe { base.add(i * stride).cast::<i64>() };
    unsafe { crate::c_abi::gos_rt_tuple_cmp(elem, target.cast::<i64>(), n, tags) }.cmp(&0)
}

/// `sort::sort_stable(xs) -> Vec<T>` for a tuple or struct element.
///
/// An aggregate element occupies several slots, so it has no single word to
/// order by; the `n`-element `tags` stream names each field's kind and the
/// comparison walks them in declaration order.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sort_stable_aggr(
    v: *const GosVec,
    n: i64,
    tags: *const u8,
) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let Some((base, len, stride)) = (unsafe { aggregate_elems(v) }) else {
            let elem_bytes = if v.is_null() {
                8
            } else {
                unsafe { &*v }.elem_bytes.max(8)
            };
            return unsafe { gos_rt_vec_new(elem_bytes) };
        };
        let mut order: Vec<usize> = (0..len).collect();
        order.sort_by(|&a, &b| unsafe {
            let pa = base.add(a * stride).cast::<i64>();
            let pb = base.add(b * stride).cast::<i64>();
            crate::c_abi::gos_rt_tuple_cmp(pa, pb, n, tags).cmp(&0)
        });
        unsafe { clone_vec(v, &order) }
    })
}

/// `sort::binary_search(xs, target) -> Option<i64>` for a tuple or struct
/// element, over a sequence already in the same order.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sort_binary_search_aggr(
    v: *const GosVec,
    target: *const u8,
    n: i64,
    tags: *const u8,
) -> i128 {
    ffi_entry!(unsafe { gos_rt_result_new(1, 0) }, {
        let Some((base, len, stride)) = (unsafe { aggregate_elems(v) }) else {
            return unsafe { gos_rt_result_new(1, 0) };
        };
        if target.is_null() {
            return unsafe { gos_rt_result_new(1, 0) };
        }
        let at = lower_bound(len, |mid| unsafe {
            aggregate_cmp(base, stride, mid, target, n, tags)
        });
        let found = at < len
            && unsafe { aggregate_cmp(base, stride, at, target, n, tags) }
                == std::cmp::Ordering::Equal;
        if found {
            unsafe { gos_rt_result_new(0, at as i64) }
        } else {
            unsafe { gos_rt_result_new(1, 0) }
        }
    })
}

/// `xs.binary_search(needle) -> Result<i64, i64>` for an element the tag
/// stream orders: `Ok(index)` of a matching element, `Err(index)` of the
/// position an insert would keep sorted. `target` addresses the needle's
/// slots, laid out as one element of the sequence.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_binary_search_aggr(
    v: *const GosVec,
    target: *const u8,
    n: i64,
    tags: *const u8,
) -> i128 {
    ffi_entry!(unsafe { gos_rt_result_new(1, 0) }, {
        let Some((base, len, stride)) = (unsafe { aggregate_elems(v) }) else {
            return unsafe { gos_rt_result_new(1, 0) };
        };
        if target.is_null() || tags.is_null() {
            return unsafe { gos_rt_result_new(1, 0) };
        }
        let at = lower_bound(len, |mid| unsafe {
            aggregate_cmp(base, stride, mid, target, n, tags)
        });
        let found = at < len
            && unsafe { aggregate_cmp(base, stride, at, target, n, tags) }
                == std::cmp::Ordering::Equal;
        unsafe { gos_rt_result_new(i64::from(!found), at as i64) }
    })
}

/// `sort::partition_point(xs, pivot) -> i64` for a tuple or struct element.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sort_partition_point_aggr(
    v: *const GosVec,
    pivot: *const u8,
    n: i64,
    tags: *const u8,
) -> i64 {
    ffi_entry!(0, {
        let Some((base, len, stride)) = (unsafe { aggregate_elems(v) }) else {
            return 0;
        };
        if pivot.is_null() {
            return 0;
        }
        lower_bound(len, |mid| unsafe {
            aggregate_cmp(base, stride, mid, pivot, n, tags)
        }) as i64
    })
}
