//! Calls to functions declared in an `unsafe extern "C"` block.
//!
//! The call is bracketed by `gos_rt_ffi_enter` / `gos_rt_ffi_leave`, which
//! mark the worker as possibly blocked and capture `errno` before anything
//! else runs. A slice argument crosses as the pointer `gos_rt_ffi_buf_begin`
//! answers for its vector and is retired by `gos_rt_ffi_buf_end` after the
//! call; a struct argument arrives from MIR already packed, as a pointer.

use std::fmt::Write as _;

use gossamer_abi::c_aggregate::{ArgPlan, Piece, PieceKind, RetPlan};
use gossamer_mir::{ForeignCall, ForeignCallback, ForeignParam, ForeignStatic, Local, Operand};

use super::{Lowerer, declare_rt, render_ty};
use crate::BuildError;

/// The LLVM type of one C class character, with the parameter attribute C
/// promotion needs for a narrow integer.
fn c_type(class: char) -> Option<(&'static str, &'static str)> {
    Some(match class {
        'c' => ("i8", "signext "),
        'C' | 'B' => ("i8", "zeroext "),
        'h' => ("i16", "signext "),
        'H' => ("i16", "zeroext "),
        'i' => ("i32", "signext "),
        'I' => ("i32", "zeroext "),
        'l' | 'L' => ("i64", ""),
        'f' => ("float", ""),
        'd' => ("double", ""),
        'v' => ("void", ""),
        _ => return None,
    })
}

fn is_signed(class: char) -> bool {
    gossamer_types::c_class_signed(class)
}

/// The LLVM type a struct piece travels as, with the extension attribute a
/// narrow integer needs.
fn piece_type(kind: PieceKind) -> (&'static str, &'static str) {
    match kind {
        PieceKind::Int { bytes: 1, signed } => ("i8", if signed { "signext " } else { "zeroext " }),
        PieceKind::Int { bytes: 2, signed } => {
            ("i16", if signed { "signext " } else { "zeroext " })
        }
        PieceKind::Int { bytes: 4, signed } => {
            ("i32", if signed { "signext " } else { "zeroext " })
        }
        PieceKind::Int { .. } => ("i64", ""),
        PieceKind::F32 => ("float", ""),
        PieceKind::F64 => ("double", ""),
    }
}

/// The LLVM result type of a struct returned as `pieces`.
fn piece_ret_type(pieces: &[Piece]) -> String {
    if let [piece] = pieces {
        return piece_type(piece.kind).0.to_string();
    }
    let parts: Vec<&str> = pieces
        .iter()
        .map(|piece| piece_type(piece.kind).0)
        .collect();
    format!("{{ {} }}", parts.join(", "))
}

/// A symbol-safe spelling of a callback's C signature, so two signatures for
/// one adapter name two entries.
fn shim_suffix(callback: ForeignCallback<'_>) -> String {
    let safe = |text: &str| -> String {
        text.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect()
    };
    format!(
        "{}_{}{}",
        safe(callback.params),
        callback.ret,
        safe(callback.ret_layout.unwrap_or_default())
    )
}

/// Stores `word` into slot `index` of the shim's `%words` array.
fn store_word(body: &mut String, count: usize, index: usize, word: &str) {
    writeln!(
        body,
        "  %slot{index} = getelementptr [{count} x i64], ptr %words, i64 0, i64 {index}"
    )
    .unwrap();
    writeln!(body, "  store i64 {word}, ptr %slot{index}, align 8").unwrap();
}

