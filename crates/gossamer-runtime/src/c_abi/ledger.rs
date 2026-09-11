//! Allocation ledger: per-family live-object counters, printed at process exit
//! when `GOS_LEAK_LEDGER` is set. Deterministic leak detection - a family whose
//! live count grows with the workload size N (instead of staying O(1)) is
//! leaking. Used to lock leak targets and prove fixes (see
//! `~/dev/contexts/gos/leaks.md`).
//!
//! The counters are `Relaxed` atomics (cheap); the at-exit hook is armed once.

use std::sync::{
    LazyLock,
    atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering},
};

/// Whether any counter, ledger, or sampler is armed for this process.
///
/// The recording hooks sit on the allocation, reference-count, and hash-probe
/// paths, where a program that asked for none of them still has to reach every
/// one. They read this and nothing else, so that program pays a single
/// predictable load per hook instead of a switch of its own.
///
/// Armed eagerly - from the process constructor for the environment switches,
/// and from the calls that turn a scope or the sampler on - because the hooks
/// gate on it: a switch read lazily from inside a hook would never be read at
/// all.
static INSTRUMENTATION: AtomicBool = AtomicBool::new(cfg!(test));

/// Reports whether any instrumentation is armed. The exact switch is
/// re-checked in each recorder's own cold body.
#[inline]
pub(crate) fn instrumentation_armed() -> bool {
    INSTRUMENTATION.load(Ordering::Relaxed)
}

/// Arms the recording hooks. Idempotent; never turned back off, so a hook
/// that has started recording keeps a consistent count.
pub fn arm_instrumentation() {
    INSTRUMENTATION.store(true, Ordering::Relaxed);
}

/// Reads the instrumentation switches once and arms the hooks when any is
/// set. Called from the runtime's process initialisation, before user code
/// runs, so a counter never misses an event that happened before its first
/// hook call.
pub fn init_instrumentation_from_env() {
    let requested = [
        "GOS_VEC_ALLOC_STATS",
        "GOS_RC_ALLOC_STATS",
        "GOS_MAP_ALLOC_STATS",
        "GOS_LEAK_LEDGER",
    ]
    .iter()
    .any(|k| std::env::var_os(k).is_some());
    if requested {
        arm_instrumentation();
    }
}

/// Runtime-managed work observed during one benchmark measurement scope.
///
/// The counters are deliberately thread-local: benchmark targets may run in
/// parallel, and each target must report only the allocations and ownership
/// work performed by its own VM. They cover runtime allocations, reference
/// count operations, and bytes copied by the VM/JIT trampoline, rather than
/// arbitrary Rust allocator activity in the compiler or harness.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BenchmarkCounters {
    /// Runtime-managed allocations, including RC nodes, strings, and Vec storage.
    pub allocations: u64,
    /// Requested bytes for the counted runtime-managed allocations.
    pub allocation_bytes: u64,
    /// Runtime ARC retain operations.
    pub arc_retains: u64,
    /// Runtime ARC release operations.
    pub arc_releases: u64,
    /// VM/JIT aggregate or string marshalling copies.
    pub boundary_copies: u64,
    /// Bytes transferred by VM/JIT marshalling copies.
    pub boundary_copy_bytes: u64,
}

thread_local! {
    static BENCHMARK_COUNTERS: std::cell::Cell<(bool, BenchmarkCounters)> =
        const { std::cell::Cell::new((false, BenchmarkCounters {
            allocations: 0,
            allocation_bytes: 0,
            arc_retains: 0,
            arc_releases: 0,
            boundary_copies: 0,
            boundary_copy_bytes: 0,
        })) };
}

static BENCHMARK_COUNTER_SCOPES: AtomicUsize = AtomicUsize::new(0);

/// Start a fresh per-thread benchmark measurement scope.
pub fn begin_benchmark_counters() {
    arm_instrumentation();
    BENCHMARK_COUNTERS.with(|counters| {
        let (enabled, _) = counters.get();
        if !enabled {
            BENCHMARK_COUNTER_SCOPES.fetch_add(1, Ordering::Relaxed);
        }
        counters.set((true, BenchmarkCounters::default()));
    });
}

