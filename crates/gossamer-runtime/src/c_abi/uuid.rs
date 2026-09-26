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
// uuid - v4 (random) and v7 (timestamp-ordered) UUID generation,
// parsing, and normalization. Logic lives in the runtime crate
// (compiled tier links against `libgossamer_runtime.a` directly);
// `gossamer-std::uuid` is a thin facade that re-exports these
// functions for the interpreter.
// ---------------------------------------------------------------

/// Generates a fresh v4 (random) UUID and returns the canonical
/// hyphenated form as a heap-owned c-string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_uuid_v4() -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let s = ::uuid::Uuid::new_v4().hyphenated().to_string();
        alloc_cstring(s.as_bytes())
    })
}

/// Generates a fresh v7 (timestamp-ordered) UUID.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_uuid_v7() -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let s = ::uuid::Uuid::now_v7().hyphenated().to_string();
        alloc_cstring(s.as_bytes())
    })
}

/// Returns 1 iff `s` parses as a canonical UUID.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_uuid_is_valid(s: *const c_char) -> i64 {
    ffi_entry!(0, {
        if s.is_null() {
            return 0;
        }
        let s = unsafe { crate::c_abi::gos_str_arg_text(s) };
        i64::from(::uuid::Uuid::parse_str(s).is_ok())
    })
}

/// Returns the lowercase canonical form of `s` if it parses, else the empty string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_uuid_normalize(s: *const c_char) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if s.is_null() {
            return alloc_cstring(b"");
        }
        let s = unsafe { crate::c_abi::gos_str_arg_text(s) };
        let out = match ::uuid::Uuid::parse_str(s) {
            Ok(u) => u.hyphenated().to_string(),
            Err(_) => String::new(),
        };
        alloc_cstring(out.as_bytes())
    })
}

/// Returns the 32-char unhyphenated form of `s`, else the empty string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_uuid_simple(s: *const c_char) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if s.is_null() {
            return alloc_cstring(b"");
        }
        let s = unsafe { crate::c_abi::gos_str_arg_text(s) };
        let out = match ::uuid::Uuid::parse_str(s) {
            Ok(u) => u.simple().to_string(),
            Err(_) => String::new(),
        };
        alloc_cstring(out.as_bytes())
    })
}

// ======================================================================
// std::iter combinators - AOT runtime helpers.
//
// The interp wires these as native fns in stdlib_builtins.rs; this block
// is the cranelift + LLVM counterpart. SPEC §10.4: data-last argument
// order; combinators specialize on i64 element width where it matters
// (the dominant case for benchmark-shaped code), with `_ptr` variants
// for word-sized pointer elements (strings and aggregates).
//
// Closure-taking helpers follow the env-ptr + fn_addr@env[0] ABI
// established by `gos_rt_arr_sort_by_i64` (above). Each helper
// transmutes env[0] to a typed `fn(env, args...) -> ret` pointer and
// calls back through it once per element.

/// Return the element count of `v` as i64 (`iter::count(xs)`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_count(v: *const GosVec) -> i64 {
    ffi_entry!(-1, {
        if v.is_null() {
            return 0;
        }
        unsafe { (*v).len }
    })
}

/// The integer elements of `vec`, each read at the width the vec stores it:
/// a packed byte vec holds one byte per element, every other a word.
///
/// # Safety
/// `vec` must be a live header whose `ptr` addresses `len` elements.
unsafe fn vec_words(vec: &GosVec) -> impl Iterator<Item = i64> + '_ {
    // SAFETY: every index is in `[0, len)` of the caller's live vec.
    (0..vec.len).map(move |index| unsafe { crate::c_abi::vec::vec_elem_load_i64(vec, index) })
}

/// Sum all i64 elements of `v`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_sum_i64(v: *const GosVec) -> i64 {
    ffi_entry!(-1, {
        if v.is_null() {
            return 0;
        }
        let vec = unsafe { &*v };
        if vec.ptr.is_null() || vec.len <= 0 {
            return 0;
        }
        unsafe { vec_words(vec) }.fold(0i64, i64::wrapping_add)
    })
}

/// Sum all i64 elements of `v`, raising the language's integer overflow panic
/// where the sum leaves `i64`. The checked build profiles call this one, so
/// `sum` overflows exactly where `+` would.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_sum_i64_checked(v: *const GosVec) -> i64 {
    ffi_entry!(-1, {
        if v.is_null() {
            return 0;
        }
        let vec = unsafe { &*v };
        if vec.ptr.is_null() || vec.len <= 0 {
            return 0;
        }
        checked_sum_i64(unsafe { vec_words(vec) })
    })
}

/// Product of all i64 elements of `v`, raising the language's integer overflow
/// panic where the product leaves `i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_product_i64_checked(v: *const GosVec) -> i64 {
    ffi_entry!(-1, {
        if v.is_null() {
            return 1;
        }
        let vec = unsafe { &*v };
        if vec.ptr.is_null() || vec.len <= 0 {
            return 1;
        }
        checked_product_i64(unsafe { vec_words(vec) })
    })
}

/// The sum of `values`, raising the language's addition overflow panic at the
/// first element that leaves `i64`.
fn checked_sum_i64(values: impl Iterator<Item = i64>) -> i64 {
    let mut total = 0i64;
    for value in values {
        let Some(next) = total.checked_add(value) else {
            crate::c_abi::panic::panic_text("attempt to add with overflow");
            unreachable!("the raise does not return on the main thread");
        };
        total = next;
    }
    total
}

/// The product of `values`, raising the language's multiplication overflow
/// panic at the first element that leaves `i64`.
fn checked_product_i64(values: impl Iterator<Item = i64>) -> i64 {
    let mut total = 1i64;
    for value in values {
        let Some(next) = total.checked_mul(value) else {
            crate::c_abi::panic::panic_text("attempt to multiply with overflow");
            unreachable!("the raise does not return on the main thread");
        };
        total = next;
    }
    total
}

/// Sum all f64 elements of `v`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_sum_f64(v: *const GosVec) -> f64 {
    ffi_entry!(f64::NAN, {
        if v.is_null() {
            return 0.0;
        }
        let vec = unsafe { &*v };
        if vec.ptr.is_null() || vec.len <= 0 {
            return 0.0;
        }
        let slice = unsafe { std::slice::from_raw_parts(vec.ptr.cast::<f64>(), vec.len as usize) };
        // A float sum starts from +0.0, the value a written `acc += x` loop
        // starts from, so negative zeros sum to +0.0 on every tier.
        slice.iter().fold(0.0, |total, value| total + value)
    })
}

/// Product of all i64 elements of `v`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_product_i64(v: *const GosVec) -> i64 {
    ffi_entry!(-1, {
        if v.is_null() {
            return 1;
        }
        let vec = unsafe { &*v };
        if vec.ptr.is_null() || vec.len <= 0 {
            return 1;
        }
        unsafe { vec_words(vec) }.fold(1i64, i64::wrapping_mul)
    })
}

/// Product of all f64 elements of `v`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_product_f64(v: *const GosVec) -> f64 {
    ffi_entry!(f64::NAN, {
        if v.is_null() {
            return 1.0;
        }
        let vec = unsafe { &*v };
        if vec.ptr.is_null() || vec.len <= 0 {
            return 1.0;
        }
        let slice = unsafe { std::slice::from_raw_parts(vec.ptr.cast::<f64>(), vec.len as usize) };
        slice.iter().copied().product()
    })
}

/// `iter::min(xs) -> Option<i64>` as an i128-packed Option:
/// `None` (= 1) for empty input, `Some(m)` otherwise. Matches the
/// 16-byte Option ABI the typechecker pins for `iter::min`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_min_i64(v: *const GosVec) -> i128 {
    ffi_entry!(1i128, {
        if v.is_null() {
            return 1i128;
        }
        let vec = unsafe { &*v };
        if vec.ptr.is_null() || vec.len <= 0 {
            return 1i128;
        }
        match unsafe { vec_words(vec) }.min() {
            Some(m) => gos_rt_result_new(0, m),
            None => 1i128,
        }
    })
}

/// `iter::max(xs) -> Option<i64>` as an i128-packed Option:
/// `None` (= 1) for empty input, `Some(m)` otherwise.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_max_i64(v: *const GosVec) -> i128 {
    ffi_entry!(1i128, {
        if v.is_null() {
            return 1i128;
        }
        let vec = unsafe { &*v };
        if vec.ptr.is_null() || vec.len <= 0 {
            return 1i128;
        }
        match unsafe { vec_words(vec) }.max() {
            Some(m) => gos_rt_result_new(0, m),
            None => 1i128,
        }
    })
}

/// The first smallest (or, with `greatest`, the last largest) `String` in
/// `v`, by text rather than by the address its slot holds.
unsafe fn string_extreme(v: *const GosVec, greatest: bool) -> i128 {
    if v.is_null() {
        return 1i128;
    }
    let vec = unsafe { &*v };
    if vec.ptr.is_null() || vec.len <= 0 {
        return 1i128;
    }
    let text = |word: i64| -> *const c_char { std::ptr::with_exposed_provenance(word as usize) };
    let mut best: Option<i64> = None;
    for word in unsafe { vec_words(vec) } {
        let Some(current) = best else {
            best = Some(word);
            continue;
        };
        let order = unsafe { crate::c_abi::string::gos_rt_str_compare(text(word), text(current)) };
        if (greatest && order >= 0) || (!greatest && order < 0) {
            best = Some(word);
        }
    }
    best.map_or(1i128, |word| gos_rt_result_new(0, word))
}

/// `xs.min() -> Option<String>` over a `Vec<String>`, ordered by text.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_min_str(v: *const GosVec) -> i128 {
    ffi_entry!(1i128, { unsafe { string_extreme(v, false) } })
}

/// `xs.max() -> Option<String>` over a `Vec<String>`, ordered by text.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_max_str(v: *const GosVec) -> i128 {
    ffi_entry!(1i128, { unsafe { string_extreme(v, true) } })
}

/// `iter::min(xs) -> Option<f64>` as an i128-packed Option carrying the
/// payload's f64 bits: `None` (= 1) for empty input, `Some(m)` otherwise.
///
/// Ordering is `f64::total_cmp`, so a NaN never silently wins the comparison
/// the way a partial ordering would leave it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_min_f64(v: *const GosVec) -> i128 {
    ffi_entry!(1i128, {
        if v.is_null() {
            return 1i128;
        }
        let vec = unsafe { &*v };
        if vec.ptr.is_null() || vec.len <= 0 {
            return 1i128;
        }
        let slice = unsafe { std::slice::from_raw_parts(vec.ptr.cast::<f64>(), vec.len as usize) };
        match slice
            .iter()
            .copied()
            .min_by(|a, b| crate::c_abi::sort::float_order(*a, *b))
        {
            Some(m) => gos_rt_result_new(0, m.to_bits() as i64),
            None => 1i128,
        }
    })
}

/// `iter::max(xs) -> Option<f64>` as an i128-packed Option carrying the
/// payload's f64 bits: `None` (= 1) for empty input, `Some(m)` otherwise.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_max_f64(v: *const GosVec) -> i128 {
    ffi_entry!(1i128, {
        if v.is_null() {
            return 1i128;
        }
        let vec = unsafe { &*v };
        if vec.ptr.is_null() || vec.len <= 0 {
            return 1i128;
        }
        let slice = unsafe { std::slice::from_raw_parts(vec.ptr.cast::<f64>(), vec.len as usize) };
        match slice
            .iter()
            .copied()
            .max_by(|a, b| crate::c_abi::sort::float_order(*a, *b))
        {
            Some(m) => gos_rt_result_new(0, m.to_bits() as i64),
            None => 1i128,
        }
    })
}

/// Build a `Vec<i64>` of `[start, end)`. Empty if `end <= start`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_range(start: i64, end: i64) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let out = unsafe { gos_rt_vec_new(8) };
        if end > start {
            for n in start..end {
                unsafe { gos_rt_vec_push_i64(out, n) };
            }
        }
        out
    })
}

/// Build a `Vec<i64>` of `[start, end]`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_range_inclusive(start: i64, end: i64) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let out = unsafe { gos_rt_vec_new(8) };
        if end >= start {
            for n in start..=end {
                unsafe { gos_rt_vec_push_i64(out, n) };
            }
        }
        out
    })
}

/// Build `Vec<i64>` of length `n` filled with `value`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_repeat_i64(value: i64, n: i64) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let out = unsafe { gos_rt_vec_new(8) };
        if n < 0 {
            crate::c_abi::panic::panic_text("iter::repeat: count must be non-negative");
        }
        if n > 0 {
            for _ in 0..n {
                unsafe { gos_rt_vec_push_i64(out, value) };
            }
        }
        out
    })
}

/// ABI class of the element a lazy iterator state carries in its 8-byte slot.
///
/// The slot itself is untyped, so the class decides which register file the
/// element travels in when a helper hands it to a Gossamer closure and how the
/// arithmetic terminals interpret its bits. Every handle records the class its
/// producer chose; a consumer that expects a different one is reading the slot
/// through the wrong ABI.
pub mod lazy_elem_class {
    /// Integer register: `i64`, `bool`, `char`, and managed pointer words.
    pub const WORD: u8 = 0;
    /// SSE register: the element's 64 float bits.
    pub const FLOAT: u8 = 1;
    /// Integer register holding the address of an element whose storage is
    /// wider than one slot. The element stays in the source buffer the state
    /// keeps alive, so a consumer reads it through the address rather than
    /// from the slot.
    pub const AGGR: u8 = 2;
    /// Integer register holding a managed `String`. Every element a consumer
    /// pulls is a share of its own, so whatever discards one releases it.
    pub const STRING: u8 = 3;
    /// Integer register holding the address of a counted copy blob that holds
    /// one element wider than a slot. Every element a consumer pulls is a
    /// share of its own, so whatever discards one releases it.
    pub const AGGR_COUNTED: u8 = 4;

    /// Whether each pulled element of `class` is a share the puller owns.
    pub const fn counted(class: u8) -> bool {
        matches!(class, STRING | AGGR_COUNTED)
    }
}

/// What a lazy handle's slots mean: their ABI class, plus - for the aggregate
/// class - the width and ownership kind of the element each address points at,
/// so a terminal can rebuild storage of the same shape.
#[derive(Clone, Copy)]
pub struct LazyElemTag {
    class: u8,
    elem_bytes: u32,
    elem_kind: u8,
    /// The vec the addresses point into, for the aggregate class, as an
    /// exposed address. A terminal that rebuilds storage reads the element
    /// layout from it, so a copied slot's pointer-bearing fields get the new
    /// container's share. Zero when no single source backs the stream.
    source: usize,
}

impl LazyElemTag {
    const fn scalar(class: u8) -> Self {
        Self {
            class,
            elem_bytes: 0,
            elem_kind: 0,
            source: 0,
        }
    }
}

