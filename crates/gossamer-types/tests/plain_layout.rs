//! The natural byte layout of plain data, and its agreement with the word-slot
//! layout for structs whose every field is a word.

#![allow(missing_docs)]

use gossamer_resolve::DefId;
use gossamer_types::{FloatTy, IntTy, PlainLayout, Substs, Ty, TyCtxt, TyKind};

fn strukt(tcx: &mut TyCtxt, local: u32, fields: Vec<Ty>) -> Ty {
    let def = DefId::local(local);
    tcx.register_struct_fields(def, fields);
    tcx.intern(TyKind::Adt {
        def,
        substs: Substs::new(),
    })
}

fn layout(size: u32, align: u32, field_offsets: &[u32]) -> PlainLayout {
    PlainLayout {
        size,
        align,
        field_offsets: field_offsets.to_vec(),
    }
}

#[test]
fn scalars_take_their_own_size_and_alignment() {
    let mut tcx = TyCtxt::new();
    let cases = [
        (tcx.bool_ty(), 1),
        (tcx.char_ty(), 4),
        (tcx.int_ty(IntTy::U8), 1),
        (tcx.int_ty(IntTy::I16), 2),
        (tcx.int_ty(IntTy::U32), 4),
        (tcx.int_ty(IntTy::I64), 8),
        (tcx.float_ty(FloatTy::F32), 4),
        (tcx.float_ty(FloatTy::F64), 8),
    ];
    for (ty, size) in cases {
        assert_eq!(tcx.plain_layout(ty), Some(layout(size, size, &[])));
    }
}

#[test]
fn fields_pack_in_declaration_order_with_trailing_padding() {
    let mut tcx = TyCtxt::new();
    let u8_ty = tcx.int_ty(IntTy::U8);
    let u16_ty = tcx.int_ty(IntTy::U16);
    let f32_ty = tcx.float_ty(FloatTy::F32);
    let i64_ty = tcx.int_ty(IntTy::I64);
    let bool_ty = tcx.bool_ty();
    let narrow = strukt(&mut tcx, 1, vec![u8_ty, u16_ty, f32_ty]);
    assert_eq!(tcx.plain_layout(narrow), Some(layout(8, 4, &[0, 2, 4])));
    let padded = strukt(&mut tcx, 2, vec![bool_ty, i64_ty, u8_ty]);
    assert_eq!(tcx.plain_layout(padded), Some(layout(24, 8, &[0, 8, 16])));
    let pair = tcx.intern(TyKind::Tuple(vec![u8_ty, narrow]));
    assert_eq!(tcx.plain_layout(pair), Some(layout(12, 4, &[0, 4])));
    let row = tcx.intern(TyKind::Array {
        elem: narrow,
        len: gossamer_types::ArrayLen::Concrete(3),
    });
    assert_eq!(tcx.plain_layout(row), Some(layout(24, 4, &[])));
}

#[test]
fn handles_and_heap_children_are_not_plain_data() {
    let mut tcx = TyCtxt::new();
    let i64_ty = tcx.int_ty(IntTy::I64);
    let string = tcx.string_ty();
    let named = strukt(&mut tcx, 1, vec![i64_ty, string]);
    let list = tcx.intern(TyKind::Vec(i64_ty));
    assert!(!tcx.is_plain_data(string));
    assert!(!tcx.is_plain_data(named));
    assert!(!tcx.is_plain_data(list));
    let nested = tcx.intern(TyKind::Tuple(vec![i64_ty, named]));
    assert!(!tcx.is_plain_data(nested));
}

#[test]
fn word_field_structs_keep_the_offsets_their_slots_give_them() {
    let mut tcx = TyCtxt::new();
    let i64_ty = tcx.int_ty(IntTy::I64);
    let f64_ty = tcx.float_ty(FloatTy::F64);
    let inner = strukt(&mut tcx, 1, vec![f64_ty, i64_ty]);
    let outer = strukt(&mut tcx, 2, vec![i64_ty, inner, f64_ty]);
    for ty in [inner, outer] {
        let plain = tcx.plain_layout(ty).expect("plain data");
        assert_eq!(plain.size, tcx.slot_bytes(ty));
        let offsets: Vec<u32> = (0..plain.field_offsets.len())
            .map(|i| u32::try_from(i).expect("few fields") * 8)
            .collect();
        if ty == inner {
            assert_eq!(plain.field_offsets, offsets);
        }
    }
    let flat_outer = tcx.plain_layout(outer).expect("plain data");
    assert_eq!(flat_outer.field_offsets, vec![0, 8, 24]);
}
