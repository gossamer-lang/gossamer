//! Runtime value representation shared by the bytecode VM and focused
//! interpreter compatibility helpers.
//! Every shared aggregate is backed by [`Arc`] rather than
//! [`std::rc::Rc`] so a [`Value`] can cross thread boundaries - a
//! prerequisite for real goroutine parallelism per
//! the risks backlog.
//! Phase P1 introduces `to_raw` / `from_raw` so that the interpreter
//! and the native backend agree on a single `u64` value layout.
//! Heap objects are registered in a global side table and addressed
//! by `u32` handles; later phases will replace the `Arc` internals
//! with the GC heap directly.

// `SmolStr` (B2) does tagged-pointer arithmetic to keep
// `Value::String` at 8 bytes inline. The unsafe is confined to
// the few methods on `SmolStr`; everything else in the crate
// keeps the safe-Rust discipline.
#![allow(unsafe_code)]

use std::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use std::borrow::Cow;
use std::cell::{Cell, UnsafeCell};
use std::collections::VecDeque;
use std::fmt;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use parking_lot::Mutex;
use smallvec::SmallVec;

use gossamer_runtime::{
    GossamerValue, SINGLETON_FALSE, SINGLETON_TRUE, SINGLETON_UNIT, TAG_FLOAT, TAG_HEAP,
    TAG_IMMEDIATE, TAG_SINGLETON, fits_i56, from_f64, from_heap_handle, from_i64, from_singleton,
    tag_of, to_f64, to_heap_handle, to_i64, to_singleton,
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
    Map(Arc<parking_lot::Mutex<DenseMap<MapKey, Value>>>),
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
                    && unsafe { gossamer_runtime::c_abi::gos_rt_rc_strong_count(base as *mut u8) }
                        > 0
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
    Float(u64),
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
            Value::Float(f) => Self::Float(f.to_bits()),
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
            Self::Float(bits) => Value::Float(f64::from_bits(*bits)),
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

/// Tagged-pointer string with 7-byte inline storage (B2).
///
/// **Encoding.** A single 8-byte word `raw`. The high bit
/// distinguishes inline from heap:
/// - `raw >> 63 == 0`: inline. The low 7 bytes hold UTF-8 content
///   (little-endian); the eighth byte (byte index 7, the high
///   byte) holds the length in `0..=7`.
/// - `raw >> 63 == 1`: heap. The low 63 bits hold a pointer
///   produced by the thin RC byte-buffer allocator. On
///   `x86_64` / aarch64, user-space pointers fit in 48 bits, so
///   masking the high bit is lossless.
///
/// **Why this matters.** Without SSO, every `Value::String(SmolStr::from("Ok"))`
/// allocates a `String` on the heap *and* an `Arc` header (~32
/// bytes total). Variant names like `"Ok"` / `"Err"` / `"Some"`
/// / `"None"`, single-char field names, and most stack tags fit
/// in 7 bytes - so a steady-state hot loop now does zero heap
/// allocation for those values.
///
/// **Safety.** All pointer arithmetic is contained in this type.
/// `Drop` and `Clone` decrement / increment the underlying heap string
/// only when the heap tag is set; inline values are pure `u64`
/// values that don't own anything. The unsafe block in
/// `as_str` casts the storage to `&[u8]`; the bytes are
/// guaranteed UTF-8 because `from_str` only stores valid UTF-8
/// inline.
pub struct SmolStr {
    raw: u64,
}

const SMOL_HEAP_TAG: u64 = 1u64 << 63;
const SMOL_PTR_MASK: u64 = !SMOL_HEAP_TAG;
const SMOL_INLINE_MAX: usize = 7;

#[repr(C)]
struct HeapSmolStr {
    strong: AtomicU32,
    len: u32,
    cap: u32,
    char_len: u32,
    char_index: Vec<u32>,
}

const SMOL_CHAR_INDEX_STRIDE: usize = 32;

fn smol_char_index(s: &str) -> Vec<u32> {
    s.char_indices()
        .enumerate()
        .filter_map(|(char_index, (byte_index, _))| {
            (char_index % SMOL_CHAR_INDEX_STRIDE == 0).then_some(byte_index as u32)
        })
        .collect()
}

impl HeapSmolStr {
    fn layout(cap: usize) -> Layout {
        let header = Layout::new::<Self>();
        let bytes = Layout::array::<u8>(cap).expect("SmolStr heap layout overflow");
        header
            .extend(bytes)
            .expect("SmolStr heap layout overflow")
            .0
            .pad_to_align()
    }

    fn alloc_with_capacity(bytes: &[u8], cap: usize) -> *const Self {
        debug_assert!(cap >= bytes.len());
        Self::alloc_with_fill(bytes.len(), cap, |dst| {
            // SAFETY: `alloc_with_fill` passes `bytes.len()` writable payload
            // bytes; the fresh allocation cannot overlap the source slice.
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
            }
        })
    }

    fn alloc_ascii_upper(bytes: &[u8]) -> *const Self {
        Self::alloc_with_fill(bytes.len(), bytes.len(), |dst| {
            for (i, &b) in bytes.iter().enumerate() {
                let upper = if b.is_ascii_lowercase() {
                    b - (b'a' - b'A')
                } else {
                    b
                };
                // SAFETY: `alloc_with_fill` passes `bytes.len()` writable
                // payload bytes and this loop writes each byte exactly once.
                unsafe {
                    *dst.add(i) = upper;
                }
            }
        })
    }

    #[allow(
        clippy::cast_ptr_alignment,
        reason = "alloc uses HeapSmolStr::layout, whose alignment is at least HeapSmolStr's alignment"
    )]
    fn alloc_with_fill<F>(len: usize, cap: usize, fill: F) -> *const Self
    where
        F: FnOnce(*mut u8),
    {
        let len_u32 = u32::try_from(len).expect("SmolStr heap string too large");
        let cap_u32 = u32::try_from(cap).expect("SmolStr heap string too large");
        let layout = Self::layout(cap);
        // SAFETY: `layout` is non-zero and was computed for the header +
        // payload.
        let ptr = unsafe { alloc(layout) };
        if ptr.is_null() {
            handle_alloc_error(layout);
        }
        let header = ptr.cast::<Self>();
        // SAFETY: `header` points to a fresh allocation large enough for the
        // header plus `len` payload bytes.
        unsafe {
            header.write(Self {
                strong: AtomicU32::new(1),
                len: len_u32,
                cap: cap_u32,
                char_len: 0,
                char_index: Vec::new(),
            });
            fill(Self::bytes_mut(header));
            let text = std::str::from_utf8_unchecked(std::slice::from_raw_parts(
                Self::bytes_ptr(header),
                len,
            ));
            (*header).char_len = text.chars().count() as u32;
            (*header).char_index = smol_char_index(text);
        }
        header
    }

    unsafe fn bytes_ptr(header: *const Self) -> *const u8 {
        // SAFETY: caller guarantees `header` points to a valid `HeapSmolStr`.
        unsafe { header.cast::<u8>().add(std::mem::size_of::<Self>()) }
    }

    unsafe fn bytes_mut(header: *mut Self) -> *mut u8 {
        // SAFETY: caller guarantees `header` points to a valid mutable allocation.
        unsafe { header.cast::<u8>().add(std::mem::size_of::<Self>()) }
    }

    unsafe fn as_str<'a>(header: *const Self) -> &'a str {
        // SAFETY: caller guarantees `header` is live. Payload bytes came
        // from a `str`/`String`, so they are valid UTF-8.
        let len = unsafe { (*header).len as usize };
        let bytes = unsafe { std::slice::from_raw_parts(Self::bytes_ptr(header), len) };
        unsafe { std::str::from_utf8_unchecked(bytes) }
    }

    unsafe fn inc(header: *const Self) {
        // SAFETY: caller owns a live strong reference to `header`.
        let prev = unsafe { (*header).strong.fetch_add(1, Ordering::Relaxed) };
        assert!(prev != u32::MAX, "SmolStr refcount overflow");
    }

    unsafe fn is_unique(header: *const Self) -> bool {
        // SAFETY: caller owns a live strong reference to `header`.
        unsafe { (*header).strong.load(Ordering::Acquire) == 1 }
    }

    unsafe fn append_unique(header: *mut Self, bytes: &[u8]) {
        // SAFETY: caller guarantees unique ownership and enough capacity.
        let len = unsafe { (*header).len as usize };
        let cap = unsafe { (*header).cap as usize };
        debug_assert!(len + bytes.len() <= cap);
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                Self::bytes_mut(header).add(len),
                bytes.len(),
            );
            (*header).len =
                u32::try_from(len + bytes.len()).expect("SmolStr heap string too large");
            let suffix = std::str::from_utf8_unchecked(bytes);
            let old_char_len = (*header).char_len as usize;
            let mut added_chars = 0usize;
            for (byte_offset, _) in suffix.char_indices() {
                let char_index = old_char_len + added_chars;
                if char_index.is_multiple_of(SMOL_CHAR_INDEX_STRIDE) {
                    (*header)
                        .char_index
                        .push(u32::try_from(len + byte_offset).expect("string too large"));
                }
                added_chars += 1;
            }
            (*header).char_len =
                u32::try_from(old_char_len + added_chars).expect("SmolStr heap string too large");
        }
    }

    unsafe fn dec(header: *const Self) {
        // SAFETY: caller owns one strong reference to `header`.
        if unsafe { (*header).strong.fetch_sub(1, Ordering::Release) } == 1 {
            std::sync::atomic::fence(Ordering::Acquire);
            let cap = unsafe { (*header).cap as usize };
            let layout = Self::layout(cap);
            // SAFETY: this is the final strong reference, so no other
            // thread can access the allocation after the release/acquire pair.
            unsafe {
                std::ptr::drop_in_place(header.cast_mut());
                dealloc(header.cast::<u8>().cast_mut(), layout);
            }
        }
    }
}

impl SmolStr {
    /// Empty string (inline, len 0).
    #[must_use]
    pub const fn new() -> Self {
        Self { raw: 0 }
    }

    /// Constructs an empty string with space reserved for at least `capacity`
    /// UTF-8 bytes.  Small hints stay inline; larger hints allocate the same
    /// thin, copy-on-write buffer used by appended strings so a mutable VM
    /// `String` can consume the reservation without reallocating.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        if capacity <= SMOL_INLINE_MAX {
            Self::new()
        } else {
            Self::new_heap_with_capacity(&[], capacity)
        }
    }

    /// Constructs a [`SmolStr`] from a borrowed `&str`. Strings
    /// up to 7 bytes are stored inline; longer strings allocate
    /// a fresh thin RC byte buffer.
    ///
    /// Intentionally not the [`std::str::FromStr`] trait method -
    /// `FromStr` returns `Result` to model fallible parsing and
    /// this conversion is infallible. Implementing the trait
    /// would force callers to `.unwrap()` an `Ok`-only path.
    #[must_use]
    #[allow(
        clippy::should_implement_trait,
        reason = "infallible conversion; FromStr would force callers to .unwrap()"
    )]
    pub fn from_str(s: &str) -> Self {
        if s.len() <= SMOL_INLINE_MAX {
            Self::new_inline(s.as_bytes())
        } else {
            Self::new_heap(s.as_bytes())
        }
    }

    /// Constructs a [`SmolStr`] from an owned [`String`]. Avoids
    /// re-allocating for inline-fitting strings; heap-bound strings
    /// move their bytes into the thin RC buffer.
    #[must_use]
    pub fn from_string(s: String) -> Self {
        if s.len() <= SMOL_INLINE_MAX {
            Self::new_inline(s.as_bytes())
        } else {
            Self::new_heap(s.as_bytes())
        }
    }

    /// Constructs an uppercase string, using a byte-wise fast path for ASCII
    /// and Rust's Unicode expansion for non-ASCII.
    #[must_use]
    pub fn to_uppercase_from(s: &str) -> Self {
        if !s.is_ascii() {
            return Self::from_string(s.to_uppercase());
        }
        let bytes = s.as_bytes();
        if bytes.len() <= SMOL_INLINE_MAX {
            let mut buf = [0u8; 8];
            for (i, &b) in bytes.iter().enumerate() {
                buf[i] = if b.is_ascii_lowercase() {
                    b - (b'a' - b'A')
                } else {
                    b
                };
            }
            buf[7] = bytes.len() as u8;
            Self {
                raw: u64::from_le_bytes(buf),
            }
        } else {
            let ptr = HeapSmolStr::alloc_ascii_upper(bytes) as usize as u64;
            debug_assert!(
                ptr & SMOL_HEAP_TAG == 0,
                "HeapSmolStr pointer must have high bit clear"
            );
            Self {
                raw: ptr | SMOL_HEAP_TAG,
            }
        }
    }

    /// Constructs from an existing `Arc<String>` - used by value
    /// registry paths that still expose strings that way.
    #[must_use]
    pub fn from_arc(arc: Arc<String>) -> Self {
        Self::from_str(arc.as_str())
    }

    fn new_inline(bytes: &[u8]) -> Self {
        debug_assert!(bytes.len() <= SMOL_INLINE_MAX);
        let mut buf = [0u8; 8];
        buf[..bytes.len()].copy_from_slice(bytes);
        // Length in the high byte (offset 7). High bit is 0,
        // so the heap tag is implicitly clear.
        buf[7] = bytes.len() as u8;
        Self {
            raw: u64::from_le_bytes(buf),
        }
    }

    fn new_heap(bytes: &[u8]) -> Self {
        Self::new_heap_with_capacity(bytes, bytes.len())
    }

    fn new_heap_with_capacity(bytes: &[u8], cap: usize) -> Self {
        debug_assert!(cap >= bytes.len());
        // SAFETY: `HeapSmolStr::alloc` returns an aligned allocation
        // obtained from the global allocator; user-space pointers on
        // supported targets fit in the low 63 bits, so OR-ing the tag
        // bit is information-preserving.
        let ptr = HeapSmolStr::alloc_with_capacity(bytes, cap) as usize as u64;
        debug_assert!(
            ptr & SMOL_HEAP_TAG == 0,
            "HeapSmolStr pointer must have high bit clear"
        );
        Self {
            raw: ptr | SMOL_HEAP_TAG,
        }
    }

    fn grown_capacity(current: usize, needed: usize) -> usize {
        debug_assert!(needed > current);
        let doubled = current.saturating_mul(2).max(16);
        doubled.max(needed)
    }

    /// Returns the borrowed string contents. Inline storage
    /// uses bytes from `self`; heap storage dereferences the
    /// underlying `String`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        if self.raw & SMOL_HEAP_TAG == 0 {
            // Inline: read length, return the prefix.
            // SAFETY: `new_inline` only writes valid UTF-8
            // bytes (since the input was a `&str`), so the
            // resulting prefix is valid UTF-8. The reference
            // ties its lifetime to `self`.
            let bytes: [u8; 8] = self.raw.to_le_bytes();
            let len = bytes[7] as usize;
            unsafe {
                let ptr = (&raw const self.raw).cast::<u8>();
                let slice = std::slice::from_raw_parts(ptr, len);
                std::str::from_utf8_unchecked(slice)
            }
        } else {
            // Heap: dereference the thin RC byte buffer.
            // SAFETY: only constructed via `HeapSmolStr::alloc`;
            // the strong count is at least 1 for the lifetime
            // of `self` (we hold one reference). We never give
            // out the raw pointer outside `Drop` / `Clone`.
            let ptr = (self.raw & SMOL_PTR_MASK) as *const HeapSmolStr;
            unsafe { HeapSmolStr::as_str(ptr) }
        }
    }

    /// Appends `s` in place. The heap variant keeps spare capacity and grows
    /// it when uniquely owned, so repeated appends to a `mut String` cost
    /// O(total length) instead of O(n^2). A shared heap string is copied once
    /// on the next append (copy-on-write). Inline storage appends in place
    /// until it exceeds the 7-byte window, then promotes to a heap string
    /// sized for both halves.
    pub fn push_str(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        if self.raw & SMOL_HEAP_TAG == 0 {
            let len = self.raw.to_le_bytes()[7] as usize;
            if len + s.len() <= SMOL_INLINE_MAX {
                let mut buf = self.raw.to_le_bytes();
                buf[len..len + s.len()].copy_from_slice(s.as_bytes());
                buf[7] = (len + s.len()) as u8;
                self.raw = u64::from_le_bytes(buf);
            } else {
                let mut owned = String::with_capacity(len + s.len());
                owned.push_str(self.as_str());
                owned.push_str(s);
                *self = Self::new_heap_with_capacity(
                    owned.as_bytes(),
                    Self::grown_capacity(SMOL_INLINE_MAX, owned.len()),
                );
            }
        } else {
            let ptr = (self.raw & SMOL_PTR_MASK) as *mut HeapSmolStr;
            // SAFETY: `ptr` comes from a live heap SmolStr owned by `self`.
            let (len, cap, unique) = unsafe {
                (
                    (*ptr).len as usize,
                    (*ptr).cap as usize,
                    HeapSmolStr::is_unique(ptr),
                )
            };
            let needed = len + s.len();
            if unique && needed <= cap {
                // SAFETY: uniqueness and capacity are checked immediately above.
                unsafe { HeapSmolStr::append_unique(ptr, s.as_bytes()) };
                return;
            }
            // A VM builtin receives a cloned receiver before its write-back.
            // Copy-on-write therefore commonly takes this branch even when a
            // prior `String::with_capacity` reservation is large enough. Keep
            // that reservation on the replacement buffer; only grow when the
            // appended bytes exceed it.
            let new_cap = if needed <= cap {
                cap
            } else {
                Self::grown_capacity(cap, needed)
            };
            let mut owned = String::with_capacity(needed);
            owned.push_str(self.as_str());
            owned.push_str(s);
            let old = std::mem::replace(
                self,
                Self::new_heap_with_capacity(owned.as_bytes(), new_cap),
            );
            drop(old);
        }
    }

    /// Appends one Unicode scalar while retaining any reserved capacity.
    pub fn push(&mut self, ch: char) {
        let mut encoded = [0u8; 4];
        self.push_str(ch.encode_utf8(&mut encoded));
    }

    /// Returns the number of Unicode scalar values.
    #[must_use]
    pub fn len(&self) -> usize {
        if self.raw & SMOL_HEAP_TAG == 0 {
            self.as_str().chars().count()
        } else {
            let ptr = (self.raw & SMOL_PTR_MASK) as *const HeapSmolStr;
            // SAFETY: a heap-tagged SmolStr always owns a live HeapSmolStr.
            unsafe { (*ptr).char_len as usize }
        }
    }

    /// Returns the UTF-8 byte length.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.as_str().len()
    }

    /// Returns the Unicode scalar at `index`.
    #[must_use]
    pub fn char_at(&self, index: usize) -> Option<char> {
        if self.raw & SMOL_HEAP_TAG == 0 {
            return self.as_str().chars().nth(index);
        }
        let ptr = (self.raw & SMOL_PTR_MASK) as *const HeapSmolStr;
        // SAFETY: a heap-tagged SmolStr always owns a live HeapSmolStr.
        let header = unsafe { &*ptr };
        if index >= header.char_len as usize {
            return None;
        }
        let block = index / SMOL_CHAR_INDEX_STRIDE;
        let block_char = block * SMOL_CHAR_INDEX_STRIDE;
        let byte = header.char_index[block] as usize;
        self.as_str()[byte..].chars().nth(index - block_char)
    }

    /// Maps a Unicode scalar position to its UTF-8 byte boundary.
    #[must_use]
    pub fn char_boundary(&self, index: usize) -> Option<usize> {
        if index > self.len() {
            return None;
        }
        if index == self.len() {
            return Some(self.byte_len());
        }
        if self.raw & SMOL_HEAP_TAG == 0 {
            return self.as_str().char_indices().nth(index).map(|(i, _)| i);
        }
        let ptr = (self.raw & SMOL_PTR_MASK) as *const HeapSmolStr;
        // SAFETY: a heap-tagged SmolStr always owns a live HeapSmolStr.
        let header = unsafe { &*ptr };
        let block = index / SMOL_CHAR_INDEX_STRIDE;
        let block_char = block * SMOL_CHAR_INDEX_STRIDE;
        let byte = header.char_index[block] as usize;
        self.as_str()[byte..]
            .char_indices()
            .nth(index - block_char)
            .map(|(offset, _)| byte + offset)
    }

    #[cfg(test)]
    fn capacity(&self) -> usize {
        if self.raw & SMOL_HEAP_TAG == 0 {
            SMOL_INLINE_MAX
        } else {
            let ptr = (self.raw & SMOL_PTR_MASK) as *const HeapSmolStr;
            // SAFETY: a heap-tagged SmolStr always owns a live HeapSmolStr.
            unsafe { (*ptr).cap as usize }
        }
    }

    /// Returns `true` iff the string has zero bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for SmolStr {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for SmolStr {
    fn clone(&self) -> Self {
        if self.raw & SMOL_HEAP_TAG != 0 {
            // SAFETY: we own a strong reference; reconstruct an
            // Arc to bump the count, then forget so we don't
            // drop our copy. The original raw stays valid.
            let ptr = (self.raw & SMOL_PTR_MASK) as *const HeapSmolStr;
            unsafe { HeapSmolStr::inc(ptr) };
        }
        Self { raw: self.raw }
    }
}

