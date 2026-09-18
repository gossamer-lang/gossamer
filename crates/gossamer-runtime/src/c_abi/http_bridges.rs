#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::similar_names)]
#![allow(clippy::many_single_char_names)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::same_length_and_capacity)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::cast_ptr_alignment)]
#![allow(clippy::ptr_as_ptr)]
#![allow(static_mut_refs)]
#![allow(unused_unsafe)]
#![allow(clippy::wildcard_imports)]

use std::os::raw::c_char;

use super::*;

// ---------------------------------------------------------------
// ---------------------------------------------------------------
// 0.4.0 HTTP-module bridges - compiled tier stateful + free-fn
// entry points. Matches the interp surface in
// `gossamer_interp::stdlib_builtins::install_http_*`.
// ---------------------------------------------------------------

// Router: stateful Box-allocated handle. Each route stores
// (method, parsed pattern, handler env+fn) so `Router.serve(req)`
// can walk the list and invoke the matching handler via the
// same fn-pointer ABI gos_rt_http_serve uses.

pub struct GosRouter {
    routes: Vec<GosRoute>,
}

struct GosRoute {
    method: String, // empty = any verb
    segments: Vec<RouteSegment>,
    env: usize,
    fn_addr: usize,
    /// `true` when the handler is a bare Gossamer `fn(http::Request) ->
    /// Result<http::Response, http::Error>` registered via
    /// `gos_rt_router_get_fn` (and friends). Dispatch calls the handler
    /// with a single `req` arg, no env. `false` for struct/closure
    /// handlers registered via `gos_rt_router_get`, which use the
    /// `fn(env, req)` closure ABI.
    bare: bool,
}

enum RouteSegment {
    Literal(String),
    Capture(String),    // `{name}` - captures one path segment
    CaptureAll(String), // `{name...}` - captures the rest
}

fn parse_route_pattern(pattern: &str) -> Vec<RouteSegment> {
    let mut out = Vec::new();
    for seg in pattern.split('/').filter(|s| !s.is_empty()) {
        if seg.starts_with('{') && seg.ends_with("...}") {
            out.push(RouteSegment::CaptureAll(seg[1..seg.len() - 4].to_string()));
        } else if seg.starts_with('{') && seg.ends_with('}') {
            out.push(RouteSegment::Capture(seg[1..seg.len() - 1].to_string()));
        } else {
            out.push(RouteSegment::Literal(seg.to_string()));
        }
    }
    out
}

/// Match `path` against a parsed route pattern, collecting `{name}`
/// captures. Returns `Some(params)` on a match (params empty when the
/// pattern is fully literal), `None` when the route does not match.
fn route_segments_match(segments: &[RouteSegment], path: &str) -> Option<Vec<(String, String)>> {
    let path_segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let mut params: Vec<(String, String)> = Vec::new();
    let mut i = 0;
    let mut j = 0;
    while i < segments.len() {
        match &segments[i] {
            RouteSegment::CaptureAll(name) => {
                params.push((name.clone(), path_segs[j..].join("/")));
                return Some(params);
            }
            RouteSegment::Capture(name) => {
                if j >= path_segs.len() {
                    return None;
                }
                params.push((name.clone(), path_segs[j].to_string()));
                i += 1;
                j += 1;
            }
            RouteSegment::Literal(lit) => {
                if j >= path_segs.len() || path_segs[j] != lit {
                    return None;
                }
                i += 1;
                j += 1;
            }
        }
    }
    if j == path_segs.len() {
        Some(params)
    } else {
        None
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_router_new() -> *mut GosRouter {
    ffi_entry!(std::ptr::null_mut(), {
        Box::into_raw(Box::new(GosRouter { routes: Vec::new() }))
    })
}

/// Registers `pattern` under an already-decoded verb.
///
/// The per-verb entry points below name their method in Rust, so they reach
/// the route table here rather than shaping a C string the string ABI would
/// have to measure as a foreign one.
unsafe fn router_add_verb(
    router: *mut GosRouter,
    method: &str,
    pattern: *const c_char,
    env: *mut u8,
    fn_addr: i64,
) {
    if router.is_null() {
        return;
    }
    let r = unsafe { &mut *router };
    let pat = if pattern.is_null() {
        String::new()
    } else {
        unsafe { crate::c_abi::gos_str_arg_string(pattern) }
    };
    let segments = parse_route_pattern(&pat);
    super::fn_registry::register(fn_addr as usize, super::fn_registry::FnKind::HttpHandlerEnv);
    // A registered handler outlives the frame that built it: the router
    // answers from it for as long as the server runs, while the scope that
    // built the closure releases its own share at the end of the statement.
    // The route therefore takes a share of the environment, and holds it for
    // the life of the router - the same ownership `spawn` gives a goroutine
    // it hands a closure to.
    unsafe { crate::c_abi::rc::gos_rt_rc_retain(env) };
    // The server dispatches this handler on a goroutine per connection, so
    // everything the environment holds is reached from several threads at
    // once and has to count its shares atomically. Registration is where
    // that can be said: it runs on the thread that built the closure, before
    // the listener exists, which is the ordering `mark_shared` requires.
    unsafe { crate::c_abi::rc::gos_rt_rc_mark_shared(env) };
    r.routes.push(GosRoute {
        method: method.to_ascii_uppercase(),
        segments,
        env: env as usize,
        fn_addr: fn_addr as usize,
        bare: false,
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_router_add(
    router: *mut GosRouter,
    method: *const c_char,
    pattern: *const c_char,
    env: *mut u8,
    fn_addr: i64,
) {
    ffi_entry!((), {
        let m = if method.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(method) }
        };
        unsafe { router_add_verb(router, &m, pattern, env, fn_addr) };
    });
}

/// `router::add(router, method, pattern)` - registers a handler-less
/// route used purely for `router::lookup` pattern matching (the index
/// of the registered route is what `lookup` returns). `method` empty
/// matches any verb.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_router_add_pattern(
    router: *mut GosRouter,
    method: *const c_char,
    pattern: *const c_char,
) {
    ffi_entry!((), {
        if router.is_null() {
            return;
        }
        let r = unsafe { &mut *router };
        let m = if method.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(method) }.to_ascii_uppercase()
        };
        let pat = if pattern.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(pattern) }
        };
        let segments = parse_route_pattern(&pat);
        r.routes.push(GosRoute {
            method: m,
            segments,
            env: 0,
            fn_addr: 0,
            bare: false,
        });
    });
}

