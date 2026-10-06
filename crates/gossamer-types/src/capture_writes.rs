//! Writes a closure makes to the bindings it captures.
//!
//! A closure captures a `Vec`, `Map`, `Set`, deque, or heap by managed
//! reference and every other value by copy. A write to a captured copy
//! changes the closure's own value and never the binding it names, so it is
//! refused where it is written. A write that reaches a `Map`, `Set`, deque,
//! or heap held in a field of the copy lands in that shared table and is
//! kept; one that reaches a `Vec` held in the copy is refused, since the
//! copy's `Vec` is not the binding's.

#![forbid(unsafe_code)]

use std::collections::HashSet;

use gossamer_ast::visitor::{Visitor, walk_expr, walk_item, walk_pattern};
use gossamer_ast::{
    Expr, ExprKind, Item, ItemKind, ModBody, NodeId, Pattern, PatternKind, SourceFile, UnaryOp,
};
use gossamer_lex::Span;
use gossamer_resolve::{Resolution, Resolutions};

use crate::context::TyCtxt;
use crate::table::TypeTable;
use crate::ty::TyKind;

/// A write to a binding a closure captured by copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureWriteDiagnostic {
    /// The captured binding.
    pub name: String,
    /// The write.
    pub span: Span,
}

impl CaptureWriteDiagnostic {
    /// The diagnostic code the refusal carries.
    pub const CODE: &'static str = "GT0114";

    /// Renders this refusal as a structured diagnostic.
    #[must_use]
    pub fn to_diagnostic(&self) -> gossamer_diagnostics::Diagnostic {
        use gossamer_diagnostics::{Code, Diagnostic, Location};
        let location = Location::new(self.span.file, self.span);
        let name = &self.name;
        Diagnostic::error(
            Code(Self::CODE),
            format!("this write changes the closure's own copy of `{name}`"),
        )
        .with_primary(location, format!("the enclosing `{name}` is left unchanged"))
        .with_help(
            "a closure captures this value by copy; return the new value, or hold it in a `Vec` or `Map`, which a closure captures by reference",
        )
    }
}

/// Checks every closure in `sf` for writes to the bindings it captures.
#[must_use]
pub fn check_capture_writes(
    sf: &SourceFile,
    resolutions: &Resolutions,
    table: &TypeTable,
    tcx: &TyCtxt,
) -> Vec<CaptureWriteDiagnostic> {
    let mut user_types = HashSet::new();
    collect_user_types(&sf.items, &mut user_types);
    let mut walker = Walker {
        resolutions,
        table,
        tcx,
        user_types,
        frames: Vec::new(),
        adapter_callbacks: HashSet::new(),
        out: Vec::new(),
    };
    walker.visit_source_file(sf);
    walker.out
}

fn collect_user_types(items: &[Item], out: &mut HashSet<String>) {
    for item in items {
        match &item.kind {
            ItemKind::Struct(decl) => {
                out.insert(decl.name.name.clone());
            }
            ItemKind::Enum(decl) => {
                out.insert(decl.name.name.clone());
            }
            ItemKind::Mod(decl) => {
                if let ModBody::Inline(inner) = &decl.body {
                    collect_user_types(inner, out);
                }
            }
            _ => {}
        }
    }
}

/// One closure being walked: the bindings it declares, and whether its
/// writes are reported here. A parallel adapter's callback is held to the
/// adapter's own purity rule (GT0090) instead.
struct Frame {
    declared: HashSet<NodeId>,
    reported: bool,
}

/// What a write does to its place.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Write {
    /// `place = v` or `place op= v`.
    Assign,
    /// A mutating method on `place`.
    Method,
    /// `&mut place` handed to a call.
    Borrow,
}

struct Walker<'a> {
    resolutions: &'a Resolutions,
    table: &'a TypeTable,
    tcx: &'a TyCtxt,
    user_types: HashSet<String>,
    frames: Vec<Frame>,
    adapter_callbacks: HashSet<NodeId>,
    out: Vec<CaptureWriteDiagnostic>,
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
                    reported: !self.adapter_callbacks.contains(&expr.id),
                });
                walk_expr(self, expr);
                self.frames.pop();
                return;
            }
            ExprKind::Assign { place, .. } => self.check_write(place, Write::Assign, expr.span),
            ExprKind::MethodCall {
                receiver,
                name,
                args,
                ..
            } => {
                if name.name.starts_with("par_") {
                    self.adapter_callbacks.extend(args.iter().map(|arg| arg.id));
                }
                if crate::is_mutating_method_name(name.name.as_str()) {
                    self.check_write(receiver, Write::Method, expr.span);
                }
            }
            ExprKind::Unary {
                op: UnaryOp::RefMut,
                operand,
            } => self.check_write(operand, Write::Borrow, expr.span),
            _ => {}
        }
        walk_expr(self, expr);
    }
}

