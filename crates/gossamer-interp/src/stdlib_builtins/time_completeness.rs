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
use crate::value::{MapKey, NativeCall, NativeDispatch, RuntimeError, RuntimeResult, Value};

/// Entry point invoked from `builtins::install`.
use super::*;

pub(crate) fn install_time_completeness(globals: &mut Vec<(&'static str, Value)>) {
    install_module_pub(
        "time",
        &[
            ("sleep", builtin_time_sleep),
            ("now", builtin_time_now_unix_ms),
            ("unix_ms", builtin_time_now_unix_ms),
            ("format_rfc3339", builtin_time_format_rfc3339),
            ("parse_rfc3339", builtin_time_parse_rfc3339),
            ("__gos_time_location_raw", builtin_time_location_raw),
            (
                "__gos_time_fixed_location_raw",
                builtin_time_fixed_location_raw,
            ),
            ("__gos_time_civil_raw", builtin_time_civil_raw),
            ("__gos_time_resolve_raw", builtin_time_resolve_raw),
            ("__gos_time_format_in_raw", builtin_time_format_in_raw),
            ("__gos_time_add_date_raw", builtin_time_add_date_raw),
        ],
        globals,
    );
}

#[cfg(not(target_arch = "wasm32"))]
fn time_location(spec: &str) -> Result<gossamer_std::time::tz::Location, String> {
    if spec == "UTC" {
        return Ok(gossamer_std::time::tz::Location::utc());
    }
    if let Some(offset) = spec.strip_prefix("UTC") {
        let sign = if offset.starts_with('-') { -1 } else { 1 };
        let value = offset.trim_start_matches(['+', '-']);
        let (hours, minutes) = value
            .split_once(':')
            .ok_or_else(|| format!("invalid fixed location `{spec}`"))?;
        let seconds = hours
            .parse::<i32>()
            .ok()
            .and_then(|hours| {
                minutes
                    .parse::<i32>()
                    .ok()
                    .map(|minutes| sign * (hours * 3600 + minutes * 60))
            })
            .ok_or_else(|| format!("invalid fixed location `{spec}`"))?;
        return gossamer_std::time::tz::Location::fixed(seconds).map_err(|error| error.to_string());
    }
    gossamer_std::time::tz::Location::lookup(spec).map_err(|error| error.to_string())
}

#[cfg(not(target_arch = "wasm32"))]
fn builtin_time_location_raw(args: &[Value]) -> RuntimeResult<Value> {
    let name = args.first().and_then(as_str).unwrap_or("");
    match time_location(name) {
        Ok(location) => Ok(ok_variant(Value::String(location.name().into()))),
        Err(error) => Ok(err_variant(error)),
    }
}

#[cfg(target_arch = "wasm32")]
fn builtin_time_location_raw(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(err_variant("IANA time zones are unsupported on wasm32"))
}

#[cfg(not(target_arch = "wasm32"))]
fn builtin_time_fixed_location_raw(args: &[Value]) -> RuntimeResult<Value> {
    let offset = args.first().and_then(value_to_int).unwrap_or(i64::MAX);
    match i32::try_from(offset)
        .ok()
        .and_then(|offset| gossamer_std::time::tz::Location::fixed(offset).ok())
    {
        Some(location) => Ok(ok_variant(Value::String(location.name().into()))),
        None => Ok(err_variant(format!(
            "fixed UTC offset {offset} seconds is outside the supported range"
        ))),
    }
}

#[cfg(target_arch = "wasm32")]
fn builtin_time_fixed_location_raw(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(err_variant(
        "fixed civil-time locations are unsupported on wasm32",
    ))
}

#[cfg(not(target_arch = "wasm32"))]
fn builtin_time_civil_raw(args: &[Value]) -> RuntimeResult<Value> {
    let unix_ms = args.first().and_then(value_to_int).unwrap_or(0);
    let spec = args.get(1).and_then(as_str).unwrap_or("");
    let result = time_location(spec).and_then(|location| {
        location
            .civil(gossamer_std::time::SystemTime::from_unix_millis(unix_ms))
            .map_err(|error| error.to_string())
    });
    match result {
        Ok(civil) => Ok(ok_variant(Value::Tuple(Arc::new(vec![
            Value::Int(i64::from(civil.year)),
            Value::Int(i64::from(civil.month)),
            Value::Int(i64::from(civil.day)),
            Value::Int(i64::from(civil.hour)),
            Value::Int(i64::from(civil.minute)),
            Value::Int(i64::from(civil.second)),
            Value::Int(i64::from(civil.nanosecond)),
            Value::Int(i64::from(civil.offset_seconds)),
            Value::Int(i64::from(civil.weekday)),
        ])))),
        Err(error) => Ok(err_variant(error)),
    }
}

#[cfg(target_arch = "wasm32")]
fn builtin_time_civil_raw(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(err_variant("civil time is unsupported on wasm32"))
}

#[cfg(not(target_arch = "wasm32"))]
fn builtin_time_resolve_raw(args: &[Value]) -> RuntimeResult<Value> {
    let spec = args.first().and_then(as_str).unwrap_or("");
    let field = |index| args.get(index).and_then(value_to_int).unwrap_or(-1);
    let converted = || -> Result<gossamer_std::time::tz::CivilTime, String> {
        Ok(gossamer_std::time::tz::CivilTime {
            year: i32::try_from(field(1)).map_err(|_| "year out of range")?,
            month: u32::try_from(field(2)).map_err(|_| "month out of range")?,
            day: u32::try_from(field(3)).map_err(|_| "day out of range")?,
            hour: u32::try_from(field(4)).map_err(|_| "hour out of range")?,
            minute: u32::try_from(field(5)).map_err(|_| "minute out of range")?,
            second: u32::try_from(field(6)).map_err(|_| "second out of range")?,
            nanosecond: u32::try_from(field(7)).map_err(|_| "nanosecond out of range")?,
            offset_seconds: 0,
            weekday: 0,
        })
    };
    let result = time_location(spec).and_then(|location| {
        converted().and_then(|civil| location.resolve(civil).map_err(|error| error.to_string()))
    });
    let tuple = match result {
        Ok(gossamer_std::time::tz::CivilResolution::Gap) => {
            vec![Value::Int(0), Value::Int(0), Value::Int(0)]
        }
        Ok(gossamer_std::time::tz::CivilResolution::Unique(value)) => vec![
            Value::Int(1),
            Value::Int(value.unix_millis()),
            Value::Int(0),
        ],
        Ok(gossamer_std::time::tz::CivilResolution::Fold { earlier, later }) => vec![
            Value::Int(2),
            Value::Int(earlier.unix_millis()),
            Value::Int(later.unix_millis()),
        ],
        Err(error) => return Ok(err_variant(error)),
    };
    Ok(ok_variant(Value::Tuple(Arc::new(tuple))))
}

#[cfg(target_arch = "wasm32")]
fn builtin_time_resolve_raw(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(err_variant("civil time is unsupported on wasm32"))
}

#[cfg(not(target_arch = "wasm32"))]
fn builtin_time_format_in_raw(args: &[Value]) -> RuntimeResult<Value> {
    let layout = args.first().and_then(as_str).unwrap_or("");
    let unix_ms = args.get(1).and_then(value_to_int).unwrap_or(0);
    let spec = args.get(2).and_then(as_str).unwrap_or("");
    let result = time_location(spec).and_then(|location| {
        gossamer_std::time::tz::format_in(
            layout,
            gossamer_std::time::SystemTime::from_unix_millis(unix_ms),
            location,
        )
        .map_err(|error| error.to_string())
    });
    match result {
        Ok(value) => Ok(ok_variant(Value::String(value.into()))),
        Err(error) => Ok(err_variant(error)),
    }
}

#[cfg(target_arch = "wasm32")]
fn builtin_time_format_in_raw(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(err_variant("civil time is unsupported on wasm32"))
}

#[cfg(not(target_arch = "wasm32"))]
fn builtin_time_add_date_raw(args: &[Value]) -> RuntimeResult<Value> {
    let unix_ms = args.first().and_then(value_to_int).unwrap_or(0);
    let spec = args.get(1).and_then(as_str).unwrap_or("");
    let int_arg = |index| {
        args.get(index)
            .and_then(value_to_int)
            .and_then(|value| i32::try_from(value).ok())
            .ok_or_else(|| format!("calendar argument {index} is out of range"))
    };
    let result = time_location(spec).and_then(|location| {
        gossamer_std::time::tz::add_date(
            gossamer_std::time::SystemTime::from_unix_millis(unix_ms),
            location,
            int_arg(2)?,
            int_arg(3)?,
            int_arg(4)?,
        )
        .map(|value| value.unix_millis())
        .map_err(|error| error.to_string())
    });
    match result {
        Ok(value) => Ok(ok_variant(Value::Int(value))),
        Err(error) => Ok(err_variant(error)),
    }
}

#[cfg(target_arch = "wasm32")]
fn builtin_time_add_date_raw(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(err_variant("civil time is unsupported on wasm32"))
}

pub(crate) fn builtin_time_sleep(args: &[Value]) -> RuntimeResult<Value> {
    let ms = args.first().and_then(value_to_int).unwrap_or(0);
    if ms < 0 {
        return Err(RuntimeError::Type(
            "time::sleep: duration_ms must be non-negative".to_string(),
        ));
    }
    let ms = u64::try_from(ms)
        .map_err(|_| RuntimeError::Type("time::sleep: duration_ms is too large".to_string()))?;
    // Sleeping is a cancellation point: a cancelled cohort wakes its
    // children now rather than at the end of the nap they were taking.
    let _elapsed =
        crate::stdlib_builtins::cohort::sleep_cancellable(std::time::Duration::from_millis(ms));
    Ok(Value::Unit)
}

/// `time::sleep(d)` for a wait given as a `time::Duration`, a count of
/// nanoseconds. A negative duration waits not at all.
pub(crate) fn builtin_time_sleep_ns(args: &[Value]) -> RuntimeResult<Value> {
    let ns = args.first().and_then(value_to_int).unwrap_or(0).max(0);
    let _elapsed = crate::stdlib_builtins::cohort::sleep_cancellable(
        std::time::Duration::from_nanos(u64::try_from(ns).unwrap_or(0)),
    );
    Ok(Value::Unit)
}

pub(crate) fn builtin_time_now_unix_ms(_args: &[Value]) -> RuntimeResult<Value> {
    let ms = gossamer_runtime::platform::system_time_now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    Ok(Value::Int(i64::try_from(ms).unwrap_or(i64::MAX)))
}

pub(crate) fn builtin_time_format_rfc3339(args: &[Value]) -> RuntimeResult<Value> {
    let ms = args.first().and_then(value_to_int).unwrap_or(0);
    let st = gossamer_std::time::SystemTime::from_unix_millis(ms);
    match gossamer_std::time::format_rfc3339(st) {
        Ok(s) => Ok(ok_variant(Value::String(s.into()))),
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

pub(crate) fn builtin_time_parse_rfc3339(args: &[Value]) -> RuntimeResult<Value> {
    let s = args.first().and_then(as_str).unwrap_or("").to_string();
    match gossamer_std::time::parse_rfc3339(&s) {
        Ok(st) => {
            // parse_rfc3339 yields a whole-second instant, so signed
            // seconds * 1000 is exact and preserves pre-1970 instants
            // (unix_millis is u128-shaped and clamps them to zero).
            let ms = st.unix_seconds().saturating_mul(1000);
            Ok(ok_variant(Value::Int(ms)))
        }
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

// ----------------------------------------------------------------------
// net::ip builtins
