//! Element-crossing entry points for the closure-taking `iter::` shims.
//!
//! The C ABI has no generics, so which register class a sequence element
//! reaches its callback in is carried by the symbol the compiler calls. Each
//! combinator here has one implementation and three entry points - word,
//! float, and by-address - so the spellings cannot drift apart, and the ABI
//! registry records which class each entry reads its buffer as.

use super::{
    GosMap, GosVec, gos_rt_map_insert_i64_i64, gos_rt_map_new, gos_rt_result_new,
    gos_rt_vec_get_i64, gos_rt_vec_get_ptr, gos_rt_vec_len, gos_rt_vec_new,
};
use crate::c_abi::vec::vec_push_elem_from;

const NONE: i128 = 1;

fn some_of(payload: i64) -> i128 {
    gos_rt_result_new(0, payload)
}

/// How a sequence element reaches a callback: as the word its slot spells, as
/// the `f64` those bits encode, or as the address of the element's storage.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ElemPass {
    Word,
    Float,
    Ptr,
}

type WordPred = unsafe extern "C" fn(env: *const u8, x: i64) -> bool;
type FloatPred = unsafe extern "C" fn(env: *const u8, x: f64) -> bool;
type PtrPred = unsafe extern "C" fn(env: *const u8, x: *mut u8) -> bool;

type WordToWord = unsafe extern "C" fn(env: *const u8, x: i64) -> i64;
type FloatToWord = unsafe extern "C" fn(env: *const u8, x: f64) -> i64;
type PtrToWord = unsafe extern "C" fn(env: *const u8, x: *mut u8) -> i64;

type WordToEnum = unsafe extern "C" fn(env: *const u8, x: i64) -> i128;
type FloatToEnum = unsafe extern "C" fn(env: *const u8, x: f64) -> i128;
type PtrToEnum = unsafe extern "C" fn(env: *const u8, x: *mut u8) -> i128;

type WordToVec = unsafe extern "C" fn(env: *const u8, x: i64) -> *mut GosVec;
type FloatToVec = unsafe extern "C" fn(env: *const u8, x: f64) -> *mut GosVec;
type PtrToVec = unsafe extern "C" fn(env: *const u8, x: *mut u8) -> *mut GosVec;

type WordCmp = unsafe extern "C" fn(env: *const u8, a: i64, b: i64) -> i64;
type FloatCmp = unsafe extern "C" fn(env: *const u8, a: f64, b: f64) -> i64;
type PtrCmp = unsafe extern "C" fn(env: *const u8, a: *mut u8, b: *mut u8) -> i64;

/// Callable address stored at `env[0]`, or `None` for a null or zero env.
pub(crate) fn env_fn_addr(env: *const u8) -> Option<*const ()> {
    if env.is_null() {
        return None;
    }
    // SAFETY: `env` is a live closure blob whose first word is the callable
    // address, the invariant the closure lowering establishes.
    let addr = unsafe { (env.cast::<usize>()).read() };
    if addr == 0 {
        None
    } else {
        Some(std::ptr::with_exposed_provenance::<()>(addr))
    }
}

fn vec_len_of(v: *const GosVec) -> i64 {
    if v.is_null() {
        0
    } else {
        // SAFETY: `v` is a live GosVec header.
        unsafe { gos_rt_vec_len(v) }
    }
}

/// A fresh vec carrying the same element width and kind as `src`, so what is
/// copied into it keeps the shape it had.
unsafe fn out_like(src: *const GosVec) -> *mut GosVec {
    if src.is_null() {
        // SAFETY: fresh allocation.
        return unsafe { gos_rt_vec_new(8) };
    }
    // SAFETY: the caller supplies a live GosVec header.
    let s = unsafe { &*src };
    unsafe { crate::c_abi::vec::gos_rt_vec_with_capacity_typed(s.elem_bytes, 0, s.elem_kind) }
}

/// Calls a bool-answering callback on `v[i]` through `pass`'s register class.
unsafe fn pred_at(
    addr: *const (),
    env: *const u8,
    v: *const GosVec,
    i: i64,
    pass: ElemPass,
) -> bool {
    // SAFETY: `addr` is the callable the closure lowering stored, compiled
    // against the same element class the symbol entered here declares.
    unsafe {
        match pass {
            ElemPass::Word => {
                let f: WordPred = std::mem::transmute(addr);
                f(env, gos_rt_vec_get_i64(v, i))
            }
            ElemPass::Float => {
                let f: FloatPred = std::mem::transmute(addr);
                f(env, f64::from_bits(gos_rt_vec_get_i64(v, i) as u64))
            }
            ElemPass::Ptr => {
                let f: PtrPred = std::mem::transmute(addr);
                f(env, gos_rt_vec_get_ptr(v, i))
            }
        }
    }
}

/// Calls an `i64`-answering callback on `v[i]` through `pass`'s class.
unsafe fn key_at(addr: *const (), env: *const u8, v: *const GosVec, i: i64, pass: ElemPass) -> i64 {
    // SAFETY: as `pred_at`.
    unsafe {
        match pass {
            ElemPass::Word => {
                let f: WordToWord = std::mem::transmute(addr);
                f(env, gos_rt_vec_get_i64(v, i))
            }
            ElemPass::Float => {
                let f: FloatToWord = std::mem::transmute(addr);
                f(env, f64::from_bits(gos_rt_vec_get_i64(v, i) as u64))
            }
            ElemPass::Ptr => {
                let f: PtrToWord = std::mem::transmute(addr);
                f(env, gos_rt_vec_get_ptr(v, i))
            }
        }
    }
}

