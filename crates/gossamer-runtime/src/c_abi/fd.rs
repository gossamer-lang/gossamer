//! `std::os::fd`: waiting for a descriptor (on Windows, a handle) to become
//! readable or writable without holding a scheduler worker.
//!
//! On Unix a goroutine's wait registers the descriptor with its worker's
//! netpoller and parks there, so the worker that sleeps in the kernel's event
//! queue is the thread that resumes the goroutine: readiness reaches it with
//! one wakeup, as it reaches a thread blocked in `poll`.
//!
//! A descriptor the event queue refuses (a regular file under epoll, a tty
//! under kqueue on macOS), and any wait outside a goroutine or on Windows,
//! runs `poll(2)` (on Windows, `WaitForMultipleObjects`) on the blocking pool
//! instead, while the goroutine is parked. Each such wait also watches a wake
//! object of its own (a pipe, or an event on Windows).
//!
//! Cohort cancellation reaches every wait through [`wake_waits`], so a wait
//! under a cancelled cohort ends instead of holding its pool thread or its
//! park.

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
    #[cfg(unix)]
    if crate::sched_global::current_gid().is_some()
        && let Some(answer) = poller::wait(fd, writable, timeout_ms, &cancelled)
    {
        return answer;
    }
    crate::sched_global::run_blocking("fd::wait", move || {
        platform::wait(fd, writable, timeout_ms, &cancelled)
    })?
}

/// What [`wake_waits`] signals for one wait in progress.
#[cfg(any(unix, windows))]
enum WakeTarget {
    /// A pool wait's wake object: a pipe's write end on Unix, an event
    /// handle on Windows.
    Os(i64),
    /// A goroutine parked in its worker's netpoller.
    #[cfg(unix)]
    Poller(crate::netpoll::Interrupter),
}

/// The descriptor waits in progress, by wait id.
#[cfg(any(unix, windows))]
static WAKES: std::sync::LazyLock<parking_lot::Mutex<std::collections::HashMap<u64, WakeTarget>>> =
    std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

#[cfg(any(unix, windows))]
static NEXT_WAKE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Signals every descriptor wait in progress, so each re-checks whether the
/// cohort it waits under was cancelled.
pub fn wake_waits() {
    #[cfg(any(unix, windows))]
    for wake in WAKES.lock().values() {
        match wake {
            WakeTarget::Os(handle) => platform::signal_wake(*handle),
            #[cfg(unix)]
            WakeTarget::Poller(interrupter) => interrupter.interrupt(),
        }
    }
}

/// A wait's registration in [`WAKES`], removed when the wait ends.
#[cfg(any(unix, windows))]
struct WakeEntry(u64);

