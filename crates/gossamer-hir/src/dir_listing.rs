//! Lists a directory without each entry's metadata when the program never
//! reads it.
//!
//! `fs::read_dir` answers `DirInfo` values whose `size` and `modified_ms`
//! cost a `stat` per entry. When nothing the program does can observe either
//! field, the `read_dir` wrapper's leaf becomes the names-only listing, which
//! takes each entry's kind from its directory record.
//!
//! The decision is conservative. A value whose type holds a `DirInfo` must be
//! used only in ways that read part of it: a field read other than `size` or
//! `modified_ms`, a binding, an index or loop source, a store into a
//! container, a variant, or a struct, an argument to a program function that
//! takes that concrete type, or a return. Any other use (formatting, `==`, a
//! sort or hash, a generic or library call, serialization) reads every field,
//! and so does a pattern that names either field.
//!
//! Every function the program wrote is scanned. A function the compiler
//! generated (`__gos_*`: derived comparisons, serde walks, library wrappers)
//! is scanned once scanned code names it, since otherwise it never runs.

use std::collections::{HashMap, HashSet};

use gossamer_resolve::DefId;
use gossamer_types::{Ty, TyCtxt, TyKind};

use crate::tree::{
    HirBlock, HirExpr, HirExprKind, HirFn, HirItem, HirItemKind, HirPat, HirPatKind, HirProgram,
    HirStmtKind, HirUnaryOp,
};

/// The struct `fs::read_dir` answers, as the injected wrapper declares it.
const DIR_INFO: &str = "__gos_fs_DirInfo";
/// The wrappers that build `DirInfo` values from the leaves' tuples.
const WRAPPERS: [&str; 2] = ["__gos_fs_read_dir", "__gos_fs_walk_dir"];
/// The full listing and the names-only listing.
const FULL_LEAF: &str = "__gos_fs_read_dir_raw";
const NAMES_LEAF: &str = "__gos_fs_read_dir_names_raw";
/// The fields a names-only listing leaves at `0`.
const METADATA_FIELDS: [&str; 2] = ["size", "modified_ms"];

/// Switches `read_dir` to the names-only listing when no use of a `DirInfo`
/// can observe its `size` or `modified_ms`.
pub(crate) fn drop_unread_dir_metadata(program: &mut HirProgram, tcx: &TyCtxt) {
    let Some(dir_info) = program.items.iter().find_map(|item| match &item.kind {
        HirItemKind::Adt(adt) if adt.name.name == DIR_INFO => item.def,
        _ => None,
    }) else {
        return;
    };
    let mut scan = Scan::new(program, tcx, dir_info);
    let generated: HashMap<&str, &HirItem> = program
        .items
        .iter()
        .filter_map(|item| match &item.kind {
            HirItemKind::Fn(f) if is_generated(&f.name.name) => Some((f.name.name.as_str(), item)),
            _ => None,
        })
        .collect();
    for item in &program.items {
        if !matches!(&item.kind, HirItemKind::Fn(f) if is_generated(&f.name.name)) {
            scan.item(item);
        }
    }
    let mut scanned = HashSet::new();
    while let Some(name) = scan.referenced.pop() {
        if scan.reads_metadata {
            return;
        }
        if let Some(item) = generated.get(name.as_str())
            && scanned.insert(name)
        {
            scan.item(item);
        }
    }
    if scan.reads_metadata {
        return;
    }
    for item in &mut program.items {
        if let HirItemKind::Fn(f) = &mut item.kind
            && f.name.name == WRAPPERS[0]
            && let Some(body) = &mut f.body
        {
            rename_leaf_in_block(&mut body.block);
        }
    }
}

/// How the expression being visited is used by the one around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Use {
    /// It reads only part of the value, or hands the whole value on to a
    /// place whose own uses are visited.
    Part,
    /// It reads every field.
    Whole,
}

