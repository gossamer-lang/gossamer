/// Splits an `Option` over a plain payload that one body builds, copies,
/// tests, and reads into its discriminant and its payload, so `Some(p)` costs
/// no heap cell.
///
/// The carrier's words are an allocation holding the payload once the payload
/// is wider than one word. When every use of the carrier and of each whole
/// copy of it is a construction, a discriminant test, a payload read, or a
/// share count the plain payload never needed, the discriminant lives in an
/// `i64` local and the payload in a local of its own type instead. Any other
/// use - a call argument, a return, a field of an aggregate, a place a
/// terminator drops - keeps the carrier as it is.
pub(crate) fn unbox_local_carriers(body: &mut Body, tcx: &gossamer_types::TyCtxt) {
    use gossamer_types::{IntTy, TyKind};

    let Some(i64_ty) = tcx.interned(&TyKind::Int(IntTy::I64)) else {
        return;
    };
    let first_free = body.arity as usize + 1;
    let payload_of: Vec<Option<gossamer_types::Ty>> = (0..body.locals.len())
        .map(|index| {
            if index < first_free {
                return None;
            }
            unboxable_option_payload(tcx, body.locals[index].ty)
        })
        .collect();
    if payload_of.iter().all(Option::is_none) {
        return;
    }
    let web_of = carrier_webs(body, &payload_of);
    let webs = handled_webs(body, &web_of, &payload_of);
    if webs.is_empty() {
        return;
    }
    let (split, entry_inits) = split_web_locals(body, i64_ty, &webs, &payload_of, tcx);
    if split.is_empty() {
        return;
    }
    rewrite_split_carriers(body, &web_of, &split);
    if let Some(entry) = body.blocks.first_mut() {
        entry.stmts.splice(0..0, entry_inits);
    }
    for (index, web) in web_of.iter().enumerate() {
        if web.is_some_and(|web| split.contains_key(&web)) {
            body.locals[index].ty = i64_ty;
        }
    }
}

/// The web each carrier local belongs to, named by its lowest local: whole
/// copies between carriers of one payload type join them into one web.
fn carrier_webs(body: &Body, payload_of: &[Option<gossamer_types::Ty>]) -> Vec<Option<usize>> {
    let mut parent: Vec<usize> = (0..payload_of.len()).collect();
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign {
                place,
                rvalue: Rvalue::Use(Operand::Copy(source)),
            } = &stmt.kind
                && place.projection.is_empty()
                && source.projection.is_empty()
                && payload_of[place.local.0 as usize].is_some()
                && payload_of[place.local.0 as usize] == payload_of[source.local.0 as usize]
            {
                let a = web_root(&mut parent, place.local.0 as usize);
                let b = web_root(&mut parent, source.local.0 as usize);
                parent[a.max(b)] = a.min(b);
            }
        }
    }
    (0..payload_of.len())
        .map(|index| payload_of[index].map(|_| web_root(&mut parent, index)))
        .collect()
}

/// The representative of `at`'s web, halving the path on the way.
fn web_root(parent: &mut [usize], mut at: usize) -> usize {
    while parent[at] != at {
        parent[at] = parent[parent[at]];
        at = parent[at];
    }
    at
}

