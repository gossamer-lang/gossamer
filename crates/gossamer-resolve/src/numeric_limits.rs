//! The limit constants every numeric primitive carries: `i64::MIN`,
//! `u8::MAX`, `f64::EPSILON`, and the rest.

#![forbid(unsafe_code)]

use crate::resolutions::{FloatWidth, IntWidth, PrimitiveTy};

/// A limit constant's value, spelled as the literal it folds to.
#[derive(Debug, Clone, PartialEq)]
pub enum LimitLiteral {
    /// Integer literal text every tier's literal parser reads back exactly.
    Int(String),
    /// Float literal text, `inf`, `-inf`, or `NaN` included.
    Float(String),
}

/// A limit constant: the primitive it is typed as, and its value.
#[derive(Debug, Clone, PartialEq)]
pub struct LimitConstant {
    /// Type of the constant; `BITS` is a `u32`, every other one has the
    /// type it bounds.
    pub ty: PrimitiveTy,
    /// The literal the constant folds to.
    pub literal: LimitLiteral,
}

/// The constant `name` of the numeric primitive spelled `ty_name`, if it
/// declares one.
#[must_use]
pub fn limit_constant(ty_name: &str, name: &str) -> Option<LimitConstant> {
    if let Some((width, signed, bits)) = int_shape(ty_name) {
        let prim = if signed {
            PrimitiveTy::Int(width)
        } else {
            PrimitiveTy::UInt(width)
        };
        let literal = match (name, signed) {
            ("MIN", true) => (-(1i128 << (bits - 1))).to_string(),
            ("MAX", true) => ((1i128 << (bits - 1)) - 1).to_string(),
            ("MIN", false) => "0".to_string(),
            // Hex, so a value above `i64::MAX` reads back as its bit pattern.
            ("MAX", false) => format!("0x{:X}", u128::MAX >> (128 - bits)),
            ("BITS", _) => {
                return Some(LimitConstant {
                    ty: PrimitiveTy::UInt(IntWidth::W32),
                    literal: LimitLiteral::Int(bits.to_string()),
                });
            }
            _ => return None,
        };
        return Some(LimitConstant {
            ty: prim,
            literal: LimitLiteral::Int(literal),
        });
    }
    let (width, value) = match ty_name {
        "f64" => (
            FloatWidth::W64,
            match name {
                "MIN" => f64::MIN,
                "MAX" => f64::MAX,
                "EPSILON" => f64::EPSILON,
                "INFINITY" => f64::INFINITY,
                "NEG_INFINITY" => f64::NEG_INFINITY,
                "NAN" => f64::NAN,
                "MIN_POSITIVE" => f64::MIN_POSITIVE,
                _ => return None,
            },
        ),
        "f32" => (
            FloatWidth::W32,
            f64::from(match name {
                "MIN" => f32::MIN,
                "MAX" => f32::MAX,
                "EPSILON" => f32::EPSILON,
                "INFINITY" => f32::INFINITY,
                "NEG_INFINITY" => f32::NEG_INFINITY,
                "NAN" => f32::NAN,
                "MIN_POSITIVE" => f32::MIN_POSITIVE,
                _ => return None,
            }),
        ),
        _ => return None,
    };
    Some(LimitConstant {
        ty: PrimitiveTy::Float(width),
        literal: LimitLiteral::Float(format!("{value:?}")),
    })
}

/// Width, signedness, and bit count of an integer primitive's name.
fn int_shape(name: &str) -> Option<(IntWidth, bool, u32)> {
    Some(match name {
        "i8" => (IntWidth::W8, true, 8),
        "i16" => (IntWidth::W16, true, 16),
        "i32" => (IntWidth::W32, true, 32),
        "i64" => (IntWidth::W64, true, 64),
        "isize" => (IntWidth::Size, true, 64),
        "u8" => (IntWidth::W8, false, 8),
        "u16" => (IntWidth::W16, false, 16),
        "u32" => (IntWidth::W32, false, 32),
        "u64" => (IntWidth::W64, false, 64),
        "usize" => (IntWidth::Size, false, 64),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(ty: &str, name: &str) -> String {
        match limit_constant(ty, name).map(|c| c.literal) {
            Some(LimitLiteral::Int(text)) => text,
            other => panic!("{ty}::{name}: {other:?}"),
        }
    }

    #[test]
    fn integer_limits_match_rust() {
        assert_eq!(int("i64", "MIN"), i64::MIN.to_string());
        assert_eq!(int("i64", "MAX"), i64::MAX.to_string());
        assert_eq!(int("i8", "MIN"), "-128");
        assert_eq!(int("u8", "MAX"), "0xFF");
        assert_eq!(int("u64", "MAX"), "0xFFFFFFFFFFFFFFFF");
        assert_eq!(int("u32", "BITS"), "32");
    }

    #[test]
    fn float_limits_read_back_exactly() {
        for name in ["MIN", "MAX", "EPSILON", "MIN_POSITIVE"] {
            let Some(LimitLiteral::Float(text)) = limit_constant("f32", name).map(|c| c.literal)
            else {
                panic!("f32::{name}");
            };
            let value: f64 = text.parse().expect("parses");
            assert_eq!(f64::from(value as f32), value, "f32::{name} exact");
        }
        assert_eq!(
            limit_constant("f64", "NAN").map(|c| c.literal),
            Some(LimitLiteral::Float("NaN".to_string()))
        );
    }

    #[test]
    fn unknown_names_have_no_constant() {
        assert!(limit_constant("i64", "EPSILON").is_none());
        assert!(limit_constant("String", "MAX").is_none());
    }
}
