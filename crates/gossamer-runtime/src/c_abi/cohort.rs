#![allow(clippy::missing_safety_doc)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::cast_possible_truncation)]

//! Runtime support for `cohort { }` - structured concurrency on the
//! compiled tiers.
//!
//! A cohort owns the goroutines `spawn`ed while it is the running
//! goroutine's current cohort. The block cannot be left until every one
//! of them has finished, which is the guarantee the construct exists to
//! provide: a child cannot outlive the block that started it.
//!
//! State lives in a process-global registry keyed by a dense `i64` id, so
//! a cohort opened on one carrier resolves from the worker a child lands
//! on. The current cohort is per goroutine rather than per thread: a
//! coroutine migrates between carriers, so the mapping is keyed by `Gid`
//! and falls back to a thread-local for callers that are not goroutines
//! (the main thread, and any plain OS thread).
//!
//! Results are positional. A child is assigned its index when it is
//! registered, at the `spawn` call, and the error a fail-fast cohort
//! reports is the lowest-index failure rather than the first to arrive,
//! so a cohort's answer does not depend on completion order, worker
//! count, or which tier is running it.
//!
//! Cancellation is cooperative and observed as an operation's ordinary
//! "nothing more is coming" answer: a cancelled `recv` reports the same
//! `None` a closed channel does. Nothing is killed, so a cancelled
//! child leaves through its own normal exit path and its `defer` frames
//! and RC releases run in order.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use parking_lot::{Condvar, Mutex};

use crate::platform::Instant;
use crate::sched::Gid;

/// Completion policy, as spelled by `Policy::` in source.
pub const POLICY_FAIL_FAST: i64 = 0;
pub const POLICY_COLLECT_ALL: i64 = 1;
pub const POLICY_RACE: i64 = 2;
/// The first failure becomes the block's `Err` - the default.
pub const ON_ERROR_PROPAGATE: i64 = 0;
/// Every failure is named on stderr as it happens; the block answers `Ok`.
pub const ON_ERROR_LOG: i64 = 1;
/// A failure changes nothing the block answers. It is still counted, still
/// drained, and still named by the drain report at exit.
pub const ON_ERROR_IGNORE: i64 = 2;

/// Execution context for children, as spelled by `Context::` in source.
pub const ISOLATION_SHARED: i64 = 0;
pub const ISOLATION_THREAD: i64 = 1;

/// Whether any cohort has ever been opened in this process. Every
/// cancellation point consults the cohort state, so the common case -
/// a program that uses no cohort at all - must cost one relaxed load.
static ANY_COHORT: AtomicBool = AtomicBool::new(false);

/// Whether this process has opened a cohort. Cancellation points call
/// this before doing any further cohort work.
#[inline]
pub fn any_cohort_live() -> bool {
    ANY_COHORT.load(Ordering::Relaxed)
}

/// Cohorts currently in the cancelled state. `main` runs inside a root
/// cohort, so `any_cohort_live` is true for every program and cannot be
/// the fast path any more; this counter is, because cancellation is rare
/// and a cancellation point that reads zero here has nothing to check.
static CANCELLED_COHORTS: AtomicI64 = AtomicI64::new(0);

/// One child's failure, and whether any joiner ever read it.
struct ChildFailure {
    index: i64,
    message: String,
    /// Set when the child's join handle was joined, so the failure
    /// reached the program rather than vanishing.
    observed: bool,
}

/// A child that registered and has not left. The reason is the label the
/// spawn carried, empty when it carried none: a report names the task a
/// caller wrote rather than only the slot it took.
#[derive(Clone)]
struct LiveChild {
    index: i64,
    reason: String,
}

