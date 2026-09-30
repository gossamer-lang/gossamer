//! Cursor → AST mapping and indices the LSP request handlers need.
//!
//! Three pieces of state turn a position into a semantic answer:
//!
//! * `LocateResult` - the smallest path / pattern / ident the cursor sits in,
//!   produced by walking the AST once per request.
//! * [`DefinitionIndex`] - `DefId → (Ident, Span, DefKind)` plus a `NodeId →
//!   (Ident, Span)` map for every `let`-bound, `for`-bound, fn-parameter, and
//!   pattern-bound local. Built once per analysis run and reused across
//!   hover / definition / references.
//! * [`PathOccurrence`] - every path expression and type path resolved by
//!   the resolver, recorded with its head segment span. Lets references /
//!   rename find every use of a definition without re-walking the tree.

#![forbid(unsafe_code)]

use gossamer_ast::{
    ArrayExpr, Block, ClosureParam, Expr, ExprKind, FieldPattern, FieldSelector, FnDecl, FnParam,
    Ident, ImplDecl, ImplItem, Item, ItemKind, MatchArm, ModBody, NodeId, PathExpr, Pattern,
    PatternKind, SelectArm, SelectOp, SourceFile, Stmt, StmtKind, StructBody, StructDecl,
    StructExprField, TraitItem, Type, TypeKind, TypePath, Visitor,
};
use gossamer_lex::Span;
use gossamer_resolve::{DefId, DefKind, Resolution, Resolutions};
use std::collections::HashMap;

/// Outcome of locating the cursor in an AST. Each variant carries the data
/// the request handler needs to answer hover / goto-def / references.
#[derive(Debug, Clone)]
pub(crate) enum Locate {
    /// Cursor is on the head segment of a path expression. Carries the
    /// resolved definition (or local binding) the path points to plus the
    /// head segment's span (used for the rename / reference range).
    PathExpr {
        /// `NodeId` of the enclosing `Expr` that contained the path.
        expr_id: NodeId,
        /// Source range of the head segment under the cursor.
        segment_span: Span,
        /// Identifier text under the cursor.
        name: String,
        /// Resolved meaning, if the resolver recorded one.
        resolution: Option<Resolution>,
    },
    /// Cursor is on the head segment of a type path (`fn foo(x: Bar)`).
    TypePath {
        /// `NodeId` of the enclosing `Type` node.
        type_id: NodeId,
        /// Source range of the head segment under the cursor.
        segment_span: Span,
        /// Identifier text under the cursor.
        name: String,
        /// Resolved meaning, if the resolver recorded one.
        resolution: Option<Resolution>,
    },
    /// Cursor is on a binding pattern (`let x = …`, `fn foo(x: T)`,
    /// `for x in …`).
    Binding {
        /// `NodeId` of the binding pattern.
        pattern_id: NodeId,
        /// Source range of the binding name itself (not the whole pattern).
        name_span: Span,
        /// Identifier text.
        name: String,
    },
    /// Cursor is on a struct-pattern field name. Method-call receivers and
    /// regular field accesses fall under this too - the field span anchors
    /// hover and rename.
    Field {
        /// Receiver `NodeId` (Expr or Pattern) the field belongs to.
        /// Reserved for future field-resolution work (looking up the
        /// struct definition to surface the field's declared type).
        #[allow(
            dead_code,
            reason = "owner anchors the type lookup once field-resolution lands"
        )]
        owner_id: NodeId,
        /// Source range of the field name.
        name_span: Span,
        /// Field name text.
        name: String,
        /// The field-access expression itself, whose checked type is the
        /// field's; `None` for a method name or a pattern or literal field.
        access_id: Option<NodeId>,
    },
}

/// Walks `sf`, parsed from `source`, and returns the most-specific cursor
/// target at `offset`.
pub(crate) fn locate(sf: &SourceFile, source: &str, offset: u32) -> Option<Locate> {
    let mut walker = Walker {
        offset,
        best: None,
        source,
    };
    walker.visit_source_file(sf);
    walker.best
}

struct Walker<'src> {
    offset: u32,
    best: Option<Locate>,
    /// The text the tree was parsed from, which places a member name the
    /// tree records only by its spelling.
    source: &'src str,
}

