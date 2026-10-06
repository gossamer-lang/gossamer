//! Closure-taking std combinator shims for the compiled tiers:
//! `result::and_then` / `or_else` / `ok` / `err`, the `option::*`
//! family, and the newer closure-taking `iter::*` entries. Mirrors
//! the interp builtins in `gossamer-interp/src/stdlib_builtins` so
//! every tier produces identical results.
//!
//! Closure ABI (shared with the `gos_rt_iter_*_i64` family in
//! `uuid.rs`): `env` is a heap blob whose first word is the callable
//! address - the lifted closure body for capturing closures, or a
//! per-shape thunk (`__fn_thunk_*`) for fn items and non-capturing
//! closures. Callbacks are invoked as `f(env, args…)`.
//!
//! Result / Option ABI: packed `i128` - low word discriminant
//! (0 = Ok/Some, 1 = Err/None), high word payload (see `pack_result`).

use super::{
    GosVec, gos_rt_result_disc, gos_rt_result_new, gos_rt_result_payload, gos_rt_vec_new,
    gos_rt_vec_push_i64,
};

type PredFn = unsafe extern "C-unwind" fn(env: *const u8, x: i64) -> bool;
type EnumFn = unsafe extern "C-unwind" fn(env: *const u8, x: i64) -> i128;
type ThunkEnumFn = unsafe extern "C-unwind" fn(env: *const u8) -> i128;
type ThunkValFn = unsafe extern "C-unwind" fn(env: *const u8) -> i64;

const NONE: i128 = 1;

/// Callable address stored at `env[0]`, or `None` for null/zero envs.
///
/// # Safety
///
/// `env` is null or a live closure environment, whose first word is the
/// closure's entry address.
unsafe fn env_fn_addr(env: *const u8) -> Option<*const ()> {
    if env.is_null() {
        return None;
    }
    // SAFETY: `env` is a live closure blob whose first word is the
    // callable address (codegen invariant shared with `gos_rt_iter_*`).
    let addr = unsafe { (env.cast::<usize>()).read() };
    if addr == 0 {
        None
    } else {
        // Recover the address's exposed provenance so the pointer is
        // sound to call under strict provenance; a bare integer
        // transmute at the call site would carry none.
        Some(std::ptr::with_exposed_provenance::<()>(addr))
    }
}

fn some_of(payload: i64) -> i128 {
    gos_rt_result_new(0, payload)
}

/// Loads the 16-byte enum value whose heap address is `addr` (the
/// payload representation of a nested Result/Option), or None when
/// the address is null.
///
/// # Safety
///
/// A non-zero `addr` addresses a live 16-byte enum copy.
unsafe fn load_enum_at(addr: i64) -> i128 {
    if addr == 0 {
        return NONE;
    }
    // SAFETY: an enum-payload word is the address of the live 16-byte
    // heap copy made at construction (`gos_rt_result_payload_i128`'s
    // contract).
    unsafe {
        let p = addr as usize as *const i64;
        let lo = (*p) as u64 as u128;
        let hi = (*p.add(1)) as u64 as u128;
        ((hi << 64) | lo) as i128
    }
}

/// GC-tracked 2-slot tuple `(a, b)`; the by-value-aggregate ABI
/// memcpys 16 contiguous bytes from the returned pointer.
fn alloc_pair(a: i64, b: i64) -> *mut u8 {
    let p = super::gos_rt_gc_alloc(16);
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

fn vec_from(xs: &[i64]) -> *mut GosVec {
    let out = gos_rt_vec_new(8);
    for &x in xs {
        // SAFETY: `out` is the fresh vec made above, or null, which `gos_rt_vec_push_i64`
        // accepts.
        unsafe { gos_rt_vec_push_i64(out, x) };
    }
    out
}

// ----------------------------------------------------------------------
// result

/// `result::and_then(f, res)` - `f(ok_payload) -> Result` when Ok,
/// Err passthrough. Callback returns a packed i128 Result.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_result_and_then(res: i128, env: *const u8) -> i128 {
    ffi_entry_passthrough!(0i128, {
        if gos_rt_result_disc(res) != 0 {
            return res;
        }
        // SAFETY: `env` is this shim's argument, as `env_fn_addr` requires (C-ABI contract).
        let Some(addr) = (unsafe { env_fn_addr(env) }) else {
            return res;
        };
        // SAFETY: addr is the callable stored by the closure lowering.
        let f: EnumFn = unsafe { std::mem::transmute(addr) };
        // SAFETY: `f` is the callback `addr` names, compiled for this shape, whose environment
        // `env` is live for the call (C-ABI contract).
        unsafe { f(env, gos_rt_result_payload(res)) }
    })
}