/// Renders live children as a report reads them: the spawn index, and the
/// spawn's own label in quotes when it carried one.
fn render_live(live: &[LiveChild]) -> String {
    live.iter()
        .map(|child| {
            if child.reason.is_empty() {
                child.index.to_string()
            } else {
                format!("{} {:?}", child.index, child.reason)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

struct CohortState {
    next_index: i64,
    outstanding: i64,
    /// Failures by child index; the lowest index is the reported one.
    failures: Vec<ChildFailure>,
    /// Children that completed without failing.
    successes: i64,
    /// The children that registered and have not left. A drain that gives
    /// up names these, so an unfinished child is identified - by its spawn
    /// index, and by the spawn's own `reason:` label when it carried one -
    /// rather than only counted.
    live: Vec<LiveChild>,
    joined: bool,
    /// Set when the cohort's own deadline fired.
    timed_out: bool,
}

struct Cohort {
    /// Enclosing cohort on the goroutine that opened this one, or 0.
    parent: i64,
    policy: i64,
    isolation: i64,
    /// What the cohort does with a child's failure. Never makes a child
    /// unaccountable: an ignored failure is still counted and still drained.
    on_error: i64,
    /// True when the cohort's children are exempt from cancellation.
    uncancellable: bool,
    /// Milliseconds the drain waits, or 0 for "as long as it takes".
    drain_ms: i64,
    cancelled: AtomicBool,
    state: Mutex<CohortState>,
    /// Signalled whenever `outstanding` or the failure set changes.
    progress: Condvar,
    /// Goroutines parked waiting for this cohort to drain.
    joiners: Mutex<Vec<Gid>>,
    /// Goroutines parked inside a cancellation point under this cohort.
    /// Cancelling drains the list and unparks each, so every one of them
    /// re-checks its own condition.
    waiters: Mutex<Vec<Gid>>,
    /// Cohorts opened under this one, still live. A goroutine parked
    /// inside a nested cohort is on that cohort's waiter list, so
    /// cancelling reaches it by walking down this edge.
    children: Mutex<Vec<i64>>,
}

static COHORTS: LazyLock<Mutex<HashMap<i64, Arc<Cohort>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_ID: AtomicI64 = AtomicI64::new(1);

/// Current cohort of each goroutine, keyed by `Gid`.
static CURRENT_BY_GID: LazyLock<Mutex<HashMap<u32, i64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

thread_local! {
    /// Current cohort for a caller that is not a scheduler goroutine -
    /// the main thread, and any isolated child running on its own thread.
    static CURRENT_ON_THREAD: std::cell::Cell<i64> = const { std::cell::Cell::new(0) };
}

fn cohort_at(id: i64) -> Option<Arc<Cohort>> {
    if id == 0 {
        return None;
    }
    COHORTS.lock().get(&id).cloned()
}

/// The running goroutine's current cohort id, or 0.
pub fn current_cohort() -> i64 {
    if !any_cohort_live() {
        return 0;
    }
    match crate::sched_global::current_gid() {
        Some(gid) => CURRENT_BY_GID.lock().get(&gid.0).copied().unwrap_or(0),
        None => CURRENT_ON_THREAD.with(std::cell::Cell::get),
    }
}

fn set_current_cohort(id: i64) {
    match crate::sched_global::current_gid() {
        Some(gid) => {
            let mut map = CURRENT_BY_GID.lock();
            if id == 0 {
                map.remove(&gid.0);
            } else {
                map.insert(gid.0, id);
            }
        }
        None => CURRENT_ON_THREAD.with(|cell| cell.set(id)),
    }
}

/// Whether `id` or any enclosing cohort is cancelled. The chain is
/// walked iteratively so nesting depth costs heap rather than frames.
fn chain_is_cancelled(id: i64) -> bool {
    let mut current = id;
    while current != 0 {
        let Some(node) = cohort_at(current) else {
            // A retired cohort has already been joined; nothing under it
            // has further work to do.
            return true;
        };
        if node.cancelled.load(Ordering::Acquire) {
            return true;
        }
        current = node.parent;
    }
    false
}

/// Whether the running goroutine's cohort chain is cancelled. This is
/// the predicate every cancellation point consults.
pub fn current_is_cancelled() -> bool {
    if CANCELLED_COHORTS.load(Ordering::Relaxed) == 0 {
        return false;
    }
    let id = current_cohort();
    id != 0 && chain_is_cancelled(id)
}

/// Registers `gid` as parked under the running goroutine's cohort, so
/// cancelling it wakes the goroutine to re-check its condition. Returns
/// the cohort the registration landed on, for the matching deregister.
///
/// A cancellation point checks the cohort before it parks, and this
/// registration happens after that check. Cancelling in the gap between
/// the two would find no waiter on record and wake nobody, leaving the
/// goroutine parked on a condition that has already been decided - so a
/// cancellation observed here unparks the goroutine on the spot. The
/// scheduler records a wake that arrives before the coroutine has
/// actually suspended, which is the same guarantee `select` relies on
/// when an arm becomes ready while it is registering.
pub fn register_waiter(gid: Gid) -> i64 {
    let id = current_cohort();
    if let Some(node) = cohort_at(id) {
        node.waiters.lock().push(gid);
    }
    if id != 0 && chain_is_cancelled(id) {
        crate::sched_global::scheduler().unpark(gid);
    }
    id
}

/// Drops `gid` from `id`'s parked set.
pub fn deregister_waiter(id: i64, gid: Gid) {
    if let Some(node) = cohort_at(id) {
        node.waiters.lock().retain(|x| *x != gid);
    }
}

/// Marks `id` cancelled and wakes everything waiting under it: the
/// goroutines parked at a cancellation point, any joiner, and the same
/// again for every cohort nested inside it.
///
/// A goroutine parked inside a nested cohort is registered on that
/// cohort's waiter list, and a parked goroutine cannot consult the
/// parent chain on its own, so the wake has to travel down every edge
/// rather than rely on the walk each cancellation point does before it
/// parks. The descendants are visited iteratively, so nesting depth
/// costs heap rather than frames.
fn cancel(id: i64) {
    let mut pending = vec![id];
    while let Some(current) = pending.pop() {
        let Some(node) = cohort_at(current) else {
            continue;
        };
        // An exempt cohort is not cancelled, and neither is anything under
        // it: `cancellable: false` is what a shielded region asks for, and a
        // shield that let cancellation through its own children would be no
        // shield at all. Its accounting is unaffected - it still drains and
        // its failures are still reported.
        if node.uncancellable {
            continue;
        }
        {
            // The flag changes and the condvar wake are issued under the
            // lock a joining OS thread holds while it tests `outstanding`,
            // so a cancellation landing between one waiter's test and its
            // wait still reaches it.
            let _state = node.state.lock();
            if node.cancelled.swap(true, Ordering::AcqRel) {
                continue;
            }
            CANCELLED_COHORTS.fetch_add(1, Ordering::AcqRel);
            node.progress.notify_all();
        }
        for gid in std::mem::take(&mut *node.waiters.lock()) {
            crate::sched_global::scheduler().unpark(gid);
        }
        wake_joiners(&node);
        pending.extend(node.children.lock().iter().copied());
    }
}

fn wake_joiners(node: &Cohort) {
    for gid in std::mem::take(&mut *node.joiners.lock()) {
        crate::sched_global::scheduler().unpark(gid);
    }
}

/// Opens a cohort on the running goroutine and makes it current.
fn push(
    policy: i64,
    timeout_ms: i64,
    isolation: i64,
    on_error: i64,
    uncancellable: i64,
    drain_ms: i64,
) -> i64 {
    ANY_COHORT.store(true, Ordering::Relaxed);
    let parent = current_cohort();
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let node = Arc::new(Cohort {
        parent,
        policy,
        isolation,
        on_error,
        uncancellable: uncancellable != 0,
        drain_ms,
        cancelled: AtomicBool::new(false),
        state: Mutex::new(CohortState {
            next_index: 0,
            outstanding: 0,
            failures: Vec::new(),
            successes: 0,
            live: Vec::new(),
            joined: false,
            timed_out: false,
        }),
        progress: Condvar::new(),
        joiners: Mutex::new(Vec::new()),
        waiters: Mutex::new(Vec::new()),
        children: Mutex::new(Vec::new()),
    });
    COHORTS.lock().insert(id, node);
    if let Some(enclosing) = cohort_at(parent) {
        enclosing.children.lock().push(id);
    }
    set_current_cohort(id);
    // An enclosing cohort cancelled between reading `parent` and linking
    // this one in never reaches the new cohort through that edge, so a
    // cohort opened under a cancelled chain starts cancelled itself.
    if parent != 0 && chain_is_cancelled(parent) {
        cancel(id);
    }
    if timeout_ms > 0 {
        // The deadline rides the scheduler's timer wheel, so a bounded
        // cohort costs an entry there rather than a thread parked on a
        // sleep.
        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
        let timer_gid = crate::sched_global::add_timer(deadline);
        crate::sched_global::register_waker(
            timer_gid,
            Box::new(move || {
                if let Some(node) = cohort_at(id) {
                    node.state.lock().timed_out = true;
                    cancel(id);
                }
            }),
        );
    }
    id
}

/// Reserves a positional slot for a child about to be spawned into
/// `id`, and counts it as outstanding. Returns the child's index.
pub fn register_child(id: i64, reason: String) -> i64 {
    let Some(node) = cohort_at(id) else {
        return -1;
    };
    let mut state = node.state.lock();
    let index = state.next_index;
    state.next_index += 1;
    state.outstanding += 1;
    state.live.push(LiveChild { index, reason });
    index
}

/// Called on the child goroutine before its body runs.
pub fn enter_child(id: i64) {
    if id != 0 {
        set_current_cohort(id);
    }
}

/// Called on the child goroutine after its body finishes, however it
/// finished. `failure` carries the message when the child panicked or
/// returned an `Err`.
pub fn leave_child(id: i64, index: i64, failure: Option<String>) {
    set_current_cohort(0);
    let Some(node) = cohort_at(id) else {
        return;
    };
    // Read and released before the state lock is taken. The joiner reaches
    // `mark_handle_observed` on its own goroutine and holds the state lock
    // while it consults the same set, so this path holding only one at a
    // time is what leaves the two no cycle to deadlock on.
    let observed = observed_ahead(id, index);
    let cancel_now;
    {
        let mut state = node.state.lock();
        state.outstanding -= 1;
        state.live.retain(|live| live.index != index);
        if let Some(message) = failure {
            // `Log` names every failure where it happens; the block still
            // answers `Ok`, so this is the only place the program sees it.
            if node.on_error == ON_ERROR_LOG {
                eprintln!("gossamer: cohort child failed: {message}");
            }
            state.failures.push(ChildFailure {
                index,
                message,
                observed,
            });
            // Fail-fast winds the siblings down as soon as one child
            // fails; race does the same as soon as one succeeds.
            cancel_now = node.policy == POLICY_FAIL_FAST;
        } else {
            state.successes += 1;
            cancel_now = node.policy == POLICY_RACE;
        }
        node.progress.notify_all();
    }
    wake_joiners(&node);
    if cancel_now {
        cancel(id);
    }
}

/// How long the root drain waits at exit before it reports what is still
/// running and lets the process end.
///
/// A `cohort { }` block drains without a deadline: leaving it is the
/// program's own statement that its children are finished, and a bound
/// there would cut short work the author asked for. The root drain runs
/// after `main` has returned, where a child still going is one nothing
/// is waiting on, and an unbounded wait there turns a goroutine that
/// never reaches a safepoint into a process that never exits. The
/// duration is generous because it only ever elapses when something has
/// already gone wrong, and what happens then is a report, not a kill.
const ROOT_DRAIN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// Waits for `node`'s children with a deadline, on the OS thread the
/// root drain runs on. Answers the number still outstanding, which is
/// zero when they all finished.
fn wait_for_drain_bounded(node: &Arc<Cohort>, deadline: std::time::Duration) -> i64 {
    let until = crate::platform::Instant::now() + deadline;
    let mut state = node.state.lock();
    while state.outstanding > 0 {
        let now = crate::platform::Instant::now();
        if now >= until {
            return state.outstanding;
        }
        if node.progress.wait_for(&mut state, until - now).timed_out() && state.outstanding > 0 {
            return state.outstanding;
        }
    }
    0
}

/// Blocks until `id` has no outstanding children. A goroutine gives its
/// carrier back while it waits; a plain OS thread keeps the condvar.
fn wait_for_drain(node: &Arc<Cohort>) {
    loop {
        {
            let state = node.state.lock();
            if state.outstanding <= 0 {
                return;
            }
        }
        if crate::sched_global::current_gid().is_some() {
            let mut parked_as = None;
            let state = node.state.lock();
            if state.outstanding <= 0 {
                return;
            }
            let mut guard = Some(state);
            crate::sched_global::park(crate::sched::ParkReason::Sync, |parker| {
                parked_as = Some(parker.gid);
                node.joiners.lock().push(parker.gid);
                drop(guard.take());
            });
            if let Some(gid) = parked_as {
                node.joiners.lock().retain(|x| *x != gid);
            }
        } else {
            let mut state = node.state.lock();
            if state.outstanding > 0 {
                node.progress.wait(&mut state);
            }
        }
    }
}

/// The cohort's outcome message, or `None` when it succeeded.
fn outcome_message(node: &Arc<Cohort>) -> Option<String> {
    let mut state = node.state.lock();
    state.joined = true;
    // `on_error` decides what the cohort DOES with a failure; `policy`
    // decides when it stops waiting. Under `Log` and `Ignore` the block
    // answers `Ok`, and the failures are marked observed so the drain
    // report does not name them a second time - they reached the program
    // through the disposition it asked for. What no setting can do is stop
    // a child being counted or drained.
    if node.on_error != ON_ERROR_PROPAGATE {
        for failure in &mut state.failures {
            failure.observed = true;
        }
        return None;
    }
    // Lowest index first, so the reported failure does not depend on
    // which child happened to finish first.
    state.failures.sort_by_key(|failure| failure.index);
    match node.policy {
        POLICY_COLLECT_ALL if !state.failures.is_empty() => Some(
            state
                .failures
                .iter()
                .map(|failure| failure.message.as_str())
                .collect::<Vec<_>>()
                .join("; "),
        ),
        POLICY_RACE => {
            if state.successes > 0 {
                None
            } else {
                state
                    .failures
                    .first()
                    .map(|failure| failure.message.clone())
                    .or_else(|| state.timed_out.then(|| "cohort timed out".to_string()))
            }
        }
        _ => state
            .failures
            .first()
            .map(|failure| failure.message.clone())
            .or_else(|| state.timed_out.then(|| "cohort timed out".to_string())),
    }
}

/// Joins the running goroutine's cohort: waits for every child, then
/// reports the cohort's outcome. Leaves the cohort current, so the
/// matching pop still runs.
fn join_current() -> Option<String> {
    let id = current_cohort();
    let node = cohort_at(id)?;
    drain_within_bound(&node);
    outcome_message(&node)
}

/// Waits for `node`'s children, bounded by its own `drain:` setting when it
/// named one. A cohort with no bound waits as long as its children take:
/// leaving the block is the program's statement that they are finished.
fn drain_within_bound(node: &Arc<Cohort>) {
    let bound = node.drain_ms;
    if bound <= 0 {
        wait_for_drain(node);
        return;
    }
    let outstanding = wait_for_drain_bounded(node, std::time::Duration::from_millis(bound as u64));
    if outstanding > 0 {
        // Silence is unconstructible: a drain that gave up names what it
        // left running, whatever the cohort's error disposition says.
        eprintln!(
            "gossamer: cohort drain bound of {bound}ms elapsed with {outstanding} goroutine(s) still running{}",
            unfinished_children(node)
        );
    }
}

/// Closes the running goroutine's cohort: cancels anything still
/// running, waits for it, retires the cohort, and restores the
/// enclosing one. Runs from the block's `defer`, so it covers every
/// exit edge - including a `return` or a `?` out of the middle of the
/// block.
fn pop_current() {
    let id = current_cohort();
    let Some(node) = cohort_at(id) else {
        return;
    };
    let already_joined = node.state.lock().joined;
    if !already_joined {
        cancel(id);
        drain_within_bound(&node);
    }
    set_current_cohort(node.parent);
    if node.cancelled.load(Ordering::Acquire) {
        CANCELLED_COHORTS.fetch_sub(1, Ordering::AcqRel);
    }
    // A handle nobody joined has no one left to mark it observed, so its
    // entry retires with the cohort rather than living for the process. An
    // observation recorded ahead of its failure retires the same way: every
    // child consumes its own on the way out, and this covers a cohort torn
    // down before one of them got there.
    CHILD_HANDLES.lock().retain(|_, (cohort, _)| *cohort != id);
    OBSERVED_AHEAD.lock().retain(|(cohort, _)| *cohort != id);
    if let Some(enclosing) = cohort_at(node.parent) {
        enclosing.children.lock().retain(|child| *child != id);
    }
    COHORTS.lock().remove(&id);
}

/// Closes every cohort still open on the running goroutine. The
/// goroutine spawn wrappers call this as their last act, so a body that
/// left through a path `defer` does not cover - a panic - still cannot
/// leave its children running.
pub fn unwind_open_cohorts() {
    if !any_cohort_live() {
        return;
    }
    while current_cohort() != 0 {
        pop_current();
    }
}

/// `runtime::cohort_push(policy, timeout_ms, isolation)` - opens a cohort.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_cohort_push(
    policy: i64,
    timeout_ms: i64,
    isolation: i64,
    on_error: i64,
    uncancellable: i64,
    drain_ms: i64,
) -> i64 {
    ffi_entry!(0, {
        push(
            policy,
            timeout_ms,
            isolation,
            on_error,
            uncancellable,
            drain_ms,
        )
    })
}

/// `runtime::cohort_join()` - waits for the cohort's children and
/// answers `Result<(), errors::Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_cohort_join() -> i128 {
    ffi_entry!(super::vec::pack_result(0, 0), {
        match join_current() {
            None => super::vec::pack_result(0, 0),
            Some(message) => {
                let err = super::errors::error_new_from_bytes(message.as_bytes());
                super::vec::pack_result(1, err as i64)
            }
        }
    })
}

/// `runtime::cohort_pop()` - closes the cohort.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_cohort_pop() {
    ffi_entry!((), { pop_current() });
}

/// `runtime::cohort_cancelled()` - whether the running goroutine's
/// cohort has been cancelled. Source-visible so a CPU-bound child can
/// cooperate at a point of its own choosing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_cohort_cancelled() -> i64 {
    ffi_entry!(0, { i64::from(current_is_cancelled()) })
}

