//! Rewrites labelled and defaulted call arguments into positional order.
//!
//! `f(width: 10)` and `fn f(width: i64 = 10)` are caller-side spellings
//! only. This pass runs between resolution and type checking, turns every
//! call into the plain positional form its callee declares, and empties
//! [`SourceFile::named_args`]. Nothing after it - the checker, HIR, MIR,
//! any tier's codegen - can tell a labelled call from a positional one,
//! so the calling convention is untouched and no tier needs to agree
//! about anything new.

#![forbid(unsafe_code)]

use std::collections::HashMap;

use gossamer_ast::visitor::{VisitorMut, walk_expr_mut, walk_item_mut};
use gossamer_ast::{
    Expr, ExprKind, FnDecl, FnParam, ImplItem, Item, ItemKind, Literal, ModBody, NodeId,
    NodeIdGenerator, PathExpr, SourceFile, TraitItem, Type, TypeKind,
};
use gossamer_lex::Span;

use crate::DefId;
use crate::diagnostic::{ResolveDiagnostic, ResolveError};
use crate::resolutions::{Resolution, Resolutions};

/// One callee's parameters, in declared order.
#[derive(Debug, Clone, Default, PartialEq)]
struct Signature {
    /// Parameter names, excluding any `self` receiver.
    names: Vec<String>,
    /// Constant default for each parameter, positionally.
    defaults: Vec<Option<Expr>>,
}

impl Signature {
    fn has_defaults(&self) -> bool {
        self.defaults.iter().any(Option::is_some)
    }

    /// Two declarations rewrite a call identically when they agree on
    /// parameter names and on the value of every default. Node ids differ
    /// between declarations, so defaults are compared by their spelling.
    fn rewrites_same_as(&self, other: &Self) -> bool {
        self.names == other.names
            && self.defaults.len() == other.defaults.len()
            && self
                .defaults
                .iter()
                .zip(&other.defaults)
                .all(|(a, b)| a.as_ref().map(default_key) == b.as_ref().map(default_key))
    }
}

/// Canonical spelling of a constant default, for comparing two
/// declarations without comparing their node ids.
fn default_key(expr: &Expr) -> String {
    match &expr.kind {
        ExprKind::Literal(lit) => format!("{lit:?}"),
        ExprKind::Unary { op, operand } => format!("{op:?}{}", default_key(operand)),
        _ => String::new(),
    }
}

/// How a call's written arguments map onto the callee's parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Plan {
    /// Already positional and complete; leave the call alone.
    Unchanged,
    /// Take `args[from]` for each parameter, or splice its default.
    Reorder(Vec<Slot>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// Move the argument written at this index into this position.
    Arg(usize),
    /// Splice the parameter's declared default here.
    Default(usize),
}

/// Rewrites every labelled or defaulted call in `sf` into positional
/// order, returning one diagnostic per call it could not.
pub fn resolve_named_arguments(
    sf: &mut SourceFile,
    resolutions: &Resolutions,
) -> Vec<ResolveDiagnostic> {
    let mut signatures = SignatureTable::default();
    for item in sf
        .items
        .iter()
        .filter(|item| crate::cfg::item_is_active(&item.attrs))
    {
        signatures.collect_item(item, resolutions);
    }
    let mut diagnostics = Vec::new();
    for item in sf
        .items
        .iter()
        .filter(|item| crate::cfg::item_is_active(&item.attrs))
    {
        check_defaults(item, &mut diagnostics);
    }
    let labels = std::mem::take(&mut sf.named_args);
    let mut ids = NodeIdGenerator::new();
    while ids.issued() < sf.next_node_id {
        let _ = ids.next();
    }
    let mut pass = Rewrite {
        signatures,
        resolutions,
        labels,
        ids: &mut ids,
        diagnostics,
    };
    pass.visit_source_file(sf);
    let diagnostics = std::mem::take(&mut pass.diagnostics);
    sf.next_node_id = ids.issued();
    diagnostics
}

/// Every callee's parameters, indexed both ways a call site can reach one.
#[derive(Debug, Default)]
struct SignatureTable {
    /// Free and module-level functions, by the definition a path
    /// resolves to.
    free: HashMap<DefId, Signature>,
    /// Methods and associated functions by `(owner type, name)`. An
    /// `impl` item carries no node id, so a path like `Point::new` is
    /// matched on the spelling of its qualifier instead of a definition.
    associated: HashMap<(String, String), Signature>,
    /// The same declarations by name alone. A method call's receiver type
    /// is not known until type checking, so `x.m(name: v)` is rewritten
    /// only when every declaration of that name would rewrite it
    /// identically.
    methods: HashMap<String, Vec<(String, Signature)>>,
}

