//! Calls to functions declared in an `unsafe extern "C"` block.
//!
//! The call is bracketed by `gos_rt_ffi_enter` / `gos_rt_ffi_leave`, which
//! mark the worker as possibly blocked and capture `errno` before anything
//! else runs. A slice argument crosses as the pointer `gos_rt_ffi_buf_begin`
//! answers for its vector and is retired by `gos_rt_ffi_buf_end` after the
//! call; a struct argument arrives from MIR already packed, as a pointer.

use std::fmt::Write as _;

use gossamer_mir::{ForeignCall, ForeignCallback, ForeignParam, Local, Operand};

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

/// A symbol-safe spelling of a callback's C signature, so two signatures for
/// one adapter name two entries.
fn shim_suffix(callback: ForeignCallback<'_>) -> String {
    format!("{}_{}", callback.params, callback.ret)
}

impl Lowerer<'_> {
    /// Emits the foreign call `name` names and answers its result in the
    /// destination's LLVM type.
    pub(crate) fn lower_foreign_call(
        &mut self,
        name: &str,
        args: &[Operand],
        dest_local: Local,
    ) -> Result<String, BuildError> {
        let call = ForeignCall::parse(name)
            .ok_or(BuildError::InternalLoweringBug("malformed foreign call"))?;
        let (ret_ty, ret_attr) =
            c_type(call.ret).ok_or(BuildError::InternalLoweringBug("foreign result class"))?;
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
        for (param, arg) in call.param_list().into_iter().zip(args) {
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
            }
        }
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
        for (vec, elements, writable, class) in buffers {
            writeln!(
                self.out,
                "  call void @gos_rt_ffi_buf_end(ptr {vec}, ptr {elements}, i64 {writable}, \
                 i64 {class})"
            )
            .unwrap();
        }
        let dest_ty = render_ty(self.tcx, self.body.local_ty(dest_local));
        let Some(result) = result else {
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
        Ok(self.widen_to_c(&result, ret_ty, &dest_ty, is_signed(call.ret)))
    }

    /// The address of the C-ABI entry for the callback intrinsic `name`: an
    /// internal function with the callback's C signature that stores its
    /// arguments as words and runs the adapter through
    /// `gos_rt_ffi_callback_run`.
    pub(crate) fn lower_foreign_callback(
        &mut self,
        name: &str,
        dest_local: Local,
    ) -> Result<String, BuildError> {
        let callback = ForeignCallback::parse(name).ok_or(BuildError::InternalLoweringBug(
            "malformed callback intrinsic",
        ))?;
        let adapter = super::mangle_fn_name(callback.adapter).into_owned();
        let shim = format!("__gos_ffi_shim_{adapter}_{}", shim_suffix(callback));
        let label = format!("__gos_ffi_cbname_{adapter}");
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
        let (ret_ty, ret_attr) =
            c_type(callback.ret).ok_or(BuildError::InternalLoweringBug("callback result class"))?;
        let count = callback.params.chars().count().max(1);
        let mut params = Vec::new();
        let mut body = String::new();
        writeln!(body, "  %words = alloca [{count} x i64], align 8").unwrap();
        for (index, class) in callback.params.chars().enumerate() {
            let (c_ty, attr) =
                c_type(class).ok_or(BuildError::InternalLoweringBug("callback parameter class"))?;
            params.push(format!("{c_ty} {attr}%a{index}"));
            let word = match class {
                'f' => {
                    writeln!(body, "  %w{index}.d = fpext float %a{index} to double").unwrap();
                    writeln!(body, "  %w{index} = bitcast double %w{index}.d to i64").unwrap();
                    format!("%w{index}")
                }
                'd' => {
                    writeln!(body, "  %w{index} = bitcast double %a{index} to i64").unwrap();
                    format!("%w{index}")
                }
                'l' | 'L' => format!("%a{index}"),
                class => {
                    let extend = if is_signed(class) { "sext" } else { "zext" };
                    writeln!(body, "  %w{index} = {extend} {c_ty} %a{index} to i64").unwrap();
                    format!("%w{index}")
                }
            };
            writeln!(
                body,
                "  %slot{index} = getelementptr [{count} x i64], ptr %words, i64 0, i64 {index}"
            )
            .unwrap();
            writeln!(body, "  store i64 {word}, ptr %slot{index}, align 8").unwrap();
        }
        writeln!(
            body,
            "  %r = call i64 @gos_rt_ffi_callback_run(ptr @\"{adapter}\", ptr %words, ptr @\"{label}\")"
        )
        .unwrap();
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
        self.runtime_refs.insert(format!(
            "define internal {ret_attr}{ret_ty} @\"{shim}\"({}) {{\n{body}}}",
            params.join(", ")
        ));
        let dest_ty = render_ty(self.tcx, self.body.local_ty(dest_local));
        let address = self.fresh();
        writeln!(self.out, "  {address} = ptrtoint ptr @\"{shim}\" to i64").unwrap();
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
