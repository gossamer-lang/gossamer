# `lang::unsafe_extern`

`unsafe extern "C" { fn strlen(text: [u8]) -> usize }` declares C functions, called inside `unsafe { }` (GT0097), and `type Name` declares a C type reached only as `ffi::Ptr<Name>` (GT0104). Parameters are scalars, `ffi::Ptr` and `Option<ffi::Ptr>`, out-parameters (`&mut` a scalar or pointer), C function pointers written `Fn(..) -> R` and filled with a named function, slices of scalars or structs, and `#[repr(C)]` structs and `ffi::Union`s by value; results are scalars, pointers, and structs (GT0098). `#[export]` makes a free function a C symbol (GT0113), `[lib] kind` builds a C library and header from the exports, and `[native]` compiles a package's own C sources. `std::ffi` reads and writes foreign memory by copy inside `unsafe` (GT0103), allocates from the C allocator, and holds callback context in `ffi::Handle`. Every call is foreign and unsafe whatever it does, clears and captures `errno` for `ffi::last_errno()` and `ffi::last_os_error()`, and when it blocks lets another worker start the goroutines waiting on its own; `#[link(name, search)]`, `#[link_name]`, and `#[cfg]` choose the library, symbol, and platform. A project refuses native code with `ffi = false` in `project.toml` (the default is `true`), and then a declaration in the project or a dependency is GT0102.

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## Declaring and calling

A block of `fn` declarations marked `unsafe extern "C"` names C functions the
program calls. Each call sits in an `unsafe { }` block, which is where the
program says it vouches for what native code does with its arguments.

```gossamer
use std::ffi

unsafe extern "C" {
    fn abs(x: ffi::c_int) -> ffi::c_int
    fn strlen(text: [u8]) -> ffi::size_t
}

fn main() {
    let text = ffi::cstring("hello").unwrap()
    println(f"{unsafe { abs(-3) }} {unsafe { strlen(text) }}")  // 3 5
}
```

The platform C library is always available, and on Windows `kernel32` too.
Another library is named on the block:

<!-- fragment -->
```gossamer
#[link(name = "z")]
unsafe extern "C" {
    fn zlibVersion() -> usize
}
```

`#[link_name = "symbol"]` calls a C symbol under another Gossamer name, and
`#[cfg(...)]` gives each platform its own declarations. `gos check --target
aarch64-apple-darwin` checks the macOS ones from any host.

## Refusing native code

Native code runs outside everything Gossamer checks. A project allows it by
default and can refuse it in the `[project]` table of `project.toml`:

```toml
[project]
id = "example.com/tool"
version = "0.1.0"
ffi = false
```

With `ffi = false`, every foreign declaration the program reaches is `GT0102`,
whether it sits in the project's own sources or in a dependency's, and
`gos check`, `gos run`, `gos test`, and `gos build` stop. The report lists
every library and file involved, one error per library:

```text
error[GT0102]: the project `example.com/app` declares foreign functions (`abs`, `labs`), but `app/project.toml` refuses native code
  --> src/main.gos:6:5
     6 |     fn abs(x: i32) -> i32
       |     ^^^^^^^^^^^^^^^^^^^^^
  ::> app/src/util.gos:2:5
     2 |     fn labs(x: i64) -> i64
       |     ^^^^^^^^^^^^^^^^^^^^^^
     note: `labs` is declared here

error[GT0102]: the dependency `example.com/beta` declares foreign functions (`labs`, `llabs`), but `app/project.toml` refuses native code
  --> beta/src/lib.gos:2:5
  ::> beta/src/helpers.gos:2:5
```

The root project's manifest decides for the whole program, a dependency's own
`ffi` key included, so a project that refuses native code cannot have a
dependency bring it in. A file run outside any project is not governed, and
the standard library's own declarations (`std::term`) are never refused.

## What crosses

