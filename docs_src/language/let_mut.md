# `lang::let_mut`

Mutable bindings can be reassigned and can be the source of `&mut`.

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

Binding mutability and reference capability are separate:

```gossamer
let mut value = [1, 2]
{
    let reference = &mut value
    reference[0] = 0       // writes value
}

let other = [3, 4]
let mut shared = &value
shared = &other        // rebinds shared; it remains read-only
```

The left side of `=` is a pattern and the right side is an expression.
`&mut value` on the right creates a mutable reference. `&mut pattern` on the
left matches a mutable reference, removes that reference layer, and copies the
referent into the inner pattern:

```gossamer
let mut count = 3
let reference = &mut count
let &mut copy = reference
```

`copy` is an independent `i64` value and is not reassignable. Only
`mut name` makes a binding reassignable. Reference patterns also compose with
other patterns, for example `let name, &mut count = entry`. For a simple
top-level copy, `let copy = *reference` is usually clearer.

An immutable binding cannot be used as the source of `&mut`, because that
would create a write path around its immutability.

Declaring a binding `mut` does not let a call infer mutable access. Pass the
place explicitly as `change(&mut value)`. `change(value)` is rejected when the
parameter is `&mut T`. A value already typed as `&mut T` can be forwarded
directly.

Implicit dereferencing does not weaken this rule. If an access path contains
any shared `&T` layer, indexing, field assignment, and mutating method calls
through that path are rejected even when every outer reference is `&mut`.

A named reference has an implicit lexical lifetime ending at the closing
brace. While `let reference = &mut value` remains active, `value` cannot be
read or mutated through another path. Use a smaller block when source access
must resume. In the persistent REPL scope, `%drop reference` ends and removes
the reference binding explicitly.
