//! Runtime value representation shared by the bytecode VM and focused
//! interpreter compatibility helpers.
//! Every shared aggregate is backed by [`Arc`] rather than
//! [`std::rc::Rc`] so a [`Value`] can cross thread boundaries, which
//! goroutines running in parallel require.
//! `to_raw` / `from_raw` give the interpreter and the native backend one
//! `u64` value layout; heap objects are registered in a global side table
//! and addressed by `u32` handles.

// `SmolStr` (B2) does tagged-pointer arithmetic to keep
// `Value::String` at 8 bytes inline. The unsafe is confined to
// the few methods on `SmolStr`; everything else in the crate
// keeps the safe-Rust discipline.
#![allow(unsafe_code)]

use std::borrow::Cow;
use std::cell::{Cell, UnsafeCell};
use std::fmt;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;
use std::sync::Arc;

use parking_lot::Mutex;
use smallvec::SmallVec;

use gossamer_runtime::{
    GossamerValue, SINGLETON_FALSE, SINGLETON_TRUE, SINGLETON_UNIT, TAG_FLOAT, TAG_HEAP,
    TAG_IMMEDIATE, TAG_SINGLETON, fits_i56, from_f64, from_heap_handle, from_i64, from_singleton,
    tag_of, to_f64, to_heap_handle, to_i64, to_singleton,
};

mod channel;
mod descriptor;
mod native;
mod render;
mod smol_str;
mod type_tag;

pub use channel::{
    Channel, RecvOutcome, SelectWaiter, SendOutcome, deadlock_error, wake_all_channel_waiters,
};
pub use descriptor::{
    element_render_descriptor, json_descriptor, ordering_descriptor, render_descriptor,
    repl_render_descriptor,
};
pub use native::{
    NativeEnumOwner, NativeEnumShape, NativeFieldKind, NativeFieldPlacement, NativeStructShape,
    NativeVariantShape, native_enum_disc, native_enum_field, native_enum_field_consume,
    native_enum_to_variant, native_shape, native_struct_shape, register_native_shapes,
    register_native_struct_shapes, registry_stats_for_test,
};
use native::{RegistryEntry, register_heap, take_heap};
use render::repr_value;
pub use render::uint_leaves;
pub(crate) use render::{
    ELEM_DESC_MARKER, described_set_order, error_chain_text, f32_render_slot, uint_desc,
    vec_render_items, vec_render_text,
};
pub use smol_str::SmolStr;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use type_tag::empty_struct_fields;
pub use type_tag::{TypeTag, intern_field_name};
pub(crate) use type_tag::{
    empty_value_arc, intern_inline_sites, intern_type_name, intern_type_tag,
};
use type_tag::{
    intern_struct_field_names, intern_struct_field_names_2, type_tag_from_static, variant_fields,
    variant_with_tag_and_fields,
};

/// Dense-entry map backing interpreter `HashMap` values.
pub type DenseMap<K, V> = indexmap::IndexMap<K, V, rustc_hash::FxBuildHasher>;

/// Constructs an empty dense interpreter map with the VM's hash builder.
#[must_use]
pub fn dense_map<K, V>() -> DenseMap<K, V> {
    DenseMap::with_hasher(rustc_hash::FxBuildHasher)
}

/// Constructs a dense interpreter map with an initial entry capacity.
#[must_use]
pub fn dense_map_with_capacity<K, V>(capacity: usize) -> DenseMap<K, V> {
    DenseMap::with_capacity_and_hasher(capacity, rustc_hash::FxBuildHasher)
}

/// Entry count a `Map::with_capacity(n)` call reserves.
///
/// A negative capacity is a type error, as on the compiled tiers.
pub fn map_capacity(capacity: i64) -> RuntimeResult<usize> {
    usize::try_from(capacity).map_err(|_| {
        RuntimeError::Type("HashMap::with_capacity: capacity must be non-negative".to_string())
    })
}

/// Empty `Vec` with room for `capacity` elements, the storage `Vec::with_capacity(n)` builds.
///
/// A negative capacity is a type error and a byte size past `isize::MAX` panics with
/// `capacity overflow`, as on the compiled tiers.
pub fn vec_with_capacity<T>(capacity: i64) -> RuntimeResult<Vec<T>> {
    let Ok(capacity) = usize::try_from(capacity) else {
        return Err(RuntimeError::Type(
            "Vec::with_capacity: capacity must be non-negative".to_string(),
        ));
    };
    let Some(bytes) = capacity
        .checked_mul(std::mem::size_of::<T>())
        .filter(|bytes| isize::try_from(*bytes).is_ok())
    else {
        return Err(RuntimeError::Panic("capacity overflow".to_string()));
    };
    let mut storage = Vec::new();
    storage
        .try_reserve_exact(capacity)
        .map_err(|_| RuntimeError::Panic(format!("memory allocation of {bytes} bytes failed")))?;
    Ok(storage)
}

/// Copy of `items` that keeps its capacity. A mutating builtin answers the receiver's storage
/// rebuilt, and that rebuilt storage is the vector whose `capacity()` the program observes.
#[must_use]
pub fn copy_with_capacity<T: Clone>(items: &Vec<T>) -> Vec<T> {
    let mut copy = Vec::with_capacity(items.capacity());
    copy.extend_from_slice(items);
    copy
}

/// Thin fixed-byte owner used by packed arrays.
#[derive(Debug)]
pub struct PackedBytes {
    ptr: NonNull<u8>,
    len: u32,
}

unsafe impl Send for PackedBytes {}
unsafe impl Sync for PackedBytes {}

impl From<Vec<u8>> for PackedBytes {
    fn from(values: Vec<u8>) -> Self {
        let boxed = values.into_boxed_slice();
        let len = u32::try_from(boxed.len()).expect("packed byte array exceeds u32 length");
        let ptr = if boxed.is_empty() {
            NonNull::dangling()
        } else {
            NonNull::new(boxed.as_ptr().cast_mut()).expect("boxed byte pointer is non-null")
        };
        std::mem::forget(boxed);
        Self { ptr, len }
    }
}

impl Clone for PackedBytes {
    fn clone(&self) -> Self {
        self.to_vec().into()
    }
}

impl Deref for PackedBytes {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len as usize) }
    }
}

impl DerefMut for PackedBytes {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len as usize) }
    }
}

impl Drop for PackedBytes {
    fn drop(&mut self) {
        let raw = std::ptr::slice_from_raw_parts_mut(self.ptr.as_ptr(), self.len as usize);
        drop(unsafe { Box::from_raw(raw) });
    }
}

/// Shared JSON tree plus a stable view into one node of that tree.
///
/// `json::get` / `json::at` can return a child object or array by cloning this
/// lightweight handle instead of deep-cloning the selected subtree. Scalars are
/// still projected into ordinary interpreter values at the query boundary.
#[derive(Debug, Clone)]
pub struct JsonInner {
    tree: Arc<gossamer_std::json::Value>,
    view: usize,
}

impl JsonInner {
    /// Owns `value` as a new canonical JSON tree and views its root.
    #[must_use]
    pub fn new(value: gossamer_std::json::Value) -> Self {
        let tree = Arc::new(value);
        let view = Arc::as_ptr(&tree) as usize;
        Self { tree, view }
    }

    /// Borrows the JSON node viewed by this handle.
    #[must_use]
    pub fn as_value(&self) -> &gossamer_std::json::Value {
        // SAFETY: `view` is either the stable address of `tree`'s root from
        // `new`, or the address of a child borrowed from the same tree by
        // `child`. The `Arc` keeps the tree allocation alive for this handle.
        unsafe { &*(self.view as *const gossamer_std::json::Value) }
    }

    /// Builds a handle viewing `child`, which must be borrowed from this
    /// handle's tree.
    #[must_use]
    pub fn child(&self, child: &gossamer_std::json::Value) -> Self {
        Self {
            tree: Arc::clone(&self.tree),
            view: std::ptr::from_ref(child) as usize,
        }
    }

    /// Clones the viewed JSON node for APIs that genuinely need an owned DOM.
    #[must_use]
    pub fn to_owned_value(&self) -> gossamer_std::json::Value {
        self.as_value().clone()
    }
}

/// A mutable VM value that is confined to the OS thread that created it.
///
/// `MutCell` values are created only by `CellNew` / `CellNewMove` around an
/// immediate `&mut` call, then consumed by its matching `CellTake`. They do
/// not escape that call protocol or cross a goroutine boundary. Keeping their
/// `Arc` handle lets [`Value`] retain its process-wide transport properties,
/// while avoiding a mutex acquisition on each local read or write.
///
/// The owner check is a defensive boundary around the `UnsafeCell`: should a
/// future compiler path accidentally let a transient cell cross threads, it
/// panics before dereferencing the value instead of creating a data race.
/// The borrow flag preserves the mutex's exclusive-access contract and makes
/// accidental re-entrant access fail deterministically.
pub struct ThreadConfinedCell {
    owner: std::thread::ThreadId,
    borrowed: Cell<bool>,
    value: UnsafeCell<Value>,
}

// Access to `value` is permitted only after `lock` verifies that the current
// thread is `owner`. `ThreadConfinedCellGuard` is !Send, so a borrowed value
// cannot be moved to another thread. The Arc control block remains atomic,
// making handle clones and drops safe even when an invalid handle is moved.
unsafe impl Send for ThreadConfinedCell {}
unsafe impl Sync for ThreadConfinedCell {}

impl fmt::Debug for ThreadConfinedCell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ThreadConfinedCell")
            .field("owner", &self.owner)
            .finish_non_exhaustive()
    }
}

impl ThreadConfinedCell {
    #[must_use]
    pub(crate) fn new(value: Value) -> Self {
        Self {
            owner: std::thread::current().id(),
            borrowed: Cell::new(false),
            value: UnsafeCell::new(value),
        }
    }

