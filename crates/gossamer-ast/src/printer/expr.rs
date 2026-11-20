//! Printing of expressions with precedence-aware parenthesisation.

#![forbid(unsafe_code)]

use crate::common::{BinaryOp, UnaryOp};
use crate::expr::{
    ArrayExpr, Block, ClosureParam, Expr, ExprKind, FieldSelector, Label, Literal, MatchArm,
    SelectArm, SelectOp, StructExprField,
};
use crate::stmt::{Stmt, StmtKind};

use super::Printer;

/// Virtual precedence level used to gate parenthesisation.
///
/// Higher numbers bind tighter. Binary operator slots are derived from the
/// SPEC §4.7 table by subtracting the SPEC level from a fixed base, so a SPEC
/// level of 5 (`*`) produces a larger binding value than a SPEC level of 6
/// (`+`). Primary expressions sit at `u8::MAX` and statement-like constructs
/// (`return`, `break`, assignment) live at level `0`.
pub(super) type Precedence = u8;

const PREC_PRIMARY: Precedence = u8::MAX;
const PREC_POSTFIX: Precedence = 250;
const PREC_UNARY: Precedence = 240;
const PREC_CAST: Precedence = 230;
const PREC_RANGE: Precedence = 20;
const PREC_ASSIGN: Precedence = 10;
const PREC_STMT: Precedence = 0;

/// Converts a SPEC §4.7 level (where lower is tighter) into this printer's
/// binding-strength scale (where higher is tighter).
fn binding_of(op: BinaryOp) -> Precedence {
    const BASE: u8 = 200;
    BASE.saturating_sub(op.precedence())
}

impl Printer {
    /// Renders an expression at statement-level precedence (no outer parens).
    pub fn print_expr(&mut self, expr: &Expr) {
        self.print_expr_prec(expr, PREC_STMT);
    }

    pub(super) fn print_expr_prec(&mut self, expr: &Expr, min_prec: Precedence) {
        let prec = expr_precedence(expr);
        if prec < min_prec {
            self.write("(");
            self.print_expr_inner(expr);
            self.write(")");
        } else {
            self.print_expr_inner(expr);
        }
    }

