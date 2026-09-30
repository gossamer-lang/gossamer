# `std::http`

Status: experimental

HTTP/1.1 and HTTP/2 client and server. HTTP/2 negotiates via ALPN over TLS automatically (Go-style); h2c entry points are explicit. Write a handler as a cohort and an arena: `cohort { }` joins or cancels every goroutine the request spawned before the response is written, and its first child failure becomes the block's `Err` for the handler to turn into a status; `arena { }` bump-allocates what the request builds and frees it wholesale on every exit path, with escape checked at compile time. Dependency injection is closure capture - build the router from closures capturing the pool and the configuration.

## Items

| Item | Signature | Description |
|---|---|---|
| `Request` | `type Request` | HTTP request value passed to a handler. Fields: `method`, `path`, `query`, `query_pairs`, `headers`, `body`, `raw_body`, `peer_addr`. |
| `Response` | `type Response` | HTTP response value returned from a handler. |
| `Method` | `type Method` | HTTP method enumeration. |
| `StatusCode` | `type StatusCode` | HTTP status code. |
| `Headers` | `type Headers` | Case-insensitive header map. |
| `Server` | `type Server` | A configured server. `http::serve(addr, handler)` keeps every default; build a `Server` when a deployment needs its own budgets, its bound address read back, or a shutdown it drives. Setters chain and answer the server: `read_header_timeout_ms`, `read_body_timeout_ms`, `write_timeout_ms`, `idle_timeout_ms`, `request_timeout_ms`, `max_header_bytes`, `max_body_bytes`, `max_connections`, `server_name`. Then `listen(addr) -> Result<(), Error>` binds - so `addr() -> String` reads back a port-0 assignment before `serve(handler)` blocks - and `shutdown(deadline_ms) -> bool` stops accepting and waits for in-flight requests, answering whether the drain finished. |
| `serve` | `fn serve(addr: String, handler: http::Handler) -> Result<(), errors::Error>` | Convenience: bind and serve an HTTP handler. `Result<(), Error>` - a bind failure is an Err value. |
| `serve_tls` | `fn serve_tls(addr: String, cert_pem: String, key_pem: String, handler: http::Handler) -> Result<(), errors::Error>` | TLS-terminating server: `serve_tls(addr, cert_pem, key_pem, handler) -> Result<(), Error>`. Builds a rustls config from the PEM cert chain + key and serves HTTPS with the same handler contract as `serve`. |
| `Client` | `type Client` | HTTP client; configure redirects and timeout via `Client::builder()`. |
| `ResponseStream` | `type ResponseStream` | A streaming response body. `http::stream` produces one over an upstream response, read with `next_line` / `next_chunk`; `ResponseStream::new()` opens one the handler writes itself, with `write(text)` and `write_bytes(bytes)` answering the queued byte count (or -1 once closed), `is_open()` saying whether the client is still there, and `close()` ending the body. Either kind is consumed by `Response::stream(status, content_type, body)` - write from a goroutine and the handler answers immediately while the bytes are framed as they arrive. |
| `request` | `fn request(method: String, url: String, body: String, headers: Vec<(String, String)>) -> Result<http::Response, errors::Error>` | One-shot request with a string body: `(method, url, body, headers) -> Result<Response, Error>`. |
| `request_bytes` | `fn request_bytes(method: String, url: String, body: Vec<u8>, headers: Vec<(String, String)>) -> Result<http::Response, errors::Error>` | One-shot request with a byte body: `(method, url, body: [u8], headers) -> Result<Response, Error>`. |
| `stream` | `fn stream(method: String, url: String, body: String, headers: Vec<(String, String)>) -> Result<http::ResponseStream, errors::Error>` | One-shot request read incrementally: `(method, url, body, headers) -> Result<ResponseStream, Error>`. |
| `get` | `fn get(url: String, headers: Vec<(String, String)>) -> Result<http::Response, errors::Error>` | One-shot GET: `(url, headers) -> Result<Response, Error>`. |
| `post` | `fn post(url: String, body: String, content_type: String) -> Result<http::Response, errors::Error>` | One-shot POST: `(url, body, content_type) -> Result<Response, Error>`. |
| `put` | `fn put(url: String, body: String, content_type: String) -> Result<http::Response, errors::Error>` | One-shot PUT: `(url, body, content_type) -> Result<Response, Error>`. |
| `delete` | `fn delete(url: String, body: String, headers: Vec<(String, String)>) -> Result<http::Response, errors::Error>` | One-shot DELETE: `(url, body, headers) -> Result<Response, Error>`. |
| `head` | `fn head(url: String, headers: Vec<(String, String)>) -> Result<http::Response, errors::Error>` | One-shot HEAD: `(url, headers) -> Result<Response, Error>`. |
| `options` | `fn options(url: String, headers: Vec<(String, String)>) -> Result<http::Response, errors::Error>` | One-shot OPTIONS: `(url, headers) -> Result<Response, Error>`. |
| `Http2Handler` | `trait Http2Handler` | Bounded-body HTTP/2 handler: serve(Request) -> Response. |
| `Http2StreamingHandler` | `trait Http2StreamingHandler` | Chunked-body HTTP/2 handler: serve(Request, StreamingResponseWriter) -> Result. |
| `StreamingResponseWriter` | `type StreamingResponseWriter` | Streaming HTTP/2 response writer; set_status / header / write_chunk / finish. |
| `Http2Config` | `type Http2Config` | Per-connection HTTP/2 tuning (window sizes, max concurrent streams, frame caps). |
| `Http2ServerHandle` | `type Http2ServerHandle` | Handle to a running HTTP/2 connection for shutdown / in-flight counts. |
| `Http2Error` | `type Http2Error` | HTTP/2 server error: Io, Protocol, Handler. |
| `serve_h2c` | `fn serve_h2c(addr: String, handler: http::Handler) -> Result<(), errors::Error>` | Bind a plain-TCP listener and serve h2c (HTTP/2 cleartext). |
| `Trailers` | `type Trailers` | HTTP/2 trailing HEADERS (alias for Headers) - used by `ResponseWriter::write_trailers` and `Request::trailers`. |
| `PushOptions` | `type PushOptions` | Prioritization knobs for `ResponseWriter::push_promise` (weight, depends_on, exclusive). |
| `PushStream` | `type PushStream` | Server-initiated push stream returned by `ResponseWriter::push_promise`. Supports send_head / write / write_trailers / end. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## `Request`

