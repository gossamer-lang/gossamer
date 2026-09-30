# `std::collections`

Status: experimental

Built-in container types.

## Items

| Item | Signature | Description |
|---|---|---|
| `Vec` | `type Vec` | Growable contiguous sequence. Vec literals use `[a, b]` and empty Vec literals use `[]`. |
| `Deque` | `type Deque` | Double-ended queue with explicit front/back methods; use `Deque::new()` or `Deque::from([a, b])`. |
| `Queue` | `type Queue` | FIFO-only queue; use `Queue::new()` or `Queue::from([a, b])`. |
| `Stack` | `type Stack` | LIFO-only stack; use `Stack::new()` or `Stack::from([a, b])`. |
| `MaxHeap` | `type MaxHeap` | Max-priority heap; use `MaxHeap::new()` or `MaxHeap::from([a, b])`. |
| `MinHeap` | `type MinHeap` | Min-priority heap; use `MinHeap::new()` or `MinHeap::from([a, b])`. |
| `Map` | `type Map` | Key-value map backed by the swiss-table layout; literals use `{key: value}` and empty map literals use `{}`. |
| `BTreeMap` | `type BTreeMap` | Ordered key-value map backed by BTreeMap. |
| `Set` | `type Set` | Unique-value set backed by a hash table; literals use `#{a, b}` and empty set literals use `#{}`. |
| `BTreeSet` | `type BTreeSet` | Ordered unique-value set backed by BTreeSet. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## `Set<T>` methods

`Set` provides `new`, `insert`, `remove`, `contains`, `len`, `is_empty`,
`clear`, `iter`, `to_vec`, `union`, `intersection`, `difference`,
`symmetric_difference`, `is_subset`, `is_superset`, and `is_disjoint`.
Use `#{a, b, c}` for a `Set` literal.

As in Rust, `map` is an iterator method rather than a `Set` method. Use
`set.iter().map(f)`. Calling `set.map(f)` is a type error.

## `BTreeSet<T>` methods

`BTreeSet` provides the same set method surface as `Set`, but iteration
and `to_vec` return values in sorted order. Use an expected type to shape a
set literal:

```gos
let ordered: BTreeSet<i64> = #{3, 1, 2, 1}
println(ordered.to_vec())
```
