/// Synthesizes a `fn __gos_typeinfo_<Name>() -> Vec<(String, String)>`
/// returning each field's `(name, type)` for every named-field struct,
/// so `typeInfo::<Name>()` reflects the type's fields at compile time
/// (the comptime reflection surface). Only emitted when the source
/// mentions `typeInfo` so non-reflecting programs carry no extra items.
#[must_use]
pub fn synthesize_type_info(parsed: &SourceFile) -> String {
    let index = ProgramIndex::build(parsed);
    let mut out = String::new();
    let mut emitted: HashSet<String> = HashSet::new();
    let mut emit = |out: &mut String, name: String, entries: &[(String, String)]| {
        if emitted.insert(name.clone()) {
            emit_named(out, &name, entries);
        }
    };
    for (module, item) in flatten_items_with_modules(&parsed.items) {
        let (name, entries) = match &item.kind {
            ItemKind::Struct(decl) if decl.generics.params.is_empty() => {
                let Some(entries) = struct_entries(&decl.body) else {
                    continue;
                };
                (&decl.name.name, entries)
            }
            ItemKind::Enum(decl) if decl.generics.params.is_empty() => {
                (&decl.name.name, enum_entries(decl))
            }
            _ => continue,
        };
        let mut id = module_segments(&module);
        id.push(name.clone());
        emit(&mut out, reflect_fn_name(&id, &[]), &entries);
    }
    // A generic type reflects per instantiation: `typeInfo::<W<i64>>()`
    // describes the fields `W<i64>` actually has. Each spelling the source
    // reflects on gets its own function with the arguments substituted in.
    for (module, segments, args) in generic_type_info_requests(parsed) {
        let Some(id) = index.resolve_type(&module, &segments) else {
            continue;
        };
        let Some(IndexedItem::Type {
            entries: Some(entries),
            params,
            ..
        }) = index.item(&id)
        else {
            continue;
        };
        let entries = substitute_entries(entries, params, &args);
        let mangled: Vec<String> = args.iter().map(|arg| mangle_ty(arg)).collect();
        emit(&mut out, reflect_fn_name(&id, &mangled), &entries);
    }
    out
}

/// The segments of a `a::b` module path as `flatten_items_with_modules`
/// spells it, empty at the unit root.
fn module_segments(module: &str) -> Vec<String> {
    if module.is_empty() {
        Vec::new()
    } else {
        module.split("::").map(str::to_string).collect()
    }
}

/// `(field name, field type)` for a struct body, or `None` for a unit
/// struct, which has nothing to reflect. Tuple-struct fields are named by
/// their position, matching how they are written (`p.0`).
fn struct_entries(body: &StructBody) -> Option<Vec<(String, String)>> {
    match body {
        StructBody::Named(fields) => Some(
            fields
                .iter()
                .map(|f| (f.name.name.clone(), ty_to_string(&f.ty)))
                .collect(),
        ),
        StructBody::Tuple(fields) => Some(
            fields
                .iter()
                .enumerate()
                .map(|(i, f)| (i.to_string(), ty_to_string(&f.ty)))
                .collect(),
        ),
        StructBody::Unit => None,
    }
}

