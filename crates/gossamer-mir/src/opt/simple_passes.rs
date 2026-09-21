/// Replaces `SwitchInt` terminators whose discriminant is a known
/// constant with a direct `Goto` to the matching target. Runs after
/// constant folding so that simple `if false { ... } else { ... }`
/// branches fold away entirely. Stream E.2.
pub fn const_branch_elim(body: &mut Body) {
    use crate::ir::Terminator;
    let const_locals: HashMap<u32, i128> = const_int_locals(body);
    for block in &mut body.blocks {
        let Terminator::SwitchInt {
            discriminant,
            arms,
            default,
        } = &block.terminator
        else {
            continue;
        };
        let known = match discriminant {
            Operand::Const(ConstValue::Int(n)) => Some(*n),
            Operand::Const(ConstValue::Bool(b)) => Some(i128::from(*b)),
            Operand::Copy(place) => {
                if place.projection.is_empty() {
                    const_locals.get(&place.local.0).copied()
                } else {
                    None
                }
            }
            _ => None,
        };
        let Some(value) = known else { continue };
        let value_i128 = value;
        let mut target = *default;
        for (arm_value, arm_target) in arms {
            if *arm_value == value_i128 {
                target = *arm_target;
                break;
            }
        }
        block.terminator = Terminator::Goto { target };
    }
}

/// Removes basic blocks unreachable from the entry block, renumbers the
/// survivors consecutively, and rewrites every terminator target through
/// the old-id -> new-id map. Runs after [`const_branch_elim`], which turns
/// constant `SwitchInt`s into `Goto`s and orphans the arms no longer taken;
/// without this sweep those arms linger as dead blocks that later passes
/// and codegen still walk. Preserves the `block.id == position` invariant
/// the verifier enforces.
pub fn dead_block_sweep(body: &mut Body) {
    use std::collections::VecDeque;
    if body.blocks.is_empty() {
        return;
    }
    let id_to_index: HashMap<u32, usize> = body
        .blocks
        .iter()
        .enumerate()
        .map(|(i, b)| (b.id.0, i))
        .collect();
    let mut reachable = vec![false; body.blocks.len()];
    let entry = body.blocks[0].id;
    let mut queue: VecDeque<BlockId> = VecDeque::new();
    if let Some(&i) = id_to_index.get(&entry.0) {
        reachable[i] = true;
        queue.push_back(entry);
    }
    while let Some(b) = queue.pop_front() {
        let Some(&i) = id_to_index.get(&b.0) else {
            continue;
        };
        for succ in block_successors(&body.blocks[i].terminator) {
            if let Some(&j) = id_to_index.get(&succ.0)
                && !reachable[j]
            {
                reachable[j] = true;
                queue.push_back(succ);
            }
        }
    }
    if reachable.iter().all(|&r| r) {
        return;
    }
    // Renumber the survivors consecutively in their current order, so the
    // entry block (always reachable, always first) stays `BlockId(0)`.
    let mut old_to_new: HashMap<u32, BlockId> = HashMap::new();
    let mut next = 0u32;
    for (i, block) in body.blocks.iter().enumerate() {
        if reachable[i] {
            old_to_new.insert(block.id.0, BlockId(next));
            next += 1;
        }
    }
    let blocks = std::mem::take(&mut body.blocks);
    let mut new_blocks = Vec::with_capacity(next as usize);
    for (i, mut block) in blocks.into_iter().enumerate() {
        if !reachable[i] {
            continue;
        }
        block.id = old_to_new[&block.id.0];
        remap_terminator_targets(&mut block.terminator, &old_to_new);
        new_blocks.push(block);
    }
    body.blocks = new_blocks;
}

/// Successor block ids of a terminator (every block it may transfer
/// control to).
fn block_successors(t: &Terminator) -> Vec<BlockId> {
    match t {
        Terminator::Goto { target } => vec![*target],
        Terminator::SwitchInt { arms, default, .. } => {
            let mut out: Vec<BlockId> = arms.iter().map(|(_, b)| *b).collect();
            out.push(*default);
            out
        }
        Terminator::Call { target, .. } => target.iter().copied().collect(),
        Terminator::Assert { target, .. } => vec![*target],
        Terminator::Drop { target, .. } => vec![*target],
        Terminator::Return | Terminator::Unreachable | Terminator::Panic { .. } => Vec::new(),
    }
}

