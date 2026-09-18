//! Chunked work behind the parallel collection adapters.
//!
//! An adapter call cuts `[0, len)` into leaves and runs a leaf body over each
//! one, on the calling worker and on helpers drawn from the pool. Leaves are
//! claimed from one cursor, so the caller always makes progress on its own
//! and waits only for leaves a running helper has already claimed: a helper
//! still queued when the cursor runs out finds nothing and touches nothing.
//! That is what keeps a nested adapter call from starving the pool.
//!
//! The leaf layout is a function of the input length and the call's mode,
//! and the worker count only decides who runs which leaf. A reduction's
//! leaves are a fixed width, so the tree it combines has the same shape on
//! every machine and a float reduction answers the same bits at any worker
//! count.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use parking_lot::{Condvar, Mutex};

use super::panic::DeferredFault;
use super::vec::GosVec;

/// Fewest elements an elementwise adapter spreads over the pool. A shorter
/// input runs where the call was made, as its sequential twin would.
pub const ELEM_GRAIN: i64 = 4;

/// Leaves each worker's share of an elementwise walk is cut into, so a worker
/// that finishes early takes another leaf rather than idling.
const ELEM_LEAVES_PER_WORKER: i64 = 4;

/// Elements in one leaf of a reduction. A constant, never derived from the
/// worker count: the reduction tree's shape must be a function of the input
/// length alone.
pub const REDUCE_GRAIN: i64 = 1024;

/// Bytes of leaf results below which the caller moves them into place itself.
/// Moving results is a memory copy, and a copy this small finishes before a
/// helper could be started to share it.
const PAR_COPY_GRAIN: usize = 1 << 20;

/// How an adapter call cuts its input into leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Elementwise results keep their order whatever the cut, so the leaf
    /// width may follow the machine.
    Elementwise,
    /// A reduction's leaves are fixed-width so its tree is the same shape on
    /// every machine.
    Reduction,
}

impl Mode {
    /// The mode an adapter call names by its integer code.
    #[must_use]
    pub fn from_code(code: i64) -> Self {
        if code == 1 {
            Self::Reduction
        } else {
            Self::Elementwise
        }
    }
}

/// The leaves `[0, len)` is cut into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Leaves {
    len: i64,
    width: i64,
    count: i64,
}

impl Leaves {
    /// Cuts `[0, len)` for `mode` on a pool of `workers`. An empty input is
    /// one empty leaf, so the leaf body still answers a value of the right
    /// shape.
    #[must_use]
    pub fn new(len: i64, mode: Mode, workers: usize) -> Self {
        let len = len.max(0);
        let width = match mode {
            Mode::Reduction => REDUCE_GRAIN,
            Mode::Elementwise => elem_width(len, workers),
        };
        let count = if len == 0 { 1 } else { ceil_div(len, width) };
        Self { len, width, count }
    }

    /// Number of leaves.
    #[must_use]
    pub fn count(&self) -> i64 {
        self.count
    }

    /// Elements per leaf; the last leaf may hold fewer.
    #[must_use]
    pub fn width(&self) -> i64 {
        self.width
    }

    /// The half-open index range leaf `leaf` covers.
    #[must_use]
    pub fn bounds(&self, leaf: i64) -> (i64, i64) {
        let lo = (leaf * self.width).min(self.len);
        let hi = (lo + self.width).min(self.len);
        (lo, hi)
    }

    /// Helpers worth drawing from a pool of `workers` for these leaves: the
    /// caller is one worker, and a helper with no leaf to take is waste.
    #[must_use]
    pub fn helpers(&self, workers: usize) -> usize {
        let spare = workers.saturating_sub(1);
        let others = usize::try_from(self.count - 1).unwrap_or(0);
        spare.min(others)
    }
}

/// Elementwise leaf width for `len` elements on `workers` workers.
#[must_use]
pub fn elem_width(len: i64, workers: usize) -> i64 {
    let len = len.max(1);
    let workers = i64::try_from(workers).unwrap_or(i64::MAX);
    if workers <= 1 || len < ELEM_GRAIN {
        return len;
    }
    ceil_div(len, workers.saturating_mul(ELEM_LEAVES_PER_WORKER)).max(1)
}

/// `a / b` rounded up, for a non-negative `a` and a positive `b`.
fn ceil_div(a: i64, b: i64) -> i64 {
    a / b + i64::from(a % b != 0)
}