| Gossamer parameter | C sees |
|---|---|
| `i8` .. `u64`, `isize`, `usize`, `bool`, `f32`, `f64` | the scalar |
| `ffi::Ptr<T>` | a non-null `T *` |
| `Option<ffi::Ptr<T>>` | a `T *` that may be `NULL` (`None`) |
| `&mut` a scalar, a `Ptr`, or an `Option<Ptr>` | an out-parameter: `int *`, `T **`, written back |
| `Fn(A..) -> R` | a C function pointer, filled with a named function |
| `[T]` of scalars (a `Vec`, a fixed array, a window) | a pointer to the first element, read-only |
| `&mut [T]` | a pointer whose writes come back |
| a `#[repr(C)]` struct of plain data, or an `ffi::Union` | the struct by value, as C passes it |
| `&mut` such a struct | a `T *` whose writes come back |
| `[S]` or `&mut [S]` of such structs | a pointer to their C-layout elements, `&mut` writes copied back |

A result is a scalar, `()`, `Ptr<T>`, `Option<Ptr<T>>`, or a `#[repr(C)]`
struct. A struct crosses in registers, on the stack, or through a pointer to a
copy, as the target's C calling convention says, so `poll(&mut fds, ..)`,
`writev(fd, iov, ..)`, and a function returning `struct timespec` are declared
the way their C headers read. A project whose `gossamer-version` predates 0.67
gets GT0112 for a by-value struct parameter, which used to cross as a pointer;
`gos check --fix` rewrites it to `&mut`. A `Ptr` result that
comes back `NULL` raises GX0013, so declare `Option<Ptr<T>>` wherever the C API
can answer `NULL`. A pointer a Gossamer argument crosses as is valid for the
call only.

```gossamer
unsafe extern "C" {
    fn memset(dst: &mut [u8], c: i32, n: usize) -> usize
}

fn main() {
    let mut buf = #[0u8; 6]
    unsafe { memset(&mut buf[1..4], 7, 3) }
    println(buf)  // #[0, 7, 7, 7, 0, 0]
}
```

`std::ffi` names the C types for the target (`c_int`, `c_long` - 32 bits on
Windows, 64 elsewhere - `size_t`, ...), and turns strings into C strings and
back with `cstring` and `from_cstr`.

## Foreign types and pointers

A C library hands out handles whose layout it keeps to itself. Declare one with
`type Name` inside the extern block and reach it through `ffi::Ptr`:

<!-- fragment -->
```gossamer
use std::ffi
use std::ffi::Ptr

#[link(name = "sqlite3")]
unsafe extern "C" {
    type Sqlite3
    type Stmt

    fn sqlite3_open(path: [u8], db: &mut Option<Ptr<Sqlite3>>) -> ffi::c_int
    fn sqlite3_prepare_v2(db: Ptr<Sqlite3>, sql: [u8], len: ffi::c_int, stmt: &mut Option<Ptr<Stmt>>, tail: &mut Option<Ptr<u8>>) -> ffi::c_int
    fn sqlite3_step(stmt: Ptr<Stmt>) -> ffi::c_int
    fn sqlite3_column_text(stmt: Ptr<Stmt>, col: ffi::c_int) -> Option<Ptr<u8>>
    fn sqlite3_finalize(stmt: Ptr<Stmt>) -> ffi::c_int
    fn sqlite3_close(db: Ptr<Sqlite3>) -> ffi::c_int
}

fn main() {
    let mut opened: Option<Ptr<Sqlite3>> = None
    unsafe { sqlite3_open(ffi::cstring(":memory:").unwrap(), &mut opened) }
    let db = opened.unwrap()
    defer unsafe { sqlite3_close(db) }
    let mut stmt: Option<Ptr<Stmt>> = None
    let mut tail: Option<Ptr<u8>> = None
    let sql = ffi::cstring("select 'hello'").unwrap()
    unsafe { sqlite3_prepare_v2(db, sql, -1, &mut stmt, &mut tail) }
    let stmt = stmt.unwrap()
    while unsafe { sqlite3_step(stmt) } == 100 {
        let text = unsafe { sqlite3_column_text(stmt, 0) }.unwrap()
        println(unsafe { ffi::read_cstr(text) }.unwrap())
    }
    unsafe { sqlite3_finalize(stmt) }
}
```

