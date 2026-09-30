//! Standard library for Gossamer (Phases 22 → 26).
//! Introduces this crate as the manifest + Rust-side runtime
//! support for every stdlib module. Subsequent phases extend the
//! manifest with their own module entries while reusing the shared
//! infrastructure exposed here. The Gossamer source files that
//! eventually compile via `gos build` will live alongside this crate
//! in `crates/gossamer-std/std/*.gos` and call into the helpers here
//! for primitives the language can't yet express in itself.

#![deny(unsafe_code)]

/// Archive readers and writers (zip, tar).
pub mod archive;
/// `AsyncRead`/`AsyncWrite` bridge over `net::TcpStream`.
#[cfg(not(target_arch = "wasm32"))]
pub mod async_tcp;
pub mod blocking_pool;
pub mod bufio;
pub mod bytes;
pub mod collections;
/// Compression and decompression codecs (gzip, flate, zlib, bzip2).
pub mod compress;
/// Min-heap (priority queue) over `Vec<i64>` (`std::container::heap`).
/// Linked list + sorted-on-insert Vec / list variants.
/// FIFO queue / LIFO stack / double-ended queue over `Vec<i64>`.
/// Sorted set + map containers backed by `Vec<i64>`.
pub mod context;
// The pure hash families compile to wasm; the OS-RNG / signing submodules
// inside are gated to native (see crypto.rs).
pub mod crypto;
pub mod database;
pub mod encoding;
/// TOML 1.0 parsing + emission (`std::encoding::toml`).
pub mod encoding_toml;
/// Process environment, command-line arguments, working directory
/// (Rust `std::env` shape).
pub mod env;
pub mod errors;
pub mod exec;
pub mod flag;
pub mod fmt;
pub mod fs;
/// Non-cryptographic hash functions (FNV, etc.).
pub mod hash;
/// HTML escaping utilities and optional template engine.
pub mod html;
pub mod http;
/// HTTP/1.1 chunked transfer-encoding (RFC 7230 §4.1) reader / writer.
pub mod http_chunked;
/// HTTP cookie parsing + builder (RFC 6265).
pub mod http_cookie;
/// CSRF protection (double-submit cookie + Origin/Referer).
#[cfg(not(target_arch = "wasm32"))]
pub mod http_csrf;
/// Form parsing (`application/x-www-form-urlencoded`).
pub mod http_form;
/// HTTP/2 server (h2 crate over goroutine future-driver).
#[cfg(not(target_arch = "wasm32"))]
pub mod http_h2;
/// HTTP/3 server + client (quinn + h3 over a private tokio runtime).
#[cfg(not(target_arch = "wasm32"))]
pub mod http_h3;
/// Operational health, readiness, and liveness handlers.
pub mod http_health;
/// HTTP middleware suite (logger, recoverer, request-id, CORS, basic-auth, gzip,
/// body_limit, timeout, hsts, security_headers, cache_control, etag, bearer_auth, rate_limit).
#[cfg(not(target_arch = "wasm32"))]
pub mod http_middleware;
/// Multipart form-data (`multipart/form-data`, RFC 7578) streaming parser.
#[cfg(not(target_arch = "wasm32"))]
pub mod http_multipart;
/// Native h1 HTTP client over `std::net::TcpStream`.
#[cfg(not(target_arch = "wasm32"))]
pub mod http_native_client;
/// HTTP reverse proxy (Director, hop-by-hop strip, error handler).
#[cfg(not(target_arch = "wasm32"))]
pub mod http_proxy;
/// Typed query-string wrapper (URL query component).
pub mod http_query;
/// HTTP router (Go 1.22-class ServeMux with captures + method gating).
pub mod http_router;
/// HTTP session management (signed-cookie store by default).
#[cfg(not(target_arch = "wasm32"))]
pub mod http_session;
/// Server-Sent Events (`text/event-stream`).
pub mod http_sse;
/// Application state container (TypeMap, dependency injection).
pub mod http_state;
/// HTTP static file server (ETag, Last-Modified, Range, MIME sniff).
pub mod http_static_files;
/// WebSocket (RFC 6455) - first-party stdlib support.
pub mod http_websocket;
/// In-memory RGBA image model with PNG and JPEG codecs.
#[cfg(not(target_arch = "wasm32"))]
pub mod image;
pub mod io;
pub mod io_handles;
pub mod iter;
/// JWT (JSON Web Tokens, RFC 7519) - HS256/384/512, ES256, EdDSA.
#[cfg(not(target_arch = "wasm32"))]
pub mod jwt;
/// Process lifecycle: graceful-shutdown hooks, signal handling, sd_notify.
#[cfg(not(target_arch = "wasm32"))]
pub mod lifecycle;
/// Go-style `log` package: flat `Println` / `Printf` / `Fatal`.
pub mod log;
/// Goroutine-driven future polling (no tokio runtime). Always
/// compiled - drives HTTP/2 + future async stacks.
#[cfg(not(target_arch = "wasm32"))]
pub mod runtime_future;

