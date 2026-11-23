//! The threads parallel compilation runs on while a program is running.
//!
//! A compile inside a running program (the VM's JIT) holds the pool for as
//! long as it works, and the pool's threads exit when the last holder lets
//! go, so a program pays no idle compiler threads between compiles. The pool
//! starts only when a pass has enough bodies to share out, with a thread per
//! `BODIES_PER_THREAD` of them, so a small program compiles in line and
//! starts no thread at all. A target without threads compiles in line.

#[cfg(not(target_arch = "wasm32"))]
use std::sync::Arc;

#[cfg(not(target_arch = "wasm32"))]
use parking_lot::Mutex;
#[cfg(not(target_arch = "wasm32"))]
use rayon::ThreadPool;

/// Holders, and the pool they share while any exist.
#[cfg(not(target_arch = "wasm32"))]
static POOL: Mutex<(usize, Option<Arc<ThreadPool>>)> = Mutex::new((0, None));

/// The bodies one compile thread is given. A thread's start-up and the spin
/// rayon's idle workers take before sleeping outweigh what it saves on fewer;
/// a pass over a few thousand bodies is as fast on four threads as on 24.
#[cfg(not(target_arch = "wasm32"))]
const BODIES_PER_THREAD: usize = 512;

/// Keeps the compile pool alive until dropped.
#[derive(Debug)]
#[must_use = "the pool lives only while the guard does"]
pub struct CompileThreads(());

/// Joins the holders of the compile pool, which starts on the first pass
/// with work to share out.
pub fn hold() -> CompileThreads {
    #[cfg(not(target_arch = "wasm32"))]
    {
        POOL.lock().0 += 1;
    }
    CompileThreads(())
}

/// The pool a pass over `bodies` bodies runs on, or `None` when the pass is
/// too small to share out and runs in line on the calling thread.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn pool_for(bodies: usize) -> Option<Arc<ThreadPool>> {
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let threads = bodies.div_ceil(BODIES_PER_THREAD).min(cores);
    if threads < 2 {
        return None;
    }
    let mut pool = POOL.lock();
    if let Some(running) = &pool.1
        && running.current_num_threads() >= threads
    {
        return Some(Arc::clone(running));
    }
    let started = Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|index| format!("gos-compile-{index}"))
            .build()
            .ok()?,
    );
    // Kept for the holders' later passes; with no holder it serves this pass
    // and its threads exit when the pass lets go of it.
    if pool.0 > 0 {
        pool.1 = Some(Arc::clone(&started));
    }
    Some(started)
}

impl Drop for CompileThreads {
    fn drop(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let retired = {
                let mut pool = POOL.lock();
                pool.0 -= 1;
                if pool.0 == 0 { pool.1.take() } else { None }
            };
            // Dropped outside the lock: the pool's threads exit on their own.
            drop(retired);
        }
    }
}

/// Runs `op` on the held compile pool, or where rayon would run it when no
/// compile holds one.
pub fn install<R: Send>(op: impl FnOnce() -> R + Send) -> R {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let pool = POOL.lock().1.clone();
        if let Some(pool) = pool {
            return pool.install(op);
        }
    }
    op()
}