/// `runtime::cohort_cancel()` - cancels the running goroutine's cohort
/// from inside it, so a child that finds its own answer can wind the
/// others down without failing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_cohort_cancel() {
    ffi_entry!((), {
        let id = current_cohort();
        if id != 0 {
            cancel(id);
        }
    });
}

/// Whether children of the running goroutine's cohort run on dedicated
/// OS threads. Read by the spawn path.
pub fn current_isolation() -> i64 {
    let id = current_cohort();
    let mut current = id;
    while current != 0 {
        let Some(node) = cohort_at(current) else {
            return ISOLATION_SHARED;
        };
        if node.isolation != ISOLATION_SHARED {
            return node.isolation;
        }
        current = node.parent;
    }
    ISOLATION_SHARED
}

/// The join handle of every cohort child, by handle address, so joining
/// a handle can mark that child's failure as one the program saw.
static CHILD_HANDLES: LazyLock<Mutex<HashMap<usize, (i64, i64)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Records which cohort child a join handle belongs to.
pub fn note_child_handle(handle: usize, cohort: i64, index: i64) {
    if handle == 0 || cohort == 0 {
        return;
    }
    CHILD_HANDLES.lock().insert(handle, (cohort, index));
}

/// Children whose outcome a joiner read before their failure was
/// recorded, as `(cohort, index)`.
///
/// A child delivers its outcome to the join handle before it reports to
/// its cohort, so the joiner can reach `mark_handle_observed` first. The
/// observation is kept here until [`leave_child`] pushes the failure,
/// which is born observed rather than orphaned.
static OBSERVED_AHEAD: LazyLock<Mutex<HashSet<(i64, i64)>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Marks the child behind `handle` as observed: its outcome reached the
/// program, so a failure it reported is not an orphaned one.
///
/// The failure may not be recorded yet, so an observation that arrives
/// first is remembered rather than dropped.
pub fn mark_handle_observed(handle: usize) {
    if handle == 0 {
        return;
    }
    let entry = CHILD_HANDLES.lock().remove(&handle);
    let Some((cohort, index)) = entry else {
        return;
    };
    if let Some(node) = cohort_at(cohort) {
        let mut state = node.state.lock();
        let mut found = false;
        for failure in &mut state.failures {
            if failure.index == index {
                failure.observed = true;
                found = true;
            }
        }
        if !found {
            OBSERVED_AHEAD.lock().insert((cohort, index));
        }
    }
}

