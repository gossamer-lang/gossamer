//! A closure stored into the value it captures.
//!
//! A closure names the bindings it captures, so `o.callback = Some(|| o.x)`
//! leaves `o` holding a closure that holds `o`: a reference cycle neither
//! side ever releases, and one the runtime does not collect. The store is
//! reported where it is written: an assignment into a place under a binding
//! the stored value's closure captures, or a method that writes that
//! binding, such as a `push`, taking such a closure; a program's own method
//! stores only the arguments its body keeps in `self`. A closure bound by
//! `let` and stored later counts as the closure it names. A weak capture
//! (`[weak o] || ..`) names no strong reference to `o` and is never
//! reported.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};

use gossamer_ast::visitor::{Visitor, walk_expr, walk_pattern, walk_stmt};
use gossamer_ast::{
    ArrayExpr, Expr, ExprKind, FnDecl, FnParam, ImplItem, Item, ItemKind, ModBody, NodeId, Pattern,
    PatternKind, SourceFile, Stmt, StmtKind, TraitItem, UnaryOp,
};
use gossamer_lex::Span;
use gossamer_resolve::{Resolution, Resolutions};

use crate::table::TypeTable;

/// The lint these findings belong to.
pub const CLOSURE_SELF_CYCLE_LINT: &str = "closure_self_cycle";

/// A closure stored into a value it captures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosureSelfCycle {
    /// The store.
    pub span: Span,
    /// The binding the closure captures and is stored into.
    pub name: String,
}

/// Every store in `sf` of a closure into a value it captures.
#[must_use]
pub fn find_closure_self_cycles(
    sf: &SourceFile,
    resolutions: &Resolutions,
    table: &TypeTable,
) -> Vec<ClosureSelfCycle> {
    // A method that hands an argument to another of the program's methods
    // keeps it where that one does, so the table grows until it settles.
    let mut kept_args = HashMap::new();
    loop {
        let mut next = HashMap::new();
        for item in &sf.items {
            note_kept_args(item, resolutions, table, &kept_args, &mut next);
        }
        if next == kept_args {
            break;
        }
        kept_args = next;
    }
    let mut finder = Finder {
        resolutions,
        table,
        closure_bindings: HashMap::new(),
        kept_args,
        out: Vec::new(),
    };
    finder.visit_source_file(sf);
    finder.out
}

struct Finder<'a> {
    resolutions: &'a Resolutions,
    table: &'a TypeTable,
    /// Bindings `let`-bound to a closure written in place, with the locals
    /// the closure names.
    closure_bindings: HashMap<NodeId, HashSet<NodeId>>,
    /// For each method name the program declares, which of its arguments
    /// some declaration of that name keeps in `self`.
    kept_args: HashMap<String, Vec<bool>>,
    out: Vec<ClosureSelfCycle>,
}

/// Records, for every method `item` declares, which arguments it keeps.
fn note_kept_args(
    item: &Item,
    resolutions: &Resolutions,
    table: &TypeTable,
    known: &HashMap<String, Vec<bool>>,
    kept: &mut HashMap<String, Vec<bool>>,
) {
    let mut note = |decl: &FnDecl| {
        let keeps = arguments_kept(decl, resolutions, table, known);
        let entry = kept.entry(decl.name.name.clone()).or_default();
        if entry.len() < keeps.len() {
            entry.resize(keeps.len(), false);
        }
        for (slot, keeps) in entry.iter_mut().zip(keeps) {
            *slot |= keeps;
        }
    };
    match &item.kind {
        ItemKind::Impl(decl) => {
            for member in &decl.items {
                if let ImplItem::Fn(decl) = member {
                    note(decl);
                }
            }
        }
        ItemKind::Trait(decl) => {
            for member in &decl.items {
                if let TraitItem::Fn(decl) = member {
                    note(decl);
                }
            }
        }
        ItemKind::Mod(decl) => {
            if let ModBody::Inline(items) = &decl.body {
                for item in items {
                    note_kept_args(item, resolutions, table, known, kept);
                }
            }
        }
        _ => {}
    }
}