struct Scan<'a> {
    tcx: &'a TyCtxt,
    dir_info: DefId,
    /// The parameter types of each program function, by its definition.
    fn_params: HashMap<DefId, Vec<Ty>>,
    /// The parameter types (after `self`) of each method name.
    method_params: HashMap<String, Vec<Vec<Ty>>>,
    holds: HashMap<Ty, bool>,
    /// Generated functions the scanned code names, still to scan.
    referenced: Vec<String>,
    reads_metadata: bool,
}

impl<'a> Scan<'a> {
    fn new(program: &HirProgram, tcx: &'a TyCtxt, dir_info: DefId) -> Self {
        let mut fn_params = HashMap::new();
        let mut method_params: HashMap<String, Vec<Vec<Ty>>> = HashMap::new();
        for item in &program.items {
            match &item.kind {
                HirItemKind::Fn(f) => {
                    if let Some(def) = item.def {
                        fn_params.insert(def, f.params.iter().map(|p| p.ty).collect());
                    }
                }
                HirItemKind::Impl(imp) => {
                    for m in &imp.methods {
                        let skip = usize::from(m.has_self);
                        method_params
                            .entry(m.name.name.clone())
                            .or_default()
                            .push(m.params.iter().skip(skip).map(|p| p.ty).collect());
                    }
                }
                _ => {}
            }
        }
        Self {
            tcx,
            dir_info,
            fn_params,
            method_params,
            holds: HashMap::new(),
            referenced: Vec::new(),
            reads_metadata: false,
        }
    }

    fn item(&mut self, item: &HirItem) {
        match &item.kind {
            HirItemKind::Fn(f) => {
                if !WRAPPERS.contains(&f.name.name.as_str()) {
                    self.function(f);
                }
            }
            // A method of `DirInfo` itself (its derived `eq`, `cmp`, `hash`)
            // runs only when the program calls it or compares two values,
            // and either of those is a use of the whole value already.
            HirItemKind::Impl(imp) if self.is_dir_info(imp.self_ty) => {}
            HirItemKind::Impl(imp) => imp.methods.iter().for_each(|m| self.function(m)),
            HirItemKind::Trait(t) => t.methods.iter().for_each(|m| self.function(m)),
            HirItemKind::Const(c) => self.expr(&c.value, Use::Whole),
            HirItemKind::Static(s) => self.expr(&s.value, Use::Whole),
            HirItemKind::Adt(_) => {}
        }
    }

    fn function(&mut self, f: &HirFn) {
        for param in &f.params {
            self.pattern(&param.pattern);
        }
        if let Some(body) = &f.body {
            self.block(&body.block, Use::Part);
        }
    }

    fn block(&mut self, block: &HirBlock, tail_use: Use) {
        for stmt in &block.stmts {
            match &stmt.kind {
                HirStmtKind::Let { pattern, init, .. } => {
                    self.pattern(pattern);
                    if let Some(init) = init {
                        self.expr(init, Use::Part);
                    }
                }
                HirStmtKind::Expr { expr, .. } | HirStmtKind::Defer(expr) => {
                    self.expr(expr, Use::Part);
                }
                HirStmtKind::Item(item) => self.item(item),
            }
        }
        if let Some(tail) = &block.tail {
            self.expr(tail, tail_use);
        }
    }

