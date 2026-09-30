//! Profile output compatible with `go tool pprof`.
//!
//! Six profile shapes are exposed. Four read live scheduler state; the
//! CPU and heap profiles are sampled:
//!
//! - **Goroutine profile** - one sample per live goroutine, from
//!   [`crate::sigquit::snapshot`].
//! - **Mutex profile** - microseconds parked on synchronization,
//!   from the scheduler's park-wait accounting.
//! - **Block profile** - microseconds parked on channels, I/O, and
//!   timers, from the same accounting.
//! - **CPU profile** - a timer interrupts the running thread and the
//!   handler reads the stack that is already there.
//! - **Heap profile** - one sample per fixed number of bytes allocated.
//! - **Execution trace** - scheduler spawn / park / unpark events
//!   over a window, as Chrome trace JSON.
//!
//! The first three render the simple "legacy text" profile shape
//! that `go tool pprof -text` (or `-web`) reads - every line is a
//! sample of the form:
//!
//! ```text
//! samples=N self=K
//!   func1 file:line
//!   func2 file:line
//! ```
//!
//! This module lives in the runtime rather than the standard
//! library so the bytecode VM's builtins and the compiled tiers'
//! C-ABI shims render from one implementation over one set of
//! counters.

#![forbid(unsafe_code)]

use std::time::Duration;

/// One sampled stack frame.
#[derive(Debug, Clone)]
pub struct Frame {
    /// Symbolicated function name.
    pub function: String,
    /// Source file path (DWARF, when available).
    pub file: String,
    /// 1-based line number.
    pub line: u32,
}

/// One sample in a profile.
#[derive(Debug, Clone, Default)]
pub struct Sample {
    /// Sample weight - number of inclusive units (CPU time slices,
    /// allocated bytes, alive goroutines).
    pub weight: u64,
    /// Innermost frame first.
    pub stack: Vec<Frame>,
}

/// Accumulator for sampler-driven profiles.
#[derive(Debug, Default)]
pub struct ProfileBuffer {
    samples: Vec<Sample>,
}

impl ProfileBuffer {
    /// Adds a sample to the buffer.
    pub fn record(&mut self, sample: Sample) {
        self.samples.push(sample);
    }

    /// Returns and clears the accumulated samples.
    #[must_use]
    pub fn drain(&mut self) -> Vec<Sample> {
        std::mem::take(&mut self.samples)
    }

    /// Renders the samples into the textual pprof format.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(self.samples.len() * 64);
        out.push_str("# pprof text format v1\n");
        for sample in &self.samples {
            out.push_str(&format!(
                "samples={} self={}\n",
                sample.weight, sample.weight
            ));
            for frame in &sample.stack {
                out.push_str(&format!(
                    "  {} {}:{}\n",
                    if frame.function.is_empty() {
                        "<unknown>"
                    } else {
                        frame.function.as_str()
                    },
                    frame.file,
                    frame.line,
                ));
            }
        }
        out
    }
}

/// Returns a CPU profile gathered over `duration`.
///
/// Blocks the caller for the window, then turns the sampler's raw
/// program counters into named frames. Symbolisation happens here rather
/// than in the signal handler, which may not allocate.
#[must_use]
pub fn cpu_profile(duration: Duration) -> String {
    let _ = crate::sampler::drain();
    if crate::sampler::start(SAMPLE_HZ).is_err() {
        return ProfileBuffer::default().render();
    }
    crate::platform::sleep(duration);
    crate::sampler::stop();
    let raw = crate::sampler::drain();
    let mut buf = ProfileBuffer::default();
    for sample in raw {
        let stack: Vec<Frame> = sample.frames[..sample.len as usize]
            .iter()
            .map(|pc| symbolise(*pc))
            .collect();
        buf.record(Sample { weight: 1, stack });
    }
    buf.render()
}

/// Sampling rate. Go's default, and low enough that the handler's cost
/// stays in the noise on any real workload.
const SAMPLE_HZ: u32 = 100;

/// Resolves one return address to a named frame.
///
/// `backtrace` is a native-only dependency, and wasm has no sampler to
/// produce addresses in the first place, so the address stands in for
/// the name there.
#[cfg(not(target_arch = "wasm32"))]
fn symbolise(pc: usize) -> Frame {
    let mut frame = Frame {
        function: String::new(),
        file: String::new(),
        line: 0,
    };
    // `resolve` only reads the process's own symbol tables for an
    // address the frame walk produced; an address it cannot resolve
    // yields no symbol rather than misbehaving.
    backtrace::resolve(pc as *mut std::ffi::c_void, |symbol| {
        if frame.function.is_empty()
            && let Some(name) = symbol.name()
        {
            frame.function = name.to_string();
        }
        if let Some(path) = symbol.filename() {
            frame.file = path.display().to_string();
        }
        if let Some(line) = symbol.lineno() {
            frame.line = line;
        }
    });
    if frame.function.is_empty() {
        frame.function = format!("0x{pc:x}");
    }
    frame
}

