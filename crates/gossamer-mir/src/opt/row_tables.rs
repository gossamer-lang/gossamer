// ---------------------------------------------------------------------------
// Row header tables for nested vectors read inside a loop nest.
// ---------------------------------------------------------------------------

/// One loop nest whose reads of the rows of a nested vector go through a table.
struct RowTablePlan {
    /// The outer loop's header.
    header: usize,
    /// The block that enters the outer loop.
    entry: usize,
    /// The local the nested vector is reached from.
    root: Local,
    /// The projections from `root` to the nested vector.
    prefix: Vec<Projection>,
    /// The type of the nested vector place.
    nested_ty: Ty,
    /// Every row read in the nest: `(block, statement, row local, index)`,
    /// with no statement for a read the block's terminator makes.
    reads: Vec<(usize, Option<usize>, Local, Operand)>,
}

/// Reads the rows of a nested vector through a table of row headers built once
/// before the loop nest that reads them.
///
/// `grid[ny][nx]` loads the row's handle out of the outer vector and then the
/// row's length and data pointer out of the header that handle names, one
/// dependent load per access that no loop-invariant hoisting reaches when `ny`
/// comes from data. A table holding a copy of each row's header prefix puts
/// every row's length and data pointer at a fixed stride from one base, so the
/// row's address is arithmetic and the header load is gone.
///
/// The copies stay exact while nothing resizes a row or changes the outer
/// vector, so a nest qualifies only when:
/// - it is a counted loop containing another counted loop, whose single entry
///   block builds the table once for all the rows the nest reads;
/// - every mention of the root local in the body is a read, the root is never
///   borrowed or goroutine-shared, and no call inside the nest receives it;
/// - every read of a row inside the nest binds a local used only to ask the
///   row's length, an element's address, or a scalar element's value - reads
///   that touch no header field past the copied prefix - and a copy of the
///   outer vector used in the nest only asks its length;
/// - no copy of the root's aggregate that holds the nested vector is taken.
///
/// Each row read becomes the same bounds check the indexed read performs,
/// followed by the table element's address, which the row's element accesses
/// read exactly as they read a row header.
pub(crate) fn tabulate_nested_row_reads(body: &mut Body, tcx: &TyCtxt) {
    let Some(types) = RowTableTypes::interned(tcx) else {
        return;
    };
    while let Some(plan) = row_table_plan(body, tcx) {
        apply_row_table_plan(body, &types, plan);
    }
}

/// The types the table rewrite gives its locals.
#[derive(Clone, Copy)]
struct RowTableTypes {
    int64: Ty,
    uint64: Ty,
    boolean: Ty,
    unit: Ty,
    /// `Vec<(i64, i64, i64, i64)>`: one header prefix per element.
    table: Ty,
}

impl RowTableTypes {
    fn header_kind(int64: Ty) -> TyKind {
        TyKind::Tuple(vec![int64; 4])
    }

    fn interned(tcx: &TyCtxt) -> Option<Self> {
        let int64 = tcx.interned(&TyKind::Int(gossamer_types::IntTy::I64))?;
        let header = tcx.interned(&Self::header_kind(int64))?;
        Some(Self {
            int64,
            uint64: tcx.interned(&TyKind::Int(gossamer_types::IntTy::U64))?,
            boolean: tcx.interned(&TyKind::Bool)?,
            unit: tcx.unit_interned()?,
            table: tcx.interned(&TyKind::Vec(header))?,
        })
    }
}

/// Interns the types [`tabulate_nested_row_reads`] needs, which the
/// optimiser, holding the context immutably, can then look up.
pub(crate) fn intern_row_table_types(tcx: &mut TyCtxt) {
    let i64_ty = tcx.int_ty(gossamer_types::IntTy::I64);
    let _ = tcx.int_ty(gossamer_types::IntTy::U64);
    let _ = tcx.bool_ty();
    let _ = tcx.unit();
    let header_ty = tcx.intern(RowTableTypes::header_kind(i64_ty));
    let _ = tcx.intern(TyKind::Vec(header_ty));
}

