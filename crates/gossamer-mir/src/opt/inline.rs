/// Detects trivial wrapper functions (a two-block body whose entry block
/// immediately calls another function with all params forwarded in order)
/// and rewrites every call site to invoke the inner function directly,
/// eliminating the intermediate call frame.
///
/// Must be called before [`optimise`] so that subsequent copy-propagation
/// and dead-store passes see the tightened call graph.
pub fn inline_trivial_wrappers(bodies: &mut [Body]) {
    if !inlining_enabled() {
        return;
    }
    let mut wrappers: HashMap<String, Operand> = HashMap::new();
    for body in bodies.iter() {
        if let Some(inner_callee) = detect_wrapper_callee(body) {
            wrappers.insert(body.name.clone(), inner_callee);
        }
    }
    if wrappers.is_empty() {
        return;
    }
    for body in bodies.iter_mut() {
        for block in &mut body.blocks {
            if let Terminator::Call { callee, .. } = &mut block.terminator {
                if let Operand::Const(ConstValue::Str(name)) = callee {
                    if let Some(inner) = wrappers.get(name.as_str()) {
                        *callee = inner.clone();
                    }
                }
            }
        }
    }
}

/// Returns the inner callee if `body` is a trivial wrapper, `None` otherwise.
/// A trivial wrapper has exactly two blocks:
/// - bb0: no statements, `Call` terminator whose args are exactly the
///   params `Local(1)..=Local(arity)` forwarded in source order.
/// - bb1: no statements, `Return` terminator.
fn detect_wrapper_callee(body: &Body) -> Option<Operand> {
    if body.blocks.len() != 2 {
        return None;
    }
    let bb0 = &body.blocks[0];
    let bb1 = &body.blocks[1];
    if !bb0.stmts.is_empty() || !bb1.stmts.is_empty() {
        return None;
    }
    if !matches!(bb1.terminator, Terminator::Return) {
        return None;
    }
    let Terminator::Call {
        callee,
        args,
        destination,
        target,
    } = &bb0.terminator
    else {
        return None;
    };
    if *target != Some(bb1.id) {
        return None;
    }
    if destination.local != Local::RETURN || !destination.projection.is_empty() {
        return None;
    }
    if args.len() != body.arity as usize {
        return None;
    }
    for (i, arg) in args.iter().enumerate() {
        let expected = Local((i as u32) + 1);
        match arg {
            Operand::Copy(place) if place.local == expected && place.projection.is_empty() => {}
            _ => return None,
        }
    }
    Some(callee.clone())
}

/// Per-callee inlining cost ceiling, in weighted MIR units
/// (statements + one per block terminator). Replaces the old flat
/// 4-statement rule: small leaf helpers up to this weight inline.
/// Lowering emits roughly two statements per source operation (a temp
/// plus its named binding), so this admits leaves of about ten to
/// fifteen source statements.
const INLINE_COST_LIMIT: usize = 40;

/// Promotion ceiling for callees that would otherwise exceed
/// [`INLINE_COST_LIMIT`]: a callee whose cost falls in the window
/// `(INLINE_COST_LIMIT, INLINE_CONST_ARG_COST_LIMIT]` is inlined only at
/// call sites passing at least one constant argument, where folding the
/// constant through the spliced body collapses constant-conditioned
/// branches (e.g. the JSON parser's `parse_val` / `parse_str` / `parse_num`
/// dispatched on a constant mode). Mid-window callees are still registered
/// as candidates so the application pass can make this per-call-site choice.
const INLINE_CONST_ARG_COST_LIMIT: usize = 60;

/// Total weighted growth one caller may accrue from inlining before
/// further inlines into it are skipped - caps caller blow-up when a
/// hot function calls many small helpers.
const INLINE_CALLER_BUDGET: usize = 96;

/// Weighted size of a body: one unit per statement plus one per block
/// terminator.
fn body_cost(body: &Body) -> usize {
    body.blocks.iter().map(|b| b.stmts.len() + 1).sum()
}

/// Snapshot of an inlineable callee body: its arity, its locals
/// (for splicing into the caller's local table), and its one
/// computation block's statements.
#[derive(Clone)]
struct InlineableCallee {
    arity: u32,
    /// Callee locals from index `arity + 1` onward (the temps only;
    /// a parameter and the return slot are remapped to caller locals).
    extra_locals: Vec<crate::ir::LocalDecl>,
    /// Declarations of the callee's own parameter locals, for the
    /// parameters that need one of their own at the call site.
    param_locals: Vec<crate::ir::LocalDecl>,
    /// One-based indices of the parameters the callee assigns to. Such
    /// a parameter cannot share the caller's argument local: a `mut`
    /// parameter is the callee's own value, and remapping it onto the
    /// argument would land the write in the caller's variable.
    written_params: Vec<u32>,
    /// Statements from the callee's single computation block, before
    /// remapping.
    stmts: Vec<crate::ir::Statement>,
    /// Weighted cost of the callee body (see [`body_cost`]).
    cost: usize,
}

