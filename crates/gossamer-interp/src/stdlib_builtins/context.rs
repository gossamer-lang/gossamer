#![allow(
    unused_imports,
    dead_code,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::missing_errors_doc,
    clippy::unnecessary_wraps,
    clippy::needless_pass_by_value
)]
//! `std::context` builtins for the bytecode VM - request-scoped
//! cancellation and deadlines. The handle is a struct holding its context
//! node, shared by every copy of the handle; a child holds its parent, so
//! a context lives exactly as long as a handle or a descendant does. A
//! registry of weak references by id lets a deadline and the server reach
//! a context without keeping it alive.
//!
//! Constructors `background` / `with_cancel` / `with_timeout` plus the
//! `cancel` / `is_cancelled` / `done` methods are the bit-identical VM
//! mirror of the compiled `gos_rt_ctx_*` shims. Cancellation is eager
//! down the child tree; `is_cancelled` also walks up the parent chain
//! and honours an optional deadline. Deadlines use `std::time::Instant`
//! plus one timer thread that drives the same cancellation path as
//! explicit cancel, so `done_chan()` is selectable on timeout.
//!
//! The closure-returning `with_cancel -> (ctx, cancel)` shape is a
//! documented follow-up. `done_chan()` returns the context's "done"
//! channel; `cancel` or deadline expiry closes it so a `select` recv arm
//! on the channel becomes ready (closed-channel select readiness).

use std::collections::HashMap as StdHashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, LazyLock, Weak};
use std::time::Duration;

use gossamer_runtime::platform::Instant;

use super::deadline_queue::DeadlineQueue;
use crate::builtins::{BuiltinFnPub, value_to_int};
use crate::value::{Channel, RuntimeError, RuntimeResult, Value};

struct CtxNode {
    id: i64,
    cancelled: AtomicBool,
    deadline: Option<Instant>,
    parent: Option<Arc<CtxNode>>,
    /// A context outside this registry whose cancellation this node
    /// follows. A request's context takes the server's, so a peer that
    /// disconnects and a process that begins shutting down each reach the
    /// handler without a second watcher to keep in step.
    follows: Option<gossamer_std::context::Context>,
    /// Live children by id; `cancel` walks these depth-first.
    children: parking_lot::Mutex<StdHashMap<i64, Weak<CtxNode>>>,
    /// The context's "done" channel. `cancel` closes it so a
    /// `select { _ = ctx.done_chan().recv() => ... }` arm becomes ready
    /// via closed-channel select readiness. Closing is idempotent.
    chan: Channel,
}

impl Drop for CtxNode {
    fn drop(&mut self) {
        if let Some(deadline) = self.deadline {
            DEADLINES.remove(deadline, self.id);
        }
        if let Some(parent) = &self.parent {
            parent.children.lock().remove(&self.id);
        }
        CTX_REGISTRY.lock().remove(&self.id);
    }
}

/// Every live context by id, held weakly: what a deadline or the server
/// cancels by id reaches a context only while something else holds it.
static CTX_REGISTRY: LazyLock<parking_lot::Mutex<StdHashMap<i64, Weak<CtxNode>>>> =
    LazyLock::new(|| parking_lot::Mutex::new(StdHashMap::new()));
static NEXT_ID: AtomicI64 = AtomicI64::new(1);

/// Context deadlines. One thread serves every context: a deadline costs an
/// entry here rather than a thread parked on a sleep, and a context that is
/// cancelled or dropped first removes its entry at once.
static DEADLINES: DeadlineQueue = DeadlineQueue::new("gossamer-ctx-timer", cancel_id);

/// Whether any context deadline is still waiting to fire. A pending deadline
/// is an actor outside the goroutine set that will close a done channel, so
/// a program blocked on one is waiting rather than deadlocked.
pub(crate) fn deadline_pending() -> bool {
    DEADLINES.pending()
}

