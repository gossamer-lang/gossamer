# Migrating from Kotlin to Gossamer

Kotlin and Gossamer share null-safe habits, expression-oriented
control flow, lambdas, and pattern-style branching. Gossamer is not
JVM-hosted and does not use exceptions or `suspend`; it uses explicit
`Option<T>`, `Result<T, E>`, goroutines, and channels.

## Quick Map

| Kotlin | Gossamer |
| --- | --- |
| `val x = 5` | `let x = 5` |
| `var x = 5` | `let mut x = 5` |
| `fun f(x: Int): Int = x + 1` | `fn f(x: i64) -> i64 { x + 1 }` |
| `{ x: Int -> x + 1 }` | `|x: i64| x + 1` |
| `if (c) a else b` | `if c { a } else { b }` |
| `when (x) { ... }` | `match x { ... }` |
| `data class User(...)` | `struct User { ... }` |
| `User("Ada", 36)` | `User { name: "Ada", age: 36 }` |
| `sealed class` | `enum` |
| `T?` | `Option<T>` |
| `try` / `catch` | `Result<T, E>` |
| `launch { ... }` | `spawn(|| { ... })` |
| `async { ... }.await()` | channel send and receive |
| `println("$name")` | `println("{name}")` |

## Gossamer 0.47 Syntax At A Glance

Kotlin uses semicolons optionally and commas in parameter lists, data classes,
and collection literals. Gossamer accepts semicolons only as same-line
statement separators and rejects trailing semicolons. Commas are required in a
delimited list on one line; newlines separate items in a multiline list.
Multiline commas are accepted for migration, but `gos fmt` removes them.

```kotlin
data class User(
    val name: String,
    val active: Boolean,
)
val user = User(name = "Ada", active = true)
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

Named structs use keyed braces. Tuple structs and tuple enum variants use
parentheses. Collection and product access stays explicit:

```kotlin
val first = users[0]
val enabled = pair.second
val cached = byName["Ada"] // User?
```

```gos
let users = #[user, rename(user, "Grace")]
let first = users[0]              // List/Vec index; traps if out of bounds
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

## Null Safety To Option

Kotlin:

```kotlin
val len = name?.length
val display = name ?: "anonymous"
val forced = name!!.uppercase()
```

Gossamer:

```gos
let len = name.map(|s: String| s.len())
let display = name.unwrap_or("anonymous")
let forced = strings::to_uppercase(name.unwrap())

if let Some(n) = name {
    println("hello, {n}")
}
```

`None` only exists inside `Option<T>`. It is not a universal null.

## Data Classes To Structs

```kotlin
data class User(val name: String, val age: Int)
val u = User("Ada", 36)
val older = u.copy(age = 37)
```

```gos
struct User {
    name: String
    age: i64
}

let u = User { name: "Ada", age: 36 }
let older = User { age: 37, ..u }
```

Named structs use braces. Tuple-like parentheses are for enum tuple
variants and tuple structs, not ordinary named structs.

## Sealed Classes To Enums

```kotlin
sealed class Outcome
data class Win(val message: String) : Outcome()
data class Loss(val message: String) : Outcome()
object Draw : Outcome()
```

```gos
enum Outcome {
    Win(String)
    Loss(String)
    Draw
}

match outcome {
    Outcome::Win(message) => println("{message}")
    Outcome::Loss(message) => eprintln("{message}")
    Outcome::Draw => println("draw")
}
```

`match` is exhaustive, so adding a new enum variant forces call sites to
handle it.

## Exceptions To Result

Kotlin:

```kotlin
fun readConfig(path: String): Config {
    val text = File(path).readText()
    return parseConfig(text)
}
```

Gossamer:

```gos
use std::{errors, fs}

fn read_config(path: String) -> Result<Config, errors::Error> {
    let text = fs::read_to_string(path)?
    parse_config(text)
}
```

Use `match` when a caller should recover locally:

```gos
let cfg = match read_config(path) {
    Ok(v) => v
    Err(e) => {
        eprintln("config error: {e}")
        default_config()
    }
}
```

## Collections

Kotlin collection chains become `std::iter` pipelines. The pipe operator
passes the value on the left into a free function: a bare callable takes
it as its only argument, and a closure names the slot it fills (`v`
below), so a data-first callee reads left to right.

```kotlin
val total = listOf(1, 2, 3, 4)
    .filter { it % 2 == 0 }
    .sumOf { it * it }
```

```gos
use std::iter

let total = #[1, 2, 3, 4]
    |> |v| iter::filter(v, |n: i64| n % 2 == 0)
    |> |v| iter::sum_by(v, |n: i64| n * n)
```

Mutating operations stay method-shaped:

```gos
let mut xs = #[3, 1, 2]
xs.sort()
xs.push(4)
```

## Coroutines To Goroutines

```kotlin
fun main() = runBlocking {
    val result = async { fetchData(url) }
    println(result.await())
}
```