/// Stop the current per-thread benchmark measurement scope and return its data.
#[must_use]
pub fn finish_benchmark_counters() -> BenchmarkCounters {
    BENCHMARK_COUNTERS.with(|counters| {
        let (enabled, snapshot) = counters.get();
        counters.set((false, BenchmarkCounters::default()));
        if enabled {
            BENCHMARK_COUNTER_SCOPES.fetch_sub(1, Ordering::Relaxed);
            snapshot
        } else {
            BenchmarkCounters::default()
        }
    })
}

#[inline]
fn with_benchmark_counters(update: impl FnOnce(&mut BenchmarkCounters)) {
    if !instrumentation_armed() {
        return;
    }
    with_benchmark_counters_slow(update);
}

#[cold]
#[inline(never)]
fn with_benchmark_counters_slow(update: impl FnOnce(&mut BenchmarkCounters)) {
    if BENCHMARK_COUNTER_SCOPES.load(Ordering::Relaxed) == 0 {
        return;
    }
    BENCHMARK_COUNTERS.with(|counters| {
        let (enabled, mut snapshot) = counters.get();
        if enabled {
            update(&mut snapshot);
            counters.set((enabled, snapshot));
        }
    });
}

/// Record one runtime-managed allocation and its requested byte footprint.
#[inline]
pub fn benchmark_allocation(bytes: usize) {
    with_benchmark_counters(|counters| {
        counters.allocations = counters.allocations.saturating_add(1);
        counters.allocation_bytes = counters.allocation_bytes.saturating_add(bytes as u64);
    });
}

/// Record one runtime ARC retain.
#[inline]
pub fn benchmark_arc_retain() {
    with_benchmark_counters(|counters| {
        counters.arc_retains = counters.arc_retains.saturating_add(1);
    });
}

/// Record one runtime ARC release.
#[inline]
pub fn benchmark_arc_release() {
    with_benchmark_counters(|counters| {
        counters.arc_releases = counters.arc_releases.saturating_add(1);
    });
}

/// Record one VM/JIT boundary copy and the bytes moved.
#[inline]
pub fn benchmark_boundary_copy(bytes: usize) {
    with_benchmark_counters(|counters| {
        counters.boundary_copies = counters.boundary_copies.saturating_add(1);
        counters.boundary_copy_bytes = counters.boundary_copy_bytes.saturating_add(bytes as u64);
    });
}

pub static AGGR_LIVE: AtomicI64 = AtomicI64::new(0);
pub static RC_LIVE: AtomicI64 = AtomicI64::new(0);
pub static STR_LIVE: AtomicI64 = AtomicI64::new(0);
pub static VEC_LIVE: AtomicI64 = AtomicI64::new(0);
pub static MAP_LIVE: AtomicI64 = AtomicI64::new(0);

// Allocation-shape counters for the compact GosVec layout. These are totals
// rather than live counts: the point is to expose how often a workload pays
// for the header, owner carrier, inline payload, or a split buffer. They are
// reported only when `GOS_VEC_ALLOC_STATS=1` is set.
pub static VEC_INLINE_ALLOCS: AtomicU64 = AtomicU64::new(0);
pub static VEC_SPLIT_ALLOCS: AtomicU64 = AtomicU64::new(0);
pub static VEC_OWNER_ALLOCS: AtomicU64 = AtomicU64::new(0);
pub static VEC_REGION_ALLOCS: AtomicU64 = AtomicU64::new(0);
pub static VEC_REQUESTED_BYTES: AtomicU64 = AtomicU64::new(0);
pub static VEC_USABLE_BYTES: AtomicU64 = AtomicU64::new(0);
pub static VEC_PACKED_CONVERSIONS: AtomicU64 = AtomicU64::new(0);
pub static VEC_PACKED_ROWS: AtomicU64 = AtomicU64::new(0);
pub static VEC_PACKED_BYTES: AtomicU64 = AtomicU64::new(0);

