# Migrating from Rust to Gossamer

Gossamer deliberately feels Rust-shaped: `fn`, `struct`, `enum`,
`impl`, `trait`, `match`, modules, attributes, and `Result<T, E>` all
look familiar. The important differences are ownership, borrowing,
concurrency, and which Rust features are intentionally absent.

## Quick Map

| Rust | Gossamer |
| --- | --- |
| `fn f(x: i64) -> i64 { x + 1 }` | Same. |
| `struct Point { x: i64, y: i64 }` | Same declaration. |
| `Point { x: 1, y: 2 }` | Same named literal. |
| tuple structs and enum variants use `Name(...)` | Same. |
| named struct positional shorthand is unavailable | Same. Use keyed fields. |
| `Option<T>` and `Result<T, E>` | Same core shape. |
| `?` | Same propagation model. |
| `format!("{name}: {}", x)`, `println!(..)` | `f"{name}: {x}"`, `println(..)` - no `!` on any call |
| `vec![a, b]` | `#[a, b]` (GP0064 rewrites it) |
| `async fn` and `.await` | Declined (GP0061). Use `spawn(|| expr)` plus channels or blocking calls. |
| `std::thread::spawn` | `spawn(|| { ... })` inside a `cohort { }` |
| `dyn Trait` | Declined (GP0061). Use generics or an enum. |
| `x.wrapping_add(y)`, `x.wrapping_mul(y)` | `x +% y`, `x *% y` |
| `x.checked_add(y)`, `x.saturating_sub(y)` | Same. |
| `cargo build` | `gos build` |
| `cargo test` | `gos test` |
| `cargo fmt` | `gos fmt` |

Entry files may omit `fn main`. Bare statements at file scope become an
implicit `fn main()`.

## Syntax At A Glance

Rust and Gossamer use similar delimiters, but not the same separators.
Gossamer accepts semicolons only between statements on the same line; trailing
semicolons are rejected. Commas are required inside a delimited list
written on one line; newlines are the canonical separators once the list is
multiline. Multiline commas remain accepted for migration, but `gos fmt`
removes them.

```rust
struct User {
    name: String,
    active: bool,
}
let user = User { name: "Ada".into(), active: true };
```

```gos
struct User {
    name: String
    active: bool
}

fn rename(
    user: User
    name: String
) -> User {
    User {
        name: name
        active: user.active
    }
}

enum Lookup {
    Found {
        index: i64
        user: User
    }
    Missing(String)
}

let user = User { name: "Ada", active: true } // one line needs commas
```

Named structs always use keyed braces. Tuple structs and tuple enum variants
use parentheses; named enum payloads use keyed braces. Collections and product
types have distinct access forms:

```rust
let first = &users[0];
let enabled = pair.1;
let cached = by_name.get("Ada"); // Option<&User>
```

<!-- fragment -->
```gos
let users = #[user, rename(user, "Grace")]
let first = users[0]              // Vec/array index; traps if out of bounds
let initial = first.name[0]       // String index is a char (a Unicode scalar)
let pair = (first.name, first.active)
let enabled = pair.1              // tuple field
let mut by_name: Map<String, User> = Map::new()
by_name.insert(first.name, first)
let cached = by_name.get("Ada")   // Map lookup returns Option<V>
let found = Lookup::Found {
    index: 0
    user: cached.unwrap()
}
```

## Collection Literals

Use `#[a, b]` for `Vec<T>`, `[a, b]` or `[value; N]` for fixed arrays,
`{key: value}` and `{}` for `Map<K, V>`, and `#{a, b}` for `Set<T>`. An
expected `BTreeSet<T>` type shapes the same set literal into an ordered set.
Queues, stacks, deques, and heaps are built through their type with `new()` or
`from(#[...])`. A Rust `vec![...]` is Gossamer's `#[a, b]`, and a Rust
`[a, b]` array keeps its spelling. Rust's `HashMap`, `HashSet`, `VecDeque`, and `BinaryHeap`
spellings are not accepted - each container has exactly one name.

```gos
let values = #[1, 2, 3]
let fixed = [1, 2, 3]
let map = {"ada": 36, "grace": 37}
let empty: Map<String, i64> = {}
let set = #{"parse", "lower", "parse"}
let ordered: BTreeSet<String> = #{"lower", "parse"}
let queue = Queue::from([1, 2, 3])
let stack = Stack::from([1, 2, 3])
let deque = Deque::from([1, 2, 3])
let max_heap = MaxHeap::from([1, 2, 3])
let min_heap = MinHeap::from([1, 2, 3])
let pair = (1, "two")
```

## Formatting And Interpolated Strings

Rust's `format!` family has no `!` here: `format`, `println`, `print`,
`eprintln`, and `panic` are ordinary calls with the same `{}` and `{:spec}`
placeholders. An `f"..."` string interpolates any expression, each with an
optional spec, so Rust's inline captures port directly and the positional
arguments move inline:

```rust
let line = format!("{name} is {age} years old");
println!("{:>8} {:.2} {:?}", user.name, ratio, tags);
```

<!-- fragment -->
```gos
let line = f"{name} is {age} years old"
println(f"{user.name:>8} {ratio:.2} {tags:?}")
```

