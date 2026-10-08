//! Writes a spawned closure makes to the bindings it captures, and callables
//! handed to another goroutine.
//!
//! A closure names the bindings it captures, so its writes reach them. A
//! spawned closure is the exception: it runs on another goroutine, so it
//! takes a snapshot of each capture at the spawn, and a write to one changes
//! only the goroutine's snapshot. Such a write is refused where it is
//! written. A synchronisation handle (a `Mutex`, a channel end, a
//! `sync::Shared`, an atomic) is shared by every snapshot of it, so calling
//! it is not a write to a snapshot.
//!
//! A callable that reaches another goroutine any other way - `spawn(f)`, a
//! spawned closure calling `f`, or a function that spawns its callable
//! parameter - carries the bindings it captured rather than snapshots of
//! them. It is accepted only where every binding it captures, through any
//! closure it captures in turn, is one nothing writes, so the goroutine and
//! the code around it cannot reach each other's state. A callable whose
//! captures cannot be seen from here is refused.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap, HashSet};

use gossamer_ast::visitor::{Visitor, walk_expr, walk_item, walk_pattern, walk_stmt};
use gossamer_ast::{
    Expr, ExprKind, FieldSelector, FnDecl, FnParam, ImplItem, Item, ItemKind, NodeId, Pattern,
    PatternKind, SourceFile, Stmt, StmtKind, TypeKind, UnaryOp,
};
use gossamer_lex::Span;
use gossamer_resolve::{DefId, Resolution, Resolutions};

use crate::context::TyCtxt;
use crate::table::TypeTable;
use crate::ty::{Ty, TyKind};

/// A write a spawned closure makes to its snapshot of a captured binding, or
/// a callable handed to another goroutine that would share state with the
/// code around it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureWriteDiagnostic {
    /// What was refused.
    pub kind: CaptureWriteKind,
    /// The write, or the callable crossing to the goroutine.
    pub span: Span,
}

/// The refusals [`check_capture_writes`] makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureWriteKind {
    /// A spawned closure writes its snapshot of `name`.
    SnapshotWrite {
        /// The captured binding.
        name: String,
    },
    /// A callable run on another goroutine captures `shared`, which the code
    /// around it writes or which the callable writes itself.
    SharedCapture {
        /// The callable, quoted as written, or `this closure`.
        callable: String,
        /// The binding both goroutines would reach.
        shared: String,
    },
    /// A callable run on another goroutine whose captures are not visible
    /// here: a call's result, a field, a reassigned binding, a closure's
    /// parameter.
    OpaqueCallable {
        /// The callable, quoted as written.
        callable: String,
    },
}

impl CaptureWriteDiagnostic {
    /// The code a write to a spawned closure's snapshot carries.
    pub const CODE: &'static str = "GT0114";
    /// The code a callable sharing state with another goroutine carries.
    pub const SHARED_CODE: &'static str = "GT0118";

    /// Renders this refusal as a structured diagnostic.
    #[must_use]
    pub fn to_diagnostic(&self) -> gossamer_diagnostics::Diagnostic {
        use gossamer_diagnostics::{Code, Diagnostic, Location};
        let location = Location::new(self.span.file, self.span);
        match &self.kind {
            CaptureWriteKind::SnapshotWrite { name } => Diagnostic::error(
                Code(Self::CODE),
                format!("this write changes the goroutine's own snapshot of `{name}`"),
            )
            .with_primary(
                location,
                format!("a spawned closure takes a snapshot of `{name}` at the spawn"),
            )
            .with_help(
                "return the value and read it with `join()`, send it on a channel, or share it through a `sync::Shared`",
            ),
            CaptureWriteKind::SharedCapture { callable, shared } => Diagnostic::error(
                Code(Self::SHARED_CODE),
                format!(
                    "{callable} runs on another goroutine but shares `{shared}` with the code around it"
                ),
            )
            .with_primary(
                location,
                format!("`{shared}` is written, and {callable} captures it rather than a snapshot"),
            )
            .with_help(
                "spawn a closure written at the `spawn`, which snapshots what it captures, and answer values through `join()`, a channel, or a `sync::Shared`",
            ),
            CaptureWriteKind::OpaqueCallable { callable } => Diagnostic::error(
                Code(Self::SHARED_CODE),
                format!(
                    "{callable} runs on another goroutine, and what it captures is not known here"
                ),
            )
            .with_primary(
                location,
                "a goroutine may run only a callable whose captures nothing else writes",
            )
            .with_help(
                "spawn a named function, or a closure written at the `spawn` or bound by `let` before it",
            ),
        }
    }
}