/// Whether a joiner already read this child's outcome.
fn observed_ahead(cohort: i64, index: i64) -> bool {
    OBSERVED_AHEAD.lock().remove(&(cohort, index))
}

/// Opens the process-wide root cohort that `main` runs inside.
///
/// Every `spawn` is a child of some cohort, so a goroutine cannot outlive
/// the program and a failure cannot vanish unread. The root's policy is
/// collect-all: it exists to bound lifetimes and surface failures, not to
/// impose fail-fast on a program that never asked for it, so one child's
/// `Err` never cancels another's work.
pub fn open_root() {
    if current_cohort() != 0 {
        return;
    }
    // The root is collect-all and propagating: it exists to bound lifetimes
    // and surface failures, not to impose a policy the program never asked
    // for. Its drain bound is `ROOT_DRAIN_DEADLINE`, applied at close.
    push(
        POLICY_COLLECT_ALL,
        0,
        ISOLATION_SHARED,
        ON_ERROR_PROPAGATE,
        0,
        0,
    );
}

/// Closes the root cohort: waits for every child `main` left running, then
/// reports any failure that nothing in the program ever read.
///
/// A failure whose join handle was joined has already reached the program
/// through that handle, so only the unobserved ones are reported here.
pub fn close_root() {
    let id = current_cohort();
    let Some(node) = cohort_at(id) else {
        return;
    };
    // The root drain runs on the main OS thread, after `main` has
    // returned: a goroutine still running here is one nothing joined.
    let outstanding = if crate::sched_global::current_gid().is_some() {
        wait_for_drain(&node);
        0
    } else {
        wait_for_drain_bounded(&node, ROOT_DRAIN_DEADLINE)
    };
    if outstanding > 0 {
        eprintln!(
            "gossamer: {outstanding} spawned goroutine(s) had not finished {} seconds after \
             `main` returned; exiting without them{}",
            ROOT_DRAIN_DEADLINE.as_secs(),
            unfinished_children(&node)
        );
    }
    let orphaned: Vec<String> = {
        let state = node.state.lock();
        state
            .failures
            .iter()
            .filter(|failure| !failure.observed)
            .map(|failure| failure.message.clone())
            .collect()
    };
    for message in orphaned {
        eprintln!("gossamer: spawned goroutine failed with nobody to observe it: {message}");
    }
    node.state.lock().joined = true;
    pop_current();
}