    /// Borrows the transient value on its owner thread.
    ///
    /// Panics if a foreign thread or a re-entrant caller attempts access,
    /// preserving the exclusive-access contract formerly provided by `Mutex`.
    pub fn lock(&self) -> ThreadConfinedCellGuard<'_> {
        assert_eq!(
            self.owner,
            std::thread::current().id(),
            "transient VM MutCell accessed from a different thread"
        );
        assert!(
            !self.borrowed.replace(true),
            "transient VM MutCell accessed re-entrantly"
        );
        ThreadConfinedCellGuard {
            cell: self,
            // A guard must remain on its owner thread: its Drop resets a
            // thread-local borrow flag and it may expose `&mut Value`.
            _not_send: PhantomData,
        }
    }

    fn into_inner(self) -> Value {
        self.value.into_inner()
    }
}

/// Exclusive, non-send access to a [`ThreadConfinedCell`] value.
pub struct ThreadConfinedCellGuard<'a> {
    cell: &'a ThreadConfinedCell,
    _not_send: PhantomData<std::rc::Rc<()>>,
}

impl std::ops::Deref for ThreadConfinedCellGuard<'_> {
    type Target = Value;

    fn deref(&self) -> &Self::Target {
        // SAFETY: `lock` checked the owning thread and set the exclusive
        // borrow flag before constructing this guard. The guard is !Send.
        unsafe { &*self.cell.value.get() }
    }
}

impl std::ops::DerefMut for ThreadConfinedCellGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: as above, and `&mut self` guarantees the caller has the
        // guard's unique mutable access for the duration of this borrow.
        unsafe { &mut *self.cell.value.get() }
    }
}

impl Drop for ThreadConfinedCellGuard<'_> {
    fn drop(&mut self) {
        self.cell.borrowed.set(false);
    }
}

/// One runtime value produced or consumed by the interpreter.
///
/// Unboxed integer / float / bool / char types sit inline; aggregates
/// (strings, tuples, arrays, structs) are reference-counted so that
/// assignment and argument passing share their backing storage, mirror-
/// ing the GC semantics described in SPEC §3.3.
///
/// **B1 layout (this commit).** Every variant payload is at most
/// one pointer / one scalar, so `size_of::<Value>() == 16` (one
/// 8-byte payload + 8-byte discriminant/padding). Pre-B1, the
/// `FloatArray` / `Variant` / `Struct` / `Builtin` / `Native`
/// variants inlined a `String` (24 bytes) plus an `Arc`, pushing
/// `size_of::<Value>` to 48 bytes - every register-file slot
/// paid the worst-case width even when holding `Int(i64)`. We
/// pull each heavy variant behind an `Arc<Inner>` so the enum
/// payload is one ptr; cloning a `Value` is now a refcount
/// bump in the worst case instead of a `String::clone`.
#[derive(Debug, Clone)]
pub enum Value {
    /// `()`.
    Unit,
    /// `bool`.
    Bool(bool),
    /// Signed 64-bit integer.
    Int(i64),
    /// 64-bit float.
    Float(f64),
    /// `char`.
    Char(char),
    /// UTF-8 string. Stored inline when ≤ 7 bytes (no heap
    /// allocation); otherwise an `Arc<String>` behind a tag
    /// bit. See [`SmolStr`].
    String(SmolStr),
    /// A parsed JSON document retained in the stdlib's canonical tree.
    ///
    /// Keeping this behind an `Arc` lets `json::parse` hand a document
    /// directly to `json::render` without first allocating an interpreter
    /// array/map tree and then rebuilding the same JSON tree for encoding.
    /// JSON query builtins expose children lazily when a program actually
    /// traverses the document.
    Json(Arc<JsonInner>),
    /// Tuple aggregate.
    Tuple(Arc<Vec<Value>>),
    /// Boxed aggregate storage shared by fixed arrays and non-packed Vec
    /// values. Static type checking keeps their identities and method
    /// capabilities distinct even when this internal representation matches.
    Array(Arc<Vec<Value>>),
    /// Flat f64 storage for an array of a struct whose fields
    /// are all `f64`.
    FloatArray(Arc<FloatArrayInner>),
    /// Flat `i64` storage for a primitive integer array literal.
    IntArray(Arc<Vec<i64>>),
    /// Packed storage for a `u8` array or vector.
    ByteArray(Arc<PackedBytes>),
    /// Single-allocation storage for large fixed byte arrays.
    InlineByteArray(Arc<SmallVec<[u8; 1024]>>),
    /// Growable packed storage for a `Vec<u8>`.
    ByteVec(Arc<Vec<u8>>),
    /// Flat `f64` storage for a primitive float array literal /
    /// `Vec<f64>`. Avoids per-element `Value::Float` boxing on
    /// hot loops over numeric arrays (nbody's `dx`/`dy`/`dz`/`mag`
    /// scratch arrays read every f64 here straight into a typed
    /// register).
    FloatVec(Arc<Vec<f64>>),
    /// Opaque VM-only lazy iterator state handle. The concrete state lives in
    /// the stdlib `iter` registry so the `Value` enum does not recursively
    /// carry iterator closures and upstream states. The handle owns that
    /// registry slot: the last share dropping releases the state, so a cursor
    /// abandoned before exhaustion costs nothing beyond its own lifetime.
    LazyIter(Arc<crate::stdlib_builtins::iter::LazyIterHandle>),
    /// Enum variant or tuple-struct constructor payload.
    Variant(Arc<VariantInner>),
    /// Struct-shaped aggregate.
    Struct(Arc<StructInner>),
    /// Native (compiled-representation) enum value handed across the
    /// JIT boundary as a raw pointer. Structural access goes through
    /// the carried shape; drop of the last clone releases the
    /// reference through the runtime.
    NativeEnum(Arc<NativeEnumOwner>),
    /// User-defined callable.
    Closure(Arc<Closure>),
    /// Built-in intrinsic callable.
    Builtin(Arc<BuiltinInner>),
    /// Built-in callable that can re-enter the interpreter through a
    /// [`NativeDispatch`] handle.
    Native(Arc<NativeInner>),
    /// Concurrent channel endpoint.
    Channel(Channel),
    /// Hash-map aggregate. `IndexMap` keeps entries dense while retaining
    /// O(1) lookup through the Fx hasher; this avoids hashbrown's full
    /// `(K, V)` power-of-two bucket slack on map-heavy workloads. The mutex keeps
    /// `Value: Send + Sync` so goroutines can pass maps through
    /// channels.
    Map(Arc<parking_lot::Mutex<crate::vm_map::VmMap>>),
    /// Typed `HashMap<i64, i64>` aggregate. Skips the [`MapKey`]
    /// enum-tag dispatch on every op and avoids the [`Value`]
    /// box around each integer value. k-nucleotide's k-mer
    /// frequency tables ride this variant, dropping per-iteration
    /// hash + compare cost dramatically.
    IntMap(Arc<parking_lot::Mutex<DenseMap<i64, i64>>>),
    /// Typed `HashMap<String, i64>` aggregate. Drops both the
    /// [`MapKey`] enum tag and the [`Value`] box around each count:
    /// an entry is a bare `(SmolStr, i64)`, ~16 bytes lighter than
    /// the generic `Map`'s `(MapKey, Value)`. Because a `HashMap`
    /// keeps entries dense, that per-entry saving translates directly into
    /// lower peak RSS for string-frequency tables (k-mer / n-gram / token
    /// counts).
    StrIntMap(Arc<parking_lot::Mutex<DenseMap<SmolStr, i64>>>),
    /// Unsigned 64-bit integer - same bit pattern as `Int(n as i64)`
    /// but formats as an unsigned decimal value. Used exclusively for
    /// `x as u64` casts to preserve unsigned display semantics.
    Uint(u64),
    /// Non-owning weak reference produced by `x.downgrade()`. Observes
    /// the liveness of the referent's `Arc` without keeping it alive;
    /// `w.upgrade()` yields `Some` while a strong reference survives and
    /// `None` once the last one is dropped.
    Weak(WeakValue),
    /// Write-back cell carrying a `&mut Vec<T>` / `&mut [T]` call
    /// argument. The caller wraps the aggregate at the call site,
    /// the callee unwraps it at frame entry and stores the final
    /// parameter value back on return, and the caller then reads it
    /// out - write-through `&mut` parameter semantics on top of the
    /// VM's clone-on-write value model. Never escapes the call
    /// protocol: no user-visible op ever observes a `MutCell`.
    MutCell(Arc<ThreadConfinedCell>),
    /// Shared storage for a local that a closure captures by managed
    /// reference.
    ///
    /// The enclosing binding's cell register and the closure's upvalue
    /// name the same cell, so an in-place mutation reached through
    /// either is observed by the other - the VM's stand-in for the
    /// single heap buffer the compiled tiers share. Assigning the whole
    /// variable installs a *fresh* cell instead of writing through this
    /// one, which keeps each binding's slot independent exactly as a
    /// pointer-sized slot is on the compiled tiers. The mutex keeps
    /// `Value: Send + Sync`, so a captured local may cross a goroutine
    /// boundary. Never observed by a user-visible op: the compiler
    /// brackets every instruction that names the binding with the
    /// matching capture-cell load / store.
    CaptureCell(Arc<Mutex<Value>>),
    /// Poisoned / uninitialised sentinel.
    Void,
}

