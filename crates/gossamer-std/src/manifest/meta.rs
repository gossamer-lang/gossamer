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
    clippy::needless_collect,
    clippy::elidable_lifetime_names,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::missing_const_for_fn,
    clippy::needless_range_loop,
    clippy::ptr_arg,
    clippy::ptr_as_ptr,
    clippy::redundant_closure,
    clippy::redundant_closure_for_method_calls,
    clippy::semicolon_if_nothing_returned,
    clippy::single_call_fn,
    clippy::unused_self,
    clippy::range_plus_one,
    clippy::missing_safety_doc,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::cast_ptr_alignment,
    clippy::manual_assert,
    clippy::manual_string_new,
    clippy::match_bool,
    clippy::nonminimal_bool,
    clippy::redundant_pattern_matching,
    clippy::useless_let_if_seq
)]
//! Static manifest of every registered stdlib module.
//! Each stdlib milestone extends this table with
//! the modules it adds. Entries are listed in phase-introduction order
//! so a `gos doc` walk renders modules in the same sequence as the
//! implementation plan.

#![forbid(unsafe_code)]
use crate::registry::{StdItem, StdItemKind, StdModule};

use super::*;

pub const RUNTIME: StdModule = StdModule {
    path: "std::runtime",
    summary: "Goroutine / scheduler introspection and tuning.",
    items: &[
        StdItem {
            name: "collect_cycles",
            kind: StdItemKind::Function,
            doc: "Requests collection of unreachable reference cycles; returns `()`.",
        },
        StdItem {
            name: "cycle_collection_supported",
            kind: StdItemKind::Function,
            doc: "Reports whether this execution tier reclaims unreachable reference cycles.",
        },
        StdItem {
            name: "scheduler_stats_json",
            kind: StdItemKind::Function,
            doc: "Returns a compact JSON snapshot of goroutine scheduler counters.",
        },
        StdItem {
            name: "arena_push",
            kind: StdItemKind::Function,
            doc: "Opens an arena region for bump allocation.",
        },
        StdItem {
            name: "arena_pop",
            kind: StdItemKind::Function,
            doc: "Closes the innermost arena region, freeing its slabs.",
        },
        StdItem {
            name: "set_panic_hook",
            kind: StdItemKind::Function,
            doc: "Installs a hook invoked with the message on panic.",
        },
        StdItem {
            name: "cohort_push",
            kind: StdItemKind::Function,
            doc: "Opens a cohort on the running goroutine. Written `cohort { }` in source; the block desugars to this plus `cohort_join` and a deferred `cohort_pop`.",
        },
        StdItem {
            name: "cohort_join",
            kind: StdItemKind::Function,
            doc: "Waits for every child of the running goroutine's cohort and answers `Result<(), errors::Error>`.",
        },
        StdItem {
            name: "cohort_pop",
            kind: StdItemKind::Function,
            doc: "Closes the running goroutine's cohort, cancelling and joining anything still running.",
        },
        StdItem {
            name: "cohorts",
            kind: StdItemKind::Function,
            doc: "One descriptor line per live cohort, oldest id first: id, parent, completion policy, error disposition, outstanding count, and the spawn indices still running. A cohort is enumerable so a program can say what it is waiting on without joining it.",
        },
        StdItem {
            name: "root",
            kind: StdItemKind::Function,
            doc: "The root cohort's descriptor line, in the shape `cohorts()` answers, or an empty String when no cohort is open. The root is the one cohort every program has - `main` runs inside it - and the one whose drain bounds process exit.",
        },
        StdItem {
            name: "cohort_cancelled",
            kind: StdItemKind::Function,
            doc: "Whether the running goroutine's cohort has been cancelled. A CPU-bound child polls this to cooperate at a point of its own choosing.",
        },
        StdItem {
            name: "cohort_cancel",
            kind: StdItemKind::Function,
            doc: "Cancels the running goroutine's cohort, winding its siblings down without failing it.",
        },
    ],
};

