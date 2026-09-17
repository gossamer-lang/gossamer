#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::similar_names)]
#![allow(clippy::many_single_char_names)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::same_length_and_capacity)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::cast_ptr_alignment)]
#![allow(clippy::ptr_as_ptr)]
#![allow(static_mut_refs)]
#![allow(unused_unsafe)]
#![allow(clippy::wildcard_imports)]

use super::*;

// ---------------------------------------------------------------
// Vec runtime - a `{ elem_bytes, len, cap, ptr }` struct
// ---------------------------------------------------------------

// A vector whose element kind says STRING holds strings this runtime
// allocated, so its teardown frees them through the typed entry point: the
// untyped one re-derives what the kind already records, by asking the
// allocator whether the bytes in front of each body may be read.

/// Element kind tag carried in the `GosVec` header so
/// `gos_rt_vec_free` can free element payloads instead of just the
/// backing byte buffer. Default `0` (primitive) preserves the
/// shallow-free behaviour every existing call site assumes; typed
/// vecs created via `gos_rt_vec_new_typed` opt in to deep free.
///
/// Encoding is deliberately small (one byte) so the field fits in
/// the existing 4-byte padding between `elem_bytes` (u32) and
/// `ptr` (8-byte aligned pointer). Adding it does not change the
/// struct size, the offset of `ptr`, or the offset of `len` - all
/// of which the codegen reads at fixed offsets.
pub mod vec_elem_kind {
    /// Element payload is a primitive value owning no other heap
    /// memory (i64, f64, u8, bool, etc.). Shallow free of the
    /// backing buffer is correct.
    pub const PRIMITIVE: u8 = 0;
    /// Element is a `*mut c_char` cstring; each element is freed
    /// via `gos_rt_str_free` before the buffer itself is reclaimed.
    pub const STRING: u8 = 1;
    /// Element is a `*mut GosVec`; each element is recursively
    /// freed via `gos_rt_vec_free`.
    pub const VEC: u8 = 2;
    /// Element is a `*mut GosMap`; each element is freed via
    /// `gos_rt_map_free`.
    pub const MAP: u8 = 3;
    /// Element is a `*mut GosError`; each element is freed via
    /// `gos_rt_error_free`.
    pub const ERROR: u8 = 4;
    /// Element is a multi-slot struct/tuple stored inline whose option
    /// payload words may hold copy-blob pointers. The per-type guarded
    /// meta lives in the Vec's versioned owner carrier; `gos_rt_vec_free`
    /// releases each element's guarded children, and `gos_rt_vec_push`
    /// retains them when the element bytes are copied in.
    pub const AGGR_GUARDED: u8 = 5;
    /// Element is a multi-slot struct/tuple stored inline whose slots
    /// embed heap pointers (runtime strings / nested vecs) OWNED by the
    /// vec. The per-vec slot layout lives in the versioned owner carrier
    /// ([`super::vec::vec_slot_children`]); `gos_rt_vec_free` deep-frees
    /// each live child even when the vec was never iterated (the
    /// early-`break` path), and `gos_rt_vec_push` retains the copied
    /// slot's children. Set via [`super::vec::vec_set_slot_children`] by
    /// runtime materializer shims - never by codegen.
    pub const AGGR_OWNED: u8 = 6;
    /// Slot-child kind (NOT a vec-level `elem_kind`): the slot word holds
    /// a reference-counted heap node - a user enum or struct pointer
    /// (possibly tag-bit-encoded) - retained via `gos_rt_rc_retain` and
    /// released via `gos_rt_rc_release`, both of which mask the low tag
    /// bits and walk the node's own child meta. Used inside `AGGR_OWNED`
    /// slot-children layouts for an aggregate element's enum/struct
    /// fields.
    pub const RC_NODE: u8 = 7;
    /// Element is a reference-counted heap node pointer (a payload-bearing
    /// user enum, possibly tag-bit-encoded) OWNED by the vec: a push moves
    /// the frame's share in (the MIR treats `gos_rt_vec_push` as consuming,
    /// same as `STRING` elements), `gos_rt_vec_free` releases each element
    /// via `gos_rt_rc_release`, and a storage duplication (clone / slice)
    /// retains each copied element so both storages own their shares. Set
    /// via [`super::vec::gos_rt_vec_mark_rc_elems`], emitted by the MIR
    /// lowering right after constructing a vec of payload-enum elements.
    pub const RC_ENUM: u8 = 8;
    /// Element is an aggregate - a struct, tuple, or fixed array - held
    /// inline in the storage whose slots own no heap children, and which
    /// occupies a single slot. Its slot address is the value, so a read
    /// that hands the element to the program copies the slot block out;
    /// the width alone cannot say this, since a one-word scalar element
    /// is the value itself. Shallow free of the buffer is correct.
    pub const AGGR_FLAT: u8 = 10;
    /// Slot-child kind (NOT a vec-level `elem_kind`): the slot word holds
    /// a `*mut GosSet`. A `GosSet` carries no count of its holders, so a
    /// copied slot takes a table of its own via `gos_rt_set_clone` and the
    /// vec's teardown frees the one it holds via `gos_rt_set_free` - the
    /// same contract [`MAP`] carries inside an `AGGR_OWNED` layout.
    pub const SET: u8 = 11;
    /// Element is a `*mut GosJson` handle into a parsed document. Each handle
    /// holds a share of the document's tree, so `gos_rt_vec_free` gives every
    /// one back - without it a vector of children keeps the whole document
    /// alive for as long as the program runs.
    pub const JSON: u8 = 12;
}

#[repr(C)]
pub struct GosVec {
    pub len: i64,
    pub cap: i64,
    pub elem_bytes: u32,
    /// Element-kind tag (see [`vec_elem_kind`]) so `gos_rt_vec_free`
    /// can deep-free pointer-bearing element types. Sits in the
    /// padding before `ptr` so the struct layout (size, ptr offset,
    /// len offset) is unchanged from prior 0.5 releases.
    pub elem_kind: u8,
    /// Header flags (was `_reserved[0]`, struct offset 21), a small
    /// bitfield: `VEC_REGION_FLAG` when this header and its buffer live
    /// in an arena slab (so `gos_rt_vec_free` skips them), and
    /// `VEC_SPLIT_FLAG` when `ptr` points at a separately-allocated
    /// buffer rather than the inline one that rides with the header (so
    /// `gos_rt_vec_free` knows to reclaim that buffer on its own). The
    /// two are mutually exclusive: region vecs never split.
    pub region_flag: u8,
    /// Strong refcount of a non-region Vec - an actual `AtomicU16` so its
    /// atomic loads/stores are sound. (A prior `[u8; 3]` reinterpret-cast to
    /// `AtomicU16` tripped Miri's Stacked-Borrows model: a read-only,
    /// single-byte-provenance pointer cannot be retagged for atomic
    /// read-write access.) Same offset (22) and size as the bytes it
    /// replaces, so `ptr` stays at offset 24 and the layout is unchanged.
    pub rc: std::sync::atomic::AtomicU16,
    pub ptr: SyncRawPtr<u8>,
    /// Unique allocation identity, deliberately appended after the legacy
    /// 32-byte prefix whose offsets are part of the native ABI. Heap Vecs get
    /// this directly in the header; region Vecs are bulk-owned and use zero.
    pub generation: u64,
    /// Guarded-aggregate element metadata. Primitive Vecs keep this null, so
    /// their common path carries no separately allocated ownership state.
    pub elem_meta: SyncRawPtr<i64>,
    /// Lazily allocated metadata only for aggregate-owned element layouts.
    /// This remains an owned carrier rather than an address-keyed registry.
    pub owner: SyncRawPtr<VecOwner>,
    /// Incremented by every structural mutation. Appended after the existing
    /// ABI fields so their offsets remain stable. Lazy borrowed iterators
    /// snapshot it independently from allocation identity.
    pub mutation_generation: u64,
}

/// ABI-versioned optional ownership state for aggregate-owned Vec elements.
///
/// Unlike the former address-keyed metadata maps, this allocation is owned by
/// the Vec header and dies with it. Primitive and guarded-aggregate Vecs do
/// not allocate it; their identity and guarded metadata are in [`GosVec`].
#[repr(C)]
pub struct VecOwner {
    abi_version: u16,
    kind: u16,
    destructor: u32,
    slot_children: Option<Box<[VecSlotChild]>>,
}

const ABI_OWNER_VERSION: u16 = 1;
const ABI_OWNER_KIND_VEC: u16 = 1;
const ABI_OWNER_DTOR_VEC: u32 = 1;
static NEXT_VEC_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Identities a goroutine claims in one go, so a vector's allocation identity
/// stays unique across the process without a read-modify-write on every
/// construction.
const VEC_GENERATION_BLOCK: u64 = 1 << 16;

thread_local! {
    /// The identities this thread still holds: the next one to hand out and
    /// the end of its claimed block.
    static VEC_GENERATIONS: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) };
}

fn next_vec_generation() -> u64 {
    VEC_GENERATIONS.with(|held| {
        let (next, end) = held.get();
        if next < end {
            held.set((next + 1, end));
            return next;
        }
        let base = NEXT_VEC_GENERATION
            .fetch_add(VEC_GENERATION_BLOCK, std::sync::atomic::Ordering::Relaxed);
        held.set((base + 1, base + VEC_GENERATION_BLOCK));
        base
    })
}

fn new_vec_owner() -> SyncRawPtr<VecOwner> {
    let owner = Box::new(VecOwner {
        abi_version: ABI_OWNER_VERSION,
        kind: ABI_OWNER_KIND_VEC,
        destructor: ABI_OWNER_DTOR_VEC,
        slot_children: None,
    });
    let owner = Box::into_raw(owner);
    crate::c_abi::ledger::vec_owner_alloc(
        std::mem::size_of::<VecOwner>(),
        allocator_usable_bytes(owner.cast(), std::mem::size_of::<VecOwner>()),
    );
    SyncRawPtr::new(owner)
}

/// Allocation capacity returned by the allocator that owns runtime Vec
/// storage. The runtime deliberately uses the system allocator under TSan,
/// Miri, fuzzing, and wasm, where a mimalloc query would be invalid; those
/// configurations report the exact requested layout size instead.
#[inline]
fn allocator_usable_bytes(ptr: *const u8, requested: usize) -> usize {
    #[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
    {
        let _ = requested;
        // SAFETY: `ptr` was just returned by the process-wide mimalloc-backed
        // global allocator and remains live for this query.
        unsafe { libmimalloc_sys::mi_usable_size(ptr.cast_mut().cast()) }
    }
    #[cfg(any(tsan, miri, fuzzing, target_arch = "wasm32"))]
    {
        let _ = ptr;
        requested
    }
}

#[inline]
fn vec_owner(v: &GosVec) -> Option<&VecOwner> {
    let owner = v.owner.as_ptr();
    if owner.is_null() {
        return None;
    }
    // SAFETY: a non-region Vec owner is created with its header and reclaimed
    // only after the header's last release.
    let owner = unsafe { &*owner };
    (owner.abi_version == ABI_OWNER_VERSION
        && owner.kind == ABI_OWNER_KIND_VEC
        && owner.destructor == ABI_OWNER_DTOR_VEC)
        .then_some(owner)
}

/// Generation of this Vec allocation, or zero for a region Vec. This is a
/// diagnostic/testing hook; it is stored independently of header address.
#[must_use]
pub fn vec_owner_generation(v: &GosVec) -> u64 {
    v.generation
}

/// Mark a structural mutation of a live Vec header.
#[inline]
pub(crate) fn bump_vec_mutation_generation(v: &mut GosVec) {
    v.mutation_generation = v.mutation_generation.wrapping_add(1);
}

#[inline]
fn vec_owner_mut(v: &mut GosVec) -> Option<&mut VecOwner> {
    let owner = v.owner.as_ptr();
    if owner.is_null() {
        return None;
    }
    // SAFETY: callers hold exclusive access while attaching metadata.
    let owner = unsafe { &mut *owner };
    (owner.abi_version == ABI_OWNER_VERSION
        && owner.kind == ABI_OWNER_KIND_VEC
        && owner.destructor == ABI_OWNER_DTOR_VEC)
        .then_some(owner)
}

/// Returns the lazily-created aggregate-owned metadata carrier. The header
/// holds generation and guarded metadata directly, so this allocation is only
/// paid by Vecs that need a dynamic slot-child layout.
fn ensure_vec_owner(v: &mut GosVec) -> &mut VecOwner {
    if v.owner.is_null() {
        v.owner = new_vec_owner();
    }
    vec_owner_mut(v).expect("fresh Vec owner must use the current ABI")
}

pub(crate) unsafe fn drop_vec_owner(v: &mut GosVec) {
    let owner = v.owner.as_ptr();
    if owner.is_null() {
        return;
    }
    v.owner = SyncRawPtr::NULL;
    // SAFETY: final Vec release owns the carrier exactly once.
    drop(unsafe { Box::from_raw(owner) });
}

/// `region_flag` bit marking a GosVec (and its backing buffer) as
/// arena-region-allocated: both live in region slabs, so `gos_rt_vec_free`
/// must skip them (they are freed wholesale at `arena_pop`).
const VEC_REGION_FLAG: u8 = 1;

/// `region_flag` bit marking a non-region GosVec whose `ptr` points at a
/// separately-allocated element buffer (grown past the inline capacity)
/// rather than the inline buffer that rides with the header. Set on the
/// first grow, or at construction for a capacity larger than the inline
/// buffer holds. `gos_rt_vec_free` reclaims that separate buffer only for
/// a split vec; an inline vec's buffer is freed with the header block.
const VEC_SPLIT_FLAG: u8 = 2;

/// Header was allocated as `Box<GosVec>` without an unused inline buffer.
const VEC_COMPACT_HEADER_FLAG: u8 = 8;

/// Element words held inline, immediately after the [`GosVec`] header, in a
/// single [`InlineVec`] allocation. Six words is the selected default from the
/// audited game/stress matrix; `inline-vec-words-8` remains available for
/// explicit A/B checks and wins when Cargo's `--all-features` also enables the
/// compatibility `inline-vec-words-6` flag. The trailing buffer is private to
/// this runtime allocation, while `GosVec`'s stable native ABI header remains
/// unchanged.
#[cfg(feature = "inline-vec-words-8")]
const INLINE_BUF_WORDS: usize = 8;
#[cfg(not(feature = "inline-vec-words-8"))]
const INLINE_BUF_WORDS: usize = 6;
const INLINE_BUF_BYTES: usize = INLINE_BUF_WORDS * 8;

/// A non-region `GosVec` header physically followed by its initial element
/// buffer, allocated as one `Box<InlineVec>`. `repr(C)` keeps `header` at
/// offset 0 so a `*mut GosVec` and a `*mut InlineVec` share an address and
/// every codegen path that reads header fields at fixed offsets is
/// unaffected. While the element count stays within the inline buffer,
/// `header.ptr` points into `buf` (one allocation, header and first
/// elements in one cache line); a push past the inline capacity moves `ptr`
/// to a separately-allocated buffer and sets [`VEC_SPLIT_FLAG`], after which
/// `buf` is unused space reclaimed with the header.
#[repr(C)]
pub(crate) struct InlineVec {
    pub(crate) header: GosVec,
    buf: [u64; INLINE_BUF_WORDS],
}

/// Inline element capacity for a given element width (how many elements of
/// `elem_bytes` fit in [`INLINE_BUF_BYTES`]). Zero for a zero or oversized
/// element width, which then always uses a separate buffer.
#[inline]
fn inline_cap(elem_bytes: u32) -> i64 {
    if elem_bytes == 0 || elem_bytes as usize > INLINE_BUF_BYTES {
        0
    } else {
        (INLINE_BUF_BYTES / elem_bytes as usize) as i64
    }
}

/// Allocates a non-region `GosVec` as a single `Box<InlineVec>` (header +
/// contiguous inline element buffer). When `cap` fits the inline buffer,
/// `header.ptr` points into it and the vec is not split; otherwise a
/// separate `cap * elem_bytes` buffer is allocated, `header.ptr` points at
/// it, and [`VEC_SPLIT_FLAG`] is set. `len` initialises the header length;
/// the data region is left zeroed for the caller to fill. Increments the
/// vec ledger and sets the strong count to 1, symmetric with
/// [`super::map::gos_rt_vec_free`].
pub(crate) unsafe fn alloc_box_vec(
    elem_bytes: u32,
    elem_kind: u8,
    cap: i64,
    len: i64,
) -> *mut GosVec {
    let cap = cap.max(len).max(0);
    let icap = inline_cap(elem_bytes);
    let (init_ptr, real_cap, flag) = if cap <= icap {
        (SyncRawPtr::NULL, icap, 0u8)
    } else {
        let bytes = checked_buffer_bytes(cap as usize, elem_bytes as usize);
        (
            SyncRawPtr::new(alloc_vec_buffer(bytes)),
            cap,
            VEC_SPLIT_FLAG,
        )
    };
    crate::c_abi::ledger::vec_inc();
    if flag != 0 {
        let boxed = Box::new(GosVec {
            len,
            cap: real_cap,
            elem_bytes,
            elem_kind,
            region_flag: flag | VEC_COMPACT_HEADER_FLAG,
            rc: std::sync::atomic::AtomicU16::new(1),
            ptr: init_ptr,
            generation: next_vec_generation(),
            mutation_generation: 0,
            elem_meta: SyncRawPtr::NULL,
            owner: SyncRawPtr::NULL,
        });
        let boxed_ptr = Box::into_raw(boxed);
        crate::c_abi::ledger::vec_inline_alloc(
            std::mem::size_of::<GosVec>(),
            allocator_usable_bytes(boxed_ptr.cast(), std::mem::size_of::<GosVec>()),
        );
        crate::c_abi::ledger::vec_split_alloc(
            checked_buffer_bytes(cap as usize, elem_bytes as usize),
            allocator_usable_bytes(
                init_ptr.as_const_ptr(),
                checked_buffer_bytes(cap as usize, elem_bytes as usize),
            ),
        );
        return boxed_ptr;
    }
    let boxed = Box::new(InlineVec {
        header: GosVec {
            len,
            cap: real_cap,
            elem_bytes,
            elem_kind,
            region_flag: flag,
            rc: std::sync::atomic::AtomicU16::new(1),
            ptr: init_ptr,
            generation: next_vec_generation(),
            mutation_generation: 0,
            elem_meta: SyncRawPtr::NULL,
            owner: SyncRawPtr::NULL,
        },
        buf: [0u64; INLINE_BUF_WORDS],
    });
    let boxed_ptr = Box::into_raw(boxed);
    crate::c_abi::ledger::vec_inline_alloc(
        std::mem::size_of::<InlineVec>(),
        allocator_usable_bytes(boxed_ptr.cast(), std::mem::size_of::<InlineVec>()),
    );
    if flag != 0 {
        crate::c_abi::ledger::vec_split_alloc(
            checked_buffer_bytes(cap as usize, elem_bytes as usize),
            allocator_usable_bytes(
                init_ptr.as_const_ptr(),
                checked_buffer_bytes(cap as usize, elem_bytes as usize),
            ),
        );
    }
    if flag == 0 {
        // Inline: point `header.ptr` at this block's own contiguous buffer.
        // The self-pointer is derived from the raw allocation after
        // `into_raw`, not from a `&mut boxed.buf` borrow taken before it, so
        // its provenance spans the whole block and stays live across every
        // later `&mut *boxed_ptr` reborrow of the header. A borrow taken
        // before `into_raw` is narrower than the allocation and would be
        // invalidated when `into_raw` reasserts uniqueness over it.
        unsafe {
            let bufptr = (&raw mut (*boxed_ptr).buf).cast::<u8>();
            (*boxed_ptr).header.ptr = SyncRawPtr::new(bufptr);
        }
    }
    boxed_ptr.cast::<GosVec>()
}