/// The type of `root` projected through `projection`, or `None` when a step
/// cannot be followed.
fn projected_place_ty(body: &Body, tcx: &TyCtxt, root: Local, projection: &[Projection]) -> Option<Ty> {
    let mut ty = body.locals.get(root.0 as usize)?.ty;
    for step in projection {
        let mut base = ty;
        while let TyKind::Ref { inner, .. } = tcx.kind_of(base) {
            base = *inner;
        }
        ty = match step {
            Projection::Deref => match tcx.kind_of(ty) {
                TyKind::Ref { inner, .. } => *inner,
                _ => return None,
            },
            Projection::Field(index) => match tcx.kind_of(base) {
                TyKind::Tuple(elems) => *elems.get(*index as usize)?,
                TyKind::Adt { def, .. } => *tcx.struct_field_tys(*def)?.get(*index as usize)?,
                _ => return None,
            },
            Projection::Index(_) => match tcx.kind_of(base) {
                TyKind::Vec(elem) => *elem,
                _ => return None,
            },
            Projection::Downcast(_) | Projection::Discriminant => return None,
        };
    }
    Some(ty)
}

/// Whether `ty`, behind any references, is a `Vec` whose elements are `Vec`s.
fn is_nested_vec(tcx: &TyCtxt, ty: Ty) -> bool {
    let mut ty = ty;
    while let TyKind::Ref { inner, .. } = tcx.kind_of(ty) {
        ty = *inner;
    }
    matches!(tcx.kind_of(ty), TyKind::Vec(elem) if matches!(tcx.kind_of(*elem), TyKind::Vec(_)))
}

fn row_table_plan(body: &Body, tcx: &TyCtxt) -> Option<RowTablePlan> {
    let succs: Vec<Vec<usize>> = body
        .blocks
        .iter()
        .map(|b| successor_indices(&b.terminator))
        .collect();
    let mut nests: Vec<(usize, Vec<usize>)> = Vec::new();
    for h in 0..body.blocks.len() {
        let Some(header) = recognise_counted_header(&body.blocks[h]) else {
            continue;
        };
        let Some((region, _)) = counted_loop_region(body, &succs, h, header.body_entry, header.exit)
        else {
            continue;
        };
        if region
            .iter()
            .any(|&b| recognise_counted_header(&body.blocks[b]).is_some())
        {
            nests.push((h, region));
        }
    }
    // The widest nest first, so one table serves every loop inside it.
    nests.sort_by_key(|(_, region)| std::cmp::Reverse(region.len()));
    let share = crate::ownership::ShareFacts::compute(body);
    for (h, region) in nests {
        let in_loop = |b: usize| b == h || region.contains(&b);
        let Some(entry) = loop_entry_block(body, &succs, h, &in_loop) else {
            continue;
        };
        let mut groups: Vec<RowTablePlan> = Vec::new();
        for &b in &region {
            for (si, stmt) in body.blocks[b].stmts.iter().enumerate() {
                let StatementKind::Assign {
                    place,
                    rvalue: Rvalue::Use(Operand::Copy(src)),
                } = &stmt.kind
                else {
                    continue;
                };
                let Some((Projection::Index(index), prefix)) = src.projection.split_last() else {
                    continue;
                };
                if !place.projection.is_empty()
                    || !prefix.iter().all(|p| matches!(p, Projection::Field(_) | Projection::Deref))
                {
                    continue;
                }
                let Some(nested_ty) = projected_place_ty(body, tcx, src.local, prefix) else {
                    continue;
                };
                if !is_nested_vec(tcx, nested_ty) {
                    continue;
                }
                let read = (b, Some(si), place.local, Operand::Copy(Place::local(*index)));
                add_row_read(&mut groups, (h, entry), (src.local, prefix), nested_ty, read);
            }
            // A row of a nested vector held in a local is read through the
            // element getter, which answers the row's handle.
            if let Terminator::Call {
                callee: Operand::Const(ConstValue::Str(name)),
                args,
                destination,
                target: Some(_),
            } = &body.blocks[b].terminator
                && name == "gos_rt_vec_get_i64"
                && let [Operand::Copy(src), index] = args.as_slice()
                && destination.projection.is_empty()
                && !operand_mentions_local(index, src.local)
                && src
                    .projection
                    .iter()
                    .all(|p| matches!(p, Projection::Field(_) | Projection::Deref))
                && let Some(nested_ty) = projected_place_ty(body, tcx, src.local, &src.projection)
                && is_nested_vec(tcx, nested_ty)
            {
                let read = (b, None, destination.local, index.clone());
                add_row_read(&mut groups, (h, entry), (src.local, &src.projection), nested_ty, read);
            }
        }
        for group in groups {
            if !share.is_goroutine_shared(group.root)
                && row_table_plan_holds(body, &group, &in_loop)
            {
                return Some(group);
            }
        }
    }
    None
}

