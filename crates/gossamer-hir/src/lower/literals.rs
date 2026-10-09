//! Lowering struct, array, map, and set literals.

use gossamer_ast::{
    ArrayExpr as AstArrayExpr, Expr as AstExpr, ExprKind as AstExprKind, Ident, NodeId,
};
use gossamer_lex::Span;
use gossamer_resolve::Resolution;
use gossamer_types::Ty;

use crate::tree::{
    HirArrayExpr, HirBlock, HirExpr, HirExprKind, HirLiteral, HirPat, HirPatKind, HirStmt,
    HirStmtKind,
};

use super::{BTREE_SET_DEF_LOCAL, HASH_SET_DEF_LOCAL, Lowerer, struct_literal_positional_index};

/// The binding a map literal with aggregate values is built in.
const MAP_LITERAL_BINDING: &str = "__gos_map_literal";

/// Whether `e` is the block a map literal with aggregate values lowers to,
/// whose answer is the map it just built and which no other name holds.
#[must_use]
pub fn is_map_literal_block(e: &HirExpr) -> bool {
    matches!(
        &e.kind,
        HirExprKind::Block(HirBlock { tail: Some(tail), .. })
            if matches!(
                &tail.kind,
                HirExprKind::Path { segments, .. }
                    if matches!(segments.as_slice(), [only] if only.name == MAP_LITERAL_BINDING)
            )
    )
}

