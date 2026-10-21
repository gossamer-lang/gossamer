//! Values of their own for vectors stored into a new holder.

use super::*;
use crate::ir::Projection;

/// Where the value a store hands on came from, following whole-local copies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StoredOrigin {
    /// Built, answered, or cloned for this store: nothing else names it.
    Fresh,
    /// A binding of this frame, which the program may still read or write.
    Binding,
    /// Read out of a field, element, or referent of another place.
    Place,
}

/// The origin of the value `local` holds and every local on the chain of
/// whole-value copies that carried it there, nearest first. A place read
/// ends the chain at the base local of the place it was read from.
pub(super) fn stored_origin(
    body: &Body,
    tcx: &gossamer_types::TyCtxt,
    local: Local,
) -> (StoredOrigin, Vec<Local>, Option<Place>) {
    let arity = body.arity as usize;
    let mut chain = vec![local];
    let mut current = local;
    for _ in 0..body.locals.len() {
        let mut source: Option<Place> = None;
        let mut writes = 0usize;
        for stmt in body.blocks.iter().flat_map(|block| &block.stmts) {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            if place.local != current || !place.projection.is_empty() {
                continue;
            }
            if matches!(rvalue, Rvalue::Use(Operand::Const(ConstValue::Int(0)))) {
                continue;
            }
            writes += 1;
            source = match rvalue {
                Rvalue::Use(Operand::Copy(src)) => Some(src.clone()),
                _ => None,
            };
        }
        let i = current.0 as usize;
        // A reference parameter names the caller's place.
        if (1..=arity).contains(&i) && reference_ty(tcx, body.locals[i].ty) {
            return (StoredOrigin::Place, chain, Some(Place::local(current)));
        }
        let named = body.locals[i].debug_name.is_some() && i > arity;
        match (writes, source) {
            (1, Some(src)) if !src.projection.is_empty() => {
                return (StoredOrigin::Place, chain, Some(src));
            }
            (1, Some(src)) if (src.local.0 as usize) < body.locals.len() => {
                chain.push(src.local);
                current = src.local;
            }
            _ if named => return (StoredOrigin::Binding, chain, None),
            _ if writes > 1 => return (StoredOrigin::Binding, chain, None),
            _ => return (StoredOrigin::Fresh, chain, None),
        }
    }
    (StoredOrigin::Binding, chain, None)
}

/// Whether `local` is a closure environment: allocated under a closure's
/// child layout.
fn is_closure_env(body: &Body, local: Local) -> bool {
    body.blocks.iter().flat_map(|b| &b.stmts).any(|stmt| {
        matches!(&stmt.kind, StatementKind::Assign {
            place,
            rvalue: Rvalue::CallIntrinsic { name: "gos_rc_alloc", args },
        } if place.projection.is_empty()
            && place.local == local
            && matches!(args.get(1), Some(Operand::Const(ConstValue::Str(meta)))
                if meta.starts_with("gos_rc_meta_closure_")))
    })
}

fn reference_ty(tcx: &gossamer_types::TyCtxt, ty: gossamer_types::Ty) -> bool {
    matches!(tcx.kind_of(ty), gossamer_types::TyKind::Ref { .. })
}

/// Whether `local` is mentioned anywhere other than by a reference-count
/// helper.
fn operand_mentions(op: &Operand, local: Local) -> bool {
    matches!(op, Operand::Copy(p) if p.local == local
        || p.projection.iter().any(|step| matches!(step, Projection::Index(l) if *l == local)))
}

fn is_count_helper(name: &str) -> bool {
    name.ends_with("_retain")
        || name.ends_with("_release")
        || name.ends_with("_free")
        || name.contains("_retain_")
        || name.contains("_release_")
        || name.contains("_free_")
}

fn stmt_reads(stmt: &Statement, local: Local) -> bool {
    let StatementKind::Assign { place, rvalue } = &stmt.kind else {
        return false;
    };
    if place.local == local && !place.projection.is_empty() {
        return true;
    }
    if place
        .projection
        .iter()
        .any(|step| matches!(step, Projection::Index(l) if *l == local))
    {
        return true;
    }
    let m = |op: &Operand| operand_mentions(op, local);
    match rvalue {
        Rvalue::CallIntrinsic { name, args } => !is_count_helper(name) && args.iter().any(m),
        Rvalue::Use(op)
        | Rvalue::UnaryOp { operand: op, .. }
        | Rvalue::Cast { operand: op, .. } => m(op),
        Rvalue::BinaryOp { lhs, rhs, .. } => m(lhs) || m(rhs),
        Rvalue::Aggregate { operands, .. } => operands.iter().any(m),
        Rvalue::Repeat { value, .. } => m(value),
        Rvalue::Ref { place: p, .. } | Rvalue::Len(p) => p.local == local,
        Rvalue::StaticLoad(_) => false,
    }
}

