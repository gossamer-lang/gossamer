#![forbid(unsafe_code)]

/// Archimedes' constant π.
pub const PI: f64 = std::f64::consts::PI;
/// Euler's number e.
pub const E: f64 = std::f64::consts::E;
/// √2.
pub const SQRT_2: f64 = std::f64::consts::SQRT_2;
/// Natural logarithm of 2.
pub const LN_2: f64 = std::f64::consts::LN_2;
/// Natural logarithm of 10.
pub const LN_10: f64 = std::f64::consts::LN_10;
/// Log base 2 of e.
pub const LOG2_E: f64 = std::f64::consts::LOG2_E;
/// Log base 10 of e.
pub const LOG10_E: f64 = std::f64::consts::LOG10_E;
/// Golden ratio φ = (1 + √5) / 2.
pub const PHI: f64 = 1.618_033_988_749_895;
/// Largest finite f64 value.
pub const MAX_F64: f64 = f64::MAX;
/// Smallest positive normal f64 value.
pub const MIN_POSITIVE_F64: f64 = f64::MIN_POSITIVE;
/// Positive infinity.
pub const INF: f64 = f64::INFINITY;
/// Negative infinity.
pub const NEG_INF: f64 = f64::NEG_INFINITY;

/// Absolute value of `x`.
#[must_use]
pub fn abs(x: f64) -> f64 {
    x.abs()
}

/// Square root of `x`.
#[must_use]
pub fn sqrt(x: f64) -> f64 {
    x.sqrt()
}

/// Cube root of `x`.
#[must_use]
pub fn cbrt(x: f64) -> f64 {
    x.cbrt()
}

/// Largest integer value not greater than `x`.
#[must_use]
pub fn floor(x: f64) -> f64 {
    x.floor()
}

/// Smallest integer value not less than `x`.
#[must_use]
pub fn ceil(x: f64) -> f64 {
    x.ceil()
}

/// Nearest integer to `x`, rounding half away from zero.
#[must_use]
pub fn round(x: f64) -> f64 {
    x.round()
}

/// Integer part of `x` with the fractional part discarded.
#[must_use]
pub fn trunc(x: f64) -> f64 {
    x.trunc()
}

/// Sine of `x` (in radians).
#[must_use]
pub fn sin(x: f64) -> f64 {
    x.sin()
}

/// Cosine of `x` (in radians).
#[must_use]
pub fn cos(x: f64) -> f64 {
    x.cos()
}

/// Tangent of `x` (in radians).
#[must_use]
pub fn tan(x: f64) -> f64 {
    x.tan()
}

/// Arcsine of `x` in radians; result in \[-π/2, π/2\].
#[must_use]
pub fn asin(x: f64) -> f64 {
    x.asin()
}

/// Arccosine of `x` in radians; result in \[0, π\].
#[must_use]
pub fn acos(x: f64) -> f64 {
    x.acos()
}

/// Arctangent of `x` in radians; result in \[-π/2, π/2\].
#[must_use]
pub fn atan(x: f64) -> f64 {
    x.atan()
}

/// Four-quadrant arctangent of `y/x` in radians; result in \[-π, π\].
#[must_use]
pub fn atan2(y: f64, x: f64) -> f64 {
    y.atan2(x)
}

/// Hyperbolic sine of `x`.
#[must_use]
pub fn sinh(x: f64) -> f64 {
    x.sinh()
}

/// Hyperbolic cosine of `x`.
#[must_use]
pub fn cosh(x: f64) -> f64 {
    x.cosh()
}

/// Hyperbolic tangent of `x`.
#[must_use]
pub fn tanh(x: f64) -> f64 {
    x.tanh()
}

/// e raised to the power `x`.
#[must_use]
pub fn exp(x: f64) -> f64 {
    x.exp()
}

/// 2 raised to the power `x`.
#[must_use]
pub fn exp2(x: f64) -> f64 {
    x.exp2()
}

/// Natural logarithm of `x`.
#[must_use]
pub fn ln(x: f64) -> f64 {
    x.ln()
}

