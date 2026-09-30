//! Escaped value-aggregate copy blobs and their deterministic reclamation.

use super::*;

// ---------------------------------------------------------------
// Escaped value-aggregate copy-blobs (deterministic reclamation).
// ---------------------------------------------------------------
//
// When a multi-slot struct value flows into a `Some(..)`/`Ok(..)` payload
// on the LLVM tier, the backend makes a heap copy of its flat slots.
// Those copies are single-owner value snapshots; `gos_rt_rc_alloc_copy`
// puts them under reference counting with an `RC_KIND_STRUCT_GUARDED`
// meta so the MIR drop pass can release them deterministically when the
// owning aggregate slot dies.
//
// A guarded payload may also hold a non-copy pointer (map-get result,
// construction aggregate, or borrow), so a walk over an untyped slot has to
// tell a copy blob apart without trusting the bytes in front of an arbitrary
// pointer. Copy blobs are allocated in an exclusive mimalloc arena that holds
// nothing else, so membership is an address-range test and the block is just
// `[RcHeader | payload]`. A blob the arena cannot serve (the arena is full,
// or the build has no mimalloc) carries a tagged owner word in front of its
// header instead, which is what the walk reads outside the arena.

/// Virtual size reserved for the copy-blob arena: the largest power of two
/// mimalloc reserves as one arena (16 GiB is past its per-arena limit once
/// the arena's own bitmap is counted). Address space only; pages are
/// committed as blobs are allocated.
#[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
const COPY_BLOB_ARENA_BYTES: usize = 1 << 33;

/// Start of the copy-blob arena. Until the arena exists it names the top
/// `COPY_BLOB_ARENA_BYTES` of the address space, which is kernel half on every
/// supported 64-bit target, so the range test is false for any user pointer
/// before, during, and after a failed reservation.
#[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
static COPY_BLOB_ARENA_LO: AtomicUsize = AtomicUsize::new(COPY_BLOB_ARENA_BYTES.wrapping_neg());

/// The heap that allocates only in the copy-blob arena; null until reserved,
/// and for good when the reservation is unavailable.
#[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
static COPY_BLOB_HEAP: std::sync::atomic::AtomicPtr<libmimalloc_sys::mi_heap_t> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

#[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
unsafe extern "C" {
    // Exported by the mimalloc build `libmimalloc-sys` links; the binding
    // crate declares the reservation calls but not this accessor.
    fn mi_arena_area(arena_id: libmimalloc_sys::mi_arena_id_t, size: *mut usize) -> *mut u8;
}

/// Whether `p` lies in the copy-blob arena.
#[inline]
pub(super) fn in_copy_blob_arena(p: *const u8) -> bool {
    #[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
    {
        (p as usize).wrapping_sub(COPY_BLOB_ARENA_LO.load(Ordering::Relaxed))
            < COPY_BLOB_ARENA_BYTES
    }
    #[cfg(any(tsan, miri, fuzzing, target_arch = "wasm32"))]
    {
        let _ = p;
        false
    }
}

/// `total` bytes for a copy blob's header and payload from the arena heap, or
/// null when the arena cannot serve it.
#[inline]
fn copy_blob_arena_alloc(total: usize, zeroed: bool) -> *mut u8 {
    #[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
    {
        let mut heap = COPY_BLOB_HEAP.load(Ordering::Relaxed);
        if heap.is_null() {
            heap = copy_blob_heap_init();
            if heap.is_null() {
                return std::ptr::null_mut();
            }
        }
        // SAFETY: `heap` is the live arena heap; mimalloc heaps in v3 serve
        // allocations from any thread.
        let block: *mut u8 = unsafe {
            match (total <= MI_SMALL_SIZE_MAX, zeroed) {
                (true, false) => libmimalloc_sys::mi_heap_malloc_small(heap, total),
                (true, true) => libmimalloc_sys::mi_heap_zalloc_small(heap, total),
                (false, false) => libmimalloc_sys::mi_heap_malloc(heap, total),
                (false, true) => libmimalloc_sys::mi_heap_zalloc(heap, total),
            }
        }
        .cast();
        debug_assert!(block.is_null() || in_copy_blob_arena(block));
        block
    }
    #[cfg(any(tsan, miri, fuzzing, target_arch = "wasm32"))]
    {
        let _ = (total, zeroed);
        std::ptr::null_mut()
    }
}