/// Rewrites every block target in `t` through `map` (old id -> new id).
/// Targets absent from the map belong to removed-unreachable blocks and
/// so cannot be reached from `t`; they are left as-is.
fn remap_terminator_targets(t: &mut Terminator, map: &HashMap<u32, BlockId>) {
    let remap = |b: &mut BlockId| {
        if let Some(&new) = map.get(&b.0) {
            *b = new;
        }
    };
    match t {
        Terminator::Goto { target } => remap(target),
        Terminator::SwitchInt { arms, default, .. } => {
            for (_, b) in arms.iter_mut() {
                remap(b);
            }
            remap(default);
        }
        Terminator::Call { target, .. } => {
            if let Some(b) = target {
                remap(b);
            }
        }
        Terminator::Assert { target, .. } => remap(target),
        Terminator::Drop { target, .. } => remap(target),
        Terminator::Return | Terminator::Unreachable | Terminator::Panic { .. } => {}
    }
}

fn const_int_locals(body: &Body) -> HashMap<u32, i128> {
    // A local is treated as a known constant only when *every*
    // store to it (across all blocks) writes the same constant
    // value - otherwise control-flow-sensitive code such as
    // `let mut neg = false; if cond { neg = true }; if neg { ... }`
    // would mistake the second assignment for unconditional and
    // collapse the second `if` into a direct goto, miscompiling
    // the conditional branch.
    let mut candidates: HashMap<u32, Option<i128>> = HashMap::new();
    let mut tainted: std::collections::HashSet<u32> = std::collections::HashSet::new();
    for block in &body.blocks {
        for stmt in &block.stmts {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            if !place.projection.is_empty() {
                continue;
            }
            let local_id = place.local.0;
            if tainted.contains(&local_id) {
                continue;
            }
            let value = match rvalue {
                Rvalue::Use(Operand::Const(ConstValue::Int(n))) => Some(*n),
                Rvalue::Use(Operand::Const(ConstValue::Bool(b))) => Some(i128::from(*b)),
                _ => None,
            };
            match (value, candidates.get(&local_id).copied()) {
                (None, _) => {
                    tainted.insert(local_id);
                    candidates.remove(&local_id);
                }
                (Some(v), None) => {
                    candidates.insert(local_id, Some(v));
                }
                (Some(v), Some(Some(prev))) if prev == v => {}
                _ => {
                    tainted.insert(local_id);
                    candidates.remove(&local_id);
                }
            }
        }
    }
    candidates
        .into_iter()
        .filter_map(|(k, v)| v.map(|n| (k, n)))
        .collect()
}

/// Folds `BinaryOp` / `UnaryOp` rvalues whose operands are both
/// [`Operand::Const`], and `BinaryOp`s where one operand is a
/// constant identity / absorbing element (`x + 0`, `x * 1`, `x & 0`,
/// `b | true`, ...).
pub fn const_fold(body: &mut Body) {
    for block in &mut body.blocks {
        for stmt in &mut block.stmts {
            if let StatementKind::Assign {
                rvalue: ref mut rv, ..
            } = stmt.kind
            {
                if let Some(folded) = try_fold(rv) {
                    *rv = Rvalue::Use(Operand::Const(folded));
                } else if let Some(simplified) = try_identity_fold(rv) {
                    *rv = simplified;
                }
            }
        }
    }
}

/// Type-aware constant folding for integer arithmetic. Debug profiles leave
/// an overflowing operation in MIR so code generation can emit the runtime
/// overflow panic. Release profiles fold with the declared integer width's
/// wrapping semantics.
fn const_fold_typed(body: &mut Body, tcx: &TyCtxt, checked_overflow: bool) {
    let local_tys: Vec<_> = body.locals.iter().map(|local| local.ty).collect();
    for block in &mut body.blocks {
        for stmt in &mut block.stmts {
            if let StatementKind::Assign {
                place,
                rvalue: rv,
            } = &mut stmt.kind
            {
                let folded = match tcx.kind_of(local_tys[place.local.0 as usize]) {
                    TyKind::Int(int_ty) => try_fold_typed_int(rv, *int_ty, checked_overflow),
                    _ => try_fold(rv),
                };
                if let Some(folded) = folded {
                    *rv = Rvalue::Use(Operand::Const(folded));
                } else if let Some(simplified) = try_identity_fold(rv) {
                    *rv = simplified;
                }
            }
        }
    }
}

