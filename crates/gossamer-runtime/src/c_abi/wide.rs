//! 128-bit integer arithmetic the Cranelift tier calls where its backend has
//! no instruction: a multiply whose operands do not both fit 64 bits, and
//! division. Each operand crosses as its low and high words.

use super::concat::wide_from_words;

/// `gos_rt_wide_divrem`'s operation: signed or unsigned, quotient or
/// remainder.
pub mod divrem_op {
    /// Signed quotient.
    pub const SIGNED_DIV: i64 = 0;
    /// Unsigned quotient.
    pub const UNSIGNED_DIV: i64 = 1;
    /// Signed remainder.
    pub const SIGNED_REM: i64 = 2;
    /// Unsigned remainder.
    pub const UNSIGNED_REM: i64 = 3;
}

fn bits(low: i64, high: i64) -> u128 {
    wide_from_words(low.cast_unsigned(), high.cast_unsigned())
}

/// The product of two 128-bit integers (`u128` when `unsigned` is non-zero),
/// raising the overflow panic every tier raises when it does not fit.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn gos_rt_wide_mul_checked(
    a_low: i64,
    a_high: i64,
    b_low: i64,
    b_high: i64,
    unsigned: i64,
) -> i128 {
    let (a, b) = (bits(a_low, a_high), bits(b_low, b_high));
    let product = if unsigned != 0 {
        a.checked_mul(b).map(u128::cast_signed)
    } else {
        a.cast_signed().checked_mul(b.cast_signed())
    };
    match product {
        Some(value) => value,
        None => super::panic::panic_text("attempt to multiply with overflow"),
    }
}

/// The quotient or remainder `op` names (see [`divrem_op`]) of two 128-bit
/// integers. The caller has already ruled out a zero divisor and a signed
/// `MIN / -1`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_wide_divrem(
    a_low: i64,
    a_high: i64,
    b_low: i64,
    b_high: i64,
    op: i64,
) -> i128 {
    let (a, b) = (bits(a_low, a_high), bits(b_low, b_high));
    if b == 0 {
        return 0;
    }
    match op {
        divrem_op::UNSIGNED_DIV => (a / b).cast_signed(),
        divrem_op::SIGNED_REM => a.cast_signed().wrapping_rem(b.cast_signed()),
        divrem_op::UNSIGNED_REM => (a % b).cast_signed(),
        _ => a.cast_signed().wrapping_div(b.cast_signed()),
    }
}

/// A 128-bit integer passed as its low and high words, converted to a
/// float: bit 0 of `flags` reads the bits as a `u128`, bit 1 rounds once to
/// `f32` (answered widened, exactly), as the language's `as` does.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_wide_to_float(low: i64, high: i64, flags: i64) -> f64 {
    let value = bits(low, high);
    match (flags & 1 != 0, flags & 2 != 0) {
        (true, false) => value as f64,
        (true, true) => f64::from(value as f32),
        (false, false) => value.cast_signed() as f64,
        (false, true) => f64::from(value.cast_signed() as f32),
    }
}

/// `value as i128` (`as u128` when `unsigned` is non-zero): saturating at the
/// target's range, with NaN reading as zero, as the language's `as` does.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_float_to_wide(value: f64, unsigned: i64) -> i128 {
    if unsigned != 0 {
        (value as u128).cast_signed()
    } else {
        value as i128
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(v: i128) -> (i64, i64) {
        let bits = v.cast_unsigned();
        (
            (bits as u64).cast_signed(),
            ((bits >> 64) as u64).cast_signed(),
        )
    }

    #[test]
    fn a_wide_product_and_quotient_match_the_language() {
        let (al, ah) = words(-170_141_183_460_469_231_731_687_303_715_884_105_727);
        let (bl, bh) = words(1);
        assert_eq!(
            gos_rt_wide_mul_checked(al, ah, bl, bh, 0),
            -170_141_183_460_469_231_731_687_303_715_884_105_727
        );
        let (al, ah) = words(100_000_000_000_000_000_007);
        let (bl, bh) = words(-10);
        assert_eq!(
            gos_rt_wide_divrem(al, ah, bl, bh, divrem_op::SIGNED_DIV),
            -10_000_000_000_000_000_000
        );
        assert_eq!(gos_rt_wide_divrem(al, ah, bl, bh, divrem_op::SIGNED_REM), 7);
    }
}
