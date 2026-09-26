# Gossamer Binding ABI 0.4

Status: shipped in `gossamer-binding` 0.4.0.

This document specifies the four new ABI shapes added in 0.4 and
defines their ownership, lifetime, reclamation, FFI, and threading
guarantees. Earlier shapes (`Unit`, `Bool`, `I64`, `F64`, `Char`,
`String`, `Tuple`, `Vec`, `Option`, `Result`, `Opaque`, `Any`)
are unchanged.

The ABI is additive only - 0.4 does not remove or rename any
0.3 shape, and the C-ABI struct layouts of all 0.3 types are
preserved.

## What changed

| Type | Source spelling | Rust shape | C-ABI struct |
|---|---|---|---|
| `Bytes`    | `Bytes`             | [`Bytes`](src/conv.rs) (newtype around `Vec<u8>`) | [`GosBytes`](src/native.rs) |
| `Map<K,V>` | `Map<K, V>`         | `HashMap<K, V>`       | [`GosMap`](src/native.rs)   |
| `Variant`  | `Variant<arm \| arm \| ...>` | [`DynValue`](src/conv.rs) | [`GosDynVariant`](src/native.rs) |
| `Callback` | `Fn(args...) -> ret` | [`BindingCallback`](src/conv.rs) (interp) / [`NativeCallback`](src/native.rs) (compiled) | `u64` handle |

## Bytes

### Wire shape

```c
struct GosBytes {
    int64_t  len;   // byte length
    int64_t  cap;   // allocated capacity (>= len)
    uint8_t *ptr;   // byte buffer
};
```

The header is allocated on the heap via `Box::into_raw`. The
data buffer is heap-owned (via `Vec::into_boxed_slice` +
`std::mem::forget`); reclamation happens through the runtime's
`gos_rt_bytes_free` helper which mirrors `gos_rt_vec_free`.

### Ownership

- **Producer** (binding fn returning `Bytes`): owns the buffer
  until `to_output` runs. After `to_output`, the runtime owns
  it.
- **Consumer** (binding fn taking `Bytes`): receives a borrow.
  `from_input` performs a copy; the runtime retains its own
  buffer.

### Lifetime

- Returned `Bytes` survives the binding call. Storing it in a
  goroutine-shared `Arc<Mutex<...>>` is safe; the underlying
  buffer is heap-owned. On the interp tier the resulting
  `Value::IntArray(Arc<Vec<i64>>)` is reference counted by its
  `Arc`; on the compiled tier the `GosBytes*` header is heap-owned
  and reclaimed by `gos_rt_bytes_free`.
- Input `Bytes` materialised by `from_input` is a fresh `Vec<u8>`
  owned by the binding. The wire pointer becomes invalid the
  moment the binding fn returns.

### Pinning

Bytes do not pin. The buffer is heap-allocated; address stability
across calls is not guaranteed. Bindings that need stable
addresses (FFI consumers, `mmap` payloads, etc.) must copy.

### Reclamation

Interp tier: stored as `Value::IntArray(Arc<Vec<i64>>)` with each
byte widened to `i64`. Memory cost is 8× the byte length. The `Arc`
reference count reclaims the buffer when the last reference drops.

Compiled tier: stored as `*mut GosBytes`. The runtime's
`gos_rt_bytes_free` reclaims the header (`Box::from_raw`) and the
data buffer (`Vec::from_raw_parts`), the same way `GosVec` is freed -
emitted deterministically by the compiler's drop pass, not by a
collector tick.

### Coroutine / async safety

`Bytes` is `Send + Sync`. Moving it across goroutines is safe.
There is no inherent mutation (the inner `Vec<u8>` is owned, not
shared).

### FFI safety

`*const GosBytes` / `*mut GosBytes` are `#[repr(C)]` and have a
stable layout. The header pointer is `Send + Sync` only because
the data buffer is exclusively owned at any time.

## Map<K, V>

### Wire shape