static SUBMITTED: AtomicU64 = AtomicU64::new(0);

/// Records `n` helpers handed to a pool, and arms the exit report when
/// `GOS_PAR_STATS` asks for one.
pub fn note_submitted(n: u64) {
    arm_stats_report();
    SUBMITTED.fetch_add(n, Ordering::Relaxed);
}

/// Helpers handed to a pool since start-up, on every tier.
#[must_use]
pub fn submitted() -> u64 {
    SUBMITTED.load(Ordering::Relaxed)
}

/// Arms the `GOS_PAR_STATS` exit report. Called on every adapter call, so a
/// program whose calls never reach the pool still reports its zero.
pub fn arm_stats_report() {
    #[cfg(all(any(unix, windows), not(miri)))]
    {
        static ARMED: std::sync::Once = std::sync::Once::new();
        ARMED.call_once(|| {
            if std::env::var_os("GOS_PAR_STATS").is_some() {
                // SAFETY: `report` is a plain `extern "C" fn()` that lives for
                // the whole process, which is all `atexit` requires.
                unsafe { atexit(report) };
            }
        });
    }
}

// The report runs from the C runtime's exit path. Declared directly rather
// than through `libc`, which is a unix-only dependency.
#[cfg(all(any(unix, windows), not(miri)))]
unsafe extern "C" {
    fn atexit(callback: extern "C" fn()) -> std::ffi::c_int;
}

#[cfg(all(any(unix, windows), not(miri)))]
extern "C" fn report() {
    eprintln!("par submitted: {}", submitted());
}

/// How many helpers hold a claim, and the goroutine waiting for them.
struct JobState {
    active: usize,
    // A wasm build has no goroutine to park, so nothing waits by gid there.
    #[cfg(not(target_arch = "wasm32"))]
    waiter: Option<crate::sched::task::Gid>,
}

/// One leaf's outcome, filled in by whichever worker ran it.
type OutcomeSlot<R, F> = Mutex<Option<Result<R, F>>>;

/// One adapter call's leaves, shared by the caller and its helpers.
pub struct Job<R, F> {
    leaves: Leaves,
    cursor: AtomicI64,
    /// Lowest leaf that has failed so far; leaves above it are skipped.
    lowest_failure: AtomicI64,
    outcomes: Box<[OutcomeSlot<R, F>]>,
    state: Mutex<JobState>,
    idle: Condvar,
}

impl<R: Send, F: Send> Job<R, F> {
    /// A job over `leaves` with no leaf yet claimed.
    #[must_use]
    pub fn new(leaves: Leaves) -> Arc<Self> {
        let count = usize::try_from(leaves.count()).unwrap_or(0);
        Arc::new(Self {
            leaves,
            cursor: AtomicI64::new(0),
            lowest_failure: AtomicI64::new(i64::MAX),
            outcomes: (0..count).map(|_| Mutex::new(None)).collect(),
            state: Mutex::new(JobState {
                active: 0,
                #[cfg(not(target_arch = "wasm32"))]
                waiter: None,
            }),
            idle: Condvar::new(),
        })
    }

    /// The leaves this job covers.
    #[must_use]
    pub fn leaves(&self) -> Leaves {
        self.leaves
    }

    /// Claims and runs leaves until none is left.
    fn work(&self, leaf: &mut dyn FnMut(i64, i64) -> Result<R, F>) {
        loop {
            let index = self.cursor.fetch_add(1, Ordering::SeqCst);
            if index >= self.leaves.count() || index > self.lowest_failure.load(Ordering::SeqCst) {
                return;
            }
            let (lo, hi) = self.leaves.bounds(index);
            let outcome = leaf(lo, hi);
            if outcome.is_err() {
                self.lowest_failure.fetch_min(index, Ordering::SeqCst);
            }
            let slot = usize::try_from(index).unwrap_or(0);
            *self.outcomes[slot].lock() = Some(outcome);
        }
    }

    /// Runs as a helper: counted while it holds a claim, so the caller waits
    /// for exactly the leaves helpers took.
    pub fn help(&self, mut leaf: impl FnMut(i64, i64) -> Result<R, F>) {
        self.state.lock().active += 1;
        let _leaving = HelperExit { job: self };
        self.work(&mut leaf);
    }

