//! `cohort { }` builtins for the bytecode VM - the twin of
//! `gossamer-runtime/src/c_abi/cohort.rs`.
//!
//! The two substrates differ in what a goroutine is: the compiled tiers
//! multiplex coroutines over carrier threads, while a VM goroutine owns
//! a pool thread for its whole life. The current cohort is therefore
//! thread-local here and `Gid`-keyed there, and both answer the same
//! questions in the same order, which is what keeps the observable
//! behaviour identical.
//!
//! Everything else matches by construction: a child's index is assigned
//! at its `spawn` call, the reported failure is the lowest-index one
//! rather than the first to arrive, and cancellation is observed as an
//! operation's ordinary "nothing more is coming" answer.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use crate::builtins::{BuiltinFnPub, value_to_int};
use crate::value::{RuntimeResult, Value};

use gossamer_runtime::platform::Instant;

/// Completion policy, as spelled by `Policy::` in source.
pub(crate) const POLICY_FAIL_FAST: i64 = 0;
pub(crate) const POLICY_COLLECT_ALL: i64 = 1;
pub(crate) const POLICY_RACE: i64 = 2;
/// The first failure becomes the block's `Err` - the default.
pub(crate) const ON_ERROR_PROPAGATE: i64 = 0;
/// Every failure is named on stderr as it happens; the block answers `Ok`.
pub(crate) const ON_ERROR_LOG: i64 = 1;
/// A failure changes nothing the block answers. It is still counted, still
/// drained, and still named by the drain report at exit.
pub(crate) const ON_ERROR_IGNORE: i64 = 2;

/// Execution context for children, as spelled by `Context::` in source.
pub(crate) const ISOLATION_SHARED: i64 = 0;
pub(crate) const ISOLATION_THREAD: i64 = 1;

/// Whether any cohort has ever been opened, so a program that uses none
/// pays one relaxed load per cancellation point.
static ANY_COHORT: AtomicBool = AtomicBool::new(false);

/// Whether this process has opened a cohort.
#[inline]
pub(crate) fn any_cohort_live() -> bool {
    ANY_COHORT.load(Ordering::Relaxed)
}

/// Cohorts currently cancelled. `main` runs inside a root cohort, so
/// "any cohort exists" is true for every program; this counter is the
/// fast path a cancellation point reads instead.
static CANCELLED_COHORTS: AtomicI64 = AtomicI64::new(0);

/// The join handle of every cohort child, keyed by the handle channel's
/// identity, so joining it marks that child's failure as one the program
/// saw.
static CHILD_HANDLES: LazyLock<parking_lot::Mutex<HashMap<usize, (i64, i64)>>> =
    LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));

