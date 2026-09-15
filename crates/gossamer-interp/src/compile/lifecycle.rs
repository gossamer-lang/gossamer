#![allow(clippy::too_many_lines, clippy::wildcard_imports)]
use super::*;

impl<'tcx> FnBuilder<'tcx> {
    pub(crate) fn new(
        name: &'static str,
        tcx: &'tcx TyCtxt,
        layouts: &'tcx StructLayouts,
        wrappers: &'tcx InlinableWrappers,
        inline_fns: &'tcx InlinableFns,
        fn_param_shareable: &'tcx crate::compile::FnParamShareable,
        fn_param_tys: &'tcx FnParamTypes,
        module_consts: &'tcx ConstValues,
        method_muts: &'tcx MutSelfMethods,
        impl_methods: &'tcx ImplMethodNames,
        mut_statics: &'tcx MutStatics,
        source_map: Option<&'tcx gossamer_lex::SourceMap>,
        cov: Option<&'tcx gossamer_lex::SourceMap>,
    ) -> Self {
        Self {
            name,
            tcx,
            dispatch: None,
            dispatch_key: "",
            layouts,
            wrappers,
            inline_fns,
            fn_param_shareable,
            fn_param_tys,
            inlining: Vec::new(),
            inlined_nodes: 0,
            module_consts,
            method_muts,
            impl_methods,
            mut_statics,
            source_map,
            cov,
            flat_locals: std::collections::HashMap::new(),
            flat_int_locals: std::collections::HashSet::new(),
            flat_float_locals: std::collections::HashSet::new(),
            uint_display_locals: std::collections::HashSet::new(),
            reference_alias_regs: std::collections::HashSet::new(),
            capture_cell_names: std::collections::HashSet::new(),
            capture_cells: Vec::new(),
            capture_cells_used: false,
            escaped_reference_reg_floor: 0,
            collection_locals: std::collections::HashSet::new(),
            lazy_iterator_locals: std::collections::HashSet::new(),
            flag_set_locals: std::collections::HashSet::new(),
            duration_cell_locals: std::collections::HashSet::new(),
            instrs: Vec::new(),
            instruction_locations: Vec::new(),
            inline_sites: Vec::new(),
            current_inline_site: None,
            consts: Vec::new(),
            const_cache: HashMap::new(),
            f64_consts: Vec::new(),
            f64_const_cache: HashMap::new(),
            i64_consts: Vec::new(),
            i64_const_cache: HashMap::new(),
            globals: Vec::new(),
            shape_names: Vec::new(),
            global_cache: HashMap::new(),
            next_reg: 0,
            next_float_reg: 0,
            next_int_reg: 0,
            max_reg: 0,
            max_float_reg: 0,
            max_int_reg: 0,
            scopes: vec![Scope::default()],
            loop_stack: Vec::new(),
            pending_loop_label: None,
            defer_stack: Vec::new(),
            closure_protos: Vec::new(),
            select_arms: Vec::new(),
            wide_ops: Vec::new(),
            next_cache_idx: 0,
            next_arith_cache_idx: 0,
            next_field_cache_idx: 0,
            mut_ref_params: Vec::new(),
            i64_params: Vec::new(),
            consumable: std::collections::HashSet::new(),
        }
    }

    pub(crate) fn bind_param(&mut self, pattern: &HirPat, reg: Reg) -> RuntimeResult<()> {
        self.bind_pattern_locals(pattern, reg)
    }

    pub(crate) fn bind_typed_param(&mut self, pattern: &HirPat, reg: Reg, kind: RegKind) {
        if let HirPatKind::Binding { name, .. } = &pattern.kind {
            self.bind_local(&name.name, TypedReg { reg, kind });
        }
    }