/// True when this non-region GosVec's `ptr` is a separately-allocated
/// buffer (grown past the inline capacity) rather than the inline buffer
/// carried in the [`InlineVec`] header block. `gos_rt_vec_free` reclaims
/// that separate buffer only for a split vec.
#[inline]
pub(crate) fn vec_is_split(v: &GosVec) -> bool {
    v.region_flag & VEC_SPLIT_FLAG != 0
}

#[inline]
pub(crate) fn vec_has_compact_header(v: &GosVec) -> bool {
    v.region_flag & VEC_COMPACT_HEADER_FLAG != 0
}

pub(crate) unsafe fn consume_byte_vec<R>(v: *mut GosVec, f: impl FnOnce(&[u8]) -> R) -> R {
    let vec = unsafe { &mut *v };
    let bytes = if vec.elem_bytes == 1 && vec.len > 0 && !vec.ptr.is_null() {
        unsafe { std::slice::from_raw_parts(vec.ptr.as_ptr(), vec.len as usize) }
    } else {
        &[]
    };
    let result = f(bytes);
    let _ = u32::try_from(bytes.len()).unwrap_or_else(|_| {
        crate::c_abi::panic::panic_text("byte vector is too large to store");
        0
    });
    // A container that keeps the BYTES rather than the handle needs no share
    // of the vector at all, so none is given back here: the caller's own
    // release, at the end of the scope that built the value, is the only one.
    result
}

pub(crate) unsafe fn consume_byte_vec_preserving_source<R>(
    v: *mut GosVec,
    f: impl FnOnce(&[u8]) -> R,
) -> R {
    let vec = unsafe { &mut *v };
    let bytes = if vec.elem_bytes == 1 && vec.len > 0 && !vec.ptr.is_null() {
        unsafe { std::slice::from_raw_parts(vec.ptr.as_ptr(), vec.len as usize) }
    } else {
        &[]
    };
    let result = f(bytes);
    let _ = u32::try_from(bytes.len()).unwrap_or_else(|_| {
        crate::c_abi::panic::panic_text("byte vector is too large to store");
        0
    });
    // As above: the bytes are what the container keeps, so the source's own
    // holders are the only ones, and this takes none of their shares.
    result
}

/// Allocate a GosVec header from the active region if one is open (so it is
/// freed wholesale at pop and `gos_rt_vec_free` skips it), else from the
/// global allocator as a single `Box<InlineVec>` via [`alloc_box_vec`].
unsafe fn alloc_vec_header(mut v: GosVec) -> *mut GosVec {
    let p = crate::c_abi::rc::region_alloc_bytes(std::mem::size_of::<GosVec>());
    if p.is_null() {
        unsafe { alloc_box_vec(v.elem_bytes, v.elem_kind, v.cap, v.len) }
    } else {
        crate::c_abi::ledger::vec_region_alloc(std::mem::size_of::<GosVec>());
        v.region_flag = VEC_REGION_FLAG;
        let hp = p.cast::<GosVec>();
        unsafe { std::ptr::write(hp, v) };
        hp
    }
}

/// Constructs a Vec with a requested capacity while preserving the active
/// region allocation path.  `alloc_box_vec` is deliberately the non-region
/// implementation (it uses a boxed inline header), so calling it directly
/// from `with_capacity` bypasses the arena even when the caller's loop is
/// regioned.  Start from the ordinary region-aware empty constructor, then
/// reserve the requested capacity through the shared growth path.
unsafe fn alloc_vec_with_capacity(elem_bytes: u32, elem_kind: u8, cap: i64) -> *mut GosVec {
    if cap < 0 {
        crate::c_abi::panic::panic_text("Vec::with_capacity: capacity must be non-negative");
    }
    if !crate::c_abi::rc::region_is_active() {
        return unsafe { alloc_box_vec(elem_bytes, elem_kind, cap, 0) };
    }
    let v = unsafe {
        alloc_vec_header(GosVec {
            len: 0,
            cap: 0,
            elem_bytes,
            elem_kind,
            region_flag: 0,
            rc: std::sync::atomic::AtomicU16::new(0),
            ptr: SyncRawPtr::NULL,
            generation: 0,
            mutation_generation: 0,
            elem_meta: SyncRawPtr::NULL,
            owner: SyncRawPtr::NULL,
        })
    };
    if !v.is_null() && cap > 0 {
        unsafe { vec_reserve_to(&mut *v, cap, true) };
    }
    v
}

pub(crate) fn vec_elem_meta(v: *const GosVec) -> *const i64 {
    if v.is_null() {
        return std::ptr::null();
    }
    // SAFETY: callers only query live Vec headers.
    // SAFETY: callers only query live Vec headers.
    unsafe { (*v).elem_meta.as_const_ptr() }
}

/// One owned heap child inside each element slot of an
/// [`vec_elem_kind::AGGR_OWNED`] vec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VecSlotChild {
    /// Discriminant value under which the child word holds a live
    /// pointer (`0` = Ok/Some side); negative means unconditional.
    pub gate: i64,
    /// Word index (8-byte units) of the discriminant within the slot.
    /// Ignored when `gate` is negative.
    pub disc_word: usize,
    /// Word index of the child pointer within the slot.
    pub word: usize,
    /// Child kind - [`vec_elem_kind::STRING`], [`vec_elem_kind::VEC`],
    /// [`vec_elem_kind::MAP`] or [`vec_elem_kind::RC_NODE`] - selecting the
    /// free / retain routine.
    pub kind: u8,
}

/// Slot-children layout of an `AGGR_OWNED` vec, or `None` for any
/// other vec.
pub fn vec_slot_children(v: &GosVec) -> Option<&[VecSlotChild]> {
    vec_owner(v).and_then(|owner| owner.slot_children.as_deref())
}

/// Tags `v` as an [`vec_elem_kind::AGGR_OWNED`] vec and records where
/// the owned heap children live inside each element slot, so
/// `gos_rt_vec_free` deep-frees them even when the vec was never
/// (or only partially) iterated.
///
/// Materializer shims MUST call this AFTER their construction pushes:
/// a freshly `alloc_cstring`'d child's initial reference is the vec's
/// own share, while `gos_rt_vec_push` retains the children of every
/// slot pushed onto an already-tagged vec (the push-site source keeps
/// its own share, mirroring the `AGGR_GUARDED` contract).
///
/// No-op for null / region vecs (region storage is freed wholesale at
/// `arena_pop` and never walked).
pub fn vec_set_slot_children(v: *mut GosVec, children: &'static [VecSlotChild]) {
    if v.is_null() {
        return;
    }
    // SAFETY: callers hand a live header they just allocated.
    let vec = unsafe { &mut *v };
    if vec_is_region(vec) {
        return;
    }
    vec.elem_kind = vec_elem_kind::AGGR_OWNED;
    ensure_vec_owner(vec).slot_children = Some(children.into());
}

/// Calls `f` with each live, non-null child pointer (and its kind)
/// inside the element slot at `slot`, per the `AGGR_OWNED` layout.
unsafe fn visit_slot_children(
    slot: *const u8,
    children: &[VecSlotChild],
    mut f: impl FnMut(*mut u8, u8),
) {
    unsafe { visit_slot_child_words(slot, children, |_, child, kind| f(child, kind)) };
}

/// Same walk as [`visit_slot_children`], but the callback also receives the
/// address of the slot word holding the child. A kind whose copy takes a value
/// of its own - [`vec_elem_kind::MAP`] - writes the new handle back through it.
unsafe fn visit_slot_child_words(
    slot: *const u8,
    children: &[VecSlotChild],
    mut f: impl FnMut(*mut u8, *mut u8, u8),
) {
    for c in children {
        if c.gate >= 0 {
            let disc = unsafe { slot.add(c.disc_word * 8).cast::<i64>().read_unaligned() };
            if disc != c.gate {
                continue;
            }
        }
        // Slots hold child pointers exposed as integers by the flat-slot
        // ABI; recover provenance before use.
        let word = unsafe { slot.add(c.word * 8).cast_mut() };
        let child = unsafe { slot_read_word(word) };
        if !child.is_null() {
            f(word, child, c.kind);
        }
    }
}

/// Retain the owned children of a slot copy handed OUT of the `AGGR_OWNED`
/// vec `v`, which now shares them with the vec.
///
/// A `GosMap` carries no reference count, so a map child cannot be shared: the
/// copy takes a table of its own and the new handle is written back into the
/// slot, leaving the vec's own table to the vec. The counted children are
/// retained, as a second owner of each.
pub(crate) unsafe fn vec_retain_slot_children(v: *const GosVec, slot: *mut u8) {
    let Some(children) = vec_slot_children(unsafe { &*v }) else {
        return;
    };
    unsafe {
        visit_slot_child_words(slot, children, |word, child, kind| match kind {
            vec_elem_kind::STRING => crate::c_abi::string::gos_rt_str_retain(child.cast()),
            vec_elem_kind::VEC => vec_retain_header(child.cast()),
            vec_elem_kind::RC_NODE => crate::c_abi::rc::gos_rt_rc_retain(child),
            vec_elem_kind::MAP => {
                let cloned = crate::c_abi::gos_rt_map_clone(child.cast());
                slot_write_word(word, cloned as *mut u8);
            }
            vec_elem_kind::SET => {
                let cloned = crate::c_abi::set::gos_rt_set_clone(child.cast());
                slot_write_word(word, cloned as *mut u8);
            }
            _ => {}
        });
    }
}

/// Builds a borrowing `*mut GosVec` view over an array, for a `&[T]` /
/// `&Vec<T>` parameter fed a `&array`. Word-sized element layouts point
/// directly at the source array so mutable slice writes alias the caller's
/// storage. Fixed arrays store one word per element, so sub-word element
/// views use an 8-byte stride rather than the packed Vec stride.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_borrow_arr(
    elem_bytes: u32,
    data: *const u8,
    len: i64,
) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        if len < 0 {
            crate::c_abi::panic::panic_text("Vec length must be non-negative");
        }
        let view_elem_bytes = elem_bytes.max(8);
        let v = unsafe { alloc_box_vec(view_elem_bytes, vec_elem_kind::PRIMITIVE, 0, 0) };
        unsafe {
            (*v).len = len;
            (*v).cap = len;
            (*v).ptr = SyncRawPtr::new(data.cast_mut());
        }
        v
    })
}

/// Builds a borrowing `*mut GosVec` view over a packed array. This is the
/// LLVM counterpart of [`gos_rt_vec_borrow_arr`] for fixed `[u8; N]` arrays,
/// whose native storage is `[N x i8]` rather than one 8-byte slot per element.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_borrow_packed_arr(
    elem_bytes: u32,
    data: *const u8,
    len: i64,
) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        if len < 0 {
            crate::c_abi::panic::panic_text("Vec length must be non-negative");
        }
        let v = unsafe { alloc_box_vec(elem_bytes.max(1), vec_elem_kind::PRIMITIVE, 0, 0) };
        unsafe {
            (*v).len = len;
            (*v).cap = len;
            (*v).ptr = SyncRawPtr::new(data.cast_mut());
        }
        v
    })
}

/// Release the owned children of the element slot at `slot` of the
/// `AGGR_OWNED` vec `v` (the slot is about to be overwritten).
pub(crate) unsafe fn vec_release_slot_children(v: *const GosVec, slot: *const u8) {
    let Some(children) = vec_slot_children(unsafe { &*v }) else {
        return;
    };
    unsafe {
        visit_slot_children(slot, children, |child, kind| match kind {
            vec_elem_kind::STRING => crate::c_abi::string::gos_rt_str_free_typed(child.cast()),
            vec_elem_kind::VEC => crate::c_abi::map::gos_rt_vec_free(child.cast()),
            vec_elem_kind::MAP => crate::c_abi::map::gos_rt_map_free(child.cast()),
            vec_elem_kind::SET => crate::c_abi::map::gos_rt_set_free(child.cast()),
            vec_elem_kind::RC_NODE => crate::c_abi::rc::gos_rt_rc_release(child),
            _ => {}
        });
    }
}

/// Writes a slot-block element - a tuple, struct, or fixed array - to a
/// `Vec` at `idx`.
///
/// The word-sized setter stores one machine word, which for a multi-slot
/// element writes the first field and leaves the rest of the slot as it
/// was. This copies the whole slot, and hands over the element's
/// reference-counted children with it: the outgoing element's shares are
/// released and the incoming slot's are retained, so the vec holds
/// exactly one share of each child it now carries.
///
/// Invalid indexing is a bounds panic, exactly as the scalar setter and
/// an indexed read are; it is never silently ignored.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_set_slots(v: *mut GosVec, idx: i64, slots: *const u8) {
    ffi_entry!((), {
        if v.is_null() {
            crate::c_abi::panic::panic_oob_text("vec index", idx, 0);
        }
        let vec = unsafe { &mut *v };
        if idx < 0 || idx >= vec.len {
            crate::c_abi::panic::panic_oob_text("vec index", idx, vec.len);
        }
        let stride = vec.elem_bytes as usize;
        if slots.is_null() || vec.ptr.is_null() || stride == 0 {
            return;
        }
        let kind = vec.elem_kind;
        let meta = vec_elem_meta(vec);
        let destination = unsafe { vec.ptr.as_ptr().add(idx as usize * stride) };
        unsafe {
            match kind {
                vec_elem_kind::AGGR_OWNED => vec_release_slot_children(vec, destination),
                vec_elem_kind::AGGR_GUARDED if !meta.is_null() => {
                    crate::c_abi::rc::gos_rt_aggr_release_children(destination, meta);
                }
                _ => {}
            }
            crate::c_abi::string::copy_small_bytes(slots, destination, stride);
            match kind {
                vec_elem_kind::AGGR_OWNED => vec_retain_slot_children(vec, destination),
                vec_elem_kind::AGGR_GUARDED if !meta.is_null() => {
                    crate::c_abi::rc::gos_rt_aggr_retain_children(destination, meta);
                }
                _ => {}
            }
        }
    });
}

/// Release the owned children of every element of an `AGGR_OWNED`
/// vec. Called by `gos_rt_vec_free` before the buffer is reclaimed,
/// closing the early-`break` leak: slots the consumer never walked
/// still drop their strings / nested vecs here.
pub(crate) unsafe fn vec_release_owned_children(v: &GosVec) {
    if rc_trace_enabled() {
        eprintln!(
            "VDEEPF  v={:p} children={} kind={}",
            std::ptr::from_ref(v),
            vec_slot_children(v).map_or(0, <[_]>::len),
            v.elem_kind
        );
    }
    let Some(children) = vec_slot_children(v) else {
        return;
    };
    if v.ptr.is_null() {
        return;
    }
    let stride = v.elem_bytes as usize;
    if stride == 0 {
        return;
    }
    for i in 0..v.len.max(0) as usize {
        unsafe {
            visit_slot_children(v.ptr.add(i * stride), children, |child, kind| match kind {
                vec_elem_kind::STRING => crate::c_abi::string::gos_rt_str_free_typed(child.cast()),
                vec_elem_kind::VEC => crate::c_abi::map::gos_rt_vec_free(child.cast()),
                vec_elem_kind::MAP => crate::c_abi::map::gos_rt_map_free(child.cast()),
                vec_elem_kind::SET => crate::c_abi::map::gos_rt_set_free(child.cast()),
                vec_elem_kind::RC_NODE => crate::c_abi::rc::gos_rt_rc_release(child),
                _ => {}
            });
        }
    }
}

/// Increment a `GosVec`'s strong count by one. The `String`-field
/// counterpart `gos_rt_str_retain` bumps a shared string; this bumps a
/// shared `Vec`/`[T]` reached as a by-value struct field, so a struct copy
/// (`let b = a`) gives both owners a share and each `gos_rt_vec_free` at
/// their deaths balances. Null-safe; region and count-0 headers handled by
/// `vec_retain_header`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_retain(v: *mut GosVec) {
    ffi_entry!((), {
        unsafe { vec_retain_header(v) };
    });
}

