#![allow(
    unused_imports,
    dead_code,
    unreachable_pub,
    missing_docs,
    clippy::wildcard_imports,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::similar_names,
    clippy::many_single_char_names,
    clippy::items_after_statements,
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::option_if_let_else,
    clippy::match_same_arms,
    clippy::if_not_else,
    clippy::single_match_else,
    clippy::needless_pass_by_value,
    clippy::manual_let_else,
    clippy::redundant_else,
    clippy::collapsible_if,
    clippy::collapsible_else_if,
    clippy::map_unwrap_or,
    clippy::struct_excessive_bools,
    clippy::module_name_repetitions,
    clippy::unnecessary_wraps,
    clippy::large_enum_variant,
    clippy::if_same_then_else,
    clippy::single_match,
    clippy::useless_conversion,
    clippy::needless_borrows_for_generic_args,
    clippy::let_and_return,
    unsafe_op_in_unsafe_fn,
    unsafe_code,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks,
    clippy::needless_collect,
    clippy::elidable_lifetime_names,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::missing_const_for_fn,
    clippy::needless_range_loop,
    clippy::cognitive_complexity,
    clippy::unused_io_amount,
    clippy::ptr_arg,
    clippy::ptr_as_ptr,
    clippy::redundant_closure,
    clippy::redundant_closure_for_method_calls,
    clippy::semicolon_if_nothing_returned,
    clippy::single_call_fn,
    clippy::unused_self,
    clippy::range_plus_one,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::cast_ptr_alignment,
    clippy::manual_assert,
    clippy::manual_string_new,
    clippy::match_bool,
    clippy::nonminimal_bool,
    clippy::redundant_pattern_matching,
    clippy::useless_let_if_seq
)]
//! Wires up Gossamer-callable builtins for stdlib modules whose
//! Rust-side implementation already exists but had no user-facing
//! exposure. Each `install_*` helper is invoked from
//! `builtins::install` so user code that writes
//! `strings::join`, `strconv::parse_i64`, `net::TcpStream::connect`,
//! `time::Instant::now`, etc. resolves to a real callable.
//!
//! All builtins return a `Result`-shaped variant (`Ok` / `Err`) on
//! fallible operations so callers can chain `?` without wrapping.

use std::cell::RefCell;
use std::collections::HashMap as StdHashMap;
use std::io::Read as IoRead;
use std::sync::Arc;

use gossamer_ast::Ident;
use std::sync::atomic::{AtomicBool as StdAtomicBool, AtomicI64 as StdAtomicI64, Ordering};

use crate::value::SmolStr;

use gossamer_std::bufio as bufio_std;
use gossamer_std::math as math_std;
#[cfg(not(target_arch = "wasm32"))]
use gossamer_std::net as net_std;
use gossamer_std::os as os_std;
use gossamer_std::path as path_std;
use gossamer_std::strconv as strconv_std;
use gossamer_std::strings as strings_std;
use gossamer_std::unicode as unicode_std;
use gossamer_std::utf8 as utf8_std;

use gossamer_std::iter as iter_std;
use gossamer_std::utf16 as utf16_std;

use crate::builtins::{
    BuiltinFnPub, as_str, err_variant, install_module_pub, none_variant, ok_variant, some_variant,
    value_to_int,
};
use crate::value::{MapKey, NativeCall, NativeDispatch, RuntimeResult, Value};

/// Entry point invoked from `builtins::install`.
use super::*;

