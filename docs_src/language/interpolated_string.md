# `lang::interpolated_string`

`f"..."` string literal whose `{expr}` / `{expr:spec}` placeholders render any expression, with `format`'s specs: `f"{xs.len()} items, {total:.2}"`.

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

An interpolated string writes values where they appear in the text. Each
`{..}` placeholder holds an expression, and the literal is a `String` like
any other, so it goes wherever a string does:

```gossamer
let name = "Ada"
let visits = #[3, 5]
println(f"{name} has visited {visits.len()} times")

let greeting = f"hello, {name.to_uppercase()}!"
```

A placeholder holds any expression - a binding, a field path, a call, an
index, arithmetic, a conditional - and may carry the same `:spec` a
`format` placeholder does after the first `:` at its top level:

```gossamer
struct Account {
    owner: String
    balance: f64
}

let account = Account { owner: "Grace", balance: 1234.5 }
println(f"{account.owner:>8} {account.balance * 1.05:.2}")

let pair = (3, "three")
println(f"{pair.0} is spelled {pair.1}")

let scores = { "ada": 91 }
let n = 255
println(f"{scores["ada"]} {n:x} {n + 1:08} {if n > 99 { "big" } else { "small" }}")
```

The placeholder ends at the `}` that closes it, so brackets, strings, and
braces inside the expression are its own. A `::` path (`{i64::MAX}`) is not
a spec; an expression that needs a top-level `:` of its own goes in
parentheses. Each expression is evaluated once, left to right.

`{{` and `}}` write a literal brace; a lone `{` or `}` is `GP0065`, and an
empty placeholder (`{}`, `{:.2}`) is `GP0063`. A triple-quoted body
interpolates the same way: `f"""..."""` is dedented as a `"""` string is,
and its placeholders may span lines.

## `f"..."` or `format`

Both build the same `String`, on every tier. Reach for `f"..."` when the
text reads best with the values in place. Keep `format("...", a, b)` when
one template is filled from several places, or the arguments are long
enough that the text reads better on its own:

```gossamer
let items = #[4, 8]
println(format("{} items, {} in total", items.len(), items.iter().sum()))
```