/// Reserves the copy-blob arena once and answers its heap, or null when the
/// platform refuses the reservation.
#[cfg(not(any(tsan, miri, fuzzing, target_arch = "wasm32")))]
#[cold]
#[inline(never)]
fn copy_blob_heap_init() -> *mut libmimalloc_sys::mi_heap_t {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let mut id: libmimalloc_sys::mi_arena_id_t = std::ptr::null_mut();
        // SAFETY: reserving address space has no precondition; `id` is written
        // only on success.
        let reserved = unsafe {
            libmimalloc_sys::mi_reserve_os_memory_ex(
                COPY_BLOB_ARENA_BYTES,
                false,
                false,
                true,
                &raw mut id,
            )
        };
        if reserved != 0 {
            return;
        }
        let mut size = 0usize;
        // SAFETY: `id` names the arena just reserved.
        let start = unsafe { mi_arena_area(id, &raw mut size) };
        // The range test uses the constant span, so an arena of any other
        // size would misclassify addresses; such an arena is left unused.
        if start.is_null() || size != COPY_BLOB_ARENA_BYTES {
            return;
        }
        // SAFETY: `id` names a live exclusive arena.
        let heap = unsafe { libmimalloc_sys::mi_heap_new_in_arena(id) };
        if heap.is_null() {
            return;
        }
        // The range is published before the heap, so every block the heap
        // hands out is already recognised.
        COPY_BLOB_ARENA_LO.store(start as usize, Ordering::Release);
        COPY_BLOB_HEAP.store(heap, Ordering::Release);
    });
    COPY_BLOB_HEAP.load(Ordering::Acquire)
}

/// The carrier in front of a copy blob's ordinary RC header when the blob
/// lives outside the copy-blob arena: one word naming the ABI version, the
/// carrier kind, and the destructor the block answers to, under a magic that
/// no ordinary payload word carries.
///
/// Outside the arena this word is what says a pointer read out of an untyped
/// `Option` / `Result` slot is a copy blob rather than some other allocation,
/// so it is a whole word of tag rather than a flag. The child layout is not
/// repeated here: the RC header interns it as `meta_id`, and `meta_of` reads
/// the same blob back.
#[repr(C)]
pub(super) struct CopyBlobOwner {
    tag: u64,
}

const COPY_BLOB_OWNER_VERSION: u16 = 1;
const COPY_BLOB_OWNER_KIND: u16 = 3;
const COPY_BLOB_OWNER_DTOR: u32 = 1;
const COPY_BLOB_OWNER_MAGIC: u16 = 0xC0B5;
pub(super) const COPY_BLOB_DISC: u8 = 0xCB;
pub(super) const COPY_BLOB_OWNER_BYTES: usize = std::mem::size_of::<CopyBlobOwner>();

/// The one value a live carrier's word holds.
const COPY_BLOB_OWNER_TAG: u64 = ((COPY_BLOB_OWNER_MAGIC as u64) << 48)
    | ((COPY_BLOB_OWNER_VERSION as u64) << 40)
    | ((COPY_BLOB_OWNER_KIND as u64) << 32)
    | (COPY_BLOB_OWNER_DTOR as u64);

/// Whether `payload` could be a managed allocation this module may inspect.
///
/// An `Option` / `Result` payload word is untyped: it carries a pointer for a
/// managed value and a plain scalar for `Option<i64>` and friends. Reading a
/// header out of a scalar is a wild dereference, so a word that cannot be an
/// allocation address is rejected before any load. Managed allocations come
/// from the allocator with at least pointer alignment and sit well above the
/// first page, which no small integer or `-1` sentinel satisfies.
#[inline]
fn payload_may_be_managed(payload: *const u8) -> bool {
    let address = payload as usize;
    address >= MIN_MANAGED_ADDRESS
        && address.is_multiple_of(std::mem::align_of::<usize>())
        && address != usize::MAX
}

/// Lowest address a managed allocation may occupy. The first page is never
/// mapped, so any word below it is a scalar payload rather than a pointer.
const MIN_MANAGED_ADDRESS: usize = 0x1000;

#[inline]
pub(super) unsafe fn copy_blob_owner(payload: *mut u8) -> Option<&'static CopyBlobOwner> {
    if !payload_may_be_managed(payload) {
        return None;
    }
    // SAFETY: `payload` may be a managed node (checked above), so its header is readable heap
    // memory.
    let header = unsafe { header_ptr(payload) };
    // SAFETY: `header` is readable heap memory (checked above).
    if unsafe { (*header).disc } != COPY_BLOB_DISC {
        return None;
    }
    // SAFETY: a copy blob carries its owner in the bytes before the header.
    let owner = unsafe {
        &*((header as *mut u8)
            .sub(COPY_BLOB_OWNER_BYTES)
            .cast::<CopyBlobOwner>())
    };
    // SAFETY: `header` is a copy blob's header.
    (owner.tag == COPY_BLOB_OWNER_TAG && !unsafe { meta_of(header) }.is_null()).then_some(owner)
}

/// Whether `payload` is a live copy blob's payload. The arena holds only copy
/// blobs, each a header followed by its payload, so a word-aligned address
/// whose header lies in it needs only the header's own marks; anywhere else
/// the owner tag decides.
#[inline]
pub(super) unsafe fn is_copy_blob(payload: *mut u8) -> bool {
    let header = payload.wrapping_sub(RC_HEADER_SIZE).cast::<RcHeader>();
    if in_copy_blob_arena(header.cast()) {
        return (payload as usize).is_multiple_of(std::mem::align_of::<usize>())
            // SAFETY: `header` lies in the copy-blob arena (checked above).
            && unsafe { (*header).disc } == COPY_BLOB_DISC
            // SAFETY: `header` lies in the copy-blob arena (checked above).
            && !unsafe { meta_of(header) }.is_null();
    }
    // SAFETY: this `unsafe fn`'s caller passes `payload` a candidate the owner probe accepts.
    unsafe { copy_blob_owner(payload).is_some() }
}

