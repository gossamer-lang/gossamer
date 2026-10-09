//! High-level IR for the Gossamer compiler.
//! The HIR is a desugared, explicit version of the parsed and type-
//! checked AST. Control-flow sugar - `for` loops, the `?` operator,
//! and the forward-pipe `|>` - is lowered into primitive forms so that
//! later passes (bytecode, MIR) see one spelling per concept.
//! Each HIR node carries a stable [`HirId`] and a [`gossamer_types::Ty`]
//! annotation. Types come from the [`gossamer_types::TypeTable`];
//! nodes that the checker did not touch receive the interner's error
//! sentinel so that later passes can still walk the tree.

#![forbid(unsafe_code)]

mod capture_cells;
mod disjoint_windows;
mod f32_round;
mod foreign;
mod foreign_boundary;
mod fuse;
mod ids;
mod lift;
mod lower;
mod par;
mod place_refs;
mod tree;

pub use foreign::{FOREIGN_DISPATCHER, foreign_layouts, foreign_signature, route_foreign_calls};
pub use foreign_boundary::{
    ATOMIC_CAS as FFI_ATOMIC_CAS, ATOMIC_RMW as FFI_ATOMIC_RMW, CALLBACK as FFI_CALLBACK,
    EXPORT as FFI_EXPORT, EXPORTS_FN as FFI_EXPORTS_FN, HANDLE_CELL as FFI_HANDLE_CELL,
    HANDLE_PIN as FFI_HANDLE_PIN, HANDLE_RELEASE as FFI_HANDLE_RELEASE,
    HANDLE_STORE as FFI_HANDLE_STORE, INDIRECT_SYMBOL as FFI_INDIRECT_SYMBOL,
    LOAD_FLOAT as FFI_LOAD_FLOAT, LOAD_INT as FFI_LOAD_INT, NULL_RESULT as FFI_NULL_RESULT,
    STORE_FLOAT as FFI_STORE_FLOAT, STORE_INT as FFI_STORE_INT, SYMBOL as FFI_SYMBOL,
    VIEW_CHECK as FFI_VIEW_CHECK, VIEW_LEN_CHECK as FFI_VIEW_LEN_CHECK,
    VIEW_RANGE_CHECK as FFI_VIEW_RANGE_CHECK,
};
pub use fuse::fuse_iter_pipelines;
pub use gossamer_ast::STATIC_INIT_FN;
pub use ids::{HirId, HirIdGenerator};
pub use lift::{
    LIFTED_CLOSURE_PREFIX, collect_free_vars, collect_pattern_names, is_capture_env_load,
    lift_closures, shadowed_global_names,
};
pub use lower::{is_map_literal_block, lower_source_file};
pub use par::PAR_RUN;
pub use tree::FnOrigin;
pub use tree::{
    HirAdt, HirAdtKind, HirArrayExpr, HirBinaryOp, HirBlock, HirBody, HirConst, HirExpr,
    HirExprKind, HirFieldPat, HirFn, HirImpl, HirItem, HirItemKind, HirLiteral, HirMatchArm,
    HirParam, HirPat, HirPatKind, HirProgram, HirSelectArm, HirSelectOp, HirStatic, HirStmt,
    HirStmtKind, HirTrait, HirUnaryOp,
};
pub use tree::{for_each_child_expr, for_each_child_expr_in_block};
