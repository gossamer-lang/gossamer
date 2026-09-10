# Gossamer - Skill Card

Drop this file into a model's context to teach it idiomatic Gossamer.
Self-contained. For anything not covered here, lean on the toolchain
(section 2) instead of guessing.

## 1. What Gossamer is

A goroutine-powered, fast-compiling language with automatic,
deterministic memory management (reference counting with cycle
collection, plus `arena { }` regions - no borrow checker, no
lifetimes, no tracing-GC pauses). Syntax is Rust-flavoured; the
runtime is Go-shaped (goroutines, channels). Source files conventionally end
in `.gos`, but `gos run TARGET` runs an existing file with any extension. The
toolchain binary is `gos`, and projects carry a
`project.toml` manifest. Pre-1.0.0: APIs may change. Most documented
surface is available on the bytecode VM, in-process JIT, and LLVM AOT,
but support is item- and platform-specific; check `gos feature-status`
and validate the target tier before relying on an API in production.

## 2. Lean on the toolchain

Run code early and often - the toolchain is the reference, not this
card:

- `gos mcp` (stdio MCP server, e.g. `claude mcp add gossamer -- gos
  mcp`) exposes `check` (structured diagnostics), `execute`, `build`,
  `test`, `fmt`, `doc`, `explain`, `lint`, `feature_status`, and semantic
  navigation (`hover`, `definition`, `references`, `workspace_symbols`).
  `check`, `execute`, `fmt`, `doc`, and `lint` take an inline `source`
  string in place of a path, so a snippet needs no file of its own.
  Prefer these to memorized API detail: `hover` answers "what is this and
  its type", `check` validates a draft, `explain CODE` expands any
  diagnostic, `doc` lists a file's items or a stdlib module's exports,
  `feature_status` says whether an API is settled. This card ships as its
  `gossamer://skill-card` resource.
- Without MCP: `gos check FILE` (rustc-class diagnostics with
  did-you-mean), `gos run FILE`, `gos explain CODE`, `gos doc FILE`.
- `gos check` is necessary, not sufficient - semantics are proven by
  `gos run`, and compiled behavior by `gos build`.

## 3. Cheat sheet

```gossamer
use std::io

const PI: f64 = 3.14159

struct Point { x: f64, y: f64 }
struct Pair(i64, i64)
enum Shape { Circle(f64), Rect { w: f64, h: f64 } }

trait Area { fn area(&self) -> f64 }

impl Area for Shape {
    fn area(&self) -> f64 {
        match self {
            Shape::Circle(r) => 3.14159 * r * r,
            Shape::Rect { w, h } => w * h,
        }
    }
}

fn sum(xs: [i64]) -> i64 {
    let mut acc = 0
    for n in xs { acc += n }
    acc
}

fn main() {
    let total = sum([1, 2, 3])
    println("total: {}", total)
}
```

## 4. Idioms

### 4.1 Not Rust: values, parameters, assignment

The syntax is Rust-flavoured; the semantics are not. There is **no
ownership transfer, no borrow checker, and no lifetimes**. Reason about
values, not moves.

**Every parameter is by value, and by value means the caller is safe.**
Passing a collection costs no copy where none is observable - a callee
that only reads reads the caller's own storage - and the callee still
cannot change what the caller holds. Declaring the parameter `mut` lets
the callee write it; the write lands on the callee's own value, never on
the caller's variable, so a `mut` parameter (and one whose value outlives
the call) is the case that takes storage of its own.

```gossamer
fn tally(mut xs: Vec<i64>) -> i64 { xs.push(1); xs.len() }   // no copy on the way in
fn extend(xs: &mut Vec<i64>) { xs.push(1) }                  // the one form that writes back

let mut items = #[1, 2]
tally(items)                 // items is still #[1, 2]
extend(&mut items)           // items is now #[1, 2, 1]
```

- **Pass `coll`, not `&coll`.** A `&` is for writing back (`&mut`) or for
  a signature that asks for one; it is never needed to avoid a copy.
- **A call never takes `&mut` implicitly.** `change(&mut value)` is
  spelled at the call site, and the parameter's type says `&mut T`.
- **Assignment is the same story**: `let b = a` gives `b` a value of its
  own. `b.push(3)` leaves `a` alone. `a.clone()` is only needed when you
  want a second value from one you are still writing to.
- **Passing a value twice is fine.** Nothing is consumed, so `f(xs)`
  then `g(xs)` needs no clone and no borrow.
- **Scalars are copies** (`i64`, `f64`, `bool`, `char`).
- **The one asymmetry is closure capture**: a closure captures a
  container (`Vec`, `Map`, `Set`) by managed reference, so `xs.push(v)`
  inside a closure IS visible outside it, while everything else - a
  `Copy` scalar, a `String`, a struct - is captured by value, so
  `count += 1` and `s.field = v` are not. Accumulate into a container,
  never into a captured scalar or struct.

The Rust habits that do not compile here, in the order they actually
show up:

| Rust habit | Gossamer |
|---|---|
| `vec![a, b]` | `#[a, b]` (`[a, b]` is a fixed array) |
| `HashMap::new()` + `insert` | `{"k": 1}` / `Map::new()` |
| `HashSet` / `BTreeSet::from` | `#{a, b}` |
| `include!` / `#[path]` | a file IS a module - reach it with `use` |
| `"text".to_string()` | a string literal already IS a `String` |
| `&str` vs `String` | one `String` type; no parameter asks for a `&` |
| `move` closures, lifetimes, `Rc<RefCell<_>>` | capture is automatic; RC is the memory model |
| `impl Display` to print | `{}` renders every type; `impl Display` only to override |
| `?` on an `Option` in a `Result` fn | works, but the fn must return `Option` for the `Option` case |

### 4.2 Writing it

Write clear, low-complexity, concise code.

- **Import everything you name.** A module's items are never in scope on
  their own - reach them through a path or a `use`. For the standard
  library write `use std::{env, fs}` before `env::args()` or
  `fs::read(...)`; for a sibling file write `use util::add` (or spell
  `util::add(..)` in full). The file layout declares the module, the
  import brings its names in. A bare name some module declares reports
  GR0011 with the exact `use` line to add. A `[dependencies]` package is
  a stronger case: its module is reached ONLY through the import that
  names it, so write `use intcode` (or `use "example.com/intcode"`,
  `use intcode::run`, `use intcode::{run}`, `use intcode as ic`) before
  any `intcode::` path - an un-imported one reports GR0016. A package
  name may carry `-`, which no identifier may: its module name is the
  final path segment with each `-` replaced by `_`, so `pgsql-gos` is
  imported as `use pgsql_gos` and a written `use pgsql-gos` reports
  GP0040. Two packages reaching source under one module name report
  GR0019. Primitive and core collection types, variants, macros, and the
  documented prelude need no import.
- **Default to immutable.** `let` first; `let mut` only for a binding
  that genuinely changes, kept near a single update site. `if` /
  `match` / `loop ... break v` are expressions - bind their result.
