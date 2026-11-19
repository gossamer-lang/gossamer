//! Reference-counting type-meta ABI shared by the MIR lowerer (which
//! emits the descriptor blobs) and the runtime (which parses them in
//! `gos_rt_rc_release`). Single source of truth for the blob's kind
//! tags and layout so the two sides cannot drift.
//!
//! # Type-meta blob format (a flat, self-describing `[i64]`)
//!
//! Codegen emits one blob per RC-managed allocation shape as a single
//! contiguous module constant. The object header's `meta` field points
//! at word 0.
//!
//! ```text
//! [0] kind            - RC_KIND_*
//! [1] variant_count V
//! then V variant records, each variable-length:
//!     disc            - discriminant this record describes
//!     child_count C   - number of RC-pointer child words
//!     off_0 .. off_C  - payload WORD indices (byte offset / 8) holding
//!                       RC-managed child pointers to release
//! ```
//!
//! For an enum, release reads the live discriminant from payload word 0
//! and releases the matching record's children. For a struct/tuple
//! there is a single record and the discriminant is ignored. The MIR
//! lowerer emits one single-record `RC_KIND_STRUCT` blob per enum
//! variant (each allocation carries its own descriptor), so the enum
//! disc-search path is reserved for future shared descriptors.

/// `meta[0]` kind: enum (release reads the live disc from payload word 0
/// and matches a record).
pub const RC_KIND_ENUM: i64 = 0;
/// `meta[0]` kind: struct/tuple/single-variant (one record, disc ignored).
pub const RC_KIND_STRUCT: i64 = 1;
/// `meta[0]` kind: string-like heap object (wired in a later phase).
pub const RC_KIND_STRING: i64 = 2;
/// `meta[0]` kind: vec-like heap object (wired in a later phase).
pub const RC_KIND_VEC: i64 = 3;
/// `meta[0]` kind: map-like heap object (wired in a later phase).
pub const RC_KIND_MAP: i64 = 4;
/// `meta[0]` kind: closure environment (wired in a later phase).
pub const RC_KIND_CLOSURE: i64 = 5;

/// Guarded struct layout for escaped value-aggregate heap copies. Entries
/// are `(disc_word, payload_word)` pairs instead of bare offsets: the
/// child at `payload_word` is live only when the word at `disc_word`
/// reads 0 (the `Some`/`Ok` discriminant) - or unconditionally when
/// `disc_word` is negative. Every child pointer is additionally checked
/// for copy-blob provenance before retain/release (the copy-blob arena's
/// address range, or the owner word a blob outside it carries), so a
/// payload that came from a non-RC producer (map get, borrow, the
/// Cranelift tier's construction-allocated aggregates) is left alone.
pub const RC_KIND_STRUCT_GUARDED: i64 = 6;

/// Layout of an element copied out of a container that owns its elements'
/// heap children: `[kind, count, (gate, disc_word, word, child)*]`, the
/// per-slot child list the container itself walks, so the copy gives back
/// exactly what the element's words own. `child` is a container slot-child
/// kind: a string, vec, map, set, or reference-counted node.
pub const RC_KIND_SLOT_CHILDREN: i64 = 7;

/// Layout of a runtime object the runtime itself allocated, such as a lock:
/// `[kind, finalizer]`, where `finalizer` is the address of an
/// `unsafe extern "C" fn(*mut u8)` that drops the payload in place. The node
/// has no counted children; its last release runs the finalizer before the
/// block is freed.
pub const RC_KIND_FINALIZED: i64 = 8;

/// `meta[0]` flag above the kind bits: no object laid out by this meta can
/// be part of a reference cycle, so the cycle collector never buffers one as
/// a candidate. Read the kind itself through [`rc_kind`].
pub const RC_KIND_ACYCLIC: i64 = 1 << 32;

/// The kind a `meta[0]` word names, without the flags above it.
#[must_use]
pub const fn rc_kind(word: i64) -> i64 {
    word & 0xffff_ffff
}

/// Slot-child gate naming a child that is the whole element: a `Set` or deque
/// stored in a container occupies its one slot as the table handle itself.
/// Like any negative gate it is unconditional; it also tells an element read
/// to answer the handle word rather than a copy of the slot block.
pub const SLOT_GATE_WHOLE_ELEMENT: i64 = -2;