/// Calls an `Option`-answering callback on `v[i]` through `pass`'s class.
unsafe fn opt_at(
    addr: *const (),
    env: *const u8,
    v: *const GosVec,
    i: i64,
    pass: ElemPass,
) -> i128 {
    // SAFETY: as `pred_at`.
    unsafe {
        match pass {
            ElemPass::Word => {
                let f: WordToEnum = std::mem::transmute(addr);
                f(env, gos_rt_vec_get_i64(v, i))
            }
            ElemPass::Float => {
                let f: FloatToEnum = std::mem::transmute(addr);
                f(env, f64::from_bits(gos_rt_vec_get_i64(v, i) as u64))
            }
            ElemPass::Ptr => {
                let f: PtrToEnum = std::mem::transmute(addr);
                f(env, gos_rt_vec_get_ptr(v, i))
            }
        }
    }
}

/// Calls a sequence-answering callback on `v[i]` through `pass`'s class.
unsafe fn seq_at(
    addr: *const (),
    env: *const u8,
    v: *const GosVec,
    i: i64,
    pass: ElemPass,
) -> *mut GosVec {
    // SAFETY: as `pred_at`.
    unsafe {
        match pass {
            ElemPass::Word => {
                let f: WordToVec = std::mem::transmute(addr);
                f(env, gos_rt_vec_get_i64(v, i))
            }
            ElemPass::Float => {
                let f: FloatToVec = std::mem::transmute(addr);
                f(env, f64::from_bits(gos_rt_vec_get_i64(v, i) as u64))
            }
            ElemPass::Ptr => {
                let f: PtrToVec = std::mem::transmute(addr);
                f(env, gos_rt_vec_get_ptr(v, i))
            }
        }
    }
}

/// Calls a comparator on `v[i]` and `v[j]` through `pass`'s class.
unsafe fn cmp_at(
    addr: *const (),
    env: *const u8,
    v: *const GosVec,
    i: i64,
    j: i64,
    pass: ElemPass,
) -> i64 {
    // SAFETY: as `pred_at`.
    unsafe {
        match pass {
            ElemPass::Word => {
                let f: WordCmp = std::mem::transmute(addr);
                f(env, gos_rt_vec_get_i64(v, i), gos_rt_vec_get_i64(v, j))
            }
            ElemPass::Float => {
                let f: FloatCmp = std::mem::transmute(addr);
                f(
                    env,
                    f64::from_bits(gos_rt_vec_get_i64(v, i) as u64),
                    f64::from_bits(gos_rt_vec_get_i64(v, j) as u64),
                )
            }
            ElemPass::Ptr => {
                let f: PtrCmp = std::mem::transmute(addr);
                f(env, gos_rt_vec_get_ptr(v, i), gos_rt_vec_get_ptr(v, j))
            }
        }
    }
}

/// Defines the three element-crossing entry points of one combinator over a
/// single implementation, so the word, float, and by-address spellings answer
/// the same way and cannot drift apart.
macro_rules! cross_shims {
    ($body:ident, ($($arg:ident: $aty:ty),*) -> $ret:ty, $word:ident, $float:ident, $ptr:ident, $fallback:expr, $doc:literal) => {
        #[doc = $doc]
        #[doc = ""]
        #[doc = "Word-slot elements."]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $word($($arg: $aty),*) -> $ret {
            ffi_entry!($fallback, { unsafe { $body($($arg),*, ElemPass::Word) } })
        }
        #[doc = $doc]
        #[doc = ""]
        #[doc = "`f64` elements, whose slot bits reach the callback in an SSE register."]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $float($($arg: $aty),*) -> $ret {
            ffi_entry!($fallback, { unsafe { $body($($arg),*, ElemPass::Float) } })
        }
        #[doc = $doc]
        #[doc = ""]
        #[doc = "Elements the callback receives by the address of their storage."]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $ptr($($arg: $aty),*) -> $ret {
            ffi_entry!($fallback, { unsafe { $body($($arg),*, ElemPass::Ptr) } })
        }
    };
}

// ----------------------------------------------------------------------
// take_while / skip_while

unsafe fn take_while_impl(env: *const u8, v: *const GosVec, pass: ElemPass) -> *mut GosVec {
    let out = unsafe { out_like(v) };
    let Some(addr) = env_fn_addr(env) else {
        return out;
    };
    for i in 0..vec_len_of(v) {
        if !unsafe { pred_at(addr, env, v, i, pass) } {
            break;
        }
        unsafe { vec_push_elem_from(out, v, i) };
    }
    out
}

cross_shims!(
    take_while_impl,
    (env: *const u8, v: *const GosVec) -> *mut GosVec,
    gos_rt_iter_take_while_i64,
    gos_rt_iter_take_while_f64,
    gos_rt_iter_take_while_ptr,
    std::ptr::null_mut(),
    "`iter::take_while(p, xs)` - the longest prefix of elements satisfying `p`."
);

