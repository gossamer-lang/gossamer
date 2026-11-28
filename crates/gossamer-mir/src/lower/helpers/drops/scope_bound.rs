//! Releases of scope-bound bindings: a binding whose type holds a cleanup
//! gives back its share where its block ends, so the cleanup runs at the
//! cleanup point that follows.

use super::*;

/// The marker `__gos_release_binding(x)` lowers to until the drop passes have
/// placed every release, carrying the binding as its one operand.
pub(crate) const RELEASE_BINDING_MARKER: &str = "gos_release_binding_marker";

/// The marker a cleanup point gives each temporary of the statement before
/// it whose type holds a cleanup. A temporary owns exactly what it holds, so
/// unlike a binding it never stands for a table another local owns.
pub(crate) const RELEASE_TEMP_MARKER: &str = "gos_release_temp_marker";

/// Replaces each release marker with the releases the body gives the
/// binding's local, each followed by the value that empties what it released,
/// so every later release of the local reads an empty value. A marker for a
/// local the body never releases (a parameter, a value it handed on) becomes a
/// no-op.
///
/// Runs after the passes that move or elide shares: until then the marker is
/// a read of the binding, so none of them hands the binding's share to an
/// earlier holder.
pub(crate) fn release_scope_bound_bindings(body: &mut Body, tcx: &gossamer_types::TyCtxt) {
    let has_marker = body
        .blocks
        .iter()
        .any(|block| block.stmts.iter().any(|stmt| is_marker(stmt).is_some()));
    if !has_marker {
        return;
    }
    // A release marker's own destination is built `()`, so the unit type is
    // interned whenever a marker exists.
    let Some(unit_ty) = tcx.unit_interned() else {
        return;
    };
    let mut releases: HashMap<Local, Vec<Rvalue>> = HashMap::new();
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign {
                rvalue: rvalue @ Rvalue::CallIntrinsic { name, args },
                ..
            } = &stmt.kind
                && is_release_intrinsic(name)
                && let Some(Operand::Copy(place)) = args.first()
            {
                let list = releases.entry(place.local).or_default();
                if !list.contains(rvalue) {
                    list.push(rvalue.clone());
                }
            }
        }
    }
    let mut entry_empties: Vec<(Place, Rvalue)> = Vec::new();
    if let Some(entry) = body.blocks.first() {
        for stmt in &entry.stmts {
            if let StatementKind::Assign { place, rvalue } = &stmt.kind
                && releases.contains_key(&place.local)
                && is_empty_value(rvalue)
            {
                if !entry_empties.iter().any(|(seen, _)| seen == place) {
                    entry_empties.push((place.clone(), rvalue.clone()));
                }
            }
        }
    }
    // A binding of a table (a map, a set) is an alias of the table another
    // local owns when that local is its one source: the release that ends
    // the table is the owner's.
    let mut sources: HashMap<Local, Option<Local>> = HashMap::new();
    for block in &body.blocks {
        for stmt in &block.stmts {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            if !place.projection.is_empty() || is_empty_value(rvalue) {
                continue;
            }
            let source = match rvalue {
                Rvalue::Use(Operand::Copy(src)) if src.projection.is_empty() => Some(src.local),
                _ => None,
            };
            sources
                .entry(place.local)
                .and_modify(|seen| *seen = None)
                .or_insert(source);
        }
    }
    let owner_of = |mut local: Local| {
        for _ in 0..8 {
            if releases.contains_key(&local) {
                return Some(local);
            }
            local = sources.get(&local).copied().flatten()?;
        }
        None
    };
    let local_tys: Vec<gossamer_types::Ty> = body.locals.iter().map(|decl| decl.ty).collect();
    let mut new_locals = 0usize;
    let first_new = body.locals.len();
    for block in &mut body.blocks {
        if !block.stmts.iter().any(|stmt| is_marker(stmt).is_some()) {
            continue;
        }
        let stmts = std::mem::take(&mut block.stmts);
        let mut out = Vec::with_capacity(stmts.len() + 4);
        for stmt in stmts {
            let Some((local, binding)) = marker_of(&stmt) else {
                out.push(stmt);
                continue;
            };
            let span = stmt.span;
            let owner = if binding {
                owner_of(local)
            } else {
                Some(local)
            };
            let Some(release) = owner.and_then(|owner| releases.get(&owner)) else {
                out.push(Statement {
                    kind: StatementKind::Nop,
                    span,
                    inlined: None,
                });
                continue;
            };
            // Every release reads the value before any empties it: a carrier
            // gives back its payload's children and then its slot.
            let mut empties: Vec<Statement> = Vec::new();
            for rvalue in release {
                let Rvalue::CallIntrinsic { name, args } = rvalue else {
                    continue;
                };
                let Some(Operand::Copy(place)) = args.first() else {
                    continue;
                };
                let mut fresh_unit = || {
                    let dest =
                        Local(u32::try_from(first_new + new_locals).expect("local overflow"));
                    new_locals += 1;
                    Place::local(dest)
                };
                out.push(Statement {
                    kind: StatementKind::Assign {
                        place: fresh_unit(),
                        rvalue: rvalue.clone(),
                    },
                    span,
                    inlined: None,
                });
                let empty = if *name == "gos_rt_aggr_release_children" {
                    (
                        fresh_unit(),
                        Rvalue::CallIntrinsic {
                            name: "gos_rt_aggr_zero_guarded",
                            args: args.clone(),
                        },
                    )
                } else if place.projection.is_empty()
                    && tcx.is_two_word_value(local_tys[place.local.0 as usize])
                {
                    // A carrier is emptied to the arm that holds nothing, so
                    // a later read of its discriminant takes no payload.
                    (
                        place.clone(),
                        Rvalue::CallIntrinsic {
                            name: "gos_rt_result_new",
                            args: vec![
                                Operand::Const(ConstValue::Int(1)),
                                Operand::Const(ConstValue::Int(0)),
                            ],
                        },
                    )
                } else if let Some((_, empty)) =
                    entry_empties.iter().find(|(seen, _)| seen == place)
                {
                    (place.clone(), empty.clone())
                } else {
                    (
                        place.clone(),
                        Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                    )
                };
                let empty = Statement {
                    kind: StatementKind::Assign {
                        place: empty.0,
                        rvalue: empty.1,
                    },
                    span,
                    inlined: None,
                };
                let repeated = empties.iter().any(|seen| {
                    matches!((&seen.kind, &empty.kind),
                        (StatementKind::Assign { place: a, rvalue: ra },
                         StatementKind::Assign { place: b, rvalue: rb }) if a == b && ra == rb)
                });
                if !repeated {
                    empties.push(empty);
                }
            }
            out.extend(empties);
        }
        block.stmts = out;
    }
    for _ in 0..new_locals {
        body.locals.push(LocalDecl {
            ty: unit_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
    }
}

/// The local a release marker names, and whether it is a binding's.
fn marker_of(stmt: &Statement) -> Option<(Local, bool)> {
    match &stmt.kind {
        StatementKind::Assign {
            rvalue: Rvalue::CallIntrinsic { name, args },
            ..
        } if *name == RELEASE_BINDING_MARKER || *name == RELEASE_TEMP_MARKER => {
            match args.first() {
                Some(Operand::Copy(place)) if place.projection.is_empty() => {
                    Some((place.local, *name == RELEASE_BINDING_MARKER))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn is_marker(stmt: &Statement) -> Option<Local> {
    marker_of(stmt).map(|(local, _)| local)
}

/// Whether `name` gives back a share of its first operand.
fn is_release_intrinsic(name: &str) -> bool {
    name.ends_with("_release")
        || name.ends_with("_release_children")
        || name.ends_with("_free")
        || name.ends_with("_free_typed")
        || name.contains("_drop")
}

/// Whether `rvalue` is a value the body's entry empties a local with.
fn is_empty_value(rvalue: &Rvalue) -> bool {
    match rvalue {
        Rvalue::Use(Operand::Const(ConstValue::Int(0))) => true,
        Rvalue::CallIntrinsic { name, args } => {
            *name == "gos_rt_result_new"
                && matches!(
                    args.as_slice(),
                    [
                        Operand::Const(ConstValue::Int(1)),
                        Operand::Const(ConstValue::Int(0))
                    ]
                )
        }
        _ => false,
    }
}

/// Runs the cleanups a return released, before the caller goes on: a body
/// that holds a value whose release runs a cleanup gives its last shares back
/// as it returns, and the bytecode VM runs them as the frame goes.
pub(crate) fn drain_cleanups_at_returns(body: &mut Body, tcx: &gossamer_types::TyCtxt) {
    if !body.locals.iter().any(|local| tcx.is_scope_bound(local.ty)) {
        return;
    }
    // Lowering a body interns `()`, for its statements' own destinations.
    let Some(unit_ty) = tcx.unit_interned() else {
        return;
    };
    let mut drains = 0usize;
    let first = body.locals.len();
    for block in &mut body.blocks {
        if !matches!(block.terminator, Terminator::Return) {
            continue;
        }
        let dest = Local(u32::try_from(first + drains).expect("local overflow"));
        drains += 1;
        block.stmts.push(Statement {
            kind: StatementKind::Assign {
                place: Place::local(dest),
                rvalue: Rvalue::CallIntrinsic {
                    name: "gos_rt_runtime_run_cleanups",
                    args: Vec::new(),
                },
            },
            span: block.span,
            inlined: None,
        });
    }
    for _ in 0..drains {
        body.locals.push(LocalDecl {
            ty: unit_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
    }
}
