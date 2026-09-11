//! Runtime support library linked into every Gossamer program.
//! Provides the C-ABI shims, allocator, scheduler, and
//! reference-counting memory management that compiled and
//! interpreted programs share.

// `c_abi` requires unsafe for `#[no_mangle] extern "C"` symbols and
// raw-pointer dispatch. The rest of the crate stays safe by
// scoping unsafe blocks inside that module.

// Process-wide allocator for the Gossamer toolchain and every compiled
// program. Replacing the platform default - notably musl's malloc on the
// static-musl release link path and Windows ucrt HeapAlloc, both slow under
// goroutine-heavy allocation contention - equalises allocator performance
// across Linux, macOS, and Windows. Defined here so the single definition
// covers both the `gos` binary (which links this crate as an rlib) and every
// program `gos build` links against libgossamer_runtime.a.
// ThreadSanitizer is incompatible with a custom global allocator:
// mimalloc's lazy global lock init (`mi_lock_init`) memcpys a shared
// lock with no synchronisation on a thread's first allocation, which
// TSan correctly flags as a data race, and an uninstrumented allocator
// blinds TSan to real heap races regardless. Under `-Zsanitizer=thread`
// fall back to the default system allocator, which TSan instruments -
// the standard practice for custom allocators under sanitizers
// (jemalloc / mimalloc document the same). Miri cannot execute
// mimalloc's foreign allocation functions at all - it models the
// default Rust allocator and rejects the C `mi_malloc_aligned`
// call - so it falls back to the system allocator for the same
// reason. The cargo-fuzz harness (`--cfg fuzzing`) likewise falls
// back: it runs many independent programs in one long-lived process
// under ASan, where the system allocator keeps RSS bounded across
// iterations and lets ASan instrument the heap (a custom global
// allocator blinds it). Every other build - release, debug, the
// standalone ASan job, every compiled program - keeps mimalloc.
// wasm32-unknown-unknown has no mimalloc backend (it links no libc and
// mimalloc's C arena code does not target it); the browser playground
// uses the default dlmalloc that ships with the wasm std.
#[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
#[global_allocator]
static GLOBAL_ALLOCATOR: SamplingAllocator = SamplingAllocator;

/// mimalloc, with a byte counter in front of it for heap profiles.
///
/// Until sampling is armed the hook is one relaxed load and a
/// never-taken branch. Every method carries `#[inline]` so that this
/// wrapper collapses into `__rust_alloc` alongside mimalloc's own
/// inlined fast path, which is what keeps allocation-bound programs at
/// the unwrapped allocator's speed. Recording a sample walks the stack
/// into a fixed buffer: the recorder runs inside the allocator and so
/// must not allocate.
#[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
struct SamplingAllocator;

#[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
// SAFETY: every method forwards to mimalloc, which upholds the
// `GlobalAlloc` contract; the sampling hook only reads the layout size.
unsafe impl std::alloc::GlobalAlloc for SamplingAllocator {
    #[inline]
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        let ptr = unsafe { mimalloc::MiMalloc.alloc(layout) };
        if !ptr.is_null() {
            crate::sampler::record_allocation(layout.size());
        }
        ptr
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract.
        unsafe { mimalloc::MiMalloc.dealloc(ptr, layout) }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        // SAFETY: the caller upholds the contract.
        let ptr = unsafe { mimalloc::MiMalloc.alloc_zeroed(layout) };
        if !ptr.is_null() {
            crate::sampler::record_allocation(layout.size());
        }
        ptr
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller upholds the contract.
        let out = unsafe { mimalloc::MiMalloc.realloc(ptr, layout, new_size) };
        if !out.is_null() && new_size > layout.size() {
            crate::sampler::record_allocation(new_size - layout.size());
        }
        out
    }
}

