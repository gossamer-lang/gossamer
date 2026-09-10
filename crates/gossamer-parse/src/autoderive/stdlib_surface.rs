/// Module names the stdlib rewrite table keys on. A path headed by one of
/// these is stdlib surface only where the file imported that module from
/// `std`, so a project that declares its own `sql` module keeps its own
/// `sql::Stmt`. A head outside this set is not a module key at all
/// (`Http2Config::default`) and passes through unguarded.
const REWRITTEN_STDLIB_MODULES: &[&str] = &[
    "csrf", "form", "fs", "http", "path", "pem", "sql", "tar", "time", "x509", "zip",
];

/// Stdlib modules this compilation unit reached through `use std::...`, under
/// the name each is spelled by. `use` decls inside `mod` bodies are hoisted to
/// the source file, so a bundle answers for every one of its files.
fn stdlib_modules_in_scope(sf: &SourceFile) -> std::collections::HashSet<String> {
    use gossamer_ast::UseTarget;

    let mut out: std::collections::HashSet<String> = std::collections::HashSet::new();
    for decl in &sf.uses {
        let UseTarget::Module(path) = &decl.target else {
            continue;
        };
        if path.segments.first().map(|s| s.name.as_str()) != Some("std") {
            continue;
        }
        if let Some(entries) = &decl.list {
            out.extend(entries.iter().map(|entry| entry.name.name.clone()));
        }
        if let Some(last) = path.segments.last() {
            out.insert(last.name.clone());
        }
    }
    out
}

/// The rewrite-table module key `head` names, or `None` where `head` spells a
/// stdlib module this unit never imported. A path rooted at `std` names the
/// stdlib outright, whatever is in scope.
fn stdlib_module_key<'a>(
    scope: &std::collections::HashSet<String>,
    head: &'a str,
    rooted_std: bool,
) -> Option<&'a str> {
    if rooted_std || scope.contains(head) || !REWRITTEN_STDLIB_MODULES.contains(&head) {
        return Some(head);
    }
    None
}

/// Bare names a `use std::<module>::<item>` brought into scope that reach an
/// injected wrapper, mapped to that wrapper's mangled name. An item the file
/// declares itself keeps its own meaning, so a program with its own `shield`
/// is untouched.
fn imported_wrapper_names(sf: &SourceFile) -> std::collections::HashMap<String, &'static str> {
    use gossamer_ast::{ItemKind, UseTarget};

    let declared: std::collections::HashSet<&str> = sf
        .items
        .iter()
        .filter_map(|item| match &item.kind {
            ItemKind::Fn(decl) => Some(decl.name.name.as_str()),
            ItemKind::Struct(decl) => Some(decl.name.name.as_str()),
            ItemKind::Enum(decl) => Some(decl.name.name.as_str()),
            _ => None,
        })
        .collect();
    let mut out: std::collections::HashMap<String, &'static str> = std::collections::HashMap::new();
    for decl in &sf.uses {
        let UseTarget::Module(path) = &decl.target else {
            continue;
        };
        let base: Vec<String> = path.segments.iter().map(|s| s.name.clone()).collect();
        // Only `std` reaches an injected wrapper; `use sql::Stmt` over a
        // project's own `sql` module binds that module's `Stmt`.
        if base.first().map(String::as_str) != Some("std") {
            continue;
        }
        let mut candidates: Vec<(Vec<String>, String)> = Vec::new();
        if let Some(entries) = &decl.list {
            for entry in entries {
                let mut segments = base.clone();
                segments.extend(entry.prefix.iter().map(|s| s.name.clone()));
                segments.push(entry.name.name.clone());
                let bound = entry
                    .alias
                    .as_ref()
                    .map_or_else(|| entry.name.name.clone(), |a| a.name.clone());
                candidates.push((segments, bound));
            }
        } else {
            let bound = decl.alias.as_ref().map_or_else(
                || base.last().cloned().unwrap_or_default(),
                |a| a.name.clone(),
            );
            candidates.push((base.clone(), bound));
        }
        for (segments, bound) in candidates {
            let n = segments.len();
            if n < 2 || declared.contains(bound.as_str()) {
                continue;
            }
            if let Some(mangled) = mangled_stdlib_name(
                segments[n - 2].as_str(),
                segments[n - 1].as_str(),
            ) {
                out.insert(bound, mangled);
            }
        }
    }
    out
}

