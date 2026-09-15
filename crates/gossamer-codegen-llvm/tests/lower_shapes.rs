//! Unit-test coverage per MIR shape for the LLVM lowerer.
//!
//! Each test hand-rolls a synthetic Body, runs `render_ir_to_string`
//! against it, and asserts substring properties on the resulting IR
//! rather than committing a full snapshot. This avoids the
//! string-pool / metadata non-determinism that plagued the earlier
//! `lower_snapshots` suite while still catching regressions in the
//! lowering of each MIR shape.

#![allow(missing_docs)]

use gossamer_codegen_llvm::render_ir_to_string;
use gossamer_lex::{SourceMap, Span};
use gossamer_mir::{
    BasicBlock, BinOp, BlockId, Body, ConstValue, Local, LocalDecl, Operand, Place, Rvalue,
    Statement, StatementKind, Terminator,
};
use gossamer_resolve::DefId;
use gossamer_types::{ArrayLen, IntTy, TyCtxt, TyKind};

fn dummy_span() -> Span {
    let mut map = SourceMap::new();
    let file = map.add_file("shapes.gos", "");
    Span::new(file, 0, 0)
}

fn place(local: u32) -> Place {
    Place {
        local: Local(local),
        projection: Vec::new(),
    }
}

fn const_int_assign(local: u32, value: i64) -> Statement {
    Statement {
        span: dummy_span(),
        inlined: None,
        kind: StatementKind::Assign {
            place: place(local),
            rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(value)))),
        },
    }
}

fn binop_assign(local: u32, op: BinOp, lhs: u32, rhs: u32) -> Statement {
    Statement {
        span: dummy_span(),
        kind: StatementKind::Assign {
            place: place(local),
            rvalue: Rvalue::BinaryOp {
                op,
                lhs: Operand::Copy(place(lhs)),
                rhs: Operand::Copy(place(rhs)),
            },
        },
        inlined: None,
    }
}

fn build_binop_main(op: BinOp, lhs: i64, rhs: i64) -> (Body, TyCtxt) {
    let mut tcx = TyCtxt::new();
    let i64_ty = tcx.intern(TyKind::Int(IntTy::I64));
    let body = Body {
        name: "main".to_string(),
        def: None,
        arity: 0,
        locals: vec![
            LocalDecl {
                ty: i64_ty,
                debug_name: None,
                mutable: false,
                region: false,
            },
            LocalDecl {
                ty: i64_ty,
                debug_name: None,
                mutable: false,
                region: false,
            },
            LocalDecl {
                ty: i64_ty,
                debug_name: None,
                mutable: false,
                region: false,
            },
        ],
        blocks: vec![BasicBlock {
            id: BlockId(0),
            stmts: vec![
                const_int_assign(1, lhs),
                const_int_assign(2, rhs),
                binop_assign(0, op, 1, 2),
            ],
            terminator: Terminator::Return,
            span: dummy_span(),
            terminator_span: None,
            terminator_inlined: None,
        }],
        span: dummy_span(),
    };
    (body, tcx)
}

fn build_const_int_main(value: i64) -> (Body, TyCtxt) {
    let mut tcx = TyCtxt::new();
    let i64_ty = tcx.intern(TyKind::Int(IntTy::I64));
    let body = Body {
        name: "main".to_string(),
        def: None,
        arity: 0,
        locals: vec![LocalDecl {
            ty: i64_ty,
            debug_name: None,
            mutable: false,
            region: false,
        }],
        blocks: vec![BasicBlock {
            id: BlockId(0),
            stmts: vec![const_int_assign(0, value)],
            terminator: Terminator::Return,
            span: dummy_span(),
            terminator_span: None,
            terminator_inlined: None,
        }],
        span: dummy_span(),
    };
    (body, tcx)
}