/// `router::lookup(router, method, path) -> Option<i64>` - the index of
/// the first route whose method (empty = any) and pattern match, packed
/// as the 2-word Option (disc=0 Some, disc=1 None). Mirrors the interp
/// `router::lookup` matching exactly.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_router_lookup(
    router: *const GosRouter,
    method: *const c_char,
    path: *const c_char,
) -> i128 {
    ffi_entry!(unsafe { crate::c_abi::vec::gos_rt_result_new(1, 0) }, {
        if router.is_null() {
            return unsafe { crate::c_abi::vec::gos_rt_result_new(1, 0) };
        }
        let r = unsafe { &*router };
        let m = if method.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(method) }.to_ascii_uppercase()
        };
        let p = if path.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(path) }
        };
        for (i, route) in r.routes.iter().enumerate() {
            if (route.method.is_empty() || route.method == m)
                && route_segments_match(&route.segments, &p).is_some()
            {
                return unsafe { crate::c_abi::vec::gos_rt_result_new(0, i as i64) };
            }
        }
        unsafe { crate::c_abi::vec::gos_rt_result_new(1, 0) }
    })
}

/// Internal helper: bare-fn variant of `gos_rt_router_add`. Used by
/// `gos_rt_router_get_fn` / `_post_fn` / etc. when the registered
/// handler has no env (a top-level `fn`).
unsafe fn router_add_bare(
    router: *mut GosRouter,
    method: &str,
    pattern: *const c_char,
    fn_addr: i64,
) {
    if router.is_null() {
        return;
    }
    let r = unsafe { &mut *router };
    let m = method.to_ascii_uppercase();
    let pat = if pattern.is_null() {
        String::new()
    } else {
        unsafe { crate::c_abi::gos_str_arg_string(pattern) }
    };
    let segments = parse_route_pattern(&pat);
    super::fn_registry::register(
        fn_addr as usize,
        super::fn_registry::FnKind::HttpHandlerBare,
    );
    r.routes.push(GosRoute {
        method: m,
        segments,
        env: 0,
        fn_addr: fn_addr as usize,
        bare: true,
    });
}