/// Inlines small (≤ `INLINE_STMT_LIMIT`-statement) single-block callee
/// bodies into their call sites. Only inlines when:
/// - The callee has 1 or 2 blocks with all statements in block 0.
/// - All statements are plain assignments (`Assign`) with no calls and
///   no projected places on the destination.
/// - All call-site arguments are simple bare-local copies or constants
///   (no projections on the arg places).
/// - The call destination place has no projections.
///
/// Run before `optimise` so the per-body passes see the flattened graph.
pub fn inline_small_callees(bodies: &mut [Body]) {
    if !inlining_enabled() {
        return;
    }
    let mut inlineables: HashMap<String, InlineableCallee> = HashMap::new();
    for body in bodies.iter() {
        if let Some(ic) = try_build_inlineable(body) {
            inlineables.insert(body.name.clone(), ic);
        }
    }
    if inlineables.is_empty() {
        return;
    }
    let def_to_name = def_to_name_map(bodies);
    for body in bodies.iter_mut() {
        inline_into_body(body, &inlineables, &def_to_name);
    }
}

fn try_build_inlineable(body: &Body) -> Option<InlineableCallee> {
    // Callee must have 1 or 2 blocks. With 2 blocks, block 1 is an
    // empty Return continuation (same shape as `detect_wrapper_callee`).
    if body.blocks.len() > 2 || body.blocks.is_empty() {
        return None;
    }
    let bb0 = &body.blocks[0];
    // The computation block must not contain nested calls.
    for stmt in &bb0.stmts {
        let StatementKind::Assign { place, rvalue } = &stmt.kind else {
            return None;
        };
        // Destination must be a bare local (no projections).
        if !place.projection.is_empty() {
            return None;
        }
        // Rvalue must not be a call or contain a call.
        if matches!(rvalue, Rvalue::CallIntrinsic { .. }) {
            return None;
        }
        // Aggregate-building callees stay out of the single-block path:
        // it remaps the return slot to the caller's destination without
        // copying the callee's typed return local, so a later nested
        // field read would resolve its leaf type against an untyped
        // local. `inline_general` is the aggregate-capable path - it
        // copies every callee `LocalDecl` (with its `ty`), preserving
        // the return type by construction.
        if matches!(rvalue, Rvalue::Aggregate { .. } | Rvalue::Repeat { .. }) {
            return None;
        }
    }
    // Terminator of bb0: either Return or Goto{bb1}.
    match &bb0.terminator {
        Terminator::Return => {}
        Terminator::Goto { target } => {
            if body.blocks.len() < 2 {
                return None;
            }
            let bb1 = &body.blocks[1];
            if bb1.id != *target || !bb1.stmts.is_empty() {
                return None;
            }
            if !matches!(bb1.terminator, Terminator::Return) {
                return None;
            }
        }
        _ => return None,
    }
    let cost = body_cost(body);
    if cost > INLINE_COST_LIMIT {
        return None;
    }
    // Slice off the extra locals (temps beyond params + return slot).
    let param_end = (body.arity + 1) as usize;
    let extra_locals = if param_end < body.locals.len() {
        body.locals[param_end..].to_vec()
    } else {
        Vec::new()
    };
    let param_locals = body.locals[1..param_end.min(body.locals.len())].to_vec();
    Some(InlineableCallee {
        arity: body.arity,
        extra_locals,
        param_locals,
        written_params: written_parameters(body.arity, &bb0.stmts),
        stmts: bb0.stmts.clone(),
        cost,
    })
}

/// The one-based parameter indices `stmts` writes to, either by
/// assigning the local or by taking a mutable reference to it.
fn written_parameters(arity: u32, stmts: &[crate::ir::Statement]) -> Vec<u32> {
    let mut written = Vec::new();
    let note = |local: Local, written: &mut Vec<u32>| {
        if local.0 >= 1 && local.0 <= arity && !written.contains(&local.0) {
            written.push(local.0);
        }
    };
    for stmt in stmts {
        let StatementKind::Assign { place, rvalue } = &stmt.kind else {
            continue;
        };
        note(place.local, &mut written);
        if let Rvalue::Ref {
            mutable: true,
            place,
        } = rvalue
        {
            note(place.local, &mut written);
        }
    }
    written
}

