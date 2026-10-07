//! Landing pads that run pending `defer`s when a fault unwinds a frame.
//!
//! The deferred expressions pending at an instruction depend only on where
//! the instruction sits in the source, so the chunk is cut into spans at
//! every point the pending set changes. A span with pending defers gets a
//! landing pad, compiled in place behind a jump so its expressions resolve
//! the locals they name exactly as the normal exit's copies do.

#![allow(clippy::wildcard_imports)]
use super::*;
use crate::bytecode::{UnwindEntry, UnwindKind};

impl FnBuilder<'_> {
    /// The deferred expressions a fault at the current instruction runs, in
    /// the order it runs them: innermost block first, each block's last
    /// registered first.
    fn pending_defers(&self) -> Vec<HirExpr> {
        let mut pending = Vec::new();
        for (frame_idx, frame) in self.defer_stack.iter().enumerate().rev() {
            for (expr_idx, expr) in frame.iter().enumerate().rev() {
                if !self.running_defers.contains(&(frame_idx, expr_idx)) {
                    pending.push(expr.clone());
                }
            }
        }
        pending
    }

    /// Ends the span whose pending set is about to change, giving it a
    /// landing pad when it holds instructions and pending defers.
    pub(crate) fn close_unwind_region(&mut self) -> RuntimeResult<()> {
        if self.in_unwind_pad > 0 {
            return Ok(());
        }
        let start = self.unwind_region_start;
        let end = self.cur_idx();
        let pending = self.pending_defers();
        if end > start && !pending.is_empty() {
            let skip = self.emit(Op::Jump { target: 0 });
            let landing = self.cur_idx();
            let mark = self.register_mark();
            self.in_unwind_pad += 1;
            let mut compiled = Ok(());
            for expr in &pending {
                let note_start = self.cur_idx();
                if let Err(err) = self.compile_expr(expr) {
                    compiled = Err(err);
                    break;
                }
                let note_end = self.cur_idx();
                if note_end > note_start {
                    self.unwind_table.push(UnwindEntry {
                        start: note_start,
                        end: note_end,
                        landing: note_end,
                        kind: UnwindKind::Note,
                    });
                }
            }
            self.emit(Op::ResumeUnwind);
            self.in_unwind_pad -= 1;
            self.restore_register_mark(mark);
            compiled?;
            let after = self.cur_idx();
            self.patch_jump(skip, after);
            self.unwind_table.push(UnwindEntry {
                start,
                end,
                landing,
                kind: UnwindKind::Cleanup,
            });
        }
        self.unwind_region_start = self.cur_idx();
        Ok(())
    }

    /// Registers `expr` as deferred in the innermost block.
    pub(crate) fn push_defer(&mut self, expr: HirExpr) -> RuntimeResult<()> {
        self.close_unwind_region()?;
        if let Some(frame) = self.defer_stack.last_mut() {
            frame.push(expr);
        }
        Ok(())
    }

    /// Marks the `expr_idx`th defer of frame `frame_idx` as running on the
    /// exit edge being compiled, so a fault inside it does not run it again.
    pub(crate) fn mark_defer_running(
        &mut self,
        frame_idx: usize,
        expr_idx: usize,
    ) -> RuntimeResult<()> {
        if self.in_unwind_pad > 0 {
            return Ok(());
        }
        self.close_unwind_region()?;
        self.running_defers.push((frame_idx, expr_idx));
        Ok(())
    }

    /// Ends an exit edge: the defers it ran are pending again for the code
    /// that follows, which leaves the block some other way.
    pub(crate) fn finish_defer_edge(&mut self) -> RuntimeResult<()> {
        if self.in_unwind_pad > 0 || self.running_defers.is_empty() {
            return Ok(());
        }
        self.close_unwind_region()?;
        self.running_defers.clear();
        Ok(())
    }

    /// Pops the innermost block's defer frame, ending the span its defers
    /// were pending in.
    pub(crate) fn pop_defer_frame(&mut self) -> RuntimeResult<Vec<HirExpr>> {
        let changes = self
            .defer_stack
            .last()
            .is_some_and(|frame| !frame.is_empty());
        if changes {
            self.close_unwind_region()?;
        }
        Ok(self.defer_stack.pop().unwrap_or_default())
    }
}
