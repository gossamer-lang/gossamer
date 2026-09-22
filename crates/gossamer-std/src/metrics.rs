//! Runtime support for `std::metrics`.
//!
//! Prometheus-compatible text exposition. Three primitive metric
//! shapes (Counter, Gauge, Histogram), a `Registry` that holds them
//! in registration order, and `http::serve_metrics` to expose the
//! `/metrics` endpoint over the existing `http::server::run` loop.
//!
//! All primitives are cheap to clone (`Arc` under the hood) so a
//! single metric value can be registered with the `Registry` and
//! also held by application code that updates it. Counter and Gauge
//! are lock-free; Histogram uses a `parking_lot::Mutex` to guard the
//! per-bucket counters.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

#[cfg(not(target_arch = "wasm32"))]
use crate::errors::Error;
// The Prometheus exposition types stay available everywhere; only the
// `/metrics` HTTP serve loop (which needs the gated `http::server`) is
// wasm-gated below.
#[cfg(not(target_arch = "wasm32"))]
use crate::http::{Request, Response, StatusCode, server};

/// Monotonic-increasing `u64` counter. Lock-free.
#[derive(Clone)]
pub struct Counter {
    inner: Arc<CounterInner>,
}

struct CounterInner {
    name: String,
    help: String,
    value: AtomicU64,
}

impl Counter {
    /// Constructs a fresh counter with metric name + help text.
    #[must_use]
    pub fn new(name: &str, help: &str) -> Self {
        Self {
            inner: Arc::new(CounterInner {
                name: name.to_string(),
                help: help.to_string(),
                value: AtomicU64::new(0),
            }),
        }
    }

    /// Increments the counter by one.
    pub fn inc(&self) {
        self.inner.value.fetch_add(1, Ordering::Relaxed);
    }

    /// Increments the counter by `n`.
    pub fn inc_by(&self, n: u64) {
        self.inner.value.fetch_add(n, Ordering::Relaxed);
    }

    /// Returns the current value.
    #[must_use]
    pub fn value(&self) -> u64 {
        self.inner.value.load(Ordering::Relaxed)
    }

    /// Returns the metric name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// Returns the help text.
    #[must_use]
    pub fn help(&self) -> &str {
        &self.inner.help
    }
}

/// Bidirectional `f64` gauge. Lock-free via `AtomicU64::to_bits`.
#[derive(Clone)]
pub struct Gauge {
    inner: Arc<GaugeInner>,
}

struct GaugeInner {
    name: String,
    help: String,
    bits: AtomicU64,
}

impl Gauge {
    /// Constructs a fresh gauge initialised to `0.0`.
    #[must_use]
    pub fn new(name: &str, help: &str) -> Self {
        Self {
            inner: Arc::new(GaugeInner {
                name: name.to_string(),
                help: help.to_string(),
                bits: AtomicU64::new(0_f64.to_bits()),
            }),
        }
    }

    /// Sets the gauge to `v`.
    pub fn set(&self, v: f64) {
        self.inner.bits.store(v.to_bits(), Ordering::Relaxed);
    }

    /// Adds `v` to the gauge.
    pub fn add(&self, v: f64) {
        self.update(|current| current + v);
    }

    /// Subtracts `v` from the gauge.
    pub fn sub(&self, v: f64) {
        self.update(|current| current - v);
    }

    /// Returns the current value.
    #[must_use]
    pub fn value(&self) -> f64 {
        f64::from_bits(self.inner.bits.load(Ordering::Relaxed))
    }

    /// Returns the metric name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// Returns the help text.
    #[must_use]
    pub fn help(&self) -> &str {
        &self.inner.help
    }