/// Opaque lazy `Iterator<i64>` state used by 2027 native iterator lowering.
///
/// `class` is the [`lazy_elem_class`] tag the producing helper stamped on the
/// handle; adapters carry it forward unchanged.
pub struct GosLazyIterI64 {
    inner: Box<dyn Iterator<Item = i64>>,
    tag: LazyElemTag,
}

/// Opaque lazy `Iterator<(i64, i64)>` state used by enumerate and zip.
///
/// `classes` names the [`lazy_elem_class`] of each half, so a consumer that
/// discards a pair gives back the shares a counted half carries.
pub struct GosLazyIterPairI64 {
    inner: Box<dyn Iterator<Item = (i64, i64)>>,
    classes: [u8; 2],
}

struct BorrowedGosVecI64 {
    source: *mut GosVec,
    generation: u64,
    mutation_generation: u64,
    len: i64,
    cap: i64,
    index: i64,
}

impl Iterator for BorrowedGosVecI64 {
    type Item = i64;

    fn next(&mut self) -> Option<Self::Item> {
        if self.source.is_null() || self.index >= self.len {
            return None;
        }
        // SAFETY: construction retains the source header until this state is
        // dropped. Element replacement keeps the header shape unchanged and
        // is intentionally observed by loading the slot on each pull.
        let source = unsafe { &*self.source };
        if source.generation != self.generation
            || source.mutation_generation != self.mutation_generation
            || source.len != self.len
            || source.cap != self.cap
        {
            crate::c_abi::panic::panic_text(
                "borrowed Vec source was structurally mutated during iteration",
            );
            return None;
        }
        let value = unsafe { gos_rt_vec_get_i64(self.source, self.index) };
        self.index += 1;
        Some(value)
    }
}

impl Drop for BorrowedGosVecI64 {
    fn drop(&mut self) {
        // SAFETY: construction retained exactly one source share.
        unsafe { crate::c_abi::map::gos_rt_vec_free(self.source) };
    }
}

/// Wrap an element source as a lazy handle carrying `tag`.
fn lazy_tagged<I>(tag: LazyElemTag, iter: I) -> *mut GosLazyIterI64
where
    I: Iterator<Item = i64> + 'static,
{
    Box::into_raw(Box::new(GosLazyIterI64 {
        inner: Box::new(iter),
        tag,
    }))
}

/// Wrap an element source as a lazy handle tagged with its ABI class.
fn lazy_classed<I>(class: u8, iter: I) -> *mut GosLazyIterI64
where
    I: Iterator<Item = i64> + 'static,
{
    lazy_tagged(LazyElemTag::scalar(class), iter)
}

fn lazy_i64<I>(iter: I) -> *mut GosLazyIterI64
where
    I: Iterator<Item = i64> + 'static,
{
    lazy_classed(lazy_elem_class::WORD, iter)
}

/// Wrap a float source, storing each element as its 64-bit pattern.
fn lazy_f64<I>(iter: I) -> *mut GosLazyIterI64
where
    I: Iterator<Item = f64> + 'static,
{
    lazy_classed(
        lazy_elem_class::FLOAT,
        iter.map(|value| value.to_bits() as i64),
    )
}

struct GosRangeFromI64 {
    current: i64,
    /// Whether stepping past `i64::MAX` raises the overflow panic, as `+`
    /// does in the checked build profiles, or wraps, as it does in release.
    checked: bool,
}

/// The value an open range yields next and the one after it. A checked range
/// has no value after `i64::MAX`; an unchecked one wraps to `i64::MIN`.
fn advance_range_from_i64(current: i64, checked: bool) -> Option<(i64, i64)> {
    if checked && current == i64::MAX {
        None
    } else {
        Some((current, current.wrapping_add(1)))
    }
}

impl Iterator for GosRangeFromI64 {
    type Item = i64;

    fn next(&mut self) -> Option<Self::Item> {
        let Some((out, next)) = advance_range_from_i64(self.current, self.checked) else {
            crate::c_abi::panic::panic_text("attempt to add with overflow in open integer range");
            unreachable!("the raise does not return on the main thread");
        };
        self.current = next;
        Some(out)
    }
}

fn lazy_pair_i64<I>(classes: [u8; 2], iter: I) -> *mut GosLazyIterPairI64
where
    I: Iterator<Item = (i64, i64)> + 'static,
{
    Box::into_raw(Box::new(GosLazyIterPairI64 {
        inner: Box::new(iter),
        classes,
    }))
}

/// Gives back the share a pulled element carries when its consumer does not
/// keep it. Only a `STRING` element is counted; every other class owns
/// nothing beyond its slot.
unsafe fn release_pulled(class: u8, x: i64) {
    match class {
        lazy_elem_class::STRING => {
            let text: *mut c_char = std::ptr::with_exposed_provenance_mut(x as usize);
            // SAFETY: a STRING slot holds a compiler-typed string the puller
            // owns one share of.
            unsafe { crate::c_abi::string::gos_rt_str_free_typed(text) };
        }
        lazy_elem_class::AGGR_COUNTED => {
            let blob: *mut u8 = std::ptr::with_exposed_provenance_mut(x as usize);
            // SAFETY: an AGGR_COUNTED slot holds a copy blob the puller owns
            // one share of.
            unsafe { crate::c_abi::rc::gos_rt_rc_release(blob) };
        }
        _ => {}
    }
}

/// Mints the share a puller owns of an element read out of storage that keeps
/// its own.
unsafe fn share_pulled(class: u8, x: i64) -> i64 {
    match class {
        lazy_elem_class::STRING => {
            let text: *const c_char = std::ptr::with_exposed_provenance(x as usize);
            // SAFETY: the storage the element was read from holds a live share.
            unsafe { crate::c_abi::string::gos_rt_str_retain_typed(text) };
        }
        lazy_elem_class::AGGR_COUNTED => {
            let blob: *mut u8 = std::ptr::with_exposed_provenance_mut(x as usize);
            // SAFETY: the storage the element was read from holds a live share.
            unsafe { crate::c_abi::rc::gos_rt_rc_retain(blob) };
        }
        _ => {}
    }
    x
}

/// `skip(n)` over a counted stream: each skipped element is released.
struct SkipReleasing {
    upstream: Box<dyn Iterator<Item = i64>>,
    remaining: usize,
    class: u8,
}

impl Iterator for SkipReleasing {
    type Item = i64;

    fn next(&mut self) -> Option<i64> {
        while self.remaining > 0 {
            self.remaining -= 1;
            let skipped = self.upstream.next()?;
            unsafe { release_pulled(self.class, skipped) };
        }
        self.upstream.next()
    }
}

/// `step_by(step)` over a counted stream: each element stepped over is
/// released.
struct StepByReleasing {
    upstream: Box<dyn Iterator<Item = i64>>,
    step: usize,
    started: bool,
    class: u8,
}

impl Iterator for StepByReleasing {
    type Item = i64;

    fn next(&mut self) -> Option<i64> {
        if self.started {
            for _ in 1..self.step {
                let skipped = self.upstream.next()?;
                unsafe { release_pulled(self.class, skipped) };
            }
        } else {
            self.started = true;
        }
        self.upstream.next()
    }
}

/// `zip` whose left half is released when the right side ends first.
struct ZipReleasing {
    left: Box<dyn Iterator<Item = i64>>,
    right: Box<dyn Iterator<Item = i64>>,
    left_class: u8,
}

impl Iterator for ZipReleasing {
    type Item = (i64, i64);

    fn next(&mut self) -> Option<(i64, i64)> {
        let a = self.left.next()?;
        if let Some(b) = self.right.next() {
            Some((a, b))
        } else {
            unsafe { release_pulled(self.left_class, a) };
            None
        }
    }
}

/// Yields `remaining` shares of one `String`, holding a share of its own for
/// as long as the state lives.
struct RepeatStr {
    value: i64,
    remaining: usize,
}

impl Iterator for RepeatStr {
    type Item = i64;

    fn next(&mut self) -> Option<i64> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        Some(unsafe { share_pulled(lazy_elem_class::STRING, self.value) })
    }
}

impl Drop for RepeatStr {
    fn drop(&mut self) {
        unsafe { release_pulled(lazy_elem_class::STRING, self.value) };
    }
}

/// Consume a lazy handle, yielding its element source and ABI-class tag.
/// Adapters that neither read nor produce element values use this so the
/// class survives the chain unchanged.
unsafe fn take_lazy_tagged(
    iter: *mut GosLazyIterI64,
) -> (Box<dyn Iterator<Item = i64>>, LazyElemTag) {
    if iter.is_null() {
        (
            Box::new(std::iter::empty()),
            LazyElemTag::scalar(lazy_elem_class::WORD),
        )
    } else {
        // SAFETY: lazy iterator helpers are linear; consuming a helper
        // argument transfers ownership of the opaque state to this function.
        let state = unsafe { Box::from_raw(iter) };
        (state.inner, state.tag)
    }
}

/// Consume a handle whose slots are integer-register words: an element value
/// or - for the aggregate class - an element's address. Both reach a callback
/// the same way; only a float slot would be read in the wrong register file,
/// so that is what this rejects.
unsafe fn take_lazy_word(
    iter: *mut GosLazyIterI64,
) -> (Box<dyn Iterator<Item = i64>>, LazyElemTag) {
    // SAFETY: same linear-ownership contract as `take_lazy_tagged`.
    let (inner, tag) = unsafe { take_lazy_tagged(iter) };
    if tag.class == lazy_elem_class::FLOAT {
        class_mismatch(tag.class, lazy_elem_class::WORD);
    }
    (inner, tag)
}

/// Consume a lazy handle whose elements this helper reads as `want`.
///
/// A handle produced for one class and consumed as another reinterprets every
/// slot, so the tags are checked here: this is the one place a producer and a
/// consumer meet.
unsafe fn take_lazy_as(iter: *mut GosLazyIterI64, want: u8) -> Box<dyn Iterator<Item = i64>> {
    // SAFETY: same linear-ownership contract as `take_lazy_tagged`.
    let (inner, tag) = unsafe { take_lazy_tagged(iter) };
    if tag.class != want {
        class_mismatch(tag.class, want);
    }
    inner
}

/// Ends the process when the two ends of a lazy pipeline disagree about what
/// its slots hold.
///
/// The producer and the consumer of a handle have to read the same bits out of
/// the same register file; a disagreement is a codegen fault, and there is no
/// value to answer with. Raising it as a panic would let the FFI catch turn it
/// into a sentinel - an empty sequence or a zero - which is how this class of
/// fault reached programs as a wrong number instead of a stopped process.
#[cold]
#[inline(never)]
fn class_mismatch(carried: u8, want: u8) -> ! {
    eprintln!(
        "gossamer runtime: lazy iterator element class mismatch - \
         the handle carries class {carried} and the consumer reads class {want}. \
         This is a code generation fault; the program cannot continue."
    );
    unsafe { gos_rt_flush_stdout() };
    std::process::abort();
}

unsafe fn take_lazy_i64(iter: *mut GosLazyIterI64) -> Box<dyn Iterator<Item = i64>> {
    // SAFETY: same linear-ownership contract as `take_lazy_tagged`.
    unsafe { take_lazy_as(iter, lazy_elem_class::WORD) }
}

/// Consume a float-carrying lazy handle as `f64` values.
unsafe fn take_lazy_f64(iter: *mut GosLazyIterI64) -> impl Iterator<Item = f64> {
    // SAFETY: same linear-ownership contract as `take_lazy_tagged`.
    unsafe { take_lazy_as(iter, lazy_elem_class::FLOAT) }.map(|bits| f64::from_bits(bits as u64))
}

/// Declared width of a mapped element, clamped to the widths the Vec storage
/// addresses. A zero or negative argument means the call site had no width to
/// declare, which reads back as the word-slot default.
///
/// A width past one word is a flat slot block - a tuple or struct result - and
/// the output vec strides by the whole block, because every reader addresses
/// such an element inline rather than through a handle.
fn mapped_stride(out_bytes: i64) -> u32 {
    match out_bytes {
        1 | 2 | 4 => out_bytes as u32,
        n if n > 8 => u32::try_from(n).unwrap_or(8),
        _ => 8,
    }
}

/// Appends one mapped result to `out`.
///
/// A word-wide element is the value itself. A wider one is a flat slot block
/// the callback returns the address of, so the block's bytes are copied into
/// the element's own storage.
unsafe fn push_mapped(out: *mut GosVec, y: i64, out_bytes: i64, by_block: bool) {
    if by_block || out_bytes > 8 {
        let block = std::ptr::with_exposed_provenance_mut::<u8>(y as usize);
        if !block.is_null() {
            unsafe { crate::c_abi::vec::gos_rt_vec_push(out, block) };
            // The callback answered a heap copy of its result, whose words -
            // and the shares they carry - now live in the vec's element.
            let bytes = u64::try_from(out_bytes.max(1)).unwrap_or(8).div_ceil(8) * 8;
            crate::c_abi::gc::gos_rt_aggr_free(block, bytes);
        }
    } else {
        unsafe { gos_rt_vec_push_i64(out, y) };
    }
}

/// A fresh output vec carrying the same element width and ownership kind as
/// `src`.
///
/// An element-preserving combinator writes the source's own elements back out,
/// so the result must declare the same stride: a byte-strided `bool` or `u8`
/// sequence copied into a word-strided header would be read at the wrong
/// offsets by every indexed access.
unsafe fn vec_like_source(src: *const GosVec, capacity: i64) -> *mut GosVec {
    if src.is_null() {
        return unsafe { gos_rt_vec_new(8) };
    }
    // SAFETY: the caller supplies a live GosVec header.
    let source = unsafe { &*src };
    unsafe {
        crate::c_abi::vec::gos_rt_vec_with_capacity_typed(
            source.elem_bytes,
            capacity.max(0),
            source.elem_kind,
        )
    }
}

/// A Gossamer closure body reached through its env blob.
///
/// The env's first word is the body address; a null env or a zero address is a
/// closure the lowering could not materialise, and every caller answers with
/// its identity result rather than calling through a wild pointer.
unsafe fn lazy_callback<F: Copy>(env: *const u8) -> Option<F> {
    debug_assert_eq!(size_of::<F>(), size_of::<usize>());
    if env.is_null() {
        return None;
    }
    // SAFETY: the closure ABI places the body address at env[0].
    let raw = unsafe { (env as *const usize).read() };
    if raw == 0 {
        return None;
    }
    // SAFETY: `raw` addresses a closure body the lowering compiled to the
    // signature this combinator selected from its element classes.
    Some(unsafe { std::mem::transmute_copy::<usize, F>(&raw) })
}

