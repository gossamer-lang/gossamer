# `std::os::signal`

Status: experimental

POSIX-style signal subscription (Go's os/signal shape).

## Items

| Item | Signature | Description |
|---|---|---|
| `Signal` | `type Signal` | Opaque signal name; constructors live in `sigs`. |
| `Notifier` | `type Notifier` | Returned by `on(sig)`; supports wait / try_wait / stop. |
| `on` | `fn on(signum: i64) -> os::signal::Notifier` | Subscribes to a signal; returns a Notifier. |
| `wait` | `fn wait(notifier: os::signal::Notifier) -> bool` | `wait() -> bool`: blocks the calling goroutine until the subscribed signal fires (`true`) or its cohort is cancelled (`false`), without holding a scheduler worker. On wasm32, which delivers no signals, it answers `false` at once. |
| `try_wait` | `fn try_wait(notifier: os::signal::Notifier) -> bool` | Non-blocking poll: returns true if the subscribed signal has fired. |
| `stop` | `fn stop(notifier: os::signal::Notifier) -> ()` | `stop()`: the notifier delivers nothing more, so its waits answer `false`; once no notifier subscribes to the signal it takes its default disposition again. Go's `signal.Stop`. |
| `SIGHUP` | `const SIGHUP` | Hangup: the controlling terminal closed (1). |
| `SIGINT` | `const SIGINT` | Interrupt, Ctrl-C (2). |
| `SIGQUIT` | `const SIGQUIT` | Quit, Ctrl-\ (3); also renders the goroutine dump. |
| `SIGTERM` | `const SIGTERM` | Termination request (15). |
| `SIGUSR1` | `const SIGUSR1` | User signal 1 (10 on Linux, 30 on macOS). |
| `SIGUSR2` | `const SIGUSR2` | User signal 2 (12 on Linux, 31 on macOS). |
| `SIGWINCH` | `const SIGWINCH` | The terminal window changed size (28). Never delivered on Windows. |
| `SIGTSTP` | `const SIGTSTP` | Terminal stop, Ctrl-Z (20 on Linux, 18 on macOS). Subscribing replaces the default stop, so the program stops itself. Never delivered on Windows. |
| `SIGCONT` | `const SIGCONT` | Continue after a stop (18 on Linux, 19 on macOS). Never delivered on Windows. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