/// Adds a row read to the group for its nested vector, opening the group.
fn add_row_read(
    groups: &mut Vec<RowTablePlan>,
    (header, entry): (usize, usize),
    (root, prefix): (Local, &[Projection]),
    nested_ty: Ty,
    read: (usize, Option<usize>, Local, Operand),
) {
    match groups.iter_mut().find(|g| g.root == root && g.prefix == prefix) {
        Some(group) => group.reads.push(read),
        None => groups.push(RowTablePlan {
            header,
            entry,
            root,
            prefix: prefix.to_vec(),
            nested_ty,
            reads: vec![read],
        }),
    }
}

/// The runtime calls that read a vector's length or elements and keep no
/// handle to it.
fn reads_vector_only(name: &str) -> bool {
    matches!(name, "gos_rt_vec_len" | "gos_rt_vec_get_ptr" | "gos_rt_vec_get_i64")
}

/// Whether nothing in `body` can resize a row of the plan's nested vector or
/// change the vector itself while the nest runs, by the rules
/// [`tabulate_nested_row_reads`] states.
fn row_table_plan_holds(body: &Body, plan: &RowTablePlan, in_loop: &dyn Fn(usize) -> bool) -> bool {
    root_uses_only_read(body, plan, in_loop)
        .is_some_and(|outer_copies| row_locals_only_read(body, plan, in_loop, &outer_copies))
}

/// The locals holding a copy of the nested vector itself, when every use of the
/// root in `body` only reads it; `None` when some use could change or keep it.
fn root_uses_only_read(
    body: &Body,
    plan: &RowTablePlan,
    in_loop: &dyn Fn(usize) -> bool,
) -> Option<Vec<Local>> {
    let root = plan.root;
    let related = |proj: &[Projection]| proj.starts_with(&plan.prefix) || plan.prefix.starts_with(proj);
    let mut outer_copies: Vec<Local> = Vec::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        for (si, stmt) in block.stmts.iter().enumerate() {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                if stmt_mentions_local(stmt, root) {
                    return None;
                }
                continue;
            };
            if place.local == root || matches!(rvalue, Rvalue::Ref { place: p, .. } if p.local == root) {
                return None;
            }
            if !rvalue_mentions_local(rvalue, root) {
                continue;
            }
            if let Rvalue::Use(Operand::Copy(src)) = rvalue
                && src.local == root
                && place.projection.is_empty()
            {
                let proj = src.projection.as_slice();
                if proj == plan.prefix.as_slice() {
                    outer_copies.push(place.local);
                } else if proj.len() == plan.prefix.len() + 1 && proj.starts_with(&plan.prefix) {
                    let tabulated = plan.reads.iter().any(|(b, s, _, _)| (*b, *s) == (bi, Some(si)));
                    if in_loop(bi) && !tabulated
                        || !in_loop(bi) && local_mentioned_in(body, place.local, in_loop)
                    {
                        return None;
                    }
                } else if plan.prefix.starts_with(proj) {
                    return None;
                }
                continue;
            }
            let mut reaches = false;
            for_each_rvalue_operand_place(rvalue, &mut |p: &Place| {
                reaches |= p.local == root && related(&p.projection);
            });
            if reaches {
                return None;
            }
        }
        // A call inside the nest could change the rows through the root, and
        // one anywhere else could keep a name for the nested vector a call
        // inside the nest then changes it through. The length is fixed while
        // the nest changes no row and not the vector.
        let tabulated = plan.reads.iter().any(|(b, s, _, _)| *b == bi && s.is_none());
        let asks_length = matches!(&block.terminator, Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            args,
            destination,
            ..
        } if name == "gos_rt_vec_len"
            && destination.local != root
            && matches!(args.as_slice(), [Operand::Copy(p)] if p.local == root && p.projection == plan.prefix));
        if term_mentions_local(&block.terminator, root)
            && !tabulated
            && !asks_length
            && (in_loop(bi)
                || (term_reaches_nested(&block.terminator, root, &related)
                    && !reads_nested_outside(body, &block.terminator, root, in_loop)))
        {
            return None;
        }
    }
    Some(outer_copies)
}

