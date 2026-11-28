//! The float conversions of `math::Rational`, whose numerator and
//! denominator cross as decimal text: the exact value of an `f64`, and the
//! `f64` nearest a ratio of integers, rounded half to even.

use num_bigint::{BigInt, BigUint};
use num_integer::Integer;
use num_traits::{ToPrimitive, Zero};

/// The numerator and positive denominator, in lowest terms, of the exact
/// value of `x`.
///
/// # Errors
/// `x` is NaN or an infinity, which no ratio of integers equals.
pub fn from_f64(x: f64) -> Result<(String, String), String> {
    if !x.is_finite() {
        return Err(format!("rational: {x} has no exact value"));
    }
    if x == 0.0 {
        return Ok(("0".to_string(), "1".to_string()));
    }
    let bits = x.to_bits();
    let biased = i64::try_from((bits >> 52) & 0x7ff).unwrap_or(0);
    let fraction = bits & ((1u64 << 52) - 1);
    let (mantissa, exponent) = if biased == 0 {
        (fraction, -1074)
    } else {
        (fraction | (1u64 << 52), biased - 1075)
    };
    // A finite nonzero float has a nonzero mantissa.
    let zeros = mantissa.trailing_zeros();
    let odd = mantissa >> zeros;
    let exponent = exponent + i64::from(zeros);
    let mut numerator = BigInt::from(odd);
    let mut denominator = BigInt::from(1u8);
    let shift = usize::try_from(exponent.unsigned_abs()).unwrap_or(0);
    if exponent >= 0 {
        numerator <<= shift;
    } else {
        denominator <<= shift;
    }
    if x < 0.0 {
        numerator = -numerator;
    }
    Ok((numerator.to_string(), denominator.to_string()))
}

/// `2^e` for an exponent of the normal range, built from its bits.
fn pow2(e: i64) -> f64 {
    debug_assert!((-1022..=1023).contains(&e));
    f64::from_bits(u64::try_from(e + 1023).unwrap_or(0) << 52)
}

/// `m * 2^e` where the product is representable (or overflows to infinity),
/// so each step is exact.
fn scale(m: f64, mut e: i64) -> f64 {
    let mut v = m;
    while e > 1000 {
        v *= pow2(1000);
        e -= 1000;
    }
    while e < -1000 {
        v *= pow2(-1000);
        e += 1000;
    }
    v * pow2(e)
}

