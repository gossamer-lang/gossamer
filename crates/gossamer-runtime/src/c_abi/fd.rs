//! `std::os::fd`: waiting for a descriptor (on Windows, a handle) to become
//! readable or writable without holding a scheduler worker.
//!
//! The wait runs `poll(2)` (on Windows, `WaitForMultipleObjects`) on the
//! blocking pool while the calling goroutine is parked. `poll` answers for
//! every kind of descriptor a program holds, a terminal included, where the
//! kernel event queues the netpoller uses reject some of them (kqueue refuses
//! tty devices on macOS).
//!
//! Each wait also watches a wake object of its own (a pipe, or an event on
//! Windows) that cohort cancellation signals through [`wake_waits`], so a
//! wait under a cancelled cohort ends instead of holding its pool thread.

#![allow(unsafe_code)]

#[cfg(any(unix, windows))]
use std::time::{Duration, Instant};

/// Waits up to `timeout_ms` milliseconds (forever when negative) for `fd` to
/// be readable, or writable when `writable`. Answers whether it became
/// ready - `false` when the timeout passed or `cancelled` came to hold - or
/// the OS error that ended the wait. `cancelled` is re-checked whenever
/// [`wake_waits`] runs.
///
/// # Errors
///
/// The descriptor is not open, or the platform reports a failure.
pub fn wait(
    fd: i64,
    writable: bool,
    timeout_ms: i64,
    cancelled: impl Fn() -> bool + Send + 'static,
) -> Result<bool, String> {
    crate::sched_global::run_blocking("fd::wait", move || {
        platform::wait(fd, writable, timeout_ms, &cancelled)
    })?
}

/// Wake objects of the descriptor waits in progress, by wait id: a pipe's
/// write end on Unix, an event handle on Windows.
#[cfg(any(unix, windows))]
static WAKES: std::sync::LazyLock<parking_lot::Mutex<std::collections::HashMap<u64, i64>>> =
    std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

#[cfg(any(unix, windows))]
static NEXT_WAKE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Signals every descriptor wait in progress, so each re-checks whether the
/// cohort it waits under was cancelled.
pub fn wake_waits() {
    #[cfg(any(unix, windows))]
    for wake in WAKES.lock().values() {
        platform::signal_wake(*wake);
    }
}

/// A wait's registration in [`WAKES`], removed when the wait ends.
#[cfg(any(unix, windows))]
struct WakeEntry(u64);

#[cfg(any(unix, windows))]
impl WakeEntry {
    fn register(wake: i64) -> Self {
        let id = NEXT_WAKE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        WAKES.lock().insert(id, wake);
        Self(id)
    }
}

#[cfg(any(unix, windows))]
impl Drop for WakeEntry {
    fn drop(&mut self) {
        WAKES.lock().remove(&self.0);
    }
}

/// `__gos_fd_wait_raw(fd, writable, timeout_ms)` on the compiled tiers:
/// `Ok(1)` when ready, `Ok(0)` when the timeout passed or the caller's
/// cohort was cancelled, `Err` otherwise.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_fd_wait_raw(fd: i64, writable: i64, timeout_ms: i64) -> i128 {
    ffi_entry!({
        // A goroutine's wait runs on a pool thread, so it asks about the
        // caller's cohort by id rather than about the pool thread's. Any other
        // caller waits inline and asks its own tier.
        let cancelled: Box<dyn Fn() -> bool + Send> = if gossamer_coro::in_goroutine() {
            let cohort = super::cohort::current_cohort();
            Box::new(move || cohort != 0 && super::cohort::chain_is_cancelled(cohort))
        } else {
            Box::new(super::cohort::caller_is_cancelled)
        };
        match wait(fd, writable != 0, timeout_ms, cancelled) {
            Ok(ready) => super::result::gos_rt_result_new(0, i64::from(ready)),
            Err(message) => {
                let error = super::errors::error_new_from_bytes(message.as_bytes());
                super::result::gos_rt_result_new(1, error as i64)
            }
        }
    })
}

/// The instant `timeout_ms` from now, or `None` to wait forever.
#[cfg(any(unix, windows))]
fn deadline(timeout_ms: i64) -> Option<Instant> {
    u64::try_from(timeout_ms)
        .ok()
        .map(|ms| Instant::now() + Duration::from_millis(ms))
}

/// Milliseconds left until `deadline`, as the timeout argument the platform
/// wait takes: `None` waits forever.
#[cfg(any(unix, windows))]
fn remaining_ms(deadline: Option<Instant>) -> Option<u64> {
    deadline.map(|at| {
        let left = at.saturating_duration_since(Instant::now());
        u64::try_from(left.as_millis()).unwrap_or(u64::MAX)
    })
}

#[cfg(unix)]
mod platform {
    /// A non-blocking pipe whose read end a wait polls beside its descriptor.
    struct WakePipe {
        read: i32,
        write: i32,
    }

    impl WakePipe {
        fn new() -> Result<Self, String> {
            let mut ends = [0i32; 2];
            // SAFETY: `ends` has room for the two descriptors `pipe` writes.
            if unsafe { libc::pipe(ends.as_mut_ptr()) } != 0 {
                return Err(format!("fd::wait: {}", std::io::Error::last_os_error()));
            }
            for end in ends {
                // SAFETY: `end` is a descriptor `pipe` just opened.
                unsafe {
                    libc::fcntl(end, libc::F_SETFL, libc::O_NONBLOCK);
                    libc::fcntl(end, libc::F_SETFD, libc::FD_CLOEXEC);
                }
            }
            Ok(Self {
                read: ends[0],
                write: ends[1],
            })
        }