fn build_integer_abs_main() -> (Body, TyCtxt) {
    let mut tcx = TyCtxt::new();
    let i64_ty = tcx.intern(TyKind::Int(IntTy::I64));
    let body = Body {
        name: "main".to_string(),
        def: None,
        arity: 0,
        locals: vec![
            LocalDecl {
                ty: i64_ty,
                debug_name: None,
                mutable: false,
                region: false,
            },
            LocalDecl {
                ty: i64_ty,
                debug_name: None,
                mutable: false,
                region: false,
            },
        ],
        blocks: vec![BasicBlock {
            id: BlockId(0),
            stmts: vec![
                const_int_assign(1, -42),
                Statement {
                    span: dummy_span(),
                    kind: StatementKind::Assign {
                        place: place(0),
                        rvalue: Rvalue::CallIntrinsic {
                            name: "abs",
                            args: vec![Operand::Copy(place(1))],
                        },
                    },
                    inlined: None,
                },
            ],
            terminator: Terminator::Return,
            span: dummy_span(),
            terminator_span: None,
            terminator_inlined: None,
        }],
        span: dummy_span(),
    };
    (body, tcx)
}

fn build_vec_pop_main() -> (Body, TyCtxt) {
    let mut tcx = TyCtxt::new();
    let unit = tcx.intern(TyKind::Unit);
    let i64_ty = tcx.intern(TyKind::Int(IntTy::I64));
    let vec_i64 = tcx.intern(TyKind::Vec(i64_ty));
    let option_i64 = tcx.intern(TyKind::Adt {
        def: DefId::local(u32::MAX - 1),
        substs: gossamer_types::Substs::from_types([i64_ty]),
    });
    let body = Body {
        name: "main".to_string(),
        def: None,
        arity: 0,
        locals: vec![
            LocalDecl {
                ty: unit,
                debug_name: None,
                mutable: false,
                region: false,
            },
            LocalDecl {
                ty: vec_i64,
                debug_name: None,
                mutable: true,
                region: false,
            },
            LocalDecl {
                ty: option_i64,
                debug_name: None,
                mutable: false,
                region: false,
            },
        ],
        blocks: vec![
            BasicBlock {
                id: BlockId(0),
                stmts: vec![],
                terminator: Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_vec_pop_opt".to_string())),
                    args: vec![Operand::Copy(place(1))],
                    destination: place(2),
                    target: Some(BlockId(1)),
                },
                span: dummy_span(),
                terminator_span: None,
                terminator_inlined: None,
            },
            BasicBlock {
                id: BlockId(1),
                stmts: vec![],
                terminator: Terminator::Return,
                span: dummy_span(),
                terminator_span: None,
                terminator_inlined: None,
            },
        ],
        span: dummy_span(),
    };
    (body, tcx)
}

fn build_chan_recv_option_main(try_recv: bool) -> (Body, TyCtxt) {
    let mut tcx = TyCtxt::new();
    let unit = tcx.intern(TyKind::Unit);
    let i64_ty = tcx.intern(TyKind::Int(IntTy::I64));
    let receiver_i64 = tcx.intern(TyKind::Receiver(i64_ty));
    let option_i64 = tcx.intern(TyKind::Adt {
        def: DefId::local(u32::MAX - 1),
        substs: gossamer_types::Substs::from_types([i64_ty]),
    });
    let callee = if try_recv {
        "gos_rt_chan_try_recv_option"
    } else {
        "gos_rt_chan_recv_option"
    };
    let body = Body {
        name: "main".to_string(),
        def: None,
        arity: 0,
        locals: vec![
            LocalDecl {
                ty: unit,
                debug_name: None,
                mutable: false,
                region: false,
            },
            LocalDecl {
                ty: receiver_i64,
                debug_name: None,
                mutable: false,
                region: false,
            },
            LocalDecl {
                ty: option_i64,
                debug_name: None,
                mutable: false,
                region: false,
            },
        ],
        blocks: vec![
            BasicBlock {
                id: BlockId(0),
                stmts: vec![],
                terminator: Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(callee.to_string())),
                    args: vec![Operand::Copy(place(1))],
                    destination: place(2),
                    target: Some(BlockId(1)),
                },
                span: dummy_span(),
                terminator_span: None,
                terminator_inlined: None,
            },
            BasicBlock {
                id: BlockId(1),
                stmts: vec![],
                terminator: Terminator::Return,
                span: dummy_span(),
                terminator_span: None,
                terminator_inlined: None,
            },
        ],
        span: dummy_span(),
    };
    (body, tcx)
}

