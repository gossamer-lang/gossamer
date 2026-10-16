# `lang::keyword_arguments`

Keyword arguments and parameter defaults: a call may name any parameter (`volume(depth: 4, width: 2)`), and a parameter may declare a default known while compiling (`fn volume(width: i64, height: i64 = 2)`, `mode: Mode = Mode::Fast`, `parent: Option<Window> = None`) that is spliced into every call omitting it, meaning what it means where the function is declared. Positional arguments come first, then names. Both are caller-side spellings rewritten into the callee's declared order before type checking, so the calling convention is unchanged. A name on a method call is matched when every type declaring that method name would rewrite the call identically; when they disagree the call is reported (GR0013) rather than guessed.

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

A call may name the parameter each argument fills, and a parameter may
declare a default that callers can leave out.

```gossamer
fn volume(width: i64, height: i64 = 2, depth: i64 = 3) -> i64 {
    width * height * depth
}

fn main() {
    println("{}", volume(2))                              // 12
    println("{}", volume(2, 3))                           // 18
    println("{}", volume(width: 2, height: 3, depth: 4))  // 24
    println("{}", volume(depth: 4, width: 2, height: 3))  // 24
    println("{}", volume(2, depth: 10))                   // 40
}
```

Both are spellings at the call site. The compiler rewrites every call
into the order its callee declares before type checking, so the compiled
program is the same one you would get by writing the arguments in order.
The calling convention is untouched, and the bytecode VM, the JIT, and
native builds all compile the identical call.

## Naming an argument

Write `name: value` in place of a positional argument. A name selects the
parameter it fills, so named arguments may appear in any order.

Positional arguments come first, then names:

<!-- fragment -->
```gossamer
volume(2, depth: 10)        // width positionally, depth by name
volume(width: 2, 3)         // error[GR0013]
```

Once a name is used the remaining positions are no longer in written
order, so every later argument needs a name too.

A name has to name a parameter of the callee, and may be given once:

<!-- fragment -->
```gossamer
volume(depht: 3)            // error[GR0013]: `depht` is not a parameter
volume(width: 1, width: 2)  // error[GR0013]: `width` is given twice
```

## Declaring a default

Write `= value` after a parameter's type. A call that omits the parameter
gets that value spliced in at its position.

```gossamer
fn label(text: String, prefix: String = "item", times: i64 = 1) -> String
```

A default must be a value known while compiling: an integer, float,
string, char, byte, or bool literal, optionally negated (`-1`), a `const`
or an associated const, integer arithmetic over those, or a `comptime`
expression. A scalar const or arithmetic over consts folds to a literal
that is spliced into every call that omits it. A const is resolved where
the function is declared, so every call receives the declaration's value:

```gossamer
const DEFAULT_TIMEOUT: i64 = 30

fn connect(host: String, timeout: i64 = DEFAULT_TIMEOUT * 1000) -> String {
    f"{host} {timeout}"
}

fn main() {
    println(connect("db"))
}
```

A default may also hold data: an enum variant (`Mode::Fast`, `None`,
`Some(4)`, `Ok(0)`), a struct construction, a tuple, an array, a `Vec`,
`Map`, or `Set` literal, a function, or a const of any type, built from
values that are themselves known while compiling. Every name in it means
what it means where the function is declared, so a call site that has
another `Mode` or `ORIGIN` in scope still receives the declaration's:

```gossamer
enum Profile { Core, Compat }

struct Size { w: i64, h: i64 }

fn open(
    title: String
    profile: Profile = Profile::Core
    size: Size = Size { w: 800, h: 600 }
    parent: Option<String> = None
    tags: Vec<String> = #[]
) -> String {
    f"{title} {size.w}x{size.h} {parent} {tags.len()}"
}

fn main() {
    println(open("main"))
    println(open("tools", size: Size { w: 320, h: 200 }, parent: Some("main")))
}
```

A default whose value is computed at run time - a call to a function, a
closure, a `static`, or another parameter - is rejected:

<!-- compile_fail GR0014 -->
```gossamer
fn f(a: i64, b: i64 = a + 1)   // error[GR0014]: a parameter default must be a value known while compiling
```

Defaults are per call site. Two calls to the same function never share a
value, so a default of a String or any other owned type is safe.

## Methods and associated functions

Both forms work on methods and associated functions:

```gossamer
struct Rect { w: i64, h: i64 }

impl Rect {
    fn make(w: i64, h: i64 = 5) -> Rect { Rect { w: w, h: h } }
    fn scaled(&self, factor: i64 = 2) -> i64 { self.w * self.h * factor }
}

let r = Rect::make(w: 3)     // h defaults to 5
r.scaled()                   // factor defaults to 2
r.scaled(factor: 10)
```

A method call binds its names and defaults against the declaration its
receiver's type reaches, so another type that happens to declare a method
of the same name, with other parameters, changes nothing:

```gossamer
struct Socket { host: String }
struct Database { url: String }

impl Socket { fn connect(&self, timeout: i64 = 30) -> String { f"{self.host} {timeout}" } }
impl Database { fn connect(&self, retries: i64 = 3) -> String { f"{self.url} {retries}" } }

fn main() {
    let s = Socket { host: "a" }
    println(s.connect())
    println(s.connect(timeout: 5))
}
```

A receiver whose type is a generic parameter uses the declaration of the
trait that bounds it. Only a receiver whose type is not settled where the
call is written, with declarations that disagree, is reported (`GR0013`);
passing every argument by position always works.

## Diagnostics

| Code | Meaning |
|---|---|
| `GR0013` | A name that matches no parameter, is given twice, follows a positional argument, or is on a method whose receiver's type is not settled where several types declare it differently. |
| `GR0014` | A parameter default that is not a value known while compiling. |

`gos explain GR0013` and `gos explain GR0014` expand both.