unsafe fn skip_while_impl(env: *const u8, v: *const GosVec, pass: ElemPass) -> *mut GosVec {
    let out = unsafe { out_like(v) };
    let Some(addr) = env_fn_addr(env) else {
        return out;
    };
    let mut dropping = true;
    for i in 0..vec_len_of(v) {
        if dropping && unsafe { pred_at(addr, env, v, i, pass) } {
            continue;
        }
        dropping = false;
        unsafe { vec_push_elem_from(out, v, i) };
    }
    out
}

cross_shims!(
    skip_while_impl,
    (env: *const u8, v: *const GosVec) -> *mut GosVec,
    gos_rt_iter_skip_while_i64,
    gos_rt_iter_skip_while_f64,
    gos_rt_iter_skip_while_ptr,
    std::ptr::null_mut(),
    "`iter::skip_while(p, xs)` - the elements after the leading run satisfying `p`."
);

// ----------------------------------------------------------------------
// position

unsafe fn position_impl(env: *const u8, v: *const GosVec, pass: ElemPass) -> i128 {
    let Some(addr) = env_fn_addr(env) else {
        return NONE;
    };
    for i in 0..vec_len_of(v) {
        if unsafe { pred_at(addr, env, v, i, pass) } {
            return some_of(i);
        }
    }
    NONE
}

cross_shims!(
    position_impl,
    (env: *const u8, v: *const GosVec) -> i128,
    gos_rt_iter_position_i64,
    gos_rt_iter_position_f64,
    gos_rt_iter_position_ptr,
    NONE,
    "`iter::position(p, xs) -> Option<i64>` - index of the first match."
);

// ----------------------------------------------------------------------
// find, whose answer is built from a value and a companion flag

unsafe fn find_impl(env: *const u8, v: *const GosVec, pass: ElemPass) -> i64 {
    let Some(addr) = env_fn_addr(env) else {
        return 0;
    };
    for i in 0..vec_len_of(v) {
        if unsafe { pred_at(addr, env, v, i, pass) } {
            return match pass {
                // SAFETY: `v` is live and `i` is in range.
                ElemPass::Ptr => (unsafe { gos_rt_vec_get_ptr(v, i) }) as usize as i64,
                _ => unsafe { gos_rt_vec_get_i64(v, i) },
            };
        }
    }
    0
}

cross_shims!(
    find_impl,
    (env: *const u8, v: *const GosVec) -> i64,
    gos_rt_iter_find_i64,
    gos_rt_iter_find_f64,
    gos_rt_iter_find_ptr,
    0,
    "`iter::find(p, xs)` - the first matching element (0 when none; pair with the flag shim)."
);

unsafe fn find_flag_impl(env: *const u8, v: *const GosVec, pass: ElemPass) -> i64 {
    let Some(addr) = env_fn_addr(env) else {
        return 0;
    };
    for i in 0..vec_len_of(v) {
        if unsafe { pred_at(addr, env, v, i, pass) } {
            return 1;
        }
    }
    0
}

cross_shims!(
    find_flag_impl,
    (env: *const u8, v: *const GosVec) -> i64,
    gos_rt_iter_find_i64_flag,
    gos_rt_iter_find_f64_flag,
    gos_rt_iter_find_ptr_flag,
    0,
    "Companion flag for the `iter::find` shims: 1 when some element matched."
);

// ----------------------------------------------------------------------
// filter_map / find_map

unsafe fn filter_map_impl(
    env: *const u8,
    v: *const GosVec,
    out_bytes: i64,
    by_block: i64,
    pass: ElemPass,
) -> *mut GosVec {
    // The kept payloads are the callback's results, so the result carries the
    // payload's own declared width rather than the input element's. A payload
    // the callback answers by address is copied whole out of that address.
    let width = u32::try_from(out_bytes.max(1)).unwrap_or(8);
    // SAFETY: fresh allocation.
    let out = unsafe { gos_rt_vec_new(width) };
    let Some(addr) = env_fn_addr(env) else {
        return out;
    };
    for i in 0..vec_len_of(v) {
        let r = unsafe { opt_at(addr, env, v, i, pass) };
        if crate::c_abi::gos_rt_result_disc(r) != 0 {
            continue;
        }
        let payload = crate::c_abi::gos_rt_result_payload(r);
        if by_block != 0 {
            let src: *const u8 = std::ptr::with_exposed_provenance(payload as usize);
            if src.is_null() {
                continue;
            }
            // SAFETY: the callback answered the address of a payload of the
            // declared width.
            unsafe { crate::c_abi::gos_rt_vec_push(out, src) };
        } else {
            // SAFETY: `out` holds word-slot elements.
            unsafe { crate::c_abi::gos_rt_vec_push_i64(out, payload) };
        }
    }
    out
}

cross_shims!(
    filter_map_impl,
    (env: *const u8, v: *const GosVec, out_bytes: i64, by_block: i64) -> *mut GosVec,
    gos_rt_iter_filter_map_i64,
    gos_rt_iter_filter_map_f64,
    gos_rt_iter_filter_map_ptr,
    std::ptr::null_mut(),
    "`iter::filter_map(f, xs)` - the Some payloads of `f(x)`. The trailing arguments are the declared width of the kept payload and whether the callback answers its storage address."
);