impl Walker<'_> {
    fn check_write(&mut self, place: &Expr, write: Write, span: Span) {
        let Some(frame) = self.frames.last() else {
            return;
        };
        if !frame.reported {
            return;
        }
        let Some((path, binding, name)) = self.place_root(place) else {
            return;
        };
        if frame.declared.contains(&binding) {
            return;
        }
        let Some(root_ty) = self.table.get(path.root) else {
            return;
        };
        if !self.copied_capture(root_ty) {
            return;
        }
        // Each value the write passes through on the way down from the
        // binding: a shared table there takes the write, and a `Vec` there is
        // the copy's own.
        for receiver in &path.receivers {
            let Some(ty) = self.table.get(*receiver) else {
                return;
            };
            if self.shared_table(ty) {
                return;
            }
            if self.is_vec(ty) {
                self.out.push(CaptureWriteDiagnostic { name, span });
                return;
            }
        }
        let Some(target) = self.table.get(place.id) else {
            return;
        };
        let refused = match write {
            Write::Assign => true,
            Write::Method => !self.shared_table(target),
            Write::Borrow => {
                !self.shared_table(target)
                    && (!self.is_user_aggregate(target) || self.holds_vec(target, 0))
            }
        };
        if refused {
            self.out.push(CaptureWriteDiagnostic { name, span });
        }
    }

    /// A `Map`, `Set`, deque, or heap: a table a copy shares with the value
    /// it was copied from.
    fn shared_table(&self, ty: crate::ty::Ty) -> bool {
        const HANDLE_CONTAINERS: [u32; 7] = [
            u32::MAX - 7,
            u32::MAX - 18,
            u32::MAX - 19,
            u32::MAX - 28,
            u32::MAX - 30,
            u32::MAX - 31,
            u32::MAX - 32,
        ];
        match self.tcx.kind_of(self.peel(ty)) {
            TyKind::HashMap { .. } => true,
            TyKind::Adt { def, .. } => HANDLE_CONTAINERS.contains(&def.local),
            _ => false,
        }
    }

    fn is_vec(&self, ty: crate::ty::Ty) -> bool {
        matches!(
            self.tcx.kind_of(self.peel(ty)),
            TyKind::Vec(_) | TyKind::Slice(_)
        )
    }

    fn is_user_aggregate(&self, ty: crate::ty::Ty) -> bool {
        matches!(
            self.tcx.kind_of(self.peel(ty)),
            TyKind::Adt { .. } | TyKind::Nominal { .. }
        ) && self.copied_capture(ty)
    }

    /// Whether a value of `ty` holds a `Vec` other than through a shared
    /// table, which a callee handed `&mut` to it could grow.
    fn holds_vec(&self, ty: crate::ty::Ty, depth: u8) -> bool {
        if depth > 8 {
            return true;
        }
        let ty = self.peel(ty);
        if self.is_vec(ty) {
            return true;
        }
        if self.shared_table(ty) {
            return false;
        }
        match self.tcx.kind_of(ty) {
            TyKind::Tuple(parts) => parts.iter().any(|p| self.holds_vec(*p, depth + 1)),
            TyKind::Array { elem, .. } => self.holds_vec(*elem, depth + 1),
            TyKind::Adt { def, substs } => self
                .tcx
                .adt_field_tys(*def, substs)
                .is_some_and(|fields| fields.iter().any(|f| self.holds_vec(*f, depth + 1))),
            _ => false,
        }
    }

    fn peel(&self, ty: crate::ty::Ty) -> crate::ty::Ty {
        let mut ty = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(ty) {
            ty = *inner;
        }
        ty
    }

    /// The local binding a place is rooted at, through fields and indices
    /// but not through a dereference, which reaches what a reference names.
    fn place_root(&self, place: &Expr) -> Option<(PlacePath, NodeId, String)> {
        let mut cur = place;
        let mut receivers = Vec::new();
        loop {
            match &cur.kind {
                ExprKind::FieldAccess { receiver, .. } => {
                    receivers.push(receiver.id);
                    cur = receiver;
                }
                ExprKind::Index { base, .. } => {
                    receivers.push(base.id);
                    cur = base;
                }
                ExprKind::Path(path) if path.segments.len() == 1 => {
                    let Some(Resolution::Local(binding)) = self.resolutions.get(cur.id) else {
                        return None;
                    };
                    // The root itself is the captured copy, not a value the
                    // write passes through.
                    receivers.pop();
                    receivers.reverse();
                    let path_info = PlacePath {
                        root: cur.id,
                        receivers,
                    };
                    return Some((path_info, binding, path.segments[0].name.name.clone()));
                }
                _ => return None,
            }
        }
    }

    /// A value a closure captures by copy: a scalar, a `String`, a tuple or
    /// fixed array, or a struct or enum the program declares.
    fn copied_capture(&self, ty: crate::ty::Ty) -> bool {
        match self.tcx.kind_of(ty) {
            TyKind::Bool
            | TyKind::Char
            | TyKind::Int(_)
            | TyKind::Float(_)
            | TyKind::String
            | TyKind::Tuple(_)
            | TyKind::Array { .. } => true,
            TyKind::Adt { def, .. } | TyKind::Nominal { def, .. } => self
                .tcx
                .def_name(*def)
                .and_then(|name| name.rsplit("::").next())
                .is_some_and(|name| self.user_types.contains(name)),
            _ => false,
        }
    }
}

/// A place's root expression and the values between the root and the
/// written place, outermost (nearest the root) first.
struct PlacePath {
    root: NodeId,
    receivers: Vec<NodeId>,
}
