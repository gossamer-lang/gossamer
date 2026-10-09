#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
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
#![allow(clippy::wildcard_imports)]

// ---------------------------------------------------------------
// SyncI64Vec / SyncU8Vec - cross-goroutine-safe vec wrappers
// ---------------------------------------------------------------
//
// Same conceptual shape as `GosI64Vec` / `GosU8Vec` but with the
// backing storage owned by a `parking_lot::Mutex<Vec<_>>`. Every
// operation takes the mutex briefly so concurrent push/get/set
// across goroutines is safe. Use this whenever the same `vec`
// value is captured into a `go` closure or placed on a channel.

pub struct GosSyncI64Vec {
    inner: parking_lot::Mutex<Vec<i64>>,
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sync_i64_new(len: i64) -> *mut GosSyncI64Vec {
    ffi_entry!({
        let n = if len < 0 { 0 } else { len as usize };
        Box::into_raw(Box::new(GosSyncI64Vec {
            inner: parking_lot::Mutex::new(vec![0i64; n]),
        }))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sync_i64_drop(v: *mut GosSyncI64Vec) {
    ffi_entry!({
        if v.is_null() {
            return;
        }
        // SAFETY: `v` is non-null (checked above), the vector its constructor boxed, which this
        // call consumes (C-ABI contract).
        drop(unsafe { Box::from_raw(v) });
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sync_i64_len(v: *const GosSyncI64Vec) -> i64 {
    ffi_entry!({
        if v.is_null() {
            return 0;
        }
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        let v = unsafe { &*v };
        i64::try_from(v.inner.lock().len()).unwrap_or(i64::MAX)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sync_i64_get(v: *const GosSyncI64Vec, idx: i64) -> i64 {
    ffi_entry!({
        if v.is_null() || idx < 0 {
            return 0;
        }
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        let v = unsafe { &*v };
        let g = v.inner.lock();
        g.get(idx as usize).copied().unwrap_or(0)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sync_i64_set(v: *mut GosSyncI64Vec, idx: i64, val: i64) {
    ffi_entry!({
        if v.is_null() || idx < 0 {
            return;
        }
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        let v = unsafe { &*v };
        let mut g = v.inner.lock();
        if let Some(slot) = g.get_mut(idx as usize) {
            *slot = val;
        }
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sync_i64_push(v: *mut GosSyncI64Vec, val: i64) {
    ffi_entry!({
        if v.is_null() {
            return;
        }
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        let v = unsafe { &*v };
        v.inner.lock().push(val);
    });
}

/// Atomic increment: `vec[idx] += delta`, returns the new value.
/// Used by fan-out workers that share a counter slot without
/// needing a separate AtomicI64 per slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sync_i64_add(v: *mut GosSyncI64Vec, idx: i64, delta: i64) -> i64 {
    ffi_entry!({
        if v.is_null() || idx < 0 {
            return 0;
        }
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        let v = unsafe { &*v };
        let mut g = v.inner.lock();
        if let Some(slot) = g.get_mut(idx as usize) {
            *slot = slot.wrapping_add(delta);
            *slot
        } else {
            0
        }
    })
}

pub struct GosSyncU8Vec {
    inner: parking_lot::Mutex<Vec<u8>>,
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_sync_u8_new(len: i64) -> *mut GosSyncU8Vec {
    ffi_entry!({
        let n = if len < 0 { 0 } else { len as usize };
        Box::into_raw(Box::new(GosSyncU8Vec {
            inner: parking_lot::Mutex::new(vec![0u8; n]),
        }))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sync_u8_drop(v: *mut GosSyncU8Vec) {
    ffi_entry!({
        if v.is_null() {
            return;
        }
        // SAFETY: `v` is non-null (checked above), the vector its constructor boxed, which this
        // call consumes (C-ABI contract).
        drop(unsafe { Box::from_raw(v) });
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sync_u8_len(v: *const GosSyncU8Vec) -> i64 {
    ffi_entry!({
        if v.is_null() {
            return 0;
        }
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        let v = unsafe { &*v };
        i64::try_from(v.inner.lock().len()).unwrap_or(i64::MAX)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sync_u8_get(v: *const GosSyncU8Vec, idx: i64) -> i64 {
    ffi_entry!({
        if v.is_null() || idx < 0 {
            return 0;
        }
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        let v = unsafe { &*v };
        let g = v.inner.lock();
        g.get(idx as usize).copied().map_or(0, i64::from)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sync_u8_set(v: *mut GosSyncU8Vec, idx: i64, val: i64) {
    ffi_entry!({
        if v.is_null() || idx < 0 {
            return;
        }
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        let v = unsafe { &*v };
        let mut g = v.inner.lock();
        if let Some(slot) = g.get_mut(idx as usize) {
            *slot = val as u8;
        }
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sync_u8_push(v: *mut GosSyncU8Vec, val: i64) {
    ffi_entry!({
        if v.is_null() {
            return;
        }
        // SAFETY: `v` is a handle from compiled code, checked non-null above and live for the whole call.
        let v = unsafe { &*v };
        v.inner.lock().push(val as u8);
    });
}
