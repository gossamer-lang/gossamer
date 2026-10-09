# `arena { }`

An `arena` block gives a span of code its own bump allocator:
everything allocated while the block runs lands in the arena, and the
whole arena is freed at once when the block exits.

<!-- fragment -->
```gossamer
fn main() {
    let mut total = 0
    let mut i = 0
    while i < 1000 {
        arena {
            let tree = build_tree(16)
            total += check(tree)
        }
        i += 1
    }
    println(f"{total}")
}
```

## Why

For object graphs that die together - a parse tree, a request's
working set, a per-iteration data structure - individual reference
counting does work the program does not need. Inside an arena:

- **allocation is a pointer bump** (compare + add);
- **reclamation is wholesale**: slabs are released in O(slabs), with
  no per-object walk;
- **small-enum nodes are headerless**: an enum with at most 4 variants
  keeps its discriminant in pointer tag bits, so
  `Node(Tree, Tree)` costs exactly 16 bytes;
- **retain/release are no-ops** for arena values (a two-instruction
  range check at the accounting entries).

## Automatic arenas (no annotation needed)

You often do not have to write `arena { }` at all. The compiler runs a
conservative escape analysis over every loop body - `while`, `for`
(`for i in a..b`, `for x in xs`, `for (i, x) in xs.iter().enumerate()`,
`for (k, v) in m.iter()`), and bare `loop` alike - and when it can prove
that everything the body allocates dies at the
iteration boundary, it wraps the body in an arena for you. Idiomatic
build-and-discard code gets the bulk-free path with no source change:

<!-- fragment -->
```gossamer
let mut total = 0
for _ in 0..iterations {
    let tree = build_tree(depth)   // auto-regioned: bump-allocated,
    total += check(tree)          // freed wholesale at the iteration end
}
```

A sequence combinator's closure body runs once per element, so it is
analyzed and regioned on the same terms. The two ways to spell one
iteration perform the same:

<!-- fragment -->
```gossamer
// Same bulk-free as the loop above.
let total = (0..iterations).map(|_| check(build_tree(depth))).sum()
```

A closure qualifies only when the value it hands back cannot point into
the region - a scalar result is fine, a returned tree keeps the ordinary
reference-counted path - and a captured value counts as an outer one, so
handing a captured collection to a call that may keep it disqualifies the
body.

This is **sound by construction**. A region is freed wholesale, so two
things must hold for a body to get one: nothing it builds may still be
reachable once the iteration ends, and nothing it builds may hold a share
of a value from outside the loop, because the bulk free never gives that
share back. The analysis over-approximates both:

- A value from outside the loop may be read, bound, or passed to a
  function that only reads it, but not stored inside anything the body
  builds, mutated through a method, or taken back out of a call that may
  answer a share of it.
- A value the body builds may be passed to any function that cannot let
  it escape - a goroutine, a channel, a static, or a closure - and may be
  mutated and rebuilt inside the iteration, but not assigned to a binding
  that outlives it.
- `break`, `continue`, and `return` leave a regioned body only with a
  scalar value (or none); the region is closed on each such edge.
- A nested loop is fine when it is regioned itself or allocates nothing,
  so its own temporaries are still freed each inner iteration.

Whether a function "only reads" a value, or "cannot let it escape", comes
from a summary of what every function and method does with each of its
parameters, followed through the calls it makes. When the analysis cannot
prove a rule, it does **not** region, and the values keep the ordinary
reference-counted path. So automatic regioning can only make a program
faster; it never changes a result. The trade-off is the reverse of the
manual block: the worst case is a *missed speedup*, not a dangling pointer.

### Seeing the decision

When an allocation-heavy loop runs slower than expected, set
`GOS_ARENA_TRACE=1` at build time. Every loop and closure body prints
whether it was auto-regioned, and if an allocating one was not, why,
naming the call that decided it:

```text
[arena] main.gos:14:5: auto-regioned (iteration heap bulk-freed)
[arena] main.gos:31:9: NOT regioned - allocates each iteration on the slow
  per-node RC path: body mutates a value from outside the loop through a
  method, or calls a method nothing vets (`.push()`). Wrap the body in `arena { }`.
[arena] main.gos:40:18: closure body auto-regioned (per-call heap bulk-freed)
```

The reason names the exact rule that disqualified the body (an outer
value mutated or handed to a function that keeps it, a heap value leaving
the loop, a nested loop without a region of its own, an unvetted callee),
so you know whether to restructure the loop or reach for an explicit
`arena { }` - which always works, because you are then making the
no-escape guarantee yourself. Setting the variable makes the build skip its
artifact cache, so the trace always describes the compile that just ran.

## Exit behavior

The block desugars to `runtime::arena_push()` plus a block-scoped
`defer runtime::arena_pop()`, so the arena is released on **every**
exit path: normal fall-through, early `return`, `?` propagation, and
`break`/`continue` out of the block.

Arenas nest: an inner `arena { }` frees at its own close brace without
touching the outer one. Slabs from finished arenas are recycled, so an
arena per loop iteration is a bump-pointer reset, not a fresh `mmap`.

## The contract

Nothing allocated inside the block may be referenced after it exits.
The block is statement-position only and yields unit (a tail
expression is discarded), which rules out the obvious escape.

The remaining escapes are checked for you. A conservative front-end
analysis rejects, with `error[GM0003]`, any value allocated in the
block that is assigned to a binding outside it, pushed into a
container that outlives it, sent down a channel, returned, broken out
of an enclosing loop, captured in a goroutine/closure that outruns
the block, or passed into a function that might stash it. Reading an
arena value through a method or a region-safe free function stays
allowed, so build-and-discard code is unaffected. The check is sound
by over-approximation: it may ask you to restructure a sound program,
but it never lets an escaping one compile. Run `gos explain GM0003`
for the details.

Compute summaries inside, keep survivors outside:

<!-- fragment -->
```gossamer
let mut best = 0
arena {
    let g = build_graph(n)
    best = score(g)      // scalar out: fine
}
// `g` is gone; `best` survives.
```

Edge cases, pinned: `Weak` references to arena values upgrade to
`None`; unit-variant singletons (`Tree::Nil`) are process-immortal and
safe to reference anywhere.

## The primitive

`runtime::arena_push()` / `runtime::arena_pop()` are the underlying
calls for shapes where block structure does not fit. Prefer the block:
it cannot be left unbalanced, and it carries the `GM0003` escape check -
the raw primitive is the unchecked low-level escape hatch, so the
no-escape guarantee is yours to uphold when you reach for it.
