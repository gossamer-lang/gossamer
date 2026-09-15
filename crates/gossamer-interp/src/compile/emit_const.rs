#![allow(clippy::too_many_lines, clippy::wildcard_imports)]
use super::*;

impl<'tcx> FnBuilder<'tcx> {
    pub(crate) fn f64_const_idx(&mut self, value: f64) -> ConstIdx {
        let key = value.to_bits();
        if let Some(idx) = self.f64_const_cache.get(&key) {
            return *idx;
        }
        let idx = ConstIdx::try_from(self.f64_consts.len()).expect("f64 const pool overflow");
        self.f64_consts.push(value);
        self.f64_const_cache.insert(key, idx);
        idx
    }

    pub(crate) fn i64_const_idx(&mut self, value: i64) -> ConstIdx {
        if let Some(idx) = self.i64_const_cache.get(&value) {
            return *idx;
        }
        let idx = ConstIdx::try_from(self.i64_consts.len()).expect("i64 const pool overflow");
        self.i64_consts.push(value);
        self.i64_const_cache.insert(value, idx);
        idx
    }

    /// Appends `op` verbatim, outside the capture-cell bracketing. Used
    /// by the cell traffic itself and by the ops that install a cell.
    pub(crate) fn push_instr(&mut self, op: Op) -> InstrIdx {
        let idx = u32::try_from(self.instrs.len()).expect("instruction overflow");
        self.instrs.push(op);
        self.instruction_locations.push(super::InstrSource {
            location: None,
            inline_site: self.current_inline_site,
        });
        idx
    }

    /// Appends `op`, preceded and followed by the capture-cell traffic
    /// every binding it names requires. Returns the index of `op`
    /// itself, so a jump emitted here is still patched through its own
    /// slot and a label taken beforehand still names the whole sequence.
    pub(crate) fn emit(&mut self, op: Op) -> InstrIdx {
        if self.capture_cells.is_empty() {
            return self.push_instr(op);
        }
        self.emit_through_capture_cells(op)
    }

    fn emit_through_capture_cells(&mut self, op: Op) -> InstrIdx {
        let effects = crate::validate::register_effects(
            op,
            &self.closure_protos,
            &self.select_arms,
            &self.wide_ops,
        );
        let mut loads: Vec<Op> = Vec::new();
        let mut stores: Vec<Op> = Vec::new();
        for &(home, cell) in &self.capture_cells {
            let reads = effects.v_reads.contains(&home);
            let writes = effects.v_writes.contains(&home);
            match (reads, writes) {
                (false, false) => {}
                (false, true) => stores.push(Op::CaptureCellSet { cell, src: home }),
                (true, _) if reads_capture_binding_by_value(op, home) => {
                    loads.push(Op::CaptureCellGet { dst: home, cell });
                    if writes {
                        stores.push(Op::CaptureCellSet { cell, src: home });
                    }
                }
                (true, _) => {
                    loads.push(Op::CaptureCellTake { dst: home, cell });
                    stores.push(Op::CaptureCellSet { cell, src: home });
                }
            }
        }
        for load in loads {
            self.push_instr(load);
        }
        let idx = self.push_instr(op);
        for store in stores {
            self.push_instr(store);
        }
        idx
    }

    pub(crate) fn annotate_instructions(&mut self, start: usize, span: gossamer_lex::Span) {
        let Some(location) = self.source_location(span) else {
            return;
        };
        for slot in &mut self.instruction_locations[start..] {
            if slot.location.is_none() {
                slot.location = Some(location);
            }
        }
    }

    /// Where `span` starts in the source a traceback names.
    pub(crate) fn source_location(
        &self,
        span: gossamer_lex::Span,
    ) -> Option<crate::bytecode::SourceLocation> {
        Some(super::resolve_source_location(self.source_map?, span))
    }

    pub(crate) fn cur_idx(&self) -> InstrIdx {
        u32::try_from(self.instrs.len()).expect("instruction overflow")
    }

    pub(crate) fn patch_jump(&mut self, idx: InstrIdx, target: InstrIdx) {
        match &mut self.instrs[idx as usize] {
            Op::Jump { target: t }
            | Op::BranchIf { target: t, .. }
            | Op::BranchIfNot { target: t, .. }
            | Op::BranchIfLtI64 { target: t, .. }
            | Op::BranchIfGeI64 { target: t, .. }
            | Op::BranchIfGtI64 { target: t, .. }
            | Op::BranchIfLtF64 { target: t, .. }
            | Op::BranchIfGeF64 { target: t, .. } => *t = target,
            other => panic!("cannot patch non-jump: {other:?}"),
        }
    }

    /// Index of an INTERNED shape name in `shape_names` (the
    /// `VariantIs` / `StructIs` operand table), deduplicated by
    /// pointer identity.
    pub(crate) fn shape_name_idx(&mut self, name: &str) -> ConstIdx {
        let interned = crate::value::intern_type_name(name);
        if let Some(pos) = self
            .shape_names
            .iter()
            .position(|n| std::ptr::eq(*n, interned))
        {
            return ConstIdx::try_from(pos).unwrap_or(0);
        }
        self.shape_names.push(interned);
        ConstIdx::try_from(self.shape_names.len() - 1).unwrap_or(0)
    }

    pub(crate) fn const_idx(&mut self, key: ConstKey, value: Value) -> ConstIdx {
        if let Some(idx) = self.const_cache.get(&key) {
            return *idx;
        }
        let idx = ConstIdx::try_from(self.consts.len()).expect("const pool overflow");
        self.consts.push(value);
        self.const_cache.insert(key, idx);
        idx
    }

    pub(crate) fn global_idx(&mut self, name: &str) -> GlobalIdx {
        if let Some(idx) = self.global_cache.get(name) {
            return *idx;
        }
        let idx = GlobalIdx::try_from(self.globals.len()).expect("global pool overflow");
        self.globals.push(name.into());
        self.global_cache.insert(name.to_string(), idx);
        idx
    }
}

/// `true` when `op` reads the capture-cell binding in register `home`
/// only as a plain value it hands on - a call or goroutine argument, a
/// returned value, a branch operand. Such an instruction leaves the cell
/// populated, so a closure reached from inside it still observes the
/// binding. Every other instruction borrows the binding exclusively for
/// its own duration: the value moves into the home register so an
/// in-place mutation keeps its refcount at one, and the paired store
/// returns it.
fn reads_capture_binding_by_value(op: Op, home: Reg) -> bool {
    match op {
        Op::Call { .. }
        | Op::CallGlobal { .. }
        | Op::MakeClosure { .. }
        | Op::Select { .. }
        | Op::Return { .. }
        | Op::Jump { .. }
        | Op::BranchIf { .. }
        | Op::BranchIfNot { .. }
        | Op::BranchIfLtI64 { .. }
        | Op::BranchIfGeI64 { .. }
        | Op::BranchIfGtI64 { .. }
        | Op::BranchIfLtF64 { .. }
        | Op::BranchIfGeF64 { .. }
        | Op::IncJumpIfLtI64 { .. }
        | Op::IncJumpIfLeI64 { .. } => true,
        Op::MethodCall { receiver, .. } => receiver != home,
        _ => false,
    }
}