/// `total` bytes for a copy blob that carries its owner word, answered as the
/// address of its RC header. Null when the allocator refuses.
#[inline]
pub(super) fn owner_blob_header(total: usize, zeroed: bool) -> *mut RcHeader {
    let total = total.saturating_add(COPY_BLOB_OWNER_BYTES);
    let base = if zeroed {
        rc_block_alloc_zeroed(total)
    } else {
        rc_block_alloc_unzeroed(total)
    };
    if base.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: `base` is a fresh block of at least the owner word plus a header.
    unsafe {
        base.cast::<CopyBlobOwner>().write(CopyBlobOwner {
            tag: COPY_BLOB_OWNER_TAG,
        });
        base.add(COPY_BLOB_OWNER_BYTES).cast::<RcHeader>()
    }
}

/// Walk the `(disc_word, payload_word)` pairs of an `RC_KIND_STRUCT_GUARDED`
/// meta over the aggregate slots at `base`, calling `f` for each child that
/// is live (negative disc word, or the disc word reads 0), non-null, and a
/// copy blob with a validated owner. `base` may be a heap payload or a stack slot - the
/// walk only reads the flat words the meta names.
#[allow(
    clippy::inline_always,
    reason = "the per-node child walk of every guarded teardown: left to the heuristic, LLVM keeps it out of line, which callgrind measured as a call per node on a tree workload"
)]
#[inline(always)]
/// Whether a structural `meta` names a `Map` child. A map is never allocated in
/// a region, so a blob whose words name one cannot take the region's no-owner
/// shortcut: its copy needs a table of its own and a release that frees it.
///
/// # Safety
/// `meta` must be null or a well-formed RC meta blob.
unsafe fn meta_names_map_child(meta: *const i64) -> bool {
    use gossamer_abi::rc::{RC_CHILD_KIND_SHIFT, RC_CHILD_MAP};
    if meta.is_null() {
        return false;
    }
    // SAFETY: `meta` is non-null (checked above) and, per this `unsafe fn`'s contract, a
    // well-formed meta table, which the variant and child counts it records keep this read
    // inside.
    if unsafe { *meta } == gossamer_abi::rc::RC_KIND_SLOT_CHILDREN {
        use crate::c_abi::vec::vec_elem_kind;
        // SAFETY: `meta` is non-null (checked above) and, per this `unsafe fn`'s contract, a
        // well-formed meta table, which the variant and child counts it records keep this read
        // inside.
        let count = usize::try_from(unsafe { *meta.add(1) }).unwrap_or(0);
        return (0..count).any(|i| {
            // SAFETY: `meta` is non-null (checked above) and, per this `unsafe fn`'s contract, a
            // well-formed meta table, which the variant and child counts it records keep this
            // read inside.
            let child = unsafe { *meta.add(2 + i * 4 + 3) };
            [
                vec_elem_kind::MAP,
                vec_elem_kind::SET,
                vec_elem_kind::DEQUE,
                vec_elem_kind::HEAP,
                vec_elem_kind::ITER,
                vec_elem_kind::ITER_PAIR,
            ]
            .iter()
            .any(|kind| child == i64::from(*kind))
        });
    }
    // SAFETY: `meta` is non-null (checked above) and, per this `unsafe fn`'s contract, a
    // well-formed meta table, which the variant and child counts it records keep this read
    // inside.
    if unsafe { *meta } != RC_KIND_STRUCT {
        return false;
    }
    // SAFETY: `meta` is non-null (checked above) and, per this `unsafe fn`'s contract, a
    // well-formed meta table, which the variant and child counts it records keep this read
    // inside.
    let variants = unsafe { *meta.add(1) };
    let mut idx: usize = 2;
    for _ in 0..variants.max(0) {
        // SAFETY: `meta` is non-null (checked above) and, per this `unsafe fn`'s contract, a
        // well-formed meta table, which the variant and child counts it records keep this read
        // inside.
        let count = usize::try_from(unsafe { *meta.add(idx + 1) }).unwrap_or(0);
        for j in 0..count {
            // SAFETY: `meta` is non-null (checked above) and, per this `unsafe fn`'s contract, a
            // well-formed meta table, which the variant and child counts it records keep this
            // read inside.
            let entry = unsafe { *meta.add(idx + 2 + j) };
            if matches!(
                entry >> RC_CHILD_KIND_SHIFT,
                RC_CHILD_MAP
                    | gossamer_abi::rc::RC_CHILD_SET
                    | gossamer_abi::rc::RC_CHILD_DEQUE
                    | gossamer_abi::rc::RC_CHILD_HEAP
                    | gossamer_abi::rc::RC_CHILD_ITER
                    | gossamer_abi::rc::RC_CHILD_ITER_PAIR
            ) {
                return true;
            }
        }
        idx += 2 + count;
    }
    false
}