/// The webs whose every mention is a shape the rewrite handles, in ascending
/// order so the locals the split adds are numbered the same on every build.
fn handled_webs(
    body: &Body,
    web_of: &[Option<usize>],
    payload_of: &[Option<gossamer_types::Ty>],
) -> Vec<usize> {
    let mut valid: std::collections::HashMap<usize, bool> =
        web_of.iter().flatten().map(|web| (*web, true)).collect();
    let member = |operand: &Operand| match operand {
        Operand::Copy(place) if place.projection.is_empty() => web_of[place.local.0 as usize],
        _ => None,
    };
    let members: Vec<(Local, usize)> = web_of
        .iter()
        .enumerate()
        .filter_map(|(index, web)| web.map(|web| (Local(index as u32), web)))
        .collect();
    for block in &body.blocks {
        for stmt in &block.stmts {
            let handled = carrier_statement_shape(stmt, &member, payload_of, body);
            for &(local, web) in &members {
                if handled != Some(web) && stmt_mentions_local(stmt, local) {
                    valid.insert(web, false);
                }
            }
        }
        let handled = carrier_terminator_shape(&block.terminator, &member);
        for &(local, web) in &members {
            let mentions = term_mentions_local(&block.terminator, local)
                || matches!(&block.terminator, Terminator::Drop { place, .. }
                    if place_mentions_local(place, local));
            if mentions && handled != Some(web) {
                valid.insert(web, false);
            }
        }
    }
    // A share-count statement's result must be read nowhere, since the
    // rewrite leaves it unassigned.
    let count_results: Vec<(usize, Local)> = body
        .blocks
        .iter()
        .flat_map(|block| block.stmts.iter())
        .filter_map(|stmt| match &stmt.kind {
            StatementKind::Assign {
                place,
                rvalue: Rvalue::CallIntrinsic { name, args },
            } if matches!(*name, "gos_rt_option_slot_retain" | "gos_rt_option_slot_release") => {
                Some((member(args.first()?)?, place.local))
            }
            _ => None,
        })
        .collect();
    for (web, result) in count_results {
        let mentions = body
            .blocks
            .iter()
            .map(|block| {
                block
                    .stmts
                    .iter()
                    .filter(|stmt| stmt_mentions_local(stmt, result))
                    .count()
                    + usize::from(term_mentions_local(&block.terminator, result))
            })
            .sum::<usize>();
        if mentions != 1 {
            valid.insert(web, false);
        }
    }
    let mut webs: Vec<usize> = valid
        .iter()
        .filter(|(_, ok)| **ok)
        .map(|(web, _)| *web)
        .collect();
    webs.sort_unstable();
    webs
}

/// One discriminant and one payload local per web, and the statements that
/// initialise both on entry so every path reads storage of its own.
fn split_web_locals(
    body: &mut Body,
    i64_ty: gossamer_types::Ty,
    webs: &[usize],
    payload_of: &[Option<gossamer_types::Ty>],
    tcx: &gossamer_types::TyCtxt,
) -> (std::collections::HashMap<usize, (Local, Local)>, Vec<Statement>) {
    let mut split = std::collections::HashMap::new();
    let mut entry_inits = Vec::new();
    for &web in webs {
        let Some(payload_ty) = payload_of[web] else {
            continue;
        };
        let Some(zero) = zero_plain_payload(tcx, payload_ty) else {
            continue;
        };
        let disc = Local(body.locals.len() as u32);
        body.locals.push(LocalDecl {
            ty: i64_ty,
            debug_name: None,
            mutable: true,
            region: false,
        });
        let payload = Local(body.locals.len() as u32);
        body.locals.push(LocalDecl {
            ty: payload_ty,
            debug_name: None,
            mutable: true,
            region: false,
        });
        let span = body.span;
        entry_inits.push(Statement {
            kind: StatementKind::Assign {
                place: Place::local(disc),
                rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(1))),
            },
            span,
            inlined: InlineChain::default(),
        });
        entry_inits.push(Statement {
            kind: StatementKind::Assign {
                place: Place::local(payload),
                rvalue: zero,
            },
            span,
            inlined: InlineChain::default(),
        });
        split.insert(web, (disc, payload));
    }
    (split, entry_inits)
}

/// Rewrites every statement and arm-test call that touches a split carrier.
fn rewrite_split_carriers(
    body: &mut Body,
    web_of: &[Option<usize>],
    split: &std::collections::HashMap<usize, (Local, Local)>,
) {
    let member = |operand: &Operand| match operand {
        Operand::Copy(place) if place.projection.is_empty() => web_of[place.local.0 as usize],
        _ => None,
    };
    let parts = |operand: &Operand| member(operand).and_then(|web| split.get(&web).copied());
    for block in &mut body.blocks {
        let mut rebuilt = Vec::with_capacity(block.stmts.len());
        for stmt in block.stmts.drain(..) {
            match carrier_replacement(&stmt.kind, web_of, split, &parts) {
                None => rebuilt.push(stmt),
                Some(kinds) if kinds.is_empty() => rebuilt.push(Statement {
                    kind: StatementKind::Nop,
                    ..stmt
                }),
                Some(kinds) => {
                    for kind in kinds {
                        rebuilt.push(Statement {
                            kind,
                            span: stmt.span,
                            inlined: stmt.inlined.clone(),
                        });
                    }
                }
            }
        }
        block.stmts = rebuilt;
        if let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            args,
            destination,
            target: Some(target),
        } = &block.terminator
            && let Some((disc, _)) = args.first().and_then(&parts)
        {
            let op = match name.as_str() {
                "gos_rt_result_is_ok" | "gos_rt_option_is_some" => BinOp::Eq,
                _ => BinOp::Ne,
            };
            let span = block.terminator_span.unwrap_or(block.span);
            block.stmts.push(Statement {
                kind: StatementKind::Assign {
                    place: destination.clone(),
                    rvalue: Rvalue::BinaryOp {
                        op,
                        lhs: Operand::Copy(Place::local(disc)),
                        rhs: Operand::Const(ConstValue::Int(0)),
                    },
                },
                span,
                inlined: block.terminator_inlined.clone(),
            });
            block.terminator = Terminator::Goto { target: *target };
        }
    }
}

