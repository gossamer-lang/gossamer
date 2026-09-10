# Gossamer Language Specification

> Status: pre-1.0.0 draft. Models the current Gossamer language -
> a language targeting the 2026+ Rust ecosystem. The CLI toolchain is
> `gos` (single unified binary in the spirit of `go` or `cargo`).
> Source files use the extension `.gos`. The manifest file is
> `project.toml`; the lockfile is `project.lock`.

---

## 0. Contract status

This document distinguishes four active lifecycle levels:

- **Stable**: a feature named Stable by `gos feature-status` has a
  compatibility commitment under §17. Stable language syntax and semantics in
  this document are normative for the active edition.
- **Shipped**: the feature is included in release artifacts and documented,
  but is not yet protected by the Stable compatibility commitment.
- **Experimental**: the syntax or API is implemented enough to be exposed,
  but may change incompatibly, gain limits, or be withdrawn in a later
  release. Experimental APIs are not part of the 1.0 compatibility promise.
- **Planned**: documentation of an intended direction only. Planned surface
  has no implementation or compatibility commitment.

`Removed` entries are retained as historical records so tooling can explain a
withdrawn path. They are not active surface.

The generated `gos feature-status` registry is authoritative for the status
of individual language and standard-library entries. This specification is
authoritative for the semantics of Stable language constructs; it does not
silently promote a manifest entry merely because it is documented here.

Until a 1.0 release designates a core-library set explicitly, standard-library
modules default to Experimental. Network protocols, databases, templates,
archive formats, process launching, and platform drivers remain Experimental
regardless of examples elsewhere in this document. In particular, compatibility
aliases do not become Stable merely because they remain wired.

---

## 1. Introduction

Gossamer is a general-purpose, automatically memory-managed, statically
typed programming language with first-class concurrency. It supports the
explicit target tiers in §11.1, compiles to a single self-contained binary, and
shares Go's runtime model (M:N goroutine scheduler, channels, automatic
memory management). Its surface syntax, type system, and error-handling
discipline are taken from Rust (2024 edition): `fn` declarations, `let`
bindings, `struct`/`enum`/`trait`/`impl`, pattern matching, `Option` and
`Result`, the `?` operator, monomorphized generics.

Gossamer deliberately omits:

- Lifetimes and the borrow checker (automatic memory management
  removes the need).
- Manual memory management and `Drop` semantics tied to stack frames.
- `nil`/`null` of any kind.
- Raw pointers in safe code.
- Exceptions.

The language is designed so that:

- **Character economy.** Fewer keystrokes per idea. Gossamer aims to
  be a human-friendly and an AI-friendly language. Given two
  equally-clear forms, the shorter one wins.
  Concrete consequences:
  - One line-comment syntax (`//`), not three (`//`, `///`, `//!`).
  - One block-comment syntax (`/* */`), not three.
  - Short keywords where unambiguous (`fn`, not `function`; `use`, not
    `import`; `mut`, not `mutable`).
  - Punctuation over keywords when equally clear (`|>` over `pipe`).
  - No ceremonial scaffolding: no empty `init()` functions, no class
    boilerplate, no annotation blocks where a sigil suffices.
  This principle resolves style disputes: when two forms are otherwise
  equivalent, pick the one with fewer characters.
- **Expressiveness via functional combinators.** Character economy
  cuts ceremony out of programs; expressiveness keeps the remaining
  programs at the expression level instead of dropping them to
  procedural loops with named accumulators. The forward pipe `|>`
  (§4.6) is one half of this; a comprehensive `std::iter` /
  `std::option` / `std::result` surface (§10.4) is the other. The
  rule of thumb: a data transformation should read top-to-bottom as
  a chain of named stages, not as a `for` loop building up a `let
  mut` accumulator. `for` loops keep their place for side-effects
  and complex state; transformations should compose. Every stdlib free
  function takes its data first, so `iter::map(xs, f)` reads as the
  `xs.map(f)` it stands for, and a `|>` step names the slot the value
  fills so `x |> |v| f(a, b, v)` reads as `f(a, b, x)`.
- A single pass over the source file classifies it into tokens.
- A single recursive-descent parser produces an AST without
  context-dependent parsing tricks beyond bounded lookahead.
- The type system is decidable and cheap to check (no higher-rank types,
  no higher-kinded types, no type-level computation beyond const
  evaluation of sizeof-like queries).
- Compile-speed goals are measured by the checked-in performance suite. Native
  debug and release builds both use LLVM; Cranelift is reserved for in-process
  JIT tier-up under `gos`.

Notation follows the Go specification's EBNF conventions. Lowercase
productions are lexical terminals; CamelCase productions are grammatical
non-terminals.

---

## 2. Source representation

Source files are UTF-8 encoded Unicode and have extension `.gos`. Files are
module-oriented; they do not contain a Go-style `package` declaration. Project
identity and dependencies are declared in `project.toml` (§6.4 and §16).

```
newline        = U+000A
unicode_char   = any Unicode code point except newline
letter         = unicode_letter | "_"
decimal_digit  = "0" ... "9"
```

Whitespace is any sequence of U+0020, U+0009, U+000D, U+000A. Statements are
separated by a newline after expressions that do not continue on the next
line, by a surrounding block delimiter (`{ ... }`), or by a semicolon between
two statements on the same authored line. A semicolon is a separator, never a
terminator: it is invalid before a newline, `}`, or end of input.

### 2.1 Comments

- Line: `// ... <newline>`
- Block: `/* ... */` (may not nest).

There is no separate doc-comment syntax. A run of `//` comments
immediately preceding an item (no blank line between) is its
documentation; a run of `//` comments at the very top of a file is the
module's documentation. Tooling reads these by position. This keeps one
comment form instead of three (`//`, `///`, `//!`).

### 2.2 Tokens

Four classes: identifiers, keywords, literals, punctuation. The longest
legal match rule applies.

### 2.3 Identifiers

```
identifier = letter { letter | unicode_digit } .
```

Identifiers are case-sensitive. `_` alone is the "discard" pattern and
is not a binding.

Visibility follows Rust: items are private by default. Public items use
the `pub` keyword, and `pub(package)` restricts an item to the declaring
package (§6.3a). Gossamer does not use Go's capitalization-based
visibility rule.

### 2.4 Keywords

Reserved:

```
as        async     await     break     const     continue
crate     else      enum      extern    false     fn
for       if        impl      in        let       loop
match     mod       mut       pub       return    self
Self      static    struct    super     trait     true
type      unsafe    use       where     while     yield
```

Reserved but currently unused (future extensions): `async`, `await`,
`crate`, `yield`, `extern`, `package`.

Contextual, never reserved:

```
arena     cohort    comptime  defer     newtype   packed    select
```

Each of these is a keyword only where its construct can start and an
ordinary identifier everywhere else, so a binding, field, parameter,
method, or function may carry any of these names. What claims each one:

| Word | Claimed when |
|---|---|
| `arena` | directly before `{`, in statement position |
| `cohort` | directly before `{`, or before a parenthesised header followed by `{` |
| `comptime` | directly before `{`, before `fn`, or before a parameter that is not its own `:` |
| `defer` | in statement position, directly before a name, a keyword, or `{` |
| `newtype` | directly before the alias name |
| `packed` | directly before `enum` |
| `select` | directly before `{` |

`use` is the sole path-binding keyword; there is no `import`. `package`
is reserved but has no role - source files do not declare a package;
see §6. `move` is **not** a keyword: Gossamer has no ownership
transfer, so the Rust-style `move` closure qualifier would be
meaningless. Closures capture by managed reference for the
containers (`Vec`, `Map`, `Set`) and by value for everything else, with
no opt-in needed.

### 2.5 Operators and punctuation

```
+  -  *  /  %
&  |  ^  <<  >>
+= -= *= /= %= &= |= ^= <<= >>=
=  ==  !=  <  <=  >  >=
!  &&  ||
|>                                  // pipe (F#-style forward pipe)
.  ..  ..=  ...  ::  ->  =>
(  )  [  ]  {  }
,  ;  :  ?  #  @
```

Unlike Rust, Gossamer does not use `&` to mean "borrow" - `&expr` takes
a managed reference (see §4.3). As a prefix, `*expr` dereferences a
managed reference (`&T -> T`); it works anywhere, not only inside an
`unsafe` block. Regular method/field access auto-dereferences, so an
explicit `*` is rarely needed.

### 2.6 Literals

Integer, float, string, char (rune), bool, unit (`()`).

```
int_lit     = decimal_lit | bin_lit | oct_lit | hex_lit
decimal_lit = digit { digit | "_" }
bin_lit     = "0b" bin_digit { bin_digit | "_" }
oct_lit     = "0o" oct_digit { oct_digit | "_" }
hex_lit     = "0x" hex_digit { hex_digit | "_" }

float_lit   = decimal_digits "." decimal_digits [ exponent ]
            | decimal_digits exponent
            | "." decimal_digits [ exponent ]
exponent    = ( "e" | "E" ) [ "+" | "-" ] decimal_digits

char_lit    = "'" ( unicode_char | byte_escape | unicode_escape ) "'"

string_lit  = "\"" { string_char | escape } "\""
triple_str  = "\"\"\"" { string_char | escape | newline } "\"\"\""
raw_string  = "r\"" { raw_char } "\"" | "r#\"" { raw_char } "\"#"

byte_lit    = "b'" byte_char "'"
byte_string = "b\"" { byte_char } "\""
```

A triple-quoted literal that spans lines strips the indentation its
body shares with its closing delimiter. Only whitespace may follow the
opening `"""` on its line (`GP0033` otherwise), and the newline after the
opening delimiter is not part of the value. When the closing `"""` sits
on a line of its own, that line contributes its whitespace to the
measure and the newline before it is not part of the value; when content
sits immediately before the closing delimiter, the value's last line has
no trailing newline. The measure is the longest leading-whitespace
prefix shared by every non-blank content line and by the closing
delimiter's line, compared as text so tabs and spaces never merge. A
whitespace-only line becomes empty. Escapes are decoded after the
indentation is removed, so `\n` in the body is a newline escape rather
than a line break. A single-line `"""..."""` takes its body verbatim.

Literal suffixes disambiguate type:

- `42i32`, `42u64`, `42usize` - typed integer literals.
- `3.14f32`, `3.14f64` - typed float literals.
- Untyped literals default to `i64` / `f64` unless context infers otherwise.

Integer literals may contain `_` as a visual separator anywhere after
the first digit and not adjacent to the decimal point or exponent
marker.

### 2.7 Statement termination

A block is a sequence of statements and an optional trailing expression.
Statements are either ended by `;` or are self-terminated by their
trailing `}` (for control-flow constructs). An expression without a
trailing `;` at the end of a block is the block's value. This mirrors
Rust 2024 exactly.

Example:

```
fn abs(n: i32) -> i32 {
  if n < 0 { -n } else { n }          // trailing expression, no ';'
}
```

Unlike Go, Gossamer neither inserts nor accepts `;`. The lexer emits tokens
verbatim; the parser uses whitespace, newlines, and delimiters as separators.

Delimited lists use commas on one line and newlines across multiple lines.
This applies to every delimited list: function parameters and arguments,
closure parameters, struct fields and literals, enum variants and payload
fields, tuples and tuple types, `Vec` / array / `Map` / `Set` literals, tuple,
slice, and struct patterns, generic parameters and arguments, and `use` lists.
A comma at the end of a multiline entry is accepted for migration, but
`gos fmt` removes it.

A newline separates elements only in positions where a comma would, so a
parenthesised expression spanning lines stays that expression: `(\n a + b \n)`
is the sum, not a one-element tuple. The one-element tuple is still written
with its comma, `(a,)`.

One narrow newline rule disambiguates the three operators that are
also unary prefixes (`&`, `*`, `-`): when one of them appears as the
first non-whitespace token on a new line, it begins a new statement
rather than continuing the previous expression as a binary operator.
So:

```
let s = read_file(path)?
&s |> strings::lines |> |v| iter::for_each(v, handle)   // two statements
```

parses as a let followed by a pipe-expression statement, not as
`let s = read_file(path)? & s |> ...`. Multi-line continuation of
those three operators still works when the operator sits at the end
of the previous line (`let x = a -\n  b`) or inside parentheses.
The other binary operators (`+`, `&&`, `|>`, `==`, …) continue across
newlines unconditionally.

> **Gotcha - leading `&` / `*` / `-` starts a new statement.** A line break
> before one of these three operators is **not** a continuation. Splitting a binary
> expression as
>
> ```
> let total = subtotal
>     - discount        // parsed as a new statement `-discount`, NOT a subtraction
> ```
>
> silently changes the meaning (here `total` binds to `subtotal` and
> `-discount` becomes a separate, discarded statement). Keep the
> operator at the **end** of the previous line, or wrap the expression
> in parentheses:
>
> ```
> let total = subtotal -
>     discount          // continues correctly
> let total = (subtotal
>     - discount)       // continues correctly
> ```

An entry file's top-level statements follow the same termination rules.
They form the body of an implicit `fn main` (§6.10) and have no
tail-expression value: a trailing bare expression is an ordinary
statement whose value is discarded, and the implicit `main` returns
`()` unless a `?` operator forces `Result<(), errors::Error>`.

---

## 3. Types

### 3.1 Built-in primitive types

- Signed ints: `i8`, `i16`, `i32`, `i64`, `i128`, `isize`.
- Unsigned ints: `u8`, `u16`, `u32`, `u64`, `u128`, `usize`.
  (`i128` / `u128` are reserved spellings, rejected with `GT0014` -
  see the conformance note below.)
- Floats: `f32`, `f64`.
- `bool` (1 byte).
- `char` - a 32-bit Unicode scalar value (not a surrogate).
- `()` - the unit type, inhabited by the value `()`.
- `!` - the never type (uninhabited; result type of `panic`, `return`,
  infinite loops).

**The i64 runtime model.** Every integer type of 64 bits or less
(`i8`-`i64`, `u8`-`u64`, `isize`, `usize`) is represented at runtime
as a 64-bit signed value, on every tier. Arithmetic, comparison,
division, remainder, and shifts all run at 64-bit signed width; the
declared narrow or unsigned width is observable only at an explicit
`as` cast, which truncates to the declared width and then extends by
the target's signedness (`300 as u8 == 44`, `200 as i8 == -56`).
Consequences of the model:

- `+`, `-`, and `*` follow Rust's profile-sensitive integer overflow
  behavior at the declared type width. Debug execution, including `gos`
  and `gos build`, panics on overflow. `gos build --release` wraps at the
  declared width, so a release `200u8 + 200u8` evaluates to `144`.
- `u64`/`usize` values use the same 64-bit payload as signed integers,
  but arithmetic, comparison, shifts, division/remainder, and display are
  type-aware on every tier. Casts reinterpret or truncate to the target
  width (`(0 - 1) as u64` prints `18446744073709551615`); an explicit
  cast back to a signed type reinterprets the same bits.
- `<<` and `>>` mask the shift amount to the low 6 bits
  (`1 << 70 == 1 << 6`); `>>` is the arithmetic (sign-propagating)
  shift.
- Float → int casts saturate at i64 width with no narrow mask
  (`300.7 as u8 == 300`, `1e20 as i64 == i64::MAX`, NaN → 0).

The VM, Cranelift JIT, and LLVM debug backend all enforce the same checked
behavior. Explicit `wrapping_add` and `wrapping_mul` retain wrapping behavior
at the declared integer width in every profile. Other Rust integer arithmetic
method families are not yet part of Gossamer's public method surface.

`i128` and `u128` are not supported on any tier. The checker rejects
every spelling of these types at the declaration site with a
compile-time error (`GT0014`), so `gos`, the JIT, and `gos build`
all fail identically - there is no interpreter-only acceptance and no
silent 64-bit narrowing.

Silent surprise is never part of the contract - the behaviour is
explicit two's-complement arithmetic at 64-bit width, not
"undefined."

**No implicit numeric widening.** All numeric conversions - widening or
narrowing - require an explicit `as` cast. `let bigger: i64 = small_i32`
is a type error; write `let bigger = small_i32 as i64`. This prevents
silent truncation, silent sign changes, and surprise precision loss.

**The `as` whitelist.** `as` is whitelist-checked (`GT0005`). The
permitted shapes are: numeric ↔ numeric (any integer or float type on
either side, `f32` sources included; float → int truncates toward zero
and saturates as above), `bool` → integer, `char` → integer, `u8` →
`char`, and same-type no-ops. Every other `as` shape is a compile-time
error.

### 3.2 Strings

`String` is a runtime-managed, growable UTF-8 string. It is mutable
through `push_str`, `+`, and similar methods. Because `String` is
runtime-managed, there is no `&str`/`String` split and no lifetime
parameter. String literals have type `String`.

`char` is a 32-bit Unicode scalar value. A `String` is not indexable by
`char`; iteration is via `.chars()` (an iterator of `char`) or
`.bytes()` (an iterator of `u8`). Byte-level substring operations go
through `.as_bytes()` which returns an owned `Vec<u8>`.

### 3.3 Collections (built-in generic types)

| Type | Semantics |
|---|---|
| `Vec<T>` | Owned growable sequence. |
| `[T; N]` | Owned fixed-size array. The length is part of its type. |
| `[T]` | Unsized slice. Ordinarily used as `&[T]` or `&mut [T]`. |
| `Map<K, V>` | Hash map. Analogue of Go's `map[K]V`. |
| `BTreeMap<K, V>` | Ordered map. |
| `Set<T>` | Unordered set. |
| `BTreeSet<T>` | Ordered set. |
| `Deque<T>` | Double-ended queue. |
| `Queue<T>` | FIFO queue. |
| `Stack<T>` | LIFO stack. |
| `MaxHeap<T>`, `MinHeap<T>` | Priority heaps. |
| `Sender<T>`, `Receiver<T>` | Channel endpoints. Always come as a pair from `channel<T>()`. |

