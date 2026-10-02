# Sequence and reference safety model

Gossamer uses a deliberately smaller reference model than Rust. It has no
lifetime parameters and does not attach an owning allocation handle to every
reference. References are therefore non-owning lexical views. The compiler
rejects the escape forms that would require lifetime or arbitrary alias
reasoning.

## Language shape

| Type | Meaning |
|---|---|
| `[T; N]` | Owned fixed-size contiguous value. `N` is part of the type. |
| `[T]` | Unsized sequence type. It is never an owned local or field. |
| `&[T]` | Shared lexical view used at a direct call or inside a bounded block. |
| `&mut [T]` | Exclusive lexical view that may replace elements but not resize. |
| `Vec<T>` | Owned growable contiguous value. |

`#[a, b]` creates a Vec, and `[a, b]` creates a fixed `[T; N]` array whose
length is part of its type.
`.slice(start, end)` is a checked copy and returns `Result<Vec<T>,
errors::Error>`; it is not a borrowed sub-slice. The borrowed sub-slice is the
mutable window `&mut xs[lo..hi]`: a `&mut [T]` over those elements of `xs`
that borrows `xs`'s root exclusively for its lexical scope, exactly as
`&mut xs` does. A mutating method on `xs[lo..hi]` acts on that window.

Owned Vec assignment and by-value calls produce independent writable storage.
Nested Vec elements are cloned recursively. Named Vec values sent to a
goroutine or channel are cloned before publication, and every reachable
String, RC node, nested Vec, or aggregate-owned child is switched to shared
atomic RC before the handoff. GT0055 rejects every inline struct, tuple, or
array passed directly to a spawned call because that compiled spawn ABI cannot yet copy
arbitrary inline layouts. Channels support scalar-only aggregates but reject
aggregates containing nested Vec storage until their child-ownership
descriptor is complete. Publish supported fields separately and reconstruct
the aggregate in the receiving goroutine. Borrowed Vec parameters keep
write-through semantics only when their type is `&mut Vec<T>` or `&mut [T]`.

## Reference restrictions

GT0052 rejects a reference that would:

- return from a function, except a shared `&str` proven to contain only static
  string literals;
- enter an owned field, tuple, array, Vec, map, channel, or inferred container;
- borrow a temporary into a named local;
- be captured or returned by a closure;
- cross a `spawn` or a channel boundary;
- be rebound to an alias whose backing place has a shorter lexical scope.

A direct call may borrow a temporary because the view cannot leave that call.
A named reference may borrow a stable named place. Shared reference aliases
retain the original backing root, so a cursor may advance through a matched
child reference while that backing value remains live. Direct rebinding to
another stable named place is also permitted and updates the lexical borrow
record. Copying a mutable reference remains prohibited because Gossamer has
no ownership move that could invalidate the source alias.

GT0053 enforces lexical access exclusion. Shared references prevent mutation
of their source. Mutable references prevent reads, mutation, and another
borrow of their source. The reference remains active until the closing brace,
not merely until its last use. GT0054 rejects aggregate reference patterns,
which would otherwise copy an owned aggregate out of a non-owning view.

## Runtime representation

Vec values use the versioned `GosVec` header defined in
`crates/gossamer-runtime/src/c_abi/vec.rs`. The stable prefix contains length,
capacity, element width and kind, flags, reference count, and the data pointer.
Allocation identity, aggregate metadata, ownership metadata, and structural
mutation generation follow it.

An array-to-slice call currently builds a compact `GosVec` view header with
`gos_rt_vec_borrow_arr`. Its data pointer aliases the array storage and the
header does not own or drop array elements. This representation is valid only
because the type checker keeps the view call-scoped and the source lexical
place live. It is not a general escaping slice representation.

Vec-to-slice calls use the source Vec header. The slice method catalog cannot
resize it, and the lexical checker prevents source access while a mutable view
is active. The VM, Cranelift, and LLVM consume the same checked HIR and MIR
shape. The FFI surface does not expose a source-language raw pointer or an
escaping slice handle.

Lazy sequence iterators retain their Vec source and snapshot its allocation
and structural-mutation generations. Structural mutation produces a runtime
panic. Iteration yields managed element values rather than raw element
references.

## Invariants and enforcement

| Invariant | Enforcement |
|---|---|
| Bare `[T]` is never sized storage | GT0049 in AST type lowering. |
| A view cannot outlive a local, frame, or temporary | GT0052 escape restrictions; temporary views are direct-call-only. |
| Vec growth cannot invalidate an escaping slice | Slice references cannot escape; owned Vec boundaries clone storage. |
| Shared and writable views do not overlap source access | GT0053 lexical named-root exclusion and duplicate mutable-argument checks. |
| A view never drops borrowed elements | Borrowed array headers are untagged non-owning views and are not element owners. |
| Nested Vec copies do not share growable child headers | `gos_rt_vec_clone` recursively clones nested Vec slots. |
| Reference values do not cross concurrency boundaries | GT0052 checks `spawn`, closure capture, channel types, and channel sends. |
| Nested growable storage does not dangle or double-release across concurrency | GT0055 rejects all inline aggregate spawned-call arguments and aggregate-with-Vec channel sends; top-level Vec publication clones storage and marks every managed child shared. |
| Bounds and offset arithmetic are checked | Sequence indexing and `.slice` use checked integer bounds before runtime access. |
| Backends agree on owned parameter behavior | MIR inserts Vec clones before compiled by-value calls; parity tests cover VM, JIT, and LLVM. |

## Rejected alternatives

A raw pointer-and-length slice was rejected as a general model because
retaining a Vec header does not pin a buffer that can be replaced during
growth. Retaining an intermediate slice header also does not tie a nested view
to the original allocation. A copy-on-write backing-storage redesign remains
possible, but it would require a new versioned ABI and every inline backend
load and mutation to participate. Runtime borrow flags alone were also
rejected because uninstrumented inline backend accesses could bypass them.

The current restriction model is smaller and enforceable with the existing
ABI. It intentionally omits escaping sub-slices. The mutable window
`&mut xs[lo..hi]` is a pointer-and-length header, and it is sound for the
reason a general one is not: it is a `&mut` borrow of `xs`'s root, so nothing
else reads, writes, grows, or frees that root while the window lives, and
GT0052 keeps it from outliving its lexical scope. The window owns its header
alone; the elements stay the sequence's.

## Remaining proof boundary

The rules above cover source-language references and the sequence operations
that create them. They do not prove the safety of arbitrary foreign code, an
incorrect hand-written binding, or unrelated mutable values deliberately
shared between goroutines. Those boundaries require their own ABI validation
or synchronization contract. `gos test --race` remains the diagnostic tool
for explicitly shared concurrent state outside this reference model.
