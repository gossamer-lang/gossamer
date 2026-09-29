# `std::option`

Status: experimental

Option combinators as data-first free functions for pipelines: map, filter, unwrap_or, and_then, etc.

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## API details and source

The [implementation source](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/option.rs) contains the complete declarations and implementation notes. The table below lists canonical Gossamer call signatures; every item name links directly to its implementation file.

| Item | Canonical signature or declaration | Description |
|---|---|---|
| [`and_then`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/option.rs) | `fn and_then<T, U>(value: Option<T>, f: Fn(T) -> Option<U>) -> Option<U>` | Chains a fallible step: Some(v) -> f(v), None stays None. |
| [`filter`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/option.rs) | `fn filter<T>(value: Option<T>, predicate: Fn(T) -> bool) -> Option<T>` | Keeps Some(v) only when the predicate holds. |
| [`flatten`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/option.rs) | `fn flatten<T>(value: Option<Option<T>>) -> Option<T>` | Collapses Option<Option<T>> one level. |
| [`is_none`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/option.rs) | `fn is_none<T>(value: Option<T>) -> bool` | True for None. |
| [`is_some`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/option.rs) | `fn is_some<T>(value: Option<T>) -> bool` | True for Some. |
| [`iter`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/option.rs) | `fn iter<T>(value: Option<T>) -> Vec<T>` | Zero-or-one element sequence view. |
| [`map`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/option.rs) | `fn map<T, U>(value: Option<T>, f: Fn(T) -> U) -> Option<U>` | Transforms the Some payload, None stays None. |
| [`or`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/option.rs) | `fn or<T>(value: Option<T>, fallback: Option<T>) -> Option<T>` | First Some of self and the alternative. |
| [`or_else`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/option.rs) | `fn or_else<T>(value: Option<T>, fallback: Fn() -> Option<T>) -> Option<T>` | First Some of self and a lazily built alternative. |
| [`unwrap_or`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/option.rs) | `fn unwrap_or<T>(value: Option<T>, fallback: T) -> T` | Unwraps with a fallback value for None. |
| [`unwrap_or_else`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/option.rs) | `fn unwrap_or_else<T>(value: Option<T>, fallback: Fn() -> T) -> T` | Unwraps with a lazily computed fallback for None. |
| [`zip`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/option.rs) | `fn zip<T, U>(value: Option<T>, other: Option<U>) -> Option<(T, U)>` | Pairs two Somes into Some((a, b)). |
