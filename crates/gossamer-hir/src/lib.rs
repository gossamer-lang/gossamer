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

mod fuse;
mod ids;
mod lift;
mod lower;
mod place_refs;
mod tree;

pub use fuse::fuse_iter_pipelines;
pub use ids::{HirId, HirIdGenerator};
pub use lift::{
    LIFTED_CLOSURE_PREFIX, collect_free_vars, collect_pattern_names, is_capture_env_load,
    lift_closures, shadowed_global_names,
};
pub use lower::lower_source_file;
pub use tree::FnOrigin;
pub use tree::{
    HirAdt, HirAdtKind, HirArrayExpr, HirBinaryOp, HirBlock, HirBody, HirConst, HirExpr,
    HirExprKind, HirFieldPat, HirFn, HirImpl, HirItem, HirItemKind, HirLiteral, HirMatchArm,
    HirParam, HirPat, HirPatKind, HirProgram, HirSelectArm, HirSelectOp, HirStatic, HirStmt,
    HirStmtKind, HirTrait, HirUnaryOp,
};
pub use tree::{for_each_child_expr, for_each_child_expr_in_block};