fn inline_into_body(
    body: &mut Body,
    inlineables: &HashMap<String, InlineableCallee>,
    def_to_name: &HashMap<u32, String>,
) {
    // Iterate over block indices because we mutate `body.locals` (to
    // add callee temps) during the loop. The block list itself does
    // not grow - we splice statements into existing blocks.
    let mut budget = INLINE_CALLER_BUDGET;
    let mut bi = 0;
    while bi < body.blocks.len() {
        let Terminator::Call {
            callee,
            args,
            destination,
            target,
        } = body.blocks[bi].terminator.clone()
        else {
            bi += 1;
            continue;
        };
        let Some(callee_name) = callee_body_name(&callee, def_to_name) else {
            bi += 1;
            continue;
        };
        if callee_name.as_str() == body.name.as_str() {
            bi += 1;
            continue;
        }
        let Some(ic) = inlineables.get(&callee_name) else {
            bi += 1;
            continue;
        };
        if ic.cost > budget {
            bi += 1;
            continue;
        }
        // Guard: arity must match (defensive against a resolved
        // `FnRef` whose call site disagrees with the callee).
        if ic.arity as usize != args.len() {
            bi += 1;
            continue;
        }
        // Guard: destination must be a bare local.
        if !destination.projection.is_empty() {
            bi += 1;
            continue;
        }
        // Guard: all args must be bare-local copies or consts.
        let arg_locals: Vec<Option<Local>> = args
            .iter()
            .map(|op| match op {
                Operand::Copy(p) if p.projection.is_empty() => Some(p.local),
                _ => None,
            })
            .collect();
        if arg_locals.iter().any(Option::is_none) {
            // Some arg is a const or projected place; fall back to
            // introducing a let binding for each such arg.
            // For simplicity, skip inlining this site.
            bi += 1;
            continue;
        }
        // All guards passed - inline.
        budget -= ic.cost;
        let orig_local_count = body.locals.len() as u32;
        body.locals.extend(ic.extra_locals.iter().cloned());

        // A parameter the callee writes gets a local of its own here,
        // initialized from the argument. Sharing the caller's argument
        // local is what makes the read-only case free, and is exactly
        // what a written parameter must not do: `fn f(mut n: i64)`
        // takes the caller's value, not the caller's variable.
        let mut param_copies: Vec<(u32, Local)> = Vec::new();
        let mut bindings: Vec<crate::ir::Statement> = Vec::new();
        for &idx in &ic.written_params {
            let decl = ic.param_locals[(idx - 1) as usize].clone();
            let copy = Local(body.locals.len() as u32);
            body.locals.push(decl);
            bindings.push(crate::ir::Statement {
                kind: StatementKind::Assign {
                    place: Place {
                        local: copy,
                        projection: Vec::new(),
                    },
                    rvalue: Rvalue::Use(args[(idx - 1) as usize].clone()),
                },
                span: body.blocks[bi].span,
            });
            param_copies.push((idx, copy));
        }

        // Build a remapping closure: callee Local → caller Local.
        let remap = |l: Local| -> Local {
            if l == Local::RETURN {
                return destination.local;
            }
            let idx = l.0;
            if idx >= 1 && idx <= ic.arity {
                if let Some((_, copy)) = param_copies.iter().find(|(param, _)| *param == idx) {
                    return *copy;
                }
                // Param the callee only reads: the argument local is
                // the same value, so it needs no copy.
                return arg_locals[(idx - 1) as usize].unwrap_or(Local(idx));
            }
            // Temp: shift by (orig_local_count - (arity + 1)).
            let temp_idx = idx - ic.arity - 1;
            Local(orig_local_count + temp_idx)
        };

        let remapped: Vec<crate::ir::Statement> = ic
            .stmts
            .iter()
            .map(|stmt| remap_statement(stmt, &remap))
            .collect();

        // Replace the Call terminator with Goto and splice statements.
        let continuation = target.unwrap_or(BlockId(bi as u32 + 1));
        body.blocks[bi].stmts.extend(bindings);
        body.blocks[bi].stmts.extend(remapped);
        body.blocks[bi].terminator = Terminator::Goto {
            target: continuation,
        };
        bi += 1;
    }
}

fn remap_statement(
    stmt: &crate::ir::Statement,
    remap: &impl Fn(Local) -> Local,
) -> crate::ir::Statement {
    let StatementKind::Assign { place, rvalue } = &stmt.kind else {
        return stmt.clone();
    };
    let new_place = remap_place(place, remap);
    let new_rvalue = remap_rvalue(rvalue, remap);
    crate::ir::Statement {
        kind: StatementKind::Assign {
            place: new_place,
            rvalue: new_rvalue,
        },
        span: stmt.span,
    }
}

fn remap_place(place: &Place, remap: &impl Fn(Local) -> Local) -> Place {
    // A `Projection::Index(local)` carries a runtime index local that lives in
    // the callee's local space; it must be remapped into the caller's appended
    // locals like the place root, or the spliced access reads a colliding
    // caller local (`a[i]` in a small inlined callee read the wrong index).
    let projection = place
        .projection
        .iter()
        .map(|p| match p {
            Projection::Index(idx) => Projection::Index(remap(*idx)),
            other => other.clone(),
        })
        .collect();
    Place {
        local: remap(place.local),
        projection,
    }
}

fn remap_operand(op: &Operand, remap: &impl Fn(Local) -> Local) -> Operand {
    match op {
        Operand::Copy(place) => Operand::Copy(remap_place(place, remap)),
        other => other.clone(),
    }
}