/// `(variant name, payload spelling)` for each variant in declaration
/// order. A unit variant carries `()`, a single payload carries that type,
/// and a multi-field variant carries the tuple of its fields - the same
/// spellings a `match` arm binds.
fn enum_entries(decl: &gossamer_ast::EnumDecl) -> Vec<(String, String)> {
    decl.variants
        .iter()
        .map(|variant| {
            let payload = match &variant.body {
                StructBody::Unit => "()".to_string(),
                StructBody::Tuple(fields) => match fields.as_slice() {
                    [one] => ty_to_string(&one.ty),
                    many => format!(
                        "({})",
                        many.iter()
                            .map(|f| ty_to_string(&f.ty))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                },
                StructBody::Named(fields) => format!(
                    "({})",
                    fields
                        .iter()
                        .map(|f| ty_to_string(&f.ty))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            };
            (variant.name.name.clone(), payload)
        })
        .collect()
}

fn param_names(generics: &gossamer_ast::Generics) -> Vec<String> {
    generics
        .params
        .iter()
        .filter_map(|p| match p {
            gossamer_ast::GenericParam::Type { name, .. } => Some(name.name.clone()),
            _ => None,
        })
        .collect()
}

/// Replaces each type-parameter name in an entry's type spelling with the
/// argument at the same position.
fn substitute_entries(
    entries: &[(String, String)],
    params: &[String],
    args: &[String],
) -> Vec<(String, String)> {
    entries
        .iter()
        .map(|(name, ty)| {
            let mut ty = ty.clone();
            for (param, arg) in params.iter().zip(args) {
                ty = substitute_param(&ty, param, arg);
            }
            (name.clone(), ty)
        })
        .collect()
}

/// Rewrites whole-identifier occurrences of `param` in a type spelling.
/// Matching on identifier boundaries keeps `T` from rewriting the `T` in
/// `Tree`.
fn substitute_param(ty: &str, param: &str, arg: &str) -> String {
    let mut out = String::with_capacity(ty.len());
    let mut rest = ty;
    while let Some(at) = rest.find(param) {
        let before_ok = rest[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric() && c != '_');
        let after = &rest[at + param.len()..];
        let after_ok = after
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric() && c != '_');
        out.push_str(&rest[..at]);
        if before_ok && after_ok {
            out.push_str(arg);
        } else {
            out.push_str(param);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

fn emit_named(out: &mut String, fn_name: &str, entries: &[(String, String)]) {
    let rendered: Vec<String> = entries
        .iter()
        .map(|(name, ty)| format!("(\"{name}\", \"{ty}\")"))
        .collect();
    out.push_str(&format!(
        "fn {fn_name}() -> Vec<(String, String)> {{ Vec::from([{}]) }}\n",
        rendered.join(", "),
    ));
}

/// Reduces a type spelling to the identifier characters a function name
/// accepts, so two different spellings cannot collide on the same name.
fn mangle_ty(spelling: &str) -> String {
    spelling
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect()
}

/// Every `typeInfo::<Name<Args>>()` spelling in the source, as `(module it
/// is written in, type path, argument spellings)`.
fn generic_type_info_requests(sf: &SourceFile) -> Vec<(Vec<String>, Vec<String>, Vec<String>)> {
    use gossamer_ast::Visitor;
    use gossamer_ast::expr::Expr;
    use gossamer_ast::visitor::walk_expr;

    struct Collect<'a> {
        module: &'a [String],
        found: &'a mut Vec<(Vec<String>, Vec<String>, Vec<String>)>,
    }
    impl Visitor for Collect<'_> {
        fn visit_expr(&mut self, expr: &Expr) {
            walk_expr(self, expr);
            let Some(tp) = typeinfo_call_type(expr) else {
                return;
            };
            let Some(type_seg) = tp.segments.last() else {
                return;
            };
            if type_seg.generics.is_empty() {
                return;
            }
            let args: Vec<String> = type_seg
                .generics
                .iter()
                .filter_map(|arg| match arg {
                    GenericArg::Type(t) => Some(ty_to_string(t)),
                    GenericArg::Const(_) => None,
                })
                .collect();
            let segments = tp.segments.iter().map(|s| s.name.name.clone()).collect();
            let entry = (self.module.to_vec(), segments, args);
            if !self.found.contains(&entry) {
                self.found.push(entry);
            }
        }
    }
    let mut found = Vec::new();
    for_each_item(&sf.items, &mut Vec::new(), &mut |module, item| {
        Collect {
            module,
            found: &mut found,
        }
        .visit_item(item);
    });
    found
}

/// Rewrites `typeInfo::<Type>()` into a call to the reflection function of
/// the type the path names from the module it is written in. A type with
/// nothing to reflect keeps a name no function answers, which the resolver
/// reports as such.
pub fn rewrite_type_info_calls(sf: &mut SourceFile) {
    use gossamer_ast::VisitorMut;
    use gossamer_ast::expr::{Expr, ExprKind};
    use gossamer_ast::visitor::walk_expr_mut;

    struct Rewriter<'a> {
        index: &'a ProgramIndex,
        module: &'a [String],
    }
    impl VisitorMut for Rewriter<'_> {
        fn visit_expr(&mut self, expr: &mut Expr) {
            walk_expr_mut(self, expr);
            let Some(tp) = typeinfo_call_type(expr) else {
                return;
            };
            let Some(type_seg) = tp.segments.last() else {
                return;
            };
            let args: Vec<String> = type_seg
                .generics
                .iter()
                .filter_map(|arg| match arg {
                    GenericArg::Type(t) => Some(mangle_ty(&ty_to_string(t))),
                    GenericArg::Const(_) => None,
                })
                .collect();
            let segments: Vec<String> = tp.segments.iter().map(|s| s.name.name.clone()).collect();
            let target = match self.index.resolve_type(self.module, &segments) {
                Some(id) => reflect_fn_name(&id, &args),
                None => reflect_fn_name(std::slice::from_ref(&type_seg.name.name), &args),
            };
            let ExprKind::Call { callee, .. } = &mut expr.kind else {
                return;
            };
            let ExprKind::Path(path) = &mut callee.kind else {
                return;
            };
            let seg = &mut path.segments[0];
            seg.name.name = target;
            seg.generics.clear();
        }
    }
    let index = ProgramIndex::build(sf);
    for_each_item_mut(&mut sf.items, &mut Vec::new(), &mut |module, item| {
        Rewriter {
            index: &index,
            module,
        }
        .visit_item(item);
    });
}

