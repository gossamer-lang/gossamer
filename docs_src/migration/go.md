# Migrating from Go to Gossamer

Gossamer keeps Go's practical concurrency model: goroutines,
channels, `select`, and `defer` are first-class. The surface syntax is
closer to Rust, so the largest migration cost is mechanical syntax plus
more explicit types and errors.

## Quick Map

| Go | Gossamer | Notes |
| --- | --- | --- |
| `func f(x int) int { return x + 1 }` | `fn f(x: i64) -> i64 { x + 1 }` | `return` is optional for the final expression. |
| `x := 5` | `let x = 5` | Use `let mut` when rebinding. |
| `type Point struct { X int; Y int }` | `struct Point { x: i64, y: i64 }` | Named structs are constructed with braces. |
| `Point{X: 1, Y: 2}` | `Point { x: 1, y: 2 }` | Named fields. |
| `Point{1, 2}` | `Point { x: 1, y: 2 }` | Named structs require keyed fields. |
| tuple-like constructor | `enum Msg { Data(String) }` then `Msg::Data("x")` | Parentheses are for enum variants and tuple structs, not named structs. |
| `func (p Point) Norm() int` | `impl Point { fn norm(&self) -> i64 { ... } }` | Methods live in `impl` blocks. |
| `type Reader interface { Read([]byte) int }` | `trait Reader { fn read(&self, buf: &mut [u8]) -> i64 }` | Traits are nominal. |
| `if err != nil { return err }` | `let v = f()?` | `?` propagates `Err`. |
| `go work()` | `go work()` | Same idea. |
| `defer cleanup()` | `defer cleanup()` | Same idea. |
| `ch <- v` | `tx.send(v)` | Channels use sender and receiver handles. |
| `v, ok := <-ch` | `while let Some(v) = rx.recv() { ... }` | `None` means the channel is closed. |
| `make([]int, 0, 16)` | `Vec::<i64>::with_capacity(16)` | `Vec<T>` owns growable storage; `&[T]` is a borrowed slice view. |
| `make(map[string]int)` | `Map::<String, i64>::new()` | Import from `std::collections`. |
| `map[string]int{"k": 1}` | `{"k": 1}` | Map literal. |
| set via `map[T]struct{}` | `#{...}` | Set literal, or typed `BTreeSet<T>` for ordered sets. |
| FIFO queue slice | `Queue::from([1, 2])` | `push` appends, `pop` removes from the front. |
| stack slice | `Stack::from([1, 2])` | `push` appends, `pop` removes from the top. |
| `container/heap` | `MinHeap::from([...])`, or `MaxHeap::from([...])` | Heap operations are `push`, `pop`, and `peek`. |
| `container/list` or ring-buffer deque | `Deque<i64>` | Use explicit front/back methods. |

Entry files may use top-level statements. Items are hoisted, and bare
statements become the body of an implicit `fn main()`.

## Gossamer 0.47 Syntax At A Glance

Go permits implicit statement termination but still uses commas in multiline
composite literals. Gossamer permits semicolons only between same-line
statements and uses a stricter layout rule:
commas separate items on one line, while newlines separate items in a
multiline delimited list. Legacy multiline commas parse, but `gos fmt` removes
them.

```go
type User struct {
    Name   string
    Active bool
}
user := User{Name: "Ada", Active: true}
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

Named structs require keyed braces. Parentheses construct tuple structs and
tuple enum variants. Collection and field access is explicit:

```go
first := users[0]
enabled := pair.Enabled
cached, ok := byName["Ada"]
```

```gos
let users = #[user, rename(user, "Grace")]
let first = users[0]              // slice/Vec index; traps if out of bounds
let initial = first.name[0]       // String index is a UTF-8 byte as i64
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

## Errors

Go usually returns `(value, error)`. Gossamer uses `Result<T, E>`:

```go
data, err := os.ReadFile("config.toml")
if err != nil {
    return err
}
fmt.Println(len(data))
```

```gos
use std::{errors, fs}

fn run() -> Result<(), errors::Error> {
    let data = fs::read("config.toml")?
    println("{}", data.len())
    Ok(())
}
```

Use `Option<T>` for values that may be absent. There is no nil pointer.

## Interfaces And Traits

Go interfaces are structural. A type satisfies an interface when it has
the right methods. Gossamer traits are nominal, so the conformance is
explicit:

```gos
trait Writer {
    fn write(&mut self, data: [u8]) -> Result<i64, errors::Error>
}

struct Buffer { data: [u8] }

impl Writer for Buffer {
    fn write(&mut self, data: [u8]) -> Result<i64, errors::Error> {
        for b in data {
            self.data.push(b)
        }
        Ok(data.len() as i64)
    }
}
```

Generic bounds use `T: Trait`. Runtime trait objects are not the default
escape hatch; prefer generics or a closed `enum` plus `match`.

