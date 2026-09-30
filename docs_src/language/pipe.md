# `lang::pipe`

Forward-pipe operator `|>`, for composing free functions in a functional style. A step is either a bare callable (`x |> f`) or a closure whose parameter is the piped value (`x |> |v| f(a, v)`). Methods chain on their own and are the shorter spelling; a method chain can feed a pipe.

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## What the pipe is for

A free function has no receiver, so a chain of them nests inside-out and
reads right to left. `|>` turns that back into reading order:

```gossamer
struct Order { id: i64, total: f64 }

fn with_tax(o: Order) -> Order { Order { id: o.id, total: o.total * 1.2 } }
fn discount(pct: f64, o: Order) -> Order { Order { id: o.id, total: o.total * (1.0 - pct) } }
fn label(o: Order) -> String { f"#{o.id} {o.total}" }

fn main() {
    let o = Order { id: 1, total: 100.0 }
    println("{}", o |> with_tax |> |v| discount(0.1, v) |> label)
}
```

Without the pipe that is `label(discount(0.1, with_tax(o)))`, which the
reader has to unwrap from the inside.

## Prefer methods where a receiver exists

Anything with a receiver already chains, and the chain is shorter. Reach
for the pipe when the transforms are free functions, not to restate a
method call:

```gossamer
fn main() {
    // write this
    println("{}", "  Ab  ".trim().to_lowercase())

    // not a pipe form of the same thing
}
```

The two mix: a method chain is an ordinary operand, so it can feed a pipe
step, and a pipe step's result can be chained onto.

```gossamer
fn exclaim(s: String) -> String { s + "!" }

fn main() {
    println("{}", "  Ab  ".trim().to_lowercase() |> exclaim)
}
```

## The two step shapes

A **bare callable** takes the piped value as its only argument:

```gossamer
fn double(n: i64) -> i64 { n * 2 }

fn main() {
    println("{}", 3 |> double)
}
```

A step that **writes arguments is a closure**, and its parameter is the
slot. The parameter may sit anywhere the body reaches:

```gossamer
use std::{iter, strings}

fn main() {
    println("{:?}", "a,b,c" |> |v| strings::split(v, ","))
    println("{}", #[1, 2, 3, 4] |> |v| iter::filter(v, |x| x % 2 == 0).len())
}
```

A closure written directly as a step IS the call it stands for: the
parameter is bound in the caller's frame, so a chain of steps is one
chain, the step costs nothing a hand-written call would not, and a
`let mut` the body updates is the caller's. A body whose control flow
leaves the closure - a `return`, a `?` - keeps the closure it was
written against.

The body needs no call at all - any expression over the parameter works:

```gossamer
fn main() {
    println("{}", 3 |> |v| v * 2)
}
```

## Why the slot is named

Every std free function takes its data first, so a step could in
principle assume the leading slot. It does not, because a step may pipe
into any callee - a function this program declares, a package's, or a
method on an external receiver - and none of those owe the operator a
convention. Naming the slot means the reader sees which argument the
value fills without knowing anything about the callee:

```gossamer
use std::{iter, strings}

fn main() {
    println("{:?}", "a,b,c" |> |v| strings::split(v, ","))
    println("{:?}", #[1, 2] |> |value| iter::map(value, |v| v * 2))
    println("{}", 3 |> |v| clamp(0, 10, v))
}
```

An argument-taking step that is not a closure reports `GP0041`, whatever
the callee is - a formatting call included, since `println` is an ordinary
call. Write it as a closure: `value |> |v| println("{}", v)`.

## The retired `$`

`$` spelled the slot in earlier releases. It is no longer part of the
language, and any `$` reports `GP0027`:

- In a step: `x |> f(a, $)` is written `x |> |v| f(a, v)`.
- As a receiver: `x |> $.trim` is written `x.trim()` - a method already
  chains, and the chain can feed a pipe.
- As a callback: `xs.map($.abs)` is written `xs.map(math::abs)` or
  `xs.map(|v| v.abs())`.

`gos check --fix` rewrites an argument-taking step into the closure it
stands for, putting the parameter in the trailing slot, so confirm that
is the slot the call needs.