pub(crate) fn install_time_extras(globals: &mut Vec<(&'static str, Value)>) {
    install_module_pub(
        "time",
        &[
            ("now_nanos", builtin_time_now_nanos),
            ("monotonic_ms", builtin_time_monotonic_ms),
            ("monotonic_nanos", builtin_time_monotonic_nanos),
            ("since_ms", builtin_time_since_ms),
            ("Instant::now", builtin_time_instant_now),
            ("Instant::elapsed_ms", builtin_time_instant_elapsed_ms),
            ("Instant::elapsed", builtin_time_instant_elapsed),
            (
                "Instant::duration_since",
                builtin_time_instant_duration_since,
            ),
            ("Duration::from_nanos", builtin_time_duration_from_nanos),
            ("Duration::from_micros", builtin_time_duration_from_micros),
            ("Duration::from_millis", builtin_time_duration_from_millis),
            ("Duration::from_secs", builtin_time_duration_from_secs),
            (
                "Duration::from_secs_f64",
                builtin_time_duration_from_secs_f64,
            ),
            ("Duration::as_nanos", builtin_time_duration_as_nanos),
            ("Duration::as_micros", builtin_time_duration_as_micros),
            ("Duration::as_millis", builtin_time_duration_as_millis),
            ("Duration::as_secs", builtin_time_duration_as_secs),
            ("Duration::as_secs_f64", builtin_time_duration_as_secs_f64),
            (
                "__sleep_ns",
                super::time_completeness::builtin_time_sleep_ns,
            ),
            ("__sleep_ns_ctx", crate::builtins::builtin_time_sleep_ns_ctx),
        ],
        globals,
    );
    globals.push((
        "Instant::now",
        crate::builtins::builtin_pub("Instant::now", builtin_time_instant_now),
    ));
    globals.push((
        "elapsed_ms",
        crate::builtins::builtin_pub("elapsed_ms", builtin_time_instant_elapsed_ms),
    ));
}

pub(crate) fn builtin_time_now_nanos(_args: &[Value]) -> RuntimeResult<Value> {
    let nanos = gossamer_runtime::platform::system_time_now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    Ok(Value::Int(i64::try_from(nanos).unwrap_or(i64::MAX)))
}

thread_local! {
    pub(crate) static MONOTONIC_BASE: std::cell::OnceCell<gossamer_runtime::platform::Instant> = const { std::cell::OnceCell::new() };
}

pub(crate) fn monotonic_base() -> gossamer_runtime::platform::Instant {
    MONOTONIC_BASE.with(|cell| *cell.get_or_init(gossamer_runtime::platform::Instant::now))
}

pub(crate) fn builtin_time_monotonic_ms(_args: &[Value]) -> RuntimeResult<Value> {
    let dur = monotonic_base().elapsed();
    Ok(Value::Int(
        i64::try_from(dur.as_millis()).unwrap_or(i64::MAX),
    ))
}

pub(crate) fn builtin_time_monotonic_nanos(_args: &[Value]) -> RuntimeResult<Value> {
    let dur = monotonic_base().elapsed();
    Ok(Value::Int(
        i64::try_from(dur.as_nanos()).unwrap_or(i64::MAX),
    ))
}

pub(crate) fn builtin_time_since_ms(args: &[Value]) -> RuntimeResult<Value> {
    let start = args.first().and_then(value_to_int).unwrap_or(0);
    let now = i64::try_from(monotonic_base().elapsed().as_millis()).unwrap_or(i64::MAX);
    Ok(Value::Int(now.saturating_sub(start)))
}

// `time::Duration` is a count of nanoseconds and `time::Instant` a reading of
// the monotonic clock in nanoseconds, the representation the compiled tiers'
// `gos_rt_duration_*` / `gos_rt_instant_*` helpers share; each builtin is the
// helper itself.

fn int_arg(args: &[Value], index: usize) -> i64 {
    args.get(index).and_then(value_to_int).unwrap_or(0)
}

fn float_arg(args: &[Value], index: usize) -> f64 {
    match args.get(index) {
        Some(Value::Float(f)) => *f,
        Some(other) => value_to_int(other).map_or(0.0, |n| n as f64),
        None => 0.0,
    }
}

pub(crate) fn builtin_time_instant_now(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Int(unsafe {
        gossamer_runtime::c_abi::time::gos_rt_instant_now()
    }))
}

pub(crate) fn builtin_time_instant_elapsed_ms(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Int(unsafe {
        gossamer_runtime::c_abi::time::gos_rt_instant_elapsed_ms(int_arg(args, 0))
    }))
}

pub(crate) fn builtin_time_instant_elapsed(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Int(unsafe {
        gossamer_runtime::c_abi::time::gos_rt_instant_elapsed(int_arg(args, 0))
    }))
}

pub(crate) fn builtin_time_instant_duration_since(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Int(
        gossamer_runtime::c_abi::time::gos_rt_instant_duration_since(
            int_arg(args, 0),
            int_arg(args, 1),
        ),
    ))
}

pub(crate) fn builtin_time_duration_from_nanos(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Int(
        gossamer_runtime::c_abi::time::gos_rt_duration_from_nanos(int_arg(args, 0)),
    ))
}

pub(crate) fn builtin_time_duration_from_micros(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Int(
        gossamer_runtime::c_abi::time::gos_rt_duration_from_micros(int_arg(args, 0)),
    ))
}

pub(crate) fn builtin_time_duration_from_millis(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Int(
        gossamer_runtime::c_abi::time::gos_rt_duration_from_millis(int_arg(args, 0)),
    ))
}

pub(crate) fn builtin_time_duration_from_secs(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Int(
        gossamer_runtime::c_abi::time::gos_rt_duration_from_secs(int_arg(args, 0)),
    ))
}

pub(crate) fn builtin_time_duration_from_secs_f64(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Int(
        gossamer_runtime::c_abi::time::gos_rt_duration_from_secs_f64(float_arg(args, 0)),
    ))
}

pub(crate) fn builtin_time_duration_as_nanos(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Int(
        gossamer_runtime::c_abi::time::gos_rt_duration_as_nanos(int_arg(args, 0)),
    ))
}

pub(crate) fn builtin_time_duration_as_micros(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Int(
        gossamer_runtime::c_abi::time::gos_rt_duration_as_micros(int_arg(args, 0)),
    ))
}

pub(crate) fn builtin_time_duration_as_millis(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Int(
        gossamer_runtime::c_abi::time::gos_rt_duration_as_millis(int_arg(args, 0)),
    ))
}

pub(crate) fn builtin_time_duration_as_secs(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Int(
        gossamer_runtime::c_abi::time::gos_rt_duration_as_secs(int_arg(args, 0)),
    ))
}

