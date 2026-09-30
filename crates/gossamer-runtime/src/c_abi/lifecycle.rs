#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]

//! Process lifecycle: readiness, graceful shutdown, and the systemd
//! `sd_notify` protocol.
//!
//! The state is process-global because a process has one lifecycle, and
//! it is the same state on every tier: the bytecode VM's builtins and the
//! compiled tiers' C-ABI shims both read and write it here, so a program
//! that flips readiness and waits for a signal behaves identically under
//! `gos run` and a native build.
//!
//! Shutdown is observed rather than dispatched. A program waits for it and
//! then drains with ordinary code:
//!
//! ```text
//! spawn(|| serve())
//! lifecycle::ready()
//! lifecycle::await_shutdown()
//! pool.close()
//! ```
//!
//! This is Go-shaped and needs no callback registry: the drain sequence is
//! the statements after the wait, in the order they are written, with
//! `defer` available for the rest.

use std::os::raw::c_char;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Whether the process has declared itself ready to serve traffic. False
/// until `lifecycle::ready()`, and false again once shutdown begins, so a
/// readiness probe fails ahead of the drain and the load balancer stops
/// sending new work.
static READY: AtomicBool = AtomicBool::new(false);

/// How long a waiter sleeps between shutdown checks. The wait ends on the
/// flag, not on the tick, so this only bounds how stale the observation
/// can be.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Writes `payload` to `$NOTIFY_SOCKET` when the process runs under a
/// service manager that set one. No-op otherwise and on every platform
/// without unix datagram sockets.
#[cfg(unix)]
fn sd_notify(payload: &str) {
    let Ok(socket_path) = std::env::var("NOTIFY_SOCKET") else {
        return;
    };
    if socket_path.is_empty() {
        return;
    }
    // A leading `@` selects the abstract namespace, which `UnixDatagram`
    // addresses with a leading NUL.
    let path = if let Some(rest) = socket_path.strip_prefix('@') {
        format!("\0{rest}")
    } else {
        socket_path
    };
    if let Ok(sock) = std::os::unix::net::UnixDatagram::unbound() {
        let _ = sock.send_to(payload.as_bytes(), path);
    }
}

#[cfg(not(unix))]
fn sd_notify(_payload: &str) {}

/// Declares the process ready and tells the service manager.
pub fn set_ready(ready: bool) {
    READY.store(ready, Ordering::Release);
    if ready {
        sd_notify("READY=1\n");
    }
}

/// Whether the process has declared itself ready.
#[must_use]
pub fn is_ready() -> bool {
    READY.load(Ordering::Acquire) && !is_shutting_down()
}

/// Whether the shutdown sequence has begun.
#[must_use]
pub fn is_shutting_down() -> bool {
    crate::sched_global::is_shutdown_requested()
}

/// Begins shutdown: readiness drops, the service manager is told, and
/// every server accept loop stops taking new connections while in-flight
/// requests finish.
pub fn begin_shutdown() {
    if is_shutting_down() {
        return;
    }
    READY.store(false, Ordering::Release);
    sd_notify("STOPPING=1\n");
    crate::sched_global::request_shutdown();
    wake_shutdown_watchers();
    // An acceptor parked in `accept()` tests its shutdown flag only when a
    // connection arrives, so the flag has to be delivered as one. wasm32
    // has no listening server to reach.
    #[cfg(not(target_arch = "wasm32"))]
    crate::c_abi::http_server::wake_http_acceptors();
    crate::c_abi::context::cancel_live_requests();
}

/// Additional shutdown flags an embedding wants flipped alongside the
/// scheduler's. The bytecode VM's HTTP servers own one each, so a
/// `lifecycle::shutdown()` from Gossamer code stops them too.
type ShutdownFlags = parking_lot::Mutex<Vec<std::sync::Arc<AtomicBool>>>;

fn shutdown_flags() -> &'static ShutdownFlags {
    static FLAGS: std::sync::OnceLock<ShutdownFlags> = std::sync::OnceLock::new();
    FLAGS.get_or_init(|| parking_lot::Mutex::new(Vec::new()))
}

/// Registers `flag` to be set when shutdown begins. Already-set shutdown
/// flips it immediately, so a server started during the drain does not
/// come up serving.
pub fn register_shutdown_flag(flag: &std::sync::Arc<AtomicBool>) {
    if is_shutting_down() {
        flag.store(true, Ordering::Release);
        return;
    }
    shutdown_flags().lock().push(std::sync::Arc::clone(flag));
}

/// Sets every registered flag.
fn wake_shutdown_watchers() {
    for flag in shutdown_flags().lock().iter() {
        flag.store(true, Ordering::Release);
    }
}

/// Blocks until shutdown begins.
pub fn await_shutdown() {
    while !is_shutting_down() {
        crate::platform::sleep(POLL_INTERVAL);
    }
    // A registered flag may have been added after `begin_shutdown` ran.
    wake_shutdown_watchers();
}

/// Reports a free-text status to the service manager.
pub fn notify_status(message: &str) {
    sd_notify(&format!("STATUS={message}\n"));
}

/// `lifecycle::ready()` - declare the process ready to serve.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_lifecycle_ready() {
    ffi_entry!((), { set_ready(true) });
}

/// `lifecycle::set_ready(ready)` - set readiness explicitly.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_lifecycle_set_ready(ready: i64) {
    ffi_entry!((), { set_ready(ready != 0) });
}

/// `lifecycle::is_ready() -> bool`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_lifecycle_is_ready() -> i64 {
    ffi_entry!(0, { i64::from(is_ready()) })
}

/// `lifecycle::shutdown()` - begin the shutdown sequence.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_lifecycle_shutdown() {
    ffi_entry!((), { begin_shutdown() });
}

/// `lifecycle::is_shutting_down() -> bool`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_lifecycle_is_shutting_down() -> i64 {
    ffi_entry!(0, { i64::from(is_shutting_down()) })
}

/// `lifecycle::await_shutdown()` - block until shutdown begins.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_lifecycle_await_shutdown() {
    ffi_entry!((), { await_shutdown() });
}

/// `lifecycle::notify_status(message)` - report status to the service
/// manager.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_lifecycle_notify_status(message: *const c_char) {
    ffi_entry!((), {
        let text = if message.is_null() {
            String::new()
        } else {
            // SAFETY: `message` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_string(message) }
        };
        notify_status(&text);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_is_false_before_ready_is_declared() {
        // The process starts not-ready: a readiness probe must fail until
        // the program says otherwise.
        assert!(!READY.load(Ordering::Acquire));
    }

    #[test]
    fn a_registered_flag_is_set_when_shutdown_begins() {
        let flag = std::sync::Arc::new(AtomicBool::new(false));
        register_shutdown_flag(&flag);
        wake_shutdown_watchers();
        assert!(flag.load(Ordering::Acquire));
    }
}