fn try_fold_typed_int(
    rvalue: &Rvalue,
    int_ty: gossamer_types::IntTy,
    checked_overflow: bool,
) -> Option<ConstValue> {
    let Rvalue::BinaryOp {
        op,
        lhs: Operand::Const(ConstValue::Int(lhs)),
        rhs: Operand::Const(ConstValue::Int(rhs)),
    } = rvalue
    else {
        return try_fold(rvalue);
    };
    if matches!(op, BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge) {
        return fold_typed_integer_ordering(*op, *lhs, *rhs, int_ty);
    }
    if !matches!(
        op,
        BinOp::Add
            | BinOp::WrappingAdd
            | BinOp::Sub
            | BinOp::WrappingSub
            | BinOp::Mul
            | BinOp::WrappingMul
    ) {
        return fold_binary(*op, &ConstValue::Int(*lhs), &ConstValue::Int(*rhs));
    }
    let fold_op = match op {
        BinOp::WrappingAdd => BinOp::Add,
        BinOp::WrappingSub => BinOp::Sub,
        BinOp::WrappingMul => BinOp::Mul,
        _ => *op,
    };
    let checked = checked_overflow && matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul);
    fold_typed_integer_arithmetic(fold_op, *lhs, *rhs, int_ty, checked)
        .map(ConstValue::Int)
}

/// Folds `<`, `<=`, `>`, `>=` at the operands' declared signedness.
///
/// An unsigned operand is stored sign-extended in `ConstValue::Int`, so a
/// `u64` at or above 2^63 reads as negative. Comparing it as `i128` would
/// disagree with the `ucmp` the three tiers emit for that type, which is the
/// one thing a constant fold must never do.
fn fold_typed_integer_ordering(
    op: BinOp,
    lhs: i128,
    rhs: i128,
    int_ty: gossamer_types::IntTy,
) -> Option<ConstValue> {
    use gossamer_types::IntTy;

    let unsigned_width = match int_ty {
        IntTy::U8 => Some(8),
        IntTy::U16 => Some(16),
        IntTy::U32 => Some(32),
        IntTy::U64 | IntTy::U128 | IntTy::Usize => Some(64),
        _ => None,
    };
    let ordering = match unsigned_width {
        // Mask to the declared width first: a narrow operand may still be
        // carrying sign extension from an earlier signed step, and the tiers
        // compare only the bits the type actually holds.
        Some(64) => (lhs as u64).cmp(&(rhs as u64)),
        Some(bits) => {
            let mask = (1u64 << bits) - 1;
            ((lhs as u64) & mask).cmp(&((rhs as u64) & mask))
        }
        None => lhs.cmp(&rhs),
    };
    let value = match op {
        BinOp::Lt => ordering.is_lt(),
        BinOp::Le => ordering.is_le(),
        BinOp::Gt => ordering.is_gt(),
        BinOp::Ge => ordering.is_ge(),
        _ => return None,
    };
    Some(ConstValue::Bool(value))
}