/// Marks every reference-counted value reachable from a Vec as shared before
/// the Vec is published to another goroutine. Vec headers already use an
/// atomic count, but String and RC-node elements must switch their own headers
/// to atomic mode as well. Nested Vecs and aggregate-owned element slots are
/// walked recursively.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_mark_shared(v: *mut GosVec) {
    ffi_entry!((), {
        if v.is_null() {
            return;
        }
        let vec = unsafe { &*v };
        if vec.ptr.is_null() || vec.len <= 0 {
            return;
        }
        let len = vec.len as usize;
        let stride = vec.elem_bytes as usize;
        match vec.elem_kind {
            vec_elem_kind::STRING | vec_elem_kind::RC_ENUM | vec_elem_kind::VEC if stride == 8 => {
                for index in 0..len {
                    let child = unsafe { slot_read_word(vec.ptr.add(index * stride)) };
                    if child.is_null() {
                        continue;
                    }
                    if vec.elem_kind == vec_elem_kind::VEC {
                        unsafe { gos_rt_vec_mark_shared(child.cast()) };
                    } else {
                        unsafe { crate::c_abi::rc::gos_rt_rc_mark_shared(child) };
                    }
                }
            }
            vec_elem_kind::AGGR_OWNED => {
                if let Some(children) = vec_slot_children(vec) {
                    for index in 0..len {
                        let slot = unsafe { vec.ptr.add(index * stride) };
                        unsafe {
                            visit_slot_children(slot, children, |child, kind| match kind {
                                vec_elem_kind::VEC => gos_rt_vec_mark_shared(child.cast()),
                                vec_elem_kind::MAP => {
                                    crate::c_abi::map::gos_rt_map_mark_shared(child.cast());
                                }
                                vec_elem_kind::STRING | vec_elem_kind::RC_NODE => {
                                    crate::c_abi::rc::gos_rt_rc_mark_shared(child);
                                }
                                _ => {}
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    });
}

/// Bump a non-region `GosVec`'s strong count by one (the header-RC
/// counterpart of `gos_rt_str_retain` for nested-vec children).
/// Whether `GOS_RC_TRACE` was set at first use: prints one line per vec
/// retain and release so a leak's per-pointer balance can be read off a run.
/// Read once, so the hot paths pay a relaxed load and nothing else.
pub(crate) fn rc_trace_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("GOS_RC_TRACE").is_some())
}

pub(crate) unsafe fn vec_retain_header(v: *mut GosVec) {
    if v.is_null() {
        return;
    }
    if rc_trace_enabled() {
        eprintln!("VRETAIN v={v:p}");
    }
    let vec = unsafe { &*v };
    if vec_is_region(vec) {
        return;
    }
    // Headers from constructors that never wrote a count read 0; in that
    // case a simple +1 would bring it to 1, and the next release would
    // reclaim a still-live header. Use a CAS loop to atomically jump from
    // 0 to 2 (treating 0 as the same as 1), while a normal retain is +1.
    let atomic = vec_rc_atomic(vec);
    let mut current = atomic.load(std::sync::atomic::Ordering::Relaxed);
    loop {
        let next = current.max(1).saturating_add(1);
        match atomic.compare_exchange_weak(
            current,
            next,
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(actual) => current = actual,
        }
    }
}

/// Propagates ownership-bearing element kinds from `src` to `out`
/// after `out` received a raw copy of (some of) `src`'s slots.
/// String and RC-node children gain a retained share. Nested Vec children are
/// cloned and the copied slot is rewritten, preserving Vec value semantics
/// recursively instead of exposing a shared growable header. Covers `STRING`,
/// `VEC`, `RC_ENUM` and
/// `AGGR_OWNED` element kinds; `AGGR_GUARDED` keeps its dedicated
/// copy-blob path at the existing call sites. No-op for primitive /
/// region / null vecs.
/// Gives `out`, whose first `src.len` slots are a raw copy of `src`'s, its own
/// share of every element: guarded aggregates retain their copy-blob children
/// under the source's element metadata, and owned pointer slots retain or
/// clone through [`vec_share_owned_elements`].
pub(crate) unsafe fn vec_adopt_element_shares(src: *const GosVec, out: *mut GosVec) {
    if src.is_null() || out.is_null() {
        return;
    }
    let s = unsafe { &*src };
    if s.elem_kind == vec_elem_kind::AGGR_GUARDED {
        let meta = vec_elem_meta(src);
        if !meta.is_null() {
            unsafe { gos_rt_vec_set_elem_meta(out, meta) };
            let data = unsafe { (*out).ptr.as_ptr() };
            let stride = s.elem_bytes as usize;
            for i in 0..s.len.max(0) as usize {
                unsafe {
                    crate::c_abi::rc::gos_rt_aggr_retain_children(data.add(i * stride), meta);
                }
            }
        }
    }
    unsafe { vec_share_owned_elements(src, out) };
}

pub(crate) unsafe fn vec_share_owned_elements(src: *const GosVec, out: *mut GosVec) {
    if src.is_null() || out.is_null() {
        return;
    }
    let s = unsafe { &*src };
    match s.elem_kind {
        vec_elem_kind::STRING
        | vec_elem_kind::VEC
        | vec_elem_kind::RC_ENUM
        | vec_elem_kind::MAP
        | vec_elem_kind::JSON
            if s.elem_bytes == 8 =>
        {
            unsafe { (*out).elem_kind = s.elem_kind };
            let len = unsafe { (*out).len.max(0) as usize };
            for i in 0..len {
                // Exposed-integer slot (flat-slot ABI); recover provenance.
                let slot = unsafe { (*out).ptr.add(i * 8) };
                let child = unsafe { slot_read_word(slot) };
                if child.is_null() {
                    continue;
                }
                match s.elem_kind {
                    vec_elem_kind::STRING => unsafe {
                        crate::c_abi::string::gos_rt_str_retain(child.cast());
                    },
                    vec_elem_kind::RC_ENUM => unsafe {
                        crate::c_abi::rc::gos_rt_rc_retain(child);
                    },
                    // A JSON handle carries no count, so the copy takes a box
                    // of its own onto the same document.
                    vec_elem_kind::JSON => unsafe {
                        let cloned = crate::c_abi::json::gos_rt_json_clone_handle(child.cast());
                        slot_write_word(slot, cloned.cast::<u8>());
                    },
                    // A `GosMap` carries no reference count, so a map element
                    // cannot be shared: the copy takes a table of its own and
                    // the new handle goes back into the slot, leaving the
                    // source's table to the source. Both vectors free their
                    // elements, so one table under two of them is freed twice.
                    vec_elem_kind::MAP => unsafe {
                        let cloned = crate::c_abi::gos_rt_map_clone(child.cast());
                        slot_write_word(slot, cloned as *mut u8);
                    },
                    _ => unsafe {
                        let cloned = crate::c_abi::gos_rt_vec_clone(child.cast());
                        slot_write_word(slot, cloned.cast::<u8>());
                    },
                }
            }
        }
        vec_elem_kind::AGGR_OWNED => {
            if rc_trace_enabled() {
                eprintln!(
                    "VSHARE  src={src:p} out={out:p} children={}",
                    vec_slot_children(s).map_or(0, <[_]>::len)
                );
            }
            if let Some(children) = vec_slot_children(s) {
                // `vec_set_slot_children` takes its own exclusive header
                // borrow to create the lazy metadata carrier. Do not retain a
                // prior `&mut GosVec` across that call: doing so violates
                // Stacked Borrows when the clone path later reads its fields.
                vec_set_slot_children(out, children);
                let (data, stride, len) = unsafe {
                    let o = &*out;
                    (o.ptr.as_ptr(), o.elem_bytes as usize, o.len.max(0) as usize)
                };
                if stride == 0 || data.is_null() {
                    return;
                }
                for i in 0..len {
                    let slot = unsafe { data.add(i * stride) };
                    // The one walk every slot-child kind goes through, so a
                    // kind added to the layout reaches the copy as well as
                    // the free. A `GosString` is immutable and a heap node is
                    // counted, so both are shared; a `GosVec`, a `GosMap`,
                    // and a `GosSet` are mutable and the copy takes storage
                    // of its own, written back through the slot word.
                    unsafe {
                        visit_slot_child_words(slot, children, |word, child, kind| match kind {
                            vec_elem_kind::STRING => {
                                crate::c_abi::string::gos_rt_str_retain(child.cast());
                            }
                            vec_elem_kind::VEC => {
                                let cloned = crate::c_abi::gos_rt_vec_clone(child.cast());
                                slot_write_word(word, cloned.cast::<u8>());
                            }
                            vec_elem_kind::MAP => {
                                let cloned = crate::c_abi::gos_rt_map_clone(child.cast());
                                slot_write_word(word, cloned as *mut u8);
                            }
                            vec_elem_kind::SET => {
                                let cloned = crate::c_abi::set::gos_rt_set_clone(child.cast());
                                slot_write_word(word, cloned as *mut u8);
                            }
                            vec_elem_kind::RC_NODE => crate::c_abi::rc::gos_rt_rc_retain(child),
                            _ => {}
                        });
                    }
                }
            } else {
                // Layout unknown (cannot happen for live vecs; defensive):
                // fall back to a shallow copy that never double-frees.
                let o = unsafe { &mut *out };
                o.elem_kind = vec_elem_kind::PRIMITIVE;
            }
        }
        _ => {}
    }
}

/// Tags `v` as owning reference-counted enum-node elements
/// ([`vec_elem_kind::RC_ENUM`]): `gos_rt_vec_free` releases each element
/// and storage duplication retains each copy. Emitted by the MIR
/// lowering right after constructing a vec whose element type is a
/// payload-bearing user enum. Only a `PRIMITIVE` vec is re-tagged, so a
/// meta set by a materializer shim is never clobbered. No-op for null /
/// region vecs (region storage is freed wholesale and never walked).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_mark_rc_elems(v: *mut GosVec) {
    if v.is_null() {
        return;
    }
    let vec = unsafe { &mut *v };
    if vec_is_region(vec) || vec.elem_kind != vec_elem_kind::PRIMITIVE || vec.elem_bytes != 8 {
        return;
    }
    vec.elem_kind = vec_elem_kind::RC_ENUM;
}

/// Tags `v` as owning nested-vec elements ([`vec_elem_kind::VEC`]):
/// `gos_rt_vec_free` releases each element vec's share and storage
/// duplication (clone / slice / filter) retains each copy. Emitted by
/// the MIR lowering right after constructing a vec whose element type
/// is itself `Vec`/`[T]`; the pushes minted the container's shares.
/// Only a `PRIMITIVE` vec is re-tagged; no-op for null / region vecs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_mark_vec_elems(v: *mut GosVec) {
    if v.is_null() {
        return;
    }
    let vec = unsafe { &mut *v };
    if vec_is_region(vec) || vec.elem_kind != vec_elem_kind::PRIMITIVE || vec.elem_bytes != 8 {
        return;
    }
    vec.elem_kind = vec_elem_kind::VEC;
}

/// Tags `v` as owning `String` elements ([`vec_elem_kind::STRING`]):
/// `gos_rt_vec_free` releases each element and storage duplication retains
/// each copy. Emitted by the MIR lowering after a runtime shim whose callback
/// answered each element as a fresh share. Only a `PRIMITIVE` vec is
/// re-tagged; no-op for null / region vecs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_mark_str_elems(v: *mut GosVec) {
    if v.is_null() {
        return;
    }
    let vec = unsafe { &mut *v };
    if vec_is_region(vec) || vec.elem_kind != vec_elem_kind::PRIMITIVE || vec.elem_bytes != 8 {
        return;
    }
    vec.elem_kind = vec_elem_kind::STRING;
}

/// Width of the header prefix every inline element access reads: length at
/// offset 0, element width at 16, and the data pointer at 24.
pub const VEC_HEADER_PREFIX_BYTES: usize = 32;

/// A vector of copies of the header prefix of each nested-vector element of
/// `v`, one [`VEC_HEADER_PREFIX_BYTES`]-wide primitive element per row, with a
/// null row copied as zeros (an empty row).
///
/// The address of element `i` serves as row `i`'s handle for reading its
/// length and elements: the copy names the same length and the same element
/// storage the row does for as long as no row is resized and `v` itself is not
/// changed, which the compiler proves for the loop nest that builds the table.
/// A reader then reaches a row's elements from the table without first loading
/// the row's own header.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_header_table(v: *const GosVec) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let len = if v.is_null() {
            0
        } else {
            unsafe { (*v).len.max(0) }
        };
        let table = unsafe {
            alloc_box_vec(
                VEC_HEADER_PREFIX_BYTES as u32,
                vec_elem_kind::PRIMITIVE,
                len,
                len,
            )
        };
        if len == 0 || table.is_null() {
            return table;
        }
        let rows = unsafe { (*v).ptr };
        let out = unsafe { (*table).ptr };
        for i in 0..len as usize {
            let row = unsafe { slot_read_word(rows.add(i * 8)) }.cast::<u8>();
            let dst = unsafe { out.add(i * VEC_HEADER_PREFIX_BYTES) };
            if row.is_null() {
                unsafe { std::ptr::write_bytes(dst, 0, VEC_HEADER_PREFIX_BYTES) };
            } else {
                unsafe { std::ptr::copy_nonoverlapping(row, dst, VEC_HEADER_PREFIX_BYTES) };
            }
        }
        table
    })
}

/// Rebuilds every nested-vector element of `v` in index order, in place.
///
/// A `Vec<Vec<T>>` grown element by element has its element storage scattered
/// across the heap in the order the pushes happened to allocate, and a walk of
/// it pays that order in cache misses. Rebuilding each element hands the
/// allocator the elements in the order a reader visits them, at a peak cost of
/// one element rather than a second copy of the whole structure.
///
/// Each element is replaced by a copy of itself and the original is released,
/// which is what a whole-container copy does to each element - so the value is
/// the one the program already had, and any other name for an element keeps
/// the element it named. A vector whose elements are not nested vectors is
/// left alone: their storage is the one contiguous buffer it already is.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_compact_elems(v: *mut GosVec) {
    if v.is_null() {
        return;
    }
    let vec = unsafe { &mut *v };
    if vec_is_region(vec)
        || vec.elem_kind != vec_elem_kind::VEC
        || vec.elem_bytes != 8
        || vec.ptr.is_null()
    {
        return;
    }
    for i in 0..vec.len.max(0) as usize {
        // Exposed-integer slot (flat-slot ABI); recover provenance.
        let slot = unsafe { vec.ptr.add(i * 8) };
        let child = unsafe { slot_read_word(slot) }.cast::<GosVec>();
        if child.is_null() {
            continue;
        }
        let fresh = unsafe { crate::c_abi::string::gos_rt_vec_clone(child) };
        if fresh.is_null() {
            continue;
        }
        unsafe { slot_write_word(slot, fresh.cast::<u8>()) };
        unsafe { crate::c_abi::map::gos_rt_vec_free(child) };
    }
}

/// Tags `v` as holding guarded aggregate elements and records the
/// per-type meta used to retain/release their copy-blob children.
/// Emitted by the MIR lowering right after constructing a vec whose
/// element type carries a guarded meta. No-op for null / region vecs
/// (region storage is freed wholesale and never walked).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_set_elem_meta(v: *mut GosVec, meta: *const i64) {
    if v.is_null() || meta.is_null() {
        return;
    }
    let vec = unsafe { &mut *v };
    if vec_is_region(vec) {
        return;
    }
    vec.elem_kind = vec_elem_kind::AGGR_GUARDED;
    vec.elem_meta = SyncRawPtr::from_const(meta);
}

/// Tags `v` as an [`vec_elem_kind::AGGR_OWNED`] vec carrying inline
/// aggregate elements whose RC child pointers (strings, nested vecs,
/// user enum/struct heap nodes) the vec owns, recording where they sit
/// inside each element slot. Emitted by the MIR lowering right after
/// constructing such a vec. The `meta` blob is a static, codegen-owned
/// `i64` array: `[count, (gate, disc_word, word, kind) * count]`, with
/// `kind` a [`vec_elem_kind`] slot-child tag (`STRING` / `VEC` /
/// `RC_NODE`). No-op for null / region vecs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_set_slot_children(v: *mut GosVec, meta: *const i64) {
    if v.is_null() || meta.is_null() {
        return;
    }
    let count = unsafe { *meta }.max(0) as usize;
    let mut children = Vec::with_capacity(count);
    for i in 0..count {
        let base = 1 + i * 4;
        children.push(VecSlotChild {
            gate: unsafe { *meta.add(base) },
            disc_word: unsafe { *meta.add(base + 1) }.max(0) as usize,
            word: unsafe { *meta.add(base + 2) }.max(0) as usize,
            kind: unsafe { *meta.add(base + 3) } as u8,
        });
    }
    let vec = unsafe { &mut *v };
    if !vec_is_region(vec) {
        vec.elem_kind = vec_elem_kind::AGGR_OWNED;
        ensure_vec_owner(vec).slot_children = Some(children.into_boxed_slice());
    }
}

/// Declares that `v`'s elements own the counted words `children` names, so
/// each push takes a share of them and the vec's free gives those back. No-op
/// for a null or region vec, or when no word is counted.
pub(crate) fn vec_own_slot_children(v: *mut GosVec, children: Box<[VecSlotChild]>) {
    if v.is_null() || children.is_empty() {
        return;
    }
    // SAFETY: callers hand a live header they just allocated.
    let vec = unsafe { &mut *v };
    if vec_is_region(vec) {
        return;
    }
    vec.elem_kind = vec_elem_kind::AGGR_OWNED;
    ensure_vec_owner(vec).slot_children = Some(children);
}

/// Release the guarded children of every element of an
/// `AGGR_GUARDED` vec. Called by `gos_rt_vec_free` before the buffer
/// is reclaimed.
pub(crate) unsafe fn vec_release_guarded_elements(v: &GosVec) {
    let meta = vec_elem_meta(v);
    if meta.is_null() || v.ptr.is_null() {
        return;
    }
    let stride = v.elem_bytes as usize;
    if stride == 0 {
        return;
    }
    for i in 0..v.len.max(0) as usize {
        unsafe {
            crate::c_abi::rc::gos_rt_aggr_release_children(v.ptr.add(i * stride), meta);
        }
    }
}

/// True when this GosVec was allocated inside an arena region.
#[inline]
pub fn vec_is_region(v: &GosVec) -> bool {
    v.region_flag & VEC_REGION_FLAG != 0
}