/// The statements that stand for `kind` once its carriers are split, or `None`
/// when it touches no split carrier. An empty list drops the statement.
fn carrier_replacement(
    kind: &StatementKind,
    web_of: &[Option<usize>],
    split: &std::collections::HashMap<usize, (Local, Local)>,
    parts: &impl Fn(&Operand) -> Option<(Local, Local)>,
) -> Option<Vec<StatementKind>> {
    let StatementKind::Assign { place, rvalue } = kind else {
        return None;
    };
    let dest_parts = place
        .projection
        .is_empty()
        .then(|| web_of[place.local.0 as usize])
        .flatten()
        .and_then(|web| split.get(&web).copied());
    let copy_into = |into: Local, from: Local| StatementKind::Assign {
        place: Place::local(into),
        rvalue: Rvalue::Use(Operand::Copy(Place::local(from))),
    };
    match (dest_parts, rvalue) {
        (Some((disc, payload)), Rvalue::CallIntrinsic { name, args })
            if *name == "gos_rt_result_new" =>
        {
            let Some(Operand::Const(ConstValue::Int(d))) = args.first() else {
                return None;
            };
            let mut kinds = vec![StatementKind::Assign {
                place: Place::local(disc),
                rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(*d))),
            }];
            if let Some(Operand::Copy(agg)) = args.get(1) {
                kinds.push(StatementKind::Assign {
                    place: Place::local(payload),
                    rvalue: Rvalue::Use(Operand::Copy(agg.clone())),
                });
            }
            Some(kinds)
        }
        (Some(_), Rvalue::Use(Operand::Const(_))) => Some(Vec::new()),
        (Some((disc, payload)), Rvalue::Use(source)) => {
            let (from_disc, from_payload) = parts(source)?;
            Some(vec![copy_into(disc, from_disc), copy_into(payload, from_payload)])
        }
        (None, Rvalue::CallIntrinsic { name, args }) => {
            let (disc, payload) = parts(args.first()?)?;
            match *name {
                "gos_rt_result_disc" => Some(vec![StatementKind::Assign {
                    place: place.clone(),
                    rvalue: Rvalue::Use(Operand::Copy(Place::local(disc))),
                }]),
                "gos_rt_result_payload" => Some(vec![StatementKind::Assign {
                    place: place.clone(),
                    rvalue: Rvalue::Use(Operand::Copy(Place::local(payload))),
                }]),
                "gos_rt_option_slot_retain" | "gos_rt_option_slot_release" => Some(Vec::new()),
                _ => None,
            }
        }
        _ => None,
    }
}

/// The payload of `Option<P>` when `P` is a tuple or struct of scalars wider
/// than one word.
fn unboxable_option_payload(
    tcx: &gossamer_types::TyCtxt,
    ty: gossamer_types::Ty,
) -> Option<gossamer_types::Ty> {
    use gossamer_types::TyKind;
    let TyKind::Adt { def, substs } = tcx.kind_of(ty) else {
        return None;
    };
    if def.local != u32::MAX - 1 {
        return None;
    }
    let payload = substs.types().first().copied()?;
    let fields = scalar_fields(tcx, payload)?;
    (fields.len() > 1 && tcx.plain_layout(payload).is_some()).then_some(payload)
}

