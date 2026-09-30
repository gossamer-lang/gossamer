# `std::iter`

Status: experimental

Sequence adapters: map, filter, fold, zip, enumerate, chain, etc. A `Vec` argument is traversed eagerly; an `Iterator` argument keeps the adapter lazy and answers with another iterator.

## Items

| Item | Signature | Description |
|---|---|---|
| `count` | `fn count<T>(items: Vec<T>) -> i64` | Number of elements. |
| `collect` | `fn collect<T>(items: Vec<T>) -> Vec<T>` | Materializes a sequence into a Vec. |
| `once` | `fn once<T>(value: T) -> Vec<T>` | Single-element Vec containing value. |
| `empty` | `fn empty<T>() -> Vec<T>` | Empty Vec. |
| `take` | `fn take<T>(items: Vec<T>, n: i64) -> Vec<T>` | First n elements. |
| `skip` | `fn skip<T>(items: Vec<T>, n: i64) -> Vec<T>` | All elements after the first n. |
| `step_by` | `fn step_by<T>(items: Vec<T>, step: i64) -> Vec<T>` | Every step-th element, starting at index 0. |
| `zip` | `fn zip<A, B>(left: Vec<A>, right: Vec<B>) -> Vec<(A, B)>` | Pairs elements from two sequences. |
| `enumerate` | `fn enumerate<T>(items: Vec<T>) -> Iterator<(i64, T)>` | Pairs each element with its index. |
| `chain` | `fn chain<T>(left: Vec<T>, right: Vec<T>) -> Vec<T>` | Concatenates two sequences. |
| `flatten` | `fn flatten<T>(items: Vec<Vec<T>>) -> Vec<T>` | Flattens a Vec<Vec<T>> into Vec<T>. |
| `rev` | `fn rev<T>(items: Vec<T>) -> Vec<T>` | Returns the elements in reverse order. |
| `dedup` | `fn dedup<T: Eq>(items: Vec<T>) -> Vec<T>` | Removes consecutive duplicate elements. |
| `map` | `fn map<T, U>(items: Vec<T>, f: Fn(T) -> U) -> Vec<U>` | Applies f to each element. Returns a Vec for a Vec argument, and another iterator for an iterator argument. |
| `filter` | `fn filter<T>(items: Vec<T>, predicate: Fn(T) -> bool) -> Vec<T>` | Returns elements where f is true. |
| `fold` | `fn fold<T, U>(items: Vec<T>, init: U, f: Fn(U, T) -> U) -> U` | Reduces a sequence with an accumulator. |
| `flat_map` | `fn flat_map<T, U>(items: Vec<T>, f: Fn(T) -> Vec<U>) -> Vec<U>` | Maps f and flattens one level. |
| `any` | `fn any<T>(items: Vec<T>, predicate: Fn(T) -> bool) -> bool` | True if any element satisfies f. |
| `all` | `fn all<T>(items: Vec<T>, predicate: Fn(T) -> bool) -> bool` | True if every element satisfies f. |
| `sum` | `fn sum<T>(items: Vec<T>) -> T` | Sum of i64 or f64 elements. |
| `product` | `fn product<T>(items: Vec<T>) -> T` | Product of i64 or f64 elements. |
| `min` | `fn min<T: Ord>(items: Vec<T>) -> Option<T>` | Smallest element, or None when empty. |
| `max` | `fn max<T: Ord>(items: Vec<T>) -> Option<T>` | Largest element, or None when empty. |
| `range` | `fn range(start: i64, end: i64) -> Vec<i64>` | Half-open integer sequence [start, end). |
| `range_inclusive` | `fn range_inclusive(start: i64, end: i64) -> Vec<i64>` | Closed integer sequence [start, end]. |
| `repeat` | `fn repeat<T>(value: T, count: i64) -> Vec<T>` | A value repeated n times. |
| `unzip` | `fn unzip<A, B>(items: Vec<(A, B)>) -> (Vec<A>, Vec<B>)` | Splits a sequence of pairs into two Vecs. |
| `windows` | `fn windows<T>(items: Vec<T>, n: i64) -> Vec<Vec<T>>` | Overlapping windows of width n. |
| `pairwise` | `fn pairwise<T>(items: Vec<T>) -> Vec<(T, T)>` | Consecutive overlapping pairs. |
| `chunks` | `fn chunks<T>(items: Vec<T>, n: i64) -> Vec<Vec<T>>` | Non-overlapping chunks of length n. |
| `for_each` | `fn for_each<T>(items: Vec<T>, f: Fn(T) -> ()) -> ()` | Applies f to each element for its side effect. |
| `filter_map` | `fn filter_map<T, U>(items: Vec<T>, f: Fn(T) -> Option<U>) -> Vec<U>` | Maps each element and keeps the Some results. |
| `reduce` | `fn reduce<T>(items: Vec<T>, f: Fn(T, T) -> T) -> Option<T>` | Folds with the first element as the initial accumulator. |
| `scan` | `fn scan<T, S>(items: Vec<T>, init: S, f: Fn(S, T) -> S) -> Vec<S>` | Folds while yielding each intermediate accumulator. |
| `sum_by` | `fn sum_by<T>(items: Vec<T>, f: Fn(T) -> i64) -> i64` | Sum of f(element) over the sequence. |
| `product_by` | `fn product_by<T>(items: Vec<T>, f: Fn(T) -> i64) -> i64` | Product of f(element) over the sequence. |
| `find` | `fn find<T>(items: Vec<T>, predicate: Fn(T) -> bool) -> Option<T>` | First element satisfying f, or None. |
| `position` | `fn position<T>(items: Vec<T>, predicate: Fn(T) -> bool) -> Option<i64>` | Index of the first element satisfying f, or None. |
| `find_map` | `fn find_map<T, U>(items: Vec<T>, f: Fn(T) -> Option<U>) -> Option<U>` | First Some result of f over the sequence. |
| `take_while` | `fn take_while<T>(items: Vec<T>, predicate: Fn(T) -> bool) -> Vec<T>` | Leading run of elements satisfying f. |
| `skip_while` | `fn skip_while<T>(items: Vec<T>, predicate: Fn(T) -> bool) -> Vec<T>` | Elements after the leading run satisfying f. |
| `partition` | `fn partition<T>(items: Vec<T>, predicate: Fn(T) -> bool) -> (Vec<T>, Vec<T>)` | Splits into (matching, non-matching) by f. |
| `sort_by` | `fn sort_by<T>(items: Vec<T>, compare: Fn(T, T) -> i64) -> Vec<T>` | Sorted copy ordered by the comparison closure. |
| `sort_by_key` | `fn sort_by_key<T, K: Ord>(items: Vec<T>, key: Fn(T) -> K) -> Vec<T>` | Sorted copy ordered by a derived key. |
| `min_by` | `fn min_by<T>(items: Vec<T>, compare: Fn(T, T) -> i64) -> Option<T>` | Smallest element by the comparison closure. |
| `max_by` | `fn max_by<T>(items: Vec<T>, compare: Fn(T, T) -> i64) -> Option<T>` | Largest element by the comparison closure. |
| `min_by_key` | `fn min_by_key<T, K: Ord>(items: Vec<T>, key: Fn(T) -> K) -> Option<T>` | Element with the smallest derived key. |
| `max_by_key` | `fn max_by_key<T, K: Ord>(items: Vec<T>, key: Fn(T) -> K) -> Option<T>` | Element with the largest derived key. |
| `chunk_by` | `fn chunk_by<T, K: Eq>(items: Vec<T>, key: Fn(T) -> K) -> Map<K, Vec<T>>` | Groups elements into a map keyed by f. |
| `count_by` | `fn count_by<T, K: Eq>(items: Vec<T>, key: Fn(T) -> K) -> Map<K, i64>` | Counts elements per key derived by f. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## Eager and lazy

These free functions traverse eagerly: a `Vec` argument is walked and the
result materialized. Laziness comes from the argument, not the spelling - an
`Iterator<T>` argument (`xs.iter()`, or a range) keeps `map`, `filter`,
`take`, `skip`, `enumerate`, `chain`, and `zip` lazy and answers with another
iterator, and a terminal consumes that state once. Use `collect` to
materialize it, or traverse a collection through its own methods
(`xs.map(f)`, `xs.sum()`), which always answer eagerly.
See the [lazy iterator protocol](../design/lazy_iterators.md) for ownership,
short-circuiting, overflow, and backend behavior.