pub(super) unsafe fn visit_guarded_children(
    base: *mut u8,
    meta: *const i64,
    mut f: impl FnMut(*mut u8),
) {
    // SAFETY: this `unsafe fn`'s caller passes `meta` a guarded-children table, whose second word
    // is its entry count.
    let entry_count = unsafe { *meta.add(1) };
    for i in 0..entry_count.max(0) {
        // SAFETY: `i` is below the entry count, inside the table of three-word entries.
        let gate = unsafe { *meta.add(2 + (i as usize) * 3) };
        // SAFETY: `i` is below the entry count, inside the table of three-word entries.
        let disc_word = unsafe { *meta.add(3 + (i as usize) * 3) };
        // SAFETY: `i` is below the entry count, inside the table of three-word entries.
        let payload_word = unsafe { *meta.add(4 + (i as usize) * 3) };
        // `gate` is the discriminant value under which the payload word
        // holds a copy-blob pointer (0 = Ok/Some side, 1 = Err side);
        // negative means unconditional (both sides are blobs).
        if gate >= 0 {
            // SAFETY: `disc_word` names a word inside `base`.
            let disc = unsafe { *(base.add(disc_word as usize * 8) as *const i64) };
            if disc != gate {
                continue;
            }
        }
        // SAFETY: `payload_word` names a word inside `base`.
        let slot = unsafe { base.add(payload_word as usize * 8) };
        // SAFETY: `slot` is that word.
        let child = unsafe { crate::c_abi::vec::slot_read_word(slot) };
        // SAFETY: `child` is non-null, a candidate the blob probe accepts.
        if !child.is_null() && unsafe { is_copy_blob(child) } {
            f(child);
        }
    }
}

/// Allocate an RC copy-blob, memcpy `size` bytes from `src`, retain the
/// guarded children the copy now shares with its source, and attach an
/// explicit owner carrier. Inside a `region` block the bytes are
/// bump-allocated and freed wholesale at pop, so it has no individual owner
/// and its children are not retained (region objects never run the
/// per-node teardown walk).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_rc_alloc_copy(
    size: u64,
    meta: *const i64,
    src: *const u8,
) -> *mut u8 {
    // SAFETY: `meta` and `src` are this shim's arguments, each null or live, with `src` holding
    // `size` bytes (C-ABI contract).
    unsafe { rc_alloc_from(size, meta, src, true) }
}

/// Allocates the same blob as [`gos_rt_rc_alloc_copy`] and takes the source's
/// share of the children rather than minting one.
///
/// The caller is giving up the words it copied here - its own walk over them
/// is what the compiler removed alongside this call - so the children keep the
/// count they already had and the blob is the one holding it.
///
/// # Safety
/// `src` must name `size` readable bytes laid out for `meta`, and the caller
/// must not release the children of those words afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_rc_alloc_move(
    size: u64,
    meta: *const i64,
    src: *const u8,
) -> *mut u8 {
    // SAFETY: `meta` and `src` are this shim's arguments, each null or live, with `src` holding
    // `size` bytes (C-ABI contract).
    unsafe { rc_alloc_from(size, meta, src, false) }
}

/// Copies `size` payload bytes. Escaped aggregates are a few words wide, and a
/// fixed-width move of those is a handful of instructions where the general
/// copy is a library call.
#[inline]
unsafe fn copy_payload(src: *const u8, dst: *mut u8, size: usize) {
    #[inline]
    unsafe fn words<const N: usize>(src: *const u8, dst: *mut u8) {
        // SAFETY: this `unsafe fn`'s caller passes `src` and `dst` each addressing `N` words.
        unsafe {
            dst.cast::<[u64; N]>()
                .write_unaligned(src.cast::<[u64; N]>().read_unaligned());
        }
    }
    // SAFETY: `src` and `dst` each address `size` bytes, which the arms copy in whole words.
    unsafe {
        match size {
            8 => words::<1>(src, dst),
            16 => words::<2>(src, dst),
            24 => words::<3>(src, dst),
            32 => words::<4>(src, dst),
            40 => words::<5>(src, dst),
            48 => words::<6>(src, dst),
            56 => words::<7>(src, dst),
            64 => words::<8>(src, dst),
            _ => std::ptr::copy_nonoverlapping(src, dst, size),
        }
    }
}

