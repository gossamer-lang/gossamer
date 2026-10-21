# `std::sync`

Status: experimental

Synchronisation primitives beyond channels.

## Items

| Item | Signature | Description |
|---|---|---|
| `Channel` | `type Channel` | Bidirectional channel handle. |
| `Sender` | `type Sender` | The sending end of a channel: `send(v)`, `close()`. |
| `Receiver` | `type Receiver` | The receiving end of a channel: `recv() -> Option<T>` waits for a value, `None` once it is closed and drained; `try_recv() -> Option<T>` answers at once, `None` when nothing is queued. |
| `Mutex` | `type Mutex` | Mutual-exclusion lock: `lock()` / `unlock()` around the code it guards. |
| `RwLock` | `type RwLock` | Reader-writer lock guarding an `i64`: `read`, `write`, `with_read` (calls back with the value read under the shared lock, released first), `with_write` (calls back holding the exclusive lock and stores the answer). |
| `Shared` | `type Shared` | A value several goroutines reach, every read and write taken under one lock. Build with `Shared::new(value)`; read with `get` or `with`, write with `set`, and read-modify-write with `update`, which holds the lock across the callback. |
| `Once` | `type Once` | One-shot initialisation latch: `call(f)` runs `f` the first time and answers whether this call ran it. |
| `WaitGroup` | `type WaitGroup` | Counts goroutines and waits for them to finish: `add(n)`, `done()`, `wait()`, and `wait_ctx(ctx)`, which answers whether the group finished before `ctx` ended. |
| `Barrier` | `type Barrier` | Synchronisation barrier across goroutines: `wait()` returns once every party has arrived. |
| `AtomicI64` | `type AtomicI64` | Atomic 64-bit signed integer. Build with `AtomicI64::new(v)`; `load()`, `store(v)`, `compare_exchange(current, new)` (stores `new` and answers true when the value was `current`), `fetch_add(v)` and `fetch_sub(v)` (answer the value before the change). |
| `AtomicI32` | `type AtomicI32` | Atomic 32-bit signed integer. Build with `AtomicI32::new(v)`; `load()`, `store(v)`, `compare_exchange(current, new)` (stores `new` and answers true when the value was `current`), `fetch_add(v)` and `fetch_sub(v)` (answer the value before the change). |
| `AtomicU64` | `type AtomicU64` | Atomic 64-bit unsigned integer. Build with `AtomicU64::new(v)`; `load()`, `store(v)`, `compare_exchange(current, new)` (stores `new` and answers true when the value was `current`), `fetch_add(v)` and `fetch_sub(v)` (answer the value before the change). |
| `AtomicBool` | `type AtomicBool` | Atomic boolean. Build with `AtomicBool::new(v)`; `load()`, `store(v)`, `compare_exchange(current, new)` (stores `new` and answers true when the value was `current`). |
| `Map` | `type Map` | Concurrent key/value map. |
| `channel` | `fn channel<T>(capacity: i64) -> sync::Channel<T>` | Creates a typed channel, returning (Sender, Receiver). |
| `channel_unbounded` | `fn channel_unbounded<T>() -> sync::Channel<T>` | Creates an explicit unbounded typed channel, returning (Sender, Receiver). |
| `shield` | `fn shield<T>(f: Fn() -> T) -> T` | Runs a callable in a cohort exempt from cancellation and answers its value: a cancel landing on an enclosing cohort does not reach the work inside. The shielded region is still counted and still drained. |
| `with_timeout` | `fn with_timeout<T>(f: Fn() -> T, ms: i64) -> Result<T, errors::Error>` | Runs a callable on a child of a cohort bounded by a millisecond deadline, answering `Ok(value)` when it finished inside the bound and `Err` naming the bound when it did not. Cancellation is cooperative: a child that reaches no cancellation point runs on and is named in the cohort's drain report. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