pub const ERRORS: StdModule = StdModule {
    path: "std::errors",
    summary: "Error construction, wrapping, and chain traversal.",
    items: &[
        StdItem {
            name: "Error",
            kind: StdItemKind::Type,
            doc: "Error value carrying a message, an optional cause, and structured diagnostic fields. Methods: `message() -> String` (top message only), `cause() -> Option<Error>`, `chain() -> Vec<Error>` (self then every ancestor cause), `is(needle) -> bool`, `with_field(key, value) -> Error` (an immutable copy carrying one more field; re-setting a key replaces its value), `field(key) -> Option<String>`, `fields() -> Vec<(String, String)>` in insertion order. `{}` renders the colon-joined chain. Example: `let e = errors::new(\"query failed\").with_field(\"sqlstate\", \"23505\")`.",
        },
        StdItem {
            name: "new",
            kind: StdItemKind::Function,
            doc: "Constructs a fresh error from a message.",
        },
        StdItem {
            name: "newf",
            kind: StdItemKind::Function,
            doc: "Constructs a fresh error from a format template, e.g. `newf(\"status {}\", code)`.",
        },
        StdItem {
            name: "wrap",
            kind: StdItemKind::Function,
            doc: "Wraps a cause with a higher-level message.",
        },
        StdItem {
            name: "is",
            kind: StdItemKind::Function,
            doc: "`is(error, needle) -> bool` - true when `needle` matches `error` or any link of its cause chain. Prefer a sentinel error VALUE, which matches by identity so two errors sharing a message stay distinct: `let NOT_FOUND = errors::new(\"not found\")`, then `errors::is(err, NOT_FOUND)`. A String `needle` falls back to a message substring test.",
        },
        StdItem {
            name: "join",
            kind: StdItemKind::Function,
            doc: "Joins a list of errors into one; messages are joined with \"; \" (None for an empty list).",
        },
    ],
};

pub const FLAG: StdModule = StdModule {
    path: "std::flag",
    summary: "Batteries-included CLI argument parsing.",
    items: &[
        StdItem {
            name: "Value",
            kind: StdItemKind::Type,
            doc: "Parsed command-line flag value.",
        },
        StdItem {
            name: "Set",
            kind: StdItemKind::Type,
            doc: "Flag definition + parsing set.",
        },
        StdItem {
            name: "Error",
            kind: StdItemKind::Type,
            doc: "Error produced while parsing flags.",
        },
        StdItem {
            name: "parse",
            kind: StdItemKind::Function,
            doc: "Parses the default flag set against the given args.",
        },
        StdItem {
            name: "string",
            kind: StdItemKind::Function,
            doc: "Defines a string flag on the default set.",
        },
        StdItem {
            name: "int",
            kind: StdItemKind::Function,
            doc: "Defines an integer flag on the default set.",
        },
        StdItem {
            name: "bool",
            kind: StdItemKind::Function,
            doc: "Defines a boolean flag on the default set.",
        },
        StdItem {
            name: "define",
            kind: StdItemKind::Function,
            doc: "Registers a flag definition on the default set.",
        },
    ],
};

pub const SLOG: StdModule = StdModule {
    path: "std::slog",
    summary: "Structured, levelled logging.",
    items: &[
        StdItem {
            name: "Logger",
            kind: StdItemKind::Type,
            doc: "Logger handle.",
        },
        StdItem {
            name: "Field",
            kind: StdItemKind::Type,
            doc: "Key/value pair threaded through a logger.",
        },
        StdItem {
            name: "TextHandler",
            kind: StdItemKind::Type,
            doc: "Line-oriented handler.",
        },
        StdItem {
            name: "JsonHandler",
            kind: StdItemKind::Type,
            doc: "JSON-lines handler.",
        },
        StdItem {
            name: "info",
            kind: StdItemKind::Function,
            doc: "Logs a JSON record at INFO level. Trailing args are key/value pairs.",
        },
        StdItem {
            name: "warn",
            kind: StdItemKind::Function,
            doc: "Logs a JSON record at WARN level.",
        },
        StdItem {
            name: "error",
            kind: StdItemKind::Function,
            doc: "Logs a JSON record at ERROR level.",
        },
        StdItem {
            name: "debug",
            kind: StdItemKind::Function,
            doc: "Logs a JSON record at DEBUG level.",
        },
    ],
};

pub const TESTING: StdModule = StdModule {
    path: "std::testing",
    summary: "Assertions and sub-test harness helpers.",
    items: &[
        StdItem {
            name: "Runner",
            kind: StdItemKind::Type,
            doc: "Sub-test collector.",
        },
        StdItem {
            name: "check",
            kind: StdItemKind::Function,
            doc: "Asserts a condition.",
        },
        StdItem {
            name: "check_eq",
            kind: StdItemKind::Function,
            doc: "Asserts equality, rendering a diff on failure.",
        },
        StdItem {
            name: "check_ok",
            kind: StdItemKind::Function,
            doc: "Asserts a Result is Ok, recording without panicking.",
        },
        StdItem {
            name: "wait_for_scheduler_idle",
            kind: StdItemKind::Function,
            doc: "Waits for the scheduler to become idle within a timeout.",
        },
    ],
};