/// Checks every spawned closure in `sf` for writes to the bindings it
/// captures, and every callable handed to another goroutine for state it
/// would share with the code around it.
#[must_use]
pub fn check_capture_writes(
    sf: &SourceFile,
    resolutions: &Resolutions,
    table: &TypeTable,
    tcx: &TyCtxt,
) -> Vec<CaptureWriteDiagnostic> {
    let mut facts = Facts {
        resolutions,
        table,
        tcx,
        written: HashSet::new(),
        names: HashMap::new(),
        closure_bound: HashMap::new(),
        captures: HashMap::new(),
        open: Vec::new(),
        params: HashMap::new(),
    };
    facts.visit_source_file(sf);
    // A function that hands a callable parameter to a goroutine passes the
    // requirement to its callers, which may pass their own parameters on in
    // turn; each pass can only add parameters, so this settles.
    let mut transferred: HashSet<(FnKey, usize)> = HashSet::new();
    loop {
        let mut walker = Walker {
            facts: &facts,
            transferred: &transferred,
            frames: Vec::new(),
            spawned: HashSet::new(),
            reached: HashSet::new(),
            out: BTreeMap::new(),
        };
        walker.visit_source_file(sf);
        if walker.reached.is_subset(&transferred) {
            return walker.out.into_values().collect();
        }
        transferred.extend(walker.reached);
    }
}

/// A function a call reaches.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum FnKey {
    Free(DefId),
    /// A method, by its `impl` block's type name and its own name.
    Method(String, String),
}

/// One binding a closure captures, at one place it is used.
#[derive(Debug, Clone)]
struct Capture {
    binding: NodeId,
    use_id: NodeId,
    name: String,
}

/// A closure being walked: the bindings it declares and the locals it names.
struct OpenClosure {
    id: NodeId,
    declared: HashSet<NodeId>,
    named: Vec<Capture>,
}

/// What the whole file says about bindings and closures, read once.
struct Facts<'a> {
    resolutions: &'a Resolutions,
    table: &'a TypeTable,
    tcx: &'a TyCtxt,
    /// Bindings something writes: reassigns, lends `&mut`, or changes in
    /// place through a method that writes its receiver.
    written: HashSet<NodeId>,
    names: HashMap<NodeId, String>,
    /// Bindings `let`-bound to a closure written in place, by closure id.
    closure_bound: HashMap<NodeId, NodeId>,
    /// The bindings each closure captures, by closure id.
    captures: HashMap<NodeId, Vec<Capture>>,
    open: Vec<OpenClosure>,
    /// Function parameters, by binding: the function and the parameter's
    /// position among the arguments a call passes.
    params: HashMap<NodeId, (FnKey, usize)>,
}

