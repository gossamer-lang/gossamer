//! Pure shared builtin helpers.
//!
//! Every helper here is called by **both** the tree-walking
//! interpreter and (eventually) the native backend's runtime
//! stubs. Putting the canonical implementation in one place stops
//! the interpreter from carrying interpreter-specific logic for
//! things that ought to behave the same in compiled code.
//!
//! Functions here are **pure** - they take Rust values and return
//! `String` / primitive outputs. Anything that needs heap
//! allocation or I/O belongs in a later slice that wires the
//! internal ABI through Cranelift.

#![forbid(unsafe_code)]

/// Canonical decimal rendering of a 64-bit signed integer, used
/// wherever Gossamer programs observe an `i64` as text - `println`,
/// `format!("{n}")`, `to_string`, assertion diffs, etc.
#[must_use]
pub fn format_int(n: i64) -> String {
    format!("{n}")
}

/// Canonical decimal rendering of a 64-bit unsigned integer. A slot holds the
/// value's bits, so one at or above `i64::MAX` reads as its own decimal here
/// rather than the negative [`format_int`] would spell.
#[must_use]
pub fn format_uint(n: u64) -> String {
    format!("{n}")
}

/// Canonical rendering of a 64-bit float. Matches Rust's `{f}`
/// format - the interpreter and native backend must not diverge on
/// NaN / infinity / negative-zero output, so the single
/// implementation lives here.
#[must_use]
pub fn format_float(f: f64) -> String {
    let mut text = FloatText::new();
    // Every byte `f64_display` writes is ASCII.
    String::from_utf8_lossy(f64_display(f, &mut text)).into_owned()
}

/// Buffer for [`f64_display`]: a fixed slot for the usual rendering and a
/// heap spill, left unallocated, for the long ones.
pub struct FloatText {
    shortest: zmij::Buffer,
    buf: [u8; FLOAT_LAYOUT_CAP],
    long: String,
}

impl std::fmt::Debug for FloatText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FloatText").finish_non_exhaustive()
    }
}

/// Longest layout written into the fixed slot. A longer one is almost all
/// padding zeros (`1e300`, `1e-300`) and takes the core formatter.
const FLOAT_LAYOUT_CAP: usize = 40;

impl FloatText {
    /// An empty buffer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            shortest: zmij::Buffer::new(),
            buf: [0; FLOAT_LAYOUT_CAP],
            long: String::new(),
        }
    }
}

impl Default for FloatText {
    fn default() -> Self {
        Self::new()
    }
}

/// The text Rust's `Display` for `f64` renders for `x`: the shortest digits
/// that read back as `x`, laid out as a plain decimal and never with an
/// exponent (`1`, `1.5`, `0.0000001`, `1000000000000000000000`, `-0`), or
/// `NaN`, `inf`, `-inf`.
///
/// The digits come from a shortest round-trip formatter and are re-laid out
/// here, which is what keeps the rendering one `format!("{x}")` agrees with
/// at a fraction of its cost.
pub fn f64_display(value: f64, out: &mut FloatText) -> &[u8] {
    if value.is_nan() {
        return b"NaN";
    }
    if value.is_infinite() {
        return if value < 0.0 { b"-inf" } else { b"inf" };
    }
    let negative = value.is_sign_negative();
    if value == 0.0 {
        return if negative { b"-0" } else { b"0" };
    }
    let FloatText {
        shortest,
        buf,
        long,
    } = out;
    let signed = shortest.format_finite(value).as_bytes();
    if let Some(kept) = plain_shortest_len(value, signed) {
        return &signed[..kept];
    }
    let text = &signed[usize::from(negative)..];
    let mut digits = [0u8; 20];
    let (count, point) = shortest_digits(text, &mut digits);
    let width = match usize::try_from(point) {
        Err(_) | Ok(0) => 2 + point.unsigned_abs() as usize + count,
        Ok(whole) if whole >= count => whole,
        Ok(_) => count + 1,
    };
    if width + 1 > FLOAT_LAYOUT_CAP || may_sit_on_a_tie(value.abs(), &digits[..count], point) {
        return display_through_core(value, long);
    }
    let mut len = 0usize;
    if negative {
        buf[len] = b'-';
        len += 1;
    }
    let digits = &digits[..count];
    if point <= 0 {
        let zeros = point.unsigned_abs() as usize;
        buf[len] = b'0';
        buf[len + 1] = b'.';
        len += 2;
        buf[len..len + zeros].fill(b'0');
        len += zeros;
        buf[len..len + count].copy_from_slice(digits);
        len += count;
    } else {
        let whole = point as usize;
        if whole >= count {
            buf[len..len + count].copy_from_slice(digits);
            len += count;
            buf[len..len + whole - count].fill(b'0');
            len += whole - count;
        } else {
            buf[len..len + whole].copy_from_slice(&digits[..whole]);
            len += whole;
            buf[len] = b'.';
            len += 1;
            buf[len..len + count - whole].copy_from_slice(&digits[whole..]);
            len += count - whole;
        }
    }
    &buf[..len]
}

