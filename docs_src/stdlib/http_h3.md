# `std::http_h3`

Status: experimental

HTTP/3 over QUIC. std::http_h3 is the retained 0.27 spelling; no std::http::h3 alias.

## Items

| Item | Signature | Description |
|---|---|---|
| `Handler` | `trait Handler` | Per-request handler. `fn serve(&self, request: Request) -> Response`. |
| `H3Error` | `type H3Error` | Transport / protocol error variants surfaced from quinn + h3. |
| `serve` | `fn serve(addr: String, cert_path: String, key_path: String, handler: http_h3::Handler) -> Result<(), errors::Error>` | Run an HTTP/3 server bound to `addr` with TLS certificate + key paths and the supplied handler. |
| `Client` | `type Client` | HTTP/3 client. `new` validates against the Mozilla root store; `insecure` skips verification (dev only). Methods: `get`, `post`, `put`, `delete`, `head`, `request`. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
