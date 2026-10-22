# Calling Rust

A **binding crate** brings Rust code into a Gossamer program: an ordinary
Rust library that depends on `gossamer-binding`, marks
the functions it wants to publish, and is named in the project's
`project.toml` under `[rust-bindings]`. `gos run` builds it once as an
optimized shared library and loads it, `gos build` links it into the
binary, and the functions become `use`-able from `.gos` source like any
other module.

C functions need no crate: declare them in an `unsafe extern "C"` block
([Foreign functions](language/unsafe_extern.md)); a project refuses them
with `ffi = false`. A `[rust-bindings]` entry is its own opt-in and the
`ffi` key does not affect it.

## Quick start

Scaffold a project and a binding crate beside it:

```sh
gos new example.com/greeter --path greeter
cd greeter
gos new example.com/greeter/native --path native --template binding
```

`--template binding` writes a ready-to-edit crate:

```
native/
├── Cargo.toml
└── src/
    └── lib.rs
```

```rust
// native/src/lib.rs
use gossamer_binding::{GosError, gos_module};

#[gos_module("native")]
mod bindings {
    use super::*;

    /// Greet the supplied name.
    pub fn greet(name: String) -> String {
        format!("hello, {name}")
    }

    /// Fallible example: parse an integer.
    pub fn parse_int(s: String) -> Result<i64, GosError> {
        Ok(s.parse::<i64>()?)
    }
}
```

`gos new` prints the manifest entry to add. The key is the **Cargo
package name**, which the template derives from the id's tail:

```toml
# project.toml
[project]
id      = "example.com/greeter"
version = "0.1.0"

[rust-bindings]
native-binding = { path = "native" }
```

The `gossamer-binding` dependency the template writes is pinned to the
toolchain that scaffolded it; the toolchain patches it to its own copy when
it builds the binding, so nothing has to be fetched for it.

Call it from Gossamer. The module is the string in `#[gos_module]`,
and - like every other module - it has to be imported before a path
through it resolves:

<!-- fragment -->
```gossamer
// src/main.gos
use native

fn main() {
    println("{}", native::greet("world"))
    println("{:?}", native::parse_int("41"))
}
```

```sh
gos run src/main.gos      # hello, world
                          # Ok(41)
gos build src/main.gos    # a native binary with the binding linked in
```

The first run builds the binding with Cargo, in release, into
`$XDG_CACHE_HOME/gossamer/bindings/`; every project that names the same
binding with the same features shares that build, and a `path` binding is
rebuilt when its sources change. Later runs start in a few milliseconds, and
the library loads only when the program first calls one of its items.

## Declaring the module

`#[gos_module("path")]` on a `mod { ... }` publishes every `pub fn`
inside it. The string is the Gossamer-side module path; use `::` for a
nested one (`#[gos_module("acme::layout")]`).

Two rules the block imposes:

- **Keep `use` imports outside the block.** The macro lifts the
  function bodies into a generated module that pulls in the
  surrounding scope with `use super::*`, so imports written *inside*
  the block do not travel with them. Everything else - helper `fn`s,
  types, consts - may stay inside.
- **`///` doc comments flow through** to the item's registered
  documentation.

A `register_module!` macro form also exists and is what the attribute
expands to. Prefer the attribute; reach for the macro only for a
nested path whose C-ABI symbol prefix you want to spell by hand.

## The type vocabulary

A binding function's parameters and return type must be shapes the
ABI knows. Anything else is a compile error in the binding crate
(`the trait bound ...: BindingAbi is not satisfied`).