impl Walker<'_> {
    /// The span of the member `name` written after the `.` that follows a
    /// receiver ending at `receiver_end`, within an expression ending at
    /// `end`; whitespace and line breaks may sit either side of the `.`.
    fn member_span(&self, name: &Ident, receiver_end: u32, end: u32, fallback: Span) -> Span {
        let window = self
            .source
            .get(receiver_end as usize..(end as usize).min(self.source.len()))
            .unwrap_or_default();
        let after_dot = window
            .find('.')
            .map(|dot| dot + 1 + (window[dot + 1..].len() - window[dot + 1..].trim_start().len()));
        match after_dot {
            Some(at) if window[at..].starts_with(name.name.as_str()) => {
                let start = receiver_end + u32::try_from(at).unwrap_or(0);
                let len = u32::try_from(name.name.len()).unwrap_or(0);
                Span::new(fallback.file, start, start + len)
            }
            _ => ident_span(name, fallback),
        }
    }

    fn record(&mut self, candidate: Locate, span: Span) {
        if !contains(span, self.offset) {
            return;
        }
        if let Some(current) = &self.best {
            let current_span = locate_span(current);
            // Prefer the tighter (smaller) span - leaf wins over its ancestor.
            if span_width(span) < span_width(current_span) {
                self.best = Some(candidate);
            }
        } else {
            self.best = Some(candidate);
        }
    }

    fn visit_source_file(&mut self, sf: &SourceFile) {
        for item in &sf.items {
            self.visit_item(item);
        }
    }

    fn visit_item(&mut self, item: &Item) {
        match &item.kind {
            ItemKind::Fn(decl) => self.visit_fn(decl),
            ItemKind::Impl(decl) => self.visit_impl(decl),
            ItemKind::Trait(decl) => {
                for trait_item in &decl.items {
                    if let TraitItem::Fn(inner) = trait_item {
                        self.visit_fn(inner);
                    }
                }
            }
            ItemKind::Mod(decl) => {
                if let ModBody::Inline(items) = &decl.body {
                    for nested in items {
                        self.visit_item(nested);
                    }
                }
            }
            ItemKind::Struct(decl) => self.visit_struct(decl),
            ItemKind::Enum(_)
            | ItemKind::TypeAlias(_)
            | ItemKind::Const(_)
            | ItemKind::Static(_)
            | ItemKind::AttrItem(_) => {}
        }
    }

    fn visit_struct(&mut self, decl: &StructDecl) {
        if let StructBody::Named(fields) = &decl.body {
            for field in fields {
                self.visit_type(&field.ty);
            }
        }
    }

    fn visit_impl(&mut self, decl: &ImplDecl) {
        for impl_item in &decl.items {
            if let ImplItem::Fn(inner) = impl_item {
                self.visit_fn(inner);
            }
        }
    }

    fn visit_fn(&mut self, decl: &FnDecl) {
        for param in &decl.params {
            if let FnParam::Typed { pattern, ty, .. } = param {
                self.visit_pattern(pattern);
                self.visit_type(ty);
            }
        }
        if let Some(ret) = &decl.ret {
            self.visit_type(ret);
        }
        if let Some(body) = &decl.body {
            self.visit_expr(body);
        }
    }

    fn visit_type(&mut self, ty: &Type) {
        match &ty.kind {
            TypeKind::Path(path) => self.visit_type_path(path, ty.id, ty.span),
            TypeKind::Ref { inner, .. } | TypeKind::Slice(inner) => self.visit_type(inner),
            TypeKind::Array { elem, .. } => self.visit_type(elem),
            TypeKind::Tuple(parts) => {
                for part in parts {
                    self.visit_type(part);
                }
            }
            TypeKind::Fn { params, ret, .. } => {
                for param in params {
                    self.visit_type(param);
                }
                if let Some(ret) = ret {
                    self.visit_type(ret);
                }
            }
            TypeKind::Infer | TypeKind::Never | TypeKind::Unit => {}
        }
    }

    fn visit_type_path(&mut self, path: &TypePath, type_id: NodeId, fallback: Span) {
        let head_span = path
            .segments
            .first()
            .map_or(fallback, |seg| ident_span(&seg.name, fallback));
        if let Some(seg) = path.segments.first() {
            self.record(
                Locate::TypePath {
                    type_id,
                    segment_span: head_span,
                    name: seg.name.name.clone(),
                    resolution: None,
                },
                head_span,
            );
        }
    }

    fn visit_pattern(&mut self, pattern: &Pattern) {
        match &pattern.kind {
            PatternKind::Ident { name, .. } => {
                let name_span = ident_span(name, pattern.span);
                self.record(
                    Locate::Binding {
                        pattern_id: pattern.id,
                        name_span,
                        name: name.name.clone(),
                    },
                    name_span,
                );
            }
            PatternKind::Tuple(parts) => {
                for part in parts {
                    self.visit_pattern(part);
                }
            }
            PatternKind::Struct { fields, .. } => {
                for field in fields {
                    self.visit_field_pattern(pattern.id, field, pattern.span);
                }
            }
            PatternKind::TupleStruct { elems, .. } => {
                for elem in elems {
                    self.visit_pattern(elem);
                }
            }
            PatternKind::Or(alts) => {
                for alt in alts {
                    self.visit_pattern(alt);
                }
            }
            PatternKind::Ref { inner, .. } => self.visit_pattern(inner),
            PatternKind::Slice {
                prefix,
                rest,
                suffix,
            } => {
                for part in prefix {
                    self.visit_pattern(part);
                }
                if let Some(rest) = rest {
                    self.visit_pattern(rest);
                }
                for part in suffix {
                    self.visit_pattern(part);
                }
            }
            PatternKind::Wildcard
            | PatternKind::Literal(_)
            | PatternKind::Path(_)
            | PatternKind::Range { .. }
            | PatternKind::Rest
            | PatternKind::Error => {}
        }
    }

    fn visit_field_pattern(&mut self, owner_id: NodeId, field: &FieldPattern, fallback: Span) {
        let span = ident_span(&field.name, fallback);
        self.record(
            Locate::Field {
                owner_id,
                name_span: span,
                name: field.name.name.clone(),
                access_id: None,
            },
            span,
        );
        if let Some(sub) = &field.pattern {
            self.visit_pattern(sub);
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "flat-shape dispatch / lowering - splitting hides the per-arm intent"
    )]
    fn visit_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Block(block) | ExprKind::Unsafe(block) => self.visit_block(block),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.visit_expr(condition);
                self.visit_expr(then_branch);
                if let Some(else_branch) = else_branch {
                    self.visit_expr(else_branch);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.visit_expr(scrutinee);
                for arm in arms {
                    self.visit_match_arm(arm);
                }
            }
            ExprKind::Loop { body, .. } | ExprKind::While { body, .. } => self.visit_expr(body),
            ExprKind::For {
                pattern,
                iter,
                body,
                ..
            } => {
                self.visit_pattern(pattern);
                self.visit_expr(iter);
                self.visit_expr(body);
            }
            ExprKind::Closure { params, body, .. } => self.visit_closure(params, body),
            ExprKind::Path(path) => self.visit_path_expr(path, expr),
            ExprKind::Call { callee, args } => {
                self.visit_expr(callee);
                for arg in args {
                    self.visit_expr(arg);
                }
            }
            ExprKind::MethodCall {
                receiver,
                name,
                args,
                ..
            } => {
                self.visit_expr(receiver);
                let span = self.member_span(name, receiver.span.end, expr.span.end, expr.span);
                self.record(
                    Locate::Field {
                        owner_id: receiver.id,
                        name_span: span,
                        name: name.name.clone(),
                        access_id: None,
                    },
                    span,
                );
                for arg in args {
                    self.visit_expr(arg);
                }
            }
            ExprKind::FieldAccess { receiver, field } => {
                self.visit_expr(receiver);
                if let FieldSelector::Named(name) = field {
                    let span = self.member_span(name, receiver.span.end, expr.span.end, expr.span);
                    self.record(
                        Locate::Field {
                            owner_id: receiver.id,
                            name_span: span,
                            name: name.name.clone(),
                            access_id: Some(expr.id),
                        },
                        span,
                    );
                }
            }
            ExprKind::Index { base, index } => {
                self.visit_expr(base);
                self.visit_expr(index);
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                self.visit_expr(lhs);
                self.visit_expr(rhs);
            }
            ExprKind::Unary { operand, .. } => self.visit_expr(operand),
            ExprKind::Cast { value, ty } => {
                self.visit_expr(value);
                self.visit_type(ty);
            }
            ExprKind::Try(value) => self.visit_expr(value),
            ExprKind::Tuple(parts) | ExprKind::MapLiteral(parts) | ExprKind::SetLiteral(parts) => {
                for part in parts {
                    self.visit_expr(part);
                }
            }
            ExprKind::Struct {
                path, fields, base, ..
            } => {
                self.visit_struct_path(path, expr);
                for field in fields {
                    self.visit_struct_expr_field(expr.id, field, expr.span);
                }
                if let Some(base) = base {
                    self.visit_expr(base);
                }
            }
            ExprKind::Array(arr) | ExprKind::FixedArray(arr) => match arr {
                ArrayExpr::List(elems) => {
                    for elem in elems {
                        self.visit_expr(elem);
                    }
                }
                ArrayExpr::Repeat { value, count } => {
                    self.visit_expr(value);
                    self.visit_expr(count);
                }
            },
            ExprKind::Assign { place, value, .. } => {
                self.visit_expr(place);
                self.visit_expr(value);
            }
            ExprKind::Range { start, end, .. } => {
                if let Some(start) = start {
                    self.visit_expr(start);
                }
                if let Some(end) = end {
                    self.visit_expr(end);
                }
            }
            ExprKind::Select(arms) => {
                for arm in arms {
                    self.visit_select_arm(arm);
                }
            }
            ExprKind::Return(inner) => {
                if let Some(inner) = inner {
                    self.visit_expr(inner);
                }
            }
            ExprKind::Break { value, .. } => {
                if let Some(value) = value {
                    self.visit_expr(value);
                }
            }
            ExprKind::Literal(_) | ExprKind::Continue { .. } | ExprKind::Error => {}
        }
    }

    fn visit_block(&mut self, block: &Block) {
        for stmt in &block.stmts {
            self.visit_stmt(stmt);
        }
        if let Some(tail) = &block.tail {
            self.visit_expr(tail);
        }
    }

    fn visit_stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Let { pattern, ty, init } => {
                self.visit_pattern(pattern);
                if let Some(ty) = ty {
                    self.visit_type(ty);
                }
                if let Some(init) = init {
                    self.visit_expr(init);
                }
            }
            StmtKind::Expr { expr, .. } | StmtKind::Defer(expr) => {
                self.visit_expr(expr);
            }
            StmtKind::Item(item) => self.visit_item(item),
        }
    }

    fn visit_match_arm(&mut self, arm: &MatchArm) {
        self.visit_pattern(&arm.pattern);
        if let Some(guard) = &arm.guard {
            self.visit_expr(guard);
        }
        self.visit_expr(&arm.body);
    }

    fn visit_select_arm(&mut self, arm: &SelectArm) {
        match &arm.op {
            SelectOp::Recv { pattern, channel } => {
                self.visit_pattern(pattern);
                self.visit_expr(channel);
            }
            SelectOp::Send { channel, value } => {
                self.visit_expr(channel);
                self.visit_expr(value);
            }
            SelectOp::Default => {}
        }
        self.visit_expr(&arm.body);
    }

    fn visit_closure(&mut self, params: &[ClosureParam], body: &Expr) {
        for param in params {
            self.visit_pattern(&param.pattern);
            if let Some(ty) = &param.ty {
                self.visit_type(ty);
            }
        }
        self.visit_expr(body);
    }

    fn visit_path_expr(&mut self, path: &PathExpr, expr: &Expr) {
        if let Some(seg) = path.segments.first() {
            let head_span = ident_span(&seg.name, expr.span);
            self.record(
                Locate::PathExpr {
                    expr_id: expr.id,
                    segment_span: head_span,
                    name: seg.name.name.clone(),
                    resolution: None,
                },
                head_span,
            );
        }
    }

    fn visit_struct_path(&mut self, path: &PathExpr, expr: &Expr) {
        if let Some(seg) = path.segments.first() {
            let head_span = ident_span(&seg.name, expr.span);
            self.record(
                Locate::PathExpr {
                    expr_id: expr.id,
                    segment_span: head_span,
                    name: seg.name.name.clone(),
                    resolution: None,
                },
                head_span,
            );
        }
    }

    fn visit_struct_expr_field(
        &mut self,
        owner_id: NodeId,
        field: &StructExprField,
        fallback: Span,
    ) {
        let span = ident_span(&field.name, fallback);
        self.record(
            Locate::Field {
                owner_id,
                name_span: span,
                name: field.name.name.clone(),
                access_id: None,
            },
            span,
        );
        if let Some(value) = &field.value {
            self.visit_expr(value);
        }
    }
}