/// `result::or_else(f, res)` - `f(err_payload) -> Result` when Err,
/// Ok passthrough.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_result_or_else(res: i128, env: *const u8) -> i128 {
    ffi_entry_passthrough!(0i128, {
        if gos_rt_result_disc(res) == 0 {
            return res;
        }
        // SAFETY: `env` is this shim's argument, as `env_fn_addr` requires (C-ABI contract).
        let Some(addr) = (unsafe { env_fn_addr(env) }) else {
            return res;
        };
        // SAFETY: addr is the callable stored by the closure lowering.
        let f: EnumFn = unsafe { std::mem::transmute(addr) };
        // SAFETY: `f` is the callback `addr` names, compiled for this shape, whose environment
        // `env` is live for the call (C-ABI contract).
        unsafe { f(env, gos_rt_result_payload(res)) }
    })
}

/// `result::ok(res) -> Option<T>` - Ok payload as Some, Err as None.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_to_opt_ok(res: i128) -> i128 {
    if gos_rt_result_disc(res) == 0 {
        some_of(gos_rt_result_payload(res))
    } else {
        NONE
    }
}

/// `result::err(res) -> Option<E>` - Err payload as Some, Ok as None.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_to_opt_err(res: i128) -> i128 {
    if gos_rt_result_disc(res) != 0 {
        some_of(gos_rt_result_payload(res))
    } else {
        NONE
    }
}

// ----------------------------------------------------------------------
// option

/// `option::and_then(f, opt)` - `f(payload) -> Option` when Some,
/// None passthrough.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_option_and_then(opt: i128, env: *const u8) -> i128 {
    ffi_entry_passthrough!(NONE, {
        if gos_rt_result_disc(opt) != 0 {
            return NONE;
        }
        // SAFETY: `env` is this shim's argument, as `env_fn_addr` requires (C-ABI contract).
        let Some(addr) = (unsafe { env_fn_addr(env) }) else {
            return NONE;
        };
        // SAFETY: addr is the callable stored by the closure lowering.
        let f: EnumFn = unsafe { std::mem::transmute(addr) };
        // SAFETY: `f` is the callback `addr` names, compiled for this shape, whose environment
        // `env` is live for the call (C-ABI contract).
        unsafe { f(env, gos_rt_result_payload(opt)) }
    })
}

/// `option::filter(p, opt)` - keeps Some(x) only when `p(x)` holds.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_option_filter(opt: i128, env: *const u8) -> i128 {
    ffi_entry_passthrough!(NONE, {
        if gos_rt_result_disc(opt) != 0 {
            return NONE;
        }
        // SAFETY: `env` is this shim's argument, as `env_fn_addr` requires (C-ABI contract).
        let Some(addr) = (unsafe { env_fn_addr(env) }) else {
            return NONE;
        };
        let payload = gos_rt_result_payload(opt);
        // SAFETY: addr is the callable stored by the closure lowering.
        let p: PredFn = unsafe { std::mem::transmute(addr) };
        // SAFETY: `p` is the predicate `addr` names, compiled for this shape, whose environment
        // `env` is live for the call (C-ABI contract).
        if unsafe { p(env, payload) } {
            some_of(payload)
        } else {
            NONE
        }
    })
}

/// `option::or(alt, opt)` - data-last: returns `opt` when Some,
/// otherwise `alt`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_option_or(alt: i128, opt: i128) -> i128 {
    if gos_rt_result_disc(opt) == 0 {
        opt
    } else {
        alt
    }
}

/// `option::or_else(f, opt)` - returns `opt` when Some, otherwise
/// `f() -> Option`.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_option_or_else(opt: i128, env: *const u8) -> i128 {
    ffi_entry_passthrough!(NONE, {
        if gos_rt_result_disc(opt) == 0 {
            return opt;
        }
        // SAFETY: `env` is this shim's argument, as `env_fn_addr` requires (C-ABI contract).
        let Some(addr) = (unsafe { env_fn_addr(env) }) else {
            return NONE;
        };
        // SAFETY: addr is the callable stored by the closure lowering.
        let f: ThunkEnumFn = unsafe { std::mem::transmute(addr) };
        // SAFETY: `f` is the callback `addr` names, compiled for this shape, whose environment
        // `env` is live for the call (C-ABI contract).
        unsafe { f(env) }
    })
}

