# Parallel collection adapters

Every eager walk over a collection has a parallel twin. Change `map` to
`par_map` and the walk runs on every core the machine has:

```gossamer
let scaled = xs.par_map(|v| v * 2.0)
let kept = xs.par_filter(|v| v > 0.0)
let total = xs.par_sum()
let folded = xs.par_reduce(0.0, |a, b| a + b)
```

There is no new syntax, no channel, no handle, and nothing to join. An
adapter answers exactly what its sequential twin answers, in the same order.

| Sequential | Parallel | Answers |
|---|---|---|
| `xs.map(f)` | `xs.par_map(f)` | `Vec<U>`, elements in input order |
| `xs.filter(f)` | `xs.par_filter(f)` | `Vec<T>`, elements in input order |
| `xs.iter().fold(init, f)` | `xs.par_reduce(init, f)` | `T`, for an associative `f` |
| `xs.iter().sum()` | `xs.par_sum()` | `T` |
| `xs.min()` | `xs.par_min()` | `Option<T>` |
| `xs.max()` | `xs.par_max()` | `Option<T>` |

The receiver is a `Vec<T>`, a fixed array `[T; N]`, a slice `[T]`, or an
integer range. A range answers a `Vec` from `par_map`, where its sequential
`map` answers a lazy iterator. The lazy `iter()` surface keeps its sequential
semantics: `xs.iter().par_map(f)` is not a method, and `.collect()` first is
the spelling that works. A `Map` or `Set` has no parallel twin, because the
language promises nothing about its traversal order.

## Which callbacks an adapter takes

An adapter runs its callback on several workers at the same time. That is
sound only when the callback is a function of its arguments alone, so the
compiler checks it, rather than leaving it to convention.

The callback is a **closure literal** written at the call, or a **named
function**. Either must be pure: it performs no I/O, reads no mutable global,
starts no goroutine, writes nothing its caller can see, and calls only pure
functions. Purity is decided over the whole call graph, so a callback that
calls `mid`, which calls `log`, which prints, is refused with that path:

```text
error[GT0090]: the callback passed to `par_map` is not pure
  this callback calls `mid`, which calls `log`, which prints (`println`)
```

Mutating locals is pure: a callback may build a `Vec`, loop, and push onto it,
because that storage belongs to the call.

Two shapes are refused outright, and the reason for each is worth knowing:

- **A closure that writes something it captured.** A closure captures a `Vec`,
  `Map`, or `Set` by managed reference, so `xs.par_map(|v| { seen.push(v); v })`
  would have every worker pushing into one container at once. A captured
  scalar is refused too: every worker shares one closure environment.
- **A callable reached through a binding.** `let f = |v| v * 2` then
  `xs.par_map(f)` hides the body at the call site, so its purity cannot be
  decided there. Write the literal at the call, or name a function.

The callback shorthand keeps working because its target is known:
`xs.par_map(math::abs)`.

A callback that never returns hangs exactly as the same call would
sequentially. Termination is not checked, and nothing about it is unsound.

`par_min` and `par_max` run the element type's ordering on every worker, so a
type with its own `impl Ord` must have a pure `cmp`.

Effectful concurrency belongs in a `cohort { }` with `spawn`; GT0090 says so.

| Reach for | When |
|---|---|
| An adapter | the same pure computation over every element of a sequence |
| `cohort { }` with `spawn` | work that performs effects, talks over channels, or has a shape other than a walk |

## `par_reduce` asks for associativity and nothing else

`par_reduce(identity, combine)` answers `identity` for an empty input, and
otherwise every element combined, in index order. `combine` must be
associative: `combine(combine(a, b), c)` and `combine(a, combine(b, c))` must
agree. The compiler cannot check that, and does not pretend to.

`combine` does **not** need to be commutative. Leaves combine in index order,
so string concatenation and matrix multiplication are correct:

```gossamer
let words = #["a", "b", "c", "d", "e"]
println("{}", words.par_reduce("", |a, b| a + b))    // abcde
```

`identity` is used only when the input is empty; it is not folded into every
leaf, so an identity that is not really one does not multiply.

`par_sum`, `par_min`, and `par_max` are the specialisations whose combine the
compiler already knows, so they carry no contract for the caller.

## The same answer on every machine

A reduction cuts its input into leaves of a fixed width and combines them in
index order. The tree's shape is a function of the input length alone, never
of the worker count. So:

- a float `par_sum` answers identical bits at one worker, four, and sixteen,
  and on every tier;
- a combine that is associative but not commutative is correct;
- a combine that is not associative answers a wrong result that is the same
  wrong result on every run and every machine, which can be found.

A work-stealing scheduler splits wherever an idle worker steals, so the same
float reduction can answer different bits on two runs of one program on one
machine. The fixed tree is what rules that out here.

When a callback panics on several workers, the panic reported is the one at
the lowest element index, as the sequential walk would have raised first. A
program's failure does not depend on scheduling.

## What an adapter costs

A call too short to be worth spreading - fewer elements than one leaf - runs
where it was made, so `par_map` on three elements costs what `map` costs. With
one worker every leaf runs on the calling goroutine. Inside an `arena { }`
block an adapter also runs on the calling worker: the arena belongs to that
worker, and a helper cannot allocate into it.

The adapters speed up work that computes more than it moves. A walk whose cost
is writing its results - a cheap map over millions of elements - is bound by
memory bandwidth, and more cores add little to it.

`GOSSAMER_MAX_PROCS` pins the compiled scheduler's worker count and
`GOSSAMER_VM_GOROUTINE_WORKERS` the bytecode VM's; with neither set, the
adapters use every core. `GOS_PAR_STATS=1` prints, at exit, how many helpers
the program's adapters handed to the pool.
