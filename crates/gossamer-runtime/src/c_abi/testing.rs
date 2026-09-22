#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::similar_names)]
#![allow(clippy::many_single_char_names)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::same_length_and_capacity)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::cast_ptr_alignment)]
#![allow(clippy::ptr_as_ptr)]
#![allow(static_mut_refs)]
#![allow(unused_unsafe)]
#![allow(clippy::wildcard_imports)]

use std::collections::HashMap;
use std::os::raw::c_char;
use std::sync::{Mutex, OnceLock};

/// Process-scoped static responders keyed by their immutable response.
///
/// Source benchmarks call `httptest::server` repeatedly while calibrating.
/// Reusing an identical responder keeps that benchmark focused on client
/// transport work instead of accumulating one detached accept thread per
/// sample.
static HTTP_TEST_SERVERS: OnceLock<Mutex<HashMap<(u16, String), String>>> = OnceLock::new();

// ---------------------------------------------------------------
// testing module - minimal `check`, `check_eq`, `check_ok` that
// log to stderr. Real test discovery / reporting is done via the
// interpreter today; these stubs make the example compile.
// ---------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_testing_check(cond: bool, msg: *const c_char) -> bool {
    ffi_entry!(false, {
        if !cond {
            let m = if msg.is_null() {
                "check failed".to_string()
            } else {
                unsafe { crate::c_abi::gos_str_arg_string(msg) }
            };
            eprintln!("test check failed: {m}");
        }
        cond
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_testing_check_eq_i64(a: i64, b: i64, msg: *const c_char) -> bool {
    ffi_entry!(false, {
        let ok = a == b;
        if !ok {
            let m = if msg.is_null() {
                String::new()
            } else {
                unsafe { crate::c_abi::gos_str_arg_string(msg) }
            };
            eprintln!("test check_eq failed: {a} != {b} ({m})");
        }
        ok
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_testing_wait_for_scheduler_idle(timeout_ms: i64) -> bool {
    ffi_entry!(false, {
        let deadline = crate::platform::Instant::now()
            + std::time::Duration::from_millis(timeout_ms.max(0) as u64);
        let scheduler = crate::sched_global::scheduler();
        loop {
            let stats = scheduler.stats();
            if scheduler.live_goroutines() == 0 && stats.spawned == stats.finished {
                return true;
            }
            if crate::platform::Instant::now() >= deadline {
                return false;
            }
            crate::platform::sleep(std::time::Duration::from_millis(1));
        }
    })
}

/// Starts or reuses a process-scoped static HTTP responder for source
/// integration tests and benchmarks.
///
/// The listener is bound before the worker starts, making the returned loopback
/// URL safe to use immediately. Test processes own the worker lifetime: it is
/// intentionally released by normal process shutdown rather than exposing a
/// cross-tier handle with native-pointer lifetime requirements.
pub fn httptest_server(status: i64, body: &str) -> Result<String, std::io::Error> {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let status = u16::try_from(status)
        .ok()
        .filter(|code| (100..=599).contains(code))
        .unwrap_or(500);
    let key = (status, body.to_string());
    let servers = HTTP_TEST_SERVERS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut servers = servers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(url) = servers.get(&key) {
        return Ok(url.clone());
    }

    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let response = format!(
        "HTTP/1.1 {status} Test\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes();
    std::thread::Builder::new()
        .name("gos-httptest-server".to_string())
        .spawn(move || {
            loop {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(5)));
                let mut head = [0_u8; 8192];
                let _ = stream.read(&mut head);
                let _ = stream.write_all(&response);
                let _ = stream.flush();
            }
        })?;
    let url = format!("http://{address}");
    servers.insert(key, url.clone());
    Ok(url)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_httptest_server(status: i64, body: *const c_char) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let body = if body.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(body) }
        };
        let Ok(url) = httptest_server(status, &body) else {
            return std::ptr::null_mut();
        };
        super::string::alloc_cstring(url.as_bytes())
    })
}

/// `httptest::record(handler, method, path, body)` - calls `handler` with a
/// request built in memory and answers its response.
///
/// No socket, no port, no accept loop: a handler is a function from a
/// request to a response, and a test that only wants to know what it
/// answers should not have to run a server to find out. Use
/// `http::Server` with port 0 when the test is about the wire.
#[cfg(not(target_arch = "wasm32"))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_httptest_record(
    method: *const std::os::raw::c_char,
    path: *const std::os::raw::c_char,
    body: *const std::os::raw::c_char,
    handler_env: *mut u8,
    handler_fn: i64,
) -> i128 {
    ffi_entry!(0i128, {
        if handler_fn == 0 {
            let err =
                crate::c_abi::errors::error_new_from_bytes(b"httptest::record: handler is null");
            return crate::c_abi::vec::pack_result(1, err as i64);
        }
        let text = |p: *const std::os::raw::c_char| {
            if p.is_null() {
                String::new()
            } else {
                unsafe { crate::c_abi::gos_str_arg_string(p) }
            }
        };
        let body_bytes = text(body).into_bytes();
        let mut request = crate::c_abi::http_client::GosHttpRequest {
            method: text(method),
            url: text(path),
            headers: Vec::new(),
            body: body_bytes,
            body_offset: 0,
            params: Vec::new(),
            values: Vec::new(),
            agent: None,
            peer: String::new(),
            context_site: None,
            context: crate::c_abi::context::open_request_context(0),
        };
        type HandlerFn = unsafe extern "C-unwind" fn(
            env: *mut u8,
            req: *mut crate::c_abi::http_client::GosHttpRequest,
        ) -> i128;
        // SAFETY: `handler_fn` came from `gos_fn_addr` over a handler
        // dispatch symbol at the call site, with `handler_env` alongside.
        let handler: HandlerFn = unsafe { std::mem::transmute(handler_fn as usize) };
        let req_ptr: *mut crate::c_abi::http_client::GosHttpRequest = &raw mut request;
        let result = unsafe { handler(handler_env, req_ptr) };
        crate::c_abi::context::close_request_context(std::mem::replace(&mut request.context, 0));
        result
    })
}