/// Which of a method's arguments (after `self`) its body keeps in `self`:
/// assigns into a place under `self`, or hands to a call that writes a place
/// under `self` and keeps it (by `known` for the program's methods),
/// directly or through a local made from it.
fn arguments_kept(
    decl: &FnDecl,
    resolutions: &Resolutions,
    table: &TypeTable,
    known: &HashMap<String, Vec<bool>>,
) -> Vec<bool> {
    let params: Vec<Option<NodeId>> = decl
        .params
        .iter()
        .filter_map(|param| match param {
            FnParam::Receiver(_) => None,
            FnParam::Typed { pattern, .. } => Some(match &pattern.kind {
                PatternKind::Ident { .. } => Some(pattern.id),
                _ => None,
            }),
        })
        .collect();
    let Some(body) = &decl.body else {
        return vec![false; params.len()];
    };
    params
        .iter()
        .map(|param| {
            // A parameter bound by a pattern is assumed kept.
            let Some(param) = param else {
                return true;
            };
            let mut search = KeptSearch {
                resolutions,
                table,
                known,
                from: HashSet::from([*param]),
                kept: false,
            };
            // A local made from the parameter carries it on, and may be
            // declared before or after the statement that makes it so.
            loop {
                let before = search.from.len();
                search.visit_expr(body);
                if search.kept || search.from.len() == before {
                    break search.kept;
                }
            }
        })
        .collect()
}

/// Whether a method body keeps any of the bindings in `from` in `self`.
struct KeptSearch<'a> {
    resolutions: &'a Resolutions,
    table: &'a TypeTable,
    known: &'a HashMap<String, Vec<bool>>,
    from: HashSet<NodeId>,
    kept: bool,
}

impl KeptSearch<'_> {
    fn names_kept_binding(&self, expr: &Expr) -> bool {
        let mut named = Named {
            resolutions: self.resolutions,
            locals: HashSet::new(),
        };
        named.visit_expr(expr);
        !named.locals.is_disjoint(&self.from)
    }
}

/// Whether `place` lies under `self`.
fn under_self(place: &Expr) -> bool {
    match &place.kind {
        ExprKind::Unary {
            op: UnaryOp::RefMut,
            operand,
        } => under_self(operand),
        _ => root_name(place).as_deref() == Some("self"),
    }
}

impl Visitor for KeptSearch<'_> {
    fn visit_stmt(&mut self, stmt: &Stmt) {
        if let StmtKind::Let {
            pattern,
            init: Some(init),
            ..
        } = &stmt.kind
            && self.names_kept_binding(init)
        {
            let mut bound = Vec::new();
            pattern_bindings(pattern, &mut bound);
            self.from.extend(bound);
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Assign { place, value, .. }
                if under_self(place) && self.names_kept_binding(value) =>
            {
                self.kept = true;
            }
            ExprKind::MethodCall {
                receiver,
                name,
                args,
                ..
            } if under_self(receiver) && self.table.writes_receiver(receiver.id) => {
                let known = method_kept_args(self.known, &name.name);
                if args.iter().enumerate().any(|(i, arg)| {
                    known.is_none_or(|known| known.get(i) != Some(&false))
                        && self.names_kept_binding(arg)
                }) {
                    self.kept = true;
                }
            }
            ExprKind::Call { args, .. }
                if args.iter().any(under_self)
                    && args.iter().any(|arg| self.names_kept_binding(arg)) =>
            {
                self.kept = true;
            }
            _ => {}
        }
        walk_expr(self, expr);
    }
}

/// Which arguments a receiver-writing call of the method `name` keeps, where
/// that is known: a program's method keeps what its body stores in `self`,
/// while a built-in one, and a name both carry, keeps what it is given.
fn method_kept_args<'a>(kept: &'a HashMap<String, Vec<bool>>, name: &str) -> Option<&'a Vec<bool>> {
    if crate::is_mutating_method_name(name) {
        return None;
    }
    kept.get(name)
}

/// The bindings a pattern introduces.
fn pattern_bindings(pattern: &Pattern, out: &mut Vec<NodeId>) {
    struct Bindings<'a>(&'a mut Vec<NodeId>);
    impl Visitor for Bindings<'_> {
        fn visit_pattern(&mut self, pattern: &Pattern) {
            if let PatternKind::Ident { .. } = &pattern.kind {
                self.0.push(pattern.id);
            }
            walk_pattern(self, pattern);
        }
    }
    Bindings(out).visit_pattern(pattern);
}

/// The locals named anywhere inside `expr`.
struct Named<'a> {
    resolutions: &'a Resolutions,
    locals: HashSet<NodeId>,
}

impl Visitor for Named<'_> {
    fn visit_expr(&mut self, expr: &Expr) {
        if let ExprKind::Path(_) = &expr.kind
            && let Some(Resolution::Local(binding)) = self.resolutions.get(expr.id)
        {
            self.locals.insert(binding);
        }
        walk_expr(self, expr);
    }
}

