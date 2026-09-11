#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::must_use_candidate)]
#![allow(unused_unsafe)]
#![allow(clippy::wildcard_imports)]

use std::ffi::c_char;

use super::*;
use crate::c_abi::vec::vec_elem_kind;

// ---------------------------------------------------------------
// Deque / Queue / Stack - a `GosVec` of elements plus the index of
// the front one, so a FIFO pop is O(1) amortised. The element store
// is the same one `Vec<T>` uses, so an element of any type - a
// scalar, a `String`, a nested container, an inline struct or tuple -
// is held, owned, and released exactly as a `Vec<T>` element is.
// ---------------------------------------------------------------

/// A deque's element storage and the index its live range starts at.
#[repr(C)]
pub struct GosDeque {
    vec: *mut GosVec,
    head: i64,
}

unsafe fn deque_alloc(elem_bytes: i32, elem_kind: u8) -> *mut GosDeque {
    let bytes = if elem_bytes > 0 { elem_bytes } else { 8 };
    let vec = unsafe { crate::c_abi::vec::gos_rt_vec_new_typed(bytes as u32, elem_kind) };
    Box::into_raw(Box::new(GosDeque { vec, head: 0 }))
}

/// Number of live elements: everything from the front index to the end of
/// the element store.
unsafe fn deque_live_len(d: *const GosDeque) -> i64 {
    if d.is_null() {
        return 0;
    }
    let deque = unsafe { &*d };
    if deque.vec.is_null() {
        return 0;
    }
    (unsafe { &*deque.vec }.len - deque.head).max(0)
}

/// Moves the live range down to index zero. Every operation that reads or
/// writes the element store through the `Vec` ABI runs this first, so that
/// ABI only ever sees a store whose elements start where it expects them.
unsafe fn deque_compact(d: *mut GosDeque) {
    if d.is_null() {
        return;
    }
    let deque = unsafe { &mut *d };
    if deque.head <= 0 || deque.vec.is_null() {
        return;
    }
    let vec = unsafe { &mut *deque.vec };
    let live = (vec.len - deque.head).max(0);
    let stride = vec.elem_bytes as usize;
    if live > 0 && !vec.ptr.is_null() && stride > 0 {
        let base = vec.ptr.as_ptr();
        unsafe {
            std::ptr::copy(
                base.add(deque.head as usize * stride),
                base,
                live as usize * stride,
            );
        }
    }
    vec.len = live;
    deque.head = 0;
}

/// Moves the live range down when the dead prefix has grown to half the
/// store, leaving it in place otherwise.
unsafe fn deque_compact_dead_prefix(d: *mut GosDeque) {
    if d.is_null() {
        return;
    }
    let deque = unsafe { &*d };
    if deque.head <= 0 || deque.vec.is_null() {
        return;
    }
    let len = unsafe { &*deque.vec }.len;
    if deque.head.saturating_mul(2) >= len {
        unsafe { deque_compact(d) };
    }
}

/// The element store, with its live range starting at index zero: the shape
/// every `Vec` entry point - rendering, ownership metadata, deep-free -
/// reads.
///
/// # Safety
/// `d` is a live `GosDeque` or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_deque_vec(d: *mut GosDeque) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        if d.is_null() {
            return std::ptr::null_mut();
        }
        unsafe { deque_compact(d) };
        unsafe { &*d }.vec
    })
}

/// Create a new empty deque whose elements are one word wide.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_deque_new() -> *mut GosDeque {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { deque_alloc(8, vec_elem_kind::PRIMITIVE) }
    })
}

/// Create a new empty deque holding elements of `elem_bytes` bytes, owned
/// per `elem_kind` (the `Vec` element-kind tags).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_deque_new_typed(elem_bytes: i32, elem_kind: u8) -> *mut GosDeque {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { deque_alloc(elem_bytes, elem_kind) }
    })
}

