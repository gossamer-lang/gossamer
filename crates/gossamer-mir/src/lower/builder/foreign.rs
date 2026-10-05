//! Calls to functions declared in an `unsafe extern "C"` block.
//!
//! `gossamer_hir::route_foreign_calls` turns each call into
//! `__gos_ffi_call(symbol, signature, library, layouts, args..)`; this lowers
//! it to one intrinsic the backends call directly. A `#[repr(C)]` struct or
//! fixed array argument is packed here, leaf by leaf, into a C-layout buffer
//! the call receives a pointer to, and unpacked after the call when the
//! callee's writes come back, so the backends see only scalars, slice
//! handles, and raw pointers.

use gossamer_hir::{HirExpr, HirExprKind, HirLiteral, HirUnaryOp};
use gossamer_lex::Span;
use gossamer_types::{CLeaf, FloatTy, IntTy, Ty, TyKind, c_class_signed, c_class_width};

use super::Builder;
use crate::ir::{
    ConstValue, ForeignCall, ForeignCallback, ForeignParam, ForeignStatic, Local, Operand, Place,
    Projection, Rvalue,
};

/// A struct or array argument packed for the call.
struct Packed {
    buffer: Local,
    place: Place,
    leaves: Vec<CLeaf>,
    writable: bool,
}

fn literal(expr: &HirExpr) -> Option<&str> {
    match &expr.kind {
        HirExprKind::Literal(HirLiteral::String(text)) => Some(text),
        _ => None,
    }
}