pub(crate) fn builtin_time_duration_as_secs_f64(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Float(
        gossamer_runtime::c_abi::time::gos_rt_duration_as_secs_f64(int_arg(args, 0)),
    ))
}

// ----------------------------------------------------------------------
// net (TCP listener / stream + UDP socket + DNS)
//
// Sockets are referred to from Gossamer code via opaque handle values
// (`net::TcpStream` / `net::TcpListener` / `net::UdpSocket` structs
// holding a __handle: i64). The Rust-side socket lives in a
// process-global registry keyed by handle id.
//
// Process-global (not `thread_local!`): goroutines run on an OS
// worker-thread pool, so a socket handle minted on one worker must
// resolve on another after the goroutine migrates between workers. A
// `thread_local!` registry silently lost every cross-goroutine update.
// Mirrors the set/deque/sync registries.
//
// The lock discipline is what keeps this global registry deadlock-free:
// each socket is held behind its own `Arc<parking_lot::Mutex<_>>`, and
// the registry mutex is held only for the O(1) map lookup that clones
// the `Arc` out. The (possibly blocking) I/O then runs under the
// per-socket mutex alone, never under the registry mutex. So when the
// VM scheduler parks a goroutine inside a blocking `read` / `accept` /
// `recv_from` (and its worker thread moves on to another goroutine),
// only that one socket's mutex is held - the registry stays free for
// every other socket access. The per-socket mutex serializes concurrent
// ops on the same socket, which is correct. The stream registries wrap
// the socket in `Option<_>` so `start_tls` can move the plaintext stream
// out by value and `close` can drop it, modelling close idempotently.
pub(crate) static NEXT_NET_ID: GlobalReg<i64> =
    GlobalReg::new(|| parking_lot::ReentrantMutex::new(RefCell::new(1)));
#[cfg(not(target_arch = "wasm32"))]
pub(crate) static TCP_STREAM_REGISTRY: GlobalReg<
    StdHashMap<i64, Arc<parking_lot::Mutex<Option<net_std::TcpStream>>>>,
> = GlobalReg::new(|| parking_lot::ReentrantMutex::new(RefCell::new(StdHashMap::new())));
#[cfg(not(target_arch = "wasm32"))]
pub(crate) static TLS_STREAM_REGISTRY: GlobalReg<
    StdHashMap<i64, Arc<parking_lot::Mutex<net_std::TlsStream>>>,
> = GlobalReg::new(|| parking_lot::ReentrantMutex::new(RefCell::new(StdHashMap::new())));
#[cfg(not(target_arch = "wasm32"))]
pub(crate) static TCP_LISTENER_REGISTRY: GlobalReg<
    StdHashMap<i64, Arc<parking_lot::Mutex<net_std::TcpListener>>>,
> = GlobalReg::new(|| parking_lot::ReentrantMutex::new(RefCell::new(StdHashMap::new())));
#[cfg(not(target_arch = "wasm32"))]
pub(crate) static UDP_REGISTRY: GlobalReg<
    StdHashMap<i64, Arc<parking_lot::Mutex<net_std::UdpSocket>>>,
> = GlobalReg::new(|| parking_lot::ReentrantMutex::new(RefCell::new(StdHashMap::new())));
#[cfg(unix)]
pub(crate) static UNIX_STREAM_REGISTRY: GlobalReg<
    StdHashMap<i64, Arc<parking_lot::Mutex<std::os::unix::net::UnixStream>>>,
> = GlobalReg::new(|| parking_lot::ReentrantMutex::new(RefCell::new(StdHashMap::new())));
#[cfg(unix)]
pub(crate) static UNIX_LISTENER_REGISTRY: GlobalReg<
    StdHashMap<i64, Arc<parking_lot::Mutex<std::os::unix::net::UnixListener>>>,
> = GlobalReg::new(|| parking_lot::ReentrantMutex::new(RefCell::new(StdHashMap::new())));

pub(crate) fn next_net_id() -> i64 {
    NEXT_NET_ID.with(|c| {
        let mut v = c.borrow_mut();
        let id = *v;
        *v += 1;
        id
    })
}

/// Clones out the per-socket `Arc` for `id` under a brief registry-lock,
/// releasing the global registry lock before any blocking I/O runs.
pub(crate) fn fetch_socket<T: 'static>(
    reg: &GlobalReg<StdHashMap<i64, Arc<parking_lot::Mutex<T>>>>,
    id: i64,
) -> Option<Arc<parking_lot::Mutex<T>>> {
    reg.with(|r| r.borrow().get(&id).cloned())
}

pub(crate) fn handle_struct(name: &'static str, id: i64) -> Value {
    Value::struct_(
        name,
        Arc::unwrap_or_clone(Arc::new(vec![("__handle", Value::Int(id))])),
    )
}

pub(crate) fn handle_id(value: &Value) -> Option<i64> {
    if let Value::Struct(inner) = value {
        for (ident, v) in &inner.fields {
            if (*ident) == "__handle" {
                if let Value::Int(n) = v {
                    return Some(*n);
                }
            }
        }
    }
    None
}