/// Convenience verb-specific entry points that map cleanly to
/// `Router.get(pattern, handler)` etc. in Gossamer source. Spelled
/// out one per verb so the `pub extern "C" fn` line parses through
/// the dispatch-consistency test's source scanner (macro-generated
/// fn names are invisible to a textual scan).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_router_get(
    router: *mut GosRouter,
    pattern: *const c_char,
    env: *mut u8,
    fn_addr: i64,
) -> *mut GosRouter {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { router_add_verb(router, "GET", pattern, env, fn_addr) };
        router
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_router_post(
    router: *mut GosRouter,
    pattern: *const c_char,
    env: *mut u8,
    fn_addr: i64,
) -> *mut GosRouter {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { router_add_verb(router, "POST", pattern, env, fn_addr) };
        router
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_router_put(
    router: *mut GosRouter,
    pattern: *const c_char,
    env: *mut u8,
    fn_addr: i64,
) -> *mut GosRouter {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { router_add_verb(router, "PUT", pattern, env, fn_addr) };
        router
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_router_delete(
    router: *mut GosRouter,
    pattern: *const c_char,
    env: *mut u8,
    fn_addr: i64,
) -> *mut GosRouter {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { router_add_verb(router, "DELETE", pattern, env, fn_addr) };
        router
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_router_patch(
    router: *mut GosRouter,
    pattern: *const c_char,
    env: *mut u8,
    fn_addr: i64,
) -> *mut GosRouter {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { router_add_verb(router, "PATCH", pattern, env, fn_addr) };
        router
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_router_head(
    router: *mut GosRouter,
    pattern: *const c_char,
    env: *mut u8,
    fn_addr: i64,
) -> *mut GosRouter {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { router_add_verb(router, "HEAD", pattern, env, fn_addr) };
        router
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_router_options(
    router: *mut GosRouter,
    pattern: *const c_char,
    env: *mut u8,
    fn_addr: i64,
) -> *mut GosRouter {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { router_add_verb(router, "OPTIONS", pattern, env, fn_addr) };
        router
    })
}

/// Bare-fn variants: register a top-level Gossamer `fn(http::Request)
/// -> Result<http::Response, http::Error>` directly as a handler - no
/// env, no struct wrapper. Dispatch invokes the function with the
/// request as its single argument.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_router_get_fn(
    router: *mut GosRouter,
    pattern: *const c_char,
    fn_addr: i64,
) -> *mut GosRouter {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { router_add_bare(router, "GET", pattern, fn_addr) };
        router
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_router_post_fn(
    router: *mut GosRouter,
    pattern: *const c_char,
    fn_addr: i64,
) -> *mut GosRouter {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { router_add_bare(router, "POST", pattern, fn_addr) };
        router
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_router_put_fn(
    router: *mut GosRouter,
    pattern: *const c_char,
    fn_addr: i64,
) -> *mut GosRouter {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { router_add_bare(router, "PUT", pattern, fn_addr) };
        router
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_router_delete_fn(
    router: *mut GosRouter,
    pattern: *const c_char,
    fn_addr: i64,
) -> *mut GosRouter {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { router_add_bare(router, "DELETE", pattern, fn_addr) };
        router
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_router_patch_fn(
    router: *mut GosRouter,
    pattern: *const c_char,
    fn_addr: i64,
) -> *mut GosRouter {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { router_add_bare(router, "PATCH", pattern, fn_addr) };
        router
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_router_head_fn(
    router: *mut GosRouter,
    pattern: *const c_char,
    fn_addr: i64,
) -> *mut GosRouter {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { router_add_bare(router, "HEAD", pattern, fn_addr) };
        router
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_router_options_fn(
    router: *mut GosRouter,
    pattern: *const c_char,
    fn_addr: i64,
) -> *mut GosRouter {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe { router_add_bare(router, "OPTIONS", pattern, fn_addr) };
        router
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_router_add_fn(
    router: *mut GosRouter,
    method: *const c_char,
    pattern: *const c_char,
    fn_addr: i64,
) {
    ffi_entry!((), {
        let m = if method.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(method) }
        };
        unsafe { router_add_bare(router, &m, pattern, fn_addr) }
    });
}

/// Dispatch a request through the router. Walks the route table,
/// invokes the first matching handler via fn-pointer ABI, and
/// returns its `*mut GosResult`. Returns a 404-shaped result when
/// nothing matches.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn gos_rt_router_serve(
    router: *const GosRouter,
    req: *mut GosHttpRequest,
) -> i128 {
    ffi_entry_passthrough!(0i128, {
        if router.is_null() || req.is_null() {
            return router_404_result();
        }
        let r = unsafe { &*router };
        // Clone the request's path + method so the borrow ends before
        // we write captured params back through the `*mut req`.
        let path = unsafe { (*req).url_path_only().to_string() };
        let method = unsafe { (*req).method.clone() };
        for route in &r.routes {
            if !route.method.is_empty() && !route.method.eq_ignore_ascii_case(&method) {
                continue;
            }
            if let Some(params) = route_segments_match(&route.segments, &path) {
                unsafe { (*req).params = params };
                if route.bare {
                    super::fn_registry::verify(
                        route.fn_addr,
                        super::fn_registry::FnKind::HttpHandlerBare,
                    );
                    type BareFn = unsafe extern "C-unwind" fn(req: *mut GosHttpRequest) -> i128;
                    let handler: BareFn = unsafe { std::mem::transmute(route.fn_addr) };
                    return unsafe { handler(req) };
                }
                super::fn_registry::verify(
                    route.fn_addr,
                    super::fn_registry::FnKind::HttpHandlerEnv,
                );
                type HandlerFn =
                    unsafe extern "C-unwind" fn(env: *mut u8, req: *mut GosHttpRequest) -> i128;
                let handler: HandlerFn = unsafe { std::mem::transmute(route.fn_addr) };
                return unsafe { handler(route.env as *mut u8, req) };
            }
        }
        router_404_result()
    })
}

fn router_404_result() -> i128 {
    let resp = Box::into_raw(Box::new(GosHttpResponse {
        status: 404,
        body: SyncRawPtr::new(alloc_cstring(b"not found")),
        headers: Vec::new(),
        body_bytes: None,
        content_type: "text/plain; charset=utf-8".into(),
        stream_handle: -1,
    }));
    crate::c_abi::vec::pack_result(0, resp as i64)
}

// ---------------------------------------------------------------
// Shared static-file Range (RFC 7233) handling. Both the compiled
// `gos_rt_file_server_serve` shim and the interp-tier
// `native_file_server_serve` native evaluate the request `Range:`
// header and build any `multipart/byteranges` body through these
// helpers, so partial-content responses are bit-identical across
// tiers.
// ---------------------------------------------------------------

/// Fixed `multipart/byteranges` boundary. Static rather than random so
/// a multi-range response is byte-deterministic across tiers; a public
/// server would randomise this, but the cross-tier parity gate requires
/// a stable wire image.
pub const BYTERANGES_BOUNDARY: &str = "gossamer_byteranges_boundary";

/// Outcome of evaluating a `Range:` header against a file length.
pub enum RangeOutcome {
    /// No (parseable) Range header - serve the whole file (200).
    Whole,
    /// One satisfiable range - 206 + Content-Range.
    Single { start: u64, end: u64 },
    /// Several satisfiable ranges - 206 multipart/byteranges.
    Multi(Vec<(u64, u64)>),
    /// A Range header naming no satisfiable range - 416.
    Unsatisfiable,
}

/// Parses an RFC 7233 `Range: bytes=...` header against the file length
/// `len`. Supports `N-M`, open-ended `N-`, and suffix `-N` specs,
/// single or comma-separated. A missing / syntactically invalid header
/// yields `Whole`; a well-formed header whose every spec is out of
/// range yields `Unsatisfiable`.
#[must_use]
pub fn evaluate_range(header: Option<&str>, len: u64) -> RangeOutcome {
    let Some(header) = header else {
        return RangeOutcome::Whole;
    };
    let Some(rest) = header.trim().strip_prefix("bytes=") else {
        return RangeOutcome::Whole;
    };
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    let mut saw_spec = false;
    for spec in rest.split(',') {
        let spec = spec.trim();
        if spec.is_empty() {
            continue;
        }
        saw_spec = true;
        let Some((start_s, end_s)) = spec.split_once('-') else {
            return RangeOutcome::Whole;
        };
        let (start_s, end_s) = (start_s.trim(), end_s.trim());
        let resolved = if start_s.is_empty() {
            // Suffix form `-N`: the last N bytes.
            match end_s.parse::<u64>() {
                Ok(n) if n > 0 && len > 0 => {
                    let n = n.min(len);
                    Some((len - n, len - 1))
                }
                Ok(_) => None,
                Err(_) => return RangeOutcome::Whole,
            }
        } else {
            match start_s.parse::<u64>() {
                Ok(start) if len > 0 && start < len => {
                    let end = if end_s.is_empty() {
                        len - 1
                    } else {
                        match end_s.parse::<u64>() {
                            Ok(e) => e.min(len - 1),
                            Err(_) => return RangeOutcome::Whole,
                        }
                    };
                    if start <= end {
                        Some((start, end))
                    } else {
                        None
                    }
                }
                Ok(_) => None,
                Err(_) => return RangeOutcome::Whole,
            }
        };
        if let Some(r) = resolved {
            ranges.push(r);
        }
    }
    if !saw_spec {
        return RangeOutcome::Whole;
    }
    match ranges.len() {
        0 => RangeOutcome::Unsatisfiable,
        1 => RangeOutcome::Single {
            start: ranges[0].0,
            end: ranges[0].1,
        },
        _ => RangeOutcome::Multi(ranges),
    }
}

/// Inclusive `[start, end]` slice of `file`, clamped to its bounds.
#[must_use]
pub fn range_slice(file: &[u8], start: u64, end: u64) -> Vec<u8> {
    let s = (start as usize).min(file.len());
    let e = (end as usize).saturating_add(1).min(file.len());
    file[s.min(e)..e].to_vec()
}

/// The `Content-Range` header value for a single 206 range.
#[must_use]
pub fn content_range_value(start: u64, end: u64, total: u64) -> String {
    format!("bytes {start}-{end}/{total}")
}

/// The `Content-Type` for a `multipart/byteranges` response.
#[must_use]
pub fn multipart_content_type() -> String {
    format!("multipart/byteranges; boundary={BYTERANGES_BOUNDARY}")
}

/// Builds the `multipart/byteranges` body for `ranges`.
#[must_use]
pub fn build_multipart_body(file: &[u8], ranges: &[(u64, u64)], mime: &str, total: u64) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    for &(start, end) in ranges {
        out.extend_from_slice(b"--");
        out.extend_from_slice(BYTERANGES_BOUNDARY.as_bytes());
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(format!("Content-Type: {mime}\r\n").as_bytes());
        out.extend_from_slice(
            format!("Content-Range: bytes {start}-{end}/{total}\r\n\r\n").as_bytes(),
        );
        out.extend_from_slice(&range_slice(file, start, end));
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"--");
    out.extend_from_slice(BYTERANGES_BOUNDARY.as_bytes());
    out.extend_from_slice(b"--\r\n");
    out
}

// FileServer: read-and-serve from a root directory with a path
// prefix strip. Mirrors `static_files::FileServer`'s common case.

pub struct GosFileServer {
    root: String,
    prefix: String,
}

/// Default cap on a static file served to a client, in bytes. A file
/// larger than this is treated as not-found so an attacker-controlled
/// path cannot force an unbounded read into memory. Matches
/// `gossamer_std::http_static_files::FileServer::max_file_bytes`.
pub const STATIC_FILE_MAX_BYTES: u64 = 100 * 1024 * 1024;

/// Outcome of resolving a static-file request path against a root.
pub enum StaticResolution {
    /// An in-root regular file, no larger than the size cap, safe to
    /// read and serve.
    File(std::path::PathBuf),
    /// The request escaped the configured root (absolute path, `..`,
    /// or a symlink pointing outside the root).
    Forbidden,
    /// The request did not resolve to a servable, in-limit regular
    /// file.
    NotFound,
    /// The request named a directory without a trailing slash. The
    /// caller answers `301` to the same path with one, so the page's
    /// own relative links resolve under the directory rather than
    /// beside it.
    Redirect,
}

/// Resolves `rel` under `root` for a static-file request, enforcing
/// the traversal and size guards shared by the compiled and interpreted
/// static file servers. `rel` is the request path with the route
/// prefix already stripped and leading slashes trimmed.
///
/// Both sides are canonicalized and the resolved path must stay within
/// the canonical `root`; unlike a textual `..` scan this also defeats
/// symlink escapes and platform path prefixes (Windows `\\?\`, macOS
/// `/private`). An absolute request path is refused before the join so
/// it cannot replace the root outright.
pub fn resolve_static_path(root: &std::path::Path, rel: &str, max_bytes: u64) -> StaticResolution {
    let rel_path = std::path::Path::new(rel);
    if rel_path.is_absolute() {
        return StaticResolution::Forbidden;
    }
    let candidate = root.join(rel_path);
    let Ok(canonical) = std::fs::canonicalize(&candidate) else {
        return StaticResolution::NotFound;
    };
    let root_canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    if !canonical.starts_with(&root_canonical) {
        return StaticResolution::Forbidden;
    }
    let Ok(meta) = std::fs::metadata(&canonical) else {
        return StaticResolution::NotFound;
    };
    if meta.is_dir() {
        // A directory is served as the `index.html` inside it, which is
        // what makes a site's own directory URLs resolve.
        if !rel.is_empty() && !rel.ends_with('/') {
            return StaticResolution::Redirect;
        }
        let index = canonical.join("index.html");
        let Ok(index_meta) = std::fs::metadata(&index) else {
            return StaticResolution::NotFound;
        };
        return servable(index, &index_meta, max_bytes);
    }
    servable(canonical, &meta, max_bytes)
}

/// `path` as a resolution, once it is known to be inside the root.
fn servable(
    path: std::path::PathBuf,
    meta: &std::fs::Metadata,
    max_bytes: u64,
) -> StaticResolution {
    if !meta.is_file() {
        return StaticResolution::NotFound;
    }
    if max_bytes > 0 && meta.len() > max_bytes {
        return StaticResolution::NotFound;
    }
    StaticResolution::File(path)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_file_server_new(
    root: *const c_char,
    prefix: *const c_char,
) -> *mut GosFileServer {
    ffi_entry!(std::ptr::null_mut(), {
        let root_s = if root.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(root) }
        };
        let prefix_s = if prefix.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(prefix) }
        };
        Box::into_raw(Box::new(GosFileServer {
            root: root_s,
            prefix: prefix_s,
        }))
    })
}

/// `FileServer.serve(req) -> Result<Response, Error>`. Reads the
/// requested file from disk; rejects path traversal; returns 404
/// when missing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_file_server_serve(
    fs: *const GosFileServer,
    req: *const GosHttpRequest,
) -> i128 {
    ffi_entry!(0i128, {
        if fs.is_null() || req.is_null() {
            return router_404_result();
        }
        let server = unsafe { &*fs };
        let request = unsafe { &*req };
        let path = request.url_path_only();
        let rel = path.strip_prefix(&server.prefix).unwrap_or(path);
        let rel = rel.trim_start_matches('/');
        let full = match resolve_static_path(
            std::path::Path::new(&server.root),
            rel,
            STATIC_FILE_MAX_BYTES,
        ) {
            StaticResolution::File(p) => p,
            StaticResolution::Forbidden => {
                return crate::c_abi::vec::pack_result(
                    0,
                    Box::into_raw(Box::new(GosHttpResponse {
                        status: 403,
                        body: SyncRawPtr::new(alloc_cstring(b"forbidden")),
                        headers: Vec::new(),
                        body_bytes: None,
                        content_type: "text/plain; charset=utf-8".into(),
                        stream_handle: -1,
                    })) as i64,
                );
            }
            StaticResolution::Redirect => {
                return crate::c_abi::vec::pack_result(
                    0,
                    Box::into_raw(Box::new(GosHttpResponse {
                        status: 301,
                        body: SyncRawPtr::new(alloc_cstring(b"")),
                        headers: vec![("location".to_string(), format!("{path}/"))],
                        body_bytes: None,
                        content_type: "text/plain; charset=utf-8".into(),
                        stream_handle: -1,
                    })) as i64,
                );
            }
            StaticResolution::NotFound => return router_404_result(),
        };
        match std::fs::read(&full) {
            Ok(bytes) => {
                let mime = mime_for_path_str(&full.to_string_lossy());
                let total = bytes.len() as u64;
                let range_header = request
                    .headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("range"))
                    .map(|(_, v)| v.as_str());
                let (status, body, content_type, mut headers) =
                    match evaluate_range(range_header, total) {
                        RangeOutcome::Whole => (
                            200,
                            bytes,
                            mime.to_string(),
                            vec![("accept-ranges".to_string(), "bytes".to_string())],
                        ),
                        RangeOutcome::Single { start, end } => {
                            let slice = range_slice(&bytes, start, end);
                            (
                                206,
                                slice,
                                mime.to_string(),
                                vec![
                                    ("accept-ranges".to_string(), "bytes".to_string()),
                                    (
                                        "content-range".to_string(),
                                        content_range_value(start, end, total),
                                    ),
                                ],
                            )
                        }
                        RangeOutcome::Multi(ranges) => {
                            let body = build_multipart_body(&bytes, &ranges, mime, total);
                            (
                                206,
                                body,
                                multipart_content_type(),
                                vec![("accept-ranges".to_string(), "bytes".to_string())],
                            )
                        }
                        RangeOutcome::Unsatisfiable => (
                            416,
                            Vec::new(),
                            "text/plain; charset=utf-8".to_string(),
                            vec![("content-range".to_string(), format!("bytes */{total}"))],
                        ),
                    };
                headers.insert(0, ("content-type".to_string(), content_type.clone()));
                let body_cstr = alloc_cstring(&body);
                crate::c_abi::vec::pack_result(
                    0,
                    Box::into_raw(Box::new(GosHttpResponse {
                        status,
                        body: SyncRawPtr::new(body_cstr),
                        headers,
                        body_bytes: Some(body),
                        content_type: content_type.into(),
                        stream_handle: -1,
                    })) as i64,
                )
            }
            Err(_) => router_404_result(),
        }
    })
}

/// `static_files::serve_file(path) -> Result<Response, errors::Error>` -
/// one-shot read of a single file into a 200 Response (content-type from
/// the extension), or `Err` when the file cannot be read. Distinct from
/// `FileServer` (no prefix-strip / Range handling); mirrors the interp
/// `static_files::serve_file`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_static_serve_file(path: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        let path_s = if path.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(path) }
        };
        match std::fs::read(&path_s) {
            Ok(bytes) => {
                let mime = mime_for_path_str(&path_s);
                let body_cstr = alloc_cstring(&bytes);
                crate::c_abi::vec::pack_result(
                    0,
                    Box::into_raw(Box::new(GosHttpResponse {
                        status: 200,
                        body: SyncRawPtr::new(body_cstr),
                        headers: vec![("content-type".to_string(), mime.to_string())],
                        body_bytes: Some(bytes),
                        content_type: mime.into(),
                        stream_handle: -1,
                    })) as i64,
                )
            }
            Err(e) => {
                let err = crate::c_abi::errors::error_new_from_bytes(format!("{e}").as_bytes());
                crate::c_abi::vec::pack_result(1, err as i64)
            }
        }
    })
}