impl Drop for SmolStr {
    fn drop(&mut self) {
        if self.raw & SMOL_HEAP_TAG != 0 {
            // SAFETY: we own one strong reference produced by
            // `HeapSmolStr::alloc`. Decrementing releases it exactly once.
            let ptr = (self.raw & SMOL_PTR_MASK) as *const HeapSmolStr;
            unsafe { HeapSmolStr::dec(ptr) };
        }
    }
}

impl PartialEq for SmolStr {
    fn eq(&self, other: &Self) -> bool {
        // Fast path: both inline with same raw bits → equal.
        if self.raw == other.raw {
            return true;
        }
        self.as_str() == other.as_str()
    }
}

impl Eq for SmolStr {}

impl std::hash::Hash for SmolStr {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl PartialOrd for SmolStr {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SmolStr {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl fmt::Debug for SmolStr {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), out)
    }
}

impl fmt::Display for SmolStr {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(self.as_str())
    }
}

impl AsRef<str> for SmolStr {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::ops::Deref for SmolStr {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq<str> for SmolStr {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for SmolStr {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl From<String> for SmolStr {
    fn from(s: String) -> Self {
        Self::from_string(s)
    }
}

impl From<&str> for SmolStr {
    fn from(s: &str) -> Self {
        Self::from_str(s)
    }
}

impl From<Arc<String>> for SmolStr {
    fn from(arc: Arc<String>) -> Self {
        Self::from_arc(arc)
    }
}

// SAFETY: heap storage is a thin atomic-refcounted immutable byte buffer.
// Inline storage is plain bytes copyable across threads.
unsafe impl Send for SmolStr {}
unsafe impl Sync for SmolStr {}

/// Compact integer identity for struct and enum-variant names.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TypeTag(Arc<str>);

static TYPE_TAGS: std::sync::LazyLock<
    parking_lot::Mutex<rustc_hash::FxHashMap<String, std::sync::Weak<str>>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()));

impl TypeTag {
    /// Returns the interned textual name for this tag.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the compact numeric identity stored in aggregate nodes.
    #[must_use]
    pub fn id(&self) -> u64 {
        Arc::as_ptr(&self.0).cast::<()>() as usize as u64
    }
}

impl fmt::Debug for TypeTag {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), out)
    }
}

impl fmt::Display for TypeTag {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(self.as_str())
    }
}

impl AsRef<str> for TypeTag {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq<str> for TypeTag {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for TypeTag {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

/// Returns a `&'static str` identity for `name`, allocating once
/// per distinct byte sequence. Used by [`Value::variant`],
/// [`Value::struct_`], and [`Value::float_array`] to deduplicate
/// type names across all values that share them - programs
/// typically have a fixed, small set of named types.
///
/// The leak is bounded by that set, not by call count.
#[must_use]
pub(crate) fn intern_type_name(name: &str) -> &'static str {
    static INTERNED: OnceLock<parking_lot::Mutex<rustc_hash::FxHashSet<&'static str>>> =
        OnceLock::new();
    let set = INTERNED.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashSet::default()));
    let mut guard = set.lock();
    if let Some(&s) = guard.get(name) {
        return s;
    }
    let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
    guard.insert(leaked);
    leaked
}

/// A chunk's inline-site table under one process-wide identity, shared the way
/// its frame names are, so a call-stack frame can hold it by reference. Equal
/// tables from recompiling the same function share one entry.
pub(crate) fn intern_inline_sites(
    sites: &[crate::bytecode::InlineSite],
) -> &'static [crate::bytecode::InlineSite] {
    type Table = rustc_hash::FxHashSet<&'static [crate::bytecode::InlineSite]>;
    static INTERNED: OnceLock<parking_lot::Mutex<Table>> = OnceLock::new();
    if sites.is_empty() {
        return &[];
    }
    let set = INTERNED.get_or_init(|| parking_lot::Mutex::new(Table::default()));
    let mut guard = set.lock();
    if let Some(&interned) = guard.get(sites) {
        return interned;
    }
    let leaked: &'static [crate::bytecode::InlineSite] =
        Box::leak(sites.to_vec().into_boxed_slice());
    guard.insert(leaked);
    leaked
}

/// Smallest table size that triggers a sweep for dead entries, and the floor
/// the watermark returns to.
const TYPE_TAG_SWEEP_MIN: usize = 64;
/// Table size at which the next sweep runs. Held under the `TYPE_TAGS` lock.
static TYPE_TAG_SWEEP_AT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(TYPE_TAG_SWEEP_MIN);

/// Resolves the wrapper names every optional- or fallible-returning builtin
/// constructs, without reaching the shared table. Each slot holds a strong
/// share of the same allocation the table hands out, so tag identity - which
/// aggregate dispatch compares by pointer - is unchanged.
fn cached_prelude_tag(name: &str) -> Option<TypeTag> {
    static SOME: OnceLock<TypeTag> = OnceLock::new();
    static NONE: OnceLock<TypeTag> = OnceLock::new();
    static OK: OnceLock<TypeTag> = OnceLock::new();
    static ERR: OnceLock<TypeTag> = OnceLock::new();
    let slot = match name {
        "Some" => &SOME,
        "None" => &NONE,
        "Ok" => &OK,
        "Err" => &ERR,
        _ => return None,
    };
    Some(slot.get_or_init(|| intern_type_tag_uncached(name)).clone())
}

/// Returns a compact identity for a type / variant name, allocating the name
/// text at most once through [`intern_type_name`].
#[must_use]
pub(crate) fn intern_type_tag(name: &str) -> TypeTag {
    if let Some(tag) = cached_prelude_tag(name) {
        return tag;
    }
    intern_type_tag_uncached(name)
}

fn intern_type_tag_uncached(name: &str) -> TypeTag {
    use std::sync::atomic::Ordering;
    let mut tags = TYPE_TAGS.lock();
    if let Some(existing) = tags.get(name).and_then(std::sync::Weak::upgrade) {
        return TypeTag(existing);
    }
    // A tag whose last share dropped leaves a dead entry behind. Reclaiming
    // the whole table on every intern costs one atomic load per distinct name
    // per construction, so the sweep runs only once the table has grown past
    // its size at the previous sweep. Re-interning a dead name reclaims that
    // entry directly through the insert below.
    if tags.len() >= TYPE_TAG_SWEEP_AT.load(Ordering::Relaxed) {
        tags.retain(|_, weak| weak.strong_count() != 0);
        TYPE_TAG_SWEEP_AT.store(
            tags.len().saturating_mul(2).max(TYPE_TAG_SWEEP_MIN),
            Ordering::Relaxed,
        );
    }
    let owned: Arc<str> = Arc::from(name);
    tags.insert(name.to_owned(), Arc::downgrade(&owned));
    TypeTag(owned)
}

#[must_use]
fn type_tag_from_static(name: &'static str) -> TypeTag {
    intern_type_tag(name)
}

/// Closed integer range eligible for the small-variant cache, mirroring
/// the `CPython` small-int cache. Bounds the cache to
/// `names x (SMALL_INT_MAX - SMALL_INT_MIN + 1)` entries per thread.
const SMALL_VARIANT_INT_MIN: i64 = -128;
const SMALL_VARIANT_INT_MAX: i64 = 1024;
/// Max byte length of a single `String`-payload variant eligible for
/// interning. Bounded to the inline `SmolStr` range so the cache key never
/// holds a heap allocation, and so an unbounded space of distinct long
/// strings cannot grow the table. Covers the common case of a small set of
/// repeated string-payload variants (e.g. enum-like tags such as
/// `Str("alpha")` duplicated across many records).
const SMALL_VARIANT_STR_MAX: usize = 16;

/// Cache key for an interned single-small-scalar (or nullary) variant
/// node. `name` is an interned `&'static str`, unique per distinct
/// content, so it identifies the variant exactly.
#[derive(PartialEq, Eq, Hash)]
enum SmallVariantKey {
    Unit(TypeTag),
    Int(TypeTag, i64),
    Bool(TypeTag, bool),
    /// A single short `String` payload. The `SmolStr` is inline (≤ the
    /// `SMALL_VARIANT_STR_MAX` bound) so the key holds no heap allocation.
    Str(TypeTag, SmolStr),
}

thread_local! {
    /// Per-thread interning table for small immutable variant nodes
    /// (lever 3). Thread-local to avoid the cross-thread lock contention
    /// a shared global table would impose on per-connection VM threads.
    ///
    /// Holds a `Weak` rather than a strong reference so the cache never
    /// keeps a node alive on its own: identical small variants that are
    /// concurrently live share one allocation (every leaf of a tree), but
    /// once the last user reference drops, the node is freed and its
    /// liveness is observable through `downgrade()`/`upgrade()` exactly as
    /// for a non-interned node - preserving weak-reference tier parity.
    static SMALL_VARIANT_CACHE: std::cell::RefCell<
        rustc_hash::FxHashMap<SmallVariantKey, std::sync::Weak<VariantInner>>,
    > = std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

/// Returns the cache key if this `(name, fields)` pair is eligible for
/// small-variant interning: nullary, or a single `Int` in the cached
/// range, or a single `Bool`. Everything else (multi-field nodes,
/// large ints, aggregate payloads) returns `None` and allocates fresh.
fn small_variant_key(name: &TypeTag, fields: &[Value]) -> Option<SmallVariantKey> {
    match fields {
        [] => Some(SmallVariantKey::Unit(name.clone())),
        [Value::Int(n)] if (SMALL_VARIANT_INT_MIN..=SMALL_VARIANT_INT_MAX).contains(n) => {
            Some(SmallVariantKey::Int(name.clone(), *n))
        }
        [Value::Bool(b)] => Some(SmallVariantKey::Bool(name.clone(), *b)),
        // A single short string payload: immutable, so sharing one node
        // across all identical occurrences is sound exactly as for scalars.
        [Value::String(s)] if s.len() <= SMALL_VARIANT_STR_MAX => {
            Some(SmallVariantKey::Str(name.clone(), s.clone()))
        }
        _ => None,
    }
}

/// Returns a shared `Arc<VariantInner>` for an interning-eligible node:
/// reuses the cached node when a live one exists, otherwise allocates a
/// fresh one and records a `Weak` to it. The node is immutable, so all
/// aliases observe identical structure; the cache holding only a `Weak`
/// keeps liveness (and thus `Weak::upgrade`) faithful to the user's
/// references.
fn intern_small_variant(
    name: TypeTag,
    fields: SmallVec<[Value; 2]>,
    key: SmallVariantKey,
) -> Arc<VariantInner> {
    SMALL_VARIANT_CACHE.with(|cache| {
        if let Some(existing) = cache.borrow().get(&key).and_then(std::sync::Weak::upgrade) {
            return existing;
        }
        let node = Arc::new(VariantInner { name, fields });
        cache.borrow_mut().insert(key, Arc::downgrade(&node));
        node
    })
}

fn variant_with_tag_and_fields(name: TypeTag, fields: SmallVec<[Value; 2]>) -> Value {
    if let Some(key) = small_variant_key(&name, &fields) {
        return Value::Variant(intern_small_variant(name, fields, key));
    }
    Value::Variant(Arc::new(VariantInner { name, fields }))
}

/// Converts a constructor's temporary field `Vec` into the inline payload
/// storage used by ordinary enum nodes. `SmallVec::from_vec` only inlines when
/// the source Vec's capacity is <= the inline capacity; VM call-argument Vecs
/// are pooled and may carry a larger spare capacity from an unrelated call
/// site. For arity <= 2, force a move into inline storage so a two-field enum
/// node does not retain an accidental heap buffer.
fn variant_fields(fields: Vec<Value>) -> SmallVec<[Value; 2]> {
    if fields.len() <= 2 {
        fields.into_iter().collect()
    } else {
        SmallVec::from_vec(fields)
    }
}

/// Interns a struct field name to a `&'static str` with a leak
/// bounded by the program's fixed set of field names. Exposed for
/// `gossamer-binding`'s `#[derive(GosStruct)]` glue, which builds a
/// `Value::Struct` from runtime field-name strings.
#[must_use]
pub fn intern_field_name(name: &str) -> &'static str {
    intern_type_name(name)
}

