//! Structural-invariant tests for `gossamer_mir::verify`.
//!
//! Two halves:
//!
//! 1. Positive: every body produced by the lowerer for a corpus
//!    of representative programs passes `verify_body`. This is
//!    the property the production pipeline relies on; if it
//!    drifts the `optimise()` pass starts panicking under
//!    `debug_assertions`.
//! 2. Negative: hand-corrupt a body and assert the matching
//!    `VerifyError` fires. Pins each diagnostic shape so a
//!    refactor that loses an invariant check is a test failure,
//!    not a silent regression.

#![allow(missing_docs)]

use gossamer_hir::lower_source_file;
use gossamer_lex::SourceMap;
use gossamer_mir::verify::{VerifyError, verify_body};
use gossamer_mir::{
    BlockId, Body, IteratorAdapterKind, IteratorOwnership, IteratorSourceKind, Local, Operand,
    Place, Statement, StatementKind, Terminator, lower_program, optimise,
};
use gossamer_parse::parse_source_file;
use gossamer_resolve::resolve_source_file;
use gossamer_types::{TyCtxt, typecheck_source_file};

fn build(source: &str) -> (Vec<Body>, TyCtxt) {
    let mut map = SourceMap::new();
    let file = map.add_file("verify.gos", source.to_string());
    let (mut sf, diags) = parse_source_file(source, file);
    assert!(diags.is_empty(), "parse: {diags:?}");
    let (resolutions, _) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &resolutions);
    let mut tcx = TyCtxt::new();
    let (table, _) = typecheck_source_file(&sf, &resolutions, &mut tcx);
    let hir = lower_source_file(&sf, &resolutions, &table, &mut tcx);
    let bodies = lower_program(&hir, &mut tcx);
    (bodies, tcx)
}

#[test]
fn retained_mir_lower_oom_reproducers_terminate() {
    const LARGE_REPRO: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fuzz/artifacts/mir_lower/oom-73482b7de2d0447f96f167d9ecf35dabf9628704"
    ));
    const EMPTY_REPRO: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fuzz/artifacts/mir_lower/oom-da39a3ee5e6b4b0d3255bfef95601890afd80709"
    ));

    for (name, bytes) in [("large", LARGE_REPRO), ("empty", EMPTY_REPRO)] {
        let source = std::str::from_utf8(bytes).expect("retained MIR artifact is UTF-8");
        let mut map = SourceMap::new();
        let file = map.add_file(format!("mir-lower-oom-{name}.gos"), source.to_owned());
        let (mut sf, parse_diagnostics) = parse_source_file(source, file);
        if !parse_diagnostics.is_empty() {
            continue;
        }
        let (resolutions, resolve_diagnostics) = resolve_source_file(&sf);
        let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &resolutions);
        if !resolve_diagnostics.is_empty() {
            continue;
        }
        let mut tcx = TyCtxt::new();
        let (table, type_diagnostics) = typecheck_source_file(&sf, &resolutions, &mut tcx);
        if !type_diagnostics.is_empty() {
            continue;
        }
        let hir = lower_source_file(&sf, &resolutions, &table, &mut tcx);
        let mut bodies = lower_program(&hir, &mut tcx);
        for body in &mut bodies {
            optimise(body, &tcx);
            verify_body(body).expect("retained MIR artifact must preserve verifier invariants");
        }
    }
}

#[test]
fn identity_body_passes_verify() {
    let (bodies, _) = build("fn id(x: i64) -> i64 { x }\n");
    for body in &bodies {
        verify_body(body).expect("identity body must verify");
    }
}

#[test]
fn binary_op_body_passes_verify() {
    let (bodies, _) = build("fn add(a: i64, b: i64) -> i64 { a + b }\n");
    for body in &bodies {
        verify_body(body).expect("binary-op body must verify");
    }
}

