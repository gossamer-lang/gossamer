//! End-to-end test that the cranelift-jit backend produces native
//! code we can call through a function pointer in-process. The
//! body shapes here parallel the smallest cases the bytecode VM
//! hands to the JIT trampoline.

#![allow(missing_docs)]
#![allow(unsafe_code)]

use std::mem;

use gossamer_codegen_cranelift::compile_to_jit;
use gossamer_lex::SourceMap;
use gossamer_mir::{
    BasicBlock, BinOp, Body, ConstValue, LocalDecl, Operand, Place, Projection, Rvalue, Statement,
    StatementKind, Terminator,
};
use gossamer_types::{FloatTy, IntTy, TyCtxt};

fn dummy_span() -> gossamer_lex::Span {
    let mut map = SourceMap::new();
    let file = map.add_file("jit.gos", "");
    gossamer_lex::Span::new(file, 0, 0)
}

fn place(local: u32) -> Place {
    Place {
        local: gossamer_mir::Local(local),
        projection: Vec::<Projection>::new(),
    }
}

#[test]
fn jit_compiles_const_int_returning_body() {
    // fn compute() -> i64 { 42 }
    // `main` is deliberately never JIT-promoted (the VM keeps it on bytecode),
    // so a named helper stands in as the compiled root here.
    let mut tcx = TyCtxt::new();
    let i64_ty = tcx.int_ty(IntTy::I64);
    let body = Body {
        name: "compute".to_string(),
        def: None,
        arity: 0,
        locals: vec![LocalDecl {
            ty: i64_ty,
            debug_name: None,
            mutable: false,
            region: false,
        }],
        blocks: vec![BasicBlock {
            id: gossamer_mir::BlockId(0),
            stmts: vec![Statement {
                span: dummy_span(),
                inlined: None,
                kind: StatementKind::Assign {
                    place: place(0),
                    rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(42))),
                },
            }],
            terminator: Terminator::Return,
            span: dummy_span(),
            terminator_span: None,
            terminator_inlined: None,
        }],
        span: dummy_span(),
    };
    let artifact = compile_to_jit(
        &[body],
        &tcx,
        &std::collections::HashMap::new(),
        &std::collections::HashMap::new(),
    )
    .expect("compile");
    let compute_fn = artifact.functions.get("compute").expect("compute present");
    // SAFETY: the test only invokes `compute_fn` while `artifact` is
    // live, matching the trampoline's lifetime contract.
    let result: i64 = unsafe {
        let f: extern "C" fn() -> i64 = mem::transmute(compute_fn.ptr);
        f()
    };
    assert_eq!(result, 42);
}