Arrays and Vec use value semantics. A writable Vec copy has independent
storage, including nested Vec elements. Slices are non-owning lexical views,
and Vec is the only sequence type that owns growable storage.

#### Collection literals

`#[a, b, c]` creates a `Vec<T>`. `[a, b, c]` and `[value; N]` create fixed
`[T; N]` arrays; `N` must be a compile-time constant. An expected type can
shape either spelling into the other where the shapes agree. The repeat form
follows the same spelling as the list form: `[value; N]` is a fixed array and
`#[value; N]` is a `Vec`.

`Queue`, `Stack`, `Deque`, `MaxHeap`, and `MinHeap` have no literal form and
are constructed through their type, with `T::new()` for an empty container and
`T::from([a, b, c])` from a Vec literal. The retired `<[a, b]`, `[a, b]>`,
`^[a, b]`, and `_[a, b]` spellings are `GP0032`.

```gossamer
let words = ["yes", "wow"]
let fixed = #["m", "n"]
let zeros = #[0; 4]
fn count(xs: [String]) -> i64 { xs.len() }
let names: [String; 2] = ["m", "n"]
count(names)
let map = {"ada": 36, "grace": 37}
let set = #{"compiler", "runtime"}
let ordered: BTreeSet<String> = #{"compiler", "runtime"}
```

Different array lengths are different types and do not silently join to Vec.
There is no implicit conversion between an owned array and Vec.

Shared and mutable references unsize in the same four places as Rust:
`&[T; N]` and `&Vec<T>` coerce to `&[T]`; `&mut [T; N]` and
`&mut Vec<T>` coerce to `&mut [T]`. Arrays, slices, and Vec share the
implemented slice surface, including queries, checked indexing helpers,
conversion with `to_vec`, and in-place ordering methods. Eager collection
combinators such as `map`, `filter`, `fold`, `sum`, and `take` are Vec methods;
arrays and slices use `iter()` before applying iterator combinators. Only Vec
exposes length- or capacity-changing methods. A mutable slice can modify
existing elements and use non-resizing mutable methods such as `swap`, `sort`,
`sort_by`, `sort_by_key`, `reverse`, and `fill`.

`.slice(start, end)` is a checked copying operation that returns
`Result<Vec<T>, errors::Error>`. It is not a borrowed sub-slice. Gossamer does
not currently expose an escaping sub-slice value. Sequence iteration yields
managed element values rather than element references; the iterator retains
its source state and detects structural mutation.

### 3.3a Slicing

A range in index position takes a subsequence:

```
let xs = #[1, 2, 3, 4, 5]
xs[1..3]     // #[2, 3]
xs[..2]      // #[1, 2]
xs[3..]      // #[4, 5]
xs[..]       // the whole sequence
xs[1..=3]    // #[2, 3, 4]
```

Arrays, slices, and `Vec` all accept it, and so does `String`.

**Bounds clamp; they do not panic.** `xs[1..99]` yields the elements that
exist. This is deliberate and differs from element indexing, which panics:
an out-of-range single index has no sensible answer, while an out-of-range
range has exactly one - the part that overlaps. It is also the rule
`substring` already follows, and slicing a `String` *is* `substring`.

**A `String` slice takes byte offsets** (§3.2), the same index space
`substring`, `byte_at`, and `as_bytes` use, and snaps outward to codepoint
boundaries so the result is always valid text. `s.len()` and `s[i]` count
Unicode scalars, so do not mix the two spellings on non-ASCII text.

### 3.4 Pointers and references

- `T` is a value. Primitive values copy directly. Owned arrays and Vec values
  remain usable after assignment or a by-value call. Each writable Vec copy
  has independent storage, including recursively nested Vec elements. There
  is no source-level ownership transfer or `move` keyword. A by-value
  container argument is given that storage where the callee could write it or
  let something derived from it outlive the call; a callee that provably does
  neither reads the caller's storage, so the call costs nothing the value's
  length.
- `&T` is a shared, non-owning lexical view. It cannot be null. It permits
  reads but not writes to the source place. It is not a **parameter** type:
  an argument is passed without copying whatever its type, so the sigil
  names no choice there and reports `GP0054`, as a shared `&` written on an
  argument reports `GP0055`. It remains a binding (`let view = &values`)
  and a pattern.
- `&mut T` is an exclusive, non-owning lexical view. It requires a writable
  source place and writes through to that place. It is the one reference a
  parameter takes, and it is spelled `&mut` at the call site too. A
  reference reaching a parameter that takes the value is `GT0082`, with the
  `*x` rewrite: a reference names a value rather than being one.
- `*` reaches the place a reference names. Over a value there is no such
  place, so a write through one - `*x = v` where `x` is an `i64` - is
  `GT0083`; the read is the value itself.
- `[T]` is the sequence view a parameter names directly, and it accepts an
  array, a `Vec`, or another view. `&mut [T]` is the form that writes
  through to the sequence it views.
- A method receiver is `&self` (reads) or `&mut self` (writes). `&self` and
  `self` name the same value, since a receiver is reached without copying
  either way; only `&mut self` writes back.

References are deliberately restricted because the runtime does not attach an
owning backing-allocation handle to every reference representation. A named
reference may borrow only a stable named place. A direct call may borrow a
temporary for the duration of that call. A reference cannot be returned,
nested in an owned local, field, container, channel, or closure, or sent to a
goroutine. The only reference return exception is a shared `&str` expression
proved to consist entirely of static string literals. These rules are checked
with GT0052.

Shared and mutable named references remain active until their lexical scope
ends. Any active reference prevents mutation through the source binding, and
an active mutable reference also prevents reads through the source binding.
Taking another mutable reference conflicts with either kind of active view.
GT0053 reports these conflicts. This is intentionally lexical rather than
non-lexical: put a view in a smaller block when source access must resume.

A mutable reference binding may be rebound directly to another stable named
place; the lexical record moves to the new root. Rebinding through another
reference or from a temporary is rejected.

Reference patterns are available wherever patterns are accepted. `&p` matches
a shared reference, `&mut p` matches a mutable reference, and each form removes
one reference layer before matching `p`. The reference mutability must match
exactly: `&p` does not match `&mut T`, and `&mut p` does not match `&T`.
Extracting through a reference pattern follows Gossamer's normal value-copy
semantics. Scalars and aggregates are copied into independent values; the
pattern does not create another alias to the referent.

Raw pointers (`*const T`, `*mut T`) are **not** part of the language
today: the type spellings do not parse (`GP0001`), and there is no safe
or unsafe way to construct one in Gossamer source. FFI goes through the
`gossamer-binding` ABI (§12), not raw pointers. (The `unsafe` keyword
parses - see §8.7 - but grants no extra powers, because there is nothing
unsafe to do.)

`&T` and `&mut T` have implicit lexical lifetimes. A named reference remains
active from its declaration through the closing brace of that scope. Gossamer
does not shorten this interval at the reference's last use, and safe code has
no explicit lifetime annotations. Their safety comes from the restrictions
above, not from automatic reference counting. Reference counting a container
is insufficient when a view points into storage that the container can
replace.

**`&mut` parameter semantics.** A `&mut T` parameter writes through to
the caller's storage on every tier, including scalar values, strings,
vectors, slices, structs, enums, and fixed-size `[T; N]` arrays. Element
writes, growth via `push` for growable vectors, `swap`, forwarding the
reference into a nested call, early-return paths, a struct field as the
argument place, and writes from a closure taking the parameter are all
visible in the caller's binding after the call returns. Calls do not create
mutable references implicitly. A writable place must appear as `&mut place`
at the call site; an expression already typed as `&mut T` can be forwarded
directly. Passing the same root twice as `&mut` in one call
(`f(&mut v, &mut v)`) is rejected. References cannot cross a goroutine
boundary, so `spawn(|| f(&mut v))` is rejected.

### 3.5 Function types

```
FnType = "fn" "(" [ TypeList ] ")" [ "->" Type ]
       | "Fn" "(" [ TypeList ] ")" [ "->" Type ]       // closure trait
       | "FnMut" "(" [ TypeList ] ")" [ "->" Type ]
       | "FnOnce" "(" [ TypeList ] ")" [ "->" Type ]
```

Plain `fn(...) -> ...` is a raw function-pointer shape. A bare named function
coerces to a compatible `Fn(...) -> ...` callback when passed to a higher-order
function or sequence combinator. `Fn`, `FnMut`, `FnOnce` are closure traits (as
in Rust). Closures that capture the environment satisfy the appropriate
closure trait and are heap-allocated. Closure capture does not currently
distinguish shared from exclusive environment access, so `Fn` and `FnMut`
collapse into essentially the same constraint; the distinction is retained
for readability and forward compatibility.

#### 3.5.1 Keyword arguments and parameter defaults

```
Param       = [ "comptime" ] Pattern ":" Type [ "=" ConstExpr ]
CallArg     = [ Ident "=" ] Expr
```

A call may name the parameter each argument fills, and a parameter may
declare a constant default.

```gossamer
fn volume(width: i64, height: i64 = 2, depth: i64 = 3) -> i64 { .. }

volume(2)                              // 2, 2, 3
volume(depth = 4, width = 2, height = 3)  // 2, 3, 4
volume(2, depth = 10)                   // 2, 2, 10
```

Both are spellings at the call site. Between name resolution and type
checking every call is rewritten into the order its callee declares:
named arguments are moved to their parameter's position and a parameter
the call omits has its default spliced in. The calling convention is
unchanged, and the bytecode VM, the JIT, and native builds compile the
identical positional call.

An argument label binds with `=`, not `:`; `:` is the annotation and
struct-literal spelling, and `==` is a single token, so an equality test
in argument position is never read as a label.

A name must name a parameter of the callee and may be given once.
Positional arguments precede named ones: once a name is used, the
positions after it are no longer in written order, so every later
argument needs a name too. Violations are `GR0013`.

A default must be a literal - integer, float, string, char, byte, or bool
- optionally negated. It is spliced into every call that omits the
parameter, so an expression needing resolution at each of those sites is
rejected with `GR0014`. Each call site receives its own copy, so no two
calls share a default value.

Methods and associated functions accept both forms. A method call is
rewritten before its receiver's type is known, so the rewrite must be
independent of which type the receiver has: when several types declare a
method of that name, the call is rewritten only if they agree on
parameter names and defaults, and is otherwise reported as `GR0013`
rather than resolved by guessing. Passing the arguments positionally is
always accepted.

### 3.6 Structs

```
StructDecl = [ "pub" ] "struct" Ident [ Generics ] [ StructBody ] [ WhereClause ]
StructBody = "{" [ FieldList ] "}" | "(" [ TypeList ] ")"
FieldList  = SingleLineFields | MultiLineFields
SingleLineFields = Field { "," Field }
MultiLineFields  = newline Field { [ "," ] newline Field } [ "," ] newline
Field      = [ "pub" ] Ident ":" Type
```

Struct values are allocated inline when they are local variables, but
may escape to the managed heap via escape analysis (any field mutation
through a `&T`, any storage in a channel, any capture by a closure that
outlives the caller, etc.).

Example:

```
pub struct Point { pub x: f64, pub y: f64 }
struct Wrapper { first: i32, second: i32 }
struct Marker
struct Empty {}
struct EmptyTuple()
```

Struct declarations follow Rust's three shapes. A missing body declares a unit
struct, braces declare a named-field struct, and parentheses declare a tuple
struct. Empty named structs use `struct Empty {}` and empty tuple structs use
`struct EmptyTuple()`.

**Functional record update.** A struct literal may spread a base value
with `..base` and override individual fields:

```
let origin = Point { x: 0.0, y: 0.0 }
let shifted = Point { x: 10.0, ..origin }
```

Explicit fields win over the base for the same name; exactly one `..base`
spread is allowed. Fields copied from the base share its heap children and
are retained, so the base stays usable after the update.

A spread references every field the literal does not name, so the whole
struct has to be visible here (§6.3a): a struct with any private field
cannot be updated from outside the module that declares it, exactly as it
cannot be constructed there.

### 3.7 Enums (sum types)

```
EnumDecl = [ "pub" ] [ "packed" ] "enum" Ident [ Generics ] [ ":" UIntType ]
           "{" VariantList "}" [ WhereClause ]
VariantList = SingleLineVariants | MultiLineVariants
SingleLineVariants = Variant { "," Variant }
MultiLineVariants  = newline Variant { [ "," ] newline Variant } [ "," ] newline
Variant  = Ident [ "(" TypeList ")" | "{" FieldList "}" ]
```

Enum values carry a discriminant and the payload of the active variant.
The built-in `Option` and `Result` are defined as:

```
pub enum Option<T> { Some(T), None }
pub enum Result<T, E> { Ok(T), Err(E) }
```

**Representation.** A declaration says how wide the discriminant is
stored, and nothing else about the enum changes with it:

| Declaration | Width |
|---|---|
| `enum Foo { .. }` | the smallest byte-aligned width holding every variant: 8, 16, 32, or 64 bits |
| `enum Foo: uN { .. }` | exactly `N` bits |
| `packed enum Foo { .. }` | the smallest number of bits holding every variant |
| `packed enum Foo: uN { .. }` | exactly `N` bits |

`N` is any unsigned width from `u1` to `u64`, sub-byte widths included;
anything else after the colon reports `GP0050`. A width too narrow for
the variants reports `GT0081` and names the width they need - `packed
enum Dir: u1 { N, E, S, W }` needs 2 bits. Variants number from zero in
declaration order, and the discriminant occupies the low `N` bits of its
storage with the rest zero, on every tier.

`packed` is contextual: it is a keyword only directly before `enum`, so
a binding, field, or function may still be called `packed`.

Everything a program can observe is independent of the choice - the
variant a value names, `==`, ordering, matching, `{}` and `{:?}`,
serialization, use as a `Map` key, and storage in an array or a tuple all
behave the same at every width.

> **Constraint - variant names share the module namespace.** Unlike
> Rust, a variant name is not scoped under its enum: every variant in a
> module occupies the module's top-level name namespace. Two enums in
> the same module therefore cannot both declare a variant with the same
> name (`enum Color { …, C }` and `enum Grade { …, C }` collide with
> `GR0003: the name 'C' is defined multiple times`). Give colliding
> variants distinct names. This is also why method dispatch is largely
> name-global. (`Option` / `Result` are special-cased and do not
> reserve `Some` / `None` / `Ok` / `Err` against your enums.)

### 3.8 Traits

```
TraitDecl = [ "pub" ] "trait" Ident [ Generics ] [ ":" BoundList ] "{" TraitItems "}"
TraitItem = FnSig
          | FnDecl                         // with default body
          | "type" Ident [ ":" BoundList ] [ "=" Type ]
          | "const" Ident ":" Type [ "=" Expr ]
```

Traits support:

- Required and default methods.
- Associated types and associated constants (see below).
- Bounds on trait generics.
- Supertraits (`trait Foo: Bar + Baz`).
- Default methods.

**Associated types.** A trait declares `type Item`, optionally with a
default; each `impl` supplies one concrete type for it. A projection
`Self::Item` or `T::Item` names that type in a signature or a body:

```
trait Holder {
    type Item
    fn get(&self) -> Self::Item
}
impl Holder for Label {
    type Item = String
    fn get(&self) -> Self::Item { self.text }
}
fn shout<T: Holder>(h: T) -> T::Item { h.get() }
```

A projection resolves, in order, to an equality constraint on the bound
(`T: Holder<Item = String>`), to the concrete impl when the base names a
type (`Label::Item`, or `Self::Item` inside an impl), to the trait's
default, and finally to the trait's single implementor. It is a compile
error when several impls supply the item and no equality constraint picks
one (`GT0061`); write the constraint or name the concrete type. A
supertrait's associated items are reachable through the subtrait that
inherits them, since a projection resolves at check time rather than
through dispatch. An impl that omits an associated item the trait
declares without a default is rejected (`GT0059`), and a projection of an
item nothing declares is rejected (`GT0060`). Every projection resolves to a concrete type before
lowering, so all three tiers agree.

Not supported: generic associated types (`type Item<T>`), associated
types on `dyn Trait` (there are no trait objects, §3.11), and inferring a
projection across several candidate impls without a constraint.

**Associated constants.** A trait declares `const MAX: i64`, optionally
with a default; each impl supplies a value. Read one as `Type::MAX`,
`Self::MAX`, or `T::MAX` through a bound:

```
trait Bounded {
    const MAX: i64
    const STEP: i64 = 5
}
impl Bounded for Gauge { const MAX: i64 = 100 }
fn headroom<T: Bounded>(g: T) -> i64 { T::MAX + T::STEP }
```

Each associated constant becomes an ordinary constant, so its value is
folded identically on every tier. `T::MAX` follows the same resolution
order as a type projection: the trait's default, else the trait's single
implementor.

**Trait bounds on generic functions (static dispatch).** A generic
function may bound its type parameters by a trait and call the trait's
methods on a parameter receiver:

```
trait Shape {
    fn name(&self) -> String
    fn area(&self) -> i64
}
fn report<T: Shape>(s: T) -> String {
    format("{}: {}", s.name(), s.area())
}
```

