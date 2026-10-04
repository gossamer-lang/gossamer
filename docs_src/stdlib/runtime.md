# `std::runtime`

Status: experimental

Goroutine / scheduler introspection and tuning.

## Items

| Item | Signature | Description |
|---|---|---|
| `collect_cycles` | `fn collect_cycles() -> ()` | Requests collection of unreachable reference cycles; returns `()`. |
| `cycle_collection_supported` | `fn cycle_collection_supported() -> bool` | Reports whether this execution tier reclaims unreachable reference cycles. |
| `scheduler_stats_json` | `fn scheduler_stats_json() -> String` | Returns a compact JSON snapshot of goroutine scheduler counters. |
| `arena_push` | `fn arena_push() -> ()` | Opens an arena region for bump allocation. |
| `arena_pop` | `fn arena_pop() -> ()` | Closes the innermost arena region, freeing its slabs. |
| `set_panic_hook` | `fn set_panic_hook(hook: Fn(String) -> ()) -> ()` | Installs a hook invoked with the message on panic. |
| `at_exit` | `fn at_exit(hook: Fn() -> ()) -> ()` | Runs a closure when the program ends: returning from `main`, `process::exit`, or an uncaught panic. Hooks run last registered first. |
| `cohort_push` | `fn cohort_push(policy: i64, timeout_ms: i64, isolation: i64, on_error: i64, uncancellable: i64, drain_ms: i64) -> ()` | Opens a cohort on the running goroutine. Written `cohort { }` in source; the block desugars to this plus `cohort_join` and a deferred `cohort_pop`. |
| `cohort_join` | `fn cohort_join() -> Result<(), errors::Error>` | Waits for every child of the running goroutine's cohort and answers `Result<(), errors::Error>`. |
| `cohort_pop` | `fn cohort_pop() -> ()` | Closes the running goroutine's cohort, cancelling and joining anything still running. |
| `cohorts` | `fn cohorts() -> Vec<String>` | One descriptor line per live cohort, oldest id first: id, parent, completion policy, error disposition, outstanding count, and the spawn indices still running. A cohort is enumerable so a program can say what it is waiting on without joining it. |
| `root` | `fn root() -> String` | The root cohort's descriptor line, in the shape `cohorts()` answers, or an empty String when no cohort is open. The root is the one cohort every program has - `main` runs inside it - and the one whose drain bounds process exit. |
| `cohort_cancelled` | `fn cohort_cancelled() -> bool` | Whether the running goroutine's cohort has been cancelled. A CPU-bound child polls this to cooperate at a point of its own choosing. |
| `cohort_cancel` | `fn cohort_cancel() -> ()` | Cancels the running goroutine's cohort, winding its siblings down without failing it. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## Cycle collection

`collect_cycles()` is experimental. The compiled runtime collects
thread-local RC graphs; values that have crossed a goroutine boundary are
excluded because concurrent mutation cannot safely participate in its
thread-local trial-deletion pass. Break such cycles with `Weak<T>`.

The bytecode VM uses `Arc`-backed values and currently treats this call as a
no-op. `Weak<T>::upgrade()` remains valid for the supported VM heap values,
but collection-driven weak invalidation is not a cross-tier guarantee yet.
