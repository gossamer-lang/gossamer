//! Which functions are pure functions of their arguments.
//!
//! A function is pure when it reads no mutable global state, writes through
//! no reference its caller holds, performs no I/O, starts no goroutine, and
//! calls only pure functions. Mutating its own locals, and its by-value
//! parameters, stays pure: that storage is not observable outside the call.
//!
//! The facts are computed bottom-up over the call graph to a fixed point, and
//! each impure function records the shortest call chain to the first effect
//! found, so a diagnostic can say why rather than only that.
//!
//! A write through a function's own `&mut` parameter, or through `&mut self`,
//! makes the function impure on its own, but whether it reaches the caller's
//! state is decided at each call: passing `&mut` to a local the caller owns
//! keeps the caller pure.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet, VecDeque};

use gossamer_ast::{
    Block, BlockKind, Expr, ExprKind, FnDecl, FnParam, ImplItem, Item, ItemKind, ModBody,
    Mutability, NodeId, Pattern, PatternKind, Receiver, SourceFile, StmtKind, TypeKind, UnaryOp,
    UseTarget,
};
use gossamer_lex::Span;
use gossamer_resolve::{DefId, DefKind, Resolution, Resolutions};

use crate::context::TyCtxt;
use crate::table::TypeTable;
use crate::ty::{Ty, TyKind};

/// What a body may do besides compute.
///
/// A write through a captured container counts as `writes_caller_state`: a
/// closure captures a `Vec`, `Map`, or `Set` by managed reference, so its
/// storage is the caller's.
// Each flag is an independent effect class a body may have any subset of, and
// the diagnostics name them one by one; a packed set would hide those names.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Effects {
    /// Reads or writes the world outside the program: I/O, time, randomness,
    /// the environment, or a mutable global.
    pub world: bool,
    /// Writes storage the caller can observe: through a `&mut` parameter, a
    /// captured binding, or a static.
    pub writes_caller_state: bool,
    /// Starts goroutines or synchronises with them.
    pub concurrency: bool,
    /// Runs foreign or `unsafe` code the compiler cannot see into.
    pub foreign: bool,
    /// Calls a callable whose body is not known at this point.
    pub higher_order: bool,
}

impl Effects {
    /// Whether no effect is present.
    #[must_use]
    pub const fn is_pure(self) -> bool {
        !(self.world
            || self.writes_caller_state
            || self.concurrency
            || self.foreign
            || self.higher_order)
    }

    /// Every effect either side has.
    #[must_use]
    pub const fn join(self, other: Self) -> Self {
        Self {
            world: self.world || other.world,
            writes_caller_state: self.writes_caller_state || other.writes_caller_state,
            concurrency: self.concurrency || other.concurrency,
            foreign: self.foreign || other.foreign,
            higher_order: self.higher_order || other.higher_order,
        }
    }

    fn of(kind: EffectKind) -> Self {
        let mut effects = Self::default();
        match kind {
            EffectKind::World => effects.world = true,
            EffectKind::WritesCallerState => effects.writes_caller_state = true,
            EffectKind::Concurrency => effects.concurrency = true,
            EffectKind::Foreign => effects.foreign = true,
            EffectKind::HigherOrder => effects.higher_order = true,
        }
        effects
    }
}

/// Which kind of effect one finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectKind {
    /// See [`Effects::world`].
    World,
    /// See [`Effects::writes_caller_state`].
    WritesCallerState,
    /// See [`Effects::concurrency`].
    Concurrency,
    /// See [`Effects::foreign`].
    Foreign,
    /// See [`Effects::higher_order`].
    HigherOrder,
}

/// One effect found in a body: what it does, where, and in words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effect {
    /// Its kind.
    pub kind: EffectKind,
    /// What it does, as a clause: "prints (`println`)", "writes `seen`,
    /// which it captured".
    pub description: String,
    /// Where it is written.
    pub span: Span,
}

/// Why a function or callback is not pure: the chain of calls from it to the
/// function whose own body has the effect, and that effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Impurity {
    /// Function names from the first callee to the one with the effect;
    /// empty when the body itself has it.
    pub path: Vec<String>,
    /// The effect at the end of the path.
    pub effect: Effect,
}

impl Impurity {
    /// The reason as a clause: "calls `mid`, which calls `log`, which
    /// prints (`println`)".
    #[must_use]
    pub fn describe(&self) -> String {
        let mut out = String::new();
        for name in &self.path {
            out.push_str(&format!("calls `{name}`, which "));
        }
        out.push_str(&self.effect.description);
        out
    }
}

/// A function the analysis knows the body of.
#[derive(Debug, Clone)]
struct Unit {
    name: String,
    /// Effects its own body has, not counting writes through its own `&mut`
    /// parameters or `&mut self`.
    direct: Option<Effect>,
    /// Whether its body writes through its own `&mut` parameters.
    param_write: Option<Effect>,
    /// Functions its body calls.
    callees: Vec<usize>,
    /// Effects reaching a caller that passes it only local storage.
    external: Effects,
    /// Why `external` is not pure.
    reason: Option<Impurity>,
}

