# Codegen ABI: generic monomorphisation

Gossamer's compiled tiers (`gos build` and `gos build --release`)
monomorphise every generic instantiation to a concrete function in
MIR, then lower it through LLVM; the in-process Cranelift JIT behind
`gos` does the same. Monomorphisation specialises each
instantiation on its concrete type, so inner-loop ops are typed - an
`i64` add is a single machine instruction, not a polymorphic dispatch.

This page documents what the ABI represents and the one type it
deliberately rejects.

## What works

A generic instantiation compiles end-to-end - and runs with identical
output across the bytecode VM, the Cranelift JIT, and the LLVM AOT
backend - for:

- All integer types up to 64 bits (`i8` … `i64`, `u8` … `u64`,
  `isize`, `usize`), `f32` / `f64`, `bool`, `char`.
- `&T` and `&mut T` references - pointers, 64 bits on every shipped
  platform - for any `T`.
- Heap-managed aggregate handles: `String`, `Vec<T>`,
  `Map<K, V>`, map-backed `BTreeMap<K, V>`, channel halves,
  `Mutex<T>`, `WaitGroup`, atomics. Each is a 64-bit handle to
  runtime-managed storage.
- **User structs, tuples, enums, and strings passed by value.** A
  generic function over a by-value struct compiles and runs:

  ```gossamer
  struct Point { x: i64, y: i64 }
  fn id<T>(v: T) -> T { v }

  fn main() {
      let p = id(Point { x: 1, y: 2 })
      println(f"{p.x} {p.y}")
  }
  ```

  So does a generic struct type that stores its parameter inline, with
  methods on it:

  ```gossamer
  struct Point { x: i64, y: i64 }
  struct Wrapper<T> { value: T }
  impl<T> Wrapper<T> { fn get(&self) -> T { self.value } }

  fn main() {
      let w = Wrapper { value: Point { x: 7, y: 9 } }
      let p = w.get()
      println(f"{p.x} {p.y}")
  }
  ```

  Each instantiation lays its fields out by the concrete type and
  specialises each method by receiver type, including recursive
  generics, multiple type parameters (`Pair<A, B>`), nested generic
  structs, and arrays of generic structs.

## What fails to compile

`i128` / `u128` are **rejected at type-check time** (`GT0014`): no tier
has a 128-bit runtime representation, and the bytecode VM would
otherwise run them at silent 64-bit width. Use `i64` / `u64`, or split
the value into two 64-bit halves.

This is the contract: if the program type-checks, it compiles and runs
with identical output on every tier. There is no codegen path that
accepts a 128-bit type and silently produces a wrong binary - you get a
hard compile error, never garbage output.

## How a struct parameter is laid out

For a by-value struct `T`, the monomorphiser records a
per-instantiation field-type table and propagates each field's layout
(size, alignment, offsets) from MIR through to the Cranelift / LLVM
lowering, so a `Wrapper<Point>` stores a whole `Point` inline and a
generic method's `-> T` return is the real concrete type rather than an
opaque pointer. The type checker brings an `impl<T>`'s generics into
scope for each method, so `-> T` records a rigid parameter that the
monomorphiser then specialises by receiver type.

Methods on a generic struct still require the explicit
`impl<T> Wrapper<T>` form, as in Rust.

## See also

- `gos explain GT0014` - the 128-bit rejection in full.
- The language spec (`SPEC.md` in the repository root) - generics and
  monomorphisation semantics.