fn build_vec_swap_main() -> (Body, TyCtxt) {
    let mut tcx = TyCtxt::new();
    let unit = tcx.intern(TyKind::Unit);
    let i64_ty = tcx.intern(TyKind::Int(IntTy::I64));
    let vec_i64 = tcx.intern(TyKind::Vec(i64_ty));
    let body = Body {
        name: "main".to_string(),
        def: None,
        arity: 0,
        locals: vec![
            LocalDecl {
                ty: unit,
                debug_name: None,
                mutable: false,
                region: false,
            },
            LocalDecl {
                ty: vec_i64,
                debug_name: None,
                mutable: true,
                region: false,
            },
            LocalDecl {
                ty: i64_ty,
                debug_name: None,
                mutable: false,
                region: false,
            },
            LocalDecl {
                ty: i64_ty,
                debug_name: None,
                mutable: false,
                region: false,
            },
        ],
        blocks: vec![
            BasicBlock {
                id: BlockId(0),
                stmts: vec![const_int_assign(2, 0), const_int_assign(3, 1)],
                terminator: Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_vec_swap_i64".to_string())),
                    args: vec![
                        Operand::Copy(place(1)),
                        Operand::Copy(place(2)),
                        Operand::Copy(place(3)),
                    ],
                    destination: place(0),
                    target: Some(BlockId(1)),
                },
                span: dummy_span(),
                terminator_span: None,
                terminator_inlined: None,
            },
            BasicBlock {
                id: BlockId(1),
                stmts: vec![],
                terminator: Terminator::Return,
                span: dummy_span(),
                terminator_span: None,
                terminator_inlined: None,
            },
        ],
        span: dummy_span(),
    };
    (body, tcx)
}

fn build_string_map_capacity_main() -> (Body, TyCtxt) {
    let mut tcx = TyCtxt::new();
    let unit = tcx.intern(TyKind::Unit);
    let i64_ty = tcx.intern(TyKind::Int(IntTy::I64));
    let string_ty = tcx.intern(TyKind::String);
    let map_ty = tcx.intern(TyKind::HashMap {
        key: string_ty,
        value: i64_ty,
        ordered: false,
    });
    let body = Body {
        name: "main".to_string(),
        def: None,
        arity: 0,
        locals: vec![
            LocalDecl {
                ty: unit,
                debug_name: None,
                mutable: false,
                region: false,
            },
            LocalDecl {
                ty: map_ty,
                debug_name: None,
                mutable: true,
                region: false,
            },
            LocalDecl {
                ty: i64_ty,
                debug_name: None,
                mutable: false,
                region: false,
            },
        ],
        blocks: vec![
            BasicBlock {
                id: BlockId(0),
                stmts: vec![const_int_assign(2, 16)],
                terminator: Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(
                        "std::collections::Map::with_capacity".to_string(),
                    )),
                    args: vec![Operand::Copy(place(2))],
                    destination: place(1),
                    target: Some(BlockId(1)),
                },
                span: dummy_span(),
                terminator_span: None,
                terminator_inlined: None,
            },
            BasicBlock {
                id: BlockId(1),
                stmts: vec![],
                terminator: Terminator::Return,
                span: dummy_span(),
                terminator_span: None,
                terminator_inlined: None,
            },
        ],
        span: dummy_span(),
    };
    (body, tcx)
}

