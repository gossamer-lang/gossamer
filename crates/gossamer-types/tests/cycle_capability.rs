//! Which types the cycle collector has to consider: a value can be part of a
//! reference cycle only when its type graph leads back to it through a
//! reference the program can write in place, or reaches a type whose contents
//! it does not state. A write into a shared value copies it first, so values
//! alone build trees.

#![allow(missing_docs)]

use gossamer_resolve::DefId;
use gossamer_types::{IntTy, Substs, Ty, TyCtxt, TyKind};

fn adt(tcx: &mut TyCtxt, local: u32) -> Ty {
    tcx.intern(TyKind::Adt {
        def: DefId::local(local),
        substs: Substs::new(),
    })
}

fn option(tcx: &mut TyCtxt, payload: Ty) -> Ty {
    tcx.intern(TyKind::Adt {
        def: DefId::local(u32::MAX - 1),
        substs: Substs::from_types([payload]),
    })
}

#[test]
fn a_recursive_enum_builds_trees_only() {
    let mut tcx = TyCtxt::new();
    let i64_ty = tcx.int_ty(IntTy::I64);
    let expr = adt(&mut tcx, 1);
    tcx.register_enum_variant_tys(
        DefId::local(1),
        vec![vec![i64_ty], vec![expr, expr], vec![]],
    );
    assert!(!tcx.can_join_cycle(expr));
}

#[test]
fn a_struct_of_scalars_and_strings_cannot_join_a_cycle() {
    let mut tcx = TyCtxt::new();
    let i64_ty = tcx.int_ty(IntTy::I64);
    let string = tcx.string_ty();
    let names = tcx.intern(TyKind::Vec(string));
    let point = adt(&mut tcx, 2);
    tcx.register_struct_fields(DefId::local(2), vec![i64_ty, string, names]);
    assert!(!tcx.can_join_cycle(point));
}

#[test]
fn a_struct_that_reaches_itself_through_a_field_builds_trees_only() {
    let mut tcx = TyCtxt::new();
    let i64_ty = tcx.int_ty(IntTy::I64);
    let node = adt(&mut tcx, 3);
    let next = option(&mut tcx, node);
    tcx.register_struct_fields(DefId::local(3), vec![i64_ty, next]);
    assert!(!tcx.can_join_cycle(node));
}

#[test]
fn an_enum_that_reaches_itself_through_a_struct_builds_trees_only() {
    let mut tcx = TyCtxt::new();
    let wrapper = adt(&mut tcx, 4);
    let tree = adt(&mut tcx, 5);
    let held = option(&mut tcx, tree);
    let list = tcx.intern(TyKind::Vec(tree));
    tcx.register_struct_fields(DefId::local(4), vec![held, list]);
    tcx.register_enum_variant_tys(DefId::local(5), vec![vec![wrapper], vec![]]);
    assert!(!tcx.can_join_cycle(tree));
    assert!(!tcx.can_join_cycle(wrapper));
}

#[test]
fn a_type_that_reaches_a_value_of_unstated_contents_can_join_a_cycle() {
    let mut tcx = TyCtxt::new();
    let dynamic = tcx.dyn_value_ty();
    let holder = adt(&mut tcx, 6);
    tcx.register_struct_fields(DefId::local(6), vec![dynamic]);
    assert!(tcx.can_join_cycle(holder));
    let leaf = adt(&mut tcx, 7);
    tcx.register_enum_variant_tys(DefId::local(7), vec![vec![holder]]);
    assert!(tcx.can_join_cycle(leaf));
}