    /// Whether `ty` is, or holds, a `DirInfo`.
    fn holds(&mut self, ty: Ty) -> bool {
        if let Some(&known) = self.holds.get(&ty) {
            return known;
        }
        // A type that holds itself answers `false` until its walk finishes.
        self.holds.insert(ty, false);
        let answer = match self.tcx.kind_of(ty).clone() {
            TyKind::Adt { def, substs } => {
                def == self.dir_info
                    || substs.types().into_iter().any(|t| self.holds(t))
                    || self
                        .tcx
                        .struct_field_tys(def)
                        .map(<[Ty]>::to_vec)
                        .unwrap_or_default()
                        .into_iter()
                        .any(|t| self.holds(t))
                    || self
                        .tcx
                        .enum_variant_tys(def)
                        .map(<[Vec<Ty>]>::to_vec)
                        .unwrap_or_default()
                        .into_iter()
                        .flatten()
                        .any(|t| self.holds(t))
            }
            TyKind::Vec(t)
            | TyKind::Slice(t)
            | TyKind::Iterator(t)
            | TyKind::Sender(t)
            | TyKind::Receiver(t)
            | TyKind::JoinHandle(t) => self.holds(t),
            TyKind::Array { elem, .. } => self.holds(elem),
            TyKind::Ref { inner, .. } => self.holds(inner),
            TyKind::Tuple(parts) => parts.into_iter().any(|t| self.holds(t)),
            TyKind::HashMap { key, value, .. } => self.holds(key) || self.holds(value),
            TyKind::Nominal { repr, .. } => self.holds(repr),
            // An alias this scan does not expand may name one.
            TyKind::Alias { .. } => true,
            // A callable is not a value of the types it takes; a closure's
            // uses of its parameters are visited where it is written. A type
            // parameter's argument is checked where the generic is called.
            TyKind::Bool
            | TyKind::Char
            | TyKind::String
            | TyKind::Int(_)
            | TyKind::Float(_)
            | TyKind::Unit
            | TyKind::Never
            | TyKind::Simd { .. }
            | TyKind::Range(_)
            | TyKind::Duration
            | TyKind::Instant
            | TyKind::JsonValue
            | TyKind::DynValue
            | TyKind::DynError
            | TyKind::FnDef { .. }
            | TyKind::FnPtr(_)
            | TyKind::FnTrait(_)
            | TyKind::Closure { .. }
            | TyKind::Dyn(_)
            | TyKind::Var(_)
            | TyKind::Param { .. }
            | TyKind::Error => false,
        };
        self.holds.insert(ty, answer);
        answer
    }

    fn is_dir_info(&self, ty: Ty) -> bool {
        let mut ty = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(ty) {
            ty = *inner;
        }
        matches!(self.tcx.kind_of(ty), TyKind::Adt { def, .. } if *def == self.dir_info)
    }

    fn pattern(&mut self, pat: &HirPat) {
        match &pat.kind {
            HirPatKind::Struct { fields, .. } => {
                if self.is_dir_info(pat.ty)
                    && fields
                        .iter()
                        .any(|f| METADATA_FIELDS.contains(&f.name.name.as_str()))
                {
                    self.reads_metadata = true;
                }
                for field in fields {
                    if let Some(p) = &field.pattern {
                        self.pattern(p);
                    }
                }
            }
            HirPatKind::Tuple(parts)
            | HirPatKind::Or(parts)
            | HirPatKind::Variant { fields: parts, .. } => {
                for part in parts {
                    self.pattern(part);
                }
            }
            HirPatKind::Slice { .. }
            | HirPatKind::Ref { .. }
            | HirPatKind::At { .. }
            | HirPatKind::Range { .. } => {
                // These name sub-patterns this scan does not walk, so a value
                // of the right type reaching one counts as read whole.
                if self.holds(pat.ty) {
                    self.reads_metadata = true;
                }
            }
            HirPatKind::Wildcard
            | HirPatKind::Binding { .. }
            | HirPatKind::Literal(_)
            | HirPatKind::Rest => {}
        }
    }