```c
struct GosMap {
    GosVec *keys;     // parallel keys vector
    GosVec *values;   // parallel values vector
};
```

`keys[i]` pairs with `values[i]`. Order is not significant; for
duplicate keys, the first entry wins.

### Ownership / lifetime / reclamation

Both `keys` and `values` are independent `GosVec` headers with
the same lifetime as a returned `Vec<T>`. The outer
`BindingGosMap` header is heap-owned and reclaimed through
`gos_rt_binding_map_free`. Note that `gos_rt_map_free` targets
the runtime's incompatible `GosMap` layout (a `Mutex<MapStorage>`)
and MUST NOT be called on a binding-side pointer.

### Concrete impls shipped in 0.4

- `HashMap<i64, i64>`
- `HashMap<String, String>`
- `HashMap<String, i64>`

Bindings needing other key/value pairs add their own `BindingAbi`
impls in the binding crate; the macro discovers them via the
trait.

### Hasher

The `BindingAbi` and `FromGos`/`ToGos` impls pin to
`std::collections::HashMap`'s default hasher (`RandomState`).
Generic-over-hasher impls are deliberately not provided - the
ABI surface is per-collection-type, not per-hasher.

## Variant (DynValue)

### Wire shape

```c
struct GosDynVariant {
    const char       *name;        // arm name, NUL-terminated, arena-allocated
    int32_t           payload_len; // number of payload values
    int32_t           pad;         // alignment padding
    GosVariantValue  *payload;     // payload buffer
};
```

`GosVariantValue` reuses the existing 0.3 shape; new payload
tag values added in 0.4 are documented in the macro arm.

### Rust shape

`DynValue` is an enum covering every primitive variant the
runtime can carry plus `Tagged { name, payload }` for the named
arms. The `Tagged` arm is the canonical wire form; bare variants
(Nil, Bool, Int, ...) wrap in a synthetic arm name on output.

```rust
pub enum DynValue {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Char(char),
    String(String),
    Bytes(Vec<u8>),
    List(Vec<DynValue>),
    Map(Vec<(DynValue, DynValue)>),
    Tagged { name: String, payload: Vec<DynValue> },
}
```

### Ownership / lifetime / reclamation

Same model as the existing `GosVariant`. Header + payload buffer are
heap-allocated. The tracing collector and its `gos_rt_gc_reset` tick
were removed (`gos_rt_gc_reset` is now a no-op), so these are reclaimed
deterministically by the compiler's drop pass - or persist until
process exit if they escape that analysis. Arm-name strings are
arena-allocated and escape with the variant.

### Dispatch model

Downstream Gossamer code pattern-matches on the variant arm name
string. The type checker accepts `DynValue` as `Type::Variant(&[])`
(permissive default); bindings that want stricter type
checking declare a custom `Type::Variant(&[VariantArm { name,
payload }])` table.

### Use cases

- **Redis RESP** (Integer / SimpleString / BulkString / Array / Error)
- **Postgres typed columns** (column-type-driven decoding)
- **OpenTelemetry attribute values** (string / bool / i64 / f64 / array)
- **MessagePack / CBOR** decoders

## Callback

A binding takes a Gossamer closure as a `PersistentCallback` or a
`BindingCallback` parameter of a `cb_fn`, and calls it with
`invoke(dispatch, args)`. The two tiers reach the closure differently,
behind the same call:

- **Interpreter**: the parameter wraps the closure `Value`, and `invoke`
  re-enters the interpreter through [`NativeDispatch::call_value`].
- **Compiled tiers**: the caller registers the closure with the runtime
  (`gos_rt_binding_callback_register`) together with the register class
  of each parameter and of the result, and the binding receives the
  `u64` handle. `invoke` converts its arguments to tagged wire values and
  calls `gos_rt_callback_invoke`, which calls the closure's compiled
  code. The `cb_fn`'s `extern "C"` thunk hands the body a
  `CompiledDispatch`, since there is no interpreter to re-enter.