/// Create a deque from a `Vec`, preserving iteration order. The deque takes
/// its own copy, retaining whatever each element owns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_deque_from_vec(v: *const GosVec) -> *mut GosDeque {
    ffi_entry!(std::ptr::null_mut(), {
        let vec = if v.is_null() {
            unsafe { crate::c_abi::vec::gos_rt_vec_new_typed(8u32, vec_elem_kind::PRIMITIVE) }
        } else {
            unsafe { crate::c_abi::string::gos_rt_vec_clone(v) }
        };
        Box::into_raw(Box::new(GosDeque { vec, head: 0 }))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_deque_from_vec_i64(v: *const GosVec) -> *mut GosDeque {
    unsafe { gos_rt_deque_from_vec(v) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_queue_new() -> *mut GosDeque {
    unsafe { gos_rt_deque_new() }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_queue_from_vec_i64(v: *const GosVec) -> *mut GosDeque {
    unsafe { gos_rt_deque_from_vec(v) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_stack_new() -> *mut GosDeque {
    unsafe { gos_rt_deque_new() }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_stack_from_vec_i64(v: *const GosVec) -> *mut GosDeque {
    unsafe { gos_rt_deque_from_vec(v) }
}

/// Appends the one-word element `value` to the back.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_deque_push_back(d: *mut GosDeque, value: i64) {
    ffi_entry!((), {
        let word = value;
        unsafe { deque_push_back_slot(d, std::ptr::addr_of!(word).cast()) };
    });
}

/// Appends a float to the back, stored as its bit pattern: a one-word slot
/// holds the bits, and the read on the way out reinterprets them.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_deque_push_back_f64(d: *mut GosDeque, value: f64) {
    ffi_entry!((), {
        let word = value.to_bits() as i64;
        unsafe { deque_push_back_slot(d, std::ptr::addr_of!(word).cast()) };
    });
}

/// Appends the element whose slots `elem` addresses to the back. The width
/// and ownership of those slots come from the element store's header, so a
/// multi-slot struct, tuple, or array is copied in whole.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_deque_push_back_wide(d: *mut GosDeque, elem: *const u8) {
    ffi_entry!((), { unsafe { deque_push_back_slot(d, elem) } });
}

unsafe fn deque_push_back_slot(d: *mut GosDeque, elem: *const u8) {
    if d.is_null() || elem.is_null() {
        return;
    }
    // The store grows at its end, which is past the live range wherever that
    // range begins, so a dead prefix does not stand in the way of a push. It
    // is reclaimed on the same terms a pop reclaims it, which is what keeps
    // a queue drained and refilled from moving its contents each time.
    unsafe { deque_compact_dead_prefix(d) };
    let deque = unsafe { &mut *d };
    if deque.vec.is_null() {
        deque.vec =
            unsafe { crate::c_abi::vec::gos_rt_vec_new_typed(8u32, vec_elem_kind::PRIMITIVE) };
    }
    unsafe { crate::c_abi::vec::gos_rt_vec_push(deque.vec, elem) };
}

/// Prepends the one-word element `value` to the front.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_deque_push_front(d: *mut GosDeque, value: i64) {
    ffi_entry!((), {
        let word = value;
        unsafe { deque_push_front_slot(d, std::ptr::addr_of!(word).cast()) };
    });
}

/// Prepends a float to the front, stored as its bit pattern.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_deque_push_front_f64(d: *mut GosDeque, value: f64) {
    ffi_entry!((), {
        let word = value.to_bits() as i64;
        unsafe { deque_push_front_slot(d, std::ptr::addr_of!(word).cast()) };
    });
}

/// Prepends the element whose slots `elem` addresses to the front.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_deque_push_front_wide(d: *mut GosDeque, elem: *const u8) {
    ffi_entry!((), { unsafe { deque_push_front_slot(d, elem) } });
}

unsafe fn deque_push_front_slot(d: *mut GosDeque, elem: *const u8) {
    if d.is_null() || elem.is_null() {
        return;
    }
    unsafe { deque_compact(d) };
    let deque = unsafe { &mut *d };
    if deque.vec.is_null() {
        deque.vec =
            unsafe { crate::c_abi::vec::gos_rt_vec_new_typed(8u32, vec_elem_kind::PRIMITIVE) };
    }
    // Push at the back to grow the store by one element's worth of storage
    // (with the ownership the element kind asks for), then rotate that
    // element down to index zero.
    unsafe { crate::c_abi::vec::gos_rt_vec_push(deque.vec, elem) };
    let vec = unsafe { &mut *deque.vec };
    let stride = vec.elem_bytes as usize;
    if vec.len <= 1 || vec.ptr.is_null() || stride == 0 {
        return;
    }
    let base = vec.ptr.as_ptr();
    let mut scratch = vec![0u8; stride];
    unsafe {
        std::ptr::copy_nonoverlapping(
            base.add((vec.len as usize - 1) * stride),
            scratch.as_mut_ptr(),
            stride,
        );
        std::ptr::copy(base, base.add(stride), (vec.len as usize - 1) * stride);
        std::ptr::copy_nonoverlapping(scratch.as_ptr(), base, stride);
    }
}

/// The element at `idx` of the live range as the `Option` payload word the
/// caller owns: the value itself for a one-word element, a copy of the slot
/// block for a wider one. The value a `pop` or a `peek` answers is the
/// caller's, so it stays readable however the container is used next.
unsafe fn deque_payload_at(d: *const GosDeque, idx: i64) -> Option<i64> {
    if d.is_null() {
        return None;
    }
    let deque = unsafe { &*d };
    if deque.vec.is_null() {
        return None;
    }
    let vec = unsafe { &*deque.vec };
    let at = deque.head + idx;
    if at < 0 || at >= vec.len {
        return None;
    }
    Some(unsafe { crate::c_abi::vec::vec_elem_owned_payload_word(vec, at) })
}

/// Removes and returns the front element as `Option<T>` packed into i128
/// (disc=0 `Some`, disc=1 `None`). The payload of a multi-slot element is
/// the address of its slots, which stay readable until the next mutation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_deque_pop_front(d: *mut GosDeque) -> i128 {
    ffi_entry!(0i128, {
        // A wide element's payload is a copy of its slots rather than an
        // address into the store, so the live range may start anywhere and
        // the dead prefix is reclaimed only once it is half the store. Every
        // element then moves at most once per halving, which is what makes a
        // drain cost its own length rather than its length squared.
        unsafe { deque_compact_dead_prefix(d) };
        match unsafe { deque_payload_at(d, 0) } {
            Some(word) => {
                unsafe { &mut *d }.head += 1;
                unsafe { gos_rt_result_new(0, word) }
            }
            None => unsafe { gos_rt_result_new(1, 0) },
        }
    })
}

/// Removes and returns the back element as `Option<T>` packed into i128.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_deque_pop_back(d: *mut GosDeque) -> i128 {
    ffi_entry!(0i128, {
        let len = unsafe { deque_live_len(d) };
        if len <= 0 {
            return unsafe { gos_rt_result_new(1, 0) };
        }
        match unsafe { deque_payload_at(d, len - 1) } {
            Some(word) => {
                let vec = unsafe { &mut *(*d).vec };
                vec.len -= 1;
                unsafe { gos_rt_result_new(0, word) }
            }
            None => unsafe { gos_rt_result_new(1, 0) },
        }
    })
}