/// Whether every row local the plan tabulates holds only rows the table
/// answers for and is used only to read them, and every copy of the nested
/// vector the nest uses only asks its length.
fn row_locals_only_read(
    body: &Body,
    plan: &RowTablePlan,
    in_loop: &dyn Fn(usize) -> bool,
    outer_copies: &[Local],
) -> bool {
    let root = plan.root;
    let only_calls = |local: Local, allowed: &[&str]| {
        body.blocks.iter().enumerate().all(|(bi, block)| {
            let stmts_ok = block.stmts.iter().all(|stmt| match &stmt.kind {
                StatementKind::Assign { place, rvalue } if place.local == local => {
                    place.projection.is_empty()
                        && (matches!(rvalue, Rvalue::Use(Operand::Const(ConstValue::Int(0))))
                            || matches!(rvalue, Rvalue::Use(Operand::Copy(src)) if src.local == root))
                }
                _ => !stmt_mentions_local(stmt, local),
            });
            let term_ok = match &block.terminator {
                Terminator::Call { destination, .. }
                    if destination.local == local
                        && plan.reads.iter().any(|(b, s, r, _)| *b == bi && s.is_none() && *r == local) =>
                {
                    true
                }
                Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(name)),
                    args,
                    destination,
                    ..
                } if args.first().is_some_and(|a| matches!(a, Operand::Copy(p) if p.local == local && p.projection.is_empty())) => {
                    in_loop(bi)
                        && allowed.contains(&name.as_str())
                        && destination.local != local
                        && !args[1..].iter().any(|a| operand_mentions_local(a, local))
                }
                // A failing bounds check reads the row's length to report it.
                Terminator::Assert {
                    cond,
                    msg:
                        crate::ir::AssertMessage::BoundsCheck {
                            index,
                            seq: Operand::Copy(p),
                        },
                    ..
                } if p.local == local && p.projection.is_empty() => {
                    !operand_mentions_local(cond, local) && !operand_mentions_local(index, local)
                }
                other => !term_mentions_local(other, local),
            };
            stmts_ok && term_ok
        })
    };
    // Every value a row local holds is a row the table answers for.
    let defs_are_reads = |local: Local| {
        body.blocks.iter().enumerate().all(|(bi, block)| {
            let stmts_ok = block.stmts.iter().enumerate().all(|(si, s)| match &s.kind {
                StatementKind::Assign { place, rvalue } if place.local == local => {
                    matches!(rvalue, Rvalue::Use(Operand::Const(ConstValue::Int(0))))
                        || plan.reads.iter().any(|(b, st, r, _)| (*b, *st, *r) == (bi, Some(si), local))
                }
                _ => true,
            });
            let term_ok = !term_writes_bare(&block.terminator, local)
                || plan.reads.iter().any(|(b, st, r, _)| (*b, *st, *r) == (bi, None, local));
            stmts_ok && term_ok
        })
    };
    plan.reads.iter().all(|&(_, _, r, _)| {
        !outer_copies.contains(&r)
            && defs_are_reads(r)
            && only_calls(r, &["gos_rt_vec_len", "gos_rt_vec_get_ptr", "gos_rt_vec_get_i64"])
    }) && outer_copies
        .iter()
        .all(|&x| !local_mentioned_in(body, x, in_loop) || only_calls(x, &["gos_rt_vec_len"]))
}

/// Whether a terminator outside the nest only reads the nested vector's length
/// or one of its rows into a local the nest never mentions.
fn reads_nested_outside(
    body: &Body,
    term: &Terminator,
    root: Local,
    in_loop: &dyn Fn(usize) -> bool,
) -> bool {
    let Terminator::Call {
        callee: Operand::Const(ConstValue::Str(name)),
        args,
        destination,
        ..
    } = term
    else {
        return false;
    };
    reads_vector_only(name)
        && matches!(args.first(), Some(Operand::Copy(p)) if p.local == root)
        && !args[1..].iter().any(|a| operand_mentions_local(a, root))
        && destination.local != root
        && !local_mentioned_in(body, destination.local, in_loop)
}

/// Whether a terminator operand names `root` itself, the nested vector, or a
/// place inside it.
fn term_reaches_nested(term: &Terminator, root: Local, related: &dyn Fn(&[Projection]) -> bool) -> bool {
    let reaches = |op: &Operand| matches!(op, Operand::Copy(p) if p.local == root && related(&p.projection));
    match term {
        Terminator::Call { callee, args, destination, .. } => {
            reaches(callee) || args.iter().any(reaches) || destination.local == root
        }
        Terminator::SwitchInt { discriminant, .. } => reaches(discriminant),
        Terminator::Assert { cond, msg, .. } => reaches(cond) || msg.operands().any(reaches),
        Terminator::Drop { place, .. } => place.local == root,
        Terminator::Goto { .. }
        | Terminator::Return
        | Terminator::Unreachable
        | Terminator::Panic { .. } => false,
    }
}

