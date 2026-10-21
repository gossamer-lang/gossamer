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
        shared_stores: Vec::new(),
        lent_slots: Vec::new(),
        address_params: Vec::new(),
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
        shared_stores: Vec::new(),
        lent_slots: Vec::new(),
        address_params: Vec::new(),
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
        shared_stores: Vec::new(),
        lent_slots: Vec::new(),
        address_params: Vec::new(),
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
        shared_stores: Vec::new(),
        lent_slots: Vec::new(),
        address_params: Vec::new(),
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
        shared_stores: Vec::new(),
        lent_slots: Vec::new(),
        address_params: Vec::new(),
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
        shared_stores: Vec::new(),
        lent_slots: Vec::new(),
        address_params: Vec::new(),
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
        shared_stores: Vec::new(),
        lent_slots: Vec::new(),
        address_params: Vec::new(),
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

/// A checked `add` tests for overflow with the intrinsic, whose value is
/// the result; the wrapping form is the plain instruction.
#[test]
fn binop_add_i64_emits_add_instruction() {
    let (body, tcx) = build_binop_main(BinOp::Add, 3, 4);
    let ir = render_ir_to_string(&[body], &tcx, false).unwrap();
    assert!(
        ir.contains("@llvm.sadd.with.overflow.i64(i64"),
        "IR was:\n{ir}"
    );
    let (body, tcx) = build_binop_main(BinOp::WrappingAdd, 3, 4);
    let ir = render_ir_to_string(&[body], &tcx, false).unwrap();
    assert!(ir.contains("add i64"), "IR was:\n{ir}");
}

/// A checked `sub` tests for overflow with the intrinsic, whose value is
/// the result; the wrapping form is the plain instruction.
#[test]
fn binop_sub_i64_emits_sub_instruction() {
    let (body, tcx) = build_binop_main(BinOp::Sub, 10, 3);
    let ir = render_ir_to_string(&[body], &tcx, false).unwrap();
    assert!(
        ir.contains("@llvm.ssub.with.overflow.i64(i64"),
        "IR was:\n{ir}"
    );
    let (body, tcx) = build_binop_main(BinOp::WrappingSub, 10, 3);
    let ir = render_ir_to_string(&[body], &tcx, false).unwrap();
    assert!(ir.contains("sub i64"), "IR was:\n{ir}");
}

/// A checked `mul` tests for overflow with the intrinsic, whose value is
/// the result; the wrapping form is the plain instruction.
#[test]
fn binop_mul_i64_emits_mul_instruction() {
    let (body, tcx) = build_binop_main(BinOp::Mul, 6, 7);
    let ir = render_ir_to_string(&[body], &tcx, false).unwrap();
    assert!(
        ir.contains("@llvm.smul.with.overflow.i64(i64"),
        "IR was:\n{ir}"
    );
    let (body, tcx) = build_binop_main(BinOp::WrappingMul, 6, 7);
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
        shared_stores: Vec::new(),
        lent_slots: Vec::new(),
        address_params: Vec::new(),
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
fn fixed_u8_array_keeps_one_slot_per_element() {
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
        shared_stores: Vec::new(),
        lent_slots: Vec::new(),
        address_params: Vec::new(),
    };

    // A byte array is laid out as every tier, descriptor, and runtime walk
    // reads it: one word per element.
    let ir = render_ir_to_string(&[body], &tcx, false).unwrap();
    assert!(ir.contains("alloca [64 x i64]"), "IR was:\n{ir}");
    assert!(!ir.contains("alloca [64 x i8]"), "IR was:\n{ir}");
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
    let (mut resolutions, _) = gossamer_resolve::resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &mut resolutions);
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
        signature, "define i64 @\"total\"(ptr noalias readonly nocapture %p0) #0 {",
        "a flat-slot aggregate keeps the by-pointer convention:\n{ir}"
    );
    assert!(
        body.contains("%l1 = getelementptr inbounds i8, ptr %p0, i64 0")
            && !body.contains("llvm.memcpy"),
        "a parameter the body only reads is read in the caller's storage:\n{body}"
    );
}

#[test]
fn a_parameter_the_body_writes_is_copied_into_the_frame() {
    let (bodies, tcx) = build_from_source(
        r#"
struct Point { x: i64, y: i64 }

fn shifted(mut p: Point) -> i64 {
    p.x = p.x + 5
    p.x + p.y
}

fn main() { println("{}", shifted(Point { x: 1, y: 2 })) }
"#,
    );
    let ir = render_ir_to_string(&bodies, &tcx, false).unwrap();
    let body = body_text(&ir, "define i64 @\"shifted\"(");
    assert!(
        body.contains("llvm.memcpy.p0.p0.i64(ptr %l1, ptr %p0, i64 16"),
        "the entry copies the caller's two slots into this frame:\n{body}"
    );
}

