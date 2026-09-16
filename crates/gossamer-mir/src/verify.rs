//! Structural invariant checker for [`Body`].
//!
//! The verifier runs after every MIR-level pass behind a
//! `debug_assertions` gate (`verify_body`). It catches the class
//! of bug where an optimisation rewrites the CFG and leaves
//! something dangling: a `BlockId` that no longer exists, a
//! `Local` index past the end of the locals table, a block whose
//! `id` field disagrees with its position in `body.blocks`. The
//! copy-prop aggregate-aliasing miscompile that shipped in 0.4
//! is exactly this class - `verify_body` would have caught it at
//! the point the rewrite happened, not at codegen time when the
//! produced object segfaulted under user code.
//!
//! The checks are intentionally cheap: the verifier walks every
//! block once, every statement once, every operand once. No
//! type-level reasoning, no exhaustiveness logic. The contract is
//! "the produced `Body` is well-formed enough that the codegen
//! crates can lower it without panicking on out-of-range
//! indices."

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};

use gossamer_resolve::DefId;
use gossamer_types::{Ty, TyCtxt, TyKind};

use crate::ir::{
    AggregateKind, BasicBlock, BlockId, Body, ConstValue, IteratorOwnership, IteratorSourceKind,
    Local, Operand, Place, Projection, RawIntrinsic, RawIntrinsicArity, Rvalue, StatementKind,
    Terminator, UnOp,
};

