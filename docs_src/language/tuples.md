# `lang::tuple`

Fixed-length group of values whose element types may differ.
<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

A tuple is written with parentheses and needs no import. Its type is the
parenthesised list of its element types, so `(1, "two", 3.0)` has type
`(i64, String, f64)`.

```gossamer
let entry = (1, "two", 3.0)
println(f"{entry.0} {entry.1} {entry.2}")
```

Unlike a `Vec<T>`, a tuple's length is part of its type and its elements do not
have to share a type. Unlike a struct, its fields are positional and it needs no
declaration.

## Construction

```gossamer
let pair = (3, 4)          // (i64, i64)
let mixed = (1, "two", 3.0) // (i64, String, f64)
let single = (5,)           // one-element tuple; the comma is required
let unit = ()               // the empty tuple
```

`(5)` is a parenthesised expression, not a tuple - the trailing comma in `(5,)`
is what makes it one.

## Positional access

Elements are read with `.0`, `.1`, `.2`, and so on. Reads chain through nested
tuples:

```gossamer
let nested = ((1, 2), "outer")
println(f"{nested.0.1}")   // 2
```

A `mut` binding assigns positionally, including through a nested tuple:

```gossamer
let mut counter = (0, "hits")
counter.0 = 7
let mut grid = ((1, 2), 3)
grid.0.1 = 42
```

## Destructuring

A tuple pattern binds every element at once, in `let`, in `for`, in `match`, and
in a function's parameter list:

<!-- fragment -->
```gossamer
let id, name, weight = (1, "two", 3.0)

for (key, value) in map.iter() {
    println("{key}={value}")
}

fn label((rank, name): (i64, String)) -> String {
    format("{rank}: {name}")
}

match point {
    (0, 0) => println("origin"),
    (x, _) => println("x = {x}"),
}
```

## Comparison and ordering

Tuples compare structurally, element by element in declaration order, with no
`#[derive(...)]`. Equality needs every element equal; ordering is
lexicographic, so the first differing element decides.

```gossamer
println("{}", (1, 2) < (1, 3))     // true
println("{}", (1, "a") == (1, "a")) // true
```

That ordering is what `sort` uses on a sequence of tuples, which makes a tuple
the usual sort key:

```gossamer
let mut pairs = #[(3, "c"), (1, "a"), (2, "b")]
pairs.sort()
```

## Where tuples appear

A tuple is an ordinary value: it can be a function return, a struct field, a
`Vec` element, a `Map` key, or a channel payload.

```gossamer
fn min_max(xs: [i64]) -> (i64, i64) {
    (xs.min().unwrap_or(0), xs.max().unwrap_or(0))
}

struct Reading { at: (i64, i64), value: f64 }

let by_position: Map<(i64, i64), String> = Map::new()
```

`Map::iter()` yields `[(K, V)]`, and `Vec::enumerate()` yields
`Vec<(i64, T)>`, so the `for (a, b) in ...` shape reads the same everywhere.

## Destructuring assignment

A comma-separated list on the left of `=` writes each element to its own
target, so several places update from one right-hand side. The right-hand
side is evaluated in full before the first write, which is what makes a swap
need no temporary:

```gossamer
let mut a = 1
let mut b = 2
a, b = b, a
```

Every target follows the rules a scalar assignment follows: it must be a
writable place, so a binding, a field, an index, a tuple position, or a
dereference. Targets may nest, and `_` discards the element opposite it:

<!-- fragment -->
```gossamer
let mut point = Point { x: 0, y: 0 }
let mut cells = #[0, 0]
point.x, cells[1] = 5, 6

let mut head = 0
let mut left = 0
let mut right = 0
head, (left, right) = 1, (2, 3)

let mut kept = 0
_, kept = 99, 42
```

A compound operator pairs element-wise: each place is read, combined with its
own element of the right-hand value, and written back, so `x, y += 2, 3` is
`x += 2` and `y += 3`.

Parentheses group a pattern only where one sits beside others - a `match`
arm, a `for` binding, a parameter, or a nested element of the list itself.
A parenthesised target list reports `GP0042` and carries the rewrite.

## Methods

A tuple's surface is mostly syntax: positional access, destructuring, and
structural comparison. Its methods are the four that do not assume a sequence:

| Method | Returns |
|---|---|
| `len()` | element count, folded at compile time from the type |
| `is_empty()` | true only for `()` |
| `get(i)` | element at a runtime index; prefer `t.0` when the position is known |
| `clone()` | a copy of the tuple |
| `to_string()` | `(a, b, ...)`, the text `{}` and `{:?}` produce |
| `into()` / `try_into()` | conversion through a `From` / `TryFrom` impl |

`iter()` and the combinators built on it are rejected: a tuple's elements may
differ in type, so there is no element type to yield. Walk a tuple by
destructuring it, not by iterating it.

## Discovery

`%info Tuple` in the REPL describes the type, and `%explain <binding>` on a
tuple binding lists its positional elements and their types:

```text
>>> let t = (1, "two", 3.0)
>>> %e t
t: (i64, String, f64) [binding]
t.0: i64 [element]
t.1: String [element]
t.2: f64 [element]
```