#[cfg(any(unix, windows))]
impl WakeEntry {
    fn register(wake: WakeTarget) -> Self {
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

/// Waits that park in the calling worker's netpoller.
#[cfg(unix)]
mod poller {
    use crate::netpoll::{Direction, Registration, Wake};

    /// Waits as [`super::wait`] does, parked in the worker's netpoller.
    /// `None` when the descriptor is one the poller does not take, for the
    /// blocking pool to wait on instead.
    pub(super) fn wait(
        fd: i64,
        writable: bool,
        timeout_ms: i64,
        cancelled: &dyn Fn() -> bool,
    ) -> Option<Result<bool, String>> {
        let deadline = super::deadline(timeout_ms);
        let Ok(raw) = i32::try_from(fd) else {
            return Some(Err(format!("fd::wait: {fd} is not a descriptor")));
        };
        if cancelled() {
            return Some(Ok(false));
        }
        match ready_now(raw, writable) {
            Err(error) => return Some(Err(error)),
            Ok(true) => return Some(Ok(true)),
            Ok(false) if timeout_ms == 0 => return Some(Ok(false)),
            Ok(false) => {}
        }
        if !queue_takes(raw) {
            return None;
        }
        let mut source = mio::unix::SourceFd(&raw);
        let registration = Registration::new(&mut source).ok()?;
        let direction = if writable {
            Direction::Write
        } else {
            Direction::Read
        };
        let answer = {
            // Registered before the first check, so a cancellation from
            // here on either is seen by that check or interrupts the park.
            let _entry = super::WakeEntry::register(super::WakeTarget::Poller(
                registration.interrupter(direction),
            ));
            loop {
                let seen = registration.interrupts();
                if cancelled() {
                    break Ok(false);
                }
                match registration.park_until_ready(direction, deadline, seen) {
                    Wake::TimedOut => break Ok(false),
                    Wake::Interrupted => {}
                    Wake::Ready => match ready_now(raw, writable) {
                        Ok(false) => {}
                        answer => break answer,
                    },
                }
            }
        };
        registration.deregister(&mut source);
        Some(answer)
    }

    /// Whether `fd` is ready now, without waiting.
    fn ready_now(fd: i32, writable: bool) -> Result<bool, String> {
        let mut entry = libc::pollfd {
            fd,
            events: if writable {
                libc::POLLOUT
            } else {
                libc::POLLIN
            },
            revents: 0,
        };
        loop {
            // SAFETY: `entry` is one valid `pollfd` for the duration of the call.
            let n = unsafe { libc::poll(&raw mut entry, 1, 0) };
            if n >= 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(format!("fd::wait: {error}"));
            }
        }
        if entry.revents & libc::POLLNVAL != 0 {
            return Err(format!("fd::wait: descriptor {fd} is not open"));
        }
        // Readiness, a hang-up, and an error all end the wait: the next
        // read or write reports which.
        Ok(entry.revents != 0)
    }

    /// Whether the kernel event queue reports readiness for `fd` the way
    /// `poll` does. A regular file or a directory is always ready to `poll`
    /// and is answered before this is asked; kqueue on macOS refuses tty
    /// devices.
    fn queue_takes(fd: i32) -> bool {
        // SAFETY: `fstat` writes one `stat` through the pointer and touches
        // nothing else; a zeroed `stat` is a valid value of the type.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: as above; `stat` is live for the call.
        if unsafe { libc::fstat(fd, &raw mut stat) } != 0 {
            return false;
        }
        let kind = stat.st_mode & libc::S_IFMT;
        if kind == libc::S_IFREG || kind == libc::S_IFDIR {
            return false;
        }
        !(cfg!(target_vendor = "apple") && kind == libc::S_IFCHR)
    }
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
        let _entry = super::WakeEntry::register(super::WakeTarget::Os(i64::from(wake.write)));
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
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
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

    /// The handle `raw` names: a standard stream's own for 0, 1, and 2, which
    /// no kernel handle equals (handles are multiples of four, and 0 is
    /// null), and `raw` itself otherwise.
    fn std_stream_handle(raw: i64) -> HANDLE {
        let which = match raw {
            0 => STD_INPUT_HANDLE,
            1 => STD_OUTPUT_HANDLE,
            2 => STD_ERROR_HANDLE,
            _ => return raw as isize as HANDLE,
        };
        // SAFETY: GetStdHandle reads the process's standard handle table.
        unsafe { GetStdHandle(which) }
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
        let _entry = super::WakeEntry::register(super::WakeTarget::Os(wake.0 as isize as i64));
        let handles = [std_stream_handle(handle), wake.0];
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

#[cfg(all(test, unix))]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use super::wait;

    const POLLER_ONLY_CHILD_ENV: &str = "GOSSAMER_FD_POLLER_ONLY_CHILD";

    /// A pipe's read and write ends.
    fn pipe() -> (i32, i32) {
        let mut ends = [0i32; 2];
        // SAFETY: `ends` has room for the two descriptors `pipe` writes.
        assert_eq!(unsafe { libc::pipe(ends.as_mut_ptr()) }, 0, "pipe");
        (ends[0], ends[1])
    }

    fn close(fd: i32) {
        // SAFETY: `fd` is a descriptor this test opened and closes once.
        unsafe { libc::close(fd) };
    }

    /// Writes one byte to `fd` from another thread after `delay`.
    fn write_later(fd: i32, delay: Duration) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            // SAFETY: the buffer is one readable byte.
            unsafe { libc::write(fd, b"x".as_ptr().cast(), 1) };
        })
    }

    /// Runs `body` as a goroutine on the scheduler and answers its result.
    fn in_goroutine<T: Send + 'static>(body: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        let _ = crate::sched_global::spawn(Box::new(move || {
            let _ = tx.send(body());
        }));
        rx.recv().expect("the goroutine answers")
    }

    #[test]
    #[cfg_attr(miri, ignore)] // parks goroutines in the kernel event queue
    fn a_goroutine_resumes_when_its_pipe_turns_readable() {
        let (read, write) = pipe();
        let writer = write_later(write, Duration::from_millis(20));
        let answer = in_goroutine(move || wait(i64::from(read), false, -1, || false));
        writer.join().expect("writer");
        assert_eq!(answer, Ok(true));
        close(read);
        close(write);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // parks goroutines in the kernel event queue
    fn waits_that_end_before_their_deadline_leave_no_timers_behind() {
        use crate::netpoll::{Direction, Registration, Wake};
        let (read, write) = pipe();
        let pending = in_goroutine(move || {
            let mut source = mio::unix::SourceFd(&read);
            let registration = Registration::new(&mut source).expect("register the pipe");
            for _ in 0..400 {
                // SAFETY: the buffer is one readable byte.
                unsafe { libc::write(write, b"x".as_ptr().cast(), 1) };
                let deadline = Instant::now() + Duration::from_mins(10);
                while registration.park_until_ready(
                    Direction::Read,
                    Some(deadline),
                    registration.interrupts(),
                ) != Wake::Ready
                {}
                let mut byte = 0u8;
                // SAFETY: `byte` has room for the one byte read.
                unsafe { libc::read(read, (&raw mut byte).cast(), 1) };
            }
            let pending = registration.pending_timers();
            registration.deregister(&mut source);
            pending
        });
        assert!(pending < 8, "{pending} timers outlived their waits");
        close(read);
        close(write);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // parks goroutines in the kernel event queue
    fn a_goroutine_wait_ends_at_its_timeout() {
        let (read, write) = pipe();
        let start = Instant::now();
        let answer = in_goroutine(move || wait(i64::from(read), false, 30, || false));
        assert_eq!(answer, Ok(false));
        assert!(
            start.elapsed() >= Duration::from_millis(25),
            "woke before its timeout"
        );
        close(read);
        close(write);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // parks goroutines in the kernel event queue
    fn a_goroutine_wait_ends_when_its_cancellation_is_signalled() {
        let (read, write) = pipe();
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancelled);
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            flag.store(true, Ordering::SeqCst);
            super::wake_waits();
        });
        let answer = in_goroutine(move || {
            wait(i64::from(read), false, -1, move || {
                cancelled.load(Ordering::SeqCst)
            })
        });
        canceller.join().expect("canceller");
        assert_eq!(answer, Ok(false));
        close(read);
        close(write);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // parks goroutines in the kernel event queue
    fn a_goroutine_wait_on_a_descriptor_that_is_not_open_is_an_error() {
        // Above every kernel's ceiling on descriptor numbers, so never open,
        // whatever the tests running beside this one open.
        let never_open: i64 = 1 << 30;
        let answer = in_goroutine(move || wait(never_open, false, 1000, || false));
        assert!(answer.is_err(), "answered {answer:?}");
    }

    #[test]
    #[cfg_attr(miri, ignore)] // starts a one-worker subprocess with mmap-stack coroutines
    fn goroutine_descriptor_waits_hold_no_blocking_thread() {
        let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .arg("--exact")
            .arg("c_abi::fd::tests::poller_only_child")
            .arg("--nocapture")
            .env(POLLER_ONLY_CHILD_ENV, "1")
            .env("GOSSAMER_MAX_PROCS", "1")
            .status()
            .expect("start one-worker child");
        assert!(status.success(), "one-worker child failed: {status}");
    }

    /// In a process of its own: goroutines wait on a pipe on the one worker
    /// while another goroutine writes it, and the waits resume with no
    /// thread of the blocking pool ever started.
    #[test]
    #[cfg_attr(miri, ignore)] // parks goroutines in the kernel event queue
    fn poller_only_child() {
        if std::env::var_os(POLLER_ONLY_CHILD_ENV).is_none() {
            return;
        }
        let (read, write) = pipe();
        for _ in 0..20 {
            let _ = crate::sched_global::spawn(Box::new(move || {
                crate::sched_global::sleep_until(Instant::now() + Duration::from_millis(2));
                // SAFETY: the buffer is one readable byte.
                unsafe { libc::write(write, b"x".as_ptr().cast(), 1) };
            }));
            let answer = in_goroutine(move || {
                let ready = wait(i64::from(read), false, -1, || false);
                let mut byte = [0u8];
                // SAFETY: `byte` is one writable byte.
                unsafe { libc::read(read, byte.as_mut_ptr().cast(), 1) };
                ready
            });
            assert_eq!(answer, Ok(true));
        }
        assert_eq!(
            crate::blocking_pool::blocking_threads(),
            0,
            "a goroutine's descriptor wait went through the blocking pool"
        );
        close(read);
        close(write);
    }
}
