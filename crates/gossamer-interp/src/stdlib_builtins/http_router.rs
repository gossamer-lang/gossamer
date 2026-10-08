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

// Router: route registration and pattern lookup are pure computation
// (no sockets), so this module builds on every target including wasm32;
// only the serving dispatch (`http::serve`) is socket-bound and stays
// gated.
//
// A router handle holds its table, shared by every copy of the handle and
// by a server serving it, so the routes and their handlers live exactly as
// long as something can still dispatch to them, on whichever worker thread
// that happens.

/// One route: the method it answers (empty for any), its pattern, and the
/// handler it dispatches to (`Unit` for a route `router::add` registered
/// without one).
pub(crate) struct Route {
    method: String,
    pattern: String,
    handler: Value,
}

#[derive(Default)]
pub(crate) struct RouterTable {
    pub(crate) routes: Vec<Route>,
}

type SharedTable = parking_lot::Mutex<RouterTable>;

// Static router-builtin tables are kept at module scope rather than
// inside `install_http_router` so clippy's `items-after-statements`
// doesn't fire - and so we never need `Box::leak(format!(…))` for
// what is conceptually a static lookup. The previous shape leaked
// process-lifetime strings on first install, which leaksanitizer
// flagged on fuzz-target exit.
pub(crate) const ROUTER_FREE_FNS: &[(&str, &str, BuiltinFnPub)] = &[
    ("http::router::new", "router::new", builtin_router_new),
    ("http::router::add", "router::add", builtin_router_add),
    (
        "http::router::lookup",
        "router::lookup",
        builtin_router_lookup,
    ),
];

pub(crate) const ROUTER_METHODS: &[(&str, BuiltinFnPub)] = &[
    ("Router::get", builtin_router_method_get),
    ("Router::post", builtin_router_method_post),
    ("Router::put", builtin_router_method_put),
    ("Router::delete", builtin_router_method_delete),
    ("Router::patch", builtin_router_method_patch),
    ("Router::head", builtin_router_method_head),
    ("Router::options", builtin_router_method_options),
];

// Module-local (not the `http_sse` glob export): `http_sse` is gated
// off wasm32 while this module is not.
fn arg_str(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.as_str().to_string(),
        _ => String::new(),
    }
}