        fn drain(&self) {
            let mut buf = [0u8; 64];
            // SAFETY: `buf` is writable for its length; the read end is
            // non-blocking, so the loop ends when the pipe is empty.
            while unsafe { libc::read(self.read, buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
        }
    }

    impl Drop for WakePipe {
        fn drop(&mut self) {
            // SAFETY: both ends belong to this pipe and are closed once.
            unsafe {
                libc::close(self.read);
                libc::close(self.write);
            }
        }
    }

    pub(super) fn signal_wake(write: i64) {
        let Ok(write) = i32::try_from(write) else {
            return;
        };
        // SAFETY: `write` is the write end of a registered pipe, open while
        // its wait is registered; a full pipe already holds a wake.
        unsafe { libc::write(write, [1u8].as_ptr().cast(), 1) };
    }

    pub(super) fn wait(
        fd: i64,
        writable: bool,
        timeout_ms: i64,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<bool, String> {
        let deadline = super::deadline(timeout_ms);
        let fd = i32::try_from(fd).map_err(|_| format!("fd::wait: {fd} is not a descriptor"))?;
        let events = if writable {
            libc::POLLOUT
        } else {
            libc::POLLIN
        };
        let wake = WakePipe::new()?;
        // Registered before the first check, so a cancellation from here on
        // either is seen by that check or signals the pipe.
        let _entry = super::WakeEntry::register(i64::from(wake.write));
        loop {
            if cancelled() {
                return Ok(false);
            }
            let timeout = super::remaining_ms(deadline)
                .map_or(-1, |ms| i32::try_from(ms).unwrap_or(i32::MAX));
            let mut entries = [
                libc::pollfd {
                    fd,
                    events,
                    revents: 0,
                },
                libc::pollfd {
                    fd: wake.read,
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: `entries` is two valid `pollfd`s for the duration of the call.
            let n = unsafe { libc::poll(entries.as_mut_ptr(), 2, timeout) };
            if n < 0 {
                let error = std::io::Error::last_os_error();
                // A signal interrupted the wait; wait out what is left of it.
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(format!("fd::wait: {error}"));
            }
            if n == 0 {
                return Ok(false);
            }
            let revents = entries[0].revents;
            if revents & libc::POLLNVAL != 0 {
                return Err(format!("fd::wait: descriptor {fd} is not open"));
            }
            if revents != 0 {
                // Readiness, a hang-up, and an error all end the wait: the
                // next read or write reports which.
                return Ok(true);
            }
            wake.drain();
        }
    }
}

#[cfg(windows)]
mod platform {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        CreateEventW, INFINITE, ResetEvent, SetEvent, WaitForMultipleObjects,
    };

    /// A manual-reset event a wait watches beside its handle.
    struct WakeEvent(HANDLE);

    impl WakeEvent {
        fn new() -> Result<Self, String> {
            // SAFETY: no security attributes and no name; the event starts
            // unsignalled and resets only when this wait resets it.
            let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
            if event.is_null() {
                return Err(format!("fd::wait: {}", std::io::Error::last_os_error()));
            }
            Ok(Self(event))
        }
    }

    impl Drop for WakeEvent {
        fn drop(&mut self) {
            // SAFETY: the event belongs to this wait and is closed once.
            unsafe { CloseHandle(self.0) };
        }
    }

    pub(super) fn signal_wake(event: i64) {
        // SAFETY: `event` is a registered wait's event, open while its wait
        // is registered.
        unsafe { SetEvent(event as isize as HANDLE) };
    }

    pub(super) fn wait(
        handle: i64,
        writable: bool,
        timeout_ms: i64,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<bool, String> {
        // A console, pipe, or file handle accepts a write whenever it is open.
        if writable {
            return Ok(true);
        }
        let deadline = super::deadline(timeout_ms);
        let wake = WakeEvent::new()?;
        // Registered before the first check, so a cancellation from here on
        // either is seen by that check or signals the event.
        let _entry = super::WakeEntry::register(wake.0 as isize as i64);
        let handles = [handle as isize as HANDLE, wake.0];
        loop {
            if cancelled() {
                return Ok(false);
            }
            let timeout = super::remaining_ms(deadline)
                .map_or(INFINITE, |ms| u32::try_from(ms).unwrap_or(INFINITE - 1));
            // SAFETY: both handles are open for the call; an invalid program
            // handle fails the wait rather than touching memory.
            let status = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, timeout) };
            match status {
                WAIT_OBJECT_0 => return Ok(true),
                WAIT_TIMEOUT => return Ok(false),
                s if s == WAIT_OBJECT_0 + 1 => {
                    // SAFETY: the event belongs to this wait.
                    unsafe { ResetEvent(wake.0) };
                }
                _ => return Err(format!("fd::wait: {}", std::io::Error::last_os_error())),
            }
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    pub(super) fn wait(
        _fd: i64,
        _writable: bool,
        _timeout_ms: i64,
        _cancelled: &dyn Fn() -> bool,
    ) -> Result<bool, String> {
        Err("fd::wait: descriptors are not available on this target".to_string())
    }
}