/// The spawn indices of `node`'s children that never left, as a phrase to
/// append to a drain report. Empty when every child finished: an
/// unfinished child is what the invariant is about, so it is named rather
/// than only counted.
fn unfinished_children(node: &Arc<Cohort>) -> String {
    let live = node.state.lock().live.clone();
    if live.is_empty() {
        return String::new();
    }
    format!(" (spawn index {})", render_live(&live))
}

/// One descriptor line per live cohort, oldest id first: the id, its
/// parent, its completion policy, its error disposition, how many children
/// are outstanding, and the spawn indices of the ones that have not left.
///
/// A cohort is enumerable so a program can say what it is still waiting on
/// without joining it. The lines are text on purpose - this is a diagnostic
/// surface, like `pprof`, and a caller reads it or prints it.
pub fn cohort_report_lines() -> Vec<String> {
    let mut nodes: Vec<(i64, Arc<Cohort>)> = COHORTS
        .lock()
        .iter()
        .map(|(id, node)| (*id, Arc::clone(node)))
        .collect();
    nodes.sort_by_key(|(id, _)| *id);
    nodes
        .into_iter()
        .map(|(id, node)| describe_cohort(id, &node))
        .collect()
}

/// One cohort's descriptor line, the shape both [`cohort_report_lines`] and
/// [`root_report_line`] answer in.
fn describe_cohort(id: i64, node: &Arc<Cohort>) -> String {
    let state = node.state.lock();
    format!(
        "id={id} parent={} policy={} on_error={} outstanding={} cancelled={} live=[{}]",
        node.parent,
        policy_name(node.policy),
        on_error_name(node.on_error),
        state.outstanding,
        node.cancelled.load(Ordering::Acquire),
        render_live(&state.live)
    )
}

