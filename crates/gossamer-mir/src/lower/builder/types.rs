#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::wildcard_imports)]
#![allow(clippy::similar_names)]
#![allow(clippy::many_single_char_names)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::option_if_let_else)]
#![allow(clippy::match_same_arms)]
#![allow(clippy::if_not_else)]
#![allow(clippy::single_match_else)]
#![allow(clippy::needless_pass_by_value)]
#![allow(clippy::manual_let_else)]
#![allow(clippy::redundant_else)]
#![allow(clippy::collapsible_if)]
#![allow(clippy::collapsible_else_if)]
#![allow(clippy::map_unwrap_or)]
#![allow(clippy::struct_excessive_bools)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::unnecessary_wraps)]
#![allow(clippy::large_enum_variant)]
#![allow(clippy::if_same_then_else)]
#![allow(clippy::single_match)]
#![allow(clippy::useless_conversion)]

use std::collections::HashMap;

use gossamer_ast::Ident;
use gossamer_hir::{
    HirAdtKind, HirBinaryOp, HirBlock, HirExpr, HirExprKind, HirFn, HirItem, HirItemKind,
    HirLiteral, HirMatchArm, HirPat, HirPatKind, HirProgram, HirStmt, HirStmtKind, HirUnaryOp,
};
use gossamer_lex::Span;
use gossamer_types::{Ty, TyCtxt};

use crate::ir::{
    BasicBlock, BinOp, BlockId, Body, ConstValue, Local, LocalDecl, Operand, Place, Rvalue,
    Statement, StatementKind, Terminator, UnOp,
};

use super::*;

use super::Builder;

impl<'a> Builder<'a> {
    pub(crate) fn new(
        _name: String,
        span: Span,
        tcx: &'a mut TyCtxt,
        structs: &'a HashMap<String, Vec<String>>,
        struct_defs: &'a HashMap<gossamer_resolve::DefId, String>,
        enums: &'a EnumIndex,
        impl_methods: &'a HashMap<String, Option<Ty>>,
        impl_method_receivers: &'a HashMap<String, Ty>,
        impl_method_inputs: &'a HashMap<String, Vec<Ty>>,
        fn_ret_names: &'a HashMap<String, Ty>,
        fn_returns: &'a HashMap<gossamer_resolve::DefId, Ty>,
        fn_inputs: &'a HashMap<gossamer_resolve::DefId, Vec<Ty>>,
        fn_param_shareable: &'a HashMap<gossamer_resolve::DefId, Vec<bool>>,
        consts: &'a HashMap<gossamer_resolve::DefId, ConstValue>,
        mut_statics: &'a HashMap<gossamer_resolve::DefId, crate::ir::StaticRef>,
        const_inits: &'a HashMap<gossamer_resolve::DefId, HirExpr>,
        region_unsafe: &'a std::collections::HashSet<gossamer_resolve::DefId>,
    ) -> Self {
        Self {
            tcx,
            locals: Vec::new(),
            blocks: Vec::new(),
            current: None,
            scopes: vec![HashMap::new()],
            named_locals: std::collections::HashSet::new(),
            reference_aliases: vec![HashMap::new()],
            fn_span: span,
            structs,
            struct_defs,
            enums,
            impl_methods,
            impl_method_receivers,
            impl_method_inputs,
            fn_ret_names,
            fn_returns,
            fn_inputs,
            fn_param_shareable,
            consts,
            mut_statics,
            const_inits,
            region_unsafe,
            local_struct: HashMap::new(),
            mut_receiver_reloads: HashMap::new(),
            slot_ref_locals: std::collections::HashSet::new(),
            local_elem_struct: HashMap::new(),
            local_closure: HashMap::new(),
            local_fn_name: HashMap::new(),
            local_runtime_kind: HashMap::new(),
            local_binary_heap_min_i64: std::collections::HashSet::new(),
            local_aggr_iter: std::collections::HashSet::new(),
            local_define_layout: HashMap::new(),
            param_locals: std::collections::HashSet::new(),
            loop_stack: Vec::new(),
            pending_loop_label: None,
            payload_defer_block: None,
            region_depth: 0,
            deferred_auto_region_collections: Vec::new(),
            defer_stack: Vec::new(),
        }
    }

