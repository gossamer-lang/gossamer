# `std::http::sse`

Status: experimental

Server-Sent Events (text/event-stream) emitter with heartbeat ticks and retry hint.

## Items

| Item | Signature | Description |
|---|---|---|
| `Stream` | `type Stream` | Active SSE stream - handler writes events through it (Rust-side). |
| `Event` | `type Event` | One SSE event (id, event, data, retry). |
| `encode_event` | `fn encode_event(event: String, data: String, id: String) -> String` | Render one event block as a string: `(event, data, id) -> String`. Available in interp + compiled. |
| `encode_comment` | `fn encode_comment(comment: String) -> String` | Render a `:`-prefixed keepalive line. Available in interp + compiled. |
| `encode_retry` | `fn encode_retry(ms: i64) -> String` | Render a `retry:` reconnect-hint directive in milliseconds. Available in interp + compiled. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