/// Redirects the user-facing stdlib struct surface
/// (`encoding::pem::decode(..)`, the `pem::Block { .. }` literal,
/// `pem::Block` type annotations) onto the injected real-struct
/// wrappers. Mirrors `rewrite_serde_generic_calls` but covers
/// multi-segment module paths in call, struct-literal, and type
/// positions.
#[allow(
    clippy::too_many_lines,
    reason = "the nested visitor helpers form one closed stdlib path-rewrite pass"
)]
pub fn rewrite_stdlib_struct_surface(sf: &mut SourceFile) {
    use gossamer_ast::VisitorMut;
    use gossamer_ast::expr::{Expr, ExprKind};
    use gossamer_ast::ty::{Type, TypeKind};
    use gossamer_ast::visitor::{walk_expr_mut, walk_type_mut};

    fn collapse_expr(
        path: &mut gossamer_ast::PathExpr,
        scope: &std::collections::HashSet<String>,
    ) {
        let n = path.segments.len();
        if n < 2 {
            return;
        }
        // A path rooted at `std`, or at a module this unit imported from it,
        // names the stdlib the whole way down: `archive::tar::read` is the
        // stdlib's `read` wherever `archive` came from `std`.
        let root = path.segments[0].name.name.as_str();
        let rooted = root == "std" || scope.contains(root);
        let key = |i: usize| {
            stdlib_module_key(scope, path.segments[i].name.name.as_str(), rooted).map(str::to_owned)
        };
        let head3 = if n >= 3 { key(n - 3) } else { None };
        let head2 = key(n - 2);
        let head3 = head3.as_deref();
        let head2 = head2.as_deref();
        // Enum-variant paths: `sql::Value::Int(..)` /
        // `sql::IsolationLevel::Serializable` collapse to the
        // injected enum + variant, guarded on the `sql` segment so
        // a user's own `Value::Int` is untouched.
        if n >= 3 && head3 == Some("sql") {
            let enum_name = path.segments[n - 2].name.name.as_str();
            if let Some(mangled) = match enum_name {
                "Value" => Some("__gos_sql_Value"),
                "IsolationLevel" => Some("__gos_sql_IsolationLevel"),
                _ => None,
            } {
                let variant = std::mem::replace(
                    &mut path.segments[n - 1],
                    gossamer_ast::PathSegment::new(""),
                );
                path.segments = vec![gossamer_ast::PathSegment::new(mangled), variant];
                return;
            }
            // Associated free functions: `sql::Select::new(..)`,
            // `sql::Pool::open(..)`, `sql::migrate::up(..)` collapse
            // to their injected free-fn wrappers.
            let assoc = (enum_name, path.segments[n - 1].name.name.as_str());
            if let Some(mangled) = match assoc {
                ("Select", "new") => Some("__gos_sql_select_new"),
                ("Pool", "open") => Some("__gos_sql_pool_open"),
                ("Pool", "open_with") => Some("__gos_sql_pool_open_with"),
                ("migrate", "up") => Some("__gos_sql_migrate_up"),
                _ => None,
            } {
                path.segments = vec![gossamer_ast::PathSegment::new(mangled)];
                return;
            }
        }
        if n >= 3
            && head3 == Some("path")
            && path.segments[n - 2].name.name.as_str() == "Path"
        {
            let method = std::mem::replace(
                &mut path.segments[n - 1],
                gossamer_ast::PathSegment::new(""),
            );
            path.segments = vec![gossamer_ast::PathSegment::new("__gos_path_Path"), method];
            return;
        }
        if n >= 3 && head3 == Some("time") {
            let public_type = path.segments[n - 2].name.name.as_str();
            if let Some(mangled) = match public_type {
                "Location" => Some("__gos_time_Location"),
                "CivilResolution" => Some("__gos_time_CivilResolution"),
                _ => None,
            } {
                let member = std::mem::replace(
                    &mut path.segments[n - 1],
                    gossamer_ast::PathSegment::new(""),
                );
                path.segments = vec![gossamer_ast::PathSegment::new(mangled), member];
                return;
            }
        }
        if n >= 3
            && matches!(head3, Some("csrf" | "form"))
            && collapse_http_security_path(path)
        {
            return;
        }
        if let Some(name) = head2
            .and_then(|parent| mangled_stdlib_name(parent, path.segments[n - 1].name.name.as_str()))
        {
            let mut seg = gossamer_ast::PathSegment::new(name);
            seg.generics = std::mem::take(&mut path.segments[n - 1].generics);
            path.segments = vec![seg];
        }
    }

    fn collapse_type(
        path: &mut gossamer_ast::ty::TypePath,
        scope: &std::collections::HashSet<String>,
    ) {
        let n = path.segments.len();
        if n < 2 {
            return;
        }
        let root = path.segments[0].name.name.as_str();
        let rooted = root == "std" || scope.contains(root);
        let parent =
            stdlib_module_key(scope, path.segments[n - 2].name.name.as_str(), rooted).map(str::to_owned);
        let parent = parent.as_deref();
        // `sql::Error` is the standard error type at the language
        // level - redirect to `errors::Error`.
        if parent == Some("sql") && path.segments[n - 1].name.name.as_str() == "Error" {
            path.segments = vec![
                gossamer_ast::ty::TypePathSegment::new("errors"),
                gossamer_ast::ty::TypePathSegment::new("Error"),
            ];
            return;
        }
        if let Some(name) = parent
            .and_then(|parent| mangled_stdlib_name(parent, path.segments[n - 1].name.name.as_str()))
        {
            let mut seg = gossamer_ast::ty::TypePathSegment::new(name);
            seg.generics = std::mem::take(&mut path.segments[n - 1].generics);
            path.segments = vec![seg];
        }
    }

    /// Rewrites a bare name a `use std::<module>::<item>` bound, which
    /// reaches the same injected wrapper the qualified path does. Without
    /// it the imported spelling type-checks and then finds no such name at
    /// run time.
    struct BareImportRewriter {
        imported: std::collections::HashMap<String, &'static str>,
    }

    impl VisitorMut for BareImportRewriter {
        fn visit_expr(&mut self, expr: &mut Expr) {
            walk_expr_mut(self, expr);
            if let ExprKind::Path(path) = &mut expr.kind
                && path.segments.len() == 1
                && let Some(mangled) = self.imported.get(path.segments[0].name.name.as_str())
            {
                let mut seg = gossamer_ast::PathSegment::new(*mangled);
                seg.generics = std::mem::take(&mut path.segments[0].generics);
                path.segments = vec![seg];
            }
        }

        fn visit_type(&mut self, ty: &mut Type) {
            walk_type_mut(self, ty);
            if let TypeKind::Path(path) = &mut ty.kind
                && path.segments.len() == 1
                && let Some(mangled) = self.imported.get(path.segments[0].name.name.as_str())
            {
                let mut seg = gossamer_ast::ty::TypePathSegment::new(*mangled);
                seg.generics = std::mem::take(&mut path.segments[0].generics);
                path.segments = vec![seg];
            }
        }
    }

    struct Rewriter {
        scope: std::collections::HashSet<String>,
    }
    impl VisitorMut for Rewriter {
        fn visit_expr(&mut self, expr: &mut Expr) {
            walk_expr_mut(self, expr);
            // `r.form_file(name)` is a request/multipart convenience that
            // composes the injected multipart parser, so it lowers to the
            // free wrapper `__gos_http_request_form_file(r, name)` rather
            // than a runtime method.
            if let ExprKind::MethodCall { name, args, .. } = &expr.kind
                && name.name.as_str() == "form_file"
                && args.len() == 1
            {
                rewrite_form_file_method(expr);
                return;
            }
            match &mut expr.kind {
                ExprKind::Call { callee, .. } => {
                    if let ExprKind::Path(path) = &mut callee.kind {
                        collapse_expr(path, &self.scope);
                    }
                }
                ExprKind::Path(path) => collapse_expr(path, &self.scope),
                ExprKind::Struct { path, .. } => collapse_expr(path, &self.scope),
                _ => {}
            }
        }
        fn visit_type(&mut self, ty: &mut Type) {
            walk_type_mut(self, ty);
            if let TypeKind::Path(tp) = &mut ty.kind {
                collapse_type(tp, &self.scope);
            }
        }
    }
    let imported = imported_wrapper_names(sf);
    let scope = stdlib_modules_in_scope(sf);
    Rewriter { scope }.visit_source_file(sf);
    if !imported.is_empty() {
        BareImportRewriter { imported }.visit_source_file(sf);
    }
}