    fn expr(&mut self, expr: &HirExpr, how: Use) {
        if self.reads_metadata {
            return;
        }
        if how == Use::Whole && self.holds(expr.ty) {
            self.reads_metadata = true;
            return;
        }
        match &expr.kind {
            HirExprKind::Path { segments, .. } => {
                if let Some(last) = segments.last()
                    && is_generated(&last.name)
                {
                    self.referenced.push(last.name.clone());
                }
            }
            HirExprKind::Literal(_)
            | HirExprKind::Continue { .. }
            | HirExprKind::Placeholder
            | HirExprKind::LiftedClosure { .. } => {}
            HirExprKind::Field { receiver, name } => {
                if self.is_dir_info(receiver.ty) && METADATA_FIELDS.contains(&name.name.as_str()) {
                    self.reads_metadata = true;
                    return;
                }
                self.expr(receiver, Use::Part);
            }
            HirExprKind::TupleIndex { receiver, .. } => self.expr(receiver, Use::Part),
            HirExprKind::Index { base, index } => {
                self.expr(base, Use::Part);
                self.expr(index, Use::Whole);
            }
            HirExprKind::Unary { op, operand } => {
                let inner = match op {
                    HirUnaryOp::RefShared | HirUnaryOp::RefMut | HirUnaryOp::Deref => how,
                    _ => Use::Whole,
                };
                self.expr(operand, inner);
            }
            HirExprKind::Binary { lhs, rhs, .. } => {
                self.expr(lhs, Use::Whole);
                self.expr(rhs, Use::Whole);
            }
            HirExprKind::Assign { place, value } => {
                self.expr(place, Use::Part);
                self.expr(value, Use::Part);
            }
            HirExprKind::If { .. }
            | HirExprKind::Match { .. }
            | HirExprKind::Loop { .. }
            | HirExprKind::While { .. }
            | HirExprKind::Block(_)
            | HirExprKind::Closure { .. } => self.control_flow(expr, how),
            HirExprKind::Return(value) | HirExprKind::Break { value, .. } => {
                if let Some(v) = value {
                    self.expr(v, Use::Part);
                }
            }
            HirExprKind::Tuple(parts) => parts.iter().for_each(|p| self.expr(p, Use::Part)),
            HirExprKind::Array(array) => {
                for_each_array_part(array, |p| self.expr(p, Use::Part));
            }
            HirExprKind::Cast { value, .. } => self.expr(value, Use::Whole),
            HirExprKind::Range { start, end, .. } => {
                for e in [start, end].into_iter().flatten() {
                    self.expr(e, Use::Whole);
                }
            }
            HirExprKind::Select { .. } => {
                crate::tree::for_each_child_expr(expr, &mut |child| {
                    self.expr(child, Use::Whole);
                });
            }
            HirExprKind::Call { callee, args } => self.call(callee, args),
            HirExprKind::MethodCall {
                receiver,
                name,
                args,
                ..
            } => self.method_call(receiver, &name.name, args),
        }
    }

