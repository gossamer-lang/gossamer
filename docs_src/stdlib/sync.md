# `std::sync`

Status: experimental

Synchronisation primitives beyond channels.

## Items

| Item | Signature | Description |
|---|---|---|
| `Channel` | `type Channel` | Bidirectional channel handle. |
| `Sender` | `type Sender` | The sending end of a channel: `send(v)`, `close()`. |
| `Receiver` | `type Receiver` | The receiving end of a channel: `recv() -> Option<T>`, `None` once it is closed and drained. |
| `Mutex` | `type Mutex` | Mutual-exclusion lock: `lock()` / `unlock()` around the code it guards. |
| `RwLock` | `type RwLock` | Reader-writer lock guarding an `i64`: `read`, `write`, `with_read` (calls back with the value read under the shared lock, released first), `with_write` (calls back holding the exclusive lock and stores the answer). |
| `Shared` | `type Shared` | A value several goroutines reach, every read and write taken under one lock. Build with `Shared::new(value)`; read with `get` or `with`, write with `set`, and read-modify-write with `update`, which holds the lock across the callback. |
| `Once` | `type Once` | One-shot initialisation latch. |
| `WaitGroup` | `type WaitGroup` | Counts goroutines and waits for them to finish. |
| `Barrier` | `type Barrier` | Synchronisation barrier across goroutines. |
| `AtomicI64` | `type AtomicI64` | Atomic 64-bit signed integer. |
| `AtomicI32` | `type AtomicI32` | Atomic 32-bit signed integer. |
| `AtomicU64` | `type AtomicU64` | Atomic 64-bit unsigned integer. |
| `AtomicBool` | `type AtomicBool` | Atomic boolean. |
| `Map` | `type Map` | Concurrent key/value map. |
| `channel` | `fn channel<T>(capacity: i64) -> sync::Channel<T>` | Creates a typed channel, returning (Sender, Receiver). |
| `channel_unbounded` | `fn channel_unbounded<T>() -> sync::Channel<T>` | Creates an explicit unbounded typed channel, returning (Sender, Receiver). |
| `shield` | `fn shield<T>(f: Fn() -> T) -> T` | Runs a callable in a cohort exempt from cancellation and answers its value: a cancel landing on an enclosing cohort does not reach the work inside. The shielded region is still counted and still drained. |
| `with_timeout` | `fn with_timeout<T>(f: Fn() -> T, ms: i64) -> Result<T, errors::Error>` | Runs a callable on a child of a cohort bounded by a millisecond deadline, answering `Ok(value)` when it finished inside the bound and `Err` naming the bound when it did not. Cancellation is cooperative: a child that reaches no cancellation point runs on and is named in the cohort's drain report. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