/// Reads element `idx` of `v` as an i64, honoring the header's
/// `elem_bytes` (packed byte vecs zero-extend, word vecs read the
/// full 8 bytes). `idx` must already be bounds-checked by the
/// caller. 16-byte elements are regex-internal and never reach the
/// scalar helpers; reading their first word is a safe fallback.
pub(crate) unsafe fn vec_elem_load_i64(v: &GosVec, idx: i64) -> i64 {
    let p = unsafe { v.ptr.add((idx as usize) * (v.elem_bytes as usize)) };
    match v.elem_bytes {
        1 => i64::from(unsafe { p.read() }),
        2 => i64::from(unsafe { p.cast::<u16>().read_unaligned() }),
        4 => i64::from(unsafe { p.cast::<u32>().read_unaligned() }),
        _ => {
            debug_assert!(
                v.elem_bytes >= 8,
                "vec_elem_load_i64: unexpected elem_bytes {}",
                v.elem_bytes
            );
            unsafe { p.cast::<i64>().read_unaligned() }
        }
    }
}

/// `true` when the element is a struct or tuple held inline in the storage,
/// so its slot address is the value and its slots may embed owned children.
/// A one-field struct occupies a single slot and is still one of these, which
/// is why the width alone does not answer the question.
pub(crate) fn vec_elem_is_inline_aggregate(v: &GosVec) -> bool {
    v.elem_bytes > 8
        || matches!(
            v.elem_kind,
            vec_elem_kind::AGGR_OWNED | vec_elem_kind::AGGR_GUARDED | vec_elem_kind::AGGR_FLAT
        )
}

/// The payload word for an element handed back as an owned value.
///
/// A word-wide element is the value itself. A wider one is a flat slot block
/// the caller addresses in place, so its words move into a counted blob: the
/// value has to stay readable across the next mutation of the storage it came
/// from, and the carrier holding it gives back the children those words own.
pub(crate) unsafe fn vec_elem_owned_payload_word(v: &GosVec, idx: i64) -> i64 {
    unsafe { vec_elem_payload_blob(v, idx, false) }
}

/// The payload word for an element handed to the program while the vec keeps
/// its own.
///
/// An inline aggregate is copied into a counted blob as for an owned read.
/// Both holders are then live, so each of the copy's children gains a share.
pub(crate) unsafe fn vec_elem_shared_payload_word(v: &GosVec, idx: i64) -> i64 {
    unsafe { vec_elem_payload_blob(v, idx, true) }
}

/// Copies element `idx` into the counted blob an `Option` carrier answers,
/// laid out by what the element's words own: the vec's guarded meta, its slot
/// children, or nothing for an element of scalars.
unsafe fn vec_elem_payload_blob(v: &GosVec, idx: i64, shared: bool) -> i64 {
    let stride = v.elem_bytes as usize;
    if stride == 0 || v.ptr.is_null() || !vec_elem_is_inline_aggregate(v) {
        return unsafe { vec_elem_load_i64(v, idx) };
    }
    let guarded = vec_elem_meta(std::ptr::from_ref(v));
    let meta = match v.elem_kind {
        vec_elem_kind::AGGR_GUARDED if !guarded.is_null() => guarded,
        vec_elem_kind::AGGR_OWNED => vec_slot_children(v)
            .map_or(crate::c_abi::rc::LEAF_BLOB_META.as_ptr(), |children| {
                slot_children_blob_meta(children).as_ptr()
            }),
        _ => crate::c_abi::rc::LEAF_BLOB_META.as_ptr(),
    };
    let src = unsafe { v.ptr.add((idx as usize) * stride) };
    let (copy, counted) =
        unsafe { crate::c_abi::rc::counted_element_copy(stride as u64, meta, src) };
    if copy.is_null() {
        return 0;
    }
    if shared && counted {
        match v.elem_kind {
            vec_elem_kind::AGGR_GUARDED if !guarded.is_null() => unsafe {
                crate::c_abi::rc::gos_rt_aggr_retain_children(copy, guarded);
            },
            vec_elem_kind::AGGR_OWNED => unsafe {
                vec_retain_slot_children(std::ptr::from_ref(v), copy);
            },
            _ => {}
        }
    }
    copy as i64
}

/// A slot-children list with the counted-blob meta that names it.
type InternedSlotChildren = (Box<[VecSlotChild]>, &'static [i64]);

/// The counted-blob meta naming `children`. It is interned for the life of
/// the process because a copy taken out of a vec can outlive that vec.
fn slot_children_blob_meta(children: &[VecSlotChild]) -> &'static [i64] {
    thread_local! {
        static SEEN: std::cell::RefCell<Vec<InternedSlotChildren>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }
    static INTERNED: parking_lot::Mutex<Vec<InternedSlotChildren>> =
        parking_lot::Mutex::new(Vec::new());
    let find = |table: &[InternedSlotChildren]| {
        table
            .iter()
            .find(|(layout, _)| **layout == *children)
            .map(|(_, meta)| *meta)
    };
    if let Some(meta) = SEEN.with(|seen| find(&seen.borrow())) {
        return meta;
    }
    let meta = {
        let mut interned = INTERNED.lock();
        if let Some(meta) = find(&interned) {
            meta
        } else {
            let mut blob = Vec::with_capacity(2 + children.len() * 4);
            blob.push(gossamer_abi::rc::RC_KIND_SLOT_CHILDREN);
            blob.push(i64::try_from(children.len()).unwrap_or(i64::MAX));
            for child in children {
                blob.extend([
                    child.gate,
                    i64::try_from(child.disc_word).unwrap_or(i64::MAX),
                    i64::try_from(child.word).unwrap_or(i64::MAX),
                    i64::from(child.kind),
                ]);
            }
            let meta: &'static [i64] = Box::leak(blob.into_boxed_slice());
            interned.push((children.into(), meta));
            meta
        }
    };
    SEEN.with(|seen| seen.borrow_mut().push((children.into(), meta)));
    meta
}

/// Writes `value` to element `idx` of `v`, truncating to the
/// header's `elem_bytes`. Same preconditions as
/// [`vec_elem_load_i64`].
/// Gives back the share the slot at `idx` holds, for an element kind whose
/// word is a handle the vector owns.
///
/// The vector's teardown releases one share per slot, so a store that replaces
/// a handle has to return the outgoing one here or the vector's own count is
/// the only thing left naming it.
pub(crate) unsafe fn vec_release_owned_elem(v: &GosVec, idx: i64, incoming: i64) {
    if v.elem_bytes != 8 || v.ptr.is_null() || idx < 0 || idx >= v.len {
        return;
    }
    let p = unsafe { v.ptr.add((idx as usize) * 8) };
    let raw = unsafe { slot_read_word(p) }.expose_provenance();
    // A slot storing back what it already holds keeps its one share.
    if raw == 0 || raw == (incoming as usize) {
        return;
    }
    let old: *mut u8 = std::ptr::with_exposed_provenance_mut(raw);
    match v.elem_kind {
        vec_elem_kind::STRING => unsafe {
            crate::c_abi::string::gos_rt_str_free_typed(old.cast());
        },
        vec_elem_kind::VEC => unsafe { crate::c_abi::map::gos_rt_vec_free(old.cast()) },
        vec_elem_kind::MAP => unsafe { crate::c_abi::map::gos_rt_map_free(old.cast()) },
        vec_elem_kind::SET => unsafe { crate::c_abi::map::gos_rt_set_free(old.cast()) },
        vec_elem_kind::RC_ENUM => unsafe { crate::c_abi::rc::gos_rt_rc_release(old) },
        vec_elem_kind::JSON => unsafe { crate::c_abi::json::gos_rt_json_free(old.cast()) },
        _ => {}
    }
}

pub(crate) unsafe fn vec_elem_store_i64(v: &GosVec, idx: i64, value: i64) {
    let p = unsafe { v.ptr.add((idx as usize) * (v.elem_bytes as usize)) };
    match v.elem_bytes {
        1 => unsafe { p.write(value as u8) },
        2 => unsafe { p.cast::<u16>().write_unaligned(value as u16) },
        4 => unsafe { p.cast::<u32>().write_unaligned(value as u32) },
        _ => {
            debug_assert!(
                v.elem_bytes >= 8,
                "vec_elem_store_i64: unexpected elem_bytes {}",
                v.elem_bytes
            );
            unsafe { p.cast::<i64>().write_unaligned(value) };
        }
    }
}

/// Reads the child word a slot holds.
///
/// A slot is eight bytes wide on every target and holds its word
/// little-endian, while a pointer is the target's own width: read the eight
/// bytes, then narrow. Reading through `usize` instead would move four bytes
/// on a 32-bit target and leave the rest of the word unread.
///
/// # Safety
/// `slot` addresses eight readable bytes.
#[inline]
pub(crate) unsafe fn slot_read_word(slot: *const u8) -> *mut u8 {
    let raw = unsafe { slot.cast::<u64>().read_unaligned() };
    std::ptr::with_exposed_provenance_mut(usize::try_from(raw).unwrap_or_default())
}

/// Writes `child` into a slot as the eight-byte word [`slot_read_word`]
/// reads back, zero-extended where a pointer is narrower than the slot.
///
/// # Safety
/// `slot` addresses eight writable bytes.
#[inline]
pub(crate) unsafe fn slot_write_word(slot: *mut u8, child: *mut u8) {
    let raw = child.expose_provenance() as u64;
    unsafe { slot.cast::<u64>().write_unaligned(raw) };
}

/// The atomic refcount field. Interior-mutable, so a shared `&GosVec` can
/// update it without any pointer reinterpretation.
#[inline]
pub fn vec_rc_atomic(v: &GosVec) -> &std::sync::atomic::AtomicU16 {
    &v.rc
}

/// Strong refcount of a non-region Vec, stored in the `rc` atomic field.
/// A Vec aliased > 65535 times is unreachable; the count saturates rather than
/// wrapping. Region Vecs ignore this (they are freed wholesale at region pop).
#[inline]
pub fn vec_rc(v: &GosVec) -> u16 {
    vec_rc_atomic(v).load(std::sync::atomic::Ordering::Relaxed)
}

#[inline]
pub fn vec_set_rc(v: &GosVec, rc: u16) {
    vec_rc_atomic(v).store(rc, std::sync::atomic::Ordering::Relaxed);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_new(elem_bytes: u32) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        unsafe {
            alloc_vec_header(GosVec {
                len: 0,
                cap: 0,
                elem_bytes,
                elem_kind: vec_elem_kind::PRIMITIVE,
                region_flag: 0,
                rc: std::sync::atomic::AtomicU16::new(0),
                ptr: SyncRawPtr::NULL,
                generation: 0,
                mutation_generation: 0,
                elem_meta: SyncRawPtr::NULL,
                owner: SyncRawPtr::NULL,
            })
        }
    })
}

/// The header tag for a requested element kind.
///
/// An inline-aggregate kind describes elements whose heap children are
/// described by the vec's metadata carrier rather than by the tag, and the
/// carrier is attached after the elements are in place (see
/// [`vec_set_slot_children`]). Such a request builds the storage untagged
/// and is not a mistake; a tag outside the set is.
///
/// Every one-word owning kind - including [`vec_elem_kind::RC_ENUM`], whose
/// elements are reference-counted node pointers - is carried straight
/// through, so a vec built to hold owned elements is tagged for the deep
/// free from the moment it exists.
fn header_elem_kind(requested: u8, site: &str) -> u8 {
    match requested {
        vec_elem_kind::AGGR_GUARDED | vec_elem_kind::AGGR_OWNED => vec_elem_kind::PRIMITIVE,
        kind if kind <= vec_elem_kind::ERROR
            || kind == vec_elem_kind::RC_ENUM
            || kind == vec_elem_kind::AGGR_FLAT
            || kind == vec_elem_kind::JSON =>
        {
            kind
        }
        other => {
            eprintln!("{site}: unknown elem_kind {other}; falling back to PRIMITIVE");
            vec_elem_kind::PRIMITIVE
        }
    }
}

/// `gos_rt_vec_new`-like constructor that records the element kind
/// in the header so `gos_rt_vec_free` can deep-free pointer-bearing
/// payloads. `elem_kind` must be a value from [`vec_elem_kind`];
/// out-of-range values fall back to `PRIMITIVE` with an `eprintln!`
/// warning.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_new_typed(elem_bytes: u32, elem_kind: u8) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let kind = header_elem_kind(elem_kind, "gos_rt_vec_new_typed");
        unsafe {
            alloc_vec_header(GosVec {
                len: 0,
                cap: 0,
                elem_bytes,
                elem_kind: kind,
                region_flag: 0,
                rc: std::sync::atomic::AtomicU16::new(0),
                ptr: SyncRawPtr::NULL,
                generation: 0,
                mutation_generation: 0,
                elem_meta: SyncRawPtr::NULL,
                owner: SyncRawPtr::NULL,
            })
        }
    })
}

/// Allocates an uninitialised `bytes`-byte vec element buffer with 8-byte
/// alignment. Vec slots hold 8-byte words (`i64` / pointer), so the
/// buffer must be word-aligned for the slot accesses across the
/// runtime to be sound; a `Vec<u8>` (align 1) only happens to work
/// because the system allocator over-aligns. Backed by a leaked
/// `Vec<u64>`; free with [`free_vec_buffer`] passing the same `bytes`.
///
/// Only slots below `GosVec.len` are readable. Spare capacity is never read
/// before `push` / copy initialises it, so reserving a large vector does not
/// need to touch every page up front.
pub(crate) fn alloc_vec_buffer(bytes: usize) -> *mut u8 {
    let words = bytes.div_ceil(8).max(1);
    let mut buf: Vec<u64> = Vec::with_capacity(words);
    let ptr = buf.as_mut_ptr().cast::<u8>();
    std::mem::forget(buf);
    ptr
}

/// Byte size of `count` elements of `elem_bytes` each. A Vec whose
/// byte size overflows the address space cannot be allocated: the
/// wrapping product would size a short buffer while the header still
/// records the oversized `cap`, so subsequent pushes write past the
/// allocation. Matching the VM's `capacity overflow` panic keeps the
/// three tiers identical instead of corrupting the heap.
#[inline]
fn checked_buffer_bytes(count: usize, elem_bytes: usize) -> usize {
    count.checked_mul(elem_bytes).unwrap_or_else(|| {
        // The raise exits the process (main goroutine) or unwinds (spawned
        // goroutine); this arm never returns.
        crate::c_abi::panic::panic_text("capacity overflow");
        0
    })
}

/// Frees a buffer from [`alloc_vec_buffer`]. `bytes` must equal the
/// value passed at allocation; every GosVec buffer is sized
/// `cap * elem_bytes`, stable across the buffer's life.
pub(crate) unsafe fn free_vec_buffer(ptr: *mut u8, bytes: usize) {
    let words = bytes.div_ceil(8).max(1);
    // SAFETY: `ptr` came from `alloc_vec_buffer(bytes)`, so the same
    // capacity reconstructs its `Vec<u64>` allocation exactly. Length is zero
    // because spare capacity may be uninitialised.
    drop(unsafe { Vec::<u64>::from_raw_parts(ptr.cast::<u64>(), 0, words) });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_with_capacity(elem_bytes: u32, cap: i64) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        // Header + reserved element buffer in one `Box<InlineVec>` (or a
        // separate buffer for a capacity larger than the inline slot). Only
        // initialized slots below len are readable; spare split capacity is
        // intentionally not zero-filled.
        unsafe { alloc_vec_with_capacity(elem_bytes, vec_elem_kind::PRIMITIVE, cap) }
    })
}

/// Constructs a primitive Vec containing `count` copies of `value`.
///
/// Runtime-sized `[value; count]` expressions used to reserve capacity and
/// execute the ordinary checked push path once per element. The final length
/// and capacity are already known, so constructing the initialized buffer in
/// one runtime call avoids repeated header traffic and branches.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_repeat_primitive(
    elem_bytes: u32,
    count: i64,
    value: i64,
) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        if count < 0 {
            crate::c_abi::panic::panic_text("array repeat count must be non-negative");
        }
        if !matches!(elem_bytes, 1 | 2 | 4 | 8) {
            crate::c_abi::panic::panic_text("invalid primitive array element width");
        }
        let vec = unsafe { alloc_vec_with_capacity(elem_bytes, vec_elem_kind::PRIMITIVE, count) };
        if vec.is_null() {
            return vec;
        }
        let bytes = checked_buffer_bytes(count as usize, elem_bytes as usize);
        if bytes != 0 {
            let data = unsafe { (*vec).ptr.as_ptr() };
            let encoded = value.to_ne_bytes();
            let width = elem_bytes as usize;
            let pattern = &encoded[..width];
            if pattern.iter().all(|byte| *byte == pattern[0]) {
                // Every byte of the element is the same, so the whole buffer is
                // one fill - the case a zeroed or byte-repeating element takes.
                unsafe { std::ptr::write_bytes(data, pattern[0], bytes) };
            } else {
                // Write one element, then double the filled region until the
                // buffer is covered: each step is one `memcpy` rather than one
                // call per element.
                unsafe { std::ptr::copy_nonoverlapping(pattern.as_ptr(), data, width) };
                let mut filled = width;
                while filled < bytes {
                    let step = filled.min(bytes - filled);
                    unsafe { std::ptr::copy_nonoverlapping(data, data.add(filled), step) };
                    filled += step;
                }
            }
        }
        unsafe { (*vec).len = count };
        vec
    })
}

/// `gos_rt_vec_with_capacity` variant that records the element
/// kind in the header so `gos_rt_vec_free` can deep-free
/// pointer-bearing payloads. See [`vec_elem_kind`] for the tag
/// encoding. Out-of-range tags fall back to `PRIMITIVE` with an
/// `eprintln!` warning.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_with_capacity_typed(
    elem_bytes: u32,
    cap: i64,
    elem_kind: u8,
) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let kind = header_elem_kind(elem_kind, "gos_rt_vec_with_capacity_typed");
        unsafe { alloc_vec_with_capacity(elem_bytes, kind, cap) }
    })
}

