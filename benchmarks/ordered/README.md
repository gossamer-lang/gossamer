# Ordered container costs

`btreemap.gos` measures the operations a `BTreeMap` is chosen for - insert,
a hit and a miss, a full traversal, a window of a thousand keys, and a
`pop_first` drain - at a thousand, a hundred thousand, and a million entries,
for `i64` and `String` keys. Every `BTreeMap` row has a `Map` row beside it,
because the hash map answers an ordered traversal by sorting a copy of its
keys on every read, which is the shape the tree replaced.

`std_btreemap.rs` runs the same measurements over Rust's
`std::collections::BTreeMap`, filling its tree in the same scrambled order, so
the two can be read side by side.

```bash
cargo build --release --bin gos
./target/release/gos run benchmarks/ordered/run.gos --gos ./target/release/gos
```

`run.gos` builds both, prints one row per measurement with the ratio between
them, and exits non-zero when a claim stops holding:

- an ordered traversal of the tree is no slower than a traversal of the hash
  map of the same size, at every size and both key types;
- a window of a thousand keys out of a hundred thousand or a million costs no
  more than ten times what the same window costs out of a thousand, so a range
  is paid for by what it reads rather than by what the tree holds;
- insert and a hit stay within ten times Rust's readings (`--skip-rust` drops
  the comparison and the claim, for a machine with no `rustc`).

Two costs have a cause worth naming when reading the rows. A traversal reads
the tree's entries into a sequence before the loop walks them, so it pays a
copy Rust's cursor does not; a `pop_first` builds the one-entry map its answer
is taken out of, which is what makes a drain cost more per entry than the
removal itself.