/// The field types of a tuple or struct whose every field is a scalar.
fn scalar_fields(
    tcx: &gossamer_types::TyCtxt,
    ty: gossamer_types::Ty,
) -> Option<Vec<gossamer_types::Ty>> {
    use gossamer_types::TyKind;
    let fields = match tcx.kind_of(ty) {
        TyKind::Tuple(items) => items.clone(),
        TyKind::Adt { def, substs } if def.local < u32::MAX - 16 => {
            tcx.adt_field_tys(*def, substs)?.to_vec()
        }
        _ => return None,
    };
    fields
        .iter()
        .all(|field| {
            matches!(
                tcx.kind_of(*field),
                TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char
            )
        })
        .then_some(fields)
}

/// A zero value of a payload `scalar_fields` accepts.
fn zero_plain_payload(tcx: &gossamer_types::TyCtxt, ty: gossamer_types::Ty) -> Option<Rvalue> {
    use gossamer_types::TyKind;
    let fields = scalar_fields(tcx, ty)?;
    let operands = fields
        .iter()
        .map(|field| {
            Operand::Const(match tcx.kind_of(*field) {
                TyKind::Float(_) => ConstValue::Float(0),
                TyKind::Bool => ConstValue::Bool(false),
                TyKind::Char => ConstValue::Char('\0'),
                _ => ConstValue::Int(0),
            })
        })
        .collect();
    let kind = match tcx.kind_of(ty) {
        TyKind::Adt { def, .. } => crate::ir::AggregateKind::Adt {
            def: *def,
            variant: 0,
        },
        _ => crate::ir::AggregateKind::Tuple,
    };
    Some(Rvalue::Aggregate { kind, operands })
}

/// The web a statement touches when it is a shape the rewrite handles.
fn carrier_statement_shape(
    stmt: &Statement,
    member: &impl Fn(&Operand) -> Option<usize>,
    payload_of: &[Option<gossamer_types::Ty>],
    body: &Body,
) -> Option<usize> {
    let StatementKind::Assign { place, rvalue } = &stmt.kind else {
        return None;
    };
    let dest_web = place
        .projection
        .is_empty()
        .then(|| member(&Operand::Copy(place.clone())))
        .flatten();
    match rvalue {
        Rvalue::CallIntrinsic { name, args } if *name == "gos_rt_result_new" => {
            let web = dest_web?;
            let payload_ty = payload_of[place.local.0 as usize]?;
            match args.as_slice() {
                [Operand::Const(ConstValue::Int(1)), Operand::Const(ConstValue::Int(0))] => {
                    Some(web)
                }
                [Operand::Const(ConstValue::Int(0)), Operand::Copy(agg)]
                    if agg.projection.is_empty()
                        && body.locals[agg.local.0 as usize].ty == payload_ty =>
                {
                    Some(web)
                }
                _ => None,
            }
        }
        Rvalue::Use(Operand::Const(ConstValue::Int(0))) => dest_web,
        Rvalue::Use(source) => {
            let web = dest_web?;
            (member(source) == Some(web)).then_some(web)
        }
        Rvalue::CallIntrinsic { name, args }
            if place.projection.is_empty()
                && matches!(
                    *name,
                    "gos_rt_result_disc"
                        | "gos_rt_result_payload"
                        | "gos_rt_option_slot_retain"
                        | "gos_rt_option_slot_release"
                )
                && dest_web.is_none() =>
        {
            let [arg] = args.as_slice() else {
                return None;
            };
            let web = member(arg)?;
            if *name == "gos_rt_result_payload" {
                let payload_ty = payload_of[match arg {
                    Operand::Copy(p) => p.local.0 as usize,
                    _ => return None,
                }]?;
                (body.locals[place.local.0 as usize].ty == payload_ty).then_some(web)
            } else {
                Some(web)
            }
        }
        _ => None,
    }
}

/// The web a terminator touches when it is a discriminant test the rewrite
/// handles.
fn carrier_terminator_shape(
    terminator: &Terminator,
    member: &impl Fn(&Operand) -> Option<usize>,
) -> Option<usize> {
    let Terminator::Call {
        callee: Operand::Const(ConstValue::Str(name)),
        args,
        destination,
        target: Some(_),
    } = terminator
    else {
        return None;
    };
    if !matches!(
        name.as_str(),
        "gos_rt_result_is_ok" | "gos_rt_result_is_err" | "gos_rt_option_is_some" | "gos_rt_option_is_none"
    ) || !destination.projection.is_empty()
    {
        return None;
    }
    let [arg] = args.as_slice() else {
        return None;
    };
    let web = member(arg)?;
    (member(&Operand::Copy(destination.clone())).is_none()).then_some(web)
}
