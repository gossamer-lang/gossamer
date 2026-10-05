//! Rules for `ffi::Ptr`, foreign types, the `ffi` memory operations, C
//! callbacks, layout queries, unions, and foreign statics: GT0103 through
//! GT0111.

use gossamer_ast::{Expr, ExprKind, Literal, NodeId};
use gossamer_resolve::{DefId, Resolution};

use super::TypeChecker;
use crate::{ForeignError, Ty, TyKind, TypeError};

/// The `std::ffi` operations that read, write, or forge native memory, by the
/// name the injected wrapper declares, with what the call vouches for.
const UNSAFE_OPERATIONS: &[(&str, &str, &str)] = &[
    (
        "__gos_ffi_read",
        "`ffi::read`",
        "the pointer addresses a live `T`",
    ),
    (
        "__gos_ffi_read_at",
        "`ffi::read_at`",
        "the pointer addresses an array of `T` at least that long",
    ),
    (
        "__gos_ffi_write",
        "`ffi::write`",
        "the pointer addresses writable memory for a `T`",
    ),
    (
        "__gos_ffi_write_at",
        "`ffi::write_at`",
        "the pointer addresses a writable array of `T` at least that long",
    ),
    (
        "__gos_ffi_alloc",
        "`ffi::alloc`",
        "the memory is freed exactly once, by `ffi::free` or by the library it is handed to",
    ),
    (
        "__gos_ffi_free",
        "`ffi::free`",
        "the memory came from the platform C allocator and is not used again",
    ),
    (
        "__gos_ffi_read_bytes",
        "`ffi::read_bytes`",
        "the pointer addresses at least that many readable bytes",
    ),
    (
        "__gos_ffi_read_cstr",
        "`ffi::read_cstr`",
        "the pointer addresses a NUL-terminated string",
    ),
    (
        "__gos_ffi_write_bytes",
        "`ffi::write_bytes`",
        "the pointer addresses room for the bytes",
    ),
    (
        "__gos_ffi_to_c_bytes",
        "`ffi::to_c_bytes`",
        "the copy is freed exactly once, by `ffi::free` or by the library it is handed to",
    ),
    (
        "__gos_ffi_fn_from_ptr",
        "`ffi::fn_from_ptr`",
        "the address is a native function with exactly this signature",
    ),
    (
        "__gos_ffi_Ptr::from_address",
        "`ffi::Ptr::from_address`",
        "the integer is the address of a live value of the pointee type",
    ),
    (
        "__gos_ffi_Handle::from_ptr",
        "`ffi::Handle::from_ptr`",
        "the pointer came from `as_ptr` on a handle of this type",
    ),
    (
        "__gos_ffi_View::new",
        "`ffi::View::new`",
        "the pointer addresses that many live values of the element type for as long as the view is used",
    ),
    (
        "__gos_ffi_atomic_load",
        "`ffi::atomic_load`",
        "the pointer addresses a live integer every concurrent access reaches atomically",
    ),
    (
        "__gos_ffi_atomic_store",
        "`ffi::atomic_store`",
        "the pointer addresses a live integer every concurrent access reaches atomically",
    ),
    (
        "__gos_ffi_atomic_swap",
        "`ffi::atomic_swap`",
        "the pointer addresses a live integer every concurrent access reaches atomically",
    ),
    (
        "__gos_ffi_atomic_compare_exchange",
        "`ffi::atomic_compare_exchange`",
        "the pointer addresses a live integer every concurrent access reaches atomically",
    ),
    (
        "__gos_ffi_atomic_fetch_add",
        "`ffi::atomic_fetch_add`",
        "the pointer addresses a live integer every concurrent access reaches atomically",
    ),
    (
        "__gos_ffi_atomic_fetch_sub",
        "`ffi::atomic_fetch_sub`",
        "the pointer addresses a live integer every concurrent access reaches atomically",
    ),
    (
        "__gos_ffi_atomic_fetch_and",
        "`ffi::atomic_fetch_and`",
        "the pointer addresses a live integer every concurrent access reaches atomically",
    ),
    (
        "__gos_ffi_atomic_fetch_or",
        "`ffi::atomic_fetch_or`",
        "the pointer addresses a live integer every concurrent access reaches atomically",
    ),
    (
        "__gos_ffi_atomic_fetch_xor",
        "`ffi::atomic_fetch_xor`",
        "the pointer addresses a live integer every concurrent access reaches atomically",
    ),
];

