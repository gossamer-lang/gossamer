// `Simd<T, N>` reaches HIR as the fixed array `[T; N]` it is laid out as, and
// every lane-wise operation lowers to a loop over the lanes written with the
// scalar operations each tier already agrees on. The lane rules are spelled
// out here, once, so every tier computes the same bits: integer lanes wrap,
// shift counts are taken modulo the lane width, `min` / `max` propagate NaN
// and order `-0.0` below `+0.0`, and a float reduction adds in a fixed
// balanced tree.

use gossamer_ast::{
    BinaryOp as AstBinOp, Expr as AstExpr, ExprKind as AstExprKind, Ident, NodeId,
    UnaryOp as AstUnaryOp,
};
use gossamer_lex::Span;
use gossamer_types::{ArrayLen, IntTy, Ty, TyKind};

use super::Lowerer;
use crate::tree::{
    HirArrayExpr, HirBinaryOp, HirBlock, HirExpr, HirExprKind, HirLiteral, HirPat, HirPatKind,
    HirStmt, HirStmtKind, HirUnaryOp,
};

/// The working array a runtime-length reduction pairs in place, and the
/// binding holding how many of its lanes the current round reads.
#[derive(Clone, Copy)]
struct LaneWork<'a> {
    array: &'a str,
    array_ty: Ty,
    count: &'a str,
    elem: Ty,
}

/// The sequence a lane window reads or writes. A place is lowered again at
/// each use, so a write lands in the storage the call names; any other
/// expression is evaluated once into a binding.
enum WindowSeq<'a> {
    Place(&'a AstExpr),
    Bound { name: String, ty: Ty },
}

/// The element type and lane count of a `Simd<T, N>` value.
#[derive(Clone, Copy)]
pub(super) struct SimdShape {
    pub(super) elem: Ty,
    pub(super) lanes: ArrayLen,
}

/// The lane operation a binary operator names.
#[derive(Clone, Copy)]
enum LaneOp {
    Arith(HirBinaryOp),
    Wrapping(&'static str),
    Shift(HirBinaryOp),
}

impl Lowerer<'_> {
    /// The `Simd` shape of the value `node` produces, seen through references.
    pub(super) fn simd_shape(&self, node: NodeId) -> Option<SimdShape> {
        let mut ty = self.table.get(node)?;
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(ty) {
            ty = *inner;
        }
        match self.tcx.kind(ty)? {
            TyKind::Simd { elem, lanes } => Some(SimdShape {
                elem: *elem,
                lanes: *lanes,
            }),
            _ => None,
        }
    }

    /// `a <op> b` over two `Simd` operands of `shape`.
    pub(super) fn lower_simd_binary(
        &mut self,
        op: AstBinOp,
        lhs: &AstExpr,
        rhs: &AstExpr,
        shape: SimdShape,
        span: Span,
    ) -> Option<HirExprKind> {
        let float = self.is_float(shape.elem);
        let lane_op = match op {
            AstBinOp::Add if float => LaneOp::Arith(HirBinaryOp::Add),
            AstBinOp::Sub if float => LaneOp::Arith(HirBinaryOp::Sub),
            AstBinOp::Mul if float => LaneOp::Arith(HirBinaryOp::Mul),
            AstBinOp::Div if float => LaneOp::Arith(HirBinaryOp::Div),
            AstBinOp::Add | AstBinOp::WrappingAdd => LaneOp::Wrapping("__gos_wrapping_add"),
            AstBinOp::Sub | AstBinOp::WrappingSub => LaneOp::Wrapping("__gos_wrapping_sub"),
            AstBinOp::Mul | AstBinOp::WrappingMul => LaneOp::Wrapping("__gos_wrapping_mul"),
            AstBinOp::BitAnd if !float => LaneOp::Arith(HirBinaryOp::BitAnd),
            AstBinOp::BitOr if !float => LaneOp::Arith(HirBinaryOp::BitOr),
            AstBinOp::BitXor if !float => LaneOp::Arith(HirBinaryOp::BitXor),
            AstBinOp::Shl if !float => LaneOp::Shift(HirBinaryOp::Shl),
            AstBinOp::Shr if !float => LaneOp::Shift(HirBinaryOp::Shr),
            _ => return None,
        };
        let a = self.lower_expr(lhs);
        let b = self.lower_expr(rhs);
        let elem = shape.elem;
        let width = self.int_bits(elem);
        Some(
            self.simd_lane_loop(shape, vec![a, b], elem, span, |this, lanes, span| {
                let (x, y) = (lanes[0].clone(), lanes[1].clone());
                match lane_op {
                    LaneOp::Arith(op) => this.hir_binary(op, x, y, elem, span),
                    LaneOp::Wrapping(method) => this.hir_expr(
                        elem,
                        span,
                        HirExprKind::MethodCall {
                            receiver: Box::new(x),
                            name: Ident::new(method),
                            args: vec![y],
                            owner: None,
                        },
                    ),
                    LaneOp::Shift(op) => {
                        let mask = this.hir_int(i64::from(width) - 1, elem, span);
                        let count = this.hir_binary(HirBinaryOp::BitAnd, y, mask, elem, span);
                        this.hir_binary(op, x, count, elem, span)
                    }
                }
            }),
        )
    }

    /// `-v` over a `Simd` operand: floats negate, integers wrap.
    pub(super) fn lower_simd_neg(
        &mut self,
        operand: &AstExpr,
        shape: SimdShape,
        span: Span,
    ) -> HirExprKind {
        let v = self.lower_expr(operand);
        let elem = shape.elem;
        let float = self.is_float(elem);
        self.simd_lane_loop(shape, vec![v], elem, span, |this, lanes, span| {
            let x = lanes[0].clone();
            if float {
                this.hir_expr(
                    elem,
                    span,
                    HirExprKind::Unary {
                        op: HirUnaryOp::Neg,
                        operand: Box::new(x),
                    },
                )
            } else {
                let zero = this.hir_int(0, elem, span);
                this.hir_expr(
                    elem,
                    span,
                    HirExprKind::MethodCall {
                        receiver: Box::new(zero),
                        name: Ident::new("__gos_wrapping_sub"),
                        args: vec![x],
                        owner: None,
                    },
                )
            }
        })
    }

    /// A method on a `Simd` receiver, or `None` for a name the type does not
    /// declare (the checker has already reported it).
    pub(super) fn lower_simd_method(
        &mut self,
        receiver: &AstExpr,
        name: &str,
        args: &[AstExpr],
        shape: SimdShape,
        result: NodeId,
        span: Span,
    ) -> Option<HirExprKind> {
        if let Some(kind) = self.lower_simd_lane_method(receiver, name, args, shape, result, span) {
            return Some(kind);
        }
        if let Some(kind) = self.lower_simd_mask_method(receiver, name, args, shape, result, span) {
            return Some(kind);
        }
        if let Some(kind) =
            self.lower_simd_convert_method(receiver, name, args, shape, result, span)
        {
            return Some(kind);
        }
        self.lower_simd_shuffle_method(receiver, name, args, shape, result, span)
    }

    /// Lane-wise arithmetic, windows, and reductions.
    fn lower_simd_lane_method(
        &mut self,
        receiver: &AstExpr,
        name: &str,
        args: &[AstExpr],
        shape: SimdShape,
        result: NodeId,
        span: Span,
    ) -> Option<HirExprKind> {
        let elem = shape.elem;
        let float = self.is_float(elem);
        let bool_ty = self.tcx.bool_ty();
        Some(match (name, args.len()) {
            ("to_array", 0) => self.lower_expr(receiver).kind,
            ("store", 2) => self.lower_simd_store(receiver, &args[0], &args[1], shape, span)?,
            ("store_prefix", 3) => {
                self.lower_simd_store_prefix(receiver, &args[0], &args[1], &args[2], shape, span)
            }
            ("min" | "max", 1) => {
                let is_min = name == "min";
                let a = self.lower_expr(receiver);
                let b = self.lower_expr(&args[0]);
                self.simd_lane_loop(shape, vec![a, b], elem, span, |this, lanes, span| {
                    this.lane_min_max(
                        lanes[0].clone(),
                        lanes[1].clone(),
                        elem,
                        float,
                        is_min,
                        span,
                    )
                })
            }
            ("abs", 0) => {
                let a = self.lower_expr(receiver);
                self.simd_lane_loop(shape, vec![a], elem, span, |this, lanes, span| {
                    this.lane_abs(lanes[0].clone(), elem, float, span)
                })
            }
            ("sqrt", 0) if float => {
                let a = self.lower_expr(receiver);
                self.simd_lane_loop(shape, vec![a], elem, span, |this, lanes, span| {
                    let callee = this.hir_expr(
                        elem,
                        span,
                        HirExprKind::Path {
                            segments: vec![Ident::new("math"), Ident::new("sqrt")],
                            def: None,
                        },
                    );
                    this.hir_expr(
                        elem,
                        span,
                        HirExprKind::Call {
                            callee: Box::new(callee),
                            args: vec![lanes[0].clone()],
                        },
                    )
                })
            }
            ("reduce_sum" | "reduce_min" | "reduce_max" | "reduce_and" | "reduce_or", 0) => {
                let result_ty = self.table.get(result).unwrap_or(elem);
                self.lower_simd_reduce(receiver, name, shape, result_ty, span)?
            }
            ("saturating_add" | "saturating_sub", 1) => {
                let adding = name == "saturating_add";
                let a = self.lower_expr(receiver);
                let b = self.lower_expr(&args[0]);
                self.simd_lane_loop(shape, vec![a, b], elem, span, |this, lanes, span| {
                    this.lane_saturating(lanes[0].clone(), lanes[1].clone(), elem, adding, span)
                })
            }
            ("abs_diff", 1) => {
                let unsigned = self.result_shape(result)?.elem;
                let a = self.lower_expr(receiver);
                let b = self.lower_expr(&args[0]);
                self.simd_lane_loop(shape, vec![a, b], unsigned, span, |this, lanes, span| {
                    let (x, y) = (lanes[0].clone(), lanes[1].clone());
                    let x_wins =
                        this.hir_binary(HirBinaryOp::Gt, x.clone(), y.clone(), bool_ty, span);
                    let forward =
                        this.wrapping(x.clone(), y.clone(), "__gos_wrapping_sub", elem, span);
                    let backward = this.wrapping(y, x, "__gos_wrapping_sub", elem, span);
                    let diff = this.hir_if(x_wins, forward, backward, elem, span);
                    this.hir_cast(diff, unsigned, span)
                })
            }
            ("mul_add", 2) => {
                let owner = if self.float_width(elem) == 32 {
                    "f32"
                } else {
                    "f64"
                };
                let a = self.lower_expr(receiver);
                let b = self.lower_expr(&args[0]);
                let c = self.lower_expr(&args[1]);
                self.simd_lane_loop(shape, vec![a, b, c], elem, span, |this, lanes, span| {
                    this.hir_call_path(&[owner, "mul_add"], lanes.to_vec(), elem, span)
                })
            }
            _ => return None,
        })
    }