/// Desugars `xs.sort_by_key(f)` / `xs.sort_by_key_desc(f)` method calls into
/// `xs.sort_by(|a, b| { let ka = f(a); let kb = f(b); cmp })`, where `cmp`
/// orders by the key with `<`. Because the key is compared with the `<`
/// operator (which works on scalars, strings, and tuples on every tier),
/// multi-key sorting via a tuple key - `xs.sort_by_key(|e| (e.a, e.b))` -
/// works without pinning the key to `i64`. Source-to-source, so it compiles
/// bit-identically on all tiers.
use gossamer_ast::VisitorMut as _;
use gossamer_ast::expr::Expr as SbkExpr;
use gossamer_ast::expr::ExprKind as SbkExprKind;
use gossamer_ast::pattern::Pattern as SbkPattern;
use gossamer_ast::stmt::Stmt as SbkStmt;
use gossamer_lex::Span as SbkSpan;

const SBK_KEY: &str = "__gos_sbk_key";
const SBK_A: &str = "__gos_sbk_a";
const SBK_B: &str = "__gos_sbk_b";
const SBK_KA: &str = "__gos_sbk_ka";
const SBK_KB: &str = "__gos_sbk_kb";

/// AST builder for the `sort_by_key` desugar. Holds a fresh-id counter -
/// the checker records inferred types per `NodeId`, so every synthesized
/// node needs a unique id rather than `DUMMY`.
struct SbkBuilder {
    next: u32,
}

