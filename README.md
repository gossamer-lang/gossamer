# Gossamer

[![CI](https://github.com/gossamer-lang/gossamer/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/gossamer-lang/gossamer/actions/workflows/ci.yml)

[Homepage and docs](https://gossamer-lang.org/)
  
[Try it out](https://gossamer-lang.org/playground/)
  
[Take the tour](https://gossamer-lang.org/tour/)

## North Star Goals

* Trustworthy (Stable, Secure, Correct)

* Ergonomic (Concise, Expressive, Understandable)

* Performant (Fast, Efficient, Scalable)

### Central Thread

Tier parity: Interpreted or compiled logic behaves the same.

## Current Status

In heavy development. Core elements of the language may change or break.

My goal is a language stable enough for 1.0.0 (and beyond).

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for details on Github Issues, 
PRs, and the LLM policy.

## Motivations

Why build Gossamer? Why use it?

My inspirations tell the story:

I love the confidence that comes from Rust and F#: the feeling that if it
 compiles, it probably works. Algebraic data types, pattern matching, and 
 explicit error handling feel like a natural way to build correct and 
 maintainable software.

From Python, having a REPL open or being able to iterate quickly on a script 
without waiting for a compile step.

Go, meanwhile, is an incredible tool for building and shipping software. 
It feels fast, minimal, and frictionless. The extensive standard library
let's you be productive quickly.
 
Zig's approach to metaprogramming via comptime, C#'s top level statements,
Java's experimental approach to structured colorless concurrency all appeal.

Swift's approach to garbage collection with ARC strikes a powerful balance.
 
### A Single Language?

What if one language could combine all of those ideas?

What if I could iterate quickly in a REPL or script, then compile the exact
 same program into an optimized standalone binary with no code changes?

What if that language could perform Python like while interpreted, 
but closer to Go when compiled?

I built Gossamer because I wanted that language for myself.

My goal is for Gossamer to replace Go, Python, F#/C#, Kotlin/Java, and 
(some) Rust for most of my own projects and use cases.

## Features inspired by multiple languages:

| Feature                                         | Gossamer | Rust |  Go |  F# | Python | Elixir | Kotlin |
| ----------------------------------------------- | :------: | :--: | :-: | :-: | :----: | :----: | :----: |
| Strong static type system                       |    ✓     |   ✓  |  ✓  |  ✓  |        |        |    ✓   |
| Algebraic data types / discriminated unions     |    ✓     |   ✓  |     |  ✓  |        |        |        |
| Exhaustive pattern matching                     |    ✓     |   ✓  |     |  ✓  |        |    ✓   |    ✓   |
| Error handling via `?` with `Result` & `Option` |    ✓     |   ✓  |     |     |        |        |        |
| No `null` by default                            |    ✓     |   ✓  |     |  ✓  |        |    ✓   |    ✓   |
| Immutable by default                            |    ✓     |   ✓  |     |  ✓  |        |    ✓   |   ✓    |
| Reference mutability and escape checks          |    ✓     |   ✓  |     |     |        |        |        |
| Automatic memory management                     |    ✓     |      |  ✓  |  ✓  |    ✓   |    ✓   |    ✓   |
| Small portable binaries                         |    ✓     |   ✓  |  ✓  |     |        |    ✓   |        |
| Pipe operator (`\|>`)                           |    ✓     |      |     |  ✓  |        |    ✓   |        |
| Interpreted / scripting mode                    |    ✓     |      |     |  ✓  |    ✓   |    ✓   |    ✓   |
| Interactive REPL                                |    ✓     |      |     |  ✓  |    ✓   |    ✓   |    ✓   |
| Keyword arguments                               |    ✓     |      |     |  ✓  |    ✓   |   ✓    |    ✓   |
| Default argument values                         |    ✓     |      |     |  ✓  |    ✓   |   ✓    |    ✓   |
| Built in MCP server                             |    ✓     |      |  ✓  |     |        |        |        |
| Parallel collection adapters                    |    ✓     |   ✓  |     |  ✓  |        |        |    ✓   |

**Wrapping arithmetic operators**

`a +% b`, `a -% b`, and `a *% b` add, subtract, and multiply.

`+%=`, `-%=`, `*%=` for compounding.

**Not Transpiled**

Gossamer compiles directly to native, it does not transpile to Rust or Go.

**No Macros**

No user-defined macros. Metaprogramming is Zig-style `comptime`: code
runs during compilation and folds into the program, and a `for` loop
over `typeInfo::<T>()` reflection generates native per-field code.

**Gossamer is Extensible in Rust.**

Gossamer is built to extend simply via (synchronous) Rust.

## Features unique to Gossamer

Or at least - not a carbon copy by intent!

**Default colorless structured concurrency**

As of 0.51.0 - Gossamer supports and defaults to colorless yet structured concurrency.

**Parallel collection adapters**

As of 0.62.0, every eager collection walk has a parallel twin that spreads the work over
every core the machine has:

    let scaled  = xs.par_map(|v| v * 2.0)
    let kept    = xs.par_filter(|v| v > 0.0)
    let total   = xs.par_sum()
    let folded  = xs.par_reduce(0.0, |a, b| a + b)

No channel, no cohort, no handle, and no new syntax. The callback is a closure literal or a
named pure function, which is how the compiler knows running it on many workers at once is
sound; one that writes a container it captured is rejected rather than raced. Effectful work
still belongs in a `cohort { }` with `spawn`, and the diagnostic says so.

A reduction's tree is a function of the input length, never of the machine's worker count,
so a float `par_sum` answers the same bits everywhere and `par_reduce` works with a combine
that is associative but not commutative. `par_reduce` asks the caller for associativity and
nothing else.

**Tier Parity Across Interpreted/Compiled**

Bit-identical semantics across VM / JIT / AOT treated as a release gate.

The code you run in the REPL, as a script, as a debug binary, and as an
optimized release binary behaves the same as an explicit language goal.

**Memory: Automatic, Deterministic, with Checked Regions**
Gossamer's automatic memory management uses deterministic reference counting,
and features checked regions. No lifetime ceremony.

**Commas or Newlines**

For structs, enums, match branches, function arguments & parameters, 
use commas for single line, and newlines for multi-line. 
This gives a consistent and cleaner look.
(Optional - `gos fmt` will clean this up).

**Collection Literals**

Inspired by Clojure here (#{} for set):

| Collection | Empty | With Data |
|---|---|---|
| Fixed Array | [] | [1,2,3] |
| Vec | #[] | #[1,2,3] |
| Map | {} | {"one": 1, "two": 2, "three": 3} |
| Set | #{} | #{1,2,3} |
| Tuple | () | (1, "two", 3.0) |

**Distinct Types for Queue, Stack, MinHeap, MaxHeap**

Gossamer implements distinct types here instead of reusing existing structures.

Typically:

* MinHeap: MaxHeap with Reverse (or similar)
* Stack: Vec with specific method usage
* Queue: Deque with specific method usage

This enables having a stronger type contract as well as making it more convenient to write.

If I want to know an argument to a function only allows LIFO behavior - I'd use Stack over Vec.

If I want a MinHeap - I can create it and use push/pop without worrying about making
the number a negative, or using "Reverse" to wrap it.

Of course you can still use the other structures - but the recommended course of action
is to use the dedicated ones.

| Collection | Empty | With Data |
|---|---|---|
| MaxHeap | MaxHeap::new() | MaxHeap::from([1,2,3]) |
| MinHeap | MinHeap::new() | MinHeap::from([1,2,3]) |
| Queue | Queue::new() | Queue::from([1,2,3]) |
| Stack | Stack::new() | Stack::from([1,2,3]) |
| Deque | Deque::new() | Deque::from([1,2,3]) |

## Details

- Language spec: [`SPEC.md`](SPEC.md)
- Project style guide: [`GUIDELINES.md`](GUIDELINES.md)
- AI skill card: [`SKILL.md`](SKILL.md) - drop this file into a model's context to teach it how to write idiomatic Gossamer (also embedded in `gos skill-prompt`).
- Editor integrations: [`gossamer-lang/gossamer-editor-support`](https://github.com/gossamer-lang/gossamer-editor-support) (VSCode, Vim, Neovim, Helix, Emacs, Sublime, Zed, plus a tree-sitter grammar)

Source files use the `.gos` extension.

The CLI is `gos`. 

Manifests live in `project.toml`.

Pre-stable. `gos feature-status` distinguishes available Shipped surface from
compatibility-protected Stable surface. Until entries are explicitly promoted
to Stable, treat them as may-change-with-notice.

### Packages, Modules, and Visibility

A **package** is the unit of distribution: one `project.toml`, one project id,
the thing `gos add` pulls in. A **module** is a directory of source under
`src/` - `src/util/mod.gos` is module `util`. A module nested inside another is
a **module descendant**: `src/deep/nest/` is `deep::nest`, a descendant of
`deep`.

Visibility is defined against those three. An item with no annotation is
private to the module that declares it and to that module's descendants, as in
Rust. `pub(package)` widens it to every module of the declaring package and no
further. `pub` makes it part of the package's public API. Methods and struct
fields carry their own visibility, so a `pub` type can keep private helpers and
a private representation.

Full rules: [visibility](docs_src/language/visibility.md) and SPEC §6.3a.

### On Mutability and Ownership

The broad goal is be inspired by Rust, but not as strict.
Gossamer uses a conservative lexical borrow checker rather than Rust's
ownership and lifetime system. References have implicit lifetimes ending at
the closing brace, and safe Gossamer has no explicit lifetime annotations.
Bindings are immutable by default, and a function can mutate caller-owned data
only through a mutable reference.

## Parity and the REPL

Tier parity between interpreted Gossamer and compiled Gossamer is a primary
language goal.

This extends to the REPL as much as practical. Because top-level REPL bindings
share one persistent scope, a reference would otherwise protect its source for
the rest of the session. `%drop NAME` ends and removes that binding's lexical
lifetime while preserving completed mutations and later independent bindings.

## Gossamer's Syntax

For scripts and examples, the entry file may skip the `fn main` wrapper:
bare statements at file scope become the body of an implicit `fn main()`,
so this is a complete program:

```gossamer
println("Hello World")
```

A top-level `?` makes the implicit main return `Result<(),
errors::Error>`; set a process exit code with `std::process::exit(n)`.

Gossamer has a forward-pipe operator (`|>`) for composing free
functions, which have no receiver to chain from. `x |> f` is `f(x)`; a
step that writes arguments is a closure whose parameter is that slot, so
`x |> |v| f(a, v)` is `f(a, x)` and `x |> |v| g(v, a)` is `g(x, a)`. Anything
with a receiver already chains, and the method chain is the shorter
spelling - the two mix freely:

```gossamer
use std::{iter, strings}

fn double(x: i64) -> i64 { x * 2 }
fn add(a: i64, b: i64) -> i64 { a + b }
fn clamp(lo: i64, hi: i64, x: i64) -> i64 {
    if x < lo { lo } else if x > hi { hi } else { x }
}

fn main() {
    // 3 -> double -> add 10 -> clamp to [0, 100]
    let n = 3 |> double |> |v| add(10, v) |> |v| clamp(0, 100, v)
    println("arithmetic: {}", n)

    // A method chain is an ordinary operand, so it can feed a pipe.
    let words = "  Hello  World  ".to_lowercase()
        |> strings::split_whitespace
        |> iter::count

    println("words: {}", words)
}
```

Types define their own operators. `impl Add for T` gives `+` its
meaning, and the same shape covers `-`, `*`, `[]`, and the rest.
Structural `==` and `.clone()` are automatic - no derive needed - so a
custom operator is the part that is genuinely yours to write:

```gossamer
struct Vec2 { x: f64, y: f64 }

impl Add for Vec2 {
    fn add(self, o: Vec2) -> Vec2 { Vec2 { x: self.x + o.x, y: self.y + o.y } }
}

fn main() {
    let sum = Vec2 { x: 1.5, y: 2.0 } + Vec2 { x: 3.0, y: 4.0 }
    println("({}, {})", sum.x, sum.y)   // (4.5, 6)
    println("{}", sum == sum.clone())   // true
}
```

A goroutine + channel example:

```gossamer
use std::sync::channel

fn add(a: i64, b: i64) -> i64 { a + b }

fn main() {
    let tx, rx = channel::<i64>()
    spawn(|| { tx.send(40 |> |v| add(2, v)) })
    if let Some(answer) = rx.recv() {
        println("answer: {}", answer)
    }
}
```

Or spawn a goroutine and join its result - `Ok(value)`, or `Err(message)`
if it panicked:

```gossamer
fn add(a: i64, b: i64) -> i64 { a + b }

fn main() {
    let h = spawn(|| 40 |> |v| add(2, v))
    match h.join() {
        Ok(v) => println("answer: {}", v),
        Err(e) => println("worker failed: {}", e),
    }
}
```

A `cohort { }` owns the goroutines started inside it: the block cannot be
left until every one of them has finished, and a child's failure cancels
its siblings and becomes the block's `Result`.

```gossamer
use std::errors

fn fetch(name: String) -> Result<String, errors::Error> { Ok(name) }

fn gather() -> Result<(), errors::Error> {
    cohort {
        let a = spawn(|| fetch("one"))
        let b = spawn(|| fetch("two"))
        println("{} {}", a.join()??, b.join()??)
    }
}

fn main() {
    println("{:?}", gather())
}
```

`main` itself runs inside a cohort, so no goroutine outlives the program
and a spawned failure nobody joins is reported rather than lost. `spawn(|| expr)`
remains the detached form for work that should outlive its block.

## REPL meta commands

The REPL starts with `gos <version> REPL [<architecture>-<os>]` and uses the
`>>>` prompt. Use `%help` to list commands: `%info`/`%i` answers one public
symbol by name and shows its item help - `*` widens the name to a prefix
(`Set*`), a suffix (`*Set`), or a substring (`*Set*`) - `%bindings`/`%b`,
`%declarations`/`%d`,
and `%history`/`%h` inspect the session, `%reset`/`%r` clears it, and
`%quit`/`%q` exits. Tab completes the word at the cursor, including every
member a binding reaches after `.`. Up/down cycles history; Enter continues until braces close;
Ctrl-D also exits. Expression results print as plain values. Declaration and
binding confirmations are hidden unless the REPL is started with `-v`; listings
wrap to the terminal width.

## Toolchain commands

```sh
# Build the toolchain.
cargo build --workspace

# Create a new project.
./target/debug/gos new example.com/hello --path hello
cd hello

# Type-check, execute, build.
gos check src/main.gos
gos run src/main.gos
gos build src/main.gos

# Lint, format, test.
gos lint .
gos fmt src/main.gos
gos test src/main.gos

# Drop into the REPL.
gos
```

Sequence types follow Rust's model. `[T; N]` is an owned fixed-size array,
`[T]` is an unsized slice used behind `&` or `&mut`, and `Vec<T>` is the only
owned growable sequence. A bracket literal such as `[1, 2, 3]` creates a
`Vec` by default. Use `#[1, 2, 3]` when a fixed array is required explicitly,
or let an expected fixed type such as `[i64; 3]` shape a plain bracket literal.
Map literals use `{key: value}` and construct `Map` values. Set literals
use `#{value, ...}` and construct `Set` values, or `BTreeSet` values when
an expected `BTreeSet<T>` type is present.
References to arrays and Vec values coerce to slice references in the same
four shared and mutable forms as Rust. Arrays and slices expose the implemented
slice-method surface, while Vec additionally owns eager collection
combinators, resizing, and capacity operations. Mutable arrays and slices
support non-resizing mutation such as `sort`, `reverse`, `swap`, and `fill`.
`%i` shows these distinct type surfaces and `%e` filters them further by the
binding's writable capability.

## Foreign Function Interface (FFI)

Gossamer can call native (Rust) code through the `[rust-bindings]`
section of `project.toml`. A Rust crate that depends on
`gossamer-binding` marks the functions it publishes with
`#[gos_module]`, and the toolchain compiles and links it into the
produced binary (or the interpreter) - the bound functions are then
`use`-able from `.gos` source like any other module. `gos new ID
--template binding` scaffolds the crate:

```toml
# project.toml
[rust-bindings]
echo-binding = { path = "echo-binding" }
```

```rust
// echo-binding/src/lib.rs
use gossamer_binding::gos_module;

#[gos_module("echo")]
mod bindings {
    /// Shout the input.
    pub fn shout(s: String) -> String {
        s.to_uppercase()
    }
}
```

```gossamer
use echo::shout
fn main() { println("{}", shout("hello")) }
```

The boundary uses the typed `gossamer-binding` ABI (integers, floats,
strings, tuples, vectors, `Option` / `Result`, opaque handles, byte
buffers, callbacks); a panic inside a binding is caught and surfaced as
a `Result::Err`. There is no source-level `extern "C"` item form - the
`extern` keyword is reserved (`GP0016`) and `[rust-bindings]` is the
single FFI surface. The full instructions - the type vocabulary,
errors, opaque handles, blocking work, wrapping a crate that knows
nothing about Gossamer, and the tier rules - are in [Calling
Rust](https://gossamer-lang.org/docs/rust_bindings/).
See also [`SPEC.md` section 12](SPEC.md) and
[`example-external-libraries/`](example-external-libraries/) for
end-to-end examples (a Gossamer-aware crate, and a plain published
crate wrapped thinly).

## Supported Platforms

The runtime's stackful goroutines (corosensei) need a per-arch
context-switch implementation. The current support matrix:

The supported target contract is the executable matrix in
[`conformance/target_matrix.tsv`](conformance/target_matrix.tsv) and the
matching [supported-targets documentation](docs_src/supported_targets.md).
Tier 1 executes the bytecode VM, JIT-enabled VM, and LLVM AOT binaries on
native CI for Linux x86_64/aarch64, Apple Silicon macOS, and Windows x86_64.
Linux x86_64/aarch64 musl AOT output is Tier 2: it is built from supported
hosts, executed natively or under QEMU, and compared with the pure bytecode
VM. Intel macOS is artifact-only pending execution evidence; armv7, riscv64,
and wasm are not supported execution targets.

### LLVM

`gos build --release` shells out to `llc` / `opt` / `clang`, and prefers LLVM
22 - the major `rustc` bundles - so a developer's machine and CI put the same
optimiser behind the same program. A build made with another major is the same
program; what it is not is comparable, so the toolchain says which one
answered.

LLVM 18 and newer are supported. Most distributions package one of those;
LLVM 22 usually comes from [apt.llvm.org](https://apt.llvm.org) on Linux or
`brew install llvm@22` on macOS.

### Raspberry Pi

Raspberry Pi OS 64-bit (and any `aarch64` Linux) is first-class. Install
the `linux-aarch64` release, then `gos` works out of the box (the VM
and its in-process JIT are self-contained). To compile natively on the
Pi, also install system LLVM and a C compiler:

```sh
sudo apt-get install -y llvm clang
```

### Cross-compiling to a Raspberry Pi

Build a Pi binary from a Linux, macOS, or Windows desktop. The
musl-static target is the host-agnostic path (no target sysroot needed):

```sh
rustup target add aarch64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl -p gossamer-runtime
gos build --release --target aarch64-unknown-linux-musl app.gos
# copy the static binary to the Pi and run it - no runtime deps
```

For a glibc (dynamic) Pi binary, target `aarch64-unknown-linux-gnu`; on a
Linux host install `gcc-aarch64-linux-gnu`, and on macOS/Windows supply an
aarch64 glibc sysroot via `GOS_CROSS_SYSROOT`. See SPEC §11.4 for the full
contract.

## Editor Support

Support for various editors (VS Code, Neovim, etc) [here](https://github.com/gossamer-lang/gossamer-editor-support) - syntax and LSP support.
   
[Lite Anvil](https://github.com/danpozmanter/lite-anvil) supports Gossamer as a first class language (syntax & LSP).

## Status and Rough Roadmap

Examples run through the bytecode VM by default (with optional deferred JIT
tier-up) and compile in debug or release mode.

There are gaps to fill in the standard library, bugs and optimizations to find via real world usage.

This project is still early but starting to find its sea legs. Right now performance, resource usage, functionality, and productivity
all feel very promising. But do not trust this yet.

My main goals are:

* Making Gossamer reliable enough to run real production code, and trust.

* Optimizing Gossamer toward Go-grade performance and resource usage. Claims
  are limited to workloads recorded by the checked-in benchmark suite; broad
  language-level parity is a goal, not a current guarantee.

* Building a reliable standard library to reduce the need to reach for third party libraries (using Golang as the gold standard, with small changes that feel right).

* Writing some ecosystem libraries for key functionality (gRPC, Postgres, etc) that shouldn't be in the standard library, but are necessary for real work. (Very early).

* Ensuring the developer experience fits the broad goals I have for a language that can replace or reduce my use of Go, Rust, Python, and F#.

## Build

```sh
cargo build --workspace
./target/debug/gos --version
```

## License

Licensed under Apache-2.0. See [`LICENSE`](LICENSE).