/// The registered name of the injected `ffi::View` struct.
pub(crate) const VIEW_TYPE: &str = "__gos_ffi_View";

/// The registered name of the injected `ffi::Union` struct.
pub(crate) const UNION_TYPE: &str = "__gos_ffi_Union";

/// A layout query or union operation, checked once the enclosing body's
/// inference has settled.
pub(crate) struct PendingLayout {
    kind: LayoutQuery,
    span: gossamer_lex::Span,
    /// The callee whose recorded type carries a query's type argument.
    callee: Option<NodeId>,
    /// The union a union operation acts on.
    union: Option<Ty>,
    /// The member type a union operation takes or gives.
    member: Option<Ty>,
    /// `offset_of`'s field path, when it is a literal.
    path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayoutQuery {
    SizeOf,
    AlignOf,
    OffsetOf,
    UnionOp,
}

/// The registered name of the injected `ffi::Ptr` struct.
pub(crate) const PTR_TYPE: &str = "__gos_ffi_Ptr";
/// The registered name of the injected `ffi::Handle` struct.
pub(crate) const HANDLE_TYPE: &str = "__gos_ffi_Handle";

impl TypeChecker<'_> {
    /// The declared name of the struct or enum `ty`, when it is one.
    fn adt_name(&mut self, ty: Ty) -> Option<(DefId, String, Vec<Ty>)> {
        let ty = self.infer.resolve(self.tcx, ty);
        let Some(TyKind::Adt { def, substs }) = self.tcx.kind(ty).cloned() else {
            return None;
        };
        let name = self.tcx.def_name(def)?.to_string();
        Some((def, name, substs.types()))
    }

    /// The pointee of `ty` when it is `ffi::Ptr<T>`.
    pub(super) fn ffi_ptr_elem(&mut self, ty: Ty) -> Option<Ty> {
        let (_, name, args) = self.adt_name(ty)?;
        (name == PTR_TYPE).then(|| args.first().copied()).flatten()
    }

    /// Whether `ty` is `ffi::Handle<T>`.
    fn is_ffi_handle(&mut self, ty: Ty) -> bool {
        self.adt_name(ty)
            .is_some_and(|(_, name, _)| name == HANDLE_TYPE)
    }

    /// The element type of `ty` when it is `ffi::View<T>`.
    fn ffi_view_elem(&mut self, ty: Ty) -> Option<Ty> {
        let (_, name, args) = self.adt_name(ty)?;
        (name == VIEW_TYPE).then(|| args.first().copied()).flatten()
    }

    /// The payload of `ty` when it is `Option<T>`.
    fn option_payload(&mut self, ty: Ty) -> Option<Ty> {
        let (_, name, args) = self.adt_name(ty)?;
        (name == "Option").then(|| args.first().copied()).flatten()
    }

    /// The foreign type `ty` names, when it is one declared `type Name` in an
    /// extern block.
    fn opaque_name(&mut self, ty: Ty) -> Option<String> {
        let (def, name, _) = self.adt_name(ty)?;
        self.opaque_types
            .contains(&def)
            .then(|| name.rsplit("::").next().unwrap_or(&name).to_string())
    }

    /// Why `elem` cannot be what an `ffi::Ptr` points at, or `None` when it
    /// can: a scalar, a `Ptr`, a foreign type, or a `#[repr(C)]` plain-data
    /// struct.
    pub(super) fn pointee_problem(&mut self, elem: Ty) -> Option<String> {
        let elem = self.infer.resolve(self.tcx, elem);
        if self.foreign_scalar(elem)
            || self.ffi_ptr_elem(elem).is_some()
            || self.opaque_name(elem).is_some()
            || matches!(
                self.tcx.kind(elem),
                Some(TyKind::Param { .. } | TyKind::Var(_))
            )
        {
            return None;
        }
        if let Some(TyKind::Adt { def, .. }) = self.tcx.kind(elem).cloned()
            && self.repr_c_structs.contains(&def)
            && self.tcx.c_leaves(elem).is_some()
        {
            return None;
        }
        Some(self.render_public_ty(elem))
    }

    /// Why `ty` cannot be a value a foreign function takes or answers, when
    /// it is a pointer form: `Ptr<T>` or `Option<Ptr<T>>` with a pointee
    /// without C layout. `Some(Ok(()))` for an accepted pointer form, `None`
    /// when `ty` is not one.
    pub(super) fn foreign_pointer_form(&mut self, ty: Ty) -> Option<Result<(), String>> {
        let ty = self.infer.resolve(self.tcx, ty);
        let ptr = self.option_payload(ty).unwrap_or(ty);
        let elem = self.ffi_ptr_elem(ptr)?;
        Some(match self.pointee_problem(elem) {
            None => Ok(()),
            Some(pointee) => Err(pointee),
        })
    }