// RC allocation-shape counters.  `payload_bytes` deliberately excludes the
// fixed eight-byte RcHeader, while `usable_bytes` is the allocator capacity
// for the complete block.  Keeping both makes header and allocator-bin costs
// visible for recursive-enum workloads without changing the header ABI.
// Region allocations are reported separately because they are suballocated
// from an arena slab and therefore have no meaningful per-object usable size.
pub static RC_HEAP_ALLOCS: AtomicU64 = AtomicU64::new(0);
pub static RC_REGION_ALLOCS: AtomicU64 = AtomicU64::new(0);
pub static RC_REUSE_ALLOCS: AtomicU64 = AtomicU64::new(0);
pub static RC_PAYLOAD_BYTES: AtomicU64 = AtomicU64::new(0);
pub static RC_USABLE_BYTES: AtomicU64 = AtomicU64::new(0);

// String-key map workload counters. These distinguish borrowed probes from
// the unavoidable owned-key allocation on a first insertion, and make
// formatting's temporary work visible without changing map semantics.
pub static MAP_STR_PROBES: AtomicU64 = AtomicU64::new(0);
pub static MAP_STR_KEY_COPIES: AtomicU64 = AtomicU64::new(0);
pub static MAP_STR_KEY_COPY_BYTES: AtomicU64 = AtomicU64::new(0);
pub static MAP_FORMAT_CALLS: AtomicU64 = AtomicU64::new(0);
pub static MAP_FORMAT_ENTRIES: AtomicU64 = AtomicU64::new(0);

static VEC_ALLOC_STATS_ENABLED: LazyLock<bool> =
    LazyLock::new(|| std::env::var_os("GOS_VEC_ALLOC_STATS").is_some());
static RC_ALLOC_STATS_ENABLED: LazyLock<bool> =
    LazyLock::new(|| std::env::var_os("GOS_RC_ALLOC_STATS").is_some());
static MAP_ALLOC_STATS_ENABLED: LazyLock<bool> =
    LazyLock::new(|| std::env::var_os("GOS_MAP_ALLOC_STATS").is_some());

#[inline]
fn vec_alloc_stats_enabled() -> bool {
    cfg!(test) || *VEC_ALLOC_STATS_ENABLED
}

#[inline]
pub(crate) fn rc_alloc_stats_enabled() -> bool {
    cfg!(test) || *RC_ALLOC_STATS_ENABLED
}

#[inline]
fn map_alloc_stats_enabled() -> bool {
    cfg!(test) || *MAP_ALLOC_STATS_ENABLED
}

#[cfg(all(any(unix, windows), not(miri)))]
static ARMED: std::sync::Once = std::sync::Once::new();

// The report runs from the C runtime's exit path. Declaring `atexit`
// directly rather than through `libc` reaches it on every hosted target,
// including Windows, where `libc` is a unix-only dependency.
#[cfg(all(any(unix, windows), not(miri)))]
unsafe extern "C" {
    fn atexit(callback: extern "C" fn()) -> std::ffi::c_int;
}

#[cfg(all(any(unix, windows), not(miri)))]
extern "C" fn report() {
    if std::env::var("GOS_LEAK_LEDGER").is_ok() {
        eprintln!(
            "LEAK LEDGER (live at exit): aggr={} rc={} str={} vec={} map={}",
            AGGR_LIVE.load(Ordering::SeqCst),
            RC_LIVE.load(Ordering::SeqCst),
            STR_LIVE.load(Ordering::SeqCst),
            VEC_LIVE.load(Ordering::SeqCst),
            MAP_LIVE.load(Ordering::SeqCst),
        );
    }
    if std::env::var("GOS_VEC_ALLOC_STATS").is_ok() {
        eprintln!(
            "VEC ALLOC STATS: inline={} split={} owner={} region={} requested_bytes={} usable_bytes={} packed_conversions={} packed_rows={} packed_bytes={}",
            VEC_INLINE_ALLOCS.load(Ordering::Relaxed),
            VEC_SPLIT_ALLOCS.load(Ordering::Relaxed),
            VEC_OWNER_ALLOCS.load(Ordering::Relaxed),
            VEC_REGION_ALLOCS.load(Ordering::Relaxed),
            VEC_REQUESTED_BYTES.load(Ordering::Relaxed),
            VEC_USABLE_BYTES.load(Ordering::Relaxed),
            VEC_PACKED_CONVERSIONS.load(Ordering::Relaxed),
            VEC_PACKED_ROWS.load(Ordering::Relaxed),
            VEC_PACKED_BYTES.load(Ordering::Relaxed),
        );
    }
    if std::env::var("GOS_RC_ALLOC_STATS").is_ok() {
        eprintln!(
            "RC ALLOC STATS: heap={} region={} reuse={} payload_bytes={} usable_bytes={}",
            RC_HEAP_ALLOCS.load(Ordering::Relaxed),
            RC_REGION_ALLOCS.load(Ordering::Relaxed),
            RC_REUSE_ALLOCS.load(Ordering::Relaxed),
            RC_PAYLOAD_BYTES.load(Ordering::Relaxed),
            RC_USABLE_BYTES.load(Ordering::Relaxed),
        );
    }
    if std::env::var("GOS_MAP_ALLOC_STATS").is_ok() {
        eprintln!(
            "MAP STRING STATS: probes={} key_copies={} key_copy_bytes={} format_calls={} format_entries={}",
            MAP_STR_PROBES.load(Ordering::Relaxed),
            MAP_STR_KEY_COPIES.load(Ordering::Relaxed),
            MAP_STR_KEY_COPY_BYTES.load(Ordering::Relaxed),
            MAP_FORMAT_CALLS.load(Ordering::Relaxed),
            MAP_FORMAT_ENTRIES.load(Ordering::Relaxed),
        );
    }
}

