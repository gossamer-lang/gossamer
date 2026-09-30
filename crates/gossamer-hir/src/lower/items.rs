//! Lowering items: functions, structs, enums, impls, and traits.

use gossamer_ast::{
    EnumDecl, FnDecl as AstFnDecl, FnParam as AstFnParam, Ident, ImplDecl, ImplItem,
    Item as AstItem, ItemKind as AstItemKind, Mutability, Pattern as AstPat, StructDecl, TraitDecl,
    TraitItem,
};
use gossamer_lex::Span;
use gossamer_resolve::Resolution;

use crate::tree::{
    FnOrigin, HirAdt, HirAdtKind, HirBlock, HirBody, HirConst, HirExpr, HirExprKind, HirFn,
    HirImpl, HirItem, HirItemKind, HirParam, HirPat, HirPatKind, HirStatic, HirStmt, HirStmtKind,
    HirTrait,
};

use super::{Lowerer, qualified_item_name};

impl Lowerer<'_> {
    pub(super) fn lower_item(&mut self, item: &AstItem, module_path: &[String]) -> Option<HirItem> {
        self.current_module = module_path.to_vec();
        let def = self.resolutions.definition_of(item.id);
        let kind = match &item.kind {
            AstItemKind::Fn(decl) => HirItemKind::Fn(self.lower_fn(decl, item.span)),
            AstItemKind::Const(decl) => HirItemKind::Const(HirConst {
                name: decl.name.clone(),
                ty: self.ty_of(decl.value.id),
                value: self.lower_expr(&decl.value),
            }),
            AstItemKind::Static(decl) => HirItemKind::Static(HirStatic {
                name: decl.name.clone(),
                // The declared type is what the cell holds; an initializer
                // that names no element type (`Map::new()`, `#[]`) would
                // otherwise leave the static's own type an inference var.
                ty: self.declared_or_init_ty(decl.ty.id, decl.value.id),
                mutable: matches!(decl.mutability, Mutability::Mutable),
                value: self.lower_expr(&decl.value),
            }),
            // An ADT declared inside a module carries its qualified name as
            // its identity, matching what the type checker registers. Every
            // name-keyed table below here - the MIR struct/variant tables,
            // `{:?}` dispatch, the native constructor registry - reads that
            // name, so two modules may declare the same one.
            AstItemKind::Struct(decl) => {
                let mut adt = self.lower_struct(decl);
                adt.name = Ident::new(qualified_item_name(module_path, &adt.name.name));
                HirItemKind::Adt(adt)
            }
            AstItemKind::Enum(decl) => {
                let mut adt = self.lower_enum(decl);
                adt.name = Ident::new(qualified_item_name(module_path, &adt.name.name));
                HirItemKind::Adt(adt)
            }
            AstItemKind::Impl(decl) => HirItemKind::Impl(self.lower_impl(decl, item.span)),
            AstItemKind::Trait(decl) => HirItemKind::Trait(self.lower_trait(decl, item.span)),
            AstItemKind::TypeAlias(_) | AstItemKind::Mod(_) | AstItemKind::AttrItem(_) => {
                return None;
            }
        };
        Some(HirItem {
            id: self.fresh(),
            span: item.span,
            def,
            module_path: module_path.to_vec(),
            kind,
        })
    }

    fn lower_fn(&mut self, decl: &AstFnDecl, span: Span) -> HirFn {
        self.lower_fn_with_self(decl, span, None, None)
    }

    /// Lowers an impl-method body with the impl's `Self` type
    /// applied to the `self` receiver. Lets MIR field-access
    /// lowering find the struct name on `self.field` reads
    /// without falling through to the unsupported placeholder.
    fn lower_fn_with_self(
        &mut self,
        decl: &AstFnDecl,
        span: Span,
        self_ty: Option<gossamer_types::Ty>,
        impl_generics: Option<&gossamer_ast::Generics>,
    ) -> HirFn {
        let mut params = Vec::new();
        let mut has_self = false;
        // Destructuring `let`s injected at body entry for non-trivial param
        // patterns (`(a, b)`, `Pt(a, b)`, `P { x, y }`): MIR binds only one
        // name per parameter, so the param takes a fresh binding and the
        // pattern is bound by a `let` reusing the let-destructuring path.
        let mut param_destructures: Vec<HirStmt> = Vec::new();
        for param in &decl.params {
            match param {
                AstFnParam::Receiver(kind) => {
                    has_self = true;
                    let id = self.fresh();
                    let base = self_ty.unwrap_or_else(|| self.error_ty());
                    // For `&self` / `&mut self`, type `self` as a
                    // Ref so the codegen lowers field access
                    // (`self.x`) and field assignment (`self.x =
                    // y`) through the pointer - matching how
                    // free-function `&mut Type` parameters already
                    // work. Owned `self` keeps the value type.
                    let ty = match kind {
                        gossamer_ast::Receiver::Owned => base,
                        gossamer_ast::Receiver::RefShared => {
                            self.tcx.intern(gossamer_types::TyKind::Ref {
                                mutability: gossamer_types::Mutbl::Not,
                                inner: base,
                            })
                        }
                        gossamer_ast::Receiver::RefMut => {
                            self.tcx.intern(gossamer_types::TyKind::Ref {
                                mutability: gossamer_types::Mutbl::Mut,
                                inner: base,
                            })
                        }
                    };
                    params.push(HirParam {
                        pattern: HirPat {
                            id,
                            span,
                            ty,
                            kind: HirPatKind::Binding {
                                name: Ident::new("self"),
                                mutable: matches!(kind, gossamer_ast::Receiver::RefMut),
                            },
                        },
                        ty,
                        is_comptime: false,
                    });
                }
                AstFnParam::Typed {
                    pattern,
                    ty: ast_ty,
                    is_comptime,
                    ..
                } => {
                    let ty = self.ty_of(ast_ty.id);
                    let p = self.lower_typed_param(
                        pattern,
                        ty,
                        *is_comptime,
                        params.len(),
                        span,
                        &mut param_destructures,
                    );
                    params.push(p);
                }
            }
        }
        params.extend(self.const_generic_params(impl_generics, &decl.generics, span));
        let generic_names = impl_generics
            .into_iter()
            .flat_map(|generics| generics.params.iter())
            .chain(decl.generics.params.iter())
            .map(|param| match param {
                gossamer_ast::GenericParam::Type { name, .. }
                | gossamer_ast::GenericParam::Const { name, .. } => name.name.clone(),
                gossamer_ast::GenericParam::Lifetime { .. } => String::new(),
            })
            .collect();
        let saved_generic_names = std::mem::replace(&mut self.current_generic_names, generic_names);
        let ret = decl.ret.as_ref().map(|ty| self.ty_of(ty.id));
        let saved_ret = self
            .current_fn_ret_ty
            .replace(ret.unwrap_or_else(|| self.tcx.unit()));
        let body = decl.body.as_ref().map(|body| {
            let mut block = self.lower_expr_as_block(body);
            if !param_destructures.is_empty() {
                let mut stmts = std::mem::take(&mut param_destructures);
                stmts.append(&mut block.stmts);
                block.stmts = stmts;
            }
            self.discard_undeclared_tail(&decl.name.name, ret, &mut block);
            HirBody { block }
        });
        self.current_fn_ret_ty = saved_ret;
        self.current_generic_names = saved_generic_names;
        HirFn {
            name: decl.name.clone(),
            params,
            ret,
            body,
            is_unsafe: decl.is_unsafe,
            is_comptime: decl.is_comptime,
            has_self,
            origin: FnOrigin::Declared,
        }
    }

    /// The trailing parameters a function's const generic parameters arrive
    /// as. A const generic parameter is a value the body reads, so each is a
    /// parameter of its own name, in declaration order (the impl's first),
    /// which is the order a call hands the values over in.
    fn const_generic_params(
        &mut self,
        impl_generics: Option<&gossamer_ast::Generics>,
        generics: &gossamer_ast::Generics,
        span: Span,
    ) -> Vec<HirParam> {
        let mut params = Vec::new();
        let const_params = impl_generics
            .into_iter()
            .flat_map(|generics| generics.params.iter())
            .chain(generics.params.iter());
        for param in const_params {
            if let gossamer_ast::GenericParam::Const { name, ty, .. } = param {
                let ty = self.ty_of(ty.id);
                params.push(HirParam {
                    pattern: HirPat {
                        id: self.fresh(),
                        span,
                        ty,
                        kind: HirPatKind::Binding {
                            name: name.clone(),
                            mutable: false,
                        },
                    },
                    ty,
                    is_comptime: false,
                });
            }
        }
        params
    }

    /// Demotes a value-producing tail to a statement when the signature
    /// answers a unit - written `-> ()` or left off. The value is computed
    /// for its effects and dropped, so every tier agrees with the signature
    /// the caller reads; the checker reports the undeclared spelling as a
    /// lint.
    ///
    /// A wrapper the front end synthesized around an expression - the REPL's
    /// per-input entry point, the binding-type probe - answers that
    /// expression by construction, and its caller reads the value back
    /// rather than the signature. A tail that already answers a unit has no
    /// value to discard and keeps its place.
    fn discard_undeclared_tail(
        &mut self,
        name: &str,
        ret: Option<gossamer_types::Ty>,
        block: &mut HirBlock,
    ) {
        if name.starts_with("__") {
            return;
        }
        let answers_unit =
            ret.is_none_or(|ty| matches!(self.tcx.kind(ty), Some(gossamer_types::TyKind::Unit)));
        let tail_holds_value = block.tail.as_ref().is_some_and(|tail| {
            !matches!(self.tcx.kind(tail.ty), Some(gossamer_types::TyKind::Unit))
        });
        if !answers_unit || !tail_holds_value {
            return;
        }
        let Some(tail) = block.tail.take() else {
            return;
        };
        block.ty = self.unit();
        block.stmts.push(HirStmt {
            id: self.fresh(),
            span: tail.span,
            kind: HirStmtKind::Expr {
                expr: *tail,
                has_semi: true,
            },
        });
    }

    /// Lowers one typed parameter. A non-trivial pattern (`(a, b)`,
    /// `Pt(a, b)`, `P { x, y }`) is bound to a fresh `__paramN` local and
    /// destructured by a `let` appended to `destructures` for injection at
    /// body entry, since MIR binds only a single name per parameter.
    fn lower_typed_param(
        &mut self,
        pattern: &AstPat,
        ty: gossamer_types::Ty,
        is_comptime: bool,
        index: usize,
        span: Span,
        destructures: &mut Vec<HirStmt>,
    ) -> HirParam {
        let lowered = self.lower_pat_with_ty(pattern, ty);
        let pattern = if matches!(
            lowered.kind,
            HirPatKind::Binding { .. } | HirPatKind::Wildcard
        ) {
            lowered
        } else {
            let name = format!("__param{index}");
            destructures.push(HirStmt {
                id: self.fresh(),
                span,
                kind: HirStmtKind::Let {
                    pattern: lowered,
                    ty,
                    init: Some(HirExpr {
                        id: self.fresh(),
                        span,
                        ty,
                        kind: HirExprKind::Path {
                            segments: vec![Ident::new(name.clone())],
                            def: None,
                        },
                    }),
                },
            });
            HirPat {
                id: self.fresh(),
                span,
                ty,
                kind: HirPatKind::Binding {
                    name: Ident::new(name),
                    mutable: false,
                },
            }
        };
        HirParam {
            pattern,
            ty,
            is_comptime,
        }
    }

    fn lower_struct(&mut self, decl: &StructDecl) -> HirAdt {
        let ty = self.error_ty();
        let fields = match &decl.body {
            gossamer_ast::StructBody::Named(named) => {
                named.iter().map(|f| f.name.clone()).collect()
            }
            // Tuple-struct fields are modelled as positional names "0".."N-1"
            // so construction and `.N` access reuse the named-field path.
            gossamer_ast::StructBody::Tuple(tup) => (0..tup.len())
                .map(|i| gossamer_ast::Ident::new(i.to_string()))
                .collect(),
            gossamer_ast::StructBody::Unit => Vec::new(),
        };
        HirAdt {
            name: decl.name.clone(),
            kind: HirAdtKind::Struct(fields),
            self_ty: ty,
            repr: gossamer_ast::EnumRepr::default(),
        }
    }

    fn lower_enum(&mut self, decl: &EnumDecl) -> HirAdt {
        let variants = decl
            .variants
            .iter()
            .map(|variant| {
                let (struct_fields, struct_field_tys) = match &variant.body {
                    gossamer_ast::StructBody::Named(fields) => {
                        let names: Vec<_> = fields.iter().map(|f| f.name.clone()).collect();
                        let tys: Vec<_> = fields.iter().map(|f| self.ty_of(f.ty.id)).collect();
                        (Some(names), Some(tys))
                    }
                    gossamer_ast::StructBody::Tuple(fields) => {
                        // Store positional field types so MIR lowering can assign
                        // the correct type (e.g. f64) to tuple-variant bindings
                        // instead of always using i64.
                        let tys: Vec<_> = fields.iter().map(|f| self.ty_of(f.ty.id)).collect();
                        (None, Some(tys))
                    }
                    gossamer_ast::StructBody::Unit => (None, None),
                };
                crate::tree::HirEnumVariant {
                    name: variant.name.clone(),
                    struct_fields,
                    struct_field_tys,
                }
            })
            .collect();
        let ty = self.error_ty();
        HirAdt {
            name: decl.name.clone(),
            kind: HirAdtKind::Enum(variants),
            self_ty: ty,
            repr: decl.repr,
        }
    }

    fn lower_impl(&mut self, decl: &ImplDecl, span: Span) -> HirImpl {
        let self_ty = self.ty_of(decl.self_ty.id);
        // An impl block's self type identifies the type it extends, so it
        // carries the same qualified identity the declaration registered -
        // otherwise two modules' `impl Point` would emit one symbol each
        // under the same name.
        let self_name = match &decl.self_ty.kind {
            gossamer_ast::TypeKind::Path(path) => self
                .resolutions
                .get(decl.self_ty.id)
                .and_then(|resolution| match resolution {
                    Resolution::Def { def, .. } => self.module_type_names.get(&def).cloned(),
                    _ => None,
                })
                .map(Ident::new)
                .or_else(|| {
                    // The written path is already the qualified spelling for
                    // an `impl a::Point`; keep every segment so the methods
                    // register under the type's identity.
                    let segments: Vec<&str> = path
                        .segments
                        .iter()
                        .map(|seg| seg.name.name.as_str())
                        .filter(|seg| !matches!(*seg, "crate" | "self" | "super" | "root"))
                        .collect();
                    (!segments.is_empty()).then(|| Ident::new(segments.join("::")))
                }),
            // A tuple has no path to name it by, so it registers under the
            // spelling every layer shares for one.
            _ => gossamer_types::printer::structural_impl_owner(self.tcx, self_ty).map(Ident::new),
        };
        let trait_name = decl
            .trait_ref
            .as_ref()
            .and_then(|bound| bound.path.segments.last())
            .map(|seg| seg.name.clone());
        let methods = decl
            .items
            .iter()
            .filter_map(|item| match item {
                ImplItem::Fn(fn_decl) => Some(self.lower_fn_with_self(
                    fn_decl,
                    span,
                    Some(self_ty),
                    Some(&decl.generics),
                )),
                // Associated types are already resolved to concrete types
                // in the type table, and every associated constant is a
                // top-level constant by the time lowering runs, so neither
                // needs a body-level HIR node.
                ImplItem::Const { .. } | ImplItem::Type { .. } => None,
            })
            .collect();
        HirImpl {
            self_ty,
            self_name,
            trait_name,
            methods,
        }
    }

    fn lower_trait(&mut self, decl: &TraitDecl, span: Span) -> HirTrait {
        let methods = decl
            .items
            .iter()
            .filter_map(|item| match item {
                TraitItem::Fn(fn_decl) => Some(self.lower_fn(fn_decl, span)),
                // Associated declarations carry no executable code: a
                // projection resolves during type checking and a constant
                // is hoisted to a top-level `const` before lowering.
                TraitItem::Type { .. } | TraitItem::Const { .. } => None,
            })
            .collect();
        HirTrait {
            name: decl.name.clone(),
            methods,
        }
    }
}