pub const HTTPTEST: StdModule = StdModule {
    path: "std::httptest",
    summary: "Fixtures for testing HTTP code. A handler is a function from a request to a response, so `record` calls one in memory; a test that is about the wire builds an `http::Server`, binds port 0, and reads the address back.",
    items: &[
        StdItem {
            name: "record",
            kind: StdItemKind::Function,
            doc: "`record(handler, method, path, body) -> Result<Response, Error>` - calls `handler` with a request built in memory and answers its response. No socket, no port, no accept loop.",
        },
        StdItem {
            name: "server",
            kind: StdItemKind::Function,
            doc: "server(status, body) -> String: starts an isolated loopback static-response server and returns its http:// base URL. Use http::get or http::Client as the test client. The server is test-process scoped and stops when that process exits. For a programmable server, build an `http::Server` over a `Router` and bind port 0.",
        },
    ],
};

pub const IMAGE: StdModule = StdModule {
    path: "std::image",
    summary: "Opaque RGBA8 image handles with PNG and JPEG codecs.",
    items: &[
        StdItem {
            name: "new",
            kind: StdItemKind::Function,
            doc: "Allocates a transparent image handle.",
        },
        StdItem {
            name: "filled",
            kind: StdItemKind::Function,
            doc: "Allocates an image handle filled with a packed 0xRRGGBBAA colour.",
        },
        StdItem {
            name: "decode_base64",
            kind: StdItemKind::Function,
            doc: "Decodes a base64 PNG or JPEG; returns zero for malformed input.",
        },
        StdItem {
            name: "width",
            kind: StdItemKind::Function,
            doc: "Returns an image width in pixels.",
        },
        StdItem {
            name: "height",
            kind: StdItemKind::Function,
            doc: "Returns an image height in pixels.",
        },
        StdItem {
            name: "pixel",
            kind: StdItemKind::Function,
            doc: "Returns packed 0xRRGGBBAA, or -1 outside the image.",
        },
        StdItem {
            name: "set_pixel",
            kind: StdItemKind::Function,
            doc: "Sets a packed 0xRRGGBBAA pixel and reports whether it was in bounds.",
        },
        StdItem {
            name: "encode_png_base64",
            kind: StdItemKind::Function,
            doc: "Encodes an image as lossless base64 PNG.",
        },
        StdItem {
            name: "encode_jpeg_base64",
            kind: StdItemKind::Function,
            doc: "Encodes an image as base64 JPEG at quality 1 through 100.",
        },
    ],
};

