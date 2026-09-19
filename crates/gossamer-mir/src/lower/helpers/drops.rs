#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::wildcard_imports)]
#![allow(clippy::similar_names)]
#![allow(clippy::many_single_char_names)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::option_if_let_else)]
#![allow(clippy::match_same_arms)]
#![allow(clippy::if_not_else)]
#![allow(clippy::single_match_else)]
#![allow(clippy::needless_pass_by_value)]
#![allow(clippy::manual_let_else)]
#![allow(clippy::redundant_else)]
#![allow(clippy::collapsible_if)]
#![allow(clippy::collapsible_else_if)]
#![allow(clippy::map_unwrap_or)]
#![allow(clippy::struct_excessive_bools)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::unnecessary_wraps)]
#![allow(clippy::large_enum_variant)]
#![allow(clippy::if_same_then_else)]
#![allow(clippy::single_match)]
#![allow(clippy::useless_conversion)]
#![allow(clippy::needless_borrows_for_generic_args)]
#![allow(clippy::let_and_return)]
#![allow(clippy::needless_collect)]

use std::collections::HashMap;

use gossamer_ast::Ident;
use gossamer_hir::{
    HirAdtKind, HirBinaryOp, HirBlock, HirExpr, HirExprKind, HirFn, HirItem, HirItemKind,
    HirLiteral, HirMatchArm, HirPat, HirPatKind, HirProgram, HirStmt, HirStmtKind, HirUnaryOp,
};
use gossamer_lex::Span;
use gossamer_types::{Ty, TyCtxt};

use crate::ir::{
    BasicBlock, BinOp, BlockId, Body, ConstValue, Local, LocalDecl, Operand, Place, Rvalue,
    Statement, StatementKind, Terminator, UnOp,
};

use super::*;

/// Inserts balanced `gos_rt_rc_retain` / `gos_rt_rc_release` calls for
/// reference-counted heap values so the compiled tier matches the
/// interpreter tier's `Arc` clone/drop semantics. This is the sound RC
/// model: the strong count always equals the number of live references,
/// so aliasing (`let b = a; let c = a`), returning a borrowed argument,
/// storing into a struct, etc. are all handled by the counts - there is
/// no fragile move/escape/ownership inference to get wrong.
///
/// Acquisitions (`+1`, emit a retain at the site) - any operation that
/// creates a new reference to an RC value:
/// - `to = Copy(from)` (binding/assignment, including into the return
///   slot - that mints the caller's reference),
/// - `gos_store(obj, off, val)` (the heap object gains a child reference;
///   freed transitively when the object's refcount hits zero),
/// - an aggregate operand / `Repeat` element (the struct/tuple/array
///   gains a reference),
/// - a consuming container/channel call argument.
///
/// Releases (`-1`): every RC-managed local that is neither a parameter
/// nor the return slot, at every return and before every reassignment.
/// Such locals are zeroed at entry so each release is null-safe on any
/// path. Parameters are borrowed (the caller owns and releases them) and
/// the return slot is transferred to the caller, so neither is released
/// here - and because every new reference retains, this is balanced with
/// no callee-signature analysis.
/// How one heap-managed field of a by-value aggregate is retained/released
/// at its owner's copy/death. Selects the runtime helper pair so a `Vec` /
/// `[T]` field (no RC header, routed through the vec allocator's own count)
/// is never handed to `gos_rt_rc_release` (which would read a nonexistent
/// header and corrupt the heap).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FieldRcKind {
    /// A `String` / boxed-enum / RC-node field: `gos_rt_rc_retain` /
    /// `gos_rt_rc_release`.
    Rc,
    /// A `Weak<T>` field: `gos_rt_rc_weak_retain` / `gos_rt_rc_weak_release`.
    Weak,
    /// A `Vec<T>` / `[T]` field: `gos_rt_vec_retain` / `gos_rt_vec_free`.
    Vec,
    /// A `Map` field. A `GosMap` has no reference count, so the field is its
    /// map's sole owner: a copy takes a clone of its own
    /// (`gos_rt_map_field_clone`), and a death or overwrite frees through
    /// `gos_rt_map_field_release`, which nulls the slot so a release booked
    /// on more than one exit edge stays a no-op. Both take the FIELD's
    /// address rather than the map word.
    Map,
    /// A `Set` / `BTreeSet` field, owned the way a [`FieldRcKind::Map`] field
    /// is: a `GosSet` carries no reference count either.
    Set,
    /// A `Deque` / `Queue` / `Stack` field. The three share the `GosDeque`
    /// header, which carries no reference count.
    Deque,
    /// A `MinHeap` / `MaxHeap` field. A heap IS a `GosVec`, but its push and
    /// pop write the store in place rather than through a copy-on-write, so a
    /// copy takes a store of its own rather than a share of the same one.
    Heap,
    /// An `Option` / `Result` field an arm of which holds a heap payload the
    /// carrier owns: `gos_rt_result_payload_retain` /
    /// `gos_rt_result_payload_release` on the carrier's words, with the
    /// payload kind of each arm (`1` a `String`, `2` a `Vec`, `4` an
    /// `errors::Error` cell, `0` nothing).
    Carrier { ok: u8, err: u8 },
}

impl FieldRcKind {
    /// The `(retain, release)` helper pair for a field whose container carries
    /// no reference count of its own, or `None` for a reference-counted one.
    ///
    /// "Retain" is a clone for these: a share of one container is a copy of
    /// its storage, since nothing counts holders. Both helpers take the
    /// FIELD's address and write the slot back.
    /// The `(retain, release)` helper pair for one field of this kind.
    pub(crate) const fn helpers(self) -> (&'static str, &'static str) {
        match self.value_container_helpers() {
            Some(pair) => pair,
            None => match self {
                Self::Weak => ("gos_rt_rc_weak_retain", "gos_rt_rc_weak_release"),
                Self::Vec => ("gos_rt_vec_retain", "gos_rt_vec_free"),
                _ => ("gos_rt_rc_retain", "gos_rt_rc_release"),
            },
        }
    }

    pub(crate) const fn value_container_helpers(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::Map => Some(("gos_rt_map_field_clone", "gos_rt_map_field_release")),
            Self::Set => Some(("gos_rt_set_field_clone", "gos_rt_set_field_release")),
            Self::Deque => Some(("gos_rt_deque_field_clone", "gos_rt_deque_field_release")),
            Self::Heap => Some(("gos_rt_bheap_field_clone", "gos_rt_bheap_field_release")),
            Self::Rc | Self::Weak | Self::Vec | Self::Carrier { .. } => None,
        }
    }

    /// True when a share of this field is a copy of its storage rather than a
    /// count on shared storage.
    pub(crate) const fn is_value_container(self) -> bool {
        self.value_container_helpers().is_some()
    }
}

/// One field-level retain/release in the by-value-aggregate teardown:
/// `(is_retain, aggregate_local, field_projection_path, kind)`. The path is
/// the chain of field indices from the aggregate local down to the heap
/// slot - one element for a direct field, more for a field nested inside a
/// by-value sub-struct or tuple.
type FieldGap = (bool, Local, Vec<u32>, FieldRcKind);

/// Heap-managed field projection paths of one by-value aggregate local:
/// each `(field_projection_path, kind)`.
type AggFieldPaths = Vec<(Vec<u32>, FieldRcKind)>;

/// Heap-managed field projection paths within a by-value aggregate, each
/// paired with the runtime helper family that frees it. Recurses through
/// by-value struct, tuple, and fixed-array fields, so an `Outer { inner: Inner { s:
/// String } }` releases `inner.s` when `Outer` dies; a non-recursive walk
/// left the nested `String` retained forever. `Vec` / `[T]` fields are
/// included with [`FieldRcKind::Vec`] so a struct's backing vector is freed
/// through `gos_rt_vec_free` when the struct dies (a stack-value aggregate
/// has no other teardown that reaches its vec field). Sentinel / inline-enum
/// ADTs are excluded (their own teardown frees them), and the whole sentinel
/// range (`u32::MAX - 16 ..`) is skipped because those ADTs lower to opaque
/// one-slot handles whose declared field lists do not describe the alloca.
/// Shared with the struct-literal `..base` retain (`expr_field.rs`) so retain
/// and release recurse in lockstep and a nested shared field is freed exactly
/// once.
/// True for the string-accumulator helpers that take ownership of their first
/// argument and return the accumulator to store back.
///
/// Each of these frees or reuses the old buffer itself and hands back the
/// current one, so `s = f(s, ..)` is a move, not a fresh value beside a live
/// old one. Two consequences, and both must hold for every name here: the
/// result must not be retained (an extra count forces the copy-on-write path,
/// making an append loop quadratic), and the destination must not be released
/// (the callee already owns the old buffer, so the release lands on the buffer
/// the call just returned - a use-after-free that then corrupts the owner
/// prefix a later append reads).
///
/// The two rules above are applied at different points in this pass, and this
/// predicate is what keeps them describing the same set of calls.
fn is_self_consuming_append(name: &str) -> bool {
    matches!(
        name,
        "gos_rt_str_concat_drop_a"
            | "gos_rt_str_append_i64"
            | "gos_rt_str_append_f64"
            | "gos_rt_str_append_bytes"
            | "gos_rt_str_push_char"
            | "gos_rt_str_push_byte"
            // Appends through `gos_rt_str_append_bytes` and answers the
            // accumulator in the carrier's payload.
            | "gos_rt_str_push_utf8"
    )
}

/// The type a place of type `ty` projects through: the referent for a
/// `&`/`&mut`, and `ty` itself otherwise. A field projection reads the same
/// field either way, so a rule keyed on the aggregate's fields resolves the
/// base through this.
fn pointee_of(tcx: &TyCtxt, ty: Ty) -> Ty {
    match tcx.kind_of(ty) {
        gossamer_types::TyKind::Ref { inner, .. } => *inner,
        _ => ty,
    }
}

/// Per-local: whether a container this frame constructed keeps a reference of
/// its own after being written into an aggregate the frame then returns.
///
/// Three references exist for `fn f() -> (i64, Vec<i64>) { (i, #[..]) }`: the
/// constructor's, the one the `Rvalue::Aggregate` retain mints for the tuple
/// local, and the one the return copy mints for the caller. The frame gives
/// up the tuple local's at the return-site field free, and the caller owns
/// the third - so the constructor's is the frame's to release, and treating
/// the operand as moved into the aggregate instead leaves it with nothing to
/// free it. A builder called in a loop then grows without bound.
///
/// The claim needs BOTH halves of that exchange visible, so it is only made
/// for an aggregate copied into the return slot. An aggregate handed to a
/// container instead - `Map::from([(k, #[..])])` - is stored by taking the
/// share it already holds rather than minting one, and adding a release here
/// would free a buffer the container still reads.
///
/// It also needs that construction to be the container's ONLY escape. An
/// in-place append writes through the container without taking a share of it
/// and does not count; a bare copy, a reference, a consuming call, or a store
/// into a heap slot each hand the pointer somewhere this frame cannot see.
fn aggregates_minting_their_own_share(body: &Body, tcx: &TyCtxt) -> Vec<bool> {
    use gossamer_types::TyKind;
    let n = body.locals.len();
    // Per container local: the aggregates it was written into, and whether
    // anything else could be holding its pointer.
    let mut carriers: Vec<Vec<Local>> = vec![Vec::new(); n];
    let mut escapes_elsewhere = vec![false; n];
    let mut returned = vec![false; n];
    // `(reference, referent)` for each whole-local reference taken.
    let mut ref_sources: Vec<(Local, Local)> = Vec::new();
    // `(copy, source)` for each whole-local bare copy.
    let mut copy_aliases: Vec<(Local, Local)> = Vec::new();
    // Locals read anywhere other than as a call argument.
    let mut read_beyond_calls = vec![false; n];
    fn note_read(op: &Operand, reads: &mut [bool]) {
        if let Operand::Copy(p) = op
            && let Some(slot) = reads.get_mut(p.local.0 as usize)
        {
            *slot = true;
        }
    }
    for block in &body.blocks {
        for stmt in &block.stmts {
            let reads = &mut read_beyond_calls;
            let mut note = |op: &Operand| note_read(op, reads);
            match &stmt.kind {
                StatementKind::Assign { place, rvalue } => {
                    if !place.projection.is_empty() {
                        note(&Operand::Copy(Place::local(place.local)));
                    }
                    match rvalue {
                        Rvalue::Use(op)
                        | Rvalue::UnaryOp { operand: op, .. }
                        | Rvalue::Cast { operand: op, .. }
                        | Rvalue::Repeat { value: op, .. } => note(op),
                        Rvalue::BinaryOp { lhs, rhs, .. } => {
                            note(lhs);
                            note(rhs);
                        }
                        Rvalue::Aggregate { operands, .. } => operands.iter().for_each(&mut note),
                        Rvalue::CallIntrinsic { args, .. } => args.iter().for_each(&mut note),
                        Rvalue::Ref { place: p, .. } | Rvalue::Len(p) => {
                            note(&Operand::Copy(Place::local(p.local)));
                        }
                        Rvalue::StaticLoad(_) => {}
                    }
                }
                StatementKind::StaticStore { value, .. } => note(value),
                _ => {}
            }
        }
        match &block.terminator {
            Terminator::Call { callee, .. } => {
                if let Operand::Copy(p) = callee
                    && (p.local.0 as usize) < n
                {
                    read_beyond_calls[p.local.0 as usize] = true;
                }
            }
            Terminator::Drop { place, .. }
            | Terminator::SwitchInt {
                discriminant: Operand::Copy(place),
                ..
            } if (place.local.0 as usize) < n => {
                read_beyond_calls[place.local.0 as usize] = true;
            }
            _ => {}
        }
    }
    let container = |l: Local| -> bool {
        body.locals.get(l.0 as usize).is_some_and(|decl| {
            !decl.region && matches!(tcx.kind_of(decl.ty), TyKind::Vec(_) | TyKind::Slice(_))
        })
    };
    let note_escape = |op: &Operand, escapes: &mut Vec<bool>| {
        if let Operand::Copy(p) = op
            && p.projection.is_empty()
            && (p.local.0 as usize) < escapes.len()
        {
            escapes[p.local.0 as usize] = true;
        }
    };
    for block in &body.blocks {
        for stmt in &block.stmts {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                if let StatementKind::StaticStore { value, .. } = &stmt.kind {
                    note_escape(value, &mut escapes_elsewhere);
                }
                continue;
            };
            if let Rvalue::Use(Operand::Copy(src)) = rvalue
                && place.local == Local::RETURN
                && place.projection.is_empty()
                && src.projection.is_empty()
                && (src.local.0 as usize) < n
            {
                returned[src.local.0 as usize] = true;
            }
            match rvalue {
                Rvalue::Aggregate { operands, .. } => {
                    for op in operands {
                        if let Operand::Copy(p) = op
                            && p.projection.is_empty()
                            && (p.local.0 as usize) < n
                        {
                            if place.projection.is_empty() {
                                carriers[p.local.0 as usize].push(place.local);
                            } else {
                                escapes_elsewhere[p.local.0 as usize] = true;
                            }
                        }
                    }
                }
                Rvalue::Use(Operand::Copy(src))
                    if src.projection.is_empty()
                        && place.projection.is_empty()
                        && place.local != Local::RETURN
                        && (src.local.0 as usize) < n
                        && (place.local.0 as usize) < n =>
                {
                    copy_aliases.push((place.local, src.local));
                }
                Rvalue::Use(op) | Rvalue::Cast { operand: op, .. } => {
                    note_escape(op, &mut escapes_elsewhere);
                }
                Rvalue::Repeat { value, .. } => note_escape(value, &mut escapes_elsewhere),
                Rvalue::CallIntrinsic { name, args } => {
                    // Tagging a container's element layout, or reading it,
                    // keeps no pointer to it.
                    let borrows_first = reads_container_only(name)
                        || matches!(
                            *name,
                            "gos_rt_vec_set_slot_children" | "gos_rt_vec_set_elem_meta"
                        );
                    for (idx, op) in args.iter().enumerate() {
                        if !(borrows_first && idx == 0) {
                            note_escape(op, &mut escapes_elsewhere);
                        }
                    }
                }
                Rvalue::Ref { place: p, .. } => {
                    if (p.local.0 as usize) < n {
                        if p.projection.is_empty() && place.projection.is_empty() {
                            ref_sources.push((place.local, p.local));
                        } else {
                            escapes_elsewhere[p.local.0 as usize] = true;
                        }
                    }
                }
                Rvalue::UnaryOp { .. }
                | Rvalue::BinaryOp { .. }
                | Rvalue::Len(_)
                | Rvalue::StaticLoad(_) => {}
            }
        }
        match &block.terminator {
            Terminator::Call { callee, args, .. } => {
                note_escape(callee, &mut escapes_elsewhere);
                // An in-place append writes through the container and a read
                // looks at it; neither keeps its pointer past the call.
                let in_place = matches!(
                    callee,
                    Operand::Const(ConstValue::Str(name))
                        if appends_through_container(name.as_str())
                            || reads_container_only(name.as_str())
                );
                // A program function borrows its arguments for the call and
                // takes a share of any it keeps.
                if matches!(callee, Operand::FnRef { .. }) {
                    continue;
                }
                for (idx, op) in args.iter().enumerate() {
                    if in_place && idx == 0 {
                        continue;
                    }
                    note_escape(op, &mut escapes_elsewhere);
                }
            }
            Terminator::Drop { place, .. } if (place.local.0 as usize) < n => {
                escapes_elsewhere[place.local.0 as usize] = true;
            }
            _ => {}
        }
    }
    // A reference handed only to calls names the container for the call's
    // duration: no parameter outlives its call, so nothing keeps it past the
    // frame. Any other use of the reference lets the pointer travel.
    for &(reference, referent) in &ref_sources {
        if read_beyond_calls[reference.0 as usize] {
            escapes_elsewhere[referent.0 as usize] = true;
        }
    }
    // A bare copy names the same container. It keeps the pointer only where
    // the copy itself does: read other than by a call, or handed to a call
    // that may hold on to it. Followed to a fixed point, since a copy of a
    // copy escapes through the last one.
    loop {
        let mut changed = false;
        for &(copy, source) in &copy_aliases {
            let (c, s) = (copy.0 as usize, source.0 as usize);
            if (escapes_elsewhere[c] || read_beyond_calls[c] || !carriers[c].is_empty())
                && !escapes_elsewhere[s]
            {
                escapes_elsewhere[s] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    // An aggregate takes a reference per slot, unconditionally: the retain for
    // a `Vec` operand of an `Rvalue::Aggregate` is minted whatever becomes of
    // the aggregate afterwards (see the `Rvalue::Aggregate` arm of the
    // retain-site walk). So a container written into one keeps a share of its
    // own, and the frame is who gives that back - whether the aggregate is
    // returned, pushed into a container, or dropped where it stands.
    //
    // `returned` still decides nothing here, but it stays computed: the caller
    // pairs this with `moved_into_return`, which is the case where the local's
    // own share does leave with the value.
    let _ = &returned;
    (0..n)
        .map(|i| {
            container(Local(u32::try_from(i).unwrap_or(0)))
                && !escapes_elsewhere[i]
                && !carriers[i].is_empty()
        })
        .collect()
}

/// Runtime calls that read the container they are handed as their first
/// argument and keep no pointer to it once they return.
fn reads_container_only(name: &str) -> bool {
    matches!(
        name,
        "gos_rt_len"
            | "gos_rt_vec_len"
            | "gos_rt_vec_get_i64"
            | "gos_rt_vec_get_i64_unchecked"
            | "gos_rt_vec_get_i128"
            | "gos_rt_vec_get_opt"
            | "gos_rt_vec_get_ptr"
            | "gos_rt_vec_get_ptr_unchecked"
            | "gos_rt_vec_contains_i64"
            | "gos_rt_vec_contains_str"
    )
}

/// Locals a call terminator answers into.
fn call_destinations(body: &Body) -> Vec<bool> {
    let mut out = vec![false; body.locals.len()];
    for block in &body.blocks {
        if let Terminator::Call { destination, .. } = &block.terminator
            && destination.projection.is_empty()
            && (destination.local.0 as usize) < out.len()
        {
            out[destination.local.0 as usize] = true;
        }
    }
    out
}

/// Whether `src` goes on holding the value container it names after an
/// aggregate is built from it: a call's answer, which the frame frees, or a
/// by-value parameter, which the caller frees.
fn container_source_keeps_own(body: &Body, src: Local, call_dest: &[bool]) -> bool {
    let s = src.0 as usize;
    call_dest.get(s).copied().unwrap_or(false) || (1..=body.arity as usize).contains(&s)
}

/// Whether slot `idx` of the aggregate built into `dest` takes a value
/// container of its own from `src`. A container with no reference count
/// cannot have two owners, so a source that keeps its own hands the slot a
/// clone, and the aggregate's field release frees only that clone.
fn aggregate_slot_takes_own_container(
    tcx: &TyCtxt,
    body: &Body,
    dest: &Place,
    idx: usize,
    src: &Place,
    call_dest: &[bool],
) -> bool {
    if !dest.projection.is_empty()
        || !src.projection.is_empty()
        || !container_source_keeps_own(body, src.local, call_dest)
    {
        return false;
    }
    let Some(decl) = body.locals.get(dest.local.0 as usize) else {
        return false;
    };
    let path = [u32::try_from(idx).unwrap_or(u32::MAX)];
    aggregate_rc_field_paths(tcx, decl.ty)
        .iter()
        .any(|(fp, kind)| fp.as_slice() == path && kind.is_value_container())
}

/// The field kind of an `Option` / `Result` whose `Some` / `Ok` payload is a
/// heap value the carrier owns, or `None` for any other type.
pub(crate) fn carrier_field_kind(tcx: &TyCtxt, ty: Ty) -> Option<FieldRcKind> {
    use gossamer_types::TyKind;
    let TyKind::Adt { def, substs } = tcx.kind_of(ty) else {
        return None;
    };
    if def.local != u32::MAX && def.local != u32::MAX - 1 {
        return None;
    }
    let arm = |payload: Option<&Ty>| match payload {
        Some(t) if tcx.is_counted_node(*t) => 4,
        Some(t) => match tcx.kind_of(*t) {
            TyKind::String => 1,
            TyKind::Vec(_) | TyKind::Slice(_) => 2,
            TyKind::DynError => 4,
            _ => 0,
        },
        None => 0,
    };
    let types = substs.types();
    let (ok, err) = (arm(types.first()), arm(types.get(1)));
    (ok != 0 || err != 0).then_some(FieldRcKind::Carrier { ok, err })
}

pub(crate) fn aggregate_rc_field_paths(tcx: &TyCtxt, ty: Ty) -> AggFieldPaths {
    fn recursable(tcx: &TyCtxt, ty: Ty) -> bool {
        use gossamer_types::TyKind;
        match tcx.kind_of(ty) {
            TyKind::Adt { def, .. } => def.local < u32::MAX - 16 && !tcx.is_inline_enum_ty(ty),
            TyKind::Tuple(_) | TyKind::Array { .. } => true,
            _ => false,
        }
    }
    /// The helper family for a field type, or `None` when the field owns no
    /// heap child this walk should free.
    fn field_kind(tcx: &TyCtxt, t: Ty) -> Option<FieldRcKind> {
        use gossamer_types::TyKind;
        // A `Vec`/`[T]` field frees through the vec allocator's own
        // count at its owner's death; a projected reassignment
        // (`c.field = [...]`) releases the old buffer before the store
        // and retains the new one after it (the projected-store arm in
        // the field-gap pass), so the RHS temp's own cleanup and the
        // death free each hold their own share.
        // The container families a struct field can hold whose storage is
        // reached through a handle that carries no reference count. A copy of
        // the field would otherwise name one store under two owners, so the
        // copy takes a store of its own and the field's death frees it.
        const HASH_SET_DEF_LOCAL: u32 = u32::MAX - 7;
        const BTREE_SET_DEF_LOCAL: u32 = u32::MAX - 18;
        const VEC_DEQUE_DEF_LOCAL: u32 = u32::MAX - 19;
        const BINARY_HEAP_DEF_LOCAL: u32 = u32::MAX - 28;
        const MIN_HEAP_DEF_LOCAL: u32 = u32::MAX - 30;
        const VEC_QUEUE_DEF_LOCAL: u32 = u32::MAX - 31;
        const VEC_STACK_DEF_LOCAL: u32 = u32::MAX - 32;
        if matches!(tcx.kind_of(t), TyKind::Vec(_) | TyKind::Slice(_)) {
            Some(FieldRcKind::Vec)
        } else if matches!(tcx.kind_of(t), TyKind::HashMap { .. }) {
            Some(FieldRcKind::Map)
        } else if let TyKind::Adt { def, .. } = tcx.kind_of(t)
            && let Some(kind) = match def.local {
                HASH_SET_DEF_LOCAL | BTREE_SET_DEF_LOCAL => Some(FieldRcKind::Set),
                VEC_DEQUE_DEF_LOCAL | VEC_QUEUE_DEF_LOCAL | VEC_STACK_DEF_LOCAL => {
                    Some(FieldRcKind::Deque)
                }
                BINARY_HEAP_DEF_LOCAL | MIN_HEAP_DEF_LOCAL => Some(FieldRcKind::Heap),
                _ => None,
            }
        {
            Some(kind)
        } else if let Some(kind) = carrier_field_kind(tcx, t) {
            Some(kind)
        } else if tcx.is_rc_managed(t) {
            Some(if tcx.is_weak_ty(t) {
                FieldRcKind::Weak
            } else {
                FieldRcKind::Rc
            })
        } else {
            None
        }
    }
    fn walk(tcx: &TyCtxt, ty: Ty, prefix: &mut Vec<u32>, out: &mut AggFieldPaths) {
        use gossamer_types::TyKind;
        // By-value aggregates cannot contain themselves because that would
        // have infinite size, so the structural walk terminates without an
        // arbitrary nesting limit.
        let field_tys: Vec<Ty> = match tcx.kind_of(ty) {
            TyKind::Adt { def, substs }
                if def.local < u32::MAX - 16 && !tcx.is_inline_enum_ty(ty) =>
            {
                match tcx.adt_field_tys(*def, substs) {
                    Some(fields) => fields.to_vec(),
                    None => return,
                }
            }
            TyKind::Tuple(elems) => elems.clone(),
            TyKind::Array { elem, len } => vec![*elem; len.to_usize()],
            _ => return,
        };
        for (i, t) in field_tys.iter().enumerate() {
            let idx = u32::try_from(i).unwrap_or(0);
            if let Some(kind) = field_kind(tcx, *t) {
                prefix.push(idx);
                out.push((prefix.clone(), kind));
                prefix.pop();
            } else if recursable(tcx, *t) {
                prefix.push(idx);
                walk(tcx, *t, prefix, out);
                prefix.pop();
            }
        }
    }
    let mut out = Vec::new();
    let mut prefix = Vec::new();
    walk(tcx, ty, &mut prefix, &mut out);
    out
}

/// Forward-propagates a concrete local type through `B = Copy(A)` chains: when
/// `A` has a resolved type but `B` was left an inference variable, `B` takes
/// `A`'s type. A fixpoint, so chains (`A -> B -> C`) settle fully. Run before
/// the RC passes so a `?` / `unwrap` extraction (typed from the scrutinee's
/// substs) copied into an otherwise-`Var` binding is recognised as RC-managed
/// and released - without it the extracted `String` leaks.
pub(crate) fn propagate_copy_types(body: &mut Body, tcx: &gossamer_types::TyCtxt) {
    use gossamer_types::TyKind;
    let n = body.locals.len();
    let unresolved = |ty| matches!(tcx.kind_of(ty), TyKind::Var(_) | TyKind::Error);
    // Type of `base.Field(idx)`, seeing through one `&`. Used to flow a
    // resolved aggregate's field type onto an otherwise-`Var` destination -
    // e.g. `inner = Copy(a.Field(0))` once `a` is known to be a struct.
    let field_ty = |base_ty: gossamer_types::Ty, idx: u32| -> Option<gossamer_types::Ty> {
        let mut t = base_ty;
        if let TyKind::Ref { inner, .. } = tcx.kind_of(t) {
            t = *inner;
        }
        match tcx.kind_of(t) {
            TyKind::Adt { def, substs } => tcx
                .adt_field_tys(*def, substs)
                .and_then(|tys| tys.get(idx as usize).copied()),
            TyKind::Tuple(elems) => elems.get(idx as usize).copied(),
            TyKind::Array { elem, len } if (idx as usize) < len.to_usize() => Some(*elem),
            _ => None,
        }
    };
    let mut changed = true;
    while changed {
        changed = false;
        let updates: Vec<(usize, gossamer_types::Ty)> = body
            .blocks
            .iter()
            .flat_map(|b| &b.stmts)
            .filter_map(|stmt| {
                let StatementKind::Assign {
                    place,
                    rvalue: Rvalue::Use(Operand::Copy(p)),
                } = &stmt.kind
                else {
                    return None;
                };
                if !place.projection.is_empty()
                    || (place.local.0 as usize) >= n
                    || (p.local.0 as usize) >= n
                    || !unresolved(body.locals[place.local.0 as usize].ty)
                {
                    return None;
                }
                let src_ty = body.locals[p.local.0 as usize].ty;
                if unresolved(src_ty) {
                    return None;
                }
                // Bare copy: destination inherits the source type directly.
                if p.projection.is_empty() {
                    return Some((place.local.0 as usize, src_ty));
                }
                // Single field projection: destination inherits the field type,
                // so a chain `a.inner.tag` resolves one level per fixpoint pass.
                if let [crate::ir::Projection::Field(idx)] = p.projection.as_slice() {
                    if let Some(ft) = field_ty(src_ty, *idx) {
                        if !unresolved(ft) {
                            return Some((place.local.0 as usize, ft));
                        }
                    }
                }
                None
            })
            .collect();
        for (d, ty) in updates {
            if unresolved(body.locals[d].ty) {
                body.locals[d].ty = ty;
                changed = true;
            }
        }
    }
}

/// Whether the call `name` answering into `dest` hands the frame a counted
/// aggregate blob: a runtime call that always does, or a container read whose
/// element is an addressed aggregate, which answers a blob of its words.
fn answers_counted_blob(
    name: &str,
    dest: Local,
    body: &Body,
    tcx: &gossamer_types::TyCtxt,
) -> bool {
    use gossamer_types::TyKind;
    if gossamer_abi::answers_counted_payload(name) {
        return true;
    }
    if !gossamer_abi::answers_counted_element(name) {
        return false;
    }
    let Some(local) = body.locals.get(dest.0 as usize) else {
        return false;
    };
    match tcx.kind_of(local.ty) {
        TyKind::Adt { def, substs } if def.local == u32::MAX || def.local == u32::MAX - 1 => substs
            .types()
            .first()
            .is_some_and(|elem| tcx.elem_is_addressed_aggregate(*elem)),
        _ => false,
    }
}

/// Runtime calls that answer one of the sequence's OWN elements, wrapped in a
/// carrier: the payload is the container's value, not a fresh one.
///
/// The combinator each shim implements is what the ABI registry declares, so
/// the set is derived rather than spelled: `max`, `min`, their `_by` and
/// `_by_key` forms, `find`, and `reduce` each hand back an element the
/// sequence still holds, exactly as `first` / `last` / an index read do.
fn answers_borrowed_element(name: &str) -> bool {
    if matches!(
        name,
        "gos_rt_vec_get_i128"
            | "gos_rt_vec_first"
            | "gos_rt_vec_last"
            // The upgrade's fresh strong reference is pinned by the shadow
            // local `gos_rt_weak_opt_payload` answers, so the carrier's
            // payload is a view of what that handle already owns.
            | "gos_rt_rc_weak_upgrade_opt"
    ) {
        return true;
    }
    gossamer_abi::combinator_abi_of(name).is_some_and(|abi| {
        matches!(
            abi.combinator,
            "max" | "min" | "max_by" | "min_by" | "max_by_key" | "min_by_key" | "find" | "reduce"
        )
    })
}

/// `Vec` locals that hold a counted share of every value they are given.
///
/// A local built by a call (`let mut v = #[]`) and later rebound by a bare copy
/// (`v = f()?`, where the extraction lands in a temporary first) holds values
/// with two kinds of owner: the call's result is the local's own, and a copied
/// value is its source's. Which one it holds at a later rebinding depends on
/// the path taken, so no single owner can give the replaced value back. Such a
/// local takes a share of each value copied into it instead, and releases
/// whatever it holds before every rebinding and at every return, the way a
/// `String` binding does; the source keeps its own share.
///
/// Only a local whose value is never copied on into another local qualifies:
/// that copy would take no share, and would dangle once the local rebinds.
pub(crate) fn rebound_vec_owners(body: &Body, tcx: &gossamer_types::TyCtxt) -> Vec<bool> {
    use gossamer_types::TyKind;
    let n = body.locals.len();
    let arity = body.arity as usize;
    let is_vec = |i: usize| {
        i < n
            && matches!(
                tcx.kind_of(body.locals[i].ty),
                TyKind::Vec(_) | TyKind::Slice(_)
            )
    };
    let mut call_defs = vec![0u32; n];
    let mut copy_defs = vec![0u32; n];
    let mut disqualified = vec![false; n];
    for block in &body.blocks {
        for stmt in &block.stmts {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            if place.projection.is_empty() && (place.local.0 as usize) < n {
                let d = place.local.0 as usize;
                match rvalue {
                    Rvalue::Use(Operand::Const(ConstValue::Int(0))) => {}
                    Rvalue::Use(Operand::Copy(src))
                        if src.projection.is_empty() && src.local != place.local =>
                    {
                        if is_vec(src.local.0 as usize) {
                            copy_defs[d] += 1;
                        } else {
                            disqualified[d] = true;
                        }
                    }
                    _ => disqualified[d] = true,
                }
            }
            // A bare copy of the local into another local names its value
            // without a share of its own.
            if let Rvalue::Use(Operand::Copy(src)) = rvalue
                && src.projection.is_empty()
                && (src.local.0 as usize) < n
                && place.local != Local::RETURN
            {
                disqualified[src.local.0 as usize] = true;
            }
        }
        if let Terminator::Call {
            callee,
            destination,
            ..
        } = &block.terminator
            && (destination.local.0 as usize) < n
        {
            let d = destination.local.0 as usize;
            let borrowed = matches!(
                callee,
                Operand::Const(ConstValue::Str(name))
                    if returns_borrowed_pointer(name.as_str())
            );
            if !destination.projection.is_empty() || borrowed {
                disqualified[d] = true;
            } else {
                call_defs[d] += 1;
            }
        }
    }
    (0..n)
        .map(|i| {
            i > arity
                && is_vec(i)
                && !body.locals[i].region
                && !disqualified[i]
                && call_defs[i] > 0
                && copy_defs[i] > 0
        })
        .collect()
}

pub(crate) fn insert_rc_releases(body: &mut Body, tcx: &gossamer_types::TyCtxt) {
    let n_locals = body.locals.len();
    if n_locals == 0 {
        return;
    }
    let arity = body.arity as usize;

    // An RC-managed local that is neither the return slot (0) nor a
    // parameter (1..=arity). `i > arity` excludes both.
    // Region-owned locals are excluded everywhere: their values are freed
    // wholesale at `arena_pop`, so emitting a retain/release would touch
    // freed memory after the pop.
    let is_rc = |i: usize| {
        i > arity && i < n_locals && tcx.is_rc_managed(body.locals[i].ty) && !body.locals[i].region
    };
    let rc_operand = |op: &Operand| -> Option<Local> {
        if let Operand::Copy(p) = op
            && p.projection.is_empty()
            && (p.local.0 as usize) < n_locals
            && tcx.is_rc_managed(body.locals[p.local.0 as usize].ty)
            && !body.locals[p.local.0 as usize].region
        {
            Some(p.local)
        } else {
            None
        }
    };
    // A bare `Vec`/`[T]` operand is not RC-managed, but its buffer has an
    // independent reference count. Every container insertion therefore mints
    // the container's share before the call; the source's ordinary cleanup
    // remains in place. This explicit two-owner state handles overwrite,
    // removal, and every early exit without relying on a leak-prone drop
    // suppression pass.
    let vec_operand = |op: &Operand| -> Option<Local> {
        if let Operand::Copy(p) = op
            && p.projection.is_empty()
            && (p.local.0 as usize) < n_locals
            && matches!(
                tcx.kind_of(body.locals[p.local.0 as usize].ty),
                gossamer_types::TyKind::Vec(_) | gossamer_types::TyKind::Slice(_)
            )
            && !body.locals[p.local.0 as usize].region
        {
            Some(p.local)
        } else {
            None
        }
    };
    // RC-managed field slots of a by-value aggregate (struct / tuple), as
    // (field_index, is_weak). In the LLVM backend such aggregates are stack
    // slots with no heap teardown, so the RC fields they retain at
    // construction/copy must be released when the local dies.
    let agg_rc_fields = |ty: Ty| -> AggFieldPaths { aggregate_rc_field_paths(tcx, ty) };
    // (No early-out on `is_rc` locals alone: a function may only copy a
    // borrowed RC *parameter* into its return slot - e.g. `fn id(t: Tree)
    // -> Tree { t }` - which still needs a return-copy retain. The
    // empty-work check after collecting retain/release sites handles the
    // genuine no-op case.)

    // Retain sites within statement sequences: `(block, stmt_idx,
    // local, count)` - insert `count` retains of `local` just after the
    // statement. Collected first, applied after the release edits so
    // statement indices stay valid.
    // Self-accumulation copy-backs from the in-place string builder:
    // `tmp = gos_rt_str_concat_drop_a(s, frag)` (a block's Call terminator)
    // whose result is copied straight back - `s = Copy(tmp)` as the first
    // statement of the successor block. `concat_drop_a` consumes `s`'s old
    // buffer (appends in place, or reallocates and frees it) and returns the
    // new one, so this copy-back is a move that *replaces* `s`: it must NOT
    // retain `tmp` (that would drive the reused buffer's count above 1 and
    // force every append onto the copy-on-write path - O(n^2)) and must NOT
    // release the old `s` (already owned/freed by the call - double-free).
    // The `(succ_block, 0)` of each such copy-back is recorded here.
    let mut copyback_sites: std::collections::HashSet<(usize, usize)> =
        std::collections::HashSet::new();
    for block in &body.blocks {
        if let Terminator::Call {
            callee,
            args,
            destination,
            target: Some(succ),
        } = &block.terminator
            && matches!(callee, Operand::Const(ConstValue::Str(n)) if is_self_consuming_append(n))
            && destination.projection.is_empty()
            && let Some(Operand::Copy(arg0)) = args.first()
        {
            // The self-consuming append accumulator is `arg0` - a bare local
            // (`acc`) or a `&mut String` deref place (`*s`). The copy-back
            // stores `tmp` straight back into that same place; recognising it
            // (by matching both the local AND the projection) keeps the
            // accumulator off the retain-of-result / release-of-old paths.
            let (tmp, succ_idx) = (destination.local, succ.0 as usize);
            if succ_idx >= body.blocks.len() {
                continue;
            }
            // The accumulator arrives either as the call's own answer or as
            // the payload of the carrier it answers, so the copy-back is the
            // successor's first statement or the one after the extraction.
            let stmts = &body.blocks[succ_idx].stmts;
            let (source, copy_at) = match stmts.first().map(|s| &s.kind) {
                Some(StatementKind::Assign {
                    place,
                    rvalue: Rvalue::CallIntrinsic { name, args },
                }) if *name == "gos_rt_result_payload"
                    && place.projection.is_empty()
                    && matches!(args.first(), Some(Operand::Copy(c)) if c.local == tmp
                        && c.projection.is_empty()) =>
                {
                    (place.local, 1)
                }
                _ => (tmp, 0),
            };
            if let Some(stmt) = stmts.get(copy_at)
                && let StatementKind::Assign {
                    place,
                    rvalue: Rvalue::Use(Operand::Copy(src)),
                } = &stmt.kind
                && place.local == arg0.local
                && place.projection == arg0.projection
                && src.local == source
                && src.projection.is_empty()
            {
                copyback_sites.insert((succ_idx, copy_at));
            }
        }
    }
    // Post-call reload of a `&mut String` writeback (`L = *R`, where `R` is the
    // `&mut String` ref produced for the call): a copy-back, not a fresh
    // reassignment. The callee already released the value `L` previously held
    // (its `*R = …` displaced it through the slot), so the release-before-
    // reassignment that would otherwise fire for `L` must be suppressed - else
    // it double-frees. The reload itself takes no retain (its source is a
    // borrowed deref), so adding it here only cancels the spurious release.
    for (bi, block) in body.blocks.iter().enumerate() {
        for (si, stmt) in block.stmts.iter().enumerate() {
            if let StatementKind::Assign {
                place,
                rvalue: Rvalue::Use(Operand::Copy(src)),
            } = &stmt.kind
                && place.projection.is_empty()
                && (src.local.0 as usize) < n_locals
                && matches!(src.projection.as_slice(), [crate::ir::Projection::Deref])
                && matches!(
                    tcx.kind_of(body.locals[src.local.0 as usize].ty),
                    gossamer_types::TyKind::Ref { inner, .. }
                        if matches!(tcx.kind_of(*inner), gossamer_types::TyKind::String)
                            || tcx.is_payload_enum(*inner)
                )
            {
                copyback_sites.insert((bi, si));
            }
        }
    }

    // Locals this body already releases. `insert_drops_at_returns` runs before
    // this pass, so its frees are the record of which locals the frame still
    // owns a share of.
    let vec_released: std::collections::HashSet<u32> = body
        .blocks
        .iter()
        .flat_map(|b| b.stmts.iter())
        .filter_map(|stmt| {
            let StatementKind::Assign {
                rvalue: Rvalue::CallIntrinsic { name, args },
                ..
            } = &stmt.kind
            else {
                return None;
            };
            if *name != "gos_rt_vec_free" {
                return None;
            }
            match args.first() {
                Some(Operand::Copy(p)) if p.projection.is_empty() => Some(p.local.0),
                _ => None,
            }
        })
        .collect();
    let mut retain_sites: Vec<(usize, usize, Local, usize)> = Vec::new();
    // Extraction retains kept because the binding they feed is stored into an
    // aggregate, as `(block, stmt, payload, binding)`.
    let mut stored_extraction_retains: Vec<(usize, usize, Local, Local)> = Vec::new();
    // `gos_rt_result_new` payload mints this pass may still owe, decided once
    // the release schedule is known.
    let mut carrier_vec_sites: Vec<(usize, usize, Local)> = Vec::new();
    // Retains to emit at the end of a block (just before a consuming
    // terminator call), `(block, local)`.
    let mut terminator_retains: Vec<(usize, Local)> = Vec::new();
    // The same, for a by-value aggregate argument: a struct's heap fields are
    // counted per binding rather than through the aggregate, and the
    // aggregate the container stores names no children of its own, so the
    // container's share is one retain per RC field.
    let mut terminator_field_retains: Vec<(usize, Local, Vec<u32>, FieldRcKind)> = Vec::new();
    // By-value enum locals loaded from a container slot the CONTAINER
    // still owns (`row[i]` via `gos_rt_vec_get_i128`, `xs.first()`,
    // `xs.last()`): their payload word is an interior borrow of the
    // vec's element, not the transferred single reference a consumed
    // `Result` hands to `?` / `unwrap()`. A `String` payload extracted
    // from one of these must RETAIN (the binding takes its own share;
    // the vec's `elem_kind` deep-free keeps the vec's), never move -
    // moving released the vec's only share and the deep-free at
    // `gos_rt_vec_free` then double-freed it.
    let mut borrowed_enum_src = vec![false; n_locals];
    for block in &body.blocks {
        if let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            destination,
            ..
        } = &block.terminator
            && destination.projection.is_empty()
            && (destination.local.0 as usize) < n_locals
            && answers_borrowed_element(name.as_str())
        {
            borrowed_enum_src[destination.local.0 as usize] = true;
        }
    }
    // A carrier read straight out of a container or aggregate slot - the
    // `defs[i]` of a by-value `[Option<Vec<i64>>; N]`, a struct's carrier
    // field - is the same borrow those helpers answer, but a fixed array and
    // a by-value aggregate are indexed IN PLACE, so the read is a projected
    // copy rather than a call and nothing above sees it. The payload
    // extracted from such a carrier belongs to the container, whose own
    // per-element release gives it back; classifying it as owned frees it
    // under every other holder.
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign {
                place,
                rvalue: Rvalue::Use(Operand::Copy(src)),
            } = &stmt.kind
                && place.projection.is_empty()
                && !src.projection.is_empty()
                && (place.local.0 as usize) < n_locals
                && src.projection.iter().all(|p| {
                    matches!(
                        p,
                        crate::ir::Projection::Field(_) | crate::ir::Projection::Index(_)
                    )
                })
            {
                borrowed_enum_src[place.local.0 as usize] = true;
            }
        }
    }
    // Propagate forward through plain copies (`let opt = row[0]` then
    // matching on a scrutinee temp copied from `opt`).
    {
        let copy_edges: Vec<(usize, usize)> = body
            .blocks
            .iter()
            .flat_map(|b| &b.stmts)
            .filter_map(|stmt| {
                if let StatementKind::Assign {
                    place,
                    rvalue: Rvalue::Use(Operand::Copy(p)),
                } = &stmt.kind
                    && place.projection.is_empty()
                    && p.projection.is_empty()
                    && (place.local.0 as usize) < n_locals
                    && (p.local.0 as usize) < n_locals
                {
                    Some((place.local.0 as usize, p.local.0 as usize))
                } else {
                    None
                }
            })
            .collect();
        let mut changed = true;
        while changed {
            changed = false;
            for &(dest, src) in &copy_edges {
                if borrowed_enum_src[src] && !borrowed_enum_src[dest] {
                    borrowed_enum_src[dest] = true;
                    changed = true;
                }
            }
        }
    }
    // A carrier the payload walk emptied right after an extraction held a
    // share of its own (`own_carrier_payloads` took it where the carrier was
    // read out of its slot), and the extraction handed that share over. Its
    // payload is the binding's, not a borrow of the slot.
    for block in &body.blocks {
        for pair in block.stmts.windows(2) {
            if let [
                Statement {
                    kind:
                        StatementKind::Assign {
                            rvalue: Rvalue::CallIntrinsic { name, args },
                            ..
                        },
                    ..
                },
                Statement {
                    kind:
                        StatementKind::Assign {
                            place: emptied,
                            rvalue:
                                Rvalue::CallIntrinsic {
                                    name: empty_name,
                                    args: empty_args,
                                },
                        },
                    ..
                },
            ] = pair
                && *name == "gos_rt_result_payload"
                && *empty_name == "gos_rt_result_new"
                && matches!(
                    empty_args.as_slice(),
                    [
                        Operand::Const(ConstValue::Int(1)),
                        Operand::Const(ConstValue::Int(0))
                    ]
                )
                && emptied.projection.is_empty()
                && matches!(args.first(), Some(Operand::Copy(c))
                    if c.projection.is_empty() && c.local == emptied.local)
                && (emptied.local.0 as usize) < n_locals
            {
                borrowed_enum_src[emptied.local.0 as usize] = false;
            }
        }
    }
    let enum_arg_is_borrowed = |args: &[Operand]| -> bool {
        matches!(
            args.first(),
            Some(Operand::Copy(p))
                if p.projection.is_empty()
                    && (p.local.0 as usize) < n_locals
                    && borrowed_enum_src[p.local.0 as usize]
        )
    };
    // Whether the carrier an extraction reads is one this frame built rather
    // than one it was handed.
    //
    // A carrier PARAMETER is the caller's value - the frame receives the two
    // words, not a share of what the payload points at - so the payload is a
    // borrow and stays the caller's to release. Only a carrier the frame
    // itself constructed hands its payload over.
    let carrier_is_frame_owned = |args: &[Operand]| -> bool {
        matches!(
            args.first(),
            Some(Operand::Copy(p))
                if p.projection.is_empty() && (p.local.0 as usize) > arity
        )
    };
    // Word-slot element loads out of a vec the CONTAINER still owns: the
    // `gos_rt_vec_get_i64` destination of a for-loop / index read. When the
    // loop lowering's element-type pin did not reach (`for s in
    // strings::split(...)` - a free-call iter expression), the destination
    // local is typed i64 and `rc_operand` cannot see the String underneath,
    // so the copy into a String-typed binding neither mints a share nor
    // schedules a release. The binding's release (or the caller's, when the
    // value is returned) then collides with the vec's `elem_kind` deep-free
    // - a double free. The retain/owned arms below mint a share for any
    // String-typed binding copied from one of these destinations, mirroring
    // the `borrowed_enum_src` extraction contract.
    let mut borrowed_word_elem_src = vec![false; n_locals];
    for block in &body.blocks {
        if let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            destination,
            ..
        } = &block.terminator
            && destination.projection.is_empty()
            && (destination.local.0 as usize) < n_locals
            && matches!(
                name.as_str(),
                "gos_rt_vec_get_i64" | "gos_rt_vec_get_i64_unchecked"
            )
        {
            borrowed_word_elem_src[destination.local.0 as usize] = true;
        }
    }
    let is_string_local = |l: Local| -> bool {
        (l.0 as usize) < n_locals
            && matches!(
                tcx.kind_of(body.locals[l.0 as usize].ty),
                gossamer_types::TyKind::String
            )
    };
    // Locals holding a `String` payload freshly extracted from a consumed
    // by-value `Result`/`Option` (`f()?`, `r.unwrap()`). The extraction yields
    // the single owning reference the enum held, so copying it into the binding
    // (`let s = f()?`) must MOVE rather than retain - a retain there leaves the
    // extracted reference dangling once the binding is released (a leak).
    // Restricted to `String` payloads: an aggregate (`Adt`) payload carries
    // nested-RC fields whose release is balanced by the copy retain, so moving
    // it would double-free (the `from_json -> Config` path).
    let mut extraction_results = vec![false; n_locals];
    // Locals whose every whole-local assignment is a CONSTANT (the
    // tagged-null unit-variant representation, null-outs): such values
    // are immortal-by-construction - retaining/releasing them is a
    // guaranteed runtime no-op, so skip emitting the calls at all.
    let mut saw_const_assign = vec![false; n_locals];
    let mut saw_other_assign = vec![false; n_locals];
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign { place, rvalue } = &stmt.kind
                && place.projection.is_empty()
                && (place.local.0 as usize) < n_locals
            {
                // Only INTEGER constants qualify: the tagged-null
                // unit-variant representation and null-outs. A string
                // literal is a real heap-shaped value whose holders
                // retain it - eliding those desynchronizes the
                // accounting.
                if matches!(rvalue, Rvalue::Use(Operand::Const(ConstValue::Int(_)))) {
                    saw_const_assign[place.local.0 as usize] = true;
                } else {
                    saw_other_assign[place.local.0 as usize] = true;
                }
            }
        }
        if let Terminator::Call { destination, .. } = &block.terminator
            && destination.projection.is_empty()
            && (destination.local.0 as usize) < n_locals
        {
            saw_other_assign[destination.local.0 as usize] = true;
        }
    }
    // Parameters and the return slot receive their values from the
    // caller - never const-only, regardless of body-local assignments.
    let const_init_only: Vec<bool> = (0..n_locals)
        .map(|i| i > body.arity as usize && saw_const_assign[i] && !saw_other_assign[i])
        .collect();

    // Locals stored into a heap aggregate (`gos_store` value argument). The
    // store gives the aggregate a reference; the copy that fed the store is
    // therefore load-bearing (extract -> retain -> store keeps one, the binding
    // release drops back to one). Such a binding must NOT have its retain
    // skipped, or the aggregate's later release double-frees (the synthesized
    // `from_json` parses a field String and stores it into the struct).
    let mut stored_into_aggregate = vec![false; n_locals];
    {
        use gossamer_types::TyKind;
        // A reference-counted payload - a `String`, a payload-bearing enum
        // node, a closure - or one left unresolved as `Var` (the nested `?` in
        // a function whose own return type doesn't pin the Ok type, so
        // inference never settles the extraction local). The carrier is two
        // words by value and releases nothing, so its share is the one the
        // extraction hands over whatever the payload's shape; a payload that
        // goes on into an aggregate keeps its retain through the transitive
        // `stored_into_aggregate` gate.
        let is_rc_payload = |l: Local| {
            (l.0 as usize) < n_locals
                && (tcx.is_rc_managed(body.locals[l.0 as usize].ty)
                    || matches!(tcx.kind_of(body.locals[l.0 as usize].ty), TyKind::Var(_)))
        };
        let mark_stored = |op: &Operand, set: &mut [bool]| {
            if let Operand::Copy(p) = op
                && p.projection.is_empty()
                && (p.local.0 as usize) < n_locals
            {
                set[p.local.0 as usize] = true;
            }
        };
        // A carrier copied to or from another binding keeps its payload share
        // through an extraction (`own_carrier_payloads` gives it back), so
        // reading the payload out of one hands nothing over.
        let carrier_aliases = bare_copies(body);
        let carrier_is_aliased = |args: &[Operand]| {
            matches!(args.first(), Some(Operand::Copy(p))
                if p.projection.is_empty()
                    && (p.local.0 as usize) < n_locals
                    && (carrier_aliases.sourced[p.local.0 as usize]
                        || carrier_aliases.target[p.local.0 as usize]))
        };
        for block in &body.blocks {
            for stmt in &block.stmts {
                let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                    continue;
                };
                match rvalue {
                    Rvalue::CallIntrinsic { name, args } => {
                        // Borrowed-slot extractions are NOT moves - see
                        // `borrowed_enum_src`.
                        if *name == "gos_rt_result_payload"
                            && place.projection.is_empty()
                            && is_rc_payload(place.local)
                            && !enum_arg_is_borrowed(args)
                            && carrier_is_frame_owned(args)
                            && !carrier_is_aliased(args)
                        {
                            extraction_results[place.local.0 as usize] = true;
                        }
                        // `gos_store` (object field write) and `gos_rt_result_new`
                        // (`Ok`/`Some` payload) both take ownership of the value
                        // argument; the copy feeding them keeps its retain.
                        let stored_args: &[Operand] = if *name == "gos_store" {
                            args.get(2).map(std::slice::from_ref).unwrap_or(&[])
                        } else if *name == "gos_rt_result_new" {
                            args
                        } else {
                            &[]
                        };
                        for op in stored_args {
                            mark_stored(op, &mut stored_into_aggregate);
                        }
                    }
                    // Struct / tuple / enum construction owns each operand.
                    Rvalue::Aggregate { operands, .. } => {
                        for op in operands {
                            mark_stored(op, &mut stored_into_aggregate);
                        }
                    }
                    _ => {}
                }
            }
            if let Terminator::Call {
                callee: Operand::Const(ConstValue::Str(name)),
                destination,
                args,
                ..
            } = &block.terminator
                && matches!(
                    name.as_str(),
                    "gos_rt_option_unwrap" | "gos_rt_result_unwrap"
                )
                && destination.projection.is_empty()
                && is_rc_payload(destination.local)
                && !enum_arg_is_borrowed(args)
            {
                extraction_results[destination.local.0 as usize] = true;
            }
        }
        // Propagate "stored" backward through `dest = Copy(src)` edges: a value
        // copied into a binding that is itself stored is also (transitively)
        // stored, so its own copy retain is load-bearing. This catches the
        // multi-hop flow the synthesized `from_json` uses (parse a field String,
        // copy it through temporaries, then place it in the result struct).
        let copy_edges: Vec<(usize, usize)> = body
            .blocks
            .iter()
            .flat_map(|b| &b.stmts)
            .filter_map(|stmt| {
                if let StatementKind::Assign {
                    place,
                    rvalue: Rvalue::Use(Operand::Copy(p)),
                } = &stmt.kind
                    && place.projection.is_empty()
                    && p.projection.is_empty()
                    && (place.local.0 as usize) < n_locals
                    && (p.local.0 as usize) < n_locals
                {
                    Some((place.local.0 as usize, p.local.0 as usize))
                } else {
                    None
                }
            })
            .collect();
        let mut changed = true;
        while changed {
            changed = false;
            for &(dest, src) in &copy_edges {
                if stored_into_aggregate[dest] && !stored_into_aggregate[src] {
                    stored_into_aggregate[src] = true;
                    changed = true;
                }
            }
        }
    }
    // Locals that VIEW a child slot of an enum node the frame does not own:
    // the node is a parameter, so the caller holds it for the whole call and
    // nothing here reassigns or releases it. Every use of the view is inside
    // that lifetime, so the binder needs no share of its own - which is what
    // a traversal written against a reference got for free. Minting one
    // anyway costs a retain/release pair per node, and the release decrements
    // a live node to a non-zero count, which is the cycle collector's
    // definition of a candidate root: a read-only walk of an acyclic tree
    // would fill the candidate buffer with the whole tree.
    let enum_child_borrow = {
        let mut stable_param = vec![false; n_locals];
        for i in 1..=(body.arity as usize).min(n_locals.saturating_sub(1)) {
            stable_param[i] = true;
        }
        let mut view = vec![false; n_locals];
        let mut disqualified = vec![false; n_locals];
        let mut copy_edges: Vec<(usize, usize)> = Vec::new();
        for block in &body.blocks {
            for stmt in &block.stmts {
                let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                    continue;
                };
                // A write through a projection reaches the pointee's storage,
                // so the base is more than a view of it.
                if !place.projection.is_empty() {
                    if (place.local.0 as usize) < n_locals {
                        disqualified[place.local.0 as usize] = true;
                    }
                    continue;
                }
                if (place.local.0 as usize) >= n_locals {
                    continue;
                }
                let i = place.local.0 as usize;
                // A parameter the body writes to is no longer the caller's
                // value for the rest of the frame.
                stable_param[i] = false;
                match rvalue {
                    // A boxed aggregate payload is copied out of its box with
                    // a share of each counted field, so its binding is no view.
                    Rvalue::CallIntrinsic { name, .. }
                        if *name == "gos_enum_load"
                            && tcx.is_boxed_payload_binding(body.locals[i].ty) =>
                    {
                        disqualified[i] = true;
                    }
                    Rvalue::CallIntrinsic { name, args }
                        if matches!(*name, "gos_enum_load" | "gos_enum_slot_ptr") =>
                    {
                        match args.first() {
                            Some(Operand::Copy(src)) if src.projection.is_empty() => {
                                copy_edges.push((i, src.local.0 as usize));
                            }
                            _ => disqualified[i] = true,
                        }
                    }
                    Rvalue::Use(Operand::Copy(src)) if src.projection.is_empty() => {
                        copy_edges.push((i, src.local.0 as usize));
                    }
                    _ => disqualified[i] = true,
                }
            }
            if let Terminator::Call { destination, .. } = &block.terminator
                && (destination.local.0 as usize) < n_locals
            {
                disqualified[destination.local.0 as usize] = true;
                stable_param[destination.local.0 as usize] = false;
            }
        }
        // A view that is stored into an aggregate, or handed to the caller,
        // outlives the node it came from and owns its own share.
        for block in &body.blocks {
            for stmt in &block.stmts {
                if let StatementKind::Assign { place, rvalue } = &stmt.kind
                    && place.local == Local::RETURN
                    && let Rvalue::Use(Operand::Copy(src)) = rvalue
                    && src.projection.is_empty()
                    && (src.local.0 as usize) < n_locals
                {
                    disqualified[src.local.0 as usize] = true;
                }
            }
        }
        for i in 0..n_locals {
            if stored_into_aggregate[i] {
                disqualified[i] = true;
            }
        }
        // Seed from every `gos_enum_load` off a still-stable parameter, then
        // let the view travel the copy edges that carry it.
        let mut changed = true;
        while changed {
            changed = false;
            for &(dest, src) in &copy_edges {
                if disqualified[dest] || view[dest] {
                    continue;
                }
                if (stable_param[src] || view[src]) && src != dest {
                    view[dest] = true;
                    changed = true;
                }
            }
        }
        for i in 0..n_locals {
            view[i] = view[i] && !disqualified[i] && i > body.arity as usize;
        }
        view
    };

    // Copies out of an extraction result that took no retain because they
    // move the value: `(block, statement, source)`. Move elision decides
    // whether each really is a move, the way it does for every other copy.
    let mut extraction_moves: Vec<(usize, usize, Local)> = Vec::new();
    for (block_idx, block) in body.blocks.iter().enumerate() {
        for (stmt_idx, stmt) in block.stmts.iter().enumerate() {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            match rvalue {
                // New binding/alias to an RC value (covers `RETURN =
                // Copy(x)`, which mints the caller's reference).
                Rvalue::Use(op) => {
                    // Skip the retain only for a by-value enum-payload extraction
                    // moved into a binding that is NOT itself stored into an
                    // aggregate. A stored binding keeps the retain (the store
                    // consumes one reference, the binding release drops the
                    // other) - see `stored_into_aggregate`.
                    let skip_extraction_move = rc_operand(op).is_some_and(|l| {
                        extraction_results[l.0 as usize]
                            && !(place.projection.is_empty()
                                && stored_into_aggregate[place.local.0 as usize])
                    });
                    // A bare copy into a reference local is an alias and
                    // mints nothing. A projected store through one reaches
                    // the pointee's own storage - a field of the borrowed
                    // aggregate - so the aggregate gains the share the way a
                    // field store on an owned local does.
                    let ref_alias = place.projection.is_empty()
                        && matches!(
                            tcx.kind_of(body.locals[place.local.0 as usize].ty),
                            gossamer_types::TyKind::Ref { .. }
                        );
                    // A view of a node the caller holds needs no share; see
                    // `enum_child_borrow`.
                    let borrowed_view =
                        place.projection.is_empty() && enum_child_borrow[place.local.0 as usize];
                    if skip_extraction_move
                        && let Some(l) = rc_operand(op)
                        && !copyback_sites.contains(&(block_idx, stmt_idx))
                        && !ref_alias
                        && !borrowed_view
                    {
                        extraction_moves.push((block_idx, stmt_idx, l));
                    }
                    if let Some(l) = rc_operand(op)
                        && !copyback_sites.contains(&(block_idx, stmt_idx))
                        && !skip_extraction_move
                        && !ref_alias
                        && !borrowed_view
                    {
                        retain_sites.push((block_idx, stmt_idx, l, 1));
                        // The binding this extraction feeds keeps the share
                        // only while it has a release to give it back with.
                        // Whether it does is settled once move elision has
                        // run, so the pairing is checked there.
                        if extraction_results[l.0 as usize]
                            && place.projection.is_empty()
                            && stored_into_aggregate[place.local.0 as usize]
                        {
                            stored_extraction_retains.push((block_idx, stmt_idx, l, place.local));
                        }
                    }
                    // A String-typed binding copied from an untyped borrowed
                    // word-slot element (see `borrowed_word_elem_src`): mint
                    // the binding's share here - the vec's deep-free keeps
                    // the container's. `rc_operand` is None for these (the
                    // source local is typed i64), so this never doubles the
                    // retain above.
                    if rc_operand(op).is_none()
                        && let Operand::Copy(p) = op
                        && p.projection.is_empty()
                        && (p.local.0 as usize) < n_locals
                        && borrowed_word_elem_src[p.local.0 as usize]
                        && place.projection.is_empty()
                        && is_string_local(place.local)
                        && !copyback_sites.contains(&(block_idx, stmt_idx))
                    {
                        retain_sites.push((block_idx, stmt_idx, p.local, 1));
                    }
                    // A deref-load of an RC-pointee `&`/`&mut` param straight
                    // into the return slot (`fn take(s: &mut String) -> String
                    // { *s }` lowers to `_0 = Copy(_1)` with `_1: &mut String`,
                    // the deref folded into a bare copy by type coercion). The
                    // load mints the caller's reference to the pointee, but
                    // `rc_operand` is None here (the source local is a `Ref`,
                    // not RC-managed), so retain the value now in the return
                    // slot - matching the reference the caller receives and
                    // releases. Gated on the return slot itself so a deref-load
                    // into an ordinary local (a borrow, or copied onward into
                    // the return where the onward copy already retains) is left
                    // alone. Excludes the `[Deref]` writeback-reload shape,
                    // which the writeback recognizer routes through
                    // `copyback_sites`.
                    if rc_operand(op).is_none()
                        && place.local == Local::RETURN
                        && place.projection.is_empty()
                        && (place.local.0 as usize) < n_locals
                        && tcx.is_rc_managed(body.locals[place.local.0 as usize].ty)
                        && let Operand::Copy(src) = op
                        && src.projection.is_empty()
                        && (src.local.0 as usize) < n_locals
                        && matches!(
                            tcx.kind_of(body.locals[src.local.0 as usize].ty),
                            gossamer_types::TyKind::Ref { inner, .. }
                                if tcx.is_rc_managed(*inner)
                        )
                        && !copyback_sites.contains(&(block_idx, stmt_idx))
                    {
                        retain_sites.push((block_idx, stmt_idx, place.local, 1));
                    }
                    // A vec-carried container parameter returned by value:
                    // the caller owns its argument temp AND books a free
                    // for every container-returning call (`inferred_free`),
                    // so the return must mint the caller's share. Covers
                    // `Vec` / `[T]` parameters and the const-generic
                    // `[T; N]` (Param-length arrays are coerced through
                    // `gos_rt_vec_from_arr` at every call site); a
                    // concrete-length array parameter is slot-copied
                    // inline and stays out.
                    if place.local == Local::RETURN
                        && place.projection.is_empty()
                        && let Operand::Copy(src) = op
                        && src.projection.is_empty()
                        && (1..=arity).contains(&(src.local.0 as usize))
                        && matches!(
                            tcx.kind_of(body.locals[src.local.0 as usize].ty),
                            gossamer_types::TyKind::Vec(_)
                                | gossamer_types::TyKind::Slice(_)
                                | gossamer_types::TyKind::Array {
                                    len: gossamer_types::ArrayLen::Param(_),
                                    ..
                                }
                        )
                        && !copyback_sites.contains(&(block_idx, stmt_idx))
                    {
                        retain_sites.push((block_idx, stmt_idx, src.local, 1));
                    }
                }
                // Storing an RC child into a heap object - the object
                // gains a reference (released via its type-meta on free).
                Rvalue::CallIntrinsic { name, args } if *name == "gos_store" => {
                    if let Some(l) = args.get(2).and_then(&rc_operand) {
                        retain_sites.push((block_idx, stmt_idx, l, 1));
                    }
                }
                // `dest = gos_enum_tag(src, disc)` is an IDENTITY alias of
                // the same allocation (the tag bits live in the pointer):
                // ownership-wise it is `dest = Copy(src)` - retain the
                // source (move elision transfers instead when this is its
                // only read).
                Rvalue::CallIntrinsic { name, args } if *name == "gos_enum_tag" => {
                    if let Some(l) = args.first().and_then(&rc_operand) {
                        retain_sites.push((block_idx, stmt_idx, l, 1));
                    }
                }
                // Wrapping a heap value into a `Result` or `Option`
                // (`Ok(v)` / `Err(v)` / `Some(v)`). The carrier takes the
                // reference out - it flows into the return, or `unwrap` /
                // `?` hands it to whoever takes the payload - so the
                // payload is acquired here. Without this, `Ok(J::Obj(ps))`
                // released the enum payload while the returned Result
                // still pointed at it, dropping a node from every
                // `self.parse()?`-built tree.
                //
                // A `Vec` payload counts the same way. It carries no RC
                // header, so it is reached through the vec allocator's own
                // count rather than `rc_operand` - and without its share,
                // `Some(v).unwrap()` left one reference with two owners,
                // the binding it came from and the binding it was
                // unwrapped into, so the second release freed storage the
                // first had already returned.
                Rvalue::CallIntrinsic { name, args } if *name == "gos_rt_result_new" => {
                    if let Some(l) = args.get(1).and_then(rc_operand) {
                        retain_sites.push((block_idx, stmt_idx, l, 1));
                    } else if let Some(l) = args.get(1).and_then(vec_operand) {
                        // The carrier's share is only the frame's to mint when
                        // the frame still holds one to balance it. A payload
                        // built for the carrier and handed to the caller with it
                        // has no release scheduled (the drop pass, which has
                        // already run, moved it into the return), so minting
                        // here would leave a share nothing returns.
                        if vec_released.contains(&l.0) {
                            retain_sites.push((block_idx, stmt_idx, l, 1));
                        } else {
                            // This pass schedules releases of its own, and a
                            // payload one of them covers needs the same mint.
                            // The set is only final once `releasable` is, so
                            // the site is settled after it below.
                            carrier_vec_sites.push((block_idx, stmt_idx, l));
                        }
                    }
                }
                // Aggregate fields / repeated elements - the
                // struct/tuple/array gains a reference per slot. Vec/Slice
                // operands count too: the owner's field-death free
                // (`FieldRcKind::Vec`) holds its own share, so the slot
                // must be minted here just like an RC slot.
                Rvalue::Aggregate { operands, .. } => {
                    for op in operands {
                        if let Some(l) = rc_operand(op).or_else(|| vec_operand(op)) {
                            retain_sites.push((block_idx, stmt_idx, l, 1));
                        }
                    }
                }
                Rvalue::Repeat { value, count } => {
                    if let Some(l) = rc_operand(value).or_else(|| vec_operand(value)) {
                        retain_sites.push((block_idx, stmt_idx, l, *count as usize));
                    }
                }
                _ => {}
            }
        }
        if let Terminator::Call { callee, args, .. } = &block.terminator
            && let Operand::Const(ConstValue::Str(name)) = callee
            && is_consuming_call(name)
        {
            // arg0 is the container/channel/closure RECEIVER (borrowed, mutated
            // in place) - only the value argument(s) (arg1..) are consumed and
            // gain a stored reference. Retaining the receiver too (now that it
            // is RC-managed) would over-retain it and leak it.
            for (arg_idx, arg) in args.iter().enumerate().skip(1) {
                // Vec elements pushed into a Vec are handled by the dedicated
                // `insert_drops_at_returns` block below. Letting this generic
                // consuming-call path retain them too leaves the inner Vec at
                // rc=1 after both the local and outer Vec are freed.
                if is_element_push(name) && arg_idx == 1 && vec_operand(arg).is_some() {
                    continue;
                }
                // A keyed container mints its own share of a stored `Vec`
                // value inside the insert, so a second one minted here would
                // never be given back. An aggregate-keyed insert consumes its
                // key instead: the key's counted slots are folded into the
                // entry's bytes and their shares given back, so a `Vec` key
                // takes this frame's share the way any consumed argument does.
                let consumes_key = arg_idx == 1
                    && matches!(
                        name.as_str(),
                        "gos_rt_map_insert_skey" | "gos_rt_map_insert_skey_opt"
                    );
                if name.starts_with("gos_rt_map_insert")
                    && !consumes_key
                    && vec_operand(arg).is_some()
                {
                    continue;
                }
                // A map of payload-enum or callable values owns its values the
                // same way, so the insert mints the entry's share of the node.
                if (name.starts_with("gos_rt_map_insert")
                    || name.starts_with("gos_rt_map_or_insert"))
                    && arg_idx >= 2
                    && let Some(l) = rc_operand(arg)
                    && tcx.is_counted_node(body.locals[l.0 as usize].ty)
                {
                    continue;
                }
                if let Some(l) = rc_operand(arg).or_else(|| vec_operand(arg)) {
                    terminator_retains.push((block_idx, l));
                    continue;
                }
                // A struct or tuple argument, for a container that keeps the
                // aggregate's own word rather than copying its slots: the
                // caller's binding releases the fields where it ends, so the
                // stored entry needs a share of each of them.
                if !stores_aggregate_by_pointer(name) {
                    continue;
                }
                // A keyed map copies an aggregate value into a
                // reference-counted blob. When the value type's structural
                // meta is registered, the blob's copy retains every heap
                // field itself and its release at the entry's death gives
                // each back - a share minted here would have no releaser. The
                // check is the same fact the backend's copy site reads, so
                // the two sides always agree; a route that never registered
                // the meta keeps the mint and degrades to a bounded leak
                // rather than an entry whose fields die under it. A channel
                // send boxes its aggregate the same way.
                if (name.starts_with("gos_rt_map_insert")
                    || name.starts_with("gos_rt_map_or_insert")
                    || name.starts_with("gos_rt_chan_send"))
                    && let Operand::Copy(vp) = arg
                    && vp.projection.is_empty()
                    && (vp.local.0 as usize) < body.locals.len()
                    && tcx
                        .rc_meta(&format!(
                            "gos_rc_meta_boxaggr_{}",
                            body.locals[vp.local.0 as usize].ty.as_u32()
                        ))
                        .is_some()
                {
                    continue;
                }
                if let Operand::Copy(p) = arg
                    && p.projection.is_empty()
                    && (p.local.0 as usize) < body.locals.len()
                {
                    for (path, kind) in
                        aggregate_rc_field_paths(tcx, body.locals[p.local.0 as usize].ty)
                    {
                        terminator_field_retains.push((block_idx, p.local, path, kind));
                    }
                }
            }
        }
    }

    // A local is *owned* (holds a reference this function must release)
    // only when an assignment gives it ownership:
    // - `gos_rc_alloc` (fresh allocation),
    // - a user-function call that returns an RC value (the callee minted
    //   the caller's reference via its return-copy retain),
    // - `to = Copy(from)` of an RC value (retained above).
    // Values *loaded* from a structure (`gos_load`, match-arm bindings,
    // field/index reads) or returned by a runtime accessor are interior
    // borrows - the containing object still owns them, so releasing them
    // here would double-free. They are excluded.
    // Locals that are the source of a bare `x = Copy(y)` statement: the copy
    // *target* becomes the owner. Used below to decide whether a by-value enum
    // payload extraction is owned by this frame (used inline) or by a binding
    // it was copied into (`let x = to_json()?`).
    let BareCopies {
        sourced: copy_sourced,
        sourced_to_binding: copy_sourced_to_binding,
        target: copy_target,
    } = bare_copies(body);
    // Payload locals read out of a carrier this frame consumed by value, and
    // how many bindings copy each one. `let v = f()?` lowers to an extraction
    // followed by a copy, so the binding - not the extraction - is the owner;
    // a payload copied more than once has no single owner and is left alone.
    let mut payload_src = vec![false; n_locals];
    let mut payload_copies = vec![0usize; n_locals];
    for block in &body.blocks {
        for stmt in &block.stmts {
            match &stmt.kind {
                StatementKind::Assign {
                    place,
                    rvalue: Rvalue::CallIntrinsic { name, args },
                } if *name == "gos_rt_result_payload"
                    && place.projection.is_empty()
                    && (place.local.0 as usize) < n_locals
                    && !enum_arg_is_borrowed(args) =>
                {
                    payload_src[place.local.0 as usize] = true;
                }
                StatementKind::Assign {
                    rvalue: Rvalue::Use(Operand::Copy(p)),
                    ..
                } if p.projection.is_empty() && (p.local.0 as usize) < n_locals => {
                    payload_copies[p.local.0 as usize] += 1;
                }
                _ => {}
            }
        }
    }

    // A reference-counted payload extracted out of a BORROWED container slot
    // (see `borrowed_enum_src`) into a binding this frame will own and
    // release (the `owned` `gos_rt_result_payload` arm below) needs its
    // own share: retain at the extraction site so the binding's release
    // and the container's element deep-free are both balanced. The
    // gating mirrors that `owned` arm exactly - retain iff a release
    // will be scheduled.
    for (block_idx, block) in body.blocks.iter().enumerate() {
        for (stmt_idx, stmt) in block.stmts.iter().enumerate() {
            if let StatementKind::Assign {
                place,
                rvalue: Rvalue::CallIntrinsic { name, args },
            } = &stmt.kind
                && *name == "gos_rt_result_payload"
                && place.projection.is_empty()
                && (place.local.0 as usize) < n_locals
                && !copy_sourced[place.local.0 as usize]
                && tcx.is_rc_managed(body.locals[place.local.0 as usize].ty)
                && enum_arg_is_borrowed(args)
                && match args.first() {
                    Some(Operand::Copy(p)) if p.projection.is_empty() => {
                        let e = p.local.0 as usize;
                        e >= n_locals || (!copy_sourced[e] && !copy_target[e])
                    }
                    _ => true,
                }
            {
                retain_sites.push((block_idx, stmt_idx, place.local, 1));
            }
        }
    }

    // Destinations of runtime calls answering a counted aggregate blob, which
    // the holder walk owns whatever their arm types say.
    let mut counted_answer_dest = vec![false; n_locals];
    for block in &body.blocks {
        if let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            destination,
            ..
        } = &block.terminator
            && destination.projection.is_empty()
            && (destination.local.0 as usize) < n_locals
            && answers_counted_blob(name, destination.local, body, tcx)
        {
            counted_answer_dest[destination.local.0 as usize] = true;
        }
    }
    // An `Err` payload read out of an option holder the frame built: the holder
    // keeps its share of the payload (its walk gives it back with the blob it
    // holds), so a binding the owned arm below releases takes a share of its
    // own. The gating is the owned arm's.
    for (block_idx, block) in body.blocks.iter().enumerate() {
        for (stmt_idx, stmt) in block.stmts.iter().enumerate() {
            if let StatementKind::Assign {
                place,
                rvalue: Rvalue::CallIntrinsic { name, args },
            } = &stmt.kind
                && *name == "gos_rt_result_payload"
                && place.projection.is_empty()
                && (place.local.0 as usize) < n_locals
                && !copy_sourced_to_binding[place.local.0 as usize]
                && tcx.is_rc_managed(body.locals[place.local.0 as usize].ty)
                && carrier_is_frame_owned(args)
                && let Some(Operand::Copy(src)) = args.first()
                && src.projection.is_empty()
                && (src.local.0 as usize) < n_locals
                && !copy_sourced[src.local.0 as usize]
                && !copy_target[src.local.0 as usize]
                && holder_err_kind(tcx, body.locals[src.local.0 as usize].ty).is_some()
                && (holds_counted_blob_arm(tcx, body.locals[src.local.0 as usize].ty)
                    || counted_answer_dest[src.local.0 as usize])
            {
                retain_sites.push((block_idx, stmt_idx, place.local, 1));
            }
        }
    }

    let mut owned = vec![false; n_locals];
    // Vec / Slice locals that became owned by extracting a `Vec`/`[T]` field
    // out of a by-value aggregate (`let v = rec.data`, or the borrowed method
    // receiver temp of `rec.data.len()`). Unlike a `String` field extract -
    // whose local is `is_rc_managed` and so lands in `releasable` for a
    // `gos_rt_rc_release` - a Vec local is not RC-managed, so it is released
    // here explicitly through `gos_rt_vec_free`, balancing the `gos_rt_vec_retain`
    // the field pass mints at the extract. The container-drop pass never marks
    // these (they are neither constructor nor call destinations), so there is
    // no double free.
    let mut vec_field_extract = vec![false; n_locals];
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign { place, rvalue } = &stmt.kind
                && place.projection.is_empty()
            {
                let i = place.local.0 as usize;
                if i >= n_locals {
                    continue;
                }
                if body.locals[i].region {
                    // Region-owned: freed wholesale at pop, never released here.
                    continue;
                }
                match rvalue {
                    Rvalue::CallIntrinsic { name, .. }
                        if *name == "gos_rc_alloc" || *name == "gos_rc_alloc_tagged" =>
                    {
                        owned[i] = true;
                    }
                    // The RC cell a `Weak` observes for a by-value aggregate
                    // referent: the frame that created it owns its strong
                    // reference and releases it at scope end, after which the
                    // outstanding weak count decides when the cell is freed.
                    Rvalue::CallIntrinsic { name, .. } if *name == "gos_rt_rc_weak_cell" => {
                        owned[i] = true;
                    }
                    // The shadow local pinning a `w.upgrade()` result: the
                    // upgrade shim took a fresh strong reference for the
                    // `Some` payload, and this extract (null for `None`)
                    // is the frame's owning handle on it - released at
                    // scope exit / reassignment like any owned RC local.
                    Rvalue::CallIntrinsic { name, .. } if *name == "gos_rt_weak_opt_payload" => {
                        owned[i] = true;
                    }
                    // A `String` payload moved out of a consumed by-value
                    // `Result`/`Option`/inline enum (`match o { Some(s) => … }`)
                    // and used INLINE (not copied into an owning binding). The
                    // frame owns it and must release it; the enum value itself
                    // frees nothing. When the extraction is copied into a
                    // binding (`let x = to_json()?`), that binding owns it (the
                    // `Use(Copy)` arm below) - so this arm excludes
                    // `copy_sourced` to avoid double-freeing the autoderive path.
                    // A `String` payload moved out of a consumed by-value
                    // `Result`/`Option`/inline enum (`match o { Some(s) => … }`)
                    // and used INLINE (not copied into an owning binding). The
                    // frame owns it and must release it; the enum value itself
                    // frees nothing. When the extraction is copied into a
                    // binding (`let x = to_json()?`), that binding owns it (the
                    // `Use(Copy)` arm below) - so this arm excludes
                    // `copy_sourced` to avoid double-freeing the autoderive path.
                    // `let v = f()?` / `let v = match r { Ok(v) => v, .. }` with a
                    // `Vec` payload: the extraction is copied into this binding,
                    // which becomes the payload's only owner. A GosVec carries no
                    // RC header, so it is released through the vec path.
                    Rvalue::Use(Operand::Copy(p))
                        if p.projection.is_empty()
                            && (p.local.0 as usize) < n_locals
                            && payload_src[p.local.0 as usize]
                            && payload_copies[p.local.0 as usize] == 1
                            && !copy_target[p.local.0 as usize]
                            && matches!(
                                tcx.kind_of(body.locals[i].ty),
                                gossamer_types::TyKind::Vec(_) | gossamer_types::TyKind::Slice(_)
                            ) =>
                    {
                        owned[i] = true;
                        vec_field_extract[i] = true;
                    }
                    // A `Vec` / `[T]` payload moved out of a consumed by-value
                    // `Result`/`Option`/inline enum (`match r { Ok(v) => … }`,
                    // `if let`, `?`). The carrier frees nothing, and a GosVec
                    // carries no RC header, so this local is the only owner and
                    // is released through the same `gos_rt_vec_free` path an
                    // aggregate-field extract uses.
                    Rvalue::CallIntrinsic { name, args }
                        if *name == "gos_rt_result_payload"
                            && !copy_sourced[i]
                            && matches!(
                                tcx.kind_of(body.locals[i].ty),
                                gossamer_types::TyKind::Vec(_) | gossamer_types::TyKind::Slice(_)
                            )
                            && !enum_arg_is_borrowed(args)
                            && match args.first() {
                                Some(Operand::Copy(p)) if p.projection.is_empty() => {
                                    let e = p.local.0 as usize;
                                    e >= n_locals || (!copy_sourced[e] && !copy_target[e])
                                }
                                _ => true,
                            } =>
                    {
                        owned[i] = true;
                        vec_field_extract[i] = true;
                    }
                    // Any other reference-counted payload read out of a
                    // consumed by-value carrier and used where it stands - a
                    // `String`, a payload-bearing enum node whose arm is
                    // matched inline. The carrier releases nothing, so the
                    // frame holds the payload's one share and gives it back at
                    // scope; the node's own meta then frees the heap fields it
                    // carries.
                    Rvalue::CallIntrinsic { name, args }
                        if *name == "gos_rt_result_payload"
                            && !copy_sourced_to_binding[i]
                            && tcx.is_rc_managed(body.locals[i].ty)
                            && carrier_is_frame_owned(args)
                            && match args.first() {
                                Some(Operand::Copy(p)) if p.projection.is_empty() => {
                                    let e = p.local.0 as usize;
                                    e >= n_locals || (!copy_sourced[e] && !copy_target[e])
                                }
                                _ => true,
                            } =>
                    {
                        owned[i] = true;
                    }
                    Rvalue::Use(Operand::Copy(p))
                        if p.projection.is_empty()
                            && (p.local.0 as usize) < n_locals
                            && tcx.is_rc_managed(body.locals[p.local.0 as usize].ty) =>
                    {
                        owned[i] = true;
                    }
                    // A String binding copied from an untyped borrowed
                    // word-slot element: the retain arm above minted its
                    // share, so the frame owns and releases it like any
                    // RC copy (the source local is typed i64, so the
                    // rc-managed arm above cannot see it).
                    Rvalue::Use(Operand::Copy(p))
                        if p.projection.is_empty()
                            && (p.local.0 as usize) < n_locals
                            && borrowed_word_elem_src[p.local.0 as usize]
                            && matches!(
                                tcx.kind_of(body.locals[i].ty),
                                gossamer_types::TyKind::String
                            ) =>
                    {
                        owned[i] = true;
                    }
                    // Identity tag of an RC enum pointer: same ownership
                    // shape as `Copy`.
                    Rvalue::CallIntrinsic { name, args }
                        if *name == "gos_enum_tag"
                            && matches!(
                                args.first(),
                                Some(Operand::Copy(p))
                                    if p.projection.is_empty()
                                        && (p.local.0 as usize) < n_locals
                                        && tcx.is_rc_managed(
                                            body.locals[p.local.0 as usize].ty
                                        )
                            ) =>
                    {
                        owned[i] = true;
                    }
                    // `s = Copy(payload)` where the `?` / match extraction left
                    // the source local typed `Var` but the binding settled on a
                    // concrete RC type (`let s = f()?` for `f -> Result<String,
                    // _>`): the consumed enum transferred the payload's
                    // ownership to this binding, so the frame must release it.
                    // Field-extract `X = Copy(Y.field)` of an RC field: X owns a
                    // new reference to that value (retained at the extract site
                    // in the field pass), released at scope like any RC local.
                    //
                    // `Y` reaches the aggregate either by value or through a
                    // `&`/`&mut` parameter, and the extract mints the same
                    // share either way, so ownership is read through the
                    // pointee - the identical predicate the retain pass uses.
                    // A value-container field is excluded there and so is
                    // excluded here: no share is minted for one, so none is
                    // owed back.
                    Rvalue::Use(Operand::Copy(p))
                        if p.projection.len() == 1 && (p.local.0 as usize) < n_locals =>
                    {
                        if let crate::ir::Projection::Field(fidx) = p.projection[0] {
                            let base_ty = pointee_of(tcx, body.locals[p.local.0 as usize].ty);
                            if agg_rc_fields(base_ty).iter().any(|(path, kind)| {
                                path.as_slice() == [fidx] && !kind.is_value_container()
                            }) {
                                owned[i] = true;
                                if matches!(
                                    tcx.kind_of(body.locals[i].ty),
                                    gossamer_types::TyKind::Vec(_)
                                        | gossamer_types::TyKind::Slice(_)
                                ) {
                                    vec_field_extract[i] = true;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Terminator::Call {
            callee,
            destination,
            ..
        } = &block.terminator
            && destination.projection.is_empty()
        {
            let i = destination.local.0 as usize;
            // A user function transfers ownership of its RC return value;
            // a runtime accessor (`gos_rt_*`) or a raw `gos_load` /
            // `gos_store` may hand back an interior borrow it still owns,
            // so do not treat that as owned. `gos_load` appears in
            // terminator position (not just as a `CallIntrinsic`
            // statement) when it sits at a block boundary - e.g. the
            // element load of a `for x in xs` loop body. Releasing such a
            // borrow frees a value the container still owns (double-free /
            // use-after-free on the next iteration).
            // `gos_rt_rc_downgrade` is the one runtime call that hands
            // back an *owned* reference (a fresh weak count) rather than
            // an interior borrow: the local owns that weak count and must
            // weak_release it at scope end. Every other `gos_rt_*` return
            // is a borrow the runtime still owns.
            let owns_return = match callee {
                Operand::FnRef { .. } => true,
                Operand::Const(ConstValue::Str(name)) => {
                    (!name.starts_with("gos_rt_") && name != "gos_load" && name != "gos_store")
                        || name == "gos_rt_rc_downgrade"
                        || mints_owned_string(name)
                        || mints_owned_error(name)
                }
                _ => true,
            };
            // Region-owned call results (e.g. a tree built inside a region
            // block) are freed at pop - never release them here.
            if owns_return && i < n_locals && !body.locals[i].region {
                owned[i] = true;
            }
        }
    }

    // Move elision. An owned local that is *read exactly once*, and whose
    // single read is a consuming acquisition (copy / store / aggregate /
    // container-push), transfers its single reference to the new owner:
    // no retain at that site and no release of the source. This collapses
    // the common construct-and-move pattern (build a child, store it into
    // a node, return the node) to zero refcount traffic, while genuine
    // aliasing (`let b = a; let c = a`, two reads) still retains.
    //
    // `total_reads` must never *under*-count, or a still-aliased value
    // would be elided and double-freed; counting every operand and
    // place-base appearance (writes excepted) keeps it conservative.
    let mut total_reads = vec![0u32; n_locals];
    let bump = |reads: &mut [u32], op: &Operand| {
        // Only a bare (unprojected) Copy aliases the value itself; a
        // projected copy reads a field, which is a separate value.
        if let Operand::Copy(p) = op
            && p.projection.is_empty()
        {
            let i = p.local.0 as usize;
            if i < n_locals {
                reads[i] = reads[i].saturating_add(1);
            }
        }
    };
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign { rvalue, .. } = &stmt.kind {
                match rvalue {
                    Rvalue::Use(op)
                    | Rvalue::UnaryOp { operand: op, .. }
                    | Rvalue::Cast { operand: op, .. }
                    | Rvalue::Repeat { value: op, .. } => bump(&mut total_reads, op),
                    Rvalue::BinaryOp { lhs, rhs, .. } => {
                        bump(&mut total_reads, lhs);
                        bump(&mut total_reads, rhs);
                    }
                    Rvalue::Aggregate { operands, .. } => {
                        for op in operands {
                            bump(&mut total_reads, op);
                        }
                    }
                    Rvalue::CallIntrinsic { name, args } => {
                        if *name == "gos_store" {
                            // Only the stored value (arg 2) flows; the
                            // object (arg 0) is merely written through.
                            if let Some(op) = args.get(2) {
                                bump(&mut total_reads, op);
                            }
                        } else if *name == "gos_enum_set_disc" {
                            // Writes the discriminant byte through the
                            // pointer; aliases nothing.
                        } else if *name != "gos_load" && *name != "gos_enum_disc" {
                            // `gos_load` / `gos_enum_disc` only access
                            // their object; every other intrinsic
                            // consumes its args.
                            for op in args {
                                bump(&mut total_reads, op);
                            }
                        }
                    }
                    // `Ref`/`Len`/projected reads access memory, they do
                    // not alias the bare value.
                    Rvalue::Ref { .. } | Rvalue::Len(_) => {}
                    // Reads a scalar global by symbol; aliases no local.
                    Rvalue::StaticLoad(_) => {}
                }
            }
        }
        match &block.terminator {
            Terminator::SwitchInt { discriminant, .. } => bump(&mut total_reads, discriminant),
            Terminator::Call { callee, args, .. } => {
                bump(&mut total_reads, callee);
                for op in args {
                    bump(&mut total_reads, op);
                }
            }
            Terminator::Assert { cond, msg, .. } => {
                bump(&mut total_reads, cond);
                for op in msg.operands() {
                    bump(&mut total_reads, op);
                }
            }
            _ => {}
        }
    }
    // `let xs = f()` binds the callee's value to a name: the call result is
    // read exactly once, by this copy, so its share transfers to the binding
    // and the binding is what owes the release. Without this the binding is
    // not an owner, so the share it took from the temporary is never given
    // back - a leak that only shows once the value is stored somewhere that
    // counts shares.
    {
        let mut changed = true;
        while changed {
            changed = false;
            for block in &body.blocks {
                for stmt in &block.stmts {
                    if let StatementKind::Assign { place, rvalue } = &stmt.kind
                        && place.projection.is_empty()
                        && (place.local.0 as usize) < n_locals
                        && !owned[place.local.0 as usize]
                        && let Rvalue::Use(Operand::Copy(src)) = rvalue
                        && src.projection.is_empty()
                        && (src.local.0 as usize) < n_locals
                        && owned[src.local.0 as usize]
                        && total_reads[src.local.0 as usize] == 1
                        && !body.locals[place.local.0 as usize].region
                    {
                        owned[place.local.0 as usize] = true;
                        changed = true;
                    }
                }
            }
        }
    }

    // A local has a consuming read iff it sources a retain site.
    let mut consuming_read = vec![false; n_locals];
    for (_, _, l, _) in &retain_sites {
        let i = l.0 as usize;
        if i < n_locals {
            consuming_read[i] = true;
        }
    }
    for (_, l) in &terminator_retains {
        let i = l.0 as usize;
        if i < n_locals {
            consuming_read[i] = true;
        }
    }
    for &(_, _, l) in &extraction_moves {
        let i = l.0 as usize;
        if i < n_locals {
            consuming_read[i] = true;
        }
    }
    // A source read inside a loop runs on every iteration, so a static read
    // count of 1 does not license a move: the value is re-read across the loop
    // back-edge. Move-eliding the retain there while the destination is
    // released each iteration over-releases the source (refcount underflow ->
    // premature free). Compute the blocks on a cycle, then forbid move-elision
    // for any local read inside one - keeping the balancing retain.
    let nb = body.blocks.len();
    let succs: Vec<Vec<usize>> = body
        .blocks
        .iter()
        .map(|b| match &b.terminator {
            Terminator::Goto { target } => vec![target.0 as usize],
            Terminator::SwitchInt { arms, default, .. } => {
                let mut v: Vec<usize> = arms.iter().map(|(_, t)| t.0 as usize).collect();
                v.push(default.0 as usize);
                v
            }
            Terminator::Call { target, .. } => target.iter().map(|t| t.0 as usize).collect(),
            Terminator::Assert { target, .. } => vec![target.0 as usize],
            Terminator::Drop { target, .. } => vec![target.0 as usize],
            _ => Vec::new(),
        })
        .collect();
    let block_in_loop: Vec<bool> = (0..nb)
        .map(|start| {
            // `start` lies on a cycle iff it is reachable from one of its own
            // successors (a path leaves `start` and returns to it).
            let mut seen = vec![false; nb];
            let mut stack: Vec<usize> = succs[start].clone();
            while let Some(b) = stack.pop() {
                if b == start {
                    return true;
                }
                if b >= nb || seen[b] {
                    continue;
                }
                seen[b] = true;
                stack.extend(succs[b].iter().copied());
            }
            false
        })
        .collect();
    let mut read_in_loop = vec![false; n_locals];
    // Locals (re)assigned inside a loop hold a fresh value each iteration, so
    // a single read of them is a genuine move (the value is consumed and
    // replaced, e.g. an accumulator or a per-iteration binding moved into a
    // container). Only a loop-INVARIANT source - read in the loop but defined
    // outside it - is re-read across the back-edge and must keep its retain.
    let mut assigned_in_loop = vec![false; n_locals];
    let mark_copy = |op: &Operand, out: &mut Vec<bool>| {
        if let Operand::Copy(p) = op
            && p.projection.is_empty()
            && (p.local.0 as usize) < n_locals
        {
            out[p.local.0 as usize] = true;
        }
    };
    for (bi, block) in body.blocks.iter().enumerate() {
        if !block_in_loop[bi] {
            continue;
        }
        for stmt in &block.stmts {
            if let StatementKind::Assign { place, .. } = &stmt.kind
                && place.projection.is_empty()
                && (place.local.0 as usize) < n_locals
            {
                assigned_in_loop[place.local.0 as usize] = true;
            }
            if let StatementKind::Assign { rvalue, .. } = &stmt.kind {
                match rvalue {
                    Rvalue::Use(op)
                    | Rvalue::UnaryOp { operand: op, .. }
                    | Rvalue::Cast { operand: op, .. }
                    | Rvalue::Repeat { value: op, .. } => mark_copy(op, &mut read_in_loop),
                    Rvalue::BinaryOp { lhs, rhs, .. } => {
                        mark_copy(lhs, &mut read_in_loop);
                        mark_copy(rhs, &mut read_in_loop);
                    }
                    Rvalue::Aggregate { operands, .. } => {
                        for op in operands {
                            mark_copy(op, &mut read_in_loop);
                        }
                    }
                    Rvalue::CallIntrinsic { args, .. } => {
                        for op in args {
                            mark_copy(op, &mut read_in_loop);
                        }
                    }
                    _ => {}
                }
            }
        }
        match &block.terminator {
            Terminator::SwitchInt { discriminant, .. } => {
                mark_copy(discriminant, &mut read_in_loop);
            }
            Terminator::Call {
                callee,
                args,
                destination,
                ..
            } => {
                mark_copy(callee, &mut read_in_loop);
                for op in args {
                    mark_copy(op, &mut read_in_loop);
                }
                if destination.projection.is_empty() && (destination.local.0 as usize) < n_locals {
                    assigned_in_loop[destination.local.0 as usize] = true;
                }
            }
            Terminator::Assert { cond, msg, .. } => {
                mark_copy(cond, &mut read_in_loop);
                for op in msg.operands() {
                    mark_copy(op, &mut read_in_loop);
                }
            }
            _ => {}
        }
    }
    // A loop-invariant source (read inside a loop, not reassigned there) is
    // re-read every iteration, so its single static read is not a move.
    let mut moved: Vec<bool> = (0..n_locals)
        .map(|i| {
            owned[i]
                && total_reads[i] == 1
                && consuming_read[i]
                && !(read_in_loop[i] && !assigned_in_loop[i])
        })
        .collect();

    // A move elision is only sound when the consuming read runs on EVERY
    // path from the value's assignment to function exit: with the retain
    // and the owner release both elided, a path that skips the consume
    // (`if cond { keys.push(v) }`) never frees the value. Keep the
    // elision only when the consuming site's block lies on every
    // assignment-to-return path - checked by walking the CFG from each
    // assignment with the consuming block removed; reaching a Return
    // means a consume-skipping path exists, so the retain/release pair
    // must stay (the balanced counts are correct on both paths).
    {
        // The single consuming site's block per local (total_reads == 1
        // guarantees at most one). A statement-position consume mid-block
        // still runs whenever its block is entered, so block granularity
        // is exact for the walk below; the same-block case additionally
        // requires the consume at or after the assignment position.
        let mut consume_site: Vec<Option<(usize, usize)>> = vec![None; n_locals];
        for (bi, si, l, _) in &retain_sites {
            let i = l.0 as usize;
            if i < n_locals {
                consume_site[i] = Some((*bi, *si));
            }
        }
        for (bi, l) in &terminator_retains {
            let i = l.0 as usize;
            if i < n_locals {
                consume_site[i] = Some((*bi, usize::MAX));
            }
        }
        for &(bi, si, l) in &extraction_moves {
            let i = l.0 as usize;
            if i < n_locals {
                consume_site[i] = Some((bi, si));
            }
        }
        let mut assign_sites: Vec<Vec<(usize, usize)>> = vec![Vec::new(); n_locals];
        for (bi, block) in body.blocks.iter().enumerate() {
            for (si, stmt) in block.stmts.iter().enumerate() {
                if let StatementKind::Assign { place, .. } = &stmt.kind
                    && place.projection.is_empty()
                    && (place.local.0 as usize) < n_locals
                {
                    assign_sites[place.local.0 as usize].push((bi, si));
                }
            }
            if let Terminator::Call { destination, .. } = &block.terminator
                && destination.projection.is_empty()
                && (destination.local.0 as usize) < n_locals
            {
                assign_sites[destination.local.0 as usize].push((bi, usize::MAX));
            }
        }
        let successors = |bi: usize| -> Vec<usize> {
            match &body.blocks[bi].terminator {
                Terminator::Goto { target } => vec![target.0 as usize],
                Terminator::SwitchInt { arms, default, .. } => {
                    let mut v: Vec<usize> = arms.iter().map(|(_, t)| t.0 as usize).collect();
                    v.push(default.0 as usize);
                    v
                }
                Terminator::Call { target, .. } => {
                    target.map(|t| vec![t.0 as usize]).unwrap_or_default()
                }
                Terminator::Assert { target, .. } => vec![target.0 as usize],
                _ => Vec::new(),
            }
        };
        let return_reachable_avoiding = |start: usize, avoid: usize| -> bool {
            let mut seen = vec![false; body.blocks.len()];
            let mut stack = vec![start];
            while let Some(b) = stack.pop() {
                if b == avoid || b >= body.blocks.len() || seen[b] {
                    continue;
                }
                seen[b] = true;
                if matches!(body.blocks[b].terminator, Terminator::Return) {
                    return true;
                }
                stack.extend(successors(b));
            }
            false
        };
        for i in 0..n_locals {
            if !moved[i] {
                continue;
            }
            let Some((cbi, csi)) = consume_site[i] else {
                continue;
            };
            let covered = assign_sites[i].iter().all(|&(abi, asi)| {
                if abi == cbi {
                    // Same block: the consume covers this assignment only
                    // when it runs after it on the block's straight line.
                    return csi == usize::MAX || asi < csi;
                }
                // The assignment's block flows on through its successors;
                // if a Return is reachable without entering the consuming
                // block, a consume-skipping path exists.
                !successors(abi)
                    .into_iter()
                    .any(|s| return_reachable_avoiding(s, cbi))
            });
            if !covered {
                moved[i] = false;
            }
        }
    }

    // A binding that is itself moved into the aggregate hands over the one
    // reference the carrier gave it, so the extraction that fed it mints
    // nothing: the retain was kept only against a release move elision has
    // now cancelled, and the carrier's own share would be left with no owner.
    {
        let cancelled: std::collections::HashSet<(usize, usize, u32)> = stored_extraction_retains
            .iter()
            .filter(|(_, _, _, binding)| moved[binding.0 as usize])
            .map(|(bi, si, payload, _)| (*bi, *si, payload.0))
            .collect();
        if !cancelled.is_empty() {
            retain_sites.retain(|(bi, si, l, _)| !cancelled.contains(&(*bi, *si, l.0)));
        }
    }
    // A copy out of an extraction result took no retain on the premise that it
    // moves the value. Where move elision keeps the source as an owner, the
    // copy takes the share it would otherwise have taken, so the source's
    // release and the copy's give back one share each.
    for &(bi, si, l) in &extraction_moves {
        let i = l.0 as usize;
        if i < n_locals && owned[i] && !moved[i] {
            retain_sites.push((bi, si, l, 1));
        }
    }
    // Drop retains whose source is moved (the single reference transfers
    // to the new owner; no `+1`).
    retain_sites.retain(|(_, _, l, _)| !moved[l.0 as usize]);
    terminator_retains.retain(|(_, l)| !moved[l.0 as usize]);
    // Drop retains of immortal-by-construction constants.
    retain_sites.retain(|(_, _, l, _)| !const_init_only[l.0 as usize]);
    terminator_retains.retain(|(_, l)| !const_init_only[l.0 as usize]);
    // A rebound `Vec` owner takes a share of each value copied into it and
    // gives back what it holds before every rebinding and at every return.
    let rebound = rebound_vec_owners(body, tcx);
    for (bi, block) in body.blocks.iter().enumerate() {
        for (si, stmt) in block.stmts.iter().enumerate() {
            if let StatementKind::Assign {
                place,
                rvalue: Rvalue::Use(Operand::Copy(src)),
            } = &stmt.kind
                && place.projection.is_empty()
                && src.projection.is_empty()
                && rebound[place.local.0 as usize]
                && !moved[src.local.0 as usize]
            {
                retain_sites.push((bi, si, src.local, 1));
            }
        }
    }
    for (i, &is_rebound) in rebound.iter().enumerate() {
        if is_rebound {
            owned[i] = true;
            vec_field_extract[i] = true;
            moved[i] = false;
        }
    }

    // Releasable owners: RC locals (not parameter / return slot) that are
    // owned here and not moved out. Each surviving new reference was
    // retained above, so releasing every owner keeps the count balanced.
    // A local whose value flows into the return slot must NOT be released here
    // - the caller receives and owns it (else an owned producer result that is
    // returned would be freed at scope AND by the caller). Backward closure
    // from `Local::RETURN` over bare `Copy` and aggregate-operand edges.
    let mut flows_to_return = vec![false; n_locals];
    flows_to_return[Local::RETURN.0 as usize] = true;
    let mut rf_changed = true;
    while rf_changed {
        rf_changed = false;
        for block in &body.blocks {
            for stmt in &block.stmts {
                let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                    continue;
                };
                if !place.projection.is_empty() || (place.local.0 as usize) >= n_locals {
                    continue;
                }
                if !flows_to_return[place.local.0 as usize] {
                    continue;
                }
                let mut mark = |l: Local, ch: &mut bool| {
                    let f = l.0 as usize;
                    if f < n_locals && !flows_to_return[f] {
                        flows_to_return[f] = true;
                        *ch = true;
                    }
                };
                match rvalue {
                    Rvalue::Use(Operand::Copy(pp)) if pp.projection.is_empty() => {
                        mark(pp.local, &mut rf_changed);
                    }
                    // Identity tag: the source IS the returned allocation.
                    Rvalue::CallIntrinsic { name, args } if *name == "gos_enum_tag" => {
                        if let Some(Operand::Copy(pp)) = args.first()
                            && pp.projection.is_empty()
                        {
                            mark(pp.local, &mut rf_changed);
                        }
                    }
                    Rvalue::Aggregate { operands, .. } => {
                        for op in operands {
                            if let Operand::Copy(pp) = op
                                && pp.projection.is_empty()
                            {
                                mark(pp.local, &mut rf_changed);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    // A Vec/Slice local that was extracted from an aggregate field is an
    // owner too (the field pass retained its share), but is not `is_rc` (a
    // GosVec has no RC header). Release it through the same machinery so its
    // `gos_rt_vec_free` balances the extract-site `gos_rt_vec_retain`. Guarded
    // to body temporaries so a parameter / return-slot is never included.
    let is_vec_field_owner =
        |i: usize| vec_field_extract[i] && i > arity && i < n_locals && !body.locals[i].region;
    let releasable: Vec<Local> = (0..n_locals)
        .filter(|&i| {
            (is_rc(i) || is_vec_field_owner(i))
                && owned[i]
                && !moved[i]
                && !flows_to_return[i]
                // A view of a node the caller holds took no share, so it has
                // none to give back; see `enum_child_borrow`.
                && !enum_child_borrow[i]
        })
        .map(|i| Local(u32::try_from(i).unwrap_or(0)))
        .collect();

    // A `Vec` payload the frame releases is one the frame still holds when the
    // carrier takes it, so the carrier gets a share of its own here - the two
    // sides of the exchange the `vec_released` gate above books for a payload
    // an earlier pass already scheduled. Without it a released payload leaves
    // in the carrier with the frame's release freeing it under the caller.
    {
        let released: std::collections::HashSet<u32> = releasable.iter().map(|l| l.0).collect();
        for (block_idx, stmt_idx, local) in carrier_vec_sites.drain(..) {
            if released.contains(&local.0) {
                retain_sites.push((block_idx, stmt_idx, local, 1));
            }
        }
    }

    // Return-copy move: `Local(0) = Copy(l)` in a `Return` block, where
    // `l` is a frame-owned RC local that flows into the return slot, is a
    // MOVE - `l`'s own reference transfers to the caller, and the frame
    // never releases `l` (it is `flows_to_return`, so excluded from
    // `releasable`). The return-copy retain scheduled above would mint a
    // SECOND reference nothing balances, leaking one per call whenever `l`
    // has other reads (the `s += ...; return s` accumulator: the `+=` and
    // the return copy) so plain single-read move-elision cannot fire. Drop
    // that retain. A parameter source keeps its retain - `is_rc` is false
    // for the borrowed params (`i <= arity`), so returning one genuinely
    // mints the caller's new reference and is left untouched.
    retain_sites.retain(|&(bi, si, l, _)| {
        let li = l.0 as usize;
        let is_return_copy = matches!(
            body.blocks.get(bi).and_then(|b| b.stmts.get(si)),
            Some(Statement {
                kind:
                    StatementKind::Assign {
                        place,
                        rvalue: Rvalue::Use(Operand::Copy(src)),
                    },
                ..
            }) if place.local == Local::RETURN
                && place.projection.is_empty()
                && src.local == l
                && src.projection.is_empty()
        ) && matches!(
            body.blocks.get(bi).map(|b| &b.terminator),
            Some(Terminator::Return)
        );
        !(is_return_copy && is_rc(li) && owned[li] && flows_to_return[li])
    });

    // Aggregate locals whose every whole-local assignment is a payload
    // extraction are BORROWS: the source Result or enum node owns the
    // payload's fields, the extraction never retained them, so it must not
    // release them at death either. `gos_enum_load` also runs BEFORE its
    // arm's discriminant test, so on any other variant the local holds a
    // payload of a different shape - releasing its fields at return would
    // read one variant's words as another's.
    let mut extraction_seed = vec![false; n_locals];
    {
        // A carrier the frame was handed hands its payload over with it: a
        // callee's `Result` is the caller's value, and a channel `send` mints
        // the element's share (see `stores_aggregate_by_pointer`) for the
        // receiver to give back. So an aggregate payload taken out of one of
        // those is OWNED, not a view of a carrier something else still holds,
        // and its fields are released when it dies. A carrier the frame built
        // itself, or one reached through a reference, keeps the borrow rule.
        let mut from_owned_carrier = vec![false; n_locals];
        for block in &body.blocks {
            let Terminator::Call {
                callee,
                destination,
                ..
            } = &block.terminator
            else {
                continue;
            };
            if !destination.projection.is_empty() || (destination.local.0 as usize) >= n_locals {
                continue;
            }
            let hands_over = match callee {
                Operand::FnRef { .. } => true,
                Operand::Const(ConstValue::Str(name)) => {
                    name.starts_with("gos_rt_chan_recv") || name.starts_with("gos_rt_chan_try_recv")
                }
                _ => false,
            };
            if hands_over {
                from_owned_carrier[destination.local.0 as usize] = true;
            }
        }
        // A carrier bound to a local of its own is the same carrier the call
        // answered: `let outcome = f(..)` followed by `match outcome` reads a
        // value this frame owns exactly as matching the call's own destination
        // does, so the payload taken out of it is owned and its fields are
        // released when it dies.
        {
            let copy_edges: Vec<(usize, usize)> = body
                .blocks
                .iter()
                .flat_map(|b| &b.stmts)
                .filter_map(|stmt| {
                    if let StatementKind::Assign {
                        place,
                        rvalue: Rvalue::Use(Operand::Copy(src)),
                    } = &stmt.kind
                        && place.projection.is_empty()
                        && src.projection.is_empty()
                        && (place.local.0 as usize) < n_locals
                        && (src.local.0 as usize) < n_locals
                    {
                        Some((place.local.0 as usize, src.local.0 as usize))
                    } else {
                        None
                    }
                })
                .collect();
            let mut changed = true;
            while changed {
                changed = false;
                for &(dest, src) in &copy_edges {
                    if from_owned_carrier[src] && !from_owned_carrier[dest] {
                        from_owned_carrier[dest] = true;
                        changed = true;
                    }
                }
            }
        }
        let extracts_owned = |args: &[Operand]| -> bool {
            matches!(
                args.first(),
                Some(Operand::Copy(p))
                    if p.projection.is_empty()
                        && (p.local.0 as usize) < n_locals
                        && from_owned_carrier[p.local.0 as usize]
            )
        };
        let mut non_extraction = vec![false; n_locals];
        for block in &body.blocks {
            for stmt in &block.stmts {
                if let StatementKind::Assign { place, rvalue } = &stmt.kind
                    && place.projection.is_empty()
                    && (place.local.0 as usize) < n_locals
                {
                    match rvalue {
                        Rvalue::CallIntrinsic { name, args }
                            if matches!(
                                *name,
                                "gos_rt_result_payload" | "gos_enum_load" | "gos_enum_slot_ptr"
                            ) =>
                        {
                            // A boxed aggregate payload is copied out of its box
                            // with a share of each counted field, whoever holds
                            // the node, so the binding owns what it holds.
                            let copies_box = *name == "gos_enum_load"
                                && tcx.is_boxed_payload_binding(
                                    body.locals[place.local.0 as usize].ty,
                                );
                            if copies_box || extracts_owned(args) {
                                non_extraction[place.local.0 as usize] = true;
                            } else {
                                extraction_seed[place.local.0 as usize] = true;
                            }
                        }
                        _ => non_extraction[place.local.0 as usize] = true,
                    }
                }
            }
            if let Terminator::Call {
                callee,
                args,
                destination,
                ..
            } = &block.terminator
                && destination.projection.is_empty()
                && (destination.local.0 as usize) < n_locals
            {
                // `unwrap` on a carrier the frame does not own answers the
                // payload the carrier still holds - a `Map` entry's blob, say -
                // exactly as the statement-position payload reads do. The
                // aggregate that lands in the destination is a view of those
                // slots: nothing was minted for it, so nothing is released
                // when it dies.
                let payload_read = matches!(
                    callee,
                    Operand::Const(ConstValue::Str(name))
                        if matches!(name.as_str(), "gos_rt_option_unwrap" | "gos_rt_result_unwrap")
                );
                if payload_read && !extracts_owned(args) {
                    extraction_seed[destination.local.0 as usize] = true;
                } else {
                    non_extraction[destination.local.0 as usize] = true;
                }
            }
        }
        for i in 0..n_locals {
            extraction_seed[i] = extraction_seed[i] && !non_extraction[i];
        }
    }
    // Field-extract `X = Copy(Y.field)` of an RC field: X holds a fresh
    // reference to the field value, so retain it. Added after move-elision
    // filtering so it always fires - Y still owns its own copy of the field
    // and releases it when Y dies.
    //
    // `Y` may be a `&`/`&mut` parameter: `fn tags(&self) -> Vec<String> {
    // self.tags }` projects the field straight off the reference, and the
    // value the caller receives is a share of the referent's field just as it
    // is when the base is owned. The referent keeps its own, so the fields
    // are read through the pointee type.
    //
    // A value-container field (a `Map`, `Set`, `Deque`, or heap) is not one of
    // them: its handle carries no reference count, so the extract is a borrow
    // of the owner's storage and the whole ownership story is the owner's own
    // field-clone / field-release schedule. Routing one through `rc_helper`
    // would reach `gos_rt_rc_retain`, which reads a header the allocation does
    // not carry.
    for (block_idx, block) in body.blocks.iter().enumerate() {
        for (stmt_idx, stmt) in block.stmts.iter().enumerate() {
            if let StatementKind::Assign { place, rvalue } = &stmt.kind
                && place.projection.is_empty()
                && (place.local.0 as usize) < n_locals
                // A region local releases nothing, so it is a view of the field.
                && !body.locals[place.local.0 as usize].region
                && let Rvalue::Use(Operand::Copy(src)) = rvalue
                && src.projection.len() == 1
                && let crate::ir::Projection::Field(fidx) = src.projection[0]
                && (src.local.0 as usize) < n_locals
                // A carrier field is two words by value: the binding copied out
                // of it reads the aggregate's payload share, and nothing gives
                // that binding a release of its own.
                && agg_rc_fields(pointee_of(tcx, body.locals[src.local.0 as usize].ty))
                    .iter()
                    .any(|(path, kind)| {
                        path.as_slice() == [fidx]
                            && !kind.is_value_container()
                            && !matches!(kind, FieldRcKind::Carrier { .. })
                    })
            {
                retain_sites.push((block_idx, stmt_idx, place.local, 1));
            }
        }
    }

    // A `Map` word that leaves through the return slot is the caller's to
    // free: every map-returning call books a `gos_rt_map_free` on its
    // destination. A `GosMap` carries no reference count, so when that word
    // came out of an aggregate's field the aggregate and the return slot
    // would name one table under two owners. The return slot takes a table of
    // its own instead - `gos_rt_map_field_clone` reads the slot's address and
    // writes the clone back, so the aggregate keeps the map its own release
    // schedule frees.
    let returned_map_field_sites: Vec<(usize, usize)> = {
        let mut from_map_field: std::collections::HashSet<u32> = std::collections::HashSet::new();
        let is_map_local = |l: Local| {
            (l.0 as usize) < n_locals
                && matches!(
                    tcx.kind_of(body.locals[l.0 as usize].ty),
                    gossamer_types::TyKind::HashMap { .. }
                )
        };
        // A copy chain can cross blocks in either order, so iterate to a
        // fixpoint rather than assuming the definition comes first.
        let mut changed = true;
        while changed {
            changed = false;
            for block in &body.blocks {
                for stmt in &block.stmts {
                    let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                        continue;
                    };
                    let Rvalue::Use(Operand::Copy(src)) = rvalue else {
                        continue;
                    };
                    if !place.projection.is_empty()
                        || !is_map_local(place.local)
                        || from_map_field.contains(&place.local.0)
                    {
                        continue;
                    }
                    let carries = if src.projection.is_empty() {
                        from_map_field.contains(&src.local.0)
                    } else {
                        src.projection
                            .iter()
                            .all(|p| matches!(p, crate::ir::Projection::Field(_)))
                    };
                    if carries {
                        from_map_field.insert(place.local.0);
                        changed = true;
                    }
                }
            }
        }
        let mut sites = Vec::new();
        for (block_idx, block) in body.blocks.iter().enumerate() {
            for (stmt_idx, stmt) in block.stmts.iter().enumerate() {
                if let StatementKind::Assign {
                    place,
                    rvalue: Rvalue::Use(Operand::Copy(src)),
                } = &stmt.kind
                    && place.local == Local::RETURN
                    && place.projection.is_empty()
                    && is_map_local(place.local)
                    && src.projection.is_empty()
                    && from_map_field.contains(&src.local.0)
                {
                    sites.push((block_idx, stmt_idx));
                }
            }
        }
        sites
    };

    // Aggregate locals that are BORROWS of a container element: the
    // `for p in &v` loop variable, whose value is `Copy`-ed from a
    // `gos_rt_vec_get_ptr` interior pointer the vec still owns. Such a
    // local must NOT release the element's RC fields - the container (or
    // the by-value aggregate that was pushed into it) owns them, so a
    // per-field release here double-frees with the owner's release. The
    // get_ptr result type is a raw element pointer, so the copy-on-load
    // never minted a balancing retain; treat the whole local as a
    // non-owning view. Mirrors `extraction_seed`, but propagates through
    // the `loopvar = Copy(get_ptr_result)` edge the loop lowering emits.
    // A lifted closure's capture prologue projects each captured value out of
    // its environment (`gos_load(__env, offset)`, `__env` being the lifted
    // body's first parameter). A closure observes the value the enclosing
    // scope holds - a struct is captured by managed reference - so the local
    // it lands in is a view of that storage, exactly as a container element
    // pointer is. Seeding it here gives it the whole non-owning treatment: no
    // share of the captured aggregate's heap fields, and no release of them.
    let capture_env_load = |block: &BasicBlock| -> Option<usize> {
        let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            args,
            destination,
            ..
        } = &block.terminator
        else {
            return None;
        };
        if name != "gos_load"
            || !body.name.starts_with(gossamer_hir::LIFTED_CLOSURE_PREFIX)
            || body.arity == 0
            || !destination.projection.is_empty()
            || (destination.local.0 as usize) >= n_locals
        {
            return None;
        }
        match args.first() {
            Some(Operand::Copy(base)) if base.projection.is_empty() && base.local == Local(1) => {
                Some(destination.local.0 as usize)
            }
            _ => None,
        }
    };

    let vec_borrow_agg = {
        let mut get_ptr_dest = vec![false; n_locals];
        for block in &body.blocks {
            if let Some(dest) = capture_env_load(block) {
                get_ptr_dest[dest] = true;
            }
            if let Terminator::Call {
                callee: Operand::Const(ConstValue::Str(name)),
                destination,
                ..
            } = &block.terminator
                && destination.projection.is_empty()
                && (destination.local.0 as usize) < n_locals
                && name == "gos_rt_vec_get_ptr"
            {
                get_ptr_dest[destination.local.0 as usize] = true;
            }
        }
        // A whole-local assignment that is neither a bare `Copy` nor the
        // get_ptr terminator gives the local an owned value - disqualify.
        // Collect the copy sources so the fixpoint can require every one
        // to itself be a borrow.
        let mut disqualified = vec![false; n_locals];
        let mut copy_srcs: Vec<Vec<usize>> = vec![Vec::new(); n_locals];
        for block in &body.blocks {
            for stmt in &block.stmts {
                if let StatementKind::Assign { place, rvalue } = &stmt.kind
                    && place.projection.is_empty()
                    && (place.local.0 as usize) < n_locals
                {
                    let i = place.local.0 as usize;
                    match rvalue {
                        Rvalue::Use(Operand::Copy(src))
                            if src.projection.is_empty() && (src.local.0 as usize) < n_locals =>
                        {
                            copy_srcs[i].push(src.local.0 as usize);
                        }
                        // The address of a part of a borrowed element - the
                        // aggregate half of a destructured `(k, v)` binding -
                        // names the same storage the element does, so it is a
                        // borrow exactly as the element pointer is.
                        Rvalue::BinaryOp {
                            op: BinOp::Add,
                            lhs: Operand::Copy(src),
                            rhs: Operand::Const(ConstValue::Int(_)),
                        } if src.projection.is_empty() && (src.local.0 as usize) < n_locals => {
                            copy_srcs[i].push(src.local.0 as usize);
                        }
                        _ => disqualified[i] = true,
                    }
                }
            }
            // A non-get_ptr call destination is an owned result.
            if let Terminator::Call { destination, .. } = &block.terminator
                && destination.projection.is_empty()
                && (destination.local.0 as usize) < n_locals
                && !get_ptr_dest[destination.local.0 as usize]
            {
                disqualified[destination.local.0 as usize] = true;
            }
        }
        let mut borrow = get_ptr_dest.clone();
        let mut changed = true;
        while changed {
            changed = false;
            for i in 0..n_locals {
                if borrow[i] || disqualified[i] || copy_srcs[i].is_empty() {
                    continue;
                }
                if copy_srcs[i].iter().all(|&s| borrow[s]) {
                    borrow[i] = true;
                    changed = true;
                }
            }
        }
        borrow
    };

    // By-value aggregate locals (struct / tuple, not a parameter / region)
    // carrying RC fields that need per-field retain (on copy) + release (on
    // drop), since the stack-slot aggregate itself has no heap teardown.
    let agg_locals: Vec<(usize, AggFieldPaths)> = ((arity + 1)..n_locals)
        .filter(|&i| {
            !body.locals[i].region
                && !extraction_seed[i]
                && !vec_borrow_agg[i]
                && !enum_child_borrow[i]
        })
        .filter_map(|i| {
            let fields = agg_rc_fields(body.locals[i].ty);
            if fields.is_empty() {
                None
            } else {
                Some((i, fields))
            }
        })
        .collect();

    // Locals that BORROW an aggregate carrying RC fields. A store through one
    // reaches the borrowed aggregate's own field, so the field's previous
    // value is released and the stored one retained exactly as on an owned
    // aggregate. Their fields are never released at return: the borrow owns
    // nothing.
    // By-value aggregate PARAMETERS carrying RC fields.
    //
    // The frame's copy of one is a shallow copy of its slots, so it shares
    // every field's heap value with the caller without a share of its own.
    // Everything else already treats such a frame's struct as owning its
    // fields - a field store through a `&mut` receiver releases the old
    // value and retains the new one - so the copy has to own them too, or
    // the release frees a value the caller still holds and the retained
    // one is never freed at all. Entry retains and a return release are
    // what make the two agree.
    //
    // Kept apart from `agg_locals` because a parameter must NOT be
    // zero-initialised: its slots arrive holding the caller's values.
    //
    // A parameter the body only reads is left out: the caller holds every
    // field for the whole call, so the pair would cancel, and a `Map`
    // field's retain copies the whole table.
    let param_agg_locals: Vec<(usize, AggFieldPaths)> = (1..=arity.min(n_locals.saturating_sub(1)))
        .filter(|&i| {
            !body.locals[i].region
                && !matches!(
                    tcx.kind_of(body.locals[i].ty),
                    gossamer_types::TyKind::Ref { .. }
                )
        })
        .filter_map(|i| {
            let fields = agg_rc_fields(body.locals[i].ty);
            (!fields.is_empty()).then_some((i, fields))
        })
        .filter(|(i, fields)| {
            !param_fields_only_read(body, Local(u32::try_from(*i).unwrap_or(0)), fields)
        })
        .collect();

    // Dests of a bare `dest = Copy(src)` whose source is an aggregate carrying
    // RC fields, but which the ownership sets exclude (a loop's by-value
    // element extract, classified as a borrow of the container's storage).
    // The struct-copy retain below mints each heap field's share on them
    // exactly as on an owned local, so they take the RELEASE half of the
    // schedule too - zero-init, a release before a reassignment or a
    // projected overwrite, and a release at return - one release per mint.
    // They stay outside the retain-only arms keyed to the owned sets.
    let borrow_copy_agg_locals: Vec<(usize, AggFieldPaths)> = {
        let owned: std::collections::HashSet<usize> = agg_locals
            .iter()
            .chain(param_agg_locals.iter())
            .map(|(l, _)| *l)
            .collect();
        let mut minted: Vec<usize> = Vec::new();
        for block in &body.blocks {
            for stmt in &block.stmts {
                if let StatementKind::Assign {
                    place,
                    rvalue: Rvalue::Use(Operand::Copy(src)),
                } = &stmt.kind
                    && place.projection.is_empty()
                    && src.projection.is_empty()
                    // The return place hands its fields to the caller, whose
                    // frame owns them from the copy on: a release here would
                    // free what the caller was just given.
                    && place.local.0 != 0
                    && (place.local.0 as usize) < n_locals
                    && (src.local.0 as usize) < n_locals
                    && !owned.contains(&(place.local.0 as usize))
                    // The same views the struct-copy retain skips: no share is
                    // minted for one, so none is owed back.
                    && !vec_borrow_agg[place.local.0 as usize]
                    && !extraction_seed[place.local.0 as usize]
                    && !enum_child_borrow[place.local.0 as usize]
                    && !body.locals[place.local.0 as usize].region
                    && !agg_rc_fields(body.locals[src.local.0 as usize].ty).is_empty()
                    && !minted.contains(&(place.local.0 as usize))
                {
                    minted.push(place.local.0 as usize);
                }
            }
        }
        minted
            .into_iter()
            .filter_map(|i| {
                let fields = agg_rc_fields(body.locals[i].ty);
                (!fields.is_empty()).then_some((i, fields))
            })
            .collect()
    };

    let ref_agg_locals: Vec<(usize, AggFieldPaths)> = (0..n_locals)
        .filter(|&i| {
            !body.locals[i].region
                && matches!(
                    tcx.kind_of(body.locals[i].ty),
                    gossamer_types::TyKind::Ref { .. }
                )
        })
        .filter_map(|i| {
            let fields = agg_rc_fields(pointee_of(tcx, body.locals[i].ty));
            (!fields.is_empty()).then_some((i, fields))
        })
        .collect();

    // An `Ok(v)` / `Err(v)` wrap of a payload the frame does not own owes that
    // payload's fields a share: the wrap copies the aggregate's words, and the
    // consumer's own bindings release what it hands them. None of the sets
    // above records that work - it is keyed on the payload being an extraction,
    // not on any local the pass classifies as owned - and a sibling match arm
    // built by a call is enough to leave every one of them empty, so the exit
    // has to ask for it directly. A payload the frame BUILT is excluded: it
    // owns its fields already, and a share minted for it would have no
    // releaser.
    let owes_extracted_payload_retain = body.blocks.iter().flat_map(|b| &b.stmts).any(|stmt| {
        let StatementKind::Assign {
            rvalue: Rvalue::CallIntrinsic { name, args },
            ..
        } = &stmt.kind
        else {
            return false;
        };
        *name == "gos_rt_result_new"
            && args.iter().any(|op| {
                matches!(op, Operand::Copy(src)
                    if src.projection.is_empty()
                        && (src.local.0 as usize) < n_locals
                        && extraction_seed[src.local.0 as usize]
                        && !agg_rc_fields(body.locals[src.local.0 as usize].ty).is_empty())
            })
    });
    if releasable.is_empty()
        && retain_sites.is_empty()
        && terminator_retains.is_empty()
        && agg_locals.is_empty()
        && param_agg_locals.is_empty()
        && ref_agg_locals.is_empty()
        && returned_map_field_sites.is_empty()
        && !owes_extracted_payload_retain
    {
        return;
    }

    let releasable_set: std::collections::HashSet<u32> = releasable.iter().map(|l| l.0).collect();
    // An RC owner whose value reaches the return slot on some path still holds
    // its share of every value a reassignment replaces, and of the value it
    // holds at a return that does not hand it to the caller. Only a return
    // that copies it straight into the slot moves that share out.
    let returned_owners: Vec<Local> = (0..n_locals)
        .filter(|&i| {
            (is_rc(i) || rebound[i])
                && owned[i]
                && !moved[i]
                && flows_to_return[i]
                && !enum_child_borrow[i]
        })
        .map(|i| Local(u32::try_from(i).unwrap_or(0)))
        .collect();
    let overwrite_releasable: std::collections::HashSet<u32> = releasable_set
        .iter()
        .copied()
        .chain(returned_owners.iter().map(|l| l.0))
        .collect();
    let n_blocks = body.blocks.len();

    // Per-block, per-gap insertions. `gaps[b][g]` lists the retain/
    // release calls to emit just before the original statement at index
    // `g` (gap `len` = just before the terminator). Building all
    // insertions against the *original* indices and then rebuilding each
    // block in one pass keeps positions valid regardless of how many
    // statements are inserted.
    let mut gaps: Vec<Vec<Vec<(bool, Local)>>> = body
        .blocks
        .iter()
        .map(|b| vec![Vec::new(); b.stmts.len() + 1])
        .collect();

    // Parallel to `gaps`, but each entry is (is_retain, local, field_index,
    // is_weak) - a retain/release of one RC field of a by-value aggregate.
    let mut field_gaps: Vec<Vec<Vec<FieldGap>>> = body
        .blocks
        .iter()
        .map(|b| vec![Vec::new(); b.stmts.len() + 1])
        .collect();

    for (bi, si) in &returned_map_field_sites {
        field_gaps[*bi][si + 1].push((true, Local::RETURN, Vec::new(), FieldRcKind::Map));
    }

    for bi in 0..n_blocks {
        let len = body.blocks[bi].stmts.len();
        // Release before each stmt-position reassignment of an owner - for
        // ANY rvalue, not just `gos_rc_alloc`. A named binding rebound in a
        // loop (`let t = build(d)`, where the build result is `Copy`-ed into
        // `t`) must release the previous iteration's value before it is
        // overwritten, or every iteration's value leaks until the function
        // returns. The entry zero-init makes the first release (of the
        // null initial value) safe; on the loop back-edge the incoming value
        // is the previous iteration's owned object, which is then freed.
        for (si, stmt) in body.blocks[bi].stmts.iter().enumerate() {
            if let StatementKind::Assign { place, .. } = &stmt.kind
                && place.projection.is_empty()
                && overwrite_releasable.contains(&place.local.0)
                && !copyback_sites.contains(&(bi, si))
            {
                gaps[bi][si].push((false, place.local));
            }
        }
        // Release before a Call-terminator reassignment of an owner - unless
        // the call *consumes* the old value of that same local. The in-place
        // string builder `s = gos_rt_str_concat_drop_a(s, frag)` reads `s`,
        // appends in place (or reallocates and frees the old buffer), and
        // returns the result: it already owns/frees the old `s`, so releasing
        // it here would read freed memory and double-free.
        if let Terminator::Call {
            destination,
            callee,
            args,
            ..
        } = &body.blocks[bi].terminator
            && destination.projection.is_empty()
            && overwrite_releasable.contains(&destination.local.0)
        {
            let self_consuming = matches!(callee, Operand::Const(ConstValue::Str(n)) if is_self_consuming_append(n))
                && matches!(args.first(), Some(Operand::Copy(p)) if p.projection.is_empty() && p.local == destination.local);
            if !self_consuming {
                gaps[bi][len].push((false, destination.local));
            }
        }
        // Retain element/value before a consuming container/channel call.
        // (recorded in `terminator_retains`)
        // Release every owner at each return.
        if matches!(body.blocks[bi].terminator, Terminator::Return) {
            for &local in &releasable {
                gaps[bi][len].push((false, local));
            }
            for &local in &returned_owners {
                let moved_out = body.blocks[bi].stmts.iter().any(|stmt| {
                    matches!(
                        &stmt.kind,
                        StatementKind::Assign {
                            place,
                            rvalue: Rvalue::Use(Operand::Copy(src)),
                        } if place.local == Local::RETURN
                            && place.projection.is_empty()
                            && src.local == local
                            && src.projection.is_empty()
                    )
                });
                if !moved_out {
                    gaps[bi][len].push((false, local));
                }
            }
        }
    }
    // Retain each acquisition. For whole-local reassignment of an owner, mint
    // the replacement share before releasing the previous value. The source
    // can be a child borrowed from that previous value, as in
    // `cursor = next` while walking a recursive list. Releasing `cursor`
    // first recursively reclaims `next`, so a retain after the copy reads a
    // dangling pointer. Other acquisition forms retain after the statement as
    // before.
    for (bi, si, local, count) in &retain_sites {
        let retain_gap = if matches!(
            body.blocks[*bi].stmts.get(*si),
            Some(Statement {
                kind:
                    StatementKind::Assign {
                        place,
                        rvalue: Rvalue::Use(Operand::Copy(src)),
                    },
                ..
            }) if place.projection.is_empty()
                && src.projection.is_empty()
                && overwrite_releasable.contains(&place.local.0)
        ) {
            *si
        } else {
            *si + 1
        };
        for _ in 0..*count {
            gaps[*bi][retain_gap].push((true, *local));
        }
    }
    // Retain consuming-call arguments just before the terminator.
    for (bi, local) in &terminator_retains {
        let len = body.blocks[*bi].stmts.len();
        gaps[*bi][len].push((true, *local));
    }
    for (bi, local, path, kind) in &terminator_field_retains {
        let len = body.blocks[*bi].stmts.len();
        field_gaps[*bi][len].push((true, *local, path.clone(), *kind));
    }

    // Locals a call answers into. A container constructor's own answer is
    // freed by `insert_container_frees`, and a by-value parameter by the
    // caller, so an aggregate handed one as a `Map` field operand would name
    // the table its source frees. The field takes a table of its own instead -
    // a map cannot be co-owned. Keyed on the destination FIELD's kind rather
    // than the source local's type: a collection constructor normalises its
    // destination to the handle word.
    let call_dest = call_destinations(body);

    // Locals holding a word read out of an owned aggregate's field. The
    // aggregate still frees that field at its own death, so a store handing
    // the word on has to give the destination a value container of its own.
    let borrows_owned_field: Vec<bool> = {
        let owners: std::collections::HashSet<usize> = agg_locals
            .iter()
            .chain(param_agg_locals.iter())
            .chain(ref_agg_locals.iter())
            .chain(borrow_copy_agg_locals.iter())
            .map(|(l, _)| *l)
            .collect();
        let mut out = vec![false; n_locals];
        for block in &body.blocks {
            for stmt in &block.stmts {
                if let StatementKind::Assign {
                    place,
                    rvalue: Rvalue::Use(Operand::Copy(src)),
                } = &stmt.kind
                    && place.projection.is_empty()
                    && (place.local.0 as usize) < n_locals
                    && !src.projection.is_empty()
                    && src
                        .projection
                        .iter()
                        .all(|p| matches!(p, crate::ir::Projection::Field(_)))
                    && owners.contains(&(src.local.0 as usize))
                {
                    out[place.local.0 as usize] = true;
                }
            }
        }
        out
    };

    // An aggregate taken out of a carrier whose box owns the payload's children
    // (the structural meta) is a copy of words the box keeps, so a destination
    // that releases its fields at death takes shares of its own first.
    let owns_extracted_payload = |l: Local| {
        let i = l.0 as usize;
        i < n_locals
            && agg_locals
                .iter()
                .chain(borrow_copy_agg_locals.iter())
                .any(|(owner, _)| *owner == i)
            && tcx
                .rc_meta(&format!(
                    "gos_rc_meta_boxaggr_{}",
                    body.locals[i].ty.as_u32()
                ))
                .is_some()
    };

    // Field-level retain/release for by-value aggregate locals: release the
    // previous value's RC fields before any reassignment (null-safe on the
    // first assignment via the entry zero-init), retain the shared fields after
    // a struct copy, and release every aggregate's fields at return.
    for (bi, block) in body.blocks.iter().enumerate() {
        let len = block.stmts.len();
        for (si, stmt) in block.stmts.iter().enumerate() {
            // Projected field store `agg.field = value` on a managed
            // aggregate local: release the field's previous buffer
            // before the store and retain the stored value after it.
            // The RHS temp keeps its own cleanup (ctor-free / scope
            // release) and the aggregate's field-death free owns the
            // new share, so each reference is freed exactly once.
            if let StatementKind::Assign { place, rvalue } = &stmt.kind
                && !place.projection.is_empty()
                && place
                    .projection
                    .iter()
                    .all(|p| matches!(p, crate::ir::Projection::Field(_)))
                && (agg_locals.iter().any(|(l, _)| *l == place.local.0 as usize)
                    || param_agg_locals
                        .iter()
                        .any(|(l, _)| *l == place.local.0 as usize)
                    || ref_agg_locals
                        .iter()
                        .any(|(l, _)| *l == place.local.0 as usize)
                    || borrow_copy_agg_locals
                        .iter()
                        .any(|(l, _)| *l == place.local.0 as usize))
            {
                // A source whose single consuming read is this store hands
                // its own reference to the field: the retain that would mint
                // a second one is dropped for the same reason the whole-local
                // move drops it, and the field's death frees the one share
                // that arrived.
                let source_moved = matches!(
                    rvalue,
                    Rvalue::Use(Operand::Copy(src))
                        if src.projection.is_empty()
                            && (src.local.0 as usize) < moved.len()
                            && moved[src.local.0 as usize]
                );
                // A map cannot be co-owned, so the after-store "retain" is a
                // clone of the field's own. It is owed only when the source
                // local keeps a release of the original - a source with no
                // booked release hands its map over, and a clone would strand
                // the moved-in one with no owner.
                let map_source_kept = matches!(
                    rvalue,
                    Rvalue::Use(Operand::Copy(src))
                        if src.projection.is_empty()
                            && ((src.local.0 as usize) < n_locals
                                && (releasable_set.contains(&src.local.0)
                                    || borrows_owned_field[src.local.0 as usize]))
                );
                let path: Vec<u32> = place
                    .projection
                    .iter()
                    .map(|p| match p {
                        crate::ir::Projection::Field(i) => *i,
                        _ => 0,
                    })
                    .collect();
                // A store whose path names a nested aggregate writes every RC
                // leaf beneath it, so each one is what the gap pair is owed:
                // the old leaf goes back before the store and the destination
                // takes a share of the new one after it. Keying only on an
                // exact leaf match left `outer.inner = Inner::new(..)`
                // balancing nothing, so the source's own end-of-scope release
                // freed what the field had just been handed.
                let leaves: Vec<(Vec<u32>, FieldRcKind)> = agg_locals
                    .iter()
                    .chain(param_agg_locals.iter())
                    .chain(ref_agg_locals.iter())
                    .chain(borrow_copy_agg_locals.iter())
                    .find(|(l, _)| *l == place.local.0 as usize)
                    .map(|(_, fields)| {
                        fields
                            .iter()
                            .filter(|(p, _)| p.starts_with(path.as_slice()))
                            .map(|(p, k)| (p.clone(), *k))
                            .collect()
                    })
                    .unwrap_or_default();
                for (leaf, kind) in leaves {
                    field_gaps[bi][si].push((false, place.local, leaf.clone(), kind));
                    // A leaf reached below the stored path belongs to the
                    // sub-aggregate the source still releases at its own
                    // death, so the destination takes a share of every kind -
                    // a value container's being a table of its own.
                    let nested = leaf.len() > path.len();
                    let wants_retain = if kind.is_value_container() && !nested {
                        map_source_kept
                    } else {
                        !source_moved
                    };
                    if wants_retain {
                        // The field's share is minted right here, so the
                        // value is named once more than the frame accounts
                        // for whenever the copy already minted one of its own
                        // or the frame keeps no release to balance it.
                        if matches!(kind, FieldRcKind::Vec | FieldRcKind::Rc)
                            && !nested
                            && let Rvalue::Use(Operand::Copy(src)) = rvalue
                            && src.projection.is_empty()
                            && (src.local.0 as usize) < n_locals
                        {
                            let copy_minted = retain_sites
                                .iter()
                                .any(|&(rb, rs, l, _)| rb == bi && rs == si && l == src.local);
                            let frame_keeps = releasable_set.contains(&src.local.0)
                                || vec_released.contains(&src.local.0)
                                || borrows_owned_field[src.local.0 as usize];
                            if copy_minted || !frame_keeps {
                                gaps[bi][si + 1].push((false, src.local));
                            }
                        }
                        field_gaps[bi][si + 1].push((true, place.local, leaf, kind));
                    }
                }
            }
            if let StatementKind::Assign { place, rvalue } = &stmt.kind
                && place.projection.is_empty()
            {
                // Release the previous value's RC fields before reassigning an
                // owned aggregate local (null-safe first time via zero-init).
                if let Some((_, fields)) = agg_locals
                    .iter()
                    .chain(borrow_copy_agg_locals.iter())
                    .find(|(l, _)| *l == place.local.0 as usize)
                {
                    for (f, w) in fields {
                        field_gaps[bi][si].push((false, place.local, f.clone(), *w));
                    }
                }
                // Struct copy `dest = Copy(src)` where `src` is an aggregate:
                // `dest` shares each RC field pointer, so retain them after the
                // copy. Keyed on the SOURCE being an aggregate (not on `dest`
                // being a managed local) so a copy into the return slot - which
                // transfers the value to the caller while the source local is
                // released at this return - keeps the fields alive.
                //
                // A destination that is a view of storage someone else owns - a
                // container element, a lifted closure's environment slot - is
                // excluded: it releases nothing, so a share minted here would
                // have no releaser, and a `Map` field's share is a clone of the
                // whole table.
                //
                // A region local is a view too: loop-region eligibility keeps
                // its source alive for the whole iteration, and the region
                // releases nothing at its death, so it takes no share.
                if let Rvalue::Use(Operand::Copy(src)) = rvalue
                    && src.projection.is_empty()
                    && (src.local.0 as usize) < body.locals.len()
                    && !body.locals[place.local.0 as usize].region
                    && !vec_borrow_agg[place.local.0 as usize]
                    && !extraction_seed[place.local.0 as usize]
                    && !enum_child_borrow[place.local.0 as usize]
                {
                    for (f, w) in agg_rc_fields(body.locals[src.local.0 as usize].ty) {
                        field_gaps[bi][si + 1].push((true, place.local, f, w));
                    }
                }
                // `dest = Repeat(Copy(src))` fills every slot with the same
                // words, so each slot names the source aggregate's field
                // pointers. The destination releases each slot's fields at its
                // death, so every slot takes a share of its own here - a value
                // container a table of its own, exactly as the whole-aggregate
                // copy above does. A repeated value that IS the managed thing
                // rather than an aggregate holding one is minted per slot by
                // the operand pass instead, which is what the empty field walk
                // says.
                if let Rvalue::Repeat {
                    value: Operand::Copy(src),
                    ..
                } = rvalue
                    && src.projection.is_empty()
                    && (src.local.0 as usize) < body.locals.len()
                    && !agg_rc_fields(body.locals[src.local.0 as usize].ty).is_empty()
                    && !body.locals[place.local.0 as usize].region
                    && !vec_borrow_agg[place.local.0 as usize]
                    && !extraction_seed[place.local.0 as usize]
                    && !enum_child_borrow[place.local.0 as usize]
                {
                    for (f, w) in agg_rc_fields(body.locals[place.local.0 as usize].ty) {
                        field_gaps[bi][si + 1].push((true, place.local, f, w));
                    }
                }
                // Sub-aggregate field extract `dest = Copy(src.field)` where
                // the extracted value is itself a by-value struct/tuple: `dest`
                // becomes its own agg-local and releases its nested RC fields at
                // death, so it must retain its share here. Keyed on DEST's type
                // (the extracted sub-aggregate). A direct RC-field extract (dest
                // is a `String`/`Vec`) has no aggregate RC fields, so this is a
                // no-op there - those are retained by the owned-extract path.
                // A `Deref` step is the same sharing copy: `dest = Copy(*r)`
                // reads the struct the reference names into a local that owns
                // its own share of what the words point at.
                if let Rvalue::Use(Operand::Copy(src)) = rvalue
                    && !src.projection.is_empty()
                    && src.projection.iter().all(|p| {
                        matches!(
                            p,
                            crate::ir::Projection::Field(_) | crate::ir::Projection::Deref
                        )
                    })
                    && place.projection.is_empty()
                    && (place.local.0 as usize) < body.locals.len()
                    && !body.locals[place.local.0 as usize].region
                {
                    for (f, w) in agg_rc_fields(body.locals[place.local.0 as usize].ty) {
                        field_gaps[bi][si + 1].push((true, place.local, f, w));
                    }
                }
                // Aggregate construction `dest = Aggregate[.., Copy(src), ..]`
                // whose operand copies a by-value aggregate: the new struct's
                // slot shares each of `src`'s RC field pointers, so retain them
                // (mirrors the whole-local struct-copy retain above). The shared
                // pointers are reached through `src` itself - a one-level
                // projection equivalent to the new aggregate's nested slot - so
                // the source's at-death release is balanced by the new owner's.
                if let Rvalue::Aggregate { operands, .. } = rvalue {
                    for op in operands {
                        if let Operand::Copy(src) = op
                            && src.projection.is_empty()
                            && (src.local.0 as usize) < body.locals.len()
                            && !body.locals[place.local.0 as usize].region
                        {
                            for (f, w) in agg_rc_fields(body.locals[src.local.0 as usize].ty) {
                                field_gaps[bi][si + 1].push((true, src.local, f, w));
                            }
                        }
                    }
                    // A bare `Map` operand whose source is a call's own answer:
                    // that source frees the table it holds, so the field takes
                    // one of its own rather than a second owner of the same.
                    if place.projection.is_empty() {
                        for (idx, op) in operands.iter().enumerate() {
                            let Operand::Copy(src) = op else { continue };
                            if !src.projection.is_empty()
                                || (src.local.0 as usize) >= n_locals
                                || !container_source_keeps_own(body, src.local, &call_dest)
                            {
                                continue;
                            }
                            let path = vec![u32::try_from(idx).unwrap_or(0)];
                            let owns = agg_locals
                                .iter()
                                .chain(param_agg_locals.iter())
                                .chain(borrow_copy_agg_locals.iter())
                                .find(|(l, _)| *l == place.local.0 as usize)
                                .and_then(|(_, fields)| {
                                    fields.iter().find_map(|(p, k)| {
                                        (*p == path && k.is_value_container()).then_some(*k)
                                    })
                                });
                            if let Some(kind) = owns {
                                field_gaps[bi][si + 1].push((true, place.local, path, kind));
                            }
                        }
                    }
                }
                // `Ok(v)` / `Err(v)` / `Some(v)` with a by-value aggregate
                // payload: `gos_rt_result_new` heap-copies the aggregate's
                // words, so the payload copy shares each of the source's RC /
                // Vec field pointers. The extraction site (`?`, `unwrap`,
                // `unwrap_or`) hands those fields to the consumer's own
                // bindings, whose at-death releases balance the payload's
                // share - so retain the fields here, after the wrap, leaving
                // them alive past the source aggregate's own field release.
                // A payload boxed under its structural meta is a counted blob
                // that retains the payload's children itself, and the box's
                // release gives them back, so no share is minted for it here.
                if let Rvalue::CallIntrinsic { name, .. } = rvalue
                    && *name == "gos_rt_result_payload"
                    && owns_extracted_payload(place.local)
                {
                    for (f, w) in agg_rc_fields(body.locals[place.local.0 as usize].ty) {
                        field_gaps[bi][si + 1].push((true, place.local, f, w));
                    }
                }
                if let Rvalue::CallIntrinsic { name, args } = rvalue
                    && *name == "gos_rt_result_new"
                {
                    let box_owns_children = |ty: gossamer_types::Ty| {
                        tcx.rc_meta(&format!("gos_rc_meta_boxaggr_{}", ty.as_u32()))
                            .is_some()
                    };
                    for op in args {
                        if let Operand::Copy(src) = op
                            && src.projection.is_empty()
                            && (src.local.0 as usize) < body.locals.len()
                            && !box_owns_children(body.locals[src.local.0 as usize].ty)
                        {
                            for (f, w) in agg_rc_fields(body.locals[src.local.0 as usize].ty) {
                                field_gaps[bi][si + 1].push((true, src.local, f, w));
                            }
                        }
                    }
                }
            }
        }
        if matches!(block.terminator, Terminator::Return) {
            for (li, fields) in agg_locals
                .iter()
                .chain(param_agg_locals.iter())
                .chain(borrow_copy_agg_locals.iter())
            {
                for (f, w) in fields {
                    field_gaps[bi][len].push((
                        false,
                        Local(u32::try_from(*li).unwrap_or(0)),
                        f.clone(),
                        *w,
                    ));
                }
            }
        }
        // A call that reassigns an owned aggregate local (`h = make()`) must
        // release the previous value's RC fields first - the statement-position
        // release above only sees `Assign`, not a call-terminator destination.
        if let Terminator::Call { destination, .. } = &block.terminator
            && destination.projection.is_empty()
            && let Some((_, fields)) = agg_locals
                .iter()
                .chain(borrow_copy_agg_locals.iter())
                .find(|(l, _)| *l == destination.local.0 as usize)
        {
            for (f, w) in fields {
                field_gaps[bi][len].push((false, destination.local, f.clone(), *w));
            }
        }
        // A slot load into an owned aggregate local - the `(k, v)` a loop
        // binds from a sequence element - copies words the container still
        // owns, so the binding takes its own share of every RC field the copy
        // now names. The release booked for the local at its death balances
        // it. The retain lands at the head of the call's successor, where the
        // loaded value first exists.
        if let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            destination,
            target: Some(succ),
            ..
        } = &block.terminator
            && name == "gos_load"
            && destination.projection.is_empty()
            && let Some((_, fields)) = agg_locals
                .iter()
                .find(|(l, _)| *l == destination.local.0 as usize)
        {
            let succ = succ.0 as usize;
            for (f, w) in fields {
                field_gaps[succ][0].push((true, destination.local, f.clone(), *w));
            }
        }
        // `unwrap` answers the words a carrier's box keeps, so an owning
        // destination takes its shares where the value first exists.
        if let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            destination,
            target: Some(succ),
            ..
        } = &block.terminator
            && matches!(
                name.as_str(),
                "gos_rt_result_unwrap" | "gos_rt_option_unwrap"
            )
            && destination.projection.is_empty()
            && owns_extracted_payload(destination.local)
        {
            for (f, w) in agg_rc_fields(body.locals[destination.local.0 as usize].ty) {
                field_gaps[succ.0 as usize][0].push((true, destination.local, f, w));
            }
        }
    }

    // The frame's own share of each by-value aggregate parameter's RC
    // fields, taken before anything reads them. Pushed at the entry
    // block's first gap so it precedes every use, including a field store
    // in the entry block itself.
    let has_entry_gap = field_gaps.first().is_some_and(|block| !block.is_empty());
    for (li, fields) in param_agg_locals.iter().filter(|_| has_entry_gap) {
        for (f, w) in fields {
            field_gaps[0][0].push((true, Local(u32::try_from(*li).unwrap_or(0)), f.clone(), *w));
        }
    }

    // Pre-allocate one unit-typed local per emitted retain/release call.
    let total_calls: usize = gaps.iter().flatten().map(Vec::len).sum::<usize>()
        + field_gaps.iter().flatten().map(Vec::len).sum::<usize>();
    let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
    let mut next_unit = body.locals.len();
    for _ in 0..total_calls {
        body.locals.push(LocalDecl {
            ty: unit_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
    }

    // Rebuild each block: zero-init owners at entry, then interleave the
    // gap insertions with the original statements.
    for bi in 0..n_blocks {
        let span = body.blocks[bi].span;
        let orig: Vec<Statement> = std::mem::take(&mut body.blocks[bi].stmts);
        let block_gaps = std::mem::take(&mut gaps[bi]);
        let block_field_gaps = std::mem::take(&mut field_gaps[bi]);
        let mut new_stmts: Vec<Statement> = Vec::with_capacity(orig.len() + total_calls);
        // Entry block: zero-init releasable owners so every release is
        // null-safe regardless of the path taken to it.
        if bi == 0 {
            for &local in releasable.iter().chain(&returned_owners) {
                new_stmts.push(Statement {
                    kind: StatementKind::Assign {
                        place: Place::local(local),
                        rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                    },
                    span,
                    inlined: None,
                });
            }
            // Zero-init each aggregate local's RC field slots so the
            // release-before-reassignment reads null (a no-op) on the first
            // assignment instead of dereferencing an uninitialised slot.
            for (li, fields) in agg_locals.iter().chain(borrow_copy_agg_locals.iter()) {
                for (f, _) in fields {
                    new_stmts.push(Statement {
                        kind: StatementKind::Assign {
                            place: Place {
                                local: Local(u32::try_from(*li).unwrap_or(0)),
                                projection: f
                                    .iter()
                                    .map(|idx| crate::ir::Projection::Field(*idx))
                                    .collect(),
                            },
                            rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                        },
                        span,
                        inlined: None,
                    });
                }
            }
        }
        let mut orig_iter = orig.into_iter();
        for g in 0..block_gaps.len() {
            // Emit retains before releases at each gap: a value copied
            // out (e.g. into the return slot) must be retained before the
            // at-return releases of its aliasing locals, or those
            // releases would free it before the caller's reference is
            // minted.
            for pass_retain in [true, false] {
                for &(is_retain, local) in &block_gaps[g] {
                    if is_retain != pass_retain {
                        continue;
                    }
                    // A `Weak<T>` local is weak-counted: route its
                    // retain/release through the weak helpers so the
                    // payload's strong lifetime is unaffected and the
                    // allocation frees only when both counts reach zero.
                    let name = if (local.0 as usize) < body.locals.len() {
                        rc_helper(tcx, body.locals[local.0 as usize].ty, is_retain)
                    } else if is_retain {
                        "gos_rt_rc_retain"
                    } else {
                        "gos_rt_rc_release"
                    };
                    let dest = Local(u32::try_from(next_unit).expect("local overflow"));
                    next_unit += 1;
                    new_stmts.push(rc_call_stmt(name, dest, local, span));
                }
                for (is_retain, local, path, kind) in &block_field_gaps[g] {
                    if *is_retain != pass_retain {
                        continue;
                    }
                    let name = match (*is_retain, *kind) {
                        (true, FieldRcKind::Rc) => "gos_rt_rc_retain",
                        (false, FieldRcKind::Rc) => "gos_rt_rc_release",
                        (true, FieldRcKind::Weak) => "gos_rt_rc_weak_retain",
                        (false, FieldRcKind::Weak) => "gos_rt_rc_weak_release",
                        (true, FieldRcKind::Vec) => "gos_rt_vec_retain",
                        (false, FieldRcKind::Vec) => "gos_rt_vec_free",
                        (true, FieldRcKind::Carrier { .. }) => "gos_rt_result_payload_retain",
                        (false, FieldRcKind::Carrier { .. }) => "gos_rt_result_payload_release",
                        (retain, other) => match other.value_container_helpers() {
                            Some((clone, release)) => {
                                if retain {
                                    clone
                                } else {
                                    release
                                }
                            }
                            None => unreachable!("every non-value-container kind is matched above"),
                        },
                    };
                    let dest = Local(u32::try_from(next_unit).expect("local overflow"));
                    next_unit += 1;
                    let mut stmt = field_rc_call_stmt(name, dest, *local, path, span);
                    push_carrier_kinds(&mut stmt, *kind);
                    new_stmts.push(stmt);
                }
            }
            if let Some(stmt) = orig_iter.next() {
                new_stmts.push(stmt);
            }
        }
        body.blocks[bi].stmts = new_stmts;
    }
}

/// Runtime calls that take ownership of an RC-managed argument (it
/// outlives the call), so the argument is a move, not a borrow. Missing
/// one would free a value the container/channel still references; an
/// extra one only leaks. Keep this list complete for RC-managed payloads.
/// Runtime calls that hand back an owned `String` the frame must release.
///
/// Two families answer one: a shim the ABI registry declares `mints_string`
/// (its Rust signature is `-> *mut c_char`, a fresh allocation), and the
/// carrier accessors below, whose `String` answer is the payload's single
/// reference handed over rather than a new allocation - the carrier never
/// releases one, so the frame that takes the value is the one that gives it
/// back. `unwrap_or` / `unwrap_or_else` retain a fallback that becomes the
/// answer, so both arms answer one owned share.
///
/// Deliberately EXCLUDES `gos_rt_str_concat_drop_a` and the in-place append
/// family, which answer their accumulator itself, and `gos_rt_result_payload`
/// (the payload may already be owned by its binding). A missing entry only
/// leaks; a wrong one double-frees.
fn mints_owned_string(name: &str) -> bool {
    matches!(
        name,
        "gos_rt_result_unwrap_or_str"
            | "gos_rt_result_unwrap_or_node"
            | "gos_rt_option_unwrap"
            | "gos_rt_result_unwrap"
            | "gos_rt_option_default_with"
            | "gos_rt_result_default_with"
    ) || gossamer_abi::mints_owned_string(name)
}

/// Whether `name` answers a fresh `errors::Error` cell whose one share the
/// caller holds.
fn mints_owned_error(name: &str) -> bool {
    matches!(
        name,
        "gos_rt_error_new" | "gos_rt_error_from" | "gos_rt_error_wrap" | "gos_rt_error_with_field"
    )
}

/// True when the body only READS a by-value aggregate parameter: nothing
/// writes its slots, its words are never copied out whole, and no value read
/// out of it reaches a call, container, reference, or global that keeps it
/// past the call.
///
/// Such a parameter needs no share of its own. The caller holds every field
/// for the whole call, so the frame's entry retain and its return release
/// cancel, and eliding the pair is what keeps a read-only accessor on a
/// `Map`-carrying struct proportional to the work it does: a `GosMap` has no
/// reference count, so its retain copies the entire table.
///
/// Handing the parameter whole to another Gossamer function is such a read:
/// the callee books its own share if it needs one, so a nested by-value `self`
/// costs the lookups it performs rather than a copy of the table they read.
///
/// `rc_fields` names the parameter's heap-managed field paths, and only a
/// place that can reach one of them is judged at all: a scalar field owns
/// nothing, so putting one in a tuple, a struct literal, or a container says
/// nothing about who owns the table beside it.
///
/// Conservative by construction - every use the walk does not recognise as a
/// plain read answers `false`, and the parameter keeps its own share.
fn param_fields_only_read(body: &Body, p: Local, rc_fields: &AggFieldPaths) -> bool {
    use crate::ir::{Operand, Projection, Rvalue, StatementKind, Terminator};

    // True when a place rooted in `p` can reach one of its heap-managed
    // fields: the whole parameter can, a projection can when it lies on the
    // path to such a field or runs through one, and a projection this walk
    // cannot read as a field path is assumed to. A projection that reaches
    // only scalar slots carries nothing the frame could have to own, so the
    // uses below judge it as they judge an unrelated local.
    let reaches_rc_field = |projection: &[Projection]| {
        let mut path = Vec::with_capacity(projection.len());
        for step in projection {
            match step {
                Projection::Field(f) => path.push(*f),
                Projection::Discriminant => return false,
                _ => return true,
            }
        }
        rc_fields.iter().any(|(field, _)| {
            field.starts_with(path.as_slice()) || path.starts_with(field.as_slice())
        })
    };
    // A place rooted in `p` whose value shares one of its heap fields.
    let touches_rc = |pl: &crate::ir::Place| pl.local == p && reaches_rc_field(&pl.projection);

    // Locals holding a heap-managed value read out of `p`'s slots, through
    // bare copies. Whatever they reach, the parameter's field reaches.
    let mut carries: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut changed = true;
    while changed {
        changed = false;
        for block in &body.blocks {
            for stmt in &block.stmts {
                let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                    continue;
                };
                if !place.projection.is_empty() {
                    continue;
                }
                let Rvalue::Use(Operand::Copy(src)) = rvalue else {
                    continue;
                };
                let carried = if src.local == p {
                    !src.projection.is_empty() && reaches_rc_field(&src.projection)
                } else {
                    src.projection.is_empty() && carries.contains(&src.local.0)
                };
                if carried && carries.insert(place.local.0) {
                    changed = true;
                }
            }
        }
    }

    // Any read of one of the parameter's heap fields, or of a local carrying
    // one.
    let reads = |op: &Operand| match op {
        Operand::Copy(pl) => touches_rc(pl) || carries.contains(&pl.local.0),
        _ => false,
    };
    // The parameter's words copied out whole: the copy names the same heap
    // values under a second owner, and every sharing rule downstream keys on
    // one, so the frame must own what it hands over.
    let copies_whole =
        |op: &Operand| matches!(op, Operand::Copy(pl) if pl.local == p && pl.projection.is_empty());
    // A call that keeps what it is handed, or one whose callee this walk
    // cannot name.
    let keeps_args = |callee: &Operand| match callee {
        Operand::Const(ConstValue::Str(name)) => {
            is_consuming_call(name) || stores_aggregate_by_pointer(name)
        }
        Operand::FnRef { .. } => false,
        _ => true,
    };
    // A Gossamer callee, as opposed to a runtime symbol. Its own drop pass
    // books whatever share its by-value aggregate parameter needs, into
    // parameter storage that is its frame's rather than the argument's, so
    // handing the whole parameter to one transfers no ownership: this frame
    // outlives the nested call, and whoever owns the fields still owns them
    // across it. A runtime symbol's contract is per-symbol instead, so a
    // whole hand-over to one keeps the share.
    let user_callee = |callee: &Operand| match callee {
        Operand::FnRef { .. } => true,
        Operand::Const(ConstValue::Str(name)) => {
            !name.starts_with("gos_rt_") && name != "gos_load" && name != "gos_store"
        }
        _ => false,
    };

    for block in &body.blocks {
        for stmt in &block.stmts {
            match &stmt.kind {
                StatementKind::Assign { place, rvalue } => {
                    if place.local == p {
                        return false;
                    }
                    // A store through a projection hands the value to whatever
                    // the destination is part of.
                    let stores_into_slot = !place.projection.is_empty();
                    match rvalue {
                        Rvalue::Use(op) => {
                            if copies_whole(op) || (stores_into_slot && reads(op)) {
                                return false;
                            }
                        }
                        Rvalue::UnaryOp { operand: op, .. }
                        | Rvalue::Cast { operand: op, .. }
                        | Rvalue::Repeat { value: op, .. } => {
                            if copies_whole(op) || (stores_into_slot && reads(op)) {
                                return false;
                            }
                        }
                        Rvalue::BinaryOp { lhs, rhs, .. } => {
                            if [lhs, rhs]
                                .iter()
                                .any(|op| copies_whole(op) || (stores_into_slot && reads(op)))
                            {
                                return false;
                            }
                        }
                        Rvalue::Len(_) | Rvalue::StaticLoad(_) => {}
                        Rvalue::Ref { place: pl, .. } => {
                            if pl.local == p || carries.contains(&pl.local.0) {
                                return false;
                            }
                        }
                        Rvalue::Aggregate { operands, .. } => {
                            if operands.iter().any(&reads) {
                                return false;
                            }
                        }
                        Rvalue::CallIntrinsic { name, args } => {
                            // The field helpers take the ADDRESS of the place
                            // they are handed and write the slot back, so one
                            // over the parameter is a store, not a read.
                            let keeps = is_consuming_call(name)
                                || stores_aggregate_by_pointer(name)
                                || name.starts_with("gos_store")
                                || name.contains("_field_clone")
                                || name.contains("_field_release");
                            if args.iter().any(|op| {
                                copies_whole(op) || ((keeps || stores_into_slot) && reads(op))
                            }) {
                                return false;
                            }
                        }
                    }
                }
                StatementKind::StorageLive(_)
                | StatementKind::StorageDead(_)
                | StatementKind::Nop => {}
                StatementKind::SetDiscriminant { place, .. } => {
                    if place.local == p {
                        return false;
                    }
                }
                StatementKind::StaticStore { value, .. } => {
                    if reads(value) {
                        return false;
                    }
                }
                StatementKind::IterSource { dst, source, .. } => {
                    if dst.local == p || reads(source) {
                        return false;
                    }
                }
                StatementKind::IterAdapter {
                    dst,
                    upstream,
                    closure_or_arg,
                    ..
                } => {
                    if dst.local == p
                        || upstream.local == p
                        || carries.contains(&upstream.local.0)
                        || closure_or_arg.as_ref().is_some_and(&reads)
                    {
                        return false;
                    }
                }
                StatementKind::IterNext {
                    dst_option,
                    iter_place,
                    ..
                } => {
                    if dst_option.local == p
                        || iter_place.local == p
                        || carries.contains(&iter_place.local.0)
                    {
                        return false;
                    }
                }
            }
        }
        match &block.terminator {
            Terminator::Call {
                callee,
                args,
                destination,
                ..
            } => {
                if destination.local == p {
                    return false;
                }
                let keeps = keeps_args(callee);
                let hands_over = !user_callee(callee);
                if args
                    .iter()
                    .any(|op| (hands_over && copies_whole(op)) || (keeps && reads(op)))
                {
                    return false;
                }
            }
            Terminator::Drop { place, .. } => {
                if place.local == p || carries.contains(&place.local.0) {
                    return false;
                }
            }
            Terminator::Goto { .. }
            | Terminator::Return
            | Terminator::SwitchInt { .. }
            | Terminator::Assert { .. }
            | Terminator::Unreachable
            | Terminator::Panic { .. } => {}
        }
    }
    true
}

/// A call the second argument's heap ownership moves through: a container
/// push, whatever container it is. The element store owns the pushed value
/// from then on, so the frame must not free it independently.
pub(crate) fn is_element_push(name: &str) -> bool {
    name.starts_with("gos_rt_vec_push")
        || name.starts_with("gos_rt_deque_push")
        || (name.starts_with("gos_rt_bheap_") && name.contains("_push"))
}

/// Consuming calls that mint the container's own share of a stored `Vec` and
/// give it back at the container's teardown.
///
/// The exchange is balanced on both sides, so the frame keeps the release of
/// the sequence it built and reclaims it per site rather than only at the
/// return - which is what a container filled in a loop needs.
fn stores_owned_vec_value(name: &str) -> bool {
    is_element_push(name)
        || name.starts_with("gos_rt_map_insert")
        || name.starts_with("gos_rt_map_or_insert")
        || name.starts_with("gos_rt_omap_insert")
        || name.starts_with("gos_rt_set_insert")
        || name.starts_with("gos_rt_ovec_insert")
        || name.starts_with("gos_rt_vec_insert")
}

/// Consuming calls whose container keeps the ARGUMENT'S OWN aggregate word,
/// so a struct argument's heap fields need a share for the stored entry.
///
/// A hash container stores the word it is handed. A sequence container copies
/// the element's slots into its own storage and retains their heap children
/// itself, so a share minted here would never be given back - the entry reads
/// correctly either way, and the extra count leaks. Membership is therefore
/// evidence-driven: a container belongs here only where dropping the share
/// leaves a stored entry reading freed memory.
fn stores_aggregate_by_pointer(name: &str) -> bool {
    name.starts_with("gos_rt_map_insert")
        || name.starts_with("gos_rt_map_or_insert")
        || name.starts_with("gos_rt_omap_insert")
        || name.starts_with("gos_rt_set_insert")
        || name.starts_with("gos_rt_chan_send")
}

pub(crate) fn is_consuming_call(name: &str) -> bool {
    is_element_push(name)
        // `xs[i] = v` writes the value into the element store, which owns its
        // elements from then on, so the store mints the container's share the
        // way a push does.
        || name.starts_with("gos_rt_vec_set_i64")
        || name.starts_with("gos_rt_vec_set_i128")
        || name.starts_with("gos_rt_vec_insert")
        || name.starts_with("gos_rt_set_insert")
        || name.starts_with("gos_rt_map_insert")
        // `HashMap::or_insert` consumes its key and, on an absent key,
        // stores the supplied value. The retained value share becomes the
        // map's ownership; the returned value is separately marked as an
        // interior borrow by `returns_borrowed_pointer`.
        || name.starts_with("gos_rt_map_or_insert")
        || name.starts_with("gos_rt_omap_insert")
        || name.starts_with("gos_rt_ovec_insert")
        || name.starts_with("gos_rt_chan_send")
        // `option.ok_or(err)` packs the error word into the carrier it
        // answers without taking a share of its own, so the carrier's
        // payload release is the error cell's only give-back.
        || name == "gos_rt_result_ok_or"
}

/// Picks the retain/release runtime helper for a heap value by its type. Vecs
/// carry no RC header, so they route through the Vec allocator's reference
/// count (`gos_rt_vec_retain` / `gos_rt_vec_free`); `Weak<T>` routes through the
/// weak helpers; compiler-typed strings route through typed string helpers so
/// generated cleanup does not lock the public raw-string registry; everything
/// else uses the generic `gos_rt_rc_retain` / `gos_rt_rc_release`.
fn rc_helper(
    tcx: &gossamer_types::TyCtxt,
    ty: gossamer_types::Ty,
    is_retain: bool,
) -> &'static str {
    use gossamer_types::TyKind;
    match tcx.kind_of(ty) {
        TyKind::String => {
            if is_retain {
                "gos_rt_str_retain_typed"
            } else {
                "gos_rt_str_free_typed"
            }
        }
        // A whole-local `Array` gap only arises for vec-carried arrays
        // (monomorphised `[T; N]` parameters); inline fixed arrays never
        // enter the retain/release schedule.
        TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. } => {
            if is_retain {
                "gos_rt_vec_retain"
            } else {
                "gos_rt_vec_free"
            }
        }
        _ if tcx.is_weak_ty(ty) => {
            if is_retain {
                "gos_rt_rc_weak_retain"
            } else {
                "gos_rt_rc_weak_release"
            }
        }
        _ => {
            if is_retain {
                "gos_rt_rc_retain"
            } else {
                "gos_rt_rc_release"
            }
        }
    }
}

/// Builds a `gos_rt_rc_retain` / `gos_rt_rc_release` call on one RC field of a
/// by-value aggregate local (`local.field_idx`).
fn field_rc_call_stmt(
    name: &'static str,
    dest: Local,
    local: Local,
    field_path: &[u32],
    span: gossamer_lex::Span,
) -> Statement {
    Statement {
        kind: StatementKind::Assign {
            place: Place::local(dest),
            rvalue: Rvalue::CallIntrinsic {
                name,
                args: vec![Operand::Copy(Place {
                    local,
                    projection: field_path
                        .iter()
                        .map(|idx| crate::ir::Projection::Field(*idx))
                        .collect(),
                })],
            },
        },
        span,
        inlined: None,
    }
}

/// Appends a carrier field's arm kinds to the payload call `stmt` makes on it;
/// every other field kind's call takes the field alone.
pub(crate) fn push_carrier_kinds(stmt: &mut Statement, kind: FieldRcKind) {
    if let FieldRcKind::Carrier { ok, err } = kind
        && let StatementKind::Assign {
            rvalue: Rvalue::CallIntrinsic { args, .. },
            ..
        } = &mut stmt.kind
    {
        args.push(Operand::Const(ConstValue::Int(i128::from(ok))));
        args.push(Operand::Const(ConstValue::Int(i128::from(err))));
    }
}

fn rc_call_stmt(
    name: &'static str,
    dest: Local,
    local: Local,
    span: gossamer_lex::Span,
) -> Statement {
    Statement {
        kind: StatementKind::Assign {
            place: Place::local(dest),
            rvalue: Rvalue::CallIntrinsic {
                name,
                args: vec![Operand::Copy(Place::local(local))],
            },
        },
        span,
        inlined: None,
    }
}

/// Deterministic reclamation for escaped value-aggregate heap copies.
///
/// The LLVM backend heap-copies a multi-slot struct that flows into a
/// `Some(..)`/`Ok(..)`/`Err(..)` payload (`gos_rt_rc_alloc_copy`, an RC
/// blob in the copy-blob provenance set). This pass gives every holder
/// of such a payload pointer exactly one share:
///
/// - an option-typed local (`{disc, payload}` by value) is a holder: it
///   retains after every initialisation except the `gos_rt_result_new`
///   mint itself and call destinations (the callee's return-copy mints
///   the caller's share), and releases before reassignment and at
///   return;
/// - a guarded slot of a stack aggregate is a holder: the aggregate
///   retains its children after every whole-local initialisation
///   (construction operands keep their own shares) and releases them
///   before reassignment, before a call-destination overwrite, and at
///   return;
/// - an option field store (`s.next = o`, directly or through a
///   reference) releases the slot's previous payload and retains the
///   new one in place;
/// - entry blocks zero the guarded slots and option locals so the first
///   release never reads stack garbage.
///
/// Every retain/release the runtime performs is gated on the copy-blob
/// provenance set, so pointers produced by anything other than
/// `gos_rt_rc_alloc_copy` (map gets, borrows, the Cranelift tier's
/// construction-allocated aggregates) are never touched: a missed entry
/// can only leak, never corrupt.
pub(crate) fn insert_aggr_copy_drops(body: &mut Body, tcx: &gossamer_types::TyCtxt) {
    use gossamer_types::TyKind;
    let n_locals = body.locals.len();
    if n_locals == 0 {
        return;
    }
    let arity = body.arity as usize;

    // A guarded meta symbol with at least one (gate, disc, payload) entry.
    let walk_meta = |ty: gossamer_types::Ty| -> Option<String> {
        let sym = tcx.aggr_copy_meta(ty)?;
        let blob = tcx.rc_meta(sym)?;
        if blob.len() >= 2 && blob[1] > 0 {
            Some(sym.to_string())
        } else {
            None
        }
    };
    let guarded_locals: Vec<(Local, String)> = ((arity + 1)..n_locals)
        .filter(|&i| !body.locals[i].region)
        .filter_map(|i| {
            walk_meta(body.locals[i].ty).map(|sym| (Local(u32::try_from(i).unwrap_or(0)), sym))
        })
        .collect();
    // The return slot participates in retains only: a return-copy mints
    // the caller's share (released by the caller), but the slot itself
    // is never released here.
    let retain_meta_of = |l: Local| -> Option<String> {
        let i = l.0 as usize;
        if i >= n_locals || body.locals[i].region || (1..=arity).contains(&i) {
            return None;
        }
        walk_meta(body.locals[i].ty)
    };

    // By-value Option/Result locals whose payload type registered a
    // copy-blob meta on either side.
    let is_guarded_option = |ty: gossamer_types::Ty| -> bool {
        match tcx.kind_of(ty) {
            TyKind::Adt { def, substs } if def.local == u32::MAX || def.local == u32::MAX - 1 => {
                substs
                    .types()
                    .iter()
                    .take(2)
                    .any(|p| tcx.aggr_copy_meta(*p).is_some())
            }
            _ => false,
        }
    };
    // A runtime call answering its aggregate payload as a counted blob hands
    // the frame the only share of that blob, so the destination holds it the
    // way a carrier the frame built does.
    let mut counted_answer = vec![false; n_locals];
    for block in &body.blocks {
        if let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            destination,
            ..
        } = &block.terminator
            && destination.projection.is_empty()
            && (destination.local.0 as usize) < n_locals
            && answers_counted_blob(name, destination.local, body, tcx)
        {
            counted_answer[destination.local.0 as usize] = true;
        }
    }
    let option_holder = |l: Local| -> bool {
        let i = l.0 as usize;
        i > arity
            && i < n_locals
            && !body.locals[i].region
            && (is_guarded_option(body.locals[i].ty) || counted_answer[i])
    };
    // `result_new` destinations whose payload type carries a copy-blob
    // meta are guarded option holders even when the typer left the
    // destination's type unresolved (`Ok(S { .. })` through a `Var`
    // temp): without the classification, the temp's sweep release is
    // never emitted and the payload blob leaves the function one count
    // high - pinned in the collector buffer, one leak per call.
    let mut mint_holders = vec![false; n_locals];
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign { place, rvalue } = &stmt.kind
                && place.projection.is_empty()
                && (place.local.0 as usize) < n_locals
                && let Rvalue::CallIntrinsic { name, args } = rvalue
                && (*name == "gos_rt_result_new" || *name == "gos_rt_result_new_f64")
                && let Some(Operand::Copy(pp)) = args.get(1)
                && pp.projection.is_empty()
                && (pp.local.0 as usize) < n_locals
                && tcx
                    .aggr_copy_meta(body.locals[pp.local.0 as usize].ty)
                    .is_some()
            {
                mint_holders[place.local.0 as usize] = true;
            }
        }
    }
    let option_holders: Vec<Local> = ((arity + 1)..n_locals)
        .filter(|&i| {
            !body.locals[i].region
                && (is_guarded_option(body.locals[i].ty) || mint_holders[i] || counted_answer[i])
        })
        .map(|i| Local(u32::try_from(i).unwrap_or(0)))
        .collect();

    // A field store whose base resolves (through references) to a type
    // with a guarded meta, assigning an option-typed value: the slot's
    // old payload is released and the new one retained in place.
    let peel_ref = |mut ty: gossamer_types::Ty| -> gossamer_types::Ty {
        while let TyKind::Ref { inner, .. } = tcx.kind_of(ty) {
            ty = *inner;
        }
        ty
    };
    // The type a chain of field projections reaches, seeing through a
    // reference at each step. `None` when the path leaves the layout this
    // walk understands.
    let projected_field_ty = |base: gossamer_types::Ty,
                              projection: &[crate::ir::Projection]|
     -> Option<gossamer_types::Ty> {
        let mut ty = peel_ref(base);
        for step in projection {
            let next = match step {
                crate::ir::Projection::Field(idx) => match tcx.kind_of(ty) {
                    TyKind::Adt { def, substs } => tcx
                        .adt_field_tys(*def, substs)
                        .and_then(|tys| tys.get(*idx as usize).copied()),
                    TyKind::Tuple(elems) => elems.get(*idx as usize).copied(),
                    TyKind::Array { elem, len } if (*idx as usize) < len.to_usize() => Some(*elem),
                    _ => None,
                },
                crate::ir::Projection::Index(_) => match tcx.kind_of(ty) {
                    TyKind::Array { elem, .. } | TyKind::Vec(elem) | TyKind::Slice(elem) => {
                        Some(*elem)
                    }
                    _ => None,
                },
                crate::ir::Projection::Deref
                | crate::ir::Projection::Downcast(_)
                | crate::ir::Projection::Discriminant => None,
            }?;
            ty = peel_ref(next);
        }
        Some(ty)
    };
    // The by-value `{disc, payload}` carrier itself, whatever it holds. The
    // slot helpers read the payload word beside the discriminant, so the
    // address handed to one has to name a two-word carrier and nothing else:
    // pointed at a single-word field they would read the field beside it.
    let is_option_slot_ty = |ty: gossamer_types::Ty| -> bool {
        matches!(
            tcx.kind_of(peel_ref(ty)),
            TyKind::Adt { def, .. } if def.local == u32::MAX || def.local == u32::MAX - 1
        )
    };
    // A store through `&mut Option<T>` reaches the caller's carrier: the
    // slot it names takes a share of the payload exactly as an aggregate's
    // own carrier field does.
    let is_carrier_deref_store = |place: &Place, rvalue: &Rvalue| -> bool {
        if place.projection.as_slice() != [crate::ir::Projection::Deref] {
            return false;
        }
        let i = place.local.0 as usize;
        if i >= n_locals {
            return false;
        }
        let TyKind::Ref { inner, .. } = tcx.kind_of(body.locals[i].ty) else {
            return false;
        };
        is_option_slot_ty(*inner) && !matches!(rvalue, Rvalue::Use(Operand::Const(_)))
    };
    let is_option_field_store = |place: &Place, rvalue: &Rvalue| -> bool {
        if is_carrier_deref_store(place, rvalue) {
            return true;
        }
        if place.projection.is_empty()
            || !place.projection.iter().all(|p| {
                matches!(
                    p,
                    crate::ir::Projection::Field(_) | crate::ir::Projection::Index(_)
                )
            })
        {
            return false;
        }
        let i = place.local.0 as usize;
        if i >= n_locals {
            return false;
        }
        // The destination has to be the carrier itself. A store into any
        // other field or element of the same aggregate is an ordinary write.
        let Some(slot_ty) = projected_field_ty(body.locals[i].ty, &place.projection) else {
            return false;
        };
        if !is_option_slot_ty(slot_ty) {
            return false;
        }
        // An element of a sequence of carriers takes its own share the way
        // an aggregate's carrier field does: the sequence owns what it
        // holds, so the temporary the store copied from is free to release
        // its own on the next overwrite and at the return sweep.
        if walk_meta(peel_ref(body.locals[i].ty)).is_none() && !is_guarded_option(slot_ty) {
            return false;
        }
        match rvalue {
            Rvalue::Use(Operand::Copy(src)) if src.projection.is_empty() => {
                option_holder(src.local) || is_guarded_option(body.locals[src.local.0 as usize].ty)
            }
            Rvalue::Use(_) | Rvalue::CallIntrinsic { .. } => {
                // Any other store into a carrier slot replaces whatever it
                // held, so the old payload is released and the new one
                // retained; a payload outside the copy-blob set no-ops.
                true
            }
            _ => false,
        }
    };

    let mut gaps: Vec<Vec<Vec<Statement>>> = body
        .blocks
        .iter()
        .map(|b| vec![Vec::new(); b.stmts.len() + 1])
        .collect();
    let mut next_unit = body.locals.len();
    let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
    let mut extra_locals = 0usize;
    let call_stmt = |name: &'static str,
                     args: Vec<Operand>,
                     span: gossamer_lex::Span,
                     next_unit: &mut usize,
                     extra: &mut usize|
     -> Statement {
        let dest = Local(u32::try_from(*next_unit).expect("local overflow"));
        *next_unit += 1;
        *extra += 1;
        Statement {
            kind: StatementKind::Assign {
                place: Place::local(dest),
                rvalue: Rvalue::CallIntrinsic { name, args },
            },
            span,
            inlined: None,
        }
    };
    let walk_args = |l: Local, sym: &str| -> Vec<Operand> {
        vec![
            Operand::Copy(Place::local(l)),
            Operand::Const(ConstValue::Str(sym.to_string())),
        ]
    };

    for (bi, block) in body.blocks.iter().enumerate() {
        let len = block.stmts.len();
        let span = block.span;
        for (si, stmt) in block.stmts.iter().enumerate() {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            if place.projection.is_empty() {
                // Whole-local (re)initialisation of a guarded aggregate:
                // release the previous children, retain the new ones.
                if let Some((_, sym)) = guarded_locals.iter().find(|(l, _)| *l == place.local) {
                    gaps[bi][si].push(call_stmt(
                        "gos_rt_aggr_release_children",
                        walk_args(place.local, sym),
                        span,
                        &mut next_unit,
                        &mut extra_locals,
                    ));
                }
                if let Some(sym) = retain_meta_of(place.local)
                    && !matches!(rvalue, Rvalue::Use(Operand::Const(_)))
                {
                    gaps[bi][si + 1].push(call_stmt(
                        "gos_rt_aggr_retain_children",
                        walk_args(place.local, &sym),
                        span,
                        &mut next_unit,
                        &mut extra_locals,
                    ));
                }
                // Whole-local (re)initialisation of an option holder.
                if option_holder(place.local) || place.local == Local::RETURN {
                    let holder_ty_ok = if place.local == Local::RETURN {
                        is_guarded_option(body.locals[0].ty)
                    } else {
                        true
                    };
                    if holder_ty_ok {
                        if option_holder(place.local) {
                            gaps[bi][si].push(call_stmt(
                                "gos_rt_option_slot_release",
                                vec![Operand::Copy(Place::local(place.local))],
                                span,
                                &mut next_unit,
                                &mut extra_locals,
                            ));
                        }
                        let is_mint = matches!(
                            rvalue,
                            Rvalue::CallIntrinsic { name, .. }
                                if *name == "gos_rt_result_new"
                                    || *name == "gos_rt_result_new_f64"
                        );
                        let is_const = matches!(rvalue, Rvalue::Use(Operand::Const(_)));
                        if !is_mint && !is_const {
                            gaps[bi][si + 1].push(call_stmt(
                                "gos_rt_option_slot_retain",
                                vec![Operand::Copy(Place::local(place.local))],
                                span,
                                &mut next_unit,
                                &mut extra_locals,
                            ));
                        }
                    }
                }
            } else if is_option_field_store(place, rvalue) {
                // Overwriting an owning option slot in place: release the
                // old payload, store, retain the new one. The helpers read
                // the payload word beside the discriminant, so they take the
                // slot's address: a reference already is that address, while
                // a field names it through the aggregate.
                let slot = if place.projection.as_slice() == [crate::ir::Projection::Deref] {
                    Place::local(place.local)
                } else {
                    place.clone()
                };
                gaps[bi][si].push(call_stmt(
                    "gos_rt_option_slot_release",
                    vec![Operand::Copy(slot.clone())],
                    span,
                    &mut next_unit,
                    &mut extra_locals,
                ));
                gaps[bi][si + 1].push(call_stmt(
                    "gos_rt_option_slot_retain",
                    vec![Operand::Copy(slot)],
                    span,
                    &mut next_unit,
                    &mut extra_locals,
                ));
            }
        }
        // A container store takes its own share of the carrier's payload:
        // the element lives as long as the container, while the local the
        // value came from is still released on its next overwrite and by
        // the return sweep. `xs[i] = v` also drops what the element held.
        // A by-value parameter is lent, so the caller keeps its share and the
        // container takes one of its own exactly as from a local.
        if let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            args,
            ..
        } = &block.terminator
            && is_consuming_call(name)
        {
            for arg in args.iter().skip(1) {
                let Operand::Copy(p) = arg else { continue };
                let lent_param = (1..=arity).contains(&(p.local.0 as usize))
                    && (p.local.0 as usize) < n_locals
                    && is_guarded_option(body.locals[p.local.0 as usize].ty);
                if !p.projection.is_empty() || !(option_holder(p.local) || lent_param) {
                    continue;
                }
                if name.starts_with("gos_rt_vec_set")
                    && let Some(Operand::Copy(recv)) = args.first()
                    && let Some(Operand::Copy(index)) = args.get(1)
                    && recv.projection.is_empty()
                    && index.projection.is_empty()
                    && p.local != index.local
                {
                    let element = Place {
                        local: recv.local,
                        projection: vec![crate::ir::Projection::Index(index.local)].into(),
                    };
                    gaps[bi][len].push(call_stmt(
                        "gos_rt_option_slot_release",
                        vec![Operand::Copy(element)],
                        span,
                        &mut next_unit,
                        &mut extra_locals,
                    ));
                }
                gaps[bi][len].push(call_stmt(
                    "gos_rt_option_slot_retain",
                    vec![Operand::Copy(Place::local(p.local))],
                    span,
                    &mut next_unit,
                    &mut extra_locals,
                ));
            }
        }
        // A call destination is minted by the callee: release the old
        // value, never retain the new one.
        if let Terminator::Call { destination, .. } = &block.terminator
            && destination.projection.is_empty()
        {
            if let Some((_, sym)) = guarded_locals.iter().find(|(l, _)| *l == destination.local) {
                gaps[bi][len].push(call_stmt(
                    "gos_rt_aggr_release_children",
                    walk_args(destination.local, sym),
                    span,
                    &mut next_unit,
                    &mut extra_locals,
                ));
            }
            if option_holder(destination.local) {
                gaps[bi][len].push(call_stmt(
                    "gos_rt_option_slot_release",
                    vec![Operand::Copy(Place::local(destination.local))],
                    span,
                    &mut next_unit,
                    &mut extra_locals,
                ));
            }
        }
        // `unwrap` copies out the words of a payload its carrier keeps, and the
        // carrier gives those words' children back when it is released, so a
        // destination that releases children of its own takes its shares once
        // the call has answered. A carrier this frame releases is a holder; a
        // carrier parameter is released by the caller.
        if let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            args,
            destination,
            target: Some(target),
            ..
        } = &block.terminator
            && matches!(
                name.as_str(),
                "gos_rt_result_unwrap" | "gos_rt_option_unwrap"
            )
            && destination.projection.is_empty()
            && let Some(Operand::Copy(carrier)) = args.first()
            && carrier.projection.is_empty()
            && (carrier.local.0 as usize) < n_locals
            && (option_holder(carrier.local)
                || ((1..=arity).contains(&(carrier.local.0 as usize))
                    && is_guarded_option(body.locals[carrier.local.0 as usize].ty)))
            && let Some(sym) = retain_meta_of(destination.local)
            && let Some(head) = gaps.get_mut(target.0 as usize).and_then(|g| g.first_mut())
        {
            head.push(call_stmt(
                "gos_rt_aggr_retain_children",
                walk_args(destination.local, &sym),
                span,
                &mut next_unit,
                &mut extra_locals,
            ));
        }
        // A carrier read out of a box is a view of words the box keeps, so
        // the destination takes its own share once the call has answered.
        if let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            destination,
            target: Some(target),
            ..
        } = &block.terminator
            && destination.projection.is_empty()
            && option_holder(destination.local)
            && reads_boxed_carrier(name)
            && let Some(head) = gaps.get_mut(target.0 as usize).and_then(|g| g.first_mut())
        {
            head.push(call_stmt(
                "gos_rt_option_slot_retain",
                vec![Operand::Copy(Place::local(destination.local))],
                span,
                &mut next_unit,
                &mut extra_locals,
            ));
        }
        if matches!(block.terminator, Terminator::Return) {
            for (l, sym) in &guarded_locals {
                gaps[bi][len].push(call_stmt(
                    "gos_rt_aggr_release_children",
                    walk_args(*l, sym),
                    span,
                    &mut next_unit,
                    &mut extra_locals,
                ));
            }
            for l in &option_holders {
                gaps[bi][len].push(call_stmt(
                    "gos_rt_option_slot_release",
                    vec![Operand::Copy(Place::local(*l))],
                    span,
                    &mut next_unit,
                    &mut extra_locals,
                ));
            }
        }
    }

    if extra_locals == 0 && guarded_locals.is_empty() && option_holders.is_empty() {
        return;
    }

    // Entry-block zeroing: guarded slots via the runtime walk, option
    // holders via a plain zero store (both {disc, payload} words).
    let mut entry_inits: Vec<Statement> = Vec::new();
    if let Some(first) = body.blocks.first() {
        let span = first.span;
        for (l, sym) in &guarded_locals {
            entry_inits.push(call_stmt(
                "gos_rt_aggr_zero_guarded",
                walk_args(*l, sym),
                span,
                &mut next_unit,
                &mut extra_locals,
            ));
        }
        for l in &option_holders {
            entry_inits.push(Statement {
                kind: StatementKind::Assign {
                    place: Place::local(*l),
                    rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                },
                span,
                inlined: None,
            });
        }
    }

    for _ in 0..extra_locals {
        body.locals.push(LocalDecl {
            ty: unit_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
    }

    let n_blocks = body.blocks.len();
    for bi in 0..n_blocks {
        let orig: Vec<Statement> = std::mem::take(&mut body.blocks[bi].stmts);
        let block_gaps = std::mem::take(&mut gaps[bi]);
        let mut new_stmts: Vec<Statement> = Vec::with_capacity(orig.len() + 4);
        if bi == 0 {
            new_stmts.append(&mut entry_inits);
        }
        let mut orig_iter = orig.into_iter();
        for g in 0..block_gaps.len() {
            new_stmts.extend(block_gaps[g].iter().cloned());
            if let Some(stmt) = orig_iter.next() {
                new_stmts.push(stmt);
            }
        }
        body.blocks[bi].stmts = new_stmts;
    }
}

// Slot-child kinds, mirroring `gossamer_runtime::c_abi::vec::vec_elem_kind`
// (kept in sync by value). `RC_NODE` covers user enum / struct heap
// pointers (tag-bit-encoded) released via `gos_rt_rc_release`.
const SLOT_KIND_STRING: i64 = 1;
const SLOT_KIND_VEC: i64 = 2;
const SLOT_KIND_MAP: i64 = 3;
const SLOT_KIND_RC_NODE: i64 = 7;
const SLOT_KIND_SET: i64 = 11;
const SLOT_HASH_SET_DEF_LOCAL: u32 = u32::MAX - 7;
const SLOT_BTREE_SET_DEF_LOCAL: u32 = u32::MAX - 18;

/// Walks the flat slot layout of a by-value aggregate `ty`, appending one
/// `(gate, disc_word, word, kind)` entry per RC child pointer the vec must
/// own. `gate` is `-1` for an unconditional pointer field, or the
/// discriminant value gating an `Option`/`Result` payload word. Sets
/// `has_direct` when an unconditional (non-`Option`/`Result`) RC field is
/// present - the signal that the element needs the `AGGR_OWNED` path
/// rather than the copy-blob-only `AGGR_GUARDED` path. Recurses through
/// nested inline struct / tuple fields at absolute word offsets.
fn collect_slot_rc_children(
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
fn collect_field_rc(
    tcx: &gossamer_types::TyCtxt,
    fty: gossamer_types::Ty,
    word: i64,
    depth: u32,
    out: &mut Vec<(i64, i64, i64, i64)>,
    has_direct: &mut bool,
) {
    use gossamer_types::TyKind;
    match tcx.kind_of(fty) {
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
        TyKind::Adt { def, .. }
            if matches!(
                def.local,
                SLOT_HASH_SET_DEF_LOCAL | SLOT_BTREE_SET_DEF_LOCAL
            ) =>
        {
            out.push((-1, 0, word, SLOT_KIND_SET));
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
                    TyKind::String => Some(SLOT_KIND_STRING),
                    TyKind::Vec(_) | TyKind::Slice(_) => Some(SLOT_KIND_VEC),
                    TyKind::HashMap { .. } => Some(SLOT_KIND_MAP),
                    TyKind::Adt { def, .. }
                        if matches!(
                            def.local,
                            SLOT_HASH_SET_DEF_LOCAL | SLOT_BTREE_SET_DEF_LOCAL
                        ) =>
                    {
                        Some(SLOT_KIND_SET)
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
}

impl ElemOwnership {
    /// The runtime symbol that tags an element store with this ownership.
    pub(crate) fn symbol(&self) -> &'static str {
        match self {
            Self::Guarded(_) => "gos_rt_vec_set_elem_meta",
            Self::Owned(_) => "gos_rt_vec_set_slot_children",
            Self::RcElems => "gos_rt_vec_mark_rc_elems",
            Self::VecElems => "gos_rt_vec_mark_vec_elems",
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
fn ensure_slot_children_meta(
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
    if let gossamer_types::TyKind::Adt { def, .. } = tcx.kind_of(elem)
        && matches!(
            def.local,
            SLOT_HASH_SET_DEF_LOCAL | SLOT_BTREE_SET_DEF_LOCAL
        )
    {
        children.push((-1, 0, 0, SLOT_KIND_SET));
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
pub(crate) fn insert_vec_elem_metas(body: &mut Body, tcx: &mut gossamer_types::TyCtxt) {
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
        MapBlob,
        MapVec,
        MapFloatKeys,
        MapOrdered { unsigned: bool },
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
        // The backend copies an aggregate value into a blob whenever EITHER
        // meta is registered - it reads the structural one first and falls
        // back to the guarded copy meta - so the map has to be tagged as
        // holding blob values under exactly the same condition. Tagging is
        // what makes the entry take its own share and give it back at the
        // entry's death; under-tagging leaves the stored blob owned by the
        // inserting frame alone, which then frees it out from under the map.
        let structural = format!("gos_rc_meta_boxaggr_{}", value.as_u32());
        if tcx.aggr_copy_meta(*value).is_some()
            || tcx.rc_meta(&structural).is_some()
            || tcx.is_counted_node(*value)
        {
            Some(VecMeta::MapBlob)
        } else if let TyKind::Vec(elem) | TyKind::Slice(elem) = tcx.kind_of(*value) {
            // A byte sequence is stored as the bytes themselves - the insert
            // copies them out and the entry owns no handle - so tagging the
            // map as holding vec shares would release something no entry
            // holds. Every other element keeps its handle, and its share.
            let bytes = matches!(tcx.kind_of(*elem), TyKind::Int(gossamer_types::IntTy::U8));
            (!bytes).then_some(VecMeta::MapVec)
        } else {
            None
        }
    };

    // A float-keyed map sorts its keys by value in every ordered traversal.
    let map_float_keys = |l: Local| -> Option<VecMeta> {
        let ty = body.locals.get(l.0 as usize)?.ty;
        let TyKind::HashMap { key, .. } = tcx.kind_of(ty) else {
            return None;
        };
        matches!(tcx.kind_of(*key), TyKind::Float(_)).then_some(VecMeta::MapFloatKeys)
    };

    // A `BTreeMap` keeps its entries in the ordered tree.
    let map_ordered = |l: Local| -> Option<VecMeta> {
        let ty = body.locals.get(l.0 as usize)?.ty;
        let TyKind::HashMap {
            key, ordered: true, ..
        } = tcx.kind_of(ty)
        else {
            return None;
        };
        let unsigned = matches!(
            tcx.kind_of(*key),
            TyKind::Int(gossamer_types::IntTy::U64 | gossamer_types::IntTy::Usize)
        );
        Some(VecMeta::MapOrdered { unsigned })
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
        // dedicated `STRING` kind; `Weak` elements are not strong owners.
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
    let mk =
        |l: Local, meta: &VecMeta, span: gossamer_lex::Span, next_unit: &mut usize| -> Statement {
            let dest = Local(u32::try_from(*next_unit).expect("local overflow"));
            *next_unit += 1;
            let rvalue = match meta {
                VecMeta::MapBlob => Rvalue::CallIntrinsic {
                    name: "gos_rt_map_set_blob_values",
                    args: vec![Operand::Copy(Place::local(l))],
                },
                VecMeta::MapVec => Rvalue::CallIntrinsic {
                    name: "gos_rt_map_set_vec_values",
                    args: vec![Operand::Copy(Place::local(l))],
                },
                VecMeta::MapFloatKeys => Rvalue::CallIntrinsic {
                    name: "gos_rt_map_set_float_keys",
                    args: vec![Operand::Copy(Place::local(l))],
                },
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
            };
            Statement {
                kind: StatementKind::Assign {
                    place: Place::local(dest),
                    rvalue,
                },
                span,
                inlined: None,
            }
        };

    for (bi, l, meta) in &head_inserts {
        let span = body.blocks[*bi].span;
        let stmt = mk(*l, meta, span, &mut next_unit);
        body.blocks[*bi].stmts.insert(0, stmt);
        // Shift any statement-gap inserts in the same block.
        for ins in &mut stmt_inserts {
            if ins.0 == *bi {
                ins.1 += 1;
            }
        }
    }
    // Insert in descending gap order so earlier indices stay valid.
    let mut by_block: Vec<(usize, usize, Local, VecMeta)> = stmt_inserts;
    by_block.sort_by_key(|ins| std::cmp::Reverse((ins.0, ins.1)));
    for (bi, gap, l, meta) in by_block {
        let span = body.blocks[bi].span;
        let stmt = mk(l, &meta, span, &mut next_unit);
        body.blocks[bi].stmts.insert(gap, stmt);
    }
    for _ in body.locals.len()..next_unit {
        body.locals.push(LocalDecl {
            ty: unit_ty,
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
type PendingRelease = (usize, Local, &'static str, Option<String>);

/// True for the RC retain intrinsics [`insert_aggr_copy_drops`] anchors to
/// the statement whose destination they cover.
fn is_rc_retain_intrinsic(name: &str) -> bool {
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
fn retain_anchor_end(stmts: &[Statement], si: usize) -> usize {
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

/// True when nothing looks at what a call answered.
///
/// A `let _ = m.insert(..)` binds the answer to a name no one reads, so the
/// destination's only reader is that copy. Following the one hop keeps the
/// discarded-answer form the same whether the call site names the answer or
/// not.
fn answer_is_discarded(
    body: &Body,
    reads: &std::collections::HashMap<u32, usize>,
    dest: Local,
) -> bool {
    if reads.get(&dest.0).copied().unwrap_or(0) == 0 {
        return true;
    }
    let mut hop: Option<Local> = None;
    for block in &body.blocks {
        for stmt in &block.stmts {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            let Rvalue::Use(Operand::Copy(src)) = rvalue else {
                continue;
            };
            if src.projection.is_empty() && src.local == dest {
                if !place.projection.is_empty() || hop.is_some() {
                    return false;
                }
                hop = Some(place.local);
            }
        }
    }
    // Any other shape of read - an argument, an operand - looks at the answer.
    let copied_out = usize::from(hop.is_some());
    if reads.get(&dest.0).copied().unwrap_or(0) != copied_out {
        return false;
    }
    hop.is_some_and(|h| reads.get(&h.0).copied().unwrap_or(0) == 0)
}
/// Keyed containers whose storage COPIES a `String` key's text rather than
/// keeping the pointer it was handed.
///
/// The frame stays the key's only owner, so it reclaims the key per site
/// rather than only at the return - which is what a map filled in a loop
/// needs. An enum key is the other case: the map keeps that node and releases
/// it itself.
fn copies_string_key(name: &str) -> bool {
    (name.starts_with("gos_rt_map_") || name.starts_with("gos_rt_set_"))
        && (name.contains("_str") || name.contains("_skey"))
}

/// Releases a `String` a keyed container copied and nothing else names.
///
/// A key built at the call site - `m.insert(format("k{i}"), v)` - is a
/// temporary the frame owns: the container keeps the text, not the pointer, so
/// the string has no other holder once the call returns. Only a key bound to a
/// name reaches the binding's own release, so an unbound one is reclaimed
/// here, at the call that consumed it.
///
/// The condition is deliberately narrow: the operand must be a bare local of
/// `String` type, read exactly once in the whole body (this argument), and
/// written by a call rather than aliased from another binding.
pub(crate) fn insert_copied_key_releases(body: &mut Body, tcx: &gossamer_types::TyCtxt) {
    use gossamer_types::TyKind;
    if body.locals.is_empty() {
        return;
    }
    let reads = collect_local_read_counts(body);
    let mut written_by_call = vec![false; body.locals.len()];
    let mut written_otherwise = vec![false; body.locals.len()];
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign { place, .. } = &stmt.kind
                && place.projection.is_empty()
                && (place.local.0 as usize) < written_otherwise.len()
            {
                written_otherwise[place.local.0 as usize] = true;
            }
        }
        if let Terminator::Call { destination, .. } = &block.terminator
            && destination.projection.is_empty()
            && (destination.local.0 as usize) < written_by_call.len()
        {
            written_by_call[destination.local.0 as usize] = true;
        }
    }
    let mut sites: Vec<(usize, Local)> = Vec::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            args,
            target: Some(_),
            ..
        } = &block.terminator
        else {
            continue;
        };
        if !copies_string_key(name) {
            continue;
        }
        for arg in args.iter().skip(1) {
            let Operand::Copy(p) = arg else { continue };
            if !p.projection.is_empty() {
                continue;
            }
            let idx = p.local.0 as usize;
            if idx >= body.locals.len()
                || body.locals[idx].region
                || !matches!(tcx.kind_of(body.locals[idx].ty), TyKind::String)
                || !written_by_call[idx]
                || written_otherwise[idx]
                || reads.get(&p.local.0).copied().unwrap_or(0) != 1
            {
                continue;
            }
            sites.push((bi, p.local));
        }
    }
    if sites.is_empty() {
        return;
    }
    let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
    for (bi, local) in sites {
        let Terminator::Call {
            target: Some(t), ..
        } = body.blocks[bi].terminator
        else {
            continue;
        };
        let dest = Local(u32::try_from(body.locals.len()).expect("local overflow"));
        body.locals.push(LocalDecl {
            ty: unit_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
        let span = body.blocks[t.0 as usize].span;
        body.blocks[t.0 as usize].stmts.insert(
            0,
            Statement {
                kind: StatementKind::Assign {
                    place: Place::local(dest),
                    rvalue: Rvalue::CallIntrinsic {
                        name: "gos_rt_str_free_typed",
                        args: vec![Operand::Copy(Place::local(local))],
                    },
                },
                span,
                inlined: None,
            },
        );
    }
}

pub(crate) fn insert_early_releases(body: &mut Body, tcx: &gossamer_types::TyCtxt) {
    // Locals whose payload is extracted anywhere in the body - a
    // by-value Result/Option slot read (`gos_rt_result_payload`), an
    // unwrap that answers the payload word (`gos_rt_result_unwrap_or`),
    // or an enum-box payload load (`gos_enum_load`). The extraction
    // BORROWS the value's children (shared field pointers, no retains),
    // and that borrow's lifetime is invisible to the mention analysis,
    // so these locals' releases must stay at the return sweep (see the
    // candidate match below).
    let extracts_payload = |name: &str| {
        matches!(
            name,
            "gos_rt_result_payload"
                | "gos_rt_result_payload_f64"
                | "gos_rt_result_payload_i128"
                | "gos_rt_result_unwrap"
                | "gos_rt_result_unwrap_carrier"
                | "gos_rt_result_unwrap_or"
                | "gos_rt_result_unwrap_or_carrier"
                | "gos_rt_result_unwrap_or_str"
                | "gos_rt_result_unwrap_or_node"
                | "gos_rt_result_unwrap_or_vec"
                | "gos_rt_result_ok"
                | "gos_rt_result_err"
                | "gos_rt_option_unwrap"
                | "gos_rt_option_unwrap_carrier"
                | "gos_enum_load"
                | "gos_enum_slot_ptr"
        )
    };
    let receiver_local = |args: &[Operand]| match args.first() {
        Some(Operand::Copy(p)) if p.projection.is_empty() => Some(p.local.0),
        _ => None,
    };
    let extracted_from: std::collections::HashSet<u32> = body
        .blocks
        .iter()
        .flat_map(|b| {
            let from_stmts = b.stmts.iter().filter_map(|stmt| match &stmt.kind {
                StatementKind::Assign {
                    rvalue: Rvalue::CallIntrinsic { name, args },
                    ..
                } if extracts_payload(name) => receiver_local(args),
                _ => None,
            });
            let from_call = match &b.terminator {
                Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(name)),
                    args,
                    ..
                } if extracts_payload(name) => receiver_local(args),
                _ => None,
            };
            from_stmts.chain(from_call)
        })
        .collect();

    let n_locals = body.locals.len();
    let n_blocks = body.blocks.len();
    if n_locals == 0 || n_blocks == 0 {
        return;
    }

    // RELEASE-side accounting only. A retain READS its argument (it
    // hands a fresh share to a holder that was just initialised from
    // this local), so retains MUST count as mentions: inserting the
    // early release+null between a store and its follow-up retain made
    // the retain see null - the new holder never got its share and the
    // node freed while still referenced.
    let accounting = |name: &str| -> bool {
        matches!(
            name,
            "gos_rt_rc_release"
                | "gos_rt_rc_weak_release"
                | "gos_rt_aggr_release_children"
                | "gos_rt_aggr_zero_guarded"
                | "gos_rt_option_slot_release"
        )
    };

    // Candidates: (local, release-intrinsic, optional meta symbol),
    // harvested from release calls sitting in Return blocks.
    let mut candidates: Vec<(Local, &'static str, Option<String>)> = Vec::new();
    let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::new();
    for block in &body.blocks {
        if !matches!(block.terminator, Terminator::Return) {
            continue;
        }
        for stmt in &block.stmts {
            let StatementKind::Assign {
                rvalue: Rvalue::CallIntrinsic { name, args },
                ..
            } = &stmt.kind
            else {
                continue;
            };
            let Some(Operand::Copy(p)) = args.first() else {
                continue;
            };
            if !p.projection.is_empty() {
                continue;
            }
            let release: &'static str = match *name {
                // An enum box whose payload was loaded somewhere in the
                // body keeps its at-return release: the load result
                // borrows the box's children (string / vec payloads freed
                // at box teardown), and an early release would free them
                // under the borrower.
                "gos_rt_rc_release" if !extracted_from.contains(&p.local.0) => "gos_rt_rc_release",
                "gos_rt_rc_weak_release" => "gos_rt_rc_weak_release",
                "gos_rt_aggr_release_children" => "gos_rt_aggr_release_children",
                // Early-relocating an option-slot release is unsound
                // when the result's payload is EXTRACTED somewhere in
                // the body: the extraction BORROWS the payload blob's
                // children (shared field pointers, no retains), and
                // that borrow's lifetime is invisible to the mention
                // analysis - the relocated release (typically right at
                // the extraction) frees the blob under the borrower.
                // Results that are never extracted-from keep early
                // placement (Option-chain workloads rely on it to keep
                // RAM flat).
                "gos_rt_option_slot_release" if !extracted_from.contains(&p.local.0) => {
                    "gos_rt_option_slot_release"
                }
                _ => continue,
            };
            if !seen.insert(p.local.0) {
                continue;
            }
            let meta = if release == "gos_rt_aggr_release_children" {
                match args.get(1) {
                    Some(Operand::Const(ConstValue::Str(sym))) => Some(sym.clone()),
                    _ => continue,
                }
            } else {
                None
            };
            candidates.push((p.local, release, meta));
        }
    }
    if candidates.is_empty() {
        return;
    }

    // Weak references make drop timing observable: a `Weak` created from
    // a local in this frame must keep observing it alive until the frame
    // ends, exactly as the VM does. When the body creates any weak
    // reference, the RC locals keep their at-return placement; guarded
    // aggregates and option holders cannot be downgraded and stay
    // eligible.
    let has_downgrade = body.blocks.iter().any(|b| {
        b.stmts.iter().any(|st| {
            matches!(
                &st.kind,
                StatementKind::Assign {
                    rvalue: Rvalue::CallIntrinsic { name, .. },
                    ..
                } if *name == "gos_rt_rc_downgrade"
            )
        }) || matches!(
            &b.terminator,
            Terminator::Call {
                callee: Operand::Const(ConstValue::Str(n)),
                ..
            } if n == "gos_rt_rc_downgrade" || n == "downgrade"
        )
    });
    if has_downgrade {
        candidates.retain(|(_, release, _)| {
            *release != "gos_rt_rc_release" && *release != "gos_rt_rc_weak_release"
        });
        if candidates.is_empty() {
            return;
        }
    }

    // Real mentions per block, and the Ref pin. A mention is any
    // appearance of the bare local in a non-accounting statement or in
    // a terminator. Constant stores (the zero-inits) don't count.
    let mut pinned: Vec<bool> = vec![false; n_locals];
    let mut mention_stmt: Vec<Vec<Option<usize>>> = vec![vec![None; n_locals]; n_blocks];
    let mut mention_term: Vec<Vec<bool>> = vec![vec![false; n_locals]; n_blocks];
    {
        let mark = |l: Local,
                    bi: usize,
                    si: Option<usize>,
                    mention_stmt: &mut Vec<Vec<Option<usize>>>,
                    mention_term: &mut Vec<Vec<bool>>| {
            let i = l.0 as usize;
            if i >= n_locals {
                return;
            }
            match si {
                Some(si) => mention_stmt[bi][i] = Some(si),
                None => mention_term[bi][i] = true,
            }
        };
        let locals_in_operand = |op: &Operand, out: &mut Vec<Local>| {
            if let Operand::Copy(p) = op {
                out.push(p.local);
            }
        };
        for (bi, block) in body.blocks.iter().enumerate() {
            for (si, stmt) in block.stmts.iter().enumerate() {
                let (place, rvalue) = match &stmt.kind {
                    StatementKind::Assign { place, rvalue } => (place, rvalue),
                    StatementKind::StorageLive(_)
                    | StatementKind::StorageDead(_)
                    | StatementKind::Nop => {
                        // Storage markers / no-ops, not value uses.
                        continue;
                    }
                    StatementKind::SetDiscriminant { place, .. } => {
                        mark(
                            place.local,
                            bi,
                            Some(si),
                            &mut mention_stmt,
                            &mut mention_term,
                        );
                        continue;
                    }
                    StatementKind::StaticStore { value, .. } => {
                        // The stored value is used here; mark its local.
                        let mut ls: Vec<Local> = Vec::new();
                        locals_in_operand(value, &mut ls);
                        for l in ls {
                            mark(l, bi, Some(si), &mut mention_stmt, &mut mention_term);
                        }
                        continue;
                    }
                    StatementKind::IterSource { dst, source, .. } => {
                        let mut ls: Vec<Local> = Vec::new();
                        locals_in_operand(source, &mut ls);
                        ls.push(dst.local);
                        for l in ls {
                            mark(l, bi, Some(si), &mut mention_stmt, &mut mention_term);
                        }
                        continue;
                    }
                    StatementKind::IterAdapter {
                        dst,
                        upstream,
                        closure_or_arg,
                        ..
                    } => {
                        let mut ls = vec![dst.local, upstream.local];
                        if let Some(arg) = closure_or_arg {
                            locals_in_operand(arg, &mut ls);
                        }
                        for l in ls {
                            mark(l, bi, Some(si), &mut mention_stmt, &mut mention_term);
                        }
                        continue;
                    }
                    StatementKind::IterNext {
                        dst_option,
                        iter_place,
                        ..
                    } => {
                        mark(
                            dst_option.local,
                            bi,
                            Some(si),
                            &mut mention_stmt,
                            &mut mention_term,
                        );
                        mark(
                            iter_place.local,
                            bi,
                            Some(si),
                            &mut mention_stmt,
                            &mut mention_term,
                        );
                        continue;
                    }
                };
                let mut ls: Vec<Local> = Vec::new();
                match rvalue {
                    Rvalue::CallIntrinsic { name, args } if accounting(name) => {
                        // Accounting calls are not program uses.
                        let _ = args;
                    }
                    Rvalue::Use(Operand::Const(_)) => {
                        // Constant (re)initialisation - the zero-init
                        // pattern; not a use of the heap value.
                    }
                    Rvalue::Ref { place: rp, .. } => {
                        pinned[rp.local.0 as usize] = true;
                        ls.push(rp.local);
                        ls.push(place.local);
                    }
                    Rvalue::Use(op) => {
                        locals_in_operand(op, &mut ls);
                        ls.push(place.local);
                    }
                    Rvalue::BinaryOp { lhs, rhs, .. } => {
                        locals_in_operand(lhs, &mut ls);
                        locals_in_operand(rhs, &mut ls);
                        ls.push(place.local);
                    }
                    Rvalue::UnaryOp { operand, .. } => {
                        locals_in_operand(operand, &mut ls);
                        ls.push(place.local);
                    }
                    Rvalue::CallIntrinsic { args, .. } => {
                        for a in args {
                            locals_in_operand(a, &mut ls);
                        }
                        ls.push(place.local);
                    }
                    Rvalue::Aggregate { operands, .. } => {
                        // An aggregate literal (fixed array, tuple) copies
                        // heap POINTERS out of its operands without
                        // retaining them - the aggregate borrows the
                        // operand locals' shares. Releasing an operand at
                        // its last textual mention would free a node the
                        // aggregate still references, so pin operands to
                        // the return-site release.
                        for a in operands {
                            locals_in_operand(a, &mut ls);
                            if let Operand::Copy(p) = a {
                                pinned[p.local.0 as usize] = true;
                            }
                        }
                        ls.push(place.local);
                    }
                    Rvalue::Repeat { value, .. } => {
                        locals_in_operand(value, &mut ls);
                        ls.push(place.local);
                    }
                    _ => {
                        // Unmodelled rvalue shapes: pin everything they
                        // could mention by pinning the destination and
                        // bailing on precision for this statement.
                        ls.push(place.local);
                    }
                }
                for l in ls {
                    mark(l, bi, Some(si), &mut mention_stmt, &mut mention_term);
                }
            }
            let mut ls: Vec<Local> = Vec::new();
            match &block.terminator {
                Terminator::Call {
                    callee,
                    args,
                    destination,
                    ..
                } => {
                    // The callee is read by the call as much as an argument
                    // is: a callable value reaches its body through this
                    // operand, so a release hoisted past it would free the
                    // environment the call is about to enter.
                    locals_in_operand(callee, &mut ls);
                    for a in args {
                        locals_in_operand(a, &mut ls);
                    }
                    ls.push(destination.local);
                }
                Terminator::SwitchInt { discriminant, .. } => {
                    locals_in_operand(discriminant, &mut ls);
                }
                Terminator::Assert { cond, msg, .. } => {
                    locals_in_operand(cond, &mut ls);
                    for op in msg.operands() {
                        locals_in_operand(op, &mut ls);
                    }
                }
                Terminator::Drop { place, .. } => {
                    ls.push(place.local);
                }
                _ => {}
            }
            for l in ls {
                mark(l, bi, None, &mut mention_stmt, &mut mention_term);
            }
        }
    }

    // Successor map.
    let succs: Vec<Vec<usize>> = body
        .blocks
        .iter()
        .map(|b| match &b.terminator {
            Terminator::Goto { target } => vec![target.0 as usize],
            Terminator::SwitchInt { arms, default, .. } => {
                let mut v: Vec<usize> = arms.iter().map(|(_, t)| t.0 as usize).collect();
                v.push(default.0 as usize);
                v
            }
            Terminator::Call { target, .. } => target.iter().map(|t| t.0 as usize).collect(),
            Terminator::Assert { target, .. } => vec![target.0 as usize],
            Terminator::Drop { target, .. } => vec![target.0 as usize],
            _ => Vec::new(),
        })
        .collect();

    // Per candidate: blocks from whose EXIT a mention is reachable.
    // Fixpoint over the reversed edges.
    let mut inserts_after_stmt: Vec<Vec<PendingRelease>> = vec![Vec::new(); n_blocks];
    let mut inserts_at_head: Vec<Vec<(Local, &'static str, Option<String>)>> =
        vec![Vec::new(); n_blocks];
    for (l, release, meta) in &candidates {
        let li = l.0 as usize;
        if li >= n_locals || pinned[li] {
            continue;
        }
        let mentions: Vec<bool> = (0..n_blocks)
            .map(|bi| mention_stmt[bi][li].is_some() || mention_term[bi][li])
            .collect();
        let mut reach: Vec<bool> = vec![false; n_blocks];
        let mut changed = true;
        while changed {
            changed = false;
            for bi in 0..n_blocks {
                if reach[bi] {
                    continue;
                }
                let r = succs[bi].iter().any(|&s| mentions[s] || reach[s]);
                if r {
                    reach[bi] = true;
                    changed = true;
                }
            }
        }
        for bi in 0..n_blocks {
            if !mentions[bi] || reach[bi] {
                continue;
            }
            if matches!(body.blocks[bi].terminator, Terminator::Return) {
                // The backstop already covers this block.
                continue;
            }
            if mention_term[bi][li] {
                for &s in &succs[bi] {
                    inserts_at_head[s].push((*l, release, meta.clone()));
                }
            } else if let Some(si) = mention_stmt[bi][li] {
                inserts_after_stmt[bi].push((si, *l, release, meta.clone()));
            }
        }
    }

    let total: usize = inserts_after_stmt.iter().map(Vec::len).sum::<usize>()
        + inserts_at_head.iter().map(Vec::len).sum::<usize>();
    if total == 0 {
        return;
    }

    let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
    let mut next_unit = body.locals.len();
    let release_stmts = |l: Local,
                         release: &'static str,
                         meta: &Option<String>,
                         span: gossamer_lex::Span,
                         next_unit: &mut usize|
     -> Vec<Statement> {
        let dest = Local(u32::try_from(*next_unit).expect("local overflow"));
        *next_unit += 1;
        let mut args = vec![Operand::Copy(Place::local(l))];
        if let Some(sym) = meta {
            args.push(Operand::Const(ConstValue::Str(sym.clone())));
        }
        let mut v = vec![Statement {
            kind: StatementKind::Assign {
                place: Place::local(dest),
                rvalue: Rvalue::CallIntrinsic {
                    name: release,
                    args,
                },
            },
            span,
            inlined: None,
        }];
        // Null out so the at-return backstop (and any
        // release-before-reassign) reads an empty value. Guarded
        // aggregates zero their option slots through the meta walk;
        // scalar holders zero the whole slot.
        if release == "gos_rt_aggr_release_children" {
            let dest2 = Local(u32::try_from(*next_unit).expect("local overflow"));
            *next_unit += 1;
            v.push(Statement {
                kind: StatementKind::Assign {
                    place: Place::local(dest2),
                    rvalue: Rvalue::CallIntrinsic {
                        name: "gos_rt_aggr_zero_guarded",
                        args: vec![
                            Operand::Copy(Place::local(l)),
                            Operand::Const(ConstValue::Str(meta.clone().unwrap_or_default())),
                        ],
                    },
                },
                span,
                inlined: None,
            });
        } else {
            v.push(Statement {
                kind: StatementKind::Assign {
                    place: Place::local(l),
                    rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                },
                span,
                inlined: None,
            });
        }
        v
    };

    let mut new_unit_locals = 0usize;
    for bi in 0..n_blocks {
        let head = std::mem::take(&mut inserts_at_head[bi]);
        let mut after = std::mem::take(&mut inserts_after_stmt[bi]);
        if head.is_empty() && after.is_empty() {
            continue;
        }
        let span = body.blocks[bi].span;
        let orig: Vec<Statement> = std::mem::take(&mut body.blocks[bi].stmts);
        // A statement that copies this local into another place hands the
        // new holder an alias of the same payload, and the copy pass takes
        // that holder's share in the retains anchored right after it. The
        // release belongs after those, so the share the alias keeps is
        // taken before this one is given up.
        for entry in &mut after {
            entry.0 = retain_anchor_end(&orig, entry.0);
        }
        after.sort_by_key(|(si, ..)| *si);
        let mut new_stmts: Vec<Statement> =
            Vec::with_capacity(orig.len() + 2 * (head.len() + after.len()));
        // The retains opening a block take the shares a predecessor's call
        // destination keeps, read from a payload the released local may hold
        // the only reference to, so the head releases follow them.
        let head_at = orig
            .iter()
            .position(|stmt| {
                !matches!(
                    &stmt.kind,
                    StatementKind::Assign {
                        rvalue: Rvalue::CallIntrinsic { name, .. },
                        ..
                    } if is_rc_retain_intrinsic(name)
                )
            })
            .unwrap_or(orig.len());
        let mut head_emitted = false;
        let emit_head = |new_stmts: &mut Vec<Statement>, next_unit: &mut usize| -> usize {
            let before = *next_unit;
            for (l, release, meta) in &head {
                new_stmts.extend(release_stmts(*l, release, meta, span, next_unit));
            }
            *next_unit - before
        };
        for (si, stmt) in orig.into_iter().enumerate() {
            if si == head_at {
                new_unit_locals += emit_head(&mut new_stmts, &mut next_unit);
                head_emitted = true;
            }
            new_stmts.push(stmt);
            for (asi, l, release, meta) in &after {
                if *asi == si {
                    let before = next_unit;
                    new_stmts.extend(release_stmts(*l, release, meta, span, &mut next_unit));
                    new_unit_locals += next_unit - before;
                }
            }
        }
        if !head_emitted {
            new_unit_locals += emit_head(&mut new_stmts, &mut next_unit);
        }
        body.blocks[bi].stmts = new_stmts;
    }
    for _ in 0..new_unit_locals {
        body.locals.push(LocalDecl {
            ty: unit_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
    }
}

/// The `gos_rt_result_payload_release` kind of an option holder's `Err`
/// payload, for a `Result` whose `Ok` arm is an aggregate the holder walk owns
/// and whose `Err` arm is a counted value: `1` a `String`, `4` an
/// `errors::Error` cell. A `Vec` error payload is owned by the binding that
/// extracts it, through the vector's own release.
pub(crate) fn holder_err_kind(tcx: &gossamer_types::TyCtxt, ty: gossamer_types::Ty) -> Option<i64> {
    use gossamer_types::TyKind;
    let TyKind::Adt { def, substs } = tcx.kind_of(ty) else {
        return None;
    };
    if def.local != u32::MAX {
        return None;
    }
    let types = substs.types();
    let ok = *types.first()?;
    let ok_is_aggregate = match tcx.kind_of(ok) {
        TyKind::Adt { def, .. } => def.local < u32::MAX - 16 && !tcx.is_inline_enum_ty(ok),
        TyKind::Tuple(_) | TyKind::Array { .. } => true,
        _ => false,
    };
    if !ok_is_aggregate {
        return None;
    }
    let err = *types.get(1)?;
    if tcx.is_counted_node(err) {
        return Some(4);
    }
    match tcx.kind_of(err) {
        TyKind::String => Some(1),
        TyKind::DynError => Some(4),
        _ => None,
    }
}

/// The storage kind of an `ok_or` replacement error, in the kinds
/// `gos_rt_result_ok_payload_release` takes: `1` a `String`, `2` a `Vec`, `4`
/// an `errors::Error` cell, `0` a value the carrier does not own. The call
/// consumes the replacement on either arm, so the kind is what lets the arm
/// that discards it give it back.
pub(crate) fn ok_or_err_kind(tcx: &gossamer_types::TyCtxt, ty: gossamer_types::Ty) -> i64 {
    use gossamer_types::TyKind;
    if tcx.is_counted_node(ty) {
        return 4;
    }
    match tcx.kind_of(ty) {
        TyKind::String => 1,
        TyKind::Vec(_) | TyKind::Slice(_) => 2,
        TyKind::DynError => 4,
        _ => 0,
    }
}

/// Gives every `gos_rt_result_ok_or` call the kind its replacement error is
/// given back by. The method resolves through several dispatch tables, each
/// assembling its own argument list, so the kind is appended once here - where
/// every lowering path's call is already in hand - rather than at each site
/// that can build one. A call that already carries its kind is left alone.
pub(crate) fn complete_ok_or_err_kind(body: &mut Body, tcx: &gossamer_types::TyCtxt) {
    let local_tys: Vec<gossamer_types::Ty> = body.locals.iter().map(|l| l.ty).collect();
    for block in &mut body.blocks {
        let Terminator::Call { callee, args, .. } = &mut block.terminator else {
            continue;
        };
        let Operand::Const(ConstValue::Str(name)) = callee else {
            continue;
        };
        if name != "gos_rt_result_ok_or" || args.len() != 2 {
            continue;
        }
        let kind = match &args[1] {
            Operand::Copy(p) if p.projection.is_empty() => local_tys
                .get(p.local.0 as usize)
                .map_or(0, |ty| ok_or_err_kind(tcx, *ty)),
            _ => 0,
        };
        args.push(Operand::Const(ConstValue::Int(i128::from(kind))));
    }
}

/// Whether a `Result` / `Option` has an arm holding a counted aggregate blob,
/// which makes a carrier of it an option holder rather than a carrier the
/// payload walk owns.
pub(crate) fn holds_counted_blob_arm(tcx: &gossamer_types::TyCtxt, ty: gossamer_types::Ty) -> bool {
    use gossamer_types::TyKind;
    match tcx.kind_of(ty) {
        TyKind::Adt { def, substs } if def.local == u32::MAX || def.local == u32::MAX - 1 => substs
            .types()
            .iter()
            .take(2)
            .any(|p| tcx.aggr_copy_meta(*p).is_some()),
        _ => false,
    }
}

/// Pairs each option-holder share with the share of its `Err` payload.
///
/// The holder helpers account for the copy blob an aggregate arm holds; an
/// `Err` arm holding a string, vector, or error cell is owned by the same
/// holder, so every retain and release the holder walk makes on a holder local
/// takes or gives back that payload too. The helper acts on the `Err` arm only,
/// so on an `Ok` value it does nothing.
pub(crate) fn pair_holder_err_arm_calls(body: &mut Body, tcx: &gossamer_types::TyCtxt) {
    let n_locals = body.locals.len();
    let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
    let mut sinks = 0usize;
    for block in &mut body.blocks {
        let mut stmts = Vec::with_capacity(block.stmts.len());
        for stmt in std::mem::take(&mut block.stmts) {
            let paired = match &stmt.kind {
                StatementKind::Assign {
                    rvalue: Rvalue::CallIntrinsic { name, args },
                    ..
                } if matches!(
                    *name,
                    "gos_rt_option_slot_release" | "gos_rt_option_slot_retain"
                ) =>
                {
                    match args.as_slice() {
                        [Operand::Copy(p)]
                            if p.projection.is_empty() && (p.local.0 as usize) < n_locals =>
                        {
                            holder_err_kind(tcx, body.locals[p.local.0 as usize].ty).map(|kind| {
                                let helper = if *name == "gos_rt_option_slot_release" {
                                    "gos_rt_result_payload_release"
                                } else {
                                    "gos_rt_result_payload_retain"
                                };
                                (helper, p.local, kind)
                            })
                        }
                        _ => None,
                    }
                }
                _ => None,
            };
            let span = stmt.span;
            stmts.push(stmt);
            if let Some((helper, local, kind)) = paired {
                let sink = Local(u32::try_from(n_locals + sinks).expect("local overflow"));
                sinks += 1;
                stmts.push(Statement {
                    kind: StatementKind::Assign {
                        place: Place::local(sink),
                        rvalue: Rvalue::CallIntrinsic {
                            name: helper,
                            args: vec![
                                Operand::Copy(Place::local(local)),
                                Operand::Const(ConstValue::Int(0)),
                                Operand::Const(ConstValue::Int(i128::from(kind))),
                            ],
                        },
                    },
                    span,
                    inlined: None,
                });
            }
        }
        block.stmts = stmts;
    }
    for _ in 0..sinks {
        body.locals.push(LocalDecl {
            ty: unit_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
    }
}

/// Gives a by-value payload-enum parameter a share of its own.
///
/// A `mut` parameter is the callee's value, not the caller's variable, so a
/// body that rebinds one through a `&mut` borrow owns whatever the slot ends
/// up holding: it takes a share at entry and gives one back at its death, so
/// the release the rebinding makes is the frame's own and the value the slot
/// ends with is the frame's to hand on.
pub(crate) fn own_rebound_enum_parameters(body: &mut Body, tcx: &gossamer_types::TyCtxt) {
    let arity = body.arity as usize;
    if arity == 0 || body.locals.is_empty() {
        return;
    }
    let n_locals = body.locals.len();
    let mut rebound: Vec<Local> = Vec::new();
    for i in 1..=arity.min(n_locals - 1) {
        if body.locals[i].region || !tcx.is_payload_enum(body.locals[i].ty) {
            continue;
        }
        let local = Local(u32::try_from(i).expect("local index fits in u32"));
        let borrowed = body.blocks.iter().flat_map(|b| b.stmts.iter()).any(|stmt| {
            matches!(
                &stmt.kind,
                StatementKind::Assign {
                    rvalue: Rvalue::Ref {
                        mutable: true,
                        place,
                    },
                    ..
                } if place.local == local && place.projection.is_empty()
            )
        });
        if borrowed {
            rebound.push(local);
        }
    }
    if rebound.is_empty() {
        return;
    }
    let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
    let fresh_unit = |body: &mut Body| -> Local {
        let local = Local(u32::try_from(body.locals.len()).expect("local index fits in u32"));
        body.locals.push(LocalDecl {
            ty: unit_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
        local
    };
    for &param in &rebound {
        for bi in 0..body.blocks.len() {
            if !matches!(body.blocks[bi].terminator, Terminator::Return) {
                continue;
            }
            let span = body.blocks[bi].span;
            let dest = fresh_unit(body);
            body.blocks[bi]
                .stmts
                .push(rc_call_stmt("gos_rt_rc_release", dest, param, span));
        }
        let span = body.blocks[0].span;
        let dest = fresh_unit(body);
        body.blocks[0]
            .stmts
            .insert(0, rc_call_stmt("gos_rt_rc_retain", dest, param, span));
    }
}

/// Releases the node a `&mut <payload enum>` reference displaced.
///
/// That receiver names the caller's slot, so `*self = Variant(..)` rebinds the
/// caller's binding: the node the slot held loses the share the binding gave
/// it, and the callee is the only side that can see both values. The release
/// follows the store, so a replacement built from the old node keeps it alive
/// until the slot no longer names it.
pub(crate) fn release_displaced_enum_targets(body: &mut Body, tcx: &gossamer_types::TyCtxt) {
    let n_locals = body.locals.len();
    let pointee_of = |local: Local| -> Option<gossamer_types::Ty> {
        let i = local.0 as usize;
        if i >= n_locals {
            return None;
        }
        let gossamer_types::TyKind::Ref {
            mutability: gossamer_types::Mutbl::Mut,
            inner,
        } = tcx.kind_of(body.locals[i].ty)
        else {
            return None;
        };
        tcx.is_payload_enum(*inner).then_some(*inner)
    };
    let mut sites: Vec<(usize, usize, Local, gossamer_types::Ty)> = Vec::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        for (si, stmt) in block.stmts.iter().enumerate() {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            if place.projection.as_slice() != [crate::ir::Projection::Deref]
                || matches!(rvalue, Rvalue::Use(Operand::Const(_)))
            {
                continue;
            }
            if let Some(pointee) = pointee_of(place.local) {
                sites.push((bi, si, place.local, pointee));
            }
        }
    }
    if sites.is_empty() {
        return;
    }
    let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
    for (bi, si, reference, pointee) in sites.into_iter().rev() {
        let span = body.blocks[bi].stmts[si].span;
        let mut push_local = |ty: gossamer_types::Ty| -> Local {
            let local = Local(u32::try_from(body.locals.len()).expect("local index fits in u32"));
            body.locals.push(LocalDecl {
                ty,
                debug_name: None,
                mutable: false,
                region: false,
            });
            local
        };
        let old = push_local(pointee);
        let dest = push_local(unit_ty);
        body.blocks[bi]
            .stmts
            .insert(si + 1, rc_call_stmt("gos_rt_rc_release", dest, old, span));
        body.blocks[bi].stmts.insert(
            si,
            Statement {
                kind: StatementKind::Assign {
                    place: Place::local(old),
                    rvalue: Rvalue::CallIntrinsic {
                        name: "gos_load",
                        args: vec![
                            Operand::Copy(Place::local(reference)),
                            Operand::Const(ConstValue::Int(0)),
                        ],
                    },
                },
                span,
                inlined: None,
            },
        );
    }
}

/// Reclaims a channel that never leaves the function that made it.
///
/// A channel is shared: the sender end, the receiver end, and any goroutine
/// that captured one all reach the same handle, and only a party that took a
/// reference of its own may give one back. So the drop is emitted only where
/// every local derived from the pair is confined to this body - read by the
/// channel's own runtime helpers and nothing else. A channel that is returned,
/// stored, captured, or handed to any other callee is left alone.
///
/// The drop is what runs the channel's teardown, which gives back the share
/// each send minted for a value nobody received.
pub(crate) fn drop_confined_channels(body: &mut Body) {
    const CREATORS: &[&str] = &[
        "channel",
        "channel::new",
        "channel::unbounded",
        "sync::channel",
        "sync::channel_unbounded",
        "std::sync::channel",
        "std::sync::channel_unbounded",
        "sync::Channel::new",
        "Channel::new",
    ];

    let n = body.locals.len();
    let creations: Vec<Local> = body
        .blocks
        .iter()
        .filter_map(|block| match &block.terminator {
            Terminator::Call {
                callee: Operand::Const(ConstValue::Str(name)),
                destination,
                ..
            } if destination.projection.is_empty()
                && (destination.local.0 as usize) < n
                && CREATORS.contains(&name.as_str()) =>
            {
                Some(destination.local)
            }
            _ => None,
        })
        .collect();
    if creations.is_empty() {
        return;
    }

    // Each channel is judged on its own: one that escapes leaves the others
    // alone, and a body that opens several reclaims each of them.
    let mut handles: Vec<Local> = Vec::new();
    for pair in creations {
        if let Some(handle) = confined_channel_handle(body, pair, n) {
            handles.push(handle);
        }
    }
    handles.sort_unstable_by_key(|l| l.0);
    handles.dedup();
    for handle in handles {
        emit_channel_drops(body, handle);
    }
}

/// The local holding a channel's handle, when every local derived from the pair
/// `pair` names is confined to this body - read by the channel's own runtime
/// helpers and nothing else. A channel that is returned, stored, captured, or
/// handed to any other callee answers `None`.
fn confined_channel_handle(body: &Body, pair: Local, n: usize) -> Option<Local> {
    let mut derived = vec![false; n];
    derived[pair.0 as usize] = true;
    let mut changed = true;
    while changed {
        changed = false;
        for block in &body.blocks {
            for stmt in &block.stmts {
                if let StatementKind::Assign { place, rvalue } = &stmt.kind
                    && place.projection.is_empty()
                    && (place.local.0 as usize) < n
                    && !derived[place.local.0 as usize]
                    && let Rvalue::Use(Operand::Copy(src)) = rvalue
                    && (src.local.0 as usize) < n
                    && derived[src.local.0 as usize]
                {
                    derived[place.local.0 as usize] = true;
                    changed = true;
                }
            }
        }
    }

    let reads = |op: &Operand| -> bool {
        matches!(op, Operand::Copy(p) if (p.local.0 as usize) < n && derived[p.local.0 as usize])
    };
    for block in &body.blocks {
        for stmt in &block.stmts {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            let confined_target = place.projection.is_empty()
                && (place.local.0 as usize) < n
                && derived[place.local.0 as usize];
            match rvalue {
                // Splitting the pair, or aliasing an end, stays inside.
                Rvalue::Use(op) if reads(op) && !confined_target => return None,
                Rvalue::CallIntrinsic { name, args }
                    if args.iter().any(&reads) && !name.starts_with("gos_rt_chan_") =>
                {
                    return None;
                }
                Rvalue::Aggregate { operands, .. }
                    if operands.iter().any(&reads) && !confined_target =>
                {
                    return None;
                }
                Rvalue::BinaryOp { lhs, rhs, .. } if reads(lhs) || reads(rhs) => return None,
                _ => {}
            }
        }
        match &block.terminator {
            Terminator::Call { callee, args, .. } if args.iter().any(&reads) => {
                let confined_callee = matches!(
                    callee,
                    Operand::Const(ConstValue::Str(name)) if name.starts_with("gos_rt_chan_")
                );
                if !confined_callee {
                    return None;
                }
            }
            Terminator::SwitchInt { discriminant, .. } if reads(discriminant) => return None,
            _ => {}
        }
    }
    // The return slot is derived exactly when an end is returned.
    if derived[Local::RETURN.0 as usize] {
        return None;
    }

    // Drop through the local a send or a recv named: that one holds the handle
    // itself rather than the pair it was projected out of.
    body.blocks
        .iter()
        .find_map(|block| match &block.terminator {
            Terminator::Call {
                callee: Operand::Const(ConstValue::Str(name)),
                args,
                ..
            } if name.starts_with("gos_rt_chan_") => match args.first() {
                Some(Operand::Copy(p))
                    if p.projection.is_empty()
                        && (p.local.0 as usize) < n
                        && derived[p.local.0 as usize] =>
                {
                    Some(p.local)
                }
                _ => None,
            },
            _ => None,
        })
}

/// One drop per channel: wherever the handle is reassigned, and again at every
/// exit. A loop that opens a channel per iteration reclaims each one as the
/// next takes its place, and the handle starts null so the first iteration's
/// drop is the no-op a null handle answers.
fn emit_channel_drops(body: &mut Body, handle: Local) {
    let unit_ty = body.locals[0].ty;
    let drop_stmt = |body: &mut Body, span: Span| -> Statement {
        let sink = Local(u32::try_from(body.locals.len()).expect("local overflow"));
        body.locals.push(crate::ir::LocalDecl {
            ty: unit_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
        Statement {
            kind: StatementKind::Assign {
                place: Place::local(sink),
                rvalue: Rvalue::CallIntrinsic {
                    name: "gos_rt_chan_drop",
                    args: vec![Operand::Copy(Place::local(handle))],
                },
            },
            span,
            inlined: None,
        }
    };

    let mut reassignments: Vec<(usize, usize)> = Vec::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        for (si, stmt) in block.stmts.iter().enumerate() {
            if let StatementKind::Assign { place, .. } = &stmt.kind
                && place.projection.is_empty()
                && place.local == handle
            {
                reassignments.push((bi, si));
            }
        }
    }
    reassignments.sort_unstable();
    for (bi, si) in reassignments.into_iter().rev() {
        let span = body.blocks[bi].span;
        let stmt = drop_stmt(body, span);
        body.blocks[bi].stmts.insert(si, stmt);
    }

    let returns: Vec<usize> = body
        .blocks
        .iter()
        .enumerate()
        .filter(|(_, b)| matches!(b.terminator, Terminator::Return))
        .map(|(i, _)| i)
        .collect();
    for bi in returns {
        let span = body.blocks[bi].span;
        let stmt = drop_stmt(body, span);
        body.blocks[bi].stmts.push(stmt);
    }

    let span = body.blocks[0].span;
    body.blocks[0].stmts.insert(
        0,
        Statement {
            kind: StatementKind::Assign {
                place: Place::local(handle),
                rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            },
            span,
            inlined: None,
        },
    );
}

/// Tells a channel what its element word owns, at each send.
///
/// The send mints the channel's share of the element's heap storage (see
/// `stores_aggregate_by_pointer`); a receiver gives it back, and a value nobody
/// receives is given back by the channel's teardown - which needs to know the
/// shape of the word it is holding, and learns it here, where the element's
/// static type is in hand.
/// The runtime call that marks a value of type `ty` shared as it is sent to
/// another goroutine, with the layout meta symbol an in-place aggregate walk
/// reads, or `None` when the value counts nothing.
fn send_mark_shared_call(
    tcx: &mut gossamer_types::TyCtxt,
    ty: gossamer_types::Ty,
) -> Option<(&'static str, Option<String>)> {
    use gossamer_types::TyKind;
    let mut cur = ty;
    while let TyKind::Ref { inner, .. } = tcx.kind_of(cur) {
        cur = *inner;
    }
    match tcx.kind_of(cur).clone() {
        TyKind::Vec(_) | TyKind::Slice(_) => return Some(("gos_rt_vec_mark_shared", None)),
        TyKind::HashMap { .. } => return Some(("gos_rt_map_mark_shared", None)),
        TyKind::Adt { def, .. } if is_set_def(tcx, def) => {
            return Some(("gos_rt_set_mark_shared", None));
        }
        _ => {}
    }
    if tcx.is_rc_managed(cur) {
        return Some(("gos_rt_rc_mark_shared", None));
    }
    let inline = match tcx.kind_of(cur) {
        TyKind::Tuple(_) | TyKind::Array { .. } => true,
        TyKind::Adt { def, .. } => {
            def.local < u32::MAX - 16 && tcx.struct_field_tys(*def).is_some()
        }
        _ => false,
    };
    if !inline {
        return None;
    }
    let mut entries = Vec::new();
    send_layout_entries(tcx, cur, 0, 0, &mut entries);
    if entries.is_empty() {
        return None;
    }
    let symbol = format!("gos_rc_meta_sendmark_{}", cur.as_u32());
    if tcx.rc_meta(&symbol).is_none() {
        let mut blob = vec![gossamer_abi::rc::RC_KIND_STRUCT, 1, 0, entries.len() as i64];
        blob.extend_from_slice(&entries);
        tcx.register_rc_meta(symbol.clone(), blob);
    }
    Some(("gos_rt_aggr_mark_shared_children", Some(symbol)))
}

/// Whether `def` names the `Set` / `BTreeSet` handle.
fn is_set_def(tcx: &gossamer_types::TyCtxt, def: gossamer_resolve::DefId) -> bool {
    tcx.def_name(def)
        .is_some_and(|name| matches!(name, "Set" | "BTreeSet"))
}

/// Child-word entries naming every counted word of the by-value aggregate `ty`
/// laid out from `base_word`, for the in-place sharing walk.
fn send_layout_entries(
    tcx: &gossamer_types::TyCtxt,
    ty: gossamer_types::Ty,
    base_word: i64,
    depth: u32,
    out: &mut Vec<i64>,
) {
    use gossamer_abi::rc::{
        RC_CHILD_KIND_SHIFT, RC_CHILD_MAP, RC_CHILD_RC, RC_CHILD_SET, RC_CHILD_VEC,
    };
    use gossamer_types::TyKind;
    if depth > 16 {
        return;
    }
    let field_tys: Vec<gossamer_types::Ty> = match tcx.kind_of(ty) {
        TyKind::Tuple(elems) => elems.clone(),
        TyKind::Array { elem, len } => vec![*elem; len.to_usize()],
        TyKind::Adt { def, substs } if def.local < u32::MAX - 16 => {
            match tcx.adt_field_tys(*def, substs) {
                Some(fields) => fields.to_vec(),
                None => return,
            }
        }
        _ => return,
    };
    let mut word = base_word;
    for fty in field_tys {
        let fwords = i64::from(tcx.slot_bytes(fty).max(8) / 8);
        let entry = |kind: i64, at: i64| (kind << RC_CHILD_KIND_SHIFT) | at;
        match tcx.kind_of(fty).clone() {
            TyKind::Vec(_) | TyKind::Slice(_) => out.push(entry(RC_CHILD_VEC, word)),
            TyKind::HashMap { .. } => out.push(entry(RC_CHILD_MAP, word)),
            TyKind::Adt { def, .. } if is_set_def(tcx, def) => {
                out.push(entry(RC_CHILD_SET, word));
            }
            // An `Option` / `Result` holds its payload in the word after the
            // discriminant; a `None` payload word is zero, which the walk
            // skips, and a scalar payload is never a counted kind.
            TyKind::Adt { def, substs } if def.local == u32::MAX || def.local == u32::MAX - 1 => {
                let payload_kind = substs.types().iter().find_map(|t| match tcx.kind_of(*t) {
                    _ if tcx.is_counted_node(*t) => Some(RC_CHILD_RC),
                    TyKind::String => Some(RC_CHILD_RC),
                    TyKind::Vec(_) | TyKind::Slice(_) => Some(RC_CHILD_VEC),
                    _ => None,
                });
                let uniform = substs.types().iter().all(|t| {
                    tcx.is_counted_node(*t)
                        || matches!(
                            tcx.kind_of(*t),
                            TyKind::String | TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Unit
                        )
                });
                if let Some(kind) = payload_kind
                    && uniform
                    && substs
                        .types()
                        .iter()
                        .filter(|t| !matches!(tcx.kind_of(**t), TyKind::Unit))
                        .all(|t| {
                            let same = match tcx.kind_of(*t) {
                                _ if tcx.is_counted_node(*t) => RC_CHILD_RC,
                                TyKind::String => RC_CHILD_RC,
                                _ => RC_CHILD_VEC,
                            };
                            same == kind
                        })
                {
                    out.push(entry(kind, word + 1));
                }
            }
            _ if tcx.is_rc_managed(fty) => out.push(entry(RC_CHILD_RC, word)),
            TyKind::Tuple(_) | TyKind::Array { .. } | TyKind::Adt { .. } => {
                send_layout_entries(tcx, fty, word, depth + 1, out);
            }
            _ => {}
        }
        word += fwords;
    }
}

pub(crate) fn record_channel_elem_kind(body: &mut Body, tcx: &mut gossamer_types::TyCtxt) {
    use gossamer_types::TyKind;

    let kind_of = |ty: gossamer_types::Ty| -> Option<i64> {
        let mut cur = ty;
        loop {
            match tcx.kind_of(cur) {
                TyKind::Ref { inner, .. } => cur = *inner,
                TyKind::String => return Some(1),
                TyKind::Vec(_) | TyKind::Slice(_) => return Some(2),
                // A struct or tuple travels as one counted node whose own
                // teardown reaches its fields.
                TyKind::Adt { def, .. } if def.local < u32::MAX - 16 => {
                    return tcx.struct_field_tys(*def).map(|_| 3);
                }
                TyKind::Tuple(_) => return Some(3),
                _ => return None,
            }
        }
    };

    // One character per 8-byte slot of an aggregate element, saying what that
    // slot owns. The channel carries a heap copy of the aggregate, so this is
    // what its teardown walks to give a value nobody received back.
    fn slot_desc(tcx: &gossamer_types::TyCtxt, ty: gossamer_types::Ty, out: &mut String) -> bool {
        use gossamer_types::TyKind;
        match tcx.kind_of(ty) {
            TyKind::Ref { inner, .. } => slot_desc(tcx, *inner, out),
            TyKind::String => {
                out.push('S');
                true
            }
            TyKind::Vec(_) | TyKind::Slice(_) => {
                out.push('V');
                true
            }
            TyKind::Int(_) | TyKind::Bool | TyKind::Char | TyKind::Float(_) => {
                out.push('s');
                true
            }
            TyKind::Tuple(items) => {
                let items = items.clone();
                !items.is_empty() && items.iter().all(|item| slot_desc(tcx, *item, out))
            }
            TyKind::Array { elem, len } => {
                let elem = *elem;
                let gossamer_types::ArrayLen::Concrete(len) = *len else {
                    return false;
                };
                len > 0 && (0..len).all(|_| slot_desc(tcx, elem, out))
            }
            TyKind::Adt { def, substs } if def.local < u32::MAX - 16 => {
                let fields = tcx
                    .adt_field_tys(*def, substs)
                    .map(<[gossamer_types::Ty]>::to_vec);
                match fields {
                    Some(fields) if !fields.is_empty() => {
                        fields.iter().all(|f| slot_desc(tcx, *f, out))
                    }
                    _ => false,
                }
            }
            _ => false,
        }
    }
    let descriptor = |ty: gossamer_types::Ty| -> Option<String> {
        let mut out = String::new();
        // A descriptor earns its place only where a slot owns storage; an
        // all-scalar aggregate has nothing to give back.
        (slot_desc(tcx, ty, &mut out) && out.bytes().any(|b| b != b's')).then_some(out)
    };

    let mut sites: Vec<(usize, Local, Local, i64, Option<String>)> = Vec::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            args,
            ..
        } = &block.terminator
        else {
            continue;
        };
        if !matches!(name.as_str(), "gos_rt_chan_send" | "gos_rt_chan_try_send") {
            continue;
        }
        let (Some(Operand::Copy(chan)), Some(Operand::Copy(val))) = (args.first(), args.get(1))
        else {
            continue;
        };
        if !chan.projection.is_empty() || (val.local.0 as usize) >= body.locals.len() {
            continue;
        }
        if !val.projection.is_empty() {
            continue;
        }
        let val_ty = body.locals[val.local.0 as usize].ty;
        let kind = kind_of(val_ty);
        // A value boxed under its structural meta is a counted copy that owns
        // its children, so releasing the box is the whole give-back.
        let boxed_with_children = tcx
            .rc_meta(&format!("gos_rc_meta_boxaggr_{}", val_ty.as_u32()))
            .is_some();
        let desc = if kind == Some(3) && !boxed_with_children {
            descriptor(val_ty)
        } else {
            None
        };
        sites.push((bi, chan.local, val.local, kind.unwrap_or(0), desc));
    }
    if sites.is_empty() {
        return;
    }
    let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
    for (bi, chan, val, kind, desc) in sites {
        // The value reaches the receiving goroutine, so everything it counts
        // switches to atomic reference counting before it is enqueued.
        let val_ty = body.locals[val.0 as usize].ty;
        if let Some((name, meta)) = send_mark_shared_call(tcx, val_ty) {
            let sink = Local(u32::try_from(body.locals.len()).expect("local overflow"));
            body.locals.push(crate::ir::LocalDecl {
                ty: unit_ty,
                debug_name: None,
                mutable: false,
                region: false,
            });
            if let Some(block) = body.blocks.get_mut(bi) {
                let span = block.span;
                let mut args = vec![Operand::Copy(Place::local(val))];
                if let Some(meta) = meta {
                    args.push(Operand::Const(ConstValue::Str(meta)));
                }
                block.stmts.push(Statement {
                    kind: StatementKind::Assign {
                        place: Place::local(sink),
                        rvalue: Rvalue::CallIntrinsic { name, args },
                    },
                    span,
                    inlined: None,
                });
            }
        }
        if kind == 0 {
            continue;
        }
        let record = |name: &'static str, arg: Operand, body: &mut Body| {
            let sink = Local(u32::try_from(body.locals.len()).expect("local overflow"));
            body.locals.push(crate::ir::LocalDecl {
                ty: unit_ty,
                debug_name: None,
                mutable: false,
                region: false,
            });
            let Some(block) = body.blocks.get_mut(bi) else {
                return;
            };
            let span = block.span;
            block.stmts.push(Statement {
                kind: StatementKind::Assign {
                    place: Place::local(sink),
                    rvalue: Rvalue::CallIntrinsic {
                        name,
                        args: vec![Operand::Copy(Place::local(chan)), arg],
                    },
                },
                span,
                inlined: None,
            });
        };
        record(
            "gos_rt_chan_set_elem_kind",
            Operand::Const(ConstValue::Int(i128::from(kind))),
            body,
        );
        if let Some(desc) = desc {
            record(
                "gos_rt_chan_set_elem_desc",
                Operand::Const(ConstValue::Str(desc)),
                body,
            );
        }
    }
}

/// Gives back the `String` / `Vec` payload of every carrier local the frame
/// owns, on every path.
///
/// A carrier minted by a call, copied from another owned carrier, or built by
/// `gos_rt_result_new` holds the payload's one share. That share leaves the
/// local through a consuming mention - an extraction (`if let`, `unwrap`,
/// `?`), a call that takes the carrier, a copy into another carrier or into the
/// return slot - and stays through a borrowing one: an arm query (`is_some`,
/// `is_ok`) or a rendering. So the local releases its payload before each
/// redefinition and at every return, and is emptied to `None` right after a
/// consuming mention; the release on a `None` arm is a no-op, which is what
/// makes the placement independent of the path taken.
///
/// A runtime call that consumes a carrier the local outlives is handed a share
/// of its own first. A local whose uses the walk cannot classify, or whose
/// other consuming mention is not its last use, is left alone, and so is every
/// carrier it exchanges values with.
pub(crate) fn own_carrier_payloads(body: &mut Body, tcx: &gossamer_types::TyCtxt) {
    use gossamer_types::TyKind;

    // Entry points that read a carrier's discriminant and nothing else.
    fn queries_arm(name: &str) -> bool {
        matches!(
            name,
            "gos_rt_result_is_ok"
                | "gos_rt_result_is_err"
                | "gos_rt_result_disc"
                | "gos_rt_option_is_some"
                | "gos_rt_option_is_none"
        )
    }
    // Entry points that render their arguments and keep none of them.
    fn renders_args(name: &str) -> bool {
        matches!(
            name,
            "__concat" | "__debug" | "println" | "print" | "eprintln" | "eprint"
        )
    }

    // The `gos_rt_result_payload_release` kinds of a carrier's two arms,
    // `(ok, err)`: `1` for a `String`, `2` for a `Vec` / slice, `4` for a
    // counted node (an `errors::Error` cell, a payload-enum node, or a
    // callable's environment), `0` for an arm whose payload the helper does
    // not own. `None` when neither arm is one.
    let payload_kind = |ty: gossamer_types::Ty| -> Option<(i64, i64)> {
        let arm = |payload: Option<&gossamer_types::Ty>| match payload {
            Some(t) if tcx.is_counted_node(*t) => 4,
            Some(t) => match tcx.kind_of(*t) {
                TyKind::String => 1,
                TyKind::Vec(_) | TyKind::Slice(_) => 2,
                TyKind::DynError => 4,
                _ => 0,
            },
            None => 0,
        };
        match tcx.kind_of(ty) {
            TyKind::Adt { def, substs } if def.local == u32::MAX || def.local == u32::MAX - 1 => {
                let tys = substs.types();
                // An arm holding a counted aggregate blob makes the carrier an
                // option holder, whose walk owns the blob and releases it
                // after every field copied out of it has taken its share.
                if tys.iter().take(2).any(|p| tcx.aggr_copy_meta(*p).is_some()) {
                    return None;
                }
                let kinds = (arm(tys.first()), arm(tys.get(1)));
                (kinds != (0, 0)).then_some(kinds)
            }
            _ => None,
        }
    };

    let n_locals = body.locals.len();
    let arity = body.arity as usize;
    // A runtime call answering a counted aggregate blob makes its destination
    // an option holder, which the holder walk owns the way it owns a carrier
    // whose arm type names the blob.
    let mut counted_answer = vec![false; n_locals];
    for block in &body.blocks {
        if let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            destination,
            ..
        } = &block.terminator
            && destination.projection.is_empty()
            && (destination.local.0 as usize) < n_locals
            && answers_counted_blob(name, destination.local, body, tcx)
        {
            counted_answer[destination.local.0 as usize] = true;
        }
    }
    let kinds: Vec<Option<(i64, i64)>> = (0..n_locals)
        .map(|i| {
            if i == 0 || body.locals[i].region || counted_answer[i] {
                None
            } else {
                payload_kind(body.locals[i].ty)
            }
        })
        .collect();
    if kinds.iter().all(Option::is_none) {
        return;
    }
    let is_carrier = |local: usize| local < n_locals && kinds[local].is_some();
    // A by-value carrier parameter is the caller's value, lent for the call.
    // A mention that hands it on takes a share of its own for what it hands
    // over, and a parameter the body reassigns takes a share at entry and is
    // then owned the way any other carrier local is.
    let is_param = |local: usize| (1..=arity).contains(&local);
    let mut reassigned = vec![false; n_locals];
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign { place, .. } = &stmt.kind
                && place.projection.is_empty()
                && (place.local.0 as usize) < n_locals
            {
                reassigned[place.local.0 as usize] = true;
            }
        }
        if let Terminator::Call { destination, .. } = &block.terminator
            && (destination.local.0 as usize) < n_locals
        {
            reassigned[destination.local.0 as usize] = true;
        }
    }
    let lent_param = |local: usize| is_param(local) && is_carrier(local) && !reassigned[local];

    /// Where a consuming mention sits, which decides where the emptying goes.
    #[derive(Clone, Copy)]
    enum Site {
        /// Statement `si` of block `bi`: emptied right after it.
        Stmt(usize, usize),
        /// The call ending block `bi`: emptied at the head of its target.
        Call(usize),
    }

    // Temporaries built only to be read by an ordering comparator: a key
    // wrapped in a one-element tuple for `gos_rt_desc_cmp`. The comparator
    // reads the words and keeps none of them, so a carrier placed in one is
    // lent rather than handed over.
    let mut comparator_temp = vec![true; n_locals];
    {
        let mut built = vec![false; n_locals];
        for block in &body.blocks {
            for stmt in &block.stmts {
                match &stmt.kind {
                    StatementKind::Assign {
                        place,
                        rvalue: Rvalue::Aggregate { .. },
                    } if place.projection.is_empty() && (place.local.0 as usize) < n_locals => {
                        built[place.local.0 as usize] = true;
                        for_each_rvalue_place(
                            match &stmt.kind {
                                StatementKind::Assign { rvalue, .. } => rvalue,
                                _ => unreachable!("matched an assignment above"),
                            },
                            &mut |p| {
                                if (p.local.0 as usize) < n_locals {
                                    comparator_temp[p.local.0 as usize] = false;
                                }
                            },
                        );
                    }
                    other => for_each_stmt_place(other, &mut |p| {
                        if (p.local.0 as usize) < n_locals {
                            comparator_temp[p.local.0 as usize] = false;
                        }
                    }),
                }
            }
            let reads_only = matches!(
                &block.terminator,
                Terminator::Call { callee: Operand::Const(ConstValue::Str(name)), .. }
                    if name == "gos_rt_desc_cmp"
            );
            match &block.terminator {
                Terminator::Call {
                    callee,
                    args,
                    destination,
                    ..
                } => {
                    if (destination.local.0 as usize) < n_locals {
                        comparator_temp[destination.local.0 as usize] = false;
                    }
                    for op in std::iter::once(callee).chain(args) {
                        if let Operand::Copy(p) = op
                            && (p.local.0 as usize) < n_locals
                            && !(reads_only && p.projection.is_empty())
                        {
                            comparator_temp[p.local.0 as usize] = false;
                        }
                    }
                }
                other => {
                    for local in 0..n_locals {
                        if crate::opt::term_mentions_local(other, Local(local as u32)) {
                            comparator_temp[local] = false;
                        }
                    }
                }
            }
        }
        for (local, built) in built.into_iter().enumerate() {
            comparator_temp[local] &= built;
        }
    }

    // Whether the by-value aggregate a local holds gives back the carrier
    // payloads its slots own - the field walk's reach.
    let owns_carrier_fields = |local: usize| {
        let ty = body.locals[local].ty;
        let by_value = match tcx.kind_of(ty) {
            TyKind::Tuple(_) | TyKind::Array { .. } => true,
            TyKind::Adt { def, .. } => {
                def.local < u32::MAX - 16 && !tcx.is_inline_enum_ty(ty) && !tcx.is_rc_managed(ty)
            }
            _ => false,
        };
        by_value
            && aggregate_rc_field_paths(tcx, ty)
                .iter()
                .any(|(_, kind)| matches!(kind, FieldRcKind::Carrier { .. }))
    };
    // Whether a box of a carrier of type `ty` owns the carrier's payload: only
    // the structural meta names the payload word as a child.
    let box_owns_payload = |ty: gossamer_types::Ty| {
        tcx.rc_meta(&format!("gos_rc_meta_boxaggr_{}", ty.as_u32()))
            .is_some()
    };
    // Whether a `gos_rt_result_new` payload operand is a carrier the backend
    // copies into a counted box, which takes a share of its payload.
    let boxes_with_share = |op: &Operand| {
        let Operand::Copy(p) = op else {
            return false;
        };
        if !p.projection.is_empty() || p.local.0 as usize >= n_locals {
            return false;
        }
        kinds[p.local.0 as usize].is_some() && box_owns_payload(body.locals[p.local.0 as usize].ty)
    };
    // Carriers read out of a box, which keeps its own share: each takes one
    // of its own right after the read.
    let mut views: Vec<(usize, Site)> = Vec::new();
    let aliases = bare_copies(body);
    // `(carrier, block, stmt)`: an aggregate built at that statement took a
    // share of the carrier's payload.
    let mut aggregate_shares: Vec<(usize, usize, usize)> = Vec::new();
    let mut withdrawn = vec![false; n_locals];
    let mut copies: Vec<(usize, usize)> = Vec::new();
    let mut consumes: Vec<(usize, Site)> = Vec::new();
    // Mentions that hand a lent parameter on, each taking a share first.
    let mut escapes: Vec<(usize, Site)> = Vec::new();
    // Redefinitions: before statement `si` of block `bi`, or before the call
    // ending block `bi` (`None`).
    let mut redefinitions: Vec<(usize, usize, Option<usize>)> = Vec::new();
    let mut predecessors = vec![0u32; body.blocks.len()];

    let mentions_of = |op: &Operand, out: &mut Vec<usize>| {
        if let Operand::Copy(p) = op {
            out.push(p.local.0 as usize);
        }
    };

    for (bi, block) in body.blocks.iter().enumerate() {
        for (si, stmt) in block.stmts.iter().enumerate() {
            // The give-back of an arm a call left unheld belongs to that call's
            // mention, which the terminator walk classifies.
            if let StatementKind::Assign {
                rvalue: Rvalue::CallIntrinsic { args, .. },
                ..
            } = &stmt.kind
                && let Some(Operand::Copy(p)) = args.first()
                && gives_back_payload(stmt, p.local)
            {
                continue;
            }
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                for_each_stmt_place(&stmt.kind, &mut |p| {
                    if (p.local.0 as usize) < n_locals {
                        withdrawn[p.local.0 as usize] = true;
                    }
                });
                continue;
            };
            let dest = place.local.0 as usize;
            if is_carrier(dest) {
                if place.projection.is_empty() {
                    let owned_def = match rvalue {
                        // A carrier copied out of an aggregate's field leaves
                        // the field holding its share, so the binding takes
                        // one of its own and gives it back like any owner.
                        Rvalue::Use(Operand::Copy(src))
                            if !src.projection.is_empty()
                                && (src.local.0 as usize) < n_locals
                                && src
                                    .projection
                                    .iter()
                                    .all(|p| matches!(p, crate::ir::Projection::Field(_))) =>
                        {
                            views.push((dest, Site::Stmt(bi, si)));
                            true
                        }
                        Rvalue::Use(Operand::Copy(src)) => {
                            src.projection.is_empty()
                                && is_carrier(src.local.0 as usize)
                                && kinds[src.local.0 as usize] == kinds[dest]
                        }
                        Rvalue::CallIntrinsic { name, .. } => {
                            *name == "gos_rt_result_new" || reads_boxed_carrier(name)
                        }
                        _ => false,
                    };
                    if let Rvalue::CallIntrinsic { name, .. } = rvalue
                        && reads_boxed_carrier(name)
                    {
                        if box_owns_payload(body.locals[dest].ty) {
                            views.push((dest, Site::Stmt(bi, si)));
                        } else {
                            withdrawn[dest] = true;
                        }
                    }
                    if owned_def {
                        redefinitions.push((dest, bi, Some(si)));
                    } else {
                        withdrawn[dest] = true;
                    }
                } else {
                    withdrawn[dest] = true;
                }
            }
            match rvalue {
                Rvalue::Use(Operand::Copy(src))
                    if src.projection.is_empty() && is_carrier(src.local.0 as usize) =>
                {
                    let s = src.local.0 as usize;
                    if lent_param(s) {
                        escapes.push((s, Site::Stmt(bi, si)));
                    } else {
                        if place.projection.is_empty()
                            && is_carrier(dest)
                            && kinds[s] == kinds[dest]
                        {
                            copies.push((dest, s));
                        }
                        consumes.push((s, Site::Stmt(bi, si)));
                    }
                }
                Rvalue::CallIntrinsic { name, args } => {
                    let mut locals = Vec::new();
                    for (idx, arg) in args.iter().enumerate() {
                        if idx == 0 && queries_arm(name) {
                            continue;
                        }
                        // A carrier payload is copied into a box holding its
                        // own share, so the operand keeps the one it has.
                        if idx == 1 && *name == "gos_rt_result_new" && boxes_with_share(arg) {
                            continue;
                        }
                        // The release pass takes no share out of the extraction
                        // of an aliased carrier or of a parameter, so the carrier
                        // keeps its own.
                        if idx == 0
                            && *name == "gos_rt_result_payload"
                            && matches!(arg, Operand::Copy(p)
                                if p.projection.is_empty()
                                    && (p.local.0 as usize) < n_locals
                                    && (aliases.sourced[p.local.0 as usize]
                                        || aliases.target[p.local.0 as usize]
                                        || is_param(p.local.0 as usize)))
                        {
                            continue;
                        }
                        mentions_of(arg, &mut locals);
                    }
                    for l in locals.into_iter().filter(|l| is_carrier(*l)) {
                        if lent_param(l) {
                            escapes.push((l, Site::Stmt(bi, si)));
                        } else {
                            consumes.push((l, Site::Stmt(bi, si)));
                        }
                    }
                    for arg in args {
                        if let Operand::Copy(p) = arg
                            && !p.projection.is_empty()
                            && (p.local.0 as usize) < n_locals
                        {
                            withdrawn[p.local.0 as usize] = true;
                        }
                    }
                }
                Rvalue::Aggregate { .. }
                    if place.projection.is_empty() && comparator_temp[dest.min(n_locals - 1)] => {}
                // A tuple, array, or by-value struct takes a share of each
                // carrier slot it is built from, and gives it back at its own
                // death, so the operand is lent and keeps its share.
                Rvalue::Aggregate { operands, .. }
                    if place.projection.is_empty()
                        && dest < n_locals
                        && owns_carrier_fields(dest) =>
                {
                    for op in operands {
                        match op {
                            // An aggregate's carrier field owns the `Ok` /
                            // `Some` arm only.
                            Operand::Copy(p)
                                if p.projection.is_empty()
                                    && kinds
                                        .get(p.local.0 as usize)
                                        .copied()
                                        .flatten()
                                        .is_some_and(|(ok, err)| ok != 0 && err == 0) =>
                            {
                                aggregate_shares.push((p.local.0 as usize, bi, si));
                            }
                            Operand::Copy(p) if (p.local.0 as usize) < n_locals => {
                                withdrawn[p.local.0 as usize] = true;
                            }
                            _ => {}
                        }
                    }
                }
                _ => for_each_rvalue_place(rvalue, &mut |p| {
                    if (p.local.0 as usize) < n_locals {
                        withdrawn[p.local.0 as usize] = true;
                    }
                }),
            }
        }
        match &block.terminator {
            Terminator::Call {
                callee,
                args,
                destination,
                target,
            } => {
                let name = match callee {
                    Operand::Const(ConstValue::Str(name)) => name.as_str(),
                    _ => "",
                };
                if let Operand::Copy(p) = callee
                    && (p.local.0 as usize) < n_locals
                {
                    withdrawn[p.local.0 as usize] = true;
                }
                let dest = destination.local.0 as usize;
                let gossamer_callee = match callee {
                    Operand::FnRef { .. } | Operand::Copy(_) => true,
                    Operand::Const(ConstValue::Str(name)) => {
                        !name.starts_with("gos_rt_") && name != "gos_load" && name != "gos_store"
                    }
                    Operand::Const(_) => false,
                };
                let mut consumed_here = Vec::new();
                for (idx, arg) in args.iter().enumerate() {
                    let Operand::Copy(p) = arg else {
                        continue;
                    };
                    let l = p.local.0 as usize;
                    if l >= n_locals {
                        continue;
                    }
                    if !p.projection.is_empty() {
                        withdrawn[l] = true;
                        continue;
                    }
                    // A vec that holds carriers owns their payloads, so its push
                    // takes a share of its own and the argument keeps its one.
                    // A map keeps a two-word value in a box of its own, copied
                    // at the call, and a `get_or` default is read in place, so
                    // the argument keeps its share either way.
                    let map_copies_value = name.starts_with("gos_rt_map_get_or")
                        || ((name.starts_with("gos_rt_map_insert")
                            || name.starts_with("gos_rt_map_or_insert"))
                            && {
                                let ty = body.locals[l].ty;
                                tcx.rc_meta(&format!("gos_rc_meta_boxaggr_{}", ty.as_u32()))
                                    .is_some()
                                    || tcx.aggr_copy_meta(ty).is_some()
                            });
                    // A boxed-carrier reader answers words the box still owns
                    // and takes its own share of them, so the fallback it may
                    // answer instead is lent the same way. A Gossamer callee
                    // takes its by-value parameters as borrows it cannot
                    // outlive, so a carrier handed to one stays this frame's.
                    if !is_carrier(l)
                        || renders_args(name)
                        || (idx == 0 && queries_arm(name))
                        || name.starts_with("gos_rt_vec_push")
                        || map_copies_value
                        || reads_boxed_carrier(name)
                        || gossamer_callee
                    {
                        continue;
                    }
                    consumed_here.push(l);
                }
                for l in consumed_here {
                    match target {
                        _ if lent_param(l) => escapes.push((l, Site::Call(bi))),
                        Some(_) => consumes.push((l, Site::Call(bi))),
                        None => withdrawn[l] = true,
                    }
                }
                if is_carrier(dest)
                    && destination.projection.is_empty()
                    && target.is_some()
                    && reads_boxed_carrier(name)
                {
                    if box_owns_payload(body.locals[dest].ty) {
                        views.push((dest, Site::Call(bi)));
                    } else {
                        withdrawn[dest] = true;
                    }
                }
                if is_carrier(dest) {
                    // A carrier read out of a container slot borrows the
                    // payload the container still owns.
                    if destination.projection.is_empty()
                        && target.is_some()
                        && !answers_borrowed_element(name)
                        && !returns_borrowed_pointer(name)
                    {
                        redefinitions.push((dest, bi, None));
                    } else {
                        withdrawn[dest] = true;
                    }
                }
            }
            Terminator::SwitchInt { discriminant, .. } => {
                if let Operand::Copy(p) = discriminant
                    && (p.local.0 as usize) < n_locals
                {
                    withdrawn[p.local.0 as usize] = true;
                }
            }
            Terminator::Assert { cond, msg, .. } => {
                for op in std::iter::once(cond).chain(msg.operands()) {
                    if let Operand::Copy(p) = op
                        && (p.local.0 as usize) < n_locals
                    {
                        withdrawn[p.local.0 as usize] = true;
                    }
                }
            }
            Terminator::Drop { place, .. } => {
                if (place.local.0 as usize) < n_locals {
                    withdrawn[place.local.0 as usize] = true;
                }
            }
            Terminator::Goto { .. }
            | Terminator::Return
            | Terminator::Unreachable
            | Terminator::Panic { .. } => {}
        }
        for succ in successors_of(&block.terminator) {
            if let Some(count) = predecessors.get_mut(succ) {
                *count += 1;
            }
        }
    }

    // A lent parameter is the caller's to release, so this frame never owns it.
    for (local, slot) in withdrawn.iter_mut().enumerate() {
        if lent_param(local) {
            *slot = true;
        }
    }
    // A redefinition that also reads the local hands the old value to the
    // call or rvalue defining the new one, so the release before it would free
    // what that reader takes.
    for &(local, bi, si) in &redefinitions {
        let block = &body.blocks[bi];
        let reads_itself = match si {
            Some(si) => match &block.stmts[si].kind {
                StatementKind::Assign { rvalue, .. } => {
                    crate::opt::rvalue_mentions_local(rvalue, Local(local as u32))
                }
                _ => true,
            },
            None => match &block.terminator {
                Terminator::Call { callee, args, .. } => std::iter::once(callee)
                    .chain(args)
                    .any(|op| matches!(op, Operand::Copy(p) if p.local.0 as usize == local)),
                _ => true,
            },
        };
        if reads_itself {
            withdrawn[local] = true;
        }
    }
    // A consuming mention empties the local, which is sound only when nothing
    // reads it afterwards, and a call's emptying needs a target only that
    // call reaches.
    let mut kept_consumes = Vec::with_capacity(consumes.len());
    // `(carrier, block, arm)`: the call ending the block takes a share of the
    // carrier before it runs - of the one arm named, or of both.
    let mut handed_shares: Vec<(usize, usize, Option<usize>)> = Vec::new();
    for &(local, site) in &consumes {
        let last_use = match site {
            Site::Stmt(bi, si) => copy_is_last_use(body, (bi, si), Local(local as u32)),
            Site::Call(bi) => {
                let single_entry = match &body.blocks[bi].terminator {
                    Terminator::Call {
                        target: Some(t), ..
                    } => predecessors.get(t.0 as usize).copied() == Some(1),
                    _ => false,
                };
                let read_later = !call_is_last_read(body, bi, Local(local as u32));
                // A runtime call the local outlives is handed a share of its
                // own, so the local keeps the one it holds for its later
                // readers. A call that leaves one receiver arm unheld is
                // handed only the arm its answer keeps, since nothing gives
                // the other arm back while the local still holds it.
                if read_later
                    && let Terminator::Call { callee, args, .. } = &body.blocks[bi].terminator
                {
                    let name = match callee {
                        Operand::Const(ConstValue::Str(name)) => name.as_str(),
                        _ => "",
                    };
                    let answered_arm = discarded_receiver_arm(name).and_then(|(arm, recv_at)| {
                        matches!(args.get(recv_at), Some(Operand::Copy(p))
                            if p.projection.is_empty() && p.local.0 as usize == local)
                        .then_some(1 - arm)
                    });
                    handed_shares.push((local, bi, answered_arm));
                    continue;
                }
                single_entry && !read_later
            }
        };
        if last_use {
            kept_consumes.push((local, site));
            continue;
        }
        // A copy into another carrier binding that the source outlives leaves
        // both holding the value, so the copy takes a share of its own and
        // neither is emptied.
        let shared_copy = match site {
            Site::Stmt(bi, si) => match &body.blocks[bi].stmts[si].kind {
                StatementKind::Assign {
                    place,
                    rvalue: Rvalue::Use(Operand::Copy(src)),
                } if place.projection.is_empty()
                    && src.projection.is_empty()
                    && src.local.0 as usize == local
                    && place.local != Local::RETURN
                    && copies.contains(&(place.local.0 as usize, local)) =>
                {
                    Some(place.local.0 as usize)
                }
                _ => None,
            },
            Site::Call(_) => None,
        };
        match shared_copy {
            Some(dest) => {
                views.push((dest, site));
                copies.retain(|&pair| pair != (dest, local));
            }
            None => {
                withdrawn[local] = true;
                kept_consumes.push((local, site));
            }
        }
    }
    consumes = kept_consumes;
    // Two carriers that exchange a value share its fate.
    loop {
        let mut changed = false;
        for &(dest, src) in &copies {
            if withdrawn[dest] != withdrawn[src] {
                withdrawn[dest] = true;
                withdrawn[src] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let owner = |local: usize| is_carrier(local) && !withdrawn[local];
    if (0..n_locals).all(|l| !owner(l))
        && aggregate_shares.is_empty()
        && views.is_empty()
        && escapes.is_empty()
        && handed_shares.is_empty()
    {
        return;
    }
    let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
    let new_local = |body: &mut Body| {
        let sink = Local(u32::try_from(body.locals.len()).expect("local overflow"));
        body.locals.push(crate::ir::LocalDecl {
            ty: unit_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
        sink
    };
    let release_stmt = |sink: Local, local: usize, span| Statement {
        kind: StatementKind::Assign {
            place: Place::local(sink),
            rvalue: Rvalue::CallIntrinsic {
                name: "gos_rt_result_payload_release",
                args: vec![
                    Operand::Copy(Place::local(Local(local as u32))),
                    Operand::Const(ConstValue::Int(i128::from(kinds[local].map_or(0, |k| k.0)))),
                    Operand::Const(ConstValue::Int(i128::from(kinds[local].map_or(0, |k| k.1)))),
                ],
            },
        },
        span,
        inlined: None,
    };
    let retain_stmt = |sink: Local, local: usize, span| Statement {
        kind: StatementKind::Assign {
            place: Place::local(sink),
            rvalue: Rvalue::CallIntrinsic {
                name: "gos_rt_result_payload_retain",
                args: vec![
                    Operand::Copy(Place::local(Local(local as u32))),
                    Operand::Const(ConstValue::Int(i128::from(kinds[local].map_or(0, |k| k.0)))),
                    Operand::Const(ConstValue::Int(i128::from(kinds[local].map_or(0, |k| k.1)))),
                ],
            },
        },
        span,
        inlined: None,
    };
    let empty_stmt = |local: usize, span| Statement {
        kind: StatementKind::Assign {
            place: Place::local(Local(local as u32)),
            rvalue: Rvalue::CallIntrinsic {
                name: "gos_rt_result_new",
                args: vec![
                    Operand::Const(ConstValue::Int(1)),
                    Operand::Const(ConstValue::Int(0)),
                ],
            },
        },
        span,
        inlined: None,
    };

    // `(block, position, rank, statement)`, applied from the highest position
    // down so each insertion leaves the positions below it valid. Statements
    // sharing a position run in rank order: an entry emptying or entry share
    // first (`ENTRY`), then the emptying of a value a mention just handed over
    // (`EMPTY`), then releases (`RELEASE`), which so read either a share the
    // local still holds or an empty arm, and shares taken last (`RETAIN`).
    const ENTRY: u8 = 0;
    const EMPTY: u8 = 1;
    const RELEASE: u8 = 2;
    const RETAIN: u8 = 3;
    let mut inserts: Vec<(usize, usize, u8, Statement)> = Vec::new();
    // The aggregate's field walk gives back both arms' payloads, so the share
    // it takes covers both arms too.
    for &(local, bi, si) in &aggregate_shares {
        let sink = new_local(body);
        let span = body.blocks[bi].span;
        inserts.push((bi, si + 1, RETAIN, retain_stmt(sink, local, span)));
    }
    for &(local, bi, si) in &redefinitions {
        if !owner(local) {
            continue;
        }
        let sink = new_local(body);
        let span = body.blocks[bi].span;
        let position = si.unwrap_or(body.blocks[bi].stmts.len());
        inserts.push((bi, position, RELEASE, release_stmt(sink, local, span)));
    }
    for &(local, site) in &consumes {
        if !owner(local) {
            continue;
        }
        match site {
            Site::Stmt(bi, si) => {
                let span = body.blocks[bi].span;
                inserts.push((bi, si + 1, EMPTY, empty_stmt(local, span)));
            }
            Site::Call(bi) => {
                if let Terminator::Call {
                    target: Some(t), ..
                } = &body.blocks[bi].terminator
                {
                    let target = t.0 as usize;
                    let span = body.blocks[target].span;
                    // The give-back of the arm the call left unheld reads the
                    // carrier first, so the emptying follows it.
                    let position = body.blocks[target]
                        .stmts
                        .iter()
                        .take_while(|stmt| gives_back_payload(stmt, Local(local as u32)))
                        .count();
                    inserts.push((target, position, EMPTY, empty_stmt(local, span)));
                }
            }
        }
    }
    // A view takes its share whether or not the walk owns it: an owner gives
    // the share back, and a local the walk leaves alone keeps the box's
    // payload alive rather than reading it after the box is gone.
    for &(local, site) in &views {
        let (bi, position) = match site {
            Site::Stmt(bi, si) => (bi, si + 1),
            Site::Call(bi) => match &body.blocks[bi].terminator {
                Terminator::Call {
                    target: Some(t), ..
                } => (t.0 as usize, 0),
                _ => continue,
            },
        };
        let sink = new_local(body);
        let span = body.blocks[bi].span;
        inserts.push((bi, position, RETAIN, retain_stmt(sink, local, span)));
    }
    // A lent parameter takes the share it hands on right before the mention
    // that hands it on, which leaves the parameter itself as it was.
    for &(local, site) in &escapes {
        let (bi, position) = match site {
            Site::Stmt(bi, si) => (bi, si),
            Site::Call(bi) => (bi, body.blocks[bi].stmts.len()),
        };
        let sink = new_local(body);
        let span = body.blocks[bi].span;
        inserts.push((bi, position, RETAIN, retain_stmt(sink, local, span)));
    }
    for &(local, bi, arm) in &handed_shares {
        if !owner(local) {
            continue;
        }
        let sink = new_local(body);
        let span = body.blocks[bi].span;
        let mut stmt = retain_stmt(sink, local, span);
        if let (
            Some(arm),
            StatementKind::Assign {
                rvalue: Rvalue::CallIntrinsic { args, .. },
                ..
            },
        ) = (arm, &mut stmt.kind)
        {
            args[2 - arm] = Operand::Const(ConstValue::Int(0));
        }
        let position = body.blocks[bi].stmts.len();
        inserts.push((bi, position, RETAIN, stmt));
    }
    let returns: Vec<usize> = body
        .blocks
        .iter()
        .enumerate()
        .filter(|(_, block)| matches!(block.terminator, Terminator::Return))
        .map(|(bi, _)| bi)
        .collect();
    for local in (0..n_locals).filter(|l| owner(*l)) {
        for &bi in &returns {
            let sink = new_local(body);
            let span = body.blocks[bi].span;
            let position = body.blocks[bi].stmts.len();
            inserts.push((bi, position, RELEASE, release_stmt(sink, local, span)));
        }
        let span = body.blocks[0].span;
        // A reassigned parameter arrives holding the caller's payload, so it
        // takes a share of that at entry where any other local starts empty.
        if is_param(local) {
            let sink = new_local(body);
            inserts.push((0, 0, ENTRY, retain_stmt(sink, local, span)));
        } else {
            inserts.push((0, 0, ENTRY, empty_stmt(local, span)));
        }
    }
    inserts.sort_by_key(|a| (a.0, a.1, a.2));
    for (bi, position, _, stmt) in inserts.into_iter().rev() {
        let stmts = &mut body.blocks[bi].stmts;
        let position = position.min(stmts.len());
        stmts.insert(position, stmt);
    }
}

/// The locals bare whole-local copies (`dest = Copy(src)`) connect.
struct BareCopies {
    /// Read whole by a copy.
    sourced: Vec<bool>,
    /// Read whole by a copy into a binding. A copy into the return slot hands
    /// the share to the caller only on the path that returns, so a payload
    /// read out of a carrier is still this frame's on every other.
    sourced_to_binding: Vec<bool>,
    /// Written whole by a copy. With `sourced` this flags an enum value that
    /// is aliased (copied to or from another binding): its by-value payload
    /// pointer is shared, so no extraction owns or releases it, since matching
    /// both aliases would free the one payload twice.
    target: Vec<bool>,
}

fn bare_copies(body: &Body) -> BareCopies {
    let n_locals = body.locals.len();
    let mut facts = BareCopies {
        sourced: vec![false; n_locals],
        sourced_to_binding: vec![false; n_locals],
        target: vec![false; n_locals],
    };
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign {
                place,
                rvalue: Rvalue::Use(Operand::Copy(p)),
            } = &stmt.kind
                && p.projection.is_empty()
                && (p.local.0 as usize) < n_locals
            {
                facts.sourced[p.local.0 as usize] = true;
                if place.local != Local::RETURN {
                    facts.sourced_to_binding[p.local.0 as usize] = true;
                }
                if place.projection.is_empty() && (place.local.0 as usize) < n_locals {
                    facts.target[place.local.0 as usize] = true;
                }
            }
        }
    }
    facts
}

/// Readers that answer the carrier a two-word payload was boxed as: the words
/// stay the box's, so the answer is a view of what the box owns.
fn reads_boxed_carrier(name: &str) -> bool {
    matches!(
        name,
        "gos_rt_result_payload_i128"
            | "gos_rt_option_unwrap_carrier"
            | "gos_rt_result_unwrap_carrier"
            | "gos_rt_result_unwrap_or_carrier"
    )
}

/// Whether `stmt` releases the payload `local` holds.
fn gives_back_payload(stmt: &Statement, local: Local) -> bool {
    matches!(&stmt.kind, StatementKind::Assign {
        rvalue: Rvalue::CallIntrinsic { name, args },
        ..
    } if matches!(*name, "gos_rt_result_payload_release" | "gos_rt_result_ok_payload_release")
        && matches!(args.first(), Some(Operand::Copy(p))
            if p.projection.is_empty() && p.local == local))
}

/// Whether nothing reads `local` after the call ending block `bi`, on any path
/// up to wherever `local` is next defined, counting a give-back of its payload
/// as no read: releasing what the carrier holds hands nothing to a reader.
fn call_is_last_read(body: &Body, bi: usize, local: Local) -> bool {
    let mut work = successors_of(&body.blocks[bi].terminator);
    let mut seen = vec![false; body.blocks.len()];
    while let Some(b) = work.pop() {
        if b >= body.blocks.len() || std::mem::replace(&mut seen[b], true) {
            continue;
        }
        let block = &body.blocks[b];
        let rest: Vec<Statement> = block
            .stmts
            .iter()
            .filter(|stmt| !gives_back_payload(stmt, local))
            .cloned()
            .collect();
        match scan_for_local(&rest, Some(&block.terminator), local) {
            LocalScan::Mentioned => return false,
            LocalScan::Redefined => {}
            LocalScan::Clear => work.extend(successors_of(&block.terminator)),
        }
    }
    true
}

/// Whether the call ending block `bi` holds the last mention of `local` on
/// every path out of it, up to wherever `local` is next defined.
fn terminator_is_last_use(body: &Body, bi: usize, local: Local) -> bool {
    let mut work = successors_of(&body.blocks[bi].terminator);
    let mut seen = vec![false; body.blocks.len()];
    while let Some(b) = work.pop() {
        if b >= body.blocks.len() || std::mem::replace(&mut seen[b], true) {
            continue;
        }
        let block = &body.blocks[b];
        match scan_for_local(&block.stmts, Some(&block.terminator), local) {
            LocalScan::Mentioned => return false,
            LocalScan::Redefined => {}
            LocalScan::Clear => work.extend(successors_of(&block.terminator)),
        }
    }
    true
}

/// Every place an rvalue reads.
fn for_each_rvalue_place(rvalue: &Rvalue, f: &mut impl FnMut(&Place)) {
    let mut operand = |op: &Operand| {
        if let Operand::Copy(p) = op {
            f(p);
        }
    };
    match rvalue {
        Rvalue::Use(op)
        | Rvalue::UnaryOp { operand: op, .. }
        | Rvalue::Cast { operand: op, .. }
        | Rvalue::Repeat { value: op, .. } => operand(op),
        Rvalue::BinaryOp { lhs, rhs, .. } => {
            operand(lhs);
            operand(rhs);
        }
        Rvalue::Aggregate { operands, .. } | Rvalue::CallIntrinsic { args: operands, .. } => {
            for op in operands {
                operand(op);
            }
        }
        Rvalue::Len(place) | Rvalue::Ref { place, .. } => f(place),
        Rvalue::StaticLoad(_) => {}
    }
}

/// Every place a non-assignment statement reads or writes.
fn for_each_stmt_place(kind: &StatementKind, f: &mut impl FnMut(&Place)) {
    let mut operand = |op: &Operand| {
        if let Operand::Copy(p) = op {
            f(p);
        }
    };
    match kind {
        StatementKind::Assign { place, rvalue } => {
            f(place);
            for_each_rvalue_place(rvalue, f);
        }
        StatementKind::StaticStore { value, .. } => operand(value),
        StatementKind::IterSource { source, .. } => operand(source),
        StatementKind::IterAdapter {
            closure_or_arg: Some(op),
            ..
        } => operand(op),
        _ => {}
    }
}

/// The receiver arm a `Result` entry point leaves nobody holding, with the
/// receiver's argument position: `0` for `Ok`, `1` for `Err`.
///
/// `map` and `and_then` hand the closure the `Ok` payload, `map_err` and
/// `or_else` the `Err` one. A call that answers the `Ok` payload or a fallback,
/// or turns the carrier into an `Option` of one arm, leaves the other arm's
/// payload unheld. The arm it does not name is the one its answer holds.
fn discarded_receiver_arm(name: &str) -> Option<(usize, usize)> {
    Some(match name {
        "gos_rt_result_map" | "gos_rt_result_map_bare" | "gos_rt_result_and_then" => (0, 0),
        "gos_rt_result_map_i64" => (0, 1),
        "gos_rt_result_default" | "gos_rt_result_default_f64" => (1, 1),
        "gos_rt_result_to_opt_err" | "gos_rt_result_err" => (0, 0),
        "gos_rt_result_map_err"
        | "gos_rt_result_map_err_bare"
        | "gos_rt_result_or_else"
        | "gos_rt_result_unwrap_or"
        | "gos_rt_result_unwrap_or_str"
        | "gos_rt_result_unwrap_or_node"
        | "gos_rt_result_unwrap_or_vec"
        | "gos_rt_result_unwrap_or_carrier"
        | "gos_rt_result_default_with"
        | "gos_rt_result_ok"
        | "gos_rt_result_to_opt_ok" => (1, 0),
        _ => return None,
    })
}

/// Emits the give-back for `carrier.map(f)`.
///
/// `map` hands the payload to the closure, which answers a value of its own (a
/// parameter it returns unchanged mints the caller's share), so the receiver's
/// payload has no holder left. A carrier never releases a `String` or `Vec`
/// payload of its own accord, so the release is emitted at the call. The
/// helper answers the arm: an `Err` / `None` payload word belongs to the value
/// the mapped carrier still carries.
pub(crate) fn release_mapped_payloads(body: &mut Body, tcx: &gossamer_types::TyCtxt) {
    use gossamer_types::TyKind;

    // `arm` is 0 for the `Ok` / `Some` payload and 1 for the `Err` payload.
    let payload_kind = |ty: gossamer_types::Ty, arm: usize| -> Option<i64> {
        let mut cur = ty;
        loop {
            match tcx.kind_of(cur) {
                TyKind::Ref { inner, .. } => cur = *inner,
                TyKind::Adt { def, substs }
                    if def.local == u32::MAX || def.local == u32::MAX - 1 =>
                {
                    return substs.types().get(arm).and_then(|payload| {
                        if tcx.is_counted_node(*payload) {
                            return Some(4);
                        }
                        match tcx.kind_of(*payload) {
                            TyKind::String => Some(1),
                            TyKind::Vec(_) | TyKind::Slice(_) => Some(2),
                            TyKind::DynError => Some(4),
                            _ => None,
                        }
                    });
                }
                _ => return None,
            }
        }
    };

    let mut counted_answer_dest = vec![false; body.locals.len()];
    for block in &body.blocks {
        if let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            destination,
            ..
        } = &block.terminator
            && destination.projection.is_empty()
            && (destination.local.0 as usize) < body.locals.len()
            && answers_counted_blob(name, destination.local, body, tcx)
        {
            counted_answer_dest[destination.local.0 as usize] = true;
        }
    }
    let mut sites: Vec<(usize, Local, i64, usize)> = Vec::new();
    let mut passthrough_shares: Vec<(usize, Local, i64)> = Vec::new();
    for (block_index, block) in body.blocks.iter().enumerate() {
        let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            args,
            target: Some(target),
            ..
        } = &block.terminator
        else {
            continue;
        };
        // `map` and `and_then` answer an `Err` receiver as it is, so the
        // answer holds the receiver's error. A holder receiver keeps its own
        // share for its walk to give back, so the answer takes another.
        let passthrough_recv = match name.as_str() {
            "gos_rt_result_map" | "gos_rt_result_map_bare" | "gos_rt_result_and_then" => {
                args.first()
            }
            "gos_rt_result_map_i64" => args.get(1),
            _ => None,
        };
        if let Some(Operand::Copy(recv)) = passthrough_recv
            && recv.projection.is_empty()
            && (recv.local.0 as usize) < body.locals.len()
        {
            let recv_ty = body.locals[recv.local.0 as usize].ty;
            if (holds_counted_blob_arm(tcx, recv_ty) || counted_answer_dest[recv.local.0 as usize])
                && let Some(kind) = holder_err_kind(tcx, recv_ty)
            {
                passthrough_shares.push((block_index, recv.local, kind));
            }
        }
        let Some((arm, recv_at)) = discarded_receiver_arm(name) else {
            continue;
        };
        let Some(Operand::Copy(recv)) = args.get(recv_at) else {
            continue;
        };
        if !recv.projection.is_empty() || (recv.local.0 as usize) >= body.locals.len() {
            continue;
        }
        // An option holder's walk gives back its payloads itself.
        let recv_ty = body.locals[recv.local.0 as usize].ty;
        if holds_counted_blob_arm(tcx, recv_ty) || counted_answer_dest[recv.local.0 as usize] {
            continue;
        }
        // A receiver read again after the call still holds its payloads for
        // that reader, which gives them back on its own.
        if let Some(kind) = payload_kind(recv_ty, arm)
            && terminator_is_last_use(body, block_index, recv.local)
        {
            sites.push((target.0 as usize, recv.local, kind, arm));
        }
    }
    if sites.is_empty() && passthrough_shares.is_empty() {
        return;
    }
    let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
    for (block_index, recv, kind) in passthrough_shares {
        let sink = Local(u32::try_from(body.locals.len()).expect("local overflow"));
        body.locals.push(crate::ir::LocalDecl {
            ty: unit_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
        let block = &mut body.blocks[block_index];
        let span = block.span;
        block.stmts.push(Statement {
            kind: StatementKind::Assign {
                place: Place::local(sink),
                rvalue: Rvalue::CallIntrinsic {
                    name: "gos_rt_result_payload_retain",
                    args: vec![
                        Operand::Copy(Place::local(recv)),
                        Operand::Const(ConstValue::Int(0)),
                        Operand::Const(ConstValue::Int(i128::from(kind))),
                    ],
                },
            },
            span,
            inlined: None,
        });
    }
    for (target, recv, kind, arm) in sites {
        let sink = Local(u32::try_from(body.locals.len()).expect("local overflow"));
        body.locals.push(crate::ir::LocalDecl {
            ty: unit_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
        let Some(block) = body.blocks.get_mut(target) else {
            continue;
        };
        let span = block.span;
        block.stmts.insert(
            0,
            Statement {
                kind: StatementKind::Assign {
                    place: Place::local(sink),
                    // Each helper acts on its own arm only: the other arm's
                    // payload is the one the mapped carrier still holds.
                    rvalue: if arm == 0 {
                        Rvalue::CallIntrinsic {
                            name: "gos_rt_result_ok_payload_release",
                            args: vec![
                                Operand::Copy(Place::local(recv)),
                                Operand::Const(ConstValue::Int(i128::from(kind))),
                            ],
                        }
                    } else {
                        Rvalue::CallIntrinsic {
                            name: "gos_rt_result_payload_release",
                            args: vec![
                                Operand::Copy(Place::local(recv)),
                                Operand::Const(ConstValue::Int(0)),
                                Operand::Const(ConstValue::Int(i128::from(kind))),
                            ],
                        }
                    },
                },
                span,
                inlined: None,
            },
        );
    }
}

/// Reclaims a constructed container that is overwritten before it is ever read.
///
/// `let mut vals: Vec<T> = #[]` followed by `vals = f(..)?` builds an empty
/// container and then replaces it. The replacement disqualifies the local from
/// the per-site reuse machinery, so the construction it replaced was reaching
/// no free at all - one buffer per iteration of whatever loop it sits in.
///
/// The free is emitted only where every path back from the overwrite reaches a
/// construction of that same local with no mention of it in between. A value no
/// one read cannot have been aliased or stored, so nothing else can be holding
/// it, and the construction on every incoming path is what makes the free
/// well-defined rather than a free of an uninitialised slot.
/// The reclamation helper for a container constructor, or `None` when the name
/// is not one. Shared with [`free_overwritten_ctor_values`] so the two agree on
/// what the frame owns.
pub(crate) fn container_ctor_free(name: &str) -> Option<&'static str> {
    match name {
        "gos_rt_map_new" | "gos_rt_map_new_with_capacity" | "Map::new" | "HashMap::new" => {
            Some("gos_rt_map_free")
        }
        "gos_rt_vec_new" | "gos_rt_vec_with_capacity" | "Vec::new" => Some("gos_rt_vec_free"),
        "gos_rt_set_new" | "gos_rt_btree_set_new" | "Set::new" => Some("gos_rt_set_free"),
        _ => None,
    }
}

/// Releases a constructed container whose only use is being built into an
/// aggregate that mints its own share of it.
///
/// `rows.push(Row { vals: v })` gives the `Row` a share of `v` and the
/// container another, so `v` holds three: its own, the aggregate's, and the
/// container's. The aggregate and the container each give theirs back; nothing
/// gives back the frame's, and a loop building one row per iteration keeps
/// every `v` it ever made.
///
/// Emitted only where the local is read exactly once - by that aggregate - so
/// the value is dead the moment the aggregate has taken its share.
pub(crate) fn free_overwritten_ctor_values(
    body: &mut Body,
    tcx: &gossamer_types::TyCtxt,
    ctor_free: &dyn Fn(&str) -> Option<&'static str>,
) {
    let n = body.locals.len();
    // A rebound owner gives back every value it replaces itself.
    let rebound = rebound_vec_owners(body, tcx);
    // Constructions, by local.
    let mut ctor_at: std::collections::HashMap<u32, &'static str> =
        std::collections::HashMap::new();
    let mut ctor_blocks: std::collections::HashSet<(usize, u32)> = std::collections::HashSet::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        if let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            destination,
            ..
        } = &block.terminator
            && destination.projection.is_empty()
            && (destination.local.0 as usize) < n
            && !rebound[destination.local.0 as usize]
            && let Some(free) = ctor_free(name.as_str())
        {
            ctor_at.insert(destination.local.0, free);
            ctor_blocks.insert((bi, destination.local.0));
        }
    }
    if ctor_at.is_empty() {
        return;
    }

    let mentions =
        |op: &Operand, local: u32| -> bool { matches!(op, Operand::Copy(p) if p.local.0 == local) };
    let stmt_mentions = |stmt: &Statement, local: u32| -> bool {
        match &stmt.kind {
            StatementKind::Assign { place, rvalue } => {
                let read = match rvalue {
                    Rvalue::Use(op)
                    | Rvalue::UnaryOp { operand: op, .. }
                    | Rvalue::Cast { operand: op, .. }
                    | Rvalue::Repeat { value: op, .. } => mentions(op, local),
                    Rvalue::BinaryOp { lhs, rhs, .. } => {
                        mentions(lhs, local) || mentions(rhs, local)
                    }
                    Rvalue::Aggregate { operands, .. } => {
                        operands.iter().any(|op| mentions(op, local))
                    }
                    Rvalue::CallIntrinsic { args, .. } => args.iter().any(|op| mentions(op, local)),
                    Rvalue::Ref { place, .. } | Rvalue::Len(place) => place.local.0 == local,
                    Rvalue::StaticLoad(_) => false,
                };
                // A projected write reads the local to reach the field.
                read || (!place.projection.is_empty() && place.local.0 == local)
            }
            _ => false,
        }
    };
    let term_mentions = |t: &Terminator, local: u32| -> bool {
        match t {
            Terminator::Call { callee, args, .. } => {
                mentions(callee, local) || args.iter().any(|op| mentions(op, local))
            }
            Terminator::SwitchInt { discriminant, .. } => mentions(discriminant, local),
            Terminator::Assert { cond, msg, .. } => {
                mentions(cond, local) || msg.operands().any(|op| mentions(op, local))
            }
            Terminator::Drop { place, .. } => place.local.0 == local,
            _ => false,
        }
    };

    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); body.blocks.len()];
    for (bi, block) in body.blocks.iter().enumerate() {
        for succ in successors_of(&block.terminator) {
            if succ < preds.len() {
                preds[succ].push(bi);
            }
        }
    }

    // Overwrite sites: a whole-local assignment that does not read the local.
    let mut sites: Vec<(usize, usize, Local, &'static str)> = Vec::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        for (si, stmt) in block.stmts.iter().enumerate() {
            let StatementKind::Assign { place, .. } = &stmt.kind else {
                continue;
            };
            if !place.projection.is_empty() {
                continue;
            }
            let local = place.local.0;
            let Some(&free) = ctor_at.get(&local) else {
                continue;
            };
            if stmt_mentions(stmt, local) {
                continue;
            }
            // Every path back from here must reach a construction of `local`
            // with no mention of it in between.
            let mut ok = true;
            let mut seen_blocks: std::collections::HashSet<usize> =
                std::collections::HashSet::new();
            // (block, index one past the last statement to examine)
            let mut work: Vec<(usize, usize)> = vec![(bi, si)];
            while let Some((wb, upto)) = work.pop() {
                let mut reached_ctor = false;
                for stmt in body.blocks[wb].stmts[..upto].iter().rev() {
                    if stmt_mentions(stmt, local) {
                        ok = false;
                        break;
                    }
                    if let StatementKind::Assign { place, .. } = &stmt.kind
                        && place.projection.is_empty()
                        && place.local.0 == local
                    {
                        // An earlier whole-local write that is not the
                        // construction: its value is what this site would free,
                        // and it is not known to be owned.
                        ok = false;
                        break;
                    }
                }
                if !ok {
                    break;
                }
                if upto == body.blocks[wb].stmts.len()
                    && term_mentions(&body.blocks[wb].terminator, local)
                {
                    // The terminator reads it - unless it is the construction
                    // that defines it.
                    if ctor_blocks.contains(&(wb, local)) {
                        reached_ctor = true;
                    } else {
                        ok = false;
                        break;
                    }
                } else if ctor_blocks.contains(&(wb, local)) && upto == body.blocks[wb].stmts.len()
                {
                    reached_ctor = true;
                }
                if reached_ctor {
                    continue;
                }
                if wb == 0 || preds[wb].is_empty() {
                    // Entry reached with no construction on this path.
                    ok = false;
                    break;
                }
                for &pred in &preds[wb] {
                    if seen_blocks.insert(pred) {
                        work.push((pred, body.blocks[pred].stmts.len()));
                    }
                }
            }
            if ok {
                sites.push((bi, si, place.local, free));
            }
        }
    }
    if sites.is_empty() {
        return;
    }

    let unit_ty = body.locals[0].ty;
    sites.sort_by_key(|&(bi, si, _, _)| (bi, si));
    for (bi, si, local, free) in sites.into_iter().rev() {
        let dest = Local(u32::try_from(body.locals.len()).expect("local overflow"));
        body.locals.push(crate::ir::LocalDecl {
            ty: unit_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
        let span = body.blocks[bi].stmts[si].span;
        body.blocks[bi].stmts.insert(
            si,
            Statement {
                kind: StatementKind::Assign {
                    place: Place::local(dest),
                    rvalue: Rvalue::CallIntrinsic {
                        name: free,
                        args: vec![Operand::Copy(Place::local(local))],
                    },
                },
                span,
                inlined: None,
            },
        );
    }
}

/// What a run of statements does with one local, read in order.
enum LocalScan {
    /// The local is read (or written through) before any redefinition.
    Mentioned,
    /// The local is redefined whole before any read.
    Redefined,
    /// The local is neither read nor redefined.
    Clear,
}

fn scan_for_local(stmts: &[Statement], terminator: Option<&Terminator>, local: Local) -> LocalScan {
    for stmt in stmts {
        if let StatementKind::Assign { place, rvalue } = &stmt.kind
            && place.projection.is_empty()
            && place.local == local
        {
            return if crate::opt::rvalue_mentions_local(rvalue, local) {
                LocalScan::Mentioned
            } else {
                LocalScan::Redefined
            };
        }
        if crate::opt::stmt_mentions_local(stmt, local) {
            return LocalScan::Mentioned;
        }
    }
    match terminator {
        None => LocalScan::Clear,
        Some(Terminator::Call {
            callee,
            args,
            destination,
            ..
        }) if destination.projection.is_empty() && destination.local == local => {
            let reads = std::iter::once(callee).chain(args).any(
                |op| matches!(op, Operand::Copy(p) if crate::opt::place_mentions_local(p, local)),
            );
            if reads {
                LocalScan::Mentioned
            } else {
                LocalScan::Redefined
            }
        }
        Some(Terminator::Drop { place, .. }) if crate::opt::place_mentions_local(place, local) => {
            LocalScan::Mentioned
        }
        Some(t) if crate::opt::term_mentions_local(t, local) => LocalScan::Mentioned,
        Some(_) => LocalScan::Clear,
    }
}

/// Whether the copy at statement `si` of block `bi` is the last mention of
/// `local` on every path, up to wherever `local` is next defined.
///
/// A move hands the allocation to the copy's destination, which is sound only
/// when nothing reads `local` afterwards and the copy cannot run again for the
/// same allocation: a path that returns to the copy without redefining `local`
/// would hand one allocation over on every turn of the loop.
fn copy_is_last_use(body: &Body, (bi, si): (usize, usize), local: Local) -> bool {
    let block = &body.blocks[bi];
    let mut work = match scan_for_local(&block.stmts[si + 1..], Some(&block.terminator), local) {
        LocalScan::Mentioned => return false,
        LocalScan::Redefined => return true,
        LocalScan::Clear => successors_of(&block.terminator),
    };
    let mut seen = vec![false; body.blocks.len()];
    while let Some(b) = work.pop() {
        if b >= body.blocks.len() || std::mem::replace(&mut seen[b], true) {
            continue;
        }
        if b == bi {
            // Back at the copy's own block: only the statements ahead of the
            // copy stand between this path and the copy running again.
            match scan_for_local(&body.blocks[bi].stmts[..si], None, local) {
                LocalScan::Redefined => continue,
                LocalScan::Mentioned | LocalScan::Clear => return false,
            }
        }
        let next = &body.blocks[b];
        match scan_for_local(&next.stmts, Some(&next.terminator), local) {
            LocalScan::Mentioned => return false,
            LocalScan::Redefined => {}
            LocalScan::Clear => work.extend(successors_of(&next.terminator)),
        }
    }
    true
}

/// The statement that defines `local` as a bare copy of another local.
fn copy_definition_site(body: &Body, local: usize) -> Option<(usize, usize)> {
    body.blocks.iter().enumerate().find_map(|(bi, block)| {
        block
            .stmts
            .iter()
            .position(|stmt| {
                matches!(
                    &stmt.kind,
                    StatementKind::Assign { place, rvalue: Rvalue::Use(Operand::Copy(_)) }
                        if place.projection.is_empty() && place.local.0 as usize == local
                )
            })
            .map(|si| (bi, si))
    })
}

/// Whether every copy along a move chain is the last use of the local it
/// reads: the copy at `site` of `source`, and each pass-through hop's copy of
/// the local below it, down to `origin`.
fn move_chain_is_last_use(
    body: &Body,
    site: (usize, usize),
    source: usize,
    origin: usize,
    hops: &[usize],
) -> bool {
    if !copy_is_last_use(body, site, Local(source as u32)) {
        return false;
    }
    let chain: Vec<usize> = hops
        .iter()
        .copied()
        .chain(std::iter::once(origin))
        .collect();
    chain.windows(2).all(|pair| {
        copy_definition_site(body, pair[0])
            .is_some_and(|def| copy_is_last_use(body, def, Local(pair[1] as u32)))
    })
}

/// Blocks control can reach from `t`.
fn successors_of(t: &Terminator) -> Vec<usize> {
    match t {
        Terminator::Goto { target } => vec![target.0 as usize],
        Terminator::SwitchInt { arms, default, .. } => {
            let mut v: Vec<usize> = arms.iter().map(|(_, t)| t.0 as usize).collect();
            v.push(default.0 as usize);
            v
        }
        Terminator::Call { target, .. } => target.iter().map(|t| t.0 as usize).collect(),
        Terminator::Assert { target, .. } => vec![target.0 as usize],
        Terminator::Drop { target, .. } => vec![target.0 as usize],
        _ => Vec::new(),
    }
}

/// Clears the region flag on every call result.
///
/// A local created while a region is open is region storage only where the
/// allocation came from the region's bump. A call result does not: a user
/// function allocates under its own frame's rules, and the string helpers
/// promote a copy of region-backed bytes to the heap so a recycled slab cannot
/// land on its own source. Neither is reclaimed by the slab sweep at pop, so
/// the frame has to release it.
///
/// Clearing the flag only lets the ownership rules apply; it never makes a
/// borrowed result owned. Where a result really is region storage, every free
/// path (`gos_rt_rc_release`, `gos_rt_vec_free`, `str_free_impl`) answers an
/// address-range test and returns without touching the memory, so a release
/// the region already reclaimed is a no-op.
pub(crate) fn clear_region_on_call_results(body: &mut Body) {
    let mut results: Vec<Local> = Vec::new();
    for block in &body.blocks {
        if let Terminator::Call { destination, .. } = &block.terminator
            && destination.projection.is_empty()
        {
            results.push(destination.local);
        }
    }
    for local in results {
        if let Some(decl) = body.locals.get_mut(local.0 as usize) {
            decl.region = false;
        }
    }
}

/// Reclaim helpers whose value is a unique, non-reference-counted allocation,
/// so a bare copy that consumes it for the last time can carry the reclaim to
/// its new holder.
///
/// A value outside this set is left on the conservative at-return reclaim: two
/// owners of one un-counted allocation free it twice.
fn transferable_by_move(free: &str) -> bool {
    matches!(
        free,
        "gos_rt_vec_free"
            | "gos_rt_map_free"
            | "gos_rt_lazy_iter_drop_i64"
            | "gos_rt_lazy_iter_drop_pair_i64"
            | "gos_rt_http_response_free"
    )
}

/// Runtime calls that only READ the `http::Response` they are given: the box
/// they are handed does not outlive the call and is not what they answer.
///
/// Reclaiming a response is safe exactly where the frame can see its whole
/// life, and a call outside this set can hand the box on - `with_header`
/// answers the very same pointer - so a response that reaches one is left to
/// whoever ends up holding it.
fn reads_response_only(name: &str) -> bool {
    matches!(
        name,
        "gos_rt_http_response_status"
            | "gos_rt_http_response_body"
            | "gos_rt_http_response_raw_bytes"
            | "gos_rt_http_response_headers"
            | "gos_rt_http_response_get_header"
            | "gos_rt_http_response_content_type"
            | "gos_rt_http_response_location"
            | "gos_rt_http_response_free"
    )
}

pub(crate) fn insert_drops_at_returns(body: &mut Body, tcx: &gossamer_types::TyCtxt) {
    use gossamer_types::TyKind;

    if body.locals.is_empty() {
        return;
    }
    // Balanced share for a Vec element pushed into a vec: the container's
    // element teardown (`gos_rt_vec_free`'s VEC element kind) releases one
    // share per slot, so the push must mint the container's own share
    // here while the frame keeps its per-site/at-return free - correct on
    // every path, including a conditional push that never runs.
    {
        let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
        let mut retains: Vec<(usize, Local)> = Vec::new();
        for (bi, block) in body.blocks.iter().enumerate() {
            if let Terminator::Call {
                callee: Operand::Const(ConstValue::Str(name)),
                args,
                ..
            } = &block.terminator
                && is_element_push(name)
                && let Some(Operand::Copy(p)) = args.get(1)
                && p.projection.is_empty()
                && (p.local.0 as usize) < body.locals.len()
                && matches!(
                    tcx.kind_of(body.locals[p.local.0 as usize].ty),
                    TyKind::Vec(_) | TyKind::Slice(_)
                )
                && !body.locals[p.local.0 as usize].region
            {
                retains.push((bi, p.local));
            }
        }
        for (bi, l) in retains {
            let dest = Local(u32::try_from(body.locals.len()).expect("local overflow"));
            body.locals.push(LocalDecl {
                ty: unit_ty,
                debug_name: None,
                mutable: false,
                region: false,
            });
            let span = body.blocks[bi].span;
            body.blocks[bi].stmts.push(Statement {
                kind: StatementKind::Assign {
                    place: Place::local(dest),
                    rvalue: Rvalue::CallIntrinsic {
                        name: "gos_rt_vec_retain",
                        args: vec![Operand::Copy(Place::local(l))],
                    },
                },
                span,
                inlined: None,
            });
        }
    }
    // Per-local: the constructor symbol that allocated it (if
    // any). `None` means the local was either never assigned, was
    // assigned by something other than a recognised constructor,
    // or has been disqualified by a subsequent re-assignment.
    let mut owner_ctor: Vec<Option<&'static str>> = vec![None; body.locals.len()];
    let mut moved_into_return: Vec<bool> = vec![false; body.locals.len()];

    // Drop-before-overwrite sites for aggregate-typed locals. Each
    // entry `(block_idx, stmt_idx, local, size_bytes)` means
    // "insert `gos_rt_aggr_free(local, size)` before block
    // `block_idx`'s statement at index `stmt_idx`". The null check
    // inside `gos_rt_aggr_free` makes this a no-op on the first
    // assignment (the local holds 0/null pre-init) and reclaims
    // the previous allocation on every subsequent assignment
    // - closing the loop-body aggregate-leak case.
    let mut drop_before_sites: Vec<(usize, usize, Local, i64)> = Vec::new();

    // Only `zip` and `enumerate` build the two-word pair state. Every other
    // runtime entry point answers a word handle whatever its element type, and
    // a Gossamer function's answer is known only by its signature.
    let iterator_free = |ty: Ty, callee: &Operand| -> Option<&'static str> {
        let TyKind::Iterator(item) = tcx.kind_of(ty) else {
            return None;
        };
        let pair_state = match callee {
            Operand::Const(ConstValue::Str(name)) if name.starts_with("gos_rt_") => matches!(
                name.as_str(),
                "gos_rt_lazy_iter_enumerate_i64" | "gos_rt_lazy_iter_zip_i64"
            ),
            _ => matches!(tcx.kind_of(*item), TyKind::Tuple(items) if items.len() == 2),
        };
        if pair_state {
            Some("gos_rt_lazy_iter_drop_pair_i64")
        } else {
            Some("gos_rt_lazy_iter_drop_i64")
        }
    };

    let ctor_to_free = |name: &str| -> Option<&'static str> {
        match name {
            // Runtime-symbol form (used by some peephole sites).
            "gos_rt_map_new" | "gos_rt_map_new_with_capacity" => Some("gos_rt_map_free"),
            "gos_rt_vec_new"
            | "gos_rt_vec_with_capacity"
            | "gos_rt_vec_repeat_primitive"
            | "gos_rt_bheap_max_new_i64"
            | "gos_rt_bheap_max_from_vec_i64"
            | "gos_rt_bheap_min_new_i64"
            | "gos_rt_bheap_min_from_vec_i64"
            | "gos_rt_bheap_new_typed"
            | "gos_rt_bheap_max_from_vec_desc"
            | "gos_rt_bheap_min_from_vec_desc" => Some("gos_rt_vec_free"),
            // Always returns a freshly allocated vec the frame owns,
            // whatever the destination's inferred type (a cloned borrowed
            // row lands in a Slice-typed local the type-based inference
            // below does not cover).
            "gos_rt_vec_clone" => Some("gos_rt_vec_free"),
            // A binding taken from a container copies its storage, so the
            // copy is the frame's to reclaim exactly as a constructed one is.
            // A queue and a stack share the deque header, so they share its
            // reclamation too.
            "gos_rt_set_clone" => Some("gos_rt_set_free"),
            "gos_rt_map_clone" => Some("gos_rt_map_free"),
            "gos_rt_deque_clone" | "gos_rt_queue_clone" | "gos_rt_stack_clone" => {
                Some("gos_rt_deque_free")
            }
            "gos_rt_set_new"
            | "gos_rt_btree_set_new"
            | "gos_rt_set_union"
            | "gos_rt_set_intersection"
            | "gos_rt_set_intersection_skey"
            | "gos_rt_set_difference"
            | "gos_rt_set_symmetric_difference" => Some("gos_rt_set_free"),
            // A `http::Response` is a box the runtime owns; the server's
            // reclaim after writing a handler's answer is the same call, so a
            // response that never leaves the frame that built it is reclaimed
            // exactly once here instead of outliving the process.
            "gos_rt_http_response_text_new"
            | "gos_rt_http_response_json_new"
            | "gos_rt_http_response_stream_new" => Some("gos_rt_http_response_free"),
            // Iterator over a Vec - the destination local is typed as
            // the source Vec so the `.next()` dispatch can recover the
            // element type. Without this entry the type-based
            // `inferred_free` path would schedule `gos_rt_vec_free` on
            // a `*mut GosArrIter`, mis-interpreting its bytes as a
            // `GosVec` header and corrupting the heap on free.
            "gos_rt_arr_iter" => Some("gos_rt_arr_iter_free"),
            // Path-form constructors emitted by the call lowerer.
            // The cranelift backend's `lower_intrinsic_call` table
            // routes these straight to the runtime helper, so the
            // drop pass needs to recognise both forms.
            "Map::new"
            | "collections::Map::new"
            | "HashMap::new"
            | "collections::HashMap::new"
            | "Map::with_capacity"
            | "collections::Map::with_capacity"
            | "HashMap::with_capacity"
            | "collections::HashMap::with_capacity"
            | "BTreeMap::new"
            | "collections::BTreeMap::new" => Some("gos_rt_map_free"),
            "Vec::new" | "Vec::with_capacity" => Some("gos_rt_vec_free"),
            "Set::new"
            | "collections::Set::new"
            | "HashSet::new"
            | "collections::HashSet::new"
            | "BTreeSet::new"
            | "collections::BTreeSet::new" => Some("gos_rt_set_free"),
            "gos_rt_deque_new"
            | "gos_rt_deque_new_typed"
            | "gos_rt_deque_from_vec"
            | "Deque::new"
            | "collections::Deque::new"
            | "VecDeque::new"
            | "collections::VecDeque::new"
            | "gos_rt_deque_from_vec_i64"
            | "gos_rt_queue_new"
            | "Queue::new"
            | "collections::Queue::new"
            | "VecQueue::new"
            | "collections::VecQueue::new"
            | "gos_rt_queue_from_vec_i64"
            | "gos_rt_stack_new"
            | "Stack::new"
            | "collections::Stack::new"
            | "VecStack::new"
            | "collections::VecStack::new"
            | "gos_rt_stack_from_vec_i64" => Some("gos_rt_deque_free"),
            _ => None,
        }
    };

    let arity = body.arity as usize;
    let last_block = body.blocks.len();

    // A carrier a channel receive answered is this frame's own, and so is a
    // copy of it: every send hands the channel a table of its own, so the
    // binding that takes the payload out owns that map. A Gossamer call's
    // carrier is not: its payload may be a map borrowed out of another.
    let n_all = body.locals.len();
    let mut owned_carrier = vec![false; n_all];
    for block in &body.blocks {
        if let Terminator::Call {
            callee,
            destination,
            ..
        } = &block.terminator
            && destination.projection.is_empty()
            && (destination.local.0 as usize) < n_all
            && let Operand::Const(ConstValue::Str(name)) = callee
            && (name.starts_with("gos_rt_chan_recv") || name.starts_with("gos_rt_chan_try_recv"))
        {
            owned_carrier[destination.local.0 as usize] = true;
        }
    }
    loop {
        let mut changed = false;
        for stmt in body.blocks.iter().flat_map(|b| &b.stmts) {
            if let StatementKind::Assign {
                place,
                rvalue: Rvalue::Use(Operand::Copy(src)),
            } = &stmt.kind
                && place.projection.is_empty()
                && src.projection.is_empty()
                && (place.local.0 as usize) < n_all
                && (src.local.0 as usize) < n_all
                && owned_carrier[src.local.0 as usize]
                && !owned_carrier[place.local.0 as usize]
            {
                owned_carrier[place.local.0 as usize] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let takes_owned_map = |rvalue: &Rvalue, dest: usize| -> bool {
        matches!(tcx.kind_of(body.locals[dest].ty), TyKind::HashMap { .. })
            && matches!(
                rvalue,
                Rvalue::CallIntrinsic { name: "gos_rt_result_payload", args }
                    if matches!(
                        args.first(),
                        Some(Operand::Copy(c))
                            if c.projection.is_empty()
                                && (c.local.0 as usize) < n_all
                                && owned_carrier[c.local.0 as usize]
                    )
            )
    };

    // Pass 1: discover constructor-allocated locals. Track every
    // assignment that *might* invalidate ownership (re-assignment,
    // projection writes) so we can disqualify aliasing patterns. Track every
    // assignment that *might* invalidate ownership (re-assignment,
    // projection writes) so we can disqualify aliasing patterns.
    //
    // Also disqualifies any local passed as a Copy arg to a Call
    // whose callee may capture its arguments (any user FnRef, or a
    // named runtime helper outside the non-capturing whitelist).
    // Without this disqualification, the drop pass would free a
    // container whose pointer is now retained inside the callee
    // (e.g. `flag::parse(os::args())` slurps the args vec; freeing
    // the args vec after the call orphans the parsed `rest`
    // strings).
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign { place, rvalue } = &stmt.kind {
                let idx = place.local.0 as usize;
                if !place.projection.is_empty() {
                    // Writing through a projection on this local
                    // doesn't move ownership, so it stays valid.
                    continue;
                }
                if idx == 0 || idx <= arity || idx >= owner_ctor.len() {
                    continue;
                }
                // note: `Rvalue::Aggregate` /
                // `Rvalue::Repeat` are NOT tracked here. The LLVM
                // backend (used by `gos build`) lowers aggregates
                // to stack slots that die with the function frame
                // - no leak. The Cranelift backend (used by the
                // in-process JIT for `gos`) routes them through
                // `gos_rt_aggr_alloc`, which lives in the
                // process-wide registry; long-running JIT bodies
                // can call `gos_rt_gc_reset` at safepoints to
                // reclaim. Emitting `gos_rt_aggr_free` here would
                // double-free the stack slot under LLVM, which is
                // the default backend.
                // Re-assignment of an owning local - disqualify.
                if owner_ctor[idx].is_some() && !matches!(rvalue, Rvalue::CallIntrinsic { .. }) {
                    owner_ctor[idx] = None;
                }
                if owner_ctor[idx].is_none() && takes_owned_map(rvalue, idx) {
                    owner_ctor[idx] = Some("gos_rt_map_free");
                }
            }
        }
        if let Terminator::Call {
            callee,
            destination,
            args,
            ..
        } = &block.terminator
        {
            let idx = destination.local.0 as usize;
            if idx == 0 || idx <= arity || idx >= owner_ctor.len() {
                continue;
            }
            if !destination.projection.is_empty() {
                continue;
            }
            // Any local of a heap-container type that's the
            // destination of a Call also owns the result - the
            // callee returned a freshly-allocated container that
            // this frame must drop unless it's then moved into
            // the return slot. Match by static type, since the
            // callee name ("count_kmers", arbitrary user fn)
            // doesn't telegraph ownership.
            //
            // A handful of runtime callees return *borrowed*
            // pointers - `gos_rt_os_args` hands back the global
            // `ARGS_VEC` sentinel that lives for the whole
            // process; passing it to `gos_rt_vec_free` aborts in
            // `__libc_free` on the next-pointer probe. Skip the
            // inferred_free assignment for those.
            let dest_ty = body.locals[idx].ty;
            // A map unwrapped out of a carrier is the payload that carrier
            // holds: the frame owns it only when it owns the carrier, and a
            // map answered by `get` on a map of maps is the outer map's.
            let unwraps_lent_map = matches!(tcx.kind_of(dest_ty), TyKind::HashMap { .. })
                && matches!(
                    callee,
                    Operand::Const(ConstValue::Str(s))
                        if matches!(
                            s.as_str(),
                            "gos_rt_option_unwrap" | "gos_rt_result_unwrap" | "gos_rt_option_expect"
                                | "gos_rt_result_expect"
                        )
                )
                && !matches!(
                    args.first(),
                    Some(Operand::Copy(c))
                        if c.projection.is_empty()
                            && (c.local.0 as usize) < n_all
                            && owned_carrier[c.local.0 as usize]
                );
            let borrowed_callee = unwraps_lent_map
                || matches!(
                    callee,
                    Operand::Const(ConstValue::Str(s))
                        if returns_borrowed_pointer(s.as_str())
                );
            let inferred_free: Option<&'static str> = if borrowed_callee {
                None
            } else {
                // A Gossamer function answering `[T]` - a slice parameter it
                // returns, or the runtime-length carrier of a const generic
                // `[T; N]` - hands the caller a share of its own.
                let gossamer_callee = match callee {
                    Operand::FnRef { .. } => true,
                    Operand::Const(ConstValue::Str(s)) => {
                        !s.starts_with("gos_rt_") && !s.starts_with("__")
                    }
                    _ => false,
                };
                match tcx.kind_of(dest_ty) {
                    TyKind::HashMap { .. } => Some("gos_rt_map_free"),
                    TyKind::Vec(_) => Some("gos_rt_vec_free"),
                    TyKind::Slice(_) if gossamer_callee => Some("gos_rt_vec_free"),
                    _ => iterator_free(dest_ty, callee),
                }
            };
            if let Operand::Const(ConstValue::Str(name)) = callee {
                if let Some(free) = ctor_to_free(name.as_str()) {
                    if owner_ctor[idx].is_none() {
                        owner_ctor[idx] = Some(free);
                        continue;
                    }
                }
            }
            if let Some(free) = inferred_free {
                if owner_ctor[idx].is_none() {
                    owner_ctor[idx] = Some(free);
                    continue;
                }
            }
            // when a Call returns an aggregate
            // (Adt / Tuple / Array) into a local, queue a
            // drop-before-overwrite of the prior value at the end
            // of this block (just before the Call terminator
            // runs). On the first execution the local holds 0/null
            // and `gos_rt_aggr_free` no-ops via its null check; on
            // every subsequent execution (loop reuse, repeated
            // call) the prior allocation is reclaimed instead of
            // leaked. The end-of-scope drop continues to handle
            // the final allocation at function return.
            let dest_is_aggregate = matches!(
                tcx.kind_of(dest_ty),
                TyKind::Adt { .. } | TyKind::Tuple(_) | TyKind::Array { .. }
            );
            // note: Call destinations of aggregate
            // type are not tracked here. See the matching comment in
            // the stmt-loop above - LLVM uses stack slots, Cranelift
            // JIT uses tracked heap allocs reclaimable via
            // `gos_rt_gc_reset` at safepoints.
            let _ = dest_is_aggregate;
            // Any other Call destination invalidates ownership
            // (the local now holds something else).
            owner_ctor[idx] = None;
        }
    }

    // A response the frame hands to anything but a reader may be the value
    // that call answers, so the frame can no longer see where the box ends up
    // and leaves the reclaim to whoever does.
    for block in &body.blocks {
        let Terminator::Call { callee, args, .. } = &block.terminator else {
            continue;
        };
        let reader = matches!(
            callee,
            Operand::Const(ConstValue::Str(name)) if reads_response_only(name.as_str())
        );
        if reader {
            continue;
        }
        for arg in args {
            let Operand::Copy(place) = arg else {
                continue;
            };
            let idx = place.local.0 as usize;
            if place.projection.is_empty()
                && idx < owner_ctor.len()
                && owner_ctor[idx] == Some("gos_rt_http_response_free")
            {
                owner_ctor[idx] = None;
            }
        }
    }

    // Heap slots that own what is written into them: an `gos_rc_alloc`
    // result carries a child descriptor, so its release reclaims the
    // children and a store into it needs no aliasing suppression.
    let owning_rc_slots: std::collections::HashSet<Local> = {
        let mut slots = std::collections::HashSet::new();
        for block in &body.blocks {
            for stmt in &block.stmts {
                if let StatementKind::Assign {
                    place,
                    rvalue: Rvalue::CallIntrinsic { name, .. },
                } = &stmt.kind
                    && matches!(*name, "gos_rc_alloc" | "gos_rc_alloc_tagged")
                    && place.projection.is_empty()
                {
                    slots.insert(place.local);
                }
            }
        }
        slots
    };

    // Aliasing summary: a local that is the source of a bare `Copy`, or
    // the value element (arg1..) of a consuming container/channel/closure
    // call, may outlive this frame, so the per-iteration reuse free must
    // not reclaim it. Computed once here and shared by the move-transfer
    // below and the reuse filter further down.
    let mut aliased = {
        let mut aliased = vec![false; body.locals.len()];
        // A write into an aggregate's own field is balanced the way a store
        // into an owning heap slot is: the by-value-aggregate pass mints the
        // field's share at the write and gives it back at the field's death,
        // so the source local stays reclaimable by its own frame. Without
        // this, a container written into a field reaches only the at-return
        // drop, so a frame that fills such a field in a loop keeps every
        // buffer but the last.
        let writes_owned_field = |place: &Place| -> bool {
            let Some(&crate::ir::Projection::Field(idx)) = place.projection.first() else {
                return false;
            };
            place.projection.len() == 1
                && body.locals.get(place.local.0 as usize).is_some_and(|decl| {
                    aggregate_rc_field_paths(tcx, decl.ty)
                        .iter()
                        .any(|(path, kind)| {
                            (matches!(kind, FieldRcKind::Vec) || kind.is_value_container())
                                && path.as_slice() == [idx]
                        })
                })
        };
        for block in &body.blocks {
            for stmt in &block.stmts {
                if let StatementKind::Assign {
                    place: dest,
                    rvalue: Rvalue::Use(Operand::Copy(p)),
                } = &stmt.kind
                    && p.projection.is_empty()
                    && (p.local.0 as usize) < aliased.len()
                    && !writes_owned_field(dest)
                {
                    aliased[p.local.0 as usize] = true;
                }
                // `gos_store(slot, offset, value)` writes `value` into a heap
                // slot the frame cannot see through, so the per-iteration
                // reuse free must not reclaim what it points at - unless the
                // slot belongs to an object that owns its children. An RC
                // allocation carries a descriptor whose release walks those
                // slots, so its store is balanced and the source local goes
                // back to being reclaimed by its own frame.
                if let StatementKind::Assign {
                    rvalue: Rvalue::CallIntrinsic { name, args },
                    ..
                } = &stmt.kind
                    && *name == "gos_store"
                    && let Some(Operand::Copy(p)) = args.get(2)
                    && p.projection.is_empty()
                    && (p.local.0 as usize) < aliased.len()
                    && !args
                        .first()
                        .and_then(|slot| match slot {
                            Operand::Copy(place) if place.projection.is_empty() => {
                                Some(place.local)
                            }
                            _ => None,
                        })
                        .is_some_and(|slot| owning_rc_slots.contains(&slot))
                {
                    aliased[p.local.0 as usize] = true;
                }
            }
            if let Terminator::Call {
                callee: Operand::Const(ConstValue::Str(name)),
                args,
                ..
            } = &block.terminator
                && is_consuming_call(name)
            {
                for arg in args.iter().skip(1) {
                    if let Operand::Copy(p) = arg
                        && p.projection.is_empty()
                        && (p.local.0 as usize) < aliased.len()
                    {
                        // A `Vec` stored into a container is BALANCED (a
                        // retain minted by the container hands it its own
                        // share, freed by the container's element or value
                        // teardown), so the frame's per-site reuse of the
                        // stored local stays sound and load-bearing.
                        if stores_owned_vec_value(name)
                            && matches!(
                                tcx.kind_of(body.locals[p.local.0 as usize].ty),
                                TyKind::Vec(_) | TyKind::Slice(_)
                            )
                        {
                            continue;
                        }
                        aliased[p.local.0 as usize] = true;
                    }
                }
            }
        }
        aliased
    };

    // Move-transfer: a bare `dst = Copy(src)` that consumes a
    // constructor-owned container (`Vec` / `HashMap`) for the last time
    // hands its allocation to `dst`. Pass 1's reassignment rule
    // disqualified `dst` (it is written by a plain copy, not a
    // constructor) and marked `src` aliased, dropping both onto the
    // conservative return-only free - so `let mut v = ...; while ... { v =
    // make() }` leaks every prior buffer. Transferring `src`'s free to
    // `dst` (and clearing `src`) lets the null-safe per-site reuse
    // machinery below free `dst`'s previous value before each overwrite
    // and its final value at return; `src` is never freed (its allocation
    // now lives in `dst`).
    //
    // The transfer fires only when `src` is a live `Vec`/`Map` owner
    // consumed exactly once (this copy, so it is dead afterwards -
    // counting every operand appearance keeps that conservative) and
    // `dst` is not itself aliased into a surviving holder (which would let
    // the per-iteration free dangle the alias). `dst` then lands in
    // `reuse`, and each transferred copy is recorded as a stmt-position
    // drop-before-overwrite site.
    fn bump_place_read(reads: &mut [u32], p: &Place) {
        let i = p.local.0 as usize;
        if i < reads.len() {
            reads[i] = reads[i].saturating_add(1);
        }
    }
    fn bump_op_read(reads: &mut [u32], op: &Operand) {
        if let Operand::Copy(p) = op {
            bump_place_read(reads, p);
        }
    }
    // A freshly-owned container handed back by a call: a `Vec<T>` /
    // `[T]` (`Slice`) / `HashMap` Call-destination whose callee is not a
    // borrowed-pointer returner. These are the same heap allocation at
    // runtime (`rc_helper` routes `Vec`/`Slice` to `gos_rt_vec_free`), so
    // when one is CONSUMED EXACTLY ONCE by a bare copy the move-transfer
    // may hand its ownership to the copy target. Unlike `inferred_free`
    // this is NOT folded into `owner_ctor` globally: a `Slice` result read
    // more than once stays a non-owner (the pre-existing conservative
    // leak), because the move-based drop pass cannot safely give an
    // aliased, non-refcounted container two owners (double-free).
    let fresh_container_free: Vec<Option<&'static str>> = {
        let mut fresh = vec![None; body.locals.len()];
        for block in &body.blocks {
            if let Terminator::Call {
                callee,
                destination,
                ..
            } = &block.terminator
                && destination.projection.is_empty()
                && (destination.local.0 as usize) < fresh.len()
            {
                let borrowed = matches!(
                    callee,
                    Operand::Const(ConstValue::Str(s)) if returns_borrowed_pointer(s.as_str())
                );
                if !borrowed {
                    // A constructor the reclaim table names answers its own
                    // free whatever the destination's type says: an opaque
                    // runtime handle carries no container type to read it from.
                    let named = match callee {
                        Operand::Const(ConstValue::Str(s)) => ctor_to_free(s.as_str()),
                        _ => None,
                    };
                    fresh[destination.local.0 as usize] = named.or_else(|| {
                        match tcx.kind_of(body.locals[destination.local.0 as usize].ty) {
                            TyKind::HashMap { .. } => Some("gos_rt_map_free"),
                            TyKind::Vec(_) | TyKind::Slice(_) => Some("gos_rt_vec_free"),
                            _ => {
                                iterator_free(body.locals[destination.local.0 as usize].ty, callee)
                            }
                        }
                    });
                }
            }
        }
        fresh
    };

    let mut move_copy_sites: Vec<(usize, usize, Local)> = Vec::new();
    // `(block, dst, src, origin)` for each move: the origin is emptied right
    // after the copy, so it keeps a reclaim for every path that skips the move.
    let mut moved_sources: Vec<(usize, Local, Local, Local)> = Vec::new();
    {
        let mut consume_reads = vec![0u32; body.locals.len()];
        for block in &body.blocks {
            for stmt in &block.stmts {
                if let StatementKind::Assign { rvalue, .. } = &stmt.kind {
                    match rvalue {
                        Rvalue::Use(op)
                        | Rvalue::UnaryOp { operand: op, .. }
                        | Rvalue::Cast { operand: op, .. }
                        | Rvalue::Repeat { value: op, .. } => {
                            bump_op_read(&mut consume_reads, op);
                        }
                        Rvalue::BinaryOp { lhs, rhs, .. } => {
                            bump_op_read(&mut consume_reads, lhs);
                            bump_op_read(&mut consume_reads, rhs);
                        }
                        Rvalue::Aggregate { operands, .. } => {
                            for op in operands {
                                bump_op_read(&mut consume_reads, op);
                            }
                        }
                        // Tagging lazy state records how its elements are
                        // owned on the state itself; it takes no share of the
                        // handle and keeps no pointer to it, so it does not
                        // stand between the state and the local a later move
                        // hands it to.
                        Rvalue::CallIntrinsic {
                            name: "gos_rt_lazy_iter_set_elem_meta",
                            ..
                        } => {}
                        Rvalue::CallIntrinsic { args, .. } => {
                            for op in args {
                                bump_op_read(&mut consume_reads, op);
                            }
                        }
                        Rvalue::Len(p) | Rvalue::Ref { place: p, .. } => {
                            bump_place_read(&mut consume_reads, p);
                        }
                        Rvalue::StaticLoad(_) => {}
                    }
                }
            }
            match &block.terminator {
                Terminator::SwitchInt { discriminant, .. } => {
                    bump_op_read(&mut consume_reads, discriminant);
                }
                Terminator::Call { callee, args, .. } => {
                    bump_op_read(&mut consume_reads, callee);
                    // An in-place append writes through the container it is
                    // handed; it takes no share of it and keeps no pointer to
                    // it, so it does not stand between the container and the
                    // local a later move hands it to. Counting it would leave
                    // `let v = #[a, b]` inside a loop non-transferable, and
                    // the outer binding it is moved into would leak every
                    // prior buffer.
                    let in_place_container = matches!(
                        callee,
                        Operand::Const(ConstValue::Str(name))
                            if appends_through_container(name.as_str())
                                || borrows_vec_receiver(name.as_str())
                    );
                    for (idx, op) in args.iter().enumerate() {
                        if in_place_container && idx == 0 {
                            continue;
                        }
                        bump_op_read(&mut consume_reads, op);
                    }
                }
                Terminator::Assert { cond, msg, .. } => {
                    bump_op_read(&mut consume_reads, cond);
                    for op in msg.operands() {
                        bump_op_read(&mut consume_reads, op);
                    }
                }
                Terminator::Drop { place, .. } => bump_place_read(&mut consume_reads, place),
                _ => {}
            }
        }
        // The sole whole-local definition of each local, when it is a bare
        // copy of another local. `let t = make(); x = t` names one allocation
        // through `t`, and the transfer has to see past that hop: otherwise
        // the first copy is refused because `t` is copied onward and the
        // second because `t` never became an owner, so neither end frees and
        // every prior buffer is lost.
        let sole_copy_source: Vec<Option<usize>> = {
            let mut source: Vec<Option<usize>> = vec![None; body.locals.len()];
            let mut definitions = vec![0u32; body.locals.len()];
            for block in &body.blocks {
                for stmt in &block.stmts {
                    if let StatementKind::Assign { place, rvalue } = &stmt.kind
                        && place.projection.is_empty()
                        && (place.local.0 as usize) < source.len()
                        // A null store holds no allocation: it is the placeholder
                        // that keeps a release on an untaken path a no-op.
                        && !matches!(rvalue, Rvalue::Use(Operand::Const(ConstValue::Int(0))))
                    {
                        let i = place.local.0 as usize;
                        definitions[i] = definitions[i].saturating_add(1);
                        if let Rvalue::Use(Operand::Copy(from)) = rvalue
                            && from.projection.is_empty()
                        {
                            source[i] = Some(from.local.0 as usize);
                        }
                    }
                }
                if let Terminator::Call { destination, .. } = &block.terminator
                    && destination.projection.is_empty()
                    && (destination.local.0 as usize) < source.len()
                {
                    let i = destination.local.0 as usize;
                    definitions[i] = definitions[i].saturating_add(1);
                }
            }
            for i in 0..source.len() {
                if definitions[i] != 1 {
                    source[i] = None;
                }
            }
            source
        };
        // Walks back through pass-through hops to the local that owns the
        // allocation, answering it with the hops crossed. A hop qualifies
        // only when its one definition is the copy that brought the
        // allocation in and its one read is the copy that hands it on, so it
        // holds that allocation and nothing else, and clearing its ownership
        // record can strand nothing.
        fn resolve_origin(
            mut current: usize,
            owner_ctor: &[Option<&'static str>],
            fresh_container_free: &[Option<&'static str>],
            sole_copy_source: &[Option<usize>],
            consume_reads: &[u32],
            arity: usize,
        ) -> Option<(usize, Vec<usize>)> {
            let mut hops = Vec::new();
            for _ in 0..8 {
                if owner_ctor[current]
                    .or(fresh_container_free[current])
                    .is_some()
                {
                    return Some((current, hops));
                }
                let previous = sole_copy_source[current]?;
                if previous <= arity || previous >= owner_ctor.len() || consume_reads[current] != 1
                {
                    return None;
                }
                hops.push(current);
                current = previous;
            }
            None
        }
        // A move-transfer target must ALWAYS hold a value it owns, so its
        // drop-before-overwrite never frees a pointer another local owns.
        // `dst` qualifies only when every whole-local assignment to it
        // establishes ownership: a fresh container call-result, or a bare
        // copy of a fresh container consumed exactly once (itself
        // move-transferable). A plain alias-copy (`cur = h` where `h` is
        // read elsewhere too) disqualifies `dst` - freeing `cur`'s aliased
        // initial value would double-free `h`'s owner.
        let owning_copy = |src: &Place, site: (usize, usize)| -> bool {
            if !src.projection.is_empty() {
                return false;
            }
            let s = src.local.0 as usize;
            if s >= owner_ctor.len() {
                return false;
            }
            let Some((origin, hops)) = resolve_origin(
                s,
                &owner_ctor,
                &fresh_container_free,
                &sole_copy_source,
                &consume_reads,
                arity,
            ) else {
                return false;
            };
            owner_ctor[origin]
                .or(fresh_container_free[origin])
                .is_some_and(transferable_by_move)
                && consume_reads[origin] == 1
                && move_chain_is_last_use(body, site, s, origin, &hops)
        };
        // A rebound owner takes a share of what is copied into it, so a copy
        // into it never moves its source's.
        let mut dst_all_owning: Vec<bool> = rebound_vec_owners(body, tcx)
            .into_iter()
            .map(|r| !r)
            .collect();
        for (bi, block) in body.blocks.iter().enumerate() {
            for (si, stmt) in block.stmts.iter().enumerate() {
                if let StatementKind::Assign { place, rvalue } = &stmt.kind
                    && place.projection.is_empty()
                    && (place.local.0 as usize) < dst_all_owning.len()
                {
                    let owning = matches!(
                        rvalue,
                        Rvalue::Use(Operand::Copy(src)) if owning_copy(src, (bi, si))
                    );
                    if !owning {
                        dst_all_owning[place.local.0 as usize] = false;
                    }
                }
            }
            if let Terminator::Call { destination, .. } = &block.terminator
                && destination.projection.is_empty()
                && (destination.local.0 as usize) < dst_all_owning.len()
                && fresh_container_free[destination.local.0 as usize].is_none()
            {
                dst_all_owning[destination.local.0 as usize] = false;
            }
        }

        for (bi, block) in body.blocks.iter().enumerate() {
            for (si, stmt) in block.stmts.iter().enumerate() {
                let StatementKind::Assign {
                    place,
                    rvalue: Rvalue::Use(Operand::Copy(src)),
                } = &stmt.kind
                else {
                    continue;
                };
                if !place.projection.is_empty() || !src.projection.is_empty() {
                    continue;
                }
                let d = place.local.0 as usize;
                let s = src.local.0 as usize;
                if d == s
                    || d <= arity
                    || s <= arity
                    || d >= owner_ctor.len()
                    || s >= owner_ctor.len()
                {
                    continue;
                }
                if !dst_all_owning[d] {
                    continue;
                }
                // `src` is a live owner either recorded in `owner_ctor`
                // (a constructor / `Vec`-returning call) or a fresh
                // `Vec`/`Slice`/`Map` call-result (`fresh_container_free`,
                // which unlike `owner_ctor` also covers `Slice`), reached
                // through any number of pass-through copies.
                let Some((origin, hops)) = resolve_origin(
                    s,
                    &owner_ctor,
                    &fresh_container_free,
                    &sole_copy_source,
                    &consume_reads,
                    arity,
                ) else {
                    continue;
                };
                let Some(free) = owner_ctor[origin].or(fresh_container_free[origin]) else {
                    continue;
                };
                if !transferable_by_move(free) {
                    continue;
                }
                // `src` must be consumed exactly once (this copy) and `dst`
                // must not be aliased into another holder. `dst` may only
                // already own the same free (a prior constructor of the
                // same kind, disqualified by pass 1's reassignment rule).
                if consume_reads[origin] != 1 || aliased[d] {
                    continue;
                }
                if !move_chain_is_last_use(body, (bi, si), s, origin, &hops) {
                    continue;
                }
                if let Some(existing) = owner_ctor[d]
                    && existing != free
                {
                    continue;
                }
                owner_ctor[d] = Some(free);
                // The origin keeps its own reclaim. The copy is its only read,
                // so it is emptied right after it: the reclaim is a no-op on
                // the path that moved the value, and still frees the value on a
                // path that never reached the copy.
                aliased[origin] = false;
                for hop in hops {
                    owner_ctor[hop] = None;
                }
                move_copy_sites.push((bi, si, place.local));
                moved_sources.push((bi, place.local, src.local, Local(origin as u32)));
            }
        }
    }

    // Iterator values are unique lazy-runtime handles. Passing one by value to
    // another function transfers ownership to that callee, and the runtime
    // lazy helpers either embed it in a returned adapter or consume and drop
    // it. Clear this frame's owner record so return cleanup cannot free it
    // after the callee already consumed it.
    // An advance reads the state in place and leaves it with this frame.
    for block in &body.blocks {
        let Terminator::Call { callee, args, .. } = &block.terminator else {
            continue;
        };
        if let Operand::Const(ConstValue::Str(name)) = callee
            && matches!(
                name.as_str(),
                "gos_rt_lazy_iter_next_i64"
                    | "gos_rt_lazy_iter_next_f64"
                    | "gos_rt_lazy_iter_next_pair_i64"
            )
        {
            continue;
        }
        for arg in args {
            let Operand::Copy(place) = arg else {
                continue;
            };
            let idx = place.local.0 as usize;
            if place.projection.is_empty()
                && idx < owner_ctor.len()
                && matches!(tcx.kind_of(body.locals[idx].ty), TyKind::Iterator(_))
            {
                owner_ctor[idx] = None;
            }
        }
    }

    // A container handle stored into an aggregate's field belongs to that
    // aggregate from then on - it outlives the frame whenever the aggregate
    // does. Handles the field walk does not track (a `Map`, a `Set`, an
    // ordered container: no RC header, no field-death free) would otherwise
    // be freed at return while the field still names them. A `Vec` or an
    // RC field is tracked, retained at its store, and stays out of this.
    for block in &body.blocks {
        for stmt in &block.stmts {
            let StatementKind::Assign {
                place,
                rvalue: Rvalue::Use(Operand::Copy(src)),
            } = &stmt.kind
            else {
                continue;
            };
            if place.projection.is_empty()
                || !place
                    .projection
                    .iter()
                    .all(|p| matches!(p, crate::ir::Projection::Field(_)))
                || !src.projection.is_empty()
            {
                continue;
            }
            let idx = src.local.0 as usize;
            if idx >= owner_ctor.len() {
                continue;
            }
            let ty = body.locals[idx].ty;
            let tracked = matches!(tcx.kind_of(ty), TyKind::Vec(_) | TyKind::Slice(_))
                || tcx.is_rc_managed(ty);
            if !tracked {
                owner_ctor[idx] = None;
            }
        }
    }

    // Pass 2: detect locals that *transitively* flow into the
    // return slot. The constructor result may be copied through a
    // chain of intermediate locals before landing in `Local::RETURN`
    // (e.g. `Local(0) = Local(4); Local(4) = Local(5);
    // Local(5) = HashMap::new()`). Any local in that chain
    // shares the same heap pointer and must not be dropped, since
    // `Local::RETURN` will be moved out to the caller.
    //
    // Build a "Copy edge" graph (`from` → `to` whenever
    // `Assign(to, Use(Copy(from)))` appears with bare projections),
    // then walk it backwards from `Local::RETURN` to its closure.
    let call_dest = call_destinations(body);
    let mut copy_edges_to: Vec<Vec<Local>> = vec![Vec::new(); body.locals.len()];
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign { place, rvalue } = &stmt.kind {
                if !place.projection.is_empty() {
                    continue;
                }
                let to_idx = place.local.0 as usize;
                if to_idx >= copy_edges_to.len() {
                    continue;
                }
                match rvalue {
                    Rvalue::Use(Operand::Copy(p)) if p.projection.is_empty() => {
                        copy_edges_to[to_idx].push(p.local);
                    }
                    // An aggregate moves each `Copy` operand into
                    // the constructed value's storage. If the
                    // aggregate later flows to RETURN, every
                    // moved-in source local must skip its drop -
                    // its allocation is now owned by the caller via
                    // the returned aggregate. Without this edge,
                    // a `let v = Vec::new(); push(v, ...); Foo {
                    // ids: v }` body emits a `gos_rt_vec_free(v)`
                    // before Return, freeing storage that the
                    // returned struct's `ids` field still aliases -
                    // the caller's `f.ids.len()` then reads garbage.
                    // A slot that takes a container of its own leaves the
                    // source holding the one it built, so that source is not
                    // part of the returned value.
                    Rvalue::Aggregate { operands, .. } => {
                        for (idx, op) in operands.iter().enumerate() {
                            if let Operand::Copy(p) = op
                                && p.projection.is_empty()
                                && !aggregate_slot_takes_own_container(
                                    tcx, body, place, idx, p, &call_dest,
                                )
                            {
                                copy_edges_to[to_idx].push(p.local);
                            }
                        }
                    }
                    // `Ok(v)` / `Some(v)` puts the payload's word in the
                    // carrier, so a carrier that reaches the return slot takes
                    // the payload with it and the frame's own reclaim would
                    // free storage the caller is about to read.
                    Rvalue::CallIntrinsic { name, args } if *name == "gos_rt_result_new" => {
                        if let Some(Operand::Copy(p)) = args.get(1)
                            && p.projection.is_empty()
                        {
                            copy_edges_to[to_idx].push(p.local);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    let mut stack = vec![Local::RETURN];
    moved_into_return[Local::RETURN.0 as usize] = true;
    while let Some(cur) = stack.pop() {
        let cur_idx = cur.0 as usize;
        if cur_idx >= copy_edges_to.len() {
            continue;
        }
        for src in copy_edges_to[cur_idx].clone() {
            let src_idx = src.0 as usize;
            if src_idx >= moved_into_return.len() {
                continue;
            }
            if !moved_into_return[src_idx] {
                moved_into_return[src_idx] = true;
                stack.push(src);
            }
        }
    }
    // Enum-box locals (`gos_rc_alloc` / `gos_rc_alloc_tagged` results).
    // A Vec stored into one is BALANCED at the constructor - the store
    // retains the box's share and the box's kind-tagged meta entry frees
    // it on teardown - so the frame's own free stays load-bearing and the
    // `gos_store` moved-into-return rule below must not suppress it.
    let enum_box_locals: Vec<bool> = {
        let mut boxes = vec![false; body.locals.len()];
        for block in &body.blocks {
            for stmt in &block.stmts {
                if let StatementKind::Assign {
                    place,
                    rvalue: Rvalue::CallIntrinsic { name, .. },
                } = &stmt.kind
                    && matches!(*name, "gos_rc_alloc" | "gos_rc_alloc_tagged")
                    && place.projection.is_empty()
                    && (place.local.0 as usize) < boxes.len()
                {
                    boxes[place.local.0 as usize] = true;
                }
            }
        }
        boxes
    };
    let is_container_local = |op: &Operand| -> bool {
        matches!(op, Operand::Copy(p) if p.projection.is_empty()
        && (p.local.0 as usize) < body.locals.len()
        && matches!(
            tcx.kind_of(body.locals[p.local.0 as usize].ty),
            gossamer_types::TyKind::Vec(_) | gossamer_types::TyKind::Slice(_)
        ))
    };

    // Calls whose destination flows into `Local::RETURN` move every
    // pointer-shaped Copy argument into the return value too. Tuple
    // construction in particular lowers as a synthesised
    // `__tuple(...)` Call - the Vec/aggregate operands are moved
    // into the constructed value, so they must skip their drop.
    // Iterate to a fixed point because a moved-in Call destination
    // can propagate the same closure backwards through more Copy
    // edges (the dest of an inner construct may feed an outer one).
    let mut changed = true;
    while changed {
        changed = false;
        // Helper: propagate "moved into return" through one Call's
        // arg list when its destination already flows there.
        // Used for both Terminator::Call and Rvalue::CallIntrinsic
        // (the result-ctor / aggregate-helper paths route through
        // the Rvalue form), so the same chain - Vec → struct
        // operand → gos_rt_result_new → Local::RETURN - is walked
        // back to the Vec and skips its drop.
        let propagate_call_args = |args: &[Operand], moved: &mut Vec<bool>, changed: &mut bool| {
            for arg in args {
                if let Operand::Copy(p) = arg
                    && p.projection.is_empty()
                {
                    let idx = p.local.0 as usize;
                    if idx < moved.len() && !moved[idx] {
                        moved[idx] = true;
                        *changed = true;
                        let mut stack = vec![Local(u32::try_from(idx).unwrap_or(0))];
                        while let Some(cur) = stack.pop() {
                            let cur_idx = cur.0 as usize;
                            if cur_idx >= copy_edges_to.len() {
                                continue;
                            }
                            for src in copy_edges_to[cur_idx].clone() {
                                let src_idx = src.0 as usize;
                                if src_idx < moved.len() && !moved[src_idx] {
                                    moved[src_idx] = true;
                                    *changed = true;
                                    stack.push(src);
                                }
                            }
                        }
                    }
                }
            }
        };
        for block in &body.blocks {
            // Rvalue-position calls (the `Ok(...)` /
            // result-ctor path uses `Rvalue::CallIntrinsic
            // { name: "gos_rt_result_new", args: [disc, payload] }`).
            // Without this arm, a `Vec` inside a struct that's
            // wrapped in `Result::Ok(R { xs: v })` was not
            // recognised as moved-into-return and the drop pass
            // freed it before the caller unwrapped, producing a
            // dangling Vec in the returned `Result`.
            for stmt in &block.stmts {
                let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                    continue;
                };
                if let Rvalue::CallIntrinsic { name, args } = rvalue {
                    // `gos_store(obj, off, val)`: storing `val` into heap
                    // object `obj`. When `obj` escapes into the return
                    // value (a recursive-enum payload, e.g.
                    // `J::Arr(v)` stored as `gos_store(arr, 8, v)` then
                    // `return arr`), `val` escapes with it. Freeing `val`
                    // here would dangle the returned object's child
                    // pointer - exactly the `Vec`-in-enum crash.
                    if *name == "gos_store"
                        && let Some(Operand::Copy(obj_p)) = args.first()
                        && obj_p.projection.is_empty()
                    {
                        let obj_idx = obj_p.local.0 as usize;
                        if obj_idx < moved_into_return.len() && moved_into_return[obj_idx] {
                            if let Some(val) = args.get(2) {
                                // A Vec payload stored into an enum box is
                                // balanced (constructor retain + box-owned
                                // free through the kind-tagged meta), so
                                // the frame's own free stays; only
                                // non-container children escape with the
                                // returned box.
                                let balanced =
                                    enum_box_locals.get(obj_idx).copied().unwrap_or(false)
                                        && is_container_local(val);
                                if !balanced {
                                    propagate_call_args(
                                        std::slice::from_ref(val),
                                        &mut moved_into_return,
                                        &mut changed,
                                    );
                                }
                            }
                        }
                        continue;
                    }
                }
                if place.projection.is_empty()
                    && let Rvalue::CallIntrinsic { args, .. } = rvalue
                {
                    let dest_idx = place.local.0 as usize;
                    if dest_idx >= moved_into_return.len() || !moved_into_return[dest_idx] {
                        continue;
                    }
                    propagate_call_args(args, &mut moved_into_return, &mut changed);
                }
            }
            // `gos_rt_vec_push(container, elem)`: the element's heap
            // ownership moves into the container, which deep-frees its direct
            // elements on drop or carries them to the caller when returned -
            // either way an independent drop of the element here would
            // double-free / dangle. Mark the direct element unconditionally.
            // Done inside the fixpoint (not a separate pass) so a pushed enum's
            // own escaped children - `inner` in `outer.push(J::Arr(inner))`,
            // reached via the `gos_store` rule above - propagate through
            // arbitrarily deep nesting.
            //
            // The element's TRANSITIVE children only escape when the container
            // itself does. When the pushed element is a tuple aggregate
            // `(k, J::Map(inner))`, the nested enum box and the `inner` Vec it
            // owns reach the caller only if the container is returned; then
            // walking the copy-edge graph back from the element suppresses
            // their drops so they survive the escape. When the container is
            // freed locally its deep-free reclaims the direct tuple element but
            // does not recurse into the nested Vec's own elements, so those keep
            // their independent drops - suppressing them unconditionally would
            // leak. Gate the copy-edge walk on the container being
            // moved-into-return.
            if let Terminator::Call { callee, args, .. } = &block.terminator
                && let Operand::Const(ConstValue::Str(name)) = callee
                && is_element_push(name)
                && let Some(elem_op @ Operand::Copy(p)) = args.get(1)
                && p.projection.is_empty()
                && !is_container_local(elem_op)
            {
                let idx = p.local.0 as usize;
                if idx < moved_into_return.len() && !moved_into_return[idx] {
                    moved_into_return[idx] = true;
                    changed = true;
                }
                if let Some(Operand::Copy(container)) = args.first()
                    && container.projection.is_empty()
                    && (container.local.0 as usize) < moved_into_return.len()
                    && moved_into_return[container.local.0 as usize]
                {
                    // Walk the copy-edge graph back from the element's children
                    // (a tuple aggregate's `Copy(enum_box)` operand), marking
                    // each transitively. Starting from `p.local` rather than
                    // calling `propagate_call_args` avoids its short-circuit on
                    // the already-marked element, which would stop before the
                    // enum box. Marking the enum box lets the fixpoint's
                    // `gos_enum_tag` / `gos_store` rules carry moved-ness on to
                    // the nested `inner` Vec.
                    let mut stack = vec![p.local];
                    while let Some(cur) = stack.pop() {
                        let cur_idx = cur.0 as usize;
                        if cur_idx >= copy_edges_to.len() {
                            continue;
                        }
                        for src in copy_edges_to[cur_idx].clone() {
                            let src_idx = src.0 as usize;
                            if src_idx < moved_into_return.len() && !moved_into_return[src_idx] {
                                moved_into_return[src_idx] = true;
                                changed = true;
                                stack.push(src);
                            }
                        }
                    }
                }
            }
            if let Terminator::Call {
                callee,
                destination,
                args,
                ..
            } = &block.terminator
            {
                if !destination.projection.is_empty() {
                    continue;
                }
                let dest_idx = destination.local.0 as usize;
                if dest_idx >= moved_into_return.len() || !moved_into_return[dest_idx] {
                    continue;
                }
                // Only aggregate-constructor callees actually move
                // their args into the destination value. Generic
                // Calls (println, str_concat, map_get_or, every
                // user fn) consume their args without retaining
                // them, so propagating "moved" through their args
                // would mark unrelated heap-owning locals as
                // moved-into-return and silently skip their drops.
                if !is_aggregate_ctor_callee(callee) {
                    continue;
                }
                propagate_call_args(args, &mut moved_into_return, &mut changed);
            }
        }
    }

    // (`gos_rt_vec_push` element-ownership transfer is handled inside
    // the fixpoint above so it composes with the `gos_store` rule for
    // arbitrarily deep enum/container nesting.)

    // A `HashMap` consumed as an operand of a struct/tuple `Rvalue::Aggregate`
    // is MOVED into that aggregate's field WHEN nothing mints a share for the
    // slot: ownership transfers and the aggregate's field-death release is the
    // only one, so freeing it here too would double-free. A slot the retain
    // pass gives a table of its own is the other side of that condition - the
    // frame still owns what it built and releases it here.
    //
    // A `Vec` / `[T]` operand is the other case and is deliberately absent:
    // the construction mints the field's share, so the frame keeps the release
    // of the sequence it built. Suppressing that release left the
    // construction's own share with nothing to return it, and a frame building
    // such a value in a loop held every buffer it ever built.
    let moved_into_aggregate = {
        let mut moved = vec![false; body.locals.len()];
        // A slot that takes a table of its own (the retain pass's aggregate
        // arm) leaves the frame the release of the one it built - the same
        // schedule a `Set`, a `Deque`, and a heap operand already run.
        for block in &body.blocks {
            for stmt in &block.stmts {
                if let StatementKind::Assign {
                    place,
                    rvalue: Rvalue::Aggregate { operands, .. },
                } = &stmt.kind
                {
                    for (idx, op) in operands.iter().enumerate() {
                        if let Operand::Copy(p) = op
                            && p.projection.is_empty()
                            && (p.local.0 as usize) < moved.len()
                            && matches!(
                                tcx.kind_of(body.locals[p.local.0 as usize].ty),
                                TyKind::HashMap { .. }
                            )
                            && !aggregate_slot_takes_own_container(
                                tcx, body, place, idx, p, &call_dest,
                            )
                        {
                            moved[p.local.0 as usize] = true;
                        }
                    }
                }
            }
        }
        moved
    };

    let mints_own_share = aggregates_minting_their_own_share(body, tcx);

    // Pass 3: collect drop targets in stable local-index order.
    // The constructor-name → free-name table already restricts
    // candidates to runtime container shapes; we trust the MIR's
    // type assignment and skip a redundant TyKind check here.
    let _ = TyKind::Bool; // silence unused-import lint outside the closure
    // A store into a module global hands the container to a cell that outlives
    // every frame, so the frame that built it keeps no claim on it.
    let stored_in_static = {
        let mut stored = vec![false; body.locals.len()];
        for block in &body.blocks {
            for stmt in &block.stmts {
                if let StatementKind::StaticStore {
                    value: Operand::Copy(p),
                    ..
                } = &stmt.kind
                    && p.projection.is_empty()
                    && (p.local.0 as usize) < stored.len()
                {
                    stored[p.local.0 as usize] = true;
                }
            }
        }
        stored
    };
    // A value container sent through a channel is the receiver's from then
    // on: its handle carries no count, so the send hands the table itself over
    // and the sender keeps no claim on it.
    let mut sent_away = vec![false; owner_ctor.len()];
    for block in &body.blocks {
        if let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            args,
            ..
        } = &block.terminator
            && matches!(name.as_str(), "gos_rt_chan_send" | "gos_rt_chan_try_send")
            && let Some(Operand::Copy(value)) = args.get(1)
            && value.projection.is_empty()
            && (value.local.0 as usize) < sent_away.len()
        {
            sent_away[value.local.0 as usize] = true;
        }
    }
    let drop_targets_all: Vec<(Local, &'static str)> = (0..owner_ctor.len())
        .filter_map(|i| {
            let free = owner_ctor[i]?;
            if stored_in_static[i] {
                return None;
            }
            if sent_away[i]
                && matches!(
                    free,
                    "gos_rt_map_free" | "gos_rt_set_free" | "gos_rt_deque_free"
                )
            {
                return None;
            }
            if (moved_into_return[i] || moved_into_aggregate[i]) && !mints_own_share[i] {
                return None;
            }
            Some((Local(i as u32), free))
        })
        .collect();

    // Non-aliased container ctor locals get full per-site management below
    // (zero-init + drop-before-overwrite + at-return, all null-safe) so a
    // container rebuilt each loop iteration frees every prior allocation
    // instead of leaking all but the last. Aliased locals (the source of a
    // bare `Copy`) are left to the conservative return-only path - freeing one
    // before its reassignment could dangle the alias. Locals captured by a
    // call were already disqualified from `owner_ctor` in pass 1. `aliased`
    // was computed once after pass 1 and shared with the move-transfer.
    let reuse: Vec<(Local, &'static str)> = drop_targets_all
        .iter()
        .filter(|(l, free)| {
            // Every counted container reclaims per site, not only the two
            // that were named here: a `Set`, a `Deque`, a `Queue`, and a
            // `Stack` rebuilt each iteration reached the return-only path and
            // so freed all but the last. A `http::Response` box is the same
            // per-site shape: one built per turn of a loop is reclaimed on
            // each turn rather than growing the process by one response, and
            // so is the lazy state a loop over an adapter chain builds.
            !aliased[l.0 as usize]
                && matches!(
                    *free,
                    "gos_rt_vec_free"
                        | "gos_rt_map_free"
                        | "gos_rt_set_free"
                        | "gos_rt_deque_free"
                        | "gos_rt_http_response_free"
                        | "gos_rt_lazy_iter_drop_i64"
                        | "gos_rt_lazy_iter_drop_pair_i64"
                )
        })
        .copied()
        .collect();
    let reuse_set: std::collections::BTreeSet<u32> = reuse.iter().map(|(l, _)| l.0).collect();
    let drop_targets: Vec<(Local, &'static str)> = drop_targets_all
        .into_iter()
        .filter(|(l, _)| !reuse_set.contains(&l.0))
        .collect();

    if drop_targets.is_empty() && reuse.is_empty() {
        return;
    }

    // Per-target must-init dataflow. For each drop target `L`,
    // compute `init_at_return[L][R]` - `true` when every path from
    // entry to Return block `R` passes through at least one
    // definition of `L`. A definition is a Call terminator whose
    // destination is `L` or a stmt-position assignment to `L`.
    //
    // The earlier (type-only) pass scheduled a free at every
    // Return for every recognised owner local, including shapes
    // like `let m: HashMap<...>; if cond { m = HashMap::new() };
    // return m;` where the `else` branch reaches Return without
    // ever initialising `m`. Calling `gos_rt_map_free` on the
    // uninit slot aborts in the allocator metadata probe.
    //
    // Approach: minimal forward dataflow with intersection at
    // joins (the "must-init" lattice). Drops are emitted only at
    // Return blocks where the target is must-init at the point of
    // return; cases where the proof is undecidable (irreducible
    // CFG, complex loops) conservatively skip the drop - a leak
    // is preferable to a free of uninit memory.
    let init_at_return = compute_init_at_returns(body, &drop_targets);

    for block_idx in 0..last_block {
        if !matches!(body.blocks[block_idx].terminator, Terminator::Return) {
            continue;
        }
        let span = body.blocks[block_idx].span;
        let init_row = &init_at_return[block_idx];
        for (target_idx, (local, free_name)) in drop_targets.iter().enumerate() {
            if !init_row[target_idx] {
                continue;
            }
            let dest = Local(u32::try_from(body.locals.len()).expect("local overflow"));
            let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
            body.locals.push(LocalDecl {
                ty: unit_ty,
                debug_name: None,
                mutable: false,
                region: false,
            });
            // Emit the free as a CallIntrinsic stmt - the cranelift
            // lowerer's statement path handles it without any block
            // rewiring. `gos_rt_aggr_free` needs a second `size`
            // arg the codegen derives from the local's type; all
            // other helpers (Vec/Map/Set/...) are single-arg.
            // `gos_rt_aggr_free` takes 2 args (ptr + size); the
            // other heap-container free helpers take only the
            // receiver pointer.
            let args = if *free_name == "gos_rt_aggr_free" {
                let size = aggr_size_bytes(tcx, body.locals[local.0 as usize].ty);
                vec![
                    Operand::Copy(Place::local(*local)),
                    Operand::Const(ConstValue::Int(i128::from(size))),
                ]
            } else {
                vec![Operand::Copy(Place::local(*local))]
            };
            body.blocks[block_idx].stmts.push(Statement {
                kind: StatementKind::Assign {
                    place: Place::local(dest),
                    rvalue: Rvalue::CallIntrinsic {
                        name: free_name,
                        args,
                    },
                },
                span,
                inlined: None,
            });
        }
    }

    // drop-before-overwrite for aggregate
    // reassignments. Skip sites where the local is not provably
    // initialised on every path leading to this statement -
    // freeing an uninitialised aggregate local reads garbage from
    // the Cranelift Variable slot and aborts in `__libc_free`.
    //
    // For each candidate site, compute "is local must-init at
    // block entry?" via the same dataflow used by
    // `compute_init_at_returns`. Then walk the block statements
    // up to `stmt_idx`, updating must-init on each Assign to
    // this local. Drop is emitted only if must-init is true at
    // the point of the candidate stmt.
    let candidate_locals: Vec<Local> = drop_before_sites
        .iter()
        .map(|(_, _, l, _)| *l)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let init_at_each_return = if candidate_locals.is_empty() {
        Vec::new()
    } else {
        let targets: Vec<(Local, &'static str)> = candidate_locals
            .iter()
            .map(|l| (*l, "gos_rt_aggr_free"))
            .collect();
        compute_init_at_block_entries(body, &targets)
    };
    let local_to_target_idx: std::collections::BTreeMap<Local, usize> = candidate_locals
        .iter()
        .enumerate()
        .map(|(i, l)| (*l, i))
        .collect();
    let must_init_at = |block_idx: usize, stmt_idx: usize, local: Local| -> bool {
        let Some(target_idx) = local_to_target_idx.get(&local) else {
            return false;
        };
        if block_idx >= init_at_each_return.len() {
            return false;
        }
        let mut init = init_at_each_return[block_idx][*target_idx];
        // Walk stmts up to stmt_idx and update must-init based on
        // Assign destinations.
        for (i, stmt) in body.blocks[block_idx].stmts.iter().enumerate() {
            if i >= stmt_idx {
                break;
            }
            if let StatementKind::Assign { place, .. } = &stmt.kind
                && place.projection.is_empty()
                && place.local == local
            {
                init = true;
            }
        }
        init
    };
    drop_before_sites.sort_by_key(|a| (a.0, a.1));
    let drop_before_sites: Vec<_> = drop_before_sites
        .into_iter()
        .filter(|(b, s, l, _)| must_init_at(*b, *s, *l))
        .collect();
    for (block_idx, stmt_idx, local, size) in drop_before_sites.into_iter().rev() {
        if block_idx >= body.blocks.len() {
            continue;
        }
        let span = body.blocks[block_idx]
            .stmts
            .get(stmt_idx)
            .map_or(body.blocks[block_idx].span, |s| s.span);
        let dest = Local(u32::try_from(body.locals.len()).expect("local overflow"));
        let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
        body.locals.push(LocalDecl {
            ty: unit_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
        let drop_stmt = Statement {
            kind: StatementKind::Assign {
                place: Place::local(dest),
                rvalue: Rvalue::CallIntrinsic {
                    name: "gos_rt_aggr_free",
                    args: vec![
                        Operand::Copy(Place::local(local)),
                        Operand::Const(ConstValue::Int(i128::from(size))),
                    ],
                },
            },
            span,
            inlined: None,
        };
        body.blocks[block_idx].stmts.insert(stmt_idx, drop_stmt);
    }

    // Drop-before-overwrite at each move-transfer copy `dst = Copy(src)`:
    // free `dst`'s previous value before it is rebound, so a container
    // moved into an outer binding every loop iteration reclaims each prior
    // buffer. Null-safe on the first pass via the reuse zero-init below.
    // Restricted to `dst` locals that reached `reuse` (a non-aliased
    // Vec/Map owner not moved into the return slot); a `dst` moved into the
    // return is freed by the caller instead. Inserted in reverse
    // (block, stmt) order so earlier statement indices stay valid, and
    // before the reuse zero-init prepends at block 0.
    if !move_copy_sites.is_empty() {
        let mut sites: Vec<(usize, usize, Local, &'static str)> = move_copy_sites
            .iter()
            .filter_map(|&(bi, si, dst)| {
                reuse
                    .iter()
                    .find(|(l, _)| *l == dst)
                    .map(|(_, free)| (bi, si, dst, *free))
            })
            .collect();
        sites.sort_by_key(|&(bi, si, _, _)| (bi, si));
        let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
        for (block_idx, stmt_idx, local, free_name) in sites.into_iter().rev() {
            if block_idx >= body.blocks.len() || stmt_idx > body.blocks[block_idx].stmts.len() {
                continue;
            }
            let dest = Local(u32::try_from(body.locals.len()).expect("local overflow"));
            body.locals.push(LocalDecl {
                ty: unit_ty,
                debug_name: None,
                mutable: false,
                region: false,
            });
            let span = body.blocks[block_idx]
                .stmts
                .get(stmt_idx)
                .map_or(body.blocks[block_idx].span, |s| s.span);
            body.blocks[block_idx].stmts.insert(
                stmt_idx,
                Statement {
                    kind: StatementKind::Assign {
                        place: Place::local(dest),
                        rvalue: Rvalue::CallIntrinsic {
                            name: free_name,
                            args: vec![Operand::Copy(Place::local(local))],
                        },
                    },
                    span,
                    inlined: None,
                },
            );
        }
    }

    // Empty each moved origin right after the copy that moved it. The copy is
    // found by its content, since the insertions above shift statement indices.
    for &(block_idx, dst, src, origin) in &moved_sources {
        let Some(block) = body.blocks.get_mut(block_idx) else {
            continue;
        };
        let Some(copy_idx) = block.stmts.iter().position(|stmt| {
            matches!(
                &stmt.kind,
                StatementKind::Assign { place, rvalue: Rvalue::Use(Operand::Copy(from)) }
                    if place.projection.is_empty()
                        && place.local == dst
                        && from.projection.is_empty()
                        && from.local == src
            )
        }) else {
            continue;
        };
        let span = block.stmts[copy_idx].span;
        block.stmts.insert(
            copy_idx + 1,
            Statement {
                kind: StatementKind::Assign {
                    place: Place::local(origin),
                    rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                },
                span,
                inlined: None,
            },
        );
    }

    // Dedicated lifetime for non-aliased Vec/Map ctor locals: zero-init at
    // entry (null), free the previous value before each ctor-Call that
    // reassigns the local (loop reuse), and free the final value at every
    // Return. Every free is null-safe (`gos_rt_vec_free` / `gos_rt_map_free`
    // no-op on null), so this needs no path-sensitive must-init proof and never
    // double-frees: the drop-before frees prior allocations, the at-Return
    // frees the last one, and a never-constructed local stays null.
    if !reuse.is_empty() {
        let span0 = body.blocks[0].span;
        for (local, _) in reuse.iter().rev() {
            body.blocks[0].stmts.insert(
                0,
                Statement {
                    kind: StatementKind::Assign {
                        place: Place::local(*local),
                        rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                    },
                    span: span0,
                    inlined: None,
                },
            );
        }
        let free_of: std::collections::BTreeMap<u32, &'static str> =
            reuse.iter().map(|(l, f)| (l.0, *f)).collect();
        // (block_idx, free_name, local) - each appended to the block's stmts,
        // i.e. just before its terminator.
        let mut sites: Vec<(usize, &'static str, Local)> = Vec::new();
        for (block_idx, block) in body.blocks.iter().enumerate() {
            match &block.terminator {
                Terminator::Call {
                    destination, args, ..
                } if destination.projection.is_empty() => {
                    // A call that READS its own destination (`xs = f(xs)`)
                    // still needs the old value live when it runs, so the
                    // drop-before-overwrite is skipped there; the prior
                    // binding is reclaimed by the at-return free instead.
                    let self_read = args.iter().any(|a| {
                        matches!(a, Operand::Copy(p)
                            if p.projection.is_empty() && p.local == destination.local)
                    });
                    if !self_read && let Some(&free_name) = free_of.get(&destination.local.0) {
                        sites.push((block_idx, free_name, destination.local));
                    }
                }
                Terminator::Return => {
                    for (local, free_name) in &reuse {
                        sites.push((block_idx, *free_name, *local));
                    }
                }
                _ => {}
            }
        }
        // A map taken out of an owned carrier is defined by a statement, so
        // its previous value is freed just before that statement runs.
        let mut stmt_sites: Vec<(usize, usize, &'static str, Local)> = Vec::new();
        for (block_idx, block) in body.blocks.iter().enumerate() {
            for (stmt_idx, stmt) in block.stmts.iter().enumerate() {
                if let StatementKind::Assign {
                    place,
                    rvalue:
                        Rvalue::CallIntrinsic {
                            name: "gos_rt_result_payload",
                            ..
                        },
                } = &stmt.kind
                    && place.projection.is_empty()
                    && let Some(&free_name) = free_of.get(&place.local.0)
                {
                    stmt_sites.push((block_idx, stmt_idx, free_name, place.local));
                }
            }
        }
        let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
        for (block_idx, stmt_idx, free_name, local) in stmt_sites.into_iter().rev() {
            let dest = Local(u32::try_from(body.locals.len()).expect("local overflow"));
            body.locals.push(LocalDecl {
                ty: unit_ty,
                debug_name: None,
                mutable: false,
                region: false,
            });
            let span = body.blocks[block_idx].span;
            body.blocks[block_idx].stmts.insert(
                stmt_idx,
                Statement {
                    kind: StatementKind::Assign {
                        place: Place::local(dest),
                        rvalue: Rvalue::CallIntrinsic {
                            name: free_name,
                            args: vec![Operand::Copy(Place::local(local))],
                        },
                    },
                    span,
                    inlined: None,
                },
            );
        }
        for (block_idx, free_name, local) in sites {
            let dest = Local(u32::try_from(body.locals.len()).expect("local overflow"));
            body.locals.push(LocalDecl {
                ty: unit_ty,
                debug_name: None,
                mutable: false,
                region: false,
            });
            let span = body.blocks[block_idx].span;
            body.blocks[block_idx].stmts.push(Statement {
                kind: StatementKind::Assign {
                    place: Place::local(dest),
                    rvalue: Rvalue::CallIntrinsic {
                        name: free_name,
                        args: vec![Operand::Copy(Place::local(local))],
                    },
                },
                span,
                inlined: None,
            });
        }
    }
}

/// Rewrites `gos_rt_str_concat` calls to the consuming variant when the MIR
/// emits the copy-back pattern: `tmp = str_concat(out, frag); out = Copy(tmp)`.
///
/// The Gossamer MIR builder lowers `out += frag` as two instructions across
/// two basic blocks:
///
/// ```text
/// bb_n:  Call { gos_rt_str_concat, [Copy(out), Copy(frag)] → tmp, target: bb_succ }
/// bb_succ: Assign { out ← Use(Copy(tmp)) }; …
/// ```
///
/// After the copy-back, the OLD value of `out` is unreachable. Without the consuming
/// variant, that allocation leaks on every loop iteration, producing O(n²) total
/// allocations for an accumulation loop over n elements.
///
/// `gos_rt_str_concat_drop_a(out, frag)` reads both args, allocates the result,
/// then frees `out` - safe because the free happens after the read. It no-ops
/// silently on null and rodata/literal `out` values.
/// Counts how many times each local is *read* across the whole body (as an
/// operand, a `Ref`/`Len`/`Drop` place base, a projected store base, or an
/// `Index` projection). Assignment / call-`destination` positions are writes
/// and are not counted. Used by [`fuse_substring_map_inc`] to prove the scratch
/// key String flows only into the fused probe.
fn collect_local_read_counts(body: &Body) -> HashMap<u32, usize> {
    fn read_index_locals(place: &Place, counts: &mut HashMap<u32, usize>) {
        for proj in &place.projection {
            if let crate::ir::Projection::Index(idx) = proj {
                *counts.entry(idx.0).or_insert(0) += 1;
            }
        }
    }
    fn read_place(place: &Place, counts: &mut HashMap<u32, usize>) {
        *counts.entry(place.local.0).or_insert(0) += 1;
        read_index_locals(place, counts);
    }
    // A store destination reads its base only when addressed through a
    // projection (`*p = v`, `a[i] = v`); a bare `x = v` is a pure write.
    fn read_store_dest(place: &Place, counts: &mut HashMap<u32, usize>) {
        if !place.projection.is_empty() {
            *counts.entry(place.local.0).or_insert(0) += 1;
        }
        read_index_locals(place, counts);
    }
    fn read_operand(op: &Operand, counts: &mut HashMap<u32, usize>) {
        if let Operand::Copy(place) = op {
            read_place(place, counts);
        }
    }
    fn read_rvalue(rv: &Rvalue, counts: &mut HashMap<u32, usize>) {
        match rv {
            Rvalue::Use(op)
            | Rvalue::UnaryOp { operand: op, .. }
            | Rvalue::Cast { operand: op, .. } => {
                read_operand(op, counts);
            }
            Rvalue::BinaryOp { lhs, rhs, .. } => {
                read_operand(lhs, counts);
                read_operand(rhs, counts);
            }
            Rvalue::Aggregate { operands, .. } => {
                for op in operands {
                    read_operand(op, counts);
                }
            }
            Rvalue::Repeat { value, .. } => read_operand(value, counts),
            Rvalue::CallIntrinsic { args, .. } => {
                for op in args {
                    read_operand(op, counts);
                }
            }
            Rvalue::Len(place) | Rvalue::Ref { place, .. } => read_place(place, counts),
            Rvalue::StaticLoad(_) => {}
        }
    }
    let mut counts: HashMap<u32, usize> = HashMap::new();
    for block in &body.blocks {
        for stmt in &block.stmts {
            match &stmt.kind {
                StatementKind::Assign { place, rvalue } => {
                    read_store_dest(place, &mut counts);
                    read_rvalue(rvalue, &mut counts);
                }
                StatementKind::SetDiscriminant { place, .. } => read_store_dest(place, &mut counts),
                StatementKind::StaticStore { value, .. } => read_operand(value, &mut counts),
                StatementKind::IterSource { dst, source, .. } => {
                    read_store_dest(dst, &mut counts);
                    read_operand(source, &mut counts);
                }
                StatementKind::IterAdapter {
                    dst,
                    upstream,
                    closure_or_arg,
                    ..
                } => {
                    read_store_dest(dst, &mut counts);
                    read_place(upstream, &mut counts);
                    if let Some(arg) = closure_or_arg {
                        read_operand(arg, &mut counts);
                    }
                }
                StatementKind::IterNext {
                    dst_option,
                    iter_place,
                    ..
                } => {
                    read_store_dest(dst_option, &mut counts);
                    read_place(iter_place, &mut counts);
                }
                StatementKind::StorageLive(_)
                | StatementKind::StorageDead(_)
                | StatementKind::Nop => {}
            }
        }
        match &block.terminator {
            Terminator::SwitchInt { discriminant, .. } => read_operand(discriminant, &mut counts),
            Terminator::Call {
                callee,
                args,
                destination,
                ..
            } => {
                read_operand(callee, &mut counts);
                for op in args {
                    read_operand(op, &mut counts);
                }
                read_store_dest(destination, &mut counts);
            }
            Terminator::Assert { cond, msg, .. } => {
                read_operand(cond, &mut counts);
                for op in msg.operands() {
                    read_operand(op, &mut counts);
                }
            }
            Terminator::Drop { place, .. } => read_place(place, &mut counts),
            Terminator::Goto { .. }
            | Terminator::Return
            | Terminator::Unreachable
            | Terminator::Panic { .. } => {}
        }
    }
    counts
}

/// Fuses `kmer = seq.substring(i, i + k); m.inc(kmer, by)` into a single
/// borrowed-slice probe `gos_rt_map_inc_at_str_i64(m, seq, i, len, by)`, where
/// `len = (i + k) - i`. The scratch String the substring would allocate on
/// every probe is removed; the borrowed shim materialises a key only on the
/// first occurrence of each distinct k-mer (k-nucleotide's hot `count_kmers`
/// loop). Runs before the RC passes so no retain/release is emitted for the
/// String that no longer exists.
///
/// The rewrite only fires when the scratch String flows *only* into the probe
/// (read exactly once through the copy, and the key read exactly once by the
/// `inc`), so a k-mer observed elsewhere keeps the allocating path.
pub(crate) fn fuse_substring_map_inc(body: &mut Body) {
    let n = body.blocks.len();
    let reads = collect_local_read_counts(body);

    struct Plan {
        substr_idx: usize,
        inc_idx: usize,
        seq: Operand,
        start: Operand,
        end: Operand,
        start_ty: Ty,
        m: Operand,
        by: Operand,
        inc_dest: Place,
        inc_target: Option<BlockId>,
        subst_local: Local,
        remove_copy_local: Option<Local>,
    }

    let mut plans: Vec<Plan> = Vec::new();
    for inc_idx in 0..n {
        let Terminator::Call {
            callee,
            args,
            destination: inc_dest,
            target: inc_target,
        } = &body.blocks[inc_idx].terminator
        else {
            continue;
        };
        let Operand::Const(ConstValue::Str(name)) = callee else {
            continue;
        };
        if !matches!(
            name.as_str(),
            "gos_rt_map_inc_str_i64" | "gos_rt_map_inc_typed_str_i64"
        ) || args.len() != 3
        {
            continue;
        }
        let Operand::Copy(key_place) = &args[1] else {
            continue;
        };
        if !key_place.projection.is_empty() {
            continue;
        }
        let key_local = key_place.local;
        let m = args[0].clone();
        let by = args[2].clone();

        // Resolve the String source: either the key is copied from the
        // substring result inside this block (`key = Copy(subst)`), or the
        // substring result is used as the key directly.
        let mut subst_local = key_local;
        let mut remove_copy_local: Option<Local> = None;
        for stmt in &body.blocks[inc_idx].stmts {
            if let StatementKind::Assign {
                place,
                rvalue: Rvalue::Use(Operand::Copy(src)),
            } = &stmt.kind
                && place.local == key_local
                && place.projection.is_empty()
                && src.projection.is_empty()
            {
                subst_local = src.local;
                remove_copy_local = Some(key_local);
            }
        }

        // Find the `gos_rt_str_substring` producing `subst_local`, whose sole
        // successor is this inc block.
        let mut found: Option<(usize, Operand, Operand, Operand)> = None;
        for substr_idx in 0..n {
            let Terminator::Call {
                callee: sc,
                args: sa,
                destination: sd,
                target: Some(st),
            } = &body.blocks[substr_idx].terminator
            else {
                continue;
            };
            let Operand::Const(ConstValue::Str(sname)) = sc else {
                continue;
            };
            if sname != "gos_rt_str_substring"
                || sa.len() != 3
                || sd.local != subst_local
                || !sd.projection.is_empty()
                || st.0 as usize != inc_idx
            {
                continue;
            }
            found = Some((substr_idx, sa[0].clone(), sa[1].clone(), sa[2].clone()));
            break;
        }
        let Some((substr_idx, seq, start, end)) = found else {
            continue;
        };

        // `start` must be a bare local so `len = end - start` is well-typed
        // and readable at the inc block (its i64 type also types `len`).
        let Operand::Copy(start_place) = &start else {
            continue;
        };
        if !start_place.projection.is_empty() {
            continue;
        }
        let start_ty = body.locals[start_place.local.0 as usize].ty;

        // The scratch String must flow only into the probe.
        let subst_reads = reads.get(&subst_local.0).copied().unwrap_or(0);
        let key_reads = reads.get(&key_local.0).copied().unwrap_or(0);
        if subst_local == key_local {
            if key_reads != 1 {
                continue;
            }
        } else if subst_reads != 1 || key_reads != 1 {
            continue;
        }

        plans.push(Plan {
            substr_idx,
            inc_idx,
            seq,
            start,
            end,
            start_ty,
            m,
            by,
            inc_dest: inc_dest.clone(),
            inc_target: *inc_target,
            subst_local,
            remove_copy_local,
        });
    }

    for plan in plans {
        // Fresh `len` local (i64), computed where `start`/`end` are live.
        let len_local = Local(body.locals.len() as u32);
        body.locals.push(LocalDecl {
            ty: plan.start_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
        let substr_span = body.blocks[plan.substr_idx].span;
        let inc_block_id = body.blocks[plan.inc_idx].id;
        {
            let substr_block = &mut body.blocks[plan.substr_idx];
            substr_block.stmts.push(Statement {
                kind: StatementKind::Assign {
                    place: Place::local(len_local),
                    rvalue: Rvalue::BinaryOp {
                        op: BinOp::Sub,
                        lhs: plan.end.clone(),
                        rhs: plan.start.clone(),
                    },
                },
                span: substr_span,
                inlined: None,
            });
            // Null the scratch String slot so it is a defined null: any release
            // the RC pass may still schedule for its declared `String` type is
            // then a no-op rather than a read of an unassigned slot.
            substr_block.stmts.push(Statement {
                kind: StatementKind::Assign {
                    place: Place::local(plan.subst_local),
                    rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                },
                span: substr_span,
                inlined: None,
            });
            substr_block.terminator = Terminator::Goto {
                target: inc_block_id,
            };
        }
        {
            let inc_block = &mut body.blocks[plan.inc_idx];
            if let Some(copy_local) = plan.remove_copy_local {
                inc_block.stmts.retain(|stmt| {
                    !matches!(
                        &stmt.kind,
                        StatementKind::Assign {
                            place,
                            rvalue: Rvalue::Use(Operand::Copy(src)),
                        } if place.local == copy_local
                            && place.projection.is_empty()
                            && src.local == plan.subst_local
                            && src.projection.is_empty()
                    )
                });
            }
            inc_block.terminator = Terminator::Call {
                callee: Operand::Const(ConstValue::Str("gos_rt_map_inc_at_str_i64".to_string())),
                args: vec![
                    plan.m,
                    plan.seq,
                    plan.start,
                    Operand::Copy(Place::local(len_local)),
                    plan.by,
                ],
                destination: plan.inc_dest,
                target: plan.inc_target,
            };
        }
    }
}

pub(crate) fn rewrite_str_concat_consuming(body: &mut Body) {
    let n_blocks = body.blocks.len();
    // Collect rename targets: (block_idx) where the Call should be renamed.
    let mut targets: Vec<usize> = Vec::new();
    for block_idx in 0..n_blocks {
        let Terminator::Call {
            callee,
            args,
            destination,
            target,
        } = &body.blocks[block_idx].terminator
        else {
            continue;
        };
        // Must be a str_concat call.
        let Operand::Const(ConstValue::Str(name)) = callee else {
            continue;
        };
        if name != "gos_rt_str_concat" {
            continue;
        }
        // Destination must be a bare local (no projection).
        if !destination.projection.is_empty() {
            continue;
        }
        let tmp_local = destination.local;
        // First arg must be a bare Copy of some local `src`.
        let Some(Operand::Copy(src_place)) = args.first() else {
            continue;
        };
        if !src_place.projection.is_empty() {
            continue;
        }
        let src_local = src_place.local;
        // If first-arg == destination (no copy-back needed), rename directly.
        if src_local == tmp_local {
            targets.push(block_idx);
            continue;
        }
        // Otherwise: check that the successor block's FIRST statement copies
        // `tmp` back into `src` - the copy-back pattern.
        let Some(succ_id) = target else { continue };
        let succ_idx = succ_id.0 as usize;
        if succ_idx >= n_blocks {
            continue;
        }
        let first_stmt = body.blocks[succ_idx].stmts.first();
        let is_copy_back = matches!(
            first_stmt,
            Some(Statement {
                kind: StatementKind::Assign {
                    place,
                    rvalue: Rvalue::Use(Operand::Copy(src_of_copy)),
                },
                ..
            }) if place.local == src_local
                && place.projection.is_empty()
                && src_of_copy.local == tmp_local
                && src_of_copy.projection.is_empty()
        );
        if is_copy_back {
            targets.push(block_idx);
        }
    }
    // Apply the renames.
    for block_idx in targets {
        if let Terminator::Call { callee, .. } = &mut body.blocks[block_idx].terminator {
            *callee = Operand::Const(ConstValue::Str("gos_rt_str_concat_drop_a".to_string()));
        }
    }
}

pub(crate) fn compute_init_at_block_entries(
    body: &Body,
    targets: &[(Local, &'static str)],
) -> Vec<Vec<bool>> {
    let n_blocks = body.blocks.len();
    let n_targets = targets.len();
    if n_blocks == 0 || n_targets == 0 {
        return vec![vec![false; n_targets]; n_blocks];
    }

    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n_blocks];
    for (i, block) in body.blocks.iter().enumerate() {
        for s in block_successors(&block.terminator) {
            let si = s.0 as usize;
            if si < n_blocks {
                preds[si].push(i);
            }
        }
    }
    let target_locals: Vec<u32> = targets.iter().map(|(l, _)| l.0).collect();

    let mut stmt_defs = vec![vec![false; n_targets]; n_blocks];
    for (i, block) in body.blocks.iter().enumerate() {
        for stmt in &block.stmts {
            if let StatementKind::Assign { place, .. } = &stmt.kind
                && place.projection.is_empty()
            {
                for (t, l) in target_locals.iter().enumerate() {
                    if place.local.0 == *l {
                        stmt_defs[i][t] = true;
                    }
                }
            }
        }
    }
    let mut term_defs = vec![vec![false; n_targets]; n_blocks];
    for (i, block) in body.blocks.iter().enumerate() {
        if let Terminator::Call { destination, .. } = &block.terminator
            && destination.projection.is_empty()
        {
            for (t, l) in target_locals.iter().enumerate() {
                if destination.local.0 == *l {
                    term_defs[i][t] = true;
                }
            }
        }
    }

    // Must-init ("definitely initialised") is a forward intersection
    // analysis, so its correct solution is the GREATEST fixpoint: seed
    // every block TOP (`true`) and iterate downward. The entry block (no
    // predecessors) pins to `false`, and any loop back-edge that starts
    // `true` lets a value defined before the loop stay must-init across
    // the join instead of collapsing to `false` on the first pass (which
    // a least-fixpoint `false` seed would do, wrongly reporting a
    // pre-loop definition as not-yet-initialised inside the loop).
    let mut init_in = vec![vec![true; n_targets]; n_blocks];
    let mut init_out = vec![vec![true; n_targets]; n_blocks];
    let mut changed = true;
    while changed {
        changed = false;
        for i in 0..n_blocks {
            for t in 0..n_targets {
                let new_in = if preds[i].is_empty() {
                    false
                } else {
                    preds[i].iter().all(|&p| init_out[p][t] || term_defs[p][t])
                };
                let new_out = new_in || stmt_defs[i][t];
                if new_in != init_in[i][t] || new_out != init_out[i][t] {
                    init_in[i][t] = new_in;
                    init_out[i][t] = new_out;
                    changed = true;
                }
            }
        }
    }
    init_in
}

pub(crate) fn compute_init_at_returns(
    body: &Body,
    targets: &[(Local, &'static str)],
) -> Vec<Vec<bool>> {
    let n_blocks = body.blocks.len();
    let n_targets = targets.len();
    let mut out = vec![vec![false; n_targets]; n_blocks];
    if n_blocks == 0 || n_targets == 0 {
        return out;
    }

    // Predecessor map for join nodes.
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n_blocks];
    for (i, block) in body.blocks.iter().enumerate() {
        for s in block_successors(&block.terminator) {
            let si = s.0 as usize;
            if si < n_blocks {
                preds[si].push(i);
            }
        }
    }

    let target_locals: Vec<u32> = targets.iter().map(|(l, _)| l.0).collect();

    // init_in[B][t] - must-init at entry of B.
    // init_out[B][t] - must-init after all of B's stmts (used at
    // the Return point for Return-terminated blocks).
    // Must-init is a forward intersection analysis; its correct
    // solution is the GREATEST fixpoint, so seed every block TOP
    // (`true`) and iterate downward. The entry block pins to `false`
    // (no predecessors), while a loop back-edge seeded `true` keeps a
    // value defined before the loop must-init across the join - a
    // `false` seed would read the join as not-init forever and skip an
    // otherwise-required at-return free.
    let mut init_in = vec![vec![true; n_targets]; n_blocks];
    let mut init_out = vec![vec![true; n_targets]; n_blocks];

    // Pre-compute stmt-position defs per (block, target).
    let mut stmt_defs = vec![vec![false; n_targets]; n_blocks];
    for (i, block) in body.blocks.iter().enumerate() {
        for stmt in &block.stmts {
            if let StatementKind::Assign { place, .. } = &stmt.kind
                && place.projection.is_empty()
            {
                for (t, l) in target_locals.iter().enumerate() {
                    if place.local.0 == *l {
                        stmt_defs[i][t] = true;
                    }
                }
            }
        }
    }
    // Terminator-position defs (Call destinations).
    let mut term_defs = vec![vec![false; n_targets]; n_blocks];
    for (i, block) in body.blocks.iter().enumerate() {
        if let Terminator::Call { destination, .. } = &block.terminator
            && destination.projection.is_empty()
        {
            for (t, l) in target_locals.iter().enumerate() {
                if destination.local.0 == *l {
                    term_defs[i][t] = true;
                }
            }
        }
    }

    // Successors of a Call see the destination as already
    // initialised. Encode that by folding `term_defs[B]` into
    // `init_out[B]` *and* into the value propagated to successors.
    let mut changed = true;
    while changed {
        changed = false;
        for i in 0..n_blocks {
            for t in 0..n_targets {
                // Join: must-init at entry = AND across predecessors.
                let new_in = if preds[i].is_empty() {
                    false
                } else {
                    preds[i].iter().all(|&p| init_out[p][t] || term_defs[p][t])
                };
                // Transfer: pick up stmt defs that fire before any
                // terminator-position read. The Return point reads
                // *after* stmts but the terminator itself is the
                // return - so `init_out` for a Return block sees
                // stmt defs from this block.
                let new_out = new_in || stmt_defs[i][t];
                if new_in != init_in[i][t] || new_out != init_out[i][t] {
                    init_in[i][t] = new_in;
                    init_out[i][t] = new_out;
                    changed = true;
                }
            }
        }
    }

    // For each block, `out[B][t]` is the must-init bit at the
    // *point of return*. Return blocks read `init_out[B]` (defs in
    // this block's stmts count); non-Return blocks see the value
    // they would have at the terminator boundary, which callers
    // ignore - the drop pass only consults Return blocks.
    for i in 0..n_blocks {
        out[i].clone_from(&init_out[i]);
    }
    out
}

pub(crate) fn block_successors(t: &Terminator) -> Vec<BlockId> {
    match t {
        Terminator::Goto { target } => vec![*target],
        Terminator::SwitchInt { arms, default, .. } => {
            let mut out: Vec<BlockId> = arms.iter().map(|(_, b)| *b).collect();
            out.push(*default);
            out
        }
        Terminator::Call { target, .. } => target.iter().copied().collect(),
        Terminator::Assert { target, .. } | Terminator::Drop { target, .. } => vec![*target],
        Terminator::Return | Terminator::Unreachable | Terminator::Panic { .. } => Vec::new(),
    }
}

/// Hoists loop-carried release-before-reassign pairs to the value's
/// last mention in the previous iteration.
///
/// `insert_rc_releases` anchors the release of a reassigned local's
/// OLD value to the reassignment itself. In the ubiquitous loop shape
///
/// ```text
/// loop { tree = build(d); use(&tree) }
/// ```
///
/// the reassignment sits AFTER the next value has been built, so the
/// old and new structures coexist - for binary-trees-style workloads
/// that doubles transient RSS. This pass walks back from each
/// `release(x); x = Copy(tmp)` pair through the unique-predecessor
/// chain to x's last mention, and inserts `release(x); x = null`
/// right after it. The original release stays as a null-safe
/// backstop (releasing null is a no-op), so a missed hoist can only
/// keep the old timing - never double-free.
pub(crate) fn hoist_loop_carried_releases(body: &mut Body, tcx: &gossamer_types::TyCtxt) {
    let n_locals = body.locals.len();
    let n_blocks = body.blocks.len();
    if n_blocks == 0 {
        return;
    }
    let is_rc = |l: Local| -> bool {
        let i = l.0 as usize;
        i < n_locals && tcx.is_rc_managed(body.locals[i].ty) && !body.locals[i].region
    };
    // Predecessor map (multi-pred blocks stop the backward walk).
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n_blocks];
    for (bi, block) in body.blocks.iter().enumerate() {
        let mut add = |t: &BlockId| preds[t.0 as usize].push(bi);
        match &block.terminator {
            Terminator::Goto { target } => add(target),
            Terminator::SwitchInt { arms, default, .. } => {
                for (_, t) in arms {
                    add(t);
                }
                add(default);
            }
            Terminator::Call {
                target: Some(t), ..
            } => add(t),
            Terminator::Assert { target, .. } | Terminator::Drop { target, .. } => add(target),
            _ => {}
        }
    }
    // Successor map for the forward-liveness safety check below.
    let succs: Vec<Vec<usize>> = body
        .blocks
        .iter()
        .map(|b| match &b.terminator {
            Terminator::Goto { target } => vec![target.0 as usize],
            Terminator::SwitchInt { arms, default, .. } => {
                let mut v: Vec<usize> = arms.iter().map(|(_, t)| t.0 as usize).collect();
                v.push(default.0 as usize);
                v
            }
            Terminator::Call { target, .. } => target.iter().map(|t| t.0 as usize).collect(),
            Terminator::Assert { target, .. } => vec![target.0 as usize],
            Terminator::Drop { target, .. } => vec![target.0 as usize],
            _ => Vec::new(),
        })
        .collect();

    // The release-side accounting names whose args are not value READS.
    let accounting_release = |name: &str| -> bool {
        matches!(
            name,
            "gos_rt_rc_release"
                | "gos_rt_rc_weak_release"
                | "gos_rt_aggr_release_children"
                | "gos_rt_aggr_zero_guarded"
                | "gos_rt_option_slot_release"
        )
    };
    // True when the statement READS local x (writes excepted; the
    // backstop release of x itself excepted).
    let stmt_mentions = |stmt: &Statement, x: Local| -> bool {
        let StatementKind::Assign { place, rvalue } = &stmt.kind else {
            return false;
        };
        if !place.projection.is_empty() && place.local == x {
            return true;
        }
        let in_op = |op: &Operand| matches!(op, Operand::Copy(p) if p.local == x);
        match rvalue {
            Rvalue::Use(op) => in_op(op),
            Rvalue::BinaryOp { lhs, rhs, .. } => in_op(lhs) || in_op(rhs),
            Rvalue::UnaryOp { operand, .. } | Rvalue::Cast { operand, .. } => in_op(operand),
            Rvalue::Aggregate { operands, .. } => operands.iter().any(in_op),
            Rvalue::Repeat { value, .. } => in_op(value),
            Rvalue::Ref { place: rp, .. } => rp.local == x,
            Rvalue::Len(p) => p.local == x,
            Rvalue::CallIntrinsic { name, args } => {
                if accounting_release(name) {
                    false
                } else {
                    args.iter().any(in_op)
                }
            }
            // Reads a scalar global by symbol; mentions no local.
            Rvalue::StaticLoad(_) => false,
        }
    };
    let stmt_writes = |stmt: &Statement, x: Local| -> bool {
        matches!(&stmt.kind, StatementKind::Assign { place, .. }
            if place.projection.is_empty() && place.local == x)
    };
    let term_mentions = |t: &Terminator, x: Local| -> bool {
        let in_op = |op: &Operand| matches!(op, Operand::Copy(p) if p.local == x);
        match t {
            Terminator::SwitchInt { discriminant, .. } => in_op(discriminant),
            Terminator::Call {
                callee,
                args,
                destination,
                ..
            } => {
                in_op(callee)
                    || args.iter().any(in_op)
                    || (!destination.projection.is_empty() && destination.local == x)
            }
            Terminator::Assert { cond, msg, .. } => in_op(cond) || msg.operands().any(in_op),
            _ => false,
        }
    };
    let term_writes = |t: &Terminator, x: Local| -> bool {
        matches!(t, Terminator::Call { destination, .. }
            if destination.projection.is_empty() && destination.local == x)
    };

    // Collect the hoists: (target block, insert-after stmt index or
    // None for "after terminator-mention is unsupported"), the local.
    struct Hoist {
        at_block: usize,
        after_stmt: usize,
        local: Local,
    }
    // A borrowed local is pinned: the reference names its slot and outlives
    // the statement that took it, so the local's last direct mention is not
    // where its value stops being read. The same rule
    // [`insert_early_releases`] applies for the same reason.
    let mut borrowed: Vec<bool> = vec![false; n_locals];
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign {
                rvalue: Rvalue::Ref { place, .. },
                ..
            } = &stmt.kind
                && (place.local.0 as usize) < n_locals
            {
                borrowed[place.local.0 as usize] = true;
            }
        }
    }
    let mut hoists: Vec<Hoist> = Vec::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        for si in 0..block.stmts.len().saturating_sub(1) {
            // Pattern: release(x) immediately followed by x = Copy(_).
            let StatementKind::Assign {
                rvalue: Rvalue::CallIntrinsic { name, args },
                ..
            } = &block.stmts[si].kind
            else {
                continue;
            };
            if *name != "gos_rt_rc_release" {
                continue;
            }
            let Some(Operand::Copy(xp)) = args.first() else {
                continue;
            };
            if !xp.projection.is_empty() {
                continue;
            }
            let x = xp.local;
            if !is_rc(x) || borrowed[x.0 as usize] {
                continue;
            }
            let reassign = matches!(&block.stmts[si + 1].kind,
                StatementKind::Assign { place, rvalue }
                    if place.projection.is_empty()
                        && place.local == x
                        && matches!(rvalue, Rvalue::Use(Operand::Copy(_))));
            if !reassign {
                continue;
            }
            // Walk backward to x's last mention, through unique-pred
            // edges, without crossing a write to x or another release
            // of x (an existing earlier release means this one is
            // already a backstop).
            let mut cur = bi;
            let mut start = si; // exclusive upper bound within cur
            let mut found: Option<(usize, usize)> = None;
            let mut steps = 0;
            'walk: loop {
                let blk = &body.blocks[cur];
                for sj in (0..start).rev() {
                    let st = &blk.stmts[sj];
                    if let StatementKind::Assign {
                        rvalue: Rvalue::CallIntrinsic { name, args },
                        ..
                    } = &st.kind
                        && *name == "gos_rt_rc_release"
                        && matches!(args.first(), Some(Operand::Copy(p)) if p.local == x)
                    {
                        // Already released earlier on this path.
                        break 'walk;
                    }
                    if stmt_writes(st, x) {
                        break 'walk;
                    }
                    if stmt_mentions(st, x) {
                        found = Some((cur, sj));
                        break 'walk;
                    }
                }
                steps += 1;
                if steps > 64 {
                    break;
                }
                // At a join (e.g. a loop head: entry edge + back edge),
                // follow the back edge - the highest-numbered
                // predecessor, i.e. the loop body's bottom. This is
                // sound because the original release stays in place as
                // a null-safe backstop: paths that bypass the hoisted
                // release (the loop-entry edge) release the old value
                // exactly where they always did, and every block on
                // the walked segment has been verified mention-free in
                // full, so no path through it can read the nulled
                // local.
                let Some(&p) = preds[cur].iter().max() else {
                    break;
                };
                if p == cur {
                    break;
                }
                let pterm = &body.blocks[p].terminator;
                if term_writes(pterm, x) {
                    break;
                }
                if term_mentions(pterm, x) {
                    // Terminator-position mention (e.g. a call arg):
                    // inserting after a terminator means a successor
                    // head, and `cur`'s head IS that point - but only
                    // when the mention is the unique pred's terminator
                    // and x is not its destination. Insert at the head
                    // of `cur`.
                    found = Some((cur, usize::MAX));
                    break;
                }
                cur = p;
                start = body.blocks[p].stmts.len();
            }
            let Some((mb, ms)) = found else {
                continue;
            };
            // Hoisting to the immediate predecessor position of the
            // original release is a no-op; skip. (`usize::MAX` is the
            // head-of-block sentinel for terminator mentions - always
            // a real hoist, and `+ 1` on it would overflow.)
            if mb == bi && ms != usize::MAX && ms + 1 >= si {
                continue;
            }
            // Forward-liveness guard. The hoisted release NULLS `x`, so it
            // is only sound when `x` is dead from the insertion point until
            // its next write on EVERY path - not just the single back-edge
            // path the walk above verified. With a branch inside the loop
            // body (e.g. a group-match `for` loop that reads the key in one
            // arm and pushes it in another), `x` is read again past the
            // chosen mention; nulling it there frees a still-live value.
            // Walk forward from the insertion point; skip the hoist if any
            // path reads `x` before rewriting it.
            let start_stmt = if ms == usize::MAX { 0 } else { ms + 1 };
            let mut live = false;
            {
                let mut stack: Vec<(usize, usize)> = vec![(mb, start_stmt)];
                let mut visited_from0 = vec![false; n_blocks];
                'fwd: while let Some((b, from)) = stack.pop() {
                    let blk = &body.blocks[b];
                    let mut killed = false;
                    for sj in from..blk.stmts.len() {
                        let st = &blk.stmts[sj];
                        if stmt_mentions(st, x) {
                            live = true;
                            break 'fwd;
                        }
                        if stmt_writes(st, x) {
                            killed = true;
                            break;
                        }
                    }
                    if killed {
                        continue;
                    }
                    if term_mentions(&blk.terminator, x) {
                        live = true;
                        break 'fwd;
                    }
                    // A terminator call whose destination is `x` reissues it
                    // on return: the old value is dead past this block.
                    if term_writes(&blk.terminator, x) {
                        continue;
                    }
                    for &s in &succs[b] {
                        if !visited_from0[s] {
                            visited_from0[s] = true;
                            stack.push((s, 0));
                        }
                    }
                }
            }
            if live {
                continue;
            }
            hoists.push(Hoist {
                at_block: mb,
                after_stmt: ms,
                local: x,
            });
        }
    }
    if hoists.is_empty() {
        return;
    }

    let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
    let mut next_local = body.locals.len();
    // Descending insertion order keeps earlier indices valid.
    hoists.sort_by_key(|h| std::cmp::Reverse((h.at_block, h.after_stmt)));
    for h in hoists {
        let span = body.blocks[h.at_block].span;
        let rel_dest = Local(u32::try_from(next_local).expect("local overflow"));
        next_local += 1;
        let release = Statement {
            kind: StatementKind::Assign {
                place: Place::local(rel_dest),
                rvalue: Rvalue::CallIntrinsic {
                    name: "gos_rt_rc_release",
                    args: vec![Operand::Copy(Place::local(h.local))],
                },
            },
            span,
            inlined: None,
        };
        let null_out = Statement {
            kind: StatementKind::Assign {
                place: Place::local(h.local),
                rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            },
            span,
            inlined: None,
        };
        let at = if h.after_stmt == usize::MAX {
            0
        } else {
            h.after_stmt + 1
        };
        body.blocks[h.at_block].stmts.insert(at, null_out);
        body.blocks[h.at_block].stmts.insert(at, release);
    }
    for _ in body.locals.len()..next_local {
        body.locals.push(LocalDecl {
            ty: unit_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
    }
}

/// Names of bodies that borrow every `json::Value` parameter they take.
///
/// A `gos_rt_json_*` entry reads the tree its handle views and mints a fresh
/// handle for anything it answers, so a parameter that reaches nothing else
/// cannot leave the call inside the result. `gos_rt_json_identity` is the one
/// entry that answers its own argument, so it is not one of those reads.
///
/// The callers of `option::and_then` / `result::map` and their siblings pass
/// the payload to one of these bodies, which is what lets the carrier holding
/// it be reclaimed after the call.
pub(crate) fn collect_json_borrowing_fns(
    bodies: &[Body],
    tcx: &gossamer_types::TyCtxt,
) -> std::collections::HashSet<String> {
    use gossamer_types::TyKind;
    let mut out = std::collections::HashSet::new();
    for body in bodies {
        let arity = body.arity as usize;
        let json_params: Vec<usize> = (1..=arity)
            .filter(|&i| {
                body.locals
                    .get(i)
                    .is_some_and(|l| matches!(tcx.kind_of(l.ty), TyKind::JsonValue))
            })
            .collect();
        if json_params.is_empty() {
            continue;
        }
        let escaped = std::cell::Cell::new(false);
        let mut escapes = |p: &Place| {
            if json_params.contains(&(p.local.0 as usize)) {
                escaped.set(true);
            }
        };
        for block in &body.blocks {
            for stmt in &block.stmts {
                let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                    for_each_stmt_place(&stmt.kind, &mut escapes);
                    continue;
                };
                if json_params.contains(&(place.local.0 as usize)) {
                    escaped.set(true);
                }
                match rvalue {
                    Rvalue::CallIntrinsic { name, args } if json_entry_borrows(name) => {
                        let _ = args;
                    }
                    _ => for_each_rvalue_place(rvalue, &mut escapes),
                }
            }
            match &block.terminator {
                Terminator::Call { callee, args, .. } => {
                    let borrows = matches!(
                        callee,
                        Operand::Const(ConstValue::Str(n)) if json_entry_borrows(n)
                    );
                    if !borrows {
                        for a in args {
                            if let Operand::Copy(p) = a {
                                escapes(p);
                            }
                        }
                    }
                }
                Terminator::SwitchInt {
                    discriminant: Operand::Copy(p),
                    ..
                } => escapes(p),
                _ => {}
            }
        }
        if !escaped.get() {
            out.insert(body.name.clone());
        }
    }
    out
}

/// `true` when `name` is a json runtime entry that reads its handle argument
/// and answers something that never aliases it.
fn json_entry_borrows(name: &str) -> bool {
    name.starts_with("gos_rt_json_")
        && !matches!(
            name,
            "gos_rt_json_identity" | "gos_rt_json_free" | "gos_rt_json_free_slots"
        )
}

/// `true` when a value of `ty` can carry a `json::Value` handle.
///
/// A callee that answers one may be handing back the very handle it was
/// given, which is the one shape where a by-value argument is not a borrow.
fn ty_reaches_json_value(tcx: &gossamer_types::TyCtxt, ty: gossamer_types::Ty) -> bool {
    fn walk(
        tcx: &gossamer_types::TyCtxt,
        ty: gossamer_types::Ty,
        seen: &mut Vec<gossamer_types::Ty>,
    ) -> bool {
        use gossamer_types::TyKind;
        if seen.contains(&ty) {
            return false;
        }
        seen.push(ty);
        match tcx.kind_of(ty) {
            TyKind::JsonValue => true,
            TyKind::Ref { inner, .. } => walk(tcx, *inner, seen),
            TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. } => {
                walk(tcx, *elem, seen)
            }
            TyKind::Tuple(elems) => elems.clone().iter().any(|e| walk(tcx, *e, seen)),
            TyKind::HashMap { key, value, .. } => {
                let (key, value) = (*key, *value);
                walk(tcx, key, seen) || walk(tcx, value, seen)
            }
            TyKind::Adt { def, substs } => {
                let def = *def;
                if substs.types().iter().any(|t| walk(tcx, *t, seen)) {
                    return true;
                }
                match tcx.adt_field_tys(def, substs) {
                    Some(fields) => fields.to_vec().iter().any(|f| walk(tcx, *f, seen)),
                    None => false,
                }
            }
            _ => false,
        }
    }
    walk(tcx, ty, &mut Vec::new())
}

/// Frees provably single-owner `json::Value` handle locals.
///
/// `gos_rt_json_parse` / `gos_rt_json_get` mint one heap handle per
/// call (a `Box<GosJson>` holding an `Arc` share of the parsed tree);
/// nothing reclaimed them, so every parse in a loop leaked the whole
/// document. A local qualifies when every whole-local write is a call
/// destination or a null/zero init, and its value never escapes: it
/// may only be read as an argument to `gos_rt_json_*` runtime entries
/// (which borrow). Qualifying locals get `gos_rt_json_free` before
/// each re-initialising call and at every return. Aliased, stored,
/// returned, or user-call-passed handles keep today's (leaking)
/// behaviour - a leak is recoverable, a dangling handle is not.
pub(crate) fn insert_json_frees(
    body: &mut Body,
    tcx: &gossamer_types::TyCtxt,
    json_borrowing_fns: &std::collections::HashSet<String>,
) {
    use gossamer_types::TyKind;
    let n_locals = body.locals.len();
    let arity = body.arity as usize;
    let mut candidate = vec![false; n_locals];
    // A carrier local whose `Some` / `Ok` payload is a handle owns that
    // handle: `json::get(v, k)` mints one per call and the arm is the only
    // thing naming it, so the give-back is the carrier's rather than a
    // separate local's.
    let mut is_carrier = vec![false; n_locals];
    let mut any = false;
    for i in (arity + 1)..n_locals {
        if body.locals[i].region {
            continue;
        }
        match tcx.kind_of(body.locals[i].ty) {
            TyKind::JsonValue => {
                candidate[i] = true;
                any = true;
            }
            TyKind::Adt { def, substs }
                if (def.local == u32::MAX || def.local == u32::MAX - 1)
                    && substs
                        .types()
                        .first()
                        .is_some_and(|p| matches!(tcx.kind_of(*p), TyKind::JsonValue)) =>
            {
                candidate[i] = true;
                is_carrier[i] = true;
                any = true;
            }
            _ => {}
        }
    }
    if !any {
        return;
    }
    let is_json_rt = |name: &str| name.starts_with("gos_rt_json_");
    // Entries that read a carrier's arm and hand nothing of its payload out.
    let carrier_query = |name: &str| {
        matches!(
            name,
            "gos_rt_result_is_ok" | "gos_rt_result_is_err" | "gos_rt_result_disc"
        )
    };
    // Combinators that hand the payload to a closure, as (carrier, env)
    // argument positions. `filter` is deliberately absent: it answers the very
    // payload it was given.
    let combinator_slots = |name: &str| -> Option<(usize, usize)> {
        match name {
            "gos_rt_option_and_then" | "gos_rt_result_and_then" | "gos_rt_result_map" => {
                Some((0, 1))
            }
            "gos_rt_option_map_i64" => Some((1, 0)),
            _ => None,
        }
    };
    // The closure each env local carries, from the `gos_fn_addr` the lowering
    // stores at offset 8. An env written with more than one closure answers
    // `None`, so a reused env is judged as unknown rather than as the last
    // name written into it.
    let closure_of_env = {
        let mut fn_addr: std::collections::HashMap<u32, &str> = std::collections::HashMap::new();
        let mut const_int: std::collections::HashMap<u32, i128> = std::collections::HashMap::new();
        for block in &body.blocks {
            for stmt in &block.stmts {
                let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                    continue;
                };
                match rvalue {
                    Rvalue::CallIntrinsic { name, args } if *name == "gos_fn_addr" => {
                        if let Some(Operand::Const(ConstValue::Str(n))) = args.first() {
                            fn_addr.insert(place.local.0, n.as_str());
                        }
                    }
                    Rvalue::Use(Operand::Const(ConstValue::Int(n))) => {
                        const_int.insert(place.local.0, *n);
                    }
                    _ => {}
                }
            }
        }
        // The callable slot the closure lowering writes: offset 8 of the env
        // block, spelled either as a literal or as a local holding it.
        let is_callable_slot = |op: &Operand| match op {
            Operand::Const(ConstValue::Int(n)) => *n == 8,
            Operand::Copy(p) => const_int.get(&p.local.0) == Some(&8),
            _ => false,
        };
        let mut env: std::collections::HashMap<u32, Option<&str>> =
            std::collections::HashMap::new();
        for block in &body.blocks {
            for stmt in &block.stmts {
                let StatementKind::Assign { rvalue, .. } = &stmt.kind else {
                    continue;
                };
                let Rvalue::CallIntrinsic { name, args } = rvalue else {
                    continue;
                };
                if *name != "gos_store" {
                    continue;
                }
                let [Operand::Copy(target), offset, Operand::Copy(value)] = args.as_slice() else {
                    continue;
                };
                if !is_callable_slot(offset) {
                    continue;
                }
                let stored = fn_addr.get(&value.local.0).copied();
                env.entry(target.local.0)
                    .and_modify(|slot| {
                        if *slot != stored {
                            *slot = None;
                        }
                    })
                    .or_insert(stored);
            }
        }
        env
    };
    let combinator_borrows = |name: &str, args: &[Operand], local: u32| -> bool {
        let Some((carrier_idx, env_idx)) = combinator_slots(name) else {
            return false;
        };
        if !matches!(args.get(carrier_idx), Some(Operand::Copy(p)) if p.local.0 == local) {
            return false;
        }
        let Some(Operand::Copy(env)) = args.get(env_idx) else {
            return false;
        };
        closure_of_env
            .get(&env.local.0)
            .copied()
            .flatten()
            .is_some_and(|n| json_borrowing_fns.contains(n))
    };
    // A handle read out of a container is the container's, not the frame's:
    // the container hands back the slot's word and reclaims it at its own
    // death, so freeing it here would give the same handle back twice.
    let borrows_from_container = |name: &str| {
        name.starts_with("gos_rt_vec_get")
            || name.starts_with("gos_rt_iter")
            || name.starts_with("gos_rt_deque_get")
            || name.starts_with("gos_rt_map_get")
    };
    // Whole-local handle moves (`v = Copy(tmp)` with both sides
    // JSON-typed): ownership transfers when the move is the source's
    // ONLY value read and its only such move - the destination owns
    // the handle, the source is never freed. Pre-scan to identify
    // them so the escape check below can treat the move as allowed.
    // Locals that own a handle, directly or through a carrier's arm. A move
    // hands ownership on within one class; the two are never interchangeable,
    // since one names the handle and the other names the arm holding it.
    let jv: Vec<bool> = (0..n_locals)
        .map(|i| matches!(tcx.kind_of(body.locals[i].ty), TyKind::JsonValue) || is_carrier[i])
        .collect();
    let mut value_reads = vec![0usize; n_locals];
    let mut move_edges: Vec<(usize, usize, usize, usize)> = Vec::new(); // (src, dest, bi, si)
    for (bi, block) in body.blocks.iter().enumerate() {
        let mut count_op = |op: &Operand| {
            if let Operand::Copy(p) = op
                && p.projection.is_empty()
                && (p.local.0 as usize) < n_locals
            {
                value_reads[p.local.0 as usize] += 1;
            }
        };
        for (si, stmt) in block.stmts.iter().enumerate() {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            match rvalue {
                Rvalue::Use(Operand::Copy(src)) => {
                    if place.projection.is_empty()
                        && src.projection.is_empty()
                        && (place.local.0 as usize) < n_locals
                        && (src.local.0 as usize) < n_locals
                        && jv[place.local.0 as usize]
                        && jv[src.local.0 as usize]
                        && is_carrier[place.local.0 as usize] == is_carrier[src.local.0 as usize]
                    {
                        move_edges.push((src.local.0 as usize, place.local.0 as usize, bi, si));
                    } else {
                        count_op(&Operand::Copy(src.clone()));
                    }
                }
                Rvalue::CallIntrinsic { name, args } if is_json_rt(name) => {
                    // Borrowing json-runtime args are not value reads.
                    let _ = args;
                }
                Rvalue::CallIntrinsic { args, .. } => {
                    for a in args {
                        count_op(a);
                    }
                }
                Rvalue::BinaryOp { lhs, rhs, .. } => {
                    count_op(lhs);
                    count_op(rhs);
                }
                Rvalue::UnaryOp { operand, .. } | Rvalue::Cast { operand, .. } => count_op(operand),
                Rvalue::Aggregate { operands, .. } => {
                    for a in operands {
                        count_op(a);
                    }
                }
                Rvalue::Repeat { value, .. } => count_op(value),
                Rvalue::Ref { .. } | Rvalue::Len(_) => {}
                Rvalue::Use(_) => {}
                Rvalue::StaticLoad(_) => {}
            }
        }
        if let Terminator::Call { callee, args, .. } = &block.terminator {
            let allowed = matches!(callee, Operand::Const(ConstValue::Str(n)) if is_json_rt(n));
            if !allowed {
                for a in args {
                    count_op(a);
                }
            }
        }
    }
    // A source moves cleanly when it has exactly one outgoing move and
    // no other value reads.
    let mut moved_from = vec![false; n_locals];
    let mut move_inits: Vec<(usize, usize, usize)> = Vec::new(); // (dest, bi, si)
    {
        let mut out_moves = vec![0usize; n_locals];
        for &(src, _, _, _) in &move_edges {
            out_moves[src] += 1;
        }
        for &(src, dest, bi, si) in &move_edges {
            if out_moves[src] == 1 && value_reads[src] == 0 {
                moved_from[src] = true;
                move_inits.push((dest, bi, si));
            }
        }
    }
    // A read withdraws the local unless the site allows its class: a handle
    // and a carrier reach different entry points, so each has its own verdict.
    fn check_op(
        op: &Operand,
        allowed: bool,
        allowed_carrier: bool,
        carrier: &[bool],
        c: &mut [bool],
    ) {
        if let Operand::Copy(p) = op
            && (p.local.0 as usize) < c.len()
            && c[p.local.0 as usize]
        {
            let ok = if carrier[p.local.0 as usize] {
                allowed_carrier
            } else {
                allowed
            };
            if !ok {
                c[p.local.0 as usize] = false;
            }
        }
    }
    // Init sites per local: (block, stmt-or-terminator marker).
    let mut init_sites: Vec<Vec<(usize, usize)>> = vec![Vec::new(); n_locals];
    for (bi, block) in body.blocks.iter().enumerate() {
        for (si, stmt) in block.stmts.iter().enumerate() {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            // Reads: any appearance as a Copy operand outside a
            // json-runtime call argument escapes the handle.
            match rvalue {
                Rvalue::CallIntrinsic { name, args } => {
                    let allowed = is_json_rt(name);
                    let queries = carrier_query(name);
                    for a in args {
                        let carrier_ok = queries
                            || matches!(a, Operand::Copy(p) if combinator_borrows(name, args, p.local.0));
                        check_op(a, allowed, carrier_ok, &is_carrier, &mut candidate);
                    }
                }
                Rvalue::Use(op) => {
                    let clean_move = matches!(
                        op,
                        Operand::Copy(p)
                            if p.projection.is_empty()
                                && (p.local.0 as usize) < n_locals
                                && moved_from[p.local.0 as usize]
                                && place.projection.is_empty()
                                && (place.local.0 as usize) < n_locals
                                && jv[place.local.0 as usize]
                    );
                    check_op(op, clean_move, clean_move, &is_carrier, &mut candidate);
                }
                Rvalue::BinaryOp { lhs, rhs, .. } => {
                    check_op(lhs, false, false, &is_carrier, &mut candidate);
                    check_op(rhs, false, false, &is_carrier, &mut candidate);
                }
                Rvalue::UnaryOp { operand, .. } | Rvalue::Cast { operand, .. } => {
                    check_op(operand, false, false, &is_carrier, &mut candidate);
                }
                Rvalue::Aggregate { operands, .. } => {
                    for a in operands {
                        check_op(a, false, false, &is_carrier, &mut candidate);
                    }
                }
                Rvalue::Repeat { value, .. } => {
                    check_op(value, false, false, &is_carrier, &mut candidate);
                }
                Rvalue::Ref { place: rp, .. } => {
                    if candidate.get(rp.local.0 as usize).copied().unwrap_or(false) {
                        candidate[rp.local.0 as usize] = false;
                    }
                }
                Rvalue::Len(_) => {}
                Rvalue::StaticLoad(_) => {}
            }
            // Writes to the candidate itself.
            if place.projection.is_empty() && (place.local.0 as usize) < n_locals {
                let i = place.local.0 as usize;
                if candidate[i] {
                    match rvalue {
                        Rvalue::CallIntrinsic { name, .. } if borrows_from_container(name) => {
                            candidate[i] = false;
                        }
                        Rvalue::CallIntrinsic { .. } => init_sites[i].push((bi, si)),
                        Rvalue::Use(Operand::Const(ConstValue::Int(_))) => {}
                        // A move carries the source's ownership, so it hands
                        // over a free only when the source had one to give.
                        Rvalue::Use(Operand::Copy(src))
                            if src.projection.is_empty()
                                && (src.local.0 as usize) < n_locals
                                && moved_from[src.local.0 as usize] =>
                        {
                            if candidate[src.local.0 as usize] {
                                init_sites[i].push((bi, si));
                            } else {
                                candidate[i] = false;
                            }
                        }
                        _ => candidate[i] = false,
                    }
                }
            } else if !place.projection.is_empty()
                && (place.local.0 as usize) < n_locals
                && candidate[place.local.0 as usize]
            {
                candidate[place.local.0 as usize] = false;
            }
        }
        match &block.terminator {
            Terminator::Call {
                callee,
                args,
                destination,
                ..
            } => {
                let callee_name = match callee {
                    Operand::Const(ConstValue::Str(n)) => Some(n.as_str()),
                    _ => None,
                };
                // A by-value argument to a named user function is a borrow:
                // the callee cannot outlive the call, and a container it
                // stores the handle in takes a handle of its own. The shape
                // that would alias is a callee whose answer can carry the
                // handle back out, so a destination type reaching a
                // `json::Value` leaves the frame's handle disowned.
                let user_call_borrows = matches!(callee, Operand::FnRef { .. })
                    && (destination.local.0 as usize) < n_locals
                    && !ty_reaches_json_value(tcx, body.locals[destination.local.0 as usize].ty);
                let allowed = callee_name.is_some_and(is_json_rt) || user_call_borrows;
                let queries = callee_name.is_some_and(carrier_query);
                for a in args {
                    if let Operand::Copy(p) = a
                        && (p.local.0 as usize) < n_locals
                        && candidate[p.local.0 as usize]
                    {
                        let ok = if is_carrier[p.local.0 as usize] {
                            queries
                                || callee_name
                                    .is_some_and(|n| combinator_borrows(n, args, p.local.0))
                        } else {
                            allowed
                        };
                        if !ok {
                            candidate[p.local.0 as usize] = false;
                        }
                    }
                }
                if destination.projection.is_empty()
                    && (destination.local.0 as usize) < n_locals
                    && candidate[destination.local.0 as usize]
                {
                    if callee_name.is_some_and(borrows_from_container) {
                        candidate[destination.local.0 as usize] = false;
                    } else {
                        init_sites[destination.local.0 as usize].push((bi, usize::MAX));
                    }
                }
            }
            Terminator::SwitchInt { discriminant, .. } => {
                if let Operand::Copy(p) = discriminant
                    && (p.local.0 as usize) < n_locals
                    && candidate[p.local.0 as usize]
                {
                    candidate[p.local.0 as usize] = false;
                }
            }
            _ => {}
        }
    }
    let qualified: Vec<usize> = (0..n_locals)
        .filter(|&i| candidate[i] && !moved_from[i])
        .collect();
    if qualified.is_empty() {
        return;
    }
    let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
    let mut next_local = body.locals.len();
    let free_stmt = |l: usize, span: gossamer_lex::Span, next: &mut usize| -> Statement {
        let dest = Local(u32::try_from(*next).expect("local overflow"));
        *next += 1;
        let operand = Operand::Copy(Place::local(Local(u32::try_from(l).unwrap_or(0))));
        // A carrier gives back the handle its `Some` / `Ok` arm names; the
        // other arm's payload word belongs to the error value. Kind 3 is the
        // `json::Value` payload.
        let rvalue = if is_carrier[l] {
            Rvalue::CallIntrinsic {
                name: "gos_rt_result_ok_payload_release",
                args: vec![operand, Operand::Const(ConstValue::Int(3))],
            }
        } else {
            Rvalue::CallIntrinsic {
                name: "gos_rt_json_free",
                args: vec![operand],
            }
        };
        Statement {
            kind: StatementKind::Assign {
                place: Place::local(dest),
                rvalue,
            },
            span,
            inlined: None,
        }
    };
    // Per-block gap lists: stmt-index -> stmts to insert before it,
    // plus an end-of-block list for Return frees.
    let nb = body.blocks.len();
    let mut pre_gaps: Vec<Vec<(usize, Statement)>> = vec![Vec::new(); nb];
    let mut end_gaps: Vec<Vec<Statement>> = vec![Vec::new(); nb];
    for &l in &qualified {
        // Free the previous value before each re-initialising call
        // (first execution frees the zero init, which is null-safe).
        for &(bi, si) in &init_sites[l] {
            let span = body.blocks[bi].span;
            if si == usize::MAX {
                end_gaps[bi].push(free_stmt(l, span, &mut next_local));
            } else {
                pre_gaps[bi].push((si, free_stmt(l, span, &mut next_local)));
            }
        }
    }
    for (bi, block) in body.blocks.iter().enumerate() {
        if matches!(block.terminator, Terminator::Return) {
            let span = block.span;
            for &l in &qualified {
                end_gaps[bi].push(free_stmt(l, span, &mut next_local));
            }
        }
    }
    for bi in (0..nb).rev() {
        pre_gaps[bi].sort_by_key(|(si, _)| std::cmp::Reverse(*si));
        let drained: Vec<(usize, Statement)> = std::mem::take(&mut pre_gaps[bi]);
        for (si, stmt) in drained {
            body.blocks[bi].stmts.insert(si, stmt);
        }
        for stmt in std::mem::take(&mut end_gaps[bi]) {
            body.blocks[bi].stmts.push(stmt);
        }
    }
    // The pre-init frees below read the local's previous value; only
    // some locals get the MIR zero-init, so make it explicit for every
    // qualified local (free of null is a no-op).
    if !body.blocks.is_empty() {
        let span = body.blocks[0].span;
        for (k, &l) in qualified.iter().enumerate() {
            body.blocks[0].stmts.insert(
                k,
                Statement {
                    kind: StatementKind::Assign {
                        place: Place::local(Local(u32::try_from(l).unwrap_or(0))),
                        rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                    },
                    span,
                    inlined: None,
                },
            );
        }
    }
    for _ in body.locals.len()..next_local {
        body.locals.push(LocalDecl {
            ty: unit_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
    }
}

/// A vector helper that reads or writes through the vector it is handed
/// without taking a share of it or keeping a pointer to it.
///
/// Such a call stands beside the value rather than between it and the local a
/// later move hands it to, so it does not make the vector's ownership
/// ambiguous.
fn borrows_vec_receiver(name: &str) -> bool {
    if !name.starts_with("gos_rt_vec_") {
        return matches!(name, "gos_rt_len");
    }
    !matches!(
        name,
        "gos_rt_vec_free"
            | "gos_rt_vec_retain"
            | "gos_rt_vec_mark_shared"
            | "gos_rt_vec_assign"
            | "gos_rt_vec_clone"
            | "gos_rt_vec_set_slot_children"
            | "gos_rt_vec_set_elem_meta"
    )
}

/// Gives back the share a frame minted for a holder when the binding that
/// minted it is rebound.
///
/// A store into a heap object takes a share of the value it writes, and the
/// frame keeps its own. Rebinding the frame's name to the object that now
/// holds the value leaves that share with no name, so it is returned here.
pub(crate) fn release_rebound_rc_locals(body: &mut Body, tcx: &gossamer_types::TyCtxt) {
    let n_locals = body.locals.len();
    let arity = body.arity as usize;
    if n_locals == 0 {
        return;
    }
    let is_rc = |l: Local| -> bool {
        let i = l.0 as usize;
        i > arity && i < n_locals && tcx.is_rc_managed(body.locals[i].ty) && !body.locals[i].region
    };
    let bare_arg = |args: &[Operand]| -> Option<Local> {
        match args.first() {
            Some(Operand::Copy(p)) if p.projection.is_empty() => Some(p.local),
            _ => None,
        }
    };
    let mut sites: Vec<(usize, usize, Local)> = Vec::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        // Locals this block minted a holder's share for, still unbalanced.
        let mut minted: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
        for (si, stmt) in block.stmts.iter().enumerate() {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            if let Rvalue::CallIntrinsic { name, args } = rvalue {
                match *name {
                    "gos_rt_rc_retain" => {
                        if let Some(l) = bare_arg(args)
                            && is_rc(l)
                        {
                            minted.insert(l.0);
                        }
                        continue;
                    }
                    "gos_rt_rc_release" => {
                        if let Some(l) = bare_arg(args) {
                            minted.remove(&l.0);
                        }
                        continue;
                    }
                    _ => {}
                }
            }
            if !place.projection.is_empty() || !minted.contains(&place.local.0) {
                continue;
            }
            // A rebinding that names another value, not one derived from the
            // local itself.
            let rebinds = match rvalue {
                Rvalue::Use(Operand::Copy(src)) => {
                    src.projection.is_empty() && src.local != place.local
                }
                _ => false,
            };
            if rebinds {
                sites.push((bi, si, place.local));
            }
            minted.remove(&place.local.0);
        }
    }
    if sites.is_empty() {
        return;
    }
    let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
    for (bi, si, local) in sites.into_iter().rev() {
        let span = body.blocks[bi].stmts[si].span;
        let dest = Local(u32::try_from(body.locals.len()).expect("local overflow"));
        body.locals.push(LocalDecl {
            ty: unit_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
        body.blocks[bi]
            .stmts
            .insert(si, rc_call_stmt("gos_rt_rc_release", dest, local, span));
    }
}
