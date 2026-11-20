//! Reads of a binding declared without a value, on a path where nothing has
//! assigned it yet.
//!
//! `let mut x: i64` declares a binding whose value the code after it
//! supplies. Every read of it must follow an assignment on every path that
//! reaches the read: a branch that skips the assignment leaves no value to
//! read. The walk follows evaluation order, carrying the bindings that may
//! still be unassigned; branches join by union, a path that cannot continue
//! (`return`, `break`, `continue`, a call answering `!`) joins as nothing,
//! and a loop's exit joins the state before it with every `break` out of it.
//! A closure reads its captures where it is written, and a `defer` reads its
//! bindings where it is declared, so both are checked at that point.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};

use gossamer_ast::{
    ArrayExpr, AssignOp, BinaryOp, Block, Expr, ExprKind, ImplItem, Item, ItemKind, ModBody,
    NodeId, Pattern, PatternKind, SelectOp, SourceFile, Stmt, StmtKind, TraitItem,
};
use gossamer_lex::Span;
use gossamer_resolve::{Resolution, Resolutions};

use crate::context::TyCtxt;
use crate::table::TypeTable;
use crate::ty::TyKind;

/// A read of a binding that may not have been assigned on every path to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnassignedReadDiagnostic {
    /// The binding read.
    pub name: String,
    /// The read.
    pub span: Span,
}

impl UnassignedReadDiagnostic {
    /// The code this refusal carries.
    pub const CODE: &'static str = "GT0129";

    /// Renders this refusal as a structured diagnostic.
    #[must_use]
    pub fn to_diagnostic(&self) -> gossamer_diagnostics::Diagnostic {
        use gossamer_diagnostics::{Code, Diagnostic, Location};
        let name = &self.name;
        Diagnostic::error(
            Code(Self::CODE),
            format!("`{name}` is read before it is assigned on every path"),
        )
        .with_primary(
            Location::new(self.span.file, self.span),
            format!("a path reaches this read without assigning `{name}`"),
        )
        .with_help(format!(
            "give `{name}` a value where it is declared, or assign it on every path before this read"
        ))
    }
}

/// Checks every function in `sf` for reads of a binding declared without a
/// value on a path where it has not been assigned.
#[must_use]
pub fn check_definite_assignment(
    sf: &SourceFile,
    resolutions: &Resolutions,
    table: &TypeTable,
    tcx: &TyCtxt,
) -> Vec<UnassignedReadDiagnostic> {
    let mut walk = Walk {
        resolutions,
        table,
        tcx,
        names: HashMap::new(),
        loops: Vec::new(),
        reported: HashSet::new(),
        out: Vec::new(),
    };
    walk.items(&sf.items);
    walk.out
}

/// The bindings that may still be unassigned, or `None` where no path
/// reaches.
type State = Option<HashSet<NodeId>>;

fn join(a: State, b: State) -> State {
    match (a, b) {
        (None, other) | (other, None) => other,
        (Some(mut a), Some(b)) => {
            a.extend(b);
            Some(a)
        }
    }
}

/// A loop being walked: its label and the states its `break`s leave with.
struct LoopFrame {
    label: Option<String>,
    breaks: State,
}

struct Walk<'a> {
    resolutions: &'a Resolutions,
    table: &'a TypeTable,
    tcx: &'a TyCtxt,
    /// The name of every binding declared without a value.
    names: HashMap<NodeId, String>,
    loops: Vec<LoopFrame>,
    /// Bindings already reported, so one missing assignment is one finding.
    reported: HashSet<NodeId>,
    out: Vec<UnassignedReadDiagnostic>,
}

