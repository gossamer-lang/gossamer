#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::doc_markdown)]

//! C-ABI shims for `std::math::big::*`. Big integers are carried as
//! decimal `String`s across the Gossamer boundary (matching the
//! interp, which stores every `big::Int` / `big::Uint` as a decimal
//! `Value::String`), so each shim parses its decimal-string operands,
//! computes with `num-bigint`, and returns a freshly-allocated decimal
//! (or hex) c-string. Result-returning entries pack a `*mut GosResult`
//! (disc 0 Ok, disc 1 Err); Option-returning entries pack disc 0 Some,
//! disc 1 None.

use std::os::raw::c_char;

use num_bigint::{BigInt, BigUint};
use num_integer::Integer;
use num_traits::{One, Pow, Signed, ToPrimitive, Zero};

use super::string::alloc_cstring;
use super::vec::gos_rt_result_new;

fn err_result(msg: &str) -> i128 {
    let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
    gos_rt_result_new(1, err as i64)
}

/// # Safety
///
/// `s` is null or a live string body.
unsafe fn cstr<'a>(s: *const c_char) -> &'a str {
    if s.is_null() {
        return "0";
    }
    // SAFETY: callers pass a Gossamer `String`, read through its length header.
    let bytes = unsafe { crate::c_abi::gos_str_arg_bytes(s) };
    std::str::from_utf8(bytes).unwrap_or("0")
}

/// # Safety
///
/// `s` is null or a live string body.
unsafe fn int(s: *const c_char) -> BigInt {
    // SAFETY: this function's contract covers `s`, as `cstr` requires.
    unsafe { cstr(s) }
        .parse::<BigInt>()
        .unwrap_or_else(|_| BigInt::zero())
}

/// # Safety
///
/// `s` is null or a live string body.
unsafe fn uint(s: *const c_char) -> BigUint {
    // SAFETY: this function's contract covers `s`, as `cstr` requires.
    unsafe { cstr(s) }
        .parse::<BigUint>()
        .unwrap_or_else(|_| BigUint::zero())
}

fn dec(value: &impl std::fmt::Display) -> *mut c_char {
    alloc_cstring(value.to_string().as_bytes())
}

fn ok_str(value: &impl std::fmt::Display) -> i128 {
    let p = alloc_cstring(value.to_string().as_bytes());
    gos_rt_result_new(0, p as i64)
}

fn some_i64(n: i64) -> i128 {
    gos_rt_result_new(0, n)
}

fn none_opt() -> i128 {
    gos_rt_result_new(1, 0)
}

// --- factorial -------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_math_big_factorial(n: i64) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let mut result = BigInt::one();
        let mut i: i64 = 2;
        while i <= n {
            result *= i;
            i += 1;
        }
        dec(&result)
    })
}