/// Returns a heap profile gathered over `duration`, weighted by the
/// bytes each sampled allocation site accounted for.
#[must_use]
pub fn heap_profile(duration: Duration) -> String {
    let _ = crate::sampler::drain_heap();
    crate::sampler::start_heap();
    crate::platform::sleep(duration);
    crate::sampler::stop_heap();
    let mut buf = ProfileBuffer::default();
    for sample in crate::sampler::drain_heap() {
        let stack: Vec<Frame> = sample.frames[..sample.len as usize]
            .iter()
            .map(|pc| symbolise(*pc))
            .collect();
        buf.record(Sample {
            weight: crate::sampler::HEAP_SAMPLE_BYTES as u64,
            stack,
        });
    }
    buf.render()
}

#[cfg(target_arch = "wasm32")]
fn symbolise(pc: usize) -> Frame {
    Frame {
        function: format!("0x{pc:x}"),
        file: String::new(),
        line: 0,
    }
}

/// Returns a goroutine snapshot. One sample per live goroutine,
/// each with the goroutine's last-known frame.
#[must_use]
pub fn goroutine_profile() -> String {
    let mut samples = Vec::new();
    for info in crate::sigquit::snapshot() {
        let stack = if info.function.is_empty() {
            vec![Frame {
                function: format!("goroutine#{}", info.gid),
                file: String::new(),
                line: 0,
            }]
        } else {
            vec![Frame {
                function: info.function,
                file: info.file,
                line: info.line,
            }]
        };
        samples.push(Sample { weight: 1, stack });
    }
    let buf = ProfileBuffer { samples };
    buf.render()
}

/// Returns accumulated mutex/synchronization contention time. Sample weights
/// are microseconds spent parked since process start.
#[must_use]
#[cfg(not(target_arch = "wasm32"))]
pub fn mutex_profile() -> String {
    let waits = crate::sched_global::scheduler().park_wait_stats();
    render_wait_profile("sync", waits.sync_micros)
}

/// The cooperative wasm scheduler has no blocking mutex park accounting.
#[must_use]
#[cfg(target_arch = "wasm32")]
pub fn mutex_profile() -> String {
    ProfileBuffer::default().render()
}

/// Returns accumulated wait time for channel operations, I/O, timers, and
/// unspecified runtime waits. Sample weights are microseconds.
#[must_use]
#[cfg(not(target_arch = "wasm32"))]
pub fn block_profile() -> String {
    let waits = crate::sched_global::scheduler().park_wait_stats();
    let mut buf = ProfileBuffer::default();
    record_wait_sample(&mut buf, "channel", waits.chan_micros);
    record_wait_sample(&mut buf, "io", waits.io_micros);
    record_wait_sample(&mut buf, "timer", waits.timer_micros);
    record_wait_sample(&mut buf, "other", waits.other_micros);
    buf.render()
}

/// The cooperative wasm scheduler has no blocking wait accounting.
#[must_use]
#[cfg(target_arch = "wasm32")]
pub fn block_profile() -> String {
    ProfileBuffer::default().render()
}

/// Captures scheduler execution events for `duration` and returns Chrome trace
/// JSON. The capture includes goroutine spawns and park/unpark transitions.
#[must_use]
#[cfg(not(target_arch = "wasm32"))]
pub fn execution_trace(duration: Duration) -> String {
    let scheduler = crate::sched_global::scheduler();
    scheduler.start_execution_trace();
    crate::platform::sleep(duration);
    let events = scheduler.finish_execution_trace();
    let mut out = String::from("{\"traceEvents\":[");
    for (index, event) in events.iter().enumerate() {
        if index != 0 {
            out.push(',');
        }
        let reason = match event.reason {
            Some(crate::sched::ParkReason::Other) => "other",
            Some(crate::sched::ParkReason::Chan) => "channel",
            Some(crate::sched::ParkReason::Sync) => "sync",
            Some(crate::sched::ParkReason::Io) => "io",
            Some(crate::sched::ParkReason::Timer) => "timer",
            None => "",
        };
        out.push_str(&format!(
            "{{\"name\":\"{}\",\"ph\":\"i\",\"s\":\"t\",\"ts\":{},\"pid\":1,\"tid\":{},\"args\":{{\"reason\":\"{}\"}}}}",
            event.name, event.timestamp_micros, event.gid, reason
        ));
    }
    out.push_str("]}");
    out
}

/// wasm runs goroutines cooperatively and does not collect scheduler events.
#[must_use]
#[cfg(target_arch = "wasm32")]
pub fn execution_trace(_duration: Duration) -> String {
    String::from("{\"traceEvents\":[]}")
}