pub const TIME: StdModule = StdModule {
    path: "std::time",
    summary: "Wall-clock and monotonic time facilities.",
    items: &[
        StdItem {
            name: "Instant",
            kind: StdItemKind::Type,
            doc: "A monotonic clock reading in nanoseconds: `now()`, `elapsed()`, `elapsed_ms()`, `duration_since(earlier)`.",
        },
        StdItem {
            name: "Duration",
            kind: StdItemKind::Type,
            doc: "A span of nanoseconds: `from_nanos` / `from_micros` / `from_millis` / `from_secs` / `from_secs_f64`, the matching `as_*` accessors, `+`, `-`, and ordering.",
        },
        StdItem {
            name: "SystemTime",
            kind: StdItemKind::Type,
            doc: "Wall-clock point-in-time.",
        },
        StdItem {
            name: "CivilTime",
            kind: StdItemKind::Type,
            doc: "Calendar fields interpreted only with an explicit location.",
        },
        StdItem {
            name: "CivilResolution",
            kind: StdItemKind::Type,
            doc: "Explicit unique, gap, or fold result of resolving civil fields.",
        },
        StdItem {
            name: "Location",
            kind: StdItemKind::Type,
            doc: "Immutable IANA or fixed-offset time-zone location.",
        },
        StdItem {
            name: "sleep",
            kind: StdItemKind::Function,
            doc: "Suspends the current goroutine for a `Duration`, or for an integer count of milliseconds.",
        },
        StdItem {
            name: "after",
            kind: StdItemKind::Function,
            doc: "A `Receiver` that yields once after `Duration`, so a deadline composes with `select` and `while let` the way any other channel does.",
        },
        StdItem {
            name: "sleep_ctx",
            kind: StdItemKind::Function,
            doc: "Sleeps unless the context fires first; false when it cancelled the wait.",
        },
        StdItem {
            name: "__sleep_ns",
            kind: StdItemKind::Builtin,
            doc: "Internal: suspends the current goroutine for a nanosecond count; the lowering of `sleep` and `sleep_ctx`.",
        },
        StdItem {
            name: "__sleep_ns_ctx",
            kind: StdItemKind::Builtin,
            doc: "Internal: context-aware nanosecond sleep; the lowering of `sleep_ctx` that wakes early when the context fires.",
        },
        StdItem {
            name: "now",
            kind: StdItemKind::Function,
            doc: "Wall-clock milliseconds since the Unix epoch; `Instant::now()` reads the monotonic clock.",
        },
        StdItem {
            name: "format_rfc3339",
            kind: StdItemKind::Function,
            doc: "Formats a `SystemTime` in RFC 3339 (`YYYY-MM-DDTHH:MM:SSZ`).",
        },
        StdItem {
            name: "format_in",
            kind: StdItemKind::Function,
            doc: "Formats an instant in an explicit civil-time location.",
        },
        StdItem {
            name: "add_date",
            kind: StdItemKind::Function,
            doc: "Adds calendar units and rejects ambiguous or nonexistent results.",
        },
        StdItem {
            name: "parse_rfc3339",
            kind: StdItemKind::Function,
            doc: "Parses an RFC 3339 timestamp into a `SystemTime`.",
        },
        StdItem {
            name: "freeze",
            kind: StdItemKind::Function,
            doc: "`freeze(ms)` - pin the wall clock at `ms` since the epoch, so a test over anything time-dependent - a token's expiry, a rate-limit window, a cache's TTL - reads a clock it controls. The monotonic clock and `sleep` are untouched: this pins what the program is told the time is, not how long anything takes.",
        },
        StdItem {
            name: "advance",
            kind: StdItemKind::Function,
            doc: "`advance(ms) -> i64` - move a frozen wall clock forward and answer the new reading. Freezes at the current reading first when it is not already frozen.",
        },
        StdItem {
            name: "unfreeze",
            kind: StdItemKind::Function,
            doc: "`unfreeze()` - return to the real wall clock.",
        },
        StdItem {
            name: "is_frozen",
            kind: StdItemKind::Function,
            doc: "`is_frozen() -> bool` - whether the wall clock is pinned.",
        },
        StdItem {
            name: "now_ms",
            kind: StdItemKind::Function,
            doc: "Wall-clock milliseconds since the Unix epoch.",
        },
        StdItem {
            name: "now_nanos",
            kind: StdItemKind::Function,
            doc: "Wall-clock nanoseconds since the Unix epoch.",
        },
        StdItem {
            name: "unix_ms",
            kind: StdItemKind::Function,
            doc: "Current Unix time in milliseconds.",
        },
        StdItem {
            name: "monotonic_ms",
            kind: StdItemKind::Function,
            doc: "Monotonic clock reading in milliseconds.",
        },
        StdItem {
            name: "monotonic_nanos",
            kind: StdItemKind::Function,
            doc: "Monotonic clock reading in nanoseconds.",
        },
        StdItem {
            name: "since_ms",
            kind: StdItemKind::Function,
            doc: "Milliseconds elapsed since an earlier monotonic reading.",
        },
    ],
};

pub const PANIC: StdModule = StdModule {
    path: "std::panic",
    summary: "Panic / `catch_unwind` integration.",
    items: &[StdItem {
        name: "panic",
        kind: StdItemKind::Builtin,
        doc: "Aborts the current goroutine with a message.",
    }],
};

pub const UUID: StdModule = StdModule {
    path: "std::uuid",
    summary: "UUID v4 (random) and v7 (timestamp-ordered) generation, parse, and normalize.",
    items: &[
        StdItem {
            name: "v4",
            kind: StdItemKind::Function,
            doc: "Generate a fresh random v4 UUID as a canonical hyphenated string.",
        },
        StdItem {
            name: "v7",
            kind: StdItemKind::Function,
            doc: "Generate a fresh v7 (timestamp-ordered) UUID.",
        },
        StdItem {
            name: "is_valid",
            kind: StdItemKind::Function,
            doc: "Return true iff the string parses as a canonical UUID.",
        },
        StdItem {
            name: "normalize",
            kind: StdItemKind::Function,
            doc: "Lowercase canonical UUID form of the input, or empty string on parse failure.",
        },
        StdItem {
            name: "simple",
            kind: StdItemKind::Function,
            doc: "32-character unhyphenated form of the input, or empty string on parse failure.",
        },
    ],
};