// --- signed Int ------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_math_big_int_from_i64(n: i64) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), { dec(&BigInt::from(n)) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_big_int_from_str(s: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        // SAFETY: `s` is this shim's argument, as `cstr` requires (C-ABI contract).
        match unsafe { cstr(s) }.parse::<BigInt>() {
            Ok(n) => ok_str(&n),
            Err(e) => err_result(&format!("big::Int: {e}")),
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_big_int_to_str(s: *const c_char) -> *mut c_char {
    // SAFETY: `s` is this shim's argument, as `int` requires (C-ABI contract).
    ffi_entry!(std::ptr::null_mut(), { dec(&unsafe { int(s) }) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_big_int_to_hex(s: *const c_char) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        // SAFETY: `s` is this shim's argument, as `int` requires (C-ABI contract).
        let n = unsafe { int(s) };
        alloc_cstring(format!("{n:x}").as_bytes())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_big_int_to_i64(s: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        // SAFETY: `s` is this shim's argument, as `int` requires (C-ABI contract).
        match unsafe { int(s) }.to_i64() {
            Some(n) => some_i64(n),
            None => none_opt(),
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_big_int_is_zero(s: *const c_char) -> i32 {
    // SAFETY: `s` is this shim's argument, as `int` requires (C-ABI contract).
    ffi_entry!(0, { i32::from(unsafe { int(s) }.is_zero()) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_big_int_is_positive(s: *const c_char) -> i32 {
    // SAFETY: `s` is this shim's argument, as `int` requires (C-ABI contract).
    ffi_entry!(0, { i32::from(unsafe { int(s) } > BigInt::zero()) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_big_int_is_negative(s: *const c_char) -> i32 {
    // SAFETY: `s` is this shim's argument, as `int` requires (C-ABI contract).
    ffi_entry!(0, { i32::from(unsafe { int(s) } < BigInt::zero()) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_big_int_add(
    a: *const c_char,
    b: *const c_char,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        // SAFETY: `a` is this shim's argument, null or a live string body (C-ABI contract),
        // which `int` accepts.
        let a = unsafe { int(a) };
        // SAFETY: `b` is this shim's argument, null or a live string body (C-ABI contract),
        // which `int` accepts.
        let b = unsafe { int(b) };
        dec(&(a + b))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_big_int_sub(
    a: *const c_char,
    b: *const c_char,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        // SAFETY: `a` is this shim's argument, null or a live string body (C-ABI contract),
        // which `int` accepts.
        let a = unsafe { int(a) };
        // SAFETY: `b` is this shim's argument, null or a live string body (C-ABI contract),
        // which `int` accepts.
        let b = unsafe { int(b) };
        dec(&(a - b))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_big_int_mul(
    a: *const c_char,
    b: *const c_char,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        // SAFETY: `a` is this shim's argument, null or a live string body (C-ABI contract),
        // which `int` accepts.
        let a = unsafe { int(a) };
        // SAFETY: `b` is this shim's argument, null or a live string body (C-ABI contract),
        // which `int` accepts.
        let b = unsafe { int(b) };
        dec(&(a * b))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_big_int_div(a: *const c_char, b: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        // SAFETY: `b` is this shim's argument, as `int` requires (C-ABI contract).
        let d = unsafe { int(b) };
        if d.is_zero() {
            err_result("big::Int: division by zero")
        } else {
            // SAFETY: `a` is this shim's argument, as `int` requires (C-ABI contract).
            ok_str(&(unsafe { int(a) } / d))
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_big_int_rem(a: *const c_char, b: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        // SAFETY: `b` is this shim's argument, as `int` requires (C-ABI contract).
        let d = unsafe { int(b) };
        if d.is_zero() {
            err_result("big::Int: division by zero")
        } else {
            // SAFETY: `a` is this shim's argument, as `int` requires (C-ABI contract).
            ok_str(&(unsafe { int(a) } % d))
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_big_int_pow(s: *const c_char, exp: i64) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let e = u32::try_from(exp.max(0)).unwrap_or(0);
        // SAFETY: `s` is this shim's argument, as `int` requires (C-ABI contract).
        dec(&unsafe { int(s) }.pow(e))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_big_int_abs(s: *const c_char) -> *mut c_char {
    // SAFETY: `s` is this shim's argument, as `int` requires (C-ABI contract).
    ffi_entry!(std::ptr::null_mut(), { dec(&unsafe { int(s) }.abs()) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_big_int_neg(s: *const c_char) -> *mut c_char {
    // SAFETY: `s` is this shim's argument, as `int` requires (C-ABI contract).
    ffi_entry!(std::ptr::null_mut(), { dec(&(-unsafe { int(s) })) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_big_int_gcd(
    a: *const c_char,
    b: *const c_char,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        // SAFETY: `a` is this shim's argument, null or a live string body (C-ABI contract),
        // which `int` accepts.
        let a = unsafe { int(a) };
        // SAFETY: `b` is this shim's argument, null or a live string body (C-ABI contract),
        // which `int` accepts.
        let b = unsafe { int(b) };
        dec(&a.gcd(&b))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_big_int_lcm(
    a: *const c_char,
    b: *const c_char,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        // SAFETY: `a` is this shim's argument, null or a live string body (C-ABI contract),
        // which `int` accepts.
        let a = unsafe { int(a) };
        // SAFETY: `b` is this shim's argument, null or a live string body (C-ABI contract),
        // which `int` accepts.
        let b = unsafe { int(b) };
        dec(&a.lcm(&b))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_big_int_cmp(a: *const c_char, b: *const c_char) -> i64 {
    ffi_entry!(0, {
        // SAFETY: `a` is this shim's argument, as `int` requires (C-ABI contract). `b` is this
        // shim's argument, as `int` requires (C-ABI contract).
        match unsafe { int(a) }.cmp(&unsafe { int(b) }) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        }
    })
}

// --- unsigned Uint ---------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_math_big_uint_from_u64(n: i64) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        dec(&BigUint::from(n.max(0) as u64))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_big_uint_from_str(s: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        // SAFETY: `s` is this shim's argument, as `cstr` requires (C-ABI contract).
        match unsafe { cstr(s) }.parse::<BigUint>() {
            Ok(n) => ok_str(&n),
            Err(e) => err_result(&format!("big::Uint: {e}")),
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_big_uint_to_str(s: *const c_char) -> *mut c_char {
    // SAFETY: `s` is this shim's argument, as `uint` requires (C-ABI contract).
    ffi_entry!(std::ptr::null_mut(), { dec(&unsafe { uint(s) }) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_big_uint_to_hex(s: *const c_char) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        // SAFETY: `s` is this shim's argument, as `uint` requires (C-ABI contract).
        let n = unsafe { uint(s) };
        alloc_cstring(format!("{n:x}").as_bytes())
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_big_uint_to_u64(s: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        // SAFETY: `s` is this shim's argument, as `uint` requires (C-ABI contract).
        match unsafe { uint(s) }.to_u64() {
            Some(n) => some_i64(n as i64),
            None => none_opt(),
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_big_uint_is_zero(s: *const c_char) -> i32 {
    // SAFETY: `s` is this shim's argument, as `uint` requires (C-ABI contract).
    ffi_entry!(0, { i32::from(unsafe { uint(s) }.is_zero()) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_big_uint_add(
    a: *const c_char,
    b: *const c_char,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        // SAFETY: `a` is this shim's argument, null or a live string body (C-ABI contract),
        // which `uint` accepts.
        let a = unsafe { uint(a) };
        // SAFETY: `b` is this shim's argument, null or a live string body (C-ABI contract),
        // which `uint` accepts.
        let b = unsafe { uint(b) };
        dec(&(a + b))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_big_uint_mul(
    a: *const c_char,
    b: *const c_char,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        // SAFETY: `a` is this shim's argument, null or a live string body (C-ABI contract),
        // which `uint` accepts.
        let a = unsafe { uint(a) };
        // SAFETY: `b` is this shim's argument, null or a live string body (C-ABI contract),
        // which `uint` accepts.
        let b = unsafe { uint(b) };
        dec(&(a * b))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_big_uint_pow(s: *const c_char, exp: i64) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let e = u32::try_from(exp.max(0)).unwrap_or(0);
        // SAFETY: `s` is this shim's argument, as `uint` requires (C-ABI contract).
        dec(&unsafe { uint(s) }.pow(e))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_big_uint_pow_mod(
    base: *const c_char,
    exp: *const c_char,
    modulus: *const c_char,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        // SAFETY: `base` is this shim's argument, as `uint` requires (C-ABI contract). `exp` is
        // this shim's argument, as `uint` requires (C-ABI contract). `modulus` is this shim's
        // argument, as `uint` requires (C-ABI contract).
        dec(&unsafe { uint(base) }.modpow(&unsafe { uint(exp) }, &unsafe { uint(modulus) }))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_big_uint_bit_len(s: *const c_char) -> i64 {
    // SAFETY: `s` is this shim's argument, as `uint` requires (C-ABI contract).
    ffi_entry!(0, { unsafe { uint(s) }.bits() as i64 })
}