    /// Lane comparisons and the mask operations over them.
    fn lower_simd_mask_method(
        &mut self,
        receiver: &AstExpr,
        name: &str,
        args: &[AstExpr],
        shape: SimdShape,
        result: NodeId,
        span: Span,
    ) -> Option<HirExprKind> {
        let elem = shape.elem;
        let bool_ty = self.tcx.bool_ty();
        Some(match (name, args.len()) {
            ("lanes_eq" | "lanes_lt" | "lanes_le" | "lanes_gt" | "lanes_ge" | "lanes_ne", 1) => {
                let op = match name {
                    "lanes_eq" => HirBinaryOp::Eq,
                    "lanes_lt" => HirBinaryOp::Lt,
                    "lanes_le" => HirBinaryOp::Le,
                    "lanes_gt" => HirBinaryOp::Gt,
                    "lanes_ge" => HirBinaryOp::Ge,
                    _ => HirBinaryOp::Ne,
                };
                let a = self.lower_expr(receiver);
                let b = self.lower_expr(&args[0]);
                self.simd_lane_loop(shape, vec![a, b], bool_ty, span, |this, lanes, span| {
                    this.hir_binary(op, lanes[0].clone(), lanes[1].clone(), bool_ty, span)
                })
            }
            ("select", 2) => {
                let mask = self.lower_expr(receiver);
                let a = self.lower_expr(&args[0]);
                let b = self.lower_expr(&args[1]);
                let lane_ty = self
                    .simd_shape(args[0].id)
                    .map_or(elem, |value_shape| value_shape.elem);
                self.simd_lane_loop(
                    shape,
                    vec![mask, a, b],
                    lane_ty,
                    span,
                    |this, lanes, span| {
                        this.hir_if(
                            lanes[0].clone(),
                            lanes[1].clone(),
                            lanes[2].clone(),
                            lane_ty,
                            span,
                        )
                    },
                )
            }
            ("any" | "all", 0) => {
                let ArrayLen::Concrete(lanes_in) = shape.lanes else {
                    let fold = if name == "any" {
                        "reduce_or"
                    } else {
                        "reduce_and"
                    };
                    return self.lower_simd_reduce(receiver, fold, shape, bool_ty, span);
                };
                let u64_ty = self.tcx.int_ty(IntTy::U64);
                let (stmts, bits) = self.simd_mask_bits(receiver, lanes_in, span);
                let (op, target) = if name == "any" {
                    (HirBinaryOp::Ne, 0)
                } else {
                    (HirBinaryOp::Eq, (1i64 << lanes_in) - 1)
                };
                let target = self.hir_int(target, u64_ty, span);
                let tail = self.hir_binary(op, bits, target, bool_ty, span);
                HirExprKind::Block(HirBlock {
                    id: self.fresh(),
                    span,
                    stmts,
                    tail: Some(Box::new(tail)),
                    ty: bool_ty,
                    is_comptime: false,
                })
            }
            ("to_bitmask", 0) => {
                let ArrayLen::Concrete(lanes_in) = shape.lanes else {
                    return None;
                };
                let u64_ty = self.tcx.int_ty(IntTy::U64);
                let (stmts, bits) = self.simd_mask_bits(receiver, lanes_in, span);
                HirExprKind::Block(HirBlock {
                    id: self.fresh(),
                    span,
                    stmts,
                    tail: Some(Box::new(bits)),
                    ty: u64_ty,
                    is_comptime: false,
                })
            }
            ("first_set", 0) => self.lower_simd_first_set(receiver, shape, result, span)?,
            _ => return None,
        })
    }

    /// Lane conversions: casts, bits, widening, and narrowing.
    fn lower_simd_convert_method(
        &mut self,
        receiver: &AstExpr,
        name: &str,
        args: &[AstExpr],
        shape: SimdShape,
        result: NodeId,
        span: Span,
    ) -> Option<HirExprKind> {
        let elem = shape.elem;
        let float = self.is_float(elem);
        Some(match (name, args.len()) {
            ("cast", 0) => {
                let target = self.result_shape(result)?.elem;
                let a = self.lower_expr(receiver);
                self.simd_lane_loop(shape, vec![a], target, span, |this, lanes, span| {
                    this.hir_cast(lanes[0].clone(), target, span)
                })
            }
            ("to_bits", 0) => {
                let target = self.result_shape(result)?.elem;
                let a = self.lower_expr(receiver);
                let owner = float.then(|| {
                    if self.float_width(elem) == 32 {
                        "f32"
                    } else {
                        "f64"
                    }
                });
                self.simd_lane_loop(
                    shape,
                    vec![a],
                    target,
                    span,
                    |this, lanes, span| match owner {
                        Some(owner) => this.hir_call_path(
                            &[owner, "to_bits"],
                            vec![lanes[0].clone()],
                            target,
                            span,
                        ),
                        None => this.hir_cast(lanes[0].clone(), target, span),
                    },
                )
            }
            ("widen_low" | "widen_high", 0) => {
                let out = self.result_shape(result)?;
                let ArrayLen::Concrete(half) = out.lanes else {
                    return None;
                };
                let start = if name == "widen_low" { 0 } else { half };
                let (bind, v_name, v_ty) = self.simd_bind(receiver, span);
                let lanes = (0..half)
                    .map(|j| {
                        let lane = self.simd_lane(&v_name, v_ty, start + j, elem, span);
                        self.hir_cast(lane, out.elem, span)
                    })
                    .collect();
                self.simd_array_block(vec![bind], lanes, out, span)
            }
            ("narrow", 1) => {
                let out = self.result_shape(result)?;
                let ArrayLen::Concrete(lanes_in) = shape.lanes else {
                    return None;
                };
                let (bind_a, a_name, a_ty) = self.simd_bind(receiver, span);
                let (bind_b, b_name, b_ty) = self.simd_bind(&args[0], span);
                let mut lanes = Vec::with_capacity(lanes_in * 2);
                for (name, ty) in [(&a_name, a_ty), (&b_name, b_ty)] {
                    for j in 0..lanes_in {
                        let lane = self.simd_lane(name, ty, j, elem, span);
                        lanes.push(self.lane_narrow(lane, elem, out.elem, span));
                    }
                }
                self.simd_array_block(vec![bind_a, bind_b], lanes, out, span)
            }
            _ => return None,
        })
    }

    /// Shuffles: literal and dynamic swizzles and interleaving.
    fn lower_simd_shuffle_method(
        &mut self,
        receiver: &AstExpr,
        name: &str,
        args: &[AstExpr],
        shape: SimdShape,
        result: NodeId,
        span: Span,
    ) -> Option<HirExprKind> {
        let elem = shape.elem;
        let bool_ty = self.tcx.bool_ty();
        Some(match (name, args.len()) {
            ("swizzle", 1) | ("concat_swizzle", 2) => {
                let out = self.result_shape(result)?;
                let ArrayLen::Concrete(lanes_in) = shape.lanes else {
                    return None;
                };
                let indices = swizzle_literals(args.last()?)?;
                let (bind_a, a_name, a_ty) = self.simd_bind(receiver, span);
                let mut stmts = vec![bind_a];
                let second = if name == "concat_swizzle" {
                    let (bind_b, b_name, b_ty) = self.simd_bind(&args[0], span);
                    stmts.push(bind_b);
                    Some((b_name, b_ty))
                } else {
                    None
                };
                let lanes = indices
                    .into_iter()
                    .map(|index| match &second {
                        Some((b_name, b_ty)) if index >= lanes_in => {
                            self.simd_lane(b_name, *b_ty, index - lanes_in, elem, span)
                        }
                        _ => self.simd_lane(&a_name, a_ty, index, elem, span),
                    })
                    .collect();
                self.simd_array_block(stmts, lanes, out, span)
            }
            ("interleave", 1) => self.lower_simd_interleave(receiver, &args[0], shape, span)?,
            ("swizzle_dyn", 1) => {
                let i64_ty = self.i64_ty();
                let (bind, v_name, v_ty) = self.simd_bind(receiver, span);
                let idx = self.lower_expr(&args[0]);
                let witness = self.hir_path(&v_name, v_ty, span);
                let count = self.lane_count_expr(shape, Some(witness), span)?;
                let looped =
                    self.simd_lane_loop(shape, vec![idx], elem, span, |this, lanes, span| {
                        let index = this.hir_cast(lanes[0].clone(), i64_ty, span);
                        let inside = this.hir_binary(
                            HirBinaryOp::Lt,
                            index.clone(),
                            count.clone(),
                            bool_ty,
                            span,
                        );
                        let picked = this.simd_index(&v_name, v_ty, index, elem, span);
                        let zero = this.zero_of(elem, span);
                        this.hir_if(inside, picked, zero, elem, span)
                    });
                let ty = v_ty;
                let tail = self.hir_expr(ty, span, looped);
                HirExprKind::Block(HirBlock {
                    id: self.fresh(),
                    span,
                    stmts: vec![bind],
                    tail: Some(Box::new(tail)),
                    ty,
                    is_comptime: false,
                })
            }
            _ => return None,
        })
    }

