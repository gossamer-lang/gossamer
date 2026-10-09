#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::wildcard_imports)]

use std::collections::HashMap;
use std::os::raw::c_char;

use super::*;

// ---------------------------------------------------------------
// sync::Map - concurrent String -> String map
// ---------------------------------------------------------------
//
// Wraps `parking_lot::RwLock<HashMap<String, String>>`. Reads
// take the shared lock; writes take the exclusive lock. The
// String value choice mirrors the most common Go `sync.Map`
// caller (caches, session stores, feature-flag maps); callers
// that need richer payload types can JSON-encode through the
// same surface.

pub struct GosSyncMap {
    inner: parking_lot::RwLock<HashMap<String, String>>,
}

super::rc::managed_handle!(GosSyncMap);

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sync_map_new() -> *mut GosSyncMap {
    ffi_entry!({
        super::rc::alloc_managed(GosSyncMap {
            inner: parking_lot::RwLock::new(HashMap::new()),
        })
    })
}

/// # Safety
///
/// `p` is null or a live string body.
unsafe fn cstr_to_string(p: *const c_char) -> String {
    // SAFETY: this function's contract is the one the reader states for `p`.
    unsafe { crate::c_abi::gos_str_arg_string(p) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sync_map_set(
    m: *mut GosSyncMap,
    key: *const c_char,
    value: *const c_char,
) {
    ffi_entry!({
        if m.is_null() {
            return;
        }
        // SAFETY: `m` is a handle from compiled code, checked non-null above and live for the whole call.
        let m = unsafe { &*m };
        // SAFETY: `key` is this shim's argument, as `cstr_to_string` requires (C-ABI contract).
        let k = unsafe { cstr_to_string(key) };
        // SAFETY: `value` is this shim's argument, as `cstr_to_string` requires (C-ABI contract).
        let v = unsafe { cstr_to_string(value) };
        m.inner.write().insert(k, v);
    });
}

/// Returns `Option<String>` as `*mut GosResult` (disc=0 → Some
/// with c-string payload, disc=1 → None). Mirrors the shape used
/// by `gos_rt_map_get_str` and friends so the MIR dispatcher can
/// pin the destination to `Option<String>` without inventing a
/// fresh result-discriminant convention.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sync_map_get(m: *mut GosSyncMap, key: *const c_char) -> i128 {
    ffi_entry!({
        if m.is_null() {
            return gos_rt_result_new(1, 0);
        }
        // SAFETY: `key` is this shim's argument, as `cstr_to_string` requires (C-ABI contract).
        let k = unsafe { cstr_to_string(key) };
        // SAFETY: `m` is a handle from compiled code, checked non-null above and live for the whole call.
        let guard = unsafe { &*m }.inner.read();
        match guard.get(&k) {
            Some(v) => {
                let cs = alloc_cstring(v.as_bytes()) as i64;
                gos_rt_result_new(0, cs)
            }
            None => gos_rt_result_new(1, 0),
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sync_map_delete(m: *mut GosSyncMap, key: *const c_char) {
    ffi_entry!({
        if m.is_null() {
            return;
        }
        // SAFETY: `key` is this shim's argument, as `cstr_to_string` requires (C-ABI contract).
        let k = unsafe { cstr_to_string(key) };
        // SAFETY: `m` is a handle from compiled code, checked non-null above and live for the whole call.
        unsafe { &*m }.inner.write().remove(&k);
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sync_map_len(m: *mut GosSyncMap) -> i64 {
    ffi_entry!({
        if m.is_null() {
            return 0;
        }
        // SAFETY: `m` is a handle from compiled code, checked non-null above and live for the whole call.
        unsafe { &*m }.inner.read().len() as i64
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sync_map_contains(m: *mut GosSyncMap, key: *const c_char) -> i64 {
    ffi_entry!({
        if m.is_null() {
            return 0;
        }
        // SAFETY: `key` is this shim's argument, as `cstr_to_string` requires (C-ABI contract).
        let k = unsafe { cstr_to_string(key) };
        // SAFETY: `m` is a handle from compiled code, checked non-null above and live for the whole call.
        i64::from(unsafe { &*m }.inner.read().contains_key(&k))
    })
}

/// Returns the live keys as a `*mut GosVec` of `*c_char`
/// (`Vec<String>` ABI). Order is not guaranteed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sync_map_keys(
    m: *mut GosSyncMap,
) -> *mut crate::c_abi::vec::GosVec {
    ffi_entry!({
        let v = {
            crate::c_abi::vec::gos_rt_vec_new_typed(8, crate::c_abi::vec::vec_elem_kind::STRING)
        };
        if m.is_null() {
            return v;
        }
        // SAFETY: `m` is a handle from compiled code, checked non-null above and live for the whole call.
        for key in unsafe { &*m }.inner.read().keys() {
            let cs = alloc_cstring(key.as_bytes());
            let cs_i64 = cs as i64;
            // SAFETY: `v` is the fresh vec made above, or null, which `gos_rt_vec_push` accepts,
            // and `cs_i64` is one 8-byte element.
            unsafe {
                crate::c_abi::vec::gos_rt_vec_push(v, std::ptr::addr_of!(cs_i64).cast::<u8>());
            }
        }
        v
    })
}
