//! Slot kinds and the reference-counted children of slot containers.

use super::*;

// Slot-child kinds, mirroring `gossamer_runtime::c_abi::vec::vec_elem_kind`
// (kept in sync by value). `RC_NODE` covers user enum / struct heap
// pointers (tag-bit-encoded) released via `gos_rt_rc_release`.
pub(super) const SLOT_KIND_STRING: i64 = 1;
pub(super) const SLOT_KIND_VEC: i64 = 2;
pub(super) const SLOT_KIND_MAP: i64 = 3;
pub(super) const SLOT_KIND_RC_NODE: i64 = 7;
pub(super) const SLOT_KIND_SET: i64 = 11;
pub(super) const SLOT_KIND_DEQUE: i64 = 13;
pub(super) const SLOT_KIND_HEAP: i64 = 14;
pub(super) const SLOT_KIND_ITER: i64 = 15;
pub(super) const SLOT_KIND_ITER_PAIR: i64 = 16;
pub(super) const SLOT_KIND_WEAK: i64 = 17;
pub(super) const SLOT_HASH_SET_DEF_LOCAL: u32 = u32::MAX - 7;
pub(super) const SLOT_BTREE_SET_DEF_LOCAL: u32 = u32::MAX - 18;
pub(super) const SLOT_VEC_DEQUE_DEF_LOCAL: u32 = u32::MAX - 19;
pub(super) const SLOT_BINARY_HEAP_DEF_LOCAL: u32 = u32::MAX - 28;
pub(super) const SLOT_MIN_HEAP_DEF_LOCAL: u32 = u32::MAX - 30;
pub(super) const SLOT_VEC_QUEUE_DEF_LOCAL: u32 = u32::MAX - 31;
pub(super) const SLOT_VEC_STACK_DEF_LOCAL: u32 = u32::MAX - 32;

/// The standard containers a value reaches through a handle a copy cannot
/// share: a `Set`, a deque (`Deque` / `Queue` / `Stack`), or a heap.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum HandleContainer {
    Set,
    Deque,
    Heap,
}

/// Whether a lazy iterator yielding `item` runs on the two-word pair state
/// `zip` and `enumerate` build: exactly two `i64` halves. Every other element
/// rides the one-word state, an element wider than a slot as its address.
pub(crate) fn lazy_iter_is_pair_state(
    tcx: &gossamer_types::TyCtxt,
    item: gossamer_types::Ty,
) -> bool {
    use gossamer_types::{IntTy, TyKind};
    matches!(
        tcx.kind_of(item),
        TyKind::Tuple(fields)
            if fields.len() == 2
                && fields
                    .iter()
                    .all(|field| matches!(tcx.kind_of(*field), TyKind::Int(IntTy::I64)))
    )
}

/// The child kind an aggregate holds a lazy iterator field under, or `None`
/// when `ty` is not an iterator.
pub(crate) fn lazy_iter_child_kind(
    tcx: &gossamer_types::TyCtxt,
    ty: gossamer_types::Ty,
) -> Option<i64> {
    let gossamer_types::TyKind::Iterator(item) = tcx.kind_of(ty) else {
        return None;
    };
    Some(if lazy_iter_is_pair_state(tcx, *item) {
        gossamer_abi::rc::RC_CHILD_ITER_PAIR
    } else {
        gossamer_abi::rc::RC_CHILD_ITER
    })
}

/// Which [`HandleContainer`] `ty` is, if any.
pub(crate) fn handle_container(
    tcx: &gossamer_types::TyCtxt,
    ty: gossamer_types::Ty,
) -> Option<HandleContainer> {
    let gossamer_types::TyKind::Adt { def, .. } = tcx.kind_of(ty) else {
        return None;
    };
    match def.local {
        SLOT_HASH_SET_DEF_LOCAL | SLOT_BTREE_SET_DEF_LOCAL => Some(HandleContainer::Set),
        SLOT_VEC_DEQUE_DEF_LOCAL | SLOT_VEC_QUEUE_DEF_LOCAL | SLOT_VEC_STACK_DEF_LOCAL => {
            Some(HandleContainer::Deque)
        }
        SLOT_BINARY_HEAP_DEF_LOCAL | SLOT_MIN_HEAP_DEF_LOCAL => Some(HandleContainer::Heap),
        _ => None,
    }
}

/// The slot-child kind an element store owns a [`HandleContainer`] under.
pub(super) fn handle_slot_kind(container: HandleContainer) -> i64 {
    match container {
        HandleContainer::Set => SLOT_KIND_SET,
        HandleContainer::Deque => SLOT_KIND_DEQUE,
        HandleContainer::Heap => SLOT_KIND_HEAP,
    }
}