/// Per-function purity, plus the shortest call path to the first effect, so a
/// diagnostic can show why rather than only that.
#[derive(Debug, Clone, Default)]
pub struct PurityFacts {
    units: Vec<Unit>,
    by_def: HashMap<DefId, usize>,
    by_name: HashMap<String, usize>,
    /// Impl methods by `(type name, method)`; one type may have several
    /// blocks declaring the same name through different traits.
    by_method: HashMap<(String, String), Vec<usize>>,
    /// Which methods take `&mut self`, by `(type name, method)`.
    mut_self_methods: HashSet<(String, String)>,
    /// `static mut` items, whose reads see other workers' writes.
    mutable_statics: HashSet<DefId>,
}

impl PurityFacts {
    /// Whether the function `def` is pure.
    #[must_use]
    pub fn is_pure(&self, def: DefId) -> bool {
        self.by_def.get(&def).is_some_and(|&i| self.unit_is_pure(i))
    }

    /// The call chain from `def` to the first effect, beginning with `def`
    /// itself, or `None` when it is pure.
    #[must_use]
    pub fn first_effect_path(&self, def: DefId) -> Option<Vec<String>> {
        self.by_def.get(&def).and_then(|&i| self.unit_path(i))
    }

    /// [`Self::is_pure`] by the function's name, for tests.
    #[must_use]
    pub fn is_pure_named(&self, name: &str) -> bool {
        self.by_name
            .get(name)
            .is_some_and(|&i| self.unit_is_pure(i))
    }

    /// [`Self::first_effect_path`] by the function's name, for tests.
    #[must_use]
    pub fn first_effect_path_named(&self, name: &str) -> Option<Vec<String>> {
        self.by_name.get(name).and_then(|&i| self.unit_path(i))
    }

    /// Why the function `def` is not pure, or `None` when it is.
    #[must_use]
    pub fn impurity(&self, def: DefId) -> Option<Impurity> {
        let &i = self.by_def.get(&def)?;
        self.unit_impurity(i)
    }

    /// Why the method `method` a user impl block declares for `ty` is not
    /// pure, or `None` when every such block's is, or none declares it.
    #[must_use]
    pub fn method_impurity(&self, ty: &str, method: &str) -> Option<Impurity> {
        let units = self.by_method.get(&(ty.to_string(), method.to_string()))?;
        units.iter().find_map(|&i| self.unit_impurity(i))
    }

    /// [`Self::method_impurity`] for a `Type::method` path.
    #[must_use]
    pub fn path_method_impurity(&self, segments: &[String]) -> Option<Impurity> {
        let [.., ty, method] = segments else {
            return None;
        };
        self.method_impurity(ty, method)
    }

    fn unit_is_pure(&self, i: usize) -> bool {
        let unit = &self.units[i];
        unit.external.is_pure() && unit.param_write.is_none()
    }

    fn unit_impurity(&self, i: usize) -> Option<Impurity> {
        let unit = &self.units[i];
        if let Some(reason) = &unit.reason {
            return Some(reason.clone());
        }
        unit.param_write.clone().map(|effect| Impurity {
            path: Vec::new(),
            effect,
        })
    }

    fn unit_path(&self, i: usize) -> Option<Vec<String>> {
        let impurity = self.unit_impurity(i)?;
        let mut path = vec![self.units[i].name.clone()];
        path.extend(impurity.path);
        Some(path)
    }
}

/// Computes purity for every function in `sf`.
#[must_use]
pub fn analyze(
    sf: &SourceFile,
    resolutions: &Resolutions,
    table: &TypeTable,
    tcx: &TyCtxt,
) -> PurityFacts {
    let imports = ImportMap::new(sf);
    let mut facts = PurityFacts::default();
    let mut decls: Vec<(&FnDecl, Option<String>)> = Vec::new();
    collect_units(&sf.items, resolutions, &mut facts, &mut decls);
    let context = Context {
        resolutions,
        table,
        tcx,
        imports: &imports,
    };
    for (index, (decl, owner)) in decls.iter().enumerate() {
        let scan = scan_fn(&context, &facts, decl, owner.as_deref());
        let unit = &mut facts.units[index];
        unit.direct = scan.direct;
        unit.param_write = scan.param_write;
        unit.callees = scan.callees;
    }
    propagate(&mut facts);
    facts
}