Each call site instantiates the type parameters independently, so one
generic function serves any number of concrete types in a program. The
bound is enforced at the call site: passing a type with no matching
`impl` is a compile error (`GT0017`). A method called on a bound
parameter resolves to the trait method's declared return type. Every
instantiation is monomorphised and the trait-method call is lowered to
the concrete impl's symbol (`Square::name`), giving static dispatch that
is bit-identical across the bytecode VM, the Cranelift JIT, and the LLVM
AOT tiers. A type parameter may carry several bounds (`T: Read + Write`),
and methods resolve from any of them. The arguments are struct-typed;
`dyn Trait`, blanket impls, and supertrait method inheritance through a
bound are not yet part of static dispatch.

Gossamer does **not** support:

- Higher-ranked trait bounds (`for<'a> ...`).
- Trait objects of any kind. There is no `dyn Trait` (§3.11), so
  object-safety / dyn-compatibility rules do not apply - polymorphism
  is monomorphised generic bounds only.

### 3.9 Impl blocks

```
ImplDecl      = "impl" [ Generics ] Type [ WhereClause ] "{" ImplItems "}"
ImplDeclTrait = "impl" [ Generics ] TraitRef "for" Type [ WhereClause ] "{" ImplItems "}"
```

Inherent impls attach methods/associated items to a type. Trait impls
declare that a type satisfies a trait, and define exactly the items the trait
declares: an item outside that contract is `GT0072`, and a `(trait, type)`
pair implemented twice is `GT0073` (§3.13).

Method receivers:

- `fn m(self)` - receives the value by copy (Copy types) or by
  managed reference (heap types). The caller's binding remains
  usable after the call.
- `fn m(&self)` - shared access. With managed references this is
  just "pass the ref".
- `fn m(&mut self)` - writable access. Same runtime as `&self`; used by the
  type checker to forbid mutating method calls on non-writable places. It does
  not provide Rust lifetime or non-lexical-borrow analysis (§7.5).

### 3.10 Generics

```
Generics  = "<" GenericParam { "," GenericParam } ">"
GenericParam = LifetimeParam | TypeParam | ConstParam
TypeParam = Ident [ ":" BoundList ] [ "=" Type ]
ConstParam = "const" Ident ":" Type [ "=" Expr ]
```

Lifetime parameters exist syntactically only for FFI signatures that
mirror Rust crates - they are parsed and ignored by the type checker in
safe code. In normal code, lifetimes are never written.

Generic instantiation in expressions uses the turbofish `::<T>`:

```
let v = Vec::<i32>::new()
let tx, rx = channel::<String>()
```

The bare form `name<T>(...)` is also accepted when the parser can
disambiguate with one-token lookahead after the closing `>` (must be
`(`, `::`, or `{`).

A const parameter over an array length binds the length of a fixed-size
array parameter:

```
fn sum<const N: usize>(xs: [i64; N]) -> i64 {
    let mut acc = 0
    for x in xs { acc += x }
    acc
}
```

`N` is inferred from the array argument's length at the call site and
keyed into monomorphisation, so each distinct length instantiates an
independent specialisation that runs identically on the bytecode VM,
the Cranelift JIT, and the LLVM AOT tiers. The body may iterate the
parameter and read `xs.len()`, the const may appear in the return type
(`-> [i64; N]`), and a function may take more than one const parameter
(`<const N: usize, const M: usize>`). The const is inferred from a
`[T; N]` argument; it is not yet usable as a bare value expression in
the body or as a repeat count (`[0; N]`).

Monomorphisation specialises each `(def, substs)` pair independently
and runs identically on the bytecode VM, the Cranelift JIT, and the
LLVM AOT tiers. A generic parameter may be instantiated with a scalar
or with an aggregate - a struct, tuple, fixed-size array, `Vec<T>`,
`String`, or `f64` - and the aggregate is threaded by value through the
specialisation, including across recursive calls. Generic struct types
(`struct Wrapper<T> { value: T }`) and their `impl<T>` methods
specialise per instantiation on every tier. Bounds are static dispatch
and may be written several to a parameter (`T: A + B`), in the parameter
list or in a `where` clause; there is no `dyn Trait`, no blanket impl,
and no supertrait method inheritance through the bound.

### 3.11 Dynamic dispatch

There is no `dyn Trait` and no trait-object type. `dyn` is not a
reserved word, and the `dyn Trait` type spelling does not parse
(`GP0001`). Polymorphism is provided by generic bounds with static
dispatch (§3.10): `fn f<T: Trait>(x: T)` monomorphises per call site.
A heterogeneous collection is modelled with an `enum` whose variants
carry the alternatives, matched exhaustively.

For closures, the callable trait type `Fn(args) -> ret` (§3.5) is the
one place a value of "some callable" is passed dynamically; it is a fat
pointer, but it is not spelled `dyn`.

### 3.12 Type aliases

```
TypeAlias = "type" Ident [ Generics ] "=" [ "new" ] Type
```

Without `new` the alias is **transparent**: the name is a second spelling
of the target and substitutes for it everywhere, so the two are the same
type and interchange freely. Aliases take parameters
(`type Pair<A> = (A, A)`). An alias that reaches itself is rejected
(GT0024).

```
type Id = i64
type Pair<A> = (A, A)
```

With `new` the alias is **opaque**: it declares a type of its own over an
unchanged representation. `newtype UserId = i64` makes `UserId` and
`i64` distinct, as are two opaque aliases of the same target. The
distinction exists only in the checker - the runtime value is the
representation, so an opaque alias adds no layout, no indirection, and no
cost on any tier.

```
newtype UserId = i64
```

An opaque alias converts to and from its own representation with
`.into()`, in both directions and without work. A fixed array converts
into the `Vec` of its element the same way. Every other pair requires an
`impl From` written for it; reaching for `.into()` without one is GT0066.

The target of `.into()` - and of the fallible `.try_into()`, which reaches
an `impl TryFrom` and answers `Result<B, E>` - comes from the use site,
never from the receiver. An annotated binding, a typed parameter, and a
return position each fix it; a call none of them reach has no target at
all, which is GT0066 as well.

An opaque alias inherits equality, ordering, hashing, and formatting from
its representation, which is what lets it be a `Map` or `Set` key, sort,
and print. It inherits nothing else: the representation's methods
(GT0002) and operators (GT0003) are not part of it, and are reached by
converting or by writing an `impl` on the alias. Inherent and operator
`impl` blocks apply to an opaque alias as to any other type.

A struct field typed by either form of alias serializes as the
representation and decodes back to the alias (§10.3).

### 3.13 Derivable traits

A struct may be annotated with `#[derive(...)]` to have standard trait methods
generated automatically:

```
#[derive(PartialEq, Eq, Default, Debug)]
struct Point { x: i64, y: i64 }
```

The supported traits are `Debug`, `Default`, `PartialEq`, `Eq`, `PartialOrd`,
and `Ord`:

- `PartialEq` / `Eq` - `==` and `!=` compare field-by-field. (`Eq` is a marker
  requiring `PartialEq`.) Note structs / enums already compare by value with no
  derive (§3.12); the derive forces it for generic / container-field types.
- `PartialOrd` / `Ord` - `<` `<=` `>` `>=` order field-by-field (structs) or by
  variant rank then payload (enums); likewise automatic for plain types.
- `Default` - `Type::default()` builds a zero-valued instance (`0` / `false` /
  `""` / `[]` / each field type's own default; skipped when a field type has no
  derivable default).
- `Debug` - `{:?}` renders `Name { field: value, … }`.

A plain struct or enum whose fields all render gets that structural rendering
with no derive at all, on both channels: `Display` (`{}`) and `Debug`
(`{:?}`). A **generic** type does not: what its fields render as depends on
the arguments each instantiation supplies, so the declaration asks for the
rendering with `#[derive(Debug)]`. Formatting a generic type without one is
`GT0062`, on every tier.

`Display` and `Debug` are distinct contracts, exactly as in Rust. Each
declares one method that answers a `String`, and both are written `fn fmt`;
the `impl` header is what says which contract a block supplies. Neither
channel borrows the other's method, so `impl Display for T { fn fmt }` leaves
`{:?}` showing the synthesized shape and `impl Debug for T { fn fmt }` leaves
`{}` showing it. `x.to_string()` renders through `Display`, whether that is
the synthesized rendering or a written one; declaring the rendering as
`fn to_string` inside an `impl Display` block is `GP0053`, with the rewrite
that names it `fmt`.

An `impl Trait for Type` block defines exactly the items the trait declares. A
`fn` the trait does not declare is `GT0072` - it would become an inherent
method under a misleading header, reachable by name but never through the
trait - and belongs in an inherent `impl Type { … }` block or in the trait's
own declaration. One `(trait, type)` pair is implemented once: a second block
for the same pair, or an `impl Debug for T` competing with a
`#[derive(Debug)]` on `T`, is `GT0073`.

`Clone` is **not** derivable (`GT0025`): structs copy by value, so `let b = a`
copies and `a.clone()` is a universal builtin. `Hash`, `Copy`, `Display`, and
serde are likewise automatic; `From` / operators are written `impl Trait for T`.

The methods are synthesized as ordinary Gossamer `impl` source at parse time,
so they compile and run identically on every tier. Fields may be primitives,
`String`, `[T]`, **nested structs** (which derive the same traits), and the
struct may be **generic** (`struct Wrap<T> { … }`).

`#[derive(...)]` also works on **enums**, generic ones included, and on
variants with struct payloads. Tuple (`Circle(f64)`), unit (`Point`), and
struct-payload (`Rect { w, h }`) variants may be mixed freely:
`Clone`, `PartialEq` / `Eq`, `Debug` (`Rect { w: 2, h: 3 }`), and
`Default` (which selects the `#[default]` unit variant) all derive and
run identically on every tier.

A struct / tuple used as a `Map` / `Set` key is hashed and compared by
value on every tier - so two equal-valued keys at distinct allocations resolve
to the same entry - with no `#[derive(Hash)]`; hashing is automatic, and
`#[derive(Hash)]` is rejected (`GT0025`).

---

## 4. Variables, expressions, statements

### 4.1 Bindings

```
LetStmt = "let" LetTargets [ ":" Type ] [ "=" ExprList ]
LetTargets = Pattern { "," Pattern }
ExprList = Expr { "," Expr }
```

- `let x = 1` - immutable binding, type inferred.
- `let mut x = 1` - mutable binding.
- `let a, b = pair` - destructuring.
- `let a, b = 1, 2` - the same list, written out element by element.
- `let a, (b, c) = 1, (2, 3)` - a nested element keeps its parentheses.
- `let Point { x, y } = p` - struct destructuring.
- `let Point { x: a, y: b } = p` - renamed struct destructuring.
- `let Nested { p: Point { x, y }, label } = n` - nested struct.
- `let Shape::Pair(m, n) = s` - enum / tuple-struct variant.
- `let &value = shared` - copy the value behind a shared reference.
- `let &mut value = writable` - copy the value behind a mutable reference.
- `let (A(g, _) | B(g)) = v` - or-pattern (alternatives must bind the
  same names).
- `let x: i64 = 1` - annotated.

A `let` lists its targets separated by commas, and a list of two or more
binds the elements of a tuple value element-wise. Writing that list as a
parenthesised tuple pattern - `let (a, b) = pair` - reports `GP0042` and
carries the paren-free rewrite. Parentheses group a pattern only where one
sits beside others: a `match` arm, a `for` binding, a parameter, and a
nested element of the list itself.

A right-hand side may be written as a list too, in which case it is the
tuple its elements form: `let a, b = 1, 2` and `let a, b = (1, 2)` are the
same binding.

A `let` pattern must be irrefutable (it always matches); a refutable
pattern requires `let ... else { ... }` (the `else` block must diverge).
Irrefutable struct, nested-struct, variant, and or-pattern destructuring
bind correct values on the bytecode VM, the Cranelift JIT, and the LLVM
AOT tiers.

Shadowing is permitted.

A comma-separated list on the left of `=` is a destructuring target: each
element is written from the matching element of the right-hand side, which
is evaluated in full before the first write. Every element must be a
writable place - a binding, a field, an index, a tuple position, or a
dereference - or `_`, which discards its element. Targets nest, and a
nested one keeps its parentheses. A compound operator pairs element-wise:
each place is read, combined with its own element of the right-hand value,
and written back.

```
a, b = b, a
p.x, xs[1] = 5, 6
head, (left, right) = 1, (2, 3)
_, kept = 99, 42
x, y += 2, 3
```

The left side of `=` is a pattern and the right side is an expression. This
makes the two uses of reference syntax complementary: `&mut place` in an
expression creates a mutable reference, while `&mut pattern` in a pattern
matches and removes a mutable-reference layer. Only `mut name` makes a binding
reassignable. For example, `let &mut value = reference` does not make `value`
reassignable; `let &mut mut value = reference` does.

For a simple top-level copy, `let value = *reference` is usually the clearest
spelling. Reference patterns remain useful and uniform when nested, such as
`let name, &mut count = entry`.

### 4.2 Expressions

Every construct except `let`, `use`, item declarations, and control
flow **statements with a trailing `;`** is an expression. Block
expressions return their tail expression:

```
let n = {
  let x = 2
  x * x
}
```

The control-flow constructs `if`, `match`, `loop`, `while`, `for`,
`unsafe { ... }`, and `{ ... }` are expressions. `while` and `for`
evaluate to `()`. `loop` can return a value via `break value;`.

### 4.3 Reference expressions

`&expr` creates a managed reference. `&mut expr` creates a writable
managed reference and requires `expr` to be a mutable place. Both refer
to the source place rather than copying it: after `let r = &mut x`, a
write through `r` is observable through `x`. A fresh temporary, such as
`&mut [1, 2]`, is also a writable source place.

The reference capability is fixed by its type. `let mut r = &x` permits
rebinding `r`, but `r` remains an `&T` and cannot write through a later
target. `let r = &mut x` permits writes through `r` without permitting
`r` itself to be rebound.

`*expr` dereferences a managed reference (`&T -> T`); it is not
restricted to `unsafe` and there are no raw pointers to dereference.
Regular `&T -> T` dereference is also implicit at `.` and index
operators, so an explicit `*` is rarely needed.

### 4.4 Control flow

#### `if`

```
IfExpr      = "if" Condition Block [ "else" ( IfExpr | Block ) ]
Condition   = CondClause { "&&" CondClause }
CondClause  = "let" Pattern "=" Expr | Expr
```

An `if` without an `else` has type `()`. With `else`, both arms must
produce the same type (or one is `!`).

A condition is a let-chain: a sequence of clauses joined by `&&`, where
each clause is either a `let` binding (`let PAT = expr`, which matches a
refutable pattern against a scrutinee) or a boolean expression. The chain
holds when every clause holds: each `let` clause must match and each
boolean clause must be `true`. Earlier `let` bindings are in scope for
every later clause and for the body and `else` branch.

```
if let Some(x) = a && let Some(y) = b && x > 0 {
  use(x + y)
}
```

A `let` clause chain is `&&`-only: joining `let` clauses with `||`
without parentheses is a parse error (`GP0001`). The construct is a
front-end desugar into nested `match`, so it runs identically across all
tiers.

#### `match`

```
MatchExpr = "match" Expr "{" MatchArm { MatchSep MatchArm } [ "," ] "}"
MatchArm  = Pattern [ "if" Expr ] "=>" ( Expr | Block )
MatchSep  = "," | LineBreak
```

`match` is exhaustive. Non-exhaustive `match` is a compile error.
Arms on separate lines do not need commas. Same-line expression arms require
a comma; a block body also forms an unambiguous arm boundary.
Patterns support literals, wildcards (`_`), ranges, bindings,
struct/enum destructuring, and or-patterns (`A | B`). Ranges may be
closed (`1..=10`), exclusive (`1..10`), or open-ended: `..=hi` and
`..hi` (open start), or `lo..` (open end, covering up to the type maximum
inclusive). Range patterns are opaque to exhaustiveness analysis, so a `_`
arm is still required even when the ranges appear to cover the type. An
inclusive marker requires an upper bound, so bare `..=` and `lo..=` are
parse errors.

```
match divide(a, b) {
  Ok(v) => println("got: {}", v),
  Err(e) => eprintln("err: {}", e),
}
```

#### `while`, `loop`, `for`

```
WhileExpr  = "while" Condition Block
LoopExpr   = "loop" Block
ForExpr    = "for" Pattern "in" Expr Block
```

A `while` condition is the same let-chain form as `if` (see above): a
sequence of `let PAT = expr` and boolean clauses joined by `&&`, with
earlier bindings in scope for later clauses and the body. The loop runs
while the whole chain holds.

`for` desugars to a loop that calls `.next()` on an iterator. Any type
implementing `Iterator<Item = T>` (see §10.4 on stdlib traits) can be
ranged over. The built-in ranges `a..b` and `a..=b` implement
`Iterator`.

A `Result` or an `Option` is not a sequence, and a `for` over one is
rejected (`GT0067`). Each holds at most one value and carries no element
type, so such a loop would bind nothing and run zero times while whatever
its body read off the binding still type-checked. Take the value out
first - with `?`, a `match`, `if let Some(v) = ..`, or `unwrap_or(..)` -
and iterate that.

#### `break`, `continue`

`break [expr]` exits the innermost loop (value only valid in `loop`).
`continue` jumps to the next iteration. Labeled variants
(`'outer: loop { break 'outer; }`) are supported.

#### `return`

`return expr;` exits the enclosing function. `return;` returns `()`.

#### `arena`

```
ArenaStmt = "arena" Block
```

`arena` is a contextual keyword (an identifier `arena` not followed by
`{` is an ordinary name). Every allocation made while the block runs is
bump-allocated into a fresh arena and freed wholesale when the block
exits, on every exit path - the construct desugars to
`runtime::arena_push()` plus a block-scoped
`defer runtime::arena_pop()`. The block is statement-position only and
yields `()`; a tail expression is evaluated and discarded.

