// A Tour of Gossamer - lesson data plus the page controller.
//
// Renders the step list, the current lesson (prose on the left, a live
// "Try Gossamer" playground on the right), and the prev/next controls
// into #tour-root, and keeps the visible lesson in sync with
// location.hash so every step is deep-linkable and the browser's
// back/forward buttons work without extra wiring.
//
// The playground module is imported lazily so the tour chrome paints
// immediately and a load failure degrades to a readable code listing
// instead of a blank panel.

const PLAYGROUND_URL = "../playground/playground.js";
const PLAY_HEIGHT = "320px";

const LESSONS = [
  {
    slug: "hello",
    title: "Hello + values",
    prose: `
      <p>Gossamer is a fast-compiling language with a Rust-flavoured
      surface and a Go-shaped runtime. Bindings are <strong>immutable by
      default</strong> - reach for <code>let mut</code> only when a value
      genuinely changes after construction.</p>
      <p>String literals are already <code>String</code>, so there is no
      <code>.to_string()</code> noise. An <code>f"..."</code> string
      interpolates any expression in braces - <code>{name}</code>,
      <code>{name.len()}</code>, <code>{pi * 2.0:.2}</code> - with an
      optional format spec after the <code>:</code>.</p>
      <p>Press <strong>Run</strong> (or Ctrl / Cmd + Enter) to execute the
      program on the right. Edit it freely and run it again.</p>`,
    code: `// Bindings are immutable by default; reach for \`let mut\` only when
// a value really changes. String literals are already \`String\`.
let name = "Gossamer"
let pi = 3.14159

let greeting = "hello, " + name
println(f"{greeting}!")

// A placeholder holds any expression, with an optional \`:spec\`.
println(f"{name} is {name.len()} bytes long")
println(f"pi is about {pi:.2}, tau about {pi * 2.0:.2}")
`,
  },
  {
    slug: "loops",
    title: "Loops and loop expressions",
    prose: `
      <p>A <code>for</code> loop walks a range or a collection directly - no <code>.iter()</code>, no <code>as usize</code> on indices. <code>enumerate()</code> pairs each element with its index.</p>
      <p><code>while</code> covers a condition and <code>while let</code> drains an <code>Option</code>-yielding call. <code>loop</code> is an <strong>expression</strong>: <code>break value</code> is what the loop evaluates to, so a search does not need a mutable result slot. <code>if</code> and <code>match</code> are expressions too.</p>
      <p>A label breaks out of an outer level from inside an inner one, and <code>continue</code> skips the rest of one iteration.</p>`,
    code: `fn main() {
    // \`for\` walks a range or a collection - no \`.iter()\`, no \`as usize\`.
    let mut total = 0
    for n in 1..=5 { total += n }
    println("1..=5 sums to {total}")

    for (i, name) in #["ada", "grace", "alan"].iter().enumerate() {
        println("{i}: {name}")
    }

    // \`while\` for a condition, \`while let\` to drain an Option-yielding call.
    let mut countdown = 3
    while countdown > 0 {
        print("{countdown}.. ")
        countdown -= 1
    }
    println("liftoff")

    let mut stack = #[1, 2, 3]
    while let Some(top) = stack.pop() {
        print("{top} ")
    }
    println("")

    // \`loop\` is an expression: \`break value\` is what it evaluates to.
    let mut n = 1
    let doubled_past_100 = loop {
        n *= 2
        if n > 100 { break n }
    }
    println("first power of two past 100 = {doubled_past_100}")

    // \`if\` and \`match\` are expressions too - bind their result.
    let size = if doubled_past_100 > 64 { "big" } else { "small" }
    println("that is {size}")

    // A labeled loop breaks the outer level from inside the inner one.
    let mut found = (0, 0)
    'search: for row in 0..5 {
        for col in 0..5 {
            if row * col == 6 {
                found = (row, col)
                break 'search
            }
        }
    }
    println("first pair with product 6 = {:?}", found)

    // \`continue\` skips the rest of one iteration.
    let mut odds = #[]
    for n in 0..10 {
        if n % 2 == 0 { continue }
        odds.push(n)
    }
    println("odds = {:?}", odds)
}
`,
  },
  {
    slug: "match",
    title: "Pattern matching + enums",
    prose: `
      <p>Enums are sum types - a variant can carry a tuple payload, named
      fields, or nothing at all. <code>match</code> is an
      <strong>expression</strong> and must be <strong>exhaustive</strong>:
      the compiler rejects a missing case, so there is no silent
      fallthrough.</p>
      <p>Patterns bind payloads directly, with no dereference, and arms can
      use ranges, guards, and tuple shapes. Here each <code>Shape</code>
      variant maps to its own area formula.</p>`,
    code: `enum Shape {
    Circle(f64),
    Rect { w: f64, h: f64 },
    Line,
}

// \`match\` is exhaustive and yields a value - every variant is handled.
fn area(shape: Shape) -> f64 {
    match shape {
        Shape::Circle(r) => 3.14159 * *r * *r,
        Shape::Rect { w, h } => *w * *h,
        Shape::Line => 0.0,
    }
}

let shapes = [Shape::Circle(2.0), Shape::Rect { w: 3.0, h: 4.0 }, Shape::Line]
for s in shapes {
    println("area = {}", area(s))
}
`,
  },
  {
    slug: "closures",
    title: "Closures + higher-order fns",
    prose: `
      <p>A closure is <code>|param: T| body</code> and captures its
      environment automatically - there is no <code>move</code>. Two
      callable types name a function value: <code>fn(args) -> ret</code>
      for a bare code pointer, and <code>Fn(args) -> ret</code> - the
      callable trait - which also accepts capturing closures.</p>
      <p>A higher-order function takes an <code>Fn(...)</code> parameter,
      and a bare <code>fn</code> coerces into it at the call site. The
      sequence combinators - <code>filter</code>, <code>map</code>,
      <code>sum</code> - are higher-order all the way down, taking a
      closure per step.</p>`,
    code: `// \`Fn(i64) -> i64\` accepts both capturing closures and bare functions.
fn apply(f: Fn(i64) -> i64, x: i64) -> i64 { f(x) }

fn inc(y: i64) -> i64 { y + 1 }

let scale = 10
let scaled = |y: i64| scale * y       // captures \`scale\`
println("scaled(5) = {}", apply(scaled, 5))
println("inc(41)   = {}", apply(inc, 41))   // bare fn coerces

// Closures power the sequence combinators, one per step.
let total = (1..=6).filter(|n| n % 2 == 0).map(|n| n * n).sum()
println("sum of squares of evens in 1..=6 = {total}")
`,
  },
  {
    slug: "arguments",
    title: "Named arguments + defaults",
    prose: `
      <p>A parameter may declare a <strong>constant default</strong> -
      <code>greeting: String = "hello"</code> - and a call that omits it
      gets that value. Only a literal may be a default, so what a call
      means never depends on when it runs.</p>
      <p>An argument may also <strong>name</strong> the parameter it
      fills, binding with <code>:</code>:
      <code>greet("world", excited: true)</code> skips over a
      defaulted parameter in the middle without repeating it.
      Positional arguments come first; after that, names may appear in
      any order.</p>
      <p>Both are spellings at the call site. Between resolution and
      type checking every call is rewritten into the order its callee
      declares, so the bytecode VM, the JIT, and a native build all
      compile the identical positional call - names and defaults cost
      nothing at run time.</p>`,
    code: `// A parameter may declare a constant default; a call may omit it.
fn greet(name: String, greeting: String = "hello", excited: bool = false) -> String {
    let line = greeting + ", " + name
    if excited { line + "!" } else { line }
}

// Defaults work on methods and associated functions too.
struct Box { w: i64, h: i64 }

impl Box {
    fn new(w: i64, h: i64 = 1) -> Box { Box { w: w, h: h } }
    fn area(self, scale: i64 = 1) -> i64 { self.w * self.h * scale }
}

println("{}", greet("world"))
println("{}", greet("world", "hi"))

// Name an argument with \`:\` to skip over a default in between.
println("{}", greet("world", excited: true))

// Names may come in any order once positional arguments are done.
println("{}", greet(greeting: "hey", name: "Gossamer", excited: true))

let b = Box::new(3)
println("area = {}", b.area())
println("scaled = {}", b.area(scale: 10))
`,
  },
  {
    slug: "pipes",
    title: "Forward pipe |>",
    prose: `
      <p>The forward-pipe <code>|></code> turns nested calls into
      left-to-right dataflow. <code>x |> f</code> is <code>f(x)</code>; a
      step that writes its own arguments is a closure whose parameter is
      the slot the value fills, so <code>x |> |v| f(a, v)</code> reads
      <code>f(a, x)</code>.</p>
      <p>A method already chains, so <code>x.trim()</code> needs no pipe,
      and a method chain is an ordinary pipe operand. Ranges are plain
      values too, so a combinator chain like
      <code>(1..=5).filter(...).sum()</code> needs no pipe at all - pick
      whichever direction reads best.</p>`,
    code: `fn double(x: i64) -> i64 { x * 2 }
fn add(a: i64, b: i64) -> i64 { a + b }

// \`x |> f\` is \`f(x)\`; a closure step names the slot: \`f(a, x)\`.
let n = 3 |> double |> |v| add(10, v)
println("3 |> double |> |v| add(10, v) = {n}")

// A method already chains, and the chain can feed a pipe.
let shout = "  hi there  ".trim().to_uppercase()
println("shout = {shout}")

// Ranges are values and combinators are methods - chain them directly.
let total = (1..=5).filter(|n| n % 2 == 1).sum()
println("sum of odds in 1..=5 = {total}")
`,
  },
  {
    slug: "sequences",
    title: "Vec, arrays, slices, tuples",
    prose: `
      <p>The literal spellings are distinct: <code>#[..]</code> builds a growable <code>Vec&lt;T&gt;</code>, <code>[..]</code> builds a fixed <code>[T; N]</code> array. A parameter typed <code>[T]</code> is a view that takes either, and passing one copies nothing.</p>
      <p>Only <code>Vec</code> carries the resizing surface - <code>push</code>, <code>insert</code>, <code>remove</code>, <code>truncate</code>. Arrays and slices keep the fixed-size operations, so a length change is a type error rather than a surprise.</p>
      <p>A <strong>tuple</strong> groups a fixed number of values whose types may differ. Read it positionally with <code>t.0</code>, destructure it in a <code>let</code>, and compare tuples structurally.</p>`,
    code: `fn main() {
    // \`#[..]\` builds a growable Vec; \`[..]\` builds a fixed array.
    let mut queue = #[4, 8, 15]
    queue.push(16)
    let fixed = [1, 2, 3]

    println("vec   = {:?} (len {})", queue, queue.len())
    println("array = {:?} (len {})", fixed, fixed.len())

    // Indices are plain i64 - no casts. Reads and writes are bounds-checked.
    println("queue[0] = {}, last = {:?}", queue[0], queue.last())

    // A \`[T]\` parameter takes an array, a Vec, or another view.
    println("sum through a slice view = {}", total(queue))

    // Vec owns the resizing surface: insert, remove, truncate.
    let _ = queue.insert(0, 1)
    let _ = queue.remove(1)
    println("after insert/remove = {:?}", queue)

    // Tuples group values of different types; read them positionally or
    // destructure them.
    let entry = ("gossamer", 2026, true)
    let name, year, _ = entry
    println("{name} ({year}), tuple len = {}", entry.len())
    println("field access: {}", entry.0)
}

fn total(xs: [i64]) -> i64 {
    let mut acc = 0
    for x in xs { acc += x }
    acc
}
`,
  },
  {
    slug: "maps-sets",
    title: "Map, Set, and the BTree pair",
    prose: `
      <p><code>{}</code> is a <code>Map</code> literal and <code>#{}</code> is a <code>Set</code> literal. <code>m.inc(k)</code> is the counter idiom and <code>m.get_or(k, default)</code> the fallback read, so neither needs a get-then-branch.</p>
      <p>Sets carry the algebra - <code>union</code>, <code>intersection</code>, <code>difference</code>, <code>is_subset</code> - not just membership.</p>
      <p><code>BTreeMap</code> and <code>BTreeSet</code> keep their keys in sorted order, which is what you want whenever output order is part of the result.</p>`,
    code: `use std::collections::{BTreeMap, BTreeSet}

// \`{}\` is a Map literal; \`#{}\` is a Set literal.
let mut stock = {"apples": 12, "pears": 3}
stock.insert("figs", 7)

// \`inc\` is the counter idiom, \`get_or\` the fallback read.
stock.inc("apples", 5)
println("apples = {}", stock.get_or("apples", 0))
println("kiwis  = {}", stock.get_or("kiwis", 0))
println("has figs = {}", stock.contains_key("figs"))

for (name, count) in stock.iter() {
    if count > 6 { println("plenty of {name}: {count}") }
}

// Sets carry the algebra, not just membership.
let planted = #{"apples", "pears", "plums"}
let sold = #{"pears", "figs"}
println("both      = {:?}", planted.intersection(sold).to_vec())
println("unsold    = {:?}", planted.difference(sold).to_vec())
println("every one = {:?}", planted.union(sold).to_vec())

// The BTree pair keeps keys in sorted order.
let mut ordered: BTreeMap<String, i64> = BTreeMap::new()
ordered.insert("zebra", 1)
ordered.insert("ant", 2)
println("sorted keys = {:?}", ordered.keys())

let tags: BTreeSet<String> = #{"gamma", "alpha", "beta"}
println("sorted tags = {:?}", tags.to_vec())
`,
  },
  {
    slug: "queues-heaps",
    title: "Deque, Queue, Stack, heaps",
    prose: `
      <p>Each container has exactly one name and one contract. <code>Queue</code> is FIFO-only and <code>Stack</code> is LIFO-only even though a <code>Vec</code> could do either - the narrower type states the intent in the signature.</p>
      <p><code>Deque</code> pushes and pops at both ends. <code>MaxHeap</code> and <code>MinHeap</code> give priority order directly, so a min-heap never means negating your keys.</p>
      <p>None of these have a literal: build them with <code>T::new()</code> or <code>T::from([..])</code>.</p>`,
    code: `use std::collections::{Deque, Queue, Stack, MinHeap, MaxHeap}

let mut q = Queue::from([1, 2, 3])
q.push(4)
println("queue front = {:?}, len = {}", q.pop(), q.len())

let mut st = Stack::from([1, 2, 3])
st.push(4)
println("stack top = {:?}", st.pop())

let mut dq = Deque::from([2, 3])
dq.push_front(1)
dq.push_back(4)
println("deque ends = {:?} {:?}", dq.pop_front(), dq.pop_back())

let mut hi = MaxHeap::from([3, 9, 4])
hi.push(11)
println("max = {:?}", hi.pop())

let mut lo = MinHeap::from([3, 9, 4])
println("min = {:?}", lo.pop())
`,
  },
  {
    slug: "functional",
    title: "map, filter, fold and friends",
    prose: `
      <p>The combinators are methods on any Vec, array, or range - <code>map</code>, <code>filter</code>, <code>fold</code>, <code>any</code>, <code>all</code>, <code>find</code>, <code>position</code>, <code>min_by_key</code>, <code>take</code>, <code>step_by</code>, <code>rev</code>.</p>
      <p>They chain left to right, so a query reads in the order it happens and never needs a manual accumulator loop. Terminals like <code>sum</code>, <code>count</code>, <code>collect</code>, and <code>join</code> end the chain.</p>
      <p>Ranges carry the same surface as collections, so <code>(1..=6).filter(..).map(..).collect()</code> is one pipeline.</p>`,
    code: `fn main() {
    let readings = #[12, 7, 30, 4, 18, 25]

    // Transform, select, and reduce - each combinator is a method.
    println("doubled  = {:?}", readings.map(|n| n * 2))
    println("over ten = {:?}", readings.filter(|n| n > 10))
    println("sum      = {}", readings.sum())
    println("count    = {}", readings.count(|n| n % 2 == 0))
    println("fold     = {}", readings.fold(0, |acc, n| acc + n * n))

    // Questions about a sequence answer directly.
    println("any over 25 = {}", readings.any(|n| n > 25))
    println("all positive = {}", readings.all(|n| n > 0))
    println("first over 15 = {:?}", readings.find(|n| n > 15))
    println("its index = {:?}", readings.position(|n| n > 15))
    println("largest = {:?}", readings.max())
    println("closest to 20 = {:?}", readings.min_by_key(|n| if n > 20 { n - 20 } else { 20 - n }))

    // Ranges carry the same surface, and the pipeline reads left to right.
    let squares = (1..=6).filter(|n| n % 2 == 1).map(|n| n * n).collect()
    println("odd squares = {:?}", squares)
    println("reversed    = {:?}", (1..=5).rev().collect())
    println("every other = {:?}", (0..10).step_by(3).collect())
    println("first three = {:?}", readings.iter().take(3).collect())

    // Sorting takes the key you care about; \`join\` renders the result.
    let mut names = #["Ada", "Grace", "Alan", "Barbara"]
    names.sort_by_key(|n| n.len())
    println("by length = {}", names.join(", "))
}
`,
  },
  {
    slug: "sorting",
    title: "Sorting and searching",
    prose: `
      <p><code>sort</code> orders in place and <code>sort_by_key</code> takes the key you actually care about, so ordering by a derived value needs no comparator boilerplate.</p>
      <p>Structs, tuples, and enums compare structurally by declaration order with no derive, so a sequence of them sorts as it reads.</p>
      <p>Searching splits the same way: <code>find</code> and <code>position</code> scan linearly, while a sorted sequence supports a binary search you can write in a handful of lines.</p>`,
    code: `struct Player { name: String, score: i64 }

let mut scores = #[42, 7, 19, 7, 88, 3]

// \`sort\` orders in place; \`sort_by_key\` takes the key you care about.
scores.sort()
println("sorted     = {:?}", scores)
println("descending = {:?}", scores.rev())
println("no repeats = {:?}", scores.dedup())

// Structs and tuples compare structurally, field by field, with no
// derive - so a sequence of them sorts as it reads.
let mut board = #[
    Player { name: "ada", score: 19 },
    Player { name: "grace", score: 88 },
    Player { name: "alan", score: 42 },
]
board.sort_by_key(|p| 0 - p.score)
for p in board {
    println("  {:>6} {:>3}", p.name, p.score)
}

// Binary search over the sorted sequence: halve the window each step.
println("19 lives at index {:?}", index_of_sorted(scores, 19))
println("20 is absent: {:?}", index_of_sorted(scores, 20))

// The linear searches read the same either way.
println("first over 40 = {:?}", scores.find(|n| n > 40))
println("its position  = {:?}", scores.position(|n| n > 40))

fn index_of_sorted(xs: [i64], needle: i64) -> Option<i64> {
    let mut lo = 0
    let mut hi = xs.len() - 1
    while lo <= hi {
        let mid = (lo + hi) / 2
        if xs[mid] == needle { return Some(mid) }
        if xs[mid] < needle { lo = mid + 1 } else { hi = mid - 1 }
    }
    None
}
`,
  },
  {
    slug: "strings",
    title: "Strings + format specs",
    prose: `
      <p>Strings are values, not references you juggle. Concatenate with
      <code>+</code>, append with <code>+=</code>, and reach a string's own
      methods by chaining - <code>title.trim().to_title()</code>.</p>
      <p>Formatted output follows Rust's <code>{:spec}</code> grammar: width
      and alignment (<code>{:>8}</code>, <code>{:^7}</code>), zero-padding and
      radix (<code>{:08x}</code>), and precision (<code>{:.2}</code>), for
      both positional and named arguments.</p>`,
    code: `fn main() {
    let title = "  the gossamer tour  "

    // Methods chain directly; strings are plain values.
    let clean = title.trim().to_title()
    println("[{clean}]")

    // \`+=\` appends; \`+\` concatenates with no separator.
    let mut line = "items:"
    for part in ["alpha", "beta", "gamma"] {
        line += " " + part
    }
    println("{line}")

    // Format specs follow Rust's {:spec} grammar.
    println("[{:>8}]", 42)
    println("[{:08x}]", 255)
    println("[{:^7}]", "hi")
    println("[{:.2}]", 3.14159)
}
`,
  },
  {
    slug: "types",
    title: "Structs / traits / generics / derive",
    prose: `
      <p>Structs are runtime-managed value types; traits define a shared
      interface that each type implements. They compare by value and
      <code>.clone()</code> with no derive, so <code>==</code> and
      <code>.clone()</code> just work on every tier;
      <code>#[derive(Debug)]</code> adds the <code>{:?}</code>
      representation (<code>Default</code>, <code>PartialOrd</code>, and
      <code>Ord</code> are derivable too).</p>
      <p>Generic functions take trait bounds -
      <code>fn farther&lt;T: Distance&gt;(...)</code> - and each call site
      monomorphises to a direct call, with no dynamic dispatch.</p>`,
    code: `// Structs compare by value and \`.clone()\` with no derive; derive Debug for {:?}.
#[derive(Debug)]
struct Point { x: i64, y: i64 }

trait Distance {
    fn from_origin(self) -> i64
}

impl Distance for Point {
    fn from_origin(self) -> i64 { self.x * self.x + self.y * self.y }
}

// Generic over any \`Distance\`, dispatched statically and monomorphised.
fn farther<T: Distance>(a: T, b: T) -> bool {
    a.from_origin() > b.from_origin()
}

let a = Point { x: 3, y: 4 }
let b = Point { x: 1, y: 2 }
println("a = {:?}", a)
println("a == a.clone(): {}", a == a.clone())
println("a farther than b: {}", farther(a, b))
`,
  },
  {
    slug: "operators",
    title: "Operator overloading",
    prose: `
      <p>Arithmetic operators are trait methods, so a type opts into them
      by implementing the trait: <code>impl Add for Vec2</code> gives
      <code>Vec2 + Vec2</code> its meaning, and the same shape covers
      <code>Sub</code>, <code>Mul</code>, and the rest.</p>
      <p>These are written by hand, not derived. The compiler already
      synthesizes <code>==</code>, ordering, and <code>.clone()</code>
      for you - a custom <code>+</code> is the part that is genuinely
      yours to define.</p>`,
    code: `// Operator overloading: \`impl Add for T\` makes \`+\` work on your type.
struct Vec2 { x: f64, y: f64 }

impl Add for Vec2 {
    fn add(self, other: Vec2) -> Vec2 {
        Vec2 { x: self.x + other.x, y: self.y + other.y }
    }
}

let a = Vec2 { x: 1.0, y: 2.0 }
let b = Vec2 { x: 3.0, y: 4.0 }
let c = a + b
println("sum = ({}, {})", c.x, c.y)
`,
  },
  {
    slug: "recursive-enum",
    title: "Recursive enums",
    prose: `
      <p>An enum variant can refer to the enum itself, so a tree or an
      expression type needs nothing special: a variant payload can be the
      enum, and every node is heap-shared automatically. A
      <code>match</code> reaches straight through with no dereference.</p>
      <p>There are no pointer wrappers to reach for: every value is
      heap-shared and reference-counted already, so a variant payload
      names the enum directly. Here a tiny arithmetic evaluator recurses
      with one exhaustive <code>match</code> - the whole shape of a real
      tree-walking interpreter.</p>`,
    code: `// A recursive enum needs no wrapping: a variant payload can be the enum
// itself, and every node is heap-shared and reference-counted.
enum Expr {
    Num(i64),
    Add(Expr, Expr),
    Mul(Expr, Expr),
}

fn eval(e: Expr) -> i64 {
    match e {
        Expr::Num(n) => *n,
        Expr::Add(a, b) => eval(a) + eval(b),
        Expr::Mul(a, b) => eval(a) * eval(b),
    }
}

// (2 + 3) * 4
let tree = Expr::Mul(Expr::Add(Expr::Num(2), Expr::Num(3)), Expr::Num(4))
println("(2 + 3) * 4 = {}", eval(tree))
`,
  },
  {
    slug: "options",
    title: "Option + the ? operator",
    prose: `
      <p><code>Option&lt;T&gt;</code> is <code>Some(v)</code> or
      <code>None</code> - the type-level "maybe absent", with no null in
      sight. Read it with <code>if let Some(v) = ...</code>, and inside
      an <code>Option</code>-returning function <code>?</code>
      short-circuits the moment a value is <code>None</code>.</p>
      <p>It is the same <code>?</code> that propagates <code>Err</code>
      in a <code>Result</code> function: the present path stays flat
      while the absent path needs no nesting at all.</p>`,
    code: `// \`?\` propagates \`None\` inside an Option-returning function.
fn first_even(xs: [i64]) -> Option<i64> {
    for x in xs {
        if *x % 2 == 0 { return Some(*x) }
    }
    None
}

// \`?\` chains Option-returning calls: if \`first_even\` yields \`None\`,
// this returns \`None\` immediately - no nesting on the absent path.
fn half_of_first_even(xs: [i64]) -> Option<i64> {
    let n = first_even(xs)?
    Some(n / 2)
}

if let Some(n) = first_even([3, 5, 8, 9]) {
    println("first even = {n}")
}

println("half = {:?}", half_of_first_even([3, 5, 8, 9]))
println("half = {:?}", half_of_first_even([1, 3, 5]))
`,
  },
  {
    slug: "errors",
    title: "Error handling",
    prose: `
      <p>Gossamer has no exceptions. Fallible functions return
      <code>Result&lt;T, E&gt;</code> and <code>?</code> propagates the
      <code>Err</code> branch upward. Build and chain typed errors with
      <code>std::errors</code>: <code>errors::new</code>,
      <code>errors::wrap</code> for higher-level context, and printing a
      wrapped error shows the colon-joined cause chain.</p>
      <p>Handle the present and error paths with a small <code>match</code>
      when the recovery action is side-effecting. Use the data-last
      <code>std::result</code> helpers when you are transforming values
      inside a pipeline.</p>`,
    code: `use std::errors

// Fallible work returns \`Result<T, E>\`; \`?\` propagates the \`Err\`.
fn parse_port(text: String) -> Result<i64, errors::Error> {
    let n: i64 = match text.to_i64() {
        Some(n) => n,
        None => return Err(errors::new(format("not a number: {text}"))),
    }
    if n <= 0 { return Err(errors::new(format("must be positive: {n}"))) }
    Ok(n)
}

// Match the Ok value; handle the Err in-line.
match parse_port("8080") {
    Ok(n) => println("port = {n}"),
    Err(e) => eprintln("error: {}", e.message()),
}

// \`wrap\` adds context; printing shows the colon-joined cause chain.
let bad = parse_port("oops").map_err(|e| errors::wrap(e, "loading config"))
if let Err(e) = bad { println("{e}") }
`,
  },
  {
    slug: "goroutines",
    title: "Goroutines + channels",
    prose: `
      <p>Concurrency is Go-shaped: <code>spawn(f)</code> starts a goroutine
      under the enclosing <code>cohort</code> - <code>main</code> is an
      implicit one - and <code>channel()</code> returns a typed
      <code>(Sender, Receiver)</code> pair. The producer sends every value and <code>close()</code>s the
      channel; the consumer drains it with the canonical
      <code>while let Some(v) = rx.recv()</code>.</p>
      <p>Here the producer runs to completion before the drain finishes,
      which is the subset the in-browser runtime supports.
      <strong>Full goroutine interleaving</strong> - goroutines that block
      and hand off mid-run - runs natively with <code>gos</code>.</p>`,
    code: `use std::sync

// The producer sends every value, then closes - it runs to
// completion, so no mid-run hand-off is needed.
fn produce(tx: sync::Sender<i64>) {
    for n in 1..=5 { tx.send(n * n) }
    tx.close()
}

let tx, rx = sync::channel(5)
spawn(|| produce(tx))

// \`recv\` yields \`Some\` until the channel is closed and drained.
let mut total = 0
while let Some(v) = rx.recv() { total += v }
println("sum of squares 1..=5 = {total}")
`,
  },
  {
    slug: "select",
    title: "select over channels",
    prose: `
      <p><code>select</code> multiplexes several channel operations at
      once: each arm is a receive or a send, the arms are polled in
      source order, and the goroutine takes the first one that is ready.
      An optional <code>default</code> arm fires when none is.</p>
      <p>Here two producers feed two channels and <code>select</code>
      merges their values as they arrive - the standard fan-in pattern.
      Full mid-run hand-off between goroutines that block and resume runs
      natively with <code>gos</code>.</p>`,
    code: `use std::sync

fn produce(tx: sync::Sender<i64>, xs: Vec<i64>) {
    for x in xs { tx.send(x) }
}

let tx_hi, rx_hi = sync::channel(3)
let tx_lo, rx_lo = sync::channel(2)

spawn(|| produce(tx_hi, #[1, 2, 3]))
spawn(|| produce(tx_lo, #[10, 20]))

// Five values arrive across two channels; \`select\` takes whichever
// arm is ready, polling them in source order.
let mut total = 0
for _ in 0..5 {
    select {
        v = rx_hi.recv() => total += v,
        v = rx_lo.recv() => total += v,
    }
}
println("merged total = {total}")
`,
  },
  {
    slug: "shared-state",
    title: "Shared state + WaitGroup",
    prose: `
      <p>When sharing memory is simpler than passing messages, reach for
      the <code>std::sync</code> primitives. A <code>WaitGroup</code>
      tracks outstanding work: <code>add</code> before spawning,
      <code>done</code> as each worker finishes, and <code>wait</code>
      blocks until the count reaches zero - no sleeps.</p>
      <p>The shared counter here is an <code>AtomicI64</code>, so
      concurrent <code>fetch_add</code>s compose without an explicit
      lock. Sync handles are shared by value: a copy refers to the same
      underlying state.</p>`,
    code: `use std::sync

// Each worker runs to completion: it folds its share into the shared
// atomic counter, then signals the WaitGroup. Sync handles are shared
// by value - a copy refers to the same underlying state.
fn worker(total: sync::AtomicI64, wg: sync::WaitGroup, i: i64) {
    sync::AtomicI64::fetch_add(total, i * i)
    wg.done()
}

let total = sync::AtomicI64::new(0)
let wg = sync::WaitGroup::new()

for i in 1..=4 {
    wg.add(1)
    spawn(|| worker(total, wg, i))
}
wg.wait()      // block until every worker has called \`done\`

println("sum of squares 1..=4 = {}", sync::AtomicI64::load(total))
`,
  },
  {
    slug: "defer",
    title: "defer for cleanup",
    prose: `
      <p><code>defer expr</code> schedules work to run when control
      leaves the enclosing block by <strong>any</strong> path -
      fall-through, <code>return</code>, <code>break</code>, or
      <code>continue</code> - in <strong>LIFO</strong> order. It keeps a
      resource's release next to its acquisition instead of scattered
      across every exit.</p>
      <p>In a loop body it runs once per iteration. It is the same
      pattern Go uses: open, <code>defer</code> the close, then use - and
      the close is guaranteed to happen.</p>`,
    code: `fn use_resource(name: String) {
    println("  open {name}")
    defer println("  close {name}")     // runs on every exit path
    println("  use {name}")
}

// \`defer\` runs when control leaves the block, in LIFO order.
{
    defer println("third")
    defer println("second")
    defer println("first")
    println("body runs")
}
println("---")
use_resource("file")
`,
  },
  {
    slug: "arena",
    title: "arena { } regions",
    prose: `
      <p>Memory is automatic: deterministic reference counting, no borrow
      checker, no GC pauses. For an object graph that all dies together,
      an <code>arena { }</code> block bump-allocates everything inside it
      and frees the whole region at every exit path.</p>
      <p>The contract is simple: nothing allocated inside may escape, so
      you compute a scalar or string summary inside and keep that. Here
      each round fills a throwaway buffer and only the running total
      survives the block.</p>`,
    code: `fn main() {
    // Everything allocated inside an \`arena { }\` is bump-allocated and
    // freed wholesale at every exit. The contract: nothing allocated
    // inside may escape, so compute a scalar summary and keep that.
    let mut grand_total = 0
    for round in 1..=3 {
        arena {
            let mut scratch: Vec<i64> = #[]
            for i in 0..1000 { scratch.push(i * round) }

            let mut sum = 0
            for x in scratch { sum += x }
            grand_total += sum      // a scalar survives; the Vec does not
        }
    }
    println("grand total = {grand_total}")
}
`,
  },
  {
    slug: "comptime",
    title: "Compile-time evaluation",
    prose: `
      <p>A <code>comptime fn</code> runs during compilation. Inside a
      <code>comptime { }</code> block its result is folded to a literal
      before the program runs, so the work happens once, at build time,
      and every tier embeds the same constant.</p>
      <p>Use it for lookup tables and derived constants - anything better
      computed at build time than on every run.
      <code>const FACT_10: i64 = comptime { factorial(10) }</code> ships
      the answer, not the loop that produced it.</p>`,
    code: `// A \`comptime fn\` runs at compile time; calling it inside a
// \`comptime { }\` block folds the result into a literal, so the runtime
// never repeats the work - every tier compiles the same constant.
comptime fn factorial(n: i64) -> i64 {
    let mut acc = 1
    for i in 2..=n { acc *= i }
    acc
}

const FACT_10: i64 = comptime { factorial(10) }

println("10! folded at compile time = {FACT_10}")

// A \`comptime\` block can fold an inline computation to a literal too.
let triangular = comptime {
    let mut acc = 0
    for i in 1..=100 { acc += i }
    acc
}
println("sum 1..=100 = {triangular}")
`,
  },
  {
    slug: "time",
    title: "Dates and times",
    prose: `
      <p>An instant is milliseconds since the epoch: <code>time::parse_rfc3339</code> reads one, <code>time::format_rfc3339</code> writes one back, and both return a <code>Result</code> so bad input is never a silent zero.</p>
      <p>A <code>Duration</code> is a value you build, add, and read back through <code>as_secs</code> / <code>as_millis</code>. The span between two instants is a duration over their difference.</p>
      <p>Because instants are integers, comparing and sorting them is ordinary arithmetic.</p>`,
    code: `use std::time

// RFC 3339 text in, milliseconds since the epoch out.
let launch = time::parse_rfc3339("2026-08-06T09:30:00Z").unwrap_or(0)
let landing = time::parse_rfc3339("2026-08-07T11:00:00Z").unwrap_or(0)
println("launch ms = {launch}")

// Durations are values: build them, add them, read them back.
let day = time::Duration::from_secs(86400)
let hold = time::Duration::from_secs(5400)
let expected = launch + day.as_millis() + hold.as_millis()
println("expected  = {}", time::format_rfc3339(expected).unwrap_or("?"))

// A span between two instants is a duration over the difference.
let flight = time::Duration::from_millis(landing - launch)
println("flight    = {} h {} m", flight.as_secs() / 3600, (flight.as_secs() % 3600) / 60)

// Comparison and ordering are plain integer work.
println("on time   = {}", landing <= expected)

// A schedule is just a sequence of instants - sort and render it.
let mut stops = #[
    time::parse_rfc3339("2026-08-06T18:00:00Z").unwrap_or(0),
    launch,
    landing,
]
stops.sort()
for at in stops {
    println("  {}", time::format_rfc3339(at).unwrap_or("?"))
}

// Bad input is a \`Result\`, never a silent zero.
match time::parse_rfc3339("not a timestamp") {
    Ok(ms) => println("parsed {ms}"),
    Err(e) => println("rejected: {e}"),
}
`,
  },
  {
    slug: "json",
    title: "JSON round-trip",
    prose: `
      <p>Every user <code>struct</code> automatically gains two generic
      free functions: <code>to_json::&lt;T&gt;(&amp;value)</code> encodes
      it to text and <code>from_json::&lt;T&gt;(&amp;text)</code> decodes
      text back into a typed value, validating each field against its
      declared type with path-qualified errors.</p>
      <p>There is one spelling - the turbofish form - and it works on
      every tier. Here a <code>Server</code> config makes the full round
      trip; for unknown-shape documents the dynamic
      <code>json::parse</code> API stays available.</p>`,
    code: `// Every user struct gets \`to_json::<T>\` / \`from_json::<T>\` for free.
// \`?\` makes this entry file's implicit \`main\` return a Result.
#[derive(Debug)]
struct Server { host: String, port: i64, tls: bool }

let cfg = Server { host: "localhost", port: 8080, tls: true }

// Encode the struct to JSON text...
let text = to_json::<Server>(cfg)?
println("encoded = {text}")

// ...then decode it straight back into a typed struct, validating
// each field against its declared type.
let back: Server = from_json::<Server>(text)?
println("decoded = {:?}", back)
println("address = {}:{}", back.host, back.port)
`,
  },
  {
    slug: "encoding",
    title: "Encoding bytes and records",
    prose: `
      <p><code>base64</code> and <code>hex</code> round-trip bytes through text, which is what most transport and storage formats need.</p>
      <p>Strings expose the record-splitting surface - <code>lines</code>, <code>split_once</code>, <code>split</code> - so a delimited file is a loop rather than a parser.</p>
      <p>Every decode returns a <code>Result</code>, so malformed input is handled at the point it arrives.</p>`,
    code: `use std::encoding::{base64, hex}
use std::errors

// Base64 and hex round-trip bytes through text.
let secret = "gossamer".as_bytes()
let encoded = base64::encode(secret)
println("base64 = {encoded}")
let raw = base64::decode(encoded)?
println("back   = {} bytes, first = {}", raw.len(), raw[0] as char)
println("hex    = {}", hex::encode(secret))

// A delimited record splits with the string surface.
let sheet = "name,role\\nada,analyst\\ngrace,admiral\\n"
for line in sheet.lines() {
    if let Some((name, role)) = line.split_once(",") {
        println("  {name} | {role}")
    }
}

// Hex decodes back to the same bytes it encoded.
let round = hex::decode(hex::encode(secret))?
println("hex round-trips = {}", round.len() == secret.len())
`,
  },
  {
    slug: "regex",
    title: "Regular expressions",
    prose: `
      <p><code>std::regex</code> wraps Rust's <code>regex</code> crate.
      <code>regex::compile</code> checks a literal pattern while the program
      compiles and answers the <code>Pattern</code> itself;
      <code>regex::new</code> is the fallible spelling for a pattern built
      at run time. Compile once, then reuse the pattern across
      <code>is_match</code>, <code>find</code> / <code>find_all</code>,
      <code>captures</code>, <code>replace_all</code>, and
      <code>split</code>.</p>
      <p><code>captures</code> returns <code>[full, group1, ...]</code>,
      each an <code>Option&lt;String&gt;</code>. A
      <strong>let-chain</strong> binds every group and tests them in one
      condition, so a structured parse collapses to a single
      <code>if</code>.</p>`,
    code: `use std::regex

// A literal pattern is checked while compiling, so this is the Pattern.
let re = regex::compile("([0-9]{4}-[0-9]{2}-[0-9]{2}) ([A-Z]+) (.+)")

let lines = ["2026-06-29 ERROR disk full", "2026-06-30 INFO restarted"]
for line in lines {
    // \`captures\` yields [full, group1, group2, ...]; a let-chain
    // binds all three groups and tests them in one condition.
    if let Some(c) = regex::captures(re, line)
        && let Some(date) = c[1]
        && let Some(level) = c[2]
        && let Some(msg) = c[3] {
        println(f"{date}  [{level}]  {msg}")
    }
}
`,
  },
  {
    slug: "http-server",
    title: "HTTP serving and routing",
    prose: `
      <p>A handler takes an <code>http::Request</code> and returns a <code>Result&lt;http::Response, errors::Error&gt;</code> - that is the entire contract. Routes chain with <code>|&gt;</code>, one verb method per route, and <code>{id}</code> path parameters come back through <code>path_int</code> / <code>path_value</code>.</p>
      <p>On a host the program ends with <code>http::serve("127.0.0.1:8080", routes)?</code>. This page runs in a browser sandbox with no sockets, so it builds the router and then constructs the same responses the handlers return.</p>`,
    code: `use std::errors
use std::http
use std::http::router

struct Note { id: i64, title: String }

// A handler takes a request and returns a response - nothing else. The
// router hands it the matched request; \`{id}\` arrives through \`path_int\`.
fn get_note(r: http::Request) -> Result<http::Response, errors::Error> {
    let id = r.path_int("id").unwrap_or(0)
    Ok(http::Response::json(200, note_json(id)?))
}

fn health(_r: http::Request) -> Result<http::Response, errors::Error> {
    Ok(http::Response::text(200, "ok"))
}

fn note_json(id: i64) -> Result<String, errors::Error> {
    to_json::<Note>(Note { id: id, title: "written in Gossamer" })
}

// Routes chain by method, one verb per route.
let routes = router::Router::new()
    .get("/health", health)
    .get("/notes/{id}", get_note)
    .post("/notes", get_note)
println("router ready")

// On a host this is the whole program:
//     http::serve("127.0.0.1:8080", routes)?
// The browser sandbox has no sockets, so the responses below are built
// exactly as the handlers build them.
let ok = http::Response::text(200, "ok")
println("GET /health   -> {} {}", ok.status, ok.body)

let note = http::Response::json(200, note_json(42)?)
println("GET /notes/42 -> {} {}", note.status, note.body)

let created = http::Response::json(201, note_json(43)?)
    .with_header("location", "/notes/43")
println("POST /notes   -> {} ({} header)", created.status, created.headers.len())

let missing = http::Response::text(404, "no such note")
println("miss          -> {} {}", missing.status, missing.body)
`,
  },
  {
    slug: "http-client",
    title: "HTTP clients and streaming",
    prose: `
      <p>A response body is text, so decoding it is the same work whether it arrived over the wire or came from a file: <code>from_json::&lt;T&gt;</code> for a known shape, <code>json::parse</code> for a partly-known one.</p>
      <p>A streaming endpoint hands you the body in pieces rather than as one document, so each chunk is handled as it lands - the loop below reads a server-sent-event feed line by line.</p>
      <p>On a host, <code>http::get(url)?.body</code> supplies the text. The browser sandbox has no sockets, so the payload here is captured inline.</p>`,
    code: `use std::errors
use std::encoding::json
use std::strings

struct Repo { name: String, stars: i64 }

// On a host, one call fetches a body:
//     let body = http::get("https://api.example.com/repos/gossamer")?.body
// The browser sandbox has no sockets, so this page decodes a captured
// payload the same way the response body would be decoded.
let body = "{\\"name\\":\\"gossamer\\",\\"stars\\":420}"

// Typed decode: fields land in a struct, unknown keys are ignored.
let repo = from_json::<Repo>(body)?
println("{} has {} stars", repo.name, repo.stars)

// Dynamic decode for a shape you only partly know: query the document
// itself rather than describing it with a struct.
let doc = json::parse(body)?
println("keys       = {:?}", doc.keys())
if let Some(name) = doc.get("name") {
    println("name field = {:?}", name.as_str())
}
println("re-encoded = {}", to_json::<Repo>(repo)?)

// Streaming: a server-sent-event feed arrives as lines, not as one
// document, so each chunk is handled as it lands.
let feed = "event: tick\\ndata: 1\\n\\nevent: tick\\ndata: 2\\n\\nevent: done\\ndata: bye\\n"
let mut ticks = 0
for line in feed.lines() {
    if let Some(payload) = line.strip_prefix("data: ") {
        if payload == "bye" {
            println("stream closed")
        } else {
            ticks += 1
            println("chunk {ticks}: {payload}")
        }
    }
}

// A failed request is a \`Result\`, so the error path is written once.
let bad = from_json::<Repo>("{\\"name\\":42}")
match bad {
    Ok(r) => println("decoded {}", r.name),
    Err(e) => println("decode failed: {e}"),
}
`,
  },
  {
    slug: "testing",
    title: "Tests and assertions",
    prose: `
      <p><code>assert</code> and <code>assert_eq</code> are prelude builtins - no import and no harness needed to state what must hold.</p>
      <p>Unit tests live beside the code they cover in a <code>#[cfg(test)] mod</code> with a name unique to the file, reaching items through <code>super::</code>. <code>gos test</code> runs them, along with any fenced code in a doc comment, so documented examples cannot rot.</p>`,
    code: `fn median(xs: Vec<i64>) -> i64 {
    let mut sorted = xs
    sorted.sort()
    let mid = sorted.len() / 2
    if sorted.len() % 2 == 1 { sorted[mid] } else { (sorted[mid - 1] + sorted[mid]) / 2 }
}

// \`assert\` and \`assert_eq\` are prelude builtins - no import, no harness.
assert(median(#[3, 1, 2]) == 2, "odd count takes the middle")
assert_eq(median(#[4, 1, 3, 2]), 2, "even count averages the pair")
println("median checks passed")

// A failing assertion panics with the message you wrote.
let empty: Vec<i64> = Vec::from([])
assert(empty.is_empty(), "an empty vec has no elements")

// \`#[test]\` functions run under \`gos test\`, and the fenced code in a
// doc comment above an item runs there too, so examples cannot rot.
println("2 of 2 checks passed")

#[cfg(test)]
mod tour_testing_tests {
    use std::testing

    #[test]
    fn median_of_odd_count() {
        let _ = testing::check_eq(super::median(#[5, 1, 3]), 3, "middle value")
    }

    #[test]
    fn median_of_even_count() {
        let _ = testing::check_eq(super::median(#[1, 2, 3, 4]), 2, "averaged pair")
    }
}
`,
  },
  {
    slug: "together",
    title: "A small program",
    prose: `
      <p>One small program, every idea at once: immutable bindings, a
      <code>HashMap</code> counter, iteration with tuple destructuring, a
      <code>#[derive]</code>d struct, a descending <code>sort_by_key</code>
      with <code>Reverse</code>, and an aligned format spec.</p>
      <p>It counts word frequencies, moves the entries into <code>Tally</code>
      structs, sorts by count, and prints a tidy table. Edit the input text
      and run it again - that is the whole language in twenty lines. Go build
      something.</p>`,
    code: `#[derive(Debug)]
struct Tally { word: String, count: i64 }

let text = "go rust go gossamer rust go"

// Count each word; \`inc\` does get-or-zero then add.
let mut counts = {}
for word in text.split(" ") {
    counts.inc(word)
}

// Move the entries into structs and sort by count, descending.
let mut rows = #[]
for (word, count) in counts.iter() {
    rows.push(Tally { word: word, count: count })
}
rows.sort_by_key(|r| Reverse(r.count))

for r in rows {
    println("{:>9} x {}", r.word, r.count)
}
`,
  },
];