A foreign type has no Gossamer value: it is never constructed, read, or passed
by value (GT0104). `ffi::c_void` is the pointee of a `void *`, and
`p.cast::<U>()` converts between pointer types.

A `Ptr` is an ordinary value. It can sit in a struct field or a collection, be
compared, and be sent through a channel to another goroutine; what native code
allows a pointer to be used for is the library's rule to follow. Everything
that reaches memory through it is `unsafe`.

## Foreign memory

Each of these copies, so a Gossamer value never points into memory C may free:

| Operation | Does |
|---|---|
| `ffi::read::<T>(p)`, `ffi::read_at::<T>(p, i)` | copy a `T` (a scalar, a `Ptr`, or a `#[repr(C)]` struct) out of `*p` or `p[i]` |
| `ffi::write(p, v)`, `ffi::write_at(p, i, v)` | copy `v` into `*p` or `p[i]` |
| `ffi::read_bytes(p, n)`, `ffi::read_cstr(p)` | copy `n` bytes, or a NUL-terminated string, into a `Vec<u8>` or `String` |
| `ffi::write_bytes(p, bytes)` | copy bytes into C memory |
| `ffi::alloc::<T>(n)`, `ffi::free(p)` | zeroed memory from, and back to, the platform C allocator |
| `ffi::to_c_bytes(bytes)` | a C-allocated copy, for C to keep or take over |
| `ffi::size_of::<T>()` | the C size of `T` |

```gossamer
use std::ffi
use std::ffi::Ptr

fn main() {
    let words: Ptr<i64> = unsafe { ffi::alloc(3) }
    defer unsafe { ffi::free(words) }
    unsafe { ffi::write_at(words, 2, 42i64) }
    let last: i64 = unsafe { ffi::read_at(words, 2) }
    println(last)  // 42
}
```

Nothing is freed automatically. Memory the library allocates goes back through
the library's own free function; memory the program allocates and hands over
belongs to the library from then on; memory the program allocates and keeps it
frees with `ffi::free`. A struct the library fills (an out-struct with pointer
fields) lives in C memory: allocate it with `ffi::alloc`, pass the `Ptr`, and
`ffi::read` it back. A `Ptr` read out of C memory carries whatever address C
stored, `NULL` included; `p.address() == 0` tests for it.

## Unions, layout, and globals

`ffi::Union<(i64, f64)>` is a C union of the listed members: `Union::new(v)` or
`Union::zeroed()` builds one, `u.get::<f64>()` reads it as a member, and
`u.set(v)` writes one. `ffi::size_of::<T>()`, `align_of::<T>()`, and
`offset_of::<T>("field.path")` answer C layout while compiling (GT0109 for a
path naming no field). A C global is declared `static NAME: T` in the extern
block and reached through `ffi::addr_of(NAME)`:

<!-- fragment -->
```gossamer
unsafe extern "C" {
    static mut opterr: ffi::c_int
}

unsafe { ffi::write(ffi::addr_of(opterr), 0) }
```

`unsafe { ffi::View::new(p, len) }` reads and writes C memory in place, with
`v[i]`, `v.set(i, x)`, `v.slice(lo, hi)`, `v.fill(x)`, `v.copy_from(xs)`, and
`v.to_vec()`, each access bounds-checked; `ffi::atomic_load`, `atomic_store`,
`atomic_swap`, `atomic_compare_exchange`, and `atomic_fetch_add` and its
siblings change a shared 32- or 64-bit integer atomically.

## Callbacks

A C function pointer parameter is written `Fn(A..) -> R` with scalar, pointer,
and `#[repr(C)]` struct types. Pass a top-level function of exactly that signature by name;
state it needs travels through the library's `void *` argument as an
`ffi::Handle`:

