# Methods by type

This page is the source-facing reference for inherent methods on core
types such as `String`, `Vec`, `Map`, `Set`, `BTreeSet`, `Option`, and
`Result`.

Items listed here resolve in `gos` (interpreter), forced-JIT execution,
and `gos build [--release]` (compiled) unless a row explicitly says
otherwise. The implementation contract is that interpreter builtins, MIR
lowering, compiled runtime ABI, docs, and parity tests all agree.

Most methods below dispatch by name through the compiler's MIR table at
[`crates/gossamer-mir/src/lower.rs`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-mir/src/lower.rs).

If a method you expect is not listed, the compiler will emit a
`CallIntrinsic{name:"unsupported"}` MIR node and the codegen
will refuse to emit it. Report the call shape; most gaps are small
dispatch-table additions.

## String

| Method | Returns | Notes |
|---|---|---|
| `String::new()` | `String` | Associated function; empty owned string. |
| `String::with_capacity(n)` | `String` | Associated function; reserves mutable builder storage in the VM and native runtime. |
| `String::from<T: Display>(value)` | `String` | Associated function; identity for strings, display conversion for values that implement `Display`. |
| `String::from_utf8(bytes)` | `Result<String, errors::Error>` | Associated function; decodes a byte vector, returning `Err` for invalid UTF-8. |
| `s.len()` | `i64` | Byte length, not codepoint count. Use `utf8::rune_count` for code points. |
| `s.is_empty()` | `bool` | |
| `s.clear()` | `()` | Replaces the string with `""` through mutating-method writeback. |
| `s.truncate(n)` | `()` | Keeps at most `n` bytes, clamped to a valid UTF-8 boundary. |
| `s.push(ch)` / `s.push_char(ch)` | `()` | Appends a Unicode scalar through mutating-method writeback. |
| `s.push_byte(b)` | `()` | Appends the byte as the matching Unicode scalar. |
| `s.push_str(t)` | `()` | Appends string contents through mutating-method writeback. |
| `s.chars()` | `Vec<char>` | Unicode scalar values. |
| `s.trim()` | `String` | ASCII whitespace strip. |
| `s.contains(needle)` | `bool` | Substring search. |
| `s.starts_with(prefix)` | `bool` | |
| `s.ends_with(suffix)` | `bool` | |
| `s.find(needle)` | `Option<i64>` | Byte position of first match. |
| `s.replace(from, to)` | `String` | Replaces every occurrence. |
| `s.split(delim)` | `Vec<String>` | Splits on every delimiter occurrence. |
| `s.to_lowercase()` | `String` | Lowercase; Unicode-aware. (`to_lowercase` is not a method.) |
| `s.to_uppercase()` | `String` | Uppercase; Unicode-aware. |
| `s.clone()` | `String` | |
| `s.as_bytes()` | `Vec<u8>` | Materializes the UTF-8 bytes. This is an intentional divergence from Rust's borrowed `&[u8]` result because the current cross-tier string ABI does not expose a stable borrowed byte view. |
| `s.parse<T>()` / `s.parse::<T>()` | `Result<T, errors::Error>` | Parses into the expected result type, such as `let n: i64 = s.parse()?`. |
| `s.to_i64()` | `Option<i64>` | Parses the string; `None` on malformed input. |
| `s.to_f64()` | `Option<f64>` | |
| `s.to_bool()` | `Option<bool>` | Accepts `true` / `false`. |
| `s.trim_matches(set)` | `String` | Strips any char in `set` from both ends; `trim_start_matches` / `trim_end_matches` do one end. |
| `s.split_once(sep)` | `Option<(String, String)>` | First occurrence; `rsplit_once` takes the last. |

## Vec

`Vec<T>` is the only owned growable sequence. `[T; N]`, `&[T]`, and `&mut [T]`
share only the non-resizing methods listed below. Mutable arrays and slices may
reorder or replace existing elements, but cannot change their length or
capacity. An ordered collection - `Vec`, fixed arrays, slices, `Map`,
`BTreeMap` - traverses the values it holds, so `map` / `filter` / `fold`
answer eagerly on it; `iter()` is how a caller asks for the lazy walk that
never holds the whole sequence, and `collect()` ends one. A `Set` has no
element order, so every traversal on one is written on the iterator
`s.iter()` answers. `collect` belongs
to the iterator, so it is not written on a collection that already holds its
values; `to_vec` is the conversion a borrowed or fixed-length sequence
carries, so a `Vec` has neither.
The literal spelling of each container is in
[Collection literals](collection_literals.md).

