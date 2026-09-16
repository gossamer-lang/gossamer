//! Heap-allocator cleanup analysis.
//!
//! Identifies locals that own the result of a runtime heap-allocator
//! call and computes a per-local drop position so the codegen can
//! release the allocation as soon as the value is no longer live -
//! not at the function's `Return`. Without per-block drops, every
//! owning binding stays alive until process exit, peak RSS carries
//! the union of all live + dead allocations, and `main()`-shaped
//! programs are forced to keep their input buffers around long
//! after the last use.
//!
//! The drop placement uses a small backward liveness pass per
//! candidate. Two rules cover the common shapes:
//!
//! - **Block-exit drop:** if a block uses (or holds live) the local
//!   and no successor block sees it live, drop at the end of that
//!   block (after the last statement, before the terminator). This
//!   matches the "last use was inside this block" pattern.
//!
//! - **Block-entry drop:** if a block does not use the local, no
//!   successor sees it live, but every predecessor had it live at
//!   exit, drop at the start of that block. This catches the
//!   "loop just finished" / "branch joined" pattern where the local
//!   transitions from live to dead between blocks.
//!
//! When neither rule fires (mixed-liveness predecessors, irreducible
//! CFGs), the local falls back to the legacy `at-Return` placement
//! - still better than leaking, just not as tight.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::escape::{CaptureSummary, analyse_with_summary as analyse_escape_with_summary};
use crate::ir::{
    BlockId, Body, ConstValue, Local, Operand, Place, Rvalue, StatementKind, Terminator,
};

/// Names of runtime functions that allocate on the heap and return
/// an owning pointer. Each entry maps a constructor symbol to the
/// matching reclamation symbol the codegen should call when the
/// destination local is dropped.
///
/// The constructor side covers both the user-facing names that MIR
/// preserves verbatim from the surface syntax (e.g. `U8Vec::new`)
/// and the runtime symbols that lowering occasionally produces
/// directly. Both forms reach the codegen as `Call { callee:
/// Const(Str(...)), ... }`, so the cleanup pass has to recognise
/// either string before its dominance check decides whether to emit
/// a free.
pub const HEAP_ALLOCATOR_PAIRS: &[(&str, &str)] = &[
    ("gos_rt_heap_i64_new", "gos_rt_heap_i64_free"),
    ("I64Vec::new", "gos_rt_heap_i64_free"),
    ("heap_i64::new", "gos_rt_heap_i64_free"),
    ("gos_rt_heap_u8_new", "gos_rt_heap_u8_free"),
    ("U8Vec::new", "gos_rt_heap_u8_free"),
    ("heap_u8::new", "gos_rt_heap_u8_free"),
    ("gos_rt_chan_new", "gos_rt_chan_drop"),
    // A spawn handle is a one-shot channel shared with the child. The
    // drop releases the handle's share; the child releases its own as
    // it leaves, and whichever party is last reclaims the storage.
    ("gos_rt_spawn", "gos_rt_chan_drop"),
    ("gos_rt_spawn_ex", "gos_rt_chan_drop"),
    // String allocators reachable from user code. The owning
    // pointer returned by `read_to_string` (and friends) lives in
    // the runtime's `Box<[u8]>::into_raw` domain; `gos_rt_str_free`
    // reverses the leak. The cleanup pass is gated by the same
    // escape-analysis filter as the heap-vec pairs, so only owning
    // bindings whose only uses are non-capturing reader helpers
    // (length / byte / substring read) reach the free.
    ("gos_rt_stream_read_to_string", "gos_rt_str_free"),
];

/// Where in the body a particular cleanup entry should be emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropAt {
    /// Emit the drop just before every `Return` terminator. Used as
    /// the fallback when liveness cannot pinpoint a tighter site.
    Return,
    /// Emit the drop at the start of `block`, before any other
    /// statement. Used when the local was alive in every predecessor
    /// of the block and this block (and its successors) no longer
    /// reference it.
    BlockEntry(BlockId),
    /// Emit the drop at the end of `block`, after the last statement
    /// and before the terminator. Used when the last live use of
    /// the local sits inside this block and no successor sees it.
    BlockExit(BlockId),
}