impl Visitor for Facts<'_> {
    fn visit_item(&mut self, item: &Item) {
        if !gossamer_ast::cfg::item_is_active(&item.attrs) {
            return;
        }
        match &item.kind {
            ItemKind::Fn(decl) => {
                if let Some(def) = self.resolutions.definition_of(item.id) {
                    self.note_params(&FnKey::Free(def), decl);
                }
            }
            ItemKind::Impl(imp) => {
                if let TypeKind::Path(path) = &imp.self_ty.kind
                    && let Some(owner) = path.segments.last()
                {
                    for entry in &imp.items {
                        if let ImplItem::Fn(decl) = entry {
                            let key =
                                FnKey::Method(owner.name.name.clone(), decl.name.name.clone());
                            self.note_params(&key, decl);
                        }
                    }
                }
            }
            _ => {}
        }
        walk_item(self, item);
    }

    fn visit_stmt(&mut self, stmt: &Stmt) {
        if let StmtKind::Let {
            pattern,
            init: Some(init),
            ..
        } = &stmt.kind
            && matches!(
                pattern.kind,
                PatternKind::Ident {
                    subpattern: None,
                    ..
                }
            )
            && matches!(init.kind, ExprKind::Closure { .. })
        {
            self.closure_bound.insert(pattern.id, init.id);
        }
        walk_stmt(self, stmt);
    }

    fn visit_pattern(&mut self, pattern: &Pattern) {
        if let PatternKind::Ident { name, .. } = &pattern.kind {
            self.names.insert(pattern.id, name.name.clone());
        }
        for closure in &mut self.open {
            closure.declared.insert(pattern.id);
        }
        walk_pattern(self, pattern);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Closure { .. } => {
                self.open.push(OpenClosure {
                    id: expr.id,
                    declared: HashSet::new(),
                    named: Vec::new(),
                });
                walk_expr(self, expr);
                if let Some(closure) = self.open.pop() {
                    let mut seen = HashSet::new();
                    let captures = closure
                        .named
                        .into_iter()
                        .filter(|c| !closure.declared.contains(&c.binding))
                        .filter(|c| seen.insert(c.binding))
                        .collect();
                    self.captures.insert(closure.id, captures);
                }
                return;
            }
            ExprKind::Path(path) if path.segments.len() == 1 => {
                if let Some(Resolution::Local(binding)) = self.resolutions.get(expr.id) {
                    let capture = Capture {
                        binding,
                        use_id: expr.id,
                        name: path.segments[0].name.name.clone(),
                    };
                    for closure in &mut self.open {
                        closure.named.push(capture.clone());
                    }
                }
            }
            ExprKind::Assign { place, .. } => self.note_write(place, false),
            ExprKind::Unary {
                op: UnaryOp::RefMut,
                operand,
            } => self.note_write(operand, true),
            ExprKind::MethodCall { receiver, .. } if self.table.writes_receiver(receiver.id) => {
                self.note_write(receiver, true);
            }
            ExprKind::Call { args, .. } => {
                if let Some(receiver) = args.first()
                    && self.table.writes_receiver(receiver.id)
                {
                    self.note_write(receiver, true);
                }
            }
            _ => {}
        }
        walk_expr(self, expr);
    }
}

impl Facts<'_> {
    fn note_params(&mut self, key: &FnKey, decl: &FnDecl) {
        let typed = decl.params.iter().filter_map(|param| match param {
            FnParam::Typed { pattern, .. } => Some(pattern),
            FnParam::Receiver(_) => None,
        });
        for (index, pattern) in typed.enumerate() {
            if matches!(pattern.kind, PatternKind::Ident { .. }) {
                self.params.insert(pattern.id, (key.clone(), index));
            }
        }
    }

    /// Records the binding a write to `place` reaches. `through_value` is a
    /// write that changes the value `place` holds rather than replacing it,
    /// which a synchronisation handle there takes on behalf of every holder.
    fn note_write(&mut self, place: &Expr, through_value: bool) {
        let Some((binding, _)) = place_root(self.resolutions, place) else {
            return;
        };
        if through_value
            && !self
                .table
                .get(place.id)
                .is_some_and(|ty| is_value(self.tcx, ty))
        {
            return;
        }
        self.written.insert(binding);
    }

    fn ty_of(&self, use_id: NodeId, binding: NodeId) -> Option<Ty> {
        self.table.get(use_id).or_else(|| self.table.get(binding))
    }

    /// Whether a value of `ty` may hold a closure, and with it captures.
    fn may_hold_closure(&self, ty: Ty) -> bool {
        self.holds_closure(ty, &mut HashSet::new())
    }

    fn holds_closure(&self, ty: Ty, seen: &mut HashSet<Ty>) -> bool {
        if !seen.insert(ty) {
            return false;
        }
        match self.tcx.kind_of(ty) {
            TyKind::FnPtr(_)
            | TyKind::FnTrait(_)
            | TyKind::Closure { .. }
            | TyKind::Param { .. }
            | TyKind::Dyn(_)
            | TyKind::Var(_) => true,
            TyKind::Tuple(parts) => parts.iter().any(|part| self.holds_closure(*part, seen)),
            TyKind::Array { elem, .. } => self.holds_closure(*elem, seen),
            TyKind::Slice(inner)
            | TyKind::Vec(inner)
            | TyKind::Iterator(inner)
            | TyKind::Range(inner)
            | TyKind::Sender(inner)
            | TyKind::Receiver(inner)
            | TyKind::JoinHandle(inner)
            | TyKind::Ref { inner, .. } => self.holds_closure(*inner, seen),
            TyKind::HashMap { key, value, .. } => {
                self.holds_closure(*key, seen) || self.holds_closure(*value, seen)
            }
            TyKind::Nominal { repr, .. } => self.holds_closure(*repr, seen),
            TyKind::Adt { def, substs } | TyKind::Alias { def, substs } => {
                substs
                    .types()
                    .into_iter()
                    .any(|part| self.holds_closure(part, seen))
                    || self
                        .tcx
                        .struct_field_tys(*def)
                        .is_some_and(|fields| fields.iter().any(|f| self.holds_closure(*f, seen)))
                    || self.tcx.enum_variant_tys(*def).is_some_and(|variants| {
                        variants
                            .iter()
                            .flatten()
                            .any(|field| self.holds_closure(*field, seen))
                    })
            }
            _ => false,
        }
    }
}

