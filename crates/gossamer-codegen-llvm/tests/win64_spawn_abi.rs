//! Win64 spawn ABI gate, runnable on any host.
//!
//! The runtime invokes a spawned callable as `extern "C" fn(..) -> i128`
//! and reads the value from xmm0, while a gossamer callable returns the
//! two words in the GP-register pair. This suite renders the `spawn(f)`
//! lowering under the Windows triple and pins the forwarding thunk that
//! bridges the two. It owns its own test binary because the triple
//! override is process-wide and set once.

#![allow(missing_docs)]

use gossamer_codegen_llvm::{render_ir_to_string, set_target_triple};
use gossamer_lex::{SourceMap, Span};
use gossamer_mir::{
    BasicBlock, BlockId, Body, ConstValue, Local, LocalDecl, Operand, Place, Rvalue, Statement,
    StatementKind, Terminator,
};
use gossamer_types::{IntTy, TyCtxt, TyKind};

/// A `main` that builds a closure env, reads the callable at slot 0, and
/// hands both to `gos_rt_spawn_ex` with the given return width - the
/// shape `lower_spawn` emits.
fn spawn_main(ret_words: i64) -> (Body, TyCtxt) {
    let mut map = SourceMap::new();
    let span = Span::new(map.add_file("spawn.gos", ""), 0, 0);
    let mut tcx = TyCtxt::new();
    let i64_ty = tcx.int_ty(IntTy::I64);
    let unit_ty = tcx.intern(TyKind::Unit);

    let assign = |local: u32, rvalue: Rvalue| Statement {
        kind: StatementKind::Assign {
            place: Place {
                local: Local(local),
                projection: Vec::new(),
            },
            rvalue,
        },
        span,
        inlined: None,
    };
    let copy = |local: u32| {
        Operand::Copy(Place {
            local: Local(local),
            projection: Vec::new(),
        })
    };

    let body = Body {
        name: "main".to_string(),
        def: None,
        arity: 0,
        locals: std::iter::once(unit_ty)
            .chain(std::iter::repeat_n(i64_ty, 7))
            .map(|ty| LocalDecl {
                ty,
                debug_name: None,
                mutable: false,
                region: false,
            })
            .collect(),
        blocks: vec![
            BasicBlock {
                id: BlockId(0),
                stmts: vec![
                    assign(
                        1,
                        Rvalue::CallIntrinsic {
                            name: "gos_alloc",
                            args: vec![Operand::Const(ConstValue::Int(16))],
                        },
                    ),
                    assign(
                        2,
                        Rvalue::CallIntrinsic {
                            name: "gos_fn_addr",
                            args: vec![Operand::Const(ConstValue::Str(
                                "__fn_thunk__r".to_string(),
                            ))],
                        },
                    ),
                    assign(
                        3,
                        Rvalue::CallIntrinsic {
                            name: "gos_store",
                            args: vec![copy(1), Operand::Const(ConstValue::Int(0)), copy(2)],
                        },
                    ),
                    assign(
                        4,
                        Rvalue::CallIntrinsic {
                            name: "gos_load",
                            args: vec![copy(1), Operand::Const(ConstValue::Int(0))],
                        },
                    ),
                    assign(
                        5,
                        Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(ret_words)))),
                    ),
                    assign(6, Rvalue::Use(Operand::Const(ConstValue::Int(0)))),
                ],
                terminator: Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("gos_rt_spawn_ex".to_string())),
                    args: vec![copy(4), copy(1), copy(5), copy(6)],
                    destination: Place {
                        local: Local(7),
                        projection: Vec::new(),
                    },
                    target: Some(BlockId(1)),
                },
                span,
                terminator_span: None,
                terminator_inlined: None,
            },
            BasicBlock {
                id: BlockId(1),
                stmts: Vec::new(),
                terminator: Terminator::Return,
                span,
                terminator_span: None,
                terminator_inlined: None,
            },
        ],
        span,
    };
    (body, tcx)
}

#[test]
fn win64_spawn_of_a_two_word_callable_goes_through_the_vector_return_thunk() {
    set_target_triple("x86_64-pc-windows-msvc".to_string());

    let (wide, tcx) = spawn_main(2);
    let ir = render_ir_to_string(&[wide], &tcx, false).expect("render wide spawn");
    assert!(
        ir.contains(r#"define linkonce_odr <16 x i8> @"__gos_spawn_wide$cabi"(ptr %env)"#),
        "the forwarding thunk must be defined, IR was:\n{ir}"
    );
    assert!(
        ir.contains(r#"ptrtoint ptr @"__gos_spawn_wide$cabi" to i64"#),
        "the spawn call must take the thunk's address, IR was:\n{ir}"
    );

    let (narrow, tcx) = spawn_main(1);
    let ir = render_ir_to_string(&[narrow], &tcx, false).expect("render one-word spawn");
    assert!(
        !ir.contains("__gos_spawn_wide$cabi"),
        "a one-word callable returns in the GP register already, IR was:\n{ir}"
    );
}