/// Chaining combinators for `Option<T>`. F#-style free functions
/// with data-last argument order so they thread through `|>`.
/// See SPEC §10.4a.
pub mod option;

/// Chaining combinators for `Result<T, E>`. F#-style free functions
/// with data-last argument order so they thread through `|>`.
/// See SPEC §10.4b. The `?` operator (SPEC §4.5) remains the right
/// tool for short-circuit propagation.
pub mod result;

pub mod json;
pub mod manifest;
/// Mathematical constants, f64 functions, and integer bit operations.
pub mod math;
pub mod mathrand;
/// Prometheus-compatible metrics primitives + registry (`std::metrics`).
pub mod metrics;
/// Media type parsing + extension lookup (`std::mime`).
pub mod mime_types;
#[cfg(not(target_arch = "wasm32"))]
pub mod net;
/// On wasm only the pure IP-address parsing submodule of `net` is
/// reachable; the socket types require host I/O that the browser
/// sandbox does not provide.
#[cfg(target_arch = "wasm32")]
pub mod net {
    /// String-level IPv4/IPv6 parsing and classification (`std::net::ip`).
    pub mod ip;
}
/// Typed IP address parsing / classification (`std::net::netip`).
pub mod net_ip_typed;
pub mod os;
/// POSIX user / group lookup (`std::os::user`).
pub mod os_user;
pub mod panic;
pub mod path;
/// Profile generators shared with the compiled tiers. The
/// implementation lives in `gossamer_runtime` so the bytecode VM
/// and the C-ABI shims render from one set of counters.
pub mod pprof {
    pub use gossamer_runtime::pprof::{
        Frame, ProfileBuffer, Sample, block_profile, execution_trace, goroutine_profile,
        mutex_profile, route,
    };
}
/// Child processes and process control (Rust `std::process` shape).
pub mod process;
pub mod regex;
pub mod registry;
pub mod runtime;
pub mod sched_global;
pub mod signal;
pub mod slog;
pub mod sort;
pub mod strconv;
pub mod strings;
pub mod sync;
pub mod testing;
pub mod text;
/// OS thread spawn and sleep helpers.
pub mod thread;
pub mod time;
#[cfg(not(target_arch = "wasm32"))]
pub mod tls;
/// W3C trace-context tracing primitives + OTLP JSON exporter
/// (`std::trace`).
#[cfg(not(target_arch = "wasm32"))]
pub mod trace;
pub mod unicode;
pub mod url;
pub mod utf16;
pub mod utf8;
/// UUID generation (v4 random, v7 timestamp-ordered) plus parse/normalize.
#[cfg(not(target_arch = "wasm32"))]
pub mod uuid;
/// Validation framework: `Validate` trait, builtins (length, range, email, regex, ...).
pub mod validate;

pub use registry::{
    StdItem, StdItemKind, StdItemRecord, StdModule, item, item_records, module, modules,
};
