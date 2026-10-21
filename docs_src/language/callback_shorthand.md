# `lang::callback_shorthand`

A callback written without `|v|`: a std free function named in value position stands for the closure that calls it, as in `xs.map(math::abs)`.

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

A callback slot takes a closure. When that closure would do nothing but
call one std function on its parameter, name the function instead.

## A std function in value position

Naming a std free function where a callback is expected takes the closure
that calls it:

```gossamer
use std::{encoding::base64, math}

fn main() {
    println("{:?}", #[1.0, -2.0].map(math::abs))
    println("{:?}", #["ab".as_bytes(), "cd".as_bytes()].map(base64::encode))
}
```

Each of those is the closure `|v| math::abs(v)` / `|v| base64::encode(v)`.
The parameter count comes from the function's own signature, so a std item
with no fixed parameter list has no closure to stand for and reports
`GT0015`. A macro is not a function at all: `fmt::format` reports `GR0018`
and is written `format(..)`, inside a closure of your own:

```gossamer
fn main() {
    println("{:?}", #[1, 2].map(|v| format("<{}>", v)))
}
```

## Everything else is a written closure

There is no projection shorthand. A method call, a field read, an index,
and a tuple projection are each written as the closure they are:

```gossamer
struct Person { name: String }

fn main() {
    println("{:?}", #[" a ", " b "].map(|v| v.trim()))
    println("{:?}", #[(1, 2), (3, 4)].map(|t| t.0))

    let people = #[Person { name: "ada" }, Person { name: "grace" }]
    println("{:?}", people.map(|p| p.name))
}
```

`$` is not a callback shorthand, and is not part of the language at all:
`xs.map($.abs)` reports `GP0027`. See [`|>`](pipe.md) for the closure a
pipe step is written as.