    fn print_expr_inner(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Literal(lit) => self.print_literal(lit),
            ExprKind::Path(path) => self.print_path_expr(path),
            ExprKind::Call { callee, args } => self.print_call(callee, args),
            ExprKind::MethodCall {
                receiver,
                name,
                generics,
                args,
                ..
            } => self.print_method_call(receiver, name, generics, args),
            ExprKind::FieldAccess { receiver, field } => self.print_field_access(receiver, field),
            ExprKind::Index { base, index } => self.print_index(base, index),
            ExprKind::Unary { op, operand } => self.print_unary(*op, operand),
            ExprKind::Binary { op, lhs, rhs } => self.print_binary(*op, lhs, rhs),
            ExprKind::Assign { op, place, value } => {
                self.print_expr_prec(place, PREC_ASSIGN + 1);
                self.write(" ");
                self.write(op.as_str());
                self.write(" ");
                self.print_expr_prec(value, PREC_ASSIGN);
            }
            ExprKind::Cast {
                value,
                ty,
                wrapping,
            } => {
                self.print_expr_prec(value, PREC_CAST);
                self.write(if *wrapping { " as% " } else { " as " });
                self.print_type(ty);
            }
            ExprKind::Try(inner) => {
                self.print_expr_prec(inner, PREC_POSTFIX);
                self.write("?");
            }
            ExprKind::Block(block) => self.print_block(block),
            ExprKind::Unsafe(block) => {
                self.write("unsafe ");
                self.print_block(block);
            }
            ExprKind::Tuple(items) => self.print_tuple_expr(items),
            ExprKind::MapLiteral(entries) => self.print_map_literal(entries),
            ExprKind::SetLiteral(entries) => self.print_set_literal(entries),
            ExprKind::Array(array) => self.print_array(array),
            ExprKind::FixedArray(array) => self.print_fixed_array(array),
            ExprKind::Select(arms) => self.print_select(arms),
            _ => self.print_expr_control(expr),
        }
    }

    fn print_expr_control(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => self.print_if(condition, then_branch, else_branch.as_deref()),
            ExprKind::Match { scrutinee, arms } => self.print_match(scrutinee, arms),
            ExprKind::Loop { label, body } => {
                self.print_label(label.as_ref());
                self.write("loop ");
                self.print_expr(body);
            }
            ExprKind::While {
                label,
                condition,
                body,
            } => {
                self.print_label(label.as_ref());
                self.write("while ");
                self.print_expr(condition);
                self.write(" ");
                self.print_expr(body);
            }
            ExprKind::For {
                label,
                pattern,
                iter,
                body,
            } => self.print_for(label.as_ref(), pattern, iter, body),
            ExprKind::Closure { params, ret, body } => {
                self.print_closure(params, ret.as_ref(), body);
            }
            ExprKind::Range { start, end, kind } => {
                if let Some(start) = start {
                    self.print_expr_prec(start, PREC_RANGE + 1);
                }
                self.write(kind.as_str());
                if let Some(end) = end {
                    self.print_expr_prec(end, PREC_RANGE + 1);
                }
            }
            ExprKind::Struct {
                path, fields, base, ..
            } => {
                self.print_struct_expr(path, fields, base.as_deref());
            }
            _ => self.print_expr_terminal(expr),
        }
    }

    fn print_expr_terminal(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Return(value) => {
                self.write("return");
                if let Some(expr) = value {
                    self.write(" ");
                    self.print_expr(expr);
                }
            }
            ExprKind::Break { label, value } => {
                self.write("break");
                if let Some(label) = label {
                    self.write(" '");
                    self.write(&label.name);
                }
                if let Some(expr) = value {
                    self.write(" ");
                    self.print_expr(expr);
                }
            }
            ExprKind::Continue { label } => {
                self.write("continue");
                if let Some(label) = label {
                    self.write(" '");
                    self.write(&label.name);
                }
            }
            _ => {}
        }
    }

    fn print_field_access(&mut self, receiver: &Expr, field: &FieldSelector) {
        self.print_expr_prec(receiver, PREC_POSTFIX);
        self.write(".");
        match field {
            FieldSelector::Named(ident) => self.write_ident(ident),
            FieldSelector::Index(index) => self.write(&index.to_string()),
        }
    }

    fn print_index(&mut self, base: &Expr, index: &Expr) {
        self.print_expr_prec(base, PREC_POSTFIX);
        self.write("[");
        self.print_expr(index);
        self.write("]");
    }

    fn print_for(
        &mut self,
        label: Option<&Label>,
        pattern: &crate::pattern::Pattern,
        iter: &Expr,
        body: &Expr,
    ) {
        self.print_label(label);
        self.write("for ");
        self.print_pattern(pattern);
        self.write(" in ");
        self.print_expr(iter);
        self.write(" ");
        self.print_expr(body);
    }

    fn print_call(&mut self, callee: &Expr, args: &[Expr]) {
        self.print_expr_prec(callee, PREC_POSTFIX);
        self.print_arg_list(args);
    }

    /// Emits `(a, b, c)` on one line when it fits, else breaks each
    /// argument onto its own indented line.
    fn print_arg_list(&mut self, args: &[Expr]) {
        self.write("(");
        if args.is_empty() {
            self.write(")");
            return;
        }
        let inline = self.speculative(|probe| {
            for (index, arg) in args.iter().enumerate() {
                if index > 0 {
                    probe.write(", ");
                }
                probe.print_expr(arg);
            }
        });
        if self.current_column() + inline.len() < super::MAX_LINE_WIDTH && !inline.contains('\n') {
            self.write(&inline);
            self.write(")");
            return;
        }
        self.newline();
        self.indent_in();
        for arg in args {
            self.print_expr(arg);
            self.write(",");
            self.newline();
        }
        self.indent_out();
        self.write(")");
    }

    fn print_method_call(
        &mut self,
        receiver: &Expr,
        name: &crate::common::Ident,
        generics: &[crate::ty::GenericArg],
        args: &[Expr],
    ) {
        self.print_expr_prec(receiver, PREC_POSTFIX);
        self.write(".");
        self.write_ident(name);
        if !generics.is_empty() {
            self.write("::<");
            for (index, arg) in generics.iter().enumerate() {
                if index > 0 {
                    self.write(", ");
                }
                self.print_generic_arg(arg);
            }
            self.write(">");
        }
        self.print_arg_list(args);
    }

    fn print_unary(&mut self, op: UnaryOp, operand: &Expr) {
        self.write(op.as_str());
        self.print_expr_prec(operand, PREC_UNARY);
    }

    fn print_binary(&mut self, op: BinaryOp, lhs: &Expr, rhs: &Expr) {
        if matches!(op, BinaryOp::PipeGt) {
            self.print_pipe_chain(lhs, rhs);
            return;
        }
        let prec = binding_of(op);
        self.print_expr_prec(lhs, prec);
        self.write(" ");
        self.write(op.as_str());
        self.write(" ");
        self.print_expr_prec(rhs, prec.saturating_add(1));
    }

    fn print_pipe_chain(&mut self, lhs: &Expr, rhs: &Expr) {
        let mut hops: Vec<&Expr> = Vec::new();
        hops.push(rhs);
        let mut current = lhs;
        while let ExprKind::Binary {
            op: BinaryOp::PipeGt,
            lhs: next_lhs,
            rhs: next_rhs,
        } = &current.kind
        {
            hops.push(next_rhs);
            current = next_lhs;
        }
        hops.reverse();
        let source = current;
        let pipe_binding = binding_of(BinaryOp::PipeGt);
        self.print_expr_prec(source, pipe_binding);
        if hops.len() >= 2 {
            self.indent_in();
            for hop in &hops {
                self.newline();
                self.write("|> ");
                self.print_expr_prec(hop, pipe_binding.saturating_add(1));
            }
            self.indent_out();
        } else if let Some(hop) = hops.first() {
            self.write(" |> ");
            self.print_expr_prec(hop, pipe_binding.saturating_add(1));
        }
    }

    fn print_if(&mut self, condition: &Expr, then_branch: &Expr, else_branch: Option<&Expr>) {
        self.write("if ");
        self.print_expr(condition);
        self.write(" ");
        self.print_expr(then_branch);
        if let Some(else_branch) = else_branch {
            self.write(" else ");
            self.print_expr(else_branch);
        }
    }

    fn print_match(&mut self, scrutinee: &Expr, arms: &[MatchArm]) {
        self.write("match ");
        self.print_expr(scrutinee);
        self.write(" {");
        self.newline();
        self.indent_in();
        for arm in arms {
            self.print_match_arm(arm);
            self.newline();
        }
        self.indent_out();
        self.write("}");
    }

    fn print_match_arm(&mut self, arm: &MatchArm) {
        self.print_pattern(&arm.pattern);
        if let Some(guard) = &arm.guard {
            self.write(" if ");
            self.print_expr(guard);
        }
        self.write(" => ");
        self.print_expr(&arm.body);
        self.write(",");
    }

    fn print_label(&mut self, label: Option<&Label>) {
        if let Some(label) = label {
            self.write("'");
            self.write(&label.name);
            self.write(": ");
        }
    }

    pub(super) fn print_block(&mut self, block: &Block) {
        if block.stmts.is_empty() && block.tail.is_none() {
            self.write("{}");
            return;
        }
        self.write("{");
        self.newline();
        self.indent_in();
        for stmt in &block.stmts {
            self.print_stmt(stmt);
            self.newline();
        }
        if let Some(tail) = &block.tail {
            self.print_expr(tail);
            self.newline();
        }
        self.indent_out();
        self.write("}");
    }

    fn print_closure(
        &mut self,
        params: &[ClosureParam],
        ret: Option<&crate::ty::Type>,
        body: &Expr,
    ) {
        self.write("|");
        for (index, param) in params.iter().enumerate() {
            if index > 0 {
                self.write(", ");
            }
            self.print_pattern(&param.pattern);
            if let Some(ty) = &param.ty {
                self.write(": ");
                self.print_type(ty);
            }
        }
        self.write("|");
        if let Some(ret) = ret {
            self.write(" -> ");
            self.print_type(ret);
            self.write(" ");
            self.print_expr(body);
        } else {
            self.write(" ");
            self.print_expr(body);
        }
    }

    fn print_tuple_expr(&mut self, items: &[Expr]) {
        self.write("(");
        for (index, item) in items.iter().enumerate() {
            if index > 0 {
                self.write(", ");
            }
            self.print_expr(item);
        }
        if items.len() == 1 {
            self.write(",");
        }
        self.write(")");
    }

    fn print_struct_expr(
        &mut self,
        path: &crate::expr::PathExpr,
        fields: &[StructExprField],
        base: Option<&Expr>,
    ) {
        self.print_path_expr(path);
        self.write("(");
        for (index, field) in fields.iter().enumerate() {
            if index > 0 {
                self.write(", ");
            }
            if let Some(value) = &field.value {
                self.print_expr(value);
            } else {
                self.write_ident(&field.name);
            }
        }
        if let Some(base) = base {
            if !fields.is_empty() {
                self.write(", ");
            }
            self.write("..");
            self.print_expr(base);
        }
        self.write(")");
    }

    fn print_array(&mut self, array: &ArrayExpr) {
        match array {
            ArrayExpr::List(items) => {
                self.write("[");
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        self.write(", ");
                    }
                    self.print_expr(item);
                }
                self.write("]");
            }
            ArrayExpr::Repeat { value, count } => {
                self.write("[");
                self.print_expr(value);
                self.write("; ");
                self.print_expr(count);
                self.write("]");
            }
        }
    }

    fn print_map_literal(&mut self, entries: &[Expr]) {
        self.write("{");
        for (index, entry) in entries.iter().enumerate() {
            if index > 0 {
                self.write(", ");
            }
            if let ExprKind::Tuple(parts) = &entry.kind
                && let [key, value] = parts.as_slice()
            {
                self.print_expr(key);
                self.write(": ");
                self.print_expr(value);
                continue;
            }
            self.print_expr(entry);
        }
        self.write("}");
    }

    fn print_set_literal(&mut self, entries: &[Expr]) {
        self.write("#{");
        for (index, entry) in entries.iter().enumerate() {
            if index > 0 {
                self.write(", ");
            }
            self.print_expr(entry);
        }
        self.write("}");
    }

    fn print_fixed_array(&mut self, array: &ArrayExpr) {
        match array {
            ArrayExpr::List(items) => {
                self.write("#[");
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        self.write(", ");
                    }
                    self.print_expr(item);
                }
                self.write("]");
            }
            ArrayExpr::Repeat { value, count } => {
                self.write("#[");
                self.print_expr(value);
                self.write("; ");
                self.print_expr(count);
                self.write("]");
            }
        }
    }

    fn print_select(&mut self, arms: &[SelectArm]) {
        self.write("select {");
        self.newline();
        self.indent_in();
        for arm in arms {
            self.print_select_arm(arm);
            self.newline();
        }
        self.indent_out();
        self.write("}");
    }

    fn print_select_arm(&mut self, arm: &SelectArm) {
        match &arm.op {
            SelectOp::Recv { pattern, channel } => {
                self.print_pattern(pattern);
                self.write(" = ");
                self.print_expr(channel);
                self.write(".recv()");
            }
            SelectOp::Send { channel, value } => {
                self.print_expr(channel);
                self.write(".send(");
                self.print_expr(value);
                self.write(")");
            }
            SelectOp::Default => self.write("default"),
        }
        self.write(" => ");
        self.print_expr(&arm.body);
        self.write(",");
    }

    /// Renders a literal, escaping string contents.
    pub fn print_literal(&mut self, literal: &Literal) {
        match literal {
            Literal::Int(raw) | Literal::Float(raw) => self.write(raw),
            Literal::String(value) => {
                self.write("\"");
                self.write_escaped_str(value);
                self.write("\"");
            }
            Literal::RawString { hashes, value } => {
                self.write("r");
                for _ in 0..*hashes {
                    self.write("#");
                }
                self.write("\"");
                self.write(value);
                self.write("\"");
                for _ in 0..*hashes {
                    self.write("#");
                }
            }
            Literal::Char(ch) => {
                self.write("'");
                self.write(&escape_char(*ch));
                self.write("'");
            }
            Literal::Byte(byte) => {
                self.write("b'");
                self.write(&escape_byte(*byte));
                self.write("'");
            }
            Literal::ByteString(bytes) => {
                self.write("b\"");
                for byte in bytes {
                    self.write(&escape_byte(*byte));
                }
                self.write("\"");
            }
            Literal::RawByteString { hashes, value } => {
                self.write("br");
                for _ in 0..*hashes {
                    self.write("#");
                }
                self.write("\"");
                for byte in value {
                    self.write(&String::from(*byte as char));
                }
                self.write("\"");
                for _ in 0..*hashes {
                    self.write("#");
                }
            }
            Literal::Bool(value) => self.write(if *value { "true" } else { "false" }),
            Literal::Unit => self.write("()"),
        }
    }

    /// Renders a statement including its trailing newline semantics.
    pub fn print_stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Let { pattern, ty, init } => {
                self.write("let ");
                self.print_pattern(pattern);
                if let Some(ty) = ty {
                    self.write(": ");
                    self.print_type(ty);
                }
                if let Some(expr) = init {
                    self.write(" = ");
                    self.print_expr(expr);
                }
            }
            StmtKind::Expr { expr, has_semi } => {
                self.print_expr(expr);
                if *has_semi {
                    self.write(";");
                }
            }
            StmtKind::Item(item) => self.print_item(item),
            StmtKind::Defer(expr) => {
                self.write("defer ");
                self.print_expr(expr);
            }
        }
    }
}

