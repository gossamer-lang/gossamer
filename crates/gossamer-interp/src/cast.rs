//! Runtime semantics for the GT0005 `as`-cast whitelist, shared by
//! the bytecode `Op::CastScalar` and the tree-walker so both engines
//! produce bit-identical results (and match the compiled tiers).

use gossamer_types::{FloatTy, IntTy, TyKind};

use crate::value::Value;

/// Compile-time-resolved cast destination. Collapses the `TyKind`
/// surface to exactly the shapes the i64 runtime model distinguishes.
/// `pub` because it rides inside the (crate-private) `bytecode::Op`
/// enum, which is itself declared `pub` within its private module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    unreachable_pub,
    reason = "the cast target is public to the crate's own backends only"
)]
pub enum CastTarget {
    /// Narrow signed/unsigned int: truncate to `width` bits, then
    /// extend by `signed`.
    IntNarrow {
        /// Declared width in bits (8 / 16 / 32).
        width: u8,
        /// `true` for i8/i16/i32, `false` for u8/u16/u32.
        signed: bool,
    },
    /// `i64` / `isize` - full-width signed, identity on the bit
    /// pattern.
    I64,
    /// `u64` / `usize` - full-width with unsigned display provenance.
    U64,
    /// `f32` - value rounds through f32 precision (stored as f64).
    F32,
    /// `f64`.
    F64,
    /// `f32` from a `u64` / `usize` source, whose bits read as unsigned.
    F32FromUnsigned,
    /// `f64` from a `u64` / `usize` source, whose bits read as unsigned.
    F64FromUnsigned,
    /// `char` - operand is a `u8` by the whitelist; mask to the
    /// declared width and take the code point.
    Char,
    /// `bool` - only the same-type no-op is whitelisted.
    Bool,
}

impl CastTarget {
    /// Maps a resolved target `TyKind` to its runtime cast shape.
    /// Returns `None` for non-scalar targets (those never pass the
    /// checker's GT0005 whitelist).
    pub(crate) fn of(kind: &TyKind) -> Option<Self> {
        Some(match kind {
            TyKind::Int(IntTy::I8) => Self::IntNarrow {
                width: 8,
                signed: true,
            },
            TyKind::Int(IntTy::I16) => Self::IntNarrow {
                width: 16,
                signed: true,
            },
            TyKind::Int(IntTy::I32) => Self::IntNarrow {
                width: 32,
                signed: true,
            },
            TyKind::Int(IntTy::U8) => Self::IntNarrow {
                width: 8,
                signed: false,
            },
            TyKind::Int(IntTy::U16) => Self::IntNarrow {
                width: 16,
                signed: false,
            },
            TyKind::Int(IntTy::U32) => Self::IntNarrow {
                width: 32,
                signed: false,
            },
            TyKind::Int(IntTy::I64 | IntTy::Isize) => Self::I64,
            TyKind::Int(IntTy::U64 | IntTy::Usize) => Self::U64,
            TyKind::Float(FloatTy::F32) => Self::F32,
            TyKind::Float(FloatTy::F64) => Self::F64,
            TyKind::Char => Self::Char,
            TyKind::Bool => Self::Bool,
            _ => return None,
        })
    }

    /// The same target read from a source of kind `source`: a `u64` or
    /// `usize` operand converts to a float by its unsigned value.
    pub(crate) fn read_from(self, source: Option<&TyKind>) -> Self {
        let unsigned = matches!(source, Some(TyKind::Int(IntTy::U64 | IntTy::Usize)));
        match self {
            Self::F32 if unsigned => Self::F32FromUnsigned,
            Self::F64 if unsigned => Self::F64FromUnsigned,
            other => other,
        }
    }
}

/// Truncates `v` to `width` bits and extends back by `signed` -
/// the single masking point of the i64 runtime model
/// (`300 as u8 == 44`, `200 as i8 == -56`).
fn trunc_extend(v: i64, width: u8, signed: bool) -> i64 {
    let shift = 64 - u32::from(width);
    if signed {
        (v << shift) >> shift
    } else {
        ((v as u64) << shift >> shift) as i64
    }
}

/// The 64-bit integer bit pattern behind any int-like scalar.
fn int_base(v: &Value) -> Option<i64> {
    match v {
        Value::Int(n) => Some(*n),
        Value::Uint(n) => Some(*n as i64),
        Value::Bool(b) => Some(i64::from(*b)),
        Value::Char(c) => Some(i64::from(u32::from(*c))),
        _ => None,
    }
}

