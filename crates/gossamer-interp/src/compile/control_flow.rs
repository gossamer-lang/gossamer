#![allow(clippy::too_many_lines, clippy::wildcard_imports)]
use super::*;

impl<'tcx> FnBuilder<'tcx> {
    pub(crate) fn compile_if(
        &mut self,
        condition: &HirExpr,
        then_branch: &HirExpr,
        else_branch: Option<&HirExpr>,
    ) -> RuntimeResult<Reg> {
        let cond_reg = self.compile_expr(condition)?;
        let result = self.alloc_reg();
        let branch_idx = self.emit(Op::BranchIfNot {
            cond: cond_reg,
            target: 0,
        });
        let then_reg = self.compile_expr(then_branch)?;
        self.emit(Op::Move {
            dst: result,
            src: then_reg,
        });
        let jump_end = self.emit(Op::Jump { target: 0 });
        let else_start = self.cur_idx();
        self.patch_jump(branch_idx, else_start);
        if let Some(else_branch) = else_branch {
            let else_reg = self.compile_expr(else_branch)?;
            self.emit(Op::Move {
                dst: result,
                src: else_reg,
            });
        } else {
            let unit_reg = self.load_unit();
            self.emit(Op::Move {
                dst: result,
                src: unit_reg,
            });
        }
        let after = self.cur_idx();
        self.patch_jump(jump_end, after);
        Ok(result)
    }

    /// Compiles `if cond { … } else { … }` whose value is discarded
    /// (statement position). Each branch is compiled in statement
    /// context via `compile_expr_discarded`, so a tail-position
    /// in-place mutation (`v.push(x)`) lowers to its dedicated op
    /// rather than the value-returning builtin path that deep-copies
    /// the whole collection per call. No result register is allocated
    /// and no per-branch `Move` is emitted.
    pub(crate) fn compile_if_discarded(
        &mut self,
        condition: &HirExpr,
        then_branch: &HirExpr,
        else_branch: Option<&HirExpr>,
    ) -> RuntimeResult<()> {
        let outer_register_mark = self.register_mark();
        let cond_reg = self.compile_expr(condition)?;
        let branch_idx = self.emit(Op::BranchIfNot {
            cond: cond_reg,
            target: 0,
        });
        let branch_register_mark = self.register_mark();
        self.compile_expr_discarded(then_branch)?;
        self.restore_register_mark(branch_register_mark);
        if let Some(else_branch) = else_branch {
            let jump_end = self.emit(Op::Jump { target: 0 });
            let else_start = self.cur_idx();
            self.patch_jump(branch_idx, else_start);
            self.compile_expr_discarded(else_branch)?;
            self.restore_register_mark(branch_register_mark);
            let after = self.cur_idx();
            self.patch_jump(jump_end, after);
        } else {
            let after = self.cur_idx();
            self.patch_jump(branch_idx, after);
        }
        self.restore_register_mark(outer_register_mark);
        Ok(())
    }

    pub(crate) fn compile_while(
        &mut self,
        condition: &HirExpr,
        body: &HirExpr,
    ) -> RuntimeResult<Reg> {
        // Fused-branch fast path: `while lhs < rhs` / `while
        // lhs >= rhs` etc. on typed i64 / f64 operands gets
        // lowered to a single `BranchIfGeI64` / `BranchIfLtI64`
        // that pairs the comparison with the exit jump. Cuts
        // two dispatched ops (typed compare + BranchIfNot /
        // BranchIf) down to one per loop iteration.
        //
        // Loop-invariant literal operands get hoisted above
        // `loop_start` so the LoadConst ops don't re-execute
        // per iteration.
        let label = self.pending_loop_label.take();
        let hoisted = self.try_hoist_condition_literals(condition)?;
        let loop_start = self.cur_idx();
        let exit_patch = if let Some((lhs_reg, rhs_reg, op, kind)) = hoisted {
            Some(self.emit_fused_exit_branch(op, kind, lhs_reg, rhs_reg))
        } else {
            self.try_compile_fused_exit_branch(condition)?
        }
        .unwrap_or_else(|| {
            let cond_reg = self.compile_expr(condition).unwrap_or(0);
            self.emit(Op::BranchIfNot {
                cond: cond_reg,
                target: 0,
            })
        });
        let result = self.alloc_reg();
        self.loop_stack.push(LoopCtx {
            break_patches: Vec::new(),
            continue_patches: Vec::new(),
            result_reg: result,
            defer_depth: self.defer_stack.len(),
            label,
        });
        let body_value_regs_start = self.next_reg;
        self.compile_loop_body(body)?;
        self.emit_iteration_value_release(body_value_regs_start);
        self.emit(Op::Jump { target: loop_start });
        let after = self.cur_idx();
        self.patch_jump(exit_patch, after);
        let ctx = self
            .loop_stack
            .pop()
            .expect("loop stack underflow on while");
        for patch in ctx.break_patches {
            self.patch_jump(patch, after);
        }
        // `continue` in a `while` loop re-evaluates the condition,
        // so route every recorded patch back to `loop_start` -
        // identical semantics to the previous direct-jump form.
        for patch in ctx.continue_patches {
            self.patch_jump(patch, loop_start);
        }
        Ok(self.load_unit())
    }