```gos
let tx, rx = channel()

spawn(|| {
    tx.send(fetch_data(url))
    tx.close()
}()

if let Some(result) = rx.recv() {
    println("{result}")
}
```

For fan-out and fan-in, use `sync::WaitGroup`:

```gos
let wg = sync::WaitGroup::new()
let tx, rx = channel()

for url in urls {
    wg.add(1)
    let tx = tx.clone()
    spawn(|| {
        defer wg.done()
        tx.send(http::get(url, #[]))
    }()
}

spawn(|| {
    wg.wait()
    tx.close()
}()

while let Some(result) = rx.recv() {
    handle(result)
}
```

## Associated Functions

Kotlin companion factories become associated functions:

```gos
struct Connection { host: String }

impl Connection {
    pub fn local() -> Connection {
        Connection { host: "localhost" }
    }
}

const DEFAULT_PORT: i64 = 5432
```

## Integer Overflow And Wrapping Arithmetic

Kotlin's `Int` and `Long` arithmetic wraps silently on overflow, and
`Math.addExact` / `Math.multiplyExact` throw instead. Gossamer's plain `+`,
`-`, and `*` behave like the `Exact` forms under `gos run`, the JIT, and
`gos build` - an overflow panics - and wrap only under
`gos build --release`. Code that relies on the JVM's wrapping (hashes,
`hashCode` combinations, pseudo-random generators) ports to the wrapping
operators, which wrap at the declared width on every tier and in every
profile.

| Kotlin | Gossamer |
| --- | --- |
| `a + b`, `a - b`, `a * b` (wrapping) | `a +% b`, `a -% b`, `a *% b` |
| `Math.addExact(a, b)` | `a + b` (panics on overflow outside release builds) |
| `h = 31 * h + x` | `h = 31 *% h +% x` |
| `Int`, `Long` | `i32`, `i64` |

```kotlin
fun nextSeed(seed: Long): Long = seed * 6364136223846793005L + 1442695040888963407L

fun stringHash(bytes: ByteArray): Int {
    var h = 0
    for (b in bytes) h = 31 * h + b
    return h
}
```

```gos
fn next_seed(seed: i64) -> i64 {
    seed *% 6364136223846793005 +% 1442695040888963407
}

fn string_hash(text: String) -> i32 {
    let mut h: i32 = 0
    for b in text.bytes() { h = 31 *% h +% b as i32 }
    h
}
```

A Kotlin `Byte` is signed while `text.bytes()` yields `u8`, so the two hashes
agree for ASCII text. Compound forms `+%=`, `-%=`, and `*%=` exist as well.

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

Coming from Kotlin: `pub(package)` is `internal`, `pub` is
`public`, and no annotation is closer to `private` than to Kotlin's default -
Kotlin defaults to public, Gossamer defaults to private. There is no
`protected`, because there is no inheritance.

## Standard Library Map

| Kotlin / JVM | Gossamer |
| --- | --- |
| `File(path).readText()` | `fs::read_to_string(path)` |
| `File(path).readBytes()` | `fs::read(path)` |
| `File(path).writeText(s)` | `fs::write(path, s)` |
| `System.getenv("X")` | `env::var("X")` |
| `ProcessBuilder(cmd).start()` | `process::run(cmd, args)` |
| `System.exit(0)` | `process::exit(0)` |
| `println(x)` | `println("{x}")` |
| `Regex(pattern)` | `regex::compile(pattern)` |
| `s.trim()` | `strings::trim(s)` |
| `s.uppercase()` | `strings::to_uppercase(s)` |
| `s.toInt()` | `strconv::parse_i64(s)` |
| `listOf(...)` | `[...]` |
| `mutableListOf(...)` | `let mut xs = [...]` |
| `arrayOf(...)` | `#[...]` |
| `mapOf(k to v)` | `{key: value}` or `{}` for an empty `Map` |
| `setOf(...)` | `#{...}` for `Set`, or typed `BTreeSet` |
| `ArrayDeque` as queue | `Queue<i64>` from `Queue::from([a, b])`, `push`, and FIFO `pop` |
| `ArrayDeque` as stack | `Stack<i64>` from `Stack::from([a, b])`, `push`, and LIFO `pop` |
| `ArrayDeque` as deque | `Deque<i64>` with explicit front/back methods |
| `PriorityQueue` | `MinHeap::from([...])`, or `MaxHeap::from([...])` for max-first order |
| `OkHttp` / `Ktor HttpClient` | `http::Client::new()` or `http::get(url, [])` |
| `ktor server { ... }` | `http::serve(addr, handler)` |
| `kotlinx.serialization` | `encoding::json` |
| `kotlinx.coroutines.launch` | `spawn(|| { ... })` |
| `Mutex()` | `sync::Mutex::new()` |
| `CountDownLatch(n)` | `sync::WaitGroup::new()` |
