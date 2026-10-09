#![allow(clippy::missing_safety_doc)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_precision_loss)]

//! Runtime support for `std::metrics` - Prometheus-compatible
//! Counter / Gauge / Histogram primitives and a Registry that renders
//! the text-exposition format.
//!
//! Each metric is an opaque heap `Box<GosMetric>`; compiled tiers
//! carry the pointer as an `i64` and the MIR receiver-kind dispatch
//! tags constructor results (`metrics::Counter`, `metrics::Gauge`,
//! `metrics::Histogram`, `metrics::Registry`) so method calls route
//! to the helpers below. A `Registry` stores the metric pointers it
//! was handed (registration order preserved) and reads through them
//! at render time, so updates made through the original handle are
//! observed.
//!
//! The rendering logic is kept bit-identical to the VM's
//! `gossamer_std::metrics::Registry::expose` so the Prometheus text
//! matches on every tier. The primitives are reimplemented here
//! (rather than depending on `gossamer_std::metrics`, which would form
//! a `runtime -> std -> runtime` dependency cycle).

use std::fmt::Write as _;
use std::os::raw::c_char;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

use super::SyncRawPtr;
use super::http_client::{GosHttpRequest, GosHttpResponse};
use super::string::alloc_cstring;
use super::vec::GosVec;

/// Reads a Gossamer c-string argument into an owned `String`.
unsafe fn read_cstr(s: *const c_char) -> String {
    // SAFETY: this `unsafe fn`'s caller passes `s` live or null, which `gos_str_arg_string`
    // accepts.
    unsafe { crate::c_abi::gos_str_arg_string(s) }
}

/// Reads a Gossamer `[f64]` / `Vec<f64>` argument into a `Vec<f64>`.
/// The backing buffer is a contiguous run of `len` 8-byte `f64`s.
unsafe fn read_f64_vec(v: *const GosVec) -> Vec<f64> {
    // SAFETY: this `unsafe fn`'s caller passes `v` null or a live `Vec`.
    unsafe { crate::c_abi::vec::VecView::of(v) }.map_or_else(Vec::new, |vec| {
        vec.words()
            .map(|bits| f64::from_bits(bits as u64))
            .collect()
    })
}

struct HistState {
    counts: Vec<u64>,
    sum: f64,
    count: u64,
}

enum MetricBody {
    Counter(AtomicU64),
    Gauge(AtomicU64),
    Histogram {
        bounds: Vec<f64>,
        state: Mutex<HistState>,
    },
}

/// Opaque heap handle for a single metric.
pub struct GosMetric {
    name: String,
    help: String,
    body: MetricBody,
}

/// Opaque heap handle for a metric registry. Holds metric pointers in
/// registration order; the metrics themselves are owned by the
/// process (their `Box`es leak for the program lifetime, matching the
/// other stateful-handle shims).
pub struct GosRegistry {
    metrics: Mutex<Vec<SyncRawPtr<GosMetric>>>,
}