impl Value {
    /// Returns the source-level type name most useful in a runtime diagnostic.
    ///
    /// This deliberately hides VM storage variants such as `IntArray` and
    /// `FloatVec`: users wrote `Vec<i64>` and `Vec<f64>`, not those internal
    /// The integer this value holds, whichever width it was written at.
    ///
    /// An unsigned expression carries a `Uint`, so a reader that matches only
    /// `Int` sees nothing where a `u64` was passed - and a silent default then
    /// answers for a value the program did supply. Every reader of an integer
    /// argument goes through here so the two representations are one.
    #[must_use]
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Int(n) => Some(*n),
            Self::Uint(n) => Some(*n as i64),
            _ => None,
        }
    }

    /// The bytes of a byte-vector value, borrowed where the representation is
    /// already packed and materialised only where it is not.
    ///
    /// A `Vec<u8>` reaches a builtin in whichever representation the VM chose
    /// for it - packed (`ByteArray`, `InlineByteArray`, `ByteVec`), flat
    /// integers (`IntArray`), or boxed values (`Array`) - and which one it gets
    /// depends on how the program built it, not on its type. A `String` stands
    /// for its own UTF-8. Every builtin that takes `[u8]` reads it through
    /// here, so a representation is understood by all of them or by none.
    ///
    /// `None` for a value that is not a byte vector, including an `Array`
    /// holding anything but integers. Integers are taken at their low byte, as
    /// `as u8` does.
    #[must_use]
    pub fn byte_slice(&self) -> Option<Cow<'_, [u8]>> {
        match self {
            Self::ByteArray(bytes) => Some(Cow::Borrowed(bytes.as_ref())),
            Self::InlineByteArray(bytes) => Some(Cow::Borrowed(bytes.as_slice())),
            Self::ByteVec(bytes) => Some(Cow::Borrowed(bytes.as_slice())),
            Self::String(text) => Some(Cow::Borrowed(text.as_str().as_bytes())),
            Self::IntArray(ns) => Some(Cow::Owned(ns.iter().map(|n| *n as u8).collect())),
            Self::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items.iter() {
                    out.push(item.as_i64()? as u8);
                }
                Some(Cow::Owned(out))
            }
            _ => None,
        }
    }

    /// The bytes of a byte-vector value, empty where it is not one.
    #[must_use]
    pub fn bytes_or_empty(&self) -> Vec<u8> {
        self.byte_slice().map_or_else(Vec::new, Cow::into_owned)
    }

    /// The bytes of `start..end`, gathered without reading the rest.
    ///
    /// A caller that wants a window of a buffer pays for the window: a
    /// representation that already holds bytes lends them, and one that holds
    /// boxed elements builds only the elements asked for. Reaching a window
    /// through [`Self::byte_slice`] would instead cost the whole buffer on
    /// every call, which is what a store checking one record in a resident
    /// file does per read.
    ///
    /// `None` where the value is not a byte buffer or the window is not
    /// inside it.
    #[must_use]
    pub fn byte_window(&self, start: usize, end: usize) -> Option<Cow<'_, [u8]>> {
        if end < start {
            return None;
        }
        match self {
            Self::ByteArray(bytes) => bytes.get(start..end).map(Cow::Borrowed),
            Self::InlineByteArray(bytes) => bytes.as_slice().get(start..end).map(Cow::Borrowed),
            Self::ByteVec(bytes) => bytes.as_slice().get(start..end).map(Cow::Borrowed),
            Self::String(text) => text.as_str().as_bytes().get(start..end).map(Cow::Borrowed),
            Self::IntArray(ns) => ns
                .get(start..end)
                .map(|w| Cow::Owned(w.iter().map(|n| *n as u8).collect())),
            Self::Array(items) => {
                let window = items.get(start..end)?;
                let mut out = Vec::with_capacity(window.len());
                for item in window {
                    out.push(item.as_i64()? as u8);
                }
                Some(Cow::Owned(out))
            }
            _ => None,
        }
    }

    /// The bit pattern this value holds, whichever width it was written at.
    #[must_use]
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Int(n) => Some(*n as u64),
            Self::Uint(n) => Some(*n),
            _ => None,
        }
    }

    /// representations.
    #[must_use]
    pub fn type_name(&self) -> String {
        match self {
            Self::Unit => "()".to_string(),
            Self::Bool(_) => "bool".to_string(),
            Self::Int(_) => "i64".to_string(),
            Self::Float(_) => "f64".to_string(),
            Self::Char(_) => "char".to_string(),
            Self::String(_) => "String".to_string(),
            Self::Json(_) => "json::Value".to_string(),
            Self::Tuple(_) => "tuple".to_string(),
            Self::Array(_) | Self::FloatArray(_) | Self::IntArray(_) => "Vec".to_string(),
            Self::ByteArray(_) | Self::InlineByteArray(_) | Self::ByteVec(_) => {
                "Vec<u8>".to_string()
            }
            Self::FloatVec(_) => "Vec<f64>".to_string(),
            Self::LazyIter(_) => "iter::Iter".to_string(),
            Self::Variant(inner) => inner.name.to_string(),
            Self::Struct(inner) => inner.name.to_string(),
            Self::NativeEnum(_) => "enum".to_string(),
            Self::Closure(_) => "closure".to_string(),
            Self::Builtin(_) | Self::Native(_) => "function".to_string(),
            Self::Channel(_) => "Channel".to_string(),
            Self::Map(_) | Self::IntMap(_) | Self::StrIntMap(_) => "Map".to_string(),
            Self::Uint(_) => "u64".to_string(),
            Self::Weak(_) => "Weak".to_string(),
            Self::MutCell(cell) => cell.lock().type_name(),
            Self::CaptureCell(cell) => cell.lock().type_name(),
            Self::Void => "uninitialized value".to_string(),
        }
    }

    /// Renders this value as a source-like representation for interactive
    /// inspection. Unlike [`fmt::Display`], strings and chars are quoted.
    #[must_use]
    pub fn repr(&self) -> String {
        repr_value(self)
    }

    /// Borrows the elements of an `Array` or `Tuple` as a slice - both back
    /// onto `[Value]`, so read-only element access shares one path.
    #[must_use]
    pub(crate) fn as_value_slice(&self) -> Option<&[Value]> {
        match self {
            Value::Array(a) => Some(a),
            Value::Tuple(a) => Some(a),
            _ => None,
        }
    }
}

/// Iteratively reclaim a tree of owned child `Value`s with an explicit
/// worklist, so a depth-N recursive aggregate (linked list, tree, graph) tears
/// down in O(N) heap and O(1) native stack instead of overflowing the host
/// stack through nested `Arc` drop glue. Seeded by the `Drop` impls of the
/// recursive aggregate payloads ([`VariantInner`] / [`StructInner`]); once
/// teardown enters through one of those, the whole reachable owned-`Value`
/// subgraph is dismantled here.
///
/// For each popped value that uniquely owns children, the children are moved
/// onto the worklist and the now-childless shell drops shallowly. A
/// still-shared payload (`try_unwrap` returns `Err`) is just dereferenced. The
/// `Drop`-implementing payloads (`Variant`/`Struct`) are emptied with
/// `mem::take` rather than a field move, which a `Drop` type forbids; the
/// nested drop of the emptied shell re-enters this routine with nothing to do,
/// so the native recursion stays at most one frame deep.
///
/// Aggregate map *keys* (`MapKey::Agg`) keep their own drop glue: a deeply
/// nested aggregate used as a map key is neither a `Value` chain nor an
/// idiomatic shape, so it is out of scope here.
fn dismantle_children(mut stack: Vec<Value>) {
    while let Some(v) = stack.pop() {
        match v {
            Value::Variant(a) => {
                if let Ok(mut inner) = Arc::try_unwrap(a) {
                    stack.extend(std::mem::take(&mut inner.fields));
                }
            }
            Value::Struct(a) => {
                if let Ok(mut inner) = Arc::try_unwrap(a) {
                    let fields = std::mem::take(&mut inner.fields);
                    stack.extend(fields.into_values());
                }
            }
            Value::Tuple(a) | Value::Array(a) => {
                if let Ok(vec) = Arc::try_unwrap(a) {
                    stack.extend(vec);
                }
            }
            Value::Closure(a) => {
                if let Ok(inner) = Arc::try_unwrap(a) {
                    stack.extend(inner.capture_values);
                }
            }
            Value::Map(a) => {
                if let Ok(m) = Arc::try_unwrap(a) {
                    stack.extend(m.into_inner().into_values());
                }
            }
            Value::MutCell(a) => {
                if let Ok(m) = Arc::try_unwrap(a) {
                    stack.push(m.into_inner());
                }
            }
            Value::CaptureCell(a) => {
                if let Ok(m) = Arc::try_unwrap(a) {
                    stack.push(m.into_inner());
                }
            }
            _ => {}
        }
    }
}

/// Native-stack recursion depth below which recursive aggregate teardown is
/// left to drop directly (cheap, allocation-free). At or above it a payload
/// switches to the iterative [`dismantle_children`] worklist, bounding the host
/// stack so a deep chain cannot overflow it. Comfortably below any thread's
/// stack budget while keeping the common shallow case off the worklist.
const DROP_RECURSION_LIMIT: u32 = 512;