pub(crate) fn install_http_router(globals: &mut Vec<(&'static str, Value)>) {
    for &(qualified, short, call) in ROUTER_FREE_FNS {
        globals.push((qualified, crate::builtins::builtin_pub(qualified, call)));
        globals.push((short, crate::builtins::builtin_pub(short, call)));
    }
    // Constructor aliases that mirror the compiled-tier surface
    // (Gossamer source: `let r = router::Router::new()`).
    for alias in [
        "router::Router::new",
        "http::router::Router::new",
        "Router::new",
    ] {
        globals.push((
            alias,
            crate::builtins::builtin_pub(alias, builtin_router_new as BuiltinFnPub),
        ));
    }
    // Method-style add/get/post/etc. So `r.get(pattern, handler)`
    // resolves to `Router::get(r, pattern, handler)` which stores
    // the route + a Value-handler in the registry. Dispatch via
    // http::serve goes through Router::serve below.
    for &(qualified, call) in ROUTER_METHODS {
        globals.push((qualified, crate::builtins::builtin_pub(qualified, call)));
    }
    // `r.path_value("id")` on a server Request reads the router's path
    // captures. Registered for both the bare and `http::`-qualified
    // dispatch keys.
    for key in ["Request::path_value", "http::Request::path_value"] {
        globals.push((
            key,
            crate::builtins::builtin_pub(key, builtin_request_path_value as BuiltinFnPub),
        ));
    }
    for key in ["Request::path_int", "http::Request::path_int"] {
        globals.push((
            key,
            crate::builtins::builtin_pub(key, builtin_request_path_int as BuiltinFnPub),
        ));
    }
    for key in ["Request::path_float", "http::Request::path_float"] {
        globals.push((
            key,
            crate::builtins::builtin_pub(key, builtin_request_path_float as BuiltinFnPub),
        ));
    }
}

pub(crate) fn router_method_add(verb: &'static str, args: &[Value]) -> RuntimeResult<Value> {
    let Some(table) = args.first().and_then(router_of) else {
        return Ok(err_variant("Router method: first arg must be a Router"));
    };
    table.lock().routes.push(Route {
        method: verb.to_string(),
        pattern: arg_str(args.get(1)),
        handler: args.get(2).cloned().unwrap_or(Value::Unit),
    });
    Ok(args.first().cloned().unwrap_or(Value::Unit))
}

pub(crate) fn builtin_router_method_get(args: &[Value]) -> RuntimeResult<Value> {
    router_method_add("GET", args)
}
pub(crate) fn builtin_router_method_post(args: &[Value]) -> RuntimeResult<Value> {
    router_method_add("POST", args)
}
pub(crate) fn builtin_router_method_put(args: &[Value]) -> RuntimeResult<Value> {
    router_method_add("PUT", args)
}
pub(crate) fn builtin_router_method_delete(args: &[Value]) -> RuntimeResult<Value> {
    router_method_add("DELETE", args)
}
pub(crate) fn builtin_router_method_patch(args: &[Value]) -> RuntimeResult<Value> {
    router_method_add("PATCH", args)
}
pub(crate) fn builtin_router_method_head(args: &[Value]) -> RuntimeResult<Value> {
    router_method_add("HEAD", args)
}
pub(crate) fn builtin_router_method_options(args: &[Value]) -> RuntimeResult<Value> {
    router_method_add("OPTIONS", args)
}

/// `Router::serve(router, request)` - invoked by `http::serve`'s
/// dispatch loop when the handler is a Router. Walks the route
/// table, finds the first match for the request's (method, path),
/// and invokes the stored handler via `NativeDispatch::call_fn`
/// so the user's `fn serve(&self, req) -> Result<...>` runs with
/// the right receiver. Returns 404 when no route matches.
pub(crate) fn native_router_serve(
    dispatch: &mut dyn crate::value::NativeDispatch,
    args: &[Value],
) -> RuntimeResult<Value> {
    let Some(table) = args.first().and_then(router_of) else {
        return Ok(ok_variant(http_404_response()));
    };
    let request = args.get(1).cloned().unwrap_or(Value::Unit);
    // Extract method + path from the Request value.
    let (method, path) = request_method_and_path(&request);
    // Find a matching route index and its captured path params in one
    // pass, so selection and capture use identical matcher semantics.
    // The handler is cloned out so the table's lock is not held while the
    // handler runs, which may itself add routes.
    let matched = table.lock().routes.iter().find_map(|route| {
        if route.method.is_empty() || route.method.eq_ignore_ascii_case(&method) {
            pattern_captures(&route.pattern, &path).map(|caps| (route.handler.clone(), caps))
        } else {
            None
        }
    });
    let Some((handler, captures)) = matched else {
        return Ok(ok_variant(http_404_response()));
    };
    // Attach captures to the request so the handler can read them via
    // `r.path_value("id")`; mirrors the compiled tier writing
    // `GosHttpRequest.params` in `gos_rt_router_serve`.
    let request = inject_path_params(request, &captures);
    // Struct handlers route through `{StructName}::serve(handler, request)`
    // because the bare "serve" name is shared across every impl and gets
    // overwritten as more types load. Any other shape (Closure, Builtin,
    // Native, or the bytecode VM's `Value::String` fn-name surrogate)
    // calls the handler directly with the request as its sole argument.
    match &handler {
        Value::Struct(inner) => {
            let method_name = format!("{}::serve", inner.name);
            dispatch.call_fn(&method_name, vec![handler.clone(), request])
        }
        // A route handler is declared `Fn(Request, Params)`, so a closure that
        // spells both parameters is called with both. The captures already ride
        // on the request - `r.path_value(name)` is how a handler reads them, on
        // this tier and the compiled one - so the second argument carries them
        // for the signature's sake. A handler that declares only the request
        // (a `serve`-shaped one) still gets one argument.
        _ => {
            let args = if handler_wants_params(&handler) {
                vec![request, params_value(&captures)]
            } else {
                vec![request]
            };
            dispatch.call_value(&handler, args)
        }
    }
}

/// Whether a route handler declares the `Params` parameter beside the request.
///
/// A closure's chunk counts its captured upvalues among its registers, so the
/// declared parameters are what is left once the captures are taken off.
fn handler_wants_params(handler: &Value) -> bool {
    match handler {
        Value::Closure(c) => usize::from(c.chunk.arity).saturating_sub(c.capture_values.len()) >= 2,
        _ => false,
    }
}

/// The captured path parameters, as the `Params` value a handler declares.
fn params_value(captures: &[(String, String)]) -> Value {
    let entries: Vec<Value> = captures
        .iter()
        .map(|(k, v)| {
            Value::Array(Arc::new(vec![
                Value::String(k.clone().into()),
                Value::String(v.clone().into()),
            ]))
        })
        .collect();
    Value::struct_(
        "Params",
        vec![("captures", Value::Array(Arc::new(entries)))],
    )
}

pub(crate) fn http_404_response() -> Value {
    let mut fields = vec![
        ("status", Value::Int(404)),
        ("body", Value::String("not found".into())),
    ];
    fields.push(("headers", Value::Array(Arc::new(Vec::new()))));
    Value::struct_("Response", Arc::unwrap_or_clone(Arc::new(fields)))
}

pub(crate) fn request_method_and_path(v: &Value) -> (String, String) {
    let mut method = String::new();
    let mut path = String::new();
    if let Value::Struct(inner) = v {
        for (i, val) in &inner.fields {
            match ((*i), val) {
                ("method", Value::String(s)) => method = s.as_str().to_string(),
                ("path", Value::String(s)) => path = s.as_str().to_string(),
                _ => {}
            }
        }
    }
    (method, path)
}

/// Rebuild a `Request` struct value with a hidden `__params` field
/// carrying the router's path captures. The handler reads them back
/// through `Request::path_value`.
fn inject_path_params(request: Value, captures: &[(String, String)]) -> Value {
    let Value::Struct(inner) = &request else {
        return request;
    };
    let mut fields: Vec<(&'static str, Value)> = inner.fields.to_vec();
    let params: Vec<Value> = captures
        .iter()
        .map(|(k, v)| {
            Value::Tuple(Arc::from(vec![
                Value::String(SmolStr::from(k.as_str())),
                Value::String(SmolStr::from(v.as_str())),
            ]))
        })
        .collect();
    fields.push(("__params", Value::Array(Arc::new(params))));
    Value::struct_(inner.name.clone(), fields)
}

/// Look up a router-captured path parameter on a server Request value
/// (the hidden `__params` field) by name.
fn path_param_str(args: &[Value]) -> Option<String> {
    let name = arg_str(args.get(1));
    let Some(Value::Struct(inner)) = args.first() else {
        return None;
    };
    for (field, val) in &inner.fields {
        if (*field) == "__params" {
            if let Value::Array(items) = val {
                for item in items.iter() {
                    if let Value::Tuple(t) = item {
                        if let (Some(Value::String(k)), Some(Value::String(v))) =
                            (t.first(), t.get(1))
                        {
                            if k.as_str() == name.as_str() {
                                return Some(v.as_str().to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

/// `Request::path_value(req, name) -> String` - the router-captured
/// path parameter, or `""` when absent. Matches the compiled
/// `gos_rt_http_request_path_value` shim.
pub(crate) fn builtin_request_path_value(args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::String(SmolStr::from(
        path_param_str(args).unwrap_or_default(),
    )))
}

/// `Request::path_int(req, name) -> Option<i64>` - typed extractor for
/// a numeric path segment. `None` when absent or not an integer.
pub(crate) fn builtin_request_path_int(args: &[Value]) -> RuntimeResult<Value> {
    match path_param_str(args).and_then(|s| s.trim().parse::<i64>().ok()) {
        Some(n) => Ok(some_variant(Value::Int(n))),
        None => Ok(none_variant()),
    }
}

/// `Request::path_float(req, name) -> Option<f64>` - typed extractor.
/// `None` when absent or unparseable.
pub(crate) fn builtin_request_path_float(args: &[Value]) -> RuntimeResult<Value> {
    match path_param_str(args).and_then(|s| s.trim().parse::<f64>().ok()) {
        Some(n) => Ok(some_variant(Value::Float(n))),
        None => Ok(none_variant()),
    }
}

pub(crate) fn builtin_router_new(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(crate::value::state_handle(
        "Router",
        "__router",
        Arc::new(SharedTable::default()),
    ))
}

/// The table a router handle holds.
pub(crate) fn router_of(v: &Value) -> Option<Arc<SharedTable>> {
    crate::value::handle_state(v, "Router", "__router")
}

pub(crate) fn builtin_router_add(args: &[Value]) -> RuntimeResult<Value> {
    let Some(table) = args.first().and_then(router_of) else {
        return Ok(err_variant("router::add: first arg must be a Router"));
    };
    table.lock().routes.push(Route {
        method: arg_str(args.get(1)).to_ascii_uppercase(),
        pattern: arg_str(args.get(2)),
        handler: Value::Unit,
    });
    Ok(ok_variant(Value::Unit))
}

pub(crate) fn builtin_router_lookup(args: &[Value]) -> RuntimeResult<Value> {
    let Some(table) = args.first().and_then(router_of) else {
        return Ok(none_variant());
    };
    let method = arg_str(args.get(1)).to_ascii_uppercase();
    let path = arg_str(args.get(2));
    let matched = table.lock().routes.iter().position(|route| {
        (route.method.is_empty() || route.method == method)
            && pattern_matches(&route.pattern, &path)
    });
    match matched {
        Some(idx) => Ok(some_variant(Value::Int(idx as i64))),
        None => Ok(none_variant()),
    }
}

/// Match `path` against a route `pattern`, collecting `{name}` and
/// `{name...}` captures. `Some(params)` on a match (empty for a fully
/// literal pattern), `None` otherwise. Mirrors the compiled tier's
/// `route_segments_match` in `runtime::c_abi::http_bridges` so route
/// selection and captures agree bit-for-bit across tiers.
pub(crate) fn pattern_captures(pattern: &str, path: &str) -> Option<Vec<(String, String)>> {
    let pat_segs: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let path_segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let mut params: Vec<(String, String)> = Vec::new();
    let mut i = 0;
    let mut j = 0;
    while i < pat_segs.len() {
        let p = pat_segs[i];
        if p.starts_with('{') && p.ends_with("...}") {
            params.push((p[1..p.len() - 4].to_string(), path_segs[j..].join("/")));
            return Some(params);
        } else if p.starts_with('{') && p.ends_with('}') {
            if j >= path_segs.len() {
                return None;
            }
            params.push((p[1..p.len() - 1].to_string(), path_segs[j].to_string()));
            i += 1;
            j += 1;
        } else {
            if j >= path_segs.len() || path_segs[j] != p {
                return None;
            }
            i += 1;
            j += 1;
        }
    }
    if j == path_segs.len() {
        Some(params)
    } else {
        None
    }
}

pub(crate) fn pattern_matches(pattern: &str, path: &str) -> bool {
    pattern_captures(pattern, path).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_router_table_is_freed_with_its_last_handle() {
        let router = builtin_router_new(&[]).expect("Router::new");
        let copy = router.clone();
        let table = router_of(&router).expect("a router handle");
        builtin_router_add(&[
            router,
            Value::String("get".into()),
            Value::String("/items/{id}".into()),
        ])
        .expect("router::add");
        assert_eq!(table.lock().routes.len(), 1);
        drop(copy);
        assert_eq!(
            Arc::strong_count(&table),
            1,
            "only this test still holds the table"
        );
    }
}