fn stmt_redefines(stmt: &Statement, local: Local) -> bool {
    matches!(&stmt.kind, StatementKind::Assign { place, rvalue }
        if place.local == local
            && place.projection.is_empty()
            && !matches!(rvalue, Rvalue::Use(Operand::Const(ConstValue::Int(0)))))
}

fn terminator_reads(terminator: &Terminator, local: Local) -> bool {
    let m = |op: &Operand| operand_mentions(op, local);
    match terminator {
        Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            args,
            ..
        } => !is_count_helper(name) && args.iter().any(m),
        Terminator::Call { args, callee, .. } => args.iter().any(m) || m(callee),
        Terminator::SwitchInt { discriminant, .. } => m(discriminant),
        _ => false,
    }
}

fn terminator_redefines(terminator: &Terminator, local: Local) -> bool {
    matches!(terminator, Terminator::Call { destination, .. }
        if destination.local == local && destination.projection.is_empty())
}

/// Whether some path from just past statement `stmt` of `block` reads
/// `local` before giving it a whole new value. A loop back into `block`
/// rescans it from its first statement.
pub(super) fn read_after(body: &Body, local: Local, block: usize, stmt: usize) -> bool {
    let scan = |bi: usize, from: usize| -> Option<bool> {
        let b = &body.blocks[bi];
        for s in b.stmts.iter().skip(from) {
            if stmt_reads(s, local) {
                return Some(true);
            }
            if stmt_redefines(s, local) {
                return Some(false);
            }
        }
        if terminator_reads(&b.terminator, local) {
            return Some(true);
        }
        if terminator_redefines(&b.terminator, local) {
            return Some(false);
        }
        None
    };
    // A store the terminator makes is followed by the block's successors
    // only; the terminator itself is the store.
    if stmt < body.blocks[block].stmts.len()
        && let Some(read) = scan(block, stmt + 1)
    {
        return read;
    }
    let mut seen = vec![false; body.blocks.len()];
    let mut queue: Vec<usize> = block_successors(&body.blocks[block].terminator)
        .into_iter()
        .map(|b| b.as_u32() as usize)
        .collect();
    while let Some(bi) = queue.pop() {
        if bi >= seen.len() || std::mem::replace(&mut seen[bi], true) {
            continue;
        }
        match scan(bi, 0) {
            Some(true) => return true,
            Some(false) => {}
            None => queue.extend(
                block_successors(&body.blocks[bi].terminator)
                    .into_iter()
                    .map(|b| b.as_u32() as usize),
            ),
        }
    }
    false
}

/// Whether the value a store at `(block, stmt)` hands on is still observable
/// through where it came from once the store has run. `ret` marks a copy
/// into the return slot, past which this frame's own values are dead.
pub(super) fn source_outlives_store(
    body: &Body,
    tcx: &gossamer_types::TyCtxt,
    value: Local,
    (block, stmt): (usize, usize),
    ret: bool,
) -> bool {
    let (origin, chain, place) = stored_origin(body, tcx, value);
    let chain_read = || chain.iter().any(|l| read_after(body, *l, block, stmt));
    match origin {
        // A value built for the store that the frame still reads afterwards
        // is named after all: a call result taken straight into a binding.
        StoredOrigin::Fresh | StoredOrigin::Binding => !ret && chain_read(),
        StoredOrigin::Place => {
            let Some(place) = place else { return true };
            // A referent belongs to the caller, which outlives this frame.
            let through_reference = place
                .projection
                .iter()
                .any(|s| matches!(s, Projection::Deref))
                || reference_ty(tcx, body.locals[place.local.0 as usize].ty);
            if through_reference {
                return true;
            }
            !ret && (chain_read() || read_after(body, place.local, block, stmt))
        }
    }
}

