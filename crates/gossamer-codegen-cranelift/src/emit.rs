//! MIR → CLIF-style textual IR emitter.
//! The production backend will feed a full `cranelift_codegen::Function`
//! through the Cranelift crate, but that crate pulls in `unsafe` code
//! which the Gossamer workspace forbids. therefore emits a
//! textual Cranelift-style IR so downstream passes (and humans) can
//! still read the emitted code, and so the structural lowering from
//! MIR to instruction-granularity IR is exercised end-to-end. A later
//! phase will replace this text backend with the real Cranelift call
//! graph behind an explicit RFC.

#![forbid(unsafe_code)]

use std::fmt::Write;

use gossamer_mir::{
    BinOp, Body, ConstValue, Operand, Place, Rvalue, StatementKind, Terminator, UnOp,
};

/// Output of the text backend: one CLIF-like module per input MIR.
#[derive(Debug, Clone, Default)]
pub struct Module {
    /// Source-order functions.
    pub functions: Vec<FunctionText>,
}

/// Textual Cranelift function.
#[derive(Debug, Clone)]
pub struct FunctionText {
    /// Function name.
    pub name: String,
    /// Fully rendered CLIF-style text.
    pub text: String,
    /// Number of parameters (excluding the return slot).
    pub arity: u32,
    /// Number of basic blocks in the rendered function.
    pub block_count: u32,
}

/// Emits a single MIR [`Body`] as CLIF-style text.
#[must_use]
pub fn emit_function(body: &Body) -> FunctionText {
    let mut out = String::new();
    write!(&mut out, "function %{}(", body.name).unwrap();
    for i in 0..body.arity {
        if i > 0 {
            out.push_str(", ");
        }
        let _ = write!(out, "v{}: i64", i + 1);
    }
    out.push(')');
    let has_ret = matches!(
        body.blocks
            .iter()
            .find_map(|b| if let Terminator::Return = b.terminator {
                Some(())
            } else {
                None
            }),
        Some(())
    );
    if has_ret {
        out.push_str(" -> i64");
    }
    out.push_str(" {\n");
    for block in &body.blocks {
        let _ = writeln!(out, "block{}:", block.id.as_u32());
        for stmt in &block.stmts {
            emit_statement(&mut out, stmt);
        }
        emit_terminator(&mut out, &block.terminator);
    }
    out.push_str("}\n");
    FunctionText {
        name: body.name.clone(),
        text: out,
        arity: body.arity,
        block_count: u32::try_from(body.blocks.len()).unwrap_or(u32::MAX),
    }
}

/// Emits every function in `bodies` into a single [`Module`].
#[must_use]
pub fn emit_module(bodies: &[Body]) -> Module {
    Module {
        functions: bodies.iter().map(emit_function).collect(),
    }
}

fn emit_statement(out: &mut String, stmt: &gossamer_mir::Statement) {
    match &stmt.kind {
        StatementKind::Assign { place, rvalue } => {
            let _ = write!(out, "    ");
            emit_place(out, place);
            out.push_str(" = ");
            emit_rvalue(out, rvalue);
            out.push('\n');
        }
        StatementKind::StorageLive(local) => {
            let _ = writeln!(out, "    storage_live v{}", local.as_u32());
        }
        StatementKind::StorageDead(local) => {
            let _ = writeln!(out, "    storage_dead v{}", local.as_u32());
        }
        StatementKind::SetDiscriminant { place, variant } => {
            let _ = write!(out, "    set_discriminant ");
            emit_place(out, place);
            let _ = writeln!(out, ", {variant}");
        }
        StatementKind::StaticStore { target, value } => {
            let _ = write!(out, "    static_store @{} = ", target.symbol);
            emit_operand(out, value);
            out.push('\n');
        }
        StatementKind::IterSource { .. } => out.push_str("    iter_source\n"),
        StatementKind::IterAdapter { .. } => out.push_str("    iter_adapter\n"),
        StatementKind::IterNext { .. } => out.push_str("    iter_next\n"),
        StatementKind::Nop => {
            out.push_str("    nop\n");
        }
    }
}

fn emit_terminator(out: &mut String, terminator: &Terminator) {
    match terminator {
        Terminator::Goto { target } => {
            let _ = writeln!(out, "    jump block{}", target.as_u32());
        }
        Terminator::SwitchInt {
            discriminant,
            arms,
            default,
        } => {
            out.push_str("    switch_int ");
            emit_operand(out, discriminant);
            out.push_str(" {");
            for (i, (value, target)) in arms.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                let _ = write!(out, "{value} -> block{}", target.as_u32());
            }
            let _ = writeln!(out, "}}, default block{}", default.as_u32());
        }
        Terminator::Return => {
            out.push_str("    return v0\n");
        }
        Terminator::Call {
            callee,
            args,
            destination,
            target,
        } => {
            out.push_str("    ");
            emit_place(out, destination);
            out.push_str(" = call ");
            emit_operand(out, callee);
            out.push('(');
            for (i, arg) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                emit_operand(out, arg);
            }
            out.push(')');
            match target {
                Some(block) => {
                    let _ = writeln!(out, " -> block{}", block.as_u32());
                }
                None => out.push_str(" -> diverge\n"),
            }
        }
        Terminator::Assert {
            cond,
            expected,
            msg,
            target,
        } => {
            let _ = write!(out, "    assert ");
            emit_operand(out, cond);
            let _ = writeln!(
                out,
                " == {expected:?} else panic({msg:?}) -> block{}",
                target.as_u32()
            );
        }
        Terminator::Unreachable => {
            out.push_str("    unreachable\n");
        }
        Terminator::Panic { message } => {
            let _ = writeln!(out, "    panic {message:?}");
        }
        Terminator::Drop { place, target } => {
            out.push_str("    drop ");
            emit_place(out, place);
            let _ = writeln!(out, " -> block{}", target.as_u32());
        }
    }
}

