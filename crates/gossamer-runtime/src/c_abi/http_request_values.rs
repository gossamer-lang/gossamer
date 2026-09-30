#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::wildcard_imports)]

//! Request-scoped value map shims for the compiled tiers - Go's
//! `context.WithValue` / `ctx.Value(key)` analog on `http::Request`.
//!
//! `set_value(req, key, val)` mutates the request's `values` bag in
//! place (replace-then-push, the same semantics as
//! `gos_rt_http_request_set_header`) and returns the same pointer for
//! chaining; `value(req, key)` reads it back, empty string when the
//! key was never attached. This mirrors the router `params` /
//! `path_value` field pattern in `http_client` and the interp
//! `__values` builtins in
//! `gossamer_interp::stdlib_builtins::http_request_values`.
//!
//! Cross-tier contract: a handler that threads the returned request
//! (`let r = r.set_value("user", "alice")`) observes the attached
//! value identically on the VM, Cranelift, and LLVM tiers. On the
//! compiled tiers `set_value` returns the same `*mut GosHttpRequest`
//! it mutated; the interp tier rebuilds an immutable Request struct
//! with the updated bag and returns that. Threading the result is
//! therefore the rule on every tier.
//!
//! Field dependency: `GosHttpRequest` must carry
//! `pub values: Vec<(String, String)>` (see the ready-to-apply delta
//! in the accompanying spec - the field is added to the struct in
//! `http_client.rs` and initialised `Vec::new()` at every
//! constructor, exactly as `params` was).

use std::os::raw::c_char;

use super::*;

/// Owns a borrowed C-string into a Rust `String`; null → empty.
///
/// # Safety
///
/// `p` is null or a live string body.
unsafe fn cstr_to_string(p: *const c_char) -> String {
    // SAFETY: this function's contract is the one the reader states for `p`.
    unsafe { crate::c_abi::gos_str_arg_string(p) }
}

/// `Request.set_value(key, value) -> Request` - attach a
/// request-scoped string value, replacing any prior value under the
/// same key. Mutates in place and returns `req` for chaining (the
/// `with_header` pattern). A null request passes through unchanged.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_request_set_value(
    req: *mut GosHttpRequest,
    key: *const c_char,
    value: *const c_char,
) -> *mut GosHttpRequest {
    ffi_entry!(req, {
        if req.is_null() {
            return req;
        }
        // SAFETY: `key` is this shim's argument, as `cstr_to_string` requires (C-ABI contract).
        let k = unsafe { cstr_to_string(key) };
        // SAFETY: `value` is this shim's argument, as `cstr_to_string` requires (C-ABI contract).
        let v = unsafe { cstr_to_string(value) };
        // SAFETY: `req` is a handle from compiled code, checked non-null above and live for the whole call.
        let r = unsafe { &mut *req };
        r.values.retain(|(ek, _)| *ek != k);
        r.values.push((k, v));
        req
    })
}

/// `Request.value(key) -> String` - the request-scoped value attached
/// under `key`, or `""` when none was set. Mirrors Go's
/// `ctx.Value(key)` read side. Keys match exactly (case-sensitive):
/// they are user namespace, not HTTP header names.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_request_value(
    req: *const GosHttpRequest,
    key: *const c_char,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if req.is_null() || key.is_null() {
            return alloc_cstring(b"");
        }
        // SAFETY: `key` is this shim's argument, as `cstr_to_string` requires (C-ABI contract).
        let wanted = unsafe { cstr_to_string(key) };
        // SAFETY: `req` is a handle from compiled code, checked non-null above and live for the whole call.
        let r = unsafe { &*req };
        let found = r
            .values
            .iter()
            .find(|(k, _)| *k == wanted)
            .map_or(String::new(), |(_, v)| v.clone());
        alloc_cstring(found.as_bytes())
    })
}

/// First value for `key` in an `application/x-www-form-urlencoded`
/// request body, percent-decoded with `+` as space (the `std::http::
/// form` semantics), or `""` when the key is absent.
fn form_lookup(body: &str, key: &str) -> String {
    for pair in body.split('&') {
        let (raw_key, raw_val) = pair.split_once('=').unwrap_or((pair, ""));
        if crate::c_abi::url::percent_decode(raw_key, true) == key {
            return crate::c_abi::url::percent_decode(raw_val, true);
        }
    }
    String::new()
}

/// `Request.form_value(key) -> String` - the first form field value
/// parsed from the request body, or `""` when absent. Convenience over
/// hand-parsing `r.body` through `http::form`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_request_form_value(
    req: *const GosHttpRequest,
    key: *const c_char,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if req.is_null() || key.is_null() {
            return alloc_cstring(b"");
        }
        // SAFETY: `key` is this shim's argument, as `cstr_to_string` requires (C-ABI contract).
        let wanted = unsafe { cstr_to_string(key) };
        // SAFETY: `req` is a handle from compiled code, checked non-null above and live for the whole call.
        let body = crate::c_abi::http_client::request_body_slice(unsafe { &*req });
        let body = std::str::from_utf8(body).unwrap_or("");
        alloc_cstring(form_lookup(body, &wanted).as_bytes())
    })
}

/// `Request.basic_auth() -> Option<(String, String)>` - the decoded
/// `(user, password)` from an `Authorization: Basic <base64>` header,
/// or `None` when the header is missing or malformed. Packed as the
/// 2-word `Option` the compiled tiers read (`disc=0` Some, payload a
/// `(String, String)` pair pointer), mirroring `gos_rt_str_split_once`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_http_request_basic_auth(req: *const GosHttpRequest) -> i128 {
    ffi_entry!(crate::c_abi::result::gos_rt_result_new(1, 0), {
        if req.is_null() {
            return crate::c_abi::result::gos_rt_result_new(1, 0);
        }
        // SAFETY: `req` is a handle from compiled code, checked non-null above and live for the whole call.
        let r = unsafe { &*req };
        let header = r
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .map_or("", |(_, v)| v.as_str());
        let Some((user, pass)) = decode_basic_credentials(header) else {
            return crate::c_abi::result::gos_rt_result_new(1, 0);
        };
        #[repr(C)]
        struct Pair {
            a: i64,
            b: i64,
        }
        let pair = Box::into_raw(Box::new(Pair {
            a: alloc_cstring(user.as_bytes()) as i64,
            b: alloc_cstring(pass.as_bytes()) as i64,
        }));
        crate::c_abi::result::gos_rt_result_new(0, pair as i64)
    })
}

/// Decodes an `Authorization: Basic <base64(user:pass)>` header value
/// into `(user, password)`, or `None` for any non-Basic / malformed
/// input.
fn decode_basic_credentials(header: &str) -> Option<(String, String)> {
    let token = header
        .strip_prefix("Basic ")
        .or_else(|| header.strip_prefix("basic "))?;
    let decoded = crate::c_abi::encoding::base64_decode(token.trim()).ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (user, pass) = decoded.split_once(':')?;
    Some((user.to_string(), pass.to_string()))
}