/// Builds a fresh `*mut GosVec` from a stack/heap array.
/// `Box::into_raw`s the resulting GosVec header.
///
/// Used at the binding-call boundary to convert a Gossamer
/// `[T; N]` array literal (or similarly-shaped value) into the
/// `*mut GosVec` shape the binding's C-ABI thunk expects for a
/// `Vec<T>` parameter.
///
/// The source is the inline `[T; N]` layout - one 8-byte slot per
/// element - so a sub-word element (`bool` / `u8`, canonical stride 1)
/// repacks from each slot's low byte; a flat `len * elem_bytes`
/// memcpy would read the first slots' spare bytes as elements.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_from_arr(
    elem_bytes: u32,
    data: *const u8,
    len: i64,
) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        if len < 0 {
            crate::c_abi::panic::panic_text("Vec length must be non-negative");
        }
        let n = checked_buffer_bytes(len as usize, elem_bytes as usize);
        // Header + element buffer in one `Box<InlineVec>` (inline for a
        // small array, else a separate buffer); `ptr` lands at the data
        // region either way, so the copy target is uniform.
        let v = unsafe { alloc_box_vec(elem_bytes, vec_elem_kind::PRIMITIVE, len, len) };
        if n > 0 && !data.is_null() {
            let eb = elem_bytes as usize;
            if eb < 8 {
                for i in 0..(len as usize) {
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            data.add(i * 8),
                            (*v).ptr.as_ptr().add(i * eb),
                            eb,
                        );
                    }
                }
            } else {
                unsafe { std::ptr::copy_nonoverlapping(data, (*v).ptr.as_ptr(), n) };
            }
        }
        v
    })
}

/// Builds a fresh `*mut GosVec` from a packed native array. LLVM stores direct
/// fixed `[u8; N]` arrays as `[N x i8]`, so this helper copies `len *
/// elem_bytes` contiguous bytes instead of applying the legacy word-slot
/// repack used by [`gos_rt_vec_from_arr`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_from_packed_arr(
    elem_bytes: u32,
    data: *const u8,
    len: i64,
) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        if len < 0 {
            crate::c_abi::panic::panic_text("Vec length must be non-negative");
        }
        let n = checked_buffer_bytes(len as usize, elem_bytes as usize);
        let v = unsafe { alloc_box_vec(elem_bytes.max(1), vec_elem_kind::PRIMITIVE, len, len) };
        if n > 0 && !data.is_null() {
            unsafe { std::ptr::copy_nonoverlapping(data, (*v).ptr.as_ptr(), n) };
        }
        v
    })
}

/// Converts a flat 2-level nested array `[Array{T,inner_len}; outer_len]` into
/// a `Vec<*mut GosVec>` where every inner flat array has been promoted to a
/// heap-allocated `GosVec`. Needed when a `[[T]]` literal is coerced at a
/// call site that expects `Vec<Vec<T>>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_nested_arr_to_vec(
    inner_elem_bytes: i64,
    inner_len: i64,
    raw: *const u8,
    outer_len: i64,
) -> *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        // Outer Vec holds pointer-sized elements (*mut GosVec).
        let outer = unsafe { gos_rt_vec_new(8) };
        if raw.is_null() || outer_len <= 0 || inner_len <= 0 || inner_elem_bytes <= 0 {
            return outer;
        }
        // Rows use the inline word-per-slot layout (`inner_len` 8-byte
        // slots each), independent of the element's canonical stride;
        // `gos_rt_vec_from_arr` repacks each row's sub-word elements.
        let stride = checked_buffer_bytes(inner_len as usize, 8);
        for i in 0..(outer_len as usize) {
            let inner_raw = unsafe { raw.add(i * stride) };
            let inner_vec =
                unsafe { gos_rt_vec_from_arr(inner_elem_bytes as u32, inner_raw, inner_len) };
            let inner_ptr_i64 = inner_vec as i64;
            let bytes = inner_ptr_i64.to_ne_bytes();
            unsafe { gos_rt_vec_push(outer, bytes.as_ptr()) };
        }
        outer
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_len(v: *const GosVec) -> i64 {
    ffi_entry!(-1, {
        if v.is_null() {
            return 0;
        }
        unsafe { (*v).len }
    })
}

/// Returns the total element capacity of `v` without changing its allocation.
///
/// A null Vec is the canonical empty representation and therefore reports
/// zero, matching [`gos_rt_vec_len`].  Keeping this query in the runtime
/// rather than exposing the header layout lets the ABI retain freedom to
/// change the backing representation while callers continue to plan capacity
/// explicitly.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_capacity(v: *const GosVec) -> i64 {
    ffi_entry!(0, {
        if v.is_null() {
            return 0;
        }
        unsafe { (*v).cap.max(0) }
    })
}

/// Typed-i64 wrapper around [`gos_rt_vec_push`]. Spills the value
/// to a stack slot and forwards its address so the byte-erased
/// push helper can `memcpy` it into the vec's storage. Used by the
/// dynamic-count `[value; n]` lowering - passing an i64 directly
/// to the byte-erased helper would otherwise need a per-call-site
/// stack slot in cranelift.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_push_i64(v: *mut GosVec, value: i64) {
    ffi_entry!((), {
        let bytes = value.to_ne_bytes();
        unsafe { gos_rt_vec_push(v, bytes.as_ptr()) };
    });
}

/// Pushes a 16-byte `i128` element (the by-value `Result`/`Option`
/// representation) by forwarding its address to the byte-erased push. The
/// vec's `elem_bytes` must be 16.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_push_i128(v: *mut GosVec, value: i128) {
    ffi_entry!((), {
        let bytes = value.to_ne_bytes();
        unsafe { gos_rt_vec_push(v, bytes.as_ptr()) };
    });
}

/// Reads a 16-byte `i128` element (by-value `Result`/`Option`) at `idx`.
/// Null vec / out-of-range → 0 (matching `gos_rt_vec_get_i64`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_get_i128(v: *const GosVec, idx: i64) -> i128 {
    ffi_entry!(0, {
        if v.is_null() {
            return 0;
        }
        let vec = unsafe { &*v };
        if idx < 0 || idx >= vec.len {
            return 0;
        }
        let p = unsafe { vec.ptr.add((idx as usize) * (vec.elem_bytes as usize)) };
        unsafe { (p as *const i128).read_unaligned() }
    })
}

/// Writes the 16-byte `i128` (the by-value `Result` / `Option` carrier) at
/// `idx`. The store counterpart of [`gos_rt_vec_get_i128`]: `xs[i] = v` on a
/// `Vec` of a two-word carrier writes both words, where the i64 store would
/// keep only the discriminant and drop the payload. Invalid indexing is a
/// bounds panic, matching [`gos_rt_vec_set_i64`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_set_i128(v: *mut GosVec, idx: i64, value: i128) {
    ffi_entry!((), {
        if v.is_null() {
            crate::c_abi::panic::panic_oob_text("vec index", idx, 0);
        }
        let vec = unsafe { &mut *v };
        if idx < 0 || idx >= vec.len {
            crate::c_abi::panic::panic_oob_text("vec index", idx, vec.len);
        }
        let p = unsafe { vec.ptr.add((idx as usize) * (vec.elem_bytes as usize)) };
        unsafe { (p as *mut i128).write_unaligned(value) };
    });
}

/// Capacity a growing vec moves to: at least double the old one, and a power
/// of two. A vec that outgrows its inline buffer (a few elements wide) then
/// continues on the 4, 8, 16 sequence a vec with no inline buffer follows,
/// rather than on one offset by the inline capacity.
fn next_geometric_cap(old_cap: i64, min_cap: i64) -> i64 {
    let wanted = old_cap.saturating_mul(2).max(4).max(min_cap);
    u64::try_from(wanted)
        .ok()
        .and_then(u64::checked_next_power_of_two)
        .and_then(|cap| i64::try_from(cap).ok())
        .unwrap_or(wanted)
}

unsafe fn vec_reserve_to(vec: &mut GosVec, min_cap: i64, exact: bool) {
    let min_cap = min_cap.max(vec.len).max(0);
    if min_cap <= vec.cap {
        return;
    }
    let new_cap = if exact {
        min_cap
    } else {
        next_geometric_cap(vec.cap, min_cap)
    };
    let old_bytes = checked_buffer_bytes(vec.cap as usize, vec.elem_bytes as usize);
    let new_bytes = checked_buffer_bytes(new_cap as usize, vec.elem_bytes as usize);
    if vec_is_region(vec) {
        // Region-allocated vecs grow into a fresh region buffer and leave the
        // old one to the enclosing region's wholesale reclamation.
        let region_buf = crate::c_abi::rc::region_alloc_bytes(new_bytes);
        let new_buf = if region_buf.is_null() {
            let new_buf = alloc_vec_buffer(new_bytes);
            crate::c_abi::ledger::vec_split_alloc(
                new_bytes,
                allocator_usable_bytes(new_buf, new_bytes),
            );
            new_buf
        } else {
            crate::c_abi::ledger::vec_region_alloc(new_bytes);
            region_buf
        };
        if !vec.ptr.is_null() && old_bytes > 0 {
            unsafe {
                std::ptr::copy_nonoverlapping(vec.ptr.as_ptr(), new_buf, old_bytes);
            }
        }
        vec.ptr = SyncRawPtr::new(new_buf);
        vec.cap = new_cap;
        return;
    }

    // Spare split capacity is intentionally uninitialised; only the old live
    // slots copied below are readable.
    let new_buf = alloc_vec_buffer(new_bytes);
    crate::c_abi::ledger::vec_split_alloc(new_bytes, allocator_usable_bytes(new_buf, new_bytes));
    let was_split = vec.region_flag & VEC_SPLIT_FLAG != 0;
    if !vec.ptr.is_null() && old_bytes > 0 {
        unsafe {
            std::ptr::copy_nonoverlapping(vec.ptr.as_ptr(), new_buf, old_bytes);
        }
        if was_split {
            // The old buffer was a standalone `alloc_vec_buffer` block.
            unsafe { free_vec_buffer(vec.ptr.as_ptr(), old_bytes) };
        }
    }
    vec.ptr = SyncRawPtr::new(new_buf);
    vec.cap = new_cap;
    vec.region_flag |= VEC_SPLIT_FLAG;
}

/// The packed bytes a `GosVec` holds, or `None` when its slots are wider than
/// a byte.
///
/// A `Vec<u8>` reaches a shim either packed one byte per slot or widened to
/// the eight-byte slot a view uses, and `elem_bytes` is what says which.
///
/// # Safety
/// `v` must be null or point to a live `GosVec` whose buffer outlives the
/// returned slice.
#[must_use]
pub unsafe fn vec_byte_slice<'a>(v: *const GosVec) -> Option<&'a [u8]> {
    if v.is_null() {
        return None;
    }
    let header = unsafe { &*v };
    let len = usize::try_from(header.len.max(0)).unwrap_or(0);
    if header.ptr.is_null() || len == 0 {
        return Some(&[]);
    }
    if header.elem_bytes != 1 {
        return None;
    }
    Some(unsafe { std::slice::from_raw_parts(header.ptr.as_const_ptr(), len) })
}

/// The bytes of `v[start..end]`, borrowed where the slots are already bytes.
///
/// A reader that wants one record out of a large buffer gathers that record
/// rather than the buffer: materialising the whole of a wide-slot vector to
/// hand back a few bytes is work proportional to the file per row read.
/// `None` when the window is not inside the vector.
///
/// # Safety
/// `v` must be null or point to a live `GosVec` whose buffer outlives the
/// returned value.
#[must_use]
pub unsafe fn vec_bytes_window<'a>(
    v: *const GosVec,
    start: usize,
    end: usize,
) -> Option<std::borrow::Cow<'a, [u8]>> {
    use std::borrow::Cow;
    if v.is_null() || end < start {
        return None;
    }
    let header = unsafe { &*v };
    let len = usize::try_from(header.len.max(0)).unwrap_or(0);
    if end > len {
        return None;
    }
    if start == end {
        return Some(Cow::Borrowed(&[]));
    }
    if header.ptr.is_null() {
        return None;
    }
    if header.elem_bytes == 1 {
        return Some(Cow::Borrowed(unsafe {
            std::slice::from_raw_parts(header.ptr.as_const_ptr().add(start), end - start)
        }));
    }
    let words = unsafe {
        std::slice::from_raw_parts(
            header.ptr.as_const_ptr().cast::<i64>().add(start),
            end - start,
        )
    };
    Some(Cow::Owned(words.iter().map(|&w| w as u8).collect()))
}

/// The bytes a `GosVec` holds, borrowed where its slots are already bytes.
///
/// This is the form a reader wants: a packed buffer costs nothing to read and
/// a wide one is gathered once, with no branch at the call site.
///
/// # Safety
/// `v` must be null or point to a live `GosVec` whose buffer outlives the
/// returned value.
#[must_use]
pub unsafe fn vec_bytes_cow<'a>(v: *const GosVec) -> std::borrow::Cow<'a, [u8]> {
    match unsafe { vec_byte_slice(v) } {
        Some(slice) => std::borrow::Cow::Borrowed(slice),
        None => std::borrow::Cow::Owned(unsafe { vec_bytes(v) }),
    }
}

/// The bytes a `GosVec` holds, whatever slot width it holds them in.
///
/// Every shim that reads a byte vector goes through here or through
/// [`vec_byte_slice`], so a layout is understood by all of them or by none.
/// A wide slot contributes its low byte, as `as u8` does.
///
/// # Safety
/// `v` must be null or point to a live `GosVec`.
#[must_use]
pub unsafe fn vec_bytes(v: *const GosVec) -> Vec<u8> {
    if v.is_null() {
        return Vec::new();
    }
    let header = unsafe { &*v };
    let len = usize::try_from(header.len.max(0)).unwrap_or(0);
    if header.ptr.is_null() || len == 0 {
        return Vec::new();
    }
    if header.elem_bytes == 1 {
        return unsafe { std::slice::from_raw_parts(header.ptr.as_const_ptr(), len) }.to_vec();
    }
    let words = unsafe { std::slice::from_raw_parts(header.ptr.as_const_ptr().cast::<i64>(), len) };
    words.iter().map(|&w| w as u8).collect()
}

/// Ensures `v.capacity() >= min_cap`, growing geometrically when needed.
/// The value is a total capacity, not an additional element count.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_reserve_at_least(v: *mut GosVec, min_cap: i64) {
    ffi_entry!((), {
        if v.is_null() {
            return;
        }
        let vec = unsafe { &mut *v };
        bump_vec_mutation_generation(vec);
        unsafe { vec_reserve_to(vec, min_cap, false) };
    });
}

/// Ensures `v.capacity() >= cap` without geometric over-allocation.
/// Existing larger capacity is preserved; this function never shrinks.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_reserve_exact(v: *mut GosVec, cap: i64) {
    ffi_entry!((), {
        if v.is_null() {
            return;
        }
        let vec = unsafe { &mut *v };
        bump_vec_mutation_generation(vec);
        unsafe { vec_reserve_to(vec, cap, true) };
    });
}

/// Grows `vec` to hold `need` elements with the capacity that growing one
/// push at a time reaches.
unsafe fn vec_reserve_as_pushed(vec: &mut GosVec, need: i64) {
    if need <= vec.cap {
        return;
    }
    let mut cap = vec.cap;
    while cap < need {
        cap = next_geometric_cap(cap, cap.saturating_add(1));
    }
    unsafe { vec_reserve_to(vec, cap, true) };
}

/// Appends every byte of the string `s` to the byte vector `v`, leaving the
/// vector exactly as one [`gos_rt_vec_push`] per byte would: the same bytes in
/// order, the capacity that growth one element at a time reaches, and the
/// mutation count advanced once per byte.
///
/// # Safety
///
/// `v` is null or a live `GosVec` of unsigned bytes; `s` is null or a
/// Gossamer string body.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_extend_str_bytes(
    v: *mut GosVec,
    s: *const std::os::raw::c_char,
) {
    ffi_entry!((), {
        if v.is_null() || s.is_null() {
            return;
        }
        let bytes: &[u8] = unsafe { crate::c_abi::string::gos_str_arg_bytes(s) };
        if bytes.is_empty() {
            return;
        }
        let vec = unsafe { &mut *v };
        let added = bytes.len() as i64;
        vec.mutation_generation = vec.mutation_generation.wrapping_add(bytes.len() as u64);
        let need = vec.len.saturating_add(added);
        unsafe { vec_reserve_as_pushed(vec, need) };
        let stride = vec.elem_bytes as usize;
        let base = unsafe { vec.ptr.add((vec.len as usize) * stride) };
        if stride == 1 {
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), base, bytes.len()) };
        } else {
            for (i, &b) in bytes.iter().enumerate() {
                let word = u64::from(b).to_le_bytes();
                unsafe {
                    crate::c_abi::string::copy_small_bytes(
                        word.as_ptr(),
                        base.add(i * stride),
                        stride.min(8),
                    );
                }
            }
        }
        vec.len = need;
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_push(v: *mut GosVec, elem: *const u8) {
    ffi_entry!((), {
        if v.is_null() || elem.is_null() {
            return;
        }
        let vec = unsafe { &mut *v };
        bump_vec_mutation_generation(vec);
        if vec.len == vec.cap {
            // Grow geometrically (cap -> max(4, cap*2)).
            unsafe { vec_reserve_to(vec, vec.len.saturating_add(1), false) };
        }
        // STRING / VEC / MAP elements are pointer-sized and transferred by
        // REFERENCE: the drop pass retains the inbound value at the push site
        // (so the container holds a reference-counted element) and
        // `gos_rt_vec_free` releases each one through its `elem_kind` deep-free.
        // Storing the pointer directly - no per-push clone - lets the
        // compile-time RC own the element exactly once. The previous clone left
        // the caller's original retained-but-never-released (a per-push leak,
        // since the container held the copy, not the original). `gos_rt_str_free`
        // tag-checks each pointer at deep-free, so a stored `.rodata` literal or
        // region string is skipped rather than mis-freed.
        let dst = unsafe { vec.ptr.add((vec.len as usize) * (vec.elem_bytes as usize)) };
        if vec.elem_bytes as usize == 8
            && matches!(
                vec.elem_kind,
                vec_elem_kind::STRING
                    | vec_elem_kind::VEC
                    | vec_elem_kind::MAP
                    | vec_elem_kind::ERROR
                    | vec_elem_kind::RC_ENUM
            )
        {
            let child = unsafe { elem.cast::<*mut u8>().read_unaligned() };
            let _ = child.expose_provenance();
            unsafe { dst.cast::<*mut u8>().write_unaligned(child) };
        } else {
            unsafe {
                crate::c_abi::string::copy_small_bytes(elem, dst, vec.elem_bytes as usize);
            }
        }
        vec.len += 1;
        // A guarded aggregate element shares its copy-blob children with
        // the source slots (which keep their own shares and release them
        // when the source dies); the vec's copy must hold its own.
        if vec.elem_kind == vec_elem_kind::AGGR_GUARDED {
            let meta = vec_elem_meta(v);
            if !meta.is_null() {
                unsafe { crate::c_abi::rc::gos_rt_aggr_retain_children(dst, meta) };
            }
        }
        // Same sharing contract for owned-slot-children vecs: the source
        // slot keeps its share, the vec's copy holds its own.
        if vec.elem_kind == vec_elem_kind::AGGR_OWNED {
            unsafe { vec_retain_slot_children(v, dst) };
        }
    });
}