fn intern_struct_field_names(names: &[&'static str]) -> Arc<[&'static str]> {
    type StructShapeCache = rustc_hash::FxHashMap<Box<[&'static str]>, Arc<[&'static str]>>;

    thread_local! {
        static SHAPES: std::cell::RefCell<StructShapeCache> =
            std::cell::RefCell::new(rustc_hash::FxHashMap::default());
    }
    SHAPES.with(|shapes| {
        let mut shapes = shapes.borrow_mut();
        if let Some(existing) = shapes.get(names) {
            return Arc::clone(existing);
        }
        let shape: Arc<[&'static str]> = Arc::from(names);
        shapes.insert(names.into(), Arc::clone(&shape));
        shape
    })
}

fn intern_struct_field_names_2(field0: &'static str, field1: &'static str) -> Arc<[&'static str]> {
    if field0 == "0" && field1 == "1" {
        static POSITIONAL_2: OnceLock<Arc<[&'static str]>> = OnceLock::new();
        return Arc::clone(POSITIONAL_2.get_or_init(|| Arc::from(["0", "1"])));
    }
    intern_struct_field_names(&[field0, field1])
}

/// Shared empty `Arc<Vec<Value>>` sentinel returned by every
/// constructor that would otherwise allocate a fresh empty `Vec`
/// plus Arc header (~32 B per call). All empty-payload variants
/// and arrays share this single allocation.
#[must_use]
pub(crate) fn empty_value_arc() -> Arc<Vec<Value>> {
    static EMPTY: OnceLock<Arc<Vec<Value>>> = OnceLock::new();
    Arc::clone(EMPTY.get_or_init(|| Arc::new(Vec::new())))
}

/// Shared empty `Arc<Vec<(&'static str, Value)>>` sentinel for
/// field-less struct constructors.
#[must_use]
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(crate) fn empty_struct_fields() -> Vec<(&'static str, Value)> {
    Vec::new()
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
/// What a send did.
///
/// A send into a closed channel is a program error, matching Go: the
/// receiver has been told no more values are coming. A deadlocked send is
/// one no runnable goroutine can ever complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// The value is on the channel.
    Sent,
    /// Nothing left running can take the value.
    Deadlocked,
    /// The channel was closed.
    Closed,
}

/// Shared channel backing a `(Sender<T>, Receiver<T>)` pair.
///
/// Capacity semantics mirror modern Go where `0` is an unbuffered
/// rendezvous channel, positive values are bounded buffers, and
/// `Channel::unbounded()` is the explicit queue form retained for
/// Gossamer code that wants non-blocking producer growth.
#[derive(Clone)]
pub struct Channel {
    inner: Arc<ChannelInner>,
}

impl Channel {
    /// Stable identity of this channel, shared by every clone of it.
    /// A join handle is a channel, so this is what keys a cohort child
    /// to the handle its joiner holds.
    #[must_use]
    pub fn identity(&self) -> usize {
        Arc::as_ptr(&self.inner) as usize
    }
}

struct ChannelInner {
    state: Mutex<ChannelState>,
    cv: parking_lot::Condvar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChannelCapacity {
    Unbuffered,
    Unbounded,
    Bounded(usize),
}

struct ChannelMessage {
    id: u64,
    value: Value,
}

struct ChannelState {
    buf: VecDeque<ChannelMessage>,
    capacity: ChannelCapacity,
    closed: bool,
    next_send_id: u64,
    waiting_receivers: usize,
    waiting_senders: usize,
    select_waiters: Vec<Arc<SelectWaiter>>,
    /// Whether this channel currently contributes to the pending-handoff count
    /// maintained through [`crate::vm::goroutine::adjust_pending_handoffs`],
    /// kept in step with [`ChannelState::has_ready_waiter`] so the global count
    /// is a plain sum.
    counted_ready: bool,
}

impl ChannelState {
    /// True when a thread already waiting on this channel would complete if
    /// it woke right now: a queued value with a receiver to take it, or room
    /// (or a receiver) for a blocked sender's value.
    ///
    /// A closed channel releases every waiter, so it is always ready.
    fn has_ready_waiter(&self) -> bool {
        if self.closed && (self.waiting_receivers > 0 || self.waiting_senders > 0) {
            return true;
        }
        if self.waiting_receivers > 0 && !self.buf.is_empty() {
            return true;
        }
        if self.waiting_senders == 0 {
            return false;
        }
        match self.capacity {
            // An unbuffered sender's value is queued until a receiver takes
            // it. A receiver present means the handoff is imminent; an empty
            // buffer means it already happened and the sender has simply not
            // observed it yet. Either way the sender is not stuck.
            ChannelCapacity::Unbuffered => self.waiting_receivers > 0 || self.buf.is_empty(),
            ChannelCapacity::Unbounded => true,
            ChannelCapacity::Bounded(capacity) => self.buf.len() < capacity,
        }
    }
}

/// Wait handle used by the bytecode VM to park one `select` expression
/// across several channels and wake when any arm may be ready.
pub struct SelectWaiter {
    ready: Mutex<bool>,
    cv: parking_lot::Condvar,
}

impl SelectWaiter {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            ready: Mutex::new(false),
            cv: parking_lot::Condvar::new(),
        })
    }

    fn wake(&self) {
        let mut ready = self.ready.lock();
        *ready = true;
        self.cv.notify_all();
    }

    fn wait(&self) {
        let mut ready = self.ready.lock();
        while !*ready {
            self.cv.wait(&mut ready);
        }
    }
}

/// Every channel still reachable, so a deadlock report can recompute
/// readiness from the channels themselves rather than trust a counter that
/// any interleaving could have left behind.
static LIVE_CHANNELS: Mutex<Vec<std::sync::Weak<ChannelInner>>> = Mutex::new(Vec::new());

fn register_live_channel(inner: &Arc<ChannelInner>) {
    let mut live = LIVE_CHANNELS.lock();
    live.retain(|weak| weak.strong_count() > 0);
    live.push(Arc::downgrade(inner));
}

/// True when any channel other than `holding` has a waiter that would
/// complete, or is being changed right now by another thread.
///
/// Called only once the waiter counts already say every participant is
/// blocked, so the walk is rare. A channel whose lock is held elsewhere is
/// mid-update and counts as progress: reporting a deadlock over a state
/// still being written would turn a working program into a failure, while
/// missing one only means the program waits as it did before.
fn any_channel_can_progress(holding: &ChannelInner) -> bool {
    let live = LIVE_CHANNELS.lock();
    for weak in live.iter() {
        let Some(inner) = weak.upgrade() else {
            continue;
        };
        if std::ptr::eq(Arc::as_ptr(&inner), std::ptr::from_ref(holding)) {
            continue;
        }
        let Some(state) = inner.state.try_lock() else {
            return true;
        };
        if state.has_ready_waiter() {
            return true;
        }
    }
    false
}

/// Wakes every goroutine parked on a channel so each re-evaluates whether
/// the program can still make progress. Called once when `main` returns:
/// the set of participants shrinks at that moment, and a waiter parked
/// before it can only learn so by being given a chance to look again.
pub fn wake_all_channel_waiters() {
    let live = LIVE_CHANNELS.lock();
    for weak in live.iter() {
        if let Some(inner) = weak.upgrade() {
            // Taken and released around the notify: a waiter decides
            // whether to sleep while holding this lock, so notifying
            // without it can land in the gap between that decision and
            // the wait, waking nobody and leaving the waiter asleep on a
            // condition that has already changed.
            drop(inner.state.lock());
            inner.cv.notify_all();
        }
    }
}

/// The deadlock report: every participant is waiting on a channel, so the
/// value the operation waits for can never arrive.
#[must_use]
pub fn deadlock_error(op: &'static str) -> RuntimeError {
    if gossamer_runtime::platform::CAN_BLOCK {
        RuntimeError::Panic(format!(
            "all goroutines are asleep - deadlock! ({op} can never complete)"
        ))
    } else {
        RuntimeError::WouldNeverWake(op)
    }
}

/// Result of a blocking receive.
#[derive(Debug)]
pub enum RecvOutcome {
    /// A value was taken from the channel.
    Value(Value),
    /// Every sender is gone and the channel is drained.
    Closed,
    /// Nothing in the program can send: every participant is waiting on a
    /// channel, so no value will ever arrive.
    Deadlocked,
}

impl Channel {
    /// Constructs a new unbuffered channel.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(0)
    }

    /// Constructs an explicit unbounded queue channel.
    #[must_use]
    pub fn unbounded() -> Self {
        Self::with_mode(ChannelCapacity::Unbounded)
    }

    /// Constructs a channel with the given buffered capacity. A
    /// `capacity` of `0` is unbuffered; a positive value bounds the
    /// buffer so a send parks once the buffer reaches capacity.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        if capacity == 0 {
            Self::with_mode(ChannelCapacity::Unbuffered)
        } else {
            Self::with_mode(ChannelCapacity::Bounded(capacity))
        }
    }

    fn with_mode(capacity: ChannelCapacity) -> Self {
        let channel = Self {
            inner: Arc::new(ChannelInner {
                state: Mutex::new(ChannelState {
                    buf: VecDeque::new(),
                    capacity,
                    closed: false,
                    next_send_id: 1,
                    waiting_receivers: 0,
                    waiting_senders: 0,
                    select_waiters: Vec::new(),
                    counted_ready: false,
                }),
                cv: parking_lot::Condvar::new(),
            }),
        };
        register_live_channel(&channel.inner);
        channel
    }

    /// Pushes `value` onto the channel and notifies any parked
    /// receiver so it can re-check. On a bounded channel (positive
    /// capacity) the caller parks on the condvar while the buffer is at
    /// capacity, so a producer outrunning its consumer applies
    /// backpressure exactly as the compiled tier's `gos_rt_chan_send`.
    #[must_use]
    pub fn send(&self, value: Value) -> SendOutcome {
        let mut guard = self.inner.state.lock();
        // A receiver has been told no more values are coming, so a value
        // handed over after the close would be dropped where the program
        // expects it delivered. Go panics here and so does this.
        if guard.closed {
            return SendOutcome::Closed;
        }
        match guard.capacity {
            ChannelCapacity::Unbuffered => {
                let id = guard.next_send_id;
                guard.next_send_id = guard.next_send_id.wrapping_add(1).max(1);
                guard.buf.push_back(ChannelMessage { id, value });
                // Register before announcing: a receiver that wakes on the
                // notify must find this sender already on record, or the
                // channel reads as holding a value nobody is waiting to hand
                // over. The registration is released only once the value has
                // been taken, for the same reason.
                guard.waiting_senders += 1;
                self.notify_channel_changed(&mut guard);
                let mut deadlocked = false;
                while guard.buf.iter().any(|msg| msg.id == id)
                    && !guard.closed
                    // A cancelled cohort has no reader left to take this
                    // value, so the send stops waiting for a handoff that
                    // is not coming.
                    && !crate::stdlib_builtins::cohort::current_is_cancelled()
                {
                    let Some(_waiting) = crate::vm::goroutine::ChannelWait::enter(|| {
                        guard.has_ready_waiter()
                            || any_channel_can_progress(&self.inner)
                            || crate::stdlib_builtins::cohort::deadline_pending()
                    }) else {
                        deadlocked = true;
                        break;
                    };
                    self.inner.cv.wait(&mut guard);
                }
                guard.waiting_senders = guard.waiting_senders.saturating_sub(1);
                Self::sync_ready_count(&mut guard);
                if deadlocked {
                    return SendOutcome::Deadlocked;
                }
            }
            ChannelCapacity::Unbounded => {
                guard.buf.push_back(ChannelMessage { id: 0, value });
                self.notify_channel_changed(&mut guard);
            }
            ChannelCapacity::Bounded(capacity) => {
                if guard.buf.len() >= capacity {
                    guard.waiting_senders += 1;
                    Self::sync_ready_count(&mut guard);
                    let mut deadlocked = false;
                    while guard.buf.len() >= capacity {
                        let Some(_waiting) = crate::vm::goroutine::ChannelWait::enter(|| {
                            guard.has_ready_waiter() || any_channel_can_progress(&self.inner)
                        }) else {
                            deadlocked = true;
                            break;
                        };
                        self.inner.cv.wait(&mut guard);
                    }
                    guard.waiting_senders = guard.waiting_senders.saturating_sub(1);
                    Self::sync_ready_count(&mut guard);
                    if deadlocked {
                        return SendOutcome::Deadlocked;
                    }
                }
                guard.buf.push_back(ChannelMessage { id: 0, value });
                self.notify_channel_changed(&mut guard);
            }
        }
        // A close that landed while this send was parked leaves no reader
        // expecting the value, which is the same program error.
        if guard.closed {
            return SendOutcome::Closed;
        }
        SendOutcome::Sent
    }

    /// Non-blocking send. Enqueues `value` and returns `true` when the
    /// operation can complete immediately; returns
    /// `false` without enqueueing when a bounded buffer is at capacity.
    /// Used by `select` so a full send arm reads as not-ready instead
    /// of blocking inside the readiness probe.
    #[must_use]
    pub fn try_send(&self, value: Value) -> SendOutcome {
        let mut guard = self.inner.state.lock();
        if guard.closed {
            return SendOutcome::Closed;
        }
        match guard.capacity {
            ChannelCapacity::Unbuffered => {
                if guard.waiting_receivers == 0 {
                    return SendOutcome::Deadlocked;
                }
                guard.buf.push_back(ChannelMessage { id: 0, value });
                self.notify_channel_changed(&mut guard);
                SendOutcome::Sent
            }
            ChannelCapacity::Unbounded => {
                guard.buf.push_back(ChannelMessage { id: 0, value });
                self.notify_channel_changed(&mut guard);
                SendOutcome::Sent
            }
            ChannelCapacity::Bounded(capacity) => {
                if guard.buf.len() >= capacity {
                    return SendOutcome::Deadlocked;
                }
                guard.buf.push_back(ChannelMessage { id: 0, value });
                self.notify_channel_changed(&mut guard);
                SendOutcome::Sent
            }
        }
    }

    /// Marks the channel as closed and wakes every parked receiver
    /// so they observe the closed state and exit their wait. Returns
    /// `true` when this call performed the close and `false` when the
    /// channel was already closed - the caller turns the latter into
    /// a `close of closed channel` panic, matching Go. That panic is
    /// goroutine-scoped, so it ends only the offending goroutine
    /// (fatal on `main`) and never aborts the whole process.
    #[must_use]
    pub fn close(&self) -> bool {
        let mut guard = self.inner.state.lock();
        if guard.closed {
            return false;
        }
        guard.closed = true;
        self.notify_channel_changed(&mut guard);
        true
    }

    /// Non-blocking receive. Returns `None` when the channel is
    /// empty (regardless of close state - callers that need
    /// drain-aware semantics should use [`Channel::recv`]).
    #[must_use]
    pub fn try_recv(&self) -> Option<Value> {
        let mut guard = self.inner.state.lock();
        let value = guard.buf.pop_front().map(|msg| msg.value);
        if value.is_some() {
            self.notify_channel_changed(&mut guard);
        }
        value
    }

    /// Blocking receive. Parks until a value is available or the
    /// channel is closed AND drained. Returns `None` only after
    /// observing `closed = true && buf.is_empty()`. Mirrors Go's
    /// `v, ok := <-ch` shape so `while let Some(v) = rx.recv()`
    /// drains and exits cleanly when the producer closes.
    #[must_use]
    pub fn recv(&self) -> RecvOutcome {
        let mut guard = self.inner.state.lock();
        // The registration is held across the wait *and* the take that ends
        // it. Dropping it the moment the wait returns would leave a queued
        // value with no receiver on record, and a sender checking in that
        // window would read a channel that is about to hand off as stuck.
        let mut registered = false;
        let outcome = loop {
            // A cancelled cohort answers its children the way a closed
            // channel does: nothing more is coming. What already arrived is
            // still handed over, because a closed channel drains before it
            // answers `None` and cancellation says no more will be sent, not
            // that a delivered value is dropped. A join handle carries its
            // child's outcome on this path, and the failure that cancelled
            // the cohort is often the very value waiting in the buffer.
            if crate::stdlib_builtins::cohort::current_is_cancelled() {
                if let Some(msg) = guard.buf.pop_front() {
                    break RecvOutcome::Value(msg.value);
                }
                break RecvOutcome::Closed;
            }
            if let Some(msg) = guard.buf.pop_front() {
                break RecvOutcome::Value(msg.value);
            }
            if guard.closed {
                break RecvOutcome::Closed;
            }
            if !registered {
                registered = true;
                guard.waiting_receivers += 1;
                Self::sync_ready_count(&mut guard);
                self.wake_select_waiters(&guard);
            }
            let Some(_waiting) = crate::vm::goroutine::ChannelWait::enter(|| {
                guard.has_ready_waiter()
                    || any_channel_can_progress(&self.inner)
                    || crate::stdlib_builtins::context::deadline_pending()
                    || crate::stdlib_builtins::cohort::deadline_pending()
            }) else {
                break RecvOutcome::Deadlocked;
            };
            self.inner.cv.wait(&mut guard);
        };
        if registered {
            guard.waiting_receivers = guard.waiting_receivers.saturating_sub(1);
        }
        self.notify_channel_changed(&mut guard);
        outcome
    }

    /// Blocking receive which also observes a caller-provided cancellation
    /// predicate. A context that has already fired short-circuits without
    /// consuming a queued value, matching the native runtime's receive
    /// ordering, so the answer does not depend on whether a sender reached
    /// the channel first. The bounded wait makes a cancellation raised during
    /// the wait visible even though a Context does not share this channel's
    /// condvar.
    #[must_use]
    pub fn recv_with_cancel(&self, is_cancelled: impl Fn() -> bool) -> Option<Value> {
        if is_cancelled() {
            return None;
        }
        let mut guard = self.inner.state.lock();
        loop {
            if let Some(msg) = guard.buf.pop_front() {
                self.notify_channel_changed(&mut guard);
                return Some(msg.value);
            }
            if guard.closed || is_cancelled() {
                return None;
            }
            // Nothing can send, and no cancellation can be raised, while this
            // waits on the browser build, so waiting cannot change the answer
            // a drained open channel gives.
            if !gossamer_runtime::platform::CAN_BLOCK {
                return None;
            }
            guard.waiting_receivers += 1;
            self.wake_select_waiters(&guard);
            self.inner
                .cv
                .wait_for(&mut guard, Duration::from_millis(50));
            guard.waiting_receivers = guard.waiting_receivers.saturating_sub(1);
        }
    }

    /// Returns `true` when the channel currently has at least one
    /// pending value. Used by `select` to pick a ready arm.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        !self.inner.state.lock().buf.is_empty()
    }

    /// `true` when both buffer drained and channel closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        let guard = self.inner.state.lock();
        guard.closed && guard.buf.is_empty()
    }

    /// Registers a `select` waiter that will be woken when this
    /// channel's readiness may have changed.
    pub fn register_select_waiter(&self, waiter: &Arc<SelectWaiter>) {
        let mut guard = self.inner.state.lock();
        if !guard.select_waiters.iter().any(|w| Arc::ptr_eq(w, waiter)) {
            guard.select_waiters.push(Arc::clone(waiter));
        }
    }

    /// Removes a previously registered `select` waiter.
    pub fn unregister_select_waiter(&self, waiter: &Arc<SelectWaiter>) {
        let mut guard = self.inner.state.lock();
        guard.select_waiters.retain(|w| !Arc::ptr_eq(w, waiter));
    }

    /// Constructs a waiter for a blocking `select`.
    #[must_use]
    pub fn select_waiter() -> Arc<SelectWaiter> {
        SelectWaiter::new()
    }

    /// Blocks until any registered channel wakes the waiter.
    pub fn wait_select(waiter: &SelectWaiter) {
        waiter.wait();
    }

    fn notify_channel_changed(&self, guard: &mut ChannelState) {
        Self::sync_ready_count(guard);
        self.inner.cv.notify_all();
        self.wake_select_waiters(guard);
    }

    /// Re-reads this channel's readiness and applies the difference to the
    /// process-wide count. Called under the channel lock on every state
    /// change and on every waiter arrival or departure, so the count is
    /// always the number of channels with a waiter that could proceed.
    fn sync_ready_count(guard: &mut ChannelState) {
        let ready = guard.has_ready_waiter();
        if ready == guard.counted_ready {
            return;
        }
        guard.counted_ready = ready;
        crate::vm::goroutine::adjust_pending_handoffs(ready);
    }

    fn wake_select_waiters(&self, guard: &ChannelState) {
        let waiters = guard.select_waiters.clone();
        for waiter in waiters {
            waiter.wake();
        }
    }
}