| Gossamer | Rust in the binding | Notes |
|---|---|---|
| `()` | `()` | |
| `bool` | `bool` | |
| `i64` | `i64` | The default integer on both sides. |
| `f64` | `f64` | |
| `char` | `char` | |
| `String` | `String` | |
| `[u8]` / `Vec<u8>` | `Bytes` | Newtype over `Vec<u8>`; `b.0` is the vector. A parameter takes any `[u8]`; an answer is a `Vec<u8>` the caller owns. |
| `Vec<T>` | `Vec<T>` | Over any element in this table, nested as deep as needed. |
| `Option<T>` | `Option<T>` | Over any element, `Option<Option<T>>` included. |
| `Result<T, E>` | `Result<T, E>` | See [Errors](#errors). |
| `Map<K, V>` | `HashMap<K, V>` | `String` keys over any value; `<i64, i64>` and `<i64, String>` too. |
| tuple | tuple | Up to four elements inside another shape; on its own, `(i64, i64)`, `(f64, f64)`, `(i64, String)`, `(String, i64)`, `(String, String)`, `(i64, String, bool)`. |
| a struct | `#[derive(GosStruct)]` struct | Crosses by value, field by field, and inside any other shape. |
| `DynValue` | `DynValue` | A value whose shape the data decides; see [Dynamic values](#dynamic-values). |
| a declared arm set | `Type::Variant(&[VariantArm..])` | Matched as the Gossamer enum spelling the same arms. |
| a closure | `PersistentCallback` / `BindingCallback` | Taken by a `cb_fn`; see [Callbacks](#callbacks). |

A shape that nests composites (`Option<(i64, String)>`, `Vec<Bytes>`,
`Result<Vec<Probe>, String>`, `Vec<Option<i64>>`) crosses as JSON text that a
walk the compiler generates for its Gossamer type writes and reads, so it
answers the same on every tier. A top-level tuple must be one listed above;
put a richer one in a `Vec`, `Option`, or struct.

Shapes outside that set need a hand-written wrapper on the Rust side:
take the pieces the table covers and assemble the richer value inside
the binding. A binding crate may also add its own `BindingAbi` impl -
the macro discovers it through the trait - which is how a new
`HashMap` key/value pair or a wider tuple is added.

## Errors

A fallible binding returns `Result<T, E>`; the Gossamer caller sees
the same `Result` and can `?` it. `GosError` is the error type the
binding crate carries, and it converts from any `std::error::Error`,
so `?` works inside a binding body:

```rust
pub fn parse_int(s: String) -> Result<i64, GosError> {
    Ok(s.parse::<i64>()?)
}
```

<!-- fragment -->
```gossamer
match native::parse_int("nope") {
    Ok(n) => println(f"{n}"),
    Err(e) => eprintln(f"{e}"),
}
```

`Result<T, String>` works too, when a plain message is all the caller
needs; both error types carry the same shapes.

A **panic** inside a binding is caught at the boundary and raised as a
Gossamer panic with the binding's message, on every tier. Return an `Err`
rather than panicking for anything a caller should handle.

## Long synchronous work

A binding call runs on the goroutine that made it, so a long blocking
call would hold a scheduler thread. `#[gos_blocking]` marks the body as a
blocking call, and the scheduler runs other goroutines meanwhile:

```rust
use gossamer_binding::gos_blocking;

#[gos_blocking]
pub fn read_everything(path: String) -> Result<String, GosError> {
    Ok(std::fs::read_to_string(path)?)
}
```

## Opaque handles

State that should live in Rust - a connection, a parser, a device -
stays there behind a handle. `#[gos_opaque]` on an inherent `impl`
block publishes every `pub fn` in it:

```rust
use gossamer_binding::gos_opaque;

#[derive(Default)]
pub struct Counter {
    n: i64,
}

#[gos_opaque]
impl Counter {
    pub fn new() -> Self {
        Self { n: 0 }
    }
    pub fn bump(&mut self, by: i64) -> i64 {
        self.n += by;
        self.n
    }
    pub fn value(&self) -> i64 {
        self.n
    }
}
```

The type's name is the Gossamer-side module. A `Self`-returning
associated function registers a new value and answers its `i64`
handle; every method takes that handle as its first argument:

<!-- fragment -->
```gossamer
use Counter

fn main() {
    let c = Counter::new()
    println("{}", Counter::bump(c, 5))    // 5
    println("{}", Counter::value(c))      // 5
}
```

Each type gets its own registry of `Mutex`-guarded values, so a handle
is safe to pass between goroutines. A handle the registry does not hold is a
panic naming the method.

## Structs by value

A struct that should cross as a value derives `GosStruct`:

```rust
use gossamer_binding::GosStruct;

#[derive(Default, Clone, GosStruct)]
pub struct Point {
    pub x: i64,
    pub y: i64,
}

pub fn shift(p: Point, dx: i64) -> Point {
    Point { x: p.x + dx, y: p.y }
}
```

The program declares the struct it reads, with the same name and the Rust
struct's fields in order; the value crosses field by field into it. A binding
answering a struct the program does not declare is `GT0122`.

<!-- fragment -->
```gossamer
struct Point { x: i64, y: i64 }

let p = native::shift(Point { x: 1, y: 2 }, 3)
println(f"{p.x}")   // 4
```

## Dynamic values

A decoder, a database column typed by its own metadata, or an RPC reply has a
shape the *data* decides. `DynValue` is that value: `Nil | Bool | Int | Float |
Char | String | Bytes | List | Map | Tagged { name, payload }`, where a tagged
arm's name is a runtime string. It is a first-class Gossamer type - no import,
no mirror enum, and no Rust needed to build one:

```gossamer
let row = DynValue::tagged("Row", #[DynValue::int(9), DynValue::string("ok")])

println("{} {} {}", row.kind(), row.name(), row.len())   // tagged Row 2
if row.name() == "Row" {
    println("{:?} {:?}", row.at(0).as_i64(), row.at(1).as_str())
}
```

`kind()` answers `nil`, `bool`, `int`, `float`, `char`, `string`, `bytes`,
`list`, `map`, or `tagged`; `name()` is the arm's runtime name (empty for
every other kind); `len()`, `at(i)`, and `key_at(i)` read the values it holds;
`as_i64` / `as_f64` / `as_bool` / `as_char` / `as_str` answer `Option`, and
`as_bytes` a `Vec`. Two values are equal when their contents are, an arm
matching on its name and every payload field.

A binding returning `DynValue` hands back exactly that value. Its shape is
read through these accessors: matching it against an enum's patterns is
`GT0001`, and a binding that knows its arms declares them.

### Declaring the arms

A binding that knows its arm set declares it, and the program matches that set
as the ordinary enum spelling the same names:

```rust
const TYPE: Type = Type::Variant(&[
    VariantArm { name: "Integer", payload: &[Type::I64] },
    VariantArm { name: "Text", payload: &[Type::String] },
    VariantArm { name: "Nothing", payload: &[] },
]);
```

<!-- fragment -->
```gossamer
enum Reply { Integer(i64), Text(String), Nothing }

match conn::reply(id) {
    Reply::Integer(n) => println(f"int {n}"),
    Reply::Text(s) => println(f"text {s}"),
    Reply::Nothing => println("nothing"),
}
```

The arm table is an ABI input; the type the program matches on is the enum it
declared. The boundary selects that enum's discriminant from the wire's arm
name and fills the variant's fields, so the match reads the same arm on every
tier. An arm outside the declared set reports itself rather than becoming
another variant.

## Callbacks

A binding takes a Gossamer closure through a `cb_fn`, whose first
parameter is the dispatch it calls the closure through:

```rust
register_module!(
    name: events,
    doc: "Callback example.",

    cb_fn apply(d, f: PersistentCallback, x: i64) -> i64 {
        match f.invoke(d, vec![Value::Int(x)]) {
            Ok(Value::Int(n)) => n,
            _ => -1,
        }
    }
);
```

<!-- fragment -->
```gossamer
use events

let k = 3
println("{}", events::apply(|x| x * k, 14))
```

A `BindingCallback` is **call-scoped**: it is valid until the binding
function returns. A `PersistentCallback` holds a share of the closure for as
long as the binding keeps it, so a callable one call stores (a resize hook, an
event handler) can be called by a later `cb_fn` through that call's dispatch.
Under `gos run`, a function that calls a `cb_fn` runs on the VM, so a
callable from VM or JIT code can be called back.

The closure's parameters and result are integers, floats, `bool`,
`char`, and `String` (its result may also be `()`), and it takes at
most four parameters.

## Wrapping a crate that knows nothing about Gossamer

Most Rust crates are not Gossamer-aware, and they do not need to be.
Write a thin wrapper crate that depends on both:

```sh
gos add unic-segment --rust-binding
```

That records the crate under `[rust-bindings]` and scaffolds a wrapper
under `.gos-bindings/`, where you select the API surface to expose:

```rust
use gossamer_binding::gos_module;
use unic_segment::Graphemes;

#[gos_module("unic_segment")]
mod bindings {
    use super::*;

    /// Split a string into Unicode grapheme clusters.
    pub fn graphemes(s: String) -> Vec<String> {
        Graphemes::new(&s).map(str::to_string).collect()
    }
}
```

`gos bindgen path/to/lib.rs` goes one step further: it reads a Rust
source file, and for every `pub fn` whose signature already fits the
ABI vocabulary it emits a `#[gos_module]` item with a `todo()` body.
Functions it cannot express are listed as comments, so the gap is
visible rather than silent.

## The `[rust-bindings]` table

Each entry is keyed by the Cargo crate name. Four source forms:

```toml
[rust-bindings]
# A crate in this repository.
native  = { path = "native" }

# A crate from a git repository (branch / tag / rev select the checkout).
remote  = { git = "https://github.com/acme/gos-remote", tag = "v1.2.0" }

# A crate from crates.io.
segment = { version = "0.9" }

# A single Rust source file; `deps` is a verbatim Cargo dependency
# fragment for the crate the toolchain scaffolds around it.
tiny    = { src = "native/tiny.rs", deps = "unic-segment = \"0.9\"" }
```

Every form names source. A binding is compiled against the toolchain that
builds the program, so a pre-built archive has no form: its wire layout would
be whatever an older toolchain wrote, and nothing could check it.

`features` and `default-features` are accepted on the `path`, `git`,
and `version` forms and are passed straight to Cargo.

`gos tidy` leaves `[rust-bindings]` entries alone - they are reached
through Rust, not through a Gossamer `use`, so the import scan cannot
see them.

## Tiers and the ABI

`gos run`, `gos test`, `gos check`, and the REPL load a program's bindings,
its own and its dependencies', as one shared library over the C ABI in
`gossamer-binding-abi`; `gos build` links the same binding crates into the
native binary. Either way the bindings are one crate graph, so a Rust crate
two bindings depend on is one copy, with one copy of its global state.

Binding calls run on the bytecode VM, the Cranelift JIT, and LLVM AOT
(`gos build`, `gos build --release`).
Values that are pointer-shaped on both sides - `String`, `Vec<T>`, an
opaque handle - cross unchanged; `Bytes`, `Map<K, V>`, tuples,
`#[derive(GosStruct)]` structs, and `DynValue` (open or with declared arms)
are converted between the runtime's own shape and the wire shape at each
boundary crossing, so the same program prints the same thing on every tier.

### What stays compatible

A binding crate depends on `gossamer-binding` by version, and the toolchain
building it supplies its own copy of that crate through Cargo's `[patch]`.

- **The Rust source API is the contract.** `#[gos_module]`, `#[gos_opaque]`,
  `#[gos_blocking]`, `#[derive(GosStruct)]`, the `BindingAbi` trait, and the
  wire types it names (`Bytes`, `DynValue`, callbacks, `Map`) follow the
  [compatibility policy](compatibility.md): a minor release only adds to them,
  and a removal goes through a deprecation period. A binding that builds
  against one release builds against the next.
- **The binary interface is versioned.** A library records the binding ABI
  it was built for (currently `3.0`); `gos` refuses one built for another
  major version, and one whose signatures changed since they were cached,
  before any call.

`crates/gossamer-binding/ABI.md` in the repository documents the entry
points, the wire shapes, and their ownership rules - read it before writing a
`BindingAbi` impl of your own.

## Testing a binding

`gossamer_binding::testing::call("module::item", &[Value::Int(1)])` calls an
item through the entry `gos run` uses, so a binding crate's own `cargo test`
needs no Gossamer toolchain.

## Shipping a library that carries a binding

A published Gossamer package may declare `[rust-bindings]` like any
other project; the consumer's toolchain builds the Rust crate, once, into
its shared binding cache. Keep the Rust crate inside the published
tree (a `path` entry), or point at a crates.io release, so a consumer
fetching the package gets a buildable source tree.

`gos publish` builds the package's `path` bindings for the machine it runs on
and packs their library into the package under the first one's
`gos-prebuilt/<target>/`, covered by the package's signature. A program on the
same target whose bindings are exactly that set, with the same sources,
features, and binding ABI, loads the library on its first run with no Rust
toolchain installed; any other program builds the sources as before.

A root project with `ffi = false` under `[project]` refuses every binding,
its own and its dependencies', before anything builds or loads, as it refuses
`unsafe extern "C"` declarations.

Worked examples live in `example-external-libraries/` in the
repository: a Gossamer-aware crate, a wrapper around a published crate
that is not, and a system-clipboard binding that wraps `arboard`.