/// Why a callback expression is not pure, or `None` when it is. `expr` is
/// a closure literal; everything it names that it does not bind itself is a
/// capture.
#[must_use]
pub fn closure_impurity(
    facts: &PurityFacts,
    closure: &Expr,
    sf: &SourceFile,
    resolutions: &Resolutions,
    table: &TypeTable,
    tcx: &TyCtxt,
) -> Option<Impurity> {
    let ExprKind::Closure { params, body, .. } = &closure.kind else {
        return None;
    };
    let imports = ImportMap::new(sf);
    let context = Context {
        resolutions,
        table,
        tcx,
        imports: &imports,
    };
    let mut locals = HashSet::new();
    for param in params {
        collect_bindings(&param.pattern, &mut locals);
    }
    collect_body_bindings(body, &mut locals);
    let mut scan = Scan::new(&context, facts, locals, HashMap::new(), None);
    scan.captures_are_callers = true;
    scan.expr(body);
    if let Some(effect) = scan.direct {
        return Some(Impurity {
            path: Vec::new(),
            effect,
        });
    }
    scan.callees
        .iter()
        .find_map(|&callee| facts.units[callee].reason.clone().map(|r| (callee, r)))
        .map(|(callee, reason)| {
            let mut path = vec![facts.units[callee].name.clone()];
            path.extend(reason.path);
            Impurity {
                path,
                effect: reason.effect,
            }
        })
}

/// Why the stdlib function a path names is not pure, or `None` when it is,
/// or when the path does not name a stdlib function.
#[must_use]
pub fn path_impurity(
    node: NodeId,
    segments: &[String],
    sf: &SourceFile,
    resolutions: &Resolutions,
    span: Span,
) -> Option<Effect> {
    let imports = ImportMap::new(sf);
    let canonical = imports.canonical(resolutions, node, segments);
    classify_std_path(&canonical, span)
}

struct Context<'a> {
    resolutions: &'a Resolutions,
    table: &'a TypeTable,
    tcx: &'a TyCtxt,
    imports: &'a ImportMap,
}

/// Registers every function with a body as a unit, in a stable order.
fn collect_units<'a>(
    items: &'a [Item],
    resolutions: &Resolutions,
    facts: &mut PurityFacts,
    decls: &mut Vec<(&'a FnDecl, Option<String>)>,
) {
    for item in items {
        match &item.kind {
            ItemKind::Fn(decl) if decl.body.is_some() => {
                let index = push_unit(facts, decls, decl, None);
                if let Some(def) = resolutions.definition_of(item.id) {
                    facts.by_def.insert(def, index);
                }
            }
            ItemKind::Impl(imp) => {
                let Some(owner) = type_name(&imp.self_ty) else {
                    continue;
                };
                for impl_item in &imp.items {
                    let ImplItem::Fn(decl) = impl_item else {
                        continue;
                    };
                    let key = (owner.clone(), decl.name.name.clone());
                    if decl
                        .params
                        .first()
                        .is_some_and(|param| matches!(param, FnParam::Receiver(Receiver::RefMut)))
                    {
                        facts.mut_self_methods.insert(key.clone());
                    }
                    if decl.body.is_none() {
                        continue;
                    }
                    let index = push_unit(facts, decls, decl, Some(owner.clone()));
                    facts.by_method.entry(key).or_default().push(index);
                }
            }
            ItemKind::Mod(module) => {
                if let ModBody::Inline(inner) = &module.body {
                    collect_units(inner, resolutions, facts, decls);
                }
            }
            ItemKind::Static(decl) if decl.mutability == Mutability::Mutable => {
                if let Some(def) = resolutions.definition_of(item.id) {
                    facts.mutable_statics.insert(def);
                }
            }
            _ => {}
        }
    }
}

fn push_unit<'a>(
    facts: &mut PurityFacts,
    decls: &mut Vec<(&'a FnDecl, Option<String>)>,
    decl: &'a FnDecl,
    owner: Option<String>,
) -> usize {
    let index = facts.units.len();
    let name = match &owner {
        Some(owner) => format!("{owner}::{}", decl.name.name),
        None => decl.name.name.clone(),
    };
    facts.by_name.entry(name.clone()).or_insert(index);
    facts.units.push(Unit {
        name,
        direct: None,
        param_write: None,
        callees: Vec::new(),
        external: Effects::default(),
        reason: None,
    });
    decls.push((decl, owner));
    index
}

/// The last segment of a type's path, which names its impl blocks.
fn type_name(ty: &gossamer_ast::Type) -> Option<String> {
    match &ty.kind {
        TypeKind::Path(path) => path.segments.last().map(|seg| seg.name.name.clone()),
        TypeKind::Ref { inner, .. } => type_name(inner),
        _ => None,
    }
}

