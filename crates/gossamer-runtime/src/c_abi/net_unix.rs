//! C-ABI dispatch shims for `std::net::UnixListener` / `UnixStream`
//! (AF_UNIX stream sockets). Mirrors the `net_tcp.rs` handle model and
//! the bytecode-VM builtins so the compiled (Cranelift / LLVM) tier
//! resolves the same calls natively.
//!
//! Platform: Unix-domain sockets are a POSIX feature. The real
//! implementation is `#[cfg(unix)]` over `std::os::unix::net`; on
//! non-unix targets every entry point is a stub that returns an
//! `Err(errors::Error)` (or a no-op `close`), so a program that does not
//! exercise Unix sockets still links and runs on Windows.
//!
//! Result shapes match the TCP shims (`gos_rt_result_new`):
//! - `bind` / `connect`  -> `Result<UnixListener|UnixStream, Error>` (Ok = i64 handle)
//! - `accept`            -> `Result<(UnixStream, String), Error>` (Ok = *Pair{stream, addr})
//! - `read`              -> `Result<[u8], Error>`
//! - `read_to_string`    -> `Result<String, Error>`
//! - `write`             -> `Result<i64, Error>` (bytes written)
//! - `close`             -> ()

#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::wildcard_imports)]

use std::os::raw::c_char;

#[cfg(unix)]
fn cstr_to_str(p: *const c_char) -> String {
    unsafe { crate::c_abi::gos_str_arg_string(p) }
}

/// Packs `Err(errors::Error)` as the runtime's `i128` Result.
fn unix_err(msg: &str) -> i128 {
    let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
    super::vec::gos_rt_result_new(1, err as i64)
}

#[cfg(unix)]
mod imp {
    use super::cstr_to_str;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::os::raw::c_char;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::{Arc, LazyLock};

    use parking_lot::Mutex;

    static UNIX_LISTENERS: Mutex<Option<HashMap<i64, Arc<UnixListener>>>> = Mutex::new(None);
    static UNIX_STREAMS: Mutex<Option<HashMap<i64, Arc<UnixStream>>>> = Mutex::new(None);
    static NEXT_UNIX_HANDLE: AtomicI64 = AtomicI64::new(1);
    // Keep the handle namespace disjoint from TCP's so a stray cross-type
    // call surfaces as a stale handle rather than touching a live socket.
    static UNIX_BASE: LazyLock<i64> = LazyLock::new(|| 1 << 40);