thread_local! {
    /// Current recursive-drop nesting depth for the aggregate payloads on this
    /// thread. Read once per `VariantInner` / `StructInner` drop to decide
    /// recurse-vs-iterate; never observed by user code.
    static DROP_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// RAII restore of [`DROP_DEPTH`], so the depth is correct even if a nested
/// drop unwinds.
struct DropDepthGuard(u32);

impl Drop for DropDepthGuard {
    fn drop(&mut self) {
        DROP_DEPTH.with(|d| d.set(self.0));
    }
}

impl Drop for VariantInner {
    fn drop(&mut self) {
        if self.fields.is_empty() {
            return;
        }
        let depth = DROP_DEPTH.with(std::cell::Cell::get);
        if depth >= DROP_RECURSION_LIMIT {
            // Deep: flatten the remaining subgraph iteratively. `mem::take`
            // leaves the shell empty so its post-return field drop is a no-op,
            // and the worklist's own emptied shells re-enter as no-ops.
            dismantle_children(std::mem::take(&mut self.fields).into_vec());
            return;
        }
        // Shallow: take the fields (alloc-free for the inline arity) and drop
        // them ourselves with the depth raised, so a long chain trips the
        // iterative path before it can overflow the host stack.
        DROP_DEPTH.with(|d| d.set(depth + 1));
        let _guard = DropDepthGuard(depth);
        drop(std::mem::take(&mut self.fields));
    }
}

impl Drop for StructInner {
    fn drop(&mut self) {
        if self.fields.is_empty() {
            return;
        }
        let depth = DROP_DEPTH.with(std::cell::Cell::get);
        if depth >= DROP_RECURSION_LIMIT {
            let fields = std::mem::take(&mut self.fields);
            dismantle_children(fields.into_values().into_vec());
            return;
        }
        DROP_DEPTH.with(|d| d.set(depth + 1));
        let _guard = DropDepthGuard(depth);
        drop(std::mem::take(&mut self.fields));
    }
}

/// Type-erased weak handle backing [`Value::Weak`]. Each arm holds a
/// `std::sync::Weak` to the corresponding heap variant's `Arc`, so
/// upgrading reconstructs the original `Value` shape when the referent
/// is still alive. A downgrade of a non-heap (Copy) value records
/// [`WeakValue::Dead`] - there is no allocation to observe, so it never
/// upgrades.
#[derive(Debug)]
pub enum WeakValue {
    /// Weak reference to a [`Value::Variant`] payload.
    Variant(std::sync::Weak<VariantInner>),
    /// Weak reference to a [`Value::Struct`] payload.
    Struct(std::sync::Weak<StructInner>),
    /// Weak reference to a [`Value::Array`] payload.
    Array(std::sync::Weak<Vec<Value>>),
    /// Weak reference to a [`Value::Tuple`] payload.
    Tuple(std::sync::Weak<Vec<Value>>),
    /// Weak reference to a [`Value::NativeEnum`] node, observed through the
    /// runtime's intrusive weak count. Boxed so this variant is a single
    /// niche-bearing pointer like the others, keeping `WeakValue` (and thus the
    /// inline `Value::Weak`) within the 16-byte hot-`Value` budget. Kept inline
    /// in `Value` (not `Arc`-wrapped) so each `Value` clone/drop maps 1:1 to the
    /// intrusive weak retain/release below.
    NativeEnum(Box<NativeEnumWeakRef>),
    /// Downgrade of a value with no observable allocation; never upgrades.
    Dead,
}

/// The referent identity of a [`WeakValue::NativeEnum`]: the tagged native
/// pointer (disc bits intact) and the layout needed to rebuild a strong handle.
#[derive(Debug)]
pub struct NativeEnumWeakRef {
    /// Tagged native pointer of the referent.
    pub ptr: usize,
    /// Layout for the rebuilt handle.
    pub shape: Arc<NativeEnumShape>,
}

impl Clone for WeakValue {
    fn clone(&self) -> Self {
        match self {
            WeakValue::Variant(w) => WeakValue::Variant(w.clone()),
            WeakValue::Struct(w) => WeakValue::Struct(w.clone()),
            WeakValue::Array(w) => WeakValue::Array(w.clone()),
            WeakValue::Tuple(w) => WeakValue::Tuple(w.clone()),
            WeakValue::NativeEnum(r) => {
                let base = r.ptr & !7;
                if base != 0 {
                    // SAFETY: a copied weak handle observes the same node; bump
                    // the intrusive weak count so its drop is balanced.
                    unsafe { gossamer_runtime::c_abi::gos_rt_rc_weak_retain(base as *mut u8) };
                }
                WeakValue::NativeEnum(Box::new(NativeEnumWeakRef {
                    ptr: r.ptr,
                    shape: Arc::clone(&r.shape),
                }))
            }
            WeakValue::Dead => WeakValue::Dead,
        }
    }
}

impl Drop for WeakValue {
    fn drop(&mut self) {
        if let WeakValue::NativeEnum(r) = self {
            let base = r.ptr & !7;
            if base != 0 {
                // SAFETY: releasing the weak count this handle took at downgrade
                // / clone; frees the block once strong and weak both reach zero.
                unsafe { gossamer_runtime::c_abi::gos_rt_rc_weak_release(base as *mut u8) };
            }
        }
    }
}

impl WeakValue {
    /// Builds a weak handle from a strong value. Heap variants record a
    /// `std::sync::Weak` to their `Arc`; a native enum takes an intrusive weak
    /// count; everything else is `Dead`.
    #[must_use]
    pub fn downgrade(value: &Value) -> Self {
        match value {
            Value::Variant(a) => WeakValue::Variant(Arc::downgrade(a)),
            Value::Struct(a) => WeakValue::Struct(Arc::downgrade(a)),
            Value::Array(a) => WeakValue::Array(Arc::downgrade(a)),
            Value::Tuple(a) => WeakValue::Tuple(Arc::downgrade(a)),
            Value::NativeEnum(h) => {
                let base = h.ptr & !7;
                if base == 0 {
                    return WeakValue::Dead;
                }
                // SAFETY: bumps the referent's intrusive weak count; the block
                // outlives every strong reference until this weak is released.
                unsafe { gossamer_runtime::c_abi::gos_rt_rc_downgrade(base as *mut u8) };
                WeakValue::NativeEnum(Box::new(NativeEnumWeakRef {
                    ptr: h.ptr,
                    shape: Arc::clone(&h.shape),
                }))
            }
            _ => WeakValue::Dead,
        }
    }

    /// Reconstructs the strong [`Value`] if the referent is still alive.
    #[must_use]
    pub fn upgrade(&self) -> Option<Value> {
        match self {
            WeakValue::Variant(w) => w.upgrade().map(Value::Variant),
            WeakValue::Struct(w) => w.upgrade().map(Value::Struct),
            WeakValue::Array(w) => w.upgrade().map(Value::Array),
            WeakValue::Tuple(w) => w.upgrade().map(Value::Tuple),
            WeakValue::NativeEnum(r) => {
                let base = r.ptr & !7;
                // SAFETY: reading the strong count of a weak-pinned (still
                // allocated) node; > 0 means a strong owner survives.
                if base != 0
                    && unsafe { gossamer_runtime::c_abi::rc_strong_count(base as *mut u8) } > 0
                {
                    // SAFETY: co-owning a live node; the returned borrowed handle
                    // releases this retain once on drop.
                    unsafe { gossamer_runtime::c_abi::gos_rt_rc_retain(base as *mut u8) };
                    Some(Value::NativeEnum(Arc::new(NativeEnumOwner {
                        ptr: r.ptr,
                        shape: Arc::clone(&r.shape),
                        owned: false,
                    })))
                } else {
                    None
                }
            }
            WeakValue::Dead => None,
        }
    }
}

/// A float key's bit pattern. Keys are equal when their bits are, and order
/// by IEEE total order: `-0.0` sorts just below `0.0` and a NaN at either
/// end, the order the compiled tiers' ordered traversals use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FloatBits(pub u64);

impl Ord for FloatBits {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        f64::from_bits(self.0).total_cmp(&f64::from_bits(other.0))
    }
}

impl PartialOrd for FloatBits {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Ordered key type for [`Value::Map`]. Wraps a [`Value`] and
/// gives it a `(tag, content)` total order so any value the user
/// can hash (int / bool / char / string) sorts deterministically.
/// Aggregate values (arrays, structs, closures) collapse to a
/// single bucket - they're rejected at insert time, not here.
///
/// String keys are stored as [`SmolStr`] (8 B inline for ≤ 7-byte
/// keys, otherwise an `Arc<str>` behind a tag bit) instead of an
/// owned `String`. For maps with many short string keys (k-mer
/// counts, tag dictionaries, …) this halves per-key residency
/// and removes one heap allocation per insert.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MapKey {
    /// Sentinel for non-hashable inputs; all equal so their map
    /// degenerates to a single slot. Lets the runtime stay
    /// total even if user code passes an aggregate as a key.
    NonHashable,
    /// `bool` key.
    Bool(bool),
    /// `i64` key (every integer width converges here).
    Int(i64),
    /// A key the renderer reads as unsigned. Only the rendered copy of a map
    /// whose keys were declared `u64` / `usize` holds one, so a key at or
    /// above `i64::MAX` prints as its own decimal; a live map keys by
    /// [`MapKey::Int`] whatever width the source named.
    Uint(u64),
    /// `char` key.
    Char(char),
    /// `f64` key, held as the value's bit pattern so two keys compare and
    /// hash exactly as the compiled tiers' raw eight bytes do, and read back
    /// as the float they spell.
    Float(FloatBits),
    /// String key (stored inline when ≤ 7 bytes - see [`SmolStr`]).
    Str(SmolStr),
    /// Aggregate key - struct / tuple / enum variant - hashed by *value*:
    /// the type/variant name plus each field's `MapKey`, recursively. Two
    /// equal-valued aggregates at distinct allocations produce equal keys, so
    /// `HashMap<Point, _>` keys by content the way the compiled tier does.
    /// Boxed so the rare aggregate-key case does not widen every `MapKey`
    /// (a scalar/string key stays 16 bytes instead of paying for two inline
    /// fat pointers).
    Agg(Box<AggKey>),
}

/// Shape an aggregate map key was built from, so the key can be rebuilt as
/// the value the program wrote rather than an anonymous field list.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AggShape {
    /// A tuple: positional, rebuilt as [`Value::Tuple`].
    Tuple,
    /// An array: positional, rebuilt as [`Value::Array`].
    Array,
    /// An enum variant: positional under its variant name.
    Variant,
    /// A struct, carrying its field names in declaration order.
    Struct(Arc<[&'static str]>),
}

/// Boxed payload of [`MapKey::Agg`]: an aggregate map key hashed by value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AggKey {
    /// An enum variant's declaration position, which is what orders two
    /// values of one enum - the discriminant the compiled tiers compare.
    /// Zero for every other shape, whose name orders it. Ahead of `name` so
    /// the derived ordering reads it first; it is a function of the name, so
    /// equality and hashing are unchanged by carrying it.
    pub rank: i64,
    /// Type / variant name (`""` for a tuple, `"[]"` for an array).
    pub name: TypeTag,
    /// Each field's key, recursively.
    pub fields: Box<[MapKey]>,
    /// How to rebuild the original value from `name` and `fields`.
    pub shape: AggShape,
}