/// Address of the live element at `idx` from the front, or null when there
/// is none.
unsafe fn deque_elem_ptr(d: *const GosDeque, idx: i64) -> *mut u8 {
    if d.is_null() {
        return std::ptr::null_mut();
    }
    let deque = unsafe { &*d };
    if deque.vec.is_null() {
        return std::ptr::null_mut();
    }
    let vec = unsafe { &*deque.vec };
    let at = deque.head + idx;
    if at < 0 || at >= vec.len || vec.ptr.is_null() || vec.elem_bytes == 0 {
        return std::ptr::null_mut();
    }
    unsafe { vec.ptr.add((at as usize) * (vec.elem_bytes as usize)) }
}

/// `pop_front` whose element the caller already owns storage for: the front
/// element's slots move into `out` and the answer is the `Option`
/// discriminant (0 written, 1 empty).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_deque_pop_front_into(d: *mut GosDeque, out: *mut u8) -> i64 {
    ffi_entry!(1, {
        if out.is_null() {
            return 1;
        }
        unsafe { deque_compact_dead_prefix(d) };
        let src = unsafe { deque_elem_ptr(d, 0) };
        if src.is_null() {
            return 1;
        }
        let stride = unsafe { &*(*d).vec }.elem_bytes as usize;
        unsafe { crate::c_abi::string::copy_small_bytes(src, out, stride) };
        unsafe { &mut *d }.head += 1;
        0
    })
}