    pub(crate) fn finish(mut self, arity: u16) -> FnChunk {
        debug_assert_eq!(self.instrs.len(), self.instruction_locations.len());
        optimize_float_accumulator_moves(&mut self.instrs, &mut self.instruction_locations);
        optimize_i64_to_f64_divs(&mut self.instrs, &mut self.instruction_locations);
        if !self.capture_cells_used {
            optimize_tail_call_argument_moves(&mut self.instrs, &self.mut_ref_params);
        }
        let instruction_locations = encode_instruction_locations(&self.instruction_locations);
        let inline_sites = crate::value::intern_inline_sites(&self.inline_sites);
        let mut chunk = FnChunk {
            name: self.name,
            arity,
            register_count: self.max_reg.max(self.next_reg),
            float_count: self.max_float_reg.max(self.next_float_reg),
            int_count: self.max_int_reg.max(self.next_int_reg),
            instrs: self.instrs,
            instruction_locations,
            inline_sites,
            consts: self.consts,
            f64_consts: self.f64_consts,
            i64_consts: self.i64_consts,
            globals: self.globals,
            shape_names: self.shape_names,
            closure_protos: self.closure_protos,
            select_arms: self.select_arms,
            wide_ops: self.wide_ops,
            // The actual cache `Vec`s live in per-`Vm`
            // `ChunkState` and are sized from these counts. See
            // `vm::Vm::chunk_state_for`.
            call_cache_count: self.next_cache_idx,
            arith_cache_count: self.next_arith_cache_idx,
            field_cache_count: self.next_field_cache_idx,
            mut_ref_params: self.mut_ref_params,
            i64_params: self.i64_params,
        };
        // Release growth-by-doubling slack on every Vec field
        // unconditionally - any code path that produces a chunk
        // via `finish` benefits, with no risk of a future caller
        // forgetting an explicit compact() call.
        chunk.compact();
        chunk
    }
}

/// A caller has no live locals after `Call; Return dst`. Transfer its argument
/// handles into the child frame instead of cloning them. The second rewrite
/// follows the common shared-borrow materialization chain
/// `Move local -> borrow_temp; Move borrow_temp -> arg_slot` so borrowed
/// aggregate tail calls also avoid an otherwise redundant retain/release pair.
///
/// A `&mut` parameter register in `mut_ref_params` stays live past the call:
/// the frame publishes its final value back to the caller's write-back cell
/// on the return path, so its handle is not the child's to take.
fn optimize_tail_call_argument_moves(instrs: &mut [Op], mut_ref_params: &[Reg]) {
    for call_idx in 0..instrs.len().saturating_sub(1) {
        let (dst, args, argc) = match instrs[call_idx] {
            Op::Call {
                dst, args, argc, ..
            }
            | Op::CallGlobal {
                dst, args, argc, ..
            } => (dst, args, argc),
            _ => continue,
        };
        if !matches!(instrs[call_idx + 1], Op::Return { value } if value == dst) {
            continue;
        }
        let Some(first_move_idx) = call_idx.checked_sub(usize::from(argc)) else {
            continue;
        };
        for offset in 0..argc {
            let arg_slot = args + offset;
            let move_idx = first_move_idx + usize::from(offset);
            let Op::Move { dst: move_dst, src } = instrs[move_idx] else {
                continue;
            };
            if move_dst != arg_slot || mut_ref_params.contains(&src) {
                continue;
            }
            instrs[move_idx] = Op::MoveConsume { dst: arg_slot, src };

            if move_idx > 0
                && let Op::Move {
                    dst: producer_dst,
                    src: original,
                } = instrs[move_idx - 1]
                && producer_dst == src
                && !mut_ref_params.contains(&original)
            {
                instrs[move_idx - 1] = Op::MoveConsume {
                    dst: src,
                    src: original,
                };
            }
        }
    }
}

fn optimize_i64_to_f64_divs(instrs: &mut Vec<Op>, locations: &mut Vec<super::InstrSource>) {
    if instrs.len() < 2 {
        return;
    }

    let branch_targets = branch_targets(instrs);
    let mut remove = vec![false; instrs.len()];
    let mut idx = 0usize;
    while idx + 1 < instrs.len() {
        let Op::IntToFloatF64 {
            dst_f: cast_f,
            src_i,
        } = instrs[idx]
        else {
            idx += 1;
            continue;
        };
        let Op::DivF64 {
            dst_f,
            lhs_f,
            rhs_f,
        } = instrs[idx + 1]
        else {
            idx += 1;
            continue;
        };
        if rhs_f != cast_f
            || branch_targets[idx]
            || float_reg_read_before_write(&instrs[idx + 2..], cast_f)
        {
            idx += 1;
            continue;
        }
        instrs[idx + 1] = Op::DivF64ByI64 {
            dst_f,
            lhs_f,
            rhs_i: src_i,
        };
        remove[idx] = true;
        idx += 2;
    }

    if remove.iter().any(|drop| *drop) {
        compact_instrs(instrs, locations, &remove);
    }
}