// At-exit auto-print of the ledger covers every target with a C runtime and
// is skipped under Miri, which cannot execute `atexit` (and runs with
// `-Zmiri-ignore-leaks`, so the report is moot there anyway). On a target
// without one - wasm - the counters still tally but the report is read via a
// debugger / explicit query rather than printed at exit.
#[inline]
fn arm() {
    #[cfg(all(any(unix, windows), not(miri)))]
    ARMED.call_once(|| unsafe {
        atexit(report);
    });
}

#[inline]
pub fn aggr_inc() {
    if !instrumentation_armed() {
        return;
    }
    arm();
    AGGR_LIVE.fetch_add(1, Ordering::Relaxed);
}
#[inline]
pub fn aggr_dec() {
    if !instrumentation_armed() {
        return;
    }
    AGGR_LIVE.fetch_sub(1, Ordering::Relaxed);
}
#[inline]
pub fn rc_inc() {
    if !instrumentation_armed() {
        return;
    }
    arm();
    RC_LIVE.fetch_add(1, Ordering::Relaxed);
}
#[inline]
pub fn rc_dec() {
    if !instrumentation_armed() {
        return;
    }
    RC_LIVE.fetch_sub(1, Ordering::Relaxed);
}
#[inline]
pub fn str_inc() {
    if !instrumentation_armed() {
        return;
    }
    arm();
    STR_LIVE.fetch_add(1, Ordering::Relaxed);
}
#[inline]
pub fn str_dec() {
    if !instrumentation_armed() {
        return;
    }
    STR_LIVE.fetch_sub(1, Ordering::Relaxed);
}
#[inline]
pub fn vec_inc() {
    if !instrumentation_armed() {
        return;
    }
    arm();
    VEC_LIVE.fetch_add(1, Ordering::Relaxed);
}
#[inline]
pub fn vec_dec() {
    if !instrumentation_armed() {
        return;
    }
    VEC_LIVE.fetch_sub(1, Ordering::Relaxed);
}

#[inline]
fn record_vec_bytes(requested: usize, usable: usize) {
    VEC_REQUESTED_BYTES.fetch_add(requested as u64, Ordering::Relaxed);
    VEC_USABLE_BYTES.fetch_add(usable as u64, Ordering::Relaxed);
}

/// Record one compact Vec allocation. `usable` is queried from mimalloc when
/// it owns the allocation; sanitizer/Miri/wasm builds report the requested
/// layout size instead because they intentionally use a different allocator.
#[inline]
pub fn vec_inline_alloc(requested: usize, usable: usize) {
    if !instrumentation_armed() {
        return;
    }
    vec_inline_alloc_slow(requested, usable);
}

#[cold]
#[inline(never)]
fn vec_inline_alloc_slow(requested: usize, usable: usize) {
    benchmark_allocation(requested);
    if !vec_alloc_stats_enabled() {
        return;
    }
    arm();
    VEC_INLINE_ALLOCS.fetch_add(1, Ordering::Relaxed);
    record_vec_bytes(requested, usable);
}

