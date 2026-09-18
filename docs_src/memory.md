# Memory model

Gossamer manages memory automatically and has no manual ownership transfer or
explicit lifetime annotations. It does have a conservative lexical borrow
checker. Every named `&` or `&mut` reference has an implicit lifetime from its
declaration through the closing brace of that scope.

Under the hood the compiled tiers use **deterministic reference
counting** with a cycle collector, not a tracing collector: most
values are reclaimed at the moment their last reference dies, RAM
stays flat and predictable, and there are no stop-the-world tracing
pauses. The `arena { }` block (below) layers bulk allocation on top
for short-lived object graphs.

## Relationship to Swift's ARC

Gossamer's compiled-tier reference counting is closely modeled on
Swift's Automatic Reference Counting (ARC), with one significant
addition.

What is the same as Swift ARC:

- **Compiler-inserted retain/release.** The compiler emits balanced
  retain/release pairs at compile time; there is no background thread
  and no tracing pass scanning the heap for liveness.
- **Deterministic, immediate reclamation.** A strong count reaching
  zero destroys the value right then and releases its children. A
  liveness pass releases at a value's *last use*, the same kind of
  last-use shortening Swift's ARC performs.
- **Weak references.** `Weak<T>` mirrors Swift's `weak`: it does not
  raise the strong count, and `upgrade()` returns `None` once the
  referent is gone. Gossamer does not provide Swift's `unowned`.

Where Gossamer diverges from Swift ARC:

- **Reference cycles are reclaimed automatically.** This is the key
  difference. Swift's ARC never collects cycles: a retain cycle leaks
  unless you break it by hand with `weak`/`unowned`. Gossamer's
  compiled tiers add a Bacon-Rajan trial-deletion cycle collector (see
  below) that reclaims cycles with no annotation, so ownership cycles
  are not a leak you have to design around. The one exception is the
  bytecode interpreter (`gos`), which backs values with Rust's
  `Arc` and does *not* collect cycles - there a strong cycle leaks,
  matching Swift's behavior. This is the same cross-tier caveat noted
  under weak references.
- **Reference counts are non-atomic by default.** Swift uses atomic
  reference counts. Gossamer counts goroutine-local objects
  non-atomically and promotes an object to atomic counting only when it
  escapes to another goroutine, so single-goroutine code does not pay
  for the synchronization.
- **`arena { }` regions** layer bulk bump-allocation on top, which ARC
  has no equivalent for.

In short: ARC's compile-time discipline and determinism, plus the
automatic cycle reclamation that ARC leaves to the programmer.

## Values vs references

- **Value-semantic types** are copied on assignment and passed
  by value: `bool`, `char`, `i8`..`i64`, `u8`..`u64`,
  `isize`/`usize`, `f32`/`f64`.
- **Reference-semantic types** share their backing storage when
  copied; the runtime reclaims the backing when the last reference
  dies. This includes `String`, `Vec<T>`, structs, enums, and closures.
  `[T; N]` is an owned fixed array. `[T]` is unsized and appears through a
  borrowed `&[T]` or `&mut [T]` view, so a bare slice is not an owned heap
  collection.

## `&` and `&mut`

`&x` creates a shared lexical view. `&mut x` creates an exclusive lexical
view. Both alias the source place rather than copying it. Any active reference
prevents mutation of its source through another path, and an active `&mut`
also prevents reads through the source. Access remains available through the
reference itself. In particular:

```gossamer
let mut xs = [1, 2]
let r = &mut xs
r[0] = 0
// xs and r both observe [0, 2]
```

`let mut r = &xs` makes the reference binding rebindable, not its
referent writable. Its type remains `&T` after a rebind. Conversely,
`let r = &mut xs` permits writes through `r` but does not make `r`
rebindable. The type checker rejects:

- A `&mut` taken on a non-`mut` binding.
- A bare value passed to an `&mut T` parameter. Write `call(&mut value)`;
  only an existing `&mut T` reference may be forwarded without another `&mut`.
- An assignment through a shared `&T` reference.
- A mutating method call on an immutable place.
- A projection that crosses any shared layer inside a nested reference chain.

