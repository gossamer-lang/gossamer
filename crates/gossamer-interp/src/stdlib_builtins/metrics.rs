#![allow(
    unused_imports,
    dead_code,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::missing_errors_doc,
    clippy::unnecessary_wraps,
    clippy::needless_pass_by_value
)]
//! `std::metrics` builtins for the bytecode VM - Prometheus-compatible
//! Counter / Gauge / Histogram and a rendering Registry. Each handle holds
//! its metric or registry, shared by every copy of the handle, and a
//! registry holds a share of every metric registered with it, so a metric
//! lives as long as a handle or a registry reaches it. The metric
//! primitives and the text-exposition rendering are
//! `gossamer_std::metrics`, so the Prometheus text matches the compiled
//! tiers byte-for-byte.

use std::sync::Arc;

use gossamer_std::metrics::{Counter, Gauge, Histogram, Metric, Registry};

use crate::builtins::BuiltinFnPub;
use crate::value::{RuntimeResult, Value};

pub(crate) fn install_metrics(globals: &mut Vec<(&'static str, Value)>) {
    let entries: &[(&str, BuiltinFnPub)] = &[
        ("Counter::new", builtin_counter_new),
        ("Counter::inc", builtin_counter_inc),
        ("Counter::value", builtin_counter_value),
        ("Gauge::new", builtin_gauge_new),
        ("Gauge::set", builtin_gauge_set),
        ("Gauge::inc", builtin_gauge_inc),
        ("Gauge::dec", builtin_gauge_dec),
        ("Gauge::value", builtin_gauge_value),
        ("Histogram::new", builtin_histogram_new),
        ("Histogram::observe", builtin_histogram_observe),
        ("Histogram::sum", builtin_histogram_sum),
        ("Histogram::count", builtin_histogram_count),
        ("Registry::new", builtin_registry_new),
        ("Registry::register", builtin_registry_register),
        ("Registry::render", builtin_registry_render),
        ("serve_metrics", builtin_serve_metrics),
    ];
    for (name, call) in entries {
        // The handle struct names are `metrics::Counter` /
        // `metrics::Gauge` / `metrics::Histogram` / `metrics::Registry`,
        // so `qualified_method_key` emits `metrics::<Type>::method`; the
        // module-qualified spelling covers that and free-call
        // resolution of `metrics::<Type>::new`, the bare spelling covers
        // a `use std::metrics::<Type>` call site.
        let mod_q: &'static str = Box::leak(format!("metrics::{name}").into_boxed_str());
        globals.push((*name, crate::builtins::builtin_pub(name, *call)));
        globals.push((mod_q, crate::builtins::builtin_pub(mod_q, *call)));
    }
}

const METRIC_FIELD: &str = "__metric";
const REGISTRY_NAME: &str = "metrics::Registry";
const REGISTRY_FIELD: &str = "__registry";

fn metric_handle(kind: &'static str, metric: Metric) -> Value {
    crate::value::state_handle(kind, METRIC_FIELD, Arc::new(metric))
}

/// The metric a Counter, Gauge, or Histogram handle holds.
fn metric_of(value: &Value) -> Option<Arc<Metric>> {
    ["metrics::Counter", "metrics::Gauge", "metrics::Histogram"]
        .into_iter()
        .find_map(|kind| crate::value::handle_state(value, kind, METRIC_FIELD))
}

fn registry_of(value: &Value) -> Option<Arc<Registry>> {
    crate::value::handle_state(value, REGISTRY_NAME, REGISTRY_FIELD)
}

fn str_arg(args: &[Value], idx: usize) -> String {
    match args.get(idx) {
        Some(Value::String(s)) => s.as_str().to_string(),
        _ => String::new(),
    }
}

fn f64_arg(args: &[Value], idx: usize) -> f64 {
    match args.get(idx) {
        Some(Value::Float(x)) => *x,
        Some(Value::Int(n)) => *n as f64,
        _ => 0.0,
    }
}

fn buckets_arg(value: Option<&Value>) -> Vec<f64> {
    let Some(v) = value else {
        return Vec::new();
    };
    match v {
        Value::FloatVec(items) => items.iter().copied().collect(),
        Value::IntArray(items) => items.iter().map(|n| *n as f64).collect(),
        Value::Array(items) => items.iter().filter_map(elem_f64).collect(),
        rx @ Value::FloatArray(_) => match rx.float_array_to_value_array() {
            Value::Array(items) => items.iter().filter_map(elem_f64).collect(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

fn elem_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Float(x) => Some(*x),
        Value::Int(n) => Some(*n as f64),
        _ => None,
    }
}

pub(crate) fn builtin_counter_new(args: &[Value]) -> RuntimeResult<Value> {
    let metric = Metric::Counter(Counter::new(&str_arg(args, 0), &str_arg(args, 1)));
    Ok(metric_handle("metrics::Counter", metric))
}

pub(crate) fn builtin_counter_inc(args: &[Value]) -> RuntimeResult<Value> {
    if let Some(metric) = args.first().and_then(metric_of)
        && let Metric::Counter(c) = &*metric
    {
        c.inc();
    }
    Ok(Value::Unit)
}

pub(crate) fn builtin_counter_value(args: &[Value]) -> RuntimeResult<Value> {
    let v = args
        .first()
        .and_then(metric_of)
        .and_then(|metric| match &*metric {
            Metric::Counter(c) => Some(c.value()),
            _ => None,
        })
        .unwrap_or(0);
    Ok(Value::Int(v as i64))
}

pub(crate) fn builtin_gauge_new(args: &[Value]) -> RuntimeResult<Value> {
    let metric = Metric::Gauge(Gauge::new(&str_arg(args, 0), &str_arg(args, 1)));
    Ok(metric_handle("metrics::Gauge", metric))
}

/// Runs `f` on the gauge `value` holds, if it holds one.
fn with_gauge(value: Option<&Value>, f: impl FnOnce(&Gauge)) {
    if let Some(metric) = value.and_then(metric_of)
        && let Metric::Gauge(g) = &*metric
    {
        f(g);
    }
}

pub(crate) fn builtin_gauge_set(args: &[Value]) -> RuntimeResult<Value> {
    let v = f64_arg(args, 1);
    with_gauge(args.first(), |g| g.set(v));
    Ok(Value::Unit)
}

pub(crate) fn builtin_gauge_inc(args: &[Value]) -> RuntimeResult<Value> {
    with_gauge(args.first(), |g| g.add(1.0));
    Ok(Value::Unit)
}

pub(crate) fn builtin_gauge_dec(args: &[Value]) -> RuntimeResult<Value> {
    with_gauge(args.first(), |g| g.sub(1.0));
    Ok(Value::Unit)
}

pub(crate) fn builtin_gauge_value(args: &[Value]) -> RuntimeResult<Value> {
    let mut v = 0.0;
    with_gauge(args.first(), |g| v = g.value());
    Ok(Value::Float(v))
}

pub(crate) fn builtin_histogram_new(args: &[Value]) -> RuntimeResult<Value> {
    let buckets = buckets_arg(args.get(2));
    let metric = Metric::Histogram(Histogram::new(
        &str_arg(args, 0),
        &str_arg(args, 1),
        &buckets,
    ));
    Ok(metric_handle("metrics::Histogram", metric))
}

/// Runs `f` on the histogram `value` holds, if it holds one.
fn with_histogram(value: Option<&Value>, f: impl FnOnce(&Histogram)) {
    if let Some(metric) = value.and_then(metric_of)
        && let Metric::Histogram(h) = &*metric
    {
        f(h);
    }
}

pub(crate) fn builtin_histogram_observe(args: &[Value]) -> RuntimeResult<Value> {
    let v = f64_arg(args, 1);
    with_histogram(args.first(), |h| h.observe(v));
    Ok(Value::Unit)
}

pub(crate) fn builtin_histogram_sum(args: &[Value]) -> RuntimeResult<Value> {
    let mut v = 0.0;
    with_histogram(args.first(), |h| v = h.sum());
    Ok(Value::Float(v))
}

pub(crate) fn builtin_histogram_count(args: &[Value]) -> RuntimeResult<Value> {
    let mut v = 0;
    with_histogram(args.first(), |h| v = h.count());
    Ok(Value::Int(v as i64))
}

pub(crate) fn builtin_registry_new(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(crate::value::state_handle(
        REGISTRY_NAME,
        REGISTRY_FIELD,
        Arc::new(Registry::new()),
    ))
}

pub(crate) fn builtin_registry_register(args: &[Value]) -> RuntimeResult<Value> {
    if let (Some(registry), Some(metric)) = (
        args.first().and_then(registry_of),
        args.get(1).and_then(metric_of),
    ) {
        registry.register(Metric::clone(&metric));
    }
    Ok(Value::Unit)
}

pub(crate) fn builtin_registry_render(args: &[Value]) -> RuntimeResult<Value> {
    let text = args
        .first()
        .and_then(registry_of)
        .map(|registry| registry.expose())
        .unwrap_or_default();
    Ok(Value::String(text.into()))
}

/// `metrics::serve_metrics(addr, registry) -> Result<(), errors::Error>` -
/// serves the registry on `/metrics` over the std http server. Blocks
/// the calling goroutine until shutdown; the compiled tier serves over
/// the runtime's own server via `gos_rt_metrics_serve`.
pub(crate) fn builtin_serve_metrics(args: &[Value]) -> RuntimeResult<Value> {
    let addr = str_arg(args, 0);
    let registry = args
        .get(1)
        .and_then(registry_of)
        .map(|registry| Registry::clone(&registry));
    let Some(registry) = registry else {
        return Ok(crate::builtins::err_variant(
            "serve_metrics: unknown registry handle",
        ));
    };
    match gossamer_std::metrics::serve_metrics(&addr, registry) {
        Ok(()) => Ok(Value::variant("Ok", vec![Value::Unit])),
        Err(e) => Ok(crate::builtins::err_variant(format!("{e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_registry_keeps_a_metric_whose_handles_are_gone() {
        let registry = builtin_registry_new(&[]).expect("Registry::new");
        let counter = builtin_counter_new(&[
            Value::String("requests_total".into()),
            Value::String("Requests served.".into()),
        ])
        .expect("Counter::new");
        builtin_counter_inc(std::slice::from_ref(&counter)).expect("Counter::inc");
        builtin_registry_register(&[registry.clone(), counter.clone()]).expect("register");
        let metric = metric_of(&counter).expect("a metric handle");
        drop(counter);
        assert_eq!(
            Arc::strong_count(&metric),
            1,
            "only this test holds the handle's metric"
        );
        let Value::String(text) = builtin_registry_render(&[registry]).expect("render") else {
            panic!("render answers a string");
        };
        assert!(text.contains("requests_total 1"), "{text}");
    }
}