/// Walks the flat slot layout of a by-value aggregate `ty`, appending one
/// `(gate, disc_word, word, kind)` entry per RC child pointer the vec must
/// own. `gate` is `-1` for an unconditional pointer field, or the
/// discriminant value gating an `Option`/`Result` payload word. Sets
/// `has_direct` when an unconditional (non-`Option`/`Result`) RC field is
/// present - the signal that the element needs the `AGGR_OWNED` path
/// rather than the copy-blob-only `AGGR_GUARDED` path. Recurses through
/// nested inline struct / tuple fields at absolute word offsets.
pub(super) fn collect_slot_rc_children(
    tcx: &gossamer_types::TyCtxt,
    ty: gossamer_types::Ty,
    base_word: i64,
    depth: u32,
    out: &mut Vec<(i64, i64, i64, i64)>,
    has_direct: &mut bool,
) {
    use gossamer_types::TyKind;
    if depth > 8 {
        return;
    }
    let field_tys: Vec<gossamer_types::Ty> = match tcx.kind_of(ty) {
        TyKind::Tuple(elems) => elems.clone(),
        TyKind::Adt { def, substs } => {
            // Opaque stdlib handles / Weak / Option / Result sentinels have
            // their own teardown; never walk their declared field lists here.
            if def.local >= u32::MAX - 16 {
                return;
            }
            match tcx.adt_field_tys(*def, substs) {
                Some(f) => f.to_vec(),
                None => return,
            }
        }
        _ => return,
    };
    let mut word = base_word;
    for fty in field_tys {
        let fwords = i64::from(tcx.slot_bytes(fty).max(8) / 8);
        collect_field_rc(tcx, fty, word, depth, out, has_direct);
        word += fwords;
    }
}

/// Classifies one aggregate field at absolute `word`, appending its RC
/// child entry (or recursing into a nested inline aggregate).
pub(super) fn collect_field_rc(
    tcx: &gossamer_types::TyCtxt,
    fty: gossamer_types::Ty,
    word: i64,
    depth: u32,
    out: &mut Vec<(i64, i64, i64, i64)>,
    has_direct: &mut bool,
) {
    use gossamer_types::TyKind;
    match tcx.kind_of(fty) {
        // A `Weak` field holds a weak share of its target, which a copy of
        // the element takes and the element's death gives back; it is never
        // a strong owner.
        _ if tcx.is_weak_ty(fty) => {
            out.push((-1, 0, word, SLOT_KIND_WEAK));
            *has_direct = true;
        }
        TyKind::String => {
            out.push((-1, 0, word, SLOT_KIND_STRING));
            *has_direct = true;
        }
        TyKind::Vec(_) | TyKind::Slice(_) => {
            out.push((-1, 0, word, SLOT_KIND_VEC));
            *has_direct = true;
        }
        // A `GosMap` carries no reference count, so the element owns a table
        // of its own: the retain path clones it in and the free path drops it.
        // Without this entry the field is nobody's, and the frame that built
        // the element frees the table the element is left pointing at.
        TyKind::HashMap { .. } => {
            out.push((-1, 0, word, SLOT_KIND_MAP));
            *has_direct = true;
        }
        // A `GosSet` table is reached through a handle carrying no count of
        // its holders, exactly as a `GosMap` is, so the element store owns a
        // table per slot: the copy paths clone one in and the free path drops
        // it. `BTreeSet` shares the handle and the helpers.
        TyKind::Adt { .. } if handle_container(tcx, fty).is_some() => {
            if let Some(container) = handle_container(tcx, fty) {
                out.push((-1, 0, word, handle_slot_kind(container)));
            }
            *has_direct = true;
        }
        // `Option`/`Result`: the payload word holds a heap pointer only on
        // the side(s) whose inner type is heap-managed. Gate each side on
        // its discriminant (0 = Ok/Some, 1 = Err). Copy-blob and enum
        // payloads carry an `RcHeader`, so `gos_rt_rc_release` reclaims
        // them; a bare `String`/`Vec` payload uses its own kind.
        TyKind::Adt { def, substs } if def.local == u32::MAX || def.local == u32::MAX - 1 => {
            let payload_kind = |t: gossamer_types::Ty| -> Option<i64> {
                match tcx.kind_of(t) {
                    _ if tcx.is_weak_ty(t) => Some(SLOT_KIND_WEAK),
                    TyKind::String => Some(SLOT_KIND_STRING),
                    TyKind::Vec(_) | TyKind::Slice(_) => Some(SLOT_KIND_VEC),
                    TyKind::HashMap { .. } => Some(SLOT_KIND_MAP),
                    TyKind::Adt { .. } if handle_container(tcx, t).is_some() => {
                        handle_container(tcx, t).map(handle_slot_kind)
                    }
                    TyKind::Adt { .. } | TyKind::Tuple(_)
                        if tcx.is_rc_managed(t) || tcx.slot_bytes(t) > 8 =>
                    {
                        Some(SLOT_KIND_RC_NODE)
                    }
                    _ if tcx.is_counted_node(t) => Some(SLOT_KIND_RC_NODE),
                    _ => None,
                }
            };
            let tys = substs.types();
            let sides = [tys.first().copied(), tys.get(1).copied()];
            for (gate, side) in (0i64..).zip(sides) {
                let Some(k) = side.and_then(payload_kind) else {
                    continue;
                };
                out.push((gate, word, word + 1, k));
                // A `String`, container, enum-node, or callable payload is no
                // copy blob, so only the owned-children store gives it back.
                if k != SLOT_KIND_RC_NODE || side.is_some_and(|t| tcx.is_counted_node(t)) {
                    *has_direct = true;
                }
            }
        }
        // A callable's one word is its counted capture environment.
        TyKind::FnTrait(_) | TyKind::Closure { .. } => {
            out.push((-1, 0, word, SLOT_KIND_RC_NODE));
            *has_direct = true;
        }
        // An iterator field holds a share of the handle.
        TyKind::Iterator(item) => {
            let kind = if lazy_iter_is_pair_state(tcx, *item) {
                SLOT_KIND_ITER_PAIR
            } else {
                SLOT_KIND_ITER
            };
            out.push((-1, 0, word, kind));
            *has_direct = true;
        }
        TyKind::Adt { .. } => {
            if tcx.is_rc_managed(fty) {
                // Heap user enum (a single tag-encoded pointer slot).
                out.push((-1, 0, word, SLOT_KIND_RC_NODE));
                *has_direct = true;
            } else {
                // Inline struct / newtype: its fields occupy these slots.
                collect_slot_rc_children(tcx, fty, word, depth + 1, out, has_direct);
            }
        }
        TyKind::Tuple(_) => collect_slot_rc_children(tcx, fty, word, depth + 1, out, has_direct),
        _ => {}
    }
}