/// Which stores a statement makes of a whole local, as operand positions.
#[derive(Clone, Copy)]
enum StoreSite {
    /// Operand `index` of an aggregate.
    Aggregate(usize),
    /// Argument `index` of an intrinsic call that keeps it.
    Intrinsic(usize),
    /// The right-hand side of a field or element store.
    FieldStore,
    /// The value copied into the return slot.
    Return,
    /// The value argument `index` of a call storing into a container.
    ContainerStore(usize),
}

/// The type at `path` inside the aggregate `ty`.
pub(super) fn field_ty_at(
    tcx: &gossamer_types::TyCtxt,
    ty: gossamer_types::Ty,
    path: &[u32],
) -> Option<gossamer_types::Ty> {
    use gossamer_types::TyKind;
    let mut ty = ty;
    for &index in path {
        ty = match tcx.kind_of(ty) {
            TyKind::Adt { def, substs } => {
                let fields = if substs.is_empty() {
                    tcx.struct_field_tys(*def)?.to_vec()
                } else {
                    tcx.struct_fields_instance(*def, substs)
                        .map(<[gossamer_types::Ty]>::to_vec)
                        .or_else(|| {
                            tcx.adt_field_tys(*def, substs)
                                .map(<[gossamer_types::Ty]>::to_vec)
                        })?
                };
                *fields.get(index as usize)?
            }
            TyKind::Tuple(elems) => *elems.get(index as usize)?,
            TyKind::Array { elem, len } if (index as usize) < len.to_usize() => *elem,
            _ => return None,
        };
    }
    Some(ty)
}

/// How a stored value of type `ty` gets storage of its own: one vector
/// clone, a clone per vector field of an aggregate, or nothing when it holds
/// no vector.
enum CopyShape {
    Vector(gossamer_types::Ty),
    Fields(gossamer_types::Ty, Vec<(Vec<u32>, gossamer_types::Ty)>),
}

fn copy_shape(tcx: &gossamer_types::TyCtxt, ty: gossamer_types::Ty) -> Option<CopyShape> {
    use gossamer_types::TyKind;
    // A `&mut Vec` crosses as the vector's handle, so a store of one through
    // its reference copies the vector it names.
    let mut peeled = ty;
    while let TyKind::Ref { inner, .. } = tcx.kind_of(peeled) {
        peeled = *inner;
    }
    match tcx.kind_of(peeled) {
        TyKind::Vec(_) | TyKind::Slice(_) => Some(CopyShape::Vector(peeled)),
        TyKind::Adt { .. } | TyKind::Tuple(_) | TyKind::Array { .. } if peeled == ty => {
            let fields: Vec<(Vec<u32>, gossamer_types::Ty)> = aggregate_rc_field_paths(tcx, ty)
                .into_iter()
                .filter(|(_, kind)| *kind == FieldRcKind::Vec)
                .filter_map(|(path, _)| {
                    let field = field_ty_at(tcx, ty, &path)?;
                    matches!(tcx.kind_of(field), TyKind::Vec(_) | TyKind::Slice(_))
                        .then_some((path, field))
                })
                .collect();
            (!fields.is_empty()).then_some(CopyShape::Fields(ty, fields))
        }
        _ => None,
    }
}

fn container_store_value(name: &str, args: &[Operand]) -> Option<usize> {
    let stores = name.starts_with("gos_rt_vec_push")
        || name.starts_with("gos_rt_deque_push")
        || matches!(
            name,
            "gos_rt_vec_set" | "gos_rt_vec_set_i64" | "gos_rt_vec_set_i64_unchecked"
        )
        || name.starts_with("gos_rt_vec_insert")
        || name.starts_with("gos_rt_map_insert")
        || name.starts_with("gos_rt_map_or_insert");
    (stores && args.len() >= 2).then(|| args.len() - 1)
}

fn new_local(body: &mut Body, ty: gossamer_types::Ty) -> Local {
    let local = Local(u32::try_from(body.locals.len()).expect("local overflow"));
    body.locals.push(crate::ir::LocalDecl {
        ty,
        debug_name: None,
        mutable: false,
        region: false,
    });
    local
}

fn new_block(body: &mut Body, span: gossamer_lex::Span) -> usize {
    let index = body.blocks.len();
    body.blocks.push(crate::ir::BasicBlock {
        id: BlockId(u32::try_from(index).expect("block overflow")),
        stmts: Vec::new(),
        terminator: Terminator::Return,
        span,
        terminator_span: None,
        terminator_inlined: None,
    });
    index
}