    /// Whether `ty` holds a native address anywhere in its value: a `Ptr`
    /// or a `Handle`, directly or in a field, element, or payload.
    fn holds_native_address(&mut self, ty: Ty, depth: u32) -> bool {
        if depth > 8 {
            return false;
        }
        let ty = self.infer.resolve(self.tcx, ty);
        if self.ffi_ptr_elem(ty).is_some()
            || self.is_ffi_handle(ty)
            || self.ffi_view_elem(ty).is_some()
        {
            return true;
        }
        match self.tcx.kind(ty).cloned() {
            Some(TyKind::Vec(inner) | TyKind::Slice(inner) | TyKind::Array { elem: inner, .. }) => {
                self.holds_native_address(inner, depth + 1)
            }
            Some(TyKind::Tuple(items)) => items
                .iter()
                .any(|item| self.holds_native_address(*item, depth + 1)),
            Some(TyKind::Adt { def, substs }) => {
                let fields = self
                    .tcx
                    .adt_field_tys(def, &substs)
                    .map(<[Ty]>::to_vec)
                    .unwrap_or_default();
                let args = substs.types();
                fields
                    .into_iter()
                    .chain(args)
                    .any(|field| self.holds_native_address(field, depth + 1))
            }
            _ => false,
        }
    }

    /// GT0106 for a `comptime` block or a serialized value whose type holds
    /// a native address.
    pub(super) fn reject_native_address(
        &mut self,
        ty: Ty,
        context: &str,
        span: gossamer_lex::Span,
    ) {
        if self.holds_native_address(ty, 0) {
            let ty = self.render_public_ty(ty);
            self.emit(
                TypeError::Foreign(ForeignError::NotPortable {
                    ty,
                    context: context.to_string(),
                }),
                span,
            );
        }
    }

    /// The `ffi` operation `callee` names, with what it vouches for, when it
    /// is one of [`UNSAFE_OPERATIONS`]. A free operation keeps its injected
    /// name; `Ptr::from_address` and `Handle::from_ptr` are told from a
    /// program's own types of those names by what they answer.
    fn ffi_operation(
        &mut self,
        callee: &Expr,
        ret: Ty,
    ) -> Option<(&'static str, &'static str, &'static str)> {
        let ExprKind::Path(path) = &callee.kind else {
            return None;
        };
        let last = path.segments.last()?.name.name.clone();
        let owner = path
            .segments
            .len()
            .checked_sub(2)
            .and_then(|i| path.segments.get(i))
            .map(|segment| segment.name.name.clone());
        let owner_answers_ffi = match last.as_str() {
            "from_address" => {
                let ptr = self.option_payload(ret).unwrap_or(ret);
                self.ffi_ptr_elem(ptr).is_some()
            }
            "from_ptr" => self.is_ffi_handle(ret),
            "new" => self.ffi_view_elem(ret).is_some(),
            _ => false,
        };
        UNSAFE_OPERATIONS
            .iter()
            .copied()
            .find(|(name, _, _)| match name.split_once("::") {
                None => owner.is_none() && last == *name,
                Some((_, want_last)) => last == want_last && owner.is_some() && owner_answers_ffi,
            })
    }

    /// GT0103 for a call to an `ffi` operation outside `unsafe { }`, and
    /// GT0104 and GT0105 for one over a type with no C layout, once the
    /// call's types are known.
    pub(super) fn check_ffi_operation(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        arg_tys: &[Ty],
        ret: Ty,
    ) {
        self.check_ffi_layout_operation(callee, args, arg_tys, ret);
        let Some((name, what, why)) = self.ffi_operation(callee, ret) else {
            return;
        };
        if self.unsafe_depth == 0 {
            self.emit(
                TypeError::Foreign(ForeignError::UnsafeOperation {
                    what: format!("the call to {what}"),
                    why: format!("the call site vouches that {why}"),
                }),
                callee.span,
            );
        }
        self.pending_ffi_types
            .push((name, what, callee.span, arg_tys.to_vec(), ret));
    }