#[allow(
    clippy::inline_always,
    reason = "the allocation path of every escaped aggregate: left to the heuristic, LLVM keeps it out of line in both entry points, which callgrind measured at 15% more instructions on a tree workload"
)]
#[inline(always)]
unsafe fn rc_alloc_from(
    size: u64,
    meta: *const i64,
    src: *const u8,
    retain_children: bool,
) -> *mut u8 {
    let in_region = region_active();
    // SAFETY: this `unsafe fn`'s caller passes `meta` live or null, which `meta_names_map_child`
    // accepts.
    if in_region && !unsafe { meta_names_map_child(meta) } {
        // SAFETY: `meta` is not passed, so the allocation carries no children.
        let payload = unsafe { gos_rt_rc_alloc(size, std::ptr::null()) };
        if !payload.is_null() && !src.is_null() {
            // SAFETY: `payload` is a fresh block of `size` bytes, and `src` holds `size` bytes
            // (this `unsafe fn`'s caller).
            unsafe { std::ptr::copy_nonoverlapping(src, payload, size as usize) };
        }
        return payload;
    }
    // SAFETY: this `unsafe fn`'s caller passes `meta` and `src` null or live, `src` holding
    // `size` bytes.
    unsafe { heap_blob_from(size, meta, src, retain_children) }
}

/// Layout of a runtime-built blob whose words own no heap child.
pub(crate) static LEAF_BLOB_META: [i64; 2] = [RC_KIND_STRUCT_GUARDED, 0];

/// Copies a container element's `size` bytes at `src` into a counted blob laid
/// out by `meta`, which takes over the shares those words carry, and answers
/// whether it did. Inside a region the copy is the region's and is reclaimed
/// with it: a region local is a view, so no share is minted for it.
///
/// # Safety
/// `src` must name `size` readable bytes laid out for `meta`, a static layout.
pub(crate) unsafe fn counted_element_copy(
    size: u64,
    meta: *const i64,
    src: *const u8,
) -> (*mut u8, bool) {
    // SAFETY: this `unsafe fn`'s caller passes `meta` live or null, which `meta_names_map_child`
    // accepts.
    if region_active() && !unsafe { meta_names_map_child(meta) } {
        // SAFETY: this `unsafe fn`'s caller passes `meta` and `src` null or live, `src` holding
        // `size` bytes.
        return (unsafe { rc_alloc_from(size, meta, src, false) }, false);
    }
    // SAFETY: this `unsafe fn`'s caller passes `meta` and `src` null or live, `src` holding
    // `size` bytes.
    (unsafe { heap_blob_from(size, meta, src, false) }, true)
}

/// Moves `words` into a counted blob laid out by `meta`, which takes over the
/// shares those words carry.
///
/// A runtime call answering an aggregate payload hands this blob to the frame,
/// whose release of the carrier gives the children back. It never takes a
/// region's no-owner shortcut: the children a runtime call builds are not
/// region allocations the arena reclaims.
pub(crate) fn counted_words(words: &[i64], meta: &'static [i64]) -> *mut u8 {
    let size = u64::try_from(words.len().saturating_mul(8)).unwrap_or(u64::MAX);
    // SAFETY: `words` is `size` readable bytes and `meta` is a static, well-formed layout
    // naming only words inside them.
    unsafe { heap_blob_from(size, meta.as_ptr(), words.as_ptr().cast::<u8>(), false) }
}

/// Allocates a counted copy blob on the heap, outside any region.
///
/// # Safety
/// As [`gos_rt_rc_alloc_copy`].
unsafe fn heap_blob_from(
    size: u64,
    meta: *const i64,
    src: *const u8,
    retain_children: bool,
) -> *mut u8 {
    let Some(meta_id) = meta_intern(meta) else {
        return std::ptr::null_mut();
    };
    let total = RC_HEADER_SIZE.saturating_add(size as usize);
    // Every byte of the block is written below - the header and the payload
    // the copy fills - so the zero fill would be overwritten wholesale. A
    // source that covers the payload is the only shape that holds; anything
    // else keeps the zeroed block.
    let zeroed = src.is_null();
    let arena_block = copy_blob_arena_alloc(total, zeroed);
    let header = if arena_block.is_null() {
        owner_blob_header(total, zeroed)
    } else {
        arena_block.cast::<RcHeader>()
    };
    if header.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: `header` is non-null (checked above), a fresh block of a header plus `size` bytes.
    unsafe {
        (*header).strong = 1;
        (*header).weak = AtomicU8::new(0);
        (*header).disc = COPY_BLOB_DISC;
        (*header).meta_id = meta_id;
    }
    rc_live_inc();
    // SAFETY: the block holds the header followed by the payload.
    let payload = unsafe { (header as *mut u8).add(RC_HEADER_SIZE) };
    if payload.is_null() || src.is_null() {
        return payload;
    }
    // SAFETY: `payload` holds `size` bytes and `src` holds `size` bytes (this `unsafe fn`'s
    // caller).
    unsafe { copy_payload(src, payload, size as usize) };
    if meta.is_null() {
        // A leaf blob: its words are scalars, so the copy shares no RC
        // child with the source and there is nothing to retain. The
        // interning above already accepts null as "no child layout".
        return payload;
    }
    if !retain_children {
        // The source gave up its share of these words, so the blob holds the
        // count they already carried.
        return payload;
    }
    // SAFETY: `payload` is laid out as `meta` describes.
    unsafe { retain_blob_children(payload, meta) };
    payload
}

