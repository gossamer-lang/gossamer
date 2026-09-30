# `std::thread`

OS-thread scheduling hints and CPU introspection; user concurrency uses goroutines, not thread spawning.

## Items

| Item | Signature | Description |
|---|---|---|
| `yield_now` | `fn yield_now() -> ()` | Hints to the scheduler to switch to another runnable thread. |
| `num_cpus` | `fn num_cpus() -> i64` | Returns the number of logical CPUs available. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