    /// Runs the pointee checks recorded since the last call, now that the
    /// body's inference has settled.
    pub(super) fn finish_ffi_checks(&mut self) {
        for (name, what, span, arg_tys, ret) in std::mem::take(&mut self.pending_ffi_types) {
            self.check_ffi_operation_types(name, what, span, &arg_tys, ret);
        }
        for pending in std::mem::take(&mut self.pending_ffi_layouts) {
            self.check_pending_layout(&pending);
        }
    }

    /// The final segment of `callee` when it is a plain path.
    fn callee_last(callee: &Expr) -> Option<&str> {
        let ExprKind::Path(path) = &callee.kind else {
            return None;
        };
        Some(path.segments.last()?.name.name.as_str())
    }

    /// Records the layout queries (`size_of`, `align_of`, `offset_of`), the
    /// union constructors, and `addr_of` for checking.
    fn check_ffi_layout_operation(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        arg_tys: &[Ty],
        ret: Ty,
    ) {
        let Some(last) = Self::callee_last(callee) else {
            return;
        };
        let kind = match last {
            "__gos_ffi_size_of" => LayoutQuery::SizeOf,
            "__gos_ffi_align_of" => LayoutQuery::AlignOf,
            "__gos_ffi_offset_of" => LayoutQuery::OffsetOf,
            "__gos_ffi_addr_of" => {
                self.check_addr_of_argument(args);
                return;
            }
            "__gos_ffi_fn_addr" => {
                self.check_fn_addr_argument(args);
                return;
            }
            "new" | "zeroed" if self.union_members(ret).is_some() => {
                self.pending_ffi_layouts.push(PendingLayout {
                    kind: LayoutQuery::UnionOp,
                    span: callee.span,
                    callee: None,
                    union: Some(ret),
                    member: (last == "new").then(|| arg_tys.first().copied()).flatten(),
                    path: None,
                });
                return;
            }
            _ => return,
        };
        let path = match args.first().map(|arg| &arg.kind) {
            Some(ExprKind::Literal(Literal::String(text))) => Some(text.clone()),
            _ => None,
        };
        self.pending_ffi_layouts.push(PendingLayout {
            kind,
            span: callee.span,
            callee: Some(callee.id),
            union: None,
            member: None,
            path,
        });
    }

    /// Marks the argument of an `ffi::addr_of` call as the one place a
    /// foreign static may be named, before the argument is checked.
    pub(super) fn note_addr_of_argument(&mut self, callee: &Expr, args: &[Expr]) {
        if Self::callee_last(callee) == Some("__gos_ffi_addr_of")
            && let Some(arg) = args.first()
        {
            self.addr_of_args.insert(arg.id);
        }
    }

    /// GT0111 for an `ffi::addr_of` argument that is not a foreign static.
    fn check_addr_of_argument(&mut self, args: &[Expr]) {
        let Some(arg) = args.first() else {
            return;
        };
        let names_static = matches!(arg.kind, ExprKind::Path(_))
            && matches!(
                self.resolutions.get(arg.id),
                Some(Resolution::Def { def, .. }) if self.foreign_statics.contains_key(&def)
            );
        if !names_static {
            self.emit(
                TypeError::Foreign(ForeignError::ForeignStatic {
                    what: "`ffi::addr_of` takes the name of a foreign static".to_string(),
                }),
                arg.span,
            );
        }
    }

    /// GT0108 for an `ffi::fn_addr` argument that is not a named top-level
    /// function, and GT0107 for one whose signature has no C form.
    fn check_fn_addr_argument(&mut self, args: &[Expr]) {
        let Some(arg) = args.first() else {
            return;
        };
        let named = match (&arg.kind, self.resolutions.get(arg.id)) {
            (ExprKind::Path(_), Some(Resolution::Def { def, .. }))
                if !self.foreign_fns.contains_key(&def) =>
            {
                self.fn_sigs.get(&def).cloned()
            }
            _ => None,
        };
        let Some(sig) = named else {
            let found = if matches!(arg.kind, ExprKind::Closure { .. }) {
                "a closure".to_string()
            } else {
                let ty = self
                    .table
                    .get(arg.id)
                    .unwrap_or_else(|| self.tcx.error_ty());
                format!("a value of type `{}`", self.render_public_ty(ty))
            };
            self.emit(
                TypeError::Foreign(ForeignError::CallbackArgument {
                    name: "ffi::fn_addr".to_string(),
                    expected: "a named top-level function".to_string(),
                    found,
                }),
                arg.span,
            );
            return;
        };
        let ty = self.tcx.intern(TyKind::FnTrait(sig));
        if let Some(why) = self.callback_problem(ty) {
            let rendered = self.render_public_ty(ty);
            self.emit(
                TypeError::Foreign(ForeignError::CallbackSignature {
                    name: "ffi::fn_addr".to_string(),
                    ty: rendered,
                    why,
                }),
                arg.span,
            );
        }
    }

