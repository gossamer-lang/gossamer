// ---------------------------------------------------------------------------
// Index ranges every caller proves, carried into the callee.
// ---------------------------------------------------------------------------

/// An index parameter every call site proves in range of a vector parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct EntryRange {
    /// Argument position of the vector.
    vec: usize,
    /// Argument position of the index.
    index: usize,
}

/// Removes the index checks a function makes on `xs[i]` when `xs` and `i` are
/// parameters and every call in the program passes an `i` that a counted
/// `for i in 0..xs.len()` loop at the call site holds below the length of the
/// `xs` it passes. Inside the function the fact holds wherever neither
/// parameter changes: the index is never written, and nothing the function
/// does can change the vector's length.
///
/// The set of call sites must be the whole program's, so a function whose
/// name is also used as a value - a callback, a spawned body, an export - is
/// left alone, and so is one no call site names.
pub fn propagate_entry_bounds(bodies: &mut [Body], tcx: &TyCtxt, effects: &ProgramEffects) {
    let by_name: HashMap<&str, usize> = bodies
        .iter()
        .enumerate()
        .map(|(i, b)| (b.name.as_str(), i))
        .collect();
    let mut by_def: HashMap<gossamer_resolve::DefId, Option<usize>> = HashMap::new();
    for (i, b) in bodies.iter().enumerate() {
        if let Some(def) = b.def {
            by_def
                .entry(def)
                .and_modify(|slot| *slot = None)
                .or_insert(Some(i));
        }
    }
    let resolve = |callee: &Operand| -> Option<usize> {
        match callee {
            Operand::FnRef { def, substs } if substs.is_empty() => {
                by_def.get(def).copied().flatten()
            }
            Operand::Const(ConstValue::Str(name)) => by_name.get(name.as_str()).copied(),
            _ => None,
        }
    };

    let mut used_as_value: HashSet<usize> = HashSet::new();
    let mut proven: HashMap<usize, Option<HashSet<EntryRange>>> = HashMap::new();
    for caller in bodies.iter() {
        let sites = counted_call_sites(caller, effects);
        for (bi, block) in caller.blocks.iter().enumerate() {
            for stmt in &block.stmts {
                for op in statement_operands(stmt) {
                    if let Some(target) = resolve(op) {
                        used_as_value.insert(target);
                    }
                }
            }
            let Terminator::Call { callee, args, .. } = &block.terminator else {
                continue;
            };
            for arg in args {
                if let Some(target) = resolve(arg) {
                    used_as_value.insert(target);
                }
            }
            let Some(target) = resolve(callee) else {
                continue;
            };
            let here: HashSet<EntryRange> = sites.get(&bi).cloned().unwrap_or_default();
            let slot = proven.entry(target).or_insert_with(|| Some(here.clone()));
            if let Some(set) = slot {
                set.retain(|r| here.contains(r));
            }
        }
    }

    for (target, ranges) in proven {
        let Some(ranges) = ranges.filter(|r| !r.is_empty()) else {
            continue;
        };
        if used_as_value.contains(&target) {
            continue;
        }
        let body = &mut bodies[target];
        for range in ranges {
            apply_entry_range(body, tcx, effects, range);
        }
    }
}

/// The operands a statement reads, where a function named as a value shows.
fn statement_operands(stmt: &Statement) -> Vec<&Operand> {
    let StatementKind::Assign { rvalue, .. } = &stmt.kind else {
        return Vec::new();
    };
    match rvalue {
        Rvalue::Use(op) | Rvalue::UnaryOp { operand: op, .. } | Rvalue::Cast { operand: op, .. } => {
            vec![op]
        }
        Rvalue::BinaryOp { lhs, rhs, .. } => vec![lhs, rhs],
        Rvalue::Aggregate { operands, .. } => operands.iter().collect(),
        Rvalue::Repeat { value, .. } => vec![value],
        Rvalue::CallIntrinsic { args, .. } => args.iter().collect(),
        Rvalue::Len(_) | Rvalue::Ref { .. } | Rvalue::StaticLoad(_) => Vec::new(),
    }
}