/// `option::default_with(f, opt)` - Some payload, or `f()`.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_option_default_with(opt: i128, env: *const u8) -> i64 {
    ffi_entry_passthrough!(0, {
        if gos_rt_result_disc(opt) == 0 {
            return gos_rt_result_payload(opt);
        }
        // SAFETY: `env` is this shim's argument, as `env_fn_addr` requires (C-ABI contract).
        let Some(addr) = (unsafe { env_fn_addr(env) }) else {
            return 0;
        };
        // SAFETY: addr is the callable stored by the closure lowering.
        let f: ThunkValFn = unsafe { std::mem::transmute(addr) };
        // SAFETY: `f` is the callback `addr` names, compiled for this shape, whose environment
        // `env` is live for the call (C-ABI contract).
        unsafe { f(env) }
    })
}

/// `opt.ok_or_else(f)` - Some payload becomes Ok (same packed repr),
/// None becomes `Err(f())`.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_result_ok_or_else(opt: i128, env: *const u8) -> i128 {
    ffi_entry_passthrough!(NONE, {
        if gos_rt_result_disc(opt) == 0 {
            return opt;
        }
        // SAFETY: `env` is this shim's argument, as `env_fn_addr` requires (C-ABI contract).
        let Some(addr) = (unsafe { env_fn_addr(env) }) else {
            return opt;
        };
        // SAFETY: addr is the callable stored by the closure lowering.
        let f: ThunkValFn = unsafe { std::mem::transmute(addr) };
        // SAFETY: `f` is the callback `addr` names, compiled for this shape, whose environment
        // `env` is live for the call (C-ABI contract).
        gos_rt_result_new(1, unsafe { f(env) })
    })
}

/// `option::zip(first, second)` - `Some((a, b))` when both are Some.
/// Matches the interp's argument order: the data-last pipe passes the
/// piped option as `second`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_option_zip(first: i128, second: i128) -> i128 {
    ffi_entry!(NONE, {
        if gos_rt_result_disc(first) != 0 || gos_rt_result_disc(second) != 0 {
            return NONE;
        }
        let pair = alloc_pair(gos_rt_result_payload(first), gos_rt_result_payload(second));
        if pair.is_null() {
            return NONE;
        }
        some_of(pair as i64)
    })
}

/// `option::flatten(opt)` - `Some(inner)` loads the nested 16-byte
/// Option from the payload word; None stays None.
///
/// # Safety
///
/// A `Some` payload of `opt` addresses a live 16-byte enum copy.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_option_flatten(opt: i128) -> i128 {
    if gos_rt_result_disc(opt) != 0 {
        return NONE;
    }
    // SAFETY: a `Some` payload is the enum copy this function's contract names.
    unsafe { load_enum_at(gos_rt_result_payload(opt)) }
}

/// `option::iter(opt) -> [T]` - zero- or one-element Vec.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_option_iter(opt: i128) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        if gos_rt_result_disc(opt) == 0 {
            vec_from(&[gos_rt_result_payload(opt)])
        } else {
            vec_from(&[])
        }
    })
}

// ----------------------------------------------------------------------
// iter - closure-taking entries over Vec<i64>-shaped sequences.

/// `iter::unzip(pairs) -> ([i64], [i64])` - split a `Vec<(i64, i64)>`
/// into the vec of first components and the vec of seconds. Each
/// input element is a 16-byte 2-slot tuple; the result is the
/// by-value `(Vec, Vec)` pair returned as a 16-byte heap blob.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_iter_unzip_i64(v: *const GosVec) -> *mut u8 {
    ffi_entry!(std::ptr::null_mut(), {
        let mut a = Vec::new();
        let mut b = Vec::new();
        // SAFETY: `v` is this shim's argument, null or a live `Vec` (C-ABI contract).
        let pairs = unsafe { crate::c_abi::vec::VecView::of(v) }.filter(|vec| vec.width() >= 16);
        if let Some(pairs) = pairs {
            for i in 0..pairs.len() {
                let elem = pairs.elem(i);
                let word = |at: usize| {
                    let mut bytes = [0u8; 8];
                    bytes.copy_from_slice(&elem[at..at + 8]);
                    i64::from_ne_bytes(bytes)
                };
                a.push(word(0));
                b.push(word(8));
            }
        }
        alloc_pair(vec_from(&a) as i64, vec_from(&b) as i64)
    })
}

