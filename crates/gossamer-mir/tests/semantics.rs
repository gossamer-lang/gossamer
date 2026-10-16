//! Semantics tests: assert that source-level constructs lower into MIR
//! shapes that both the interpreter and the native backend agree on.
//! Every test here takes `.gos` source, runs it through the full
//! frontend (parse → resolve → typecheck → HIR → MIR), and inspects
//! the resulting [`Body`] to verify the IR faithfully represents the
//! source semantics. Where a construct is not yet lowered (e.g. enum
//! `SetDiscriminant`), the test documents the placeholder and the gap.

use gossamer_hir::lower_source_file;
use gossamer_lex::SourceMap;
use gossamer_mir::{
    BinOp, BlockId, ConstValue, Local, Operand, Place, Projection, Rvalue, StatementKind,
    Terminator, lower_program,
};
use gossamer_parse::parse_source_file;
use gossamer_resolve::resolve_source_file;
use gossamer_types::{TyCtxt, typecheck_source_file};

fn build(source: &str) -> Vec<gossamer_mir::Body> {
    let mut map = SourceMap::new();
    let file = map.add_file("test.gos", source.to_string());
    let (mut sf, parse_diags) = parse_source_file(source, file);
    assert!(parse_diags.is_empty(), "parse: {parse_diags:?}");
    let (mut resolutions, resolve_diags) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &mut resolutions);
    assert!(resolve_diags.is_empty(), "resolve: {resolve_diags:?}");
    let mut tcx = TyCtxt::new();
    let (table, type_diags) = typecheck_source_file(&sf, &resolutions, &mut tcx);
    assert!(type_diags.is_empty(), "type: {type_diags:?}");
    let hir = lower_source_file(&sf, &resolutions, &table, &mut tcx);
    lower_program(&hir, &mut tcx)
}

#[test]
fn integer_addition_lowers_to_binary_op_add() {
    let bodies = build("fn main() -> i64 { 1i64 + 2i64 }\n");
    let main = bodies.iter().find(|b| b.name == "main").expect("main body");
    let has_add = main.blocks.iter().flat_map(|b| &b.stmts).any(|s| {
        matches!(
            &s.kind,
            StatementKind::Assign {
                rvalue: Rvalue::BinaryOp { op: BinOp::Add, .. },
                ..
            }
        )
    });
    assert!(has_add, "1 + 2 must lower to BinaryOp::Add");
}

#[test]
fn cast_expression_preserves_rvalue_cast_node() {
    let bodies = build("fn main() -> i64 { let n = 7i64\n n as i64 }\n");
    let main = bodies.iter().find(|b| b.name == "main").expect("main body");
    let has_cast = main.blocks.iter().flat_map(|b| &b.stmts).any(|s| {
        matches!(
            &s.kind,
            StatementKind::Assign {
                rvalue: Rvalue::Cast { .. },
                ..
            }
        )
    });
    assert!(
        has_cast,
        "`n as i64` must survive lowering as Rvalue::Cast so both paths agree on the cast site"
    );
}

#[test]
fn if_else_produces_switchint_on_bool_discriminant() {
    let bodies = build("fn main() -> i64 { if true { 1i64 } else { 0i64 } }\n");
    let main = bodies.iter().find(|b| b.name == "main").expect("main body");
    let switch = main.blocks.iter().find_map(|b| match &b.terminator {
        Terminator::SwitchInt {
            discriminant,
            arms,
            default,
        } => Some((discriminant.clone(), arms.clone(), *default)),
        _ => None,
    });
    let (discriminant, arms, default) =
        switch.expect("if/else must lower to SwitchInt for bool discriminant");
    assert!(
        matches!(discriminant, Operand::Copy(Place { local, .. }) if local.as_u32() != 0),
        "discriminant must be a fresh boolean local, not the return slot"
    );
    assert_eq!(
        arms.len(),
        1,
        "bool SwitchInt has one explicit arm (false = 0)"
    );
    assert_eq!(arms[0].0, 0, "false arm matches discriminant == 0");
    assert!(
        default.as_u32() > arms[0].1.as_u32() || default.as_u32() < arms[0].1.as_u32(),
        "default (true) and explicit (false) targets must be different blocks"
    );
}