fn optimize_float_accumulator_moves(instrs: &mut Vec<Op>, locations: &mut Vec<super::InstrSource>) {
    if instrs.len() < 2 {
        return;
    }

    let branch_targets = branch_targets(instrs);
    let mut remove = vec![false; instrs.len()];
    let mut idx = 0usize;
    while idx + 1 < instrs.len() {
        let Some((old_dst, a_f, b_f, c_f, is_sub)) = mul_fused_parts(instrs[idx]) else {
            idx += 1;
            continue;
        };
        let Op::MoveF64 { dst_f, src_f } = instrs[idx + 1] else {
            idx += 1;
            continue;
        };
        if src_f != old_dst
            || dst_f == old_dst
            || branch_targets[idx + 1]
            || float_reg_read_before_write(&instrs[idx + 2..], old_dst)
        {
            idx += 1;
            continue;
        }
        instrs[idx] = if is_sub {
            Op::MulSubF64 {
                dst_f,
                a_f,
                b_f,
                c_f,
            }
        } else {
            Op::MulAddF64 {
                dst_f,
                a_f,
                b_f,
                c_f,
            }
        };
        remove[idx + 1] = true;
        idx += 2;
    }

    if remove.iter().any(|drop| *drop) {
        compact_instrs(instrs, locations, &remove);
    }
}

fn mul_fused_parts(op: Op) -> Option<(Reg, Reg, Reg, Reg, bool)> {
    match op {
        Op::MulAddF64 {
            dst_f,
            a_f,
            b_f,
            c_f,
        } => Some((dst_f, a_f, b_f, c_f, false)),
        Op::MulSubF64 {
            dst_f,
            a_f,
            b_f,
            c_f,
        } => Some((dst_f, a_f, b_f, c_f, true)),
        _ => None,
    }
}

fn float_reg_read_before_write(instrs: &[Op], reg: Reg) -> bool {
    for op in instrs {
        if op_reads_float(*op, reg) {
            return true;
        }
        if op_writes_float(*op, reg) {
            return false;
        }
    }
    false
}

fn op_reads_float(op: Op, reg: Reg) -> bool {
    match op {
        Op::AddF64 {
            lhs_f: a, rhs_f: b, ..
        }
        | Op::SubF64 {
            lhs_f: a, rhs_f: b, ..
        }
        | Op::MulF64 {
            lhs_f: a, rhs_f: b, ..
        }
        | Op::DivF64 {
            lhs_f: a, rhs_f: b, ..
        }
        | Op::LtF64 {
            lhs_f: a, rhs_f: b, ..
        }
        | Op::LeF64 {
            lhs_f: a, rhs_f: b, ..
        }
        | Op::GtF64 {
            lhs_f: a, rhs_f: b, ..
        }
        | Op::GeF64 {
            lhs_f: a, rhs_f: b, ..
        }
        | Op::EqF64 {
            lhs_f: a, rhs_f: b, ..
        }
        | Op::NeF64 {
            lhs_f: a, rhs_f: b, ..
        }
        | Op::BranchIfLtF64 {
            lhs_f: a, rhs_f: b, ..
        }
        | Op::BranchIfGeF64 {
            lhs_f: a, rhs_f: b, ..
        } => a == reg || b == reg,
        Op::DivF64ByI64 { lhs_f, .. } => lhs_f == reg,
        Op::NegF64 { src_f, .. }
        | Op::BoxF64 { src_f, .. }
        | Op::SqrtF64 { src_f, .. }
        | Op::SinF64 { src_f, .. }
        | Op::CosF64 { src_f, .. }
        | Op::AbsF64 { src_f, .. }
        | Op::FloorF64 { src_f, .. }
        | Op::CeilF64 { src_f, .. }
        | Op::ExpF64 { src_f, .. }
        | Op::LnF64 { src_f, .. }
        | Op::FloatToIntI64 { src_f, .. }
        | Op::MoveF64 { src_f, .. } => src_f == reg,
        Op::MulAddF64 { a_f, b_f, c_f, .. } | Op::MulSubF64 { a_f, b_f, c_f, .. } => {
            a_f == reg || b_f == reg || c_f == reg
        }
        Op::FloatVecSetF64 { value_f, .. }
        | Op::FlatSetF64 { value_f, .. }
        | Op::FlatSetF64I { value_f, .. }
        | Op::IndexedFieldSetF64 { value_f, .. }
        | Op::IndexedFieldSetF64ByOffset { value_f, .. } => value_f == reg,
        _ => false,
    }
}