    /// [`Self::expr`] for the branching and looping forms, whose value is
    /// the one a branch or body answers.
    fn control_flow(&mut self, expr: &HirExpr, how: Use) {
        match &expr.kind {
            HirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.expr(condition, Use::Whole);
                self.expr(then_branch, how);
                if let Some(e) = else_branch {
                    self.expr(e, how);
                }
            }
            HirExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee, Use::Part);
                for arm in arms {
                    self.pattern(&arm.pattern);
                    if let Some(g) = &arm.guard {
                        self.expr(g, Use::Whole);
                    }
                    self.expr(&arm.body, how);
                }
            }
            HirExprKind::Loop { body, .. } => self.expr(body, Use::Part),
            HirExprKind::While {
                condition, body, ..
            } => {
                self.expr(condition, Use::Whole);
                self.expr(body, Use::Part);
            }
            HirExprKind::Block(block) => self.block(block, how),
            HirExprKind::Closure { params, body, .. } => {
                for param in params {
                    self.pattern(&param.pattern);
                }
                self.expr(body, Use::Part);
            }
            _ => {}
        }
    }

    fn call(&mut self, callee: &HirExpr, args: &[HirExpr]) {
        let takes = self.callee_parts(callee, args.len());
        self.expr(callee, Use::Part);
        for (i, arg) in args.iter().enumerate() {
            self.expr(arg, takes.get(i).copied().unwrap_or(Use::Whole));
        }
    }

    /// How a call to `callee` with `arity` arguments uses each one.
    fn callee_parts(&mut self, callee: &HirExpr, arity: usize) -> Vec<Use> {
        let every = |how| vec![how; arity];
        // A closure value runs a body visited where the closure is written.
        if matches!(
            self.tcx.kind_of(callee.ty),
            TyKind::FnPtr(_) | TyKind::FnTrait(_)
        ) && !matches!(&callee.kind, HirExprKind::Path { def: Some(_), .. })
        {
            return every(Use::Part);
        }
        let HirExprKind::Path { segments, def } = &callee.kind else {
            return every(Use::Whole);
        };
        let last = segments.last().map_or("", |s| s.name.as_str());
        // A struct literal, an enum variant, or `Some` / `Ok` / `Err` stores
        // its arguments, whose later uses are visited where they happen.
        if last == "__struct" || last.starts_with(|c: char| c.is_ascii_uppercase()) {
            return every(Use::Part);
        }
        // A program function reads its parameters where its own body is
        // visited, unless the parameter is generic.
        if let Some(params) = def.and_then(|d| self.fn_params.get(&d).cloned()) {
            return params.iter().map(|&t| self.concrete_part(t)).collect();
        }
        every(Use::Whole)
    }

    /// `Part` for a parameter of a concrete type, `Whole` for one that
    /// names a type parameter.
    fn concrete_part(&self, ty: Ty) -> Use {
        if mentions_type_param(self.tcx, ty) {
            Use::Whole
        } else {
            Use::Part
        }
    }

    fn method_call(&mut self, receiver: &HirExpr, name: &str, args: &[HirExpr]) {
        let (on_receiver, on_args) = self.method_parts(receiver.ty, name, args.len());
        self.expr(receiver, on_receiver);
        for (i, arg) in args.iter().enumerate() {
            self.expr(arg, on_args.get(i).copied().unwrap_or(Use::Whole));
        }
    }

    /// How a method call uses its receiver and each argument.
    fn method_parts(&mut self, receiver: Ty, name: &str, arity: usize) -> (Use, Vec<Use>) {
        let mut ty = receiver;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(ty) {
            ty = *inner;
        }
        if !self.holds(ty) {
            // A program method takes its arguments as its parameters say.
            let args = match self.method_params.get(name).cloned() {
                Some(overloads) => (0..arity)
                    .map(|i| {
                        if overloads
                            .iter()
                            .all(|p| p.get(i).is_some_and(|&t| !mentions_type_param(self.tcx, t)))
                        {
                            Use::Part
                        } else {
                            Use::Whole
                        }
                    })
                    .collect(),
                None => vec![Use::Whole; arity],
            };
            return (Use::Whole, args);
        }
        let sequence = matches!(
            self.tcx.kind_of(ty),
            TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Array { .. } | TyKind::Iterator(_)
        );
        let carrier = matches!(
            self.tcx.kind_of(ty),
            TyKind::Adt { def, .. } if def.local == u32::MAX || def.local == u32::MAX - 1
        );
        let part = if sequence {
            SEQUENCE_PART_METHODS.contains(&name)
        } else if carrier {
            CARRIER_PART_METHODS.contains(&name)
        } else {
            // The value itself: a copy hands it on whole; anything else reads it.
            name == "clone"
        };
        if part {
            (Use::Part, vec![Use::Part; arity])
        } else {
            (Use::Whole, vec![Use::Whole; arity])
        }
    }
}

