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
// the user follows lock/unlock discipline. A goroutine that finds
// the lock held parks, giving its worker back to the scheduler, and
// is counted among the program's waiters, so a lock nothing will
// release is reported as a deadlock. A thread that is not a
// goroutine waits on a condvar. The pointer is heap-allocated and
// shared by every goroutine that captures it.

pub struct GosMutex {
    state: parking_lot::Mutex<MutexState>,
    /// Signalled on unlock for a waiting thread that is not a goroutine.
    released: parking_lot::Condvar,
    /// Goroutine id of the most recent unlocker. Read by the next
    /// lock acquirer to record a happens-before edge into the race
    /// detector. `-1` means "never been locked".
    last_unlocker: AtomicI64,
    /// Goroutine id of the current owner (the goroutine that took
    /// the lock). `-1` means unlocked. Read by `unlock` to refuse
    /// a cross-goroutine release.
    owner: AtomicI64,
}

super::rc::managed_handle!(GosMutex);

#[derive(Default)]
struct MutexState {
    locked: bool,
    /// Goroutines parked waiting for the lock, in arrival order.
    parked: std::collections::VecDeque<crate::sched::Gid>,
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_mutex_new() -> *mut GosMutex {
    ffi_entry!({
        super::rc::alloc_managed(GosMutex {
            state: parking_lot::Mutex::new(MutexState::default()),
            released: parking_lot::Condvar::new(),
            last_unlocker: AtomicI64::new(-1),
            owner: AtomicI64::new(-1),
        })
    })
}

/// Takes `m`, waiting until it is free.
fn acquire(m: &GosMutex) {
    if gossamer_coro::in_goroutine() {
        loop {
            let mut state = m.state.lock();
            if !state.locked {
                state.locked = true;
                return;
            }
            let mut guard = Some(state);
            crate::sched_global::park(crate::sched::ParkReason::Sync, |parker| {
                if let Some(state) = guard.as_mut() {
                    state.parked.push_back(parker.gid);
                }
                drop(guard.take());
            });
        }
    }
    let mut state = m.state.lock();
    if !state.locked {
        state.locked = true;
        return;
    }
    drop(state);
    // `main` waiting on a lock is waiting on the program's goroutines, so it
    // is counted the way a cohort join is: a lock no goroutine left can
    // release is a deadlock rather than a hang.
    let addr = std::ptr::from_ref(m) as usize;
    crate::sched_global::main_waits_on(
        "Mutex::lock",
        std::sync::Arc::new(move || {
            // SAFETY: the mutex outlives this wait, which `main` ends with
            // `end_main_wait` before `acquire` returns.
            let m = unsafe { &*(addr as *const GosMutex) };
            m.state.lock().locked
        }),
    );
    let mut state = m.state.lock();
    while state.locked {
        m.released.wait(&mut state);
    }
    state.locked = true;
    drop(state);
    crate::sched_global::end_main_wait();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_mutex_lock(m: *mut GosMutex) {
    ffi_entry!({
        if m.is_null() {
            return;
        }
        // SAFETY: `m` is a handle from compiled code, checked non-null above and live for the whole call.
        let m = unsafe { &*m };
        acquire(m);
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
    ffi_entry!({
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
        let mut state = m.state.lock();
        state.locked = false;
        let next = state.parked.pop_front();
        drop(state);
        if let Some(gid) = next {
            crate::sched_global::scheduler().unpark(gid);
        }
        m.released.notify_one();
    });
}
