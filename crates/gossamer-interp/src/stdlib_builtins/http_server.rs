//! `std::http::Server` builtins - the configurable server object.
//!
//! The server's limits and its bound listener live in
//! `gossamer_runtime::c_abi::http_server_handle`, the same state the
//! compiled tiers' shims read, so a `Server` configured in Gossamer
//! applies the same budgets under `gos run` and in a native build. Only
//! the accept loop differs: the VM answers each request on a goroutine
//! through its own dispatcher, exactly as `http::serve` does.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use gossamer_std::http as http_std;

use crate::builtins::{BuiltinFnPub, as_str, err_variant, ok_variant, value_to_int};
use crate::value::{NativeDispatch, RuntimeError, RuntimeResult, Value};

/// One configured server on the VM tier.
struct VmServer {
    config: parking_lot::Mutex<http_std::server::Config>,
    /// How long one request's context lives before it is cancelled.
    request_timeout_ms: std::sync::atomic::AtomicI64,
    listener: parking_lot::Mutex<Option<std::net::TcpListener>>,
    bound_addr: parking_lot::Mutex<String>,
    shutdown: Arc<AtomicBool>,
    in_flight: Arc<AtomicUsize>,
}

fn registry() -> &'static parking_lot::Mutex<Vec<Arc<VmServer>>> {
    static REGISTRY: std::sync::OnceLock<parking_lot::Mutex<Vec<Arc<VmServer>>>> =
        std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| parking_lot::Mutex::new(Vec::new()))
}

fn server_at(handle: i64) -> Option<Arc<VmServer>> {
    let index = usize::try_from(handle).ok()?;
    registry().lock().get(index).map(Arc::clone)
}

/// The registry index inside a `Server` value.
///
/// The handle is a one-field struct rather than a bare integer so the VM's
/// method dispatch resolves `s.listen(..)` through the receiver's type
/// name; an integer receiver would reach `i64::listen` and then a bare
/// global of that name.
fn handle_of(args: &[Value]) -> i64 {
    match args.first() {
        Some(Value::Struct(inner)) => inner
            .fields
            .iter()
            .find(|(f, _)| (**f) == "__server")
            .and_then(|(_, v)| value_to_int(v))
            .unwrap_or(-1),
        other => other.and_then(value_to_int).unwrap_or(-1),
    }
}

/// The `Server` value carrying `handle`.
fn server_value(handle: i64) -> Value {
    Value::struct_("Server", vec![("__server", Value::Int(handle))])
}

/// Milliseconds, clamped at zero. A negative budget is not a shorter one.
fn millis(value: i64) -> Option<std::time::Duration> {
    let ms = u64::try_from(value.max(0)).unwrap_or(0);
    (ms != 0).then(|| std::time::Duration::from_millis(ms))
}

/// A byte or connection budget, clamped at zero.
fn budget(value: i64) -> usize {
    usize::try_from(value.max(0)).unwrap_or(usize::MAX)
}

