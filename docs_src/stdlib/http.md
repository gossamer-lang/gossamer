# `std::http`

Status: experimental

HTTP/1.1 and HTTP/2 client and server. HTTP/2 negotiates via ALPN over TLS automatically (Go-style); h2c entry points are explicit. Write a handler as a cohort and an arena: `cohort { }` joins or cancels every goroutine the request spawned before the response is written, and its first child failure becomes the block's `Err` for the handler to turn into a status; `arena { }` bump-allocates what the request builds and frees it wholesale on every exit path, with escape checked at compile time. Dependency injection is closure capture - build the router from closures capturing the pool and the configuration.

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## API details and source

The [implementation source](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) contains the complete declarations and implementation notes. The table below lists canonical Gossamer call signatures; every item name links directly to its implementation file.

| Item | Canonical signature or declaration | Description |
|---|---|---|
| [`Client`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type Client` | HTTP client; configure redirects and timeout via `Client::builder()`. |
| [`Headers`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type Headers` | Case-insensitive header map. |
| [`Http2Config`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type Http2Config` | Per-connection HTTP/2 tuning (window sizes, max concurrent streams, frame caps). |
| [`Http2Error`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type Http2Error` | HTTP/2 server error: Io, Protocol, Handler. |
| [`Http2Handler`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `trait Http2Handler` | Bounded-body HTTP/2 handler: serve(Request) -> Response. |
| [`Http2ServerHandle`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type Http2ServerHandle` | Handle to a running HTTP/2 connection for shutdown / in-flight counts. |
| [`Http2StreamingHandler`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `trait Http2StreamingHandler` | Chunked-body HTTP/2 handler: serve(Request, StreamingResponseWriter) -> Result. |
| [`Method`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type Method` | HTTP method enumeration. |
| [`PushOptions`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type PushOptions` | Prioritization knobs for `ResponseWriter::push_promise` (weight, depends_on, exclusive). |
| [`PushStream`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type PushStream` | Server-initiated push stream returned by `ResponseWriter::push_promise`. Supports send_head / write / write_trailers / end. |
| [`Request`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type Request` | HTTP request value passed to a handler. |
| [`Response`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type Response` | HTTP response value returned from a handler. |
| [`ResponseStream`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type ResponseStream` | Streaming response body from `http::stream`; `next_line` / `next_chunk`, consumed by `Response::stream`. |
| [`Server`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type Server` | HTTP server bound to a TCP listener. |
| [`StatusCode`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type StatusCode` | HTTP status code. |
| [`StreamingResponseWriter`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type StreamingResponseWriter` | Streaming HTTP/2 response writer; set_status / header / write_chunk / finish. |
| [`Trailers`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type Trailers` | HTTP/2 trailing HEADERS (alias for Headers) - used by `ResponseWriter::write_trailers` and `Request::trailers`. |
| [`Reader`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type Reader` | Decodes a chunked body from any Read source (Rust-side; streaming). |
| [`Writer`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type Writer` | Encodes raw bytes into chunked frames over any Write sink (Rust-side; streaming). |
| [`decode`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn decode(body: String) -> String` | One-shot: concatenates data chunks from a complete chunked body. Available in interp + compiled. |
| [`encode`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn encode(body: String) -> String` | One-shot: wraps a buffer in chunked transfer-encoding with terminator. Available in interp + compiled. |
| [`Cookie`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type Cookie` | Parsed cookie with name, value, and Set-Cookie attributes. |
| [`CookieBuilder`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type CookieBuilder` | Fluent builder for Set-Cookie response headers. |
| [`SameSite`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type SameSite` | SameSite attribute: Strict / Lax / None. |
| [`parse_cookie_header`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn parse_cookie_header(header: String) -> Vec<http::cookie::Cookie>` | Parse a Cookie request header into (name, value) pairs. |
| [`serialize`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn serialize(name: String, value: String) -> String` | Render a Cookie as a Set-Cookie header value. |
| [`Config`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type Config` | Signing key, cookie / header names, and origin allowlist. |
| [`RouteAuth`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type RouteAuth` | Per-route policy: Required, Optional, or Skipped. |
| [`attach_cookie`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn attach_cookie(request: http::Request, secret: String) -> Result<http::Request, errors::Error>` | Set the CSRF cookie on a Response. |
| [`check`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn check(request: http::Request, secret: String) -> Result<http::Request, errors::Error>` | Combined origin + token gate; returns Err on failure. |
| [`extract_token`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn extract_token(request: http::Request, secret: String) -> Result<http::Request, errors::Error>` | Pull a token from the configured header or form field. |
| [`issue_token`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn issue_token(secret: Vec<u8>) -> Result<String, errors::Error>` | Mint a fresh CSRF token bound to the configured signing key. |
| [`origin_allowed`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn origin_allowed(request: http::Request, secret: String) -> Result<http::Request, errors::Error>` | Origin / Referer allowlist check for unsafe methods. |
| [`verify_token`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn verify_token(cookie_token: String, supplied_token: String, secret: Vec<u8>) -> Result<(), errors::Error>` | Constant-time verify of a presented token against the cookie value. |
| [`delete`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn delete(url: String, body: String, headers: Vec<(String, String)>) -> Result<http::Response, errors::Error>` | One-shot DELETE: `(url, body, headers) -> Result<Response, Error>`. |
| [`Form`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type Form` | Parsed url-encoded body, queryable by field name. |
| [`FormBuilder`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type FormBuilder` | Builder for url-encoded request bodies. |
| [`get`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn get(url: String, headers: Vec<(String, String)>) -> Result<http::Response, errors::Error>` | One-shot GET: `(url, headers) -> Result<Response, Error>`. |
| [`head`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn head(url: String, headers: Vec<(String, String)>) -> Result<http::Response, errors::Error>` | One-shot HEAD: `(url, headers) -> Result<Response, Error>`. |
| [`Health`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type Health` | Aggregates a set of named probes into a single status. |
| [`Probe`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `trait Probe` | One health check returning Ok or Err with a short message. |
| [`Chain`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type Chain` | Helper for composing middleware in a single value. |
| [`Handler`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `trait Handler` | Anything serving (Request, Params) -> Response. |
| [`accepts_gzip`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn accepts_gzip(request: http::Request) -> bool` | Check an Accept-Encoding header for a gzip token. Available in interp + compiled. |
| [`bearer_ok`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn bearer_ok(request: http::Request, verify: Fn(String) -> bool) -> bool` | Run a verify closure on the request's Bearer token; false (without calling verify) when no Bearer header is present. Available in interp + compiled. |
| [`decode_basic_auth`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn decode_basic_auth(request: http::Request) -> Option<(String, String)>` | Decode a Basic-auth Authorization header into (user, password). Interp tier. |
| [`new_request_id`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn new_request_id() -> String` | Generate a process-monotonic request id string. Available in interp + compiled. |
| [`tag`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn tag(handler: http::Handler) -> http::Handler` | Wrap a handler (`tag(inner) -> Handler`), prepending `mw:` to each response body. Deterministic composition primitive; available in interp + compiled. |
| [`Config`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type Config` | Per-form size, part-count, and disk-spill limits. |
| [`Form`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type Form` | Parsed multipart envelope: fields + file parts. |
| [`Part`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type Part` | One field or file entry from a multipart body. |
| [`PartData`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type PartData` | In-memory bytes or spilled-to-disk path for a part. |
| [`parse`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn parse(request: http::Request) -> Result<http::multipart::Form, errors::Error>` | Stream-parse from any Read source into a Form. |
| [`Client`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type Client` | Native h1 client (Rust-side; full builder surface). |
| [`Error`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type Error` | Connect / Tls / Http / Redirect / Timeout / Io. |
| [`delete`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn delete(url: String) -> Result<http::Response, errors::Error>` | One-shot DELETE → Result<Response, Error>. Interp tier. |
| [`get`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn get(url: String) -> Result<http::Response, errors::Error>` | One-shot GET → Result<Response, Error>. Interp tier (compiled tier shares http::get). |
| [`post`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn post(url: String, body: Vec<u8>, content_type: String) -> Result<http::Response, errors::Error>` | One-shot POST: `(url, body, content_type)`. Interp tier. |
| [`put`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn put(url: String, body: Vec<u8>, content_type: String) -> Result<http::Response, errors::Error>` | One-shot PUT: `(url, body, content_type)`. Interp tier. |
| [`options`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn options(url: String, headers: Vec<(String, String)>) -> Result<http::Response, errors::Error>` | One-shot OPTIONS: `(url, headers) -> Result<Response, Error>`. |
| [`post`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn post(url: String, body: String, content_type: String) -> Result<http::Response, errors::Error>` | One-shot POST: `(url, body, content_type) -> Result<Response, Error>`. |
| [`Director`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type Director` | Fn(&mut Request) request mutator (Rust-side). |
| [`Proxy`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type Proxy` | Reverse-proxy handler (Rust-side). |
| [`forward`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn forward(url: String, method: String, body: Vec<u8>) -> Result<http::Response, errors::Error>` | One-shot upstream forward: `(url, method, body) -> Result<Response, Error>`. Interp tier. |
| [`put`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn put(url: String, body: String, content_type: String) -> Result<http::Response, errors::Error>` | One-shot PUT: `(url, body, content_type) -> Result<Response, Error>`. |
| [`Query`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type Query` | Parsed query string with typed get / get_all / contains. |
| [`request`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn request(method: String, url: String, body: String, headers: Vec<(String, String)>) -> Result<http::Response, errors::Error>` | One-shot request with a string body: `(method, url, body, headers) -> Result<Response, Error>`. |
| [`request_bytes`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn request_bytes(method: String, url: String, body: Vec<u8>, headers: Vec<(String, String)>) -> Result<http::Response, errors::Error>` | One-shot request with a byte body: `(method, url, body: [u8], headers) -> Result<Response, Error>`. |
| [`Handler`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `trait Handler` | Anything callable as `Fn(Request, Params) -> Response`. |
| [`Params`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type Params` | Captured path parameters. Read inside a handler with `r.path_value(name) -> String`; returns `""` for an undeclared name. All tiers. |
| [`Router`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type Router` | Routing table. Build with `Router::new()`, register routes via the verb methods, then pass to `http::serve`. Verb methods return the router so they chain with `|>`. |
| [`add`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn add(router: http::router::Router, method: String, pattern: String) -> Result<(), errors::Error>` | Register a pattern-only route: `(router, method, pattern)`. Used with `lookup` for low-level dispatch. |
| [`lookup`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn lookup(router: http::router::Router, method: String, path: String) -> Option<http::router::Match>` | Find the index of the first route matching `(method, path)`. Returns `Option<i64>`. |
| [`new`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn new() -> http::router::Router` | Allocate a fresh Router handle. |
| [`serve`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn serve(addr: String, handler: http::Handler) -> Result<(), errors::Error>` | Convenience: bind and serve an HTTP handler. `Result<(), Error>` - a bind failure is an Err value. |
| [`serve_h2c`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn serve_h2c(addr: String, handler: http::Handler) -> Result<(), errors::Error>` | Bind a plain-TCP listener and serve h2c (HTTP/2 cleartext). |
| [`serve_tls`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn serve_tls(addr: String, cert_pem: String, key_pem: String, handler: http::Handler) -> Result<(), errors::Error>` | TLS-terminating server: `serve_tls(addr, cert_pem, key_pem, handler) -> Result<(), Error>`. Builds a rustls config from the PEM cert chain + key and serves HTTPS with the same handler contract as `serve`. |
| [`SerializationMode`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type SerializationMode` | Session payload encoding: Json or Bincode. |
| [`Session`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type Session` | Per-request session view; mutations persist on response. |
| [`SessionConfig`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type SessionConfig` | Cookie name, domain, signing key, serialization mode. |
| [`SessionStore`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `trait SessionStore` | Backend interface: load / save / delete by session id. |
| [`SignedCookieStore`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type SignedCookieStore` | Cookie-backed store with HMAC signature; no server state. |
| [`sign`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn sign(value: String, secret: Vec<u8>) -> String` | Sign session data into a tamper-evident cookie value. |
| [`verify`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn verify(value: String, secret: Vec<u8>) -> Result<String, errors::Error>` | Verify and decode a signed session cookie value. |
| [`with_session`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn with_session(request: http::Request, secret: String) -> Result<http::Request, errors::Error>` | Run a closure with the session bound; persist any mutations. |
| [`Event`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type Event` | One SSE event (id, event, data, retry). |
| [`Stream`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type Stream` | Active SSE stream - handler writes events through it (Rust-side). |
| [`encode_comment`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn encode_comment(comment: String) -> String` | Render a `:`-prefixed keepalive line. Available in interp + compiled. |
| [`encode_event`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn encode_event(event: String, data: String, id: String) -> String` | Render one event block as a string: `(event, data, id) -> String`. Available in interp + compiled. |
| [`encode_retry`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn encode_retry(ms: i64) -> String` | Render a `retry:` reconnect-hint directive in milliseconds. Available in interp + compiled. |
| [`AppState`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type AppState` | TypeMap of T values shared across handlers. |
| [`State`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type State` | Newtype wrapper T for ergonomic handler arguments. |
| [`FileServer`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type FileServer` | Static-file handler rooted at a directory (Rust-side; streaming). |
| [`mime_for_path`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn mime_for_path(path: String) -> String` | Guess a MIME type from a file path's extension. Available in interp + compiled. |
| [`serve_file`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn serve_file(path: String) -> Result<http::Response, errors::Error>` | Read a single file and return it as a Response struct. Interp tier. |
| [`stream`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn stream(method: String, url: String, body: String, headers: Vec<(String, String)>) -> Result<http::ResponseStream, errors::Error>` | One-shot request read incrementally: `(method, url, body, headers) -> Result<ResponseStream, Error>`. |
| [`Error`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type Error` | Io / Protocol / BadHandshake. |
| [`Message`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type Message` | Text / Binary / Ping / Pong / Close. |
| [`WebSocket`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `type WebSocket` | Accepted WebSocket connection (Rust-side framing). |
| [`accept`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn accept(request: http::Request) -> Result<http::Response, errors::Error>` | Validate a WebSocket upgrade request and answer the 101 Switching Protocols response that completes the handshake. |
| [`accept_key`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn accept_key(key: String) -> String` | Compute RFC 6455 Sec-WebSocket-Accept from a client nonce. Available in interp + compiled. |
| [`close`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn close(conn: http::websocket::Conn) -> Result<(), errors::Error>` | close(ws) -> Result<(), Error>: send a close frame and release the handle. |
| [`connect`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn connect(url: String) -> Result<http::websocket::Conn, errors::Error>` | connect(url) -> Result<i64, Error>: client TCP connect + RFC 6455 upgrade; returns a WebSocket handle. |
| [`is_websocket_upgrade`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn is_websocket_upgrade(request: http::Request) -> bool` | Test whether an incoming Request carries a WebSocket upgrade handshake. Interp tier. |
| [`recv`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn recv(conn: http::websocket::Conn) -> Result<http::websocket::Message, errors::Error>` | recv(ws) -> Result<String, Error>: next text message; Err on close/error. |
| [`send_binary`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn send_binary(conn: http::websocket::Conn, data: Vec<u8>) -> Result<(), errors::Error>` | send_binary(ws, data) -> Result<(), Error>: send one binary frame. |
| [`send_text`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn send_text(conn: http::websocket::Conn, text: String) -> Result<(), errors::Error>` | send_text(ws, s) -> Result<(), Error>: send one text frame. |
| [`serve`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/http.rs) | `fn serve(addr: String, handler: Fn(http::websocket::Conn) -> ()) -> Result<(), errors::Error>` | serve(addr, handler) -> Result<(), Error>: bind, upgrade each connection, dispatch the handler's handle(self, ws) per connection. |


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