/// `result.map_err(closure)`. If Err, calls closure and rebuilds.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_result_map_err(result: i128, closure: *const u8) -> i128 {
    ffi_entry_passthrough!(0i128, {
        if gos_rt_result_disc(result) != 1 || closure.is_null() {
            return result;
        }
        // SAFETY: `closure` is a heap blob whose first word is the
        // lifted function's address (codegen invariant).
        let fn_addr = unsafe { *closure.cast::<i64>() };
        if fn_addr == 0 {
            return result;
        }
        // The lifted function address is stored as a 64-bit word but a
        // function pointer is target-pointer-width (32-bit on wasm32),
        // so narrow through `usize` before reinterpreting. Identity on
        // 64-bit native.
        // SAFETY: a non-zero entry word is the address of the compiled closure, of the signature
        // this shim calls it through (C-ABI contract).
        let f: extern "C" fn(i64, i64) -> i64 =
            unsafe { std::mem::transmute(crate::c_abi::code_address(fn_addr as usize)) };
        let new_payload = f(closure as i64, gos_rt_result_payload(result));
        gos_rt_result_new(1, new_payload)
    })
}

/// `result.map(closure)` for **capturing** closures whose lifted
/// function follows the env-first ABI `extern "C" fn(env, payload)
/// -> i64`. Non-capturing closures must dispatch through
/// [`gos_rt_result_map_bare`] instead - they have no env slot, so
/// passing one would shadow the payload arg and the closure would
/// transform the env pointer instead of the payload (the askq
/// round-2 corruption pre-fix).
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_result_map(result: i128, closure: *const u8) -> i128 {
    ffi_entry_passthrough!(0i128, {
        if gos_rt_result_disc(result) != 0 || closure.is_null() {
            return result;
        }
        // SAFETY: `closure` is this shim's `u8` argument, non-null (checked above), live for the
        // call (C-ABI contract).
        let fn_addr = unsafe { *closure.cast::<i64>() };
        if fn_addr == 0 {
            return result;
        }
        // The lifted function address is stored as a 64-bit word but a
        // function pointer is target-pointer-width (32-bit on wasm32),
        // so narrow through `usize` before reinterpreting. Identity on
        // 64-bit native.
        // SAFETY: a non-zero entry word is the address of the compiled closure, of the signature
        // this shim calls it through (C-ABI contract).
        let f: extern "C" fn(i64, i64) -> i64 =
            unsafe { std::mem::transmute(crate::c_abi::code_address(fn_addr as usize)) };
        let new_payload = f(closure as i64, gos_rt_result_payload(result));
        gos_rt_result_new(0, new_payload)
    })
}

/// `result::default_with(closure, result)` - returns the `Ok` value
/// unchanged, or calls `closure` on the `Err` payload and returns its
/// result. The returned `i64` is the unwrapped `T` (a scalar value or
/// a pointer, depending on `T`). Mirrors `gos_rt_result_map`'s closure
/// invocation convention.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_result_default_with(
    result: i128,
    closure: *const u8,
) -> i64 {
    ffi_entry_passthrough!(0, {
        if gos_rt_result_disc(result) == 0 {
            return gos_rt_result_payload(result);
        }
        if closure.is_null() {
            return 0;
        }
        // SAFETY: `closure` is this shim's `u8` argument, non-null (checked above), live for the
        // call (C-ABI contract).
        let fn_addr = unsafe { *closure.cast::<i64>() };
        if fn_addr == 0 {
            return 0;
        }
        // The lifted function address is stored as a 64-bit word but a
        // function pointer is target-pointer-width (32-bit on wasm32),
        // so narrow through `usize` before reinterpreting. Identity on
        // 64-bit native.
        // SAFETY: a non-zero entry word is the address of the compiled closure, of the signature
        // this shim calls it through (C-ABI contract).
        let f: extern "C" fn(i64, i64) -> i64 =
            unsafe { std::mem::transmute(crate::c_abi::code_address(fn_addr as usize)) };
        f(closure as i64, gos_rt_result_payload(result))
    })
}

/// `result::default(fallback, result)` - returns the `Ok` payload,
/// or `fallback` when the Result is `Err`. The returned `i64` is the
/// unwrapped `T` (a scalar value or a pointer, depending on `T`).
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_default(fallback: i64, result: i128) -> i64 {
    ffi_entry!(0, {
        if gos_rt_result_disc(result) == 0 {
            gos_rt_result_payload(result)
        } else {
            fallback
        }
    })
}