/// How a container owns one element of type `elem`, as the runtime call that
/// tags its element store. `None` when the element owns nothing beyond its
/// own slots.
pub(crate) enum ElemOwnership {
    /// Copy-blob children behind conditional (`Option` / `Result`) payloads.
    Guarded(String),
    /// Unconditional RC children (a `String`, nested vec, or enum pointer).
    Owned(String),
    /// The element is itself one payload-enum RC node.
    RcElems,
    /// The element is itself one counted container handle.
    VecElems,
    /// The element is itself one `Weak` reference.
    WeakElems,
}

impl ElemOwnership {
    /// The runtime symbol that tags an element store with this ownership.
    pub(crate) fn symbol(&self) -> &'static str {
        match self {
            Self::Guarded(_) => "gos_rt_vec_set_elem_meta",
            Self::Owned(_) => "gos_rt_vec_set_slot_children",
            Self::RcElems => "gos_rt_vec_mark_rc_elems",
            Self::VecElems => "gos_rt_vec_mark_vec_elems",
            Self::WeakElems => "gos_rt_vec_mark_weak_elems",
        }
    }

    /// The metadata blob symbol the call passes, when it takes one.
    pub(crate) fn meta(&self) -> Option<&str> {
        match self {
            Self::Guarded(sym) | Self::Owned(sym) => Some(sym),
            _ => None,
        }
    }
}

/// The ownership an element store of `elem` elements needs, registering any
/// metadata blob the runtime call refers to. The single answer both the
/// `Vec` construction pass and the slot-container constructors read, so a
/// `Deque<T>` owns its elements exactly as a `Vec<T>` does.
pub(crate) fn elem_ownership(
    tcx: &mut gossamer_types::TyCtxt,
    elem: gossamer_types::Ty,
) -> Option<ElemOwnership> {
    use gossamer_types::TyKind;
    if let Some(sym) = ensure_slot_children_meta(tcx, elem) {
        return Some(ElemOwnership::Owned(sym));
    }
    if let Some(sym) = tcx.aggr_copy_meta(elem)
        && let Some(blob) = tcx.rc_meta(sym)
        && blob.len() >= 2
        && blob[1] > 0
    {
        return Some(ElemOwnership::Guarded(sym.to_string()));
    }
    if tcx.is_counted_node(elem) {
        return Some(ElemOwnership::RcElems);
    }
    if tcx.is_weak_ty(elem) {
        return Some(ElemOwnership::WeakElems);
    }
    if matches!(tcx.kind_of(elem), TyKind::Vec(_) | TyKind::Slice(_)) {
        return Some(ElemOwnership::VecElems);
    }
    None
}