type CallF64F64 = unsafe extern "C" fn(env: *const u8, x: f64) -> f64;
type CallF64Word = unsafe extern "C" fn(env: *const u8, x: f64) -> i64;
type CallWordF64 = unsafe extern "C" fn(env: *const u8, x: i64) -> f64;
type PredF64 = unsafe extern "C" fn(env: *const u8, x: f64) -> bool;
type FoldF64F64 = unsafe extern "C" fn(env: *const u8, acc: f64, x: f64) -> f64;
type FoldF64Word = unsafe extern "C" fn(env: *const u8, acc: f64, x: i64) -> f64;
type FoldWordF64 = unsafe extern "C" fn(env: *const u8, acc: i64, x: f64) -> i64;
type FoldWordPtr = unsafe extern "C" fn(env: *const u8, acc: i64, x: *const u8) -> i64;
type FoldF64Ptr = unsafe extern "C" fn(env: *const u8, acc: f64, x: *const u8) -> f64;
type CallPtrF64 = unsafe extern "C" fn(env: *const u8, x: *const u8) -> f64;
type CallPtrWord = unsafe extern "C" fn(env: *const u8, x: *const u8) -> i64;
type PredPtr = unsafe extern "C" fn(env: *const u8, x: *const u8) -> bool;

unsafe fn take_lazy_pair_i64(
    iter: *mut GosLazyIterPairI64,
) -> (Box<dyn Iterator<Item = (i64, i64)>>, [u8; 2]) {
    if iter.is_null() {
        (
            Box::new(std::iter::empty()),
            [lazy_elem_class::WORD, lazy_elem_class::WORD],
        )
    } else {
        // SAFETY: lazy iterator helpers are linear; consuming a helper
        // argument transfers ownership of the opaque state to this function.
        let state = unsafe { Box::from_raw(iter) };
        (state.inner, state.classes)
    }
}

/// Release an unconsumed lazy i64 iterator and its complete adapter chain.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_drop_i64(iter: *mut GosLazyIterI64) {
    ffi_entry!((), {
        if !iter.is_null() {
            // SAFETY: an unconsumed iterator handle has exactly one owner.
            // Dropping the outer box recursively drops every upstream adapter.
            drop(unsafe { Box::from_raw(iter) });
        }
    });
}

/// Release an unconsumed lazy pair iterator and its complete adapter chain.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_drop_pair_i64(iter: *mut GosLazyIterPairI64) {
    ffi_entry!((), {
        if !iter.is_null() {
            // SAFETY: an unconsumed iterator handle has exactly one owner.
            drop(unsafe { Box::from_raw(iter) });
        }
    });
}

/// Advance a lazy i64 iterator without consuming the iterator handle.
///
/// Returns `Option<i64>` in the runtime's packed i128 carrier: discriminant 0
/// for `Some(value)`, discriminant 1 for `None`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_next_i64(iter: *mut GosLazyIterI64) -> i128 {
    ffi_entry!(gos_rt_result_new(1, 0), {
        if iter.is_null() {
            return gos_rt_result_new(1, 0);
        }
        // SAFETY: the handle remains owned by the caller. This helper only
        // advances the state in place, so repeated calls after exhaustion keep
        // returning None through the underlying iterator contract.
        match unsafe { &mut *iter }.inner.next() {
            Some(value) => gos_rt_result_new(0, value),
            None => gos_rt_result_new(1, 0),
        }
    })
}

/// Advance a lazy pair iterator without consuming the iterator handle.
///
/// Returns `Option<(i64, i64)>` in the packed i128 carrier: discriminant 0
/// with the address of a two-slot block holding the pair, or discriminant 1
/// for `None`. The block is the same collector-owned copy an `Option` over a
/// wide sequence element carries, so the pair stays readable after the next
/// pull.
/// Copy-blob layout of a pulled pair: two words and no counted child.
static PAIR_BLOB_META: [i64; 2] = [gossamer_abi::rc::RC_KIND_STRUCT_GUARDED, 0];

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_next_pair_i64(iter: *mut GosLazyIterPairI64) -> i128 {
    ffi_entry!(gos_rt_result_new(1, 0), {
        if iter.is_null() {
            return gos_rt_result_new(1, 0);
        }
        // SAFETY: the handle remains owned by the caller; this only advances
        // the state in place.
        let Some((first, second)) = unsafe { &mut *iter }.inner.next() else {
            return gos_rt_result_new(1, 0);
        };
        // The pair is a counted leaf blob the `Some` payload owns, so whatever
        // releases that option slot gives the pair's storage back. Its words
        // are the elements themselves, whose shares the puller already holds.
        let words = [first, second];
        // SAFETY: `words` is 16 readable bytes and the leaf meta names no child.
        let blob = unsafe {
            crate::c_abi::rc::gos_rt_rc_alloc_move(
                16,
                PAIR_BLOB_META.as_ptr(),
                words.as_ptr().cast::<u8>(),
            )
        };
        if blob.is_null() {
            crate::c_abi::panic::panic_text("lazy pair iterator: out of memory for the pair");
            return gos_rt_result_new(1, 0);
        }
        gos_rt_result_new(0, blob.expose_provenance() as i64)
    })
}

/// Hands a lazy pair iterator on as a stream of pair addresses.
///
/// A word adapter reads an element wider than one slot through its address, so
/// each pair becomes the counted two-word blob [`gos_rt_lazy_iter_next_pair_i64`]
/// answers, a share the puller owns and gives back when it discards the pair.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_pair_blobs(
    iter: *mut GosLazyIterPairI64,
) -> *mut GosLazyIterI64 {
    ffi_entry!(std::ptr::null_mut(), {
        let (pairs, _classes) = unsafe { take_lazy_pair_i64(iter) };
        let tag = LazyElemTag {
            class: lazy_elem_class::AGGR_COUNTED,
            elem_bytes: 16,
            elem_kind: super::vec::vec_elem_kind::PRIMITIVE,
            source: 0,
        };
        lazy_tagged(
            tag,
            pairs.map(|(first, second)| {
                let words = [first, second];
                // SAFETY: `words` is 16 readable bytes and the leaf meta names
                // no child.
                let blob = unsafe {
                    crate::c_abi::rc::gos_rt_rc_alloc_move(
                        16,
                        PAIR_BLOB_META.as_ptr(),
                        words.as_ptr().cast::<u8>(),
                    )
                };
                if blob.is_null() {
                    crate::c_abi::panic::panic_text(
                        "lazy pair iterator: out of memory for the pair",
                    );
                }
                blob.expose_provenance() as i64
            }),
        )
    })
}

/// Lazy `[start, end)` i64 range.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_range_i64(start: i64, end: i64) -> *mut GosLazyIterI64 {
    ffi_entry!(std::ptr::null_mut(), { lazy_i64(start..end) })
}

/// Lazy `start..` i64 range for the release profile: it yields `i64::MAX`,
/// wraps, and continues, as `+` does there.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_range_from_i64(start: i64) -> *mut GosLazyIterI64 {
    ffi_entry!(std::ptr::null_mut(), {
        lazy_i64(GosRangeFromI64 {
            current: start,
            checked: false,
        })
    })
}

/// Lazy `start..` i64 range for the checked build profiles: stepping past
/// `i64::MAX` raises the overflow panic `+` raises there.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_range_from_i64_checked(
    start: i64,
) -> *mut GosLazyIterI64 {
    ffi_entry!(std::ptr::null_mut(), {
        lazy_i64(GosRangeFromI64 {
            current: start,
            checked: true,
        })
    })
}

/// Lazy `[start, end]` i64 range.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_range_inclusive_i64(
    start: i64,
    end: i64,
) -> *mut GosLazyIterI64 {
    ffi_entry!(std::ptr::null_mut(), { lazy_i64(start..=end) })
}

/// Borrow `source` as a lazy element stream tagged with `class`.
///
/// The lazy state yields one 8-byte slot per element, so a source whose slots
/// are wider holds elements this shape cannot address. That is a lowering
/// error rather than a recoverable input, and reading it as slots would walk
/// off the element boundary, so it stops here with a diagnosable panic.
unsafe fn lazy_from_vec_classed(source: *mut GosVec, class: u8) -> *mut GosLazyIterI64 {
    if source.is_null() {
        return lazy_classed(class, std::iter::empty());
    }
    // SAFETY: the caller supplies a live GosVec header.
    let header = unsafe { &*source };
    if header.elem_bytes > 8 {
        crate::c_abi::panic::panic_text(
            "lazy iterator source holds multi-slot elements; use the eager sequence surface",
        );
    }
    // SAFETY: retaining the header gives the iterator state its own share
    // until `BorrowedGosVecI64::drop`.
    unsafe { gos_rt_vec_retain(source) };
    let borrowed = BorrowedGosVecI64 {
        source,
        generation: header.generation,
        mutation_generation: header.mutation_generation,
        len: header.len,
        cap: header.cap,
        index: 0,
    };
    if class == lazy_elem_class::STRING {
        // The vec keeps its own share of each element, so a pull mints the
        // puller's.
        return lazy_classed(
            class,
            borrowed.map(|x| unsafe { share_pulled(lazy_elem_class::STRING, x) }),
        );
    }
    lazy_classed(class, borrowed)
}

/// Lazy borrowed source over a `Vec<i64>`. The source header is retained so
/// unpulled elements stay live. Element replacement is visible, while a
/// length, capacity, or allocation-identity change fails on the next pull.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_from_vec_i64(source: *mut GosVec) -> *mut GosLazyIterI64 {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { lazy_from_vec_classed(source, lazy_elem_class::WORD) }
    })
}

/// Lazy borrowed source over a `Vec<String>`. Each pull hands the puller a
/// share of the element, so the stream's consumers own what they pull.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_from_vec_str(source: *mut GosVec) -> *mut GosLazyIterI64 {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { lazy_from_vec_classed(source, lazy_elem_class::STRING) }
    })
}

/// Lazy borrowed source over a `Vec<T>` whose element is wider than one slot.
/// Each pull yields the address of the element's storage inside the retained
/// source, which is the same shape a callback over an eager multi-slot
/// sequence receives.
struct BorrowedGosVecAggr {
    source: *mut GosVec,
    generation: u64,
    mutation_generation: u64,
    len: i64,
    cap: i64,
    index: i64,
}

impl Iterator for BorrowedGosVecAggr {
    type Item = i64;

    fn next(&mut self) -> Option<Self::Item> {
        if self.source.is_null() || self.index >= self.len {
            return None;
        }
        // SAFETY: construction retains the source header until this state is
        // dropped, so the buffer the addresses point into stays live.
        let source = unsafe { &*self.source };
        if source.generation != self.generation
            || source.mutation_generation != self.mutation_generation
            || source.len != self.len
            || source.cap != self.cap
        {
            crate::c_abi::panic::panic_text(
                "borrowed Vec source was structurally mutated during iteration",
            );
            return None;
        }
        // SAFETY: `index` is in `[0, len)` against this same live header.
        let slot = unsafe { crate::c_abi::signal::gos_rt_vec_get_ptr(self.source, self.index) };
        self.index += 1;
        Some(slot as i64)
    }
}

impl Drop for BorrowedGosVecAggr {
    fn drop(&mut self) {
        // SAFETY: construction retained exactly one source share.
        unsafe { crate::c_abi::map::gos_rt_vec_free(self.source) };
    }
}

/// Borrow `source` as a lazy stream of element addresses.
///
/// This is the multi-slot counterpart of [`gos_rt_lazy_iter_from_vec_i64`]:
/// the element stays where it is and its address rides the slot, so an element
/// of any width reaches a callback without being packed into one.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_from_vec_aggr(
    source: *mut GosVec,
) -> *mut GosLazyIterI64 {
    ffi_entry!(std::ptr::null_mut(), {
        if source.is_null() {
            return lazy_classed(lazy_elem_class::AGGR, std::iter::empty());
        }
        // SAFETY: the caller supplies a live GosVec header.
        let header = unsafe { &*source };
        let tag = LazyElemTag {
            class: lazy_elem_class::AGGR,
            elem_bytes: header.elem_bytes,
            elem_kind: header.elem_kind,
            source: source.expose_provenance(),
        };
        // SAFETY: retaining the header gives the iterator state its own share
        // until `BorrowedGosVecAggr::drop`.
        unsafe { gos_rt_vec_retain(source) };
        lazy_tagged(
            tag,
            BorrowedGosVecAggr {
                source,
                generation: header.generation,
                mutation_generation: header.mutation_generation,
                len: header.len,
                cap: header.cap,
                index: 0,
            },
        )
    })
}

/// Consume a lazy stream of element addresses into a `GosVec` of the same
/// element shape, copying each element out whole and minting the container's
/// share of any pointer-bearing field.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_collect_aggr(iter: *mut GosLazyIterI64) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let (upstream, tag) = unsafe { take_lazy_tagged(iter) };
        debug_assert!(
            matches!(
                tag.class,
                lazy_elem_class::AGGR | lazy_elem_class::AGGR_COUNTED
            ),
            "lazy aggregate collect reads a handle of class {}",
            tag.class
        );
        let out = unsafe {
            crate::c_abi::vec::gos_rt_vec_with_capacity_typed(tag.elem_bytes, 0, tag.elem_kind)
        };
        let mut upstream = upstream;
        for slot in upstream.by_ref() {
            let addr: *const u8 = std::ptr::with_exposed_provenance(slot as usize);
            if addr.is_null() {
                continue;
            }
            // SAFETY: every yielded address points at one element of a live
            // source whose width is the one the output vec was built with.
            unsafe { gos_rt_vec_push(out, addr) };
            if tag.class == lazy_elem_class::AGGR_COUNTED {
                // The element's words, and the child shares they carry, now
                // live in the vec, so the blob goes back without its children.
                unsafe { crate::c_abi::rc::release_blob_moved(addr.cast_mut()) };
            }
        }
        // The copied slots are raw copies of the source's, so any
        // pointer-bearing field needs the output's own share. Done while the
        // upstream state - and with it the source's retained share - is still
        // alive, so the layout it describes is still there to read.
        if tag.source != 0 {
            let source: *const GosVec = std::ptr::with_exposed_provenance(tag.source);
            // SAFETY: the source header is retained by the upstream state,
            // which is dropped only after this call.
            unsafe { crate::c_abi::vec::vec_share_owned_elements(source, out) };
        }
        drop(upstream);
        out
    })
}

/// Lazy borrowed source over a `Vec<f64>`, tagged so the float terminals read
/// each slot as a double. Same borrow contract as the word-slot form.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_from_vec_f64(source: *mut GosVec) -> *mut GosLazyIterI64 {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { lazy_from_vec_classed(source, lazy_elem_class::FLOAT) }
    })
}

/// Lazy repeat of an i64 value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_repeat_i64(value: i64, n: i64) -> *mut GosLazyIterI64 {
    ffi_entry!(std::ptr::null_mut(), {
        if n < 0 {
            crate::c_abi::panic::panic_text("iter::repeat: count must be non-negative");
        }
        let n = usize::try_from(n).unwrap_or(0);
        lazy_i64(std::iter::repeat_n(value, n))
    })
}

/// Walks a String's Unicode scalars from an owned copy of its text. The
/// cursor keeps the text and a byte position rather than one slot per scalar,
/// so the walk costs the text once instead of eight bytes per character.
struct StrScalars {
    text: String,
    byte_index: usize,
}