impl Default for Channel {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Channel {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "<channel len={}>", self.inner.state.lock().buf.len())
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
}

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

impl fmt::Display for Value {
    #[allow(
        clippy::too_many_lines,
        reason = "one match whose length is the value set"
    )]
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Primitive formatting delegates to the shared runtime
        // helpers so the interpreter and the native backend produce
        // byte-identical text. the parity plan.
        match self {
            Self::NativeEnum(o) => fmt::Display::fmt(&native_enum_to_variant(o), out),
            // Cells render as their inner value - they are a call-
            // protocol artifact, never a user-visible shape.
            Self::MutCell(c) => {
                let inner = c.lock().clone();
                fmt::Display::fmt(&inner, out)
            }
            Self::CaptureCell(c) => {
                let inner = c.lock().clone();
                fmt::Display::fmt(&inner, out)
            }
            Self::Unit => out.write_str(gossamer_runtime::builtins::format_unit()),
            Self::Bool(b) => out.write_str(gossamer_runtime::builtins::format_bool(*b)),
            Self::Int(i) => out.write_str(&gossamer_runtime::builtins::format_int(*i)),
            Self::Float(f) => out.write_str(&gossamer_runtime::builtins::format_float(*f)),
            Self::Char(c) => write!(out, "{c}"),
            Self::String(s) => out.write_str(s),
            Self::Json(value) => out.write_str(&gossamer_std::json::encode(value.as_value())),
            Self::Tuple(parts) => write_tuple(out, parts),
            Self::Array(parts) => write_array(out, parts),
            Self::FloatArray(_) => write_array(out, &self.float_array_elems()),
            Self::IntArray(data) => {
                let elems: Vec<Value> = data.iter().copied().map(Value::Int).collect();
                write_array(out, &elems)
            }
            Self::ByteArray(data) => {
                let elems: Vec<Value> = data
                    .iter()
                    .copied()
                    .map(|value| Value::Int(i64::from(value)))
                    .collect();
                write_array(out, &elems)
            }
            Self::InlineByteArray(data) => {
                let elems: Vec<Value> = data
                    .iter()
                    .copied()
                    .map(|value| Value::Int(i64::from(value)))
                    .collect();
                write_array(out, &elems)
            }
            Self::ByteVec(data) => {
                let elems: Vec<Value> = data
                    .iter()
                    .copied()
                    .map(|value| Value::Int(i64::from(value)))
                    .collect();
                write_array(out, &elems)
            }
            Self::FloatVec(data) => {
                let elems: Vec<Value> = data.iter().copied().map(Value::Float).collect();
                write_array(out, &elems)
            }
            Self::LazyIter(id) => match crate::stdlib_builtins::iter::lazy_iter_repr(id.id()) {
                Some(range) => out.write_str(&range),
                None => out.write_str("<iterator>"),
            },
            Self::Variant(inner) => write_variant(out, inner.name.as_str(), &inner.fields),
            Self::Struct(inner) => {
                // Placeholder expressions evaluate to this sentinel in the VM;
                // the compiled tiers emit "<value>" for the same cases.
                if inner.name == "<stub>" {
                    return out.write_str("<value>");
                }
                if matches!(inner.name.as_str(), "bytes::Buffer" | "bytes::Builder") {
                    return out.write_str(inner.name.as_str());
                }
                if let Some(items) = vec_render_items(inner) {
                    return out.write_str(&repr_vec(items));
                }
                if is_set_struct_name(inner.name.as_str()) {
                    return out.write_str(&repr_set(self));
                }
                if is_deque_struct_name(inner.name.as_str()) {
                    return out.write_str(&repr_deque(self, inner.name.as_str()));
                }
                if is_heap_struct_name(inner.name.as_str()) {
                    return out.write_str(&repr_binary_heap(self, inner.name.as_str()));
                }
                // `errors::Error` prints Go-style as its colon-joined
                // cause chain ("outer: mid: root") so `format!("{}", e)`
                // and `?`-surfaced errors match the compiled tiers'
                // `gos_rt_error_display` path. `.message()` stays
                // top-level-only. Other structs keep the default
                // `Name { f: v, … }` shape used everywhere else.
                if let Some(text) = error_chain_text(self) {
                    return out.write_str(&text);
                }
                write_struct(
                    out,
                    source_facing_nested_item_name(inner.name.as_str()),
                    &inner.fields.to_vec(),
                )
            }
            Self::Closure(_) => out.write_str("<closure>"),
            Self::Builtin(inner) => write!(out, "<builtin {}>", inner.name),
            Self::Native(inner) => write!(out, "<native {}>", inner.name),
            Self::Channel(ch) => write!(out, "{ch:?}"),
            Self::Map(map) => write_map(out, &map.lock()),
            Self::IntMap(map) => write_int_map(out, &map.lock()),
            Self::StrIntMap(map) => write_str_int_map(out, &map.lock()),
            Self::Uint(n) => write!(out, "{n}"),
            Self::Weak(_) => out.write_str("<weak>"),
            Self::Void => out.write_str("<void>"),
        }
    }
}

/// Descriptor bytes naming where a rendered value's integers were declared
/// unsigned. The compiler builds one per rendered argument whose type holds
/// such an integer; [`uint_leaves`] walks the value alongside it.
pub(crate) mod uint_desc {
    /// Nothing under this position is unsigned.
    pub(crate) const NONE: u8 = b'.';
    /// This integer position is unsigned.
    pub(crate) const UINT: u8 = b'u';
    /// A fixed array or a slice; the element's own descriptor follows.
    /// Renders in bare brackets, which is how both are written.
    pub(crate) const SEQ: u8 = b'v';
    /// A `Vec`; the element's own descriptor follows. Renders in the
    /// `Vec` literal's own spelling, `#[..]`, which is what tells it
    /// apart from a fixed array at every depth - the two share one
    /// runtime representation, so only the descriptor knows.
    pub(crate) const VEC: u8 = b'V';
    /// A map; the key's descriptor follows, then the value's.
    pub(crate) const MAP: u8 = b'm';
    /// A tuple; its arity follows as one byte, then that many descriptors.
    pub(crate) const TUPLE: u8 = b't';
    /// An `Option`; the payload's descriptor follows.
    pub(crate) const OPTION: u8 = b'o';
    /// A `Result`; the `Ok` descriptor follows, then the `Err` one.
    pub(crate) const RESULT: u8 = b'r';
    /// A `Set` / `BTreeSet` handle; the element's own descriptor follows.
    /// The elements live in a runtime registry, so the descriptor rides
    /// on the handle the renderer copies.
    pub(crate) const SET: u8 = b's';
    /// A `Deque` / `Queue` / `Stack` / heap handle; the element's own
    /// descriptor follows. The elements live in a runtime registry, so
    /// the descriptor rides on the handle the renderer copies.
    pub(crate) const CONTAINER: u8 = b'c';
    /// A struct; its field count follows as one byte, then that many
    /// descriptors, in declaration order. Only the REPL builds this:
    /// a program renders a struct through the `to_string` synthesized
    /// for its type, which describes each field where it formats it.
    pub(crate) const ADT: u8 = b'A';
}

/// Name of the one-field wrapper [`uint_leaves`] puts a `Vec` in so the
/// renderer knows to spell it `#[..]`. A `Vec` and a fixed array share
/// one runtime representation, so the descriptor built from the static
/// type is the only thing that can tell them apart; the wrapper carries
/// that answer on the renderer's private copy and nowhere else.
pub(crate) const VEC_RENDER_NAME: &str = "__vec";

/// Field a rendered container handle carries to describe its elements,
/// for the containers whose elements the renderer reads out of a
/// registry rather than out of the value. Only [`uint_leaves`] adds it.
pub(crate) const ELEM_DESC_MARKER: &str = "__elemdesc";

/// Returns `value` with the integers the descriptor names re-boxed as
/// [`Value::Uint`], so a `u64` at or above `i64::MAX` renders as its own
/// decimal instead of the negative the same bits spell. The copy is the
/// renderer's alone, so the source keeps its own representation.
#[must_use]
pub fn uint_leaves(value: &Value, desc: &[u8]) -> Value {
    let mut cursor = 0usize;
    convert_uint(value, desc, &mut cursor)
}

/// Advances `cursor` past one descriptor without converting anything.
fn skip_uint_desc(desc: &[u8], cursor: &mut usize) {
    let tag = desc.get(*cursor).copied().unwrap_or(uint_desc::NONE);
    *cursor += 1;
    match tag {
        uint_desc::SEQ
        | uint_desc::VEC
        | uint_desc::OPTION
        | uint_desc::CONTAINER
        | uint_desc::SET => {
            skip_uint_desc(desc, cursor);
        }
        uint_desc::MAP | uint_desc::RESULT => {
            skip_uint_desc(desc, cursor);
            skip_uint_desc(desc, cursor);
        }
        uint_desc::TUPLE | uint_desc::ADT => {
            let arity = desc.get(*cursor).copied().unwrap_or(0) as usize;
            *cursor += 1;
            for _ in 0..arity {
                skip_uint_desc(desc, cursor);
            }
        }
        _ => {}
    }
}

fn convert_uint(value: &Value, desc: &[u8], cursor: &mut usize) -> Value {
    let tag = desc.get(*cursor).copied().unwrap_or(uint_desc::NONE);
    *cursor += 1;
    match tag {
        uint_desc::UINT => match value {
            Value::Int(n) => Value::Uint(*n as u64),
            other => other.clone(),
        },
        uint_desc::SEQ => convert_uint_sequence(value, desc, cursor),
        uint_desc::VEC => {
            // A value already carrying the `Vec` spelling reaches a second
            // format site inside the rendering that describes it again.
            if let Value::Struct(inner) = value
                && vec_render_items(inner).is_some()
            {
                skip_uint_desc(desc, cursor);
                return value.clone();
            }
            let converted = convert_uint_sequence(value, desc, cursor);
            Value::struct_(VEC_RENDER_NAME, vec![("items", converted)])
        }
        uint_desc::TUPLE => {
            let arity = desc.get(*cursor).copied().unwrap_or(0) as usize;
            *cursor += 1;
            let Value::Tuple(parts) = value else {
                for _ in 0..arity {
                    skip_uint_desc(desc, cursor);
                }
                return value.clone();
            };
            let mut out = Vec::with_capacity(parts.len());
            for i in 0..arity {
                match parts.get(i) {
                    Some(part) => out.push(convert_uint(part, desc, cursor)),
                    None => skip_uint_desc(desc, cursor),
                }
            }
            out.extend(parts.iter().skip(arity).cloned());
            Value::Tuple(Arc::new(out))
        }
        uint_desc::ADT => {
            let count = desc.get(*cursor).copied().unwrap_or(0) as usize;
            *cursor += 1;
            let Value::Struct(inner) = value else {
                for _ in 0..count {
                    skip_uint_desc(desc, cursor);
                }
                return value.clone();
            };
            let mut fields = inner.fields.to_vec();
            for i in 0..count {
                match fields.get_mut(i) {
                    Some((_, field)) => *field = convert_uint(&field.clone(), desc, cursor),
                    None => skip_uint_desc(desc, cursor),
                }
            }
            Value::struct_(inner.name.as_str(), fields)
        }
        uint_desc::OPTION | uint_desc::RESULT => convert_uint_variant(value, desc, cursor, tag),
        uint_desc::MAP => convert_uint_map(value, desc, cursor),
        uint_desc::CONTAINER | uint_desc::SET => {
            let elem_at = *cursor;
            skip_uint_desc(desc, cursor);
            match value {
                Value::Struct(inner)
                    if desc[elem_at..*cursor].iter().any(|b| *b != uint_desc::NONE) =>
                {
                    let elem_desc: String =
                        desc[elem_at..*cursor].iter().map(|b| *b as char).collect();
                    let mut fields = inner.fields.to_vec();
                    fields.push((ELEM_DESC_MARKER, Value::String(elem_desc.as_str().into())));
                    Value::struct_(inner.name.as_str(), fields)
                }
                other => other.clone(),
            }
        }
        _ => value.clone(),
    }
}

fn convert_uint_sequence(value: &Value, desc: &[u8], cursor: &mut usize) -> Value {
    let elem_at = *cursor;
    skip_uint_desc(desc, cursor);
    let convert = |v: &Value| {
        let mut elem_cursor = elem_at;
        convert_uint(v, desc, &mut elem_cursor)
    };
    match value {
        Value::Array(items) => Value::Array(Arc::new(items.iter().map(convert).collect())),
        Value::IntArray(items) => Value::Array(Arc::new(
            items.iter().map(|n| convert(&Value::Int(*n))).collect(),
        )),
        other => other.clone(),
    }
}

fn convert_uint_variant(value: &Value, desc: &[u8], cursor: &mut usize, tag: u8) -> Value {
    let ok_at = *cursor;
    skip_uint_desc(desc, cursor);
    let err_at = *cursor;
    if tag == uint_desc::RESULT {
        skip_uint_desc(desc, cursor);
    }
    let Value::Variant(variant) = value else {
        return value.clone();
    };
    let arm_at = match variant.name.as_str() {
        "Some" | "Ok" => ok_at,
        "Err" if tag == uint_desc::RESULT => err_at,
        _ => return value.clone(),
    };
    let fields = variant
        .fields
        .iter()
        .map(|field| {
            let mut field_cursor = arm_at;
            convert_uint(field, desc, &mut field_cursor)
        })
        .collect();
    Value::variant(variant.name.as_str(), fields)
}

fn convert_uint_map(value: &Value, desc: &[u8], cursor: &mut usize) -> Value {
    let key_at = *cursor;
    skip_uint_desc(desc, cursor);
    let val_at = *cursor;
    skip_uint_desc(desc, cursor);
    let key_described = desc[key_at..val_at].iter().any(|b| *b != uint_desc::NONE);
    // A key renders from the `MapKey` the copy stores, so a key the type
    // describes - an unsigned integer, a `Vec`, or an aggregate holding
    // either - is rebuilt from its described value.
    let convert_key = |key: &MapKey| {
        if key_described {
            let mut key_cursor = key_at;
            MapKey::rendered(&convert_uint(&key.to_value(), desc, &mut key_cursor))
        } else {
            key.clone()
        }
    };
    let convert_value = |v: &Value| {
        let mut value_cursor = val_at;
        convert_uint(v, desc, &mut value_cursor)
    };
    let mut out: DenseMap<MapKey, Value> = dense_map();
    match value {
        Value::Map(map) => {
            for (key, entry) in map.lock().iter() {
                out.insert(convert_key(key), convert_value(entry));
            }
        }
        Value::IntMap(map) => {
            for (key, entry) in map.lock().iter() {
                out.insert(
                    convert_key(&MapKey::Int(*key)),
                    convert_value(&Value::Int(*entry)),
                );
            }
        }
        Value::StrIntMap(map) => {
            for (key, entry) in map.lock().iter() {
                out.insert(MapKey::Str(key.clone()), convert_value(&Value::Int(*entry)));
            }
        }
        other => return other.clone(),
    }
    Value::Map(Arc::new(parking_lot::Mutex::new(out)))
}

/// Renders one struct field, reading it as unsigned when the declaration
/// named it `u64` / `usize`, which is what the derived `fmt` the compiled
/// tiers call does.
fn uint_aware_field(struct_name: &str, field_name: &str, field: &Value) -> String {
    match field {
        Value::Int(n) if crate::builtins::struct_field_is_uint(struct_name, field_name) => {
            format!("{}", *n as u64)
        }
        other => repr_value(other),
    }
}

/// A map key's text: a `String` key reads quoted, and every other key reads
/// the way the same value reads anywhere else, which is what both compiled
/// tiers render from the key's tag.
fn map_key_text(key: &Value) -> String {
    match key {
        Value::String(text) => format!("{:?}", text.as_str()),
        other => other.to_string(),
    }
}