    /// `a.interleave(b)`: the lanes of `a` and `b` alternating, as two vectors.
    fn lower_simd_interleave(
        &mut self,
        receiver: &AstExpr,
        other: &AstExpr,
        shape: SimdShape,
        span: Span,
    ) -> Option<HirExprKind> {
        let elem = shape.elem;
        let ArrayLen::Concrete(lanes_in) = shape.lanes else {
            return None;
        };
        let (bind_a, a_name, a_ty) = self.simd_bind(receiver, span);
        let (bind_b, b_name, b_ty) = self.simd_bind(other, span);
        // Each half is the fixed array a vector is laid out as.
        let half_ty = self.tcx.intern(TyKind::Array {
            elem,
            len: shape.lanes,
        });
        let mut halves = Vec::with_capacity(2);
        for half in 0..2 {
            let mut lanes = Vec::with_capacity(lanes_in);
            for k in 0..lanes_in {
                let source = half * lanes_in + k;
                let (name, ty) = if source % 2 == 0 {
                    (&a_name, a_ty)
                } else {
                    (&b_name, b_ty)
                };
                lanes.push(self.simd_lane(name, ty, source / 2, elem, span));
            }
            let array = self.hir_expr(half_ty, span, HirExprKind::Array(HirArrayExpr::List(lanes)));
            halves.push(array);
        }
        let tuple_ty = self.tcx.intern(TyKind::Tuple(vec![half_ty, half_ty]));
        let tuple = self.hir_expr(tuple_ty, span, HirExprKind::Tuple(halves));
        Some(HirExprKind::Block(HirBlock {
            id: self.fresh(),
            span,
            stmts: vec![bind_a, bind_b],
            tail: Some(Box::new(tuple)),
            ty: tuple_ty,
            is_comptime: false,
        }))
    }

    /// `mask.first_set()`: the lowest true lane, or `None`.
    fn lower_simd_first_set(
        &mut self,
        receiver: &AstExpr,
        shape: SimdShape,
        result: NodeId,
        span: Span,
    ) -> Option<HirExprKind> {
        let bool_ty = self.tcx.bool_ty();
        let ArrayLen::Concrete(lanes_in) = shape.lanes else {
            return None;
        };
        let option_ty = self.table.get(result)?;
        let u64_ty = self.tcx.int_ty(IntTy::U64);
        let i64_ty = self.i64_ty();
        let (mut stmts, bits) = self.simd_mask_bits(receiver, lanes_in, span);
        let bits_name = self.simd_temp("bits");
        stmts.push(self.hir_let(&bits_name, false, u64_ty, bits, span));
        let word = self.hir_path(&bits_name, u64_ty, span);
        let zero = self.hir_int(0, u64_ty, span);
        let none_set = self.hir_binary(HirBinaryOp::Eq, word, zero, bool_ty, span);
        let none = self.hir_path("None", option_ty, span);
        let word = self.hir_path(&bits_name, u64_ty, span);
        let lowest = self.hir_call_path(
            &["math", "bits", "trailing_zeros"],
            vec![word],
            i64_ty,
            span,
        );
        let lowest = self.hir_cast(lowest, i64_ty, span);
        let some = self.hir_call_path(&["Some"], vec![lowest], option_ty, span);
        let tail = self.hir_if(none_set, none, some, option_ty, span);
        Some(HirExprKind::Block(HirBlock {
            id: self.fresh(),
            span,
            stmts,
            tail: Some(Box::new(tail)),
            ty: option_ty,
            is_comptime: false,
        }))
    }

    /// The bitmask of the mask `receiver` as one `u64` expression, lane `i`
    /// at bit `i`, with the bindings it reads. A lane comparison is read
    /// in place, each lane's test feeding its bit, so no mask is built in
    /// between.
    fn simd_mask_bits(
        &mut self,
        receiver: &AstExpr,
        lanes: usize,
        span: Span,
    ) -> (Vec<HirStmt>, HirExpr) {
        let u64_ty = self.tcx.int_ty(IntTy::U64);
        let bool_ty = self.tcx.bool_ty();
        let comparison = match &receiver.kind {
            AstExprKind::MethodCall {
                receiver: lhs,
                name,
                args,
                ..
            } if args.len() == 1 && self.simd_shape(lhs.id).is_some() => {
                let op = match name.name.as_str() {
                    "lanes_eq" => Some(HirBinaryOp::Eq),
                    "lanes_ne" => Some(HirBinaryOp::Ne),
                    "lanes_lt" => Some(HirBinaryOp::Lt),
                    "lanes_le" => Some(HirBinaryOp::Le),
                    "lanes_gt" => Some(HirBinaryOp::Gt),
                    "lanes_ge" => Some(HirBinaryOp::Ge),
                    _ => None,
                };
                op.map(|op| (op, &**lhs, &args[0]))
            }
            _ => None,
        };
        let mut stmts = Vec::new();
        let source = if let Some((op, lhs, rhs)) = comparison {
            let elem = self.simd_shape(lhs.id).map_or(bool_ty, |shape| shape.elem);
            let (bind_a, a_name, a_ty) = self.simd_bind(lhs, span);
            let (bind_b, b_name, b_ty) = self.simd_bind(rhs, span);
            stmts.push(bind_a);
            stmts.push(bind_b);
            MaskLanes::Compare {
                op,
                lhs: (a_name, a_ty),
                rhs: (b_name, b_ty),
                elem,
            }
        } else {
            let (bind, m_name, m_ty) = self.simd_bind(receiver, span);
            stmts.push(bind);
            MaskLanes::Mask(m_name, m_ty)
        };
        let bits: Vec<HirExpr> = (0..lanes)
            .map(|i| {
                let test = self.mask_lane_test(&source, i, span);
                let bit = self.hir_cast(test, u64_ty, span);
                if i == 0 {
                    bit
                } else {
                    let shift = self.hir_int(i64::try_from(i).unwrap_or(0), u64_ty, span);
                    self.hir_binary(HirBinaryOp::Shl, bit, shift, u64_ty, span)
                }
            })
            .collect();
        let zero = self.hir_int(0, u64_ty, span);
        let word = self
            .balanced_tree(
                bits,
                &mut |this: &mut Self, a: HirExpr, b: HirExpr, span: Span| {
                    this.hir_binary(HirBinaryOp::BitOr, a, b, u64_ty, span)
                },
                span,
            )
            .unwrap_or(zero);
        (stmts, word)
    }

    /// Lane `i` of a mask as a `bool`: the lane comparison it is built from,
    /// read in place, or the lane of a mask value.
    fn mask_lane_test(&mut self, source: &MaskLanes, i: usize, span: Span) -> HirExpr {
        let bool_ty = self.tcx.bool_ty();
        match source {
            MaskLanes::Compare { op, lhs, rhs, elem } => {
                let x = self.simd_lane(&lhs.0, lhs.1, i, *elem, span);
                let y = self.simd_lane(&rhs.0, rhs.1, i, *elem, span);
                self.hir_binary(*op, x, y, bool_ty, span)
            }
            MaskLanes::Mask(name, ty) => self.simd_lane(name, *ty, i, bool_ty, span),
        }
    }

    /// The `Simd` shape of the value `node` answers.
    fn result_shape(&self, node: NodeId) -> Option<SimdShape> {
        match self.tcx.kind(self.table.get(node)?)? {
            TyKind::Simd { elem, lanes } => Some(SimdShape {
                elem: *elem,
                lanes: *lanes,
            }),
            _ => None,
        }
    }

    /// `let v = <expr>`, with the binding's name and type.
    fn simd_bind(&mut self, expr: &AstExpr, span: Span) -> (HirStmt, String, Ty) {
        let value = self.lower_expr(expr);
        let ty = value.ty;
        let name = self.simd_temp("v");
        (self.hir_let(&name, false, ty, value, span), name, ty)
    }

    /// `name[lane]` for a literal lane.
    fn simd_lane(&mut self, name: &str, ty: Ty, lane: usize, elem: Ty, span: Span) -> HirExpr {
        let i64_ty = self.i64_ty();
        let index = self.hir_int(i64::try_from(lane).unwrap_or(0), i64_ty, span);
        self.simd_index(name, ty, index, elem, span)
    }

    /// `{ stmts; [lanes..] }` as a vector of shape `out`.
    fn simd_array_block(
        &mut self,
        stmts: Vec<HirStmt>,
        lanes: Vec<HirExpr>,
        out: SimdShape,
        span: Span,
    ) -> HirExprKind {
        let ty = self.tcx.intern(TyKind::Array {
            elem: out.elem,
            len: out.lanes,
        });
        let array = self.hir_expr(ty, span, HirExprKind::Array(HirArrayExpr::List(lanes)));
        HirExprKind::Block(HirBlock {
            id: self.fresh(),
            span,
            stmts,
            tail: Some(Box::new(array)),
            ty,
            is_comptime: false,
        })
    }

    fn hir_cast(&mut self, value: HirExpr, ty: Ty, span: Span) -> HirExpr {
        self.hir_expr(
            ty,
            span,
            HirExprKind::Cast {
                value: Box::new(value),
                ty,
            },
        )
    }

    /// `owner::name(args)`, a call to a function the path names.
    fn hir_call_path(&mut self, path: &[&str], args: Vec<HirExpr>, ty: Ty, span: Span) -> HirExpr {
        let callee = self.hir_expr(
            ty,
            span,
            HirExprKind::Path {
                segments: path.iter().map(|segment| Ident::new(*segment)).collect(),
                def: None,
            },
        );
        self.hir_expr(
            ty,
            span,
            HirExprKind::Call {
                callee: Box::new(callee),
                args,
            },
        )
    }

    /// `a <method> b` with a wrapping integer operation.
    fn wrapping(&mut self, a: HirExpr, b: HirExpr, method: &str, ty: Ty, span: Span) -> HirExpr {
        self.hir_expr(
            ty,
            span,
            HirExprKind::MethodCall {
                receiver: Box::new(a),
                name: Ident::new(method),
                args: vec![b],
                owner: None,
            },
        )
    }

    /// The width of a float lane, 32 or 64.
    fn float_width(&self, ty: Ty) -> u32 {
        match self.tcx.kind(ty) {
            Some(TyKind::Float(gossamer_types::FloatTy::F32)) => 32,
            _ => 64,
        }
    }

    fn is_signed(&self, ty: Ty) -> bool {
        matches!(
            self.tcx.kind(ty),
            Some(TyKind::Int(
                IntTy::I8 | IntTy::I16 | IntTy::I32 | IntTy::I64 | IntTy::Isize
            ))
        )
    }

