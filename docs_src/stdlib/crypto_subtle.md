# `std::crypto::subtle`

Status: experimental

Constant-time comparison helpers.

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## API details and source

The [implementation source](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/crypto.rs) contains the complete declarations and implementation notes. The table below lists canonical Gossamer call signatures; every item name links directly to its implementation file.

| Item | Canonical signature or declaration | Description |
|---|---|---|
| [`constant_time_eq`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/crypto.rs) | `fn constant_time_eq(a: Vec<u8>, b: Vec<u8>) -> bool` | Compares two byte slices without data-dependent branches. |
