#![allow(
    unused_imports,
    dead_code,
    unreachable_pub,
    missing_docs,
    clippy::wildcard_imports,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::similar_names,
    clippy::many_single_char_names,
    clippy::items_after_statements,
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::option_if_let_else,
    clippy::match_same_arms,
    clippy::if_not_else,
    clippy::single_match_else,
    clippy::needless_pass_by_value,
    clippy::manual_let_else,
    clippy::redundant_else,
    clippy::collapsible_if,
    clippy::collapsible_else_if,
    clippy::map_unwrap_or,
    clippy::struct_excessive_bools,
    clippy::module_name_repetitions,
    clippy::unnecessary_wraps,
    clippy::large_enum_variant,
    clippy::if_same_then_else,
    clippy::single_match,
    clippy::useless_conversion,
    clippy::needless_borrows_for_generic_args,
    clippy::let_and_return,
    unsafe_op_in_unsafe_fn,
    unsafe_code,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks,
    clippy::needless_collect,
    clippy::elidable_lifetime_names,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::missing_const_for_fn,
    clippy::needless_range_loop,
    clippy::cognitive_complexity,
    clippy::unused_io_amount,
    clippy::ptr_arg,
    clippy::ptr_as_ptr,
    clippy::redundant_closure,
    clippy::redundant_closure_for_method_calls,
    clippy::semicolon_if_nothing_returned,
    clippy::single_call_fn,
    clippy::unused_self,
    clippy::range_plus_one,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::cast_ptr_alignment,
    clippy::manual_assert,
    clippy::manual_string_new,
    clippy::match_bool,
    clippy::nonminimal_bool,
    clippy::redundant_pattern_matching,
    clippy::useless_let_if_seq
)]
//! Wires up Gossamer-callable builtins for stdlib modules whose
//! Rust-side implementation already exists but had no user-facing
//! exposure. Each `install_*` helper is invoked from
//! `builtins::install` so user code that writes
//! `strings::join`, `strconv::parse_i64`, `net::TcpStream::connect`,
//! `time::Instant::now`, etc. resolves to a real callable.
//!
//! All builtins return a `Result`-shaped variant (`Ok` / `Err`) on
//! fallible operations so callers can chain `?` without wrapping.

use std::cell::RefCell;
use std::collections::HashMap as StdHashMap;
use std::io::Read as IoRead;
use std::sync::Arc;

use gossamer_ast::Ident;
use std::sync::atomic::{AtomicBool as StdAtomicBool, AtomicI64 as StdAtomicI64, Ordering};

use crate::value::SmolStr;

use gossamer_std::bufio as bufio_std;
use gossamer_std::math as math_std;
#[cfg(not(target_arch = "wasm32"))]
use gossamer_std::net as net_std;
use gossamer_std::os as os_std;
use gossamer_std::path as path_std;
use gossamer_std::strconv as strconv_std;
use gossamer_std::strings as strings_std;
use gossamer_std::unicode as unicode_std;
use gossamer_std::utf8 as utf8_std;

use gossamer_std::iter as iter_std;
use gossamer_std::utf16 as utf16_std;

use crate::builtins::{
    BuiltinFnPub, as_str, err_variant, install_module_pub, none_variant, ok_variant, some_variant,
    value_to_int,
};
use crate::value::{MapKey, NativeCall, NativeDispatch, RuntimeResult, Value};

/// Entry point invoked from `builtins::install`.
use super::*;