/// Hands out fresh node ids for cloned `inline for` bodies. The resolver
/// and checker key maps on `NodeId`, so each unrolled copy needs unique
/// ids rather than the source body's (which would collide).
struct InlineForIds {
    next: u32,
}

impl InlineForIds {
    fn id(&mut self) -> NodeId {
        let id = NodeId::from_raw(self.next);
        self.next += 1;
        id
    }
}

/// Renumbers a cloned subtree's node ids in place.
struct InlineForRenum<'a> {
    ids: &'a mut InlineForIds,
}

impl gossamer_ast::VisitorMut for InlineForRenum<'_> {
    fn visit_expr(&mut self, e: &mut gossamer_ast::expr::Expr) {
        e.id = self.ids.id();
        gossamer_ast::visitor::walk_expr_mut(self, e);
    }
    fn visit_pattern(&mut self, p: &mut gossamer_ast::pattern::Pattern) {
        p.id = self.ids.id();
        gossamer_ast::visitor::walk_pattern_mut(self, p);
    }
    fn visit_stmt(&mut self, s: &mut gossamer_ast::stmt::Stmt) {
        s.id = self.ids.id();
        gossamer_ast::visitor::walk_stmt_mut(self, s);
    }
    fn visit_type(&mut self, t: &mut gossamer_ast::ty::Type) {
        t.id = self.ids.id();
        gossamer_ast::visitor::walk_type_mut(self, t);
    }
}

/// Expands every `for PAT in typeInfo::<T>() { BODY }` over a known struct
/// `T` into a straight-line block: the body is cloned once per field, the
/// loop variables bound to the field's `(name, type)` as comptime string
/// literals, `field_of(recv, name)` resolved to `recv.<name>`, and
/// `match` / `if` over the comptime values folded to the taken branch.
/// Pure AST -> AST in the single compile, so the emitted field code lowers
/// natively on every tier and the construct never reaches the comptime
/// fold pass (no whole-program re-compile tax).
pub fn expand_typeinfo_loops(sf: &mut SourceFile) {
    use gossamer_ast::VisitorMut;
    let index = ProgramIndex::build(sf);
    let mut ids = InlineForIds {
        next: sf.next_node_id,
    };
    for_each_item_mut(&mut sf.items, &mut Vec::new(), &mut |module, item| {
        TypeInfoLoopExpander {
            index: &index,
            module,
            ids: &mut ids,
        }
        .visit_item(item);
    });
    sf.next_node_id = ids.next;
}

struct TypeInfoLoopExpander<'a> {
    index: &'a ProgramIndex,
    module: &'a [String],
    ids: &'a mut InlineForIds,
}

impl gossamer_ast::VisitorMut for TypeInfoLoopExpander<'_> {
    fn visit_expr(&mut self, e: &mut gossamer_ast::expr::Expr) {
        use gossamer_ast::expr::ExprKind;
        // Post-order: expand nested field loops before the enclosing one
        // clones the body.
        gossamer_ast::visitor::walk_expr_mut(self, e);
        let ExprKind::For {
            pattern,
            iter,
            body,
            ..
        } = &e.kind
        else {
            return;
        };
        let Some(tp) = typeinfo_call_type(iter) else {
            return;
        };
        if tp.segments.iter().any(|seg| !seg.generics.is_empty()) {
            return;
        }
        let segments: Vec<String> = tp.segments.iter().map(|s| s.name.name.clone()).collect();
        let Some(fields) = self
            .index
            .resolve_type(self.module, &segments)
            .and_then(|id| self.index.struct_fields(&id))
        else {
            return;
        };
        let block = unroll_typeinfo_loop(pattern, body, fields, self.ids);
        e.kind = ExprKind::Block(block);
    }
}

