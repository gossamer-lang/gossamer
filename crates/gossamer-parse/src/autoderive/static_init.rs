// Runs a heap-valued `static mut`'s initializer at entry so the cell holds it.

/// Prepends, to the entry function, an assignment of every `static mut`
/// declaration whose value is not a scalar constant.
///
/// A scalar cell carries its value in the global's own initializer. A cell
/// holding a container or a string names heap storage instead, which only a
/// running program can build, so the entry writes it before anything reads it.
pub(crate) fn initialize_heap_mut_statics(sf: &mut SourceFile) {
    let mut inits: Vec<(Vec<String>, Expr)> = Vec::new();
    collect_heap_mut_statics(&sf.items, &mut Vec::new(), &mut inits);
    if inits.is_empty() {
        return;
    }
    // A module's own static is private to it, so its build runs in a function
    // the module declares; the entry calls that instead of naming the cell.
    let mut next_id = sf.next_node_id;
    let module_inits = extract_module_inits(&mut inits);
    let mut calls: Vec<Vec<String>> = Vec::new();
    for (module, statics) in module_inits {
        let name = static_init_fn_name(&module);
        add_module_init_fn(&mut sf.items, &module, &name, statics, &mut next_id);
        let mut path = module.clone();
        path.push(name);
        calls.push(path);
    }
    sf.next_node_id = next_id;
    let Some(entry) = sf.items.iter_mut().find_map(|item| match &mut item.kind {
        ItemKind::Fn(decl) if decl.name.name == "main" => match decl.body.as_deref_mut() {
            Some(Expr {
                kind: ExprKind::Block(block),
                ..
            }) => Some(block),
            _ => None,
        },
        _ => None,
    }) else {
        return;
    };
    let mut next = sf.next_node_id;
    let mut id = || {
        let n = NodeId::from_raw(next);
        next = next.saturating_add(1);
        n
    };
    let mut prologue: Vec<gossamer_ast::stmt::Stmt> = Vec::with_capacity(inits.len());
    for path in calls {
        prologue.push(module_init_call_stmt(&path, &mut id));
    }
    for (path, value) in inits {
        let span = value.span;
        let target = Expr {
            id: id(),
            span,
            kind: ExprKind::Path(PathExpr {
                segments: path
                    .into_iter()
                    .map(|name| PathSegment {
                        name: Ident::new(name),
                        generics: Vec::new(),
                    })
                    .collect(),
            }),
        };
        prologue.push(gossamer_ast::stmt::Stmt {
            id: id(),
            span,
            kind: gossamer_ast::stmt::StmtKind::Expr {
                expr: Box::new(Expr {
                    id: id(),
                    span,
                    kind: ExprKind::Assign {
                        op: gossamer_ast::common::AssignOp::Assign,
                        place: Box::new(target),
                        value: Box::new(value),
                    },
                }),
                has_semi: false,
            },
        });
    }
    prologue.append(&mut entry.stmts);
    entry.stmts = prologue;
    sf.next_node_id = next;
}

/// Collects the module-qualified path and initializer of every `static mut`
/// the entry has to build at run time.
fn collect_heap_mut_statics(
    items: &[Item],
    module: &mut Vec<String>,
    out: &mut Vec<(Vec<String>, Expr)>,
) {
    for item in items {
        match &item.kind {
            ItemKind::Static(decl)
                if decl.mutability == gossamer_ast::common::Mutability::Mutable && !scalar_static_ty(&decl.ty) =>
            {
                let mut path = module.clone();
                path.push(decl.name.name.clone());
                out.push((path, decl.value.clone()));
            }
            ItemKind::Mod(mod_decl) => {
                if let ModBody::Inline(inner) = &mod_decl.body {
                    module.push(mod_decl.name.name.clone());
                    collect_heap_mut_statics(inner, module, out);
                    module.pop();
                }
            }
            _ => {}
        }
    }
}

/// Whether a static's declared type is one the global's own initializer can
/// carry: an integer, a float, a `bool`, or a `char`.
fn scalar_static_ty(ty: &gossamer_ast::Type) -> bool {
    let TypeKind::Path(path) = &ty.kind else {
        return false;
    };
    let [seg] = path.segments.as_slice() else {
        return false;
    };
    matches!(
        seg.name.name.as_str(),
        "bool"
            | "char"
            | "f32"
            | "f64"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "isize"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "usize"
    )
}

/// The name of the per-module initializer a module's heap statics are built in.
fn static_init_fn_name(module: &[String]) -> String {
    format!("__gos_static_init_{}", module.join("_"))
}

/// One module's heap statics: the module path and its `(name, initializer)`
/// pairs.
type ModuleStaticInits = (Vec<String>, Vec<(String, Expr)>);