/// Per-body cleanup plan. Each entry identifies an owning heap
/// local and a `DropAt` site where the matching `_free` call
/// should be emitted. The list is in deterministic order
/// (`(local, drop_at)` ascending) so the codegens emit the same
/// byte stream on repeated runs.
#[derive(Debug, Clone, Default)]
pub struct CleanupPlan {
    entries: Vec<CleanupEntry>,
}

/// One owning heap local that the cleanup pass found.
#[derive(Debug, Clone, Copy)]
pub struct CleanupEntry {
    /// Local that holds the owning pointer.
    pub local: Local,
    /// Runtime symbol the codegen should call to free `local`.
    pub free_fn: &'static str,
    /// Where the drop should be emitted.
    pub drop_at: DropAt,
}

impl CleanupPlan {
    /// Returns every cleanup entry in stable order. Codegen tiers
    /// that still emit drops only at `Return` can filter via
    /// [`Self::at_return`].
    #[must_use]
    pub fn entries(&self) -> &[CleanupEntry] {
        &self.entries
    }

    /// Subset to emit at every `Return` terminator.
    pub fn at_return(&self) -> impl Iterator<Item = &CleanupEntry> {
        self.entries
            .iter()
            .filter(|e| matches!(e.drop_at, DropAt::Return))
    }

    /// Subset to emit at the start of `block`.
    pub fn at_block_entry(&self, block: BlockId) -> impl Iterator<Item = &CleanupEntry> {
        self.entries
            .iter()
            .filter(move |e| e.drop_at == DropAt::BlockEntry(block))
    }

    /// Subset to emit at the end of `block` (before its terminator).
    pub fn at_block_exit(&self, block: BlockId) -> impl Iterator<Item = &CleanupEntry> {
        self.entries
            .iter()
            .filter(move |e| e.drop_at == DropAt::BlockExit(block))
    }