/// Sequence methods that reach elements only through the callbacks and
/// bindings they hand them to, or move them without reading them.
const SEQUENCE_PART_METHODS: &[&str] = &[
    "all",
    "any",
    "append",
    "chain",
    "chunks",
    "clear",
    "clone",
    "collect",
    "count",
    "enumerate",
    "extend",
    "filter",
    "filter_map",
    "find",
    "find_map",
    "first",
    "flat_map",
    "for_each",
    "get",
    "insert",
    "into_iter",
    "is_empty",
    "iter",
    "last",
    "len",
    "map",
    "max_by_key",
    "min_by_key",
    "next",
    "pop",
    "position",
    "push",
    "remove",
    "retain",
    "rev",
    "reverse",
    "skip",
    "skip_while",
    "sort_by",
    "sort_by_key",
    "split_off",
    "step_by",
    "swap",
    "take",
    "take_while",
    "to_vec",
    "truncate",
    "windows",
    "zip",
];

/// `Option` / `Result` methods that hand the payload on without reading it.
const CARRIER_PART_METHODS: &[&str] = &[
    "and_then",
    "clone",
    "err",
    "expect",
    "filter",
    "is_err",
    "is_none",
    "is_ok",
    "is_some",
    "iter",
    "map",
    "map_err",
    "ok",
    "ok_or",
    "ok_or_else",
    "or",
    "or_else",
    "unwrap",
    "unwrap_or",
    "unwrap_or_else",
];

/// Whether `name` is a function the compiler generated rather than one the
/// program wrote.
fn is_generated(name: &str) -> bool {
    name.starts_with("__gos_")
}

/// Whether `ty` names a type parameter anywhere inside it.
fn mentions_type_param(tcx: &TyCtxt, ty: Ty) -> bool {
    match tcx.kind_of(ty).clone() {
        TyKind::Param { .. } | TyKind::Var(_) => true,
        TyKind::Vec(t) | TyKind::Slice(t) | TyKind::Iterator(t) => mentions_type_param(tcx, t),
        TyKind::Array { elem, .. } => mentions_type_param(tcx, elem),
        TyKind::Ref { inner, .. } => mentions_type_param(tcx, inner),
        TyKind::Tuple(parts) => parts.into_iter().any(|t| mentions_type_param(tcx, t)),
        TyKind::HashMap { key, value, .. } => {
            mentions_type_param(tcx, key) || mentions_type_param(tcx, value)
        }
        TyKind::Adt { substs, .. } => substs
            .types()
            .into_iter()
            .any(|t| mentions_type_param(tcx, t)),
        _ => false,
    }
}

fn for_each_array_part(array: &crate::tree::HirArrayExpr, mut f: impl FnMut(&HirExpr)) {
    match array {
        crate::tree::HirArrayExpr::List(parts) => parts.iter().for_each(f),
        crate::tree::HirArrayExpr::Repeat { value, count } => {
            f(value);
            f(count);
        }
    }
}

/// Renames the full listing's leaf to the names-only one in `block`.
fn rename_leaf_in_block(block: &mut HirBlock) {
    let mut seen = HashSet::new();
    for stmt in &mut block.stmts {
        match &mut stmt.kind {
            HirStmtKind::Let { init: Some(e), .. }
            | HirStmtKind::Expr { expr: e, .. }
            | HirStmtKind::Defer(e) => rename_leaf(e, &mut seen),
            _ => {}
        }
    }
    if let Some(tail) = &mut block.tail {
        rename_leaf(tail, &mut seen);
    }
}

fn rename_leaf(expr: &mut HirExpr, seen: &mut HashSet<crate::ids::HirId>) {
    if !seen.insert(expr.id) {
        return;
    }
    if let HirExprKind::Path { segments, .. } = &mut expr.kind
        && let [only] = segments.as_mut_slice()
        && only.name == FULL_LEAF
    {
        only.name = NAMES_LEAF.to_string();
        return;
    }
    if let HirExprKind::Block(block) = &mut expr.kind {
        rename_leaf_in_block(block);
        return;
    }
    crate::tree::for_each_child_expr_mut(expr, &mut |child| rename_leaf(child, seen));
}