#[test]
fn match_body_passes_verify() {
    let source = r"
fn pick(b: bool) -> i64 {
    match b { true => 1, false => 0 }
}
";
    let (bodies, _) = build(source);
    for body in &bodies {
        verify_body(body).expect("match body must verify");
    }
}

#[test]
fn loop_body_passes_verify() {
    let source = r"
fn count(n: i64) -> i64 {
    let mut i = 0
    while i < n { i = i + 1 }
    i
}
";
    let (bodies, _) = build(source);
    for body in &bodies {
        verify_body(body).expect("loop body must verify");
    }
}

#[test]
fn optimise_preserves_verify_invariants() {
    // Several programs exercising arms of `optimise`. After
    // each pass `optimise` runs `debug_verify_body` itself; this
    // explicit second check pins the invariant at the
    // post-optimise resting state.
    let sources = [
        "fn id(x: i64) -> i64 { x }\n",
        "fn add(a: i64, b: i64) -> i64 { a + b }\n",
        "fn const_branch() -> i64 { if true { 1 } else { 0 } }\n",
        "fn loop_acc(n: i64) -> i64 { let mut s = 0\n let mut i = 0\n while i < n { s = s + i\n i = i + 1 }\n s }\n",
    ];
    for source in sources {
        let (mut bodies, tcx) = build(source);
        for body in &mut bodies {
            optimise(body, &tcx);
            verify_body(body).expect("optimise must preserve verify invariants");
        }
    }
}

// ----------------------------------------------------------------
// Negative cases - hand-corrupted bodies.
// ----------------------------------------------------------------

#[test]
fn block_id_mismatch_is_detected() {
    let (mut bodies, _) = build("fn id(x: i64) -> i64 { x }\n");
    let body = &mut bodies[0];
    // Stamp a wrong stored id on the entry block.
    body.blocks[0].id = BlockId(42);
    let errors = verify_body(body).expect_err("mismatched block id must fail");
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, VerifyError::BlockIdMismatch { stored, .. } if stored.0 == 42)),
        "expected BlockIdMismatch in {errors:?}",
    );
}

#[test]
fn out_of_range_goto_target_is_detected() {
    let source = r"
fn pick(b: bool) -> i64 {
    if b { 1 } else { 0 }
}
";
    let (mut bodies, _) = build(source);
    let body = &mut bodies[0];
    // Locate any Goto and rewrite its target to a block past the
    // end of the CFG.
    let n_blocks = body.blocks.len() as u32;
    let mut rewrote_one = false;
    for block in &mut body.blocks {
        if let Terminator::Goto { target } = &mut block.terminator {
            *target = BlockId(n_blocks + 5);
            rewrote_one = true;
            break;
        }
    }
    assert!(rewrote_one, "test fixture: expected at least one Goto");
    let errors = verify_body(body).expect_err("out-of-range target must fail");
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, VerifyError::BlockOutOfRange { .. })),
        "expected BlockOutOfRange in {errors:?}",
    );
}

#[test]
fn out_of_range_local_is_detected() {
    let (mut bodies, _) = build("fn id(x: i64) -> i64 { x }\n");
    let body = &mut bodies[0];
    let n_locals = body.locals.len() as u32;
    // Inject a StorageLive referencing a local past the end.
    body.blocks[0].stmts.insert(
        0,
        Statement {
            kind: StatementKind::StorageLive(Local(n_locals + 7)),
            span: body.span,
            inlined: None,
        },
    );
    let errors = verify_body(body).expect_err("out-of-range local must fail");
    assert!(
        errors.iter().any(
            |e| matches!(e, VerifyError::LocalOutOfRange { local, .. } if local.0 == n_locals + 7)
        ),
        "expected LocalOutOfRange in {errors:?}",
    );
}