fn locate_span(loc: &Locate) -> Span {
    match loc {
        Locate::PathExpr { segment_span, .. }
        | Locate::TypePath { segment_span, .. }
        | Locate::Binding {
            name_span: segment_span,
            ..
        }
        | Locate::Field {
            name_span: segment_span,
            ..
        } => *segment_span,
    }
}

fn span_width(span: Span) -> u32 {
    span.end.saturating_sub(span.start)
}

fn contains(span: Span, offset: u32) -> bool {
    span.start <= offset && offset <= span.end
}

/// Best-effort span for `name` inside `fallback` - the parser does not
/// stash spans on `Ident`, but the head identifier of a path always sits
/// at the path's `start`. Accurate for top-level paths; for sub-segments
/// we fall back to the whole-expression span.
fn ident_span(name: &Ident, fallback: Span) -> Span {
    let len = name.name.len() as u32;
    Span::new(fallback.file, fallback.start, fallback.start + len)
}

/// One occurrence of a path / type-path inside the document, with the
/// span of its head segment and the resolution the resolver recorded.
#[derive(Debug, Clone)]
pub(crate) struct PathOccurrence {
    /// Source range of the head segment.
    pub span: Span,
    /// Head identifier text. Used for fallback whole-word matching
    /// when the resolver couldn't tag the occurrence.
    #[allow(
        dead_code,
        reason = "consumed by request-handler fallback when the resolver-tagged path is empty"
    )]
    pub name: String,
    /// Resolution recorded for this occurrence, if any.
    pub resolution: Option<Resolution>,
}

