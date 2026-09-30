//! Audit M19 (0.6.0): callback handle table round-trip.

#![allow(
    clippy::cast_ptr_alignment,
    reason = "Counter is `#[repr(align(8))]`; the cast is safe"
)]

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Barrier, mpsc};
use std::time::Duration;

// Pinned 8-byte-aligned box so the `*const u8 → AtomicU32` cast
// in the callback is safe under stricter alignment lints.
#[repr(align(8))]
struct Counter(AtomicU32);

extern "C" fn callback_fn(
    ctx: *const u8,
    _args: *const u8,
    _args_len: u32,
    _result: *mut u8,
) -> i32 {
    // ctx is a `*const Counter` we passed at register time.
    // SAFETY: the registering thread is alive for the duration
    // of the test; the AtomicU32 stays on its stack.
    let counter = unsafe { &*ctx.cast::<Counter>() };
    counter.0.fetch_add(1, Ordering::AcqRel);
    0
}

#[test]
fn callback_register_invoke_unregister_round_trip() {
    let counter = Counter(AtomicU32::new(0));
    let ctx: *const u8 = std::ptr::from_ref(&counter).cast::<u8>();

    let handle = gossamer_runtime::c_abi::gos_rt_callback_register(ctx, callback_fn);
    assert!(handle != 0, "register must return a non-zero handle");

    // First invocation: callback fires, counter increments to 1.
    let mut result = [0u8; 16];
    // SAFETY: every pointer argument is a value this test built above and still holds live; a
    // null one is accepted by the callee.
    let rc = unsafe {
        gossamer_runtime::c_abi::gos_rt_callback_invoke(
            handle,
            std::ptr::null(),
            0,
            result.as_mut_ptr(),
        )
    };
    assert_eq!(rc, 0, "callback returned 0 on success");
    assert_eq!(counter.0.load(Ordering::Acquire), 1);

    // Unregister: subsequent invocations return -1.
    gossamer_runtime::c_abi::gos_rt_callback_unregister(handle);
    // SAFETY: every pointer argument is a value this test built above and still holds live; a
    // null one is accepted by the callee.
    let rc2 = unsafe {
        gossamer_runtime::c_abi::gos_rt_callback_invoke(
            handle,
            std::ptr::null(),
            0,
            result.as_mut_ptr(),
        )
    };
    assert_eq!(rc2, -1, "invoke must return -1 for an unregistered handle");
    // Counter unchanged after unregister.
    assert_eq!(counter.0.load(Ordering::Acquire), 1);
}

#[test]
fn callback_invoke_unknown_handle_returns_minus_one() {
    let mut result = [0u8; 16];
    // SAFETY: every pointer argument is a value this test built above and still holds live; a
    // null one is accepted by the callee.
    let rc = unsafe {
        gossamer_runtime::c_abi::gos_rt_callback_invoke(
            0xDEAD_BEEF_DEAD_BEEF,
            std::ptr::null(),
            0,
            result.as_mut_ptr(),
        )
    };
    assert_eq!(rc, -1);
}

#[test]
fn callback_invoke_zero_handle_short_circuits() {
    // SAFETY: every pointer argument is a value this test built above and still holds live; a
    // null one is accepted by the callee.
    let rc = unsafe {
        gossamer_runtime::c_abi::gos_rt_callback_invoke(
            0,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, -1);
}

struct BlockingContext {
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
}

extern "C" fn blocking_callback(
    ctx: *const u8,
    _args: *const u8,
    _args_len: u32,
    _result: *mut u8,
) -> i32 {
    // SAFETY: every pointer argument is a value this test built above and still holds live; a
    // null one is accepted by the callee.
    let ctx = unsafe { &*ctx.cast::<BlockingContext>() };
    ctx.entered.wait();
    ctx.release.wait();
    0
}

#[test]
fn unregister_waits_for_an_in_flight_callback() {
    let context = Box::new(BlockingContext {
        entered: Arc::new(Barrier::new(2)),
        release: Arc::new(Barrier::new(2)),
    });
    let handle = gossamer_runtime::c_abi::gos_rt_callback_register(
        std::ptr::from_ref(context.as_ref()).cast(),
        blocking_callback,
    );
    let invoke_handle = handle;
    // SAFETY: every pointer argument is a value this test built above and still holds live; a
    // null one is accepted by the callee.
    let invoke = std::thread::spawn(move || unsafe {
        gossamer_runtime::c_abi::gos_rt_callback_invoke(
            invoke_handle,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
        )
    });
    context.entered.wait();

    let (done_tx, done_rx) = mpsc::channel();
    let unregister = std::thread::spawn(move || {
        gossamer_runtime::c_abi::gos_rt_callback_unregister(handle);
        done_tx.send(()).unwrap();
    });
    assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
    context.release.wait();
    assert_eq!(invoke.join().unwrap(), 0);
    done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    unregister.join().unwrap();
    drop(context);
}

#[repr(align(8))]
struct SelfUnregisterContext(std::sync::atomic::AtomicU64);

extern "C" fn self_unregister_callback(
    ctx: *const u8,
    _args: *const u8,
    _args_len: u32,
    _result: *mut u8,
) -> i32 {
    // SAFETY: every pointer argument is a value this test built above and still holds live; a
    // null one is accepted by the callee.
    let ctx = unsafe { &*ctx.cast::<SelfUnregisterContext>() };
    gossamer_runtime::c_abi::gos_rt_callback_unregister(ctx.0.load(Ordering::Acquire));
    0
}

#[test]
fn callback_can_unregister_itself_without_deadlocking() {
    let context = SelfUnregisterContext(std::sync::atomic::AtomicU64::new(0));
    let handle = gossamer_runtime::c_abi::gos_rt_callback_register(
        std::ptr::from_ref(&context).cast(),
        self_unregister_callback,
    );
    context.0.store(handle, Ordering::Release);
    assert_eq!(
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            gossamer_runtime::c_abi::gos_rt_callback_invoke(
                handle,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
            )
        },
        0
    );
    assert_eq!(
        // SAFETY: every pointer argument is a value this test built above and still holds live; a
        // null one is accepted by the callee.
        unsafe {
            gossamer_runtime::c_abi::gos_rt_callback_invoke(
                handle,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
            )
        },
        -1
    );
}