impl Iterator for StrScalars {
    type Item = i64;

    fn next(&mut self) -> Option<i64> {
        let c = self.text[self.byte_index..].chars().next()?;
        self.byte_index += c.len_utf8();
        Some(i64::from(u32::from(c)))
    }
}

/// Walks a String's UTF-8 bytes from an owned copy of its text.
struct StrBytes {
    text: String,
    index: usize,
}

impl Iterator for StrBytes {
    type Item = i64;

    fn next(&mut self) -> Option<i64> {
        let byte = *self.text.as_bytes().get(self.index)?;
        self.index += 1;
        Some(i64::from(byte))
    }
}

/// Lazy cursor over a String's Unicode scalars, each yielded as its code
/// point.
///
/// # Safety
/// `s` is a live NUL-terminated string pointer or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_str_chars(s: *const c_char) -> *mut GosLazyIterI64 {
    ffi_entry!(std::ptr::null_mut(), {
        let text = unsafe { crate::c_abi::gos_str_arg_string(s) };
        lazy_i64(StrScalars {
            text,
            byte_index: 0,
        })
    })
}

/// Lazy cursor over a String's UTF-8 bytes.
///
/// # Safety
/// `s` is a live NUL-terminated string pointer or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_str_bytes(s: *const c_char) -> *mut GosLazyIterI64 {
    ffi_entry!(std::ptr::null_mut(), {
        let text = unsafe { crate::c_abi::gos_str_arg_string(s) };
        lazy_i64(StrBytes { text, index: 0 })
    })
}

/// Lazy single-item i64 iterator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_once_i64(value: i64) -> *mut GosLazyIterI64 {
    ffi_entry!(std::ptr::null_mut(), { lazy_i64(std::iter::once(value)) })
}

/// Lazy repeat of a `String` `n` times. The state takes a share of `value`,
/// since the caller's binding may end before the stream is drained.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_repeat_str(value: i64, n: i64) -> *mut GosLazyIterI64 {
    ffi_entry!(std::ptr::null_mut(), {
        if n < 0 {
            crate::c_abi::panic::panic_text("iter::repeat: count must be non-negative");
        }
        let remaining = usize::try_from(n).unwrap_or(0);
        let value = unsafe { share_pulled(lazy_elem_class::STRING, value) };
        lazy_classed(lazy_elem_class::STRING, RepeatStr { value, remaining })
    })
}

/// Lazy single-item `String` iterator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_once_str(value: i64) -> *mut GosLazyIterI64 {
    ffi_entry!(std::ptr::null_mut(), {
        let value = unsafe { share_pulled(lazy_elem_class::STRING, value) };
        lazy_classed(
            lazy_elem_class::STRING,
            RepeatStr {
                value,
                remaining: 1,
            },
        )
    })
}

/// Lazy `take(n)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_take_i64(
    n: i64,
    iter: *mut GosLazyIterI64,
) -> *mut GosLazyIterI64 {
    ffi_entry!(std::ptr::null_mut(), {
        if n < 0 {
            crate::c_abi::panic::panic_text("iter::take: count must be non-negative");
        }
        let n = usize::try_from(n).unwrap_or(0);
        let (upstream, tag) = unsafe { take_lazy_tagged(iter) };
        lazy_tagged(tag, upstream.take(n))
    })
}

/// Lazy `step_by(step)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_step_by_i64(
    step: i64,
    iter: *mut GosLazyIterI64,
) -> *mut GosLazyIterI64 {
    ffi_entry!(std::ptr::null_mut(), {
        if step <= 0 {
            crate::c_abi::panic::panic_text("iter::step_by: step must be positive");
        }
        let step = usize::try_from(step).unwrap_or(1);
        let (upstream, tag) = unsafe { take_lazy_tagged(iter) };
        if lazy_elem_class::counted(tag.class) {
            return lazy_tagged(
                tag,
                StepByReleasing {
                    upstream,
                    step,
                    started: false,
                    class: tag.class,
                },
            );
        }
        lazy_tagged(tag, upstream.step_by(step))
    })
}

/// Lazy `skip(n)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_skip_i64(
    n: i64,
    iter: *mut GosLazyIterI64,
) -> *mut GosLazyIterI64 {
    ffi_entry!(std::ptr::null_mut(), {
        if n < 0 {
            crate::c_abi::panic::panic_text("iter::skip: count must be non-negative");
        }
        let n = usize::try_from(n).unwrap_or(0);
        let (upstream, tag) = unsafe { take_lazy_tagged(iter) };
        if lazy_elem_class::counted(tag.class) {
            return lazy_tagged(
                tag,
                SkipReleasing {
                    upstream,
                    remaining: n,
                    class: tag.class,
                },
            );
        }
        lazy_tagged(tag, upstream.skip(n))
    })
}

/// Lazy `chain(other)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_chain_i64(
    first: *mut GosLazyIterI64,
    second: *mut GosLazyIterI64,
) -> *mut GosLazyIterI64 {
    ffi_entry!(std::ptr::null_mut(), {
        let (first, first_tag) = unsafe { take_lazy_tagged(first) };
        let (second, second_tag) = unsafe { take_lazy_tagged(second) };
        let second_class = second_tag.class;
        let class = first_tag.class;
        debug_assert_eq!(
            class, second_class,
            "iter::chain joins two element classes: {class} and {second_class}"
        );
        // Both sides stream one element type, so either side's source vec
        // describes the layout a collecting terminal reads child shares from;
        // the joined stream names whichever side has one.
        let tag = if first_tag.source == 0 {
            second_tag
        } else {
            first_tag
        };
        lazy_tagged(tag, first.chain(second))
    })
}

/// Lazy `enumerate`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_enumerate_i64(
    iter: *mut GosLazyIterI64,
) -> *mut GosLazyIterPairI64 {
    ffi_entry!(std::ptr::null_mut(), {
        let (upstream, tag) = unsafe { take_lazy_word(iter) };
        lazy_pair_i64(
            [lazy_elem_class::WORD, tag.class],
            upstream
                .enumerate()
                .map(|(idx, value)| (i64::try_from(idx).unwrap_or(i64::MAX), value)),
        )
    })
}

/// Lazy `zip`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_zip_i64(
    left: *mut GosLazyIterI64,
    right: *mut GosLazyIterI64,
) -> *mut GosLazyIterPairI64 {
    ffi_entry!(std::ptr::null_mut(), {
        let (left, left_tag) = unsafe { take_lazy_word(left) };
        let (right, right_tag) = unsafe { take_lazy_word(right) };
        let classes = [left_tag.class, right_tag.class];
        if lazy_elem_class::counted(left_tag.class) {
            return lazy_pair_i64(
                classes,
                ZipReleasing {
                    left,
                    right,
                    left_class: left_tag.class,
                },
            );
        }
        lazy_pair_i64(classes, left.zip(right))
    })
}

/// A share of a closure environment, held for as long as the lazy adapter
/// that calls it.
///
/// A lazy adapter stores its callback's environment and invokes it when the
/// pipeline is finally consumed, which is after the frame that built the
/// adapter has dropped its own share. The share taken here is released when
/// the adapter is.
struct EnvShare(*const u8);

impl EnvShare {
    /// Takes a share of `env`, which must be a reference-counted payload.
    unsafe fn new(env: *const u8) -> Self {
        unsafe { crate::c_abi::gos_rt_rc_retain(env.cast_mut()) };
        Self(env)
    }

    fn ptr(&self) -> *const u8 {
        self.0
    }
}

impl Drop for EnvShare {
    fn drop(&mut self) {
        unsafe { crate::c_abi::gos_rt_rc_release(self.0.cast_mut()) };
    }
}

// SAFETY: the environment is a reference-counted blob of code addresses and
// captured values, and the adapter holding this share may be consumed on a
// different worker thread than the one that built it.
unsafe impl Send for EnvShare {}

/// Lazy `map(f)` over word-register elements whose results are of
/// `out_class`.
///
/// The callback reads its argument without taking it, so the pulled element's
/// share is released once the callback has answered; the result is the
/// callback's own value, a fresh share when it is counted.
unsafe fn lazy_map_word(
    env: *const u8,
    iter: *mut GosLazyIterI64,
    out_class: u8,
) -> *mut GosLazyIterI64 {
    type CallFn = unsafe extern "C" fn(env: *const u8, x: i64) -> i64;
    let (upstream, tag) = unsafe { take_lazy_word(iter) };
    let Some(f) = (unsafe { lazy_callback::<CallFn>(env) }) else {
        return lazy_classed(out_class, std::iter::empty());
    };
    let share = unsafe { EnvShare::new(env) };
    let in_class = tag.class;
    lazy_classed(
        out_class,
        upstream.map(move |x| unsafe {
            let y = f(share.ptr(), x);
            release_pulled(in_class, x);
            y
        }),
    )
}

/// Lazy `map(f)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_map_i64(
    env: *const u8,
    iter: *mut GosLazyIterI64,
) -> *mut GosLazyIterI64 {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { lazy_map_word(env, iter, lazy_elem_class::WORD) }
    })
}

/// Lazy `map(f)` whose callback answers a `String`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_map_str(
    env: *const u8,
    iter: *mut GosLazyIterI64,
) -> *mut GosLazyIterI64 {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { lazy_map_word(env, iter, lazy_elem_class::STRING) }
    })
}

/// Lazy `filter_map(f)` over word-register elements whose callback answers an
/// `Option` carrying a payload of `out_class`.
///
/// The callback reads its argument without taking it, so the pulled element's
/// share is released once it has answered; a `Some` payload is the callback's
/// own value, a fresh share when it is counted, and a `None` holds nothing.
unsafe fn lazy_filter_map_word(
    env: *const u8,
    iter: *mut GosLazyIterI64,
    out_class: u8,
) -> *mut GosLazyIterI64 {
    type OptionFn = unsafe extern "C" fn(env: *const u8, x: i64) -> i128;
    let (upstream, tag) = unsafe { take_lazy_word(iter) };
    let Some(f) = (unsafe { lazy_callback::<OptionFn>(env) }) else {
        return lazy_classed(out_class, std::iter::empty());
    };
    let share = unsafe { EnvShare::new(env) };
    let in_class = tag.class;
    lazy_classed(
        out_class,
        upstream.filter_map(move |x| unsafe {
            let answer = f(share.ptr(), x);
            release_pulled(in_class, x);
            (super::vec::gos_rt_result_disc(answer) == 0)
                .then(|| super::vec::gos_rt_result_payload(answer))
        }),
    )
}

/// Lazy `filter_map(f)` whose callback answers `Option<i64>`, `Option<bool>`,
/// or `Option<char>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_filter_map_i64(
    env: *const u8,
    iter: *mut GosLazyIterI64,
) -> *mut GosLazyIterI64 {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { lazy_filter_map_word(env, iter, lazy_elem_class::WORD) }
    })
}

/// Lazy `filter_map(f)` whose callback answers `Option<String>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_filter_map_str(
    env: *const u8,
    iter: *mut GosLazyIterI64,
) -> *mut GosLazyIterI64 {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { lazy_filter_map_word(env, iter, lazy_elem_class::STRING) }
    })
}

/// Lazy `map(f)` whose callback answers a struct, tuple, or array.
///
/// Such a callback returns the address of the element's flat slot block, a
/// collector-owned allocation, so the state yields that address and a consumer
/// reads the element through it. `out_bytes` is the element's width, which a
/// terminal that rebuilds storage strides by.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_map_aggr(
    env: *const u8,
    iter: *mut GosLazyIterI64,
    out_bytes: i64,
) -> *mut GosLazyIterI64 {
    ffi_entry!(std::ptr::null_mut(), {
        let (upstream, in_tag) = unsafe { take_lazy_word(iter) };
        let tag = LazyElemTag {
            class: lazy_elem_class::AGGR,
            elem_bytes: mapped_stride(out_bytes),
            elem_kind: 0,
            source: 0,
        };
        type CallFn = unsafe extern "C" fn(env: *const u8, x: i64) -> i64;
        let Some(f) = (unsafe { lazy_callback::<CallFn>(env) }) else {
            return lazy_tagged(tag, std::iter::empty());
        };
        let share = unsafe { EnvShare::new(env) };
        let in_class = in_tag.class;
        lazy_tagged(
            tag,
            upstream.map(move |x| unsafe {
                let y = f(share.ptr(), x);
                release_pulled(in_class, x);
                y
            }),
        )
    })
}

/// Makes each element of a lazy `map` over struct, tuple, or array results a
/// counted share its consumer owns.
///
/// The callback answers a block it allocated and no one owns. Each pull moves
/// that block's words, with the child shares they carry, into a copy blob
/// `meta` describes, so a consumer that discards an element releases it and
/// one that keeps an element keeps a counted value. Only a mapped stream is
/// converted: a stream borrowed from a vec already has an owner.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_set_elem_meta(
    iter: *mut GosLazyIterI64,
    meta: *const i64,
) {
    ffi_entry!((), {
        if iter.is_null() || meta.is_null() {
            return;
        }
        // SAFETY: the caller hands over a live lazy handle it still owns.
        let state = unsafe { &mut *iter };
        if state.tag.class != lazy_elem_class::AGGR || state.tag.source != 0 {
            return;
        }
        let bytes = u64::from(state.tag.elem_bytes).div_ceil(8) * 8;
        let meta_addr = meta.expose_provenance();
        let upstream = std::mem::replace(&mut state.inner, Box::new(std::iter::empty()));
        state.inner = Box::new(upstream.map(move |word| {
            let block: *mut u8 = std::ptr::with_exposed_provenance_mut(word as usize);
            if block.is_null() {
                return word;
            }
            let meta: *const i64 = std::ptr::with_exposed_provenance(meta_addr);
            // SAFETY: the block holds `bytes` of one element the callback built,
            // and the blob takes the shares its words carry.
            let blob = unsafe { crate::c_abi::rc::gos_rt_rc_alloc_move(bytes, meta, block) };
            if blob.is_null() {
                eprintln!(
                    "gossamer runtime: OOM counting a mapped element ({bytes} bytes); aborting"
                );
                std::process::abort();
            }
            crate::c_abi::gc::gos_rt_aggr_free(block, bytes);
            blob.expose_provenance() as i64
        }));
        state.tag.class = lazy_elem_class::AGGR_COUNTED;
    });
}

/// Lazy `filter(p)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_filter_i64(
    env: *const u8,
    iter: *mut GosLazyIterI64,
) -> *mut GosLazyIterI64 {
    ffi_entry!(std::ptr::null_mut(), {
        let (upstream, tag) = unsafe { take_lazy_word(iter) };
        if env.is_null() {
            return lazy_i64(std::iter::empty());
        }
        type PredFn = unsafe extern "C" fn(env: *const u8, x: i64) -> bool;
        let fn_addr_raw = unsafe { (env as *const usize).read() };
        if fn_addr_raw == 0 {
            return lazy_i64(std::iter::empty());
        }
        let p: PredFn = unsafe { std::mem::transmute(fn_addr_raw) };
        let share = unsafe { EnvShare::new(env) };
        let class = tag.class;
        lazy_tagged(
            tag,
            upstream.filter(move |x| unsafe {
                let keep = p(share.ptr(), *x);
                if !keep {
                    release_pulled(class, *x);
                }
                keep
            }),
        )
    })
}