```gossamer
use std::ffi
use std::ffi::Ptr

unsafe extern "C" {
    fn qsort(base: &mut [i32], count: usize, size: usize, compare: Fn(Ptr<ffi::c_void>, Ptr<ffi::c_void>) -> ffi::c_int)
}

fn ascending(a: Ptr<ffi::c_void>, b: Ptr<ffi::c_void>) -> ffi::c_int {
    let x: i32 = unsafe { ffi::read(a.cast()) }
    let y: i32 = unsafe { ffi::read(b.cast()) }
    if x < y { -1 } else if x > y { 1 } else { 0 }
}

fn main() {
    let mut xs = #[5i32, 1, 4]
    unsafe { qsort(&mut xs, 3, 4, ascending) }
    println(xs)  // #[1, 4, 5]
}
```

- A callback runs on the thread that invokes it: during a foreign call
  (`qsort`, `sqlite3_exec`), during a later call that runs one the library
  stored (`glfwPollEvents`, `gtk_main`, `DispatchMessage`), or on a thread the
  library started (an audio callback, a worker pool). On a library's thread,
  blocking calls block that thread and a panic ends the program with its
  report.
- A panic inside a callback a foreign call ran is held until that call returns,
  then raised there; it never unwinds through C frames. Meanwhile the callback
  answers zero.
- A callback may allocate, call foreign functions, and block. It spawns only
  inside a `cohort`.
- A closure is not a C function pointer (GT0108): C cannot keep its captures
  alive. A handle can.

## Handles

`ffi::Handle::new(value)` keeps a Gossamer value alive under an integer native
code can hold:

| Call | Does |
|---|---|
| `h.as_ptr()` | the `void *` to hand to C |
| `unsafe { ffi::Handle::<T>::from_ptr(p) }` | the handle back, in a callback |
| `h.get()`, `h.set(v)`, `h.update(f)` | read, replace, or change the value (`f` takes `&mut T`) |
| `h.take()`, `h.release()` | answer and drop it, or just drop it |

A released or unknown handle raises GX0014 rather than reading freed memory. A
handle does not synchronize: goroutines that change one value guard it with
`std::sync`.

## Function pointers

A native function a library hands back (`dlsym`, `GetProcAddress`) becomes a
callable with its C signature:

<!-- fragment -->
```gossamer
let doubler = unsafe { ffi::fn_from_ptr::<Fn(ffi::c_int) -> ffi::c_int>(address) }
println(doubler(21))
```

`ffi::fn_addr(f)` goes the other way: the C entry of the top-level function
`f`, as a `Ptr<c_void>` for the function-pointer field of an ops table or a
registration array (`luaL_Reg`, `sqlite3_module`). `Ptr::null()` fills a field
C leaves empty.

## Linking a library

`#[link(name = "z")]` links `libz` (on Windows `z.lib`, loading `z.dll`). A
library vendored with the project names its directory, relative to the root of
the package that declares it:

<!-- fragment -->
```gossamer
#[link(name = "engine", search = "native")]
unsafe extern "C" {
    fn engine_version() -> ffi::c_int
}
```

`gos run` loads it from there, and `gos build` links it from there; a built
program finds a shared library the way the platform does (`LD_LIBRARY_PATH`,
`DYLD_LIBRARY_PATH`, or beside the `.exe`). On Linux, a release build of a
program that links a library of its own links dynamically, since a static musl
binary cannot load the platform's glibc libraries.

## C sources of your own

A package compiles C it carries with a `[native]` table in `project.toml`:

```toml
[native]
sources = ["csrc/shim.c"]
include = ["csrc/include"]
defines = { LEVEL = "2" }
```

Declarations without `#[link]` reach its functions, under `gos run` and in a
built binary alike, and a dependency's sources come with it. The compiler is
`GOS_CC`, then `CC`, then the platform's (`gos env` shows which); objects are
cached under `.gos-cache/native/` and rebuilt when a source, a header it
includes, or the compiler changes. A fixed-arity shim here is how a program
calls a variadic C function.