#[test]
fn jit_compiles_simple_arithmetic_function() {
    // fn add(a: i64, b: i64) -> i64 { a + b }
    let mut tcx = TyCtxt::new();
    let i64_ty = tcx.int_ty(IntTy::I64);
    let body = Body {
        name: "add".to_string(),
        def: None,
        arity: 2,
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
            id: gossamer_mir::BlockId(0),
            stmts: vec![Statement {
                span: dummy_span(),
                kind: StatementKind::Assign {
                    place: place(0),
                    rvalue: Rvalue::BinaryOp {
                        op: BinOp::Add,
                        lhs: Operand::Copy(place(1)),
                        rhs: Operand::Copy(place(2)),
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
    let artifact = compile_to_jit(
        &[body],
        &tcx,
        &std::collections::HashMap::new(),
        &std::collections::HashMap::new(),
    )
    .expect("compile");
    let add_fn = artifact.functions.get("add").expect("add present");
    let result: i64 = unsafe {
        let f: extern "C" fn(i64, i64) -> i64 = mem::transmute(add_fn.ptr);
        f(7, 35)
    };
    assert_eq!(result, 42);
}

#[test]
fn detached_jit_preserves_float_abi() {
    let mut tcx = TyCtxt::new();
    let f64_ty = tcx.float_ty(FloatTy::F64);
    let body = Body {
        name: "add_f64".to_string(),
        def: None,
        arity: 2,
        locals: vec![
            LocalDecl {
                ty: f64_ty,
                debug_name: None,
                mutable: false,
                region: false,
            },
            LocalDecl {
                ty: f64_ty,
                debug_name: None,
                mutable: false,
                region: false,
            },
            LocalDecl {
                ty: f64_ty,
                debug_name: None,
                mutable: false,
                region: false,
            },
        ],
        blocks: vec![BasicBlock {
            id: gossamer_mir::BlockId(0),
            stmts: vec![Statement {
                span: dummy_span(),
                kind: StatementKind::Assign {
                    place: place(0),
                    rvalue: Rvalue::BinaryOp {
                        op: BinOp::Add,
                        lhs: Operand::Copy(place(1)),
                        rhs: Operand::Copy(place(2)),
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
    let artifact = compile_to_jit(
        &[body],
        &tcx,
        &std::collections::HashMap::new(),
        &std::collections::HashMap::new(),
    )
    .expect("compile f64 body");
    assert!(artifact.is_detached());
    let ptr = artifact.functions.get("add_f64").expect("f64 entry").ptr;
    // SAFETY: MIR and the cast both use the platform C ABI with two f64
    // parameters and one f64 result. The artifact owns the code during call.
    let result =
        unsafe { mem::transmute::<*const u8, extern "C" fn(f64, f64) -> f64>(ptr)(1.25, 2.5) };
    assert_eq!(result, 3.75);
}

fn i64_decl(ty: gossamer_types::Ty) -> LocalDecl {
    LocalDecl {
        ty,
        debug_name: None,
        mutable: false,
        region: false,
    }
}

#[test]
fn jit_unresolved_qualified_call_aborts_compile_not_zero_stub() {
    // fn f(x: i64) -> i64 { Foo::bar(x) }
    //
    // `Foo::bar` has no cranelift lowering (not an intrinsic, runtime
    // symbol, or user body). The backend must refuse to compile the
    // body so the VM (which resolves the call correctly) runs it,
    // rather than silently lowering the call to a zero constant and
    // returning garbage.
    let mut tcx = TyCtxt::new();
    let i64_ty = tcx.int_ty(IntTy::I64);
    let body = Body {
        name: "f".to_string(),
        def: None,
        arity: 1,
        locals: vec![i64_decl(i64_ty), i64_decl(i64_ty), i64_decl(i64_ty)],
        blocks: vec![
            BasicBlock {
                id: gossamer_mir::BlockId(0),
                stmts: vec![],
                terminator: Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("Foo::bar".to_string())),
                    args: vec![Operand::Copy(place(1))],
                    destination: place(2),
                    target: Some(gossamer_mir::BlockId(1)),
                },
                span: dummy_span(),
                terminator_span: None,
                terminator_inlined: None,
            },
            BasicBlock {
                id: gossamer_mir::BlockId(1),
                stmts: vec![Statement {
                    span: dummy_span(),
                    kind: StatementKind::Assign {
                        place: place(0),
                        rvalue: Rvalue::Use(Operand::Copy(place(2))),
                    },
                    inlined: None,
                }],
                terminator: Terminator::Return,
                span: dummy_span(),
                terminator_span: None,
                terminator_inlined: None,
            },
        ],
        span: dummy_span(),
    };
    let result = compile_to_jit(
        &[body],
        &tcx,
        &std::collections::HashMap::new(),
        &std::collections::HashMap::new(),
    );
    assert!(
        result.is_err(),
        "compile_to_jit must refuse an unresolved qualified call instead of \
         emitting a silent zero-stub"
    );
}

#[test]
fn jit_some_constructor_still_compiles_as_identity() {
    // fn f(x: i64) -> i64 { Some(x) }
    //
    // The BUG 3 fix must not over-refuse: `Ok` / `Some` / `Err` with a
    // payload lower to an identity pass-through and must still compile.
    let mut tcx = TyCtxt::new();
    let i64_ty = tcx.int_ty(IntTy::I64);
    let body = Body {
        name: "f".to_string(),
        def: None,
        arity: 1,
        locals: vec![i64_decl(i64_ty), i64_decl(i64_ty), i64_decl(i64_ty)],
        blocks: vec![
            BasicBlock {
                id: gossamer_mir::BlockId(0),
                stmts: vec![],
                terminator: Terminator::Call {
                    callee: Operand::Const(ConstValue::Str("Some".to_string())),
                    args: vec![Operand::Copy(place(1))],
                    destination: place(2),
                    target: Some(gossamer_mir::BlockId(1)),
                },
                span: dummy_span(),
                terminator_span: None,
                terminator_inlined: None,
            },
            BasicBlock {
                id: gossamer_mir::BlockId(1),
                stmts: vec![Statement {
                    span: dummy_span(),
                    kind: StatementKind::Assign {
                        place: place(0),
                        rvalue: Rvalue::Use(Operand::Copy(place(2))),
                    },
                    inlined: None,
                }],
                terminator: Terminator::Return,
                span: dummy_span(),
                terminator_span: None,
                terminator_inlined: None,
            },
        ],
        span: dummy_span(),
    };
    let artifact = compile_to_jit(
        &[body],
        &tcx,
        &std::collections::HashMap::new(),
        &std::collections::HashMap::new(),
    )
    .expect("Some(x) lowers to identity and must still compile");
    assert!(
        artifact.is_detached(),
        "the compiler module must be dropped before entries are callable"
    );
    let f = artifact.functions.get("f").expect("f present");
    // SAFETY: `f` is live for the duration of `artifact`.
    let result: i64 = unsafe {
        let g: extern "C" fn(i64) -> i64 = mem::transmute(f.ptr);
        g(7)
    };
    assert_eq!(result, 7, "Some(x) must pass its payload through unchanged");
}

#[test]
fn jit_artifact_drops_without_panic() {
    let mut tcx = TyCtxt::new();
    let i64_ty = tcx.int_ty(IntTy::I64);
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
            id: gossamer_mir::BlockId(0),
            stmts: vec![Statement {
                span: dummy_span(),
                kind: StatementKind::Assign {
                    place: place(0),
                    rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(0))),
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
    let artifact = compile_to_jit(
        &[body],
        &tcx,
        &std::collections::HashMap::new(),
        &std::collections::HashMap::new(),
    )
    .expect("compile");
    assert!(artifact.is_detached());
    drop(artifact);
}

#[test]
fn detached_artifacts_can_be_repeatedly_called_and_reclaimed() {
    let mut tcx = TyCtxt::new();
    let i64_ty = tcx.int_ty(IntTy::I64);
    let body = Body {
        name: "answer".to_string(),
        def: None,
        arity: 0,
        locals: vec![LocalDecl {
            ty: i64_ty,
            debug_name: None,
            mutable: false,
            region: false,
        }],
        blocks: vec![BasicBlock {
            id: gossamer_mir::BlockId(0),
            stmts: vec![Statement {
                span: dummy_span(),
                kind: StatementKind::Assign {
                    place: place(0),
                    rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(42))),
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

    for _ in 0..32 {
        let artifact = compile_to_jit(
            std::slice::from_ref(&body),
            &tcx,
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        )
        .expect("compile detached artifact");
        assert!(artifact.is_detached());
        let ptr = artifact.functions.get("answer").expect("answer entry").ptr;
        // SAFETY: the signature is derived from the scalar MIR body and the
        // owning artifact remains alive across this call.
        let result = unsafe { mem::transmute::<*const u8, extern "C" fn() -> i64>(ptr)() };
        assert_eq!(result, 42);
        drop(artifact);
    }
}