| Receiver | Available surface |
|---|---|
| `[T; N]`, `&[T; N]`, `&[T]` | `len`, `is_empty`, `slice`, `first`, `last`, `get`, `contains`, `index_of`, `count_of`, `windows`, `chunks`, `join`, `to_vec`, `iter`, and the parallel adapters `par_map`, `par_filter`, `par_reduce`, `par_sum`, `par_min`, `par_max`; fixed arrays also have value-preserving `clone` |
| `&mut [T; N]`, `&mut [T]` | Shared methods plus in-place `sort`, `sort_by`, `sort_by_key`, `reverse`, `swap`, and `fill` |
| `Vec<T>`, `&Vec<T>`, `&mut Vec<T>` | Shared methods plus resizing and capacity operations |

| Method | Returns | Notes |
|---|---|---|
| `Vec::new()` | `Vec<T>` | Associated function; empty vector. |
| `Vec::with_capacity(n)` | `Vec<T>` | Associated function; empty vector whose `capacity()` is at least `n`. |
| `v.push(item)` | `()` | Amortised O(1). |
| `v.pop()` | `Option<T>` | |
| `v.clear()` | `()` | Removes all elements. |
| `v.truncate(n)` | `()` | Keeps the first `n` elements, clamping negative lengths to `0`. |
| `v.extend(xs)` / `v.extend_from_slice(xs)` | `()` | Appends elements from another vector or inline array with matching element layout. |
| `v.reserve(n)` / `v.reserve_exact(n)` | `()` | Ensures room for at least `n` total elements. |
| `v.capacity()` | `i64` | Returns allocated element capacity. |
| `v.len()` | `i64` | |
| `v.is_empty()` | `bool` | |
| `v.iter()` | `Iterator<T>` | Lazy walk; adapters on it run only as a terminal pulls. |
| `v.filter(pred)` | `Vec<T>` | Elements where `pred` holds. |
| `v.map(f)` | `Vec<U>` | Transform every element. |
| `v.sum()` | `T` | Sum in the element's own type; `0` for empty. |
| `v.min()` / `v.max()` | `Option<T>` | `None` for empty. |
| `v.count()` | `i64` | Element count. |
| `v.any(pred)` / `v.all(pred)` | `bool` | Short-circuiting. |
| `v.find(pred)` | `Option<T>` | First match; `v.position(pred)` returns its index. |
| `v.fold(init, f)` | `U` | Left fold: `f(acc, x)` per element. |
| `v.par_map(f)` / `v.par_filter(pred)` | `Vec<U>` / `Vec<T>` | Parallel twins of `map` / `filter`, elements in input order; the callback is a closure literal or a named pure function. See [Parallel collection adapters](parallel.md). |
| `v.par_reduce(identity, combine)` | `T` | `identity` for an empty input, else every element combined in index order; `combine` must be associative, not commutative. |
| `v.par_sum()` / `v.par_min()` / `v.par_max()` | `T` / `Option<T>` / `Option<T>` | Parallel twins of `sum` / `min` / `max`; a float sum answers the same bits at any worker count. |
| `v.max_by_key(f)` / `v.min_by_key(f)` | `Option<T>` | Extremum by derived key. |
| `v.take(n)` | `Vec<T>` | First `n` elements (fewer if short). |
| `v.step_by(n)` | `Vec<T>` | Every `n`-th element, starting at index 0. |
| `v.join(sep)` | `String` | Elements rendered as `{}` renders them and joined with `sep`; no traversal. |
| `v.first()` / `v.last()` | `Option<T>` | |
| `v.insert(i, item)` / `v.remove(i)` | `Result<_, errors::Error>` | Bounds-checked mutation helpers. |
| `v.rev()` | `Vec<T>` | Non-mutating; `v.reverse()` is in-place. |
| `v.contains(x)` | `bool` | `v.index_of(x)` returns `Option<i64>`, `v.count_of(x)` the tally. |
| `v.sort()` / `v.sort_by(cmp)` / `v.sort_by_key(f)` | `()` | In-place; `Reverse(k)` keys give descending order. |
| `v.swap(i, j)` | `()` | Exchanges two existing elements; an index outside `[0, len)` panics. |
| `v.fill(value)` | `()` | Clones `value` into every existing element without changing length or capacity; also available on mutable arrays and slices. |