The closure's parameters and result are integers, floats, `bool`, `char`,
and `String` (the result may also be `()`), with at most four parameters.

### Lifetime - STRICT

**Call-scoped.** A callback is valid only for the duration of the binding
fn that received it. On the compiled tiers the caller releases the handle
when the call returns, and an `invoke` after that answers an error instead
of calling anything. On the interpreter the wrapped `Value` may be dropped
or recycled after the return.

### Coroutine / async safety

The interpreter's `invoke` re-enters the interpreter via
`NativeDispatch::call_value`, and goroutine yielding inside the callback
works as it does for any Gossamer call. The compiled path calls the closure
on the binding's own thread, inside the binding call.

## ABI versioning

The binding ABI follows a single integer version exposed at
`gossamer-binding = "0.4"` in `[dependencies]`. The contract:

- **Additive within a major**: new variants / new struct fields
  appended at the end. Existing offsets are stable.
- **Macro-emitted symbols are stable** within a major. The
  C-ABI thunk name format
  `gos_binding_<symbol_prefix>__<item>` does not change.
- **Major bumps break compatibility**. A v0.5 may rename, remove,
  or reorder ABI shapes. v0.5 bindings will not link against
  v0.4 runtimes.
- **Patch / minor (0.4.x → 0.4.y)** is backwards compatible.

## Cross-cutting

### Default-impl for panic-catch

Every `BindingAbi::Output` must be `Default`, because the
macro-generated thunk wraps the user fn in
`std::panic::catch_unwind` and returns `Output::default()` on
panic. All ABI 0.4 outputs (`*mut GosBytes`, `*mut GosMap`,
`*mut GosDynVariant`, `u64`) have `Default` impls (null pointer
or zero).

### Resolver-side mirror

`gossamer-resolve::BindingType` mirrors `gossamer-binding::Type`
exactly. The new shapes (`Bytes`, `Map`, `Variant`, `Callback`)
are reflected verbatim with the same arm names and the same
source-spelling rules.

### Driver-side mirror

`gossamer-driver::DumpedType` mirrors `gossamer-binding::Type` in
JSON form. The runner template `sigs_dump.rs.tmpl` emits the
JSON; the driver parses it. New JSON tags: `"bytes"`, `"map"`,
`"variant"`, `"callback"`.

### Tier coverage matrix

| Type | `gos run` (interpreter / JIT) | `gos build` (LLVM) | `gos build --release` (LLVM) |
|---|---|---|---|
| Bytes        | works | works | works |
| Map<K, V>    | works | works | works |
| Variant      | works | works | works |
| Callback     | works | works | works |

## Failure semantics

- **Type mismatch at the boundary**: `FromGos::from_gos`
  returns `RuntimeError::Type(msg)`. Bindings see this only if
  they ignore the macro and call `FromGos` directly; the macro
  routes the error through the standard runtime error channel.
- **Wire corruption**: null pointers are tolerated and yield
  empty / Nil values. Out-of-range `u8` elements in a `Bytes`
  payload (e.g. `Value::Int(300)`) yield
  `RuntimeError::Type(...)` and do not panic.
- **Panic inside binding fn**: caught by the
  `register_module!`-generated thunk's `catch_unwind`; the
  thunk returns `Output::default()`. Bindings observe this as a
  null/empty return.
- **Callback retention past return**: the compiled tiers answer
  an error from the next `invoke`; the interpreter may panic on it.

## Examples

See `crates/gossamer-binding/tests/abi04_export.rs` for an
end-to-end exercise of every new type via the `register_module!`
macro and the resulting `extern "C"` thunks.

## Compatibility

The 0.4 ABI is backwards compatible with 0.3 binding crates.
Existing binding crates need no changes; they continue to compile
against `gossamer-binding = "0.4"` and produce identical
`extern "C"` symbols.

Forward compatibility: 0.5 will be a major ABI bump.