impl SbkBuilder {
    fn id(&mut self) -> NodeId {
        let id = NodeId::from_raw(self.next);
        self.next += 1;
        id
    }
    fn expr(&mut self, span: SbkSpan, kind: SbkExprKind) -> SbkExpr {
        SbkExpr {
            id: self.id(),
            span,
            kind,
        }
    }
    fn path(&mut self, name: &str, span: SbkSpan) -> SbkExpr {
        self.expr(
            span,
            SbkExprKind::Path(gossamer_ast::expr::PathExpr {
                segments: vec![gossamer_ast::expr::PathSegment::new(name)],
            }),
        )
    }
    fn int_lit(&mut self, n: i64, span: SbkSpan) -> SbkExpr {
        self.expr(
            span,
            SbkExprKind::Literal(gossamer_ast::expr::Literal::Int(n.to_string())),
        )
    }
    fn ident_pat(&mut self, name: &str, span: SbkSpan) -> SbkPattern {
        SbkPattern {
            id: self.id(),
            span,
            kind: gossamer_ast::pattern::PatternKind::Ident {
                mutability: gossamer_ast::common::Mutability::Immutable,
                name: gossamer_ast::Ident::new(name),
                subpattern: None,
            },
        }
    }
    fn let_stmt(&mut self, name: &str, init: SbkExpr, span: SbkSpan) -> SbkStmt {
        let pattern = self.ident_pat(name, span);
        SbkStmt::new(
            self.id(),
            span,
            gossamer_ast::stmt::StmtKind::Let {
                pattern,
                ty: None,
                init: Some(Box::new(init)),
            },
        )
    }
    fn block(&mut self, stmts: Vec<SbkStmt>, tail: SbkExpr, span: SbkSpan) -> SbkExpr {
        self.expr(
            span,
            SbkExprKind::Block(gossamer_ast::expr::Block {
                stmts,
                tail: Some(Box::new(tail)),
                synthetic: true,
                kind: gossamer_ast::BlockKind::Plain,
            }),
        )
    }
    /// `path(left) < path(right)`.
    fn cmp_paths(&mut self, left: &str, right: &str, span: SbkSpan) -> SbkExpr {
        let left_e = self.path(left, span);
        let right_e = self.path(right, span);
        self.expr(
            span,
            SbkExprKind::Binary {
                op: gossamer_ast::common::BinaryOp::Lt,
                lhs: Box::new(left_e),
                rhs: Box::new(right_e),
            },
        )
    }
    fn key_call(&mut self, arg_name: &str, span: SbkSpan) -> SbkExpr {
        let callee = self.path(SBK_KEY, span);
        let arg = self.path(arg_name, span);
        self.expr(
            span,
            SbkExprKind::Call {
                callee: Box::new(callee),
                args: vec![arg],
            },
        )
    }
    /// Deep-clones `body` with fresh node ids, renaming references to the key
    /// closure's parameter `param` to `to`. Inlining the key body (rather than
    /// capturing the key closure and calling it) sidesteps a native-tier
    /// quirk where a comparator that captures another closure and returns an
    /// aggregate key misbehaves when invoked through the sort callback ABI.
    fn clone_inline(&mut self, body: &SbkExpr, param: &str, to: &str) -> SbkExpr {
        let mut cloned = body.clone();
        SbkRenum { d: self, param, to }.visit_expr(&mut cloned);
        cloned
    }
    /// Builds the `|a, b| { let ka = ...; let kb = ...; cmp }` comparator that
    /// orders elements by their key with `<` (flipped for `desc`).
    fn comparator(
        &mut self,
        first_key: SbkStmt,
        second_key: SbkStmt,
        desc: bool,
        span: SbkSpan,
    ) -> SbkExpr {
        use gossamer_ast::expr::ClosureParam;
        let lt_forward = self.cmp_paths(SBK_KA, SBK_KB, span);
        let lt_backward = self.cmp_paths(SBK_KB, SBK_KA, span);
        let (first, second) = if desc {
            (lt_backward, lt_forward)
        } else {
            (lt_forward, lt_backward)
        };
        let one = self.int_lit(1, span);
        let zero = self.int_lit(0, span);
        let neg = self.int_lit(-1, span);
        let then_one = self.block(vec![], one, span);
        let then_zero = self.block(vec![], zero, span);
        let then_neg = self.block(vec![], neg, span);
        let inner_if = self.expr(
            span,
            SbkExprKind::If {
                condition: Box::new(second),
                then_branch: Box::new(then_one),
                else_branch: Some(Box::new(then_zero)),
            },
        );
        let if_chain = self.expr(
            span,
            SbkExprKind::If {
                condition: Box::new(first),
                then_branch: Box::new(then_neg),
                else_branch: Some(Box::new(inner_if)),
            },
        );
        let cmp_body = self.block(vec![first_key, second_key], if_chain, span);
        let pat_a = self.ident_pat(SBK_A, span);
        let pat_b = self.ident_pat(SBK_B, span);
        self.expr(
            span,
            SbkExprKind::Closure {
                params: vec![
                    ClosureParam {
                        pattern: pat_a,
                        ty: None,
                    },
                    ClosureParam {
                        pattern: pat_b,
                        ty: None,
                    },
                ],
                ret: None,
                body: Box::new(cmp_body),
            },
        )
    }
    /// Builds the `|a, b| { ... }` comparator from an inline-able key `body`,
    /// comparing element-wise so a tuple key with `Reverse(x)` members can mix
    /// ascending and descending. For each element (or the whole key when it is
    /// not a tuple literal): if it is `Reverse(inner)` the element is ordered
    /// descending; `desc` flips every element. Each element value is inlined
    /// (the key param substituted for `a` / `b`) and ordered with `<`, which
    /// works on scalars, strings, and tuples on every tier.
    fn element_wise_comparator(
        &mut self,
        body: &SbkExpr,
        param: &str,
        desc: bool,
        span: SbkSpan,
    ) -> SbkExpr {
        use gossamer_ast::expr::ClosureParam;
        // Decompose a tuple-literal key into its elements; any other key is a
        // single element.
        let elems: Vec<SbkExpr> = match &body.kind {
            SbkExprKind::Tuple(es) => es.clone(),
            _ => vec![body.clone()],
        };
        let zero = self.int_lit(0, span);
        let mut chain = self.block(vec![], zero, span);
        for (i, elem) in elems.iter().enumerate().rev() {
            let (inner, reversed) = sbk_strip_reverse(elem);
            let flip = reversed ^ desc;
            let a_name = format!("__gos_sbk_a{i}");
            let b_name = format!("__gos_sbk_b{i}");
            let a_val = self.clone_inline(inner, param, SBK_A);
            let b_val = self.clone_inline(inner, param, SBK_B);
            let let_a = self.let_stmt(&a_name, a_val, span);
            let let_b = self.let_stmt(&b_name, b_val, span);
            // The "this element is less" direction: `a < b` ascending, or
            // `b < a` when this element is reversed.
            let (less, greater) = if flip {
                (
                    self.cmp_paths(&b_name, &a_name, span),
                    self.cmp_paths(&a_name, &b_name, span),
                )
            } else {
                (
                    self.cmp_paths(&a_name, &b_name, span),
                    self.cmp_paths(&b_name, &a_name, span),
                )
            };
            let neg = self.int_lit(-1, span);
            let one = self.int_lit(1, span);
            let then_neg = self.block(vec![], neg, span);
            let then_one = self.block(vec![], one, span);
            let greater_if = self.expr(
                span,
                SbkExprKind::If {
                    condition: Box::new(greater),
                    then_branch: Box::new(then_one),
                    else_branch: Some(Box::new(chain)),
                },
            );
            let less_if = self.expr(
                span,
                SbkExprKind::If {
                    condition: Box::new(less),
                    then_branch: Box::new(then_neg),
                    else_branch: Some(Box::new(greater_if)),
                },
            );
            chain = self.block(vec![let_a, let_b], less_if, span);
        }
        let pat_a = self.ident_pat(SBK_A, span);
        let pat_b = self.ident_pat(SBK_B, span);
        self.expr(
            span,
            SbkExprKind::Closure {
                params: vec![
                    ClosureParam {
                        pattern: pat_a,
                        ty: None,
                    },
                    ClosureParam {
                        pattern: pat_b,
                        ty: None,
                    },
                ],
                ret: None,
                body: Box::new(chain),
            },
        )
    }
    fn rewrite(&mut self, e: &mut SbkExpr, desc: bool) {
        let span = e.span;
        let SbkExprKind::MethodCall {
            receiver,
            name: written,
            name_span: written_span,
            mut args,
            ..
        } = std::mem::replace(&mut e.kind, SbkExprKind::Tuple(Vec::new()))
        else {
            return;
        };
        let key_fn = args.pop().expect("checked len == 1");
        // Inline a closure-literal key (`|e| body`) by substituting its single
        // ident param; non-closure keys (a fn name / variable) bind and call.
        let inline_param = sbk_inline_param(&key_fn);
        let (outer_stmts, comparator) = if let Some(param) = inline_param {
            let body = match &key_fn.kind {
                SbkExprKind::Closure { body, .. } => body.clone(),
                _ => unreachable!("inline_param implies a closure"),
            };
            // Build a Reverse-aware element-wise comparator straight from the
            // key body: a tuple key compares lexicographically, with each
            // `Reverse(x)` element ordered descending (and the whole key
            // reversed for `sort_by_key_desc`).
            (
                Vec::new(),
                self.element_wise_comparator(&body, &param, desc, span),
            )
        } else {
            let let_key = self.let_stmt(SBK_KEY, key_fn, span);
            let call_a = self.key_call(SBK_A, span);
            let call_b = self.key_call(SBK_B, span);
            let first_key = self.let_stmt(SBK_KA, call_a, span);
            let second_key = self.let_stmt(SBK_KB, call_b, span);
            (
                vec![let_key],
                self.comparator(first_key, second_key, desc, span),
            )
        };
        let sort_call = self.expr(
            span,
            SbkExprKind::MethodCall {
                receiver,
                name: gossamer_ast::Ident::new("sort_by"),
                // The name the source wrote, so a diagnostic about this call
                // reports the spelling the reader can find in the file.
                name_span: written_span,
                desugared_from: Some(written),
                generics: Vec::new(),
                args: vec![comparator],
            },
        );
        e.kind = SbkExprKind::Block(gossamer_ast::expr::Block {
            stmts: outer_stmts,
            tail: Some(Box::new(sort_call)),
            synthetic: true,
            kind: gossamer_ast::BlockKind::Plain,
        });
    }
}