fn build_ffi_put_main(
    helper: &'static str,
    value: ConstValue,
    width: Option<i64>,
) -> (Body, TyCtxt) {
    let mut tcx = TyCtxt::new();
    let i64_ty = tcx.intern(TyKind::Int(IntTy::I64));
    let unit = tcx.unit();
    let local = |ty| LocalDecl {
        ty,
        debug_name: None,
        mutable: false,
        region: false,
    };
    let mut args = vec![Operand::Copy(place(1)), Operand::Const(ConstValue::Int(8))];
    if let Some(width) = width {
        args.push(Operand::Const(ConstValue::Int(i128::from(width))));
    }
    args.push(Operand::Const(value));
    let put = Statement {
        span: dummy_span(),
        inlined: None,
        kind: StatementKind::Assign {
            place: place(2),
            rvalue: Rvalue::CallIntrinsic { name: helper, args },
        },
    };
    let body = Body {
        name: "main".to_string(),
        def: None,
        arity: 0,
        locals: vec![local(i64_ty), local(i64_ty), local(unit)],
        blocks: vec![BasicBlock {
            id: BlockId(0),
            stmts: vec![const_int_assign(1, 4096), put, const_int_assign(0, 0)],
            terminator: Terminator::Return,
            span: dummy_span(),
            terminator_span: None,
            terminator_inlined: None,
        }],
        span: dummy_span(),
        shared_stores: Vec::new(),
        lent_slots: Vec::new(),
        address_params: Vec::new(),
    };
    (body, tcx)
}

#[test]
fn a_foreign_field_write_at_a_constant_offset_is_an_inline_store() {
    let cases = [
        ("gos_rt_ffi_put", ConstValue::Int(7), Some(4), "store i32"),
        (
            "gos_rt_ffi_put_f64",
            ConstValue::Float(1.5f64.to_bits()),
            None,
            "store double",
        ),
        (
            "gos_rt_ffi_put_f32",
            ConstValue::Float(1.5f64.to_bits()),
            None,
            "store float",
        ),
    ];
    for (helper, value, width, store) in cases {
        let (body, tcx) = build_ffi_put_main(helper, value, width);
        let ir = render_ir_to_string(&[body], &tcx, false).unwrap();
        let main = ir
            .split("define ")
            .find(|f| f.contains("@\"gos_main\"("))
            .unwrap_or_else(|| panic!("main is defined:\n{ir}"));
        assert!(main.contains(store), "{helper} stores inline:\n{main}");
        assert!(
            !main.contains(&format!("@{helper}(")),
            "{helper} makes no runtime call:\n{main}"
        );
    }
}

fn build_ffi_scalar_main(
    helper: &'static str,
    class: char,
    value: Option<ConstValue>,
) -> (Body, TyCtxt) {
    let mut tcx = TyCtxt::new();
    let i64_ty = tcx.intern(TyKind::Int(IntTy::I64));
    let dest_ty = match (helper, value.is_some()) {
        (_, true) => tcx.unit(),
        ("gos_rt_ffi_load_float", false) => tcx.intern(TyKind::Float(gossamer_types::FloatTy::F64)),
        _ => i64_ty,
    };
    let local = |ty| LocalDecl {
        ty,
        debug_name: None,
        mutable: false,
        region: false,
    };
    let mut args = vec![
        Operand::Copy(place(1)),
        Operand::Const(ConstValue::Int(i128::from(u32::from(class)))),
    ];
    args.extend(value.map(Operand::Const));
    let call = Statement {
        span: dummy_span(),
        inlined: None,
        kind: StatementKind::Assign {
            place: place(2),
            rvalue: Rvalue::CallIntrinsic { name: helper, args },
        },
    };
    let body = Body {
        name: "main".to_string(),
        def: None,
        arity: 0,
        locals: vec![local(i64_ty), local(i64_ty), local(dest_ty)],
        blocks: vec![BasicBlock {
            id: BlockId(0),
            stmts: vec![const_int_assign(1, 4096), call, const_int_assign(0, 0)],
            terminator: Terminator::Return,
            span: dummy_span(),
            terminator_span: None,
            terminator_inlined: None,
        }],
        span: dummy_span(),
        shared_stores: Vec::new(),
        lent_slots: Vec::new(),
        address_params: Vec::new(),
    };
    (body, tcx)
}