/// Spreads each function's effects to its callers, recording the shortest
/// chain to an effect: a breadth-first walk backwards from the functions
/// whose own bodies have one.
fn propagate(facts: &mut PurityFacts) {
    let count = facts.units.len();
    let mut callers: Vec<Vec<usize>> = vec![Vec::new(); count];
    for (caller, unit) in facts.units.iter().enumerate() {
        for &callee in &unit.callees {
            callers[callee].push(caller);
        }
    }
    let mut queue = VecDeque::new();
    for (index, unit) in facts.units.iter_mut().enumerate() {
        if let Some(effect) = &unit.direct {
            unit.external = Effects::of(effect.kind);
            unit.reason = Some(Impurity {
                path: Vec::new(),
                effect: effect.clone(),
            });
            queue.push_back(index);
        }
    }
    while let Some(callee) = queue.pop_front() {
        let Some(reason) = facts.units[callee].reason.clone() else {
            continue;
        };
        let callee_name = facts.units[callee].name.clone();
        let effects = facts.units[callee].external;
        for &caller in &callers[callee] {
            let unit = &mut facts.units[caller];
            let joined = unit.external.join(effects);
            if unit.reason.is_none() {
                let mut path = vec![callee_name.clone()];
                path.extend(reason.path.clone());
                unit.reason = Some(Impurity {
                    path,
                    effect: reason.effect.clone(),
                });
                unit.external = joined;
                queue.push_back(caller);
            } else if joined != unit.external {
                unit.external = joined;
                queue.push_back(caller);
            }
        }
    }
}

/// What a function body does, as [`scan_fn`] finds it.
struct FnScan {
    direct: Option<Effect>,
    param_write: Option<Effect>,
    callees: Vec<usize>,
}

fn scan_fn(
    context: &Context<'_>,
    facts: &PurityFacts,
    decl: &FnDecl,
    owner: Option<&str>,
) -> FnScan {
    let mut locals = HashSet::new();
    let mut params = HashMap::new();
    let mut receiver = None;
    for param in &decl.params {
        match param {
            FnParam::Receiver(kind) => receiver = Some((*kind, owner.map(str::to_string))),
            FnParam::Typed { pattern, ty, .. } => {
                let mut bound = HashSet::new();
                collect_bindings(pattern, &mut bound);
                let kind = match &ty.kind {
                    TypeKind::Ref {
                        mutability: Mutability::Mutable,
                        ..
                    } => ParamKind::MutRef,
                    _ => ParamKind::Value,
                };
                for id in bound {
                    locals.insert(id);
                    params.insert(id, kind);
                }
            }
        }
    }
    let Some(body) = &decl.body else {
        return FnScan {
            direct: None,
            param_write: None,
            callees: Vec::new(),
        };
    };
    collect_body_bindings(body, &mut locals);
    let mut scan = Scan::new(context, facts, locals, params, receiver);
    scan.expr(body);
    FnScan {
        direct: scan.direct,
        param_write: scan.param_write,
        callees: scan.callees,
    }
}

/// How a parameter binding may be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParamKind {
    /// The callee's own value.
    Value,
    /// The caller's storage, reached through `&mut`.
    MutRef,
}

/// Where a written place's storage lives.
enum Root {
    /// Storage this body owns.
    Local,
    /// Storage the caller handed over through `&mut`, or `&mut self`.
    CallerParam(String),
    /// A binding the body did not declare: a capture.
    Captured(String),
    /// A static.
    Static(String),
    /// Something the analysis cannot place.
    Unknown,
}

/// Walks one body, recording the first effect and every known callee.
struct Scan<'a> {
    context: &'a Context<'a>,
    facts: &'a PurityFacts,
    locals: HashSet<NodeId>,
    params: HashMap<NodeId, ParamKind>,
    receiver: Option<(Receiver, Option<String>)>,
    /// Locals bound to a closure literal or a named function, which the
    /// body may call without leaving what the analysis can see.
    known_callables: HashSet<NodeId>,
    /// When scanning a callback, a capture is the caller's storage.
    captures_are_callers: bool,
    direct: Option<Effect>,
    param_write: Option<Effect>,
    callees: Vec<usize>,
}

impl<'a> Scan<'a> {
    fn new(
        context: &'a Context<'a>,
        facts: &'a PurityFacts,
        locals: HashSet<NodeId>,
        params: HashMap<NodeId, ParamKind>,
        receiver: Option<(Receiver, Option<String>)>,
    ) -> Self {
        Self {
            context,
            facts,
            locals,
            params,
            receiver,
            known_callables: HashSet::new(),
            captures_are_callers: false,
            direct: None,
            param_write: None,
            callees: Vec::new(),
        }
    }

    fn effect(&mut self, kind: EffectKind, description: String, span: Span) {
        if self.direct.is_none() {
            self.direct = Some(Effect {
                kind,
                description,
                span,
            });
        }
    }

    fn call_unit(&mut self, index: usize) {
        if !self.callees.contains(&index) {
            self.callees.push(index);
        }
    }

    /// Records a write to the place rooted at `place`.
    fn write(&mut self, place: &Expr, span: Span) {
        match self.root(place) {
            Root::Local => {}
            Root::CallerParam(name) => {
                if self.param_write.is_none() {
                    self.param_write = Some(Effect {
                        kind: EffectKind::WritesCallerState,
                        description: format!("writes through the `&mut` parameter `{name}`"),
                        span,
                    });
                }
            }
            Root::Captured(name) => {
                if self.captures_are_callers {
                    self.effect(
                        EffectKind::WritesCallerState,
                        format!("writes `{name}`, which it captured"),
                        span,
                    );
                }
            }
            Root::Static(name) => self.effect(
                EffectKind::WritesCallerState,
                format!("writes the static `{name}`"),
                span,
            ),
            Root::Unknown => self.effect(
                EffectKind::WritesCallerState,
                "writes storage it does not own".to_string(),
                span,
            ),
        }
    }

