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
    /// An `Iterator` field: a share of the lazy handle, which every holder
    /// advances as one cursor (`gos_rt_lazy_iter_retain_i64` /
    /// `gos_rt_lazy_iter_drop_i64`).
    Iter,
    /// An `Iterator` field on the pair state `zip` and `enumerate` build.
    IterPair,
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
                Self::Iter => ("gos_rt_lazy_iter_retain_i64", "gos_rt_lazy_iter_drop_i64"),
                Self::IterPair => (
                    "gos_rt_lazy_iter_retain_pair_i64",
                    "gos_rt_lazy_iter_drop_pair_i64",
                ),
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
            Self::Rc
            | Self::Weak
            | Self::Vec
            | Self::Carrier { .. }
            | Self::Iter
            | Self::IterPair => None,
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
            | "gos_rt_str_append_u64"
            | "gos_rt_str_append_bool"
            | "gos_rt_str_append_f64"
            | "gos_rt_str_append_bytes"
            | "gos_rt_str_push_substring"
            | "gos_rt_str_push_char"
            | "gos_rt_str_push_byte"
            | "gos_rt_str_clear"
            | "gos_rt_str_truncate"
            // Appends through `gos_rt_str_append_bytes` and answers the
            // accumulator in the carrier's payload.
            | "gos_rt_str_push_utf8"
            | "gos_rt_str_push_json_quoted"
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
    // An iterator handle is minted a share by the aggregate exactly as a
    // sequence is.
    let container = |l: Local| -> bool {
        body.locals.get(l.0 as usize).is_some_and(|decl| {
            !decl.region
                && matches!(
                    tcx.kind_of(decl.ty),
                    TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Iterator(_)
                )
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
    let mut minted: Vec<bool> = (0..n)
        .map(|i| {
            container(Local(u32::try_from(i).unwrap_or(0)))
                && !escapes_elsewhere[i]
                && !carriers[i].is_empty()
        })
        .collect();
    // A bare copy names the value its source built, so an aggregate minting a
    // share of the copy leaves the source's own share with the frame too.
    loop {
        let mut changed = false;
        for &(copy, source) in &copy_aliases {
            let (c, s) = (copy.0 as usize, source.0 as usize);
            if minted[c] && !minted[s] && container(source) {
                minted[s] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    minted
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
        } else if let TyKind::Iterator(item) = tcx.kind_of(t) {
            Some(if lazy_iter_is_pair_state(tcx, *item) {
                FieldRcKind::IterPair
            } else {
                FieldRcKind::Iter
            })
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

/// Carrier locals that view a slot a container still owns: the answer of a
/// call that hands back one of the sequence's own elements (`xs.first()`,
/// `xs.max_by_key(f)`), a carrier read in place out of a fixed array or an
/// aggregate field, and every plain copy of one. Their payload is the
/// container's, not a share the frame holds.
fn borrowed_carriers(body: &Body) -> Vec<bool> {
    let n_locals = body.locals.len();
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
    borrowed_enum_src
}

/// Takes the frame's share of the payload a fallback accessor answers from a
/// carrier that views a container's element.
///
/// `unwrap_or` on a carrier answers the `Some` payload or a retained fallback,
/// and the binding that takes the answer releases it. A carrier the frame
/// owns hands its payload's share over with it; a carrier that views an
/// element does not, so its payload takes a share before the call and both
/// arms answer one owned share.
pub(crate) fn retain_borrowed_fallback_payloads(body: &mut Body, tcx: &gossamer_types::TyCtxt) {
    let borrowed = borrowed_carriers(body);
    let sites: Vec<(usize, Local, i64)> = body
        .blocks
        .iter()
        .enumerate()
        .filter_map(|(block_index, block)| {
            let Terminator::Call {
                callee: Operand::Const(ConstValue::Str(name)),
                args,
                ..
            } = &block.terminator
            else {
                return None;
            };
            let kind = match name.as_str() {
                "gos_rt_result_unwrap_or_str" => 1,
                "gos_rt_result_unwrap_or_vec" => 2,
                "gos_rt_result_unwrap_or_node" => 4,
                _ => return None,
            };
            match args.first() {
                Some(Operand::Copy(carrier))
                    if carrier.projection.is_empty()
                        && borrowed
                            .get(carrier.local.0 as usize)
                            .copied()
                            .unwrap_or(false) =>
                {
                    Some((block_index, carrier.local, kind))
                }
                _ => None,
            }
        })
        .collect();
    let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
    for (block_index, carrier, kind) in sites {
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
                        Operand::Copy(Place::local(carrier)),
                        Operand::Const(ConstValue::Int(i128::from(kind))),
                        Operand::Const(ConstValue::Int(0)),
                    ],
                },
            },
            span,
            inlined: None,
        });
    }
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
    let iter_operand = |op: &Operand| -> Option<Local> {
        if let Operand::Copy(p) = op
            && p.projection.is_empty()
            && (p.local.0 as usize) < n_locals
            && matches!(
                tcx.kind_of(body.locals[p.local.0 as usize].ty),
                gossamer_types::TyKind::Iterator(_)
            )
            && !body.locals[p.local.0 as usize].region
        {
            Some(p.local)
        } else {
            None
        }
    };
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
    let mut borrowed_enum_src = borrowed_carriers(body);
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
                // An iterator operand is a handle whose holders share one
                // cursor: the slot takes a share, and the frame keeps the
                // release of the one it built.
                Rvalue::Aggregate { operands, .. } => {
                    for op in operands {
                        if let Some(l) = rc_operand(op)
                            .or_else(|| vec_operand(op))
                            .or_else(|| iter_operand(op))
                        {
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
                                // An iterator handle is counted outside the RC
                                // header the same way a sequence is.
                                if matches!(
                                    tcx.kind_of(body.locals[i].ty),
                                    gossamer_types::TyKind::Vec(_)
                                        | gossamer_types::TyKind::Slice(_)
                                        | gossamer_types::TyKind::Iterator(_)
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
            args,
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
            // An `unwrap` hands over the payload's share only when the frame
            // owns the carrier; on a carrier that views a container's element
            // (`xs.first()`, `xs.max_by_key(f)`) the payload stays the
            // container's, and a binding copied from it takes its own share.
            let owns_return = match callee {
                Operand::FnRef { .. } => true,
                Operand::Const(ConstValue::Str(name)) => {
                    (!name.starts_with("gos_rt_") && name != "gos_load" && name != "gos_store")
                        || name == "gos_rt_rc_downgrade"
                        || (mints_owned_string(name)
                            && !(matches!(
                                name.as_str(),
                                "gos_rt_option_unwrap" | "gos_rt_result_unwrap"
                            ) && enum_arg_is_borrowed(args)))
                        || mints_owned_error(name)
                        || mints_owned_node(name)
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
    // A copy that is the source's last whole-local read is a transfer
    // instead: the reads that stand beside the buffer (element stores,
    // len, gets) take no share of it, so when nothing else releases the
    // source's own reference - it is not an RC owner, not an extraction
    // result, not itself rebound, each of which gives the value back
    // somewhere - that reference, a constructor's or a callee's answer
    // which nothing else frees, moves into the owner whole, and the
    // owner's release-before-rebinding and death are what free it.
    // `copy_is_last_use` rejects a source read after the copy or across a
    // back-edge without redefinition, and the statement-copy count rejects
    // a sibling branch handing the same reference to a second owner.
    let rebound = rebound_vec_owners(body, tcx);
    let mut stmt_copy_reads = vec![0u32; n_locals];
    {
        let bump = |op: &Operand, reads: &mut [u32]| {
            if let Operand::Copy(p) = op
                && p.projection.is_empty()
                && (p.local.0 as usize) < reads.len()
            {
                let i = p.local.0 as usize;
                reads[i] = reads[i].saturating_add(1);
            }
        };
        for block in &body.blocks {
            for stmt in &block.stmts {
                if let StatementKind::Assign { rvalue, .. } = &stmt.kind {
                    match rvalue {
                        Rvalue::Use(op)
                        | Rvalue::UnaryOp { operand: op, .. }
                        | Rvalue::Cast { operand: op, .. }
                        | Rvalue::Repeat { value: op, .. } => bump(op, &mut stmt_copy_reads),
                        Rvalue::BinaryOp { lhs, rhs, .. } => {
                            bump(lhs, &mut stmt_copy_reads);
                            bump(rhs, &mut stmt_copy_reads);
                        }
                        Rvalue::Aggregate { operands, .. } => {
                            for op in operands {
                                bump(op, &mut stmt_copy_reads);
                            }
                        }
                        Rvalue::CallIntrinsic { args, .. } => {
                            for op in args {
                                bump(op, &mut stmt_copy_reads);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
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
                let s = src.local.0 as usize;
                let transfer = s > arity
                    && !body.locals[s].region
                    && stmt_copy_reads[s] == 1
                    && !vec_field_extract[s]
                    && !rebound[s]
                    && !(is_rc(s) && owned[s])
                    && copy_is_last_use(body, (bi, si), src.local);
                if !transfer {
                    retain_sites.push((bi, si, src.local, 1));
                }
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
                    // A bare `Map` operand whose source keeps its table - a
                    // call's own answer, a parameter, or a word read out of an
                    // owned aggregate's field - still frees that table, so the
                    // field takes one of its own rather than a second owner of
                    // the same.
                    if place.projection.is_empty() {
                        for (idx, op) in operands.iter().enumerate() {
                            let Operand::Copy(src) = op else { continue };
                            if !src.projection.is_empty()
                                || (src.local.0 as usize) >= n_locals
                                || !(container_source_keeps_own(body, src.local, &call_dest)
                                    || borrows_owned_field[src.local.0 as usize])
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
                        (retain, kind @ (FieldRcKind::Iter | FieldRcKind::IterPair)) => {
                            let (share, release) = kind.helpers();
                            if retain { share } else { release }
                        }
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

/// Whether `name` answers a fresh counted runtime node - a channel, a join
/// handle, a context or its done channel, a `std::sync` handle, a byte
/// builder or buffer, a random generator, or file open options - whose one
/// share the caller holds. An open-options setter answers its receiver with a
/// share of its own.
fn mints_owned_node(name: &str) -> bool {
    matches!(
        name,
        "gos_rt_chan_new"
            | "gos_rt_fs_open_options_new"
            | "gos_rt_fs_open_options_read"
            | "gos_rt_fs_open_options_write"
            | "gos_rt_fs_open_options_append"
            | "gos_rt_fs_open_options_truncate"
            | "gos_rt_fs_open_options_create"
            | "gos_rt_fs_open_options_create_new"
            | "gos_rt_spawn"
            | "gos_rt_spawn_ex"
            | "gos_rt_ctx_cancelled"
            | "gos_rt_ctx_background"
            | "gos_rt_bytes_builder_new"
            | "gos_rt_bytes_builder_with_capacity"
            | "gos_rt_bytes_buffer_new"
            | "gos_rt_bytes_buffer_with_capacity"
            | "gos_rt_math_rng_new"
            | "gos_rt_ctx_with_cancel"
            | "gos_rt_ctx_with_timeout"
            | "gos_rt_rwlock_new"
            | "gos_rt_sync_map_new"
            | "gos_rt_shared_new"
            | "gos_rt_mutex_new"
            | "gos_rt_once_new"
            | "gos_rt_wg_new"
            | "gos_rt_barrier_new"
            | "gos_rt_atomic_i64_new"
            | "gos_rt_atomic_bool_new"
    )
}

mod answers;
mod channels;
mod copies;
mod field_releases;
mod json;
mod param_reads;
mod read_counts;
mod slots;
mod tables;

pub(crate) use answers::*;
pub(crate) use channels::*;
pub(crate) use copies::*;
pub(crate) use field_releases::*;
pub(crate) use json::*;
pub(crate) use param_reads::*;
pub(crate) use read_counts::*;
pub(crate) use slots::*;
pub(crate) use tables::*;
