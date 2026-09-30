# `lang::match`

Exhaustive pattern match expression.

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

`match` is an expression: every arm yields a value and the whole `match`
binds to it. Arms must be exhaustive - a non-exhaustive `match` is a
compile error (`GM0001`), so add a `_` arm only when every remaining case
genuinely means the same thing.

<!-- fragment -->
```gossamer
let label = match shape {
    Shape::Circle(r) => "round"
    Shape::Rect { w, h } if w == h => "square"
    Shape::Rect { .. } => "boxy"
}
```

Commas between arms are optional when the next arm begins on a new line.
Same-line expression arms use a comma. Block-bodied arms need no comma.

## Patterns

- Literals, `_` wildcard, and bindings (`name`, `mut name`).
- Enum variants: tuple `Shape::Line(n)`, struct `Shape::Named { id }`,
  unit `Shape::Dot`.
- Tuple-struct patterns: `Pt(a, b)` (a tuple struct destructures
  positionally, like a tuple).
- Struct patterns `Point { x, y }`, renamed `Point { x: a, y: b }`.
- Tuples `(a, b)`; rest `..` for the unmatched fields (`Shape::Box(..)`,
  `Point { x, .. }`).
- Reference patterns `&value` and `&mut value`. They require the matching
  shared or mutable reference and bind an independent copy of its referent.
- Ranges: closed `1..=5`, exclusive `1..5`, open-ended (`..=hi`, `lo..`);
  a range pattern is opaque to exhaustiveness, so a `_` arm is still
  required.
- Or-patterns `a | b`, `@`-bindings `n @ 1..=3`, and guards `Some(n) if n
  > 0`.

<!-- fragment -->
```gossamer
let kind = match s {
    Shape::Dot => 0,
    Shape::Line(..) => 1,
    Shape::Box(w, h) if w == h => 2,
    Shape::Box(..) => 3,
}
```

Patterns also drive `let`, `if let` / `while let`, `for`, and function
parameters - see [fn](fn.md) for parameter destructuring. The
`matches(expr, pat)` macro is a boolean one-arm match.

Reference patterns remove one reference layer before matching their inner
pattern. They work both at the top level and when nested. `&mut pattern` is
independent of `mut name`: the former matches a mutable reference, while the
latter makes a binding reassignable.