unsafe fn find_map_impl(env: *const u8, v: *const GosVec, pass: ElemPass) -> i128 {
    let Some(addr) = env_fn_addr(env) else {
        return NONE;
    };
    for i in 0..vec_len_of(v) {
        let r = unsafe { opt_at(addr, env, v, i, pass) };
        if crate::c_abi::gos_rt_result_disc(r) == 0 {
            return r;
        }
    }
    NONE
}

cross_shims!(
    find_map_impl,
    (env: *const u8, v: *const GosVec) -> i128,
    gos_rt_iter_find_map_i64,
    gos_rt_iter_find_map_f64,
    gos_rt_iter_find_map_ptr,
    NONE,
    "`iter::find_map(f, xs) -> Option<U>` - the first Some payload of `f(x)`."
);

// ----------------------------------------------------------------------
// reduce / min_by / max_by, which answer an element of the input

/// Index of the element a comparator selects, folding from the first element.
/// `keep_greater` picks the later element when the comparison is positive.
unsafe fn select_by(
    env: *const u8,
    v: *const GosVec,
    pass: ElemPass,
    keep_greater: bool,
) -> Option<i64> {
    let addr = env_fn_addr(env)?;
    let len = vec_len_of(v);
    if len == 0 {
        return None;
    }
    let mut best = 0;
    for i in 1..len {
        let ord = unsafe { cmp_at(addr, env, v, i, best, pass) };
        if (keep_greater && ord > 0) || (!keep_greater && ord < 0) {
            best = i;
        }
    }
    Some(best)
}

/// An `Option` holding `v[idx]`. A by-address element travels as the address
/// of its storage, which is the payload word the carrier holds for it; every
/// other class travels as the word its slot spells.
unsafe fn some_elem_at(v: *const GosVec, idx: i64, pass: ElemPass) -> i128 {
    // SAFETY: `v` is a live GosVec and `idx` is in range.
    unsafe {
        match pass {
            ElemPass::Ptr => some_of(gos_rt_vec_get_ptr(v, idx) as usize as i64),
            _ => some_of(gos_rt_vec_get_i64(v, idx)),
        }
    }
}

unsafe fn reduce_impl(env: *const u8, v: *const GosVec, pass: ElemPass) -> i128 {
    // `reduce` folds with the first element as the seed, so the accumulator
    // is an element and the callback answers one.
    let Some(addr) = env_fn_addr(env) else {
        return NONE;
    };
    let len = vec_len_of(v);
    if len == 0 {
        return NONE;
    }
    let mut acc = match pass {
        // SAFETY: `v` is live and index 0 is in range.
        ElemPass::Ptr => (unsafe { gos_rt_vec_get_ptr(v, 0) }) as usize as i64,
        _ => unsafe { gos_rt_vec_get_i64(v, 0) },
    };
    for i in 1..len {
        acc = unsafe { fold_step(addr, env, acc, v, i, pass) };
    }
    some_of(acc)
}

/// One `(acc, element) -> acc` step where the accumulator shares the element's
/// class, which is the shape `reduce` folds with.
unsafe fn fold_step(
    addr: *const (),
    env: *const u8,
    acc: i64,
    v: *const GosVec,
    i: i64,
    pass: ElemPass,
) -> i64 {
    unsafe { fold_step_typed(addr, env, acc, v, i, pass, pass) }
}

/// One `(acc, element) -> acc` step with the element in `elem`'s class and the
/// accumulator in `acc_pass`'s, which are independent: a word accumulator over
/// a float sequence rides an integer register while the element rides an SSE
/// one, and a signature naming the wrong pair reads the other's bits.
unsafe fn fold_step_typed(
    addr: *const (),
    env: *const u8,
    acc: i64,
    v: *const GosVec,
    i: i64,
    elem: ElemPass,
    acc_pass: ElemPass,
) -> i64 {
    type WordWord = unsafe extern "C" fn(env: *const u8, acc: i64, x: i64) -> i64;
    type WordFloat = unsafe extern "C" fn(env: *const u8, acc: i64, x: f64) -> i64;
    type WordPtr = unsafe extern "C" fn(env: *const u8, acc: i64, x: *mut u8) -> i64;
    type FloatWord = unsafe extern "C" fn(env: *const u8, acc: f64, x: i64) -> f64;
    type FloatFloat = unsafe extern "C" fn(env: *const u8, acc: f64, x: f64) -> f64;
    type FloatPtr = unsafe extern "C" fn(env: *const u8, acc: f64, x: *mut u8) -> f64;
    // SAFETY: `addr` is the callable the closure lowering stored, compiled
    // against the pair of classes this symbol declares.
    unsafe {
        let acc_f = f64::from_bits(acc as u64);
        match (acc_pass, elem) {
            (ElemPass::Float, ElemPass::Word) => {
                let f: FloatWord = std::mem::transmute(addr);
                f(env, acc_f, gos_rt_vec_get_i64(v, i)).to_bits() as i64
            }
            (ElemPass::Float, ElemPass::Float) => {
                let f: FloatFloat = std::mem::transmute(addr);
                f(env, acc_f, f64::from_bits(gos_rt_vec_get_i64(v, i) as u64)).to_bits() as i64
            }
            (ElemPass::Float, ElemPass::Ptr) => {
                let f: FloatPtr = std::mem::transmute(addr);
                f(env, acc_f, gos_rt_vec_get_ptr(v, i)).to_bits() as i64
            }
            (_, ElemPass::Word) => {
                let f: WordWord = std::mem::transmute(addr);
                f(env, acc, gos_rt_vec_get_i64(v, i))
            }
            (_, ElemPass::Float) => {
                let f: WordFloat = std::mem::transmute(addr);
                f(env, acc, f64::from_bits(gos_rt_vec_get_i64(v, i) as u64))
            }
            (_, ElemPass::Ptr) => {
                let f: WordPtr = std::mem::transmute(addr);
                f(env, acc, gos_rt_vec_get_ptr(v, i))
            }
        }
    }
}