    /// GT0111 for a foreign static named outside `ffi::addr_of`.
    fn check_foreign_static_use(&mut self, expr: &Expr) {
        if self.addr_of_args.contains(&expr.id) {
            return;
        }
        if let Some(Resolution::Def { def, .. }) = self.resolutions.get(expr.id)
            && let Some(name) = self.foreign_statics.get(&def).cloned()
        {
            self.emit(
                TypeError::Foreign(ForeignError::ForeignStatic {
                    what: format!("the foreign static `{name}` is used as a value"),
                }),
                expr.span,
            );
        }
    }

    /// The member types of `ty` when it is `ffi::Union<M>`: the elements of
    /// the tuple `M`, or `M` alone.
    pub(super) fn union_members(&mut self, ty: Ty) -> Option<Vec<Ty>> {
        let (_, name, args) = self.adt_name(ty)?;
        if name != UNION_TYPE {
            return None;
        }
        let members = self.infer.resolve(self.tcx, *args.first()?);
        Some(match self.tcx.kind(members).cloned() {
            Some(TyKind::Tuple(items)) => items,
            _ => vec![members],
        })
    }

    /// Records a `get` or `set` on a union for checking.
    fn note_union_method(&mut self, expr: &Expr, ty: Ty) {
        let ExprKind::MethodCall {
            receiver,
            name,
            name_span,
            args,
            ..
        } = &expr.kind
        else {
            return;
        };
        let method = name.name.as_str();
        if !matches!(method, "get" | "set") {
            return;
        }
        let Some(receiver_ty) = self.table.get(receiver.id) else {
            return;
        };
        if self.union_members(receiver_ty).is_none() {
            return;
        }
        let member = if method == "get" {
            Some(ty)
        } else {
            args.first().and_then(|arg| self.table.get(arg.id))
        };
        self.pending_ffi_layouts.push(PendingLayout {
            kind: LayoutQuery::UnionOp,
            span: *name_span,
            callee: None,
            union: Some(receiver_ty),
            member,
            path: None,
        });
    }

    /// The first type argument of the generic function `callee` resolved to.
    fn query_type_argument(&mut self, callee: NodeId) -> Option<Ty> {
        let ty = self.table.get(callee)?;
        let ty = self.infer.resolve(self.tcx, ty);
        match self.tcx.kind(ty).cloned() {
            Some(TyKind::FnDef { substs, .. }) => {
                let arg = *substs.types().first()?;
                Some(self.infer.resolve(self.tcx, arg))
            }
            _ => None,
        }
    }

    /// Why `ty` has no C size, or `None` when it has one.
    fn c_layout_problem(&mut self, ty: Ty) -> Option<TypeError> {
        let ty = self.infer.resolve(self.tcx, ty);
        if matches!(
            self.tcx.kind(ty),
            Some(TyKind::Param { .. } | TyKind::Var(_))
        ) {
            return None;
        }
        if let Some(name) = self.opaque_name(ty) {
            return Some(TypeError::Foreign(ForeignError::OpaqueByValue {
                name,
                context: "its size and layout stay in the native library".to_string(),
            }));
        }
        if self.pointee_problem(ty).is_some() || self.tcx.plain_layout(ty).is_none() {
            return Some(TypeError::Foreign(ForeignError::PointerTarget {
                ty: self.render_public_ty(ty),
                context: "it has no size or offsets to ask for".to_string(),
            }));
        }
        None
    }

