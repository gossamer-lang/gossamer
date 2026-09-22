//! HTTP/3 server C-ABI shim (`std::http_h3::serve`). Mirrors the
//! h1 / h2 server shims: the MIR lowerer emits a call to
//! [`gos_rt_http3_serve`] passing the listen address, the TLS
//! certificate / key file paths, and the handler as an
//! `(env_ptr, fn_addr)` pair. Each accepted request is marshalled
//! into the same [`GosHttpRequest`] struct the h1 / h2 servers use
//! and the handler's response is extracted through the shared HTTP
//! response lowering path, so the body and headers a
//! handler returns are served byte-for-byte across every tier.
//!
//! The QUIC + h3 engine lives in [`gossamer_http3`]; this shim only
//! adapts the C-ABI handler contract into the engine's
//! `Fn(H3Request) -> H3Response` callback.

#![allow(clippy::missing_safety_doc)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::os::raw::c_char;

use gossamer_http3::{H3Request, H3Response};

use super::http_client::GosHttpRequest;
use super::http_server::extract_response_struct;

type HandlerFn = unsafe extern "C-unwind" fn(env: *mut u8, req: *mut GosHttpRequest) -> i128;

/// Reads a nullable C string into an owned `String`; an empty
/// default is substituted for a null pointer.
fn cstr_or(ptr: *const c_char, default: &str) -> String {
    if ptr.is_null() {
        default.to_string()
    } else {
        unsafe { crate::c_abi::gos_str_arg_string(ptr) }
    }
}

/// Marshals an engine request into the runtime's `GosHttpRequest`.
/// Header names arrive lowercase off the HTTP/3 framing; the bag is
/// normalized (sort + last-wins dedupe) to the same view the h1 / h2
/// paths produce so `r.headers` agrees across transports.
fn gos_request_from_wire(req: H3Request) -> GosHttpRequest {
    let mut headers = req.headers;
    super::http_server::normalize_header_bag(&mut headers);
    GosHttpRequest {
        method: req.method,
        url: if req.query.is_empty() {
            req.path
        } else {
            format!("{}?{}", req.path, req.query)
        },
        headers,
        body: req.body,
        body_offset: 0,
        params: Vec::new(),
        values: Vec::new(),
        agent: None,
        peer: String::new(),
        context_site: None,
        context: 0,
    }
}

/// Invokes the Gossamer handler for one request and lowers its
/// result into an engine response. A null `fn_addr` (legacy stub),
/// an `Err` result, or a null response all resolve to a 500.
fn dispatch(env_addr: usize, fn_addr: usize, req: H3Request) -> H3Response {
    if fn_addr == 0 {
        return H3Response {
            status: 200,
            headers: Vec::new(),
            body: b"ok".to_vec(),
        };
    }
    let mut gos_req = gos_request_from_wire(req);
    // SAFETY: `fn_addr` / `env_addr` come from a `gos_fn_addr`
    // intrinsic at the user's `http_h3::serve(...)` call site. The
    // handler ABI is the shared `(env, req) -> i128` shape the h1 /
    // h2 servers use; the handler returns `Result<http::Response,
    // http::Error>` packed as an `i128`.
    let handler: HandlerFn = unsafe { std::mem::transmute::<usize, HandlerFn>(fn_addr) };
    let env_ptr = env_addr as *mut u8;
    let req_ptr: *mut GosHttpRequest = &raw mut gos_req;
    // SAFETY: `env_ptr` and `req_ptr` are valid for this call frame;
    // the handler consumes them inline or copies.
    let result_ptr = unsafe { handler(env_ptr, req_ptr) };
    let extracted = extract_response_struct(result_ptr);
    // SAFETY: `drop_handler_result` frees the response box the
    // handler returned. `result_ptr` is owned by this frame (the
    // extraction above clones out, it does not take ownership).
    unsafe { super::http_server::drop_handler_result(result_ptr) };
    // The handler may have allocated into the per-worker arena;
    // reset it after the response has been copied out so a
    // long-lived connection does not grow the arena without bound.
    unsafe { super::gc::gos_rt_gc_reset() };
    match extracted {
        Some((status, headers, body)) => H3Response {
            status,
            headers,
            body,
        },
        None => H3Response::internal_error(),
    }
}

/// Packs an `Err(errors::Error)` runtime `Result` carrying `msg` -
/// the bind-failure value `gos_rt_http3_serve` hands back to the
/// caller's `Result<(), http::Error>` match.
fn http3_serve_err_result(msg: &str) -> i128 {
    let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
    super::vec::pack_result(1, err as i64)
}

/// Binds a QUIC + HTTP/3 endpoint on `addr` with the TLS keypair at
/// `cert_path` / `key_path`, dispatching each request to
/// `handler_fn(handler_env, request)`. Returns the Gossamer-visible
/// `Result<(), http::Error>`: a packed `Err` when the cert / key
/// cannot be read or the endpoint cannot bind (interp parity - the
/// VM hands the same `Err` to the caller's match), or a packed
/// `Ok(())` if the accept loop ever exits.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_http3_serve(
    addr: *const c_char,
    cert_path: *const c_char,
    key_path: *const c_char,
    handler_env: *mut u8,
    handler_fn: i64,
) -> i128 {
    let addr_s = cstr_or(addr, "0.0.0.0:8443");
    let cert_s = cstr_or(cert_path, "");
    let key_s = cstr_or(key_path, "");
    let env_addr = handler_env as usize;
    let fn_addr = handler_fn as usize;
    // `serve_files` reads the keypair and produces the same error
    // wording the interpreter adapter does, so a cert / key / bind
    // failure renders byte-identically across tiers.
    match gossamer_http3::serve_files(&addr_s, &cert_s, &key_s, move |req: H3Request| {
        dispatch(env_addr, fn_addr, req)
    }) {
        Ok(()) => super::vec::pack_result(0, 0),
        Err(e) => http3_serve_err_result(&format!("http_h3::serve: {e}")),
    }
}