    /// Replaces unresolved (`Var` / `Error`) fields of a tuple type
    /// with `i64` so the codegen reads each slot as an integer
    /// instead of defaulting to a pointer. A multi-slot tuple element
    /// read out of a `Vec` (`v[i].0`) inherits its field types from
    /// the element type the binding was pinned to; when that element
    /// came from an unannotated `let mut xs = []` the int-literal
    /// fields can still be `Var`, and a `Var` field lowers to a `ptr`
    /// load - so `v[i].0` is reinterpreted as a string pointer. This
    /// mirrors the i64 fallback the for-loop tuple-destructuring path
    /// already applies. Non-tuple types and tuples with all-concrete
    /// fields pass through unchanged.
    /// Ensures `ty` renders as the 2-word by-value `i128` Result/Option
    /// representation. A `gos_rt_result_new` result is ALWAYS an i128
    /// Result/Option, but type inference sometimes leaves the binding's type
    /// an unresolved `Var` (e.g. a combinator-chain intermediate), which would
    /// render as `ptr` and TRUNCATE the i128 on store. Returns `ty` unchanged
    /// when it is already a Result/Option Adt, else a canonical `Option<i64>`
    /// (same i128 representation).
    pub(crate) fn result_repr_ty(&mut self, ty: Ty) -> Ty {
        use gossamer_types::TyKind;
        if matches!(
            self.tcx.kind_of(ty),
            TyKind::Adt { def, .. } if def.local == u32::MAX || def.local == u32::MAX - 1
        ) {
            return ty;
        }
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let substs = gossamer_types::Substs::from_types([i64_ty]);
        self.tcx.intern(TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX - 1),
            substs,
        })
    }

    pub(crate) fn resolve_var_tuple_fields(&mut self, ty: Ty) -> Ty {
        use gossamer_types::TyKind;
        match self.tcx.kind_of(ty).clone() {
            TyKind::Tuple(fields) => {
                let needs_fix = fields
                    .iter()
                    .any(|f| matches!(self.tcx.kind_of(*f), TyKind::Var(_) | TyKind::Error));
                if !needs_fix {
                    return ty;
                }
                let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                let resolved: Vec<Ty> = fields
                    .iter()
                    .map(|f| {
                        if matches!(self.tcx.kind_of(*f), TyKind::Var(_) | TyKind::Error) {
                            i64_ty
                        } else {
                            *f
                        }
                    })
                    .collect();
                self.tcx.intern(TyKind::Tuple(resolved))
            }
            // A `[T; N]` element whose `T` is still an inference
            // variable lowers its `Index` reads as `ptr` loads, the
            // same failure mode as a `Var` tuple field. Pin it to i64.
            TyKind::Array { elem, len }
                if matches!(self.tcx.kind_of(elem), TyKind::Var(_) | TyKind::Error) =>
            {
                let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                self.tcx.intern(TyKind::Array { elem: i64_ty, len })
            }
            _ => ty,
        }
    }

    /// Name an `impl Trait for <primitive>` block registers its methods
    /// under, for a receiver whose type is one of those primitives.
    ///
    /// A primitive is not a struct, so it has no entry in `struct_defs`, but a
    /// user impl on it keys its methods by the spelling the impl was written
    /// with. The receiver of a `&self` method has to be borrowed at the call
    /// site the same way a struct receiver is; without a name to look up, the
    /// declaration is never consulted and the value travels where the body
    /// expects its address.
    pub(crate) fn primitive_impl_name(&self, ty: Ty) -> Option<String> {
        use gossamer_types::TyKind;
        let mut cur = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(cur) {
            cur = *inner;
        }
        matches!(
            self.tcx.kind_of(cur),
            TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char
        )
        .then(|| gossamer_types::printer::render_ty(self.tcx, cur))
    }

    /// Name an `impl` block registers its methods under, for a receiver whose
    /// type is a core one rather than a declared struct or enum.
    ///
    /// The spelling matches the checker's, which is what keeps one call
    /// reaching one body: the checker records the owner an `impl` block was
    /// written for, the bytecode VM registers the block under that name, and
    /// this is where the compiled tiers look it up.
    pub(crate) fn builtin_impl_owner_name(&self, ty: Ty) -> Option<String> {
        use gossamer_types::TyKind;
        if let Some(name) = self.primitive_impl_name(ty) {
            return Some(name);
        }
        let mut cur = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(cur) {
            cur = *inner;
        }
        match self.tcx.kind_of(cur) {
            TyKind::String => Some("String".to_string()),
            TyKind::Vec(_) => Some("Vec".to_string()),
            TyKind::HashMap { ordered, .. } => {
                Some(if *ordered { "BTreeMap" } else { "Map" }.to_string())
            }
            TyKind::Tuple(parts) => Some(format!("tuple_{}", parts.len())),
            _ => None,
        }
    }

    pub(crate) fn struct_name_of(&self, ty: Ty) -> Option<String> {
        use gossamer_types::TyKind;
        let mut cur = ty;
        loop {
            match self.tcx.kind_of(cur) {
                TyKind::Adt { def, .. } => {
                    if let Some(name) = self.struct_defs.get(def).cloned() {
                        return Some(name);
                    }
                    // Fallback: stdlib structs aren't user-
                    // declared so they don't appear in
                    // `struct_defs`, but their field layout
                    // lives in `stdlib_struct_shapes`. Match on
                    // the rendered type name (last `::`-segment).
                    let rendered = gossamer_types::printer::render_ty(self.tcx, cur);
                    let bare = rendered.rsplit("::").next().unwrap_or(&rendered);
                    if self.structs.contains_key(bare) {
                        return Some(bare.to_string());
                    }
                    return None;
                }
                TyKind::Ref { inner, .. } => cur = *inner,
                // The typechecker resolves stdlib types whose path
                // isn't declared in the resolver (e.g.
                // `&fs::DirInfo`) to `JsonValue` as a default. The
                // path information is lost in the typed `Ty`, but
                // the rendered form still reports the original
                // segment when the path matched a stdlib module
                // directly. Probe `stdlib_struct_shapes` against
                // the bare last segment to recover the layout.
                TyKind::JsonValue => {
                    let rendered = gossamer_types::printer::render_ty(self.tcx, cur);
                    let bare = rendered.rsplit("::").next().unwrap_or(&rendered);
                    if self.structs.contains_key(bare) {
                        return Some(bare.to_string());
                    }
                    return None;
                }
                _ => return None,
            }
        }
    }

    /// Bare type name of an Adt for method dispatch (`Type::method`), seeing
    /// through `&`. Unlike `struct_name_of` this also names user enums (which
    /// aren't in `struct_defs`); the caller gates on the mangled method
    /// actually existing in `impl_methods`, so naming a stdlib Adt is harmless.
    /// The `Ok` payload type `B` of a `Result<B, E>` (the result type of
    /// `x.try_into()`), so the call can route to `B::try_from(x)`.
    pub(crate) fn result_ok_ty(&self, ty: Ty) -> Option<Ty> {
        use gossamer_types::TyKind;
        if let TyKind::Adt { substs, .. } = self.tcx.kind_of(ty) {
            return substs.types().first().copied();
        }
        None
    }

    /// The enum a scrutinee's type names, as the key the enum index is
    /// registered under. A variant name may be declared by several enums, so
    /// match dispatch resolves through the value's own type rather than
    /// through the program-wide variant map.
    pub(crate) fn enum_index_name_of(&self, ty: Ty) -> Option<String> {
        use gossamer_types::TyKind;
        let mut cur = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(cur) {
            cur = *inner;
        }
        if !matches!(self.tcx.kind_of(cur), TyKind::Adt { .. }) {
            return None;
        }
        let rendered = gossamer_types::printer::render_ty(self.tcx, cur);
        let rendered = rendered.split('<').next().unwrap_or(&rendered).to_string();
        let bare = rendered
            .rsplit("::")
            .next()
            .unwrap_or(&rendered)
            .to_string();
        [rendered, bare]
            .into_iter()
            .find(|key| self.enums.by_enum.contains_key(key))
    }

    pub(crate) fn adt_dispatch_name(&self, ty: Ty) -> Option<String> {
        use gossamer_types::TyKind;
        if let Some(name) = self.struct_name_of(ty) {
            return Some(name);
        }
        let mut cur = ty;
        loop {
            match self.tcx.kind_of(cur) {
                TyKind::Adt { def, .. } => {
                    // A user type's registered name is its identity, which
                    // carries the modules containing it; impl methods hang
                    // off that same name.
                    if let Some(registered) = self.tcx.def_name(*def)
                        && !registered.starts_with("adt#")
                    {
                        return Some(registered.to_string());
                    }
                    let rendered = gossamer_types::printer::render_ty(self.tcx, cur);
                    let bare = rendered.rsplit("::").next().unwrap_or(&rendered);
                    // Impl methods register under the type's source name, so
                    // a generic instantiation drops its argument suffix
                    // (`Wrap<f64>` -> `Wrap`).
                    let bare = bare.split('<').next().unwrap_or(bare);
                    // `adt#N` is the debug placeholder for an Adt whose name the
                    // tcx never registered (user enums). Reject it so the caller
                    // falls back to the `local_struct` tag, which has the name.
                    if bare.starts_with("adt#") {
                        return None;
                    }
                    return Some(bare.to_string());
                }
                TyKind::Ref { inner, .. } => cur = *inner,
                // A sequence and a map are their own type kinds rather than
                // named Adts, so their source name is the one an `impl` for
                // them registers under.
                TyKind::Vec(_) | TyKind::Slice(_) => return Some("Vec".to_string()),
                TyKind::HashMap { ordered, .. } => {
                    return Some(if *ordered { "BTreeMap" } else { "Map" }.to_string());
                }
                _ => return None,
            }
        }
    }

    /// The name an `impl` for the container this local holds registers under.
    ///
    /// A `Set`, a `Deque`, a `Queue`, a `Stack`, and a heap reach codegen as
    /// bare `i64` handles, so their type says nothing about what they are and
    /// the construction tag is the only record.
    pub(crate) fn runtime_kind_dispatch_name(&self, local: Local) -> Option<String> {
        let name = match *self.local_runtime_kind.get(&local)? {
            "collections::HashSet" => "Set",
            "collections::BTreeSet" => "BTreeSet",
            "collections::VecDeque" => "Deque",
            "collections::VecQueue" => "Queue",
            "collections::VecStack" => "Stack",
            "collections::MaxHeap" => "MaxHeap",
            "collections::MinHeap" => "MinHeap",
            _ => return None,
        };
        Some(name.to_string())
    }

    /// Handler dispatch symbol for `fn_name`: the synthesized
    /// `::__ok_wrap` thunk when the callable declares a bare
    /// `http::Response` return (the HTTP runtime's C-ABI reads every
    /// handler return as a packed Result i128), else `fn_name` itself.
    pub(crate) fn handler_dispatch_symbol(&self, fn_name: String) -> String {
        use crate::lower::helpers::{handler_ok_wrap_name, is_bare_response_ty};
        match self.fn_ret_names.get(&fn_name) {
            Some(ret) if is_bare_response_ty(self.tcx, *ret) => handler_ok_wrap_name(&fn_name),
            _ => fn_name,
        }
    }

    pub(crate) fn router_bare_variant(symbol: &str) -> Option<&'static str> {
        match symbol {
            "gos_rt_router_get" => Some("gos_rt_router_get_fn"),
            "gos_rt_router_post" => Some("gos_rt_router_post_fn"),
            "gos_rt_router_put" => Some("gos_rt_router_put_fn"),
            "gos_rt_router_delete" => Some("gos_rt_router_delete_fn"),
            "gos_rt_router_patch" => Some("gos_rt_router_patch_fn"),
            "gos_rt_router_head" => Some("gos_rt_router_head_fn"),
            "gos_rt_router_options" => Some("gos_rt_router_options_fn"),
            "gos_rt_router_add" => Some("gos_rt_router_add_fn"),
            _ => None,
        }
    }

    pub(crate) fn emit_router_handler_abi(
        &mut self,
        handler_local: Local,
        span: Span,
    ) -> RouterHandlerAbi {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        if let Some(fn_name) = self.local_fn_name.get(&handler_local).cloned() {
            let fn_name = self.handler_dispatch_symbol(fn_name);
            let fn_addr_local = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(fn_addr_local),
                Rvalue::CallIntrinsic {
                    name: "gos_fn_addr",
                    args: vec![Operand::Const(ConstValue::Str(fn_name))],
                },
                span,
            );
            return RouterHandlerAbi::Bare(Operand::Copy(Place::local(fn_addr_local)));
        }
        // A closure handler carries its lifted body as the dispatch symbol
        // and its captured environment as the first argument, the same
        // shape a `serve` method has.
        if let Some(closure_name) = self.local_closure.get(&handler_local).cloned() {
            let fn_name = self.handler_dispatch_symbol(closure_name);
            let fn_addr_local = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(fn_addr_local),
                Rvalue::CallIntrinsic {
                    name: "gos_fn_addr",
                    args: vec![Operand::Const(ConstValue::Str(fn_name))],
                },
                span,
            );
            return RouterHandlerAbi::WithEnv {
                env: Operand::Copy(Place::local(handler_local)),
                fn_addr: Operand::Copy(Place::local(fn_addr_local)),
            };
        }
        let handler_ty = self.locals[handler_local.0 as usize].ty;
        let handler_struct = self
            .struct_name_of(handler_ty)
            .unwrap_or_else(|| "Handler".to_string());
        let serve_fn_name = self.handler_dispatch_symbol(format!("{handler_struct}::serve"));
        let fn_addr_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(fn_addr_local),
            Rvalue::CallIntrinsic {
                name: "gos_fn_addr",
                args: vec![Operand::Const(ConstValue::Str(serve_fn_name))],
            },
            span,
        );
        RouterHandlerAbi::WithEnv {
            env: Operand::Copy(Place::local(handler_local)),
            fn_addr: Operand::Copy(Place::local(fn_addr_local)),
        }
    }

    pub(crate) fn lookup_define_field(
        &mut self,
        receiver_local: Local,
        long_name: &str,
        span: Span,
    ) -> Option<Local> {
        let layout = self.local_define_layout.get(&receiver_local)?.clone();
        let (idx, &(_, cell_kind)) = layout
            .iter()
            .enumerate()
            .find(|(_, (n, _))| n == long_name)?;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let dest = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(dest),
            Rvalue::Use(Operand::Copy(Place {
                local: receiver_local,
                projection: vec![crate::Projection::Field(idx as u32)],
            })),
            span,
        );
        self.local_runtime_kind.insert(dest, cell_kind);
        Some(dest)
    }

    pub(crate) fn is_json_value_ty(&self, ty: Ty) -> bool {
        use gossamer_types::TyKind;
        let mut cur = ty;
        loop {
            match self.tcx.kind_of(cur) {
                TyKind::JsonValue => return true,
                TyKind::Ref { inner, .. } => cur = *inner,
                _ => return false,
            }
        }
    }

    /// The declared value type of a map, seeing through a leading `&`.
    pub(crate) fn hash_map_value_ty(&self, ty: Ty) -> Option<Ty> {
        use gossamer_types::TyKind;
        let mut cur = ty;
        loop {
            match self.tcx.kind_of(cur) {
                TyKind::Ref { inner, .. } => cur = *inner,
                TyKind::HashMap { value, .. } => return Some(*value),
                _ => return None,
            }
        }
    }

    pub(crate) fn hash_map_value_kind(&self, ty: Ty) -> Option<MapValueKind> {
        use gossamer_types::TyKind;
        let mut cur = ty;
        loop {
            match self.tcx.kind_of(cur) {
                TyKind::Ref { inner, .. } => cur = *inner,
                TyKind::HashMap { value, .. } => {
                    return Some(map_value_kind_from(self.tcx, *value));
                }
                _ => return None,
            }
        }
    }

    /// The typed `gos_rt_map_insert_*_opt` entry point for a map of type
    /// `map_ty`, whose key is one of the scalar / `String` fast paths.
    ///
    /// The value picks the storage width - an aggregate value crosses as an
    /// 8-byte handle word, so it shares the `i64` entry points - and the key
    /// then picks between the integer and string spelling, because a `String`
    /// key must reach the string path whatever the value does.
    pub(crate) fn map_insert_helper(&self, map_ty: Ty) -> &'static str {
        let string_key = matches!(self.hash_map_key_kind(map_ty), Some(MapKeyKind::String));
        match self.hash_map_value_kind(map_ty) {
            Some(MapValueKind::String) if string_key => "gos_rt_map_insert_str_str_opt",
            Some(MapValueKind::String) => "gos_rt_map_insert_i64_str_opt",
            _ if string_key => "gos_rt_map_insert_typed_str_i64_opt",
            _ => "gos_rt_map_insert_i64_i64_opt",
        }
    }

    /// `(K, V)` of a `HashMap<K, V>` (seeing through a leading `&`).
    pub(crate) fn hash_map_kv_tys(&self, ty: Ty) -> Option<(Ty, Ty)> {
        use gossamer_types::TyKind;
        let mut cur = ty;
        loop {
            match self.tcx.kind_of(cur) {
                TyKind::Ref { inner, .. } => cur = *inner,
                TyKind::HashMap { key, value, .. } => return Some((*key, *value)),
                _ => return None,
            }
        }
    }

    /// True when `ty` (through a leading `&`) is a struct or tuple - the only
    /// shapes that route through the content-hashing map key path. Bare
    /// scalars / `String` / enums keep their own paths.
    pub(crate) fn is_aggregate_key(&self, ty: Ty) -> bool {
        use gossamer_types::TyKind;
        let mut cur = ty;
        loop {
            match self.tcx.kind_of(cur) {
                TyKind::Ref { inner, .. } => cur = *inner,
                TyKind::Tuple(_) | TyKind::Array { .. } => return true,
                TyKind::Vec(_) | TyKind::Slice(_) => return self.key_descriptor(cur).is_some(),
                // An `Option<T>` is keyed by its two words, exactly as a
                // two-element tuple is.
                TyKind::Adt { def, .. } if def.local == u32::MAX - 1 => {
                    return self.key_descriptor(cur).is_some();
                }
                TyKind::Adt { .. } => return self.struct_name_of(cur).is_some(),
                _ => return false,
            }
        }
    }

    /// Per-slot layout descriptor for a struct / tuple used as a map key, or
    /// `None` if the type can't be content-keyed. Each character describes one
    /// 8-byte slot of the aggregate's flat buffer: `'s'` = scalar (read
    /// inline), `'S'` = `String` pointer (dereferenced and folded by content).
    /// Nested all-scalar / String structs inline their slots, so the
    /// descriptor flattens them. `Vec` / nested-enum fields aren't keyable.
    pub(crate) fn key_descriptor(&self, ty: Ty) -> Option<String> {
        let mut out = String::new();
        if self.append_key_descriptor(ty, &mut out) {
            Some(out)
        } else {
            None
        }
    }

    fn append_key_descriptor(&self, ty: Ty, out: &mut String) -> bool {
        use gossamer_types::TyKind;
        match self.tcx.kind_of(ty) {
            TyKind::Ref { inner, .. } => self.append_key_descriptor(*inner, out),
            TyKind::Int(_) | TyKind::Bool | TyKind::Char | TyKind::Float(_) => {
                out.push('s');
                true
            }
            TyKind::String => {
                out.push('S');
                true
            }
            // A sequence of one-word scalars folds by content. Its elements
            // must be readable as raw bytes, so a sequence of strings or of
            // aggregates is not keyable.
            TyKind::Vec(elem) | TyKind::Slice(elem) => {
                let elem = *elem;
                let scalar = matches!(
                    self.tcx.kind_of(elem),
                    TyKind::Int(_) | TyKind::Bool | TyKind::Char | TyKind::Float(_)
                );
                if scalar {
                    out.push('V');
                }
                scalar
            }
            TyKind::Tuple(elems) => {
                let elems = elems.clone();
                !elems.is_empty() && elems.iter().all(|e| self.append_key_descriptor(*e, out))
            }
            // A fixed array is N inline copies of its element, so its slots
            // are the element's descriptor repeated. A const-generic length
            // is concrete by the time codegen runs.
            TyKind::Array { elem, len } => {
                let elem = *elem;
                let gossamer_types::ArrayLen::Concrete(len) = *len else {
                    return false;
                };
                len > 0 && (0..len).all(|_| self.append_key_descriptor(elem, out))
            }
            // An `Option<T>` key is its two-word carrier: the discriminant,
            // then the payload the `Some` arm holds. Both slots are the key's
            // own content, so the same descriptor that spells a tuple spells
            // this.
            TyKind::Adt { def, substs } if def.local == u32::MAX - 1 => {
                let substs = substs.clone();
                out.push('s');
                substs
                    .types()
                    .first()
                    .is_some_and(|payload| self.append_key_descriptor(*payload, out))
            }
            TyKind::Adt { def, substs } => {
                if self.struct_name_of(ty).is_none() {
                    return false;
                }
                let fields = self.tcx.adt_field_tys(*def, substs).map(<[Ty]>::to_vec);
                // A field-less struct contributes no slots: every value of it
                // holds the same (empty) content, so they all key one entry.
                match fields {
                    Some(fields) => fields.iter().all(|f| self.append_key_descriptor(*f, out)),
                    None => false,
                }
            }
            _ => false,
        }
    }

    pub(crate) fn hash_map_key_kind(&self, ty: Ty) -> Option<MapKeyKind> {
        use gossamer_types::TyKind;
        let mut cur = ty;
        loop {
            match self.tcx.kind_of(cur) {
                TyKind::Ref { inner, .. } => cur = *inner,
                TyKind::HashMap { key, .. } => {
                    return Some(map_key_kind_from(self.tcx, *key));
                }
                _ => return None,
            }
        }
    }

    /// Element kind (`I64`-like vs `String`) of a `HashSet<T>` receiver.
    /// The set's MIR handle type is erased to a pointer-sized `i64`, so the
    /// element type is recovered from the HIR receiver-expression type
    /// (which still carries the generic), defaulting to `String` when it
    /// cannot be resolved.
    pub(crate) fn set_elem_kind_of(&self, receiver: &HirExpr) -> MapKeyKind {
        match self.first_generic_of(receiver.ty) {
            Some(t) => map_key_kind_from(self.tcx, self.peel_ref_ty(t)),
            None => MapKeyKind::String,
        }
    }

    /// Strips any leading `&T` / `&mut T` layers from `ty`, returning the
    /// referent. Used where dispatch keys off a value's element kind and a
    /// borrowed argument (`set.contains(&k)`) must classify like the owned
    /// value.
    pub(crate) fn peel_ref_ty(&self, ty: Ty) -> Ty {
        use gossamer_types::TyKind;
        let mut cur = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(cur) {
            cur = *inner;
        }
        cur
    }

    /// `true` when a reference to `ty` is the address of a slot rather than
    /// the value itself. A scalar has no runtime handle of its own, so both
    /// `&T` and `&mut T` over one carry the place's address and a read
    /// through the reference loads from it.
    pub(crate) fn slot_addressed_pointee(&self, ty: Ty) -> bool {
        use gossamer_types::TyKind;
        matches!(
            self.tcx.kind_of(ty),
            TyKind::Int(_)
                | TyKind::Float(_)
                | TyKind::Bool
                | TyKind::Char
                | TyKind::Duration
                | TyKind::Instant
        )
    }

    /// Reads the value a slot-addressed reference points at.
    pub(crate) fn load_slot_value(&mut self, local: Local, pointee: Ty, span: Span) -> Local {
        let dest = self.fresh(pointee);
        self.emit_assign(
            Place::local(dest),
            Rvalue::CallIntrinsic {
                name: "gos_load",
                args: vec![
                    Operand::Copy(Place::local(local)),
                    Operand::Const(ConstValue::Int(0)),
                ],
            },
            span,
        );
        dest
    }

    /// Returns `true` when `&mut <place>` over an operand of type
    /// `operand_ty`, materialised in a local of type `local_ty`, must take
    /// the place's slot address rather than pass its value. Scalars,
    /// `String`, and the `Option` / `Result` carriers are the shapes a
    /// callee rebinds wholesale through the reference, so the caller hands
    /// over the slot and reloads from it after the call.
    ///
    /// The local's type answers only for an operand the checker left
    /// unresolved: a handle-backed container (`Set`, `Deque`, `Stack`, an
    /// opaque runtime handle) lives in an `i64`-shaped local, and reading
    /// that local as the operand's type would take the address of a handle
    /// slot and hand the callee a pointer where it expects the handle.
    /// True for a value whose storage is the slots themselves - a user
    /// struct, a tuple, a fixed array - so a place of this type is read by
    /// copying it and a reference to it must be the place's address.
    pub(crate) fn inline_aggregate_ty(&self, ty: Ty) -> bool {
        use gossamer_types::TyKind;
        match self.tcx.kind_of(ty) {
            TyKind::Tuple(_) | TyKind::Array { .. } => true,
            TyKind::Adt { .. } => {
                !self.tcx.is_rc_managed(ty)
                    && !self.tcx.is_inline_enum_ty(ty)
                    && self.struct_name_of(ty).is_some()
            }
            _ => false,
        }
    }

    pub(crate) fn mut_ref_takes_slot_address(&self, operand_ty: Ty, local_ty: Ty) -> bool {
        use gossamer_types::TyKind;
        let rebindable = |ty: Ty| {
            match self.tcx.kind_of(ty) {
                TyKind::Int(_)
                | TyKind::Float(_)
                | TyKind::Bool
                | TyKind::Char
                | TyKind::String
                | TyKind::Duration
                | TyKind::Instant => true,
                // An `Option` / `Result` is a carrier the callee replaces
                // whole (`*o = Some(v)`), so the reference has to name the
                // caller's slot rather than a copy of the carrier. A payload
                // enum is the same shape: `*e = Variant(..)` names a new node,
                // and the caller's binding has to end up holding that node.
                TyKind::Adt { def, .. } => {
                    def.local == u32::MAX
                        || def.local == u32::MAX - 1
                        || self.tcx.is_payload_enum(ty)
                }
                _ => false,
            }
        };
        if matches!(self.tcx.kind_of(operand_ty), TyKind::Var(_)) {
            rebindable(local_ty)
        } else {
            rebindable(operand_ty)
        }
    }

    /// Element type of a `Vec<T>` / `[T]` / `[T; N]` receiver, peeling any
    /// leading references. `None` when `ty` is not a sequence.
    pub(crate) fn seq_elem_of(&self, ty: Ty) -> Option<Ty> {
        use gossamer_types::TyKind;
        let mut cur = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(cur) {
            cur = *inner;
        }
        match self.tcx.kind_of(cur) {
            TyKind::Vec(e) | TyKind::Slice(e) => Some(*e),
            TyKind::Array { elem, .. } => Some(*elem),
            // Iterator state yields the sequence's element, so a consumer
            // recovering an element type from its receiver reaches the same
            // answer whether it holds the sequence or a walk over it.
            TyKind::Iterator(e) | TyKind::Range(e) => Some(*e),
            _ => None,
        }
    }

    pub(crate) fn first_generic_of(&self, ty: Ty) -> Option<Ty> {
        use gossamer_types::{GenericArg, TyKind};
        let mut cur = ty;
        loop {
            match self.tcx.kind_of(cur) {
                TyKind::Ref { inner, .. } => cur = *inner,
                TyKind::Adt { substs, .. } => {
                    for arg in substs.as_slice() {
                        if let GenericArg::Type(t) = arg {
                            return Some(*t);
                        }
                    }
                    return None;
                }
                _ => return None,
            }
        }
    }

    pub(crate) fn second_generic_of(&self, ty: Ty) -> Option<Ty> {
        use gossamer_types::{GenericArg, TyKind};
        let mut cur = ty;
        loop {
            match self.tcx.kind_of(cur) {
                TyKind::Ref { inner, .. } => cur = *inner,
                TyKind::Adt { substs, .. } => {
                    let types: Vec<Ty> = substs
                        .as_slice()
                        .iter()
                        .filter_map(|arg| match arg {
                            GenericArg::Type(t) => Some(*t),
                            GenericArg::Const(_) => None,
                        })
                        .collect();
                    return types.get(1).copied();
                }
                _ => return None,
            }
        }
    }

    pub(crate) fn option_adt_ty(&mut self) -> Ty {
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX - 1),
            substs: gossamer_types::Substs::new(),
        })
    }

    pub(crate) fn option_tuple3_i64_i64_str_ty(&mut self) -> Ty {
        let i = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let s = self.tcx.string_ty();
        let tup = self
            .tcx
            .intern(gossamer_types::TyKind::Tuple(vec![i, i, s]));
        let substs = gossamer_types::Substs::from_types([tup]);
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX - 1),
            substs,
        })
    }

    pub(crate) fn option_string_ty(&mut self) -> Ty {
        let s = self.tcx.string_ty();
        let substs = gossamer_types::Substs::from_types([s]);
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX - 1),
            substs,
        })
    }

    pub(crate) fn option_vec_option_string_ty(&mut self) -> Ty {
        let opt_s = self.option_string_ty();
        let v = self.tcx.intern(gossamer_types::TyKind::Vec(opt_s));
        let substs = gossamer_types::Substs::from_types([v]);
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX - 1),
            substs,
        })
    }

    pub(crate) fn option_vec_u8_ty(&mut self) -> Ty {
        let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
        let v = self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty));
        let substs = gossamer_types::Substs::from_types([v]);
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX - 1),
            substs,
        })
    }

    /// Static MIR type of a field on an opaque runtime-kind struct
    /// (`http::Request` / `http::Response` / `errors::Error`). The
    /// checker leaves these fields as inference Vars (the structs are
    /// checker-opaque; `Request` cannot be name-pinned because the
    /// legacy client builder struct shares the name), so method
    /// dispatch on a field expression must consult the same table the
    /// `lower_field_access` accessors use or `.len()` on `r.query`
    /// lands on the len-prefixed reader and dereferences a c-string.
    pub(crate) fn runtime_field_static_ty(&mut self, kind: &str, field: &str) -> Option<Ty> {
        let str_pair_vec = |b: &mut Self| {
            let s = b.tcx.string_ty();
            let tup = b.tcx.intern(gossamer_types::TyKind::Tuple(vec![s, s]));
            b.tcx.intern(gossamer_types::TyKind::Vec(tup))
        };
        let u8_vec = |b: &mut Self| {
            let u8_ty = b.tcx.int_ty(gossamer_types::IntTy::U8);
            b.tcx.intern(gossamer_types::TyKind::Vec(u8_ty))
        };
        match (kind, field) {
            ("http::Request", "method" | "path" | "query" | "body")
            | ("http::Response", "body" | "content_type" | "location")
            | ("errors::Error", "message") => Some(self.tcx.string_ty()),
            ("http::Request" | "http::Response", "headers") => Some(str_pair_vec(self)),
            ("http::Request", "raw_body") | ("http::Response", "raw_bytes") => Some(u8_vec(self)),
            ("http::Response", "status") => Some(self.tcx.int_ty(gossamer_types::IntTy::I64)),
            _ => None,
        }
    }

    /// `Vec<(String, String)>` - ordered string key/value pairs, the
    /// shape of `http::Request`/`Response` headers and the
    /// `http::cookie::parse_cookie_header` result.
    pub(crate) fn string_pair_vec_ty(&mut self) -> Ty {
        let s = self.tcx.string_ty();
        let tup = self.tcx.intern(gossamer_types::TyKind::Tuple(vec![s, s]));
        self.tcx.intern(gossamer_types::TyKind::Vec(tup))
    }

    pub(crate) fn option_vec_string_ty(&mut self) -> Ty {
        let s = self.tcx.string_ty();
        let v = self.tcx.intern(gossamer_types::TyKind::Vec(s));
        let substs = gossamer_types::Substs::from_types([v]);
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX - 1),
            substs,
        })
    }

    pub(crate) fn option_string_adt_ty(&mut self) -> Ty {
        let s = self.tcx.string_ty();
        let substs = gossamer_types::Substs::from_types([s]);
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX - 1),
            substs,
        })
    }

    /// `Option<(String, String)>` sentinel Adt - the packed shape the
    /// `gos_rt_http_request_basic_auth` / `decode_basic_auth` /
    /// `gos_rt_str_split_once` family return.
    pub(crate) fn option_pair_string_adt_ty(&mut self) -> Ty {
        let s = self.tcx.string_ty();
        let tup = self.tcx.intern(gossamer_types::TyKind::Tuple(vec![s, s]));
        let substs = gossamer_types::Substs::from_types([tup]);
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX - 1),
            substs,
        })
    }

    pub(crate) fn option_i64_adt_ty(&mut self) -> Ty {
        let i = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let substs = gossamer_types::Substs::from_types([i]);
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX - 1),
            substs,
        })
    }

    pub(crate) fn option_f64_adt_ty(&mut self) -> Ty {
        let f = self.tcx.float_ty(gossamer_types::FloatTy::F64);
        let substs = gossamer_types::Substs::from_types([f]);
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX - 1),
            substs,
        })
    }

    pub(crate) fn option_bool_adt_ty(&mut self) -> Ty {
        let b = self.tcx.bool_ty();
        let substs = gossamer_types::Substs::from_types([b]);
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX - 1),
            substs,
        })
    }

    /// `Result<http::Response, errors::Error>` with the Ok payload
    /// pinned to the sentinel Response Adt (`u32::MAX - 5`) so field
    /// projections resolve via `stdlib_struct_shapes`. Same shape as
    /// the `http::get` / `http::request` free-call destinations.
    pub(crate) fn result_response_error_adt_ty(&mut self) -> Ty {
        let resp = self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX - 5),
            substs: gossamer_types::Substs::new(),
        });
        let e = self.tcx.dyn_error_ty();
        let substs = gossamer_types::Substs::from_types([resp, e]);
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX),
            substs,
        })
    }

    pub(crate) fn result_string_error_adt_ty(&mut self) -> Ty {
        let s = self.tcx.string_ty();
        let e = self.tcx.dyn_error_ty();
        let substs = gossamer_types::Substs::from_types([s, e]);
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX),
            substs,
        })
    }

    pub(crate) fn result_vec_u8_error_ty(&mut self) -> Ty {
        let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
        let vec = self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty));
        let e = self.tcx.dyn_error_ty();
        let substs = gossamer_types::Substs::from_types([vec, e]);
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX),
            substs,
        })
    }

    /// `Result<inner, errors::Error>` for an arbitrary Ok type.
    pub(crate) fn result_of(&mut self, inner: Ty) -> Ty {
        let e = self.tcx.dyn_error_ty();
        let substs = gossamer_types::Substs::from_types([inner, e]);
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX),
            substs,
        })
    }

    /// `Result<ok, err>` for an arbitrary error type - the shape
    /// `binary_search` answers, whose `Err` carries an insertion index
    /// rather than a diagnostic.
    pub(crate) fn result_adt_of(&mut self, ok: Ty, err: Ty) -> Ty {
        let substs = gossamer_types::Substs::from_types([ok, err]);
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX),
            substs,
        })
    }

    /// The x509 `CertInfo` leaf tuple: `(String, String, [u8], i64,
    /// i64, [String], [u8])`.
    pub(crate) fn tuple_cert_info_ty(&mut self) -> Ty {
        let s = self.tcx.string_ty();
        let i = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
        let vec_u8 = self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty));
        let vec_str = self.tcx.intern(gossamer_types::TyKind::Vec(s));
        self.tcx.intern(gossamer_types::TyKind::Tuple(vec![
            s, s, vec_u8, i, i, vec_str, vec_u8,
        ]))
    }

    /// The `fs::metadata` leaf tuple `(size: i64, is_file: bool,
    /// is_dir: bool, is_symlink: bool, readonly: bool,
    /// modified_unix_ms: i64)`. Field order matches the VM's
    /// `fs::Metadata` struct.
    pub(crate) fn tuple_fs_metadata_ty(&mut self) -> Ty {
        let i = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let b = self.tcx.bool_ty();
        self.tcx
            .intern(gossamer_types::TyKind::Tuple(vec![i, b, b, b, b, i]))
    }

    /// The archive entry leaf tuple `(String, [u8], bool)`.
    pub(crate) fn tuple_entry_ty(&mut self) -> Ty {
        let s = self.tcx.string_ty();
        let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
        let vec_u8 = self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty));
        let b = self.tcx.bool_ty();
        self.tcx
            .intern(gossamer_types::TyKind::Tuple(vec![s, vec_u8, b]))
    }

    /// The tuple type `(String, [u8])`.
    pub(crate) fn tuple_str_bytes_ty(&mut self) -> Ty {
        let s = self.tcx.string_ty();
        let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
        let vec_u8 = self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty));
        self.tcx
            .intern(gossamer_types::TyKind::Tuple(vec![s, vec_u8]))
    }

    pub(crate) fn result_pair_i64_error_ty(&mut self) -> Ty {
        let i = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let tup = self.tcx.intern(gossamer_types::TyKind::Tuple(vec![i, i]));
        let e = self.tcx.dyn_error_ty();
        let substs = gossamer_types::Substs::from_types([tup, e]);
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX),
            substs,
        })
    }

    pub(crate) fn result_vec_vec_string_error_ty(&mut self) -> Ty {
        let s = self.tcx.string_ty();
        let inner = self.tcx.intern(gossamer_types::TyKind::Vec(s));
        let outer = self.tcx.intern(gossamer_types::TyKind::Vec(inner));
        let e = self.tcx.dyn_error_ty();
        let substs = gossamer_types::Substs::from_types([outer, e]);
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX),
            substs,
        })
    }

    pub(crate) fn result_vec_string_error_ty(&mut self) -> Ty {
        let s = self.tcx.string_ty();
        let vec = self.tcx.intern(gossamer_types::TyKind::Vec(s));
        let e = self.tcx.dyn_error_ty();
        let substs = gossamer_types::Substs::from_types([vec, e]);
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX),
            substs,
        })
    }

    pub(crate) fn flag_set_ty(&mut self) -> Ty {
        let def = gossamer_resolve::DefId::local(u32::MAX - 21);
        self.tcx.register_def_name(def, "flag::Set");
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def,
            substs: gossamer_types::Substs::new(),
        })
    }

    /// Element type of a `Vec<T>` / `[T]` receiver (peeling a leading
    /// `&` borrow), falling back to `i64` when the receiver is not a
    /// vec/slice. Lets the safe Vec helpers (`slice` / `insert` /
    /// `remove`) carry the receiver's real element type into their
    /// `Result` so a `Vec<String>` result indexes as strings rather than
    /// reading the heap pointer back as an i64.
    pub(crate) fn vec_receiver_elem_ty(&mut self, recv: Ty) -> Ty {
        use gossamer_types::TyKind;
        let mut t = recv;
        if let TyKind::Ref { inner, .. } = self.tcx.kind_of(t) {
            t = *inner;
        }
        match self.tcx.kind_of(t) {
            TyKind::Vec(e) | TyKind::Slice(e) => *e,
            _ => self.tcx.int_ty(gossamer_types::IntTy::I64),
        }
    }

    /// True when a value of `ty` is a block of flat slots addressed in
    /// place - a struct, tuple, or fixed array - rather than a single word.
    ///
    /// A scalar, a `String`, an `Option` / `Result` carrier, an inline enum,
    /// and every opaque runtime handle (`Map`, `Set`, `fs::File`, a socket)
    /// are one word each. A handle's Adt carries no field table, which is
    /// what separates it from a struct here.
    pub(crate) fn is_inline_slot_block(&self, ty: Ty) -> bool {
        use gossamer_types::TyKind;
        let mut t = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(t) {
            t = *inner;
        }
        match self.tcx.kind_of(t) {
            TyKind::Tuple(_) | TyKind::Array { .. } => true,
            TyKind::Adt { def, .. } => {
                def.local != u32::MAX
                    && def.local != u32::MAX - 1
                    && !self.tcx.is_inline_enum_ty(t)
                    && self.tcx.struct_field_tys(*def).is_some()
            }
            _ => false,
        }
    }

    /// `Result<Vec<json::Value>, errors::Error>` - the shape
    /// `gos_rt_yaml_parse_all` returns (one `json::Value` handle per
    /// document in a multi-document YAML stream).
    pub(crate) fn result_vec_json_value_error_ty(&mut self) -> Ty {
        let jv = self.tcx.json_value_ty();
        let vec = self.tcx.intern(gossamer_types::TyKind::Vec(jv));
        let e = self.tcx.dyn_error_ty();
        let substs = gossamer_types::Substs::from_types([vec, e]);
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX),
            substs,
        })
    }

    /// The opaque `fs::DirInfo` blob handle shared by `fs::read_dir` and
    /// `fs::walk_dir` - a heap blob address held in a single scalar slot,
    /// not an inline struct.
    pub(crate) fn dir_info_adt_ty(&mut self) -> Ty {
        let def = gossamer_resolve::DefId::local(u32::MAX - 2);
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def,
            substs: gossamer_types::Substs::new(),
        })
    }

    pub(crate) fn result_unit_error_adt_ty(&mut self) -> Ty {
        let u = self.tcx.unit();
        let e = self.tcx.dyn_error_ty();
        let substs = gossamer_types::Substs::from_types([u, e]);
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX),
            substs,
        })
    }

    /// `Result<ok, String>` - the shape `gos_rt_join` produces (Ok
    /// value, or Err panic message as a String).
    pub(crate) fn result_payload_string_error_ty(&mut self, ok: Ty) -> Ty {
        let s = self.tcx.string_ty();
        let substs = gossamer_types::Substs::from_types([ok, s]);
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX),
            substs,
        })
    }

    pub(crate) fn result_i64_error_adt_ty(&mut self) -> Ty {
        let i = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let e = self.tcx.dyn_error_ty();
        let substs = gossamer_types::Substs::from_types([i, e]);
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX),
            substs,
        })
    }

    pub(crate) fn result_f64_error_adt_ty(&mut self) -> Ty {
        let f = self.tcx.float_ty(gossamer_types::FloatTy::F64);
        let e = self.tcx.dyn_error_ty();
        let substs = gossamer_types::Substs::from_types([f, e]);
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX),
            substs,
        })
    }

    pub(crate) fn result_bool_error_adt_ty(&mut self) -> Ty {
        let b = self.tcx.bool_ty();
        let e = self.tcx.dyn_error_ty();
        let substs = gossamer_types::Substs::from_types([b, e]);
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX),
            substs,
        })
    }

    pub(crate) fn result_json_value_error_adt_ty(&mut self) -> Ty {
        let j = self.tcx.json_value_ty();
        let e = self.tcx.dyn_error_ty();
        let substs = gossamer_types::Substs::from_types([j, e]);
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX),
            substs,
        })
    }

    pub(crate) fn option_json_value_adt_ty(&mut self) -> Ty {
        let j = self.tcx.json_value_ty();
        let substs = gossamer_types::Substs::from_types([j]);
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX - 1),
            substs,
        })
    }

    pub(crate) fn option_json_array_adt_ty(&mut self) -> Ty {
        let j = self.tcx.json_value_ty();
        let vec_ty = self.tcx.intern(gossamer_types::TyKind::Vec(j));
        let substs = gossamer_types::Substs::from_types([vec_ty]);
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX - 1),
            substs,
        })
    }

    pub(crate) fn option_string_vec_adt_ty(&mut self) -> Ty {
        let s = self.tcx.string_ty();
        let vec_ty = self.tcx.intern(gossamer_types::TyKind::Vec(s));
        let substs = gossamer_types::Substs::from_types([vec_ty]);
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX - 1),
            substs,
        })
    }

    /// True when an `iter()` receiver is a `HashMap` / `BTreeMap` (both
    /// `TyKind::HashMap`) or a `HashSet`. Such receivers must NOT be peeled
    /// to drive the for-loop: their handle is not a `GosVec`, and a map's
    /// `.iter()` yields `(K, V)` PAIRS, not the receiver's element type -
    /// so iteration must run over the runtime snapshot Vec the method
    /// produces (with the element type from the `.iter()` result), not the
    /// receiver. (`HashMap` is not a named Adt, so `runtime_kind_from_ty`
    /// returns `None` for it - this checks the kind directly.)
    pub(crate) fn receiver_is_map_or_set(&self, receiver: &gossamer_hir::HirExpr) -> bool {
        use gossamer_types::TyKind;
        let mut cur = receiver.ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(cur) {
            cur = *inner;
        }
        matches!(self.tcx.kind_of(cur), TyKind::HashMap { .. })
            || matches!(
                self.runtime_kind_from_ty(receiver.ty),
                Some("collections::HashSet" | "collections::BTreeSet")
            )
    }

    /// Recovers the runtime-kind tag of an opaque-handle stdlib type from
    /// the receiver's *type* when the construction-site tag was lost - e.g.
    /// a `HashSet<String>` flowing in as a function parameter or out as a
    /// return value carries no `local_runtime_kind` entry, so method
    /// dispatch on it would miss without this fallback.
    /// The `context::Context` handle type, as the checker's sentinel Adt,
    /// so a `request.context` read carries the identity its methods
    /// dispatch on.
    pub(crate) fn context_handle_ty(&mut self) -> Ty {
        let def = gossamer_resolve::DefId::local(u32::MAX - 11);
        self.tcx.register_def_name(def, "context::Context");
        self.tcx.intern(gossamer_types::TyKind::Adt {
            def,
            substs: gossamer_types::Substs::new(),
        })
    }

    pub(crate) fn runtime_kind_from_ty(&self, ty: Ty) -> Option<&'static str> {
        use gossamer_types::TyKind;
        let mut cur = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(cur) {
            cur = *inner;
        }
        // `DynError` is the type of an `errors::Error`, so it answers that
        // kind directly rather than through the rendered name below: the
        // rendered tail is `Error`, which a user type may also be called.
        if matches!(self.tcx.kind_of(cur), TyKind::DynError) {
            return Some("errors::Error");
        }
        let rendered = gossamer_types::printer::render_ty(self.tcx, cur);
        let bare = rendered.rsplit("::").next().unwrap_or(&rendered);
        let name = bare.split('<').next().unwrap_or(bare).trim();
        match name {
            "Set" => Some("collections::HashSet"),
            "BTreeSet" => Some("collections::BTreeSet"),
            "Map" => Some("collections::HashMap"),
            "BTreeMap" => Some("collections::HashMap"),
            "Deque" => Some("collections::VecDeque"),
            "Queue" => Some("collections::VecQueue"),
            "Stack" => Some("collections::VecStack"),
            // A `sync::AtomicBool` reaching a method call by parameter
            // (no local construction to tag) still routes `load`/`store`
            // to the bool-typed shims.
            "AtomicBool" => Some("sync::AtomicBool"),
            // `validate::Errors` / `validate::FieldError` handles flowing
            // in by parameter or out by return carry no construction tag;
            // recover the handle kind from the receiver's named type.
            "Errors" => Some("validate::Errors"),
            "FieldError" => Some("validate::FieldError"),
            // A `context::Context` passed as a parameter (the canonical
            // request-propagation shape) carries no construction tag, so
            // its `is_cancelled` / `cancel` / `done` / `done_chan` calls
            // route through the type here.
            "Context" => Some("context::Context"),
            // `sync::Once` / `sync::RwLock` / `sync::Shared` reaching a
            // method call recover their handle kind from the named type;
            // their closure-taking methods route to the free lowering,
            // which is the only path that can build the env thunk.
            "Once" => Some("sync::Once"),
            "RwLock" => Some("sync::RwLock"),
            "Shared" => Some("sync::Shared"),
            // `net::TcpStream` / `TcpListener` / `UdpSocket` / `UnixStream`
            // / `UnixListener` flowing through a struct field or parameter
            // (no local construction tag) recover their handle kind from
            // the named sentinel Adt the checker now resolves the
            // annotation to, so `conn.sock.read(..)` dispatches to the
            // runtime helper instead of an undefined name-global symbol.
            // A `http::Server` passed to a goroutine as a parameter - the
            // shape `go run_server(s)` takes - carries no construction tag,
            // so its methods recover the handle kind from the named type.
            // A `Router` reaching a handler slot as a parameter - the shape
            // `go run(server, app)` takes - carries no construction tag, so
            // it recovers its handle kind from the named type and dispatches
            // through the router's own serve rather than a struct method
            // nothing declares.
            "Router" => Some("http::Router"),
            "ResponseStream" => Some("http::ResponseStream"),
            "Server" => Some("http::Server"),
            "TcpStream" => Some("net::TcpStream"),
            "TcpListener" => Some("net::TcpListener"),
            "UdpSocket" => Some("net::UdpSocket"),
            "UnixStream" => Some("net::UnixStream"),
            "UnixListener" => Some("net::UnixListener"),
            "File" => Some("fs::File"),
            "OpenOptions" => Some("fs::OpenOptions"),
            // A piped child handle extracted from
            // `process::spawn_piped(..)`'s Ok payload; routes
            // `write_stdin` / `read_line` / `wait` / ... to the
            // child shims.
            "Child" => Some("process::Child"),
            "Stream" => Some("io::Stream"),
            "Notifier" => Some("signal::Notifier"),
            _ => None,
        }
    }

    pub(crate) fn expr_runtime_kind(&self, expr: &HirExpr) -> Option<&'static str> {
        let HirExprKind::MethodCall { receiver, name, .. } = &expr.kind else {
            return None;
        };
        let receiver_kind = self
            .receiver_local_from_path(receiver)
            .and_then(|l| self.local_runtime_kind.get(&l).copied())
            .or_else(|| self.expr_runtime_kind(receiver))?;
        match (receiver_kind, name.name.as_str()) {
            ("http::Client", "get" | "post" | "put" | "options" | "delete" | "head") => {
                Some("http::Request")
            }
            // Configured-policy request entry points yield the same
            // packed Result<Response, errors::Error> as `.send()`.
            ("http::Client", "request" | "request_bytes") => Some("http::SendResult"),
            ("http::ClientBuilder", "max_redirects" | "timeout_ms" | "cookie_jar" | "proxy") => {
                Some("http::ClientBuilder")
            }
            ("http::ClientBuilder", "build") => Some("http::Client"),
            ("http::Request", "header" | "body") => Some("http::Request"),
            // `.send()` yields `Result<Response, errors::Error>` -
            // a dedicated tag so chained `.map_err(..)` / `.map(..)`
            // route through the result helpers instead of the
            // identity copy.
            ("http::Request", "send") => Some("http::SendResult"),
            _ => None,
        }
    }

    /// True when `expr` is a chained `.send()` whose result is the
    /// packed `Result<Response, errors::Error>` - the HIR type is an
    /// inference Var there, so result-combinator dispatch consults
    /// this structural probe as a fallback.
    pub(crate) fn expr_is_send_result(&self, expr: &HirExpr) -> bool {
        self.expr_runtime_kind(expr) == Some("http::SendResult")
    }

    /// Byte offset of each payload field within a heap enum node's slot slab.
    ///
    /// A field occupies its declared slot width, the way a struct field does,
    /// so a two-word `Option` / `Result` / inline-enum field keeps both its
    /// discriminant and its payload word. Every other field is one word: a
    /// multi-slot struct / tuple / array payload is stored as the pointer to
    /// its box, and a field whose declared type never resolved is read and
    /// written as a bare word.
    ///
    /// Construction and every match site must agree on these offsets, so both
    /// ends compute them from the same declared field types.
    pub(crate) fn variant_payload_offsets(&self, field_tys: &[Ty], n_fields: usize) -> Vec<i64> {
        use gossamer_types::TyKind;
        let mut offsets = Vec::with_capacity(n_fields);
        let mut byte = 0i64;
        for i in 0..n_fields {
            offsets.push(byte);
            let wide = field_tys
                .get(i)
                .copied()
                .filter(|&t| !matches!(self.tcx.kind_of(t), TyKind::Var(_) | TyKind::Error))
                .is_some_and(|t| crate::lower::carrier_ref::is_two_word_carrier(self.tcx, t));
            byte += if wide { 16 } else { 8 };
        }
        offsets
    }

    /// Total payload bytes of a variant whose fields have these offsets.
    pub(crate) fn variant_payload_bytes(&self, field_tys: &[Ty], n_fields: usize) -> i64 {
        use gossamer_types::TyKind;
        let mut byte = 0i64;
        for i in 0..n_fields {
            let wide = field_tys
                .get(i)
                .copied()
                .filter(|&t| !matches!(self.tcx.kind_of(t), TyKind::Var(_) | TyKind::Error))
                .is_some_and(|t| crate::lower::carrier_ref::is_two_word_carrier(self.tcx, t));
            byte += if wide { 16 } else { 8 };
        }
        byte.max(8)
    }

    /// Whether `ty`'s Ok / Some payload is itself a `Result` or `Option`.
    ///
    /// A carrier is two words and does not fit the payload half, so a
    /// nested one is boxed and the payload holds its address. A reader
    /// that hands the payload back as a value must load it rather than
    /// treat the address as the carrier.
    /// Whether the carrier's payload is a `Vec` / `[T]` - the case where
    /// `unwrap_or`'s fallback is itself a heap value and only one of the two
    /// becomes the answer.
    pub(crate) fn carrier_payload_is_sequence(&self, ty: Ty) -> bool {
        use gossamer_types::TyKind;
        let mut cur = ty;
        loop {
            match self.tcx.kind_of(cur) {
                TyKind::Ref { inner, .. } => cur = *inner,
                TyKind::Adt { def, substs }
                    if def.local == u32::MAX || def.local == u32::MAX - 1 =>
                {
                    return substs.types().first().is_some_and(|payload| {
                        matches!(
                            self.tcx.kind_of(*payload),
                            TyKind::Vec(_) | TyKind::Slice(_)
                        )
                    });
                }
                _ => return false,
            }
        }
    }

    /// Whether the carrier's payload is a `String` - the case where
    /// `unwrap_or`'s fallback is itself a heap value and only one of the two
    /// becomes the answer.
    pub(crate) fn carrier_payload_is_string(&self, ty: Ty) -> bool {
        use gossamer_types::TyKind;
        let mut cur = ty;
        loop {
            match self.tcx.kind_of(cur) {
                TyKind::Ref { inner, .. } => cur = *inner,
                TyKind::Adt { def, substs }
                    if def.local == u32::MAX || def.local == u32::MAX - 1 =>
                {
                    return substs.types().first().is_some_and(|payload| {
                        matches!(self.tcx.kind_of(*payload), TyKind::String)
                    });
                }
                _ => return false,
            }
        }
    }

    /// The release helper for a carrier's heap payload, or `None` when the
    /// payload owns no storage of its own.
    pub(crate) fn carrier_payload_free_helper(&self, ty: Ty) -> Option<&'static str> {
        use gossamer_types::TyKind;
        let mut cur = ty;
        loop {
            match self.tcx.kind_of(cur) {
                TyKind::Ref { inner, .. } => cur = *inner,
                TyKind::Adt { def, substs }
                    if def.local == u32::MAX || def.local == u32::MAX - 1 =>
                {
                    return substs.types().first().and_then(|payload| {
                        match self.tcx.kind_of(*payload) {
                            TyKind::String => Some("gos_rt_str_free_typed"),
                            TyKind::Vec(_) | TyKind::Slice(_) => Some("gos_rt_vec_free"),
                            _ => None,
                        }
                    });
                }
                _ => return None,
            }
        }
    }

    pub(crate) fn carrier_payload_is_carrier(&self, ty: Ty) -> bool {
        use gossamer_types::TyKind;
        let mut cur = ty;
        loop {
            match self.tcx.kind_of(cur) {
                TyKind::Ref { inner, .. } => cur = *inner,
                TyKind::Adt { def, substs }
                    if def.local == u32::MAX || def.local == u32::MAX - 1 =>
                {
                    return substs
                        .types()
                        .first()
                        .is_some_and(|payload| self.is_result_or_option_adt(*payload));
                }
                _ => return false,
            }
        }
    }

    pub(crate) fn is_result_or_option_adt(&self, ty: Ty) -> bool {
        use gossamer_types::TyKind;
        let mut cur = ty;
        loop {
            match self.tcx.kind_of(cur) {
                TyKind::Ref { inner, .. } => cur = *inner,
                TyKind::Adt { def, .. } => {
                    return def.local == u32::MAX || def.local == u32::MAX - 1;
                }
                _ => return false,
            }
        }
    }

    /// True for the Option sentinel Adt specifically (ref-transparent).
    pub(crate) fn is_option_adt(&self, ty: Ty) -> bool {
        use gossamer_types::TyKind;
        let mut cur = ty;
        loop {
            match self.tcx.kind_of(cur) {
                TyKind::Ref { inner, .. } => cur = *inner,
                TyKind::Adt { def, .. } => return def.local == u32::MAX - 1,
                _ => return false,
            }
        }
    }

    /// True when `ty` lowers to the 2-word by-value enum
    /// representation (sentinel Result/Option Adt or an inline user
    /// enum). Payloads of this shape are heap-copied at
    /// `gos_rt_result_new` and must be extracted through
    /// `gos_rt_result_payload_i128`, not the scalar extractor.
    pub(crate) fn is_by_value_enum_ty(&self, ty: Ty) -> bool {
        use gossamer_types::TyKind;
        matches!(
            self.tcx.kind_of(ty),
            TyKind::Adt { def, .. } if def.local == u32::MAX || def.local == u32::MAX - 1
        ) || self.tcx.is_inline_enum_ty(ty)
    }

    pub(crate) fn adt_generic_at(&self, ty: Ty, idx: usize) -> Option<Ty> {
        use gossamer_types::TyKind;
        let mut cur = ty;
        loop {
            match self.tcx.kind_of(cur) {
                TyKind::Ref { inner, .. } => cur = *inner,
                TyKind::Adt { substs, .. } => {
                    return substs.types().get(idx).copied();
                }
                _ => return None,
            }
        }
    }

    /// Flat `(disc_word, payload_word)` pairs for the guarded copy-blob
    /// meta of a struct type: one pair per `Option`/`Result` field whose
    /// `Ok`/`Some` payload is a multi-slot aggregate (the shapes the LLVM
    /// backend heap-copies and stores by pointer in the payload word),
    /// recursing through multi-slot inline struct fields. Word offsets
    /// are absolute within the struct's flat slot layout.
    pub(crate) fn guarded_child_pairs(&self, ty: Ty) -> Vec<(i64, i64, i64)> {
        let mut pairs = Vec::new();
        self.collect_guarded_pairs(ty, 0, 0, &mut pairs);
        pairs
    }

    fn collect_guarded_pairs(
        &self,
        ty: Ty,
        base_word: i64,
        depth: u32,
        out: &mut Vec<(i64, i64, i64)>,
    ) {
        use gossamer_types::TyKind;
        if depth > 8 {
            return;
        }
        let TyKind::Adt { def, .. } = self.tcx.kind_of(ty) else {
            return;
        };
        if def.local == u32::MAX || def.local == u32::MAX - 1 {
            return;
        }
        let Some(field_tys) = self.tcx.struct_field_tys(*def) else {
            return;
        };
        let field_tys: Vec<Ty> = field_tys.to_vec();
        let mut word = base_word;
        for fty in field_tys {
            let fwords = i64::from(self.type_slot_bytes(fty).max(8) / 8);
            match self.tcx.kind_of(fty) {
                TyKind::Adt { def, substs }
                    if def.local == u32::MAX || def.local == u32::MAX - 1 =>
                {
                    // By-value `{disc, payload}` field. The payload word
                    // holds a heap-copy pointer whenever the active side's
                    // payload is a by-value aggregate, one slot or many:
                    // substs[0] (Ok/Some) under disc 0, substs[1] (Err)
                    // under disc 1. When both sides are copies the entry
                    // is unconditional (gate -1). The runtime walk
                    // re-checks the discriminant gate and the copy-blob
                    // provenance set, so over-approximating is safe.
                    //
                    // The shape has to match what the backend actually
                    // copies. A one-field struct is a single slot and is
                    // copied like any other, so its owner needs the entry
                    // to keep a share of it - the payload the option local
                    // released on its way out is the same blob.
                    let is_copy_shape = |t: Ty| self.is_inline_slot_block(t);
                    let ok_side = substs.types().first().copied().is_some_and(is_copy_shape);
                    let err_side = substs.types().get(1).copied().is_some_and(is_copy_shape);
                    match (ok_side, err_side) {
                        (true, true) => out.push((-1, word, word + 1)),
                        (true, false) => out.push((0, word, word + 1)),
                        (false, true) => out.push((1, word, word + 1)),
                        (false, false) => {}
                    }
                }
                TyKind::Adt { .. } if fwords > 1 => {
                    // Multi-slot inline sub-struct: its fields occupy this
                    // struct's slots directly.
                    self.collect_guarded_pairs(fty, word, depth + 1, out);
                }
                _ => {}
            }
            word += fwords;
        }
    }

    /// Registers (idempotently) the `RC_KIND_STRUCT_GUARDED` copy-blob
    /// meta for `ty` and returns its symbol, or `None` when the type has
    /// no guarded child slots (a leaf - its copies need no meta and no
    /// drop-pass walks).
    pub(crate) fn ensure_aggr_copy_meta(&mut self, ty: Ty) -> Option<String> {
        if let Some(sym) = self.tcx.aggr_copy_meta(ty) {
            return Some(sym.to_string());
        }
        let pairs = self.guarded_child_pairs(ty);
        let symbol = if pairs.is_empty() {
            // Leaf struct: its copies carry no child pointers of their
            // own, but they still need the guarded-kind meta so the
            // provenance set entry is removed at free, and so a parent's
            // guarded slot can reclaim them. One shared blob serves
            // every leaf type.
            let symbol = "gos_rc_meta_copyblob_leaf".to_string();
            self.tcx.register_rc_meta(
                symbol.clone(),
                vec![gossamer_abi::rc::RC_KIND_STRUCT_GUARDED, 0],
            );
            symbol
        } else {
            let symbol = format!("gos_rc_meta_copyblob_{}", ty.as_u32());
            let mut blob = vec![gossamer_abi::rc::RC_KIND_STRUCT_GUARDED, pairs.len() as i64];
            for (g, d, p) in &pairs {
                blob.push(*g);
                blob.push(*d);
                blob.push(*p);
            }
            self.tcx.register_rc_meta(symbol.clone(), blob);
            symbol
        };
        self.tcx.register_aggr_copy_meta(ty, symbol.clone());
        Some(symbol)
    }

    /// Encoded child entries for a by-value aggregate `ty`, recursing through
    /// inline sub-structs / tuples at absolute word offsets. Both RC-managed
    /// children (`String` / enum nodes) and `Vec` children must be named: an
    /// escaped aggregate copy owns a share of each and releases them through
    /// their respective runtime paths.
    pub(crate) fn aggr_child_entries(&self, ty: Ty) -> Vec<i64> {
        let mut out = Vec::new();
        self.collect_aggr_child_entries(ty, 0, 0, &mut out);
        out
    }

    fn collect_aggr_child_entries(&self, ty: Ty, base_word: i64, depth: u32, out: &mut Vec<i64>) {
        use gossamer_types::TyKind;
        if depth > 16 {
            return;
        }
        let field_tys: Vec<Ty> = match self.tcx.kind_of(ty) {
            TyKind::Adt { def, .. }
                if def.local < u32::MAX - 16 && !self.tcx.is_inline_enum_ty(ty) =>
            {
                match self.tcx.struct_field_tys(*def) {
                    Some(fields) => fields.to_vec(),
                    None => return,
                }
            }
            TyKind::Tuple(elems) => elems.clone(),
            TyKind::Array { elem, len } => vec![*elem; len.to_usize()],
            _ => return,
        };
        let mut word = base_word;
        for fty in field_tys {
            let fwords = i64::from(self.type_slot_bytes(fty).max(8) / 8);
            if matches!(self.tcx.kind_of(fty), TyKind::Vec(_) | TyKind::Slice(_)) {
                out.push(
                    (gossamer_abi::rc::RC_CHILD_VEC << gossamer_abi::rc::RC_CHILD_KIND_SHIFT)
                        | word,
                );
            } else if matches!(self.tcx.kind_of(fty), TyKind::HashMap { .. }) {
                // A `GosMap` has no reference count, so a copy of this
                // aggregate cannot share the field's table: the blob takes a
                // clone of its own and frees it at its death.
                out.push(
                    (gossamer_abi::rc::RC_CHILD_MAP << gossamer_abi::rc::RC_CHILD_KIND_SHIFT)
                        | word,
                );
            } else if self.tcx.is_rc_managed(fty) {
                out.push(
                    (gossamer_abi::rc::RC_CHILD_RC << gossamer_abi::rc::RC_CHILD_KIND_SHIFT) | word,
                );
            } else if matches!(
                self.tcx.kind_of(fty),
                TyKind::Tuple(_) | TyKind::Adt { .. } | TyKind::Array { .. }
            ) {
                // Inline sub-struct / tuple / array: its fields occupy these
                // slots.
                self.collect_aggr_child_entries(fty, word, depth + 1, out);
            }
            word += fwords;
        }
    }

    /// Registers (idempotently) the `RC_KIND_STRUCT` child-word meta for an
    /// enum-payload box of aggregate type `ty`, returning its symbol, or
    /// `None` when the aggregate has no owning children (a scalar struct like
    /// `Point` - its box is a meta-less leaf the release walk frees directly).
    pub(crate) fn ensure_aggr_struct_meta(&mut self, ty: Ty) -> Option<String> {
        let entries = self.aggr_child_entries(ty);
        if entries.is_empty() {
            return None;
        }
        let symbol = format!("gos_rc_meta_boxaggr_{}", ty.as_u32());
        if self.tcx.rc_meta(&symbol).is_some() {
            return Some(symbol);
        }
        let mut blob = vec![gossamer_abi::rc::RC_KIND_STRUCT, 1, 0, entries.len() as i64];
        blob.extend_from_slice(&entries);
        self.tcx.register_rc_meta(symbol.clone(), blob);
        Some(symbol)
    }

    /// Builds (idempotently) the structural-equality descriptor for a heap
    /// (recursive / `Box`) user enum `ty` and returns its codegen symbol, or
    /// `None` when a field is one the walk cannot describe (a nested enum of a
    /// *different* type, or a non-scalar/non-self aggregate) - the caller then
    /// keeps pointer identity for that enum. Blob (pure `i64`, consumed by
    /// `gos_rt_enum_struct_eq`): `[num_variants]` then, per variant in
    /// declaration order, `[num_fields, kind_0, ..]`.
    pub(crate) fn ensure_enum_eq_desc(&mut self, ty: gossamer_types::Ty) -> Option<String> {
        use gossamer_types::TyKind;
        // `a == b` on heap enums often compares `&Tree`; a reference to a heap
        // enum is the same node pointer, so peel refs to reach the `Adt`.
        let mut ty = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(ty) {
            ty = *inner;
        }
        let TyKind::Adt { def, .. } = self.tcx.kind_of(ty) else {
            return None;
        };
        let def = *def;
        // Only heap (RC-managed, non-inline 2-word) user enums reach the
        // pointer-identity path this replaces.
        if self.tcx.is_inline_enum_ty(ty) || !self.tcx.is_rc_managed(ty) {
            return None;
        }
        let symbol = format!("gos_rc_meta_enumeq_{}", ty.as_u32());
        if self.tcx.rc_meta(&symbol).is_some() {
            return Some(symbol);
        }
        let variants = self.tcx.enum_variant_tys(def)?.to_vec();
        let mut blob: Vec<i64> = vec![variants.len() as i64];
        for fields in &variants {
            blob.push(fields.len() as i64);
            for &fty in fields {
                blob.push(self.enum_eq_field_kind(fty, def)?);
            }
        }
        self.tcx.register_rc_meta(symbol.clone(), blob);
        Some(symbol)
    }

    /// Classifies one enum-variant field for the structural-eq descriptor:
    /// `0` word (int/bool/char), `1` `f64`, `2` `String`, `3` a nested field of
    /// the same enum (recurse), `4` `Vec<Self>`, `5` `Vec<(String, Self)>`.
    /// `None` for anything else (a *different* nested enum, other aggregate),
    /// which makes the whole enum fall back to pointer identity.
    fn enum_eq_field_kind(
        &self,
        fty: gossamer_types::Ty,
        self_def: gossamer_resolve::DefId,
    ) -> Option<i64> {
        use gossamer_types::TyKind;
        let is_self_enum = |this: &Self, t: gossamer_types::Ty| matches!(this.tcx.kind_of(t), TyKind::Adt { def, .. } if *def == self_def);
        match self.tcx.kind_of(fty) {
            TyKind::Bool | TyKind::Char | TyKind::Int(_) => Some(0),
            TyKind::Float(_) => Some(1),
            TyKind::String => Some(2),
            TyKind::Adt { def, .. } if *def == self_def => Some(3),
            TyKind::Vec(elem) | TyKind::Slice(elem) => {
                let elem = *elem;
                if is_self_enum(self, elem) {
                    Some(4)
                } else if let TyKind::Tuple(ts) = self.tcx.kind_of(elem)
                    && ts.len() == 2
                    && matches!(self.tcx.kind_of(ts[0]), TyKind::String)
                    && is_self_enum(self, ts[1])
                {
                    Some(5)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Whether a value of this type lives in a container slot as its own
    /// words rather than as a handle: a tuple, a fixed array, or a user
    /// struct. A binding to one names the slot's address, and its field reads
    /// take their offsets from there.
    pub(crate) fn is_inline_aggregate_ty(&self, ty: Ty) -> bool {
        use gossamer_types::TyKind;
        match self.tcx.kind_of(ty) {
            TyKind::Tuple(_) | TyKind::Array { .. } => true,
            TyKind::Adt { def, .. } => {
                def.local < u32::MAX - 16 && self.tcx.struct_field_tys(*def).is_some()
            }
            _ => false,
        }
    }

    pub(crate) fn elem_bytes_of(&self, ty: Ty) -> u32 {
        use gossamer_types::TyKind;
        match self.tcx.kind_of(ty) {
            // Bool uses a 1-byte element stride: each element occupies
            // exactly 1 byte in the GosVec data buffer. The inline
            // get/set paths read and write via the header-driven byte
            // path (`elem_bytes == 1` branch), and the push path uses
            // `gos_rt_vec_push_i64` which memcpys only `elem_bytes`
            // bytes from the i64 payload, so a bool push correctly
            // stores the low byte (0 or 1) with no overflow.
            TyKind::Bool => 1,
            // `u8` is byte-packed (stride 1): a `[u8]` / `Vec<u8>` stores one
            // byte per element like Go's `[]byte`, not an 8-byte word per byte.
            // u8 is unsigned, so the runtime's byte get path (`load i8` +
            // zero-extend, shared with `bool`) reconstructs the value exactly.
            // Signed `i8` and the wider narrow ints stay 8-byte: the get path
            // only distinguishes byte (1) from word (8) stride, and a signed
            // byte would need sign-extension. Removes the 8x RAM overhead on
            // byte buffers (the unbounded-cache leak benchmark + all binary/IO
            // buffers).
            TyKind::Int(gossamer_types::IntTy::U8) => 1,
            // Char occupies a full 8-byte slot so it aligns with the
            // word-stride fast paths throughout the codegen.
            TyKind::Char => 8,
            TyKind::Int(_) | TyKind::Float(_) => 8,
            TyKind::String => 8,
            // Tuples / aggregate ADTs occupy `slot_count * 8` bytes
            // in the flat-stack representation the native codegen
            // uses (mirrors `type_slot_count` in cranelift's
            // native.rs). A `(String, String)` tuple is two i64
            // slots = 16 bytes; treating it as 8 like any other
            // compound type would make `[(a, b), (c, d)].to_vec()`
            // copy only half of each pair.
            TyKind::Tuple(_) | TyKind::Array { .. } | TyKind::Adt { .. } => {
                self.type_slot_bytes(ty)
            }
            // Default to pointer-sized for everything else (refs,
            // enums-as-handles, channels, …).
            _ => 8,
        }
    }

    /// Builds the `Weak<T>` sentinel Adt (def `u32::MAX - 6`), matching
    /// the typechecker's `weak_adt_ty`. Used to pin the result of
    /// `x.downgrade()` so the drop pass releases it via the weak helpers.
    pub(crate) fn weak_adt_ty(&mut self, payload: Ty) -> Ty {
        use gossamer_types::TyKind;
        let def = gossamer_resolve::DefId::local(u32::MAX - 6);
        let substs = gossamer_types::Substs::from_types([payload]);
        self.tcx.intern(TyKind::Adt { def, substs })
    }

    /// Builds the `WeakCell<T>` sentinel Adt (def `u32::MAX - 33`): the RC
    /// cell a `Weak<T>` observes when `T` is a by-value aggregate with no RC
    /// header of its own. Registered as RC-managed so the drop pass gives the
    /// cell the ordinary strong retain/release schedule, and left with no
    /// registered field layout so every backend treats it as a pointer word.
    pub(crate) fn weak_cell_adt_ty(&mut self, payload: Ty) -> Ty {
        use gossamer_types::TyKind;
        let def = gossamer_resolve::DefId::local(u32::MAX - 33);
        self.tcx.register_def_name(def, "WeakCell");
        let substs = gossamer_types::Substs::from_types([payload]);
        let ty = self.tcx.intern(TyKind::Adt { def, substs });
        self.tcx.register_rc_managed_ty(ty);
        ty
    }

    /// True when a `.downgrade()` receiver of this type is a by-value
    /// aggregate whose runtime value is inline slot data rather than an RC
    /// payload pointer, so the weak must observe an RC cell holding a copy.
    pub(crate) fn weak_referent_needs_cell(&self, ty: Ty) -> bool {
        use gossamer_types::TyKind;
        if self.tcx.is_rc_managed(ty) {
            return false;
        }
        match self.tcx.kind_of(ty) {
            // Stdlib sentinel Adts (`u32::MAX - 16 ..= u32::MAX`) are opaque
            // runtime handles, not slot data.
            TyKind::Adt { def, .. } => def.local < u32::MAX - 16 && !self.tcx.is_inline_enum_ty(ty),
            TyKind::Tuple(_) | TyKind::Array { .. } => true,
            _ => false,
        }
    }

    /// Extracts `T` from a `Weak<T>` sentinel Adt; returns `None` for any
    /// other type.
    pub(crate) fn weak_payload_ty(&self, ty: Ty) -> Option<Ty> {
        use gossamer_types::TyKind;
        match self.tcx.kind_of(ty) {
            TyKind::Adt { def, substs } if def.local == u32::MAX - 6 => {
                substs.types().first().copied()
            }
            _ => None,
        }
    }

    /// Builds the `Option<T>` sentinel Adt (def `u32::MAX - 1`) carrying
    /// a concrete payload subst. Used to pin the result of `w.upgrade()`
    /// so the standard match/if-let machinery reads the discriminant and
    /// binds the payload at the right type.
    /// The value a callable expression answers: the declared return of a
    /// closure literal, or the output its type names.
    pub(crate) fn closure_expr_output_ty(&self, expr: &HirExpr) -> Option<Ty> {
        use gossamer_types::TyKind;
        let concrete = |this: &Self, ty: Ty| {
            (!matches!(
                this.tcx.kind_of(ty),
                TyKind::Var(_) | TyKind::Error | TyKind::Never
            ))
            .then_some(ty)
        };
        if let HirExprKind::Closure { body, ret, .. } = &expr.kind {
            if let Some(declared) = ret.and_then(|ty| concrete(self, ty)) {
                return Some(declared);
            }
            if let Some(from_body) = concrete(self, body.ty) {
                return Some(from_body);
            }
        }
        match self.tcx.kind_of(expr.ty) {
            TyKind::FnPtr(sig) | TyKind::FnTrait(sig) => {
                let output = sig.output;
                concrete(self, output)
            }
            _ => None,
        }
    }

    /// The value a callable local answers, when its type names one.
    pub(crate) fn callable_output_ty(&self, callable: Local) -> Option<Ty> {
        use gossamer_types::TyKind;
        let ty = self.locals[callable.0 as usize].ty;
        let (TyKind::FnPtr(sig) | TyKind::FnTrait(sig)) = self.tcx.kind_of(ty) else {
            return None;
        };
        let output = sig.output;
        (!matches!(
            self.tcx.kind_of(output),
            TyKind::Var(_) | TyKind::Error | TyKind::Never
        ))
        .then_some(output)
    }

    pub(crate) fn option_payload_adt_ty(&mut self, payload: Ty) -> Ty {
        use gossamer_types::TyKind;
        let def = gossamer_resolve::DefId::local(u32::MAX - 1);
        let substs = gossamer_types::Substs::from_types([payload]);
        self.tcx.intern(TyKind::Adt { def, substs })
    }

    /// Payload type of an `Option<T>`, or `None` for any other type.
    pub(crate) fn option_payload_of(&self, ty: Ty) -> Option<Ty> {
        use gossamer_types::TyKind;
        match self.tcx.kind_of(ty) {
            TyKind::Adt { def, substs } if def.local == u32::MAX - 1 => {
                substs.types().first().copied()
            }
            _ => None,
        }
    }

    pub(crate) fn type_slot_bytes(&self, ty: Ty) -> u32 {
        // Single source of truth on the `TyCtxt` so the vec-element
        // layout passes (`insert_vec_elem_metas`) and the builder agree.
        // A generic ADT whose per-instantiation field table is not on the
        // `TyCtxt` yet still has its concrete arguments in its own `substs`,
        // so resolve the declared `Param` fields through them here: a layout
        // baked into MIR (a Vec's element width, an aggregate copy size)
        // has to describe the instantiation, not the template.
        self.slot_bytes_instantiated(ty, &[])
    }

    /// Slot footprint of `ty` in bytes with `params` supplying the enclosing
    /// instantiation's type arguments for any `Param` field it reaches.
    fn slot_bytes_instantiated(&self, ty: Ty, params: &[Ty]) -> u32 {
        use gossamer_types::TyKind;
        match self.tcx.kind_of(ty) {
            TyKind::Param { idx, .. } => params
                .get(idx.0 as usize)
                .map_or_else(|| self.tcx.slot_bytes(ty), |t| self.type_slot_bytes(*t)),
            TyKind::Tuple(elems) => {
                let total: u32 = elems
                    .iter()
                    .map(|t| self.slot_bytes_instantiated(*t, params).max(8) / 8)
                    .sum();
                total.max(1) * 8
            }
            TyKind::Array { elem, len } => {
                let elem_bytes = self.slot_bytes_instantiated(*elem, params).max(8);
                u32::try_from(len.to_usize())
                    .unwrap_or(1)
                    .saturating_mul(elem_bytes)
            }
            // Sentinel ADTs (`Option` / `Result` and the opaque stdlib
            // handles) carry a fixed width the `TyCtxt` owns.
            TyKind::Adt { def, .. } if def.local >= u32::MAX - 6 => self.tcx.slot_bytes(ty),
            TyKind::Adt { def, substs } => {
                let Some(field_tys) = self.tcx.adt_field_tys(*def, substs) else {
                    return self.tcx.slot_bytes(ty);
                };
                let args = substs.types();
                let total: u32 = field_tys
                    .iter()
                    .map(|t| self.slot_bytes_instantiated(*t, &args).max(8) / 8)
                    .sum();
                total.max(1) * 8
            }
            _ => self.tcx.slot_bytes(ty),
        }
    }

    pub(crate) fn binding_type_to_mir(&mut self, t: &gossamer_resolve::BindingType) -> Ty {
        use gossamer_resolve::BindingType as B;
        use gossamer_types::TyKind;
        match t {
            B::Unit => self.tcx.unit(),
            B::Bool => self.tcx.bool_ty(),
            B::I64 => self.tcx.int_ty(gossamer_types::IntTy::I64),
            B::F64 => self.tcx.float_ty(gossamer_types::FloatTy::F64),
            B::Char => self.tcx.char_ty(),
            B::String => self.tcx.string_ty(),
            B::Bytes => {
                // Bytes is `[u8]` at the source level, and a slice carries
                // that: it shares the `Vec` runtime representation - the same
                // IntArray path `[i64]` takes - while keeping the bare-bracket
                // spelling the source type is written in.
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                self.tcx.intern(TyKind::Slice(u8_ty))
            }
            B::Vec(inner) => {
                let inner_ty = self.binding_type_to_mir(inner);
                self.tcx.intern(TyKind::Vec(inner_ty))
            }
            // A binding's variant with no declared arms is the open
            // dynamic value: its shape is decided by the data, so it
            // carries the runtime's `DynValue`.
            B::Variant(arms) if arms.is_empty() => self.tcx.intern(TyKind::DynValue),
            // Option / Result / a declared arm set map to the runtime's
            // tagged-union pointer; the codegen treats them as ptr-sized.
            B::Option(_) | B::Result(_, _) | B::Variant(_) => {
                self.tcx.int_ty(gossamer_types::IntTy::I64)
            }
            // Map / Callback / Tuple / Opaque / Any all flow as
            // ptr-sized values (handles or untyped passthroughs).
            B::Map(_, _) | B::Callback(_, _) | B::Tuple(_) | B::Opaque(_) | B::Any => {
                self.tcx.int_ty(gossamer_types::IntTy::I64)
            }
        }
    }

    pub(crate) fn struct_name_from_expr(&self, expr: &HirExpr) -> Option<String> {
        use gossamer_types::TyKind;
        if let Some(name) = self.struct_name_of(expr.ty) {
            return Some(name);
        }
        match &expr.kind {
            HirExprKind::Index { base, .. } => {
                // Prefer the element-type registration (survives
                // inference-variable leakage) before walking the
                // base's static type.
                if let HirExprKind::Path { segments, .. } = &base.kind {
                    if let Some(first) = segments.first() {
                        if let Some(local) = self.lookup_local(&first.name) {
                            if let Some(name) = self.local_elem_struct.get(&local).cloned() {
                                return Some(name);
                            }
                        }
                    }
                }
                let mut cur = base.ty;
                loop {
                    match self.tcx.kind_of(cur) {
                        TyKind::Array { elem, .. } | TyKind::Slice(elem) | TyKind::Vec(elem) => {
                            return self.struct_name_of(*elem);
                        }
                        TyKind::Ref { inner, .. } => cur = *inner,
                        _ => return self.struct_name_from_expr(base),
                    }
                }
            }
            HirExprKind::TupleIndex { receiver, index } => {
                let mut cur = receiver.ty;
                loop {
                    match self.tcx.kind_of(cur) {
                        TyKind::Tuple(elems) => {
                            let elem = *elems.get(*index as usize)?;
                            return self.struct_name_of(elem);
                        }
                        TyKind::Ref { inner, .. } => cur = *inner,
                        _ => return self.struct_name_from_expr(receiver),
                    }
                }
            }
            HirExprKind::Path { segments, .. } => {
                let first = segments.first()?;
                let local = self.lookup_local(&first.name)?;
                // A variant constructor's type is left a `Var`, so recover the
                // struct / enum name from the `local_struct` tag first.
                if let Some(name) = self.local_struct.get(&local).cloned() {
                    return Some(name);
                }
                let ty = self.locals.get(local.0 as usize)?.ty;
                self.struct_name_of(ty)
            }
            _ => None,
        }
    }

    pub(crate) fn substs_of(&self, ty: Ty) -> gossamer_types::Substs {
        match self.tcx.kind(ty) {
            Some(gossamer_types::TyKind::FnDef { substs, .. }) => substs.clone(),
            _ => gossamer_types::Substs::new(),
        }
    }

    /// Element type of a sequence or iterator type, peeling a reference.
    pub(crate) fn sequence_elem_ty_of(&self, ty: Ty) -> Option<Ty> {
        use gossamer_types::TyKind;
        let mut peeled = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(peeled) {
            peeled = *inner;
        }
        match self.tcx.kind_of(peeled) {
            TyKind::Array { elem, .. }
            | TyKind::Slice(elem)
            | TyKind::Vec(elem)
            | TyKind::Iterator(elem) => Some(*elem),
            _ => None,
        }
    }

    pub(crate) fn iter_element_kind(&self, ty: Ty) -> Option<gossamer_types::TyKind> {
        use gossamer_types::TyKind;
        let kind = self.tcx.kind_of(ty).clone();
        let kind = match kind {
            TyKind::Ref { inner, .. } => self.tcx.kind_of(inner).clone(),
            other => other,
        };
        match kind {
            TyKind::Array { elem, .. }
            | TyKind::Slice(elem)
            | TyKind::Vec(elem)
            | TyKind::Iterator(elem) => Some(self.tcx.kind_of(elem).clone()),
            _ => None,
        }
    }
}