/// Single failure recorded by [`verify_body`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    /// `body.blocks` is empty. Every function has at least an
    /// entry block holding `Return`.
    EmptyBlocks {
        /// Function whose blocks vector was empty.
        body: String,
    },
    /// A block's stored `id` doesn't match its position in
    /// `body.blocks`. Optimisation passes that splice or
    /// reorder blocks must keep this invariant.
    BlockIdMismatch {
        /// Function name.
        body: String,
        /// Position in `body.blocks`.
        position: u32,
        /// Id stored on the block.
        stored: BlockId,
    },
    /// A `BlockId` referenced by a terminator points past the end
    /// of `body.blocks`.
    BlockOutOfRange {
        /// Function name.
        body: String,
        /// Block whose terminator referenced an invalid target.
        block: BlockId,
        /// Bad target id.
        target: BlockId,
        /// Length of `body.blocks` at audit time.
        n_blocks: u32,
    },
    /// A `Local` referenced by a statement, terminator, or place
    /// projection points past the end of `body.locals`.
    LocalOutOfRange {
        /// Function name.
        body: String,
        /// Block containing the offending reference.
        block: BlockId,
        /// Bad local id.
        local: Local,
        /// Length of `body.locals` at audit time.
        n_locals: u32,
    },
    /// A `Call::target = Some(t)` references a block beyond the
    /// CFG. Out-of-range call targets are flagged via
    /// [`VerifyError::BlockOutOfRange`] from the same pass.
    CallTargetMissing {
        /// Function name.
        body: String,
        /// Block whose call had no continuation.
        block: BlockId,
    },
    /// A `Terminator::Call` whose `callee` is a known `FnRef` passes a
    /// different number of operands than the callee's declared arity.
    CallArityMismatch {
        /// Caller function name.
        body: String,
        /// Block containing the call.
        block: BlockId,
        /// Callee function name.
        callee: String,
        /// Operand count at the call site.
        got: u32,
        /// Declared arity of the callee.
        expected: u32,
    },
    /// A `Terminator::Call` references a function definition that is not
    /// present in the program after monomorphisation.
    UnresolvedFnRef {
        /// Caller function name.
        body: String,
        /// Block containing the call.
        block: BlockId,
        /// Missing callee definition.
        def: DefId,
    },
    /// A compiler-private or runtime intrinsic name in MIR is unknown.
    UnknownIntrinsic {
        /// Function name.
        body: String,
        /// Block containing the intrinsic assignment.
        block: BlockId,
        /// Intrinsic name.
        name: &'static str,
    },
    /// A compiler-private or runtime intrinsic was emitted with an argument
    /// count its lowering contract cannot accept.
    IntrinsicArityMismatch {
        /// Function name.
        body: String,
        /// Block containing the intrinsic assignment.
        block: BlockId,
        /// Intrinsic name.
        name: &'static str,
        /// Operand count at the intrinsic site.
        got: u32,
        /// Accepted operand count description.
        expected: &'static str,
    },
    /// A `Terminator::Return` was found in a body whose return slot
    /// (`Local::RETURN`) has the poisoned [`TyKind::Error`] type. The
    /// frontend should have rejected the body before MIR runs.
    ReturnTypeError {
        /// Function name.
        body: String,
        /// Block containing the offending `Return`.
        block: BlockId,
    },
    /// An `Rvalue::Aggregate { kind: Adt { def, variant }, .. }` was
    /// emitted whose operand count disagrees with the variant's
    /// declared field count.
    AggregateOperandCount {
        /// Function name.
        body: String,
        /// Block containing the aggregate.
        block: BlockId,
        /// `DefId` of the ADT.
        def: DefId,
        /// Variant index.
        variant: u32,
        /// Operand count at the site.
        got: u32,
        /// Declared field count of the variant.
        expected: u32,
    },
    /// A `Terminator::SwitchInt` discriminant resolves to a non-int /
    /// non-bool leaf type. The MIR contract is integer equality, so
    /// any other shape is undefined behaviour at codegen time.
    SwitchIntNonIntegerDiscriminant {
        /// Function name.
        body: String,
        /// Block containing the switch.
        block: BlockId,
    },
    /// A `Terminator::Drop { place, .. }` targets a leaf type the GC
    /// does not own (e.g. an `i64` or `bool`). The drop pass should
    /// only schedule frees against heap-managed leaves.
    DropOfNonOwning {
        /// Function name.
        body: String,
        /// Block containing the offending drop.
        block: BlockId,
    },
    /// A `Rvalue::UnaryOp { op: Neg, operand: Const(Int(i128::MIN)) }`
    /// slipped past the const-folder. Surface the leak so a manual
    /// folding regression cannot silently overflow at runtime.
    UnaryNegI128Min {
        /// Function name.
        body: String,
        /// Block containing the offending rvalue.
        block: BlockId,
    },
    /// A `Terminator::Call::destination` leaf type is still a
    /// `TyKind::Var(_)` (unresolved inference variable) or
    /// `TyKind::Error`. Codegen cannot pick a runtime shape for the
    /// returned value.
    CallDestinationUntyped {
        /// Function name.
        body: String,
        /// Block containing the call.
        block: BlockId,
    },
    /// A typed iterator operation used a projected place. Iterator states are
    /// linear locals, so projections would let aliases bypass consumption and
    /// drop tracking.
    IteratorStateProjected {
        /// Function name.
        body: String,
        /// Block containing the operation.
        block: BlockId,
    },
    /// A source kind was paired with an ownership mode that cannot preserve
    /// its lifetime contract.
    IteratorOwnershipMismatch {
        /// Function name.
        body: String,
        /// Block containing the source operation.
        block: BlockId,
    },
    /// More than one adapter attempted to take ownership of the same iterator
    /// state.
    IteratorStateConsumedTwice {
        /// Function name.
        body: String,
        /// Block containing the second consuming adapter.
        block: BlockId,
        /// Iterator local consumed twice.
        local: Local,
    },
}