// mimalloc's `mi_option_purge_delay` (15),
// `mi_option_deprecated_max_segment_reclaim` (21), and
// `mi_option_allow_thp` (43) enum indices. The `libmimalloc-sys` binding
// exposes only a curated subset of the option enum that omits these on the
// v3 build, so they are pinned by value and guarded against a future mimalloc
// enum shift by `allocator_tests` below.
#[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
const MI_OPTION_PURGE_DELAY: libmimalloc_sys::mi_option_t = 15;
#[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
const MI_OPTION_MAX_SEGMENT_RECLAIM: libmimalloc_sys::mi_option_t = 21;
#[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
const MI_OPTION_ALLOW_THP: libmimalloc_sys::mi_option_t = 43;

// Factory defaults of the pinned options, captured the first time the
// allocator is configured - before `init_process_allocator` overwrites them.
// The index-shift guard reads this snapshot instead of a live
// `mi_option_get`: option state is process-global, so once any caller runs
// the init the live values read back as our tuned values, and the test runner
// orders tests non-deterministically. Snapshotting under the `OnceLock`
// before the first set makes the guard observe the true defaults regardless
// of ordering.
#[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
static MIMALLOC_OPTION_DEFAULTS: std::sync::OnceLock<(i64, i64, i64)> = std::sync::OnceLock::new();

/// Pristine defaults of the two pinned mimalloc options, snapshotted once
/// before the first `init_process_allocator` call mutates them.
#[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
fn mimalloc_option_defaults() -> (i64, i64, i64) {
    *MIMALLOC_OPTION_DEFAULTS.get_or_init(|| {
        // SAFETY: option get is thread-safe; the global allocator is mimalloc
        // in this build, initialised before any code reaches here.
        unsafe {
            (
                widen(libmimalloc_sys::mi_option_get(MI_OPTION_PURGE_DELAY)),
                widen(libmimalloc_sys::mi_option_get(
                    MI_OPTION_MAX_SEGMENT_RECLAIM,
                )),
                widen(libmimalloc_sys::mi_option_get(MI_OPTION_ALLOW_THP)),
            )
        }
    })
}

/// Widens a mimalloc option value to `i64`, whatever width `c_long`
/// carries on the target (32 bits on LLP64 Windows, 64 elsewhere).
#[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
fn widen(value: impl Into<i64>) -> i64 {
    value.into()
}

/// Default mimalloc purge delay for release programs, in milliseconds.
///
/// How long a freed page waits before its memory is handed back to the
/// kernel. This is mimalloc's own default, which the runtime keeps: the
/// delay has to outlast an allocate-and-free cycle, or a loop that builds
/// and drops a container pays an `madvise` round trip per iteration.
///
/// Resident memory does not pay for the wait. What a program holds is
/// decided by its live set and by the segments mimalloc keeps whole, and
/// neither moves with this delay - a burst allocated and dropped holds the
/// same bytes a second later whatever it is set to. Set
/// `GOS_ALLOC_PURGE_DELAY` to another millisecond value to tune it, or to
/// `0` for immediate return-to-OS behaviour.
#[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
fn configured_purge_delay() -> std::os::raw::c_long {
    std::env::var("GOS_ALLOC_PURGE_DELAY")
        .ok()
        .and_then(|s| s.parse::<std::os::raw::c_long>().ok())
        .unwrap_or(1000)
}