#[test]
fn a_foreign_scalar_of_a_constant_class_is_an_inline_load_or_store() {
    let cases = [
        (
            "gos_rt_ffi_load_int",
            'i',
            None,
            &["load i32", "sext i32"][..],
        ),
        (
            "gos_rt_ffi_load_int",
            'H',
            None,
            &["load i16", "zext i16"][..],
        ),
        (
            "gos_rt_ffi_load_int",
            'B',
            None,
            &["load i8", "icmp ne i8"][..],
        ),
        (
            "gos_rt_ffi_load_float",
            'f',
            None,
            &["load float", "fpext float"][..],
        ),
        (
            "gos_rt_ffi_store_int",
            'h',
            Some(ConstValue::Int(7)),
            &["store i16"][..],
        ),
        (
            "gos_rt_ffi_store_float",
            'd',
            Some(ConstValue::Float(1.5f64.to_bits())),
            &["store double"][..],
        ),
    ];
    for (helper, class, value, shapes) in cases {
        let (body, tcx) = build_ffi_scalar_main(helper, class, value);
        let ir = render_ir_to_string(&[body], &tcx, false).unwrap();
        let main = ir
            .split("define ")
            .find(|f| f.contains("@\"gos_main\"("))
            .unwrap_or_else(|| panic!("main is defined:\n{ir}"));
        for shape in shapes {
            assert!(
                main.contains(shape),
                "{helper} {class} has `{shape}`:\n{main}"
            );
        }
        assert!(
            !main.contains(&format!("@{helper}(")),
            "{helper} makes no runtime call:\n{main}"
        );
    }
}

fn build_unit_intrinsic_main(helper: &'static str, args: Vec<Operand>) -> (Body, TyCtxt) {
    let mut tcx = TyCtxt::new();
    let i64_ty = tcx.intern(TyKind::Int(IntTy::I64));
    let unit = tcx.unit();
    let local = |ty| LocalDecl {
        ty,
        debug_name: None,
        mutable: false,
        region: false,
    };
    let call = Statement {
        span: dummy_span(),
        inlined: None,
        kind: StatementKind::Assign {
            place: place(2),
            rvalue: Rvalue::CallIntrinsic { name: helper, args },
        },
    };
    let body = Body {
        name: "main".to_string(),
        def: None,
        arity: 0,
        locals: vec![local(i64_ty), local(i64_ty), local(unit)],
        blocks: vec![BasicBlock {
            id: BlockId(0),
            stmts: vec![const_int_assign(1, 4096), call, const_int_assign(0, 0)],
            terminator: Terminator::Return,
            span: dummy_span(),
            terminator_span: None,
            terminator_inlined: None,
        }],
        span: dummy_span(),
        shared_stores: Vec::new(),
        lent_slots: Vec::new(),
        address_params: Vec::new(),
    };
    (body, tcx)
}

#[test]
fn native_static_finalizer_is_shared_across_stores_and_functions() {
    let (mut body, mut tcx) = build_binop_main(BinOp::Add, 1, 2);
    let text_ty = tcx.intern(TyKind::String);
    let target = gossamer_mir::StaticRef {
        symbol: "gos_static_cleanup_test".to_string(),
        ty: text_ty,
        init: ConstValue::Int(0),
    };
    let store = |text: &str| Statement {
        span: dummy_span(),
        inlined: None,
        kind: StatementKind::StaticStore {
            target: target.clone(),
            value: Operand::Const(ConstValue::Str(text.to_string())),
        },
    };
    body.blocks[0].stmts.insert(0, store("first"));
    body.blocks[0].stmts.push(store("second"));
    let mut other = body.clone();
    other.name = "replace_static".to_string();
    let ir = render_ir_to_string(&[body, other], &tcx, false).unwrap();
    assert_eq!(
        ir.matches("define linkonce_odr void @gos_static_cleanup_test__drop()")
            .count(),
        1,
        "{ir}"
    );
    assert_eq!(
        ir.matches("call void @gos_rt_register_static_drop(")
            .count(),
        4,
        "{ir}"
    );
    assert!(ir.contains("call void @gos_rt_str_free_typed("), "{ir}");
    assert!(
        ir.contains("store ptr null, ptr @gos_static_cleanup_test"),
        "{ir}"
    );
}

fn gos_main_ir(body: Body, tcx: &TyCtxt) -> String {
    let ir = render_ir_to_string(&[body], tcx, false).unwrap();
    ir.split("define ")
        .find(|f| f.contains("@\"gos_main\"("))
        .unwrap_or_else(|| panic!("main is defined:\n{ir}"))
        .to_string()
}

