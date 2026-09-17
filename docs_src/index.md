# Gossamer

A fast-compiling language with Rust-flavoured syntax and a Go-shaped
runtime. Memory is automatic
and deterministic - a Swift-like model of reference counting (with
automatic cycle collection) plus `arena { }` regions, and no tracing
collector.

- Source on GitHub: [gossamer-lang/gossamer](https://github.com/gossamer-lang/gossamer)
- Language spec: [`SPEC.md`](https://github.com/gossamer-lang/gossamer/blob/main/SPEC.md)
- Project style guide: [`GUIDELINES.md`](https://github.com/gossamer-lang/gossamer/blob/main/GUIDELINES.md)
- Security policy: [`SECURITY.md`](https://github.com/gossamer-lang/gossamer/blob/main/SECURITY.md)
- AI skill card: [`SKILL.md`](https://github.com/gossamer-lang/gossamer/blob/main/SKILL.md) -
  drop it into a model's context to teach it idiomatic Gossamer
  (`gos skill-prompt` prints the same text)

Gossamer is pre-1.0.0, so the public API may still change before 1.0.

## Hello, Gossamer

```gossamer
fn main() {
    println("hello, world")
}
```

For scripts and examples, the entry file may skip the `fn main`
wrapper: bare statements at file scope become the body of an implicit
`fn main()`, so this is a complete program too:

```gossamer
println("hello, world")
```

See [Top-level statements](language/top_level_statements.md).

## Hello, Goroutines and Channels

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

## A Script, No Boilerplate

No `fn main`, no ceremony - the file's top-level statements *are* the
program. A collection traverses the values it already holds, so a
data-processing script reads top to bottom:

```gossamer
let nums = [4, 8, 15, 16, 23, 42]
println("sum of evens: {}", nums.filter(|n| n % 2 == 0).sum())
```

See [Top-level statements](language/top_level_statements.md).

## Why Gossamer

- **Ergonomic** - Forward pipes, Rust-like error handling, minimal magic.
- **Efficient** - a small memory footprint, near-instant startup on the
bytecode VM for a quick edit-run cycle, and native compiled execution
when you ship a binary.
- **Interpreted and Compiled** - Develop code quickly with a bytecode vm powered
interpreter and a REPL. Ship an optimized compiled single binary.
- **Portable** - Tier 1 execution on Linux (x86_64 and aarch64, including
Raspberry Pi OS 64-bit), Apple Silicon macOS, and Windows x86_64. Linux-musl
AOT deployment is Tier 2 with QEMU-backed VM differential evidence. See the
[supported target matrix](supported_targets.md) for artifact-only and
unsupported registered triples.
- **Go-style goroutines** - (`spawn(|| expr)`) with typed channels.
- **Go-style async** - Colorless functions and stackful coroutines.
- **Rust-style type system** - statically-typed, generics with
  trait bounds, pattern-matching, `Option<T>` / `Result<T, E>`.
- **Swift-like memory model** - deterministic reference counting
(closely modeled on Swift's ARC, with automatic cycle collection
added) plus `arena { }` regions reclaim values as the last reference
dies. No lifetimes, no borrow-checker surface, no tracing collector.
- **Extensible in Rust** - Write libraries in safe Rust.

## Where to go next

- [Install](install.md) - build from source today, prebuilt
  binaries coming with the 1.0.0 release.
- [Running](running.md) - `gos` cheat-sheet.
- [Syntax](syntax.md) - grammar tour with worked examples.
- [Collection literals](collection_literals.md) - create Vecs, fixed arrays,
  Maps, Sets, and BTreeSets.
- [Memory model](memory.md) - how values, references, and
  automatic memory management fit together.
- [Writing libraries](libraries.md) - `project.toml`, module
  layout, publishing.
- [Calling Rust](rust_bindings.md) - binding crates, the type
  vocabulary, opaque handles.
- [Standard library](stdlib.md) - module index.
- [Prelude](prelude.md) - everything available without imports.
- [Toolchain](toolchain.md) - every subcommand.

Coming from another language? Start with the migration guide for
[Rust](migration/rust.md), [Go](migration/go.md),
[Python](migration/python.md), [Kotlin](migration/kotlin.md), or
[F#](migration/fsharp.md) - each maps what you already know onto
Gossamer.