/// Whether [`vec_release_elem_at`] releases nothing for any element of `vec`,
/// so a run of its elements is dropped without visiting each one.
fn vec_elems_release_nothing(vec: &GosVec) -> bool {
    match vec.elem_kind {
        vec_elem_kind::AGGR_GUARDED | vec_elem_kind::AGGR_OWNED => false,
        vec_elem_kind::STRING
        | vec_elem_kind::VEC
        | vec_elem_kind::MAP
        | vec_elem_kind::RC_ENUM
        | vec_elem_kind::JSON => vec.elem_bytes as usize != 8,
        _ => true,
    }
}

/// Whether [`vec_retain_elem_at_for_copy`] takes no share and succeeds for
/// every in-bounds element of `vec`.
fn vec_elems_copy_without_retain(vec: &GosVec) -> bool {
    matches!(
        vec.elem_kind,
        vec_elem_kind::PRIMITIVE
            | vec_elem_kind::AGGR_GUARDED
            | vec_elem_kind::AGGR_OWNED
            | vec_elem_kind::AGGR_FLAT
    )
}

unsafe fn vec_release_elem_at(v: *mut GosVec, idx: i64) {
    if v.is_null() || idx < 0 {
        return;
    }
    let vec = unsafe { &*v };
    if vec.ptr.is_null() || idx >= vec.len {
        return;
    }
    let stride = vec.elem_bytes as usize;
    if stride == 0 {
        return;
    }
    let slot = unsafe { vec.ptr.add((idx as usize) * stride) };
    if vec.elem_kind == vec_elem_kind::AGGR_GUARDED {
        let meta = vec_elem_meta(vec);
        if !meta.is_null() {
            unsafe { crate::c_abi::rc::gos_rt_aggr_release_children(slot, meta) };
        }
        return;
    }
    if vec.elem_kind == vec_elem_kind::AGGR_OWNED {
        unsafe { vec_release_slot_children(vec, slot) };
        return;
    }
    if vec.elem_bytes as usize != 8 {
        return;
    }
    let raw = unsafe { slot_read_word(slot) }.expose_provenance();
    if raw == 0 {
        return;
    }
    let ptr: *mut u8 = std::ptr::with_exposed_provenance_mut(raw);
    unsafe {
        match vec.elem_kind {
            vec_elem_kind::STRING => crate::c_abi::string::gos_rt_str_free_typed(ptr.cast()),
            vec_elem_kind::VEC => crate::c_abi::map::gos_rt_vec_free(ptr.cast()),
            vec_elem_kind::MAP => crate::c_abi::map::gos_rt_map_free(ptr.cast()),
            vec_elem_kind::RC_ENUM => crate::c_abi::rc::gos_rt_rc_release(ptr),
            vec_elem_kind::JSON => crate::c_abi::json::gos_rt_json_free(ptr.cast()),
            _ => {}
        }
    }
}

/// Appends `src[idx]` to `dst`, moving the element whole at the source's own
/// stride and giving `dst` its own share of any reference-counted children.
///
/// The element's width lives in the source's header, so this is the copy a
/// sequence combinator makes whatever its element: a word, an `f64`'s bits,
/// a managed pointer, or inline aggregate slots. Answers false when the copy
/// could not be made, which leaves `dst` unchanged.
pub(crate) unsafe fn vec_push_elem_from(dst: *mut GosVec, src: *const GosVec, idx: i64) -> bool {
    if dst.is_null() || src.is_null() {
        return false;
    }
    let src_ref = unsafe { &*src };
    let stride = src_ref.elem_bytes as usize;
    if stride == 0 || src_ref.ptr.is_null() || idx < 0 || idx >= src_ref.len {
        return false;
    }
    if !unsafe { vec_retain_elem_at_for_copy(src, idx) } {
        return false;
    }
    let elem = unsafe { src_ref.ptr.add((idx as usize) * stride) };
    unsafe { gos_rt_vec_push(dst, elem) };
    true
}

/// True when `a[i]` and `b[j]` hold the same bytes at the shared element
/// stride. Both vectors must carry the same element width.
///
/// Reached from the sequence shims, which are native-only.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) unsafe fn vec_elem_bytes_eq(a: *const GosVec, i: i64, b: *const GosVec, j: i64) -> bool {
    if a.is_null() || b.is_null() {
        return false;
    }
    let (av, bv) = unsafe { (&*a, &*b) };
    let stride = av.elem_bytes as usize;
    if stride == 0 || stride != bv.elem_bytes as usize {
        return false;
    }
    if av.ptr.is_null() || bv.ptr.is_null() || i < 0 || j < 0 || i >= av.len || j >= bv.len {
        return false;
    }
    let pa = unsafe { av.ptr.add((i as usize) * stride) };
    let pb = unsafe { bv.ptr.add((j as usize) * stride) };
    unsafe { std::slice::from_raw_parts(pa, stride) == std::slice::from_raw_parts(pb, stride) }
}

unsafe fn vec_retain_elem_at_for_copy(v: *const GosVec, idx: i64) -> bool {
    if v.is_null() || idx < 0 {
        return false;
    }
    let vec = unsafe { &*v };
    if vec.ptr.is_null() || idx >= vec.len {
        return false;
    }
    let stride = vec.elem_bytes as usize;
    if stride == 0 {
        return false;
    }
    let slot = unsafe { vec.ptr.add((idx as usize) * stride) };
    if matches!(
        vec.elem_kind,
        vec_elem_kind::PRIMITIVE
            | vec_elem_kind::AGGR_GUARDED
            | vec_elem_kind::AGGR_OWNED
            | vec_elem_kind::AGGR_FLAT
    ) {
        return true;
    }
    if vec.elem_bytes as usize != 8 {
        return false;
    }
    let raw = unsafe { slot_read_word(slot) }.expose_provenance();
    if raw == 0 {
        return true;
    }
    let ptr: *mut u8 = std::ptr::with_exposed_provenance_mut(raw);
    unsafe {
        match vec.elem_kind {
            vec_elem_kind::STRING => crate::c_abi::string::gos_rt_str_retain(ptr.cast()),
            vec_elem_kind::VEC => vec_retain_header(ptr.cast()),
            vec_elem_kind::RC_ENUM | vec_elem_kind::ERROR => {
                crate::c_abi::rc::gos_rt_rc_retain(ptr);
            }
            // A JSON handle carries no count, so the copy takes a box of its
            // own onto the same document and the slot names that one.
            vec_elem_kind::JSON => {
                let cloned = crate::c_abi::json::gos_rt_json_clone_handle(ptr.cast());
                slot_write_word(slot, cloned.cast::<u8>());
            }
            // A GosMap carries no count, so a shared element has no protocol.
            vec_elem_kind::MAP => return false,
            _ => {}
        }
    }
    true
}

/// `v.clear()` - drop all live elements and keep capacity.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_clear(v: *mut GosVec) {
    ffi_entry!((), {
        if v.is_null() {
            return;
        }
        let len = unsafe { (*v).len.max(0) };
        unsafe { bump_vec_mutation_generation(&mut *v) };
        if !vec_elems_release_nothing(unsafe { &*v }) {
            for idx in 0..len {
                unsafe { vec_release_elem_at(v, idx) };
            }
        }
        unsafe {
            (*v).len = 0;
        }
    });
}

/// `v.truncate(n)` - drop elements at indices `n..len`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_truncate(v: *mut GosVec, len: i64) {
    ffi_entry!((), {
        if v.is_null() {
            return;
        }
        if len < 0 {
            crate::c_abi::panic::panic_text("truncate: length must be non-negative");
        }
        let new_len = len;
        let old_len = unsafe { (*v).len.max(0) };
        if new_len >= old_len {
            return;
        }
        unsafe { bump_vec_mutation_generation(&mut *v) };
        if !vec_elems_release_nothing(unsafe { &*v }) {
            for idx in new_len..old_len {
                unsafe { vec_release_elem_at(v, idx) };
            }
        }
        unsafe {
            (*v).len = new_len;
        }
    });
}

/// `*dst = src` through a `&mut Vec<T>`: the header every holder of the
/// reference names keeps its identity and takes a copy of `src`'s elements,
/// sharing each the way `gos_rt_vec_clone` does, after releasing the
/// elements it held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_assign(dst: *mut GosVec, src: *const GosVec) {
    ffi_entry!((), {
        if dst.is_null() || src.is_null() || std::ptr::addr_eq(dst.cast_const(), src) {
            return;
        }
        unsafe { gos_rt_vec_clear(dst) };
        let s = unsafe { &*src };
        let len = s.len.max(0);
        let stride = s.elem_bytes as usize;
        {
            let d = unsafe { &mut *dst };
            // The emptied buffer holds bytes, so a new slot width recounts
            // its capacity rather than reallocating it.
            if d.elem_bytes != s.elem_bytes {
                let bytes = d.cap.max(0) as usize * d.elem_bytes as usize;
                d.elem_bytes = s.elem_bytes;
                d.cap = bytes.checked_div(stride).unwrap_or(0) as i64;
            }
            d.elem_kind = s.elem_kind;
            unsafe { vec_reserve_to(d, len, true) };
        }
        if len > 0 && stride > 0 && !s.ptr.is_null() {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    s.ptr.as_ptr(),
                    (*dst).ptr.as_ptr(),
                    len as usize * stride,
                );
            }
        }
        unsafe { (*dst).len = len };
        unsafe { vec_adopt_element_shares(src, dst) };
    });
}

/// `v.extend(xs)` / `v.extend_from_slice(xs)` / `v.append(xs)`.
///
/// Copies elements from `src` into `dst`. Pointer-bearing elements are retained
/// when the runtime has a retain protocol; map/error element vecs are left
/// unchanged rather than shallow-copied unsafely.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_extend(dst: *mut GosVec, src: *const GosVec) {
    ffi_entry!((), {
        if dst.is_null() || src.is_null() {
            return;
        }
        if std::ptr::addr_eq(dst.cast_const(), src) {
            let snapshot = unsafe { crate::c_abi::gos_rt_vec_clone(src) };
            if snapshot.is_null() {
                return;
            }
            unsafe { gos_rt_vec_extend(dst, snapshot) };
            unsafe { crate::c_abi::map::gos_rt_vec_free(snapshot) };
            return;
        }
        let src_ref = unsafe { &*src };
        let dst_ref = unsafe { &*dst };
        if src_ref.elem_bytes != dst_ref.elem_bytes || src_ref.elem_kind != dst_ref.elem_kind {
            return;
        }
        if src_ref.elem_kind == vec_elem_kind::MAP {
            return;
        }
        let len = src_ref.len.max(0);
        let stride = src_ref.elem_bytes as usize;
        if stride == 0 || src_ref.ptr.is_null() {
            return;
        }
        // Flat elements own no heap children, so one copy of the block leaves
        // `dst` exactly as a push per element would.
        if matches!(
            src_ref.elem_kind,
            vec_elem_kind::PRIMITIVE | vec_elem_kind::AGGR_FLAT
        ) {
            if len == 0 {
                return;
            }
            let dst_mut = unsafe { &mut *dst };
            dst_mut.mutation_generation = dst_mut.mutation_generation.wrapping_add(len as u64);
            let need = dst_mut.len.saturating_add(len);
            unsafe { vec_reserve_as_pushed(dst_mut, need) };
            unsafe {
                std::ptr::copy_nonoverlapping(
                    src_ref.ptr.as_ptr(),
                    dst_mut.ptr.add((dst_mut.len as usize) * stride),
                    (len as usize) * stride,
                );
            }
            dst_mut.len = need;
            return;
        }
        for idx in 0..len {
            if !unsafe { vec_retain_elem_at_for_copy(src, idx) } {
                return;
            }
            let elem = unsafe { src_ref.ptr.add((idx as usize) * stride) };
            unsafe { gos_rt_vec_push(dst, elem) };
        }
    });
}

// Tagged-union encoding for `Result<T, E>` and `Option<T>`: a 2-word
// BY-VALUE `i128` with the discriminant in the low 64 bits and the
// payload in the high 64 bits. Convention: `disc == 0` = Ok / Some,
// `disc == 1` = Err / None - the distinguishing bit pattern dispatch
// reads. Construction is a register pack with zero allocation; the
// payload flows as a normal value (a scalar, or a pointer to a
// heap-copied aggregate) managed by RC like any other binding.

/// Pack `(disc, payload)` into the 2-word Result/Option value.
#[inline]
#[must_use]
pub fn pack_result(disc: i64, payload: i64) -> i128 {
    (((payload as u64 as u128) << 64) | (disc as u64 as u128)) as i128
}

#[inline]
pub(crate) fn result_disc_of(r: i128) -> i64 {
    (r as u128 as u64) as i64
}

#[inline]
pub(crate) fn result_payload_of(r: i128) -> i64 {
    ((r as u128 >> 64) as u64) as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_new(disc: i64, payload: i64) -> i128 {
    pack_result(disc, payload)
}

/// `gos_rt_result_new` for a carrier whose aggregate payload took the share
/// its source was holding.
///
/// The packing is the same two words; the spelling is what tells a back end
/// to box the payload by taking those words rather than minting a second
/// share of the children they name. A back end that boxes before the call
/// reaches this entry unchanged.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_new_owned(disc: i64, payload: i64) -> i128 {
    pack_result(disc, payload)
}

/// `gos_rt_result_new` variant for f64 payloads - stores the value's
/// `to_bits()` so the symmetric `gos_rt_result_payload_f64` reads back the
/// original f64.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_new_f64(disc: i64, payload: f64) -> i128 {
    pack_result(disc, payload.to_bits() as i64)
}