/// Consume a lazy i64 iterator into a `GosVec`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_collect_i64(iter: *mut GosLazyIterI64) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let (upstream, tag) = unsafe { take_lazy_tagged(iter) };
        // A counted element's pulled share becomes the vec's, so the vec
        // declares that it owns its elements.
        let out = if tag.class == lazy_elem_class::STRING {
            unsafe {
                crate::c_abi::vec::gos_rt_vec_new_typed(8, crate::c_abi::vec::vec_elem_kind::STRING)
            }
        } else {
            unsafe { gos_rt_vec_new(8) }
        };
        for x in upstream {
            unsafe { gos_rt_vec_push_i64(out, x) };
        }
        out
    })
}

/// Consume a lazy pair iterator into a `Vec<(i64, i64)>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_collect_pair_i64(
    iter: *mut GosLazyIterPairI64,
) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let out = unsafe { gos_rt_vec_new(16) };
        let (pairs, _classes) = unsafe { take_lazy_pair_i64(iter) };
        for (a, b) in pairs {
            let slot: [i64; 2] = [a, b];
            unsafe { gos_rt_vec_push(out, slot.as_ptr().cast::<u8>()) };
        }
        out
    })
}

/// Count a lazy i64 iterator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_count_i64(iter: *mut GosLazyIterI64) -> i64 {
    ffi_entry!(0, {
        let (upstream, tag) = unsafe { take_lazy_tagged(iter) };
        if !lazy_elem_class::counted(tag.class) {
            return i64::try_from(upstream.count()).unwrap_or(i64::MAX);
        }
        let mut count = 0i64;
        for x in upstream {
            unsafe { release_pulled(tag.class, x) };
            count = count.saturating_add(1);
        }
        count
    })
}

/// Count a lazy pair iterator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_count_pair_i64(iter: *mut GosLazyIterPairI64) -> i64 {
    ffi_entry!(0, {
        let (pairs, [first, second]) = unsafe { take_lazy_pair_i64(iter) };
        let mut count = 0i64;
        for (a, b) in pairs {
            unsafe {
                release_pulled(first, a);
                release_pulled(second, b);
            }
            count = count.saturating_add(1);
        }
        count
    })
}

/// Sum a lazy i64 iterator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_sum_i64(iter: *mut GosLazyIterI64) -> i64 {
    ffi_entry!(0, {
        unsafe { take_lazy_i64(iter) }.fold(0i64, i64::wrapping_add)
    })
}

/// Product of a lazy i64 iterator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_product_i64(iter: *mut GosLazyIterI64) -> i64 {
    ffi_entry!(1, {
        unsafe { take_lazy_i64(iter) }.fold(1i64, i64::wrapping_mul)
    })
}

/// Sum of a lazy i64 iterator, raising the language's integer overflow panic
/// where the sum leaves `i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_sum_i64_checked(iter: *mut GosLazyIterI64) -> i64 {
    ffi_entry!(0, { checked_sum_i64(unsafe { take_lazy_i64(iter) }) })
}

/// Product of a lazy i64 iterator, raising the language's integer overflow
/// panic where the product leaves `i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_product_i64_checked(iter: *mut GosLazyIterI64) -> i64 {
    ffi_entry!(1, { checked_product_i64(unsafe { take_lazy_i64(iter) }) })
}

/// The least (or, with `greatest`, the greatest) element of a word-register
/// stream as `Option<T>` in the packed carrier.
///
/// An integer orders by its word. A `String` orders by its text, and every
/// element that loses is released, leaving the caller the winner's share. Ties
/// keep the first minimum and the last maximum, as `Iterator::min` and
/// `Iterator::max` do.
unsafe fn lazy_word_extreme(iter: *mut GosLazyIterI64, greatest: bool) -> i128 {
    let (upstream, tag) = unsafe { take_lazy_tagged(iter) };
    let best = match tag.class {
        lazy_elem_class::WORD => {
            if greatest {
                upstream.max()
            } else {
                upstream.min()
            }
        }
        lazy_elem_class::STRING => {
            let mut best: Option<i64> = None;
            for x in upstream {
                let Some(current) = best else {
                    best = Some(x);
                    continue;
                };
                let text = |word: i64| -> *const c_char {
                    std::ptr::with_exposed_provenance(word as usize)
                };
                let order =
                    unsafe { crate::c_abi::string::gos_rt_str_compare(text(x), text(current)) };
                let replaces = if greatest { order >= 0 } else { order < 0 };
                if replaces {
                    unsafe { release_pulled(tag.class, current) };
                    best = Some(x);
                } else {
                    unsafe { release_pulled(tag.class, x) };
                }
            }
            best
        }
        other => class_mismatch(other, lazy_elem_class::WORD),
    };
    match best {
        Some(value) => gos_rt_result_new(0, value),
        None => gos_rt_result_new(1, 0),
    }
}

/// Minimum of a lazy i64 or `String` iterator as `Option<T>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_min_i64(iter: *mut GosLazyIterI64) -> i128 {
    ffi_entry!(gos_rt_result_new(1, 0), {
        unsafe { lazy_word_extreme(iter, false) }
    })
}

/// Maximum of a lazy i64 or `String` iterator as `Option<T>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_max_i64(iter: *mut GosLazyIterI64) -> i128 {
    ffi_entry!(gos_rt_result_new(1, 0), {
        unsafe { lazy_word_extreme(iter, true) }
    })
}

/// Fold a lazy i64 iterator with an i64 accumulator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_fold_i64(
    init: i64,
    env: *const u8,
    iter: *mut GosLazyIterI64,
) -> i64 {
    ffi_entry!(init, {
        if env.is_null() {
            return init;
        }
        type FoldFn = unsafe extern "C" fn(env: *const u8, acc: i64, x: i64) -> i64;
        let fn_addr_raw = unsafe { (env as *const usize).read() };
        if fn_addr_raw == 0 {
            return init;
        }
        let f: FoldFn = unsafe { std::mem::transmute(fn_addr_raw) };
        let mut acc = init;
        let (upstream, tag) = unsafe { take_lazy_word(iter) };
        for x in upstream {
            acc = unsafe { f(env, acc, x) };
            unsafe { release_pulled(tag.class, x) };
        }
        acc
    })
}

/// Short-circuiting `any` over a lazy i64 iterator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_any_i64(
    env: *const u8,
    iter: *mut GosLazyIterI64,
) -> i64 {
    ffi_entry!(0, {
        if env.is_null() {
            return 0;
        }
        type PredFn = unsafe extern "C" fn(env: *const u8, x: i64) -> bool;
        let fn_addr_raw = unsafe { (env as *const usize).read() };
        if fn_addr_raw == 0 {
            return 0;
        }
        let p: PredFn = unsafe { std::mem::transmute(fn_addr_raw) };
        let (upstream, tag) = unsafe { take_lazy_word(iter) };
        for x in upstream {
            let hit = unsafe { p(env, x) };
            unsafe { release_pulled(tag.class, x) };
            if hit {
                return 1;
            }
        }
        0
    })
}

/// Short-circuiting `all` over a lazy i64 iterator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_all_i64(
    env: *const u8,
    iter: *mut GosLazyIterI64,
) -> i64 {
    ffi_entry!(1, {
        if env.is_null() {
            return 1;
        }
        type PredFn = unsafe extern "C" fn(env: *const u8, x: i64) -> bool;
        let fn_addr_raw = unsafe { (env as *const usize).read() };
        if fn_addr_raw == 0 {
            return 1;
        }
        let p: PredFn = unsafe { std::mem::transmute(fn_addr_raw) };
        let (upstream, tag) = unsafe { take_lazy_word(iter) };
        for x in upstream {
            let hit = unsafe { p(env, x) };
            unsafe { release_pulled(tag.class, x) };
            if !hit {
                return 0;
            }
        }
        1
    })
}

/// Short-circuiting `find` over a lazy i64 iterator. Returns `Option<i64>`
/// using the runtime Result/Option i128 carrier, with disc 0 for Some and
/// disc 1 for None.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_find_i64(
    env: *const u8,
    iter: *mut GosLazyIterI64,
) -> i128 {
    ffi_entry!(gos_rt_result_new(1, 0), {
        if env.is_null() {
            return gos_rt_result_new(1, 0);
        }
        type PredFn = unsafe extern "C" fn(env: *const u8, x: i64) -> bool;
        let fn_addr_raw = unsafe { (env as *const usize).read() };
        if fn_addr_raw == 0 {
            return gos_rt_result_new(1, 0);
        }
        let p: PredFn = unsafe { std::mem::transmute(fn_addr_raw) };
        let (upstream, tag) = unsafe { take_lazy_word(iter) };
        for x in upstream {
            if unsafe { p(env, x) } {
                return gos_rt_result_new(0, x);
            }
            unsafe { release_pulled(tag.class, x) };
        }
        gos_rt_result_new(1, 0)
    })
}

/// Lazy repeat of an f64 value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_repeat_f64(value: f64, n: i64) -> *mut GosLazyIterI64 {
    ffi_entry!(std::ptr::null_mut(), {
        if n < 0 {
            crate::c_abi::panic::panic_text("iter::repeat: count must be non-negative");
        }
        let n = usize::try_from(n).unwrap_or(0);
        lazy_f64(std::iter::repeat_n(value, n))
    })
}

/// Lazy single-item f64 iterator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_once_f64(value: f64) -> *mut GosLazyIterI64 {
    ffi_entry!(std::ptr::null_mut(), { lazy_f64(std::iter::once(value)) })
}

/// Advance a float-carrying lazy iterator without consuming the handle.
///
/// Returns `Option<f64>` in the packed i128 carrier; the payload word holds the
/// element's float bits, which is the representation an `Option<f64>` slot uses.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_next_f64(iter: *mut GosLazyIterI64) -> i128 {
    ffi_entry!(gos_rt_result_new(1, 0), {
        if iter.is_null() {
            return gos_rt_result_new(1, 0);
        }
        // SAFETY: the handle remains owned by the caller; this only advances
        // the state in place.
        let state = unsafe { &mut *iter };
        debug_assert_eq!(
            state.tag.class,
            lazy_elem_class::FLOAT,
            "lazy iterator element class mismatch on next()"
        );
        match state.inner.next() {
            Some(bits) => gos_rt_result_new(0, bits),
            None => gos_rt_result_new(1, 0),
        }
    })
}

/// Lazy `map(f)` for `f64 -> f64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_map_f64(
    env: *const u8,
    iter: *mut GosLazyIterI64,
) -> *mut GosLazyIterI64 {
    ffi_entry!(std::ptr::null_mut(), {
        let upstream = unsafe { take_lazy_f64(iter) };
        let Some(f) = (unsafe { lazy_callback::<CallF64F64>(env) }) else {
            return lazy_f64(std::iter::empty());
        };
        let share = unsafe { EnvShare::new(env) };
        lazy_f64(upstream.map(move |x| unsafe { f(share.ptr(), x) }))
    })
}

/// Lazy `map(f)` from `f64` to a word-register result of `out_class`.
unsafe fn lazy_map_f64_word(
    env: *const u8,
    iter: *mut GosLazyIterI64,
    out_class: u8,
) -> *mut GosLazyIterI64 {
    let upstream = unsafe { take_lazy_f64(iter) };
    let Some(f) = (unsafe { lazy_callback::<CallF64Word>(env) }) else {
        return lazy_classed(out_class, std::iter::empty());
    };
    let share = unsafe { EnvShare::new(env) };
    lazy_classed(
        out_class,
        upstream.map(move |x| unsafe { f(share.ptr(), x) }),
    )
}

/// Lazy `map(f)` for `f64 -> i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_map_f64_word(
    env: *const u8,
    iter: *mut GosLazyIterI64,
) -> *mut GosLazyIterI64 {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { lazy_map_f64_word(env, iter, lazy_elem_class::WORD) }
    })
}

/// Lazy `map(f)` for `f64 -> String`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_map_f64_str(
    env: *const u8,
    iter: *mut GosLazyIterI64,
) -> *mut GosLazyIterI64 {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { lazy_map_f64_word(env, iter, lazy_elem_class::STRING) }
    })
}

/// Lazy `map(f)` for `i64 -> f64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_map_word_f64(
    env: *const u8,
    iter: *mut GosLazyIterI64,
) -> *mut GosLazyIterI64 {
    ffi_entry!(std::ptr::null_mut(), {
        // The word reaches the callback in an integer register whether it is
        // an element's value or an aggregate element's address, so both
        // classes are operands here - only a float slot would be read in the
        // wrong register file.
        let (upstream, tag) = unsafe { take_lazy_word(iter) };
        let Some(f) = (unsafe { lazy_callback::<CallWordF64>(env) }) else {
            return lazy_f64(std::iter::empty());
        };
        let share = unsafe { EnvShare::new(env) };
        let in_class = tag.class;
        lazy_f64(upstream.map(move |x| unsafe {
            let y = f(share.ptr(), x);
            release_pulled(in_class, x);
            y
        }))
    })
}

/// Lazy `filter(p)` over f64 elements.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_filter_f64(
    env: *const u8,
    iter: *mut GosLazyIterI64,
) -> *mut GosLazyIterI64 {
    ffi_entry!(std::ptr::null_mut(), {
        let upstream = unsafe { take_lazy_f64(iter) };
        let Some(p) = (unsafe { lazy_callback::<PredF64>(env) }) else {
            return lazy_f64(std::iter::empty());
        };
        let share = unsafe { EnvShare::new(env) };
        lazy_f64(upstream.filter(move |x| unsafe { p(share.ptr(), *x) }))
    })
}

/// Sum a lazy f64 iterator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_sum_f64(iter: *mut GosLazyIterI64) -> f64 {
    // Starts from +0.0 as `gos_rt_iter_sum_f64` does.
    ffi_entry!(0.0, {
        unsafe { take_lazy_f64(iter) }.fold(0.0, |total, value| total + value)
    })
}

/// Product of a lazy f64 iterator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_product_f64(iter: *mut GosLazyIterI64) -> f64 {
    ffi_entry!(1.0, { unsafe { take_lazy_f64(iter) }.product() })
}

/// Minimum of a lazy f64 iterator as `Option<f64>`, with the payload word
/// holding the winner's float bits.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_min_f64(iter: *mut GosLazyIterI64) -> i128 {
    ffi_entry!(gos_rt_result_new(1, 0), {
        match unsafe { take_lazy_f64(iter) }.reduce(f64::min) {
            Some(value) => gos_rt_result_new(0, value.to_bits() as i64),
            None => gos_rt_result_new(1, 0),
        }
    })
}

