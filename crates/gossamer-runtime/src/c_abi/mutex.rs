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

use std::sync::atomic::{AtomicI64, Ordering};

// ---------------------------------------------------------------
// Mutex<T> primitive
// ---------------------------------------------------------------
//
// Naked synchronisation primitive - no payload, no RAII guard,
// the user follows lock/unlock discipline. Backed by
// `parking_lot::Mutex<()>` so contention uses futexes on
// Linux. The pointer is heap-allocated and shared by every
// goroutine that captures it.

pub struct GosMutex {
    inner: parking_lot::Mutex<()>,
    /// Goroutine id of the most recent unlocker. Read by the next
    /// lock acquirer to record a happens-before edge into the race
    /// detector. `-1` means "never been locked".
    last_unlocker: AtomicI64,
    /// Goroutine id of the current owner (the goroutine that took
    /// the lock). `-1` means unlocked. Read by `unlock` to refuse
    /// a cross-goroutine release - `force_unlock` on a mutex held
    /// by another goroutine is `parking_lot` UB.
    owner: AtomicI64,
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_mutex_new() -> *mut GosMutex {
    ffi_entry!(std::ptr::null_mut(), {
        Box::into_raw(Box::new(GosMutex {
            inner: parking_lot::Mutex::new(()),
            last_unlocker: AtomicI64::new(-1),
            owner: AtomicI64::new(-1),
        }))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_mutex_lock(m: *mut GosMutex) {
    ffi_entry!((), {
        if m.is_null() {
            return;
        }
        // SAFETY: `m` is a handle from compiled code, checked non-null above and live for the whole call.
        let m = unsafe { &*m };
        // Forget the guard - the user calls unlock explicitly.
        let guard = m.inner.lock();
        std::mem::forget(guard);
        m.owner
            .store(i64::from(crate::race::current_gid()), Ordering::Release);
        let from = m.last_unlocker.load(Ordering::Acquire);
        if from >= 0 {
            crate::race::record_sync(u32::try_from(from).unwrap_or(0), crate::race::current_gid());
        }
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_mutex_unlock(m: *mut GosMutex) {
    ffi_entry!((), {
        if m.is_null() {
            return;
        }
        // SAFETY: `m` is a handle from compiled code, checked non-null above and live for the whole call.
        let m = unsafe { &*m };
        let me = i64::from(crate::race::current_gid());
        let owner = m.owner.load(Ordering::Acquire);
        if owner != me {
            eprintln!(
                "panic: mutex.unlock() from goroutine {me} but mutex is held by {owner}; \
                 cross-goroutine unlock is undefined behaviour",
            );
            std::process::abort();
        }
        m.owner.store(-1, Ordering::Release);
        m.last_unlocker.store(me, Ordering::Release);
        // SAFETY: matched with the `forget` in lock - the lock is held by
        // this goroutine (owner check above), so releasing it here is the
        // unlock of a held lock.
        unsafe { m.inner.force_unlock() };
    });
}
