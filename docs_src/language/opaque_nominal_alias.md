# `lang::opaque_nominal_alias`

`type Name = new Repr` declares a distinct nominal type over an unchanged runtime representation, erased before lowering so no tier sees one. It inherits equality, ordering, hashing, and formatting - which describe the value both sides share - and nothing else: arithmetic needs the alias's own `impl Add`, and the representation's methods are not in scope. `.into()` converts to and from its own representation; any other pair needs `impl From`.

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

A transparent alias (`type Id = i64`) is another spelling of its target.
An **opaque** alias puts `new` before the target and declares a type of
its own:

```gossamer
newtype UserId = i64
newtype Score = i64
```

`UserId` and `i64` are now different types, and so are `UserId` and
`Score`. Nothing converts between them on its own:

<!-- fragment -->
```gossamer
let id: UserId = 41.into()
let n: i64 = id       // error[GT0001]: type mismatch: expected `i64`, found `UserId`
let s: Score = id     // error[GT0001]: type mismatch: expected `Score`, found `UserId`

fn charge(user: UserId, amount: Score) { }
charge(amount, user)  // rejected: the arguments cannot be swapped by accident
```

The distinction is entirely in the checker. The runtime value is exactly
the representation, so an opaque alias costs nothing, changes no layout,
and behaves identically on the bytecode VM, the JIT, and a native build.

## Converting

`.into()` crosses between an alias and its own representation, in both
directions. The two are one value, so the conversion carries no work:

<!-- fragment -->
```gossamer
let id: UserId = 41.into()
let raw: i64 = id.into()
```

Every other pair needs a conversion written for it. Reaching for
`.into()` without one is reported at check time rather than failing when
the program runs. So is a call no use site gives a target: the target of
`.into()` comes from the annotation, parameter, or return that receives
it, never from the receiver.

<!-- fragment -->
```gossamer
let s: Score = id.into()
// error[GT0066]: no conversion from `UserId` to `Score`
//   = help: write `impl From<UserId> for Score`
```

<!-- fragment -->
```gossamer
impl From<UserId> for Score {
    fn from(u: UserId) -> Score { (u.value() * 10).into() }
}

let s: Score = id.into()   // runs the impl
```

## What an opaque alias inherits

Nothing from its representation's behaviour. Hiding what the type is
made of is the whole point, so the representation's API is not part of
the alias:

<!-- fragment -->
```gossamer
newtype Name = String

let n: Name = "ada".into()
n.len()          // error[GT0002]: no method named `len` found for type `Name`

let a: UserId = 1.into()
let b: UserId = 2.into()
a + b            // error[GT0003]: cannot apply `+` to `UserId`
```

Both are deliberate. Adding two user ids is rarely meaningful, and where
it is, it is worth writing down. To reach the representation's surface,
convert to it, or give the alias a method of its own.

**Equality, ordering, hashing, and formatting are inherited.** Those
describe the value, which the alias and its representation genuinely
share, and they are what let an alias be a `Map` or `Set` key, sort, and
print:

<!-- fragment -->
```gossamer
let mut seats: Map<UserId, String> = Map::new()
seats.insert(a, "front")
println("{} {} {}", a < b, a == b, a)   // true false 1
```

## Its own impl

An opaque alias carries inherent and operator `impl` blocks like any
other type:

<!-- fragment -->
```gossamer
impl UserId {
    fn value(&self) -> i64 { self.into() }
    fn next(&self) -> UserId { (self.value() + 1).into() }
}

impl Add for UserId {
    fn add(&self, other: UserId) -> UserId {
        (self.value() + other.value()).into()
    }
}
```

## Choosing between the three

| Want | Use |
| --- | --- |
| A shorter spelling of an existing type | `type Id = i64` |
| A distinct type over the same representation | `newtype Id = i64` |
| Named fields, or a layout of its own | `struct Id { .. }` |

## Serialization

A struct field typed by an alias serializes as its representation, and
decoding converts back, so an alias is usable in the types a program
actually exchanges:

```gossamer
newtype UserId = i64

struct Rec { id: UserId, n: i64 }

let text = to_json::<Rec>(Rec { id: 7.into(), n: 2 })?   // {"id":7,"n":2}
let back = from_json::<Rec>(text)?                      // back.id is a UserId
```