fn gauge_update(bits: &AtomicU64, op: impl Fn(f64) -> f64) {
    loop {
        let prev = bits.load(Ordering::Relaxed);
        let next = op(f64::from_bits(prev)).to_bits();
        if bits
            .compare_exchange_weak(prev, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return;
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_metrics_counter_new(
    name: *const c_char,
    help: *const c_char,
) -> *mut GosMetric {
    ffi_entry!({
        Box::into_raw(Box::new(GosMetric {
            // SAFETY: `name` and `help` are this shim's arguments, each null or a live string
            // body (C-ABI contract), which `read_cstr` accepts.
            name: unsafe { read_cstr(name) },
            // SAFETY: `name` and `help` are this shim's arguments, each null or a live string
            // body (C-ABI contract), which `read_cstr` accepts.
            help: unsafe { read_cstr(help) },
            body: MetricBody::Counter(AtomicU64::new(0)),
        }))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_metrics_counter_inc(m: *mut GosMetric) {
    ffi_entry!({
        // SAFETY: `m` is this shim's argument, null or a live metric (C-ABI contract), which
        // `as_ref` accepts.
        if let Some(MetricBody::Counter(v)) = unsafe { m.as_ref() }.map(|m| &m.body) {
            v.fetch_add(1, Ordering::Relaxed);
        }
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_metrics_counter_value(m: *mut GosMetric) -> i64 {
    ffi_entry!({
        // SAFETY: `m` is this shim's argument, null or a live metric (C-ABI contract), which
        // `as_ref` accepts.
        match unsafe { m.as_ref() }.map(|m| &m.body) {
            Some(MetricBody::Counter(v)) => v.load(Ordering::Relaxed) as i64,
            _ => 0,
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_metrics_gauge_new(
    name: *const c_char,
    help: *const c_char,
) -> *mut GosMetric {
    ffi_entry!({
        Box::into_raw(Box::new(GosMetric {
            // SAFETY: `name` and `help` are this shim's arguments, each null or a live string
            // body (C-ABI contract), which `read_cstr` accepts.
            name: unsafe { read_cstr(name) },
            // SAFETY: `name` and `help` are this shim's arguments, each null or a live string
            // body (C-ABI contract), which `read_cstr` accepts.
            help: unsafe { read_cstr(help) },
            body: MetricBody::Gauge(AtomicU64::new(0_f64.to_bits())),
        }))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_metrics_gauge_set(m: *mut GosMetric, v: f64) {
    ffi_entry!({
        // SAFETY: `m` is this shim's argument, null or a live metric (C-ABI contract), which
        // `as_ref` accepts.
        if let Some(MetricBody::Gauge(bits)) = unsafe { m.as_ref() }.map(|m| &m.body) {
            bits.store(v.to_bits(), Ordering::Relaxed);
        }
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_metrics_gauge_inc(m: *mut GosMetric) {
    ffi_entry!({
        // SAFETY: `m` is this shim's argument, null or a live metric (C-ABI contract), which
        // `as_ref` accepts.
        if let Some(MetricBody::Gauge(bits)) = unsafe { m.as_ref() }.map(|m| &m.body) {
            gauge_update(bits, |c| c + 1.0);
        }
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_metrics_gauge_dec(m: *mut GosMetric) {
    ffi_entry!({
        // SAFETY: `m` is this shim's argument, null or a live metric (C-ABI contract), which
        // `as_ref` accepts.
        if let Some(MetricBody::Gauge(bits)) = unsafe { m.as_ref() }.map(|m| &m.body) {
            gauge_update(bits, |c| c - 1.0);
        }
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_metrics_gauge_value(m: *mut GosMetric) -> f64 {
    ffi_entry!({
        // SAFETY: `m` is this shim's argument, null or a live metric (C-ABI contract), which
        // `as_ref` accepts.
        match unsafe { m.as_ref() }.map(|m| &m.body) {
            Some(MetricBody::Gauge(bits)) => f64::from_bits(bits.load(Ordering::Relaxed)),
            _ => 0.0,
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_metrics_histogram_new(
    name: *const c_char,
    help: *const c_char,
    buckets: *const GosVec,
) -> *mut GosMetric {
    ffi_entry!({
        // SAFETY: `buckets` is this shim's argument, live for the call (C-ABI contract) or null,
        // which `read_f64_vec` accepts.
        let bounds = unsafe { read_f64_vec(buckets) };
        let counts = vec![0_u64; bounds.len()];
        Box::into_raw(Box::new(GosMetric {
            // SAFETY: `name` and `help` are this shim's arguments, each null or a live string
            // body (C-ABI contract), which `read_cstr` accepts.
            name: unsafe { read_cstr(name) },
            // SAFETY: `name` and `help` are this shim's arguments, each null or a live string
            // body (C-ABI contract), which `read_cstr` accepts.
            help: unsafe { read_cstr(help) },
            body: MetricBody::Histogram {
                bounds,
                state: Mutex::new(HistState {
                    counts,
                    sum: 0.0,
                    count: 0,
                }),
            },
        }))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_metrics_histogram_observe(m: *mut GosMetric, v: f64) {
    ffi_entry!({
        if let Some(MetricBody::Histogram { bounds, state }) =
            // SAFETY: `m` is this shim's argument, null or a live metric (C-ABI contract), which
            // `as_ref` accepts.
            unsafe { m.as_ref() }.map(|m| &m.body)
        {
            let mut s = state.lock();
            for (i, bound) in bounds.iter().enumerate() {
                if v <= *bound {
                    s.counts[i] += 1;
                }
            }
            s.sum += v;
            s.count += 1;
        }
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_metrics_histogram_sum(m: *mut GosMetric) -> f64 {
    ffi_entry!({
        // SAFETY: `m` is this shim's argument, null or a live metric (C-ABI contract), which
        // `as_ref` accepts.
        match unsafe { m.as_ref() }.map(|m| &m.body) {
            Some(MetricBody::Histogram { state, .. }) => state.lock().sum,
            _ => 0.0,
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_metrics_histogram_count(m: *mut GosMetric) -> i64 {
    ffi_entry!({
        // SAFETY: `m` is this shim's argument, null or a live metric (C-ABI contract), which
        // `as_ref` accepts.
        match unsafe { m.as_ref() }.map(|m| &m.body) {
            Some(MetricBody::Histogram { state, .. }) => state.lock().count as i64,
            _ => 0,
        }
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_metrics_registry_new() -> *mut GosRegistry {
    ffi_entry!({
        Box::into_raw(Box::new(GosRegistry {
            metrics: Mutex::new(Vec::new()),
        }))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_metrics_registry_register(r: *mut GosRegistry, m: *mut GosMetric) {
    ffi_entry!({
        // SAFETY: `r` is this shim's argument, null or a live registry (C-ABI contract), which
        // `as_ref` accepts.
        if let (Some(reg), false) = (unsafe { r.as_ref() }, m.is_null()) {
            reg.metrics.lock().push(SyncRawPtr::new(m));
        }
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_metrics_registry_render(r: *mut GosRegistry) -> *mut c_char {
    ffi_entry!({
        // SAFETY: `r` is this shim's argument, null or a live registry (C-ABI contract), which
        // `as_ref` accepts.
        let Some(reg) = (unsafe { r.as_ref() }) else {
            return alloc_cstring(b"");
        };
        alloc_cstring(render_registry(reg).as_bytes())
    })
}

/// Renders every metric registered with `reg` as Prometheus
/// text-exposition. Kept bit-identical to
/// `gossamer_std::metrics::Registry::expose` so the body matches the
/// VM tier byte-for-byte.
fn render_registry(reg: &GosRegistry) -> String {
    let mut out = String::new();
    for slot in reg.metrics.lock().iter() {
        // SAFETY: a registered slot is a metric handle, and metric handles are never freed, so it
        // lives for the program.
        if let Some(metric) = unsafe { slot.as_const_ptr().as_ref() } {
            render_metric(&mut out, metric);
        }
    }
    out
}

/// HTTP handler bound to a `*GosRegistry` env. Renders the registry on
/// `/metrics` and answers `404 not found` on every other path,
/// mirroring `gossamer_std::metrics::serve_metrics`. Matches the
/// `HandlerFn` ABI `gos_rt_http_serve` invokes.
unsafe extern "C" fn metrics_handler(env: *mut u8, req: *mut GosHttpRequest) -> i128 {
    ffi_entry!({
        // SAFETY: the server passes the request it owns, null or live for the call.
        let Some(request) = (unsafe { req.as_ref() }) else {
            return metrics_not_found();
        };
        // `r.path` strips the query component, matching the interp tier.
        let path = request.url.split('?').next().unwrap_or(&request.url);
        if path != "/metrics" {
            return metrics_not_found();
        }
        // SAFETY: `env` is the registry `gos_rt_metrics_serve` registered, and registries are
        // never freed.
        let body = match unsafe { (env as *mut GosRegistry).as_ref() } {
            Some(reg) => render_registry(reg),
            None => String::new(),
        };
        let resp = Box::into_raw(Box::new(GosHttpResponse {
            status: 200,
            body: SyncRawPtr::new(alloc_cstring(body.as_bytes())),
            headers: Vec::new(),
            body_bytes: None,
            content_type: "text/plain; version=0.0.4; charset=utf-8".into(),
            stream_handle: -1,
        }));
        super::result::pack_result(0, resp as i64)
    })
}

/// A buffered `404 not found` response packed as the handler's `Ok`
/// result - the non-`/metrics` path on the metrics endpoint.
fn metrics_not_found() -> i128 {
    let resp = Box::into_raw(Box::new(GosHttpResponse {
        status: 404,
        body: SyncRawPtr::new(alloc_cstring(b"not found")),
        headers: Vec::new(),
        body_bytes: None,
        content_type: "text/plain; charset=utf-8".into(),
        stream_handle: -1,
    }));
    super::result::pack_result(0, resp as i64)
}

/// `metrics::serve_metrics(addr, registry)` - serves the registry's
/// Prometheus exposition on `/metrics` over the runtime's own HTTP
/// server. Blocks the calling goroutine until shutdown. Returns the
/// Gossamer `Result<(), errors::Error>` shape (Err on bind failure),
/// matching `gossamer_std::metrics::serve_metrics`.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_metrics_serve(
    addr: *const c_char,
    registry: *mut GosRegistry,
) -> i128 {
    // SAFETY: `addr` and `registry` are this shim's arguments (C-ABI contract), and
    // `metrics_handler` is the environment-taking handler `gos_rt_http_serve` calls with that
    // registry.
    unsafe {
        super::http_server::gos_rt_http_serve(
            addr,
            registry.cast::<u8>(),
            metrics_handler as *const () as i64,
        )
    }
}

fn render_metric(out: &mut String, m: &GosMetric) {
    match &m.body {
        MetricBody::Counter(v) => {
            let _ = writeln!(out, "# HELP {} {}", m.name, m.help);
            let _ = writeln!(out, "# TYPE {} counter", m.name);
            let _ = writeln!(out, "{} {}", m.name, v.load(Ordering::Relaxed));
        }
        MetricBody::Gauge(bits) => {
            let value = f64::from_bits(bits.load(Ordering::Relaxed));
            let _ = writeln!(out, "# HELP {} {}", m.name, m.help);
            let _ = writeln!(out, "# TYPE {} gauge", m.name);
            let _ = writeln!(out, "{} {}", m.name, format_f64(value));
        }
        MetricBody::Histogram { bounds, state } => {
            let s = state.lock();
            let _ = writeln!(out, "# HELP {} {}", m.name, m.help);
            let _ = writeln!(out, "# TYPE {} histogram", m.name);
            for (bound, c) in bounds.iter().zip(s.counts.iter()) {
                let _ = writeln!(
                    out,
                    "{}_bucket{{le=\"{}\"}} {}",
                    m.name,
                    format_f64(*bound),
                    c
                );
            }
            let _ = writeln!(out, "{}_bucket{{le=\"+Inf\"}} {}", m.name, s.count);
            let _ = writeln!(out, "{}_sum {}", m.name, format_f64(s.sum));
            let _ = writeln!(out, "{}_count {}", m.name, s.count);
        }
    }
}

/// Matches `gossamer_std::metrics::format_f64` byte-for-byte so the
/// Prometheus text is identical across the VM and compiled tiers.
fn format_f64(v: f64) -> String {
    if v.is_nan() {
        "NaN".to_string()
    } else if v.is_infinite() {
        if v.is_sign_negative() {
            "-Inf".to_string()
        } else {
            "+Inf".to_string()
        }
    } else if v.fract() == 0.0 && v.abs() < 1e16 {
        format!("{v:.0}")
    } else {
        format!("{v}")
    }
}
