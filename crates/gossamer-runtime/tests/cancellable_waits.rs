//! Signal and descriptor waits end when the cohort they wait under is
//! cancelled: [`wake_cancellable_waits`] wakes them and each re-checks the
//! predicate its caller supplied.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

#[cfg(unix)]
use gossamer_runtime::c_abi::fd;
use gossamer_runtime::c_abi::{signal, wake_cancellable_waits};

/// Runs `wait` on a thread, cancels it once it is blocked, and answers what
/// it returned. A wait that never ends fails the receive.
fn cancel_while_blocked<T: Send + 'static>(
    wait: impl FnOnce(Arc<AtomicBool>) -> T + Send + 'static,
) -> T {
    let cancelled = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let flag = Arc::clone(&cancelled);
    std::thread::spawn(move || {
        let _ = tx.send(wait(flag));
    });
    // The wait has not ended on its own: nothing else can end it.
    assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
    cancelled.store(true, Ordering::Release);
    wake_cancellable_waits();
    rx.recv_timeout(Duration::from_secs(10))
        .expect("a cancelled wait returns once woken")
}

#[test]
fn signal_wait_returns_false_when_its_cohort_is_cancelled() {
    let handle = signal::gos_rt_signal_on(signal_number());
    let fired = cancel_while_blocked(move |flag| {
        signal::wait_until(handle, move || flag.load(Ordering::Acquire))
    });
    assert!(!fired);
}

#[cfg(unix)]
#[test]
fn descriptor_wait_returns_false_when_its_cohort_is_cancelled() {
    let mut ends = [0i32; 2];
    // SAFETY: `ends` has room for the two descriptors `pipe` writes.
    assert_eq!(unsafe { libc::pipe(ends.as_mut_ptr()) }, 0);
    let read_end = i64::from(ends[0]);
    let ready = cancel_while_blocked(move |flag| {
        fd::wait(read_end, false, -1, move || flag.load(Ordering::Acquire))
    });
    assert_eq!(ready, Ok(false));
    // SAFETY: both ends belong to this test.
    unsafe {
        libc::close(ends[0]);
        libc::close(ends[1]);
    }
}

#[cfg(unix)]
#[test]
fn descriptor_wait_still_reports_readiness_after_a_foreign_wake() {
    let mut ends = [0i32; 2];
    // SAFETY: `ends` has room for the two descriptors `pipe` writes.
    assert_eq!(unsafe { libc::pipe(ends.as_mut_ptr()) }, 0);
    let (read_end, write_end) = (i64::from(ends[0]), ends[1]);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(fd::wait(read_end, false, -1, || false));
    });
    // A cancellation elsewhere wakes any wait in progress; this one is not
    // cancelled, so it goes back to waiting.
    wake_cancellable_waits();
    assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
    // SAFETY: `write_end` is open and the buffer holds one byte.
    let written = unsafe { libc::write(write_end, [7u8].as_ptr().cast(), 1) };
    assert_eq!(written, 1);
    assert_eq!(rx.recv_timeout(Duration::from_secs(10)), Ok(Ok(true)));
    // SAFETY: both ends belong to this test.
    unsafe {
        libc::close(ends[0]);
        libc::close(ends[1]);
    }
}

/// A signal number the test process never receives.
fn signal_number() -> i32 {
    if cfg!(target_os = "macos") { 31 } else { 12 }
}