    /// Runs `transfer`, which may block, where it cannot hold a scheduler
    /// worker: a goroutine is pinned to the worker it started on, so one held
    /// in a system call would strand every goroutine placed behind it. Inline
    /// off the scheduler, on the blocking pool from a goroutine.
    fn off_worker<T: Send + 'static>(
        label: &'static str,
        transfer: impl FnOnce() -> std::io::Result<T> + Send + 'static,
    ) -> std::io::Result<T> {
        crate::sched_global::run_blocking(label, transfer).map_err(std::io::Error::other)?
    }

    fn next_handle() -> i64 {
        *UNIX_BASE + NEXT_UNIX_HANDLE.fetch_add(1, Ordering::Relaxed)
    }

    fn listener_clone(h: i64) -> Option<Arc<UnixListener>> {
        UNIX_LISTENERS
            .lock()
            .as_ref()
            .and_then(|m| m.get(&h).cloned())
    }

    fn stream_clone(h: i64) -> Option<Arc<UnixStream>> {
        UNIX_STREAMS
            .lock()
            .as_ref()
            .and_then(|m| m.get(&h).cloned())
    }

    fn insert_stream(s: UnixStream) -> i64 {
        let h = next_handle();
        UNIX_STREAMS
            .lock()
            .get_or_insert_with(HashMap::new)
            .insert(h, Arc::new(s));
        h
    }

    pub(super) unsafe fn listener_bind(path: *const c_char) -> i128 {
        let p = cstr_to_str(path);
        match UnixListener::bind(&p) {
            Ok(l) => {
                let h = next_handle();
                UNIX_LISTENERS
                    .lock()
                    .get_or_insert_with(HashMap::new)
                    .insert(h, Arc::new(l));
                super::super::vec::gos_rt_result_new(0, h)
            }
            Err(e) => super::unix_err(&format!("{e}")),
        }
    }

    pub(super) unsafe fn listener_accept(h: i64) -> i128 {
        let Some(listener) = listener_clone(h) else {
            return super::unix_err("UnixListener::accept: stale handle");
        };
        match off_worker("unix-accept", move || listener.accept()) {
            Ok((stream, addr)) => {
                let sh = insert_stream(stream);
                let addr_str = addr
                    .as_pathname()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let addr_cs = super::super::string::alloc_cstring(addr_str.as_bytes());
                #[repr(C)]
                struct Pair {
                    stream: i64,
                    addr: i64,
                }
                let pair = Box::into_raw(Box::new(Pair {
                    stream: sh,
                    addr: addr_cs as i64,
                }));
                super::super::vec::gos_rt_result_new(0, pair as i64)
            }
            Err(e) => super::unix_err(&format!("{e}")),
        }
    }

    pub(super) unsafe fn listener_close(h: i64) {
        if let Some(m) = UNIX_LISTENERS.lock().as_mut() {
            m.remove(&h);
        }
    }

    pub(super) unsafe fn stream_connect(path: *const c_char) -> i128 {
        let p = cstr_to_str(path);
        match UnixStream::connect(&p) {
            Ok(s) => super::super::vec::gos_rt_result_new(0, insert_stream(s)),
            Err(e) => super::unix_err(&format!("{e}")),
        }
    }

    pub(super) unsafe fn stream_read(h: i64, max: i64) -> i128 {
        let cap = max.clamp(1, 1 << 24) as usize;
        let mut buf = vec![0u8; cap];
        let Some(stream) = stream_clone(h) else {
            return super::unix_err("UnixStream::read: stale handle");
        };
        let read = off_worker("unix-stream-read", move || {
            (&*stream).read(&mut buf).map(|n| {
                buf.truncate(n);
                buf
            })
        });
        match read {
            Ok(buf) => super::super::vec::gos_rt_result_new(
                0,
                super::super::encoding::bytes_to_gosvec(&buf) as i64,
            ),
            Err(e) => super::unix_err(&format!("{e}")),
        }
    }

    pub(super) unsafe fn stream_read_to_string(h: i64) -> i128 {
        let Some(stream) = stream_clone(h) else {
            return super::unix_err("UnixStream::read_to_string: stale handle");
        };
        let read = off_worker("unix-stream-read-string", move || {
            let mut out = Vec::new();
            (&*stream).read_to_end(&mut out).map(|_| out)
        });
        let out = match read {
            Ok(out) => out,
            Err(e) => return super::unix_err(&format!("{e}")),
        };
        let s = String::from_utf8_lossy(&out);
        super::super::vec::gos_rt_result_new(
            0,
            super::super::string::alloc_cstring(s.as_bytes()) as i64,
        )
    }

    pub(super) unsafe fn stream_write(h: i64, data: *const super::super::vec::GosVec) -> i128 {
        let bytes = unsafe { super::super::encoding::gosvec_u8(data) };
        let Some(stream) = stream_clone(h) else {
            return super::unix_err("UnixStream::write: stale handle");
        };
        let len = bytes.len() as i64;
        match off_worker("unix-stream-write", move || (&*stream).write_all(&bytes)) {
            Ok(()) => super::super::vec::gos_rt_result_new(0, len),
            Err(e) => super::unix_err(&format!("{e}")),
        }
    }

    pub(super) unsafe fn stream_close(h: i64) {
        if let Some(m) = UNIX_STREAMS.lock().as_mut() {
            m.remove(&h);
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use std::os::raw::c_char;

    const UNSUPPORTED_BIND: &str =
        "net::UnixListener::bind: Unix-domain sockets are not supported on this platform";
    const UNSUPPORTED_ACCEPT: &str =
        "net::UnixListener::accept: Unix-domain sockets are not supported on this platform";
    const UNSUPPORTED_CONNECT: &str =
        "net::UnixStream::connect: Unix-domain sockets are not supported on this platform";
    const UNSUPPORTED_READ: &str =
        "net::UnixStream::read: Unix-domain sockets are not supported on this platform";
    const UNSUPPORTED_READ_TO_STRING: &str =
        "net::UnixStream::read_to_string: Unix-domain sockets are not supported on this platform";
    const UNSUPPORTED_WRITE: &str =
        "net::UnixStream::write: Unix-domain sockets are not supported on this platform";

    pub(super) unsafe fn listener_bind(_path: *const c_char) -> i128 {
        super::unix_err(UNSUPPORTED_BIND)
    }
    pub(super) unsafe fn listener_accept(_h: i64) -> i128 {
        super::unix_err(UNSUPPORTED_ACCEPT)
    }
    pub(super) unsafe fn listener_close(_h: i64) {}
    pub(super) unsafe fn stream_connect(_path: *const c_char) -> i128 {
        super::unix_err(UNSUPPORTED_CONNECT)
    }
    pub(super) unsafe fn stream_read(_h: i64, _max: i64) -> i128 {
        super::unix_err(UNSUPPORTED_READ)
    }
    pub(super) unsafe fn stream_read_to_string(_h: i64) -> i128 {
        super::unix_err(UNSUPPORTED_READ_TO_STRING)
    }
    pub(super) unsafe fn stream_write(_h: i64, _data: *const super::super::vec::GosVec) -> i128 {
        super::unix_err(UNSUPPORTED_WRITE)
    }
    pub(super) unsafe fn stream_close(_h: i64) {}
}

/// `net::UnixListener::bind(path) -> Result<UnixListener, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_unix_listener_bind(path: *const c_char) -> i128 {
    ffi_entry!(0i128, { unsafe { imp::listener_bind(path) } })
}

/// `net::UnixListener::accept(handle) -> Result<(UnixStream, String), Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_unix_listener_accept(h: i64) -> i128 {
    ffi_entry!(0i128, { unsafe { imp::listener_accept(h) } })
}

/// `net::UnixListener::close(handle)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_unix_listener_close(h: i64) {
    ffi_entry!((), { unsafe { imp::listener_close(h) } });
}

/// `net::UnixStream::connect(path) -> Result<UnixStream, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_unix_stream_connect(path: *const c_char) -> i128 {
    ffi_entry!(0i128, { unsafe { imp::stream_connect(path) } })
}

/// `net::UnixStream::read(handle, max) -> Result<[u8], Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_unix_stream_read(h: i64, max: i64) -> i128 {
    ffi_entry!(0i128, { unsafe { imp::stream_read(h, max) } })
}

/// `net::UnixStream::read_to_string(handle) -> Result<String, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_unix_stream_read_to_string(h: i64) -> i128 {
    ffi_entry!(0i128, { unsafe { imp::stream_read_to_string(h) } })
}

/// `net::UnixStream::write(handle, data: [u8]) -> Result<i64, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_unix_stream_write(h: i64, data: *const super::vec::GosVec) -> i128 {
    ffi_entry!(0i128, { unsafe { imp::stream_write(h, data) } })
}

/// `net::UnixStream::close(handle)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_unix_stream_close(h: i64) {
    ffi_entry!((), { unsafe { imp::stream_close(h) } });
}
