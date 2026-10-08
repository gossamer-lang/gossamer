//! Typing standard library calls, handles, and their return types.

use super::{
    ArrayExpr, BINARY_HEAP_DEF_LOCAL, BTREE_SET_DEF_LOCAL, BinaryOp, DURATION_METHODS,
    DeferredStructural, DeferredStructuralKind, Expectation, Expr, ExprKind, FloatTy, FnSig,
    HANDLE_METHODS, HANDLE_SENTINEL_SPAN, HASH_SET_DEF_LOCAL, IntTy, Literal, MIN_HEAP_DEF_LOCAL,
    Mutbl, NodeId, PURE_HANDLE_HI_OFFSET, PURE_HANDLES, REVERSE_DEF_LOCAL, SYNC_HANDLE_HI_OFFSET,
    SYNC_HANDLE_LO_OFFSET, Shape, Span, Ty, TyKind, TypeChecker, TypeError, U8_VEC_OFFSET, UnaryOp,
    VEC_DEQUE_DEF_LOCAL, VEC_QUEUE_DEF_LOCAL, VEC_STACK_DEF_LOCAL, atomic_method, fs_file_ctor,
    is_catalog_type_param, is_channel_constructor_path, is_opaque_handle_def, net_socket_ctor,
    render_array_len, stdlib_fs_handle, stdlib_handle_ctor, stdlib_handle_def_offset,
    stdlib_net_handle, strip_catalog_wrapper, table_handle_owner,
};

