//! The threads parallel compilation runs on while a program is running.
//!
//! A compile inside a running program (the VM's JIT) holds the pool for as
//! long as it works, and the pool's threads exit when the last holder lets
//! go, so a program pays no idle compiler threads between compiles. A
//! compile nothing holds the pool for (a `gos build` that exits afterwards)
//! runs on rayon's global pool. A target without threads compiles in line.

#[cfg(not(target_arch = "wasm32"))]
use std::sync::Arc;

#[cfg(not(target_arch = "wasm32"))]
use parking_lot::Mutex;
#[cfg(not(target_arch = "wasm32"))]
use rayon::ThreadPool;

/// Holders, and the pool they share while any exist.
#[cfg(not(target_arch = "wasm32"))]
static POOL: Mutex<(usize, Option<Arc<ThreadPool>>)> = Mutex::new((0, None));

/// Keeps the compile pool alive until dropped.
#[derive(Debug)]
#[must_use = "the pool lives only while the guard does"]
pub struct CompileThreads(());

/// Starts the compile pool, or joins the holders of the running one.
pub fn hold() -> CompileThreads {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut pool = POOL.lock();
        pool.0 += 1;
        if pool.1.is_none() {
            pool.1 = rayon::ThreadPoolBuilder::new()
                .thread_name(|index| format!("gos-compile-{index}"))
                .build()
                .ok()
                .map(Arc::new);
        }
    }
    CompileThreads(())
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