These are lexical borrowing rules. You never write `'a`, and Gossamer does not
infer an earlier endpoint from a reference's last use. Use a smaller block
when source access must resume. Gossamer rejects a second named `&mut` to the
same root, a temporary `&mut` overlapping an active reference, and duplicate
`&mut` roots in one call.

Taking a mutable reference to an existing mutable-reference binding creates a
nested exclusive view of that reference slot:

```gossamer
let mut a: [i64; 2] = [1, 2]
let mut b = &mut a       // b: &mut [i64; 2]
let c = &mut b           // c: &mut &mut [i64; 2]
c[0] = 0                 // auto-dereferences both layers
// a, b, and c all observe [0, 2]
```

`c` does not create independent storage or a sibling borrow of `a`; it
exclusively borrows the reference slot `b`, which already points at `a`.
While `c` is active, `b` must be accessed through `c`. References cannot cross
goroutine boundaries. Share owned values using channels or synchronization as
described in the [concurrency memory model](design/memory_model.md).

## How reclamation works

- **Reference counting, compiler-inserted.** The MIR lowering inserts
  balanced retain/release pairs; a liveness pass releases owned values
  at their *last use* rather than at function exit, so a large
  structure does not pin memory while unrelated code runs.
- **Cycle collector.** Reference cycles are reclaimed by a Bacon-Rajan
  style collector that runs on demand (`runtime::collect_cycles()`) and from
  allocation pressure (a fixed candidate-count threshold, not a
  wall-clock timer, so collection is a deterministic function of
  allocation events). Acyclic data never pays for it. Because Gossamer
  has no user-visible finalizer, *when* a cycle is reclaimed is invisible
  to program output, so collection never changes a result or breaks
  cross-tier reproducibility.
- **Weak references.** `Weak<T>` observes a value without keeping it
  alive; `w.upgrade()` returns `Option<T>` and answers `None` once the
  referent has been reclaimed. `x.downgrade()` pins the referent it
  observes in the frame that took the weak, so the referent stays
  strongly reachable for the whole of that scope and is reclaimed when
  the scope ends, whatever the collector is doing. That makes
  `upgrade()` a function of scope, never of collection timing:
  a `Weak` aimed at a member of a cyclic shape answers the same on
  `gos`, the JIT, and `gos build`, before or after
  `runtime::collect_cycles()`. `feature-testing-examples/weak_into_strong_cycle.gos`
  pins the guarantee across all three tiers.
- **Compact representation.** A heap enum node carries an 8-byte
  runtime header. Enums with at most 4 variants store their
  discriminant in pointer tag bits, so a two-pointer tree node costs
  24 bytes - and only 16 inside an arena.

## Uniqueness

The compiled tiers prove, per function, which values are held exactly once.
A value is *unique* at a point in the program when nothing but the binding
that holds it can observe it: no other binding, container, struct field,
global, closure, goroutine, or live reference reaches the same storage.
There is no annotation for this and no diagnostic; the compiler infers it,
and a program computes the same result whether or not a proof succeeds.

Two things change when it does:

- **A copy becomes a handoff.** `let b = a` gives `b` a value of its own,
  which is a copy of `a`'s storage. When `a` is unique and nothing reads it
  again, no program can tell the copy from `a` itself, so `b` takes `a`'s
  storage instead, with no copy and no second allocation.
- **The last use hands over its share.** Handing a value to a new holder -
  a struct field, a container, a returned value - normally adds a reference
  for the holder and gives the old binding's reference back later. When the
  handoff is the binding's last use, the holder takes the binding's own
  reference, and neither count adjustment happens. A constructor that builds
  a struct from its locals and returns it pays for no reference counting on
  the way out.

What defeats the proof is anything that gives the storage a second
observer while the first is still in use:

- storing the value in a container, a field, or a struct literal and then
  reading the original binding again;
- capturing it in a closure;
- sending it to another goroutine, through `spawn` or a channel;
- writing it into a global;
- holding a live `&` or `&mut` reference to the binding;
- receiving it as a parameter, which the caller lends rather than hands
  over.