/// `pop_back` whose element the caller already owns storage for: the back
/// element's slots move into `out` and the answer is the `Option`
/// discriminant (0 written, 1 empty).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_deque_pop_back_into(d: *mut GosDeque, out: *mut u8) -> i64 {
    ffi_entry!(1, {
        if out.is_null() {
            return 1;
        }
        let len = unsafe { deque_live_len(d) };
        let src = unsafe { deque_elem_ptr(d, len - 1) };
        if src.is_null() {
            return 1;
        }
        let vec = unsafe { &mut *(*d).vec };
        unsafe { crate::c_abi::string::copy_small_bytes(src, out, vec.elem_bytes as usize) };
        vec.len -= 1;
        0
    })
}

/// Returns the front element as `Option<T>` without removing it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_deque_peek_front(d: *const GosDeque) -> i128 {
    ffi_entry!(0i128, {
        match unsafe { deque_payload_at(d, 0) } {
            Some(word) => unsafe { gos_rt_result_new(0, word) },
            None => unsafe { gos_rt_result_new(1, 0) },
        }
    })
}

/// Returns the back element as `Option<T>` without removing it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_deque_peek_back(d: *const GosDeque) -> i128 {
    ffi_entry!(0i128, {
        let len = unsafe { deque_live_len(d) };
        if len <= 0 {
            return unsafe { gos_rt_result_new(1, 0) };
        }
        match unsafe { deque_payload_at(d, len - 1) } {
            Some(word) => unsafe { gos_rt_result_new(0, word) },
            None => unsafe { gos_rt_result_new(1, 0) },
        }
    })
}

/// Return the number of elements in the deque.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_deque_len(d: *const GosDeque) -> i64 {
    ffi_entry!(0, { unsafe { deque_live_len(d) } })
}

/// Return 1 if the deque is empty, 0 otherwise.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_deque_is_empty(d: *const GosDeque) -> i32 {
    ffi_entry!(1, { i32::from(unsafe { deque_live_len(d) } <= 0) })
}

/// Remove all elements from the deque, releasing whatever they own.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_deque_clear(d: *mut GosDeque) {
    ffi_entry!((), {
        if d.is_null() {
            return;
        }
        unsafe { deque_compact(d) };
        let deque = unsafe { &mut *d };
        if !deque.vec.is_null() {
            unsafe { crate::c_abi::vec::gos_rt_vec_truncate(deque.vec, 0) };
        }
    });
}

/// Renders `owner [a, b, c]` over the front-to-back order, reading each
/// element through `tags` - the one text form every tier prints for these
/// containers.
unsafe fn deque_format_with(d: *mut GosDeque, owner: &str, tags: *const u8) -> *mut c_char {
    let text = if tags.is_null() {
        unsafe { deque_format_words(d, owner) }
    } else {
        let stream = unsafe { crate::c_abi::map::DescStream::new(tags) };
        unsafe { deque_format_at(d, owner, stream, 0) }
    };
    crate::c_abi::string::alloc_cstring(text.as_bytes())
}