impl gossamer_ast::VisitorMut for SbkBuilder {
    fn visit_expr(&mut self, e: &mut SbkExpr) {
        gossamer_ast::visitor::walk_expr_mut(self, e);
        let SbkExprKind::MethodCall { name, args, .. } = &e.kind else {
            return;
        };
        let desc = match name.name.as_str() {
            "sort_by_key" => false,
            "sort_by_key_desc" => true,
            _ => return,
        };
        if args.len() == 1 {
            self.rewrite(e, desc);
        }
    }
}

/// Renumbers a cloned key body's node ids and renames its parameter.
struct SbkRenum<'a> {
    d: &'a mut SbkBuilder,
    param: &'a str,
    to: &'a str,
}

impl gossamer_ast::VisitorMut for SbkRenum<'_> {
    fn visit_expr(&mut self, e: &mut SbkExpr) {
        e.id = self.d.id();
        if let SbkExprKind::Path(p) = &mut e.kind
            && p.segments.len() == 1
            && p.segments[0].name.name == self.param
        {
            p.segments[0].name = gossamer_ast::Ident::new(self.to);
        }
        gossamer_ast::visitor::walk_expr_mut(self, e);
    }
    fn visit_pattern(&mut self, p: &mut SbkPattern) {
        p.id = self.d.id();
        gossamer_ast::visitor::walk_pattern_mut(self, p);
    }
    fn visit_stmt(&mut self, s: &mut SbkStmt) {
        s.id = self.d.id();
        gossamer_ast::visitor::walk_stmt_mut(self, s);
    }
}

/// If `e` is a `Reverse(inner)` call (the sort-key descending marker), returns
/// `(inner, true)`; otherwise `(e, false)`. `Reverse` is recognized only here,
/// inside a `sort_by_key` key, where it is stripped before the body is inlined -
/// so it never needs to be a real constructible type.
fn sbk_strip_reverse(e: &SbkExpr) -> (&SbkExpr, bool) {
    if let SbkExprKind::Call { callee, args } = &e.kind
        && args.len() == 1
        && let SbkExprKind::Path(p) = &callee.kind
        && p.segments.len() == 1
        && p.segments[0].name.name == "Reverse"
    {
        return (&args[0], true);
    }
    (e, false)
}

