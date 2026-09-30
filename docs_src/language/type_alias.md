# `lang::type_alias`

Transparent type alias: `type X = T` (and generic `type Pair<A> = (A, A)`) is interchangeable with its target everywhere; a cyclic alias is rejected (`GT0024`).

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

A `type` declaration names an existing type. It is **transparent** - an
alias is not a new nominal type, just another spelling of its target, so
the two are interchangeable wherever a type is written: `let` bindings,
parameters, returns, struct fields, and nested composites.

```gossamer
type Id = i64
type Names = Vec<String>

fn next(id: Id) -> Id { id + 1 }   // Id and i64 are the same type

let a: Id = 41
println("{}", next(a))            // 42
let ns: Names = #["go", "rust"]
```

## Generic aliases

An alias may take type parameters; they are substituted with the
use-site arguments.

```gossamer
type Pair<A> = (A, A)

let p: Pair<i64> = (3, 4)
let x, y = p
```

## Alias chains

Aliases may refer to other aliases; the chain is expanded to the
underlying type. A chain that expands to itself is a cyclic alias and is
rejected at check time with `GT0024`:

<!-- compile_fail GT0024 -->
```gossamer
type A = B
type B = A      // error[GT0024]: type alias `B` is cyclic - it expands to itself
```

## Opaque aliases

Writing `new` before the target - `newtype UserId = i64` - declares a
distinct type over the same representation instead of another spelling
of it. It inherits equality, ordering, hashing and formatting and
nothing else, converts to and from its representation with `.into()`,
and carries its own `impl`. See
[opaque nominal aliases](opaque_nominal_alias.md).

Use a transparent alias to shorten a spelling, an opaque one to make a
distinction the checker enforces, and a single-field `struct` when the
type needs named fields or its own layout.
