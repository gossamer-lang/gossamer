# `std::lifecycle`

Status: experimental

Process readiness and graceful shutdown, with systemd sd_notify. Shutdown is observed, not dispatched: wait for it, then drain with ordinary statements - `spawn(|| serve())`, `lifecycle::ready()`, `lifecycle::await_shutdown()`, then the cleanup.

## Items

| Item | Signature | Description |
|---|---|---|
| `ready` | `fn ready() -> ()` | `ready()` - declare the process ready to serve traffic; emits `sd_notify(READY=1)` under a service manager. |
| `set_ready` | `fn set_ready(ready: bool) -> ()` | `set_ready(ready: bool)` - set readiness explicitly. Readiness also drops on its own when shutdown begins. |
| `is_ready` | `fn is_ready() -> bool` | `is_ready() -> bool` - whether the process is ready to serve. False before `ready()` and once shutdown has begun, so a readiness probe fails ahead of the drain. |
| `shutdown` | `fn shutdown() -> ()` | `shutdown()` - begin the graceful shutdown sequence: readiness drops and every server stops accepting while in-flight requests finish. |
| `is_shutting_down` | `fn is_shutting_down() -> bool` | `is_shutting_down() -> bool` - whether shutdown has begun. A long-running worker polls it to leave on its own terms. |
| `await_shutdown` | `fn await_shutdown() -> ()` | `await_shutdown()` - block until shutdown begins, whether from SIGTERM, SIGINT, or `shutdown()`. The statements after it are the drain sequence. |
| `notify_status` | `fn notify_status(message: String) -> ()` | `notify_status(message: String)` - report free-text status to the service manager (`sd_notify(STATUS=...)`). |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