/// Unrolls one field loop body across `fields`, returning the straight-line
/// block. Each field's copy is scoped in its own nested block so any `let`
/// bindings the body introduces do not collide across iterations.
fn unroll_typeinfo_loop(
    pattern: &gossamer_ast::pattern::Pattern,
    body: &gossamer_ast::expr::Expr,
    fields: &[(String, String)],
    ids: &mut InlineForIds,
) -> gossamer_ast::expr::Block {
    use gossamer_ast::expr::Block;
    use gossamer_ast::stmt::{Stmt, StmtKind};
    let mut stmts = Vec::with_capacity(fields.len());
    for (fname, ftype) in fields {
        let mut binds: HashMap<String, String> = HashMap::new();
        bind_loop_pattern(pattern, fname, ftype, &mut binds);
        let mut clone = body.clone();
        let mut subst = InlineForSubst { binds: &binds };
        subst.visit_expr(&mut clone);
        let mut renum = InlineForRenum { ids };
        renum.visit_expr(&mut clone);
        let span = clone.span;
        let id = ids.id();
        stmts.push(Stmt::new(
            id,
            span,
            StmtKind::Expr {
                expr: Box::new(clone),
                has_semi: true,
            },
        ));
    }
    Block {
        stmts,
        tail: None,
        synthetic: true,
        kind: gossamer_ast::BlockKind::Plain,
    }
}

/// Binds the loop pattern's identifiers to the current field's `(name,
/// type)` strings. Supports the canonical `(name, ty)` tuple pattern;
/// wildcards bind nothing.
fn bind_loop_pattern(
    pattern: &gossamer_ast::pattern::Pattern,
    fname: &str,
    ftype: &str,
    binds: &mut HashMap<String, String>,
) {
    use gossamer_ast::pattern::PatternKind;
    let PatternKind::Tuple(elems) = &pattern.kind else {
        return;
    };
    if elems.len() != 2 {
        return;
    }
    bind_one(&elems[0], fname, binds);
    bind_one(&elems[1], ftype, binds);
}

fn bind_one(p: &gossamer_ast::pattern::Pattern, val: &str, binds: &mut HashMap<String, String>) {
    use gossamer_ast::pattern::PatternKind;
    if let PatternKind::Ident {
        name,
        subpattern: None,
        ..
    } = &p.kind
    {
        binds.insert(name.name.clone(), val.to_string());
    }
}

/// Substitutes bound loop variables with their comptime string values,
/// resolves `field_of(recv, name)` projections, and folds `match` / `if` /
/// string operations over the now-comptime values. Post-order, so a node's
/// children are already substituted when it is folded.
struct InlineForSubst<'a> {
    binds: &'a HashMap<String, String>,
}

impl gossamer_ast::VisitorMut for InlineForSubst<'_> {
    fn visit_expr(&mut self, e: &mut gossamer_ast::expr::Expr) {
        gossamer_ast::visitor::walk_expr_mut(self, e);
        if let Some(kind) = self.fold_node(e) {
            e.kind = kind;
        }
    }
}

impl InlineForSubst<'_> {
    fn fold_node(&self, e: &gossamer_ast::expr::Expr) -> Option<gossamer_ast::expr::ExprKind> {
        use gossamer_ast::common::Ident;
        use gossamer_ast::expr::{ExprKind, FieldSelector, Literal};
        match &e.kind {
            ExprKind::Path(p) if p.segments.len() == 1 && p.segments[0].generics.is_empty() => self
                .binds
                .get(p.segments[0].name.name.as_str())
                .map(|v| ExprKind::Literal(Literal::String(v.clone()))),
            ExprKind::Call { callee, args } if is_field_of(callee) && args.len() == 2 => {
                let ExprKind::Literal(Literal::String(fld)) = &args[1].kind else {
                    return None;
                };
                Some(ExprKind::FieldAccess {
                    receiver: Box::new(args[0].clone()),
                    field: FieldSelector::Named(Ident::new(fld.clone())),
                })
            }
            ExprKind::Match { scrutinee, arms } => {
                let s = literal_str(scrutinee)?;
                let body = select_arm(&s, arms)?;
                Some(body.kind.clone())
            }
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let ExprKind::Literal(Literal::Bool(b)) = &condition.kind else {
                    return None;
                };
                if *b {
                    Some(then_branch.kind.clone())
                } else {
                    Some(
                        else_branch
                            .as_ref()
                            .map_or(ExprKind::Literal(Literal::Unit), |x| x.kind.clone()),
                    )
                }
            }
            ExprKind::Binary { op, lhs, rhs } => fold_binary(*op, lhs, rhs),
            _ => None,
        }
    }
}

fn is_field_of(callee: &gossamer_ast::expr::Expr) -> bool {
    use gossamer_ast::expr::ExprKind;
    let ExprKind::Path(p) = &callee.kind else {
        return false;
    };
    p.segments.len() == 1 && p.segments[0].name.name == "field_of"
}