/// One definition declared in the document.
#[derive(Debug, Clone)]
pub(crate) struct DefinitionInfo {
    /// Source span covering the whole item. Reserved for code-action /
    /// peek-definition surfaces that want the whole declaration body.
    #[allow(
        dead_code,
        reason = "exposed for code-action / peek-definition surfaces"
    )]
    pub item_span: Span,
    /// Source span covering just the declaring identifier.
    pub name_span: Span,
    /// Definition name.
    pub name: String,
    /// Kind of definition.
    pub kind: DefKind,
    /// Pretty-printed signature for the hover popup (e.g. `fn foo(x: i64)
    /// -> bool`). Empty for kinds that don't have a useful single-line
    /// rendering (e.g. modules).
    pub signature: String,
    /// Doc comment - the leading `///` block right above the declaration,
    /// joined into a single string (one `\n` per line, no leading slashes).
    pub docs: String,
}

/// One local binding inside the document.
#[derive(Debug, Clone)]
pub(crate) struct BindingInfo {
    /// Source span of the binding pattern as a whole. Reserved for
    /// future hover surfaces that want to outline the entire `(a, b)`
    /// or `Some(x)` shape rather than just the name.
    #[allow(
        dead_code,
        reason = "exposed for hover surfaces that outline the full binding pattern"
    )]
    pub pattern_span: Span,
    /// Span of just the bound identifier.
    pub name_span: Span,
    /// Identifier text.
    pub name: String,
    /// `true` for `let mut x` / `&mut x` / `for mut x in ...`.
    pub mutable: bool,
}

