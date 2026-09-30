# `std::pprof`

Status: experimental

Runtime profiles in the text format `go tool pprof` reads, plus a Chrome-trace scheduler capture.

## Items

| Item | Signature | Description |
|---|---|---|
| `cpu_profile` | `fn cpu_profile(millis: i64) -> String` | Sampled CPU profile over the given milliseconds; a timer reads the running stack. |
| `heap_profile` | `fn heap_profile(millis: i64) -> String` | Sampled allocation profile over the given milliseconds, weighted by bytes. |
| `goroutine_profile` | `fn goroutine_profile() -> String` | Text profile with one sample per live goroutine and its last-known frame. |
| `mutex_profile` | `fn mutex_profile() -> String` | Text profile of microseconds parked on synchronization since process start. |
| `block_profile` | `fn block_profile() -> String` | Text profile of microseconds parked on channels, I/O, and timers since process start. |
| `execution_trace` | `fn execution_trace(millis: i64) -> String` | Chrome trace JSON of scheduler spawn/park/unpark events; blocks for the given milliseconds. |
| `route` | `fn route(path: String, query: String) -> Option<String>` | Serves a `/debug/pprof/...` path, returning the body to write, or `None` for an unknown path. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## Formats

The three profiles render the legacy text shape `go tool pprof -text` reads:

```text
# pprof text format v1
samples=N self=N
  function file:line
```

`execution_trace` returns a Chrome trace object (`{"traceEvents":[...]}`) that `chrome://tracing` and Perfetto load directly.

## Mounting the endpoints

`route` answers the paths Go's `net/http/pprof` uses, so a handler can forward straight to it:

```text
use std::http
use std::pprof

fn debug_handler(r: http::Request) -> Result<http::Response, errors::Error> {
    match pprof::route(r.path, r.query) {
        Some(body) => Ok(http::Response::text(200, body))
        None => Ok(http::Response::text(404, "not found"))
    }
}
```

Paths served: `/debug/pprof/` (index), `/debug/pprof/goroutine`, `/debug/pprof/mutex`, `/debug/pprof/block`, and `/debug/pprof/trace?seconds=N`.

## Sampling

`cpu_profile` and `heap_profile` are sampled rather than read from live state. A `SIGPROF` timer interrupts the running thread at 100 Hz and the handler walks the frame-pointer chain into a fixed buffer, allocating nothing and taking no lock; addresses are resolved to names when the profile is drained. `heap_profile` records one stack per 512 KiB allocated, from inside the global allocator, which is why it uses the same allocation-free walk.

Sampling costs nothing until it is asked for. Instrumenting every function instead would cost about 2.7x on call-heavy code, because an opaque call at each entry is an inlining barrier; frame pointers, which the walk needs, measured free.