A placeholder holds any expression, `f"{xs.len()} items"` and
`f"{a + b:>8}"` included; the first `:` outside the expression's brackets
starts the spec, and a `::` path is not one. `{{` and `}}` write braces, `f"""..."""`
interpolates a multiline string, and `{:+}` / `{:e}` format a sign and an
exponent as in Rust.

## Ownership And References

Gossamer does not expose Rust's ownership-by-move model or lifetime
syntax. Heap aggregates are runtime-managed, primitives copy by value,
and references are ordinary aliases.

```gos
let a = #[1, 2, 3]
let b = a
println("{} {}", a.len(), b.len())
```

`&mut` still means the callee may write through the reference, but the
compiler does not implement Rust's lifetime or non-lexical-borrow analysis.
As in Rust, a writable place must be passed explicitly as `&mut value`;
`function(value)` never creates a mutable reference. An existing `&mut T`
reference can be forwarded directly.
It rejects a second simple named `&mut` to the same root while the first is in
lexical scope, overlapping temporary mutable references, and duplicate mutable
roots in one call. More complex aliases remain a correctness hazard as they
are in a language with shared mutable objects.

## Traits

Traits are nominal and implemented explicitly:

```gos
trait Area {
    fn area(&self) -> f64
}

struct Circle { r: f64 }

impl Area for Circle {
    fn area(&self) -> f64 { 3.14159 * self.r * self.r }
}

fn total<T: Area>(xs: [T]) -> f64 {
    let mut out = 0.0
    for x in xs {
        out += x.area()
    }
    out
}
```

A trait-qualified call names the implementation explicitly, as in Rust:
`Area::area(circle)`. Operator traits take one impl per right-hand type,
so `impl Mul<f64> for Vec2` sits beside `impl Mul for Vec2`, each with an
optional `type Output`.

There is no `unsafe` in Gossamer source.

## Derives And Value Operations

The supported user derives are intentionally small. Use derives for
compiler-provided formatting, defaults, and ordering or equality when
the type needs those generated implementations.

```gos
#[derive(Debug, PartialEq, Eq)]
struct User {
    name: String
    age: i64
}
```

Do not port Rust derives mechanically. `Clone`, `Copy`, `Hash`,
`Serialize`, and `Deserialize` are automatic rather than derived, and
writing them reports GT0025. `#[derive(Default)]` needs every field to
have a default (GP0062). Typed serde is a pair of functions named by a
turbofish; `#[rename("key")]`, `#[skip]`, and `#[default]` play the part
of the `#[serde(..)]` field attributes, and enums are externally tagged:

```rust
#[derive(Serialize, Deserialize)]
struct User {
    name: String,
    #[serde(rename = "years")]
    age: i64,
}
let text = serde_json::to_string(&user)?;
let back: User = serde_json::from_str(&text)?;
```

<!-- fragment -->
```gos
use std::encoding::json

struct User {
    name: String
    #[rename("years")]
    age: i64
}
let text = json::to_json::<User>(user)?
let back = json::from_json::<User>(text)?
```

Aggregate values can be used directly in vectors and ordinary structs, and
any hashable value keys a `Map` or `Set`: integers, strings, tuples,
structs, and enums, compared by value. Floats are not hashable, as in Rust.

## Async Code

Rust:

```rust
let response = reqwest::get(url).await?;
```

Gossamer:

```gos
use std::{errors, http}

fn fetch(url: String) -> Result<String, errors::Error> {
    let response = http::get(url, #[])?
    Ok(response.body)
}
```

For fan-out, spawn goroutines and collect through channels:

<!-- fragment -->
```gos
let tx, rx = channel()

for url in urls {
    let tx = tx.clone()
    spawn(|| {
        tx.send(http::get(url, #[]))
    })
}

let mut responses = #[]
for _ in urls {
    responses.push(rx.recv().unwrap())
}
```

Blocking IO is acceptable. The runtime parks goroutines around blocking
operations where the standard library provides integration.

Outside `main`, a `spawn` sits inside a `cohort { }` in its own function
(GT0086). The block joins every child on every exit, like
`std::thread::scope`, and answers the first failure as its `Err`:

```gos
use std::{errors, http}

fn fetch_both(a: String, b: String) -> Result<(), errors::Error> {
    cohort {
        let first = spawn(|| http::get(a, #[]))
        let second = spawn(|| http::get(b, #[]))
        let _ = first.join()
        let _ = second.join()
    }
}
```

## Collections And Pipelines

Rust iterator chains port as written, without the `*` dereferences:
`iter()` answers a lazy iterator, adapters stay lazy, and a terminal ends
the chain. A type with `impl Iterator for T` gains the same adapters and
terminals, and `collect()` into `Result<Vec<T>, E>` or `Option<Vec<T>>`
stops at the first failure, as in Rust.

```rust
let total: i64 = xs.iter()
    .filter(|n| **n % 2 == 0)
    .map(|n| n * n)
    .sum();
let parsed: Result<Vec<i64>, _> = words.iter().map(|w| w.parse()).collect();
```