fn remap_rvalue(rv: &Rvalue, remap: &impl Fn(Local) -> Local) -> Rvalue {
    match rv {
        Rvalue::Use(op) => Rvalue::Use(remap_operand(op, remap)),
        Rvalue::BinaryOp { op, lhs, rhs } => Rvalue::BinaryOp {
            op: *op,
            lhs: remap_operand(lhs, remap),
            rhs: remap_operand(rhs, remap),
        },
        Rvalue::UnaryOp { op, operand } => Rvalue::UnaryOp {
            op: *op,
            operand: remap_operand(operand, remap),
        },
        Rvalue::Cast { operand, target } => Rvalue::Cast {
            operand: remap_operand(operand, remap),
            target: *target,
        },
        Rvalue::Aggregate { kind, operands } => Rvalue::Aggregate {
            kind: kind.clone(),
            operands: operands.iter().map(|op| remap_operand(op, remap)).collect(),
        },
        Rvalue::Repeat { value, count } => Rvalue::Repeat {
            value: remap_operand(value, remap),
            count: *count,
        },
        Rvalue::Len(place) => Rvalue::Len(remap_place(place, remap)),
        Rvalue::Ref { place, mutable } => Rvalue::Ref {
            place: remap_place(place, remap),
            mutable: *mutable,
        },
        Rvalue::CallIntrinsic { name, args } => Rvalue::CallIntrinsic {
            name,
            args: args.iter().map(|op| remap_operand(op, remap)).collect(),
        },
        // No local operands to remap; the static is referenced by symbol.
        Rvalue::StaticLoad(_) => rv.clone(),
    }
}

/// A callee eligible for general inlining: a clone of its whole body
/// plus its weighted cost.
#[derive(Clone)]
struct GeneralCallee {
    body: Body,
    cost: usize,
}

/// Decides whether `body` may be inlined as a whole-CFG splice.
/// Requires a real (non-diverging) `Return` path, bounded cost, and no
/// `Drop` terminator: drop semantics depend on the callee's own scope
/// boundaries, so relocating a `Drop` into the caller is unsound.
fn try_build_general(body: &Body) -> Option<GeneralCallee> {
    if body.blocks.is_empty() {
        return None;
    }
    let cost = body_cost(body);
    // Register candidates up to the const-arg promotion ceiling; the
    // application pass admits the mid-window ones only at constant-fed
    // call sites.
    if cost > INLINE_CONST_ARG_COST_LIMIT {
        return None;
    }
    let mut has_return = false;
    for b in &body.blocks {
        match &b.terminator {
            Terminator::Return => has_return = true,
            Terminator::Drop { .. } => return None,
            _ => {}
        }
    }
    if !has_return {
        return None;
    }
    Some(GeneralCallee {
        body: body.clone(),
        cost,
    })
}

/// Inlines user-function call sites whose callee is a registered
/// `GeneralCallee` - splicing the callee's whole CFG so multi-block,
/// call-containing, and aggregate-returning callees flatten into the
/// caller. This is the strongest safe lever for JIT coverage: the JIT
/// compile-set BFS drops any body that calls an excluded body, so
/// dissolving the call edge promotes whole chains.
///
/// Each caller has a single weighted growth budget; a direct
/// self-recursive call is never inlined. Spliced blocks are rescanned
/// within the same caller (bounded by the budget), so a chain
/// `top -> mid -> lo` flattens in one pass.
pub fn inline_general(bodies: &mut [Body]) {
    if !inlining_enabled() {
        return;
    }
    let def_to_name = def_to_name_map(bodies);
    let mut callees: HashMap<String, GeneralCallee> = HashMap::new();
    for b in bodies.iter() {
        // A self-recursive callee would re-inline its own body one level per
        // splice into every caller, bounded only by the growth budget. Refuse
        // to register it so recursion stays a real call. (Caller
        // self-recursion is already skipped in `inline_general_into`.)
        let self_recursive = b.blocks.iter().any(|blk| {
            matches!(&blk.terminator, Terminator::Call { callee, .. }
                if callee_body_name(callee, &def_to_name).as_deref() == Some(b.name.as_str()))
        });
        if self_recursive {
            continue;
        }
        if let Some(gc) = try_build_general(b) {
            callees.insert(b.name.clone(), gc);
        }
    }
    if callees.is_empty() {
        return;
    }
    for body in bodies.iter_mut() {
        inline_general_into(body, &callees, &def_to_name);
    }
}

fn inline_general_into(
    body: &mut Body,
    callees: &HashMap<String, GeneralCallee>,
    def_to_name: &HashMap<u32, String>,
) {
    let mut budget = INLINE_CALLER_BUDGET;
    // The block list grows as callees are spliced; scanning the
    // appended blocks too flattens a call chain transitively, and the
    // shared `budget` (each splice costs >= 1) guarantees termination.
    let mut bi = 0;
    while bi < body.blocks.len() {
        let Terminator::Call {
            callee,
            args,
            destination,
            target,
        } = body.blocks[bi].terminator.clone()
        else {
            bi += 1;
            continue;
        };
        let Some(name) = callee_body_name(&callee, def_to_name) else {
            bi += 1;
            continue;
        };
        if name.as_str() == body.name.as_str() {
            bi += 1;
            continue;
        }
        let Some(gc) = callees.get(name.as_str()) else {
            bi += 1;
            continue;
        };
        let Some(continuation) = target else {
            bi += 1;
            continue;
        };
        if gc.body.arity as usize != args.len() {
            bi += 1;
            continue;
        }
        // Callees over the base limit are in the promotion window: inline
        // them only when a call-site argument is a constant. The constant
        // flows into the spliced body and the follow-up `const_fold`
        // collapses the now-constant-conditioned branches that make the
        // larger body worth pulling in.
        let promoted = gc.cost > INLINE_COST_LIMIT;
        if promoted && !args.iter().any(|a| matches!(a, Operand::Const(_))) {
            bi += 1;
            continue;
        }
        if gc.cost > budget {
            bi += 1;
            continue;
        }
        budget -= gc.cost;
        splice_callee(body, bi, &gc.body, &args, &destination, continuation);
        if promoted {
            const_fold(body);
        }
        bi += 1;
    }
}

