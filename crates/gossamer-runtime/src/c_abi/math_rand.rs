#![allow(clippy::missing_safety_doc)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_precision_loss)]

//! Runtime support for `std::math::rand` - the deterministic
//! SplitMix64 `Rng`. The handle is an opaque heap `Box<GosRng>`;
//! compiled tiers carry the pointer as an `i64` and the MIR
//! receiver-kind dispatch tags constructor results `math::rand::Rng`
//! so method calls route to the helpers below.
//!
//! The SplitMix64 step is inlined here (rather than depending on
//! `gossamer_std::mathrand`, which would form a `runtime -> std ->
//! runtime` dependency cycle) and kept bit-identical to the VM's
//! `gossamer_std::mathrand::Rng` so the sequence matches on every tier.

/// Opaque heap handle wrapping the deterministic SplitMix64 state.
pub struct GosRng {
    state: u64,
}

super::rc::managed_handle!(GosRng);

impl GosRng {
    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

/// Allocate a new RNG seeded with `seed` (reinterpreted as `u64`).
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_math_rng_new(seed: i64) -> *mut GosRng {
    ffi_entry!(std::ptr::null_mut(), {
        super::rc::alloc_managed(GosRng { state: seed as u64 })
    })
}

/// Next 64-bit output; the `u64` bit pattern is returned as `i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_rng_next_u64(r: *mut GosRng) -> i64 {
    ffi_entry!(0, {
        if r.is_null() {
            return 0;
        }
        // SAFETY: `r` is a handle from compiled code, checked non-null above and live for the whole call.
        unsafe { &mut *r }.next_u64() as i64
    })
}

/// Next 32-bit output, zero-extended into `i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_rng_next_u32(r: *mut GosRng) -> i64 {
    ffi_entry!(0, {
        if r.is_null() {
            return 0;
        }
        // SAFETY: `r` is a handle from compiled code, checked non-null above and live for the whole call.
        i64::from((unsafe { &mut *r }.next_u64() >> 32) as u32)
    })
}

/// Uniform value in `[low, high)`. An empty or inverted range yields
/// `low` rather than unwinding across the `extern "C"` boundary
/// (the Rust `Rng::range_u64` asserts; the shim guards instead).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_rng_range_u64(r: *mut GosRng, low: i64, high: i64) -> i64 {
    ffi_entry!(0, {
        if r.is_null() {
            return low;
        }
        let lo = low as u64;
        let hi = high as u64;
        if hi <= lo {
            return low;
        }
        // SAFETY: `r` is a handle from compiled code, checked non-null above and live for the whole call.
        (lo + unsafe { &mut *r }.next_u64() % (hi - lo)) as i64
    })
}

/// Uniform `f64` in `[0.0, 1.0)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_math_rng_next_f64(r: *mut GosRng) -> f64 {
    ffi_entry!(0.0, {
        if r.is_null() {
            return 0.0;
        }
        // SAFETY: `r` is a handle from compiled code, checked non-null above and live for the whole call.
        let v = unsafe { &mut *r }.next_u64();
        (v >> 11) as f64 / ((1u64 << 53) as f64)
    })
}