/// Declares the parameters a struct argument `index` arrives as under
/// `plan`, rebuilds it in memory where it arrived in registers, and answers
/// the word holding its address.
fn receive_struct(
    index: usize,
    layout: &gossamer_abi::c_aggregate::CLayout,
    plan: &ArgPlan,
    params: &mut Vec<String>,
    body: &mut String,
) -> String {
    match plan {
        ArgPlan::Pieces {
            pad_int,
            pad_float,
            pieces,
        } => {
            for pad in 0..*pad_int {
                params.push(format!("i64 %pad{index}.{pad}"));
            }
            for pad in 0..*pad_float {
                params.push(format!("double %padf{index}.{pad}"));
            }
            writeln!(
                body,
                "  %s{index} = alloca [{} x i8], align {}",
                layout.size.max(8).next_multiple_of(8),
                layout.align.max(8)
            )
            .unwrap();
            for (k, piece) in pieces.iter().enumerate() {
                let (ty, attr) = piece_type(piece.kind);
                params.push(format!("{ty} {attr}%p{index}.{k}"));
                writeln!(
                    body,
                    "  %p{index}.{k}.a = getelementptr i8, ptr %s{index}, i64 {}",
                    piece.offset
                )
                .unwrap();
                writeln!(
                    body,
                    "  store {ty} %p{index}.{k}, ptr %p{index}.{k}.a, align 1"
                )
                .unwrap();
            }
            writeln!(body, "  %w{index} = ptrtoint ptr %s{index} to i64").unwrap();
        }
        ArgPlan::Memory { size, align } => {
            params.push(format!(
                "ptr byval([{} x i8]) align {} %a{index}",
                size.max(&1),
                align.max(&1)
            ));
            writeln!(body, "  %w{index} = ptrtoint ptr %a{index} to i64").unwrap();
        }
        ArgPlan::Indirect | ArgPlan::Scalar => {
            params.push(format!("ptr %a{index}"));
            writeln!(body, "  %w{index} = ptrtoint ptr %a{index} to i64").unwrap();
        }
    }
    format!("%w{index}")
}