/// Splits the module-scoped entries out of `inits`, grouped by module.
fn extract_module_inits(inits: &mut Vec<(Vec<String>, Expr)>) -> Vec<ModuleStaticInits> {
    let mut grouped: Vec<ModuleStaticInits> = Vec::new();
    let mut root: Vec<(Vec<String>, Expr)> = Vec::new();
    for (path, value) in std::mem::take(inits) {
        let Some((name, module)) = path.split_last() else {
            continue;
        };
        if module.is_empty() {
            root.push((path.clone(), value));
            continue;
        }
        let module = module.to_vec();
        match grouped.iter_mut().find(|(m, _)| *m == module) {
            Some((_, entries)) => entries.push((name.clone(), value)),
            None => grouped.push((module, vec![(name.clone(), value)])),
        }
    }
    *inits = root;
    grouped
}

/// Appends `pub fn <name>() { STATIC = <init> ... }` to the module at `module`.
fn add_module_init_fn(
    items: &mut Vec<Item>,
    module: &[String],
    name: &str,
    statics: Vec<(String, Expr)>,
    next: &mut u32,
) {
    let Some(target) = module_items_mut(items, module) else {
        return;
    };
    let mut id = || {
        let n = NodeId::from_raw(*next);
        *next = next.saturating_add(1);
        n
    };
    let span = statics
        .first()
        .map_or_else(Span::default, |(_, value)| value.span);
    let stmts: Vec<gossamer_ast::stmt::Stmt> = statics
        .into_iter()
        .map(|(static_name, value)| {
            let span = value.span;
            let place = Expr {
                id: id(),
                span,
                kind: ExprKind::Path(PathExpr {
                    segments: vec![PathSegment {
                        name: Ident::new(static_name),
                        generics: Vec::new(),
                    }],
                }),
            };
            gossamer_ast::stmt::Stmt {
                id: id(),
                span,
                kind: gossamer_ast::stmt::StmtKind::Expr {
                    expr: Box::new(Expr {
                        id: id(),
                        span,
                        kind: ExprKind::Assign {
                            op: gossamer_ast::common::AssignOp::Assign,
                            place: Box::new(place),
                            value: Box::new(value),
                        },
                    }),
                    has_semi: false,
                },
            }
        })
        .collect();
    let body = Expr {
        id: id(),
        span,
        kind: ExprKind::Block(gossamer_ast::expr::Block {
            stmts,
            tail: None,
            synthetic: true,
            kind: gossamer_ast::BlockKind::Plain,
        }),
    };
    let decl = gossamer_ast::items::FnDecl {
        attrs: gossamer_ast::Attrs::default(),
        span,
        is_unsafe: false,
        is_comptime: false,
        visibility: gossamer_ast::Visibility::Public,
        name: Ident::new(name.to_string()),
        generics: gossamer_ast::Generics::default(),
        params: Vec::new(),
        ret: None,
        where_clause: gossamer_ast::WhereClause::default(),
        body: Some(Box::new(body)),
    };
    target.push(Item {
        id: id(),
        span,
        attrs: gossamer_ast::Attrs::default(),
        visibility: gossamer_ast::Visibility::Public,
        kind: ItemKind::Fn(decl),
    });
}

/// The item list of the inline module at `path`, if the program declares it.
fn module_items_mut<'a>(items: &'a mut Vec<Item>, path: &[String]) -> Option<&'a mut Vec<Item>> {
    let Some((head, rest)) = path.split_first() else {
        return Some(items);
    };
    for item in items.iter_mut() {
        if let ItemKind::Mod(decl) = &mut item.kind
            && decl.name.name == *head
            && let ModBody::Inline(inner) = &mut decl.body
        {
            return module_items_mut(inner, rest);
        }
    }
    None
}

/// `mod::__gos_static_init_mod()` as a statement.
fn module_init_call_stmt(
    path: &[String],
    id: &mut impl FnMut() -> NodeId,
) -> gossamer_ast::stmt::Stmt {
    let span = Span::default();
    let callee = Expr {
        id: id(),
        span,
        kind: ExprKind::Path(PathExpr {
            segments: path
                .iter()
                .map(|name| PathSegment {
                    name: Ident::new(name.clone()),
                    generics: Vec::new(),
                })
                .collect(),
        }),
    };
    gossamer_ast::stmt::Stmt {
        id: id(),
        span,
        kind: gossamer_ast::stmt::StmtKind::Expr {
            expr: Box::new(Expr {
                id: id(),
                span,
                kind: ExprKind::Call {
                    callee: Box::new(callee),
                    args: Vec::new(),
                },
            }),
            has_semi: false,
        },
    }
}
