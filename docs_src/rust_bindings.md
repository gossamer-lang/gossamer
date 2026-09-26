# Calling Rust

Gossamer has exactly one foreign-function surface: a **binding crate**
- an ordinary Rust library that depends on `gossamer-binding`, marks
the functions it wants to publish, and is named in the project's
`project.toml` under `[rust-bindings]`. The toolchain compiles it,
links it into the interpreter and into `gos build` binaries, and the
functions become `use`-able from `.gos` source like any other module.

There is no source-level `extern "C"` item form. Writing one reports
`GP0016`, and `extern` stays a reserved word.

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
toolchain that scaffolded it; the toolchain resolves it against its own
copy when it builds the binding, so nothing has to be fetched for it.

Call it from Gossamer. The module is the string in `#[gos_module]`,
and - like every other module - it has to be imported before a path
through it resolves:

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

The first run against a project builds a per-project runner with
Cargo, which takes a while; later runs reuse the cached build under
`$XDG_CACHE_HOME/gossamer/runners/`. Changing `[rust-bindings]`
re-fingerprints the runner and rebuilds it.

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
| `[u8]` | `Bytes` | Newtype over `Vec<u8>`; `b.0` is the vector. |
| `Vec<T>` | `Vec<T>` | Over any element the table already lists. |
| `Option<T>` | `Option<T>` | |
| `Result<T, E>` | `Result<T, E>` | See [Errors](#errors). |
| `Map<K, V>` | `HashMap<K, V>` | `<i64, i64>`, `<String, String>`, `<String, i64>`. |
| tuple | tuple | `(i64, i64)`, `(f64, f64)`, `(i64, String)`, `(String, i64)`, `(String, String)`, `(i64, String, bool)`. |
| a struct | `#[derive(GosStruct)]` struct | Crosses by value, field by field. |
| `DynValue` | `DynValue` | A value whose shape the data decides; see [Dynamic values](#dynamic-values). |
| a declared arm set | `Type::Variant(&[VariantArm..])` | Matched as the Gossamer enum spelling the same arms. |
| a closure | `PersistentCallback` / `BindingCallback` | Taken by a `cb_fn`; see [Callbacks](#callbacks). |

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

```gossamer
match native::parse_int("nope") {
    Ok(n) => println("{}", n),
    Err(e) => eprintln("{}", e),
}
```

`Result<T, String>` works too, when a plain message is all the caller
needs.

A **panic** inside a binding is caught at the boundary rather than
unwinding into Gossamer code: the thunk returns the output type's
default (a null pointer, a zero, an empty value). Return an `Err`
rather than panicking for anything a caller should handle.

## Long synchronous work

A binding call runs on the goroutine that made it, so a long blocking
call would hold a scheduler thread. `#[gos_blocking]` moves the body
onto the blocking pool:

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

```gossamer
use Counter

fn main() {
    let c = Counter::new()
    println("{}", Counter::bump(c, 5))    // 5
    println("{}", Counter::value(c))      // 5
}
```

Each type gets its own registry of `Mutex`-guarded values, so a handle
is safe to pass between goroutines.

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

A binding returning `DynValue` hands back exactly that value.

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

```gossamer
enum Reply { Integer(i64), Text(String), Nothing }

match conn::reply(id) {
    Reply::Integer(n) => println("int {}", n),
    Reply::Text(s) => println("text {}", s),
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

```gossamer
use events

let k = 3
println("{}", events::apply(|x| x * k, 14))
```

The closure is **call-scoped**: it is valid only until the binding
function returns. A compiled program releases it then, and a later
`invoke` reports an error instead of calling anything. A callback that
must outlive the call needs a different design - hand the binding a
channel, or have Gossamer poll.

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

Each entry is keyed by the Cargo crate name. Five source forms:

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

# A pre-built static archive, with the binding ABI it was built against.
vendored = { prebuilt = "vendor/libacme.a", abi = "2.0" }
```

`features` and `default-features` are accepted on the `path`, `git`,
and `version` forms and are passed straight to Cargo.

`gos tidy` leaves `[rust-bindings]` entries alone - they are reached
through Rust, not through a Gossamer `use`, so the import scan cannot
see them.

## Tiers and the ABI

Bindings are aimed at `gos build`, which links each binding crate into
the native binary, and are supported by `gos run` and `gos test`,
which build a per-project copy of `gos` with the bindings linked in.
The REPL does not load them: it notes a project's `[rust-bindings]`
at startup, and `gos run` or `gos build` is the way to call them.

Binding calls run on the bytecode VM, the Cranelift JIT, and LLVM AOT
(`gos build`, `gos build --release`).
Values that are pointer-shaped on both sides - `String`, `Vec<T>`, an
opaque handle - cross unchanged; `Bytes`, `Map<K, V>`, tuples,
`#[derive(GosStruct)]` structs, and `DynValue` (open or with declared arms)
are converted between the runtime's own shape and the wire shape at each
boundary crossing, so the same program prints the same thing on every tier.

`gossamer-binding` carries an ABI version (currently `2.0`) that the
runtime checks at load time, so a stale binding is reported rather
than silently corrupting memory. Minor bumps add wire shapes; a major
bump breaks compatibility, and a binding must be rebuilt against the
matching toolchain.

`crates/gossamer-binding/ABI_0_4.md` in the repository documents each
wire shape's layout, ownership, and reclamation rules - read it before
writing a `BindingAbi` impl of your own.

## Shipping a library that carries a binding

A published Gossamer package may declare `[rust-bindings]` like any
other project; the consumer's toolchain builds the Rust crate as part
of the consumer's runner. Keep the Rust crate inside the published
tree (a `path` entry), or point at a crates.io release, so a consumer
fetching the package gets a buildable source tree.

Worked examples live in `example-external-libraries/` in the
repository: a Gossamer-aware crate, a wrapper around a published crate
that is not, and a system-clipboard binding that wraps `arboard`.
