//! 128-bit integers on the bytecode VM. A value that fits `i64` travels as
//! [`Value::Int`] (and a `u128` that fits `u64` as [`Value::Uint`]), as any
//! other integer does; only a wider one is boxed, so a program whose 128-bit
//! numbers stay small allocates nothing for them. The operations here are
//! typed by the compiler: it knows an operand is `i128` or `u128` and says
//! which, so the width a value is carried at never decides the arithmetic.

use std::cmp::Ordering;
use std::sync::Arc;

use super::{RuntimeError, RuntimeResult, Value};

/// A 128-bit integer too wide for [`Value::Int`] / [`Value::Uint`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WideInt {
    /// An `i128` outside `i64`.
    I(i128),
    /// A `u128` above `u64::MAX`.
    U(u128),
}

/// A binary operation on two 128-bit operands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WideOp {
    /// `+`.
    Add,
    /// `-`.
    Sub,
    /// `*`.
    Mul,
    /// `/`.
    Div,
    /// `%`.
    Rem,
    /// `&`.
    BitAnd,
    /// `|`.
    BitOr,
    /// `^`.
    BitXor,
    /// `<<`.
    Shl,
    /// `>>`.
    Shr,
}

impl Value {
    /// The value an `i128` computation answers, boxed only when it does not
    /// fit `i64`.
    #[must_use]
    pub fn from_i128(value: i128) -> Self {
        if let Ok(small) = i64::try_from(value) {
            Self::Int(small)
        } else if let Some(inline) = Self::inline_wide(value) {
            inline
        } else {
            Self::Wide(Arc::new(WideInt::I(value)))
        }
    }

    /// `value` held inline, when it lies in `-2^119 .. 2^119`.
    fn inline_wide(value: i128) -> Option<Self> {
        if !(-(1i128 << 119)..(1i128 << 119)).contains(&value) {
            return None;
        }
        let bytes = value.to_le_bytes();
        let mut inline = [0u8; 15];
        inline.copy_from_slice(&bytes[..15]);
        Some(Self::WideInline(inline))
    }

    /// The 128-bit value this holds when it is one of the wide forms.
    #[must_use]
    pub fn wide_int(&self) -> Option<WideInt> {
        match self {
            Self::Wide(wide) => Some(**wide),
            Self::WideInline(inline) => {
                // The sixteenth byte repeats the sign of the fifteenth.
                let mut bytes = [0u8; 16];
                bytes[..15].copy_from_slice(inline);
                bytes[15] = if inline[14] & 0x80 == 0 { 0 } else { 0xff };
                Some(WideInt::I(i128::from_le_bytes(bytes)))
            }
            _ => None,
        }
    }

    /// Whether this is a 128-bit value wider than a word.
    #[must_use]
    pub const fn is_wide(&self) -> bool {
        matches!(self, Self::Wide(_) | Self::WideInline(..))
    }

    /// The value a `u128` computation answers, boxed only when it does not
    /// fit `u64`.
    #[must_use]
    pub fn from_u128(value: u128) -> Self {
        if let Ok(small) = i64::try_from(value) {
            Self::Int(small)
        } else if let Ok(word) = u64::try_from(value) {
            Self::Uint(word)
        } else if let Some(inline) = i128::try_from(value).ok().and_then(Self::inline_wide) {
            inline
        } else {
            Self::Wide(Arc::new(WideInt::U(value)))
        }
    }

    /// The integer this value holds read as an `i128`.
    #[must_use]
    pub fn as_i128(&self) -> Option<i128> {
        match self {
            Self::Int(n) => Some(i128::from(*n)),
            Self::Uint(n) => Some(i128::from(*n)),
            Self::Wide(_) | Self::WideInline(..) => match self.wide_int()? {
                WideInt::I(n) => Some(n),
                WideInt::U(n) => i128::try_from(n).ok(),
            },
            _ => None,
        }
    }