impl Builder<'_> {
    /// Lowers a routed foreign call: one intrinsic whose name carries the
    /// symbol, the call site's signature, and the library.
    pub(crate) fn lower_foreign_call(
        &mut self,
        args: &[HirExpr],
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        let [symbol, signature, library, _layouts, rest @ ..] = args else {
            return None;
        };
        let (symbol, signature, library) =
            (literal(symbol)?, literal(signature)?, literal(library)?);
        let declared = ForeignCall::intrinsic_name(symbol, signature, library);
        let declared_call = ForeignCall::parse(declared)?;
        let params = declared_call.param_list();
        let returns_struct = declared_call.ret_layout.is_some();
        let ret = signature.rsplit_once('>')?.1;
        let mut spelling = String::with_capacity(signature.len());
        let mut operands = Vec::with_capacity(rest.len());
        let mut packed = Vec::new();
        // A call through an address carries the address before the C
        // arguments; it is the backend's call target.
        let rest = if symbol == gossamer_hir::FFI_INDIRECT_SYMBOL {
            let (target, rest) = rest.split_first()?;
            let local = self.lower_expr(target)?;
            operands.push(Operand::Copy(Place::local(local)));
            rest
        } else {
            rest
        };
        // A struct result comes back through the final argument, a
        // one-element array the backend writes and the call reads back.
        let (rest, holder) = if returns_struct {
            let (holder, rest) = rest.split_last()?;
            (rest, Some(holder))
        } else {
            (rest, None)
        };
        for (param, arg) in params.iter().zip(rest) {
            let aggregate = match param {
                ForeignParam::Struct { writable } => Some(*writable),
                ForeignParam::ByValue(_) => Some(false),
                ForeignParam::Slice { writable, .. } if self.is_fixed_array_arg(arg) => {
                    Some(*writable)
                }
                _ => None,
            };
            if let Some(writable) = aggregate {
                let (inner_ty, place) = self.foreign_aggregate_place(arg)?;
                let (size, leaves) = self.tcx.c_leaves(inner_ty)?;
                let buffer = self.pack_foreign_aggregate(&place, &leaves, size, span);
                operands.push(Operand::Copy(Place::local(buffer)));
                match param {
                    ForeignParam::ByValue(_) => spelling.push_str(&param.spelling()),
                    _ => spelling.push(if writable { 'R' } else { 'r' }),
                }
                packed.push(Packed {
                    buffer,
                    place,
                    leaves,
                    writable,
                });
                continue;
            }
            match param {
                ForeignParam::Scalar(class) => spelling.push(*class),
                ForeignParam::Slice { elem, writable } => {
                    spelling.push(if *writable { 'P' } else { 'p' });
                    spelling.push(*elem);
                }
                ForeignParam::Struct { .. } | ForeignParam::ByValue(_) => {
                    unreachable!("packed above")
                }
            }
            let local = self.lower_expr(arg)?;
            operands.push(Operand::Copy(Place::local(local)));
        }
        if let Some(holder) = holder {
            let (inner_ty, place) = self.foreign_aggregate_place(holder)?;
            let (size, leaves) = self.tcx.c_leaves(inner_ty)?;
            let buffer = self.pack_foreign_aggregate(&place, &leaves, size, span);
            operands.push(Operand::Copy(Place::local(buffer)));
            packed.push(Packed {
                buffer,
                place,
                leaves,
                writable: true,
            });
        }
        let name = ForeignCall::intrinsic_name(symbol, &format!("{spelling}>{ret}"), library);
        let dest = self.fresh(ty);
        self.emit_assign(
            Place::local(dest),
            Rvalue::CallIntrinsic {
                name,
                args: operands,
            },
            span,
        );
        for packed in packed {
            if packed.writable {
                self.unpack_foreign_aggregate(&packed, span);
            }
            self.foreign_runtime(
                "gos_rt_ffi_struct_free",
                vec![Operand::Copy(Place::local(packed.buffer))],
                span,
            );
        }
        Some(dest)
    }

    /// Lowers `intrinsic`, one of the calls `gossamer_hir`'s
    /// foreign-boundary pass emits.
    pub(crate) fn lower_ffi_intrinsic(
        &mut self,
        intrinsic: FfiIntrinsic,
        args: &[HirExpr],
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        let runtime = match intrinsic {
            FfiIntrinsic::Runtime(runtime) => runtime,
            FfiIntrinsic::Callback => return self.lower_ffi_callback(args, ty, false, span),
            FfiIntrinsic::Export => return self.lower_ffi_callback(args, ty, true, span),
            FfiIntrinsic::Symbol => {
                let [symbol, library] = args else {
                    return None;
                };
                let name = ForeignStatic::intrinsic_name(literal(symbol)?, literal(library)?);
                let dest = self.fresh(ty);
                self.emit_assign(
                    Place::local(dest),
                    Rvalue::CallIntrinsic {
                        name,
                        args: Vec::new(),
                    },
                    span,
                );
                return Some(dest);
            }
        };
        let mut operands = Vec::with_capacity(args.len());
        for arg in args {
            let local = self.lower_expr(arg)?;
            operands.push(Operand::Copy(Place::local(local)));
        }
        let dest = self.fresh(ty);
        self.emit_assign(
            Place::local(dest),
            Rvalue::CallIntrinsic {
                name: runtime,
                args: operands,
            },
            span,
        );
        Some(dest)
    }

    /// `__gos_ffi_callback(adapter, signature, name)`: the address of a C-ABI
    /// entry the backend generates for `adapter`. `__gos_ffi_export(adapter,
    /// signature, symbol)` is the same entry defined externally as `symbol`.
    fn lower_ffi_callback(
        &mut self,
        args: &[HirExpr],
        ty: Ty,
        exported: bool,
        span: Span,
    ) -> Option<Local> {
        let [adapter, signature, name] = args else {
            return None;
        };
        let HirExprKind::Path { segments, .. } = &adapter.kind else {
            return None;
        };
        let adapter = segments.last()?.name.as_str();
        let intrinsic = if exported {
            ForeignCallback::export_intrinsic_name(literal(signature)?, adapter, literal(name)?)
        } else {
            ForeignCallback::intrinsic_name(literal(signature)?, adapter, literal(name)?)
        };
        let dest = self.fresh(ty);
        // The adapter's name as an operand keeps its body reachable.
        self.emit_assign(
            Place::local(dest),
            Rvalue::CallIntrinsic {
                name: intrinsic,
                args: vec![Operand::Const(ConstValue::Str(adapter.to_string()))],
            },
            span,
        );
        Some(dest)
    }

    /// Whether `arg` is a fixed array, or `&mut` one, rather than a sequence
    /// handle: an inline value the call needs packed.
    fn is_fixed_array_arg(&self, arg: &HirExpr) -> bool {
        let ty = match self.tcx.kind_of(arg.ty) {
            TyKind::Ref { inner, .. } => *inner,
            _ => arg.ty,
        };
        matches!(self.tcx.kind_of(ty), TyKind::Array { .. })
    }

    /// The type and place of a struct or array argument: the borrowed place
    /// for `&mut x`, the referent of a `&mut T` value, or a temporary holding
    /// a by-value argument.
    fn foreign_aggregate_place(&mut self, arg: &HirExpr) -> Option<(Ty, Place)> {
        if let HirExprKind::Unary {
            op: HirUnaryOp::RefMut | HirUnaryOp::RefShared,
            operand,
        } = &arg.kind
        {
            let place = self.lower_place_expr(operand)?;
            return Some((operand.ty, place));
        }
        let local = self.lower_expr(arg)?;
        let ty = match self.tcx.kind_of(arg.ty) {
            TyKind::Ref { inner, .. } => *inner,
            _ => arg.ty,
        };
        Some((ty, Place::local(local)))
    }

    /// The place of `leaf` inside the aggregate at `base`.
    fn foreign_leaf_place(&mut self, base: &Place, leaf: &CLeaf, span: Span) -> Place {
        let mut place = base.clone();
        for step in &leaf.steps {
            match step {
                gossamer_types::CStep::Field(index) => {
                    place.projection.push(Projection::Field(*index));
                }
                gossamer_types::CStep::Index(index) => {
                    let i64_ty = self.tcx.int_ty(IntTy::I64);
                    let at = self.fresh(i64_ty);
                    self.emit_assign(
                        Place::local(at),
                        Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(*index)))),
                        span,
                    );
                    place.projection.push(Projection::Index(at));
                }
            }
        }
        place
    }

    fn foreign_runtime(&mut self, name: &'static str, args: Vec<Operand>, span: Span) -> Local {
        let ret = gossamer_abi::lookup(name).map(|entry| entry.sig.ret);
        let ty = match ret {
            Some(gossamer_abi::AbiType::F64) => self.tcx.float_ty(FloatTy::F64),
            Some(gossamer_abi::AbiType::Void) => self.tcx.unit(),
            _ => self.tcx.int_ty(IntTy::I64),
        };
        let dest = self.fresh(ty);
        self.emit_assign(
            Place::local(dest),
            Rvalue::CallIntrinsic { name, args },
            span,
        );
        dest
    }

    fn int_const(value: u32) -> Operand {
        Operand::Const(ConstValue::Int(i128::from(value)))
    }

    /// Allocates a zeroed C buffer of `size` bytes and writes each leaf of the
    /// aggregate at `place` into it.
    fn pack_foreign_aggregate(
        &mut self,
        place: &Place,
        leaves: &[CLeaf],
        size: u32,
        span: Span,
    ) -> Local {
        let buffer =
            self.foreign_runtime("gos_rt_ffi_struct_alloc", vec![Self::int_const(size)], span);
        let f64_ty = self.tcx.float_ty(FloatTy::F64);
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        for leaf in leaves {
            let leaf_place = self.foreign_leaf_place(place, leaf, span);
            let value = self.fresh(leaf.ty);
            self.emit_assign(
                Place::local(value),
                Rvalue::Use(Operand::Copy(leaf_place)),
                span,
            );
            let buffer_op = Operand::Copy(Place::local(buffer));
            let offset = Self::int_const(leaf.offset);
            match leaf.class {
                'f' | 'd' => {
                    let wide = self.fresh(f64_ty);
                    self.emit_assign(
                        Place::local(wide),
                        Rvalue::Cast {
                            operand: Operand::Copy(Place::local(value)),
                            target: f64_ty,
                        },
                        span,
                    );
                    let helper = if leaf.class == 'f' {
                        "gos_rt_ffi_put_f32"
                    } else {
                        "gos_rt_ffi_put_f64"
                    };
                    self.foreign_runtime(
                        helper,
                        vec![buffer_op, offset, Operand::Copy(Place::local(wide))],
                        span,
                    );
                }
                class => {
                    let word = self.fresh(i64_ty);
                    self.emit_assign(
                        Place::local(word),
                        Rvalue::Cast {
                            operand: Operand::Copy(Place::local(value)),
                            target: i64_ty,
                        },
                        span,
                    );
                    let width = c_class_width(class).unwrap_or(8);
                    self.foreign_runtime(
                        "gos_rt_ffi_put",
                        vec![
                            buffer_op,
                            offset,
                            Self::int_const(width),
                            Operand::Copy(Place::local(word)),
                        ],
                        span,
                    );
                }
            }
        }
        buffer
    }

    /// Reads each leaf back from the C buffer into the aggregate's place.
    fn unpack_foreign_aggregate(&mut self, packed: &Packed, span: Span) {
        for leaf in &packed.leaves {
            let buffer_op = Operand::Copy(Place::local(packed.buffer));
            let offset = Self::int_const(leaf.offset);
            let raw = match leaf.class {
                'f' => self.foreign_runtime("gos_rt_ffi_get_f32", vec![buffer_op, offset], span),
                'd' => self.foreign_runtime("gos_rt_ffi_get_f64", vec![buffer_op, offset], span),
                class => self.foreign_runtime(
                    "gos_rt_ffi_get",
                    vec![
                        buffer_op,
                        offset,
                        Self::int_const(c_class_width(class).unwrap_or(8)),
                        Self::int_const(u32::from(c_class_signed(class))),
                    ],
                    span,
                ),
            };
            let value = self.fresh(leaf.ty);
            let read_back = if leaf.class == 'B' {
                // A C `_Bool` reads back as whether its byte is set.
                Rvalue::BinaryOp {
                    op: crate::ir::BinOp::Ne,
                    lhs: Operand::Copy(Place::local(raw)),
                    rhs: Operand::Const(ConstValue::Int(0)),
                }
            } else {
                Rvalue::Cast {
                    operand: Operand::Copy(Place::local(raw)),
                    target: leaf.ty,
                }
            };
            self.emit_assign(Place::local(value), read_back, span);
            let leaf_place = self.foreign_leaf_place(&packed.place, leaf, span);
            self.emit_assign(
                leaf_place,
                Rvalue::Use(Operand::Copy(Place::local(value))),
                span,
            );
        }
    }
}