/// The root cohort's descriptor line, or an empty string when no cohort is
/// open. The root is the one cohort every program has - `main` runs inside
/// it - and the one whose drain bounds process exit, so it is the cohort a
/// program asks about at shutdown.
pub fn root_report_line() -> String {
    let root = COHORTS
        .lock()
        .iter()
        .filter(|(_, node)| node.parent == 0)
        .map(|(id, node)| (*id, Arc::clone(node)))
        .min_by_key(|(id, _)| *id);
    match root {
        Some((id, node)) => describe_cohort(id, &node),
        None => String::new(),
    }
}

/// Name of a completion policy, for [`cohort_report_lines`].
fn policy_name(policy: i64) -> &'static str {
    match policy {
        POLICY_COLLECT_ALL => "CollectAll",
        POLICY_RACE => "Race",
        _ => "FailFast",
    }
}

/// Name of an error disposition, for [`cohort_report_lines`].
fn on_error_name(on_error: i64) -> &'static str {
    match on_error {
        ON_ERROR_LOG => "Log",
        ON_ERROR_IGNORE => "Ignore",
        _ => "Propagate",
    }
}

/// `runtime::cohorts()` - the live cohort descriptors as a `Vec<String>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_cohorts() -> *mut crate::c_abi::vec::GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let vec = unsafe {
            crate::c_abi::vec::gos_rt_vec_new_typed(8, crate::c_abi::vec::vec_elem_kind::STRING)
        };
        if vec.is_null() {
            return vec;
        }
        for line in cohort_report_lines() {
            let cs = crate::c_abi::string::alloc_cstring(line.as_bytes()) as i64;
            unsafe { crate::c_abi::vec::gos_rt_vec_push(vec, std::ptr::addr_of!(cs).cast::<u8>()) };
        }
        vec
    })
}