/// Takes a share of every child a copy blob's `meta` names, for words that
/// were copied from storage keeping its own.
unsafe fn retain_blob_children(payload: *mut u8, meta: *const i64) {
    // SAFETY: this `unsafe fn`'s caller passes `payload` laid out as `meta` describes.
    unsafe {
        if *meta == gossamer_abi::rc::RC_KIND_STRUCT_GUARDED {
            visit_guarded_children(payload, meta, |child| {
                gos_rt_rc_retain(child);
            });
        } else {
            // Map-owned aggregate copies use ordinary structural metadata so
            // their direct String / RC and Vec fields are retained along with
            // the copied words. `visit_entries` reads this blob from the
            // freshly initialised header and dispatches each child kind.
            visit_entries(payload, |kind, child| {
                if kind == gossamer_abi::rc::RC_CHILD_VEC {
                    crate::c_abi::gos_rt_vec_retain(child.cast());
                } else if kind == gossamer_abi::rc::RC_CHILD_RC {
                    gos_rt_rc_retain(child);
                }
            });
            clone_map_children(payload);
        }
    }
}

/// The structural child kind a map-boxed carrier's meta names for its payload
/// word - `RC_CHILD_RC` for a `String`, `RC_CHILD_VEC` for a `Vec` - or `None`
/// for a block whose meta names no single child.
pub(crate) unsafe fn boxed_carrier_child_kind(payload: *mut u8) -> Option<i64> {
    if payload.is_null() || in_region_arena(payload) {
        return None;
    }
    // SAFETY: `payload` is non-null and outside the region arena (checked above), a live node
    // (this `unsafe fn`'s caller).
    let meta = unsafe { meta_of(header_ptr(payload)) };
    // `[RC_KIND_STRUCT, variants, disc, child_count, entry..]`.
    // SAFETY: `meta` is non-null (checked first), a meta table of at least a variant header.
    if meta.is_null() || unsafe { *meta } != RC_KIND_STRUCT || unsafe { *meta.add(3) } != 1 {
        return None;
    }
    // SAFETY: a one-child struct meta holds its entry at word 4.
    Some(unsafe { *meta.add(4) } >> gossamer_abi::rc::RC_CHILD_KIND_SHIFT)
}

/// Gives back a copy blob whose words were just copied into another owner,
/// which takes the blob's child shares with them. A blob no one else holds is
/// freed without walking its children; one that is still held keeps them, so
/// the new owner takes shares of its own first.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) unsafe fn release_blob_moved(payload: *mut u8) {
    if payload.is_null() || in_region_arena(payload) {
        return;
    }
    // SAFETY: this `unsafe fn`'s caller passes `payload` live; non-null, checked above.
    let h = unsafe { header_ptr(payload) };
    // SAFETY: `h` is the header of the live node `payload`.
    let strong = unsafe { load_strong(h) };
    let exclusive = strong & (SHARED_BIT | BUFFERED_BIT | REGION_BIT) == 0
        && strong & STRONG_COUNT_MASK == 1
        // SAFETY: `h` is the header of the live node `payload`.
        && unsafe { (*h).weak.load(Ordering::Relaxed) } == 0;
    if exclusive {
        rc_live_dec();
        // SAFETY: the count is one and thread-local, and no weak reference or
        // collector buffer pins the block, so this is its last reference.
        unsafe { rc_block_free(block_base(h)) };
        return;
    }
    // SAFETY: `h` is the header of the live node `payload`.
    let meta = unsafe { meta_of(h) };
    if !meta.is_null() {
        // SAFETY: `payload` is laid out as `meta` describes.
        unsafe { retain_blob_children(payload, meta) };
    }
    // SAFETY: `payload` is a live node whose moved share this gives back.
    unsafe { gos_rt_rc_release(payload) };
}

/// Release the guarded children held in the aggregate slots at `base`
/// (a stack aggregate dying or being overwritten). Null-safe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_aggr_release_children(base: *mut u8, meta: *const i64) {
    if base.is_null() || meta.is_null() {
        return;
    }
    // SAFETY: `base` and `meta` are this shim's arguments, non-null (checked above), with `base`
    // laid out as `meta` describes (C-ABI contract).
    unsafe {
        visit_guarded_children(base, meta, |child| {
            gos_rt_rc_release(child);
        });
    }
}

/// Retain the guarded children held in the aggregate slots at `base`
/// (a stack aggregate that was just whole-copied). Null-safe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_aggr_retain_children(base: *mut u8, meta: *const i64) {
    if base.is_null() || meta.is_null() {
        return;
    }
    // SAFETY: `base` and `meta` are this shim's arguments, non-null (checked above), with `base`
    // laid out as `meta` describes (C-ABI contract).
    unsafe {
        visit_guarded_children(base, meta, |child| {
            gos_rt_rc_retain(child);
        });
    }
}