/// Gives a vector, or an aggregate holding one, stored into a new holder - a
/// tuple, struct, or enum being built, an `Option` or `Result`, a container, a
/// field, or the return slot - storage of its own when the place it came from
/// can still read or write it afterwards.
///
/// A vector's storage is shared by count and written in place, so a holder
/// and a binding naming one buffer would each see the other's writes. A value
/// nothing names after the store is handed over as it is.
pub(crate) fn copy_values_into_holders(body: &mut Body, tcx: &gossamer_types::TyCtxt) {
    let n_locals = body.locals.len();
    let bare = |op: &Operand| match op {
        Operand::Copy(p) if p.projection.is_empty() && (p.local.0 as usize) < n_locals => {
            Some(p.local)
        }
        _ => None,
    };
    let mut sites: Vec<(usize, usize, StoreSite, Local)> = Vec::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        for (si, stmt) in block.stmts.iter().enumerate() {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            match rvalue {
                Rvalue::Aggregate { operands, .. } => {
                    // A lent comparison slot reads the value where it lies.
                    if place.projection.is_empty() && body.lent_slots.contains(&place.local) {
                        continue;
                    }
                    for (k, op) in operands.iter().enumerate() {
                        // An operand the literal shares with a binding nothing
                        // writes, in a value only read, keeps that storage.
                        if place.projection.is_empty()
                            && body.shared_stores.contains(&(place.local, k))
                        {
                            continue;
                        }
                        if let Some(l) = bare(op) {
                            sites.push((bi, si, StoreSite::Aggregate(k), l));
                        }
                    }
                }
                Rvalue::CallIntrinsic { name, args } => {
                    let index = match *name {
                        "gos_rt_result_new" | "gos_rt_result_new_owned" => 1,
                        "gos_store" | "gos_rt_enum_box_aggr" => 2,
                        _ => continue,
                    };
                    // A closure names its captures rather than copying them:
                    // its writes reach the binding it captured.
                    if *name == "gos_store"
                        && matches!(args.first(), Some(Operand::Copy(env)) if env.projection.is_empty()
                            && is_closure_env(body, env.local))
                    {
                        continue;
                    }
                    if let Some(l) = args.get(index).and_then(bare) {
                        sites.push((bi, si, StoreSite::Intrinsic(index), l));
                    }
                }
                Rvalue::Use(op) => {
                    let Some(l) = bare(op) else { continue };
                    if place.local == Local::RETURN && place.projection.is_empty() {
                        sites.push((bi, si, StoreSite::Return, l));
                    } else if !place.projection.is_empty() {
                        sites.push((bi, si, StoreSite::FieldStore, l));
                    }
                }
                _ => {}
            }
        }
        // A container store of a vector is `copy_stored_containers`' to copy;
        // an aggregate holding one is this pass's.
        if let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            args,
            ..
        } = &block.terminator
            && let Some(index) = container_store_value(name, args)
            && let Some(l) = args.get(index).and_then(bare)
            && matches!(
                copy_shape(tcx, body.locals[l.0 as usize].ty),
                Some(CopyShape::Fields(..))
            )
        {
            sites.push((bi, block.stmts.len(), StoreSite::ContainerStore(index), l));
        }
    }
    let mut copies: Vec<(usize, usize, StoreSite, Local, CopyShape)> = Vec::new();
    for (bi, si, site, value) in sites {
        let Some(shape) = copy_shape(tcx, body.locals[value.0 as usize].ty) else {
            continue;
        };
        let ret = matches!(site, StoreSite::Return);
        if source_outlives_store(body, tcx, value, (bi, si), ret) {
            copies.push((bi, si, site, value, shape));
        }
    }
    // Later sites first, so splitting a block keeps earlier indices valid.
    // Two operands of one statement share its position, and the first split
    // moves that statement into a block of its own; each split is recorded as
    // `(block, first moved index, new block)` so a later site follows it.
    copies.sort_by_key(|&(bi, si, ..)| std::cmp::Reverse((bi, si)));
    let mut relocations: Vec<(usize, usize, usize)> = Vec::new();
    for (bi, si, site, value, shape) in copies {
        let (bi, si) = relocations
            .iter()
            .fold((bi, si), |(b, s), &(from, start, to)| {
                if b == from && s >= start {
                    (to, s - start)
                } else {
                    (b, s)
                }
            });
        let span = body.blocks[bi].span;
        let copy_ty = match &shape {
            CopyShape::Vector(ty) | CopyShape::Fields(ty, _) => *ty,
        };
        let copy = new_local(body, copy_ty);
        // A copy made inside an arena region lives in that region too.
        let in_region = body.locals[value.0 as usize].region;
        body.locals[copy.0 as usize].region = in_region;
        // An enum node takes its share of a vector payload by a retain the
        // constructor places just before the store, which the copy then takes
        // instead.
        let node_retain = if matches!(site, StoreSite::Intrinsic(2)) {
            (0..si).rev().find(|&r| {
                matches!(&body.blocks[bi].stmts[r].kind, StatementKind::Assign {
                    rvalue: Rvalue::CallIntrinsic { name: "gos_rt_vec_retain", args }, ..
                } if matches!(args.as_slice(), [Operand::Copy(p)] if *p == Place::local(value)))
            })
        } else {
            None
        };
        let split = node_retain.unwrap_or(si);
        let rest = new_block(body, span);
        relocations.push((bi, split, rest));
        let rest_stmts = body.blocks[bi].stmts.split_off(split);
        let terminator = std::mem::replace(&mut body.blocks[bi].terminator, Terminator::Return);
        body.blocks[rest].stmts = rest_stmts;
        body.blocks[rest].terminator = terminator;
        body.blocks[rest].terminator_span = body.blocks[bi].terminator_span;
        let inlined = body.blocks[bi].terminator_inlined.clone();
        body.blocks[rest].terminator_inlined = inlined;
        let with_copy = Operand::Copy(Place::local(copy));
        if node_retain.is_some()
            && let StatementKind::Assign {
                rvalue: Rvalue::CallIntrinsic { args, .. },
                ..
            } = &mut body.blocks[rest].stmts[0].kind
        {
            args[0] = with_copy.clone();
        }
        match site {
            StoreSite::ContainerStore(k) => {
                if let Terminator::Call { args, .. } = &mut body.blocks[rest].terminator {
                    args[k] = with_copy.clone();
                }
            }
            _ => {
                if let StatementKind::Assign { rvalue, .. } =
                    &mut body.blocks[rest].stmts[si - split].kind
                {
                    let operand = match (site, rvalue) {
                        (StoreSite::Aggregate(k), Rvalue::Aggregate { operands, .. }) => {
                            operands.get_mut(k)
                        }
                        (StoreSite::Intrinsic(k), Rvalue::CallIntrinsic { args, .. }) => {
                            args.get_mut(k)
                        }
                        (StoreSite::FieldStore | StoreSite::Return, Rvalue::Use(op)) => Some(op),
                        _ => None,
                    };
                    if let Some(op) = operand {
                        *op = with_copy.clone();
                    }
                }
            }
        }
        let clone_call = |source: Place, destination: Place, target: usize| Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_vec_clone".to_string())),
            args: vec![Operand::Copy(source)],
            destination,
            target: Some(BlockId(u32::try_from(target).expect("block overflow"))),
        };
        match shape {
            CopyShape::Vector(_) => {
                body.blocks[bi].terminator =
                    clone_call(Place::local(value), Place::local(copy), rest);
            }
            CopyShape::Fields(_, fields) => {
                // The aggregate's words first, then each vector field swapped for
                // a clone of its own.
                body.blocks[bi].stmts.push(Statement {
                    kind: StatementKind::Assign {
                        place: Place::local(copy),
                        rvalue: Rvalue::Use(Operand::Copy(Place::local(value))),
                    },
                    span,
                    inlined: None,
                });
                let mut at = bi;
                for (path, field_ty) in fields {
                    let field = Place {
                        local: copy,
                        projection: path
                            .iter()
                            .map(|i| Projection::Field(*i))
                            .collect::<Vec<_>>()
                            .into(),
                    };
                    let cloned = new_local(body, field_ty);
                    body.locals[cloned.0 as usize].region = in_region;
                    let next = new_block(body, span);
                    body.blocks[at].terminator =
                        clone_call(field.clone(), Place::local(cloned), next);
                    body.blocks[next].stmts.push(Statement {
                        kind: StatementKind::Assign {
                            place: field,
                            rvalue: Rvalue::Use(Operand::Copy(Place::local(cloned))),
                        },
                        span,
                        inlined: None,
                    });
                    at = next;
                }
                body.blocks[at].terminator = Terminator::Goto {
                    target: BlockId(u32::try_from(rest).expect("block overflow")),
                };
            }
        }
    }
}