/// Registers (idempotently) the `AGGR_OWNED` slot-children meta for vec
/// element type `elem` and returns its symbol, or `None` when the element
/// carries no unconditional RC child pointer (in which case the copy-blob
/// `AGGR_GUARDED` path, if any, applies instead). Blob layout:
/// `[count, (gate, disc_word, word, kind) * count]`.
pub(super) fn ensure_slot_children_meta(
    tcx: &mut gossamer_types::TyCtxt,
    elem: gossamer_types::Ty,
) -> Option<String> {
    let mut children = Vec::new();
    let mut has_direct = false;
    // A bare `Set` element IS the slot: its one word holds the table handle,
    // which carries no count of its holders, so the store owns a table per
    // element - minted when an element is copied in, dropped with it. The
    // walk below descends into an aggregate's fields, so an element that is
    // itself such a handle is named here.
    if let Some(container) = handle_container(tcx, elem) {
        children.push((
            gossamer_abi::rc::SLOT_GATE_WHOLE_ELEMENT,
            0,
            0,
            handle_slot_kind(container),
        ));
        has_direct = true;
    } else if let gossamer_types::TyKind::Adt { def, .. } = tcx.kind_of(elem)
        && (def.local == u32::MAX || def.local == u32::MAX - 1)
    {
        // A bare `Option` / `Result` element is one carrier: its payload word
        // is the element's child on the arm that holds one.
        collect_field_rc(tcx, elem, 0, 0, &mut children, &mut has_direct);
    } else {
        collect_slot_rc_children(tcx, elem, 0, 0, &mut children, &mut has_direct);
    }
    if !has_direct || children.is_empty() {
        return None;
    }
    let symbol = format!("gos_rc_slotchildren_{}", elem.as_u32());
    let mut blob = Vec::with_capacity(1 + children.len() * 4);
    blob.push(children.len() as i64);
    for (gate, disc_word, word, kind) in &children {
        blob.push(*gate);
        blob.push(*disc_word);
        blob.push(*word);
        blob.push(*kind);
    }
    tcx.register_rc_meta(symbol.clone(), blob);
    Some(symbol)
}

/// The runtime marker naming how a map holding `value` values owns them, or
/// `None` for values it stores as plain words or bytes. The one answer the
/// construction pass and every backend's map literal read.
#[must_use]
pub fn map_value_owner_marker(
    tcx: &gossamer_types::TyCtxt,
    value: gossamer_types::Ty,
) -> Option<&'static str> {
    use gossamer_types::TyKind;
    let mut value = value;
    while let TyKind::Ref { inner, .. } = tcx.kind_of(value) {
        value = *inner;
    }
    if tcx.is_weak_ty(value) {
        return Some("gos_rt_map_set_weak_values");
    }
    // The backend copies an aggregate value into a blob whenever EITHER meta
    // is registered - it reads the structural one first and falls back to the
    // guarded copy meta - so the map has to be tagged as holding blob values
    // under exactly the same condition. Tagging is what makes the entry take
    // its own share and give it back at the entry's death.
    let structural = format!("gos_rc_meta_boxaggr_{}", value.as_u32());
    if tcx.aggr_copy_meta(value).is_some()
        || tcx.rc_meta(&structural).is_some()
        || tcx.is_counted_node(value)
    {
        return Some("gos_rt_map_set_blob_values");
    }
    match tcx.kind_of(value) {
        // A byte sequence is stored as the bytes themselves - the insert copies
        // them out and the entry owns no handle - so tagging the map as holding
        // vec shares would release something no entry holds.
        TyKind::Vec(elem) | TyKind::Slice(elem) => {
            let bytes = matches!(tcx.kind_of(*elem), TyKind::Int(gossamer_types::IntTy::U8));
            (!bytes).then_some("gos_rt_map_set_vec_values")
        }
        // A table carries no reference count, so each entry owns a copy.
        TyKind::HashMap { .. } => Some("gos_rt_map_set_map_values"),
        _ => handle_container(tcx, value).map(|container| match container {
            HandleContainer::Set => "gos_rt_map_set_set_values",
            HandleContainer::Deque => "gos_rt_map_set_deque_values",
            HandleContainer::Heap => "gos_rt_map_set_heap_values",
        }),
    }
}