// ---------------------------------------------------------------------
// Child-entry encoding (RC_KIND_ENUM / RC_KIND_STRUCT records).
//
// Each child entry packs the payload WORD index in its low 32 bits and a
// child kind in the bits above. Kind 0 is a plain RC-node or String
// pointer (dispatched by header sniff at release), so every pre-existing
// blob - whose entries are bare word indices - decodes unchanged.
// ---------------------------------------------------------------------

/// Low 32 bits of a child entry: the payload word index.
pub const RC_CHILD_WORD_MASK: i64 = 0xffff_ffff;
/// Bit position of the child kind within a child entry.
pub const RC_CHILD_KIND_SHIFT: u32 = 32;
/// Child kind: RC-node or String pointer (header-sniffed at release).
pub const RC_CHILD_RC: i64 = 0;
/// Child kind: `*mut GosVec` owned by the node - the constructor retains
/// the vec's strong count for the node's share and release walks it
/// through `gos_rt_vec_free`, so a Vec payload survives its constructing
/// frame however the node escapes (return, call argument, container).
pub const RC_CHILD_VEC: i64 = 1;
/// Child kind: `*mut GosMap` the node owns outright. A `GosMap` carries no
/// reference count, so a copy takes a table of its own (`gos_rt_map_clone`,
/// written back into the copy's word) and the node's release frees it through
/// `gos_rt_map_free`.
pub const RC_CHILD_MAP: i64 = 2;
/// Child kind: a copy blob in a carrier field's payload word, live when the
/// carrier's discriminant (the word before) is 0, the `Ok` / `Some` arm.
/// Walked as an RC child.
pub const RC_CHILD_BLOB_OK: i64 = 3;
/// Child kind: a copy blob in a carrier field's payload word, live when the
/// carrier's discriminant is 1, the `Err` arm.
pub const RC_CHILD_BLOB_ERR: i64 = 4;
/// Child kind: a copy blob in a carrier field's payload word on both arms.
pub const RC_CHILD_BLOB_ANY: i64 = 5;
/// Child kind: a boxed list of an error's structured fields, which the node
/// owns alone and drops when it is freed.
pub const RC_CHILD_ERROR_FIELDS: i64 = 6;
/// Child entry naming a `Set` the blob owns outright: a set carries no
/// reference count, so a copy takes a table of its own.
pub const RC_CHILD_SET: i64 = 7;
/// Child entry naming a `Deque` / `Queue` / `Stack` the blob owns outright:
/// the header carries no reference count, so a copy takes a store of its own.
pub const RC_CHILD_DEQUE: i64 = 8;
/// Child entry naming a `MinHeap` / `MaxHeap` the blob owns outright. A heap
/// is a counted vector, but a heap write reaches its store in place, so a copy
/// takes a heap of its own rather than a share.
pub const RC_CHILD_HEAP: i64 = 9;
/// Child entry naming a lazy iterator handle the blob holds a share of. A copy
/// of the blob takes a share, and every holder advances one cursor.
pub const RC_CHILD_ITER: i64 = 10;
/// Child entry naming a lazy pair iterator handle (the state `zip` and
/// `enumerate` build), held on the same terms as [`RC_CHILD_ITER`].
pub const RC_CHILD_ITER_PAIR: i64 = 11;
/// Child entry naming a `Weak` reference the blob holds one weak share of. A
/// copy takes a weak share, the blob's death gives it back, and no walk that
/// follows strong edges - the cycle collector's included - crosses it.
pub const RC_CHILD_WEAK: i64 = 12;

/// Child kind: a counted node or `String` in a carrier field's payload word,
/// live when the carrier's discriminant is 0, the `Ok` / `Some` arm.
pub const RC_CHILD_RC_OK: i64 = 13;
/// Child kind: a counted node or `String` in a carrier field's payload word,
/// live when the carrier's discriminant is 1, the `Err` arm.
pub const RC_CHILD_RC_ERR: i64 = 14;
/// Child kind: a `Vec` in a carrier field's payload word on the `Ok` arm.
pub const RC_CHILD_VEC_OK: i64 = 15;
/// Child kind: a `Vec` in a carrier field's payload word on the `Err` arm.
pub const RC_CHILD_VEC_ERR: i64 = 16;
/// Child kind: a `Weak` in a carrier field's payload word on the `Ok` arm.
pub const RC_CHILD_WEAK_OK: i64 = 17;
/// Child kind: a `Weak` in a carrier field's payload word on the `Err` arm.
pub const RC_CHILD_WEAK_ERR: i64 = 18;
/// Child entry naming an owned JSON handle. A copied field clones the handle
/// onto the same document, and release drops that independent handle.
pub const RC_CHILD_JSON: i64 = 19;