pub(crate) fn install_context(globals: &mut Vec<(&'static str, Value)>) {
    let entries: &[(&str, BuiltinFnPub)] = &[
        ("Context::background", builtin_ctx_background),
        ("context::Context::background", builtin_ctx_background),
        ("Context::with_cancel", builtin_ctx_with_cancel),
        ("context::Context::with_cancel", builtin_ctx_with_cancel),
        ("Context::with_timeout", builtin_ctx_with_timeout),
        ("context::Context::with_timeout", builtin_ctx_with_timeout),
        // Methods (dispatched via the `context::Context` struct-name
        // key that `qualified_method_key` forms).
        ("context::Context::cancel", builtin_ctx_cancel),
        ("context::Context::is_cancelled", builtin_ctx_is_cancelled),
        ("context::Context::done", builtin_ctx_done),
        ("context::Context::done_chan", builtin_ctx_done_chan),
    ];
    for (name, call) in entries {
        globals.push((*name, crate::builtins::builtin_pub(name, *call)));
    }
}

const HANDLE_NAME: &str = "context::Context";
const HANDLE_FIELD: &str = "__ctx";

fn ctx_handle(node: Arc<CtxNode>) -> Value {
    crate::value::state_handle(HANDLE_NAME, HANDLE_FIELD, node)
}

fn node_of_value(value: &Value) -> Option<Arc<CtxNode>> {
    crate::value::handle_state(value, HANDLE_NAME, HANDLE_FIELD)
}

fn node_of_id(id: i64) -> Option<Arc<CtxNode>> {
    CTX_REGISTRY.lock().get(&id).and_then(Weak::upgrade)
}

fn alloc_node(deadline: Option<Instant>, parent: Option<Arc<CtxNode>>) -> RuntimeResult<Value> {
    let node = insert_node(deadline, parent, None);
    arm_deadline(&node)?;
    Ok(ctx_handle(node))
}

/// Builds a node under `parent` and registers it. A deadline is recorded
/// here and armed by [`arm_deadline`].
fn insert_node(
    deadline: Option<Instant>,
    parent: Option<Arc<CtxNode>>,
    follows: Option<gossamer_std::context::Context>,
) -> Arc<CtxNode> {
    let node = Arc::new(CtxNode {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        cancelled: AtomicBool::new(false),
        deadline,
        parent,
        follows,
        children: parking_lot::Mutex::new(StdHashMap::new()),
        chan: Channel::new(),
    });
    CTX_REGISTRY.lock().insert(node.id, Arc::downgrade(&node));
    if let Some(parent) = &node.parent {
        parent
            .children
            .lock()
            .insert(node.id, Arc::downgrade(&node));
        // A parent that finished cancelling before this link was made
        // never reaches the child through its own walk, so the child
        // takes the ancestry's state at birth.
        if node_is_cancelled(parent) {
            cancel_node(&node);
        }
    }
    node
}

/// Queues `node`'s deadline with the timer thread. A node that cannot be
/// armed is cancelled, so no context outlives a deadline nothing will
/// enforce.
fn arm_deadline(node: &Arc<CtxNode>) -> RuntimeResult<()> {
    let Some(deadline) = node.deadline else {
        return Ok(());
    };
    if let Err(e) = DEADLINES.start() {
        cancel_node(node);
        return Err(RuntimeError::Panic(format!(
            "context deadline: cannot start the timer thread: {e}"
        )));
    }
    DEADLINES.insert(deadline, node.id, &node.cancelled);
    Ok(())
}

/// Contexts belonging to requests currently in flight, by id.
///
/// Shutdown cancels every one, so a handler that watches its context
/// learns the process is going down at the same moment the accept loop
/// stops taking new work.
static LIVE_REQUESTS: parking_lot::Mutex<Vec<i64>> = parking_lot::Mutex::new(Vec::new());

/// Cancels every in-flight request's context.
pub(crate) fn cancel_live_requests() {
    for id in std::mem::take(&mut *LIVE_REQUESTS.lock()) {
        cancel_id(id);
    }
}

/// Allocates a request-scoped context with no deadline of its own and
/// answers `(value, id)`.
///
/// The id is what the server cancels with when the request ends; the
/// value is what the handler reads off `request.context`.
pub(crate) fn request_context(follows: Option<gossamer_std::context::Context>) -> (Value, i64) {
    let node = insert_node(None, None, follows);
    let id = node.id;
    LIVE_REQUESTS.lock().push(id);
    (ctx_handle(node), id)
}

/// [`request_context`] for a request served under a deadline of
/// `deadline_ms` milliseconds, or none when it is not positive.
pub(crate) fn timed_request_context(
    deadline_ms: i64,
    follows: Option<gossamer_std::context::Context>,
) -> RuntimeResult<(Value, i64)> {
    let deadline = (deadline_ms > 0)
        .then(|| Instant::now() + std::time::Duration::from_millis(deadline_ms as u64));
    let node = insert_node(deadline, None, follows);
    arm_deadline(&node)?;
    let id = node.id;
    LIVE_REQUESTS.lock().push(id);
    Ok((ctx_handle(node), id))
}

/// Cancels a context the server created, and every descendant.
pub(crate) fn cancel_request_context(id: i64) {
    if id >= 0 {
        LIVE_REQUESTS.lock().retain(|live| *live != id);
        cancel_id(id);
    }
}

pub(crate) fn builtin_ctx_background(_args: &[Value]) -> RuntimeResult<Value> {
    alloc_node(None, None)
}

pub(crate) fn builtin_ctx_with_cancel(args: &[Value]) -> RuntimeResult<Value> {
    let parent = args.first().and_then(node_of_value);
    alloc_node(None, parent)
}

pub(crate) fn builtin_ctx_with_timeout(args: &[Value]) -> RuntimeResult<Value> {
    let parent = args.first().and_then(node_of_value);
    let millis = args.get(1).and_then(value_to_int).unwrap_or(0);
    if millis < 0 {
        return Err(RuntimeError::Type(
            "Context::with_timeout: timeout_ms must be non-negative".to_string(),
        ));
    }
    let millis = u64::try_from(millis).map_err(|_| {
        RuntimeError::Type("Context::with_timeout: timeout_ms is too large".to_string())
    })?;
    let deadline = Instant::now() + Duration::from_millis(millis);
    alloc_node(Some(deadline), parent)
}

/// Cancels the context registered under `id`, if one is still held.
fn cancel_id(id: i64) {
    if let Some(node) = node_of_id(id) {
        cancel_node(&node);
    }
}

/// Cancels `root` and every descendant. The walk carries its own stack so a
/// deep context chain costs heap rather than call frames, and each child set
/// is copied out before its node is cancelled so no tree lock is held across
/// the cancel of a child.
fn cancel_node(root: &Arc<CtxNode>) {
    let mut pending = vec![Arc::clone(root)];
    while let Some(node) = pending.pop() {
        if node.cancelled.swap(true, Ordering::AcqRel) {
            continue;
        }
        if let Some(deadline) = node.deadline {
            DEADLINES.remove(deadline, node.id);
        }
        // Closing the done channel makes the node's `select` recv arm
        // ready; idempotent, so a repeated cancel is harmless.
        let _ = node.chan.close();
        let kids: Vec<Weak<CtxNode>> = node.children.lock().values().cloned().collect();
        pending.extend(kids.iter().filter_map(Weak::upgrade));
    }
}

pub(crate) fn builtin_ctx_cancel(args: &[Value]) -> RuntimeResult<Value> {
    if let Some(node) = args.first().and_then(node_of_value) {
        cancel_node(&node);
    }
    Ok(Value::Unit)
}

/// Whether `node` or any ancestor is cancelled or past its deadline. The
/// chain is walked iteratively so ancestry depth costs no call frames.
fn node_is_cancelled(node: &CtxNode) -> bool {
    let mut current = Some(node);
    while let Some(node) = current {
        if node.cancelled.load(Ordering::Acquire) {
            return true;
        }
        if node.follows.as_ref().is_some_and(|ctx| ctx.is_cancelled()) {
            return true;
        }
        if let Some(deadline) = node.deadline
            && Instant::now() >= deadline
        {
            return true;
        }
        current = node.parent.as_deref();
    }
    false
}

/// Returns whether a VM `Context` value has been cancelled or timed out.
/// Cancellation-aware VM primitives use this so they share the parent and
/// deadline semantics of `Context::is_cancelled`.
pub(crate) fn value_is_cancelled(value: &Value) -> bool {
    node_of_value(value).is_some_and(|node| node_is_cancelled(&node))
}

pub(crate) fn builtin_ctx_is_cancelled(args: &[Value]) -> RuntimeResult<Value> {
    let cancelled = args.first().is_some_and(value_is_cancelled);
    Ok(Value::Bool(cancelled))
}

pub(crate) fn builtin_ctx_done(args: &[Value]) -> RuntimeResult<Value> {
    builtin_ctx_is_cancelled(args)
}

/// `ctx.done_chan()` - the context's done channel as a `Receiver`.
/// `cancel` (this context or an ancestor) closes it, so a closed-channel
/// `select` recv arm fires on cancellation. Returns the same channel on
/// every call for a given context.
pub(crate) fn builtin_ctx_done_chan(args: &[Value]) -> RuntimeResult<Value> {
    match args.first().and_then(node_of_value) {
        Some(node) => {
            if node_is_cancelled(&node) {
                let _ = node.chan.close();
            }
            Ok(Value::Channel(node.chan.clone()))
        }
        // No context reports exactly one thing through this channel:
        // readiness. A closed channel is that.
        None => {
            let chan = Channel::new();
            let _ = chan.close();
            Ok(Value::Channel(chan))
        }
    }
}