cross_shims!(
    reduce_impl,
    (env: *const u8, v: *const GosVec) -> i128,
    gos_rt_iter_reduce_i64,
    gos_rt_iter_reduce_f64,
    gos_rt_iter_reduce_ptr,
    NONE,
    "`iter::reduce(f, xs) -> Option<T>` - a fold seeded by the first element."
);

unsafe fn min_by_impl(env: *const u8, v: *const GosVec, pass: ElemPass) -> i128 {
    match unsafe { select_by(env, v, pass, false) } {
        Some(idx) => unsafe { some_elem_at(v, idx, pass) },
        None => NONE,
    }
}

cross_shims!(
    min_by_impl,
    (env: *const u8, v: *const GosVec) -> i128,
    gos_rt_iter_min_by_i64,
    gos_rt_iter_min_by_f64,
    gos_rt_iter_min_by_ptr,
    NONE,
    "`iter::min_by(cmp, xs) -> Option<T>` - the smallest element by `cmp`."
);

unsafe fn max_by_impl(env: *const u8, v: *const GosVec, pass: ElemPass) -> i128 {
    match unsafe { select_by(env, v, pass, true) } {
        Some(idx) => unsafe { some_elem_at(v, idx, pass) },
        None => NONE,
    }
}

cross_shims!(
    max_by_impl,
    (env: *const u8, v: *const GosVec) -> i128,
    gos_rt_iter_max_by_i64,
    gos_rt_iter_max_by_f64,
    gos_rt_iter_max_by_ptr,
    NONE,
    "`iter::max_by(cmp, xs) -> Option<T>` - the largest element by `cmp`."
);

// ----------------------------------------------------------------------
// sort_by

unsafe fn sort_by_impl(env: *const u8, v: *const GosVec, pass: ElemPass) -> *mut GosVec {
    let out = unsafe { out_like(v) };
    let Some(addr) = env_fn_addr(env) else {
        return out;
    };
    let len = vec_len_of(v);
    let mut order: Vec<i64> = (0..len).collect();
    // An insertion sort keeps the comparator's calls in a fixed order, which
    // is what makes the answer identical on every tier for a comparator that
    // is not a total order.
    for i in 1..order.len() {
        let mut j = i;
        while j > 0 {
            let ord = unsafe { cmp_at(addr, env, v, order[j], order[j - 1], pass) };
            if ord >= 0 {
                break;
            }
            order.swap(j, j - 1);
            j -= 1;
        }
    }
    for idx in order {
        unsafe { vec_push_elem_from(out, v, idx) };
    }
    out
}

cross_shims!(
    sort_by_impl,
    (env: *const u8, v: *const GosVec) -> *mut GosVec,
    gos_rt_iter_sorted_by_i64,
    gos_rt_iter_sorted_by_f64,
    gos_rt_iter_sorted_by_ptr,
    std::ptr::null_mut(),
    "`iter::sort_by(cmp, xs)` - a copy ordered by the comparison closure."
);

// ----------------------------------------------------------------------
// scan / product_by

/// `iter::scan(init, f, xs)` over `elem`-class elements and an `acc`-class
/// accumulator, which the element's class does not decide.
unsafe fn scan_impl(
    init: i64,
    env: *const u8,
    v: *const GosVec,
    elem: ElemPass,
    acc_pass: ElemPass,
) -> *mut GosVec {
    // Each output is an accumulator the callback produced, so the result is a
    // word-slot sequence whatever the input element was.
    let out = unsafe { gos_rt_vec_new(8) };
    let Some(addr) = env_fn_addr(env) else {
        return out;
    };
    let mut acc = init;
    for i in 0..vec_len_of(v) {
        acc = unsafe { fold_step_typed(addr, env, acc, v, i, elem, acc_pass) };
        unsafe { crate::c_abi::gos_rt_vec_push_i64(out, acc) };
    }
    out
}

/// Defines one `scan` entry point per (element class, accumulator class) pair,
/// over the shared implementation, so the six spellings cannot drift apart.
macro_rules! scan_shims {
    ($( $name:ident = ($elem:expr, $acc:expr) ),+ $(,)?) => {
        $(
            /// `iter::scan(init, f, xs)` - a running fold, one output per element.
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn $name(
                init: i64,
                env: *const u8,
                v: *const GosVec,
            ) -> *mut GosVec {
                ffi_entry!(std::ptr::null_mut(), {
                    unsafe { scan_impl(init, env, v, $elem, $acc) }
                })
            }
        )+
    };
}

scan_shims!(
    gos_rt_iter_scan_i64 = (ElemPass::Word, ElemPass::Word),
    gos_rt_iter_scan_word_f64 = (ElemPass::Word, ElemPass::Float),
    gos_rt_iter_scan_f64 = (ElemPass::Float, ElemPass::Float),
    gos_rt_iter_scan_f64_word = (ElemPass::Float, ElemPass::Word),
    gos_rt_iter_scan_ptr = (ElemPass::Ptr, ElemPass::Word),
    gos_rt_iter_scan_ptr_f64 = (ElemPass::Ptr, ElemPass::Float),
);