Contract: a value allocated inside the block must not be referenced
after the block exits (no assignment to outer bindings, no stores into
outliving containers, no channel sends, no captures that outrun the
block). `Weak` references to arena values upgrade to `None`. Arenas
nest; inner arenas free at their own close brace.

#### `cohort`

```
CohortExpr   = "cohort" [ "(" CohortArgs ")" ] Block
CohortArgs   = CohortArg { "," CohortArg }
CohortArg    = "policy" ":" PolicyVariant
             | "timeout" ":" IntLiteral
             | "context" ":" ContextVariant
PolicyVariant  = "Policy" "::" ( "FailFast" | "CollectAll" | "Race" )
ContextVariant = "Context" "::" ( "Default" | "Isolated" )
```

`cohort` is a contextual keyword (an identifier `cohort` not followed by
`{`, or by a parenthesised header and `{`, is an ordinary name). Every
goroutine started with `spawn` while the block runs belongs to the
cohort, and the block cannot be left until each of them has finished:
a child cannot outlive the block that started it.

The block is an expression yielding `Result<(), errors::Error>`, so it
binds and composes with `?`. A tail expression in the body is evaluated
and discarded, as in `arena`. The construct desugars to
`runtime::cohort_push(..)`, a block-scoped `defer runtime::cohort_pop()`,
and a tail `runtime::cohort_join()`, so the join runs on every exit path
including `return`, `break`, and a `?` out of the middle of the block.

A child fails by panicking or by answering `Err`. Under the default
`Policy::FailFast` the first failure cancels its siblings and becomes the
cohort's `Err`; the reported failure is the one with the lowest spawn
index, not the first to arrive, so the answer never depends on completion
order or on which tier is running. `Policy::CollectAll` runs every child
and reports every failure; `Policy::Race` stops at the first success.

Cancellation is cooperative. A cancelled child observes cancellation as
an operation's ordinary "nothing more is coming" answer - a `recv`
reports `None`, the same answer a closed channel gives, and a `sleep`
returns early - so it leaves through its own normal exit path and its
`defer` frames and destructors run in order. Nothing is killed. Pure
computation is not a cancellation point; a CPU-bound child cooperates by
polling `runtime::cohort_cancelled()`. `timeout:` cancels the cohort once
the given number of milliseconds has passed.

`on_error:` decides what the cohort DOES with a child's failure, where
`policy:` decides when the block stops waiting. `OnError::Propagate` (the
default) makes the first failure the block's `Err`; `OnError::Log` names
every failure on stderr as it happens and the block answers `Ok`;
`OnError::Ignore` answers `Ok` and says nothing. No disposition makes a
child unaccountable: it is still counted, still drained, and a child that
never finishes is still named by the drain report.

`cancellable: false` exempts the cohort and everything under it from
cancellation - what a shielded region asks for. The exemption covers
cancellation only; the block still drains and still reports.

`drain: ms` bounds the wait for the children once the body is done, which is
a different question from `timeout:`, that bounds the body's own work. A
cohort with no `drain:` waits as long as its children take, because leaving
the block is the program's statement that they are finished. A drain that
gives up names what it left running.

`isolation: Isolation::Thread` runs each child on a dedicated OS thread for
its whole life, which is what synchronous Rust FFI and never-yielding
CPU-bound work need. `Isolation::Shared` is the default, and channels work
across both unchanged. The key names what it decides - whether a child owns
a thread - and leaves `context` to `context::Context`, the cancellation type
a cohort may one day inherit; the retired `context:` spelling reports
`GP0056` with the rewrite.

`runtime::cohorts()` answers one descriptor line per live cohort, oldest id
first, naming its id, parent, completion policy, error disposition, how many
children are outstanding, whether it is cancelled, and the spawn indices that
have not left. A child spawned with a `reason:` label is named by that label
beside its index. A cohort is enumerable so a program can say what it is still
waiting on without joining it. A drain that gives up names those same children
rather than only counting them. `runtime::root()` answers the root cohort's
line in that same shape - the one cohort every program has, and the one whose
drain bounds process exit.

`main` runs inside an implicit root cohort, so every `spawn` belongs to
one: no goroutine outlives the program, and a child failure that nothing
observed through its join handle is reported on stderr at exit instead of
vanishing. The root's policy is `CollectAll`, so one child's failure never
cancels another's work.

The root cohort is `main`'s alone. Its extent is the process, which is the
same extent `main` has, and is longer than any other function's - so a
`spawn` in a function other than `main` must be written lexically inside a
`cohort { }` in that same function body. Anywhere else it is `GT0086`. See
[Spawn scope](#spawn-scope).

```
fn gather() -> Result<(), errors::Error> {
  cohort {
    let a = spawn(|| fetch("one"))
    let b = spawn(|| fetch("two"))
    println("{} {}", a.join()??, b.join()??)
  }
}
```

#### `defer`

```
DeferStmt = "defer" Expr
```

`defer` is contextual (§2.4): it opens the statement only in statement
position and directly before a name, a keyword, or `{`, so a binding,
field, or function may still be called `defer`.

`defer` is **block-scoped**, following Swift and Zig rather than Go: a
deferred expression runs when control leaves its *enclosing block* - by
falling off the end, `return`, `break`, or `continue` - not when the whole
function returns. Within a block, deferred expressions run in LIFO order. The
argument is any expression, commonly a call or a `{ }` block.

```
fn read_all(path: String) -> Result<Vec<u8>, Error> {
  let file = os::open(path)?
  defer file.close()      // runs when this function's block exits
  file.read_to_end()
}
```

Because the scope is the nearest `{ }`, a `defer` inside a loop body runs at
the end of *each* iteration:

```
while let Some(conn) = listener.accept() {
  defer conn.close()      // closed at the end of every iteration
  handle(conn)
}
```

Deferred expressions are **evaluated when they run**, not when registered:
they read the current value of any variable they reference at block exit (the
same capture rule as Swift/Zig). A deferred expression's own value and any
control flow inside it are discarded; a panic raised inside one propagates.

#### `spawn` / `join`

`spawn(f)` runs the callable `f` (a function or closure taking no
arguments) on a goroutine and returns a `JoinHandle<T>`, where `T` is
`f`'s return type. `handle.join()` blocks until the goroutine finishes
and yields its outcome as `Result<T, String>`: `Ok(value)` on a normal
return, or `Err(message)` if the goroutine panicked.

`spawn` is the only way to start a goroutine. Every one attaches to the
cohort it is written inside, so a goroutine never outlives the block that
started it and a failure nobody joined is still reported. There is no
detached form.

Ownership is fixed at the spawn and there is no re-parenting: nothing
moves a running goroutine from one cohort to another, so the block a
child belongs to is decided once, where it was written.

### Spawn scope

A `spawn` is an error unless one of these holds:

1. it is lexically inside a `cohort { ... }` block in the same function
   body, at any nesting depth and through any number of closures;
2. the enclosing function is `main`, including an entry file's top-level
   statements, which are a synthesized `main`.

Anything else is `GT0086`. Without the rule the attachment is dynamic
state: a function that spawns and opens no cohort of its own hands its
children to whatever cohort its caller happens to be inside, and to the
root cohort - whose extent is the process - when the program has none.
Neither side of the call says so, since the callee's signature does not
mention spawning and the caller cannot see what it took on. `main` is
exempt because the root cohort's extent IS main's extent, so a child
started there does not outlive the scope that owns it.

A method in an `impl` block is a function for this purpose, including one
named `main`. There is no escape hatch and no suppression attribute: a
helper that spawns workers and hands the handles back for its caller to
join is the shape the rule refuses, and the fix is to move the spawns into
the caller's cohort and have the helper take or answer a closure.

`spawn(f, reason: "..")` labels the goroutine. The label is text a report
prints: the cohort's enumeration and drain reports name a labelled child by
it in place of the bare spawn index, so what is still running reads as the
task the program meant rather than the slot it took.

A closure body runs on the child, so an operand that must be read where
the spawn is written is bound first:

```
let snapshot = xs[0]
spawn(|| worker(snapshot))
```

```
let h = spawn(|| compute())
match h.join() {
    Ok(v)  => use(v),
    Err(e) => report(e),
}
```

#### `select`

```
SelectExpr = "select" "{" SelectArm { "," SelectArm } [ "," ] "}"
SelectArm  = RecvPattern "=" RecvExpr "=>" ( Expr | Block )
           | SendExpr            "=>" ( Expr | Block )
           | "default"           "=>" ( Expr | Block )
RecvExpr   = Expr ".recv()"
SendExpr   = Expr ".send(" Expr ")"
```

`select` is contextual (§2.4): only the word directly before `{` opens
the expression, so a method or binding may still be called `select`.

`select` chooses exactly one of its communication operations to proceed,
pseudo-randomly among those ready. If none is ready and no `default`
arm exists, the goroutine blocks. Matches Go's select semantics.

Example (from examples.md):

```
select {
  Ok(msg) = rx_ok.recv() => println("success: {}", msg),
  Err(err) = rx_err.recv() => println("error: {}", err),
}
```

The binding pattern on the left (`Ok(msg)`) matches the `Result` returned
by `.recv()` (see §8.3).

### 4.5 The `?` operator

```
TryExpr = Expr "?"
```

If applied to `Result<T, E>`, it evaluates to `T` on `Ok`, or returns
`Err(From::from(e))` from the enclosing function on `Err`. If applied
to `Option<T>`, evaluates to `T` on `Some`, or returns `None` on `None`.
The enclosing function's return type must be `Result<_, E2>` (where
`E: Into<E2>`) or `Option<_>` respectively.

The HIR desugar tracks the enclosing function's declared return
type so the cross-type conversion is automatic. When the inner
expression's `Err` type differs from the enclosing function's
`Err` type the desugar inserts a call to `errors::Error::from`
(or any user-supplied `Into<E2>` impl when the typechecker
recognises one) on the propagated value. Result and Option are
disambiguated by inspecting the inner type's ADT name; ambiguity
falls back to the Result desugar. See
`feature-testing-examples/try_option_propagation.gos` and
`feature-testing-examples/try_err_conversion.gos` for end-to-end
coverage across all three tiers.

### 4.6 Pipe expression (F#-style forward pipe)

```
PipeExpr = Expr "|>" Expr
```

The forward-pipe operator `|>` composes **free functions**, which have no
receiver to chain from. The operator is **left-associative** and has very
low precedence (just above assignment), so `a |> f |> g` parses as
`(a |> f) |> g` and means `g(f(a))`.

A step either takes the piped value as its only argument, or is a closure
whose parameter is the slot the value fills. No argument order is
assumed, so a step reads the same whatever the callee's parameters are -
a stdlib function, one this program declares, or a method on an external
receiver.

Lowering rules:

1. `x |> path` where `path` resolves to a callable of arity 1:
   → `path(x)`.
2. `x |> path()` and `x |> recv.method()`, with no written arguments:
   → `path(x)` and `recv.method(x)`.
3. `x |> closure_expr` where `closure_expr` evaluates to a callable:
   → `closure_expr(x)` (arity must be 1). A closure written inline is the
   ordinary spelling of a step that fills a particular slot:
   `x |> |v| path(a1, ..., v, ..., an)` is
   `path(a1, ..., x, ..., an)`, and the same holds for
   `x |> |v| recv.method(a1, ..., v, ..., an)` and for a turbofish call
   `x |> |v| path::<T1, ..., Tk>(a1, ..., v, ..., an)`.
4. The parameter may sit anywhere the closure body reaches, so
   `x |> |v| f(a, v).method()` is `f(a, x).method()`.

A closure written directly as a step is lowered to the block
`{ let v = x; body }` rather than a closure the pipe invokes, so the step
carries no closure allocation and no capture boundary. A body whose control
flow leaves the closure - a `return`, a `?`, a `break` or `continue`
targeting an outer loop - keeps the closure it was written against, since
those target the closure rather than the enclosing function.

A step that writes its own arguments and is not a closure reports
`GP0041`: the slot the value fills would otherwise have to be assumed,
and the signatures where that assumption is wrong are frequently
homogeneous enough that type checking cannot detect it. A formatting
macro written as a step reports `GP0025` for the same reason.

**Retired form.** `$` spelled the slot in earlier releases and is no
longer part of the language. Any `$` reports `GP0027`: in a step
(`x |> f(a, $)` is `x |> |v| f(a, v)`), as a receiver (`x |> $.trim` is
`x.trim()`, since a method already chains and a method chain is an
ordinary operand that may feed a pipe step), or as a callback argument
(`xs.map($.abs)` is `xs.map(|v| v.abs())` or `xs.map(math::abs)`).

**Std functions in value position.** A stdlib free function named where
a value is expected is rewritten into the closure that calls it, using
the parameter count its signature declares: `xs.map(math::abs)` is
`xs.map(|v| math::abs(v))`. Every tier therefore lowers the same
closure. A function with no fixed parameter list (the formatting
family, `panic::panic`) has no such closure and is reported as
`GT0015`.

If the right operand is not a call form matching one of the above, the
compiler emits `E0601: right-hand side of '|>' must be a callable`.

Type-checking rule: the type of the piped value must unify with the type
of the parameter it fills in the right-hand callable. Method lookup,
trait resolution, auto-deref, and the `?` operator all apply to the
desugared call exactly as they would to a hand-written call.

Examples:

```
// The slot is named, so a step reads the same for any callee.
text |> |v| strings::slice(v, 1, 3)
name |> |v| format("hello {}", v) |> println

// A bare callable needs no closure.
3 |> double |> negate
```

Idiomatic iterator chains:

```
let total =
  1..=100
  |> |v| iter::filter(v, |n| n % 2 == 0)
  |> |v| iter::map(v, |n| n * n)
  |> iter::sum::<i64>()
```

Desugars to:

```
let total = iter::sum::<i64>(iter::map(iter::filter(1..=100, |n| n % 2 == 0), |n| n * n))
```

**Argument-order convention.** Every stdlib free function takes the
value it transforms as its **first** positional parameter, so a free call
reads as the method it stands for: `iter::map(xs, f)` beside `xs.map(f)`,
`strings::split(s, ",")` beside `s.split(",")`. A call written with the
data in the trailing slot reports `GR0021` and still means what it did,
so the rest of the body is diagnosed on its own terms.

Interaction with `?`:

```
read_file(path)? |> parse_json::<Config>()?
```

Here `?` binds tighter than `|>` (§4.7 precedence), so this parses as
`(read_file(path)?) |> (parse_json::<Config>()?)` - the inner `?`
unwraps the `Result<String, _>`, pipes the `String` into
`parse_json`, and the outer `?` unwraps that result.

### 4.7 Operators and precedence

From highest to lowest:

| Level | Operators | Associativity |
|---|---|---|
| 1 | `::` path | left |
| 2 | `.` method/field, `[]`, `()`, `?`, postfix | left |
| 3 | unary `-`, `!`, `&`, `&mut`, `*` (deref) | right |
| 4 | `as` cast | left |
| 5 | `*`, `/`, `%` | left |
| 6 | `+`, `-` | left |
| 7 | `<<`, `>>` | left |
| 8 | `&` bitand | left |
| 9 | `^` bitxor | left |
| 10 | `\|` bitor | left |
| 11 | `==` `!=` `<` `<=` `>` `>=` | none (non-associative) |
| 12 | `&&` | left |
| 13 | `\|\|` | left |
| 14 | `..` `..=` range | none |
| 15 | `\|>` pipe | left |
| 16 | `=`, `+=`, `-=`, etc. (statement-only) | right |

---

## 5. Patterns

```
Pattern = LiteralPattern
        | IdentPattern                      // binding
        | "_"                                // wildcard
        | "(" Pattern { "," Pattern } ")"   // tuple
        | Path ( "(" PatternList ")" | "{" FieldPatternList "}" )  // struct/enum
        | Pattern "|" Pattern                // or-pattern
        | Literal ".." Literal               // range, exclusive
        | Literal "..=" Literal              // range, inclusive
        | ".." Literal                       // open-start, exclusive
        | "..=" Literal                      // open-start, inclusive
        | Literal ".."                       // open-end, exclusive
        | "&" [ "mut" ] Pattern              // reference pattern
        | "mut" IdentPattern                 // mutable binding
        | ".." Pattern?                      // rest pattern
```

An open-ended range covers up to the type's maximum (inclusive). An
inclusive marker requires an upper bound, so bare `..=` and `lo..=` are
parse errors. Range patterns are opaque to the exhaustiveness checker, so a
match using only ranges still needs a `_` arm.

A `let` binding (§4.1) and a `let` clause in an `if` / `while` condition
(§4.4) require an irrefutable pattern (or an `else` branch that diverges,
for `let ... else`). Struct, nested-struct, variant, and or-pattern
destructuring in irrefutable position bind identically across all tiers.

`&p` and `&mut p` are reference patterns. They require a shared or mutable
reference respectively, remove that reference layer, and then match `p`
against a value copy of the referent. This rule applies at the top level and
inside tuples, structs, variants, slices, or other patterns. `mut name` is a
binding modifier and is independent of `&mut p`.

Exhaustiveness is checked via matrix decomposition (the Maranget
algorithm, same as Rust).

---

## 6. Projects, modules, and source files

Gossamer cleanly separates two concepts that other languages often
conflate:

- **Module** - how code is *organized* into namespaces. No version, no
  owner, no network identity.
- **Project** - how code is *distributed* and *versioned*. Carries a
  stable domain-based identifier, a semver, and dependency
  declarations.

A project contains one module or many. A module never spans projects.

### 6.1 Source files

