//! The packed layout of structs whose fields are narrower than a word.

#![allow(missing_docs)]

use gossamer_resolve::DefId;
use gossamer_types::{FloatTy, IntTy, PackedLayout, Substs, Ty, TyCtxt, TyKind};

fn strukt(tcx: &mut TyCtxt, local: u32, fields: Vec<Ty>) -> Ty {
    let def = DefId::local(local);
    tcx.register_struct_fields(def, fields);
    tcx.intern(TyKind::Adt {
        def,
        substs: Substs::new(),
    })
}

fn layout(size: u32, field_offsets: &[u32], field_bytes: &[u32]) -> PackedLayout {
    PackedLayout {
        size,
        field_offsets: field_offsets.to_vec(),
        field_bytes: field_bytes.to_vec(),
    }
}

#[test]
fn narrow_integer_fields_sit_at_their_own_width_and_f32_keeps_a_word() {
    let mut tcx = TyCtxt::new();
    let u8_ty = tcx.int_ty(IntTy::U8);
    let u16_ty = tcx.int_ty(IntTy::U16);
    let u32_ty = tcx.int_ty(IntTy::U32);
    let f32_ty = tcx.float_ty(FloatTy::F32);
    let pixel = strukt(&mut tcx, 1, vec![u8_ty, u8_ty, u16_ty, f32_ty]);
    assert_eq!(
        tcx.packed_layout(pixel),
        Some(layout(16, &[0, 1, 2, 8], &[1, 1, 2, 8]))
    );
    assert_eq!(tcx.slot_bytes(pixel), 16);
    let word = strukt(&mut tcx, 2, vec![u8_ty, u8_ty, u16_ty, u32_ty]);
    assert_eq!(
        tcx.packed_layout(word),
        Some(layout(8, &[0, 1, 2, 4], &[1, 1, 2, 4]))
    );
    assert_eq!(tcx.slot_bytes(word), 8);
    assert_eq!(
        tcx.packed_leaves(pixel),
        Some(vec![(0, u8_ty), (1, u8_ty), (2, u16_ty), (8, f32_ty)])
    );
}

#[test]
fn a_nested_struct_sits_on_a_word_boundary_at_its_own_size() {
    let mut tcx = TyCtxt::new();
    let u8_ty = tcx.int_ty(IntTy::U8);
    let u16_ty = tcx.int_ty(IntTy::U16);
    let bool_ty = tcx.bool_ty();
    let inner = strukt(&mut tcx, 1, vec![u8_ty, u16_ty]);
    let outer = strukt(&mut tcx, 2, vec![bool_ty, u8_ty, inner, u16_ty]);
    assert_eq!(
        tcx.packed_layout(outer),
        Some(layout(24, &[0, 1, 8, 16], &[1, 1, 8, 2]))
    );
    assert_eq!(
        tcx.packed_leaves(outer),
        Some(vec![
            (0, bool_ty),
            (1, u8_ty),
            (8, u8_ty),
            (10, u16_ty),
            (16, u16_ty)
        ])
    );
}

#[test]
fn a_struct_the_packing_would_not_change_or_cannot_hold_keeps_its_words() {
    let mut tcx = TyCtxt::new();
    let u8_ty = tcx.int_ty(IntTy::U8);
    let i64_ty = tcx.int_ty(IntTy::I64);
    let string = tcx.string_ty();
    let aligned = strukt(&mut tcx, 1, vec![u8_ty, i64_ty]);
    assert_eq!(tcx.packed_layout(aligned), None);
    assert_eq!(tcx.slot_bytes(aligned), 16);
    let named = strukt(&mut tcx, 2, vec![u8_ty, string]);
    assert_eq!(tcx.packed_layout(named), None);
    let pair = strukt(&mut tcx, 3, vec![u8_ty, u8_ty]);
    assert_eq!(tcx.slot_bytes(pair), 8);
    tcx.keep_word_fields(DefId::local(3));
    assert_eq!(tcx.packed_layout(pair), None);
    assert_eq!(tcx.slot_bytes(pair), 16);
}
