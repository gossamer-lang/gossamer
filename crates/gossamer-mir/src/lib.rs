//! Mid-level SSA-lite IR.
//! The MIR sits between HIR and native-code generation. Its CFG-
//! oriented shape matches what Cranelift and LLVM want to consume, so
//! can translate MIR directly without a second
//! lowering pass. Each function becomes a [`Body`] holding typed
//! locals, basic blocks, and a terminator per block.

#![forbid(unsafe_code)]

mod cleanup;
mod dce;
mod escape;
mod ir;
mod lower;
mod monomorph;
mod opt;
mod ownership;
pub mod uniqueness;
pub mod verify;

pub use cleanup::{
    CleanupEntry, CleanupPlan, DropAt, HEAP_ALLOCATOR_PAIRS, plan as plan_cleanup,
    plan_with_summary as plan_cleanup_with_summary,
};
pub use dce::{PruneReport, Scope, prune_scoped, prune_unreachable};
pub use escape::{
    CaptureSummary, EscapeSet, analyse as analyse_escape,
    analyse_with_summary as analyse_escape_with_summary, build_capture_summary,
};
pub use ir::{
    AggregateKind, AssertMessage, BasicBlock, BinOp, BlockId, Body, ConstValue, F64MathIntrinsic,
    InlineChain, InlineFrame, IteratorAdapterKind, IteratorOwnership, IteratorSourceKind, Local,
    LocalDecl, Operand, Place, Projection, RawIntrinsic, RawIntrinsicArity, Rvalue, Statement,
    StatementKind, StaticRef, Terminator, UnOp, local_is_uint_cast,
};
pub use lower::helpers::escape::{ParamShare, collect_region_unsafe_fns, collect_shareable_params};
pub use lower::{lower_program, mangle_callable_shape};
pub use monomorph::{
    bind_template_params, check_generic_layouts, mangled_name, method_mangled_name, monomorphise,
    subst_param_ty,
};
pub use opt::{
    UniquenessReport, carrier_payload_views, const_branch_elim, const_fold, const_value_of,
    copy_propagate, dead_block_sweep, dead_store_elim, enable_uniqueness_report, inline_general,
    inline_small_callees, inline_trivial_wrappers, optimise, optimise_debug, optimise_for_jit,
    statement_count,
};