/// Maximum of a lazy f64 iterator as `Option<f64>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_max_f64(iter: *mut GosLazyIterI64) -> i128 {
    ffi_entry!(gos_rt_result_new(1, 0), {
        match unsafe { take_lazy_f64(iter) }.reduce(f64::max) {
            Some(value) => gos_rt_result_new(0, value.to_bits() as i64),
            None => gos_rt_result_new(1, 0),
        }
    })
}

/// Fold a lazy f64 iterator with an f64 accumulator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_fold_f64(
    init: f64,
    env: *const u8,
    iter: *mut GosLazyIterI64,
) -> f64 {
    ffi_entry!(init, {
        let upstream = unsafe { take_lazy_f64(iter) };
        let Some(f) = (unsafe { lazy_callback::<FoldF64F64>(env) }) else {
            return init;
        };
        let mut acc = init;
        for x in upstream {
            acc = unsafe { f(env, acc, x) };
        }
        acc
    })
}

/// Fold a lazy i64 iterator with an f64 accumulator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_fold_f64_word(
    init: f64,
    env: *const u8,
    iter: *mut GosLazyIterI64,
) -> f64 {
    ffi_entry!(init, {
        // See `gos_rt_lazy_iter_map_word_f64`: the element reaches the
        // callback as a word either way.
        let (upstream, tag) = unsafe { take_lazy_word(iter) };
        let Some(f) = (unsafe { lazy_callback::<FoldF64Word>(env) }) else {
            return init;
        };
        let mut acc = init;
        for x in upstream {
            acc = unsafe { f(env, acc, x) };
            unsafe { release_pulled(tag.class, x) };
        }
        acc
    })
}

/// Fold a lazy f64 iterator with an i64 accumulator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_fold_word_f64(
    init: i64,
    env: *const u8,
    iter: *mut GosLazyIterI64,
) -> i64 {
    ffi_entry!(init, {
        let upstream = unsafe { take_lazy_f64(iter) };
        let Some(f) = (unsafe { lazy_callback::<FoldWordF64>(env) }) else {
            return init;
        };
        let mut acc = init;
        for x in upstream {
            acc = unsafe { f(env, acc, x) };
        }
        acc
    })
}

/// Short-circuiting `any` over a lazy f64 iterator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_any_f64(
    env: *const u8,
    iter: *mut GosLazyIterI64,
) -> i64 {
    ffi_entry!(0, {
        let upstream = unsafe { take_lazy_f64(iter) };
        let Some(p) = (unsafe { lazy_callback::<PredF64>(env) }) else {
            return 0;
        };
        i64::from(upstream.into_iter().any(|x| unsafe { p(env, x) }))
    })
}

/// Short-circuiting `all` over a lazy f64 iterator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_all_f64(
    env: *const u8,
    iter: *mut GosLazyIterI64,
) -> i64 {
    ffi_entry!(1, {
        let upstream = unsafe { take_lazy_f64(iter) };
        let Some(p) = (unsafe { lazy_callback::<PredF64>(env) }) else {
            return 1;
        };
        i64::from(upstream.into_iter().all(|x| unsafe { p(env, x) }))
    })
}

/// Short-circuiting `find` over a lazy f64 iterator, as `Option<f64>` in the
/// i128 carrier with the payload word holding the match's float bits.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lazy_iter_find_f64(
    env: *const u8,
    iter: *mut GosLazyIterI64,
) -> i128 {
    ffi_entry!(gos_rt_result_new(1, 0), {
        let upstream = unsafe { take_lazy_f64(iter) };
        let Some(p) = (unsafe { lazy_callback::<PredF64>(env) }) else {
            return gos_rt_result_new(1, 0);
        };
        for x in upstream {
            if unsafe { p(env, x) } {
                return gos_rt_result_new(0, x.to_bits() as i64);
            }
        }
        gos_rt_result_new(1, 0)
    })
}

/// Build `Vec<i64>` from the first `n` elements of `v`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_take_i64(n: i64, v: *const GosVec) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let out = unsafe { vec_like_source(v, 0) };
        if n < 0 {
            crate::c_abi::panic::panic_text("iter::take: count must be non-negative");
        }
        if v.is_null() {
            return out;
        }
        let vec = unsafe { &*v };
        let take_n = n.min(vec.len);
        for i in 0..take_n {
            unsafe { crate::c_abi::vec::vec_push_elem_from(out, v, i) };
        }
        out
    })
}

/// Build `Vec<i64>` dropping the first `n` elements of `v`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_skip_i64(n: i64, v: *const GosVec) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let out = unsafe { vec_like_source(v, 0) };
        if n < 0 {
            crate::c_abi::panic::panic_text("iter::skip: count must be non-negative");
        }
        if v.is_null() {
            return out;
        }
        let vec = unsafe { &*v };
        let start = n.min(vec.len);
        for i in start..vec.len {
            unsafe { crate::c_abi::vec::vec_push_elem_from(out, v, i) };
        }
        out
    })
}

/// Reverse a `Vec<i64>` into a fresh vec.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_reversed_i64(v: *const GosVec) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let out = unsafe { vec_like_source(v, 0) };
        if v.is_null() {
            return out;
        }
        let vec = unsafe { &*v };
        for i in (0..vec.len).rev() {
            unsafe { crate::c_abi::vec::vec_push_elem_from(out, v, i) };
        }
        out
    })
}

/// Concatenate two `Vec<i64>`s.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_chain_i64(a: *const GosVec, b: *const GosVec) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let out = unsafe { vec_like_source(a, 0) };
        for v in [a, b] {
            if v.is_null() {
                continue;
            }
            let vec = unsafe { &*v };
            for i in 0..vec.len {
                unsafe { crate::c_abi::vec::vec_push_elem_from(out, v, i) };
            }
        }
        out
    })
}

/// `iter::dedup(xs)` - drop consecutive duplicate elements.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_dedup_i64(
    v: *const GosVec,
    desc: *const u8,
    has_desc: i64,
) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let out = unsafe { vec_like_source(v, 0) };
        if v.is_null() {
            return out;
        }
        let vec = unsafe { &*v };
        // Consecutive equality is over the whole element. An element whose
        // slot word is not its value - a String, an enum reached through its
        // node - is compared through the ordering descriptor, which is what
        // makes two equal values built at different addresses one element.
        // The descriptor is a tag stream, not text: its bytes include zero,
        // so the caller says whether one is present rather than the stream
        // being read to a terminator.
        let tags = (has_desc != 0 && !desc.is_null()).then_some(desc);
        let mut prev: Option<i64> = None;
        for i in 0..vec.len {
            let dup = prev.is_some_and(|j| unsafe { elems_equal(v, i, j, tags) });
            if !dup {
                unsafe { crate::c_abi::vec::vec_push_elem_from(out, v, i) };
                prev = Some(i);
            }
        }
        out
    })
}

/// Whether `v[i]` and `v[j]` are the same value: through the ordering
/// descriptor when one is given, and byte-wise at the element's stride
/// otherwise.
unsafe fn elems_equal(v: *const GosVec, i: i64, j: i64, tags: Option<*const u8>) -> bool {
    let Some(tags) = tags else {
        return unsafe { crate::c_abi::vec::vec_elem_bytes_eq(v, i, v, j) };
    };
    let (a, b) = unsafe {
        (
            crate::c_abi::gos_rt_vec_get_ptr(v, i),
            crate::c_abi::gos_rt_vec_get_ptr(v, j),
        )
    };
    if a.is_null() || b.is_null() {
        return false;
    }
    unsafe { crate::c_abi::desc_cmp::gos_rt_desc_cmp(a, b, tags) == 0 }
}

/// `iter::flatten(xss)` - concatenate a `Vec<Vec<i64>>` into one
/// `Vec<i64>`. Each outer element is an 8-byte `*mut GosVec`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_flatten_i64(v: *const GosVec) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        if v.is_null() {
            return unsafe { gos_rt_vec_new(8) };
        }
        let outer = unsafe { &*v };
        // The flattened sequence carries the inner sequence's element, so the
        // result takes its width and kind from the first inner vec present.
        let mut out: *mut GosVec = std::ptr::null_mut();
        for i in 0..outer.len {
            let inner = unsafe { gos_rt_vec_get_i64(v, i) } as usize as *const GosVec;
            if inner.is_null() {
                continue;
            }
            if out.is_null() {
                out = unsafe { vec_like_source(inner, 0) };
            }
            let inner_ref = unsafe { &*inner };
            for j in 0..inner_ref.len {
                unsafe { crate::c_abi::vec::vec_push_elem_from(out, inner, j) };
            }
        }
        if out.is_null() {
            out = unsafe { gos_rt_vec_new(8) };
        }
        out
    })
}

/// `iter::enumerate(xs)` - `Vec<(i64, i64)>` of `(index, value)`.
/// Each element is a 16-byte 2-slot tuple read by the multislot
/// for-loop path (`gos_rt_vec_get_ptr` + `gos_load` at 0 / 8).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_enumerate_i64(v: *const GosVec) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let out = unsafe { gos_rt_vec_new(16) };
        if v.is_null() {
            return out;
        }
        let vec = unsafe { &*v };
        for i in 0..vec.len {
            let x = unsafe { gos_rt_vec_get_i64(v, i) };
            let slot: [i64; 2] = [i, x];
            unsafe { gos_rt_vec_push(out, slot.as_ptr().cast::<u8>()) };
        }
        out
    })
}

/// `iter::zip(a, b)` - `Vec<(i64, i64)>`, stopping at the shorter
/// input. 16-byte 2-slot tuple elements.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_zip_i64(a: *const GosVec, b: *const GosVec) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let out = unsafe { gos_rt_vec_new(16) };
        if a.is_null() || b.is_null() {
            return out;
        }
        let av = unsafe { &*a };
        let bv = unsafe { &*b };
        let n = av.len.min(bv.len);
        for i in 0..n {
            let x = unsafe { gos_rt_vec_get_i64(a, i) };
            let y = unsafe { gos_rt_vec_get_i64(b, i) };
            let slot: [i64; 2] = [x, y];
            unsafe { gos_rt_vec_push(out, slot.as_ptr().cast::<u8>()) };
        }
        out
    })
}

/// `iter::windows(n, xs)` - `Vec<Vec<i64>>` of every contiguous
/// width-`n` window. Empty when `xs` is shorter than `n`. Outer is a
/// VEC-typed vec of inner `*mut GosVec` pointers
/// (recursively freed).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_windowed_i64(n: i64, v: *const GosVec) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let out = unsafe {
            crate::c_abi::vec::gos_rt_vec_new_typed(8, crate::c_abi::vec::vec_elem_kind::VEC)
        };
        if n <= 0 {
            crate::c_abi::panic::panic_text("iter::windows: count must be positive");
        }
        if v.is_null() {
            return out;
        }
        let vec = unsafe { &*v };
        if vec.len < n {
            return out;
        }
        for start in 0..=(vec.len - n) {
            let inner = unsafe { vec_like_source(v, n) };
            for j in 0..n {
                unsafe { crate::c_abi::vec::vec_push_elem_from(inner, v, start + j) };
            }
            let inner_val = inner as i64;
            unsafe { gos_rt_vec_push(out, std::ptr::addr_of!(inner_val).cast::<u8>()) };
        }
        out
    })
}

/// `iter::chunks(n, xs)` - `Vec<Vec<i64>>` of consecutive
/// width-`n` chunks; the final chunk may be short.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_chunk_by_size_i64(n: i64, v: *const GosVec) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let out = unsafe {
            crate::c_abi::vec::gos_rt_vec_new_typed(8, crate::c_abi::vec::vec_elem_kind::VEC)
        };
        if n <= 0 {
            crate::c_abi::panic::panic_text("iter::chunks: count must be positive");
        }
        if v.is_null() {
            return out;
        }
        let vec = unsafe { &*v };
        let mut start = 0;
        while start < vec.len {
            let inner = unsafe { vec_like_source(v, n) };
            let end = (start + n).min(vec.len);
            for j in start..end {
                unsafe { crate::c_abi::vec::vec_push_elem_from(inner, v, j) };
            }
            let inner_val = inner as i64;
            unsafe { gos_rt_vec_push(out, std::ptr::addr_of!(inner_val).cast::<u8>()) };
            start += n;
        }
        out
    })
}

// -- Closure-taking iter helpers. Closure ABI: env pointer with
// fn_addr at env[0]. Each helper transmutes env[0] to a specific
// `(env, args...) -> ret` signature determined by the combinator's
// callback contract.

/// `iter::for_each(f, xs)` - call `f(x)` once per element.
/// Closure body sig: `(env: *const u8, x: i64) -> i64` (return value
/// ignored; using i64 keeps the callback ABI uniform with sort_by).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_for_each_i64(env: *const u8, v: *const GosVec) {
    ffi_entry!((), {
        if env.is_null() || v.is_null() {
            return;
        }
        let vec = unsafe { &*v };
        if vec.len <= 0 {
            return;
        }
        type CallFn = unsafe extern "C" fn(env: *const u8, x: i64) -> i64;
        let fn_addr_raw = unsafe { (env as *const usize).read() };
        if fn_addr_raw == 0 {
            return;
        }
        let f: CallFn = unsafe { std::mem::transmute(fn_addr_raw) };
        for i in 0..vec.len {
            let x = unsafe { gos_rt_vec_get_i64(v, i) };
            unsafe { f(env, x) };
        }
    });
}

/// `iter::for_each(f, xs)` for `Vec<String>` / `Vec<*ptr>` shape.
/// Closure body sig: `(env: *const u8, x: *const u8) -> i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_for_each_ptr(env: *const u8, v: *const GosVec) {
    ffi_entry!((), {
        if env.is_null() || v.is_null() {
            return;
        }
        let vec = unsafe { &*v };
        if vec.len <= 0 {
            return;
        }
        type CallFn = unsafe extern "C" fn(env: *const u8, x: *const u8) -> i64;
        let fn_addr_raw = unsafe { (env as *const usize).read() };
        if fn_addr_raw == 0 {
            return;
        }
        let f: CallFn = unsafe { std::mem::transmute(fn_addr_raw) };
        for i in 0..vec.len {
            let p = unsafe { gos_rt_vec_get_ptr(v, i) };
            unsafe { f(env, p) };
        }
    });
}

