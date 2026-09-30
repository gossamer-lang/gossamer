# Syntax tour

Gossamer's surface is Rust with two simplifications:

- No explicit lifetime annotations. References have implicit lexical
  lifetimes ending at the closing brace.
- Semicolons may replace newlines only between statements on the same line.
  They are separators, not trailing terminators.

See [`SPEC.md`](https://github.com/gossamer-lang/gossamer/blob/main/SPEC.md)
for the full grammar and semantics.

Delimited lists use commas on one line and newlines across multiple lines.
This rule covers every delimited list: function arguments and parameters,
closure parameters, struct fields and literals, enum variants and payload
fields, tuples and tuple types, `Vec` / array / `Map` / `Set` literals, tuple,
slice, and struct patterns, generic parameters and arguments, and `use` lists.
Multiline trailing commas are accepted for migration and removed by `gos fmt`.
A newline separates elements only where a comma could, so a parenthesised
expression spanning lines stays that expression rather than becoming a
one-element tuple.

Statement separators follow the same layout-first principle:

```gossamer
let width = 6; let height = 7; println(width * height)
```

The semicolons above replace newlines. A semicolon before a newline, `}`, or
the end of the file is invalid, so `let width = 6;` remains an error.

## Comments

Two forms, no others:

- `// ...` - line comment to end of line.
- `/* ... */` - block comment. Does **not** nest.

There is no separate `///` or `//!` doc-comment syntax. A run
of `//` lines immediately above an item (no blank line
between) is its documentation; a run at the top of a file is
the module's. Tooling reads these by position.

## Items

```gossamer
const PI: f64 = 3.14159
static MAX: u32 = 1024

type Id = i64            // transparent type alias

struct Point { x: f64, y: f64 }
struct Pair { first: i64, second: i64 }

enum Shape {
    Circle(f64),
    Rect { w: f64, h: f64 },
}

trait Area {
    fn area(&self) -> f64
}

impl Area for Shape {
    fn area(&self) -> f64 {
        match self {
            Shape::Circle(r) => 3.14159 * r * r,
            Shape::Rect { w, h } => w * h,
        }
    }
}
```

A function that answers a value declares the type it answers with. A body
whose tail expression produces one through a signature with no `-> T`
reports the `GL0055` warning, since the caller reads the signature and takes
back a unit. A body with no tail expression answers a unit and declares nothing.

## Top-level statements

The entry file may skip the `fn main` wrapper. Bare statements at file
scope become the body of an implicit `fn main()`; items declared
alongside them are hoisted out as usual:

```gossamer
println("Hello World")
```

A `?` at the top level makes the implicit main return
`Result<(), errors::Error>`; set a process exit code with
`std::process::exit(n)`. See
[Top-level statements](language/top_level_statements.md) for the full
rules.

## Generic structs

A struct may carry one or more type parameters. The typechecker
infers each parameter from the field values at the construction
site - no turbofish annotation is needed:

```gossamer
struct Pair<A, B> { fst: A, snd: B }
struct Cell<T>    { value: T }

fn main() {
    // Parameters inferred: Pair<i64, String>
    let p = Pair { fst: 42, snd: "answer" }
    println(f"{p.fst} = {p.snd}")   // 42 = answer

    // Same struct, different instantiation: Pair<i64, i64>
    let nums = Pair { fst: 10, snd: 32 }
    println("{}", nums.fst + nums.snd)  // 42

    let c = Cell { value: 99 }
    println(f"{c.value}")              // 99
}
```

Field reads carry the per-instance concrete type. When two fields
share the same parameter (`Pair<i64, i64>`), arithmetic across
them typechecks directly - no extra annotation required.
Named struct literals use braces with keyed fields only. Tuple structs use
parenthesized construction.

Generic structs take multiple type parameters, and generic methods
work too: an `impl<T> Cell<T> { ... }` block specializes per
instantiation. Field access and methods run on all three tiers.

## Expressions

Everything is an expression. Blocks evaluate to their tail:

<!-- fragment -->
```gossamer
let max = if x > y { x } else { y }
let label = match status {
    200 => "ok"
    404 => "missing"
    _ => "other"
}
```

Match arms on separate lines do not require commas. Commas remain accepted,
and are required between expression-bodied arms written on the same line.

Integer range expressions are lazy `Iterator<T>` values, where explicitly
typed bounds preserve their integer type and otherwise default to `i64`. See
the [lazy iterator protocol](design/lazy_iterators.md) for ownership, adapters,
and terminal behavior. `lo..hi` excludes `hi`, while `lo..=hi` includes it. An
omitted lower bound starts at zero. An omitted upper bound is unbounded:
like Rust's `RangeFrom`, it panics when a step would pass the type's
maximum, on every tier and in every profile. The REPL prints open ranges without
realising them, such as `10..` or `..10`. Because `..=` is inclusive, it
always requires an upper bound; `10..=` is a parse error.

## Destructuring assignment

A tuple on the left of `=` writes each element to its own target. The
right-hand side is evaluated before the first write, so a swap needs no
temporary:

<!-- fragment -->
```gossamer
let mut a = 1
let mut b = 2
a, b = b, a
p.x, xs[1] = 5, 6
_, kept = 99, 42
x, y += 2, 3
```

Each target must be a writable place - a binding, a field, an index, a tuple
position, or a dereference - or `_` to discard the element. Targets nest, and
a compound operator pairs element-wise. Parentheses group a pattern only
where one sits beside others, so a parenthesised target list reports
`GP0042` with the rewrite.

## Forward pipe (`|>`)

The forward-pipe operator composes free functions, which have no
receiver to chain from. It is the functional-style tool: reach for it
when a value flows through two or more free-function transforms.
Anything with a receiver already chains, and the method chain is the
shorter spelling - prefer `s.trim().to_lowercase()` over any pipe form
of it. The two mix freely, and a method chain can feed a pipe.

`|>` is left-associative with very low precedence, so `a |> f |> g`
reads as `g(f(a))` with no parentheses needed.

A step is one of two shapes. A **bare callable** takes the piped value
as its only argument; a step that **writes arguments** is a **closure**,
whose parameter is the slot the value fills:

```gossamer
fn double(x: i64) -> i64 { x * 2 }
fn add(a: i64, b: i64) -> i64 { a + b }
fn clamp(lo: i64, hi: i64, x: i64) -> i64 {
    if x < lo { lo } else if x > hi { hi } else { x }
}

// Reads left-to-right instead of inside-out.
let n = 3 |> double |> |v| add(10, v) |> |v| clamp(0, 100, v)

// Equivalent nested form:
let same = clamp(0, 100, add(10, double(3)))
```

The parameter may sit anywhere the body reaches, so
`xs |> |v| f(a, v).len()` is `f(a, xs).len()`. The body needs no call at
all: `x |> |v| v * 2`. Piping into a method on an external receiver
works the same way: `x |> |v| recv.m(a, v)`.

### Why the slot is named

Every std free function takes its data first, so a step could in
principle assume the leading slot. It does not: a step may pipe into any
callee - a function this program declares, a package's, or a method on an
external receiver - and none of those owe the operator a convention.
Naming the slot means the reader sees which argument the value fills
without knowing anything about the callee:

```gossamer
use std::{iter, strings}

let parts = "a,b,c" |> |v| strings::split(v, ",")
let doubled = #[1, 2] |> |value| iter::map(value, |v| v * 2)
let bounded = 3 |> |v| clamp(0, 10, v)
```

An argument-taking step that is not a closure reports `GP0041`, whatever
the callee is - a formatting call included. `$`, which spelled
the slot in earlier releases, is no longer part of the language and
reports `GP0027` wherever it appears. `gos check --fix` rewrites an
argument-taking step into the closure it stands for, putting the
parameter in the trailing slot, so confirm that is the slot the call
needs.

## Callback shorthand

A callback that only calls one std function can be written without `|v|`.

A std free function named where a value is expected is the closure that
calls it, so `#[1.0, -2.0].map(math::abs)` means
`map(|v| math::abs(v))`. A std item with no fixed parameter list has no
such closure and reports `GT0015`; a macro is not a function at all, so
`fmt::format` reports `GR0018` and is written `format(..)` inside a
closure of your own.

Everything else is a written closure - a method call, a field read, an
index, and a tuple projection each spell out what they do:

```gossamer
struct Person { name: String }

let trimmed = #[" a ", " b "].map(|v| v.trim())
let firsts = #[(1, 2), (3, 4)].map(|t| t.0)
let people = #[Person { name: "ada" }].map(|p| p.name)
```

`$` is not a callback shorthand, and is not part of the language at all:
`xs.map($.abs)` reports `GP0027`.

## Pattern matching

- `_` - wildcard.
- `name` / `mut name` - bind.
- `Some(inner)` / `None` - variant destructure.
- `Point { x, y }` / `Point { x: a, y: b }` - struct destructure (and renamed).
- `(a, b)` - tuple destructure.
- `&value` / `&mut value` - shared or mutable reference destructure.
- `1..=5` / `1..5` - closed and exclusive range.
- `..=hi` / `..hi` / `lo..` - open-ended range (an open end covers up to
  the type maximum). `lo..=` is a parse error because `..=` requires an
  upper bound.
- `a | b` - or-pattern.
- `x @ 1..=3` - `@`-binding.
- `..` - rest.

Guards: `Some(n) if n > 0 => ...`

Range patterns are opaque to exhaustiveness, so a `_` arm is still
required. The struct, variant, tuple, and or-pattern forms also work in
irrefutable `let` bindings: `let Point { x, y } = p`, `let Shape::Pair(m,
n) = s`, `let (A(g, _) | B(g)) = v` (or-pattern alternatives must bind the
same names).

The left side of `=` is a pattern and the right side is an expression.
Consequently, `&mut place` on the right creates a mutable reference, while
`&mut pattern` on the left matches and removes a mutable-reference layer.
Reference mutability must match exactly. The inner binding receives an
independent value copy, including when the referent is an aggregate.

`mut name` is separate: it makes `name` reassignable. `let &mut value =
reference` does not make `value` reassignable, while `let &mut mut value =
reference` does. For a simple top-level copy, `let value = *reference` is often
clearer. Reference patterns are especially useful when nested, as in `let
(name, &mut count) = entry`.

## Conditions and let-chains

An `if` or `while` condition may chain clauses with `&&`, where each
clause is either `let PAT = expr` or a boolean. Earlier `let` bindings
are in scope for later clauses and the body:

<!-- fragment -->
```gossamer
if let Some(x) = a && let Some(y) = b && x > 0 {
    process(x + y)
}
while i < xs.len() && let n = xs[i] && n > 0 {
    sum += n
    i += 1
}
```

A `let` clause chain is `&&`-only: `||` cannot join `let` clauses
without parentheses.

## Loops

<!-- fragment -->
```gossamer
loop { ... break value }
while cond { ... }
for item in iter { ... }
```

`break value` returns a value from `loop`. `continue` jumps to
the top.

## Arrays, slices, Vec, and ranges

Gossamer follows Rust's sequence model:

- `[T; N]` is an owned fixed-size array. Use `[a, b]` for an explicit fixed
  literal.
- `[T]` is unsized and is ordinarily used as `&[T]` or `&mut [T]`.
- `Vec<T>` is the only owned growable sequence. `#[a, b]` creates a Vec, and
  `[a, b]` creates a fixed `[T; N]` array whose length is part of its type.
- The repeat form follows the same spelling rule: `[5; 5]` is a fixed array of
  five `5`s, and `#[6; 7]` is a Vec of seven `6`s.
- `{key: value}` creates a `Map<K, V>`, and `#{a, b}` creates a
  `Set<T>` unless an expected `BTreeSet<T>` type shapes it.
- Queues, stacks, deques, and heaps have no literal form. Build them through
  their type: `Queue::new()` / `Queue::from([a, b])`, and the same `new` /
  `from` pair on `Stack`, `Deque`, `MaxHeap`, and `MinHeap`.
- `(a, b, c)` creates a tuple, whose element types may differ.

See [Collection literals](collection_literals.md) for examples of Vec,
fixed-array, Map, Set, BTreeSet, Queue, Stack, Deque,
MaxHeap, and MinHeap
construction, and [Tuples](language/tuples.md) for the tuple surface.

`&[T; N]` and `&Vec<T>` coerce to `&[T]`; their mutable forms coerce to
`&mut [T]`. Arrays and slices support queries and non-resizing operations.
Mutable arrays and slices can reorder or replace existing elements with
`sort`, `reverse`, `swap`, and `fill`.
Only Vec supports `push`, `pop`, insertion, removal, truncation, extension,
reservation, and capacity queries. See [method support](method_support.md#vec)
for the exact surfaces.

A range is a lazy `Iterator<i64>` value: `(2..n)` is exclusive and `(1..=n)`
is inclusive. It can be iterated directly or stored and consumed later:

```gossamer
let first_three = 0..3
for i in first_three { println(i) }
```

The sequence combinators are methods on any Vec or range -
`filter`, `map`, `sum`, `count(pred)`, `any` / `all`, `find` /
`position`, `fold`, `min` / `max`, `take`, `step_by`, `join` - so a
query chains directly with no accumulator:

<!-- fragment -->
```gossamer
let odds_sq = (1..=9).filter(|n| n % 2 == 1).map(|n| n * n).sum()
let primes = (2..limit).filter(|k| sieve[k])
```

Range binds looser than arithmetic and tighter than `|>`, so
`i * i..n` reads `(i * i)..n`.

## Error handling

```gossamer
use std::{fs, io}

fn load(path: String) -> Result<String, io::Error> {
    let raw = fs::read_to_string(path)?
    Ok(raw)
}
```

`?` propagates the `Err` variant. Wrap with
`std::errors::wrap(err, "while loading config")` for context.

## Arenas

<!-- fragment -->
```gossamer
arena {
    let tree = build_tree(16)
    total += check(tree)
}
```

Everything allocated inside an `arena { }` block is bump-allocated
and freed wholesale when the block exits - on every exit path,
including early `return` and `?`. Allocation becomes a pointer bump;
reclamation is O(slabs) with no per-object teardown; small-enum nodes
drop their runtime header entirely (a two-pointer tree node is exactly
16 bytes). The contract: nothing allocated inside the block may be
referenced after it exits. See the
[memory model](memory.md#arenas-arena) for the full semantics.

## Concurrency

<!-- fragment -->
```gossamer
// Structured: the block owns what it starts, waits for all of it, and
// reports the first failure as its own `Result<(), errors::Error>`.
fn gather() -> Result<(), errors::Error> {
    cohort {
        let a = spawn(|| fetch("one"))
        let b = spawn(|| fetch("two"))
        println("{} {}", a.join()??, b.join()??)
    }
}

// A spawn outside any `cohort { }` block attaches to the implicit root
// cohort `main` runs inside; there is no detached form.
let tx, rx = channel::<i64>()
spawn(|| { tx.send(42) })
let n = rx.recv()

select {
    a = rx_a.recv() => handle_a(a),
    b = rx_b.recv() => handle_b(b),
    _ = time::after(Duration::from_millis(5000)).recv() => timeout(),
}
```

`cohort { }` is structured concurrency: every `spawn` inside it belongs
to the block, which cannot be left until each child has finished, and a
child's panic or `Err` becomes the block's error after its siblings are
cancelled. `main` runs inside an implicit root cohort, so a spawned
goroutine can never outlive the program and a failure nobody joined is
reported at exit rather than lost - and because that root is main's
alone, a `spawn` in any other function must be written lexically inside a
`cohort { }` in that same function body, or the checker reports `GT0086`.
Settings ride the header:
`cohort(policy: Policy::CollectAll)`, `cohort(timeout: 500)`, and
`cohort(isolation: Isolation::Thread)` for FFI or CPU-bound children that
need a thread of their own. See [cohorts](language/cohort.md).

`spawn(|| expr)` spawns a goroutine - a real stackful coroutine on the
M:N scheduler. Blocking primitives (channel ops, mutex contention,
`time::sleep`, network reads, filesystem syscalls) park the
goroutine, freeing the worker thread to run other goroutines.
Channels are typed: `channel()` / `channel(0)` is unbuffered,
`channel(n)` is bounded, and `channel::unbounded()` is the explicit
queue form. `select` multiplexes sends and receives; it is a keyword, so
no function, method, or field may be named `select`.

Scheduling uses watchdog-requested cooperative safepoints; there is no
asynchronous preemption. Park points and every function boundary yield, and the
bytecode VM also polls loop back-edges every 1,024 iterations. The compiled
back-ends leave back-edges un-polled, so a CPU-bound loop that calls nothing
holds its worker to completion in a `gos build` binary - give it a call on an
outer iteration to hand the worker back. The watchdog requests a yield and may
interrupt a blocking syscall with `SIGURG` or a Windows APC. See
[runtime design - Preemption](design/runtime.md#preemption).

## Closures and higher-order fns

Lambdas use `|param: T| body`; captures from the enclosing scope
work transparently (runtime-managed, no `move`).

Higher-order parameters distinguish two callable types:

| Type | Accepts | Representation |
|------|---------|----------------|
| `fn(args) -> ret` | raw pointer shape | raw code pointer |
| `Fn(args) -> ret` | bare items **and** capturing closures | env+code fat pointer |

```gossamer
fn apply(f: Fn(i64) -> i64, x: i64) -> i64 { f(x) }
fn add_one(y: i64) -> i64 { y + 1 }

fn main() {
    let scale = 10
    let scaled = |y: i64| scale * y    // captures `scale`
    println("{}", apply(scaled, 5))   // 50
    println("{}", apply(add_one, 41)) // 42 - bare fn coerces
}
```

The conversion at the call boundary is implicit. Single trait
variant - `FnMut` / `FnOnce` parse but lower to the same
`Fn(_)` shape (the borrow-style split Rust draws is unnecessary
with automatic memory management).

## Attributes

<!-- fragment -->
```gossamer
#[test]
fn add_adds() { ... }

#[bench]
fn bench_hot_path() { ... }

#[lint(allow(unused_variable))]
fn scratch() { let x = 1 }

#[cfg(test)]
mod tests { ... }

#[derive(Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
struct Point { x: i64, y: i64 }
```

`#[derive(...)]` accepts exactly `Debug`, `Default`, `PartialEq`, `Eq`,
`PartialOrd`, and `Ord`. Any other name (`Clone`, `Hash`, `Copy`,
`Display`, `Serialize`, ...) is rejected with `GT0025`: copying,
comparison, hashing, and serialization are automatic and need no derive.

A value's rendering is synthesized too, and an `impl` overrides it:

```gossamer
use std::fmt::Display

struct Tagged { id: i64 }

impl Display for Tagged {
    fn fmt(&self) -> String { f"#{self.id}" }
}
```

`Tagged` then renders as `#1` through `{}`, `format`, `to_string()`,
`join(sep)`, and wherever one sits inside a `Vec`, `Map`, tuple, `Option`, or
struct field. `impl Debug for T { fn fmt(&self) -> String }` is the same
override for `{:?}`.

`Display` and `Debug` are distinct contracts. Each declares one method that
answers a `String`, and both are written `fn fmt`; the `impl` header says
which contract a block supplies. A type implementing one keeps the
synthesized rendering on the other channel, so
`println("{:?}", Tagged { id: 1 })` shows `Tagged { id: 1 }`. Declaring the
rendering as `fn to_string` inside an `impl Display` block reports `GP0053`.

An `impl Trait for Type` block defines the items the trait declares and
nothing else. A `fn` outside that contract reports `GT0072`: it would
otherwise become an inherent method under a misleading header, reachable by
name but never through the trait. Write it in an inherent `impl Type { .. }`
block, or declare it in the trait. One trait reaches one type through one
block, so a second `impl` of the same pair - or an `impl Debug for T` over a
`#[derive(Debug)]` - reports `GT0073`.

A trait names behaviour, never a value's type. There is no `dyn`, so
`fn width(x: Display)` reports `GT0071`; bound a generic by the trait instead
(`fn width<T: Display>(x: T)`). An `impl` header naming a trait nothing
declares reports `GT0070`, which is what catches a misspelled trait name.

The built-in traits an `impl` supplies are `Display` and `Debug`, the
comparisons `PartialEq` / `Eq` (`fn eq`) and `PartialOrd` / `Ord` (`fn cmp`),
`Clone`, `Default`, `Iterator` (`fn next`), the conversions `From` and
`TryFrom`, and the operators `Add` `Sub` `Mul` `Div` `Rem` `Neg` `Not`
`BitAnd` `BitOr` `BitXor` `Shl` `Shr` `Index` / `IndexMut`. `%info <Trait>` in
the REPL answers any of them with the method to write.

Every other built-in name is the language's own and rejects an `impl` with
`GT0084`: hashing, copying, and release are automatic (`Hash`, `Copy`,
`Drop`, `Send`, `Sync`, `Sized`), and `Into` / `TryInto` / `IntoIterator` name
the direction opposite the one that is written - `x.into()` reads the `From`
impl on the target, and `for v in x` drives `Iterator::next`. The diagnostic
names what the language does and what to write in the block's place.

## Modules

<!-- fragment -->
```gossamer
use std::http
use std::http::{Handler, Request, Response}
use "example.org/other"
```

Standard library modules require an explicit import. The import binds the
module's final path segment, or the requested alias, into the file:

<!-- fragment -->
```gossamer
use std::encoding::json
use std::fs as filesystem

let value = json::parse(text)?
let bytes = filesystem::read(path)?
```

Writing `json::parse(text)` without importing `std::encoding::json` is an
unresolved-name error. Prelude types, variants, macros, and functions listed
on the [Prelude page](prelude.md) remain available without imports.

A project's module tree is file-based: `src/foo.gos` becomes
`mod foo`, `src/bar/mod.gos` becomes `mod bar`, nesting to any depth.
Local modules import exactly like the standard library - `use foo::item`,
or spell `foo::item` in full. A bare name that some module declares but
which is not in scope reports `GR0011` with the `use` line that fixes it.

## Numeric literals

Write bare literals by default. Inference picks the type from the
binding, the call site, or the return type; suffixes are reserved
for the rare standalone case with no contextual hint.

- `42` - plain int, inferred type. Defaults to `i64`.
- `42i32` / `42u64` - explicit width when context can't pin it.
- `0xff` / `0b1010` / `0o777` - bases.
- `1_000_000` - underscore separator.
- `1.0` - plain float, inferred type. Defaults to `f64`.
- `1.0f32` - explicit float width.

## String literals

- `"hello"` - ordinary double-quoted string. Spans multiple lines
  without extra syntax; embedded newlines are preserved.
- `"\n"` / `"\t"` / `"\\"` / `"\""` - standard escapes.
- `"""..."""` - triple-quoted string. The body starts on the line after
  the opening delimiter, and the indentation it shares with the closing
  `"""` is stripped from every line, so embedded HTML, SQL, or JSON sits
  at the indentation of the surrounding code. `gos fmt` moves the whole
  block with the line that opens it.
- `r"raw"` / `r#"with embedded "quotes""#` - raw strings.
- `b"bytes"` / `b'c'` - byte literals for binary protocols.

```gossamer
let page = """
<html>
    <body>
        <h1>Hello</h1>
    </body>
</html>
"""
```

`page` holds five lines with `<html>` at column zero and `<h1>` indented
eight spaces. Escapes work as they do in `"..."`, and they are decoded
after the indentation is removed, so `\t` in the body is a tab and a
line break is a line break. Only whitespace may follow the opening
`"""` on its line.

## Formatted output

Formatted output goes through six format macros. Each takes a
Rust-style format string with `{}` placeholders, plus named captures
`{ident}` for bindings in scope:

```gossamer
let name = "jane"
let age = 30
println("hello, {name}! you are {age} years old.")
let greeting = f"welcome, {name}"
```

A named capture may walk a field path - `{account.balance}`, tuple
index `{t.0}`, nested `{o.inner.hits}` - with specs applying to the
path (`{account.balance:>8}`).

| Macro | Effect |
|-------|--------|
| `format("…", a, b)` | Returns a `String`. |
| `println("…", a, b)` | Writes to stdout + newline. |
| `print("…", a, b)` | Writes to stdout, no newline. |
| `eprintln("…", a, b)` | Writes to stderr + newline. |
| `eprint("…", a, b)` | Writes to stderr, no newline. |
| `panic("…", a, b)` | Unwinds with the rendered message. |

Alongside the format calls, a fixed set of desugar calls -
`matches`, `todo`, `unimplemented`, `unreachable`, `dbg` - and the
build-time `regex::compile` / `sql::statement` / `codegen` are built in.
Every one of them is written as an ordinary call: the set of
compiler-known names is closed and recognised at the `(`, so a `!` after
any name is a parse error (`GP0049`), as is any other `name!(…)`
(`GP0001`). There is no user-defined macro system. Compile-time metaprogramming instead goes through `comptime` - a
`comptime { ... }` block or `comptime fn` call is evaluated at compile
time and folded to a constant on every tier; see
[Comptime](language/comptime.md).

Format specs follow Rust's `{:spec}` grammar - width, alignment,
fill, zero-pad, radix, and precision (`{:>8}`, `{:08x}`, `{:^6}`,
`{:.2}`).

For the single-`String` output shape, `+` concatenates without
adding a separator:

```gossamer
let name = "world"
let greeting = "hello, " + name
```