fn literal_str(e: &gossamer_ast::expr::Expr) -> Option<String> {
    use gossamer_ast::expr::{ExprKind, Literal};
    if let ExprKind::Literal(Literal::String(s)) = &e.kind {
        Some(s.clone())
    } else {
        None
    }
}

/// Selects the first arm (no guard) whose pattern matches the comptime
/// string `s`: a string-literal pattern equal to `s`, a wildcard, or an
/// or-pattern containing either.
fn select_arm<'a>(
    s: &str,
    arms: &'a [gossamer_ast::expr::MatchArm],
) -> Option<&'a gossamer_ast::expr::Expr> {
    arms.iter()
        .find(|arm| arm.guard.is_none() && pattern_matches_str(&arm.pattern, s))
        .map(|arm| &arm.body)
}

fn pattern_matches_str(p: &gossamer_ast::pattern::Pattern, s: &str) -> bool {
    use gossamer_ast::expr::Literal;
    use gossamer_ast::pattern::PatternKind;
    match &p.kind {
        PatternKind::Wildcard => true,
        PatternKind::Literal(Literal::String(lit)) => lit == s,
        PatternKind::Or(alts) => alts.iter().any(|a| pattern_matches_str(a, s)),
        _ => false,
    }
}

/// Folds string concatenation and string equality over two comptime string
/// literals; other shapes stay runtime.
fn fold_binary(
    op: gossamer_ast::common::BinaryOp,
    lhs: &gossamer_ast::expr::Expr,
    rhs: &gossamer_ast::expr::Expr,
) -> Option<gossamer_ast::expr::ExprKind> {
    use gossamer_ast::common::BinaryOp;
    use gossamer_ast::expr::{ExprKind, Literal};
    let (l, r) = (literal_str(lhs)?, literal_str(rhs)?);
    match op {
        BinaryOp::Add => Some(ExprKind::Literal(Literal::String(l + &r))),
        BinaryOp::Eq => Some(ExprKind::Literal(Literal::Bool(l == r))),
        BinaryOp::Ne => Some(ExprKind::Literal(Literal::Bool(l != r))),
        _ => None,
    }
}

/// Specializes reflecting generic functions per turbofish call site, so a
/// reflection-driven helper can be written once - in any module or package -
/// as `fn name<T>(v: T) -> String { ... typeInfo::<T>() ... }` and called as
/// `name::<User>(x)` from anywhere. Each distinct `(function, type)` pair gets
/// a monomorphic copy `__gos_inlinefor_name_<type>` beside the generic, with
/// the type parameter replaced everywhere, so the inner `typeInfo::<T>()`
/// names the concrete type the later passes reflect. A type the program
/// declares reaches the copy through a root alias, which every module and
/// package can name; the call is rewritten to the copy, an import that bound
/// the generic binds the copy beside it, and the generic itself is removed so
/// it is never type-checked generically. A call without a turbofish is
/// renamed so the resolver reports the missing type there.
pub fn specialize_inline_for_generics(sf: &mut SourceFile) {
    let index = ProgramIndex::build(sf);
    let mut templates: HashMap<Vec<String>, String> = HashMap::new();
    collect_reflecting_templates(&sf.items, &mut Vec::new(), &mut templates);
    if templates.is_empty() {
        return;
    }
    let mut sites = TemplateSites::default();
    let mut module = Vec::new();
    for_each_item_mut(&mut sf.items, &mut module, &mut |module, item| {
        let mut rewriter = TemplateCallRewriter {
            index: &index,
            templates: &templates,
            module,
            sites: &mut sites,
        };
        gossamer_ast::VisitorMut::visit_item(&mut rewriter, item);
    });

    let mut ids = InlineForIds {
        next: sf.next_node_id,
    };
    for (template, param) in &templates {
        let (name, home) = template.split_last().expect("a template path names a function");
        let Some(items) = module_items_mut(&mut sf.items, home) else {
            continue;
        };
        let Some(position) = items.iter().position(|item| {
            matches!(&item.kind, ItemKind::Fn(decl) if decl.name.name == *name && !decl.generics.params.is_empty())
        }) else {
            continue;
        };
        let item = items.remove(position);
        let ItemKind::Fn(decl) = &item.kind else {
            continue;
        };
        for (concrete, copy_name) in sites.copies.iter().filter(|(t, ..)| t == template).map(|(_, c, n)| (c, n)) {
            let replacement = concrete.replacement(&mut ids, item.span);
            items.push(specialize_template(&item, decl, param, &replacement, copy_name, &mut ids));
        }
    }
    for id in &sites.aliased {
        let (spelling, dependency) = index.root_spelling(id);
        let segments = spelling
            .split("::")
            .map(gossamer_ast::TypePathSegment::new)
            .collect();
        let ty = gossamer_ast::Type::new(
            ids.id(),
            Span::new(sf.file, 0, 0),
            TypeKind::Path(gossamer_ast::TypePath { segments }),
        );
        sf.items.push(Item::new(
            ids.id(),
            Span::new(sf.file, 0, 0),
            gossamer_ast::Attrs::default(),
            gossamer_ast::Visibility::Inherited,
            ItemKind::TypeAlias(gossamer_ast::TypeAliasDecl {
                name: gossamer_ast::Ident::new(reflect_alias_name(id)),
                generics: gossamer_ast::Generics { params: Vec::new() },
                ty,
                nominal: false,
            }),
        ));
        if let Some(dependency) = dependency
            && !root_imports(sf, &dependency)
        {
            let mut decl = UseDecl::simple(
                ids.id(),
                Span::new(sf.file, 0, 0),
                UseTarget::Module(ModulePath::from_names([dependency])),
            );
            decl.module = Vec::new();
            sf.uses.push(decl);
        }
    }
    drop_template_imports(sf, &index, &templates);
    for (scope, path) in sites.imports {
        let mut decl = UseDecl::simple(
            ids.id(),
            Span::new(sf.file, 0, 0),
            UseTarget::Module(ModulePath::from_names(path)),
        );
        decl.module = scope;
        sf.uses.push(decl);
    }
    sf.next_node_id = ids.next;
}

