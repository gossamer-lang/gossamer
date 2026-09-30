# `std::fmt`

Status: experimental

Formatted printing and string interpolation.

## Items

| Item | Signature | Description |
|---|---|---|
| `Display` | `trait Display` | How a value renders through `{}`. The rendering is synthesized; `impl Display for T { fn fmt(&self) -> String }` overrides it, and `x.to_string()` reaches the same rendering. |
| `Debug` | `trait Debug` | How a value renders through `{:?}`. The rendering is synthesized; `impl Debug for T { fn fmt(&self) -> String }` overrides it. |
| `println` | `builtin println` | Prints to stdout followed by a newline. |
| `print` | `builtin print` | Prints to stdout without a trailing newline. |
| `eprintln` | `builtin eprintln` | Prints to stderr followed by a newline. |
| `eprint` | `builtin eprint` | Prints to stderr without a trailing newline. |
| `format` | `builtin format` | Formats arguments into an owned `String`. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