/// Records which cohort child a join handle belongs to.
pub(crate) fn note_child_handle(handle: usize, cohort: i64, index: i64) {
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
static OBSERVED_AHEAD: LazyLock<parking_lot::Mutex<HashSet<(i64, i64)>>> =
    LazyLock::new(|| parking_lot::Mutex::new(HashSet::new()));

/// Marks the child behind `handle` as observed: its outcome reached the
/// program, so a failure it reported is not an orphaned one.
///
/// The failure may not be recorded yet, so an observation that arrives
/// first is remembered rather than dropped.
pub(crate) fn mark_handle_observed(handle: usize) {
    if handle == 0 {
        return;
    }
    let entry = CHILD_HANDLES.lock().remove(&handle);
    let Some((cohort, index)) = entry else {
        return;
    };
    if let Some(node) = node_of(cohort) {
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

/// One child's failure, and whether any joiner ever read it.
struct ChildFailure {
    index: i64,
    message: String,
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
    failures: Vec<ChildFailure>,
    successes: i64,
    /// The children that registered and have not left. A drain that gives
    /// up names these, so an unfinished child is identified - by its spawn
    /// index, and by the spawn's own `reason:` label when it carried one -
    /// rather than only counted.
    live: Vec<LiveChild>,
    joined: bool,
    timed_out: bool,
}

struct CohortNode {
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
    state: parking_lot::Mutex<CohortState>,
    progress: parking_lot::Condvar,
    /// Cohorts opened under this one, still live. A child sleeping
    /// inside a nested cohort waits on that cohort's own `progress`, so
    /// cancelling reaches it by walking down this edge.
    children: parking_lot::Mutex<Vec<i64>>,
}

static COHORTS: LazyLock<parking_lot::Mutex<HashMap<i64, Arc<CohortNode>>>> =
    LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));
static NEXT_ID: AtomicI64 = AtomicI64::new(1);

thread_local! {
    /// The cohort current on this goroutine. A VM goroutine owns its
    /// thread for its whole life, so thread-local is per goroutine here.
    static CURRENT: std::cell::Cell<i64> = const { std::cell::Cell::new(0) };
}

fn node_of(id: i64) -> Option<Arc<CohortNode>> {
    if id == 0 {
        return None;
    }
    COHORTS.lock().get(&id).cloned()
}

/// The running goroutine's current cohort id, or 0.
pub(crate) fn current_cohort() -> i64 {
    if !any_cohort_live() {
        return 0;
    }
    CURRENT.with(std::cell::Cell::get)
}

fn set_current(id: i64) {
    CURRENT.with(|cell| cell.set(id));
}

/// Whether `id` or any enclosing cohort is cancelled.
fn chain_is_cancelled(id: i64) -> bool {
    let mut current = id;
    while current != 0 {
        let Some(node) = node_of(current) else {
            return true;
        };
        if node.cancelled.load(Ordering::Acquire) {
            return true;
        }
        current = node.parent;
    }
    false
}

/// Whether the running goroutine's cohort chain is cancelled - the
/// predicate every cancellation point consults.
pub(crate) fn current_is_cancelled() -> bool {
    if CANCELLED_COHORTS.load(Ordering::Relaxed) == 0 {
        return false;
    }
    let id = current_cohort();
    id != 0 && chain_is_cancelled(id)
}

/// Marks `id` cancelled, along with every cohort nested inside it, and
/// wakes what each has waiting.
///
/// A sleeping child waits on the `progress` condvar of the cohort it is
/// in, which is not the one an ancestor's failure cancels, so the wake
/// travels down every edge rather than only to the cancelled cohort's
/// own children. The descendants are visited iteratively, so nesting
/// depth costs heap rather than frames.
fn cancel(id: i64) {
    let mut pending = vec![id];
    let mut marked_any = false;
    while let Some(current) = pending.pop() {
        let Some(node) = node_of(current) else {
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
            // The flag changes and the wake are issued under the lock a
            // waiter holds while it tests the flag, so a cancellation
            // landing between one waiter's test and its park still
            // reaches it.
            let _state = node.state.lock();
            if node.cancelled.swap(true, Ordering::AcqRel) {
                continue;
            }
            marked_any = true;
            CANCELLED_COHORTS.fetch_add(1, Ordering::AcqRel);
            node.progress.notify_all();
        }
        pending.extend(node.children.lock().iter().copied());
    }
    if marked_any {
        // A cancelled cohort's children are waiting on channels of their
        // own, so wake every channel waiter to re-check its condition. The
        // channel layer answers `None` to a receiver under a cancelled
        // cohort, which is the same answer a closed channel gives.
        crate::value::wake_all_channel_waiters();
    }
}

/// Deadlines waiting to fire, earliest last so the timer thread pops the
/// back. One thread serves every cohort.
static DEADLINES: LazyLock<parking_lot::Mutex<Vec<(Instant, i64)>>> =
    LazyLock::new(|| parking_lot::Mutex::new(Vec::new()));
static DEADLINE_WAKE: LazyLock<parking_lot::Condvar> = LazyLock::new(parking_lot::Condvar::new);
static TIMER_THREAD: std::sync::Once = std::sync::Once::new();

fn schedule_deadline(deadline: Instant, id: i64) {
    {
        let mut queue = DEADLINES.lock();
        queue.push((deadline, id));
        queue.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.0));
    }
    TIMER_THREAD.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("gossamer-cohort-timer".to_string())
            .spawn(run_deadline_timer);
    });
    DEADLINE_WAKE.notify_all();
}