/// Splices `callee` into `body` at `call_block`. Appends fresh copies
/// of every callee local, clones+remaps the callee's blocks, routes
/// each callee `Return` to a landing block that writes `destination`,
/// binds params via injected `param = Use(arg)` statements, and turns
/// the original call into a `Goto` to the callee entry.
fn splice_callee(
    body: &mut Body,
    call_block: usize,
    callee: &Body,
    args: &[Operand],
    destination: &Place,
    continuation: BlockId,
) {
    let base_local = body.locals.len() as u32;
    for decl in &callee.locals {
        body.locals.push(decl.clone());
    }
    let base_block = body.blocks.len() as u32;
    let remap_local = move |l: Local| Local(base_local + l.0);
    let remap_block = move |b: BlockId| BlockId(base_block + b.0);
    // Landing block sits after the callee's own blocks.
    let landing_id = BlockId(base_block + callee.blocks.len() as u32);

    for cb in &callee.blocks {
        let stmts = cb
            .stmts
            .iter()
            .map(|s| remap_statement_full(s, &remap_local))
            .collect();
        let terminator =
            remap_terminator_full(&cb.terminator, &remap_local, &remap_block, landing_id);
        body.blocks.push(BasicBlock {
            id: remap_block(cb.id),
            stmts,
            terminator,
            span: cb.span,
        });
    }

    body.blocks.push(BasicBlock {
        id: landing_id,
        stmts: vec![Statement {
            kind: StatementKind::Assign {
                place: destination.clone(),
                rvalue: Rvalue::Use(Operand::Copy(Place {
                    local: remap_local(Local::RETURN),
                    projection: Vec::new(),
                })),
            },
            span: callee.span,
        }],
        terminator: Terminator::Goto {
            target: continuation,
        },
        span: callee.span,
    });

    let entry = remap_block(callee.blocks[0].id);
    let bind: Vec<Statement> = (0..callee.arity)
        .map(|i| Statement {
            kind: StatementKind::Assign {
                place: Place {
                    local: remap_local(Local(i + 1)),
                    projection: Vec::new(),
                },
                rvalue: Rvalue::Use(args[i as usize].clone()),
            },
            span: callee.span,
        })
        .collect();
    body.blocks[call_block].stmts.extend(bind);
    body.blocks[call_block].terminator = Terminator::Goto { target: entry };
}

/// Full statement remap that, unlike [`remap_statement`], also remaps
/// `StorageLive`/`StorageDead`/`SetDiscriminant` locals and every
/// `Projection::Index` local - required for general inlining where the
/// callee may carry storage annotations and indexed accesses.
fn remap_statement_full(stmt: &Statement, remap: &impl Fn(Local) -> Local) -> Statement {
    let kind = match &stmt.kind {
        StatementKind::Assign { place, rvalue } => StatementKind::Assign {
            place: remap_place_full(place, remap),
            rvalue: remap_rvalue_full(rvalue, remap),
        },
        StatementKind::StorageLive(l) => StatementKind::StorageLive(remap(*l)),
        StatementKind::StorageDead(l) => StatementKind::StorageDead(remap(*l)),
        StatementKind::SetDiscriminant { place, variant } => StatementKind::SetDiscriminant {
            place: remap_place_full(place, remap),
            variant: *variant,
        },
        StatementKind::StaticStore { target, value } => StatementKind::StaticStore {
            target: target.clone(),
            value: remap_operand_full(value, remap),
        },
        StatementKind::IterSource {
            dst,
            source_kind,
            source,
            item_ty,
            ownership,
        } => StatementKind::IterSource {
            dst: remap_place_full(dst, remap),
            source_kind: *source_kind,
            source: remap_operand_full(source, remap),
            item_ty: *item_ty,
            ownership: *ownership,
        },
        StatementKind::IterAdapter {
            dst,
            adapter_kind,
            upstream,
            closure_or_arg,
            item_ty,
        } => StatementKind::IterAdapter {
            dst: remap_place_full(dst, remap),
            adapter_kind: *adapter_kind,
            upstream: remap_place_full(upstream, remap),
            closure_or_arg: closure_or_arg.as_ref().map(|arg| remap_operand_full(arg, remap)),
            item_ty: *item_ty,
        },
        StatementKind::IterNext {
            dst_option,
            iter_place,
            item_ty,
        } => StatementKind::IterNext {
            dst_option: remap_place_full(dst_option, remap),
            iter_place: remap_place_full(iter_place, remap),
            item_ty: *item_ty,
        },
        StatementKind::Nop => StatementKind::Nop,
    };
    Statement {
        kind,
        span: stmt.span,
    }
}