impl MapKey {
    /// Builds a `MapKey` from any `Value`. Aggregates collapse
    /// to `NonHashable`.
    #[must_use]
    pub fn from_value(v: &Value) -> Self {
        match v {
            Value::Bool(b) => Self::Bool(*b),
            Value::Int(n) => Self::Int(*n),
            // A `u64` keys by its bits, the way the compiled tiers hash the
            // slot word, so the same key finds the same entry whichever
            // representation carried it in.
            Value::Uint(n) => Self::Int(*n as i64),
            Value::Char(c) => Self::Char(*c),
            // Key floats by their bit pattern - matches the compiled tier,
            // which hashes the raw 8 bytes.
            Value::Float(f) => Self::Float(FloatBits(f.to_bits())),
            Value::String(s) => Self::Str(s.clone()),
            Value::Tuple(vals) => Self::Agg(Box::new(AggKey {
                rank: 0,
                name: intern_type_tag(""),
                fields: vals.iter().map(Self::from_value).collect(),
                shape: AggShape::Tuple,
            })),
            Value::Array(vals) => Self::Agg(Box::new(AggKey {
                rank: 0,
                name: intern_type_tag("[]"),
                fields: vals.iter().map(Self::from_value).collect(),
                shape: AggShape::Array,
            })),
            Value::IntArray(ns) => Self::Agg(Box::new(AggKey {
                rank: 0,
                name: intern_type_tag("[]"),
                fields: ns.iter().map(|n| Self::Int(*n)).collect(),
                shape: AggShape::Array,
            })),
            Value::ByteArray(bytes) => Self::Agg(Box::new(AggKey {
                rank: 0,
                name: intern_type_tag("[]"),
                fields: bytes.iter().map(|n| Self::Int(i64::from(*n))).collect(),
                shape: AggShape::Array,
            })),
            Value::InlineByteArray(bytes) => Self::Agg(Box::new(AggKey {
                rank: 0,
                name: intern_type_tag("[]"),
                fields: bytes.iter().map(|n| Self::Int(i64::from(*n))).collect(),
                shape: AggShape::Array,
            })),
            Value::ByteVec(bytes) => Self::Agg(Box::new(AggKey {
                rank: 0,
                name: intern_type_tag("[]"),
                fields: bytes.iter().map(|n| Self::Int(i64::from(*n))).collect(),
                shape: AggShape::Array,
            })),
            Value::Struct(inner) => Self::Agg(Box::new(AggKey {
                rank: 0,
                name: inner.name.clone(),
                fields: inner
                    .fields
                    .iter()
                    .map(|(_, fv)| Self::from_value(fv))
                    .collect(),
                shape: AggShape::Struct(inner.fields.field_names()),
            })),
            Value::Variant(inner) => Self::Agg(Box::new(AggKey {
                rank: crate::builtins::variant_rank_of(inner.name.as_str()).unwrap_or(0),
                name: inner.name.clone(),
                fields: inner.fields.iter().map(Self::from_value).collect(),
                shape: AggShape::Variant,
            })),
            // A native enum hashes through its boxed shape so a user enum used
            // as a map key keeps working after Step 8 (VM-built enums are
            // native) and hashes identically to a boxed one of the same value.
            Value::NativeEnum(owner) => Self::from_value(&native_enum_to_variant(owner)),
            _ => Self::NonHashable,
        }
    }

    /// The key a rendered copy stores for `value`: [`Self::from_value`],
    /// except that an unsigned integer anywhere inside stays
    /// [`Self::Uint`], so it reads back as the decimal its type spells.
    /// Only a renderer's private copy holds one.
    #[must_use]
    pub(crate) fn rendered(value: &Value) -> Self {
        match value {
            Value::Uint(n) => Self::Uint(*n),
            Value::Tuple(vals) => Self::Agg(Box::new(AggKey {
                rank: 0,
                name: intern_type_tag(""),
                fields: vals.iter().map(Self::rendered).collect(),
                shape: AggShape::Tuple,
            })),
            Value::Array(vals) => Self::Agg(Box::new(AggKey {
                rank: 0,
                name: intern_type_tag("[]"),
                fields: vals.iter().map(Self::rendered).collect(),
                shape: AggShape::Array,
            })),
            Value::Struct(inner) => Self::Agg(Box::new(AggKey {
                rank: 0,
                name: inner.name.clone(),
                fields: inner
                    .fields
                    .iter()
                    .map(|(_, field)| Self::rendered(field))
                    .collect(),
                shape: AggShape::Struct(inner.fields.field_names()),
            })),
            Value::Variant(inner) => Self::Agg(Box::new(AggKey {
                rank: crate::builtins::variant_rank_of(inner.name.as_str()).unwrap_or(0),
                name: inner.name.clone(),
                fields: inner.fields.iter().map(Self::rendered).collect(),
                shape: AggShape::Variant,
            })),
            other => Self::from_value(other),
        }
    }

    /// Recovers the `Value` shape this key originally held. Used
    /// by `keys()` so iteration returns the user's original type.
    #[must_use]
    pub fn to_value(&self) -> Value {
        match self {
            Self::Bool(b) => Value::Bool(*b),
            Self::Int(n) => Value::Int(*n),
            Self::Uint(n) => Value::Uint(*n),
            Self::Char(c) => Value::Char(*c),
            Self::Float(bits) => Value::Float(f64::from_bits(bits.0)),
            Self::Str(s) => Value::String(s.clone()),
            // An aggregate key retains the shape it was hashed from, so it
            // rebuilds as the value the program wrote.
            Self::Agg(agg) => {
                let fields: Vec<Value> = agg.fields.iter().map(Self::to_value).collect();
                match &agg.shape {
                    AggShape::Tuple => Value::Tuple(Arc::new(fields)),
                    AggShape::Array => Value::Array(Arc::new(fields)),
                    AggShape::Variant => Value::Variant(Arc::new(VariantInner {
                        name: agg.name.clone(),
                        fields: fields.into_iter().collect(),
                    })),
                    AggShape::Struct(names) => Value::Struct(Arc::new(StructInner {
                        name: agg.name.clone(),
                        fields: StructFields::from_parts(Arc::clone(names), fields),
                    })),
                }
            }
            Self::NonHashable => Value::Unit,
        }
    }
}

/// Boxed payload of [`Value::FloatArray`]. Pre-B1 this lived
/// inline in the enum (~48 bytes); behind `Arc` it costs 8 in
/// the variant.
#[derive(Debug, Clone)]
pub struct FloatArrayInner {
    /// Element-struct name (e.g. `"Body"`). Interned via
    /// `intern_type_name` so identical names share a single
    /// `&'static` allocation (~24 B + heap save per aggregate).
    pub name: &'static str,
    /// Number of `f64` fields per element.
    pub stride: u16,
    /// Field names in declaration order.
    pub field_names: Arc<Vec<String>>,
    /// Flat f64 storage. Length equals `stride * elem_count`.
    pub data: Arc<Vec<f64>>,
}

/// Boxed payload of [`Value::Variant`].
#[derive(Debug, Clone)]
pub struct VariantInner {
    /// Variant name (interned, see `intern_type_tag`).
    pub name: TypeTag,
    /// Positional fields stored inline for the common arity (≤ 2):
    /// `Some(x)`, `Ok`/`Err`, and a two-child enum node (linked-list
    /// `Cons`, tree `Node`) keep their payload in the same heap block
    /// as the `Arc<VariantInner>` header - one allocation per value
    /// instead of two, and 64 bytes rather than 80 for a two-field
    /// node (it lands in a smaller `mimalloc` size class). Arity > 2
    /// spills to the heap. Sharing goes through the outer `Arc`.
    pub fields: SmallVec<[Value; 2]>,
}

/// Values for one struct instance plus a shared, program-owned field layout.
/// Field names are interned once per distinct declaration-order shape instead
/// of storing one pointer beside every value in every instance.
#[derive(Debug, Clone, Default)]
pub struct StructFields {
    names: Arc<[&'static str]>,
    values: Box<[Value]>,
}

impl StructFields {
    pub(crate) fn new(fields: Vec<(&'static str, Value)>) -> Self {
        let (names, values): (Vec<_>, Vec<_>) = fields.into_iter().unzip();
        Self {
            names: intern_struct_field_names(&names),
            values: values.into_boxed_slice(),
        }
    }

    /// The interned field-name layout this struct instance shares.
    #[must_use]
    pub fn field_names(&self) -> Arc<[&'static str]> {
        Arc::clone(&self.names)
    }

    /// Rebuilds a struct body from an already-interned name layout and its
    /// values in declaration order.
    #[must_use]
    pub fn from_parts(names: Arc<[&'static str]>, values: Vec<Value>) -> Self {
        Self {
            names,
            values: values.into_boxed_slice(),
        }
    }

    pub(crate) fn from_two_values(
        field0: &'static str,
        value0: Value,
        field1: &'static str,
        value1: Value,
    ) -> Self {
        Self {
            names: intern_struct_field_names_2(field0, field1),
            values: Box::new([value0, value1]),
        }
    }

    /// Returns the number of struct fields.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }
    /// Returns true when this struct has no fields.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
    /// Iterates over field names and values in declaration order.
    pub fn iter(&self) -> impl Iterator<Item = (&&'static str, &Value)> {
        self.names.iter().zip(self.values.iter())
    }
    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = (&&'static str, &mut Value)> {
        self.names.iter().zip(self.values.iter_mut())
    }
    /// Returns the field name and value at `index`.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<(&'static str, &Value)> {
        Some((*self.names.get(index)?, self.values.get(index)?))
    }
    pub(crate) fn get_mut(&mut self, index: usize) -> Option<(&&'static str, &mut Value)> {
        Some((self.names.get(index)?, self.values.get_mut(index)?))
    }
    pub(crate) fn position(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|candidate| *candidate == name)
    }
    pub(crate) fn to_vec(&self) -> Vec<(&'static str, Value)> {
        self.iter()
            .map(|(name, value)| (*name, value.clone()))
            .collect()
    }
    pub(crate) fn into_vec(self) -> Vec<(&'static str, Value)> {
        self.names
            .iter()
            .copied()
            .zip(self.values.into_vec())
            .collect()
    }
    fn into_values(self) -> Box<[Value]> {
        self.values
    }
}

impl std::ops::Index<usize> for StructFields {
    type Output = Value;
    fn index(&self, index: usize) -> &Self::Output {
        &self.values[index]
    }
}

impl std::ops::IndexMut<usize> for StructFields {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        &mut self.values[index]
    }
}