/// `runtime::root()` - the root cohort's descriptor line as a String.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_cohort_root() -> *mut std::os::raw::c_char {
    ffi_entry!(std::ptr::null_mut(), {
        crate::c_abi::string::alloc_cstring(root_report_line().as_bytes())
    })
}

/// Renders a panic payload the way the join handle does, for a child
/// whose failure reaches the cohort through the unwinding guard.
pub fn panic_failure_message(raw: Option<String>) -> String {
    raw.unwrap_or_else(|| "spawned goroutine panicked".to_string())
}

/// Runs an isolated child on an OS thread of its own, for the whole of
/// its life. Blocking there stalls nothing else, which is the point of
/// the isolated context: synchronous Rust and CPU-bound work have
/// somewhere to run that no other goroutine shares.
#[cfg(target_arch = "wasm32")]
pub fn spawn_isolated(body: Box<dyn FnOnce() + Send + 'static>) {
    // wasm32 has one thread, so isolation has nowhere to go: the body
    // runs where every other goroutine runs.
    crate::sched_global::spawn(body);
}

#[cfg(not(target_arch = "wasm32"))]
pub fn spawn_isolated(body: Box<dyn FnOnce() + Send + 'static>) {
    let cell = Arc::new(Mutex::new(Some(body)));
    let on_thread = Arc::clone(&cell);
    // The child is not a scheduler goroutine, yet it can send, receive, and
    // close. It counts as an actor from before its thread exists until the
    // thread ends, so a channel wait elsewhere is never read as terminal
    // while the child is still positioned to act.
    let actor = crate::sched_global::ExternalActor::enter();
    let spawned = std::thread::Builder::new()
        .name("gos-isolated".to_string())
        .stack_size(ISOLATED_STACK_BYTES)
        .spawn(move || {
            let _actor = actor;
            gossamer_coro::arm_stack_guard(
                ISOLATED_STACK_BYTES - gossamer_coro::STACK_GUARD_MARGIN,
            );
            let body = on_thread.lock().take();
            if let Some(body) = body {
                body();
            }
        });
    if spawned.is_err() {
        // The OS refused a thread. The child still has to run, and a
        // shared carrier is the only place left for it.
        let body = cell.lock().take();
        if let Some(body) = body {
            crate::sched_global::spawn(body);
        }
    }
}

/// Stack reserve for an isolated child. It runs the same bodies a
/// goroutine does, so it gets the same reserve a scheduler carrier has.
#[cfg(not(target_arch = "wasm32"))]
const ISOLATED_STACK_BYTES: usize = 8 * 1024 * 1024;