fn fold_typed_integer_arithmetic(
    op: BinOp,
    lhs: i128,
    rhs: i128,
    int_ty: gossamer_types::IntTy,
    checked_overflow: bool,
) -> Option<i128> {
    use gossamer_types::IntTy;

    let unsigned_bits = match int_ty {
        IntTy::U8 => Some(8),
        IntTy::U16 => Some(16),
        IntTy::U32 => Some(32),
        IntTy::U64 | IntTy::U128 | IntTy::Usize => Some(64),
        _ => None,
    };
    if let Some(bits) = unsigned_bits {
        let lhs = u128::from(lhs as u64);
        let rhs = u128::from(rhs as u64);
        let value = match op {
            BinOp::Add => lhs.checked_add(rhs),
            BinOp::Sub => lhs.checked_sub(rhs),
            BinOp::Mul => lhs.checked_mul(rhs),
            _ => unreachable!(),
        };
        let max = if bits == 64 {
            u128::from(u64::MAX)
        } else {
            (1u128 << bits) - 1
        };
        if checked_overflow {
            return value
                .filter(|value| *value <= max)
                .map(|value| i128::from(value as u64 as i64));
        }
        let mask = max;
        let wrapped = value.unwrap_or_else(|| match op {
            BinOp::Add => lhs.wrapping_add(rhs),
            BinOp::Sub => lhs.wrapping_sub(rhs),
            BinOp::Mul => lhs.wrapping_mul(rhs),
            _ => unreachable!(),
        }) as u64
            & mask as u64;
        return Some(i128::from(wrapped as i64));
    }

    let bits = match int_ty {
        IntTy::I8 => 8,
        IntTy::I16 => 16,
        IntTy::I32 => 32,
        IntTy::I64 | IntTy::I128 | IntTy::Isize => 64,
        _ => unreachable!(),
    };
    let value = match op {
        BinOp::Add => lhs + rhs,
        BinOp::Sub => lhs - rhs,
        BinOp::Mul => lhs * rhs,
        _ => unreachable!(),
    };
    let min = -(1i128 << (bits - 1));
    let max = (1i128 << (bits - 1)) - 1;
    if checked_overflow {
        return (min..=max).contains(&value).then_some(value);
    }
    let modulus = 1i128 << bits;
    let wrapped = value.rem_euclid(modulus);
    let signed = if wrapped > max {
        wrapped - modulus
    } else {
        wrapped
    };
    Some(signed)
}

fn try_fold(rvalue: &Rvalue) -> Option<ConstValue> {
    match rvalue {
        Rvalue::BinaryOp {
            op,
            lhs: Operand::Const(a),
            rhs: Operand::Const(b),
        } => fold_binary(*op, a, b),
        Rvalue::UnaryOp {
            op,
            operand: Operand::Const(c),
        } => fold_unary(*op, c),
        _ => None,
    }
}

fn fold_binary(op: BinOp, lhs: &ConstValue, rhs: &ConstValue) -> Option<ConstValue> {
    match (lhs, rhs) {
        (ConstValue::Int(x), ConstValue::Int(y)) => match op {
            BinOp::Add | BinOp::WrappingAdd => Some(ConstValue::Int(x.wrapping_add(*y))),
            BinOp::Sub => Some(ConstValue::Int(x.wrapping_sub(*y))),
            BinOp::Mul | BinOp::WrappingMul => Some(ConstValue::Int(x.wrapping_mul(*y))),
            // Div/rem signedness is carried by the typed lowering context, not
            // by `ConstValue`; folding here would make `u64`/`usize` operands
            // at or above 2^63 indistinguishable from signed i64 values.
            BinOp::Div | BinOp::Rem => None,
            BinOp::BitAnd => Some(ConstValue::Int(x & y)),
            BinOp::BitOr => Some(ConstValue::Int(x | y)),
            BinOp::BitXor => Some(ConstValue::Int(x ^ y)),
            // Equality is width- and sign-independent on the raw bits.
            BinOp::Eq => Some(ConstValue::Bool(x == y)),
            BinOp::Ne => Some(ConstValue::Bool(x != y)),
            // Ordering is not: `ConstValue` carries no signedness, and a
            // `u64`/`usize` operand at or above 2^63 is stored sign-extended,
            // so folding it with signed comparison would disagree with the
            // unsigned comparison the tiers emit for that type.
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => None,
            _ => None,
        },
        (ConstValue::Bool(x), ConstValue::Bool(y)) => match op {
            BinOp::Eq => Some(ConstValue::Bool(x == y)),
            BinOp::Ne => Some(ConstValue::Bool(x != y)),
            BinOp::BitAnd => Some(ConstValue::Bool(*x && *y)),
            BinOp::BitOr => Some(ConstValue::Bool(*x || *y)),
            BinOp::BitXor => Some(ConstValue::Bool(x ^ y)),
            _ => None,
        },
        _ => None,
    }
}

