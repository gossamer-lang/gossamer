//! Compiling calls that render values: `join`, and types with a user `Display` or `Debug`.

use super::*;

impl<'tcx> FnBuilder<'tcx> {
    /// The single `Type::method` key matching bare `method`, when exactly
    /// one `impl` in the program declares it. Two or more is genuinely
    /// ambiguous without the receiver's type, so the caller keeps the
    /// by-name dispatch.
    /// `xs.join(sep)` where the sequence's element type supplies its own
    /// rendering: the separator and that method's qualified name travel to
    /// the runtime, which dispatches per element.
    pub(super) fn try_compile_rendered_join(
        &mut self,
        receiver: &HirExpr,
        separator: &HirExpr,
    ) -> RuntimeResult<Option<Reg>> {
        let mut seq_ty = receiver.ty;
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(seq_ty) {
            seq_ty = *inner;
        }
        let elem_ty = match self.tcx.kind(seq_ty) {
            Some(TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. }) => *elem,
            _ => return Ok(None),
        };
        let Some(method) = self.user_rendering_qualified_name(elem_ty, "to_string") else {
            return Ok(None);
        };
        let receiver_reg = self.compile_expr(receiver)?;
        let sep_reg = self.compile_expr(separator)?;
        let method_reg = self.alloc_reg();
        let const_idx = self.const_idx(
            ConstKey::String(method.clone()),
            Value::String(method.as_str().into()),
        );
        self.emit(Op::LoadConst {
            dst: method_reg,
            idx: const_idx,
        });
        let args_start = self.next_reg;
        self.next_reg = self
            .next_reg
            .checked_add(2)
            .expect("register overflow reserving join args");
        self.ensure_reg_slot(args_start + 1);
        self.emit(Op::Move {
            dst: args_start,
            src: sep_reg,
        });
        self.emit(Op::Move {
            dst: args_start + 1,
            src: method_reg,
        });
        let dst = self.alloc_reg();
        let name_idx = self.global_idx("__join_rendered");
        let cache_idx = self.alloc_cache_idx();
        self.emit(Op::MethodCall {
            dst,
            receiver: receiver_reg,
            name_idx,
            args: args_start,
            argc: 2,
            cache_idx,
        });
        Ok(Some(dst))
    }

    /// The fully qualified `Type::method` a user `impl` supplies to render
    /// values of `ty` on `method`'s channel, or `None` when nothing overrides
    /// the synthesized form. `method` is `to_string` for `Display` (`{}`) and
    /// `fmt` for `Debug` (`{:?}`); the two channels never borrow each other's
    /// method, exactly as `Display` and `Debug` stay distinct traits.
    fn user_rendering_qualified_name(&self, ty: Ty, method: &str) -> Option<String> {
        let mut resolved = ty;
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = *inner;
        }
        let type_name = match self.tcx.kind(resolved) {
            Some(TyKind::Adt { def, .. }) => self.tcx.def_name(*def)?.to_string(),
            // A sequence and a map are their own type kinds rather than named
            // Adts, so their source name is the one an `impl` for them
            // registers under - without this an `impl Display for Vec` was
            // compiled and never called.
            Some(TyKind::Vec(_) | TyKind::Slice(_)) => "Vec".to_string(),
            Some(TyKind::HashMap { ordered, .. }) => {
                if *ordered { "BTreeMap" } else { "Map" }.to_string()
            }
            _ => return None,
        };
        let qualified = format!("{type_name}::{method}");
        self.fn_param_tys
            .contains_key(&qualified)
            .then_some(qualified)
    }

    /// Whether a user `impl` supplies `method` for `ty`, so a value of it
    /// renders through that body rather than the synthesized form.
    pub(crate) fn has_user_rendering(&self, ty: Ty, method: &str) -> bool {
        self.user_rendering_qualified_name(ty, method).is_some()
    }

    /// Renders `arg` through its type's own method for this channel when one
    /// exists, so `{}` shows what `impl Display` says and `{:?}` what
    /// `impl Debug` says, rather than the synthesized shape. `Ok(None)` when
    /// nothing overrides.
    pub(super) fn compile_user_rendering(
        &mut self,
        arg: &HirExpr,
        method: &str,
    ) -> RuntimeResult<Option<Reg>> {
        // A generic type whose instantiation the value alone cannot spell - a
        // field declared with a type parameter that holds a `Vec` or a `u64` -
        // takes the walk below, which hands its method the renderer's described
        // copy. Any other type calls its method directly: the body reads the
        // value, so a descriptor has nothing to spell for it.
        let static_ty = self.static_ty(arg);
        let spelled_by_instantiation = self.uint_leaves_desc(static_ty).is_some()
            && matches!(
                self.tcx.kind(static_ty),
                Some(TyKind::Adt { substs, .. }) if !substs.types().is_empty()
            );
        if self.has_user_rendering(static_ty, method) && !spelled_by_instantiation {
            let name = Ident {
                name: method.to_string(),
            };
            return self.compile_method_call(arg, &name, &[], None).map(Some);
        }
        // A container, tuple, or `Option` holding such a type renders its
        // elements the same way, at any depth. The value carries its type
        // name at run time, so the walk resolves each element's method
        // itself; this only decides whether the walk is worth entering.
        if !self.ty_contains_user_rendering(self.static_ty(arg), 0, method) {
            return Ok(None);
        }
        let idx = self.global_idx("__render_display");
        let callee_reg = self.alloc_reg();
        self.emit(Op::LoadGlobal {
            dst: callee_reg,
            idx,
        });
        let compiled = self.compile_expr(arg)?;
        // The walk sees values, and a `Vec` and a fixed array share one
        // runtime representation while a `u64` shares a slot with an
        // `i64`. The descriptor built from the static type travels with
        // the renderer's private copy so the walk spells both the way a
        // program that wrote them spells them.
        let value = match self.uint_leaves_desc(self.static_ty(arg)) {
            Some(desc) => {
                let dst = self.alloc_reg();
                let desc_idx = self.const_idx(
                    ConstKey::String(desc.clone()),
                    Value::String(desc.as_str().into()),
                );
                self.emit(Op::UintLeaves {
                    dst,
                    src: compiled,
                    desc_idx,
                });
                dst
            }
            None => compiled,
        };
        // An enum value carries only its variant name at run time, so the
        // walk cannot name the type whose `impl` renders it. The compiler
        // knows both, and hands over `Variant=Type::method` lines for every
        // enum nested in the operand.
        let mut aliases = String::new();
        self.collect_variant_rendering_aliases(self.static_ty(arg), 0, method, &mut aliases);
        let alias_reg = self.alloc_reg();
        let const_idx = self.const_idx(
            ConstKey::String(aliases.clone()),
            Value::String(aliases.as_str().into()),
        );
        self.emit(Op::LoadConst {
            dst: alias_reg,
            idx: const_idx,
        });
        let method_reg = self.alloc_reg();
        let method_idx = self.const_idx(
            ConstKey::String(method.to_string()),
            Value::String(method.into()),
        );
        self.emit(Op::LoadConst {
            dst: method_reg,
            idx: method_idx,
        });
        let args_start = self.next_reg;
        self.next_reg = self
            .next_reg
            .checked_add(3)
            .expect("register overflow reserving render args");
        self.ensure_reg_slot(args_start + 2);
        self.emit(Op::Move {
            dst: args_start,
            src: value,
        });
        self.emit(Op::Move {
            dst: args_start + 1,
            src: alias_reg,
        });
        self.emit(Op::Move {
            dst: args_start + 2,
            src: method_reg,
        });
        let dst = self.alloc_reg();
        let cache_idx = self.alloc_cache_idx();
        self.emit(Op::Call {
            dst,
            callee: callee_reg,
            args: args_start,
            argc: 3,
            cache_idx,
            may_have_cells: true,
        });
        Ok(Some(dst))
    }

    /// Appends one `Variant=Type::method` line per variant of every enum
    /// nested in `ty` whose own type supplies a rendering, so the runtime
    /// walk can resolve a variant value back to its enum.
    fn collect_variant_rendering_aliases(
        &self,
        ty: Ty,
        depth: u32,
        method: &str,
        out: &mut String,
    ) {
        if depth > 8 {
            return;
        }
        let mut resolved = ty;
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = *inner;
        }
        match self.tcx.kind(resolved) {
            Some(TyKind::Adt { def, substs }) => {
                let (def, substs) = (*def, substs.clone());
                if let Some(qualified) = self.user_rendering_qualified_name(resolved, method)
                    && let Some(names) = self.tcx.enum_variant_names(def)
                {
                    for variant in names {
                        out.push_str(variant);
                        out.push('=');
                        out.push_str(&qualified);
                        out.push('\n');
                    }
                }
                for arg in substs.types() {
                    self.collect_variant_rendering_aliases(arg, depth + 1, method, out);
                }
            }
            Some(TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. }) => {
                self.collect_variant_rendering_aliases(*elem, depth + 1, method, out);
            }
            Some(TyKind::Tuple(elems)) => {
                for elem in elems.clone() {
                    self.collect_variant_rendering_aliases(elem, depth + 1, method, out);
                }
            }
            Some(TyKind::HashMap { key, value, .. }) => {
                let (key, value) = (*key, *value);
                self.collect_variant_rendering_aliases(key, depth + 1, method, out);
                self.collect_variant_rendering_aliases(value, depth + 1, method, out);
            }
            _ => {}
        }
    }

    /// Whether `ty`, or a type nested inside it, supplies its own rendering
    /// for this channel.
    fn ty_contains_user_rendering(&self, ty: Ty, depth: u32, method: &str) -> bool {
        // A recursive type would otherwise walk forever; a rendering method
        // that only appears below this many levels is rare enough that the
        // synthesized form is the honest answer.
        if depth > 8 {
            return false;
        }
        let mut resolved = ty;
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(resolved) {
            resolved = *inner;
        }
        if self.has_user_rendering(resolved, method) {
            return true;
        }
        match self.tcx.kind(resolved) {
            Some(TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. }) => {
                self.ty_contains_user_rendering(*elem, depth + 1, method)
            }
            Some(TyKind::Tuple(elems)) => elems
                .clone()
                .iter()
                .any(|elem| self.ty_contains_user_rendering(*elem, depth + 1, method)),
            Some(TyKind::HashMap { key, value, .. }) => {
                let (key, value) = (*key, *value);
                self.ty_contains_user_rendering(key, depth + 1, method)
                    || self.ty_contains_user_rendering(value, depth + 1, method)
            }
            Some(TyKind::Adt { substs, .. }) => substs
                .types()
                .clone()
                .into_iter()
                .any(|arg| self.ty_contains_user_rendering(arg, depth + 1, method)),
            _ => false,
        }
    }

    pub(super) fn sole_impl_method(&self, method: &str) -> Option<String> {
        let suffix = format!("::{method}");
        let mut found: Option<&String> = None;
        for key in self.fn_param_tys.keys() {
            if !key.ends_with(&suffix) {
                continue;
            }
            // Only a method answers a method call. A module's free function
            // is filed under `module::name` too, and binding one here would
            // hand `value.name(..)` to a function that never took a receiver.
            if !self.impl_methods.contains(key.as_str()) {
                continue;
            }
            // `mod::Type::method` and `Type::method` name one method.
            if found.is_some_and(|prev| !prev.ends_with(key.as_str()) && !key.ends_with(prev)) {
                return None;
            }
            if found.is_none_or(|prev| key.len() < prev.len()) {
                found = Some(key);
            }
        }
        found.cloned()
    }

    pub(super) fn callee_param_tys(&self, callee: &HirExpr) -> Option<Vec<Ty>> {
        let HirExprKind::Path { segments, .. } = &callee.kind else {
            return None;
        };
        // A call through a function-typed local is indirect: the value the
        // binding holds decides the parameters, and a global that happens to
        // share the binding's name says nothing about them. Reading that
        // global's types here lowers the argument for a parameter the callee
        // does not have - a `&mut` one wraps it in a cell, and the callback
        // then receives a reference where it declared a value.
        if segments.len() == 1 && self.lookup_local(&segments[0].name).is_some() {
            return None;
        }
        let key = segments
            .iter()
            .map(|seg| seg.name.as_str())
            .collect::<Vec<_>>()
            .join("::");
        self.fn_param_tys.get(&key).cloned()
    }
}