    fn root(&self, place: &Expr) -> Root {
        match &place.kind {
            ExprKind::Path(path) if path.segments.len() == 1 => {
                let name = path.segments[0].name.name.clone();
                if name == "self" {
                    return match &self.receiver {
                        Some((Receiver::RefMut, _)) => Root::CallerParam(name),
                        Some(_) => Root::Local,
                        None => Root::Captured(name),
                    };
                }
                match self.context.resolutions.get(place.id) {
                    Some(Resolution::Local(binding)) => {
                        if self.params.get(&binding) == Some(&ParamKind::MutRef) {
                            Root::CallerParam(name)
                        } else if self.locals.contains(&binding) {
                            Root::Local
                        } else {
                            Root::Captured(name)
                        }
                    }
                    Some(Resolution::Def {
                        kind: DefKind::Static,
                        ..
                    }) => Root::Static(name),
                    _ => Root::Unknown,
                }
            }
            ExprKind::FieldAccess { receiver, .. } => self.root(receiver),
            ExprKind::Index { base, .. } => self.root(base),
            ExprKind::Unary {
                op: UnaryOp::Deref | UnaryOp::RefMut | UnaryOp::RefShared,
                operand,
            } => self.root(operand),
            // A method answering a view (`xs.get_mut(0)`) or a call answering
            // a fresh value: the value is this body's own.
            ExprKind::MethodCall { receiver, .. } => self.root(receiver),
            ExprKind::Call { .. } | ExprKind::Literal(_) => Root::Local,
            _ => Root::Unknown,
        }
    }

    fn ty_of(&self, node: NodeId) -> Option<Ty> {
        self.context.table.get(node)
    }