#[test]
fn const_int_zero_emits_store_i64() {
    let (body, tcx) = build_const_int_main(0);
    let ir = render_ir_to_string(&[body], &tcx, false).unwrap();
    assert!(ir.contains("define i64"), "IR was:\n{ir}");
    assert!(ir.contains("store i64 0"), "IR was:\n{ir}");
}

#[test]
fn string_map_with_capacity_uses_typed_storage() {
    let (body, tcx) = build_string_map_capacity_main();
    let ir = render_ir_to_string(&[body], &tcx, false).unwrap();
    assert!(
        ir.contains("@gos_rt_map_new_with_capacity_typed(i32 1, i32 0, i64"),
        "IR must select pre-sized String/i64 storage:\n{ir}"
    );
    assert!(
        !ir.contains("@gos_rt_map_new_with_capacity(i32 8, i32 8"),
        "word-only capacity storage corrupts String map semantics:\n{ir}"
    );
}

#[test]
fn const_int_max_emits_max_literal() {
    let (body, tcx) = build_const_int_main(i64::MAX);
    let ir = render_ir_to_string(&[body], &tcx, false).unwrap();
    assert!(
        ir.contains(&format!("store i64 {}", i64::MAX)),
        "IR was:\n{ir}"
    );
}

#[test]
fn binop_add_i64_emits_add_instruction() {
    let (body, tcx) = build_binop_main(BinOp::Add, 3, 4);
    let ir = render_ir_to_string(&[body], &tcx, false).unwrap();
    assert!(ir.contains("add i64"), "IR was:\n{ir}");
}

#[test]
fn binop_sub_i64_emits_sub_instruction() {
    let (body, tcx) = build_binop_main(BinOp::Sub, 10, 3);
    let ir = render_ir_to_string(&[body], &tcx, false).unwrap();
    assert!(ir.contains("sub i64"), "IR was:\n{ir}");
}

#[test]
fn binop_mul_i64_emits_mul_instruction() {
    let (body, tcx) = build_binop_main(BinOp::Mul, 6, 7);
    let ir = render_ir_to_string(&[body], &tcx, false).unwrap();
    assert!(ir.contains("mul i64"), "IR was:\n{ir}");
}

#[test]
fn binop_div_i64_emits_sdiv_instruction() {
    let (body, tcx) = build_binop_main(BinOp::Div, 20, 4);
    let ir = render_ir_to_string(&[body], &tcx, false).unwrap();
    assert!(ir.contains("sdiv i64"), "IR was:\n{ir}");
}

#[test]
fn binop_bitand_i64_emits_and_instruction() {
    let (body, tcx) = build_binop_main(BinOp::BitAnd, 15, 9);
    let ir = render_ir_to_string(&[body], &tcx, false).unwrap();
    assert!(ir.contains("and i64"), "IR was:\n{ir}");
}

#[test]
fn binop_bitor_i64_emits_or_instruction() {
    let (body, tcx) = build_binop_main(BinOp::BitOr, 12, 5);
    let ir = render_ir_to_string(&[body], &tcx, false).unwrap();
    assert!(ir.contains("or i64"), "IR was:\n{ir}");
}

#[test]
fn binop_bitxor_i64_emits_xor_instruction() {
    let (body, tcx) = build_binop_main(BinOp::BitXor, 7, 3);
    let ir = render_ir_to_string(&[body], &tcx, false).unwrap();
    assert!(ir.contains("xor i64"), "IR was:\n{ir}");
}

#[test]
fn binop_shl_i64_emits_shl_instruction() {
    let (body, tcx) = build_binop_main(BinOp::Shl, 1, 4);
    let ir = render_ir_to_string(&[body], &tcx, false).unwrap();
    assert!(ir.contains("shl i64"), "IR was:\n{ir}");
}

#[test]
fn binop_shr_i64_emits_ashr_instruction() {
    let (body, tcx) = build_binop_main(BinOp::Shr, 64, 2);
    let ir = render_ir_to_string(&[body], &tcx, false).unwrap();
    // Signed shift right lowers to ashr (arithmetic shift right).
    assert!(ir.contains("ashr i64"), "IR was:\n{ir}");
}