## Map

| Method | Returns | Notes |
|---|---|---|
| `Map::from<K, V, const N: usize>([(K, V); N])` | `Map<K, V>` | Associated function; accepts array pairs. Map literals such as `{"one": 1}` construct `Map` values directly. |
| `m.insert(k, v)` | `Option<V>` | Inserts or overwrites in place and returns the previous value when present. |
| `m.get(k)` | `Option<V>` | `None` when the key is absent. |
| `m.get_or(k, default)` | `V` | Value for `k`, or `default` when absent. |
| `m.contains_key(k)` | `bool` | Key-membership test (`m.contains(k)` is an alias). |
| `m.remove(k)` / `m.pop(k)` | `Option<V>` | Deletes the key and returns its previous value when present. |
| `m.inc(k)` / `m.inc(k, by)` | `()` | Increment an `i64` counter, inserting `0` first if absent. |
| `m.or_insert(k, default)` | `V` | Value for `k`, inserting `default` first when absent; works for aggregate values (structs, tuples) too. |
| `m.len()` | `i64` | |
| `m.iter()` | `Iterator<(K, V)>` | Lazy pairs in key order; `m.iter().collect()` materialises them and `m.map(f)` traverses eagerly. A struct, tuple, or array key is rebuilt from the map's own key layout, so a bound key sees the value that was inserted. |
| `m.values()` | `Vec<V>` | Values in key order, for every key type. |
| `m.keys()` | `Vec<K>` | Scalar and `String` keys only. A struct, tuple, or array key is rejected at check time; use `for (key, value) in m.iter()`. |
| `m.is_empty()` / `m.clear()` | `bool` / `()` | Empty test and in-place removal of all entries. |

## BTreeMap

`BTreeMap` and `Map` are distinct types over one representation: a
constructor answers its own, and neither converts to the other. BTreeMap
iterates in key order.

| Method | Returns | Notes |
|---|---|---|
| `BTreeMap::from<K, V, const N: usize>([(K, V); N])` | `BTreeMap<K, V>` | Associated function; accepts array pairs. |
| `m.insert(k, v)` | `Option<V>` | Inserts or overwrites and returns the previous value when present. |
| `m.get(k)` | `Option<V>` | `None` when the key is absent. |
| `m.get_or(k, default)` | `V` | Value for `k`, or `default` when absent. |
| `m.or_insert(k, default)` | `V` | Value for `k`, inserting `default` first when absent. |
| `m.remove(k)` / `m.pop(k)` | `Option<V>` | Deletes the key and returns its previous value when present. |
| `m.contains(k)` / `m.contains_key(k)` | `bool` | Key-membership test. |
| `m.len()` | `i64` | |
| `m.is_empty()` / `m.clear()` | `bool` / `()` | Empty test and in-place removal of all entries. |
| `m.iter()` | `Iterator<(K, V)>` | Yields pairs in ascending key order, for every key and value pairing. |
| `m.keys()` / `m.values()` | `Vec<K>` / `Vec<V>` | Snapshots keys or values in key order. |

## Set

`Set<T>` is unordered: it answers membership, cardinality, and set algebra,
and holds no promise about the order a walk sees its elements in. Sequence
operations are the iterator's, so they are written `s.iter().take(3)`,
`s.iter().count(|v| v > 1)`, `s.iter().map(f).collect()`; `s.take(3)` reports
GT0002 and names that spelling. Sort the materialised sequence, or use
`BTreeSet`, when order is part of the answer. Printing and serialization sort
both kinds, so rendered output is stable whatever order the elements went in.