impl Lowerer<'_> {
    /// Lowers `Path { field: value, … }` into a call to the synthetic
    /// `__struct` builtin. The resulting argument list interleaves
    /// field-name strings with their lowered value expressions:
    ///
    /// `Shape::Rect { w: 2.0, h: 4.0 }` → `__struct("Rect", "w", 2.0, "h", 4.0)`.
    ///
    /// When the literal carries a functional-update base
    /// (`Outer { n: 99, ..base }`), the lowered call also includes a
    /// trailing `"__base", base_expr` pair. The MIR layer fills any
    /// missing fields by reading `base.field` via projection.
    ///
    /// The VM and codegen layers can recognise `__struct` as the
    /// canonical struct-literal constructor without needing a new HIR
    /// node variant.
    pub(super) fn lower_struct_literal(
        &mut self,
        node: NodeId,
        path: &gossamer_ast::PathExpr,
        fields: &[gossamer_ast::StructExprField],
        base: Option<&gossamer_ast::Expr>,
        span: Span,
    ) -> HirExprKind {
        let mut name = path
            .segments
            .last()
            .map(|seg| seg.name.name.clone())
            .unwrap_or_default();
        if let Some(Resolution::Def { def, .. }) = self.resolutions.get(node)
            && let Some(promoted) = self.module_fn_paths.get(&def)
            && let Some(promoted_name) = promoted.last()
        {
            name.clone_from(&promoted_name.name);
        }
        // A type declared in a module is identified by its qualified name,
        // matching what the type checker registers, so two modules may
        // declare the same name without their constructors, `{:?}` dispatch,
        // or native registry entries colliding.
        if let Some(Resolution::Def { def, .. }) = self.resolutions.get(node)
            && let Some(identity) = self.module_type_names.get(&def)
            // The identity names the TYPE. A literal that names a variant of
            // it - `Value::Attr { .. }` - resolves to the same def, and the
            // constructor is keyed by the variant, so only a literal whose
            // own last segment is the type takes the qualified spelling.
            && identity.rsplit("::").next() == Some(name.as_str())
        {
            name.clone_from(identity);
        }
        // A literal may name its type through an import (`use a::Point`,
        // `use a::Point as P`); the import's target path is that identity.
        if let Some(Resolution::Import { use_id }) = self.resolutions.get(node)
            && let Some(entries) = self.import_targets.get(&use_id)
            && let Some((_, full)) = entries.iter().find(|(bound, _)| *bound == name)
        {
            name = full
                .iter()
                .map(|segment| segment.name.as_str())
                .filter(|segment| !matches!(*segment, "crate" | "self" | "super" | "root"))
                .collect::<Vec<_>>()
                .join("::");
        }
        let error_ty = self.error_ty();
        let string_ty = self.error_ty();
        let mut args = Vec::with_capacity(1 + fields.len() * 2 + 2);
        args.push(HirExpr {
            id: self.fresh(),
            span,
            ty: string_ty,
            kind: HirExprKind::Literal(HirLiteral::String(name)),
        });
        let field_names = self.resolve_struct_literal_field_names(
            path.segments
                .last()
                .map(|seg| seg.name.name.as_str())
                .unwrap_or_default(),
            fields,
        );
        let field_order = self.struct_literal_field_order(
            path.segments
                .last()
                .map(|seg| seg.name.name.as_str())
                .unwrap_or_default(),
            fields,
            &field_names,
        );
        for idx in field_order {
            let field = &fields[idx];
            args.push(HirExpr {
                id: self.fresh(),
                span,
                ty: string_ty,
                kind: HirExprKind::Literal(HirLiteral::String(
                    field_names
                        .get(&idx)
                        .cloned()
                        .unwrap_or_else(|| field.name.name.clone()),
                )),
            });
            let value = match &field.value {
                Some(expr) => self.lower_expr(expr),
                None => HirExpr {
                    id: self.fresh(),
                    span,
                    ty: error_ty,
                    kind: HirExprKind::Path {
                        segments: vec![field.name.clone()],
                        def: None,
                    },
                },
            };
            args.push(value);
        }
        if let Some(base_expr) = base {
            args.push(HirExpr {
                id: self.fresh(),
                span,
                ty: string_ty,
                kind: HirExprKind::Literal(HirLiteral::String("__base".to_string())),
            });
            args.push(self.lower_expr(base_expr));
        }
        HirExprKind::Call {
            callee: Box::new(HirExpr {
                id: self.fresh(),
                span,
                ty: error_ty,
                kind: HirExprKind::Path {
                    segments: vec![Ident::new("__struct")],
                    def: None,
                },
            }),
            args,
        }
    }

    fn struct_literal_field_order(
        &self,
        struct_name: &str,
        fields: &[gossamer_ast::StructExprField],
        resolved_names: &std::collections::HashMap<usize, String>,
    ) -> Vec<usize> {
        let Some(declared) = self.struct_fields.get(struct_name) else {
            return (0..fields.len()).collect();
        };
        let mut order = Vec::with_capacity(fields.len());
        let mut used = std::collections::HashSet::new();
        for declared_name in declared {
            for (field_idx, _) in fields.iter().enumerate() {
                if resolved_names
                    .get(&field_idx)
                    .is_some_and(|name| name == declared_name)
                {
                    order.push(field_idx);
                    used.insert(field_idx);
                }
            }
        }
        for field_idx in 0..fields.len() {
            if !used.contains(&field_idx) {
                order.push(field_idx);
            }
        }
        order
    }

    fn resolve_struct_literal_field_names(
        &self,
        struct_name: &str,
        fields: &[gossamer_ast::StructExprField],
    ) -> std::collections::HashMap<usize, String> {
        let Some(declared) = self.struct_fields.get(struct_name) else {
            return std::collections::HashMap::new();
        };
        let declared_by_name: std::collections::HashMap<&str, usize> = declared
            .iter()
            .enumerate()
            .map(|(idx, name)| (name.as_str(), idx))
            .collect();
        let mut resolved = std::collections::HashMap::new();
        let mut filled = std::collections::HashSet::new();
        for (field_idx, field) in fields.iter().enumerate() {
            if struct_literal_positional_index(&field.name.name).is_some() {
                continue;
            }
            if let Some(&decl_idx) = declared_by_name.get(field.name.name.as_str()) {
                filled.insert(decl_idx);
                resolved.insert(field_idx, declared[decl_idx].clone());
            }
        }

        let mut next_pos = 0usize;
        for (field_idx, field) in fields.iter().enumerate() {
            if struct_literal_positional_index(&field.name.name).is_none() {
                continue;
            }
            while next_pos < declared.len() && filled.contains(&next_pos) {
                next_pos += 1;
            }
            if next_pos >= declared.len() {
                continue;
            }
            filled.insert(next_pos);
            resolved.insert(field_idx, declared[next_pos].clone());
        }
        resolved
    }

    pub(super) fn lower_array(&mut self, arr: &AstArrayExpr) -> HirArrayExpr {
        match arr {
            AstArrayExpr::List(elems) => {
                HirArrayExpr::List(elems.iter().map(|e| self.lower_expr(e)).collect())
            }
            AstArrayExpr::Repeat { value, count } => HirArrayExpr::Repeat {
                value: Box::new(self.lower_expr(value)),
                count: Box::new(self.lower_expr(count)),
            },
        }
    }

    /// `true` when a map literal is built by inserting each pair instead of
    /// from the entry array. A by-value aggregate lives in the entry array as
    /// inline slots rather than as one word, and a callable value takes the
    /// env-shaped form of the map's value slot, which only an insert into the
    /// typed map gives it.
    fn map_literal_inserts(&self, map_ty: Ty) -> bool {
        use gossamer_types::TyKind;
        let Some(TyKind::HashMap { value, .. }) = self.tcx.kind(map_ty) else {
            return false;
        };
        matches!(
            self.tcx.kind(*value),
            Some(TyKind::Tuple(_) | TyKind::Adt { .. } | TyKind::FnPtr(_) | TyKind::FnTrait(_))
        )
    }

    /// Builds `{ let mut m = Map::new(); m.insert(k, v); ...; m }` for a map
    /// whose values are aggregates.
    fn lower_map_literal_by_insert(
        &mut self,
        entries: &[AstExpr],
        span: Span,
        map_ty: Ty,
    ) -> HirExprKind {
        let name = Ident::new(MAP_LITERAL_BINDING);
        let ctor = HirExpr {
            id: self.fresh(),
            span,
            ty: map_ty,
            kind: HirExprKind::Call {
                callee: Box::new(HirExpr {
                    id: self.fresh(),
                    span,
                    ty: self.error_ty(),
                    kind: HirExprKind::Path {
                        segments: vec![Ident::new("Map"), Ident::new("new")],
                        def: None,
                    },
                }),
                args: Vec::new(),
            },
        };
        let mut stmts = vec![HirStmt {
            id: self.fresh(),
            span,
            kind: HirStmtKind::Let {
                pattern: HirPat {
                    id: self.fresh(),
                    span,
                    ty: map_ty,
                    kind: HirPatKind::Binding {
                        name: name.clone(),
                        mutable: true,
                    },
                },
                ty: map_ty,
                init: Some(ctor),
            },
        }];
        for entry in entries {
            let AstExprKind::Tuple(parts) = &entry.kind else {
                continue;
            };
            let [key, value] = parts.as_slice() else {
                continue;
            };
            let receiver = HirExpr {
                id: self.fresh(),
                span,
                ty: map_ty,
                kind: HirExprKind::Path {
                    segments: vec![name.clone()],
                    def: None,
                },
            };
            let call = HirExpr {
                id: self.fresh(),
                span,
                ty: self.tcx.unit(),
                kind: HirExprKind::MethodCall {
                    receiver: Box::new(receiver),
                    name: Ident::new("insert"),
                    args: vec![self.lower_expr(key), self.lower_expr(value)],
                    owner: None,
                },
            };
            stmts.push(HirStmt {
                id: self.fresh(),
                span,
                kind: HirStmtKind::Expr {
                    expr: call,
                    has_semi: true,
                },
            });
        }
        let tail = HirExpr {
            id: self.fresh(),
            span,
            ty: map_ty,
            kind: HirExprKind::Path {
                segments: vec![name],
                def: None,
            },
        };
        HirExprKind::Block(HirBlock {
            id: self.fresh(),
            span,
            ty: map_ty,
            stmts,
            tail: Some(Box::new(tail)),
            is_comptime: false,
        })
    }

    /// Rewrites a traversal on a map or set into the walk over the iterator
    /// its `iter()` answers, materialising with `collect()` when the traversal
    /// yields a sequence. Returns `None` for every other receiver.
    pub(super) fn desugar_keyed_traversal(
        &mut self,
        expr: &AstExpr,
        receiver: &AstExpr,
        name: &Ident,
        args: &[AstExpr],
    ) -> Option<HirExprKind> {
        use gossamer_types::TyKind;

        if !gossamer_types::is_collection_traversal_method(name.name.as_str())
            || name.name == "iter"
        {
            return None;
        }
        // A reference to a keyed collection walks the collection it names, so
        // the container's own type decides the element. The cursor call below
        // takes the receiver as written, which the `iter()` lowering already
        // reads through a borrow.
        let mut recv_ty = self.ty_of(receiver.id);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(recv_ty) {
            recv_ty = *inner;
        }
        // The element a walk sees: a map yields its key/value pair, a set its
        // value.
        let elem = match self.tcx.kind(recv_ty) {
            Some(TyKind::HashMap { key, value, .. }) => {
                let (key, value) = (*key, *value);
                self.tcx.intern(TyKind::Tuple(vec![key, value]))
            }
            Some(TyKind::Adt { def, substs })
                if def.local == u32::MAX - 7 || def.local == u32::MAX - 18 =>
            {
                *substs.types().first()?
            }
            _ => return None,
        };
        let span = expr.span;
        let out_ty = self.ty_of(expr.id);
        let lowered_receiver = self.lower_expr(receiver);
        let cursor = HirExpr {
            id: self.fresh(),
            span,
            ty: self.tcx.intern(TyKind::Iterator(elem)),
            kind: HirExprKind::MethodCall {
                receiver: Box::new(lowered_receiver),
                name: Ident::new("iter"),
                args: Vec::new(),
                owner: None,
            },
        };
        let walked = HirExprKind::MethodCall {
            receiver: Box::new(cursor),
            name: name.clone(),
            args: args.iter().map(|a| self.lower_expr(a)).collect(),
            owner: None,
        };
        // An adapter answers another iterator, so a sequence result is
        // materialised the way the eager spelling promises.
        let out_elem = match self.tcx.kind(out_ty) {
            Some(TyKind::Vec(out_elem)) => *out_elem,
            _ => return Some(walked),
        };
        let inner = HirExpr {
            id: self.fresh(),
            span,
            ty: self.tcx.intern(TyKind::Iterator(out_elem)),
            kind: walked,
        };
        Some(HirExprKind::MethodCall {
            receiver: Box::new(inner),
            name: Ident::new("collect"),
            args: Vec::new(),
            owner: None,
        })
    }

    pub(super) fn lower_map_literal(
        &mut self,
        entries: &[AstExpr],
        span: Span,
        map_ty: Ty,
    ) -> HirExprKind {
        use gossamer_types::{ArrayLen, TyKind};

        if !entries.is_empty() && self.map_literal_inserts(map_ty) {
            return self.lower_map_literal_by_insert(entries, span, map_ty);
        }
        let lowered_entries: Vec<HirExpr> = entries.iter().map(|e| self.lower_expr(e)).collect();
        let pair_ty = lowered_entries.first().map_or_else(
            || match self.tcx.kind(map_ty) {
                Some(TyKind::HashMap { key, value, .. }) => {
                    self.tcx.intern(TyKind::Tuple(vec![*key, *value]))
                }
                _ => self.error_ty(),
            },
            |entry| entry.ty,
        );
        let array_ty = self.tcx.intern(TyKind::Array {
            elem: pair_ty,
            len: ArrayLen::Concrete(lowered_entries.len()),
        });
        let array_arg = HirExpr {
            id: self.fresh(),
            span,
            ty: array_ty,
            kind: HirExprKind::Array(HirArrayExpr::List(lowered_entries)),
        };
        let callee = HirExpr {
            id: self.fresh(),
            span,
            ty: self.error_ty(),
            kind: HirExprKind::Path {
                segments: vec![Ident::new("Map"), Ident::new("from")],
                def: None,
            },
        };
        HirExprKind::Call {
            callee: Box::new(callee),
            args: vec![array_arg],
        }
    }

    pub(super) fn lower_set_literal(
        &mut self,
        entries: &[AstExpr],
        span: Span,
        set_ty: Ty,
    ) -> HirExprKind {
        use gossamer_types::{ArrayLen, TyKind};

        let lowered_entries: Vec<HirExpr> = entries.iter().map(|e| self.lower_expr(e)).collect();
        let owner = match self.tcx.kind(set_ty) {
            Some(TyKind::Adt { def, .. }) if def.local == BTREE_SET_DEF_LOCAL => "BTreeSet",
            _ => "Set",
        };
        let elem_ty = lowered_entries
            .first()
            .map(|entry| entry.ty)
            .or_else(|| match self.tcx.kind(set_ty) {
                Some(TyKind::Adt { def, substs }) if def.local == HASH_SET_DEF_LOCAL => {
                    substs.types().first().copied()
                }
                Some(TyKind::Adt { def, substs }) if def.local == BTREE_SET_DEF_LOCAL => {
                    substs.types().first().copied()
                }
                _ => None,
            })
            .unwrap_or_else(|| self.error_ty());
        let array_ty = self.tcx.intern(TyKind::Array {
            elem: elem_ty,
            len: ArrayLen::Concrete(lowered_entries.len()),
        });
        let array_arg = HirExpr {
            id: self.fresh(),
            span,
            ty: array_ty,
            kind: HirExprKind::Array(HirArrayExpr::List(lowered_entries)),
        };
        let callee = HirExpr {
            id: self.fresh(),
            span,
            ty: self.error_ty(),
            kind: HirExprKind::Path {
                segments: vec![Ident::new(owner), Ident::new("from")],
                def: None,
            },
        };
        HirExprKind::Call {
            callee: Box::new(callee),
            args: vec![array_arg],
        }
    }
}
