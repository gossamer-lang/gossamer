//! Chunked work runs on the pool, reports the lowest failing chunk, runs
//! inline below the grain, and does not starve when nested.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc;

use gossamer_runtime::c_abi::par::{
    ELEM_GRAIN, Job, Leaves, Mode, REDUCE_GRAIN, gos_rt_par_reduce_grain,
};

/// Runs `job` with `helpers` OS threads helping the calling thread.
fn run_with_threads<R: Send + 'static, F: Send + 'static>(
    job: &Arc<Job<R, F>>,
    helpers: usize,
    leaf: impl Fn(i64, i64) -> Result<R, F> + Send + Sync + Clone + 'static,
) -> Result<Vec<R>, (F, Vec<R>)> {
    let handles: Vec<_> = (0..helpers)
        .map(|_| {
            let job = Arc::clone(job);
            let leaf = leaf.clone();
            std::thread::spawn(move || job.help(leaf))
        })
        .collect();
    let outcome = job.run(leaf);
    for handle in handles {
        handle.join().expect("a helper thread ran to completion");
    }
    outcome
}

#[test]
fn every_chunk_runs_and_the_ranges_tile_the_input_exactly() {
    for (len, mode) in [
        (100_000, Mode::Reduction),
        (50_000, Mode::Elementwise),
        (7, Mode::Elementwise),
    ] {
        let leaves = Leaves::new(len, mode, 8);
        let job = Job::<(i64, i64), ()>::new(leaves);
        let covered = Arc::new(AtomicI64::new(0));
        let counter = Arc::clone(&covered);
        let ranges = run_with_threads(&job, 3, move |lo, hi| {
            counter.fetch_add(hi - lo, Ordering::Relaxed);
            Ok((lo, hi))
        })
        .expect("no leaf fails");
        assert_eq!(covered.load(Ordering::Relaxed), len);
        let mut next = 0;
        for (lo, hi) in ranges {
            assert_eq!(lo, next, "leaves come back in index order");
            next = hi;
        }
        assert_eq!(next, len);
    }
}

#[test]
fn the_lowest_failing_chunk_wins_regardless_of_completion_order() {
    // Chunk 1 fails first; chunk 0 fails only once chunk 1 has, so chunk 0
    // finishes last. The reported failure must still be chunk 0's: a
    // program's failure must not depend on scheduling.
    let leaves = Leaves::new(4 * REDUCE_GRAIN, Mode::Reduction, 4);
    let job = Job::<i64, String>::new(leaves);
    let (signal, wait) = mpsc::channel::<()>();
    let wait = Arc::new(parking_lot::Mutex::new(wait));
    let outcome = run_with_threads(&job, 1, move |lo, _hi| {
        let index = lo / REDUCE_GRAIN;
        match index {
            0 => {
                let _ = wait.lock().recv();
                Err("chunk 0".to_string())
            }
            1 => {
                let _ = signal.send(());
                Err("chunk 1".to_string())
            }
            _ => Ok(index),
        }
    });
    let (failure, _) = outcome.expect_err("two chunks fail");
    assert_eq!(failure, "chunk 0");
}

#[test]
fn a_short_input_runs_inline_with_no_fork() {
    // Below the grain there is one leaf and nothing to hand a helper, so
    // `par_map` on three elements costs what `map` costs.
    let leaves = Leaves::new(ELEM_GRAIN - 1, Mode::Elementwise, 64);
    assert_eq!(leaves.count(), 1);
    assert_eq!(leaves.helpers(64), 0);
}

#[test]
fn a_single_worker_pool_runs_every_chunk_inline() {
    // With one worker a helper could only queue behind the caller, so none
    // is drawn and the caller runs every leaf itself.
    let leaves = Leaves::new(100 * REDUCE_GRAIN, Mode::Reduction, 1);
    assert!(leaves.count() > 1);
    assert_eq!(leaves.helpers(1), 0);
    let job = Job::<i64, ()>::new(leaves);
    let sums = job
        .run(|lo, hi| Ok((lo..hi).sum::<i64>()))
        .expect("no leaf fails");
    assert_eq!(
        sums.iter().sum::<i64>(),
        (0..100 * REDUCE_GRAIN).sum::<i64>()
    );
}

#[test]
fn nested_chunked_work_does_not_deadlock_the_pool() {
    // Every outer leaf runs a job of its own on the scheduler's pool. A
    // caller waits only for leaves a running helper claimed, so a helper
    // still queued behind a waiting caller holds nothing up.
    let outer = Leaves::new(64, Mode::Elementwise, 8);
    let job = Job::<i64, ()>::new(outer);
    let totals = run_with_threads(&job, 4, |lo, hi| {
        let mut total = 0;
        for _ in lo..hi {
            let inner = Job::<i64, ()>::new(Leaves::new(8 * REDUCE_GRAIN, Mode::Reduction, 8));
            for _ in 0..inner.leaves().helpers(8) {
                let helper = Arc::clone(&inner);
                let _ = gossamer_runtime::sched_global::try_spawn(Box::new(move || {
                    helper.help(|lo, hi| Ok((lo..hi).sum::<i64>()));
                }));
            }
            let sums = inner
                .run(|lo, hi| Ok((lo..hi).sum::<i64>()))
                .expect("no leaf fails");
            total += sums.iter().sum::<i64>();
        }
        Ok(total)
    })
    .expect("no leaf fails");
    let per_inner = (0..8 * REDUCE_GRAIN).sum::<i64>();
    assert_eq!(totals.iter().sum::<i64>(), 64 * per_inner);
}

#[test]
fn the_reduce_grain_does_not_move_with_the_worker_count() {
    assert_eq!(gos_rt_par_reduce_grain(), REDUCE_GRAIN);
    for workers in [1, 2, 16, 256] {
        let leaves = Leaves::new(10 * REDUCE_GRAIN + 3, Mode::Reduction, workers);
        assert_eq!(
            leaves.width(),
            REDUCE_GRAIN,
            "the reduction grain must not follow the machine"
        );
    }
}