/// Tags vecs whose element type carries a guarded copy-blob meta, right
/// after their construction, so the runtime retains each pushed
/// element's copy-blob children and releases them when the vec dies
/// (`gos_rt_vec_set_elem_meta` -> push/free/clone/slice handling).
/// Type-driven on the construction destination, so it covers literals,
/// `Vec::new`, `with_capacity`, and array->Vec coercions uniformly.
///
/// Elements that carry an unconditional (non-`Option`/`Result`) RC field -
/// a `String`, nested vec, or user enum/struct heap pointer - instead
/// take the `AGGR_OWNED` path (`gos_rt_vec_set_slot_children`): the vec
/// owns those children, retaining them on push and deep-freeing them on
/// free, so a by-value element pushed in and then dropped at its source
/// scope (or returned inside the vec) is reclaimed exactly once.
pub(crate) fn insert_vec_elem_metas(
    body: &mut Body,
    tcx: &mut gossamer_types::TyCtxt,
    comparators: &std::collections::HashSet<String>,
) {
    use gossamer_types::TyKind;
    let n_locals = body.locals.len();
    let is_vec_ctor = |name: &str| -> bool {
        matches!(
            name,
            "Vec::new"
                | "gos_rt_vec_new"
                | "gos_rt_vec_new_typed"
                | "gos_rt_vec_with_capacity"
                | "gos_rt_vec_with_capacity_typed"
                | "gos_rt_vec_repeat_primitive"
                | "gos_rt_vec_from_arr"
                | "gos_rt_nested_arr_to_vec"
        )
    };
    let is_map_ctor = |name: &str| -> bool {
        matches!(
            name,
            "Map::new" | "HashMap::new" | "gos_rt_map_new" | "gos_rt_map_new_with_capacity"
        )
    };
    let elem_ty_of = |l: Local, tcx: &gossamer_types::TyCtxt| -> Option<gossamer_types::Ty> {
        let i = l.0 as usize;
        if i >= n_locals {
            return None;
        }
        match tcx.kind_of(body.locals[i].ty) {
            TyKind::Vec(e) | TyKind::Slice(e) => Some(*e),
            _ => None,
        }
    };

    // Register the AGGR_OWNED slot-children meta for every vec-ctor whose
    // element carries an unconditional RC field. Done first, while `tcx`
    // can be borrowed mutably, before the immutable detection closures.
    let mut owned_meta: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
    {
        let mut ctor_dests: Vec<Local> = Vec::new();
        for block in &body.blocks {
            for stmt in &block.stmts {
                if let StatementKind::Assign {
                    place,
                    rvalue: Rvalue::CallIntrinsic { name, .. },
                } = &stmt.kind
                    && place.projection.is_empty()
                    && is_vec_ctor(name)
                {
                    ctor_dests.push(place.local);
                }
            }
            if let Terminator::Call {
                callee: Operand::Const(ConstValue::Str(name)),
                destination,
                ..
            } = &block.terminator
                && destination.projection.is_empty()
                && is_vec_ctor(name)
            {
                ctor_dests.push(destination.local);
            }
        }
        for l in ctor_dests {
            if owned_meta.contains_key(&l.0) {
                continue;
            }
            if let Some(elem) = elem_ty_of(l, tcx)
                && let Some(sym) = ensure_slot_children_meta(tcx, elem)
            {
                owned_meta.insert(l.0, sym);
            }
        }
    }

    // The teardown call to schedule for one vec/map construction.
    enum VecMeta {
        Guarded(String),
        Owned(String),
        RcElems,
        VecElems,
        WeakElems,
        /// The marker naming how the map owns its values.
        MapValues(&'static str),
        MapFloatKeys,
        MapOrdered {
            unsigned: bool,
        },
        /// The key type writes its own `cmp`, which the tree calls: the
        /// comparator's symbol, and whether it takes its keys by address.
        MapOrderedBy {
            comparator: String,
            by_address: bool,
            unsigned: bool,
        },
    }

    // Guarded copy-blob meta of a vec element - but only when the element
    // did NOT take the owned path (the owned layout already covers every
    // RC child, including `Option`/`Result` payloads).
    let elem_meta_of = |l: Local| -> Option<String> {
        if owned_meta.contains_key(&l.0) {
            return None;
        }
        let elem = elem_ty_of(l, tcx)?;
        let sym = tcx.aggr_copy_meta(elem)?;
        let blob = tcx.rc_meta(sym)?;
        if blob.len() >= 2 && blob[1] > 0 {
            Some(sym.to_string())
        } else {
            None
        }
    };
    let map_value_owner = |l: Local| -> Option<VecMeta> {
        let i = l.0 as usize;
        if i >= n_locals {
            return None;
        }
        let TyKind::HashMap { value, .. } = tcx.kind_of(body.locals[i].ty) else {
            return None;
        };
        map_value_owner_marker(tcx, *value).map(VecMeta::MapValues)
    };

    // A float-keyed map sorts its keys by value in every ordered traversal.
    let map_float_keys = |l: Local| -> Option<VecMeta> {
        let ty = body.locals.get(l.0 as usize)?.ty;
        let TyKind::HashMap { key, .. } = tcx.kind_of(ty) else {
            return None;
        };
        matches!(tcx.kind_of(*key), TyKind::Float(_)).then_some(VecMeta::MapFloatKeys)
    };

    // The comparator a key type writes for itself, and whether it takes its
    // keys by address: an aggregate crosses by the address of its slots, a
    // node or a scalar by its word.
    let user_comparator = |key: gossamer_types::Ty| -> Option<(String, bool)> {
        let mut key = key;
        while let TyKind::Ref { inner, .. } = tcx.kind_of(key) {
            key = *inner;
        }
        let name = match tcx.kind_of(key) {
            TyKind::Adt { def, .. } | TyKind::Nominal { def, .. } => tcx.def_name(*def)?,
            _ => return None,
        };
        let symbol = format!(
            "{}{}",
            gossamer_ast::USER_COMPARATOR_PREFIX,
            name.replace("::", "__")
        );
        if !comparators.contains(&symbol) {
            return None;
        }
        let by_address = tcx.is_flat_inline_aggregate(key);
        Some((symbol, by_address))
    };

    // A `BTreeMap` keeps its entries in the ordered tree, in the language's
    // order for the key or in the one the key type writes for itself.
    let map_ordered = |l: Local| -> Option<VecMeta> {
        let ty = body.locals.get(l.0 as usize)?.ty;
        let TyKind::HashMap {
            key, ordered: true, ..
        } = tcx.kind_of(ty)
        else {
            return None;
        };
        let key = *key;
        let unsigned = matches!(
            tcx.kind_of(key),
            TyKind::Int(gossamer_types::IntTy::U64 | gossamer_types::IntTy::Usize)
        );
        match user_comparator(key) {
            Some((comparator, by_address)) => Some(VecMeta::MapOrderedBy {
                comparator,
                by_address,
                unsigned,
            }),
            None => Some(VecMeta::MapOrdered { unsigned }),
        }
    };

    let vec_meta_of = |l: Local| -> Option<VecMeta> {
        if let Some(sym) = owned_meta.get(&l.0) {
            return Some(VecMeta::Owned(sym.clone()));
        }
        if let Some(meta) = elem_meta_of(l).map(VecMeta::Guarded) {
            return Some(meta);
        }
        // A payload-enum or callable element is a single RC node pointer the
        // vec owns outright: push moves the frame's share in
        // (`gos_rt_vec_push` is a consuming call for RC-managed locals), so
        // the vec's free releases each element. String elements keep their
        // dedicated `STRING` kind; `Weak` elements take the weak kind below.
        if elem_ty_of(l, tcx).is_some_and(|e| tcx.is_counted_node(e)) {
            return Some(VecMeta::RcElems);
        }
        // A nested-vec element is a refcounted container the outer vec
        // owns one share of (the push minted it); free must release each
        // element or the inner vecs leak.
        if elem_ty_of(l, tcx)
            .is_some_and(|e| matches!(tcx.kind_of(e), TyKind::Vec(_) | TyKind::Slice(_)))
        {
            return Some(VecMeta::VecElems);
        }
        // A `Weak` element holds a weak share the vec gives back at its death.
        if elem_ty_of(l, tcx).is_some_and(|e| tcx.is_weak_ty(e)) {
            return Some(VecMeta::WeakElems);
        }
        None
    };

    // (block, stmt-gap, dest local, meta) for statement ctors; block-head
    // inserts at the call target for terminator ctors.
    let mut stmt_inserts: Vec<(usize, usize, Local, VecMeta)> = Vec::new();
    let mut head_inserts: Vec<(usize, Local, VecMeta)> = Vec::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        for (si, stmt) in block.stmts.iter().enumerate() {
            if let StatementKind::Assign {
                place,
                rvalue: Rvalue::CallIntrinsic { name, .. },
            } = &stmt.kind
                && place.projection.is_empty()
            {
                if is_vec_ctor(name)
                    && let Some(meta) = vec_meta_of(place.local)
                {
                    stmt_inserts.push((bi, si + 1, place.local, meta));
                }
                if is_map_ctor(name)
                    && let Some(meta) = map_value_owner(place.local)
                {
                    stmt_inserts.push((bi, si + 1, place.local, meta));
                }
                if is_map_ctor(name)
                    && let Some(meta) = map_float_keys(place.local)
                {
                    stmt_inserts.push((bi, si + 1, place.local, meta));
                }
                if is_map_ctor(name)
                    && let Some(meta) = map_ordered(place.local)
                {
                    stmt_inserts.push((bi, si + 1, place.local, meta));
                }
            }
        }
        if let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            destination,
            target: Some(t),
            ..
        } = &block.terminator
            && destination.projection.is_empty()
        {
            if is_vec_ctor(name)
                && let Some(meta) = vec_meta_of(destination.local)
            {
                head_inserts.push((t.0 as usize, destination.local, meta));
            }
            if is_map_ctor(name)
                && let Some(meta) = map_value_owner(destination.local)
            {
                head_inserts.push((t.0 as usize, destination.local, meta));
            }
            if is_map_ctor(name)
                && let Some(meta) = map_float_keys(destination.local)
            {
                head_inserts.push((t.0 as usize, destination.local, meta));
            }
            if is_map_ctor(name)
                && let Some(meta) = map_ordered(destination.local)
            {
                head_inserts.push((t.0 as usize, destination.local, meta));
            }
        }
    }
    if stmt_inserts.is_empty() && head_inserts.is_empty() {
        return;
    }

    let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
    let mut next_unit = body.locals.len();
    // The marker statement, and the one that has to run before it: a marker
    // naming the key type's own comparator reads its address first.
    // The locals a comparator's address lands in, which hold a word rather
    // than the unit every other marker destination does.
    let mut word_locals: Vec<u32> = Vec::new();
    let mk = |l: Local,
              meta: &VecMeta,
              span: gossamer_lex::Span,
              next_unit: &mut usize,
              word_locals: &mut Vec<u32>|
     -> (Option<Statement>, Statement) {
        let mut prelude = None;
        let dest = Local(u32::try_from(*next_unit).expect("local overflow"));
        *next_unit += 1;
        let rvalue = match meta {
            VecMeta::MapValues(marker) => Rvalue::CallIntrinsic {
                name: marker,
                args: vec![Operand::Copy(Place::local(l))],
            },
            VecMeta::MapFloatKeys => Rvalue::CallIntrinsic {
                name: "gos_rt_map_set_float_keys",
                args: vec![Operand::Copy(Place::local(l))],
            },
            VecMeta::MapOrderedBy {
                comparator,
                by_address,
                unsigned,
            } => {
                let address = Local(u32::try_from(*next_unit).expect("local overflow"));
                *next_unit += 1;
                word_locals.push(address.0);
                prelude = Some(Statement {
                    kind: StatementKind::Assign {
                        place: Place::local(address),
                        rvalue: Rvalue::CallIntrinsic {
                            name: "gos_fn_addr",
                            args: vec![Operand::Const(ConstValue::Str(comparator.clone()))],
                        },
                    },
                    span,
                    inlined: None,
                });
                Rvalue::CallIntrinsic {
                    name: "gos_rt_map_set_ordered_by",
                    args: vec![
                        Operand::Copy(Place::local(l)),
                        Operand::Copy(Place::local(address)),
                        Operand::Const(ConstValue::Int(i128::from(*by_address))),
                        Operand::Const(ConstValue::Int(i128::from(*unsigned))),
                    ],
                }
            }
            VecMeta::MapOrdered { unsigned } => Rvalue::CallIntrinsic {
                name: "gos_rt_map_set_ordered",
                args: vec![
                    Operand::Copy(Place::local(l)),
                    Operand::Const(ConstValue::Int(i128::from(*unsigned))),
                ],
            },
            VecMeta::Guarded(sym) => Rvalue::CallIntrinsic {
                name: "gos_rt_vec_set_elem_meta",
                args: vec![
                    Operand::Copy(Place::local(l)),
                    Operand::Const(ConstValue::Str(sym.clone())),
                ],
            },
            VecMeta::Owned(sym) => Rvalue::CallIntrinsic {
                name: "gos_rt_vec_set_slot_children",
                args: vec![
                    Operand::Copy(Place::local(l)),
                    Operand::Const(ConstValue::Str(sym.clone())),
                ],
            },
            VecMeta::RcElems => Rvalue::CallIntrinsic {
                name: "gos_rt_vec_mark_rc_elems",
                args: vec![Operand::Copy(Place::local(l))],
            },
            VecMeta::VecElems => Rvalue::CallIntrinsic {
                name: "gos_rt_vec_mark_vec_elems",
                args: vec![Operand::Copy(Place::local(l))],
            },
            VecMeta::WeakElems => Rvalue::CallIntrinsic {
                name: "gos_rt_vec_mark_weak_elems",
                args: vec![Operand::Copy(Place::local(l))],
            },
        };
        (
            prelude,
            Statement {
                kind: StatementKind::Assign {
                    place: Place::local(dest),
                    rvalue,
                },
                span,
                inlined: None,
            },
        )
    };

    for (bi, l, meta) in &head_inserts {
        let span = body.blocks[*bi].span;
        let (prelude, stmt) = mk(*l, meta, span, &mut next_unit, &mut word_locals);
        let mut added = 1;
        body.blocks[*bi].stmts.insert(0, stmt);
        if let Some(prelude) = prelude {
            body.blocks[*bi].stmts.insert(0, prelude);
            added += 1;
        }
        // Shift any statement-gap inserts in the same block.
        for ins in &mut stmt_inserts {
            if ins.0 == *bi {
                ins.1 += added;
            }
        }
    }
    // Insert in descending gap order so earlier indices stay valid.
    let mut by_block: Vec<(usize, usize, Local, VecMeta)> = stmt_inserts;
    by_block.sort_by_key(|ins| std::cmp::Reverse((ins.0, ins.1)));
    for (bi, gap, l, meta) in by_block {
        let span = body.blocks[bi].span;
        let (prelude, stmt) = mk(l, &meta, span, &mut next_unit, &mut word_locals);
        body.blocks[bi].stmts.insert(gap, stmt);
        if let Some(prelude) = prelude {
            body.blocks[bi].stmts.insert(gap, prelude);
        }
    }
    let i64_ty = tcx.int_ty(gossamer_types::IntTy::I64);
    for index in body.locals.len()..next_unit {
        let holds_word = word_locals.contains(&(index as u32));
        body.locals.push(LocalDecl {
            ty: if holds_word { i64_ty } else { unit_ty },
            debug_name: None,
            mutable: false,
            region: false,
        });
    }
}

