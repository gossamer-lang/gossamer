//! Runtime support for `std::thread` on the compiled tiers.

#![allow(clippy::missing_safety_doc)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::cast_possible_wrap)]

/// `thread::num_cpus() -> i64` - logical CPU count, at least 1.
/// Mirrors `gossamer_std::thread::num_cpus` so the compiled tiers
/// agree bit-for-bit with the interpreter.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_thread_num_cpus() -> i64 {
    ffi_entry!(1, {
        std::thread::available_parallelism().map_or(1, |n| n.get() as i64)
    })
}

/// The scheduler's counters and the blocking pool's load as one compact JSON
/// object: what `runtime::scheduler_stats_json()` answers on the compiled
/// tiers and from Rust.
#[must_use]
pub fn scheduler_stats_json() -> String {
    let scheduler = crate::sched_global::scheduler();
    let stats = scheduler.stats();
    let blocking = crate::blocking_pool::stats();
    format!(
        "{{\"spawned\":{},\"finished\":{},\"steps\":{},\"yields\":{},\"steals\":{},\"injects\":{},\"parks\":{},\"unparks\":{},\"live_goroutines\":{},\"worker_count\":{},\"worker_count_cap\":{},\"blocking_threads\":{},\"blocking_queued\":{},\"blocking_admission_waiters\":{},\"blocking_oldest_queued_ms\":{}}}",
        stats.spawned,
        stats.finished,
        stats.steps,
        stats.yields,
        stats.steals,
        stats.injects,
        stats.parks,
        stats.unparks,
        scheduler.live_goroutines(),
        scheduler.worker_count(),
        crate::sched::MultiScheduler::worker_count_cap(),
        blocking.threads,
        blocking.queued,
        blocking.admission_waiters,
        blocking.oldest_queued_ms,
    )
}

/// `runtime::scheduler_stats_json() -> String` - low-overhead snapshot
/// of the global goroutine scheduler counters.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_runtime_scheduler_stats_json() -> *mut std::os::raw::c_char {
    ffi_entry!(std::ptr::null_mut(), {
        crate::c_abi::alloc_cstring(scheduler_stats_json().as_bytes())
    })
}

/// `runtime::cycle_collection_supported() -> bool` - compiled tiers run the
/// native trial-deletion collector, unlike the Arc-backed bytecode VM.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_runtime_cycle_collection_supported() -> bool {
    true
}
