//! Positional file reads from a goroutine run on the goroutine's own worker:
//! they start no blocking-pool thread and no thread per call.

use std::ffi::CString;

use gossamer_runtime::blocking_pool::{DEFAULT_BLOCKING_THREAD_CAP, blocking_threads};
use gossamer_runtime::c_abi::{
    gos_rt_fs_file_close, gos_rt_fs_file_open, gos_rt_fs_file_read_at, gos_rt_result_disc,
    gos_rt_result_payload, gos_rt_vec_free,
};
use gossamer_runtime::sched_global;

const READS: usize = 10_000;
const WORKERS: usize = 2;

/// Threads in this process, from `/proc`; `None` where there is no `/proc`.
fn thread_count() -> Option<usize> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("Threads:"))
        .and_then(|n| n.trim().parse().ok())
}

#[test]
fn goroutine_read_at_keeps_the_thread_count_bounded() {
    // Thread counts come from `/proc`, which only Linux provides; elsewhere
    // the test has nothing to measure and passes vacuously.
    let Some(before) = thread_count() else {
        return;
    };
    // SAFETY: set before this test binary's first scheduler call.
    unsafe { std::env::set_var("GOSSAMER_MAX_PROCS", WORKERS.to_string()) };
    let path = std::env::temp_dir().join(format!("gos-positional-threads-{}", std::process::id()));
    std::fs::write(&path, vec![7u8; 8192]).expect("write scratch file");
    let cpath = CString::new(path.to_string_lossy().as_bytes()).expect("path has no NUL");

    let (done_tx, done_rx) = std::sync::mpsc::channel();
    sched_global::spawn(Box::new(move || {
        let mut peak = 0;
        // SAFETY: the C string outlives every call, each answered vector is
        // freed once checked, and the handle is closed after the loop.
        let (ok, pool_grew) = unsafe {
            let opened = gos_rt_fs_file_open(cpath.as_ptr());
            assert_eq!(gos_rt_result_disc(opened), 0);
            let h = gos_rt_result_payload(opened);
            let pool_after_open = blocking_threads();
            let mut ok = true;
            for i in 0..READS {
                let read = gos_rt_fs_file_read_at(h, 64, (i % 8000) as i64);
                ok &= gos_rt_result_disc(read) == 0;
                let v = gos_rt_result_payload(read) as *mut gossamer_runtime::c_abi::GosVec;
                ok &= (*v).len == 64;
                gos_rt_vec_free(v);
                if i % 500 == 0 {
                    peak = peak.max(thread_count().unwrap_or(0));
                }
            }
            let grown = blocking_threads() > pool_after_open;
            gos_rt_fs_file_close(h);
            (ok, grown)
        };
        let _ = done_tx.send((ok, pool_grew, peak));
    }));
    let (ok, pool_grew, peak) = done_rx.recv().expect("goroutine reports");
    let _ = std::fs::remove_file(&path);
    assert!(ok, "a positional read failed");
    assert!(!pool_grew, "positional reads started blocking-pool threads");
    assert!(
        peak <= before + WORKERS + DEFAULT_BLOCKING_THREAD_CAP,
        "{peak} threads during {READS} reads, from {before}"
    );
}