impl<'a> IntoIterator for &'a StructFields {
    type Item = (&'a &'static str, &'a Value);
    type IntoIter = std::iter::Zip<std::slice::Iter<'a, &'static str>, std::slice::Iter<'a, Value>>;
    fn into_iter(self) -> Self::IntoIter {
        self.names.iter().zip(self.values.iter())
    }
}

impl<'a> IntoIterator for &'a mut StructFields {
    type Item = (&'a &'static str, &'a mut Value);
    type IntoIter =
        std::iter::Zip<std::slice::Iter<'a, &'static str>, std::slice::IterMut<'a, Value>>;
    fn into_iter(self) -> Self::IntoIter {
        self.names.iter().zip(self.values.iter_mut())
    }
}

/// Boxed payload of [`Value::Struct`].
#[derive(Debug, Clone)]
pub struct StructInner {
    /// Struct name (interned, see `intern_type_tag`).
    pub name: TypeTag,
    /// Field name/value pairs in declaration order, stored inline. The
    /// field name is an interned `&'static str` (shared across every
    /// instance of the type) rather than an owned `String`, so a struct
    /// instance no longer heap-allocates its field names - for a program
    /// holding millions of structs that removed millions of per-field
    /// allocations and shrank each slot from 40 to 32 bytes. A
    /// `Box<[_]>` (not `Vec`) drops the unused capacity word: a struct's
    /// field count is fixed at construction.
    pub fields: StructFields,
}

/// Boxed payload of [`Value::Builtin`]. Builtins are constructed
/// once at VM init and shared by `Arc`; cloning a `Value::Builtin`
/// is one refcount inc.
#[derive(Debug, Clone)]
pub struct BuiltinInner {
    /// Display name.
    pub name: &'static str,
    /// Implementation pointer.
    pub call: fn(&[Value]) -> RuntimeResult<Value>,
}

/// Boxed payload of [`Value::Native`].
#[derive(Debug, Clone)]
pub struct NativeInner {
    /// Display name.
    pub name: &'static str,
    /// Implementation pointer.
    pub call: NativeCall,
}

impl Value {
    /// Empty `Value::Array(Arc::new(Vec::new()))` shared across
    /// callers. Avoids the per-call 32 B allocation for empty
    /// results.
    #[must_use]
    pub fn empty_array() -> Self {
        Self::Array(empty_value_arc())
    }

    /// Empty `Value::Tuple(Arc::from(Vec::new()))` shared across
    /// callers.
    #[must_use]
    pub fn empty_tuple() -> Self {
        Self::Tuple(empty_value_arc())
    }

    /// Constructs a [`Value::Variant`] from owned name + shared
    /// field list. Hides the `Arc::new(VariantInner { … })`
    /// boilerplate at every constructor site.
    ///
    /// A node whose payload is a single small immutable scalar
    /// (`None`/`Nil`, `Some(0)`, an enum leaf like `Num(7)`) is shared
    /// from a thread-local cache instead of allocated fresh - the
    /// interpreter analog of the `CPython` small-int cache. Variant fields
    /// are never mutated in place (no `Arc::make_mut` site touches a
    /// `VariantInner`), so the shared node is immutable and safe to
    /// alias. The cache is thread-local rather than a global table, so
    /// there is no lock contention across per-connection VM threads.
    #[must_use]
    pub fn variant(name: impl AsRef<str>, fields: Vec<Value>) -> Self {
        let name = intern_type_tag(name.as_ref());
        // Keep bytecode-VM enum construction in the compact boxed
        // representation. Earlier builds eagerly converted any variant with a
        // registered native enum shape into the compiled-tier RC layout here.
        // That made pure interpretation pay a native handle allocation plus an
        // `Arc<NativeEnumOwner>` for every recursive tree / JSON-DOM node; the
        // stress `ast-rewrite` and `json-serde` benchmarks ballooned into
        // multi-GB RSS. Native representation is still built lazily at the JIT
        // boundary by `jit_call::build_variant_to_native_enum`, where it is
        // actually needed.
        Self::variant_with_tag(name, fields)
    }

    /// Constructs a [`Value::Variant`] from an already-interned variant tag.
    ///
    /// Bytecode enum-constructor dispatch already holds this tag in the
    /// callee sentinel. Reusing it avoids a global intern-table lookup per
    /// constructed node in recursive enum workloads.
    #[must_use]
    pub(crate) fn variant_with_tag(name: TypeTag, fields: Vec<Value>) -> Self {
        variant_with_tag_and_fields(name, variant_fields(fields))
    }

    /// Constructs a one-field variant without an intermediate argument buffer.
    #[must_use]
    pub(crate) fn variant_with_tag_1(name: TypeTag, field: Value) -> Self {
        let mut fields = SmallVec::new();
        fields.push(field);
        variant_with_tag_and_fields(name, fields)
    }

    /// Constructs a two-field variant without an intermediate argument buffer.
    #[must_use]
    pub(crate) fn variant_with_tag_2(name: TypeTag, first: Value, second: Value) -> Self {
        variant_with_tag_and_fields(name, SmallVec::from_buf([first, second]))
    }