#[inline]
pub fn vec_split_alloc(requested: usize, usable: usize) {
    if !instrumentation_armed() {
        return;
    }
    vec_split_alloc_slow(requested, usable);
}

#[cold]
#[inline(never)]
fn vec_split_alloc_slow(requested: usize, usable: usize) {
    benchmark_allocation(requested);
    if !vec_alloc_stats_enabled() {
        return;
    }
    arm();
    VEC_SPLIT_ALLOCS.fetch_add(1, Ordering::Relaxed);
    record_vec_bytes(requested, usable);
}

#[inline]
pub fn vec_owner_alloc(requested: usize, usable: usize) {
    if !instrumentation_armed() {
        return;
    }
    vec_owner_alloc_slow(requested, usable);
}

#[cold]
#[inline(never)]
fn vec_owner_alloc_slow(requested: usize, usable: usize) {
    benchmark_allocation(requested);
    if !vec_alloc_stats_enabled() {
        return;
    }
    arm();
    VEC_OWNER_ALLOCS.fetch_add(1, Ordering::Relaxed);
    record_vec_bytes(requested, usable);
}

#[inline]
pub fn vec_region_alloc(requested: usize) {
    if !instrumentation_armed() {
        return;
    }
    vec_region_alloc_slow(requested);
}

#[cold]
#[inline(never)]
fn vec_region_alloc_slow(requested: usize) {
    benchmark_allocation(requested);
    if !vec_alloc_stats_enabled() {
        return;
    }
    arm();
    VEC_REGION_ALLOCS.fetch_add(1, Ordering::Relaxed);
    // Region slabs are suballocated. Attribute their requested payload but do
    // not pretend the slab's allocator capacity belongs to one Vec.
    record_vec_bytes(requested, requested);
}

#[inline]
pub fn vec_packed_conversion(rows: usize, bytes: usize) {
    if !instrumentation_armed() {
        return;
    }
    vec_packed_conversion_slow(rows, bytes);
}

#[cold]
#[inline(never)]
fn vec_packed_conversion_slow(rows: usize, bytes: usize) {
    if !vec_alloc_stats_enabled() {
        return;
    }
    arm();
    VEC_PACKED_CONVERSIONS.fetch_add(1, Ordering::Relaxed);
    VEC_PACKED_ROWS.fetch_add(rows as u64, Ordering::Relaxed);
    VEC_PACKED_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
}

/// Record an RC allocation's user payload and allocator footprint.
///
/// `usable` includes the compact eight-byte RC header. For a region object it
/// is exactly the requested block size because the object is suballocated from
/// a slab rather than owned by the system allocator. A Perceus reuse records
/// the existing block's usable size and does not increase `heap`.
#[inline]
pub fn rc_alloc(payload: usize, usable: usize, region: bool, reuse: bool) {
    if !instrumentation_armed() {
        return;
    }
    rc_alloc_slow(payload, usable, region, reuse);
}

#[cold]
#[inline(never)]
fn rc_alloc_slow(payload: usize, usable: usize, region: bool, reuse: bool) {
    benchmark_allocation(payload);
    if !rc_alloc_stats_enabled() {
        return;
    }
    arm();
    if region {
        RC_REGION_ALLOCS.fetch_add(1, Ordering::Relaxed);
    } else if reuse {
        RC_REUSE_ALLOCS.fetch_add(1, Ordering::Relaxed);
    } else {
        RC_HEAP_ALLOCS.fetch_add(1, Ordering::Relaxed);
    }
    RC_PAYLOAD_BYTES.fetch_add(payload as u64, Ordering::Relaxed);
    RC_USABLE_BYTES.fetch_add(usable as u64, Ordering::Relaxed);
}

