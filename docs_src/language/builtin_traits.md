# Built-in traits

Traits the language knows by name: an `impl` header or a generic bound may name one without declaring it. Source is `crates/gossamer-types/src/builtin_traits.rs`; this page is regenerated from `BUILTIN_TRAITS` by `gos doc --emit-stdlib`.

| Trait | Kind | An `impl` writes |
|---|---|---|
| [`Display`](#display) | overridable | `{ fn fmt(&self) -> String }` |
| [`Debug`](#debug) | overridable | `{ fn fmt(&self) -> String }` |
| [`PartialEq`](#partialeq) | overridable | `{ fn eq(&self, other: Self) -> bool }` |
| [`Eq`](#eq) | overridable | `{ fn eq(&self, other: Self) -> bool }` |
| [`PartialOrd`](#partialord) | overridable | `{ fn cmp(&self, other: Self) -> i64 }` |
| [`Ord`](#ord) | overridable | `{ fn cmp(&self, other: Self) -> i64 }` |
| [`Clone`](#clone) | overridable | `{ fn clone(&self) -> Self }` |
| [`Default`](#default) | overridable | `{ fn default() -> Self }` |
| [`Iterator`](#iterator) | overridable | `{ fn next(&mut self) -> Option<T> }` |
| [`From`](#from) | overridable | `{ fn from(value: T) -> Self }` |
| [`TryFrom`](#tryfrom) | overridable | `{ fn try_from(value: T) -> Result<Self, E> }` |
| [`Add`](#add) | operator | `{ fn add(&self, other: Self) -> Self }` |
| [`Sub`](#sub) | operator | `{ fn sub(&self, other: Self) -> Self }` |
| [`Mul`](#mul) | operator | `{ fn mul(&self, other: Self) -> Self }` |
| [`Div`](#div) | operator | `{ fn div(&self, other: Self) -> Self }` |
| [`Rem`](#rem) | operator | `{ fn rem(&self, other: Self) -> Self }` |
| [`Neg`](#neg) | operator | `{ fn neg(&self) -> Self }` |
| [`Not`](#not) | operator | `{ fn not(&self) -> Self }` |
| [`BitAnd`](#bitand) | operator | `{ fn bitand(&self, other: Self) -> Self }` |
| [`BitOr`](#bitor) | operator | `{ fn bitor(&self, other: Self) -> Self }` |
| [`BitXor`](#bitxor) | operator | `{ fn bitxor(&self, other: Self) -> Self }` |
| [`Shl`](#shl) | operator | `{ fn shl(&self, other: Self) -> Self }` |
| [`Shr`](#shr) | operator | `{ fn shr(&self, other: Self) -> Self }` |
| [`Index`](#index) | operator | `{ fn index(&self, index: I) -> T }` |
| [`IndexMut`](#indexmut) | operator | `{ fn index(&self, index: I) -> T }` |
| [`Hash`](#hash) | automatic | nothing - the language supplies it |
| [`Hashable`](#hashable) | automatic | nothing - the language supplies it |
| [`Copy`](#copy) | automatic | nothing - the language supplies it |
| [`Sized`](#sized) | automatic | nothing - the language supplies it |
| [`Send`](#send) | automatic | nothing - the language supplies it |
| [`Sync`](#sync) | automatic | nothing - the language supplies it |
| [`Drop`](#drop) | automatic | nothing - the language supplies it |
| [`Into`](#into) | automatic | nothing - the language supplies it |
| [`TryInto`](#tryinto) | automatic | nothing - the language supplies it |
| [`IntoIterator`](#intoiterator) | automatic | nothing - the language supplies it |
| [`FromIterator`](#fromiterator) | automatic | nothing - the language supplies it |
| [`AsRef`](#asref) | automatic | nothing - the language supplies it |
| [`AsMut`](#asmut) | automatic | nothing - the language supplies it |
| [`Read`](#read) | automatic | nothing - the language supplies it |
| [`Write`](#write) | automatic | nothing - the language supplies it |
| [`Error`](#error) | automatic | nothing - the language supplies it |
| [`Future`](#future) | automatic | nothing - the language supplies it |
| [`Fn`](#fn) | automatic | nothing - the language supplies it |
| [`FnMut`](#fnmut) | automatic | nothing - the language supplies it |
| [`FnOnce`](#fnonce) | automatic | nothing - the language supplies it |

## Overridable

Every type already has this behaviour; an `impl` replaces the synthesized one. `#[derive(..)]` asks for the synthesized behaviour explicitly where a type needs it named.

### `Display`

Declared by `std::fmt`.

How a value renders through `{}`. Every type renders without one; an `impl` replaces that rendering everywhere the value is shown, including inside a `Vec`, `Map`, tuple, `Option`, or struct field, and `x.to_string()` reaches it.

```gossamer
impl Display for Type { fn fmt(&self) -> String }
```

```gossamer
impl Display for Point { fn fmt(&self) -> String { format("({}, {})", self.x, self.y) } }
```

A bound naming `Display` licenses `fmt`, `to_string` on a type parameter.

### `Debug`

Declared by `std::fmt`.

How a value renders through `{:?}`. Independent of `Display`: a type that implements one keeps the synthesized rendering on the other channel.

```gossamer
impl Debug for Type { fn fmt(&self) -> String }
```

```gossamer
impl Debug for Point { fn fmt(&self) -> String { format("Point[{}]", self.x) } }
```

A bound naming `Debug` licenses `fmt`, `to_string` on a type parameter.

### `PartialEq`

What `==` and `!=` answer. Structs, enums, tuples, and sequences compare field by field with no `impl`; one written here replaces that comparison.

```gossamer
impl PartialEq for Type { fn eq(&self, other: Self) -> bool }
```

```gossamer
impl PartialEq for Point { fn eq(&self, other: Point) -> bool { self.x == other.x } }
```

A bound naming `PartialEq` licenses `eq`, `ne` on a type parameter.

### `Eq`

The `PartialEq` contract under its total-equality spelling; both names reach the same `eq`. Usable as a bound where a key or an element has to compare.

```gossamer
impl Eq for Type { fn eq(&self, other: Self) -> bool }
```

```gossamer
fn first_of<T: Eq>(xs: Vec<T>, needle: T) -> Option<i64> { xs.position(|v| v == needle) }
```

A bound naming `Eq` licenses `eq`, `ne` on a type parameter.

### `PartialOrd`

What `<`, `<=`, `>`, and `>=` answer: negative when the receiver orders first, zero when the two tie, positive otherwise. Values compare lexicographically by declaration order with no `impl`.

```gossamer
impl PartialOrd for Type { fn cmp(&self, other: Self) -> i64 }
```

```gossamer
impl PartialOrd for Point { fn cmp(&self, other: Point) -> i64 { self.x - other.x } }
```

A bound naming `PartialOrd` licenses `cmp`, `partial_cmp` on a type parameter.

### `Ord`

The `PartialOrd` contract under its total-order spelling; both names reach the same `cmp`, and a sequence's `sort`, `min`, `max`, and sorted-sequence searches all read it, and a `BTreeSet` or `BTreeMap` seats its entries by it. A heap orders as it stores, with no comparator to call, so it declines such an element (GT0085).

```gossamer
impl Ord for Type { fn cmp(&self, other: Self) -> i64 }
```

```gossamer
impl Ord for Point { fn cmp(&self, other: Point) -> i64 { self.x - other.x } }
```

A bound naming `Ord` licenses `cmp`, `partial_cmp` on a type parameter.

### `Clone`

What `x.clone()` answers. Every value already clones field by field; an `impl` replaces that copy for the type.

```gossamer
impl Clone for Type { fn clone(&self) -> Self }
```

```gossamer
impl Clone for Point { fn clone(&self) -> Point { Point { x: self.x, y: self.y } } }
```

A bound naming `Clone` licenses `clone` on a type parameter.

### `Default`

The value `T::default()` answers. `#[derive(Default)]` synthesizes one from the fields' own defaults, with `#[default]` picking an enum's variant; an `impl` writes it directly.

```gossamer
impl Default for Type { fn default() -> Self }
```

```gossamer
impl Default for Point { fn default() -> Point { Point { x: 0, y: 0 } } }
```

A bound naming `Default` licenses `default` on a type parameter.

### `Iterator`

Makes a type walkable: `for v in value` drives `next` until it answers `None`. Any type with that method works in a `for`, and a bound naming `Iterator` licenses the adapter surface.

```gossamer
impl Iterator for Type { fn next(&mut self) -> Option<T> }
```

```gossamer
impl Iterator for Countdown { fn next(&mut self) -> Option<i64> { if self.n == 0 { None } else { self.n -= 1; Some(self.n) } } }
```

A bound naming `Iterator` licenses `next`, `take`, `skip`, `step_by`, `enumerate`, `chain`, `zip`, `map`, `filter`, `filter_map`, `flat_map`, `scan`, `take_while`, `skip_while`, `rev`, `dedup`, `flatten`, `pairwise`, `windows`, `chunks`, `collect`, `count`, `sum`, `product`, `min`, `max`, `fold`, `any`, `all`, `find`, `find_map`, `for_each`, `position`, `reduce`, `partition`, `unzip`, `sort_by`, `sort_by_key`, `min_by`, `min_by_key`, `max_by`, `max_by_key`, `sum_by`, `product_by`, `chunk_by`, `count_by` on a type parameter.

### `From`

How a value of another type becomes this one. `x.into()` reads the `From` impl on the type the use site expects, and `?` converts an error through it.

```gossamer
impl From for Type { fn from(value: T) -> Self }
```

```gossamer
impl From<i64> for Point { fn from(v: i64) -> Point { Point { x: v, y: 0 } } }
```

A bound naming `From` licenses `from` on a type parameter.

### `TryFrom`

The fallible conversion into this type. `x.try_into()` reads the `TryFrom` impl on the `Ok` payload the use site expects.

```gossamer
impl TryFrom for Type { fn try_from(value: T) -> Result<Self, E> }
```

```gossamer
impl TryFrom<i64> for Even { fn try_from(v: i64) -> Result<Even, String> { if v % 2 == 0 { Ok(Even { v }) } else { Err("odd") } } }
```

A bound naming `TryFrom` licenses `try_from` on a type parameter.

## Operators

The operator has no meaning for a user type until an `impl` supplies one. Each block defines the one method named below, and the operator dispatches to it.

### `Add`

What `a + b` answers for this type. Without an `impl` the operator is rejected: a struct carries no arithmetic of its own.

```gossamer
impl Add for Type { fn add(&self, other: Self) -> Self }
```

```gossamer
impl Add for Point { fn add(&self, other: Point) -> Point { Point { x: self.x + other.x, y: self.y + other.y } } }
```

A bound naming `Add` licenses `add` on a type parameter.

### `Sub`

What `a - b` answers for this type. Without an `impl` the operator is rejected: a struct carries no arithmetic of its own.

```gossamer
impl Sub for Type { fn sub(&self, other: Self) -> Self }
```

```gossamer
impl Sub for Point { fn sub(&self, other: Point) -> Point { Point { x: self.x - other.x, y: self.y - other.y } } }
```

A bound naming `Sub` licenses `sub` on a type parameter.

### `Mul`

What `a * b` answers for this type. Without an `impl` the operator is rejected: a struct carries no arithmetic of its own.

```gossamer
impl Mul for Type { fn mul(&self, other: Self) -> Self }
```

```gossamer
impl Mul for Point { fn mul(&self, other: Point) -> Point { Point { x: self.x * other.x, y: self.y * other.y } } }
```

A bound naming `Mul` licenses `mul` on a type parameter.

### `Div`

What `a / b` answers for this type. Without an `impl` the operator is rejected: a struct carries no arithmetic of its own.

```gossamer
impl Div for Type { fn div(&self, other: Self) -> Self }
```

```gossamer
impl Div for Point { fn div(&self, other: Point) -> Point { Point { x: self.x / other.x, y: self.y / other.y } } }
```

A bound naming `Div` licenses `div` on a type parameter.

### `Rem`

What `a % b` answers for this type. Without an `impl` the operator is rejected: a struct carries no arithmetic of its own.

```gossamer
impl Rem for Type { fn rem(&self, other: Self) -> Self }
```

```gossamer
impl Rem for Point { fn rem(&self, other: Point) -> Point { Point { x: self.x % other.x, y: self.y % other.y } } }
```

A bound naming `Rem` licenses `rem` on a type parameter.

### `Neg`

What unary `-value` answers for this type. Without an `impl` the operator is rejected: a struct has no arithmetic of its own.

```gossamer
impl Neg for Type { fn neg(&self) -> Self }
```

```gossamer
impl Neg for Point { fn neg(&self) -> Point { Point { x: -self.x, y: -self.y } } }
```

A bound naming `Neg` licenses `neg` on a type parameter.

### `Not`

What unary `!value` answers for this type. Without an `impl` the operator is rejected: a struct has no negation of its own.

```gossamer
impl Not for Type { fn not(&self) -> Self }
```

```gossamer
impl Not for Mask { fn not(&self) -> Mask { Mask { bits: !self.bits } } }
```

A bound naming `Not` licenses `not` on a type parameter.

### `BitAnd`

What `a & b` answers for this type. Without an `impl` the operator is rejected: a struct carries no bitwise meaning of its own.

```gossamer
impl BitAnd for Type { fn bitand(&self, other: Self) -> Self }
```

```gossamer
impl BitAnd for Mask { fn bitand(&self, other: Mask) -> Mask { Mask { bits: self.bits & other.bits } } }
```

A bound naming `BitAnd` licenses `bitand` on a type parameter.

### `BitOr`

What `a | b` answers for this type. Without an `impl` the operator is rejected: a struct carries no bitwise meaning of its own.

```gossamer
impl BitOr for Type { fn bitor(&self, other: Self) -> Self }
```

```gossamer
impl BitOr for Mask { fn bitor(&self, other: Mask) -> Mask { Mask { bits: self.bits | other.bits } } }
```

A bound naming `BitOr` licenses `bitor` on a type parameter.

### `BitXor`

What `a ^ b` answers for this type. Without an `impl` the operator is rejected: a struct carries no bitwise meaning of its own.

```gossamer
impl BitXor for Type { fn bitxor(&self, other: Self) -> Self }
```

```gossamer
impl BitXor for Mask { fn bitxor(&self, other: Mask) -> Mask { Mask { bits: self.bits ^ other.bits } } }
```

A bound naming `BitXor` licenses `bitxor` on a type parameter.

### `Shl`

What `a << b` answers for this type. Without an `impl` the operator is rejected: a struct carries no shift of its own.

```gossamer
impl Shl for Type { fn shl(&self, other: Self) -> Self }
```

```gossamer
impl Shl for Mask { fn shl(&self, other: Mask) -> Mask { Mask { bits: self.bits << other.bits } } }
```

A bound naming `Shl` licenses `shl` on a type parameter.

### `Shr`

What `a >> b` answers for this type. Without an `impl` the operator is rejected: a struct carries no shift of its own.

```gossamer
impl Shr for Type { fn shr(&self, other: Self) -> Self }
```

```gossamer
impl Shr for Mask { fn shr(&self, other: Mask) -> Mask { Mask { bits: self.bits >> other.bits } } }
```

A bound naming `Shr` licenses `shr` on a type parameter.

### `Index`

What `value[i]` answers for this type. The index may be any type the method takes, and the result is whatever it returns.

```gossamer
impl Index for Type { fn index(&self, index: I) -> T }
```

```gossamer
impl Index for Grid { fn index(&self, i: i64) -> i64 { self.cells[i] } }
```

A bound naming `Index` licenses `index` on a type parameter.

### `IndexMut`

The `Index` contract under its writable spelling; both names reach the same `index`.

```gossamer
impl IndexMut for Type { fn index(&self, index: I) -> T }
```

```gossamer
impl IndexMut for Grid { fn index(&self, i: i64) -> i64 { self.cells[i] } }
```

A bound naming `IndexMut` licenses `index` on a type parameter.

## Supplied by the language

The language provides this behaviour outright, so an `impl` block would name a contract nothing dispatches through and is rejected. Each entry says what to write in its place.

### `Hash`

Hashing is structural and automatic: any hashable value keys a `Map` or a `Set`, and equal keys built at different allocations reach the same slot.

Instead: Remove the block; to key on part of a value, build the key yourself and store the value beside it.

```gossamer
let m = {Point { x: 1, y: 2 }: "origin-ish"}
```

A bound naming `Hash` licenses `hash` on a type parameter.

### `Hashable`

An older spelling of `Hash`. Hashing is structural and automatic.

Instead: Remove the block.

```gossamer
let s = #{Point { x: 1, y: 2 }}
```

A bound naming `Hashable` licenses `hash` on a type parameter.

### `Copy`

Every value is passed, assigned, and captured by value already, and no parameter asks for a `&` to avoid a copy.

Instead: Remove the block.

```gossamer
let b = a
```

### `Sized`

Every type has a known size; there is no unsized value to bound against.

Instead: Remove the block.

```gossamer
fn f<T>(value: T) { }
```

### `Send`

Every value crosses a `spawn` and a channel already: memory is reference-counted and the runtime owns the synchronization.

Instead: Remove the block.

```gossamer
cohort { spawn(|| work(value)) }
```

### `Sync`

Shared access is the runtime's business, not a marker's: reach for `sync::Mutex` or a channel when goroutines share state.

Instead: Remove the block.

```gossamer
let guard = sync::Mutex::new()
```

### `Drop`

Values are released deterministically by reference counting, with no destructor hook to run at the release.

Instead: Write `defer expr`, which runs when control leaves the enclosing block by any edge the compiler sees.

```gossamer
defer file.close()
```

A bound naming `Drop` licenses `drop` on a type parameter.

### `Into`

`x.into()` reads the `From` impl on the type the use site expects, so the conversion is written once, on the target.

Instead: Write `impl From<Source> for Target { fn from(value: Source) -> Target }`.

```gossamer
let p: Point = 5.into()
```

A bound naming `Into` licenses `into` on a type parameter.

### `TryInto`

`x.try_into()` reads the `TryFrom` impl on the `Ok` payload the use site expects, so the conversion is written once, on the target.

Instead: Write `impl TryFrom<Source> for Target { fn try_from(value: Source) -> Result<Target, E> }`.

```gossamer
let p: Result<Even, String> = 5.try_into()
```

A bound naming `TryInto` licenses `try_into` on a type parameter.

### `IntoIterator`

`for v in value` drives `Iterator::next` directly; there is no separate conversion step to implement.

Instead: Write `impl Iterator for Type { fn next(&mut self) -> Option<T> }`.

```gossamer
for v in countdown { println("{}", v) }
```

A bound naming `IntoIterator` licenses `into_iter` on a type parameter.

### `FromIterator`

`collect` ends an iterator chain with a `Vec`; building any other type from a sequence is an ordinary function or an associated `from`.

Instead: Write `impl From<Vec<T>> for Type`, or a plain `Type::from_values(xs)`.

```gossamer
let xs = (1..5).map(|i| i * i).collect()
```

A bound naming `FromIterator` licenses `from_iter` on a type parameter.

### `AsRef`

A parameter is `T` or `&mut T` and nothing else, so there is no shared-reference form to convert into.

Instead: Write an inherent method on the type: `impl Type { fn as_slice(&self) -> [T] }`.

```gossamer
fn total(xs: [i64]) -> i64 { xs.iter().sum() }
```

A bound naming `AsRef` licenses `as_ref` on a type parameter.

### `AsMut`

A callee writes through a `&mut T` parameter spelled at the call site, so there is no conversion into a mutable view to implement.

Instead: Write `fn extend(xs: &mut Vec<i64>)`, called as `extend(&mut items)`.

```gossamer
extend(&mut items)
```

A bound naming `AsMut` licenses `as_mut` on a type parameter.

### `Read`

Byte input is the standard library's `io::Reader` contract, which a type implements the way it implements any declared trait.

Instead: Write `use std::io` then `impl Reader for Type`.

```gossamer
let text = io::read_to_string(source)?
```

A bound naming `Read` licenses `read` on a type parameter.

### `Write`

Byte output is the standard library's `io::Writer` contract, which a type implements the way it implements any declared trait.

Instead: Write `use std::io` then `impl Writer for Type`.

```gossamer
writer.write(bytes)?
```

A bound naming `Write` licenses `write` on a type parameter.

### `Error`

`errors::Error` is a concrete type, not a contract: a fallible function answers `Result<T, errors::Error>` and `?` converts into it.

Instead: Use `errors::new(msg)` / `errors::wrap(cause, msg)`, or an error type of your own with `impl From<Yours> for errors::Error`.

```gossamer
fn load(p: String) -> Result<String, errors::Error> { fs::read_to_string(p) }
```

### `Future`

There is no `async` / `await`: concurrency is goroutines under a `cohort { }`, and a `JoinHandle` is what a pending result is held by.

Instead: Write `cohort { let h = spawn(|| work()) }` then `h.join()`.

```gossamer
cohort { let h = spawn(|| fetch(url)); println("{}", h.join()??) }
```

### `Fn`

The type of a closure or a function passed as a value, written with its parameters in parentheses. Capture is automatic, and there is no owned-versus-borrowed distinction between the three spellings.

Instead: Write `Fn(A) -> B` in the parameter's type, as in `fn each(xs: Vec<i64>, f: Fn(i64) -> ())`.

```gossamer
fn each(xs: Vec<i64>, f: Fn(i64) -> ()) { for v in xs { f(v) } }
```

### `FnMut`

The type of a closure or a function passed as a value, written with its parameters in parentheses. Capture is automatic, and there is no owned-versus-borrowed distinction between the three spellings.

Instead: Write `Fn(A) -> B` in the parameter's type, as in `fn each(xs: Vec<i64>, f: Fn(i64) -> ())`.

```gossamer
fn each(xs: Vec<i64>, f: Fn(i64) -> ()) { for v in xs { f(v) } }
```

### `FnOnce`

The type of a closure or a function passed as a value, written with its parameters in parentheses. Capture is automatic, and there is no owned-versus-borrowed distinction between the three spellings.

Instead: Write `Fn(A) -> B` in the parameter's type, as in `fn each(xs: Vec<i64>, f: Fn(i64) -> ())`.

```gossamer
fn each(xs: Vec<i64>, f: Fn(i64) -> ()) { for v in xs { f(v) } }
```

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## Which kind is which

The three kinds above answer three different questions, and the one a trait
falls into decides whether writing an `impl` for it does anything.

**Overridable** traits name behaviour every type already has. `Point` renders
through `{}`, compares with `==`, and sorts, with no `impl` written anywhere.
An `impl` *replaces* the synthesized behaviour rather than enabling it, and it
replaces it everywhere the value is shown or compared - inside a `Vec`, a `Map`
key, a tuple, an `Option`, or a struct field.

```gossamer
struct Point { x: i64, y: i64 }

// Without this, `{}` already prints `Point { x: 1, y: 2 }`.
impl Display for Point {
    fn fmt(&self) -> String { format("({}, {})", self.x, self.y) }
}
```

**Operator** traits name behaviour a user type does *not* have until you write
it. `a + b` on two `Point`s is an error until `impl Add for Point` exists.

```gossamer
impl Add for Point {
    fn add(&self, other: Point) -> Point {
        Point { x: self.x + other.x, y: self.y + other.y }
    }
}
```

**Automatic** traits name behaviour the language supplies outright, so there is
nothing for an `impl` to dispatch through and the block is rejected. Each entry
above says what to write in its place - `defer` for `Drop`, `impl From` for
`Into`, `impl Iterator` for `IntoIterator`. They remain usable as bounds.

## `Display` and `Debug` are separate channels

Both contracts declare one method that answers a `String`, and both are written
`fn fmt`. The `impl` header is what says which channel it is: `{}` reaches only
`Display`'s and `{:?}` only `Debug`'s. A type implementing one keeps the
synthesized rendering on the other.

```gossamer
impl Display for Point { fn fmt(&self) -> String { format("({}, {})", self.x, self.y) } }
impl Debug for Point { fn fmt(&self) -> String { format("Point[{}]", self.x) } }
```

Writing `fn to_string` inside an `impl Display` block is rejected (`GP0053`):
`to_string` renders *through* `Display`, so it is the caller's spelling, not the
implementer's.

## Equality and ordering

`PartialEq` and `Eq` reach the same `eq`; `PartialOrd` and `Ord` reach the same
`cmp`. Both spellings of each pair exist so a bound reads the way it does in
Rust, not because the language distinguishes partial from total.

`cmp` answers an `i64`: negative when the receiver orders first, zero when the
two tie, positive otherwise.

```gossamer
impl Ord for Point {
    fn cmp(&self, other: Point) -> i64 { self.x - other.x }
}
```

Structs, enums, tuples, and sequences already compare field by field in
declaration order, so an `impl` is for a type that must order some other way -
by one field, or by a computed key.

## Derive asks for the synthesized behaviour

`#[derive(Debug, Default, PartialEq, Eq, PartialOrd, Ord)]` names behaviour the
type has either way; the attribute is how a type says so where a reader or a
bound wants it named. `Clone`, `Hash`, `Copy`, `Display`, and `Serialize` are
**not** derivable (`GT0025`): copying, hashing, and serialization are automatic,
and the other two are written as an `impl` when you want to override them.

## One `impl` per trait and type

One trait reaches one type through one block. A second `impl` of the same pair,
or an `impl Debug for T` over a `#[derive(Debug)]`, is rejected (`GT0073`). A
`fn` the trait does not declare is rejected too (`GT0072`) - write it in an
inherent `impl Type { .. }` block instead.

A trait names behaviour, never a value's type. There is no `dyn`, so a bare
trait in a parameter or a field has no value shape to stand for:

```gossamer
fn f(x: Display) -> String { x.to_string() }        // GT0071
fn f<T: Display>(x: T) -> String { x.to_string() }  // write this instead
```

This holds for a trait you declare yourself as much as for a built-in one. A
type of your own may still take a built-in trait's name - `struct Read { .. }`
is your `Read`, and the resolver has already said which one a path reached.

An `impl` header naming a trait from the automatic list is rejected with
`GT0084`, which names what to write in its place.

## Discovering these at the terminal

`%info` in the REPL answers for any of these by name, and a `*` widens the query:

```
>>> %info Display
>>> %info *Ord*
```
