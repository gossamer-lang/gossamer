//! Abstract syntax tree types for the Gossamer language.
//! This crate models every production in SPEC §15 as owned `Debug + Clone`
//! Rust types. Every AST node (expression, pattern, type, item, statement)
//! carries a stable [`node_id::NodeId`] and a `gossamer_lex::Span`, but
//! [`PartialEq`] implementations ignore both so structural comparisons work
//! across parser runs. A pretty-printer module (`printer`) renders any node
//! back into idiomatic Gossamer source.

#![forbid(unsafe_code)]

pub mod assoc;
pub mod common;
pub mod expr;
pub mod items;
pub mod node_id;
pub mod path;
pub mod pattern;
pub mod printer;
pub mod source_file;
pub mod stmt;
pub mod ty;
pub mod visitor;

pub use assoc::{AssocIndex, AssocResolution, MissingAssocItem};
pub use common::{
    AssignOp, BinaryOp, ERROR_IDENT, Ident, Mutability, RangeKind, UnaryOp, Visibility,
    is_error_name,
};
pub use expr::{
    ArrayExpr, Block, BlockKind, ClosureParam, Expr, ExprKind, FieldSelector, Label, Literal,
    MatchArm, PathExpr, PathSegment, SelectArm, SelectOp, StructExprField,
};
pub use gossamer_abi::format_pad::{
    PAD_ALIGN_CENTER, PAD_ALIGN_LEFT, PAD_ALIGN_RIGHT, PAD_ALIGN_SIGN_AWARE_ZERO,
    PAD_REQUEST_ALIGN_MASK, PAD_REQUEST_CENTER, PAD_REQUEST_DEFAULT, PAD_REQUEST_LEFT,
    PAD_REQUEST_RIGHT, PAD_REQUEST_ZERO_FLAG, resolve_pad_request, sign_aware_prefix_len,
};
pub use items::{
    AssocBinding, Attribute, Attrs, BINARY_SEARCH_PREFIX, ConstDecl, EnumDecl, EnumRepr,
    EnumVariant, FnDecl, FnParam, GenericParam, Generics, ImplDecl, ImplItem, Item, ItemKind,
    ModBody, ModDecl, OPERATOR_METHOD_PREFIX, PARTITION_POINT_PREFIX, Receiver,
    STRUCTURAL_COMPARATOR_PREFIX, StaticDecl, StructBody, StructDecl, StructField, TraitBound,
    TraitDecl, TraitItem, TupleField, TypeAliasDecl, USER_COMPARATOR_PREFIX, WhereClause,
    WherePredicate,
};
pub use node_id::{NodeId, NodeIdGenerator};
pub use path::{Path, Segment};
pub use pattern::{FieldPattern, Pattern, PatternKind};
pub use printer::Printer;
pub use source_file::{ModulePath, NamedArg, SourceFile, UseDecl, UseListEntry, UseTarget};
pub use stmt::{Stmt, StmtKind};
pub use ty::{FnTypeKind, GenericArg, Type, TypeKind, TypePath, TypePathSegment};
pub use visitor::{Visitor, VisitorMut};
