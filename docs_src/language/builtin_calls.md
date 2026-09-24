# `lang::builtin_calls`

The compiler-known call names, written without a sigil: the format family (print/println/eprint/eprintln/format/panic), the desugaring calls (matches/todo/unimplemented/unreachable/dbg), and codegen. The set is closed and there are no user-defined macros.

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

A handful of call names are answered by the compiler rather than resolved
to a function. Each is written as an ordinary call - there is no `!` - and
the set is closed: `name!(..)` reports `GP0049` with the rewrite that
drops the sigil, and nothing may be declared under one of these names
(`GR0020`).

## Format calls

`format`, `println`, `print`, `eprintln`, `eprint`, and `panic` take a
template plus arguments. Placeholders are positional `{}`, named
`{ident}` (a binding in scope), and `{:spec}` / `{name:spec}` with Rust's
width / fill / align / radix / precision grammar:

```gossamer
let name = "jane"
println("hello, {name}")          // named capture
println("[{:>8.2}]", 3.14159)     // [    3.14]
let msg = format("{} / {}", 1, 2) // returns a String
```

One rule decides how the arguments are read: **argument zero is the
template when it is a string literal**. A lone non-literal argument
renders as if the template were `"{}"`, so `println(value)` prints the
value and never interprets braces the value happens to carry. Two or more
arguments without a literal template report `GP0024`.

Every explicit positional argument must match one positional placeholder,
and extra or missing arguments are parse errors.

A named capture also walks a field path: struct fields (`{a.balance}`),
tuple indices (`{t.0}`), nesting (`{o.inner.hits}`), and specs on the
path (`{a.balance:>8}`, `{f.0:.2}`) all resolve against bindings in
scope:

```gossamer
struct Account { owner: String, balance: i64 }
let a = Account { owner: "jane", balance: 1200 }
println("{a.owner}: {a.balance:>8}")
```

Any other expression in a placeholder (`{age + 1}`, `{v[i]}`) is a parse
error (`GP0021`) - bind it first or pass it positionally. So is a spec the
grammar does not take (`{:+}`, `{:e}`), which would otherwise print as text.

## Desugaring calls

These expand at parse time into ordinary constructs, so they lower
uniformly on every tier:

- `matches(expr, pat)` - boolean: does `expr` match the pattern.
- `todo()` / `unimplemented()` / `unreachable()` - `panic` with a fixed
  (or supplied) message.
- `dbg(expr)` - prints `expr` with `{:?}` to stderr and yields its value.

```gossamer
if matches(n, 1..=9) { /* single digit */ }
let x = dbg(compute())   // logs the value, returns it
```

## Build-time validation

`regex::compile("…")` and `sql::statement("…")` validate a literal
argument at compile time and fold to the validated string;
`codegen(...)` splices a `comptime fn`'s `String` result back as source.
See [comptime](comptime.md).