/// Remaps a place's root local AND any `Projection::Index` local, which
/// [`remap_place`] does not - needed for indexed accesses in a spliced
/// callee.
fn remap_place_full(place: &Place, remap: &impl Fn(Local) -> Local) -> Place {
    let projection = place
        .projection
        .iter()
        .map(|p| match p {
            Projection::Index(idx) => Projection::Index(remap(*idx)),
            other => other.clone(),
        })
        .collect();
    Place {
        local: remap(place.local),
        projection,
    }
}

/// Operand remap that walks `Index` projection locals (via
/// [`remap_place_full`]).
fn remap_operand_full(op: &Operand, remap: &impl Fn(Local) -> Local) -> Operand {
    match op {
        Operand::Copy(place) => Operand::Copy(remap_place_full(place, remap)),
        other => other.clone(),
    }
}

/// Rvalue remap that walks `Index` projection locals in every operand
/// and place (via [`remap_place_full`] / [`remap_operand_full`]).
fn remap_rvalue_full(rv: &Rvalue, remap: &impl Fn(Local) -> Local) -> Rvalue {
    match rv {
        Rvalue::Use(op) => Rvalue::Use(remap_operand_full(op, remap)),
        Rvalue::BinaryOp { op, lhs, rhs } => Rvalue::BinaryOp {
            op: *op,
            lhs: remap_operand_full(lhs, remap),
            rhs: remap_operand_full(rhs, remap),
        },
        Rvalue::UnaryOp { op, operand } => Rvalue::UnaryOp {
            op: *op,
            operand: remap_operand_full(operand, remap),
        },
        Rvalue::Cast { operand, target } => Rvalue::Cast {
            operand: remap_operand_full(operand, remap),
            target: *target,
        },
        Rvalue::Aggregate { kind, operands } => Rvalue::Aggregate {
            kind: kind.clone(),
            operands: operands
                .iter()
                .map(|op| remap_operand_full(op, remap))
                .collect(),
        },
        Rvalue::Repeat { value, count } => Rvalue::Repeat {
            value: remap_operand_full(value, remap),
            count: *count,
        },
        Rvalue::Len(place) => Rvalue::Len(remap_place_full(place, remap)),
        Rvalue::Ref { place, mutable } => Rvalue::Ref {
            place: remap_place_full(place, remap),
            mutable: *mutable,
        },
        Rvalue::CallIntrinsic { name, args } => Rvalue::CallIntrinsic {
            name,
            args: args
                .iter()
                .map(|op| remap_operand_full(op, remap))
                .collect(),
        },
        // No local operands to remap; the static is referenced by symbol.
        Rvalue::StaticLoad(_) => rv.clone(),
    }
}

/// If `stmt` is a whole-local `gos_rt_rc_release(S)` call, return `S`.
fn whole_release_local(stmt: &Statement) -> Option<Local> {
    let StatementKind::Assign {
        rvalue: Rvalue::CallIntrinsic { name, args },
        ..
    } = &stmt.kind
    else {
        return None;
    };
    if *name != "gos_rt_rc_release" || args.len() != 1 {
        return None;
    }
    match &args[0] {
        Operand::Copy(p) if p.projection.is_empty() => Some(p.local),
        _ => None,
    }
}

/// If `stmt` is `D = gos_rc_alloc(size, meta)` / `gos_rc_alloc_tagged(...)` to a
/// plain local, return `D`. Both lower to a header-ed payload whose
/// discriminant is applied by a later `gos_enum_set_disc` / `gos_enum_tag`
/// statement (untouched by the rewrite), so one reuse intrinsic serves both: a
/// reused or freshly-allocated block is filled and tagged identically.
fn rc_alloc_dest(stmt: &Statement) -> Option<Local> {
    let StatementKind::Assign {
        place,
        rvalue: Rvalue::CallIntrinsic { name, args },
    } = &stmt.kind
    else {
        return None;
    };
    if !matches!(*name, "gos_rc_alloc" | "gos_rc_alloc_tagged")
        || args.len() != 2
        || !place.projection.is_empty()
    {
        return None;
    }
    Some(place.local)
}

/// True when `s` is read anywhere in `body` other than by the statement at
/// (`bi`, `ri`). A bare assignment to `s` writes it rather than reading it;
/// every other mention - a projected write, an operand, a terminator - can
/// reach the value, so recycling its block ahead of that mention would hand
/// the reader a different object.
fn reads_local_elsewhere(body: &Body, s: Local, bi: usize, ri: usize) -> bool {
    body.blocks.iter().enumerate().any(|(b, block)| {
        block
            .stmts
            .iter()
            .enumerate()
            .any(|(i, stmt)| {
                !(b == bi && i == ri)
                    && stmt_mentions_local(stmt, s)
                    && !stmt_writes_bare(stmt, s)
            })
            || term_mentions_local(&block.terminator, s)
    })
}

