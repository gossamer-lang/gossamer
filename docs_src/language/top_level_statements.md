# Top-level statements

The entry file may skip the `fn main` wrapper. Write statements at file
scope and the file becomes the body of an implicit `fn main()`:

```gossamer
println("Hello World")
```

Functions and other items declared alongside top-level statements are
hoisted to file scope, exactly as if you had written them outside `main`:

```gossamer
fn greet(name: String) -> String { format("hi, {name}") }

let who = "ada"
println("{}", greet(who))
```

## Mental model

The entry file is implicitly wrapped in `fn main()`. This is the model
Swift, Kotlin scripts, and Deno/Node ES modules use. Top-level `let`
bindings are locals of that `main`; share state across functions with
`const` / `static`.

## Return type

If any top-level statement uses `?`, the implicit `main` returns
`Result<(), errors::Error>` so propagation works:

```gossamer
use std::fs

let text = fs::read_to_string("config.toml")?
println(f"{text}")
```

Otherwise `main` returns `()`. To set a process exit code, call
`std::process::exit(n)`:

<!-- fragment -->
```gossamer
use std::process

if bad_input {
    eprintln("usage: tool <path>")
    process::exit(2)
}
```

## Rules

- Only the entry file may carry top-level statements (the file run
  directly, or `[project] entry`). Module / library files contain items
  only.
- A file uses one entry form: mixing top-level statements with an explicit
  `fn main` is an error.