/// The `f64` nearest `numerator / denominator`, ties to even, an infinity
/// past the largest finite float and zero below half the smallest subnormal.
///
/// # Errors
/// Either text is not a decimal integer, or the denominator is zero.
pub fn to_f64(numerator: &str, denominator: &str) -> Result<f64, String> {
    let top: BigInt = numerator
        .parse()
        .map_err(|_| format!("rational: `{numerator}` is not an integer"))?;
    let bottom: BigInt = denominator
        .parse()
        .map_err(|_| format!("rational: `{denominator}` is not an integer"))?;
    if bottom.is_zero() {
        return Err("rational: zero denominator".to_string());
    }
    let negative = (top < BigInt::zero()) != (bottom < BigInt::zero());
    let dividend: BigUint = top.magnitude().clone();
    let divisor: BigUint = bottom.magnitude().clone();
    if dividend.is_zero() {
        return Ok(0.0);
    }
    let signed = |value: f64| if negative { -value } else { value };
    let bits_of = |value: &BigUint| i64::try_from(value.bits()).unwrap_or(i64::MAX);
    // The 55-bit quotient `floor(dividend * 2^shift / divisor)`, with whether
    // anything was left over: two bits past the 53 kept decide the rounding,
    // the remainder breaks a tie.
    let mut shift = 54 - (bits_of(&dividend) - bits_of(&divisor));
    let (quotient, sticky) = loop {
        let amount = usize::try_from(shift.unsigned_abs()).unwrap_or(0);
        let (scaled, by) = if shift >= 0 {
            (&dividend << amount, divisor.clone())
        } else {
            (dividend.clone(), &divisor << amount)
        };
        let (quotient, remainder) = scaled.div_rem(&by);
        match bits_of(&quotient) {
            55 => break (quotient.to_u64().unwrap_or(0), !remainder.is_zero()),
            more if more > 55 => shift -= 1,
            _ => shift += 1,
        }
    };
    // The value lies in [2^exponent, 2^(exponent + 1)).
    let exponent = 54 - shift;
    if exponent > 1023 {
        return Ok(signed(f64::INFINITY));
    }
    // Bits the result keeps: 53, fewer in the subnormal range, whose last
    // bit is worth 2^-1074.
    let keep = (exponent + 1075).min(53);
    if keep < 0 {
        return Ok(signed(0.0));
    }
    let dropped = u32::try_from(55 - keep).unwrap_or(55);
    let mut mantissa = quotient >> dropped;
    let rest = quotient & ((1u64 << dropped) - 1);
    let half = 1u64 << (dropped - 1);
    if rest > half || (rest == half && (sticky || mantissa & 1 == 1)) {
        mantissa += 1;
    }
    // A 54-bit mantissa or less converts exactly.
    let kept = mantissa.to_f64().unwrap_or(0.0);
    Ok(signed(scale(kept, exponent - keep + 1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(x: f64) -> f64 {
        let (n, d) = from_f64(x).unwrap();
        to_f64(&n, &d).unwrap()
    }

    #[test]
    fn every_float_is_an_exact_ratio_that_converts_back() {
        for x in [
            1.0,
            -1.0,
            0.1,
            1.0 / 3.0,
            f64::MAX,
            f64::MIN,
            f64::MIN_POSITIVE,
            f64::MIN_POSITIVE / 3.0,
            5e-324,
            -5e-324,
            1e300,
            123_456.789,
            f64::EPSILON,
        ] {
            assert_eq!(round_trip(x).to_bits(), x.to_bits(), "{x:e}");
        }
        assert_eq!(
            from_f64(0.1).unwrap().1,
            (BigInt::from(1u8) << 55usize).to_string()
        );
        assert_eq!(from_f64(-2.5).unwrap(), ("-5".to_string(), "2".to_string()));
        assert_eq!(from_f64(-0.0).unwrap(), ("0".to_string(), "1".to_string()));
        assert!(from_f64(f64::NAN).is_err());
        assert!(from_f64(f64::NEG_INFINITY).is_err());
    }

    #[test]
    fn small_ratios_match_correctly_rounded_division() {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..20_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let a = i64::try_from(state >> 11).unwrap_or(1) - (1 << 52);
            let b = i64::try_from((state >> 30) | 1).unwrap_or(1);
            // Both stay below 2^53, so each converts exactly and the quotient
            // is IEEE's correctly rounded one; the lint's precision loss cannot
            // happen here.
            #[allow(clippy::cast_precision_loss)]
            let expected = a as f64 / b as f64;
            assert_eq!(
                to_f64(&a.to_string(), &b.to_string()).unwrap().to_bits(),
                expected.to_bits(),
                "{a}/{b}"
            );
        }
    }

    #[test]
    fn ties_go_to_even_and_extremes_saturate() {
        let two53 = 1u64 << 53;
        assert_eq!(
            to_f64(&(two53 + 1).to_string(), "1").unwrap(),
            9_007_199_254_740_992.0
        );
        assert_eq!(
            to_f64(&(two53 + 3).to_string(), "1").unwrap(),
            9_007_199_254_740_996.0
        );
        let huge = "1".to_string() + &"0".repeat(400);
        assert_eq!(to_f64(&huge, "1").unwrap(), f64::INFINITY);
        assert_eq!(to_f64(&format!("-{huge}"), "1").unwrap(), f64::NEG_INFINITY);
        assert_eq!(to_f64("1", &huge).unwrap(), 0.0);
        let half_min = (BigInt::from(1u8) << 1075usize).to_string();
        assert_eq!(to_f64("1", &half_min).unwrap(), 0.0);
        let quarter_min = (BigInt::from(1u8) << 1076usize).to_string();
        assert_eq!(to_f64("3", &quarter_min).unwrap(), 5e-324);
        assert_eq!(to_f64("1", &quarter_min).unwrap(), 0.0);
        assert_eq!(to_f64("-3", "-4").unwrap(), 0.75);
        assert!(to_f64("1", "0").is_err());
    }
}