- **Compound-assign accumulators**: `x += 1`, never `x = x + 1`.
- **`if let` / `while let`** for `Option` and single-variant matches;
  `while let Some(v) = rx.recv()` is the canonical channel drain.
  Let-chains: `if let Some(x) = a && let Some(y) = b && x > 0 { .. }`
  (`&&`-only; earlier binds visible later).
- **Comma-separated destructuring everywhere**: `let a, b = pair`,
  `let tx, rx = channel()`, `let mut x, mut y = 7, 8`. A `let` target is
  a plain list, never a parenthesised tuple pattern - `let (a, b) = pair`
  reports GP0042 with the rewrite. Parentheses still group a pattern that
  sits beside others, so `for (k, v) in m` and `match p { (x, y) => .. }`
  keep them, as does a nested element: `let a, (b, c) = 1, (2, 3)`.
  The same list assigns: `a, b = b, a` swaps with no temporary, since the
  right side is evaluated before the first write, and a compound operator
  pairs element-wise (`x, y += 2, 3`). One value on the right spreads over
  the list when it is a tuple (`let a, b = pair`). Each assignment target
  must be a writable place (a binding, field, index, tuple position, or
  dereference) or `_` to discard it; a target naming no place (`5 = 1`)
  reports GT0078.
- **`for x in xs`** over collections - no `.iter()`, no `*x`.
  Bare `String` iteration yields Unicode `char` values; use `.bytes()`
  for UTF-8 bytes; `s.chars()` is a cursor, so `.collect()` materialises it. An omitted range start begins at zero (`..3` yields
  `0`, `1`, `2`). A range can also be stored and consumed later:
  `let a = 0..3`, then `for i in a { println(i) }`. A `Result` or
  `Option` is NOT iterable (GT0067): a fallible API like
  `fs::read_dir(dir)` hands back `Result<Vec<T>, _>`, so take the value
  first (`?`, a `match`, `unwrap_or(..)`) and iterate that.
- **Bare integer indices** - `arr[i]` takes `i64`, no `as usize`.
  Indexed reads and writes outside `[0, len)` panic on every tier. Vec
  `insert` accepts `0..=len` and returns `Result<(), errors::Error>`;
  `remove` accepts `0..len` and returns `Result<T, errors::Error>`.
- **Any hashable value is a `Map` / `Set` key** - integers, `bool`,
  `char`, `String`, tuples, fixed arrays, structs, and enums (unit or
  payload), nested freely. Keys compare by value, so an equal key built
  at a different allocation finds the same slot, and `keys()` hands back
  the aggregate you wrote.
- **`m.inc(k)` / `m.inc(k, by)`** for counters; `m.or_insert(k,
  default)` for get-or-fill (`m.or_insert(k, d).method(args)` writes
  the mutation back into the stored value). `arr.swap(i, j)`.
- **`collect()` belongs to the iterator; `to_vec()` converts a
  collection.** `collect` is how an `Iterator` (a `Range` included) ends a
  chain, and it is not on a collection that already holds its values.
  `to_vec` copies a borrowed or fixed-length sequence - an array, a slice,
  a `Set`, a `BTreeSet` - into an owned `Vec`, so neither is written on a
  `Vec`, which is already both. For the same reason a `String` has no
  `to_string`: use `clone` for a copy, and `to_string` only to convert
  something else into text.
- **`to_string()` renders whatever `{}` renders.** A scalar, a tuple, a
  struct, an enum, a `Vec`, a `Map`, a `Set`, an `Option`, a `Result`, and
  every nesting of them answer the same text `format("{}", x)` builds; a
  handle, a closure, a channel, and a `JoinHandle` have no rendering and
  report GT0062. `#[derive(Display)]` is not written - the rendering is
  synthesized, and `impl Display for T { fn fmt(&self) -> String }`
  overrides it everywhere a value of that type is shown, including inside a
  `Vec`, `Map`, tuple, `Option`, or struct field. `impl Debug for T { fn fmt }`
  is the same override for `{:?}`. Both contracts declare one method that
  answers a `String` and both are written `fn fmt`; the `impl` header is what
  says which channel it is, so `{}` reaches only `Display`'s and `{:?}` only
  `Debug`'s, and a type implementing one keeps the synthesized rendering on
  the other channel. `x.to_string()` renders through `Display`. Writing
  `fn to_string` inside an `impl Display` block reports GP0053. A
  trait names behaviour, never a value's type: `fn f(x: Display)` reports
  GT0071, and an `impl` header naming a trait nothing declares reports GT0070.
- **An `impl Trait for Type` block defines the trait's items and nothing
  else.** A `fn` the trait does not declare reports GT0072 - write it in an
  inherent `impl Type { .. }` block, or declare it in the trait. One trait
  reaches one type through one block: a second `impl` of the same pair, or an
  `impl Debug for T` over a `#[derive(Debug)]`, reports GT0073.
- **A function that answers a value declares its type.** `fn add(a: i64, b:
  i64) { a + b }` reports GT0074: the signature is what a caller reads, so
  write `-> i64`. A body with no tail expression answers a unit and needs no
  return type.