/// Canonical `{:?}` rendering of a float: always recoverable as a float, so
/// an integral value keeps a `.0` and a magnitude outside the plain-decimal
/// window switches to exponent form. Mirrors Rust's `Debug for f64`, which
/// prints decimals with at least one fractional digit for `0` and for
/// `1e-4 <= |x| < 1e16`, and exponent form otherwise.
#[must_use]
pub fn format_float_debug(f: f64) -> String {
    format!("{f:?}")
}

/// Canonical rendering of a boolean: `"true"` / `"false"`. The
/// constant is shared so both paths format the value identically -
/// subtle case differences would otherwise cause parity-harness
/// divergences.
#[must_use]
pub const fn format_bool(b: bool) -> &'static str {
    if b { "true" } else { "false" }
}

/// Canonical rendering of the unit value. Hard-coded to `"()"`.
#[must_use]
pub const fn format_unit() -> &'static str {
    "()"
}

/// Canonical prefix for a runtime-error diagnostic. The interpreter
/// already uses this format via `RuntimeError`'s `Display` impl;
/// the native backend's future runtime-panic helper must emit the
/// identical prefix so `cargo test -p gossamer-cli --test parity`
/// sees byte-identical stderr in both paths.
///
/// Callers compose the full message as
/// `format!("{}{}\n", runtime_error_prefix("GX0005"), detail)`.
#[must_use]
pub fn runtime_error_prefix(code: &str) -> String {
    format!("error[{code}]: ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_float_debug_always_reads_back_as_a_float() {
        assert_eq!(format_float_debug(7.0), "7.0");
        assert_eq!(format_float_debug(-0.5), "-0.5");
        assert_eq!(format_float_debug(-0.0), "-0.0");
        assert_eq!(format_float_debug(1e300), "1e300");
        assert_eq!(format_float_debug(1e-10), "1e-10");
        assert_eq!(format_float_debug(f64::NAN), "NaN");
        assert_eq!(format_float_debug(f64::INFINITY), "inf");
        // Display keeps the bare integral spelling.
        assert_eq!(format_float(7.0), "7");
    }

    #[test]
    fn format_int_matches_rust_display_for_extremes() {
        assert_eq!(format_int(0), "0");
        assert_eq!(format_int(i64::MAX), "9223372036854775807");
        assert_eq!(format_int(i64::MIN), "-9223372036854775808");
    }

    #[test]
    fn format_float_preserves_nan_and_infinity_spelling() {
        assert_eq!(format_float(f64::INFINITY), "inf");
        assert_eq!(format_float(f64::NEG_INFINITY), "-inf");
        assert!(format_float(f64::NAN).contains("NaN"));
        assert_eq!(format_float(-0.0), "-0");
    }

    #[test]
    fn format_bool_returns_static_lowercase() {
        assert_eq!(format_bool(true), "true");
        assert_eq!(format_bool(false), "false");
    }

    #[test]
    fn runtime_error_prefix_is_code_bracket_colon_space() {
        assert_eq!(runtime_error_prefix("GX0001"), "error[GX0001]: ");
    }
}

