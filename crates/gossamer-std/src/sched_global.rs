//! Thin facade over the process-wide scheduler + netpoller singleton
//! that lives inside `gossamer-runtime::sched_global`.
//!
//! Stdlib code (`time::sleep`, `net::Tcp*`, `signal`, etc.) imports
//! the scheduler / poller through this module so the same import path
//! works for the interpreter (which links `gossamer-std`) and for
//! third-party Rust callers that pull the stdlib in for tooling.
//! Compiled binaries link the scheduler via `gossamer-runtime`
//! directly and never see this module.

#![forbid(unsafe_code)]

use std::io;

use gossamer_runtime::platform::Instant;

pub use gossamer_runtime::sched::{Gid, Interest, MultiScheduler, OsPoller, ParkReason, Readiness};
pub use gossamer_runtime::sched_global::{Parker, current_gid, park, wait_io, wait_io_until};

/// Returns a handle to the process-wide scheduler. The first caller
/// boots both the scheduler and the netpoller thread.
#[must_use]
pub fn scheduler() -> &'static MultiScheduler {
    gossamer_runtime::sched_global::scheduler()
}

/// Allocates a fresh [`Gid`] for use as a runtime-internal wait
/// handle (timers, blocking pool callbacks). These ids do not
/// correspond to user-spawned goroutines.
#[must_use]
pub fn alloc_runtime_gid() -> Gid {
    gossamer_runtime::sched_global::alloc_runtime_gid()
}

/// Registers `waker` to be invoked when the poller delivers the
/// next event tagged with `gid`.
pub fn register_waker(gid: Gid, waker: Box<dyn Fn() + Send + Sync>) {
    gossamer_runtime::sched_global::register_waker(gid, waker);
}

/// Removes any waker associated with `gid`.
pub fn forget_waker(gid: Gid) {
    gossamer_runtime::sched_global::forget_waker(gid);
}

pub use gossamer_runtime::sched_global::TimerHandle;

/// Arms a one-shot timer that runs `waker` once `deadline` passes. The
/// waker is registered before the timer can fire.
#[must_use]
pub fn add_timer(deadline: Instant, waker: Box<dyn Fn() + Send + Sync>) -> TimerHandle {
    gossamer_runtime::sched_global::add_timer(deadline, waker)
}

/// Disarms a timer armed by [`add_timer`] and drops its waker.
pub fn cancel_timer(timer: TimerHandle) {
    gossamer_runtime::sched_global::cancel_timer(timer);
}

/// Sleeps the calling OS thread until `deadline` using the netpoller's
/// internal timer wheel + a thread parking primitive.
pub fn sleep_until(deadline: Instant) {
    gossamer_runtime::sched_global::sleep_until(deadline);
}

/// Borrows the netpoller for a closure. Used by the I/O bridge code
/// in `net.rs`.
pub fn with_poller<R>(f: impl FnOnce(&mut OsPoller) -> R) -> R {
    gossamer_runtime::sched_global::with_poller(f)
}

/// Convenience wrapper around `Poller::poll(0)` that returns any
/// already-ready events without blocking.
///
/// # Errors
///
/// Returns `io::Error` when the poller's underlying `epoll`/`kqueue`
/// rejects the call.
pub fn drain_ready() -> io::Result<Vec<Readiness>> {
    gossamer_runtime::sched_global::drain_ready()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn sleep_until_returns_promptly() {
        let start = Instant::now();
        sleep_until(start + Duration::from_millis(20));
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(15));
        assert!(elapsed < Duration::from_millis(500));
    }
}