/// Configures the process allocator for a predictable memory footprint.
///
/// Keeps mimalloc's own purge delay, for the reason
/// [`configured_purge_delay`] gives. Set `GOS_ALLOC_PURGE_DELAY=0` to opt
/// into immediate return-to-OS behavior, or another millisecond value to
/// tune batching.
///
/// Compiled Gossamer programs reach this from their generated `main` via
/// `gos_rt_set_args` -> `runtime_init`; the `gos` binary (which links
/// this crate as an rlib and so shares the same mimalloc) calls it once
/// at startup so `gos` and the toolchain benefit too. Safe wrapper so
/// callers under `#![forbid(unsafe_code)]` (the `gos` `main`) can invoke
/// it. No-op under `ThreadSanitizer` and Miri, where the system
/// allocator is used.
///
/// Raises mimalloc's abandoned-segment reclaim ceiling so short-lived
/// worker heaps do not leave reclaimable segments behind after a collection
/// pass, and disables mimalloc's transparent-huge-page request. By default
/// mimalloc `madvise(MADV_HUGEPAGE)`s its arena memory, so on Linux with
/// THP in `madvise` mode the kernel backs even a tiny live set with
/// 2 MiB pages - a process whose heap fits in tens of KiB stays several
/// MiB resident - and mimalloc widens its minimal purge size to 2 MiB to
/// match. Disabling it costs no measurable wall-clock on our workloads and
/// keeps RSS proportional to the live set.
///
/// `15`, `21`, and `43` are mimalloc's `mi_option_purge_delay`,
/// `mi_option_deprecated_max_segment_reclaim`, and `mi_option_allow_thp`.
/// The `libmimalloc-sys` binding exposes only a curated subset of the option
/// enum on v3, so they are pinned by value and guarded against a future
/// mimalloc enum shift by the `allocator_tests` unit tests below, which assert
/// each index still reports its documented default.
pub fn init_process_allocator() {
    // Settle the instrumentation switches before any user code runs: the
    // recording hooks gate on the flag this arms, so a switch read later,
    // from inside a hook, would never be read at all.
    crate::c_abi::ledger::init_instrumentation_from_env();
    #[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
    {
        // Capture the pristine defaults before overwriting the options we do
        // tune, so the index-shift guard test can still read them once this
        // has run.
        let _ = mimalloc_option_defaults();
        // SAFETY: `mi_option_set` is thread-safe and valid any time after
        // the allocator initialised, which it has by the time `main` runs.
        unsafe {
            libmimalloc_sys::mi_option_set(MI_OPTION_PURGE_DELAY, configured_purge_delay());
            libmimalloc_sys::mi_option_set(MI_OPTION_MAX_SEGMENT_RECLAIM, 100);
            libmimalloc_sys::mi_option_set(MI_OPTION_ALLOW_THP, 0);
        }
    }
}

/// Forces the process allocator to collect abandoned heaps and purge eligible
/// pages. This is a no-op in builds that do not use mimalloc.
pub fn collect_process_allocator(force: bool) {
    #[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
    {
        // SAFETY: `mi_collect` is process-global and thread-safe in mimalloc.
        unsafe { libmimalloc_sys::mi_collect(force) };
    }
    #[cfg(any(tsan, miri, fuzzing, target_arch = "wasm32"))]
    {
        let _ = force;
    }
}

pub mod accept;
pub mod builtins;
pub mod c_abi;
pub mod clock;
pub mod comptime_inputs;
pub mod comptime_paths;
pub mod comptime_policy;
pub mod coverage;
pub mod fs_mode;
pub mod platform;
pub mod pprof;
pub mod preempt;
pub mod race;
pub mod replay;
pub mod safe_daemon;
pub mod safe_env;
pub mod sampler;
pub mod sched;
#[cfg(not(target_arch = "wasm32"))]
pub mod smtp;
// The process-global scheduler singleton ties together OS worker
// threads and a mio netpoller. The wasm playground links a
// cooperative single-threaded equivalent (eager goroutines; a
// would-be block diverges through `gossamer_coro::suspend`).
#[cfg(not(target_arch = "wasm32"))]
pub mod sched_global;
#[cfg(target_arch = "wasm32")]
#[path = "sched_global_wasm.rs"]
pub mod sched_global;
pub mod sigquit;
pub mod sql;
pub mod sql_migrate;
pub mod sql_pool;
pub mod stack_guard;
pub mod value;

// Native-only runtime services that pull crates with no wasm32 build:
// `ffi` (libloading dynamic loading) and `http2_server` (h2 / tokio).
// The wasm VM never needs either; native is unaffected.
#[cfg(not(target_arch = "wasm32"))]
pub mod ffi;
#[cfg(not(target_arch = "wasm32"))]
pub mod http2_server;