    /// Whether the plan has anything to emit.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Returns the cleanup plan for `body`, consulting an empty
/// inter-procedural capture summary. Equivalent to
/// [`plan_with_summary`] with no callee data; kept for callers
/// that don't have a program-wide summary handy.
#[must_use]
pub fn plan(body: &Body) -> CleanupPlan {
    plan_with_summary(body, &CaptureSummary::default())
}

/// Returns the cleanup plan for `body`, using `summary` to refine
/// the escape analyser's decisions about user-fn calls. With a
/// converged summary, owning bindings whose only outbound use is a
/// non-capturing helper get a precise per-block drop instead of
/// being forced into the escape set and skipped.
#[must_use]
pub fn plan_with_summary(body: &Body, summary: &CaptureSummary) -> CleanupPlan {
    let alloc_pairs: BTreeMap<&str, &str> = HEAP_ALLOCATOR_PAIRS.iter().copied().collect();

    // Find every Call terminator whose callee is a known allocator
    // and whose destination is rooted in a single local with no
    // projections (i.e., a fresh local receiving the allocator's
    // return value).
    let mut candidates: Vec<(Local, &'static str, BlockId)> = Vec::new();
    for block in &body.blocks {
        if let Terminator::Call {
            callee,
            destination,
            ..
        } = &block.terminator
            && let Operand::Const(ConstValue::Str(name)) = callee
            && let Some(free_fn) = alloc_pairs.get(name.as_str()).copied()
            && destination.projection.is_empty()
        {
            candidates.push((destination.local, free_fn, block.id));
        }
    }
    if candidates.is_empty() {
        return CleanupPlan::default();
    }

    // Drop any candidate whose local escapes - return values, call
    // arguments, and aggregates are off-limits because the freed
    // memory may still be reachable by the caller.
    let escape = analyse_escape_with_summary(body, summary);
    candidates.retain(|(local, _, _)| escape.is_non_escaping(*local));
    if candidates.is_empty() {
        return CleanupPlan::default();
    }

    let return_blocks: Vec<BlockId> = body
        .blocks
        .iter()
        .filter(|b| matches!(b.terminator, Terminator::Return))
        .map(|b| b.id)
        .collect();
    if return_blocks.is_empty() {
        return CleanupPlan::default();
    }

    // Precompute the predecessor map; both liveness rules consult it.
    let predecessors = compute_predecessors(body);

    let mut seen: BTreeSet<u32> = BTreeSet::new();
    let mut entries: Vec<CleanupEntry> = Vec::new();
    for (local, free_fn, alloc_block) in candidates {
        if seen.contains(&local.0) {
            continue;
        }
        let drop_sites = compute_drop_sites(body, local, &predecessors);
        if !drop_sites.is_empty() {
            // Liveness already proved each drop site is reached
            // only on paths that allocated the local; per-block
            // drops are safe regardless of dominance over Return.
            seen.insert(local.0);
            for site in drop_sites {
                entries.push(CleanupEntry {
                    local,
                    free_fn,
                    drop_at: site,
                });
            }
            continue;
        }
        // No precise site → fall back to drop-at-Return, but only
        // when the allocator strictly dominates every Return. The
        // dominance check rules out conditional allocations whose
        // slot the early-return path may never have observed (and
        // freeing uninit memory at that Return would be UB).
        if !dominates_all(body, alloc_block, &return_blocks) {
            continue;
        }
        seen.insert(local.0);
        entries.push(CleanupEntry {
            local,
            free_fn,
            drop_at: DropAt::Return,
        });
    }
    entries.sort_by_key(|e| (e.local.0, drop_at_sort_key(e.drop_at)));
    CleanupPlan { entries }
}

fn drop_at_sort_key(d: DropAt) -> (u8, u32) {
    match d {
        DropAt::Return => (0, 0),
        DropAt::BlockEntry(b) => (1, b.as_u32()),
        DropAt::BlockExit(b) => (2, b.as_u32()),
    }
}

/// Backward liveness for a single owning local plus the drop-site
/// extraction described in the module header. Returns the list of
/// `DropAt` sites (or empty when no precise placement was found,
/// which signals "fall back to Return").
fn compute_drop_sites(body: &Body, local: Local, predecessors: &[Vec<BlockId>]) -> Vec<DropAt> {
    let n = body.blocks.len();
    // The MIR routinely splits `let buf = U8Vec::new(...)` into a
    // temporary destination plus a `Local(user) = Use(Copy(temp))`
    // assignment. Both locals alias the same heap memory; freeing
    // one invalidates the other. The cleanup pass therefore tracks
    // every Copy-alias of the original allocator destination and
    // computes liveness on the union - a "use" of any chain member
    // counts as a use of the underlying allocation.
    let chain = compute_alias_chain(body, local);
    // Split stmt-uses from terminator-uses so we know whether the
    // terminator reads the local. Drops emitted at "block exit"
    // (just before the terminator) are unsafe when the terminator
    // itself reads the value - those cases must drop on the
    // successor's entry instead.
    let mut stmt_uses = vec![false; n];
    let mut term_uses = vec![false; n];
    let mut defs = vec![false; n];
    for (i, block) in body.blocks.iter().enumerate() {
        for stmt in &block.stmts {
            for &cl in &chain {
                let cl_local = Local(cl);
                if stmt_reads_local(stmt, cl_local) {
                    stmt_uses[i] = true;
                }
                if stmt_writes_local(stmt, cl_local) {
                    defs[i] = true;
                }
            }
        }
        for &cl in &chain {
            let cl_local = Local(cl);
            if terminator_reads_local(&block.terminator, cl_local) {
                term_uses[i] = true;
            }
            if terminator_writes_local(&block.terminator, cl_local) {
                defs[i] = true;
            }
        }
    }

    let mut live_in = vec![false; n];
    let mut live_out = vec![false; n];
    let mut changed = true;
    while changed {
        changed = false;
        for (i, block) in body.blocks.iter().enumerate() {
            let new_live_out = successors(&block.terminator)
                .into_iter()
                .any(|s| live_in[s.as_u32() as usize]);
            let total_uses = stmt_uses[i] || term_uses[i];
            let new_live_in = total_uses || (new_live_out && !defs[i]);
            if new_live_out != live_out[i] || new_live_in != live_in[i] {
                changed = true;
                live_out[i] = new_live_out;
                live_in[i] = new_live_in;
            }
        }
    }

    let mut sites: Vec<DropAt> = Vec::new();
    for (i, block) in body.blocks.iter().enumerate() {
        // Block-exit drop: emit just before the terminator. Safe
        // only when the local was live during stmts (or live-in)
        // AND the terminator does NOT read it AND no successor
        // sees it live. Excludes Return blocks; the legacy
        // at-Return path covers those.
        let alive_during_stmts = stmt_uses[i] || live_in[i];
        let dead_after_stmts = !term_uses[i] && !live_out[i];
        if alive_during_stmts && dead_after_stmts && !matches!(block.terminator, Terminator::Return)
        {
            sites.push(DropAt::BlockExit(block.id));
        }
    }
    for (i, _block) in body.blocks.iter().enumerate() {
        // Block-entry drop: emit at the start of S when the local
        // is dead at S's entry but every predecessor had it live
        // at end-of-block (either through the terminator or a
        // live-out of the successor it picked). This covers two
        // cases the BlockExit rule cannot:
        //   * terminator reads the local (e.g. Call args) on the
        //     way to S, and S itself does not read it;
        //   * loop exit, where the loop header's terminator does
        //     not read the local but the back-edge target does -
        //     dropping at the loop-exit's entry catches the
        //     fall-through transition.
        if stmt_uses[i] || term_uses[i] || live_in[i] {
            continue;
        }
        let preds = &predecessors[i];
        if preds.is_empty() {
            continue;
        }
        let all_preds_alive_at_end = preds.iter().all(|p| {
            let pi = p.as_u32() as usize;
            term_uses[pi] || live_out[pi]
        });
        if !all_preds_alive_at_end {
            continue;
        }
        sites.push(DropAt::BlockEntry(body.blocks[i].id));
    }

    sites
}

/// Iterates Copy-assignments to a fixed point starting from
/// `alloc_local`, returning the set of locals that alias the same
/// heap memory. A local `M` is in the chain when there is a chain
/// of `M = Use(Copy(L))` assignments rooting at `alloc_local`.
fn compute_alias_chain(body: &Body, alloc_local: Local) -> BTreeSet<u32> {
    let mut chain: BTreeSet<u32> = BTreeSet::new();
    chain.insert(alloc_local.0);
    let mut changed = true;
    while changed {
        changed = false;
        for block in &body.blocks {
            for stmt in &block.stmts {
                if let StatementKind::Assign { place, rvalue } = &stmt.kind
                    && place.projection.is_empty()
                    && let Rvalue::Use(Operand::Copy(src_place)) = rvalue
                    && src_place.projection.is_empty()
                    && chain.contains(&src_place.local.0)
                    && chain.insert(place.local.0)
                {
                    changed = true;
                }
            }
        }
    }
    chain
}

fn compute_predecessors(body: &Body) -> Vec<Vec<BlockId>> {
    let n = body.blocks.len();
    let mut preds: Vec<Vec<BlockId>> = vec![Vec::new(); n];
    for block in &body.blocks {
        for s in successors(&block.terminator) {
            let idx = s.as_u32() as usize;
            if idx < n {
                preds[idx].push(block.id);
            }
        }
    }
    preds
}

fn stmt_reads_local(stmt: &crate::ir::Statement, local: Local) -> bool {
    match &stmt.kind {
        StatementKind::Assign { rvalue, .. } => rvalue_reads_local(rvalue, local),
        StatementKind::StaticStore { value, .. } => operand_reads_local(value, local),
        _ => false,
    }
}

fn stmt_writes_local(stmt: &crate::ir::Statement, local: Local) -> bool {
    if let StatementKind::Assign { place, .. } = &stmt.kind {
        return place.local == local && place.projection.is_empty();
    }
    false
}

fn rvalue_reads_local(rvalue: &Rvalue, local: Local) -> bool {
    match rvalue {
        Rvalue::Use(op) => operand_reads_local(op, local),
        Rvalue::Repeat { value, .. } => operand_reads_local(value, local),
        Rvalue::Aggregate { operands, .. } => {
            operands.iter().any(|op| operand_reads_local(op, local))
        }
        Rvalue::BinaryOp { lhs, rhs, .. } => {
            operand_reads_local(lhs, local) || operand_reads_local(rhs, local)
        }
        Rvalue::UnaryOp { operand, .. } => operand_reads_local(operand, local),
        Rvalue::Cast { operand, .. } => operand_reads_local(operand, local),
        Rvalue::Ref { place, .. } => place_reads_local(place, local),
        Rvalue::Len(place) => place_reads_local(place, local),
        Rvalue::CallIntrinsic { args, .. } => args.iter().any(|op| operand_reads_local(op, local)),
        // No local operands; the static is referenced by symbol.
        Rvalue::StaticLoad(_) => false,
    }
}

fn operand_reads_local(op: &Operand, local: Local) -> bool {
    match op {
        Operand::Copy(place) => place_reads_local(place, local),
        Operand::Const(_) => false,
        Operand::FnRef { .. } => false,
    }
}

fn place_reads_local(place: &Place, local: Local) -> bool {
    place.local == local
}

fn terminator_reads_local(t: &Terminator, local: Local) -> bool {
    match t {
        Terminator::SwitchInt { discriminant, .. } => operand_reads_local(discriminant, local),
        Terminator::Call { args, .. } => args.iter().any(|a| operand_reads_local(a, local)),
        Terminator::Assert { cond, msg, .. } => {
            operand_reads_local(cond, local)
                || msg.operands().any(|op| operand_reads_local(op, local))
        }
        Terminator::Drop { place, .. } => place_reads_local(place, local),
        _ => false,
    }
}

fn terminator_writes_local(t: &Terminator, local: Local) -> bool {
    if let Terminator::Call { destination, .. } = t {
        return destination.local == local && destination.projection.is_empty();
    }
    false
}

/// Tests whether `gate` is on every path from the body's entry to
/// each block in `targets`.
fn dominates_all(body: &Body, gate: BlockId, targets: &[BlockId]) -> bool {
    let entry = match body.blocks.first() {
        Some(b) => b.id,
        None => return false,
    };
    if entry == gate {
        return true;
    }
    let mut visited: BTreeSet<u32> = BTreeSet::new();
    let mut queue: VecDeque<BlockId> = VecDeque::new();
    queue.push_back(entry);
    visited.insert(entry.as_u32());
    while let Some(b) = queue.pop_front() {
        if b == gate {
            continue;
        }
        let block = &body.blocks[b.as_u32() as usize];
        for succ in successors(&block.terminator) {
            if succ == gate {
                continue;
            }
            if visited.insert(succ.as_u32()) {
                queue.push_back(succ);
            }
        }
    }
    !targets.iter().any(|t| visited.contains(&t.as_u32()))
}

fn successors(t: &Terminator) -> Vec<BlockId> {
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

#[cfg(test)]
mod tests {
    use gossamer_hir::lower_source_file;
    use gossamer_lex::SourceMap;
    use gossamer_parse::parse_source_file;
    use gossamer_resolve::resolve_source_file;
    use gossamer_types::{TyCtxt, typecheck_source_file};

    use super::*;
    use crate::lower_program;

    fn build(source: &str) -> Vec<Body> {
        let mut map = SourceMap::new();
        let file = map.add_file("t.gos", source.to_string());
        let (mut sf, _) = parse_source_file(source, file);
        let (res, _) = resolve_source_file(&sf);
        let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &res);
        let mut tcx = TyCtxt::new();
        let (tbl, _) = typecheck_source_file(&sf, &res, &mut tcx);
        let hir = lower_source_file(&sf, &res, &tbl, &mut tcx);
        lower_program(&hir, &mut tcx)
    }

    #[test]
    fn empty_plan_for_function_with_no_heap_allocations() {
        let bodies = build("fn f() -> i64 { 1i64 + 2i64 }\n");
        let plan = plan(&bodies[0]);
        assert!(plan.is_empty());
    }

    #[test]
    fn a_joined_spawn_handle_is_dropped_at_its_last_use() {
        let bodies = build(
            "fn work() -> i64 { 7 }\nfn f() -> i64 {\n    let h = spawn(work)\n    h.join().unwrap_or(0)\n}\n",
        );
        let body = bodies
            .iter()
            .find(|b| b.name.ends_with('f'))
            .expect("body f");
        let plan = plan(body);
        assert!(
            plan.entries()
                .iter()
                .any(|e| e.free_fn == "gos_rt_chan_drop"),
            "spawn handle has no drop: {:?}",
            plan.entries()
        );
    }
}