/// Removes the imports that bind a reflecting template, which no longer
/// exists once its copies are made; each call site imports its copy instead.
fn drop_template_imports(
    sf: &mut SourceFile,
    index: &ProgramIndex,
    templates: &HashMap<Vec<String>, String>,
) {
    let names_template = |module: &[String], path: Vec<String>| {
        index
            .resolve(module, &path)
            .is_some_and(|target| templates.contains_key(&target))
    };
    sf.uses.retain_mut(|decl| {
        let UseTarget::Module(target) = &decl.target else {
            return true;
        };
        let base: Vec<String> = target.segments.iter().map(|s| s.name.clone()).collect();
        match &mut decl.list {
            None => decl.glob || !names_template(&decl.module, base),
            Some(entries) => {
                entries.retain(|entry| {
                    let mut full = base.clone();
                    full.extend(entry.prefix.iter().map(|s| s.name.clone()));
                    full.push(entry.name.name.clone());
                    !names_template(&decl.module, full)
                });
                !entries.is_empty()
            }
        }
    });
}

/// Name of the root alias that lets a specialized copy name the type `id`.
fn reflect_alias_name(id: &[String]) -> String {
    format!("__gos_reflect__{}", id.join("__"))
}

/// Whether the unit root already imports the module `name`.
fn root_imports(sf: &SourceFile, name: &str) -> bool {
    sf.uses.iter().any(|decl| {
        decl.module.is_empty()
            && decl.list.is_none()
            && matches!(&decl.target, UseTarget::Module(path)
                if path.segments.len() == 1 && path.segments[0].name == name)
            && decl.alias.as_ref().is_none_or(|alias| alias.name == name)
    })
}

/// The type a template's parameter is replaced with in one copy.
#[derive(Debug, Clone)]
enum TemplateArg {
    /// A struct or enum the program declares, at this absolute path.
    Declared(Vec<String>),
    /// Any other type, as written at the call site.
    Written(String, gossamer_ast::Type),
}

impl TemplateArg {
    fn key(&self) -> String {
        match self {
            Self::Declared(id) => id.join("_"),
            Self::Written(spelling, _) => spelling.clone(),
        }
    }