/// Splits `text`, the shortest formatter's `D[.D][e[-]N]` for a positive
/// value, into its significant digits, written to `digits`, and answers their
/// count and the decimal point's position relative to the first of them.
fn shortest_digits(text: &[u8], digits: &mut [u8; 20]) -> (usize, i32) {
    let mut count = 0usize;
    let mut point: i32 = 0;
    let mut seen_point = false;
    let mut exponent: i32 = 0;
    let mut pos = 0usize;
    while pos < text.len() {
        let byte = text[pos];
        match byte {
            b'0'..=b'9' => {
                if count == 0 && byte == b'0' {
                    // A leading zero only moves the point.
                    if seen_point {
                        point -= 1;
                    }
                } else {
                    digits[count] = byte;
                    count += 1;
                    if !seen_point {
                        point += 1;
                    }
                }
            }
            b'.' => seen_point = true,
            b'e' | b'E' => {
                let (sign, start) = match text.get(pos + 1) {
                    Some(b'-') => (-1, pos + 2),
                    Some(b'+') => (1, pos + 2),
                    _ => (1, pos + 1),
                };
                let mut exp: i32 = 0;
                for &digit in &text[start..] {
                    exp = exp * 10 + i32::from(digit - b'0');
                }
                exponent = sign * exp;
                break;
            }
            _ => {}
        }
        pos += 1;
    }
    while count > 0 && digits[count - 1] == b'0' {
        count -= 1;
    }
    (count, point + exponent)
}

/// Whether `value` lies exactly halfway between `digits` (read as `0.digits`
/// times `10^point`) and a neighbour one unit in the last digit away, or
/// whether that cannot be ruled out cheaply.
///
/// The shortest-digit formatter breaks such a tie to the even digit and
/// `Display` breaks it away from zero, so a tie takes the core formatter's
/// answer. A tie needs `value`'s exact decimal expansion to end one digit past
/// `digits`, so the check compares that expansion with both halfway points.
fn may_sit_on_a_tie(value: f64, digits: &[u8], point: i32) -> bool {
    let bits = value.to_bits();
    let biased = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & ((1u64 << 52) - 1);
    let (mut mantissa, mut exp2) = if biased == 0 {
        (fraction, -1074)
    } else {
        (fraction | (1u64 << 52), biased - 1075)
    };
    let zeros = mantissa.trailing_zeros();
    mantissa >>= zeros;
    exp2 += zeros as i32;
    // The halfway points are `(10 * kept +- 5) * 10^half_exp`.
    let half_exp = point - digits.len() as i32 - 1;
    // `mantissa / 2^-exp2` with `mantissa` odd ends exactly `-exp2` places past
    // the point, and a whole `value` has no fractional digit to be halfway on.
    if (exp2 < 0 && half_exp != exp2) || (exp2 >= 0 && half_exp < 0) {
        return false;
    }
    let kept = digits
        .iter()
        .fold(0u64, |acc, &c| acc * 10 + u64::from(c - b'0'));
    let halves = [kept * 10 + 5, (kept * 10).saturating_sub(5)];
    let exact = if exp2 < 0 {
        5u128
            .checked_pow(exp2.unsigned_abs())
            .and_then(|five_pow| u128::from(mantissa).checked_mul(five_pow))
            .map(|scaled| (scaled, 1u128))
    } else {
        u128::from(mantissa)
            .checked_shl(exp2.unsigned_abs())
            .filter(|v| v.checked_shr(exp2.unsigned_abs()) == Some(u128::from(mantissa)))
            .zip(10u128.checked_pow(half_exp.unsigned_abs()))
    };
    let Some((exact, scale)) = exact else {
        return true;
    };
    halves.iter().any(|&half| {
        u128::from(half)
            .checked_mul(scale)
            .is_none_or(|v| v == exact)
    })
}

/// How much of `text`, the shortest formatter's rendering of a nonzero finite
/// `value`, is already `Display`'s text: all of a plain decimal, or an
/// integer's without its `.0`, when its digits cannot sit on a tie. `None`
/// for an exponent form or a possible tie.
fn plain_shortest_len(value: f64, text: &[u8]) -> Option<usize> {
    if text.contains(&b'e') {
        return None;
    }
    let dot = text.iter().position(|&b| b == b'.')?;
    let fraction = &text[dot + 1..];
    if fraction == b"0" {
        // An integer below 2^53 is exact, so no digit past its own decides it.
        return (value.abs() < 9_007_199_254_740_992.0).then_some(dot);
    }
    // `mantissa * 2^exp2` with `mantissa` odd ends `-exp2` places past the
    // point, and a tie ends exactly one place past the shortest digits.
    (odd_mantissa_exponent(value) != -(fraction.len() as i32 + 1)).then_some(text.len())
}