// ---- lazy playground loader ------------------------------------------
let playgroundPromise = null;

/// Import the playground module once and resolve its mount function.
function loadMountPlayground() {
  if (!playgroundPromise) {
    playgroundPromise = import(PLAYGROUND_URL).then((m) => m.mountPlayground);
  }
  return playgroundPromise;
}

// ---- small DOM helper ------------------------------------------------
/// Create an element with an optional class and appended children.
function el(tag, className, ...children) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  for (const child of children) {
    if (child != null) node.append(child);
  }
  return node;
}

// ---- controller state ------------------------------------------------
let activeIndex = -1;
let playground = null;
let firstRender = true;
let refs = {};

/// Index of the lesson whose slug matches the current hash, or 0.
function indexFromHash() {
  const slug = location.hash.replace(/^#/, "");
  const i = LESSONS.findIndex((l) => l.slug === slug);
  return i >= 0 ? i : 0;
}

/// Move to a lesson by index; the hash is the single source of truth.
function navigateTo(index) {
  const clamped = Math.max(0, Math.min(LESSONS.length - 1, index));
  const slug = LESSONS[clamped].slug;
  if (location.hash === "#" + slug) {
    syncFromHash();
  } else {
    location.hash = slug;
  }
}

/// Render whatever lesson the hash names, unless it is already shown.
function syncFromHash() {
  const i = indexFromHash();
  if (i === activeIndex) return;
  showLesson(i);
}

/// A spinner shown in the playground host while the module loads.
function loadingNode() {
  return el(
    "div",
    "tour-play-loading",
    el("span", "spin"),
    document.createTextNode("Loading the playground..."),
  );
}

/// A static code listing shown when the playground cannot mount.
function fallbackNode(code) {
  const pre = el("pre");
  pre.textContent = code;
  const note = el("p", "note");
  note.innerHTML =
    "The interactive editor could not load. Copy this program and run it with <code>gos</code>.";
  return el("div", "tour-play-fallback", pre, note);
}

/// Tear down the previous lesson, render the new one, and (re)mount the
/// playground. Guards against a stale async mount when the user advances
/// before the module resolves.
async function showLesson(index) {
  if (playground) {
    playground.destroy();
    playground = null;
  }
  activeIndex = index;
  const lesson = LESSONS[index];

  refs.stepButtons.forEach((btn, i) => {
    if (i === index) btn.setAttribute("aria-current", "step");
    else btn.removeAttribute("aria-current");
  });

  refs.prose.innerHTML = `<h1>${lesson.title}</h1>${lesson.prose}`;

  const human = index + 1;
  const total = LESSONS.length;
  refs.progressText.textContent = `${human} / ${total}`;
  refs.progressFill.style.width = `${(human / total) * 100}%`;
  refs.centerText.textContent = `${human} / ${total}`;
  refs.prevBtn.disabled = index === 0;
  refs.nextBtn.disabled = index === total - 1;
  document.title = `${lesson.title} - A Tour of Gossamer`;

  // Move focus (and the viewport) to the new heading on user navigation,
  // but never steal focus on the initial page load.
  if (!firstRender) {
    const heading = refs.prose.querySelector("h1");
    if (heading) {
      heading.tabIndex = -1;
      heading.focus();
    }
  }
  firstRender = false;

  const host = refs.play;
  host.replaceChildren(loadingNode());
  try {
    const mount = await loadMountPlayground();
    if (activeIndex !== index) return;
    playground = mount(host, { source: lesson.code, height: PLAY_HEIGHT });
  } catch (err) {
    if (activeIndex !== index) return;
    host.replaceChildren(fallbackNode(lesson.code));
  }
}

/// Roving arrow-key navigation within the step list.
function onStepsKeydown(e) {
  const btns = refs.stepButtons;
  const idx = btns.indexOf(document.activeElement);
  if (idx < 0) return;
  let next = -1;
  switch (e.key) {
    case "ArrowDown":
    case "ArrowRight":
      next = Math.min(btns.length - 1, idx + 1);
      break;
    case "ArrowUp":
    case "ArrowLeft":
      next = Math.max(0, idx - 1);
      break;
    case "Home":
      next = 0;
      break;
    case "End":
      next = btns.length - 1;
      break;
    default:
      return;
  }
  e.preventDefault();
  btns[next].focus();
}

/// Build the sidebar, content shell, and controls once, caching refs.
function buildShell(root) {
  root.replaceChildren();

  const sidebar = el("aside", "tour-sidebar");
  const heading = el("h2");
  heading.id = "tour-steps-label";
  heading.textContent = "Lessons";
  const steps = el("ul", "tour-steps");
  steps.setAttribute("aria-labelledby", "tour-steps-label");
  const stepButtons = [];
  LESSONS.forEach((lesson, i) => {
    const btn = el("button", "step-btn");
    btn.type = "button";
    btn.textContent = lesson.title;
    btn.addEventListener("click", () => navigateTo(i));
    steps.append(el("li", null, btn));
    stepButtons.push(btn);
  });
  steps.addEventListener("keydown", onStepsKeydown);
  sidebar.append(heading, steps);

  const content = el("section", "tour-content");

  const progressText = el("span");
  const progressFill = el("i");
  const bar = el("div", "bar", progressFill);
  const progress = el("div", "tour-progress", progressText, bar);

  const prose = el("article", "tour-prose");
  const play = el("div", "tour-play");
  const split = el("div", "tour-split", prose, play);

  const prevBtn = el("button", "nav-btn");
  prevBtn.type = "button";
  prevBtn.innerHTML = '<span aria-hidden="true">&larr;</span> Prev';
  prevBtn.addEventListener("click", () => navigateTo(activeIndex - 1));

  const centerText = el("span", "center");

  const nextBtn = el("button", "nav-btn primary");
  nextBtn.type = "button";
  nextBtn.innerHTML = 'Next <span aria-hidden="true">&rarr;</span>';
  nextBtn.addEventListener("click", () => navigateTo(activeIndex + 1));

  const controls = el("div", "tour-controls", prevBtn, centerText, nextBtn);

  content.append(progress, split, controls);
  root.append(sidebar, content);

  refs = {
    stepButtons,
    prose,
    play,
    progressText,
    progressFill,
    centerText,
    prevBtn,
    nextBtn,
  };
}

/// Last-resort message if the page cannot be assembled at all.
function renderFatal(root, err) {
  const p = el("p", "tour-fatal");
  p.textContent =
    "The tour failed to load: " +
    (err && err.message ? err.message : String(err));
  root.replaceChildren(p);
}

function init() {
  const root = document.getElementById("tour-root");
  if (!root) return;
  try {
    buildShell(root);
    window.addEventListener("hashchange", syncFromHash);
    syncFromHash();
  } catch (err) {
    renderFatal(root, err);
  }
}

init();