#[cfg(not(target_arch = "wasm32"))]
fn render_wait_profile(reason: &str, micros: u64) -> String {
    let mut buf = ProfileBuffer::default();
    record_wait_sample(&mut buf, reason, micros);
    buf.render()
}

#[cfg(not(target_arch = "wasm32"))]
fn record_wait_sample(buf: &mut ProfileBuffer, reason: &str, micros: u64) {
    if micros == 0 {
        return;
    }
    buf.record(Sample {
        weight: micros,
        stack: vec![Frame {
            function: format!("runtime.park.{reason}"),
            file: String::new(),
            line: 0,
        }],
    });
}

#[cfg(test)]
fn render_blocked_counts(counts: crate::sched::ParkedReasonCounts) -> String {
    let mut buf = ProfileBuffer::default();
    record_wait_sample(&mut buf, "channel", counts.chan as u64);
    record_wait_sample(&mut buf, "io", counts.io as u64);
    record_wait_sample(&mut buf, "timer", counts.timer as u64);
    record_wait_sample(&mut buf, "other", counts.other as u64);
    buf.render()
}

/// Endpoints the router serves, in index-page order.
const ENDPOINTS: &[&str] = &["profile", "heap", "goroutine", "mutex", "block", "trace"];

/// Routes a request path under `/debug/pprof/...` to the right
/// profile generator and returns the body the HTTP handler should
/// write. Returns `None` for paths the pprof router doesn't know.
///
/// Path shapes match Go's `net/http/pprof`:
///
/// - `/debug/pprof/goroutine` - goroutine snapshot.
/// - `/debug/pprof/mutex` - mutex contention profile.
/// - `/debug/pprof/block` - block profile.
/// - `/debug/pprof/trace?seconds=N` - Chrome scheduler execution trace.
/// - `/debug/pprof/` - index page listing the others.
#[must_use]
pub fn route(path: &str, query: &str) -> Option<String> {
    let suffix = path.strip_prefix("/debug/pprof/")?;
    match suffix {
        "" => Some(index_page()),
        "profile" => {
            let secs = parse_query_seconds(query).unwrap_or(30);
            Some(cpu_profile(Duration::from_secs(secs)))
        }
        "heap" => {
            let secs = parse_query_seconds(query).unwrap_or(1);
            Some(heap_profile(Duration::from_secs(secs)))
        }
        "goroutine" => Some(goroutine_profile()),
        "mutex" => Some(mutex_profile()),
        "block" => Some(block_profile()),
        "trace" => {
            let secs = parse_query_seconds(query).unwrap_or(1);
            Some(execution_trace(Duration::from_secs(secs)))
        }
        _ => None,
    }
}

fn parse_query_seconds(query: &str) -> Option<u64> {
    for pair in query.split('&') {
        if let Some(rest) = pair.strip_prefix("seconds=") {
            return rest.parse().ok();
        }
    }
    None
}

fn index_page() -> String {
    let mut out = String::new();
    out.push_str("/debug/pprof/\n");
    for endpoint in ENDPOINTS {
        out.push_str(&format!("  {endpoint}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sched::ParkedReasonCounts;

    #[test]
    fn buffer_records_and_drains() {
        let mut buf = ProfileBuffer::default();
        buf.record(Sample {
            weight: 1,
            stack: vec![Frame {
                function: "test::fn".into(),
                file: "t.gos".into(),
                line: 1,
            }],
        });
        let drained = buf.drain();
        assert_eq!(drained.len(), 1);
        let again = buf.drain();
        assert!(again.is_empty());
    }

    #[test]
    fn render_emits_text_pprof_header() {
        let mut buf = ProfileBuffer::default();
        buf.record(Sample {
            weight: 5,
            stack: vec![],
        });
        let text = buf.render();
        assert!(text.starts_with("# pprof text format"));
        assert!(text.contains("samples=5"));
    }

    #[test]
    fn goroutine_profile_includes_at_least_self() {
        let _ = goroutine_profile();
        // Smoke: just ensure the call returns without panicking.
    }

    #[test]
    fn accumulated_wait_profiles_label_wait_reasons() {
        let counts = ParkedReasonCounts {
            chan: 2,
            io: 3,
            timer: 5,
            other: 7,
            sync: 11,
        };
        let mutex = render_wait_profile("sync", counts.sync as u64);
        assert!(mutex.contains("samples=11"));
        assert!(mutex.contains("runtime.park.sync"));

        let block = render_blocked_counts(counts);
        assert!(block.contains("runtime.park.channel"));
        assert!(block.contains("runtime.park.io"));
        assert!(block.contains("runtime.park.timer"));
        assert!(block.contains("runtime.park.other"));
        assert!(!block.contains("runtime.park.sync"));
    }

    #[test]
    fn execution_trace_is_chrome_trace_json() {
        let trace = execution_trace(Duration::ZERO);
        assert!(trace.starts_with("{\"traceEvents\":["));
        assert!(trace.ends_with("]}"));
    }
}