The value a handler receives. Identical fields on every tier.

| Field | Type | Meaning |
|---|---|---|
| `method` | `String` | Canonical uppercase method (`"GET"`, `"POST"`, ...). |
| `path` | `String` | Request path with the query string stripped (`/users`, never `/users?page=2`). |
| `query` | `String` | Raw query string without the leading `?`; empty when absent. |
| `query_pairs` | `[(String, String)]` | Percent-decoded `(key, value)` pairs in query order; repeated keys preserved. |
| `headers` | `[(String, String)]` | Inbound headers; names lowercased, values trimmed. Repeats of the same name collapse to the last value. |
| `body` | `String` | Request body as UTF-8 (lossy - invalid sequences become U+FFFD). |
| `raw_body` | `[u8]` | Exact body bytes; use this for binary uploads or NUL-embedded payloads. |

```text
fn serve(&self, r: http::Request) -> http::Response {
    let who = header_of(r.headers, "x-user")     // names arrive lowercased
    let upload: [u8] = r.raw_body                 // byte-exact body
    http::Response::text(200, format("{} {} q={}", r.method, r.path, r.query))
}
```

## `Response`

### Constructors

- `Response::text(status, body)` - sets `content-type: text/plain; charset=utf-8`.
- `Response::json(status, body)` - sets `content-type: application/json`.
- `Response::stream(status, content_type, rs)` - streamed body; see below.
- `http::Response { status, body, content_type }` - constructs a binary
  response without a lossy round-trip through `String`; `body` may be `[u8]`.

```text
http::Response {
    status: 202,
    body: [65, 0, 66],
    content_type: "application/octet-stream",
}.with_header("x-k", "kv")
```

The byte array is written verbatim, so handlers can return binary payloads
(images, gzip) including embedded NUL bytes.

### `with_header` chaining

`with_header(name, value)` returns a new Response with the pair applied:
any existing pair whose name matches case-insensitively is removed first,
then the new pair is appended (replace-then-push). The last write for a
given name wins:

```text
let r = http::Response::text(201, "made")
    .with_header("X-Tag", "v1")
    .with_header("x-tag", "v2")     // replaces v1
    .with_header("x-extra", "e")
// r.headers carries ("x-tag", "v2") and ("x-extra", "e")
```

An explicit `content-type` entry in `headers` overrides the `content_type`
field; with neither set the default is `text/plain; charset=utf-8`.

### Client-side `Response` fields

Responses returned by the client carry:

| Field | Type | Meaning |
|---|---|---|
| `status` | `i64` | Numeric status code. |
| `body` | `String` | Body as UTF-8 (lossy). |
| `raw_bytes` | `[u8]` | Exact body bytes - the binary-safe counterpart of `body`. |
| `content_type` | `String` | The `content-type` header, `"text/plain"` when absent. |
| `location` | `String` | The `location` header (useful with redirect-following disabled). |
| `headers` | `[(String, String)]` | Response headers: lowercase names, wire order, duplicates preserved (`set-cookie` repeats survive). |

## Streamed responses - `Response::stream`

`Response::stream(status, content_type, rs)` takes a `ResponseStream`
obtained from `http::stream` and serves it as a chunked response: the
server writes the head, then drains the upstream reader to the client in
chunked frames with a flush after each one, so bytes flow end-to-end
without buffering the body.

Consume semantics: constructing `Response::stream` consumes the
`ResponseStream`. After construction, `next_line` / `next_chunk` on that
stream return `None`, and a stream serves exactly one response - handing
the same stream to a second `Response::stream` produces an empty body.

```text
fn forward_stream(method: String, target: String, body: String,
                  headers: [(String, String)]) -> http::Response {
    match http::stream(method, target, body, headers) {
        Ok(up) => http::Response::stream(up.status, up.content_type, up),
        Err(e) => http::Response::text(502, format("upstream error: {}", e)),
    }
}
```

## HTTP/2 request streaming

Status: experimental.

The Rust-side HTTP/2 module contains a bounded, flow-control-aware
`RequestStreamingHandler` scaffold for incremental request bodies, including
chunk reads, trailers, stream deadlines, and receive-capacity release. The
public Gossamer handler contract still receives a complete bounded `Request`
on VM and AOT, so request-body streaming is not a shipped cross-tier API yet.
Use `Request.raw_body` for bounded uploads and keep handlers within the
configured body cap until the public streaming handler ABI lands.

## Handlers and `serve`

A handler is a struct implementing `http::Handler` with a
`serve(self, Request)` method. Both return shapes work, on every tier:

```text
impl http::Handler for App {
    fn serve(&self, r: http::Request) -> http::Response { ... }          // bare
}

impl http::Handler for Api {
    fn serve(&self, r: http::Request) -> Result<http::Response, http::Error> {
        Ok(http::Response::text(200, "ok"))                               // Result
    }
}
```

`http::serve(addr, handler) -> Result<(), Error>`: a bind failure (port in
use, bad address) is an `Err` value for the caller's match - not a panic -
and `Ok(())` is returned on graceful shutdown:

```text
if let Err(e) = http::serve("127.0.0.1:8080", app) {
    eprintln("{}", e)
}
```

## Server behavior

- **Body cap.** Inbound bodies are capped at 1 MiB by default
  (`Config::max_body_bytes`); a request declaring or streaming more is
  rejected with `413 Payload Too Large` and the connection closes. The
  header block is capped at 8 KiB.
- **Chunked inbound.** `Transfer-Encoding: chunked` request bodies are
  decoded before the handler runs; trailer headers are merged into
  `request.headers` with the same lowercase semantics. A request carrying
  both `Transfer-Encoding: chunked` and `Content-Length` is
  smuggling-shaped and rejected with `400 Bad Request`.
- **Malformed requests** (unparseable request line or headers) get
  `400 Bad Request` and the connection closes.
- **Wire casing.** Response header names are written lowercase on the
  wire on every tier, so byte-level assertions are tier-portable.
- **Keep-alive.** HTTP/1.1 connections are kept alive by default; the
  server inserts `connection: keep-alive` (or `close` for HTTP/1.0,
  client-requested close, or a handler-set `connection: close`).
  `Expect: 100-continue` is answered before the body is read.

## One-shot request functions

```text
http::request(method: String, url: String, body: String,
              headers: [(String, String)]) -> Result<Response, Error>
http::request_bytes(method: String, url: String, body: [u8],
                    headers: [(String, String)]) -> Result<Response, Error>
http::stream(method: String, url: String, body: String,
             headers: [(String, String)]) -> Result<ResponseStream, Error>
```

Method strings are case-insensitive (`"GET"`, `"post"`, ...). An unknown
method fails before any connection is dialed:
`Err("http::request: unknown method `BOGUS`")` (same shape for
`request_bytes` / `stream` with their own prefixes). Network-level
failures render as `Err("http: transport: ...")` - connection refused,
DNS, TLS, timeouts, and exhausted redirect budgets all use that prefix.

An empty `body` / empty byte array sends no request body.