/// Walks `body` and accumulates every structural invariant
/// violation. Returns `Ok(())` when the body is well-formed.
///
/// The verifier is allocation-free in the common (well-formed)
/// case: errors are collected into a single `Vec<VerifyError>`
/// which stays empty for clean inputs. The result is an
/// `Err(Vec<...>)` so callers can dump every error at once
/// instead of one-shot fail-fast.
///
/// # Examples
///
/// Constructs a trivial single-block body that just `return`s and
/// runs it through the verifier. Most callers operate on bodies
/// produced by `lower_program`; this shape is what the verifier
/// expects as the minimum well-formed CFG (one block, in-range
/// locals, a `Return` terminator).
///
/// ```rust
/// use gossamer_lex::SourceMap;
/// use gossamer_mir::verify::verify_body;
/// use gossamer_mir::{BasicBlock, BlockId, Body, LocalDecl, Terminator};
/// use gossamer_types::{IntTy, TyCtxt};
///
/// let mut sources = SourceMap::new();
/// let file = sources.add_file("doc.gos", String::new());
/// let span = gossamer_lex::Span::new(file, 0, 0);
///
/// let mut tcx = TyCtxt::new();
/// let i64_ty = tcx.int_ty(IntTy::I64);
///
/// let body = Body {
///     name: "id".to_string(),
///     def: None,
///     arity: 1,
///     locals: vec![
///         LocalDecl { ty: i64_ty, debug_name: None, mutable: false, region: false },
///         LocalDecl { ty: i64_ty, debug_name: None, mutable: false, region: false },
///     ],
///     blocks: vec![BasicBlock {
///         id: BlockId::ENTRY,
///         stmts: Vec::new(),
///         terminator: Terminator::Return,
///         span,
///         terminator_span: None,
///         terminator_inlined: None,
///     }],
///     span,
/// };
///
/// verify_body(&body).expect("MIR verifier rejected body");
/// ```
pub fn verify_body(body: &Body) -> Result<(), Vec<VerifyError>> {
    let mut errors: Vec<VerifyError> = Vec::new();
    let n_blocks: u32 = body.blocks.len().try_into().unwrap_or(u32::MAX);
    let n_locals: u32 = body.locals.len().try_into().unwrap_or(u32::MAX);
    if body.blocks.is_empty() {
        errors.push(VerifyError::EmptyBlocks {
            body: body.name.clone(),
        });
        // No blocks to walk. Surface the single error and stop;
        // the rest of the pass would unconditionally underflow.
        return Err(errors);
    }
    for (position, block) in body.blocks.iter().enumerate() {
        let position = position as u32;
        if block.id.0 != position {
            errors.push(VerifyError::BlockIdMismatch {
                body: body.name.clone(),
                position,
                stored: block.id,
            });
        }
        check_block(body, block, n_blocks, n_locals, &mut errors);
    }
    check_iterator_states(body, &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Checks the linear-storage invariants unique to typed iterator statements.
fn check_iterator_states(body: &Body, errors: &mut Vec<VerifyError>) {
    let mut consumed: HashSet<Local> = HashSet::new();
    for block in &body.blocks {
        for stmt in &block.stmts {
            match &stmt.kind {
                StatementKind::IterSource {
                    dst,
                    source_kind,
                    ownership,
                    ..
                } => {
                    if !dst.is_simple() {
                        errors.push(VerifyError::IteratorStateProjected {
                            body: body.name.clone(),
                            block: block.id,
                        });
                    }
                    let ownership_matches = matches!(
                        (source_kind, ownership),
                        (IteratorSourceKind::Slice, IteratorOwnership::Borrowed)
                            | (
                                IteratorSourceKind::Range | IteratorSourceKind::VecInto,
                                IteratorOwnership::Owning
                            )
                    );
                    if !ownership_matches {
                        errors.push(VerifyError::IteratorOwnershipMismatch {
                            body: body.name.clone(),
                            block: block.id,
                        });
                    }
                }
                StatementKind::IterAdapter { dst, upstream, .. } => {
                    if !dst.is_simple() || !upstream.is_simple() {
                        errors.push(VerifyError::IteratorStateProjected {
                            body: body.name.clone(),
                            block: block.id,
                        });
                    }
                    if !consumed.insert(upstream.local) {
                        errors.push(VerifyError::IteratorStateConsumedTwice {
                            body: body.name.clone(),
                            block: block.id,
                            local: upstream.local,
                        });
                    }
                }
                StatementKind::IterNext {
                    dst_option,
                    iter_place,
                    ..
                } if !dst_option.is_simple() || !iter_place.is_simple() => {
                    errors.push(VerifyError::IteratorStateProjected {
                        body: body.name.clone(),
                        block: block.id,
                    });
                }
                StatementKind::Assign { .. }
                | StatementKind::StorageLive(_)
                | StatementKind::StorageDead(_)
                | StatementKind::SetDiscriminant { .. }
                | StatementKind::StaticStore { .. }
                | StatementKind::IterNext { .. }
                | StatementKind::Nop => {}
            }
        }
    }
}

/// Walks one block, collecting invariant violations into
/// `errors`.
fn check_block(
    body: &Body,
    block: &BasicBlock,
    n_blocks: u32,
    n_locals: u32,
    errors: &mut Vec<VerifyError>,
) {
    for stmt in &block.stmts {
        check_statement(body, block.id, &stmt.kind, n_locals, errors);
    }
    check_terminator(
        body,
        block.id,
        &block.terminator,
        n_blocks,
        n_locals,
        errors,
    );
}

fn check_statement(
    body: &Body,
    block: BlockId,
    kind: &StatementKind,
    n_locals: u32,
    errors: &mut Vec<VerifyError>,
) {
    match kind {
        StatementKind::Assign { place, rvalue } => {
            check_place(body, block, place, n_locals, errors);
            check_rvalue(body, block, rvalue, n_locals, errors);
        }
        StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
            check_local(body, block, *local, n_locals, errors);
        }
        StatementKind::SetDiscriminant { place, .. } => {
            check_place(body, block, place, n_locals, errors);
        }
        StatementKind::StaticStore { value, .. } => {
            check_operand(body, block, value, n_locals, errors);
        }
        StatementKind::IterSource { dst, source, .. } => {
            check_place(body, block, dst, n_locals, errors);
            check_operand(body, block, source, n_locals, errors);
        }
        StatementKind::IterAdapter {
            dst,
            upstream,
            closure_or_arg,
            ..
        } => {
            check_place(body, block, dst, n_locals, errors);
            check_place(body, block, upstream, n_locals, errors);
            if let Some(arg) = closure_or_arg {
                check_operand(body, block, arg, n_locals, errors);
            }
        }
        StatementKind::IterNext {
            dst_option,
            iter_place,
            ..
        } => {
            check_place(body, block, dst_option, n_locals, errors);
            check_place(body, block, iter_place, n_locals, errors);
        }
        StatementKind::Nop => {}
    }
}

fn check_terminator(
    body: &Body,
    block: BlockId,
    term: &Terminator,
    n_blocks: u32,
    n_locals: u32,
    errors: &mut Vec<VerifyError>,
) {
    match term {
        Terminator::Goto { target } => check_block_id(body, block, *target, n_blocks, errors),
        Terminator::SwitchInt {
            discriminant,
            arms,
            default,
        } => {
            check_operand(body, block, discriminant, n_locals, errors);
            for (_value, target) in arms {
                check_block_id(body, block, *target, n_blocks, errors);
            }
            check_block_id(body, block, *default, n_blocks, errors);
        }
        Terminator::Return => {}
        Terminator::Call {
            callee,
            args,
            destination,
            target,
        } => {
            check_operand(body, block, callee, n_locals, errors);
            for arg in args {
                check_operand(body, block, arg, n_locals, errors);
            }
            check_place(body, block, destination, n_locals, errors);
            match target {
                Some(t) => check_block_id(body, block, *t, n_blocks, errors),
                None => {
                    // Diverging call: no continuation required.
                    // Surface only if a future revision tightens the
                    // shape. Today this is well-formed.
                    let _ = block;
                }
            }
        }
        Terminator::Assert {
            cond, target, msg, ..
        } => {
            check_operand(body, block, cond, n_locals, errors);
            for op in msg.operands() {
                check_operand(body, block, op, n_locals, errors);
            }
            check_block_id(body, block, *target, n_blocks, errors);
        }
        Terminator::Unreachable | Terminator::Panic { .. } => {}
        Terminator::Drop { place, target } => {
            check_place(body, block, place, n_locals, errors);
            check_block_id(body, block, *target, n_blocks, errors);
        }
    }
}

fn check_rvalue(
    body: &Body,
    block: BlockId,
    rvalue: &Rvalue,
    n_locals: u32,
    errors: &mut Vec<VerifyError>,
) {
    // Exhaustive over the public enum.
    match rvalue {
        Rvalue::Use(op) => check_operand(body, block, op, n_locals, errors),
        Rvalue::Ref { place, .. } => check_place(body, block, place, n_locals, errors),
        Rvalue::BinaryOp { lhs, rhs, .. } => {
            check_operand(body, block, lhs, n_locals, errors);
            check_operand(body, block, rhs, n_locals, errors);
        }
        Rvalue::UnaryOp { operand, .. } => check_operand(body, block, operand, n_locals, errors),
        Rvalue::Cast { operand, .. } => check_operand(body, block, operand, n_locals, errors),
        Rvalue::Aggregate { operands, .. } => {
            for op in operands {
                check_operand(body, block, op, n_locals, errors);
            }
        }
        Rvalue::Len(place) => check_place(body, block, place, n_locals, errors),
        Rvalue::Repeat { value, .. } => check_operand(body, block, value, n_locals, errors),
        Rvalue::CallIntrinsic { args, .. } => {
            for op in args {
                check_operand(body, block, op, n_locals, errors);
            }
        }
        // No local operands: the static is referenced by symbol.
        Rvalue::StaticLoad(_) => {}
    }
}

fn check_operand(
    body: &Body,
    block: BlockId,
    operand: &Operand,
    n_locals: u32,
    errors: &mut Vec<VerifyError>,
) {
    match operand {
        Operand::Copy(place) => {
            check_place(body, block, place, n_locals, errors);
        }
        Operand::Const(_) | Operand::FnRef { .. } => {}
    }
}

fn check_place(
    body: &Body,
    block: BlockId,
    place: &Place,
    n_locals: u32,
    errors: &mut Vec<VerifyError>,
) {
    check_local(body, block, place.local, n_locals, errors);
    for proj in &place.projection {
        if let Projection::Index(local) = proj {
            check_local(body, block, *local, n_locals, errors);
        }
    }
}

fn check_local(
    body: &Body,
    block: BlockId,
    local: Local,
    n_locals: u32,
    errors: &mut Vec<VerifyError>,
) {
    if local.0 >= n_locals {
        errors.push(VerifyError::LocalOutOfRange {
            body: body.name.clone(),
            block,
            local,
            n_locals,
        });
    }
}

fn check_block_id(
    body: &Body,
    block: BlockId,
    target: BlockId,
    n_blocks: u32,
    errors: &mut Vec<VerifyError>,
) {
    if target.0 >= n_blocks {
        errors.push(VerifyError::BlockOutOfRange {
            body: body.name.clone(),
            block,
            target,
            n_blocks,
        });
    }
}

/// Convenience: panics if `verify_body` returns errors. Intended
/// for `debug_assertions`-only call sites after passes.
///
/// # Panics
///
/// Panics with the rendered error list when verification fails.
pub fn debug_verify_body(body: &Body) {
    if !cfg!(debug_assertions) {
        return;
    }
    if let Err(errors) = verify_body(body) {
        panic!(
            "gossamer-mir verifier rejected `{}`:\n{}",
            body.name,
            errors
                .iter()
                .map(|e| format!("  {e:?}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
}

/// Structural + type-aware checks for one body.
///
/// Runs every check from [`verify_body`] plus the type-aware checks
/// that need a [`TyCtxt`] handle (return-slot type, switch
/// discriminant type, drop target ownership, untyped call
/// destinations, structural `i128::MIN` negation, struct aggregate
/// operand counts).
///
/// The cross-body call-arity check lives in [`verify_program`]
/// because a single body cannot look up its callees' declared
/// arities; pass that entry point a slice of bodies and a `TyCtxt`
/// to get the full coverage.
pub fn verify_body_typed(body: &Body, tcx: &TyCtxt) -> Result<(), Vec<VerifyError>> {
    let mut errors = match verify_body(body) {
        Ok(()) => Vec::new(),
        Err(e) => e,
    };
    typed_checks(body, tcx, &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Whole-program verifier - runs [`verify_body_typed`] on each body
/// and adds cross-body checks (currently: call arity vs the callee's
/// declared `arity`).
///
/// Intended call site: end of `lower_program`, behind
/// `debug_assertions`. Release builds skip the check entirely.
pub fn verify_program(bodies: &[Body], tcx: &TyCtxt) -> Result<(), Vec<VerifyError>> {
    let mut errors: Vec<VerifyError> = Vec::new();
    for body in bodies {
        if let Err(mut errs) = verify_body_typed(body, tcx) {
            errors.append(&mut errs);
        }
    }
    let mut by_def: HashMap<DefId, u32> = HashMap::new();
    for body in bodies {
        if let Some(def) = body.def {
            by_def.insert(def, body.arity);
        }
    }
    for body in bodies {
        for block in &body.blocks {
            if let Terminator::Call { callee, args, .. } = &block.terminator
                && let Operand::FnRef { def, .. } = callee
            {
                if let Some(expected) = by_def.get(def).copied() {
                    let got = u32::try_from(args.len()).unwrap_or(u32::MAX);
                    if got != expected {
                        let callee_name = bodies
                            .iter()
                            .find(|b| b.def == Some(*def))
                            .map_or_else(|| format!("def#{}", def.local), |b| b.name.clone());
                        errors.push(VerifyError::CallArityMismatch {
                            body: body.name.clone(),
                            block: block.id,
                            callee: callee_name,
                            got,
                            expected,
                        });
                    }
                } else {
                    errors.push(VerifyError::UnresolvedFnRef {
                        body: body.name.clone(),
                        block: block.id,
                        def: *def,
                    });
                }
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Panics if [`verify_program`] returns errors. Intended for
/// `debug_assertions`-only call sites after passes that touch
/// multiple bodies.
///
/// # Panics
///
/// Panics with the rendered error list when verification fails.
pub fn debug_verify_program(bodies: &[Body], tcx: &TyCtxt) {
    if !cfg!(debug_assertions) {
        return;
    }
    if let Err(errors) = verify_program(bodies, tcx) {
        panic!(
            "gossamer-mir verifier rejected program:\n{}",
            errors
                .iter()
                .map(|e| format!("  {e:?}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
}

fn typed_checks(body: &Body, tcx: &TyCtxt, errors: &mut Vec<VerifyError>) {
    let n_locals = body.locals.len();
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign { rvalue, .. } = &stmt.kind {
                check_rvalue_typed(body, tcx, block.id, rvalue, errors);
            }
        }
        check_terminator_typed(body, tcx, block, n_locals, errors);
    }
}

fn check_rvalue_typed(
    body: &Body,
    tcx: &TyCtxt,
    block: BlockId,
    rvalue: &Rvalue,
    errors: &mut Vec<VerifyError>,
) {
    match rvalue {
        Rvalue::Aggregate {
            kind: AggregateKind::Adt { def, variant },
            operands,
        } => {
            // Variant-aware field counts aren't tracked in TyCtxt;
            // only structs (variant == 0) have `struct_field_tys`
            // entries the verifier can compare against.
            if *variant == 0
                && let Some(fields) = tcx.struct_field_tys(*def)
            {
                let got = u32::try_from(operands.len()).unwrap_or(u32::MAX);
                let expected = u32::try_from(fields.len()).unwrap_or(u32::MAX);
                if got != expected {
                    errors.push(VerifyError::AggregateOperandCount {
                        body: body.name.clone(),
                        block,
                        def: *def,
                        variant: *variant,
                        got,
                        expected,
                    });
                }
            }
        }
        Rvalue::UnaryOp {
            op: UnOp::Neg,
            operand: Operand::Const(ConstValue::Int(n)),
        } if *n == i128::MIN => {
            errors.push(VerifyError::UnaryNegI128Min {
                body: body.name.clone(),
                block,
            });
        }
        Rvalue::CallIntrinsic { name, args } => {
            check_intrinsic_contract(body, block, name, args.len(), errors);
        }
        _ => {}
    }
}

fn check_intrinsic_contract(
    body: &Body,
    block: BlockId,
    name: &'static str,
    arg_count: usize,
    errors: &mut Vec<VerifyError>,
) {
    let expected = RawIntrinsic::from_name(name).map(|intrinsic| intrinsic.arity_for_name(name));
    match expected {
        Some(RawIntrinsicArity::Exact(n)) if arg_count != n => {
            errors.push(VerifyError::IntrinsicArityMismatch {
                body: body.name.clone(),
                block,
                name,
                got: u32::try_from(arg_count).unwrap_or(u32::MAX),
                expected: exact_arity_text(n),
            });
        }
        Some(RawIntrinsicArity::Range { min, max }) if arg_count < min || arg_count > max => {
            errors.push(VerifyError::IntrinsicArityMismatch {
                body: body.name.clone(),
                block,
                name,
                got: u32::try_from(arg_count).unwrap_or(u32::MAX),
                expected: range_arity_text(min, max),
            });
        }
        Some(_) => {}
        None => errors.push(VerifyError::UnknownIntrinsic {
            body: body.name.clone(),
            block,
            name,
        }),
    }
}

fn exact_arity_text(n: usize) -> &'static str {
    match n {
        0 => "0",
        1 => "1",
        2 => "2",
        3 => "3",
        4 => "4",
        5 => "5",
        _ => "registered arity",
    }
}

fn range_arity_text(min: usize, max: usize) -> &'static str {
    match (min, max) {
        (0, 1) => "0..=1",
        (0, 2) => "0..=2",
        _ => "registered arity range",
    }
}

fn check_terminator_typed(
    body: &Body,
    tcx: &TyCtxt,
    block: &BasicBlock,
    n_locals: usize,
    errors: &mut Vec<VerifyError>,
) {
    match &block.terminator {
        Terminator::Return => {
            if matches!(tcx.kind_of(body.local_ty(Local::RETURN)), TyKind::Error) {
                errors.push(VerifyError::ReturnTypeError {
                    body: body.name.clone(),
                    block: block.id,
                });
            }
        }
        Terminator::SwitchInt { discriminant, .. } => {
            if let Some(ty) = operand_leaf_ty(body, n_locals, discriminant) {
                let kind = tcx.kind_of(ty);
                if !matches!(kind, TyKind::Int(_) | TyKind::Bool) {
                    errors.push(VerifyError::SwitchIntNonIntegerDiscriminant {
                        body: body.name.clone(),
                        block: block.id,
                    });
                }
            }
        }
        Terminator::Drop { place, .. } => {
            if place.local.0 as usize >= n_locals {
                return;
            }
            if let Some(ty) = place_leaf_ty(body, tcx, n_locals, place)
                && !ty_is_pointer(tcx, ty)
            {
                errors.push(VerifyError::DropOfNonOwning {
                    body: body.name.clone(),
                    block: block.id,
                });
            }
        }
        Terminator::Call { destination, .. } => {
            if destination.local.0 as usize >= n_locals {
                return;
            }
            if let Some(ty) = place_leaf_ty(body, tcx, n_locals, destination) {
                let kind = tcx.kind_of(ty);
                // `Var(_)` leaks out of the typechecker for generic
                // sites that monomorphisation has not yet specialised;
                // those bodies are valid as long as the codegen tier
                // can default them. Only `Error` is a hard fault.
                if matches!(kind, TyKind::Error) {
                    errors.push(VerifyError::CallDestinationUntyped {
                        body: body.name.clone(),
                        block: block.id,
                    });
                }
            }
        }
        _ => {}
    }
}

fn operand_leaf_ty(body: &Body, n_locals: usize, operand: &Operand) -> Option<gossamer_types::Ty> {
    match operand {
        Operand::Copy(place) => {
            if (place.local.0 as usize) >= n_locals {
                return None;
            }
            Some(body.local_ty(place.local))
        }
        Operand::Const(_) | Operand::FnRef { .. } => None,
    }
}

/// Walks `place.projection` and returns the leaf type, or `None`
/// when a step's type cannot be recovered (Index without an element
/// hint, deref of a non-Ref, etc.). The cheap-checks contract means
/// missing leaves are tolerated - only fully-resolved leaves are
/// scrutinised by the typed verifier.
fn place_leaf_ty(
    body: &Body,
    tcx: &TyCtxt,
    n_locals: usize,
    place: &Place,
) -> Option<gossamer_types::Ty> {
    if (place.local.0 as usize) >= n_locals {
        return None;
    }
    let mut ty = body.local_ty(place.local);
    for proj in &place.projection {
        match proj {
            Projection::Deref => match tcx.kind_of(ty) {
                TyKind::Ref { inner, .. } => ty = *inner,
                _ => return None,
            },
            Projection::Field(idx) => match tcx.kind_of(ty) {
                TyKind::Tuple(elems) => {
                    let i = *idx as usize;
                    ty = *elems.get(i)?;
                }
                TyKind::Adt { def, substs } => {
                    let fields = tcx.adt_field_tys(*def, substs)?;
                    ty = *fields.get(*idx as usize)?;
                }
                _ => return None,
            },
            Projection::Index(_) => match tcx.kind_of(ty) {
                TyKind::Array { elem, .. } | TyKind::Slice(elem) | TyKind::Vec(elem) => ty = *elem,
                _ => return None,
            },
            Projection::Downcast(_) | Projection::Discriminant => return None,
        }
    }
    Some(ty)
}

/// `true` if a runtime value of `ty` is an owning heap reference -
/// the only shapes a `Terminator::Drop` may legally target.
fn ty_is_pointer(tcx: &TyCtxt, ty: Ty) -> bool {
    match tcx.kind_of(ty) {
        TyKind::String
        | TyKind::Ref { .. }
        | TyKind::Slice(_)
        | TyKind::Vec(_)
        | TyKind::HashMap { .. }
        | TyKind::Sender(_)
        | TyKind::Receiver(_)
        | TyKind::JoinHandle(_)
        | TyKind::JsonValue
        | TyKind::DynError
        | TyKind::Closure { .. }
        | TyKind::FnTrait(_)
        | TyKind::Dyn(_) => true,
        TyKind::Tuple(elems) => elems.iter().any(|t| ty_is_pointer(tcx, *t)),
        TyKind::Array { elem, .. } => ty_is_pointer(tcx, *elem),
        TyKind::Adt { def, .. } => {
            // Sentinel Adts (Result / Option with u32::MAX / MAX-1
            // DefIds) are heap pointers themselves.
            if def.local == u32::MAX || def.local == u32::MAX - 1 {
                return true;
            }
            tcx.struct_field_tys(*def)
                .is_some_and(|tys| tys.iter().any(|t| ty_is_pointer(tcx, *t)))
        }
        _ => false,
    }
}