    /// Constructs the boxed `Variant` representation unconditionally, never the
    /// native form. Required where a genuine `Variant` is the contract - most
    /// importantly `native_enum_to_variant`, which converts a native handle to
    /// the boxed form for equality / display / serde; routing it back through
    /// [`Value::variant`] would rebuild a native handle and loop forever.
    #[must_use]
    pub(crate) fn variant_boxed(name: &'static str, fields: Vec<Value>) -> Self {
        let name = type_tag_from_static(name);
        Self::variant_with_tag(name, fields)
    }
    /// Constructs a [`Value::Struct`].
    #[must_use]
    pub fn struct_(name: impl AsRef<str>, fields: Vec<(&'static str, Value)>) -> Self {
        Self::struct_with_tag(intern_type_tag(name.as_ref()), fields)
    }

    /// Constructs a [`Value::Struct`] from an already-interned type tag.
    ///
    /// Positional constructors keep their zero-field sentinel in the global
    /// table, so cloning that tag avoids taking the global type-tag lock for
    /// every aggregate built in a hot loop.
    #[must_use]
    pub(crate) fn struct_with_tag(name: TypeTag, fields: Vec<(&'static str, Value)>) -> Self {
        Self::Struct(Arc::new(StructInner {
            name,
            fields: StructFields::new(fields),
        }))
    }

    /// Constructs a two-field integer struct without a temporary argument
    /// vector. Used by the bytecode VM's typed positional-constructor opcode.
    #[must_use]
    pub(crate) fn struct_2_i64(
        name: &'static str,
        field0: &'static str,
        first: i64,
        field1: &'static str,
        second: i64,
    ) -> Self {
        Self::Struct(Arc::new(StructInner {
            name: type_tag_from_static(name),
            fields: StructFields::from_two_values(
                field0,
                Self::Int(first),
                field1,
                Self::Int(second),
            ),
        }))
    }
    /// Constructs a [`Value::FloatArray`].
    #[must_use]
    pub fn float_array(
        name: impl AsRef<str>,
        stride: u16,
        field_names: Arc<Vec<String>>,
        data: Arc<Vec<f64>>,
    ) -> Self {
        Self::FloatArray(Arc::new(FloatArrayInner {
            name: intern_type_name(name.as_ref()),
            stride,
            field_names,
            data,
        }))
    }
    /// Constructs a [`Value::Builtin`].
    #[must_use]
    pub fn builtin(name: &'static str, call: fn(&[Value]) -> RuntimeResult<Value>) -> Self {
        Self::Builtin(Arc::new(BuiltinInner { name, call }))
    }
    /// Constructs a [`Value::Native`].
    #[must_use]
    pub fn native(name: &'static str, call: NativeCall) -> Self {
        Self::Native(Arc::new(NativeInner { name, call }))
    }
}
/// Callback handed to [`Value::Native`] builtins. Exposes the subset
/// of the interpreter needed to dispatch back into Gossamer code.
pub trait NativeDispatch {
    /// Invokes a top-level function by name with the given arguments.
    fn call_fn(&mut self, name: &str, args: Vec<Value>) -> RuntimeResult<Value>;
    /// Whether a top-level function of this name exists, so a builtin can
    /// choose a dispatch without provoking a missing-name error.
    fn has_fn(&self, name: &str) -> bool;
    /// Invokes an arbitrary callable [`Value`]: builtin, native, or
    /// closure. Used by higher-order native builtins (e.g.
    /// `Option::map`) that receive a Gossamer closure as an argument.
    fn call_value(&mut self, callee: &Value, args: Vec<Value>) -> RuntimeResult<Value>;
    /// Spawns `callable` in a fresh worker thread with the supplied
    /// arguments. A panic in the spawned callable is isolated to the
    /// worker and does not propagate to the caller.
    fn spawn_callable(&mut self, callable: Value, args: Vec<Value>) -> RuntimeResult<()>;
    /// Spawns `callable` and returns a one-shot channel handle that
    /// `.join()` blocks on for the outcome (`Ok(value)`, or
    /// `Err(message)` if the callable panicked). Backs `spawn(f)`.
    fn spawn_join(&mut self, callable: Value, args: Vec<Value>) -> RuntimeResult<Value>;
    /// [`Self::spawn_join`] carrying the spawn's `reason:` label, which the
    /// cohort names the child by in its enumeration and drain reports.
    /// Defaults to the unlabelled form, so an implementor that does not
    /// track cohorts needs no change.
    fn spawn_join_labelled(
        &mut self,
        callable: Value,
        args: Vec<Value>,
        reason: String,
    ) -> RuntimeResult<Value> {
        let _ = reason;
        self.spawn_join(callable, args)
    }
    /// Spawns `target(args)` on a goroutine and hands the outcome to
    /// `sink` on that goroutine. A native server answers each request on
    /// its own goroutine, so its accept loop stays free while a handler
    /// runs and requests are served concurrently.
    fn spawn_with_outcome(
        &mut self,
        target: SpawnTarget,
        args: Vec<Value>,
        sink: Box<dyn FnOnce(RuntimeResult<Value>) + Send>,
    );
    /// Runs `task` on a pool worker with a dispatch of its own, for a builtin
    /// that spreads its work over several workers. Answers whether the task
    /// was queued; an implementor with no pool answers `false`, and the
    /// builtin does that share of the work itself.
    fn spawn_task(&mut self, task: NativeTask) -> bool {
        let _ = task;
        false
    }
}

/// A builtin's share of work, run on a pool worker with a dispatch of its own.
pub type NativeTask = Box<dyn FnOnce(&mut dyn NativeDispatch) + Send>;

/// What a spawned dispatch invokes.
#[derive(Debug, Clone)]
pub enum SpawnTarget {
    /// A top-level function, resolved on the goroutine that runs it.
    Named(String),
    /// A closure, builtin, or other callable value.
    Callable(Value),
}

impl SpawnTarget {
    /// What serving one request through `handler` calls, and the arguments
    /// that precede the request.
    ///
    /// A `Router`, a middleware chain, or any struct answers through its
    /// `T::serve` impl - named by struct, because the bare `serve` global is
    /// overwritten as each impl loads, and taking the handler as its
    /// receiver. A plain function or a closure IS the handler and takes the
    /// request as its only argument.
    #[must_use]
    pub fn for_handler(handler: &Value) -> (Self, Vec<Value>) {
        match handler {
            Value::Struct(inner) => (
                Self::Named(format!("{}::serve", inner.name)),
                vec![handler.clone()],
            ),
            other => (Self::Callable(other.clone()), Vec::new()),
        }
    }
}

/// Invokes `handler` for one request on the calling goroutine.
///
/// Follows [`SpawnTarget::for_handler`]'s rule for which callable a handler
/// value names.
pub fn dispatch_request(
    dispatch: &mut dyn NativeDispatch,
    handler: &Value,
    request: Value,
) -> RuntimeResult<Value> {
    let (target, mut args) = SpawnTarget::for_handler(handler);
    args.push(request);
    match target {
        SpawnTarget::Named(name) => dispatch.call_fn(&name, args),
        SpawnTarget::Callable(callee) => dispatch.call_value(&callee, args),
    }
}

/// Function pointer for [`Value::Native`] builtins.
pub type NativeCall = fn(&mut dyn NativeDispatch, &[Value]) -> RuntimeResult<Value>;

impl Value {
    /// Returns the unit value.
    #[must_use]
    pub const fn unit() -> Self {
        Self::Unit
    }

    /// Returns `true` when this value is `true` in boolean contexts.
    #[must_use]
    pub const fn is_truthy(&self) -> bool {
        matches!(self, Self::Bool(true))
    }

    /// Rehydrates a [`Value::FloatArray`] into the boxed
    /// [`Value::Array`] of [`Value::Struct`] representation.
    /// Used at every code path where a flat aggregate meets
    /// code that expects the generic shape - ABI crossings,
    /// `EvalDeferred`, `Display`, etc.
    ///
    /// # Panics
    ///
    /// Panics if `self` is not a [`Value::FloatArray`].
    #[must_use]
    pub fn float_array_to_value_array(&self) -> Value {
        let Self::FloatArray(inner) = self else {
            panic!("float_array_to_value_array: not a FloatArray");
        };
        let stride = inner.stride as usize;
        let elem_count = inner.data.len().checked_div(stride).unwrap_or(0);
        let mut out = Vec::with_capacity(elem_count);
        for i in 0..elem_count {
            let base = i * stride;
            let mut fields: Vec<(&'static str, Value)> =
                Vec::with_capacity(inner.field_names.len());
            for (j, fname) in inner.field_names.iter().enumerate() {
                fields.push((
                    crate::value::intern_type_name(fname.as_str()),
                    Value::Float(inner.data[base + j]),
                ));
            }
            out.push(Value::struct_(
                inner.name,
                Arc::unwrap_or_clone(Arc::new(fields)),
            ));
        }
        Value::Array(Arc::new(out))
    }

    /// Convenience wrapper that returns the rehydrated element
    /// vector of a [`Value::FloatArray`] so callers that just
    /// need to iterate struct elements don't have to match the
    /// outer [`Value::Array`].
    #[must_use]
    pub fn float_array_elems(&self) -> Vec<Value> {
        let Value::Array(a) = self.float_array_to_value_array() else {
            unreachable!()
        };
        a.as_ref().clone()
    }

    /// Serialises `self` into the canonical `u64` value layout.
    ///
    /// Inline scalars encode directly; heap objects are stored in the
    /// global side table and the returned word carries their handle.
    #[must_use]
    pub fn to_raw(&self) -> GossamerValue {
        match self {
            Self::NativeEnum(o) => native_enum_to_variant(o).to_raw(),
            // Write-back cells never escape the call protocol; if a
            // boundary serialises one anyway, its current inner value
            // is the only meaningful payload.
            Self::MutCell(c) => c.lock().to_raw(),
            Self::CaptureCell(c) => c.lock().to_raw(),
            Self::Unit => from_singleton(SINGLETON_UNIT),
            Self::Bool(false) => from_singleton(SINGLETON_FALSE),
            Self::Bool(true) => from_singleton(SINGLETON_TRUE),
            Self::Int(n) => {
                if fits_i56(*n) {
                    from_i64(*n)
                } else {
                    let id = register_heap(RegistryEntry::Int(*n));
                    from_heap_handle(id)
                }
            }
            Self::Float(f) => from_f64(*f),
            Self::Char(c) => {
                let payload = ((*c as u64) << 2) | 3;
                from_singleton(payload)
            }
            Self::String(s) => {
                // Preserve the VM string allocation in the raw side table.
                // `SmolStr::clone` is a refcount bump for heap strings and a
                // word copy for inline strings, so this boundary neither
                // materialises an `Arc<String>` nor copies its bytes.
                let id = register_heap(RegistryEntry::String(s.clone()));
                from_heap_handle(id)
            }
            // The compact raw ABI has no JSON-tree representation. JSON
            // values are intentionally interpreter-local; callers crossing
            // this boundary receive the same sentinel as other opaque values.
            Self::Json(_) => from_singleton(SINGLETON_UNIT),
            Self::Tuple(t) => {
                let id = register_heap(RegistryEntry::Tuple(Arc::clone(t)));
                from_heap_handle(id)
            }
            Self::Array(a) => {
                let id = register_heap(RegistryEntry::Array(Arc::clone(a)));
                from_heap_handle(id)
            }
            Self::FloatArray(data) => {
                // The tagged word only names a VM-owned side-table entry, so
                // retain the typed storage instead of rehydrating every
                // element into a boxed array at the JIT boundary.
                let id = register_heap(RegistryEntry::FloatArray(Arc::clone(data)));
                from_heap_handle(id)
            }
            Self::IntArray(data) => {
                // Keep the compact typed storage shared across the boundary.
                let id = register_heap(RegistryEntry::IntArray(Arc::clone(data)));
                from_heap_handle(id)
            }
            Self::ByteArray(data) => {
                let id = register_heap(RegistryEntry::ByteArray(Arc::clone(data)));
                from_heap_handle(id)
            }
            Self::InlineByteArray(data) => {
                let id = register_heap(RegistryEntry::InlineByteArray(Arc::clone(data)));
                from_heap_handle(id)
            }
            Self::ByteVec(data) => {
                let id = register_heap(RegistryEntry::ByteVec(Arc::clone(data)));
                from_heap_handle(id)
            }
            Self::FloatVec(data) => {
                // Keep the compact typed storage shared across the boundary.
                let id = register_heap(RegistryEntry::FloatVec(Arc::clone(data)));
                from_heap_handle(id)
            }
            Self::Variant(inner) => {
                let id = register_heap(RegistryEntry::Variant(Arc::clone(inner)));
                from_heap_handle(id)
            }
            Self::Struct(inner) => {
                let id = register_heap(RegistryEntry::Struct(Arc::clone(inner)));
                from_heap_handle(id)
            }
            Self::Closure(c) => {
                let id = register_heap(RegistryEntry::Closure(Arc::clone(c)));
                from_heap_handle(id)
            }
            Self::Channel(ch) => {
                let id = register_heap(RegistryEntry::Channel(ch.clone()));
                from_heap_handle(id)
            }
            Self::Uint(n) => {
                let n_i = *n as i64;
                if fits_i56(n_i) {
                    from_i64(n_i)
                } else {
                    let id = register_heap(RegistryEntry::Int(n_i));
                    from_heap_handle(id)
                }
            }
            Self::Map(_)
            | Self::IntMap(_)
            | Self::StrIntMap(_)
            | Self::LazyIter(_)
            | Self::Builtin(_)
            | Self::Native(_)
            | Self::Weak(_)
            | Self::Void => {
                // Unencodable in the raw layout - return a sentinel
                // that `from_raw` maps back to `Void`.
                from_singleton(SINGLETON_UNIT)
            }
        }
    }

    /// Deserialises a [`GossamerValue`] into the interpreter's
    /// convenience wrapper.  The inverse of [`Self::to_raw`].
    #[must_use]
    pub fn from_raw(raw: GossamerValue) -> Self {
        match tag_of(raw) {
            TAG_IMMEDIATE => Self::Int(to_i64(raw)),
            TAG_FLOAT => Self::Float(to_f64(raw)),
            TAG_SINGLETON => {
                let disc = to_singleton(raw);
                match disc {
                    SINGLETON_UNIT => Self::Unit,
                    SINGLETON_FALSE => Self::Bool(false),
                    SINGLETON_TRUE => Self::Bool(true),
                    _ => {
                        let low = disc & 3;
                        if low == 3 {
                            let codepoint = (disc >> 2) as u32;
                            Self::Char(char::from_u32(codepoint).unwrap_or('\0'))
                        } else {
                            Self::Void
                        }
                    }
                }
            }
            TAG_HEAP => {
                let id = to_heap_handle(raw);
                match take_heap(id) {
                    Some(RegistryEntry::Int(n)) => Self::Int(n),
                    Some(RegistryEntry::String(s)) => Self::String(s),
                    Some(RegistryEntry::Tuple(t)) => Self::Tuple(t),
                    Some(RegistryEntry::Array(a)) => Self::Array(a),
                    Some(RegistryEntry::FloatArray(a)) => Self::FloatArray(a),
                    Some(RegistryEntry::IntArray(a)) => Self::IntArray(a),
                    Some(RegistryEntry::ByteArray(a)) => Self::ByteArray(a),
                    Some(RegistryEntry::InlineByteArray(a)) => Self::InlineByteArray(a),
                    Some(RegistryEntry::ByteVec(a)) => Self::ByteVec(a),
                    Some(RegistryEntry::FloatVec(a)) => Self::FloatVec(a),
                    Some(RegistryEntry::Variant(inner)) => Self::Variant(inner),
                    Some(RegistryEntry::Struct(inner)) => Self::Struct(inner),
                    Some(RegistryEntry::Closure(c)) => Self::Closure(c),
                    Some(RegistryEntry::Channel(ch)) => Self::Channel(ch),
                    None => Self::Void,
                }
            }
            _ => Self::Void,
        }
    }
}

/// Concrete closure representation.
///
/// The bytecode VM compiles the closure body to its own `FnChunk`
/// whose leading parameters are the captured upvalues, followed by the
/// declared parameters. [`Self::chunk`] holds that body and
/// [`Self::capture_values`] the snapshotted upvalue `Value`s; the VM
/// invokes the closure by running the chunk with `capture_values ++
/// args` in the leading registers. The chunk's `arity` minus
/// `capture_values.len()` is the closure's declared parameter count.
#[derive(Debug, Clone)]
pub struct Closure {
    /// Native bytecode body run by the VM. Its leading registers hold
    /// the captured upvalues, then the declared parameters.
    pub chunk: Arc<crate::bytecode::FnChunk>,
    /// Upvalue snapshot, positionally aligned with the chunk's leading
    /// parameters. Scalars are by-value snapshots; aggregates share
    /// their `Arc` backing, so a mutation through the closure is visible
    /// to the original binding.
    pub capture_values: Vec<Value>,
}

/// Replaces any `MutCell` argument with a clone of its inner value.
/// Builtins and natives have no parameter table, so they receive the
/// plain aggregate; user functions keep the cell for write-back.
pub(crate) fn unwrap_mut_cells(mut args: Vec<Value>) -> Vec<Value> {
    for arg in &mut args {
        if let Value::MutCell(cell) = arg {
            let inner = cell.lock().clone();
            *arg = inner;
        }
    }
    args
}

/// Same as [`unwrap_mut_cells`], except for the builtins whose contract
/// is to write through a `&mut` argument rather than answer a new value.
/// Those receive the cell itself, so the mutation the compiled tiers make
/// through the caller's pointer is the mutation the VM makes too.
pub(crate) fn unwrap_mut_cells_unless_writer(name: &str, args: Vec<Value>) -> Vec<Value> {
    if builtin_writes_through_mut_ref(name) {
        args
    } else {
        unwrap_mut_cells(args)
    }
}

/// Whether the builtin `name` mutates through a `&mut` parameter.
///
/// Every other builtin takes aggregates by value, so a write-back cell is
/// unwrapped before the call and the unchanged value flows back to the
/// caller afterwards.
pub(crate) fn builtin_writes_through_mut_ref(name: &str) -> bool {
    let short = name.rsplit("::").next().unwrap_or(name);
    matches!(
        short,
        "put_u16_be_at"
            | "put_u16_le_at"
            | "put_u32_be_at"
            | "put_u32_le_at"
            | "put_u64_be_at"
            | "put_u64_le_at"
    )
}

/// Result type used throughout the interpreter for operations that can
/// abort with a runtime error.
pub type RuntimeResult<T> = Result<T, RuntimeError>;

/// Top-level interpreter errors. Each variant carries a stable
/// diagnostic code (`GX0001` …) that both the interpreter and the
/// native backend use when reporting the same failure.
#[derive(Debug, Clone, thiserror::Error)]
pub enum RuntimeError {
    /// An operation was applied to a value of the wrong kind.
    #[error("error[GX0001]: type error: {0}")]
    Type(String),
    /// A name lookup failed when interpreting a path expression.
    #[error("error[GX0002]: name `{0}` is not bound in this scope")]
    UnresolvedName(String),
    /// A call site supplied the wrong number of arguments.
    #[error("error[GX0003]: wrong number of arguments: expected {expected}, found {found}")]
    Arity {
        /// Declared arity.
        expected: usize,
        /// Supplied argument count.
        found: usize,
    },
    /// A numeric conversion or a checked runtime bounds operation failed.
    #[error("error[GX0004]: arithmetic error: {0}")]
    Arithmetic(String),
    /// `panic!(...)` invoked from user code or an exhausted match.
    #[error("error[GX0005]: panic: {0}")]
    Panic(String),
    /// A `match` expression failed to match any arm.
    #[error("error[GX0006]: no match for scrutinee at runtime")]
    MatchFailure,
    /// An unimplemented construct was reached while walking the tree.
    #[error("error[GX0007]: interpreter does not yet support {0}")]
    Unsupported(&'static str),
    /// Goroutine call depth exceeded the VM limit.
    #[error(
        "error[GX0008]: stack overflow - recursion exceeded the available stack (call depth {0})"
    )]
    StackOverflow(usize),
    /// Execution budget exhausted (the playground caps loop iterations so an
    /// unbounded loop fails cleanly instead of hanging). Only exists under the
    /// `fuel` feature; native `gos` has no budget and never raises it.
    #[cfg(feature = "fuel")]
    #[error("error[GX0009]: execution limit reached - the program ran too long")]
    FuelExhausted,
    /// A compile-time region reached a capability its `--comptime-io`
    /// level withholds. Carries the rendered denial, which names the
    /// builtin, the capability class, and the option that permits it.
    #[error("{0}")]
    ComptimeDenied(String),
    /// A wait whose target can never arrive on this build. The browser
    /// playground runs one thread and settles every goroutine at its
    /// spawn, so a rendezvous with a goroutine that has already finished
    /// - or with one that can never start - has nothing to wake it.
    #[error(
        "error[GX0011]: {0} would wait for a goroutine this build cannot run \
         concurrently - the browser playground settles each goroutine at its spawn"
    )]
    WouldNeverWake(&'static str),
    /// The program called `process::exit` on a build with no process of
    /// its own to end. Carries the status the program asked for, which
    /// the host reports in place of ending anything.
    #[error("error[GX0012]: the program exited with status {0}")]
    Exit(i32),
}

impl RuntimeError {
    /// Returns the stable `GXNNNN` diagnostic code for this runtime
    /// error. The code is the same in every execution path and is
    /// rendered by `gos explain` for long-form help.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Type(_) => "GX0001",
            Self::UnresolvedName(_) => "GX0002",
            Self::Arity { .. } => "GX0003",
            Self::Arithmetic(_) => "GX0004",
            Self::Panic(_) => "GX0005",
            Self::MatchFailure => "GX0006",
            Self::Unsupported(_) => "GX0007",
            Self::StackOverflow(_) => "GX0008",
            #[cfg(feature = "fuel")]
            Self::FuelExhausted => "GX0009",
            Self::ComptimeDenied(_) => "GX0010",
            Self::WouldNeverWake(_) => "GX0011",
            Self::Exit(_) => "GX0012",
        }
    }
}

#[cfg(test)]
mod mapkey_size_tests {
    use super::MapKey;
    // 0.18.1: boxing the rare aggregate-key arm keeps the common
    // scalar/string keys at 16 bytes instead of 40.
    #[test]
    fn mapkey_is_two_words() {
        assert_eq!(std::mem::size_of::<MapKey>(), 16);
    }
}

#[cfg(test)]
mod thread_confined_cell_tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Arc;

