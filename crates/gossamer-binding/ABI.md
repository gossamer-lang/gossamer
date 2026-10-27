# Gossamer Binding ABI 3.0

How `gos` and a Rust binding library talk. The C types live in the
`gossamer-binding-abi` crate (`crates/gossamer-binding-abi/src/lib.rs`);
`gossamer-binding` implements the library side, `gossamer-binding-host` the
`gos run` side, and `gossamer-runtime` the native-build side. Binding authors
write against the Rust source API ([Calling Rust](https://gossamer-lang.org/docs/rust_bindings/)); this document
is for changing the ABI itself or writing a `BindingAbi` impl by hand.

## Entry points

A binding library exports two symbols:

| Symbol | Signature | Answers |
|---|---|---|
| `gos_binding_abi_version` | `extern "C" fn() -> u32` | the `ABI_VERSION` it was built against, `major << 16 \| minor` |
| `gos_binding_init` | `extern "C" fn(*const HostTable) -> *const ModuleTable` | its modules, or null when it refuses the host's table |

A host refuses a library whose major differs from its own or whose minor is
newer, before it calls `gos_binding_init`. `gos_binding_init` keeps the
`HostTable` for the life of the process and is idempotent.

`gos run` builds a program's bindings as one `cdylib`, loads it with `dlopen` /
`LoadLibraryW` (never unloaded), and caches its `signatures.json` beside it. A
library whose `ModuleTable::signature_hash` differs from the cached one is
refused before any call. A native build links the bindings into one staticlib
and calls `gos_rt_binding_init(gos_binding_init)` from `main` before anything
else runs. That staticlib carries its own `std` beside the runtime's, so it
installs `gossamer_binding::RuntimeAllocator` as its global allocator, which
calls the runtime's `gos_rt_binding_heap_*` entries: one allocator owns every
Rust allocation in the program.

## The module table

`ModuleTable` lists `ModuleEntry`s, each listing `ItemEntry`s. An item carries
its name, its documentation, its signature as JSON
(`{"params":[<type>..],"ret":<type>}`), its VM thunk, its wire thunk and
symbol (absent for a module declared without `symbol_prefix:`), and flags:
`ITEM_CALLS_BACK` marks a `cb_fn`. `signature_hash` is FNV-1a over every
module path, item name, and signature, modules sorted by path.

## VM calls

```c
uint32_t vm_thunk(CallCtx *ctx, const AbiValue *args, size_t arg_count, CallOut *out);
```

`AbiValue` is a tag and two words: scalars in `a`; strings and byte buffers
as pointer and length; lists, tuples, maps (key, value pairs), variants (name
first), and structs (name, then name and value per field) as a pointer to
further `AbiValue`s and a count; callables as a handle. Arguments are borrowed
for the call. The answer is written into `out` with storage the library owns,
which the host hands back through `ModuleTable::free_out(out.owner)`. The
status is `STATUS_OK`, `STATUS_ERROR` (the answer is the message), or
`STATUS_PANIC` (the answer is the panic's message, raised as a Gossamer panic
naming the item).

A `cb_fn` receives the call's dispatch. Calling a VM callable goes through
`HostTable::vm_invoke(ctx, handle, args, count, out)`, the same shape, with the
answer freed through `vm_free_out`. A `PersistentCallback` takes a share of
its handle (`vm_callback_retain` / `callback_retain`) and gives it back when
dropped, so a callable stored by one call can be called from a later one.

## Compiled calls

```c
<ret wire type> gos_binding_<prefix>__<item>(<param wire types>...);
```

The wire types are the `BindingAbi::Input` / `Output` of each Rust type:
scalars in registers, `String` as a NUL-terminated runtime string, `Vec` as a
runtime `GosVec`, and `Bytes`, maps, tuples, `Option` / `Result`, structs,
and dynamic values as pointers to the structs in `src/native.rs`.

Ownership:

- **Arguments are borrowed.** The binding copies what it keeps. A wire value
  the caller built for an argument (`Bytes`, a map, a tuple, a struct) is
  freed by the caller once the call returns.
- **Answers belong to the caller.** Every allocation a binding answers comes
  from the runtime: strings through `HostTable::str_new`, vectors through
  `vec_with_capacity` with the element kind that frees their elements, headers
  and field arrays through `alloc`, laid out as the `Box<T>` / `Box<[T]>` the
  runtime frees them as. The caller takes each word it keeps - a string, a
  vector - into its own value and frees the containers that held them.
- **A panic** is caught at the thunk, recorded through
  `HostTable::raise_panic`, and raised by the caller's
  `gos_rt_binding_raise_pending` right after the thunk returns, in the caller's
  frame. The thunk's own answer is then a default nothing reads.

A shape that nests composites crosses as JSON text instead (`BindingAbi::JSON`,
`src/wire_json.rs`), on the VM entry as on the wire thunk: the item's
signature carries `"json": <mask>`, bit 0 the return and bit `i + 1`
parameter `i`, and the caller's walk, generated from the Gossamer type, writes
each such argument and reads the answer. `Option` is `[]` / `[v]` and `Result`
is `[0, v]` / `[1, e]`, so `Some(None)` and `None` stay apart.

Callables cross as runtime callback handles. The caller's share ends with the
call; a `PersistentCallback` holds one of its own.

## Blocking calls

`#[gos_blocking]` wraps a body in `HostTable::blocking_enter` /
`blocking_leave`, which mark the worker as inside a blocking call so the
scheduler runs other goroutines meanwhile.

## Versioning

Within a major, a minor only appends: new fields at the end of `HostTable`
(whose `size` field says which it has), new tags, new flags. A library reads
only fields its host has. A major bump may change anything, and a host
refuses every library of another major.
