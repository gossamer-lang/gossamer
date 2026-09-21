# `std::collections::ordered`

`BTreeMap<K, V>` and `BTreeSet<T>` keep their entries in a B+ tree ordered the
way the language orders the key: integers of every width (`u64` / `usize`
unsigned), floats in IEEE total order, `bool`, `char`, `String`, and tuples,
structs, and enums field by field. `get`, `insert`, and `remove` take
O(log n), and every walk (`iter`, `keys`, `values`, `{:?}`) reads the keys in
order without sorting a copy.

The ordered pair adds its ends and its ranges:

| `BTreeMap<K, V>` | `BTreeSet<T>` | Answers |
|---|---|---|
| `first_key_value()` / `last_key_value()` | `first()` / `last()` | `Option` of the first or last entry |
| `pop_first()` / `pop_last()` | `pop_first()` / `pop_last()` | that entry, removed |
| `range(lo..hi)` | `range(lo..hi)` | an iterator over the entries between two keys |

A range is written in the call, since its bounds are keys: `lo..hi`,
`lo..=hi`, `lo..`, `..hi`, `..=hi`, or `..`. A range kept in a binding reports
GT0091. Bounds that are absent or inverted answer the entries between them,
which may be none.

```gos
let mut m: BTreeMap<String, i64> = {"cy": 3, "ana": 9, "bo": 5}
let middle: Vec<(String, i64)> = m.range("b".."d").collect()
println(middle)
println(m.pop_first())
```

A key or element type that writes its own `cmp` is ordered by that body
instead, so the container reads the way the type says it compares:

```gos
struct Ranked { x: i64 }
impl Ord for Ranked {
    fn cmp(&self, other: Ranked) -> i64 { other.x - self.x }
}

let mut best: BTreeMap<Ranked, String> = BTreeMap::new()
best.insert(Ranked { x: 1 }, "one")
best.insert(Ranked { x: 3 }, "three")
println(best.first_key_value())
```

Every tier runs the same tree, so the order and every range answer the same
entries on the bytecode VM, the JIT, and a compiled build.