    /// `a + b` or `a - b` clamped to the lane type's range: the wrapped
    /// result, replaced by the bound it crossed when it overflowed.
    fn lane_saturating(
        &mut self,
        a: HirExpr,
        b: HirExpr,
        elem: Ty,
        adding: bool,
        span: Span,
    ) -> HirExpr {
        let bool_ty = self.tcx.bool_ty();
        let method = if adding {
            "__gos_wrapping_add"
        } else {
            "__gos_wrapping_sub"
        };
        let r = self.wrapping(a.clone(), b.clone(), method, elem, span);
        let (min, max) = self.int_bounds(elem, span);
        if !self.is_signed(elem) {
            // Unsigned: an add that wrapped answers less than it started at;
            // a subtraction of more than there is goes below zero.
            return if adding {
                let wrapped = self.hir_binary(HirBinaryOp::Lt, r.clone(), a, bool_ty, span);
                self.hir_if(wrapped, max, r, elem, span)
            } else {
                let under = self.hir_binary(HirBinaryOp::Gt, b, a, bool_ty, span);
                self.hir_if(under, min, r, elem, span)
            };
        }
        // Signed: the result moved the wrong way for the sign of `b`.
        let zero = self.hir_int(0, elem, span);
        let b_positive = self.hir_binary(HirBinaryOp::Gt, b.clone(), zero.clone(), bool_ty, span);
        let b_negative = self.hir_binary(HirBinaryOp::Lt, b, zero, bool_ty, span);
        let (up, down) = if adding {
            (HirBinaryOp::Lt, HirBinaryOp::Gt)
        } else {
            (HirBinaryOp::Gt, HirBinaryOp::Lt)
        };
        let over_one = self.hir_binary(up, r.clone(), a.clone(), bool_ty, span);
        let over_other = self.hir_binary(down, r.clone(), a, bool_ty, span);
        let first = self.hir_binary(HirBinaryOp::And, b_positive, over_one, bool_ty, span);
        let second = self.hir_binary(HirBinaryOp::And, b_negative, over_other, bool_ty, span);
        let (first_bound, second_bound) = if adding { (max, min) } else { (min, max) };
        let inner = self.hir_if(second, second_bound, r, elem, span);
        self.hir_if(first, first_bound, inner, elem, span)
    }

    /// The least and greatest values of an integer lane type.
    fn int_bounds(&mut self, ty: Ty, span: Span) -> (HirExpr, HirExpr) {
        let (min, max): (i128, i128) = match self.tcx.kind(ty) {
            Some(TyKind::Int(IntTy::I8)) => (i8::MIN.into(), i8::MAX.into()),
            Some(TyKind::Int(IntTy::U8)) => (0, u8::MAX.into()),
            Some(TyKind::Int(IntTy::I16)) => (i16::MIN.into(), i16::MAX.into()),
            Some(TyKind::Int(IntTy::U16)) => (0, u16::MAX.into()),
            Some(TyKind::Int(IntTy::I32)) => (i32::MIN.into(), i32::MAX.into()),
            Some(TyKind::Int(IntTy::U32)) => (0, u32::MAX.into()),
            Some(TyKind::Int(IntTy::U64 | IntTy::Usize)) => (0, u64::MAX.into()),
            _ => (i64::MIN.into(), i64::MAX.into()),
        };
        let literal = |this: &mut Self, value: i128| {
            this.hir_expr(
                ty,
                span,
                HirExprKind::Literal(HirLiteral::Int(value.to_string())),
            )
        };
        (literal(self, min), literal(self, max))
    }

    /// One lane narrowed to `narrow`: an integer clamped to its range first,
    /// a float rounded.
    fn lane_narrow(&mut self, lane: HirExpr, elem: Ty, narrow: Ty, span: Span) -> HirExpr {
        if self.is_float(elem) {
            return self.hir_cast(lane, narrow, span);
        }
        let bool_ty = self.tcx.bool_ty();
        let (min, max) = self.int_bounds(narrow, span);
        let min = self.hir_cast(min, elem, span);
        let max = self.hir_cast(max, elem, span);
        let above = self.hir_binary(HirBinaryOp::Gt, lane.clone(), max.clone(), bool_ty, span);
        let clamped = if self.is_signed(elem) {
            let below = self.hir_binary(HirBinaryOp::Lt, lane.clone(), min.clone(), bool_ty, span);
            let low = self.hir_if(below, min, lane, elem, span);
            self.hir_if(above, max, low, elem, span)
        } else {
            self.hir_if(above, max, lane, elem, span)
        };
        self.hir_cast(clamped, narrow, span)
    }

    /// `v.store_prefix(&mut xs, offset, n)`: the first `n` lanes written to
    /// `xs[offset..offset + n]`, each write checked.
    fn lower_simd_store_prefix(
        &mut self,
        receiver: &AstExpr,
        target: &AstExpr,
        offset: &AstExpr,
        count: &AstExpr,
        shape: SimdShape,
        span: Span,
    ) -> HirExprKind {
        let elem = shape.elem;
        let i64_ty = self.i64_ty();
        let unit_ty = self.tcx.unit();
        let (bind, value_name, value_ty) = self.simd_bind(receiver, span);
        let mut stmts = vec![bind];
        let place = match &target.kind {
            AstExprKind::Unary {
                op: AstUnaryOp::RefMut,
                operand,
            } => &**operand,
            _ => target,
        };
        let target = WindowSeq::Place(place);
        let offset = self.lower_expr(offset);
        let offset_name = self.simd_temp("off");
        stmts.push(self.hir_let(&offset_name, false, i64_ty, offset, span));
        let count = self.lower_expr(count);
        let count_name = self.simd_temp("count");
        stmts.push(self.hir_let(&count_name, false, i64_ty, count, span));
        let lane_name = self.simd_temp("i");
        let start = self.hir_int(0, i64_ty, span);
        stmts.push(self.hir_let(&lane_name, true, i64_ty, start, span));
        let at = self.simd_window_position(&offset_name, &lane_name, span);
        let dst = self.simd_seq_index(&target, at, elem, span);
        let lane = self.hir_path(&lane_name, i64_ty, span);
        let src = self.simd_index(&value_name, value_ty, lane, elem, span);
        let write = self.hir_assign(dst, src, span);
        let bump = self.hir_increment(&lane_name, span);
        let bound = self.hir_path(&count_name, i64_ty, span);
        stmts.push(self.hir_while_lt(&lane_name, bound, vec![write, bump], span));
        HirExprKind::Block(HirBlock {
            id: self.fresh(),
            span,
            stmts,
            tail: None,
            ty: unit_ty,
            is_comptime: false,
        })
    }

    /// A call naming the vector type or rendering a vector: `Simd::splat(v)`,
    /// `Simd::from_array(a)`, and a `{}` / `{:?}` rendering, which spells the
    /// lanes inside `Simd(..)`.
    pub(super) fn lower_simd_call(
        &mut self,
        call: &AstExpr,
        callee: &AstExpr,
        args: &[AstExpr],
    ) -> Option<HirExprKind> {
        let AstExprKind::Path(path) = &callee.kind else {
            return None;
        };
        let names: Vec<&str> = path.segments.iter().map(|s| s.name.name.as_str()).collect();
        match names.as_slice() {
            ["Simd" | "Mask", last] => {
                let shape = self.simd_shape(call.id)?;
                self.lower_simd_ctor(last, args, shape, call.span)
            }
            [render @ ("__debug" | "__concat")]
                if args.iter().any(|arg| self.simd_shape(arg.id).is_some()) =>
            {
                // Each vector part spells itself `Simd(..)` around its lanes,
                // whatever text surrounds it.
                let span = call.span;
                let string_ty = self.tcx.string_ty();
                let mut parts = Vec::with_capacity(args.len());
                for arg in args {
                    if self.simd_shape(arg.id).is_none() {
                        parts.push(self.lower_expr(arg));
                        continue;
                    }
                    let lanes = self.lower_expr(arg);
                    let rendered = self.hir_call_path(&[*render], vec![lanes], string_ty, span);
                    let open = self.hir_expr(
                        string_ty,
                        span,
                        HirExprKind::Literal(HirLiteral::String("Simd(".to_string())),
                    );
                    let close = self.hir_expr(
                        string_ty,
                        span,
                        HirExprKind::Literal(HirLiteral::String(")".to_string())),
                    );
                    parts.push(self.hir_call_path(
                        &["__concat"],
                        vec![open, rendered, close],
                        string_ty,
                        span,
                    ));
                }
                // `{:?}` renders one value, which is already the whole text.
                if *render == "__debug" && parts.len() == 1 {
                    return parts.pop().map(|part| part.kind);
                }
                let callee = self.hir_expr(
                    string_ty,
                    span,
                    HirExprKind::Path {
                        segments: vec![Ident::new("__concat")],
                        def: None,
                    },
                );
                Some(HirExprKind::Call {
                    callee: Box::new(callee),
                    args: parts,
                })
            }
            _ => None,
        }
    }