A value that crosses into a function the compiler cannot see through keeps
its ordinary copy and reference counting, because the proof only ever errs
toward sharing: treating a unique value as shared costs work, while treating
a shared value as unique would let two observers see one write.

To see what the compiler removed, build with the report:

```sh
gos build --release --uniqueness-report src/main.gos
```

Each function the proof changed gets one line on stderr naming how many
reference-count handoffs and copies it removed. `benchmarks/uniqueness/`
holds three programs whose reference-count traffic is pinned by the test
suite: a vector pipeline, a struct-heavy aggregate walk, and a string builder
loop.

## Arenas: `arena { }`

An `arena` block bump-allocates everything created while it runs and
frees the whole lot at once when the block exits:

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
    println("{}", total)
}
```

Semantics:

- **Allocation is a pointer bump** - a compare and an add, roughly an
  order of magnitude cheaper than a general heap allocation.
- **Reclamation is wholesale** - the arena's slabs are released in
  O(slabs) when the block exits, with no per-object teardown walk. The
  exit is exact on every path: early `return`, `?`, `break`, and
  normal fall-through all release the arena (the block desugars to a
  `defer`).
- **Headerless nodes.** Enum nodes of tagged-repr types (at most 4
  variants) allocated inside an arena carry **no header at all**: a
  `Node(Tree, Tree)` is exactly 16 bytes.
- **Arenas nest.** An inner `arena { }` frees at its own close brace;
  the outer arena is unaffected. Slabs from finished arenas are
  recycled, so an arena per loop iteration costs a bump-pointer reset,
  not an `mmap`.
- **Retain/release become no-ops** for arena values - the accounting
  entries recognize arena memory with a two-instruction range check.

### The contract

Nothing allocated inside an `arena { }` may be referenced after the
block exits. The block is statement-position only and yields unit, so
the obvious escape (the block's value) cannot happen, and a tail
expression inside it is deliberately discarded. The remaining escapes
are **checked for you**: a conservative front-end analysis rejects,
with `error[GM0003]`, any value allocated in the block that

- is assigned to a binding declared **outside** the block,
- is pushed into a container that outlives the block,
- is sent down a channel,
- is returned or broken out of an enclosing loop, or
- is captured in a closure or goroutine that outruns the block, or
  passed into a function that might stash it.

Reading an arena value through a method or a region-safe free function
stays allowed, so build-and-discard code is unaffected. The check is
sound by over-approximation: it may ask you to restructure a sound
program, but it never lets an escaping one compile. Run
`gos explain GM0003` for the details. (The raw
`runtime::arena_push()` / `arena_pop()` primitive below is the
unchecked escape hatch; there the contract is yours to uphold.)

Use an arena when the block is *pure computation over data that dies
together* - build, traverse, summarize, exit. If a value must survive,
let the block compute a scalar/string summary, or build the surviving
value before (outside) the arena.

Two deliberate edge-case rules. A `Weak` taken inside a block keeps
upgrading to `Some` for the rest of that block - the referent is not
individually tracked, and the escape check (`GM0003`) rejects any
program that could still hold the weak after the closing brace, so no
tier ever reads a reclaimed slab through one. And a panic that unwinds
out of a goroutine mid-arena abandons the arena's slabs to the
goroutine's teardown rather than corrupting them.

`runtime::arena_push()` / `runtime::arena_pop()` remain available as
the low-level primitive when block structure does not fit; the block
form is the idiom because it cannot be left unbalanced.

## Goroutine stacks

Each `spawn(|| expr)` launches a goroutine with its own stack. Captures are
reference-counted exactly as regular struct fields would be.

## When to reach for `RefCell<T>`-like patterns

You generally don't. Shared aliasing works directly. If you need to
mutate through a shared handle across goroutines, hold the value in a
`Mutex<T>` (from `std::sync`) and lock around every mutation.

## Stack vs heap - the pragmatic answer

- Small value types live on the stack or inline inside their
  aggregate.
- Aggregates (`String`, `Vec<T>`, structs, enums, closures) live on
  the heap, reference-counted; `T` / `T` / `T` spellings
  are accepted and transparent.
- Short-lived object graphs belong in an `arena { }`.