/// Converts a `[rust-bindings]` `*mut GosVariant` (the binding ABI's
/// `Result` / `Option` wire shape, tag `1` = Ok/Some) into the
/// runtime's packed i128 result (disc `0` = Ok/Some). String payloads
/// arrive as bare NUL-terminated arena bytes and are re-allocated as
/// header'd runtime strings; every other payload word passes through
/// bit-exact (i64/bool/char values, f64 bits, GosVec / nested
/// pointers).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_binding_variant_to_result(p: *const u8) -> i128 {
    ffi_entry!(0i128, {
        if p.is_null() {
            return pack_result(1, 0);
        }
        // GosVariant layout (repr(C) in gossamer-binding):
        // tag i32 | payload_len i32 | payload *mut GosVariantValue.
        let tag = unsafe { *p.cast::<i32>() };
        let payload_len = unsafe { *p.add(4).cast::<i32>() };
        let payload_ptr = unsafe { *p.add(8).cast::<*const u8>() };
        let disc = i64::from(tag != 1);
        if payload_len <= 0 || payload_ptr.is_null() {
            return pack_result(disc, 0);
        }
        // GosVariantValue layout: tag i32 | (pad) | data union at +8.
        // Value tag 4 = string; see `gossamer-binding::native`.
        let value_tag = unsafe { *payload_ptr.cast::<i32>() };
        let word = unsafe { *payload_ptr.add(8).cast::<i64>() };
        let payload = if value_tag == 4 && word != 0 {
            // HOST-CSTRING: a native Rust binding owns this pointer and
            // publishes it as a NUL-terminated C string, not a Gossamer
            // `String`, so it carries no length header.
            let c = unsafe { std::ffi::CStr::from_ptr(word as *const std::ffi::c_char) };
            super::string::alloc_cstring(c.to_bytes()) as i64
        } else {
            word
        };
        pack_result(disc, payload)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_disc(r: i128) -> i64 {
    result_disc_of(r)
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_dbg(p: i64) -> i64 {
    eprintln!("[rt] dbg called with raw i64 = {p:#x}");
    p
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_payload(r: i128) -> i64 {
    result_payload_of(r)
}

/// `Result<f64, _>` / `Option<f64>` Ok-payload extractor that reinterprets
/// the stored bits as f64.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_payload_f64(r: i128) -> f64 {
    f64::from_bits(result_payload_of(r) as u64)
}

/// Payload extractor for payloads that are themselves a 2-word
/// by-value enum (`Result<Option<T>, E>`, nested Results, inline
/// user enums). Construction heap-copied the inner 2-word value and
/// stored its address in the payload word; this loads it back by
/// value so the destination local holds `[disc, payload]` directly.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_payload_i128(r: i128) -> i128 {
    let addr = result_payload_of(r);
    if addr == 0 {
        return 0;
    }
    // SAFETY: the payload word of an enum-payload Result/Option is a
    // pointer to the live 16-byte heap copy made at construction
    // (`gos_rt_result_new` aggregate path).
    unsafe {
        let p = addr as usize as *const i64;
        let lo = (*p) as u64 as u128;
        let hi = (*p.add(1)) as u64 as u128;
        ((hi << 64) | lo) as i128
    }
}

/// Signature of a derived `Type::fmt`: it reads the value's flat slot buffer
/// and returns a freshly allocated runtime String the caller owns.
type AdtFmt = unsafe extern "C" fn(*const u8) -> *mut std::ffi::c_char;

/// Renders one aggregate by calling the derived `fmt` at `fmt`, taking
/// ownership of the String it returns. `value` carries whatever that `fmt`
/// receives as its receiver: a struct's slot address, or an inline enum's
/// own word - which may be zero for a unit variant, so it is not guarded.
pub(crate) unsafe fn adt_fmt_string(value: *const u8, fmt: *const std::ffi::c_void) -> String {
    if fmt.is_null() {
        return String::new();
    }
    // SAFETY: callers pass the address of a derived `Type::fmt`, emitted with
    // the `ptr(ptr)` signature `AdtFmt` names, and `value` is the receiver
    // that `fmt` expects for its type.
    let f: AdtFmt = unsafe { std::mem::transmute::<*const std::ffi::c_void, AdtFmt>(fmt) };
    unsafe { take_rt_string(f(value)) }
}

/// Renders one enum payload word, extending [`debug_payload_string`] with the
/// aggregate tag: the word is then the address of the payload's slot buffer
/// and `fmt` its derived formatter.
fn debug_payload_string_with(payload: i64, kind: i64, fmt: *const std::ffi::c_void) -> String {
    if kind == i64::from(gossamer_abi::DEBUG_PAYLOAD_ADT) {
        let slots: *const u8 = std::ptr::with_exposed_provenance(payload as usize);
        return unsafe { adt_fmt_string(slots, fmt) };
    }
    // A tuple payload is its slot buffer, and `fmt` addresses a tag stream
    // that opens with the nested marker and the tuple's arity.
    if kind == i64::from(gossamer_abi::DEBUG_PAYLOAD_TUPLE) {
        let slots: *const i64 = std::ptr::with_exposed_provenance(payload as usize);
        let tags: *const u8 = fmt.cast();
        if slots.is_null() || tags.is_null() {
            return String::new();
        }
        let arity = unsafe { *tags.add(1) } as usize;
        let mut out = String::new();
        let mut slot_cursor = 0usize;
        let mut tag_cursor = 2usize;
        unsafe {
            crate::c_abi::map::render_tuple_elements(
                &mut out,
                slots,
                crate::c_abi::map::DescStream::bare(tags),
                arity,
                &mut slot_cursor,
                &mut tag_cursor,
            );
        }
        return out;
    }
    // A descriptor payload renders through the recursive walk, so a nested
    // container needs no formatter of its own.
    if kind == i64::from(gossamer_abi::DEBUG_PAYLOAD_DESC) {
        let tags: *const u8 = fmt.cast();
        if tags.is_null() {
            return String::new();
        }
        let tags = unsafe { crate::c_abi::map::DescStream::new(tags) };
        let mut out = String::new();
        let mut cursor = 0usize;
        let slot = std::ptr::from_ref(&payload).cast::<u8>();
        unsafe {
            crate::c_abi::map::render_desc_storage(
                &mut out,
                slot,
                tags,
                &mut cursor,
                crate::c_abi::map::Storage::ByWord,
            );
        }
        return out;
    }
    debug_payload_string(payload, kind)
}

/// Renders a single enum payload word for `{:?}` Debug output, matching the
/// VM's Display-style rendering (no string quoting). `kind`: 0=i64, 1=u64,
/// 2=f64 (bit pattern), 3=bool, 4=char, 5=String pointer.
fn debug_payload_string(payload: i64, kind: i64) -> String {
    match kind {
        1 => (payload as u64).to_string(),
        2 => crate::builtins::format_float_debug(f64::from_bits(payload as u64)),
        3 => if payload != 0 { "true" } else { "false" }.to_string(),
        4 => char::from_u32(payload as u32).map_or_else(String::new, |c| c.to_string()),
        5 => {
            if payload == 0 {
                String::new()
            } else {
                let sptr: *const std::ffi::c_char =
                    std::ptr::with_exposed_provenance(payload as usize);
                unsafe { crate::c_abi::gos_str_arg_string(sptr) }
            }
        }
        // A collection payload arrives as its `GosVec` pointer, so the
        // element formatter that renders a bare `{:?}` of that vec renders
        // it inside the variant too.
        6 => unsafe { take_rt_string(super::btmap::gos_rt_vec_format_i64(vec_ptr(payload), 0)) },
        7 => unsafe { take_rt_string(super::btmap::gos_rt_vec_format_string(vec_ptr(payload), 0)) },
        8 => unsafe {
            take_rt_string(crate::c_abi::gos_rt_json_display(
                std::ptr::with_exposed_provenance(payload as usize),
            ))
        },
        // An error payload renders as the colon-joined cause chain, the way
        // a bare `{}` on the error does.
        10 => unsafe {
            take_rt_string(crate::c_abi::gos_rt_error_display(
                std::ptr::with_exposed_provenance(payload as usize),
            ))
        },
        // A unit payload carries no value: the arm renders as `()`.
        13 => "()".to_string(),
        _ => payload.to_string(),
    }
}

/// Reinterprets a payload slot as the `GosVec` pointer it holds.
fn vec_ptr(payload: i64) -> *const crate::c_abi::GosVec {
    std::ptr::with_exposed_provenance(payload as usize)
}

/// Consumes a runtime-allocated C string into an owned `String`, freeing the
/// allocation the formatter handed back.
unsafe fn take_rt_string(ptr: *mut std::ffi::c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let out = unsafe { crate::c_abi::gos_str_arg_string(ptr) };
    unsafe { super::string::gos_rt_str_free(ptr) };
    out
}

/// `{:?}` of an `Option<T>` (the by-value `i128` enum, disc 0 = Some): renders
/// `Some(<payload>)` or `None`, matching the VM. `payload_kind` selects the
/// payload formatter (see `debug_payload_string`).
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_debug_option(opt: i128, payload_kind: i64) -> *mut std::ffi::c_char {
    let s = if result_disc_of(opt) != 0 {
        "None".to_string()
    } else {
        format!(
            "Some({})",
            debug_payload_string(result_payload_of(opt), payload_kind)
        )
    };
    super::string::alloc_cstring(s.as_bytes())
}

/// `{:?}` of a `Result<T, E>` (the by-value `i128` enum, disc 0 = Ok): renders
/// `Ok(<payload>)` or `Err(<payload>)`, matching the VM. `ok_kind` / `err_kind`
/// select the per-arm payload formatter.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_debug_result(
    res: i128,
    ok_kind: i64,
    err_kind: i64,
) -> *mut std::ffi::c_char {
    let payload = result_payload_of(res);
    let s = if result_disc_of(res) == 0 {
        format!("Ok({})", debug_payload_string(payload, ok_kind))
    } else {
        format!("Err({})", debug_payload_string(payload, err_kind))
    };
    super::string::alloc_cstring(s.as_bytes())
}

/// [`gos_rt_debug_option`] for an `Option` whose payload is an aggregate:
/// `payload_kind` may be `gossamer_abi::DEBUG_PAYLOAD_ADT`, in which case `fmt` is the
/// payload type's derived formatter.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_debug_option_fmt(
    opt: i128,
    payload_kind: i64,
    fmt: *const std::ffi::c_void,
) -> *mut std::ffi::c_char {
    let s = if result_disc_of(opt) != 0 {
        "None".to_string()
    } else {
        format!(
            "Some({})",
            debug_payload_string_with(result_payload_of(opt), payload_kind, fmt)
        )
    };
    super::string::alloc_cstring(s.as_bytes())
}

/// [`gos_rt_debug_result`] for a `Result` with an aggregate arm: either kind
/// may be `gossamer_abi::DEBUG_PAYLOAD_ADT`, with the matching `fmt` naming that arm's
/// derived formatter.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_debug_result_fmt(
    res: i128,
    ok_kind: i64,
    err_kind: i64,
    ok_fmt: *const std::ffi::c_void,
    err_fmt: *const std::ffi::c_void,
) -> *mut std::ffi::c_char {
    let payload = result_payload_of(res);
    let s = if result_disc_of(res) == 0 {
        format!(
            "Ok({})",
            debug_payload_string_with(payload, ok_kind, ok_fmt)
        )
    } else {
        format!(
            "Err({})",
            debug_payload_string_with(payload, err_kind, err_fmt)
        )
    };
    super::string::alloc_cstring(s.as_bytes())
}

/// `result.unwrap()` / `option.unwrap()`. Returns the payload on the happy
/// path; panics on Err / None.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_result_unwrap(r: i128) -> i64 {
    ffi_entry!(-1, {
        if result_disc_of(r) != 0 {
            crate::c_abi::panic::panic_text("called `Result::unwrap()` on an `Err` value");
            return 0;
        }
        result_payload_of(r)
    })
}

/// `option.unwrap()` / `option.expect(msg)`. Shares the two-word carrier with
/// [`gos_rt_result_unwrap`] and differs only in the message the empty case
/// panics with, which names the shape the program actually wrote.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_option_unwrap(r: i128) -> i64 {
    ffi_entry!(-1, {
        if result_disc_of(r) != 0 {
            crate::c_abi::panic::panic_text("called `Option::unwrap()` on a `None` value");
            return 0;
        }
        result_payload_of(r)
    })
}

/// The carrier a two-word payload was boxed as, read back by value. A null box
/// reads as `None`.
fn boxed_carrier_of(r: i128) -> i128 {
    let boxed: *const u8 = std::ptr::with_exposed_provenance(result_payload_of(r) as usize);
    if boxed.is_null() {
        return gos_rt_result_new(1, 0);
    }
    // SAFETY: the payload word of a carrier whose payload is itself a carrier
    // is the address of the 16-byte box the constructor copied it into, alive
    // for as long as the outer carrier is.
    unsafe { boxed.cast::<i128>().read_unaligned() }
}

/// `option.unwrap()` / `option.expect(msg)` where the payload is itself an
/// `Option` / `Result`. The answer is the boxed carrier as it stands; the box
/// keeps its own share of that carrier's payload.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_option_unwrap_carrier(r: i128) -> i128 {
    ffi_entry!(0, {
        if result_disc_of(r) != 0 {
            crate::c_abi::panic::panic_text("called `Option::unwrap()` on a `None` value");
            return 0;
        }
        boxed_carrier_of(r)
    })
}

/// `result.unwrap()` / `result.expect(msg)` where the `Ok` payload is itself an
/// `Option` / `Result`, answered as [`gos_rt_option_unwrap_carrier`] does.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_result_unwrap_carrier(r: i128) -> i128 {
    ffi_entry!(0, {
        if result_disc_of(r) != 0 {
            crate::c_abi::panic::panic_text("called `Result::unwrap()` on an `Err` value");
            return 0;
        }
        boxed_carrier_of(r)
    })
}

/// `result.unwrap_or(default)` / `option.unwrap_or(default)`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_unwrap_or(r: i128, default: i64) -> i64 {
    if result_disc_of(r) == 0 {
        result_payload_of(r)
    } else {
        default
    }
}

/// `unwrap_or` where the value is a `Vec` / `[T]`.
///
/// The caller holds one share of the fallback and releases it where the
/// fallback's own binding ends, and one share of the answer. When the fallback
/// IS the answer those are the same Vec, so it needs the second share the
/// caller is going to give back; the word-returning form above cannot tell the
/// two apart and left the caller releasing one Vec twice.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_unwrap_or_vec(r: i128, default: i64) -> i64 {
    if result_disc_of(r) == 0 {
        return result_payload_of(r);
    }
    if default != 0 {
        unsafe { crate::c_abi::vec::gos_rt_vec_retain(default as usize as *mut GosVec) };
    }
    default
}

/// `unwrap_or` where the value is a `String`.
///
/// The caller holds one share of the fallback and releases it where the
/// fallback's own binding ends, and one share of the answer. When the fallback
/// IS the answer those are the same string, so it needs the second share the
/// caller is going to give back; the word-returning form above cannot tell the
/// two apart and left the caller releasing one string twice.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_unwrap_or_str(r: i128, default: i64) -> i64 {
    if result_disc_of(r) == 0 {
        return result_payload_of(r);
    }
    if default != 0 {
        // SAFETY: a non-zero `String` word is a runtime string body; the
        // retain answers a range test and ignores anything else.
        unsafe {
            crate::c_abi::string::gos_rt_str_retain_typed(
                default as usize as *const std::ffi::c_char,
            );
        }
    }
    default
}

/// `unwrap_or` where the value is a counted node: a payload-enum node or a
/// callable's environment.
///
/// The answer carries a share of its own on both arms, as
/// [`gos_rt_result_unwrap_or_str`] does: the payload's share moves out of the
/// carrier, and a fallback that becomes the answer takes a second share, since
/// its own binding still gives one back.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_unwrap_or_node(r: i128, default: i64) -> i64 {
    if result_disc_of(r) == 0 {
        return result_payload_of(r);
    }
    // SAFETY: a non-null word of a counted-node type points at a live RC
    // allocation, and the retain masks the enum tag bits.
    unsafe { crate::c_abi::rc::gos_rt_rc_retain(default as usize as *mut u8) };
    default
}

/// Reads the two-word carrier a map keeps boxed, answering `None` for a null
/// box: a map reader answers the box's address rather than the carrier.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_carrier_from_box(word: i64) -> i128 {
    let boxed: *const u8 = std::ptr::with_exposed_provenance(word as usize);
    if boxed.is_null() {
        return gos_rt_result_new(1, 0);
    }
    // SAFETY: a non-null word a map reader answers for a two-word value is the
    // address of the entry's 16-byte carrier.
    unsafe { boxed.cast::<i128>().read_unaligned() }
}

/// Takes a share of a carrier's `Ok` / `Some` `String` payload, for a field
/// copy that holds the carrier alongside its source. A no-op on the other arm.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_option_str_payload_retain(r: i128) {
    if result_disc_of(r) != 0 || result_payload_of(r) == 0 {
        return;
    }
    // SAFETY: the payload word of an `Ok`/`Some` arm whose static type is a
    // `String` is a runtime string body.
    unsafe {
        crate::c_abi::string::gos_rt_str_retain_typed(
            result_payload_of(r) as usize as *const std::ffi::c_char
        );
    }
}

/// Gives back a carrier field's `Ok` / `Some` `String` payload at the field's
/// death. A no-op on the other arm.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_option_str_payload_release(r: i128) {
    gos_rt_result_ok_payload_release(r, 1);
}

/// Takes a share of a carrier's `Ok` / `Some` `Vec` payload, for a field copy
/// that holds the carrier alongside its source. A no-op on the other arm.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_option_vec_payload_retain(r: i128) {
    if result_disc_of(r) != 0 || result_payload_of(r) == 0 {
        return;
    }
    // SAFETY: the payload word of an `Ok`/`Some` arm whose static type is a
    // `Vec` is a live `GosVec` header.
    unsafe { crate::c_abi::gos_rt_vec_retain(result_payload_of(r) as usize as *mut GosVec) };
}

/// Gives back a carrier field's `Ok` / `Some` `Vec` payload at the field's
/// death. A no-op on the other arm.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_option_vec_payload_release(r: i128) {
    gos_rt_result_ok_payload_release(r, 2);
}

/// Releases the heap payload of a carrier's `Ok` / `Some` arm, and nothing on
/// the other arm, whose payload word belongs to the error value.
///
/// `kind` names the payload's storage: 1 a `String`, 2 a `Vec` / slice, 3 a
/// `json::Value` handle, 4 an `errors::Error` cell. This is the give-back for `map`, which hands the
/// payload to a closure that answers a value of its own; the carrier itself
/// never releases a payload of any of those kinds.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_ok_payload_release(r: i128, kind: i64) {
    if result_disc_of(r) != 0 {
        return;
    }
    let payload = result_payload_of(r);
    if payload == 0 {
        return;
    }
    match kind {
        // SAFETY: the payload word of an `Ok`/`Some` arm whose static type is
        // a `String` is a runtime string body.
        1 => unsafe {
            crate::c_abi::string::gos_rt_str_free_typed(payload as usize as *mut std::ffi::c_char);
        },
        // SAFETY: as above, for a `Vec` header.
        2 => unsafe {
            crate::c_abi::gos_rt_vec_free(payload as usize as *mut GosVec);
        },
        // SAFETY: as above, for a `GosJson` handle box. The box holds one
        // share of the parsed document, so giving it back is what lets the
        // document die with the last handle onto it.
        3 => unsafe {
            crate::c_abi::json::gos_rt_json_free(
                payload as usize as *mut crate::c_abi::json::GosJson,
            );
        },
        // SAFETY: as above, for an error cell.
        4 => unsafe { crate::c_abi::rc::gos_rt_rc_release(payload as usize as *mut u8) },
        _ => {}
    }
}

/// Releases the heap payload of whichever arm a carrier holds: `ok_kind` names
/// the `Ok` / `Some` payload's storage and `err_kind` the `Err` payload's, in
/// the kinds [`gos_rt_result_ok_payload_release`] takes, with 0 for an arm
/// whose payload is not heap storage the carrier owns.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_payload_release(r: i128, ok_kind: i64, err_kind: i64) {
    match result_disc_of(r) {
        0 => gos_rt_result_ok_payload_release(r, ok_kind),
        // The error arm's payload word is laid out as the ok arm's is, so the
        // same release reads it once the discriminant has chosen the kind.
        1 => gos_rt_result_ok_payload_release(gos_rt_result_new(0, result_payload_of(r)), err_kind),
        _ => {}
    }
}