/// Simplifies a `BinaryOp` whose lhs or rhs is a constant identity
/// or absorbing element, rewriting to a plain `Use` of the surviving
/// operand (or the absorbed constant). Integer and bool shapes only:
/// float identities are unsound under IEEE-754 (`-0.0 + 0.0 == +0.0`,
/// `x * 0.0` with NaN/∞), and a non-constant divisor keeps its
/// runtime division so `0 / x` still faults when `x == 0`. Width
/// independence: only the constants `0` and `1` qualify - an
/// all-ones mask (`x & !0`) would depend on the operand's bit width,
/// which `ConstValue::Int(i128)` does not carry.
fn try_identity_fold(rvalue: &Rvalue) -> Option<Rvalue> {
    let Rvalue::BinaryOp { op, lhs, rhs } = rvalue else {
        return None;
    };
    if let Operand::Const(c) = rhs {
        if let Some(rv) = identity_fold_with_const(*op, lhs, c, true) {
            return Some(rv);
        }
    }
    if let Operand::Const(c) = lhs {
        if let Some(rv) = identity_fold_with_const(*op, rhs, c, false) {
            return Some(rv);
        }
    }
    None
}

/// One side of [`try_identity_fold`]: `other OP c` when `const_is_rhs`,
/// `c OP other` otherwise.
fn identity_fold_with_const(
    op: BinOp,
    other: &Operand,
    c: &ConstValue,
    const_is_rhs: bool,
) -> Option<Rvalue> {
    let keep = || Some(Rvalue::Use(other.clone()));
    let int_const = |n: i128| Some(Rvalue::Use(Operand::Const(ConstValue::Int(n))));
    let bool_const = |b: bool| Some(Rvalue::Use(Operand::Const(ConstValue::Bool(b))));
    match c {
        ConstValue::Int(n) => match (op, *n) {
            (BinOp::Add | BinOp::WrappingAdd | BinOp::BitOr | BinOp::BitXor, 0) => keep(),
            (BinOp::Mul | BinOp::WrappingMul, 1) => keep(),
            (BinOp::Mul | BinOp::WrappingMul, 0) => int_const(0),
            (BinOp::BitAnd, 0) => int_const(0),
            // Right-hand-side-only identities: subtraction and
            // division are not commutative, and a zero shift *amount*
            // is an identity while a zero shifted *value* would erase
            // the runtime shift-amount check.
            (BinOp::Sub | BinOp::Shl | BinOp::Shr, 0) if const_is_rhs => keep(),
            (BinOp::Div, 1) if const_is_rhs => keep(),
            (BinOp::Rem, 1) if const_is_rhs => int_const(0),
            _ => None,
        },
        ConstValue::Bool(b) => match (op, *b) {
            (BinOp::BitAnd, true) | (BinOp::BitOr | BinOp::BitXor, false) => keep(),
            (BinOp::BitAnd, false) => bool_const(false),
            (BinOp::BitOr, true) => bool_const(true),
            _ => None,
        },
        _ => None,
    }
}

fn fold_unary(op: crate::ir::UnOp, operand: &ConstValue) -> Option<ConstValue> {
    match (op, operand) {
        // `i128::MIN`'s negation overflows; folding through `-x` would
        // panic in debug builds. Skip the fold and leave the runtime to
        // produce the wrapping result if the program reaches that path.
        (crate::ir::UnOp::Neg, ConstValue::Int(x)) => x.checked_neg().map(ConstValue::Int),
        (crate::ir::UnOp::Not, ConstValue::Bool(b)) => Some(ConstValue::Bool(!b)),
        _ => None,
    }
}