#[test]
fn out_of_range_local_in_place_projection_is_detected() {
    // Index projection uses a Local; corrupt that one.
    let (mut bodies, _) = build("fn id(x: i64) -> i64 { x }\n");
    let body = &mut bodies[0];
    let n_locals = body.locals.len() as u32;
    body.blocks[0].stmts.insert(
        0,
        Statement {
            kind: StatementKind::Assign {
                place: Place::local(Local(0)),
                rvalue: gossamer_mir::Rvalue::Use(Operand::Copy(Place::local(Local(n_locals + 3)))),
            },
            span: body.span,
            inlined: None,
        },
    );
    let errors = verify_body(body).expect_err("out-of-range copy must fail");
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, VerifyError::LocalOutOfRange { .. })),
        "expected LocalOutOfRange in {errors:?}",
    );
}

#[test]
fn empty_blocks_is_detected() {
    let (mut bodies, _) = build("fn id(x: i64) -> i64 { x }\n");
    let body = &mut bodies[0];
    body.blocks.clear();
    let errors = verify_body(body).expect_err("empty blocks must fail");
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, VerifyError::EmptyBlocks { .. })),
        "expected EmptyBlocks in {errors:?}",
    );
}

// ----------------------------------------------------------------
// Type-aware checks (C17).
// ----------------------------------------------------------------

use gossamer_mir::verify::{verify_body_typed, verify_program};
use gossamer_mir::{AggregateKind, ConstValue, LocalDecl, Rvalue, UnOp};

#[test]
fn verify_program_passes_clean_bodies() {
    let (bodies, tcx) = build("fn add(a: i64, b: i64) -> i64 { a + b }\n");
    verify_program(&bodies, &tcx).expect("clean program must verify");
}

#[test]
fn call_arity_mismatch_is_detected() {
    let source = r"
fn callee(a: i64, b: i64) -> i64 { a + b }
fn caller() -> i64 { callee(1, 2) }
";
    let (mut bodies, tcx) = build(source);
    // Strip a single arg from any Call to `callee`.
    let mut rewrote = false;
    for body in &mut bodies {
        for block in &mut body.blocks {
            if let Terminator::Call { args, .. } = &mut block.terminator
                && !args.is_empty()
                && body.name == "caller"
            {
                args.pop();
                rewrote = true;
                break;
            }
        }
        if rewrote {
            break;
        }
    }
    assert!(rewrote, "test fixture: expected a Call terminator");
    let errors = verify_program(&bodies, &tcx).expect_err("arity mismatch must fail");
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, VerifyError::CallArityMismatch { .. })),
        "expected CallArityMismatch in {errors:?}",
    );
}

#[test]
fn unresolved_fn_ref_is_detected() {
    use gossamer_resolve::DefId;
    let source = r"
fn callee(a: i64) -> i64 { a }
fn caller() -> i64 { callee(1) }
";
    let (mut bodies, tcx) = build(source);
    let missing = DefId::local(9_999);
    let mut rewrote = false;
    for body in &mut bodies {
        for block in &mut body.blocks {
            if let Terminator::Call { callee, .. } = &mut block.terminator
                && body.name == "caller"
            {
                *callee = Operand::FnRef {
                    def: missing,
                    substs: gossamer_types::Substs::new(),
                };
                rewrote = true;
                break;
            }
        }
        if rewrote {
            break;
        }
    }
    assert!(rewrote, "test fixture: expected a Call terminator");
    let errors = verify_program(&bodies, &tcx).expect_err("missing FnRef must fail");
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, VerifyError::UnresolvedFnRef { def, .. } if *def == missing)),
        "expected UnresolvedFnRef in {errors:?}",
    );
}

#[test]
fn return_type_error_is_detected() {
    let (mut bodies, mut tcx) = build("fn id(x: i64) -> i64 { x }\n");
    let body = &mut bodies[0];
    let err_ty = tcx.error_ty();
    body.locals[0].ty = err_ty;
    let errors = verify_body_typed(body, &tcx).expect_err("error return type must fail");
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, VerifyError::ReturnTypeError { .. })),
        "expected ReturnTypeError in {errors:?}",
    );
}