pub(crate) fn install_math(globals: &mut Vec<(&'static str, Value)>) {
    install_module_pub(
        "math",
        &[
            ("abs", builtin_math_abs),
            ("sqrt", builtin_math_sqrt),
            ("cbrt", builtin_math_cbrt),
            ("floor", builtin_math_floor),
            ("ceil", builtin_math_ceil),
            ("round", builtin_math_round),
            ("trunc", builtin_math_trunc),
            ("sin", builtin_math_sin),
            ("cos", builtin_math_cos),
            ("tan", builtin_math_tan),
            ("asin", builtin_math_asin),
            ("acos", builtin_math_acos),
            ("atan", builtin_math_atan),
            ("atan2", builtin_math_atan2),
            ("sinh", builtin_math_sinh),
            ("cosh", builtin_math_cosh),
            ("tanh", builtin_math_tanh),
            ("exp", builtin_math_exp),
            ("exp2", builtin_math_exp2),
            ("ln", builtin_math_ln),
            ("log2", builtin_math_log2),
            ("log10", builtin_math_log10),
            ("log", builtin_math_log),
            ("pow", builtin_math_pow),
            ("hypot", builtin_math_hypot),
            ("min", builtin_math_min),
            ("max", builtin_math_max),
            ("clamp", builtin_math_clamp),
            ("rem", builtin_math_fmod),
            ("is_nan", builtin_math_is_nan),
            ("is_inf", builtin_math_is_inf),
            ("copysign", builtin_math_copysign),
            ("positive_diff", builtin_math_dim),
        ],
        globals,
    );
    // Expose constants as float globals.
    for (name, val) in [
        ("math::PI", math_std::PI),
        ("math::E", math_std::E),
        ("math::SQRT_2", math_std::SQRT_2),
        ("math::LN_2", math_std::LN_2),
        ("math::LN_10", math_std::LN_10),
        ("math::LOG2_E", math_std::LOG2_E),
        ("math::LOG10_E", math_std::LOG10_E),
        ("math::PHI", math_std::PHI),
        ("math::MAX_F64", math_std::MAX_F64),
        ("math::MIN_POSITIVE_F64", math_std::MIN_POSITIVE_F64),
        ("math::INF", math_std::INF),
        ("math::NEG_INF", math_std::NEG_INF),
        ("math::NAN", f64::NAN),
    ] {
        let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
        globals.push((leaked, Value::Float(val)));
    }
}

pub(crate) fn arg_f64(args: &[Value], idx: usize) -> f64 {
    match args.get(idx) {
        Some(Value::Float(f)) => *f,
        Some(Value::Int(n)) => *n as f64,
        Some(Value::Uint(n)) => *n as f64,
        _ => 0.0,
    }
}

pub(crate) fn builtin_math_abs(args: &[Value]) -> RuntimeResult<Value> {
    match args.first() {
        Some(Value::Int(n)) => return Ok(Value::Int(math_std::abs_i64(*n))),
        // An unsigned value is its own magnitude.
        Some(Value::Uint(n)) => return Ok(Value::Uint(*n)),
        _ => {}
    }
    Ok(Value::Float(math_std::abs(arg_f64(args, 0))))
}
pub(crate) fn builtin_math_sqrt(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_std::sqrt(arg_f64(args, 0))))
}
pub(crate) fn builtin_math_cbrt(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_std::cbrt(arg_f64(args, 0))))
}
pub(crate) fn builtin_math_floor(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_std::floor(arg_f64(args, 0))))
}
pub(crate) fn builtin_math_ceil(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_std::ceil(arg_f64(args, 0))))
}
pub(crate) fn builtin_math_round(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_std::round(arg_f64(args, 0))))
}
pub(crate) fn builtin_math_trunc(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_std::trunc(arg_f64(args, 0))))
}
pub(crate) fn builtin_math_sin(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_std::sin(arg_f64(args, 0))))
}
pub(crate) fn builtin_math_cos(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_std::cos(arg_f64(args, 0))))
}
pub(crate) fn builtin_math_tan(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_std::tan(arg_f64(args, 0))))
}
pub(crate) fn builtin_math_asin(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_std::asin(arg_f64(args, 0))))
}
pub(crate) fn builtin_math_acos(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_std::acos(arg_f64(args, 0))))
}
pub(crate) fn builtin_math_atan(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_std::atan(arg_f64(args, 0))))
}
pub(crate) fn builtin_math_atan2(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_std::atan2(
        arg_f64(args, 0),
        arg_f64(args, 1),
    )))
}
pub(crate) fn builtin_math_sinh(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_std::sinh(arg_f64(args, 0))))
}
pub(crate) fn builtin_math_cosh(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_std::cosh(arg_f64(args, 0))))
}
pub(crate) fn builtin_math_tanh(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_std::tanh(arg_f64(args, 0))))
}
pub(crate) fn builtin_math_exp(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_std::exp(arg_f64(args, 0))))
}
pub(crate) fn builtin_math_exp2(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_std::exp2(arg_f64(args, 0))))
}
pub(crate) fn builtin_math_ln(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_std::ln(arg_f64(args, 0))))
}
pub(crate) fn builtin_math_log2(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_std::log2(arg_f64(args, 0))))
}
pub(crate) fn builtin_math_log10(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_std::log10(arg_f64(args, 0))))
}
pub(crate) fn builtin_math_log(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_std::log(
        arg_f64(args, 0),
        arg_f64(args, 1),
    )))
}
pub(crate) fn builtin_math_pow(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_std::pow(
        arg_f64(args, 0),
        arg_f64(args, 1),
    )))
}
pub(crate) fn builtin_math_hypot(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_std::hypot(
        arg_f64(args, 0),
        arg_f64(args, 1),
    )))
}
pub(crate) fn builtin_math_clamp(args: &[Value]) -> RuntimeResult<Value> {
    // clamp(v, lo, hi) - bare prelude scalar clamp. Matches the
    // compiled tier: below lo -> lo, above hi -> hi, else v.
    if let Some(Value::Float(_)) = args.first() {
        let v = arg_f64(args, 0);
        let lo = arg_f64(args, 1);
        let hi = arg_f64(args, 2);
        let out = if v < lo {
            lo
        } else if v > hi {
            hi
        } else {
            v
        };
        return Ok(Value::Float(out));
    }
    let v = args.first().and_then(value_to_int).unwrap_or(0);
    let lo = args.get(1).and_then(value_to_int).unwrap_or(0);
    let hi = args.get(2).and_then(value_to_int).unwrap_or(0);
    let out = if v < lo {
        lo
    } else if v > hi {
        hi
    } else {
        v
    };
    Ok(Value::Int(out))
}

