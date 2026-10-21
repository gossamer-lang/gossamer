//! Coroutine stack + panic propagation regression tests.
//!
//! The crate has 3 unit tests in `lib.rs` covering the basic
//! lifecycle. None of them touch deep recursion (does the
//! guard-page trip cleanly?) or panic propagation (does
//! `panic!()` inside a goroutine surface to the caller of
//! `resume()` without poisoning the worker?). Both classes
//! have shipped past regressions in similar runtimes.
//!
//! These tests are deliberately conservative - they don't
//! attempt to exercise migration across worker threads (that's
//! `gossamer-runtime::sched`'s domain) or stack growth (the
//! coro crate uses a fixed `DefaultStack`). They pin the two
//! invariants the crate documents:
//!
//!   1. A panic on the goroutine's stack propagates to the
//!      `resume()` caller as a Rust panic - no UB, no silent
//!      swallow.
//!   2. Deep recursion that touches the guard page traps via
//!      the OS, not via undefined behaviour. We don't actually
//!      trigger the guard page here - that would terminate the
//!      test binary - but we exercise the next-best signal:
//!      large-but-bounded recursion completes cleanly with
//!      the configured stack size.

#![allow(missing_docs)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use gossamer_coro::{Goroutine, clear_current_yielder, set_current_yielder, suspend};

/// Drives a goroutine to completion, calling `resume()` until it
/// returns `true`. Returns the number of resume calls performed.
fn drive_to_done(g: &mut Goroutine) -> u32 {
    let mut count = 0;
    loop {
        set_current_yielder(g.yielder_ptr());
        let done = g.resume();
        clear_current_yielder();
        count += 1;
        if done {
            return count;
        }
    }
}

#[test]
fn panic_inside_goroutine_is_isolated_per_audit_m9() {
    // a panic inside a spawned goroutine is
    // CAUGHT inside the coroutine body so the scheduler's
    // resume call returns cleanly. The panicked flag flips for
    // observation. Pre-0.6 behaviour (panic propagated to the
    // resume caller) is intentionally inverted - it abort'd the
    // worker thread and killed sibling goroutines on it.
    let mut g = Goroutine::new(Box::new(|| {
        panic!("expected panic from goroutine");
    }));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        set_current_yielder(g.yielder_ptr());
        let _ = g.resume();
        clear_current_yielder();
    }));
    assert!(
        result.is_ok(),
        "goroutine panics must be caught inside the \
         coroutine body; the resume caller should NOT observe an unwind. \
         If this assertion ever fails, M9 has regressed."
    );
    assert!(
        gossamer_coro::any_goroutine_panicked(),
        "panicked flag must be set after a goroutine panic"
    );
}

#[test]
fn many_short_lived_goroutines_complete_serially() {
    // Spawn 1_000 short-lived goroutines back-to-back. A
    // regression in stack allocation/teardown (corosensei or
    // our wrapper) shows up as either a memory leak or a
    // crash by the 100th iteration. The test is bounded - we
    // don't run 10k here because each goroutine is a 1 MiB
    // stack `mmap`, and the test runner doesn't need that much
    // address-space churn to catch the regression class.
    let counter = Arc::new(AtomicU64::new(0));
    for _ in 0..1_000 {
        let c = Arc::clone(&counter);
        let mut g = Goroutine::new(Box::new(move || {
            c.fetch_add(1, Ordering::Relaxed);
        }));
        let resumes = drive_to_done(&mut g);
        assert_eq!(resumes, 1, "expected goroutine to finish on first resume");
    }
    assert_eq!(counter.load(Ordering::Relaxed), 1_000);
}

#[test]
fn moderate_recursion_completes_within_default_stack() {
    // 4_000 frames of trivial recursion. Each frame is small
    // (a few words for return address + scalar locals), so
    // 4_000 frames is well inside the default 1 MiB stack but
    // far enough past the previous 16 KiB clamp that the
    // older default would overflow. Catches a regression
    // where the stack-size override gets re-introduced as a
    // smaller default.
    fn recurse(n: u32, ack: &Arc<AtomicU64>) -> u32 {
        if n == 0 {
            ack.fetch_add(1, Ordering::Relaxed);
            return 0;
        }
        recurse(n - 1, ack) + 1
    }
    let observed = Arc::new(AtomicU64::new(0));
    let observed_for_main = Arc::clone(&observed);
    let mut g = Goroutine::new(Box::new(move || {
        let n = recurse(4_000, &observed_for_main);
        assert_eq!(n, 4_000);
    }));
    drive_to_done(&mut g);
    assert_eq!(observed.load(Ordering::Relaxed), 1);
}

#[test]
fn yield_loop_through_many_resumes_does_not_corrupt_state() {
    // A goroutine that yields 1_000 times before completing,
    // pushing a per-iteration value into a shared counter.
    // Past stackful-coroutine regressions (`corosensei` 0.x)
    // had a class of bug where alternate resumes would lose
    // the yielder pointer - symptom was every other yield
    // running twice or zero times. The strict equality check
    // below catches it.
    let counter = Arc::new(AtomicU64::new(0));
    let counter_for_main = Arc::clone(&counter);
    let mut g = Goroutine::new(Box::new(move || {
        for _ in 0..1_000 {
            counter_for_main.fetch_add(1, Ordering::Relaxed);
            suspend();
        }
        counter_for_main.fetch_add(1, Ordering::Relaxed);
    }));
    let resumes = drive_to_done(&mut g);
    // 1_000 yields + the post-loop final increment = 1_001
    // resumes (initial run that hits first suspend, plus 999
    // more resumes that each run one body iteration, plus one
    // more for the post-loop tail).
    assert_eq!(resumes, 1_001);
    assert_eq!(counter.load(Ordering::Relaxed), 1_001);
}

/// Address of a local inside a goroutine body that runs to completion.
fn body_stack_address() -> usize {
    let seen = Arc::new(AtomicU64::new(0));
    let seen_in_body = Arc::clone(&seen);
    let mut g = Goroutine::new(Box::new(move || {
        let addr = gossamer_coro::current_stack_ptr() as u64;
        seen_in_body.store(addr, Ordering::Release);
    }));
    drive_to_done(&mut g);
    drop(g);
    usize::try_from(seen.load(Ordering::Acquire)).expect("stack address fits usize")
}

#[test]
fn finished_goroutines_stack_serves_the_next_spawn_on_its_thread() {
    let first = body_stack_address();
    let second = body_stack_address();
    assert_eq!(first, second, "the second goroutine ran on a fresh mapping");
}

#[test]
fn a_suspended_goroutine_dropped_unfinished_releases_cleanly() {
    let mut g = Goroutine::new(Box::new(suspend));
    set_current_yielder(g.yielder_ptr());
    assert!(!g.resume());
    clear_current_yielder();
    drop(g);
    body_stack_address();
}