/// Perceus reuse pairing (`GOS_RC_NO_REUSE` disables it). Within a block, pair a
/// whole-local `gos_rt_rc_release(S)` with a later same-type
/// `D = gos_rc_alloc(size, meta)` constructor and rewrite the pair so S's block
/// is recycled in place:
///
/// ```text
///   gos_rt_rc_release(S)            token = gos_rt_rc_drop_reuse(S)
///   ...                       =>    ...
///   D = gos_rc_alloc(sz, m)         D = gos_rc_alloc_reuse(token, sz, m)
/// ```
///
/// Sound by construction: `gos_rt_rc_drop_reuse` does exactly what
/// `gos_rt_rc_release` does to S and its children (releasing them, cascading),
/// and only RETURNS S's block instead of freeing it - and only when S is the
/// unique thread-local, weak-free, unbuffered owner; otherwise it frees
/// normally and returns null, so `gos_rc_alloc_reuse` falls back to a fresh
/// allocation. A mis-pairing can therefore never corrupt, only forgo the reuse.
/// The pairing requires S unreferenced between the release and the constructor
/// (so the release simply moves to a drop-reuse at the constructor) and S dead
/// afterwards (guaranteed: the drop pass releases at last use). Covers both the
/// header-discriminant (`gos_rc_alloc`) and tagged (`gos_rc_alloc_tagged`)
/// enum constructors - the discriminant is applied by a separate later
/// statement, so one reuse intrinsic serves both. Region objects are skipped at
/// runtime (`drop_reuse` returns null for them). Compiled-tier only - the
/// bytecode VM never consumes this MIR.
// `ri` indexes both `used_release` and the block's statements and feeds
// `pairs`, so an index loop is the clear form here, not an iterator.
#[allow(clippy::needless_range_loop)]
// A cohesive three-phase pass (find pairs / mint tokens / rebuild block); it
// reads most clearly as one function.
#[allow(clippy::too_many_lines)]
pub(crate) fn insert_rc_reuse(body: &mut Body, tcx: &TyCtxt) {
    for bi in 0..body.blocks.len() {
        let n = body.blocks[bi].stmts.len();
        if n < 2 {
            continue;
        }
        // Phase A (read-only): find (release_idx, ctor_idx, S, type) pairs.
        let mut used_release = vec![false; n];
        let mut pairs: Vec<(usize, usize, Local, Ty)> = Vec::new();
        for ci in 0..n {
            let Some(d_local) = rc_alloc_dest(&body.blocks[bi].stmts[ci]) else {
                continue;
            };
            if (d_local.0 as usize) >= body.locals.len() {
                continue;
            }
            let dty = body.locals[d_local.0 as usize].ty;
            if !tcx.is_rc_managed(dty) {
                continue;
            }
            // The constructor must not itself reference a reuse candidate (it
            // names only the dest, size, and meta), so a stale candidate value
            // is never read by the construction.
            // Closest unused whole-release of the same type (not D), on EITHER
            // side of the constructor, with the candidate unreferenced in the
            // span between - so the release simply moves to a drop-reuse right
            // before the constructor. Releases land after the construct in the
            // common reassignment shape (`x = T::new(..)` releases the old `x`
            // after building the new value), so both directions matter.
            let mut best: Option<usize> = None;
            let mut best_dist = usize::MAX;
            for ri in 0..body.blocks[bi].stmts.len() {
                if ri == ci || used_release[ri] {
                    continue;
                }
                let Some(s_local) = whole_release_local(&body.blocks[bi].stmts[ri]) else {
                    continue;
                };
                if s_local == d_local || (s_local.0 as usize) >= body.locals.len() {
                    continue;
                }
                let sdecl = &body.locals[s_local.0 as usize];
                if sdecl.ty != dty || sdecl.region {
                    continue;
                }
                // Where the drop-reuse lands (just before the constructor)
                // versus where the release was determines what must be clear:
                //
                // * release BEFORE the constructor: the release only moves
                //   LATER, which can only extend a child's life, so just the
                //   span between is checked.
                // * release AFTER the constructor: the release moves EARLIER,
                //   which could free a child a constructor argument borrows
                //   (e.g. `x = T::new(x.child, ..)`), so S must be untouched
                //   from the block start through the whole window - excluding
                //   exactly that aliasing shape.
                let clash = if ri < ci {
                    (ri + 1..ci).any(|k| stmt_mentions_local(&body.blocks[bi].stmts[k], s_local))
                        || stmt_mentions_local(&body.blocks[bi].stmts[ci], s_local)
                } else {
                    // Moving the release earlier can outrun a share that is
                    // minted later - the store that hands S to the object
                    // being built retains it AFTER the constructor - and an
                    // alias of S reached through another local is invisible
                    // to a scan of this block. Pair only when nothing else in
                    // the body reads S, so no reader can hold its block. S
                    // must also already hold its value where the drop-reuse
                    // lands, so a definition of S between the constructor and
                    // the release rules the pairing out.
                    reads_local_elsewhere(body, s_local, bi, ri)
                        || (ci..ri).any(|k| {
                            stmt_writes_bare(&body.blocks[bi].stmts[k], s_local)
                        })
                };
                if clash {
                    continue;
                }
                let dist = ci.abs_diff(ri);
                if dist < best_dist {
                    best_dist = dist;
                    best = Some(ri);
                }
            }
            if let Some(ri) = best {
                let s_local = whole_release_local(&body.blocks[bi].stmts[ri]).unwrap();
                used_release[ri] = true;
                pairs.push((ri, ci, s_local, dty));
            }
        }
        if pairs.is_empty() {
            continue;
        }
        // Phase B (mint token locals; typed as the constructor's RC type so they
        // lower to pointers, and minted after the drop passes so nothing
        // releases them).
        let mut ctor_to_token: HashMap<usize, (Local, Local)> = HashMap::new();
        let mut release_remove: std::collections::HashSet<usize> = std::collections::HashSet::new();
        for &(ri, ci, s_local, dty) in &pairs {
            let token = Local(u32::try_from(body.locals.len()).expect("local overflow"));
            body.locals.push(LocalDecl {
                ty: dty,
                debug_name: None,
                mutable: false,
                region: false,
            });
            ctor_to_token.insert(ci, (token, s_local));
            release_remove.insert(ri);
        }
        // Phase C (rebuild the block).
        let orig = std::mem::take(&mut body.blocks[bi].stmts);
        let mut out: Vec<Statement> = Vec::with_capacity(orig.len() + pairs.len());
        for (idx, stmt) in orig.into_iter().enumerate() {
            if release_remove.contains(&idx) {
                continue;
            }
            if let Some(&(token, s_local)) = ctor_to_token.get(&idx) {
                let span = stmt.span;
                out.push(Statement {
                    kind: StatementKind::Assign {
                        place: Place::local(token),
                        rvalue: Rvalue::CallIntrinsic {
                            name: "gos_rt_rc_drop_reuse",
                            args: vec![Operand::Copy(Place::local(s_local))],
                        },
                    },
                    span,
                });
                let StatementKind::Assign {
                    place,
                    rvalue: Rvalue::CallIntrinsic { args, .. },
                } = stmt.kind
                else {
                    unreachable!("ctor_to_token only indexes gos_rc_alloc statements");
                };
                let mut new_args = Vec::with_capacity(3);
                new_args.push(Operand::Copy(Place::local(token)));
                new_args.extend(args); // original [size, meta]
                out.push(Statement {
                    kind: StatementKind::Assign {
                        place,
                        rvalue: Rvalue::CallIntrinsic {
                            name: "gos_rc_alloc_reuse",
                            args: new_args,
                        },
                    },
                    span,
                });
                continue;
            }
            out.push(stmt);
        }
        body.blocks[bi].stmts = out;
    }
}

