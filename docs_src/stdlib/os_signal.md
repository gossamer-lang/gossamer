# `std::os::signal`

Status: experimental

POSIX-style signal subscription (Go's os/signal shape).

## Items

| Item | Signature | Description |
|---|---|---|
| `Signal` | `type Signal` | Opaque signal name; constructors live in `sigs`. |
| `Notifier` | `type Notifier` | Returned by `on(sig)`; supports wait / try_wait. |
| `on` | `fn on(signum: i64) -> os::signal::Notifier` | Subscribes to a signal; returns a Notifier. |
| `wait` | `fn wait(notifier: os::signal::Notifier) -> ()` | Blocks the calling goroutine until the subscribed signal fires. |
| `try_wait` | `fn try_wait(notifier: os::signal::Notifier) -> bool` | Non-blocking poll: returns true if the subscribed signal has fired. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