    fn check_pending_layout(&mut self, pending: &PendingLayout) {
        match pending.kind {
            LayoutQuery::SizeOf | LayoutQuery::AlignOf | LayoutQuery::OffsetOf => {
                let Some(target) = pending.callee.and_then(|id| self.query_type_argument(id))
                else {
                    return;
                };
                if let Some(error) = self.c_layout_problem(target) {
                    self.emit(error, pending.span);
                    return;
                }
                if pending.kind == LayoutQuery::OffsetOf {
                    self.check_offset_of_path(target, pending.path.as_deref(), pending.span);
                }
            }
            LayoutQuery::UnionOp => {
                let Some(union) = pending.union else {
                    return;
                };
                if !self.check_union_members(union, pending.span) {
                    return;
                }
                let Some(member) = pending.member else {
                    return;
                };
                let member = self.infer.resolve(self.tcx, member);
                if matches!(
                    self.tcx.kind(member),
                    Some(TyKind::Var(_) | TyKind::Param { .. })
                ) {
                    return;
                }
                let members = self.union_members(union).unwrap_or_default();
                if !members.iter().any(|m| self.same_type(*m, member)) {
                    let union_name = self.render_public_ty(union);
                    let member_name = self.render_public_ty(member);
                    self.emit(
                        TypeError::Foreign(ForeignError::UnionMember {
                            union: union_name,
                            why: format!("has no member of type `{member_name}`"),
                        }),
                        pending.span,
                    );
                }
            }
        }
    }

    /// GT0110 for a union whose members have no C layout or repeat a type;
    /// answers whether the members are sound.
    pub(super) fn check_union_members(&mut self, union: Ty, span: gossamer_lex::Span) -> bool {
        let Some(members) = self.union_members(union) else {
            return true;
        };
        let union_name = self.render_public_ty(union);
        for (index, member) in members.iter().enumerate() {
            let member = self.infer.resolve(self.tcx, *member);
            if matches!(
                self.tcx.kind(member),
                Some(TyKind::Var(_) | TyKind::Param { .. })
            ) {
                return true;
            }
            if self.c_layout_problem(member).is_some() {
                let member_name = self.render_public_ty(member);
                self.emit(
                    TypeError::Foreign(ForeignError::UnionMember {
                        union: union_name,
                        why: format!("lists `{member_name}`, which has no C layout"),
                    }),
                    span,
                );
                return false;
            }
            if members[..index].iter().any(|m| self.same_type(*m, member)) {
                let member_name = self.render_public_ty(member);
                self.emit(
                    TypeError::Foreign(ForeignError::UnionMember {
                        union: union_name,
                        why: format!("lists `{member_name}` twice"),
                    }),
                    span,
                );
                return false;
            }
        }
        true
    }

    /// GT0109 for an `offset_of` path that is not a literal or names no
    /// field of `target`.
    fn check_offset_of_path(&mut self, target: Ty, path: Option<&str>, span: gossamer_lex::Span) {
        let ty_name = self.render_public_ty(target);
        let report = |this: &mut Self, path: &str, why: String| {
            this.emit(
                TypeError::Foreign(ForeignError::OffsetOfField {
                    ty: ty_name.clone(),
                    path: path.to_string(),
                    why,
                }),
                span,
            );
        };
        let Some(path) = path else {
            report(self, "..", "the path is not a string literal".to_string());
            return;
        };
        let mut current = target;
        for segment in path.split('.') {
            let current_resolved = self.infer.resolve(self.tcx, current);
            let Some(TyKind::Adt { def, substs }) = self.tcx.kind(current_resolved).cloned() else {
                let shown = self.render_public_ty(current_resolved);
                report(self, path, format!("`{shown}` has no fields"));
                return;
            };
            let names = self.struct_fields.get(&def).cloned().unwrap_or_default();
            let Some(index) = names.iter().position(|(name, _)| name == segment) else {
                let shown = self.render_public_ty(current_resolved);
                report(self, path, format!("`{shown}` has no field `{segment}`"));
                return;
            };
            let field_tys = self
                .tcx
                .adt_field_tys(def, &substs)
                .map_or_else(|| names.iter().map(|(_, ty)| *ty).collect(), <[Ty]>::to_vec);
            let Some(field) = field_tys.get(index).copied() else {
                return;
            };
            current = field;
        }
    }