fn mime_for_path_str(path: &str) -> &'static str {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "application/javascript",
        "json" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "wasm" => "application/wasm",
        "pdf" => "application/pdf",
        "txt" | "md" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        _ => "application/octet-stream",
    }
}

// NativeClient: minimal stateful handle that round-trips through
// `gos_rt_http_get` / a tiny POST helper for the methods callers
// actually use in compiled mode. The full builder surface lives
// in gossamer-std for interp; the compiled handle is intentionally
// thin since most consumers go through `http::get` / `http::Client`.

pub struct GosNativeClient;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_native_client_new() -> *mut GosNativeClient {
    ffi_entry!(std::ptr::null_mut(), {
        Box::into_raw(Box::new(GosNativeClient))
    })
}

/// `NativeClient.get(url) -> Result<Response, Error>`. Delegates
/// to the existing one-shot GET helper.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_native_client_get(
    _client: *const GosNativeClient,
    url: *const c_char,
) -> i128 {
    ffi_entry!(0i128, {
        unsafe { gos_rt_http_get(url, std::ptr::null_mut()) }
    })
}

// Proxy: stateful upstream-URL holder. `Proxy.forward(req)` issues
// a one-shot upstream request and returns the response.

pub struct GosProxy {
    upstream: String,
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_proxy_new(upstream: *const c_char) -> *mut GosProxy {
    ffi_entry!(std::ptr::null_mut(), {
        let u = if upstream.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(upstream) }
        };
        Box::into_raw(Box::new(GosProxy { upstream: u }))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_proxy_forward(
    proxy: *const GosProxy,
    req: *const GosHttpRequest,
) -> i128 {
    ffi_entry!(0i128, {
        if proxy.is_null() {
            return router_404_result();
        }
        let p = unsafe { &*proxy };
        let request_path = if req.is_null() {
            "/".to_string()
        } else {
            unsafe { (&*req).url.clone() }
        };
        let full = format!("{}{request_path}", p.upstream.trim_end_matches('/'));
        // The `url` parameter is a Gossamer `String`, read through the length
        // header that sits before the body, so the argument is built as one.
        let url = alloc_cstring(full.as_bytes());
        let forwarded = unsafe { gos_rt_http_get(url, std::ptr::null_mut()) };
        unsafe { crate::c_abi::string::gos_rt_str_free(url) };
        forwarded
    })
}

// WebSocket: handshake/frame helpers. Full bidirectional framing
// needs a per-connection state machine that mostly lives in the
// existing gossamer-std `WebSocket` Rust impl; compiled-mode users
// drive it via `accept_key` + manual frame layout for now. The
// accept-key thunk is already declared above (gos_rt_ws_accept_key).
// gos_rt_ws_frame_text - encodes one text frame for outbound use.

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ws_frame_text(payload: *const c_char) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if payload.is_null() {
            return alloc_cstring(b"");
        }
        let bytes = unsafe { crate::c_abi::gos_str_arg_bytes(payload) };
        let mut out: Vec<u8> = Vec::with_capacity(bytes.len() + 14);
        out.push(0x81); // FIN + text opcode
        let len = bytes.len();
        if len < 126 {
            out.push(len as u8);
        } else if len < 65536 {
            out.push(126);
            out.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            out.push(127);
            out.extend_from_slice(&(len as u64).to_be_bytes());
        }
        out.extend_from_slice(bytes);
        alloc_cstring(&out)
    })
}