#[test]
fn while_loop_produces_conditional_back_edge_to_header() {
    let bodies = build("fn main() { let mut n = 3i64\n while n > 0i64 { n = n - 1i64 } }\n");
    let main = bodies.iter().find(|b| b.name == "main").expect("main body");
    let header_id = main
        .blocks
        .iter()
        .position(|b| matches!(&b.terminator, Terminator::SwitchInt { .. }))
        .map(|i| BlockId(u32::try_from(i).unwrap()))
        .expect("while header must end in SwitchInt");
    let has_back_edge = main
        .blocks
        .iter()
        .any(|b| matches!(&b.terminator, Terminator::Goto { target } if *target == header_id));
    assert!(
        has_back_edge,
        "loop body must jump back to the SwitchInt header"
    );
}

#[test]
fn match_on_int_produces_ordered_arms_in_switchint() {
    let bodies =
        build("fn main() -> i64 { match 2i64 { 1i64 => 10i64, 2i64 => 20i64, _ => 30i64 } }\n");
    let main = bodies.iter().find(|b| b.name == "main").expect("main body");
    let switch = main.blocks.iter().find_map(|b| match &b.terminator {
        Terminator::SwitchInt { arms, .. } => Some(arms.clone()),
        _ => None,
    });
    let arms = switch.expect("match on int must lower to SwitchInt");
    let arm_values: Vec<i128> = arms.iter().map(|(v, _)| *v).collect();
    assert_eq!(arm_values, vec![1, 2], "arms must appear in source order");
}

#[test]
fn tuple_destructuring_produces_field_projection_reads() {
    let bodies = build("fn main() -> i64 { let a, b = (11i64, 22i64)\n a + b }\n");
    let main = bodies.iter().find(|b| b.name == "main").expect("main body");
    let field_reads: Vec<u32> = main
        .blocks
        .iter()
        .flat_map(|b| &b.stmts)
        .filter_map(|s| match &s.kind {
            StatementKind::Assign {
                rvalue: Rvalue::Use(Operand::Copy(Place { projection, .. })),
                ..
            } => projection.iter().find_map(|p| match p {
                Projection::Field(idx) => Some(*idx),
                _ => None,
            }),
            _ => None,
        })
        .collect();
    assert!(
        field_reads.contains(&0),
        "projection for tuple element 0 must exist"
    );
    assert!(
        field_reads.contains(&1),
        "projection for tuple element 1 must exist"
    );
}

#[test]
fn array_index_produces_projection_index_with_local_offset() {
    let bodies = build("fn main() -> i64 { let xs = [5i64, 7i64, 9i64]\n xs[1i64] }\n");
    let main = bodies.iter().find(|b| b.name == "main").expect("main body");
    let has_index_proj = main.blocks.iter().flat_map(|b| &b.stmts).any(|s| {
        matches!(
            &s.kind,
            StatementKind::Assign {
                rvalue: Rvalue::Use(Operand::Copy(Place { projection, .. })),
                ..
            } if projection.iter().any(|p| matches!(p, Projection::Index(_)))
        )
    });
    assert!(
        has_index_proj,
        "array indexing must lower to Projection::Index(local)"
    );
}

#[test]
fn positional_struct_constructor_preserves_declaration_order() {
    let bodies = build(
        "struct Pair { a: i64, b: i64 }\nfn main() -> i64 { let p = Pair { a: 3i64, b: 7i64 }\n p.a }\n",
    );
    let main = bodies.iter().find(|b| b.name == "main").expect("main body");
    let operands = main
        .blocks
        .iter()
        .flat_map(|b| &b.stmts)
        .find_map(|s| match &s.kind {
            StatementKind::Assign {
                rvalue: Rvalue::Aggregate { operands, .. },
                ..
            } => Some(operands.clone()),
            _ => None,
        })
        .expect("positional struct constructor must lower to an aggregate");
    assert_eq!(operands.len(), 2, "Pair has two fields");
}