fn run_deadline_timer() {
    loop {
        let mut queue = DEADLINES.lock();
        let Some(&(earliest, id)) = queue.last() else {
            DEADLINE_WAKE.wait(&mut queue);
            continue;
        };
        let now = Instant::now();
        if earliest > now {
            DEADLINE_WAKE.wait_for(&mut queue, earliest - now);
            continue;
        }
        queue.pop();
        drop(queue);
        if let Some(node) = node_of(id) {
            node.state.lock().timed_out = true;
            cancel(id);
        }
    }
}

/// Whether any cohort deadline is still pending. A pending deadline is
/// an actor outside the goroutine set that will cancel a cohort, so a
/// program waiting on one is waiting rather than deadlocked.
pub(crate) fn deadline_pending() -> bool {
    !DEADLINES.lock().is_empty()
}

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
    let node = Arc::new(CohortNode {
        parent,
        policy,
        isolation,
        on_error,
        uncancellable: uncancellable != 0,
        drain_ms,
        cancelled: AtomicBool::new(false),
        state: parking_lot::Mutex::new(CohortState {
            next_index: 0,
            outstanding: 0,
            failures: Vec::new(),
            successes: 0,
            live: Vec::new(),
            joined: false,
            timed_out: false,
        }),
        progress: parking_lot::Condvar::new(),
        children: parking_lot::Mutex::new(Vec::new()),
    });
    COHORTS.lock().insert(id, node);
    if let Some(enclosing) = node_of(parent) {
        enclosing.children.lock().push(id);
    }
    set_current(id);
    // An enclosing cohort cancelled between reading `parent` and linking
    // this one in never reaches the new cohort through that edge, so a
    // cohort opened under a cancelled chain starts cancelled itself.
    if parent != 0 && chain_is_cancelled(parent) {
        cancel(id);
    }
    if timeout_ms > 0 {
        schedule_deadline(
            Instant::now() + Duration::from_millis(timeout_ms as u64),
            id,
        );
    }
    id
}

