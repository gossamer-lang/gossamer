# `std::sort`

Status: experimental

Explicit stable ordering and sorted-sequence search, the deliberate counterpart to Vec's unstable inherent `sort`.

## Items

| Item | Signature | Description |
|---|---|---|
| `sort_stable` | `fn sort_stable(xs: Vec<T>) -> Vec<T>` | `sort_stable(xs: Vec<T>) -> Vec<T>` - a fresh ascending sequence in which equal elements keep their input order. `xs.sort()` sorts in place and is unstable; this is the spelling that guarantees stability. Example: `let ordered = sort::sort_stable(#[3, 1, 2])`. |
| `binary_search` | `fn binary_search(xs: Vec<T>, target: T) -> Option<i64>` | `binary_search(xs: Vec<T>, target: T) -> Option<i64>` - index of a matching element in an already-ascending sequence, `None` when absent. Example: `sort::binary_search(#[1, 3, 5], 3)` is `Some(1)`. |
| `partition_point` | `fn partition_point(xs: Vec<T>, pivot: T) -> i64` | `partition_point(xs: Vec<T>, pivot: T) -> i64` - the count of elements strictly less than `pivot` in an already-ascending sequence, which is also the insertion index that keeps it sorted. Example: `sort::partition_point(#[1, 2, 6, 8], 5)` is `2`. |