fn expr_precedence(expr: &Expr) -> Precedence {
    match &expr.kind {
        ExprKind::Literal(_)
        | ExprKind::Path(_)
        | ExprKind::Block(_)
        | ExprKind::Tuple(_)
        | ExprKind::MapLiteral(_)
        | ExprKind::SetLiteral(_)
        | ExprKind::Array(_)
        | ExprKind::FixedArray(_)
        | ExprKind::Struct { .. }
        | ExprKind::Unsafe(_)
        | ExprKind::If { .. }
        | ExprKind::Match { .. }
        | ExprKind::Loop { .. }
        | ExprKind::While { .. }
        | ExprKind::For { .. }
        | ExprKind::Select(_) => PREC_PRIMARY,
        ExprKind::Call { .. }
        | ExprKind::MethodCall { .. }
        | ExprKind::FieldAccess { .. }
        | ExprKind::Index { .. }
        | ExprKind::Try(_) => PREC_POSTFIX,
        ExprKind::Unary { .. } => PREC_UNARY,
        ExprKind::Cast { .. } => PREC_CAST,
        ExprKind::Binary { op, .. } => binding_of(*op),
        ExprKind::Range { .. } => PREC_RANGE,
        ExprKind::Assign { .. } => PREC_ASSIGN,
        ExprKind::Return(_)
        | ExprKind::Break { .. }
        | ExprKind::Continue { .. }
        | ExprKind::Closure { .. } => PREC_STMT,
        ExprKind::Error => PREC_PRIMARY,
    }
}

fn escape_char(ch: char) -> String {
    match ch {
        '\\' => "\\\\".into(),
        '\'' => "\\'".into(),
        '\n' => "\\n".into(),
        '\r' => "\\r".into(),
        '\t' => "\\t".into(),
        '\0' => "\\0".into(),
        other if (other as u32) < 0x20 => {
            let code = other as u32;
            format!("\\u{{{code:x}}}")
        }
        other => other.to_string(),
    }
}

fn escape_byte(byte: u8) -> String {
    match byte {
        b'\\' => "\\\\".into(),
        b'\'' => "\\'".into(),
        b'\"' => "\\\"".into(),
        b'\n' => "\\n".into(),
        b'\r' => "\\r".into(),
        b'\t' => "\\t".into(),
        0 => "\\0".into(),
        0x20..=0x7e => String::from(byte as char),
        other => format!("\\x{other:02x}"),
    }
}