/// Base-2 logarithm of `x`.
#[must_use]
pub fn log2(x: f64) -> f64 {
    x.log2()
}

/// Base-10 logarithm of `x`.
#[must_use]
pub fn log10(x: f64) -> f64 {
    x.log10()
}

/// Logarithm of `x` with the given `base`; computed as `ln(x) / ln(base)`.
#[must_use]
pub fn log(x: f64, base: f64) -> f64 {
    x.ln() / base.ln()
}

/// `x` raised to the power `y`.
#[must_use]
pub fn pow(x: f64, y: f64) -> f64 {
    x.powf(y)
}

/// Lesser of `x` and `y`, propagating NaN.
#[must_use]
pub fn min_f64(x: f64, y: f64) -> f64 {
    // f64::min propagates NaN per IEEE 754.
    x.min(y)
}

/// Greater of `x` and `y`, propagating NaN.
#[must_use]
pub fn max_f64(x: f64, y: f64) -> f64 {
    x.max(y)
}

/// Floating-point remainder of `x / y` (`x % y` semantics).
#[must_use]
pub fn fmod(x: f64, y: f64) -> f64 {
    x % y
}

/// Euclidean distance √(x² + y²), avoiding intermediate overflow.
#[must_use]
pub fn hypot(x: f64, y: f64) -> f64 {
    x.hypot(y)
}

/// Reports whether `x` is NaN.
#[must_use]
pub fn is_nan(x: f64) -> bool {
    x.is_nan()
}

/// Reports whether `x` is infinite.
///
/// `sign > 0` checks positive infinity, `sign < 0` checks negative
/// infinity, `sign == 0` checks either.
#[must_use]
pub fn is_inf(x: f64, sign: i64) -> bool {
    match sign.cmp(&0) {
        std::cmp::Ordering::Greater => x == f64::INFINITY,
        std::cmp::Ordering::Less => x == f64::NEG_INFINITY,
        std::cmp::Ordering::Equal => x.is_infinite(),
    }
}

/// Returns the IEEE 754 "not-a-number" value.
#[must_use]
pub fn nan() -> f64 {
    f64::NAN
}

/// Returns positive infinity when `sign >= 0`, negative infinity otherwise.
#[must_use]
pub fn inf(sign: i64) -> f64 {
    if sign >= 0 {
        f64::INFINITY
    } else {
        f64::NEG_INFINITY
    }
}

/// Returns a value with the magnitude of `x` and the sign of `y`.
#[must_use]
pub fn copysign(x: f64, y: f64) -> f64 {
    x.copysign(y)
}

/// Returns `max(x - y, 0.0)`, propagating NaN (Go's `math.Dim`).
#[must_use]
pub fn dim(x: f64, y: f64) -> f64 {
    let d = x - y;
    if d > 0.0 { d } else { 0.0 }
}

/// Alias for [`fmod`] - floating-point remainder of `x / y`.
#[must_use]
pub fn mod_float(x: f64, y: f64) -> f64 {
    fmod(x, y)
}

/// Lesser of two `i64` values.
#[must_use]
pub fn min_i64(x: i64, y: i64) -> i64 {
    x.min(y)
}

/// Greater of two `i64` values.
#[must_use]
pub fn max_i64(x: i64, y: i64) -> i64 {
    x.max(y)
}

/// Absolute value of `x`, saturating at `i64::MAX` for `i64::MIN`.
#[must_use]
pub fn abs_i64(x: i64) -> i64 {
    x.saturating_abs()
}

/// Arbitrary-precision signed/unsigned integers.
pub mod big;

/// Integer bit-manipulation operations on `u64` values.
pub mod bits {
    /// Number of set bits in `x` (popcount).
    #[must_use]
    pub fn count_ones(x: u64) -> u32 {
        x.count_ones()
    }

    /// Number of clear bits in `x`.
    #[must_use]
    pub fn count_zeros(x: u64) -> u32 {
        x.count_zeros()
    }