impl GosHttpRequest {
    pub(crate) fn url_path_only(&self) -> &str {
        match self.url.split('?').next() {
            Some(p) => p,
            None => self.url.as_str(),
        }
    }
}

/// chunked::encode - wrap one buffer in HTTP/1.1 chunked
/// transfer-encoding with a single data chunk + terminator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_chunked_encode(data: *const c_char) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if data.is_null() {
            return alloc_cstring(b"");
        }
        let bytes = unsafe { crate::c_abi::gos_str_arg_bytes(data) };
        let out = format!("{:x}\r\n", bytes.len());
        let mut buf: Vec<u8> = Vec::with_capacity(bytes.len() + out.len() + 7);
        buf.extend_from_slice(out.as_bytes());
        buf.extend_from_slice(bytes);
        buf.extend_from_slice(b"\r\n0\r\n\r\n");
        alloc_cstring(&buf)
    })
}

/// chunked::decode - concat the data chunks from a complete
/// chunked body (trailers discarded).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_chunked_decode(data: *const c_char) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if data.is_null() {
            return alloc_cstring(b"");
        }
        let bytes = unsafe { crate::c_abi::gos_str_arg_bytes(data) };
        let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
        let mut i = 0usize;
        while i < bytes.len() {
            // Read hex chunk size up to CRLF.
            let mut j = i;
            while j < bytes.len() && bytes[j] != b'\r' {
                j += 1;
            }
            let line = std::str::from_utf8(&bytes[i..j]).unwrap_or("");
            let size_str = line.split(';').next().unwrap_or(line).trim();
            let Ok(size) = u64::from_str_radix(size_str, 16) else {
                return alloc_cstring(b"");
            };
            // Skip CRLF.
            i = j + 2;
            if size == 0 {
                // Skip trailers up to terminating blank line.
                while i + 1 < bytes.len() && &bytes[i..i + 2] != b"\r\n" {
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                    i += 1;
                }
                break;
            }
            let take = size as usize;
            if i + take > bytes.len() {
                return alloc_cstring(b"");
            }
            out.extend_from_slice(&bytes[i..i + take]);
            i += take;
            // Skip data-trailing CRLF.
            if i + 1 < bytes.len() {
                i += 2;
            }
        }
        alloc_cstring(&out)
    })
}