impl Walk<'_> {
    fn items(&mut self, items: &[Item]) {
        for item in items {
            self.item(item);
        }
    }

    fn item(&mut self, item: &Item) {
        match &item.kind {
            ItemKind::Fn(decl) => self.body(decl.body.as_deref()),
            ItemKind::Impl(decl) => {
                for item in &decl.items {
                    if let ImplItem::Fn(decl) = item {
                        self.body(decl.body.as_deref());
                    }
                }
            }
            ItemKind::Trait(decl) => {
                for item in &decl.items {
                    if let TraitItem::Fn(decl) = item {
                        self.body(decl.body.as_deref());
                    }
                }
            }
            ItemKind::Mod(decl) => {
                if let ModBody::Inline(inner) = &decl.body {
                    self.items(inner);
                }
            }
            _ => {}
        }
    }

    /// Walks one function body from a state with nothing unassigned.
    fn body(&mut self, body: Option<&Expr>) {
        if let Some(body) = body {
            let loops = std::mem::take(&mut self.loops);
            self.expr(body, Some(HashSet::new()));
            self.loops = loops;
        }
    }

    fn read(&mut self, id: NodeId, span: Span, state: &State) {
        let Some(Resolution::Local(binding)) = self.resolutions.get(id) else {
            return;
        };
        if state.as_ref().is_some_and(|open| open.contains(&binding))
            && self.reported.insert(binding)
        {
            let name = self.names.get(&binding).cloned().unwrap_or_default();
            self.out.push(UnassignedReadDiagnostic { name, span });
        }
    }

    fn exprs<'e>(&mut self, exprs: impl IntoIterator<Item = &'e Expr>, mut state: State) -> State {
        for expr in exprs {
            state = self.expr(expr, state);
        }
        state
    }

    fn block(&mut self, block: &Block, mut state: State) -> State {
        for stmt in &block.stmts {
            state = self.stmt(stmt, state);
        }
        match &block.tail {
            Some(tail) => self.expr(tail, state),
            None => state,
        }
    }

    fn stmt(&mut self, stmt: &Stmt, state: State) -> State {
        match &stmt.kind {
            StmtKind::Let { pattern, init, .. } => {
                if let Some(init) = init {
                    return self.expr(init, state);
                }
                let mut state = state;
                self.declare_unassigned(pattern, &mut state);
                state
            }
            StmtKind::Expr { expr, .. } => self.expr(expr, state),
            StmtKind::Item(item) => {
                let loops = std::mem::take(&mut self.loops);
                self.item(item);
                self.loops = loops;
                state
            }
            StmtKind::Defer(expr) => {
                self.detached(expr, &state);
                state
            }
        }
    }

    fn declare_unassigned(&mut self, pattern: &Pattern, state: &mut State) {
        match &pattern.kind {
            PatternKind::Ident {
                name, subpattern, ..
            } => {
                self.names.insert(pattern.id, name.name.clone());
                if let Some(open) = state {
                    open.insert(pattern.id);
                }
                if let Some(sub) = subpattern {
                    self.declare_unassigned(sub, state);
                }
            }
            PatternKind::Tuple(parts) | PatternKind::Or(parts) => {
                for part in parts {
                    self.declare_unassigned(part, state);
                }
            }
            _ => {}
        }
    }

    /// Walks code that runs at another time than where it is written: its
    /// reads are checked here, and its assignments never reach the code
    /// around it.
    fn detached(&mut self, expr: &Expr, state: &State) {
        let loops = std::mem::take(&mut self.loops);
        self.expr(expr, state.clone());
        self.loops = loops;
    }

    fn assign_target(&mut self, place: &Expr, state: State) -> State {
        match &place.kind {
            ExprKind::Path(_) => {
                let mut state = state;
                if let Some(Resolution::Local(binding)) = self.resolutions.get(place.id)
                    && let Some(open) = &mut state
                {
                    open.remove(&binding);
                }
                state
            }
            ExprKind::Tuple(places) => {
                let mut state = state;
                for place in places {
                    state = self.assign_target(place, state);
                }
                state
            }
            // A write into part of a value reads the value it changes.
            _ => self.expr(place, state),
        }
    }

    fn loop_body(&mut self, label: Option<&str>, body: &Expr, state: State) -> State {
        self.loops.push(LoopFrame {
            label: label.map(str::to_string),
            breaks: None,
        });
        self.expr(body, state);
        self.loops.pop().and_then(|frame| frame.breaks)
    }

    /// `true` for a call that never returns: one typed `!`, or a call of the
    /// prelude's `panic`, `todo`, `unimplemented`, or `unreachable`, whose
    /// answer takes whatever type its position needs.
    fn diverges(&self, expr: &Expr) -> bool {
        if let ExprKind::Call { callee, .. } = &expr.kind
            && let ExprKind::Path(path) = &callee.kind
            && let [segment] = path.segments.as_slice()
            && matches!(
                segment.name.name.as_str(),
                "panic" | "todo" | "unimplemented" | "unreachable"
            )
            && self.resolutions.get(callee.id)
                == Some(Resolution::Import {
                    use_id: NodeId::DUMMY,
                })
        {
            return true;
        }
        self.table
            .get(expr.id)
            .is_some_and(|ty| matches!(self.tcx.kind(ty), Some(TyKind::Never)))
    }

    fn expr(&mut self, expr: &Expr, state: State) -> State {
        let state = self.expr_inner(expr, state);
        if matches!(
            expr.kind,
            ExprKind::Call { .. } | ExprKind::MethodCall { .. }
        ) && self.diverges(expr)
        {
            return None;
        }
        state
    }

    fn expr_inner(&mut self, expr: &Expr, state: State) -> State {
        match &expr.kind {
            ExprKind::Path(_) => {
                self.read(expr.id, expr.span, &state);
                state
            }
            ExprKind::Literal(_) | ExprKind::Error => state,
            ExprKind::Call { callee, args } => {
                let state = self.expr(callee, state);
                self.exprs(args, state)
            }
            ExprKind::MethodCall { receiver, args, .. } => {
                let state = self.expr(receiver, state);
                self.exprs(args, state)
            }
            ExprKind::FieldAccess { receiver, .. } => self.expr(receiver, state),
            ExprKind::Index { base, index } => {
                let state = self.expr(base, state);
                self.expr(index, state)
            }
            ExprKind::Unary { operand, .. }
            | ExprKind::Cast { value: operand, .. }
            | ExprKind::Try(operand) => self.expr(operand, state),
            ExprKind::Binary { op, lhs, rhs } => {
                let state = self.expr(lhs, state);
                if matches!(op, BinaryOp::And | BinaryOp::Or) {
                    let skipped = state.clone();
                    let ran = self.expr(rhs, state);
                    join(skipped, ran)
                } else {
                    self.expr(rhs, state)
                }
            }
            ExprKind::Assign { op, place, value } => {
                let state = self.expr(value, state);
                if *op == AssignOp::Assign {
                    self.assign_target(place, state)
                } else {
                    self.expr(place, state)
                }
            }
            ExprKind::Block(block) | ExprKind::Unsafe(block) => self.block(block, state),
            ExprKind::Closure { body, .. } => {
                self.detached(body, &state);
                state
            }
            ExprKind::Tuple(parts) | ExprKind::MapLiteral(parts) | ExprKind::SetLiteral(parts) => {
                self.exprs(parts, state)
            }
            ExprKind::Struct { fields, base, .. } => {
                let state = self.exprs(fields.iter().filter_map(|f| f.value.as_ref()), state);
                match base {
                    Some(base) => self.expr(base, state),
                    None => state,
                }
            }
            ExprKind::Array(array) | ExprKind::FixedArray(array) => match array {
                ArrayExpr::List(elems) => self.exprs(elems, state),
                ArrayExpr::Repeat { value, count } => {
                    let state = self.expr(value, state);
                    self.expr(count, state)
                }
            },
            ExprKind::Range { start, end, .. } => {
                let state = match start {
                    Some(start) => self.expr(start, state),
                    None => state,
                };
                match end {
                    Some(end) => self.expr(end, state),
                    None => state,
                }
            }
            ExprKind::If { .. } | ExprKind::Match { .. } | ExprKind::Select(_) => {
                self.branches(expr, state)
            }
            ExprKind::Loop { .. }
            | ExprKind::While { .. }
            | ExprKind::For { .. }
            | ExprKind::Return(_)
            | ExprKind::Break { .. }
            | ExprKind::Continue { .. } => self.transfers(expr, state),
        }
    }

    /// `if`, `match`, and `select`: each way through runs from the state
    /// before it, and the ways join after it.
    fn branches(&mut self, expr: &Expr, state: State) -> State {
        match &expr.kind {
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let state = self.expr(condition, state);
                let then = self.expr(then_branch, state.clone());
                let other = match else_branch {
                    Some(other) => self.expr(other, state),
                    None => state,
                };
                join(then, other)
            }
            ExprKind::Match { scrutinee, arms } => {
                let state = self.expr(scrutinee, state);
                let mut out = None;
                for arm in arms {
                    let mut entry = state.clone();
                    if let Some(guard) = &arm.guard {
                        entry = self.expr(guard, entry);
                    }
                    out = join(out, self.expr(&arm.body, entry));
                }
                if arms.is_empty() { state } else { out }
            }
            ExprKind::Select(arms) => {
                let mut entry = state;
                for arm in arms {
                    entry = match &arm.op {
                        SelectOp::Recv { channel, .. } => self.expr(channel, entry),
                        SelectOp::Send { channel, value } => {
                            let entry = self.expr(channel, entry);
                            self.expr(value, entry)
                        }
                        SelectOp::Default => entry,
                    };
                }
                let mut out = None;
                for arm in arms {
                    out = join(out, self.expr(&arm.body, entry.clone()));
                }
                if arms.is_empty() { entry } else { out }
            }
            _ => state,
        }
    }

    /// Loops and the expressions that leave a place: a loop joins the state
    /// before it with each `break` out of it, and `return`, `break`, and
    /// `continue` leave no state behind them.
    fn transfers(&mut self, expr: &Expr, state: State) -> State {
        match &expr.kind {
            ExprKind::Loop { label, body } => {
                self.loop_body(label.as_ref().map(|l| l.name.as_str()), body, state)
            }
            ExprKind::While {
                label,
                condition,
                body,
            } => {
                let state = self.expr(condition, state);
                let breaks =
                    self.loop_body(label.as_ref().map(|l| l.name.as_str()), body, state.clone());
                join(state, breaks)
            }
            ExprKind::For {
                label, iter, body, ..
            } => {
                let state = self.expr(iter, state);
                let breaks =
                    self.loop_body(label.as_ref().map(|l| l.name.as_str()), body, state.clone());
                join(state, breaks)
            }
            ExprKind::Return(value) => {
                if let Some(value) = value {
                    self.expr(value, state);
                }
                None
            }
            ExprKind::Break { label, value } => {
                let state = match value {
                    Some(value) => self.expr(value, state),
                    None => state,
                };
                let target = match label {
                    Some(label) => self
                        .loops
                        .iter()
                        .rposition(|frame| frame.label.as_deref() == Some(label.name.as_str())),
                    None => self.loops.len().checked_sub(1),
                };
                if let Some(index) = target {
                    let frame = &mut self.loops[index];
                    frame.breaks = join(frame.breaks.take(), state);
                }
                None
            }
            ExprKind::Continue { .. } => None,
            _ => state,
        }
    }
}