pub(crate) fn builtin_math_min(args: &[Value]) -> RuntimeResult<Value> {
    // Bare `min(xs)` over a single Vec/array argument is the Option-returning
    // reduction (`Some(smallest)` / `None`), not the two-arg scalar compare.
    if args.len() == 1 {
        return super::iter::builtin_iter_min(args);
    }
    if let Some(Value::Float(_)) = args.first() {
        return Ok(Value::Float(math_std::min_f64(
            arg_f64(args, 0),
            arg_f64(args, 1),
        )));
    }
    if let (Some(Value::Char(a)), Some(Value::Char(b))) = (args.first(), args.get(1)) {
        return Ok(Value::Char(*a.min(b)));
    }
    let x = args.first().and_then(value_to_int).unwrap_or(0);
    let y = args.get(1).and_then(value_to_int).unwrap_or(0);
    Ok(Value::Int(math_std::min_i64(x, y)))
}
pub(crate) fn builtin_math_max(args: &[Value]) -> RuntimeResult<Value> {
    // Bare `max(xs)` over a single Vec/array argument is the Option-returning
    // reduction (`Some(largest)` / `None`), not the two-arg scalar compare.
    if args.len() == 1 {
        return super::iter::builtin_iter_max(args);
    }
    if let Some(Value::Float(_)) = args.first() {
        return Ok(Value::Float(math_std::max_f64(
            arg_f64(args, 0),
            arg_f64(args, 1),
        )));
    }
    if let (Some(Value::Char(a)), Some(Value::Char(b))) = (args.first(), args.get(1)) {
        return Ok(Value::Char(*a.max(b)));
    }
    let x = args.first().and_then(value_to_int).unwrap_or(0);
    let y = args.get(1).and_then(value_to_int).unwrap_or(0);
    Ok(Value::Int(math_std::max_i64(x, y)))
}
pub(crate) fn builtin_math_min_f64(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_std::min_f64(
        arg_f64(args, 0),
        arg_f64(args, 1),
    )))
}
pub(crate) fn builtin_math_max_f64(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_std::max_f64(
        arg_f64(args, 0),
        arg_f64(args, 1),
    )))
}
pub(crate) fn builtin_math_min_i64(args: &[Value]) -> RuntimeResult<Value> {
    let x = args.first().and_then(value_to_int).unwrap_or(0);
    let y = args.get(1).and_then(value_to_int).unwrap_or(0);
    Ok(Value::Int(math_std::min_i64(x, y)))
}
pub(crate) fn builtin_math_max_i64(args: &[Value]) -> RuntimeResult<Value> {
    let x = args.first().and_then(value_to_int).unwrap_or(0);
    let y = args.get(1).and_then(value_to_int).unwrap_or(0);
    Ok(Value::Int(math_std::max_i64(x, y)))
}
pub(crate) fn builtin_math_abs_i64(args: &[Value]) -> RuntimeResult<Value> {
    let x = args.first().and_then(value_to_int).unwrap_or(0);
    Ok(Value::Int(math_std::abs_i64(x)))
}
pub(crate) fn builtin_math_fmod(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_std::fmod(
        arg_f64(args, 0),
        arg_f64(args, 1),
    )))
}
pub(crate) fn builtin_math_is_nan(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Bool(math_std::is_nan(arg_f64(args, 0))))
}
pub(crate) fn builtin_math_is_inf(args: &[Value]) -> RuntimeResult<Value> {
    let x = arg_f64(args, 0);
    let sign = args.get(1).and_then(value_to_int).unwrap_or(0);
    Ok(Value::Bool(math_std::is_inf(x, sign)))
}
pub(crate) fn builtin_math_nan(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_std::nan()))
}
pub(crate) fn builtin_math_inf(args: &[Value]) -> RuntimeResult<Value> {
    let sign = args.first().and_then(value_to_int).unwrap_or(0);
    Ok(Value::Float(math_std::inf(sign)))
}
pub(crate) fn builtin_math_copysign(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_std::copysign(
        arg_f64(args, 0),
        arg_f64(args, 1),
    )))
}
pub(crate) fn builtin_math_dim(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(math_std::dim(
        arg_f64(args, 0),
        arg_f64(args, 1),
    )))
}

// ----------------------------------------------------------------------
// math::bits

/// The bit pattern of an integer argument. `x as u64` produces a
/// [`Value::Uint`], which carries the same 64 bits an [`Value::Int`] would,
/// so both spell the operand a bit intrinsic acts on.
pub(crate) fn arg_u64(args: &[Value], idx: usize) -> u64 {
    match args.get(idx) {
        Some(Value::Int(n)) => *n as u64,
        Some(Value::Uint(n)) => *n,
        _ => 0,
    }
}