    fn replacement(&self, ids: &mut InlineForIds, span: Span) -> gossamer_ast::Type {
        match self {
            Self::Declared(id) => gossamer_ast::Type::new(
                ids.id(),
                span,
                TypeKind::Path(gossamer_ast::TypePath {
                    segments: vec![gossamer_ast::TypePathSegment::new(reflect_alias_name(id))],
                }),
            ),
            Self::Written(_, ty) => ty.clone(),
        }
    }
}
/// Every generic free function whose body reflects one of its own type
/// parameters (`typeInfo::<T>()` in a field loop or as a value), by absolute
/// path, with that parameter.
fn collect_reflecting_templates(
    items: &[Item],
    module: &mut Vec<String>,
    out: &mut HashMap<Vec<String>, String>,
) {
    use gossamer_ast::{GenericParam, Visitor};
    for item in items {
        match &item.kind {
            ItemKind::Fn(decl) => {
                let params: Vec<String> = decl
                    .generics
                    .params
                    .iter()
                    .filter_map(|p| match p {
                        GenericParam::Type { name, .. } => Some(name.name.clone()),
                        _ => None,
                    })
                    .collect();
                let [param] = params.as_slice() else {
                    continue;
                };
                let Some(body) = &decl.body else { continue };
                let mut probe = TemplateProbe { param, found: false };
                probe.visit_expr(body);
                if probe.found {
                    let mut path = module.clone();
                    path.push(decl.name.name.clone());
                    out.insert(path, param.clone());
                }
            }
            ItemKind::Mod(decl) => {
                if let ModBody::Inline(inner) = &decl.body {
                    module.push(decl.name.name.clone());
                    collect_reflecting_templates(inner, module, out);
                    module.pop();
                }
            }
            _ => {}
        }
    }
}

/// Finds a `typeInfo::<P>()` call on the type parameter `P`.
struct TemplateProbe<'a> {
    param: &'a str,
    found: bool,
}

impl gossamer_ast::Visitor for TemplateProbe<'_> {
    fn visit_expr(&mut self, e: &gossamer_ast::expr::Expr) {
        if self.found {
            return;
        }
        if typeinfo_call_type(e).is_some_and(|tp| {
            matches!(tp.segments.as_slice(), [seg] if seg.name.name == self.param && seg.generics.is_empty())
        }) {
            self.found = true;
            return;
        }
        gossamer_ast::visitor::walk_expr(self, e);
    }
}

/// The type path a `typeInfo::<Type>()` call reflects.
fn typeinfo_call_type(e: &gossamer_ast::expr::Expr) -> Option<&gossamer_ast::TypePath> {
    use gossamer_ast::expr::ExprKind;
    let ExprKind::Call { callee, args } = &e.kind else {
        return None;
    };
    if !args.is_empty() {
        return None;
    }
    let ExprKind::Path(path) = &callee.kind else {
        return None;
    };
    let [seg] = path.segments.as_slice() else {
        return None;
    };
    if seg.name.name != "typeInfo" {
        return None;
    }
    let [GenericArg::Type(ty)] = seg.generics.as_slice() else {
        return None;
    };
    match &ty.kind {
        TypeKind::Path(tp) => Some(tp),
        _ => None,
    }
}

/// What the call sites of reflecting templates ask for.
#[derive(Default)]
struct TemplateSites {
    /// `(template, argument, copy name)`, one per distinct copy.
    copies: Vec<(Vec<String>, TemplateArg, String)>,
    /// Declared types a copy names through a root alias.
    aliased: std::collections::BTreeSet<Vec<String>>,
    /// `(module, path)` imports binding a copy where the generic was
    /// imported.
    imports: std::collections::BTreeSet<(Vec<String>, Vec<String>)>,
}

/// Rewrites each call of a reflecting template in one module's bodies to the
/// copy for its turbofish type, recording the copies that need to exist.
struct TemplateCallRewriter<'a> {
    index: &'a ProgramIndex,
    templates: &'a HashMap<Vec<String>, String>,
    module: &'a [String],
    sites: &'a mut TemplateSites,
}

impl gossamer_ast::VisitorMut for TemplateCallRewriter<'_> {
    fn visit_expr(&mut self, e: &mut gossamer_ast::expr::Expr) {
        use gossamer_ast::expr::ExprKind;
        gossamer_ast::visitor::walk_expr_mut(self, e);
        let ExprKind::Call { callee, .. } = &mut e.kind else {
            return;
        };
        let ExprKind::Path(path) = &mut callee.kind else {
            return;
        };
        let names: Vec<String> = path.segments.iter().map(|s| s.name.name.clone()).collect();
        let Some(template) = self.index.resolve(self.module, &names) else {
            return;
        };
        if !self.templates.contains_key(&template) {
            return;
        }
        let last = path.segments.last_mut().expect("a resolved path has a segment");
        let original = last.name.name.clone();
        let fn_name = template.last().expect("a template path names a function").clone();
        let arg = match last.generics.as_slice() {
            [] => {
                // No type named, so no specialisation exists to call; the
                // resolver reports the missing turbofish at this name.
                last.name = gossamer_ast::Ident::new(format!(
                    "{}{original}",
                    gossamer_ast::common::REFLECTING_CALL_WITHOUT_TYPE_PREFIX
                ));
                return;
            }
            [GenericArg::Type(ty)] => self.template_arg(ty),
            _ => return,
        };
        let copy_name = format!("__gos_inlinefor_{fn_name}_{}", mangle_ty(&arg.key()));
        if let TemplateArg::Declared(id) = &arg {
            self.sites.aliased.insert(id.clone());
        }
        if !self
            .sites
            .copies
            .iter()
            .any(|(t, _, name)| *t == template && *name == copy_name)
        {
            self.sites.copies.push((template.clone(), arg, copy_name.clone()));
        }
        last.name = gossamer_ast::Ident::new(copy_name.clone());
        last.generics.clear();
        if names.len() == 1
            && let Some((scope, imported)) = self.import_binding(&original)
        {
            let mut bound = imported[..imported.len() - 1].to_vec();
            bound.push(copy_name);
            self.sites.imports.insert((scope, bound));
        }
    }
}

