//! Foreign calls under the scheduler, on one worker so no second worker can
//! hide a problem: a call that blocks hands the worker's other goroutines to
//! a fresh worker, and the `errno` each call leaves stays with its own
//! goroutine however their steps interleave.

#![cfg(unix)]

use crate::common::{TIERS, gos_run_on, stderr, stdout};

fn on_one_worker(src: &str) -> String {
    let mut answer: Option<String> = None;
    for tier in TIERS {
        let out = gos_run_on(tier, src, Some(1), &[]);
        assert!(out.status.success(), "{tier:?} failed: {}", stderr(&out));
        let text = stdout(&out);
        if let Some(first) = &answer {
            assert_eq!(&text, first, "{tier:?} answered differently");
        } else {
            answer = Some(text);
        }
    }
    answer.unwrap_or_default()
}

/// One goroutine blocks in `read` on an empty pipe; only the other
/// goroutine's `write` can end it.
const BLOCKED_READ: &str = r#"use std::{errors, time}

unsafe extern "C" {
    fn pipe(fds: &mut [i32]) -> i32
    fn read(fd: i32, buf: &mut [u8], n: usize) -> isize
    fn write(fd: i32, buf: [u8], n: usize) -> isize
}

fn relay() -> Result<(), errors::Error> {
    let mut fds = #[0i32, 0]
    unsafe { pipe(&mut fds) }
    let reader = fds[0]
    let writer = fds[1]
    cohort {
        let blocked = spawn(|| {
            let mut buf = #[0u8; 4]
            unsafe { read(reader, &mut buf, 4) }
        })
        let feeder = spawn(|| {
            time::sleep(time::Duration::from_millis(20))
            unsafe { write(writer, #[1u8, 2, 3], 3) }
        })
        println(f"fed {feeder.join()?} read {blocked.join()?}")
    }
}

fn main() {
    relay().unwrap()
}
"#;

#[test]
fn a_blocking_foreign_call_does_not_hold_back_other_goroutines() {
    assert_eq!(on_one_worker(BLOCKED_READ).trim(), "fed 3 read 3");
}

/// Two goroutines take turns on one worker, each failing a call with its own
/// `errno` and reading it back only after the other has run.
const ERRNO_PER_GOROUTINE: &str = r#"use std::{errors, ffi, time}

unsafe extern "C" {
    fn close(fd: i32) -> i32
    fn chdir(path: [u8]) -> i32
}

fn failing_close(rounds: i64) -> bool {
    let mut every = true
    for _ in 0..rounds {
        unsafe { close(-1) }
        time::sleep(time::Duration::from_millis(1))
        every = every && ffi::last_errno() == 9
    }
    every
}

fn failing_chdir(rounds: i64) -> bool {
    let path = ffi::cstring("/gossamer-no-such-directory").unwrap()
    let mut every = true
    for _ in 0..rounds {
        unsafe { chdir(path) }
        time::sleep(time::Duration::from_millis(1))
        every = every && ffi::last_errno() == 2
    }
    every
}

fn race() -> Result<(), errors::Error> {
    cohort {
        let closes = spawn(|| failing_close(40))
        let chdirs = spawn(|| failing_chdir(40))
        println(f"EBADF kept: {closes.join()?} ENOENT kept: {chdirs.join()?}")
    }
}

fn main() {
    race().unwrap()
}
"#;

#[test]
fn errno_stays_with_the_goroutine_whose_call_set_it() {
    assert_eq!(
        on_one_worker(ERRNO_PER_GOROUTINE).trim(),
        "EBADF kept: true ENOENT kept: true"
    );
}

/// Asks the platform whether `main` runs on the process main thread.
const MAIN_THREAD: &str = r#"#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn gettid() -> i32
    fn getpid() -> i32
}

#[cfg(target_os = "linux")]
fn on_main_thread() -> bool {
    unsafe { gettid() == getpid() }
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn pthread_main_np() -> i32
}

#[cfg(target_os = "macos")]
fn on_main_thread() -> bool {
    unsafe { pthread_main_np() == 1 }
}

fn main() {
    println(f"main thread: {on_main_thread()}")
}
"#;

#[test]
fn main_runs_on_the_process_main_thread_on_every_tier() {
    assert_eq!(on_one_worker(MAIN_THREAD).trim(), "main thread: true");
}
