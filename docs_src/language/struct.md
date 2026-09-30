# `lang::struct`

Product type declaration.

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## Functional record update

A struct literal may spread a base value with `..base` and override
individual fields:

<!-- fragment -->
```gossamer
let p2 = Point { x: 10, y: p1.y }
```

- Explicit fields win over the base for the same name.
- Exactly one `..base` spread is allowed (a second is a parse error).
- A spread references every field the literal does not name, so the whole
  struct has to be visible where the update is written. A struct with any
  private field cannot be updated from outside the module that declares it,
  exactly as it cannot be constructed there - see
  [visibility](visibility.md).
- Fields copied from the base share its heap children and are retained,
  so the base stays usable after the update with no double-free. Output
  is identical across the VM, Cranelift, and LLVM tiers.

## Declaration and construction

Struct declarations follow Rust's three shapes: unit structs, named-field
structs, and tuple structs. Empty named structs use braces, and empty tuple
structs use parentheses:

```gossamer
struct Unit
struct Empty {}
struct EmptyTuple()
struct Pt { x: i64, y: i64 }
struct Pair(String, i64)

let unit = Unit
let empty = Empty {}
let empty_tuple = EmptyTuple()
let p = Pt { x: 3, y: 4 }     // keyed fields, any order
let pair = Pair("row", 4)
println(f"{p.x} {p.y}")
println(f"{pair.0} {pair.1}")
let Pt { x, y } = p
```

Named structs must be constructed with keyed fields in `Name { field: value }`;
both `Name(...)` and positional `Name { value }` forms are rejected. Unit
structs use either `Name` or `Name {}`, while tuple structs must be constructed
with `Name(...)`.

On one line, commas separate fields. In a multiline declaration or literal,
newlines separate fields. Multiline commas are accepted for migration and
`gos fmt` removes them:

```gossamer
struct Point {
    x: i64
    y: i64
}

let p = Point {
    x: 3
    y: 4
}
```

## Value semantics: copy and compare with no derive

Structs are value types. Binding or `.clone()`-ing copies the whole value
(heap children retained), and `==` / `!=` / `<` / `<=` / `>` / `>=` are
synthesized automatically - no `#[derive]` needed - whenever every field
is comparable (scalars, `String`, nested comparable types). Ordering is
lexicographic by field declaration order:

```gossamer
struct Point { x: i64, y: i64 }

let a = Point { x: 1, y: 2 }
let b = a.clone()                          // `clone` is a universal builtin
println("{}", a == b)                     // true, no derive
println("{}", a < Point { x: 1, y: 3 })   // true (lexicographic by field)
```

A user `impl` of `eq` / `cmp` overrides the synthesized comparison.

## Derivable traits

`#[derive(...)]` is limited to `Debug`, `Default`, `PartialEq`, `Eq`,
`PartialOrd`, and `Ord`, synthesized as real source so `{:?}`,
`Type::default()`, and the comparisons work on every tier. The derive
only *forces* synthesis where the automatic gate is conservative (generic
or container-typed fields):

```gossamer
#[derive(Debug, Default, PartialEq)]
struct Config { retries: i64, verbose: bool }
```

`Clone`, `Copy`, `Hash`, `Display`, `Serialize`, and `Deserialize` are
**not** derivable (`GT0025`): copying, hashing, and serialization are
already automatic. Conversion / operator traits (`From`, `Add`, ...) are
written `impl Trait for T`, not derived.
