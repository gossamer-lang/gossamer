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

// ---------------------------------------------------------------
// Math helpers
// ---------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_sqrt(x: f64) -> f64 {
    ffi_entry!(f64::NAN, { x.sqrt() })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_pow(x: f64, y: f64) -> f64 {
    ffi_entry!(f64::NAN, { x.powf(y) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_sin(x: f64) -> f64 {
    ffi_entry!(f64::NAN, { x.sin() })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_cos(x: f64) -> f64 {
    ffi_entry!(f64::NAN, { x.cos() })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_log(x: f64) -> f64 {
    ffi_entry!(f64::NAN, { x.ln() })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_exp(x: f64) -> f64 {
    ffi_entry!(f64::NAN, { x.exp() })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_abs(x: f64) -> f64 {
    ffi_entry!(f64::NAN, { x.abs() })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_floor(x: f64) -> f64 {
    ffi_entry!(f64::NAN, { x.floor() })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_ceil(x: f64) -> f64 {
    ffi_entry!(f64::NAN, { x.ceil() })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_tan(x: f64) -> f64 {
    ffi_entry!(f64::NAN, { x.tan() })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_asin(x: f64) -> f64 {
    ffi_entry!(f64::NAN, { x.asin() })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_acos(x: f64) -> f64 {
    ffi_entry!(f64::NAN, { x.acos() })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_atan(x: f64) -> f64 {
    ffi_entry!(f64::NAN, { x.atan() })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_atan2(y: f64, x: f64) -> f64 {
    ffi_entry!(f64::NAN, { y.atan2(x) })
}

/// `math::log(x, base)`: the logarithm of `x` in `base`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_log_base(x: f64, base: f64) -> f64 {
    ffi_entry!(f64::NAN, { x.ln() / base.ln() })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_sinh(x: f64) -> f64 {
    ffi_entry!(f64::NAN, { x.sinh() })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_cosh(x: f64) -> f64 {
    ffi_entry!(f64::NAN, { x.cosh() })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_tanh(x: f64) -> f64 {
    ffi_entry!(f64::NAN, { x.tanh() })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_log2(x: f64) -> f64 {
    ffi_entry!(f64::NAN, { x.log2() })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_log10(x: f64) -> f64 {
    ffi_entry!(f64::NAN, { x.log10() })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_cbrt(x: f64) -> f64 {
    ffi_entry!(f64::NAN, { x.cbrt() })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_round(x: f64) -> f64 {
    ffi_entry!(f64::NAN, { x.round() })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_exp2(x: f64) -> f64 {
    ffi_entry!(f64::NAN, { x.exp2() })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_fmod(x: f64, y: f64) -> f64 {
    ffi_entry!(f64::NAN, { x % y })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_hypot(x: f64, y: f64) -> f64 {
    ffi_entry!(f64::NAN, { x.hypot(y) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_copysign(x: f64, y: f64) -> f64 {
    ffi_entry!(f64::NAN, { x.copysign(y) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_dim(x: f64, y: f64) -> f64 {
    ffi_entry!(f64::NAN, { (x - y).max(0.0) })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_trunc(x: f64) -> f64 {
    ffi_entry!(f64::NAN, { x.trunc() })
}

/// Integer path for `math::abs(x)`, saturating at `i64::MAX`
/// for `i64::MIN`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_math_abs_i64(x: i64) -> i64 {
    x.saturating_abs()
}

fn normalize_wrapping_int(value: u64, bits: i64, signed: i64) -> i64 {
    let bits = bits.clamp(1, 64) as u32;
    let masked = if bits == 64 {
        value
    } else {
        value & ((1_u64 << bits) - 1)
    };
    if signed != 0 && bits < 64 && masked & (1_u64 << (bits - 1)) != 0 {
        (masked | (!0_u64 << bits)) as i64
    } else {
        masked as i64
    }
}

/// Profile-independent, Rust-compatible wrapping integer addition.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_int_wrapping_add(x: i64, y: i64, bits: i64, signed: i64) -> i64 {
    normalize_wrapping_int((x as u64).wrapping_add(y as u64), bits, signed)
}

/// Profile-independent, Rust-compatible wrapping integer multiplication.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_int_wrapping_mul(x: i64, y: i64, bits: i64, signed: i64) -> i64 {
    normalize_wrapping_int((x as u64).wrapping_mul(y as u64), bits, signed)
}

/// `math::is_nan(x)` - 1 when `x` is NaN, else 0.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_math_is_nan(x: f64) -> i32 {
    i32::from(x.is_nan())
}

/// `math::is_inf(x, sign)` - `sign > 0` checks +∞, `sign < 0` checks
/// −∞, `sign == 0` checks either. Mirrors `gossamer_std::math::is_inf`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_math_is_inf(x: f64, sign: i64) -> i32 {
    let hit = match sign.cmp(&0) {
        std::cmp::Ordering::Greater => x == f64::INFINITY,
        std::cmp::Ordering::Less => x == f64::NEG_INFINITY,
        std::cmp::Ordering::Equal => x.is_infinite(),
    };
    i32::from(hit)
}

/// `math::nan()` - the IEEE 754 not-a-number value.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_math_nan() -> f64 {
    f64::NAN
}

/// `math::inf(sign)` - positive infinity when `sign >= 0`, else
/// negative infinity (mirrors `gossamer_std::math::inf`).
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_math_inf(sign: i64) -> f64 {
    if sign >= 0 {
        f64::INFINITY
    } else {
        f64::NEG_INFINITY
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_time_now_ms() -> i64 {
    ffi_entry!(-1, { crate::clock::wall_ms() })
}

/// `time::freeze(ms)` - pin the wall clock at `ms` since the epoch.
///
/// A test over anything time-dependent - a token's expiry, a rate-limit
/// window, a cache's TTL - reads a clock it controls instead of waiting
/// for a real one. The monotonic clock is untouched, and so is `sleep`:
/// this pins what the program is told the time is, not how long anything
/// takes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_time_freeze(ms: i64) {
    ffi_entry!((), { crate::clock::freeze(ms) });
}

/// `time::advance(ms)` - move a frozen clock forward. Freezes it at the
/// current reading first when it is not already frozen, so a test need not
/// name a starting instant it does not care about.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_time_advance(ms: i64) -> i64 {
    ffi_entry!(-1, { crate::clock::advance(ms) })
}

/// `time::unfreeze()` - return to the real wall clock.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_time_unfreeze() {
    ffi_entry!((), { crate::clock::unfreeze() });
}

/// `time::is_frozen() -> bool`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_time_is_frozen() -> i64 {
    ffi_entry!(0, { i64::from(crate::clock::is_frozen()) })
}

// ---------------------------------------------------------------
// math::bits::* - scalar bit primitives over u64 (values cross the
// C-ABI as i64 bit patterns). Mirrors `gossamer_std::math::bits`.
// ---------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_bits_count_ones(x: i64) -> i64 {
    i64::from((x as u64).count_ones())
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_bits_count_zeros(x: i64) -> i64 {
    i64::from((x as u64).count_zeros())
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_bits_leading_zeros(x: i64) -> i64 {
    i64::from((x as u64).leading_zeros())
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_bits_trailing_zeros(x: i64) -> i64 {
    i64::from((x as u64).trailing_zeros())
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_bits_reverse_bits(x: i64) -> i64 {
    (x as u64).reverse_bits() as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_bits_reverse_bytes(x: i64) -> i64 {
    (x as u64).swap_bytes() as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_bits_len(x: i64) -> i64 {
    i64::from(64 - (x as u64).leading_zeros())
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_bits_rotate_left(x: i64, n: i64) -> i64 {
    let shift = n.rem_euclid(64) as u32;
    (x as u64).rotate_left(shift) as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_bits_rotate_right(x: i64, n: i64) -> i64 {
    let shift = n.rem_euclid(64) as u32;
    (x as u64).rotate_right(shift) as i64
}

/// Allocates a GC-tracked 2-slot tuple `(i64, i64)` on the heap and
/// returns it as a raw pointer. The compiled-tier by-value-aggregate
/// ABI memcpys `slots * 8` bytes out of this pointer into the
/// caller's tuple alloca, so the two i64 slots must be contiguous.
fn alloc_pair(a: i64, b: i64) -> *mut u8 {
    let p = crate::c_abi::gos_rt_gc_alloc(16);
    if !p.is_null() {
        let slots = p.cast::<i64>();
        unsafe {
            *slots = a;
            *slots.add(1) = b;
        }
    }
    p
}

/// `math::bits::add(x, y, carry) -> (sum, carry_out)`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_bits_add(x: i64, y: i64, carry: i64) -> *mut u8 {
    let (s1, c1) = (x as u64).overflowing_add(y as u64);
    let (s2, c2) = s1.overflowing_add(carry as u64);
    alloc_pair(s2 as i64, (u64::from(c1) + u64::from(c2)) as i64)
}

/// `math::bits::sub(x, y, borrow) -> (diff, borrow_out)`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_bits_sub(x: i64, y: i64, borrow: i64) -> *mut u8 {
    let (d1, b1) = (x as u64).overflowing_sub(y as u64);
    let (d2, b2) = d1.overflowing_sub(borrow as u64);
    alloc_pair(d2 as i64, (u64::from(b1) + u64::from(b2)) as i64)
}

/// `math::bits::mul(x, y) -> (hi, lo)` (full 128-bit product).
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_bits_mul(x: i64, y: i64) -> *mut u8 {
    let p = u128::from(x as u64) * u128::from(y as u64);
    alloc_pair((p >> 64) as u64 as i64, p as u64 as i64)
}

/// `math::bits::div(hi, lo, y) -> (quotient, remainder)`. Mirrors
/// `gossamer_std::math::bits::div`; a zero divisor or overflowing
/// quotient yields `(0, 0)` instead of aborting the process.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_bits_div(hi: i64, lo: i64, y: i64) -> *mut u8 {
    let y = y as u64;
    if y == 0 {
        return alloc_pair(0, 0);
    }
    let dividend = (u128::from(hi as u64) << 64) | u128::from(lo as u64);
    let q = dividend / u128::from(y);
    if q > u128::from(u64::MAX) {
        return alloc_pair(0, 0);
    }
    alloc_pair(q as u64 as i64, (dividend % u128::from(y)) as u64 as i64)
}

/// `f64::to_bits(x) -> u64`: the value's IEEE-754 binary64 encoding.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_f64_to_bits(x: f64) -> i64 {
    x.to_bits() as i64
}

/// `f64::from_bits(b) -> f64`: the binary64 value `b` encodes.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_f64_from_bits(bits: i64) -> f64 {
    f64::from_bits(bits as u64)
}

/// `f32::to_bits(x) -> u32`: the binary32 encoding of `x` rounded to
/// single precision. Gossamer holds every float in a 64-bit slot, so the
/// rounding is part of the contract rather than a lossy step around it.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_f32_to_bits(x: f64) -> i64 {
    i64::from((x as f32).to_bits())
}

/// `f32::from_bits(b) -> f32`: the binary32 value the low 32 bits of `b`
/// encode, widened to the 64-bit float slot without further rounding.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_f32_from_bits(bits: i64) -> f64 {
    f64::from(f32::from_bits(bits as u64 as u32))
}
