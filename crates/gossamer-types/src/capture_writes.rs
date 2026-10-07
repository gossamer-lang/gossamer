//! Writes a spawned closure makes to the bindings it captures.
//!
//! A closure names the bindings it captures, so its writes reach them. A
//! spawned closure is the exception: it runs on another goroutine, so it
//! takes a snapshot of each capture at the spawn, and a write to one changes
//! only the goroutine's snapshot. Such a write is refused where it is
//! written. A synchronisation handle (a `Mutex`, a channel end, a
//! `sync::Shared`, an atomic) is shared by every snapshot of it, so calling
//! it is not a write to a snapshot.

#![forbid(unsafe_code)]

use std::collections::HashSet;

use gossamer_ast::visitor::{Visitor, walk_expr, walk_item, walk_pattern};
use gossamer_ast::{Expr, ExprKind, Item, NodeId, Pattern, PatternKind, SourceFile, UnaryOp};
use gossamer_lex::Span;
use gossamer_resolve::{Resolution, Resolutions};

use crate::context::TyCtxt;
use crate::table::TypeTable;
use crate::ty::TyKind;

/// A write a spawned closure makes to its snapshot of a captured binding.
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
            format!("this write changes the goroutine's own snapshot of `{name}`"),
        )
        .with_primary(
            location,
            format!("a spawned closure takes a snapshot of `{name}` at the spawn"),
        )
        .with_help(
            "return the value and read it with `join()`, send it on a channel, or share it through a `sync::Shared`",
        )
    }
}

/// Checks every spawned closure in `sf` for writes to the bindings it
/// captures.
#[must_use]
pub fn check_capture_writes(
    sf: &SourceFile,
    resolutions: &Resolutions,
    table: &TypeTable,
    tcx: &TyCtxt,
) -> Vec<CaptureWriteDiagnostic> {
    let mut walker = Walker {
        resolutions,
        table,
        tcx,
        frames: Vec::new(),
        spawned: HashSet::new(),
        out: Vec::new(),
    };
    walker.visit_source_file(sf);
    walker.out
}

/// One closure being walked: the bindings it declares, and whether it is
/// the closure a `spawn` runs.
struct Frame {
    declared: HashSet<NodeId>,
    spawned: bool,
}

struct Walker<'a> {
    resolutions: &'a Resolutions,
    table: &'a TypeTable,
    tcx: &'a TyCtxt,
    frames: Vec<Frame>,
    spawned: HashSet<NodeId>,
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
                    spawned: self.spawned.contains(&expr.id),
                });
                walk_expr(self, expr);
                self.frames.pop();
                return;
            }
            // The prelude `spawn`, not a module's own (`exec::spawn`).
            ExprKind::Call { callee, args } => {
                if let ExprKind::Path(path) = &callee.kind
                    && let [seg] = path.segments.as_slice()
                    && seg.name.name == "spawn"
                    && let Some(first) = args.first()
                {
                    self.spawned.insert(first.id);
                }
            }
            ExprKind::Assign { place, .. } => self.check_write(place, false),
            ExprKind::MethodCall { receiver, name, .. }
                if crate::is_mutating_method_name(name.name.as_str()) =>
            {
                self.check_write(receiver, true);
            }
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
        let Some((binding, name)) = self.place_root(place) else {
            return;
        };
        if !self.captured_by_spawn(binding) {
            return;
        }
        if through_value && !self.table.get(place.id).is_some_and(|ty| self.is_value(ty)) {
            return;
        }
        self.out.push(CaptureWriteDiagnostic {
            name,
            span: place.span,
        });
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

    /// Whether a value of `ty` is data a snapshot copies, rather than a
    /// handle every snapshot shares.
    fn is_value(&self, ty: crate::ty::Ty) -> bool {
        let mut ty = ty;
        while let TyKind::Ref { inner, .. } = self.tcx.kind_of(ty) {
            ty = *inner;
        }
        match self.tcx.kind_of(ty) {
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

    /// The local binding a place is rooted at, through fields and indices
    /// but not through a dereference, which reaches what a reference names.
    fn place_root(&self, place: &Expr) -> Option<(NodeId, String)> {
        let mut cur = place;
        loop {
            match &cur.kind {
                ExprKind::FieldAccess { receiver, .. } => cur = receiver,
                ExprKind::Index { base, .. } => cur = base,
                ExprKind::Path(path) if path.segments.len() == 1 => {
                    let Some(Resolution::Local(binding)) = self.resolutions.get(cur.id) else {
                        return None;
                    };
                    return Some((binding, path.segments[0].name.name.clone()));
                }
                _ => return None,
            }
        }
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