| Method | Returns | Notes |
|---|---|---|
| `s.insert(v)` | `bool` | Inserts the value and reports whether it was newly added. |
| `s.contains(v)` | `bool` | Membership test. |
| `s.remove(v)` | `bool` | Deletes the value and reports whether it was present. |
| `s.len()` | `i64` | |
| `s.is_empty()` / `s.clear()` | `bool` / `()` | Empty test and in-place removal of all values. |
| `s.iter()` | `Iterator<T>` | The walk every sequence operation starts from. |
| `s.to_vec()` | `Vec<T>` | Materialises the values, in the same unordered walk. |
| `s.union(other)` | `Set<T>` | Set union. |
| `s.intersection(other)` | `Set<T>` | Shared values. |
| `s.difference(other)` | `Set<T>` | Values present only in `s`. |
| `s.symmetric_difference(other)` | `Set<T>` | Values present in exactly one set. |
| `s.is_subset(other)` / `s.is_superset(other)` | `bool` | Inclusion checks. |
| `s.is_disjoint(other)` | `bool` | True when the sets share no values. |

## BTreeSet

`BTreeSet<T>` is the sorted set: the same method surface as `Set<T>`, and its
`iter()` / `to_vec()` read in ascending element order.

## Deque

`Deque<T>` is a double-ended ring buffer; both ends are constant-time. It
holds its elements in the same element store a `Vec<T>` uses, so the element
is any type a `Vec` holds - a scalar, a `String`, a tuple, a struct, an enum,
a fixed array, a nested container, an `Option` - and the pop / peek methods
answer `Option<T>` in that element type. Like Rust's `VecDeque`, `Deque` uses
explicit front/back method names.

| Method | Returns | Notes |
|---|---|---|
| `d.push_back(v)` | `()` | Append to the back. |
| `d.push_front(v)` | `()` | Prepend to the front. |
| `d.pop_back()` | `Option<T>` | Remove and return the back element. |
| `d.pop_front()` | `Option<T>` | Remove and return the front element. |
| `d.peek_back()` | `Option<T>` | Back element without removing it. |
| `d.peek_front()` | `Option<T>` | Front element without removing it. |
| `d.len()` | `i64` | |
| `d.is_empty()` | `bool` | |
| `d.clear()` | `()` | Removes all values. |

## Queue

`Queue<T>` is a restricted FIFO queue over any element a `Vec` holds:
`push` appends to the back, `pop` removes from the front, and `peek` observes
the front without removing it. Build one with `Queue::new()` or `Queue::from([a, b])`.

| Method | Returns | Notes |
|---|---|---|
| `q.push(v)` | `()` | Append to the back. |
| `q.pop()` | `Option<T>` | Remove and return the front element. |
| `q.peek()` | `Option<T>` | Return the front element without removing it. |
| `q.len()` | `i64` | |
| `q.is_empty()` | `bool` | |
| `q.clear()` | `()` | Removes all values. |

## Stack

`Stack<T>` is a restricted LIFO stack over any element a `Vec` holds:
`push` appends to the top, `pop` removes from the top, and `peek` observes
the top without removing it. Build one with `Stack::new()` or `Stack::from([a, b])`.

| Method | Returns | Notes |
|---|---|---|
| `s.push(v)` | `()` | Push onto the top. |
| `s.pop()` | `Option<T>` | Remove and return the top element. |
| `s.peek()` | `Option<T>` | Return the top element without removing it. |
| `s.len()` | `i64` | |
| `s.is_empty()` | `bool` | |
| `s.clear()` | `()` | Removes all values. |

## MaxHeap And MinHeap

`MaxHeap<T>` and `MinHeap<T>` order their elements by the element's own
comparison: `push` inserts, `pop` removes the greatest (or least) element, and
`peek` observes it. The element is any type the language orders - scalars and
`String` by value, tuples and structs field by field, sequences
lexicographically, `Option` and `Result` by arm then payload, and an enum by
variant rank then payload. A `Map` or a `Set` has no element order and is
reported as GT0068, as is a `u64` or `usize`, whose range outruns the signed
comparison the heap orders by.

## `to_string` and the Display rendering