    fn expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Path(path) => self.read_path(expr, path),
            ExprKind::Call { callee, args } => {
                self.call(expr, callee);
                for arg in args {
                    self.expr(arg);
                }
            }
            ExprKind::MethodCall {
                receiver,
                name,
                args,
                ..
            } => {
                self.method_call(expr, receiver, &name.name);
                self.expr(receiver);
                for arg in args {
                    self.expr(arg);
                }
            }
            ExprKind::Assign { place, value, .. } => {
                self.write(place, expr.span);
                self.expr_place_reads(place);
                self.expr(value);
            }
            ExprKind::Unary {
                op: UnaryOp::RefMut,
                operand,
            } => {
                // Handing out `&mut` lets whatever receives it write.
                self.write(operand, expr.span);
                self.expr(operand);
            }
            ExprKind::Block(block) => self.block(block, expr.span),
            ExprKind::Unsafe(block) => {
                self.effect(
                    EffectKind::Foreign,
                    "runs an `unsafe` block".to_string(),
                    expr.span,
                );
                self.block(block, expr.span);
            }
            ExprKind::Select(_) => {
                self.effect(
                    EffectKind::Concurrency,
                    "waits in a `select`".to_string(),
                    expr.span,
                );
                gossamer_ast::visitor::walk_expr(&mut SubScan(self), expr);
            }
            ExprKind::For { iter, body, .. } => {
                // Walking a captured iterator consumes it.
                if let Some(ty) = self.ty_of(iter.id)
                    && matches!(self.context.tcx.kind_of(ty), TyKind::Iterator(_))
                {
                    self.write(iter, expr.span);
                }
                self.expr(iter);
                self.expr(body);
            }
            ExprKind::Closure { body, .. } => self.expr(body),
            _ => gossamer_ast::visitor::walk_expr(&mut SubScan(self), expr),
        }
    }

    /// Reads inside a written place (an index expression, say) still count.
    fn expr_place_reads(&mut self, place: &Expr) {
        match &place.kind {
            ExprKind::Index { base, index } => {
                self.expr_place_reads(base);
                self.expr(index);
            }
            ExprKind::FieldAccess { receiver, .. } => self.expr_place_reads(receiver),
            ExprKind::Unary { operand, .. } => self.expr_place_reads(operand),
            _ => {}
        }
    }

    fn block(&mut self, block: &Block, span: Span) {
        if block.kind == BlockKind::Cohort {
            self.effect(
                EffectKind::Concurrency,
                "opens a `cohort`".to_string(),
                span,
            );
        }
        for stmt in &block.stmts {
            match &stmt.kind {
                StmtKind::Let { pattern, init, .. } => {
                    if let Some(init) = init {
                        if self.is_known_callable(init)
                            && let PatternKind::Ident { .. } = &pattern.kind
                        {
                            self.known_callables.insert(pattern.id);
                        }
                        self.expr(init);
                    }
                }
                StmtKind::Expr { expr, .. } | StmtKind::Defer(expr) => self.expr(expr),
                StmtKind::Item(_) => {}
            }
        }
        if let Some(tail) = &block.tail {
            self.expr(tail);
        }
    }

    fn is_known_callable(&self, init: &Expr) -> bool {
        match &init.kind {
            ExprKind::Closure { .. } => true,
            ExprKind::Path(_) => matches!(
                self.context.resolutions.get(init.id),
                Some(Resolution::Def {
                    kind: DefKind::Fn,
                    ..
                })
            ),
            _ => false,
        }
    }

    fn read_path(&mut self, expr: &Expr, path: &gossamer_ast::PathExpr) {
        if let Some(Resolution::Def {
            kind: DefKind::Static,
            def,
        }) = self.context.resolutions.get(expr.id)
            && self.facts.mutable_statics.contains(&def)
        {
            let name = path
                .segments
                .last()
                .map_or_else(String::new, |seg| seg.name.name.clone());
            self.effect(
                EffectKind::World,
                format!("reads the mutable static `{name}`"),
                expr.span,
            );
        }
    }

    fn call(&mut self, expr: &Expr, callee: &Expr) {
        let ExprKind::Path(path) = &callee.kind else {
            self.expr(callee);
            if !matches!(callee.kind, ExprKind::Closure { .. }) {
                self.effect(
                    EffectKind::HigherOrder,
                    "calls a value whose body is not known here".to_string(),
                    expr.span,
                );
            }
            return;
        };
        let segments: Vec<String> = path
            .segments
            .iter()
            .map(|seg| seg.name.name.clone())
            .collect();
        let written = segments.join("::");
        match self.context.resolutions.get(callee.id) {
            Some(Resolution::Local(binding)) => {
                if self.params.contains_key(&binding) {
                    self.effect(
                        EffectKind::HigherOrder,
                        format!(
                            "calls through the parameter `{written}`, whose body is not known here"
                        ),
                        expr.span,
                    );
                } else if !self.locals.contains(&binding)
                    || !self.known_callables.contains(&binding)
                {
                    self.effect(
                        EffectKind::HigherOrder,
                        format!("calls `{written}`, whose body is not known here"),
                        expr.span,
                    );
                }
            }
            Some(Resolution::Def { def, kind }) => match kind {
                DefKind::Fn => {
                    if let Some(&index) = self.facts.by_def.get(&def) {
                        self.call_unit(index);
                    } else if !self.call_method_path(&segments) {
                        self.effect(
                            EffectKind::World,
                            format!("calls `{written}`, whose body is not known here"),
                            expr.span,
                        );
                    }
                }
                DefKind::Struct | DefKind::Enum | DefKind::Variant => {}
                _ => {
                    if !self.call_method_path(&segments) {
                        self.effect(
                            EffectKind::HigherOrder,
                            format!("calls `{written}`, whose body is not known here"),
                            expr.span,
                        );
                    }
                }
            },
            _ => {
                if self.call_method_path(&segments) {
                    return;
                }
                let canonical =
                    self.context
                        .imports
                        .canonical(self.context.resolutions, callee.id, &segments);
                if let Some(effect) = classify_std_path(&canonical, expr.span) {
                    self.effect(effect.kind, effect.description, effect.span);
                }
            }
        }
    }

    /// A `Type::method(..)` call into a user impl block; answers whether the
    /// path named one.
    fn call_method_path(&mut self, segments: &[String]) -> bool {
        let [.., owner, method] = segments else {
            return false;
        };
        let Some(units) = self
            .facts
            .by_method
            .get(&(owner.clone(), method.clone()))
            .cloned()
        else {
            return false;
        };
        for index in units {
            self.call_unit(index);
        }
        true
    }

    fn method_call(&mut self, expr: &Expr, receiver: &Expr, method: &str) {
        let receiver_ty = self.ty_of(receiver.id);
        let mut peeled = receiver_ty;
        while let Some(ty) = peeled {
            match self.context.tcx.kind_of(ty) {
                TyKind::Ref { inner, .. } => peeled = Some(*inner),
                _ => break,
            }
        }
        let Some(ty) = peeled else {
            self.effect(
                EffectKind::World,
                format!("calls `{method}` on a value whose type is not known here"),
                expr.span,
            );
            return;
        };
        match self.context.tcx.kind_of(ty) {
            TyKind::Sender(_) | TyKind::Receiver(_) | TyKind::JoinHandle(_) => {
                self.effect(
                    EffectKind::Concurrency,
                    format!("calls `{method}` on a channel or goroutine handle"),
                    expr.span,
                );
            }
            TyKind::Iterator(_) => {
                // An iterator is a cursor: every walk of it advances it.
                self.write(receiver, expr.span);
            }
            TyKind::Adt { def, .. } | TyKind::Nominal { def, .. } => {
                let name = self
                    .context
                    .tcx
                    .def_name(*def)
                    .map(|n| n.rsplit("::").next().unwrap_or(n).to_string())
                    .unwrap_or_default();
                let key = (name.clone(), method.to_string());
                if let Some(units) = self.facts.by_method.get(&key).cloned() {
                    for index in units {
                        self.call_unit(index);
                    }
                    if self.facts.mut_self_methods.contains(&key) {
                        self.write(receiver, expr.span);
                    }
                } else if is_value_adt(&name) || is_universal_method(method) {
                    if MUTATOR_METHODS.contains(&method) {
                        self.write(receiver, expr.span);
                    }
                } else if def.local >= u32::MAX - crate::checker::HANDLE_SENTINEL_SPAN {
                    let kind = if is_sync_handle(&name) {
                        EffectKind::Concurrency
                    } else {
                        EffectKind::World
                    };
                    self.effect(kind, format!("calls `{name}::{method}`"), expr.span);
                } else {
                    self.effect(
                        EffectKind::World,
                        format!("calls `{name}::{method}`, whose body is not known here"),
                        expr.span,
                    );
                }
            }
            TyKind::Instant => {
                self.effect(
                    EffectKind::World,
                    format!("reads the clock (`Instant::{method}`)"),
                    expr.span,
                );
            }
            TyKind::Var(_) | TyKind::Error | TyKind::Param { .. } => {
                if !is_universal_method(method) {
                    self.effect(
                        EffectKind::World,
                        format!("calls `{method}` on a value whose type is not known here"),
                        expr.span,
                    );
                }
            }
            _ => {
                if MUTATOR_METHODS.contains(&method) {
                    self.write(receiver, expr.span);
                }
            }
        }
    }
}

