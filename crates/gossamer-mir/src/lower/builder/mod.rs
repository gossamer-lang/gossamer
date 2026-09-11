#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::wildcard_imports)]
#![allow(clippy::similar_names)]
#![allow(clippy::many_single_char_names)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::option_if_let_else)]
#![allow(clippy::match_same_arms)]
#![allow(clippy::if_not_else)]
#![allow(clippy::single_match_else)]
#![allow(clippy::needless_pass_by_value)]
#![allow(clippy::manual_let_else)]
#![allow(clippy::redundant_else)]
#![allow(clippy::collapsible_if)]
#![allow(clippy::collapsible_else_if)]
#![allow(clippy::map_unwrap_or)]
#![allow(clippy::struct_excessive_bools)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::unnecessary_wraps)]
#![allow(clippy::large_enum_variant)]
#![allow(clippy::if_same_then_else)]
#![allow(clippy::single_match)]
#![allow(clippy::useless_conversion)]

use std::collections::HashMap;

use gossamer_ast::Ident;
use gossamer_hir::{
    HirAdtKind, HirBinaryOp, HirBlock, HirExpr, HirExprKind, HirFn, HirItem, HirItemKind,
    HirLiteral, HirMatchArm, HirPat, HirPatKind, HirProgram, HirStmt, HirStmtKind, HirUnaryOp,
};
use gossamer_lex::Span;
use gossamer_types::{Ty, TyCtxt};

use crate::ir::{
    BasicBlock, BinOp, BlockId, Body, ConstValue, Local, LocalDecl, Operand, Place, Rvalue,
    Statement, StatementKind, Terminator, UnOp,
};

use super::*;