/// Heap-box a multi-slot aggregate that is a user-enum variant payload:
/// allocate an RC cell carrying `meta` (an `RC_KIND_STRUCT` child-word list),
/// copy `size` bytes from `src`, and retain every RC child the box now
/// co-owns. The enum's variant meta lists this box's slot as a child, so the
/// enum's release frees the box; the box's own release walk then reclaims its
/// `String` / RC-node children exactly once. The retain balances the source
/// aggregate's scope-end teardown release, so the box keeps a live reference
/// even after the constructing frame returns. Inside a `region { … }` the
/// bytes are bump-allocated and freed wholesale at pop, so the box is
/// meta-less and its children are not retained (region objects never run the
/// per-node teardown walk).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_enum_box_aggr(
    size: u64,
    meta: *const i64,
    src: *const u8,
) -> *mut u8 {
    let in_region = region_active();
    // SAFETY: `meta` is this shim's argument, null or a live meta table (C-ABI contract).
    let payload = unsafe { gos_rt_rc_alloc(size, if in_region { std::ptr::null() } else { meta }) };
    if payload.is_null() || src.is_null() {
        return payload;
    }
    // SAFETY: `payload` is a fresh block of `size` bytes, and `src` is this shim's `size`-byte
    // argument (C-ABI contract).
    unsafe { std::ptr::copy_nonoverlapping(src, payload, size as usize) };
    if in_region {
        return payload;
    }
    // SAFETY: `payload` is laid out as `meta` describes.
    unsafe {
        visit_children_raw(payload, |c| {
            gos_rt_rc_retain(c);
        });
        // The copy co-owns any Vec child alongside its source.
        visit_vec_children(payload, |v| {
            crate::c_abi::vec::vec_retain_header(v.cast());
        });
        clone_map_children(payload);
    }
    payload
}

/// Copy a by-value aggregate's slot bytes into the RC cell a `Weak` observes:
/// allocate a headered cell carrying `meta` (an `RC_KIND_STRUCT` child-word
/// list), copy `size` bytes from `src`, and retain every RC child the cell now
/// co-owns. The caller's frame owns the returned strong reference and releases
/// it at scope end, at which point the outstanding weak count keeps the cell
/// allocated until the last `Weak` is released.
///
/// The cell is always allocated globally, never bump-allocated in an enclosing
/// `arena { … }`: weak liveness is per-object, and a region block reclaims its
/// slab wholesale without individual accounting.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_rc_weak_cell(
    size: u64,
    meta: *const i64,
    src: *const u8,
) -> *mut u8 {
    let total = (size as usize).saturating_add(RC_HEADER_SIZE);
    let Some(meta_id) = meta_intern(meta) else {
        return std::ptr::null_mut();
    };
    let base = rc_block_alloc_zeroed(total);
    if base.is_null() {
        return std::ptr::null_mut();
    }
    let h = base as *mut RcHeader;
    // SAFETY: `base` is a fresh zeroed block of at least `RC_HEADER_SIZE`.
    unsafe {
        (*h).strong = 1;
        (*h).weak = AtomicU8::new(0);
        (*h).disc = 0;
        (*h).meta_id = meta_id;
    }
    rc_live_inc();
    crate::c_abi::ledger::rc_alloc(size as usize, 0, false, false);
    // SAFETY: the payload begins one header past the block base.
    let payload = unsafe { base.add(RC_HEADER_SIZE) };
    if src.is_null() {
        return payload;
    }
    // SAFETY: `src` addresses `size` bytes of the source aggregate's slots and
    // the fresh cell cannot overlap it.
    unsafe { std::ptr::copy_nonoverlapping(src, payload, size as usize) };
    // SAFETY: the header's meta describes the payload words just written.
    unsafe {
        visit_children_raw(payload, |c| {
            gos_rt_rc_retain(c);
        });
        visit_vec_children(payload, |v| {
            crate::c_abi::vec::vec_retain_header(v.cast());
        });
        clone_map_children(payload);
    }
    payload
}

/// Retain every RC child `payload` names through its header meta - a `String`
/// (`gos_rt_str_retain`) or RC-node (`gos_rt_rc_retain`) child. Used after a
/// multi-slot aggregate enum payload is materialised by value into a match
/// binding: the binding co-owns the box's children, so its scope-end teardown
/// release is balanced by this retain. `payload` is the box pointer; its
/// children are the same pointers the binding now aliases. Null-safe; a
/// region-arena box is left untouched.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_rc_retain_children(payload: *mut u8) {
    let base = untag_rc(payload);
    if base.is_null() || in_region_arena(base) {
        return;
    }
    // SAFETY: `base` is non-null and outside the region arena (checked above), this shim's live
    // counted argument (C-ABI contract).
    unsafe {
        visit_children_raw(base, |c| {
            gos_rt_rc_retain(c);
        });
        // The binding co-owns any Vec child alongside the box.
        visit_vec_children(base, |v| {
            crate::c_abi::vec::vec_retain_header(v.cast());
        });
    }
}

