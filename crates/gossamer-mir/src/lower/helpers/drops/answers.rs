//! Values a body answers or discards, and the shares they carry.

use super::*;

/// True when nothing looks at what a call answered.
///
/// A `let _ = m.insert(..)` binds the answer to a name no one reads, so the
/// destination's only reader is that copy. Following the one hop keeps the
/// discarded-answer form the same whether the call site names the answer or
/// not.
pub(super) fn answer_is_discarded(
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
pub(super) fn copies_string_key(name: &str) -> bool {
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
                | "gos_rt_result_unwrap_or_map"
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

/// Moves each passthrough share to the front of its block.
///
/// The share is taken at the head of the block a passthrough combinator's call
/// continues in, where the answer is already written. Later passes place the
/// receiver's last-use release at that same head, and a release that runs first
/// would free the blob the answer still names.
pub(crate) fn lead_passthrough_shares(body: &mut Body) {
    for block in &mut body.blocks {
        let (mut leading, rest): (Vec<Statement>, Vec<Statement>) =
            std::mem::take(&mut block.stmts)
                .into_iter()
                .partition(|stmt| {
                    matches!(&stmt.kind, StatementKind::Assign {
                    rvalue: Rvalue::CallIntrinsic { name, .. },
                    ..
                } if *name == "gos_rt_option_slot_retain_ok")
                });
        leading.extend(rest);
        block.stmts = leading;
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