/// `exp2` in `value == mantissa * 2^exp2` with `mantissa` odd, for a nonzero
/// finite `value`.
fn odd_mantissa_exponent(value: f64) -> i32 {
    let bits = value.to_bits();
    let biased = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & ((1u64 << 52) - 1);
    let (mantissa, exp2) = if biased == 0 {
        (fraction, -1074)
    } else {
        (fraction | (1u64 << 52), biased - 1075)
    };
    exp2 + mantissa.trailing_zeros() as i32
}

/// The core formatter's own `Display` text for `value`.
fn display_through_core(value: f64, long: &mut String) -> &[u8] {
    use std::fmt::Write as _;
    long.clear();
    // Writing into a `String` cannot fail.
    let _ = write!(long, "{value}");
    long.as_bytes()
}

#[cfg(test)]
mod float_display_tests {
    use super::{FloatText, f64_display};

    // Miri interprets every formatted value; a few hundred still reach each
    // branch of the layout and the tie check.
    const RANDOM_VALUES: usize = if cfg!(miri) { 300 } else { 1_000_000 };
    const TIE_VALUES: usize = if cfg!(miri) { 20 } else { 300_000 };
    const EXPONENT_STEP: usize = if cfg!(miri) { 29 } else { 1 };

    fn check(x: f64) {
        let mut text = FloatText::new();
        let got = f64_display(x, &mut text);
        assert_eq!(
            std::str::from_utf8(got).unwrap(),
            format!("{x}"),
            "bits {:#x}",
            x.to_bits()
        );
    }

    #[test]
    fn f64_display_matches_rust_display_on_edge_values() {
        let edges = [
            0.0,
            -0.0,
            1.0,
            -1.0,
            1.5,
            0.1,
            0.3,
            0.1 + 0.2,
            1e21,
            1e22,
            1e-7,
            123_456.789,
            f64::MIN_POSITIVE,
            f64::MAX,
            f64::MIN,
            f64::EPSILON,
            f64::from_bits(1),
            f64::from_bits(0x000f_ffff_ffff_ffff),
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            9_007_199_254_740_992.0,
            9_007_199_254_740_993.0,
            9_007_199_254_740_991.0,
        ];
        for x in edges {
            check(x);
        }
        for e in (-320..=308).step_by(EXPONENT_STEP) {
            check(format!("1e{e}").parse().unwrap());
            check(format!("-7.25e{e}").parse().unwrap());
        }
        for n in ((1u64 << 53) - 64..(1 << 53) + 64).step_by(EXPONENT_STEP) {
            check(n as f64);
        }
    }

    #[test]
    fn f64_display_matches_rust_display_on_random_bit_patterns() {
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        for _ in 0..RANDOM_VALUES {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            check(f64::from_bits(state));
        }
    }

    #[test]
    fn f64_display_matches_rust_display_on_plain_decimals() {
        let mut state: u64 = 0x5851_f42d_4c95_7f2d;
        for _ in 0..RANDOM_VALUES {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let decimal = (state % 1_000_000_000) as f64 / 10f64.powi((state >> 40) as i32 % 12);
            check(decimal);
            check(-decimal);
            // Every mantissa under a binary exponent that prints without one.
            let biased = 1023 - 16 + (state >> 52) % 69;
            check(f64::from_bits(
                (biased << 52) | (state & ((1u64 << 52) - 1)),
            ));
        }
    }

    #[test]
    fn f64_display_breaks_exact_ties_as_rust_display_does() {
        let mut state: u64 = 0x2545_f491_4f6c_dd1d;
        for _ in 0..TIE_VALUES {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let m = (state >> 11) as f64;
            for shift in [-12, -8, -4, -3, -2, -1, 1, 4, 8, 20, 60, 100] {
                check(m * 2f64.powi(shift));
                check(-(m * 2f64.powi(shift)));
            }
        }
    }
}