/// Whether any statement or terminator in the blocks `blocks` selects mentions
/// `local`.
fn local_mentioned_in(body: &Body, local: Local, blocks: &dyn Fn(usize) -> bool) -> bool {
    body.blocks.iter().enumerate().any(|(bi, block)| {
        blocks(bi)
            && (block.stmts.iter().any(|s| stmt_mentions_local(s, local))
                || term_mentions_local(&block.terminator, local))
    })
}

/// Applies `f` to every place an rvalue reads through an operand.
fn for_each_rvalue_operand_place(rvalue: &Rvalue, f: &mut impl FnMut(&Place)) {
    let mut op = |o: &Operand| {
        if let Operand::Copy(p) = o {
            f(p);
        }
    };
    match rvalue {
        Rvalue::Use(o) | Rvalue::UnaryOp { operand: o, .. } | Rvalue::Cast { operand: o, .. } => op(o),
        Rvalue::BinaryOp { lhs, rhs, .. } => {
            op(lhs);
            op(rhs);
        }
        Rvalue::Aggregate { operands, .. } => operands.iter().for_each(op),
        Rvalue::CallIntrinsic { args, .. } => args.iter().for_each(op),
        Rvalue::Repeat { value, .. } => op(value),
        Rvalue::Ref { place, .. } | Rvalue::Len(place) => f(place),
        Rvalue::StaticLoad(_) => {}
    }
}

/// The locals one table rewrite threads through its steps.
struct TableRewrite {
    types: RowTableTypes,
    span: Span,
    /// The nested vector's length, read where the table is built.
    rows: Local,
    table: Local,
}

impl TableRewrite {
    fn assign(&self, place: Local, rvalue: Rvalue) -> Statement {
        Statement {
            kind: StatementKind::Assign {
                place: Place::local(place),
                rvalue,
            },
            span: self.span,
            inlined: None,
        }
    }

    /// `gos_rt_vec_free(table)`, into a sink local of its own.
    fn free_table(&self, body: &mut Body) -> Statement {
        let sink = fresh_local(body, self.types.unit);
        self.assign(
            sink,
            Rvalue::CallIntrinsic {
                name: "gos_rt_vec_free",
                args: vec![Operand::Copy(Place::local(self.table))],
            },
        )
    }

    /// The statements that compare `index` with the row count, and the bounds
    /// assert on their result that continues at `target`.
    fn bounds_check(&self, body: &mut Body, index: &Operand, target: BlockId) -> (Vec<Statement>, Terminator) {
        let index_u = fresh_local(body, self.types.uint64);
        let rows_u = fresh_local(body, self.types.uint64);
        let in_bounds = fresh_local(body, self.types.boolean);
        let stmts = vec![
            self.assign(
                index_u,
                Rvalue::Cast {
                    operand: index.clone(),
                    target: self.types.uint64,
                },
            ),
            self.assign(
                rows_u,
                Rvalue::Cast {
                    operand: Operand::Copy(Place::local(self.rows)),
                    target: self.types.uint64,
                },
            ),
            self.assign(
                in_bounds,
                Rvalue::BinaryOp {
                    op: BinOp::Lt,
                    lhs: Operand::Copy(Place::local(index_u)),
                    rhs: Operand::Copy(Place::local(rows_u)),
                },
            ),
        ];
        let assert = Terminator::Assert {
            cond: Operand::Copy(Place::local(in_bounds)),
            expected: true,
            msg: crate::ir::AssertMessage::BoundsCheck {
                index: index.clone(),
                seq: Operand::Copy(Place::local(self.table)),
            },
            target,
        };
        (stmts, assert)
    }

    /// A block that binds `row` to table element `index` and continues at
    /// `target`.
    fn address_block(&self, id: usize, index: Operand, row: Local, target: BlockId, span: Span) -> BasicBlock {
        BasicBlock {
            id: BlockId(id as u32),
            stmts: Vec::new(),
            terminator: Terminator::Call {
                callee: Operand::Const(ConstValue::Str("gos_rt_vec_get_ptr_unchecked".to_string())),
                args: vec![Operand::Copy(Place::local(self.table)), index],
                destination: Place::local(row),
                target: Some(target),
            },
            span,
            terminator_span: None,
            terminator_inlined: None,
        }
    }