#[test]
fn switch_int_non_integer_discriminant_is_detected() {
    let (mut bodies, mut tcx) = build("fn pick(b: bool) -> i64 { if b { 1 } else { 0 } }\n");
    let body = &mut bodies[0];
    // Inject a SwitchInt on a String operand. Allocate a String
    // local, then rewrite an existing SwitchInt's discriminant to
    // read from it.
    let str_ty = tcx.string_ty();
    body.locals.push(LocalDecl {
        ty: str_ty,
        debug_name: None,
        mutable: false,
        region: false,
    });
    let bad_local = Local((body.locals.len() - 1) as u32);
    let mut rewrote = false;
    for block in &mut body.blocks {
        if let Terminator::SwitchInt { discriminant, .. } = &mut block.terminator {
            *discriminant = Operand::Copy(Place::local(bad_local));
            rewrote = true;
            break;
        }
    }
    assert!(rewrote, "test fixture: expected a SwitchInt terminator");
    let errors = verify_body_typed(body, &tcx).expect_err("non-int switch discriminant must fail");
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, VerifyError::SwitchIntNonIntegerDiscriminant { .. })),
        "expected SwitchIntNonIntegerDiscriminant in {errors:?}",
    );
}

#[test]
fn drop_of_non_owning_is_detected() {
    let (mut bodies, tcx) = build("fn id(x: i64) -> i64 { x }\n");
    let body = &mut bodies[0];
    // x is i64 - not a heap pointer. Inject a Drop targeting it
    // and a fresh dummy block to satisfy block-id contiguity.
    let span = body.span;
    let new_id = BlockId(body.blocks.len() as u32);
    body.blocks.push(gossamer_mir::BasicBlock {
        id: new_id,
        stmts: Vec::new(),
        terminator: Terminator::Return,
        span,
        terminator_span: None,
        terminator_inlined: None,
    });
    let drop_block_id = BlockId(body.blocks.len() as u32);
    body.blocks.push(gossamer_mir::BasicBlock {
        id: drop_block_id,
        stmts: Vec::new(),
        terminator: Terminator::Drop {
            place: Place::local(Local(1)),
            target: new_id,
        },
        span,
        terminator_span: None,
        terminator_inlined: None,
    });
    let errors = verify_body_typed(body, &tcx).expect_err("drop of i64 must fail");
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, VerifyError::DropOfNonOwning { .. })),
        "expected DropOfNonOwning in {errors:?}",
    );
}

#[test]
fn unary_neg_i128_min_is_detected() {
    let (mut bodies, tcx) = build("fn n() -> i64 { 0 }\n");
    let body = &mut bodies[0];
    let span = body.span;
    body.blocks[0].stmts.insert(
        0,
        Statement {
            kind: StatementKind::Assign {
                place: Place::local(Local(0)),
                rvalue: Rvalue::UnaryOp {
                    op: UnOp::Neg,
                    operand: Operand::Const(ConstValue::Int(i128::MIN)),
                },
            },
            span,
            inlined: None,
        },
    );
    let errors = verify_body_typed(body, &tcx).expect_err("neg(i128::MIN) must fail");
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, VerifyError::UnaryNegI128Min { .. })),
        "expected UnaryNegI128Min in {errors:?}",
    );
}

