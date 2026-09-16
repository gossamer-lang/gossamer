# Migrating from F# to Gossamer

F# and Gossamer both favor immutable `let` bindings, pattern matching,
`Option<T>`, `Result<T, E>`, and left-to-right pipelines. The main
differences are syntax, pipe argument order, concurrency, and the
absence of F# metaprogramming features.

## Quick Map

| F# | Gossamer | Notes |
| --- | --- | --- |
| `let x = 5` | `let x = 5` | Same. |
| `let mutable x = 5` | `let mut x = 5` | Same meaning. |
| `let f x y = x + y` | `fn f(x: i64, y: i64) -> i64 { x + y }` | Functions are not curried. |
| `fun x -> x + 1` | `|x: i64| x + 1` | Closure syntax. |
| `if c then a else b` | `if c { a } else { b }` | Braces required. |
| `match x with | A -> ...` | `match x { A => ... }` | Exhaustive. |
| record | `struct` | Construct with braces. |
| discriminated union | `enum` | Tuple variants use parentheses. |
| `async { ... }` | `spawn(|| { ... })` | Goroutine. |
| `Task` result | channel receive or direct `Result` | Blocking calls are acceptable. |
| `printfn "%d" n` | `println("{n}")` | Format strings are Rust-like. |

## Gossamer 0.47 Syntax At A Glance

F# uses indentation and separates list elements with semicolons. Gossamer uses
semicolons only between same-line statements. Inside delimiters, commas are
required on one line and
newlines are canonical across multiple lines. Multiline commas remain accepted
for migration, but `gos fmt` removes them.

```fsharp
type User = {
    Name: string
    Active: bool
}
let user = { Name = "Ada"; Active = true }
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

Gossamer uses indexing for sequences, named fields for structs, numeric fields
for tuples, and `Option`-returning lookup for maps:

```fsharp
let first = users[0]
let enabled = snd pair
let cached = Map.tryFind "Ada" byName
```

```gos
let users = #[user, rename(user, "Grace")]
let first = users[0]              // Vec/array index; traps if out of bounds
let initial = first.name[0]       // String index is a UTF-8 byte as i64
let pair = (first.name, first.active)
let enabled = pair.1
let mut by_name: Map<String, User> = Map::new()
by_name.insert(first.name, first)
let cached = by_name.get("Ada")   // Map lookup returns Option<V>
let found = Lookup::Found {
    index: 0
    user: cached.unwrap()
}
```

Gossamer collection literals cover the common F# collection shapes:
`#[a, b]` for `Vec<T>`, `[a, b]` for a fixed array, `{key: value}` for
`Map<K, V>`, and `#{a, b}` for `Set<T>` or an expected `BTreeSet<T>`.
`Queue<i64>`, `Stack<i64>`, `Deque<i64>`, `MaxHeap<i64>`, and `MinHeap<i64>`
are built through their type with `new()` or `from([...])`.

## Pipe Operator

F# pipes into the next function's first argument, which works because
currying makes `List.filter f` a genuine one-argument function.
Gossamer has no currying, so a step that writes arguments is a closure
whose parameter is the slot the piped value fills - anywhere in the
argument list, not a fixed end.

```fsharp
[1; 2; 3; 4]
|> List.filter (fun n -> n % 2 = 0)
|> List.sumBy (fun n -> n * n)
```

```gos
use std::iter

[1, 2, 3, 4]
    |> |v| iter::filter(v, |n: i64| n % 2 == 0)
    |> |v| iter::sum_by(v, |n: i64| n * n)
```

Naming the slot is what lets one operator serve any callee, whatever
its parameters. Every stdlib free function takes its data first,
mirroring the method receiver, and a step reads the same when it pipes
into a function this program declares or a package's.

```gos
use std::strings

let parts = "a,b,c" |> |v| strings::split(v, ",")
```

The other half of the translation is that Gossamer has methods and F#
does not. Where F# must pipe, Gossamer usually chains - `s.trim()`,
`xs.iter().filter(f).sum()` - and the pipe is for the free functions
that have no receiver to chain from. A method chain can feed a pipe.

## Records To Structs

```fsharp
type Config = {
    Host: string
    Port: int
    Verbose: bool
}

let cfg = { Host = "localhost"; Port = 8080; Verbose = false }
let updated = { cfg with Port = 9090 }
```

```gos
struct Config {
    host: String
    port: i64
    verbose: bool
}

let cfg = Config { host: "localhost", port: 8080, verbose: false }
let updated = Config { port: 9090, ..cfg }
```

Named structs use braces with keyed fields:

```gos
struct Pair { left: i64, right: i64 }

let a = Pair { left: 1, right: 2 }
```

Parentheses are reserved for tuple structs and enum tuple variants.

## Discriminated Unions To Enums

```fsharp
type Tree =
    | Leaf
    | Node of int * Tree * Tree
```