#[test]
fn binop_rem_i64_emits_srem_instruction() {
    let (body, tcx) = build_binop_main(BinOp::Rem, 17, 5);
    let ir = render_ir_to_string(&[body], &tcx, false).unwrap();
    assert!(ir.contains("srem i64"), "IR was:\n{ir}");
}

#[test]
fn vec_i64_pop_option_is_inlined() {
    let (body, tcx) = build_vec_pop_main();
    let ir = render_ir_to_string(&[body], &tcx, false).unwrap();
    assert!(
        !ir.contains("call i128 @gos_rt_vec_pop_opt"),
        "Vec<i64>::pop should inline instead of calling runtime:\n{ir}"
    );
    assert!(ir.contains("vpop_some_"), "IR was:\n{ir}");
    assert!(ir.contains("shl i128"), "IR was:\n{ir}");
    assert!(ir.contains("store i128"), "IR was:\n{ir}");
}

#[test]
fn channel_recv_option_uses_status_out_pointer_call() {
    let (body, tcx) = build_chan_recv_option_main(false);
    let ir = render_ir_to_string(&[body], &tcx, false).unwrap();
    assert!(
        !ir.contains("call i128 @\"gos_rt_chan_recv_option\""),
        "channel recv should avoid direct i128 C-ABI return:\n{ir}"
    );
    assert!(
        ir.contains("call i32 @\"gos_rt_chan_recv\""),
        "IR was:\n{ir}"
    );
    assert!(ir.contains("shl i128"), "IR was:\n{ir}");
    assert!(ir.contains("store i128"), "IR was:\n{ir}");
}

#[test]
fn channel_try_recv_option_uses_status_out_pointer_call() {
    let (body, tcx) = build_chan_recv_option_main(true);
    let ir = render_ir_to_string(&[body], &tcx, false).unwrap();
    assert!(
        !ir.contains("call i128 @\"gos_rt_chan_try_recv_option\""),
        "channel try_recv should avoid direct i128 C-ABI return:\n{ir}"
    );
    assert!(
        ir.contains("call i32 @\"gos_rt_chan_try_recv\""),
        "IR was:\n{ir}"
    );
    assert!(ir.contains("shl i128"), "IR was:\n{ir}");
    assert!(ir.contains("store i128"), "IR was:\n{ir}");
}

#[test]
fn vec_i64_swap_is_inlined() {
    let (body, tcx) = build_vec_swap_main();
    let ir = render_ir_to_string(&[body], &tcx, false).unwrap();
    assert!(
        !ir.contains("call void @gos_rt_vec_swap_i64"),
        "Vec<i64>::swap should inline instead of calling runtime:\n{ir}"
    );
    assert!(ir.contains("vsw_swap_"), "IR was:\n{ir}");
    assert!(ir.contains("store i64"), "IR was:\n{ir}");
}

#[test]
fn ir_contains_module_id_header() {
    let (body, tcx) = build_const_int_main(0);
    let ir = render_ir_to_string(&[body], &tcx, false).unwrap();
    assert!(ir.contains("ModuleID"), "IR was:\n{ir}");
}

#[test]
fn ir_contains_target_triple() {
    let (body, tcx) = build_const_int_main(0);
    let ir = render_ir_to_string(&[body], &tcx, false).unwrap();
    assert!(ir.contains("target triple"), "IR was:\n{ir}");
}

