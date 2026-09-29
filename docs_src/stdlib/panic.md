# `std::panic`

Status: unproven

Goroutine-scoped panics; there is no `catch_unwind`, and `runtime::set_panic_hook` observes one.

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## API details and source

The [implementation source](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/panic.rs) contains the complete declarations and implementation notes. The table below lists canonical Gossamer call signatures; every item name links directly to its implementation file.

| Item | Canonical signature or declaration | Description |
|---|---|---|
| [`panic`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/panic.rs) | `builtin panic(...)` | Aborts the current goroutine with a message. |