/// For each call block of `body` inside a counted loop over a vector the loop
/// cannot resize, the (vector, index) argument pairs the loop proves: the
/// vector argument is the loop's vector and the index argument is its
/// counter.
fn counted_call_sites(body: &Body, effects: &ProgramEffects) -> HashMap<usize, HashSet<EntryRange>> {
    let succs: Vec<Vec<usize>> = body
        .blocks
        .iter()
        .map(|b| successor_indices(&b.terminator))
        .collect();
    let mut out: HashMap<usize, HashSet<EntryRange>> = HashMap::new();
    for h in 0..body.blocks.len() {
        let Some(header) = recognise_counted_header(&body.blocks[h]).filter(|hd| !hd.inclusive)
        else {
            continue;
        };
        let Some((region, latch)) =
            counted_loop_region(body, &succs, h, header.body_entry, header.exit)
        else {
            continue;
        };
        if !verify_counter(body, &region, h, latch, header.counter, true) {
            continue;
        }
        let Some(xs) = bound_traces_to_len(body, &region, h, header.bound) else {
            continue;
        };
        if vec_length_blocker(body, effects, &region, h, xs).is_some() {
            continue;
        }
        let family = vec_family(body, &region, h, xs);
        for &b in &region {
            let Terminator::Call { args, .. } = &body.blocks[b].terminator else {
                continue;
            };
            let vecs = args.iter().enumerate().filter(|(_, a)| {
                matches!(a, Operand::Copy(p) if family.contains(&p.local) && p.projection.is_empty())
            });
            for (vec, _) in vecs {
                for (index, arg) in args.iter().enumerate() {
                    if index != vec && index_is_counter(body, &region, header.counter, arg) {
                        out.entry(b).or_default().insert(EntryRange { vec, index });
                    }
                }
            }
        }
    }
    out
}

/// Rewrites `body`'s checked accesses `xs[i]` to their unchecked form, where
/// `xs` and `i` are the parameters `range` names, when the body keeps both.
fn apply_entry_range(body: &mut Body, tcx: &TyCtxt, effects: &ProgramEffects, range: EntryRange) {
    let arity = body.arity as usize;
    if range.vec >= arity || range.index >= arity {
        return;
    }
    // Locals 1..=arity hold the parameters in order.
    let param = |pos: usize| Local(u32::try_from(pos + 1).unwrap_or(u32::MAX));
    let (xs, index) = (param(range.vec), param(range.index));
    let all: Vec<usize> = (0..body.blocks.len()).collect();
    let index_kept = !body
        .blocks
        .iter()
        .any(|b| b.stmts.iter().any(|s| stmt_writes_bare(s, index)) || term_writes_bare(&b.terminator, index));
    if !index_kept
        || address_taken(body, index)
        || address_taken(body, xs)
        || !vec_elem_is_unchecked_scalar(body, tcx, xs)
        || vec_length_blocker(body, effects, &all, usize::MAX, xs).is_some()
    {
        return;
    }
    let family = vec_family(body, &all, usize::MAX, xs);
    let mut rewrites: Vec<(usize, &'static str)> = Vec::new();
    for (bi, block) in body.blocks.iter().enumerate() {
        let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            args,
            ..
        } = &block.terminator
        else {
            continue;
        };
        let (unchecked, idx_arg) = match (name.as_str(), args.as_slice()) {
            ("gos_rt_vec_get_i64", [_, idx]) => ("gos_rt_vec_get_i64_unchecked", idx),
            ("gos_rt_vec_set_i64", [_, idx, _]) => ("gos_rt_vec_set_i64_unchecked", idx),
            _ => continue,
        };
        let receiver_is_xs = matches!(
            &args[0],
            Operand::Copy(p) if family.contains(&p.local) && p.projection.is_empty()
        );
        if receiver_is_xs && index_is_counter(body, &all, index, idx_arg) {
            rewrites.push((bi, unchecked));
        }
    }
    for (b, name) in rewrites {
        if let Terminator::Call { callee, .. } = &mut body.blocks[b].terminator {
            *callee = Operand::Const(ConstValue::Str(name.to_string()));
        }
    }
}