/// Replaces `Copy(place)` operands with the rvalue that flowed into
/// the place, when that rvalue is itself a `Use(Const|Copy)`. Operates
/// block-local only.
///
/// Aggregate locals (`Array`/`Tuple`/`Adt`) are excluded from
/// propagation: an assignment `_X = Use(Copy(_Y))` for an aggregate
/// is a memcpy between distinct storage slots, not an alias. Forwarding
/// `Copy(_X)` to `Copy(_Y)` would route a later `&mut _X` borrow at
/// the wrong slot, so writes through the borrow would land on `_Y`'s
/// (now stale) storage instead of the user's named binding.
///
/// Bindings whose RHS reads through a projection (`Copy(a[i])`,
/// `Copy(s.f)`) are also unsafe to forward across an intervening
/// write: in `let t = a[lo]; a[lo] = a[hi]; a[hi] = t`, propagating
/// `t -> Copy(a[lo])` into the third statement reads the freshly-
/// stored `a[hi]` value instead of the original `a[lo]`. We only
/// retain bindings whose RHS is a `Const` or `Copy(simple-local)`.
pub fn copy_propagate(body: &mut Body, tcx: &TyCtxt) {
    let aggregate_locals = aggregate_locals(body, tcx);
    for block in &mut body.blocks {
        let mut bindings: HashMap<Local, Operand> = HashMap::new();
        for stmt in &mut block.stmts {
            if let StatementKind::StaticStore { value, .. } = &mut stmt.kind {
                substitute_operand(value, &bindings);
                continue;
            }
            if let StatementKind::Assign { place, rvalue } = &mut stmt.kind {
                // Substitute reads first (covers `Use` and every other
                // rvalue shape uniformly).
                substitute_rvalue(rvalue, &bindings);
                if !place.is_simple() {
                    // A projected write (`*p`, `p.field`) does not
                    // reassign the local's own value; leave bindings.
                    continue;
                }
                // Any assignment to `place.local` invalidates its prior
                // binding and any binding that referenced it - otherwise
                // a stale value (e.g. a null-init constant) would be
                // propagated past the reassignment into later uses.
                bindings.remove(&place.local);
                bindings.retain(|_, v| {
                    !matches!(v, Operand::Copy(p) if p.local == place.local && p.projection.is_empty())
                });
                // Record a fresh copy/const binding only for `Use` of a
                // simple, non-aggregate operand.
                if let Rvalue::Use(operand) = rvalue {
                    let dest_aggregate = aggregate_locals
                        .get(place.local.0 as usize)
                        .copied()
                        .unwrap_or(false);
                    let operand_is_simple = match operand {
                        Operand::Const(_) | Operand::FnRef { .. } => true,
                        Operand::Copy(p) => p.is_simple(),
                    };
                    if !dest_aggregate && operand_is_simple {
                        bindings.insert(place.local, operand.clone());
                    }
                }
            }
        }
        // `x = x` states nothing. It arises from folding an identity update
        // (`x += 0`), and every later pass and the backends have to carry a
        // statement whose destination is also its source. Drop it here, where
        // the copy shapes are already being canonicalised.
        block.stmts.retain(|stmt| {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                return true;
            };
            let Rvalue::Use(Operand::Copy(source)) = rvalue else {
                return true;
            };
            !(place.is_simple() && source.is_simple() && place.local == source.local)
        });
    }
}

fn substitute_rvalue(rvalue: &mut Rvalue, bindings: &HashMap<Local, Operand>) {
    match rvalue {
        Rvalue::Use(op) => substitute_operand(op, bindings),
        Rvalue::BinaryOp { lhs, rhs, .. } => {
            substitute_operand(lhs, bindings);
            substitute_operand(rhs, bindings);
        }
        Rvalue::UnaryOp { operand, .. } => substitute_operand(operand, bindings),
        Rvalue::Cast { operand, .. } => substitute_operand(operand, bindings),
        Rvalue::Aggregate { operands, .. } => {
            for op in operands {
                substitute_operand(op, bindings);
            }
        }
        Rvalue::CallIntrinsic { name, args } => {
            if reads_arg_as_slot(name) {
                return;
            }
            for op in args {
                substitute_operand(op, bindings);
            }
        }
        Rvalue::Repeat { value, .. } => substitute_operand(value, bindings),
        Rvalue::Len(_) | Rvalue::Ref { .. } | Rvalue::StaticLoad(_) => {}
    }
}

/// Intrinsics whose argument names a STORAGE SLOT rather than a value: each
/// takes the place's address and writes the slot through it. An operand
/// carrying the same value names different storage, so these arguments stay
/// exactly as the drop pass wrote them.
fn reads_arg_as_slot(name: &str) -> bool {
    matches!(
        name,
        "gos_rt_map_field_clone"
            | "gos_rt_map_field_release"
            | "gos_rt_option_slot_retain"
            | "gos_rt_option_slot_retain_ok"
            | "gos_rt_option_slot_release"
    )
}

