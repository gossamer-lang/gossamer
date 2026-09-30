//! C-ABI dispatch shims for `std::net::UdpSocket`. Mirrors the
//! bytecode-VM builtins in `gossamer-interp/src/stdlib_builtins/net.rs`
//! so the compiled (Cranelift / LLVM) tier resolves the same calls
//! natively instead of failing to link.
//!
//! Handle model identical to `c_abi/net_tcp.rs`: a process-global
//! `i64`-keyed registry of `Arc<UdpSocket>`; each op clones the `Arc`
//! under the lock and performs the (possibly blocking) `recv_from`
//! outside it. `UdpSocket::{send_to, recv_from, local_addr}` all take
//! `&self`, so no exclusive access is ever handed out. Cross-platform
//! via `std::net` (Linux / macOS / Windows); no libc / raw-fd surface.
//!
//! Result shapes (packed `i128` via `gos_rt_result_new`):
//! - `bind`        -> `Result<UdpSocket, Error>` (Ok payload = i64 handle)
//! - `send_to`     -> `Result<i64, Error>` (bytes sent)
//! - `recv_from`   -> `Result<([u8], String), Error>` (Ok payload = *Pair{bytes, addr})
//! - `local_addr`  -> `Result<String, Error>`
//! - `close`       -> () (Void)

#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::wildcard_imports)]

use std::collections::HashMap;
use std::net::UdpSocket;
use std::os::raw::c_char;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use parking_lot::Mutex;

static UDP_SOCKETS: Mutex<Option<HashMap<i64, Arc<UdpSocket>>>> = Mutex::new(None);
static NEXT_UDP_HANDLE: AtomicI64 = AtomicI64::new(1);

fn next_handle() -> i64 {
    NEXT_UDP_HANDLE.fetch_add(1, Ordering::Relaxed)
}

/// # Safety
///
/// `p` is null or a live string body.
unsafe fn cstr_to_str(p: *const c_char) -> String {
    // SAFETY: this function's contract is the one the reader states for `p`.
    unsafe { crate::c_abi::gos_str_arg_string(p) }
}

fn udp_err(msg: &str) -> i128 {
    let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
    super::result::gos_rt_result_new(1, err as i64)
}

/// The text a datagram failure reads as, matching the interp tier's
/// `IoError` rendering so one failure reads the same on every tier.
fn socket_error(err: &std::io::Error, context: &str) -> String {
    crate::c_abi::fs::classify_io_error(err, context)
}

fn socket_clone(h: i64) -> Option<Arc<UdpSocket>> {
    UDP_SOCKETS.lock().as_ref().and_then(|m| m.get(&h).cloned())
}

/// `net::UdpSocket::bind(addr) -> Result<UdpSocket, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_udp_bind(addr: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        // SAFETY: `addr` is this shim's argument, as `cstr_to_str` requires (C-ABI contract).
        let a = unsafe { cstr_to_str(addr) };
        match UdpSocket::bind(&a) {
            Ok(s) => {
                let h = next_handle();
                UDP_SOCKETS
                    .lock()
                    .get_or_insert_with(HashMap::new)
                    .insert(h, Arc::new(s));
                super::result::gos_rt_result_new(0, h)
            }
            Err(e) => udp_err(&socket_error(&e, &a)),
        }
    })
}

/// `net::UdpSocket::send_to(handle, data: [u8], addr) -> Result<i64, Error>`.
/// Returns the byte count sent. The MIR dispatch coerces a `String` /
/// byte-array-literal payload to the `Vec<u8>` ABI before the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_udp_send_to(
    h: i64,
    data: *const super::vec::GosVec,
    addr: *const c_char,
) -> i128 {
    ffi_entry!(0i128, {
        let Some(sock) = socket_clone(h) else {
            return udp_err("UdpSocket::send_to: stale handle");
        };
        // SAFETY: `data` is this shim's argument, live for the call (C-ABI contract) or null,
        // which `vec_bytes` accepts.
        let bytes = unsafe { crate::c_abi::vec::vec_bytes(data) };
        // SAFETY: `addr` is this shim's argument, as `cstr_to_str` requires (C-ABI contract).
        let target = unsafe { cstr_to_str(addr) };
        match sock.send_to(&bytes, &target) {
            Ok(n) => super::result::gos_rt_result_new(0, n as i64),
            Err(e) => udp_err(&socket_error(&e, &target)),
        }
    })
}