## Concurrency

Channels are created with `channel::<T>()`. `channel()` and
`channel(0)` are unbuffered, `channel(n)` is bounded, and
`channel::unbounded()` is explicitly unbounded.

```gos
let tx, rx = channel::<i64>()

spawn(|| {
    defer tx.close()
    for n in 0..3 {
        tx.send(n)
    }
}()

while let Some(n) = rx.recv() {
    println("{n}")
}
```

`select` is Go-shaped:

```gos
select {
    v = rx.recv() => println("got {v}")
    tx.send(42) => println("sent")
    default => println("would block")
}
```

## HTTP

Handlers implement `http::Handler` and return `Result<http::Response,
http::Error>` when they can fail:

```gos
use std::http

struct App { }

impl http::Handler for App {
    fn serve(&self, r: http::Request) -> Result<http::Response, http::Error> {
        if r.path() == "/bytes" {
            return Ok(http::Response {
                status: 200
                body: [65, 0, 66]
                content_type: "application/octet-stream"
            })
        }
        Ok(http::Response::text(200, "hello\n"))
    }
}

fn main() {
    if let Err(e) = http::serve("127.0.0.1:8080", App { }) {
        eprintln("serve failed: {e}")
    }
}
```

`http::get(url, headers)` returns `Result<http::Response,
errors::Error>`. Pass `[]` when there are no headers.

## SQL

`std::database::sql` is a driver registry and wrapper surface. Drivers
register themselves at startup. User code normally opens a connection
through `sql::open(driver, dsn)` or a pool through `sql::Pool::open`.

```gos
use std::database::sql

fn count_users() -> Result<i64, sql::Error> {
    let mut db = sql::open("sqlite", "file:app.db")?
    let mut rows = db.query("select id from users", [])?
    defer rows.close()

    let mut count = 0
    while let Some(_row) = rows.next_row()? {
        count += 1
    }
    Ok(count)
}
```

Do not construct driver wrapper structs directly. They are real named
structs now, so when a fixture does need a literal, it uses braces.

## Collections And Pipelines

Gossamer keeps ordinary loops for side effects and early returns.
Transformation pipelines use free functions in `std::iter` with the
data argument last:

```gos
use std::iter

let total = #[1, 2, 3, 4, 5]
    |> |v| iter::filter(v, |n: i64| n % 2 == 0)
    |> |v| iter::sum_by(v, |n: i64| n * n)
```

The same pipe-friendly shape exists for `std::option` and
`std::result`.

## Integer Overflow And Wrapping Arithmetic

Go integers wrap silently on overflow. Gossamer's plain `+`, `-`, and `*`
do not: they panic on overflow under `gos run`, the JIT, and `gos build`,
and wrap only under `gos build --release`. Code that relies on Go's
wrapping - hashes, checksums, pseudo-random generators, `counter++` on a
`uint8` - ports to the wrapping operators, which wrap at the declared
width on every tier and in every profile.

| Go | Gossamer |
| --- | --- |
| `a + b`, `a - b`, `a * b` (wrapping) | `a +% b`, `a -% b`, `a *% b` |
| `h *= 16777619` | `h *%= 16777619` (also `+%=`, `-%=`) |
| `counter++` on a `uint8` at 255 | `counter +%= 1` |
| `uint32(b)` | `b as u32` |

```go
func fnv1a(data []byte) uint32 {
    hash := uint32(2166136261)
    for _, b := range data {
        hash ^= uint32(b)
        hash *= 16777619
    }
    return hash
}
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

Keep plain operators where overflow would be a bug: the panic reports it
where Go would hand back a wrapped value. `^`, `<<`, `>>`, `&`, and `|` need
no special spelling.

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

Coming from Go, the biggest change is that visibility is
declared, not inferred from capitalization. `Amount` and `amount` are equally
public or private depending on the `pub` you write. Go's package-level
visibility maps to `pub(package)`; Go has no equivalent of `pub`, because a
Go package's exported surface is exported to everyone.

## Common Ports

| Go | Gossamer |
| --- | --- |
| `os.ReadFile(path)` | `fs::read(path)` |
| `os.ReadFile` as text | `fs::read_to_string(path)` |
| `os.WriteFile(path, data, 0644)` | `fs::write(path, data)` |
| `os.Getenv("NAME")` | `env::var("NAME")` |
| `os.Args` | `env::args()` |
| `exec.Command(name, args...).Run()` | `process::run(name, args)` |
| `strings.TrimSpace(s)` | `strings::trim(s)` |
| `strconv.Atoi(s)` | `strconv::parse_i64(s)` |
| `time.Sleep(d)` | `time::sleep(ms)` |
| `sync.WaitGroup` | `sync::WaitGroup` |
| `net/http` server | `std::http` |
| WebSocket handler | `std::http::websocket` |
| SSE handler | `std::http::sse` |