fn substitute_operand(operand: &mut Operand, bindings: &HashMap<Local, Operand>) {
    let Operand::Copy(Place {
        local,
        ref projection,
    }) = *operand
    else {
        return;
    };
    if !projection.is_empty() {
        return;
    }
    if let Some(replacement) = bindings.get(&local) {
        *operand = replacement.clone();
    }
}

/// Removes assignments whose destination local is never read again and
/// is not observable (no projections, no exported writes). A simple
/// forward-use count keeps it local to each block.
///
/// Aggregate-typed locals are also exempt: even with `use_count` == 0,
/// a later `&mut _X`-style borrow may bind to `_X`'s storage slot,
/// and dropping the write would surface uninitialised data through
/// the borrow. The same guard `copy_propagate` uses applies here so
/// the two passes treat aggregate identity consistently.
pub fn dead_store_elim(body: &mut Body, tcx: &TyCtxt) {
    // Walk the whole body once and tally cross-block reads, then drop
    // const-producing assignments whose destination local is read
    // nowhere. A per-block counter misses the common case where a
    // match/if-join writes a temporary in the arm blocks and reads it
    // back in the join block.
    let mut use_count: HashMap<Local, usize> = HashMap::new();
    for block in &body.blocks {
        for stmt in &block.stmts {
            match &stmt.kind {
                StatementKind::Assign { place, rvalue } => {
                    if !place.projection.is_empty() {
                        count_place_reads(place, &mut use_count);
                    }
                    count_rvalue_reads(rvalue, &mut use_count);
                }
                StatementKind::StaticStore { value, .. } => {
                    count_operand_reads(value, &mut use_count);
                }
                _ => {}
            }
        }
        count_terminator_reads(&block.terminator, &mut use_count);
    }
    // The return slot is implicitly read by `Terminator::Return` even
    // though we do not surface the operand in the terminator itself.
    // Pin its use count so dead-store-elim never drops writes into it.
    *use_count.entry(Local::RETURN).or_insert(0) += 1;

    let aggregates = aggregate_locals(body, tcx);
    for block in &mut body.blocks {
        let mut retained = Vec::with_capacity(block.stmts.len());
        for stmt in std::mem::take(&mut block.stmts) {
            if let StatementKind::Assign {
                place,
                rvalue: Rvalue::Use(Operand::Const(_)),
            } = &stmt.kind
            {
                let dest_aggregate = aggregates
                    .get(place.local.0 as usize)
                    .copied()
                    .unwrap_or(false);
                if place.is_simple()
                    && !dest_aggregate
                    && use_count.get(&place.local).copied().unwrap_or(0) == 0
                {
                    continue;
                }
            }
            retained.push(stmt);
        }
        block.stmts = retained;
    }
}

fn count_rvalue_reads(rvalue: &Rvalue, uses: &mut HashMap<Local, usize>) {
    match rvalue {
        Rvalue::Use(op) => count_operand_reads(op, uses),
        Rvalue::BinaryOp { lhs, rhs, .. } => {
            count_operand_reads(lhs, uses);
            count_operand_reads(rhs, uses);
        }
        Rvalue::UnaryOp { operand, .. } => count_operand_reads(operand, uses),
        Rvalue::Cast { operand, .. } => count_operand_reads(operand, uses),
        Rvalue::Aggregate { operands, .. } => {
            for op in operands {
                count_operand_reads(op, uses);
            }
        }
        Rvalue::CallIntrinsic { args, .. } => {
            for op in args {
                count_operand_reads(op, uses);
            }
        }
        Rvalue::Repeat { value, .. } => count_operand_reads(value, uses),
        Rvalue::Len(place) | Rvalue::Ref { place, .. } => {
            count_place_reads(place, uses);
        }
        // No local reads; the static is referenced by symbol.
        Rvalue::StaticLoad(_) => {}
    }
}

fn count_operand_reads(operand: &Operand, uses: &mut HashMap<Local, usize>) {
    if let Operand::Copy(place) = operand {
        count_place_reads(place, uses);
    }
}

/// Counts a read of the root local plus every local referenced by a
/// [`Projection::Index`] inside `place.projection`. Without this, an
/// index expression such as `xs[i]` only registers `xs` as read,
/// letting dead-store elimination drop the `i = Const(...)` store and
/// leaving the projection pointing at an uninitialised slot.
fn count_place_reads(place: &Place, uses: &mut HashMap<Local, usize>) {
    *uses.entry(place.local).or_insert(0) += 1;
    for proj in &place.projection {
        if let Projection::Index(idx) = proj {
            *uses.entry(*idx).or_insert(0) += 1;
        }
    }
}

