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

/// What the runtime's stored handler answers when a dispatch shim calls it.
///
/// Win64 returns a `Result` carrier through xmm0 rather than the general
/// register pair, and the backends emit that thunk only for a shim named
/// in `gossamer_abi::I128_HANDLER_REGISTRATIONS`. A handler that answers
/// nothing needs no such arrangement.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HandlerReturn {
    Carrier,
    Unit,
}

use super::*;

use super::Builder;

impl<'a> Builder<'a> {
    pub(crate) fn emit_single_arg_call(
        &mut self,
        name: &'static str,
        receiver: Local,
        ret_ty: Ty,
        span: Span,
    ) -> Local {
        let dest = self.fresh(ret_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name.to_string())),
            args: vec![Operand::Copy(Place::local(receiver))],
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        dest
    }

    /// `param` with the generic arguments the call site instantiated
    /// `callee_ty` with substituted into it.
    ///
    /// A callee's declared parameter types name its own type parameters, so
    /// a `Fn(T) -> U` parameter reaches the argument lowering as the
    /// template wrote it. Anything that reads register classes off that
    /// signature needs the types the call site actually passes.
    pub(crate) fn instantiate_param_ty(&mut self, callee_ty: Ty, param: Ty) -> Ty {
        let gossamer_types::TyKind::FnDef { substs, .. } = self.tcx.kind_of(callee_ty).clone()
        else {
            return param;
        };
        if substs.as_slice().is_empty() {
            return param;
        }
        let subst_tys = crate::monomorph::subst_type_arguments(&substs);
        crate::monomorph::subst_param_ty(self.tcx, param, &subst_tys)
    }

    pub(crate) fn coerce_to_fn_trait_if_needed(
        &mut self,
        source_local: Local,
        expected: Ty,
        span: Span,
    ) -> Local {
        use gossamer_types::TyKind;
        let expected_kind = self.tcx.kind_of(expected).clone();
        let sig_opt = match &expected_kind {
            TyKind::FnTrait(sig) | TyKind::FnPtr(sig) => Some(sig.clone()),
            _ => return source_local,
        };
        let Some(sig) = sig_opt else {
            return source_local;
        };
        let source_ty = self.locals[source_local.0 as usize].ty;
        let source_kind = self.tcx.kind_of(source_ty);
        // Wrap when the source is a genuine fn item (`FnDef`)
        // OR a local that the MIR builder marked as holding a
        // function-name string constant (the lift-closures pass
        // produces these for non-capturing closures: the local
        // ends up Copy(Const(Str("__closure_N"))) - a rodata
        // pointer, NOT a callable address). FnPtr-typed locals
        // are already env_ptr-shaped after this round of fixes,
        // so re-wrapping them would double-indirect; FnTrait and
        // Closure values are env-shaped by construction.
        let names_a_fn = self.local_fn_name.contains_key(&source_local);
        let needs_wrap = matches!(source_kind, TyKind::FnDef { .. }) || names_a_fn;
        if !needs_wrap {
            return source_local;
        }
        let env_ty = expected;
        // Only the environment carries the callable type, so that type says an
        // environment is owned here and the drop passes reclaim it. The sizes,
        // offsets, thunk address, and store sinks around it are plain words.
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let unit_ty = self.tcx.unit();
        // Allocate the env blob: 16 bytes (thunk ptr + real fn ptr). It is
        // reference-counted like a capturing closure's, so every callable
        // value reaches its owner in one shape and one predicate decides who
        // reclaims it. The blob holds two code addresses and no managed
        // child, hence the empty meta symbol.
        let size_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(size_local),
            Rvalue::Use(Operand::Const(ConstValue::Int(16))),
            span,
        );
        let env_local = self.fresh(env_ty);
        self.emit_assign(
            Place::local(env_local),
            Rvalue::CallIntrinsic {
                name: "gos_rc_alloc",
                args: vec![
                    Operand::Copy(Place::local(size_local)),
                    Operand::Const(ConstValue::Str(String::new())),
                ],
            },
            span,
        );
        // Resolve the per-shape thunk name. Encodes input + return
        // types so the backend can synthesize a thunk with the
        // correct calling convention regardless of arg / ret types.
        let thunk_name = mangle_callable_shape(self.tcx, &sig);
        let tramp_addr_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(tramp_addr_local),
            Rvalue::CallIntrinsic {
                name: "gos_fn_addr",
                args: vec![Operand::Const(ConstValue::Str(thunk_name))],
            },
            span,
        );
        let zero_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(zero_local),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        let sink_a = self.fresh(unit_ty);
        self.emit_assign(
            Place::local(sink_a),
            Rvalue::CallIntrinsic {
                name: "gos_store",
                args: vec![
                    Operand::Copy(Place::local(env_local)),
                    Operand::Copy(Place::local(zero_local)),
                    Operand::Copy(Place::local(tramp_addr_local)),
                ],
            },
            span,
        );
        let eight_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(eight_local),
            Rvalue::Use(Operand::Const(ConstValue::Int(8))),
            span,
        );
        // When the source local was bound to a fn name via
        // `let c = some_fn_name` (e.g. a lifted non-capturing
        // closure), its slot holds the address of the *string*
        // (the way the MIR encodes a `def: None` path), not the
        // function. Resolve to the real fn address via
        // `gos_fn_addr` so the trampoline forwards to the actual
        // code. Direct fn references (FnDef/FnPtr-typed locals)
        // already hold the right value.
        let real_fn_operand = if let Some(name) = self.local_fn_name.get(&source_local).cloned() {
            let name = self.callable_fn_value_symbol(&name, &sig);
            let addr_local = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(addr_local),
                Rvalue::CallIntrinsic {
                    name: "gos_fn_addr",
                    args: vec![Operand::Const(ConstValue::Str(name))],
                },
                span,
            );
            Operand::Copy(Place::local(addr_local))
        } else {
            Operand::Copy(Place::local(source_local))
        };
        let sink_b = self.fresh(unit_ty);
        self.emit_assign(
            Place::local(sink_b),
            Rvalue::CallIntrinsic {
                name: "gos_store",
                args: vec![
                    Operand::Copy(Place::local(env_local)),
                    Operand::Copy(Place::local(eight_local)),
                    real_fn_operand,
                ],
            },
            span,
        );
        env_local
    }

    fn callable_fn_value_symbol(&self, name: &str, sig: &gossamer_types::FnSig) -> String {
        let canonical = name.strip_prefix("std::").unwrap_or(name);
        if matches!(canonical, "println" | "fmt::println")
            && sig.inputs.len() == 1
            && matches!(
                self.tcx.kind_of(sig.output),
                gossamer_types::TyKind::Int(_) | gossamer_types::TyKind::Unit
            )
        {
            return match self.tcx.kind_of(sig.inputs[0]) {
                gossamer_types::TyKind::Float(_) => "gos_rt_println_fn_f64".to_string(),
                gossamer_types::TyKind::String => "gos_rt_println_fn_str_word".to_string(),
                _ => "gos_rt_println_fn_i64".to_string(),
            };
        }
        name.to_string()
    }

    /// Lowers `spawn(f) -> JoinHandle<T>`. The callable `f` is coerced
    /// into the closure env-blob shape (`[code_ptr, captures…]`); the
    /// entry address sits at offset 0 and the blob itself is the env
    /// passed as the implicit first argument. The runtime helper runs
    /// it on a goroutine and threads the outcome back through the
    /// handle's `gos_rt_join`.
    pub(crate) fn lower_spawn(
        &mut self,
        f_expr: &HirExpr,
        reason: Option<&HirExpr>,
        span: Span,
    ) -> Option<Local> {
        use gossamer_types::TyKind;
        // The spawned closure's captures escape to the new goroutine:
        // switch any RC-managed capture to atomic reference counting
        // before it's copied into the env. Captures are free-variable
        // expressions (paths), so re-lowering them here is idempotent.
        if let gossamer_hir::HirExprKind::LiftedClosure { captures, .. } = &f_expr.kind {
            for cap in captures.clone() {
                if let Some(c) = self.lower_expr(&cap) {
                    self.emit_mark_shared_if_rc(c, span);
                }
            }
        }
        let f_local = self.lower_expr(f_expr)?;
        let f_ty = self.locals[f_local.0 as usize].ty;
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        // The handle's element type is the callable's return type when
        // it is statically known; otherwise default to i64 (the raw
        // 8-byte slot the runtime carries).
        let elem = match self.tcx.kind_of(f_ty).clone() {
            TyKind::FnTrait(sig) | TyKind::FnPtr(sig) => sig.output,
            _ => i64_ty,
        };
        // Coerce a bare fn / non-capturing closure into the env-blob
        // shape so `code = load(env+0)` is uniform; capturing closures
        // are already env-shaped and pass through unchanged.
        let fn_trait_ty = self.tcx.intern(TyKind::FnTrait(gossamer_types::FnSig {
            inputs: Vec::new(),
            output: elem,
        }));
        let env_local = self.coerce_to_fn_trait_if_needed(f_local, fn_trait_ty, span);
        // The environment reaches a second goroutine, so its count becomes
        // atomic and the child takes a share of its own. The child gives that
        // share back however it leaves, which is what lets the spawning frame
        // release its own share at the end of the scope that built it - a loop
        // that spawns per iteration then holds one environment, not all of them.
        self.emit_mark_shared_if_rc(env_local, span);
        let unit_ty = self.tcx.unit();
        let child_share = self.fresh(unit_ty);
        self.emit_assign(
            Place::local(child_share),
            Rvalue::CallIntrinsic {
                name: "gos_rt_rc_retain",
                args: vec![Operand::Copy(Place::local(env_local))],
            },
            span,
        );
        let code_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(code_local),
            Rvalue::CallIntrinsic {
                name: "gos_load",
                args: vec![
                    Operand::Copy(Place::local(env_local)),
                    Operand::Const(ConstValue::Int(0)),
                ],
            },
            span,
        );
        let handle_ty = self.tcx.intern(TyKind::JoinHandle(elem));
        let dest = self.fresh(handle_ty);
        let next = self.new_block(span);
        // How the callable's return crosses the spawn boundary. A
        // `Result` or an `Option` comes back in two registers, so the
        // runtime has to read both and copy them into a cell; reading
        // one word would keep the discriminant and drop the payload.
        let (ret_words, err_kind) = self.spawn_return_shape(elem);
        let ret_words_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(ret_words_local),
            Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(ret_words)))),
            span,
        );
        let err_kind_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(err_kind_local),
            Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(err_kind)))),
            span,
        );
        // The `reason:` label, or the empty string when the spawn carried
        // none. The cohort reads it on this goroutine, before the child is
        // queued, and names the child by it in its reports.
        let reason_operand = match reason {
            Some(expr) => match self.lower_expr(expr) {
                Some(local) => Operand::Copy(Place::local(local)),
                None => Operand::Const(ConstValue::Str(String::new())),
            },
            None => Operand::Const(ConstValue::Str(String::new())),
        };
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_spawn_ex".to_string())),
            args: vec![
                Operand::Copy(Place::local(code_local)),
                Operand::Copy(Place::local(env_local)),
                Operand::Copy(Place::local(ret_words_local)),
                Operand::Copy(Place::local(err_kind_local)),
                reason_operand,
            ],
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        Some(dest)
    }

    /// `httptest::record(handler, method, path, body)` - calls a handler
    /// with a request built in memory. Same handler dispatch as
    /// [`Self::lower_http_serve`], with the request's parts in place of an
    /// address and no listener at all.
    pub(crate) fn lower_httptest_record(
        &mut self,
        handler_expr: &HirExpr,
        method_expr: &HirExpr,
        path_expr: &HirExpr,
        body_expr: &HirExpr,
        span: Span,
    ) -> Option<Local> {
        let method_local = self.lower_expr(method_expr)?;
        let path_local = self.lower_expr(path_expr)?;
        let body_local = self.lower_expr(body_expr)?;
        let handler_local = self.lower_expr(handler_expr)?;
        let fn_addr_local = self.handler_fn_addr_local(handler_local, "serve", span)?;
        let result_ty = self.result_response_error_adt_ty();
        let dest = self.terminate_handler_dispatch(
            "gos_rt_httptest_record",
            &[method_local, path_local, body_local],
            handler_local,
            fn_addr_local,
            HandlerReturn::Carrier,
            result_ty,
            span,
        );
        Some(dest)
    }

    /// `server.serve(handler)` - the `http::Server` counterpart of
    /// [`Self::lower_http_serve`]. The server carries its own listener and
    /// limits, so only the handler's environment and dispatch address
    /// cross with it.
    pub(crate) fn lower_http_server_serve(
        &mut self,
        server_local: Local,
        handler_expr: &HirExpr,
        span: Span,
    ) -> Option<Local> {
        let handler_local = self.lower_expr(handler_expr)?;
        let fn_addr_local = self.handler_fn_addr_local(handler_local, "serve", span)?;
        let result_ty = self.result_unit_error_adt_ty();
        let dest = self.terminate_handler_dispatch(
            "gos_rt_http_server_serve",
            &[server_local],
            handler_local,
            fn_addr_local,
            HandlerReturn::Carrier,
            result_ty,
            span,
        );
        Some(dest)
    }

    pub(crate) fn lower_http_serve(
        &mut self,
        addr_expr: &HirExpr,
        handler_expr: &HirExpr,
        _ty: Ty,
        span: Span,
    ) -> Option<Local> {
        let addr_local = self.lower_expr(addr_expr)?;
        let handler_local = self.lower_expr(handler_expr)?;
        let fn_addr_local = self.handler_fn_addr_local(handler_local, "serve", span)?;
        // The runtime shim returns the packed `Result<(), Error>`
        // directly: `Err` on bind failure, `Ok(())` when the accept
        // loop exits. Typing the destination as the Result ADT keeps
        // `match http::serve(..) { Err(e) => println!("{}", e) }`
        // lowering with a DynError-typed `e` instead of a void
        // binding (the LLVM `sext void` regression).
        let result_ty = self.result_unit_error_adt_ty();
        let dest = self.terminate_handler_dispatch(
            "gos_rt_http_serve",
            &[addr_local],
            handler_local,
            fn_addr_local,
            HandlerReturn::Carrier,
            result_ty,
            span,
        );
        Some(dest)
    }

    /// `http::serve_tls(addr, cert_pem, key_pem, handler)` - TLS variant
    /// of [`Self::lower_http_serve`]. Threads the certificate and key
    /// PEM ahead of the handler env + serve-method address so the
    /// runtime terminates TLS before dispatching back into Gossamer.
    pub(crate) fn lower_http_serve_tls(
        &mut self,
        addr_expr: &HirExpr,
        cert_expr: &HirExpr,
        key_expr: &HirExpr,
        handler_expr: &HirExpr,
        _ty: Ty,
        span: Span,
    ) -> Option<Local> {
        let addr_local = self.lower_expr(addr_expr)?;
        let cert_local = self.lower_expr(cert_expr)?;
        let key_local = self.lower_expr(key_expr)?;
        let handler_local = self.lower_expr(handler_expr)?;
        let fn_addr_local = self.handler_fn_addr_local(handler_local, "serve", span)?;
        let result_ty = self.result_unit_error_adt_ty();
        let dest = self.terminate_handler_dispatch(
            "gos_rt_http_serve_tls",
            &[addr_local, cert_local, key_local],
            handler_local,
            fn_addr_local,
            HandlerReturn::Carrier,
            result_ty,
            span,
        );
        Some(dest)
    }

    /// Terminates the current block with a handler-dispatch call to
    /// `symbol`: the `leading` arguments, then the handler's environment
    /// and its dispatch address, answering the call's destination local.
    ///
    /// The address argument's position is what a backend keys the Win64
    /// carrier-return thunk on, so a dispatch whose handler answers a
    /// `Result` must be listed in [`gossamer_abi::I128_HANDLER_REGISTRATIONS`]
    /// at exactly the position this builds.
    fn terminate_handler_dispatch(
        &mut self,
        symbol: &str,
        leading: &[Local],
        handler_local: Local,
        fn_addr_local: Local,
        returns: HandlerReturn,
        result_ty: Ty,
        span: Span,
    ) -> Local {
        let addr_index = leading.len() + 1;
        debug_assert!(
            returns == HandlerReturn::Unit
                || gossamer_abi::I128_HANDLER_REGISTRATIONS.contains(&(symbol, addr_index)),
            "{symbol} dispatches a carrier-returning handler, so it belongs in \
             I128_HANDLER_REGISTRATIONS at address index {addr_index}"
        );
        let mut args: Vec<Operand> = leading
            .iter()
            .map(|local| Operand::Copy(Place::local(*local)))
            .collect();
        args.push(Operand::Copy(Place::local(handler_local)));
        args.push(Operand::Copy(Place::local(fn_addr_local)));
        let dest = self.fresh(result_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str(symbol.to_string())),
            args,
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        dest
    }

    /// Resolves a handler value's dispatch method (`method`) to a
    /// `gos_fn_addr` local. If the handler is a stateful runtime type
    /// (Router / FileServer / Proxy / Middleware), its `serve` lives in
    /// gossamer-runtime; otherwise the user-defined `{T}::{method}` is
    /// looked up. Shared by the HTTP serve lowerings (`serve`) and the
    /// WebSocket serve lowering (`handle`).
    fn handler_fn_addr_local(
        &mut self,
        handler_local: Local,
        method: &str,
        span: Span,
    ) -> Option<Local> {
        // A handler built here carries its construction tag; one arriving
        // as a parameter - the shape `go run(server, app)` takes - carries
        // none, so its named type answers instead. Without that a `Router`
        // passed in reached a struct-method symbol nothing declares.
        let handler_ty = self.locals[handler_local.0 as usize].ty;
        let handler_runtime_kind = self
            .local_runtime_kind
            .get(&handler_local)
            .copied()
            .or_else(|| self.runtime_kind_from_ty(handler_ty));
        let serve_fn_name = match handler_runtime_kind {
            Some("http::Router") => "gos_rt_router_serve".to_string(),
            Some("http::FileServer") => "gos_rt_file_server_serve".to_string(),
            Some("http::Proxy") => "gos_rt_proxy_forward".to_string(),
            Some("http::Middleware") => "gos_rt_middleware_serve".to_string(),
            _ => {
                // A plain `fn` is a handler in its own right. It carries no
                // environment, so it dispatches through its synthesized
                // env-ignoring thunk and the runtime keeps one call shape for
                // every handler form. A closure is already env-shaped: the
                // handler value IS its environment.
                if let Some(fn_name) = self.local_fn_name.get(&handler_local).cloned() {
                    crate::lower::helpers::handler_env_wrap_name(&fn_name)
                } else if let Some(closure_name) = self.local_closure.get(&handler_local).cloned() {
                    self.handler_dispatch_symbol(closure_name)
                } else {
                    let handler_ty = self.locals[handler_local.0 as usize].ty;
                    let handler_struct = self.struct_name_of(handler_ty)?;
                    self.handler_dispatch_symbol(format!("{handler_struct}::{method}"))
                }
            }
        };
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let fn_addr_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(fn_addr_local),
            Rvalue::CallIntrinsic {
                name: "gos_fn_addr",
                args: vec![Operand::Const(ConstValue::Str(serve_fn_name))],
            },
            span,
        );
        Some(fn_addr_local)
    }

    /// `http_h3::serve(addr, cert_path, key_path, handler)` - HTTP/3
    /// variant of [`Self::lower_http_serve`]. Threads the TLS keypair
    /// file paths ahead of the handler env + serve-method address.
    pub(crate) fn lower_http3_serve(
        &mut self,
        addr_expr: &HirExpr,
        cert_expr: &HirExpr,
        key_expr: &HirExpr,
        handler_expr: &HirExpr,
        _ty: Ty,
        span: Span,
    ) -> Option<Local> {
        let addr_local = self.lower_expr(addr_expr)?;
        let cert_local = self.lower_expr(cert_expr)?;
        let key_local = self.lower_expr(key_expr)?;
        let handler_local = self.lower_expr(handler_expr)?;
        let fn_addr_local = self.handler_fn_addr_local(handler_local, "serve", span)?;
        let result_ty = self.result_unit_error_adt_ty();
        let dest = self.terminate_handler_dispatch(
            "gos_rt_http3_serve",
            &[addr_local, cert_local, key_local],
            handler_local,
            fn_addr_local,
            HandlerReturn::Carrier,
            result_ty,
            span,
        );
        Some(dest)
    }

    /// `websocket::serve(addr, handler)` - binds a WebSocket listener and
    /// dispatches each upgraded connection to the handler's
    /// `handle(&self, ws: i64)` method. Mirrors [`Self::lower_http_serve`]:
    /// passes the handler env + `gos_fn_addr("{T}::handle")` so the
    /// runtime can call back into Gossamer code per connection.
    pub(crate) fn lower_websocket_serve(
        &mut self,
        addr_expr: &HirExpr,
        handler_expr: &HirExpr,
        _ty: Ty,
        span: Span,
    ) -> Option<Local> {
        let addr_local = self.lower_expr(addr_expr)?;
        let handler_local = self.lower_expr(handler_expr)?;
        let fn_addr_local = self.handler_fn_addr_local(handler_local, "handle", span)?;
        let result_ty = self.result_unit_error_adt_ty();
        let dest = self.terminate_handler_dispatch(
            "gos_rt_ws_serve",
            &[addr_local],
            handler_local,
            fn_addr_local,
            HandlerReturn::Unit,
            result_ty,
            span,
        );
        Some(dest)
    }

    /// `sql::register_native(name, driver)`: register a Gossamer-
    /// implemented SQL driver. Captures the driver value's env local +
    /// `gos_fn_addr("<Type>::dispatch")` and emits
    /// `gos_rt_sql_register_native(name_ptr, env, fn_addr)`, which
    /// builds a `GossamerDriver` and registers it in `crate::sql`. The
    /// driver struct is stateless (a ZST whose `dispatch` never reads
    /// `self`), so the env pointer dangling after the call returns is
    /// harmless. Return type is unit.
    pub(crate) fn lower_sql_register_native(
        &mut self,
        name_expr: &HirExpr,
        driver_expr: &HirExpr,
        _ty: Ty,
        span: Span,
    ) -> Option<Local> {
        let name_local = self.lower_expr(name_expr)?;
        let driver_local = self.lower_expr(driver_expr)?;
        let driver_ty = self.locals[driver_local.0 as usize].ty;
        let driver_struct = self.struct_name_of(driver_ty)?;
        let dispatch_fn_name = self.handler_dispatch_symbol(format!("{driver_struct}::dispatch"));
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let fn_addr_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(fn_addr_local),
            Rvalue::CallIntrinsic {
                name: "gos_fn_addr",
                args: vec![Operand::Const(ConstValue::Str(dispatch_fn_name))],
            },
            span,
        );
        let unit_ty = self.tcx.unit();
        let dest = self.fresh(unit_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_sql_register_native".to_string())),
            args: vec![
                Operand::Copy(Place::local(name_local)),
                Operand::Copy(Place::local(driver_local)),
                Operand::Copy(Place::local(fn_addr_local)),
            ],
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        Some(dest)
    }

    pub(crate) fn lower_http2_bind_and_run_h2c(
        &mut self,
        addr_expr: &HirExpr,
        handler_expr: &HirExpr,
        _ty: Ty,
        span: Span,
    ) -> Option<Local> {
        let addr_local = self.lower_expr(addr_expr)?;
        let handler_local = self.lower_expr(handler_expr)?;
        let handler_ty = self.locals[handler_local.0 as usize].ty;
        let handler_struct = self.struct_name_of(handler_ty)?;
        let serve_fn_name = self.handler_dispatch_symbol(format!("{handler_struct}::serve"));
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let fn_addr_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(fn_addr_local),
            Rvalue::CallIntrinsic {
                name: "gos_fn_addr",
                args: vec![Operand::Const(ConstValue::Str(serve_fn_name))],
            },
            span,
        );
        // Same Result-typed destination shape as `lower_http_serve`.
        let result_ty = self.result_unit_error_adt_ty();
        let dest = self.fresh(result_ty);
        let next = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_http2_bind_and_run_h2c".to_string())),
            args: vec![
                Operand::Copy(Place::local(addr_local)),
                Operand::Copy(Place::local(handler_local)),
                Operand::Copy(Place::local(fn_addr_local)),
            ],
            destination: Place::local(dest),
            target: Some(next),
        });
        self.set_current(next);
        Some(dest)
    }

    pub(crate) fn lower_flag_define(
        &mut self,
        name_expr: &HirExpr,
        specs_expr: &HirExpr,
        _ty: Ty,
        span: Span,
    ) -> Option<Local> {
        // Only handle the inline-array literal form. Dynamic specs
        // would need real runtime support - fall through to the
        // generic call dispatch (which produces a stub) so the rest
        // of the program still compiles.
        let HirExprKind::Array(gossamer_hir::HirArrayExpr::List(specs)) = &specs_expr.kind else {
            return None;
        };

        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);

        // Step 1: `flag::Set::new(name)` -> set_local.
        let name_local = self.lower_expr(name_expr)?;
        let set_local = self.fresh(i64_ty);
        let after_new = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_flag_set_new".to_string())),
            args: vec![Operand::Copy(Place::local(name_local))],
            destination: Place::local(set_local),
            target: Some(after_new),
        });
        self.set_current(after_new);
        self.local_runtime_kind.insert(set_local, "flag::Set");

        // Step 2: walk each spec, register it on the set, and stash
        // the resulting cell local + kind for the aggregate that
        // becomes the return value.
        let mut layout: Vec<(String, &'static str)> = Vec::with_capacity(specs.len());
        let mut cell_locals: Vec<Local> = Vec::with_capacity(specs.len());
        for spec in specs {
            let HirExprKind::Call {
                callee: spec_callee,
                args: spec_args,
            } = &spec.kind
            else {
                return None;
            };
            let HirExprKind::Path {
                segments: spec_segs,
                ..
            } = &spec_callee.kind
            else {
                return None;
            };
            let spec_path: String = spec_segs
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join("::");
            let (rt_helper, cell_kind, default_ty): (&'static str, &'static str, Ty) =
                match spec_path.as_str() {
                    "flag::int" => ("gos_rt_flag_set_int", "flag::Cell::Int", i64_ty),
                    "flag::string" => (
                        "gos_rt_flag_set_string",
                        "flag::Cell::String",
                        self.tcx.string_ty(),
                    ),
                    "flag::bool" => (
                        "gos_rt_flag_set_bool",
                        "flag::Cell::Bool",
                        self.tcx.bool_ty(),
                    ),
                    _ => return None,
                };
            // spec args: (long, default, help, short)
            if spec_args.len() < 3 {
                return None;
            }
            let long_local = self.lower_expr(&spec_args[0])?;
            let default_local = self.lower_expr(&spec_args[1])?;
            let help_local = self.lower_expr(&spec_args[2])?;
            let short_expr = spec_args.get(3);

            // Recover the long-name string for the layout table. Only
            // string-literal long names are supported; dynamic names
            // would have to be looked up at runtime, defeating the
            // purpose of a static field map.
            let long_name = match &spec_args[0].kind {
                HirExprKind::Literal(gossamer_hir::HirLiteral::String(s)) => s.clone(),
                _ => return None,
            };

            // Coerce the default operand's type so cranelift's
            // signature for the runtime helper picks the right ABI
            // slot (string ptr / i64 / i8). The lowered local already
            // has the literal's type from `lower_expr`; the helper
            // still wants its own pinned ABI.
            let _ = default_ty;

            let cell_local = self.fresh(i64_ty);
            let after_reg = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str(rt_helper.to_string())),
                args: vec![
                    Operand::Copy(Place::local(set_local)),
                    Operand::Copy(Place::local(long_local)),
                    Operand::Copy(Place::local(default_local)),
                    Operand::Copy(Place::local(help_local)),
                ],
                destination: Place::local(cell_local),
                target: Some(after_reg),
            });
            self.set_current(after_reg);
            self.local_runtime_kind.insert(cell_local, cell_kind);

            // Optional short alias: `flag::int("count", ..., 'c')`.
            if let Some(short_arg) = short_expr {
                if let HirExprKind::Literal(gossamer_hir::HirLiteral::Char(ch)) = &short_arg.kind {
                    let short_local = self.fresh(i64_ty);
                    self.emit_assign(
                        Place::local(short_local),
                        Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(*ch as u32)))),
                        span,
                    );
                    let unit_ty = self.tcx.unit();
                    let unit_dest = self.fresh(unit_ty);
                    let after_short = self.new_block(span);
                    self.terminate(Terminator::Call {
                        callee: Operand::Const(ConstValue::Str(
                            "gos_rt_flag_set_short".to_string(),
                        )),
                        args: vec![
                            Operand::Copy(Place::local(set_local)),
                            Operand::Copy(Place::local(short_local)),
                        ],
                        destination: Place::local(unit_dest),
                        target: Some(after_short),
                    });
                    self.set_current(after_short);
                }
            }

            layout.push((long_name, cell_kind));
            cell_locals.push(cell_local);
        }

        // Step 3: `set.parse(os::args())`. `gos_rt_os_args` returns
        // a pointer the parser recognises as the `argv + 1`
        // sentinel.
        let args_local = self.fresh(i64_ty);
        let after_args = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_os_args".to_string())),
            args: vec![],
            destination: Place::local(args_local),
            target: Some(after_args),
        });
        self.set_current(after_args);
        let parse_dest = self.fresh(i64_ty);
        let after_parse = self.new_block(span);
        self.terminate(Terminator::Call {
            callee: Operand::Const(ConstValue::Str("gos_rt_flag_set_parse".to_string())),
            args: vec![
                Operand::Copy(Place::local(set_local)),
                Operand::Copy(Place::local(args_local)),
            ],
            destination: Place::local(parse_dest),
            target: Some(after_parse),
        });
        self.set_current(after_parse);

        // Step 4: build an aggregate of cell pointers as the result.
        // The aggregate's local picks up the layout table so field
        // access dispatches positionally. Each cell is i64-shaped
        // (a pointer) for codegen purposes.
        let agg_ty = self.tcx.intern(gossamer_types::TyKind::Tuple(
            (0..cell_locals.len()).map(|_| i64_ty).collect(),
        ));
        let dest = self.fresh(agg_ty);
        let operands: Vec<Operand> = cell_locals
            .iter()
            .map(|l| Operand::Copy(Place::local(*l)))
            .collect();
        self.emit_assign(
            Place::local(dest),
            Rvalue::Aggregate {
                kind: crate::AggregateKind::Tuple,
                operands,
            },
            span,
        );
        self.local_runtime_kind.insert(dest, "flag::DefineResult");
        self.local_define_layout.insert(dest, layout);
        Some(dest)
    }

    pub(crate) fn lower_result_no_payload(
        &mut self,
        disc: i64,
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let disc_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(disc_local),
            Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(disc)))),
            span,
        );
        let zero_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(zero_local),
            Rvalue::Use(Operand::Const(ConstValue::Int(0))),
            span,
        );
        // Pin the dest to the i128 Result/Option representation even
        // when inference left `ty` an unresolved `Var` (else the
        // i128 truncates through a `ptr` slot) - mirrors
        // `lower_result_ctor`.
        let ty = self.result_repr_ty(ty);
        let dest = self.fresh(ty);
        self.emit_assign(
            Place::local(dest),
            Rvalue::CallIntrinsic {
                name: "gos_rt_result_new",
                args: vec![
                    Operand::Copy(Place::local(disc_local)),
                    Operand::Copy(Place::local(zero_local)),
                ],
            },
            span,
        );
        Some(dest)
    }

    /// True when an enum-variant payload of type `ty` is a multi-slot
    /// by-value aggregate (struct / tuple / fixed array > 1 word) that must
    /// be heap-boxed to fit the one-word payload slot. Sentinel ADTs
    /// (`Option` / `Result`, `u32::MAX` / `- 1`), opaque handles, and
    /// inline-able enums are by-value or single-pointer values handled
    /// elsewhere and are never boxed here.
    /// Materializes a fixed-array payload argument into a heap GosVec when
    /// the variant's declared field type is `Vec<T>` / `[T]`. An
    /// unannotated array-literal local (`let inner = [1, 2, 3]`) infers as
    /// `[T; N]`; passed to an enum constructor it would otherwise be boxed
    /// as a raw aggregate blob whose bytes are then misread as a GosVec
    /// header when the variant is iterated - garbage length, then a fault.
    /// A direct array-literal argument already lowers as a GosVec because
    /// its type is inferred in the field's `Vec`/`Slice` context.
    fn coerce_enum_payload_array(
        &mut self,
        payload: Local,
        expected: Option<Ty>,
        span: Span,
    ) -> Local {
        use gossamer_types::TyKind;
        let Some(expected) = expected else {
            return payload;
        };
        let peel = |b: &Self, t: Ty| match b.tcx.kind_of(t) {
            TyKind::Ref { inner, .. } => *inner,
            _ => t,
        };
        if !matches!(
            self.tcx.kind_of(peel(self, expected)),
            TyKind::Vec(_) | TyKind::Slice(_)
        ) {
            return payload;
        }
        let payload_inner = peel(self, self.locals[payload.0 as usize].ty);
        if let TyKind::Array { elem, len } = self.tcx.kind_of(payload_inner).clone() {
            return self.coerce_array_to_vec(payload, elem, len, span);
        }
        payload
    }

    /// How a spawned callable's return type crosses the spawn boundary:
    /// how many registers it occupies, and - when it is a `Result` -
    /// what its `Err` payload is, so a cohort can name a failed child's
    /// error. `Option` reports two words and no error kind: its `None`
    /// shares `Err`'s discriminant but is not a failure.
    fn spawn_return_shape(&mut self, elem: Ty) -> (i64, i64) {
        use gossamer_types::TyKind;
        const ERR_KIND_NONE: i64 = 0;
        const ERR_KIND_ERROR: i64 = 1;
        const ERR_KIND_STRING: i64 = 2;
        const ERR_KIND_OTHER: i64 = 3;
        let TyKind::Adt { def, substs } = self.tcx.kind_of(elem).clone() else {
            return (1, ERR_KIND_NONE);
        };
        // Option is the sentinel one below Result; both are two-word.
        if def.local == u32::MAX - 1 {
            return (2, ERR_KIND_NONE);
        }
        if def.local != u32::MAX {
            return (1, ERR_KIND_NONE);
        }
        let err_kind = match substs.types().get(1).map(|ty| self.tcx.kind_of(*ty)) {
            Some(TyKind::DynError) => ERR_KIND_ERROR,
            Some(TyKind::String) => ERR_KIND_STRING,
            _ => ERR_KIND_OTHER,
        };
        (2, err_kind)
    }

    pub(crate) fn is_boxable_aggregate_payload(&self, ty: Ty) -> bool {
        use gossamer_types::TyKind;
        let aggregate = match self.tcx.kind_of(ty) {
            TyKind::Adt { def, .. } => def.local < u32::MAX - 16 && !self.tcx.is_inline_enum_ty(ty),
            TyKind::Tuple(_) | TyKind::Array { .. } => true,
            _ => false,
        };
        // A single-slot aggregate fits the payload word and is stored in it,
        // unless that word is a counted handle: the node owns a share of what
        // it holds, and the box is what carries the child meta saying so.
        aggregate && (self.type_slot_bytes(ty) > 8 || !self.aggr_child_entries(ty).is_empty())
    }

    /// Boxes the multi-slot aggregate in `payload_local` into an RC cell and
    /// returns a fresh local holding the box pointer. The box carries the
    /// aggregate's `RC_KIND_STRUCT` child-word meta (so its release reclaims
    /// `String` / nested-node children) and retains those children at copy
    /// time (so they outlive the source aggregate's scope-end teardown).
    fn box_aggregate_payload(&mut self, payload_local: Local, agg_ty: Ty, span: Span) -> Local {
        self.tcx.register_boxed_enum_payload(agg_ty);
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let size_bytes = i128::from(self.type_slot_bytes(agg_ty));
        let meta_sym = self.ensure_aggr_struct_meta(agg_ty).unwrap_or_default();
        let size_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(size_local),
            Rvalue::Use(Operand::Const(ConstValue::Int(size_bytes))),
            span,
        );
        let boxed = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(boxed),
            Rvalue::CallIntrinsic {
                name: "gos_rt_enum_box_aggr",
                args: vec![
                    Operand::Copy(Place::local(size_local)),
                    Operand::Const(ConstValue::Str(meta_sym)),
                    Operand::Copy(Place::local(payload_local)),
                ],
            },
            span,
        );
        boxed
    }

    pub(crate) fn lower_user_enum_ctor(
        &mut self,
        enum_name: &str,
        variant_idx: u32,
        args: &[HirExpr],
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        let mut payload = Vec::with_capacity(args.len());
        for arg in args {
            payload.push(self.lower_expr(arg)?);
        }
        self.lower_user_enum_ctor_from_locals(enum_name, variant_idx, &payload, ty, span)
    }

    /// Builds one variant of a user enum from payload values already lowered
    /// into locals. The expression form above is this with its arguments
    /// lowered first; a caller that computes a payload at run time - a Rust
    /// binding's declared arm set, decoded into the enum it names - reaches
    /// the same construction here.
    pub(crate) fn lower_user_enum_ctor_from_locals(
        &mut self,
        enum_name: &str,
        variant_idx: u32,
        payload: &[Local],
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let n_args = payload.len();

        // Declared field types of this variant (tuple and struct payloads
        // both record theirs), used to materialize a fixed-array argument
        // into a heap GosVec when the field is `Vec<T>` / `[T]`, and to place
        // each field within the node's slot slab.
        let field_tys: Vec<Ty> = self
            .enums
            .by_enum
            .get(enum_name)
            .and_then(|vs| vs.get(variant_idx as usize))
            .and_then(|vname| self.enums.field_tys_of(enum_name, vname))
            .unwrap_or_default();
        let payload_offsets = self.variant_payload_offsets(&field_tys, n_args);
        // Payload bytes only: the discriminant lives in the RC header byte.
        let bytes = i128::from(self.variant_payload_bytes(&field_tys, n_args));

        // Inline-able enums (every variant <=1 field that fits in 8 bytes) use
        // the 2-word by-value `i128` [disc, payload] representation - pack the
        // discriminant and the single field inline, no heap node. Mirrors the
        // Result/Option lowering and eliminates the per-node allocation storm
        // for JSON-DOM-shaped enums.
        if self.tcx.is_inline_enum_ty(ty) {
            let disc_local = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(disc_local),
                Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(variant_idx)))),
                span,
            );
            let (payload_local, is_f64) = if let Some(first) = payload.first().copied() {
                let p = self.coerce_enum_payload_array(first, field_tys.first().copied(), span);
                let pty = self.locals[p.0 as usize].ty;
                let is_f64 = matches!(self.tcx.kind_of(pty), gossamer_types::TyKind::Float(_));
                (p, is_f64)
            } else {
                let p = self.fresh(i64_ty);
                self.emit_assign(
                    Place::local(p),
                    Rvalue::Use(Operand::Const(ConstValue::Int(0))),
                    span,
                );
                (p, false)
            };
            let ctor = if is_f64 {
                "gos_rt_result_new_f64"
            } else {
                "gos_rt_result_new"
            };
            let dest = self.fresh(ty);
            self.emit_assign(
                Place::local(dest),
                Rvalue::CallIntrinsic {
                    name: ctor,
                    args: vec![
                        Operand::Copy(Place::local(disc_local)),
                        Operand::Copy(Place::local(payload_local)),
                    ],
                },
                span,
            );
            return Some(dest);
        }

        // Every heap-allocated user enum is reference counted. Record
        // this construction's type so the drop pass recognises locals of
        // it as RC-managed (the enum's HIR `self_ty` is a placeholder, so
        // this is the only reliable source of the real `Adt` handle).
        self.tcx.register_rc_managed_ty(ty);

        // Payload-less variants (e.g. `Tree::Leaf`) carry only a discriminant
        // and are never mutated, so every construction shares one pinned
        // per-tag singleton instead of allocating a fresh 8-byte node - a
        // large RAM win for recursive enums (a full binary tree is ~half
        // leaves). The runtime returns a borrow; the enclosing aggregate's
        // store retains it and teardown releases it (balanced).
        if n_args == 0 {
            // Tagged repr: a unit variant needs no object at all - the
            // value is a TAGGED NULL (`disc << 1`, base pointer zero),
            // the niche encoding Rust uses for `Option<Box<T>>`. No
            // allocation, no singleton cache, and every accounting
            // entry sees a null base and no-ops. Payload loads are
            // null-guarded in `gos_enum_load`, and unit variants have
            // no fields to load anyway.
            if self.enum_repr_tagged(enum_name) {
                let dest = self.fresh(ty);
                self.emit_assign(
                    Place::local(dest),
                    Rvalue::Use(Operand::Const(ConstValue::Int(
                        i128::from(variant_idx) << 1,
                    ))),
                    span,
                );
                return Some(dest);
            }
            let tag_local = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(tag_local),
                Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(variant_idx)))),
                span,
            );
            let dest = self.fresh(ty);
            let next = self.new_block(span);
            self.terminate(Terminator::Call {
                callee: Operand::Const(ConstValue::Str("gos_rt_enum_unit".to_string())),
                args: vec![Operand::Copy(Place::local(tag_local))],
                destination: Place::local(dest),
                target: Some(next),
            });
            self.set_current(next);
            return Some(dest);
        }

        // Lower the payload args first so we know each field's type, and
        // record which payload words hold RC-managed child pointers
        // (heap-allocated user enums). Word 0 is the discriminant; field
        // `i` lands at word `i + 1`. A field is an RC child when it is the
        // same enum (direct recursion) or an already-registered RC enum
        // (mutual recursion / forward reference).
        let mut payload_locals = Vec::with_capacity(n_args);
        let mut vec_payloads = vec![false; n_args];
        let mut child_offsets: Vec<i64> = Vec::new();
        for (i, value) in payload.iter().copied().enumerate() {
            let payload_local =
                self.coerce_enum_payload_array(value, field_tys.get(i).copied(), span);
            let payload_ty = self.locals[payload_local.0 as usize].ty;
            // A child entry names the field's WORD within the slot slab, so
            // it follows the field's placement rather than its position.
            let child_word = payload_offsets.get(i).copied().unwrap_or((i * 8) as i64) / 8;
            // A multi-slot aggregate payload (struct / tuple / array > 1 word)
            // does not fit the one-word payload slot. Heap-box it into an RC
            // cell and store the POINTER; the box owns its RC children (a
            // `String` / nested node field) and is reclaimed when the enum's
            // child release frees it. Sentinel ADTs (Option / Result) are
            // by-value 2-word values that occupy both of their words in
            // place, the way a struct field of the same type does.
            if self.is_boxable_aggregate_payload(payload_ty) {
                let boxed = self.box_aggregate_payload(payload_local, payload_ty, span);
                child_offsets.push(child_word);
                payload_locals.push(boxed);
            } else {
                if payload_ty == ty || self.tcx.is_rc_managed(payload_ty) {
                    child_offsets.push(child_word);
                } else if matches!(
                    self.tcx.kind_of(payload_ty),
                    gossamer_types::TyKind::Vec(_) | gossamer_types::TyKind::Slice(_)
                ) {
                    // A Vec/[T] payload is a refcounted container the node
                    // must own: the store below retains the node's share
                    // and the kind-tagged meta entry has release free it,
                    // so the vec survives the constructing frame however
                    // the node escapes (return, call argument, container).
                    use gossamer_abi::rc::{RC_CHILD_KIND_SHIFT, RC_CHILD_VEC};
                    child_offsets.push(child_word | (RC_CHILD_VEC << RC_CHILD_KIND_SHIFT));
                    vec_payloads[i] = true;
                }
                payload_locals.push(payload_local);
            }
        }

        // Register this variant's child-layout meta and obtain the
        // codegen symbol to reference. A variant with no RC-pointer
        // children needs no descriptor - the empty symbol lowers to a
        // null meta pointer, which the runtime treats as a leaf.
        let meta_symbol = if child_offsets.is_empty() {
            String::new()
        } else {
            self.register_rc_variant_meta(enum_name, variant_idx, &child_offsets)
        };

        let size_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(size_local),
            Rvalue::Use(Operand::Const(ConstValue::Int(bytes))),
            span,
        );
        let dest = self.fresh(ty);
        let tagged = self.enum_repr_tagged(enum_name);
        self.emit_assign(
            Place::local(dest),
            Rvalue::CallIntrinsic {
                name: if tagged {
                    "gos_rc_alloc_tagged"
                } else {
                    "gos_rc_alloc"
                },
                args: vec![
                    Operand::Copy(Place::local(size_local)),
                    Operand::Const(ConstValue::Str(meta_symbol)),
                ],
            },
            span,
        );
        let unit_ty = self.tcx.unit();
        let disc_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(disc_local),
            Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(variant_idx)))),
            span,
        );
        if !tagged {
            // Header representation: write the discriminant byte.
            let store_dest = self.fresh(unit_ty);
            self.emit_assign(
                Place::local(store_dest),
                Rvalue::CallIntrinsic {
                    name: "gos_enum_set_disc",
                    args: vec![
                        Operand::Copy(Place::local(dest)),
                        Operand::Copy(Place::local(disc_local)),
                    ],
                },
                span,
            );
        }
        // Write each payload at offset i*8 (already lowered above).
        for (i, payload_local) in payload_locals.into_iter().enumerate() {
            // The node's share of a Vec payload (see the kind-tagged meta
            // entry above); the constructing frame keeps its own release,
            // so the count stays balanced whether or not the node escapes.
            if vec_payloads[i] {
                let retain_dest = self.fresh(unit_ty);
                self.emit_assign(
                    Place::local(retain_dest),
                    Rvalue::CallIntrinsic {
                        name: "gos_rt_vec_retain",
                        args: vec![Operand::Copy(Place::local(payload_local))],
                    },
                    span,
                );
            }
            let off_local = self.fresh(i64_ty);
            self.emit_assign(
                Place::local(off_local),
                Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(
                    payload_offsets.get(i).copied().unwrap_or((i * 8) as i64),
                )))),
                span,
            );
            // A two-word carrier field owns both of its words in the node,
            // so its write takes the store sized for it.
            let wide = crate::lower::carrier_ref::is_two_word_carrier(
                self.tcx,
                self.locals[payload_local.0 as usize].ty,
            );
            let payload_dest = self.fresh(unit_ty);
            self.emit_assign(
                Place::local(payload_dest),
                Rvalue::CallIntrinsic {
                    name: if wide { "gos_store_i128" } else { "gos_store" },
                    args: vec![
                        Operand::Copy(Place::local(dest)),
                        Operand::Copy(Place::local(off_local)),
                        Operand::Copy(Place::local(payload_local)),
                    ],
                },
                span,
            );
        }
        if tagged {
            // Fold the discriminant into pointer bits 1-2; the tagged
            // pointer is the value every consumer sees.
            let tagged_dest = self.fresh(ty);
            self.emit_assign(
                Place::local(tagged_dest),
                Rvalue::CallIntrinsic {
                    name: "gos_enum_tag",
                    args: vec![
                        Operand::Copy(Place::local(dest)),
                        Operand::Copy(Place::local(disc_local)),
                    ],
                },
                span,
            );
            return Some(tagged_dest);
        }
        Some(dest)
    }

    /// Builds the single-record RC type-meta blob for one enum variant
    /// and registers it on the `TyCtxt` under a stable codegen symbol,
    /// returning that symbol. Because each allocation stores its own
    /// meta pointer, the descriptor only needs to describe *this*
    /// variant's children - so it uses the struct-kind shape (one
    /// record, discriminant ignored at release time). See the blob
    /// format in `gossamer-runtime` `c_abi::rc`.
    /// True when `enum_name`'s heap representation carries the
    /// discriminant in pointer bits 1-2 instead of a header byte:
    /// at most 4 variants. Bit 0 stays 0 - odd pointers are string
    /// bodies - and 8-byte alignment frees exactly bits 0-2. Larger
    /// enums keep the header-disc representation. Every construction
    /// and match site for a type must agree, so both consult this.
    pub(crate) fn enum_repr_tagged(&self, enum_name: &str) -> bool {
        self.enums
            .by_enum
            .get(enum_name)
            .is_some_and(|v| !v.is_empty() && v.len() <= 4)
    }

    fn register_rc_variant_meta(
        &mut self,
        enum_name: &str,
        variant_idx: u32,
        child_offsets: &[i64],
    ) -> String {
        use gossamer_abi::rc::RC_KIND_STRUCT;
        let sanitised: String = enum_name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let symbol = format!("gos_rc_meta_{sanitised}_v{variant_idx}");
        let mut blob = vec![
            RC_KIND_STRUCT,
            1,
            i64::from(variant_idx),
            child_offsets.len() as i64,
        ];
        blob.extend_from_slice(child_offsets);
        self.tcx.register_rc_meta(symbol.clone(), blob);
        symbol
    }

    pub(crate) fn lower_result_ctor(
        &mut self,
        disc: i64,
        payload_expr: &HirExpr,
        ty: Ty,
        span: Span,
    ) -> Option<Local> {
        if std::env::var("GOS_MIR_TRACE").is_ok() {
            eprintln!("[lower_result_ctor] disc={disc}");
        }
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let disc_local = self.fresh(i64_ty);
        self.emit_assign(
            Place::local(disc_local),
            Rvalue::Use(Operand::Const(ConstValue::Int(i128::from(disc)))),
            span,
        );
        let payload_local = self.lower_expr(payload_expr)?;
        // Route f64 payloads through `gos_rt_result_new_f64` so the
        // bit pattern is preserved via `to_bits` (matching the
        // symmetric `gos_rt_result_payload_f64` extractor). Without
        // this, the LLVM tier coerces f64 → i64 via `fptosi`,
        // truncating values like `3.5` to `3`.
        let payload_ty = self.locals[payload_local.0 as usize].ty;
        // A multi-slot aggregate payload is heap-copied by the backend;
        // registering its guarded meta here turns that copy into a
        // reference-counted blob the drop pass can reclaim.
        if self.type_slot_bytes(payload_ty) > 8
            && matches!(
                self.tcx.kind_of(payload_ty),
                gossamer_types::TyKind::Adt { .. } | gossamer_types::TyKind::Tuple(_)
            )
        {
            let _ = self.ensure_aggr_copy_meta(payload_ty);
        }
        let payload_is_f64 = matches!(
            self.tcx.kind_of(payload_ty),
            gossamer_types::TyKind::Float(_)
        );
        // Pin the dest to the i128 Result/Option representation even when
        // inference left `ty` an unresolved `Var` (else the i128 truncates
        // through a `ptr` slot).
        let rty = self.result_repr_ty(ty);
        let dest = self.fresh(rty);
        let intrinsic_name = if payload_is_f64 {
            "gos_rt_result_new_f64"
        } else {
            "gos_rt_result_new"
        };
        self.emit_assign(
            Place::local(dest),
            Rvalue::CallIntrinsic {
                name: intrinsic_name,
                args: vec![
                    Operand::Copy(Place::local(disc_local)),
                    Operand::Copy(Place::local(payload_local)),
                ],
            },
            span,
        );
        Some(dest)
    }
}