#[test]
fn call_destination_untyped_is_detected() {
    let (mut bodies, mut tcx) = build("fn id(x: i64) -> i64 { x }\n");
    let body = &mut bodies[0];
    let err_ty = tcx.error_ty();
    let span = body.span;
    // Add an Error-typed dest local; splice a Call terminator
    // through a fresh continuation block.
    body.locals.push(LocalDecl {
        ty: err_ty,
        debug_name: None,
        mutable: false,
        region: false,
    });
    let bad_dest = Local((body.locals.len() - 1) as u32);
    let cont_id = BlockId(body.blocks.len() as u32);
    body.blocks.push(gossamer_mir::BasicBlock {
        id: cont_id,
        stmts: Vec::new(),
        terminator: Terminator::Return,
        span,
        terminator_span: None,
        terminator_inlined: None,
    });
    let call_id = BlockId(body.blocks.len() as u32);
    body.blocks.push(gossamer_mir::BasicBlock {
        id: call_id,
        stmts: Vec::new(),
        terminator: Terminator::Call {
            callee: Operand::Const(ConstValue::Str("phantom".to_string())),
            args: Vec::new(),
            destination: Place::local(bad_dest),
            target: Some(cont_id),
        },
        span,
        terminator_span: None,
        terminator_inlined: None,
    });
    let errors = verify_body_typed(body, &tcx).expect_err("untyped call destination must fail");
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, VerifyError::CallDestinationUntyped { .. })),
        "expected CallDestinationUntyped in {errors:?}",
    );
}

#[test]
fn aggregate_operand_count_is_detected() {
    use gossamer_resolve::DefId;
    use gossamer_types::IntTy;
    let (mut bodies, mut tcx) = build("fn id(x: i64) -> i64 { x }\n");
    let body = &mut bodies[0];
    // Register a fake struct with three fields and emit an
    // Aggregate with two operands.
    let i64_ty = tcx.int_ty(IntTy::I64);
    let fake = DefId::local(7_777);
    tcx.register_struct_fields(fake, vec![i64_ty, i64_ty, i64_ty]);
    let span = body.span;
    body.blocks[0].stmts.insert(
        0,
        Statement {
            kind: StatementKind::Assign {
                place: Place::local(Local(0)),
                rvalue: Rvalue::Aggregate {
                    kind: AggregateKind::Adt {
                        def: fake,
                        variant: 0,
                    },
                    operands: vec![
                        Operand::Const(ConstValue::Int(0)),
                        Operand::Const(ConstValue::Int(0)),
                    ],
                },
            },
            span,
            inlined: None,
        },
    );
    let errors = verify_body_typed(body, &tcx).expect_err("short aggregate must fail");
    assert!(
        errors.iter().any(|e| matches!(
            e,
            VerifyError::AggregateOperandCount {
                got: 2,
                expected: 3,
                ..
            }
        )),
        "expected AggregateOperandCount in {errors:?}",
    );
}

#[test]
fn unknown_intrinsic_is_detected() {
    let (mut bodies, tcx) = build("fn id(x: i64) -> i64 { x }\n");
    let body = &mut bodies[0];
    body.blocks[0].stmts.insert(
        0,
        Statement {
            kind: StatementKind::Assign {
                place: Place::local(Local(0)),
                rvalue: Rvalue::CallIntrinsic {
                    name: "is_halted",
                    args: Vec::new(),
                },
            },
            span: body.span,
            inlined: None,
        },
    );
    let errors = verify_body_typed(body, &tcx).expect_err("unknown intrinsic must fail");
    assert!(
        errors.iter().any(|e| matches!(
            e,
            VerifyError::UnknownIntrinsic {
                name: "is_halted",
                ..
            }
        )),
        "expected UnknownIntrinsic in {errors:?}",
    );
}

#[test]
fn intrinsic_arity_mismatch_is_detected() {
    let (mut bodies, tcx) = build("fn id(x: i64) -> i64 { x }\n");
    let body = &mut bodies[0];
    body.blocks[0].stmts.insert(
        0,
        Statement {
            kind: StatementKind::Assign {
                place: Place::local(Local(0)),
                rvalue: Rvalue::CallIntrinsic {
                    name: "gos_load",
                    args: vec![Operand::Const(ConstValue::Int(0))],
                },
            },
            span: body.span,
            inlined: None,
        },
    );
    let errors = verify_body_typed(body, &tcx).expect_err("bad intrinsic arity must fail");
    assert!(
        errors.iter().any(|e| matches!(
            e,
            VerifyError::IntrinsicArityMismatch {
                name: "gos_load",
                got: 1,
                expected: "2",
                ..
            }
        )),
        "expected IntrinsicArityMismatch in {errors:?}",
    );
}