    /// Runs as the caller: takes leaves until none is left, then waits for
    /// the leaves helpers claimed. Answers every leaf's value in index
    /// order, or the lowest-indexed failure with the values that succeeded.
    pub fn run(
        &self,
        mut leaf: impl FnMut(i64, i64) -> Result<R, F>,
    ) -> Result<Vec<R>, (F, Vec<R>)> {
        self.work(&mut leaf);
        self.wait_for_helpers();
        let mut values = Vec::with_capacity(self.outcomes.len());
        let mut failure = None;
        for slot in &self.outcomes {
            match slot.lock().take() {
                Some(Ok(value)) => values.push(value),
                Some(Err(err)) if failure.is_none() => failure = Some(err),
                Some(Err(_)) | None => {}
            }
        }
        match failure {
            Some(err) => Err((err, values)),
            None => Ok(values),
        }
    }

    fn wait_for_helpers(&self) {
        #[cfg(not(target_arch = "wasm32"))]
        if crate::sched_global::current_gid().is_some() {
            // A goroutine parks, so its worker can run a helper queued behind
            // it. The completion check runs under the lock the last helper
            // takes to wake it, so no wake falls between check and park.
            loop {
                if self.state.lock().active == 0 {
                    return;
                }
                crate::sched_global::park(crate::sched::ParkReason::Other, |parker| {
                    let mut state = self.state.lock();
                    if state.active == 0 {
                        crate::sched_global::scheduler().unpark(parker.gid);
                    } else {
                        state.waiter = Some(parker.gid);
                    }
                });
            }
        }
        let mut state = self.state.lock();
        while state.active > 0 {
            self.idle.wait(&mut state);
        }
    }
}

/// Takes a helper off the active count however it leaves, waking the caller
/// when it was the last one.
struct HelperExit<'a, R, F> {
    job: &'a Job<R, F>,
}

impl<R, F> Drop for HelperExit<'_, R, F> {
    fn drop(&mut self) {
        let mut state = self.job.state.lock();
        state.active -= 1;
        if state.active == 0 {
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(gid) = state.waiter.take() {
                crate::sched_global::scheduler().unpark(gid);
            }
            self.job.idle.notify_all();
        }
    }
}

/// Runs `body` with its faults held rather than reported, answering the
/// fault it raised. Arena regions the body opened and never closed are
/// closed, since the unwind skipped their closing.
pub fn run_deferred<T>(body: impl FnOnce() -> T) -> Result<T, DeferredFault> {
    let depth = super::rc::region_depth();
    let _deferred = super::panic::DeferredFaults::enter();
    match std::panic::catch_unwind(AssertUnwindSafe(body)) {
        Ok(value) => Ok(value),
        Err(payload) => {
            super::rc::close_regions_above(depth);
            Err(caught_fault(payload.as_ref()))
        }
    }
}

/// The fault a caught unwind carries. A runtime panic that is not a
/// Gossamer fault still names what it said.
fn caught_fault(payload: &(dyn std::any::Any + Send)) -> DeferredFault {
    if let Some(fault) = super::panic::take_deferred_fault(payload) {
        return fault;
    }
    let text = if let Some(text) = payload.downcast_ref::<&'static str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "panic".to_string()
    };
    DeferredFault {
        text,
        trace: String::new(),
    }
}

/// Workers the compiled pool offers an adapter call made on this thread. A
/// call inside an arena region runs on its own worker: the region belongs to
/// this thread, and a helper cannot allocate into it.
fn compiled_workers() -> usize {
    if super::rc::region_is_active() {
        return 1;
    }
    #[cfg(target_arch = "wasm32")]
    {
        1
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        crate::sched_global::scheduler().worker_count().max(1)
    }
}

/// Elementwise leaf width for an input of `len` elements on the compiled pool.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_par_elem_chunk(len: i64) -> i64 {
    elem_width(len, compiled_workers())
}

/// Elements in one leaf of a reduction.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_par_reduce_grain() -> i64 {
    REDUCE_GRAIN
}

/// Helpers handed to a pool since start-up.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_par_submitted() -> i64 {
    i64::try_from(submitted()).unwrap_or(i64::MAX)
}

/// The leaf body an adapter call hands the runtime: a closure over the
/// half-open range `[lo, hi)` answering that range's results as a `Vec`.
type LeafFn = unsafe extern "C-unwind" fn(env: *const u8, lo: i64, hi: i64) -> *mut GosVec;