/// `iter::map(f, xs)` for `Vec<i64> -> Vec<i64>`.
///
/// Closure body sig: `(env, i64) -> i64`. `out_bytes` is the declared width of
/// the mapped element: a map changes the element type, so the output's stride
/// comes from the call site rather than from the source header.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_map_i64(
    env: *const u8,
    v: *const GosVec,
    out_bytes: i64,
    out_is_block: i64,
) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let out = unsafe { gos_rt_vec_new(mapped_stride(out_bytes)) };
        if env.is_null() || v.is_null() {
            return out;
        }
        let vec = unsafe { &*v };
        type CallFn = unsafe extern "C" fn(env: *const u8, x: i64) -> i64;
        let fn_addr_raw = unsafe { (env as *const usize).read() };
        if fn_addr_raw == 0 {
            return out;
        }
        let f: CallFn = unsafe { std::mem::transmute(fn_addr_raw) };
        for i in 0..vec.len {
            let x = unsafe { gos_rt_vec_get_i64(v, i) };
            let y = unsafe { f(env, x) };
            unsafe { push_mapped(out, y, out_bytes, out_is_block != 0) };
        }
        out
    })
}

/// `iter::map(f, xs)` for aggregate elements whose callback receives an
/// address to the element's complete flat-slot storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_map_ptr_i64(
    env: *const u8,
    v: *const GosVec,
    out_bytes: i64,
    out_is_block: i64,
) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let out = unsafe { gos_rt_vec_new(mapped_stride(out_bytes)) };
        if env.is_null() || v.is_null() {
            return out;
        }
        type CallFn = unsafe extern "C" fn(env: *const u8, x: *const u8) -> i64;
        let fn_addr_raw = unsafe { (env as *const usize).read() };
        if fn_addr_raw == 0 {
            return out;
        }
        let f: CallFn = unsafe { std::mem::transmute(fn_addr_raw) };
        let len = unsafe { gos_rt_vec_len(v) };
        for i in 0..len {
            let x = unsafe { gos_rt_vec_get_ptr(v, i) };
            let y = unsafe { f(env, x) };
            unsafe { push_mapped(out, y, out_bytes, out_is_block != 0) };
        }
        out
    })
}

/// `iter::filter(p, xs)` for `Vec<i64>`. Predicate returns i64
/// (truthy = nonzero) to keep the callback ABI uniform.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_filter_i64(env: *const u8, v: *const GosVec) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let out = unsafe { vec_like_source(v, 0) };
        if env.is_null() || v.is_null() {
            return out;
        }
        let vec = unsafe { &*v };
        type PredFn = unsafe extern "C" fn(env: *const u8, x: i64) -> bool;
        let fn_addr_raw = unsafe { (env as *const usize).read() };
        if fn_addr_raw == 0 {
            return out;
        }
        let p: PredFn = unsafe { std::mem::transmute(fn_addr_raw) };
        for i in 0..vec.len {
            let x = unsafe { gos_rt_vec_get_i64(v, i) };
            if unsafe { p(env, x) } {
                unsafe { gos_rt_vec_push_i64(out, x) };
            }
        }
        // The kept slots are raw copies of the source's; when the source
        // owns pointer-bearing elements the result must hold its own
        // shares (and carry the same element kind) or the source's free
        // would dangle every survivor.
        unsafe { crate::c_abi::vec::vec_share_owned_elements(v, out) };
        out
    })
}

/// `iter::for_each(f, xs)` for `Vec<f64>`. Element bits are read as an
/// 8-byte word and reinterpreted as `f64` so the closure receives the
/// value in the float ABI (an `f64` param rides an SSE register, not the
/// integer register `f(env, x: i64)` would fill).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_for_each_f64(env: *const u8, v: *const GosVec) {
    ffi_entry!((), {
        if env.is_null() || v.is_null() {
            return;
        }
        let vec = unsafe { &*v };
        if vec.len <= 0 {
            return;
        }
        type CallFn = unsafe extern "C" fn(env: *const u8, x: f64) -> i64;
        let fn_addr_raw = unsafe { (env as *const usize).read() };
        if fn_addr_raw == 0 {
            return;
        }
        let f: CallFn = unsafe { std::mem::transmute(fn_addr_raw) };
        for i in 0..vec.len {
            let x = f64::from_bits(unsafe { gos_rt_vec_get_i64(v, i) } as u64);
            unsafe { f(env, x) };
        }
    });
}

/// `iter::map(f, xs)` for `Vec<f64> -> Vec<f64>`. Reads each element's
/// bits as `f64`, calls the float-ABI closure, and stores the result
/// bits back into the new Vec. The input and output register class each
/// pick their own shim (an `f64` rides an SSE register, an `i64` /
/// pointer an integer one) - a mismatched pairing would read the result
/// out of the wrong register.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_map_f64(env: *const u8, v: *const GosVec) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let out = unsafe { gos_rt_vec_new(8) };
        if env.is_null() || v.is_null() {
            return out;
        }
        let vec = unsafe { &*v };
        type CallFn = unsafe extern "C" fn(env: *const u8, x: f64) -> f64;
        let fn_addr_raw = unsafe { (env as *const usize).read() };
        if fn_addr_raw == 0 {
            return out;
        }
        let f: CallFn = unsafe { std::mem::transmute(fn_addr_raw) };
        for i in 0..vec.len {
            let x = f64::from_bits(unsafe { gos_rt_vec_get_i64(v, i) } as u64);
            let y = unsafe { f(env, x) };
            unsafe { gos_rt_vec_push_i64(out, y.to_bits() as i64) };
        }
        out
    })
}

/// `iter::map(f, xs)` for `Vec<f64> -> Vec<i64 / ptr>` - an `f64`
/// element mapped to an integer-register result (`|x| x as i64`,
/// `|x| format!("{}", x)`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_map_f64_word(
    env: *const u8,
    v: *const GosVec,
    out_bytes: i64,
    out_is_block: i64,
) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let out = unsafe { gos_rt_vec_new(mapped_stride(out_bytes)) };
        if env.is_null() || v.is_null() {
            return out;
        }
        let vec = unsafe { &*v };
        type CallFn = unsafe extern "C" fn(env: *const u8, x: f64) -> i64;
        let fn_addr_raw = unsafe { (env as *const usize).read() };
        if fn_addr_raw == 0 {
            return out;
        }
        let f: CallFn = unsafe { std::mem::transmute(fn_addr_raw) };
        for i in 0..vec.len {
            let x = f64::from_bits(unsafe { gos_rt_vec_get_i64(v, i) } as u64);
            let y = unsafe { f(env, x) };
            unsafe { push_mapped(out, y, out_bytes, out_is_block != 0) };
        }
        out
    })
}

/// `iter::map(f, xs)` for `Vec<i64 / ptr> -> Vec<f64>` - an
/// integer-register element mapped to an `f64` result (`|i| i as f64`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_map_word_f64(env: *const u8, v: *const GosVec) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let out = unsafe { gos_rt_vec_new(8) };
        if env.is_null() || v.is_null() {
            return out;
        }
        let vec = unsafe { &*v };
        type CallFn = unsafe extern "C" fn(env: *const u8, x: i64) -> f64;
        let fn_addr_raw = unsafe { (env as *const usize).read() };
        if fn_addr_raw == 0 {
            return out;
        }
        let f: CallFn = unsafe { std::mem::transmute(fn_addr_raw) };
        for i in 0..vec.len {
            let x = unsafe { gos_rt_vec_get_i64(v, i) };
            let y = unsafe { f(env, x) };
            unsafe { gos_rt_vec_push_i64(out, y.to_bits() as i64) };
        }
        out
    })
}

/// `iter::filter(p, xs)` for `Vec<f64>`. The kept elements are the
/// original bit patterns; only the predicate sees the reinterpreted
/// `f64` value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_filter_f64(env: *const u8, v: *const GosVec) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let out = unsafe { vec_like_source(v, 0) };
        if env.is_null() || v.is_null() {
            return out;
        }
        let vec = unsafe { &*v };
        type PredFn = unsafe extern "C" fn(env: *const u8, x: f64) -> bool;
        let fn_addr_raw = unsafe { (env as *const usize).read() };
        if fn_addr_raw == 0 {
            return out;
        }
        let p: PredFn = unsafe { std::mem::transmute(fn_addr_raw) };
        for i in 0..vec.len {
            let bits = unsafe { gos_rt_vec_get_i64(v, i) };
            if unsafe { p(env, f64::from_bits(bits as u64)) } {
                unsafe { gos_rt_vec_push_i64(out, bits) };
            }
        }
        out
    })
}

/// `iter::any(p, xs)` over a `Vec<f64>`: the slot bits are reinterpreted as
/// `f64` so the predicate sees the float value rather than its bit pattern.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_any_f64(env: *const u8, v: *const GosVec) -> i64 {
    ffi_entry!(-1, {
        if env.is_null() || v.is_null() {
            return 0;
        }
        let vec = unsafe { &*v };
        type PredFn = unsafe extern "C" fn(env: *const u8, x: f64) -> bool;
        let fn_addr_raw = unsafe { (env as *const usize).read() };
        if fn_addr_raw == 0 {
            return 0;
        }
        let p: PredFn = unsafe { std::mem::transmute(fn_addr_raw) };
        for i in 0..vec.len {
            let bits = unsafe { gos_rt_vec_get_i64(v, i) };
            if unsafe { p(env, f64::from_bits(bits as u64)) } {
                return 1;
            }
        }
        0
    })
}

/// `iter::filter(p, xs)` for multi-slot aggregate elements. The predicate
/// receives each element's storage address, and a kept element is copied out
/// whole, so the result keeps the source's element width and ownership shape.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_filter_ptr(env: *const u8, v: *const GosVec) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        if v.is_null() {
            return unsafe { gos_rt_vec_new(8) };
        }
        let vec = unsafe { &*v };
        let out = unsafe {
            crate::c_abi::vec::gos_rt_vec_with_capacity_typed(
                vec.elem_bytes,
                vec.len.max(0),
                vec.elem_kind,
            )
        };
        let Some(p) = (unsafe { lazy_callback::<PredPtr>(env) }) else {
            return out;
        };
        for i in 0..vec.len {
            let slot = unsafe { gos_rt_vec_get_ptr(v, i) };
            if unsafe { p(env, slot) } {
                unsafe { gos_rt_vec_push(out, slot) };
            }
        }
        // Kept slots are raw copies of the source's, so pointer-bearing
        // elements need their own shares before either vec is freed.
        unsafe { crate::c_abi::vec::vec_share_owned_elements(v, out) };
        out
    })
}

/// `iter::map(f, xs)` for aggregate elements producing `f64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_map_ptr_f64(env: *const u8, v: *const GosVec) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let out = unsafe { gos_rt_vec_new(8) };
        if v.is_null() {
            return out;
        }
        let Some(f) = (unsafe { lazy_callback::<CallPtrF64>(env) }) else {
            return out;
        };
        let len = unsafe { gos_rt_vec_len(v) };
        for i in 0..len {
            let slot = unsafe { gos_rt_vec_get_ptr(v, i) };
            let y = unsafe { f(env, slot) };
            unsafe { gos_rt_vec_push_i64(out, y.to_bits() as i64) };
        }
        out
    })
}

/// `iter::all(p, xs)` for `Vec<f64>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_all_f64(env: *const u8, v: *const GosVec) -> i64 {
    ffi_entry!(1, {
        if v.is_null() {
            return 1;
        }
        let Some(p) = (unsafe { lazy_callback::<PredF64>(env) }) else {
            return 1;
        };
        let vec = unsafe { &*v };
        for i in 0..vec.len {
            let bits = unsafe { gos_rt_vec_get_i64(v, i) };
            if !unsafe { p(env, f64::from_bits(bits as u64)) } {
                return 0;
            }
        }
        1
    })
}

/// `iter::fold(init, f, xs)` for `Vec<f64>` with an f64 accumulator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_fold_f64(init: f64, env: *const u8, v: *const GosVec) -> f64 {
    ffi_entry!(init, {
        if v.is_null() {
            return init;
        }
        let Some(f) = (unsafe { lazy_callback::<FoldF64F64>(env) }) else {
            return init;
        };
        let vec = unsafe { &*v };
        let mut acc = init;
        for i in 0..vec.len {
            let bits = unsafe { gos_rt_vec_get_i64(v, i) };
            acc = unsafe { f(env, acc, f64::from_bits(bits as u64)) };
        }
        acc
    })
}

/// `iter::fold(init, f, xs)` for `Vec<i64>` with an f64 accumulator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_fold_f64_word(
    init: f64,
    env: *const u8,
    v: *const GosVec,
) -> f64 {
    ffi_entry!(init, {
        if v.is_null() {
            return init;
        }
        let Some(f) = (unsafe { lazy_callback::<FoldF64Word>(env) }) else {
            return init;
        };
        let vec = unsafe { &*v };
        let mut acc = init;
        for i in 0..vec.len {
            let x = unsafe { gos_rt_vec_get_i64(v, i) };
            acc = unsafe { f(env, acc, x) };
        }
        acc
    })
}

/// `iter::fold(init, f, xs)` for `Vec<f64>` with an i64 accumulator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_fold_word_f64(
    init: i64,
    env: *const u8,
    v: *const GosVec,
) -> i64 {
    ffi_entry!(init, {
        if v.is_null() {
            return init;
        }
        let Some(f) = (unsafe { lazy_callback::<FoldWordF64>(env) }) else {
            return init;
        };
        let vec = unsafe { &*v };
        let mut acc = init;
        for i in 0..vec.len {
            let bits = unsafe { gos_rt_vec_get_i64(v, i) };
            acc = unsafe { f(env, acc, f64::from_bits(bits as u64)) };
        }
        acc
    })
}

/// `iter::fold(init, f, xs)` for aggregate elements with an i64 accumulator;
/// the callback receives each element's storage address.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_fold_ptr(init: i64, env: *const u8, v: *const GosVec) -> i64 {
    ffi_entry!(init, {
        if v.is_null() {
            return init;
        }
        let Some(f) = (unsafe { lazy_callback::<FoldWordPtr>(env) }) else {
            return init;
        };
        let len = unsafe { gos_rt_vec_len(v) };
        let mut acc = init;
        for i in 0..len {
            let slot = unsafe { gos_rt_vec_get_ptr(v, i) };
            acc = unsafe { f(env, acc, slot) };
        }
        acc
    })
}

/// `iter::fold(init, f, xs)` for aggregate elements with an f64 accumulator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_fold_f64_ptr(
    init: f64,
    env: *const u8,
    v: *const GosVec,
) -> f64 {
    ffi_entry!(init, {
        if v.is_null() {
            return init;
        }
        let Some(f) = (unsafe { lazy_callback::<FoldF64Ptr>(env) }) else {
            return init;
        };
        let len = unsafe { gos_rt_vec_len(v) };
        let mut acc = init;
        for i in 0..len {
            let slot = unsafe { gos_rt_vec_get_ptr(v, i) };
            acc = unsafe { f(env, acc, slot) };
        }
        acc
    })
}

/// `iter::sum_by(f, xs)` for `Vec<f64>` summing `f64` projections.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_sum_by_f64(env: *const u8, v: *const GosVec) -> f64 {
    ffi_entry!(0.0, {
        if v.is_null() {
            return 0.0;
        }
        let Some(f) = (unsafe { lazy_callback::<CallF64F64>(env) }) else {
            return 0.0;
        };
        let vec = unsafe { &*v };
        let mut total = 0.0f64;
        for i in 0..vec.len {
            let bits = unsafe { gos_rt_vec_get_i64(v, i) };
            total += unsafe { f(env, f64::from_bits(bits as u64)) };
        }
        total
    })
}

