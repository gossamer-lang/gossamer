# Builtins and prelude

Gossamer puts these names in every file. No `use` is needed. A local
definition with the same name wins.

Standard library modules are not part of the prelude. Import each module
before using its qualified name:

```gossamer
use std::{env, fs}

let args = env::args()
let text = fs::read_to_string("input.txt")?
```

Without the corresponding `use`, `env::args()` and
`fs::read_to_string(...)` are unresolved. Importing a module does not place
all of its functions in the bare-name namespace.

## Output and formatting

Format strings use `{}` and `{name}` placeholders.

| Name | Signature | Description |
|---|---|---|
| `println` | `println("fmt", values...)` | Print formatted text to stdout, then a newline. |
| `print` | `print("fmt", values...)` | Print formatted text to stdout with no newline. |
| `eprintln` | `eprintln("fmt", values...)` | Print formatted text to stderr, then a newline. |
| `eprint` | `eprint("fmt", values...)` | Print formatted text to stderr with no newline. |
| `format` | `format("fmt", values...) -> String` | Render formatted text into an owned `String`. |
| `panic` | `panic("fmt", values...) -> !` | Stop the current goroutine with the rendered message. |

```gossamer
let who = "world"
println("hello, {who}")
println("{} + {} = {}", 1, 2, 1 + 2)
```

Output that ends in a newline leaves the process on the write that ends it,
whether stdout is a terminal, a pipe, or a file, so a program that announces a
line and then blocks is visible to whatever is reading it. `print` with no
terminator accumulates: a prompt written that way needs
`io::stdout().flush()` before the read. `eprint` / `eprintln` are never
buffered and flush stdout first, so the two streams keep their order.

## Fixed compiler-known calls

| Name | Signature | Description |
|---|---|---|
| `matches` | `matches(expr, pattern) -> bool` | Test whether `expr` matches `pattern`. |
| `todo` | `todo("msg"?) -> !` | Mark code as intentionally unfinished and panic if reached. |
| `unimplemented` | `unimplemented("msg"?) -> !` | Mark an unsupported path and panic if reached. |
| `unreachable` | `unreachable("msg"?) -> !` | Mark an impossible path and panic if reached. |
| `dbg` | `dbg(expr) -> T` | Print `expr` with debug formatting, then return it. |
| `regex::compile` | `regex::compile("pattern") -> regex::Pattern` | Compile a regular expression; a literal pattern is checked while the program is parsed. The `regex` module is imported like any other: `use std::regex`. |
| `sql::statement` | `sql::statement("query")` | Check a SQL literal at build time when a driver can validate it. |
| `codegen` | `codegen(...)` | Run the build-time codegen hook. |

Every one of these is an ordinary call. The set of compiler-known names is
closed and recognised at the `(`, so no sigil disambiguates them: `name!(...)`
is a parse error, and user-defined macros do not exist.

## Assertions

| Name | Signature | Description |
|---|---|---|
| `assert` | `assert(cond: bool, msg?: String)` | Panic when `cond` is false. |
| `assert_eq` | `assert_eq(a, b, msg?: String)` | Panic when `a != b`; include both values in the failure text. |

Use `todo(...)` for unfinished code.

## Scalar helpers

| Name | Signature | Description |
|---|---|---|
| `min` | `min(a, b) -> T` | Return the smaller comparable scalar. |
| `max` | `max(a, b) -> T` | Return the larger comparable scalar. |
| `min` | `min(xs) -> Option<T>` | Return the smallest collection item, or `None` when empty. |
| `max` | `max(xs) -> Option<T>` | Return the largest collection item, or `None` when empty. |
| `clamp` | `clamp(x, lo, hi) -> T` | Limit `x` to the inclusive range `[lo, hi]`. |

```gossamer
let speed = clamp(input, 0, 120)
let better = max(score_a, score_b)
```

## Concurrency

| Name | Signature | Description |
|---|---|---|
| `spawn` | `spawn(f) -> JoinHandle<T>` | Run `f` on a goroutine and return a join handle. |
| `join` | `handle.join() -> Result<T, String>` | Wait for a spawned goroutine. `Err` carries the panic message. |

```gossamer
let h = spawn(|| heavy_compute())
match h.join() {
    Ok(v) => println("{v}"),
    Err(e) => eprintln("worker panicked: {e}"),
}
```

`Sender`, `Receiver`, `Mutex`, and `WaitGroup` type names resolve in
signatures without imports. Their constructors and helpers live in `std::sync`.

## Serialization

Every user `struct` gets strict typed codecs. No derive attribute needed.

| Name | Signature | Description |
|---|---|---|
| `from_json` | `from_json::<T>(text) -> Result<T, errors::Error>` | Decode JSON into `T`; report missing fields and type mismatches. |
| `to_json` | `to_json::<T>(value) -> Result<String, errors::Error>` | Encode `T` as JSON text. |
| `from_toml` | `from_toml::<T>(text) -> Result<T, errors::Error>` | Decode TOML into `T`; report schema errors. |
| `to_toml` | `to_toml::<T>(value) -> Result<String, errors::Error>` | Encode `T` as TOML text. |
| `from_yaml` | `from_yaml::<T>(text) -> Result<T, errors::Error>` | Decode YAML into `T`; report schema errors. |
| `to_yaml` | `to_yaml::<T>(value) -> Result<String, errors::Error>` | Encode `T` as YAML text. |

```gossamer
struct Config { host: String, port: i64 }

let cfg = from_json::<Config>(text)?
```

Use `std::encoding::json` for dynamic JSON where the shape is not known at
compile time.

## Always-in-scope types

| Family | Names | Description |
|---|---|---|
| Primitives | `bool`, `char`, signed and unsigned integers, `isize`, `usize`, `f32`, `f64`, `String`, `str` | Scalar and text types. |
| Wrappers | `Option`, `Result`, `Weak` | Sum types, and the non-owning reference into an RC allocation. |
| Collections | `Vec`, `Map`, `Set`, `BTreeSet`, `BTreeMap`, `Deque`, `Queue`, `Stack`, `MaxHeap`, `MinHeap`, `Range`, `Iterator` | Core collection and sequence types. |
| Concurrency | `Sender`, `Receiver`, `Mutex`, `WaitGroup`, `JoinHandle` | Channel, lock, wait, and goroutine-handle types. |
| Lane vectors | `Simd`, `Mask` | Fixed-width vectors of `N` lanes of one scalar type, and their `bool` form; see [lane vectors](language/simd.md). |

`Range` is the type of a `0..n` bound: iterator state over a bounded integer
sequence, so it answers the same combinator surface `Iterator` does.

```gos
let counted: Range = 0..5
println("{:?}", counted.iter().rev().collect())
```

## Runtime statements

| Statement | Description |
|---|---|
| `defer expr` | Run `expr` on every exit path of the enclosing block. |
| `arena { ... }` | Allocate values inside a region and free them when the block exits. |
| `select { ... }` | Wait on multiple channel operations. |
