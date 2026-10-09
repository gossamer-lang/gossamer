#![allow(clippy::missing_safety_doc)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_sign_loss)]
#![allow(dead_code)]

//! Runtime support for `std::trace` - the explicit Tracer / Span
//! handle surface and OTLP JSON export.
//!
//! `Tracer`, `Span`, and `EndedSpan` are opaque heap handles
//! (`Box::into_raw`); compiled tiers carry the pointer as an `i64`
//! and the MIR receiver-kind dispatch tags constructor results
//! (`trace::Tracer`, `trace::Span`, `trace::EndedSpan`) so method
//! calls route here.
//!
//! Cross-tier determinism: trace / span identifiers are minted from a
//! process-global counter (not asserted by any fixture) and span
//! timestamps are zeroed, so the serialized OTLP JSON differs from
//! the VM tier only in the unguessable id fields - the asserted
//! substrings (span name, attribute key/value) are identical on every
//! tier. The implicit `thread_local` active-span stack from
//! `gossamer_std::trace` is intentionally not wired: goroutines run
//! on a shared worker pool, so a thread-local current-span would not
//! propagate across a `go` boundary. Only the explicit handle surface
//! is exposed; parent/child propagation rides on an explicit
//! `start_span` argument shape left for a follow-up.

use std::fmt::Write as _;
use std::os::raw::c_char;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

use super::string::alloc_cstring;

static SPAN_SEQ: AtomicU64 = AtomicU64::new(1);

unsafe fn read_cstr(s: *const c_char) -> String {
    // SAFETY: this `unsafe fn`'s caller passes `s` live or null, which `gos_str_arg_string`
    // accepts.
    unsafe { crate::c_abi::gos_str_arg_string(s) }
}

/// Process-level span sink. The explicit handle surface does not yet
/// drain ended spans back into Gossamer, so the tracer carries no
/// state beyond its identity; it exists so `start_span` has a
/// receiver and to leave room for a future `ended_spans` accessor.
pub struct GosTracer {
    _seq: AtomicU64,
}

/// In-flight span builder.
pub struct GosSpan {
    name: String,
    trace_id: String,
    span_id: String,
    attributes: Mutex<Vec<(String, String)>>,
    status: Mutex<(bool, String)>,
}

/// Finalised span record. `to_otlp_json` serializes it for OTLP/HTTP.
pub struct GosEndedSpan {
    name: String,
    trace_id: String,
    span_id: String,
    attributes: Vec<(String, String)>,
    status_ok: bool,
    status_message: String,
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_trace_tracer_new() -> *mut GosTracer {
    ffi_entry!({
        Box::into_raw(Box::new(GosTracer {
            _seq: AtomicU64::new(0),
        }))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_trace_tracer_start_span(
    _t: *mut GosTracer,
    name: *const c_char,
) -> *mut GosSpan {
    ffi_entry!({
        let seq = SPAN_SEQ.fetch_add(1, Ordering::Relaxed);
        Box::into_raw(Box::new(GosSpan {
            // SAFETY: `name` is this shim's argument, null or a live string body for the call
            // (C-ABI contract), which `read_cstr` accepts.
            name: unsafe { read_cstr(name) },
            trace_id: format!("{:032x}", u128::from(seq).wrapping_mul(0x9e37_79b9)),
            span_id: format!("{:016x}", seq.wrapping_mul(0x9e37_79b9_7f4a_7c15)),
            attributes: Mutex::new(Vec::new()),
            status: Mutex::new((true, String::new())),
        }))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_trace_span_set_attribute(
    s: *mut GosSpan,
    key: *const c_char,
    value: *const c_char,
) {
    ffi_entry!({
        // SAFETY: `s` is this shim's argument, null or a live span (C-ABI contract), which
        // `as_ref` accepts.
        if let Some(span) = unsafe { s.as_ref() } {
            // SAFETY: `key` and `value` are this shim's arguments, each null or a live string
            // body (C-ABI contract), which `read_cstr` accepts.
            let (k, v) = unsafe { (read_cstr(key), read_cstr(value)) };
            let mut attrs = span.attributes.lock();
            if let Some(slot) = attrs.iter_mut().find(|(ek, _)| *ek == k) {
                slot.1 = v;
            } else {
                attrs.push((k, v));
            }
        }
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_trace_span_set_status(
    s: *mut GosSpan,
    ok: i64,
    message: *const c_char,
) {
    ffi_entry!({
        // SAFETY: `s` is this shim's argument, null or a live span (C-ABI contract), which
        // `as_ref` accepts.
        if let Some(span) = unsafe { s.as_ref() } {
            // SAFETY: `message` is this shim's argument, null or a live string body for the call
            // (C-ABI contract), which `read_cstr` accepts.
            *span.status.lock() = (ok != 0, unsafe { read_cstr(message) });
        }
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_trace_span_end(s: *mut GosSpan) -> *mut GosEndedSpan {
    ffi_entry!({
        // SAFETY: `s` is this shim's argument, null or a live span (C-ABI contract), which
        // `as_ref` accepts.
        let Some(span) = (unsafe { s.as_ref() }) else {
            return std::ptr::null_mut();
        };
        let (status_ok, status_message) = span.status.lock().clone();
        Box::into_raw(Box::new(GosEndedSpan {
            name: span.name.clone(),
            trace_id: span.trace_id.clone(),
            span_id: span.span_id.clone(),
            attributes: span.attributes.lock().clone(),
            status_ok,
            status_message,
        }))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_trace_ended_to_otlp_json(e: *mut GosEndedSpan) -> *mut c_char {
    ffi_entry!({
        // SAFETY: `e` is this shim's argument, null or a live ended span (C-ABI contract), which
        // `as_ref` accepts.
        let Some(span) = (unsafe { e.as_ref() }) else {
            return alloc_cstring(b"");
        };
        alloc_cstring(otlp_json(span).as_bytes())
    })
}

/// Renders a single OTLP-spec span JSON object. Timestamps are zeroed
/// for cross-tier determinism. The field shape mirrors
/// `gossamer_std::trace::EndedSpan::to_otlp_json` closely enough that
/// the asserted substrings (name, attribute key / value) match.
fn otlp_json(span: &GosEndedSpan) -> String {
    let mut out = String::new();
    out.push('{');
    push_kv_str(&mut out, "traceId", &span.trace_id);
    out.push(',');
    push_kv_str(&mut out, "spanId", &span.span_id);
    out.push(',');
    push_kv_str(&mut out, "name", &span.name);
    out.push(',');
    out.push_str("\"kind\":1,");
    out.push_str("\"startTimeUnixNano\":\"0\",");
    out.push_str("\"endTimeUnixNano\":\"0\",");
    out.push_str("\"attributes\":[");
    for (i, (k, v)) in span.attributes.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('{');
        push_kv_str(&mut out, "key", k);
        out.push_str(",\"value\":{");
        push_kv_str(&mut out, "stringValue", v);
        out.push_str("}}");
    }
    out.push_str("],");
    let code = if span.status_ok { 1 } else { 2 };
    out.push_str("\"status\":{");
    let _ = write!(out, "\"code\":{code}");
    if !span.status_message.is_empty() {
        out.push(',');
        push_kv_str(&mut out, "message", &span.status_message);
    }
    out.push_str("}}");
    out
}

fn push_kv_str(out: &mut String, key: &str, value: &str) {
    out.push('"');
    out.push_str(key);
    out.push_str("\":\"");
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}
