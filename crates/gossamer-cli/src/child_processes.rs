//! Lifetime of the child processes a `gos` command starts.
//!
//! A test worker runs in its own process group so a per-test timeout can end
//! the whole tree it built rather than the one process the toolchain holds a
//! handle to. That group is also what the terminal's Ctrl-C never reaches:
//! the shell signals the foreground group, and a worker in a group of its own
//! is not in it. The toolchain therefore owns the lifetime itself - every
//! child it starts is registered here, an interrupt kills each registered
//! group before the command exits, and a registration is a guard, so a child
//! is unregistered on every path out of the frame that started it.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use gossamer_std::signal::{self, sigs};

/// Process ids of the children this command started, each the leader of its
/// own process group.
static LIVE: Mutex<Vec<u32>> = Mutex::new(Vec::new());
/// Whether the interrupt watcher is already running.
static WATCHING: AtomicBool = AtomicBool::new(false);

/// Registers `pid` for the lifetime of the returned guard.
///
/// The first registration starts the interrupt watcher, so a command that
/// never spawns a child pays nothing.
pub(crate) fn register(pid: u32) -> ChildGuard {
    watch_for_interrupt();
    if let Ok(mut live) = LIVE.lock() {
        live.push(pid);
    }
    ChildGuard { pid }
}

/// Unregisters a child when its frame ends.
pub(crate) struct ChildGuard {
    pid: u32,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Ok(mut live) = LIVE.lock() {
            live.retain(|pid| *pid != self.pid);
        }
    }
}

/// Registers `pid` without a guard, for a child whose handle outlives the
/// frame that started it. Its owner calls [`forget`] once it has been reaped.
pub(crate) fn track(pid: u32) {
    watch_for_interrupt();
    if let Ok(mut live) = LIVE.lock() {
        live.push(pid);
    }
}

/// Drops a [`track`]ed registration.
pub(crate) fn forget(pid: u32) {
    if let Ok(mut live) = LIVE.lock() {
        live.retain(|entry| *entry != pid);
    }
}

/// Ends every registered child's process group.
///
/// Safe to call more than once: a group that has already exited is reported
/// as absent and skipped.
pub(crate) fn terminate_all() {
    let pids = LIVE.lock().map(|live| live.clone()).unwrap_or_default();
    for pid in pids {
        terminate_group(pid);
    }
    // A command that ran a program in-process may also hold children that
    // program started; they belong to the same interrupt.
    gossamer_std::process::terminate_live_children();
}

/// Ends the process group `pid` leads.
///
/// The child was started with `process_group(0)` (Unix) or
/// `CREATE_NEW_PROCESS_GROUP` (Windows), so its pid is its group id and one
/// signal reaches everything it started.
pub(crate) fn terminate_group(pid: u32) {
    #[cfg(unix)]
    {
        use gossamer_std::process::{Signal, signal_process_group};
        let _ = signal_process_group(pid, Signal::Term);
        // SIGKILL after a short grace so a worker that ignores SIGTERM - or
        // one wedged where no handler runs - still goes away.
        std::thread::sleep(std::time::Duration::from_millis(100));
        let _ = signal_process_group(pid, Signal::Kill);
    }
    #[cfg(windows)]
    {
        // Windows has no group signal a non-console process can rely on, so
        // the leader is ended through `taskkill /T`, which walks the tree it
        // started.
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
    }
}

/// Starts the watcher that ends every registered child when this process is
/// interrupted, then exits with the conventional interrupted status.
fn watch_for_interrupt() {
    if WATCHING.swap(true, Ordering::AcqRel) {
        return;
    }
    let interrupt = signal::on(sigs::SIGINT);
    let terminate = signal::on(sigs::SIGTERM);
    std::thread::spawn(move || {
        loop {
            if interrupt.try_wait() || terminate.try_wait() {
                terminate_all();
                // 130 is the shell's convention for a process ended by
                // SIGINT, and what a caller scripting `gos test` reads.
                std::process::exit(130);
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    });
}
