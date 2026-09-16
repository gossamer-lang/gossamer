# Migrating from Python to Gossamer

Python code ports cleanly once three habits change: types are checked
before execution, absence is represented with `Option<T>`, and failures
are returned as `Result<T, E>` instead of raised as exceptions.

## Quick Map

| Python | Gossamer |
| --- | --- |
| `x = 5` | `let x = 5` |
| reassigning `x` | `let mut x = 5` |
| `def f(x): return x + 1` | `fn f(x: i64) -> i64 { x + 1 }` |
| `class User: ...` for data | `struct User { name: String, age: i64 }` |
| `User("Ada", 36)` | `User { name: "Ada", age: 36 }` |
| `None` | `Option<T>` with `Some(v)` or `None` |
| `try` / `except` | `Result<T, E>` with `?` or `match` |
| `isinstance` dispatch | `enum` plus `match`, or traits |
| list | `Vec<T>` with `[...]`; use `#[...]` for a fixed array and `&[T]` for a borrowed slice |
| dict | `Map<K, V>` with `{key: value}` and `{}` literals |
| set | `Set<T>` with `#{...}` literals, or typed `BTreeSet<T>` with `#{...}` |
| `collections.deque` as queue | `Queue<i64>` from `Queue::from([a, b])`, `push`, and FIFO `pop` |
| `collections.deque` as deque | `Deque<i64>` with explicit front/back methods |
| stack list | `Stack<i64>` from `Stack::from([a, b])`, `push`, and LIFO `pop` |
| `heapq` min-heap | `MinHeap::from([...])`; use `MaxHeap::from([...])` for max-heap order |
| `asyncio.create_task` | `spawn(|| { ... })` |
| `if __name__ == "__main__"` | entry-file top-level statements |

## Gossamer 0.47 Syntax At A Glance

Python uses indentation and permits trailing commas in multiline literals and
calls. Gossamer uses braces, permits semicolons only between statements on one
line, and treats layout inside
delimiters differently: one-line lists require commas; multiline lists use
newlines. Multiline commas are accepted while porting code, but `gos fmt`
removes them.

```python
@dataclass
class User:
    name: str
    active: bool

user = User(name="Ada", active=True)
```

```gos
struct User {
    name: String
    active: bool
}

fn rename(
    user: User
    name: String
) -> User {
    User {
        name: name
        active: user.active
    }
}

enum Lookup {
    Found {
        index: i64
        user: User
    }
    Missing(String)
}

let user = User { name: "Ada", active: true } // one line needs commas
```

Index sequences with `[]`, access struct fields by name and tuple fields by
number, and use `get` when absence is expected:

```python
first = users[0]
enabled = pair[1]
cached = by_name.get("Ada")  # User | None
```

```gos
let users = #[user, rename(user, "Grace")]
let first = users[0]              // Vec/array index; traps if out of bounds
let initial = first.name[0]       // UTF-8 byte as i64, not a Python character
let pair = (first.name, first.active)
let enabled = pair.1
let mut by_name: Map<String, User> = Map::new()
by_name.insert(first.name, first)
let cached = by_name.get("Ada")   // Map lookup returns Option<V>
let found = Lookup::Found {
    index: 0
    user: cached.unwrap()
}
```

## Data Types

Use structs for records. Named structs are always constructed with
braces:

```gos
struct User {
    name: String
    age: i64
}

let user = User { name: "Ada", age: 36 }
let older = User { age: 37, ..user }
```

Use enums for a closed set of shapes:

```gos
enum Event {
    Click(i64, i64)
    Message(String)
    Closed
}

match event {
    Event::Click(x, y) => println("click {x} {y}")
    Event::Message(text) => println("{text}")
    Event::Closed => println("closed")
}
```

## Option Instead Of None

```python
name = user.get("name")
display = name or "anonymous"
```

```gos
let name: Option<String> = user_name()
let display = name.unwrap_or("anonymous")

if let Some(n) = name {
    println("hello, {n}")
}
```

`None` is not a value of every type. It only appears inside
`Option<T>`.

## Result Instead Of Exceptions

```python
try:
    text = Path(path).read_text()
    cfg = parse_config(text)
except Exception as e:
    log(e)
    cfg = default_config()
```

```gos
use std::{errors, fs}

fn read_config(path: String) -> Result<Config, errors::Error> {
    let text = fs::read_to_string(path)?
    parse_config(text)
}

let cfg = match read_config(path) {
    Ok(v) => v
    Err(e) => {
        log(e)
        default_config()
    }
}
```

## Comprehensions And Pipelines

Python:

```python
total = sum(n * n for n in range(1, 11) if n % 2 == 0)
```

Gossamer:

```gos
use std::iter

let total = iter::range_inclusive(1, 10)
    |> |v| iter::filter(v, |n: i64| n % 2 == 0)
    |> |v| iter::sum_by(v, |n: i64| n * n)
```

For stateful code, ordinary loops are still idiomatic:

```gos
let mut counts: Map<String, i64> = Map::new()
for word in words {
    counts.inc(word, 1)
}
```

## Strings And Bytes

`String` is UTF-8. Indexing works on bytes, not Python code points.
Use UTF-8 helpers when code-point semantics matter. Use `[u8]` for
binary data.

```gos
let body: [u8] = fs::read("image.bin")?
let text = fs::read_to_string("message.txt")?
```

HTTP responses can serve binary bodies directly:

```gos
http::Response {
    status: 200
    body: [65, 0, 66]
    content_type: "application/octet-stream"
}
```

## Concurrency

Python `async` code usually becomes goroutines plus channels when work
must run concurrently:

```gos
let tx, rx = channel()

for url in urls {
    let tx = tx.clone()
    spawn(|| {
        tx.send(http::get(url, #[]))
    }()
}

while let Some(result) = rx.recv() {
    handle(result)
}
```

Close the sender when no more values will arrive, or coordinate with
`sync::WaitGroup`.

## Integer Overflow And Wrapping Arithmetic

Python integers grow without bound, so a hash or checksum written in Python
masks by hand to stay in 32 or 64 bits. Gossamer integers have a fixed
width: plain `+`, `-`, and `*` panic when a result leaves the type's range
under `gos run`, the JIT, and `gos build`, and wrap only under
`gos build --release`. The wrapping operators `+%`, `-%`, and `*%` wrap at
the declared width on every tier and in every profile, so the mask becomes
the type.

| Python | Gossamer |
| --- | --- |
| `(h * 16777619) & 0xFFFFFFFF` | `h *% 16777619` with `h: u32` |
| `(a + b) & 0xFFFFFFFFFFFFFFFF` | `a +% b` with `a, b: u64` |
| `h = (h * 31 + c) & 0xFFFFFFFF` | `h = h *% 31 +% c` (also `+%=`, `-%=`, `*%=`) |
| unbounded `int` arithmetic | `std::math::big` |

```python
def fnv1a(data: bytes) -> int:
    h = 2166136261
    for b in data:
        h = ((h ^ b) * 16777619) & 0xFFFFFFFF
    return h
```

```gos
fn fnv1a(data: String) -> u32 {
    let mut hash: u32 = 2166136261
    for b in data.bytes() {
        hash ^= b as u32
        hash *%= 16777619
    }
    hash
}
```

## Visibility

Gossamer has three visibilities, and they are declared per item, per method,
and per struct field.

| Annotation | Reachable from |
| --- | --- |
| none | the declaring module and its descendants |
| `pub(package)` | every module of the declaring package |
| `pub` | anything that depends on the package |

A **package** is the unit of distribution: one `project.toml`, one project id.
A **module** is a directory under `src/`. A module nested inside another is a
**module descendant**, and visibility flows inward only: a descendant reaches
its ancestors' private items, never the reverse.

```gossamer
// src/money/mod.gos
pub struct Amount {
    pub currency: String,
    cents: i64,                     // private representation
}

impl Amount {
    pub fn new(currency: String, cents: i64) -> Amount {
        Amount { currency: currency, cents: cents }
    }
    pub fn cents(&self) -> i64 { self.cents }
    fn normalize(&self) -> i64 { self.cents }   // private helper
}

pub(package) fn round_trip(a: Amount) -> i64 { a.normalize() }
```

A `pub` type may keep private methods and private fields, so a struct with any
private field can only be built by the module that declares it. Importing does
not widen anything: a `use` is a spelling convenience, and visibility is
decided by where the name is used.

Coming from Python, this is the largest change in kind.
`_name` and `__name` are conventions the interpreter mostly does not enforce;
Gossamer's visibility is checked at compile time and a violation is an error,
not a lint. Anything you want another module to reach needs `pub` or
`pub(package)` written on it.

## Standard Library Map

| Python | Gossamer |
| --- | --- |
| `Path(path).read_text()` | `fs::read_to_string(path)` |
| `Path(path).read_bytes()` | `fs::read(path)` |
| `Path(path).write_text(s)` | `fs::write(path, s)` |
| `os.environ.get("X")` | `env::var("X")` |
| `sys.argv` | `env::args()` |
| `subprocess.run([...])` | `process::run(program, args)` |
| `print(x)` | `println("{x}")` |
| `json.dumps(v)` | `encoding::json::encode(v)` |
| `json.loads(s)` | `encoding::json::decode::<T>(s)` |
| `re.compile(p)` | `regex::compile(p)` |
| `s.strip()` | `strings::trim(s)` |
| `int(s)` | `strconv::parse_i64(s)` |
