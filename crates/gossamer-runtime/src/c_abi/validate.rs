#![allow(clippy::missing_safety_doc)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::cast_possible_truncation)]
// `use super::*` pulls in the shared FFI helpers (`alloc_cstring`,
// `ffi_entry!`) the same way `bytes_builder` does; the glob is the
// established convention here.
#![allow(clippy::wildcard_imports)]

//! Runtime support for `std::validate` - the `FieldError` and
//! `Errors` data handles (the trait-driven surface is documented as a
//! follow-up; see the module note in the interp builtin).
//!
//! Both handle types are opaque heap `Box`es; compiled tiers carry
//! the pointer as an `i64` and the MIR receiver-kind dispatch tags
//! constructor results `validate::FieldError` / `validate::Errors` so
//! method calls route to the helpers below. The handle is never freed
//! (it leaks at process exit), matching `sync::Map` / `bytes::Builder`
//! / `math::rand::Rng`: these are short-lived per-request validation
//! scratch values, not graph nodes.
//!
//! `Errors::add` copies the supplied `FieldError`'s contents into the
//! collection, so the caller's `FieldError` handle stays usable
//! afterward and the pointer the MIR still holds never dangles.

use std::collections::BTreeMap;
use std::os::raw::c_char;

use super::*;

/// # Safety
///
/// `p` is null or a live string body.
unsafe fn cstr_to_string(p: *const c_char) -> String {
    // SAFETY: this function's contract is the one the reader states for `p`.
    unsafe { crate::c_abi::gos_str_arg_string(p) }
}

// ---------------------------------------------------------------
// validate::FieldError - a single field-level failure
// ---------------------------------------------------------------

/// Opaque heap handle for one field-level validation failure.
#[derive(Clone)]
pub struct GosFieldError {
    path: String,
    message: String,
    code: String,
}

/// Allocate a `FieldError { path, message, code }`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_field_error_new(
    path: *const c_char,
    message: *const c_char,
    code: *const c_char,
) -> *mut GosFieldError {
    ffi_entry!(std::ptr::null_mut(), {
        Box::into_raw(Box::new(GosFieldError {
            // SAFETY: `path` is this shim's argument, as `cstr_to_string` requires (C-ABI
            // contract).
            path: unsafe { cstr_to_string(path) },
            // SAFETY: `message` is this shim's argument, as `cstr_to_string` requires (C-ABI
            // contract).
            message: unsafe { cstr_to_string(message) },
            // SAFETY: `code` is this shim's argument, as `cstr_to_string` requires (C-ABI
            // contract).
            code: unsafe { cstr_to_string(code) },
        }))
    })
}

/// `fe.path()` - the field path, as a fresh runtime c-string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_field_error_path(fe: *mut GosFieldError) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if fe.is_null() {
            return alloc_cstring(b"");
        }
        // SAFETY: `fe` is a handle from compiled code, checked non-null above and live for the whole call.
        alloc_cstring(unsafe { &*fe }.path.as_bytes())
    })
}

/// `fe.message()` - the failure message, as a fresh runtime c-string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_field_error_message(fe: *mut GosFieldError) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if fe.is_null() {
            return alloc_cstring(b"");
        }
        // SAFETY: `fe` is a handle from compiled code, checked non-null above and live for the whole call.
        alloc_cstring(unsafe { &*fe }.message.as_bytes())
    })
}

/// `fe.code()` - the machine-readable rule code, as a fresh
/// runtime c-string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_field_error_code(fe: *mut GosFieldError) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if fe.is_null() {
            return alloc_cstring(b"");
        }
        // SAFETY: `fe` is a handle from compiled code, checked non-null above and live for the whole call.
        alloc_cstring(unsafe { &*fe }.code.as_bytes())
    })
}

// ---------------------------------------------------------------
// validate::Errors - per-field collection of failures
// ---------------------------------------------------------------

/// Opaque heap handle: all failures for one struct, keyed by field
/// path in sorted order so `collect` / `get` are deterministic on
/// every tier.
pub struct GosErrors {
    fields: BTreeMap<String, Vec<GosFieldError>>,
}