/// A call the foreign-boundary pass emits, by what it lowers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FfiIntrinsic {
    /// A direct call to the named runtime export.
    Runtime(&'static str),
    /// The address of a generated C-ABI entry for a callback adapter.
    Callback,
    /// An external C-ABI entry for an `#[export]` function's adapter.
    Export,
    /// The address of a C global.
    Symbol,
}

impl FfiIntrinsic {
    /// The intrinsic a bare callee path names, if it is one.
    pub(crate) fn named(name: &str) -> Option<Self> {
        Some(match name {
            gossamer_hir::FFI_NULL_RESULT => Self::Runtime("gos_rt_ffi_null_result"),
            gossamer_hir::FFI_HANDLE_PIN => Self::Runtime("gos_rt_ffi_handle_pin"),
            gossamer_hir::FFI_HANDLE_CELL => Self::Runtime("gos_rt_ffi_handle_cell"),
            gossamer_hir::FFI_HANDLE_STORE => Self::Runtime("gos_rt_ffi_handle_store"),
            gossamer_hir::FFI_HANDLE_RELEASE => Self::Runtime("gos_rt_ffi_handle_release"),
            gossamer_hir::FFI_CALLBACK => Self::Callback,
            gossamer_hir::FFI_EXPORT => Self::Export,
            gossamer_hir::FFI_SYMBOL => Self::Symbol,
            gossamer_hir::FFI_LOAD_INT => Self::Runtime("gos_rt_ffi_load_int"),
            gossamer_hir::FFI_LOAD_FLOAT => Self::Runtime("gos_rt_ffi_load_float"),
            gossamer_hir::FFI_STORE_INT => Self::Runtime("gos_rt_ffi_store_int"),
            gossamer_hir::FFI_STORE_FLOAT => Self::Runtime("gos_rt_ffi_store_float"),
            gossamer_hir::FFI_VIEW_CHECK => Self::Runtime("gos_rt_ffi_view_check"),
            gossamer_hir::FFI_VIEW_RANGE_CHECK => Self::Runtime("gos_rt_ffi_view_range_check"),
            gossamer_hir::FFI_VIEW_LEN_CHECK => Self::Runtime("gos_rt_ffi_view_len_check"),
            gossamer_hir::FFI_ATOMIC_RMW => Self::Runtime("gos_rt_ffi_atomic_rmw"),
            gossamer_hir::FFI_ATOMIC_CAS => Self::Runtime("gos_rt_ffi_atomic_cas"),
            _ => return None,
        })
    }
}