/// Releases owned heap values at their last use instead of at function
/// return, so peak RSS tracks the live set rather than the frame's
/// lifetime (a function that builds a large tree, prints a summary, and
/// then loops for seconds was holding the tree the whole time).
///
/// The pass piggybacks on the ownership judgments the earlier passes
/// already encoded in the IR: a local is a candidate exactly when a
/// return block carries a release for it (`gos_rt_rc_release` /
/// `gos_rt_rc_weak_release` from `insert_rc_releases`,
/// `gos_rt_aggr_release_children` / `gos_rt_option_slot_release` from
/// `insert_aggr_copy_drops`). For each candidate it finds the blocks
/// from whose exit no further *real* mention of the local is reachable
/// (accounting intrinsics and constant stores don't count), inserts the
/// matching release right after the last mention - or at the head of
/// each successor when the last mention is the terminator - and nulls
/// the local out. The return-block releases stay in place as a null-safe
/// backstop, so a path this analysis misses leaks nothing and a path it
/// covers cannot double-release.
///
/// Locals that appear in an `Rvalue::Ref` are pinned (released at
/// return only): the borrow's pointer value could outlive the last
/// direct mention.
/// One pending early release: insert after statement `usize` for `Local`,
/// via the named release intrinsic with an optional meta symbol.
pub(super) type PendingRelease = (usize, Local, &'static str, Option<String>);

/// True for the RC retain intrinsics [`insert_aggr_copy_drops`] anchors to
/// the statement whose destination they cover.
pub(super) fn is_rc_retain_intrinsic(name: &str) -> bool {
    matches!(
        name,
        "gos_rt_rc_retain"
            | "gos_rt_rc_weak_retain"
            | "gos_rt_vec_retain"
            | "gos_rt_str_retain_typed"
            | "gos_rt_aggr_retain_children"
            | "gos_rt_option_slot_retain"
    )
}

/// Index of the last statement in the run of RC retains that follows `si`,
/// or `si` itself when none does. Those retains take the share the
/// statement's destination keeps, so they read a payload `si` may hold the
/// only reference to.
pub(super) fn retain_anchor_end(stmts: &[Statement], si: usize) -> usize {
    let mut end = si;
    while let Some(Statement {
        kind:
            StatementKind::Assign {
                rvalue: Rvalue::CallIntrinsic { name, .. },
                ..
            },
        ..
    }) = stmts.get(end + 1)
    {
        if !is_rc_retain_intrinsic(name) {
            break;
        }
        end += 1;
    }
    end
}

/// Drops the previous-value lookup from a map insert whose result nothing
/// reads.
///
/// `m.insert(k, v)` answers the value it replaced, and the answer carries a
/// share of it for whoever receives it. In statement position nobody does, so
/// that share has no one to give it back and a loop overwriting one key keeps
/// every value it replaced. The insert that answers nothing does the same
/// store without the lookup, which is both correct here and one hash probe
/// cheaper.
pub(crate) fn drop_unread_map_insert_results(body: &mut Body, tcx: &gossamer_types::TyCtxt) {
    if body.locals.is_empty() {
        return;
    }
    let reads = collect_local_read_counts(body);
    let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
    let mut rewrites: Vec<(usize, &'static str)> = Vec::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            destination,
            ..
        } = &block.terminator
        else {
            continue;
        };
        if !destination.projection.is_empty()
            || !answer_is_discarded(body, &reads, destination.local)
        {
            continue;
        }
        let plain = match name.as_str() {
            "gos_rt_map_insert_i64_i64_opt" => "gos_rt_map_insert_i64_i64",
            "gos_rt_map_insert_str_i64_opt" => "gos_rt_map_insert_str_i64",
            "gos_rt_map_insert_typed_str_i64_opt" => "gos_rt_map_insert_typed_str_i64",
            "gos_rt_map_insert_str_str_opt" => "gos_rt_map_insert_str_str",
            "gos_rt_map_insert_i64_str_opt" => "gos_rt_map_insert_i64_str",
            _ => continue,
        };
        rewrites.push((bi, plain));
    }
    for (bi, plain) in rewrites {
        // The insert that answers nothing returns unit where the one it
        // replaces returns a two-word carrier, so the destination has to be a
        // local of the new shape: keeping the carrier-typed one would have the
        // backend read a 16-byte answer out of a call that leaves none.
        let sink = Local(u32::try_from(body.locals.len()).expect("local overflow"));
        body.locals.push(LocalDecl {
            ty: unit_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
        let Terminator::Call {
            callee,
            destination,
            ..
        } = &mut body.blocks[bi].terminator
        else {
            continue;
        };
        *callee = Operand::Const(ConstValue::Str(plain.to_string()));
        *destination = Place::local(sink);
    }
}