pub(crate) struct Builder<'a> {
    pub(crate) tcx: &'a mut TyCtxt,
    pub(crate) locals: Vec<LocalDecl>,
    pub(crate) blocks: Vec<BasicBlock>,
    pub(crate) current: Option<BlockId>,
    pub(crate) scopes: Vec<HashMap<String, Local>>,
    /// Locals a `let` has published under a name. Such a local is the
    /// binding's own storage for the rest of its scope, so no later
    /// rebinding may re-point the call that defines it.
    pub(crate) named_locals: std::collections::HashSet<Local>,
    /// Names bound to a direct local reference. These names resolve to the
    /// source local, while deref lowering uses the marker to avoid adding a
    /// second physical dereference to that source place.
    pub(crate) reference_aliases: Vec<HashMap<String, Local>>,
    pub(crate) fn_span: Span,
    pub(crate) structs: &'a HashMap<String, Vec<String>>,
    pub(crate) struct_defs: &'a HashMap<gossamer_resolve::DefId, String>,
    pub(crate) enums: &'a EnumIndex,
    pub(crate) impl_methods: &'a HashMap<String, Option<Ty>>,
    /// Declared receiver type for each mangled user impl method.
    pub(crate) impl_method_receivers: &'a HashMap<String, Ty>,
    /// Declared receiver and argument types for each mangled impl method.
    pub(crate) impl_method_inputs: &'a HashMap<String, Vec<Ty>>,
    /// Declared return types by callable name (free fns bare,
    /// impl methods mangled). See `collect_fn_ret_names`.
    pub(crate) fn_ret_names: &'a HashMap<String, Ty>,
    pub(crate) fn_returns: &'a HashMap<gossamer_resolve::DefId, Ty>,
    pub(crate) fn_inputs: &'a HashMap<gossamer_resolve::DefId, Vec<Ty>>,
    /// Per-parameter read-only summary by callee; see `collect_shareable_params`.
    pub(crate) fn_param_shareable: &'a HashMap<gossamer_resolve::DefId, Vec<bool>>,
    pub(crate) consts: &'a HashMap<gossamer_resolve::DefId, ConstValue>,
    /// Scalar `static mut` items promoted to real mutable module globals,
    /// keyed by `DefId`. A path reading one lowers to a [`Rvalue::StaticLoad`]
    /// and an assignment writing one to a [`StatementKind::StaticStore`].
    pub(crate) mut_statics: &'a HashMap<gossamer_resolve::DefId, crate::ir::StaticRef>,
    /// Declaration initializer for every top-level `const` / non-`mut`
    /// `static` item, keyed by `DefId`. A path resolving to one of these
    /// with no `consts` entry (a heap-typed initializer, which
    /// [`ConstValue`] cannot represent) re-lowers this expression at the
    /// reference site instead of falling through to the function-value path.
    pub(crate) const_inits: &'a HashMap<gossamer_resolve::DefId, HirExpr>,
    /// Free functions that may let a value escape (spawn / channel / static
    /// write / param-stash). A loop calling any of these is never
    /// auto-regioned. See `collect_region_unsafe_fns`.
    pub(crate) region_unsafe: &'a std::collections::HashSet<gossamer_resolve::DefId>,
    pub(crate) local_struct: HashMap<Local, String>,
    /// Scalar receivers borrowed mutably for a `&mut self` method, keyed by
    /// the reference local and valued by the place it borrowed. A scalar has
    /// no machine address of its own on the JIT tier, so the borrow points at
    /// a materialised slot; reloading the place from that slot after the call
    /// is what carries the callee's write back.
    pub(crate) mut_receiver_reloads: HashMap<Local, Local>,
    /// Locals bound to the *address* of a vec slot by a `&mut` for-loop.
    /// The slot of a heap-container element holds a pointer, so a method
    /// call on such a binding loads that pointer before dispatching, while
    /// an assignment through the binding still stores into the slot.
    pub(crate) slot_ref_locals: std::collections::HashSet<Local>,
    /// For locals that hold an array/tuple whose element type is a
    /// known struct, records that struct's name. Used to resolve
    /// field projections through `a[i].x` when the type checker left
    /// the element type as an unresolved inference variable.
    pub(crate) local_elem_struct: HashMap<Local, String>,
    pub(crate) local_closure: HashMap<Local, String>,
    /// Locals that hold a function-name constant (e.g. a synthesised
    /// closure body like `__closure_0` bound through a let). Tracked
    /// so that calling the local dispatches to the named function by
    /// direct call rather than treating the local as a closure env
    /// pointer.
    pub(crate) local_fn_name: HashMap<Local, String>,
    /// Runtime-shape tag for locals whose static MIR type doesn't
    /// distinguish the stdlib type behind them (everything ends
    /// up as `i64` / pointer once erased). Method dispatch reads
    /// this tag to pick the right runtime helper for `fs.string(...)`,
    /// `client.get(...)`, `req.send()`, etc.
    pub(crate) local_runtime_kind: HashMap<Local, &'static str>,
    /// Erased heap handles that must dispatch to min-heap runtime helpers
    /// even though the handle ABI is still a `GosVec<i64>`.
    pub(crate) local_binary_heap_min_i64: std::collections::HashSet<Local>,
    /// Lazy iterator-state locals whose slots hold the address of each
    /// element rather than its value. The state's type names the element, and
    /// an element wider than one slot can be either this or the dedicated pair
    /// state, so which one a local holds is recorded where it is built.
    pub(crate) local_aggr_iter: std::collections::HashSet<Local>,
    /// Per-local field layout for synthesised aggregates produced by
    /// the declarative `flag::define(...)` lowering. Maps the result
    /// local to a `Vec<(long_name, cell_kind)>` indexed by field
    /// position. Field access `flags.<long>` consults this table to
    /// emit the right `Field(idx)` projection plus the corresponding
    /// `flag::Cell::*` runtime kind tag.
    pub(crate) local_define_layout: HashMap<Local, Vec<(String, &'static str)>>,
    pub(crate) param_locals: std::collections::HashSet<Local>,
    /// Loop contexts visible at the current lowering point. The
    /// innermost loop is at the back. Each entry pairs the
    /// `continue`-target (the loop header) with the `break`-target
    /// (the block emitted right after the loop). `lower_loop` /
    /// `lower_while` push on entry and pop on exit;
    /// `HirExprKind::Break` / `Continue` lookup the back of the
    /// stack to terminate to the right block.
    pub(crate) loop_stack: Vec<LoopContext>,
    /// The label of the loop currently being lowered, set by the
    /// `Loop` / `While` lowering site just before it descends into the
    /// loop builder. Each loop builder takes it at entry (before
    /// lowering the iterand / body) and records it on the `LoopContext`
    /// it pushes, so labelled `break`/`continue` can target the right
    /// loop. `None` for an unlabelled loop.
    pub(crate) pending_loop_label: Option<String>,
    /// When set, `gos_rt_result_payload` + field/tuple-binding emissions
    /// for a Result/Option match arm are deferred into this block instead
    /// of the pre-branch header. Prevents unconditional null-deref when
    /// the scrutinee is None/Err on a subsequent loop iteration.
    pub(crate) payload_defer_block: Option<BlockId>,
    /// Nesting depth of `runtime::arena_push` .. `arena_pop` while
    /// lowering. Locals created at depth > 0 are arena-region-owned: the
    /// drop pass must not release them (the region frees them wholesale at
    /// pop; a post-pop release would be a use-after-free).
    pub(crate) region_depth: u32,
    /// One flag per compiler-inserted loop region. An explicit
    /// `runtime::collect_cycles()` inside such a loop is delayed until after
    /// its matching `arena_pop`, so the collector never inspects pointers
    /// owned by the region being torn down.
    pub(crate) deferred_auto_region_collections: Vec<bool>,
    /// One frame per lexical block currently being lowered; each frame holds
    /// that block's `defer`red expressions in registration order. Block-scoped
    /// `defer` (Swift/Zig semantics) emits a frame's expressions in LIFO order
    /// at every edge that leaves the block - normal fall-through, `return`
    /// (all frames), and `break`/`continue` (frames down to the loop's frame).
    pub(crate) defer_stack: Vec<Vec<gossamer_hir::HirExpr>>,
}

/// A live loop context: where to jump on `break` vs. `continue`,
/// plus the optional result local that `break <expr>` writes into
/// before jumping. `None` for loops whose result is unused.
#[derive(Debug, Clone)]
pub(crate) struct LoopContext {
    pub(crate) continue_to: BlockId,
    pub(crate) break_to: BlockId,
    pub(crate) result: Option<Local>,
    /// Loop label (without the leading apostrophe), or `None` for an
    /// unlabelled loop. Labelled `break`/`continue` scan the stack
    /// from the innermost outward for a matching label.
    pub(crate) label: Option<String>,
    /// Set to `true` when a `break` targeting this loop is lowered.
    /// Used so `lower_loop` can return `None` for purely divergent
    /// loops (no `break` at all), preventing a spurious `RETURN`
    /// assign at function-tail positions.
    pub(crate) break_used: bool,
    /// `defer_stack` length at loop entry. `break`/`continue` run the defers
    /// in frames at indices `>= defer_depth` (the blocks inside the loop body)
    /// before jumping, but not the loop's enclosing frames.
    pub(crate) defer_depth: usize,
}

mod ctrl;
mod expr;
mod intrinsic;
mod method_call;
pub(crate) use method_call::{ElemAbi, LazyElemFamily};
mod misc;
mod scope;
mod stdlib;
mod stmt;
mod types;

mod expr_call;

mod expr_field;

mod expr_array;

mod stdlib_json;

mod stdlib_json_stream;

mod stdlib_free;

mod stdlib_binding;

mod method_call_dispatch;