    /// GT0104 and GT0105 for an `ffi` memory operation over a type with no
    /// C layout.
    fn check_ffi_operation_types(
        &mut self,
        name: &str,
        what: &str,
        span: gossamer_lex::Span,
        arg_tys: &[Ty],
        ret: Ty,
    ) {
        if name.starts_with("__gos_ffi_atomic_") {
            let target = arg_tys.first().and_then(|p| self.ffi_ptr_elem(*p));
            if let Some(target) = target {
                let target = self.infer.resolve(self.tcx, target);
                let atomic_width = matches!(
                    self.tcx.kind(target),
                    Some(
                        TyKind::Int(
                            crate::IntTy::I32
                                | crate::IntTy::U32
                                | crate::IntTy::I64
                                | crate::IntTy::U64
                                | crate::IntTy::Isize
                                | crate::IntTy::Usize
                        ) | TyKind::Var(_)
                            | TyKind::Param { .. }
                    )
                );
                if !atomic_width {
                    let ty = self.render_public_ty(target);
                    self.emit(
                        TypeError::Foreign(ForeignError::PointerTarget {
                            ty,
                            context: format!("{what} works on 32- and 64-bit integers"),
                        }),
                        span,
                    );
                }
            }
            return;
        }
        let target = match name {
            "__gos_ffi_read" | "__gos_ffi_read_at" => Some(ret),
            "__gos_ffi_write" => arg_tys.get(1).copied(),
            "__gos_ffi_write_at" => arg_tys.get(2).copied(),
            "__gos_ffi_alloc" => self.ffi_ptr_elem(ret),
            "__gos_ffi_View::new" => self.ffi_view_elem(ret),
            _ => None,
        };
        let Some(target) = target else {
            return;
        };
        if let Some(name) = self.opaque_name(target) {
            self.emit(
                TypeError::Foreign(ForeignError::OpaqueByValue {
                    name,
                    context: format!("{what} copies values of it"),
                }),
                span,
            );
            return;
        }
        if let Some(ty) = self.pointee_problem(target) {
            self.emit(
                TypeError::Foreign(ForeignError::PointerTarget {
                    ty,
                    context: format!("{what} cannot copy it"),
                }),
                span,
            );
        }
    }

    /// GT0104 for a value of a foreign type, and GT0103 for a `Ptr` or
    /// `Handle` built from an integer outside `unsafe { }`: a struct literal
    /// or unit path whose type is `ty`.
    pub(super) fn check_ffi_construction(&mut self, ty: Ty, span: gossamer_lex::Span) {
        if let Some(name) = self.opaque_name(ty) {
            self.emit(
                TypeError::Foreign(ForeignError::OpaqueByValue {
                    name,
                    context: "it cannot be constructed".to_string(),
                }),
                span,
            );
            return;
        }
        if self.unsafe_depth == 0
            && (self.ffi_ptr_elem(ty).is_some()
                || self.is_ffi_handle(ty)
                || self.ffi_view_elem(ty).is_some())
        {
            self.emit(
                TypeError::Foreign(ForeignError::UnsafeOperation {
                    what: "building an `ffi` pointer or handle from an integer".to_string(),
                    why: "the call site vouches that the integer is a live address or handle; \
                          `ffi::Ptr::from_address` is the spelling"
                        .to_string(),
                }),
                span,
            );
        }
    }

    /// GT0103 for an assignment to the address inside a `Ptr` or `Handle`
    /// outside `unsafe { }`.
    pub(super) fn check_ffi_field_write(&mut self, place: &Expr) {
        if self.unsafe_depth > 0 {
            return;
        }
        let ExprKind::FieldAccess { receiver, .. } = &place.kind else {
            return;
        };
        let Some(receiver_ty) = self.table.get(receiver.id) else {
            return;
        };
        if self.ffi_ptr_elem(receiver_ty).is_some()
            || self.is_ffi_handle(receiver_ty)
            || self.ffi_view_elem(receiver_ty).is_some()
        {
            self.emit(
                TypeError::Foreign(ForeignError::UnsafeOperation {
                    what: "writing the address inside an `ffi` pointer or handle".to_string(),
                    why: "the call site vouches that the new address is live".to_string(),
                }),
                place.span,
            );
        }
    }

    /// Why the foreign function type `ty` cannot be a C callback, or `None`
    /// when every parameter and the result have a C form.
    /// Whether a value of `ty` crosses the C ABI as a callback or exported
    /// function's parameter or result: a scalar, a pointer form, or a
    /// `#[repr(C)]` plain-data struct or union.
    pub(super) fn c_value_ok(&mut self, ty: Ty) -> bool {
        let ty = self.infer.resolve(self.tcx, ty);
        let plain_struct = match self.tcx.kind(ty).cloned() {
            Some(TyKind::Adt { def, substs }) => {
                (self.repr_c_structs.contains(&def)
                    || self.tcx.union_members(def, &substs).is_some())
                    && self.tcx.c_leaves(ty).is_some()
            }
            _ => false,
        };
        self.foreign_scalar(ty)
            || plain_struct
            || matches!(self.foreign_pointer_form(ty), Some(Ok(())))
    }