/// Takes a share of the heap payload of whichever arm a carrier holds, by the
/// kinds [`gos_rt_result_payload_release`] takes: 1 a `String`, 2 a `Vec`, 4 an
/// `errors::Error` cell, 0 an arm with nothing to share.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_payload_retain(r: i128, ok_kind: i64, err_kind: i64) {
    let kind = match result_disc_of(r) {
        0 => ok_kind,
        1 => err_kind,
        _ => 0,
    };
    let payload = result_payload_of(r);
    if payload == 0 {
        return;
    }
    match kind {
        // SAFETY: the payload word of an arm whose static type is a `String`
        // is a runtime string body.
        1 => unsafe {
            crate::c_abi::string::gos_rt_str_retain_typed(
                payload as usize as *const std::ffi::c_char,
            );
        },
        // SAFETY: as above, for a live `GosVec` header.
        2 => unsafe { crate::c_abi::gos_rt_vec_retain(payload as usize as *mut GosVec) },
        // SAFETY: as above, for a live error cell.
        4 => unsafe { crate::c_abi::rc::gos_rt_rc_retain(payload as usize as *mut u8) },
        _ => {}
    }
}

/// `unwrap_or` where the payload is itself a `Result` / `Option` carrier -
/// `spawn(f).join().unwrap_or(Ok(v))` is the shape that reaches it.
///
/// A carrier is two words, so the word-returning `unwrap_or` above keeps
/// only the payload half and loses the discriminant, which reads back as
/// an `Err` whatever the value was.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_unwrap_or_carrier(r: i128, default: i128) -> i128 {
    if result_disc_of(r) != 0 {
        return default;
    }
    // A carrier does not fit the payload half, so a nested one is boxed
    // and the payload holds its address. Read the carrier back out.
    let boxed = result_payload_of(r) as usize as *const i128;
    if boxed.is_null() {
        return default;
    }
    // SAFETY: the pointer is the 16-byte aggregate `gos_rt_aggr_alloc`
    // handed the constructor, alive for as long as the carrier is.
    unsafe { boxed.read_unaligned() }
}

/// `result.ok()` / `option.ok()`. Returns the payload on Ok/Some, else 0.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_ok(r: i128) -> i64 {
    if result_disc_of(r) == 0 {
        result_payload_of(r)
    } else {
        0
    }
}

/// `result.err()`. Returns the error payload on Err, else 0.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_err(r: i128) -> i64 {
    if result_disc_of(r) == 1 {
        result_payload_of(r)
    } else {
        0
    }
}

/// `option.ok_or(new_err)`. On Some, answers the receiver and gives back the
/// replacement the caller handed over; on None, answers `Err(new_err)`, which
/// takes that share. The call consumes `new_err` on either arm, so the caller
/// hands its share over once and never gives it back itself. `err_kind` names
/// the replacement's storage in the kinds
/// [`gos_rt_result_ok_payload_release`] takes.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_ok_or(r: i128, new_err: i64, err_kind: i64) -> i128 {
    if result_disc_of(r) == 0 {
        gos_rt_result_ok_payload_release(pack_result(0, new_err), err_kind);
        r
    } else {
        pack_result(1, new_err)
    }
}

/// `result.is_ok()` / `option.is_some()`. 1 on Ok/Some, 0 on Err/None.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_is_ok(r: i128) -> i64 {
    i64::from(result_disc_of(r) == 0)
}

/// `result.is_err()` / `option.is_none()`. 1 on Err/None, 0 on Ok/Some.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_is_err(r: i128) -> i64 {
    i64::from(result_disc_of(r) != 0)
}

/// Maps a `gos_main` return value to a process exit code. A
/// `Result`-returning `main` yields its discriminant (`0` = `Ok`,
/// `1` = `Err`) in the low word; a unit or integer `main` yields its
/// value directly - both are the exit code. Also blocks until every
/// outstanding goroutine has settled so their stdout reaches the user
/// before the process exits.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_main_exit_code(raw: i64) -> i32 {
    ffi_entry!(-1, {
        // Drain goroutines spawned via `go expr` before exiting:
        // `live_goroutines` is incremented at spawn admission, so a
        // fast `go expr; return` main observes its goroutine here, and
        // a body that finished has already written into the buffered
        // stdout the flush below drains. Skips (and does not boot) the
        // scheduler when the program never used it.
        // Flush before the root cohort reports: stdout is buffered here
        // and stderr is not, so a report printed first would appear
        // ahead of output the program had already written.
        unsafe { gos_rt_flush_stdout() };
        // The root cohort joins what `main` spawned, and reports any
        // failure nothing observed, before the drain below.
        crate::c_abi::cohort::close_root();
        crate::sched_global::drain_goroutines_for_exit();
        // Flush any buffered stdout that workers wrote so it
        // reaches the user before the process exits.
        unsafe { gos_rt_flush_stdout() };
        raw as i32
    })
}

/// Entry-point exit handler for a `main` that returns a `Result`, given its
/// unpacked discriminant and payload word (the native `@main` shim truncates
/// the packed i128 to two i64s to avoid the i128 C-ABI). Drains goroutines and
/// flushes stdout, then: `Ok` (disc 0) exits 0; `Err` additionally renders the
/// error's Display (colon-joined cause chain) to stderr before exiting 1 - so a
/// propagated entry-point error is reported instead of silently dropped,
/// matching the VM tier.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_main_exit_code_err(disc: i64, payload: i64) -> i32 {
    ffi_entry!(-1, {
        unsafe { gos_rt_flush_stdout() };
        crate::c_abi::cohort::close_root();
        crate::sched_global::drain_goroutines_for_exit();
        unsafe { gos_rt_flush_stdout() };
        if disc == 0 {
            return 0;
        }
        if payload != 0 {
            let msg = unsafe { crate::c_abi::gos_rt_error_display(payload as *const _) };
            if !msg.is_null() {
                unsafe {
                    crate::c_abi::gos_rt_eprint_str(msg);
                    crate::c_abi::gos_rt_eprintln();
                    crate::c_abi::gos_rt_str_free(msg);
                }
            }
        }
        1
    })
}

/// `v.copy_within(src, dest, len)` - move `len` elements from `src` to
/// `dest` inside one Vec.
///
/// The source and destination ranges may overlap, which is the whole
/// reason the operation exists: a page defragment shifts a region over
/// itself. The bytes are staged through a temporary so an overlapping
/// move reads the original contents, and every element the destination
/// range drops is released before the copy lands on it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_copy_within(v: *mut GosVec, src: i64, dest: i64, len: i64) {
    ffi_entry!((), {
        if v.is_null() {
            crate::c_abi::panic::panic_text("copy_within: null vector");
        }
        let vec_len = unsafe { (*v).len.max(0) };
        if src < 0 || dest < 0 || len < 0 || src + len > vec_len || dest + len > vec_len {
            crate::c_abi::panic::panic_text("copy_within: range outside the vector");
        }
        if len == 0 || src == dest {
            return;
        }
        let stride = unsafe { (*v).elem_bytes } as usize;
        let base = unsafe { (*v).ptr };
        if stride == 0 || base.is_null() {
            return;
        }
        unsafe { bump_vec_mutation_generation(&mut *v) };
        if !vec_elems_copy_without_retain(unsafe { &*v }) {
            for offset in 0..len {
                if !unsafe { vec_retain_elem_at_for_copy(v, src + offset) } {
                    crate::c_abi::panic::panic_text("copy_within: element type cannot be copied");
                }
            }
        }
        let span = (len as usize) * stride;
        let mut staged = vec![0u8; span];
        unsafe {
            std::ptr::copy_nonoverlapping(
                base.add((src as usize) * stride),
                staged.as_mut_ptr(),
                span,
            );
        }
        if !vec_elems_release_nothing(unsafe { &*v }) {
            for offset in 0..len {
                unsafe { vec_release_elem_at(v, dest + offset) };
            }
        }
        unsafe {
            std::ptr::copy_nonoverlapping(
                staged.as_ptr(),
                base.add((dest as usize) * stride),
                span,
            );
        }
    });
}

/// `dst.copy_from_slice(src)` - overwrite every element of `dst` with the
/// matching element of `src`. Both sequences must have the same length,
/// exactly as the operation reads.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_vec_copy_from_slice(dst: *mut GosVec, src: *const GosVec) {
    ffi_entry!((), {
        if dst.is_null() || src.is_null() {
            crate::c_abi::panic::panic_text("copy_from_slice: null vector");
        }
        if std::ptr::addr_eq(dst.cast_const(), src) {
            return;
        }
        let dst_len = unsafe { (*dst).len.max(0) };
        let src_len = unsafe { (*src).len.max(0) };
        if dst_len != src_len {
            unsafe {
                crate::c_abi::panic::panic_text(
                    "copy_from_slice: source and destination differ in length",
                );
            }
        }
        let stride = unsafe { (*dst).elem_bytes } as usize;
        if unsafe { (*src).elem_bytes } as usize != stride
            || unsafe { (*src).elem_kind } != unsafe { (*dst).elem_kind }
        {
            crate::c_abi::panic::panic_text("copy_from_slice: element shapes differ");
        }
        let (dst_base, src_base) = unsafe { ((*dst).ptr.as_ptr(), (*src).ptr.as_const_ptr()) };
        if stride == 0 || dst_base.is_null() || src_base.is_null() || dst_len == 0 {
            return;
        }
        unsafe { bump_vec_mutation_generation(&mut *dst) };
        if !vec_elems_copy_without_retain(unsafe { &*src }) {
            for idx in 0..src_len {
                if !unsafe { vec_retain_elem_at_for_copy(src, idx) } {
                    crate::c_abi::panic::panic_text(
                        "copy_from_slice: element type cannot be copied",
                    );
                }
            }
        }
        if !vec_elems_release_nothing(unsafe { &*dst }) {
            for idx in 0..dst_len {
                unsafe { vec_release_elem_at(dst, idx) };
            }
        }
        unsafe {
            std::ptr::copy_nonoverlapping(src_base, dst_base, (src_len as usize) * stride);
        }
    });
}

/// Decodes a `Vec<(String, String)>` argument: each slot is a 16-byte
/// tuple of `(key, value)` c-string pointers (key at +0, value at +8).
///
/// Lives here rather than beside the HTTP client because the shape is
/// the ABI's, not any one module's: header lists and a child process's
/// environment overrides are the same pairs.
pub(crate) fn decode_header_tuple_vec(headers: *const GosVec) -> Vec<(String, String)> {
    let mut header_pairs: Vec<(String, String)> = Vec::new();
    if headers.is_null() {
        return header_pairs;
    }
    let v = unsafe { &*headers };
    let elem_bytes = v.elem_bytes as usize;
    if elem_bytes == 0 || v.ptr.is_null() {
        return header_pairs;
    }
    // Each slot must hold two 8-byte pointers; a narrower element
    // stride means the vec is not tuple-shaped and reading +8 would
    // run past the slot.
    if elem_bytes < 16 {
        return header_pairs;
    }
    for i in 0..v.len {
        let slot = unsafe { v.ptr.add((i as usize) * elem_bytes) };
        // Slots hold cstring pointers exposed as integers by the
        // flat-slot ABI; recover provenance before reading the bytes.
        let key_ptr = unsafe { slot_read_word(slot) }
            .cast_const()
            .cast::<std::os::raw::c_char>();
        let val_ptr = unsafe { slot_read_word(slot.add(8)) }
            .cast_const()
            .cast::<std::os::raw::c_char>();
        let key = if key_ptr.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(key_ptr) }
        };
        let val = if val_ptr.is_null() {
            String::new()
        } else {
            unsafe { crate::c_abi::gos_str_arg_string(val_ptr) }
        };
        header_pairs.push((key, val));
    }
    header_pairs
}

#[cfg(test)]
mod repeat_primitive_tests {
    use super::*;

    #[test]
    fn primitive_repeat_constructs_final_length_and_values() {
        unsafe {
            let zeros = gos_rt_vec_repeat_primitive(8, 1024, 0);
            assert_eq!((*zeros).len, 1024);
            assert_eq!((*zeros).cap, 1024);
            assert_eq!(gos_rt_vec_get_i64(zeros, 0), 0);
            assert_eq!(gos_rt_vec_get_i64(zeros, 1023), 0);
            crate::c_abi::map::gos_rt_vec_free(zeros);

            let bytes = gos_rt_vec_repeat_primitive(1, 4, 0xab);
            assert_eq!((*bytes).len, 4);
            assert_eq!(
                std::slice::from_raw_parts((*bytes).ptr.as_ptr(), 4),
                &[0xab; 4]
            );
            crate::c_abi::map::gos_rt_vec_free(bytes);
        }
    }
}

#[cfg(test)]
mod unwrap_or_vec_tests {
    use super::*;

    /// A `Vec` with one reference, as the compiler's ownership lowering hands
    /// one to a call.
    fn one_share() -> *mut GosVec {
        unsafe { gos_rt_vec_new_typed(8, crate::c_abi::vec::vec_elem_kind::PRIMITIVE) }
    }

    fn shares(v: *mut GosVec) -> u16 {
        vec_rc_atomic(unsafe { &*v }).load(std::sync::atomic::Ordering::SeqCst)
    }

    #[test]
    fn a_returned_fallback_carries_a_share_for_every_release_the_caller_owes() {
        let fallback = one_share();
        let before = shares(fallback);
        // Err: the fallback becomes the answer, so the caller now holds it
        // twice - once as the fallback binding, once as the answer.
        let answered = gos_rt_result_unwrap_or_vec(
            crate::c_abi::vec::gos_rt_result_new(1, 0),
            fallback as i64,
        );
        assert_eq!(answered, fallback as i64);
        assert_eq!(shares(fallback), before + 1);
        unsafe { crate::c_abi::map::gos_rt_vec_free(fallback) };
        unsafe { crate::c_abi::map::gos_rt_vec_free(fallback) };
    }

    #[test]
    fn an_unused_fallback_keeps_the_one_share_its_caller_releases() {
        let fallback = one_share();
        let payload = one_share();
        let before = shares(fallback);
        // Ok: the payload is the answer and the fallback is untouched, so the
        // caller's single release of it stays correct.
        let answered = gos_rt_result_unwrap_or_vec(
            crate::c_abi::vec::gos_rt_result_new(0, payload as i64),
            fallback as i64,
        );
        assert_eq!(answered, payload as i64);
        assert_eq!(shares(fallback), before);
        unsafe { crate::c_abi::map::gos_rt_vec_free(fallback) };
        unsafe { crate::c_abi::map::gos_rt_vec_free(payload) };
    }
}

#[cfg(test)]
mod extend_str_bytes_tests {
    use super::*;

    /// A bulk append leaves the vector one push per byte would: the same
    /// bytes, length, capacity, and mutation count, across growth.
    #[test]
    fn a_bulk_append_matches_one_push_per_byte() {
        let text =
            crate::c_abi::string::alloc_cstring_from_slices(&["ab\u{e9}cdefghij".as_bytes()]);
        unsafe {
            let bulk = gos_rt_vec_new_typed(1, vec_elem_kind::PRIMITIVE);
            let pushed = gos_rt_vec_new_typed(1, vec_elem_kind::PRIMITIVE);
            for _ in 0..5 {
                gos_rt_vec_extend_str_bytes(bulk, text);
                for &b in crate::c_abi::string::gos_str_arg_bytes(text) {
                    gos_rt_vec_push(pushed, &raw const b);
                }
                assert_eq!((*bulk).len, (*pushed).len);
                assert_eq!((*bulk).cap, (*pushed).cap);
                assert_eq!((*bulk).mutation_generation, (*pushed).mutation_generation);
            }
            let len = (*bulk).len as usize;
            assert_eq!(
                std::slice::from_raw_parts((*bulk).ptr.as_ptr(), len),
                std::slice::from_raw_parts((*pushed).ptr.as_ptr(), len)
            );
            crate::c_abi::map::gos_rt_vec_free(bulk);
            crate::c_abi::map::gos_rt_vec_free(pushed);
            crate::c_abi::string::gos_rt_str_free(text);
        }
    }
}

#[cfg(test)]
mod swap_unchecked_tests {
    use super::*;

    /// The unchecked swap leaves the vector exactly as the checked one does
    /// for indices inside the length.
    #[test]
    fn an_unchecked_swap_matches_the_checked_one_in_range() {
        unsafe {
            let a = gos_rt_vec_new_typed(8, vec_elem_kind::PRIMITIVE);
            let b = gos_rt_vec_new_typed(8, vec_elem_kind::PRIMITIVE);
            for v in [a, b] {
                for n in 0..8i64 {
                    gos_rt_vec_push(v, std::ptr::addr_of!(n).cast());
                }
            }
            for (i, j) in [(0i64, 7i64), (1, 6), (3, 4), (2, 2)] {
                crate::c_abi::signal::gos_rt_vec_swap_safe(a, i, j);
                crate::c_abi::signal::gos_rt_vec_swap_unchecked(b, i, j);
            }
            let len = (*a).len as usize;
            assert_eq!(len, (*b).len as usize);
            assert_eq!(
                std::slice::from_raw_parts((*a).ptr.as_ptr(), len * 8),
                std::slice::from_raw_parts((*b).ptr.as_ptr(), len * 8)
            );
            crate::c_abi::map::gos_rt_vec_free(a);
            crate::c_abi::map::gos_rt_vec_free(b);
        }
    }
}

#[cfg(test)]
mod slot_word_tests {
    use super::{slot_read_word, slot_write_word};

    /// A slot round-trips a child word through the helper pair, and the write
    /// fills all eight bytes: a pointer-width write would leave the upper half
    /// holding whatever the slot held before, which a later read would answer.
    #[test]
    fn a_slot_write_fills_the_whole_word_and_reads_back() {
        let mut slot = [0xABu8; 8];
        let mut target = 0u64;
        let child: *mut u8 = std::ptr::addr_of_mut!(target).cast();
        unsafe {
            slot_write_word(slot.as_mut_ptr(), child);
            assert_eq!(slot_read_word(slot.as_ptr()), child);
        }
        let high = u64::from_le_bytes(slot) >> 32;
        assert_eq!(
            high,
            (child as usize as u64) >> 32,
            "the upper half is the word's own, not the slot's previous bytes"
        );

        let mut zero = [0x5Au8; 8];
        unsafe {
            slot_write_word(zero.as_mut_ptr(), std::ptr::null_mut());
            assert!(slot_read_word(zero.as_ptr()).is_null());
        }
        assert_eq!(u64::from_le_bytes(zero), 0, "a null child clears the slot");
    }
}