/// All the indices a request handler needs in one place.
#[derive(Debug, Default, Clone)]
pub(crate) struct DefinitionIndex {
    by_def: HashMap<DefId, DefinitionInfo>,
    by_local: HashMap<NodeId, BindingInfo>,
    occurrences: Vec<PathOccurrence>,
}

impl DefinitionIndex {
    /// Builds the index by walking `sf` once. The provided source text is
    /// only used to extract doc-comment lines from positions just above
    /// each item's span.
    pub(crate) fn build(sf: &SourceFile, source: &str, resolutions: &Resolutions) -> Self {
        let mut index = Self::default();
        for item in &sf.items {
            index.visit_item(item, source, resolutions);
        }
        let mut occ_walker = OccurrenceWalker {
            resolutions,
            out: &mut index.occurrences,
        };
        occ_walker.visit_source_file(sf);
        index
    }

    /// Looks up the definition info for a `DefId`.
    pub(crate) fn def(&self, id: DefId) -> Option<&DefinitionInfo> {
        self.by_def.get(&id)
    }

    /// Looks up the local binding info for a `NodeId`.
    pub(crate) fn local(&self, id: NodeId) -> Option<&BindingInfo> {
        self.by_local.get(&id)
    }

    /// Returns every recorded path occurrence in source order.
    pub(crate) fn occurrences(&self) -> &[PathOccurrence] {
        &self.occurrences
    }

    /// Iterates every `(DefId, DefinitionInfo)` pair the document
    /// declared. Used by the server to seed completion / signature-help
    /// without exposing the underlying `HashMap`.
    pub(crate) fn def_iter(&self) -> impl Iterator<Item = (DefId, &DefinitionInfo)> {
        self.by_def.iter().map(|(k, v)| (*k, v))
    }

    /// Iterates every `(NodeId, BindingInfo)` pair, in arbitrary order.
    pub(crate) fn local_iter(&self) -> impl Iterator<Item = (NodeId, &BindingInfo)> {
        self.by_local.iter().map(|(k, v)| (*k, v))
    }