/// The child kind for a carrier field whose payload is a heap value of child
/// kind `base` (`RC_CHILD_RC`, `RC_CHILD_VEC`, or `RC_CHILD_WEAK`) on the arm
/// `gate` names (0 or 1), or `None` for another base kind.
#[must_use]
pub const fn rc_child_gated_heap_kind(gate: i64, base: i64) -> Option<i64> {
    match (base, gate) {
        (RC_CHILD_RC, 0) => Some(RC_CHILD_RC_OK),
        (RC_CHILD_RC, 1) => Some(RC_CHILD_RC_ERR),
        (RC_CHILD_VEC, 0) => Some(RC_CHILD_VEC_OK),
        (RC_CHILD_VEC, 1) => Some(RC_CHILD_VEC_ERR),
        (RC_CHILD_WEAK, 0) => Some(RC_CHILD_WEAK_OK),
        (RC_CHILD_WEAK, 1) => Some(RC_CHILD_WEAK_ERR),
        _ => None,
    }
}

/// The arm and base kind of a gated heap child kind, or `None` for any other
/// kind: the payload word holds a value of the base kind only while the
/// carrier's discriminant equals the arm.
#[must_use]
pub const fn rc_child_gated_heap(kind: i64) -> Option<(i64, i64)> {
    match kind {
        RC_CHILD_RC_OK => Some((0, RC_CHILD_RC)),
        RC_CHILD_RC_ERR => Some((1, RC_CHILD_RC)),
        RC_CHILD_VEC_OK => Some((0, RC_CHILD_VEC)),
        RC_CHILD_VEC_ERR => Some((1, RC_CHILD_VEC)),
        RC_CHILD_WEAK_OK => Some((0, RC_CHILD_WEAK)),
        RC_CHILD_WEAK_ERR => Some((1, RC_CHILD_WEAK)),
        _ => None,
    }
}

/// The copy-blob child kind for a carrier field whose payload is a blob under
/// discriminant `gate` (0, 1, or negative for both arms).
#[must_use]
pub const fn rc_child_blob_kind(gate: i64) -> i64 {
    match gate {
        0 => RC_CHILD_BLOB_OK,
        1 => RC_CHILD_BLOB_ERR,
        _ => RC_CHILD_BLOB_ANY,
    }
}

/// The discriminant gate of a copy-blob child kind (negative for both arms),
/// or `None` for any other kind.
#[must_use]
pub const fn rc_child_blob_gate(kind: i64) -> Option<i64> {
    match kind {
        RC_CHILD_BLOB_OK => Some(0),
        RC_CHILD_BLOB_ERR => Some(1),
        RC_CHILD_BLOB_ANY => Some(-1),
        _ => None,
    }
}

/// Whether a struct layout names a child that owns a table of its own: a
/// `Map`, set, deque, heap, iterator, or JSON handle. Copying such a value
/// clones the table, so the copy needs an owner even inside a region.
/// `word(i)` reads the layout's `i`th word; a layout of any other kind
/// answers `false`.
pub fn struct_meta_names_table_child(word: impl Fn(usize) -> i64) -> bool {
    if rc_kind(word(0)) != RC_KIND_STRUCT {
        return false;
    }
    let variants = word(1).max(0);
    let mut idx: usize = 2;
    for _ in 0..variants {
        let count = usize::try_from(word(idx + 1)).unwrap_or(0);
        let names_table = (0..count).any(|j| {
            matches!(
                word(idx + 2 + j) >> RC_CHILD_KIND_SHIFT,
                RC_CHILD_MAP
                    | RC_CHILD_SET
                    | RC_CHILD_DEQUE
                    | RC_CHILD_HEAP
                    | RC_CHILD_ITER
                    | RC_CHILD_ITER_PAIR
                    | RC_CHILD_JSON
            )
        });
        if names_table {
            return true;
        }
        idx += 2 + count;
    }
    false
}

