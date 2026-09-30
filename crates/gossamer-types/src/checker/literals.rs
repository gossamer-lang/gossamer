//! Integer literal parsing, bounds, and suffixes.

use super::{Expr, ExprKind, FloatTy, IntTy, IntWidth, Literal};

pub(super) fn evaluate_const_int_from_expr(expr: &Expr) -> Option<u128> {
    if let ExprKind::Literal(Literal::Int(text)) = &expr.kind {
        let cleaned = strip_int_suffix(text).replace('_', "");
        return parse_int(&cleaned);
    }
    None
}

/// Parses the magnitude of an integer literal as a `u128`. The leading
/// `-` is dropped before parsing; callers that care about signedness
/// must apply it externally. Returns `None` for non-parseable text.
pub(super) fn parse_int_magnitude(text: &str) -> Option<u128> {
    let cleaned = strip_int_suffix(text).replace('_', "");
    let trimmed = cleaned.strip_prefix('-').unwrap_or(&cleaned);
    parse_int(trimmed)
}

/// Returns `true` when the unsigned magnitude of `text` fits in the
/// declared integer type. Treats a leading `-` as the literal's
/// negation, applying the signed-range bound from the negative side.
pub(super) fn int_literal_fits(text: &str, ty: IntTy) -> bool {
    let Some(magnitude) = parse_int_magnitude(text) else {
        return false;
    };
    let negative = text.trim_start().starts_with('-');
    let (signed_max, signed_min_abs, unsigned_max) = int_bounds(ty);
    if let Some(unsigned_max) = unsigned_max {
        if negative {
            return false;
        }
        return magnitude <= unsigned_max;
    }
    let limit = if negative { signed_min_abs } else { signed_max };
    magnitude <= limit
}

/// Returns `(signed_max, signed_min_abs, unsigned_max)` for `ty`. The
/// unsigned slot is `None` for signed widths. The signed-minimum is
/// stored as a positive magnitude (`-i8::MIN` is reported as `128`).
pub(super) fn int_bounds(ty: IntTy) -> (u128, u128, Option<u128>) {
    match ty {
        IntTy::I8 => (i8::MAX as u128, 1u128 << 7, None),
        IntTy::I16 => (i16::MAX as u128, 1u128 << 15, None),
        IntTy::I32 => (i32::MAX as u128, 1u128 << 31, None),
        IntTy::I64 => (i64::MAX as u128, 1u128 << 63, None),
        IntTy::I128 => (i128::MAX as u128, 1u128 << 127, None),
        IntTy::Isize => (i64::MAX as u128, 1u128 << 63, None),
        IntTy::U8 => (0, 0, Some(u128::from(u8::MAX))),
        IntTy::U16 => (0, 0, Some(u128::from(u16::MAX))),
        IntTy::U32 => (0, 0, Some(u128::from(u32::MAX))),
        IntTy::U64 => (0, 0, Some(u128::from(u64::MAX))),
        IntTy::U128 => (0, 0, Some(u128::MAX)),
        IntTy::Usize => (0, 0, Some(u128::from(u64::MAX))),
    }
}

pub(super) fn parse_int(text: &str) -> Option<u128> {
    if let Some(rest) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        return u128::from_str_radix(rest, 16).ok();
    }
    if let Some(rest) = text.strip_prefix("0b").or_else(|| text.strip_prefix("0B")) {
        return u128::from_str_radix(rest, 2).ok();
    }
    if let Some(rest) = text.strip_prefix("0o").or_else(|| text.strip_prefix("0O")) {
        return u128::from_str_radix(rest, 8).ok();
    }
    text.parse::<u128>().ok()
}

pub(super) fn int_assoc_const(segments: &[&str]) -> Option<(IntTy, i128)> {
    if segments.len() != 2 {
        return None;
    }
    let ty = match segments[0] {
        "i8" => IntTy::I8,
        "i16" => IntTy::I16,
        "i32" => IntTy::I32,
        "i64" => IntTy::I64,
        "isize" => IntTy::Isize,
        "u8" => IntTy::U8,
        "u16" => IntTy::U16,
        "u32" => IntTy::U32,
        "u64" => IntTy::U64,
        "usize" => IntTy::Usize,
        _ => return None,
    };
    let value = match (ty, segments[1]) {
        (IntTy::I8, "MIN") => i128::from(i8::MIN),
        (IntTy::I8, "MAX") => i128::from(i8::MAX),
        (IntTy::I16, "MIN") => i128::from(i16::MIN),
        (IntTy::I16, "MAX") => i128::from(i16::MAX),
        (IntTy::I32, "MIN") => i128::from(i32::MIN),
        (IntTy::I32, "MAX") => i128::from(i32::MAX),
        (IntTy::I64 | IntTy::Isize, "MIN") => i128::from(i64::MIN),
        (IntTy::I64 | IntTy::Isize, "MAX") => i128::from(i64::MAX),
        (IntTy::U8, "MIN") => 0,
        (IntTy::U8, "MAX") => i128::from(u8::MAX),
        (IntTy::U16, "MIN") => 0,
        (IntTy::U16, "MAX") => i128::from(u16::MAX),
        (IntTy::U32, "MIN") => 0,
        (IntTy::U32, "MAX") => i128::from(u32::MAX),
        (IntTy::U64 | IntTy::Usize, "MIN") => 0,
        (IntTy::U64 | IntTy::Usize, "MAX") => i128::from(u64::MAX),
        _ => return None,
    };
    Some((ty, value))
}

pub(super) fn strip_int_suffix(text: &str) -> String {
    for (suffix, _) in INT_SUFFIXES {
        if let Some(stripped) = text.strip_suffix(suffix) {
            return stripped.to_string();
        }
    }
    for (suffix, _) in FLOAT_SUFFIXES {
        if let Some(stripped) = text.strip_suffix(suffix) {
            return stripped.to_string();
        }
    }
    text.to_string()
}

pub(super) const INT_SUFFIXES: &[(&str, IntTy)] = &[
    ("i128", IntTy::I128),
    ("u128", IntTy::U128),
    ("isize", IntTy::Isize),
    ("usize", IntTy::Usize),
    ("i64", IntTy::I64),
    ("u64", IntTy::U64),
    ("i32", IntTy::I32),
    ("u32", IntTy::U32),
    ("i16", IntTy::I16),
    ("u16", IntTy::U16),
    ("i8", IntTy::I8),
    ("u8", IntTy::U8),
];

pub(super) const FLOAT_SUFFIXES: &[(&str, FloatTy)] =
    &[("f32", FloatTy::F32), ("f64", FloatTy::F64)];

pub(super) fn int_ty_from_width(width: IntWidth, signed: bool) -> IntTy {
    match (signed, width) {
        (true, IntWidth::W8) => IntTy::I8,
        (true, IntWidth::W16) => IntTy::I16,
        (true, IntWidth::W32) => IntTy::I32,
        (true, IntWidth::W64) => IntTy::I64,
        (true, IntWidth::W128) => IntTy::I128,
        (true, IntWidth::Size) => IntTy::Isize,
        (false, IntWidth::W8) => IntTy::U8,
        (false, IntWidth::W16) => IntTy::U16,
        (false, IntWidth::W32) => IntTy::U32,
        (false, IntWidth::W64) => IntTy::U64,
        (false, IntWidth::W128) => IntTy::U128,
        (false, IntWidth::Size) => IntTy::Usize,
    }
}