    /// Rewrites the read the terminator of block `b` makes.
    fn terminator_read(&self, body: &mut Body, b: usize, row: Local, index: Operand) {
        let Terminator::Call {
            target: Some(target),
            ..
        } = body.blocks[b].terminator
        else {
            return;
        };
        let address = body.blocks.len();
        let (stmts, assert) = self.bounds_check(body, &index, BlockId(address as u32));
        body.blocks[b].stmts.extend(stmts);
        body.blocks[b].terminator = assert;
        let span = body.blocks[b].span;
        let block = self.address_block(address, index, row, target, span);
        body.blocks.push(block);
    }

    /// Rewrites the read statement `si` of block `b`, splitting the block
    /// after it.
    fn statement_read(&self, body: &mut Body, (b, si): (usize, usize), row: Local, index: Operand) {
        let address = body.blocks.len();
        let rest = address + 1;
        let rest_stmts = body.blocks[b].stmts.split_off(si + 1);
        body.blocks[b].stmts.pop();
        let (stmts, assert) = self.bounds_check(body, &index, BlockId(address as u32));
        let old_terminator = std::mem::replace(&mut body.blocks[b].terminator, assert);
        body.blocks[b].stmts.extend(stmts);
        let span = body.blocks[b].span;
        let block = self.address_block(address, index, row, BlockId(rest as u32), span);
        body.blocks.push(block);
        body.blocks.push(BasicBlock {
            id: BlockId(rest as u32),
            stmts: rest_stmts,
            terminator: old_terminator,
            span,
            terminator_span: None,
            terminator_inlined: None,
        });
    }

    /// Builds the table where the nest is entered, and gives it back before it
    /// is rebuilt and on every return.
    fn build_and_release(&self, body: &mut Body, plan: &RowTablePlan, nested: Local) {
        let build_len = body.blocks.len();
        let build_table = build_len + 1;
        let free_before_build = self.free_table(body);
        let nested_copy = self.assign(
            nested,
            Rvalue::Use(Operand::Copy(Place {
                local: plan.root,
                projection: plan.prefix.clone(),
            })),
        );
        let call = |name: &str, destination: Local, target: usize| Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name.to_string())),
            args: vec![Operand::Copy(Place::local(nested))],
            destination: Place::local(destination),
            target: Some(BlockId(target as u32)),
        };
        body.blocks.push(BasicBlock {
            id: BlockId(build_len as u32),
            stmts: vec![nested_copy, free_before_build],
            terminator: call("gos_rt_vec_len", self.rows, build_table),
            span: self.span,
            terminator_span: None,
            terminator_inlined: None,
        });
        body.blocks.push(BasicBlock {
            id: BlockId(build_table as u32),
            stmts: Vec::new(),
            terminator: call("gos_rt_vec_header_table", self.table, plan.header),
            span: self.span,
            terminator_span: None,
            terminator_inlined: None,
        });
        body.blocks[plan.entry].terminator = Terminator::Goto {
            target: BlockId(build_len as u32),
        };
        let returns: Vec<usize> = (0..body.blocks.len())
            .filter(|&b| matches!(body.blocks[b].terminator, Terminator::Return))
            .collect();
        for b in returns {
            let stmt = self.free_table(body);
            body.blocks[b].stmts.push(stmt);
        }
        let zero = self.assign(self.table, Rvalue::Use(Operand::Const(ConstValue::Int(0))));
        body.blocks[0].stmts.insert(0, zero);
    }
}

fn apply_row_table_plan(body: &mut Body, types: &RowTableTypes, plan: RowTablePlan) {
    let rewrite = TableRewrite {
        types: *types,
        span: body.blocks[plan.header].span,
        rows: fresh_local(body, types.int64),
        table: fresh_local(body, types.table),
    };
    let nested = fresh_local(body, plan.nested_ty);
    // Each row read becomes the bounds check the indexed read makes, then the
    // table element's address. Later positions in a block move first, so an
    // earlier read's position is still where it was; a block's terminator read
    // moves before its statements, since a statement read carries the
    // terminator away when it splits the block.
    let mut reads = plan.reads.clone();
    reads.sort_by_key(|(b, s, _, _)| std::cmp::Reverse((*b, s.map_or(usize::MAX, |si| si))));
    for (b, si, row, index) in reads {
        match si {
            Some(si) => rewrite.statement_read(body, (b, si), row, index),
            None => rewrite.terminator_read(body, b, row, index),
        }
    }
    rewrite.build_and_release(body, &plan, nested);
}