/// Remaps a terminator's locals and block targets; `Return` becomes a
/// `Goto` to the landing block.
fn remap_terminator_full(
    t: &Terminator,
    remap_local: &impl Fn(Local) -> Local,
    remap_block: &impl Fn(BlockId) -> BlockId,
    landing: BlockId,
) -> Terminator {
    match t {
        Terminator::Return => Terminator::Goto { target: landing },
        Terminator::Goto { target } => Terminator::Goto {
            target: remap_block(*target),
        },
        Terminator::SwitchInt {
            discriminant,
            arms,
            default,
        } => Terminator::SwitchInt {
            discriminant: remap_operand_full(discriminant, remap_local),
            arms: arms.iter().map(|(v, b)| (*v, remap_block(*b))).collect(),
            default: remap_block(*default),
        },
        Terminator::Call {
            callee,
            args,
            destination,
            target,
        } => Terminator::Call {
            callee: remap_operand_full(callee, remap_local),
            args: args
                .iter()
                .map(|a| remap_operand_full(a, remap_local))
                .collect(),
            destination: remap_place_full(destination, remap_local),
            target: target.map(remap_block),
        },
        Terminator::Assert {
            cond,
            expected,
            msg,
            target,
        } => Terminator::Assert {
            cond: remap_operand_full(cond, remap_local),
            expected: *expected,
            msg: msg.clone(),
            target: remap_block(*target),
        },
        Terminator::Unreachable => Terminator::Unreachable,
        Terminator::Panic { message } => Terminator::Panic {
            message: message.clone(),
        },
        // Excluded by `try_build_general`; kept total to satisfy the
        // compiler without relocating drop semantics into the caller.
        Terminator::Drop { .. } => Terminator::Unreachable,
    }
}

/// Identifies which locals hold aggregate types
/// (`Array` / `Tuple` / `Adt`). Aggregates have storage-identity
/// semantics: a `&mut _X` borrow is bound to `_X`'s slot, not to
/// the rvalue that flowed into it. Any optimisation that would
/// alias two aggregate locals (copy propagation forwarding
/// `_X -> _Y`, GVN/CSE folding two equal aggregate constructions
/// into one local, DCE dropping a write that "looks dead" but
/// whose slot a later borrow points at) must consult this map and
/// bail on aggregate-typed locals.
///
/// Factored out of [`copy_propagate`] so [`dead_store_elim`] and
/// any future GVN/CSE pass can share one source of truth - the
/// previous one-pass-only encoding had been a copy-prop fix that
/// the audit flagged as too narrow.
pub(crate) fn aggregate_locals(body: &Body, tcx: &TyCtxt) -> Vec<bool> {
    body.locals
        .iter()
        .map(|decl| {
            matches!(
                tcx.kind(decl.ty),
                Some(TyKind::Array { .. } | TyKind::Tuple(_) | TyKind::Adt { .. })
            )
        })
        .collect()
}