/// Why a callable cannot cross to another goroutine.
enum Refusal {
    Shares { callable: String, shared: String },
    Opaque { callable: String },
}

/// One closure being walked: the bindings it declares, and whether it is
/// the closure a `spawn` runs.
struct Frame {
    declared: HashSet<NodeId>,
    spawned: bool,
}

struct Walker<'a> {
    facts: &'a Facts<'a>,
    transferred: &'a HashSet<(FnKey, usize)>,
    frames: Vec<Frame>,
    spawned: HashSet<NodeId>,
    /// Parameters this pass found handed to a goroutine.
    reached: HashSet<(FnKey, usize)>,
    /// Refusals by where they point, so a callable checked twice reports once.
    out: BTreeMap<(u32, u32, u32), CaptureWriteDiagnostic>,
}

impl Visitor for Walker<'_> {
    fn visit_item(&mut self, item: &Item) {
        if gossamer_ast::cfg::item_is_active(&item.attrs) {
            walk_item(self, item);
        }
    }

    fn visit_pattern(&mut self, pattern: &Pattern) {
        if matches!(pattern.kind, PatternKind::Ident { .. })
            && let Some(frame) = self.frames.last_mut()
        {
            frame.declared.insert(pattern.id);
        }
        walk_pattern(self, pattern);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Closure { .. } => {
                self.frames.push(Frame {
                    declared: HashSet::new(),
                    spawned: self.spawned.contains(&expr.id),
                });
                walk_expr(self, expr);
                self.frames.pop();
                return;
            }
            ExprKind::Call { callee, args } => {
                // The prelude `spawn`, not a module's own (`exec::spawn`).
                if let ExprKind::Path(path) = &callee.kind
                    && let [seg] = path.segments.as_slice()
                    && seg.name.name == "spawn"
                    && let Some(first) = args.first()
                {
                    self.spawned.insert(first.id);
                    self.check_transfer(first, true);
                } else if let Some(Resolution::Def { def, .. }) =
                    self.facts.resolutions.get(callee.id)
                {
                    self.check_transferred_args(&FnKey::Free(def), args);
                }
                if let Some(receiver) = args.first()
                    && self.facts.table.writes_receiver(receiver.id)
                {
                    self.check_write(receiver, true);
                }
            }
            ExprKind::MethodCall {
                receiver,
                name,
                args,
                ..
            } => {
                if self.facts.table.writes_receiver(receiver.id) {
                    self.check_write(receiver, true);
                }
                if let Some(owner) = self.facts.table.method_owner(expr.id) {
                    let owner = owner.rsplit("::").next().unwrap_or(owner);
                    let key = FnKey::Method(owner.to_string(), name.name.clone());
                    self.check_transferred_args(&key, args);
                }
            }
            ExprKind::Assign { place, .. } => self.check_write(place, false),
            ExprKind::Unary {
                op: UnaryOp::RefMut,
                operand,
            } => self.check_write(operand, true),
            _ => {}
        }
        walk_expr(self, expr);
    }
}