/// The single ident parameter name of a closure-literal key, or `None`.
fn sbk_inline_param(key_fn: &SbkExpr) -> Option<String> {
    let SbkExprKind::Closure { params, .. } = &key_fn.kind else {
        return None;
    };
    if params.len() != 1 {
        return None;
    }
    match &params[0].pattern.kind {
        gossamer_ast::pattern::PatternKind::Ident {
            name,
            subpattern: None,
            ..
        } => Some(name.name.as_str().to_owned()),
        _ => None,
    }
}

/// Desugars `xs.sort_by_key(f)` / `xs.sort_by_key_desc(f)` into
/// `xs.sort_by(|a, b| { let ka = f(a); let kb = f(b); cmp })`, ordering by the
/// key with `<`. Because keys are compared with the `<` operator (which works
/// on scalars, strings, and tuples on every tier), multi-key sorting via a
/// tuple key works without pinning the key to `i64`. A closure-literal key is
/// inlined (its param substituted); other keys are bound and called.
/// Source-to-source, so it compiles bit-identically on all tiers.
pub fn desugar_sort_by_key(sf: &mut SourceFile) {
    use gossamer_ast::VisitorMut;
    let mut builder = SbkBuilder {
        next: sf.next_node_id,
    };
    builder.visit_source_file(sf);
    sf.next_node_id = builder.next;
}

/// Fills in the type argument for a bare `from_json(...)` call when the
/// surrounding `let` annotation makes it unambiguous, so the turbofish is
/// optional: `let u: User = from_json(&t)?` is rewritten to carry
/// `from_json::<User>`, and `let u: Result<User, E> = from_json(&t)` to the
/// same. Runs before `rewrite_serde_generic_calls`, which then mangles the
/// now-explicit turbofish like any other. Only `from_json` is inferred (its
/// type is the binding's; `to_json`'s type lives in its argument).
pub fn infer_serde_turbofish(sf: &mut SourceFile) {
    use gossamer_ast::expr::ExprKind;
    use gossamer_ast::stmt::{Stmt, StmtKind};
    use gossamer_ast::visitor::walk_stmt_mut;
    use gossamer_ast::{Type, VisitorMut};

    fn result_ok_type(ty: &Type) -> Option<&Type> {
        let TypeKind::Path(tp) = &ty.kind else {
            return None;
        };
        let seg = tp.segments.last()?;
        if seg.name.name != "Result" {
            return None;
        }
        match seg.generics.first()? {
            GenericArg::Type(t) => Some(t),
            GenericArg::Const(_) => None,
        }
    }

    struct Inferrer;
    impl VisitorMut for Inferrer {
        fn visit_stmt(&mut self, stmt: &mut Stmt) {
            walk_stmt_mut(self, stmt);
            let StmtKind::Let {
                ty: Some(ty),
                init: Some(init),
                ..
            } = &mut stmt.kind
            else {
                return;
            };
            // `from_json(..)?` exposes the binding type directly; bare
            // `from_json(..)` exposes it through the `Result` ok-arm.
            let (call, target): (&mut gossamer_ast::expr::Expr, Type) = match &mut init.kind {
                ExprKind::Try(inner) => (inner.as_mut(), ty.clone()),
                ExprKind::Call { .. } => match result_ok_type(ty) {
                    Some(t) => {
                        let t = t.clone();
                        (init.as_mut(), t)
                    }
                    None => return,
                },
                _ => return,
            };
            let ExprKind::Call { callee, .. } = &mut call.kind else {
                return;
            };
            let ExprKind::Path(path) = &mut callee.kind else {
                return;
            };
            if path.segments.len() != 1 {
                return;
            }
            let seg = &mut path.segments[0];
            if seg.name.name != "from_json" || !seg.generics.is_empty() {
                return;
            }
            if !matches!(target.kind, TypeKind::Path(_)) {
                return;
            }
            seg.generics.push(GenericArg::Type(target));
        }
    }
    Inferrer.visit_source_file(sf);
}