/// Applies a whitelisted scalar cast to `v`. Float → int saturates
/// at i64 width with no narrow mask; int → narrow truncates and
/// extends at the declared width; `u8 as char` masks to the declared
/// width and takes the code point. Returns `None` when the value's
/// runtime shape cannot reach `target` (checker-rejected programs
/// only).
pub(crate) fn cast_scalar(v: &Value, target: CastTarget) -> Option<Value> {
    match target {
        CastTarget::IntNarrow { width, signed } => {
            if let Value::Float(f) = v {
                // A float-to-integer cast saturates at the TARGET's range:
                // `300.7 as u8` is `255` and `-1.5 as u8` is `0`. NaN reads
                // as zero, which `as i64` already answers.
                let (low, high) = gossamer_abi::int_range::bounds(u32::from(width), signed);
                return Some(Value::Int((*f as i64).clamp(low, high)));
            }
            int_base(v).map(|n| Value::Int(trunc_extend(n, width, signed)))
        }
        CastTarget::I64 => {
            if let Value::Float(f) = v {
                return Some(Value::Int(*f as i64));
            }
            int_base(v).map(Value::Int)
        }
        CastTarget::U64 => {
            // A float saturates at the unsigned range: `-1.5 as u64` is `0`.
            if let Value::Float(f) = v {
                return Some(Value::Uint(*f as u64));
            }
            int_base(v).map(|n| Value::Uint(n as u64))
        }
        CastTarget::F32 => {
            if let Value::Float(f) = v {
                return Some(Value::Float(f64::from(*f as f32)));
            }
            int_base(v).map(|n| Value::Float(f64::from(n as f32)))
        }
        CastTarget::F64 => {
            if let Value::Float(f) = v {
                return Some(Value::Float(*f));
            }
            int_base(v).map(|n| Value::Float(n as f64))
        }
        CastTarget::F32FromUnsigned => {
            int_base(v).map(|n| Value::Float(f64::from(n as u64 as f32)))
        }
        CastTarget::F64FromUnsigned => int_base(v).map(|n| Value::Float(n as u64 as f64)),
        CastTarget::Char => {
            // Any int source reads its low byte (the same masking
            // `u8 as char` applies), matching the compiled tiers, so
            // `s[i] as char` needs no `as u8` intermediate. The masked
            // value is always a valid code point.
            int_base(v).and_then(|n| char::from_u32((n & 0xFF) as u32).map(Value::Char))
        }
        CastTarget::Bool => match v {
            Value::Bool(b) => Some(Value::Bool(*b)),
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cast_int(v: &Value, target: CastTarget) -> i64 {
        match cast_scalar(v, target) {
            Some(Value::Int(n)) => n,
            other => panic!("expected Int result, got {other:?}"),
        }
    }

    #[test]
    fn float_to_int_saturates_at_the_target_range() {
        let unsigned = CastTarget::IntNarrow {
            width: 8,
            signed: false,
        };
        let signed = CastTarget::IntNarrow {
            width: 8,
            signed: true,
        };
        assert_eq!(cast_int(&Value::Float(300.7), unsigned), 255);
        assert_eq!(cast_int(&Value::Float(-1.5), unsigned), 0);
        assert_eq!(cast_int(&Value::Float(300.7), signed), 127);
        assert_eq!(cast_int(&Value::Float(-300.7), signed), -128);
        assert_eq!(cast_int(&Value::Float(5.9), unsigned), 5);
        assert_eq!(cast_int(&Value::Float(1e20), CastTarget::I64), i64::MAX);
        assert_eq!(cast_int(&Value::Float(f64::NAN), CastTarget::I64), 0);
        assert_eq!(cast_int(&Value::Float(-3.9), CastTarget::I64), -3);
    }

    #[test]
    fn int_to_narrow_truncates_and_extends() {
        let u8_t = CastTarget::IntNarrow {
            width: 8,
            signed: false,
        };
        let i8_t = CastTarget::IntNarrow {
            width: 8,
            signed: true,
        };
        assert_eq!(cast_int(&Value::Int(300), u8_t), 44);
        assert_eq!(cast_int(&Value::Int(200), i8_t), -56);
    }

    #[test]
    fn bool_and_char_to_int() {
        assert_eq!(cast_int(&Value::Bool(true), CastTarget::I64), 1);
        assert_eq!(cast_int(&Value::Bool(false), CastTarget::I64), 0);
        assert_eq!(cast_int(&Value::Char('A'), CastTarget::I64), 65);
    }

    #[test]
    fn u8_to_char_masks_declared_width() {
        let as_char = |n: i64| match cast_scalar(&Value::Int(n), CastTarget::Char) {
            Some(Value::Char(c)) => c,
            other => panic!("expected Char result, got {other:?}"),
        };
        assert_eq!(as_char(65), 'A');
        assert_eq!(as_char(321), 'A');
    }

    #[test]
    fn f32_target_rounds_through_f32_precision() {
        match cast_scalar(&Value::Float(0.1), CastTarget::F32) {
            Some(Value::Float(f)) => assert!((f - f64::from(0.1f32)).abs() < f64::EPSILON),
            other => panic!("expected Float result, got {other:?}"),
        }
    }
}