    #[allow(
        clippy::too_many_lines,
        reason = "flat-shape dispatch / lowering - splitting hides the per-arm intent"
    )]
    fn visit_item(&mut self, item: &Item, source: &str, resolutions: &Resolutions) {
        let docs = doc_block_above(source, item.span.start);
        match &item.kind {
            ItemKind::Fn(decl) => {
                let signature = render_fn_signature(decl);
                self.record_def(
                    item,
                    &decl.name,
                    DefKind::Fn,
                    signature,
                    docs.clone(),
                    resolutions,
                );
                self.collect_fn_locals(decl);
            }
            ItemKind::Struct(decl) => {
                let signature = format!("struct {}", decl.name.name);
                self.record_def(
                    item,
                    &decl.name,
                    DefKind::Struct,
                    signature,
                    docs.clone(),
                    resolutions,
                );
            }
            ItemKind::Enum(decl) => {
                let signature = format!("enum {}", decl.name.name);
                self.record_def(
                    item,
                    &decl.name,
                    DefKind::Enum,
                    signature,
                    docs.clone(),
                    resolutions,
                );
            }
            ItemKind::Trait(decl) => {
                let signature = format!("trait {}", decl.name.name);
                self.record_def(
                    item,
                    &decl.name,
                    DefKind::Trait,
                    signature,
                    docs.clone(),
                    resolutions,
                );
                for trait_item in &decl.items {
                    if let TraitItem::Fn(inner) = trait_item {
                        self.collect_fn_locals(inner);
                    }
                }
            }
            ItemKind::Impl(decl) => {
                for impl_item in &decl.items {
                    if let ImplItem::Fn(inner) = impl_item {
                        self.collect_fn_locals(inner);
                    }
                }
            }
            ItemKind::TypeAlias(decl) => {
                let signature = format!("type {}", decl.name.name);
                self.record_def(
                    item,
                    &decl.name,
                    DefKind::TypeAlias,
                    signature,
                    docs.clone(),
                    resolutions,
                );
            }
            ItemKind::Const(decl) => {
                let signature = format!("const {}", decl.name.name);
                self.record_def(
                    item,
                    &decl.name,
                    DefKind::Const,
                    signature,
                    docs.clone(),
                    resolutions,
                );
            }
            ItemKind::Static(decl) => {
                let signature = format!("static {}", decl.name.name);
                self.record_def(
                    item,
                    &decl.name,
                    DefKind::Static,
                    signature,
                    docs.clone(),
                    resolutions,
                );
            }
            ItemKind::Mod(decl) => {
                let signature = format!("mod {}", decl.name.name);
                self.record_def(
                    item,
                    &decl.name,
                    DefKind::Mod,
                    signature,
                    docs.clone(),
                    resolutions,
                );
                if let ModBody::Inline(items) = &decl.body {
                    for nested in items {
                        self.visit_item(nested, source, resolutions);
                    }
                }
            }
            ItemKind::AttrItem(_) => {}
        }
    }

    fn record_def(
        &mut self,
        item: &Item,
        name: &Ident,
        kind: DefKind,
        signature: String,
        docs: String,
        resolutions: &Resolutions,
    ) {
        let Some(def) = resolutions.definition_of(item.id) else {
            return;
        };
        let name_span = ident_span(name, item.span);
        self.by_def.insert(
            def,
            DefinitionInfo {
                item_span: item.span,
                name_span,
                name: name.name.clone(),
                kind,
                signature,
                docs,
            },
        );
    }

    fn collect_fn_locals(&mut self, decl: &FnDecl) {
        for param in &decl.params {
            if let FnParam::Typed { pattern, .. } = param {
                self.collect_pattern_locals(pattern);
            }
        }
        if let Some(body) = &decl.body {
            self.collect_expr_locals(body);
        }
    }

    fn collect_pattern_locals(&mut self, pattern: &Pattern) {
        match &pattern.kind {
            PatternKind::Ident {
                mutability, name, ..
            } => {
                let name_span = ident_span(name, pattern.span);
                self.by_local.insert(
                    pattern.id,
                    BindingInfo {
                        pattern_span: pattern.span,
                        name_span,
                        name: name.name.clone(),
                        mutable: matches!(mutability, gossamer_ast::Mutability::Mutable),
                    },
                );
            }
            PatternKind::Tuple(parts) | PatternKind::TupleStruct { elems: parts, .. } => {
                for part in parts {
                    self.collect_pattern_locals(part);
                }
            }
            PatternKind::Struct { fields, .. } => {
                for field in fields {
                    if let Some(sub) = &field.pattern {
                        self.collect_pattern_locals(sub);
                    }
                }
            }
            PatternKind::Or(alts) => {
                for alt in alts {
                    self.collect_pattern_locals(alt);
                }
            }
            PatternKind::Ref { inner, .. } => self.collect_pattern_locals(inner),
            PatternKind::Slice {
                prefix,
                rest,
                suffix,
            } => {
                for part in prefix {
                    self.collect_pattern_locals(part);
                }
                if let Some(rest) = rest {
                    self.collect_pattern_locals(rest);
                }
                for part in suffix {
                    self.collect_pattern_locals(part);
                }
            }
            PatternKind::Wildcard
            | PatternKind::Literal(_)
            | PatternKind::Path(_)
            | PatternKind::Range { .. }
            | PatternKind::Rest
            | PatternKind::Error => {}
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "flat-shape dispatch / lowering - splitting hides the per-arm intent"
    )]
    fn collect_expr_locals(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Block(block) | ExprKind::Unsafe(block) => self.collect_block_locals(block),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.collect_expr_locals(condition);
                self.collect_expr_locals(then_branch);
                if let Some(else_branch) = else_branch {
                    self.collect_expr_locals(else_branch);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.collect_expr_locals(scrutinee);
                for arm in arms {
                    self.collect_pattern_locals(&arm.pattern);
                    if let Some(guard) = &arm.guard {
                        self.collect_expr_locals(guard);
                    }
                    self.collect_expr_locals(&arm.body);
                }
            }
            ExprKind::Loop { body, .. } | ExprKind::While { body, .. } => {
                self.collect_expr_locals(body);
            }
            ExprKind::For {
                pattern,
                iter,
                body,
                ..
            } => {
                self.collect_pattern_locals(pattern);
                self.collect_expr_locals(iter);
                self.collect_expr_locals(body);
            }
            ExprKind::Closure { params, body, .. } => {
                for param in params {
                    self.collect_pattern_locals(&param.pattern);
                }
                self.collect_expr_locals(body);
            }
            ExprKind::Call { callee, args } => {
                self.collect_expr_locals(callee);
                for arg in args {
                    self.collect_expr_locals(arg);
                }
            }
            ExprKind::MethodCall { receiver, args, .. } => {
                self.collect_expr_locals(receiver);
                for arg in args {
                    self.collect_expr_locals(arg);
                }
            }
            ExprKind::FieldAccess { receiver, .. } => self.collect_expr_locals(receiver),
            ExprKind::Index { base, index } => {
                self.collect_expr_locals(base);
                self.collect_expr_locals(index);
            }
            ExprKind::Binary { lhs, rhs, .. }
            | ExprKind::Assign {
                place: lhs,
                value: rhs,
                ..
            } => {
                self.collect_expr_locals(lhs);
                self.collect_expr_locals(rhs);
            }
            ExprKind::Unary { operand, .. } => self.collect_expr_locals(operand),
            ExprKind::Cast { value, .. } | ExprKind::Try(value) => {
                self.collect_expr_locals(value);
            }
            ExprKind::Tuple(parts) | ExprKind::MapLiteral(parts) | ExprKind::SetLiteral(parts) => {
                for part in parts {
                    self.collect_expr_locals(part);
                }
            }
            ExprKind::Struct { fields, base, .. } => {
                for field in fields {
                    if let Some(value) = &field.value {
                        self.collect_expr_locals(value);
                    }
                }
                if let Some(base) = base {
                    self.collect_expr_locals(base);
                }
            }
            ExprKind::Array(arr) | ExprKind::FixedArray(arr) => match arr {
                ArrayExpr::List(elems) => {
                    for elem in elems {
                        self.collect_expr_locals(elem);
                    }
                }
                ArrayExpr::Repeat { value, count } => {
                    self.collect_expr_locals(value);
                    self.collect_expr_locals(count);
                }
            },
            ExprKind::Range { start, end, .. } => {
                if let Some(start) = start {
                    self.collect_expr_locals(start);
                }
                if let Some(end) = end {
                    self.collect_expr_locals(end);
                }
            }
            ExprKind::Select(arms) => {
                for arm in arms {
                    match &arm.op {
                        SelectOp::Recv { pattern, channel } => {
                            self.collect_pattern_locals(pattern);
                            self.collect_expr_locals(channel);
                        }
                        SelectOp::Send { channel, value } => {
                            self.collect_expr_locals(channel);
                            self.collect_expr_locals(value);
                        }
                        SelectOp::Default => {}
                    }
                    self.collect_expr_locals(&arm.body);
                }
            }
            ExprKind::Return(inner) => {
                if let Some(inner) = inner {
                    self.collect_expr_locals(inner);
                }
            }
            ExprKind::Break { value, .. } => {
                if let Some(value) = value {
                    self.collect_expr_locals(value);
                }
            }
            ExprKind::Path(_)
            | ExprKind::Literal(_)
            | ExprKind::Continue { .. }
            | ExprKind::Error => {}
        }
    }

    fn collect_block_locals(&mut self, block: &Block) {
        for stmt in &block.stmts {
            match &stmt.kind {
                StmtKind::Let { pattern, init, .. } => {
                    self.collect_pattern_locals(pattern);
                    if let Some(init) = init {
                        self.collect_expr_locals(init);
                    }
                }
                StmtKind::Expr { expr, .. } | StmtKind::Defer(expr) => {
                    self.collect_expr_locals(expr);
                }
                StmtKind::Item(_) => {}
            }
        }
        if let Some(tail) = &block.tail {
            self.collect_expr_locals(tail);
        }
    }
}