/// A leaf's result vector, carried between workers as an address.
#[derive(Clone, Copy)]
struct Part(usize);

/// Runs the leaf body `env` over the leaves of `[0, len)` and answers every
/// leaf's results concatenated in index order. `mode` is 0 for an
/// elementwise adapter and 1 for a reduction.
///
/// A leaf's fault is held until every leaf below it has run, then the
/// lowest-indexed one is raised here, so which fault a program reports does
/// not depend on scheduling.
///
/// # Safety
///
/// `env` must be a live closure environment whose first word is a leaf body
/// of the [`LeafFn`] shape, and every leaf must answer an owned `Vec` of one
/// element layout.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_par_run(env: *const u8, len: i64, mode: i64) -> *mut GosVec {
    // A wasm build aborts on a panic rather than unwinding, so there is no
    // fault to pass through and the call is made directly.
    #[cfg(target_arch = "wasm32")]
    {
        unsafe { par_run(env, len, mode) }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        ffi_entry_passthrough!(std::ptr::null_mut(), { unsafe { par_run(env, len, mode) } })
    }
}

unsafe fn par_run(env: *const u8, len: i64, mode: i64) -> *mut GosVec {
    arm_stats_report();
    if env.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: the caller guarantees `env` is a closure environment whose
    // first word is the leaf body.
    let code = unsafe { (env as *const usize).read() };
    // SAFETY: as above, the word is a leaf body of the `LeafFn` shape.
    let leaf_fn: LeafFn = unsafe { std::mem::transmute::<usize, LeafFn>(code) };
    let workers = compiled_workers();
    let leaves = Leaves::new(len, Mode::from_code(mode), workers);
    if leaves.count() == 1 {
        let (lo, hi) = leaves.bounds(0);
        // SAFETY: `env` and `leaf_fn` are the closure the caller handed over.
        return unsafe { leaf_fn(env, lo, hi) };
    }
    let helpers = leaves.helpers(workers);
    let env_addr = env as usize;
    if helpers > 0 {
        // Helpers read the environment's captures from other threads, so
        // every count reachable from it has to be adjusted atomically.
        // SAFETY: a closure environment is an RC allocation.
        unsafe { super::rc::gos_rt_rc_mark_shared(env.cast_mut()) };
    }
    let job = Job::<Part, DeferredFault>::new(leaves);
    let run_leaf = move |lo: i64, hi: i64| {
        run_deferred(|| {
            // SAFETY: the environment outlives every leaf: the caller waits
            // for each claimed leaf before this call returns.
            Part(unsafe { leaf_fn(env_addr as *const u8, lo, hi) } as usize)
        })
    };
    for _ in 0..helpers {
        let job = Arc::clone(&job);
        let spawned = crate::sched_global::try_spawn(Box::new(move || job.help(run_leaf)));
        if spawned.is_some() {
            note_submitted(1);
        }
    }
    match job.run(run_leaf) {
        // SAFETY: every part is an owned `Vec` a leaf answered.
        Ok(parts) => unsafe { concat_parts(&parts, helpers) },
        Err((fault, parts)) => {
            for part in parts {
                // SAFETY: an owned `Vec` a leaf answered, released once here.
                unsafe { super::map::gos_rt_vec_free(part.0 as *mut GosVec) };
            }
            super::panic::reraise_deferred_fault(&fault)
        }
    }
}