```gos
enum Tree {
    Leaf
    Node(i64, Tree, Tree)
}

fn sum(t: Tree) -> i64 {
    match t {
        Tree::Leaf => 0
        Tree::Node(v, l, r) => v + sum(l) + sum(r)
    }
}
```

Recursive enum variants are runtime-managed. Add `T` only when it
makes a public type clearer.

## Option And Result

Both languages share the same vocabulary:

```fsharp
let parsed = input |> Option.bind tryParse |> Option.defaultValue 0
```

```gos
use std::option

let parsed = input
    |> |v| option::and_then(v, try_parse)
    |> |v| option::unwrap_or(v, 0)
```

For fallible work, `?` is usually clearer than a pipeline:

```gos
fn load(path: String) -> Result<Config, errors::Error> {
    let text = fs::read_to_string(path)?
    parse_config(text)
}
```

## Concurrency

Gossamer has stackful goroutines and channels instead of computation
expressions:

```gos
let wg = sync::WaitGroup::new()
let tx, rx = channel()

for url in urls {
    wg.add(1)
    let tx = tx.clone()
    spawn(|| {
        defer wg.done()
        tx.send(http::get(url, #[]))
    }()
}

spawn(|| {
    wg.wait()
    tx.close()
}()

while let Some(result) = rx.recv() {
    process(result)
}
```

## Traits

F# interfaces are object-oriented. Gossamer traits are nominal and
implemented explicitly:

```gos
trait Area {
    fn area(&self) -> f64
}

struct Circle { r: f64 }

impl Area for Circle {
    fn area(&self) -> f64 { 3.14159 * self.r * self.r }
}
```

Generic bounds use `T: Area`. For a closed set of cases, prefer an
`enum` and exhaustive `match`.

## Missing F# Features

Gossamer does not have computation expressions, active patterns, units
of measure, type providers, higher-kinded types, or curried functions.
Use closures for partial application:

```gos
fn add(x: i64, y: i64) -> i64 { x + y }
let add5 = |y: i64| add(5, y)
```

## Integer Overflow And Wrapping Arithmetic

F# arithmetic is unchecked by default and wraps on overflow, unless a scope
opens `Checked`. Gossamer's plain `+`, `-`, and `*` behave like `Checked`
under `gos run`, the JIT, and `gos build` - an overflow panics - and wrap
only under `gos build --release`. Code that relies on wrapping (hashes,
checksums, pseudo-random generators) ports to the wrapping operators, which
wrap at the declared width on every tier and in every profile.

| F# | Gossamer |
| --- | --- |
| `a + b`, `a - b`, `a * b` (unchecked) | `a +% b`, `a -% b`, `a *% b` |
| `Checked.(+)` / `open Checked` | `a + b` (panics on overflow outside release builds) |
| `hash <- hash * 16777619u` | `hash *%= 16777619` |
| `^^^`, `<<<`, `>>>` | `^`, `<<`, `>>` |
| `uint32 b`, `2166136261u` | `b as u32`, a `u32` binding |

```fsharp
let fnv1a (data: byte[]) =
    let mutable hash = 2166136261u
    for b in data do
        hash <- (hash ^^^ uint32 b) * 16777619u
    hash

let nextSeed (seed: int64) = seed * 6364136223846793005L + 1442695040888963407L
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

fn next_seed(seed: i64) -> i64 {
    seed *% 6364136223846793005 +% 1442695040888963407
}
```

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

Coming from F#: `pub` is `public`, no annotation is closer to
`private` (module-scoped, and visible to nested modules), and `pub(package)`
is the `internal` equivalent. Unlike F#, declaration order does not affect
visibility - a module's items are visible throughout it regardless of where
they appear.

## Standard Library Map

| F# / .NET | Gossamer |
| --- | --- |
| `System.IO.File.ReadAllText` | `fs::read_to_string(path)` |
| `System.IO.File.WriteAllText` | `fs::write(path, data)` |
| `Environment.GetEnvironmentVariable` | `env::var(name)` |
| `Environment.GetCommandLineArgs` | `env::args()` |
| `Console.WriteLine` | `println(...)` |
| `sprintf "%s %d" s n` | `format("{s} {n}")` |
| `List.map f xs` | `xs |> |v| iter::map(v, f)` |
| `List.filter f xs` | `xs |> |v| iter::filter(v, f)` |
| `List.fold f init xs` | `xs |> |v| iter::fold(v, init, f)` |
| `Map.find k m` | `m.get(k)` |
| `Set.contains x s` | `s.contains(x)` |
| `String.trim s` | `strings::trim(s)` |
| `int.Parse s` | `strconv::parse_i64(s)` |
| `Task.Run` | `spawn(|| { ... })` |
| `Thread.Sleep(ms)` | `time::sleep(ms)` |
| `HttpClient.GetAsync(url)` | `http::get(url, [])` |
| `Regex(pattern)` | `regex::compile(pattern)` |