/// `net::UdpSocket::recv_from(handle, max) -> Result<([u8], String), Error>`.
/// Ok payload is a heap `#[repr(C)] Pair { bytes: i64, addr: i64 }` -
/// the 2-slot tuple `([u8]-vec, sender-address-string)`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_udp_recv_from(h: i64, max: i64) -> i128 {
    ffi_entry!(0i128, {
        let Some(sock) = socket_clone(h) else {
            return udp_err("UdpSocket::recv_from: stale handle");
        };
        let cap = max.clamp(1, 1 << 16) as usize;
        match crate::sched_global::run_blocking("udp-recv-from", move || {
            let mut buf = vec![0u8; cap];
            sock.recv_from(&mut buf).map(|(n, from)| {
                buf.truncate(n);
                (buf, from)
            })
        }) {
            Ok(Ok((buf, from))) => {
                let bytes_vec = super::encoding::bytes_to_gosvec(&buf);
                let addr_cs = super::string::alloc_cstring(from.to_string().as_bytes());
                #[repr(C)]
                struct Pair {
                    bytes: i64,
                    addr: i64,
                }
                let pair = Box::into_raw(Box::new(Pair {
                    bytes: bytes_vec as i64,
                    addr: addr_cs as i64,
                }));
                super::result::gos_rt_result_new(0, pair as i64)
            }
            Ok(Err(e)) => udp_err(&socket_error(&e, "UdpSocket::recv_from")),
            Err(e) => udp_err(&e),
        }
    })
}

/// `net::UdpSocket::local_addr(handle) -> Result<String, Error>`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_udp_local_addr(h: i64) -> i128 {
    ffi_entry!(0i128, {
        let Some(sock) = socket_clone(h) else {
            return udp_err("UdpSocket::local_addr: stale handle");
        };
        match sock.local_addr() {
            Ok(a) => super::result::gos_rt_result_new(
                0,
                super::string::alloc_cstring(a.to_string().as_bytes()) as i64,
            ),
            Err(e) => udp_err(&socket_error(&e, "local_addr")),
        }
    })
}

/// `net::UdpSocket::close(handle)`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_udp_close(h: i64) {
    ffi_entry!((), {
        if let Some(m) = UDP_SOCKETS.lock().as_mut() {
            m.remove(&h);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[cfg(not(tsan))]
    fn scheduler_wait_timeout() -> Duration {
        Duration::from_secs(2)
    }

    #[cfg(tsan)]
    fn scheduler_wait_timeout() -> Duration {
        // TSan instruments the scheduler, blocking-worker handoff, and UDP
        // receive path. Keep the ordinary regression quick while allowing
        // the sanitizer build enough time to observe the real wake-up.
        Duration::from_secs(20)
    }

    #[test]
    #[cfg_attr(miri, ignore)] // Miri does not support UDP datagram sockets.
    fn receive_in_a_goroutine_resumes_after_a_datagram_arrives() {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind receiver");
        let addr = socket.local_addr().expect("receiver address");
        let handle = next_handle();
        UDP_SOCKETS
            .lock()
            .get_or_insert_with(HashMap::new)
            .insert(handle, Arc::new(socket));

        let (tx, rx) = mpsc::sync_channel(1);
        crate::sched_global::spawn(Box::new(move || {
            let result = gos_rt_udp_recv_from(handle, 64);
            let is_ok = result & i128::from(u64::MAX) == 0;
            tx.send(is_ok).expect("report receive result");
        }));

        let sender = std::thread::spawn(move || {
            crate::platform::sleep(Duration::from_millis(20));
            UdpSocket::bind("127.0.0.1:0")
                .expect("bind sender")
                .send_to(b"wake", addr)
                .expect("send datagram");
        });

        assert!(
            rx.recv_timeout(scheduler_wait_timeout())
                .expect("scheduled receive should resume"),
            "UDP receive must return an Ok runtime Result"
        );
        sender.join().expect("sender thread");
        gos_rt_udp_close(handle);
    }
}