/// sse::encode_event(name, data, id) - render one
/// `event:`/`data:` block.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sse_encode_event(
    name: *const c_char,
    data: *const c_char,
    id: *const c_char,
) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let n = if name.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(name) }
        };
        let d = if data.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(data) }
        };
        let id_s = if id.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(id) }
        };
        let mut out = String::new();
        if !id_s.is_empty() {
            out.push_str("id: ");
            out.push_str(&id_s);
            out.push('\n');
        }
        if !n.is_empty() {
            out.push_str("event: ");
            out.push_str(&n);
            out.push('\n');
        }
        for line in d.split('\n') {
            out.push_str("data: ");
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
        alloc_cstring(out.as_bytes())
    })
}

/// sse::encode_comment - render a `:`-prefixed keepalive line.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sse_encode_comment(text: *const c_char) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        let t = if text.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(text) }
        };
        alloc_cstring(format!(": {t}\n\n").as_bytes())
    })
}

/// sse::encode_retry - render a `retry:` directive.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_sse_encode_retry(ms: i64) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        alloc_cstring(format!("retry: {ms}\n\n").as_bytes())
    })
}

/// middleware::new_request_id - process-monotonic id with nanos
/// prefix.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_mw_new_request_id() -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = crate::platform::system_time_now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        alloc_cstring(format!("{nanos:x}-{n:x}").as_bytes())
    })
}