/// Reserves a positional slot for a child about to be spawned into `id`.
pub(crate) fn register_child(id: i64, reason: String) -> i64 {
    let Some(node) = node_of(id) else {
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
pub(crate) fn enter_child(id: i64) {
    if id != 0 {
        set_current(id);
    }
}

/// Called on the child goroutine once its body has finished, however it
/// finished.
pub(crate) fn leave_child(id: i64, index: i64, failure: Option<String>) {
    set_current(0);
    let Some(node) = node_of(id) else {
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
        match failure {
            Some(message) => {
                // `Log` names every failure where it happens; the block
                // still answers `Ok`, so this is the only place the program
                // sees it.
                if node.on_error == ON_ERROR_LOG {
                    eprintln!("gossamer: cohort child failed: {message}");
                }
                state.failures.push(ChildFailure {
                    index,
                    message,
                    observed,
                });
                cancel_now = node.policy == POLICY_FAIL_FAST;
            }
            None => {
                state.successes += 1;
                cancel_now = node.policy == POLICY_RACE;
            }
        }
        node.progress.notify_all();
    }
    if cancel_now {
        cancel(id);
    }
}

/// Sleeps for `duration` unless the running goroutine's cohort is
/// cancelled first, and reports whether the full duration elapsed.
///
/// The wait is on the cohort's own condition variable, which cancelling
/// signals, so a cancelled sleeper wakes at once rather than at the end
/// of a polling slice. Outside a cohort this is a plain sleep.
pub(crate) fn sleep_cancellable(duration: Duration) -> bool {
    let id = current_cohort();
    let Some(node) = node_of(id) else {
        gossamer_runtime::platform::sleep(duration);
        return true;
    };
    // Nothing runs alongside this goroutine on the browser build, so the
    // cancellation the wait would wake for is the state read here.
    if !gossamer_runtime::platform::CAN_BLOCK {
        gossamer_runtime::platform::sleep(duration);
        return !chain_is_cancelled(id);
    }
    let deadline = Instant::now() + duration;
    loop {
        if chain_is_cancelled(id) {
            return false;
        }
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        let mut state = node.state.lock();
        // Cancelling marks this cohort's own flag - an ancestor's
        // cancellation cascades down to it - and does so under this
        // lock, so testing the flag here rather than before taking the
        // lock is what makes the wake impossible to miss.
        if node.cancelled.load(Ordering::Acquire) {
            return false;
        }
        node.progress.wait_for(&mut state, deadline - now);
    }
}

/// Whether children of the running goroutine's cohort run isolated.
pub(crate) fn current_isolation() -> i64 {
    let mut current = current_cohort();
    while current != 0 {
        let Some(node) = node_of(current) else {
            return ISOLATION_SHARED;
        };
        if node.isolation != ISOLATION_SHARED {
            return node.isolation;
        }
        current = node.parent;
    }
    ISOLATION_SHARED
}

/// Waits for `node`'s children. Answers `false` when the wait can never end:
/// every participant is waiting, the children included.
fn wait_for_drain(node: &Arc<CohortNode>) -> bool {
    if node.state.lock().outstanding == 0 {
        return true;
    }
    let Some(_joining) = crate::vm::goroutine::JoinWait::enter() else {
        return false;
    };
    let mut state = node.state.lock();
    // A child settles at its spawn on the browser build, so the count is
    // already final and a wait would be for a goroutine that has finished.
    while gossamer_runtime::platform::CAN_BLOCK && state.outstanding > 0 {
        node.progress.wait(&mut state);
    }
    true
}

/// How long the root drain waits at exit before it reports what is still
/// running and lets the process end. Matches the compiled tiers'
/// `ROOT_DRAIN_DEADLINE`; see the comment there for why a `cohort { }`
/// block keeps its unbounded wait and only the root gets a deadline.
pub(crate) const ROOT_DRAIN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// Waits for `node`'s children, bounded by its own `drain:` setting when it
/// named one. A cohort with no bound waits as long as its children take:
/// leaving the block is the program's statement that they are finished.
/// Answers `false` when the children can never finish.
fn drain_within_bound(node: &Arc<CohortNode>) -> bool {
    let bound = node.drain_ms;
    if bound <= 0 {
        return wait_for_drain(node);
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
    true
}

fn wait_for_drain_bounded(node: &Arc<CohortNode>, deadline: std::time::Duration) -> i64 {
    wait_for_drain_until(node, || {
        gossamer_runtime::platform::Instant::now() + deadline
    })
}

/// Waits for `node`'s children until the instant `until` answers. Answers
/// the number still outstanding, which is zero when they all finished.
///
/// The deadline arrives as a function because a monotonic reading belongs
/// to a wait that actually happens: children that have all finished are
/// answered without one, which is all a target with no monotonic clock can
/// offer.
fn wait_for_drain_until(
    node: &Arc<CohortNode>,
    until: impl FnOnce() -> gossamer_runtime::platform::Instant,
) -> i64 {
    let mut state = node.state.lock();
    if state.outstanding == 0 {
        return 0;
    }
    let until = until();
    while state.outstanding > 0 {
        let now = gossamer_runtime::platform::Instant::now();
        if now >= until {
            return state.outstanding;
        }
        if node.progress.wait_for(&mut state, until - now).timed_out() && state.outstanding > 0 {
            return state.outstanding;
        }
    }
    0
}

fn outcome_message(node: &Arc<CohortNode>) -> Option<String> {
    let mut state = node.state.lock();
    state.joined = true;
    // `on_error` decides what the cohort DOES with a failure; `policy`
    // decides when it stops waiting. Under `Log` and `Ignore` the block
    // answers `Ok`, and the failures are marked observed so the drain
    // report does not name them a second time. What no setting can do is
    // stop a child being counted or drained.
    if node.on_error != ON_ERROR_PROPAGATE {
        for failure in &mut state.failures {
            failure.observed = true;
        }
        return None;
    }
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

fn pop_current() {
    let id = current_cohort();
    let Some(node) = node_of(id) else {
        return;
    };
    let already_joined = node.state.lock().joined;
    if !already_joined {
        cancel(id);
        // Cancelled children leave their waits, so this drain ends.
        let _ = drain_within_bound(&node);
    }
    set_current(node.parent);
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
    if let Some(enclosing) = node_of(node.parent) {
        enclosing.children.lock().retain(|child| *child != id);
    }
    COHORTS.lock().remove(&id);
}

/// Opens the process-wide root cohort that `main` runs inside.
///
/// Every `spawn` is a child of some cohort, so a goroutine cannot outlive
/// the program and a failure cannot vanish unread. The root's policy is
/// collect-all: it bounds lifetimes and surfaces failures without
/// imposing fail-fast on a program that never asked for it.
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

/// Closes the root cohort: waits for what `main` spawned, then reports
/// any failure nothing in the program ever read.
pub fn close_root() {
    let id = current_cohort();
    let Some(node) = node_of(id) else {
        return;
    };
    // The root drain runs after the program's own work is done: a
    // goroutine still running here is one nothing joined.
    let outstanding = wait_for_drain_until(&node, crate::vm::goroutine::exit_drain_deadline);
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

/// Closes every cohort still open on the running goroutine. The spawn
/// wrappers call this last, so a body that left by a path `defer` does
/// not cover still cannot leave its children running.
pub(crate) fn unwind_open_cohorts() {
    if !any_cohort_live() {
        return;
    }
    while current_cohort() != 0 {
        pop_current();
    }
}

pub(crate) fn install_cohort(globals: &mut Vec<(&'static str, Value)>) {
    let entries: &[(&str, BuiltinFnPub)] = &[
        ("runtime::cohort_push", builtin_cohort_push),
        ("cohort_push", builtin_cohort_push),
        ("runtime::cohort_join", builtin_cohort_join),
        ("cohort_join", builtin_cohort_join),
        ("runtime::cohort_pop", builtin_cohort_pop),
        ("cohort_pop", builtin_cohort_pop),
        ("runtime::cohorts", builtin_cohorts),
        ("cohorts", builtin_cohorts),
        ("runtime::root", builtin_cohort_root),
        ("runtime::cohort_cancelled", builtin_cohort_cancelled),
        ("cohort_cancelled", builtin_cohort_cancelled),
        ("runtime::cohort_cancel", builtin_cohort_cancel),
        ("cohort_cancel", builtin_cohort_cancel),
    ];
    for (name, call) in entries {
        globals.push((*name, crate::builtins::builtin_pub(name, *call)));
    }
}

fn builtin_cohort_push(args: &[Value]) -> RuntimeResult<Value> {
    let policy = args
        .first()
        .and_then(value_to_int)
        .unwrap_or(POLICY_FAIL_FAST);
    let timeout_ms = args.get(1).and_then(value_to_int).unwrap_or(0);
    let isolation = args
        .get(2)
        .and_then(value_to_int)
        .unwrap_or(ISOLATION_SHARED);
    let on_error = args
        .get(3)
        .and_then(value_to_int)
        .unwrap_or(ON_ERROR_PROPAGATE);
    let uncancellable = args.get(4).and_then(value_to_int).unwrap_or(0);
    let drain_ms = args.get(5).and_then(value_to_int).unwrap_or(0);
    push(
        policy,
        timeout_ms,
        isolation,
        on_error,
        uncancellable,
        drain_ms,
    );
    Ok(Value::Unit)
}

fn builtin_cohort_join(_args: &[Value]) -> RuntimeResult<Value> {
    let id = current_cohort();
    let Some(node) = node_of(id) else {
        return Ok(Value::variant("Ok", vec![Value::Unit]));
    };
    if !drain_within_bound(&node) {
        return Err(crate::value::deadlock_error("cohort join"));
    }
    Ok(match outcome_message(&node) {
        None => Value::variant("Ok", vec![Value::Unit]),
        Some(message) => Value::variant("Err", vec![crate::builtins::make_error_value(&message)]),
    })
}

fn builtin_cohort_pop(_args: &[Value]) -> RuntimeResult<Value> {
    pop_current();
    Ok(Value::Unit)
}

/// One descriptor line per live cohort, oldest id first: the id, its
/// parent, its completion policy, its error disposition, how many children
/// are outstanding, and the spawn indices of the ones that have not left.
///
/// A cohort is enumerable so a program can say what it is still waiting on
/// without joining it. The lines are text on purpose - this is a diagnostic
/// surface, like `pprof`, and a caller reads it or prints it.
fn builtin_cohorts(_args: &[Value]) -> RuntimeResult<Value> {
    let mut nodes: Vec<(i64, Arc<CohortNode>)> = COHORTS
        .lock()
        .iter()
        .map(|(id, node)| (*id, Arc::clone(node)))
        .collect();
    nodes.sort_by_key(|(id, _)| *id);
    let lines: Vec<Value> = nodes
        .into_iter()
        .map(|(id, node)| Value::String(describe_cohort(id, &node).as_str().into()))
        .collect();
    Ok(Value::Array(Arc::new(lines)))
}

/// One cohort's descriptor line, the shape both [`builtin_cohorts`] and
/// [`builtin_cohort_root`] answer in.
fn describe_cohort(id: i64, node: &Arc<CohortNode>) -> String {
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

/// `runtime::root()` - the root cohort's descriptor line, or an empty
/// string when no cohort is open. The root is the one cohort every program
/// has, and the one whose drain bounds process exit, so it is the cohort a
/// program asks about at shutdown.
fn builtin_cohort_root(_args: &[Value]) -> RuntimeResult<Value> {
    let root = COHORTS
        .lock()
        .iter()
        .filter(|(_, node)| node.parent == 0)
        .map(|(id, node)| (*id, Arc::clone(node)))
        .min_by_key(|(id, _)| *id);
    let line = match root {
        Some((id, node)) => describe_cohort(id, &node),
        None => String::new(),
    };
    Ok(Value::String(line.as_str().into()))
}

/// Name of a completion policy, for [`builtin_cohorts`].
fn policy_name(policy: i64) -> &'static str {
    match policy {
        POLICY_COLLECT_ALL => "CollectAll",
        POLICY_RACE => "Race",
        _ => "FailFast",
    }
}

/// Name of an error disposition, for [`builtin_cohorts`].
fn on_error_name(on_error: i64) -> &'static str {
    match on_error {
        ON_ERROR_LOG => "Log",
        ON_ERROR_IGNORE => "Ignore",
        _ => "Propagate",
    }
}

fn builtin_cohort_cancelled(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Bool(current_is_cancelled()))
}

fn builtin_cohort_cancel(_args: &[Value]) -> RuntimeResult<Value> {
    let id = current_cohort();
    if id != 0 {
        cancel(id);
    }
    Ok(Value::Unit)
}

/// The spawn indices of `node`'s children that never left, as a phrase to
/// append to a drain report. Empty when every child finished: an
/// unfinished child is what the invariant is about, so it is named rather
/// than only counted.
fn unfinished_children(node: &Arc<CohortNode>) -> String {
    let live = node.state.lock().live.clone();
    if live.is_empty() {
        return String::new();
    }
    format!(" (spawn index {})", render_live(&live))
}
