//! Lowering `math` free functions.

use super::*;

impl<'a> Builder<'a> {
    pub(super) fn lower_math_free(
        &mut self,
        joined: &str,
        args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "f64::to_bits" => (
                "gos_rt_f64_to_bits",
                self.tcx.int_ty(gossamer_types::IntTy::U64),
            ),
            "f64::from_bits" => (
                "gos_rt_f64_from_bits",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "f32::to_bits" => (
                "gos_rt_f32_to_bits",
                self.tcx.int_ty(gossamer_types::IntTy::U32),
            ),
            "f32::from_bits" => (
                "gos_rt_f32_from_bits",
                self.tcx.float_ty(gossamer_types::FloatTy::F32),
            ),
            // 0.10.0 - math::bits::* scalar primitives previously
            // VM-only. The carrying add/sub/mul/div (tuple returns)
            // stay on the VM until aggregate-return ABI lands.
            "math::bits::count_ones" => (
                "gos_rt_bits_count_ones",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "math::bits::count_zeros" => (
                "gos_rt_bits_count_zeros",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "math::bits::leading_zeros" => (
                "gos_rt_bits_leading_zeros",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "math::bits::trailing_zeros" => (
                "gos_rt_bits_trailing_zeros",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "math::bits::reverse_bits" => (
                "gos_rt_bits_reverse_bits",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "math::bits::reverse_bytes" => (
                "gos_rt_bits_reverse_bytes",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "math::bits::len" => (
                "gos_rt_bits_len",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "math::bits::rotate_left" => (
                "gos_rt_bits_rotate_left",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "math::bits::rotate_right" => (
                "gos_rt_bits_rotate_right",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            // 0.10.0 - carrying primitives return (i64, i64) via the
            // by-value-aggregate ABI (heap pointer + caller memcpy).
            "math::bits::add" | "math::bits::sub" | "math::bits::div" => {
                let i = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let tup = self.tcx.intern(gossamer_types::TyKind::Tuple(vec![i, i]));
                let sym = match joined {
                    "math::bits::add" => "gos_rt_bits_add",
                    "math::bits::sub" => "gos_rt_bits_sub",
                    _ => "gos_rt_bits_div",
                };
                (sym, tup)
            }
            "math::bits::mul" => {
                let i = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let tup = self.tcx.intern(gossamer_types::TyKind::Tuple(vec![i, i]));
                ("gos_rt_bits_mul", tup)
            }
            // 0.10.0 - math extended trig / log / round entries.
            "math::tan" => (
                "gos_rt_math_tan",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::asin" => (
                "gos_rt_math_asin",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::acos" => (
                "gos_rt_math_acos",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::atan" => (
                "gos_rt_math_atan",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "__gos_debug_quote" if args.len() == 1 => {
                let helper = if matches!(
                    self.tcx.kind_of(self.peel_ref_ty(args[0].ty)),
                    gossamer_types::TyKind::Char
                ) {
                    "gos_rt_debug_quote_char"
                } else {
                    "gos_rt_debug_quote_str"
                };
                (helper, self.tcx.string_ty())
            }
            "__gos_f32_display" if args.len() == 1 => ("gos_rt_f32_to_str", self.tcx.string_ty()),
            "__gos_dyn_display" if args.len() == 1 => ("gos_rt_dyn_display", self.tcx.string_ty()),
            "__gos_dyn_debug" if args.len() == 1 => ("gos_rt_dyn_format", self.tcx.string_ty()),
            "__gos_f32_debug" if args.len() == 1 => {
                ("gos_rt_f32_debug_to_str", self.tcx.string_ty())
            }
            "math::log" => (
                "gos_rt_math_log_base",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::atan2" => (
                "gos_rt_math_atan2",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::sinh" => (
                "gos_rt_math_sinh",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::cosh" => (
                "gos_rt_math_cosh",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            _ => return None,
        })
    }

    pub(super) fn lower_math_2_free(
        &mut self,
        joined: &str,
        args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "math::abs" if args.len() == 1 => {
                if arg_is_float(self.tcx, &args[0]) {
                    (
                        "gos_rt_math_abs",
                        self.tcx.float_ty(gossamer_types::FloatTy::F64),
                    )
                } else {
                    (
                        "gos_rt_math_abs_i64",
                        self.tcx.int_ty(gossamer_types::IntTy::I64),
                    )
                }
            }
            "math::tanh" => (
                "gos_rt_math_tanh",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::log2" => (
                "gos_rt_math_log2",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::log10" => (
                "gos_rt_math_log10",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::cbrt" => (
                "gos_rt_math_cbrt",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::round" => (
                "gos_rt_math_round",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::exp2" => (
                "gos_rt_math_exp2",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::rem" => (
                "gos_rt_math_fmod",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::hypot" => (
                "gos_rt_math_hypot",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::copysign" => (
                "gos_rt_math_copysign",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::positive_diff" => (
                "gos_rt_math_dim",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::trunc" => (
                "gos_rt_math_trunc",
                self.tcx.float_ty(gossamer_types::FloatTy::F64),
            ),
            "math::is_nan" => ("gos_rt_math_is_nan", self.tcx.bool_ty()),
            "math::is_inf" => ("gos_rt_math_is_inf", self.tcx.bool_ty()),
            // 0.10.0 - arbitrary-precision big integers. Every value
            // is carried as a decimal `String` (matching the interp),
            // so all the arithmetic entries take/return `String`.
            "math::big::factorial" => ("gos_rt_math_big_factorial", self.tcx.string_ty()),
            _ => return None,
        })
    }

    pub(super) fn lower_math_3_free(
        &mut self,
        joined: &str,
        args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "math::big::int_from_i64" => ("gos_rt_math_big_int_from_i64", self.tcx.string_ty()),
            "math::big::int_from_str" => (
                "gos_rt_math_big_int_from_str",
                self.result_string_error_adt_ty(),
            ),
            "math::big::int_to_str" => ("gos_rt_math_big_int_to_str", self.tcx.string_ty()),
            "math::big::int_to_hex" => ("gos_rt_math_big_int_to_hex", self.tcx.string_ty()),
            "math::big::int_to_i64" => ("gos_rt_math_big_int_to_i64", self.option_i64_adt_ty()),
            "math::big::int_is_zero" => ("gos_rt_math_big_int_is_zero", self.tcx.bool_ty()),
            "math::big::int_is_positive" => ("gos_rt_math_big_int_is_positive", self.tcx.bool_ty()),
            "math::big::int_is_negative" => ("gos_rt_math_big_int_is_negative", self.tcx.bool_ty()),
            "math::big::int_add" => ("gos_rt_math_big_int_add", self.tcx.string_ty()),
            "math::big::int_sub" => ("gos_rt_math_big_int_sub", self.tcx.string_ty()),
            "math::big::int_mul" => ("gos_rt_math_big_int_mul", self.tcx.string_ty()),
            "math::big::int_div" => ("gos_rt_math_big_int_div", self.result_string_error_adt_ty()),
            "math::big::int_rem" => ("gos_rt_math_big_int_rem", self.result_string_error_adt_ty()),
            "math::big::int_pow" => ("gos_rt_math_big_int_pow", self.tcx.string_ty()),
            "math::big::int_abs" => ("gos_rt_math_big_int_abs", self.tcx.string_ty()),
            "math::big::int_neg" => ("gos_rt_math_big_int_neg", self.tcx.string_ty()),
            "math::big::int_gcd" => ("gos_rt_math_big_int_gcd", self.tcx.string_ty()),
            "math::big::int_lcm" => ("gos_rt_math_big_int_lcm", self.tcx.string_ty()),
            "math::big::int_cmp" => (
                "gos_rt_math_big_int_cmp",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            "math::big::uint_from_u64" => ("gos_rt_math_big_uint_from_u64", self.tcx.string_ty()),
            "math::big::uint_from_str" => (
                "gos_rt_math_big_uint_from_str",
                self.result_string_error_adt_ty(),
            ),
            "math::big::uint_to_str" => ("gos_rt_math_big_uint_to_str", self.tcx.string_ty()),
            "math::big::uint_to_hex" => ("gos_rt_math_big_uint_to_hex", self.tcx.string_ty()),
            "math::big::uint_to_u64" => ("gos_rt_math_big_uint_to_u64", self.option_i64_adt_ty()),
            "math::big::uint_is_zero" => ("gos_rt_math_big_uint_is_zero", self.tcx.bool_ty()),
            "math::big::uint_add" => ("gos_rt_math_big_uint_add", self.tcx.string_ty()),
            "math::big::uint_mul" => ("gos_rt_math_big_uint_mul", self.tcx.string_ty()),
            "math::big::uint_pow" => ("gos_rt_math_big_uint_pow", self.tcx.string_ty()),
            "math::big::uint_pow_mod" => ("gos_rt_math_big_uint_pow_mod", self.tcx.string_ty()),
            "math::big::uint_bit_len" => (
                "gos_rt_math_big_uint_bit_len",
                self.tcx.int_ty(gossamer_types::IntTy::I64),
            ),
            // 0.7.0 scalar cmp prelude - `min(a, b)` / `max(a, b)`
            // / `clamp(x, lo, hi)`. Two-arg shape dispatches by
            // first-arg HIR type to the i64 or f64 variant; the
            // Vec-shaped `min(xs)` / `max(xs)` fallback hits the
            // bare-name dispatch later (single-arg shape is *not*
            // matched here).
            "min" | "math::min" if args.len() == 2 => {
                if let Some(unsigned) = args.iter().find(|a| arg_is_unsigned64(self.tcx, a)) {
                    return Some(("gos_rt_min_u64", unsigned.ty));
                }
                let is_f = arg_is_float(self.tcx, &args[0]);
                let sym = if is_f {
                    "gos_rt_min_f64"
                } else {
                    "gos_rt_min_i64"
                };
                let ret = if is_f {
                    self.tcx.float_ty(gossamer_types::FloatTy::F64)
                } else if arg_is_char(self.tcx, &args[0]) {
                    // Codepoint compares as i64 via `gos_rt_*_i64`, but the
                    // result is a `char` and must render as one, not its int.
                    self.tcx.char_ty()
                } else {
                    self.tcx.int_ty(gossamer_types::IntTy::I64)
                };
                (sym, ret)
            }
            "max" | "math::max" if args.len() == 2 => {
                if let Some(unsigned) = args.iter().find(|a| arg_is_unsigned64(self.tcx, a)) {
                    return Some(("gos_rt_max_u64", unsigned.ty));
                }
                let is_f = arg_is_float(self.tcx, &args[0]);
                let sym = if is_f {
                    "gos_rt_max_f64"
                } else {
                    "gos_rt_max_i64"
                };
                let ret = if is_f {
                    self.tcx.float_ty(gossamer_types::FloatTy::F64)
                } else if arg_is_char(self.tcx, &args[0]) {
                    // Codepoint compares as i64 via `gos_rt_*_i64`, but the
                    // result is a `char` and must render as one, not its int.
                    self.tcx.char_ty()
                } else {
                    self.tcx.int_ty(gossamer_types::IntTy::I64)
                };
                (sym, ret)
            }
            _ => return None,
        })
    }

    pub(super) fn lower_math_4_free(
        &mut self,
        joined: &str,
        args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "clamp" | "math::clamp" if args.len() == 3 => {
                if let Some(unsigned) = args.iter().find(|a| arg_is_unsigned64(self.tcx, a)) {
                    return Some(("gos_rt_clamp_u64", unsigned.ty));
                }
                let is_f = arg_is_float(self.tcx, &args[0]);
                let sym = if is_f {
                    "gos_rt_clamp_f64"
                } else {
                    "gos_rt_clamp_i64"
                };
                let ret = if is_f {
                    self.tcx.float_ty(gossamer_types::FloatTy::F64)
                } else if arg_is_char(self.tcx, &args[0]) {
                    // Codepoint compares as i64 via `gos_rt_*_i64`, but the
                    // result is a `char` and must render as one, not its int.
                    self.tcx.char_ty()
                } else {
                    self.tcx.int_ty(gossamer_types::IntTy::I64)
                };
                (sym, ret)
            }
            _ => return None,
        })
    }
}