/// Zero the `(disc, payload)` word pairs a guarded meta names within the
/// aggregate slots at `base`. Entry-block initialisation: without it the
/// first release-before-reassignment walk would read stack garbage, and a
/// garbage word that happens to equal a live copy-blob address would be
/// spuriously released. Null-safe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_aggr_zero_guarded(base: *mut u8, meta: *const i64) {
    if base.is_null() || meta.is_null() {
        return;
    }
    // SAFETY: `meta` is this shim's `i64` argument, non-null (checked above), live for the call
    // (C-ABI contract).
    let entry_count = unsafe { *meta.add(1) };
    for i in 0..entry_count.max(0) {
        // SAFETY: `meta` is this shim's `i64` argument, non-null (checked above), live for the
        // call (C-ABI contract).
        let gate = unsafe { *meta.add(2 + (i as usize) * 3) };
        // SAFETY: `meta` is this shim's `i64` argument, non-null (checked above), live for the
        // call (C-ABI contract).
        let disc_word = unsafe { *meta.add(3 + (i as usize) * 3) };
        // SAFETY: `meta` is this shim's `i64` argument, non-null (checked above), live for the
        // call (C-ABI contract).
        let payload_word = unsafe { *meta.add(4 + (i as usize) * 3) };
        if gate >= 0 && disc_word >= 0 {
            // Write a discriminant that fails every gate (no entry gates
            // on a negative disc), so an accidental read of the
            // not-yet-assigned field never sees a live payload. For the
            // Option/Result encoding -1 is no valid variant; the real
            // first assignment overwrites it.
            // SAFETY: `disc_word` names a word inside `base`, laid out as `meta` describes (C-ABI
            // contract).
            unsafe { *(base.add(disc_word as usize * 8) as *mut i64) = -1 };
        }
        // SAFETY: `payload_word` names a word inside `base`, laid out as `meta` describes (C-ABI
        // contract).
        unsafe { *(base.add(payload_word as usize * 8) as *mut i64) = 0 };
    }
}

/// Release the payload of the by-value `{disc, payload}` Option/Result at
/// `slot` when it carries a validated copy-blob owner. Companion to
/// [`gos_rt_option_slot_retain`]; used when an option holder dies, is
/// overwritten, or an owning field slot is replaced. Null-safe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_option_slot_release(slot: *const i64) {
    if slot.is_null() {
        return;
    }
    // SAFETY: `slot` is this shim's two-word carrier argument, non-null (checked above; C-ABI
    // contract).
    let payload = unsafe { *(slot.add(1) as *const *mut u8) };
    // SAFETY: `payload` is non-null, a candidate the blob probe accepts.
    if !payload.is_null() && unsafe { is_copy_blob(payload) } {
        // SAFETY: `payload` is a copy blob whose share the slot held.
        unsafe { gos_rt_rc_release(payload) };
        // Null the payload word so a second release of the same slot
        // (consumption-site release + the unconditional return-sweep)
        // is a no-op instead of a double-free - the same null-out
        // discipline the local-release pass uses. A later allocation can
        // reuse this address, so the explicit null-out
        // remains the second-release guard for this slot.
        // SAFETY: `slot` is this shim's `i64` argument, non-null (checked above), live for the
        // call (C-ABI contract).
        unsafe { *slot.add(1).cast_mut() = 0 };
    }
}

/// Retain the payload of the by-value `{disc, payload}` Option/Result at
/// `slot` when the discriminant reads 0 (`Some`/`Ok`) and the payload is a
/// copy-blob owner. Used when an aliased option value is stored into
/// an owning aggregate slot. Null-safe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_option_slot_retain(slot: *const i64) {
    if slot.is_null() {
        return;
    }
    // SAFETY: `slot` is this shim's two-word carrier argument, non-null (checked above; C-ABI
    // contract).
    let payload = unsafe { *(slot.add(1) as *const *mut u8) };
    // SAFETY: `payload` is non-null, a candidate the blob probe accepts.
    if !payload.is_null() && unsafe { is_copy_blob(payload) } {
        // SAFETY: `payload` is a live copy blob.
        unsafe { gos_rt_rc_retain(payload) };
    }
}

/// Retain the payload of the by-value `{disc, payload}` Option/Result at
/// `slot` only when it is `Some` / `Ok` and the payload is a copy-blob owner.
/// Used when a combinator answers its receiver's value unchanged, so the
/// answer holds the same blob: an `Err` arm there is the combinator's own
/// fresh error, which the answer already owns. Null-safe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_option_slot_retain_ok(slot: *const i64) {
    // SAFETY: `slot` is non-null (checked first) and this shim's two-word carrier argument (C-ABI
    // contract).
    if slot.is_null() || unsafe { *slot } != 0 {
        return;
    }
    // SAFETY: `slot` is this shim's argument, live for the call (C-ABI contract); non-null,
    // checked above.
    unsafe { gos_rt_option_slot_retain(slot) };
}
