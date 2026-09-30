//! Resolving written types: paths, associated projections, and const generics.

use super::{
    AstGenericArg, AstType, AstTypeKind, BTREE_SET_DEF_LOCAL, Expectation, Expr, ExprKind,
    FieldPattern, FnSig, HASH_SET_DEF_LOCAL, HashMap, IntTy, LiteralConsts, Mutbl, NodeId, Pattern,
    PatternKind, Resolution, STDLIB_TRAIT_NAMES, SYNC_HANDLE_HI_OFFSET, Span, Ty, TyKind,
    TypeChecker, TypeError, TypePath, U8_VEC_OFFSET, VALIDATE_ERRORS_DEF_LOCAL,
    VALIDATE_FIELD_ERROR_DEF_LOCAL, VEC_DEQUE_DEF_LOCAL, VEC_QUEUE_DEF_LOCAL, VEC_STACK_DEF_LOCAL,
    alias_type_args, builtin_type_head, evaluate_const_int_from_expr, path_matches_dyn_error,
    path_matches_json_value, prim_to_ty, primitive_from_name, stdlib_handle_by_path,
    stdlib_net_handle, subst_alias_params,
};

impl TypeChecker<'_> {
    pub(super) fn type_from_ast(&mut self, ast_ty: &AstType) -> Ty {
        let ty = match &ast_ty.kind {
            AstTypeKind::Unit => self.tcx.unit(),
            AstTypeKind::Never => self.tcx.never(),
            AstTypeKind::Infer => self.fresh(),
            AstTypeKind::Path(path) => self.type_from_ast_path(ast_ty.id, ast_ty.span, path),
            AstTypeKind::Tuple(elems) => {
                let tys: Vec<Ty> = elems.iter().map(|e| self.type_from_ast(e)).collect();
                self.tcx.intern(TyKind::Tuple(tys))
            }
            AstTypeKind::Array { elem, len } => {
                let elem_ty = self.type_from_ast(elem);
                let count = self.array_len_from_ast(len);
                self.tcx.intern(TyKind::Array {
                    elem: elem_ty,
                    len: count,
                })
            }
            AstTypeKind::Slice(inner) => {
                let inner_ty = self.type_from_ast(inner);
                let element = self.render_public_ty(inner_ty);
                self.emit(TypeError::UnsizedSliceValue { element }, ast_ty.span);
                self.tcx.error_ty()
            }
            AstTypeKind::Ref { mutability, inner } => {
                let inner_ty = match &inner.kind {
                    AstTypeKind::Slice(element) => {
                        let element = self.type_from_ast(element);
                        self.tcx.intern(TyKind::Slice(element))
                    }
                    _ => self.type_from_ast(inner),
                };
                let mutability = match mutability {
                    gossamer_ast::Mutability::Immutable => Mutbl::Not,
                    gossamer_ast::Mutability::Mutable => Mutbl::Mut,
                };
                self.tcx.intern(TyKind::Ref {
                    mutability,
                    inner: inner_ty,
                })
            }
            AstTypeKind::Fn { kind, params, ret } => {
                let inputs: Vec<Ty> = params.iter().map(|p| self.type_from_ast(p)).collect();
                let output = match ret.as_ref() {
                    Some(ty) => self.type_from_ast(ty),
                    None => self.tcx.unit(),
                };
                let sig = FnSig { inputs, output };
                match kind {
                    gossamer_ast::FnTypeKind::Fn => self.tcx.intern(TyKind::FnPtr(sig)),
                    // `Fn` / `FnMut` / `FnOnce` all map to the single
                    // `FnTrait` callable shape. The MIR / codegen
                    // machinery uses one fat-pointer ABI for all
                    // three; the borrow-style distinctions Rust
                    // makes are unnecessary in a fully GC'd world.
                    gossamer_ast::FnTypeKind::ClosureFn
                    | gossamer_ast::FnTypeKind::ClosureFnMut
                    | gossamer_ast::FnTypeKind::ClosureFnOnce => {
                        self.tcx.intern(TyKind::FnTrait(sig))
                    }
                }
            }
        };
        // i128 / u128 have no runtime representation on any tier
        // (GT0014); reject at the spelling site so every execution
        // mode fails identically instead of the VM running 128-bit
        // arithmetic at silent 64-bit width.
        if let Some(TyKind::Int(it @ (IntTy::I128 | IntTy::U128))) = self.tcx.kind(ty) {
            let name = if matches!(it, IntTy::I128) {
                "i128"
            } else {
                "u128"
            };
            self.emit(
                TypeError::Int128Unsupported {
                    ty: name.to_string(),
                },
                ast_ty.span,
            );
            let err = self.tcx.error_ty();
            return self.record(ast_ty.id, err);
        }
        self.record(ast_ty.id, ty)
    }

    /// Expands a `type X<..> = T` alias `def` to its underlying type `T`,
    /// substituting the alias's type parameters with the use-site arguments
    /// in `path` for a generic alias. Returns `None` when `def` is not a
    /// registered alias or the argument count does not match the alias's
    /// parameters (the caller then falls back to the nominal form). Emits
    /// GT0024 and yields the error type on a cyclic alias.
    pub(super) fn expand_type_alias(
        &mut self,
        def: gossamer_resolve::DefId,
        name: &str,
        span: Span,
        path: &TypePath,
    ) -> Option<Ty> {
        let (params, rhs) = self.alias_targets.get(&def).cloned()?;
        if !self.alias_expanding.insert(def) {
            self.emit(
                TypeError::CyclicTypeAlias {
                    name: name.to_string(),
                },
                span,
            );
            return Some(self.tcx.error_ty());
        }
        let body = if params.is_empty() {
            Some(rhs)
        } else {
            let args = alias_type_args(path);
            (args.len() == params.len()).then(|| subst_alias_params(&rhs, &params, &args))
        };
        let expanded = body.map(|b| self.type_from_ast(&b));
        self.alias_expanding.remove(&def);
        // An opaque alias keeps the expansion only as its representation:
        // the type it hands back is distinct from that representation and
        // from every other alias over it.
        if self.nominal_aliases.contains(&def) {
            return expanded.map(|repr| self.tcx.nominal_ty(def, repr));
        }
        expanded
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one arm per built-in type constructor; splitting hides the dispatch table"
    )]
    pub(super) fn type_from_ast_path(&mut self, node: NodeId, span: Span, path: &TypePath) -> Ty {
        let head_name = path
            .segments
            .first()
            .map_or("", |seg| seg.name.name.as_str());
        if let Some(prim) = primitive_from_name(head_name) {
            return prim_to_ty(self.tcx, prim);
        }
        // Inside an `impl`, `Self` names the type being implemented. Leaving
        // it as a fresh variable made every `-> Self` constructor's result
        // unconstrained, so the calls on that value went unchecked.
        if head_name == "Self"
            && path.segments.len() == 1
            && let Some(self_ty) = self.current_self_ty
        {
            return self_ty;
        }
        if path.segments.len() >= 2
            && let Some(projected) = self.resolve_assoc_type_projection(path, span)
        {
            return projected;
        }
        // Recognise the stdlib's opaque dynamic JSON value by
        // surface name. The resolver doesn't allocate a `DefId`
        // for it (it comes in via `use std::encoding::json` as a
        // bare import), so we'd otherwise fall through to a fresh
        // inference variable and lose the receiver-shape signal
        // that downstream MIR needs to route field access through
        // the json runtime helpers.
        if path_matches_json_value(path) {
            return self.tcx.json_value_ty();
        }
        if path_matches_dyn_error(path) {
            return self.tcx.dyn_error_ty();
        }
        // The open dynamic value is a prelude type, so its bare name is the
        // whole path.
        if path.segments.len() == 1 && path.segments[0].name.name.as_str() == "DynValue" {
            return self.tcx.dyn_value_ty();
        }
        // A trait names behaviour, not a value's type. Gossamer has no `dyn`,
        // so a bare trait in type position has no runtime shape to stand for
        // and would otherwise settle as an unconstrained variable that
        // accepts anything. A name that also names a type in scope is that
        // type: a program is free to declare its own `Reader`, and the
        // resolver has already said which one this path reached.
        if let Some(last) = path.segments.last()
            && !matches!(
                self.resolutions.get(node),
                Some(
                    Resolution::Def {
                        kind: gossamer_resolve::DefKind::Struct
                            | gossamer_resolve::DefKind::Enum
                            | gossamer_resolve::DefKind::TypeAlias
                            | gossamer_resolve::DefKind::TypeParam,
                        ..
                    } | Resolution::Primitive(_)
                )
            )
            && (STDLIB_TRAIT_NAMES.contains(&last.name.name.as_str())
                || self.trait_own_methods.contains_key(&last.name.name))
        {
            self.emit(
                TypeError::TraitInTypePosition {
                    name: last.name.name.clone(),
                },
                span,
            );
            return self.tcx.error_ty();
        }
        if let Some(resolution) = self.resolutions.get(node) {
            match resolution {
                Resolution::Primitive(prim) => return self.type_from_primitive(prim),
                Resolution::Def { def, kind } => {
                    // A path resolving to a generic type parameter (`fn
                    // f<T>(x: T)` or `struct Pair<A, B> { fst: A }`)
                    // must surface as `TyKind::Param`, not as an `Adt`
                    // whose `def` happens to point at the parameter's
                    // binding. Without this branch, the `A` in `Pair<A>`
                    // unifies as an opaque ADT and any concrete struct
                    // literal hits a `type mismatch: expected adt#N`
                    // error rather than driving inference of A from
                    // the field-value type.
                    //
                    // Use the resolver's per-resolution kind rather
                    // than `resolutions.kind_of(def)` because the
                    // resolver records the DefKind on the
                    // resolution itself but not always on the
                    // separate def→kind map (`bind_generics` only
                    // inserts into the scope, not into the global
                    // map).
                    if kind == gossamer_resolve::DefKind::TypeParam {
                        if let Some((idx, name)) =
                            self.current_generic_scope.get(head_name).cloned()
                        {
                            return self.tcx.intern(TyKind::Param { idx, name });
                        }
                        return self.fresh();
                    }
                    // A non-generic `type X = T` alias is transparent: a use
                    // of `X` lowers to `T`, not to an opaque `adt#N`.
                    if kind == gossamer_resolve::DefKind::TypeAlias
                        && let Some(ty) = self.expand_type_alias(def, head_name, span, path)
                    {
                        return ty;
                    }
                    let substs = self.substs_from_ast(path);
                    return self.tcx.intern(TyKind::Adt { def, substs });
                }
                Resolution::Import { .. } | Resolution::Err | Resolution::Local(_) => {}
            }
        }
        // A built-in type constructor may be written bare (`Deque<i64>`)
        // or under the module that exports it
        // (`std::collections::Deque<i64>`). Both spellings name the same
        // type, so the qualified one reduces to its last segment before
        // the table below keys on it. A user type reached by a qualified
        // path resolved above, so nothing here can capture one.
        let head_name = builtin_type_head(path).unwrap_or(head_name);
        // Fallback for built-in generic enums the resolver doesn't
        // hand out a DefId for (`Result<T, E>`, `Option<T>`). Without
        // this, an annotation like `let r: Result<i64, String> = ...`
        // falls through to a fresh inference variable, losing the
        // substs the variant-binding fixup later needs to re-pin
        // `x` in `Ok(x) => …` to the actual payload type. The
        // sentinel `DefId`s use `u32::MAX` / `u32::MAX-1` so they
        // never collide with anything the resolver emits.
        match head_name {
            "Simd" | "Mask" => return self.simd_type_from_path(head_name, path, span),
            "Result" => {
                let mut substs = self.substs_from_ast(path);
                // `Result<T>` with a single arg is shorthand for
                // `Result<T, errors::Error>`, matching Rust's
                // `anyhow::Result<T>` convention.
                if substs.types().len() == 1 {
                    let e = self.tcx.dyn_error_ty();
                    substs = crate::Substs::from_types([substs.types()[0], e]);
                }
                let def = gossamer_resolve::DefId::local(u32::MAX);
                self.tcx.register_def_name(def, "Result");
                return self.tcx.intern(TyKind::Adt { def, substs });
            }
            "Option" => {
                let substs = self.substs_from_ast(path);
                let def = gossamer_resolve::DefId::local(u32::MAX - 1);
                self.tcx.register_def_name(def, "Option");
                return self.tcx.intern(TyKind::Adt { def, substs });
            }
            "Vec" => {
                let substs = self.substs_from_ast(path);
                let elem = substs
                    .types()
                    .first()
                    .copied()
                    .unwrap_or_else(|| self.fresh());
                return self.tcx.intern(TyKind::Vec(elem));
            }
            // `Range<T>` is what a range expression produces and converts to
            // `Iterator<T>`, so either spelling accepts a range while only
            // `Range` reports back as one.
            "Iterator" | "Range" => {
                let substs = self.substs_from_ast(path);
                let item = substs
                    .types()
                    .first()
                    .copied()
                    .unwrap_or_else(|| self.fresh());
                return if head_name == "Range" {
                    self.tcx.range_ty(item)
                } else {
                    self.tcx.intern(TyKind::Iterator(item))
                };
            }
            // `Sender<T>` / `Receiver<T>` / `JoinHandle<T>` carry their
            // element type in a dedicated `TyKind`. Resolving the
            // annotation to that kind (rather than a fresh inference var)
            // lets `rx.recv()` recover `Option<T>` and a `Sender`-typed
            // param pin the channel element - without it a struct sent
            // over a channel infers as the default `i64` and materialises
            // a single pointer word instead of its inline fields.
            "Sender" => {
                let substs = self.substs_from_ast(path);
                let elem = substs
                    .types()
                    .first()
                    .copied()
                    .unwrap_or_else(|| self.fresh());
                return self.tcx.intern(TyKind::Sender(elem));
            }
            "Receiver" => {
                let substs = self.substs_from_ast(path);
                let elem = substs
                    .types()
                    .first()
                    .copied()
                    .unwrap_or_else(|| self.fresh());
                return self.tcx.intern(TyKind::Receiver(elem));
            }
            "JoinHandle" => {
                let substs = self.substs_from_ast(path);
                let elem = substs
                    .types()
                    .first()
                    .copied()
                    .unwrap_or_else(|| self.fresh());
                return self.tcx.intern(TyKind::JoinHandle(elem));
            }
            "Map" => {
                let substs = self.substs_from_ast(path);
                let tys = substs.types();
                let key = tys.first().copied().unwrap_or_else(|| self.fresh());
                let value = tys.get(1).copied().unwrap_or_else(|| self.fresh());
                return self.tcx.intern(TyKind::HashMap {
                    key,
                    value,
                    ordered: false,
                });
            }
            // `HashSet<T>` / `BTreeSet<T>` are opaque i64 handles at
            // runtime with no dedicated `TyKind`. Resolving the annotation to
            // a named sentinel Adt (rather than a fresh inference var) lets
            // method dispatch recover the receiver kind from its *type* when a
            // set/map flows across a function boundary and the construction
            // tag is gone.
            "Set" | "BTreeSet" => {
                let substs = self.substs_from_ast(path);
                // An annotation that names no element still names a set, so
                // the element is inferred rather than absent - the same
                // fallback the map arms below make. A set whose substs were
                // empty answered no element type, and every method lookup
                // that reads the receiver's element off its type then failed
                // to see a set at all: `impl Display for Set` could not call
                // `self.len()`.
                let substs = if substs.types().is_empty() {
                    let elem = self.fresh();
                    crate::Substs::from_types([elem])
                } else {
                    substs
                };
                let (local, name) = if path
                    .segments
                    .last()
                    .is_some_and(|seg| seg.name.name == "BTreeSet")
                {
                    (BTREE_SET_DEF_LOCAL, "BTreeSet")
                } else {
                    (HASH_SET_DEF_LOCAL, "Set")
                };
                let def = gossamer_resolve::DefId::local(local);
                self.tcx.register_def_name(def, name);
                return self.tcx.intern(TyKind::Adt { def, substs });
            }
            "BTreeMap" => {
                let substs = self.substs_from_ast(path);
                let tys = substs.types();
                let key = tys.first().copied().unwrap_or_else(|| self.fresh());
                let value = tys.get(1).copied().unwrap_or_else(|| self.fresh());
                return self.tcx.intern(TyKind::HashMap {
                    key,
                    value,
                    ordered: true,
                });
            }
            // Phase 1 `VecDeque` is an opaque i64 ring-buffer handle. Resolve
            // the annotation to the named sentinel Adt so method dispatch can
            // recover the receiver kind after construction tags are gone.
            "Deque" | "Queue" | "Stack" => {
                let substs = self.substs_from_ast(path);
                let elem = substs
                    .types()
                    .first()
                    .copied()
                    .unwrap_or_else(|| self.tcx.int_ty(IntTy::I64));
                let elem = self.require_slot_collection_elem(elem, head_name, span);
                let (local, name) = match head_name {
                    "Queue" => (VEC_QUEUE_DEF_LOCAL, "Queue"),
                    "Stack" => (VEC_STACK_DEF_LOCAL, "Stack"),
                    _ => (VEC_DEQUE_DEF_LOCAL, "Deque"),
                };
                let def = gossamer_resolve::DefId::local(local);
                self.tcx.register_def_name(def, name);
                let substs = crate::Substs::from_types([elem]);
                return self.tcx.intern(TyKind::Adt { def, substs });
            }
            "MaxHeap" => {
                let substs = self.substs_from_ast(path);
                let elem = substs
                    .types()
                    .first()
                    .copied()
                    .unwrap_or_else(|| self.tcx.int_ty(IntTy::I64));
                let elem = self.require_slot_collection_elem(elem, head_name, span);
                return self.binary_heap_ty(elem);
            }
            "MinHeap" => {
                let substs = self.substs_from_ast(path);
                let elem = substs
                    .types()
                    .first()
                    .copied()
                    .unwrap_or_else(|| self.tcx.int_ty(IntTy::I64));
                let elem = self.require_slot_collection_elem(elem, head_name, span);
                return self.min_heap_ty(elem);
            }
            "Reverse" => {
                let substs = self.substs_from_ast(path);
                let elem = substs
                    .types()
                    .first()
                    .copied()
                    .unwrap_or_else(|| self.fresh());
                return self.reverse_ty(elem);
            }
            // `Box<T>` / `Arc<T>` / `Rc<T>` name no distinction the language
            // draws: every value is already heap-shared and reference-counted,
            // so the wrapper reads as a choice a writer has to make and there
            // is none. The spelling reports with the rewrite that strips it,
            // and still checks as the inner type so the rest of the program
            // is diagnosed on its own terms.
            "Box" | "Arc" | "Rc" => {
                let substs = self.substs_from_ast(path);
                let tys = substs.types();
                let inner = tys.first().copied();
                let inner_text = inner.map_or_else(
                    || "T".to_string(),
                    |ty| {
                        let rendered = self.render_public_ty(ty);
                        if rendered.is_empty() {
                            "T".to_string()
                        } else {
                            rendered
                        }
                    },
                );
                self.emit(
                    TypeError::TransparentWrapper {
                        wrapper: head_name.to_string(),
                        inner: inner_text,
                    },
                    span,
                );
                if let Some(inner) = inner {
                    return inner;
                }
                return self.fresh();
            }
            // `Weak<T>` - a non-owning reference into an RC allocation.
            // Unlike `Box`/`Arc`/`Rc` it is NOT transparent: it carries
            // its own sentinel ADT so the drop pass releases it via the
            // weak helpers and `upgrade()` can produce an `Option<T>`.
            "Weak" => {
                let substs = self.substs_from_ast(path);
                let payload = substs
                    .types()
                    .first()
                    .copied()
                    .unwrap_or_else(|| self.fresh());
                return self.weak_adt_ty(payload);
            }
            _ => {}
        }
        // `time::Duration` / `time::Instant` are built-in types
        // with no resolver `DefId`. An explicit annotation (`d:
        // time::Duration`) must resolve to the dedicated `TyKind` so the
        // method form (`d.as_millis()`) dispatches on the receiver's
        // static type the same way the inference form does - otherwise
        // it falls to name-global dispatch and fails to lower on the
        // compiled tiers. Match the full module path (not a bare tail)
        // so a user type or `flag::Cell::Duration` named `Duration` is
        // left untouched.
        let segs: Vec<&str> = path.segments.iter().map(|s| s.name.name.as_str()).collect();
        if matches!(
            segs.as_slice(),
            ["time", "Duration"] | ["std", "time", "Duration"]
        ) {
            return self.tcx.duration_ty();
        }
        if matches!(
            segs.as_slice(),
            ["time", "Instant"] | ["std", "time", "Instant"]
        ) {
            return self.tcx.instant_ty();
        }
        if matches!(segs.as_slice(), ["flag", "Set"] | ["std", "flag", "Set"]) {
            return self.flag_set_ty();
        }
        match segs.as_slice() {
            ["http", "Client"] | ["std", "http", "Client"] => return self.http_client_ty(),
            ["http", "ClientBuilder"] | ["std", "http", "ClientBuilder"] => {
                return self.http_client_builder_ty();
            }
            ["http", "Request"] | ["std", "http", "Request"] => return self.http_request_ty(),
            ["http", "Response"] | ["std", "http", "Response"] => return self.http_response_ty(),
            _ => {}
        }
        // Recognise stdlib struct types by their last path segment
        // so parameter annotations like `stream: http::ResponseStream` resolve
        // to the sentinel Adt rather than a fresh inference variable.
        // Without this, the MIR can't recover the struct from the
        // parameter's type and field access (`stream.status`) falls
        // through to gos_rt_json_get instead of a Field(idx) projection.
        let tail = path.segments.last().map_or("", |s| s.name.name.as_str());
        let stdlib_def_offset: Option<u32> = match tail {
            // A written `regex::Pattern` annotation is
            // the same sentinel handle the constructor answers. A parameter
            // carries no construction site, so without this the receiver
            // stays an inference variable and its methods dispatch by bare
            // name - a spelling the bytecode VM registers and the compiled
            // tiers have no symbol for.
            "Pattern" => Some(26),
            "ResponseStream" => Some(4),
            "Response" => Some(5),
            // `context::Context` - an opaque i64 handle with no
            // dedicated `TyKind`. Resolving the annotation to a named
            // sentinel Adt (rather than a fresh inference var) lets
            // method dispatch recover the receiver kind from its *type*
            // when a context flows in as a parameter (the canonical
            // request-propagation shape) and the construction tag is
            // gone - the `is_cancelled` / `cancel` / `done` / `done_chan`
            // calls then route to the `gos_rt_ctx_*` shims.
            "Context" => Some(11),
            // `U8Vec`: a byte-buffer handle. A concrete sentinel here lets
            // the JIT marshal a `buf: U8Vec` parameter across the trampoline
            // (`ty_to_kind` keys on `u32::MAX - 20`) instead of leaving it a
            // fresh inference var the JIT can't classify. It is NOT
            // reference-counted (a handle, like the sockets), which
            // `is_rc_managed` already reports for unregistered sentinels.
            "U8Vec" => Some(U8_VEC_OFFSET),
            "I64Vec" => Some(SYNC_HANDLE_HI_OFFSET),
            "Notifier" => Some(17),
            _ => None,
        };
        if let Some(off) = stdlib_def_offset {
            let def = gossamer_resolve::DefId::local(u32::MAX - off);
            match tail {
                "Context" => self.tcx.register_def_name(def, "context::Context"),
                "U8Vec" | "I64Vec" => self.tcx.register_def_name(def, tail),
                "Notifier" => self.tcx.register_def_name(def, tail),
                // The qualified name the constructor path registers, so a
                // written annotation and a constructed value name one type
                // and the method tables recognise both.
                "Pattern" => self.tcx.register_def_name(def, "regex::Pattern"),
                _ => {}
            }
            return self.tcx.intern(TyKind::Adt {
                def,
                substs: crate::Substs::new(),
            });
        }
        // `validate::Errors` / `validate::FieldError` are opaque i64
        // handles with no dedicated `TyKind`. Resolving the annotation to
        // a named sentinel Adt (instead of a fresh inference var) lets
        // method dispatch recover the handle kind from its *type* when an
        // `Errors` / `FieldError` flows across a function boundary and the
        // construction-site tag is gone - the same recovery `HashSet`
        // and `BTreeMap` rely on.
        let validate_handle: Option<(u32, &str)> = match tail {
            "Errors" => Some((VALIDATE_ERRORS_DEF_LOCAL, "Errors")),
            "FieldError" => Some((VALIDATE_FIELD_ERROR_DEF_LOCAL, "FieldError")),
            _ => None,
        };
        if let Some((local, name)) = validate_handle {
            let def = gossamer_resolve::DefId::local(local);
            self.tcx.register_def_name(def, name);
            return self.tcx.intern(TyKind::Adt {
                def,
                substs: crate::Substs::new(),
            });
        }
        // `net::TcpStream` / `TcpListener` / `UdpSocket` / `UnixStream` /
        // `UnixListener` are opaque i64 socket handles with no dedicated
        // `TyKind`. Resolving the annotation to a named sentinel Adt
        // (instead of a fresh inference var) lets method dispatch recover
        // the handle kind from its *type* when a socket flows through a
        // struct field or a parameter and the construction-site tag is
        // gone - without this `conn.sock.read(..)` lowers to an undefined
        // name-global symbol on the compiled tiers.
        if let Some((off, name)) = stdlib_net_handle(tail) {
            let def = gossamer_resolve::DefId::local(u32::MAX - off);
            self.tcx.register_def_name(def, name);
            return self.tcx.intern(TyKind::Adt {
                def,
                substs: crate::Substs::new(),
            });
        }
        // A type written inside an associated-type binding (`T: Holder<Item
        // = Point>`) sits outside every path the resolver walks, so its
        // node carries no resolution. A user type named there still has to
        // land on its nominal Adt rather than a fresh variable.
        if let Some(def) = self.adt_def_by_name.get(tail).copied() {
            let substs = self.substs_from_ast(path);
            return self.tcx.intern(TyKind::Adt { def, substs });
        }
        // Every other opaque runtime handle, named where no constructor
        // types the slot: a parameter, a field, a return type.
        let written: Vec<&str> = path
            .segments
            .iter()
            .map(|seg| seg.name.name.as_str())
            .collect();
        if let Some((offset, name)) = stdlib_handle_by_path(&written) {
            return self.stdlib_handle_ty(offset, name);
        }
        self.fresh()
    }

    /// `traits` followed by every supertrait reachable from them, in
    /// breadth-first order. A projection resolves at check time rather
    /// than through a vtable, so an associated item a supertrait declares
    /// is reachable through the subtrait that inherits it.
    pub(super) fn with_supertraits(&self, traits: Vec<String>) -> Vec<String> {
        let mut seen: std::collections::HashSet<String> = traits.iter().cloned().collect();
        let mut out = traits;
        let mut next = 0;
        while next < out.len() {
            let current = out[next].clone();
            next += 1;
            for supertrait in self.trait_supertraits.get(&current).into_iter().flatten() {
                if seen.insert(supertrait.clone()) {
                    out.push(supertrait.clone());
                }
            }
        }
        out
    }

    /// Trait names bounding the generic parameter `name` in the scope
    /// currently being checked, from its inline bounds and `where`
    /// predicates alike.
    pub(super) fn bounds_of_param(&self, name: &str) -> Vec<String> {
        let Some((idx, _)) = self.current_generic_scope.get(name) else {
            return Vec::new();
        };
        self.current_param_bounds
            .get(idx.0 as usize)
            .cloned()
            .unwrap_or_default()
    }

    /// Resolves a two-segment type path that projects an associated type
    /// (`T::Item`, `Self::Item`, `Point::Item`). Returns `None` when the
    /// path is not a projection at all, so ordinary qualified type paths
    /// keep their existing handling.
    ///
    /// A projection resolves, in order, through an equality constraint on
    /// the base's bound, through the impl that supplies it for a concrete
    /// base, and through the bounding trait's default or its single
    /// implementor. Everything reachable here is concrete, so the recorded
    /// type never carries a projection into HIR.
    pub(super) fn resolve_assoc_type_projection(
        &mut self,
        path: &TypePath,
        span: Span,
    ) -> Option<Ty> {
        // The last two segments carry the projection, so a module-qualified
        // `inner::Holder::Item` reads the same as a bare `Holder::Item`.
        let count = path.segments.len();
        let base = path.segments.get(count.checked_sub(2)?)?.name.name.clone();
        let name = path.segments.last()?.name.name.clone();
        let is_param = self.current_generic_scope.contains_key(&base);
        let is_self = base == "Self";
        if !is_param
            && !is_self
            && self.assoc.assoc_type_for_self(&base, &name).is_none()
            && self.assoc.assoc_const_ty_for_self(&base, &name).is_none()
        {
            return None;
        }
        if !self.assoc_expanding.insert((base.clone(), name.clone())) {
            self.emit(
                TypeError::CyclicTypeAlias {
                    name: format!("{base}::{name}"),
                },
                span,
            );
            return Some(self.tcx.error_ty());
        }
        let resolved =
            self.resolve_assoc_type_projection_inner(&base, &name, is_param, is_self, span, true);
        self.assoc_expanding.remove(&(base, name));
        Some(resolved)
    }

    /// Body of [`Self::resolve_assoc_type_projection`], also reached from a
    /// method call whose receiver pins the projection. `report` is false
    /// when the same projection was already diagnosed at its declaration.
    pub(super) fn resolve_assoc_type_projection_inner(
        &mut self,
        base: &str,
        name: &str,
        is_param: bool,
        is_self: bool,
        span: Span,
        report: bool,
    ) -> Ty {
        if let Some(bound_ty) = self
            .current_assoc_bindings
            .get(&(base.to_string(), name.to_string()))
            .cloned()
        {
            return self.type_from_ast(&bound_ty);
        }
        let concrete_base = if is_self {
            self.current_self_ty_name.clone()
        } else if is_param {
            None
        } else {
            Some(base.to_string())
        };
        if let Some(concrete) = concrete_base.as_deref() {
            if let Some(ast_ty) = self.assoc.assoc_type_for_self(concrete, name).cloned() {
                return self.type_from_ast(&ast_ty);
            }
            if self.assoc.self_ty_trait_declares(concrete, name) {
                // The trait declares the item and this impl leaves it out.
                // GT0059 names the impl; a second report here would only
                // repeat it at every projection site.
                return self.tcx.error_ty();
            }
        }
        let traits: Vec<String> = if is_self {
            self.with_supertraits(self.current_trait_name.iter().cloned().collect())
        } else if is_param {
            self.with_supertraits(self.bounds_of_param(base))
        } else {
            Vec::new()
        };
        // Inside the trait declaration itself `Self` stands for every
        // implementor, so a projection that no single impl pins is not an
        // error: each specialisation resolves it against its own impl.
        let self_is_abstract = is_self && self.current_self_ty_name.is_none();
        let mut ambiguous_in = None;
        for trait_name in &traits {
            match self.assoc.assoc_type_for_trait(trait_name, name) {
                gossamer_ast::AssocResolution::Found(ast_ty) => {
                    let ast_ty = ast_ty.clone();
                    return self.type_from_ast(&ast_ty);
                }
                gossamer_ast::AssocResolution::Ambiguous => {
                    ambiguous_in = Some(trait_name.clone());
                }
                gossamer_ast::AssocResolution::Unknown => {}
            }
        }
        if let Some(trait_name) = ambiguous_in {
            if self_is_abstract || !report {
                return self.tcx.error_ty();
            }
            self.emit(
                TypeError::AmbiguousAssocItem {
                    base: base.to_string(),
                    trait_name,
                    name: name.to_string(),
                    kind: "type",
                },
                span,
            );
            return self.tcx.error_ty();
        }
        if self_is_abstract
            && traits
                .iter()
                .any(|t| self.assoc.trait_declares_type(t, name))
        {
            return self.tcx.error_ty();
        }
        if !report {
            return self.tcx.error_ty();
        }
        let declared = traits
            .iter()
            .flat_map(|t| self.assoc.declared_assoc_names(t))
            .map(ToString::to_string)
            .collect();
        self.emit(
            TypeError::UnknownAssocItem {
                base: base.to_string(),
                name: name.to_string(),
                declared,
            },
            span,
        );
        self.tcx.error_ty()
    }

    /// Types a two-segment path expression that reads an associated
    /// constant (`Point::MAX`, `T::MAX`, `Self::MAX`). Returns `None` when
    /// the path names no associated constant, leaving ordinary path
    /// resolution in charge.
    pub(super) fn check_assoc_const_path(
        &mut self,
        path: &gossamer_ast::PathExpr,
        span: Span,
    ) -> Option<Ty> {
        let count = path.segments.len();
        let base = path.segments.get(count.checked_sub(2)?)?.name.name.clone();
        let name = path.segments.last()?.name.name.clone();
        let is_param = self.current_generic_scope.contains_key(&base);
        let is_self = base == "Self";
        let concrete_base = if is_self {
            self.current_self_ty_name.clone()
        } else if is_param {
            None
        } else {
            Some(base.clone())
        };
        if let Some(concrete) = concrete_base.as_deref()
            && let Some(ast_ty) = self.assoc.assoc_const_ty_for_self(concrete, &name).cloned()
        {
            return Some(self.type_from_ast(&ast_ty));
        }
        let traits: Vec<String> = if is_self {
            self.with_supertraits(self.current_trait_name.iter().cloned().collect())
        } else if is_param {
            self.with_supertraits(self.bounds_of_param(&base))
        } else {
            return None;
        };
        let declaring: Vec<&String> = traits
            .iter()
            .filter(|t| self.assoc.trait_declares_const(t, &name))
            .collect();
        let trait_name = declaring.first().map(|t| (*t).clone())?;
        match self.assoc.assoc_const_owner_for_trait(&trait_name, &name) {
            gossamer_ast::AssocResolution::Found(owner) => {
                let ast_ty = self.assoc.assoc_const_ty_for_self(&owner, &name).cloned();
                match ast_ty {
                    Some(ast_ty) => Some(self.type_from_ast(&ast_ty)),
                    None => Some(self.tcx.error_ty()),
                }
            }
            gossamer_ast::AssocResolution::Ambiguous => {
                self.emit(
                    TypeError::AmbiguousAssocItem {
                        base,
                        trait_name,
                        name,
                        kind: "const",
                    },
                    span,
                );
                Some(self.tcx.error_ty())
            }
            gossamer_ast::AssocResolution::Unknown => None,
        }
    }

    pub(super) fn substs_from_ast(&mut self, path: &TypePath) -> crate::Substs {
        let mut args = Vec::new();
        for segment in &path.segments {
            for arg in &segment.generics {
                match arg {
                    AstGenericArg::Type(ast_ty) => {
                        if let Some(idx) = self.const_generic_type_arg(ast_ty) {
                            args.push(crate::GenericArg::ConstParam(idx));
                        } else {
                            args.push(crate::GenericArg::Type(self.type_from_ast(ast_ty)));
                        }
                    }
                    AstGenericArg::Const(expr) => {
                        let value = self.evaluate_generic_const_arg(expr);
                        args.push(crate::GenericArg::Const(value));
                    }
                }
            }
        }
        crate::Substs::from_args(args)
    }

    pub(super) fn is_integer(&self, ty: Ty) -> bool {
        matches!(self.tcx.kind(ty), Some(TyKind::Int(_)))
    }

    /// The in-scope const generic parameter a generic argument names when the
    /// parser read it as a type: `N` in `Ring<N>` inside `impl<const N: usize>`.
    pub(super) fn const_generic_type_arg(&self, ast_ty: &AstType) -> Option<crate::ParamIdx> {
        let AstTypeKind::Path(path) = &ast_ty.kind else {
            return None;
        };
        let [segment] = path.segments.as_slice() else {
            return None;
        };
        if !segment.generics.is_empty()
            || !matches!(
                self.resolutions.get(ast_ty.id),
                Some(Resolution::Def {
                    kind: gossamer_resolve::DefKind::Const,
                    ..
                })
            )
        {
            return None;
        }
        self.current_const_generic_scope
            .get(&segment.name.name)
            .map(|(idx, _)| *idx)
    }

    /// A generic ADT's arguments as position-aligned type and const vectors,
    /// the shapes [`Self::subst_generics_in_ty`] takes. A const position
    /// holds a placeholder type, and a type position no const.
    pub(super) fn adt_subst_vectors(
        &mut self,
        substs: &crate::Substs,
    ) -> (Vec<Ty>, Vec<Option<i128>>) {
        let placeholder = self.tcx.error_ty();
        substs
            .as_slice()
            .iter()
            .map(|arg| match arg {
                crate::GenericArg::Type(ty) => (*ty, None),
                crate::GenericArg::Const(value) => (placeholder, Some(*value)),
                crate::GenericArg::ConstParam(_) => (placeholder, None),
            })
            .unzip()
    }

    /// The storage type of a field or payload declared as `ty`: an array whose
    /// length is a const parameter has no length until an instantiation
    /// supplies one, so it is held as the runtime-length slice a const generic
    /// body holds. The checker keeps the declared `[T; N]` for typing.
    pub(super) fn const_length_carrier(&mut self, ty: Ty) -> Ty {
        match self.tcx.kind_of(ty).clone() {
            TyKind::Array {
                elem,
                len: crate::ArrayLen::Param(_),
            }
            | TyKind::Simd {
                elem,
                lanes: crate::ArrayLen::Param(_),
            } => self.tcx.intern(TyKind::Slice(elem)),
            _ => ty,
        }
    }

    /// The const mask of the generic ADT `ty` names, or an empty one.
    pub(super) fn fn_generic_const_mask_of_ty(&self, ty: Ty) -> Vec<bool> {
        match self.tcx.kind(ty) {
            Some(TyKind::Adt { def, .. }) => self.fn_generic_const_mask_of(*def),
            _ => Vec::new(),
        }
    }

    /// The struct literal's type with each const position filled from what
    /// its array fields said, from the type the context expects, or reported
    /// as uninferred.
    pub(super) fn finish_struct_literal_consts(
        &mut self,
        struct_ty: Ty,
        consts: &LiteralConsts,
        expected: Expectation,
        path: &gossamer_ast::PathExpr,
        span: Span,
    ) -> Ty {
        let TyKind::Adt { def, substs } = self.tcx.kind_of(struct_ty).clone() else {
            return struct_ty;
        };
        let expected_args: Vec<crate::GenericArg> = self
            .expectation_target(expected)
            .map(|target| self.infer.resolve(self.tcx, target))
            .and_then(|target| match self.tcx.kind(target) {
                Some(TyKind::Adt {
                    def: expected_def,
                    substs,
                }) if *expected_def == def => Some(substs.as_slice().to_vec()),
                _ => None,
            })
            .unwrap_or_default();
        let mut args = Vec::with_capacity(substs.len());
        let mut missing = false;
        for (i, arg) in substs.as_slice().iter().enumerate() {
            if !consts.mask.get(i).copied().unwrap_or(false) {
                args.push(arg.clone());
                continue;
            }
            let filled = match (
                consts.values.get(i).copied().flatten(),
                consts.forwarded.get(i).copied().flatten(),
            ) {
                (Some(value), _) => Some(crate::GenericArg::Const(value)),
                (None, Some(param)) => Some(crate::GenericArg::ConstParam(param)),
                (None, None) => expected_args
                    .get(i)
                    .filter(|arg| {
                        matches!(
                            arg,
                            crate::GenericArg::Const(_) | crate::GenericArg::ConstParam(_)
                        )
                    })
                    .cloned(),
            };
            if let Some(arg) = filled {
                args.push(arg);
            } else {
                missing = true;
                args.push(crate::GenericArg::Const(0));
            }
        }
        if missing {
            let callee = path
                .segments
                .last()
                .map_or_else(String::new, |segment| segment.name.name.clone());
            let literal_ty = self
                .tcx
                .def_name(def)
                .and_then(|name| name.rsplit("::").next())
                .map(str::to_string);
            self.emit(
                TypeError::ConstGenericNotInferred {
                    callee,
                    literal_ty,
                    assoc_owner: None,
                },
                span,
            );
            // The value has no type to report against, so what it meets does
            // not report a second mismatch.
            return self.tcx.error_ty();
        }
        self.tcx.intern(TyKind::Adt {
            def,
            substs: crate::Substs::from_args(args),
        })
    }

    /// `Ring::capacity(r)` spells the method call `r.capacity()`, so it hands
    /// the method's `impl` block the same const arguments, read from the
    /// receiver argument when the path names that receiver's own type.
    pub(super) fn record_qualified_method_const_generic_args(
        &mut self,
        callee: &Expr,
        arg_tys: &[Ty],
    ) {
        let ExprKind::Path(path) = &callee.kind else {
            return;
        };
        let [.., owner_segment, method_segment] = path.segments.as_slice() else {
            return;
        };
        let Some(receiver_ty) = arg_tys.first().copied() else {
            return;
        };
        let resolved = self.peel_refs(self.infer.resolve(self.tcx, receiver_ty));
        let Some(TyKind::Adt { def, .. }) = self.tcx.kind(resolved) else {
            return;
        };
        let names_receiver_type = self
            .tcx
            .def_name(*def)
            .is_some_and(|name| name.rsplit("::").next() == Some(owner_segment.name.name.as_str()));
        if names_receiver_type {
            let method = method_segment.name.name.clone();
            self.record_method_const_generic_args(callee.id, receiver_ty, &method);
        }
    }

    /// Records the values a method call on a const generic type hands the
    /// method's `impl` block, read from the receiver's arguments at the
    /// positions the block's self type carries them at.
    pub(super) fn record_method_const_generic_args(
        &mut self,
        call_id: NodeId,
        receiver_ty: Ty,
        method: &str,
    ) {
        let resolved = self.peel_refs(self.infer.resolve(self.tcx, receiver_ty));
        let Some(TyKind::Adt { def, substs }) = self.tcx.kind(resolved).cloned() else {
            return;
        };
        let Some(owner) = self.tcx.def_name(def).map(ToString::to_string) else {
            return;
        };
        let Some(params) = self
            .const_generics
            .impl_method_params
            .get(&(owner, method.to_string()))
            .cloned()
        else {
            return;
        };
        let mut args = Vec::with_capacity(params.len());
        for (position, ty) in params {
            match substs.as_slice().get(position) {
                Some(crate::GenericArg::Const(value)) => {
                    args.push(crate::ConstGenericArg::Value { value: *value, ty });
                }
                Some(crate::GenericArg::ConstParam(idx)) => {
                    let Some(name) = self.const_generic_param_name(*idx) else {
                        return;
                    };
                    args.push(crate::ConstGenericArg::Param { name, ty });
                }
                _ => return,
            }
        }
        if !args.is_empty() {
            self.table.insert_const_generic_args(call_id, args);
        }
    }

    /// Which positions of generic function `def`'s parameter list are const
    /// parameters.
    pub(super) fn fn_generic_const_mask_of(&self, def: gossamer_resolve::DefId) -> Vec<bool> {
        self.const_generics
            .param_tys
            .get(&def)
            .map(|tys| tys.iter().map(Option::is_some).collect())
            .unwrap_or_default()
    }

    /// The const generic parameter in scope that `expr` names by itself, with
    /// its position and declared type.
    pub(super) fn const_generic_param(&self, expr: &Expr) -> Option<(crate::ParamIdx, Ty)> {
        let ExprKind::Path(path) = &expr.kind else {
            return None;
        };
        let [seg] = path.segments.as_slice() else {
            return None;
        };
        self.current_const_generic_scope
            .get(&seg.name.name)
            .copied()
    }

    /// The name of the const generic parameter in scope at position `idx`.
    pub(super) fn const_generic_param_name(&self, idx: crate::ParamIdx) -> Option<String> {
        self.current_const_generic_scope
            .iter()
            .find_map(|(name, (at, _))| (*at == idx).then(|| name.clone()))
    }

    /// Types an array-length expression. A literal yields a concrete
    /// count; a bare path naming a const generic parameter in scope
    /// yields a symbolic `Param` length linked to that parameter's
    /// position; anything else is rejected and uses `0` only as an error
    /// recovery placeholder.
    pub(super) fn array_len_from_ast(&mut self, expr: &Expr) -> crate::ArrayLen {
        if let Some(len) = self.evaluate_array_len(expr) {
            return crate::ArrayLen::Concrete(len);
        }
        if let Some((idx, _)) = self.const_generic_param(expr) {
            return crate::ArrayLen::Param(idx);
        }
        self.emit(TypeError::ArrayLengthNotConstant, expr.span);
        crate::ArrayLen::Concrete(0)
    }

    /// Evaluates an array-length expression to a `usize`, emitting a
    /// diagnostic when the literal magnitude exceeds `usize::MAX`.
    /// Returns `None` for non-literal forms.
    pub(super) fn evaluate_array_len(&mut self, expr: &Expr) -> Option<usize> {
        let raw = evaluate_const_int_from_expr(expr).or_else(|| self.const_int_of_path(expr))?;
        if raw > usize::MAX as u128 {
            self.emit(
                TypeError::IntLiteralOverflow {
                    literal: format!("{raw}"),
                    ty: "usize".to_string(),
                },
                expr.span,
            );
            return None;
        }
        Some(raw as usize)
    }

    /// Evaluates a `const` generic argument to an `i128`, emitting a
    /// diagnostic when the literal magnitude does not fit. Returns
    /// `0` on overflow so the surrounding `Substs` stays well-formed.
    pub(super) fn evaluate_generic_const_arg(&mut self, expr: &Expr) -> i128 {
        let Some(raw) = evaluate_const_int_from_expr(expr) else {
            return 0;
        };
        if let Ok(value) = i128::try_from(raw) {
            value
        } else {
            self.emit(
                TypeError::IntLiteralOverflow {
                    literal: format!("{raw}"),
                    ty: "i128".to_string(),
                },
                expr.span,
            );
            0
        }
    }

    /// The built-in `Result` / `Option` constructor named by an
    /// unqualified pattern path (`Ok` / `Err` / `Some` / `None`), or
    /// `None` for a qualified path or any other name. Qualified
    /// variants (`MyEnum::Ok`) keep their user typing - only the bare
    /// spelling is the reserved built-in.
    pub(super) fn bare_result_option_ctor(path: &gossamer_ast::Path) -> Option<&'static str> {
        if path.segments.len() != 1 {
            return None;
        }
        match path.segments.last()?.name.name.as_str() {
            "Ok" => Some("Ok"),
            "Err" => Some("Err"),
            "Some" => Some("Some"),
            "None" => Some("None"),
            _ => None,
        }
    }

    pub(super) fn type_of_pattern(&mut self, pattern: &Pattern) -> Ty {
        if self.enter_recursion(pattern.span).is_err() {
            return self.tcx.error_ty();
        }
        let ty = self.type_of_pattern_kind(pattern);
        self.leave_recursion();
        ty
    }

    pub(super) fn type_of_pattern_kind(&mut self, pattern: &Pattern) -> Ty {
        // A constructor pattern's type stays a fresh inference variable
        // here; the scrutinee unification binds it. Synthesizing the
        // `Result` / `Option` Adt at this site desugars `if let Some(n)
        // = m.get(k)` differently and miscompiles the payload binding,
        // so the bare `Ok` / `Err` / `Some` / `None` mismatch is caught
        // separately in `reject_constructor_scrutinee_mismatch`.
        match &pattern.kind {
            PatternKind::Wildcard
            | PatternKind::Ident { .. }
            | PatternKind::Path(_)
            | PatternKind::Struct { .. }
            | PatternKind::TupleStruct { .. }
            | PatternKind::Slice { .. }
            | PatternKind::Rest => self.fresh(),
            PatternKind::Error => self.tcx.error_ty(),
            PatternKind::Literal(lit) => self.type_of_literal(lit, pattern.span),
            // A `..` rest leaves the width open, so the tuple's type comes
            // from the scrutinee rather than from the pattern's arity.
            PatternKind::Tuple(parts)
                if parts
                    .iter()
                    .any(|part| matches!(part.kind, PatternKind::Rest)) =>
            {
                self.fresh()
            }
            PatternKind::Tuple(parts) => {
                let tys: Vec<Ty> = parts.iter().map(|p| self.type_of_pattern(p)).collect();
                self.tcx.intern(TyKind::Tuple(tys))
            }
            PatternKind::Range { lo, hi, .. } => match lo.as_ref().or(hi.as_ref()) {
                Some(bound) => self.type_of_literal(bound, pattern.span),
                None => self.fresh(),
            },
            PatternKind::Or(alts) => match alts.first() {
                Some(first) => self.type_of_pattern(first),
                None => self.fresh(),
            },
            PatternKind::Ref { inner, mutability } => {
                let inner_ty = self.type_of_pattern(inner);
                let mutability = match mutability {
                    gossamer_ast::Mutability::Immutable => Mutbl::Not,
                    gossamer_ast::Mutability::Mutable => Mutbl::Mut,
                };
                self.tcx.intern(TyKind::Ref {
                    mutability,
                    inner: inner_ty,
                })
            }
        }
    }

    pub(super) fn bind_pattern(&mut self, pattern: &Pattern, ty: Ty) {
        self.binding_types.insert(pattern.id, ty);
        self.table.insert(pattern.id, ty);
        match &pattern.kind {
            PatternKind::Ident {
                name,
                subpattern,
                mutability,
            } => {
                self.bind_local(&name.name, ty);
                self.bind_local_mutability(&name.name, mutability.is_mutable());
                if let Some(subpattern) = subpattern {
                    self.bind_pattern(subpattern, ty);
                }
            }
            PatternKind::Tuple(parts) => {
                self.bind_tuple_pattern(pattern, parts, ty);
            }
            PatternKind::Slice {
                prefix,
                rest,
                suffix,
            } => {
                let resolved = self.infer.resolve(self.tcx, ty);
                let elem_ty = match self.tcx.kind(resolved).cloned() {
                    Some(TyKind::Vec(e) | TyKind::Slice(e) | TyKind::Array { elem: e, .. }) => e,
                    _ => self.fresh(),
                };
                for part in prefix {
                    self.bind_pattern(part, elem_ty);
                }
                if let Some(rest) = rest {
                    let rest_ty = self.tcx.intern(TyKind::Vec(elem_ty));
                    self.bind_pattern(rest, rest_ty);
                }
                for part in suffix {
                    self.bind_pattern(part, elem_ty);
                }
            }
            PatternKind::Struct { path, fields, .. } => {
                self.bind_struct_pattern(path, fields, ty);
            }
            PatternKind::TupleStruct { path, elems } => {
                // For built-in generic enums (`Option<T>`,
                // `Result<T, E>`) pull the payload type out of the
                // scrutinee's substs and bind it to the matching
                // pattern element. Without this, `Some(x)` would
                // bind `x` to a fresh inference variable that no
                // later use forces back to `T`, leaving downstream
                // code looking at an unresolved `Var`. Falls back
                // to a fresh var for any other variant constructor.
                let payload_tys = self.payload_types_for_variant(path, ty);
                for (i, elem) in elems.iter().enumerate() {
                    let elem_ty = payload_tys
                        .as_ref()
                        .and_then(|tys| tys.get(i).copied())
                        .unwrap_or_else(|| self.fresh());
                    self.bind_pattern(elem, elem_ty);
                }
            }
            PatternKind::Or(alts) => {
                for alt in alts {
                    self.bind_pattern(alt, ty);
                }
                // Rust requires every alternative to bind a name with the
                // same mode. Until that receives its own diagnostic, use the
                // strict capability intersection: one immutable occurrence
                // keeps the resulting binding immutable regardless of order.
                let mut mutability = HashMap::new();
                for alt in alts {
                    Self::collect_pattern_binding_mutability(alt, &mut mutability);
                }
                for (name, mutable) in mutability {
                    self.bind_local_mutability(&name, mutable);
                }
            }
            PatternKind::Ref { inner, mutability } => {
                let resolved = self.infer.resolve(self.tcx, ty);
                let inner_ty = match self.tcx.kind(resolved).cloned() {
                    Some(TyKind::Ref { inner, .. }) => self.infer.resolve(self.tcx, inner),
                    // The pattern is what says the value is a reference, so
                    // an as-yet-unsolved type takes that shape from it.
                    Some(TyKind::Var(_)) => {
                        let referent = self.fresh();
                        let ref_ty = self.tcx.intern(TyKind::Ref {
                            mutability: if mutability.is_mutable() {
                                Mutbl::Mut
                            } else {
                                Mutbl::Not
                            },
                            inner: referent,
                        });
                        self.unify(resolved, ref_ty, pattern.span);
                        referent
                    }
                    _ => self.tcx.error_ty(),
                };
                self.bind_pattern(inner, inner_ty);
            }
            PatternKind::Wildcard
            | PatternKind::Literal(_)
            | PatternKind::Path(_)
            | PatternKind::Range { .. }
            | PatternKind::Rest
            | PatternKind::Error => {}
        }
    }

    pub(super) fn bind_tuple_pattern(&mut self, pattern: &Pattern, parts: &[Pattern], ty: Ty) {
        let mut resolved = self.infer.resolve(self.tcx, ty);
        let mut ref_mutability = None;
        if let Some(TyKind::Ref { inner, mutability }) = self.tcx.kind(resolved).cloned() {
            resolved = self.infer.resolve(self.tcx, inner);
            ref_mutability = Some(mutability);
        }
        if matches!(self.tcx.kind(resolved), Some(TyKind::Adt { .. })) {
            let ty = self.render_public_ty(resolved);
            self.emit(TypeError::StructPatternNameRequired { ty }, pattern.span);
        }
        let element_tys = self.tuple_pattern_element_tys(resolved, ref_mutability, parts.len());
        // A `..` rest spans the elements the written prefix and suffix leave
        // between them, so the suffix binds from the end of the tuple.
        let rest_at = parts
            .iter()
            .position(|part| matches!(part.kind, PatternKind::Rest));
        for (i, part) in parts.iter().enumerate() {
            let position = match rest_at {
                Some(at) if i > at => element_tys.len() + i - parts.len(),
                _ => i,
            };
            let elem_ty = element_tys
                .get(position)
                .copied()
                .unwrap_or_else(|| self.fresh());
            self.bind_pattern(part, elem_ty);
        }
    }

    pub(super) fn tuple_pattern_element_tys(
        &mut self,
        resolved: Ty,
        ref_mutability: Option<Mutbl>,
        count: usize,
    ) -> Vec<Ty> {
        match self.tcx.kind(resolved).cloned() {
            Some(TyKind::Tuple(elems)) => match ref_mutability {
                Some(mutability) => elems
                    .into_iter()
                    .map(|inner| self.tcx.intern(TyKind::Ref { mutability, inner }))
                    .collect(),
                None => elems,
            },
            // A sequence taken apart positionally binds each part to the
            // element type, exactly as a slice pattern does. Every part
            // otherwise takes a fresh variable that no later use resolves, so
            // the binding reaches codegen with no type to dispatch a method
            // against.
            Some(TyKind::Vec(elem) | TyKind::Slice(elem) | TyKind::Array { elem, .. }) => {
                let elem = match ref_mutability {
                    Some(mutability) => self.tcx.intern(TyKind::Ref {
                        mutability,
                        inner: elem,
                    }),
                    None => elem,
                };
                vec![elem; count]
            }
            _ => (0..count).map(|_| self.fresh()).collect(),
        }
    }

    pub(super) fn bind_struct_pattern(
        &mut self,
        path: &gossamer_ast::Path,
        fields: &[FieldPattern],
        ty: Ty,
    ) {
        let mut resolved = self.infer.resolve(self.tcx, ty);
        let mut ref_mutability = None;
        while let Some(TyKind::Ref { inner, mutability }) = self.tcx.kind(resolved).cloned() {
            resolved = self.infer.resolve(self.tcx, inner);
            ref_mutability = Some(mutability);
        }
        // A struct-variant pattern reads the enum's declared variant fields;
        // any other struct pattern reads the struct's own fields.
        let variant_fields = self.struct_variant_field_tys(path, resolved);
        let declared = variant_fields.unwrap_or_else(|| match self.tcx.kind(resolved).cloned() {
            Some(TyKind::Adt { def, substs }) => {
                let names = self.struct_fields.get(&def).cloned().unwrap_or_default();
                let tys = self.tcx.adt_field_tys(def, &substs).unwrap_or_default();
                names
                    .into_iter()
                    .zip(tys.iter().copied())
                    .map(|((name, _), ty)| (name, ty))
                    .collect::<HashMap<_, _>>()
            }
            _ => HashMap::new(),
        });
        for field in fields {
            let mut field_ty = declared
                .get(&field.name.name)
                .copied()
                .unwrap_or_else(|| self.fresh());
            if let Some(mutability) = ref_mutability {
                field_ty = self.reference_binding_ty(field_ty, mutability);
            }
            self.bind_field_pattern(field, field_ty);
        }
    }

    /// Declared field types of the enum struct-variant `path` names on the
    /// `resolved` scrutinee enum, or `None` when the pattern is a plain
    /// struct pattern. Generic enums are excluded: their stored field types
    /// mention un-substituted parameters.
    pub(super) fn struct_variant_field_tys(
        &mut self,
        path: &gossamer_ast::Path,
        resolved: Ty,
    ) -> Option<HashMap<String, Ty>> {
        let TyKind::Adt { def, substs } = self.tcx.kind(resolved)? else {
            return None;
        };
        if !substs.types().is_empty() {
            return None;
        }
        let enum_name = self.tcx.def_name(*def)?.to_string();
        let variant = path.segments.last()?.name.name.clone();
        let fields = self
            .enum_variant_named_payloads
            .get(&(enum_name, variant))?
            .clone();
        Some(fields.into_iter().collect())
    }

    /// Type a payload field binds at when it is reached through a reference
    /// scrutinee. A scalar copies through the borrow and binds by value; a
    /// heap-shaped payload binds as a borrow so the referent stays live.
    pub(super) fn reference_binding_ty(&mut self, inner: Ty, mutability: Mutbl) -> Ty {
        let scalar = matches!(
            self.tcx.kind(inner),
            Some(TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char)
        );
        if scalar {
            inner
        } else {
            self.tcx.intern(TyKind::Ref { inner, mutability })
        }
    }

    pub(super) fn bind_field_pattern(&mut self, field: &FieldPattern, ty: Ty) {
        if let Some(pattern) = &field.pattern {
            self.bind_pattern(pattern, ty);
        } else {
            self.bind_local(&field.name.name, ty);
            self.bind_local_mutability(&field.name.name, false);
        }
    }

    pub(super) fn collect_pattern_binding_mutability(
        pattern: &Pattern,
        out: &mut HashMap<String, bool>,
    ) {
        match &pattern.kind {
            PatternKind::Ident {
                name,
                subpattern,
                mutability,
            } => {
                out.entry(name.name.clone())
                    .and_modify(|current| *current &= mutability.is_mutable())
                    .or_insert_with(|| mutability.is_mutable());
                if let Some(subpattern) = subpattern {
                    Self::collect_pattern_binding_mutability(subpattern, out);
                }
            }
            PatternKind::Tuple(parts) | PatternKind::Or(parts) => {
                for part in parts {
                    Self::collect_pattern_binding_mutability(part, out);
                }
            }
            PatternKind::Slice {
                prefix,
                rest,
                suffix,
            } => {
                for part in prefix {
                    Self::collect_pattern_binding_mutability(part, out);
                }
                if let Some(rest) = rest {
                    Self::collect_pattern_binding_mutability(rest, out);
                }
                for part in suffix {
                    Self::collect_pattern_binding_mutability(part, out);
                }
            }
            PatternKind::Struct { fields, .. } => {
                for field in fields {
                    if let Some(pattern) = &field.pattern {
                        Self::collect_pattern_binding_mutability(pattern, out);
                    } else {
                        out.entry(field.name.name.clone())
                            .and_modify(|current| *current = false)
                            .or_insert(false);
                    }
                }
            }
            PatternKind::TupleStruct { elems, .. } => {
                for elem in elems {
                    Self::collect_pattern_binding_mutability(elem, out);
                }
            }
            PatternKind::Ref { inner, .. } => {
                Self::collect_pattern_binding_mutability(inner, out);
            }
            PatternKind::Wildcard
            | PatternKind::Literal(_)
            | PatternKind::Path(_)
            | PatternKind::Range { .. }
            | PatternKind::Rest
            | PatternKind::Error => {}
        }
    }

    /// Returns the payload tuple element types for a tuple-struct
    /// pattern when the scrutinee is `Option<T>` or `Result<T, E>`.
    /// Returns `None` for any other shape (user enums, unknown
    /// substs); callers fall back to fresh inference variables.
    pub(super) fn payload_types_for_variant(
        &mut self,
        path: &gossamer_ast::Path,
        scrutinee_ty: Ty,
    ) -> Option<Vec<Ty>> {
        let mut resolved = self.infer.resolve(self.tcx, scrutinee_ty);
        let mut ref_mutability = None;
        while let Some(TyKind::Ref { inner, mutability }) = self.tcx.kind(resolved) {
            ref_mutability = Some(*mutability);
            resolved = self.infer.resolve(self.tcx, *inner);
        }
        let TyKind::Adt { def, substs } = self.tcx.kind(resolved)? else {
            return None;
        };
        let last = path.segments.last()?.name.name.as_str();
        let args: Vec<Ty> = substs.types();
        match (last, args.as_slice()) {
            ("Some", [t]) => Some(vec![*t]),
            ("Ok", [t, _]) => Some(vec![*t]),
            ("Err", [_, e]) => Some(vec![*e]),
            _ => {
                // User enums: bind the declared tuple-variant payload
                // types, keyed by the scrutinee's own resolved enum name
                // so a same-named variant from another enum cannot unify
                // into this match. A generic enum's declared payloads carry
                // `Param` slots; the scrutinee's own arguments are what they
                // stand for here, so `Tree<i64>`'s `Leaf(v)` binds `v: i64`.
                let enum_name = self.tcx.def_name(*def)?;
                let tys = self
                    .enum_variant_payloads
                    .get(&(enum_name.to_string(), last.to_string()))
                    .cloned()?;
                let tys: Vec<Ty> = if args.is_empty() {
                    tys
                } else {
                    tys.iter()
                        .map(|t| self.subst_params_in_ty(*t, &args))
                        .collect()
                };
                // Match ergonomics: through a reference scrutinee a
                // heap-shaped payload binds as a borrow (a cursor walk's
                // `cursor = rest` with `cursor: &List` stays typed), while
                // a scalar payload copies through the borrow and binds by
                // value (`Tree::Node(v, ..) => v + 1`), matching how the
                // lowering loads payload words.
                Some(match ref_mutability {
                    Some(mutability) => tys
                        .into_iter()
                        .map(|inner| {
                            let scalar = matches!(
                                self.tcx.kind(inner),
                                Some(
                                    TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char
                                )
                            );
                            if scalar {
                                inner
                            } else {
                                self.tcx.intern(TyKind::Ref { inner, mutability })
                            }
                        })
                        .collect(),
                    None => tys,
                })
            }
        }
    }
}