impl SignatureTable {
    fn record_associated(&mut self, owner: &str, decl: &FnDecl) {
        let sig = signature_of(decl);
        self.associated
            .insert((owner.to_string(), decl.name.name.clone()), sig.clone());
        self.methods
            .entry(decl.name.name.clone())
            .or_default()
            .push((owner.to_string(), sig));
    }

    fn collect_item(&mut self, item: &Item, resolutions: &Resolutions) {
        match &item.kind {
            ItemKind::Fn(decl) => {
                if let Some(def) = resolutions.definition_of(item.id) {
                    self.free.insert(def, signature_of(decl));
                }
            }
            ItemKind::Mod(decl) => {
                if let ModBody::Inline(items) = &decl.body {
                    for inner in items
                        .iter()
                        .filter(|i| crate::cfg::item_is_active(&i.attrs))
                    {
                        self.collect_item(inner, resolutions);
                    }
                }
            }
            ItemKind::Impl(decl) => {
                let owner = type_head_name(&decl.self_ty);
                for inner in &decl.items {
                    if let ImplItem::Fn(fn_decl) = inner {
                        self.record_associated(&owner, fn_decl);
                    }
                }
            }
            ItemKind::Trait(decl) => {
                for inner in &decl.items {
                    if let TraitItem::Fn(fn_decl) = inner {
                        self.record_associated(&decl.name.name, fn_decl);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Parameters of a compiler-known call, so a label on one binds the way a
/// label on a declared function does. `spawn` is the only one: it is a
/// primitive rather than a declaration, so nothing else would give
/// `spawn(f, reason: "..")` a parameter to bind against.
fn builtin_signature(path: &PathExpr) -> Option<Signature> {
    if path.segments.len() != 1 || path.segments[0].name.name != "spawn" {
        return None;
    }
    Some(Signature {
        names: vec!["f".to_string(), "reason".to_string()],
        defaults: vec![None, None],
    })
}

/// Reads a declaration's parameter names and defaults, dropping any
/// receiver - a method call's receiver is not one of its arguments.
fn signature_of(decl: &FnDecl) -> Signature {
    let mut sig = Signature::default();
    for param in &decl.params {
        if let FnParam::Typed {
            pattern, default, ..
        } = param
        {
            sig.names.push(binding_name(pattern));
            sig.defaults.push(default.as_deref().cloned());
        }
    }
    sig
}

struct Rewrite<'a> {
    signatures: SignatureTable,
    resolutions: &'a Resolutions,
    labels: HashMap<NodeId, Vec<gossamer_ast::NamedArg>>,
    ids: &'a mut NodeIdGenerator,
    diagnostics: Vec<ResolveDiagnostic>,
}

impl VisitorMut for Rewrite<'_> {
    // The resolver leaves an inactive item unresolved, so its calls have no
    // callee to match a label against.
    fn visit_item(&mut self, item: &mut Item) {
        if crate::cfg::item_is_active(&item.attrs) {
            walk_item_mut(self, item);
        }
    }

    fn visit_expr(&mut self, expr: &mut Expr) {
        walk_expr_mut(self, expr);
        let id = expr.id;
        let span = expr.span;
        let written = self.labels.remove(&id).unwrap_or_default();
        let Some(sig) = self.signature_for(expr, &written, span) else {
            return;
        };
        let (ExprKind::Call { args, .. } | ExprKind::MethodCall { args, .. }) = &mut expr.kind
        else {
            return;
        };
        if written.is_empty() && !sig.has_defaults() {
            return;
        }
        match self.plan(&sig, &written, args.len(), span) {
            Some(Plan::Reorder(slots)) => apply(args, &slots, &sig, self.ids),
            Some(Plan::Unchanged) | None => {}
        }
    }
}

impl Rewrite<'_> {
    /// Finds the callee's parameters, or reports why it could not.
    fn signature_for(
        &mut self,
        expr: &Expr,
        written: &[gossamer_ast::NamedArg],
        span: Span,
    ) -> Option<Signature> {
        match &expr.kind {
            ExprKind::Call { callee, .. } => {
                let ExprKind::Path(path) = &callee.kind else {
                    return self.unsupported_target(written, "this callee", span);
                };
                if let Some(def) = self.def_of(callee.id)
                    && let Some(sig) = self.signatures.free.get(&def)
                {
                    return Some(sig.clone());
                }
                // `Point::new(..)`: an `impl` item has no node id to resolve
                // against, so the qualifier's spelling selects the impl.
                if path.segments.len() >= 2 {
                    let owner = path.segments[path.segments.len() - 2].name.name.clone();
                    let name = path.segments[path.segments.len() - 1].name.name.clone();
                    if let Some(sig) = self.signatures.associated.get(&(owner, name)) {
                        return Some(sig.clone());
                    }
                }
                if let Some(sig) = builtin_signature(path) {
                    return Some(sig);
                }
                self.unsupported_target(written, &describe_path(path), span)
            }
            ExprKind::MethodCall { name, args, .. } => {
                let candidates = self.signatures.methods.get(&name.name)?;
                let (first_owner, first) = candidates.first()?;
                // The receiver's type is settled during type checking, after
                // this pass. Rewriting is safe only when every declaration of
                // the name would produce the same positional call.
                let Some((other_owner, _)) = candidates
                    .iter()
                    .find(|(_, sig)| !sig.rewrites_same_as(first))
                else {
                    return Some(first.clone());
                };
                // Declarations disagree. A call that already supplies every
                // argument by position needs no rewrite, so it is left alone
                // and the checker resolves the receiver as usual. Any
                // declaration the call could be filling is enough: the
                // candidates are indexed by name alone, so unrelated types
                // that happen to share a method name appear here, and
                // matching only the first would reject `c.get(2)` because
                // some other type's `get` takes two.
                if written.is_empty()
                    && candidates
                        .iter()
                        .any(|(_, sig)| args.len() == sig.names.len())
                {
                    return None;
                }
                self.diagnostics.push(ResolveDiagnostic::new(
                    ResolveError::AmbiguousNamedArgument {
                        method: name.name.clone(),
                        first: first_owner.clone(),
                        second: other_owner.clone(),
                    },
                    span,
                ));
                None
            }
            _ => None,
        }
    }

    fn unsupported_target(
        &mut self,
        written: &[gossamer_ast::NamedArg],
        what: &str,
        span: Span,
    ) -> Option<Signature> {
        if let Some(label) = written.first() {
            self.diagnostics.push(ResolveDiagnostic::new(
                ResolveError::NamedArgumentTarget {
                    name: label.name.name.clone(),
                    target: what.to_string(),
                },
                label.span,
            ));
            let _ = span;
        }
        None
    }

    fn def_of(&self, node: NodeId) -> Option<DefId> {
        match self.resolutions.get(node) {
            Some(Resolution::Def { def, .. }) => Some(def),
            Some(Resolution::Import { .. }) => self.resolutions.import_def(node),
            _ => None,
        }
    }

    /// Works out which written argument fills each parameter.
    fn plan(
        &mut self,
        sig: &Signature,
        written: &[gossamer_ast::NamedArg],
        arg_count: usize,
        span: Span,
    ) -> Option<Plan> {
        let labelled: HashMap<usize, &gossamer_ast::NamedArg> =
            written.iter().map(|l| (l.index, l)).collect();
        // A positional argument after a labelled one has no position left to
        // mean: the labels have already claimed parameters out of order.
        if let Some(first) = written.iter().map(|l| l.index).min()
            && (first..arg_count).any(|i| !labelled.contains_key(&i))
        {
            self.diagnostics.push(ResolveDiagnostic::new(
                ResolveError::PositionalAfterNamed,
                span,
            ));
            return None;
        }
        let mut slots: Vec<Option<Slot>> = vec![None; sig.names.len()];
        for i in 0..arg_count {
            let target = match labelled.get(&i) {
                Some(label) => {
                    let Some(position) = sig.names.iter().position(|n| *n == label.name.name)
                    else {
                        self.diagnostics.push(ResolveDiagnostic::new(
                            ResolveError::UnknownNamedArgument {
                                name: label.name.name.clone(),
                                expected: sig.names.join("`, `"),
                            },
                            label.span,
                        ));
                        return None;
                    };
                    if slots[position].is_some() {
                        self.diagnostics.push(ResolveDiagnostic::new(
                            ResolveError::DuplicateNamedArgument {
                                name: label.name.name.clone(),
                            },
                            label.span,
                        ));
                        return None;
                    }
                    position
                }
                // Unlabelled arguments keep the position they were written in.
                None if i < sig.names.len() => i,
                // More arguments than parameters is an arity error, and the
                // checker reports arity with the types in hand.
                None => return None,
            };
            slots[target] = Some(Slot::Arg(i));
        }
        // A parameter with no argument and no default. Arity alone cannot
        // explain this once names and defaults are in play - the call may
        // have the right number of arguments and still miss a parameter, or
        // omit one that has no default - so name the parameters instead.
        let missing: Vec<&str> = slots
            .iter()
            .enumerate()
            .filter(|(position, slot)| slot.is_none() && sig.defaults[*position].is_none())
            .map(|(position, _)| sig.names[position].as_str())
            .collect();
        if !missing.is_empty() {
            let optional: Vec<String> = sig
                .names
                .iter()
                .enumerate()
                .filter(|(position, _)| sig.defaults[*position].is_some())
                .map(|(_, name)| format!("`{name}`"))
                .collect();
            self.diagnostics.push(ResolveDiagnostic::new(
                ResolveError::MissingRequiredArgument {
                    missing: missing
                        .iter()
                        .map(|name| format!("`{name}`"))
                        .collect::<Vec<_>>()
                        .join(", "),
                    plural: missing.len() > 1,
                    optional: optional.join(", "),
                },
                span,
            ));
            return None;
        }
        let mut out = Vec::with_capacity(slots.len());
        for (position, slot) in slots.iter().enumerate() {
            match slot {
                Some(slot) => out.push(*slot),
                None if sig.defaults[position].is_some() => out.push(Slot::Default(position)),
                // Every remaining gap was reported above.
                None => return None,
            }
        }
        if out
            .iter()
            .enumerate()
            .all(|(position, slot)| *slot == Slot::Arg(position))
        {
            return Some(Plan::Unchanged);
        }
        Some(Plan::Reorder(out))
    }
}

/// Rebuilds `args` in declared order, splicing defaults for the gaps.
fn apply(args: &mut Vec<Expr>, slots: &[Slot], sig: &Signature, ids: &mut NodeIdGenerator) {
    let mut taken: Vec<Option<Expr>> = args.drain(..).map(Some).collect();
    for slot in slots {
        match slot {
            Slot::Arg(i) => {
                if let Some(expr) = taken.get_mut(*i).and_then(Option::take) {
                    args.push(expr);
                }
            }
            Slot::Default(position) => {
                if let Some(default) = &sig.defaults[*position] {
                    args.push(fresh_copy(default, ids));
                }
            }
        }
    }
}

/// Copies a default expression for one call site, giving every node an id
/// no other node holds.
fn fresh_copy(expr: &Expr, ids: &mut NodeIdGenerator) -> Expr {
    let mut copy = expr.clone();
    let mut renumber = Renumber { ids };
    renumber.visit_expr(&mut copy);
    copy
}

struct Renumber<'a> {
    ids: &'a mut NodeIdGenerator,
}

impl VisitorMut for Renumber<'_> {
    fn visit_expr(&mut self, expr: &mut Expr) {
        expr.id = self.ids.next();
        walk_expr_mut(self, expr);
    }
}

/// True for the expression forms a parameter default accepts: a literal,
/// optionally negated. Anything else would need resolving at each call
/// site it is spliced into, which this pass runs too late to arrange.
fn is_constant_default(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Literal(_) => true,
        ExprKind::Unary { op, operand } => {
            matches!(op, gossamer_ast::UnaryOp::Neg)
                && matches!(
                    operand.kind,
                    ExprKind::Literal(Literal::Int(_) | Literal::Float(_))
                )
        }
        _ => false,
    }
}

