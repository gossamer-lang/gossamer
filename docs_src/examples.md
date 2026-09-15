# Examples

The [`examples/`](https://github.com/gossamer-lang/gossamer/tree/main/examples)
directory ships a broad set of worked programs.

## A friendly taste

`examples/function_piping.gos` walks through the `|>` forward-pipe
operator and the F#-style combinator surface in `std::iter` /
`std::option` (SPEC §10.4 / §10.4a). `|>` straightens out nested
calls so the data flow reads left-to-right; the combinators take
the data value as the last positional parameter so each call
threads naturally. (The same combinators are also methods on any
iterator - `(1..=10).filter(|n| n % 2 == 0).sum()` - so the
pipe form below is a stylistic choice, not the only spelling.)

```gossamer
use std::iter
use std::option

fn double(x: i64) -> i64 { x * 2 }
fn add(a: i64, b: i64) -> i64 { a + b }
fn clamp(lo: i64, hi: i64, x: i64) -> i64 {
    if x < lo { lo } else if x > hi { hi } else { x }
}

fn main() {
    let n = 3 |> double |> |v| add(10, v) |> |v| clamp(0, 100, v)
    println("arithmetic: {n}")

    let total = iter::range_inclusive(1, 10)
        |> |v| iter::filter(v, |n: i64| n % 2 == 0)
        |> |v| iter::sum_by(v, |n: i64| n * n)
    println("sum of even squares: {total}")

    let xs = #[1, 3, 5, 9, 14, 21]
    let first_big = xs
        |> |v| iter::find(v, |n: i64| n > 10)
        |> |v| option::unwrap_or(v, -1)
    println("first > 10 (or -1): {first_big}")
}
```

## Running today

- **`hello_world.gos`** - one-liner that prints via `println`.
  Runs under `gos`.
- **`function_piping.gos`** - tour of the `|>` forward-pipe
  operator plus the `std::iter` / `std::option` combinator
  surface (`filter`, `sum_by`, `find`, `option::unwrap_or`, …).
  Runs under `gos` (bytecode VM + Cranelift JIT), `gos build`
  (LLVM `-O0`), and `gos build --release` (LLVM `-O3`); the
  tier_parity test confirms identical output across all three.
- **`semicolon_separators.gos`** - shows semicolons replacing newlines between
  statements on one line while keeping trailing terminators invalid.
- **`generic_struct.gos`** - three generic struct shapes: `Pair<A, B>`
  (two independent parameters), `SameType<T>` (one parameter shared by
  both fields, enabling field arithmetic), and `Triple<A, B, C>` (three
  parameters). Each construction site is a separate monomorphisation;
  parameters are inferred from the field values at the call site.
  Runs under `gos` and `gos build`.
- **`const_generics.gos`** - a `const N: usize` parameter as a value:
  `N as i64`, `[0; N]`, a turbofish (`zeros::<4>()`), a `Ring<const N: usize>`
  struct whose methods read `N`, and a `Grid<const N: usize>` enum whose payload
  names it. Identical output on every tier.
- **`wrapping_hash.gos`** - wrapping arithmetic with `+%`, `-%`, `*%` and
  their compound forms: a djb2 and an FNV-1a string hash over `u32`, a `u8`
  countdown past zero, and an `i32` total past its maximum. Identical output on
  every tier and in every build profile.
- **`simd_lanes.gos`** - lane vectors: `Simd::from_array`, `Simd::splat`,
  lane-wise arithmetic, a `dot` generic over its lane count, a `Mask` from
  `lanes_lt` feeding `select`, integer `abs` and `reduce_max`, wrapping lanes,
  and `Simd::load` / `store` over a `Vec`. Identical output on every tier.
- **`trait_bounds.gos`** - generic functions with trait bounds dispatched
  statically. One `report<T: Shape>(s: &T)` serves every type that
  implements `Shape`; the bound is enforced at compile time and each
  instantiation monomorphises to a direct call. Identical output under
  `gos`, the Cranelift JIT, and `gos build`.
- **`record_update.gos`** - functional record update
  (`Config { ..base, port: 443 }`): build a new struct from an existing
  one, overriding only the changed fields, with the base still usable
  afterward. Runs under `gos` and `gos build`.
- **`go_spawn.gos`** - goroutine fan-out with no channels.
  Every construct lowers through native codegen, so `gos build`
  produces a working binary.
- **`concurrency.gos`** - goroutines plus a `(Sender, Receiver)`
  channel, producer / consumer shape. Runs under `gos`
  (bytecode VM) and `gos build` (native), with channel operations
  lowered natively on every tier.
- **`line_count.gos`** - walks a directory via `fs::read_dir`,
  counts plain-text lines per file, fans out through a channel.
  Uses goroutines and `select`.
- **`web_server.gos`** - HTTP/1.1 echo server mirroring FastAPI's
  `/echo` handler. Accepts any method, returns method / path /
  query / body as JSON. Runs under `gos`; `curl
  http://localhost:8080/echo?name=jane` exercises it.

## More in the tree

The [`examples/`](https://github.com/gossamer-lang/gossamer/tree/main/examples)
directory ships many more worked programs - collections and data
structures, error handling, file and directory I/O, encoding
(JSON / YAML / TOML / base64 / hex), crypto hashing, regular
expressions, an HTTP client and server, compression, CLI argument
parsing, and a full multi-file project under `examples/projects/`.
Each one passes `gos check`; see `examples/README.md` for the index.

## Try it

```sh
gos run examples/hello_world.gos
gos run examples/function_piping.gos
gos run examples/web_server.gos &
curl 'http://localhost:8080/echo?name=jane'
```