    /// Number of leading zero bits in `x`.
    #[must_use]
    pub fn leading_zeros(x: u64) -> u32 {
        x.leading_zeros()
    }

    /// Number of trailing zero bits in `x`.
    #[must_use]
    pub fn trailing_zeros(x: u64) -> u32 {
        x.trailing_zeros()
    }

    /// Rotates `x` left by `n` bit positions; negative `n` rotates right.
    #[must_use]
    pub fn rotate_left(x: u64, n: i64) -> u64 {
        // Normalise n into [0, 64) to satisfy rotate_left's u32 contract.
        let shift = n.rem_euclid(64) as u32;
        x.rotate_left(shift)
    }

    /// Rotates `x` right by `n` bit positions; negative `n` rotates left.
    #[must_use]
    pub fn rotate_right(x: u64, n: i64) -> u64 {
        let shift = n.rem_euclid(64) as u32;
        x.rotate_right(shift)
    }

    /// Reverses the order of bits in `x`.
    #[must_use]
    pub fn reverse_bits(x: u64) -> u64 {
        x.reverse_bits()
    }

    /// Reverses the order of bytes in `x` (Go's `bits.ReverseBytes64`).
    #[must_use]
    pub fn reverse_bytes(x: u64) -> u64 {
        x.swap_bytes()
    }

    /// Minimum number of bits required to represent `x` (= 64 − leading zeros).
    #[must_use]
    pub fn len(x: u64) -> u32 {
        64 - x.leading_zeros()
    }

    /// Adds `x + y + carry` where `carry` must be 0 or 1.
    /// Returns `(sum, carry_out)`.
    #[must_use]
    pub fn add(x: u64, y: u64, carry: u64) -> (u64, u64) {
        let (s1, c1) = x.overflowing_add(y);
        let (s2, c2) = s1.overflowing_add(carry);
        (s2, u64::from(c1) + u64::from(c2))
    }

    /// Subtracts `x - y - borrow` where `borrow` must be 0 or 1.
    /// Returns `(difference, borrow_out)`.
    #[must_use]
    pub fn sub(x: u64, y: u64, borrow: u64) -> (u64, u64) {
        let (d1, b1) = x.overflowing_sub(y);
        let (d2, b2) = d1.overflowing_sub(borrow);
        (d2, u64::from(b1) + u64::from(b2))
    }

    /// Full 128-bit product of `x * y`. Returns `(hi, lo)`.
    #[must_use]
    pub fn mul(x: u64, y: u64) -> (u64, u64) {
        let p = u128::from(x) * u128::from(y);
        ((p >> 64) as u64, p as u64)
    }

