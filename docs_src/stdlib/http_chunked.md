# `std::http::chunked`

Status: experimental

RFC 7230 §4.1 chunked transfer-encoding reader and writer.

## Items

| Item | Signature | Description |
|---|---|---|
| `Reader` | `type Reader` | Decodes a chunked body from any Read source (Rust-side; streaming). |
| `Writer` | `type Writer` | Encodes raw bytes into chunked frames over any Write sink (Rust-side; streaming). |
| `encode` | `fn encode(body: String) -> String` | One-shot: wraps a buffer in chunked transfer-encoding with terminator. Available in interp + compiled. |
| `decode` | `fn decode(body: String) -> String` | One-shot: concatenates data chunks from a complete chunked body. Available in interp + compiled. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