/// Registers the `http::Server` builtins.
pub(crate) fn install_http_server(globals: &mut Vec<(&'static str, Value)>) {
    let methods: &[(&str, BuiltinFnPub)] = &[
        ("read_header_timeout_ms", builtin_read_header_timeout_ms),
        ("read_body_timeout_ms", builtin_read_body_timeout_ms),
        ("write_timeout_ms", builtin_write_timeout_ms),
        ("idle_timeout_ms", builtin_idle_timeout_ms),
        ("max_header_bytes", builtin_max_header_bytes),
        ("max_body_bytes", builtin_max_body_bytes),
        ("max_connections", builtin_max_connections),
        ("request_timeout_ms", builtin_request_timeout_ms),
        ("server_name", builtin_server_name),
        ("listen", builtin_listen),
        ("addr", builtin_addr),
        ("shutdown", builtin_shutdown),
        ("new", builtin_new),
    ];
    // Both dispatch keys, matching the router: a method call resolves
    // through the receiver's type name, and a path call through the
    // module-qualified one.
    let mut entries: Vec<(&'static str, BuiltinFnPub)> = Vec::with_capacity(methods.len() * 2);
    for &(method, call) in methods {
        entries.push((
            Box::leak(format!("Server::{method}").into_boxed_str()),
            call,
        ));
        entries.push((
            Box::leak(format!("http::Server::{method}").into_boxed_str()),
            call,
        ));
    }
    for (name, call) in entries {
        globals.push((name, crate::builtins::builtin_pub(name, call)));
    }
}

fn builtin_new(_args: &[Value]) -> RuntimeResult<Value> {
    let mut servers = registry().lock();
    servers.push(Arc::new(VmServer {
        config: parking_lot::Mutex::new(http_std::server::Config::default()),
        request_timeout_ms: std::sync::atomic::AtomicI64::new(0),
        listener: parking_lot::Mutex::new(None),
        bound_addr: parking_lot::Mutex::new(String::new()),
        shutdown: Arc::new(AtomicBool::new(false)),
        in_flight: Arc::new(AtomicUsize::new(0)),
    }));
    Ok(server_value(i64::try_from(servers.len() - 1).unwrap_or(-1)))
}

/// Applies one limit and answers the server, so the setters chain.
fn set_limit(
    args: &[Value],
    apply: impl FnOnce(&mut http_std::server::Config, i64),
) -> RuntimeResult<Value> {
    let handle = handle_of(args);
    let value = args.get(1).and_then(value_to_int).unwrap_or(0);
    if let Some(server) = server_at(handle) {
        apply(&mut server.config.lock(), value);
    }
    Ok(server_value(handle))
}

fn builtin_read_header_timeout_ms(args: &[Value]) -> RuntimeResult<Value> {
    set_limit(args, |c, v| c.read_header_timeout = millis(v))
}

fn builtin_read_body_timeout_ms(args: &[Value]) -> RuntimeResult<Value> {
    set_limit(args, |c, v| c.read_body_timeout = millis(v))
}

fn builtin_write_timeout_ms(args: &[Value]) -> RuntimeResult<Value> {
    set_limit(args, |c, v| c.write_timeout = millis(v))
}

fn builtin_idle_timeout_ms(args: &[Value]) -> RuntimeResult<Value> {
    set_limit(args, |c, v| c.idle_timeout = millis(v))
}

fn builtin_max_header_bytes(args: &[Value]) -> RuntimeResult<Value> {
    set_limit(args, |c, v| c.max_header_bytes = budget(v))
}

fn builtin_max_body_bytes(args: &[Value]) -> RuntimeResult<Value> {
    set_limit(args, |c, v| c.max_body_bytes = budget(v))
}

fn builtin_max_connections(args: &[Value]) -> RuntimeResult<Value> {
    set_limit(args, |c, v| c.max_connections = budget(v).max(1))
}

fn builtin_request_timeout_ms(args: &[Value]) -> RuntimeResult<Value> {
    let handle = handle_of(args);
    let ms = args.get(1).and_then(value_to_int).unwrap_or(0).max(0);
    if let Some(server) = server_at(handle) {
        server.request_timeout_ms.store(ms, Ordering::Release);
    }
    Ok(server_value(handle))
}

fn builtin_server_name(args: &[Value]) -> RuntimeResult<Value> {
    let handle = handle_of(args);
    let name = as_str(args.get(1).unwrap_or(&Value::Unit))
        .unwrap_or("")
        .to_string();
    if let Some(server) = server_at(handle) {
        server.config.lock().server_name = (!name.is_empty()).then_some(name);
    }
    Ok(server_value(handle))
}

fn builtin_listen(args: &[Value]) -> RuntimeResult<Value> {
    let handle = handle_of(args);
    let addr = as_str(args.get(1).unwrap_or(&Value::Unit))
        .unwrap_or("0.0.0.0:8080")
        .to_string();
    let Some(server) = server_at(handle) else {
        return Ok(err_variant("http::Server::listen: stale server handle"));
    };
    match gossamer_runtime::listen::bind_tcp(&addr) {
        Ok(listener) => {
            let bound = listener
                .local_addr()
                .map_or_else(|_| addr.clone(), |a| a.to_string());
            *server.bound_addr.lock() = bound;
            *server.listener.lock() = Some(listener);
            Ok(ok_variant(Value::Unit))
        }
        Err(e) => Ok(err_variant(format!("http::Server::listen: {e}"))),
    }
}

fn builtin_addr(args: &[Value]) -> RuntimeResult<Value> {
    let text = server_at(handle_of(args)).map_or_else(String::new, |s| s.bound_addr.lock().clone());
    Ok(Value::String(text.into()))
}

fn builtin_shutdown(args: &[Value]) -> RuntimeResult<Value> {
    let handle = handle_of(args);
    let deadline_ms = args.get(1).and_then(value_to_int).unwrap_or(0).max(0);
    let Some(server) = server_at(handle) else {
        return Ok(Value::Bool(false));
    };
    server.shutdown.store(true, Ordering::Release);
    server.config.lock().shutdown.store(true, Ordering::Release);
    // The acceptor is blocked in `accept()`; a self-connect wakes it so it
    // observes the flag instead of waiting for the next client.
    let addr = server.bound_addr.lock().clone();
    if let Ok(sock) = addr.parse::<std::net::SocketAddr>() {
        let _ = std::net::TcpStream::connect_timeout(&sock, std::time::Duration::from_millis(200));
    }
    let deadline = gossamer_runtime::platform::Instant::now()
        + std::time::Duration::from_millis(deadline_ms as u64);
    while server.in_flight.load(Ordering::Acquire) > 0 {
        if gossamer_runtime::platform::Instant::now() >= deadline {
            return Ok(Value::Bool(false));
        }
        gossamer_runtime::platform::sleep(std::time::Duration::from_millis(5));
    }
    Ok(Value::Bool(true))
}

/// `server.serve(handler) -> Result<(), errors::Error>` - accepts on the
/// listener `listen` bound, answering each request on its own goroutine.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn native_http_server_serve(
    dispatch: &mut dyn NativeDispatch,
    args: &[Value],
) -> RuntimeResult<Value> {
    if args.len() < 2 {
        return Err(RuntimeError::Arity {
            expected: 2,
            found: args.len(),
        });
    }
    let handle = handle_of(args);
    let handler = args[1].clone();
    let Some(server) = server_at(handle) else {
        return Ok(err_variant("http::Server::serve: stale server handle"));
    };
    let Some(listener) = server.listener.lock().take() else {
        return Ok(err_variant("http::Server::serve: call listen(addr) first"));
    };
    let config = server.config.lock().clone();
    let request_timeout_ms = server.request_timeout_ms.load(Ordering::Acquire);
    gossamer_runtime::c_abi::lifecycle::register_shutdown_flag(&config.shutdown);
    let in_flight = Arc::clone(&server.in_flight);
    let (target, leading) = crate::value::SpawnTarget::for_handler(&handler);
    let result = http_std::server::run_dispatch(listener, &config, |request, sink| {
        let method = request.method.as_str().to_string();
        let path = request.path.clone();
        let (context, context_id) = crate::stdlib_builtins::context::request_context(
            request_timeout_ms,
            Some(request.context.clone()),
        );
        let mut call_args = leading.clone();
        call_args.push(crate::builtins::request_to_value_with_context(
            &request, context,
        ));
        in_flight.fetch_add(1, Ordering::AcqRel);
        let done = Arc::clone(&in_flight);
        dispatch.spawn_with_outcome(
            target.clone(),
            call_args,
            Box::new(move |outcome| {
                sink.send(crate::builtins::handler_outcome_to_response(
                    outcome, &method, &path,
                ));
                crate::stdlib_builtins::context::cancel_request_context(context_id);
                done.fetch_sub(1, Ordering::AcqRel);
            }),
        );
    });
    match result {
        Ok(()) => Ok(ok_variant(Value::Unit)),
        Err(e) => Ok(err_variant(format!("http::Server::serve: {e}"))),
    }
}