    /// The integer this value holds read as a `u128`; a negative one has
    /// none.
    #[must_use]
    pub fn as_u128(&self) -> Option<u128> {
        match self {
            Self::Int(n) => u128::try_from(*n).ok(),
            Self::Uint(n) => Some(u128::from(*n)),
            Self::Wide(_) | Self::WideInline(..) => match self.wide_int()? {
                WideInt::I(n) => u128::try_from(n).ok(),
                WideInt::U(n) => Some(n),
            },
            _ => None,
        }
    }
}

fn operand_error(value: &Value, unsigned: bool) -> RuntimeError {
    let ty = if unsigned { "u128" } else { "i128" };
    RuntimeError::Type(format!(
        "expected `{ty}`, found `{}` ({})",
        value.type_name(),
        value.repr()
    ))
}

fn signed(value: &Value) -> RuntimeResult<i128> {
    value.as_i128().ok_or_else(|| operand_error(value, false))
}

fn unsigned(value: &Value) -> RuntimeResult<u128> {
    value.as_u128().ok_or_else(|| operand_error(value, true))
}

fn overflow(label: &str) -> RuntimeError {
    RuntimeError::Panic(format!("attempt to {label} with overflow"))
}

/// A shift amount in `0..128`, or the panic a wider one raises.
fn shift_amount(amount: &Value, left: bool) -> RuntimeResult<u32> {
    let side = if left { "left" } else { "right" };
    amount
        .as_i128()
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n < 128)
        .ok_or_else(|| RuntimeError::Panic(format!("attempt to shift {side} with overflow")))
}

/// `lhs op rhs` at 128 bits, panicking where the language does: on overflow,
/// a zero divisor, and a shift amount outside `0..128`.
///
/// # Errors
/// The panic the operation raises, or a type error for a non-integer operand.
pub fn binary(op: WideOp, lhs: &Value, rhs: &Value, is_unsigned: bool) -> RuntimeResult<Value> {
    if matches!(op, WideOp::Shl | WideOp::Shr) {
        let amount = shift_amount(rhs, op == WideOp::Shl)?;
        return Ok(if is_unsigned {
            let value = unsigned(lhs)?;
            Value::from_u128(if op == WideOp::Shl {
                value << amount
            } else {
                value >> amount
            })
        } else {
            let value = signed(lhs)?;
            Value::from_i128(if op == WideOp::Shl {
                value << amount
            } else {
                value >> amount
            })
        });
    }
    let divides = matches!(op, WideOp::Div | WideOp::Rem);
    if is_unsigned {
        let (a, b) = (unsigned(lhs)?, unsigned(rhs)?);
        if divides && b == 0 {
            return Err(RuntimeError::Panic("divide by zero".to_string()));
        }
        let value = match op {
            WideOp::Add => a.checked_add(b).ok_or_else(|| overflow("add"))?,
            WideOp::Sub => a.checked_sub(b).ok_or_else(|| overflow("subtract"))?,
            WideOp::Mul => a.checked_mul(b).ok_or_else(|| overflow("multiply"))?,
            WideOp::Div => a / b,
            WideOp::Rem => a % b,
            WideOp::BitAnd => a & b,
            WideOp::BitOr => a | b,
            WideOp::BitXor => a ^ b,
            WideOp::Shl | WideOp::Shr => unreachable!("shifts return above"),
        };
        Ok(Value::from_u128(value))
    } else {
        let (a, b) = (signed(lhs)?, signed(rhs)?);
        if divides && b == 0 {
            return Err(RuntimeError::Panic("divide by zero".to_string()));
        }
        let value = match op {
            WideOp::Add => a.checked_add(b).ok_or_else(|| overflow("add"))?,
            WideOp::Sub => a.checked_sub(b).ok_or_else(|| overflow("subtract"))?,
            WideOp::Mul => a.checked_mul(b).ok_or_else(|| overflow("multiply"))?,
            WideOp::Div => a.checked_div(b).ok_or_else(|| overflow("divide"))?,
            WideOp::Rem => a
                .checked_rem(b)
                .ok_or_else(|| overflow("calculate the remainder"))?,
            WideOp::BitAnd => a & b,
            WideOp::BitOr => a | b,
            WideOp::BitXor => a ^ b,
            WideOp::Shl | WideOp::Shr => unreachable!("shifts return above"),
        };
        Ok(Value::from_i128(value))
    }
}