// Re-export preempt-check FFI symbols so JIT-side
// `rt::gos_rt_preempt_check{,_and_yield}` lookups resolve through
// the crate root rather than the `preempt` submodule path. The
// `#[unsafe(no_mangle)]` attribute on each function gives the
// linker a single canonical symbol; the re-export only affects the
// Rust-side path.
pub use preempt::{gos_rt_preempt_check, gos_rt_preempt_check_and_yield};
pub use value::{
    GossamerValue, SINGLETON_FALSE, SINGLETON_TRUE, SINGLETON_UNIT, TAG_FLOAT, TAG_HEAP,
    TAG_IMMEDIATE, TAG_MASK, TAG_SINGLETON, fits_i56, from_f64, from_heap_handle, from_i64,
    from_singleton, tag_of, to_f64, to_heap_handle, to_i64, to_singleton,
};

#[cfg(all(test, not(any(tsan, miri, fuzzing, target_arch = "wasm32"))))]
mod allocator_tests {
    /// Guards the `mi_option_purge_delay` (15) and `mi_option_allow_thp`
    /// (43) enum indices that [`super::init_process_allocator`] pins by
    /// value, since the `libmimalloc-sys` binding doesn't name them. The
    /// documented defaults are 1000 ms and 1 (Android, the one platform
    /// defaulting `allow_thp` to 0, is not a target); if a mimalloc bump
    /// shifts the enum so either index names a different option, its
    /// default check fails loudly - the signal to re-verify the indices
    /// for the new version. The defaults come from
    /// [`super::mimalloc_option_defaults`], which snapshots them before the
    /// first init overwrites them, so this stays correct regardless of
    /// whether another test has already configured the process-global
    /// allocator.
    #[test]
    fn allocator_option_indices_are_pinned_and_settable() {
        let (purge_default, reclaim_default, thp_default) = super::mimalloc_option_defaults();
        assert_eq!(
            purge_default, 1000,
            "mimalloc option 15 default is {purge_default}, expected the 1000 ms \
             purge_delay default - the enum likely shifted; re-verify the \
             mi_option_purge_delay index for the current mimalloc version",
        );
        assert_eq!(
            reclaim_default, 10,
            "mimalloc option 21 default is {reclaim_default}, expected the abandoned-segment \
             reclaim default - the enum likely shifted; re-verify the \
             mi_option_deprecated_max_segment_reclaim index for the current mimalloc version",
        );
        assert_eq!(
            thp_default, 1,
            "mimalloc option 43 default is {thp_default}, expected the allow_thp \
             default of 1 - the enum likely shifted; re-verify the \
             mi_option_allow_thp index for the current mimalloc version",
        );
        super::init_process_allocator();
        // SAFETY: option get is thread-safe; the global allocator is mimalloc
        // in a non-tsan build, so it is initialised here.
        let purge = unsafe { libmimalloc_sys::mi_option_get(super::MI_OPTION_PURGE_DELAY) };
        let expected_purge = super::configured_purge_delay();
        assert_eq!(
            purge, expected_purge,
            "init_process_allocator must set purge_delay to the configured release value"
        );
        // SAFETY: option get is thread-safe; the global allocator is mimalloc
        // in a non-tsan build, so it is initialised here.
        let reclaim =
            unsafe { libmimalloc_sys::mi_option_get(super::MI_OPTION_MAX_SEGMENT_RECLAIM) };
        assert_eq!(
            reclaim, 100,
            "init_process_allocator must set max_segment_reclaim to 100"
        );
        // SAFETY: option get is thread-safe; the global allocator is mimalloc
        // in a non-tsan build, so it is initialised here.
        let thp = unsafe { libmimalloc_sys::mi_option_get(super::MI_OPTION_ALLOW_THP) };
        assert_eq!(thp, 0, "init_process_allocator must set allow_thp to 0");
    }
}
