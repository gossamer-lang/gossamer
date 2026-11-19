//! Methods and fields reached through a receiver: `s.run()`, `s.limit`.
//!
//! The resolver leaves a member name to the type checker, which picks it by
//! the receiver's type, so navigation reads that type from the checked table
//! and finds the member among the type's declaration and its `impl` blocks,
//! the blocks a type's body stands for included.

use gossamer_ast::visitor::{Visitor, walk_expr};
use gossamer_ast::{
    Expr, ExprKind, FieldSelector, ImplItem, Item, ItemKind, ModBody, NodeId, StructBody, TypeKind,
};
use gossamer_lex::Span;
use gossamer_types::TyKind;

use crate::session::DocumentAnalysis;

/// A method or field a receiver reaches.
pub(crate) struct Member {
    /// The span of the member's name where it is declared.
    pub(crate) name_span: Span,
    /// Its declaration as written: `fn run(&mut self, n: i64) -> u32`, or
    /// `limit: i64`.
    pub(crate) signature: String,
}

/// The name of the type a receiver expression has, when it is a type the
/// program declares.
pub(crate) fn receiver_type(doc: &DocumentAnalysis, receiver: NodeId) -> Option<String> {
    let mut ty = doc.types.get(receiver)?;
    loop {
        match doc.tcx.kind(ty)? {
            TyKind::Ref { inner, .. } => ty = *inner,
            TyKind::Adt { def, .. } => {
                let name = doc.tcx.def_name(*def)?;
                return Some(name.rsplit("::").next().unwrap_or(name).to_string());
            }
            _ => return None,
        }
    }
}

/// The method or field `member` of the type named `ty`.
pub(crate) fn member_of(doc: &DocumentAnalysis, ty: &str, member: &str) -> Option<Member> {
    find_member(&doc.sf.items, doc.source(), ty, member)
}

fn find_member(items: &[Item], source: &str, ty: &str, member: &str) -> Option<Member> {
    for item in items {
        match &item.kind {
            ItemKind::Impl(decl) if impl_owner(&decl.self_ty).as_deref() == Some(ty) => {
                for inner in &decl.items {
                    if let ImplItem::Fn(f) = inner
                        && f.name.name == member
                    {
                        let header = source
                            .get(f.span.start as usize..f.span.end as usize)
                            .unwrap_or_default();
                        let at = header.find(&format!("fn {member}"))?;
                        let start = f.span.start + u32::try_from(at + 3).ok()?;
                        let signature = header[at..]
                            .split('{')
                            .next()
                            .unwrap_or_default()
                            .trim()
                            .to_string();
                        return Some(Member {
                            name_span: Span::new(
                                f.span.file,
                                start,
                                start + u32::try_from(member.len()).ok()?,
                            ),
                            signature,
                        });
                    }
                }
            }
            ItemKind::Struct(decl) if decl.name.name == ty => {
                if let StructBody::Named(fields) = &decl.body
                    && let Some(field) = fields.iter().find(|f| f.name.name == member)
                {
                    return field_member(source, &field.name.name, field.ty.span);
                }
            }
            ItemKind::Mod(decl) => {
                if let ModBody::Inline(inner) = &decl.body
                    && let Some(found) = find_member(inner, source, ty, member)
                {
                    return Some(found);
                }
            }
            _ => {}
        }
    }
    None
}

/// A field declared `name: T` whose type sits at `ty_span`.
fn field_member(source: &str, name: &str, ty_span: Span) -> Option<Member> {
    let before = source.get(..ty_span.start as usize)?;
    let colon = before.trim_end().strip_suffix(':')?;
    let name_end = colon.trim_end().len();
    let name_start = name_end.checked_sub(name.len())?;
    if colon.get(name_start..name_end) != Some(name) {
        return None;
    }
    let line_end = source[ty_span.start as usize..]
        .find('\n')
        .map_or(source.len(), |i| ty_span.start as usize + i);
    Some(Member {
        name_span: Span::new(
            ty_span.file,
            u32::try_from(name_start).ok()?,
            u32::try_from(name_end).ok()?,
        ),
        signature: source[name_start..line_end]
            .trim()
            .trim_end_matches(',')
            .to_string(),
    })
}

/// The type an `impl` block is for, by its last path segment.
fn impl_owner(ty: &gossamer_ast::Type) -> Option<String> {
    match &ty.kind {
        TypeKind::Path(path) => path.segments.last().map(|s| s.name.name.clone()),
        _ => None,
    }
}