#[test]
fn a_carrier_release_calls_the_runtime_only_for_an_arm_that_owns_storage() {
    let kinds = |ok: i128, err: i128| {
        vec![
            Operand::Copy(place(1)),
            Operand::Const(ConstValue::Int(ok)),
            Operand::Const(ConstValue::Int(err)),
        ]
    };
    let (body, tcx) = build_unit_intrinsic_main("gos_rt_result_payload_release", kinds(0, 0));
    let main = gos_main_ir(body, &tcx);
    assert!(
        !main.contains("@gos_rt_result_payload_release("),
        "no arm owns storage, so nothing is called:\n{main}"
    );
    let (body, tcx) = build_unit_intrinsic_main("gos_rt_result_payload_release", kinds(0, 4));
    let main = gos_main_ir(body, &tcx);
    assert!(
        main.contains(", 1\n"),
        "only the `Err` arm is tested:\n{main}"
    );
    assert!(main.contains("br i1"), "the call is guarded:\n{main}");
    assert!(main.contains("@gos_rt_result_payload_release("), "{main}");
}

fn local(ty: gossamer_types::Ty) -> LocalDecl {
    LocalDecl {
        ty,
        debug_name: None,
        mutable: false,
        region: false,
    }
}

fn block(id: u32, stmts: Vec<Statement>, terminator: Terminator) -> BasicBlock {
    BasicBlock {
        id: BlockId(id),
        stmts,
        terminator,
        span: dummy_span(),
        terminator_span: None,
        terminator_inlined: None,
    }
}

/// `fill() -> [u8; 32]`, which writes its first byte, and a `main` that
/// binds its answer.
fn build_byte_array_answer() -> (Vec<Body>, TyCtxt) {
    let mut tcx = TyCtxt::new();
    let u8_ty = tcx.intern(TyKind::Int(IntTy::U8));
    let unit = tcx.intern(TyKind::Unit);
    let bytes = tcx.intern(TyKind::Array {
        elem: u8_ty,
        len: ArrayLen::Concrete(32),
    });
    let def = DefId::local(7);
    let first = Place {
        local: Local(0),
        projection: vec![gossamer_mir::Projection::Index(Local(1))],
    };
    let i64_ty = tcx.intern(TyKind::Int(IntTy::I64));
    let fill = Body {
        name: "fill".to_string(),
        def: Some(def),
        arity: 0,
        locals: vec![local(bytes), local(i64_ty)],
        blocks: vec![block(
            0,
            vec![
                const_int_assign(1, 0),
                Statement {
                    span: dummy_span(),
                    inlined: None,
                    kind: StatementKind::Assign {
                        place: first,
                        rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(7))),
                    },
                },
            ],
            Terminator::Return,
        )],
        span: dummy_span(),
        shared_stores: Vec::new(),
        lent_slots: Vec::new(),
        address_params: Vec::new(),
    };
    let main = Body {
        name: "main".to_string(),
        def: None,
        arity: 0,
        locals: vec![local(unit), local(bytes)],
        blocks: vec![
            block(
                0,
                Vec::new(),
                Terminator::Call {
                    callee: Operand::FnRef {
                        def,
                        substs: gossamer_types::Substs::new(),
                    },
                    args: Vec::new(),
                    destination: place(1),
                    target: Some(BlockId(1)),
                },
            ),
            block(1, Vec::new(), Terminator::Return),
        ],
        span: dummy_span(),
        shared_stores: Vec::new(),
        lent_slots: Vec::new(),
        address_params: Vec::new(),
    };
    (vec![fill, main], tcx)
}

#[test]
fn a_body_answering_through_caller_storage_builds_its_answer_there() {
    let (bodies, tcx) = build_byte_array_answer();
    let ir = render_ir_to_string(&bodies, &tcx, false).expect("render");
    let fill = ir
        .split("define ")
        .find(|f| f.contains("@\"fill\"("))
        .unwrap_or_else(|| panic!("fill is defined:\n{ir}"));
    assert!(fill.contains("ptr noalias nocapture %sret"), "{fill}");
    assert!(
        !fill.contains("%l0 = alloca"),
        "the answer has no slot of its own:\n{fill}"
    );
    assert!(
        !fill.contains("@llvm.memcpy"),
        "nothing is copied into the caller's storage:\n{fill}"
    );
    let main = gos_main_ir_of(&ir);
    assert!(
        !main.contains("i64 256"),
        "a [u8; 32] answer occupies 32 bytes:\n{main}"
    );
}

fn gos_main_ir_of(ir: &str) -> String {
    ir.split("define ")
        .find(|f| f.contains("@\"gos_main\"("))
        .unwrap_or_else(|| panic!("main is defined:\n{ir}"))
        .to_string()
}