#[test]
fn large_fixed_array_local_spills_to_heap_storage() {
    let mut tcx = TyCtxt::new();
    let unit_ty = tcx.intern(TyKind::Unit);
    let i64_ty = tcx.intern(TyKind::Int(IntTy::I64));
    let arr_ty = tcx.intern(TyKind::Array {
        elem: i64_ty,
        len: ArrayLen::Concrete(100_000_000),
    });
    let body = Body {
        name: "main".to_string(),
        def: None,
        arity: 0,
        locals: vec![
            LocalDecl {
                ty: unit_ty,
                debug_name: None,
                mutable: false,
                region: false,
            },
            LocalDecl {
                ty: arr_ty,
                debug_name: None,
                mutable: false,
                region: false,
            },
        ],
        blocks: vec![BasicBlock {
            id: BlockId(0),
            stmts: vec![Statement {
                span: dummy_span(),
                kind: StatementKind::Assign {
                    place: place(1),
                    rvalue: Rvalue::Repeat {
                        value: Operand::Const(ConstValue::Int(0)),
                        count: 100_000_000,
                    },
                },
                inlined: None,
            }],
            terminator: Terminator::Return,
            span: dummy_span(),
            terminator_span: None,
            terminator_inlined: None,
        }],
        span: dummy_span(),
    };

    let ir = render_ir_to_string(&[body], &tcx, false).unwrap();
    // Matched without the return-attribute list so an added attribute such as
    // `noalias` does not read as a behaviour change.
    assert!(
        ir.lines().any(|line| {
            line.contains("%l1 = call") && line.contains("@gos_rt_aggr_alloc(i64 800000000)")
        }),
        "IR was:\n{ir}"
    );
    assert!(!ir.contains("%l1 = alloca"), "IR was:\n{ir}");
    assert!(
        ir.contains("call void @\"gos_rt_aggr_free\"(ptr %l1, i64 800000000)"),
        "IR was:\n{ir}"
    );
}

#[test]
fn fixed_u8_array_uses_packed_native_storage() {
    let mut tcx = TyCtxt::new();
    let unit_ty = tcx.intern(TyKind::Unit);
    let u8_ty = tcx.intern(TyKind::Int(IntTy::U8));
    let arr_ty = tcx.intern(TyKind::Array {
        elem: u8_ty,
        len: ArrayLen::Concrete(64),
    });
    let body = Body {
        name: "main".to_string(),
        def: None,
        arity: 0,
        locals: vec![
            LocalDecl {
                ty: unit_ty,
                debug_name: None,
                mutable: false,
                region: false,
            },
            LocalDecl {
                ty: arr_ty,
                debug_name: None,
                mutable: true,
                region: false,
            },
        ],
        blocks: vec![BasicBlock {
            id: BlockId(0),
            stmts: vec![Statement {
                span: dummy_span(),
                kind: StatementKind::Assign {
                    place: place(1),
                    rvalue: Rvalue::Repeat {
                        value: Operand::Const(ConstValue::Int(0)),
                        count: 64,
                    },
                },
                inlined: None,
            }],
            terminator: Terminator::Return,
            span: dummy_span(),
            terminator_span: None,
            terminator_inlined: None,
        }],
        span: dummy_span(),
    };

    let ir = render_ir_to_string(&[body], &tcx, false).unwrap();
    assert!(ir.contains("alloca [64 x i8]"), "IR was:\n{ir}");
    assert!(
        ir.contains("@llvm.memset.p0.i64(ptr %l1, i8 0, i64 64"),
        "IR was:\n{ir}"
    );
    assert!(!ir.contains("alloca [64 x i64]"), "IR was:\n{ir}");
}

#[test]
fn integer_abs_uses_the_integer_runtime_helper() {
    let (body, tcx) = build_integer_abs_main();
    let ir = render_ir_to_string(&[body], &tcx, false).unwrap();
    assert!(
        ir.contains("call i64 @gos_rt_math_abs_i64(i64"),
        "integer abs should call the integer helper:\n{ir}"
    );
    assert!(
        !ir.contains("llvm.fabs.f64"),
        "integer abs must not call the floating-point intrinsic:\n{ir}"
    );
}