impl Lowerer<'_> {
    /// Emits the foreign call `name` names and answers its result in the
    /// destination's LLVM type. A struct by value or a struct result moves
    /// per the target's calling convention (`gossamer_abi::c_aggregate`).
    pub(crate) fn lower_foreign_call(
        &mut self,
        name: &str,
        args: &[Operand],
        dest_local: Local,
    ) -> Result<String, BuildError> {
        let call = ForeignCall::parse(name)
            .ok_or(BuildError::InternalLoweringBug("malformed foreign call"))?;
        let params = call.param_list();
        let needs_plan = call.ret_layout.is_some()
            || params
                .iter()
                .any(|param| matches!(param, ForeignParam::ByValue(_)));
        let plan = if needs_plan {
            let abi = crate::emit::target_c_abi().ok_or(BuildError::InternalLoweringBug(
                "a struct passed by value has no lowering on this target",
            ))?;
            Some(call.plan(abi))
        } else {
            None
        };
        let mut decl_params = Vec::with_capacity(args.len());
        let mut call_args = Vec::with_capacity(args.len());
        let mut buffers = Vec::new();
        declare_rt(&mut self.runtime_refs, "gos_rt_ffi_enter");
        declare_rt(&mut self.runtime_refs, "gos_rt_ffi_leave");
        // A call through an address takes its target from the first operand.
        let (target, args) = if call.is_indirect() {
            let (address, rest) = args.split_first().ok_or(BuildError::InternalLoweringBug(
                "indirect foreign call without a target",
            ))?;
            let value = self.lower_operand(address)?;
            let from = self.operand_llvm_ty(address);
            (Some(self.coerce_llvm_value(&value, &from, "ptr")), rest)
        } else {
            (None, args)
        };
        // A struct result comes back through the final operand: the buffer
        // the call's result is written to.
        let (args, holder) = if call.ret_layout.is_some() {
            let (holder, rest) = args.split_last().ok_or(BuildError::InternalLoweringBug(
                "struct result without its buffer",
            ))?;
            let value = self.lower_operand(holder)?;
            let from = self.operand_llvm_ty(holder);
            (rest, Some(self.coerce_llvm_value(&value, &from, "ptr")))
        } else {
            (args, None)
        };
        let ret_struct = call.ret_struct();
        if let (Some(plan), Some(holder), Some(layout)) = (&plan, &holder, &ret_struct)
            && plan.ret == RetPlan::Sret
        {
            let sret = format!(
                "ptr sret([{} x i8]) align {}",
                layout.size.max(1),
                layout.align.max(1)
            );
            decl_params.push(sret.clone());
            call_args.push(format!("{sret} {holder}"));
        }
        for (index, (param, arg)) in params.into_iter().zip(args).enumerate() {
            let value = self.lower_operand(arg)?;
            let from = self.operand_llvm_ty(arg);
            match param {
                ForeignParam::Scalar(class) => {
                    let (c_ty, attr) = c_type(class)
                        .ok_or(BuildError::InternalLoweringBug("foreign parameter class"))?;
                    decl_params.push(format!("{c_ty} {attr}").trim_end().to_string());
                    let value = self.widen_to_c(&value, &from, c_ty, is_signed(class));
                    call_args.push(format!("{c_ty} {attr}{value}"));
                }
                ForeignParam::Slice { elem, writable } => {
                    declare_rt(&mut self.runtime_refs, "gos_rt_ffi_buf_begin");
                    declare_rt(&mut self.runtime_refs, "gos_rt_ffi_buf_end");
                    let vec = self.coerce_llvm_value(&value, &from, "ptr");
                    let elements = self.fresh();
                    let class = u32::from(elem);
                    writeln!(
                        self.out,
                        "  {elements} = call ptr @gos_rt_ffi_buf_begin(ptr {vec}, i64 {class})"
                    )
                    .unwrap();
                    buffers.push((vec, elements.clone(), i64::from(writable), class));
                    decl_params.push("ptr".to_string());
                    call_args.push(format!("ptr {elements}"));
                }
                ForeignParam::Struct { .. } => {
                    let pointer = self.coerce_llvm_value(&value, &from, "ptr");
                    decl_params.push("ptr".to_string());
                    call_args.push(format!("ptr {pointer}"));
                }
                ForeignParam::ByValue(_) => {
                    let buffer = self.coerce_llvm_value(&value, &from, "ptr");
                    let arg_plan = plan
                        .as_ref()
                        .and_then(|plan| plan.params.get(index))
                        .cloned()
                        .ok_or(BuildError::InternalLoweringBug(
                            "struct argument without a plan",
                        ))?;
                    self.pass_struct(&arg_plan, &buffer, &mut decl_params, &mut call_args);
                }
            }
        }
        // A result in pieces comes back as an LLVM struct of them, or the
        // one piece alone.
        let piece_ret = match plan.as_ref().map(|plan| &plan.ret) {
            Some(RetPlan::Pieces(pieces)) => Some(pieces.clone()),
            _ => None,
        };
        let (ret_ty, ret_attr) = match (&piece_ret, call.ret) {
            (Some(pieces), _) => (piece_ret_type(pieces), ""),
            (None, 's') => ("void".to_string(), ""),
            (None, class) => {
                let (ty, attr) =
                    c_type(class).ok_or(BuildError::InternalLoweringBug("foreign result class"))?;
                (ty.to_string(), attr)
            }
        };
        let symbol = call.symbol;
        let callee = match &target {
            Some(pointer) => pointer.clone(),
            None => {
                let needle = format!("@{symbol}(");
                if !self.runtime_refs.iter().any(|d| d.contains(&needle)) {
                    self.runtime_refs.insert(format!(
                        "declare {ret_attr}{ret_ty} @{symbol}({})",
                        decl_params.join(", ")
                    ));
                }
                format!("@{symbol}")
            }
        };
        writeln!(self.out, "  call void @gos_rt_ffi_enter()").unwrap();
        let args_text = call_args.join(", ");
        // `nobuiltin`: the call reaches the native function, never LLVM's
        // model of a C library function of the same name.
        let result = if ret_ty == "void" {
            writeln!(self.out, "  call void {callee}({args_text}) nobuiltin").unwrap();
            None
        } else {
            let tmp = self.fresh();
            writeln!(
                self.out,
                "  {tmp} = call {ret_attr}{ret_ty} {callee}({args_text}) nobuiltin"
            )
            .unwrap();
            Some(tmp)
        };
        writeln!(self.out, "  call void @gos_rt_ffi_leave()").unwrap();
        if let (Some(pieces), Some(result), Some(holder)) = (&piece_ret, &result, &holder) {
            self.store_ret_pieces(pieces, result, &ret_ty, holder);
        }
        for (vec, elements, writable, class) in buffers {
            writeln!(
                self.out,
                "  call void @gos_rt_ffi_buf_end(ptr {vec}, ptr {elements}, i64 {writable}, \
                 i64 {class})"
            )
            .unwrap();
        }
        let dest_ty = render_ty(self.tcx, self.body.local_ty(dest_local));
        let Some(result) = result.filter(|_| piece_ret.is_none()) else {
            return Ok(match dest_ty.as_str() {
                "ptr" => "null",
                "double" | "float" => "0.0",
                _ => "0",
            }
            .to_string());
        };
        if call.ret == 'B' && dest_ty == "i1" {
            let flag = self.fresh();
            writeln!(self.out, "  {flag} = icmp ne i8 {result}, 0").unwrap();
            return Ok(flag);
        }
        Ok(self.widen_to_c(&result, &ret_ty, &dest_ty, is_signed(call.ret)))
    }

    /// Appends the arguments a struct at `buffer` travels as under `plan`.
    fn pass_struct(
        &mut self,
        plan: &ArgPlan,
        buffer: &str,
        decl_params: &mut Vec<String>,
        call_args: &mut Vec<String>,
    ) {
        match plan {
            ArgPlan::Scalar => {}
            ArgPlan::Pieces {
                pad_int,
                pad_float,
                pieces,
            } => {
                // Dummy arguments use up the registers the convention retires
                // for a struct that does not fit in what is left.
                for _ in 0..*pad_int {
                    decl_params.push("i64".to_string());
                    call_args.push("i64 0".to_string());
                }
                for _ in 0..*pad_float {
                    decl_params.push("double".to_string());
                    call_args.push("double 0.0".to_string());
                }
                for piece in pieces {
                    let (ty, attr) = piece_type(piece.kind);
                    let address = self.fresh();
                    writeln!(
                        self.out,
                        "  {address} = getelementptr i8, ptr {buffer}, i64 {}",
                        piece.offset
                    )
                    .unwrap();
                    let value = self.fresh();
                    writeln!(self.out, "  {value} = load {ty}, ptr {address}, align 1").unwrap();
                    decl_params.push(format!("{ty} {attr}").trim_end().to_string());
                    call_args.push(format!("{ty} {attr}{value}"));
                }
            }
            ArgPlan::Memory { size, align } => {
                let byval = format!("ptr byval([{} x i8]) align {}", size.max(&1), align.max(&1));
                decl_params.push(byval.clone());
                call_args.push(format!("{byval} {buffer}"));
            }
            ArgPlan::Indirect => {
                decl_params.push("ptr".to_string());
                call_args.push(format!("ptr {buffer}"));
            }
        }
    }

    /// Stores a result returned as `pieces` (in `result`, of LLVM type
    /// `ret_ty`) into the struct buffer `holder`.
    fn store_ret_pieces(&mut self, pieces: &[Piece], result: &str, ret_ty: &str, holder: &str) {
        for (index, piece) in pieces.iter().enumerate() {
            let (ty, _) = piece_type(piece.kind);
            let value = if pieces.len() == 1 {
                result.to_string()
            } else {
                let part = self.fresh();
                writeln!(
                    self.out,
                    "  {part} = extractvalue {ret_ty} {result}, {index}"
                )
                .unwrap();
                part
            };
            let address = self.fresh();
            writeln!(
                self.out,
                "  {address} = getelementptr i8, ptr {holder}, i64 {}",
                piece.offset
            )
            .unwrap();
            writeln!(self.out, "  store {ty} {value}, ptr {address}, align 1").unwrap();
        }
    }

    /// The address of the C-ABI entry for the callback intrinsic `name`: an
    /// internal function with the callback's C signature that stores its
    /// arguments as words and runs the adapter through
    /// `gos_rt_ffi_callback_run`. A struct argument arrives per the target's
    /// calling convention and its word is the address of its bytes; a struct
    /// result is written by the adapter to a buffer whose address is the
    /// word after the arguments'.
    pub(crate) fn lower_foreign_callback(
        &mut self,
        name: &str,
        dest_local: Local,
    ) -> Result<String, BuildError> {
        let callback = ForeignCallback::parse(name).ok_or(BuildError::InternalLoweringBug(
            "malformed callback intrinsic",
        ))?;
        let adapter = super::mangle_fn_name(callback.adapter).into_owned();
        let (shim, label) = if callback.exported {
            (
                callback.name.to_string(),
                format!("__gos_ffi_exname_{adapter}"),
            )
        } else {
            (
                format!("__gos_ffi_shim_{adapter}_{}", shim_suffix(callback)),
                format!("__gos_ffi_cbname_{adapter}"),
            )
        };
        declare_rt(&mut self.runtime_refs, "gos_rt_ffi_callback_run");
        let mut label_bytes = String::new();
        for byte in callback.name.bytes().chain(std::iter::once(0)) {
            if byte.is_ascii_alphanumeric() || byte == b'_' {
                label_bytes.push(char::from(byte));
            } else {
                write!(label_bytes, "\\{byte:02X}").unwrap();
            }
        }
        self.runtime_refs.insert(format!(
            "@\"{label}\" = private unnamed_addr constant [{} x i8] c\"{label_bytes}\"",
            callback.name.len() + 1
        ));
        let plan = if callback.has_struct() {
            let abi = crate::emit::target_c_abi().ok_or(BuildError::InternalLoweringBug(
                "a struct passed by value has no lowering on this target",
            ))?;
            Some(callback.plan(abi))
        } else {
            None
        };
        let param_list = callback.param_list();
        let ret_struct = callback
            .ret_layout
            .and_then(gossamer_abi::c_aggregate::CLayout::parse);
        let count = (param_list.len() + usize::from(ret_struct.is_some())).max(1);
        let mut params = Vec::new();
        let mut body = String::new();
        writeln!(body, "  %words = alloca [{count} x i64], align 8").unwrap();
        if let (Some(plan), Some(layout)) = (&plan, &ret_struct)
            && plan.ret == RetPlan::Sret
        {
            params.push(format!(
                "ptr sret([{} x i8]) align {} %sret",
                layout.size.max(1),
                layout.align.max(1)
            ));
        }
        for (index, param) in param_list.iter().enumerate() {
            let word = match param {
                ForeignParam::ByValue(layout) => {
                    let arg_plan = plan
                        .as_ref()
                        .and_then(|plan| plan.params.get(index))
                        .ok_or(BuildError::InternalLoweringBug(
                            "struct callback argument without a plan",
                        ))?;
                    receive_struct(index, layout, arg_plan, &mut params, &mut body)
                }
                ForeignParam::Scalar(class) => {
                    let class = *class;
                    let (c_ty, attr) = c_type(class)
                        .ok_or(BuildError::InternalLoweringBug("callback parameter class"))?;
                    params.push(format!("{c_ty} {attr}%a{index}"));
                    match class {
                        'f' => {
                            writeln!(body, "  %w{index}.d = fpext float %a{index} to double")
                                .unwrap();
                            writeln!(body, "  %w{index} = bitcast double %w{index}.d to i64")
                                .unwrap();
                            format!("%w{index}")
                        }
                        'd' => {
                            writeln!(body, "  %w{index} = bitcast double %a{index} to i64")
                                .unwrap();
                            format!("%w{index}")
                        }
                        'l' | 'L' => format!("%a{index}"),
                        class => {
                            let extend = if is_signed(class) { "sext" } else { "zext" };
                            writeln!(body, "  %w{index} = {extend} {c_ty} %a{index} to i64")
                                .unwrap();
                            format!("%w{index}")
                        }
                    }
                }
                ForeignParam::Slice { .. } | ForeignParam::Struct { .. } => {
                    return Err(BuildError::InternalLoweringBug(
                        "a callback takes scalars and structs",
                    ));
                }
            };
            store_word(&mut body, count, index, &word);
        }
        // A struct result is written to a buffer: the caller's for `sret`,
        // otherwise one here whose pieces are then returned.
        if let (Some(plan), Some(layout)) = (&plan, &ret_struct) {
            let buffer = if plan.ret == RetPlan::Sret {
                "%sret".to_string()
            } else {
                writeln!(
                    body,
                    "  %rbuf = alloca [{} x i8], align {}",
                    layout.size.max(8).next_multiple_of(8),
                    layout.align.max(8)
                )
                .unwrap();
                "%rbuf".to_string()
            };
            writeln!(body, "  %rbuf.w = ptrtoint ptr {buffer} to i64").unwrap();
            store_word(&mut body, count, param_list.len(), "%rbuf.w");
        }
        if callback.exported {
            // A host thread may call the entry before anything else in the
            // library has run, so the entry starts the runtime first.
            declare_rt(&mut self.runtime_refs, "gos_rt_ffi_export_run");
            let init = gossamer_mir::STATIC_INIT_FN;
            let init = if self.param_tys_by_name.contains_key(init) {
                format!("ptr @\"{}\"", super::mangle_fn_name(init))
            } else {
                "ptr null".to_string()
            };
            writeln!(
                body,
                "  %r = call i64 @gos_rt_ffi_export_run(ptr @\"{adapter}\", ptr %words, ptr @\"{label}\", {init})"
            )
            .unwrap();
        } else {
            writeln!(
                body,
                "  %r = call i64 @gos_rt_ffi_callback_run(ptr @\"{adapter}\", ptr %words, ptr @\"{label}\")"
            )
            .unwrap();
        }
        let ret_ty = match plan.as_ref().map(|plan| &plan.ret) {
            Some(RetPlan::Sret) => {
                writeln!(body, "  ret void").unwrap();
                "void".to_string()
            }
            Some(RetPlan::Pieces(pieces)) => {
                let ret_ty = piece_ret_type(pieces);
                let mut aggregate = "undef".to_string();
                for (index, piece) in pieces.iter().enumerate() {
                    let (ty, _) = piece_type(piece.kind);
                    writeln!(
                        body,
                        "  %rp{index}.a = getelementptr i8, ptr %rbuf, i64 {}",
                        piece.offset
                    )
                    .unwrap();
                    writeln!(body, "  %rp{index} = load {ty}, ptr %rp{index}.a, align 1").unwrap();
                    if pieces.len() > 1 {
                        writeln!(
                            body,
                            "  %ra{index} = insertvalue {ret_ty} {aggregate}, {ty} %rp{index}, {index}"
                        )
                        .unwrap();
                        aggregate = format!("%ra{index}");
                    } else {
                        aggregate = format!("%rp{index}");
                    }
                }
                writeln!(body, "  ret {ret_ty} {aggregate}").unwrap();
                ret_ty
            }
            _ => {
                let (ret_ty, _) = c_type(callback.ret)
                    .ok_or(BuildError::InternalLoweringBug("callback result class"))?;
                match callback.ret {
                    'v' => writeln!(body, "  ret void").unwrap(),
                    'l' | 'L' => writeln!(body, "  ret i64 %r").unwrap(),
                    'd' => {
                        writeln!(body, "  %rd = bitcast i64 %r to double").unwrap();
                        writeln!(body, "  ret double %rd").unwrap();
                    }
                    'f' => {
                        writeln!(body, "  %rd = bitcast i64 %r to double").unwrap();
                        writeln!(body, "  %rf = fptrunc double %rd to float").unwrap();
                        writeln!(body, "  ret float %rf").unwrap();
                    }
                    _ => {
                        writeln!(body, "  %rn = trunc i64 %r to {ret_ty}").unwrap();
                        writeln!(body, "  ret {ret_ty} %rn").unwrap();
                    }
                }
                ret_ty.to_string()
            }
        };
        let ret_attr = if plan.is_none() {
            c_type(callback.ret).map_or("", |(_, attr)| attr)
        } else {
            ""
        };
        // An export is the library's interface: external, and named in a
        // Windows DLL's export table.
        let linkage = if !callback.exported {
            "internal "
        } else if crate::emit::target_is_windows() {
            "dllexport "
        } else {
            "dso_local "
        };
        self.runtime_refs.insert(format!(
            "define {linkage}{ret_attr}{ret_ty} @\"{shim}\"({}) {{\n{body}}}",
            params.join(", ")
        ));
        let dest_ty = render_ty(self.tcx, self.body.local_ty(dest_local));
        let address = self.fresh();
        writeln!(self.out, "  {address} = ptrtoint ptr @\"{shim}\" to i64").unwrap();
        Ok(self.coerce_llvm_value(&address, "i64", &dest_ty))
    }

    /// The address of the C global the foreign-static intrinsic `name`
    /// names: an external global the linker resolves.
    pub(crate) fn lower_foreign_static(
        &mut self,
        name: &str,
        dest_local: Local,
    ) -> Result<String, BuildError> {
        let global = ForeignStatic::parse(name).ok_or(BuildError::InternalLoweringBug(
            "malformed foreign static intrinsic",
        ))?;
        let symbol = global.symbol;
        let needle = format!("@\"{symbol}\" =");
        if !self.runtime_refs.iter().any(|d| d.starts_with(&needle)) {
            // A global exported by a Windows DLL is reached through its
            // import table entry.
            let import = if crate::emit::target_is_windows() && !global.library.is_empty() {
                "dllimport "
            } else {
                ""
            };
            self.runtime_refs
                .insert(format!("@\"{symbol}\" = external {import}global i8"));
        }
        let dest_ty = render_ty(self.tcx, self.body.local_ty(dest_local));
        let address = self.fresh();
        writeln!(self.out, "  {address} = ptrtoint ptr @\"{symbol}\" to i64").unwrap();
        Ok(self.coerce_llvm_value(&address, "i64", &dest_ty))
    }

    /// `value` of LLVM type `from` as type `to`, extending an integer by the
    /// signedness its C class declares.
    fn widen_to_c(&mut self, value: &str, from: &str, to: &str, signed: bool) -> String {
        if from == "double" && to == "float" {
            let narrow = self.fresh();
            writeln!(self.out, "  {narrow} = fptrunc double {value} to float").unwrap();
            return narrow;
        }
        if from == "float" && to == "double" {
            let wide = self.fresh();
            writeln!(self.out, "  {wide} = fpext float {value} to double").unwrap();
            return wide;
        }
        let width = |ty: &str| ty.strip_prefix('i').and_then(|w| w.parse::<u32>().ok());
        match (width(from), width(to)) {
            (Some(f), Some(t)) if t > f && signed && f > 1 => {
                let tmp = self.fresh();
                writeln!(self.out, "  {tmp} = sext {from} {value} to {to}").unwrap();
                tmp
            }
            _ => self.coerce_llvm_value(value, from, to),
        }
    }
}