unsafe fn product_by_impl(env: *const u8, v: *const GosVec, pass: ElemPass) -> i64 {
    let Some(addr) = env_fn_addr(env) else {
        return 1;
    };
    let mut prod: i64 = 1;
    for i in 0..vec_len_of(v) {
        prod = prod.wrapping_mul(unsafe { key_at(addr, env, v, i, pass) });
    }
    prod
}

cross_shims!(
    product_by_impl,
    (env: *const u8, v: *const GosVec) -> i64,
    gos_rt_iter_product_by_i64,
    gos_rt_iter_product_by_f64,
    gos_rt_iter_product_by_ptr,
    1,
    "`iter::product_by(f, xs)` - the product of `f(x)` over the sequence."
);

// ----------------------------------------------------------------------
// partition

unsafe fn partition_impl(env: *const u8, v: *const GosVec, pass: ElemPass) -> *mut u8 {
    let yes = unsafe { out_like(v) };
    let no = unsafe { out_like(v) };
    if let Some(addr) = env_fn_addr(env) {
        for i in 0..vec_len_of(v) {
            let dst = if unsafe { pred_at(addr, env, v, i, pass) } {
                yes
            } else {
                no
            };
            unsafe { vec_push_elem_from(dst, v, i) };
        }
    }
    alloc_pair(yes as i64, no as i64)
}

/// A GC-tracked two-slot tuple; the by-value aggregate ABI copies 16
/// contiguous bytes from the returned address.
fn alloc_pair(a: i64, b: i64) -> *mut u8 {
    let p = crate::c_abi::gos_rt_gc_alloc(16);
    if !p.is_null() {
        // SAFETY: `p` is a fresh 16-byte allocation.
        unsafe {
            let slots = p.cast::<i64>();
            *slots = a;
            *slots.add(1) = b;
        }
    }
    p
}

cross_shims!(
    partition_impl,
    (env: *const u8, v: *const GosVec) -> *mut u8,
    gos_rt_iter_partition_i64,
    gos_rt_iter_partition_f64,
    gos_rt_iter_partition_ptr,
    std::ptr::null_mut(),
    "`iter::partition(p, xs) -> ([T], [T])` - matching elements first."
);

// ----------------------------------------------------------------------
// chunk_by / count_by

unsafe fn chunk_by_impl(env: *const u8, v: *const GosVec, pass: ElemPass) -> *mut GosMap {
    let out = unsafe { gos_rt_map_new(8, 8) };
    let Some(addr) = env_fn_addr(env) else {
        return out;
    };
    let mut keys: Vec<i64> = Vec::new();
    let mut groups: Vec<*mut GosVec> = Vec::new();
    for i in 0..vec_len_of(v) {
        let k = unsafe { key_at(addr, env, v, i, pass) };
        let slot = if let Some(at) = keys.iter().position(|&seen| seen == k) {
            groups[at]
        } else {
            let g = unsafe { out_like(v) };
            keys.push(k);
            groups.push(g);
            g
        };
        unsafe { vec_push_elem_from(slot, v, i) };
    }
    for (k, g) in keys.into_iter().zip(groups) {
        unsafe { gos_rt_map_insert_i64_i64(out, k, g as i64) };
    }
    // Each group was built here and handed to the map, so the map owns it.
    unsafe { crate::c_abi::map::gos_rt_map_set_vec_values(out) };
    out
}

cross_shims!(
    chunk_by_impl,
    (env: *const u8, v: *const GosVec) -> *mut GosMap,
    gos_rt_iter_group_by_i64,
    gos_rt_iter_group_by_f64,
    gos_rt_iter_group_by_ptr,
    std::ptr::null_mut(),
    "`iter::chunk_by(f, xs)` - the elements grouped into a map keyed by `f(x)`."
);

unsafe fn count_by_impl(env: *const u8, v: *const GosVec, pass: ElemPass) -> *mut GosMap {
    let out = unsafe { gos_rt_map_new(8, 8) };
    let Some(addr) = env_fn_addr(env) else {
        return out;
    };
    let mut keys: Vec<i64> = Vec::new();
    let mut counts: Vec<i64> = Vec::new();
    for i in 0..vec_len_of(v) {
        let k = unsafe { key_at(addr, env, v, i, pass) };
        if let Some(at) = keys.iter().position(|&seen| seen == k) {
            counts[at] += 1;
        } else {
            keys.push(k);
            counts.push(1);
        }
    }
    for (k, c) in keys.into_iter().zip(counts) {
        unsafe { gos_rt_map_insert_i64_i64(out, k, c) };
    }
    out
}

cross_shims!(
    count_by_impl,
    (env: *const u8, v: *const GosVec) -> *mut GosMap,
    gos_rt_iter_count_by_i64,
    gos_rt_iter_count_by_f64,
    gos_rt_iter_count_by_ptr,
    std::ptr::null_mut(),
    "`iter::count_by(f, xs)` - a map from `f(x)` to the number of elements."
);

// ----------------------------------------------------------------------
// flat_map