A source file is plain Gossamer; it does not declare a package, does
not declare its module, and contains no boilerplate header.

```
SourceFile = { UseDecl } { Item | Statement }
```

A file's module is determined by its location on disk (§6.3). Its
owning project is determined by the nearest enclosing `project.toml`
walked upward from the file (§6.4).

Bare `Statement`s at file scope are accepted only in the entry file,
where they form the body of an implicit `fn main` (§6.10).

### 6.2 Paths

Two path separators, each with one meaning:

- `::` separates **module/name** components:
  `math::vector::Vec3`.
- `.` accesses a **field or method** on a value: `v.x`, `s.len()`.

There is no third separator. Project identifiers, despite containing
`.` and `/` characters, are always written as string literals in `use`
declarations (§6.6).

### 6.3 Modules (code organization)

Modules are directory-based by default. Given a project layout:

```
my-project/
  project.toml
  src/
    main.gos
    math.gos
    math/
      vector.gos
      matrix.gos
    net/
      http.gos
      tcp.gos
```

- Every `.gos` file directly in `src/` contributes items to the
  project's root module.
- Each subdirectory of `src/` is a module named after the directory;
  every `.gos` file inside it contributes items to that module.
- Modules nest: `src/math/vector.gos` is `math::vector`.
- An optional `mod.gos` file inside a module directory holds
  module-level comments and re-exports.

Explicit inline modules are supported for cases where directory
splitting is overkill:

```
mod vector {
    struct Vec3 { x: f64, y: f64, z: f64 }
}
```

Items within the same module reference each other by bare name. Items
in a sibling or nested module use a path: `math::vector::Vec3`, or are
brought into scope with a `use` (§6.6). A module's items are never in
scope on their own; naming one without a path or import is `GR0011`. A
module name is not a value, so a module and one of its items may share a
name and a bare call still reports `GR0011` naming the import.

A module nested inside another is a **module descendant**:
`math::vector` is a descendant of `math`. Descendancy runs one way and
is what §6.3a's default visibility is written against.

### 6.3a Visibility

Three visibilities, declared per item:

| Annotation | Reachable from |
|---|---|
| none | the declaring module and its descendants |
| `pub(package)` | every module of the declaring package |
| `pub` | anything that depends on the package |

Visibility flows inward: a descendant reaches its ancestors' private
items, and an ancestor does not reach its descendants'. A `pub` item
inside a private module stays unreachable from outside that module, and
the module is what the diagnostic names.

Every named item carries a visibility - `fn`, `struct`, `enum`, `trait`,
`const`, `static`, `type`, and `mod`. So do **methods**, declared inside
the `impl` block, and **struct fields**, declared on the field. A `pub`
type may keep private methods (`GT0063`) and private fields (`GT0065`);
the type is API while its representation need not be. A struct with a
private field cannot be constructed from outside the module that
declares it, because naming the field in a literal is a reference to it.

A reference to an item the current module may not reach is `GR0008`.
Importing does not widen anything: a `use` is a spelling convenience,
and visibility is decided by where the name is used, not by where the
`use` was written.

Rust's other restriction forms are rejected with `GP0038`:
`pub(crate)`, `pub(super)`, and `pub(in path)` all name `pub(package)`
instead. There is one restricted spelling, not four.

### 6.4 Projects (unit of distribution)

A **project** is defined by a `project.toml` manifest at its root. It
is the unit of distribution, versioning, and dependency declaration.

```toml
[project]
id      = "example.com/math"
version = "0.3.1"
authors = ["Jane Doe <jane@example.com>"]
license = "Apache-2.0"

[dependencies]
"example.org/linalg"   = "1.2"
"example.com/logging"  = { git = "https://git.example.com/logging.git", tag = "v0.8.0" }
"example.net/internal" = { path = "../internal" }

[registries]
"example.org" = "https://registry.example.org/v1"

# First-fetch trust roots for registry publishers. The key is the
# publisher's 32-byte Ed25519 public key in lowercase hexadecimal.
[trusted-publishers]
"example.org/linalg" = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
```

Required fields:

- `project.id` - the project identifier (see §6.5).
- `project.version` - SemVer 2.0.0 `MAJOR.MINOR.PATCH`, with optional
  `-PRERELEASE` and `+BUILD` suffixes.

Every other key is optional.

Optional keys include `project.output` (binary name override) and
`project.entry` (path to the entry source, relative to the manifest
directory), which overrides convention-based entry resolution.

### 6.5 Project identifiers

A project identifier is a stable, location-independent string of the
form:

```
ProjectId = DomainSegment { "/" PathSegment }
DomainSegment = Label { "." Label }        // must contain at least one "."
Label         = [a-z][a-z0-9-]*
PathSegment   = [a-z0-9][a-z0-9-_]*
```

Examples: `example.com/math`, `acme.dev/tools/codegen`,
`fooware.io/json`.

Properties:

- The identifier is **not** a URL. It names no server, no repository
  service, and no protocol. Resolution to a physical source is the
  toolchain's job.
- It is not tied to any hosting provider. `github.com/...` as an
  identifier is discouraged because it couples identity to a service;
  use a domain you control.
- Ownership is social, not technical. No global authority enforces who
  may publish under a prefix - disputes are resolved by consumers
  choosing which dependency to declare.
- Short identifiers (single-segment: `math`, `fmt`) are **reserved for
  the standard library**.

### 6.6 `use` declarations

```
UseDecl    = "use" UseTarget [ "as" Ident ] [ "{" UseList "}" ]
UseTarget  = ProjectUse | ModulePath
ProjectUse = StringLit [ "::" ModulePath ]
ModulePath = Ident { "::" Ident }
UseList    = Ident [ "as" Ident ] { "," Ident [ "as" Ident ] } [ "," ]
```

A `use` target is either a string-literal project reference or an
identifier-based module path within the current project.

```
// Bring another project into scope. Bound name defaults to the last
// segment of the project id.
use "example.com/math"                        // binds `math`
use "example.com/math" as m                   // binds `m`

// Reach into a specific module of another project.
use "example.com/math"::vector                // binds `vector`
use "example.com/math"::vector::{Vec3, Vec4}

// Same-project paths use ordinary module syntax - no string.
use vector::{Vec3, Vec4}
use net::http::Server

// Standard library uses a reserved single-segment identifier and needs
// no string literal.
use std::io
use std::sync::atomic::{AtomicU64, Ordering}
use std::fmt
```

Standard library modules require an explicit `use`. Writing a qualified
path such as `fs::read(path)` without first importing `std::fs` is an
unresolved-name error. The prelude remains available without imports.
Importing a module binds its final segment, or its requested alias, but
does not import sibling modules or all module members as bare names.

The string-literal form is mandatory for any project whose identifier
contains `.` or `/`, which is every real-world external dependency.
Identifier-only paths never escape the current project.

A dependency requires its `use` on the same terms as the standard
library. Writing `math::add(1, 2)` without first importing
`"example.com/math"` is an error, even though the tool has already
resolved the package: the import is what records which package a
`math::` path comes from. Modules of the current project keep the weaker
rule - a full path such as `util::add(..)` names a sibling module
without an import, because the file layout already shows where it lives.

There is no side-effect-only `use`. A project's initialisation is
explicit through an optional `fn init()` per module, run in
dependency-topological order at program start.

### 6.7 Dependency resolution (tool-driven)

Dependency resolution is the job of the `gos` tool (§16). The compiler
itself never fetches code; it reads a resolved source tree the tool
prepared. The tool resolves each entry in `[dependencies]` by source
kind:

- **Registry**: the dependency's project-id prefix is matched against
  the `[registries]` table. A registry is a plain HTTP endpoint
  exposing signed tarballs. No central registry exists or is required;
  `[registries]` may be empty. Multiple registries coexist without
  conflict because each serves distinct domain prefixes.
- **Git**: `{ git = "...", tag = "..." | branch = "..." | rev = "..." }`.
  The tool clones the repository, expects a `project.toml` at its
  root, and verifies that the manifest's `project.id` matches the
  dependency entry's key.
- **Local path**: `{ path = "../other" }`. For developing related
  projects side by side; forbidden in published manifests.
- **URL tarball**: `{ url = "https://...", sha256 = "..." }`. Plain
  fetch of an archive with a required checksum.

A dependency's source is reached under a module name derived from the final
segment of its id, with each `-` replaced by `_`, so
`github.com/owner/pgsql-gos` is written `pgsql_gos`. Two dependencies whose
ids share that segment would reach source under one name; `module = "..."` in
the entry names one of them explicitly:

```toml
[dependencies]
"github.com/ownera/pgsql-gos" = { path = "../first", module = "pg_a" }
"github.com/ownerb/pgsql-gos" = { path = "../second" }
```

Either spelling reaches the package: the module name, or the package id
through `use "github.com/ownera/pgsql-gos" as ..`.

### 6.8 Reproducibility

On first resolution the tool writes `project.lock` recording, for
every transitive dependency:

- The resolved project identifier and version.
- The concrete source (git SHA, registry version, URL) it came from.
- A sha256 checksum of the source tree as fetched.

A checked-in lockfile yields byte-identical builds across machines.
For registry dependencies it also records the publisher key. A new registry
publisher must be authorized by a matching `[trusted-publishers]` entry in the
root manifest; a mutable registry index is not a trust root. On later fetches,
the lockfile key is authoritative and a different advertised key is rejected.

### 6.9 Decentralisation

The design assumes and protects decentralised distribution:

- No single registry is required. Offline and air-gapped builds work
  via path dependencies and a `[replace]` table.
- Registries are optional and federated by DNS prefix.
- Direct git and URL dependencies remain first-class; a project can
  live forever without ever being published to a registry.
- Identifiers carry no global authority. If two projects claim the
  same identifier, consumers pick the right one by declaring the
  source explicitly.

### 6.10 Entry point

An executable program's entry is `fn main`, returning either `()` or
`Result<(), E>`.

The entry file may omit the `fn main` wrapper and instead contain bare
statements at file scope: the entry file is then **implicitly wrapped in
`fn main()`**. The compiler collects the file's top-level statements, in
source order, as the body of a synthesized `fn main`; functions, structs,
and other items declared alongside them are hoisted to file scope as usual.
If any statement uses the `?` operator, the synthesized signature is
`fn main() -> Result<(), errors::Error>`; otherwise it returns `()`. Set a
process exit code explicitly with `std::process::exit(n)`.

A file may use exactly one entry form. Mixing bare top-level statements with
an explicit `fn main` is an error. Top-level statements are accepted only at
the entry file's top level, never inside a `mod { }` body.

The entry file is `src/main.gos` by convention, or whatever `[project] entry`
names (§6.4); a file passed directly to `gos` / `gos build` is the entry.

### 6.11 Rationale