    fn update(&self, op: impl Fn(f64) -> f64) {
        let bits = &self.inner.bits;
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
}

/// Fixed-bucket histogram. Tracks count, running sum, and per-bucket
/// cumulative counts (Prometheus-style `_bucket{le="..."}` shape).
#[derive(Clone)]
pub struct Histogram {
    inner: Arc<HistogramInner>,
}

struct HistogramInner {
    name: String,
    help: String,
    bounds: Vec<f64>,
    state: Mutex<HistogramState>,
}

struct HistogramState {
    counts: Vec<u64>,
    sum: f64,
    count: u64,
}

impl Histogram {
    /// Constructs a histogram with the given upper bounds (in
    /// ascending order). The `+Inf` bucket is implicit and tracks
    /// the total count.
    #[must_use]
    pub fn new(name: &str, help: &str, buckets: &[f64]) -> Self {
        let bounds: Vec<f64> = buckets.to_vec();
        let counts = vec![0_u64; bounds.len()];
        Self {
            inner: Arc::new(HistogramInner {
                name: name.to_string(),
                help: help.to_string(),
                bounds,
                state: Mutex::new(HistogramState {
                    counts,
                    sum: 0.0,
                    count: 0,
                }),
            }),
        }
    }

    /// Records a single observation. Every bucket whose upper bound
    /// is `>= v` is incremented (Prometheus cumulative-bucket shape).
    pub fn observe(&self, v: f64) {
        let mut s = self.inner.state.lock();
        for (i, bound) in self.inner.bounds.iter().enumerate() {
            if v <= *bound {
                s.counts[i] += 1;
            }
        }
        s.sum += v;
        s.count += 1;
    }

    /// Returns the total observation count.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.inner.state.lock().count
    }

    /// Returns the running sum of every observation.
    #[must_use]
    pub fn sum(&self) -> f64 {
        self.inner.state.lock().sum
    }

    /// Returns a snapshot of `(upper_bound, cumulative_count)` pairs
    /// for every configured bucket (the `+Inf` bucket is omitted -
    /// callers can read it from `count()`).
    #[must_use]
    pub fn buckets(&self) -> Vec<(f64, u64)> {
        let s = self.inner.state.lock();
        self.inner
            .bounds
            .iter()
            .zip(s.counts.iter())
            .map(|(b, c)| (*b, *c))
            .collect()
    }

    /// Returns the metric name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// Returns the help text.
    #[must_use]
    pub fn help(&self) -> &str {
        &self.inner.help
    }
}

/// A registered metric. `Registry::register` accepts any of the
/// three primitive shapes; rendering walks them in registration order.
#[derive(Clone)]
pub enum Metric {
    /// Counter metric.
    Counter(Counter),
    /// Gauge metric.
    Gauge(Gauge),
    /// Histogram metric.
    Histogram(Histogram),
}

impl From<Counter> for Metric {
    fn from(c: Counter) -> Self {
        Self::Counter(c)
    }
}

impl From<Gauge> for Metric {
    fn from(g: Gauge) -> Self {
        Self::Gauge(g)
    }
}

impl From<Histogram> for Metric {
    fn from(h: Histogram) -> Self {
        Self::Histogram(h)
    }
}

/// Holds a collection of metrics and renders them in Prometheus
/// text exposition format. Cheap to clone - internally an
/// `Arc<Mutex<Vec<Metric>>>`.
#[derive(Clone, Default)]
pub struct Registry {
    inner: Arc<Mutex<Vec<Metric>>>,
}

impl Registry {
    /// Constructs a fresh, empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Registers `metric`. Registration order is preserved across
    /// `expose()` calls.
    pub fn register(&self, metric: impl Into<Metric>) {
        self.inner.lock().push(metric.into());
    }

    /// Returns the number of registered metrics.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    /// `true` iff no metrics have been registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.lock().is_empty()
    }

    /// Renders every registered metric as a Prometheus text-format
    /// payload suitable for the `/metrics` endpoint.
    #[must_use]
    pub fn expose(&self) -> String {
        let metrics = self.inner.lock();
        let mut out = String::new();
        for m in metrics.iter() {
            match m {
                Metric::Counter(c) => render_counter(&mut out, c),
                Metric::Gauge(g) => render_gauge(&mut out, g),
                Metric::Histogram(h) => render_histogram(&mut out, h),
            }
        }
        out
    }
}

fn render_counter(out: &mut String, c: &Counter) {
    let name = c.name();
    out.push_str(&format!("# HELP {} {}\n", name, c.help()));
    out.push_str(&format!("# TYPE {name} counter\n"));
    out.push_str(&format!("{} {}\n", name, c.value()));
}