#[test]
fn typed_iterator_statements_preserve_linear_state_shape() {
    let (mut bodies, tcx) = build("fn id(x: i64) -> i64 { x }\n");
    let body = &mut bodies[0];
    let span = body.span;
    let i64_ty = body.locals[0].ty;
    for _ in 0..3 {
        body.locals.push(LocalDecl {
            ty: i64_ty,
            debug_name: None,
            mutable: true,
            region: false,
        });
    }
    body.blocks[0].stmts.splice(
        0..0,
        [
            Statement {
                kind: StatementKind::IterSource {
                    dst: Place::local(Local(2)),
                    source_kind: IteratorSourceKind::Range,
                    source: Operand::Const(ConstValue::Int(0)),
                    item_ty: i64_ty,
                    ownership: IteratorOwnership::Owning,
                },
                span,
                inlined: None,
            },
            Statement {
                kind: StatementKind::IterAdapter {
                    dst: Place::local(Local(3)),
                    adapter_kind: IteratorAdapterKind::Take,
                    upstream: Place::local(Local(2)),
                    closure_or_arg: Some(Operand::Const(ConstValue::Int(3))),
                    item_ty: i64_ty,
                },
                span,
                inlined: None,
            },
            Statement {
                kind: StatementKind::IterNext {
                    dst_option: Place::local(Local(4)),
                    iter_place: Place::local(Local(3)),
                    item_ty: i64_ty,
                },
                span,
                inlined: None,
            },
        ],
    );
    verify_body_typed(body, &tcx).expect("well-formed typed iterator MIR must verify");
}

#[test]
fn typed_iterator_verifier_rejects_aliased_or_mismatched_states() {
    let (mut bodies, _) = build("fn id(x: i64) -> i64 { x }\n");
    let body = &mut bodies[0];
    let span = body.span;
    let i64_ty = body.locals[0].ty;
    for _ in 0..3 {
        body.locals.push(LocalDecl {
            ty: i64_ty,
            debug_name: None,
            mutable: true,
            region: false,
        });
    }
    body.blocks[0].stmts.splice(
        0..0,
        [
            Statement {
                kind: StatementKind::IterSource {
                    dst: Place {
                        local: Local(2),
                        projection: vec![gossamer_mir::Projection::Field(0)],
                    },
                    source_kind: IteratorSourceKind::Slice,
                    source: Operand::Const(ConstValue::Int(0)),
                    item_ty: i64_ty,
                    ownership: IteratorOwnership::Owning,
                },
                span,
                inlined: None,
            },
            Statement {
                kind: StatementKind::IterAdapter {
                    dst: Place::local(Local(3)),
                    adapter_kind: IteratorAdapterKind::Map,
                    upstream: Place::local(Local(2)),
                    closure_or_arg: None,
                    item_ty: i64_ty,
                },
                span,
                inlined: None,
            },
            Statement {
                kind: StatementKind::IterAdapter {
                    dst: Place::local(Local(4)),
                    adapter_kind: IteratorAdapterKind::Filter,
                    upstream: Place::local(Local(2)),
                    closure_or_arg: None,
                    item_ty: i64_ty,
                },
                span,
                inlined: None,
            },
        ],
    );
    let errors = verify_body(body).expect_err("invalid iterator state must fail");
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, VerifyError::IteratorStateProjected { .. }))
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, VerifyError::IteratorOwnershipMismatch { .. }))
    );
    assert!(errors.iter().any(|e| matches!(
        e,
        VerifyError::IteratorStateConsumedTwice {
            local: Local(2),
            ..
        }
    )));
}
