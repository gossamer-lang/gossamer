#![allow(
    unused_imports,
    dead_code,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::missing_errors_doc,
    clippy::unnecessary_wraps,
    clippy::needless_pass_by_value
)]
//! `std::trace` builtins for the bytecode VM - the explicit
//! Tracer / Span / EndedSpan handle surface and OTLP JSON export.
//! Each Span and EndedSpan handle holds its span, shared by every copy of
//! the handle and freed with the last one. Identifiers are minted from
//! `gossamer_std::trace` and span timestamps are zeroed, so the
//! serialized OTLP JSON differs from the compiled tiers only in the
//! unguessable id fields - the asserted substrings (span name,
//! attribute key / value) are identical on every tier.
//!
//! The implicit `thread_local` active-span stack in
//! `gossamer_std::trace` is intentionally not exposed: goroutines run
//! on a shared worker pool, so a thread-local current-span would not
//! propagate across a `go` boundary. Only the explicit handle surface
//! is wired.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use gossamer_std::trace::{EndedSpan, SpanContext, SpanId, TraceId};

use crate::builtins::BuiltinFnPub;
use crate::value::{RuntimeResult, Value};

struct SpanData {
    name: String,
    trace_id: TraceId,
    span_id: SpanId,
    attributes: Vec<(String, String)>,
    status_ok: bool,
    status_message: String,
}

/// A span until it ends; `end` takes the data, so a span ends once.
type OpenSpan = parking_lot::Mutex<Option<SpanData>>;

static NEXT_TRACER_ID: AtomicI64 = AtomicI64::new(1);

pub(crate) fn install_trace(globals: &mut Vec<(&'static str, Value)>) {
    let entries: &[(&str, BuiltinFnPub)] = &[
        ("Tracer::new", builtin_tracer_new),
        ("Tracer::start_span", builtin_tracer_start_span),
        ("Span::set_attribute", builtin_span_set_attribute),
        ("Span::set_status", builtin_span_set_status),
        ("Span::end", builtin_span_end),
        ("EndedSpan::to_otlp_json", builtin_ended_to_otlp_json),
    ];
    for (name, call) in entries {
        // Handle struct names are `trace::Tracer` / `trace::Span` /
        // `trace::EndedSpan`, so `qualified_method_key` emits
        // `trace::<Type>::method`; the module-qualified spelling covers
        // that and free-call resolution of `trace::Tracer::new`, the
        // bare spelling covers a `use std::trace::<Type>` call site.
        let mod_q: &'static str = Box::leak(format!("trace::{name}").into_boxed_str());
        globals.push((*name, crate::builtins::builtin_pub(name, *call)));
        globals.push((mod_q, crate::builtins::builtin_pub(mod_q, *call)));
    }
}

fn span_of(value: &Value) -> Option<Arc<OpenSpan>> {
    crate::value::handle_state(value, "trace::Span", "__span")
}

fn ended_of(value: &Value) -> Option<Arc<EndedSpan>> {
    crate::value::handle_state(value, "trace::EndedSpan", "__ended")
}

fn str_arg(args: &[Value], idx: usize) -> String {
    match args.get(idx) {
        Some(Value::String(s)) => s.as_str().to_string(),
        _ => String::new(),
    }
}

fn bool_arg(args: &[Value], idx: usize) -> bool {
    matches!(args.get(idx), Some(Value::Bool(true)))
}

pub(crate) fn builtin_tracer_new(_args: &[Value]) -> RuntimeResult<Value> {
    // A tracer carries no state of its own; the id only tells two apart.
    let id = NEXT_TRACER_ID.fetch_add(1, Ordering::Relaxed);
    Ok(Value::struct_(
        "trace::Tracer",
        vec![("__tracer", Value::Int(id))],
    ))
}

pub(crate) fn builtin_tracer_start_span(args: &[Value]) -> RuntimeResult<Value> {
    let span = SpanData {
        name: str_arg(args, 1),
        trace_id: TraceId::new_random(),
        span_id: SpanId::new_random(),
        attributes: Vec::new(),
        status_ok: true,
        status_message: String::new(),
    };
    Ok(crate::value::state_handle(
        "trace::Span",
        "__span",
        Arc::new(OpenSpan::new(Some(span))),
    ))
}

pub(crate) fn builtin_span_set_attribute(args: &[Value]) -> RuntimeResult<Value> {
    if let Some(open) = args.first().and_then(span_of)
        && let Some(span) = open.lock().as_mut()
    {
        let key = str_arg(args, 1);
        let value = str_arg(args, 2);
        if let Some(slot) = span.attributes.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = value;
        } else {
            span.attributes.push((key, value));
        }
    }
    Ok(Value::Unit)
}

pub(crate) fn builtin_span_set_status(args: &[Value]) -> RuntimeResult<Value> {
    if let Some(open) = args.first().and_then(span_of)
        && let Some(span) = open.lock().as_mut()
    {
        span.status_ok = bool_arg(args, 1);
        span.status_message = str_arg(args, 2);
    }
    Ok(Value::Unit)
}

pub(crate) fn builtin_span_end(args: &[Value]) -> RuntimeResult<Value> {
    let Some(open) = args.first().and_then(span_of) else {
        return Ok(Value::Unit);
    };
    let Some(span) = open.lock().take() else {
        return Ok(Value::Unit);
    };
    let ended = EndedSpan {
        name: span.name,
        context: SpanContext {
            trace_id: span.trace_id,
            span_id: span.span_id,
            sampled: true,
        },
        parent: None,
        attributes: span.attributes,
        status_ok: span.status_ok,
        status_message: span.status_message,
        start_unix_nanos: 0,
        end_unix_nanos: 0,
    };
    Ok(crate::value::state_handle(
        "trace::EndedSpan",
        "__ended",
        Arc::new(ended),
    ))
}

pub(crate) fn builtin_ended_to_otlp_json(args: &[Value]) -> RuntimeResult<Value> {
    let json = args
        .first()
        .and_then(ended_of)
        .map(|ended| ended.to_otlp_json())
        .unwrap_or_default();
    Ok(Value::String(json.into()))
}