/// middleware::accepts_gzip - comma-split the header, look for a
/// gzip token.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_mw_accepts_gzip(header: *const c_char) -> i32 {
    ffi_entry!(-1, {
        if header.is_null() {
            return 0;
        }
        let h = unsafe { crate::c_abi::gos_str_arg_lossy(header) };
        let accepts = h
            .split(',')
            .any(|tok| tok.trim().eq_ignore_ascii_case("gzip"));
        i32::from(accepts)
    })
}

/// websocket::accept_key - RFC 6455 Sec-WebSocket-Accept
/// derivation: base64(sha1(client_key + GUID)).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_ws_accept_key(client_key: *const c_char) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        const WS_GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
        if client_key.is_null() {
            return alloc_cstring(b"");
        }
        let k = unsafe { crate::c_abi::gos_str_arg_bytes(client_key) };
        let mut input: Vec<u8> = Vec::with_capacity(k.len() + WS_GUID.len());
        input.extend_from_slice(k);
        input.extend_from_slice(WS_GUID);
        let digest = sha1_oneshot(&input);
        let encoded = base64_oneshot(&digest);
        alloc_cstring(encoded.as_bytes())
    })
}

/// static_files::mime_for_path - extension-driven MIME lookup.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_static_mime_for_path(path: *const c_char) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if path.is_null() {
            return alloc_cstring(b"application/octet-stream");
        }
        let p = unsafe { crate::c_abi::gos_str_arg_string(path) };
        let ext = std::path::Path::new(&p)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let mime = match ext.as_str() {
            "html" | "htm" => "text/html; charset=utf-8",
            "css" => "text/css; charset=utf-8",
            "js" | "mjs" => "application/javascript",
            "json" => "application/json",
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "svg" => "image/svg+xml",
            "webp" => "image/webp",
            "wasm" => "application/wasm",
            "pdf" => "application/pdf",
            "txt" | "md" => "text/plain; charset=utf-8",
            "xml" => "application/xml",
            _ => "application/octet-stream",
        };
        alloc_cstring(mime.as_bytes())
    })
}