impl TypeChecker<'_> {
    /// The vector type of `lanes` lanes of `elem`, reporting an element type or
    /// lane count outside the supported set.
    pub(super) fn checked_simd_ty(&mut self, elem: Ty, lanes: crate::ArrayLen, span: Span) -> Ty {
        let elem = self.infer.resolve(self.tcx, elem);
        let takes_sixteen = match self.tcx.kind(elem) {
            Some(TyKind::Float(_) | TyKind::Int(IntTy::I64 | IntTy::U64)) => false,
            Some(
                TyKind::Int(
                    IntTy::I8 | IntTy::U8 | IntTy::I16 | IntTy::U16 | IntTy::I32 | IntTy::U32,
                )
                | TyKind::Bool
                | TyKind::Var(_),
            ) => true,
            _ => {
                let ty = self.render_public_ty(elem);
                self.emit(
                    TypeError::SimdShape {
                        reason: format!("`{ty}` is not a lane type"),
                    },
                    span,
                );
                return self.tcx.error_ty();
            }
        };
        if let crate::ArrayLen::Concrete(count) = lanes
            && !(matches!(count, 2 | 4 | 8) || (count == 16 && takes_sixteen))
        {
            let ty = self.render_public_ty(elem);
            self.emit(
                TypeError::SimdShape {
                    reason: format!("{count} lanes of `{ty}`"),
                },
                span,
            );
            return self.tcx.error_ty();
        }
        self.tcx.intern(TyKind::Simd { elem, lanes })
    }

    /// The element type of a sequence a lane window reads or writes: a `Vec`,
    /// a slice, or a fixed array, seen through references.
    pub(super) fn simd_window_elem(&mut self, ty: Ty) -> Option<Ty> {
        let mut ty = self.infer.resolve(self.tcx, ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(ty) {
            ty = self.infer.resolve(self.tcx, *inner);
        }
        match self.tcx.kind(ty) {
            Some(TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. }) => {
                Some(*elem)
            }
            _ => None,
        }
    }

    /// The vector constructors: `Simd::from_array(a)`, `Simd::splat(v)`,
    /// `Simd::load(xs, offset)`, `Simd::load_or(xs, offset, fill)`,
    /// `Simd::gather(xs, indices)`, `Simd::from_bits(v)`, and
    /// `Mask::from_bitmask(bits)`.
    pub(super) fn simd_ctor_ret(
        &mut self,
        last: &str,
        args: &[Expr],
        arg_tys: &[Ty],
        expected: Expectation,
        span: Span,
    ) -> Option<Ty> {
        Some(match (last, arg_tys) {
            ("from_array", [array]) => self.simd_from_array(args, *array, span),
            ("load", [source, offset]) => {
                self.simd_load_ret("load", expected, *source, *offset, None, span)
            }
            ("load_or", [source, offset, fill]) => {
                self.simd_load_ret("load_or", expected, *source, *offset, Some(*fill), span)
            }
            ("from_bitmask", [bits]) => {
                let want = self.expected_ty(expected);
                let is_mask = matches!(
                    self.tcx.kind(want).cloned(),
                    Some(TyKind::Simd { elem, .. }) if matches!(self.tcx.kind(elem), Some(TyKind::Bool))
                );
                if !is_mask {
                    return Some(self.simd_shape_error(
                        "`Mask::from_bitmask` takes its lane count from the annotated type",
                        span,
                    ));
                }
                let u64_ty = self.tcx.int_ty(IntTy::U64);
                self.unify(u64_ty, *bits, span);
                want
            }
            ("from_bits", [bits]) => {
                let want = self.expected_ty(expected);
                let Some(TyKind::Simd { elem, lanes }) = self.tcx.kind(want).cloned() else {
                    return Some(self.simd_shape_error(
                        "`Simd::from_bits` takes its lane type from the annotated type",
                        span,
                    ));
                };
                let elem = self.infer.resolve(self.tcx, elem);
                let unsigned = self.unsigned_lane(elem);
                let source = self.tcx.intern(TyKind::Simd {
                    elem: unsigned,
                    lanes,
                });
                self.unify(source, *bits, span);
                want
            }
            ("gather", [source, indices]) => self.simd_gather(*source, *indices, span),
            ("splat", [value]) => {
                let want = self.expected_ty(expected);
                if let Some(TyKind::Simd { elem, .. }) = self.tcx.kind(want).cloned() {
                    self.unify(elem, *value, span);
                    return Some(want);
                }
                self.simd_shape_error(
                    "`Simd::splat` takes its lane count from the annotated type",
                    span,
                )
            }
            _ => return None,
        })
    }

    /// The type the context expects, resolved, or the error type without one.
    fn expected_ty(&mut self, expected: Expectation) -> Ty {
        match expected {
            Expectation::HasType(ty) => self.infer.resolve(self.tcx, ty),
            _ => self.tcx.error_ty(),
        }
    }

    /// Reports GT0089 with `reason` and answers the error type.
    fn simd_shape_error(&mut self, reason: &str, span: Span) -> Ty {
        self.emit(
            TypeError::SimdShape {
                reason: reason.to_string(),
            },
            span,
        );
        self.tcx.error_ty()
    }

    /// `Simd::from_array(a)`: the lanes and their count from a fixed array.
    fn simd_from_array(&mut self, args: &[Expr], array: Ty, span: Span) -> Ty {
        const TAKES: &str = "`Simd::from_array` takes a fixed array such as `[1.0, 2.0, 3.0, 4.0]`";
        let array = self.infer.resolve(self.tcx, array);
        let (elem, len) = match self.tcx.kind(array).cloned() {
            Some(TyKind::Array { elem, len }) => (elem, len),
            // A bracket literal names its lanes by its length; it is the
            // fixed array the vector is laid out as.
            Some(TyKind::Vec(elem)) => {
                let Some(ExprKind::Array(gossamer_ast::ArrayExpr::List(items))) =
                    args.first().map(|arg| &arg.kind)
                else {
                    return self.simd_shape_error(TAKES, span);
                };
                let len = crate::ArrayLen::Concrete(items.len());
                let fixed = self.tcx.intern(TyKind::Array { elem, len });
                if let Some(arg) = args.first() {
                    self.record(arg.id, fixed);
                }
                (elem, len)
            }
            _ => return self.simd_shape_error(TAKES, span),
        };
        self.checked_simd_ty(elem, len, span)
    }

    /// `Simd::load(xs, offset)` and `Simd::load_or(xs, offset, fill)`: the
    /// vector type the context expects, its lanes read from a sequence.
    fn simd_load_ret(
        &mut self,
        name: &str,
        expected: Expectation,
        source: Ty,
        offset: Ty,
        fill: Option<Ty>,
        span: Span,
    ) -> Ty {
        let want = self.expected_ty(expected);
        let Some(TyKind::Simd { elem, .. }) = self.tcx.kind(want).cloned() else {
            return self.simd_shape_error(
                &format!("`Simd::{name}` takes its lane count from the annotated type"),
                span,
            );
        };
        if let Some(source_elem) = self.simd_window_elem(source) {
            self.unify(elem, source_elem, span);
        } else {
            let error = self.simd_shape_error(
                &format!("`Simd::{name}` reads lanes from a `Vec<T>`, `[T]`, or `[T; N]`"),
                span,
            );
            if fill.is_none() {
                return error;
            }
        }
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        self.unify(i64_ty, offset, span);
        if let Some(fill) = fill {
            self.unify(elem, fill, span);
        }
        want
    }

    /// `Simd::gather(xs, indices)`: one lane of `xs` per index lane.
    fn simd_gather(&mut self, source: Ty, indices: Ty, span: Span) -> Ty {
        let Some(source_elem) = self.simd_window_elem(source) else {
            return self.simd_shape_error(
                "`Simd::gather` reads lanes from a `Vec<T>`, `[T]`, or `[T; N]`",
                span,
            );
        };
        let indices = self.infer.resolve(self.tcx, indices);
        let Some(TyKind::Simd { elem: index, lanes }) = self.tcx.kind(indices).cloned() else {
            return self.simd_shape_error(
                "`Simd::gather` takes its indices as a `Simd` of integers",
                span,
            );
        };
        if !matches!(self.tcx.kind(index), Some(TyKind::Int(_) | TyKind::Var(_))) {
            self.simd_shape_error("`Simd::gather` indices are integer lanes", span);
        }
        let source_elem = self.infer.resolve(self.tcx, source_elem);
        self.checked_simd_ty(source_elem, lanes, span)
    }

    /// The type of `lhs <op> rhs` when either operand is a lane vector and the
    /// operator is lane-wise; comparisons, logic, and pipes keep their own
    /// typing.
    pub(super) fn check_simd_binary(
        &mut self,
        op: BinaryOp,
        lhs_ty: Ty,
        rhs_ty: Ty,
        span: Span,
    ) -> Option<Ty> {
        if matches!(
            op,
            BinaryOp::Eq
                | BinaryOp::Ne
                | BinaryOp::Lt
                | BinaryOp::Le
                | BinaryOp::Gt
                | BinaryOp::Ge
                | BinaryOp::And
                | BinaryOp::Or
                | BinaryOp::PipeGt
        ) {
            return None;
        }
        let lhs_res = self.infer.resolve(self.tcx, lhs_ty);
        let rhs_res = self.infer.resolve(self.tcx, rhs_ty);
        let simd = [lhs_res, rhs_res]
            .into_iter()
            .find(|ty| matches!(self.tcx.kind(*ty), Some(TyKind::Simd { .. })))?;
        let Some(TyKind::Simd { elem, .. }) = self.tcx.kind(simd).cloned() else {
            return None;
        };
        self.unify(simd, lhs_ty, span);
        self.unify(simd, rhs_ty, span);
        let elem = self.infer.resolve(self.tcx, elem);
        // A lane type still spelled by an unsuffixed literal is the integer or
        // float type that literal defaults to.
        let float = matches!(self.tcx.kind(elem), Some(TyKind::Float(_)))
            || self.infer.is_float_literal_var(self.tcx, elem);
        let int = matches!(self.tcx.kind(elem), Some(TyKind::Int(_)))
            || self.infer.is_integer_constrained_var(self.tcx, elem);
        let mask = matches!(self.tcx.kind(elem), Some(TyKind::Bool));
        let defined = match op {
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul => float || int,
            BinaryOp::Div => float,
            BinaryOp::WrappingAdd
            | BinaryOp::WrappingSub
            | BinaryOp::WrappingMul
            | BinaryOp::WrappingShl
            | BinaryOp::WrappingShr
            | BinaryOp::Shl
            | BinaryOp::Shr => int,
            BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor => int || mask,
            _ => false,
        };
        if !defined {
            let ty = self.render_public_ty(simd);
            self.emit(
                TypeError::SimdShape {
                    reason: format!("`{}` is not a lane-wise operation on `{ty}`", op.as_str()),
                },
                span,
            );
            return Some(self.tcx.error_ty());
        }
        Some(simd)
    }

    /// A method call on a lane vector receiver.
    pub(super) fn check_simd_method(
        &mut self,
        method: &str,
        receiver_ty: Ty,
        args: &[Expr],
        generics: &[gossamer_ast::GenericArg],
        span: Span,
    ) -> Option<Ty> {
        let mut recv = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(recv) {
            recv = self.infer.resolve(self.tcx, *inner);
        }
        let Some(TyKind::Simd { elem, lanes }) = self.tcx.kind(recv).cloned() else {
            return None;
        };
        let elem_res = self.infer.resolve(self.tcx, elem);
        let float = matches!(self.tcx.kind(elem_res), Some(TyKind::Float(_)))
            || self.infer.is_float_literal_var(self.tcx, elem_res);
        let int = matches!(self.tcx.kind(elem_res), Some(TyKind::Int(_)))
            || self.infer.is_integer_constrained_var(self.tcx, elem_res);
        let mask = matches!(self.tcx.kind(elem_res), Some(TyKind::Bool));
        let r = SimdReceiver {
            recv,
            elem,
            elem_res,
            lanes,
            float,
            int,
            mask,
        };
        if let Some(ret) = self.simd_lane_method(method, r, args, span) {
            return Some(ret);
        }
        if let Some(ret) = self.simd_reshape_method(method, r, args, generics, span) {
            return Some(ret);
        }
        if let Some(ret) = self.simd_store_method(method, r, args) {
            return Some(ret);
        }
        for arg in args {
            self.check_expr(arg);
        }
        let ty = self.render_public_ty(recv);
        Some(self.simd_shape_error(&format!("`{ty}` has no method `{method}`"), span))
    }

    /// Lane-wise arithmetic, comparisons, reductions, and mask terminals.
    fn simd_lane_method(
        &mut self,
        method: &str,
        r: SimdReceiver,
        args: &[Expr],
        span: Span,
    ) -> Option<Ty> {
        let SimdReceiver {
            recv,
            elem,
            elem_res,
            lanes,
            float,
            int,
            mask,
        } = r;
        Some(match (method, args.len()) {
            ("to_array", 0) => self.tcx.intern(TyKind::Array { elem, len: lanes }),
            ("min" | "max", 1) if !mask => {
                let got = self.check_expr_expecting(&args[0], Expectation::HasType(recv));
                self.unify(recv, got, args[0].span);
                recv
            }
            ("abs", 0) if !mask => recv,
            ("sqrt", 0) if float => recv,
            ("lanes_eq" | "lanes_lt" | "lanes_le" | "lanes_gt" | "lanes_ge" | "lanes_ne", 1)
                if !mask =>
            {
                let got = self.check_expr_expecting(&args[0], Expectation::HasType(recv));
                self.unify(recv, got, args[0].span);
                let bool_ty = self.tcx.bool_ty();
                self.tcx.intern(TyKind::Simd {
                    elem: bool_ty,
                    lanes,
                })
            }
            ("select", 2) if mask => {
                // Both choices are vectors of the mask's lane count, which a
                // `Simd::splat` among them takes its lanes from.
                let lane = self.fresh();
                let picked = self.tcx.intern(TyKind::Simd { elem: lane, lanes });
                let a = self.check_expr_expecting(&args[0], Expectation::HasType(picked));
                self.unify(picked, a, args[0].span);
                let b = self.check_expr_expecting(&args[1], Expectation::HasType(a));
                self.unify(a, b, args[1].span);
                let a_res = self.infer.resolve(self.tcx, a);
                match self.tcx.kind(a_res) {
                    Some(TyKind::Simd { lanes: picked, .. }) if *picked == lanes => a_res,
                    _ => {
                        self.emit(
                            TypeError::SimdShape {
                                reason:
                                    "`select` picks between two vectors with the mask's lane count"
                                        .to_string(),
                            },
                            span,
                        );
                        self.tcx.error_ty()
                    }
                }
            }
            ("reduce_sum" | "reduce_min" | "reduce_max", 0) if !mask => elem,
            ("reduce_and" | "reduce_or", 0) if int || mask => elem,
            ("saturating_add" | "saturating_sub", 1) if int => {
                let got = self.check_expr_expecting(&args[0], Expectation::HasType(recv));
                self.unify(recv, got, args[0].span);
                recv
            }
            ("abs_diff", 1) if int => {
                let got = self.check_expr_expecting(&args[0], Expectation::HasType(recv));
                self.unify(recv, got, args[0].span);
                let unsigned = self.unsigned_lane(elem_res);
                self.tcx.intern(TyKind::Simd {
                    elem: unsigned,
                    lanes,
                })
            }
            ("mul_add", 2) if float => {
                for arg in args {
                    let got = self.check_expr_expecting(arg, Expectation::HasType(recv));
                    self.unify(recv, got, arg.span);
                }
                recv
            }
            ("to_bitmask", 0) if mask => self.tcx.int_ty(IntTy::U64),
            ("any" | "all", 0) if mask => self.tcx.bool_ty(),
            ("first_set", 0) if mask => {
                if self.concrete_lanes(lanes, span).is_none() {
                    return Some(self.tcx.error_ty());
                }
                let i64_ty = self.tcx.int_ty(IntTy::I64);
                self.option_adt_ty(i64_ty)
            }
            _ => return None,
        })
    }

    /// Conversions and shuffles: lane casts and bits, widening, narrowing,
    /// swizzles, and interleaving.
    fn simd_reshape_method(
        &mut self,
        method: &str,
        r: SimdReceiver,
        args: &[Expr],
        generics: &[gossamer_ast::GenericArg],
        span: Span,
    ) -> Option<Ty> {
        let SimdReceiver {
            recv,
            elem,
            elem_res,
            lanes,
            float,
            int,
            mask,
        } = r;
        Some(match (method, args.len()) {
            ("cast", 0) if !mask => {
                let targets = self.turbofish_types(generics);
                let Some(target) = targets.first().copied() else {
                    return Some(
                        self.simd_shape_error(
                            "`cast` names its lane type: `v.cast::<f32>()`",
                            span,
                        ),
                    );
                };
                self.checked_simd_ty(target, lanes, span)
            }
            ("to_bits", 0) if !mask => {
                let unsigned = self.unsigned_lane(elem_res);
                self.tcx.intern(TyKind::Simd {
                    elem: unsigned,
                    lanes,
                })
            }
            ("widen_low" | "widen_high", 0) if !mask => {
                let Some(wide) = self.widened_lane(elem_res) else {
                    let ty = self.render_public_ty(elem_res);
                    self.emit(
                        TypeError::SimdShape {
                            reason: format!("`{ty}` lanes have no wider lane type"),
                        },
                        span,
                    );
                    return Some(self.tcx.error_ty());
                };
                let Some(half) = self.concrete_lanes(lanes, span).map(|n| n / 2) else {
                    return Some(self.tcx.error_ty());
                };
                self.checked_simd_ty(wide, crate::ArrayLen::Concrete(half), span)
            }
            ("narrow", 1) if int || float => {
                let got = self.check_expr_expecting(&args[0], Expectation::HasType(recv));
                self.unify(recv, got, args[0].span);
                let Some(narrow) = self.narrowed_lane(elem_res) else {
                    let ty = self.render_public_ty(elem_res);
                    self.emit(
                        TypeError::SimdShape {
                            reason: format!("`{ty}` lanes have no narrower lane type"),
                        },
                        span,
                    );
                    return Some(self.tcx.error_ty());
                };
                let Some(double) = self.concrete_lanes(lanes, span).map(|n| n * 2) else {
                    return Some(self.tcx.error_ty());
                };
                self.checked_simd_ty(narrow, crate::ArrayLen::Concrete(double), span)
            }
            ("swizzle", 1) => {
                let Some(source) = self.concrete_lanes(lanes, span) else {
                    return Some(self.tcx.error_ty());
                };
                let Some(count) = self.swizzle_indices(&args[0], source) else {
                    return Some(self.tcx.error_ty());
                };
                self.checked_simd_ty(elem, crate::ArrayLen::Concrete(count), span)
            }
            ("concat_swizzle", 2) => {
                let got = self.check_expr_expecting(&args[0], Expectation::HasType(recv));
                self.unify(recv, got, args[0].span);
                let Some(source) = self.concrete_lanes(lanes, span) else {
                    return Some(self.tcx.error_ty());
                };
                let Some(count) = self.swizzle_indices(&args[1], source * 2) else {
                    return Some(self.tcx.error_ty());
                };
                self.checked_simd_ty(elem, crate::ArrayLen::Concrete(count), span)
            }
            ("interleave", 1) => {
                let got = self.check_expr_expecting(&args[0], Expectation::HasType(recv));
                self.unify(recv, got, args[0].span);
                if self.concrete_lanes(lanes, span).is_none() {
                    return Some(self.tcx.error_ty());
                }
                self.tcx.intern(TyKind::Tuple(vec![recv, recv]))
            }
            ("swizzle_dyn", 1)
                if matches!(self.tcx.kind(elem_res), Some(TyKind::Int(IntTy::U8))) =>
            {
                let got = self.check_expr_expecting(&args[0], Expectation::HasType(recv));
                self.unify(recv, got, args[0].span);
                recv
            }
            _ => return None,
        })
    }

    /// `store` and `store_prefix`, writing lanes into a sequence.
    fn simd_store_method(&mut self, method: &str, r: SimdReceiver, args: &[Expr]) -> Option<Ty> {
        let SimdReceiver { elem, mask, .. } = r;
        Some(match (method, args.len()) {
            ("store", 2) if !mask => {
                let target = self.check_expr(&args[0]);
                let target = self.infer.resolve(self.tcx, target);
                let writable = match self.tcx.kind(target).cloned() {
                    Some(TyKind::Ref {
                        mutability: crate::Mutbl::Mut,
                        inner,
                    }) => self.simd_window_elem(inner),
                    _ => None,
                };
                match writable {
                    Some(target_elem) => self.unify(elem, target_elem, args[0].span),
                    None => self.emit(
                        TypeError::SimdShape {
                            reason: "`store` writes its lanes through `&mut` a `Vec<T>`, `[T]`, or `[T; N]`"
                                .to_string(),
                        },
                        args[0].span,
                    ),
                }
                let i64_ty = self.tcx.int_ty(IntTy::I64);
                let offset = self.check_expr_expecting(&args[1], Expectation::HasType(i64_ty));
                self.unify(i64_ty, offset, args[1].span);
                self.tcx.unit()
            }
            ("store_prefix", 3) if !mask => {
                let target = self.check_expr(&args[0]);
                let target = self.infer.resolve(self.tcx, target);
                let writable = match self.tcx.kind(target).cloned() {
                    Some(TyKind::Ref {
                        mutability: crate::Mutbl::Mut,
                        inner,
                    }) => self.simd_window_elem(inner),
                    _ => None,
                };
                match writable {
                    Some(target_elem) => self.unify(elem, target_elem, args[0].span),
                    None => self.emit(
                        TypeError::SimdShape {
                            reason: "`store_prefix` writes its lanes through `&mut` a `Vec<T>`, `[T]`, or `[T; N]`"
                                .to_string(),
                        },
                        args[0].span,
                    ),
                }
                let i64_ty = self.tcx.int_ty(IntTy::I64);
                for arg in &args[1..] {
                    let got = self.check_expr_expecting(arg, Expectation::HasType(i64_ty));
                    self.unify(i64_ty, got, arg.span);
                }
                self.tcx.unit()
            }
            _ => return None,
        })
    }

    /// The lane count `lanes` when it is a literal, reporting one an
    /// operation needs but a const generic leaves open.
    fn concrete_lanes(&mut self, lanes: crate::ArrayLen, span: Span) -> Option<usize> {
        match lanes {
            crate::ArrayLen::Concrete(count) => Some(count),
            crate::ArrayLen::Param(_) => {
                self.emit(
                    TypeError::SimdShape {
                        reason: "this operation needs a literal lane count".to_string(),
                    },
                    span,
                );
                None
            }
        }
    }

    /// The count of a swizzle's index list `arg`: an array literal of
    /// integer literals, each below `source`, the lanes it picks from.
    fn swizzle_indices(&mut self, arg: &Expr, source: usize) -> Option<usize> {
        self.check_expr(arg);
        let (ExprKind::FixedArray(ArrayExpr::List(items))
        | ExprKind::Array(ArrayExpr::List(items))) = &arg.kind
        else {
            self.emit(
                TypeError::SimdShape {
                    reason: "a swizzle's lanes are a literal list of indices: `[3, 2, 1, 0]`"
                        .to_string(),
                },
                arg.span,
            );
            return None;
        };
        for item in items {
            let index = match &item.kind {
                ExprKind::Literal(Literal::Int(text)) => text
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
                    .parse::<usize>()
                    .ok(),
                _ => None,
            };
            match index {
                Some(index) if index < source => {}
                Some(index) => {
                    self.emit(
                        TypeError::SimdShape {
                            reason: format!(
                                "lane {index} is past the {source} lanes a swizzle picks from"
                            ),
                        },
                        item.span,
                    );
                    return None;
                }
                None => {
                    self.emit(
                        TypeError::SimdShape {
                            reason: "a swizzle index is an integer literal".to_string(),
                        },
                        item.span,
                    );
                    return None;
                }
            }
        }
        Some(items.len())
    }

    /// The unsigned lane of `elem`'s width: the lane a bit pattern or an
    /// absolute difference is.
    fn unsigned_lane(&mut self, elem: Ty) -> Ty {
        let int = match self.tcx.kind(elem) {
            Some(TyKind::Int(IntTy::I8 | IntTy::U8)) => IntTy::U8,
            Some(TyKind::Int(IntTy::I16 | IntTy::U16)) => IntTy::U16,
            Some(TyKind::Int(IntTy::I32 | IntTy::U32) | TyKind::Float(crate::FloatTy::F32)) => {
                IntTy::U32
            }
            _ => IntTy::U64,
        };
        self.tcx.int_ty(int)
    }

    /// The lane twice as wide as `elem`, keeping its signedness.
    fn widened_lane(&mut self, elem: Ty) -> Option<Ty> {
        Some(match self.tcx.kind(elem)? {
            TyKind::Int(IntTy::I8) => self.tcx.int_ty(IntTy::I16),
            TyKind::Int(IntTy::U8) => self.tcx.int_ty(IntTy::U16),
            TyKind::Int(IntTy::I16) => self.tcx.int_ty(IntTy::I32),
            TyKind::Int(IntTy::U16) => self.tcx.int_ty(IntTy::U32),
            TyKind::Int(IntTy::I32) => self.tcx.int_ty(IntTy::I64),
            TyKind::Int(IntTy::U32) => self.tcx.int_ty(IntTy::U64),
            TyKind::Float(crate::FloatTy::F32) => self.tcx.float_ty(crate::FloatTy::F64),
            _ => return None,
        })
    }

    /// The lane half as wide as `elem`, keeping its signedness.
    fn narrowed_lane(&mut self, elem: Ty) -> Option<Ty> {
        Some(match self.tcx.kind(elem)? {
            TyKind::Int(IntTy::I16) => self.tcx.int_ty(IntTy::I8),
            TyKind::Int(IntTy::U16) => self.tcx.int_ty(IntTy::U8),
            TyKind::Int(IntTy::I32) => self.tcx.int_ty(IntTy::I16),
            TyKind::Int(IntTy::U32) => self.tcx.int_ty(IntTy::U16),
            TyKind::Int(IntTy::I64) => self.tcx.int_ty(IntTy::I32),
            TyKind::Int(IntTy::U64) => self.tcx.int_ty(IntTy::U32),
            TyKind::Float(crate::FloatTy::F64) => self.tcx.float_ty(crate::FloatTy::F32),
            _ => return None,
        })
    }

    /// `Simd<T, N>`, or `Mask<N>` for a lane vector of `bool`.
    pub(super) fn render_simd_ty(&mut self, elem: Ty, lanes: crate::ArrayLen) -> String {
        let count = render_array_len(lanes);
        if matches!(self.tcx.kind(elem), Some(TyKind::Bool)) {
            format!("Mask<{count}>")
        } else {
            format!("Simd<{}, {count}>", self.render_public_ty(elem))
        }
    }

    pub(super) fn collection_call_ret_ty(
        &mut self,
        module: &[&str],
        method: &str,
        args: &[Expr],
        arg_tys: &[Ty],
        expected: Expectation,
        span: Span,
    ) -> Option<Ty> {
        match method {
            "new" => self.collection_ctor_ty(module),
            "with_capacity" => {
                let ty = self.collection_ctor_ty(module)?;
                if matches!(module.last(), Some(&("Vec" | "Map"))) {
                    return Some(ty);
                }
                let owner = module.last().copied().unwrap_or_default().to_string();
                let error = self.unresolved_method(owner, method, ty);
                self.emit(error, span);
                Some(self.tcx.error_ty())
            }
            "from" => {
                self.collection_from_ty(module, *arg_tys.first()?, expected, args.first()?.span)
            }
            _ => None,
        }
    }

    pub(super) fn flag_set_ty(&mut self) -> Ty {
        let def = gossamer_resolve::DefId::local(u32::MAX - 21);
        self.tcx.register_def_name(def, "flag::Set");
        self.tcx.intern(TyKind::Adt {
            def,
            substs: crate::Substs::new(),
        })
    }

    pub(super) fn flag_set_method_ret(&mut self, method: &str, resolved: Ty) -> Option<Ty> {
        let is_flag_set = match self.tcx.kind(resolved) {
            Some(TyKind::Adt { def, .. }) => {
                def.local == u32::MAX - 21 || self.tcx.def_name(*def) == Some("flag::Set")
            }
            _ => false,
        };
        if !is_flag_set || method != "parse" {
            return None;
        }
        let s = self.tcx.string_ty();
        let vec = self.tcx.intern(TyKind::Vec(s));
        let err = self.tcx.dyn_error_ty();
        Some(self.result_adt_ty(vec, err))
    }

    pub(super) fn validate_handle_method_ret(
        &mut self,
        method: &str,
        args: &[Expr],
        arg_tys: &[Ty],
        resolved: Ty,
        span: Span,
    ) -> Option<Ty> {
        let mut resolved = self.infer.resolve(self.tcx, resolved);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        let public_owner = self.render_public_ty(resolved);
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved) else {
            return None;
        };
        let owner = self
            .tcx
            .def_name(*def)
            .map_or_else(|| public_owner.clone(), ToString::to_string);
        let string = self.tcx.string_ty();
        let field_error = self.stdlib_handle_ty(10, "validate::FieldError");
        let owner_key = match public_owner.as_str() {
            "validate::Errors" | "validate::FieldError" => public_owner.as_str(),
            _ => owner.as_str(),
        };
        let (params, ret) = match (owner_key, method) {
            ("Errors" | "validate::Errors", "add") => (vec![string, field_error], self.tcx.unit()),
            ("Errors" | "validate::Errors", "is_empty") => (Vec::new(), self.tcx.bool_ty()),
            ("Errors" | "validate::Errors", "len") => (Vec::new(), self.tcx.int_ty(IntTy::I64)),
            ("Errors" | "validate::Errors", "count") => (vec![string], self.tcx.int_ty(IntTy::I64)),
            ("Errors" | "validate::Errors", "get") => (vec![string], string),
            ("Errors" | "validate::Errors", "collect") => (Vec::new(), string),
            ("FieldError" | "validate::FieldError", "path" | "message" | "code") => {
                (Vec::new(), string)
            }
            _ => return None,
        };
        if args.len() != params.len() {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: format!("{owner}::{method}"),
                    expected: params.len(),
                    found: args.len(),
                },
                span,
            );
            return Some(self.tcx.error_ty());
        }
        for (param, (arg_ty, arg)) in params.iter().zip(arg_tys.iter().zip(args)) {
            self.check_sig_param_arg(*param, *arg_ty, arg);
        }
        Some(ret)
    }

    pub(super) fn stdlib_handle_ty(&mut self, offset: u32, name: &str) -> Ty {
        let def = gossamer_resolve::DefId::local(u32::MAX - offset);
        self.tcx.register_def_name(def, name);
        self.tcx.intern(TyKind::Adt {
            def,
            substs: crate::Substs::new(),
        })
    }

    /// Names the value class when `ty` has no textual form, so a format
    /// macro over it is refused here rather than rendering a pointer whose
    /// bits differ on every run.
    /// Types `x.to_string()` as the rendering `{}` gives the same value, for
    /// any receiver that has one. A `String` is already its own text, and a
    /// handle, a callable, or a concurrency type has no rendering, so both
    /// keep whatever surface declares the name for them.
    pub(super) fn check_display_to_string(
        &mut self,
        method: &str,
        resolved: Ty,
        args: &[Expr],
    ) -> Option<Ty> {
        (method == "to_string"
            && args.is_empty()
            && !matches!(self.tcx.kind(resolved), Some(TyKind::String))
            && self.is_displayable_value(resolved))
        .then(|| self.tcx.string_ty())
    }

    /// Whether `ty` is a value with a rendering: what `{}` accepts, minus the
    /// lazy cursors, which stand for a sequence rather than holding one.
    pub(super) fn is_displayable_value(&mut self, ty: Ty) -> bool {
        let peeled = self.peel_refs(ty);
        !matches!(
            self.tcx.kind(peeled),
            Some(TyKind::Iterator(_) | TyKind::Range(_))
        ) && self.not_displayable(ty).is_none()
    }

    pub(super) fn not_displayable(
        &mut self,
        ty: Ty,
    ) -> Option<(String, crate::NotDisplayableClass)> {
        use crate::NotDisplayableClass as Class;
        let peeled = self.peel_refs(ty);
        match self.tcx.kind(peeled)? {
            TyKind::Adt { def, .. } if is_opaque_handle_def(def.local) => Some((
                self.tcx
                    .def_name(*def)
                    .map_or_else(|| crate::render_ty(self.tcx, peeled), ToString::to_string),
                Class::Handle,
            )),
            TyKind::FnDef { .. } | TyKind::FnPtr(_) => {
                Some(("function".to_string(), Class::Callable))
            }
            TyKind::FnTrait(_) | TyKind::Closure { .. } => {
                Some(("closure".to_string(), Class::Callable))
            }
            TyKind::Sender(_) | TyKind::Receiver(_) | TyKind::JoinHandle(_) => {
                Some((crate::render_ty(self.tcx, peeled), Class::Concurrency))
            }
            _ => None,
        }
    }

    /// Handle type a stdlib constructor or middleware wrapper yields.
    ///
    /// Every wrapping `middleware::<name>(inner, ..)` composes the same
    /// handler handle, whatever shape the inner handler has: the catalogue
    /// spells that slot `T` (any handler) or `http::Handler`, while the
    /// sibling helpers returning `bool` / `String` keep their catalogue
    /// types.
    pub(super) fn handle_call_ret_ty(&mut self, module: &[&str], last: &str) -> Option<Ty> {
        if let Some((offset, handle)) = stdlib_handle_ctor(module, last) {
            return Some(self.stdlib_handle_ty(offset, handle));
        }

        // A socket constructor answers its handle through a `Result`, so
        // `TcpStream::connect(addr)?` propagates like any fallible call.
        if let Some((offset, handle)) = net_socket_ctor(module, last) {
            let socket = self.stdlib_handle_ty(offset, handle);
            return Some(self.fallible(socket));
        }
        // Same shape for the streaming filesystem handle: opening a file
        // can fail, so `fs::File::open(p)?` propagates.
        if fs_file_ctor(module, last) {
            let file = self.stdlib_handle_ty(44, "fs::File");
            return Some(self.fallible(file));
        }
        // A pattern built at run time can be refused; a `regex::compile`
        // literal was checked while parsing, so it answers the pattern.
        match (module.strip_prefix(&["std"]).unwrap_or(module), last) {
            (["regex"], "new") => {
                let pattern = self.stdlib_handle_ty(26, "regex::Pattern");
                return Some(self.fallible(pattern));
            }
            (["regex"], "compile") => return Some(self.stdlib_handle_ty(26, "regex::Pattern")),
            _ => {}
        }
        let is_middleware = matches!(
            module,
            ["middleware"] | ["http", "middleware"] | ["std", "http", "middleware"]
        );
        if is_middleware
            && crate::stdlib_signatures::function_shape_for_path(module, last)
                .is_some_and(|shape| matches!(shape.return_ty.trim(), "T" | "http::Handler"))
        {
            return Some(self.http_handler_ty());
        }
        None
    }

    /// The handle every `middleware::*` wrapper composes and returns.
    pub(super) fn http_handler_ty(&mut self) -> Ty {
        self.stdlib_handle_ty(PURE_HANDLE_HI_OFFSET, "http::Handler")
    }

    pub(super) fn http_response_ty(&mut self) -> Ty {
        self.stdlib_handle_ty(5, "http::Response")
    }

    pub(super) fn http_client_ty(&mut self) -> Ty {
        self.stdlib_handle_ty(22, "http::Client")
    }

    pub(super) fn http_client_builder_ty(&mut self) -> Ty {
        self.stdlib_handle_ty(23, "http::ClientBuilder")
    }

    pub(super) fn http_request_ty(&mut self) -> Ty {
        self.stdlib_handle_ty(24, "http::Request")
    }

    pub(super) fn io_stream_ty(&mut self) -> Ty {
        self.stdlib_handle_ty(25, "io::Stream")
    }

    pub(super) fn bytes_handle_ty(&mut self, name: &str) -> Ty {
        let offset = if name == "bytes::Buffer" {
            super::BYTES_BUFFER_OFFSET
        } else {
            27
        };
        self.stdlib_handle_ty(offset, name)
    }

    /// Return type of a method on one of the opaque `std::net` socket
    /// handles. Without a row here the call answers a fresh variable, so
    /// `?` on `sock.read(n)` reports the operand is not a `Result`.
    ///
    /// The table is the handle's complete surface, so a name absent from
    /// it is reported here. Letting it through typed the call as a fresh
    /// variable and reached the compiled tier as an undefined `@method`
    /// symbol - a link failure with no source location, after `gos check`
    /// had said the program was fine.
    pub(super) fn net_handle_method_ret(
        &mut self,
        method: &str,
        resolved: Ty,
        arg_count: usize,
        span: Span,
    ) -> Option<Ty> {
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved) else {
            return None;
        };
        let owner = self.tcx.def_name(*def)?.to_string();
        if !matches!(
            owner.as_str(),
            "net::TcpStream"
                | "net::UnixStream"
                | "net::TcpListener"
                | "net::UnixListener"
                | "net::UdpSocket"
        ) {
            return None;
        }
        let unit = self.tcx.unit();
        match (owner.as_str(), method) {
            (
                "net::TcpStream" | "net::UnixStream" | "net::TcpListener" | "net::UnixListener"
                | "net::UdpSocket",
                "close",
            )
            | (
                "net::TcpStream",
                "set_read_timeout_ms"
                | "set_write_timeout_ms"
                | "set_nodelay"
                | "clear_read_timeout"
                | "clear_write_timeout",
            ) => Some(unit),
            ("net::TcpStream" | "net::UnixStream", "read") => {
                let bytes = self.byte_vec_ty();
                Some(self.fallible(bytes))
            }
            // Answers the byte count it wrote into the caller's buffer.
            ("net::TcpStream", "read_into") => {
                let count = self.tcx.int_ty(IntTy::I64);
                Some(self.fallible(count))
            }
            ("net::TcpStream" | "net::UnixStream", "read_to_string")
            | ("net::TcpListener" | "net::UdpSocket", "local_addr") => {
                let string = self.tcx.string_ty();
                Some(self.fallible(string))
            }
            ("net::TcpStream" | "net::UnixStream", "write" | "write_all")
            | ("net::UdpSocket", "send_to") => Some(self.fallible(unit)),
            ("net::TcpStream", "start_tls" | "start_tls_ca" | "start_tls_insecure") => {
                let handle = self.stdlib_handle_ty(12, "net::TcpStream");
                Some(self.fallible(handle))
            }
            // The peer's certificate is bytes, not a handle: what a caller
            // does with it - hash it for a channel binding, read its
            // fields - is its own business.
            ("net::TcpStream", "peer_certificate") => {
                let byte = self.tcx.int_ty(IntTy::U8);
                Some(self.tcx.intern(TyKind::Vec(byte)))
            }
            ("net::TcpListener", "accept") => {
                let stream = self.stdlib_handle_ty(12, "net::TcpStream");
                let string = self.tcx.string_ty();
                let pair = self.tcx.intern(TyKind::Tuple(vec![stream, string]));
                Some(self.fallible(pair))
            }
            ("net::UnixListener", "accept") => {
                let stream = self.stdlib_handle_ty(15, "net::UnixStream");
                let string = self.tcx.string_ty();
                let pair = self.tcx.intern(TyKind::Tuple(vec![stream, string]));
                Some(self.fallible(pair))
            }
            ("net::UdpSocket", "recv_from") => {
                let bytes = self.byte_vec_ty();
                let string = self.tcx.string_ty();
                let pair = self.tcx.intern(TyKind::Tuple(vec![bytes, string]));
                Some(self.fallible(pair))
            }
            // `clone` is the language's own copy, answered for every value.
            (_, "clone") => Some(resolved),
            _ => {
                let error = self.unresolved_method_call(owner.clone(), method, resolved, arg_count);
                self.emit(error, span);
                Some(self.tcx.error_ty())
            }
        }
    }

    /// `Vec<u8>`, the shape every socket read answers.
    pub(super) fn byte_vec_ty(&mut self) -> Ty {
        let u8_ty = self.tcx.int_ty(IntTy::U8);
        self.tcx.intern(TyKind::Vec(u8_ty))
    }

    /// `Result<ok, errors::Error>` - the stdlib's fallible answer shape.
    pub(super) fn fallible(&mut self, ok: Ty) -> Ty {
        let err = self.tcx.dyn_error_ty();
        self.result_adt_ty(ok, err)
    }

    /// Return type of a method on the streaming filesystem handles, with
    /// the parameter list each one declares.
    ///
    /// Without a contract here every `f.write(..)` typed as a fresh
    /// variable: a `Vec<u8>` passed to the text `write` reached the
    /// runtime as an argument-shape error rather than a type error, and
    /// `?` on a fallible call saw no `Result` at all.
    pub(super) fn fs_handle_method_ret(
        &mut self,
        method: &str,
        args: &[Expr],
        arg_tys: &[Ty],
        resolved: Ty,
        span: Span,
    ) -> Option<Ty> {
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved) else {
            return None;
        };
        let owner = self.tcx.def_name(*def)?.to_string();
        if !matches!(owner.as_str(), "fs::File" | "fs::OpenOptions") {
            return None;
        }
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        let string = self.tcx.string_ty();
        let unit = self.tcx.unit();
        let bytes = self.byte_vec_ty();
        let (params, ret) = match (owner.as_str(), method) {
            ("fs::File", "read") => (vec![i64_ty], self.fallible(bytes)),
            ("fs::File", "read_at") => (vec![i64_ty, i64_ty], self.fallible(bytes)),
            ("fs::File", "read_at_into") => {
                let buf = self.tcx.intern(TyKind::Ref {
                    mutability: Mutbl::Mut,
                    inner: bytes,
                });
                (vec![buf, i64_ty, i64_ty], self.fallible(i64_ty))
            }
            ("fs::File", "read_to_string") => (vec![], self.fallible(string)),
            ("fs::File", "write" | "write_all") => (vec![string], self.fallible(i64_ty)),
            ("fs::File", "write_bytes") => (vec![bytes], self.fallible(i64_ty)),
            ("fs::File", "write_at") => (vec![bytes, i64_ty], self.fallible(i64_ty)),
            ("fs::File", "seek") => (vec![i64_ty, i64_ty], self.fallible(i64_ty)),
            ("fs::File", "set_len") => (vec![i64_ty], self.fallible(unit)),
            ("fs::File", "len" | "fd") => (vec![], self.fallible(i64_ty)),
            ("fs::File", "flush" | "sync_all" | "sync_data") => (vec![], self.fallible(unit)),
            ("fs::File", "try_lock_range") => {
                (vec![i64_ty, i64_ty, bool_ty], self.fallible(bool_ty))
            }
            ("fs::File", "unlock_range") => (vec![i64_ty, i64_ty], self.fallible(unit)),
            ("fs::File", "try_lock_shared" | "try_lock_exclusive") => {
                (vec![], self.fallible(bool_ty))
            }
            ("fs::File", "unlock") => (vec![], self.fallible(unit)),
            ("fs::File", "close") => (vec![], unit),
            (
                "fs::OpenOptions",
                "read" | "write" | "append" | "truncate" | "create" | "create_new",
            ) => {
                let opts = self.stdlib_handle_ty(45, "fs::OpenOptions");
                (vec![bool_ty], opts)
            }
            ("fs::OpenOptions", "open") => {
                let file = self.stdlib_handle_ty(44, "fs::File");
                (vec![string], self.fallible(file))
            }
            _ => {
                let error = self.unresolved_method_call(owner, method, resolved, args.len());
                self.emit(error, span);
                return Some(self.tcx.error_ty());
            }
        };
        if args.len() != params.len() {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: format!("{owner}::{method}"),
                    expected: params.len(),
                    found: args.len(),
                },
                span,
            );
            return Some(self.tcx.error_ty());
        }
        for (param, (arg_ty, arg)) in params.iter().zip(arg_tys.iter().zip(args)) {
            self.check_expected_integer_literal_range(arg, Expectation::HasType(*param), *arg_ty);
            self.check_sig_param_arg(*param, *arg_ty, arg);
        }
        Some(ret)
    }

    /// Types a method on a runtime handle, whichever family owns it.
    ///
    /// Each family answers `None` for a receiver it does not own, so the
    /// order here is immaterial and adding a handle family costs one line
    /// rather than another branch in the method-call checker.
    pub(super) fn handle_family_method_ret(
        &mut self,
        method: &str,
        args: &[Expr],
        arg_tys: &[Ty],
        resolved: Ty,
        receiver: &Expr,
    ) -> Option<Ty> {
        let span = receiver.span;
        self.bytes_handle_method_ret(method, args, arg_tys, resolved, span)
            .or_else(|| self.regex_handle_method_ret(method, args, arg_tys, resolved, span))
            .or_else(|| self.fs_handle_method_ret(method, args, arg_tys, resolved, span))
            .or_else(|| self.sync_handle_method_ret(method, args, arg_tys, resolved, span))
            .or_else(|| self.heap_buffer_method_ret(method, args, arg_tys, resolved, span))
            .or_else(|| self.table_handle_method_ret(method, args, arg_tys, resolved, span))
    }

    pub(super) fn shape_ty(&mut self, shape: Shape) -> Ty {
        match shape {
            Shape::Unit => self.tcx.unit(),
            Shape::Bool => self.tcx.bool_ty(),
            Shape::I64 => self.tcx.int_ty(IntTy::I64),
            Shape::U64 => self.tcx.int_ty(IntTy::U64),
            Shape::U32 => self.tcx.int_ty(IntTy::U32),
            Shape::F64 => self.tcx.float_ty(FloatTy::F64),
            Shape::Str => self.tcx.string_ty(),
            Shape::StrVec => {
                let s = self.tcx.string_ty();
                self.tcx.intern(TyKind::Vec(s))
            }
            Shape::F64Vec => {
                let f = self.tcx.float_ty(FloatTy::F64);
                self.tcx.intern(TyKind::Vec(f))
            }
            Shape::OptStr => {
                let s = self.tcx.string_ty();
                self.option_adt_ty(s)
            }
            Shape::MutStr => {
                let s = self.tcx.string_ty();
                self.tcx.intern(TyKind::Ref {
                    mutability: Mutbl::Mut,
                    inner: s,
                })
            }
            Shape::ResultI64 => {
                let i = self.tcx.int_ty(IntTy::I64);
                let err = self.tcx.dyn_error_ty();
                self.result_adt_ty(i, err)
            }
            Shape::DoneChannel => {
                let i = self.tcx.int_ty(IntTy::I64);
                self.tcx.intern(TyKind::Receiver(i))
            }
            Shape::Handle(offset, name) => self.stdlib_handle_ty(offset, name),
            // Checked against the three instruments by `check_metric_arg`.
            Shape::Metric => self.fresh(),
        }
    }

    /// Reports an argument to `Registry::register` that is not one of the
    /// `metrics` instruments.
    pub(super) fn check_metric_arg(&mut self, arg_ty: Ty, arg: &Expr) {
        let resolved = self.infer.resolve(self.tcx, arg_ty);
        let numeric = self.infer.is_integer_constrained_var(self.tcx, resolved)
            || self.infer.is_float_literal_var(self.tcx, resolved);
        let name = match self.tcx.kind(resolved) {
            Some(TyKind::Adt { def, .. }) => self.tcx.def_name(*def),
            Some(TyKind::Var(_)) if numeric => None,
            Some(TyKind::Var(_) | TyKind::Error) | None => return,
            Some(_) => None,
        };
        if !matches!(
            name,
            Some("metrics::Counter" | "metrics::Gauge" | "metrics::Histogram")
        ) {
            self.emit(
                TypeError::TypeMismatch {
                    expected: "metrics::Counter | metrics::Gauge | metrics::Histogram".to_string(),
                    found: crate::render_ty(self.tcx, resolved),
                },
                arg.span,
            );
        }
    }

    /// Checks a call against a [`HANDLE_METHODS`] row and answers its type.
    pub(super) fn check_table_row(
        &mut self,
        callee: &str,
        params: &[Shape],
        ret: Shape,
        (args, arg_tys): (&[Expr], &[Ty]),
        span: Span,
    ) -> Ty {
        if args.len() != params.len() {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: callee.to_string(),
                    expected: params.len(),
                    found: args.len(),
                },
                span,
            );
            return self.tcx.error_ty();
        }
        for (param, (arg_ty, arg)) in params.iter().zip(arg_tys.iter().zip(args)) {
            if matches!(param, Shape::Metric) {
                self.check_metric_arg(*arg_ty, arg);
                continue;
            }
            let param = self.shape_ty(*param);
            self.check_expected_integer_literal_range(arg, Expectation::HasType(param), *arg_ty);
            self.check_sig_param_arg(param, *arg_ty, arg);
        }
        self.shape_ty(ret)
    }

    /// Types a method on a handle [`HANDLE_METHODS`] describes.
    pub(super) fn table_handle_method_ret(
        &mut self,
        method: &str,
        args: &[Expr],
        arg_tys: &[Ty],
        resolved: Ty,
        span: Span,
    ) -> Option<Ty> {
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved) else {
            return None;
        };
        if def.local < u32::MAX - HANDLE_SENTINEL_SPAN {
            return None;
        }
        let owner = self.tcx.def_name(*def)?;
        if !HANDLE_METHODS.iter().any(|(o, ..)| *o == owner) {
            return None;
        }
        let owner = owner.to_string();
        if method == "clone" && args.is_empty() {
            return Some(resolved);
        }
        // A method may be written with more than one arity (`read_line()`
        // answers the next line, `read_line(&mut buf)` appends to one), so
        // the row whose parameters match the call answers first.
        let rows = || {
            HANDLE_METHODS
                .iter()
                .filter(|(o, m, ..)| *o == owner && *m == method && *m != "new")
                .filter(|(_, m, ..)| !matches!(*m, "background" | "with_cancel" | "with_timeout"))
        };
        let row = rows()
            .find(|(_, _, params, _)| params.len() == args.len())
            .or_else(|| rows().next());
        let Some((_, _, params, ret)) = row else {
            let error = self.unresolved_method_call(owner, method, resolved, args.len());
            self.emit(error, span);
            return Some(self.tcx.error_ty());
        };
        let callee = format!("{owner}::{method}");
        Some(self.check_table_row(&callee, params, *ret, (args, arg_tys), span))
    }

    /// Types a call written on the path of a handle [`HANDLE_METHODS`]
    /// describes: a constructor, or a method in its qualified form with the
    /// handle as the first argument.
    pub(super) fn table_handle_call_ret_ty(
        &mut self,
        module: &[&str],
        last: &str,
        args: &[Expr],
        arg_tys: &[Ty],
        span: Span,
    ) -> Option<Ty> {
        let owner = table_handle_owner(module)?;
        let Some((_, _, params, ret)) = HANDLE_METHODS
            .iter()
            .find(|(o, m, ..)| *o == owner && *m == last)
        else {
            let handle = self.fresh();
            let error = self.unresolved_method_call(owner.to_string(), last, handle, args.len());
            self.emit(error, span);
            return Some(self.tcx.error_ty());
        };
        let is_ctor = matches!(last, "new" | "background" | "with_cancel" | "with_timeout");
        if is_ctor {
            let callee = format!("{owner}::{last}");
            return Some(self.check_table_row(&callee, params, *ret, (args, arg_tys), span));
        }
        let (Some(receiver_ty), Some(receiver)) = (arg_tys.first(), args.first()) else {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: format!("{owner}::{last}"),
                    expected: params.len() + 1,
                    found: 0,
                },
                span,
            );
            return Some(self.tcx.error_ty());
        };
        let offset = HANDLE_METHODS.iter().find_map(|(_, _, _, ret)| match ret {
            Shape::Handle(offset, name) if *name == owner => Some(*offset),
            _ => None,
        })?;
        let handle = self.stdlib_handle_ty(offset, owner);
        self.check_sig_param_arg(handle, *receiver_ty, receiver);
        self.table_handle_method_ret(last, &args[1..], &arg_tys[1..], handle, span)
    }

    /// Types `I64Vec::new(len)` and `U8Vec::new(len)`, the shared word and
    /// byte buffers.
    pub(super) fn heap_buffer_call_ret_ty(
        &mut self,
        module: &[&str],
        last: &str,
        args: &[Expr],
        arg_tys: &[Ty],
        span: Span,
    ) -> Option<Ty> {
        let (offset, owner) = match module {
            ["I64Vec"] => (SYNC_HANDLE_HI_OFFSET, "I64Vec"),
            ["U8Vec"] => (U8_VEC_OFFSET, "U8Vec"),
            _ => return None,
        };
        if last != "new" {
            return None;
        }
        let handle = self.stdlib_handle_ty(offset, owner);
        let len = vec![self.tcx.int_ty(IntTy::I64)];
        let ok = self.check_sync_args(&format!("{owner}::new"), &len, args, arg_tys, span);
        Some(if ok { handle } else { self.tcx.error_ty() })
    }

    /// Types a method on the `I64Vec` word buffer or the `U8Vec` byte
    /// buffer. Positions, lengths, and stored values are all `i64` words.
    pub(super) fn heap_buffer_method_ret(
        &mut self,
        method: &str,
        args: &[Expr],
        arg_tys: &[Ty],
        resolved: Ty,
        span: Span,
    ) -> Option<Ty> {
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved) else {
            return None;
        };
        let offset = u32::MAX - def.local;
        let owner = match offset {
            SYNC_HANDLE_HI_OFFSET => "I64Vec",
            U8_VEC_OFFSET => "U8Vec",
            _ => return None,
        };
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit = self.tcx.unit();
        let (params, ret) = match (owner, method) {
            (_, "clone") => (Vec::new(), resolved),
            ("I64Vec", "set_at") | ("U8Vec", "set_byte") => (vec![i64_ty, i64_ty], unit),
            ("I64Vec", "get_at") | ("U8Vec", "get_byte") => (vec![i64_ty], i64_ty),
            ("I64Vec", "vec_len") | ("U8Vec", "byte_len") => (Vec::new(), i64_ty),
            ("I64Vec", "write_range_to_stdout") | ("U8Vec", "write_byte_range_to_stdout") => {
                (vec![i64_ty, i64_ty], unit)
            }
            ("I64Vec", "write_lines_to_stdout") | ("U8Vec", "write_byte_lines_to_stdout") => {
                (vec![i64_ty, i64_ty, i64_ty], unit)
            }
            ("U8Vec", "window_key") => (vec![i64_ty, i64_ty], i64_ty),
            ("U8Vec", "count_singles" | "count_pairs") => {
                (vec![i64_ty], self.tcx.intern(TyKind::Vec(i64_ty)))
            }
            ("U8Vec", "count_kmers") => {
                let counts = self.tcx.intern(TyKind::HashMap {
                    key: i64_ty,
                    value: i64_ty,
                    ordered: false,
                });
                (vec![i64_ty, i64_ty], counts)
            }
            ("U8Vec", "to_string") => (vec![i64_ty], self.tcx.string_ty()),
            _ => {
                let error =
                    self.unresolved_method_call(owner.to_string(), method, resolved, args.len());
                self.emit(error, span);
                return Some(self.tcx.error_ty());
            }
        };
        let callee = format!("{owner}::{method}");
        let ok = self.check_sync_args(&callee, &params, args, arg_tys, span);
        Some(if ok { ret } else { self.tcx.error_ty() })
    }

    /// `(sentinel offset, display name)` of the `std::sync` handle a
    /// type-qualified path names: `sync::Mutex`, `std::sync::Mutex`, or the
    /// bare `Mutex` the prelude reaches.
    pub(super) fn sync_handle_of_path(module: &[&str]) -> Option<(u32, &'static str)> {
        let tail = match module {
            ["std", "sync", tail] | ["sync", tail] | [tail] => *tail,
            _ => return None,
        };
        PURE_HANDLES
            .iter()
            .find(|(offset, name, _)| {
                (*offset == 35 || (SYNC_HANDLE_LO_OFFSET..=SYNC_HANDLE_HI_OFFSET).contains(offset))
                    && name.strip_prefix("sync::") == Some(tail)
            })
            .map(|(offset, name, _)| (*offset, *name))
    }

    /// Types a call written on a `std::sync` type path: the constructor
    /// `T::new(..)`, or a method in its qualified form `T::load(handle)`,
    /// which is typed from the same table as the method with the handle as
    /// its receiver.
    pub(super) fn sync_call_ret_ty(
        &mut self,
        module: &[&str],
        last: &str,
        args: &[Expr],
        arg_tys: &[Ty],
        span: Span,
    ) -> Option<Ty> {
        let (offset, owner) = Self::sync_handle_of_path(module)?;
        let handle = self.stdlib_handle_ty(offset, owner);
        if last != "new" {
            let (Some(receiver_ty), Some(receiver)) = (arg_tys.first(), args.first()) else {
                self.emit(
                    TypeError::CallArityMismatch {
                        callee: format!("{owner}::{last}"),
                        expected: 1,
                        found: 0,
                    },
                    span,
                );
                return Some(self.tcx.error_ty());
            };
            self.check_sig_param_arg(handle, *receiver_ty, receiver);
            return self.sync_handle_method_ret(last, &args[1..], &arg_tys[1..], handle, span);
        }
        let params = match owner {
            "sync::RwLock" | "sync::Barrier" | "sync::AtomicI64" => {
                vec![self.tcx.int_ty(IntTy::I64)]
            }
            "sync::AtomicI32" => vec![self.tcx.int_ty(IntTy::I32)],
            "sync::AtomicU64" => vec![self.tcx.int_ty(IntTy::U64)],
            "sync::AtomicBool" => vec![self.tcx.bool_ty()],
            _ => Vec::new(),
        };
        self.check_sync_args(&format!("{owner}::new"), &params, args, arg_tys, span)
            .then_some(handle)
            .or_else(|| Some(self.tcx.error_ty()))
    }

    /// Checks a `std::sync` call's arguments against its parameter list,
    /// reporting an arity mismatch; answers whether the arity matched.
    pub(super) fn check_sync_args(
        &mut self,
        callee: &str,
        params: &[Ty],
        args: &[Expr],
        arg_tys: &[Ty],
        span: Span,
    ) -> bool {
        if args.len() != params.len() {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: callee.to_string(),
                    expected: params.len(),
                    found: args.len(),
                },
                span,
            );
            return false;
        }
        for (param, (arg_ty, arg)) in params.iter().zip(arg_tys.iter().zip(args)) {
            self.check_expected_integer_literal_range(arg, Expectation::HasType(*param), *arg_ty);
            self.check_sig_param_arg(*param, *arg_ty, arg);
        }
        true
    }

    /// Types a method on a `std::sync` handle. The table is the handle's
    /// whole surface on every tier: an atomic holds a word of its own
    /// width, and an `RwLock` guards an `i64`, the one word its compiled
    /// form stores.
    pub(super) fn sync_handle_method_ret(
        &mut self,
        method: &str,
        args: &[Expr],
        arg_tys: &[Ty],
        resolved: Ty,
        span: Span,
    ) -> Option<Ty> {
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved) else {
            return None;
        };
        let offset = u32::MAX - def.local;
        if offset != 35 && !(SYNC_HANDLE_LO_OFFSET..SYNC_HANDLE_HI_OFFSET).contains(&offset) {
            return None;
        }
        let owner = self.tcx.def_name(*def)?.to_string();
        let unit = self.tcx.unit();
        let bool_ty = self.tcx.bool_ty();
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let word = match owner.as_str() {
            "sync::AtomicI32" => Some(self.tcx.int_ty(IntTy::I32)),
            "sync::AtomicU64" => Some(self.tcx.int_ty(IntTy::U64)),
            "sync::AtomicI64" => Some(i64_ty),
            "sync::AtomicBool" => Some(bool_ty),
            _ => None,
        };
        let (params, ret) = match (owner.as_str(), method) {
            (_, "clone") => (Vec::new(), resolved),
            ("sync::Mutex", "lock" | "unlock")
            | ("sync::WaitGroup", "done" | "wait")
            | ("sync::Barrier", "wait") => (Vec::new(), unit),
            ("sync::WaitGroup", "add") => (vec![i64_ty], unit),
            ("sync::WaitGroup", "wait_ctx") => {
                let context = self.stdlib_handle_ty(11, "context::Context");
                (vec![context], bool_ty)
            }
            ("sync::Once", "call") => {
                let body = FnSig {
                    inputs: Vec::new(),
                    output: self.fresh(),
                };
                (vec![self.tcx.intern(TyKind::FnTrait(body))], bool_ty)
            }
            ("sync::RwLock", "read") => (Vec::new(), i64_ty),
            ("sync::RwLock", "write") => (vec![i64_ty], unit),
            ("sync::RwLock", "with_read" | "with_write") => {
                let body = FnSig {
                    inputs: vec![i64_ty],
                    output: i64_ty,
                };
                (vec![self.tcx.intern(TyKind::FnTrait(body))], i64_ty)
            }
            _ => {
                let Some(signature) =
                    word.and_then(|word| atomic_method(method, word, bool_ty, unit))
                else {
                    let error = self.unresolved_method_call(owner, method, resolved, args.len());
                    self.emit(error, span);
                    return Some(self.tcx.error_ty());
                };
                signature
            }
        };
        let callee = format!("{owner}::{method}");
        if self.check_sync_args(&callee, &params, args, arg_tys, span) {
            Some(ret)
        } else {
            Some(self.tcx.error_ty())
        }
    }

    /// Types a `regex::Pattern` method.
    ///
    /// Every signature here mirrors the free form the stdlib manifest
    /// declares, with the pattern as the receiver instead of the first
    /// argument. Without it the receiver stays an inference variable, so
    /// `println!("{}", pattern)` passes `gos check` and then renders a
    /// struct on the VM and a raw pointer natively.
    pub(super) fn regex_handle_method_ret(
        &mut self,
        method: &str,
        args: &[Expr],
        arg_tys: &[Ty],
        resolved: Ty,
        span: Span,
    ) -> Option<Ty> {
        // A handle is conventionally written `&regex::Pattern` where it is
        // a parameter, and the methods take it either way.
        let resolved = self.peel_refs(resolved);
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved) else {
            return None;
        };
        let owner = self.tcx.def_name(*def)?.to_string();
        if owner != "regex::Pattern" {
            return None;
        }
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        let string = self.tcx.string_ty();
        let strings = self.tcx.intern(TyKind::Vec(string));
        // `find` answers the match's start, end, and text.
        let span_ty = self.tcx.intern(TyKind::Tuple(vec![i64_ty, i64_ty, string]));
        let spans = self.tcx.intern(TyKind::Vec(span_ty));
        // A capture group that did not participate has no text.
        let maybe_string = self.option_adt_ty(string);
        let groups = self.tcx.intern(TyKind::Vec(maybe_string));
        let all_groups = self.tcx.intern(TyKind::Vec(groups));
        let (params, ret) = match method {
            "is_match" => (vec![string], bool_ty),
            "find" => (vec![string], self.option_adt_ty(span_ty)),
            "find_all" => (vec![string], spans),
            "count" => (vec![string], self.tcx.int_ty(IntTy::I64)),
            "captures" => (vec![string], self.option_adt_ty(groups)),
            "captures_all" => (vec![string], all_groups),
            "replace" | "replace_all" => (vec![string, string], string),
            "split" => (vec![string], strings),
            _ => {
                let error =
                    self.unresolved_method_call(owner.clone(), method, resolved, args.len());
                self.emit(error, span);
                return Some(self.tcx.error_ty());
            }
        };
        if args.len() != params.len() {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: format!("{owner}::{method}"),
                    expected: params.len(),
                    found: args.len(),
                },
                span,
            );
            return Some(self.tcx.error_ty());
        }
        for (param, (arg_ty, arg)) in params.iter().zip(arg_tys.iter().zip(args)) {
            self.check_expected_integer_literal_range(arg, Expectation::HasType(*param), *arg_ty);
            self.check_sig_param_arg(*param, *arg_ty, arg);
        }
        Some(ret)
    }

    pub(super) fn bytes_handle_method_ret(
        &mut self,
        method: &str,
        args: &[Expr],
        arg_tys: &[Ty],
        resolved: Ty,
        span: Span,
    ) -> Option<Ty> {
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved) else {
            return None;
        };
        let owner = self.tcx.def_name(*def)?.to_string();
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let string = self.tcx.string_ty();
        let (params, ret) = match (owner.as_str(), method) {
            ("bytes::Buffer", "push") => (vec![self.tcx.int_ty(IntTy::U8)], self.tcx.unit()),
            ("bytes::Buffer", "write_str") => (vec![string], self.tcx.unit()),
            ("bytes::Buffer", "clear") => (vec![], self.tcx.unit()),
            ("bytes::Buffer", "len") => (vec![], i64_ty),
            ("bytes::Buffer", "is_empty") => (vec![], self.tcx.bool_ty()),
            ("bytes::Buffer", "to_string") => (vec![], string),
            ("bytes::Builder", "write") => (vec![string], self.tcx.unit()),
            ("bytes::Builder", "write_char") => {
                (vec![self.tcx.intern(TyKind::Char)], self.tcx.unit())
            }
            ("bytes::Builder", "len") => (vec![], i64_ty),
            ("bytes::Builder", "build" | "as_str") => (vec![], string),
            _ => return None,
        };
        if args.len() != params.len() {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: format!("{owner}::{method}"),
                    expected: params.len(),
                    found: args.len(),
                },
                span,
            );
            return Some(self.tcx.error_ty());
        }
        for (param, (arg_ty, arg)) in params.iter().zip(arg_tys.iter().zip(args)) {
            self.check_expected_integer_literal_range(arg, Expectation::HasType(*param), *arg_ty);
            self.check_sig_param_arg(*param, *arg_ty, arg);
        }
        Some(ret)
    }

    pub(super) fn result_response_error_ty(&mut self) -> Ty {
        let resp = self.http_response_ty();
        let err = self.tcx.dyn_error_ty();
        self.result_adt_ty(resp, err)
    }

    pub(super) fn http_client_method_ret(&mut self, method: &str, resolved: Ty) -> Option<Ty> {
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved) else {
            return None;
        };
        match self.tcx.def_name(*def) {
            Some("http::Client") => match method {
                "get" | "post" | "put" | "options" | "delete" | "head" => {
                    Some(self.http_request_ty())
                }
                "request" | "request_bytes" => Some(self.result_response_error_ty()),
                _ => None,
            },
            Some("http::ClientBuilder") => match method {
                "max_redirects" | "timeout_ms" | "cookie_jar" | "proxy" => {
                    Some(self.http_client_builder_ty())
                }
                "build" => Some(self.http_client_ty()),
                _ => None,
            },
            Some("http::Request") => match method {
                "header" | "body" | "set_value" => Some(self.http_request_ty()),
                "send" => Some(self.result_response_error_ty()),
                "path" | "path_value" | "method" | "value" | "form_value" => {
                    Some(self.tcx.string_ty())
                }
                "path_int" => {
                    let i = self.tcx.int_ty(IntTy::I64);
                    Some(self.option_adt_ty(i))
                }
                "path_float" => {
                    let f = self.tcx.float_ty(FloatTy::F64);
                    Some(self.option_adt_ty(f))
                }
                "basic_auth" => {
                    let s = self.tcx.string_ty();
                    let pair = self.tcx.intern(TyKind::Tuple(vec![s, s]));
                    Some(self.option_adt_ty(pair))
                }
                _ => None,
            },
            Some("http::ResponseStream") => match method {
                "write" | "write_bytes" => Some(self.tcx.int_ty(IntTy::I64)),
                "close" => Some(self.tcx.unit()),
                "is_open" => Some(self.tcx.bool_ty()),
                _ => None,
            },
            // Every setter answers the server itself, which is what lets a
            // configuration be written as one `|>` chain.
            Some("http::Server") => match method {
                "read_header_timeout_ms"
                | "read_body_timeout_ms"
                | "write_timeout_ms"
                | "idle_timeout_ms"
                | "max_header_bytes"
                | "max_body_bytes"
                | "max_connections"
                | "request_timeout_ms"
                | "server_name" => Some(self.http_server_ty()),
                "listen" => {
                    let unit = self.tcx.unit();
                    Some(self.fallible(unit))
                }
                "serve" => {
                    let unit = self.tcx.unit();
                    Some(self.fallible(unit))
                }
                "addr" => Some(self.tcx.string_ty()),
                "shutdown" => Some(self.tcx.bool_ty()),
                _ => None,
            },
            // A verb method answers the router itself, which is what lets a
            // routing table be built as one `|>` chain. Without the row the
            // chain's later steps carry an open receiver type.
            Some("http::Router") => match method {
                "get" | "post" | "put" | "delete" | "patch" | "head" | "options" => {
                    Some(self.http_router_ty())
                }
                "serve" => Some(self.result_response_error_ty()),
                _ => None,
            },
            Some("http::Response") => match method {
                "with_header" => Some(self.http_response_ty()),
                "bytes" => {
                    let byte = self.tcx.int_ty(IntTy::U8);
                    Some(self.tcx.intern(TyKind::Vec(byte)))
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// Return type of a method on a `sync::Shared`.
    ///
    /// The guarded slot is one word that every tier reads back as an
    /// integer, so that is what a read answers and what an update stores.
    pub(super) fn shared_method_ret(
        &mut self,
        method: &str,
        resolved: Ty,
        args: &[Expr],
        span: Span,
    ) -> Option<Ty> {
        let TyKind::Adt { def, .. } = self.tcx.kind_of(resolved) else {
            return None;
        };
        if self.tcx.def_name(*def) != Some("sync::Shared") {
            return None;
        }
        let elem = self.tcx.int_ty(IntTy::I64);
        match method {
            "get" => Some(elem),
            "set" => {
                if let Some(arg) = args.first() {
                    let value = self.check_expr(arg);
                    self.unify(elem, value, arg.span);
                }
                Some(self.tcx.unit())
            }
            // `with` answers whatever the callback answers; `update` stores
            // what it answers, so that has to be the guarded type.
            "with" | "update" => {
                let output = if method == "update" {
                    elem
                } else {
                    self.fresh()
                };
                let sig = FnSig {
                    inputs: vec![elem],
                    output,
                };
                let want = self.tcx.intern(TyKind::FnTrait(sig));
                if let Some(arg) = args.first() {
                    let got = self.check_expr_expecting(arg, Expectation::HasType(want));
                    self.unify(want, got, arg.span);
                }
                Some(output)
            }
            "clone" => Some(resolved),
            _ => {
                let error = self.unresolved_method_call(
                    "sync::Shared".to_string(),
                    method,
                    resolved,
                    args.len(),
                );
                self.emit(error, span);
                Some(self.tcx.error_ty())
            }
        }
    }

    pub(super) fn http_router_ty(&mut self) -> Ty {
        self.stdlib_handle_ty(41, "http::Router")
    }

    pub(super) fn http_server_ty(&mut self) -> Ty {
        self.stdlib_handle_ty(48, "http::Server")
    }

    #[allow(
        clippy::cognitive_complexity,
        reason = "flat stdlib module dispatch table keeps call typing local"
    )]
    /// `sort::sort_stable(xs) -> Vec<T>`, answering the element type its
    /// argument holds.
    ///
    /// The catalogue row cannot say so on its own: its `Vec<T>` names a
    /// parameter the row has no argument to pin, and an unpinned return
    /// leaves the sequence spelled as a slice wherever it is shown.
    pub(super) fn sort_stable_ret_ty(
        &mut self,
        module: &[&str],
        last: &str,
        arg_tys: &[Ty],
    ) -> Option<Ty> {
        if !matches!(module, ["sort"] | ["std", "sort"]) || last != "sort_stable" {
            return None;
        }
        let resolved = self.infer.resolve(self.tcx, *arg_tys.first()?);
        let (TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. }) =
            self.tcx.kind(resolved)?
        else {
            return None;
        };
        let elem = *elem;
        Some(self.tcx.intern(TyKind::Vec(elem)))
    }

    /// Return types of the `signal` calls that answer a named runtime
    /// handle.
    pub(super) fn process_signal_ret_ty(&mut self, module: &[&str], last: &str) -> Option<Ty> {
        if !matches!(
            module,
            ["signal"] | ["os", "signal"] | ["std", "os", "signal"]
        ) {
            return None;
        }
        match last {
            // `signal::on(sig) -> signal::Notifier`. The runtime value is
            // the same opaque i64 handle; the sentinel type keeps
            // method-form dispatch (`n.wait()`, `n.try_wait()`) uniform
            // with free-form dispatch across tiers.
            "on" => {
                let notifier_def = gossamer_resolve::DefId::local(u32::MAX - 17);
                self.tcx.register_def_name(notifier_def, "Notifier");
                Some(self.tcx.intern(TyKind::Adt {
                    def: notifier_def,
                    substs: crate::Substs::new(),
                }))
            }
            "wait" | "try_wait" => Some(self.tcx.bool_ty()),
            "stop" => Some(self.tcx.unit()),
            _ => None,
        }
    }

    /// The `[rust-bindings]` item a call path names: `module::item`, a path
    /// through a module's last segment (`echo::shout` for `tools::echo`), or
    /// a bare name a `use` bound to one.
    pub(super) fn external_binding_item(
        &self,
        module: &[&str],
        last: &str,
    ) -> Option<gossamer_resolve::ExternalItem> {
        if module.is_empty() {
            return self
                .import_targets
                .iter()
                .filter(|((_, bound), _)| bound == last)
                .find_map(|(_, full)| gossamer_resolve::lookup_external_item(&full.join("::")));
        }
        let joined = format!("{}::{last}", module.join("::"));
        if let Some(item) = gossamer_resolve::lookup_external_item(&joined) {
            return Some(item);
        }
        let [leading] = module else {
            return None;
        };
        gossamer_resolve::all_external_modules()
            .into_iter()
            .filter(|m| m.path.rsplit("::").next() == Some(*leading))
            .find_map(|m| m.items.into_iter().find(|item| item.name == last))
    }

    /// The type a binding value of shape `t` has in the program, or `None`
    /// for a shape whose program type the signature alone does not name (a
    /// callback, a declared arm set, an untyped value).
    pub(super) fn binding_ty(&mut self, t: &gossamer_resolve::BindingType) -> Option<Ty> {
        use gossamer_resolve::BindingType as B;
        Some(match t {
            B::Unit => self.tcx.unit(),
            B::Bool => self.tcx.bool_ty(),
            B::I64 => self.tcx.int_ty(IntTy::I64),
            B::F64 => self.tcx.float_ty(FloatTy::F64),
            B::Char => self.tcx.char_ty(),
            B::String => self.tcx.string_ty(),
            B::Bytes => {
                let byte = self.tcx.int_ty(IntTy::U8);
                self.tcx.intern(TyKind::Slice(byte))
            }
            B::Tuple(elems) => {
                let elems = elems
                    .iter()
                    .map(|elem| self.binding_ty(elem))
                    .collect::<Option<Vec<_>>>()?;
                self.tcx.intern(TyKind::Tuple(elems))
            }
            B::Vec(elem) => {
                let elem = self.binding_ty(elem)?;
                self.tcx.intern(TyKind::Vec(elem))
            }
            B::Option(payload) => {
                let payload = self.binding_ty(payload)?;
                self.option_adt_ty(payload)
            }
            B::Result(ok, err) => {
                let ok = self.binding_ty(ok)?;
                let err = self.binding_ty(err)?;
                self.result_adt_ty(ok, err)
            }
            B::Map(key, value) => {
                let key = self.binding_ty(key)?;
                let value = self.binding_ty(value)?;
                self.tcx.intern(TyKind::HashMap {
                    key,
                    value,
                    ordered: false,
                })
            }
            B::Variant(arms) if arms.is_empty() => self.tcx.intern(TyKind::DynValue),
            B::Variant(_) | B::Callback(..) | B::Opaque(_) | B::Any => return None,
        })
    }

    pub(super) fn check_stdlib_module_ret_ty(
        &mut self,
        (module, last, user_callee): (&[&str], &str, bool),
        callee: &Expr,
        args: &[Expr],
        arg_tys: &[Ty],
        expected: Expectation,
    ) -> Option<Ty> {
        // A `[rust-bindings]` module answers its own items, whatever standard
        // type shares its name: a binding's opaque `Counter` is not
        // `metrics::Counter`.
        if gossamer_resolve::lookup_external_module(&module.join("::")).is_some() {
            return None;
        }
        if let Some(ty) = self.check_qualified_map_accessor_ret(module, last, arg_tys) {
            return Some(ty);
        }
        if is_channel_constructor_path(module, last) {
            return Some(self.channel_tuple_ty());
        }
        if let Some(ty) = self.sort_stable_ret_ty(module, last, arg_tys) {
            return Some(ty);
        }
        // `env::var(name) -> Option<String>`. Typing it concretely lets the
        // match checker reject matching its result with `Result` patterns
        // (`Ok`/`Err`), which otherwise silently fell through on the VM and
        // matched by discriminant on the compiled tier.
        if matches!(module, ["env"] | ["std", "env"]) && last == "var" {
            let s = self.tcx.string_ty();
            return Some(self.option_adt_ty(s));
        }
        if let Some(ty) = self.process_signal_ret_ty(module, last) {
            return Some(ty);
        }
        // `json::Value::*` constructor calls produce the opaque dynamic
        // JSON value. Without this the call is a fresh var, so a
        // chained method (`.set`, `.get`) loses the JsonValue receiver
        // tag and the compiled tiers cannot route it to the json
        // runtime helpers.
        if matches!(
            module,
            ["json", "Value"]
                | ["encoding", "json", "Value"]
                | ["std", "encoding", "json", "Value"]
        ) {
            return Some(self.tcx.json_value_ty());
        }
        // `DynValue::<ctor>(..)` builds the open dynamic value. Every
        // constructor answers one, whatever it was built from.
        if matches!(module, ["DynValue"]) {
            return self.dyn_value_ctor_ret(last);
        }
        if matches!(
            module,
            ["json"] | ["encoding", "json"] | ["std", "encoding", "json"]
        ) {
            return self.json_module_ret_ty(last, callee, args, arg_tys);
        }
        if matches!(module, ["errors"] | ["std", "errors"]) {
            return match last {
                "new" | "wrap" => Some(self.tcx.dyn_error_ty()),
                _ => None,
            };
        }
        // A path a user item answers (`Counter::add` on a user `Counter`) is
        // that item's, whatever runtime handle shares its name.
        let user_type = matches!(module, [name] if self.adt_def_by_name.contains_key(*name));
        if !user_callee && !user_type {
            if let Some(ty) = self.sync_call_ret_ty(module, last, args, arg_tys, callee.span) {
                return Some(ty);
            }
            if let Some(ty) = self.heap_buffer_call_ret_ty(module, last, args, arg_tys, callee.span)
            {
                return Some(ty);
            }
            if let Some(ty) =
                self.table_handle_call_ret_ty(module, last, args, arg_tys, callee.span)
            {
                return Some(ty);
            }
        }
        if let Some(ty) = self.handle_call_ret_ty(module, last) {
            return Some(ty);
        }
        if matches!(module, ["Simd" | "Mask"])
            && let Some(ty) = self.simd_ctor_ret(last, args, arg_tys, expected, callee.span)
        {
            return Some(ty);
        }
        if let Some(ty) =
            self.collection_call_ret_ty(module, last, args, arg_tys, expected, callee.span)
        {
            return Some(ty);
        }
        if matches!(module, ["fs" | "os"] | ["std", "fs" | "os"]) {
            return self.fs_call_ret_ty(last);
        }
        // `time::Duration` / `time::Instant` are their own types, a count of
        // nanoseconds and a reading of the monotonic clock in nanoseconds;
        // their constructors and accessors are the only way between them and
        // an integer.
        if matches!(module, ["time", "Duration"] | ["std", "time", "Duration"]) {
            return self.duration_fn_ret(last);
        }
        if matches!(module, ["time", "Instant"] | ["std", "time", "Instant"]) {
            return self.instant_fn_ret(last);
        }
        None
    }

    /// Return type of a `json::` / `encoding::json::` free-function call
    /// (`parse`, `get`, `as_i64`, ...); `None` for an unrecognised name.
    pub(super) fn json_module_ret_ty(
        &mut self,
        last: &str,
        callee: &Expr,
        args: &[Expr],
        arg_tys: &[Ty],
    ) -> Option<Ty> {
        // Every query below reads a document. An `Option<json::Value>` - what
        // `json::get` answers - is not one, and passing it read as `None` at
        // run time rather than being refused where it was written.
        if matches!(
            last,
            "get"
                | "at"
                | "set"
                | "keys"
                | "len"
                | "is_null"
                | "as_i64"
                | "as_u64"
                | "as_f64"
                | "as_str"
                | "as_bool"
                | "as_array"
        ) {
            self.reject_optional_json_document(args, arg_tys);
        }
        match last {
            "parse" | "decode" => {
                let j = self.tcx.json_value_ty();
                let e = self.tcx.dyn_error_ty();
                Some(self.result_adt_ty(j, e))
            }
            "render" | "encode" => {
                self.reject_json_enum_arg(last, callee, args, arg_tys);
                if let (Some(arg), Some(arg_ty)) = (args.first(), arg_tys.first()) {
                    self.reject_native_address(*arg_ty, "outside this process", arg.span);
                }
                Some(self.tcx.string_ty())
            }
            "at" | "identity" | "set" => Some(self.tcx.json_value_ty()),
            "get" => {
                let j = self.tcx.json_value_ty();
                Some(self.option_adt_ty(j))
            }
            "len" => Some(self.tcx.int_ty(IntTy::I64)),
            "is_null" => Some(self.tcx.bool_ty()),
            "as_i64" => {
                let i = self.tcx.int_ty(IntTy::I64);
                Some(self.option_adt_ty(i))
            }
            "as_u64" => {
                let u = self.tcx.int_ty(IntTy::U64);
                Some(self.option_adt_ty(u))
            }
            "as_f64" => {
                let f = self.tcx.float_ty(FloatTy::F64);
                Some(self.option_adt_ty(f))
            }
            "as_str" => {
                let s = self.tcx.string_ty();
                Some(self.option_adt_ty(s))
            }
            "as_bool" => {
                let b = self.tcx.bool_ty();
                Some(self.option_adt_ty(b))
            }
            "as_array" => {
                let j = self.tcx.json_value_ty();
                let arr = self.tcx.intern(TyKind::Vec(j));
                Some(self.option_adt_ty(arr))
            }
            _ => None,
        }
    }

    /// Reports the document argument of a `json::` query that is a carrier.
    ///
    /// The queries take the value itself, so an `Option` or a `Result` reaches
    /// one only where the writer meant to unwrap it first.
    pub(super) fn reject_optional_json_document(&mut self, args: &[Expr], arg_tys: &[Ty]) {
        let (Some(&first_ty), Some(first)) = (arg_tys.first(), args.first()) else {
            return;
        };
        let mut peeled = self.infer.resolve(self.tcx, first_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(peeled).cloned() {
            peeled = self.infer.resolve(self.tcx, inner);
        }
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(peeled) else {
            return;
        };
        // The two carrier sentinels: `Option` and `Result`.
        if def.local != u32::MAX && def.local != u32::MAX - 1 {
            return;
        }
        let found = self.render_public_ty(peeled);
        let json_ty = self.tcx.json_value_ty();
        let expected = self.render_public_ty(json_ty);
        self.emit(TypeError::TypeMismatch { expected, found }, first.span);
    }

    /// Return type of a `DynValue::<name>(..)` constructor, or `None` when
    /// the name is not one.
    pub(super) fn dyn_value_ctor_ret(&mut self, last: &str) -> Option<Ty> {
        matches!(
            last,
            "nil"
                | "bool"
                | "int"
                | "float"
                | "char"
                | "string"
                | "bytes"
                | "list"
                | "map"
                | "tagged"
        )
        .then(|| self.tcx.dyn_value_ty())
    }

    /// Return type of a method call on one of the runtime's opaque dynamic
    /// receivers - a `json::Value` or a `DynValue`. Each answers the same
    /// surface in method form that its own table declares; falling through to
    /// a fresh variable would strip the receiver's tag and leave a chained
    /// call untagged for the compiled tiers.
    pub(super) fn opaque_value_method_ret(
        &mut self,
        resolved: Ty,
        method: &str,
        arg_count: usize,
    ) -> Option<Ty> {
        match self.tcx.kind(resolved) {
            Some(TyKind::JsonValue) => self.json_value_method_ret(method, arg_count),
            Some(TyKind::DynValue) => self.dyn_value_method_ret(method, arg_count),
            _ => None,
        }
    }

    /// Types a method call on a `json::Value` or `DynValue` receiver from its
    /// table, or reports a name the table does not declare. `None` for any
    /// other receiver, and for a name every value answers.
    pub(super) fn opaque_value_method(
        &mut self,
        resolved: Ty,
        method: &str,
        arg_count: usize,
        span: Span,
    ) -> Option<Ty> {
        if let Some(ty) = self.opaque_value_method_ret(resolved, method, arg_count) {
            return Some(ty);
        }
        self.reject_unknown_opaque_value_method(resolved, method, span)
            .then(|| self.tcx.error_ty())
    }

    /// Rejects a method a `json::Value` or `DynValue` receiver does not
    /// answer. Their tables above are each receiver's whole surface: a name
    /// neither declares has no binding on any tier, so the VM would read it
    /// as a no-op and a native build would end on an undefined symbol.
    pub(super) fn reject_unknown_opaque_value_method(
        &mut self,
        resolved: Ty,
        method: &str,
        span: Span,
    ) -> bool {
        let owner = match self.tcx.kind(resolved) {
            Some(TyKind::JsonValue) => "json::Value",
            Some(TyKind::DynValue) => "DynValue",
            _ => return false,
        };
        // The conversions every value answers, and any name a user impl or
        // trait declares, keep their own resolution.
        if matches!(method, "clone" | "into" | "try_into" | "to_string")
            || self.user_method_owners.contains_key(method)
        {
            return false;
        }
        let error = self.unresolved_method(owner.to_string(), method, resolved);
        self.emit(error, span);
        true
    }

    /// Return type of a method call on a `DynValue` receiver. `None` leaves
    /// the call to the later dispatch arms, which report it as unknown.
    pub(super) fn dyn_value_method_ret(&mut self, method: &str, arg_count: usize) -> Option<Ty> {
        let ty = match (method, arg_count) {
            ("kind" | "name", 0) => self.tcx.string_ty(),
            ("len", 0) => self.tcx.int_ty(IntTy::I64),
            ("at" | "key_at", 1) | ("clone", 0) => self.tcx.dyn_value_ty(),
            ("as_i64", 0) => {
                let i = self.tcx.int_ty(IntTy::I64);
                self.option_adt_ty(i)
            }
            ("as_f64", 0) => {
                let f = self.tcx.float_ty(FloatTy::F64);
                self.option_adt_ty(f)
            }
            ("as_bool", 0) => {
                let b = self.tcx.bool_ty();
                self.option_adt_ty(b)
            }
            ("as_char", 0) => {
                let c = self.tcx.char_ty();
                self.option_adt_ty(c)
            }
            ("as_str", 0) => {
                let s = self.tcx.string_ty();
                self.option_adt_ty(s)
            }
            ("as_bytes", 0) => {
                let i = self.tcx.int_ty(IntTy::I64);
                self.tcx.intern(TyKind::Vec(i))
            }
            ("to_string", 0) => self.tcx.string_ty(),
            _ => return None,
        };
        Some(ty)
    }

    /// Return type of a method call on a `json::Value` receiver, which is
    /// the free function of the same name with the receiver as its first
    /// argument. `None` leaves the call to the later dispatch arms.
    pub(super) fn json_value_method_ret(&mut self, method: &str, arg_count: usize) -> Option<Ty> {
        let ty = match (method, arg_count) {
            ("at", 1) | ("set", 2) => self.tcx.json_value_ty(),
            ("get", 1) => {
                let j = self.tcx.json_value_ty();
                self.option_adt_ty(j)
            }
            ("keys", 0) => {
                let name = self.tcx.string_ty();
                let names = self.tcx.intern(TyKind::Vec(name));
                self.option_adt_ty(names)
            }
            ("len", 0) => self.tcx.int_ty(IntTy::I64),
            ("is_null", 0) => self.tcx.bool_ty(),
            ("as_i64", 0) => {
                let i = self.tcx.int_ty(IntTy::I64);
                self.option_adt_ty(i)
            }
            ("as_u64", 0) => {
                let u = self.tcx.int_ty(IntTy::U64);
                self.option_adt_ty(u)
            }
            ("as_f64", 0) => {
                let f = self.tcx.float_ty(FloatTy::F64);
                self.option_adt_ty(f)
            }
            ("as_str", 0) => {
                let s = self.tcx.string_ty();
                self.option_adt_ty(s)
            }
            ("as_bool", 0) => {
                let b = self.tcx.bool_ty();
                self.option_adt_ty(b)
            }
            ("as_array", 0) => {
                let j = self.tcx.json_value_ty();
                let arr = self.tcx.intern(TyKind::Vec(j));
                self.option_adt_ty(arr)
            }
            _ => return None,
        };
        Some(ty)
    }

    /// Types the parser-injected format intrinsics and the bare
    /// variant constructors. The resolver doesn't hand `Some` / `Ok` /
    /// `Err` / `None` a `DefId`, so the call expression typechecks as
    /// a fresh `Var` and the binding `let first = Some(10)` collapses
    /// to `Int(I64)` - losing the Adt wrapper. Match dispatch later
    /// treats the 8-byte `*mut GosResult` pointer as a raw i64 and
    /// reads garbage from the slot. Recognise the four standard
    /// variants here and synthesise the right Adt: `Some(t)` →
    /// `Option<t>`, `Ok(t)` → `Result<t, ?>`, `Err(e)` →
    /// `Result<?, e>`, `None` → `Option<?>`. Pinning `__concat` /
    /// `__fmt_prec` to `String` is safe: they're synthetic names the
    /// parser injects and no user code can shadow them.
    /// `spawn(f, reason: "..")`: the label is text a report prints, so
    /// anything else would reach the runtime as a word it would read as a
    /// pointer.
    pub(super) fn check_spawn_reason(&mut self, reason: Option<Ty>, span: Span) {
        let Some(reason) = reason else {
            return;
        };
        if matches!(
            self.tcx.kind_of(reason),
            TyKind::String | TyKind::Var(_) | TyKind::Error
        ) {
            return;
        }
        self.emit(
            TypeError::ArgumentTypeMismatch {
                callee: "spawn".to_string(),
                parameter: "reason".to_string(),
                expected: "String".to_string(),
                found: crate::render_ty(self.tcx, reason),
                actual: "the `reason:` label".to_string(),
            },
            span,
        );
    }

    pub(super) fn check_bare_intrinsic_call(
        &mut self,
        name: &str,
        arg_tys: &[Ty],
        span: Span,
    ) -> Option<Ty> {
        let constructor_arity = match name {
            "Some" | "Ok" | "Err" => Some(1),
            "None" => Some(0),
            _ => None,
        };
        if let Some(expected) = constructor_arity
            && arg_tys.len() != expected
        {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: name.to_string(),
                    expected,
                    found: arg_tys.len(),
                },
                span,
            );
        }
        let ty = match name {
            "__concat" | "__debug" | "__fmt_prec" | "__fmt_pad" | "__fmt_radix" | "__fmt_upper"
            | "__gos_debug_quote" | "__gos_f32_display" | "__gos_f32_debug"
            | "__gos_dyn_display" | "__gos_dyn_debug" => {
                for ty in arg_tys {
                    let resolved = self.infer.resolve(self.tcx, *ty);
                    if matches!(
                        self.tcx.kind(resolved),
                        Some(TyKind::Iterator(_) | TyKind::Range(_))
                    ) {
                        self.emit(TypeError::IteratorStateFormatted, span);
                        return Some(self.tcx.error_ty());
                    }
                    if let Some((ty, class)) = self.not_displayable(resolved) {
                        self.emit(TypeError::ValueNotDisplayable { ty, class }, span);
                        return Some(self.tcx.error_ty());
                    }
                    if let Some(ty) = self.generic_without_fmt(resolved) {
                        self.emit(
                            TypeError::ValueNotDisplayable {
                                ty,
                                class: crate::error::NotDisplayableClass::GenericWithoutDebug,
                            },
                            span,
                        );
                        return Some(self.tcx.error_ty());
                    }
                }
                self.tcx.string_ty()
            }
            "__repl_discard" => self.tcx.unit(),
            // `channel()` / `channel(n)` / `channel::unbounded()` ->
            // `(Sender<?T>, Receiver<?T>)` sharing one element var, so
            // `tx.send(v)` unifies the element through the shared `?T` and
            // `rx.recv()` yields `Option<?T>` with the real payload type even
            // for an inferred local channel. The optional constructor argument
            // is capacity only; it never changes the element type.
            "channel"
            | "channel::new"
            | "channel::unbounded"
            | "sync::channel"
            | "sync::channel_unbounded"
            | "std::sync::channel"
            | "std::sync::channel_unbounded" => self.channel_tuple_ty(),
            // `spawn(f) -> JoinHandle<T>`, T being the callable's return
            // type. Typing the call itself is what lets `spawn(f).join()`
            // resolve without binding the handle first: the method arm
            // that lowers `join` keys on the receiver's static type.
            "spawn" if matches!(arg_tys.len(), 1 | 2) => {
                self.check_spawn_reason(arg_tys.get(1).copied(), span);
                let elem = match self.tcx.kind_of(arg_tys[0]).clone() {
                    TyKind::FnTrait(sig) | TyKind::FnPtr(sig) => sig.output,
                    TyKind::Var(_) => self.fresh(),
                    _ => return None,
                };
                self.tcx.intern(TyKind::JoinHandle(elem))
            }
            "min" | "max" | "clamp" => self.scalar_bound_intrinsic_ty(name, arg_tys, span)?,
            "Some" => {
                let payload = arg_tys.first().copied().unwrap_or_else(|| self.fresh());
                self.option_adt_ty(payload)
            }
            "None" => {
                let payload = self.fresh();
                self.option_adt_ty(payload)
            }
            "Ok" => {
                let ok_ty = arg_tys.first().copied().unwrap_or_else(|| self.fresh());
                let err_ty = self.fresh();
                self.result_adt_ty(ok_ty, err_ty)
            }
            "Err" => {
                let ok_ty = self.fresh();
                let err_ty = arg_tys.first().copied().unwrap_or_else(|| self.fresh());
                self.result_adt_ty(ok_ty, err_ty)
            }
            _ => return None,
        };
        Some(ty)
    }

    /// The type a prelude `min` / `max` / `clamp` call answers. `min(xs)` /
    /// `max(xs)` reduce a sequence to `Option<T>`, exactly as `iter::min` /
    /// `iter::max` do; the scalar forms answer the operands' own type. Typing
    /// them here is what lets a binding of the result, and a format site
    /// reading it, know it is a `Vec` or a `u64` rather than an unconstrained
    /// variable.
    pub(super) fn scalar_bound_intrinsic_ty(
        &mut self,
        name: &str,
        arg_tys: &[Ty],
        span: Span,
    ) -> Option<Ty> {
        match (name, arg_tys.len()) {
            ("min" | "max", 1) => {
                let seq = self.peel_resolved_refs(arg_tys[0]);
                let elem = match self.tcx.kind(seq) {
                    Some(TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. }) => {
                        *elem
                    }
                    _ => return None,
                };
                Some(self.option_adt_ty(elem))
            }
            ("min" | "max", 2) | ("clamp", 3) => self.scalar_bound_call_ty(arg_tys, span),
            _ => None,
        }
    }

    /// `ty` resolved through inference and with every reference peeled.
    pub(super) fn peel_resolved_refs(&mut self, ty: Ty) -> Ty {
        let mut cur = self.infer.resolve(self.tcx, ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(cur) {
            cur = self.infer.resolve(self.tcx, *inner);
        }
        cur
    }

    /// The type a scalar `min` / `max` / `clamp` call answers: the operands'
    /// scalar type, where one of them names it. An unsigned 64-bit operand
    /// decides, since its values reach past what a signed word orders; an
    /// integer literal beside it takes that type at run time too.
    pub(super) fn scalar_bound_call_ty(&mut self, arg_tys: &[Ty], span: Span) -> Option<Ty> {
        let operands: Vec<Ty> = arg_tys
            .iter()
            .map(|ty| self.peel_resolved_refs(*ty))
            .collect();
        // Every operand is one value of one type: `min(2.5, 3)` and
        // `max(a_u8, b_i64)` are mismatches, as they would be for `a < b`.
        let first = *operands.first()?;
        for other in &operands[1..] {
            self.unify(first, *other, span);
        }
        let bound = self.peel_resolved_refs(first);
        let orderable = match self.tcx.kind(bound) {
            Some(TyKind::Int(_) | TyKind::Float(_) | TyKind::Char) => true,
            Some(TyKind::Var(_)) => {
                self.infer.is_integer_constrained_var(self.tcx, bound)
                    || self.infer.is_float_literal_var(self.tcx, bound)
            }
            Some(TyKind::Error) => true,
            _ => false,
        };
        if !orderable {
            let found = self.render_public_ty(bound);
            self.emit(
                TypeError::TypeMismatch {
                    expected: "a number or char".to_string(),
                    found,
                },
                span,
            );
        }
        Some(bound)
    }

    pub(super) fn channel_tuple_ty(&mut self) -> Ty {
        let elem = self.fresh();
        let sender = self.tcx.intern(TyKind::Sender(elem));
        let receiver = self.tcx.intern(TyKind::Receiver(elem));
        self.tcx.intern(TyKind::Tuple(vec![sender, receiver]))
    }

    /// The `[(String, [u8])]` entry-list parameter type of the stdlib
    /// `archive::{tar,zip}::write` calls.
    pub(super) fn archive_entry_vec_ty(&mut self) -> Ty {
        let s = self.tcx.string_ty();
        let u8_ty = self.tcx.int_ty(IntTy::U8);
        let vec_u8 = self.tcx.intern(TyKind::Vec(u8_ty));
        let pair = self.tcx.intern(TyKind::Tuple(vec![s, vec_u8]));
        self.tcx.intern(TyKind::Vec(pair))
    }

    /// Shapes stdlib call arguments from the checker-owned source signature
    /// catalogue when the parameter type is concrete enough to enforce safely.
    /// Generic, callable, and JSON-value slots are left unshaped so existing
    /// inference-sensitive paths keep their current semantics.
    pub(super) fn stdlib_signature_arg_expectations(
        &mut self,
        callee_id: NodeId,
        path: &gossamer_ast::PathExpr,
        n_args: usize,
    ) -> Option<Vec<Expectation>> {
        let names = self.resolved_value_path_names(callee_id, path);
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let (module, last) = names.split_at(names.len().saturating_sub(1));
        let name = last.first().copied()?;
        // String functions have String|char pattern slots that the generic
        // signature parser cannot model precisely. Their dedicated validator
        // both enforces the complete contract and emits one named diagnostic.
        if matches!(module, ["strings"] | ["std", "strings"]) {
            return None;
        }
        // Arguments reach the checker in the order every pass below the
        // front end uses, which the rotation in `normalize` produced.
        let shape = crate::stdlib_signatures::internal_shape_for_path(module, name)?;
        if shape.params.len() != n_args {
            return None;
        }
        // A middleware's `T` slot is the handler it wraps.
        let is_middleware = matches!(
            module,
            ["middleware"] | ["http", "middleware"] | ["std", "http", "middleware"]
        );
        Some(
            shape
                .params
                .iter()
                .map(|param| {
                    // A handler slot takes a closure or any type implementing
                    // `Handler`, so it shapes a closure without pinning the
                    // argument's type.
                    let ty = param.ty.trim();
                    if ty == "http::Handler" || (is_middleware && ty == "T") {
                        return Expectation::Coerce(self.http_handler_ty());
                    }
                    self.stdlib_signature_arg_ty(param.ty)
                        .map_or(Expectation::None, Expectation::Coerce)
                })
                .collect(),
        )
    }

    /// Emits the arity diagnostic for stdlib free functions from the same
    /// signature row `%help` displays.
    pub(super) fn check_stdlib_signature_arity(
        &mut self,
        module: &[&str],
        name: &str,
        supplied: usize,
        pipe_extra: usize,
        span: Span,
    ) {
        if matches!(module, ["slog"] | ["std", "slog"]) {
            return;
        }
        let found = supplied + pipe_extra;
        if is_channel_constructor_path(module, name) && matches!(found, 0 | 1) {
            return;
        }
        let Some(shape) = crate::stdlib_signatures::function_shape_for_path(module, name) else {
            return;
        };
        let expected = shape.params.len();
        if found != expected {
            self.emit(
                TypeError::CallArityMismatch {
                    callee: if module.is_empty() {
                        name.to_string()
                    } else {
                        format!("{}::{name}", module.join("::"))
                    },
                    expected,
                    found,
                },
                span,
            );
        }
    }

    /// Validates concrete stdlib parameter slots after argument synthesis.
    /// The expectation path shapes literals before checking; this pass catches
    /// non-literal mismatches and scalar/string literals whose checker path
    /// does not unify against expectations directly.
    pub(super) fn check_stdlib_signature_args(
        &mut self,
        module: &[&str],
        name: &str,
        args: &[Expr],
        arg_tys: &[Ty],
    ) {
        if matches!(module, ["slog"] | ["std", "slog"]) {
            return;
        }
        let Some(shape) = crate::stdlib_signatures::internal_shape_for_path(module, name) else {
            return;
        };
        if shape.params.len() != arg_tys.len() {
            return;
        }
        // A wait's millisecond count may be given as a `Duration` instead,
        // which lowering converts by its own unit.
        let waits =
            matches!(module, ["time"] | ["std", "time"]) && matches!(name, "sleep" | "sleep_ctx");
        for (param, (arg, &arg_ty)) in shape.params.iter().zip(args.iter().zip(arg_tys)) {
            if waits
                && matches!(
                    self.tcx.kind(self.infer.resolve(self.tcx, arg_ty)),
                    Some(TyKind::Duration)
                )
            {
                continue;
            }
            if let Some(param_ty) = self.stdlib_signature_arg_ty(param.ty) {
                if param.ty == "http::websocket::Conn" && self.integer_param_for_conn(arg, arg_ty) {
                    continue;
                }
                self.check_sig_param_arg(param_ty, arg_ty, arg);
            }
        }
    }

    /// Reports a parameter declared `i64` passed where a websocket call takes
    /// its connection, the shape a handler had while connections were bare
    /// integers, and answers whether it did. The fix retypes the parameter.
    fn integer_param_for_conn(&mut self, arg: &Expr, arg_ty: Ty) -> bool {
        let ExprKind::Path(_) = &arg.kind else {
            return false;
        };
        let Some(gossamer_resolve::Resolution::Local(binding)) = self.resolutions.get(arg.id)
        else {
            return false;
        };
        let Some((name, span)) = self.param_type_spans.get(&binding).cloned() else {
            return false;
        };
        let resolved = self.infer.resolve(self.tcx, arg_ty);
        if !matches!(self.tcx.kind(resolved), Some(TyKind::Int(IntTy::I64))) {
            return false;
        }
        self.emit(TypeError::IntegerWebSocketParam { param: name }, span);
        true
    }

    /// Return type for a stdlib free function from the checker-owned signature
    /// catalogue. Rows with generics or opaque nominal stdlib handles that the
    /// checker cannot represent yet fall back to the existing specialised paths
    /// or a fresh variable.
    pub(super) fn stdlib_signature_return_ty(&mut self, module: &[&str], name: &str) -> Option<Ty> {
        let shape = crate::stdlib_signatures::function_shape_for_path(module, name)?;
        self.stdlib_signature_ty(shape.return_ty)
    }

    pub(super) fn stdlib_signature_arg_ty(&mut self, src: &str) -> Option<Ty> {
        // `json::encode` / `json::render` accept scalars and structs in
        // addition to `json::Value`; pinning those slots to the opaque JSON
        // handle would reject valid calls. Return typing can still use it.
        if src.trim() == "json::Value" {
            return None;
        }
        self.stdlib_signature_ty(src)
    }

    /// Callable type for a `Fn(A, B) -> R` slot in a stdlib signature.
    ///
    /// Returns `None` when the slot is not a callable, or when any part of
    /// it names a type the catalogue cannot represent, so an unmodelled
    /// callback keeps its previous inference-variable behaviour.
    pub(super) fn stdlib_signature_fn_ty(&mut self, src: &str) -> Option<Ty> {
        let rest = src.trim().strip_prefix("Fn(")?;
        // The parameter list ends at the paren matching `Fn(`, not at the
        // last one in the row: a return type may carry its own parentheses.
        let mut depth = 1usize;
        let mut close = None;
        for (i, ch) in rest.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let close = close?;
        let (params_src, tail) = rest.split_at(close);
        let mut inputs = Vec::new();
        for part in crate::stdlib_signatures::split_top_level(params_src, ',') {
            if part.trim().is_empty() {
                continue;
            }
            inputs.push(self.stdlib_signature_ty(part)?);
        }
        // An unmodelled return type still leaves the parameters pinned, which
        // is what a closure body needs to type its field accesses.
        let output = match tail[1..].trim().strip_prefix("->") {
            Some(return_src) => self
                .stdlib_signature_ty(return_src)
                .unwrap_or_else(|| self.fresh()),
            None => self.tcx.unit(),
        };
        Some(self.tcx.intern(TyKind::FnTrait(FnSig { inputs, output })))
    }

    /// The `Param` a catalogue type parameter stands for while a signature is
    /// read as a template, one slot per letter.
    pub(super) fn catalog_param_template(&mut self, src: &str) -> Option<Ty> {
        if !self.catalog_params_as_params || !is_catalog_type_param(src) {
            return None;
        }
        let letter = src.bytes().next()?;
        Some(self.tcx.intern(TyKind::Param {
            idx: crate::ParamIdx(u32::from(letter - b'A')),
            name: src.into(),
        }))
    }

    pub(super) fn stdlib_signature_ty(&mut self, src: &str) -> Option<Ty> {
        let src = src.trim();
        // A callback slot resolves to the callable shape it declares, so a
        // closure literal passed there types its parameters from the
        // signature instead of leaving them inference variables that field
        // access then reads dynamically.
        if let Some(sig) = self.stdlib_signature_fn_ty(src) {
            return Some(sig);
        }
        if let Some(param) = self.catalog_param_template(src) {
            return Some(param);
        }
        if src.is_empty()
            || src.contains('|')
            || src.starts_with("Fn(")
            || is_catalog_type_param(src)
        {
            return None;
        }
        let src = src.strip_prefix('&').unwrap_or(src).trim();
        match src {
            "String" => return Some(self.tcx.string_ty()),
            "bool" => return Some(self.tcx.bool_ty()),
            "char" => return Some(self.tcx.char_ty()),
            "i8" => return Some(self.tcx.int_ty(IntTy::I8)),
            "i16" => return Some(self.tcx.int_ty(IntTy::I16)),
            "i32" => return Some(self.tcx.int_ty(IntTy::I32)),
            "i64" => return Some(self.tcx.int_ty(IntTy::I64)),
            "i128" => return Some(self.tcx.int_ty(IntTy::I128)),
            "isize" => return Some(self.tcx.int_ty(IntTy::Isize)),
            "u8" => return Some(self.tcx.int_ty(IntTy::U8)),
            "u16" => return Some(self.tcx.int_ty(IntTy::U16)),
            "u32" => return Some(self.tcx.int_ty(IntTy::U32)),
            "u64" => return Some(self.tcx.int_ty(IntTy::U64)),
            "u128" => return Some(self.tcx.int_ty(IntTy::U128)),
            "usize" => return Some(self.tcx.int_ty(IntTy::Usize)),
            "f32" => return Some(self.tcx.float_ty(FloatTy::F32)),
            "f64" => return Some(self.tcx.float_ty(FloatTy::F64)),
            "()" => return Some(self.tcx.unit()),
            "!" => return Some(self.tcx.intern(TyKind::Never)),
            "json::Value" => return Some(self.tcx.json_value_ty()),
            "time::Instant" => return Some(self.tcx.instant_ty()),
            "time::Duration" => return Some(self.tcx.duration_ty()),
            "io::Reader" | "io::Writer" => return Some(self.io_stream_ty()),
            "errors::Error" | "io::Error" => return Some(self.tcx.dyn_error_ty()),
            _ if src.ends_with("::Error") || src.ends_with("ParseError") => {
                return Some(self.tcx.dyn_error_ty());
            }
            _ => {}
        }
        if let Some(inner) = strip_catalog_wrapper(src, "Vec") {
            let elem = self.stdlib_signature_ty(inner)?;
            return Some(self.tcx.intern(TyKind::Vec(elem)));
        }
        if let Some(inner) = strip_catalog_wrapper(src, "Option") {
            let elem = self.stdlib_signature_ty(inner)?;
            return Some(self.option_adt_ty(elem));
        }
        for (spelling, ordered) in [("Map", false), ("BTreeMap", true)] {
            if let Some(inner) = strip_catalog_wrapper(src, spelling) {
                let parts = crate::stdlib_signatures::split_top_level(inner, ',');
                let [key_src, value_src] = parts.as_slice() else {
                    return None;
                };
                let key = self.stdlib_signature_ty(key_src)?;
                let value = self.stdlib_signature_ty(value_src)?;
                return Some(self.tcx.intern(TyKind::HashMap {
                    key,
                    value,
                    ordered,
                }));
            }
        }
        if let Some(inner) = strip_catalog_wrapper(src, "Result") {
            let parts = crate::stdlib_signatures::split_top_level(inner, ',');
            let [ok_src, err_src] = parts.as_slice() else {
                return None;
            };
            let ok = self.stdlib_signature_ty(ok_src)?;
            let err = self.stdlib_signature_ty(err_src)?;
            return Some(self.result_adt_ty(ok, err));
        }
        if let Some(inner) = src.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
            if inner.trim().is_empty() {
                return Some(self.tcx.unit());
            }
            let elems = crate::stdlib_signatures::split_top_level(inner, ',')
                .into_iter()
                .map(|part| self.stdlib_signature_ty(part))
                .collect::<Option<Vec<_>>>()?;
            return Some(self.tcx.intern(TyKind::Tuple(elems)));
        }
        self.nominal_handle_ty(src)
    }

    /// The sentinel Adt a signature slot naming a stdlib handle resolves to:
    /// the same one a written annotation gets, so the slot carries the
    /// handle's fields rather than an inference variable.
    fn nominal_handle_ty(&mut self, src: &str) -> Option<Ty> {
        if src == "http::websocket::Conn" {
            return Some(self.stdlib_handle_ty(super::WEBSOCKET_CONN_OFFSET, src));
        }
        let tail = src.rsplit("::").next().unwrap_or(src);
        if let Some(offset) = stdlib_handle_def_offset(tail) {
            let def = gossamer_resolve::DefId::local(u32::MAX - offset);
            // A socket or filesystem handle keeps the qualified name the
            // annotation path registers, so one `DefId` never carries two
            // spellings.
            let name = stdlib_net_handle(tail)
                .or_else(|| stdlib_fs_handle(tail))
                .map_or_else(|| tail.to_string(), |(_, n)| n.to_string());
            self.tcx.register_def_name(def, &name);
            return Some(self.tcx.intern(TyKind::Adt {
                def,
                substs: crate::Substs::new(),
            }));
        }
        None
    }

    /// Re-records literal nodes to a type discovered by *joining*
    /// sibling branches - `if c { [1, 2] } else { [3] }` joins to
    /// `Vec<i64>` only after both arms are checked, so the arm
    /// literals (and the wrapper nodes codegen sizes result slots
    /// from) are re-shaped afterwards. This is the synthesis-side
    /// complement of [`Expectation`], which handles every site where
    /// the expected type is known *before* checking.
    /// Bare nominal name of a struct/enum type, seeing through `&`/`&mut`.
    /// Returns `None` for non-ADT types. Used to look up operator-overload
    /// impl methods (`V2::add`).
    /// Nominal-type name of an operator operand: a user ADT, or an opaque
    /// alias.
    ///
    /// An opaque alias inherits nothing from its representation, so
    /// arithmetic on one routes to the alias's own operator impl and is
    /// rejected when it has none - the same contract a struct or enum
    /// operand gets. Comparison, hashing and formatting are unaffected;
    /// those describe the value, which the alias and its representation
    /// share.
    pub(super) fn operand_nominal_name_of(&mut self, ty: Ty) -> Option<String> {
        let mut r = self.infer.resolve(self.tcx, ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(r) {
            r = self.infer.resolve(self.tcx, *inner);
        }
        if let Some(TyKind::Nominal { def, .. }) = self.tcx.kind(r) {
            return self.tcx.def_name(*def).map(str::to_string);
        }
        self.adt_name_of(ty)
    }

    pub(super) fn adt_name_of(&mut self, ty: Ty) -> Option<String> {
        let mut r = self.infer.resolve(self.tcx, ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(r) {
            r = self.infer.resolve(self.tcx, *inner);
        }
        if let Some(TyKind::Adt { def, .. }) = self.tcx.kind(r) {
            self.tcx.def_name(*def).map(str::to_string)
        } else {
            None
        }
    }

    /// Return type of the operator-overload impl method `method` (with
    /// `arity` non-receiver parameters) on an ADT operand, seeing through
    /// `&` / `&mut`. Covers non-generic impls directly and generic impls
    /// (`impl<T> Add for Wrap<T>`) by substituting the operand
    /// instantiation's generic arguments into the stored return type.
    /// `None` when the operand is not an ADT or carries no such impl.
    pub(super) fn adt_op_method_ret(&mut self, ty: Ty, method: &str, arity: usize) -> Option<Ty> {
        let mut r = self.infer.resolve(self.tcx, ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(r) {
            r = self.infer.resolve(self.tcx, *inner);
        }
        // An opaque alias carries operator impls under its own name, and
        // takes no generic arguments of its own.
        let (def, substs) = match self.tcx.kind(r) {
            Some(TyKind::Adt { def, substs }) => (*def, substs.clone()),
            Some(TyKind::Nominal { def, .. }) => (*def, crate::Substs::new()),
            _ => return None,
        };
        let name = self.tcx.def_name(def)?.to_string();
        if let Some(&ret) = self
            .method_ret_types
            .get(&(name.clone(), method.to_string(), arity))
        {
            return Some(ret);
        }
        let &ret = self
            .generic_method_ret_types
            .get(&(name, method.to_string(), arity))?;
        let subst_tys = substs.types();
        Some(self.subst_params_in_ty(ret, &subst_tys))
    }

    /// The method `lhs <op> rhs` reaches when `lhs`'s type implements the
    /// operator for a right-hand type other than itself (`impl Mul<f64> for
    /// V2`), chosen by `rhs`'s type, with what it answers. `None` when no such
    /// impl takes `rhs`, and the operator falls to the impl for the type
    /// itself.
    pub(super) fn rhs_typed_operator_method(
        &mut self,
        (lhs, rhs): (Ty, Ty),
        method: &str,
        span: Span,
    ) -> Option<(String, Ty)> {
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(lhs) else {
            return None;
        };
        let owner = self.tcx.def_name(*def)?.to_string();
        let prefix = format!("{}{method}_", gossamer_ast::OPERATOR_METHOD_PREFIX);
        let mut candidates: Vec<(String, Ty)> = self
            .method_param_types
            .iter()
            .filter(|((o, name), params)| {
                *o == owner && name.starts_with(&prefix) && params.len() == 1
            })
            .map(|((_, name), params)| (name.clone(), params[0]))
            .collect();
        candidates.sort_by(|a, b| a.0.cmp(&b.0));
        let (name, param) = candidates
            .into_iter()
            .find(|(_, param)| self.operand_fits_param(rhs, *param))?;
        self.unify(param, rhs, span);
        let ret = self
            .method_ret_types
            .get(&(owner, name.clone(), 1))
            .copied()
            .unwrap_or_else(|| self.tcx.unit());
        Some((name, ret))
    }

    /// Whether an operand of type `operand` is one a parameter of type
    /// `param` takes: the same type, or a literal that settles on it.
    pub(super) fn operand_fits_param(&mut self, operand: Ty, param: Ty) -> bool {
        let operand = self.infer.resolve(self.tcx, operand);
        let param = self.infer.resolve(self.tcx, param);
        if operand == param {
            return true;
        }
        match (self.tcx.kind(operand), self.tcx.kind(param)) {
            (Some(TyKind::Var(_)), Some(TyKind::Int(_))) => {
                self.infer.is_integer_constrained_var(self.tcx, operand)
            }
            (Some(TyKind::Var(_)), Some(TyKind::Float(_))) => {
                self.infer.is_float_literal_var(self.tcx, operand)
            }
            (Some(TyKind::Adt { def: a, .. }), Some(TyKind::Adt { def: b, .. })) => a == b,
            _ => false,
        }
    }

    /// What `time::Duration::<name>` answers, for a constructor or an
    /// accessor.
    pub(super) fn duration_fn_ret(&mut self, name: &str) -> Option<Ty> {
        match name {
            "from_nanos" | "from_micros" | "from_millis" | "from_secs" | "from_secs_f64" => {
                Some(self.tcx.duration_ty())
            }
            "as_nanos" | "as_micros" | "as_millis" | "as_secs" => Some(self.tcx.int_ty(IntTy::I64)),
            "as_secs_f64" => Some(self.tcx.float_ty(FloatTy::F64)),
            _ => None,
        }
    }

    /// What `time::Instant::<name>` answers.
    pub(super) fn instant_fn_ret(&mut self, name: &str) -> Option<Ty> {
        match name {
            "now" => Some(self.tcx.instant_ty()),
            "elapsed_ms" => Some(self.tcx.int_ty(IntTy::I64)),
            "elapsed" | "duration_since" => Some(self.tcx.duration_ty()),
            _ => None,
        }
    }

    /// `time::Duration` / `time::Instant` methods (`d.as_millis()`,
    /// `inst.elapsed()`), which mirror the qualified free calls with the
    /// receiver as the first argument. `None` for every other receiver.
    pub(super) fn time_accessor_method_ret(
        &mut self,
        resolved: Ty,
        method: &str,
        args: &[Expr],
        arg_tys: &[Ty],
    ) -> Option<Ty> {
        match self.tcx.kind(resolved) {
            Some(TyKind::Duration) if args.is_empty() && DURATION_METHODS.contains(&method) => {
                self.duration_fn_ret(method)
            }
            Some(TyKind::Instant) => match (method, args, arg_tys) {
                ("elapsed_ms" | "elapsed", [], _) => self.instant_fn_ret(method),
                ("duration_since", [earlier], [earlier_ty]) => {
                    let instant = self.tcx.instant_ty();
                    self.unify(instant, *earlier_ty, earlier.span);
                    self.instant_fn_ret(method)
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// Return type of `method` (with `arity` non-receiver arguments) called
    /// on a generic-instantiation receiver, from the generic impl's declared
    /// return with the instantiation's arguments substituted. `None` for
    /// non-generic receivers or unknown methods.
    pub(super) fn generic_recv_method_ret(
        &mut self,
        resolved: Ty,
        method: &str,
        arity: usize,
        method_substs: &[Ty],
    ) -> Option<Ty> {
        let Some(TyKind::Adt { def, substs }) = self.tcx.kind(resolved) else {
            return None;
        };
        let substs = substs.clone();
        if substs.is_empty() {
            return None;
        }
        let name = self.tcx.def_name(*def)?.to_string();
        let &ret = self
            .generic_method_ret_types
            .get(&(name, method.to_string(), arity))?;
        let (mut subst_tys, subst_consts) = self.adt_subst_vectors(&substs);
        // The method's own type parameters take the positions after the
        // impl's, standing for what this call's arguments instantiated them to.
        for position in subst_tys.len()..self.param_slots(ret) {
            let ty = method_substs
                .get(position)
                .copied()
                .unwrap_or_else(|| self.fresh());
            subst_tys.push(ty);
        }
        Some(self.subst_generics_in_ty(ret, &subst_tys, &subst_consts))
    }

    /// Coerces a byte literal compared against an integer operand to that
    /// operand's integer type, so `s[i] == b'>'` type-checks without a cast.
    /// Returns true when it applied (caller then skips the same-type unify).
    pub(super) fn coerce_byte_literal_cmp(
        &mut self,
        lhs: &Expr,
        lhs_ty: Ty,
        rhs: &Expr,
        rhs_ty: Ty,
    ) -> bool {
        let is_byte_lit = |e: &Expr| matches!(&e.kind, ExprKind::Literal(Literal::Byte(_)));
        let lr = self.infer.resolve(self.tcx, lhs_ty);
        let rr = self.infer.resolve(self.tcx, rhs_ty);
        if is_byte_lit(lhs) && self.is_integer(rr) {
            self.record(lhs.id, rr);
            true
        } else if is_byte_lit(rhs) && self.is_integer(lr) {
            self.record(rhs.id, lr);
            true
        } else {
            false
        }
    }

    pub(super) fn adjust_literal_to_join(&mut self, expr: &Expr, expected: Ty) {
        let expected = self.infer.resolve(self.tcx, expected);
        let expected = match self.tcx.kind(expected) {
            Some(TyKind::Ref { inner, .. }) => *inner,
            _ => expected,
        };
        match &expr.kind {
            // `&[..]` / `&mut [..]`: the borrow is transparent at the
            // layout level - re-type the borrowed literal itself
            // (expected already had its `Ref` stripped above).
            ExprKind::Unary {
                op: UnaryOp::RefShared | UnaryOp::RefMut,
                operand,
            } => self.adjust_literal_to_join(operand, expected),
            ExprKind::Array(ArrayExpr::List(elems)) => {
                let _ = elems;
            }
            ExprKind::Array(ArrayExpr::Repeat { value, .. }) => {
                let _ = value;
            }
            ExprKind::Tuple(elems) => {
                if let Some(TyKind::Tuple(tys)) = self.tcx.kind(expected).cloned() {
                    if tys.len() == elems.len() {
                        self.record(expr.id, expected);
                        for (el, t) in elems.iter().zip(tys) {
                            self.adjust_literal_to_join(el, t);
                        }
                    }
                }
            }
            // Push the expected type through value-producing positions so
            // a literal in a block tail / branch / arm is re-recorded too:
            // `fn f() -> Vec<T> { [..] }`,
            // `let v: Vec<T> = if c { [..] } else { [..] }`. The wrapping
            // node is re-recorded as well - codegen sizes the block/if/
            // match result slot from its node type, so leaving it `[T; N]`
            // while the branches build a heap Vec would desync the slot.
            ExprKind::Block(block) | ExprKind::Unsafe(block) => {
                if let Some(tail) = &block.tail {
                    self.record(expr.id, expected);
                    self.adjust_literal_to_join(tail, expected);
                }
            }
            ExprKind::If {
                then_branch,
                else_branch,
                ..
            } => {
                self.record(expr.id, expected);
                self.adjust_literal_to_join(then_branch, expected);
                if let Some(else_branch) = else_branch {
                    self.adjust_literal_to_join(else_branch, expected);
                }
            }
            ExprKind::Match { arms, .. } => {
                self.record(expr.id, expected);
                for arm in arms {
                    self.adjust_literal_to_join(&arm.body, expected);
                }
            }
            _ => {}
        }
    }

    pub(super) fn option_adt_ty(&mut self, payload: Ty) -> Ty {
        let substs = crate::Substs::from_types([payload]);
        let def = gossamer_resolve::DefId::local(u32::MAX - 1);
        self.tcx.register_def_name(def, "Option");
        self.tcx.intern(TyKind::Adt { def, substs })
    }

    pub(super) fn hashset_ty(&mut self, elem: Ty) -> Ty {
        self.set_ty("Set", elem)
    }

    pub(super) fn btreeset_ty(&mut self, elem: Ty) -> Ty {
        self.set_ty("BTreeSet", elem)
    }

    pub(super) fn vecdeque_ty(&mut self, elem: Ty) -> Ty {
        let substs = crate::Substs::from_types([elem]);
        let def = gossamer_resolve::DefId::local(VEC_DEQUE_DEF_LOCAL);
        self.tcx.register_def_name(def, "Deque");
        self.tcx.intern(TyKind::Adt { def, substs })
    }

    pub(super) fn vecqueue_ty(&mut self, elem: Ty) -> Ty {
        let substs = crate::Substs::from_types([elem]);
        let def = gossamer_resolve::DefId::local(VEC_QUEUE_DEF_LOCAL);
        self.tcx.register_def_name(def, "Queue");
        self.tcx.intern(TyKind::Adt { def, substs })
    }

    pub(super) fn vecstack_ty(&mut self, elem: Ty) -> Ty {
        let substs = crate::Substs::from_types([elem]);
        let def = gossamer_resolve::DefId::local(VEC_STACK_DEF_LOCAL);
        self.tcx.register_def_name(def, "Stack");
        self.tcx.intern(TyKind::Adt { def, substs })
    }

    pub(super) fn binary_heap_ty(&mut self, elem: Ty) -> Ty {
        let substs = crate::Substs::from_types([elem]);
        let def = gossamer_resolve::DefId::local(BINARY_HEAP_DEF_LOCAL);
        self.tcx.register_def_name(def, "MaxHeap");
        self.tcx.intern(TyKind::Adt { def, substs })
    }

    pub(super) fn min_heap_ty(&mut self, elem: Ty) -> Ty {
        let substs = crate::Substs::from_types([elem]);
        let def = gossamer_resolve::DefId::local(MIN_HEAP_DEF_LOCAL);
        self.tcx.register_def_name(def, "MinHeap");
        self.tcx.intern(TyKind::Adt { def, substs })
    }

    /// The element type a slot-backed container's methods read and write. An
    /// element the constructor never pinned - `Queue::new()` with no
    /// annotation and no `push` yet - settles as `i64`, the width a slot
    /// holds.
    /// The element type a slot-backed container's methods read and write,
    /// without re-checking it. The declaration that pinned the element is
    /// where an element the container cannot hold is reported; a call on the
    /// receiver reads whatever was written there.
    pub(super) fn slot_collection_elem_as_written(&mut self, elem: Option<Ty>) -> Ty {
        elem.unwrap_or_else(|| self.tcx.int_ty(IntTy::I64))
    }

    /// Checks the element type of a slot-backed container (`Deque`, `Queue`,
    /// `Stack`, `MaxHeap`, `MinHeap`).
    ///
    /// A `Deque` / `Queue` / `Stack` stores and hands back; it holds an
    /// element of any type, in the same element store a `Vec<T>` uses. A heap
    /// also orders its elements, so its element must be one the language
    /// orders: every scalar, a `String`, a tuple, a struct, an array, a
    /// sequence, an `Option` / `Result`, and any nesting of those. A `Map` or
    /// a `Set` has no ordering, and a `u64` / `usize` runs past the signed
    /// range the heap compares by, so both are declined there. An unresolved
    /// element is left to the push that pins it.
    pub(super) fn require_slot_collection_elem(&mut self, elem: Ty, owner: &str, span: Span) -> Ty {
        let resolved = self.infer.resolve(self.tcx, elem);
        if !matches!(owner, "MaxHeap" | "MinHeap" | "BinaryHeap") {
            return elem;
        }
        self.require_container_reads_the_types_order(resolved, owner, span);
        if self.is_orderable_elem(resolved) {
            return elem;
        }
        let found = self.render_public_ty(resolved);
        self.emit(
            TypeError::SlotCollectionElement {
                owner: owner.to_string(),
                found,
            },
            span,
        );
        // Recovery keeps the element the annotation named, so the pushes and
        // pops that follow are checked against it rather than reported a
        // second time against a substituted `i64`.
        elem
    }

    /// Reports a container that orders its elements as it stores them when the
    /// element writes its own `cmp`.
    ///
    /// A sequence orders on demand, so its ordering calls route through the
    /// type's comparator, and a `BTreeMap` / `BTreeSet` hands the comparator
    /// to the tree it keeps its entries in. A heap keeps its elements in the
    /// order they were stored in, reached with no comparator to call, so the
    /// order the type declares would silently not be the one read back.
    pub(super) fn require_container_reads_the_types_order(
        &mut self,
        elem: Ty,
        owner: &str,
        span: Span,
    ) {
        let resolved = self.infer.resolve(self.tcx, elem);
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved) else {
            return;
        };
        let Some(name) = self.tcx.def_name(*def) else {
            return;
        };
        let bare = name.rsplit("::").next().unwrap_or(name).to_string();
        if !self.user_ordered_types.contains(&bare) {
            return;
        }
        let elem = self.render_public_ty(resolved);
        self.emit(
            TypeError::ContainerIgnoresUserOrder {
                owner: owner.to_string(),
                elem,
            },
            span,
        );
    }

    /// Whether values of `ty` have an ordering: the scalars, `String`, and
    /// every aggregate whose parts are themselves ordered. A `u64` / `usize`
    /// spans past the signed comparison a heap slot orders by, and a `Map` or
    /// `Set` has no element order at all.
    pub(super) fn is_orderable_elem(&mut self, ty: Ty) -> bool {
        let resolved = self.infer.resolve(self.tcx, ty);
        match self.tcx.kind(resolved) {
            Some(TyKind::Int(IntTy::U64 | IntTy::Usize)) => false,
            Some(
                TyKind::Int(_)
                | TyKind::Float(_)
                | TyKind::Bool
                | TyKind::Char
                | TyKind::String
                | TyKind::Var(_),
            ) => true,
            Some(TyKind::Ref { inner, .. }) => {
                let inner = *inner;
                self.is_orderable_elem(inner)
            }
            Some(TyKind::Vec(inner) | TyKind::Slice(inner) | TyKind::Array { elem: inner, .. }) => {
                let inner = *inner;
                self.is_orderable_elem(inner)
            }
            Some(TyKind::Tuple(elems)) => {
                let elems = elems.clone();
                elems.into_iter().all(|e| self.is_orderable_elem(e))
            }
            Some(TyKind::Adt { def, substs }) => {
                let (def, substs) = (*def, substs.clone());
                if matches!(def.local, HASH_SET_DEF_LOCAL | BTREE_SET_DEF_LOCAL) {
                    return false;
                }
                // `Option` / `Result` order by arm, then by payload; a user
                // struct or enum orders by its fields in declaration order.
                if def.local == u32::MAX || def.local == u32::MAX - 1 {
                    return substs.types().iter().all(|t| self.is_orderable_elem(*t));
                }
                if let Some(fields) = self.tcx.struct_field_tys(def) {
                    let fields = fields.to_vec();
                    return fields.into_iter().all(|f| self.is_orderable_elem(f));
                }
                if let Some(variants) = self.tcx.enum_variant_tys(def) {
                    let variants: Vec<Vec<Ty>> = variants.to_vec();
                    return variants
                        .into_iter()
                        .all(|fields| fields.into_iter().all(|f| self.is_orderable_elem(f)));
                }
                false
            }
            _ => false,
        }
    }

    pub(super) fn reverse_ty(&mut self, elem: Ty) -> Ty {
        let substs = crate::Substs::from_types([elem]);
        let def = gossamer_resolve::DefId::local(REVERSE_DEF_LOCAL);
        self.tcx.register_def_name(def, "Reverse");
        self.tcx.register_tuple_struct(def.local);
        self.tcx.register_struct_fields(def, vec![elem]);
        self.tcx
            .register_struct_fields_inst(def, substs.clone(), vec![elem]);
        self.struct_fields
            .insert(def, vec![("0".to_string(), elem)]);
        self.tcx.intern(TyKind::Adt { def, substs })
    }

    pub(super) fn set_ty(&mut self, owner: &str, elem: Ty) -> Ty {
        let substs = crate::Substs::from_types([elem]);
        let (local, name) = match owner {
            "BTreeSet" => (BTREE_SET_DEF_LOCAL, "BTreeSet"),
            _ => (HASH_SET_DEF_LOCAL, "Set"),
        };
        let def = gossamer_resolve::DefId::local(local);
        self.tcx.register_def_name(def, name);
        self.tcx.intern(TyKind::Adt { def, substs })
    }

    pub(super) fn set_elem_ty(&self, ty: Ty) -> Option<(String, Ty)> {
        match self.tcx.kind(ty) {
            Some(TyKind::Adt { def, substs })
                if matches!(def.local, HASH_SET_DEF_LOCAL | BTREE_SET_DEF_LOCAL) =>
            {
                let owner = if def.local == BTREE_SET_DEF_LOCAL {
                    "BTreeSet"
                } else {
                    "Set"
                };
                substs
                    .types()
                    .first()
                    .copied()
                    .map(|elem| (owner.to_string(), elem))
            }
            _ => None,
        }
    }

    /// Returns true when `.downgrade()` on this (already ref-peeled)
    /// receiver type has no runtime RC header: a by-value scalar, `Unit` /
    /// `Never`, a transparent time newtype, `Option` / `Result`, or an
    /// inline (2-word by-value) enum. Such a value carries no pointer for
    /// `gos_rt_rc_downgrade` to read, so a `Weak` of it faults on the
    /// compiled tiers.
    pub(super) fn downgrade_receiver_is_non_rc(&self, ty: Ty) -> bool {
        match self.tcx.kind(ty) {
            Some(
                TyKind::Bool
                | TyKind::Char
                | TyKind::Int(_)
                | TyKind::Float(_)
                | TyKind::Unit
                | TyKind::Never
                | TyKind::Duration
                | TyKind::Instant,
            ) => true,
            Some(TyKind::Adt { def, .. }) if def.local == u32::MAX || def.local == u32::MAX - 1 => {
                true
            }
            Some(TyKind::Adt { .. }) => self.tcx.is_inline_enum_ty(ty),
            _ => false,
        }
    }

    pub(super) fn weak_adt_ty(&mut self, payload: Ty) -> Ty {
        let substs = crate::Substs::from_types([payload]);
        let def = gossamer_resolve::DefId::local(u32::MAX - 6);
        self.tcx.register_def_name(def, "Weak");
        self.tcx.intern(TyKind::Adt { def, substs })
    }

    /// `value.downgrade()` produces a `Weak<T>` for any RC-managed aggregate;
    /// `weak.upgrade()` produces `Option<T>` from a `Weak<T>`. Name-global
    /// dispatch resolved these while the receiver type was an unresolved
    /// variable; a concretely-typed receiver (e.g. an enum bound from a
    /// variant constructor) needs the explicit rule. Returns `None` for any
    /// other method / receiver so normal dispatch continues.
    pub(super) fn check_weak_method(
        &mut self,
        method: &str,
        receiver_ty: Ty,
        args: &[Expr],
        receiver_span: Span,
    ) -> Option<Ty> {
        if !args.is_empty() {
            return None;
        }
        let mut resolved = self.infer.resolve(self.tcx, receiver_ty);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        if method == "downgrade" {
            // `downgrade` needs a runtime RC pointer to bump the weak count.
            // A by-value word (scalar / `Option` / `Result` / other packed
            // value) has no header, so `gos_rt_rc_downgrade` reads a bogus
            // header off the value's bits and faults on the compiled tiers
            // (the VM hands back a nonsense handle). Reject it here rather
            // than let name-global dispatch type it to `Weak<T>`. An
            // unresolved receiver (`Var`) carries no decision - leave it for
            // normal dispatch so a later-inferred aggregate still works.
            if self.downgrade_receiver_is_non_rc(resolved) {
                let ty = self.render_public_ty(resolved);
                self.emit(TypeError::WeakDowngradeNonRc { ty }, receiver_span);
                return Some(self.tcx.error_ty());
            }
            if matches!(self.tcx.kind(resolved), Some(TyKind::Adt { .. })) {
                return Some(self.weak_adt_ty(resolved));
            }
            // An unresolved receiver (an unsuffixed literal that defaults
            // later, e.g. `let x = 5; x.downgrade()`) defers to the
            // post-defaulting pass, which rejects it if it lands on a
            // by-value scalar.
            if matches!(self.tcx.kind(resolved), Some(TyKind::Var(_))) {
                self.deferred_structural.push(DeferredStructural {
                    ty: resolved,
                    span: receiver_span,
                    kind: DeferredStructuralKind::Downgrade,
                    result: None,
                });
            }
            return None;
        }
        match self.tcx.kind(resolved) {
            Some(TyKind::Adt { def, substs })
                if def.local == u32::MAX - 6 && method == "upgrade" =>
            {
                let payload = substs
                    .types()
                    .first()
                    .copied()
                    .unwrap_or_else(|| self.fresh());
                Some(self.option_adt_ty(payload))
            }
            _ => None,
        }
    }
}

/// A lane vector receiver as its methods are typed: the vector type, its
/// lane type before and after resolution, its lane count, and its lane class.
#[derive(Clone, Copy)]
struct SimdReceiver {
    recv: Ty,
    elem: Ty,
    elem_res: Ty,
    lanes: crate::ArrayLen,
    float: bool,
    int: bool,
    mask: bool,
}