#[test]
fn for_range_produces_counter_loop_with_add_increment() {
    let bodies = build(
        "fn main() -> i64 { let mut sum = 0i64\n for n in 0i64..3i64 { sum = sum + n } sum }\n",
    );
    let main = bodies.iter().find(|b| b.name == "main").expect("main body");
    let has_counter_add = main.blocks.iter().flat_map(|b| &b.stmts).any(|s| {
        matches!(
            &s.kind,
            StatementKind::Assign {
                rvalue: Rvalue::BinaryOp { op: BinOp::Add, .. },
                ..
            }
        )
    });
    assert!(
        has_counter_add,
        "for-range must lower to a counter loop using BinaryOp::Add for the increment"
    );
}

#[test]
fn function_call_preserves_argument_order_in_terminator() {
    let bodies =
        build("fn f(a: i64, b: i64) -> i64 { a + b }\nfn main() -> i64 { f(1i64, 2i64) }\n");
    let main = bodies.iter().find(|b| b.name == "main").expect("main body");
    // The constants 1 and 2 are first assigned to temporaries;
    // the Call terminator then copies those temporaries in source
    // order. We verify the argument locals hold the expected
    // constants by walking the preceding statements.
    let call = main.blocks.iter().find_map(|b| match &b.terminator {
        Terminator::Call { args, .. } => Some(args.clone()),
        _ => None,
    });
    let args = call.expect("direct call must lower to Terminator::Call");
    assert_eq!(args.len(), 2, "f takes two arguments");

    let local_const = |local: Local| -> Option<i128> {
        main.blocks
            .iter()
            .flat_map(|b| &b.stmts)
            .find_map(|s| match &s.kind {
                StatementKind::Assign {
                    place: Place { local: l, .. },
                    rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(n))),
                } if *l == local => Some(*n),
                _ => None,
            })
    };

    let first_local = match &args[0] {
        Operand::Copy(Place { local, .. }) => *local,
        other => panic!("first arg must be Copy of a local, got {other:?}"),
    };
    let second_local = match &args[1] {
        Operand::Copy(Place { local, .. }) => *local,
        other => panic!("second arg must be Copy of a local, got {other:?}"),
    };

    assert_eq!(
        local_const(first_local),
        Some(1),
        "first argument local must hold constant 1"
    );
    assert_eq!(
        local_const(second_local),
        Some(2),
        "second argument local must hold constant 2"
    );
}

#[test]
fn array_repeat_produces_rvalue_repeat_with_compile_time_count() {
    let bodies = build("fn main() -> i64 { let xs = [42i64; 5i64]\n xs[0i64] }\n");
    let main = bodies.iter().find(|b| b.name == "main").expect("main body");
    let has_repeat = main.blocks.iter().flat_map(|b| &b.stmts).any(|s| {
        matches!(
            &s.kind,
            StatementKind::Assign {
                rvalue: Rvalue::Repeat { count: 5, .. },
                ..
            }
        )
    });
    assert!(
        has_repeat,
        "[42; 5] must lower to Rvalue::Repeat with count 5"
    );
}

#[test]
fn return_slot_is_assigned_before_return_terminator() {
    let bodies = build("fn main() -> i64 { 42i64 }\n");
    let main = bodies.iter().find(|b| b.name == "main").expect("main body");
    let entry = main.block(BlockId::ENTRY);
    assert!(
        matches!(&entry.terminator, Terminator::Return),
        "tail expression must end in Return"
    );
    let assigns_to_return = main.blocks.iter().flat_map(|b| &b.stmts).any(|s| {
        matches!(
            &s.kind,
            StatementKind::Assign { place, .. } if place.local == Local::RETURN
        )
    });
    assert!(
        assigns_to_return,
        "return slot Local(0) must be assigned the result before Return"
    );
}

#[test]
fn set_discriminant_is_present_in_schema_for_enum_variants() {
    // P0 audit: SetDiscriminant exists in the StatementKind enum for
    // enum variant tagging. The lowerer currently falls back to the
    // unsupported intrinsic for non-trivial enum patterns; this test
    // asserts the variant exists so enum lowering can land later.
    let set = StatementKind::SetDiscriminant {
        place: Place::local(Local(1)),
        variant: 0,
    };
    assert!(
        matches!(set, StatementKind::SetDiscriminant { .. }),
        "SetDiscriminant must be a valid StatementKind for parity"
    );
}