unsafe fn flat_map_impl(env: *const u8, v: *const GosVec, pass: ElemPass) -> *mut GosVec {
    let Some(addr) = env_fn_addr(env) else {
        // SAFETY: fresh allocation.
        return unsafe { gos_rt_vec_new(8) };
    };
    // The concatenation carries the callback's element, so the result takes
    // its width and kind from the first sequence the callback answers.
    let mut out: *mut GosVec = std::ptr::null_mut();
    for i in 0..vec_len_of(v) {
        let inner = unsafe { seq_at(addr, env, v, i, pass) };
        if inner.is_null() {
            continue;
        }
        if out.is_null() {
            out = unsafe { out_like(inner) };
        }
        for j in 0..vec_len_of(inner) {
            unsafe { vec_push_elem_from(out, inner, j) };
        }
        // The callback answered a share of its sequence; each copied element
        // took a share of its own, so the sequence's share is given back.
        unsafe { crate::c_abi::map::gos_rt_vec_free(inner) };
    }
    if out.is_null() {
        // SAFETY: fresh allocation.
        out = unsafe { gos_rt_vec_new(8) };
    }
    out
}

cross_shims!(
    flat_map_impl,
    (env: *const u8, v: *const GosVec) -> *mut GosVec,
    gos_rt_iter_flat_map_i64,
    gos_rt_iter_flat_map_f64,
    gos_rt_iter_flat_map_ptr,
    std::ptr::null_mut(),
    "`iter::flat_map(f, xs)` - the sequences `f(x)` answers, concatenated."
);

// ----------------------------------------------------------------------
// flat_map over a callback answering a fixed array

unsafe fn flat_map_arr_impl(
    env: *const u8,
    v: *const GosVec,
    arr_len: i64,
    pass: ElemPass,
) -> *mut GosVec {
    // A fixed array is a raw buffer of contiguous slots with no header, so
    // the result is a word-slot sequence whatever the element read was.
    let out = unsafe { gos_rt_vec_new(8) };
    let Some(addr) = env_fn_addr(env) else {
        return out;
    };
    for i in 0..vec_len_of(v) {
        let buf: *const i64 =
            std::ptr::with_exposed_provenance(unsafe { key_at(addr, env, v, i, pass) } as usize);
        if buf.is_null() {
            continue;
        }
        for j in 0..arr_len.max(0) {
            // SAFETY: the callback answered a live buffer of `arr_len`
            // contiguous slots, which is the fixed-array ABI.
            let word = unsafe { buf.add(j as usize).read() };
            unsafe { crate::c_abi::gos_rt_vec_push_i64(out, word) };
        }
    }
    out
}

cross_shims!(
    flat_map_arr_impl,
    (env: *const u8, v: *const GosVec, arr_len: i64) -> *mut GosVec,
    gos_rt_iter_flat_map_arr_i64,
    gos_rt_iter_flat_map_arr_f64,
    gos_rt_iter_flat_map_arr_ptr,
    std::ptr::null_mut(),
    "`iter::flat_map(f, xs)` where `f` answers a fixed array: its slots, concatenated. The trailing argument is that array's length."
);

// ----------------------------------------------------------------------
// sort_by_key, whose key class is independent of the element's

unsafe fn sort_by_key_impl(
    env: *const u8,
    v: *const GosVec,
    key_is_f64: i64,
    pass: ElemPass,
) -> *mut GosVec {
    let out = unsafe { out_like(v) };
    let Some(addr) = env_fn_addr(env) else {
        return out;
    };
    let len = vec_len_of(v);
    let mut keyed: Vec<(i64, SortKey)> = Vec::with_capacity(len.max(0) as usize);
    for i in 0..len {
        keyed.push((i, unsafe {
            sort_key_at(addr, env, v, i, key_is_f64 != 0, pass)
        }));
    }
    keyed.sort_by(|(_, a), (_, b)| a.order(*b));
    for (idx, _) in keyed {
        unsafe { vec_push_elem_from(out, v, idx) };
    }
    out
}

/// The key `f`'s callback answers for `v[i]`, in the shape the `key_is_f64`
/// flag names, with the element passed in `pass`'s register class.
unsafe fn sort_key_at(
    addr: *const (),
    env: *const u8,
    v: *const GosVec,
    i: i64,
    key_is_f64: bool,
    pass: ElemPass,
) -> SortKey {
    // SAFETY: as `pred_at`.
    unsafe {
        match pass {
            ElemPass::Word => key_of_word(env, addr, gos_rt_vec_get_i64(v, i), key_is_f64),
            ElemPass::Float => key_of_float(env, addr, gos_rt_vec_get_i64(v, i), key_is_f64),
            ElemPass::Ptr => key_of_ptr(env, addr, gos_rt_vec_get_ptr(v, i), key_is_f64),
        }
    }
}

type WordToF64 = unsafe extern "C" fn(env: *const u8, x: i64) -> f64;
type FloatToF64 = unsafe extern "C" fn(env: *const u8, x: f64) -> f64;
type PtrToF64 = unsafe extern "C" fn(env: *const u8, x: *mut u8) -> f64;

/// One key a `*_by_key` combinator orders by. Which shape a call sees is
/// fixed by the `key_is_f64` flag the lowering passes, so the two never mix
/// within one traversal.
#[derive(Clone, Copy)]
pub(crate) enum SortKey {
    Int(i64),
    Float(f64),
}