/// `first(t: (i64, i64, i64), u: (i64, i64, i64)) -> i64` and a `main` that
/// calls it with `args`.
fn build_triple_param_call(args: Vec<Operand>) -> (Vec<Body>, TyCtxt) {
    let mut tcx = TyCtxt::new();
    let i64_ty = tcx.intern(TyKind::Int(IntTy::I64));
    let unit = tcx.intern(TyKind::Unit);
    let triple = tcx.intern(TyKind::Tuple(vec![i64_ty, i64_ty, i64_ty]));
    let def = DefId::local(9);
    let first = Body {
        name: "first".to_string(),
        def: Some(def),
        arity: 2,
        locals: vec![local(i64_ty), local(triple), local(triple)],
        blocks: vec![block(
            0,
            vec![Statement {
                span: dummy_span(),
                inlined: None,
                kind: StatementKind::Assign {
                    place: place(0),
                    rvalue: Rvalue::Use(Operand::Copy(Place {
                        local: Local(1),
                        projection: vec![gossamer_mir::Projection::Field(0)],
                    })),
                },
            }],
            Terminator::Return,
        )],
        span: dummy_span(),
        shared_stores: Vec::new(),
        lent_slots: Vec::new(),
        address_params: Vec::new(),
    };
    let main = Body {
        name: "main".to_string(),
        def: None,
        arity: 0,
        locals: vec![local(unit), local(triple), local(i64_ty), local(triple)],
        blocks: vec![
            block(
                0,
                Vec::new(),
                Terminator::Call {
                    callee: Operand::FnRef {
                        def,
                        substs: gossamer_types::Substs::new(),
                    },
                    args,
                    destination: place(2),
                    target: Some(BlockId(1)),
                },
            ),
            block(1, Vec::new(), Terminator::Return),
        ],
        span: dummy_span(),
        shared_stores: Vec::new(),
        lent_slots: Vec::new(),
        address_params: Vec::new(),
    };
    (vec![first, main], tcx)
}

#[test]
fn a_direct_callee_reads_its_aggregate_parameters_in_the_callers_storage() {
    let (bodies, tcx) =
        build_triple_param_call(vec![Operand::Copy(place(1)), Operand::Copy(place(3))]);
    let ir = render_ir_to_string(&bodies, &tcx, false).expect("render");
    let first = ir
        .split("define ")
        .find(|f| f.contains("@\"first\"("))
        .unwrap_or_else(|| panic!("first is defined:\n{ir}"));
    assert!(
        first.contains("ptr noalias readonly nocapture %p0"),
        "{first}"
    );
    let main = gos_main_ir_of(&ir);
    assert!(
        !main.contains("@llvm.memcpy"),
        "distinct bare locals are passed in place:\n{main}"
    );
}

#[test]
fn an_aggregate_passed_twice_reaches_the_callee_as_two_copies() {
    let (bodies, tcx) =
        build_triple_param_call(vec![Operand::Copy(place(1)), Operand::Copy(place(1))]);
    let ir = render_ir_to_string(&bodies, &tcx, false).expect("render");
    let main = gos_main_ir_of(&ir);
    assert_eq!(
        main.matches("@llvm.memcpy").count(),
        2,
        "each `noalias` parameter gets storage of its own:\n{main}"
    );
}

#[test]
fn boxed_enum_payload_clones_containers_in_the_destination() {
    let (bodies, tcx) = build_from_source(
        r#"
enum Page { Empty, Tabs(Tabs), Child(Page) }
struct Meta { label: String, labels: Vec<String> }
struct Tabs { id: String, pages: Map<String, Page>, values: Set<String>, meta: Option<Meta> }
impl Page {
    fn id(&self) -> String { match self { Page::Tabs(t) => t.id, _ => "" } }
}
fn main() {
    let p = Page::Tabs(Tabs { id: "tabs", pages: Map::new(), values: Set::new(), meta: Some(Meta { label: "meta", labels: #["kept"] }) })
    println(p.id())
}
"#,
    );
    let ir = render_ir_to_string(&bodies, &tcx, false).unwrap();
    let body = body_text(&ir, "define ptr @\"Page::id\"(");
    assert!(body.contains("@gos_rt_map_field_clone"), "{body}");
    assert!(body.contains("@gos_rt_set_field_clone"), "{body}");
    assert!(body.contains("@gos_rt_aggr_retain_children"), "{body}");
    assert!(!body.contains("@gos_rt_rc_retain_children"), "{body}");
}
