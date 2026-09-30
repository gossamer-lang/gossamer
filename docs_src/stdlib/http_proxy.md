# `std::http::proxy`

Status: experimental

Reverse proxy on top of http::Client. Director-style request mutator + hop-by-hop strip + error handler.

## Items

| Item | Signature | Description |
|---|---|---|
| `Proxy` | `type Proxy` | Reverse-proxy handler (Rust-side). |
| `Director` | `type Director` | Fn(&mut Request) request mutator (Rust-side). |
| `forward` | `fn forward(url: String, method: String, body: Vec<u8>) -> Result<http::Response, errors::Error>` | One-shot upstream forward: `(url, method, body) -> Result<Response, Error>`. Interp tier. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## Writing a passthrough proxy in Gossamer

The whole proxy shape is expressible directly on the `std::http`
surface: forward the inbound `raw_body` and headers upstream, relay the
upstream response back. Two modes cover the spectrum - buffered (whole
response in memory, simplest) and streamed (chunked passthrough, no
buffering).

Hop-by-hop and engine-managed headers must not be relayed; strip them
and let the client/server engines manage their own:

```text
// Drop hop-by-hop / engine-managed names; keep everything else.
fn forwardable_headers(headers: [(String, String)]) -> [(String, String)] {
    let mut out: [(String, String)] = []
    for (name, value) in headers {
        let n = name.to_lowercase()
        let restricted = n == "connection" || n == "content-length" ||
            n == "host" || n == "upgrade" || n == "accept-encoding"
        if !restricted {
            out.push((name, value))
        }
    }
    out
}
```

### Buffered forward

Byte-exact in both directions: the inbound body rides `request_bytes`
as `[u8]`, the upstream body comes back on `raw_bytes`, and a
struct-literal `Response` relays status, bytes, and content type:

```text
fn forward_buffered(method: String, target: String, body: [u8],
                    headers: [(String, String)]) -> http::Response {
    let client = http::Client::builder().max_redirects(0).build()
    match client.request_bytes(method, target, body, headers) {
        Ok(up) => http::Response {
            status: up.status,
            body: up.raw_bytes,
            content_type: up.content_type,
            headers: [],
        },
        Err(e) => http::Response::text(502, format("upstream error: {}", e)),
    }
}
```

`max_redirects(0)` keeps the proxy transparent: a 3xx from upstream is
relayed to the caller (with its `location` header intact) instead of
being chased by the proxy.

### Streamed forward

`http::stream` + `Response::stream` connect the upstream body straight
to the downstream socket as chunked frames - no buffering, first bytes
flow immediately:

```text
fn forward_stream(method: String, target: String, body: String,
                  headers: [(String, String)]) -> http::Response {
    match http::stream(method, target, body, headers) {
        Ok(up) => http::Response::stream(up.status, up.content_type, up),
        Err(e) => http::Response::text(502, format("upstream error: {}", e)),
    }
}
```

In stream mode, ask the upstream for an identity body
(`("Accept-Encoding", "identity")`) - the proxy relays bytes it does not
decode.

### The handler

```text
struct App { target_base: String }

impl http::Handler for App {
    fn serve(&self, r: http::Request) -> http::Response {
        let suffix = if r.query.len() == 0 { "" } else { "?" + &r.query }
        let target = format("{}{}{}", self.target_base, r.path, suffix)
        let headers = forwardable_headers(r.headers)
        forward_buffered(r.method, target, r.raw_body, headers)
    }
}

fn main() {
    if let Err(e) = http::serve("127.0.0.1:8080", App { target_base: "http://localhost:3000" }) {
        eprintln("{}", e)
    }
}
```

A complete worked proxy - path-prefix routing, gzip/brotli decode in
buffered mode, streaming flag, CLI parsing - ships as the `locurlfwd`
port; the same pattern at production size.