Separating modules from projects matters because conflating them
(Go's "package == import unit == distribution unit") means every
rename, split, or move becomes a breaking change visible to every
caller. Domain-based identifiers matter because they give stable names
that survive hosting changes. A tool-driven resolver matters because
network fetching, checksums, and lockfiles are operational concerns
that do not belong in the language grammar. Decentralisation matters
because pinning a language to a single registry service hands control
of the ecosystem to whoever runs it.

---

## 7. Memory model

### 7.1 Allocation

All heap-allocated values are managed automatically by the runtime.
Values that do not escape their defining function may be
stack-allocated (escape analysis). The escape rules are:

1. Any value whose address is taken (`&x`) and passed across a call
   boundary escapes.
2. Any value assigned to a field of a heap-allocated struct escapes.
3. Any value sent on a channel escapes.
4. Any value captured by a closure that is stored or passed beyond the
   creating scope escapes.

### 7.2 Automatic memory management

Memory management is deterministic reference counting for heap enums
and runtime containers, drop-pass reclamation for value aggregates,
weak references, an on-demand cycle collector
(`runtime::collect_cycles()`), and `arena { }` regions. Cycle collection and
collection-driven `Weak<T>` invalidation are Experimental: the compiled
runtime collects thread-local RC graphs, while the bytecode VM currently has
no cycle collector. They are not part of the Stable cross-tier contract.
There is no tracing collector: no pacer, no write barrier, and no GC
pause.

Memory is reclaimed deterministically, without a tracing collector:

- **Reference counting.** Recursive heap enums and runtime
  containers carry an intrusive header (`[RcHeader | payload]`).
  Codegen emits balanced retain/release pairs; when the strong
  count reaches zero the value's reference-counted children are
  released iteratively and the payload is freed. Semantics match
  the interpreter tier's shared-ownership model.
- **Weak references.** A weak reference does not contribute to the
  strong count; upgrading after the payload is destroyed yields
  `None` (Swift-ARC model).
- **Cycle collection.** On the compiled tiers, thread-local reference cycles
  are reclaimed on demand by `runtime::collect_cycles()` (Bacon-Rajan trial
  deletion). Values shared across goroutines are excluded from this pass and
  their cycles must be broken with `Weak<T>`. The bytecode VM currently treats
  `runtime::collect_cycles()` as a no-op. This entire collection surface is
  Experimental; no background collector runs.
- **Value aggregates.** Structs, tuples, and arrays are
  heap-allocated at construction and freed by the compile-time drop
  pass at scope exit. An aggregate that escapes the drop pass's
  analysis (e.g. stored through an opaque chain) is leaked until
  process exit rather than unsoundly collected.
- **Arenas.** `arena { }` bump-allocates everything constructed
  inside the block and frees it wholesale at every exit path
  (§4.4). Nothing allocated inside may be referenced after the
  block.

Safe points, pacers, and collection-triggered pauses do not exist;
allocation cost is a pointer bump (arena) or one `malloc`/refcount
(heap values).

### 7.3 Zero values

Every type has a zero value:

- Numeric: `0`.
- `bool`: `false`.
- `char`: `'\0'`.
- `String`: empty string.
- `Vec<T>`, `Map<K, V>`, channel endpoints: `Empty`/`None`-like
  empty containers, not `None`.
- `Option<T>`: `None`.
- Enums: the first-declared variant, if it has no payload; otherwise
  the zero value for a type with no natural zero is a compile error if
  observable (types with no zero-default must be initialized).
- Structs: each field at its zero value.
- `fn` / closure types: **no zero value**. A field or variable of
  function type must be explicitly initialized. (Unlike Go's nil fn.)

### 7.4 Ordering and atomics

Gossamer adopts data-race-free sequential consistency (DRF-SC) and the
following implemented happens-before edges:

- Channel operations establish happens-before relationships.
- Mutex lock/unlock establish happens-before relationships.
- WaitGroup completion happens before a successful wait returns.
- `sync::Once::call_once` publishes its completed body to every caller that
  returns from the same `Once`.
- Sequentially consistent atomic operations publish and acquire a
  happens-before edge; explicit release stores pair with acquire loads.

Relaxed atomic operations provide atomicity only and deliberately create no
happens-before edge. Additional ordering modes must not be treated as Stable
until their operation-level contract and detector support are registered.

A runtime data-race detector ships behind `gos test --race`. When it
is enabled, the LLVM AOT codegen instruments heap loads and stores with
`gos_rt_race_access` calls and the runtime (`gossamer-runtime::race`)
maintains a per-goroutine vector-clock happens-before model, recording
synchronisation edges at channel handoff, mutex unlock, WaitGroup completion,
`Once::call_once`, and the supported atomic acquire/release operations. Any
access pair left unordered by a happens-before edge is
reported and fails the test run. It is a testing instrument rather than
an always-on runtime guard, and it sees the compiled-tier accesses the
codegen instruments.

### 7.5 References and aliasing (write-through references, lexical checks)

Gossamer has no ownership transfer, `move` keyword, explicit lifetime
annotations, or non-lexical lifetime inference. It uses implicit lexical
lifetimes instead: references are non-owning views and cannot escape the call
or block that proves their source remains live.

The checker enforces the full named-root rule for its supported reference
shape. Any number of shared views may coexist. A mutable view is exclusive.
While a shared view is active the source cannot be mutated. While a mutable
view is active the source cannot be read, mutated, or borrowed again. The
view itself remains the permitted access path. A reference ends only at its
lexical scope boundary.

References cannot be stored in aggregates or containers, returned from user
functions, captured by closures, sent through channels, or spawned.
Inferred container and channel element types are checked after inference so an
omitted annotation cannot bypass the rule. A direct call-scoped borrow remains
valid, including array-to-slice and Vec-to-slice coercion. Static string
literal returns are the sole exception because their backing bytes have
process lifetime.

#### 7.5.1 What this means in practice

- Use `&` to signal "I only read this" and `&mut` to signal "I write
  through this." These markers gate writes and mutable-reference
  creation, and select the mutating versus non-mutating method where
  dispatch distinguishes them.
- Calls never infer mutable access from a parameter type. Pass a writable
  place as `&mut place`. A value already typed as `&mut T` can be forwarded
  directly to another `&mut T` parameter.
- `let mut reference = &value` permits a direct `reference = &other` rebind;
  it does not make either shared referent writable. Rebinding the reference
  through an alias is rejected.
- `let reference = &mut value` permits writing through `reference` without
  making the reference binding itself mutable.
- References alias their source place on every tier. For example,
  `let mut xs = [1, 2]; let r = &mut xs; r[0] = 0` leaves both `xs` and
  `r` observing `[0, 2]`; it never creates a copy-on-write side value.
- An immutable binding cannot be the source of `&mut`. Otherwise `let x =
  value; let r = &mut x` would provide a write path to the value that `let
  x` declares immutable.
- `&mut Vec<T>` / `&mut [T]` parameters do carry write-through to the
  caller (§3.4), so the marker is load-bearing for that data flow. The
  scope-local exclusivity rules apply to these references too.
- Returning or storing a reference is rejected. Use an owned return value or
  keep the view at a direct call boundary.
- `spawn` and `Sender::send` reject reference payloads. Owned Vec values are
  cloned before publication so sender-side growth cannot invalidate receiver
  storage.

#### 7.5.2 What this deliberately omits

This design deliberately does *not* include:

- A lifetime borrow checker, region inference, or non-lexical borrows.
- Non-lexical last-use analysis. Views deliberately last to the closing brace.
- Explicit lifetime annotations (`'a`, `'static`, `for<'a>`). Lifetime
  syntax in a generic parameter list is parsed and then ignored.
- `Send`/`Sync` marker traits. References never cross concurrency boundaries;
  runtime synchronization handles have their own explicit contracts.

This is a conservative lexical borrow checker, not Rust's lifetime system. It
enforces a smaller lexical language shape and rejects escape forms that would
require non-lexical lifetime inference or arbitrary alias reasoning.

---

## 8. Concurrency

### 8.1 Goroutines

A goroutine is a stackful coroutine scheduled cooperatively by the
runtime. `spawn(|| expr)` starts one. Each goroutine owns a fixed-size
mmap'd stack (default 1 MiB; override with `GOSSAMER_GOROUTINE_STACK`,
clamped to a 32 KiB minimum). The operating system commits pages on demand.
A byte-budget guard reports `GX0008` before the hardware guard page; the stack
does not grow or shrink.

**Argument discipline.** After `spawn(|| f(x))` returns, the caller may continue to
use `x`; Gossamer has no source-level ownership transfer. Primitive values
copy directly. A named Vec argument is cloned before the goroutine is
published, including nested Vec storage, so later growth occurs on independent
buffers. Every managed child reachable from that Vec is marked shared before
publication. Runtime synchronization handles keep their documented
shared-handle semantics. GT0055 rejects every inline struct, tuple, or
fixed-array argument at a spawned call because the compiled spawn ABI
cannot yet copy arbitrary inline layouts. Publish supported fields separately
and reconstruct the aggregate in the receiving goroutine.

A spawned call may not capture or pass a `&T` or `&mut T`. The tracked
`&`/`&mut` access markers are lexical write-intent markers (§7.5) and cannot be
carried across goroutine boundaries. Pass the underlying value (managed
reference, or `Copy`) instead.

Cross-goroutine data races on other explicitly shared mutable state are
possible. Detect them at runtime with `gos test --race`
(§7.4) and prevent them by communicating through channels rather than
sharing state.

The scheduler is an M:N work-stealing scheduler:

- **M** = OS thread (one per core by default, configurable via
  `GOSSAMER_MAX_PROCS` or `runtime::set_max_procs(n)`).
- **P** = processor (logical context, fixed count = max-procs).
- **G** = goroutine.

The network poller (epoll on Linux, kqueue on macOS/BSD, IOCP on
Windows) parks goroutines blocked on I/O without holding the
underlying OS thread. Same path covers `time::sleep` (timer wheel),
`channel.recv` / `channel.send` on a full or empty channel,
`sync::Mutex` contention, and `sync::WaitGroup::wait`. Scheduler-aware network
and selected HTTP/filesystem/process operations use a shared blocking pool.
The remaining direct filesystem and process paths are Experimental until the
builtin effect audit confirms that they cannot pin a scheduler worker.

### 8.2 Channels

```
let tx, rx = channel::<T>()             // unbuffered
let tx, rx = channel::<T>(cap: 16)      // buffered
```

Channel operations (non-`select`):

- `tx.send(v)` - blocks until a receiver is ready (unbuffered) or
  buffer has capacity.
- `rx.recv() -> Option<T>` - blocks until a sender sends a value or
  the channel is closed; returns `None` when the channel is closed and
  drained.
- `rx.try_recv() -> Option<T>` - non-blocking.
- `tx.close()` - marks the channel closed. Subsequent sends panic.
  Receives drain buffered values then return `None`.

Channels are many-to-many. Close only once.

**Deadlock is reported, not waited on.** When a channel operation would
block and **no goroutine is left running**, the caller is the whole program:
a channel that can hand nothing over will never be able to, however long it
waits. The runtime reports it and stops the program:

```text
error: runtime error: error[GX0005]: panic: all goroutines are asleep - deadlock! (send can never complete)
```

The exit code is 101, matching a panic. A deadlock is a property of the whole
program rather than of the goroutine that notices it, so the report ends the
program even when a spawned goroutine is the one that detects it - unlike an
ordinary panic, which ends only its own goroutine (§8.1).

The check is deliberately one-sided: it reports only what it can establish
without racing a goroutine that is still free to act. A program whose
goroutines are all blocked *on each other* is not reported and waits, as it
did before - the last goroutine to park is itself running at the moment it
would check, so "no goroutine left running" is never true then. Ending a
working program over a state still being written would be far worse than
missing a case, so the rule stays on the side that cannot do that.

A pending handoff is never a deadlock either: an unbuffered send has the
sender waiting for its value to be taken and the receiver waiting for a value
to arrive, and that pair is progress. Nor is a goroutine waiting on a timer,
a socket, or a blocking call - each waits on something outside the goroutine
graph.

`Sender::send` copies primitive values and clones a named Vec value, including
nested Vec storage, and marks all managed children shared. Scalar-only inline
aggregates are copied. GT0055 rejects channel values whose inline aggregate
contains nested Vec storage until that ABI has a complete child-ownership
descriptor. No ownership transfer is implied; the sender retains access to its
binding. Sending a `&T` or `&mut T` on a channel is a compile error because a
lexical view cannot cross a goroutine boundary.

### 8.3 Select

Each arm of `select` is a communication operation. The `rx.recv()`
call returns `Option<T>`, so matching `Some(v)` / `None` handles the
closed-channel case:

```
select {
  Some(v) = ch.recv() => process(v),
  None    = ch.recv() => { break }       // channel closed
  default              => do_other(),
}
```

### 8.4 `defer` and goroutines

Deferred expressions are **block-scoped** (see §`defer`): each runs when
control leaves its enclosing `{ }` block, not when the whole function or
goroutine unwinds. The exit edges are the ones the compiler can see - falling
off the end of the block, `return`, `break`, `continue`, and `?` - and each
runs that block's pending defers in LIFO order. `defer` therefore costs
nothing a hand-written statement at each of those edges would not.

A **panic is not one of those edges**. It is a violated invariant with no
`recover` to reach (§8.5), so a panicking goroutine's pending defers do not
run: on the main goroutine the process crashes, and a spawned goroutine's
panic ends that goroutine alone. Cohort accounting does not ride on `defer`
for this reason - the spawn wrapper's unwind backstop retires every cohort
the goroutine still had open, so a panicking child is still counted, still
reported, and never leaves its cohort undrained. Anything that must hold
across a panic belongs on that backstop, not on a `defer`.

### 8.5 Recovering from a panic

There is no `recover`, and no user-callable `catch_unwind`. A panic
marks a violated invariant, and the failures a caller is meant to handle
travel as `Result` (§9).

What a program can do with a panic is contain it and observe it.
`spawn(f)` is the containment boundary: a panic ends that goroutine
alone, and `handle.join() -> Result<T, String>` delivers the message to
the joiner. `runtime::set_panic_hook` observes
one before it unwinds.

### 8.6 Two concurrency primitives

The language has exactly two concurrency concepts: `cohort { }` and
`spawn`. Every other concurrency shape is a library function over them, a
cohort header setting, or a runtime capability - never a new construct.

A construct is considered only after both of these fail:

1. It is writable with `cohort` + `spawn` today, in which case it belongs in
   the standard library.
2. It is missing a cohort configuration key or a runtime capability, in
   which case that is what gets added. A header setting is a value and costs
   no grammar.

`sync::shield(f)` and `sync::with_timeout(f, ms)` are what the first answer
looks like: both are ordinary functions written with `cohort` + `spawn` over
the `cancellable:` and `timeout:` settings, and neither adds a word to the
grammar. `shield` answers `f`'s own value from a region cancellation does not
reach; `with_timeout` answers `Ok(value)` when `f` finished inside the bound
and an `Err` naming the bound when it did not. The shielded and bounded
regions are still counted, still drained, and still named if they never
finish.

Ownership is fixed at the spawn: a goroutine belongs to the cohort that was
current when `spawn` ran, and nothing moves a running goroutine between
cohorts. Detachment, if it ever exists, is a header setting rather than a
second spawn form.

No cohort header enum shares a name with a stdlib module or type, so a
setting can never read as the module it is not: the isolation setting is
`isolation: Isolation::Shared` / `Isolation::Thread` rather than `context:`,
because `context::Context` is the cancellation type a cohort may one day
inherit.

### 8.7 `unsafe`

```
unsafe { ... }
unsafe fn raw_thing() { ... }
```

`unsafe { ... }` blocks and `unsafe fn` declarations **parse and run**,
but `unsafe` grants no additional powers today: there are no raw
pointers (§3.4), so there is nothing unsafe to do. An `unsafe { ... }`
block evaluates exactly like an ordinary block expression, and calling
an `unsafe fn` needs no `unsafe` ceremony. The keyword is accepted for
Rust source compatibility and as a forward-compatible marker.

`unsafe` never disables automatic memory management or affects memory
reclamation.

Source-level `extern "C"` items are **not** an `unsafe` power - they
are rejected at parse time (`GP0016`). The sole FFI surface is the
`gossamer-binding` ABI. See §12.

---

## 9. Error handling

Errors are values of types implementing the `Error` trait. Because
there is no `dyn Trait` (§3.11), the cause chain is walked through the
concrete `errors::Error` accessors rather than a `&dyn Error`:

```
pub trait Error: Display + Debug {
  fn message(&self) -> String          // this error's own message
  fn cause(&self) -> Option<Error>     // next link in the chain, if any
}
```

Use `Result<T, E>` to signal failure; `?` to propagate. `panic` is for
unrecoverable conditions only (array out-of-bounds, unwrap on `None`,
divide by zero on integers, explicit `panic` in code).

No exceptions, no `throw`, no `try/catch` in user code (the `?`
operator handles control flow).

**`Result` is `#[must_use]` by default.** A `Result<T, E>` expression
whose value is discarded is a compile error (`GT0007`). Dropping an
error on the floor must be an intentional act.

A value is discarded when it is used as a statement, and equally when
it is the value of a construct whose own value is discarded: the tail
expression of a block, either branch of an `if`, any arm of a `match`,
and the body of a `for`, `while`, or `loop`. An else-less `if` is
typed `()` while its branch keeps the branch's own type, so
`if ready() { flush() }` discards `flush()`'s `Result` and is
reported at the call.

Two forms discard deliberately and are accepted:

- `let _ = expr` - acknowledges the value at the call site.
- `#[allow(unused_result)]` on an item, or `#![allow(unused_result)]`
  on a source file, covering everything inside it.

Consuming the value is not discarding it: `?`, a `match` on the
`Result`, binding it with `let`, and passing it to another call all
satisfy the rule.

**`#[must_use]` on user declarations.** A `fn`, `struct`, or `enum`
may carry `#[must_use]`. Discarding the return of such a function, or
a value of such a type, is `GT0064` under the same rules and the same
two escape forms. This is how a type other than `Result` - a guard, a
builder, a handle whose whole point is the value - opts into the same
guarantee.

```gossamer
#[must_use]
struct Guard { held: bool }

fn acquire() -> Guard { Guard { held: true } }

fn main() {
    acquire()              // GT0064: unused value
    let _guard = acquire() // held for the scope
}
```

---

## 10. Standard library

This is an outline; full API docs ship with the first implementation.

### 10.1 `std::fmt`

- `println(args...)` - variadic print-with-newline. Each argument
  implements `Display`.
- `eprintln(args...)`.
- `format(fmt_str, args...)` - returns `String`. `fmt_str` is a
  compile-time-validated format string (`{}` placeholders).
- `print`, `eprint` without newline.
- `Display`, `Debug` traits with derive support.

### 10.2 `std::io`

- `Reader`, `Writer` traits.
- `BufReader`, `BufWriter`.
- `std::io::stdin()`, `stdout()`, `stderr()`.
- `copy<R: Reader, W: Writer>(r: &mut R, w: &mut W) -> Result<u64, Error>`
  (static dispatch via generic bounds; there is no `dyn`, §3.11).

### 10.3 Filesystem, paths, processes, environment

Four modules, each with one job. They are separate imports; importing
one does not bring in its siblings.

**`std::fs`** - files and directories. Types `File`, `DirInfo`,
`OpenOptions`. Reading and writing: `read`, `read_to_string`, `write`,
`open`, `create`, `copy`, `rename`. Directories: `read_dir`, `walk_dir`,
`create_dir`, `create_dir_all`, `remove_file`, `remove_dir`,
`remove_dir_all`. Interrogation: `exists`, `is_file`, `is_dir`,
`is_symlink`, `file_size`, `metadata`, `canonicalize`. Temporaries:
`temp_dir(prefix)`, `temp_file(prefix)` create one; `env::temp_dir()` is
the system directory they live in.

An entry from `read_dir` / `walk_dir` already carries `is_file`,
`is_dir`, `is_symlink`, `size`, `path`, and `name`. Read those fields
rather than re-querying the path: the second lookup is slower and can
disagree with the first if the directory changed underneath.

**`std::path`** - path strings, without touching the filesystem.
`join`, `split`, `components`, `parent`, `file_name`, `file_stem`,
`extension`, `is_absolute`, `normalize`, `starts_with`, `prefixes`,
`unique_prefixes`, `matches`, `glob`, `walk`. Separator handling is
per-platform, so a path built with `join` is correct on every supported
target. `file_name`, `file_stem`, and `extension` return `Option<String>`
because not every path has one.

**`std::process`** - running other programs. `run(prog, args)` executes
directly with no shell and returns `Result<{stdout, stderr, code},
String>`. `spawn_piped(prog, args) -> Result<Child, errors::Error>`
drives an interactive child through `write_stdin`, `close_stdin`,
`read_line`, `read_stdout`, `wait`, and `kill`. Also `spawn`,
`pipeline_run`, `wait_timeout`, `kill_group`, `signal`, `id`, `exit`,
and `abort`. There is no `Command` builder; that shape belongs to the
Rust bindings, not to Gossamer.

**`std::env`** - the process environment. `args`, `program_name`, `var`,
`set_var`, `unset_var`, `current_dir`, `set_current_dir`, `home_dir`,
`temp_dir`.

**`std::os`** holds only what is genuinely about the host and fits none
of the four: `family` and `arch`, plus the `std::os::signal` and
`std::os::user` submodules. `std::os::exec` is a compatibility alias for
`std::process` and is Deprecated; new code writes `std::process`.

Fallible operations return `Result` (§9), so a discarded call is a
compile error rather than a silently ignored failure.

### 10.4 `std::iter`

**One obvious way.** Transformations (`map`, `filter`, `fold`,
`sum`, `min`, `max`, `count`, `any`, `all`, `find`, `position`,
`take`, `step_by`, …) are **methods on sequences**, and ranges carry
them too: `(1..5).map(|i| i * i).sum()`, `xs.filter(p).count()`. No
import is needed. Mutating helpers (`xs.push`, `xs.pop`, `xs.sort`,
`xs.swap`, `m.inc`, `m.or_insert`) are methods for the same reason -
they operate by side-effect on the receiver.

The same transformations also exist as **free functions in
`std::iter`** (§4.6), which is the shape `|>` pipelines want:
`0..n |> iter::sum`. Each takes its data first, so `iter::map(xs, f)`
reads as the `xs.map(f)` it stands for. The two spellings name one
implementation; the method form is the default and the free form exists
for piping.

Arrays and slices reach the eager combinators through `iter()` first;
`Vec` carries them directly. `%i` in the REPL reports each type's real
method surface.

The iterator protocol is one trait. User code declares it as
needed (or any name; the for-loop dispatch only checks for a
`.next() -> Option<T>` method on the receiver):

```
trait Iterator {
    fn next(&mut self) -> Option<i64>  // pick a concrete item type
}
```

Associated types (`type Item`) parse but the typechecker
currently does not project through them; declare the item
type concretely (`Option<i64>`, `Option<String>`, …) for now.

Any type providing a `next(&mut self) -> Option<T>` method
ranges with `for`:

```
for x in iter { body }
```

The HIR desugars to

```
{
    let mut __for_iter = iter
    loop {
        match (&mut __for_iter).next() {
            Some(x) => body,
            None => break,
        }
    }
}
```

- binding the iter once outside the loop so each `.next()`
call advances the same state. `IntoIterator` is implicit:
`Vec<T>`, `Map<K,V>`, ranges, `Receiver<T>`, and any
user struct with `fn next(&mut self) -> Option<T>` are all
iterable directly. A range stored in a binding retains its iterator state:
`let a = 0..3` followed by `for i in a { body }` visits `0`, `1`, and `2`.

The public `std::iter` module is currently Experimental and eager. Its free
functions take their data first (§4.6), accept `Vec<T>` inputs, and
materialize `Vec` results for transformations such as `map`, `filter`, `take`,
`skip`, `zip`, `chain`, `flatten`, `flat_map`, `scan`, `windows`, and `chunks`.
Consumers including `fold`, `reduce`, `sum`, `count`, `any`, `all`, `find`, and
`position` traverse that materialized sequence.

Language `for` loops and range iteration remain single-pass. They do not imply
that `std::iter` adapter chains are lazy. The accepted staged protocol specifies
ownership, invalidation, closure state, early termination, typed MIR operations,
tier parity, and edition migration in
[`docs_src/design/lazy_iterators.md`](docs_src/design/lazy_iterators.md). The
current eager signatures remain Experimental until that protocol is implemented.

Current eager example:

```
let squares = [1, 2, 3, 4]
  |> |v| iter::filter(v, |n| n % 2 == 0)
  |> |v| iter::map(v, |n| n * n)
let total = squares |> iter::sum
```

### 10.4a `std::option`

Free-function chaining surface for `Option<T>`. Every entry takes the
option first, so the free form reads as the method it mirrors.

- `map(opt, f) -> Option<U>`.
- `and_then(opt, f) -> Option<U>` - flat-map.
- `filter(opt, p) -> Option<T>`.
- `unwrap_or(opt, v) -> T`, `unwrap_or_else(opt, f) -> T`.
- `or(opt, alt) -> Option<T>` / `or_else(opt, f) -> Option<T>`.
- `iter(opt) -> Vec<T>`.
- `is_some(opt) -> bool`, `is_none(opt) -> bool`.
- `flatten(opt: Option<Option<T>>) -> Option<T>`.
- `zip(opt, other) -> Option<(T, U)>`.

The same surface is mirrored as methods on `Option<T>` for the
Rust-style call form (`opt.map(f)`, `opt.unwrap_or(0)`). Pick the
form that fits the surrounding code; don't mix in one chain.

### 10.4b `std::result`

Free-function chaining surface for `Result<T, E>`. Every entry takes
the result first, so the free form reads as the method it mirrors.

- `map(r, f) -> Result<U, E>`.
- `map_err(r, f) -> Result<T, F>`.
- `and_then(r, f) -> Result<U, E>` - flat-map.
- `or_else(r, f) -> Result<T, F>`.
- `unwrap_or(r, v) -> T`, `unwrap_or_else(r, f) -> T`.
- `ok(r) -> Option<T>`, `err(r) -> Option<E>`.
- `is_ok(r) -> bool`, `is_err(r) -> bool`.

The `?` operator (§4.5) remains the right tool for short-circuit
propagation; `result::map` / `result::and_then` are for in-pipeline
transformation when the chain doesn't return from the enclosing fn.

### 10.5 `std::strings`

- Split, join, trim, contains, replace, find, lines, chars, bytes,
  to_lowercase, to_uppercase, starts_with, ends_with, repeat.

### 10.6 `std::strconv`

- `parse_i64(s: &String) -> Result<i64, ParseError>`
- `parse_f64`, `parse_bool`, etc.
- Formatting via `fmt::format!`.

### 10.7 `std::collections`

- `Vec<T>`, `Map<K, V>`, `BTreeMap<K, V>`, `Set<T>`, `Deque<T>`,
  `Queue<T>`, and `Stack<T>`. Sequence, heap, queue, stack, deque, and ordered-container
  modules remain Experimental unless promoted by the feature registry.
  Each container has exactly one name: `HashMap`, `HashSet`, `VecDeque`,
  `VecQueue`, `VecStack`, `BinaryHeap`, `MaxBinaryHeap`, and `MinBinaryHeap`
  are rejected with `GR0006`.

### 10.8 `std::sync`

- `Mutex<T>`, `RwLock<T>` (parking_lot-style: no poisoning).
- `Once`, `WaitGroup`, `Barrier`.
- `AtomicI64`, `AtomicU64`, and `AtomicBool`. Raw-pointer atomics are not
  exposed because the safe language has no raw-pointer surface.

### 10.9 `std::time`

- `time::sleep(millis: u64)`.
- `Instant`, `Duration`.
- `SystemTime`.
- `time::now() -> SystemTime`.

### 10.10 `std::net` and `std::http`

- `net::TcpListener`, `TcpStream`, `UdpSocket`.
- `http::Server`, `http::Client`, `http::Request`, `http::Response`.
- `http::serve(addr: String, handler: impl Handler) -> Result<(), Error>`.

#### Namespace boundaries

`std::path` operates on lexical filesystem paths using the target platform's
path grammar. It performs neither URL parsing nor percent escaping; filesystem
I/O belongs to `std::fs`. `std::net::url` parses network URLs and escapes URL
components. It is not an HTTP router or a filesystem path API.

`std::process` is the canonical API for the current process and child
processes. `std::os::exec` is a deprecated compatibility facade retained for
the 0.x line; new APIs land only in `std::process`. Both remain Experimental
until cancellation, blocking behavior, and platform differences have a Stable
contract.

HTTP/3 remains Experimental under the historical `std::http_h3` spelling.
There is intentionally no `std::http::h3` alias: a second public name before
streaming and resource-limit semantics are complete would create a
compatibility promise without improving fidelity.

### 10.11 `std::encoding::json`, `std::encoding::csv`

- Dynamic surface: `json::parse(text) -> Result<json::Value, Error>`,
  `json::render(value) -> String`, plus the `json::{get, at, len,
  is_null, as_str, as_i64, as_f64, as_bool, as_array, keys}` query
  helpers.
- Strict, typed surface: every named struct in the program
  auto-derives a pair of generic serializer free functions, invoked
  with a turbofish type argument (there are no `Type::from_json`
  methods):
  - `from_json::<Type>(text: &String) -> Result<Type, errors::Error>`
  - `to_json::<Type>(value: Type) -> Result<String, errors::Error>`.
  `from_json` is the canonical one-line, serde-style deserializer:
  it validates each field against the declared field type
  recursively (nested structs by source name, `Vec<T>` / `[T; N]` /
  tuples / `Option<T>` / `Map<String, V>` walk
  through, `json::Value` fields pass through). Missing required
  fields and type mismatches surface as
  `Result::Err(errors::Error)` with a path-qualified message.
- Serialization is automatic (every struct gets `to_json::<T>` /
  `from_json::<T>` using the source field names verbatim);
  `#[derive(Serialize, Deserialize)]` is rejected (`GT0025`).
- The turbofish may spell its target through a type alias of either form
  (§3.12); the codec is the target struct's, and an opaque alias
  serializes as its representation.
- The typed surface covers a concrete struct whose fields it can
  classify. A generic struct, an enum, or a name that is not a struct
  has no codec and is reported as `GP0039`, and a struct with one
  unclassifiable field as `GP0022`; read those shapes with `json::parse`
  instead, or hand-write the function.

### 10.12 `std::thread`, `std::sync::channel`

- `thread::yield_now()` and `thread::num_cpus()` expose OS-thread scheduling
  hints and CPU availability only. There is no user-facing `thread::spawn`.
- `spawn(f)` creates a Gossamer goroutine; channels coordinate
  goroutines with `channel()` and `channel(cap)` from `std::sync`. There is
  no `std::channel` module path.
- Runtime workers, blocking pools, and protocol threads are implementation
  details, not a language-level thread API.

### 10.13 `std::panic`

- `panic(msg: String)`.
- A panic is goroutine-scoped: a spawned goroutine's panic ends that
  goroutine alone, and a main-goroutine panic is fatal (exit 101).
  `spawn(f)` reports it through `handle.join() -> Result<T, String>`.
- `runtime::set_panic_hook` observes a panic before it unwinds. There is
  no `catch_unwind`: a panic marks an invariant violation, and `Result`
  carries the failures a caller is meant to handle (§9).

---

## 11. Build and runtime

### 11.1 Targets

The table below is the support contract mirrored by
`conformance/target_matrix.tsv`. Tier 1 runs VM, JIT, and AOT on native CI.
Tier 2 supports release AOT output with VM differential evidence. Artifact and
registered rows are not supported execution targets.

| Tier | Target triple | Evidence |
|---|---|---|
| Tier 1 | `x86_64-unknown-linux-gnu` | native VM/JIT/AOT |
| Tier 1 | `aarch64-unknown-linux-gnu` | native VM/JIT/AOT |
| Tier 1 | `aarch64-apple-darwin` | native VM/JIT/AOT |
| Tier 1 | `x86_64-pc-windows-msvc` | native VM/JIT/AOT |
| Tier 2 | `x86_64-unknown-linux-musl` | release AOT with VM differential |
| Tier 2 | `aarch64-unknown-linux-musl` | release AOT under QEMU with VM differential |
| Artifact only | `x86_64-apple-darwin` | release artifact build only |
| Registered | `riscv64gc-unknown-linux-gnu` | compile check only |
| Registered | `wasm32-unknown-unknown` | platform-agnostic crate check only |
| Registered | `wasm32-wasi` | platform-agnostic crate check only |

### 11.2 Linking

Static linking by default. The produced binary embeds the runtime. On
Linux, `musl` target produces a zero-libc static binary identical in
deployment experience to `CGO_ENABLED=0` Go.

Dynamic linking for FFI is **not** available through a source-level
syntax. See §12 for the supported FFI mechanism (`[rust-bindings]`).

On Linux, `gos build --release` produces a fully-static musl binary by
default when the host-architecture musl rustup target
(`x86_64-unknown-linux-musl` or `aarch64-unknown-linux-musl`) is
installed; pass `--dynamic` to force the dynamic-glibc link path. The
`--target` flag selects a cross-compilation triple (see §11.4).

On a Raspberry Pi (any `aarch64` Linux), `gos` is fully
self-contained - the bytecode VM and its in-process Cranelift JIT need
no external tools. `gos build` shells out to the device's system LLVM
(`llc`/`opt`) and a C compiler (`cc`) for codegen and linking, so those
must be installed to compile natively on the Pi.

### 11.3 Compile modes

| Mode | Command | Backend | Pipeline | Speed | Output quality |
|---|---|---|---|---|---|
| Interpret | `gos run file.gos` | Bytecode VM | Direct dispatch; in-process Cranelift JIT tiers up hot bodies | Fastest cold start | No native codegen |
| Debug build | `gos build` | LLVM | checked arithmetic, `opt -O1`, then `llc -O0` | Sub-second for small programs | Optimized enough for development while preserving debug overflow traps |
| Release build | `gos build --release` | LLVM | `opt -O3 \| llc -O3 -mcpu=native -mattr=+prefer-256-bit` | Seconds for thousands of LoC | Vectorised, inlined |

LLVM is the canonical native backend; the Cranelift code path is
reserved for the in-process JIT inside `gossamer-interp` and is not
reachable from `gos build`. Any MIR shape the LLVM lowerer cannot
handle is a hard `gos build` failure rather than a silent per-function
Cranelift fallback. The register-based bytecode VM is the sole `gos
run` / `gos test` engine and lowers every construct natively; there is
no tree-walker interpreter. VM correctness is pinned by the tier-parity
suite and the VM-vs-LLVM-AOT differential.

### 11.4 Cross-compilation

```
gos build --target aarch64-unknown-linux-gnu  --release app.gos
gos build --target aarch64-unknown-linux-musl --release app.gos
```

All targets share the same frontend and MIR; only the backend pass and
the link differ. `gos build --target <triple>` produces a real native
binary when:

1. `<triple>` is a registered target;
2. a runtime archive for the target resolves - shipped in the
   toolchain's `lib/<triple>/`, set via
   `GOS_RUNTIME_LIB_<TRIPLE>`, or built with `cargo build --release
   --target <triple> -p gossamer-runtime` (no fallback to the host
   archive - a missing target archive is a hard error, never a
   foreign-architecture mislink); and
3. a linker for the target is available - the conventional
   `aarch64-linux-gnu-gcc` for a same-OS Linux cross, or `ld.lld` /
   `rust-lld` for an OS-crossing link (overridable via
   `CARGO_TARGET_<TRIPLE>_LINKER` / `GOS_CROSS_CC`).

The executable supported-target contract is
[`conformance/target_matrix.tsv`](conformance/target_matrix.tsv). Tier 1
executes the bytecode VM, forced-JIT VM, and LLVM AOT fixture suite natively
on Linux x86_64/aarch64, Apple Silicon macOS, and Windows x86_64. Tier 2 is
the Linux-musl AOT path for x86_64/aarch64: CI executes its output natively or
under QEMU and compares it with the pure bytecode VM. The musl-static target
is host-agnostic when the required runtime archive and linker are installed.
Cross-host glibc links require `GOS_CROSS_SYSROOT` and are not part of the
supported target contract. Artifact-only and registered-but-unsupported
triples are deliberately listed in that matrix; a locally accepted triple is
not support.

Cross-compiling *to* macOS or Windows as a target is not yet supported;
it requires external SDKs (osxcross + the Apple SDK, mingw-w64) whose
licensing and availability sit outside the toolchain.

---

## 12. FFI

The source-level `extern "C" { ... }` and `#[no_mangle] extern "C" fn`
item forms are **rejected at parse time** with diagnostic code
`GP0016`. Gossamer has exactly one FFI surface: the `[rust-bindings]`
section of `project.toml`, consumed by the `gossamer-binding` crate.
The `extern` keyword remains reserved.

Foreign code is brought into a Gossamer project by declaring a Rust
crate under `[rust-bindings]` in `project.toml`. The crate registers
its entry points with `gossamer_binding::register_module!`; the
toolchain compiles the crate into a per-project runner and links it
into the produced binary or interpreter. Types crossing the boundary
use the `gossamer-binding` ABI (`Unit`, `Bool`, `I64`, `F64`, `Char`,
`String`, `Tuple`, `Vec`, `Option`, `Result`, `Opaque`, `Any`,
`Bytes`, `Map`, `Variant`, `Callback`).

```toml
# project.toml
[rust-bindings]
my-libc-wrapper = { path = "./vendor/my-libc-wrapper", version = "0.1" }
```

```rust
// vendor/my-libc-wrapper/src/lib.rs
use gossamer_binding::register_module;
register_module!("libc", {
    fn malloc(size: u64) -> u64 { /* ... */ }
    fn free(ptr: u64) -> () { /* ... */ }
});
```

```gossamer
// program.gos
use libc::{malloc, free}
fn main() {
    let p = malloc(1024)
    free(p)
}
```

FFI rules:

- Every type that crosses the boundary uses one of the
  `gossamer-binding` ABI variants. Raw `*mut T` / `*const T`
  Gossamer pointers are not part of the boundary; integer handles or
  `Opaque<T>` carry pointers when needed.
- A binding may panic; the binding ABI wraps each entry in
  `std::panic::catch_unwind` and converts panics to a returned
  `Result::Err`.
- Calls into the runner enter a scheduler state that releases the
  P, so long-running native calls do not block other goroutines.

See `docs_src/libraries.md` and `crates/gossamer-binding/ABI_0_4.md`
for the binding ABI's full surface. A source-level `extern "C"` item
form is not implemented and remains out of scope.

---

## 13. Attributes

```
#[derive(Debug, Default, PartialEq)]
#[inline]
#[no_mangle]
#[repr(C)]
#[cfg(target_os = "linux")]
#[test]
```

Only a curated set is recognized (unknown attributes warn rather than
error for forward-compatibility).

---

## 14. Compiler-known calls

Gossamer has a small fixed set of compiler-known calls, expanded at parse
time; there is no runtime macro engine and no user-defined macros. Six are
format-shaped (below); plus the desugar calls `matches(e, pat)`,
`todo(...)` / `unimplemented(...)` / `unreachable(...)`, and `dbg(e)`, and
the build-time `regex::compile(...)` / `sql::statement(...)` /
`codegen(...)`.

Every one is written as an ordinary call. The set is closed and
recognised at the `(`, so nothing a sigil would disambiguate remains: a
`!` after one of these names is `GP0049`, and `regex!` / `sql!` - which
now live in their modules - are `GP0051`.

| Call | Returns | Destination |
|---|---|---|
| `format("…", …)` | `String` | - |
| `println("…", …)` | `()` | stdout + newline |
| `print("…", …)` | `()` | stdout, no newline |
| `eprintln("…", …)` | `()` | stderr + newline |
| `eprint("…", …)` | `()` | stderr, no newline |
| `panic("…", …)` | `!` | unwinds with the rendered message |

Stdout is buffered so that the several writes a formatted line arrives as cost
one syscall, and the buffer drains on the newline that ends a write - the same
guarantee on a terminal, a pipe, and a file, and identical on every tier. A
`print` with no terminator stays buffered until a later newline, an explicit
`io::stdout().flush()`, or exit. The stderr calls are unbuffered and flush
stdout first, so interleaved output keeps its order.

Beyond the six format calls and the desugar / build-time calls listed
above, **every `name!(...)` is a parse error** (`GP0001`), with a
diagnostic steering the user to the plain-function form. This includes
the Rust macros a newcomer reaches for: there is no `vec!`, `map!`,
`set!`, `write!`, `writeln!`, `assert!`, `assert_eq!`, `debug_assert!`,
`include_str!`, `include_bytes!`, or `env!`.

- Collection literals use `#[a, b]` for `Vec` values. Use `[a, b]` /
  `[v; N]` for fixed arrays, `{}` or `{k: v}` for `Map`, and `#{a, b}` for set
  values; there is no `vec!`, `map!`, or `set!`. A repeat literal follows its
  container's spelling: `[v; N]` is a fixed array and `#[v; N]` is a `Vec`.
- `assert(cond[, msg])` and `assert_eq(a, b[, msg])` are prelude
  *functions* called without a `!`; `std::testing` provides the
  non-panicking `check*` variants.

User-defined macros do not exist. Compile-time metaprogramming is
instead **Zig-style `comptime`** - ordinary Gossamer functions and
blocks evaluated at compile time, rather than a `macro_rules!`-style
macro language, with no separate macro grammar, no hygiene model, and no
token-tree DSL.

`comptime` is contextual (§2.4): the word opens a block directly before
`{`, declares a function directly before `fn`, and marks a parameter
when a pattern rather than its own `:` follows, so a binding or field
may still be called `comptime`.

A `comptime { ... }` block, every call to a `comptime fn`, and every
argument to a `comptime` parameter are evaluated on the bytecode VM
during compilation and folded to a literal, so the bytecode VM, the
Cranelift JIT, and the LLVM AOT backend all compile the identical
constant. A region must read only compile-time-known values (literals,
consts, other `comptime fn` results) and evaluate to a scalar or string;
otherwise it is a compile error. `typeInfo::<T>()` reflects a type's shape
at compile time as `[(String, String)]` - a named struct's fields, a tuple
struct's positions, or an enum's variants and payloads, with the arguments
substituted in for a generic instantiation - so a `comptime fn` can
generate per-type code. A type with nothing to reflect is `GR0012`. The
`regex::compile` / `sql::statement` calls validate their
argument at build time, failing the build on malformed input. See the
[`comptime` language page](docs_src/language/comptime.md).

Gossamer does not provide runtime reflection. Programs that require dynamic
type inspection must use an explicit generated schema, tagged representation,
or a `comptime`-generated adapter.

The toolchain implements the six format-shaped calls (`println`,
`print`, `eprintln`, `eprint`, `format`, `panic`), the desugar calls
(`matches`, `todo`, `unimplemented`, `unreachable`, `dbg`), and the
build-time `regex::compile` / `sql::statement` / `codegen`. Every
`name!(...)` form - including `vec!`, `write!`, `writeln!`, `map!`,
`set!`, `assert!`, `assert_eq!`, `debug_assert!`, `include_str!`,
`include_bytes!`, and `env!` - is rejected at parse time (`GP0001`), and
a `!` after a compiler-known name is `GP0049`. User-defined macros are
out of scope.

---

## 15. Grammar summary

A condensed top-level grammar:

```
SourceFile   = { UseDecl } { Item }
UseDecl      = "use" UseTarget [ "as" Ident ] [ "{" UseSpec "}" ]
UseTarget    = ProjectUse | ModulePath
ProjectUse   = StringLit [ "::" ModulePath ]
ModulePath   = Ident { "::" Ident }
Item         = FnDecl | StructDecl | EnumDecl | TraitDecl | ImplDecl
             | TypeAlias | ConstDecl | StaticDecl
             | ModDecl | AttrItem

FnDecl       = [Attrs] [ "pub" ] [ "unsafe" ] "fn" Ident [ Generics ]
               "(" [ ParamList ] ")" [ "->" Type ] [ WhereClause ] Block
               // `-> Type` is omitted only when the body answers a unit;
               // a body whose tail expression yields a value without it
               // is GT0074
ParamList    = SingleLineParams | MultiLineParams
SingleLineParams = Param { "," Param }
MultiLineParams  = newline Param { [ "," ] newline Param } [ "," ] newline
Param        = ( "self" | "&" "self" | "&" "mut" "self" | Pattern ":" Type )

Block        = "{" { Stmt } [ Expr ] "}"
Stmt         = LetStmt | Item | ExprStmt | DeferStmt
ExprStmt     = Expr

Expr         = LiteralExpr | PathExpr | CallExpr | MethodCall | FieldAccess
             | IndexExpr | UnaryExpr | BinaryExpr | AssignExpr | CastExpr
             | IfExpr | MatchExpr | LoopExpr | WhileExpr | ForExpr
             | BlockExpr | ClosureExpr | ReturnExpr | BreakExpr | ContinueExpr
             | TupleExpr | StructExpr | ArrayExpr | RangeExpr | UnsafeExpr
             | TryExpr | RefExpr | SelectExpr | MacroCall | PipeExpr

PipeExpr     = Expr "|>" PipeRhs
PipeRhs      = PathExpr                                  // x |> f
             | PathExpr "(" [ ArgList ] ")"              // x |> |v| f(a, b, v)
             | Expr "." Ident                            // x |> obj.m
             | Expr "." Ident "(" [ ArgList ] ")"        // x |> obj.m(a)
             | "(" Expr ")"                              // x |> (closure)

Pattern      = (see §5)
Type         = (see §3)
```

Full grammar lives in `grammar/grammar.bnf` in the implementation
repository.

---

## 16. Project tool

The `gos` tool reads `project.toml` to resolve dependencies, fetch
sources, and drive the compiler. Resolution is tool-driven; the
language grammar knows nothing about networks, tarballs, or version
numbers.

### 16.1 Manifest

See §6.4. The only required keys are `project.id` and
`project.version`. Everything else is optional.

### 16.2 Sources

Four dependency source kinds (§6.7), in order of typical preference:

- **Registry** - HTTP endpoint serving signed tarballs, matched by DNS
  prefix via `[registries]`.
- **Git** - clone and check out a tag, branch, or rev.
- **Local path** - for side-by-side development of related projects.
- **URL tarball** - plain archive with mandatory sha256.

No source kind is privileged; all four interoperate.

### 16.3 Lockfile

`project.lock` is a TOML file capturing the exact resolution of every
transitive dependency: project identifier, concrete source, and source
tree sha256. Checked into version control for reproducible builds.

### 16.4 Version selection

A dependency declares a version **requirement** in one of two spellings, and
there is no third:

| Spelling | Meaning |
|---|---|
| `x.y.z`, or `=x.y.z` | Exactly that version |
| `^x.y.z` | That version or any later one |

A bare literal pins, because a manifest that names a version and gets a
different one is a surprise nobody asked for. `^` opts into anything newer,
with no upper bound: a ceiling would be a guess about code that has not been
written yet, and `project.lock` is where a reproducible graph is recorded.

The resolver picks the highest available version that satisfies every
consumer's requirement, so a pin and a floor on the same dependency resolve
only when the pinned version also clears the floor. The selected graph is
pinned by `project.lock`; `--locked` rejects drift instead of selecting a newer
version.

Prerelease versions participate in SemVer precedence but are selected only by a
requirement that itself names a prerelease of the same `x.y.z`: `^1.2.0` never
resolves to `1.3.0-rc.1`, and neither does `^1.2.0-rc.1`. Build metadata is
retained in published versions and lockfiles but does not change resolution
precedence.
### 16.5 Caches

Package source trees are content-addressed under
`~/.gossamer/cache/pkg/<sha256>/source/`, overridden by `GOS_CACHE_DIR`.
Frontend caches follow `GOSSAMER_CACHE_DIR`, then the platform XDG/user cache
location. Project-native IR objects use `.gos-cache/ir-cache/`. These cache
locations are performance details and may be removed with `gos clean`. Use
`gos cache` to inspect every active root and `gos cache --prune` to remove
entries older than 30 days or beyond the `GOS_CACHE_MAX_BYTES` total budget
(20 GiB by default). `gos clean --all` also clears Rust-binding runners,
package sources, and legacy build artifacts.

### 16.6 Subcommands

- `gos init` - create `project.toml` in the current directory.
- `gos new NAME` - scaffold a new project directory.
- `gos add ID[@VERSION]` - add a dependency entry.
- `gos remove ID` - remove a dependency entry.
- `gos build` - compile the current project.
- `gos` - interpret the current project.
- `gos test` - run the project's tests.
- `gos fetch` - resolve and download (but do not build) all deps.
- `gos update` - update deps within their declared ranges.
- `gos tidy` - parse project sources, remove direct project dependencies not
  referenced by a string-quoted project import, and canonicalize manifest
  ordering. Rust bindings are retained independently.
- `gos cache` - show cache classes and roots; `--path` prints paths only and
  `--prune` applies retention policy (`--dry-run` reports without deletion).
- `gos vendor` - copy deps into `./vendor/`.
- `gos doc` - generate HTML documentation.

### 16.7 Reproducibility

`gos build --reproducible` produces a bit-identical artifact across clean
builds when all of the following inputs are fixed:

- Toolchain version.
- Target triple.
- Source file contents (current project plus all lockfile entries).
- Build flags (release/debug, features).
- Linker and external tool versions.
- The reproducibility environment, including `SOURCE_DATE_EPOCH`.

Default builds prioritize host optimization and fast incremental reuse and are
not a universal bit-identity contract. Cranelift JIT code is an in-process
execution detail, not a reproducible artifact.

### 16.8 Registries (optional)

A registry is a plain HTTP service that maps `/v1/<project-id>/<version>`
to a signed tarball plus metadata. Any party may run one. Projects
opt in by listing the registry's DNS prefix in `[registries]`:

```toml
[registries]
"acme.dev" = "https://registry.acme.dev/v1"
```

No central registry is shipped with the toolchain and none is required
to use Gossamer. A project whose dependencies are all git or path
sources is a fully supported, registry-free setup.

The index supplies availability metadata only. A registry tarball must carry a
valid Ed25519 signature, and its advertised publisher key must match either an
existing `project.lock` pin or a root-manifest `[trusted-publishers]` binding.
The first index response alone must never establish publisher identity.

---

## 17. Versioning and compatibility

### 17.1 Language version and compatibility

- `gossamer-version` in the manifest states which toolchain a project is
  written against, spelled the way the release tag is. A bare version - or an
  explicit `=` - names that toolchain and no other; a `^` version names it as a
  floor and accepts every later one. `gossamer-version = "v0.55.0"` pins;
  `gossamer-version = "^v0.55.0"` does not. The leading `v` is optional on both.
  `edition` is Rust's spelling and is rejected by name.
- The field is optional. A toolchain the requirement does not accept is
  rejected by name, so a missing surface is reported against the manifest
  rather than the source that uses it. `gos new` scaffolds a `^` floor,
  because a project should not stop building at the next release unless its
  author asked for that.
- One toolchain has one source language: there is no edition selector, and no
  two projects built by the same `gos` see different semantics.
- Experimental syntax may change between versions, but it must be reported as
  Experimental by `gos feature-status` before it is accepted.

Edition 2026 keeps the historical eager `std::iter` signatures. In edition
2027, `iter::range`, `range_inclusive`, `map`, `filter`, `take`, `skip`,
`enumerate`, `chain`, and `zip` produce linear `Iterator<T>` state. `fold`,
`any`, `all`, `find`, `count`, `sum`, and `collect` consume that state once.
Adapters pull only on terminal demand, and `any`, `all`, `find`, `take`, and
`zip` stop as soon as their result is decided. A program that needs to
materialize a 2027 iterator uses `iter::collect`; a collection that already
holds its values traverses through its own methods (`xs.map(f)`, `xs.sum()`).

### 17.2 Standard library compatibility

- Stable entries retain their canonical module path, callable signature,
  observable error/result shape, and documented resource limits throughout a
  compatible toolchain line.
- Adding an optional field, method, error variant, or capability to an
  Experimental entry is not a Stable compatibility guarantee.
- Removing, renaming, or weakening a Stable entry requires a new edition or a
  documented compatibility shim that remains available for the old edition.
- A module's status is not inherited by undocumented implementation helpers;
  only manifest-listed paths are public contracts.

### 17.3 Project formats and generated artifacts

- `project.toml` and `project.lock` are versioned public formats. A compatible
  toolchain must either read an older supported version losslessly or reject
  it with an actionable migration diagnostic; it must not silently reinterpret
  a pin, registry identity, or build setting.
- Lockfile source pins, checksums, and publisher keys are integrity data. A
  compatible toolchain must preserve them byte-for-byte unless an explicit
  update operation changes them.
- Bindings, generated ABI declarations, diagnostics codes, and target triples
  are versioned outputs. Stable consumers may depend on documented names and
  machine-readable shapes, not on incidental formatting or allocation layout.

### 17.4 Toolchain and target policy

- Patch releases fix defects without changing Stable language or library
  semantics. Minor releases may add Stable surface but cannot break existing
  Stable programs in a supported edition/target pair.
- The supported-target matrix is published with each release. A target absent
  from that matrix is Experimental even when it happens to compile.
- `gos build --release` must fail when Stable code cannot be lowered by the
  selected release backend; it must not silently substitute an interpreter or
  alternate execution tier.

### 17.5 Declined features

The features below are **permanently declined**, not planned or deferred.
They are recorded here so their absence reads as a decision rather than a
gap, and so a proposal to add one starts from the stated reason. Each is
rejected at parse or check time with a diagnostic naming the alternative.

Power, but not complexity: a feature earns its place by making correct
code the shortest code, without adding a second spelling of an existing
idiom, a new type-system theory, or machinery the reader cannot see.

**Concurrency and control flow**

- **`async` / `await`.** Function coloring, `Send`/`'static`/`Pin`
  obligations, and runtime fragmentation are the most-documented regret
  surface in both Rust and Python. Goroutines and channels (§8) cover the
  same ground with no colored functions.
- **`panic` / `recover` at function level.** A panic is a violated
  invariant; recoverable failure is `Result` (§9). Containment is the
  goroutine boundary (§8.5).
- **Exceptions, `throw` / `try` / `catch`.** Same reason, plus invisible
  control flow at every call site.

**Types and the type system**

- **Lifetimes, a borrow checker, and the `move` keyword.** The most-cited
  Rust learning obstacle. Deterministic reference counting plus `arena { }`
  is the memory model (§7); the lexical `&mut` check (§7.5) is the
  intended ceiling.
- **`dyn Trait` and trait objects.** Object-safety rules, vtable variance,
  and years of repair work on return-position trait methods. Enums cover
  the heterogeneous cases (§3.11); the `Fn` callable stays the one carved-out
  fat pointer, because callbacks genuinely need it.
- **GATs, higher-kinded types, specialization, blanket impls.** This is the
  machinery behind "the language is getting too complex". Associated types
  without GATs (§3.8) is the stopping point.
- **General union types.** Full unions wreck inference; `Result<T, E>` plus
  exhaustive `match` is the shape that pays.
- **Full units of measure.** Admired for over a decade and copied by no
  one. The opaque-alias subset captures the real-world wins.
- **`i128` / `u128`.** Rejected with `GT0014`; `math::big` covers the need.
- **Nil, zero-value initialization, untyped error interfaces, inheritance.**
  These are the recurring complaints about the languages Gossamer replaces.

**Metaprogramming**

- **User macros, procedural or declarative.** `comptime` plus `codegen`
  (§7 of the skill card, `lang::comptime`) is the metaprogramming bet: no
  hygiene problem, no separate codegen language. The macro set is fixed
  (§14).
- **Type providers.** Compile-time network dependencies break hermetic
  builds. `comptime` over checked-in files is the deterministic subset.

**Syntax**

- **Comprehensions.** A second spelling of `filter().map()`.
- **Computation expressions / generic monad syntax.** `?`, goroutines, and
  `|>` cover the concrete cases without the abstraction.
- **Scope functions (`let` / `run` / `with` / `apply` / `also`).** Five
  interchangeable spellings with documented team confusion. A method chain
  is one mechanism with an explicit receiver (§4.6).
- **Context parameters and implicit receivers.** Implicitness is the cost
  center.
- **Push-style function iterators.** Pull-style `next()` is strictly
  easier to reason about and to stop early (§10.4).

---

## Appendix A - Differences from Go

1. No `nil`. All absence goes through `Option`.
2. No implicit zero-value for function types.
3. No interfaces in Go's sense - traits with explicit `impl`.
4. No `iota` - use `const` or an enum with explicit discriminants.
5. No type switch `x.(type)` - use `match` on an enum or `match` on
   a trait object with `Any::type_id`.
6. No labeled `goto`.
7. Semicolons are allowed only as same-line statement separators; trailing
   semicolons are invalid.
8. Visibility by `pub`, not by capitalization.
9. Generics syntax is `<T>`, not `[T]`.

## Appendix B - Differences from Rust

1. No lifetimes (automatic memory management removes the need).
2. No borrow checker (automatic memory management removes the need).
3. No `Drop` trait with deterministic destruction - use `defer` for
   cleanup tied to scope; the runtime reclaims memory.
4. No `Box<T>` / `Rc<T>` / `Arc<T>` - plain references are
   runtime-managed and safe to share across goroutines.
5. `&T` is a managed reference, not a borrow, and never a parameter type:
   a parameter is `T` or `&mut T` (§3.4). `&T` and `&mut T` have the same
   runtime; the distinction is a type-check-only aliasing hint.
6. No `async`/`await` - goroutines replace the entire async story.
7. No user macros. The built-in call names are ordinary calls, spelled
   without a `!`.
8. `select`, `defer`, `cohort`, `arena`, and `comptime` added, each as
   a contextual word rather than a reserved keyword (§2.4). `spawn` is a
   prelude call name and, being the one way to start a goroutine, is one
   nothing may declare (GR0020).
9. **Forward pipe `|>`** (left-associative). A bare step takes the piped
   value as its only argument; a step that writes arguments is a closure
   whose parameter is the slot the value fills. See §4.6. Rust has no
   pipe operator; Gossamer adds it as a first-class part of the grammar.
10. **Free-function combinator surface in stdlib.** `std::iter`,
    `std::option`, and `std::result` ship free-function chaining APIs
    (see §10.4-§10.4b) alongside the method surface that every
    collection and iterator carries, so the same operation reads the
    same way whether it is chained from a receiver or piped.

## Appendix C - Go features not ported

- `iota` (use `enum` discriminants).
- Embedded structs with method promotion (use explicit delegation or
  traits with default methods).
- `panic`/`recover` at function level. A panic is contained by the
  goroutine that raised it, and `spawn(f).join()` reports it as an
  `Err` (§8.5); recoverable failure is `Result` (§9).
- Init functions with ordering by import - replaced by explicit
  `fn init()` called in dependency-topological order.
- Untyped constants with arbitrary precision - literal constants have
  a default type and are coerced at use sites; infinite-precision
  compile-time arithmetic is not performed beyond what LLVM/Cranelift
  offer.
- `goto` - omitted.

---

*End of Gossamer specification (pre-1.0.0 draft).*