/// The runtime entry that allocates a counted copy of a value laid out by
/// `meta` (`None` for a leaf), taking the source's shares of its children
/// when `moved`. A layout naming no table-owning child takes the entry that
/// bumps the open region's slab without inspecting the layout.
#[must_use]
pub fn rc_alloc_entry(moved: bool, meta: Option<&[i64]>) -> &'static str {
    let plain = meta
        .is_none_or(|words| !struct_meta_names_table_child(|i| words.get(i).copied().unwrap_or(0)));
    match (moved, plain) {
        (true, true) => "gos_rt_rc_alloc_move_plain",
        (true, false) => "gos_rt_rc_alloc_move",
        (false, true) => "gos_rt_rc_alloc_copy_plain",
        (false, false) => "gos_rt_rc_alloc_copy",
    }
}

/// [`rc_alloc_entry`] for a layout named by its symbol, `None` or empty for a
/// leaf. A symbol `layout` does not know takes the general entry.
#[must_use]
pub fn rc_alloc_entry_for<'a>(
    moved: bool,
    symbol: Option<&str>,
    layout: impl Fn(&str) -> Option<&'a [i64]>,
) -> &'static str {
    match symbol.filter(|sym| !sym.is_empty()) {
        None => rc_alloc_entry(moved, None),
        Some(sym) => match layout(sym) {
            Some(words) => rc_alloc_entry(moved, Some(words)),
            None if moved => "gos_rt_rc_alloc_move",
            None => "gos_rt_rc_alloc_copy",
        },
    }
}

/// The per-thread block compiled code bumps an open region's slab through:
/// the slab's base (null with no region open or no slab yet), the offset in
/// use, the slab's end offset, and the objects bumped into it.
pub mod region_bump {
    /// The block a compiled program defines, one per thread.
    pub const PROGRAM_BLOCK_SYMBOL: &str = "gos_rt_region_bump_tls";
    /// The runtime entry the program hands its block's accessor to.
    pub const ADOPT_SYMBOL: &str = "gos_rt_region_bump_adopt";
    /// Offset of the slab base pointer.
    pub const BASE_OFFSET: i64 = 0;
    /// Offset of the slab offset the next bump starts at.
    pub const CUR_OFFSET: i64 = 8;
    /// Offset of the slab's end offset.
    pub const END_OFFSET: i64 = 16;
    /// Offset of the count of objects bumped into the innermost region.
    pub const OBJS_OFFSET: i64 = 24;
    /// Every bump is a whole number of these bytes.
    pub const ALIGN: i64 = 8;
}

/// Layout of a counted object's header and the runtime data compiled code
/// reads to take a retain or release inline. The runtime pins its
/// `RcHeader` to these, and both code generators emit the fast paths from
/// them, so the three agree by construction.
pub mod header {
    /// Bytes of header in front of every counted payload.
    pub const BYTES: i64 = 8;
    /// Offset of the 32-bit count word from the payload.
    pub const STRONG_OFFSET: i64 = -8;
    /// Offset of the 16-bit type-meta id from the payload.
    pub const META_ID_OFFSET: i64 = -2;
    /// Mask of the strong count in the count word.
    pub const STRONG_COUNT_MASK: u32 = 0x07FF_FFFF;
    /// The object has escaped to another goroutine and counts atomically.
    pub const SHARED_BIT: u32 = 1 << 27;
    /// The cycle collector's two color bits begin here.
    pub const COLOR_SHIFT: u32 = 28;
    /// The object lives in a region and is freed with it, never counted.
    pub const REGION_BIT: u32 = 1 << 30;
    /// The object is buffered as a cycle candidate.
    pub const BUFFERED_BIT: u32 = 1 << 31;
    /// A strong count that never moves.
    pub const STRONG_IMMORTAL: u32 = STRONG_COUNT_MASK;
    /// Bits per word of the acyclic-type table.
    pub const ACYCLIC_WORD_BITS: u32 = 64;
    /// Nonzero while any counter, ledger, sampler, or trace is armed; every
    /// inline retain and release then takes the runtime call, which records
    /// it.
    pub const INSTRUMENTATION_SYMBOL: &str = "gos_rt_instrumentation";
    /// Base address of the region arena reservation; zero until one exists.
    pub const REGION_ARENA_BASE_SYMBOL: &str = "gos_rt_region_arena_base";
    /// Bytes the region arena reservation spans; zero without one.
    pub const REGION_ARENA_LEN_SYMBOL: &str = "gos_rt_region_arena_len";
    /// One bit per type-meta id, set when no cycle can pass through that
    /// layout; a release that leaves such an object alive buffers nothing.
    pub const META_ACYCLIC_SYMBOL: &str = "gos_rt_meta_acyclic";
}