/// `owner [1, 2]` over one-word integer elements.
unsafe fn deque_format_words(d: *mut GosDeque, owner: &str) -> String {
    let mut out = String::from(owner);
    out.push_str(" [");
    let vec = unsafe { gos_rt_deque_vec(d) };
    if !vec.is_null() {
        let store = unsafe { &*vec };
        for i in 0..store.len.max(0) {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&crate::builtins::format_int(unsafe {
                crate::c_abi::vec::vec_elem_load_i64(store, i)
            }));
        }
    }
    out.push(']');
    out
}

/// `owner [a, b]` reading each element at `elem_desc` in `tags`, so a
/// container nested in another shape renders through the same stream the
/// shape around it is being read from.
///
/// # Safety
/// `d` is a live deque handle and `elem_desc` indexes `tags`.
pub(crate) unsafe fn deque_format_at(
    d: *mut GosDeque,
    owner: &str,
    tags: crate::c_abi::map::DescStream,
    elem_desc: usize,
) -> String {
    let mut out = String::from(owner);
    out.push_str(" [");
    let vec = unsafe { gos_rt_deque_vec(d) };
    if !vec.is_null() {
        let store = unsafe { &*vec };
        let stride = store.elem_bytes as usize;
        for i in 0..store.len.max(0) {
            if i > 0 {
                out.push_str(", ");
            }
            let slot = unsafe { store.ptr.add((i as usize) * stride) };
            let mut cursor = elem_desc;
            unsafe {
                crate::c_abi::map::render_desc_value(&mut out, slot, tags, &mut cursor);
            };
        }
    }
    out.push(']');
    out
}

/// Format a `Deque` of one-word integer elements for `{}` / `{:?}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_deque_format(d: *mut GosDeque) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { deque_format_with(d, "Deque", std::ptr::null()) }
    })
}

/// Format a `Queue` of one-word integer elements for `{}` / `{:?}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_queue_format(d: *mut GosDeque) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { deque_format_with(d, "Queue", std::ptr::null()) }
    })
}

/// Format a `Stack` of one-word integer elements for `{}` / `{:?}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_stack_format(d: *mut GosDeque) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { deque_format_with(d, "Stack", std::ptr::null()) }
    })
}

/// Format a `Deque` whose elements are described by `tags`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_deque_format_desc(
    d: *mut GosDeque,
    tags: *const u8,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { deque_format_with(d, "Deque", tags) }
    })
}

/// Format a `Queue` whose elements are described by `tags`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_queue_format_desc(
    d: *mut GosDeque,
    tags: *const u8,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { deque_format_with(d, "Queue", tags) }
    })
}

/// Format a `Stack` whose elements are described by `tags`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_stack_format_desc(
    d: *mut GosDeque,
    tags: *const u8,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { deque_format_with(d, "Stack", tags) }
    })
}

/// Release the deque and whatever its live elements own.
/// A deque with its own element store, so a binding taken from another
/// leaves that one untouched. `Queue` and `Stack` are the same header and
/// clone through here.
///
/// # Safety
/// `d` is a live `GosDeque` or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_deque_clone(d: *mut GosDeque) -> *mut GosDeque {
    ffi_entry!(std::ptr::null_mut(), {
        if d.is_null() {
            return unsafe { gos_rt_deque_new() };
        }
        // Compacting first puts the live range at index zero, which is the
        // shape `gos_rt_vec_clone` copies and the element metadata reads.
        unsafe { deque_compact(d) };
        let source = unsafe { &*d }.vec;
        let vec = if source.is_null() {
            unsafe { crate::c_abi::vec::gos_rt_vec_new_typed(8u32, vec_elem_kind::PRIMITIVE) }
        } else {
            unsafe { crate::c_abi::string::gos_rt_vec_clone(source) }
        };
        Box::into_raw(Box::new(GosDeque { vec, head: 0 }))
    })
}