impl TemplateCallRewriter<'_> {
    fn template_arg(&self, ty: &gossamer_ast::Type) -> TemplateArg {
        if let TypeKind::Path(tp) = &ty.kind
            && tp.segments.iter().all(|seg| seg.generics.is_empty())
        {
            let names: Vec<String> = tp.segments.iter().map(|s| s.name.name.clone()).collect();
            if let Some(id) = self.index.resolve_type(self.module, &names) {
                return TemplateArg::Declared(id);
            }
        }
        TemplateArg::Written(ty_to_string(ty), ty.clone())
    }

    /// The module whose `use` binds `name` where this module reads it, and
    /// the path that `use` wrote; `None` when a declared item binds it.
    fn import_binding(&self, name: &str) -> Option<(Vec<String>, Vec<String>)> {
        (0..=self.module.len()).rev().find_map(|level| {
            let scope = &self.module[..level];
            let items = self.index.modules.get(scope)?;
            if items.items.contains_key(name) {
                return Some(None);
            }
            items
                .imports
                .get(name)
                .map(|path| Some((scope.to_vec(), path.clone())))
        })?
    }
}

/// Builds one monomorphic copy of `decl`, named `copy_name`, with the type
/// parameter `param` replaced by `replacement`, freshly numbered.
fn specialize_template(
    item: &gossamer_ast::Item,
    decl: &gossamer_ast::FnDecl,
    param: &str,
    replacement: &gossamer_ast::Type,
    copy_name: &str,
    ids: &mut InlineForIds,
) -> gossamer_ast::Item {
    use gossamer_ast::items::Generics;
    use gossamer_ast::{FnParam, Ident, VisitorMut};
    let mut copy = decl.clone();
    copy.name = Ident::new(copy_name);
    copy.generics = Generics { params: Vec::new() };

    let mut sub = SubstTypeParam { param, replacement };
    for p in &mut copy.params {
        if let FnParam::Typed { ty, .. } = p {
            sub.visit_type(ty);
        }
    }
    if let Some(ret) = &mut copy.ret {
        sub.visit_type(ret);
    }
    if let Some(body) = &mut copy.body {
        sub.visit_expr(body);
    }

    let mut renum = InlineForRenum { ids };
    for p in &mut copy.params {
        if let FnParam::Typed { pattern, ty, .. } = p {
            renum.visit_pattern(pattern);
            renum.visit_type(ty);
        }
    }
    if let Some(ret) = &mut copy.ret {
        renum.visit_type(ret);
    }
    if let Some(body) = &mut copy.body {
        renum.visit_expr(body);
    }

    gossamer_ast::Item::new(
        ids.id(),
        item.span,
        item.attrs.clone(),
        item.visibility,
        ItemKind::Fn(copy),
    )
}

/// Replaces the type parameter `param` with `replacement` throughout the
/// visited types - including the `T` inside the body's `typeInfo::<T>()`
/// turbofish, which the type walker reaches.
struct SubstTypeParam<'a> {
    param: &'a str,
    replacement: &'a gossamer_ast::Type,
}

impl gossamer_ast::VisitorMut for SubstTypeParam<'_> {
    fn visit_type(&mut self, ty: &mut gossamer_ast::ty::Type) {
        use gossamer_ast::ty::TypeKind;
        gossamer_ast::visitor::walk_type_mut(self, ty);
        if let TypeKind::Path(tp) = &ty.kind
            && let [seg] = tp.segments.as_slice()
            && seg.name.name == self.param
            && seg.generics.is_empty()
        {
            ty.kind = self.replacement.kind.clone();
        }
    }
}

