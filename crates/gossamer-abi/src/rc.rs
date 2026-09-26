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