    use super::{ThreadConfinedCell, Value};

    #[test]
    fn thread_confined_cell_rejects_foreign_access_before_deref() {
        let cell = Arc::new(ThreadConfinedCell::new(Value::Int(7)));
        assert!(matches!(&*cell.lock(), Value::Int(7)));

        let foreign = Arc::clone(&cell);
        let rejected = std::thread::spawn(move || {
            catch_unwind(AssertUnwindSafe(|| {
                let _guard = foreign.lock();
            }))
            .is_err()
        })
        .join()
        .expect("foreign thread did not panic");
        assert!(rejected, "foreign access must not reach the UnsafeCell");
    }
}

#[cfg(test)]
mod deep_drop_tests {
    use super::{StructFields, StructInner, Value, VariantInner, intern_type_tag};
    use smallvec::SmallVec;
    use std::sync::Arc;

    // A chain far deeper than the native stack could hold recursive drop
    // frames. The structures are built iteratively (construction was never the
    // problem) and dropped at the end of each test; before the iterative
    // teardown these drops overflowed the default test-thread stack.
    const DEPTH: usize = 1_000_000;

    #[test]
    fn deep_variant_chain_drops_without_stack_overflow() {
        let mut v = Value::Variant(Arc::new(VariantInner {
            name: intern_type_tag("Nil"),
            fields: SmallVec::new(),
        }));
        for _ in 0..DEPTH {
            let mut fields: SmallVec<[Value; 2]> = SmallVec::new();
            fields.push(Value::Int(0));
            fields.push(v);
            v = Value::Variant(Arc::new(VariantInner {
                name: intern_type_tag("Cons"),
                fields,
            }));
        }
        drop(v);
    }

    #[test]
    fn deep_struct_chain_drops_without_stack_overflow() {
        let mut v = Value::Unit;
        for _ in 0..DEPTH {
            let fields: Box<[(&'static str, Value)]> = Box::new([("next", v)]);
            v = Value::Struct(Arc::new(StructInner {
                name: intern_type_tag("Link"),
                fields: StructFields::new(fields.into_vec()),
            }));
        }
        drop(v);
    }

    #[test]
    fn deep_array_nested_in_variant_drops_iteratively() {
        // A Variant whose child is an Array whose child is a Variant ...: the
        // mixed chain must flatten through the worklist once teardown enters
        // via the Variant payload.
        let mut v = Value::Unit;
        for _ in 0..DEPTH {
            let arr = Value::Array(Arc::new(vec![v]));
            let mut fields: SmallVec<[Value; 2]> = SmallVec::new();
            fields.push(arr);
            v = Value::Variant(Arc::new(VariantInner {
                name: intern_type_tag("Wrap"),
                fields,
            }));
        }
        drop(v);
    }
}