## Calling Gossamer from C

`#[export]` makes a free function a C symbol, under its own name or the one
given:

<!-- fragment -->
```gossamer
#[export("calc_add")]
fn add(a: i64, b: i64) -> i64 {
    a + b
}
```

With `[lib] kind = ["staticlib", "cdylib"]` in `project.toml`, `gos build`
writes `lib<name>.a`, the shared library, and `include/<name>.h`. C may call an
export from any of its threads. A host that registered `runtime::at_exit`
hooks calls `<name>_shutdown()` before it exits. An export's types follow the
callback rules; a method, a generic or `comptime` function, `main`, or a
malformed or repeated symbol is GT0113.

## Generating declarations

`gos bindgen --c include/lib.h --link lib -o src/lib_sys.gos` runs `clang`
over a header and writes the extern block, structs, unions, constants, and
callback types for it. Pointers to scalars become slices (`const char *` is
`[u8]`), other pointers `Option<ffi::Ptr<T>>`, and `--target`, repeated, writes
`#[cfg]` where targets differ.

## Not supported

Pointer arithmetic (`read_at`, `write_at`, and `View` index arrays), a separate
`const T *` type, the address of a Gossamer value beyond one call, closures as
C function pointers, ownership annotations, bitfields (an integer field with
shifts models them), variadic functions (a fixed-arity shim in `[native]`
sources calls them), and C++.

## errno and blocking

Every call starts with `errno` cleared (on Windows, the thread's last error
too) and captures both before anything else runs, into the calling
goroutine's own slot. `ffi::last_errno()` reads the C `errno`, so a call that
succeeds without setting it answers 0; `ffi::last_os_error()` reads the
operating system's code, which is `GetLastError` on Windows and `errno`
elsewhere:

```gossamer
use std::ffi

#[cfg(unix)]
unsafe extern "C" {
    fn close(fd: i32) -> i32
}

#[cfg(unix)]
fn main() {
    if unsafe { close(-1) } != 0 {
        println(f"errno {ffi::last_errno()}")  // errno 9
    }
}

#[cfg(windows)]
fn main() {}
```

A call that blocks needs no annotation. Its worker is marked as inside a system
call, and once the call has lasted a millisecond while goroutines wait, another
worker starts to run every goroutine that has not yet started.

## Rules the checker enforces

- A call outside `unsafe { }` is `GT0097`.
- A type with no C form is `GT0098`.
- An effect claim on a declaration (`#[pure]`, `#[readonly]`, `#[effect]`,
  `#[blocking]`, `#[nonblocking]`) is `GT0099`: every foreign call is unsafe and
  foreign, and a parallel callback or a compile-time block may not make one.
- A foreign function used as a value is `GT0100`.
- A declaration active on wasm32 is `GT0101`.
- A declaration in a project whose manifest sets `ffi = false` is `GT0102`,
  reported once per library with a label at each declaration.
- A memory operation, `Ptr::from_address`, or `Handle::from_ptr` outside
  `unsafe { }` is `GT0103`.
- A foreign type used by value is `GT0104`.
- A pointee without a C layout (`Ptr<String>`) is `GT0105`.
- A `Ptr` or `Handle` in a `comptime` result or a serialized value is `GT0106`.
- A callback type with a parameter or result that has no C form is `GT0107`.
- A closure, or a function of another signature, passed as a callback is
  `GT0108`.
- A layout query naming no field is `GT0109`; a union member it does not list,
  without a C layout, or listed twice is `GT0110`; a foreign static named other
  than through `ffi::addr_of` is `GT0111`.
- A by-value struct parameter in a project older than 0.67 is `GT0112`.
- An `#[export]` the C ABI cannot carry is `GT0113`.
- Any other `extern` form is `GP0016`.

<!-- compile_fail GT0097 -->
```gossamer
unsafe extern "C" {
    fn abs(x: i32) -> i32
}

fn main() {
    println(abs(-1))
}
```