/// `iter::sum_by(f, xs)` for `Vec<i64>` summing `f64` projections.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_sum_by_word_f64(env: *const u8, v: *const GosVec) -> f64 {
    ffi_entry!(0.0, {
        if v.is_null() {
            return 0.0;
        }
        let Some(f) = (unsafe { lazy_callback::<CallWordF64>(env) }) else {
            return 0.0;
        };
        let vec = unsafe { &*v };
        let mut total = 0.0f64;
        for i in 0..vec.len {
            let x = unsafe { gos_rt_vec_get_i64(v, i) };
            total += unsafe { f(env, x) };
        }
        total
    })
}

/// `iter::sum_by(f, xs)` for `Vec<f64>` summing `i64` projections.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_sum_by_f64_word(env: *const u8, v: *const GosVec) -> i64 {
    ffi_entry!(0, {
        if v.is_null() {
            return 0;
        }
        let Some(f) = (unsafe { lazy_callback::<CallF64Word>(env) }) else {
            return 0;
        };
        let vec = unsafe { &*v };
        let mut total = 0i64;
        for i in 0..vec.len {
            let bits = unsafe { gos_rt_vec_get_i64(v, i) };
            total = total.wrapping_add(unsafe { f(env, f64::from_bits(bits as u64)) });
        }
        total
    })
}

/// `iter::sum_by(f, xs)` for aggregate elements summing `i64` projections;
/// the callback receives each element's storage address.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_sum_by_ptr(env: *const u8, v: *const GosVec) -> i64 {
    ffi_entry!(0, {
        if v.is_null() {
            return 0;
        }
        let Some(f) = (unsafe { lazy_callback::<CallPtrWord>(env) }) else {
            return 0;
        };
        let len = unsafe { gos_rt_vec_len(v) };
        let mut total = 0i64;
        for i in 0..len {
            let slot = unsafe { gos_rt_vec_get_ptr(v, i) };
            total = total.wrapping_add(unsafe { f(env, slot) });
        }
        total
    })
}

/// `iter::sum_by(f, xs)` for aggregate elements summing `f64` projections.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_sum_by_ptr_f64(env: *const u8, v: *const GosVec) -> f64 {
    ffi_entry!(0.0, {
        if v.is_null() {
            return 0.0;
        }
        let Some(f) = (unsafe { lazy_callback::<CallPtrF64>(env) }) else {
            return 0.0;
        };
        let len = unsafe { gos_rt_vec_len(v) };
        let mut total = 0.0f64;
        for i in 0..len {
            let slot = unsafe { gos_rt_vec_get_ptr(v, i) };
            total += unsafe { f(env, slot) };
        }
        total
    })
}

/// `iter::fold(init, f, xs)` for `Vec<i64>` with i64 accumulator.
/// Closure body sig: `(env, acc, x) -> acc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_fold_i64(init: i64, env: *const u8, v: *const GosVec) -> i64 {
    ffi_entry!(-1, {
        if env.is_null() || v.is_null() {
            return init;
        }
        let vec = unsafe { &*v };
        type FoldFn = unsafe extern "C" fn(env: *const u8, acc: i64, x: i64) -> i64;
        let fn_addr_raw = unsafe { (env as *const usize).read() };
        if fn_addr_raw == 0 {
            return init;
        }
        let f: FoldFn = unsafe { std::mem::transmute(fn_addr_raw) };
        let mut acc = init;
        for i in 0..vec.len {
            let x = unsafe { gos_rt_vec_get_i64(v, i) };
            acc = unsafe { f(env, acc, x) };
        }
        acc
    })
}

/// `iter::sum_by(f, xs)` for `Vec<i64>` -> i64. `f` maps each element
/// to its contribution.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_sum_by_i64(env: *const u8, v: *const GosVec) -> i64 {
    ffi_entry!(-1, {
        if env.is_null() || v.is_null() {
            return 0;
        }
        let vec = unsafe { &*v };
        type MapFn = unsafe extern "C" fn(env: *const u8, x: i64) -> i64;
        let fn_addr_raw = unsafe { (env as *const usize).read() };
        if fn_addr_raw == 0 {
            return 0;
        }
        let f: MapFn = unsafe { std::mem::transmute(fn_addr_raw) };
        let mut total: i64 = 0;
        for i in 0..vec.len {
            let x = unsafe { gos_rt_vec_get_i64(v, i) };
            total = total.wrapping_add(unsafe { f(env, x) });
        }
        total
    })
}

/// `iter::any(p, xs)` for `Vec<i64>` -> bool (returned as i64 0/1).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_any_i64(env: *const u8, v: *const GosVec) -> i64 {
    ffi_entry!(-1, {
        if env.is_null() || v.is_null() {
            return 0;
        }
        let vec = unsafe { &*v };
        type PredFn = unsafe extern "C" fn(env: *const u8, x: i64) -> bool;
        let fn_addr_raw = unsafe { (env as *const usize).read() };
        if fn_addr_raw == 0 {
            return 0;
        }
        let p: PredFn = unsafe { std::mem::transmute(fn_addr_raw) };
        for i in 0..vec.len {
            let x = unsafe { gos_rt_vec_get_i64(v, i) };
            if unsafe { p(env, x) } {
                return 1;
            }
        }
        0
    })
}

/// `iter::all(p, xs)` for `Vec<i64>` -> bool (returned as i64 0/1).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_all_i64(env: *const u8, v: *const GosVec) -> i64 {
    ffi_entry!(-1, {
        if env.is_null() || v.is_null() {
            return 1;
        }
        let vec = unsafe { &*v };
        type PredFn = unsafe extern "C" fn(env: *const u8, x: i64) -> bool;
        let fn_addr_raw = unsafe { (env as *const usize).read() };
        if fn_addr_raw == 0 {
            return 1;
        }
        let p: PredFn = unsafe { std::mem::transmute(fn_addr_raw) };
        for i in 0..vec.len {
            let x = unsafe { gos_rt_vec_get_i64(v, i) };
            if !unsafe { p(env, x) } {
                return 0;
            }
        }
        1
    })
}

/// `iter::all(p, xs)` for vectors whose elements must be passed by slot
/// address, such as user structs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_all_ptr(env: *const u8, v: *const GosVec) -> i64 {
    ffi_entry!(-1, {
        if env.is_null() || v.is_null() {
            return 1;
        }
        let vec = unsafe { &*v };
        type PredFn = unsafe extern "C" fn(env: *const u8, x: *mut u8) -> bool;
        let fn_addr_raw = unsafe { (env as *const usize).read() };
        if fn_addr_raw == 0 {
            return 1;
        }
        let p: PredFn = unsafe { std::mem::transmute(fn_addr_raw) };
        for i in 0..vec.len {
            let x = unsafe { gos_rt_vec_get_ptr(v, i) };
            if !unsafe { p(env, x) } {
                return 0;
            }
        }
        1
    })
}

/// `iter::any(p, xs)` for vectors whose elements must be passed by slot
/// address, such as user structs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_any_ptr(env: *const u8, v: *const GosVec) -> i64 {
    ffi_entry!(-1, {
        if env.is_null() || v.is_null() {
            return 0;
        }
        let vec = unsafe { &*v };
        type PredFn = unsafe extern "C" fn(env: *const u8, x: *mut u8) -> bool;
        let fn_addr_raw = unsafe { (env as *const usize).read() };
        if fn_addr_raw == 0 {
            return 0;
        }
        let p: PredFn = unsafe { std::mem::transmute(fn_addr_raw) };
        for i in 0..vec.len {
            let x = unsafe { gos_rt_vec_get_ptr(v, i) };
            if unsafe { p(env, x) } {
                return 1;
            }
        }
        0
    })
}

// ======================================================================
// std::option - non-closure accessors. The closure-taking option::map /
// and_then / filter / default_with / or_else / iter helpers stay in the
// interp VM only for the moment; they need per-shape thunks across all
// inner types, which is the open piece of the Phase 1b follow-up.

/// `option::is_some(opt)` - opt is the `*mut GosResult`-shaped enum
/// handle produced by the `Option<T>` constructor lowering (disc 0 =
/// Some, 1 = None per `lower_result_ctor`).
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_option_is_some(opt: i128) -> i64 {
    i64::from(super::vec::gos_rt_result_disc(opt) == 0)
}

/// `option::is_none(opt)`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_option_is_none(opt: i128) -> i64 {
    i64::from(super::vec::gos_rt_result_disc(opt) != 0)
}

/// `option::default(v, opt) -> v if opt is None else inner`. Specialised
/// for i64 payloads (the dominant case in arithmetic pipelines).
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_option_default_i64(fallback: i64, opt: i128) -> i64 {
    if super::vec::gos_rt_result_disc(opt) != 0 {
        fallback
    } else {
        super::vec::gos_rt_result_payload(opt)
    }
}

/// `option::default(v, opt)` specialised for f64 payloads: the stored
/// payload word is reinterpreted as its IEEE-754 bit pattern, and the
/// fallback rides the float register directly.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_option_default_f64(fallback: f64, opt: i128) -> f64 {
    if super::vec::gos_rt_result_disc(opt) != 0 {
        fallback
    } else {
        f64::from_bits(super::vec::gos_rt_result_payload(opt) as u64)
    }
}

/// `option::map(f, opt) -> Option<i64>`. Mirrors `iter::map` shape:
/// `env[0]` holds the closure body fn-addr (i64), and the closure
/// is called as `f(env, x) -> i64`. Returns a fresh `*mut GosResult`
/// (disc=0 Some, disc=1 None) so the surrounding pattern match
/// reads the standard discriminant.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_option_map_i64(env: *const u8, opt: i128) -> i128 {
    ffi_entry!(0i128, {
        // None passes through unchanged.
        if env.is_null() {
            return gos_rt_result_new(1, 0);
        }
        if super::vec::gos_rt_result_disc(opt) != 0 {
            return gos_rt_result_new(1, 0);
        }
        let payload = super::vec::gos_rt_result_payload(opt);
        type CallFn = unsafe extern "C" fn(env: *const u8, x: i64) -> i64;
        let fn_addr_raw = unsafe { (env as *const usize).read() };
        if fn_addr_raw == 0 {
            return unsafe { gos_rt_result_new(1, 0) };
        }
        let f: CallFn = unsafe { std::mem::transmute(fn_addr_raw) };
        let mapped = unsafe { f(env, payload) };
        unsafe { gos_rt_result_new(0, mapped) }
    })
}

/// `result::map(f, res) -> Result<i64, E>`. Mirror of
/// `gos_rt_option_map_i64`: maps Ok payload, passes Err through.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_result_map_i64(env: *const u8, res: i128) -> i128 {
    ffi_entry!(0i128, {
        if env.is_null() {
            return gos_rt_result_new(1, 0);
        }
        let disc = super::vec::gos_rt_result_disc(res);
        let payload = super::vec::gos_rt_result_payload(res);
        if disc != 0 {
            // Err - pass through.
            return gos_rt_result_new(disc, payload);
        }
        type CallFn = unsafe extern "C" fn(env: *const u8, x: i64) -> i64;
        let fn_addr_raw = unsafe { (env as *const usize).read() };
        if fn_addr_raw == 0 {
            return unsafe { gos_rt_result_new(1, 0) };
        }
        let f: CallFn = unsafe { std::mem::transmute(fn_addr_raw) };
        let mapped = unsafe { f(env, payload) };
        unsafe { gos_rt_result_new(0, mapped) }
    })
}

#[cfg(test)]
mod lazy_iterator_tests {
    use super::*;

    #[test]
    fn borrowed_vec_observes_element_replacement() {
        unsafe {
            let source = gos_rt_vec_new(8);
            gos_rt_vec_push_i64(source, 1);
            gos_rt_vec_push_i64(source, 2);
            let iter = gos_rt_lazy_iter_from_vec_i64(source);
            crate::c_abi::signal::gos_rt_vec_set_i64(source, 0, 9);
            assert_eq!(gos_rt_lazy_iter_sum_i64(iter), 11);
            crate::c_abi::map::gos_rt_vec_free(source);
        }
    }

    #[test]
    fn borrowed_vec_rejects_shape_restored_after_mutation() {
        unsafe {
            let source = gos_rt_vec_new(8);
            gos_rt_vec_push_i64(source, 1);
            gos_rt_vec_push_i64(source, 2);
            let iter = gos_rt_lazy_iter_from_vec_i64(source);
            gos_rt_vec_push_i64(source, 3);
            let mut popped = 0i64;
            assert_eq!(
                crate::c_abi::signal::gos_rt_vec_pop(
                    source,
                    std::ptr::addr_of_mut!(popped).cast::<u8>()
                ),
                1
            );
            assert_eq!(popped, 3);
            assert_ne!((*source).mutation_generation, 2);
            gos_rt_lazy_iter_drop_i64(iter);
            crate::c_abi::map::gos_rt_vec_free(source);
        }
    }

    #[test]
    fn next_is_idempotent_after_exhaustion() {
        unsafe {
            let iter = gos_rt_lazy_iter_range_i64(0, 2);
            assert_eq!(gos_rt_result_disc(gos_rt_lazy_iter_next_i64(iter)), 0);
            assert_eq!(gos_rt_result_payload(gos_rt_lazy_iter_next_i64(iter)), 1);
            assert_eq!(gos_rt_result_disc(gos_rt_lazy_iter_next_i64(iter)), 1);
            assert_eq!(gos_rt_result_disc(gos_rt_lazy_iter_next_i64(iter)), 1);
            gos_rt_lazy_iter_drop_i64(iter);
        }
    }

    #[test]
    fn open_range_boundary_follows_the_program_profile() {
        for checked in [true, false] {
            assert_eq!(
                advance_range_from_i64(i64::MAX - 1, checked),
                Some((i64::MAX - 1, i64::MAX))
            );
        }
        assert_eq!(advance_range_from_i64(i64::MAX, true), None);
        assert_eq!(
            advance_range_from_i64(i64::MAX, false),
            Some((i64::MAX, i64::MIN))
        );
    }

    #[test]
    fn dropping_unconsumed_vec_iterator_releases_its_source_once() {
        unsafe {
            let source = gos_rt_vec_new(8);
            gos_rt_vec_push_i64(source, 1);
            gos_rt_vec_push_i64(source, 2);
            let iter = gos_rt_lazy_iter_from_vec_i64(source);
            assert_eq!(crate::c_abi::vec::vec_rc(&*source), 2);

            crate::c_abi::map::gos_rt_vec_free(source);
            assert_eq!(crate::c_abi::vec::vec_rc(&*source), 1);
            gos_rt_lazy_iter_drop_i64(iter);
        }
    }
}