/// Rewrites the generic serde call surface - `to_json::<T>(v)`,
/// `from_json::<T>(s)`, and the toml/yaml variants - into calls to the
/// per-type free functions synthesized by [`synthesize_serde_impls`]
/// (`__gos_serde_<op>_<T>`). This is the single public spelling; there
/// are no `Type::to_json` methods.
pub fn rewrite_serde_generic_calls(sf: &mut SourceFile) {
    use gossamer_ast::VisitorMut;
    use gossamer_ast::expr::{Expr, ExprKind};
    use gossamer_ast::visitor::walk_expr_mut;

    struct Rewriter<'a> {
        symbols: &'a std::collections::HashMap<String, String>,
    }
    impl VisitorMut for Rewriter<'_> {
        fn visit_expr(&mut self, expr: &mut Expr) {
            // Rewrite inner expressions first so nested serde calls
            // (e.g. an argument that is itself `to_json::<U>(x)`) are
            // handled before the enclosing call.
            walk_expr_mut(self, expr);
            let ExprKind::Call { callee, .. } = &mut expr.kind else {
                return;
            };
            let ExprKind::Path(path) = &mut callee.kind else {
                return;
            };
            // Bare `from_json::<T>(s)` or the qualified spelling
            // `json::from_json::<T>(s)` (head must name the matching
            // format module); both collapse to the mangled free fn.
            // A format module's own typed spelling (`json::decode::<T>`,
            // `yaml::from_yaml::<T>`) names the same operation the bare
            // turbofish does, so it collapses to the same mangled fn. The
            // path is only rewritten once the call is known to carry the one
            // type argument that names which synthesized function it is.
            let qualified_op = match path.segments.len() {
                1 => None,
                2 => {
                    let head = path.segments[0].name.name.as_str();
                    let tail = path.segments[1].name.name.as_str();
                    match crate::autoderive::qualified_serde_op(head, tail) {
                        Some(op) => Some(op),
                        None => return,
                    }
                }
                _ => return,
            };
            let seg_idx = usize::from(qualified_op.is_some());
            let seg = &path.segments[seg_idx];
            let op = qualified_op.unwrap_or(seg.name.name.as_str());
            if !matches!(
                op,
                "to_json" | "from_json" | "to_toml" | "from_toml" | "to_yaml" | "from_yaml"
            ) || seg.generics.len() != 1
            {
                return;
            }
            let op = op.to_string();
            if seg_idx == 1 {
                path.segments.remove(0);
            }
            let seg = &mut path.segments[0];
            seg.name.name = op;
            let GenericArg::Type(ty) = &seg.generics[0] else {
                return;
            };
            let TypeKind::Path(tp) = &ty.kind else {
                return;
            };
            let written: Vec<&str> = tp
                .segments
                .iter()
                .map(|segment| segment.name.name.as_str())
                .filter(|segment| !matches!(*segment, "crate" | "self" | "super" | "root"))
                .collect();
            let Some(bare) = written.last().copied() else {
                return;
            };
            // Prefer the spelling as written; fall back to the leaf name,
            // which is unambiguous when only one module declares it.
            let symbol = self
                .symbols
                .get(&written.join("::"))
                .or_else(|| self.symbols.get(bare))
                .map_or_else(|| bare.to_string(), Clone::clone);
            seg.name.name = serde_fn(seg.name.name.as_str(), &symbol);
            seg.generics.clear();
        }
    }
    // How each type a turbofish can name maps to the symbol its synthesized
    // free functions carry: the qualified spelling the user wrote
    // (`a::Point`), the bare name when exactly one module declares it, and
    // any name a `use` brought into scope.
    let symbols = serde_symbol_index(sf);
    Rewriter { symbols: &symbols }.visit_source_file(sf);
}

/// Maps every spelling a turbofish may use for a user type onto the symbol
/// its synthesized serde functions carry. A bare name maps only when a
/// single module declares it; an ambiguous one is left out so the written
/// path (or an import) decides.
fn serde_symbol_index(sf: &SourceFile) -> std::collections::HashMap<String, String> {
    use std::collections::HashMap;

    let mut by_bare: HashMap<&str, Vec<String>> = HashMap::new();
    let mut out: HashMap<String, String> = HashMap::new();
    for (module, item) in flatten_items_with_modules(&sf.items) {
        let name = match &item.kind {
            ItemKind::Struct(decl) => &decl.name.name,
            ItemKind::Enum(decl) => &decl.name.name,
            _ => continue,
        };
        let ty = TyId::new(&module, name);
        out.insert(ty.path.clone(), ty.symbol.clone());
        by_bare.entry(name).or_default().push(ty.symbol);
    }
    for (bare, symbols) in by_bare {
        if let [only] = symbols.as_slice() {
            out.insert(bare.to_string(), only.clone());
        }
    }
    // A turbofish may spell a type through an alias. A transparent alias is
    // the type it names, and an opaque one serializes as its representation -
    // the rule its fields already follow - so both resolve to the target's
    // symbol. The chain bound keeps a cyclic alias from spinning here; the
    // checker is what reports the cycle (GT0024).
    let aliases = alias_targets(&sf.items);
    for name in aliases.keys() {
        let mut cursor = name.clone();
        for _ in 0..MAX_ALIAS_DEPTH {
            let Some(gossamer_ast::ty::TypeKind::Path(target)) =
                aliases.get(&cursor).map(|ty| &ty.kind)
            else {
                break;
            };
            let Some(leaf) = target.segments.last().map(|seg| seg.name.name.clone()) else {
                break;
            };
            if let Some(symbol) = out.get(&leaf).cloned() {
                out.entry(name.clone()).or_insert(symbol);
                break;
            }
            cursor = leaf;
        }
    }
    // A `use a::Point` (or `use a::{Point as P}`) makes the imported name
    // stand for that module's type at every turbofish in this file.
    for decl in &sf.uses {
        for (bound, target) in imported_type_paths(decl) {
            if let Some(symbol) = out.get(&target) {
                out.insert(bound, symbol.clone());
            }
        }
    }
    out
}

/// `(bound name, `::`-joined target path)` for each name a `use` brings in.
fn imported_type_paths(decl: &gossamer_ast::UseDecl) -> Vec<(String, String)> {
    let gossamer_ast::UseTarget::Module(path) = &decl.target else {
        return Vec::new();
    };
    let base: Vec<&str> = path
        .segments
        .iter()
        .map(|segment| segment.name.as_str())
        .filter(|segment| !matches!(*segment, "crate" | "self" | "super" | "root"))
        .collect();
    let Some(entries) = &decl.list else {
        let bound = decl
            .alias
            .as_ref()
            .map(|a| a.name.clone())
            .or_else(|| base.last().map(|s| (*s).to_string()));
        return bound
            .map(|bound| vec![(bound, base.join("::"))])
            .unwrap_or_default();
    };
    entries
        .iter()
        .map(|entry| {
            let mut full = base.clone();
            full.extend(entry.prefix.iter().map(|s| s.name.as_str()));
            full.push(entry.name.name.as_str());
            let bound = entry
                .alias
                .as_ref()
                .map_or_else(|| entry.name.name.clone(), |a| a.name.clone());
            (bound, full.join("::"))
        })
        .collect()
}

