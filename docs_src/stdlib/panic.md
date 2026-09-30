# `std::panic`

Status: experimental

Goroutine-scoped panics; there is no `catch_unwind`, and `runtime::set_panic_hook` observes one.

## Items

| Item | Signature | Description |
|---|---|---|
| `panic` | `builtin panic` | Aborts the current goroutine with a message. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