/// The declaration a cursor at `offset` sits on, when it is the name of a
/// field or a method: the type it belongs to and the member's name.
pub(crate) fn declared_member_at(doc: &DocumentAnalysis, offset: u32) -> Option<(String, String)> {
    let word = doc.word_at(offset)?.to_string();
    declared_in(&doc.sf.items, doc.source(), offset, &word)
}

fn declared_in(items: &[Item], source: &str, offset: u32, word: &str) -> Option<(String, String)> {
    for item in items {
        if !(item.span.start <= offset && offset <= item.span.end) {
            continue;
        }
        match &item.kind {
            ItemKind::Struct(decl) => {
                let found = find_member(std::slice::from_ref(item), source, &decl.name.name, word)?;
                if found.name_span.start <= offset && offset <= found.name_span.end {
                    return Some((decl.name.name.clone(), word.to_string()));
                }
            }
            ItemKind::Impl(decl) => {
                let owner = impl_owner(&decl.self_ty)?;
                let found = find_member(std::slice::from_ref(item), source, &owner, word)?;
                if found.name_span.start <= offset && offset <= found.name_span.end {
                    return Some((owner, word.to_string()));
                }
            }
            ItemKind::Mod(decl) => {
                if let ModBody::Inline(inner) = &decl.body {
                    return declared_in(inner, source, offset, word);
                }
            }
            _ => {}
        }
    }
    None
}

/// Every place the member `member` of type `ty` is named: its declaration,
/// each method call or field read whose receiver has that type, and each
/// struct literal of the type that writes the field.
pub(crate) fn member_references(doc: &DocumentAnalysis, ty: &str, member: &str) -> Vec<Span> {
    struct Scan<'a> {
        doc: &'a DocumentAnalysis,
        source: &'a str,
        ty: &'a str,
        member: &'a str,
        out: Vec<Span>,
    }
    impl Scan<'_> {
        /// The span of `member` written after the `.` that follows a
        /// receiver ending at `receiver_end`.
        fn after_dot(
            &self,
            receiver_end: u32,
            end: u32,
            file: gossamer_lex::FileId,
        ) -> Option<Span> {
            let window = self.source.get(receiver_end as usize..end as usize)?;
            let dot = window.find('.')?;
            let rest = &window[dot + 1..];
            let at = dot + 1 + (rest.len() - rest.trim_start().len());
            window[at..].starts_with(self.member).then(|| {
                let start = receiver_end + u32::try_from(at).unwrap_or(0);
                Span::new(
                    file,
                    start,
                    start + u32::try_from(self.member.len()).unwrap_or(0),
                )
            })
        }
    }
    impl Visitor for Scan<'_> {
        fn visit_expr(&mut self, expr: &Expr) {
            match &expr.kind {
                ExprKind::MethodCall { receiver, name, .. } if name.name == self.member => {
                    if receiver_type(self.doc, receiver.id).as_deref() == Some(self.ty)
                        && let Some(span) =
                            self.after_dot(receiver.span.end, expr.span.end, expr.span.file)
                    {
                        self.out.push(span);
                    }
                }
                ExprKind::FieldAccess {
                    receiver,
                    field: FieldSelector::Named(name),
                } if name.name == self.member => {
                    if receiver_type(self.doc, receiver.id).as_deref() == Some(self.ty)
                        && let Some(span) =
                            self.after_dot(receiver.span.end, expr.span.end, expr.span.file)
                    {
                        self.out.push(span);
                    }
                }
                ExprKind::Struct { path, fields, .. }
                    if path.segments.last().is_some_and(|s| s.name.name == self.ty) =>
                {
                    for field in fields {
                        if field.name.name != self.member {
                            continue;
                        }
                        // A field the literal writes is named just before its
                        // value; a filled-in default has no name of its own.
                        if let Some(value) = &field.value
                            && let Some(before) = self.source.get(..value.span.start as usize)
                            && let Some(colon) = before.trim_end().strip_suffix(':')
                            && colon.trim_end().ends_with(self.member)
                        {
                            let end = colon.trim_end().len();
                            let start = end - self.member.len();
                            self.out.push(Span::new(
                                value.span.file,
                                u32::try_from(start).unwrap_or(0),
                                u32::try_from(end).unwrap_or(0),
                            ));
                        }
                    }
                }
                _ => {}
            }
            walk_expr(self, expr);
        }
    }
    let mut scan = Scan {
        doc,
        source: doc.source(),
        ty,
        member,
        out: Vec::new(),
    };
    if let Some(decl) = member_of(doc, ty, member) {
        scan.out.push(decl.name_span);
    }
    scan.visit_source_file(&doc.sf);
    scan.out.sort_by_key(|s| (s.start, s.end));
    scan.out.dedup_by_key(|s| (s.start, s.end));
    scan.out
}