struct OccurrenceWalker<'a> {
    resolutions: &'a Resolutions,
    out: &'a mut Vec<PathOccurrence>,
}

impl Visitor for OccurrenceWalker<'_> {
    fn visit_expr(&mut self, expr: &Expr) {
        if let ExprKind::Path(path) = &expr.kind {
            if let Some(seg) = path.segments.first() {
                self.out.push(PathOccurrence {
                    span: ident_span(&seg.name, expr.span),
                    name: seg.name.name.clone(),
                    resolution: self.resolutions.get(expr.id),
                });
            }
        }
        gossamer_ast::visitor::walk_expr(self, expr);
    }

    fn visit_type(&mut self, ty: &Type) {
        if let TypeKind::Path(path) = &ty.kind {
            if let Some(seg) = path.segments.first() {
                self.out.push(PathOccurrence {
                    span: ident_span(&seg.name, ty.span),
                    name: seg.name.name.clone(),
                    resolution: self.resolutions.get(ty.id),
                });
            }
        }
        gossamer_ast::visitor::walk_type(self, ty);
    }
}

/// Lifts the resolution recorded for a path / type at `expr_id` /
/// `type_id` from the side table onto a `Locate` value. The walker can't
/// see the resolutions while it walks (the borrow would conflict with the
/// AST lifetime). Callers run this after locating to upgrade the
/// `resolution: None` placeholder.
pub(crate) fn attach_resolution(loc: &mut Locate, resolutions: &Resolutions) {
    match loc {
        Locate::PathExpr {
            expr_id,
            resolution,
            ..
        } => {
            *resolution = resolutions.get(*expr_id);
        }
        Locate::TypePath {
            type_id,
            resolution,
            ..
        } => {
            *resolution = resolutions.get(*type_id);
        }
        Locate::Binding { .. } | Locate::Field { .. } => {}
    }
}