impl Finder<'_> {
    fn named_in(&self, expr: &Expr) -> HashSet<NodeId> {
        let mut named = Named {
            resolutions: self.resolutions,
            locals: HashSet::new(),
        };
        named.visit_expr(expr);
        named.locals
    }

    /// The locals captured by the closures `value` stores: a closure it is,
    /// a `let`-bound closure it names, or either held by the constructor,
    /// literal, or branch that builds it. A closure handed to a call is the
    /// callee's to run and is not stored by this value.
    fn captured_by(&self, value: &Expr) -> HashSet<NodeId> {
        let mut captured = HashSet::new();
        self.stored(value, &mut captured);
        captured
    }

    fn stored(&self, value: &Expr, captured: &mut HashSet<NodeId>) {
        match &value.kind {
            ExprKind::Closure { body, .. } => captured.extend(self.named_in(body)),
            ExprKind::Path(_) => {
                if let Some(Resolution::Local(binding)) = self.resolutions.get(value.id)
                    && let Some(captures) = self.closure_bindings.get(&binding)
                {
                    captured.extend(captures.iter().copied());
                }
            }
            ExprKind::Call { callee, args } if is_constructor(callee) => {
                for arg in args {
                    self.stored(arg, captured);
                }
            }
            ExprKind::Struct { fields, base, .. } => {
                for value in fields.iter().filter_map(|f| f.value.as_ref()) {
                    self.stored(value, captured);
                }
                if let Some(base) = base {
                    self.stored(base, captured);
                }
            }
            ExprKind::Tuple(parts) | ExprKind::MapLiteral(parts) | ExprKind::SetLiteral(parts) => {
                for part in parts {
                    self.stored(part, captured);
                }
            }
            ExprKind::Array(ArrayExpr::List(parts))
            | ExprKind::FixedArray(ArrayExpr::List(parts)) => {
                for part in parts {
                    self.stored(part, captured);
                }
            }
            ExprKind::Block(block) => {
                if let Some(tail) = &block.tail {
                    self.stored(tail, captured);
                }
            }
            ExprKind::If {
                then_branch,
                else_branch,
                ..
            } => {
                self.stored(then_branch, captured);
                if let Some(other) = else_branch {
                    self.stored(other, captured);
                }
            }
            ExprKind::Match { arms, .. } => {
                for arm in arms {
                    self.stored(&arm.body, captured);
                }
            }
            _ => {}
        }
    }

    /// The local a place is rooted at, through fields and indices.
    fn root(&self, place: &Expr) -> Option<NodeId> {
        match &place.kind {
            ExprKind::Path(_) => match self.resolutions.get(place.id) {
                Some(Resolution::Local(binding)) => Some(binding),
                _ => None,
            },
            ExprKind::FieldAccess { receiver, .. } | ExprKind::Index { base: receiver, .. } => {
                self.root(receiver)
            }
            _ => None,
        }
    }

    fn report(&mut self, root: &Expr, binding: NodeId, values: &[&Expr], span: Span) {
        if values
            .iter()
            .any(|value| self.captured_by(value).contains(&binding))
        {
            let name = root_name(root).unwrap_or_default();
            self.out.push(ClosureSelfCycle { span, name });
        }
    }
}

/// Whether `callee` names a variant or struct constructor (`Some`, `Ok`,
/// `Shape::Circle`), which keeps its arguments in the value it builds.
fn is_constructor(callee: &Expr) -> bool {
    let ExprKind::Path(path) = &callee.kind else {
        return false;
    };
    path.segments
        .last()
        .and_then(|segment| segment.name.name.chars().next())
        .is_some_and(char::is_uppercase)
}

/// The name a place's root binding is spelled with.
fn root_name(place: &Expr) -> Option<String> {
    match &place.kind {
        ExprKind::Path(path) => path.segments.last().map(|s| s.name.name.clone()),
        ExprKind::FieldAccess { receiver, .. } | ExprKind::Index { base: receiver, .. } => {
            root_name(receiver)
        }
        _ => None,
    }
}

impl Visitor for Finder<'_> {
    fn visit_stmt(&mut self, stmt: &Stmt) {
        if let StmtKind::Let {
            pattern,
            init: Some(init),
            ..
        } = &stmt.kind
            && let PatternKind::Ident { .. } = &pattern.kind
            && let ExprKind::Closure { body, .. } = &init.kind
        {
            let captured = self.named_in(body);
            self.closure_bindings.insert(pattern.id, captured);
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Assign { place, value, .. } => {
                if let Some(binding) = self.root(place) {
                    self.report(place, binding, &[value], expr.span);
                }
            }
            ExprKind::MethodCall {
                receiver,
                name,
                args,
                ..
            } => {
                if self.table.writes_receiver(receiver.id)
                    && let Some(binding) = self.root(receiver)
                {
                    let kept = method_kept_args(&self.kept_args, &name.name);
                    let values: Vec<&Expr> = args
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| kept.is_none_or(|kept| kept.get(*i) != Some(&false)))
                        .map(|(_, arg)| arg)
                        .collect();
                    self.report(receiver, binding, &values, expr.span);
                }
            }
            _ => {}
        }
        walk_expr(self, expr);
    }
}
