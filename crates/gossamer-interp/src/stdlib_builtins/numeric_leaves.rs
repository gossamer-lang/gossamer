//! Interpreter entries for the `netip::Prefix` and `math::Rational`
//! wrapper leaves, over the runtime code the compiled tiers call through
//! `gos_rt_netip_*` and `gos_rt_rational_*`.

use std::sync::Arc;

use crate::builtins::{BuiltinFnPub, as_str, err_variant, ok_variant, value_to_int};
use crate::value::{RuntimeResult, Value};

fn text(args: &[Value], n: usize) -> String {
    args.get(n).and_then(as_str).unwrap_or("").to_string()
}

fn int(args: &[Value], n: usize) -> i64 {
    args.get(n).and_then(value_to_int).unwrap_or(0)
}

pub(crate) fn install_numeric_leaves(globals: &mut Vec<(&'static str, Value)>) {
    let entries: &[(&'static str, BuiltinFnPub)] = &[
        ("__gos_netip_prefix_raw", vm_netip_prefix_raw),
        ("__gos_netip_prefix_op_raw", vm_netip_prefix_op_raw),
        (
            "__gos_netip_prefix_contains_raw",
            vm_netip_prefix_contains_raw,
        ),
        (
            "__gos_netip_prefix_overlaps_raw",
            vm_netip_prefix_overlaps_raw,
        ),
        ("__gos_rational_from_f64_raw", vm_rational_from_f64_raw),
        ("__gos_rational_to_f64_raw", vm_rational_to_f64_raw),
    ];
    for (name, call) in entries {
        globals.push((name, crate::builtins::builtin_pub(name, *call)));
    }
}

/// `__gos_netip_prefix_raw(text)`.
fn vm_netip_prefix_raw(args: &[Value]) -> RuntimeResult<Value> {
    Ok(
        match gossamer_runtime::netip_prefix::parse(&text(args, 0)) {
            Ok((addr, len)) => ok_variant(Value::Tuple(Arc::from(vec![
                Value::String(addr.into()),
                Value::Int(len),
            ]))),
            Err(e) => err_variant(e),
        },
    )
}

/// `__gos_netip_prefix_op_raw(op, addr, len)`.
fn vm_netip_prefix_op_raw(args: &[Value]) -> RuntimeResult<Value> {
    Ok(
        match gossamer_runtime::netip_prefix::op(int(args, 0), &text(args, 1), int(args, 2)) {
            Ok(answer) => ok_variant(Value::String(answer.into())),
            Err(e) => err_variant(e),
        },
    )
}

/// `__gos_netip_prefix_contains_raw(addr, len, candidate)`.
fn vm_netip_prefix_contains_raw(args: &[Value]) -> RuntimeResult<Value> {
    Ok(ok_variant(Value::Bool(
        gossamer_runtime::netip_prefix::contains(&text(args, 0), int(args, 1), &text(args, 2)),
    )))
}

/// `__gos_netip_prefix_overlaps_raw(a, a_len, b, b_len)`.
fn vm_netip_prefix_overlaps_raw(args: &[Value]) -> RuntimeResult<Value> {
    Ok(ok_variant(Value::Bool(
        gossamer_runtime::netip_prefix::overlaps(
            &text(args, 0),
            int(args, 1),
            &text(args, 2),
            int(args, 3),
        ),
    )))
}

/// `__gos_rational_from_f64_raw(x)`.
fn vm_rational_from_f64_raw(args: &[Value]) -> RuntimeResult<Value> {
    let x = match args.first() {
        Some(Value::Float(x)) => *x,
        _ => f64::NAN,
    };
    Ok(match gossamer_runtime::rational::from_f64(x) {
        Ok((numerator, denominator)) => ok_variant(Value::Tuple(Arc::from(vec![
            Value::String(numerator.into()),
            Value::String(denominator.into()),
        ]))),
        Err(e) => err_variant(e),
    })
}

/// `__gos_rational_to_f64_raw(numerator, denominator)`.
fn vm_rational_to_f64_raw(args: &[Value]) -> RuntimeResult<Value> {
    Ok(
        match gossamer_runtime::rational::to_f64(&text(args, 0), &text(args, 1)) {
            Ok(value) => ok_variant(Value::String(format!("{value:?}").into())),
            Err(e) => err_variant(e),
        },
    )
}