fn render_gauge(out: &mut String, g: &Gauge) {
    let name = g.name();
    out.push_str(&format!("# HELP {} {}\n", name, g.help()));
    out.push_str(&format!("# TYPE {name} gauge\n"));
    out.push_str(&format!("{} {}\n", name, format_f64(g.value())));
}

fn render_histogram(out: &mut String, h: &Histogram) {
    let name = h.name();
    out.push_str(&format!("# HELP {} {}\n", name, h.help()));
    out.push_str(&format!("# TYPE {name} histogram\n"));
    let buckets = h.buckets();
    let count = h.count();
    let sum = h.sum();
    for (bound, c) in &buckets {
        out.push_str(&format!(
            "{}_bucket{{le=\"{}\"}} {}\n",
            name,
            format_f64(*bound),
            c
        ));
    }
    out.push_str(&format!("{name}_bucket{{le=\"+Inf\"}} {count}\n"));
    out.push_str(&format!("{}_sum {}\n", name, format_f64(sum)));
    out.push_str(&format!("{name}_count {count}\n"));
}

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

/// Serves `/metrics` over HTTP on `addr`. Any other path returns
/// `404`. Blocks the calling thread. Needs the gated `http::server`,
/// so it is unavailable in the wasm playground.
#[cfg(not(target_arch = "wasm32"))]
pub fn serve_metrics(addr: &str, registry: Registry) -> Result<(), Error> {
    let listener = gossamer_runtime::listen::bind_tcp(addr)
        .map_err(|e| Error::new(format!("bind {addr}: {e}")))?;
    let config = server::Config::default();
    let result = server::run(listener, &config, move |req: Request| -> Response {
        if req.path() == "/metrics" {
            let body = registry.expose();
            let mut resp = Response::text(StatusCode(200), body);
            resp.headers
                .insert("content-type", "text/plain; version=0.0.4; charset=utf-8");
            resp
        } else {
            Response::text(StatusCode(404), "not found")
        }
    });
    result.map_err(|e| Error::new(format!("serve_metrics: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn counter_inc_and_value() {
        let c = Counter::new("hits", "request hits");
        assert_eq!(c.value(), 0);
        c.inc();
        c.inc();
        c.inc();
        assert_eq!(c.value(), 3);
    }

    #[test]
    fn counter_inc_by() {
        let c = Counter::new("bytes_total", "bytes seen");
        c.inc_by(100);
        c.inc_by(50);
        assert_eq!(c.value(), 150);
    }

    #[test]
    fn counter_is_thread_safe() {
        let c = Counter::new("hits", "h");
        let c_shared = Arc::new(c);
        let mut handles = Vec::new();
        for _ in 0..10 {
            let c2 = Arc::clone(&c_shared);
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    c2.inc();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(c_shared.value(), 1000);
    }

    #[test]
    fn gauge_set_add_sub() {
        let g = Gauge::new("temp", "temperature");
        g.set(20.5);
        assert_eq!(g.value(), 20.5);
        g.add(4.5);
        assert_eq!(g.value(), 25.0);
        g.sub(10.0);
        assert_eq!(g.value(), 15.0);
    }

    #[test]
    fn gauge_negative_values() {
        let g = Gauge::new("delta", "drift");
        g.set(-5.0);
        g.sub(2.5);
        assert_eq!(g.value(), -7.5);
    }

    #[test]
    fn histogram_buckets_cumulative() {
        let h = Histogram::new("latency", "request latency", &[0.1, 0.5, 1.0, 5.0]);
        h.observe(0.05);
        h.observe(0.3);
        h.observe(0.7);
        h.observe(2.0);
        h.observe(10.0);
        let bs = h.buckets();
        assert_eq!(bs[0], (0.1, 1));
        assert_eq!(bs[1], (0.5, 2));
        assert_eq!(bs[2], (1.0, 3));
        assert_eq!(bs[3], (5.0, 4));
        assert_eq!(h.count(), 5);
        assert!((h.sum() - 13.05).abs() < 1e-9);
    }

    #[test]
    fn histogram_observe_on_boundary_counts_in_bucket() {
        let h = Histogram::new("size", "size", &[1.0, 2.0]);
        h.observe(1.0);
        h.observe(2.0);
        let bs = h.buckets();
        assert_eq!(bs[0], (1.0, 1));
        assert_eq!(bs[1], (2.0, 2));
    }

    #[test]
    fn registry_expose_counter() {
        let r = Registry::new();
        let c = Counter::new("requests_total", "total requests");
        c.inc_by(42);
        r.register(c);
        let text = r.expose();
        assert!(text.contains("# HELP requests_total total requests\n"));
        assert!(text.contains("# TYPE requests_total counter\n"));
        assert!(text.contains("requests_total 42\n"));
    }

    #[test]
    fn registry_expose_gauge_and_histogram() {
        let r = Registry::new();
        let g = Gauge::new("temp_c", "current temperature");
        g.set(21.5);
        r.register(g);
        let h = Histogram::new("rt_s", "round trip", &[0.1, 1.0]);
        h.observe(0.5);
        h.observe(0.2);
        r.register(h);
        let text = r.expose();
        assert!(text.contains("# TYPE temp_c gauge\n"));
        assert!(text.contains("temp_c 21.5\n"));
        assert!(text.contains("# TYPE rt_s histogram\n"));
        assert!(text.contains("rt_s_bucket{le=\"0.1\"} 0\n"));
        assert!(text.contains("rt_s_bucket{le=\"1\"} 2\n"));
        assert!(text.contains("rt_s_bucket{le=\"+Inf\"} 2\n"));
        assert!(text.contains("rt_s_count 2\n"));
        assert!(text.contains("rt_s_sum"));
    }

    #[test]
    fn registry_preserves_registration_order() {
        let r = Registry::new();
        let a = Counter::new("a_total", "a");
        let b = Counter::new("b_total", "b");
        let c = Counter::new("c_total", "c");
        r.register(a);
        r.register(b);
        r.register(c);
        let text = r.expose();
        let ia = text.find("a_total ").unwrap();
        let ib = text.find("b_total ").unwrap();
        let ic = text.find("c_total ").unwrap();
        assert!(ia < ib);
        assert!(ib < ic);
    }

    #[test]
    fn registry_len_and_is_empty() {
        let r = Registry::new();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
        r.register(Counter::new("x", "x"));
        assert!(!r.is_empty());
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn parse_exposition_round_trip() {
        let r = Registry::new();
        let c = Counter::new("hits_total", "hits");
        c.inc_by(7);
        r.register(c);
        let g = Gauge::new("queue_depth", "queue");
        g.set(3.5);
        r.register(g);
        let text = r.expose();
        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix("# HELP ") {
                assert!(rest.contains(' '), "HELP line missing space: {line}");
                continue;
            }
            if let Some(rest) = line.strip_prefix("# TYPE ") {
                let (_name, kind) = rest.split_once(' ').expect("TYPE name kind");
                assert!(
                    matches!(kind, "counter" | "gauge" | "histogram" | "summary"),
                    "unknown TYPE kind: {kind}"
                );
                continue;
            }
            assert!(!line.starts_with('#'), "unknown directive: {line}");
            let (name_part, value_part) = line
                .rsplit_once(' ')
                .unwrap_or_else(|| panic!("sample line missing value: {line}"));
            assert!(!name_part.is_empty(), "empty metric name in: {line}");
            assert!(
                value_part.parse::<f64>().is_ok()
                    || value_part == "NaN"
                    || value_part == "+Inf"
                    || value_part == "-Inf",
                "sample value not numeric: {value_part}"
            );
        }
        assert!(text.contains("hits_total 7\n"));
        assert!(text.contains("queue_depth 3.5\n"));
    }

    #[test]
    fn counter_shared_clone_observes_updates() {
        let c = Counter::new("hits", "hits");
        let c2 = c.clone();
        c.inc_by(5);
        assert_eq!(c2.value(), 5);
        c2.inc();
        assert_eq!(c.value(), 6);
    }
}