/// Recurses through [`gossamer_ast::visitor`] back into a [`Scan`].
struct SubScan<'s, 'a>(&'s mut Scan<'a>);

impl gossamer_ast::visitor::Visitor for SubScan<'_, '_> {
    fn visit_expr(&mut self, expr: &Expr) {
        self.0.expr(expr);
    }

    fn visit_block(&mut self, block: &Block) {
        self.0.block(block, Span::default());
    }
}

/// Methods that write their receiver in place.
const MUTATOR_METHODS: &[&str] = &[
    "append",
    "clear",
    "dedup_in_place",
    "drain",
    "extend",
    "fill",
    "inc",
    "insert",
    "or_insert",
    "pop",
    "pop_back",
    "pop_front",
    "push",
    "push_back",
    "push_front",
    "push_str",
    "remove",
    "reserve",
    "resize",
    "retain",
    "reverse",
    "set",
    "shrink_to_fit",
    "sort",
    "sort_by",
    "sort_by_key",
    "swap",
    "truncate",
];

/// Methods every value answers without touching the world.
fn is_universal_method(method: &str) -> bool {
    matches!(
        method,
        "clone"
            | "to_string"
            | "eq"
            | "ne"
            | "cmp"
            | "lt"
            | "le"
            | "gt"
            | "ge"
            | "into"
            | "try_into"
    )
}

/// Stdlib value types whose methods compute on the value alone.
fn is_value_adt(name: &str) -> bool {
    matches!(
        name,
        "Option" | "Result" | "DynValue" | "Duration" | "Error" | "Ordering" | "Reverse"
    )
}

/// Stdlib handles whose methods synchronise with other goroutines.
fn is_sync_handle(name: &str) -> bool {
    matches!(
        name,
        "Mutex"
            | "RwLock"
            | "Shared"
            | "WaitGroup"
            | "Barrier"
            | "Once"
            | "AtomicI64"
            | "AtomicI32"
            | "AtomicU64"
            | "AtomicU32"
            | "AtomicBool"
            | "Map"
    )
}

/// Stdlib modules every function of which is a pure function of its
/// arguments, as `std::`-relative module paths.
pub const PURE_STDLIB_MODULES: &[&str] = &[
    "errors",
    "iter",
    "math",
    "math::big",
    "math::bits",
    "option",
    "result",
    "sort",
    "strconv",
    "strings",
    "unicode",
    "utf8",
];

/// Submodules of a pure module that reach outside the program.
pub const IMPURE_STDLIB_SUBMODULES: &[&str] = &["math::rand"];

/// Prelude names that are pure: formatting, the assertion family, the
/// variant constructors, the scalar orderings, and the helpers the parser
/// writes `format` strings and struct updates as.
const PURE_PRELUDE: &[&str] = &[
    "__concat",
    "__debug",
    "__fmt_prec",
    "__struct",
    "__update",
    "Err",
    "None",
    "Ok",
    "Some",
    "assert",
    "assert_eq",
    "clamp",
    "format",
    "matches",
    "max",
    "min",
    "panic",
    "todo",
    "unimplemented",
    "unreachable",
];

/// Built-in type names whose associated functions build or convert a value.
const VALUE_TYPE_NAMES: &[&str] = &[
    "BTreeMap", "BTreeSet", "Deque", "DynValue", "Map", "MaxHeap", "MinHeap", "Queue", "Set",
    "Simd", "Stack", "String", "Vec", "bool", "char", "f32", "f64", "i16", "i32", "i64", "i8",
    "isize", "u16", "u32", "u64", "u8", "usize",
];