// Minimal sha1 + base64 used by gos_rt_ws_accept_key. Inlined
// here to avoid pulling in another dep - the runtime crate
// stays self-contained for these tiny one-shots.
fn sha1_oneshot(input: &[u8]) -> [u8; 20] {
    // FIPS 180-4 SHA-1.
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let bit_len = (input.len() as u64).wrapping_mul(8);
    let mut padded: Vec<u8> = Vec::with_capacity(input.len() + 72);
    padded.extend_from_slice(input);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in padded.chunks_exact(64) {
        let mut w: [u32; 80] = [0; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[4 * i],
                chunk[4 * i + 1],
                chunk[4 * i + 2],
                chunk[4 * i + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999_u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1_u32),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC_u32),
                _ => (b ^ c ^ d, 0xCA62_C1D6_u32),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut out = [0u8; 20];
    for (i, word) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

fn base64_oneshot(input: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[((b0 & 0b11) << 4 | b1 >> 4) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((b1 & 0b1111) << 2 | b2 >> 6) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 0b111111) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod static_path_tests {
    use super::{StaticResolution, gos_rt_router_new, resolve_static_path, router_add_verb};

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let mut dir = crate::platform::temp_dir();
        dir.push(format!(
            "gossamer_static_guard_{}_{}",
            tag,
            crate::platform::process_id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn in_root_file_is_served() {
        let root = temp_root("ok");
        std::fs::write(root.join("hello.txt"), b"hi").unwrap();
        match resolve_static_path(&root, "hello.txt", 0) {
            StaticResolution::File(p) => assert!(p.ends_with("hello.txt")),
            other => panic!("expected File, got {}", label(&other)),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dotdot_escape_is_forbidden() {
        let root = temp_root("escape");
        std::fs::create_dir_all(root.join("public")).unwrap();
        std::fs::write(root.join("secret.txt"), b"top secret").unwrap();
        let public = root.join("public");
        // `public/../secret.txt` resolves above `public` (the root).
        assert!(matches!(
            resolve_static_path(&public, "../secret.txt", 0),
            StaticResolution::Forbidden
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn absolute_request_path_is_forbidden() {
        let root = temp_root("abs");
        let abs = if cfg!(windows) {
            "C:\\Windows\\win.ini"
        } else {
            "/etc/passwd"
        };
        assert!(matches!(
            resolve_static_path(&root, abs, 0),
            StaticResolution::Forbidden | StaticResolution::NotFound
        ));
        // A Unix absolute path is unambiguously Forbidden.
        if !cfg!(windows) {
            assert!(matches!(
                resolve_static_path(&root, "/etc/passwd", 0),
                StaticResolution::Forbidden
            ));
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn oversized_file_is_not_found() {
        let root = temp_root("size");
        std::fs::write(root.join("big.bin"), vec![0u8; 4096]).unwrap();
        assert!(matches!(
            resolve_static_path(&root, "big.bin", 1024),
            StaticResolution::NotFound
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_forbidden() {
        let root = temp_root("symlink");
        std::fs::write(root.join("outside_target"), b"secret").unwrap();
        let served = root.join("served");
        std::fs::create_dir_all(&served).unwrap();
        std::os::unix::fs::symlink(root.join("outside_target"), served.join("link")).unwrap();
        assert!(matches!(
            resolve_static_path(&served, "link", 0),
            StaticResolution::Forbidden
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    fn label(r: &StaticResolution) -> &'static str {
        match r {
            StaticResolution::File(_) => "File",
            StaticResolution::Forbidden => "Forbidden",
            StaticResolution::NotFound => "NotFound",
            StaticResolution::Redirect => "Redirect",
        }
    }

    /// A directory is served as the index inside it, and one named
    /// without a trailing slash is redirected to the form the page's
    /// own relative links resolve under.
    #[test]
    fn a_directory_resolves_to_its_index_or_a_redirect() {
        let root = temp_root("directory-index");
        let sub = root.join("tour");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("index.html"), b"TOUR").unwrap();
        std::fs::write(root.join("index.html"), b"ROOT").unwrap();
        assert_eq!(label(&resolve_static_path(&root, "tour/", 0)), "File");
        assert_eq!(label(&resolve_static_path(&root, "tour", 0)), "Redirect");
        // The server root is the directory form already.
        assert_eq!(label(&resolve_static_path(&root, "", 0)), "File");
        // A directory with no index is still not found.
        std::fs::create_dir_all(root.join("empty")).unwrap();
        assert_eq!(label(&resolve_static_path(&root, "empty/", 0)), "NotFound");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A registered handler outlives the frame that built it, so the route
    /// has to hold a share of the closure's environment.
    ///
    /// Without one, the scope that built the closure releases the only share
    /// at the end of the statement that registered it, and the router answers
    /// every later request through a freed environment - which reads as
    /// whatever the allocator has since put there.
    #[test]
    fn a_registered_route_holds_a_share_of_its_handler_environment() {
        let meta: [i64; 2] = [8, 0];
        let env = unsafe { crate::c_abi::rc::gos_rt_rc_alloc(8, meta.as_ptr()) };
        assert!(!env.is_null(), "the test environment allocated");
        let before = unsafe { crate::c_abi::rc::rc_strong_count(env) };

        let router = unsafe { gos_rt_router_new() };
        let pattern = crate::c_abi::string::test_gos_str("/user/{id}");
        unsafe { router_add_verb(router, "GET", pattern, env, 0) };

        let after = unsafe { crate::c_abi::rc::rc_strong_count(env) };
        assert_eq!(
            after,
            before + 1,
            "registering a handler takes one share of its environment"
        );
    }

    /// The environment a handler reads from several connection goroutines
    /// counts its shares atomically, which registration is what establishes.
    #[test]
    fn a_registered_route_marks_its_handler_environment_shared() {
        let meta: [i64; 2] = [8, 0];
        let env = unsafe { crate::c_abi::rc::gos_rt_rc_alloc(8, meta.as_ptr()) };
        assert!(!env.is_null(), "the test environment allocated");
        assert!(
            !unsafe { crate::c_abi::rc::rc_payload_is_shared(env) },
            "a fresh environment starts thread-local"
        );

        let router = unsafe { gos_rt_router_new() };
        let pattern = crate::c_abi::string::test_gos_str("/user/{id}");
        unsafe { router_add_verb(router, "GET", pattern, env, 0) };

        assert!(
            unsafe { crate::c_abi::rc::rc_payload_is_shared(env) },
            "registering a handler publishes its environment to the server's goroutines"
        );
    }
}