impl Walker<'_> {
    /// Reports a write to `place` that reaches a binding a spawned closure
    /// captured. `through_value` is a write that changes the value `place`
    /// holds rather than replacing it, which a synchronisation handle there
    /// takes on behalf of every snapshot.
    fn check_write(&mut self, place: &Expr, through_value: bool) {
        let Some((binding, name)) = place_root(self.facts.resolutions, place) else {
            return;
        };
        if !self.captured_by_spawn(binding) {
            return;
        }
        if through_value
            && !self
                .facts
                .table
                .get(place.id)
                .is_some_and(|ty| is_value(self.facts.tcx, ty))
        {
            return;
        }
        self.report(CaptureWriteKind::SnapshotWrite { name }, place.span);
    }

    /// Whether the innermost closure around the write that does not declare
    /// `binding` is a spawned one: the write then reaches its snapshot.
    fn captured_by_spawn(&self, binding: NodeId) -> bool {
        for frame in self.frames.iter().rev() {
            if frame.declared.contains(&binding) {
                return false;
            }
            if frame.spawned {
                return true;
            }
        }
        false
    }

    /// Checks each argument a call passes to a parameter its callee hands to
    /// a goroutine.
    fn check_transferred_args(&mut self, key: &FnKey, args: &[Expr]) {
        for (index, arg) in args.iter().enumerate() {
            if self.transferred.contains(&(key.clone(), index)) {
                self.check_transfer(arg, false);
            }
        }
    }

    /// Checks `arg`, a callable about to run on another goroutine. A closure
    /// written directly at a `spawn` snapshots what it captures, so only the
    /// callables it captures in turn are checked; any other closure carries
    /// its captures themselves.
    fn check_transfer(&mut self, arg: &Expr, at_spawn: bool) {
        let mut params = Vec::new();
        let verdict = self.transfer_expr(arg, at_spawn, &mut params);
        self.reached.extend(params);
        match verdict {
            Ok(()) => {}
            Err(Refusal::Shares { callable, shared }) => {
                self.report(
                    CaptureWriteKind::SharedCapture { callable, shared },
                    arg.span,
                );
            }
            Err(Refusal::Opaque { callable }) => {
                self.report(CaptureWriteKind::OpaqueCallable { callable }, arg.span);
            }
        }
    }

    fn transfer_expr(
        &self,
        expr: &Expr,
        at_spawn: bool,
        params: &mut Vec<(FnKey, usize)>,
    ) -> Result<(), Refusal> {
        let facts = self.facts;
        match &expr.kind {
            ExprKind::Closure { .. } => {
                let mut seen = HashSet::new();
                for capture in facts.captures.get(&expr.id).into_iter().flatten() {
                    if !at_spawn && facts.written.contains(&capture.binding) {
                        return Err(Refusal::Shares {
                            callable: "this closure".to_string(),
                            shared: capture.name.clone(),
                        });
                    }
                    if facts
                        .ty_of(capture.use_id, capture.binding)
                        .is_some_and(|ty| facts.may_hold_closure(ty))
                    {
                        self.transfer_binding(capture, params, &mut seen)?;
                    }
                }
                Ok(())
            }
            ExprKind::Path(path) => match facts.resolutions.get(expr.id) {
                Some(Resolution::Local(binding)) => {
                    let capture = Capture {
                        binding,
                        use_id: expr.id,
                        name: path
                            .segments
                            .last()
                            .map(|seg| seg.name.name.clone())
                            .unwrap_or_default(),
                    };
                    self.transfer_binding(&capture, params, &mut HashSet::new())
                }
                _ => Ok(()),
            },
            _ if facts
                .table
                .get(expr.id)
                .is_some_and(|ty| facts.may_hold_closure(ty)) =>
            {
                Err(Refusal::Opaque {
                    callable: format!("`{}`", describe(expr)),
                })
            }
            _ => Ok(()),
        }
    }

    /// Checks a binding holding a callable that is about to run on another
    /// goroutine, through every closure it captures in turn.
    fn transfer_binding(
        &self,
        capture: &Capture,
        params: &mut Vec<(FnKey, usize)>,
        seen: &mut HashSet<NodeId>,
    ) -> Result<(), Refusal> {
        let facts = self.facts;
        if !seen.insert(capture.binding) {
            return Ok(());
        }
        let callable = format!("`{}`", capture.name);
        if facts.written.contains(&capture.binding) {
            return Err(Refusal::Opaque { callable });
        }
        if let Some(param) = facts.params.get(&capture.binding) {
            params.push(param.clone());
            return Ok(());
        }
        let Some(closure) = facts.closure_bound.get(&capture.binding) else {
            return if facts
                .ty_of(capture.use_id, capture.binding)
                .is_some_and(|ty| facts.may_hold_closure(ty))
            {
                Err(Refusal::Opaque { callable })
            } else {
                Ok(())
            };
        };
        for inner in facts.captures.get(closure).into_iter().flatten() {
            if facts.written.contains(&inner.binding) {
                return Err(Refusal::Shares {
                    callable,
                    shared: inner.name.clone(),
                });
            }
            if facts
                .ty_of(inner.use_id, inner.binding)
                .is_some_and(|ty| facts.may_hold_closure(ty))
            {
                self.transfer_binding(inner, params, seen)?;
            }
        }
        Ok(())
    }

    fn report(&mut self, kind: CaptureWriteKind, span: Span) {
        let key = (span.file.as_u32(), span.start, span.end);
        self.out
            .entry(key)
            .or_insert(CaptureWriteDiagnostic { kind, span });
    }
}

