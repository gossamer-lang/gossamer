# `std::http::native_client`

Status: experimental

Goroutine-driven HTTP/1.1 client over std::net (no ureq, no blocking pool).

## Items

| Item | Signature | Description |
|---|---|---|
| `Client` | `type Client` | Native h1 client (Rust-side; full builder surface). |
| `Error` | `type Error` | Connect / Tls / Http / Redirect / Timeout / Io. |
| `get` | `fn get(url: String) -> Result<http::Response, errors::Error>` | One-shot GET → Result<Response, Error>. Interp tier (compiled tier shares http::get). |
| `post` | `fn post(url: String, body: Vec<u8>, content_type: String) -> Result<http::Response, errors::Error>` | One-shot POST: `(url, body, content_type)`. Interp tier. |
| `put` | `fn put(url: String, body: Vec<u8>, content_type: String) -> Result<http::Response, errors::Error>` | One-shot PUT: `(url, body, content_type)`. Interp tier. |
| `delete` | `fn delete(url: String) -> Result<http::Response, errors::Error>` | One-shot DELETE → Result<Response, Error>. Interp tier. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## Configured clients - `Client::builder()`

The Gossamer-visible client surface lives on `http::Client`. A client is
configured through a builder chain and then reused for any number of
requests:

```text
let client = http::Client::builder()
    .max_redirects(0)      // 0 = never follow; default 10
    .timeout_ms(5000)      // per-request; non-positive falls back to 30 s
    .build()               // -> Client

let resp = client.request("GET", url, "", headers)?
let up   = client.request_bytes("POST", url, payload_bytes, headers)?
```

- `client.request(method: String, url: String, body: String, headers:
  [(String, String)]) -> Result<Response, Error>` - string body.
- `client.request_bytes(method, url, body: [u8], headers) ->
  Result<Response, Error>` - byte-exact body for uploads.

Method strings are case-insensitive; an unknown method is an immediate
`Err` (`"Client::request: unknown method `BOGUS`"`) with no connection
dialed. The returned `Response` carries `status`, `body`, `raw_bytes`,
`content_type`, `location`, and `headers` (lowercase names, wire order,
duplicates preserved) - see the `std::http` page.

## Redirect policy

- **Default**: up to 10 redirects are followed automatically.
- **`max_redirects(0)`**: never follow - a 3xx response is returned raw,
  with `status`, the `location` field, and all response headers intact.
  This is the proxy-correct mode: a reverse proxy relays the 3xx to its
  own client instead of chasing it.
- **Budget exhausted**: following more redirects than the configured
  budget is a transport error - `Err("http: transport: ... too many
  redirects")`.

## Legacy builder chain

The pending-request chain remains supported and works with both
`Client::new()` and a `Client::builder()...build()` client:

```text
let sent = client.get(url)
    .header("x-tag", "v1")     // appended in order
    .body("payload")
    .send()                    // -> Result<Response, Error>
```

`send()` returns the same `Result<Response, Error>` shape as
`client.request`, and the chained headers and body are honored on the
wire. An unknown method at `send()` time is
`Err("Request::send: unknown method ...")`.

## Streaming reads - `ResponseStream`

`http::stream(method, url, body, headers) -> Result<ResponseStream, Error>`
issues the request and hands back the body as an incremental reader
instead of a buffered response. The `ResponseStream` value carries
`status` and `content_type` fields plus two reading methods:

- `next_line() -> Option<String>` - one line at a time, terminator
  stripped; the natural shape for SSE and line-oriented protocols.
- `next_chunk(max_bytes) -> Option<[u8]>` - up to `max_bytes` raw bytes
  (clamped to 1 MiB); the shape for binary bodies.

Both return `None` at end of body. They read through the same buffered
reader, so interleaving line and chunk reads stays coherent - a parser
can read header lines with `next_line` and switch to `next_chunk` for a
binary payload.

```text
match http::stream("GET", url, "", []) {
    Ok(rs) => {
        while let Some(chunk) = rs.next_chunk(4096) {
            handle(chunk)
        }
    }
    Err(e) => eprintln("{}", e),
}
```

A `ResponseStream` handed to `http::Response::stream(status, content_type, rs)`
is consumed by the response: subsequent `next_line` / `next_chunk` calls
return `None`, and the stream serves exactly one response.