/// `result::default(fallback, result)` specialised for f64 payloads:
/// the stored payload word is reinterpreted as its IEEE-754 bit
/// pattern, and the fallback rides the float register directly.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn gos_rt_result_default_f64(fallback: f64, result: i128) -> f64 {
    ffi_entry_passthrough!(0.0, {
        if gos_rt_result_disc(result) == 0 {
            f64::from_bits(gos_rt_result_payload(result) as u64)
        } else {
            fallback
        }
    })
}

/// `result.map(closure)` for **non-capturing** closures whose
/// lifted function follows the bare ABI `extern "C" fn(payload) ->
/// i64` (no env slot - this is what `gossamer-hir::lift_closed`
/// produces). The MIR call-site dispatch picks this entry point
/// when the closure arg has a recorded `local_fn_name` (i.e. is
/// a direct path to a lifted function rather than a heap-allocated
/// env+code blob).
///
/// # Safety
///
/// A non-zero `fn_addr` is the entry address of a compiled `extern "C" fn(i64)
/// -> i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_result_map_bare(result: i128, fn_addr: i64) -> i128 {
    ffi_entry_passthrough!(0i128, {
        if gos_rt_result_disc(result) != 0 || fn_addr == 0 {
            return result;
        }
        // SAFETY: this shim's contract makes a non-zero `fn_addr` a compiled `extern "C" fn(i64)
        // -> i64`.
        let f: extern "C" fn(i64) -> i64 = unsafe { std::mem::transmute(fn_addr as *const ()) };
        let new_payload = f(gos_rt_result_payload(result));
        gos_rt_result_new(0, new_payload)
    })
}