pub const VALIDATE: StdModule = StdModule {
    path: "std::validate",
    summary: "Trait-based field validation: implement Validate, collect FieldErrors into Errors.",
    items: &[
        StdItem {
            name: "Validate",
            kind: StdItemKind::Trait,
            doc: "Implement on a struct to declare field-level validation rules.",
        },
        StdItem {
            name: "FieldError",
            kind: StdItemKind::Type,
            doc: "One field-scoped validation failure: dotted path, message, optional code.",
        },
        StdItem {
            name: "Errors",
            kind: StdItemKind::Type,
            doc: "Aggregated FieldError set, indexable by dotted path.",
        },
    ],
};

pub const METRICS: StdModule = StdModule {
    path: "std::metrics",
    summary: "Prometheus-compatible primitives (Counter, Gauge, Histogram) and a Registry rendering the standard text-exposition format.",
    items: &[
        StdItem {
            name: "Counter",
            kind: StdItemKind::Type,
            doc: "Monotonic-increasing u64 counter (lock-free).",
        },
        StdItem {
            name: "Gauge",
            kind: StdItemKind::Type,
            doc: "Set / inc / dec gauge (lock-free).",
        },
        StdItem {
            name: "Histogram",
            kind: StdItemKind::Type,
            doc: "Bucketed observation histogram with sum and count.",
        },
        StdItem {
            name: "Metric",
            kind: StdItemKind::Type,
            doc: "Enum holding any of the three primitives for registry storage.",
        },
        StdItem {
            name: "Registry",
            kind: StdItemKind::Type,
            doc: "Ordered collection of metrics; `render()` emits the Prometheus text-exposition format.",
        },
        StdItem {
            name: "serve_metrics",
            kind: StdItemKind::Function,
            doc: "Mounts a registry on `/metrics` over the existing http server loop.",
        },
    ],
};

pub const PPROF: StdModule = StdModule {
    path: "std::pprof",
    summary: "Runtime profiles in the text format `go tool pprof` reads, plus a Chrome-trace scheduler capture.",
    items: &[
        StdItem {
            name: "cpu_profile",
            kind: StdItemKind::Function,
            doc: "Sampled CPU profile over the given milliseconds; a timer reads the running stack.",
        },
        StdItem {
            name: "heap_profile",
            kind: StdItemKind::Function,
            doc: "Sampled allocation profile over the given milliseconds, weighted by bytes.",
        },
        StdItem {
            name: "goroutine_profile",
            kind: StdItemKind::Function,
            doc: "Text profile with one sample per live goroutine and its last-known frame.",
        },
        StdItem {
            name: "mutex_profile",
            kind: StdItemKind::Function,
            doc: "Text profile of microseconds parked on synchronization since process start.",
        },
        StdItem {
            name: "block_profile",
            kind: StdItemKind::Function,
            doc: "Text profile of microseconds parked on channels, I/O, and timers since process start.",
        },
        StdItem {
            name: "execution_trace",
            kind: StdItemKind::Function,
            doc: "Chrome trace JSON of scheduler spawn/park/unpark events; blocks for the given milliseconds.",
        },
        StdItem {
            name: "route",
            kind: StdItemKind::Function,
            doc: "Serves a `/debug/pprof/...` path, returning the body to write, or `None` for an unknown path.",
        },
    ],
};

pub const TRACE: StdModule = StdModule {
    path: "std::trace",
    summary: "W3C trace-context-compatible distributed tracing. Identifier types, request-scoped SpanContext, process-level Tracer, and OTLP JSON export.",
    items: &[
        StdItem {
            name: "TraceId",
            kind: StdItemKind::Type,
            doc: "128-bit trace identifier (W3C trace-context format).",
        },
        StdItem {
            name: "SpanId",
            kind: StdItemKind::Type,
            doc: "64-bit span identifier.",
        },
        StdItem {
            name: "SpanContext",
            kind: StdItemKind::Type,
            doc: "Request-scoped trace + span pair, propagated through `std::context`.",
        },
        StdItem {
            name: "SpanStatus",
            kind: StdItemKind::Type,
            doc: "Span outcome: Unset / Ok / Error(message).",
        },
        StdItem {
            name: "Span",
            kind: StdItemKind::Type,
            doc: "Active span builder. Attributes, events, status; `end()` finalises and records.",
        },
        StdItem {
            name: "EndedSpan",
            kind: StdItemKind::Type,
            doc: "Finalised span record; `to_otlp_json()` serialises for OTLP/HTTP export.",
        },
        StdItem {
            name: "Tracer",
            kind: StdItemKind::Type,
            doc: "Process-level span sink. `start_span`, `ended_spans`, `set_global`.",
        },
        StdItem {
            name: "SpanGuard",
            kind: StdItemKind::Type,
            doc: "RAII guard returned by `enter_span`; restores the prior active span on drop.",
        },
    ],
};