/// How a callable expression reads in a diagnostic.
fn describe(expr: &Expr) -> String {
    match &expr.kind {
        ExprKind::Path(path) => path
            .segments
            .iter()
            .map(|seg| seg.name.name.as_str())
            .collect::<Vec<_>>()
            .join("::"),
        ExprKind::FieldAccess { receiver, field } => match field {
            FieldSelector::Named(name) => format!("{}.{}", describe(receiver), name.name),
            FieldSelector::Index(index) => format!("{}.{index}", describe(receiver)),
        },
        ExprKind::Call { callee, .. } => format!("{}(..)", describe(callee)),
        ExprKind::MethodCall { receiver, name, .. } => {
            format!("{}.{}(..)", describe(receiver), name.name)
        }
        _ => "callable".to_string(),
    }
}

/// The local binding a place is rooted at, through fields and indices but
/// not through a dereference, which reaches what a reference names.
fn place_root(resolutions: &Resolutions, place: &Expr) -> Option<(NodeId, String)> {
    let mut cur = place;
    loop {
        match &cur.kind {
            ExprKind::FieldAccess { receiver, .. } => cur = receiver,
            ExprKind::Index { base, .. } => cur = base,
            ExprKind::Path(path) if path.segments.len() == 1 => {
                let Some(Resolution::Local(binding)) = resolutions.get(cur.id) else {
                    return None;
                };
                return Some((binding, path.segments[0].name.name.clone()));
            }
            _ => return None,
        }
    }
}

/// Whether a value of `ty` is data a snapshot copies, rather than a handle
/// every snapshot shares.
fn is_value(tcx: &TyCtxt, ty: Ty) -> bool {
    let mut ty = ty;
    while let TyKind::Ref { inner, .. } = tcx.kind_of(ty) {
        ty = *inner;
    }
    match tcx.kind_of(ty) {
        TyKind::Bool
        | TyKind::Char
        | TyKind::Int(_)
        | TyKind::Float(_)
        | TyKind::String
        | TyKind::Tuple(_)
        | TyKind::Array { .. }
        | TyKind::Simd { .. }
        | TyKind::Slice(_)
        | TyKind::Vec(_)
        | TyKind::HashMap { .. }
        | TyKind::Nominal { .. } => true,
        TyKind::Adt { def, .. } => {
            def.local < u32::MAX - crate::checker::HANDLE_SENTINEL_SPAN
                || COLLECTION_DEFS.contains(&def.local)
        }
        _ => false,
    }
}

/// The runtime collections (sets, deques, queues, stacks, heaps, `Option`,
/// `Result`): values a snapshot copies, though their types use the handle
/// sentinels.
const COLLECTION_DEFS: [u32; 9] = [
    u32::MAX,
    u32::MAX - 1,
    u32::MAX - 7,
    u32::MAX - 18,
    u32::MAX - 19,
    u32::MAX - 28,
    u32::MAX - 30,
    u32::MAX - 31,
    u32::MAX - 32,
];
