# `lang::generics`

Type and const parameters on functions, impls, structs, and enums. A const parameter is a value inside the item that declares it (`N as i64`, `[0; N]`), taken from an array argument's length, a turbofish, an array field or payload, or the type the context expects.

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## Trait bounds and static dispatch

A generic function may bound a type parameter by a trait and call that
trait's methods on a parameter receiver:

```gossamer
trait Shape {
    fn name(&self) -> String
    fn area(&self) -> i64
}

fn report<T: Shape>(s: T) -> String {
    format("{}: {}", s.name(), s.area())
}
```

- Each call site instantiates the type parameters independently, so one
  generic function serves any number of concrete types in a program.
- The bound is enforced: passing a type with no matching `impl` is a
  compile error (`GT0017`).
- A method called on a bound parameter resolves to the trait method's
  declared return type.
- Every instantiation is monomorphised and the trait-method call lowers
  to the concrete impl symbol (`Square::name`), giving static dispatch
  that is bit-identical across the VM, Cranelift, and LLVM tiers.
- A bound may pin an associated type with an equality constraint
  (`T: Holder<Item = i64>`), and `T::Item` / `T::MAX` project the bound
  trait's associated type / constant - see
  [trait](trait.md#associated-types).

Supported today: type parameters with one or more bounds (`T: A + B`),
written in the parameter list or a `where` clause, with struct arguments
and inherent static dispatch. Not yet part of static dispatch: `dyn Trait`,
blanket impls, and supertrait method inheritance through a bound.

## Generic struct types

A struct may hold its type parameter by value, and methods on it use the
`impl<T>` form (each receiver type specialises the method, so `-> T`
returns the real instantiated type):

```gossamer
struct Wrapper<T> { value: T }

impl<T> Wrapper<T> {
    fn get(&self) -> T { self.value }
}

fn main() {
    let n = Wrapper { value: 42 }
    let s = Wrapper { value: "hi" }
    println("{} {}", n.get(), s.get())   // 42 hi
}
```

Each instantiation lays the field out by its concrete type (a
`Wrapper<Point>` stores a whole `Point` inline) and runs bit-identically
on every tier. Multiple type parameters (`Pair<A, B>`), nested generic
structs, and arrays of generic structs all work.

## Const generic parameters

A `const N: usize` parameter is a value inside the item that declares it. A
function reads it (`N as i64`), builds a fixed array from it (`[0; N]`), and
answers one (`-> [i64; N]`):

```gossamer
fn sum<const N: usize>(xs: [i64; N]) -> i64 {
    let mut acc = 0
    for x in xs { acc += x }
    acc
}

fn zeros<const N: usize>() -> [i64; N] {
    [0; N]
}

fn describe<const N: usize>(xs: [i64; N]) -> String {
    format("{} values summing to {}", N as i64, sum(xs))
}

fn main() {
    println("{} {}", sum([1, 2, 3]), sum([10, 20, 30, 40, 50]))  // 6 150
    println("{:?} {}", zeros::<4>(), describe([7, 8]))           // [0, 0, 0, 0] 2 values summing to 15
}
```

- A call supplies `N` from the length of an array argument whose type names
  it, or from a turbofish (`zeros::<4>()`). A call that gives it neither
  reports `GT0088`.
- A caller's own const parameter may supply a callee's:
  `fn outer<const M: usize>(xs: [i64; M]) -> i64 { sum(xs) }`.
- A function may take more than one const parameter
  (`<const N: usize, const M: usize>`), and each distinct set of values is its
  own specialisation.
- A function may be generic over a lane vector's lane count:
  `fn dot<const N: usize>(a: Simd<f64, N>, b: Simd<f64, N>) -> f64` (see
  [lane vectors](simd.md)).

## Const parameters on structs and enums

A struct, a tuple struct, or an enum may declare const parameters, and a field
or variant payload may name one as an array length. Methods in
`impl<const N: usize> Ring<N>` read `N` as a value, whether the call is written
`r.capacity()` or `Ring::capacity(r)`:

```gossamer
struct Ring<const N: usize> {
    items: [i64; N]
    head: i64
}

impl<const N: usize> Ring<N> {
    fn capacity(&self) -> i64 {
        N as i64
    }

    fn push(&mut self, value: i64) {
        self.items[self.head] = value
        self.head = (self.head + 1) % (N as i64)
    }
}

enum Grid<const N: usize> {
    Filled([i64; N])
    Empty
}

fn main() {
    let mut ring = Ring { items: [0; 3], head: 0 }
    for value in 1..6 { ring.push(value) }
    let blank: Grid<2> = Grid::Empty
    let full = Grid::Filled([7, 8])
    println("{:?} {}", ring.items, ring.capacity())              // [4, 5, 3] 3
    println("{}", matches(full, Grid::Filled(_)) && matches(blank, Grid::Empty))  // true
}
```

A method may also answer an array of the block's length
(`fn snapshot(&self) -> [i64; N]`), and the caller receives a fixed `[i64; k]`
for its receiver's `k`. An associated function with no receiver takes the
block's const parameters from the turbofish on the type it is called through:

```gossamer
struct Ring<const N: usize> {
    items: [i64; N]
}

impl<const N: usize> Ring<N> {
    fn snapshot(&self) -> [i64; N] {
        self.items
    }

    fn blank() -> [i64; N] {
        [0; N]
    }
}

fn main() {
    let r = Ring { items: [4, 2, 3] }
    println("{:?} {:?}", r.snapshot(), Ring::<2>::blank())  // [4, 2, 3] [0, 0]
}
```

`Ring::blank()` with no turbofish names no length and reports `GT0088`.

A struct literal takes each const argument from the length of the array field
that names it, from a caller's own const parameter it forwards, or from the
type the context expects (`let r: Ring<3> = ...`). A variant takes it from its
payload the same way, and a variant with no payload - `Grid::Empty` - takes it
from the type the context expects: an annotated `let`, a parameter, or a
return type. A literal or variant that gives it none reports `GT0088`.

Every instantiation runs bit-identically across the bytecode VM, the Cranelift
JIT, and the LLVM AOT tiers. See `examples/const_generics.gos` for a complete
program.
