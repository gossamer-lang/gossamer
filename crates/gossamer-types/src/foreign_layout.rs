//! How a value crosses the C boundary: one class character per scalar, and
//! for a `#[repr(C)]` struct the list of scalar leaves at their C offsets.
//!
//! | class | C type |
//! |---|---|
//! | `c` / `C` | `int8_t` / `uint8_t` |
//! | `B` | `_Bool` |
//! | `h` / `H` | `int16_t` / `uint16_t` (struct fields and slice elements) |
//! | `i` / `I` | `int32_t` / `uint32_t` |
//! | `l` / `L` | `int64_t` / `uint64_t` (`isize` / `usize` too) |
//! | `f` / `d` | `float` / `double` |

use crate::{FloatTy, IntTy, Ty, TyCtxt, TyKind};

/// One step from a struct value to one of its scalar leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CStep {
    /// The field with this declaration index.
    Field(u32),
    /// The element with this index of a fixed array.
    Index(u32),
}

/// A scalar inside a plain-data value, at its C offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CLeaf {
    /// The fields and elements from the value to the leaf.
    pub steps: Vec<CStep>,
    /// Byte offset of the leaf in the C layout.
    pub offset: u32,
    /// The leaf's class character.
    pub class: char,
    /// The leaf's type.
    pub ty: Ty,
}

/// Bytes a value of class `class` occupies, or `None` for an unknown class.
#[must_use]
pub fn c_class_width(class: char) -> Option<u32> {
    Some(match class {
        'c' | 'C' | 'B' => 1,
        'h' | 'H' => 2,
        'i' | 'I' | 'f' => 4,
        'l' | 'L' | 'd' => 8,
        _ => return None,
    })
}

/// Whether an integer of class `class` is sign-extended when it widens.
#[must_use]
pub fn c_class_signed(class: char) -> bool {
    matches!(class, 'c' | 'h' | 'i' | 'l')
}

impl TyCtxt {
    /// The C class of a scalar `ty`, or `None` for a type that is not one.
    #[must_use]
    pub fn c_scalar_class(&self, ty: Ty) -> Option<char> {
        Some(match self.kind_of(ty) {
            TyKind::Bool => 'B',
            TyKind::Int(int) => match int {
                IntTy::I8 => 'c',
                IntTy::U8 => 'C',
                IntTy::I16 => 'h',
                IntTy::U16 => 'H',
                IntTy::I32 => 'i',
                IntTy::U32 => 'I',
                IntTy::I64 | IntTy::Isize => 'l',
                IntTy::U64 | IntTy::Usize => 'L',
                IntTy::I128 | IntTy::U128 => return None,
            },
            TyKind::Float(FloatTy::F32) => 'f',
            TyKind::Float(FloatTy::F64) => 'd',
            _ => return None,
        })
    }

    /// The C size of the plain-data `ty` and its scalar leaves in layout
    /// order, or `None` when a leaf has no C class (a `char`, an `i128`) or
    /// `ty` is not plain data.
    #[must_use]
    pub fn c_leaves(&self, ty: Ty) -> Option<(u32, Vec<CLeaf>)> {
        let layout = self.plain_layout(ty)?;
        let mut leaves = Vec::new();
        self.collect_c_leaves(ty, 0, &mut Vec::new(), &mut leaves)?;
        Some((layout.size, leaves))
    }

    /// Every scalar of the plain-data `ty` a calling convention classifies,
    /// as `(offset, class)` sorted by offset. A union lists each member's
    /// scalars at its own offset, so entries may share bytes.
    #[must_use]
    pub fn c_abi_leaves(&self, ty: Ty) -> Option<Vec<(u32, char)>> {
        let mut leaves = Vec::new();
        self.collect_abi_leaves(ty, 0, &mut leaves)?;
        leaves.sort_by_key(|(offset, _)| *offset);
        Some(leaves)
    }

    fn collect_abi_leaves(&self, ty: Ty, offset: u32, leaves: &mut Vec<(u32, char)>) -> Option<()> {
        if let Some(class) = self.c_scalar_class(ty) {
            leaves.push((offset, class));
            return Some(());
        }
        match self.kind_of(ty) {
            TyKind::Array { elem, len } => {
                let elem = *elem;
                let count = u32::try_from(len.to_usize()).ok()?;
                let stride = self.plain_layout(elem)?.size;
                for index in 0..count {
                    self.collect_abi_leaves(elem, offset + index * stride, leaves)?;
                }
                Some(())
            }
            TyKind::Adt { def, substs } => {
                if let Some(members) = self.union_members(*def, substs) {
                    for member in members {
                        self.collect_abi_leaves(member, offset, leaves)?;
                    }
                    return Some(());
                }
                let fields = self.adt_field_tys(*def, &substs.clone())?.to_vec();
                let layout = self.plain_layout(ty)?;
                for (field, field_offset) in fields.iter().zip(layout.field_offsets.iter()) {
                    self.collect_abi_leaves(*field, offset + field_offset, leaves)?;
                }
                Some(())
            }
            _ => None,
        }
    }

    fn collect_c_leaves(
        &self,
        ty: Ty,
        offset: u32,
        steps: &mut Vec<CStep>,
        leaves: &mut Vec<CLeaf>,
    ) -> Option<()> {
        if let Some(class) = self.c_scalar_class(ty) {
            leaves.push(CLeaf {
                steps: steps.clone(),
                offset,
                class,
                ty,
            });
            return Some(());
        }
        match self.kind_of(ty) {
            TyKind::Array { elem, len } => {
                let elem = *elem;
                let count = u32::try_from(len.to_usize()).ok()?;
                let stride = self.plain_layout(elem)?.size;
                for index in 0..count {
                    steps.push(CStep::Index(index));
                    self.collect_c_leaves(elem, offset + index * stride, steps, leaves)?;
                    steps.pop();
                }
                Some(())
            }
            TyKind::Adt { def, substs } if self.union_members(*def, substs).is_some() => {
                // A union crosses as its bytes; which member they hold is the
                // program's business.
                let size = self.plain_layout(ty)?.size;
                let byte = self.struct_field_tys(*def)?.first().and_then(|bytes| {
                    match self.kind_of(*bytes) {
                        TyKind::Array { elem, .. } => Some(*elem),
                        _ => None,
                    }
                })?;
                for index in 0..size {
                    steps.push(CStep::Field(0));
                    steps.push(CStep::Index(index));
                    leaves.push(CLeaf {
                        steps: steps.clone(),
                        offset: offset + index,
                        class: 'C',
                        ty: byte,
                    });
                    steps.pop();
                    steps.pop();
                }
                Some(())
            }
            TyKind::Adt { def, substs } => {
                let fields = self.adt_field_tys(*def, &substs.clone())?.to_vec();
                let layout = self.plain_layout(ty)?;
                for (index, (field, field_offset)) in
                    fields.iter().zip(layout.field_offsets.iter()).enumerate()
                {
                    steps.push(CStep::Field(u32::try_from(index).ok()?));
                    self.collect_c_leaves(*field, offset + field_offset, steps, leaves)?;
                    steps.pop();
                }
                Some(())
            }
            _ => None,
        }
    }
}