    /// `Simd::splat(v)` and `Simd::from_array(a)`.
    pub(super) fn lower_simd_ctor(
        &mut self,
        last: &str,
        args: &[AstExpr],
        shape: SimdShape,
        span: Span,
    ) -> Option<HirExprKind> {
        Some(match (last, args.len()) {
            ("from_array", 1) => self.lower_expr(&args[0]).kind,
            ("load", 2) => self.lower_simd_load(&args[0], &args[1], shape, span)?,
            ("splat", 1) => {
                let value = self.lower_expr(&args[0]);
                let count = self.lane_count_expr(shape, None, span)?;
                HirExprKind::Array(HirArrayExpr::Repeat {
                    value: Box::new(value),
                    count: Box::new(count),
                })
            }
            ("from_bitmask", 1) => {
                let ArrayLen::Concrete(lanes_in) = shape.lanes else {
                    return None;
                };
                let u64_ty = self.tcx.int_ty(IntTy::U64);
                let bool_ty = self.tcx.bool_ty();
                let bits = self.lower_expr(&args[0]);
                let bits_name = self.simd_temp("bits");
                let bind = self.hir_let(&bits_name, false, u64_ty, bits, span);
                let lanes = (0..lanes_in)
                    .map(|i| {
                        let word = self.hir_path(&bits_name, u64_ty, span);
                        let shift = self.hir_int(i64::try_from(i).unwrap_or(0), u64_ty, span);
                        let moved = self.hir_binary(HirBinaryOp::Shr, word, shift, u64_ty, span);
                        let one = self.hir_int(1, u64_ty, span);
                        let bit = self.hir_binary(HirBinaryOp::BitAnd, moved, one, u64_ty, span);
                        let zero = self.hir_int(0, u64_ty, span);
                        self.hir_binary(HirBinaryOp::Ne, bit, zero, bool_ty, span)
                    })
                    .collect();
                self.simd_array_block(vec![bind], lanes, shape, span)
            }
            ("from_bits", 1) => {
                let elem = shape.elem;
                let bits = self.lower_expr(&args[0]);
                let owner = self.is_float(elem).then(|| {
                    if self.float_width(elem) == 32 {
                        "f32"
                    } else {
                        "f64"
                    }
                });
                self.simd_lane_loop(
                    shape,
                    vec![bits],
                    elem,
                    span,
                    |this, lanes, span| match owner {
                        Some(owner) => this.hir_call_path(
                            &[owner, "from_bits"],
                            vec![lanes[0].clone()],
                            elem,
                            span,
                        ),
                        None => this.hir_cast(lanes[0].clone(), elem, span),
                    },
                )
            }
            ("load_or", 3) => self.lower_simd_load_or(&args[0], &args[1], &args[2], shape, span)?,
            ("gather", 2) => {
                let elem = shape.elem;
                let i64_ty = self.i64_ty();
                let mut stmts = Vec::new();
                let source = self.simd_window_seq(&args[0], &mut stmts, span);
                let indices = self.lower_expr(&args[1]);
                let looped =
                    self.simd_lane_loop(shape, vec![indices], elem, span, |this, lanes, span| {
                        let index = this.hir_cast(lanes[0].clone(), i64_ty, span);
                        this.simd_seq_index(&source, index, elem, span)
                    });
                let ty = self.tcx.intern(TyKind::Array {
                    elem,
                    len: shape.lanes,
                });
                let tail = self.hir_expr(ty, span, looped);
                HirExprKind::Block(HirBlock {
                    id: self.fresh(),
                    span,
                    stmts,
                    tail: Some(Box::new(tail)),
                    ty,
                    is_comptime: false,
                })
            }
            _ => return None,
        })
    }

    /// `Simd::load_or(xs, offset, fill)`: lane `i` is `xs[offset + i]` where
    /// that index lies inside `xs`, and `fill` past either end.
    fn lower_simd_load_or(
        &mut self,
        source: &AstExpr,
        offset: &AstExpr,
        fill: &AstExpr,
        shape: SimdShape,
        span: Span,
    ) -> Option<HirExprKind> {
        let elem = shape.elem;
        let i64_ty = self.i64_ty();
        let bool_ty = self.tcx.bool_ty();
        let count = self.lane_count_expr(shape, None, span)?;
        let mut stmts = Vec::new();
        let source = self.simd_window_seq(source, &mut stmts, span);
        let offset = self.lower_expr(offset);
        let offset_name = self.simd_temp("off");
        stmts.push(self.hir_let(&offset_name, false, i64_ty, offset, span));
        let fill = self.lower_expr(fill);
        let fill_name = self.simd_temp("fill");
        stmts.push(self.hir_let(&fill_name, false, elem, fill, span));
        let seq = self.simd_seq_expr(&source, span);
        let len = self.hir_expr(
            i64_ty,
            span,
            HirExprKind::MethodCall {
                receiver: Box::new(seq),
                name: Ident::new("len"),
                args: Vec::new(),
                owner: None,
            },
        );
        let len_name = self.simd_temp("len");
        stmts.push(self.hir_let(&len_name, false, i64_ty, len, span));
        let count_name = self.simd_temp("count");
        stmts.push(self.hir_let(&count_name, false, i64_ty, count, span));
        let out_ty = self.tcx.intern(TyKind::Array {
            elem,
            len: shape.lanes,
        });
        let filler = self.hir_path(&fill_name, elem, span);
        let lanes = self.hir_path(&count_name, i64_ty, span);
        let init = self.hir_expr(
            out_ty,
            span,
            HirExprKind::Array(HirArrayExpr::Repeat {
                value: Box::new(filler),
                count: Box::new(lanes),
            }),
        );
        let out_name = self.simd_temp("out");
        stmts.push(self.hir_let(&out_name, true, out_ty, init, span));
        let lane_name = self.simd_temp("i");
        let start = self.hir_int(0, i64_ty, span);
        stmts.push(self.hir_let(&lane_name, true, i64_ty, start, span));
        // if 0 <= offset + i && offset + i < len { out[i] = xs[offset + i] }
        let at = self.simd_window_position(&offset_name, &lane_name, span);
        let zero = self.hir_int(0, i64_ty, span);
        let not_before = self.hir_binary(HirBinaryOp::Ge, at.clone(), zero, bool_ty, span);
        let end = self.hir_path(&len_name, i64_ty, span);
        let not_after = self.hir_binary(HirBinaryOp::Lt, at.clone(), end, bool_ty, span);
        let inside = self.hir_binary(HirBinaryOp::And, not_before, not_after, bool_ty, span);
        let lane = self.hir_path(&lane_name, i64_ty, span);
        let dst = self.simd_index(&out_name, out_ty, lane, elem, span);
        let src = self.simd_seq_index(&source, at, elem, span);
        let copy = self.hir_assign(dst, src, span);
        let unit_ty = self.tcx.unit();
        let then = self.hir_unit_block(vec![copy], span);
        let guarded = self.hir_expr(
            unit_ty,
            span,
            HirExprKind::If {
                condition: Box::new(inside),
                then_branch: Box::new(then),
                else_branch: None,
            },
        );
        let guarded = HirStmt {
            id: self.fresh(),
            span,
            kind: HirStmtKind::Expr {
                expr: guarded,
                has_semi: true,
            },
        };
        let bump = self.hir_increment(&lane_name, span);
        let bound = self.hir_path(&count_name, i64_ty, span);
        stmts.push(self.hir_while_lt(&lane_name, bound, vec![guarded, bump], span));
        let tail = self.hir_path(&out_name, out_ty, span);
        Some(HirExprKind::Block(HirBlock {
            id: self.fresh(),
            span,
            stmts,
            tail: Some(Box::new(tail)),
            ty: out_ty,
            is_comptime: false,
        }))
    }

    fn lower_simd_reduce(
        &mut self,
        receiver: &AstExpr,
        name: &str,
        shape: SimdShape,
        result_ty: Ty,
        span: Span,
    ) -> Option<HirExprKind> {
        let ArrayLen::Concrete(lanes) = shape.lanes else {
            return Some(self.lower_simd_reduce_loop(receiver, name, shape, result_ty, span));
        };
        let elem = shape.elem;
        let v = self.lower_expr(receiver);
        let v_name = self.simd_temp("v");
        let v_ty = v.ty;
        let bind = self.hir_let(&v_name, false, v_ty, v, span);
        let i64_ty = self.i64_ty();
        let leaves: Vec<HirExpr> = (0..lanes)
            .map(|i| {
                let base = self.hir_path(&v_name, v_ty, span);
                let index = self.hir_int(i64::try_from(i).unwrap_or(0), i64_ty, span);
                self.hir_expr(
                    elem,
                    span,
                    HirExprKind::Index {
                        base: Box::new(base),
                        index: Box::new(index),
                    },
                )
            })
            .collect();
        let tail = self.balanced_tree(
            leaves,
            &mut |this: &mut Self, a: HirExpr, b: HirExpr, span: Span| {
                this.simd_reduce_step(name, elem, result_ty, a, b, span)
            },
            span,
        )?;
        Some(HirExprKind::Block(HirBlock {
            id: self.fresh(),
            span,
            stmts: vec![bind],
            tail: Some(Box::new(tail)),
            ty: result_ty,
            is_comptime: false,
        }))
    }

    /// `Simd::load(xs, offset)`: the lanes `xs[offset..offset + N]`, after one
    /// check that the whole window lies inside `xs`.
    fn lower_simd_load(
        &mut self,
        source: &AstExpr,
        offset: &AstExpr,
        shape: SimdShape,
        span: Span,
    ) -> Option<HirExprKind> {
        let elem = shape.elem;
        let i64_ty = self.i64_ty();
        let count = self.lane_count_expr(shape, None, span)?;
        let mut stmts = Vec::new();
        let source = self.simd_window_seq(source, &mut stmts, span);
        let offset = self.lower_expr(offset);
        let offset_name = self.simd_temp("off");
        stmts.push(self.hir_let(&offset_name, false, i64_ty, offset, span));
        let count_name = self.simd_temp("count");
        stmts.push(self.hir_let(&count_name, false, i64_ty, count, span));
        stmts.extend(self.simd_window_check(&source, &offset_name, &count_name, elem, span));

        let out_ty = self.tcx.intern(TyKind::Array {
            elem,
            len: shape.lanes,
        });
        let zero = self.zero_of(elem, span);
        let lanes = self.hir_path(&count_name, i64_ty, span);
        let init = self.hir_expr(
            out_ty,
            span,
            HirExprKind::Array(HirArrayExpr::Repeat {
                value: Box::new(zero),
                count: Box::new(lanes),
            }),
        );
        let out_name = self.simd_temp("out");
        stmts.push(self.hir_let(&out_name, true, out_ty, init, span));
        let lane_name = self.simd_temp("i");
        let start = self.hir_int(0, i64_ty, span);
        stmts.push(self.hir_let(&lane_name, true, i64_ty, start, span));
        let lane = self.hir_path(&lane_name, i64_ty, span);
        let dst = self.simd_index(&out_name, out_ty, lane, elem, span);
        let at = self.simd_window_position(&offset_name, &lane_name, span);
        let src = self.simd_seq_index(&source, at, elem, span);
        let copy = self.hir_assign(dst, src, span);
        let bump = self.hir_increment(&lane_name, span);
        let bound = self.hir_path(&count_name, i64_ty, span);
        stmts.push(self.hir_while_lt(&lane_name, bound, vec![copy, bump], span));
        let tail = self.hir_path(&out_name, out_ty, span);
        Some(HirExprKind::Block(HirBlock {
            id: self.fresh(),
            span,
            stmts,
            tail: Some(Box::new(tail)),
            ty: out_ty,
            is_comptime: false,
        }))
    }