fn emit_place(out: &mut String, place: &Place) {
    let _ = write!(out, "v{}", place.local.as_u32());
    for projection in &place.projection {
        match projection {
            gossamer_mir::Projection::Deref => out.push_str(".*"),
            gossamer_mir::Projection::Field(idx) => {
                let _ = write!(out, ".f{idx}");
            }
            gossamer_mir::Projection::Index(local) => {
                let _ = write!(out, "[v{}]", local.as_u32());
            }
            gossamer_mir::Projection::Downcast(variant) => {
                let _ = write!(out, " as variant#{variant}");
            }
            gossamer_mir::Projection::Discriminant => out.push_str(".discr"),
        }
    }
}

fn emit_operand(out: &mut String, operand: &Operand) {
    match operand {
        Operand::Copy(place) => emit_place(out, place),
        Operand::Const(value) => emit_const(out, value),
        Operand::FnRef { def, .. } => {
            let _ = write!(out, "fn#{}", def.local);
        }
    }
}

fn emit_const(out: &mut String, value: &ConstValue) {
    match value {
        ConstValue::Unit => out.push_str("()"),
        ConstValue::Bool(b) => {
            let _ = write!(out, "{b}");
        }
        ConstValue::Int(i) => {
            let _ = write!(out, "{i}");
        }
        ConstValue::Float(bits) => {
            let f = f64::from_bits(*bits);
            let _ = write!(out, "{f}");
        }
        ConstValue::Char(c) => {
            let _ = write!(out, "'{c}'");
        }
        ConstValue::Str(s) => {
            let _ = write!(out, "{s:?}");
        }
    }
}

fn emit_rvalue(out: &mut String, rvalue: &Rvalue) {
    match rvalue {
        Rvalue::Use(operand) => emit_operand(out, operand),
        Rvalue::BinaryOp { op, lhs, rhs } => {
            let _ = write!(out, "{} ", binop_mnemonic(*op));
            emit_operand(out, lhs);
            out.push_str(", ");
            emit_operand(out, rhs);
        }
        Rvalue::UnaryOp { op, operand } => {
            let _ = write!(out, "{} ", unop_mnemonic(*op));
            emit_operand(out, operand);
        }
        Rvalue::Cast { operand, target } => {
            emit_operand(out, operand);
            let _ = write!(out, " as ty#{}", target.as_u32());
        }
        Rvalue::Aggregate { kind, operands } => {
            let label = match kind {
                gossamer_mir::AggregateKind::Tuple => "tuple".to_string(),
                gossamer_mir::AggregateKind::Array => "array".to_string(),
                gossamer_mir::AggregateKind::Adt { def, variant } => {
                    format!("adt#{}.v{variant}", def.local)
                }
            };
            let _ = write!(out, "{label}(");
            for (i, op) in operands.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                emit_operand(out, op);
            }
            out.push(')');
        }
        Rvalue::Len(place) => {
            out.push_str("len ");
            emit_place(out, place);
        }
        Rvalue::Repeat { value, count } => {
            out.push('[');
            emit_operand(out, value);
            let _ = write!(out, "; {count}]");
        }
        Rvalue::Ref { mutable, place } => {
            out.push_str(if *mutable { "&mut " } else { "&" });
            emit_place(out, place);
        }
        Rvalue::CallIntrinsic { name, args } => {
            let _ = write!(out, "intrinsic_{name}(");
            for (i, op) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                emit_operand(out, op);
            }
            out.push(')');
        }
        Rvalue::StaticLoad(sref) => {
            let _ = write!(out, "static_load @{}", sref.symbol);
        }
    }
}

fn binop_mnemonic(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "iadd",
        BinOp::WrappingAdd => "iadd.wrap",
        BinOp::Sub => "isub",
        BinOp::WrappingSub => "isub.wrap",
        BinOp::Mul => "imul",
        BinOp::WrappingMul => "imul.wrap",
        BinOp::Div => "sdiv",
        BinOp::Rem => "srem",
        BinOp::BitAnd => "band",
        BinOp::BitOr => "bor",
        BinOp::BitXor => "bxor",
        BinOp::Shl => "ishl",
        BinOp::Shr => "sshr",
        BinOp::Eq => "icmp eq",
        BinOp::Ne => "icmp ne",
        BinOp::Lt => "icmp slt",
        BinOp::Le => "icmp sle",
        BinOp::Gt => "icmp sgt",
        BinOp::Ge => "icmp sge",
    }
}

fn unop_mnemonic(op: UnOp) -> &'static str {
    match op {
        UnOp::Neg => "ineg",
        UnOp::Not => "bnot",
    }
}
