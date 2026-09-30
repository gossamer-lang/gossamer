# `std::result`

Status: experimental

Result combinators as data-first free functions for pipelines: map, map_err, unwrap_or_else, etc.

## Items

| Item | Signature | Description |
|---|---|---|
| `and_then` | `fn and_then<T, E, U>(value: Result<T, E>, f: Fn(T) -> Result<U, E>) -> Result<U, E>` | Chains a fallible step on the Ok payload. |
| `unwrap_or` | `fn unwrap_or<T, E>(value: Result<T, E>, fallback: T) -> T` | Unwraps Ok with a fallback value for Err. |
| `unwrap_or_else` | `fn unwrap_or_else<T, E>(value: Result<T, E>, f: Fn(E) -> T) -> T` | Consumes the result, handling Err with a callback. |
| `err` | `fn err<T, E>(value: Result<T, E>) -> Option<E>` | Err payload as an Option. |
| `is_err` | `fn is_err<T, E>(value: Result<T, E>) -> bool` | True for Err. |
| `is_ok` | `fn is_ok<T, E>(value: Result<T, E>) -> bool` | True for Ok. |
| `map` | `fn map<T, E, U>(value: Result<T, E>, f: Fn(T) -> U) -> Result<U, E>` | Transforms the Ok payload, Err passes through. |
| `map_err` | `fn map_err<T, E, F>(value: Result<T, E>, f: Fn(E) -> F) -> Result<T, F>` | Transforms the Err payload, Ok passes through. |
| `ok` | `fn ok<T, E>(value: Result<T, E>) -> Option<T>` | Ok payload as an Option. |
| `or_else` | `fn or_else<T, E, F>(value: Result<T, E>, f: Fn(E) -> Result<T, F>) -> Result<T, F>` | Recovers from Err with a fallback computation. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
