# `std::errors`

Status: experimental

Error construction, wrapping, and chain traversal.

## Items

| Item | Signature | Description |
|---|---|---|
| `Error` | `type Error` | Error value carrying a message, an optional cause, and structured diagnostic fields. Methods: `message() -> String` (top message only), `cause() -> Option<Error>`, `chain() -> Vec<Error>` (self then every ancestor cause), `is(needle) -> bool`, `with_field(key, value) -> Error` (an immutable copy carrying one more field; re-setting a key replaces its value), `field(key) -> Option<String>`, `fields() -> Vec<(String, String)>` in insertion order. `{}` renders the colon-joined chain. Example: `let e = errors::new("query failed").with_field("sqlstate", "23505")`. |
| `new` | `fn new(message: String) -> errors::Error` | Constructs a fresh error from a message. |
| `newf` | `fn newf(format: String, args: Vec<String>) -> errors::Error` | Constructs a fresh error from a format template, e.g. `newf("status {}", code)`. |
| `wrap` | `fn wrap(error: errors::Error, context: String) -> errors::Error` | Wraps a cause with a higher-level message. |
| `is` | `fn is(error: errors::Error, needle: T) -> bool` | `is(error, needle) -> bool` - true when `needle` matches `error` or any link of its cause chain. Prefer a sentinel error VALUE, which matches by identity so two errors sharing a message stay distinct: `let NOT_FOUND = errors::new("not found")`, then `errors::is(err, NOT_FOUND)`. A String `needle` falls back to a message substring test. |
| `join` | `fn join(errors: Vec<errors::Error>) -> Option<errors::Error>` | Joins a list of errors into one; messages are joined with "; " (None for an empty list). |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## Rendering

Displaying an error renders the full cause chain, colon-joined from the
outermost wrap down to the root cause:

```text
let root = errors::new("no such file")
let mid  = errors::wrap(root, "open /etc/app.toml")
let top  = errors::wrap(mid, "reading config")
println("{}", top)
// reading config: open /etc/app.toml: no such file
```

- `err.message()` returns only the top message (`"reading config"`),
  not the chain - pair it with `err.cause()` to walk levels manually.
- `errors::join([a, b]) -> Option<Error>` combines several errors into
  one whose message is the individual messages joined with `"; "`
  (`"a; b"`); an empty list joins to `None`.
- `errors::is(err, needle)` walks the same cause chain that Display
  renders; step through it yourself with `err.cause()`.