- **A collection traverses eagerly; `iter()` makes it lazy.** `xs.map(f)`
  on a `Vec`, array, or slice walks the values it already holds and
  answers a `Vec`. `xs.iter()` answers an `Iterator`, for when you do not
  want the whole sequence in memory: `xs.iter().map(f).take(3).collect()`
  produces three elements' worth of work. A `Range` is already an iterator,
  so `(1..5).map(|i| i * i).sum()` reads straight through. The element's
  shape does not change that: a sequence of tuples or structs walks through
  the same cursor a sequence of scalars does. A `Map` hands over its pairs
  with `iter()` (a cursor, over pairs read under the map's lock);
  `m.iter().collect()` materialises them. Adapters
  (`map`, `filter`, `take`, `skip`, `step_by`, `enumerate`, `rev`,
  `zip`, `chain`, ...) answer another iterator and stay
  lazy; terminals (`collect`, `sum`, `count`, `min`/`max`, `fold`,
  `any`/`all`, `find`/`position`, `max_by_key`, `join`) end the chain
  and hand back a value. An iterator is single-use (GT0042) - bind a
  fresh `.iter()` per pipeline. The `iter::` free forms take an iterator
  too, for `|>` pipelines. **Every std free function takes its data
  first**, so a free call reads as the method it stands for -
  `iter::map(xs, f)` beside `xs.map(f)`, `option::unwrap_or(opt, 0)` beside
  `opt.unwrap_or(0)` - and a call written with the data last reports
  GR0021. `xs.join(sep)` Display-joins any
  sequence whose element `{}` renders, without a traversal.
- **A `Set` has no element order, so its traversal is the iterator's.**
  The set itself answers membership and cardinality (`insert`, `remove`,
  `contains`, `len`, `is_empty`, `clear`), set algebra (`union`,
  `intersection`, `difference`, `symmetric_difference`, `is_subset`,
  `is_superset`, `is_disjoint`), `to_vec`, and `iter`. Every sequence
  operation is written on the iterator - `s.iter().take(3)`,
  `s.iter().count(|v| v > 1)`, `s.iter().map(f).collect()` - and
  `s.take(3)` reports GT0002 with that spelling. A walk yields each element
  once in an order the language promises nothing about (every tier
  reproduces the same one, which is not a licence to depend on it): sort the
  materialised sequence, or use `BTreeSet`, when order is part of the
  answer. Printing (`{:?}`) and serialization sort both kinds, so rendered
  output is stable whatever order the elements went in.
- **A callback has one shorthand.** A std free function named in
  value position IS the closure that calls it, so `xs.map(math::abs)`
  and `xs.map(base64::encode)` need no `|v|` (a macro is not a function:
  `fmt::format` reports GR0018 and is written `format(..)`, so pass a
  closure that calls it). Everything else is a written closure:
  `xs.map(|v| v.abs())`, `people.map(|p| p.name)`. `$` is not part of
  the language: `xs.map($.abs)` reports GP0027.
- **Use metadata already returned by an API.** For entries from
  `fs::read_dir` or `fs::walk_dir`, inspect `entry.is_file`,
  `entry.is_dir`, `entry.is_symlink`, `entry.size`, `entry.path`, and
  `entry.name` directly. Do not repeat the lookup with
  `fs::is_symlink(entry.path)`, `fs::is_file`, `fs::is_dir`, or
  `fs::file_size`; redundant filesystem calls are slower and can race
  with changes after the directory read.
- **`s.to_i64()` / `to_f64()` / `to_bool()`** - strict full-string
  parses returning `Option<T>`:
  `env::args().first().unwrap_or("8").to_i64().unwrap_or(8)`.
- **Collection literal spellings are distinct.** Fixed array and Vec
  construction differs from Rust. Use `#[]` / `#[1,2,3]` for `Vec`, `[]` /
  `[1,2,3]` for fixed arrays, `{}` / `{"one": 1}` for `Map`, and `#{}` /
  `#{1,2,3}` for `Set`. `Queue`, `Stack`, `Deque`, `MaxHeap`, and `MinHeap`
  have no literal: build them with `T::new()` or `T::from([1,2,3])`. The
  repeat form follows the same spelling: `[5; 5]` is a fixed array of five
  `5`s and `#[6; 7]` is a `Vec` of seven `6`s. A value RENDERS in the
  spelling that builds it, at every depth and on every tier: `#[1, 2]` for a
  `Vec`, `[1, 2]` for a fixed array or slice, `#{1, 2}` for a `Set` or
  `BTreeSet`, `{"a": 1}` for a `Map`.
- **Prefer dedicated collection contracts for intent.** `Stack` is the
  idiomatic LIFO-only type even though `Vec` can push/pop at the end;
  `Queue` is the FIFO-only type even though a deque can model it; `MinHeap`
  and `MaxHeap` avoid negating keys or wrapping values in `Reverse`. Use the
  general structures only when you actually need their broader method surface.
  Each container has exactly one name: `HashMap`, `HashSet`, `VecDeque`,
  `VecQueue`, `VecStack`, `BinaryHeap`, `MaxBinaryHeap`, and `MinBinaryHeap`
  are rejected (GR0006).
- **`Map` and `BTreeMap` are distinct types** over one representation:
  each constructor answers its own, and neither converts to the other.
- **Collection constructors infer**: `let mut m = Map::new()`,
  `let empty: Map<String, i64> = Map::from([])`, and
  `let map = {"one": 1}`. `Map::from` accepts array pairs, while
  map literals construct `Map` values directly. `#{a, b}` constructs
  a `Set`, or a `BTreeSet` when an expected `BTreeSet<T>` type is present.
- **Two String index spaces.** `s.len()`, `s[i]`, and bare iteration
  count Unicode scalars, so `s[i]` is a `char` (compare with `'0'`,
  widen with `s[i] as i64`). `s.byte_len()`, `s.substring(a, b)`,
  `s.byte_at(i)`, `s.as_bytes()`, and `s.bytes()` are byte offsets.
  `s.byte_at(i)` is the byte as `i64` - compare with byte literals
  (`s.byte_at(i) >= b'0'`), render with `as char`; prefer it over a
  per-step `substring`. Do not mix the two: `s[i]` is not the byte at
  byte offset `i` for any non-ASCII string.
- **Format captures walk field paths**: `println("{name}:
  {a.balance} {t.0} {o.inner.hits} {a.balance:>8} {f.0:.2}")`.
  `{:?}` renders any nesting of collections, tuples, structs, enums,
  `Option`, and `Result` identically on every tier.
- **Range binds looser than arithmetic, tighter than `|>`**:
  `i * i..n` is `(i * i)..n`; `0..n |> iter::sum` pipes the range.
- **Recursive enums work directly**: `enum List { Cons(i64,
  List), Nil }`; `Box`/`Arc`/`Rc` are transparent (bare
  `Cons(i64, List)` works too).
- **Structs, enums, arrays, Vecs, tuples compare structurally** - no
  derive; lexicographic by declaration order / variant rank. A user
  `impl` of `eq`/`cmp` overrides.
- **Derivable**: `#[derive(Debug, Default, PartialEq, Eq, PartialOrd,
  Ord)]` (enums too; `#[default]` picks the variant). `Clone`,
  `Hash`, `Copy`, `Display`, `Serialize` are NOT derivable (GT0025):
  copy, hashing, and serde are automatic; conversions and operators
  are written `impl Trait for T` - `From`/`TryFrom` (used by
  `x.into()`/`x.try_into()`), `Add` `Sub` `Mul` `Div` `Rem` `Neg`
  `Index` `BitOr` `BitAnd` `BitXor` `Shl` `Shr`.
- **Labeled loops**: `'outer: for .. { break 'outer }`.
- **`defer expr`** - runs when control leaves the enclosing `{ }` by any
  edge the compiler sees (block end, `return`, `break`, `continue`, `?`),
  LIFO; per-iteration in a loop body. A panic is not one of those edges, so
  put what must hold across a panic somewhere other than a `defer`.
- **`let PAT = expr else { ... }`** - the else block must diverge.
- **Name an argument or default a parameter** when a call reads better
  for it: `fn volume(width: i64, height: i64 = 2)` then `volume(2)`,
  `volume(width = 2, height = 3)`, or `volume(2, height = 3)`. Positional
  arguments come first, then names, in any order. A default must be a
  literal (optionally negated) and is spliced per call site. Works on
  methods and associated fns; when two types declare one method name with
  different parameters, a named call on it is GR0013 - pass positionally.
- **Plain functions for free-standing logic**; `impl` only when state
  is genuinely tied to a type.
- **`Result<T, E>` + `?`** for fallibility; panic only for invariant
  violations. Exhaustive `match` - no `_ =>` unless every unmatched
  case genuinely means the same thing.
- **Goroutines under a `cohort { }`** for concurrent work: `spawn`
  inside the block, which joins every child on every exit path and
  reports the first failure. `spawn` is the only way to start one, and
  a `spawn` MUST be written lexically inside a `cohort { }` in its own
  function - `main` is the one exemption, since the root cohort's
  extent is main's own. Anywhere else it is `GT0086`, because a
  function that spawns without opening a cohort hands its children to
  whatever cohort its caller happens to be in, which neither side's
  signature says. A closure body runs on the child, so bind an operand
  you need read at the spawn site first. Channels carry values;
  `sync::Mutex` only when shared memory is simpler.
- **`arena { ... }`** for object graphs that die together:
  bump-allocated, freed wholesale at every exit. Nothing allocated
  inside may be referenced after the block - statically enforced
  (`GM0003`). Statement position only; nests.
- **Bare numeric literals** - `0`, `1.5`, never `0i64`; suffix only
  with no contextual hint. String literals are already `String` - no
  `.to_string()`, and no parameter asks for a `&` around one.
- **HTML lives in its own file**, read at run time or embedded with a
  `comptime` read, never written as markup inside `.gos` source: an
  `.html` file is what an editor, a formatter, and a template engine can
  each work on.
- **`"""` strings dedent themselves** - the body starts on the line
  after the opening delimiter, and the indentation it shares with the
  closing `"""` is stripped from every line, so an embedded SQL
  statement keeps the shape of the code around it. Escapes work as in `"..."` and
  decode after the strip; only whitespace may follow the opening `"""`
  (GP0033). `gos fmt` moves the body with the line that opens it, so
  re-indenting the statement re-indents the block.
- **The block words are contextual, never reserved**: `arena`, `cohort`,
  `comptime`, `defer`, `newtype`, `packed`, and `select` are keywords
  only where their construct starts and ordinary names everywhere else,
  so a binding, field, parameter, or method may carry any of them
  (`q.select(1)`, `let defer = 5`, `s.comptime`). `spawn` is not among
  them: it is the one way to start a goroutine, so declaring anything
  under that name reports GR0020.
- **Fixed set of compiler-known calls, each written as an ordinary
  call** - the set is closed and recognised at the `(`, so a `!` after
  one of these names reports GP0049 and any other `name!(..)` is a parse
  error (no user macros): `println` `print` `eprintln` `eprint`
  `format` `panic` (Rust `{}` / `{name}` / `{:spec}` formatting:
  width/align/fill `{:*>8}`, zero-pad `{:08}`, radix `{:x}` `{:b}`
  `{:o}`, precision `{:.2}`); `matches`, `todo`, `unimplemented`,
  `unreachable`, `dbg`; build-time `regex::compile` / `sql::statement`
  (validate the literal, and the older `regex!` / `sql!` spellings
  report GP0051) and `codegen` (splice a `comptime fn`'s String).
- **Output that ends in a newline leaves the process on that write**, on a
  terminal, a pipe, and a file alike, so a program that announces a line and
  then blocks is visible while it runs. A `print` with no terminator
  accumulates until a later newline or an explicit `io::stdout().flush()`;
  `eprint` / `eprintln` are never buffered and flush stdout first.

## 5. The `|>` forward-pipe

`|>` composes FREE FUNCTIONS, which have no receiver to chain from.
Reach for it in functional-style code - a value flowing through two or
more free-function transforms. Everything with a receiver chains on its
own, and a method chain is the shorter spelling: prefer
`s.trim().to_lowercase()` over any pipe form of it. The two mix freely,
and a chain can feed a pipe. Left-associative, very low precedence.

- `x |> f` is `f(x)` - a bare callable takes the value as its only
  argument. `x |> f()` is the same.
- A step that writes arguments is a CLOSURE, whose parameter is the
  slot the piped value fills: `x |> |v| f(a, v)` is `f(a, x)`, and
  `x |> |v| f(v, a)` is `f(x, a)`. The parameter may sit anywhere the
  body reaches, so `x |> |v| f(a, v).len()` is `f(a, x).len()`. No
  argument order is assumed, so the slot the reader sees is the slot the
  value fills - which stays true whatever a callee's parameters are. An
  argument-taking step that is not a closure is GP0041.
- A closure written directly as a step IS the call it stands for: the
  parameter is bound in the caller's frame, so the step costs nothing a
  hand-written call would not, and a `let mut` the body updates is the
  caller's. A body whose control flow leaves the closure - a `return`, a
  `?` - keeps the closure it was written against.
- `x |> |v| recv.m(a, v)` pipes into a method on an external receiver,
  and `x |> |v| v * 2` pipes into a plain expression.
- `$` is not part of the language: any `$` reports GP0027, whether in a
  step (`x |> f(a, $)` - write `x |> |v| f(a, v)`), as a receiver
  (`x |> $.trim` - write `x.trim()`), or as a callback (`xs.map($.abs)`
  - write `xs.map(math::abs)` or `xs.map(|v| v.abs())`).
- `gos check --fix` rewrites an argument-taking step into the closure
  it stands for, putting the parameter in the trailing slot - confirm
  that is the slot the call needs.

```gossamer
// free functions: the pipe earns its place
let n = 3 |> double |> |v| add(10, v) |> |v| clamp(0, 100, v)

// methods: chain them, and feed the chain to a pipe
let shout = "  Hi  ".trim().to_lowercase() |> exclaim
```

## 6. Grammar essentials

- **Comments**: `//` and non-nesting `/* */` only. A run of `//`
  lines directly above an item is its documentation; `gos test` runs
  fenced code inside doc comments. Mark a non-runnable fence with the
  `text` info string, written as "```text ... ```".
- **Semicolons are same-line separators only**: `let a = 1; let b = 2`
  replaces a newline between statements. A trailing semicolon, or one before
  a newline or `}`, is invalid. A newline followed by leading `&`, `*`, or `-`
  starts a NEW statement; for multi-line continuation, end the previous line
  with the operator or parenthesize.
- **Delimited lists** use commas on one line and newlines when multiline.
  This covers every delimited list: function arguments and parameters,
  closure parameters, struct fields and literals, enum variants and payload
  fields, tuples and tuple types, `Vec` / array / `Map` / `Set` literals,
  tuple, slice, and struct patterns, generic parameters and arguments, and
  `use` lists. Legacy multiline commas parse, and `gos fmt` removes them.
  A newline separates elements only where a comma already could, so
  `(\n expr \n)` stays a parenthesised expression rather than a 1-tuple.
- **Struct construction follows declaration shape**: unit structs use
  `Unit` or `Unit {}`, tuple structs use `Pair(a, b)`, and named structs
  require keyed `Point { x: 1, y: 2 }` fields. Positional or mixed
  braced named-struct literals are rejected.
- **Imports**: `use std::{iter, os, strings}`, alias via `{Map as
  Scores}`; always spell the full path (`std::encoding::json`, not
  `std::json`) - paths validate against the std manifest (GR0005).
  An import that names nothing reports GR0005 where it is written.
  Local modules import the same way: `use util::{add, Widget}`,
  `use deep::nest::Nested`, `use crate::util::Widget`. A path written
  inside a module is anchored at that module, so `self::child::item`,
  a bare `child::item`, and `super::sibling::item` all resolve; a
  `mod name;` with no file behind it is GR0010. A type belongs to the
  module that declares it, so two modules may each declare a `Point`
  and both stay distinct.
- **Entry file may omit `fn main`**: top-level statements become the
  implicit main (only the entry file; `?` there makes it return
  `Result<(), errors::Error>`; exit code via `process::exit(n)`).
- **Generics**: `fn f<T: Trait>(x: T)` monomorphises per call site
  on every tier (one or more bounds `T: A + B`, in the parameter list or a
  `where` clause; struct-typed params; no `dyn Trait`).
  Generic structs `struct Wrapper<T>` + `impl<T>` work. Const-generic
  array length `fn sum<const N: usize>(xs: [i64; N])` is inferred
  from the argument (not usable as a bare value or repeat count).
- **Associated items**: a trait declares `type Item` (optional default)
  and `const MAX: i64` (optional default); each impl supplies one
  concrete `type Item = T` / `const MAX: i64 = 10`. Project with
  `Self::Item`, `T::Item`, `Type::MAX`, `T::MAX`. Pin an ambiguous
  projection with an equality constraint: `T: Iterator<Item = i64>`.
  Resolution order: equality constraint, the concrete base's impl,
  the trait default, the trait's single implementor - several impls with
  no constraint is GT0061, an impl missing a required item is GT0059.
  No generic associated types, none on `dyn`.
- **A parameter is `T` or `&mut T`, and nothing else.** Passing a value
  copies nothing whatever its type, and a callee writes to the caller's
  variable only through `&mut`, so a shared `&` in a parameter names no
  choice and reports GP0054; a shared `&` on an argument reports GP0055,
  and a reference reaching a parameter that takes the value reports GT0082
  with the `*x` rewrite.
  A sequence view is written `[T]` and takes an array, a `Vec`, or another
  view; `&mut [T]` is the form that writes through. A method receiver is
  `&self` (reads) or `&mut self` (writes), and `&self` names the same value
  `self` does.
  Calls never create `&mut` implicitly: pass a writable place as
  `change(&mut value)`. Forward an existing `&mut T` as `change(value)`.
  A parameter's reference lives in its TYPE: write `fn f(xs: &mut Vec<i64>)`,
  never `fn f(&mut xs: Vec<i64>)` - a `&` in the pattern destructures a
  reference the type already names, so over a value type it reports GT0069.
  `*` reaches the place a reference names, so a write through one over a
  value (`*x = v` where `x` is an `i64`) reports GT0083.
- **`&x` and `&mut x` are aliases, never copies**, and there are no
  lifetimes or non-lexical borrow analysis. A lightweight lexical check
  rejects a second named `&mut` to the same root, overlap with an active
  named mutable alias, and repeated mutable roots in one call.
- **Types**: `bool char i8..i64 u8..u64 isize usize f32 f64 String
  [T] [T; N] (A, B) Option<T> Result<T, E> &mut T` + user types. `i128`
  / `u128` are rejected (GT0014). Transparent `type Id = i64` /
  `type Pair<A> = (A, A)` aliases substitute everywhere (cycle =
  GT0024). `newtype UserId = i64` is the OPAQUE form: a distinct
  type over the same representation, at no runtime cost. `.into()`
  converts to and from its own representation only; a fixed array
  into the `Vec` of its element is the one other built-in pair, and
  every other needs `impl From`, else GT0066. The target comes from
  the use site, never the receiver, so an `.into()` / `.try_into()`
  no annotation, parameter, or return reaches is GT0066 too. An
  alias inherits equality, ordering, hashing, and formatting - so it
  keys a `Map`, sorts, and prints - but NOT the representation's
  methods (GT0002) or operators (GT0003). Give it an `impl` of its
  own, and it serializes as the representation.
- **Casts**: `x as T` is whitelist-checked (numeric<->numeric,
  bool/char->int, u8->char). Int->narrow-int masks at width (`300 as
  u8 == 44`); float->int truncates toward zero and saturates at i64
  width, NO narrow mask (`300.7 as u8 == 300`, NaN -> 0).
- **Patterns**: literals, `_`, `name`, `mut name`, variants, structs,
  tuples, ranges (`1..=5`, `1..5`, open-ended `lo..` / `..=hi`;
  range arms still need a `_` arm - opaque to exhaustiveness),
  or-patterns, `@`-bindings, rest `..` (slice head/tail: `if let
  [first, ..rest] = xs`), guards. Irrefutable `let` destructuring
  covers structs (with renames), nested enums/tuple-structs, and
  or-patterns binding the same names.
- **Byte literals**: `b'A'` is a `u8`.
- **An enum says how wide its discriminant is stored**: `enum Foo { .. }`
  takes the smallest byte-aligned width, `packed enum Foo { .. }` the
  smallest number of bits, and either may name one with `: uN` for any
  unsigned width `u1`..`u64` (`enum Level: u16`, `packed enum Bit: u1`).
  Too narrow for the variants is GT0081, a representation that is not an
  unsigned width is GP0050, and `packed` is a keyword only before `enum`.
  Nothing else changes with the width: matching, `==`, ordering, `{}` /
  `{:?}`, serialization, `Map` keys, arrays, and tuples behave the same.
- Enums cap at 256 variants (GT0012).

## 7. Comptime (the metaprogramming story)

Zig-style `comptime`, not macros: `comptime { }` blocks, `comptime
fn` calls, and `comptime` params run on the bytecode VM during
compilation and fold to literals, so every tier compiles the same
constant. `typeInfo::<T>()` reflects fields; a plain `for (name, ty)
in typeInfo::<T>()` loop unrolls per field at compile time
(`field_of(v, name)` projects) - the basis for reflection-driven
serializers specialized per turbofish call site. `codegen` splices
a `comptime fn`-built `String` as source.

## 8. Error handling

Fallible functions return `Result<T, E>`; propagate with `?` (also
works on `Option` inside Option-returning functions, auto-converts
errors through `From`, and works inside macro args).

```gossamer
use std::{errors, fs}

fn load(path: String) -> Result<String, errors::Error> {
    fs::read_to_string(path)
        .map_err(|e| errors::wrap(e, format("reading {}", path)))
}
```

`errors::new(msg)` / `newf(fmt, ..)` / `wrap(cause, msg)` /
`is(err, needle)` / `join([..])`; walk chains with `err.cause()`.
`{}` on a wrapped error prints the colon-joined chain;
`.message()` is the top message only. Ok-path piping:
`fs::read_to_string(f) |> |v| result::map(v, |s| print("{s}"))
|> |v| result::unwrap_or_else(v, |e| eprintln("{e}"))`.

Panics are goroutine-scoped: a spawned goroutine's panic ends only
that goroutine; a main-goroutine panic is fatal (exit 101). Integer
divide/modulo by zero panics (GX0005); `i64::MIN / -1` wraps. Deep
recursion raises `GX0008` and exits 101 on every tier, with the same message.
The depth differs by tier because the limit does: the bytecode VM refuses the
call at 4096 frames (65536 on the tail-call path) and prints its call stack
with repeated frames collapsed, while JIT-compiled and native frames run until
the installed stack guard trips on the real OS stack. Neither corrupts memory.

## 9. Concurrency

**`cohort { }` is the default shape.** The block owns every goroutine
`spawn`ed inside it and cannot be left until each has finished, so a
child cannot outlive it. Its value is `Result<(), errors::Error>`: the
first child failure (lowest spawn index, never completion order) cancels
its siblings and becomes the block's `Err`, so `cohort { .. }?` reads
like any fallible call. `main` runs inside an implicit root cohort, so a
spawned goroutine never outlives the program and a failure nobody joined
is reported on stderr at exit instead of vanishing.

`spawn(f)` returns a `JoinHandle<T>` (`handle.join()` ->
`Result<T, String>`) and attaches to the cohort it is written inside.
There is no detached form, and there is no ambient one either: a `spawn`
must sit lexically inside a `cohort { }` in the same function body, or
the checker reports `GT0086`. `main` is the only exemption - the root
cohort's extent is main's own extent, so a child started there does not
outlive the scope that owns it, which is not true of any other function.
A helper that spawns and hands its handles back to the caller is the
shape the rule refuses: move the spawns into the caller's cohort and let
the helper take or answer a closure.

`spawn(f, reason: "fetch users")` labels the goroutine, and the cohort's
reports name a labelled child by that label instead of by its spawn index. Typed channels: `recv()` blocks until a value arrives or the
channel is closed, and closing is the producer's job - `close()` is what
ends a `while let Some(v) = rx.recv()` drain, and a send into a closed
channel panics with `send on closed channel`.

```gossamer
fn gather() -> Result<(), errors::Error> {
    cohort {
        let a = spawn(|| fetch("one"))
        let b = spawn(|| fetch("two"))
        println("{} {}", a.join()??, b.join()??)
    }
}
```

Header settings: `cohort(policy: Policy::CollectAll)` runs every child
and reports all failures, `Policy::Race` stops at the first success (the
losers are cancelled, not rolled back), `cohort(timeout: 500)` bounds the
block in milliseconds, and `cohort(isolation: Isolation::Thread)` gives
each child its own OS thread (`Isolation::Shared` is the default).
`cohort(on_error: OnError::Log)` names each failure on stderr and answers
`Ok` (`Propagate` is the default, `Ignore` says nothing);
`cohort(cancellable: false)` exempts the block from cancellation; and
`cohort(drain: 2000)` bounds the wait for the children once the body is
done, where `timeout:` bounds the body itself. No setting makes a child
unaccountable - every one is counted, drained, and named if it never
finishes - what synchronous Rust FFI and
never-yielding CPU-bound work need. Cancellation is cooperative and
never a kill: a cancelled `recv` answers `None` exactly as a closed
channel does, a `sleep` returns early, and the child leaves through its
own exit path with its `defer` frames running in order. Pure computation
is not a cancellation point - poll `runtime::cohort_cancelled()`.

**Scheduling is cooperative; there is no async preemption.** A goroutine
yields at a safepoint - any channel / `select` / mutex / `sleep` /
scheduler-aware read, and every function call - and a watchdog asks a
long-running worker to yield at its next one. A CPU-bound loop that calls
NOTHING holds its worker to completion under `gos build` (the compiled
back-ends leave loop back-edges un-polled so numeric loops stay tight); the
bytecode VM still yields on back-edges. Give such a loop a call on an outer
iteration - `runtime::cohort_cancelled()` serves as both the cancellation
point and the safepoint.

```gossamer
use std::sync::channel

fn produce(tx: Sender<i64>) {
    tx.send(1); tx.send(2); tx.send(3)
    tx.close()
}

fn main() {
    let tx, rx = channel()
    spawn(|| produce(tx))
    let mut total = 0
    while let Some(v) = rx.recv() { total += v }
    println("total: {}", total)
}
```

`select { x = rx_a.recv() => .., default => .. }` multiplexes (arms
poll in source order; `default` makes it non-blocking). `select` is
contextual, never reserved - only the word directly before `{` opens
the expression - so a method or binding may still be named `select`.
One-shot timer: `time::after(Duration::from_millis(n)) -> Receiver`,
whose `.recv()` is a select timeout arm.
`std::sync` also has `Mutex`, `RwLock`, atomics, `Once`, `WaitGroup`,
`Barrier`, and `Map` (concurrent string->string). Two combinators are
ordinary functions over the two primitives, not new syntax:
`sync::shield(f)` runs `f` where cancellation does not reach and answers its
value, and `sync::with_timeout(f, ms)` runs `f` under a deadline the caller
computes - which a `timeout:` header, a compile-time constant, cannot take -
answering `Ok(value)` inside the bound and an `Err` naming the bound outside
it. `std::thread`
provides scheduling hints and CPU introspection only: user code has no
OS-thread spawn API. Use `cohort { }` with `spawn(f)`.

**Closures**: `|x: T| body`; capture is automatic (no `move`).
Use `Fn(args) -> ret` for callback parameters. Plain `fn(args) -> ret`
is a raw pointer shape; bare named functions coerce to `Fn(...) -> ...`
at callback sites (no FnMut/FnOnce distinction in practice).
**Capture splits by type**: a container (`Vec`, `Map`, `Set`) is
captured by managed reference, so `xs.push(v)` inside a closure is
visible outside it; everything else - a `Copy` scalar (`i64`, `bool`,
`char`, `f64`), a `String`, a struct - is captured BY VALUE, so
`count += 1` and `s.field = v` update the closure's own copy and the
outer binding does NOT change. To accumulate across a callback -
`fs::walk_dir`, a sort comparator, a visitor - push into a `Vec` (or
collect into a `Map`) rather than writing a scalar or a struct field.

**Iterators**: any type with `fn next(&mut self) -> Option<T>` works
in `for`. Sequence combinators (`map`/`filter`/`take`/`skip`/`step_by`)
are callable as methods/free functions and materialize results.

## 10. Data structures

- `[T; N]` is an owned fixed array, `[T]` is an unsized borrowed slice,
  and `Vec<T>` is the default owned growable sequence. Literal spellings are
  `#[]` for Vec, `[]` for fixed arrays, `{}` for `Map`, and `#{}` for
  `Set` or expected `BTreeSet`; `Queue`, `Stack`, `Deque`, `MaxHeap`, and
  `MinHeap` use `new()` / `from([..])`. Arrays, slices, and Vec share the
  implemented slice method surface, and every one of them reaches
  traversal through `iter()`. Use `Stack` for a
  LIFO-only argument contract instead of a general `Vec`, `Queue` for
  FIFO-only behavior, and `MinHeap` / `MaxHeap` for explicit priority order.
  Only Vec has the full sequence surface: insert, remove, truncation,
  extension, reservation, capacity, and indexed mutation methods. Mutable arrays and slices
  support `sort`, `reverse`, `swap`, and `fill` without resizing. `%i`
  reports each type's real surface, while `%e` also removes methods that the
  binding's mutability cannot call. Method-call
  `xs.insert/remove` mutate in place and return `Result`, with an
  `Err` for an out-of-bounds index. `xs.swap(i, j)` is an indexed write:
  it returns unit and an index outside `[0, len)` panics, exactly as
  `xs[i] = v` does. Qualified calls use the same contract:
  `Vec::insert(&mut xs, i, v)` / `Vec::remove(&mut xs, i)` /
  `Vec::swap(&mut xs, i, j)`.
- **Tuple** `(A, B, ...)` groups a fixed number of values whose types may
  differ. Read and assign positionally (`t.0`, chained `t.0.1`, `t.0 = v`
  through a `mut` binding), destructure in `let` / `for` / `match` / params,
  and compare structurally in declaration order (so `sort` orders a sequence
  of tuples lexicographically). No import; methods are `len`, `is_empty`,
  `get`, `clone`, `to_string`, `into`, `try_into` - `iter()` and its
  combinators are rejected (elements may differ in type, so there is no
  element type to yield). `%i Tuple` documents it and
  `%e <binding>` lists a tuple's elements. Tuple structs are the named
  variant and are fully usable.
- `std::collections`: `Vec`, `Map` (any hashable key by value;
  `iter()` is a cursor over its `(K, V)` pairs, `keys`, `values`,
  `Map::pop`),
  `Set` (unordered; `#{...}` literals, full set algebra, and `iter()` for
  every traversal) / `BTreeSet` (the sorted set), `BTreeMap` (sorted; `String` or `i64`
  keys), `Deque`, `Queue`, `Stack` (each holds whatever a `Vec<T>` holds),
  `MaxHeap`, `MinHeap` (any element the language orders: scalars and `String`
  by value, tuples and structs field by field, sequences lexicographically,
  `Option`/`Result` by arm then payload, an enum by variant rank then payload;
  a `Map`, a `Set`, and a `u64`/`usize` are declined with GT0068).
- Bracket literals create Vec values unless an expected fixed-array type is
  present. A parameter typed `[T]` takes an array, a `Vec`, or another
  view; `&mut [T]` is the form that writes through.
- **`DynValue` is the value whose shape the data decides**: `Nil | Bool | Int
  | Float | Char | String | Bytes | List | Map | Tagged { name, payload }`, a
  prelude type needing no import. Build with `DynValue::int(7)`,
  `::string(s)`, `::list(#[..])`, `::map(keys, values)`,
  `::tagged("Row", #[..])`; read with `kind()` (`nil`/`bool`/`int`/`float`/
  `char`/`string`/`bytes`/`list`/`map`/`tagged`), `name()` (the runtime arm
  name, empty otherwise), `len()`, `at(i)`, `key_at(i)`, and
  `as_i64`/`as_f64`/`as_bool`/`as_char`/`as_str` (each an `Option`) plus
  `as_bytes`. `==` compares contents, an arm by name and payload. A decoder
  answers one without a mirror enum; a Rust binding that declares its arms is
  matched as the ordinary enum spelling the same names.
- **Weak references**: RC means a genuine cycle leaks unless one edge
  is non-owning: `strong.downgrade() -> Weak<T>`,
  `w.upgrade() -> Option<T>`; `runtime::collect_cycles()` runs the
  collector on demand. TRAP: a `Weak` into a member of a genuinely
  strong cycle reads `Some` under `gos` but `None` under `gos
  build` - break real cycles, cross-check with `gos build`.

## 11. Testing

Unit tests live in the file they cover under `#[cfg(test)] mod tests
{ ... }`, reaching items via `super::`. A module belongs to the file
that declares it, so every file in a project may write `mod tests`.
`assert(cond[, msg])` / `assert_eq(a, b[, msg])` are prelude
builtins; `std::testing::check*` record without panicking. **A test
that records no assertion FAILS** - a body that only prints decides
nothing, so it cannot report a pass; a test declared
`-> Result<(), E>` is the one exemption, since reaching `Ok` past
every `?` is its verdict. `gos test` also compiles and runs fenced
code in doc comments.

```gossamer
#[cfg(test)]
mod tests {
    use std::testing
    #[test]
    fn add_adds() { let _ = testing::check_eq(&super::add(2, 3), &5, "2+3") }
}
```

## 12. Stdlib map

Full path spelling is validated (GR0005); discover signatures with
`hover` / `gos doc` rather than memory. Modules by area:

- Core: `fmt io env fs path os process time context flag errors
  bytes bufio`. `process` is free functions - `process::run(prog,
  args) -> Result<{stdout, stderr, code}, String>` (real exec, no
  shell; NO `Command` builder - that is Rust-binding-only surface).
  Interactive children: `process::spawn_piped(prog, args) ->
  Result<Child, errors::Error>`; the `Child` drives piped stdio via
  `write_stdin(s)`, `close_stdin()`, `read_line() -> Option<String>`
  (the canonical drain is `while let Some(line) = child.read_line()`),
  `read_stdout()`, `wait() -> Result<i64, _>`, `kill()` - the
  JSON-RPC-over-stdio (MCP client) shape.
  `time`: `now_ms`, `Instant`, `Duration`, `sleep`, `after`,
  RFC 3339 parse/format. `flag`: `Set::new` + `string/int/bool`
  cells that auto-deref; no built-in required-flag.
- Text: `strings strconv utf8 unicode regex`. Prefer `to_lowercase` /
  `to_uppercase`; `split_once`, `substring(a, b)` clamps, `byte_at(i)`
  zero-fills OOB; Unicode 16 + UAX #31 identifiers (`let café = 1`);
  dynamic regex captures are positional in Gossamer code. Regex Unicode
  mode supports Unicode properties, `\w`, case folding, captures,
  replacement, and split. It does not normalize or group grapheme
  clusters, and match positions are UTF-8 byte offsets.
- Collections: section 10. Prelude scalar `min`/`max`/`clamp`
  (vec-shaped `min(xs)`/`max(xs)` return `Option<T>`).
- Encoding: `encoding::{json, yaml, toml, xml, csv, base64, base32,
  ascii85, hex, pem, binary}`. Typed serde is free functions with a
  turbofish - `from_json::<T>(&text)?` / `to_json::<T>(v)` (same for
  yaml/toml). Struct fields may be scalars, `String`, `Option<T>`,
  tuples, `Map<String, V>`, nested structs, and `Vec<T>` of those;
  fixed arrays/slices are rejected with a type diagnostic. For dynamic or
  partially-known shapes use `json::parse` + `get`/`at`/`as_i64`/
  `as_str`/`keys`/`len`. Unknown JSON keys are ignored.
- Web: `net` (Tcp/Udp/Unix sockets, `url`, `netip`, `ip`), `http`
  (client: `Client::builder()` or free `http::get/post/..`,
  `stream` for SSE; server: `http::serve(addr, handler)`,
  `Response::text/json`, `raw_body`, bodies cap 1 MiB),
  `http::router` (verb methods chain; params via
  `r.path_value("name")` / `r.path_int("id")`), `http::websocket`
  (no `wss://` yet), `http_h3`, `http::static_files`, `http::proxy`,
  middleware/session/csrf/form/multipart/state/health, `html`,
  `mime`. Experimental: `html::template::render_json`, `tls`,
  `database::sql` (drivers via `[rust-bindings]`).
- Misc: `sort`, `math::{rand, big}`, `crypto::{rand, sha256, sha512,
  hmac, blake3, aead, ed25519, ecdsa, x509, kdf, password
  (Argon2id), subtle}` (`crypto::insecure` = MD5/SHA1 compat only),
  `hash::{fnv, crc32, adler32}`, `uuid` (v4/v7), `jwt`,
  `compress::{gzip, flate, zlib, zstd, bzip2}`, `archive::{zip,
  tar}`, `metrics`, `trace`, `slog`, `lifecycle`, `validate`,
  `testing`, `runtime` (`collect_cycles`, `set_panic_hook`, `cohorts()` -
  one descriptor line per live cohort - and `root()`, that line for the
  root),
  `pprof` (`goroutine_profile` / `mutex_profile` / `block_profile` in
  `go tool pprof` text form, `execution_trace(millis)` as Chrome trace
  JSON, `route(path, query)` for `/debug/pprof/...`).

```gossamer
use std::http
use std::http::router

let r = router::Router::new()
    .get("/", handler_fn)
    .get("/ping", |_r| Ok(http::Response::text(200, "ok")))
http::serve("0.0.0.0:8080", r)?
```

## 13. Project layout

```
project.toml       # [project] id/version/gossamer-version;
                   # [dependencies]; optional entry = "src/app.gos";
                   # [rust-bindings] for native Rust crates
src/main.gos       # binary entry (lib.gos for libraries;
                   # subdir/mod.gos for module `subdir`)
tests/             # integration tests
```

A version requirement has two spellings and no third: `x.y.z` (or
`=x.y.z`) pins that version, and `^x.y.z` accepts it or anything later.
This holds for `[dependencies]` and for `gossamer-version`, which names
the toolchain the project is written against - a pin there means that
toolchain and no other, and `gos new` scaffolds `^` so a project keeps
building across releases.
Native Rust is reached ONLY through a binding crate named under
`[rust-bindings]`: `gos new ID --template binding` scaffolds one, its
`pub fn`s live inside a `#[gos_module("name")]` block (keep `use`
imports outside it), and `gos bindgen FILE` drafts one from existing
Rust. `#[gos_opaque]` on an `impl` publishes handle-taking methods
under the type's name, `#[derive(GosStruct)]` passes a struct by
value, and `#[gos_blocking]` moves a long sync call off the scheduler.

A multi-package checkout uses `[workspace] members = ["packages/*"]`
at the root (`gos new ID --template workspace`). Only the resolved
entry file may carry top-level statements.

## 14. Sharp edges and tier notes

The surface runs bit-identically across the VM (`gos`/`gos
test`), the Cranelift JIT, and LLVM AOT (`gos build`); a divergence
you can reduce is a bug - check against both `gos` and `gos
build`. Known sharp edges:

- `+` on `String` copies; heavy assembly wants `bytes::Builder` or a
  `mut String` with `+=`.
- Method dispatch is type-directed for user methods, core
  `String`/`Map`/`Vec` receivers, and typed stdlib receivers.
  Qualified paths (`Point::origin()`) remain the most explicit form
  when several types intentionally share a method name.
- `Weak` into a strong cycle is Experimental (section 10); break real
  cycles explicitly and do not depend on liveness inside a cycle.
- Not implemented (parse or reject cleanly): `async`/`await`,
  explicit lifetimes, the `move` keyword (capture is automatic).
  `gos feature-status` lists Experimental/Planned surface.

## 15. The `gos` toolchain

Bare `gos` opens the REPL. `gos run FILE [ARGS...]` runs a source file; in a
project, `gos run .` / `gos build` resolve the entry themselves. Every token
after `FILE`, including `--`, is a program argument. Put `gos run` options
before `FILE`.

The REPL starts with `gos <version> REPL [<architecture>-<os>]` and uses the
`>>>` prompt. Expression output is the value only, with no numbered markers.
Binding and declaration progress is quiet unless `-v` is enabled. `%help`
lists commands. `%info`/`%i` answers one public symbol or session name exactly,
showing item help plus a type's fields, implemented traits, and methods (and a
trait's implementors); a `*` widens the name to a prefix (`Set*`), a suffix
(`*Set`, which also reaches `BTreeSet` and `flag::Set`), or a substring
(`*Set*`), and a module item is named through its module (`fs::read_to_string`).
`%explain`/`%e` answers the same for a binding in receiver form;
`%bindings`/`%b`, `%declarations`/`%d`, and `%history`/`%h` inspect
the session, `%reset`/`%r` clears it, and `%quit`/`%q` exits. Tab completes
the word at the cursor: a keyword, a `module::item` path, every member a
binding reaches after `.` (the methods its type and mutability can call, a
struct's fields, a tuple's positions), and a type's own surface after `::` -
what `%explain` lists for a binding is what completion offers. Up/down cycles
history; Enter continues until braces close; Ctrl-D also exits. Meta-command
output wraps to the terminal width, capped at 80 columns.

| Command | Purpose |
|---------|---------|
| `gos check [--fix] / parse / build FILE`; `gos run FILE` | Typecheck; AST dump; fast native build; VM+JIT execution. `--fix` applies the rewrites the diagnostics carry, keeping only edits a re-check proves better. |
| `gos build --release [--target T]` | Full LLVM `-O3` (static-musl on Linux); cross to `{x86_64,aarch64}-unknown-linux-{gnu,musl}`. |
| `gos test / bench PATH` | `#[test]` / `#[bench]`; `--coverage`, `--parallel N`, `--format junit`, `--tier-parity`. |
| `gos fmt [--check] / lint [--fix] / doc / explain CODE` | Format; lints; item docs; diagnostic rationale. `gos doc std`, `gos doc std::<module>`, and `gos doc std::<module>::<item>` answer from the stdlib manifest. |
| `gos mcp / lsp / repl` | MCP server for agents; stdio LSP for editors; REPL. |
| `gos new / init / add / remove / tidy / fetch / vendor / publish` | Scaffold (`--template bin\|lib\|service\|workspace\|binding`) and package management: `fetch` / `vendor` prepare git, registry, and tarball dependency sources for an Ed25519-signed registry. |
| `gos watch / clean / env / completion / bindgen / feature-status` | Re-run on change; caches; toolchain info; shell completions; Rust-binding skeletons; feature registry. |
| `gos skill-prompt` | Print this card (`gos skill-prompt \| claude --append-system-prompt`). |
| `--comptime-io=none\|confined\|full` | Capabilities a `comptime` region may reach while compiling. `confined` is the default: reads under the source tree, everything else denied. A denial is `GX0010`. `project.comptime-io` pins it; the more restrictive of manifest and command line wins. |

## 16. When in doubt

Run it (section 2). Spec: `SPEC.md`. Style: `GUIDELINES.md`.
Examples: `examples/` - start with `hello_world.gos`,
`function_piping.gos`, `concurrency.gos`.