    /// Divides the 128-bit value `(hi, lo)` by `y`. Returns `(quotient, remainder)`.
    ///
    /// Panics if `y == 0` or the quotient overflows `u64`.
    #[must_use]
    pub fn div(hi: u64, lo: u64, y: u64) -> (u64, u64) {
        assert_ne!(y, 0, "division by zero");
        let dividend = (u128::from(hi) << 64) | u128::from(lo);
        let divisor = u128::from(y);
        let q = dividend / divisor;
        assert!(q <= u128::from(u64::MAX), "quotient overflows u64");
        (q as u64, (dividend % divisor) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_are_sane() {
        assert!((PI - std::f64::consts::PI).abs() < 1e-15);
        assert!((E - std::f64::consts::E).abs() < 1e-15);
        assert!((PHI - 1.618_033_988_749_895).abs() < 1e-15);
        assert!(INF.is_infinite() && INF > 0.0);
        assert!(NEG_INF.is_infinite() && NEG_INF < 0.0);
    }

    #[test]
    fn trig_round_trips() {
        let x = 1.2_f64;
        assert!((asin(sin(x)) - x).abs() < 1e-12);
        assert!((acos(cos(x)) - x).abs() < 1e-12);
        assert!((atan(tan(x)) - x).abs() < 1e-12);
    }

    #[test]
    fn log_and_exp_inverses() {
        let x = 7.3_f64;
        assert!((ln(exp(x)) - x).abs() < 1e-10);
        assert!((log2(exp2(x)) - x).abs() < 1e-10);
        assert!((log10(pow(10.0, x)) - x).abs() < 1e-10);
    }

    #[test]
    fn log_custom_base() {
        assert!((log(8.0, 2.0) - 3.0).abs() < 1e-12);
    }

    #[test]
    fn is_nan_and_nan_fn() {
        assert!(is_nan(nan()));
        assert!(!is_nan(1.0));
    }

    #[test]
    fn is_inf_sign_checks() {
        assert!(is_inf(INF, 1));
        assert!(is_inf(NEG_INF, -1));
        assert!(is_inf(INF, 0));
        assert!(is_inf(NEG_INF, 0));
        assert!(!is_inf(1.0, 0));
    }

    #[test]
    fn inf_fn_sign() {
        assert_eq!(inf(1), f64::INFINITY);
        assert_eq!(inf(0), f64::INFINITY);
        assert_eq!(inf(-1), f64::NEG_INFINITY);
    }

    #[test]
    fn dim_clamps_negative_difference() {
        assert_eq!(dim(5.0, 3.0), 2.0);
        assert_eq!(dim(3.0, 5.0), 0.0);
    }

    #[test]
    fn integer_helpers() {
        assert_eq!(min_i64(-5, 3), -5);
        assert_eq!(max_i64(-5, 3), 3);
        assert_eq!(abs_i64(-7), 7);
        assert_eq!(abs_i64(i64::MIN), i64::MAX);
    }

    #[test]
    fn bits_count_ones_and_zeros() {
        assert_eq!(bits::count_ones(0b1010_1010), 4);
        assert_eq!(bits::count_zeros(u64::MAX), 0);
    }

    #[test]
    fn bits_rotate_left_and_right_inverse() {
        let x = 0x_DEAD_BEEF_CAFE_1234_u64;
        assert_eq!(bits::rotate_left(bits::rotate_right(x, 13), 13), x);
    }

    #[test]
    fn bits_rotate_negative_n() {
        let x = 1_u64;
        assert_eq!(bits::rotate_left(x, -1), bits::rotate_right(x, 1));
    }

    #[test]
    fn bits_len_matches_leading_zeros() {
        assert_eq!(bits::len(0), 0);
        assert_eq!(bits::len(1), 1);
        assert_eq!(bits::len(8), 4);
        assert_eq!(bits::len(u64::MAX), 64);
    }

    #[test]
    fn bits_add_with_carry() {
        let (s, c) = bits::add(u64::MAX, 1, 0);
        assert_eq!(s, 0);
        assert_eq!(c, 1);
    }

    #[test]
    fn bits_sub_with_borrow() {
        let (d, b) = bits::sub(0, 1, 0);
        assert_eq!(d, u64::MAX);
        assert_eq!(b, 1);
    }

    #[test]
    fn bits_mul_full_product() {
        let (hi, lo) = bits::mul(u64::MAX, u64::MAX);
        let expected = u128::from(u64::MAX) * u128::from(u64::MAX);
        assert_eq!(hi, (expected >> 64) as u64);
        assert_eq!(lo, expected as u64);
    }

    #[test]
    fn bits_div_simple() {
        let (q, r) = bits::div(0, 100, 7);
        assert_eq!(q, 14);
        assert_eq!(r, 2);
    }

    #[test]
    fn copysign_transfers_sign() {
        assert_eq!(copysign(3.0, -1.0), -3.0);
        assert_eq!(copysign(-5.0, 1.0), 5.0);
    }

    #[test]
    fn hypot_pythagorean_triple() {
        assert!((hypot(3.0, 4.0) - 5.0).abs() < 1e-12);
    }

    #[test]
    fn reverse_bytes_round_trips() {
        let x = 0x0102030405060708_u64;
        assert_eq!(bits::reverse_bytes(bits::reverse_bytes(x)), x);
    }
}