    /// `v.store(&mut xs, offset)`: writes the lanes into
    /// `xs[offset..offset + N]`, after one check that the whole window lies
    /// inside `xs`.
    fn lower_simd_store(
        &mut self,
        receiver: &AstExpr,
        target: &AstExpr,
        offset: &AstExpr,
        shape: SimdShape,
        span: Span,
    ) -> Option<HirExprKind> {
        let elem = shape.elem;
        let i64_ty = self.i64_ty();
        let unit_ty = self.tcx.unit();
        let value = self.lower_expr(receiver);
        let value_ty = value.ty;
        let value_name = self.simd_temp("v");
        let mut stmts = vec![self.hir_let(&value_name, false, value_ty, value, span)];
        let place = match &target.kind {
            AstExprKind::Unary {
                op: AstUnaryOp::RefMut,
                operand,
            } => &**operand,
            _ => target,
        };
        let target = WindowSeq::Place(place);
        let offset = self.lower_expr(offset);
        let offset_name = self.simd_temp("off");
        stmts.push(self.hir_let(&offset_name, false, i64_ty, offset, span));
        let witness = self.hir_path(&value_name, value_ty, span);
        let count = self.lane_count_expr(shape, Some(witness), span)?;
        let count_name = self.simd_temp("count");
        stmts.push(self.hir_let(&count_name, false, i64_ty, count, span));
        stmts.extend(self.simd_window_check(&target, &offset_name, &count_name, elem, span));

        let lane_name = self.simd_temp("i");
        let start = self.hir_int(0, i64_ty, span);
        stmts.push(self.hir_let(&lane_name, true, i64_ty, start, span));
        let at = self.simd_window_position(&offset_name, &lane_name, span);
        let dst = self.simd_seq_index(&target, at, elem, span);
        let lane = self.hir_path(&lane_name, i64_ty, span);
        let src = self.simd_index(&value_name, value_ty, lane, elem, span);
        let write = self.hir_assign(dst, src, span);
        let bump = self.hir_increment(&lane_name, span);
        let bound = self.hir_path(&count_name, i64_ty, span);
        stmts.push(self.hir_while_lt(&lane_name, bound, vec![write, bump], span));
        Some(HirExprKind::Block(HirBlock {
            id: self.fresh(),
            span,
            stmts,
            tail: None,
            ty: unit_ty,
            is_comptime: false,
        }))
    }

    /// The one bounds check a window makes: reads of its first and last lanes,
    /// before any lane moves. An index outside the sequence panics with the
    /// same message on every tier, and the check is an ordinary indexed read
    /// the native tier compiles.
    fn simd_window_check(
        &mut self,
        seq: &WindowSeq<'_>,
        offset: &str,
        count: &str,
        elem: Ty,
        span: Span,
    ) -> Vec<HirStmt> {
        let i64_ty = self.i64_ty();
        let first = self.hir_path(offset, i64_ty, span);
        let off = self.hir_path(offset, i64_ty, span);
        let lanes = self.hir_path(count, i64_ty, span);
        let end = self.hir_binary(HirBinaryOp::Add, off, lanes, i64_ty, span);
        let one = self.hir_int(1, i64_ty, span);
        let last = self.hir_binary(HirBinaryOp::Sub, end, one, i64_ty, span);
        [first, last]
            .into_iter()
            .map(|index| {
                let read = self.simd_seq_index(seq, index, elem, span);
                let probe = self.simd_temp("probe");
                self.hir_let(&probe, false, elem, read, span)
            })
            .collect()
    }

    /// `{ stmts }` answering unit.
    fn hir_unit_block(&mut self, stmts: Vec<HirStmt>, span: Span) -> HirExpr {
        let unit_ty = self.tcx.unit();
        HirExpr {
            id: self.fresh(),
            span,
            ty: unit_ty,
            kind: HirExprKind::Block(HirBlock {
                id: self.fresh(),
                span,
                stmts,
                tail: None,
                ty: unit_ty,
                is_comptime: false,
            }),
        }
    }