<!-- fragment -->
```gos
let total = xs.iter().filter(|n| n % 2 == 0).map(|n| n * n).sum()
let parsed: Option<Vec<i64>> = words.iter().map(|w| w.to_i64()).collect()
```

Free functions compose with the pipe operator, which passes the left-hand
value into a function: a bare callable takes it as its only argument, and a
closure names the slot it fills, so a data-first callee reads left to right.

<!-- fragment -->
```gos
use std::iter

let total = xs
    |> |v| iter::filter(v, |n: i64| n % 2 == 0)
    |> |v| iter::sum_by(v, |n: i64| n * n)
```

Mutating collection helpers such as `push`, `sort`, `insert`, and
`remove` stay as methods.

## Integer Overflow And Wrapping Arithmetic

Plain `+`, `-`, and `*` panic on overflow at the declared width on every
tier and in every profile - including `gos build --release`, where Rust's
default wraps. There is no counterpart to `overflow-checks = false`: where
wrapping is the intent - hashes, checksums, pseudo-random generators - say
so with an operator. The `wrapping_*` methods do not exist: a call reports
GT0087, and `gos check --fix` rewrites it to the operator.

| Rust | Gossamer |
| --- | --- |
| `a.wrapping_add(b)` | `a +% b` |
| `a.wrapping_sub(b)` | `a -% b` |
| `a.wrapping_mul(b)` | `a *% b` |
| `a = a.wrapping_mul(b)` | `a *%= b` (also `+%=`, `-%=`) |
| `Wrapping<u32>` | a plain `u32` combined with `+%`, `-%`, `*%` |
| `checked_*`, `overflowing_*`, `saturating_*` | Same names and answers. |

```rust
fn fnv1a(data: &[u8]) -> u32 {
    let mut hash: u32 = 2166136261;
    for &b in data {
        hash ^= b as u32;
        hash = hash.wrapping_mul(16777619);
    }
    hash
}
```

```gos
fn fnv1a(data: String) -> u32 {
    let mut hash: u32 = 2166136261
    for b in data.bytes() {
        hash ^= b as u32
        hash *%= 16777619
    }
    hash
}
```

The operators wrap at the operands' declared width on every tier and in
every profile, so `gos run` and a release binary produce the same hash.
They bind like the operators they wrap: `a *% b +% c` is `(a *% b) +% c`.
Unary `-`, `!`, `<<`, and a signed `MIN / -1` already wrap at the declared
width without a separate spelling.

## Visibility

Gossamer has three visibilities, and they are declared per item, per method,
and per struct field.

| Annotation | Reachable from |
| --- | --- |
| none | the declaring module and its descendants |
| `pub(package)` | every module of the declaring package |
| `pub` | anything that depends on the package |

A **package** is the unit of distribution: one `project.toml`, one project id.
A **module** is a directory under `src/`. A module nested inside another is a
**module descendant**, and visibility flows inward only: a descendant reaches
its ancestors' private items, never the reverse.

```gossamer
// src/money/mod.gos
pub struct Amount {
    pub currency: String,
    cents: i64,                     // private representation
}

impl Amount {
    pub fn new(currency: String, cents: i64) -> Amount {
        Amount { currency: currency, cents: cents }
    }
    pub fn cents(&self) -> i64 { self.cents }
    fn normalize(&self) -> i64 { self.cents }   // private helper
}

pub(package) fn round_trip(a: Amount) -> i64 { a.normalize() }
```

A `pub` type may keep private methods and private fields, so a struct with any
private field can only be built by the module that declares it. Importing does
not widen anything: a `use` is a spelling convenience, and visibility is
decided by where the name is used.

Coming from Rust this is nearly the same model. `pub(package)` is
Gossamer's `pub(crate)`, and it is the only restricted form: `pub(crate)`,
`pub(super)`, and `pub(in path)` are rejected with a diagnostic naming
`pub(package)`.

## Standard Library Map

| Rust | Gossamer |
| --- | --- |
| `std::fs::read_to_string(path)` | `fs::read_to_string(path)` |
| `std::fs::read(path)` | `fs::read(path)` |
| `std::fs::write(path, data)` | `fs::write(path, data)` |
| `std::env::args()` | `env::args()` |
| `std::env::var(name).ok()` | `env::var(name)` |
| `std::process::Command` | `process::run(program, args)` |
| `std::process::exit(code)` | `process::exit(code)` |
| `Path::join` | `path::join(base, part)` |
| `std::sync::Mutex` | `sync::Mutex` |
| `std::time::Duration::from_millis` | `time::Duration::from_millis` |
| `reqwest::blocking::get(url)` | `http::get(url, [])` |
| `serde_json::to_string(&v)` / `from_str` | `json::to_json::<T>(v)` / `json::from_json::<T>(text)` |
| `Regex::new("literal")` | `regex::compile("literal")`, checked while compiling |
| `Regex::new(pattern)?` for a run-time pattern | `regex::new(pattern)?` |
| `crc32fast::hash(bytes)` | `hash::crc32::checksum(bytes)`, a `u32` |
| `std::io::Error` from `std::fs` | `io::Error` from every `fs` function |