impl SortKey {
    /// Order against another key of the same shape. Float keys order by
    /// `f64::total_cmp`, the same total order `iter::max` and `iter::min`
    /// place a float sequence in.
    pub(crate) fn order(self, other: Self) -> std::cmp::Ordering {
        match (self, other) {
            (Self::Int(a), Self::Int(b)) => a.cmp(&b),
            (Self::Float(a), Self::Float(b)) => a.total_cmp(&b),
            _ => std::cmp::Ordering::Equal,
        }
    }

    /// Whether this key displaces `best` for the requested extreme.
    fn beats(self, best: Self, want_max: bool) -> bool {
        let ord = self.order(best);
        if want_max { ord.is_gt() } else { ord.is_lt() }
    }
}

/// Calls the key callback for an element that rides an integer register.
unsafe fn key_of_word(env: *const u8, addr: *const (), x: i64, key_is_f64: bool) -> SortKey {
    if key_is_f64 {
        // SAFETY: addr is the callable stored by the closure lowering, whose
        // shape the flag names.
        let f: WordToF64 = unsafe { std::mem::transmute(addr) };
        SortKey::Float(unsafe { f(env, x) })
    } else {
        // SAFETY: as above.
        let f: WordToWord = unsafe { std::mem::transmute(addr) };
        SortKey::Int(unsafe { f(env, x) })
    }
}

/// Calls the key callback for an `f64` element, which rides an SSE register.
unsafe fn key_of_float(env: *const u8, addr: *const (), bits: i64, key_is_f64: bool) -> SortKey {
    let x = f64::from_bits(bits as u64);
    if key_is_f64 {
        // SAFETY: addr is the callable stored by the closure lowering, whose
        // shape the flag names.
        let f: FloatToF64 = unsafe { std::mem::transmute(addr) };
        SortKey::Float(unsafe { f(env, x) })
    } else {
        // SAFETY: as above.
        let f: FloatToWord = unsafe { std::mem::transmute(addr) };
        SortKey::Int(unsafe { f(env, x) })
    }
}

/// Calls the key callback for an aggregate element, passed by address.
pub(crate) unsafe fn key_of_ptr(
    env: *const u8,
    addr: *const (),
    x: *mut u8,
    key_is_f64: bool,
) -> SortKey {
    if key_is_f64 {
        // SAFETY: addr is the callable stored by the closure lowering, whose
        // shape the flag names.
        let f: PtrToF64 = unsafe { std::mem::transmute(addr) };
        SortKey::Float(unsafe { f(env, x) })
    } else {
        // SAFETY: as above.
        let f: PtrToWord = unsafe { std::mem::transmute(addr) };
        SortKey::Int(unsafe { f(env, x) })
    }
}

cross_shims!(
    sort_by_key_impl,
    (env: *const u8, v: *const GosVec, key_is_f64: i64) -> *mut GosVec,
    gos_rt_iter_sorted_by_key_i64,
    gos_rt_iter_sorted_by_key_f64,
    gos_rt_iter_sorted_by_key_ptr,
    std::ptr::null_mut(),
    "`iter::sort_by_key(f, xs)` - a copy ordered by the derived key."
);

// ----------------------------------------------------------------------
// min_by_key / max_by_key

unsafe fn select_by_key(
    env: *const u8,
    v: *const GosVec,
    key_is_f64: i64,
    pass: ElemPass,
    keep_greater: bool,
) -> i128 {
    let Some(addr) = env_fn_addr(env) else {
        return NONE;
    };
    let len = vec_len_of(v);
    if len == 0 {
        return NONE;
    }
    let mut best = 0;
    let mut best_key = unsafe { sort_key_at(addr, env, v, 0, key_is_f64 != 0, pass) };
    for i in 1..len {
        let k = unsafe { sort_key_at(addr, env, v, i, key_is_f64 != 0, pass) };
        if k.beats(best_key, keep_greater) {
            best = i;
            best_key = k;
        }
    }
    unsafe { some_elem_at(v, best, pass) }
}

unsafe fn min_by_key_impl(
    env: *const u8,
    v: *const GosVec,
    key_is_f64: i64,
    pass: ElemPass,
) -> i128 {
    unsafe { select_by_key(env, v, key_is_f64, pass, false) }
}

cross_shims!(
    min_by_key_impl,
    (env: *const u8, v: *const GosVec, key_is_f64: i64) -> i128,
    gos_rt_iter_min_by_key_i64,
    gos_rt_iter_min_by_key_f64,
    gos_rt_iter_min_by_key_ptr,
    NONE,
    "`iter::min_by_key(f, xs) -> Option<T>` - the element with the smallest key."
);

unsafe fn max_by_key_impl(
    env: *const u8,
    v: *const GosVec,
    key_is_f64: i64,
    pass: ElemPass,
) -> i128 {
    unsafe { select_by_key(env, v, key_is_f64, pass, true) }
}

cross_shims!(
    max_by_key_impl,
    (env: *const u8, v: *const GosVec, key_is_f64: i64) -> i128,
    gos_rt_iter_max_by_key_i64,
    gos_rt_iter_max_by_key_f64,
    gos_rt_iter_max_by_key_ptr,
    NONE,
    "`iter::max_by_key(f, xs) -> Option<T>` - the element with the largest key."
);