/// `lhs op rhs` wrapping at 128 bits (`+%`, `-%`, `*%`, `<<%`, `>>%`); a
/// shift takes its amount modulo 128.
///
/// # Errors
/// A type error for a non-integer operand.
pub fn wrapping(op: WideOp, lhs: &Value, rhs: &Value, is_unsigned: bool) -> RuntimeResult<Value> {
    if matches!(op, WideOp::Shl | WideOp::Shr) {
        let amount = u32::try_from(signed(rhs)?.rem_euclid(128)).unwrap_or(0);
        return Ok(if is_unsigned {
            let value = unsigned(lhs)?;
            Value::from_u128(if op == WideOp::Shl {
                value << amount
            } else {
                value >> amount
            })
        } else {
            let value = signed(lhs)?;
            Value::from_i128(if op == WideOp::Shl {
                value << amount
            } else {
                value >> amount
            })
        });
    }
    if is_unsigned {
        let (a, b) = (unsigned(lhs)?, unsigned(rhs)?);
        let value = match op {
            WideOp::Sub => a.wrapping_sub(b),
            WideOp::Mul => a.wrapping_mul(b),
            _ => a.wrapping_add(b),
        };
        Ok(Value::from_u128(value))
    } else {
        let (a, b) = (signed(lhs)?, signed(rhs)?);
        let value = match op {
            WideOp::Sub => a.wrapping_sub(b),
            WideOp::Mul => a.wrapping_mul(b),
            _ => a.wrapping_add(b),
        };
        Ok(Value::from_i128(value))
    }
}

/// `-value` for an `i128`, panicking on `i128::MIN`.
///
/// # Errors
/// The overflow panic, or a type error for a non-integer operand.
pub fn negate(value: &Value) -> RuntimeResult<Value> {
    let n = signed(value)?;
    n.checked_neg()
        .map(Value::from_i128)
        .ok_or_else(|| overflow("negate"))
}

/// `!value`, the bitwise complement at 128 bits.
///
/// # Errors
/// A type error for a non-integer operand.
pub fn complement(value: &Value, is_unsigned: bool) -> RuntimeResult<Value> {
    Ok(if is_unsigned {
        Value::from_u128(!unsigned(value)?)
    } else {
        Value::from_i128(!signed(value)?)
    })
}

/// A number's place on the line, which orders an `i128` and a `u128` past
/// `i128::MAX` alike.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Magnitude {
    Negative(i128),
    NonNegative(u128),
}

fn magnitude(value: &Value) -> Option<Magnitude> {
    match value {
        Value::Int(n) if *n < 0 => Some(Magnitude::Negative(i128::from(*n))),
        Value::Int(n) => Some(Magnitude::NonNegative(n.unsigned_abs().into())),
        Value::Uint(n) => Some(Magnitude::NonNegative(u128::from(*n))),
        Value::Wide(_) | Value::WideInline(..) => Some(match value.wide_int()? {
            WideInt::I(n) if n < 0 => Magnitude::Negative(n),
            WideInt::I(n) => Magnitude::NonNegative(n.unsigned_abs()),
            WideInt::U(n) => Magnitude::NonNegative(n),
        }),
        _ => None,
    }
}

/// How two integers order when either is a boxed 128-bit value; `None`
/// unless one is and both are integers.
#[must_use]
pub fn compare(lhs: &Value, rhs: &Value) -> Option<Ordering> {
    if !lhs.is_wide() && !rhs.is_wide() {
        return None;
    }
    compare_integers(lhs, rhs)
}

/// How two integers of any width order; `None` unless both are integers.
#[must_use]
pub fn compare_integers(lhs: &Value, rhs: &Value) -> Option<Ordering> {
    Some(magnitude(lhs)?.cmp(&magnitude(rhs)?))
}

