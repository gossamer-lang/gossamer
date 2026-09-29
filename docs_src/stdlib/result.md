# `std::result`

Status: experimental

Result combinators as data-first free functions for pipelines: map, map_err, unwrap_or_else, etc.

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## API details and source

The [implementation source](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/result.rs) contains the complete declarations and implementation notes. The table below lists canonical Gossamer call signatures; every item name links directly to its implementation file.

| Item | Canonical signature or declaration | Description |
|---|---|---|
| [`and_then`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/result.rs) | `fn and_then<T, E, U>(value: Result<T, E>, f: Fn(T) -> Result<U, E>) -> Result<U, E>` | Chains a fallible step on the Ok payload. |
| [`err`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/result.rs) | `fn err<T, E>(value: Result<T, E>) -> Option<E>` | Err payload as an Option. |
| [`is_err`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/result.rs) | `fn is_err<T, E>(value: Result<T, E>) -> bool` | True for Err. |
| [`is_ok`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/result.rs) | `fn is_ok<T, E>(value: Result<T, E>) -> bool` | True for Ok. |
| [`map`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/result.rs) | `fn map<T, E, U>(value: Result<T, E>, f: Fn(T) -> U) -> Result<U, E>` | Transforms the Ok payload, Err passes through. |
| [`map_err`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/result.rs) | `fn map_err<T, E, F>(value: Result<T, E>, f: Fn(E) -> F) -> Result<T, F>` | Transforms the Err payload, Ok passes through. |
| [`ok`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/result.rs) | `fn ok<T, E>(value: Result<T, E>) -> Option<T>` | Ok payload as an Option. |
| [`or_else`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/result.rs) | `fn or_else<T, E, F>(value: Result<T, E>, f: Fn(E) -> Result<T, F>) -> Result<T, F>` | Recovers from Err with a fallback computation. |
| [`unwrap_or`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/result.rs) | `fn unwrap_or<T, E>(value: Result<T, E>, fallback: T) -> T` | Unwraps Ok with a fallback value for Err. |
| [`unwrap_or_else`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/result.rs) | `fn unwrap_or_else<T, E>(value: Result<T, E>, f: Fn(E) -> T) -> T` | Consumes the result, handling Err with a callback. |
