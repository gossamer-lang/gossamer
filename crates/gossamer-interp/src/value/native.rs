//! The global heap side table and the shapes of native enums and structs that cross the JIT boundary.

use super::{
    Channel, Closure, FloatArrayInner, PackedBytes, SmolStr, StructInner, TypeTag, Value,
    VariantInner,
};

use std::sync::Arc;

use parking_lot::Mutex;
use smallvec::SmallVec;

// ------------------------------------------------------------------
// Global heap side table
//
// Heap-backed `Value` variants are registered here before being
// encoded as `TAG_HEAP` u64 words.

/// One heap-allocated payload stored in the global side table.
#[derive(Clone)]
pub(super) enum RegistryEntry {
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
pub(super) fn register_heap(entry: RegistryEntry) -> u32 {
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
pub(super) fn take_heap(handle: u32) -> Option<RegistryEntry> {
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
    let last = unsafe { rt::rc_strong_count(base as *mut u8) } <= 1;
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
        let rc = unsafe { rt::rc_strong_count(*base as *mut u8) };
        for _ in 0..rc.max(0) {
            // Re-check before each release so a node already driven to zero by
            // an earlier iteration (a shared node reached along two paths whose
            // count this teardown already drained) is never released past zero
            // into freed memory.
            if unsafe { rt::rc_strong_count(*base as *mut u8) } <= 0 {
                break;
            }
            // SAFETY: exclusively owned and count still positive.
            unsafe { rt::rc_release_no_buffer(*base as *mut u8) };
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
    let unique = unsafe { gossamer_runtime::c_abi::rc_strong_count(base as *mut u8) == 1 };
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
mod native_consume_tests {
    use std::sync::Arc;

    use crate::value::{intern_type_name, intern_type_tag};

    use super::{
        NativeEnumOwner, NativeEnumShape, NativeFieldKind, NativeStructShape, NativeVariantShape,
        Value, native_enum_field_consume, native_shape, native_shape_for_variant,
        native_struct_shape, register_native_shapes, register_native_struct_shapes,
    };

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
        let before = unsafe { gossamer_runtime::c_abi::rc_strong_count(child) };
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
            unsafe { gossamer_runtime::c_abi::rc_strong_count(child) },
            before,
            "moving a child must not retain it"
        );
        drop(child_owner);
        drop(owner);
    }
}