    pub(super) fn callback_problem(&mut self, ty: Ty) -> Option<String> {
        let ty = self.infer.resolve(self.tcx, ty);
        let Some(TyKind::FnPtr(sig) | TyKind::FnTrait(sig)) = self.tcx.kind(ty).cloned() else {
            return None;
        };
        let value_ok = Self::c_value_ok;
        for input in &sig.inputs {
            if !value_ok(self, *input) {
                return Some(format!(
                    "a callback parameter is a scalar, an `ffi::Ptr`, an `Option<ffi::Ptr>`, \
                     or a `#[repr(C)]` plain-data struct; `{}` is not",
                    self.render_public_ty(*input)
                ));
            }
        }
        let output = self.infer.resolve(self.tcx, sig.output);
        if !matches!(self.tcx.kind(output), Some(TyKind::Unit)) && !value_ok(self, output) {
            return Some(format!(
                "a callback answers `()`, a scalar, an `ffi::Ptr`, an `Option<ffi::Ptr>`, or a \
                 `#[repr(C)]` plain-data struct; `{}` is not",
                self.render_public_ty(output)
            ));
        }
        None
    }

    /// GT0108 for each callback argument of a call to a foreign function
    /// that is not a named function of exactly the declared type.
    pub(super) fn check_callback_arguments(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        arg_tys: &[Ty],
    ) {
        let ExprKind::Path(_) = &callee.kind else {
            return;
        };
        let Some(Resolution::Def { def, .. }) = self.resolutions.get(callee.id) else {
            return;
        };
        let Some(name) = self.foreign_fns.get(&def).cloned() else {
            return;
        };
        let Some(sig) = self.fn_sigs.get(&def).cloned() else {
            return;
        };
        for ((param, arg), arg_ty) in sig.inputs.iter().zip(args).zip(arg_tys) {
            let param = self.infer.resolve(self.tcx, *param);
            let Some(TyKind::FnPtr(expected_sig) | TyKind::FnTrait(expected_sig)) =
                self.tcx.kind(param).cloned()
            else {
                continue;
            };
            let named_fn = match (&arg.kind, self.resolutions.get(arg.id)) {
                (ExprKind::Path(_), Some(Resolution::Def { def, .. })) => self
                    .fn_sigs
                    .get(&def)
                    .cloned()
                    .filter(|_| !self.foreign_fns.contains_key(&def)),
                _ => None,
            };
            let found = match &named_fn {
                Some(found_sig) => {
                    let same = found_sig.inputs.len() == expected_sig.inputs.len()
                        && found_sig
                            .inputs
                            .iter()
                            .zip(&expected_sig.inputs)
                            .all(|(a, b)| self.same_type(*a, *b))
                        && self.same_type(found_sig.output, expected_sig.output);
                    if same {
                        continue;
                    }
                    let found_ty = self.tcx.intern(TyKind::FnTrait(found_sig.clone()));
                    format!("a function of type `{}`", self.render_public_ty(found_ty))
                }
                None if matches!(arg.kind, ExprKind::Closure { .. }) => "a closure".to_string(),
                None => format!("a value of type `{}`", self.render_public_ty(*arg_ty)),
            };
            let expected = self.render_public_ty(param);
            self.emit(
                TypeError::Foreign(ForeignError::CallbackArgument {
                    name: name.clone(),
                    expected,
                    found,
                }),
                arg.span,
            );
        }
    }

    /// The foreign-boundary rules an expression of type `ty` is subject to
    /// whatever its context: constructing a foreign type, a `Ptr`, or a
    /// `Handle`, and a `comptime` block answering a native address.
    pub(super) fn check_ffi_expr(&mut self, expr: &Expr, ty: Ty) {
        match &expr.kind {
            ExprKind::Struct { .. } => self.check_ffi_construction(ty, expr.span),
            ExprKind::Path(_) => {
                if let Some(Resolution::Def { def, .. }) = self.resolutions.get(expr.id)
                    && self.opaque_types.contains(&def)
                {
                    self.check_ffi_construction(ty, expr.span);
                }
                self.check_foreign_static_use(expr);
            }
            ExprKind::MethodCall { .. } => self.note_union_method(expr, ty),
            ExprKind::Block(block) if block.is_comptime() => {
                self.reject_native_address(ty, "after compilation", expr.span);
            }
            _ => {}
        }
    }

    fn same_type(&mut self, a: Ty, b: Ty) -> bool {
        let a = self.infer.resolve(self.tcx, a);
        let b = self.infer.resolve(self.tcx, b);
        a == b || self.render_public_ty(a) == self.render_public_ty(b)
    }
}
