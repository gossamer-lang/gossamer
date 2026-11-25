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
    /// Counts acquisitions, so a guard releases only the hold it took.
    generation: AtomicI64,
    /// Whether [`Self::poison`] holds a cause, read on every acquisition so
    /// a healthy lock costs one load.
    is_poisoned: std::sync::atomic::AtomicBool,
    /// The panic a holder unwound with while releasing the lock. Every later
    /// `lock` panics naming it, since the state the lock guards may be half
    /// written.
    poison: parking_lot::Mutex<Option<String>>,
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
            generation: AtomicI64::new(0),
            is_poisoned: std::sync::atomic::AtomicBool::new(false),
            poison: parking_lot::Mutex::new(None),
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
    // `main` stops waiting before its own hold can be seen, so a deadlock
    // check never reads the lock `main` now holds as the one it waits for.
    crate::sched_global::end_main_wait();
    drop(state);
}

/// The panic text a `lock` of a poisoned mutex raises.
pub fn poisoned_message(cause: &str) -> String {
    format!("Mutex poisoned: a goroutine panicked while holding it: {cause}")
}

/// The panic text an `unlock` of a mutex the caller does not hold raises.
pub const NOT_HELD_MESSAGE: &str = "Mutex::unlock of a lock this goroutine does not hold";

/// Takes `m` for the calling goroutine and answers the acquisition's
/// generation; a poisoned mutex is released again and the call panics.
fn take(m: &GosMutex) -> i64 {
    acquire(m);
    if m.is_poisoned.load(Ordering::Acquire) {
        let cause = m.poison.lock().clone().unwrap_or_default();
        release(m);
        crate::c_abi::panic::panic_text(&poisoned_message(&cause));
    }
    m.owner
        .store(i64::from(crate::race::current_gid()), Ordering::Release);
    let from = m.last_unlocker.load(Ordering::Acquire);
    if from >= 0 {
        crate::race::record_sync(u32::try_from(from).unwrap_or(0), crate::race::current_gid());
    }
    m.generation.fetch_add(1, Ordering::AcqRel) + 1
}

/// Frees `m` and wakes the next waiter.
fn release(m: &GosMutex) {
    let mut state = m.state.lock();
    state.locked = false;
    let next = state.parked.pop_front();
    drop(state);
    if let Some(gid) = next {
        crate::sched_global::unpark(gid);
    }
    m.released.notify_one();
}

/// Gives up the calling goroutine's hold on `m`, poisoning it when the
/// release runs because a panic is unwinding.
fn give_up(m: &GosMutex, me: i64) {
    if let Some(cause) = crate::c_abi::panic::unwinding_panic() {
        *m.poison.lock() = Some(cause);
        m.is_poisoned.store(true, Ordering::Release);
    }
    m.owner.store(-1, Ordering::Release);
    m.last_unlocker.store(me, Ordering::Release);
    release(m);
}

// The lock shims are bare (no `ffi_entry!`): their one fault is a Gossamer
// panic - a poisoned lock, or an unlock of a lock the caller does not hold -
// which unwinds to the goroutine as any panic in its code does.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_mutex_lock(m: *mut GosMutex) {
    if m.is_null() {
        return;
    }
    // SAFETY: `m` is a handle from compiled code, checked non-null above and live for the whole call.
    take(unsafe { &*m });
}

#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_mutex_unlock(m: *mut GosMutex) {
    if m.is_null() {
        return;
    }
    // SAFETY: `m` is a handle from compiled code, checked non-null above and live for the whole call.
    let m = unsafe { &*m };
    let me = i64::from(crate::race::current_gid());
    if m.owner.load(Ordering::Acquire) != me {
        crate::c_abi::panic::panic_text(NOT_HELD_MESSAGE);
    }
    give_up(m, me);
}

/// `m.lock()` bound by `let`: takes the lock and answers the acquisition's
/// generation, which the guard releases by.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_mutex_acquire(m: *mut GosMutex) -> i64 {
    if m.is_null() {
        return 0;
    }
    // SAFETY: `m` is a handle from compiled code, checked non-null above and live for the whole call.
    take(unsafe { &*m })
}

/// A guard's release: frees `m` when the calling goroutine still holds the
/// acquisition `generation` named, and does nothing once that hold has been
/// given up.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_mutex_release(m: *mut GosMutex, generation: i64) {
    if m.is_null() {
        return;
    }
    // SAFETY: `m` is a handle from compiled code, checked non-null above and live for the whole call.
    let m = unsafe { &*m };
    let me = i64::from(crate::race::current_gid());
    if m.owner.load(Ordering::Acquire) == me && m.generation.load(Ordering::Acquire) == generation {
        give_up(m, me);
    }
}