/// Moves every element of `parts` into the first non-empty one, in order,
/// and answers it. Each part is uniquely owned, so its elements move with
/// their shares and the emptied part is reclaimed without touching them. A
/// large move is shared with up to `helpers` pool workers, each copying
/// whole parts into their own window of the result.
unsafe fn concat_parts(parts: &[Part], helpers: usize) -> *mut GosVec {
    let vecs: Vec<*mut GosVec> = parts.iter().map(|part| part.0 as *mut GosVec).collect();
    let Some(&first) = vecs.first() else {
        return std::ptr::null_mut();
    };
    // SAFETY: every part is a live `Vec` a leaf answered.
    let len_of = |v: *mut GosVec| if v.is_null() { 0 } else { unsafe { (*v).len } };
    let base_index = vecs.iter().position(|&v| len_of(v) > 0).unwrap_or(0);
    let base = vecs[base_index];
    if base.is_null() {
        return first;
    }
    let total: i64 = vecs.iter().map(|&v| len_of(v)).sum();
    // SAFETY: `base` is a live, uniquely owned `Vec`.
    unsafe { super::vec::gos_rt_vec_reserve_exact(base, total) };
    // SAFETY: as above; the reservation is what the moves below write into.
    let (stride, dst) = unsafe { ((*base).elem_bytes as usize, (*base).ptr.as_ptr() as usize) };
    let mut moves: Vec<(usize, usize, usize)> = Vec::new();
    let mut offset = len_of(base) as usize;
    for (index, &part) in vecs.iter().enumerate() {
        let count = len_of(part);
        if index <= base_index || count == 0 {
            continue;
        }
        // SAFETY: a live part answered by a leaf of the same call.
        let part_ref = unsafe { &*part };
        assert_eq!(
            part_ref.elem_bytes as usize, stride,
            "parallel adapter leaves answered different element layouts"
        );
        let bytes = count as usize * stride;
        moves.push((part_ref.ptr.as_ptr() as usize, dst + offset * stride, bytes));
        offset += count as usize;
    }
    let moved_bytes: usize = moves.iter().map(|&(_, _, bytes)| bytes).sum();
    let copy = |&(from, to, bytes): &(usize, usize, usize)| {
        // SAFETY: each move reads one part's elements and writes a window of
        // the reservation no other move writes.
        unsafe {
            std::ptr::copy_nonoverlapping(from as *const u8, to as *mut u8, bytes);
        }
    };
    if helpers == 0 || moves.len() < 2 || moved_bytes < PAR_COPY_GRAIN {
        moves.iter().for_each(copy);
    } else {
        let count = i64::try_from(moves.len()).unwrap_or(i64::MAX);
        let leaves = Leaves::new(count, Mode::Elementwise, helpers + 1);
        let job = Job::<(), ()>::new(leaves);
        let moves = Arc::new(moves);
        let run_moves = {
            let moves = Arc::clone(&moves);
            move |lo: i64, hi: i64| {
                for index in lo..hi {
                    copy(&moves[usize::try_from(index).unwrap_or(0)]);
                }
                Ok(())
            }
        };
        for _ in 0..leaves.helpers(helpers + 1) {
            let job = Arc::clone(&job);
            let run_moves = run_moves.clone();
            if crate::sched_global::try_spawn(Box::new(move || job.help(run_moves))).is_some() {
                note_submitted(1);
            }
        }
        let _ = job.run(run_moves);
    }
    // SAFETY: every move has landed, so the reservation holds `total`
    // initialised elements and each moved part owns none of its own.
    unsafe {
        let dst_vec = &mut *base;
        dst_vec.len = total;
        super::vec::bump_vec_mutation_generation(dst_vec);
    }
    for (index, &part) in vecs.iter().enumerate() {
        if index == base_index || part.is_null() {
            continue;
        }
        // SAFETY: an owned part whose elements (if any) now belong to `base`.
        unsafe {
            (*part).len = 0;
            super::map::gos_rt_vec_free(part);
        }
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaves_tile_the_input_exactly() {
        for len in [0, 1, 3, 4, 5, 100, 1023, 1024, 1025, 50_000] {
            for workers in [1, 2, 4, 16] {
                for mode in [Mode::Elementwise, Mode::Reduction] {
                    let leaves = Leaves::new(len, mode, workers);
                    let mut next = 0;
                    for leaf in 0..leaves.count() {
                        let (lo, hi) = leaves.bounds(leaf);
                        assert_eq!(lo, next);
                        assert!(hi >= lo);
                        next = hi;
                    }
                    assert_eq!(next, len, "len {len} workers {workers} {mode:?}");
                }
            }
        }
    }

    #[test]
    fn a_reduction_cuts_the_same_leaves_on_every_machine() {
        for len in [0, 7, 1024, 99_999] {
            let one = Leaves::new(len, Mode::Reduction, 1);
            let many = Leaves::new(len, Mode::Reduction, 64);
            assert_eq!(one, many);
        }
    }

    #[test]
    fn a_short_elementwise_input_is_one_leaf() {
        assert_eq!(
            Leaves::new(ELEM_GRAIN - 1, Mode::Elementwise, 16).count(),
            1
        );
    }

    #[test]
    fn one_worker_draws_no_helpers() {
        let leaves = Leaves::new(100_000, Mode::Reduction, 1);
        assert!(leaves.count() > 1);
        assert_eq!(leaves.helpers(1), 0);
    }
}