    pub(crate) fn compile_loop(&mut self, body: &HirExpr) -> RuntimeResult<Reg> {
        let label = self.pending_loop_label.take();
        let loop_start = self.cur_idx();
        let result = self.alloc_reg();
        self.loop_stack.push(LoopCtx {
            break_patches: Vec::new(),
            continue_patches: Vec::new(),
            result_reg: result,
            defer_depth: self.defer_stack.len(),
            label,
        });
        let body_value_regs_start = self.next_reg;
        self.compile_loop_body(body)?;
        self.emit_iteration_value_release(body_value_regs_start);
        self.emit(Op::Jump { target: loop_start });
        let after = self.cur_idx();
        let ctx = self.loop_stack.pop().expect("loop stack underflow on loop");
        for patch in ctx.break_patches {
            self.patch_jump(patch, after);
        }
        // Bare `loop` re-enters at the body's first op, so
        // `continue` routes back to `loop_start`.
        for patch in ctx.continue_patches {
            self.patch_jump(patch, loop_start);
        }
        Ok(result)
    }

    /// Releases the loop body's per-iteration `Value` registers at the
    /// fall-through back-edge, so an aggregate built this iteration (a tree,
    /// a scratch `Vec`, the temporary tuple a `let a, b = f()` destructure
    /// leaves behind) is dropped before the next iteration allocates,
    /// instead of staying live in its register until the next write. Without
    /// this, an iteration's freshly built structure overlaps the next
    /// iteration's, doubling the peak working set of a build-then-rebuild
    /// loop. The interpreter analog of the compiled tier's region bulk-free.
    ///
    /// Clears the whole `[start, next_reg)` `Value`-register span the body
    /// allocated - named locals and anonymous temporaries alike - because a
    /// temporary (the destructure tuple) can outlive the named binding it
    /// fed. `i64`/`f64` registers live in separate spans and are `Copy`, so
    /// they are untouched. Output-invariant, preserving tier parity:
    /// Gossamer values have no observable finalizer; loop-carried state and
    /// the loop's own result register were allocated before `start`; and any
    /// value that escaped the iteration (a closure capture, a push into an
    /// outer collection) already holds its own clone. `break`/`continue`
    /// leave by their own jump and skip this, which only forgoes the early
    /// drop - never correctness.
    pub(crate) fn emit_iteration_value_release(&mut self, start: Reg) {
        let count = self.next_reg.saturating_sub(start);
        if count == 0 {
            return;
        }
        self.emit(Op::ClearRegs { start, count });
    }

    pub(crate) fn compile_return(&mut self, value: Option<&HirExpr>) -> RuntimeResult<Reg> {
        let reg = match value {
            Some(value) => {
                let reg = self.compile_expr(value)?;
                let reg = self.cloned_returned_field_container(value, reg);
                self.detach_from_pending_defers(reg)
            }
            None => self.load_unit(),
        };
        // `return` leaves every enclosing block: run all pending defer
        // frames (LIFO, innermost first) after the return value is
        // computed, before the actual Return. The HIR desugars `?` into a
        // `match` with an early `return Err(...)`, so this also runs the
        // defers above the `?` site before the error propagates.
        self.emit_defers_above(0)?;
        self.emit(Op::Return { value: reg });
        Ok(reg)
    }

    pub(crate) fn compile_break(
        &mut self,
        value: Option<&HirExpr>,
        label: Option<&str>,
    ) -> RuntimeResult<Reg> {
        let reg = match value {
            Some(value) => self.compile_expr(value)?,
            None => self.load_unit(),
        };
        let idx = self
            .resolve_loop_target(label)
            .ok_or(RuntimeError::Unsupported("break outside of loop"))?;
        let (result_reg, defer_depth) = {
            let ctx = &self.loop_stack[idx];
            (ctx.result_reg, ctx.defer_depth)
        };
        self.emit(Op::Move {
            dst: result_reg,
            src: reg,
        });
        // Run the defers of the blocks being exited (the loop body and any
        // nested blocks), but not the loop's enclosing frames.
        self.emit_defers_above(defer_depth)?;
        let patch = self.emit(Op::Jump { target: 0 });
        self.loop_stack[idx].break_patches.push(patch);
        Ok(reg)
    }

    /// Index into `loop_stack` of the loop a `break`/`continue` targets:
    /// the innermost loop carrying a matching label, or the innermost
    /// loop of any label when no label is given.
    pub(crate) fn resolve_loop_target(&self, label: Option<&str>) -> Option<usize> {
        match label {
            None => self.loop_stack.len().checked_sub(1),
            Some(name) => self
                .loop_stack
                .iter()
                .rposition(|ctx| ctx.label.as_deref() == Some(name)),
        }
    }
}