/// Lowers real source through the compiler's own pipeline, so a convention
/// test reads the IR a `gos build` produces rather than a hand-rolled body.
fn build_from_source(source: &str) -> (Vec<gossamer_mir::Body>, TyCtxt) {
    let mut map = SourceMap::new();
    let file = map.add_file("convention.gos", source.to_string());
    let (mut sf, parse_diags) = gossamer_parse::autoderive::parse_with_autoderive(source, file);
    assert!(parse_diags.is_empty(), "parse: {parse_diags:?}");
    let (resolutions, _) = gossamer_resolve::resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &resolutions);
    let mut tcx = TyCtxt::new();
    let (table, diagnostics) = gossamer_types::typecheck_source_file(&sf, &resolutions, &mut tcx);
    assert!(diagnostics.is_empty(), "typecheck: {diagnostics:?}");
    let hir = gossamer_hir::lower_source_file(&sf, &resolutions, &table, &mut tcx);
    let mut bodies = gossamer_mir::lower_program(&hir, &mut tcx);
    for body in &mut bodies {
        gossamer_mir::optimise(body, &tcx);
    }
    (bodies, tcx)
}

fn body_text<'a>(ir: &'a str, define_prefix: &str) -> &'a str {
    let start = ir
        .find(define_prefix)
        .unwrap_or_else(|| panic!("{define_prefix} not found in:\n{ir}"));
    let rest = &ir[start..];
    let end = rest.find("\n}").unwrap_or(rest.len());
    &rest[..end]
}

#[test]
fn heap_enum_by_value_parameter_crosses_a_direct_call_as_the_word() {
    let (bodies, tcx) = build_from_source(
        r#"
enum Tree { Node(Tree, Tree), Nil }

fn check(tree: Tree) -> i64 {
    match tree {
        Tree::Node(l, r) => 1 + check(l) + check(r),
        Tree::Nil => 0,
    }
}

fn main() {
    println("{}", check(Tree::Node(Tree::Nil, Tree::Nil)))
}
"#,
    );
    let ir = render_ir_to_string(&bodies, &tcx, false).unwrap();
    let body = body_text(&ir, "define i64 @\"check\"(");

    let signature = body.lines().next().unwrap();
    assert_eq!(
        signature, "define i64 @\"check\"(ptr %p0) #0 {",
        "a one-word heap enum parameter is the word, with no by-pointer attributes:\n{ir}"
    );
    assert!(
        body.contains("store ptr %p0, ptr %l1"),
        "the entry stores the word into the local slot:\n{body}"
    );
    assert!(
        !body.contains("llvm.memcpy.p0.p0.i64(ptr %l1, ptr %p0"),
        "the entry copies nothing from a caller slot:\n{body}"
    );

    let calls: Vec<&str> = body
        .lines()
        .filter(|l| l.contains("call i64 @\"check\"("))
        .collect();
    assert_eq!(calls.len(), 2, "both recursive calls are present:\n{body}");
    for call in calls {
        assert!(
            !call.contains("(ptr %l"),
            "a recursive call passes the loaded word, not a slot address: {call}"
        );
    }
}

#[test]
fn multi_slot_struct_parameter_still_arrives_by_pointer() {
    let (bodies, tcx) = build_from_source(
        r#"
struct Point { x: i64, y: i64 }

fn total(p: Point) -> i64 { p.x + p.y }

fn main() { println("{}", total(Point { x: 1, y: 2 })) }
"#,
    );
    let ir = render_ir_to_string(&bodies, &tcx, false).unwrap();
    let body = body_text(&ir, "define i64 @\"total\"(");

    let signature = body.lines().next().unwrap();
    assert_eq!(
        signature, "define i64 @\"total\"(ptr readonly nocapture %p0) #0 {",
        "a flat-slot aggregate keeps the by-pointer convention:\n{ir}"
    );
    assert!(
        body.contains("llvm.memcpy.p0.p0.i64(ptr %l1, ptr %p0, i64 16"),
        "the entry copies the caller's two slots into this frame:\n{body}"
    );
}
