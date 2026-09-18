//! Cross-crate context-hook bridge tests.
//!
//! Exercises the `install_ctx_hooks` /
//! `gos_rt_chan_recv_ctx_option` ABI surface that lets
//! `gossamer-runtime` observe a `gossamer-std::context::Context`
//! without depending on the std crate.
//!
//! The end-to-end "cancel a parked goroutine on a channel"
//! scenario needs the scheduler to be driving real goroutines
//! (the runtime's `chan_recv` OS-thread fallback uses a condvar
//! that the context cancel path can't reach without scheduler
//! infrastructure). What we CAN test from a Rust unit test:
//!
//! - `chan_recv_ctx_i64` returns `Some(v)` when a real send
//!   arrives, proving the wrapper threads the value through
//!   correctly (and the hooks don't corrupt anything when
//!   the recv completes the happy way).
//! - `chan_recv_ctx_i64` returns `None` immediately when the
//!   context is already cancelled at entry - the early-check
//!   short-circuit fires before any park / hook registration.

#![allow(missing_docs)]

use std::time::Duration;

use gossamer_std::context::{Context, with_cancel};

#[test]
fn chan_recv_ctx_returns_some_value_when_send_happens_before_cancel() {
    #[allow(unsafe_code, reason = "the test drives the raw channel C-ABI directly")]
    let chan: *mut u8 = unsafe { gossamer_runtime::c_abi::gos_rt_chan_new(8, 0).cast() };
    assert!(!chan.is_null(), "chan_new returned null");

    let (ctx, _cancel) = with_cancel(&Context::background());

    let chan_send = chan as usize;
    let sender = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        let val_slot: i64 = 42;
        #[allow(unsafe_code, reason = "the test drives the raw channel C-ABI directly")]
        unsafe {
            gossamer_runtime::c_abi::gos_rt_chan_send(
                chan_send as *mut _,
                std::ptr::addr_of!(val_slot).cast(),
            );
        }
    });

    let result = gossamer_std::context::chan_recv_ctx_i64(chan, &ctx);
    assert_eq!(result, Some(42), "expected Some(42), got {result:?}");
    sender.join().expect("sender thread");

    #[allow(unsafe_code, reason = "the test drives the raw channel C-ABI directly")]
    unsafe {
        gossamer_runtime::c_abi::gos_rt_chan_drop(chan.cast());
    }
}

#[test]
fn chan_recv_ctx_returns_none_when_context_is_already_cancelled_at_entry() {
    #[allow(unsafe_code, reason = "the test drives the raw channel C-ABI directly")]
    let chan: *mut u8 = unsafe { gossamer_runtime::c_abi::gos_rt_chan_new(8, 0).cast() };
    assert!(!chan.is_null(), "chan_new returned null");

    let (ctx, cancel) = with_cancel(&Context::background());
    cancel.cancel_with("pre-cancel");

    // Recv on an already-cancelled context should short-circuit
    // through the `is_cancelled` hook check at the top of
    // `gos_rt_chan_recv_ctx_option`, returning None without
    // ever parking on the channel.
    let result = gossamer_std::context::chan_recv_ctx_i64(chan, &ctx);
    assert_eq!(
        result, None,
        "pre-cancelled context must short-circuit to None, got {result:?}",
    );

    #[allow(unsafe_code, reason = "the test drives the raw channel C-ABI directly")]
    unsafe {
        gossamer_runtime::c_abi::gos_rt_chan_drop(chan.cast());
    }
}

#[test]
fn chan_recv_ctx_returns_none_when_cancel_fires_mid_recv_from_os_thread() {
    // Confirms the OS-thread condvar path observes context
    // cancellation. Without the bounded-timeout cancel poll
    // in `gos_rt_chan_recv_ctx_option`, this test would hang
    // forever - the recv would sit in `not_empty.wait()` with
    // no sender ever arriving and no goroutine-side unpark to
    // route the cancel through.
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};

    #[allow(unsafe_code, reason = "the test drives the raw channel C-ABI directly")]
    let chan: *mut u8 = unsafe { gossamer_runtime::c_abi::gos_rt_chan_new(8, 0).cast() };
    assert!(!chan.is_null());

    let (ctx, cancel) = with_cancel(&Context::background());
    let chan_addr = chan as usize;
    let ctx_clone = ctx.clone();
    let observed = Arc::new(AtomicI64::new(i64::MIN));
    let observed_w = Arc::clone(&observed);

    let handle = std::thread::spawn(move || {
        let chan = chan_addr as *mut u8;
        let value = gossamer_std::context::chan_recv_ctx_i64(chan, &ctx_clone);
        observed_w.store(value.unwrap_or(-1), Ordering::Release);
    });

    std::thread::sleep(Duration::from_millis(50));
    cancel.cancel_with("test cancel");

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline && observed.load(Ordering::Acquire) == i64::MIN {
        std::thread::sleep(Duration::from_millis(20));
    }
    let got = observed.load(Ordering::Acquire);
    assert_eq!(
        got, -1,
        "OS-thread recv must observe cancel and return None (-1 sentinel); got {got}",
    );
    handle.join().expect("recv thread");
}

#[test]
fn chan_recv_ctx_returns_none_when_channel_is_closed_with_no_value() {
    #[allow(unsafe_code, reason = "the test drives the raw channel C-ABI directly")]
    let chan: *mut u8 = unsafe { gossamer_runtime::c_abi::gos_rt_chan_new(8, 0).cast() };
    assert!(!chan.is_null(), "chan_new returned null");

    // Close the channel without sending anything.
    #[allow(unsafe_code)]
    unsafe {
        gossamer_runtime::c_abi::gos_rt_chan_close(chan.cast());
    }

    let ctx = Context::background();
    let result = gossamer_std::context::chan_recv_ctx_i64(chan, &ctx);
    assert_eq!(result, None, "closed channel must yield None");
    // Skip chan_drop here - it closes again and aborts. The
    // channel allocation leaks for the test process lifetime,
    // which is fine for a single-test process.
}