/// Snapshot the RC allocation-shape counters for focused runtime tests and
/// embedders that collect their own benchmark records.
#[must_use]
pub fn rc_alloc_stats() -> (u64, u64, u64, u64, u64) {
    (
        RC_HEAP_ALLOCS.load(Ordering::Relaxed),
        RC_REGION_ALLOCS.load(Ordering::Relaxed),
        RC_REUSE_ALLOCS.load(Ordering::Relaxed),
        RC_PAYLOAD_BYTES.load(Ordering::Relaxed),
        RC_USABLE_BYTES.load(Ordering::Relaxed),
    )
}
#[inline]
pub fn map_inc() {
    if !instrumentation_armed() {
        return;
    }
    arm();
    MAP_LIVE.fetch_add(1, Ordering::Relaxed);
}
#[inline]
pub fn map_dec() {
    if !instrumentation_armed() {
        return;
    }
    MAP_LIVE.fetch_sub(1, Ordering::Relaxed);
}

#[inline]
pub fn map_str_probe() {
    if !instrumentation_armed() {
        return;
    }
    map_str_probe_slow();
}

#[cold]
#[inline(never)]
fn map_str_probe_slow() {
    if !map_alloc_stats_enabled() {
        return;
    }
    arm();
    MAP_STR_PROBES.fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub fn map_str_key_copy(bytes: usize) {
    if !instrumentation_armed() {
        return;
    }
    map_str_key_copy_slow(bytes);
}

#[cold]
#[inline(never)]
fn map_str_key_copy_slow(bytes: usize) {
    if !map_alloc_stats_enabled() {
        return;
    }
    arm();
    MAP_STR_KEY_COPIES.fetch_add(1, Ordering::Relaxed);
    MAP_STR_KEY_COPY_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
}

#[inline]
pub fn map_format(entries: usize) {
    if !instrumentation_armed() {
        return;
    }
    map_format_slow(entries);
}

#[cold]
#[inline(never)]
fn map_format_slow(entries: usize) {
    if !map_alloc_stats_enabled() {
        return;
    }
    arm();
    MAP_FORMAT_CALLS.fetch_add(1, Ordering::Relaxed);
    MAP_FORMAT_ENTRIES.fetch_add(entries as u64, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An armed gate reaches the recorder.
    ///
    /// The switch is one process-wide flag that arming never turns back
    /// off, and every sibling test shares it: a test that disarmed it to
    /// watch a recorder stay quiet would be reading a counter that another
    /// test's map work is free to move the moment anything re-arms. What
    /// this test owns is its own increment, which a monotonic counter
    /// answers whatever else is running.
    #[test]
    fn an_armed_gate_reaches_the_recorder() {
        arm_instrumentation();
        assert!(instrumentation_armed());
        let before = MAP_STR_PROBES.load(Ordering::Relaxed);
        map_str_probe();
        assert!(
            MAP_STR_PROBES.load(Ordering::Relaxed) > before,
            "an armed gate reaches the recorder"
        );
    }

    /// Beginning a measurement scope arms the gate the recorders read, or
    /// the scope would count nothing.
    #[test]
    fn beginning_a_scope_arms_the_gate() {
        let restore = instrumentation_armed();
        INSTRUMENTATION.store(false, Ordering::Relaxed);
        begin_benchmark_counters();
        assert!(instrumentation_armed());
        benchmark_allocation(8);
        let counted = finish_benchmark_counters();
        assert_eq!(counted.allocations, 1);
        INSTRUMENTATION.store(restore, Ordering::Relaxed);
    }

    #[test]
    fn benchmark_counters_are_scoped_and_thread_local() {
        begin_benchmark_counters();
        benchmark_allocation(24);
        benchmark_arc_retain();
        benchmark_arc_release();
        benchmark_boundary_copy(7);
        assert_eq!(
            finish_benchmark_counters(),
            BenchmarkCounters {
                allocations: 1,
                allocation_bytes: 24,
                arc_retains: 1,
                arc_releases: 1,
                boundary_copies: 1,
                boundary_copy_bytes: 7,
            }
        );
        assert_eq!(finish_benchmark_counters(), BenchmarkCounters::default());

        let other = std::thread::spawn(|| {
            begin_benchmark_counters();
            benchmark_allocation(3);
            finish_benchmark_counters()
        })
        .join()
        .expect("benchmark counter thread must finish");
        assert_eq!(other.allocations, 1);
        assert_eq!(other.allocation_bytes, 3);
        assert_eq!(finish_benchmark_counters(), BenchmarkCounters::default());
    }
}
