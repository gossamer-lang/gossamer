# `std::option`

Status: experimental

Option combinators as data-first free functions for pipelines: map, filter, unwrap_or, and_then, etc.

## Items

| Item | Signature | Description |
|---|---|---|
| `and_then` | `fn and_then<T, U>(value: Option<T>, f: Fn(T) -> Option<U>) -> Option<U>` | Chains a fallible step: Some(v) -> f(v), None stays None. |
| `unwrap` | `fn unwrap<T>(value: Option<T>) -> T` | Returns the payload, panicking when the value is None. |
| `expect` | `fn expect<T>(value: Option<T>, message: String) -> T` | Returns the payload, panicking with the message when the value is None. |
| `unwrap_or` | `fn unwrap_or<T>(value: Option<T>, fallback: T) -> T` | Unwraps with a fallback value for None. |
| `unwrap_or_else` | `fn unwrap_or_else<T>(value: Option<T>, fallback: Fn() -> T) -> T` | Unwraps with a lazily computed fallback for None. |
| `filter` | `fn filter<T>(value: Option<T>, predicate: Fn(T) -> bool) -> Option<T>` | Keeps Some(v) only when the predicate holds. |
| `flatten` | `fn flatten<T>(value: Option<Option<T>>) -> Option<T>` | Collapses Option<Option<T>> one level. |
| `is_none` | `fn is_none<T>(value: Option<T>) -> bool` | True for None. |
| `is_some` | `fn is_some<T>(value: Option<T>) -> bool` | True for Some. |
| `ok_or` | `fn ok_or<T, E>(value: Option<T>, err: E) -> Result<T, E>` | Converts Some to Ok, or None to Err with the provided error. |
| `ok_or_else` | `fn ok_or_else<T, E>(value: Option<T>, err: Fn() -> E) -> Result<T, E>` | Converts Some to Ok, or None to Err from a fallback closure. |
| `iter` | `fn iter<T>(value: Option<T>) -> Vec<T>` | Zero-or-one element sequence view. |
| `map` | `fn map<T, U>(value: Option<T>, f: Fn(T) -> U) -> Option<U>` | Transforms the Some payload, None stays None. |
| `or` | `fn or<T>(value: Option<T>, fallback: Option<T>) -> Option<T>` | First Some of self and the alternative. |
| `or_else` | `fn or_else<T>(value: Option<T>, fallback: Fn() -> Option<T>) -> Option<T>` | First Some of self and a lazily built alternative. |
| `zip` | `fn zip<T, U>(value: Option<T>, other: Option<U>) -> Option<(T, U)>` | Pairs two Somes into Some((a, b)). |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
