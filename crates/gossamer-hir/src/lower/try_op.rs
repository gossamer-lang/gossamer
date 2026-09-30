//! Lowering the `?` operator: payload types, propagation, and error conversion.

use gossamer_ast::{Expr as AstExpr, ExprKind as AstExprKind, Ident};
use gossamer_lex::Span;

use crate::tree::{HirExpr, HirExprKind, HirMatchArm, HirPat, HirPatKind};

use super::{Lowerer, TryKind};

impl Lowerer<'_> {
    /// Returns the `T` payload type when `ty` is a `Result<T, E>`
    /// (or a `&Result<T, E>`), `None` otherwise. Used by `lower_try`
    /// so a `?`-unwrapped binding inherits a real type instead of
    /// the `Error` sentinel.
    fn try_ok_payload_ty(&self, ty: gossamer_types::Ty) -> Option<gossamer_types::Ty> {
        use gossamer_types::TyKind;
        let mut peeled = ty;
        loop {
            match self.tcx.kind(peeled)? {
                TyKind::Ref { inner, .. } => peeled = *inner,
                TyKind::Adt { substs, .. } => {
                    let args = substs.as_slice();
                    if args.is_empty() {
                        return None;
                    }
                    if let Some(gossamer_types::GenericArg::Type(t)) = args.first() {
                        return Some(*t);
                    }
                    return None;
                }
                _ => return None,
            }
        }
    }

    /// Heuristic fallback for `?` operator's `__try_value` type
    /// when the inner expression's HIR type is unresolved. Walks
    /// chained method calls - `fs::read_to_string(...)
    /// .map_err(...)` is a common shape - and returns `String`
    /// for stdlib helpers whose runtime return is a c-string. The
    /// MIR-side `pinned_ret` table is the authoritative source of
    /// truth; this list mirrors its String entries so the HIR layer
    /// can ground a `let s = ...?` binding even when the
    /// typechecker leaks a Var through `?`.
    fn try_ok_payload_ty_heuristic(&mut self, inner: &AstExpr) -> Option<gossamer_types::Ty> {
        let mut cur = inner;
        loop {
            match &cur.kind {
                AstExprKind::MethodCall { receiver, name, .. }
                    if matches!(name.name.as_str(), "map_err" | "map" | "ok" | "err") =>
                {
                    cur = receiver;
                }
                AstExprKind::Call { callee, .. } => {
                    if let AstExprKind::Path(path) = &callee.kind {
                        let joined: Vec<&str> =
                            path.segments.iter().map(|s| s.name.name.as_str()).collect();
                        let last = *joined.last()?;
                        // Match the same names the parse-side
                        // resolves to gos_rt_*_-returning helpers
                        // whose c-string return is logically a
                        // String. If the MIR pin gets it right we
                        // never reach here; this is the last-ditch
                        // path for when the typechecker hasn't
                        // resolved through `?`.
                        if matches!(
                            last,
                            "read_to_string"
                                | "read_line"
                                | "trim"
                                | "to_lowercase"
                                | "to_uppercase"
                                | "replace"
                                | "format"
                                | "join"
                        ) {
                            return Some(self.tcx.string_ty());
                        }
                    }
                    return None;
                }
                _ => return None,
            }
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "`?` desugar covers both Option and Result branches with bespoke HirExpr construction; splitting per-branch helpers would hide the structural symmetry between them"
    )]
    pub(super) fn lower_try(&mut self, inner: &AstExpr, span: Span) -> HirExprKind {
        let value = self.lower_expr(inner);
        let value_ty = value.ty;
        // Detect Option<T> vs Result<T, E> so `?` desugars to the
        // matching unwrap-or-return-propagate shape. Result is the
        // existing path; Option propagates `None` from the enclosing
        // function via `return None`.
        let kind = self.try_propagation_kind(value_ty, inner);
        let payload_ty = self
            .try_payload_ty(value_ty)
            .or_else(|| self.try_ok_payload_ty_heuristic(inner));
        let try_value_ty = payload_ty.unwrap_or_else(|| self.error_ty());
        let ok_binding_id = self.fresh();
        let ok_variant = match kind {
            TryKind::Option => "Some",
            TryKind::Result => "Ok",
        };
        let ok_pat = HirPat {
            id: self.fresh(),
            span,
            ty: value_ty,
            kind: HirPatKind::Variant {
                name: Ident::new(ok_variant),
                fields: vec![HirPat {
                    id: ok_binding_id,
                    span,
                    ty: try_value_ty,
                    kind: HirPatKind::Binding {
                        name: Ident::new("__try_value"),
                        mutable: false,
                    },
                }],
            },
        };
        let ok_body = HirExpr {
            id: self.fresh(),
            span,
            ty: try_value_ty,
            kind: HirExprKind::Path {
                segments: vec![Ident::new("__try_value")],
                def: None,
            },
        };
        // Build the early-return body. For Option this is `return
        // None`; for Result it's `return Err(__try_err)` (preserving
        // the existing semantics - From-based error conversion would
        // require typeck context not available in HIR lowering).
        let (err_pat, err_body) = match kind {
            TryKind::Option => {
                let none_pat = HirPat {
                    id: self.fresh(),
                    span,
                    ty: value_ty,
                    kind: HirPatKind::Variant {
                        name: Ident::new("None"),
                        fields: Vec::new(),
                    },
                };
                let none_value = HirExpr {
                    id: self.fresh(),
                    span,
                    ty: value_ty,
                    kind: HirExprKind::Path {
                        segments: vec![Ident::new("None")],
                        def: None,
                    },
                };
                let body = HirExpr {
                    id: self.fresh(),
                    span,
                    ty: self.tcx.never(),
                    kind: HirExprKind::Return(Some(Box::new(none_value))),
                };
                (none_pat, body)
            }
            TryKind::Result => {
                // The actual error type `E` of the inner `Result<T,E>` - may
                // be `String`, a user type, or `errors::Error`. Typing the
                // bound error as the concrete `E` (not always `errors::Error`)
                // is what lets a `String`-typed error survive `?` propagation
                // intact rather than being mis-rendered as an error handle.
                let err_ty = self
                    .try_err_payload_ty(value_ty)
                    .unwrap_or_else(|| self.error_ty());
                let err_binding_id = self.fresh();
                let err_pat = HirPat {
                    id: self.fresh(),
                    span,
                    ty: value_ty,
                    kind: HirPatKind::Variant {
                        name: Ident::new("Err"),
                        fields: vec![HirPat {
                            id: err_binding_id,
                            span,
                            ty: err_ty,
                            kind: HirPatKind::Binding {
                                name: Ident::new("__try_err"),
                                mutable: false,
                            },
                        }],
                    },
                };
                let err_value = HirExpr {
                    id: self.fresh(),
                    span,
                    ty: err_ty,
                    kind: HirExprKind::Path {
                        segments: vec![Ident::new("__try_err")],
                        def: None,
                    },
                };
                // SPEC §4.5: `?` propagates with `E: Into<E2>`
                // conversion when the inner error type differs
                // from the enclosing function's error type. We
                // detect the mismatch by comparing the inner
                // value's `Result<_, Inner>` against the outer
                // fn's `Result<_, Outer>` and route the err
                // payload through `Into::into` (the runtime
                // resolves the canonical errors::Error path for
                // String / errors::Error / user types).
                let err_value = self.maybe_convert_try_err(err_value, value_ty, span);
                // `return Err(e)` yields the ENCLOSING FUNCTION's return type
                // (`Result<T,E>`, a 2-word by-value i128), not the bare error
                // type and not the scrutinee's (possibly-unpinned `Var`) type.
                // Pinning it to the concrete fn return type is essential: a
                // `Var` would render as `ptr` and truncate the i128 payload.
                let result_ty = self.current_fn_ret_ty.unwrap_or(value_ty);
                let err_wrap = HirExpr {
                    id: self.fresh(),
                    span,
                    ty: result_ty,
                    kind: HirExprKind::Call {
                        callee: Box::new(HirExpr {
                            id: self.fresh(),
                            span,
                            ty: result_ty,
                            kind: HirExprKind::Path {
                                segments: vec![Ident::new("Err")],
                                def: None,
                            },
                        }),
                        args: vec![err_value],
                    },
                };
                let body = HirExpr {
                    id: self.fresh(),
                    span,
                    ty: self.tcx.never(),
                    kind: HirExprKind::Return(Some(Box::new(err_wrap))),
                };
                (err_pat, body)
            }
        };
        HirExprKind::Match {
            scrutinee: Box::new(value),
            arms: vec![
                HirMatchArm {
                    pattern: ok_pat,
                    guard: None,
                    body: ok_body,
                },
                HirMatchArm {
                    pattern: err_pat,
                    guard: None,
                    body: err_body,
                },
            ],
        }
    }

    /// Decide whether `?` should desugar via `Option::Some/None` or
    /// `Result::Ok/Err`. Defaults to `Result` so behaviour matches
    /// the pre-existing implementation when the type isn't known.
    fn try_propagation_kind(&self, ty: gossamer_types::Ty, inner: &AstExpr) -> TryKind {
        use gossamer_types::TyKind;
        let mut peeled = ty;
        for _ in 0..8 {
            match self.tcx.kind(peeled) {
                Some(TyKind::Ref { inner, .. }) => peeled = *inner,
                Some(TyKind::Adt { def, .. }) => {
                    if let Some(name) = self.tcx.def_name(*def) {
                        return match name {
                            "Option" => TryKind::Option,
                            "Result" => TryKind::Result,
                            _ => TryKind::Result,
                        };
                    }
                    return TryKind::Result;
                }
                _ => break,
            }
        }
        // Syntactic fallback for the case where the typechecker
        // hasn't resolved the inner expression yet - recognise
        // common Option-returning HashMap/Vec lookup shapes so
        // `m.get(&k)?` works even when the inferred type is `Var`.
        if Self::ast_is_option_shaped(inner) {
            TryKind::Option
        } else {
            TryKind::Result
        }
    }

    /// Returns true when `inner` looks like an `Option`-returning
    /// stdlib call by name. Conservative - only the dispatch-table
    /// entries whose runtime return is documented `Option<T>`.
    fn ast_is_option_shaped(inner: &AstExpr) -> bool {
        match &inner.kind {
            AstExprKind::MethodCall { name, .. } => matches!(
                name.name.as_str(),
                "get"
                    | "first"
                    | "last"
                    | "pop"
                    | "find"
                    | "find_opt"
                    | "rfind_opt"
                    | "checked_add"
                    | "checked_sub"
                    | "checked_mul"
                    | "split_once"
                    | "rsplit_once"
                    | "strip_prefix"
                    | "strip_suffix"
                    | "index_of"
            ),
            _ => false,
        }
    }

    /// Returns the payload type for the unwrapped-success branch of
    /// `?`. Works for both `Result<T, E>` and `Option<T>` - both
    /// carry `T` as their first generic argument.
    fn try_payload_ty(&self, ty: gossamer_types::Ty) -> Option<gossamer_types::Ty> {
        self.try_ok_payload_ty(ty)
    }

    /// Returns the Err generic-argument type of a `Result<_, E>`
    /// (or a reference to one), if `ty` resolves to a Result Adt.
    fn try_err_payload_ty(&self, ty: gossamer_types::Ty) -> Option<gossamer_types::Ty> {
        use gossamer_types::TyKind;
        let mut peeled = ty;
        loop {
            match self.tcx.kind(peeled)? {
                TyKind::Ref { inner, .. } => peeled = *inner,
                TyKind::Adt { substs, .. } => {
                    let args = substs.as_slice();
                    if args.len() < 2 {
                        return None;
                    }
                    if let Some(gossamer_types::GenericArg::Type(t)) = args.get(1) {
                        return Some(*t);
                    }
                    return None;
                }
                _ => return None,
            }
        }
    }

    /// If the inner expression's `Result<_, Inner>` error type
    /// differs from the enclosing function's declared `Result<_,
    /// Outer>` error type, route `err_value` through
    /// `errors::Error::from(__try_err)` so SPEC §4.5's
    /// `E: Into<E2>` propagation works. When the types align
    /// already (or when either can't be resolved) returns
    /// `err_value` unchanged so existing single-error-type
    /// programs see no change.
    fn maybe_convert_try_err(
        &mut self,
        err_value: HirExpr,
        value_ty: gossamer_types::Ty,
        span: Span,
    ) -> HirExpr {
        use gossamer_types::TyKind;
        let Some(inner_err) = self.try_err_payload_ty(value_ty) else {
            return err_value;
        };
        let Some(outer_ret) = self.current_fn_ret_ty else {
            return err_value;
        };
        let Some(outer_err) = self.try_err_payload_ty(outer_ret) else {
            return err_value;
        };
        if inner_err == outer_err {
            return err_value;
        }
        // A function answering a type of its own converts through that type's
        // `From` impl, which the checker has matched to the operand's error.
        if let Some(TyKind::Adt { def, .. }) = self.tcx.kind(outer_err).cloned()
            && def.local < u32::MAX - 64
            && let Some(owner) = self.tcx.def_name(def).map(str::to_string)
        {
            return HirExpr {
                id: self.fresh(),
                span,
                ty: outer_err,
                kind: HirExprKind::Call {
                    callee: Box::new(HirExpr {
                        id: self.fresh(),
                        span,
                        ty: self.error_ty(),
                        kind: HirExprKind::Path {
                            segments: vec![Ident::new(&owner), Ident::new("from")],
                            def: None,
                        },
                    }),
                    args: vec![err_value],
                },
            };
        }
        // `errors::Error::from` takes a message or another error; any other
        // error reaches it as the text it displays.
        let err_value = if matches!(
            self.tcx.kind(inner_err),
            Some(TyKind::String | TyKind::DynError)
        ) {
            err_value
        } else {
            let string_ty = self.tcx.string_ty();
            HirExpr {
                id: self.fresh(),
                span,
                ty: string_ty,
                kind: HirExprKind::MethodCall {
                    receiver: Box::new(err_value),
                    name: Ident::new("to_string"),
                    args: Vec::new(),
                    owner: None,
                },
            }
        };
        HirExpr {
            id: self.fresh(),
            span,
            ty: outer_err,
            kind: HirExprKind::Call {
                callee: Box::new(HirExpr {
                    id: self.fresh(),
                    span,
                    ty: self.error_ty(),
                    kind: HirExprKind::Path {
                        segments: vec![
                            Ident::new("errors"),
                            Ident::new("Error"),
                            Ident::new("from"),
                        ],
                        def: None,
                    },
                }),
                args: vec![err_value],
            },
        }
    }
}
