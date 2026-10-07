# `lang::trait`

Behaviour interface declaration.

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

A trait declares method signatures; a type provides them with `impl Trait
for Type`. A generic function bounds a parameter by a trait and calls its
methods (`fn report<T: Shape>(s: T)`) - see [generics](generics.md).

<!-- fragment -->
```gossamer
trait Area { fn area(&self) -> f64 }

impl Area for Shape {
    fn area(&self) -> f64 {
        match self {
            Shape::Circle(r) => 3.14159 * r * r,
            Shape::Rect { w, h } => w * h,
        }
    }
}
```

## Associated types

A trait declares `type Item` and each `impl` supplies one concrete type
for it. `Self::Item` and `T::Item` name that type in signatures and
bodies; the projection resolves before lowering, so every tier sees the
concrete type.

```gossamer
trait Holder {
    type Item
    fn get(&self) -> Self::Item
}

struct Label { text: String }

impl Holder for Label {
    type Item = String
    fn get(&self) -> Self::Item { self.text }
}

fn shout<T: Holder>(holder: T) -> T::Item { holder.get() }
```

A trait may give the associated type a default (`type Count = i64`),
which an impl inherits unless it restates it.

When several impls supply different types, pin the projection with an
equality constraint on the bound:

<!-- fragment -->
```gossamer
fn sum_of<T: Source<Item = i64>>(source: T) -> T::Item { source.take() + 1 }
```

Resolution order is: the equality constraint, the impl named by a
concrete base (`Label::Item`, or `Self::Item` inside an impl), the
trait's default, then the trait's single implementor. A supertrait's
associated items are reachable through the subtrait that inherits them.
An impl that omits a required associated item is rejected (`GT0059`), a
projection of an undeclared item is rejected (`GT0060`), and an ambiguous
projection is rejected with the constraint to write (`GT0061`).

A supertrait is part of its subtrait: a type that implements
`trait BufRead: Read` implements `Read` too, and an `impl BufRead for T`
without an `impl Read for T` is rejected (`GT0115`). A bound brings the
supertrait's methods with it, so a `T: BufRead` parameter answers `read`:

```gossamer
trait Read {
    fn read(&self) -> String
}

trait BufRead: Read {
    fn line(&self) -> String
}

struct Stdin {}

impl Read for Stdin {
    fn read(&self) -> String { "data" }
}

impl BufRead for Stdin {
    fn line(&self) -> String { "line" }
}

fn both<T: BufRead>(input: T) -> String {
    f"{input.read()} {input.line()}"
}

fn main() {
    println(both(Stdin {}))
}
```

A method that two of a parameter's traits declare is ambiguous (`GT0117`).

Out of scope: generic associated types (`type Item<T>`), associated types
on `dyn Trait` (Gossamer has no trait objects), and inferring a
projection across several candidate impls without a constraint.

## Associated constants

A trait declares `const MAX: i64`, optionally with a default; each impl
supplies a value. Read one as `Type::MAX`, `Self::MAX`, or `T::MAX`
through a bound.

```gossamer
trait Bounded {
    const MAX: i64
    const STEP: i64 = 5
    fn width(&self) -> i64
}

struct Gauge { span: i64 }

impl Bounded for Gauge {
    const MAX: i64 = 100
    fn width(&self) -> i64 { self.span + Self::MAX }
}

fn headroom<T: Bounded>(gauge: T) -> i64 { T::MAX - gauge.width() + T::STEP }
```

Each associated constant compiles to an ordinary constant, so its value
folds identically on the bytecode VM, the JIT, and the LLVM AOT tier.
`T::MAX` follows the same resolution order as a type projection: the
trait's default, else the trait's single implementor.

## Operator overloading

Implementing the matching trait makes an operator dispatch to its method
on a user struct, enum, or generic struct - on every tier (bytecode VM,
JIT, and LLVM AOT alike). The result type is the method's return type, so
a dot product (`Mul -> f64`) types correctly. Compound assignment
(`a += b`) routes through the same binary method.

| Operator | Trait | Method |
|---|---|---|
| `a + b` / `a - b` / `a * b` / `a / b` | `Add` / `Sub` / `Mul` / `Div` | `add` / `sub` / `mul` / `div` |
| `a % b` | `Rem` | `rem` |
| `-a` (unary) | `Neg` | `neg` |
| `a[i]` | `Index` | `index` |
| `a | b` / `a & b` / `a ^ b` | `BitOr` / `BitAnd` / `BitXor` | `bitor` / `bitand` / `bitxor` |
| `a << b` / `a >> b` | `Shl` / `Shr` | `shl` / `shr` |

```gossamer
struct V2 { x: f64, y: f64 }

impl Add for V2 { fn add(self, o: V2) -> V2 { V2 { x: self.x + o.x, y: self.y + o.y } } }
impl Mul for V2 { fn mul(self, o: V2) -> f64 { self.x * o.x + self.y * o.y } }
```

Applying an arithmetic operator to an ADT with no matching impl is a
compile error (`GT0003`). These are real impls, not derives - the operator
traits are **not** `#[derive]`-able.

## Conversions: `From` / `TryFrom`

A `from` impl powers both `B::from(x)` and `x.into()`; a `try_from` impl
powers `B::try_from(x)` and `x.try_into() -> Result<B, E>`. The `into` /
`try_into` target is inferred from the use site (`let B`, a `B` parameter
or return) and never from the receiver, so a bare `x.into()` that no use
site reaches has no target at all and is reported as `GT0066`:

```gossamer
struct Celsius { t: i64 }
struct Fahrenheit { t: i64 }

impl Fahrenheit {
    fn from(c: Celsius) -> Fahrenheit { Fahrenheit { t: c.t * 9 / 5 + 32 } }
}

let f: Fahrenheit = Celsius { t: 100 }.into()   // 212, via Fahrenheit::from
```

`?` also auto-converts a propagated `Err` through `errors::Error::from`,
so a `Result<_, String>` flows into a `Result<_, errors::Error>` function
with no explicit `map_err`.