fn count_terminator_reads(terminator: &Terminator, uses: &mut HashMap<Local, usize>) {
    match terminator {
        Terminator::SwitchInt { discriminant, .. } => count_operand_reads(discriminant, uses),
        Terminator::Call {
            callee,
            args,
            destination,
            ..
        } => {
            count_operand_reads(callee, uses);
            for op in args {
                count_operand_reads(op, uses);
            }
            if !destination.projection.is_empty() {
                count_place_reads(destination, uses);
            }
        }
        Terminator::Assert { cond, msg, .. } => {
            count_operand_reads(cond, uses);
            for op in msg.operands() {
                count_operand_reads(op, uses);
            }
        }
        _ => {}
    }
}

/// Returns the number of [`crate::ir::Statement`]s across all blocks.
#[must_use]
pub fn statement_count(body: &Body) -> usize {
    body.blocks.iter().map(|b| b.stmts.len()).sum()
}

/// Returns the [`ConstValue`] flowing into `local` in the entry block,
/// if any direct assignment records one. Convenience accessor for
/// tests that want to inspect post-const-fold state.
#[must_use]
pub fn const_value_of(body: &Body, local: Local) -> Option<ConstValue> {
    // A local is a known constant only if it is assigned EXACTLY ONCE,
    // and that single assignment is `Use(Const)`. A local reassigned
    // later (e.g. zeroed for null-safety then overwritten by a heap
    // allocation) is not constant - propagating the first const value
    // past the reassignment would miscompile every later use.
    let mut found: Option<ConstValue> = None;
    for block in &body.blocks {
        for stmt in &block.stmts {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            if place.local != local || !place.is_simple() {
                continue;
            }
            match rvalue {
                Rvalue::Use(Operand::Const(value)) if found.is_none() => {
                    found = Some(value.clone());
                }
                // A second assignment of any kind (including another
                // const) disqualifies constant folding for this local.
                _ => return None,
            }
        }
    }
    // Terminator-position definitions (Call destinations) also reassign
    // the local; if any exists, the local is not a fixed constant.
    for block in &body.blocks {
        if let Terminator::Call { destination, .. } = &block.terminator
            && destination.local == local
            && destination.is_simple()
        {
            return None;
        }
    }
    found
}

#[cfg(test)]
mod simple_passes_tests {
    use gossamer_types::IntTy;

    use super::{ConstValue, BinOp, fold_binary, fold_typed_integer_ordering};

    #[test]
    fn unsigned_ordering_folds_above_the_signed_range() {
        // `u64::MAX` is stored sign-extended, so a signed fold would call it
        // the smaller operand.
        let max = ConstValue::Int(-1);
        let ConstValue::Int(max) = max else { unreachable!() };
        assert_eq!(
            fold_typed_integer_ordering(BinOp::Gt, max, 5, IntTy::U64),
            Some(ConstValue::Bool(true))
        );
        assert_eq!(
            fold_typed_integer_ordering(BinOp::Lt, max, 5, IntTy::U64),
            Some(ConstValue::Bool(false))
        );
        assert_eq!(
            fold_typed_integer_ordering(BinOp::Ge, 5, max, IntTy::Usize),
            Some(ConstValue::Bool(false))
        );
    }

    #[test]
    fn signed_ordering_keeps_signed_semantics() {
        assert_eq!(
            fold_typed_integer_ordering(BinOp::Lt, -1, 5, IntTy::I64),
            Some(ConstValue::Bool(true))
        );
    }

    #[test]
    fn untyped_ordering_is_left_unfolded() {
        // Without a declared integer type there is no signedness to fold at.
        assert_eq!(
            fold_binary(BinOp::Lt, &ConstValue::Int(-1), &ConstValue::Int(5)),
            None
        );
        assert_eq!(
            fold_binary(BinOp::Eq, &ConstValue::Int(-1), &ConstValue::Int(5)),
            Some(ConstValue::Bool(false))
        );
    }
}