/// `result.map_err(closure)` for **non-capturing** closures.
///
/// # Safety
///
/// A non-zero `fn_addr` is the entry address of a compiled `extern "C" fn(i64)
/// -> i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_result_map_err_bare(result: i128, fn_addr: i64) -> i128 {
    ffi_entry_passthrough!(0i128, {
        if gos_rt_result_disc(result) == 0 || fn_addr == 0 {
            return result;
        }
        // SAFETY: this shim's contract makes a non-zero `fn_addr` a compiled `extern "C" fn(i64)
        // -> i64`.
        let f: extern "C" fn(i64) -> i64 = unsafe { std::mem::transmute(fn_addr as *const ()) };
        let new_payload = f(gos_rt_result_payload(result));
        gos_rt_result_new(1, new_payload)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::c_abi::vec::gos_rt_vec_len;

    /// Heap-copies a 16-byte enum value and returns its address -
    /// the payload representation of a nested Result/Option.
    fn store_enum(value: i128) -> i64 {
        let p = crate::c_abi::gos_rt_gc_alloc(16);
        assert!(!p.is_null());
        // SAFETY: `p` is a fresh 16-byte allocation.
        unsafe {
            let slots = p.cast::<i64>();
            *slots = value as u64 as i64;
            *slots.add(1) = ((value as u128) >> 64) as u64 as i64;
        }
        p as i64
    }

    extern "C" fn double_cb(_env: *const u8, x: i64) -> i64 {
        x * 2
    }
    extern "C" fn even_cb(_env: *const u8, x: i64) -> bool {
        x % 2 == 0
    }
    extern "C" fn sub_cb(_env: *const u8, a: i64, b: i64) -> i64 {
        a - b
    }
    extern "C" fn opt_pos_cb(_env: *const u8, x: i64) -> i128 {
        if x > 0 {
            gos_rt_result_new(0, x * 10)
        } else {
            NONE
        }
    }

    fn env_for(addr: usize) -> Vec<usize> {
        vec![addr, 0]
    }

    /// The word-slot elements of `v`, for asserting on a shim's result.
    fn elems(v: *mut GosVec) -> Vec<i64> {
        if v.is_null() {
            return Vec::new();
        }
        // SAFETY: `v` is a live GosVec a shim under test just returned; the
        // per-index read is bounds-checked by the accessor.
        let len = unsafe { gos_rt_vec_len(v) };
        (0..len)
            // SAFETY: every pointer argument is a value this test built above and still holds
            // live; a null one is accepted by the callee.
            .map(|i| unsafe { crate::c_abi::gos_rt_vec_get_i64(v, i) })
            .collect()
    }

    #[test]
    fn option_filter_keeps_matching_payload() {
        let env = env_for(even_cb as *const () as usize);
        let some4 = gos_rt_result_new(0, 4);
        let some3 = gos_rt_result_new(0, 3);
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let kept = unsafe { gos_rt_option_filter(some4, env.as_ptr().cast()) };
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let dropped = unsafe { gos_rt_option_filter(some3, env.as_ptr().cast()) };
        assert_eq!(gos_rt_result_disc(kept), 0);
        assert_eq!(gos_rt_result_payload(kept), 4);
        assert_eq!(gos_rt_result_disc(dropped), 1);
    }

    #[test]
    fn option_zip_pairs_payloads_in_arg_order() {
        let a = gos_rt_result_new(0, 1);
        let b = gos_rt_result_new(0, 2);
        let zipped = gos_rt_option_zip(b, a);
        assert_eq!(gos_rt_result_disc(zipped), 0);
        let pair = gos_rt_result_payload(zipped) as usize as *const i64;
        // SAFETY: zip allocated a live 2-slot pair.
        unsafe {
            assert_eq!(*pair, 2);
            assert_eq!(*pair.add(1), 1);
        }
    }

    #[test]
    fn option_flatten_loads_nested_enum() {
        let inner = gos_rt_result_new(0, 42);
        let outer = gos_rt_result_new(0, store_enum(inner));
        // SAFETY: `outer`'s payload is the enum copy `store_enum` made.
        let flat = unsafe { gos_rt_option_flatten(outer) };
        assert_eq!(gos_rt_result_disc(flat), 0);
        assert_eq!(gos_rt_result_payload(flat), 42);
        // SAFETY: `None` carries no payload to read.
        assert_eq!(
            // SAFETY: every pointer argument is a value this test built above and still holds
            // live; a null one is accepted by the callee.
            gos_rt_result_disc(unsafe { gos_rt_option_flatten(NONE) }),
            1
        );
    }

    #[test]
    fn result_and_then_chains_ok_payload() {
        let env = env_for(opt_pos_cb as *const () as usize);
        let ok = gos_rt_result_new(0, 5);
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let out = unsafe { gos_rt_result_and_then(ok, env.as_ptr().cast()) };
        assert_eq!(gos_rt_result_disc(out), 0);
        assert_eq!(gos_rt_result_payload(out), 50);
        let err = gos_rt_result_new(1, 9);
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let passthrough = unsafe { gos_rt_result_and_then(err, env.as_ptr().cast()) };
        assert_eq!(gos_rt_result_disc(passthrough), 1);
        assert_eq!(gos_rt_result_payload(passthrough), 9);
    }

    #[test]
    fn iter_partition_splits_matching_first() {
        let env = env_for(even_cb as *const () as usize);
        let v = vec_from(&[1, 2, 3, 4]);
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let pair = unsafe { crate::c_abi::gos_rt_iter_partition_i64(env.as_ptr().cast(), v) };
        // SAFETY: partition returns a live 2-slot pair of vec pointers.
        let (yes, no) = unsafe {
            let slots = pair.cast::<i64>();
            (
                *slots as usize as *mut GosVec,
                *slots.add(1) as usize as *mut GosVec,
            )
        };
        assert_eq!(elems(yes), vec![2, 4]);
        assert_eq!(elems(no), vec![1, 3]);
    }

    #[test]
    fn iter_scan_reduce_sorted_follow_interp_semantics() {
        let cmp_env = env_for(sub_cb as *const () as usize);
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let scanned = unsafe {
            crate::c_abi::gos_rt_iter_scan_i64(0, cmp_env.as_ptr().cast(), vec_from(&[1, 2, 3]))
        };
        assert_eq!(elems(scanned), vec![-1, -3, -6]);
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let reduced = unsafe {
            crate::c_abi::gos_rt_iter_reduce_i64(cmp_env.as_ptr().cast(), vec_from(&[10, 1, 2]))
        };
        assert_eq!(gos_rt_result_payload(reduced), 7);
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let sorted = unsafe {
            crate::c_abi::gos_rt_iter_sorted_by_i64(cmp_env.as_ptr().cast(), vec_from(&[3, 1, 2]))
        };
        assert_eq!(elems(sorted), vec![1, 2, 3]);
        let map_env = env_for(double_cb as *const () as usize);
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        let keyed = unsafe {
            crate::c_abi::gos_rt_iter_sorted_by_key_i64(
                map_env.as_ptr().cast(),
                vec_from(&[3, 1, 2]),
                0,
            )
        };
        assert_eq!(elems(keyed), vec![1, 2, 3]);
    }
}