    /// The sequence `source` names, bound once unless it is a place.
    fn simd_window_seq<'a>(
        &mut self,
        source: &'a AstExpr,
        stmts: &mut Vec<HirStmt>,
        span: Span,
    ) -> WindowSeq<'a> {
        if is_pure_place(source) {
            return WindowSeq::Place(source);
        }
        let value = self.lower_expr(source);
        let ty = value.ty;
        let name = self.simd_temp("src");
        stmts.push(self.hir_let(&name, false, ty, value, span));
        WindowSeq::Bound { name, ty }
    }

    fn simd_seq_expr(&mut self, seq: &WindowSeq<'_>, span: Span) -> HirExpr {
        match seq {
            WindowSeq::Place(expr) => self.lower_expr(expr),
            WindowSeq::Bound { name, ty } => self.hir_path(name, *ty, span),
        }
    }

    /// `seq[index]` read or written as one `elem`.
    fn simd_seq_index(
        &mut self,
        seq: &WindowSeq<'_>,
        index: HirExpr,
        elem: Ty,
        span: Span,
    ) -> HirExpr {
        let base = self.simd_seq_expr(seq, span);
        self.hir_expr(
            elem,
            span,
            HirExprKind::Index {
                base: Box::new(base),
                index: Box::new(index),
            },
        )
    }

    /// `offset + lane` as an index into a window's sequence.
    fn simd_window_position(&mut self, offset: &str, lane: &str, span: Span) -> HirExpr {
        let i64_ty = self.i64_ty();
        let off = self.hir_path(offset, i64_ty, span);
        let lane = self.hir_path(lane, i64_ty, span);
        self.hir_binary(HirBinaryOp::Add, off, lane, i64_ty, span)
    }

    /// `name[index]` read as one `elem`.
    fn simd_index(&mut self, name: &str, ty: Ty, index: HirExpr, elem: Ty, span: Span) -> HirExpr {
        let base = self.hir_path(name, ty, span);
        self.hir_expr(
            elem,
            span,
            HirExprKind::Index {
                base: Box::new(base),
                index: Box::new(index),
            },
        )
    }

    /// One step of a reduction: the pair `a`, `b` combined the way `name` folds.
    fn simd_reduce_step(
        &mut self,
        name: &str,
        elem: Ty,
        result_ty: Ty,
        a: HirExpr,
        b: HirExpr,
        span: Span,
    ) -> HirExpr {
        let float = self.is_float(elem);
        match name {
            "reduce_sum" if float => self.hir_binary(HirBinaryOp::Add, a, b, elem, span),
            "reduce_sum" => self.hir_expr(
                elem,
                span,
                HirExprKind::MethodCall {
                    receiver: Box::new(a),
                    name: Ident::new("__gos_wrapping_add"),
                    args: vec![b],
                    owner: None,
                },
            ),
            "reduce_min" => self.lane_min_max(a, b, elem, float, true, span),
            "reduce_max" => self.lane_min_max(a, b, elem, float, false, span),
            // A mask's lanes are `bool`s, which combine with the logical
            // operators every tier evaluates on them.
            "reduce_and" if matches!(self.tcx.kind(elem), Some(TyKind::Bool)) => {
                self.hir_binary(HirBinaryOp::And, a, b, result_ty, span)
            }
            "reduce_or" if matches!(self.tcx.kind(elem), Some(TyKind::Bool)) => {
                self.hir_binary(HirBinaryOp::Or, a, b, result_ty, span)
            }
            "reduce_and" => self.hir_binary(HirBinaryOp::BitAnd, a, b, result_ty, span),
            _ => self.hir_binary(HirBinaryOp::BitOr, a, b, result_ty, span),
        }
    }

    /// A reduction over a lane count known only at instantiation. The lanes
    /// are copied into a working array and paired in place, round by round,
    /// in the order [`Self::balanced_tree`] pairs them, so a const generic
    /// kernel folds exactly as the same kernel over a literal count:
    /// round `r` stores `w[k] = step(w[2k], w[2k + 1])` for each pair and
    /// carries an odd last lane to `w[half]`.
    fn lower_simd_reduce_loop(
        &mut self,
        receiver: &AstExpr,
        name: &str,
        shape: SimdShape,
        result_ty: Ty,
        span: Span,
    ) -> HirExprKind {
        let elem = shape.elem;
        let i64_ty = self.i64_ty();
        let bool_ty = self.tcx.bool_ty();
        let v = self.lower_expr(receiver);
        let v_ty = v.ty;
        let v_name = self.simd_temp("v");
        let mut stmts = vec![self.hir_let(&v_name, false, v_ty, v, span)];
        let witness = self.hir_path(&v_name, v_ty, span);
        let count = match self.lane_count_expr(shape, Some(witness), span) {
            Some(count) => count,
            None => self.hir_int(0, i64_ty, span),
        };
        let n_name = self.simd_temp("n");
        stmts.push(self.hir_let(&n_name, true, i64_ty, count.clone(), span));
        let w_ty = self.tcx.intern(TyKind::Array {
            elem,
            len: shape.lanes,
        });
        let zero = self.zero_of(elem, span);
        let init = self.hir_expr(
            w_ty,
            span,
            HirExprKind::Array(HirArrayExpr::Repeat {
                value: Box::new(zero),
                count: Box::new(count),
            }),
        );
        let w_name = self.simd_temp("w");
        stmts.push(self.hir_let(&w_name, true, w_ty, init, span));

        // w[i] = v[i] for every lane.
        let copy_i = self.simd_temp("i");
        let start = self.hir_int(0, i64_ty, span);
        stmts.push(self.hir_let(&copy_i, true, i64_ty, start, span));
        let index = self.hir_path(&copy_i, i64_ty, span);
        let dst = self.simd_index(&w_name, w_ty, index, elem, span);
        let index = self.hir_path(&copy_i, i64_ty, span);
        let src = self.simd_index(&v_name, v_ty, index, elem, span);
        let copy = self.hir_assign(dst, src, span);
        let bump = self.hir_increment(&copy_i, span);
        let bound = self.hir_path(&n_name, i64_ty, span);
        let copy_loop = self.hir_while_lt(&copy_i, bound, vec![copy, bump], span);
        stmts.push(copy_loop);

        // while n > 1 { pair round }
        let work = LaneWork {
            array: &w_name,
            array_ty: w_ty,
            count: &n_name,
            elem,
        };
        let round_body = self.simd_pair_round(work, name, result_ty, span);
        let n = self.hir_path(&n_name, i64_ty, span);
        let one = self.hir_int(1, i64_ty, span);
        let more = self.hir_binary(HirBinaryOp::Gt, n, one, bool_ty, span);
        stmts.push(self.hir_while(more, round_body, span));

        let first = self.hir_int(0, i64_ty, span);
        let tail = self.simd_index(&w_name, w_ty, first, elem, span);
        HirExprKind::Block(HirBlock {
            id: self.fresh(),
            span,
            stmts,
            tail: Some(Box::new(tail)),
            ty: result_ty,
            is_comptime: false,
        })
    }

    /// One round of the in-place pairing over the working array `w` of `n`
    /// lanes: `w[k] = step(w[2k], w[2k + 1])` for every pair, an odd last lane
    /// carried to `w[half]`, and `n` shrunk to the next round's count.
    fn simd_pair_round(
        &mut self,
        work: LaneWork<'_>,
        name: &str,
        result_ty: Ty,
        span: Span,
    ) -> HirExpr {
        let LaneWork {
            array: w_name,
            array_ty: w_ty,
            count: n_name,
            elem,
        } = work;
        let i64_ty = self.i64_ty();
        let at = |this: &mut Self, index: HirExpr| this.simd_index(w_name, w_ty, index, elem, span);
        let half_name = self.simd_temp("half");
        let n = self.hir_path(n_name, i64_ty, span);
        let two = self.hir_int(2, i64_ty, span);
        let half = self.hir_binary(HirBinaryOp::Div, n, two, i64_ty, span);
        let bind_half = self.hir_let(&half_name, false, i64_ty, half, span);
        let k_name = self.simd_temp("k");
        let start = self.hir_int(0, i64_ty, span);
        let bind_k = self.hir_let(&k_name, true, i64_ty, start, span);
        let k = self.hir_path(&k_name, i64_ty, span);
        let dst = at(self, k);
        let k = self.hir_path(&k_name, i64_ty, span);
        let two = self.hir_int(2, i64_ty, span);
        let left_index = self.hir_binary(HirBinaryOp::Mul, k, two, i64_ty, span);
        let left = at(self, left_index.clone());
        let one = self.hir_int(1, i64_ty, span);
        let right_index = self.hir_binary(HirBinaryOp::Add, left_index, one, i64_ty, span);
        let right = at(self, right_index);
        let combined = self.simd_reduce_step(name, elem, result_ty, left, right, span);
        let store = self.hir_assign(dst, combined, span);
        let bump = self.hir_increment(&k_name, span);
        let half_bound = self.hir_path(&half_name, i64_ty, span);
        let pair_loop = self.hir_while_lt(&k_name, half_bound, vec![store, bump], span);
        // An odd last lane moves to w[half]; for an even count that slot is
        // past the next round's lanes, so the store is inert.
        let half_index = self.hir_path(&half_name, i64_ty, span);
        let carry_dst = at(self, half_index);
        let n = self.hir_path(n_name, i64_ty, span);
        let one = self.hir_int(1, i64_ty, span);
        let last_index = self.hir_binary(HirBinaryOp::Sub, n, one, i64_ty, span);
        let carry_src = at(self, last_index);
        let carry = self.hir_assign(carry_dst, carry_src, span);
        let half_now = self.hir_path(&half_name, i64_ty, span);
        let n = self.hir_path(n_name, i64_ty, span);
        let two = self.hir_int(2, i64_ty, span);
        let odd = self.hir_binary(HirBinaryOp::Rem, n, two, i64_ty, span);
        let next_n = self.hir_binary(HirBinaryOp::Add, half_now, odd, i64_ty, span);
        let n_place = self.hir_path(n_name, i64_ty, span);
        let shrink = self.hir_assign(n_place, next_n, span);
        self.hir_unit_block(vec![bind_half, bind_k, pair_loop, carry, shrink], span)
    }

    /// `name = name + 1` as a statement.
    fn hir_increment(&mut self, name: &str, span: Span) -> HirStmt {
        let i64_ty = self.i64_ty();
        let current = self.hir_path(name, i64_ty, span);
        let one = self.hir_int(1, i64_ty, span);
        let bumped = self.hir_binary(HirBinaryOp::Add, current, one, i64_ty, span);
        let place = self.hir_path(name, i64_ty, span);
        self.hir_assign(place, bumped, span)
    }

    /// `while counter < bound { body }` as a statement.
    fn hir_while_lt(
        &mut self,
        counter: &str,
        bound: HirExpr,
        body: Vec<HirStmt>,
        span: Span,
    ) -> HirStmt {
        let i64_ty = self.i64_ty();
        let bool_ty = self.tcx.bool_ty();
        let unit_ty = self.tcx.unit();
        let current = self.hir_path(counter, i64_ty, span);
        let condition = self.hir_binary(HirBinaryOp::Lt, current, bound, bool_ty, span);
        let body = HirExpr {
            id: self.fresh(),
            span,
            ty: unit_ty,
            kind: HirExprKind::Block(HirBlock {
                id: self.fresh(),
                span,
                stmts: body,
                tail: None,
                ty: unit_ty,
                is_comptime: false,
            }),
        };
        self.hir_while(condition, body, span)
    }

    /// `while condition { body }` as a statement.
    fn hir_while(&mut self, condition: HirExpr, body: HirExpr, span: Span) -> HirStmt {
        let unit_ty = self.tcx.unit();
        let while_expr = self.hir_expr(
            unit_ty,
            span,
            HirExprKind::While {
                condition: Box::new(condition),
                body: Box::new(body),
                label: None,
            },
        );
        HirStmt {
            id: self.fresh(),
            span,
            kind: HirStmtKind::Expr {
                expr: while_expr,
                has_semi: true,
            },
        }
    }

    /// Combines `leaves` pairwise in lane order, the same tree on every tier.
    fn balanced_tree(
        &mut self,
        mut leaves: Vec<HirExpr>,
        combine: &mut dyn FnMut(&mut Self, HirExpr, HirExpr, Span) -> HirExpr,
        span: Span,
    ) -> Option<HirExpr> {
        while leaves.len() > 1 {
            let mut next = Vec::with_capacity(leaves.len().div_ceil(2));
            let mut iter = leaves.into_iter();
            while let Some(a) = iter.next() {
                match iter.next() {
                    Some(b) => next.push(combine(self, a, b, span)),
                    None => next.push(a),
                }
            }
            leaves = next;
        }
        leaves.pop()
    }

    /// `min(a, b)` / `max(a, b)` for one lane. A float lane with a NaN on
    /// either side answers NaN, and equal lanes answer `-0.0` for `min` and
    /// `+0.0` for `max` when they are zeros of opposite sign.
    fn lane_min_max(
        &mut self,
        a: HirExpr,
        b: HirExpr,
        elem: Ty,
        float: bool,
        is_min: bool,
        span: Span,
    ) -> HirExpr {
        let bool_ty = self.tcx.bool_ty();
        let (first, second) = if is_min {
            (HirBinaryOp::Lt, HirBinaryOp::Gt)
        } else {
            (HirBinaryOp::Gt, HirBinaryOp::Lt)
        };
        let a_wins = self.hir_binary(first, a.clone(), b.clone(), bool_ty, span);
        let b_wins = self.hir_binary(second, a.clone(), b.clone(), bool_ty, span);
        if !float {
            let pick = self.hir_if(a_wins, a.clone(), b.clone(), elem, span);
            return pick;
        }
        let a_nan = self.hir_binary(HirBinaryOp::Ne, a.clone(), a.clone(), bool_ty, span);
        let b_nan = self.hir_binary(HirBinaryOp::Ne, b.clone(), b.clone(), bool_ty, span);
        let any_nan = self.hir_binary(HirBinaryOp::Or, a_nan, b_nan, bool_ty, span);
        let nan = self.hir_binary(HirBinaryOp::Add, a.clone(), b.clone(), elem, span);
        // Equal lanes: the sign of a zero decides. `1.0 / x` is negative
        // exactly for `-0.0` among the zeros.
        let one = self.hir_float("1.0", elem, span);
        let recip = self.hir_binary(HirBinaryOp::Div, one, a.clone(), elem, span);
        let zero = self.hir_float("0.0", elem, span);
        let a_negative = self.hir_binary(HirBinaryOp::Lt, recip, zero, bool_ty, span);
        let tie = if is_min {
            self.hir_if(a_negative, a.clone(), b.clone(), elem, span)
        } else {
            self.hir_if(a_negative, b.clone(), a.clone(), elem, span)
        };
        let by_b = self.hir_if(b_wins, b, tie, elem, span);
        let by_a = self.hir_if(a_wins, a, by_b, elem, span);
        self.hir_if(any_nan, nan, by_a, elem, span)
    }

    /// `abs` for one lane: floats clear the sign of `-0.0` too, integers wrap.
    fn lane_abs(&mut self, a: HirExpr, elem: Ty, float: bool, span: Span) -> HirExpr {
        let bool_ty = self.tcx.bool_ty();
        if float {
            let one = self.hir_float("1.0", elem, span);
            let recip = self.hir_binary(HirBinaryOp::Div, one, a.clone(), elem, span);
            let zero = self.hir_float("0.0", elem, span);
            let negative = self.hir_binary(HirBinaryOp::Lt, recip, zero, bool_ty, span);
            let negated = self.hir_expr(
                elem,
                span,
                HirExprKind::Unary {
                    op: HirUnaryOp::Neg,
                    operand: Box::new(a.clone()),
                },
            );
            return self.hir_if(negative, negated, a, elem, span);
        }
        let zero = self.hir_int(0, elem, span);
        let negative = self.hir_binary(HirBinaryOp::Lt, a.clone(), zero, bool_ty, span);
        let zero = self.hir_int(0, elem, span);
        let negated = self.hir_expr(
            elem,
            span,
            HirExprKind::MethodCall {
                receiver: Box::new(zero),
                name: Ident::new("__gos_wrapping_sub"),
                args: vec![a.clone()],
                owner: None,
            },
        );
        self.hir_if(negative, negated, a, elem, span)
    }

    /// `{ let k0 = op0; ..; let mut out = [zero; N]; let mut i = 0;
    /// while i < N { out[i] = lane(k0[i], ..); i = i + 1 } out }`.
    fn simd_lane_loop(
        &mut self,
        shape: SimdShape,
        operands: Vec<HirExpr>,
        lane_ty: Ty,
        span: Span,
        mut lane: impl FnMut(&mut Self, &[HirExpr], Span) -> HirExpr,
    ) -> HirExprKind {
        let i64_ty = self.i64_ty();
        let out_ty = self.tcx.intern(TyKind::Array {
            elem: lane_ty,
            len: shape.lanes,
        });
        let mut stmts = Vec::new();
        let mut names = Vec::new();
        for operand in operands {
            let name = self.simd_temp("k");
            let ty = operand.ty;
            stmts.push(self.hir_let(&name, false, ty, operand, span));
            names.push((name, ty));
        }
        let first = names
            .first()
            .map(|(name, ty)| self.hir_path(name, *ty, span));
        let count = match self.lane_count_expr(shape, first, span) {
            Some(count) => count,
            None => self.hir_int(0, i64_ty, span),
        };
        let zero = self.zero_of(lane_ty, span);
        let out = self.simd_temp("out");
        let init = self.hir_expr(
            out_ty,
            span,
            HirExprKind::Array(HirArrayExpr::Repeat {
                value: Box::new(zero),
                count: Box::new(count.clone()),
            }),
        );
        stmts.push(self.hir_let(&out, true, out_ty, init, span));
        let counter = self.simd_temp("i");
        let start = self.hir_int(0, i64_ty, span);
        stmts.push(self.hir_let(&counter, true, i64_ty, start, span));

        let lanes: Vec<HirExpr> = names
            .iter()
            .map(|(name, ty)| {
                let base = self.hir_path(name, *ty, span);
                let index = self.hir_path(&counter, i64_ty, span);
                // An operand is the vector or the array it is laid out as;
                // either way its lanes are its own element type, whatever
                // the loop answers.
                let elem_ty = match self.tcx.kind(*ty) {
                    Some(TyKind::Array { elem, .. } | TyKind::Simd { elem, .. }) => *elem,
                    _ => lane_ty,
                };
                self.hir_expr(
                    elem_ty,
                    span,
                    HirExprKind::Index {
                        base: Box::new(base),
                        index: Box::new(index),
                    },
                )
            })
            .collect();
        let value = lane(self, &lanes, span);
        let out_base = self.hir_path(&out, out_ty, span);
        let out_index = self.hir_path(&counter, i64_ty, span);
        let place = self.hir_expr(
            lane_ty,
            span,
            HirExprKind::Index {
                base: Box::new(out_base),
                index: Box::new(out_index),
            },
        );
        let store = self.hir_assign(place, value, span);
        let bump = self.hir_increment(&counter, span);
        stmts.push(self.hir_while_lt(&counter, count, vec![store, bump], span));
        let tail = self.hir_path(&out, out_ty, span);
        HirExprKind::Block(HirBlock {
            id: self.fresh(),
            span,
            stmts,
            tail: Some(Box::new(tail)),
            ty: out_ty,
            is_comptime: false,
        })
    }

    /// The lane count as an `i64` expression: the literal for a concrete
    /// count, the length of `witness` for a const generic one.
    fn lane_count_expr(
        &mut self,
        shape: SimdShape,
        witness: Option<HirExpr>,
        span: Span,
    ) -> Option<HirExpr> {
        let i64_ty = self.i64_ty();
        match shape.lanes {
            ArrayLen::Concrete(n) => {
                Some(self.hir_int(i64::try_from(n).unwrap_or(0), i64_ty, span))
            }
            ArrayLen::Param(idx) => {
                if let Some(witness) = witness {
                    return Some(self.hir_expr(
                        i64_ty,
                        span,
                        HirExprKind::MethodCall {
                            receiver: Box::new(witness),
                            name: Ident::new("len"),
                            args: Vec::new(),
                            owner: None,
                        },
                    ));
                }
                // With no vector to measure, the count is the const parameter
                // itself, which the body holds as a value of its own name.
                let position = usize::try_from(idx.as_u32()).ok()?;
                let name = self
                    .current_generic_names
                    .get(position)
                    .filter(|name| !name.is_empty())?
                    .clone();
                let usize_ty = self.tcx.int_ty(IntTy::Usize);
                let param = self.hir_path(&name, usize_ty, span);
                Some(self.hir_expr(
                    i64_ty,
                    span,
                    HirExprKind::Cast {
                        value: Box::new(param),
                        ty: i64_ty,
                    },
                ))
            }
        }
    }

    fn simd_temp(&mut self, tag: &str) -> String {
        let id = self.fresh();
        format!("__simd_{tag}_{}", id.0)
    }

    fn is_float(&self, ty: Ty) -> bool {
        matches!(self.tcx.kind(ty), Some(TyKind::Float(_)))
    }

    fn int_bits(&self, ty: Ty) -> u32 {
        match self.tcx.kind(ty) {
            Some(TyKind::Int(IntTy::I8 | IntTy::U8)) => 8,
            Some(TyKind::Int(IntTy::I16 | IntTy::U16)) => 16,
            Some(TyKind::Int(IntTy::I32 | IntTy::U32)) => 32,
            _ => 64,
        }
    }

    fn i64_ty(&mut self) -> Ty {
        self.tcx.int_ty(IntTy::I64)
    }

    fn zero_of(&mut self, ty: Ty, span: Span) -> HirExpr {
        match self.tcx.kind(ty) {
            Some(TyKind::Float(_)) => self.hir_float("0.0", ty, span),
            Some(TyKind::Bool) => {
                self.hir_expr(ty, span, HirExprKind::Literal(HirLiteral::Bool(false)))
            }
            _ => self.hir_int(0, ty, span),
        }
    }

    fn hir_expr(&mut self, ty: Ty, span: Span, kind: HirExprKind) -> HirExpr {
        HirExpr {
            id: self.fresh(),
            span,
            ty,
            kind,
        }
    }

    fn hir_int(&mut self, value: i64, ty: Ty, span: Span) -> HirExpr {
        self.hir_expr(
            ty,
            span,
            HirExprKind::Literal(HirLiteral::Int(value.to_string())),
        )
    }

    fn hir_float(&mut self, text: &str, ty: Ty, span: Span) -> HirExpr {
        self.hir_expr(
            ty,
            span,
            HirExprKind::Literal(HirLiteral::Float(text.to_string())),
        )
    }

    fn hir_path(&mut self, name: &str, ty: Ty, span: Span) -> HirExpr {
        self.hir_expr(
            ty,
            span,
            HirExprKind::Path {
                segments: vec![Ident::new(name)],
                def: None,
            },
        )
    }

    fn hir_binary(
        &mut self,
        op: HirBinaryOp,
        lhs: HirExpr,
        rhs: HirExpr,
        ty: Ty,
        span: Span,
    ) -> HirExpr {
        self.hir_expr(
            ty,
            span,
            HirExprKind::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            },
        )
    }

    fn hir_if(
        &mut self,
        condition: HirExpr,
        then: HirExpr,
        otherwise: HirExpr,
        ty: Ty,
        span: Span,
    ) -> HirExpr {
        self.hir_expr(
            ty,
            span,
            HirExprKind::If {
                condition: Box::new(condition),
                then_branch: Box::new(then),
                else_branch: Some(Box::new(otherwise)),
            },
        )
    }

    fn hir_assign(&mut self, place: HirExpr, value: HirExpr, span: Span) -> HirStmt {
        let unit_ty = self.tcx.unit();
        let assign = self.hir_expr(
            unit_ty,
            span,
            HirExprKind::Assign {
                place: Box::new(place),
                value: Box::new(value),
            },
        );
        HirStmt {
            id: self.fresh(),
            span,
            kind: HirStmtKind::Expr {
                expr: assign,
                has_semi: true,
            },
        }
    }

    fn hir_let(&mut self, name: &str, mutable: bool, ty: Ty, init: HirExpr, span: Span) -> HirStmt {
        let pattern = HirPat {
            id: self.fresh(),
            span,
            ty,
            kind: HirPatKind::Binding {
                name: Ident::new(name),
                mutable,
            },
        };
        HirStmt {
            id: self.fresh(),
            span,
            kind: HirStmtKind::Let {
                pattern,
                ty,
                init: Some(init),
            },
        }
    }
}

/// The indices of a swizzle's literal index list, which the checker has
/// validated.
fn swizzle_literals(arg: &AstExpr) -> Option<Vec<usize>> {
    let (AstExprKind::FixedArray(gossamer_ast::ArrayExpr::List(items))
    | AstExprKind::Array(gossamer_ast::ArrayExpr::List(items))) = &arg.kind
    else {
        return None;
    };
    items
        .iter()
        .map(|item| match &item.kind {
            AstExprKind::Literal(gossamer_ast::Literal::Int(text)) => text
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .ok(),
            _ => None,
        })
        .collect()
}

/// A binding or a field chain over one: an expression that names storage and
/// computes nothing, so lowering it again reads the same place.
fn is_pure_place(expr: &AstExpr) -> bool {
    match &expr.kind {
        AstExprKind::Path(_) => true,
        AstExprKind::FieldAccess { receiver, .. } => is_pure_place(receiver),
        _ => false,
    }
}

/// Where a mask terminal reads its lanes: a lane comparison whose operands
/// are bound once, or a mask value.
enum MaskLanes {
    Compare {
        op: HirBinaryOp,
        lhs: (String, Ty),
        rhs: (String, Ty),
        elem: Ty,
    },
    Mask(String, Ty),
}