fn repr_value(value: &Value) -> String {
    match value {
        Value::Float(number) => repr_float(*number),
        Value::String(text) => format!("{:?}", text.as_str()),
        Value::Char(ch) => format!("{ch:?}"),
        Value::Tuple(parts) => {
            let mut rendered: Vec<String> = parts.iter().map(repr_value).collect();
            if rendered.len() == 1 {
                rendered[0].push(',');
            }
            format!("({})", rendered.join(", "))
        }
        Value::Array(parts) => format!(
            "[{}]",
            parts.iter().map(repr_value).collect::<Vec<_>>().join(", ")
        ),
        Value::FloatArray(_) => repr_value(&Value::Array(Arc::new(value.float_array_elems()))),
        Value::IntArray(data) => format!("{:?}", data.as_slice()),
        Value::ByteArray(data) => format!("{:?}", &data[..]),
        Value::InlineByteArray(data) => format!("{:?}", data.as_slice()),
        Value::ByteVec(data) => format!("{:?}", data.as_slice()),
        Value::FloatVec(data) => format!(
            "[{}]",
            data.iter()
                .map(|number| repr_float(*number))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::LazyIter(id) => crate::stdlib_builtins::iter::lazy_iter_repr(id.id())
            .unwrap_or_else(|| "<iterator>".to_string()),
        Value::Variant(inner) => {
            let fields = inner.fields.iter().map(repr_value).collect::<Vec<_>>();
            if fields.is_empty() {
                inner.name.as_str().to_string()
            } else {
                format!("{}({})", inner.name.as_str(), fields.join(", "))
            }
        }
        Value::Struct(inner)
            if matches!(inner.name.as_str(), "bytes::Buffer" | "bytes::Builder") =>
        {
            inner.name.as_str().to_string()
        }
        Value::Struct(inner) if vec_render_items(inner).is_some() => {
            repr_vec_debug(vec_render_items(inner).unwrap_or(value))
        }
        Value::Struct(inner) if is_set_struct_name(inner.name.as_str()) => repr_set(value),
        Value::Struct(inner) if is_deque_struct_name(inner.name.as_str()) => {
            repr_deque(value, inner.name.as_str())
        }
        Value::Struct(inner) if is_heap_struct_name(inner.name.as_str()) => {
            repr_binary_heap(value, inner.name.as_str())
        }
        Value::Struct(inner) => repr_struct(
            source_facing_nested_item_name(inner.name.as_str()),
            &inner.fields.to_vec(),
        ),
        Value::Map(map) => {
            let map = map.lock();
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            format!(
                "{{{}}}",
                entries
                    .iter()
                    .map(|(key, item)| format!(
                        "{}: {}",
                        map_key_text(&key.to_value()),
                        repr_value(item)
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        Value::StrIntMap(map) => {
            let map = map.lock();
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
            format!(
                "{{{}}}",
                entries
                    .iter()
                    .map(|(key, item)| format!("{:?}: {item}", key.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        Value::MutCell(cell) => repr_value(&cell.lock()),
        Value::NativeEnum(owner) => repr_value(&native_enum_to_variant(owner)),
        _ => value.to_string(),
    }
}

/// Renders `owner {a, b}` over the set's elements, sorted so printed output is
/// stable whatever order the elements went in. Elements print in their Display
/// form, matching `gos_rt_set_format_*` and the way a sequence of the same
/// elements prints.
fn repr_set(value: &Value) -> String {
    let values = crate::stdlib_builtins::set::set_display_snapshot(value).unwrap_or_default();
    // A set renders in its own literal spelling, the same one a program
    // writes to build it, at every depth and on every tier. `Set` and
    // `BTreeSet` are both written `#{..}`. The rendered copy of a set whose
    // element type names an unsigned integer or a `Vec` carries that
    // element's descriptor, so each element reads the way its type spells it.
    format!("#{{{}}}", render_elements(value, &values))
}

/// A set's elements in the order its element type gives them. The snapshot
/// arrives ordered by stored key, which is the language's order for every
/// element except one holding a `u64` / `usize`, whose bits order unsigned;
/// the rendered copy's descriptor names those.
pub(crate) fn described_set_order(handle: &Value, mut values: Vec<Value>) -> Vec<Value> {
    let Some(desc) = elem_desc_of(handle).filter(|desc| desc.as_bytes().contains(&uint_desc::UINT))
    else {
        return values;
    };
    let mut keyed: Vec<(Value, Value)> = values
        .drain(..)
        .map(|value| (uint_leaves(&value, desc.as_bytes()), value))
        .collect();
    keyed.sort_by(|a, b| crate::stdlib_builtins::iter::compare_values_total(&a.0, &b.0));
    keyed.into_iter().map(|(_, value)| value).collect()
}

fn repr_deque(value: &Value, owner: &str) -> String {
    let values = crate::stdlib_builtins::deque::deque_snapshot(value).unwrap_or_default();
    // Elements read the way a sequence's do - `Deque [a, b]` for the same
    // elements a `Vec` prints as `[a, b]`, which is what both compiled tiers
    // render from the element descriptor.
    format!("{owner} [{}]", render_elements(value, &values))
}

/// The elements of a registry-backed container, each rendered under the
/// descriptor the handle carries, comma-joined.
fn render_elements(handle: &Value, values: &[Value]) -> String {
    let desc = elem_desc_of(handle);
    values
        .iter()
        .map(|element| match &desc {
            Some(desc) => render_element(&uint_leaves(element, desc.as_bytes())),
            None => render_element(element),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The element descriptor a rendered container handle carries, if any.
pub(crate) fn elem_desc_of(handle: &Value) -> Option<String> {
    let Value::Struct(inner) = handle else {
        return None;
    };
    inner
        .fields
        .iter()
        .find_map(|(name, value)| (*name == ELEM_DESC_MARKER).then(|| value.to_string()))
}

fn is_set_struct_name(name: &str) -> bool {
    matches!(name, "Set" | "BTreeSet")
}

/// Whether `inner` is the render-only wrapper that says "this sequence
/// is a `Vec`", and the sequence it carries.
pub(crate) fn vec_render_items(inner: &StructInner) -> Option<&Value> {
    if inner.name.as_str() != VEC_RENDER_NAME {
        return None;
    }
    inner.fields.get(0).map(|(_, items)| items)
}

/// One element of a sequence as its own text. A float inside a sequence
/// reads in the form that shows it is one, which is what
/// [`write_element`] writes wherever a sequence is rendered.
fn render_element(value: &Value) -> String {
    match value {
        Value::Float(number) => repr_float(*number),
        other => other.to_string(),
    }
}

/// A `Vec` in its own literal spelling for the `repr` channel, where a
/// nested value reads as the source that would build it - a `String`
/// element in quotes, exactly as a fixed array's element does.
fn repr_vec_debug(items: &Value) -> String {
    match items.as_value_slice() {
        Some(parts) => format!(
            "#[{}]",
            parts.iter().map(repr_value).collect::<Vec<_>>().join(", ")
        ),
        None => repr_vec(items),
    }
}

/// A `Vec` in its own literal spelling, elements rendered as they are
/// anywhere else.
pub(crate) fn vec_render_text(items: &Value) -> String {
    repr_vec(items)
}

/// A `Vec` in its own literal spelling, elements rendered as they are
/// anywhere else.
fn repr_vec(items: &Value) -> String {
    if let Some(parts) = items.as_value_slice() {
        return format!(
            "#[{}]",
            parts
                .iter()
                .map(render_element)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    // A flat typed storage - an `IntArray`, a `FloatVec`, a byte buffer -
    // renders its own elements in bare brackets, and the `Vec` spelling
    // is that same text under the `Vec` opening bracket.
    let rendered = repr_value(items);
    match rendered.strip_prefix('[') {
        Some(rest) => format!("#[{rest}"),
        None => format!("#[{rendered}]"),
    }
}

fn is_deque_struct_name(name: &str) -> bool {
    matches!(name, "Deque" | "Queue" | "Stack")
}

fn is_heap_struct_name(name: &str) -> bool {
    matches!(name, "MaxHeap" | "MinHeap")
}

fn repr_binary_heap(value: &Value, owner: &str) -> String {
    let values =
        crate::stdlib_builtins::container_heap::binary_heap_snapshot(value).unwrap_or_default();
    format!("{owner} [{}]", render_elements(value, &values))
}

fn repr_float(number: f64) -> String {
    gossamer_runtime::builtins::format_float_debug(number)
}

fn repr_struct(name: &str, fields: &[(&'static str, Value)]) -> String {
    // A field declared `u64` / `usize` reads as unsigned, matching the
    // derived `fmt` the compiled tiers call.
    let render_field = |field_name: &str, field: &Value| uint_aware_field(name, field_name, field);
    let is_tuple_struct = !fields.is_empty()
        && fields
            .iter()
            .enumerate()
            .all(|(i, (n, _))| n.parse::<usize>() == Ok(i));
    if is_tuple_struct {
        let fields = fields
            .iter()
            .map(|(field_name, field)| render_field(field_name, field))
            .collect::<Vec<_>>()
            .join(", ");
        return format!("{name}({fields})");
    }
    format!(
        "{name} {{ {} }}",
        fields
            .iter()
            .map(|(field_name, field)| format!("{field_name}: {}", render_field(field_name, field)))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn source_facing_nested_item_name(name: &str) -> &str {
    // A user type's identity carries the modules containing it, so two
    // modules may declare the same name; a rendered value shows the name as
    // declared. A stdlib type whose own name is a path (`errors::Error`,
    // `bytes::Builder`) keeps every segment - its prefix names a stdlib
    // module, which no user module path does.
    let name = match name.split_once("::") {
        Some((head, _)) if gossamer_resolve::STDLIB_MODULES.contains(&head) => name,
        Some(_) => name.rsplit("::").next().unwrap_or(name),
        None => name,
    };
    let Some(rest) = name.strip_prefix("__gos_nested_") else {
        return name;
    };
    let Some((def, source_name)) = rest.split_once('_') else {
        return name;
    };
    if def.bytes().all(|byte| byte.is_ascii_digit()) && !source_name.is_empty() {
        source_name
    } else {
        name
    }
}

/// The colon-joined cause chain an `errors::Error` renders as ("outer: mid:
/// root"), or `None` for any other value.
///
/// `{}` and `{:?}` both answer this text on every tier, so the plain
/// formatter and the walk that renders operands carrying their own
/// rendering share one definition of it. `.message()` stays top-level-only.
pub(crate) fn error_chain_text(value: &Value) -> Option<String> {
    let Value::Struct(inner) = value else {
        return None;
    };
    if inner.name != "errors::Error" {
        return None;
    }
    let message = inner
        .fields
        .iter()
        .find(|(n, _)| (**n) == "message")
        .map(|(_, v)| v.clone())?;
    let mut text = format!("{message}");
    let mut cursor = inner
        .fields
        .iter()
        .find(|(n, _)| (**n) == "cause")
        .map(|(_, v)| v.clone());
    while let Some(Value::Variant(link)) = cursor {
        if link.name != "Some" || link.fields.is_empty() {
            break;
        }
        let Value::Struct(cause) = &link.fields[0] else {
            break;
        };
        if cause.name != "errors::Error" {
            break;
        }
        let Some((_, m)) = cause.fields.iter().find(|(n, _)| (**n) == "message") else {
            break;
        };
        text.push_str(": ");
        text.push_str(&m.to_string());
        cursor = cause
            .fields
            .iter()
            .find(|(n, _)| (**n) == "cause")
            .map(|(_, v)| v.clone());
    }
    Some(text)
}

/// Renders one element of an aggregate. A float always keeps a fractional
/// part or an exponent so the text reads back as a float, matching how a
/// struct field renders; every other value keeps its Display text.
fn write_element(out: &mut fmt::Formatter<'_>, value: &Value) -> fmt::Result {
    match value {
        Value::Float(f) => out.write_str(&gossamer_runtime::builtins::format_float_debug(*f)),
        other => write!(out, "{other}"),
    }
}

fn write_tuple(out: &mut fmt::Formatter<'_>, parts: &[Value]) -> fmt::Result {
    out.write_str("(")?;
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            out.write_str(", ")?;
        }
        write_element(out, part)?;
    }
    if parts.len() == 1 {
        out.write_str(",")?;
    }
    out.write_str(")")
}

/// Renders a `HashMap` as `{k: v, …}` with entries sorted by key so
/// the output is deterministic and byte-identical to the compiled
/// tiers' `gos_rt_map_format` (native map storage has its own
/// implementation-defined order).
fn write_map(out: &mut fmt::Formatter<'_>, map: &DenseMap<MapKey, Value>) -> fmt::Result {
    out.write_str("{")?;
    let mut entries: Vec<(&MapKey, &Value)> = map.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    for (i, (k, v)) in entries.iter().enumerate() {
        if i > 0 {
            out.write_str(", ")?;
        }
        write!(out, "{}: ", map_key_text(&k.to_value()))?;
        write_element(out, v)?;
    }
    out.write_str("}")
}

/// Key-sorted rendering of an `i64`-keyed, `i64`-valued map. Mirrors
/// [`write_map`] for the [`Value::IntMap`] storage shape.
fn write_int_map(out: &mut fmt::Formatter<'_>, map: &DenseMap<i64, i64>) -> fmt::Result {
    out.write_str("{")?;
    let mut entries: Vec<(&i64, &i64)> = map.iter().collect();
    entries.sort_by_key(|&(k, _)| *k);
    for (i, (k, v)) in entries.iter().enumerate() {
        if i > 0 {
            out.write_str(", ")?;
        }
        write!(out, "{k}: {v}")?;
    }
    out.write_str("}")
}

/// Key-sorted rendering of a `String`-keyed, `i64`-valued map.
/// Mirrors [`write_map`] for the [`Value::StrIntMap`] storage shape,
/// quoting keys exactly as the generic map's string keys render.
fn write_str_int_map(out: &mut fmt::Formatter<'_>, map: &DenseMap<SmolStr, i64>) -> fmt::Result {
    out.write_str("{")?;
    let mut entries: Vec<(&SmolStr, &i64)> = map.iter().collect();
    entries.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    for (i, (k, v)) in entries.iter().enumerate() {
        if i > 0 {
            out.write_str(", ")?;
        }
        write!(out, "{:?}: {v}", k.as_str())?;
    }
    out.write_str("}")
}

fn write_array(out: &mut fmt::Formatter<'_>, parts: &[Value]) -> fmt::Result {
    out.write_str("[")?;
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            out.write_str(", ")?;
        }
        write_element(out, part)?;
    }
    out.write_str("]")
}

fn write_variant(out: &mut fmt::Formatter<'_>, name: &str, fields: &[Value]) -> fmt::Result {
    out.write_str(name)?;
    if fields.is_empty() {
        return Ok(());
    }
    out.write_str("(")?;
    for (i, field) in fields.iter().enumerate() {
        if i > 0 {
            out.write_str(", ")?;
        }
        write_element(out, field)?;
    }
    out.write_str(")")
}

fn write_struct(
    out: &mut fmt::Formatter<'_>,
    name: &str,
    fields: &[(&'static str, Value)],
) -> fmt::Result {
    out.write_str(name)?;
    // A tuple struct's fields are named "0".."N-1"; render it as
    // `Name(v0, v1)` to match the derived `fmt` on the compiled tiers.
    let is_tuple_struct = !fields.is_empty()
        && fields
            .iter()
            .enumerate()
            .all(|(i, (n, _))| n.parse::<usize>() == Ok(i));
    if is_tuple_struct {
        out.write_str("(")?;
        for (i, (ident, value)) in fields.iter().enumerate() {
            if i > 0 {
                out.write_str(", ")?;
            }
            out.write_str(&uint_aware_field(name, ident, value))?;
        }
        return out.write_str(")");
    }
    out.write_str(" { ")?;
    for (i, (ident, value)) in fields.iter().enumerate() {
        if i > 0 {
            out.write_str(", ")?;
        }
        write!(
            out,
            "{}: {}",
            (*ident),
            uint_aware_field(name, ident, value)
        )?;
    }
    out.write_str(" }")
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
/// native backend use when reporting the same failure - the
/// "unified error code catalogue" half of
/// the parity plan.
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

// ------------------------------------------------------------------
// Global heap side table (Phase P1)
//
// Heap-backed `Value` variants are registered here before being
// encoded as `TAG_HEAP` u64 words.  In later phases this side table
// will be replaced by direct GC-arena storage.

/// One heap-allocated payload stored in the global side table.
#[derive(Clone)]
enum RegistryEntry {
    /// Integer that did not fit in the i56 immediate range.
    Int(i64),
    /// VM string storage. Heap strings stay in their original compact
    /// allocation; inline strings remain a single copied word.
    String(SmolStr),
    /// Tuple aggregate.
    Tuple(Arc<Vec<Value>>),
    /// Array / Vec aggregate.
    Array(Arc<Vec<Value>>),
    /// Flat f64 struct-array storage.
    FloatArray(Arc<FloatArrayInner>),
    /// Flat i64 array storage.
    IntArray(Arc<Vec<i64>>),
    /// Packed byte array storage.
    ByteArray(Arc<PackedBytes>),
    /// Single-allocation large fixed byte array storage.
    InlineByteArray(Arc<SmallVec<[u8; 1024]>>),
    /// Growable packed byte vector storage.
    ByteVec(Arc<Vec<u8>>),
    /// Flat f64 vector storage.
    FloatVec(Arc<Vec<f64>>),
    /// Enum variant or tuple-struct constructor payload.
    Variant(Arc<VariantInner>),
    /// Struct-shaped aggregate.
    Struct(Arc<StructInner>),
    /// User-defined callable.
    Closure(Arc<Closure>),
    /// Concurrent channel endpoint.
    Channel(Channel),
}

/// Global registry mapping `u32` handles to [`RegistryEntry`] values.
/// Protected by a [`Mutex`] so it is safe to access from goroutine
/// threads.
///
/// The companion `FREE_SLOTS` free-list keeps slot reuse O(1):
/// `register_heap` pops a known-empty index off the stack instead of
/// linearly scanning every slot for `None` (which was O(n) per
/// registration on long-running programs).
static REGISTRY: Mutex<RegistryStorage> = Mutex::new(RegistryStorage {
    slots: Vec::new(),
    free: Vec::new(),
});

struct RegistryStorage {
    slots: Vec<Option<RegistryEntry>>,
    free: Vec<u32>,
}

/// Stores `entry` in the global side table and returns its stable
/// handle. Reuses a previously-released slot when one is available
/// so the registry stays bounded by the in-flight raw-value count
/// instead of growing monotonically with cumulative `to_raw` calls.
fn register_heap(entry: RegistryEntry) -> u32 {
    let mut reg = REGISTRY.lock();
    if let Some(idx) = reg.free.pop() {
        reg.slots[idx as usize] = Some(entry);
        return idx;
    }
    let id = reg.slots.len();
    reg.slots.push(Some(entry));
    u32::try_from(id).expect("registry handle overflow")
}

/// Removes `handle` from the global side table and returns the
/// stored entry. The slot is recycled onto the free-list so the next
/// `register_heap` can reuse it. Returns `None` when the slot is
/// empty (the object was already taken or never registered).
fn take_heap(handle: u32) -> Option<RegistryEntry> {
    let mut reg = REGISTRY.lock();
    let entry = reg.slots.get_mut(handle as usize).and_then(Option::take)?;
    reg.free.push(handle);
    Some(entry)
}

/// Returns `(slots, occupied)` where `slots` is the size of the
/// registry's slot vector and `occupied` is the count of currently
/// non-empty slots. Test-only - exposed so the value-roundtrip suite
/// can assert that the registry stays bounded under repeated
/// `to_raw`/`from_raw` cycles.
#[doc(hidden)]
#[must_use]
pub fn registry_stats_for_test() -> (usize, usize) {
    let reg = REGISTRY.lock();
    let occupied = reg.slots.iter().filter(|s| s.is_some()).count();
    (reg.slots.len(), occupied)
}

#[cfg(test)]
mod size_assertions {
    use super::Value;

    #[test]
    fn value_size_at_most_16_bytes() {
        // Assertion lock-down for the `Value` enum size. Each
        // non-trivial variant must keep its body behind a
        // single pointer / 8-byte payload (e.g. `Arc<...>`,
        // `SmolStr`). Adding a wider payload (raw `Vec<...>`,
        // raw `String`) will fail this test.
        //
        // The natural fit on 64-bit is 16 bytes (8 disc + 8
        // payload). A future compact-value representation can collapse this
        // further to 8 bytes by encoding the tag inside the payload -
        // see `gossamer_runtime::GossamerValue` for the layout
        // the LLVM lowerer already speaks. Until then this
        // assertion is the regression guard.
        let n = std::mem::size_of::<Value>();
        assert!(n <= 16, "Value grew to {n} bytes (target ≤16)");
    }

    #[test]
    fn report_value_size_for_visibility() {
        let n = std::mem::size_of::<Value>();
        eprintln!("Value size: {n} bytes");
    }
}

// ---------------------------------------------------------------
// Native enum handles (JIT interop).
// ---------------------------------------------------------------

/// Field classification for one positional payload slot of a native
/// enum variant, used to convert raw payload words into [`Value`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeFieldKind {
    /// 64-bit integer (all integer widths occupy one slot).
    I64,
    /// 64-bit float (stored as raw bits in the slot).
    F64,
    /// Boolean (non-zero slot = true).
    Bool,
    /// Heap string (slot is a tagged c-string body pointer).
    Str,
    /// Unicode scalar value (the compiled layout stores it in the
    /// low 32 bits of the payload slot).
    Char,
    /// Another supported heap enum; index into the program's shape
    /// table.
    Enum(u32),
    /// A `Vec<E>` field where `E` is a supported heap enum: the payload
    /// slot holds a `*mut GosVec` of 8-byte PRIMITIVE slots, each a native
    /// enum pointer of the shape-table index carried here (the AOT layout a
    /// `JsonVal::Arr(Vec<JsonVal>)` variant uses).
    VecEnum(u32),
    /// A `Vec<(String, E)>` field where `E` is a supported heap enum: the
    /// payload slot holds a `*mut GosVec` of 16-byte PRIMITIVE slots laid out
    /// `[*c_char @ +0][native-enum ptr @ +8]` (the AOT layout a
    /// `JsonVal::Obj(Vec<(String, JsonVal)>)` variant uses). The index is the
    /// element enum's shape-table index.
    VecStrEnumTuple(u32),
}

/// One variant of a native enum shape.
#[derive(Debug)]
pub struct NativeVariantShape {
    /// Variant name (interned, pointer-comparable with `VariantIs`).
    pub name: &'static str,
    /// Positional field kinds.
    pub fields: Vec<NativeFieldKind>,
}

/// Layout description of a heap enum whose values may cross the JIT
/// boundary as raw native pointers. Built once per program load from
/// the HIR and shared by native value handles.
#[derive(Debug)]
pub struct NativeEnumShape {
    /// Enum name (diagnostics).
    pub enum_name: &'static str,
    /// Index of this shape in the program's shape table.
    pub index: u32,
    /// True when the discriminant lives in pointer bits 1-2 (at most
    /// 4 variants); false = header byte at `payload - 3`.
    pub tagged: bool,
    /// Variants in declaration order.
    pub variants: Vec<NativeVariantShape>,
}

/// Owning handle for a native (compiled-representation) enum value
/// produced by a JIT-compiled body. Holds one strong reference;
/// dropping the last clone releases it through the runtime.
#[derive(Debug)]
pub struct NativeEnumOwner {
    /// Tagged native pointer (compiled-tier representation).
    pub ptr: usize,
    /// Layout for VM-side structural access.
    pub shape: Arc<NativeEnumShape>,
    /// `true` when this handle exclusively owns the whole tree it roots (a
    /// value returned from a JIT body to the VM). Its drop frees the tree via
    /// the shape walk, tolerating the caller-cleans over-retention the native
    /// code leaves. `false` for a borrowed handle read out of a parent's field
    /// (`native_enum_field`), whose drop balances a single retain and must not
    /// touch the parent-owned subtree.
    pub owned: bool,
}

/// Releases one reference to a native enum value, also reclaiming the `Vec`
/// and string payloads the node-meta release does not reach (a `Vec<Enum>` /
/// `Vec<(String, Enum)>` field is a separate `GosVec` the node's child-layout
/// meta does not list). Native nodes are refcounted - the VM retains a child
/// when it reads a field - so this releases each owned reference exactly once:
/// only when a node is the *last* owner (strong count <= 1) are its `Vec` /
/// string children reclaimed. Enum-pointer children are left to the runtime's
/// own meta cascade, which is iterative and so safe for deep recursive trees
/// (an explicit walk here would overflow the native stack on a depth-20 tree).
fn release_native_enum_tree(ptr: usize, shape: &NativeEnumShape) {
    use gossamer_runtime::c_abi as rt;
    let base = ptr & !7;
    if base == 0 {
        return;
    }
    // SAFETY: `base` is a live runtime-managed node; reading its strong count
    // is valid. Single-threaded per VM, so the count is stable across the
    // check-then-reclaim below.
    let last = unsafe { rt::gos_rt_rc_strong_count(base as *mut u8) } <= 1;
    if last {
        let disc = native_enum_disc(ptr, shape);
        if let Some(variant) = shape.variants.get(disc) {
            for (i, kind) in variant.fields.iter().enumerate() {
                let slot = (base + i * 8) as *mut i64;
                // SAFETY: payload slot inside the node's allocation.
                let word = unsafe { *slot };
                match kind {
                    NativeFieldKind::Str => {
                        if word != 0 {
                            // SAFETY: a live owned cstring body.
                            unsafe { rt::gos_rt_str_free(word as *mut std::os::raw::c_char) };
                            // SAFETY: writing a slot we own.
                            unsafe { *slot = 0 };
                        }
                    }
                    NativeFieldKind::VecEnum(eidx) => {
                        release_native_vec_enum(word, *eidx);
                        // SAFETY: writing a slot we own.
                        unsafe { *slot = 0 };
                    }
                    NativeFieldKind::VecStrEnumTuple(eidx) => {
                        release_native_vec_str_enum(word, *eidx);
                        // SAFETY: writing a slot we own.
                        unsafe { *slot = 0 };
                    }
                    // Enum-pointer children are reclaimed by the runtime's meta
                    // cascade on the release below (deep-safe). Scalars own
                    // nothing.
                    NativeFieldKind::Enum(_)
                    | NativeFieldKind::I64
                    | NativeFieldKind::F64
                    | NativeFieldKind::Bool
                    | NativeFieldKind::Char => {}
                }
            }
        }
    }
    // SAFETY: `base` is a live node; releasing balances one owning reference.
    // When the count reaches zero the runtime frees the node and cascades to
    // its (still-live) enum-pointer children.
    unsafe { rt::gos_rt_rc_release(base as *mut u8) };
}

/// Releases one reference to each element of a native `Vec<Enum>` and frees the
/// buffer. Called only from the last-owner path of [`release_native_enum_tree`].
fn release_native_vec_enum(word: i64, eidx: u32) {
    use gossamer_runtime::c_abi as rt;
    if word == 0 {
        return;
    }
    let v = word as *mut rt::vec::GosVec;
    if let Some(eshape) = native_shape(eidx) {
        // SAFETY: live `GosVec` of 8-byte native-enum pointer slots.
        let len = unsafe { rt::gos_rt_vec_len(v) }.max(0);
        for i in 0..len {
            let elem = unsafe { rt::gos_rt_vec_get_i64(v, i) };
            release_native_enum_tree(elem as usize, &eshape);
        }
    }
    // SAFETY: owns this `PRIMITIVE` vec; its elements were released above.
    unsafe { rt::gos_rt_vec_free(v) };
}

/// Releases one reference to each `(String, Enum)` element of a native
/// `Vec<(String, Enum)>` (freeing key cstrings, releasing enum values) and
/// frees the buffer.
fn release_native_vec_str_enum(word: i64, eidx: u32) {
    use gossamer_runtime::c_abi as rt;
    if word == 0 {
        return;
    }
    let v = word as *mut rt::vec::GosVec;
    let eshape = native_shape(eidx);
    // SAFETY: live `GosVec` of 16-byte `[cstr][enum ptr]` slots.
    let len = unsafe { rt::gos_rt_vec_len(v) }.max(0);
    for i in 0..len {
        let p = unsafe { rt::gos_rt_vec_get_ptr(v, i) };
        if p.is_null() {
            continue;
        }
        // SAFETY: 16-byte slot: cstring word at +0, enum pointer at +8.
        let key_word = unsafe { p.cast::<i64>().read_unaligned() };
        if key_word != 0 {
            // SAFETY: a live owned key cstring.
            unsafe { rt::gos_rt_str_free(key_word as *mut std::os::raw::c_char) };
        }
        if let Some(s) = eshape.as_ref() {
            let val_word = unsafe { p.add(8).cast::<i64>().read_unaligned() };
            release_native_enum_tree(val_word as usize, s);
        }
        // SAFETY: writing slots of a vec we own; the vec's own free then
        // reclaims nothing twice.
        unsafe {
            p.cast::<i64>().write_unaligned(0);
            p.add(8).cast::<i64>().write_unaligned(0);
        }
    }
    // SAFETY: owns this vec; slots nulled above.
    unsafe { rt::gos_rt_vec_free(v) };
}

impl Drop for NativeEnumOwner {
    fn drop(&mut self) {
        let base = self.ptr & !7;
        if self.owned && base != 0 {
            free_exclusive_enum_tree(self.ptr, Arc::clone(&self.shape));
        } else {
            release_native_enum_tree(self.ptr, &self.shape);
        }
    }
}

/// Completely frees an exclusively-owned native enum tree via its VM-side
/// shape. Discovers every reachable node once (a shared node in a DAG is
/// visited a single time), reclaims each node's `String` / `Vec` payloads and
/// clears its enum-pointer slots so a release cannot re-enter the runtime
/// cascade, then drains each node's strong count to zero. Iterative worklist,
/// so a deep tree does not overflow the native stack. Sound only for an
/// exclusively-owned root (guaranteed by the caller's `strong_count <= 1`
/// gate): each node's whole reference count belongs to this tree, so draining
/// it frees exactly once - a shared subtree still held elsewhere is never
/// routed here.
fn free_exclusive_enum_tree(root_ptr: usize, root_shape: Arc<NativeEnumShape>) {
    use gossamer_runtime::c_abi as rt;
    let root_base = root_ptr & !7;
    if root_base == 0 {
        return;
    }
    // (base, full pointer for discriminant reads, shape) for each node.
    let mut seen: rustc_hash::FxHashSet<usize> = rustc_hash::FxHashSet::default();
    let mut nodes: Vec<(usize, usize, Arc<NativeEnumShape>)> = Vec::new();
    seen.insert(root_base);
    let mut work = vec![(root_ptr, root_shape)];
    while let Some((ptr, shape)) = work.pop() {
        let base = ptr & !7;
        if base == 0 {
            continue;
        }
        nodes.push((base, ptr, Arc::clone(&shape)));
        let disc = native_enum_disc(ptr, &shape);
        let Some(variant) = shape.variants.get(disc) else {
            continue;
        };
        for (i, kind) in variant.fields.iter().enumerate() {
            if let NativeFieldKind::Enum(eidx) = kind
                && let Some(cshape) = native_shape(*eidx)
            {
                // SAFETY: payload slot inside the node's allocation.
                let cword = unsafe { *((base + i * 8) as *const i64) } as usize;
                let cbase = cword & !7;
                if cbase != 0 && seen.insert(cbase) {
                    work.push((cword, cshape));
                }
            }
        }
    }
    // Reclaim `String` / `Vec` payloads and clear every enum-pointer slot so the
    // strong-count drain below cannot re-enter the runtime cascade.
    for (base, ptr, shape) in &nodes {
        let disc = native_enum_disc(*ptr, shape);
        let Some(variant) = shape.variants.get(disc) else {
            continue;
        };
        for (i, kind) in variant.fields.iter().enumerate() {
            let slot = (*base + i * 8) as *mut i64;
            // SAFETY: payload slot inside the node's allocation.
            let payload_word = unsafe { *slot };
            match kind {
                NativeFieldKind::Str => {
                    if payload_word != 0 {
                        // SAFETY: a live owned cstring body.
                        unsafe { rt::gos_rt_str_free(payload_word as *mut std::os::raw::c_char) };
                        // SAFETY: writing a slot we own.
                        unsafe { *slot = 0 };
                    }
                }
                NativeFieldKind::VecEnum(eidx) => {
                    release_native_vec_enum(payload_word, *eidx);
                    // SAFETY: writing a slot we own.
                    unsafe { *slot = 0 };
                }
                NativeFieldKind::VecStrEnumTuple(eidx) => {
                    release_native_vec_str_enum(payload_word, *eidx);
                    // SAFETY: writing a slot we own.
                    unsafe { *slot = 0 };
                }
                NativeFieldKind::Enum(_) => {
                    // SAFETY: writing a slot we own; the child is freed below.
                    unsafe { *slot = 0 };
                }
                NativeFieldKind::I64
                | NativeFieldKind::F64
                | NativeFieldKind::Bool
                | NativeFieldKind::Char => {}
            }
        }
    }
    // Drain each node's strong count to zero and free it. Slots are cleared, so
    // no release re-enters the cascade; the no-buffer release reclaims each node
    // immediately instead of leaving an over-retained interior node parked as a
    // cycle-collection candidate.
    for (base, _, _) in &nodes {
        // SAFETY: `base` is a live runtime-managed node reached from the root.
        let rc = unsafe { rt::gos_rt_rc_strong_count(*base as *mut u8) };
        for _ in 0..rc.max(0) {
            // Re-check before each release so a node already driven to zero by
            // an earlier iteration (a shared node reached along two paths whose
            // count this teardown already drained) is never released past zero
            // into freed memory.
            if unsafe { rt::gos_rt_rc_strong_count(*base as *mut u8) } <= 0 {
                break;
            }
            // SAFETY: exclusively owned and count still positive.
            unsafe { rt::gos_rt_rc_release_no_buffer(*base as *mut u8) };
        }
    }
}

/// Process-global weak compatibility table of registered native enum shapes.
/// A loaded VM owns the strong descriptor handles; this table exists only for
/// legacy shape-index operands and JIT trampoline metadata while the VM
/// migrates to fully program-owned shape sessions. Dead programs leave no
/// descriptor allocation alive through the compatibility path.
static NATIVE_SHAPES: std::sync::LazyLock<
    parking_lot::RwLock<rustc_hash::FxHashMap<u32, std::sync::Weak<NativeEnumShape>>>,
> = std::sync::LazyLock::new(|| parking_lot::RwLock::new(rustc_hash::FxHashMap::default()));
static NEXT_NATIVE_SHAPE_INDEX: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Maps a variant name to the single native enum shape that declares it, so
/// the bytecode enum constructor ([`Value::variant`]) can build the native
/// representation directly instead of a boxed `Variant` that later marshals
/// across the JIT boundary (Step 8: one representation, no marshalling copy).
/// A name declared by more than one shape maps to `None` (ambiguous - the
/// constructor cannot pick a shape from the variant name alone and falls back
/// to the boxed form).
type VariantShapeMap =
    rustc_hash::FxHashMap<&'static str, Option<std::sync::Weak<NativeEnumShape>>>;

static VARIANT_NAME_TO_SHAPE: std::sync::LazyLock<parking_lot::RwLock<VariantShapeMap>> =
    std::sync::LazyLock::new(|| parking_lot::RwLock::new(VariantShapeMap::default()));

/// Bumped after every shape registration. A later program can make a variant
/// name that was unique become ambiguous, so thread-local positive caches must
/// be discarded across registrations.
static NATIVE_SHAPE_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

thread_local! {
    /// Positive-only constructor-shape cache. Negative results cannot be
    /// cached because a later program load may register the name; once a shape
    /// is found, however, the append-only registry makes it immutable.
    static NATIVE_SHAPE_CACHE: std::cell::RefCell<(
        u64,
        rustc_hash::FxHashMap<TypeTag, Arc<NativeEnumShape>>
    )> = std::cell::RefCell::new((0, rustc_hash::FxHashMap::default()));
}

/// The native enum shape that uniquely declares a variant named `name`, or
/// `None` if no native shape declares it or more than one does.
#[must_use]
#[allow(
    dead_code,
    reason = "reached only from the native-enum path a target may not compile"
)]
pub(crate) fn native_shape_for_variant(tag: TypeTag, name: &str) -> Option<Arc<NativeEnumShape>> {
    use std::sync::atomic::Ordering;
    let generation = NATIVE_SHAPE_GENERATION.load(Ordering::Acquire);
    if let Some(shape) = NATIVE_SHAPE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.0 != generation {
            cache.0 = generation;
            cache.1.clear();
        }
        cache.1.get(&tag).cloned()
    }) {
        return Some(shape);
    }
    let shape = VARIANT_NAME_TO_SHAPE
        .read()
        .get(name)
        .cloned()
        .flatten()?
        .upgrade()?;
    NATIVE_SHAPE_CACHE.with(|cache| {
        cache.borrow_mut().1.insert(tag, Arc::clone(&shape));
    });
    Some(shape)
}

/// Atomically reserves a contiguous block of shape indices and
/// registers the shapes that `build` produces under them.
///
/// The reserve (reading the base index) and the inserts happen under a
/// single write lock, so concurrent program loads can never interleave
/// a reserve with another load's register - the indices a shape is
/// built against are guaranteed to be the indices it lands at.
///
/// `build` is handed the base index the block will occupy and must
/// return the shapes in index order, each carrying `index == base +
/// offset`. Returns `build`'s second value (typically the `DefId ->
/// index` map the shapes were built against).
pub fn register_native_shapes<R>(build: impl FnOnce(u32) -> (Vec<Arc<NativeEnumShape>>, R)) -> R {
    let mut t = NATIVE_SHAPES.write();
    t.retain(|_, weak| weak.strong_count() != 0);
    // The builder needs its base before it can reveal the batch length. Reserve
    // a fixed, intentionally generous block; shape batches are tiny and the
    // opaque compatibility ids need only be unique, not dense.
    let base = NEXT_NATIVE_SHAPE_INDEX.fetch_add(1024, std::sync::atomic::Ordering::AcqRel);
    let (shapes, result) = build(base);
    let mut names = VARIANT_NAME_TO_SHAPE.write();
    for (offset, shape) in shapes.into_iter().enumerate() {
        debug_assert_eq!(
            shape.index,
            base + u32::try_from(offset).unwrap_or(0),
            "shape table index drift",
        );
        // Step 8 builds native for every registered shape, including
        // `Vec`-bearing enums (e.g. a JSON-like `List(Vec<Node>)`). A marshalled
        // `Vec` element is a fresh, exclusively-owned native copy - never an
        // alias of a live VM node - so construction and teardown stay uniform
        // (drain-to-zero) with no mixed-ownership double free.
        for variant in &shape.variants {
            names
                .entry(variant.name)
                .and_modify(
                    |entry| match entry.as_ref().and_then(std::sync::Weak::upgrade) {
                        // The old program has gone away. Reuse the compatibility
                        // entry instead of leaving a stale ambiguity behind.
                        None => *entry = Some(Arc::downgrade(&shape)),
                        Some(existing) if Arc::ptr_eq(&existing, &shape) => {}
                        // A live second shape declaring this variant name makes
                        // the constructor ambiguous; fall back to `Variant`.
                        Some(_) => *entry = None,
                    },
                )
                .or_insert_with(|| Some(Arc::downgrade(&shape)));
        }
        t.insert(shape.index, Arc::downgrade(&shape));
    }
    NATIVE_SHAPE_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Release);
    result
}

/// Looks up a registered shape by global index.
#[must_use]
pub fn native_shape(idx: u32) -> Option<Arc<NativeEnumShape>> {
    NATIVE_SHAPES.read().get(&idx)?.upgrade()
}

/// The discriminant of a native enum pointer under `shape`.
#[must_use]
pub fn native_enum_disc(ptr: usize, shape: &NativeEnumShape) -> usize {
    if shape.tagged {
        (ptr >> 1) & 3
    } else {
        // SAFETY: header-repr values carry the disc byte at payload-3
        // by the compiled-tier layout contract.
        unsafe { *((ptr - 3) as *const u8) as usize }
    }
}

/// Reads positional field `idx` of a native enum value and converts
/// it to a [`Value`] per the variant's field kind. Returns
/// `Value::Unit` for out-of-range access (mirrors `VariantField`).
#[must_use]
pub fn native_enum_field(owner: &NativeEnumOwner, idx: usize) -> Value {
    let disc = native_enum_disc(owner.ptr, &owner.shape);
    let Some(variant) = owner.shape.variants.get(disc) else {
        return Value::Unit;
    };
    let Some(kind) = variant.fields.get(idx) else {
        return Value::Unit;
    };
    let base = owner.ptr & !7;
    if base == 0 {
        return Value::Unit;
    }
    // SAFETY: payload slot reads inside an allocation sized for the
    // variant's field count (compiled-tier layout contract).
    let word = unsafe { *((base + idx * 8) as *const i64) };
    match kind {
        NativeFieldKind::I64 => Value::Int(word),
        NativeFieldKind::F64 => Value::Float(f64::from_bits(word as u64)),
        NativeFieldKind::Bool => Value::Bool(word != 0),
        NativeFieldKind::Char => Value::Char(char::from_u32(word as u32).unwrap_or('\u{0}')),
        NativeFieldKind::Str => {
            if word == 0 {
                Value::String(SmolStr::default())
            } else {
                // SAFETY: string payload slots hold NUL-terminated
                // tagged c-string bodies.
                let c = unsafe { std::ffi::CStr::from_ptr(word as *const std::os::raw::c_char) };
                Value::String(SmolStr::from(c.to_string_lossy().as_ref()))
            }
        }
        NativeFieldKind::Enum(sidx) => {
            let Some(shape) = native_shape(*sidx) else {
                return Value::Unit;
            };
            // The VM takes its own reference to the child.
            // SAFETY: retain of a live runtime-managed value (or a
            // tagged-null, which the entry treats as null).
            unsafe {
                gossamer_runtime::c_abi::gos_rt_rc_retain(word as usize as *mut u8);
            }
            Value::NativeEnum(Arc::new(NativeEnumOwner {
                ptr: word as usize,
                shape: Arc::clone(&shape),
                owned: false,
            }))
        }
        NativeFieldKind::VecEnum(eidx) => native_vec_enum_to_array(word, *eidx),
        NativeFieldKind::VecStrEnumTuple(eidx) => native_vec_str_enum_to_array(word, *eidx),
    }
}

/// Moves an enum-pointer field out of a uniquely owned native node without a
/// retain/release round trip.  Returns `None` when the node is shared or the
/// field is not itself an enum, in which case the VM must use
/// [`native_enum_field`] and preserve ordinary clone semantics.
///
/// Clearing the payload slot transfers the parent's one child reference to
/// the returned handle: the parent's metadata-driven drop sees null and does
/// not release it a second time.  This is the native counterpart of draining a
/// slot from `VariantInner::fields` in `VariantFieldConsume`.
#[must_use]
pub fn native_enum_field_consume(owner: &mut NativeEnumOwner, idx: usize) -> Option<Value> {
    let base = owner.ptr & !7;
    if base == 0 {
        return None;
    }
    // Arc uniqueness only proves the Rust handle is unique.  Native nodes have
    // their own RC domain, so require its count to be one before mutating a
    // payload slot that another native alias could observe.
    let unique = unsafe { gossamer_runtime::c_abi::gos_rt_rc_strong_count(base as *mut u8) == 1 };
    if !unique {
        return None;
    }
    let disc = native_enum_disc(owner.ptr, &owner.shape);
    let kind = owner.shape.variants.get(disc)?.fields.get(idx)?;
    let NativeFieldKind::Enum(shape_idx) = kind else {
        return None;
    };
    let shape = native_shape(*shape_idx)?;
    let slot = (base + idx * 8) as *mut i64;
    // SAFETY: `slot` is a field of the uniquely owned allocation.  Reading and
    // zeroing it atomically transfers the parent's reference to the new owner.
    let word = unsafe { std::ptr::replace(slot, 0) };
    Some(Value::NativeEnum(Arc::new(NativeEnumOwner {
        ptr: word as usize,
        shape: Arc::clone(&shape),
        owned: false,
    })))
}

/// Reads a native `Vec<E>` payload word (a `*mut GosVec` of 8-byte native
/// enum pointer slots) into a `Value::Array` of `Value::NativeEnum` children,
/// each retained so the array owns its own reference. An empty / null vec
/// yields an empty array.
#[must_use]
pub(crate) fn native_vec_enum_to_array(word: i64, eidx: u32) -> Value {
    if word == 0 {
        return Value::Array(Arc::new(Vec::new()));
    }
    let Some(eshape) = native_shape(eidx) else {
        return Value::Array(Arc::new(Vec::new()));
    };
    let v = word as *const gossamer_runtime::c_abi::vec::GosVec;
    // SAFETY: `v` is a live `GosVec` of 8-byte pointer slots (the AOT
    // `Vec<Enum>` layout); `len`/`get_i64` read initialised in-bounds slots.
    let len = unsafe { gossamer_runtime::c_abi::gos_rt_vec_len(v) }.max(0);
    let mut out = Vec::with_capacity(len as usize);
    for i in 0..len {
        let elem = unsafe { gossamer_runtime::c_abi::gos_rt_vec_get_i64(v, i) };
        if (elem as usize) & !7 == 0 {
            out.push(Value::Unit);
            continue;
        }
        // SAFETY: co-own the child (the parent vec keeps its own share); the
        // returned `NativeEnumOwner` releases it on drop.
        unsafe { gossamer_runtime::c_abi::gos_rt_rc_retain(elem as usize as *mut u8) };
        out.push(Value::NativeEnum(Arc::new(NativeEnumOwner {
            ptr: elem as usize,
            shape: Arc::clone(&eshape),
            owned: false,
        })));
    }
    Value::Array(Arc::new(out))
}

/// Reads a native `Vec<(String, E)>` payload word (a `*mut GosVec` of 16-byte
/// `[*c_char][native-enum ptr]` slots) into a `Value::Array` of 2-tuples
/// `(Value::String, Value::NativeEnum)`. Strings are copied; enum children are
/// retained so the array owns its own reference.
#[must_use]
pub(crate) fn native_vec_str_enum_to_array(word: i64, eidx: u32) -> Value {
    if word == 0 {
        return Value::Array(Arc::new(Vec::new()));
    }
    let Some(eshape) = native_shape(eidx) else {
        return Value::Array(Arc::new(Vec::new()));
    };
    let v = word as *const gossamer_runtime::c_abi::vec::GosVec;
    // SAFETY: `v` is a live `GosVec` of 16-byte slots (the AOT
    // `Vec<(String, Enum)>` layout); `len`/`get_ptr` read in-bounds slots.
    let len = unsafe { gossamer_runtime::c_abi::gos_rt_vec_len(v) }.max(0);
    let mut out = Vec::with_capacity(len as usize);
    for i in 0..len {
        let p = unsafe { gossamer_runtime::c_abi::gos_rt_vec_get_ptr(v, i) };
        if p.is_null() {
            out.push(Value::Tuple(Arc::from(vec![
                Value::String(SmolStr::default()),
                Value::Unit,
            ])));
            continue;
        }
        // SAFETY: each 16-byte slot holds a cstring word at +0 and a native
        // enum pointer word at +8.
        let key_word = unsafe { p.cast::<i64>().read_unaligned() };
        let val_word = unsafe { p.add(8).cast::<i64>().read_unaligned() };
        let key = if key_word == 0 {
            Value::String(SmolStr::default())
        } else {
            // SAFETY: cstring words point at NUL-terminated tagged bodies.
            let c = unsafe { std::ffi::CStr::from_ptr(key_word as *const std::os::raw::c_char) };
            Value::String(SmolStr::from(c.to_string_lossy().as_ref()))
        };
        let val = if (val_word as usize) & !7 == 0 {
            Value::Unit
        } else {
            // SAFETY: co-own the child enum; the tuple's `NativeEnumOwner`
            // releases it on drop.
            unsafe { gossamer_runtime::c_abi::gos_rt_rc_retain(val_word as usize as *mut u8) };
            Value::NativeEnum(Arc::new(NativeEnumOwner {
                ptr: val_word as usize,
                shape: Arc::clone(&eshape),
                owned: false,
            }))
        };
        out.push(Value::Tuple(Arc::from(vec![key, val])));
    }
    Value::Array(Arc::new(out))
}

/// Deep-converts a native enum value into the boxed
/// [`Value::Variant`] representation - the safety valve for paths
/// that need structural `Value`s (FFI bridging, fallback equality).
#[must_use]
pub fn native_enum_to_variant(owner: &NativeEnumOwner) -> Value {
    let disc = native_enum_disc(owner.ptr, &owner.shape);
    let Some(variant) = owner.shape.variants.get(disc) else {
        return Value::Unit;
    };
    let fields: Vec<Value> = (0..variant.fields.len())
        .map(|i| deep_native_value(native_enum_field(owner, i)))
        .collect();
    Value::variant_boxed(variant.name, fields)
}

/// Deep-converts any `Value::NativeEnum` reachable through a value (directly,
/// or inside an `Array` / `Tuple` produced by a `Vec<Enum>` / `Vec<(String,
/// Enum)>` field) into the boxed `Value::Variant` representation.
fn deep_native_value(v: Value) -> Value {
    match v {
        Value::NativeEnum(child) => native_enum_to_variant(&child),
        Value::Array(arc) => Value::Array(Arc::new(
            arc.iter().cloned().map(deep_native_value).collect(),
        )),
        Value::Tuple(arc) => Value::Tuple(Arc::from(
            arc.iter()
                .cloned()
                .map(deep_native_value)
                .collect::<Vec<_>>(),
        )),
        other => other,
    }
}

// ---------------------------------------------------------------
// Native struct shapes (JIT interop).
// ---------------------------------------------------------------

/// Layout description of a user struct whose values may cross the JIT
/// boundary. Built once per program load from the HIR. Unlike a heap enum, a
/// struct in the compiled tier is a flat block of words with NO RC header, and
/// `&self` / `&mut self` point at its first byte.
///
/// Only structs whose fields are scalars or strings are registered: those
/// marshal in O(field count) with no nested aggregates, so the trampoline can
/// build / write back / free the block with no reference-counting and no
/// aliasing surface.
#[derive(Debug)]
pub struct NativeStructShape {
    /// Struct name (interned, matches `StructInner::name`).
    pub struct_name: &'static str,
    /// Index of this shape in the program's struct-shape table.
    pub index: u32,
    /// Field name + scalar kind, in declaration order.
    pub fields: Vec<(&'static str, NativeFieldKind)>,
    /// Where each field sits in the native block, in declaration order.
    pub placements: Vec<NativeFieldPlacement>,
    /// Words the native block spans.
    pub words: usize,
}

/// The bytes one struct field occupies in the native block: a word for a
/// struct laid out in words, and its own width for a narrow field of a packed
/// struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeFieldPlacement {
    /// Byte offset from the start of the block.
    pub offset: u32,
    /// Bytes the field occupies: 1, 2, 4, or 8.
    pub bytes: u8,
    /// Whether a narrow integer field widens by sign extension.
    pub signed: bool,
}

/// Process-global weak compatibility table of registered native struct shapes.
/// Loaded VMs retain descriptors; dead program descriptors are releasable even
/// though legacy shape-index slots remain append-only for now.
static NATIVE_STRUCT_SHAPES: std::sync::LazyLock<
    parking_lot::RwLock<rustc_hash::FxHashMap<u32, std::sync::Weak<NativeStructShape>>>,
> = std::sync::LazyLock::new(|| parking_lot::RwLock::new(rustc_hash::FxHashMap::default()));
static NEXT_NATIVE_STRUCT_SHAPE_INDEX: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);

/// Atomically reserves a contiguous block of struct-shape indices and
/// registers the shapes `build` produces under them. Same contract as
/// [`register_native_shapes`].
pub fn register_native_struct_shapes<R>(
    build: impl FnOnce(u32) -> (Vec<Arc<NativeStructShape>>, R),
) -> R {
    let mut t = NATIVE_STRUCT_SHAPES.write();
    t.retain(|_, weak| weak.strong_count() != 0);
    let base = NEXT_NATIVE_STRUCT_SHAPE_INDEX.fetch_add(1024, std::sync::atomic::Ordering::AcqRel);
    let (shapes, result) = build(base);
    for (offset, shape) in shapes.into_iter().enumerate() {
        debug_assert_eq!(
            shape.index,
            base + u32::try_from(offset).unwrap_or(0),
            "struct shape table index drift",
        );
        t.insert(shape.index, Arc::downgrade(&shape));
    }
    result
}

/// Looks up a registered struct shape by global index.
#[must_use]
pub fn native_struct_shape(idx: u32) -> Option<Arc<NativeStructShape>> {
    NATIVE_STRUCT_SHAPES.read().get(&idx)?.upgrade()
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
mod smolstr_tests {
    use super::SmolStr;

    #[test]
    fn repeated_appends_preserve_contents() {
        let mut s = SmolStr::new();
        for _ in 0..10_000 {
            s.push_str("aç");
        }
        assert_eq!(s.len(), 20_000);
        assert_eq!(s.char_at(19_999), Some('ç'));
        assert!(s.as_str().starts_with("açaç"));
        assert!(s.as_str().ends_with("aç"));
    }

    #[test]
    fn append_to_shared_heap_string_is_copy_on_write() {
        let mut left = SmolStr::from("abcdefgh");
        let right = left.clone();

        left.push_str("-mutated");

        assert_eq!(right.as_str(), "abcdefgh");
        assert_eq!(left.as_str(), "abcdefgh-mutated");
    }

    #[test]
    fn reserved_empty_string_reuses_vm_builder_capacity() {
        let mut text = SmolStr::with_capacity(64);
        assert_eq!(text.capacity(), 64);
        text.push_str("reserved text");
        assert_eq!(text.as_str(), "reserved text");
        assert_eq!(text.capacity(), 64);
    }
}

#[cfg(test)]
mod repr_tests {
    use std::sync::Arc;

    use smallvec::smallvec;

    use super::{StructFields, StructInner, Value, VariantInner, intern_type_tag};

    #[test]
    fn repr_quotes_strings_and_chars_recursively() {
        let list = Value::Array(Arc::new(vec![
            Value::String("wow".into()),
            Value::Char('a'),
        ]));
        let variant = Value::Variant(Arc::new(VariantInner {
            name: intern_type_tag("Ok"),
            fields: smallvec![list],
        }));
        let record = Value::Struct(Arc::new(StructInner {
            name: intern_type_tag("Message"),
            fields: StructFields::new(vec![
                ("text", Value::String("hello".into())),
                ("value", variant),
            ]),
        }));

        assert_eq!(
            record.repr(),
            "Message { text: \"hello\", value: Ok([\"wow\", 'a']) }"
        );
        assert_eq!(Value::String("wow".into()).to_string(), "wow");
    }

    #[test]
    fn repr_keeps_integral_floats_source_like_recursively() {
        let record = Value::Struct(Arc::new(StructInner {
            name: intern_type_tag("Point"),
            fields: StructFields::new(vec![("x", Value::Float(1.0)), ("y", Value::Float(4.0))]),
        }));

        assert_eq!(record.repr(), "Point { x: 1.0, y: 4.0 }");
        assert_eq!(Value::Float(2.0).repr(), "2.0");
        assert_eq!(Value::Float(2.3).repr(), "2.3");
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

#[cfg(test)]
mod native_consume_tests {
    use std::sync::Arc;

    use super::{
        NativeEnumOwner, NativeEnumShape, NativeFieldKind, NativeStructShape, NativeVariantShape,
        Value, intern_type_name, intern_type_tag, native_enum_field_consume, native_shape,
        native_shape_for_variant, native_struct_shape, register_native_shapes,
        register_native_struct_shapes,
    };

    #[test]
    fn dead_type_tag_storage_is_not_retained_by_compatibility_lookup() {
        let name = "SessionOwnedTypeTagRegression";
        let tag = intern_type_tag(name);
        let weak = Arc::downgrade(&tag.0);
        drop(tag);
        assert!(weak.upgrade().is_none());
        // The sweep is amortized against table growth, so intern enough
        // distinct names to cross the watermark and prove the dead entry is
        // reclaimed rather than accumulating.
        for i in 0..(super::TYPE_TAG_SWEEP_MIN * 2 + 2) {
            let _ = intern_type_tag(&format!("SessionOwnedTypeTagSweep{i}"));
        }
        assert!(
            !super::TYPE_TAGS.lock().contains_key(name),
            "compatibility lookup retained a dead type tag"
        );
    }

    #[test]
    fn struct_instances_share_field_name_shape_but_not_values() {
        let first = Value::struct_(
            "Point",
            vec![
                (intern_type_name("x"), Value::Int(1)),
                (intern_type_name("y"), Value::Int(2)),
            ],
        );
        let second = Value::struct_(
            "Point",
            vec![
                (intern_type_name("x"), Value::Int(3)),
                (intern_type_name("y"), Value::Int(4)),
            ],
        );
        let (Value::Struct(first), Value::Struct(second)) = (first, second) else {
            unreachable!();
        };
        assert!(Arc::ptr_eq(&first.fields.names, &second.fields.names));
        assert!(matches!(first.fields[0], Value::Int(1)));
        assert!(matches!(second.fields[0], Value::Int(3)));
    }

    #[test]
    fn compatibility_shape_indices_do_not_keep_descriptors_alive() {
        let (enum_index, enum_weak) = register_native_shapes(|base| {
            let shape = Arc::new(NativeEnumShape {
                enum_name: intern_type_name("WeakCompatibilityEnum"),
                index: base,
                tagged: true,
                variants: Vec::new(),
            });
            let weak = Arc::downgrade(&shape);
            (vec![shape], (base, weak))
        });
        assert!(enum_weak.upgrade().is_none());
        assert!(native_shape(enum_index).is_none());

        let (struct_index, struct_weak) = register_native_struct_shapes(|base| {
            let shape = Arc::new(NativeStructShape {
                struct_name: intern_type_name("WeakCompatibilityStruct"),
                index: base,
                fields: Vec::new(),
                placements: Vec::new(),
                words: 0,
            });
            let weak = Arc::downgrade(&shape);
            (vec![shape], (base, weak))
        });
        assert!(struct_weak.upgrade().is_none());
        assert!(native_struct_shape(struct_index).is_none());
    }

    #[test]
    fn native_shape_cache_invalidates_when_name_becomes_ambiguous() {
        const VARIANT: &str = "ShapeCacheAmbiguousVariant";
        let first = register_native_shapes(|base| {
            let shape = Arc::new(NativeEnumShape {
                enum_name: intern_type_name("ShapeCacheFirst"),
                index: base,
                tagged: true,
                variants: vec![NativeVariantShape {
                    name: intern_type_name(VARIANT),
                    fields: Vec::new(),
                }],
            });
            (vec![Arc::clone(&shape)], shape)
        });
        let tag = intern_type_tag(VARIANT);
        assert!(Arc::ptr_eq(
            &native_shape_for_variant(tag.clone(), VARIANT).expect("initial unique shape"),
            &first
        ));

        register_native_shapes(|base| {
            let shape = Arc::new(NativeEnumShape {
                enum_name: intern_type_name("ShapeCacheSecond"),
                index: base,
                tagged: true,
                variants: vec![NativeVariantShape {
                    name: intern_type_name(VARIANT),
                    fields: Vec::new(),
                }],
            });
            (vec![shape], ())
        });
        assert!(
            native_shape_for_variant(tag, VARIANT).is_none(),
            "registration generation must invalidate the cached unique shape"
        );
    }

    #[test]
    fn consuming_unique_native_child_transfers_without_retain() {
        let (child_shape, parent_shape) = register_native_shapes(|base| {
            let child = Arc::new(NativeEnumShape {
                enum_name: intern_type_name("ConsumeChild"),
                index: base,
                tagged: false,
                variants: vec![NativeVariantShape {
                    name: intern_type_name("ConsumeLeaf"),
                    fields: vec![NativeFieldKind::I64],
                }],
            });
            let parent = Arc::new(NativeEnumShape {
                enum_name: intern_type_name("ConsumeParent"),
                index: base + 1,
                tagged: false,
                variants: vec![NativeVariantShape {
                    name: intern_type_name("ConsumeNode"),
                    fields: vec![NativeFieldKind::Enum(base)],
                }],
            });
            (
                vec![Arc::clone(&child), Arc::clone(&parent)],
                (child, parent),
            )
        });

        // Null metadata is sufficient here: the test explicitly transfers the
        // only child slot before either allocation drops.
        let child = unsafe { gossamer_runtime::c_abi::gos_rt_rc_alloc(8, std::ptr::null()) };
        let parent = unsafe { gossamer_runtime::c_abi::gos_rt_rc_alloc(8, std::ptr::null()) };
        assert!(!child.is_null() && !parent.is_null());
        unsafe {
            *((child as usize - 3) as *mut u8) = 0;
            child.cast::<i64>().write_unaligned(7);
            *((parent as usize - 3) as *mut u8) = 0;
            parent.cast::<i64>().write_unaligned(child as i64);
        }
        let before = unsafe { gossamer_runtime::c_abi::gos_rt_rc_strong_count(child) };
        let mut owner = NativeEnumOwner {
            ptr: parent as usize,
            shape: Arc::clone(&parent_shape),
            owned: false,
        };
        let moved = native_enum_field_consume(&mut owner, 0).expect("unique child moves");
        assert_eq!(
            unsafe { parent.cast::<i64>().read_unaligned() },
            0,
            "parent slot cleared"
        );
        let Value::NativeEnum(child_owner) = moved else {
            panic!("consume returned non-enum")
        };
        assert_eq!(child_owner.ptr, child as usize);
        assert!(Arc::ptr_eq(&child_owner.shape, &child_shape));
        assert_eq!(
            unsafe { gossamer_runtime::c_abi::gos_rt_rc_strong_count(child) },
            before,
            "moving a child must not retain it"
        );
        drop(child_owner);
        drop(owner);
    }
}

/// The render descriptor for `ty`, or `None` when every value of that
/// type renders the way it always has.
///
/// The descriptor is what carries a static type into a renderer that
/// only sees values: a `Vec` and a fixed array share one runtime
/// representation, and a `u64` and an `i64` share one slot, so the
/// spelling and the signedness are knowable only from here. One
/// builder serves the bytecode compiler's format sites and the REPL,
/// so a value reads the same in both.
#[must_use]
pub fn render_descriptor(tcx: &gossamer_types::TyCtxt, ty: gossamer_types::Ty) -> Option<String> {
    descriptor_of(tcx, ty, false)
}

/// The ordering descriptor for `ty`: where the type declared an integer `u64`
/// / `usize`, whose bits order unsigned, at any depth - sequence elements,
/// tuple and struct fields, carrier payloads. `None` when it declared none,
/// so every value of the type orders as the signed words the VM compares.
///
/// The shape is the render descriptor's: [`uint_leaves`] walks a value
/// alongside it, and the re-boxed copy is what an ordering compares.
#[must_use]
pub fn ordering_descriptor(tcx: &gossamer_types::TyCtxt, ty: gossamer_types::Ty) -> Option<String> {
    let mut out = Vec::new();
    push_render_desc_with(tcx, ty, &mut out, 0, true, &[]);
    out.contains(&uint_desc::UINT)
        .then(|| out.iter().map(|b| *b as char).collect())
}

/// [`render_descriptor`] that also describes a struct's fields, for a
/// value the REPL renders from the value alone rather than through the
/// `to_string` a program's format site calls.
#[must_use]
pub fn repl_render_descriptor(
    tcx: &gossamer_types::TyCtxt,
    ty: gossamer_types::Ty,
) -> Option<String> {
    descriptor_of(tcx, ty, true)
}

fn descriptor_of(
    tcx: &gossamer_types::TyCtxt,
    ty: gossamer_types::Ty,
    adts: bool,
) -> Option<String> {
    let mut out = Vec::new();
    push_render_desc_with(tcx, ty, &mut out, 0, adts, &[]);
    out.iter()
        .any(|b| *b == uint_desc::UINT || *b == uint_desc::SET || *b == uint_desc::VEC)
        .then(|| out.iter().map(|b| *b as char).collect())
}

/// [`render_descriptor`] for a sequence whose ELEMENTS are rendered
/// while the sequence itself is not - what `xs.join(sep)` builds, since
/// the separator replaces the brackets. The outer `Vec` tag becomes the
/// bare-sequence one; every nested descriptor is unchanged.
#[must_use]
pub fn element_render_descriptor(
    tcx: &gossamer_types::TyCtxt,
    ty: gossamer_types::Ty,
) -> Option<String> {
    let mut out = Vec::new();
    push_render_desc(tcx, ty, &mut out, 0);
    if out.first() != Some(&uint_desc::VEC) {
        return None;
    }
    out[0] = uint_desc::SEQ;
    out.iter()
        .any(|b| *b == uint_desc::UINT || *b == uint_desc::SET || *b == uint_desc::VEC)
        .then(|| out.iter().map(|b| *b as char).collect())
}

/// `ty` with every reference peeled.
fn peel_refs(tcx: &gossamer_types::TyCtxt, mut ty: gossamer_types::Ty) -> gossamer_types::Ty {
    while let Some(gossamer_types::TyKind::Ref { inner, .. }) = tcx.kind(ty) {
        ty = *inner;
    }
    ty
}

fn is_unsigned64(tcx: &gossamer_types::TyCtxt, ty: gossamer_types::Ty) -> bool {
    matches!(
        tcx.kind(peel_refs(tcx, ty)),
        Some(gossamer_types::TyKind::Int(
            gossamer_types::IntTy::U64 | gossamer_types::IntTy::Usize
        ))
    )
}

fn push_render_desc(
    tcx: &gossamer_types::TyCtxt,
    ty: gossamer_types::Ty,
    out: &mut Vec<u8>,
    depth: u8,
) {
    push_render_desc_with(tcx, ty, out, depth, false, &[]);
}

fn push_render_desc_with(
    tcx: &gossamer_types::TyCtxt,
    ty: gossamer_types::Ty,
    out: &mut Vec<u8>,
    depth: u8,
    adts: bool,
    params: &[gossamer_types::Ty],
) {
    use gossamer_types::TyKind;
    if depth > 8 {
        out.push(uint_desc::NONE);
        return;
    }
    let peeled = peel_refs(tcx, ty);
    if is_unsigned64(tcx, peeled) {
        out.push(uint_desc::UINT);
        return;
    }
    match tcx.kind(peeled) {
        Some(TyKind::Param { idx, .. }) => match params.get(idx.0 as usize) {
            Some(arg) => push_render_desc_with(tcx, *arg, out, depth + 1, adts, &[]),
            None => out.push(uint_desc::NONE),
        },
        // A `Vec` renders in its own spelling; a fixed array and a slice
        // are written in bare brackets and render that way.
        Some(TyKind::Vec(elem)) => {
            let elem = *elem;
            out.push(uint_desc::VEC);
            push_render_desc_with(tcx, elem, out, depth + 1, adts, params);
        }
        Some(TyKind::Slice(elem) | TyKind::Array { elem, .. }) => {
            let elem = *elem;
            out.push(uint_desc::SEQ);
            push_render_desc_with(tcx, elem, out, depth + 1, adts, params);
        }
        Some(TyKind::Tuple(elems)) => {
            let elems = elems.clone();
            let Ok(arity) = u8::try_from(elems.len()) else {
                out.push(uint_desc::NONE);
                return;
            };
            out.push(uint_desc::TUPLE);
            out.push(arity);
            for elem in elems {
                push_render_desc_with(tcx, elem, out, depth + 1, adts, params);
            }
        }
        Some(TyKind::HashMap { key, value, .. }) => {
            let (key, value) = (*key, *value);
            out.push(uint_desc::MAP);
            push_render_desc_with(tcx, key, out, depth + 1, adts, params);
            push_render_desc_with(tcx, value, out, depth + 1, adts, params);
        }
        // `Option` and `Result` are the sentinel Adts `u32::MAX - 1` and
        // `u32::MAX`; a `Set` / `BTreeSet` is `u32::MAX - 7` / `- 18`.
        Some(TyKind::Adt { def, substs }) if def.local == u32::MAX - 1 => {
            let payload = substs.types().first().copied();
            out.push(uint_desc::OPTION);
            match payload {
                Some(payload) => push_render_desc_with(tcx, payload, out, depth + 1, adts, params),
                None => out.push(uint_desc::NONE),
            }
        }
        Some(TyKind::Adt { def, substs }) if def.local == u32::MAX => {
            let tys = substs.types();
            let (ok, err) = (tys.first().copied(), tys.get(1).copied());
            out.push(uint_desc::RESULT);
            for arm in [ok, err] {
                match arm {
                    Some(arm) => push_render_desc_with(tcx, arm, out, depth + 1, adts, params),
                    None => out.push(uint_desc::NONE),
                }
            }
        }
        // `Deque` (-19), `MaxHeap` (-28), `MinHeap` (-30), `Queue` (-31),
        // and `Stack` (-32) keep their elements in a runtime registry, so
        // the descriptor travels on the handle.
        Some(TyKind::Adt { def, substs })
            if matches!(u32::MAX - def.local, 19 | 28 | 30 | 31 | 32) =>
        {
            let elem = substs.types().first().copied();
            out.push(uint_desc::CONTAINER);
            match elem {
                Some(elem) => push_render_desc_with(tcx, elem, out, depth + 1, adts, params),
                None => out.push(uint_desc::NONE),
            }
        }
        Some(TyKind::Adt { def, substs })
            if def.local == u32::MAX - 7 || def.local == u32::MAX - 18 =>
        {
            let elem = substs.types().first().copied();
            push_set_render_desc(tcx, elem, out, depth, adts, params);
        }
        // A program renders a struct through the `to_string` synthesized for
        // its type, which describes each field at the format site inside it.
        // A field declared with a type parameter is the exception: that site
        // sees only the parameter, so the instantiated type is described here,
        // where the concrete type is known. The REPL describes every field,
        // since it renders from the value alone.
        Some(TyKind::Adt { def, substs }) if def.local < u32::MAX - 16 => {
            let (def, substs) = (*def, substs.clone());
            push_struct_render_desc(tcx, def, &substs, out, depth, adts, params);
        }
        _ => out.push(uint_desc::NONE),
    }
}

/// The descriptor of a `Set` / `BTreeSet` whose element is `elem`: the set tag
/// and the element's own descriptor when that element describes anything, and
/// nothing otherwise.
fn push_set_render_desc(
    tcx: &gossamer_types::TyCtxt,
    elem: Option<gossamer_types::Ty>,
    out: &mut Vec<u8>,
    depth: u8,
    adts: bool,
    params: &[gossamer_types::Ty],
) {
    let mut elem_desc = Vec::new();
    match elem {
        Some(elem) => push_render_desc_with(tcx, elem, &mut elem_desc, depth + 1, adts, params),
        None => elem_desc.push(uint_desc::NONE),
    }
    if elem_desc.iter().any(|b| *b != uint_desc::NONE) {
        out.push(uint_desc::SET);
        out.extend(elem_desc);
    } else {
        out.push(uint_desc::NONE);
    }
}

/// The descriptor of the user struct `def` instantiated with `substs`: every
/// field for the REPL, and otherwise only the fields declared with a type
/// parameter.
fn push_struct_render_desc(
    tcx: &gossamer_types::TyCtxt,
    def: gossamer_resolve::DefId,
    substs: &gossamer_types::Substs,
    out: &mut Vec<u8>,
    depth: u8,
    adts: bool,
    params: &[gossamer_types::Ty],
) {
    use gossamer_types::TyKind;
    // A field's declared type names the struct's own parameters, so it reads
    // them from this instantiation's arguments, each already resolved in the
    // enclosing one.
    let args: Vec<gossamer_types::Ty> = substs
        .types()
        .iter()
        .map(|arg| match tcx.kind(*arg) {
            Some(TyKind::Param { idx, .. }) => params.get(idx.0 as usize).copied().unwrap_or(*arg),
            _ => *arg,
        })
        .collect();
    let declared = tcx.struct_field_tys(def).map(<[_]>::to_vec);
    let Some(fields) = tcx.adt_field_tys(def, substs).map(<[_]>::to_vec) else {
        out.push(uint_desc::NONE);
        return;
    };
    if !adts && substs.is_empty() {
        out.push(uint_desc::NONE);
        return;
    }
    let Ok(count) = u8::try_from(fields.len()) else {
        out.push(uint_desc::NONE);
        return;
    };
    out.push(uint_desc::ADT);
    out.push(count);
    for (index, field) in fields.into_iter().enumerate() {
        let generic = declared
            .as_ref()
            .and_then(|declared| declared.get(index))
            .is_some_and(|declared| crate::compile::mentions_param(tcx, *declared));
        if adts || generic {
            push_render_desc_with(tcx, field, out, depth + 1, adts, &args);
        } else {
            out.push(uint_desc::NONE);
        }
    }
}