/// Reports any parameter default that is not a constant, everywhere a
/// declaration can appear.
fn check_defaults(item: &Item, out: &mut Vec<ResolveDiagnostic>) {
    match &item.kind {
        ItemKind::Fn(decl) => check_fn_defaults(decl, out),
        ItemKind::Mod(decl) => {
            if let ModBody::Inline(items) = &decl.body {
                for inner in items
                    .iter()
                    .filter(|i| crate::cfg::item_is_active(&i.attrs))
                {
                    check_defaults(inner, out);
                }
            }
        }
        ItemKind::Impl(decl) => {
            for inner in &decl.items {
                if let ImplItem::Fn(fn_decl) = inner {
                    check_fn_defaults(fn_decl, out);
                }
            }
        }
        ItemKind::Trait(decl) => {
            for inner in &decl.items {
                if let TraitItem::Fn(fn_decl) = inner {
                    check_fn_defaults(fn_decl, out);
                }
            }
        }
        _ => {}
    }
}

fn check_fn_defaults(decl: &FnDecl, out: &mut Vec<ResolveDiagnostic>) {
    for param in &decl.params {
        if let FnParam::Typed {
            pattern, default, ..
        } = param
            && let Some(default) = default
            && !is_constant_default(default)
        {
            out.push(ResolveDiagnostic::new(
                ResolveError::NonConstantDefault {
                    name: binding_name(pattern),
                },
                default.span,
            ));
        }
    }
}

/// The name a parameter's pattern binds, or an empty string for a
/// pattern with no single name - a destructuring parameter has no label
/// to be called by.
fn binding_name(pattern: &gossamer_ast::Pattern) -> String {
    match &pattern.kind {
        gossamer_ast::PatternKind::Ident { name, .. } => name.name.clone(),
        _ => String::new(),
    }
}

/// The head name of an `impl` block's self type, used to match a
/// `Type::assoc(..)` path against the impl that declares it.
fn type_head_name(ty: &Type) -> String {
    match &ty.kind {
        TypeKind::Path(path) => path
            .segments
            .last()
            .map(|s| s.name.name.clone())
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn describe_path(path: &PathExpr) -> String {
    path.segments
        .iter()
        .map(|s| s.name.name.as_str())
        .collect::<Vec<_>>()
        .join("::")
}