fn op_writes_float(op: Op, reg: Reg) -> bool {
    match op {
        Op::LoadConstF64 { dst_f, .. }
        | Op::AddF64 { dst_f, .. }
        | Op::SubF64 { dst_f, .. }
        | Op::MulF64 { dst_f, .. }
        | Op::DivF64 { dst_f, .. }
        | Op::DivF64ByI64 { dst_f, .. }
        | Op::NegF64 { dst_f, .. }
        | Op::UnboxF64 { dst_f, .. }
        | Op::SqrtF64 { dst_f, .. }
        | Op::SinF64 { dst_f, .. }
        | Op::CosF64 { dst_f, .. }
        | Op::AbsF64 { dst_f, .. }
        | Op::FloorF64 { dst_f, .. }
        | Op::CeilF64 { dst_f, .. }
        | Op::ExpF64 { dst_f, .. }
        | Op::LnF64 { dst_f, .. }
        | Op::MulAddF64 { dst_f, .. }
        | Op::MulSubF64 { dst_f, .. }
        | Op::MoveF64 { dst_f, .. }
        | Op::FieldGetF64 { dst_f, .. }
        | Op::IndexedFieldGetF64 { dst_f, .. }
        | Op::IndexedFieldGetF64ByOffset { dst_f, .. }
        | Op::FieldGetF64ByOffset { dst_f, .. }
        | Op::FlatGetF64 { dst_f, .. }
        | Op::FlatGetF64I { dst_f, .. }
        | Op::IntToFloatF64 { dst_f, .. }
        | Op::FloatVecGetF64 { dst_f, .. } => dst_f == reg,
        _ => false,
    }
}

fn branch_targets(instrs: &[Op]) -> Vec<bool> {
    let mut targets = vec![false; instrs.len()];
    for op in instrs {
        if let Some(target) = op_target(*op)
            && let Some(slot) = targets.get_mut(target as usize)
        {
            *slot = true;
        }
    }
    targets
}

fn compact_instrs(instrs: &mut Vec<Op>, locations: &mut Vec<super::InstrSource>, remove: &[bool]) {
    debug_assert_eq!(instrs.len(), locations.len());
    debug_assert_eq!(instrs.len(), remove.len());
    let mut remap = vec![0u32; instrs.len() + 1];
    let mut next = 0u32;
    for (idx, drop) in remove.iter().copied().enumerate() {
        remap[idx] = next;
        if !drop {
            next += 1;
        }
    }
    remap[instrs.len()] = next;

    let mut compacted = Vec::with_capacity(next as usize);
    let mut compacted_locations = Vec::with_capacity(next as usize);
    for (idx, op) in instrs.iter().copied().enumerate() {
        if !remove[idx] {
            compacted.push(remap_op_target(op, &remap));
            compacted_locations.push(locations[idx]);
        }
    }
    *instrs = compacted;
    *locations = compacted_locations;
}

fn encode_instruction_locations(
    sources: &[super::InstrSource],
) -> Vec<crate::bytecode::InstructionLocation> {
    if sources
        .iter()
        .all(|source| source.location.is_none() && source.inline_site.is_none())
    {
        return Vec::new();
    }
    let mut encoded = Vec::new();
    let mut previous = None;
    for (idx, source) in sources.iter().copied().enumerate() {
        if previous != Some(source) {
            encoded.push(crate::bytecode::InstructionLocation {
                instruction: u32::try_from(idx).expect("instruction overflow"),
                location: source.location,
                inline_site: source.inline_site,
            });
            previous = Some(source);
        }
    }
    encoded
}

fn op_target(op: Op) -> Option<InstrIdx> {
    match op {
        Op::Jump { target }
        | Op::BranchIf { target, .. }
        | Op::BranchIfNot { target, .. }
        | Op::BranchIfLtI64 { target, .. }
        | Op::BranchIfGeI64 { target, .. }
        | Op::BranchIfGtI64 { target, .. }
        | Op::BranchIfLtF64 { target, .. }
        | Op::BranchIfGeF64 { target, .. }
        | Op::IncJumpIfLtI64 { target, .. }
        | Op::IncJumpIfLeI64 { target, .. } => Some(target),
        _ => None,
    }
}

fn remap_op_target(mut op: Op, remap: &[InstrIdx]) -> Op {
    let target = match &mut op {
        Op::Jump { target }
        | Op::BranchIf { target, .. }
        | Op::BranchIfNot { target, .. }
        | Op::BranchIfLtI64 { target, .. }
        | Op::BranchIfGeI64 { target, .. }
        | Op::BranchIfGtI64 { target, .. }
        | Op::BranchIfLtF64 { target, .. }
        | Op::BranchIfGeF64 { target, .. }
        | Op::IncJumpIfLtI64 { target, .. }
        | Op::IncJumpIfLeI64 { target, .. } => target,
        _ => return op,
    };
    *target = remap[*target as usize];
    op
}