/// Why the stdlib function at `canonical` (a `std::`-relative path) is not
/// pure, or `None` when it is.
fn classify_std_path(canonical: &[String], span: Span) -> Option<Effect> {
    let written = canonical.join("::");
    match canonical {
        [name] => {
            if PURE_PRELUDE.contains(&name.as_str()) {
                return None;
            }
            let (kind, description) = match name.as_str() {
                "println" | "print" | "eprintln" | "eprint" | "dbg" | "__dbg" => {
                    (EffectKind::World, format!("prints (`{name}`)"))
                }
                "spawn" => (
                    EffectKind::Concurrency,
                    "starts a goroutine (`spawn`)".to_string(),
                ),
                _ => (
                    EffectKind::World,
                    format!("calls `{name}`, whose body is not known here"),
                ),
            };
            Some(Effect {
                kind,
                description,
                span,
            })
        }
        [owner, ..] if VALUE_TYPE_NAMES.contains(&owner.as_str()) => None,
        [.., _item] => {
            let module = &canonical[..canonical.len() - 1];
            let module_path = module.join("::");
            let impure_sub = IMPURE_STDLIB_SUBMODULES
                .iter()
                .any(|sub| module_path == *sub || module_path.starts_with(&format!("{sub}::")));
            if !impure_sub && PURE_STDLIB_MODULES.contains(&module_path.as_str()) {
                return None;
            }
            let kind = match module.first().map(String::as_str) {
                Some("sync" | "runtime" | "thread" | "context" | "channel") => {
                    EffectKind::Concurrency
                }
                _ => EffectKind::World,
            };
            Some(Effect {
                kind,
                description: format!("calls `{written}`, which reaches outside the program"),
                span,
            })
        }
        [] => None,
    }
}

/// `use` declarations, so a written path reads as the stdlib path it names.
struct ImportMap {
    targets: HashMap<(NodeId, String), Vec<String>>,
}

impl ImportMap {
    fn new(sf: &SourceFile) -> Self {
        let mut targets = HashMap::new();
        for use_decl in &sf.uses {
            let UseTarget::Module(path) = &use_decl.target else {
                continue;
            };
            let base: Vec<String> = path.segments.iter().map(|seg| seg.name.clone()).collect();
            if let Some(list) = &use_decl.list {
                for entry in list {
                    let bound = entry.alias.as_ref().unwrap_or(&entry.name).name.clone();
                    let mut full = base.clone();
                    full.extend(entry.prefix.iter().map(|ident| ident.name.clone()));
                    full.push(entry.name.name.clone());
                    targets.insert((use_decl.id, bound), full);
                }
            } else if let Some(bound) = use_decl
                .alias
                .as_ref()
                .map_or_else(|| base.last().cloned(), |alias| Some(alias.name.clone()))
            {
                targets.insert((use_decl.id, bound), base);
            }
        }
        Self { targets }
    }

    /// The `std::`-relative path `segments` names, expanding an imported
    /// first segment.
    fn canonical(
        &self,
        resolutions: &Resolutions,
        node: NodeId,
        segments: &[String],
    ) -> Vec<String> {
        let mut out: Vec<String> = segments.to_vec();
        if let (Some(Resolution::Import { use_id }), Some(first)) =
            (resolutions.get(node), segments.first())
            && let Some(target) = self.targets.get(&(use_id, first.clone()))
        {
            out.clone_from(target);
            out.extend(segments.iter().skip(1).cloned());
        }
        if out.first().is_some_and(|seg| seg == "std") {
            out.remove(0);
        }
        out
    }
}

/// Collects the binding occurrences a pattern introduces.
pub(crate) fn collect_bindings(pattern: &Pattern, out: &mut HashSet<NodeId>) {
    struct Bindings<'o>(&'o mut HashSet<NodeId>);
    impl gossamer_ast::visitor::Visitor for Bindings<'_> {
        fn visit_pattern(&mut self, pattern: &Pattern) {
            if matches!(pattern.kind, PatternKind::Ident { .. }) {
                self.0.insert(pattern.id);
            }
            gossamer_ast::visitor::walk_pattern(self, pattern);
        }
    }
    gossamer_ast::visitor::Visitor::visit_pattern(&mut Bindings(out), pattern);
}

/// Collects every binding a body declares anywhere inside it, closures
/// included: all of it is storage the body owns.
fn collect_body_bindings(body: &Expr, out: &mut HashSet<NodeId>) {
    struct Bindings<'o>(&'o mut HashSet<NodeId>);
    impl gossamer_ast::visitor::Visitor for Bindings<'_> {
        fn visit_pattern(&mut self, pattern: &Pattern) {
            if matches!(pattern.kind, PatternKind::Ident { .. }) {
                self.0.insert(pattern.id);
            }
            gossamer_ast::visitor::walk_pattern(self, pattern);
        }
    }
    gossamer_ast::visitor::Visitor::visit_expr(&mut Bindings(out), body);
}