`x.to_string()` answers the text `{}` renders for the same value, through the
same formatter: a scalar, a tuple, a struct, an enum, a `Vec`, a `Map`, a
`Set`, an `Option`, a `Result`, and every nesting of them. A `String` is
already its own text, so it carries `clone` rather than `to_string`.

A handle, a closure, a channel, and a `JoinHandle` have no rendering; both
`to_string` and `{}` on one report GT0062, naming what it is. A lazy iterator
is a cursor rather than a value, so it is collected first (GT0041).

`xs.join(sep)` uses the same rendering for every element, so any sequence whose
element `{}` renders can be joined.

## Container element types

`Deque`, `Queue`, and `Stack` store and hand back, so they hold whatever a
`Vec<T>` holds, and an element is owned, released, and rendered exactly as the
same element in a `Vec` is. A `MaxHeap` and a `MinHeap` also *order* their
elements, so their element must be one the language orders; an unordered one
(a `Map`, a `Set`) is reported as GT0068 - hold it in a `Deque` / `Queue` /
`Stack`, or key the heap by something ordered and look the value up.

## Tuple

A tuple's surface is mostly syntax rather than methods. See
[Tuples](language/tuples.md) for its method table and the reason `iter()` is
rejected.

## Option

| Method | Returns | Notes |
|---|---|---|
| `o.is_some()` / `o.is_none()` | `bool` | Variant checks. |
| `o.unwrap()` | `T` | Returns the payload or panics. |
| `o.expect(message)` | `T` | Returns the payload or panics with `message`. |
| `o.unwrap_or(default)` | `T` | Fallback value for `None`. |
| `o.unwrap_or_else(f)` | `T` | Lazy fallback for `None`. |
| `o.map(f)` | `Option<U>` | Maps the payload when present. |
| `o.and_then(f)` | `Option<U>` | Flat-map over the payload. |
| `o.filter(pred)` | `Option<T>` | Keeps `Some` only when the predicate accepts the payload. |
| `o.or(other)` / `o.or_else(f)` | `Option<T>` | Fallback option. |
| `o.ok_or(err)` / `o.ok_or_else(f)` | `Result<T, E>` | Converts absence into an error. |
| `o.flatten()` | `Option<T>` | Collapses `Option<Option<T>>` one level. |
| `o.zip(other)` | `Option<(T, U)>` | Pairs two present payloads. |

## Result

| Method | Returns | Notes |
|---|---|---|
| `r.is_ok()` / `r.is_err()` | `bool` | Variant checks. |
| `r.unwrap()` | `T` | Returns `Ok` or panics. |
| `r.expect(message)` | `T` | Returns `Ok` or panics with `message`. |
| `r.unwrap_or(default)` | `T` | Fallback value for `Err`. |
| `r.unwrap_or_else(f)` | `T` | Lazy fallback for `Err`. |
| `r.map(f)` | `Result<U, E>` | Maps the `Ok` payload. |
| `r.map_err(f)` | `Result<T, F>` | Maps the `Err` payload. |
| `r.and_then(f)` | `Result<U, E>` | Flat-map over the `Ok` payload. |
| `r.or_else(f)` | `Result<T, F>` | Recovers from `Err`. |
| `r.ok()` / `r.err()` | `Option<_>` | Extracts one side as an option. |
| `r.transpose()` | `Option<Result<T, E>>` | Converts `Result<Option<T>, E>`. |

## Channels

`channel::<T>()` returns `(Sender<T>, Receiver<T>)`. Both halves
share these methods:

| Method | Returns | Notes |
|---|---|---|
| `tx.send(v)` | `()` | Enqueues `v`; a buffered send does not block on a waiting receiver. |
| `rx.recv()` | `Option<T>` | Blocks until a value is available; `Some(v)`, or `None` once the channel is closed and drained. The canonical drain is `while let Some(v) = rx.recv()`. |
| `rx.recv_ctx(ctx)` | `Option<T>` | Blocks like `recv()` but returns `None` when the supplied [`std::context::Context`](stdlib.md#stdcontext) fires. Goroutine callers observe cancellation immediately via the scheduler unpark path; OS-thread callers within 50ms via a bounded condvar timeout. |
| `rx.try_recv()` | `Option<T>` | Non-blocking; `None` if empty. |
| `tx.close()` / `rx.close()` | `()` | Subsequent send/recv return immediately. |

## Streams (`io::stdin` / `io::stdout` / `io::stderr` / file handles)

| Method | Returns | Notes |
|---|---|---|
| `out.write(s)` / `out.write_str(s)` | `()` | UTF-8 string write. |
| `out.write_byte(b)` | `()` | Single byte. |
| `out.write_byte_array(arr, len)` | `()` | Bulk write from `[i64; N]` or `[u8; N]`. |
| `out.flush()` | `()` | Force buffer drain. |
| `io::stdin().read_line(&mut s)` | `Result<i64, errors::Error>` | Appends the raw line to `s` and returns the byte count. The buffer keeps the newline; use `s.trim()` for prompts. |
| `r.read_to_string()` | `String` | Reads until EOF. |

## Concurrency primitives

`sync::Mutex<T>::new()`:

| Method | Returns | Notes |
|---|---|---|
| `m.lock()` | `()` | Blocks until acquired. |
| `m.unlock()` | `()` | |

`sync::WaitGroup::new()`:

| Method | Returns | Notes |
|---|---|---|
| `wg.add(n)` | `()` | Bumps counter by n. |
| `wg.done()` | `()` | Decrements; notifies on zero. |
| `wg.wait()` | `()` | Blocks until counter reaches zero. |

`sync::AtomicI64::new(initial)`:

| Method | Returns | Notes |
|---|---|---|
| `a.load()` | `i64` | Relaxed ordering. |
| `a.store(v)` | `()` | Relaxed ordering. |
| `a.fetch_add(n)` | `i64` | Returns previous value. |

`I64Vec::new(len)` - heap-allocated atomic-i64 buffer for
goroutine fan-out:

| Method | Returns | Notes |
|---|---|---|
| `b.set_at(i, v)` | `()` | Lock-free atomic store. |
| `b.get_at(i)` | `i64` | Lock-free atomic load. |
| `b.vec_len()` | `i64` | |
| `b.write_range_to_stdout(off, count)` | `()` | Bulk byte write. |
| `b.write_lines_to_stdout(off, count, line_len)` | `()` | Inserts `\n` every `line_len`. |

## Module-style functions

Functions accessed through `use std::module` paths (not method
calls) are listed in [`stdlib_coverage.md`](stdlib_coverage.md).

## Compatibility APIs

Some older container helpers still return sentinel values such as `0`,
`-1`, or an empty string for absence or failure. New core APIs should
prefer `Option<T>` or `Result<T, errors::Error>`. Existing sentinel helpers
remain compatibility aliases until their callers have migrated.

## Adding a method to the dispatch table

If you need a method that isn't listed:

1. Add the runtime helper in `crates/gossamer-runtime/src/c_abi.rs`
   as a `#[unsafe(no_mangle)] pub unsafe extern "C" fn` (Rust-side
   declaration; this is internal runtime ABI, not the Gossamer
   source-level FFI surface - see SPEC.md §12 for the latter).
2. Add the dispatch arm in
   `crates/gossamer-mir/src/lower.rs::lower_method_call`.
3. Add the LLVM declaration in
   `crates/gossamer-codegen-llvm/src/emit.rs::RUNTIME_DECLARATIONS`.
4. Add the Cranelift symbol arm in
   `crates/gossamer-codegen-cranelift/src/native.rs`.
5. Register the interpreter builtin in
   `crates/gossamer-interp/src/builtins.rs::install_concurrency_builtins`
   (or the matching install function).
6. Add a small test in `crates/gossamer-codegen-cranelift/tests/`
   that exercises both tiers.

The contract is "every method visible at the language level
resolves at every tier." A method missing from any of (a) the
dispatch table, (b) the LLVM declarations, (c) the interpreter
builtins, is a bug.

## Cross-references

- [`stdlib/index.md`](stdlib/index.md) - module landing page.
- [`stdlib.md`](stdlib.md) - full generated stdlib overview.
- [`stdlib_coverage.md`](stdlib_coverage.md) - auto-generated
  coverage matrix.
- [`codegen_abi.md`](codegen_abi.md) - generic call ABI.