/// Allocate an empty `Errors`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_validate_errors_new() -> *mut GosErrors {
    ffi_entry!(std::ptr::null_mut(), {
        Box::into_raw(Box::new(GosErrors {
            fields: BTreeMap::new(),
        }))
    })
}

/// `errs.add(field, fe)` - append a copy of `fe` to `field`'s list.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_validate_errors_add(
    errs: *mut GosErrors,
    field: *const c_char,
    fe: *mut GosFieldError,
) {
    ffi_entry!((), {
        if errs.is_null() || fe.is_null() {
            return;
        }
        // SAFETY: `field` is this shim's argument, as `cstr_to_string` requires (C-ABI contract).
        let key = unsafe { cstr_to_string(field) };
        // SAFETY: `fe` is a handle from compiled code, checked non-null above and live for the whole call.
        let value = unsafe { &*fe }.clone();
        // SAFETY: `errs` is a handle from compiled code, checked non-null above and live for the whole call.
        unsafe { &mut *errs }
            .fields
            .entry(key)
            .or_default()
            .push(value);
    });
}

/// `errs.is_empty()` - `1` when no failures recorded, else `0`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_validate_errors_is_empty(errs: *mut GosErrors) -> i64 {
    ffi_entry!(1, {
        if errs.is_null() {
            return 1;
        }
        // SAFETY: `errs` is a handle from compiled code, checked non-null above and live for the whole call.
        i64::from(unsafe { &*errs }.fields.is_empty())
    })
}

/// `errs.len()` - total `FieldError` count across every field.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_validate_errors_len(errs: *mut GosErrors) -> i64 {
    ffi_entry!(0, {
        if errs.is_null() {
            return 0;
        }
        // SAFETY: `errs` is a handle from compiled code, checked non-null above and live for the whole call.
        unsafe { &*errs }
            .fields
            .values()
            .map(Vec::len)
            .sum::<usize>() as i64
    })
}

/// `errs.count(field)` - number of failures recorded for `field`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_validate_errors_count(
    errs: *mut GosErrors,
    field: *const c_char,
) -> i64 {
    ffi_entry!(0, {
        if errs.is_null() {
            return 0;
        }
        // SAFETY: `field` is this shim's argument, as `cstr_to_string` requires (C-ABI contract).
        let key = unsafe { cstr_to_string(field) };
        // SAFETY: `errs` is a handle from compiled code, checked non-null above and live for the whole call.
        unsafe { &*errs }
            .fields
            .get(&key)
            .map_or(0, |v| v.len() as i64)
    })
}

/// `errs.get(field)` - the field's messages joined with `"; "`, or
/// `""` when the field has no recorded failures.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_validate_errors_get(
    errs: *mut GosErrors,
    field: *const c_char,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if errs.is_null() {
            return alloc_cstring(b"");
        }
        // SAFETY: `field` is this shim's argument, as `cstr_to_string` requires (C-ABI contract).
        let key = unsafe { cstr_to_string(field) };
        // SAFETY: `errs` is a handle from compiled code, checked non-null above and live for the whole call.
        let joined = unsafe { &*errs }
            .fields
            .get(&key)
            .map(|v| {
                v.iter()
                    .map(|e| e.message.as_str())
                    .collect::<Vec<_>>()
                    .join("; ")
            })
            .unwrap_or_default();
        alloc_cstring(joined.as_bytes())
    })
}

/// `errs.collect()` - every failure rendered `field: message`,
/// sorted by field, joined with `"; "`. Mirrors the
/// `gossamer_std::validate::Errors` `Display` impl.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_validate_errors_collect(errs: *mut GosErrors) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if errs.is_null() {
            return alloc_cstring(b"");
        }
        let mut parts: Vec<String> = Vec::new();
        // SAFETY: `errs` is a handle from compiled code, checked non-null above and live for the whole call.
        for (field, list) in &unsafe { &*errs }.fields {
            for e in list {
                parts.push(format!("{field}: {}", e.message));
            }
        }
        alloc_cstring(parts.join("; ").as_bytes())
    })
}