/// `*dst = src` through a `&mut Deque` (or a queue or stack, which share
/// the header): the header every holder of the reference names keeps its
/// identity and takes a copy of `src`'s live range, releasing its own.
///
/// # Safety
/// `dst` and `src` are live `GosDeque`s or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_deque_assign(dst: *mut GosDeque, src: *mut GosDeque) {
    ffi_entry!((), {
        if dst.is_null() || src.is_null() || std::ptr::addr_eq(dst, src) {
            return;
        }
        unsafe { deque_compact(src) };
        let source = unsafe { &*src }.vec;
        let vec = if source.is_null() {
            unsafe { crate::c_abi::vec::gos_rt_vec_new_typed(8u32, vec_elem_kind::PRIMITIVE) }
        } else {
            unsafe { crate::c_abi::string::gos_rt_vec_clone(source) }
        };
        // Compacting first leaves the old store holding exactly its live
        // range, so its deep-free releases nothing that was popped out.
        unsafe { deque_compact(dst) };
        let target = unsafe { &mut *dst };
        let old = std::mem::replace(&mut target.vec, vec);
        target.head = 0;
        if !old.is_null() {
            unsafe { crate::c_abi::map::gos_rt_vec_free(old) };
        }
    });
}

/// Replaces the deque a by-value aggregate field holds with its own clone.
///
/// `slot` is the field's storage address. A `GosDeque` has no reference count,
/// so a copied field takes an element store of its own the way a deque binding
/// does. `Queue` and `Stack` are the same header and route through here.
/// Null-safe.
///
/// # Safety
/// `slot` addresses a `*mut GosDeque` field, or is null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_deque_field_clone(slot: *mut *mut GosDeque) {
    ffi_entry!((), {
        if slot.is_null() {
            return;
        }
        let d = unsafe { slot.read_unaligned() };
        if d.is_null() {
            return;
        }
        let cloned = unsafe { gos_rt_deque_clone(d) };
        unsafe { slot.write_unaligned(cloned) };
    });
}

/// Frees the deque a by-value aggregate field owns and nulls the slot.
///
/// Nulling makes the release idempotent, so the drop pass may book it at more
/// than one exit edge of the same field without the second booking touching a
/// freed store. Null-safe.
///
/// # Safety
/// `slot` addresses a `*mut GosDeque` field, or is null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_deque_field_release(slot: *mut *mut GosDeque) {
    ffi_entry!((), {
        if slot.is_null() {
            return;
        }
        let d = unsafe { slot.read_unaligned() };
        if d.is_null() {
            return;
        }
        unsafe { slot.write_unaligned(std::ptr::null_mut()) };
        unsafe { gos_rt_deque_free(d) };
    });
}

/// `Queue::clone` - the same header a deque has.
///
/// # Safety
/// `d` is a live `GosDeque` or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_queue_clone(d: *mut GosDeque) -> *mut GosDeque {
    unsafe { gos_rt_deque_clone(d) }
}

/// `Stack::clone` - the same header a deque has.
///
/// # Safety
/// `d` is a live `GosDeque` or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_stack_clone(d: *mut GosDeque) -> *mut GosDeque {
    unsafe { gos_rt_deque_clone(d) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_deque_free(d: *mut GosDeque) {
    ffi_entry!((), {
        if d.is_null() {
            return;
        }
        // Compacting first leaves the store holding exactly the live range,
        // so its own deep-free releases each element that is still here and
        // nothing that was popped out.
        unsafe { deque_compact(d) };
        let deque = unsafe { Box::from_raw(d) };
        if !deque.vec.is_null() {
            unsafe { crate::c_abi::map::gos_rt_vec_free(deque.vec) };
        }
    });
}
