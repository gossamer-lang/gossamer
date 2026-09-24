# Concurrency memory model

This page describes how shared memory is ordered when Gossamer
goroutines communicate.

Gossamer has automatic memory management. Heap values stay alive while
reachable, and compiled tiers promote reference counts to atomic
counting when a value crosses a goroutine boundary. That protects object
lifetime. It does not make ordinary reads and writes into a
synchronization protocol.

Ordinary memory operations inside one goroutine run in program order.
Between goroutines, an ordinary read is ordered after an ordinary write
only when a synchronization operation connects them. Without such an
edge, the operations may be observed in either order, and if both touch
the same memory location with at least one write, the program has a data
race. Gossamer keeps the managed object alive, but the racing value
itself has no useful cross-goroutine ordering guarantee.

## What this is like

The concurrency side is Go-shaped: goroutines share memory, and channels
or synchronization primitives establish ordering. Sharing a pointer is
not publication by itself.

The lifetime side is Swift-like: compiled code uses deterministic
reference counting rather than a tracing collector. Unlike Swift ARC,
compiled Gossamer also collects strong cycles, and it can keep
goroutine-local reference counts non-atomic until an object is shared.

This is not Rust's ownership model. `&` and `&mut` are shared and exclusive
lexical views enforced by a conservative borrow checker. Their implicit
lifetimes end at the closing brace. There is no explicit lifetime annotation
language or non-lexical last-use inference.

This is not C or C++ either. A data race is not documented as permission
for the compiler to assume arbitrary facts about the whole program. It
means the racing accesses are unordered by the Gossamer model, so the
value observed there is not a reliable communication result. Use
channels, locks, wait groups, barriers, once initialization, or atomics
when another goroutine must observe a write.

## Happens-before edges

A happens-before edge is the ordering relation used by the model. If A
happens-before B, then effects before A are visible to code after B
according to the synchronization primitive's contract.

Gossamer establishes these edges:

- Starting a goroutine publishes the values captured or passed to that
  goroutine before its body begins.
- A channel send publishes to the receive that obtains that value.
- Closing a channel publishes to receives that observe the closed and
  drained state.
- `select` inherits the send, receive, and close edge of the arm that
  actually runs. A `default` arm does not synchronize with another
  goroutine.
- `Mutex` unlock publishes to a later lock of the same mutex.
- `RwLock` write unlock publishes to later readers and writers that
  acquire the same lock. Concurrent readers are mutually compatible but
  do not publish to each other merely by overlapping.
- `WaitGroup::done` publishes to a `wait` call that observes the count
  reach zero.
- `Barrier::wait` is a rendezvous for its participants. Code after a
  released barrier round runs after every participant reached that same
  round.
- `Once::call` publishes the completed initialization body to every
  caller that returns from the same `Once`.
- Atomic operations follow their documented ordering. The default
  `load`, `store`, `fetch_add`, `fetch_sub`, and `compare_exchange`
  methods are sequentially consistent.

These edges compose transitively. For example, if goroutine A writes a
value, sends on a channel, goroutine B receives that send and then
unlocks a mutex, goroutine C's later lock of the same mutex is ordered
after A's write.

## Channels

`channel()` and `channel(0)` create an unbuffered rendezvous channel. A
send on an unbuffered channel does not complete until a receiver accepts
that exact value, and the send publishes to that receive.

`channel(n)` for `n > 0` creates a bounded buffered channel. Sends block
when the buffer is full. Receives block when the buffer is empty. The
send of a buffered element publishes to the receive that removes that
element from the buffer.

`channel::unbounded()` creates an explicit unbounded queue channel for
code that intentionally wants producer sends to complete without a
receiver or capacity limit. Ordering is still per element: a send
publishes to the receive of that element.

Receiving from a closed and drained channel returns `None`. Closing an
already closed channel is an error.

## Select

When multiple non-default `select` arms are ready, Gossamer polls arms in
pseudo-random order so source order does not deterministically win.
`default` is selected only when no send or receive arm is ready.

Blocking `select` registers interest in all channel arms and wakes when
any of those channels may have become ready. The selected arm determines
the synchronization edge. Arms that were registered but not selected do
not synchronize.

## Atomics

Atomic values provide indivisible operations on their own storage. They
are the right tool for counters, flags, and carefully designed lock-free
handoffs.

Sequentially consistent atomics participate in a single global order for
that atomic API surface. Release stores publish to acquire loads that
observe the released value. AcqRel read-modify-write operations both
acquire from the value they read and release the value they write.

Relaxed atomic operations are still atomic: there is no torn read or
write of the atomic value. They do not order ordinary memory. A relaxed
counter can count events, but it does not publish the data associated
with those events unless some other synchronization does that work.

## Race detector

`gos test --race` and race-enabled compiled code instrument heap
accesses and report conflicting non-atomic accesses that are not ordered
by detector-visible synchronization. The detector uses vector clocks and
records handoff edges for goroutine scheduling, channels, mutexes, wait
groups, once initialization, and detector-aware atomic operations.

The detector is diagnostic tooling, not the definition of the language.
It can miss synchronization it does not currently model, such as some
reader-writer-lock paths, and it does not make an unordered program
ordered. Treat a reported race as a real bug unless there is a precise
reason that the detector lacks the relevant edge.