/// Renders a function signature as a single line: `fn name(params) -> ret`.
fn render_fn_signature(decl: &FnDecl) -> String {
    let mut out = String::new();
    if decl.is_unsafe {
        out.push_str("unsafe ");
    }
    out.push_str("fn ");
    out.push_str(&decl.name.name);
    out.push('(');
    let mut first = true;
    for param in &decl.params {
        if !first {
            out.push_str(", ");
        }
        first = false;
        match param {
            FnParam::Receiver(receiver) => out.push_str(receiver.as_str()),
            FnParam::Typed { pattern, ty, .. } => {
                out.push_str(&pattern_signature_name(pattern));
                out.push_str(": ");
                out.push_str(&render_type(ty));
            }
        }
    }
    out.push(')');
    if let Some(ret) = &decl.ret {
        out.push_str(" -> ");
        out.push_str(&render_type(ret));
    }
    out
}

fn pattern_signature_name(pattern: &Pattern) -> String {
    match &pattern.kind {
        PatternKind::Ident { name, .. } => name.name.clone(),
        PatternKind::Wildcard => "_".to_string(),
        _ => "_".to_string(),
    }
}

/// Compact textual rendering of a `Type` for hover labels. Routes
/// through the AST pretty-printer so the spelling matches what the
/// formatter would emit.
fn render_type(ty: &Type) -> String {
    let mut printer = gossamer_ast::Printer::new();
    printer.print_type(ty);
    printer.finish()
}

/// Extracts the contiguous block of `///` lines that ends right above
/// `start`. Joins lines with newlines, strips the leading `///` and one
/// optional space. Returns an empty string if the lines above aren't
/// doc comments.
fn doc_block_above(source: &str, start: u32) -> String {
    let bytes = source.as_bytes();
    let cap = std::cmp::min(start as usize, bytes.len());
    let prefix = &source[..cap];
    let mut lines: Vec<&str> = prefix.lines().collect();
    while let Some(last) = lines.last() {
        if last.trim().is_empty() {
            lines.pop();
        } else {
            break;
        }
    }
    let mut docs: Vec<String> = Vec::new();
    while let Some(last) = lines.last() {
        let trimmed = last.trim_start();
        if let Some(rest) = trimmed.strip_prefix("///") {
            docs.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
            lines.pop();
        } else {
            break;
        }
    }
    docs.reverse();
    docs.join("\n")
}
