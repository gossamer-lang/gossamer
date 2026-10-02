# `std::collections::parallel`

The parallel twins of the eager sequence walks: `par_map`, `par_filter`,
`par_reduce`, `par_sum`, `par_min`, and `par_max`, on a `Vec`, a fixed
array, a slice, or an integer range. Each answers what its sequential twin
answers, on every tier; the callback is a closure literal or a named function
the compiler proves pure, and a reduction's answer does not depend on the
worker count. `par_chunks_mut(size, f)` writes a sequence in place, handing
each disjoint chunk to `f` with its index.

The full reference is [Parallel collection adapters](../parallel.md).