/// Adds `use std::json` and `use std::errors` to the parsed source
/// if it has synthesized impl blocks that depend on them. Idempotent
/// - checks for existing imports before inserting.
pub fn inject_synthetic_uses(sf: &mut SourceFile, file: FileId) {
    // `arena { ... }` and `cohort { }` desugar to `runtime::` calls;
    // make the module available without requiring an explicit import.
    if uses_runtime_regions(sf) && !already_imports(&sf.uses, &["std", "runtime"]) {
        sf.uses.push(UseDecl::simple(
            NodeId::DUMMY,
            Span::new(file, 0, 0),
            UseTarget::Module(ModulePath::from_names(["std", "runtime"])),
        ));
    }
    let has_synth = sf.items.iter().any(|item| {
        matches!(&item.kind, ItemKind::Fn(decl)
            if decl.name.name.starts_with("__gos_serde_")
                || decl.name.name.starts_with("__gos_"))
    });
    if !has_synth {
        return;
    }
    let dummy_span = Span::new(file, 0, 0);
    for segs in [
        // Canonical path (matches the manifest and docs); the bare
        // `std::json` spelling is an accepted alias but injecting it
        // alongside a user's `use std::encoding::json` made the two
        // look like rival modules.
        &["std", "encoding", "json"][..],
        &["std", "errors"][..],
        &["std", "encoding", "toml"][..],
        &["std", "encoding", "yaml"][..],
    ] {
        if !already_imports(&sf.uses, segs) {
            sf.uses.push(UseDecl::simple(
                NodeId::DUMMY,
                dummy_span,
                UseTarget::Module(ModulePath::from_names(segs.iter().copied())),
            ));
        }
    }
    // The `regex!` validation macro's synthesized `comptime fn` backer
    // calls `regex::compile`, so the regex module must be in scope.
    let has_regex_validator = sf.items.iter().any(
        |item| matches!(&item.kind, ItemKind::Fn(decl) if decl.name.name == "__gos_regex_validate"),
    );
    if has_regex_validator && !already_imports(&sf.uses, &["std", "regex"]) {
        sf.uses.push(UseDecl::simple(
            NodeId::DUMMY,
            dummy_span,
            UseTarget::Module(ModulePath::from_names(["std", "regex"])),
        ));
    }
    // The `__gos_http_*` request/response-security wrappers compose http,
    // crypto, encoding, bytes, strings, and net::url primitives by their
    // qualified paths, so those modules must be in scope.
    let has_http_sec = sf.items.iter().any(|item| {
        matches!(&item.kind, ItemKind::Fn(decl) if decl.name.name.starts_with("__gos_http_"))
    });
    if has_http_sec {
        for segs in [
            &["std", "http"][..],
            &["std", "strings"][..],
            &["std", "crypto"][..],
            &["std", "encoding"][..],
            &["std", "bytes"][..],
            &["std", "net", "url"][..],
        ] {
            if !already_imports(&sf.uses, segs) {
                sf.uses.push(UseDecl::simple(
                    NodeId::DUMMY,
                    dummy_span,
                    UseTarget::Module(ModulePath::from_names(segs.iter().copied())),
                ));
            }
        }
    }
}

/// True when any expression in the file calls a `runtime::` entry that
/// a block desugar emits: the `arena { ... }` region pair, or the
/// `cohort { }` push / join / pop trio.
fn uses_runtime_regions(sf: &SourceFile) -> bool {
    use gossamer_ast::Visitor;
    use gossamer_ast::expr::{Expr, ExprKind};
    struct Finder {
        found: bool,
    }
    impl Visitor for Finder {
        fn visit_expr(&mut self, expr: &Expr) {
            if self.found {
                return;
            }
            if let ExprKind::Path(p) = &expr.kind
                && p.segments.len() == 2
                && p.segments[0].name.name == "runtime"
                && matches!(
                    p.segments[1].name.name.as_str(),
                    "arena_push"
                        | "arena_pop"
                        | "cohort_push"
                        | "cohort_join"
                        | "cohort_pop"
                        | "cohort_cancelled"
                        | "cohort_cancel"
                )
            {
                self.found = true;
                return;
            }
            gossamer_ast::visitor::walk_expr(self, expr);
        }
    }
    let mut f = Finder { found: false };
    for item in &sf.items {
        gossamer_ast::visitor::walk_item(&mut f, item);
        if f.found {
            return true;
        }
    }
    false
}

fn already_imports(uses: &[UseDecl], segs: &[&str]) -> bool {
    // A use binds its LAST segment (or alias / brace-list entries):
    // `use std::encoding::json` and the synthesized `use std::json`
    // both bind `json`, and injecting the second produces a duplicate
    // -import diagnostic on perfectly valid user code. Compare the
    // bound name, not the full path.
    let bound = segs.last().copied().unwrap_or_default();
    uses.iter().any(|u| {
        if let Some(alias) = &u.alias {
            return alias.name == bound;
        }
        if let Some(list) = &u.list {
            return list.iter().any(|e| {
                e.alias
                    .as_ref()
                    .map_or(e.name.name == bound, |a| a.name == bound)
            });
        }
        match &u.target {
            UseTarget::Module(p) => p.segments.last().is_some_and(|s| s.name == bound),
            UseTarget::Project { id, module } => match module {
                Some(p) => p.segments.last().is_some_and(|s| s.name == bound),
                None => id == bound,
            },
        }
    })
}

fn synth_is_empty(synth: &str) -> bool {
    !synth.contains("__gos_serde_")
}