/// How two integer map keys order when either is a 128-bit key wider than a
/// word; `unsigned` reads a word key's bits as a `u64`, as a key of an
/// unsigned type holds them.
#[must_use]
pub fn compare_keys(a: &super::MapKey, b: &super::MapKey, unsigned: bool) -> Option<Ordering> {
    use super::MapKey;
    let place = |key: &MapKey| -> Option<Magnitude> {
        match key {
            MapKey::Int(n) if unsigned => Some(Magnitude::NonNegative(u128::from(*n as u64))),
            MapKey::Int(n) => magnitude(&Value::Int(*n)),
            MapKey::Uint(n) => Some(Magnitude::NonNegative(u128::from(*n))),
            MapKey::Wide(wide) => magnitude(&Value::Wide(Arc::new(**wide))),
            _ => None,
        }
    };
    if !matches!(a, MapKey::Wide(_)) && !matches!(b, MapKey::Wide(_)) {
        return None;
    }
    Some(place(a)?.cmp(&place(b)?))
}

impl std::fmt::Display for WideInt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::I(n) => write!(f, "{n}"),
            Self::U(n) => write!(f, "{n}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_that_fit_a_word_stay_unboxed() {
        assert!(matches!(Value::from_i128(-5), Value::Int(-5)));
        assert!(matches!(
            Value::from_u128(u128::from(u64::MAX)),
            Value::Uint(u64::MAX)
        ));
        assert!(matches!(Value::from_i128(i128::MAX), Value::Wide(_)));
    }

    #[test]
    fn values_within_120_bits_are_held_inline_and_read_back_whole() {
        let edges = [
            (1i128 << 119) - 1,
            -(1i128 << 119),
            i128::from(i64::MAX) + 1,
            i128::from(i64::MIN) - 1,
            -170_141_183_460_469_231_731_687,
            123_456_789_012_345_678_901_234_567_890,
        ];
        for v in edges {
            let value = Value::from_i128(v);
            assert!(matches!(value, Value::WideInline(..)), "{v} is held inline");
            assert_eq!(value.as_i128(), Some(v));
        }
        assert!(matches!(Value::from_i128(1i128 << 119), Value::Wide(_)));
        assert!(matches!(
            Value::from_i128(-(1i128 << 119) - 1),
            Value::Wide(_)
        ));
        let unsigned = Value::from_u128(1u128 << 100);
        assert!(matches!(unsigned, Value::WideInline(..)));
        assert_eq!(unsigned.as_u128(), Some(1u128 << 100));
        assert!(matches!(Value::from_u128(u128::MAX), Value::Wide(_)));
        assert_eq!(std::mem::size_of::<Value>(), 16);
    }

    #[test]
    fn arithmetic_checks_at_128_bits() {
        let big = Value::from_i128(i128::MAX);
        assert!(binary(WideOp::Add, &big, &Value::Int(1), false).is_err());
        let product = binary(WideOp::Mul, &Value::Int(i64::MAX), &Value::Int(4), false).unwrap();
        assert_eq!(product.as_i128(), Some(i128::from(i64::MAX) * 4));
        let shifted = binary(WideOp::Shl, &Value::Int(1), &Value::Int(127), true).unwrap();
        assert_eq!(shifted.as_u128(), Some(1u128 << 127));
        assert!(binary(WideOp::Shl, &Value::Int(1), &Value::Int(128), true).is_err());
        assert!(binary(WideOp::Div, &Value::Int(1), &Value::Int(0), false).is_err());
        assert!(binary(WideOp::Sub, &Value::Int(0), &Value::Int(1), true).is_err());
    }

    #[test]
    fn boxed_and_word_values_order_by_number() {
        let wide = Value::from_u128(u128::MAX);
        assert_eq!(compare(&Value::Int(-1), &wide), Some(Ordering::Less));
        assert_eq!(
            compare(&Value::from_i128(i128::MIN), &Value::Int(i64::MIN)),
            Some(Ordering::Less)
        );
    }
}
