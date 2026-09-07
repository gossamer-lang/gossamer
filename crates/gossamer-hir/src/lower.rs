//! AST → HIR lowering.

#![forbid(unsafe_code)]

use gossamer_ast::{
    ArrayExpr as AstArrayExpr, AssignOp, BINARY_SEARCH_PREFIX, BinaryOp as AstBinOp,
    Block as AstBlock, ClosureParam as AstClosureParam, EnumDecl, Expr as AstExpr,
    ExprKind as AstExprKind, FieldPattern as AstFieldPat, FnDecl as AstFnDecl,
    FnParam as AstFnParam, Ident, ImplDecl, ImplItem, Item as AstItem, ItemKind as AstItemKind,
    Literal as AstLiteral, MatchArm, Mutability, NodeId, PARTITION_POINT_PREFIX, Pattern as AstPat,
    PatternKind as AstPatKind, STRUCTURAL_COMPARATOR_PREFIX, SourceFile, Stmt as AstStmt,
    StmtKind as AstStmtKind, StructDecl, TraitDecl, TraitItem, Type as AstType,
    USER_COMPARATOR_PREFIX, UnaryOp,
};
use gossamer_lex::Span;
use gossamer_resolve::{Resolution, Resolutions};
use gossamer_types::{Ty, TyCtxt, TypeTable};

use crate::ids::{HirId, HirIdGenerator};
use crate::tree::{
    FnOrigin, HirAdt, HirAdtKind, HirArrayExpr, HirBinaryOp, HirBlock, HirBody, HirConst, HirExpr,
    HirExprKind, HirFieldPat, HirFn, HirImpl, HirItem, HirItemKind, HirLiteral, HirMatchArm,
    HirParam, HirPat, HirPatKind, HirProgram, HirStatic, HirStmt, HirStmtKind, HirTrait,
    HirUnaryOp,
};

const HASH_SET_DEF_LOCAL: u32 = u32::MAX - 7;
const BTREE_SET_DEF_LOCAL: u32 = u32::MAX - 18;

/// Lowers a resolved AST source file into HIR. The provided type table
/// annotates expression nodes with their inferred types; entries
/// missing from the table default to `TyCtxt::error_ty()`.
#[must_use]
pub fn lower_source_file(
    source: &SourceFile,
    resolutions: &Resolutions,
    table: &TypeTable,
    tcx: &mut TyCtxt,
) -> HirProgram {
    let mut module_fn_paths = std::collections::HashMap::new();
    collect_module_fn_paths(
        resolutions,
        &source.items,
        &mut Vec::new(),
        &mut module_fn_paths,
    );
    collect_nested_item_paths(resolutions, source, &mut module_fn_paths);
    let mut module_impl_fns = std::collections::HashSet::new();
    collect_module_impl_fns(&source.items, &mut Vec::new(), &mut module_impl_fns);
    let mut module_type_names = std::collections::HashMap::new();
    collect_module_type_names(
        resolutions,
        &source.items,
        &mut Vec::new(),
        &mut module_type_names,
    );
    let mut lowerer = Lowerer {
        resolutions,
        table,
        tcx,
        ids: HirIdGenerator::new(),
        recursion_depth: 0,
        current_fn_ret_ty: None,
        import_targets: collect_import_targets(&source.uses),
        ctor_arity: collect_ctor_arities(&source.items),
        struct_fields: collect_struct_fields(&source.items),
        unit_structs: collect_unit_structs(&source.items),
        const_literals: collect_const_literals(&source.items),
        dependency_modules: collect_dependency_modules(&source.items),
        module_fn_paths,
        module_impl_fns,
        module_type_names,
        current_module: Vec::new(),
        user_comparators: collect_user_comparators(&source.items),
        promoted_items: Vec::new(),
    };
    let mut items = Vec::new();
    let mut module_path: Vec<String> = Vec::new();
    lower_items(&mut lowerer, &source.items, &mut items, &mut module_path);
    items.append(&mut lowerer.promoted_items);
    let mut program = HirProgram { items };
    // Fuse `iter::` range pipelines into loops before returning, so every
    // consumer (the bytecode VM, and the native path that lifts closures
    // next) sees the same fused HIR. Runs before closure lifting, so
    // stage/terminal closures are still inline and can be spliced in.
    crate::fuse::fuse_iter_pipelines(&mut program, &mut *lowerer.tcx, &mut lowerer.ids);
    program
}

/// The comparator-taking spelling of an ordering call written bare, or
/// `None` for a call that names no order.
fn comparator_ordering_form(method: &str) -> Option<&'static str> {
    match method {
        "sort" => Some("sort_by"),
        "min" => Some("min_by"),
        "max" => Some("max_by"),
        _ => None,
    }
}

/// Names of the comparator functions the autoderive pass emitted: one per
/// ordered type, under the prefix that says whether the source wrote the
/// order (`__gos_cmp_`) or the compiler synthesized it (`__gos_ord_`).
fn collect_user_comparators(items: &[AstItem]) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    for item in items {
        match &item.kind {
            AstItemKind::Fn(decl)
                if decl.name.name.starts_with(USER_COMPARATOR_PREFIX)
                    || decl.name.name.starts_with(STRUCTURAL_COMPARATOR_PREFIX) =>
            {
                out.insert(decl.name.name.clone());
            }
            AstItemKind::Mod(decl) => {
                if let gossamer_ast::ModBody::Inline(inner) = &decl.body {
                    out.extend(collect_user_comparators(inner));
                }
            }
            _ => {}
        }
    }
    out
}

/// Gives every block-local function or struct a globally unique backend symbol.
///
/// Nested functions are ordinary non-capturing items, not closures. HIR keeps
/// the item statement for lexical structure while also promoting a renamed
/// copy into the program item list so the VM and native backends compile it
/// like any other function. References are rewritten by `DefId`, preserving
/// block scope even when separate blocks reuse the same source name.
fn collect_nested_item_paths(
    resolutions: &Resolutions,
    source: &SourceFile,
    out: &mut std::collections::HashMap<gossamer_resolve::DefId, Vec<Ident>>,
) {
    struct Collector<'a> {
        resolutions: &'a Resolutions,
        out: &'a mut std::collections::HashMap<gossamer_resolve::DefId, Vec<Ident>>,
    }

    impl gossamer_ast::Visitor for Collector<'_> {
        fn visit_stmt(&mut self, stmt: &AstStmt) {
            if let AstStmtKind::Item(item) = &stmt.kind
                && let Some(def) = self.resolutions.definition_of(item.id)
            {
                let name = match &item.kind {
                    AstItemKind::Fn(decl) => Some(&decl.name.name),
                    AstItemKind::Struct(decl) => Some(&decl.name.name),
                    _ => None,
                };
                if let Some(name) = name {
                    self.out.insert(
                        def,
                        vec![Ident::new(format!("__gos_nested_{}_{}", def.local, name))],
                    );
                }
            }
            gossamer_ast::visitor::walk_stmt(self, stmt);
        }
    }

    gossamer_ast::Visitor::visit_source_file(&mut Collector { resolutions, out }, source);
}

/// Flattens items in source order, descending into inline modules so
/// that `#[test]`-annotated functions inside `mod tests { ... }` reach
/// HIR (and thus the interpreter + test runner) the same way they
/// would if declared at the top level. `module_path` tracks the
/// enclosing inline-module names so each lowered item carries the
/// path it was declared under - loaders use it to register both the
/// bare name and the `mod1::mod2::item` qualified key.
fn lower_items(
    lowerer: &mut Lowerer<'_>,
    items: &[AstItem],
    out: &mut Vec<HirItem>,
    module_path: &mut Vec<String>,
) {
    for item in items {
        if !gossamer_resolve::item_is_active(&item.attrs) {
            continue;
        }
        if let AstItemKind::Mod(decl) = &item.kind {
            if let gossamer_ast::ModBody::Inline(inner) = &decl.body {
                module_path.push(decl.name.name.clone());
                lower_items(lowerer, inner, out, module_path);
                module_path.pop();
            }
            continue;
        }
        if let Some(lowered) = lowerer.lower_item(item, module_path) {
            out.push(lowered);
        }
    }
}

/// Maps every inline-module function's `DefId` to its canonical
/// `mod1::mod2::name` path segments. Path references to these defs
/// (bare in-module calls included) rewrite to the canonical spelling
/// so name-keyed dispatch on every tier agrees with the qualified
/// definition symbol - two modules may then define the same function
/// name without colliding.
/// Collects the qualified name (`lib::P::new`) of every associated
/// function declared by an `impl` inside an inline module. Below HIR
/// these bodies are keyed by that spelling, so a bare `P::new` written
/// inside the module has to be respelled to reach them.
fn collect_module_impl_fns(
    items: &[AstItem],
    module_path: &mut Vec<String>,
    out: &mut std::collections::HashSet<String>,
) {
    for item in items {
        if !gossamer_resolve::item_is_active(&item.attrs) {
            continue;
        }
        match &item.kind {
            AstItemKind::Mod(decl) => {
                if let gossamer_ast::ModBody::Inline(inner) = &decl.body {
                    module_path.push(decl.name.name.clone());
                    collect_module_impl_fns(inner, module_path, out);
                    module_path.pop();
                }
            }
            AstItemKind::Impl(decl) if !module_path.is_empty() => {
                let gossamer_ast::TypeKind::Path(tp) = &decl.self_ty.kind else {
                    continue;
                };
                let Some(owner) = tp.segments.last() else {
                    continue;
                };
                for impl_item in &decl.items {
                    if let gossamer_ast::ImplItem::Fn(fn_decl) = impl_item {
                        out.insert(format!(
                            "{}::{}::{}",
                            module_path.join("::"),
                            owner.name.name,
                            fn_decl.name.name
                        ));
                    }
                }
            }
            // An enum's variant constructors are registered under the
            // enum's module-qualified identity too, and a `Enum::Variant`
            // path written inside the module needs the same anchoring an
            // associated function does.
            AstItemKind::Enum(decl) if !module_path.is_empty() => {
                for variant in &decl.variants {
                    out.insert(format!(
                        "{}::{}::{}",
                        module_path.join("::"),
                        decl.name.name,
                        variant.name.name
                    ));
                }
            }
            // A module's constants and statics are reached by the same
            // module-relative path, so they anchor the same way.
            AstItemKind::Const(decl) if !module_path.is_empty() => {
                out.insert(format!("{}::{}", module_path.join("::"), decl.name.name));
            }
            AstItemKind::Static(decl) if !module_path.is_empty() => {
                out.insert(format!("{}::{}", module_path.join("::"), decl.name.name));
            }
            _ => {}
        }
    }
}

fn collect_module_fn_paths(
    resolutions: &Resolutions,
    items: &[AstItem],
    module_path: &mut Vec<Ident>,
    out: &mut std::collections::HashMap<gossamer_resolve::DefId, Vec<Ident>>,
) {
    for item in items {
        if !gossamer_resolve::item_is_active(&item.attrs) {
            continue;
        }
        match &item.kind {
            AstItemKind::Mod(decl) => {
                if let gossamer_ast::ModBody::Inline(inner) = &decl.body {
                    module_path.push(decl.name.clone());
                    collect_module_fn_paths(resolutions, inner, module_path, out);
                    module_path.pop();
                }
            }
            AstItemKind::Fn(decl) if !module_path.is_empty() => {
                if let Some(def) = resolutions.definition_of(item.id) {
                    let mut segs = module_path.clone();
                    segs.push(decl.name.clone());
                    out.insert(def, segs);
                }
            }
            _ => {}
        }
    }
}

/// Collects the field count of every tuple struct and tuple-variant
/// constructor (by bare name), descending into inline modules. Drives
/// `..`-rest expansion in tuple-variant patterns.
/// Records the qualified identity (`a::Point`) of every struct and enum
/// declared inside an inline module, mirroring what the type checker
/// registers as the type's name.
/// The name an item is identified by below HIR: bare at the unit root,
/// prefixed by its containing modules otherwise.
fn qualified_item_name(module_path: &[String], name: &str) -> String {
    if module_path.is_empty() {
        return name.to_string();
    }
    format!("{}::{name}", module_path.join("::"))
}

fn collect_module_type_names(
    resolutions: &Resolutions,
    items: &[AstItem],
    module_path: &mut Vec<String>,
    out: &mut std::collections::HashMap<gossamer_resolve::DefId, String>,
) {
    for item in items {
        if !gossamer_resolve::item_is_active(&item.attrs) {
            continue;
        }
        let named = match &item.kind {
            AstItemKind::Struct(decl) => Some(&decl.name.name),
            AstItemKind::Enum(decl) => Some(&decl.name.name),
            AstItemKind::Mod(decl) => {
                if let gossamer_ast::ModBody::Inline(inner) = &decl.body {
                    module_path.push(decl.name.name.clone());
                    collect_module_type_names(resolutions, inner, module_path, out);
                    module_path.pop();
                }
                None
            }
            _ => None,
        };
        if let Some(name) = named
            && !module_path.is_empty()
            && let Some(def) = resolutions.definition_of(item.id)
        {
            out.insert(def, format!("{}::{name}", module_path.join("::")));
        }
    }
}

fn collect_ctor_arities(items: &[AstItem]) -> std::collections::HashMap<String, usize> {
    let mut map = std::collections::HashMap::new();
    collect_ctor_arities_into(items, &mut map);
    map
}

fn collect_ctor_arities_into(
    items: &[AstItem],
    map: &mut std::collections::HashMap<String, usize>,
) {
    for item in items {
        match &item.kind {
            // Tuple structs are modelled as named fields ("0".."N-1") and their
            // patterns are rewritten to struct form, so `..` rest expansion for
            // them belongs to that rewrite, not here - only enum tuple variants
            // reach the positional `Variant` matcher this drives.
            AstItemKind::Enum(decl) => {
                for v in &decl.variants {
                    if let gossamer_ast::StructBody::Tuple(fields) = &v.body {
                        map.insert(v.name.name.clone(), fields.len());
                    }
                }
            }
            AstItemKind::Mod(decl) => {
                if let gossamer_ast::ModBody::Inline(inner) = &decl.body {
                    collect_ctor_arities_into(inner, map);
                }
            }
            _ => {}
        }
    }
}

/// Collects source-order field names for every struct by bare name. Tuple
/// structs use their synthetic positional field names, `"0".."N-1"`.
fn collect_struct_fields(items: &[AstItem]) -> std::collections::HashMap<String, Vec<String>> {
    let mut map = std::collections::HashMap::new();
    collect_struct_fields_into(items, &mut map);
    map
}

/// Literal value of every `const NAME: T = <literal>` the file declares,
/// keyed by name, including inside inline modules.
///
/// A pattern must be a compile-time constant, so a `const` named in one
/// stands for its value. Only a literal initializer (optionally negated)
/// is collected; a computed one keeps its path form and is reported.
/// Names of the modules a `path = "..."` dependency was inlined under.
///
/// A path written inside one is relative to that package, so a
/// `crate::`-rooted path there names the dependency's own root rather than
/// the consuming package's.
fn collect_dependency_modules(items: &[AstItem]) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    for item in items {
        if let AstItemKind::Mod(decl) = &item.kind
            && item
                .attrs
                .outer
                .iter()
                .any(|attr| attr.string_argument("dependency").is_some())
        {
            out.insert(decl.name.name.clone());
        }
    }
    out
}

fn collect_const_literals(items: &[AstItem]) -> std::collections::HashMap<String, HirLiteral> {
    fn literal_of(expr: &AstExpr) -> Option<HirLiteral> {
        match &expr.kind {
            AstExprKind::Literal(lit) => Some(lower_literal(lit)),
            AstExprKind::Unary {
                op: UnaryOp::Neg,
                operand,
            } => match literal_of(operand)? {
                HirLiteral::Int(text) => Some(HirLiteral::Int(format!("-{text}"))),
                HirLiteral::Float(text) => Some(HirLiteral::Float(format!("-{text}"))),
                _ => None,
            },
            _ => None,
        }
    }
    fn visit(items: &[AstItem], out: &mut std::collections::HashMap<String, HirLiteral>) {
        for item in items {
            match &item.kind {
                AstItemKind::Const(decl) => {
                    if let Some(lit) = literal_of(&decl.value) {
                        out.insert(decl.name.name.clone(), lit);
                    }
                }
                AstItemKind::Mod(decl) => {
                    if let gossamer_ast::ModBody::Inline(inner) = &decl.body {
                        visit(inner, out);
                    }
                }
                _ => {}
            }
        }
    }
    let mut out = std::collections::HashMap::new();
    visit(items, &mut out);
    out
}

fn collect_unit_structs(items: &[AstItem]) -> std::collections::HashSet<String> {
    fn visit(items: &[AstItem], out: &mut std::collections::HashSet<String>) {
        for item in items {
            match &item.kind {
                AstItemKind::Struct(decl)
                    if matches!(decl.body, gossamer_ast::StructBody::Unit)
                        || matches!(
                            &decl.body,
                            gossamer_ast::StructBody::Named(fields) if fields.is_empty()
                        ) =>
                {
                    out.insert(decl.name.name.clone());
                }
                AstItemKind::Mod(decl) => {
                    if let gossamer_ast::ModBody::Inline(inner) = &decl.body {
                        visit(inner, out);
                    }
                }
                _ => {}
            }
        }
    }
    let mut structs = std::collections::HashSet::new();
    visit(items, &mut structs);
    structs
}

fn collect_struct_fields_into(
    items: &[AstItem],
    map: &mut std::collections::HashMap<String, Vec<String>>,
) {
    for item in items {
        match &item.kind {
            AstItemKind::Struct(decl) => {
                let fields = match &decl.body {
                    gossamer_ast::StructBody::Named(fields) => {
                        fields.iter().map(|field| field.name.name.clone()).collect()
                    }
                    gossamer_ast::StructBody::Tuple(fields) => {
                        (0..fields.len()).map(|idx| idx.to_string()).collect()
                    }
                    gossamer_ast::StructBody::Unit => Vec::new(),
                };
                map.insert(decl.name.name.clone(), fields);
            }
            AstItemKind::Mod(decl) => {
                if let gossamer_ast::ModBody::Inline(inner) = &decl.body {
                    collect_struct_fields_into(inner, map);
                }
            }
            _ => {}
        }
    }
}

fn struct_literal_positional_index(name: &str) -> Option<usize> {
    let idx = name.parse::<usize>().ok()?;
    if idx.to_string() == name {
        Some(idx)
    } else {
        None
    }
}

/// Builds the per-`use` map of bound name → full target path consumed
/// by `lower_path_expr`'s imported-binding expansion. One declaration
/// can bind several names (`use m::{a, b as c}`), so entries carry the
/// bound spelling (alias when present) alongside the full segments.
fn collect_import_targets(
    uses: &[gossamer_ast::UseDecl],
) -> std::collections::HashMap<NodeId, Vec<(String, Vec<Ident>)>> {
    let mut map = std::collections::HashMap::new();
    for use_decl in uses {
        let gossamer_ast::UseTarget::Module(path) = &use_decl.target else {
            continue;
        };
        let base: Vec<Ident> = path.segments.clone();
        let mut entries: Vec<(String, Vec<Ident>)> = Vec::new();
        if let Some(list) = &use_decl.list {
            for entry in list {
                let bound = entry.alias.as_ref().unwrap_or(&entry.name).name.clone();
                let mut full = base.clone();
                full.extend(entry.prefix.iter().cloned());
                full.push(entry.name.clone());
                entries.push((bound, full));
            }
        } else {
            let bound = use_decl.alias.as_ref().map_or_else(
                || base.last().map(|s| s.name.clone()),
                |alias| Some(alias.name.clone()),
            );
            if let Some(bound) = bound {
                entries.push((bound, base.clone()));
            }
        }
        if !entries.is_empty() {
            map.insert(use_decl.id, entries);
        }
    }
    map
}

/// Hard limit on HIR-lowering recursion depth. Mirrors the parser /
/// type-checker guards and stops adversarial input (or front-end bugs
/// that produce one) from blowing the C stack during AST→HIR lowering.
const RECURSION_LIMIT: u32 = 256;

/// Whether `?` should desugar to the `Option`-shaped propagator
/// (`Some(v) => v, None => return None`) or the `Result`-shaped
/// propagator (`Ok(v) => v, Err(e) => return Err(e)`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum TryKind {
    Option,
    Result,
}

/// How an `.into()` involving an opaque nominal alias lowers.
#[derive(Clone, Debug, PartialEq, Eq)]
enum NominalInto {
    /// The alias and the other side share one representation, so the
    /// conversion is the value itself.
    Identity,
    /// The pair needs the target's `From` impl, named by the alias.
    From(String),
}

struct Lowerer<'a> {
    resolutions: &'a Resolutions,
    table: &'a TypeTable,
    tcx: &'a mut TyCtxt,
    ids: HirIdGenerator,
    /// Running depth of recursive entries into `lower_expr` /
    /// `lower_pat`. Reaching the cap returns a placeholder node so
    /// the rest of lowering can continue with a self-consistent tree.
    recursion_depth: u32,
    /// Declared return type of the function whose body is currently
    /// being lowered. Read by `lower_try` so the `?` desugar can
    /// detect a mismatch between the inner expression's `Err` type
    /// and the enclosing function's `Err` type, and emit an
    /// automatic conversion (`errors::new(__try_err)` etc.) so
    /// `?` propagation works across different error types - the
    /// SPEC §4.5 `E: Into<E2>` semantic.
    current_fn_ret_ty: Option<gossamer_types::Ty>,
    /// Per-`use`-declaration map of bound name → full target path,
    /// keyed by the declaration's `NodeId`. Read by `lower_path_expr`
    /// to expand a single-segment imported name to its qualified
    /// path when it targets a `[rust-bindings]` item.
    import_targets: std::collections::HashMap<NodeId, Vec<(String, Vec<Ident>)>>,
    /// Qualified identity of each struct / enum declared inside a module.
    module_type_names: std::collections::HashMap<gossamer_resolve::DefId, String>,
    /// Field count of every tuple struct and tuple-variant constructor,
    /// keyed by its bare name. Lets a `..` rest in a tuple-variant pattern
    /// (`E::C(..)`) expand to the right number of wildcards, so it matches a
    /// multi-field variant rather than only a single-field one.
    ctor_arity: std::collections::HashMap<String, usize>,
    /// Source-order field names for struct literals. Named structs can be
    /// initialized positionally inside braces, and those temporary positions
    /// are rewritten to real field names before MIR lowering.
    struct_fields: std::collections::HashMap<String, Vec<String>>,
    unit_structs: std::collections::HashSet<String>,
    /// Canonical `mod::name` segments of every inline-module function,
    /// keyed by `DefId`. Path references rewrite to this spelling so a
    /// bare in-module call names the module's own item, not whichever
    /// same-named sibling registered a flat global last.
    module_fn_paths: std::collections::HashMap<gossamer_resolve::DefId, Vec<Ident>>,
    /// Qualified names of every inline-module `impl`'s associated
    /// functions and every inline-module enum's variant constructors, for
    /// respelling a `Type::assoc` or `Enum::Variant` path written inside
    /// the module that declares it.
    module_impl_fns: std::collections::HashSet<String>,
    /// Literal value of each `const` the file declares, so a constant
    /// named in a pattern matches its value.
    const_literals: std::collections::HashMap<String, HirLiteral>,
    /// Modules an inlined dependency's source sits under.
    dependency_modules: std::collections::HashSet<String>,
    /// Module whose items are currently being lowered.
    current_module: Vec<String>,
    /// Comparator functions the autoderive pass emitted, one per type
    /// whose source supplies its own `cmp`. An ordering call on such an
    /// element names one of these rather than the structural order.
    user_comparators: std::collections::HashSet<String>,
    promoted_items: Vec<HirItem>,
}

impl Lowerer<'_> {
    fn fresh(&mut self) -> HirId {
        self.ids.next()
    }

    /// The `impl` block a method call resolves to, as the checker recorded it.
    fn method_owner_of(&self, node: NodeId) -> Option<Ident> {
        self.table.method_owner(node).map(Ident::new)
    }

    fn ty_of(&mut self, node: NodeId) -> gossamer_types::Ty {
        // `Range<T>` is a type-layer spelling of `Iterator<T>`; lowering and
        // every backend below it know only the latter.
        let ty = self.table.get(node).unwrap_or_else(|| self.tcx.error_ty());
        gossamer_types::normalize_for_lowering(self.tcx, ty)
    }

    /// How `.into()` on `receiver` producing `result` should lower when an
    /// opaque alias is involved, or `None` when neither side is one and the
    /// ordinary routing applies.
    ///
    /// The decision is made here because the erasure that follows removes
    /// the distinction it depends on: below this point both sides are the
    /// representation, and the alias's name - which keys its impl - is gone.
    fn nominal_into_route(&mut self, receiver: NodeId, result: NodeId) -> Option<NominalInto> {
        let (Some(recv), Some(res)) = (self.table.get(receiver), self.table.get(result)) else {
            return None;
        };
        // A method receiver reaches here behind whatever reference layers
        // the call site introduced (`self` in an inherent impl is `&Alias`).
        let mut recv = recv;
        while let Some(gossamer_types::TyKind::Ref { inner, .. }) = self.tcx.kind(recv) {
            recv = *inner;
        }
        let nominal_name = |tcx: &gossamer_types::TyCtxt, ty| match tcx.kind(ty) {
            Some(gossamer_types::TyKind::Nominal { def, repr }) => Some((*def, *repr)),
            _ => None,
        };
        let recv_nominal = nominal_name(self.tcx, recv);
        let res_nominal = nominal_name(self.tcx, res);
        if recv_nominal.is_none() && res_nominal.is_none() {
            return None;
        }
        if recv == res
            || recv_nominal.is_some_and(|(_, repr)| repr == res)
            || res_nominal.is_some_and(|(_, repr)| repr == recv)
        {
            return Some(NominalInto::Identity);
        }
        let (def, _) = res_nominal?;
        let name = self.tcx.def_name(def)?.to_string();
        Some(NominalInto::From(name))
    }

    /// Name of the opaque alias a method receiver is declared as, when it is
    /// one. Its impl methods are filed under this name, and the erasure that
    /// follows lowering replaces the type with its representation.
    /// Primitive name of a float receiver (`"f32"` / `"f64"`), for the
    /// method spellings that route to an associated function on it.
    fn float_receiver_width(&mut self, receiver: NodeId) -> Option<&'static str> {
        let mut recv = self.table.get(receiver)?;
        while let Some(gossamer_types::TyKind::Ref { inner, .. }) = self.tcx.kind(recv) {
            recv = *inner;
        }
        match self.tcx.kind(recv)? {
            gossamer_types::TyKind::Float(gossamer_types::FloatTy::F32) => Some("f32"),
            gossamer_types::TyKind::Float(gossamer_types::FloatTy::F64) => Some("f64"),
            _ => None,
        }
    }

    /// A `sort::` call over a struct or enum element, rewritten to the
    /// comparator-taking form.
    ///
    /// These primitives order by a single machine word, which for an aggregate
    /// element is the address rather than the value, so an aggregate reaches
    /// its order only through a comparator - the type's own when its source
    /// writes one, and the synthesized field-by-field order otherwise.
    fn lower_sequence_order_call(
        &mut self,
        callee: &AstExpr,
        args: &[AstExpr],
        span: Span,
    ) -> Option<HirExprKind> {
        let AstExprKind::Path(path) = &callee.kind else {
            return None;
        };
        let joined: Vec<&str> = path.segments.iter().map(|s| s.name.name.as_str()).collect();
        let last = *joined.last()?;
        if joined.len() > 2 || (joined.len() == 2 && joined[0] != "sort") {
            return None;
        }
        let sequence = args.first()?;
        // A type whose source writes its own `cmp` orders by that; every other
        // ordered type carries the synthesized field-by-field one, which the
        // primitives need only because they cannot compare an aggregate.
        let (symbol, elem, cmp_prefix) = self
            .element_comparator(sequence.id, USER_COMPARATOR_PREFIX)
            .map(|(symbol, elem)| (symbol, elem, USER_COMPARATOR_PREFIX))
            .or_else(|| {
                self.element_comparator(sequence.id, STRUCTURAL_COMPARATOR_PREFIX)
                    .map(|(symbol, elem)| (symbol, elem, STRUCTURAL_COMPARATOR_PREFIX))
            })?;
        let mut lowered: Vec<HirExpr> = args.iter().map(|a| self.lower_expr(a)).collect();
        match (last, lowered.len()) {
            // Caller-side normalization has already run, so a free `iter::`
            // call reaches lowering with its callback first.
            ("sort_stable", 1) => {
                let cmp = self.comparator_path(&format!("{cmp_prefix}{symbol}"), elem, span);
                let sort_by = self.free_path(&["iter", "sort_by"], span);
                Some(HirExprKind::Call {
                    callee: Box::new(sort_by),
                    args: vec![cmp, lowered.remove(0)],
                })
            }
            // The search body is monomorphic and names the comparator itself,
            // so the call passes only the sequence and the value sought.
            ("binary_search" | "partition_point", 2) => {
                let prefix = if last == "binary_search" {
                    BINARY_SEARCH_PREFIX
                } else {
                    PARTITION_POINT_PREFIX
                };
                let helper = format!("{prefix}{symbol}");
                let callee = self.free_path(&[&helper], span);
                Some(HirExprKind::Call {
                    callee: Box::new(callee),
                    args: lowered,
                })
            }
            _ => None,
        }
    }

    /// A path expression naming a free function, for a call this pass builds.
    fn free_path(&mut self, segments: &[&str], span: Span) -> HirExpr {
        HirExpr {
            id: self.fresh(),
            span,
            ty: self.tcx.error_ty(),
            kind: HirExprKind::Path {
                segments: segments.iter().map(|s| Ident::new(*s)).collect(),
                def: None,
            },
        }
    }

    /// The backend symbol of a sequence's element type, together with the
    /// element type itself, when the element is a user type carrying a
    /// comparator under `prefix`.
    ///
    /// The ordering primitives take a comparator, not a method, so the order
    /// such a type declares reaches them only by naming its synthesized
    /// functions, all of which are keyed by this symbol.
    fn element_comparator(&mut self, receiver: NodeId, prefix: &str) -> Option<(String, Ty)> {
        use gossamer_types::TyKind;
        let mut recv = self.table.get(receiver)?;
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(recv) {
            recv = *inner;
        }
        let elem = match self.tcx.kind(recv)? {
            TyKind::Vec(elem)
            | TyKind::Slice(elem)
            | TyKind::Array { elem, .. }
            | TyKind::Iterator(elem) => *elem,
            _ => return None,
        };
        let mut elem = elem;
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(elem) {
            elem = *inner;
        }
        let TyKind::Adt { def, .. } = self.tcx.kind(elem)? else {
            return None;
        };
        let registered = self.tcx.def_name(*def)?;
        if registered.starts_with("adt#") {
            return None;
        }
        let symbol = registered.replace("::", "__");
        self.user_comparators
            .contains(&format!("{prefix}{symbol}"))
            .then_some((symbol, elem))
    }

    /// A path expression naming `comparator`, typed as the two-element
    /// comparison the ordering helpers call it through.
    fn comparator_path(&mut self, comparator: &str, elem: Ty, span: Span) -> HirExpr {
        let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
        let sig = gossamer_types::FnSig {
            inputs: vec![elem, elem],
            output: i64_ty,
        };
        let ty = self.tcx.intern(gossamer_types::TyKind::FnTrait(sig));
        HirExpr {
            id: self.fresh(),
            span,
            ty,
            kind: HirExprKind::Path {
                segments: vec![Ident::new(comparator)],
                def: None,
            },
        }
    }

    fn nominal_impl_owner(&mut self, receiver: NodeId) -> Option<String> {
        let mut recv = self.table.get(receiver)?;
        while let Some(gossamer_types::TyKind::Ref { inner, .. }) = self.tcx.kind(recv) {
            recv = *inner;
        }
        let Some(gossamer_types::TyKind::Nominal { def, .. }) = self.tcx.kind(recv) else {
            return None;
        };
        let def = *def;
        Some(self.tcx.def_name(def)?.to_string())
    }

    fn unit(&mut self) -> gossamer_types::Ty {
        self.tcx.unit()
    }

    fn error_ty(&mut self) -> gossamer_types::Ty {
        self.tcx.error_ty()
    }

    fn lower_item(&mut self, item: &AstItem, module_path: &[String]) -> Option<HirItem> {
        self.current_module = module_path.to_vec();
        let def = self.resolutions.definition_of(item.id);
        let kind = match &item.kind {
            AstItemKind::Fn(decl) => HirItemKind::Fn(self.lower_fn(decl, item.span)),
            AstItemKind::Const(decl) => HirItemKind::Const(HirConst {
                name: decl.name.clone(),
                ty: self.ty_of(decl.value.id),
                value: self.lower_expr(&decl.value),
            }),
            AstItemKind::Static(decl) => HirItemKind::Static(HirStatic {
                name: decl.name.clone(),
                ty: self.ty_of(decl.value.id),
                mutable: matches!(decl.mutability, Mutability::Mutable),
                value: self.lower_expr(&decl.value),
            }),
            // An ADT declared inside a module carries its qualified name as
            // its identity, matching what the type checker registers. Every
            // name-keyed table below here - the MIR struct/variant tables,
            // `{:?}` dispatch, the native constructor registry - reads that
            // name, so two modules may declare the same one.
            AstItemKind::Struct(decl) => {
                let mut adt = self.lower_struct(decl);
                adt.name = Ident::new(qualified_item_name(module_path, &adt.name.name));
                HirItemKind::Adt(adt)
            }
            AstItemKind::Enum(decl) => {
                let mut adt = self.lower_enum(decl);
                adt.name = Ident::new(qualified_item_name(module_path, &adt.name.name));
                HirItemKind::Adt(adt)
            }
            AstItemKind::Impl(decl) => HirItemKind::Impl(self.lower_impl(decl, item.span)),
            AstItemKind::Trait(decl) => HirItemKind::Trait(self.lower_trait(decl, item.span)),
            AstItemKind::TypeAlias(_) | AstItemKind::Mod(_) | AstItemKind::AttrItem(_) => {
                return None;
            }
        };
        Some(HirItem {
            id: self.fresh(),
            span: item.span,
            def,
            module_path: module_path.to_vec(),
            kind,
        })
    }

    fn lower_fn(&mut self, decl: &AstFnDecl, span: Span) -> HirFn {
        self.lower_fn_with_self(decl, span, None)
    }

    /// Lowers an impl-method body with the impl's `Self` type
    /// applied to the `self` receiver. Lets MIR field-access
    /// lowering find the struct name on `self.field` reads
    /// without falling through to the unsupported placeholder.
    fn lower_fn_with_self(
        &mut self,
        decl: &AstFnDecl,
        span: Span,
        self_ty: Option<gossamer_types::Ty>,
    ) -> HirFn {
        let mut params = Vec::new();
        let mut has_self = false;
        // Destructuring `let`s injected at body entry for non-trivial param
        // patterns (`(a, b)`, `Pt(a, b)`, `P { x, y }`): MIR binds only one
        // name per parameter, so the param takes a fresh binding and the
        // pattern is bound by a `let` reusing the let-destructuring path.
        let mut param_destructures: Vec<HirStmt> = Vec::new();
        for param in &decl.params {
            match param {
                AstFnParam::Receiver(kind) => {
                    has_self = true;
                    let id = self.fresh();
                    let base = self_ty.unwrap_or_else(|| self.error_ty());
                    // For `&self` / `&mut self`, type `self` as a
                    // Ref so the codegen lowers field access
                    // (`self.x`) and field assignment (`self.x =
                    // y`) through the pointer - matching how
                    // free-function `&mut Type` parameters already
                    // work. Owned `self` keeps the value type.
                    let ty = match kind {
                        gossamer_ast::Receiver::Owned => base,
                        gossamer_ast::Receiver::RefShared => {
                            self.tcx.intern(gossamer_types::TyKind::Ref {
                                mutability: gossamer_types::Mutbl::Not,
                                inner: base,
                            })
                        }
                        gossamer_ast::Receiver::RefMut => {
                            self.tcx.intern(gossamer_types::TyKind::Ref {
                                mutability: gossamer_types::Mutbl::Mut,
                                inner: base,
                            })
                        }
                    };
                    params.push(HirParam {
                        pattern: HirPat {
                            id,
                            span,
                            ty,
                            kind: HirPatKind::Binding {
                                name: Ident::new("self"),
                                mutable: matches!(kind, gossamer_ast::Receiver::RefMut),
                            },
                        },
                        ty,
                        is_comptime: false,
                    });
                }
                AstFnParam::Typed {
                    pattern,
                    ty: ast_ty,
                    is_comptime,
                    ..
                } => {
                    let ty = self.ty_of(ast_ty.id);
                    let p = self.lower_typed_param(
                        pattern,
                        ty,
                        *is_comptime,
                        params.len(),
                        span,
                        &mut param_destructures,
                    );
                    params.push(p);
                }
            }
        }
        let ret = decl.ret.as_ref().map(|ty| self.ty_of(ty.id));
        let saved_ret = self
            .current_fn_ret_ty
            .replace(ret.unwrap_or_else(|| self.tcx.unit()));
        let body = decl.body.as_ref().map(|body| {
            let mut block = self.lower_expr_as_block(body);
            if !param_destructures.is_empty() {
                let mut stmts = std::mem::take(&mut param_destructures);
                stmts.append(&mut block.stmts);
                block.stmts = stmts;
            }
            self.discard_undeclared_tail(&decl.name.name, ret, &mut block);
            HirBody { block }
        });
        self.current_fn_ret_ty = saved_ret;
        HirFn {
            name: decl.name.clone(),
            params,
            ret,
            body,
            is_unsafe: decl.is_unsafe,
            is_comptime: decl.is_comptime,
            has_self,
            origin: FnOrigin::Declared,
        }
    }

    /// Demotes a value-producing tail to a statement when the signature
    /// answers a unit - written `-> ()` or left off. The value is computed
    /// for its effects and dropped, so every tier agrees with the signature
    /// the caller reads; the checker reports the undeclared spelling as a
    /// lint.
    ///
    /// A wrapper the front end synthesized around an expression - the REPL's
    /// per-input entry point, the binding-type probe - answers that
    /// expression by construction, and its caller reads the value back
    /// rather than the signature. A tail that already answers a unit has no
    /// value to discard and keeps its place.
    fn discard_undeclared_tail(
        &mut self,
        name: &str,
        ret: Option<gossamer_types::Ty>,
        block: &mut HirBlock,
    ) {
        if name.starts_with("__") {
            return;
        }
        let answers_unit =
            ret.is_none_or(|ty| matches!(self.tcx.kind(ty), Some(gossamer_types::TyKind::Unit)));
        let tail_holds_value = block.tail.as_ref().is_some_and(|tail| {
            !matches!(self.tcx.kind(tail.ty), Some(gossamer_types::TyKind::Unit))
        });
        if !answers_unit || !tail_holds_value {
            return;
        }
        let Some(tail) = block.tail.take() else {
            return;
        };
        block.ty = self.unit();
        block.stmts.push(HirStmt {
            id: self.fresh(),
            span: tail.span,
            kind: HirStmtKind::Expr {
                expr: *tail,
                has_semi: true,
            },
        });
    }

    /// Lowers one typed parameter. A non-trivial pattern (`(a, b)`,
    /// `Pt(a, b)`, `P { x, y }`) is bound to a fresh `__paramN` local and
    /// destructured by a `let` appended to `destructures` for injection at
    /// body entry, since MIR binds only a single name per parameter.
    fn lower_typed_param(
        &mut self,
        pattern: &AstPat,
        ty: gossamer_types::Ty,
        is_comptime: bool,
        index: usize,
        span: Span,
        destructures: &mut Vec<HirStmt>,
    ) -> HirParam {
        let lowered = self.lower_pat_with_ty(pattern, ty);
        let pattern = if matches!(
            lowered.kind,
            HirPatKind::Binding { .. } | HirPatKind::Wildcard
        ) {
            lowered
        } else {
            let name = format!("__param{index}");
            destructures.push(HirStmt {
                id: self.fresh(),
                span,
                kind: HirStmtKind::Let {
                    pattern: lowered,
                    ty,
                    init: Some(HirExpr {
                        id: self.fresh(),
                        span,
                        ty,
                        kind: HirExprKind::Path {
                            segments: vec![Ident::new(name.clone())],
                            def: None,
                        },
                    }),
                },
            });
            HirPat {
                id: self.fresh(),
                span,
                ty,
                kind: HirPatKind::Binding {
                    name: Ident::new(name),
                    mutable: false,
                },
            }
        };
        HirParam {
            pattern,
            ty,
            is_comptime,
        }
    }

    fn lower_struct(&mut self, decl: &StructDecl) -> HirAdt {
        let ty = self.error_ty();
        let fields = match &decl.body {
            gossamer_ast::StructBody::Named(named) => {
                named.iter().map(|f| f.name.clone()).collect()
            }
            // Tuple-struct fields are modelled as positional names "0".."N-1"
            // so construction and `.N` access reuse the named-field path.
            gossamer_ast::StructBody::Tuple(tup) => (0..tup.len())
                .map(|i| gossamer_ast::Ident::new(i.to_string()))
                .collect(),
            gossamer_ast::StructBody::Unit => Vec::new(),
        };
        HirAdt {
            name: decl.name.clone(),
            kind: HirAdtKind::Struct(fields),
            self_ty: ty,
            repr: gossamer_ast::EnumRepr::default(),
        }
    }

    fn lower_enum(&mut self, decl: &EnumDecl) -> HirAdt {
        let variants = decl
            .variants
            .iter()
            .map(|variant| {
                let (struct_fields, struct_field_tys) = match &variant.body {
                    gossamer_ast::StructBody::Named(fields) => {
                        let names: Vec<_> = fields.iter().map(|f| f.name.clone()).collect();
                        let tys: Vec<_> = fields.iter().map(|f| self.ty_of(f.ty.id)).collect();
                        (Some(names), Some(tys))
                    }
                    gossamer_ast::StructBody::Tuple(fields) => {
                        // Store positional field types so MIR lowering can assign
                        // the correct type (e.g. f64) to tuple-variant bindings
                        // instead of always using i64.
                        let tys: Vec<_> = fields.iter().map(|f| self.ty_of(f.ty.id)).collect();
                        (None, Some(tys))
                    }
                    gossamer_ast::StructBody::Unit => (None, None),
                };
                crate::tree::HirEnumVariant {
                    name: variant.name.clone(),
                    struct_fields,
                    struct_field_tys,
                }
            })
            .collect();
        let ty = self.error_ty();
        HirAdt {
            name: decl.name.clone(),
            kind: HirAdtKind::Enum(variants),
            self_ty: ty,
            repr: decl.repr,
        }
    }

    fn lower_impl(&mut self, decl: &ImplDecl, span: Span) -> HirImpl {
        let self_ty = self.ty_of(decl.self_ty.id);
        // An impl block's self type identifies the type it extends, so it
        // carries the same qualified identity the declaration registered -
        // otherwise two modules' `impl Point` would emit one symbol each
        // under the same name.
        let self_name = match &decl.self_ty.kind {
            gossamer_ast::TypeKind::Path(path) => self
                .resolutions
                .get(decl.self_ty.id)
                .and_then(|resolution| match resolution {
                    Resolution::Def { def, .. } => self.module_type_names.get(&def).cloned(),
                    _ => None,
                })
                .map(Ident::new)
                .or_else(|| {
                    // The written path is already the qualified spelling for
                    // an `impl a::Point`; keep every segment so the methods
                    // register under the type's identity.
                    let segments: Vec<&str> = path
                        .segments
                        .iter()
                        .map(|seg| seg.name.name.as_str())
                        .filter(|seg| !matches!(*seg, "crate" | "self" | "super" | "root"))
                        .collect();
                    (!segments.is_empty()).then(|| Ident::new(segments.join("::")))
                }),
            // A tuple has no path to name it by, so it registers under the
            // spelling every layer shares for one.
            _ => gossamer_types::printer::structural_impl_owner(self.tcx, self_ty).map(Ident::new),
        };
        let trait_name = decl
            .trait_ref
            .as_ref()
            .and_then(|bound| bound.path.segments.last())
            .map(|seg| seg.name.clone());
        let methods = decl
            .items
            .iter()
            .filter_map(|item| match item {
                ImplItem::Fn(fn_decl) => {
                    Some(self.lower_fn_with_self(fn_decl, span, Some(self_ty)))
                }
                // Associated types are already resolved to concrete types
                // in the type table, and every associated constant is a
                // top-level constant by the time lowering runs, so neither
                // needs a body-level HIR node.
                ImplItem::Const { .. } | ImplItem::Type { .. } => None,
            })
            .collect();
        HirImpl {
            self_ty,
            self_name,
            trait_name,
            methods,
        }
    }

    fn lower_trait(&mut self, decl: &TraitDecl, span: Span) -> HirTrait {
        let methods = decl
            .items
            .iter()
            .filter_map(|item| match item {
                TraitItem::Fn(fn_decl) => Some(self.lower_fn(fn_decl, span)),
                // Associated declarations carry no executable code: a
                // projection resolves during type checking and a constant
                // is hoisted to a top-level `const` before lowering.
                TraitItem::Type { .. } | TraitItem::Const { .. } => None,
            })
            .collect();
        HirTrait {
            name: decl.name.clone(),
            methods,
        }
    }

    fn lower_expr(&mut self, expr: &AstExpr) -> HirExpr {
        use gossamer_types::TyKind;
        if self.recursion_depth >= RECURSION_LIMIT {
            let ty = self.error_ty();
            return HirExpr {
                id: self.fresh(),
                span: expr.span,
                ty,
                kind: HirExprKind::Placeholder,
            };
        }
        self.recursion_depth += 1;
        let mut ty = self.ty_of(expr.id);
        let span = expr.span;
        let kind = self.lower_expr_kind(expr);
        self.recursion_depth = self.recursion_depth.saturating_sub(1);
        // `?`-unwrap leaves the typechecker's assigned type for the
        // outer Match unresolved when the inner Result wasn't
        // pinned. Pull the Ok-arm body's type up so any binding
        // bound to the `?`-expression carries something concrete
        // (typically String). Without this, a `let s = fs::
        // read_to_string(...)?; s.len()` lands on the generic
        // `gos_rt_len` instead of `gos_rt_str_len` and reads garbage.
        if matches!(self.tcx.kind(ty), Some(TyKind::Error | TyKind::Var(_))) {
            if let HirExprKind::Match { arms, .. } = &kind {
                if let Some(first) = arms.first() {
                    let arm_ty = first.body.ty;
                    if !matches!(self.tcx.kind(arm_ty), Some(TyKind::Error | TyKind::Var(_))) {
                        ty = arm_ty;
                    }
                }
            }
        }
        HirExpr {
            id: self.fresh(),
            span,
            ty,
            kind,
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "single dispatch match over every AST expression kind; splitting it would scatter the one-to-one HIR mapping"
    )]
    fn lower_expr_kind(&mut self, expr: &AstExpr) -> HirExprKind {
        match &expr.kind {
            AstExprKind::Literal(lit) => HirExprKind::Literal(lower_literal(lit)),
            AstExprKind::Path(path)
                if path
                    .segments
                    .last()
                    .is_some_and(|segment| self.unit_structs.contains(&segment.name.name))
                    && matches!(
                        self.resolutions.get(expr.id),
                        Some(Resolution::Def {
                            kind: gossamer_resolve::DefKind::Struct,
                            ..
                        })
                    ) =>
            {
                self.lower_struct_literal(expr.id, path, &[], None, expr.span)
            }
            AstExprKind::Path(path) => self.lower_path_expr(expr.id, path),
            AstExprKind::Call { callee, args } => {
                if let Some(lowered) = self.lower_sequence_order_call(callee, args, expr.span) {
                    lowered
                } else if let Some(lowered) = self.lower_reverse_call(callee, args, expr.span) {
                    lowered
                } else if let Some(lowered) = self.lower_tuple_struct_call(callee, args, expr.span)
                {
                    lowered
                } else {
                    let callee = Box::new(self.lower_expr(callee));
                    let mut args: Vec<HirExpr> = args.iter().map(|a| self.lower_expr(a)).collect();
                    self.resolve_format_pad_request(&callee, &mut args);
                    HirExprKind::Call { callee, args }
                }
            }
            AstExprKind::MethodCall {
                receiver,
                name,
                args,
                ..
            } => {
                if let Some(desugared) = self.desugar_or_insert_value(expr) {
                    return desugared.kind;
                }
                // Crossing an opaque alias's boundary with `.into()` is the
                // identity: the two types share one representation, and the
                // conversion exists to be written, not to compute. This is
                // decided here because the erasure below loses the very
                // distinction that selects it.
                if name.name == "into" && args.is_empty() {
                    match self.nominal_into_route(receiver.id, expr.id) {
                        Some(NominalInto::Identity) => return self.lower_expr(receiver).kind,
                        // The alias's own name keys its `From` impl; the
                        // erasure below would leave the representation's
                        // name, which no impl is filed under.
                        Some(NominalInto::From(target)) => {
                            let span = expr.span;
                            let ty = self.ty_of(expr.id);
                            let callee = HirExpr {
                                id: self.fresh(),
                                span,
                                ty,
                                kind: HirExprKind::Path {
                                    segments: vec![Ident::new(&target), Ident::new("from")],
                                    def: None,
                                },
                            };
                            return HirExprKind::Call {
                                callee: Box::new(callee),
                                args: vec![self.lower_expr(receiver)],
                            };
                        }
                        None => {}
                    }
                }
                // An opaque alias owns its method surface outright - it
                // inherits none of the representation's - so a method on one
                // is its own impl's, and that impl is filed under the alias's
                // name. Name it here, for the same reason `.into()` is named
                // here: below this point the receiver is the representation,
                // whose own methods would answer instead.
                if let Some(target) = self.nominal_impl_owner(receiver.id) {
                    let span = expr.span;
                    let ty = self.ty_of(expr.id);
                    let callee = HirExpr {
                        id: self.fresh(),
                        span,
                        ty,
                        kind: HirExprKind::Path {
                            segments: vec![Ident::new(&target), name.clone()],
                            def: None,
                        },
                    };
                    let mut call_args = vec![self.lower_expr(receiver)];
                    call_args.extend(args.iter().map(|a| self.lower_expr(a)));
                    return HirExprKind::Call {
                        callee: Box::new(callee),
                        args: call_args,
                    };
                }
                // `x.to_bits()` is the method spelling of
                // `f64::to_bits(x)`; routing it to the associated form
                // keeps one lowering for both spellings on every tier.
                if name.name == "to_bits"
                    && args.is_empty()
                    && let Some(owner) = self.float_receiver_width(receiver.id)
                {
                    let span = expr.span;
                    let ty = self.ty_of(expr.id);
                    let callee = HirExpr {
                        id: self.fresh(),
                        span,
                        ty,
                        kind: HirExprKind::Path {
                            segments: vec![Ident::new(owner), name.clone()],
                            def: None,
                        },
                    };
                    return HirExprKind::Call {
                        callee: Box::new(callee),
                        args: vec![self.lower_expr(receiver)],
                    };
                }
                // A map or set traverses eagerly, and the compiled tiers reach
                // that walk through the iterator its `iter()` answers.
                if let Some(kind) = self.desugar_keyed_traversal(expr, receiver, name, args) {
                    return kind;
                }
                // An element type whose source supplies its own `cmp` decides
                // its order. The ordering primitives take a comparator, so the
                // bare spelling names the type's comparator explicitly.
                if args.is_empty()
                    && let Some(by) = comparator_ordering_form(name.name.as_str())
                    && let Some((symbol, elem)) =
                        self.element_comparator(receiver.id, USER_COMPARATOR_PREFIX)
                {
                    let lowered_receiver = self.lower_expr(receiver);
                    let name = format!("{USER_COMPARATOR_PREFIX}{symbol}");
                    let cmp = self.comparator_path(&name, elem, expr.span);
                    return HirExprKind::MethodCall {
                        receiver: Box::new(lowered_receiver),
                        name: Ident::new(by),
                        args: vec![cmp],
                        owner: None,
                    };
                }
                let owner = self.method_owner_of(expr.id);
                HirExprKind::MethodCall {
                    receiver: Box::new(self.lower_expr(receiver)),
                    name: name.clone(),
                    args: args.iter().map(|a| self.lower_expr(a)).collect(),
                    owner,
                }
            }
            AstExprKind::FieldAccess { receiver, field } => self.lower_field(receiver, field),
            AstExprKind::Index { base, index } => HirExprKind::Index {
                base: Box::new(self.lower_expr(base)),
                index: Box::new(self.lower_expr(index)),
            },
            AstExprKind::Unary { op, operand } => HirExprKind::Unary {
                op: lower_unary_op(*op),
                operand: Box::new(self.lower_expr(operand)),
            },
            AstExprKind::Binary { op, lhs, rhs } => self.lower_binary(*op, lhs, rhs),
            AstExprKind::Assign { op, place, value } => self.lower_assign(*op, place, value, expr),
            AstExprKind::Cast { value, ty: ast_ty } => HirExprKind::Cast {
                value: Box::new(self.lower_expr(value)),
                ty: self.ty_of(ast_ty.id),
            },
            AstExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => HirExprKind::If {
                condition: Box::new(self.lower_expr(condition)),
                then_branch: Box::new(self.lower_expr(then_branch)),
                else_branch: else_branch.as_ref().map(|e| Box::new(self.lower_expr(e))),
            },
            AstExprKind::Match { scrutinee, arms } => HirExprKind::Match {
                scrutinee: Box::new(self.lower_expr(scrutinee)),
                arms: arms.iter().map(|arm| self.lower_match_arm(arm)).collect(),
            },
            AstExprKind::Loop { body, label } => HirExprKind::Loop {
                body: Box::new(self.lower_expr(body)),
                label: label.as_ref().map(|l| l.name.clone()),
            },
            AstExprKind::While {
                condition,
                body,
                label,
            } => HirExprKind::While {
                condition: Box::new(self.lower_expr(condition)),
                body: Box::new(self.lower_expr(body)),
                label: label.as_ref().map(|l| l.name.clone()),
            },
            AstExprKind::For {
                pattern,
                iter,
                body,
                label,
            } => self.lower_for(
                pattern,
                iter,
                body,
                label.as_ref().map(|l| l.name.clone()),
                expr.span,
            ),
            AstExprKind::Block(block) | AstExprKind::Unsafe(block) => {
                HirExprKind::Block(self.lower_block(block, expr.span))
            }
            AstExprKind::Closure { params, ret, body } => HirExprKind::Closure {
                params: self.lower_closure_params(params),
                ret: ret.as_ref().map(|ty| self.ty_of(ty.id)),
                body: Box::new(self.lower_expr(body)),
            },
            AstExprKind::Return(value) => {
                HirExprKind::Return(value.as_ref().map(|v| Box::new(self.lower_expr(v))))
            }
            AstExprKind::Break { value, label } => HirExprKind::Break {
                value: value.as_ref().map(|v| Box::new(self.lower_expr(v))),
                label: label.as_ref().map(|l| l.name.clone()),
            },
            AstExprKind::Continue { label } => HirExprKind::Continue {
                label: label.as_ref().map(|l| l.name.clone()),
            },
            AstExprKind::Tuple(elems) => {
                HirExprKind::Tuple(elems.iter().map(|e| self.lower_expr(e)).collect())
            }
            AstExprKind::Select(arms) => self.lower_select(arms),
            AstExprKind::Struct {
                path, fields, base, ..
            } => self.lower_struct_literal(expr.id, path, fields, base.as_deref(), expr.span),
            AstExprKind::MapLiteral(entries) => {
                let map_ty = self.ty_of(expr.id);
                self.lower_map_literal(entries, expr.span, map_ty)
            }
            AstExprKind::SetLiteral(entries) => {
                let set_ty = self.ty_of(expr.id);
                self.lower_set_literal(entries, expr.span, set_ty)
            }
            AstExprKind::Array(arr) | AstExprKind::FixedArray(arr) => {
                HirExprKind::Array(self.lower_array(arr))
            }
            AstExprKind::Range {
                start, end, kind, ..
            } => HirExprKind::Range {
                start: start.as_ref().map(|s| Box::new(self.lower_expr(s))),
                end: end.as_ref().map(|e| Box::new(self.lower_expr(e))),
                inclusive: matches!(kind, gossamer_ast::RangeKind::Inclusive),
            },
            AstExprKind::Try(inner) => self.lower_try(inner, expr.span),
            AstExprKind::Error => HirExprKind::Placeholder,
        }
    }

    fn lower_binary(&mut self, op: AstBinOp, lhs: &AstExpr, rhs: &AstExpr) -> HirExprKind {
        if matches!(op, AstBinOp::PipeGt) {
            return self.lower_pipe(lhs, rhs);
        }
        HirExprKind::Binary {
            op: lower_binary_op(op),
            lhs: Box::new(self.lower_expr(lhs)),
            rhs: Box::new(self.lower_expr(rhs)),
        }
    }

    /// Reads a lowered integer literal's value.
    fn literal_int_of(expr: &HirExpr) -> Option<i64> {
        match &expr.kind {
            HirExprKind::Literal(HirLiteral::Int(text)) => text.parse().ok(),
            _ => None,
        }
    }

    /// Turns a `__fmt_pad` call's alignment *request* into the alignment the
    /// runtime helpers implement.
    ///
    /// An omitted alignment and the `0` flag both read differently on a number
    /// than on anything else, and the value's type is first available here.
    fn resolve_format_pad_request(&mut self, callee: &HirExpr, args: &mut [HirExpr]) {
        let HirExprKind::Path { segments, .. } = &callee.kind else {
            return;
        };
        if !segments
            .last()
            .is_some_and(|segment| segment.name.as_str() == "__fmt_pad" && args.len() == 4)
        {
            return;
        }
        let Some(fill) = Self::literal_int_of(&args[2])
            .and_then(|code| u32::try_from(code).ok())
            .and_then(char::from_u32)
        else {
            return;
        };
        let Some(request) = Self::literal_int_of(&args[3]) else {
            return;
        };
        let numeric = self.pad_value_is_numeric(&args[0]);
        let (align, fill) = gossamer_ast::resolve_pad_request(request, fill, numeric);
        args[2].kind = HirExprKind::Literal(HirLiteral::Int((fill as u32).to_string()));
        args[3].kind = HirExprKind::Literal(HirLiteral::Int(align.to_string()));
    }

    /// Whether the value a `__fmt_pad` call pads renders as a number.
    ///
    /// The padded argument is the rendering wrapper the format expansion
    /// built, so the number is one call in: `__concat(x)`, `__fmt_prec(x, n)`,
    /// `__debug(x)`, or a radix prefix concatenated onto one of those.
    fn pad_value_is_numeric(&mut self, rendered: &HirExpr) -> bool {
        let HirExprKind::Call { callee, args } = &rendered.kind else {
            return false;
        };
        let HirExprKind::Path { segments, .. } = &callee.kind else {
            return false;
        };
        match segments.last().map(|segment| segment.name.as_str()) {
            // `__concat(prefix, rendering)` - the `{:#x}` radix prefix.
            Some("__concat") if args.len() == 2 => self.pad_value_is_numeric(&args[1]),
            Some("__concat" | "__fmt_prec" | "__debug" | "__fmt_radix" | "__fmt_upper") => args
                .first()
                .is_some_and(|value| self.ty_renders_as_number(value.ty)),
            _ => false,
        }
    }

    fn ty_renders_as_number(&mut self, ty: gossamer_types::Ty) -> bool {
        matches!(
            self.tcx.kind_of(ty),
            gossamer_types::TyKind::Int(_) | gossamer_types::TyKind::Float(_)
        )
    }

    fn lower_pipe(&mut self, lhs: &AstExpr, rhs: &AstExpr) -> HirExprKind {
        let piped = self.lower_expr(lhs);
        match &rhs.kind {
            AstExprKind::Call { callee, args } => {
                let mut new_args: Vec<HirExpr> = args.iter().map(|a| self.lower_expr(a)).collect();
                new_args.push(piped);
                HirExprKind::Call {
                    callee: Box::new(self.lower_expr(callee)),
                    args: new_args,
                }
            }
            AstExprKind::MethodCall {
                receiver,
                name,
                args,
                ..
            } => {
                let mut new_args: Vec<HirExpr> = args.iter().map(|a| self.lower_expr(a)).collect();
                new_args.push(piped);
                let owner = self.method_owner_of(rhs.id);
                HirExprKind::MethodCall {
                    receiver: Box::new(self.lower_expr(receiver)),
                    name: name.clone(),
                    args: new_args,
                    owner,
                }
            }
            AstExprKind::Closure { params, ret, body } if params.len() == 1 => {
                self.lower_closure_pipe_step(&params[0], ret.as_ref(), body, rhs, piped)
            }
            AstExprKind::Path(_) | AstExprKind::Closure { .. } => HirExprKind::Call {
                callee: Box::new(self.lower_expr(rhs)),
                args: vec![piped],
            },
            _ => HirExprKind::Placeholder,
        }
    }

    /// Lowers `x |> |v| body` to the block `{ let v = x; body }`.
    ///
    /// A closure written directly as a step is a spelling of the call it
    /// makes, not a value: binding the parameter keeps the step's arguments in
    /// the caller's frame, so a combinator chain stays one chain and a `Copy`
    /// scalar the body mutates is the caller's, not a copy of it.
    ///
    /// A body whose control flow leaves the closure - a `return`, a `?`, a
    /// `break` or `continue` targeting an outer loop - keeps the closure it
    /// was written against, since those target the closure rather than the
    /// enclosing function.
    fn lower_closure_pipe_step(
        &mut self,
        param: &AstClosureParam,
        ret: Option<&AstType>,
        body: &AstExpr,
        rhs: &AstExpr,
        piped: HirExpr,
    ) -> HirExprKind {
        let ty = match &param.ty {
            Some(ast_ty) => self.ty_of(ast_ty.id),
            None => self.ty_of(param.pattern.id),
        };
        let pattern = self.lower_pat_with_ty(&param.pattern, ty);
        let lowered_body = self.lower_expr(body);
        if !matches!(pattern.kind, HirPatKind::Binding { .. })
            || !crate::fuse::inline_safe(&lowered_body, 0)
        {
            let closure = HirExpr {
                id: self.fresh(),
                span: rhs.span,
                ty: self.ty_of(rhs.id),
                kind: HirExprKind::Closure {
                    params: vec![HirParam {
                        pattern,
                        ty,
                        is_comptime: false,
                    }],
                    ret: ret.map(|ty| self.ty_of(ty.id)),
                    body: Box::new(lowered_body),
                },
            };
            return HirExprKind::Call {
                callee: Box::new(closure),
                args: vec![piped],
            };
        }
        let span = rhs.span;
        let binding = HirStmt {
            id: self.fresh(),
            span,
            kind: HirStmtKind::Let {
                pattern,
                ty,
                init: Some(piped),
            },
        };
        HirExprKind::Block(HirBlock {
            id: self.fresh(),
            span,
            ty: lowered_body.ty,
            stmts: vec![binding],
            tail: Some(Box::new(lowered_body)),
            is_comptime: false,
        })
    }

    /// Rewrites `(a, b.c) = rhs` into
    /// `{ let (t0, t1) = rhs; a = t0; b.c = t1 }`, so every tier sees the
    /// ordinary assignments the destructuring stands for. The right-hand
    /// side is evaluated once, before the first target is written.
    fn lower_destructuring_assign<'a>(
        &mut self,
        op: AssignOp,
        elems: &'a [AstExpr],
        place: &AstExpr,
        value: &'a AstExpr,
        span: Span,
    ) -> HirExprKind {
        let tuple_ty = self.ty_of(place.id);
        let lowered_value = self.lower_expr(value);
        let mut targets: Vec<(&'a AstExpr, Ident)> = Vec::new();
        let mut next = 0usize;
        let pattern = self.destructuring_pattern(elems, tuple_ty, span, &mut next, &mut targets);
        let unit = self.tcx.unit();
        let mut stmts = vec![HirStmt {
            id: self.fresh(),
            span,
            kind: HirStmtKind::Let {
                pattern,
                ty: tuple_ty,
                init: Some(lowered_value),
            },
        }];
        for (target, name) in targets {
            let ty = self.ty_of(target.id);
            let lowered_target = self.lower_expr(target);
            let source = HirExpr {
                id: self.fresh(),
                span: target.span,
                ty,
                kind: HirExprKind::Path {
                    segments: vec![name],
                    def: None,
                },
            };
            // A compound operator applies element-wise: each place is read,
            // combined with its element of the right-hand value, and written
            // back, exactly as the single-place form does.
            let source = if matches!(op, AssignOp::Assign) {
                source
            } else {
                HirExpr {
                    id: self.fresh(),
                    span: target.span,
                    ty,
                    kind: HirExprKind::Binary {
                        op: compound_assign_to_binary(op),
                        lhs: Box::new(lowered_target.clone()),
                        rhs: Box::new(source),
                    },
                }
            };
            let write = HirExpr {
                id: self.fresh(),
                span: target.span,
                ty: unit,
                kind: HirExprKind::Assign {
                    place: Box::new(lowered_target),
                    value: Box::new(source),
                },
            };
            stmts.push(HirStmt {
                id: self.fresh(),
                span: target.span,
                kind: HirStmtKind::Expr {
                    expr: write,
                    has_semi: true,
                },
            });
        }
        HirExprKind::Block(HirBlock {
            id: self.fresh(),
            span,
            ty: unit,
            stmts,
            tail: None,
            is_comptime: false,
        })
    }

    /// Builds the tuple pattern binding one temporary per destructuring
    /// target, collecting each target with the name it reads back from. A
    /// nested tuple recurses; a `_` element binds nothing.
    fn destructuring_pattern<'a>(
        &mut self,
        elems: &'a [AstExpr],
        tuple_ty: Ty,
        span: Span,
        next: &mut usize,
        targets: &mut Vec<(&'a AstExpr, Ident)>,
    ) -> HirPat {
        let declared: Option<Vec<Ty>> = match self.tcx.kind(tuple_ty) {
            Some(gossamer_types::TyKind::Tuple(tys)) if tys.len() == elems.len() => {
                Some(tys.clone())
            }
            _ => None,
        };
        let elem_tys: Vec<Ty> = if let Some(tys) = declared {
            tys
        } else {
            let mut tys = Vec::with_capacity(elems.len());
            for elem in elems {
                tys.push(self.ty_of(elem.id));
            }
            tys
        };
        let mut pats = Vec::with_capacity(elems.len());
        for (elem, elem_ty) in elems.iter().zip(elem_tys) {
            let kind = match &elem.kind {
                AstExprKind::Tuple(inner) => {
                    pats.push(self.destructuring_pattern(inner, elem_ty, elem.span, next, targets));
                    continue;
                }
                _ if elem.is_wildcard() => HirPatKind::Wildcard,
                _ => {
                    let name = Ident::new(format!("__gos_destructure_{next}"));
                    *next += 1;
                    targets.push((elem, name.clone()));
                    HirPatKind::Binding {
                        name,
                        mutable: false,
                    }
                }
            };
            pats.push(HirPat {
                id: self.fresh(),
                span: elem.span,
                ty: elem_ty,
                kind,
            });
        }
        HirPat {
            id: self.fresh(),
            span,
            ty: tuple_ty,
            kind: HirPatKind::Tuple(pats),
        }
    }

    fn lower_assign(
        &mut self,
        op: AssignOp,
        place: &AstExpr,
        value: &AstExpr,
        outer: &AstExpr,
    ) -> HirExprKind {
        if let AstExprKind::Tuple(elems) = &place.kind {
            return self.lower_destructuring_assign(op, elems, place, value, outer.span);
        }
        if matches!(op, AssignOp::Assign) {
            return HirExprKind::Assign {
                place: Box::new(self.lower_expr(place)),
                value: Box::new(self.lower_expr(value)),
            };
        }
        let lowered_place = self.lower_expr(place);
        let lowered_value = self.lower_expr(value);
        let bin_op = compound_assign_to_binary(op);
        let place_ty = lowered_place.ty;
        let value_ty = lowered_value.ty;
        let bin_expr = HirExpr {
            id: self.fresh(),
            span: outer.span,
            ty: place_ty,
            kind: HirExprKind::Binary {
                op: bin_op,
                lhs: Box::new(lowered_place.clone()),
                rhs: Box::new(HirExpr {
                    ty: value_ty,
                    ..lowered_value
                }),
            },
        };
        HirExprKind::Assign {
            place: Box::new(lowered_place),
            value: Box::new(bin_expr),
        }
    }

    fn lower_field(
        &mut self,
        receiver: &AstExpr,
        field: &gossamer_ast::FieldSelector,
    ) -> HirExprKind {
        // A tuple struct models its fields as named "0".."N-1", so positional
        // access `p.0` on one routes through the named-field path (the value
        // is a struct aggregate, not a tuple).
        let tuple_struct = matches!(field, gossamer_ast::FieldSelector::Index(_))
            && self.receiver_is_tuple_struct(receiver);
        let lowered = self.lower_expr(receiver);
        match field {
            gossamer_ast::FieldSelector::Named(name) => HirExprKind::Field {
                receiver: Box::new(lowered),
                name: name.clone(),
            },
            gossamer_ast::FieldSelector::Index(idx) if tuple_struct => HirExprKind::Field {
                receiver: Box::new(lowered),
                name: gossamer_ast::Ident::new(idx.to_string()),
            },
            gossamer_ast::FieldSelector::Index(idx) => HirExprKind::TupleIndex {
                receiver: Box::new(lowered),
                index: *idx,
            },
        }
    }

    /// `true` when `receiver`'s checked type is a tuple struct (peeling
    /// references), so positional access on it is a struct field projection.
    fn receiver_is_tuple_struct(&mut self, receiver: &AstExpr) -> bool {
        let mut ty = self.ty_of(receiver.id);
        loop {
            match self.tcx.kind(ty) {
                Some(gossamer_types::TyKind::Ref { inner, .. }) => ty = *inner,
                Some(gossamer_types::TyKind::Adt { def, .. }) => {
                    let local = def.local;
                    return self.tcx.is_tuple_struct(local);
                }
                _ => return false,
            }
        }
    }

    fn lower_match_arm(&mut self, arm: &MatchArm) -> HirMatchArm {
        HirMatchArm {
            pattern: self.lower_pat(&arm.pattern),
            guard: arm.guard.as_ref().map(|g| self.lower_expr(g)),
            body: self.lower_expr(&arm.body),
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "for-loop desugar builds the let / loop / match / break HIR scaffold inline; splitting hides the structural shape"
    )]
    fn lower_for(
        &mut self,
        pattern: &AstPat,
        iter: &AstExpr,
        body: &AstExpr,
        label: Option<String>,
        span: Span,
    ) -> HirExprKind {
        let mut iter_expr = self.lower_expr(iter);
        // A reference to a `String` walks the text it names, so the cursor is
        // chosen from the referent's type. Reading the reference's own type
        // here left the loop on the generic sequence walk, which hands the
        // body each scalar as an integer rather than a `char`.
        let mut iter_referent = iter_expr.ty;
        while let Some(gossamer_types::TyKind::Ref { inner, .. }) = self.tcx.kind(iter_referent) {
            iter_referent = *inner;
        }
        if matches!(
            self.tcx.kind(iter_referent),
            Some(gossamer_types::TyKind::String)
        ) {
            let char_ty = self.tcx.char_ty();
            // `chars()` answers a cursor, and the loop drives it.
            let collection_ty = self.tcx.intern(gossamer_types::TyKind::Iterator(char_ty));
            iter_expr = HirExpr {
                id: self.fresh(),
                span: iter_expr.span,
                ty: collection_ty,
                kind: HirExprKind::MethodCall {
                    receiver: Box::new(iter_expr),
                    name: Ident::new("chars"),
                    args: Vec::new(),
                    owner: None,
                },
            };
        } else if let HirExprKind::Range { start, .. } = &mut iter_expr.kind
            && start.is_none()
        {
            let zero = HirExpr {
                id: self.fresh(),
                span: iter_expr.span,
                ty: self.tcx.int_ty(gossamer_types::IntTy::I64),
                kind: HirExprKind::Literal(HirLiteral::Int("0".to_string())),
            };
            *start = Some(Box::new(zero));
        }
        let iter_ty = iter_expr.ty;
        // For unknown / Adt iter types the canonical desugar
        // needs to bind the iter to a fresh slot and call
        // `.next()` on `&mut` of that slot - that's the
        // mechanism user `impl Iterator for T` relies on for
        // its state to persist across iterations. For built-in
        // iter shapes (ranges, arrays, vecs), the MIR fast paths
        // walk the receiver expression directly, so we keep the
        // inline shape that those detectors recognise.
        // Lazy iterator state is a cursor: it must be bound once and advanced,
        // since re-evaluating the expression that built it would hand the loop
        // a fresh cursor on every turn. A syntactic range keeps its counted
        // inline loop, and a bare `.iter()` keeps the indexed walk over its
        // source collection - both shapes the fast paths recognise by syntax.
        // A String cursor is the shape the loop can advance in place: it
        // yields one scalar at a time from a source it does not have to hold.
        // Every other pipeline keeps the walk it had, because an adapter
        // chain has no advancing shim of its own and would spin on a cursor
        // that never moves.
        let string_cursor_tail = matches!(
            &iter_expr.kind,
            HirExprKind::MethodCall { name, args, .. }
                if matches!(name.name.as_str(), "chars" | "bytes") && args.is_empty()
        );
        let lazy_state_route = string_cursor_tail
            && matches!(
                self.tcx.kind(iter_ty),
                Some(gossamer_types::TyKind::Iterator(elem)) if self.lazy_elem_is_drivable(*elem)
            );
        let needs_state_binding = lazy_state_route
            || self.iter_needs_state_binding(iter_ty)
            || Self::iter_expr_is_temporary_sequence(&iter_expr);
        if needs_state_binding {
            return self.lower_for_user_iter(pattern, iter_expr, body, label, span);
        }
        self.lower_for_inline(pattern, iter_expr, body, label, span)
    }

    /// `true` when the loop's iterable is a freshly built sequence with
    /// no home to index - a literal, or an `iter()` / `enumerate()`
    /// chain over one. Such a value must be bound before the loop; the
    /// inline shape leaves the compiled tiers indexing a temporary that
    /// no longer exists.
    fn iter_expr_is_temporary_sequence(iter_expr: &HirExpr) -> bool {
        let mut cur = iter_expr;
        loop {
            match &cur.kind {
                HirExprKind::Array(_) => return true,
                HirExprKind::MethodCall {
                    receiver,
                    name,
                    args,
                    owner: None,
                } if args.is_empty() && (name.name == "iter" || name.name == "enumerate") => {
                    cur = receiver;
                }
                _ => return false,
            }
        }
    }

    /// Desugars `for x in iter` to the canonical `loop { match
    /// (&mut __for_iter).next() { Some(x) => body, None => break } }`.
    /// Used when the iter expression is a user struct / unknown
    /// type - those need state persistence across `next()` calls.
    fn lower_for_user_iter(
        &mut self,
        pattern: &AstPat,
        iter_expr: HirExpr,
        body: &AstExpr,
        label: Option<String>,
        span: Span,
    ) -> HirExprKind {
        let iter_ty = iter_expr.ty;
        let iter_local_id = self.fresh();
        let iter_pat = HirPat {
            id: self.fresh(),
            span,
            ty: iter_ty,
            kind: HirPatKind::Binding {
                name: Ident::new("__for_iter"),
                mutable: true,
            },
        };
        let iter_let = HirStmt {
            id: self.fresh(),
            span,
            kind: HirStmtKind::Let {
                pattern: iter_pat,
                ty: iter_ty,
                init: Some(iter_expr),
            },
        };
        let iter_path = HirExpr {
            id: iter_local_id,
            span,
            ty: iter_ty,
            kind: HirExprKind::Path {
                segments: vec![Ident::new("__for_iter")],
                def: None,
            },
        };
        let iter_ref = HirExpr {
            id: self.fresh(),
            span,
            ty: iter_ty,
            kind: HirExprKind::Unary {
                op: crate::tree::HirUnaryOp::RefMut,
                operand: Box::new(iter_path),
            },
        };
        let next_call = HirExpr {
            id: self.fresh(),
            span,
            ty: self.error_ty(),
            kind: HirExprKind::MethodCall {
                receiver: Box::new(iter_ref),
                name: Ident::new("next"),
                args: Vec::new(),
                owner: None,
            },
        };
        let loop_expr = self.assemble_for_loop(pattern, next_call, body, label, span);
        let outer_block = HirBlock {
            id: self.fresh(),
            span,
            stmts: vec![iter_let],
            tail: Some(Box::new(loop_expr)),
            ty: self.unit(),
            is_comptime: false,
        };
        HirExprKind::Block(outer_block)
    }

    /// Inline shape - `loop { match <iter>.next() { ... } }`. The
    /// MIR / interp for-loop fast-paths inspect `<iter>` directly,
    /// so for built-in iterables (ranges, slices, vecs) we keep
    /// the receiver expression in place rather than introducing a
    /// `__for_iter` binding the detectors don't recognise.
    fn lower_for_inline(
        &mut self,
        pattern: &AstPat,
        iter_expr: HirExpr,
        body: &AstExpr,
        label: Option<String>,
        span: Span,
    ) -> HirExprKind {
        let next_call = HirExpr {
            id: self.fresh(),
            span,
            ty: self.error_ty(),
            kind: HirExprKind::MethodCall {
                receiver: Box::new(iter_expr),
                name: Ident::new("next"),
                args: Vec::new(),
                owner: None,
            },
        };
        self.assemble_for_loop(pattern, next_call, body, label, span)
            .kind
    }

    /// Shared builder: wraps a `match scrutinee { Some(pat) =>
    /// body, None => break }` in a `loop` whose body is one Block.
    fn assemble_for_loop(
        &mut self,
        pattern: &AstPat,
        next_call: HirExpr,
        body: &AstExpr,
        label: Option<String>,
        span: Span,
    ) -> HirExpr {
        let loop_pat = self.lower_pat(pattern);
        let pat_ty = loop_pat.ty;
        let some_pat = HirPat {
            id: self.fresh(),
            span,
            ty: pat_ty,
            kind: HirPatKind::Variant {
                name: Ident::new("Some"),
                fields: vec![loop_pat],
            },
        };
        let none_pat = HirPat {
            id: self.fresh(),
            span,
            ty: pat_ty,
            kind: HirPatKind::Variant {
                name: Ident::new("None"),
                fields: Vec::new(),
            },
        };
        let body_expr = self.lower_expr(body);
        let unit_ty = self.unit();
        let break_expr = HirExpr {
            id: self.fresh(),
            span,
            ty: self.tcx.never(),
            kind: HirExprKind::Break {
                value: None,
                label: None,
            },
        };
        let match_expr = HirExpr {
            id: self.fresh(),
            span,
            ty: unit_ty,
            kind: HirExprKind::Match {
                scrutinee: Box::new(next_call),
                arms: vec![
                    HirMatchArm {
                        pattern: some_pat,
                        guard: None,
                        body: body_expr,
                    },
                    HirMatchArm {
                        pattern: none_pat,
                        guard: None,
                        body: break_expr,
                    },
                ],
            },
        };
        let inner_block = HirBlock {
            id: self.fresh(),
            span,
            stmts: Vec::new(),
            tail: Some(Box::new(match_expr)),
            ty: unit_ty,
            is_comptime: false,
        };
        let body_block = HirExpr {
            id: self.fresh(),
            span,
            ty: unit_ty,
            kind: HirExprKind::Block(inner_block),
        };
        HirExpr {
            id: self.fresh(),
            span,
            ty: unit_ty,
            kind: HirExprKind::Loop {
                body: Box::new(body_block),
                label,
            },
        }
    }

    /// Returns `true` when an iter expression of type `ty` needs
    /// the `let mut __for_iter = ...` binding so `.next()` calls
    /// can persist state. `Adt` (user struct) and `Var(_)` shapes
    /// take the state path; ranges / arrays / vecs / slices /
    /// `HashMap`s stay inline so the MIR fast-paths can recognise
    /// the receiver expression directly.
    /// Whether the lazy iterator runtime can hand this element out one at a
    /// time. Only an element it carries in a single 8-byte slot has an
    /// advancing shim, so binding the cursor is worth it exactly for those;
    /// every other element reaches the loop through the buffered walk over
    /// the expression that produced it.
    fn lazy_elem_is_drivable(&self, elem: gossamer_types::Ty) -> bool {
        use gossamer_types::TyKind;
        matches!(
            self.tcx.kind(elem),
            Some(
                TyKind::Int(gossamer_types::IntTy::I64)
                    | TyKind::Char
                    | TyKind::String
                    | TyKind::Float(gossamer_types::FloatTy::F64)
            )
        )
    }

    fn iter_needs_state_binding(&self, ty: gossamer_types::Ty) -> bool {
        use gossamer_types::TyKind;
        let mut cur = ty;
        for _ in 0..8 {
            match self.tcx.kind(cur) {
                Some(TyKind::Ref { inner, .. }) => cur = *inner,
                // `HashSet` / `BTreeSet` sentinels are not
                // stateful iterator: it snapshots to a sorted Vec on the
                // inline path (VM and compiled both materialise `to_vec`),
                // so keep it off the `&mut __for_iter.next()` desugar that
                // a real `impl Iterator` struct needs.
                Some(TyKind::Adt { def, .. }) => {
                    return !matches!(def.local, HASH_SET_DEF_LOCAL | BTREE_SET_DEF_LOCAL);
                }
                // A type parameter's shape is only known per instantiation,
                // so it advances through `.next()`, the one protocol every
                // iterable answers. Its bound guarantees that method exists.
                Some(TyKind::Param { .. }) => return true,
                _ => return false,
            }
        }
        false
    }

    /// Lowers a `select { … }` expression into a
    /// [`HirExprKind::Select`] that preserves each arm's channel and
    /// body. The interpreter polls channels for readiness at runtime
    /// and picks the first ready arm, falling back to the `default`
    /// arm when none are ready.
    fn lower_select(&mut self, arms: &[gossamer_ast::SelectArm]) -> HirExprKind {
        if arms.is_empty() {
            return HirExprKind::Literal(HirLiteral::Unit);
        }
        let lowered = arms
            .iter()
            .map(|arm| {
                let op = match &arm.op {
                    gossamer_ast::SelectOp::Recv { pattern, channel } => {
                        crate::tree::HirSelectOp::Recv {
                            pattern: self.lower_pat(pattern),
                            channel: self.lower_expr(channel),
                        }
                    }
                    gossamer_ast::SelectOp::Send { channel, value } => {
                        crate::tree::HirSelectOp::Send {
                            channel: self.lower_expr(channel),
                            value: self.lower_expr(value),
                        }
                    }
                    gossamer_ast::SelectOp::Default => crate::tree::HirSelectOp::Default,
                };
                crate::tree::HirSelectArm {
                    op,
                    body: self.lower_expr(&arm.body),
                }
            })
            .collect();
        HirExprKind::Select { arms: lowered }
    }

    /// Returns the `T` payload type when `ty` is a `Result<T, E>`
    /// (or a `&Result<T, E>`), `None` otherwise. Used by `lower_try`
    /// so a `?`-unwrapped binding inherits a real type instead of
    /// the `Error` sentinel.
    fn try_ok_payload_ty(&self, ty: gossamer_types::Ty) -> Option<gossamer_types::Ty> {
        use gossamer_types::TyKind;
        let mut peeled = ty;
        loop {
            match self.tcx.kind(peeled)? {
                TyKind::Ref { inner, .. } => peeled = *inner,
                TyKind::Adt { substs, .. } => {
                    let args = substs.as_slice();
                    if args.is_empty() {
                        return None;
                    }
                    if let Some(gossamer_types::GenericArg::Type(t)) = args.first() {
                        return Some(*t);
                    }
                    return None;
                }
                _ => return None,
            }
        }
    }

    /// Heuristic fallback for `?` operator's `__try_value` type
    /// when the inner expression's HIR type is unresolved. Walks
    /// chained method calls - `fs::read_to_string(...)
    /// .map_err(...)` is a common shape - and returns `String`
    /// for stdlib helpers whose runtime return is a c-string. The
    /// MIR-side `pinned_ret` table is the authoritative source of
    /// truth; this list mirrors its String entries so the HIR layer
    /// can ground a `let s = ...?` binding even when the
    /// typechecker leaks a Var through `?`.
    fn try_ok_payload_ty_heuristic(&mut self, inner: &AstExpr) -> Option<gossamer_types::Ty> {
        let mut cur = inner;
        loop {
            match &cur.kind {
                AstExprKind::MethodCall { receiver, name, .. }
                    if matches!(name.name.as_str(), "map_err" | "map" | "ok" | "err") =>
                {
                    cur = receiver;
                }
                AstExprKind::Call { callee, .. } => {
                    if let AstExprKind::Path(path) = &callee.kind {
                        let joined: Vec<&str> =
                            path.segments.iter().map(|s| s.name.name.as_str()).collect();
                        let last = *joined.last()?;
                        // Match the same names the parse-side
                        // resolves to gos_rt_*_-returning helpers
                        // whose c-string return is logically a
                        // String. If the MIR pin gets it right we
                        // never reach here; this is the last-ditch
                        // path for when the typechecker hasn't
                        // resolved through `?`.
                        if matches!(
                            last,
                            "read_to_string"
                                | "read_line"
                                | "trim"
                                | "to_lowercase"
                                | "to_uppercase"
                                | "replace"
                                | "format"
                                | "join"
                        ) {
                            return Some(self.tcx.string_ty());
                        }
                    }
                    return None;
                }
                _ => return None,
            }
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "`?` desugar covers both Option and Result branches with bespoke HirExpr construction; splitting per-branch helpers would hide the structural symmetry between them"
    )]
    fn lower_try(&mut self, inner: &AstExpr, span: Span) -> HirExprKind {
        let value = self.lower_expr(inner);
        let value_ty = value.ty;
        // Detect Option<T> vs Result<T, E> so `?` desugars to the
        // matching unwrap-or-return-propagate shape. Result is the
        // existing path; Option propagates `None` from the enclosing
        // function via `return None`.
        let kind = self.try_propagation_kind(value_ty, inner);
        let payload_ty = self
            .try_payload_ty(value_ty)
            .or_else(|| self.try_ok_payload_ty_heuristic(inner));
        let try_value_ty = payload_ty.unwrap_or_else(|| self.error_ty());
        let ok_binding_id = self.fresh();
        let ok_variant = match kind {
            TryKind::Option => "Some",
            TryKind::Result => "Ok",
        };
        let ok_pat = HirPat {
            id: self.fresh(),
            span,
            ty: value_ty,
            kind: HirPatKind::Variant {
                name: Ident::new(ok_variant),
                fields: vec![HirPat {
                    id: ok_binding_id,
                    span,
                    ty: try_value_ty,
                    kind: HirPatKind::Binding {
                        name: Ident::new("__try_value"),
                        mutable: false,
                    },
                }],
            },
        };
        let ok_body = HirExpr {
            id: self.fresh(),
            span,
            ty: try_value_ty,
            kind: HirExprKind::Path {
                segments: vec![Ident::new("__try_value")],
                def: None,
            },
        };
        // Build the early-return body. For Option this is `return
        // None`; for Result it's `return Err(__try_err)` (preserving
        // the existing semantics - From-based error conversion would
        // require typeck context not available in HIR lowering).
        let (err_pat, err_body) = match kind {
            TryKind::Option => {
                let none_pat = HirPat {
                    id: self.fresh(),
                    span,
                    ty: value_ty,
                    kind: HirPatKind::Variant {
                        name: Ident::new("None"),
                        fields: Vec::new(),
                    },
                };
                let none_value = HirExpr {
                    id: self.fresh(),
                    span,
                    ty: value_ty,
                    kind: HirExprKind::Path {
                        segments: vec![Ident::new("None")],
                        def: None,
                    },
                };
                let body = HirExpr {
                    id: self.fresh(),
                    span,
                    ty: self.tcx.never(),
                    kind: HirExprKind::Return(Some(Box::new(none_value))),
                };
                (none_pat, body)
            }
            TryKind::Result => {
                // The actual error type `E` of the inner `Result<T,E>` - may
                // be `String`, a user type, or `errors::Error`. Typing the
                // bound error as the concrete `E` (not always `errors::Error`)
                // is what lets a `String`-typed error survive `?` propagation
                // intact rather than being mis-rendered as an error handle.
                let err_ty = self
                    .try_err_payload_ty(value_ty)
                    .unwrap_or_else(|| self.error_ty());
                let err_binding_id = self.fresh();
                let err_pat = HirPat {
                    id: self.fresh(),
                    span,
                    ty: value_ty,
                    kind: HirPatKind::Variant {
                        name: Ident::new("Err"),
                        fields: vec![HirPat {
                            id: err_binding_id,
                            span,
                            ty: err_ty,
                            kind: HirPatKind::Binding {
                                name: Ident::new("__try_err"),
                                mutable: false,
                            },
                        }],
                    },
                };
                let err_value = HirExpr {
                    id: self.fresh(),
                    span,
                    ty: err_ty,
                    kind: HirExprKind::Path {
                        segments: vec![Ident::new("__try_err")],
                        def: None,
                    },
                };
                // SPEC §4.5: `?` propagates with `E: Into<E2>`
                // conversion when the inner error type differs
                // from the enclosing function's error type. We
                // detect the mismatch by comparing the inner
                // value's `Result<_, Inner>` against the outer
                // fn's `Result<_, Outer>` and route the err
                // payload through `Into::into` (the runtime
                // resolves the canonical errors::Error path for
                // String / errors::Error / user types).
                let err_value = self.maybe_convert_try_err(err_value, value_ty, span);
                // `return Err(e)` yields the ENCLOSING FUNCTION's return type
                // (`Result<T,E>`, a 2-word by-value i128), not the bare error
                // type and not the scrutinee's (possibly-unpinned `Var`) type.
                // Pinning it to the concrete fn return type is essential: a
                // `Var` would render as `ptr` and truncate the i128 payload.
                let result_ty = self.current_fn_ret_ty.unwrap_or(value_ty);
                let err_wrap = HirExpr {
                    id: self.fresh(),
                    span,
                    ty: result_ty,
                    kind: HirExprKind::Call {
                        callee: Box::new(HirExpr {
                            id: self.fresh(),
                            span,
                            ty: result_ty,
                            kind: HirExprKind::Path {
                                segments: vec![Ident::new("Err")],
                                def: None,
                            },
                        }),
                        args: vec![err_value],
                    },
                };
                let body = HirExpr {
                    id: self.fresh(),
                    span,
                    ty: self.tcx.never(),
                    kind: HirExprKind::Return(Some(Box::new(err_wrap))),
                };
                (err_pat, body)
            }
        };
        HirExprKind::Match {
            scrutinee: Box::new(value),
            arms: vec![
                HirMatchArm {
                    pattern: ok_pat,
                    guard: None,
                    body: ok_body,
                },
                HirMatchArm {
                    pattern: err_pat,
                    guard: None,
                    body: err_body,
                },
            ],
        }
    }

    /// Decide whether `?` should desugar via `Option::Some/None` or
    /// `Result::Ok/Err`. Defaults to `Result` so behaviour matches
    /// the pre-existing implementation when the type isn't known.
    fn try_propagation_kind(&self, ty: gossamer_types::Ty, inner: &AstExpr) -> TryKind {
        use gossamer_types::TyKind;
        let mut peeled = ty;
        for _ in 0..8 {
            match self.tcx.kind(peeled) {
                Some(TyKind::Ref { inner, .. }) => peeled = *inner,
                Some(TyKind::Adt { def, .. }) => {
                    if let Some(name) = self.tcx.def_name(*def) {
                        return match name {
                            "Option" => TryKind::Option,
                            "Result" => TryKind::Result,
                            _ => TryKind::Result,
                        };
                    }
                    return TryKind::Result;
                }
                _ => break,
            }
        }
        // Syntactic fallback for the case where the typechecker
        // hasn't resolved the inner expression yet - recognise
        // common Option-returning HashMap/Vec lookup shapes so
        // `m.get(&k)?` works even when the inferred type is `Var`.
        if Self::ast_is_option_shaped(inner) {
            TryKind::Option
        } else {
            TryKind::Result
        }
    }

    /// Returns true when `inner` looks like an `Option`-returning
    /// stdlib call by name. Conservative - only the dispatch-table
    /// entries whose runtime return is documented `Option<T>`.
    fn ast_is_option_shaped(inner: &AstExpr) -> bool {
        match &inner.kind {
            AstExprKind::MethodCall { name, .. } => matches!(
                name.name.as_str(),
                "get"
                    | "first"
                    | "last"
                    | "pop"
                    | "find"
                    | "find_opt"
                    | "rfind_opt"
                    | "checked_add"
                    | "checked_sub"
                    | "checked_mul"
                    | "split_once"
                    | "rsplit_once"
                    | "strip_prefix"
                    | "strip_suffix"
                    | "index_of"
            ),
            _ => false,
        }
    }

    /// Returns the payload type for the unwrapped-success branch of
    /// `?`. Works for both `Result<T, E>` and `Option<T>` - both
    /// carry `T` as their first generic argument.
    fn try_payload_ty(&self, ty: gossamer_types::Ty) -> Option<gossamer_types::Ty> {
        self.try_ok_payload_ty(ty)
    }

    /// Returns the Err generic-argument type of a `Result<_, E>`
    /// (or a reference to one), if `ty` resolves to a Result Adt.
    fn try_err_payload_ty(&self, ty: gossamer_types::Ty) -> Option<gossamer_types::Ty> {
        use gossamer_types::TyKind;
        let mut peeled = ty;
        loop {
            match self.tcx.kind(peeled)? {
                TyKind::Ref { inner, .. } => peeled = *inner,
                TyKind::Adt { substs, .. } => {
                    let args = substs.as_slice();
                    if args.len() < 2 {
                        return None;
                    }
                    if let Some(gossamer_types::GenericArg::Type(t)) = args.get(1) {
                        return Some(*t);
                    }
                    return None;
                }
                _ => return None,
            }
        }
    }

    /// If the inner expression's `Result<_, Inner>` error type
    /// differs from the enclosing function's declared `Result<_,
    /// Outer>` error type, route `err_value` through
    /// `errors::Error::from(__try_err)` so SPEC §4.5's
    /// `E: Into<E2>` propagation works. When the types align
    /// already (or when either can't be resolved) returns
    /// `err_value` unchanged so existing single-error-type
    /// programs see no change.
    fn maybe_convert_try_err(
        &mut self,
        err_value: HirExpr,
        value_ty: gossamer_types::Ty,
        span: Span,
    ) -> HirExpr {
        let Some(inner_err) = self.try_err_payload_ty(value_ty) else {
            return err_value;
        };
        let Some(outer_ret) = self.current_fn_ret_ty else {
            return err_value;
        };
        let Some(outer_err) = self.try_err_payload_ty(outer_ret) else {
            return err_value;
        };
        if inner_err == outer_err {
            return err_value;
        }
        // Mismatched err types - emit `errors::Error::from(__try_err)`.
        // The std `errors::Error::from` is registered as the canonical
        // String / errors::Error / anyhow-style adapter; programs that
        // declare custom err types can extend it.
        HirExpr {
            id: self.fresh(),
            span,
            ty: outer_err,
            kind: HirExprKind::Call {
                callee: Box::new(HirExpr {
                    id: self.fresh(),
                    span,
                    ty: self.error_ty(),
                    kind: HirExprKind::Path {
                        segments: vec![
                            Ident::new("errors"),
                            Ident::new("Error"),
                            Ident::new("from"),
                        ],
                        def: None,
                    },
                }),
                args: vec![err_value],
            },
        }
    }

    fn lower_tuple_struct_call(
        &mut self,
        callee: &AstExpr,
        args: &[AstExpr],
        span: Span,
    ) -> Option<HirExprKind> {
        let AstExprKind::Path(path) = &callee.kind else {
            return None;
        };
        let Some(Resolution::Def {
            def,
            kind: gossamer_resolve::DefKind::Struct,
        }) = self.resolutions.get(callee.id)
        else {
            return None;
        };
        if !self.tcx.is_tuple_struct(def.local) {
            return None;
        }
        let called_name = path.segments.last()?.name.name.as_str();
        // A type's registered name carries the modules that contain it, so
        // compare the written leaf against the identity's leaf.
        let identity = self.tcx.def_name(def)?.to_string();
        if identity.rsplit("::").next() != Some(called_name) {
            return None;
        }
        let field_count = self.tcx.struct_field_tys(def)?.len();
        if field_count != args.len() {
            return None;
        }
        let name = identity;
        let error_ty = self.error_ty();
        let string_ty = self.error_ty();
        let mut struct_args = Vec::with_capacity(1 + args.len() * 2);
        struct_args.push(HirExpr {
            id: self.fresh(),
            span,
            ty: string_ty,
            kind: HirExprKind::Literal(HirLiteral::String(name)),
        });
        for (idx, arg) in args.iter().enumerate() {
            struct_args.push(HirExpr {
                id: self.fresh(),
                span,
                ty: string_ty,
                kind: HirExprKind::Literal(HirLiteral::String(idx.to_string())),
            });
            struct_args.push(self.lower_expr(arg));
        }
        Some(HirExprKind::Call {
            callee: Box::new(HirExpr {
                id: self.fresh(),
                span,
                ty: error_ty,
                kind: HirExprKind::Path {
                    segments: vec![Ident::new("__struct")],
                    def: None,
                },
            }),
            args: struct_args,
        })
    }

    fn lower_reverse_call(
        &mut self,
        callee: &AstExpr,
        args: &[AstExpr],
        span: Span,
    ) -> Option<HirExprKind> {
        let AstExprKind::Path(path) = &callee.kind else {
            return None;
        };
        if path.segments.len() != 1 || path.segments[0].name.name != "Reverse" || args.len() != 1 {
            return None;
        }
        let error_ty = self.error_ty();
        let string_ty = self.error_ty();
        let struct_args = vec![
            HirExpr {
                id: self.fresh(),
                span,
                ty: string_ty,
                kind: HirExprKind::Literal(HirLiteral::String("Reverse".to_string())),
            },
            HirExpr {
                id: self.fresh(),
                span,
                ty: string_ty,
                kind: HirExprKind::Literal(HirLiteral::String("0".to_string())),
            },
            self.lower_expr(&args[0]),
        ];
        Some(HirExprKind::Call {
            callee: Box::new(HirExpr {
                id: self.fresh(),
                span,
                ty: error_ty,
                kind: HirExprKind::Path {
                    segments: vec![Ident::new("__struct")],
                    def: None,
                },
            }),
            args: struct_args,
        })
    }

    /// Lowers `Path { field: value, … }` into a call to the synthetic
    /// `__struct` builtin. The resulting argument list interleaves
    /// field-name strings with their lowered value expressions:
    ///
    /// `Shape::Rect { w: 2.0, h: 4.0 }` → `__struct("Rect", "w", 2.0, "h", 4.0)`.
    ///
    /// When the literal carries a functional-update base
    /// (`Outer { n: 99, ..base }`), the lowered call also includes a
    /// trailing `"__base", base_expr` pair. The MIR layer fills any
    /// missing fields by reading `base.field` via projection.
    ///
    /// The VM and codegen layers can recognise `__struct` as the
    /// canonical struct-literal constructor without needing a new HIR
    /// node variant.
    fn lower_struct_literal(
        &mut self,
        node: NodeId,
        path: &gossamer_ast::PathExpr,
        fields: &[gossamer_ast::StructExprField],
        base: Option<&gossamer_ast::Expr>,
        span: Span,
    ) -> HirExprKind {
        let mut name = path
            .segments
            .last()
            .map(|seg| seg.name.name.clone())
            .unwrap_or_default();
        if let Some(Resolution::Def { def, .. }) = self.resolutions.get(node)
            && let Some(promoted) = self.module_fn_paths.get(&def)
            && let Some(promoted_name) = promoted.last()
        {
            name.clone_from(&promoted_name.name);
        }
        // A type declared in a module is identified by its qualified name,
        // matching what the type checker registers, so two modules may
        // declare the same name without their constructors, `{:?}` dispatch,
        // or native registry entries colliding.
        if let Some(Resolution::Def { def, .. }) = self.resolutions.get(node)
            && let Some(identity) = self.module_type_names.get(&def)
            // The identity names the TYPE. A literal that names a variant of
            // it - `Value::Attr { .. }` - resolves to the same def, and the
            // constructor is keyed by the variant, so only a literal whose
            // own last segment is the type takes the qualified spelling.
            && identity.rsplit("::").next() == Some(name.as_str())
        {
            name.clone_from(identity);
        }
        // A literal may name its type through an import (`use a::Point`,
        // `use a::Point as P`); the import's target path is that identity.
        if let Some(Resolution::Import { use_id }) = self.resolutions.get(node)
            && let Some(entries) = self.import_targets.get(&use_id)
            && let Some((_, full)) = entries.iter().find(|(bound, _)| *bound == name)
        {
            name = full
                .iter()
                .map(|segment| segment.name.as_str())
                .filter(|segment| !matches!(*segment, "crate" | "self" | "super" | "root"))
                .collect::<Vec<_>>()
                .join("::");
        }
        let error_ty = self.error_ty();
        let string_ty = self.error_ty();
        let mut args = Vec::with_capacity(1 + fields.len() * 2 + 2);
        args.push(HirExpr {
            id: self.fresh(),
            span,
            ty: string_ty,
            kind: HirExprKind::Literal(HirLiteral::String(name)),
        });
        let field_names = self.resolve_struct_literal_field_names(
            path.segments
                .last()
                .map(|seg| seg.name.name.as_str())
                .unwrap_or_default(),
            fields,
        );
        let field_order = self.struct_literal_field_order(
            path.segments
                .last()
                .map(|seg| seg.name.name.as_str())
                .unwrap_or_default(),
            fields,
            &field_names,
        );
        for idx in field_order {
            let field = &fields[idx];
            args.push(HirExpr {
                id: self.fresh(),
                span,
                ty: string_ty,
                kind: HirExprKind::Literal(HirLiteral::String(
                    field_names
                        .get(&idx)
                        .cloned()
                        .unwrap_or_else(|| field.name.name.clone()),
                )),
            });
            let value = match &field.value {
                Some(expr) => self.lower_expr(expr),
                None => HirExpr {
                    id: self.fresh(),
                    span,
                    ty: error_ty,
                    kind: HirExprKind::Path {
                        segments: vec![field.name.clone()],
                        def: None,
                    },
                },
            };
            args.push(value);
        }
        if let Some(base_expr) = base {
            args.push(HirExpr {
                id: self.fresh(),
                span,
                ty: string_ty,
                kind: HirExprKind::Literal(HirLiteral::String("__base".to_string())),
            });
            args.push(self.lower_expr(base_expr));
        }
        HirExprKind::Call {
            callee: Box::new(HirExpr {
                id: self.fresh(),
                span,
                ty: error_ty,
                kind: HirExprKind::Path {
                    segments: vec![Ident::new("__struct")],
                    def: None,
                },
            }),
            args,
        }
    }

    fn struct_literal_field_order(
        &self,
        struct_name: &str,
        fields: &[gossamer_ast::StructExprField],
        resolved_names: &std::collections::HashMap<usize, String>,
    ) -> Vec<usize> {
        let Some(declared) = self.struct_fields.get(struct_name) else {
            return (0..fields.len()).collect();
        };
        let mut order = Vec::with_capacity(fields.len());
        let mut used = std::collections::HashSet::new();
        for declared_name in declared {
            for (field_idx, _) in fields.iter().enumerate() {
                if resolved_names
                    .get(&field_idx)
                    .is_some_and(|name| name == declared_name)
                {
                    order.push(field_idx);
                    used.insert(field_idx);
                }
            }
        }
        for field_idx in 0..fields.len() {
            if !used.contains(&field_idx) {
                order.push(field_idx);
            }
        }
        order
    }

    fn resolve_struct_literal_field_names(
        &self,
        struct_name: &str,
        fields: &[gossamer_ast::StructExprField],
    ) -> std::collections::HashMap<usize, String> {
        let Some(declared) = self.struct_fields.get(struct_name) else {
            return std::collections::HashMap::new();
        };
        let declared_by_name: std::collections::HashMap<&str, usize> = declared
            .iter()
            .enumerate()
            .map(|(idx, name)| (name.as_str(), idx))
            .collect();
        let mut resolved = std::collections::HashMap::new();
        let mut filled = std::collections::HashSet::new();
        for (field_idx, field) in fields.iter().enumerate() {
            if struct_literal_positional_index(&field.name.name).is_some() {
                continue;
            }
            if let Some(&decl_idx) = declared_by_name.get(field.name.name.as_str()) {
                filled.insert(decl_idx);
                resolved.insert(field_idx, declared[decl_idx].clone());
            }
        }

        let mut next_pos = 0usize;
        for (field_idx, field) in fields.iter().enumerate() {
            if struct_literal_positional_index(&field.name.name).is_none() {
                continue;
            }
            while next_pos < declared.len() && filled.contains(&next_pos) {
                next_pos += 1;
            }
            if next_pos >= declared.len() {
                continue;
            }
            filled.insert(next_pos);
            resolved.insert(field_idx, declared[next_pos].clone());
        }
        resolved
    }

    fn lower_array(&mut self, arr: &AstArrayExpr) -> HirArrayExpr {
        match arr {
            AstArrayExpr::List(elems) => {
                HirArrayExpr::List(elems.iter().map(|e| self.lower_expr(e)).collect())
            }
            AstArrayExpr::Repeat { value, count } => HirArrayExpr::Repeat {
                value: Box::new(self.lower_expr(value)),
                count: Box::new(self.lower_expr(count)),
            },
        }
    }

    /// `true` when a map value is a by-value aggregate. Such a value lives in
    /// the entry array as inline slots rather than as one word, so the literal
    /// is built by inserting each pair instead of from the array.
    fn map_value_is_aggregate(&self, map_ty: Ty) -> bool {
        use gossamer_types::TyKind;
        let Some(TyKind::HashMap { value, .. }) = self.tcx.kind(map_ty) else {
            return false;
        };
        matches!(
            self.tcx.kind(*value),
            Some(TyKind::Tuple(_) | TyKind::Adt { .. })
        )
    }

    /// Builds `{ let mut m = Map::new(); m.insert(k, v); ...; m }` for a map
    /// whose values are aggregates.
    fn lower_map_literal_by_insert(
        &mut self,
        entries: &[AstExpr],
        span: Span,
        map_ty: Ty,
    ) -> HirExprKind {
        let name = Ident::new("__gos_map_literal");
        let ctor = HirExpr {
            id: self.fresh(),
            span,
            ty: map_ty,
            kind: HirExprKind::Call {
                callee: Box::new(HirExpr {
                    id: self.fresh(),
                    span,
                    ty: self.error_ty(),
                    kind: HirExprKind::Path {
                        segments: vec![Ident::new("Map"), Ident::new("new")],
                        def: None,
                    },
                }),
                args: Vec::new(),
            },
        };
        let mut stmts = vec![HirStmt {
            id: self.fresh(),
            span,
            kind: HirStmtKind::Let {
                pattern: HirPat {
                    id: self.fresh(),
                    span,
                    ty: map_ty,
                    kind: HirPatKind::Binding {
                        name: name.clone(),
                        mutable: true,
                    },
                },
                ty: map_ty,
                init: Some(ctor),
            },
        }];
        for entry in entries {
            let AstExprKind::Tuple(parts) = &entry.kind else {
                continue;
            };
            let [key, value] = parts.as_slice() else {
                continue;
            };
            let receiver = HirExpr {
                id: self.fresh(),
                span,
                ty: map_ty,
                kind: HirExprKind::Path {
                    segments: vec![name.clone()],
                    def: None,
                },
            };
            let call = HirExpr {
                id: self.fresh(),
                span,
                ty: self.tcx.unit(),
                kind: HirExprKind::MethodCall {
                    receiver: Box::new(receiver),
                    name: Ident::new("insert"),
                    args: vec![self.lower_expr(key), self.lower_expr(value)],
                    owner: None,
                },
            };
            stmts.push(HirStmt {
                id: self.fresh(),
                span,
                kind: HirStmtKind::Expr {
                    expr: call,
                    has_semi: true,
                },
            });
        }
        let tail = HirExpr {
            id: self.fresh(),
            span,
            ty: map_ty,
            kind: HirExprKind::Path {
                segments: vec![name],
                def: None,
            },
        };
        HirExprKind::Block(HirBlock {
            id: self.fresh(),
            span,
            ty: map_ty,
            stmts,
            tail: Some(Box::new(tail)),
            is_comptime: false,
        })
    }

    /// Rewrites a traversal on a map or set into the walk over the iterator
    /// its `iter()` answers, materialising with `collect()` when the traversal
    /// yields a sequence. Returns `None` for every other receiver.
    fn desugar_keyed_traversal(
        &mut self,
        expr: &AstExpr,
        receiver: &AstExpr,
        name: &Ident,
        args: &[AstExpr],
    ) -> Option<HirExprKind> {
        use gossamer_types::TyKind;

        if !gossamer_types::is_collection_traversal_method(name.name.as_str())
            || name.name == "iter"
        {
            return None;
        }
        // A reference to a keyed collection walks the collection it names, so
        // the container's own type decides the element. The cursor call below
        // takes the receiver as written, which the `iter()` lowering already
        // reads through a borrow.
        let mut recv_ty = self.ty_of(receiver.id);
        while let Some(TyKind::Ref { inner, .. }) = self.tcx.kind(recv_ty) {
            recv_ty = *inner;
        }
        // The element a walk sees: a map yields its key/value pair, a set its
        // value.
        let elem = match self.tcx.kind(recv_ty) {
            Some(TyKind::HashMap { key, value, .. }) => {
                let (key, value) = (*key, *value);
                self.tcx.intern(TyKind::Tuple(vec![key, value]))
            }
            Some(TyKind::Adt { def, substs })
                if def.local == u32::MAX - 7 || def.local == u32::MAX - 18 =>
            {
                *substs.types().first()?
            }
            _ => return None,
        };
        let span = expr.span;
        let out_ty = self.ty_of(expr.id);
        let lowered_receiver = self.lower_expr(receiver);
        let cursor = HirExpr {
            id: self.fresh(),
            span,
            ty: self.tcx.intern(TyKind::Iterator(elem)),
            kind: HirExprKind::MethodCall {
                receiver: Box::new(lowered_receiver),
                name: Ident::new("iter"),
                args: Vec::new(),
                owner: None,
            },
        };
        let walked = HirExprKind::MethodCall {
            receiver: Box::new(cursor),
            name: name.clone(),
            args: args.iter().map(|a| self.lower_expr(a)).collect(),
            owner: None,
        };
        // An adapter answers another iterator, so a sequence result is
        // materialised the way the eager spelling promises.
        let out_elem = match self.tcx.kind(out_ty) {
            Some(TyKind::Vec(out_elem)) => *out_elem,
            _ => return Some(walked),
        };
        let inner = HirExpr {
            id: self.fresh(),
            span,
            ty: self.tcx.intern(TyKind::Iterator(out_elem)),
            kind: walked,
        };
        Some(HirExprKind::MethodCall {
            receiver: Box::new(inner),
            name: Ident::new("collect"),
            args: Vec::new(),
            owner: None,
        })
    }

    fn lower_map_literal(&mut self, entries: &[AstExpr], span: Span, map_ty: Ty) -> HirExprKind {
        use gossamer_types::{ArrayLen, TyKind};

        if !entries.is_empty() && self.map_value_is_aggregate(map_ty) {
            return self.lower_map_literal_by_insert(entries, span, map_ty);
        }
        let lowered_entries: Vec<HirExpr> = entries.iter().map(|e| self.lower_expr(e)).collect();
        let pair_ty = lowered_entries.first().map_or_else(
            || match self.tcx.kind(map_ty) {
                Some(TyKind::HashMap { key, value, .. }) => {
                    self.tcx.intern(TyKind::Tuple(vec![*key, *value]))
                }
                _ => self.error_ty(),
            },
            |entry| entry.ty,
        );
        let array_ty = self.tcx.intern(TyKind::Array {
            elem: pair_ty,
            len: ArrayLen::Concrete(lowered_entries.len()),
        });
        let array_arg = HirExpr {
            id: self.fresh(),
            span,
            ty: array_ty,
            kind: HirExprKind::Array(HirArrayExpr::List(lowered_entries)),
        };
        let callee = HirExpr {
            id: self.fresh(),
            span,
            ty: self.error_ty(),
            kind: HirExprKind::Path {
                segments: vec![Ident::new("Map"), Ident::new("from")],
                def: None,
            },
        };
        HirExprKind::Call {
            callee: Box::new(callee),
            args: vec![array_arg],
        }
    }

    fn lower_set_literal(&mut self, entries: &[AstExpr], span: Span, set_ty: Ty) -> HirExprKind {
        use gossamer_types::{ArrayLen, TyKind};

        let lowered_entries: Vec<HirExpr> = entries.iter().map(|e| self.lower_expr(e)).collect();
        let owner = match self.tcx.kind(set_ty) {
            Some(TyKind::Adt { def, .. }) if def.local == BTREE_SET_DEF_LOCAL => "BTreeSet",
            _ => "Set",
        };
        let elem_ty = lowered_entries
            .first()
            .map(|entry| entry.ty)
            .or_else(|| match self.tcx.kind(set_ty) {
                Some(TyKind::Adt { def, substs }) if def.local == HASH_SET_DEF_LOCAL => {
                    substs.types().first().copied()
                }
                Some(TyKind::Adt { def, substs }) if def.local == BTREE_SET_DEF_LOCAL => {
                    substs.types().first().copied()
                }
                _ => None,
            })
            .unwrap_or_else(|| self.error_ty());
        let array_ty = self.tcx.intern(TyKind::Array {
            elem: elem_ty,
            len: ArrayLen::Concrete(lowered_entries.len()),
        });
        let array_arg = HirExpr {
            id: self.fresh(),
            span,
            ty: array_ty,
            kind: HirExprKind::Array(HirArrayExpr::List(lowered_entries)),
        };
        let callee = HirExpr {
            id: self.fresh(),
            span,
            ty: self.error_ty(),
            kind: HirExprKind::Path {
                segments: vec![Ident::new(owner), Ident::new("from")],
                def: None,
            },
        };
        HirExprKind::Call {
            callee: Box::new(callee),
            args: vec![array_arg],
        }
    }

    fn lower_closure_params(&mut self, params: &[AstClosureParam]) -> Vec<HirParam> {
        params
            .iter()
            .map(|param| {
                let ty = match &param.ty {
                    Some(ast_ty) => self.ty_of(ast_ty.id),
                    // Look up the pattern's resolved type from the checker. If
                    // the call site unified the param with a concrete type (e.g.
                    // String when sorting a Vec<String>), this picks it up
                    // instead of emitting the opaque error sentinel.
                    None => self.ty_of(param.pattern.id),
                };
                let pattern = self.lower_pat_with_ty(&param.pattern, ty);
                HirParam {
                    pattern,
                    ty,
                    is_comptime: false,
                }
            })
            .collect()
    }

    /// `tail` prefixed with the innermost enclosing module path under which
    /// an `impl` function of that name is registered, or `None` when no
    /// enclosing module declares one.
    ///
    /// A `Type::assoc` path is spelled relative to the module it is written
    /// in, while the impl's body is keyed by the type's full module-qualified
    /// identity. Walking outward from the innermost module lets an inner
    /// module's item win over a same-named one further out, matching name
    /// resolution.
    fn anchor_impl_fn_path(&self, tail: &[&str], from_depth: usize) -> Option<Vec<Ident>> {
        let joined = tail.join("::");
        let from_depth = from_depth.min(self.current_module.len());
        for depth in (1..=from_depth).rev() {
            let prefix = self.current_module[..depth].join("::");
            if self
                .module_impl_fns
                .contains(&format!("{prefix}::{joined}"))
            {
                return Some(
                    self.current_module[..depth]
                        .iter()
                        .map(Ident::new)
                        .chain(tail.iter().map(|s| Ident::new(*s)))
                        .collect(),
                );
            }
        }
        None
    }

    /// Full spelling of a single-segment name bound by `use` to a stdlib free
    /// function or a registered `[rust-bindings]` item, if it names one.
    fn imported_leaf_path(&self, node: NodeId, leaf: &Ident) -> Option<Vec<Ident>> {
        let Some(Resolution::Import { use_id }) = self.resolutions.get(node) else {
            return None;
        };
        let entries = self.import_targets.get(&use_id)?;
        let (_, full) = entries.iter().find(|(bound, _)| *bound == leaf.name)?;
        let qualified = full
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join("::");
        let std_qualified = qualified
            .strip_prefix("std::")
            .is_some_and(gossamer_resolve::is_stdlib_qualified);
        if std_qualified {
            Some(full.iter().skip(1).cloned().collect())
        } else if gossamer_resolve::lookup_external_item(&qualified).is_some() {
            Some(full.clone())
        } else {
            None
        }
    }

    /// Module-qualified spelling of a `Type::assoc` path whose `Type` came from
    /// a `use`, which is the key the impl's body is registered under.
    fn imported_assoc_path(&self, node: NodeId, segments: &[Ident]) -> Option<Vec<Ident>> {
        let Some(Resolution::Import { use_id }) = self.resolutions.get(node) else {
            return None;
        };
        let entries = self.import_targets.get(&use_id)?;
        let (_, full) = entries
            .iter()
            .find(|(bound, _)| *bound == segments[0].name)?;
        let mut target: Vec<&str> = full
            .iter()
            .map(|s| s.name.as_str())
            .filter(|s| !matches!(*s, "crate" | "self" | "super" | "root"))
            .collect();
        target.push(segments[1].name.as_str());
        if self.module_impl_fns.contains(&target.join("::")) {
            return Some(target.iter().map(|s| Ident::new(*s)).collect());
        }
        // The import target is spelled relative to the module the `use` was
        // written in, so the impl it names is registered under that module's
        // own path.
        self.anchor_impl_fn_path(&target, self.current_module.len())
    }

    fn lower_path_expr(&mut self, node: NodeId, path: &gossamer_ast::PathExpr) -> HirExprKind {
        let mut segments: Vec<Ident> = path.segments.iter().map(|s| s.name.clone()).collect();
        // A single-segment name bound by `use` and targeting a
        // `[rust-bindings]` item or stdlib free function expands to its full
        // qualified path.
        // Several binding modules can expose the same leaf (eight
        // tuigoose modules each define `with_block`); the bare-leaf
        // dispatch tables disambiguate by arity only, so an imported
        // name must carry its module to dispatch to the item the
        // program actually imported. std / user imports are
        // untouched - the gate is a registered external item.
        if segments.len() == 1
            && let Some(expanded) = self.imported_leaf_path(node, &segments[0])
        {
            segments = expanded;
        }
        // The resolver has already used a leading `crate` / `self` /
        // `super` / `root` to pick the target, and nothing below HIR keys
        // on those spellings, so drop them before any name-keyed
        // dispatch sees the path.
        // Only a leading qualifier on a multi-segment path routes; a
        // lone `self` is the receiver binding, not a route.
        let qualified_spelling = segments.len() > 1
            && matches!(
                segments[0].name.as_str(),
                "crate" | "self" | "super" | "root"
            );
        // How far out the enclosing module chain a `Type::assoc` /
        // `Enum::Variant` path is anchored: the current module for a bare
        // or `self::`-relative path, one level out per `super::`, and
        // nowhere for a `crate::`-rooted path, which already names its
        // route from the package root.
        let mut anchor_depth = Some(self.current_module.len());
        if qualified_spelling {
            while segments.len() > 1
                && matches!(
                    segments[0].name.as_str(),
                    "crate" | "self" | "super" | "root"
                )
            {
                match segments[0].name.as_str() {
                    // Inside an inlined dependency, the package root is that
                    // dependency's own module, not the consuming package's.
                    "crate" | "root" => {
                        anchor_depth = self
                            .current_module
                            .first()
                            .filter(|outermost| self.dependency_modules.contains(*outermost))
                            .map(|_| 1);
                    }
                    "super" => {
                        anchor_depth = anchor_depth.map(|depth| depth.saturating_sub(1));
                    }
                    _ => {}
                }
                segments.remove(0);
            }
        }
        // A `Type::assoc` or `Enum::Variant` written inside a module names
        // that module's own item, whose body is keyed by the qualified
        // spelling. Walk outward from the anchor so an inner module's item
        // wins over a same-named one further out, matching name resolution.
        if let Some(depth) = anchor_depth
            && segments.len() >= 2
            && depth > 0
        {
            let tail: Vec<&str> = segments.iter().map(|s| s.name.as_str()).collect();
            if let Some(anchored) = self.anchor_impl_fn_path(&tail, depth) {
                segments = anchored;
            }
        }
        // A `Type::assoc` whose `Type` came from a `use` names an impl
        // keyed by the type's module-qualified spelling, so the import's
        // target is what the name-keyed dispatch has to see. Without this
        // the call type-checks and is unbound at run time.
        if !qualified_spelling
            && segments.len() == 2
            && let Some(target) = self.imported_assoc_path(node, &segments)
        {
            segments = target;
        }
        // A path headed by a `use "id" as alias` binding names items
        // registered under the dependency module's real name, so the
        // head is respelled before any name-keyed dispatch sees it.
        if segments.len() > 1
            && let Some(real) = self
                .resolutions
                .project_alias(&segments[0].name)
                .or_else(|| self.resolutions.module_alias(&segments[0].name))
        {
            let real = real.to_string();
            let mut rest = segments.split_off(1);
            segments = real.split("::").map(Ident::new).collect();
            segments.append(&mut rest);
        }
        // For a multi-segment path whose head only resolves to a
        // module (no qualified-name registration), the resolver
        // leaves the resolution as the `Mod` def. The MIR /
        // codegen `def`-based dispatch can't use that, so drop
        // the def and let the joined-name dispatch take over.
        let def = match self.resolutions.get(node) {
            Some(Resolution::Def {
                def,
                kind: gossamer_resolve::DefKind::Mod,
            }) if segments.len() > 1 => {
                let _ = def;
                None
            }
            Some(Resolution::Def { def, .. }) => Some(def),
            // A `use`-imported name keeps its opaque import resolution, so
            // the definition it targets is what carries the canonical
            // spelling the rewrite below needs.
            Some(Resolution::Import { .. }) => self.resolutions.import_def(node),
            _ => None,
        };
        // A reference to an inline-module function - bare from inside
        // the module, `super::`-relative, or already qualified -
        // rewrites to the canonical `mod::name` spelling so every
        // tier's name-keyed dispatch names this def unambiguously.
        if let Some(def) = def
            && let Some(full) = self.module_fn_paths.get(&def)
        {
            segments.clone_from(full);
        }
        HirExprKind::Path { segments, def }
    }

    fn lower_block(&mut self, block: &AstBlock, span: Span) -> HirBlock {
        let mut stmts = Vec::new();
        for stmt in &block.stmts {
            stmts.push(self.lower_stmt(stmt));
        }
        // A tail expression whose value is unit is statement-shaped
        // (loop bodies, single-call blocks) - the entry-mutation
        // desugar applies there like any expression statement.
        let tail = block.tail.as_ref().map(|tail| {
            let unit_tail = {
                let t = self.ty_of(tail.id);
                matches!(self.tcx.kind_of(t), gossamer_types::TyKind::Unit)
            };
            let lowered = if unit_tail {
                self.desugar_or_insert_mutation(tail)
            } else {
                None
            };
            Box::new(lowered.unwrap_or_else(|| self.lower_expr(tail)))
        });
        let ty = match tail.as_ref() {
            Some(expr) => expr.ty,
            None => self.unit(),
        };
        HirBlock {
            id: self.fresh(),
            span,
            stmts,
            tail,
            ty,
            is_comptime: block.is_comptime(),
        }
    }

    fn lower_expr_as_block(&mut self, expr: &AstExpr) -> HirBlock {
        if let AstExprKind::Block(block) = &expr.kind {
            return self.lower_block(block, expr.span);
        }
        let lowered = self.lower_expr(expr);
        let ty = lowered.ty;
        HirBlock {
            id: self.fresh(),
            span: expr.span,
            stmts: Vec::new(),
            tail: Some(Box::new(lowered)),
            ty,
            is_comptime: false,
        }
    }

    fn lower_stmt(&mut self, stmt: &AstStmt) -> HirStmt {
        let kind = match &stmt.kind {
            AstStmtKind::Let { pattern, ty, init } => {
                let declared_ty = match ty.as_ref() {
                    Some(ast_ty) => self.ty_of(ast_ty.id),
                    None => self.error_ty(),
                };
                let init = init.as_ref().map(|expr| self.lower_expr(expr));
                // Prefer the user-written annotation over the
                // initialiser's inferred type - the annotation is
                // already what the typechecker unified the init
                // expression against, and a concrete `Result<T, E>`
                // / `Option<T>` annotation is much more useful to
                // MIR's downstream `Adt` substs lookups than the
                // raw `Var(_)` an inference variable would carry
                // through. Falls back to the init's type when no
                // annotation was written.
                let pattern_ty =
                    if matches!(self.tcx.kind_of(declared_ty), gossamer_types::TyKind::Error) {
                        init.as_ref().map_or(declared_ty, |expr| expr.ty)
                    } else {
                        declared_ty
                    };
                let pattern = self.lower_pat_with_ty(pattern, pattern_ty);
                HirStmtKind::Let {
                    pattern,
                    ty: pattern_ty,
                    init,
                }
            }
            AstStmtKind::Expr { expr, has_semi } => {
                let expr = self
                    .desugar_or_insert_mutation(expr)
                    .unwrap_or_else(|| self.lower_expr(expr));
                HirStmtKind::Expr {
                    expr,
                    has_semi: *has_semi,
                }
            }
            AstStmtKind::Item(item) => {
                if let Some(lowered) = self.lower_item(item, &[]) {
                    if matches!(lowered.kind, HirItemKind::Fn(_) | HirItemKind::Adt(_))
                        && let Some(def) = lowered.def
                        && let Some(path) = self.module_fn_paths.get(&def)
                    {
                        let mut promoted = lowered.clone();
                        let promoted_name =
                            path.last().cloned().expect("nested item path is non-empty");
                        match &mut promoted.kind {
                            HirItemKind::Fn(decl) => decl.name = promoted_name,
                            HirItemKind::Adt(decl) => decl.name = promoted_name,
                            _ => unreachable!("only functions and ADTs are promoted"),
                        }
                        self.promoted_items.push(promoted);
                    }
                    HirStmtKind::Item(Box::new(lowered))
                } else {
                    HirStmtKind::Expr {
                        expr: self.placeholder_expr(stmt.span),
                        has_semi: false,
                    }
                }
            }
            AstStmtKind::Defer(inner) => HirStmtKind::Defer(self.lower_expr(inner)),
        };
        HirStmt {
            id: self.fresh(),
            span: stmt.span,
            kind,
        }
    }

    /// One `let` of the entry desugars (`let [mut] name: ty = init`).
    fn entry_let_stmt(
        &mut self,
        span: Span,
        name: &str,
        ty: gossamer_types::Ty,
        mutable: bool,
        init: HirExpr,
    ) -> HirStmt {
        HirStmt {
            id: self.fresh(),
            span,
            kind: HirStmtKind::Let {
                pattern: HirPat {
                    id: self.fresh(),
                    span,
                    ty,
                    kind: HirPatKind::Binding {
                        name: Ident::new(name),
                        mutable,
                    },
                },
                ty,
                init: Some(init),
            },
        }
    }

    /// A bare path expression referencing one of the entry bindings.
    fn entry_path(&mut self, span: Span, name: &str, ty: gossamer_types::Ty) -> HirExpr {
        HirExpr {
            id: self.fresh(),
            span,
            ty,
            kind: HirExprKind::Path {
                segments: vec![Ident::new(name)],
                def: None,
            },
        }
    }

    /// The shared prelude of the entry desugars:
    /// `let __entry_k = k; let mut __entry_v = option::unwrap_or(d, m.get(__entry_k))`.
    /// The get-or-default shape materialises an inline aggregate local
    /// on every tier (the `or_insert` shims carry scalar values only).
    fn entry_prelude(
        &mut self,
        span: Span,
        key: HirExpr,
        default: HirExpr,
        map: HirExpr,
        value_ty: gossamer_types::Ty,
    ) -> (HirStmt, HirStmt) {
        let key_ty = key.ty;
        let k_let = self.entry_let_stmt(span, "__entry_k", key_ty, false, key);
        let k_for_get = self.entry_path(span, "__entry_k", key_ty);
        let value_substs = gossamer_types::Substs::from_types([value_ty]);
        let option_value_ty = self.tcx.intern(gossamer_types::TyKind::Adt {
            def: gossamer_resolve::DefId::local(u32::MAX - 1),
            substs: value_substs,
        });
        let get_call = HirExpr {
            id: self.fresh(),
            span,
            ty: option_value_ty,
            kind: HirExprKind::MethodCall {
                receiver: Box::new(map),
                name: Ident::new("get"),
                args: vec![k_for_get],
                owner: None,
            },
        };
        let default_callee = HirExpr {
            id: self.fresh(),
            span,
            ty: value_ty,
            kind: HirExprKind::Path {
                segments: vec![Ident::new("option"), Ident::new("unwrap_or")],
                def: None,
            },
        };
        let get_or_default = HirExpr {
            id: self.fresh(),
            span,
            ty: value_ty,
            kind: HirExprKind::Call {
                callee: Box::new(default_callee),
                args: vec![default, get_call],
            },
        };
        let v_let = self.entry_let_stmt(span, "__entry_v", value_ty, true, get_or_default);
        (k_let, v_let)
    }

    /// The write-back call of the entry desugars:
    /// `m.insert(__entry_k, __entry_v)`.
    fn entry_insert_call(
        &mut self,
        span: Span,
        map: HirExpr,
        key_ty: gossamer_types::Ty,
        value_ty: gossamer_types::Ty,
    ) -> HirExpr {
        let k = self.entry_path(span, "__entry_k", key_ty);
        let v = self.entry_path(span, "__entry_v", value_ty);
        let unit_ty = self.unit();
        HirExpr {
            id: self.fresh(),
            span,
            ty: unit_ty,
            kind: HirExprKind::MethodCall {
                receiver: Box::new(map),
                name: Ident::new("insert"),
                args: vec![k, v],
                owner: None,
            },
        }
    }

    /// Desugars a value-position `m.or_insert(k, d)` whose VALUE type is
    /// an aggregate into `{ let __k = k; let mut __v = get-or-default;
    /// m.insert(__k, __v); __v }`. The scalar shims store an 8-byte
    /// value word; an aggregate default's stack word stored raw leaves
    /// the map pointing at a dead frame slot, while get / default /
    /// insert all carry aggregates correctly on every tier.
    fn desugar_or_insert_value(&mut self, expr: &AstExpr) -> Option<HirExpr> {
        let AstExprKind::MethodCall {
            receiver: map_expr,
            name,
            args,
            ..
        } = &expr.kind
        else {
            return None;
        };
        if name.name.as_str() != "or_insert" || args.len() != 2 {
            return None;
        }
        if !matches!(map_expr.kind, AstExprKind::Path(_)) {
            return None;
        }
        let map_ty = self.ty_of(map_expr.id);
        let gossamer_types::TyKind::HashMap { value, .. } = self.tcx.kind_of(map_ty) else {
            return None;
        };
        // Struct / tuple values only: the scalar shims store their
        // stack word raw (a dead frame slot once the statement ends).
        // Vec-valued maps keep the engineered borrow path (`or_insert`
        // returns an alias of the stored vec, marked borrowed so
        // teardown frees it exactly once); scalars keep the fast shims.
        let value_kind = self.tcx.kind_of(*value);
        if !matches!(
            value_kind,
            gossamer_types::TyKind::Adt { .. } | gossamer_types::TyKind::Tuple(_)
        ) {
            return None;
        }
        let span = expr.span;
        let value_ty = self.ty_of(expr.id);
        let key = self.lower_expr(&args[0]);
        let default = self.lower_expr(&args[1]);
        // Each receiver mention lowers separately so no two tree
        // positions share a HirId.
        let map = self.lower_expr(map_expr);
        let map_again = self.lower_expr(map_expr);
        let key_ty = key.ty;
        let (k_let, v_let) = self.entry_prelude(span, key, default, map, value_ty);
        let insert_call = self.entry_insert_call(span, map_again, key_ty, value_ty);
        let insert_stmt = HirStmt {
            id: self.fresh(),
            span,
            kind: HirStmtKind::Expr {
                expr: insert_call,
                has_semi: true,
            },
        };
        let v_tail = self.entry_path(span, "__entry_v", value_ty);
        Some(HirExpr {
            id: self.fresh(),
            span,
            ty: value_ty,
            kind: HirExprKind::Block(HirBlock {
                id: self.fresh(),
                span,
                stmts: vec![k_let, v_let, insert_stmt],
                tail: Some(Box::new(v_tail)),
                ty: value_ty,
                is_comptime: false,
            }),
        })
    }

    /// Desugars the statement `m.or_insert(k, d).method(args)` on a
    /// HashMap-typed simple-place receiver into an explicit write-back:
    ///
    /// ```text
    /// { let __entry_k = k; let mut __entry_v = get-or-default;
    ///   __entry_v.method(args); m.insert(__entry_k, __entry_v) }
    /// ```
    ///
    /// so the mutation lands in the map's stored value (the map's value
    /// semantics hand `or_insert` callers a copy). The key evaluates
    /// once; the receiver must be a bare path so its re-evaluation is
    /// the same place. `None` leaves the statement to the normal
    /// lowering.
    fn desugar_or_insert_mutation(&mut self, expr: &AstExpr) -> Option<HirExpr> {
        let AstExprKind::MethodCall {
            receiver: outer_recv,
            name: outer_name,
            args: outer_args,
            ..
        } = &expr.kind
        else {
            return None;
        };
        let AstExprKind::MethodCall {
            receiver: map_expr,
            name: inner_name,
            args: inner_args,
            ..
        } = &outer_recv.kind
        else {
            return None;
        };
        if inner_name.name.as_str() != "or_insert" || inner_args.len() != 2 {
            return None;
        }
        if !matches!(map_expr.kind, AstExprKind::Path(_)) {
            return None;
        }
        let map_ty = self.ty_of(map_expr.id);
        let gossamer_types::TyKind::HashMap { value, .. } = self.tcx.kind_of(map_ty) else {
            return None;
        };
        // Struct / tuple values only - the copy-then-lose shape this
        // write-back exists for. Vec values keep the engineered borrow
        // path; scalar values have no mutating methods to chain.
        if !matches!(
            self.tcx.kind_of(*value),
            gossamer_types::TyKind::Adt { .. } | gossamer_types::TyKind::Tuple(_)
        ) {
            return None;
        }
        let span = expr.span;
        let key = self.lower_expr(&inner_args[0]);
        let default = self.lower_expr(&inner_args[1]);
        // Each receiver mention lowers separately so no two tree
        // positions share a HirId.
        let map = self.lower_expr(map_expr);
        let map_again = self.lower_expr(map_expr);
        let key_ty = key.ty;
        let value_ty = self.ty_of(outer_recv.id);
        let outer_ty = self.ty_of(expr.id);
        let unit_ty = self.unit();
        let (k_let, v_let) = self.entry_prelude(span, key, default, map, value_ty);
        let v_for_call = self.entry_path(span, "__entry_v", value_ty);
        let lowered_args: Vec<HirExpr> = outer_args.iter().map(|a| self.lower_expr(a)).collect();
        let mutate_call = HirExpr {
            id: self.fresh(),
            span,
            ty: outer_ty,
            kind: HirExprKind::MethodCall {
                receiver: Box::new(v_for_call),
                name: outer_name.clone(),
                args: lowered_args,
                owner: None,
            },
        };
        let mutate_stmt = HirStmt {
            id: self.fresh(),
            span,
            kind: HirStmtKind::Expr {
                expr: mutate_call,
                has_semi: true,
            },
        };
        let insert_call = self.entry_insert_call(span, map_again, key_ty, value_ty);
        Some(HirExpr {
            id: self.fresh(),
            span,
            ty: unit_ty,
            kind: HirExprKind::Block(HirBlock {
                id: self.fresh(),
                span,
                stmts: vec![k_let, v_let, mutate_stmt],
                tail: Some(Box::new(insert_call)),
                ty: unit_ty,
                is_comptime: false,
            }),
        })
    }

    fn placeholder_expr(&mut self, span: Span) -> HirExpr {
        let ty = self.unit();
        HirExpr {
            id: self.fresh(),
            span,
            ty,
            kind: HirExprKind::Placeholder,
        }
    }

    fn lower_pat(&mut self, pattern: &AstPat) -> HirPat {
        let ty = self.ty_of(pattern.id);
        self.lower_pat_with_ty(pattern, ty)
    }

    fn lower_pat_with_ty(&mut self, pattern: &AstPat, ty: gossamer_types::Ty) -> HirPat {
        if self.recursion_depth >= RECURSION_LIMIT {
            return HirPat {
                id: self.fresh(),
                span: pattern.span,
                ty,
                kind: HirPatKind::Wildcard,
            };
        }
        self.recursion_depth += 1;
        let kind = self.lower_pat_kind(pattern, ty);
        self.recursion_depth = self.recursion_depth.saturating_sub(1);
        HirPat {
            id: self.fresh(),
            span: pattern.span,
            ty,
            kind,
        }
    }

    /// Lowers a tuple-variant / tuple-struct pattern's elements, expanding a
    /// `..` rest into the wildcards it stands for (`E::C(..)` -> two wildcards
    /// for a two-field variant). The matchers compare element-for-element, so
    /// without this a `[Rest]` pattern only matches a single-field variant.
    fn lower_variant_pat_fields(
        &mut self,
        elems: &[AstPat],
        arity: Option<usize>,
        span: Span,
        ty: gossamer_types::Ty,
    ) -> Vec<HirPat> {
        let rest_pos = elems
            .iter()
            .position(|p| matches!(p.kind, AstPatKind::Rest));
        match (rest_pos, arity) {
            (Some(pos), Some(n)) => {
                let explicit = elems.len() - 1;
                let fill = n.saturating_sub(explicit);
                let mut out = Vec::with_capacity(n.max(elems.len()));
                for (i, p) in elems.iter().enumerate() {
                    if i == pos {
                        for _ in 0..fill {
                            out.push(HirPat {
                                id: self.fresh(),
                                span,
                                ty,
                                kind: HirPatKind::Wildcard,
                            });
                        }
                    } else {
                        out.push(self.lower_pat(p));
                    }
                }
                out
            }
            // Unknown arity or no rest: lower element-for-element (a lone
            // `(..)` still matches a single-field variant, as before).
            _ => elems.iter().map(|p| self.lower_pat(p)).collect(),
        }
    }

    fn lower_pat_kind(&mut self, pattern: &AstPat, ty: gossamer_types::Ty) -> HirPatKind {
        match &pattern.kind {
            AstPatKind::Wildcard => HirPatKind::Wildcard,
            AstPatKind::Rest => HirPatKind::Rest,
            AstPatKind::Ident {
                name,
                mutability,
                subpattern,
            } => {
                let mutable = matches!(mutability, Mutability::Mutable);
                if let Some(sub) = subpattern {
                    HirPatKind::At {
                        name: name.clone(),
                        mutable,
                        sub: Box::new(self.lower_pat(sub)),
                    }
                } else {
                    HirPatKind::Binding {
                        name: name.clone(),
                        mutable,
                    }
                }
            }
            AstPatKind::Literal(lit) => HirPatKind::Literal(lower_literal(lit)),
            AstPatKind::Path(path) => self.lower_path_pat(path),
            AstPatKind::TupleStruct { path, elems } => {
                let name = path
                    .segments
                    .last()
                    .map_or_else(|| Ident::new("<error>"), |seg| seg.name.clone());
                let arity = self.ctor_arity.get(name.name.as_str()).copied();
                let fields = self.lower_variant_pat_fields(elems, arity, pattern.span, ty);
                HirPatKind::Variant { name, fields }
            }
            AstPatKind::Struct { path, fields, rest } => {
                self.lower_struct_pat(pattern.id, path, fields, *rest)
            }
            AstPatKind::Tuple(parts) => {
                HirPatKind::Tuple(parts.iter().map(|p| self.lower_pat(p)).collect())
            }
            AstPatKind::Slice {
                prefix,
                rest,
                suffix,
            } => HirPatKind::Slice {
                prefix: prefix.iter().map(|p| self.lower_pat(p)).collect(),
                rest: rest.as_ref().map(|r| Box::new(self.lower_pat(r))),
                suffix: suffix.iter().map(|p| self.lower_pat(p)).collect(),
            },
            AstPatKind::Or(alts) => {
                HirPatKind::Or(alts.iter().map(|p| self.lower_pat(p)).collect())
            }
            AstPatKind::Ref { inner, mutability } => HirPatKind::Ref {
                inner: Box::new(self.lower_pat(inner)),
                mutable: matches!(mutability, Mutability::Mutable),
            },
            AstPatKind::Range { lo, hi, kind } => {
                self.lower_range_pat(lo.as_ref(), hi.as_ref(), *kind, ty)
            }
            AstPatKind::Error => HirPatKind::Wildcard,
        }
        .erase_unused(ty)
    }

    /// Lowers a path pattern, resolving it to a const value, unit struct, or unit variant.
    fn lower_path_pat(&mut self, path: &gossamer_ast::TypePath) -> HirPatKind {
        let name = path
            .segments
            .last()
            .map_or_else(|| Ident::new("<error>"), |seg| seg.name.clone());
        // A `const` named in a pattern stands for its value, the way
        // a literal written there does. A unit variant or unit struct
        // of the same name is the nominal pattern and keeps it.
        if self.unit_structs.contains(name.name.as_str()) {
            // A unit struct has one value, so naming it is the
            // fieldless struct pattern its braced form spells.
            return HirPatKind::Struct {
                name,
                fields: Vec::new(),
                rest: false,
            };
        }
        match self.const_literals.get(name.name.as_str()) {
            Some(lit) if !self.ctor_arity.contains_key(name.name.as_str()) => {
                HirPatKind::Literal(lit.clone())
            }
            _ => HirPatKind::Variant {
                name,
                fields: Vec::new(),
            },
        }
    }

    /// Lowers a braced struct pattern, naming it by its promoted module path when it has one.
    fn lower_struct_pat(
        &mut self,
        id: NodeId,
        path: &gossamer_ast::TypePath,
        fields: &[AstFieldPat],
        rest: bool,
    ) -> HirPatKind {
        let mut name = path
            .segments
            .last()
            .map_or_else(|| Ident::new("<error>"), |seg| seg.name.clone());
        if let Some(Resolution::Def { def, .. }) = self.resolutions.get(id)
            && let Some(promoted) = self.module_fn_paths.get(&def)
            && let Some(promoted_name) = promoted.last()
        {
            name.clone_from(promoted_name);
        }
        HirPatKind::Struct {
            name,
            fields: fields.iter().map(|f| self.lower_field_pat(f)).collect(),
            rest,
        }
    }

    /// Lowers a range pattern, closing an open bound with the scrutinee type's extreme.
    fn lower_range_pat(
        &self,
        lo: Option<&AstLiteral>,
        hi: Option<&AstLiteral>,
        kind: gossamer_ast::RangeKind,
        ty: gossamer_types::Ty,
    ) -> HirPatKind {
        let inclusive = matches!(kind, gossamer_ast::RangeKind::Inclusive);
        // An open bound denotes the scrutinee type's extreme, so
        // synthesise a type-correct min/max literal and lower to a
        // closed `lo..=hi` / `lo..hi` predicate the compiled tiers
        // already handle. An open end always reaches the maximum,
        // hence inclusive of it.
        match (lo, hi) {
            (Some(lo), Some(hi)) => HirPatKind::Range {
                lo: lower_literal(lo),
                hi: lower_literal(hi),
                inclusive,
            },
            (None, Some(hi)) => HirPatKind::Range {
                lo: int_extreme_literal(self.tcx, ty, Extreme::Min),
                hi: lower_literal(hi),
                inclusive,
            },
            (Some(lo), None) => HirPatKind::Range {
                lo: lower_literal(lo),
                hi: int_extreme_literal(self.tcx, ty, Extreme::Max),
                inclusive: true,
            },
            (None, None) => HirPatKind::Wildcard,
        }
    }

    fn lower_field_pat(&mut self, field: &AstFieldPat) -> HirFieldPat {
        HirFieldPat {
            name: field.name.clone(),
            pattern: field.pattern.as_ref().map(|p| self.lower_pat(p)),
        }
    }
}

trait PatKindExt {
    fn erase_unused(self, ty: gossamer_types::Ty) -> Self;
}

impl PatKindExt for HirPatKind {
    fn erase_unused(self, _ty: gossamer_types::Ty) -> Self {
        self
    }
}

/// Which end of an integer type's representable range to synthesise for
/// an open-ended range pattern.
#[derive(Clone, Copy)]
enum Extreme {
    Min,
    Max,
}

/// Builds the min/max integer literal for `ty`, used to close an
/// open-ended range pattern. A non-integer or unresolved type falls back
/// to `i64`'s extreme; unsigned 64-bit maxima saturate at `i64::MAX`
/// (above which `u64` aliases `i64` semantics anyway).
fn int_extreme_literal(tcx: &TyCtxt, ty: gossamer_types::Ty, extreme: Extreme) -> HirLiteral {
    let int_ty = resolve_int_ty(tcx, ty).unwrap_or(gossamer_types::IntTy::I64);
    let value = match extreme {
        Extreme::Min => int_ty_min(int_ty),
        Extreme::Max => int_ty_max(int_ty),
    };
    HirLiteral::Int(value.to_string())
}

/// Peels references and returns the concrete integer type behind `ty`.
fn resolve_int_ty(tcx: &TyCtxt, ty: gossamer_types::Ty) -> Option<gossamer_types::IntTy> {
    use gossamer_types::TyKind;
    match tcx.kind(ty)? {
        TyKind::Int(int_ty) => Some(*int_ty),
        TyKind::Ref { inner, .. } => resolve_int_ty(tcx, *inner),
        _ => None,
    }
}

fn int_ty_min(int_ty: gossamer_types::IntTy) -> i64 {
    use gossamer_types::IntTy;
    match int_ty {
        IntTy::I8 => i64::from(i8::MIN),
        IntTy::I16 => i64::from(i16::MIN),
        IntTy::I32 => i64::from(i32::MIN),
        IntTy::I64 | IntTy::I128 | IntTy::Isize => i64::MIN,
        IntTy::U8 | IntTy::U16 | IntTy::U32 | IntTy::U64 | IntTy::U128 | IntTy::Usize => 0,
    }
}

fn int_ty_max(int_ty: gossamer_types::IntTy) -> i64 {
    use gossamer_types::IntTy;
    match int_ty {
        IntTy::I8 => i64::from(i8::MAX),
        IntTy::I16 => i64::from(i16::MAX),
        IntTy::I32 => i64::from(i32::MAX),
        IntTy::I64 | IntTy::I128 | IntTy::Isize => i64::MAX,
        IntTy::U8 => i64::from(u8::MAX),
        IntTy::U16 => i64::from(u16::MAX),
        IntTy::U32 => i64::from(u32::MAX),
        IntTy::U64 | IntTy::U128 | IntTy::Usize => i64::MAX,
    }
}

fn lower_literal(lit: &AstLiteral) -> HirLiteral {
    match lit {
        AstLiteral::Int(text) => HirLiteral::Int(text.clone()),
        AstLiteral::Float(text) => HirLiteral::Float(text.clone()),
        AstLiteral::String(text) => HirLiteral::String(text.clone()),
        AstLiteral::RawString { value, .. } => HirLiteral::String(value.clone()),
        AstLiteral::Char(c) => HirLiteral::Char(*c),
        AstLiteral::Byte(b) => HirLiteral::Byte(*b),
        AstLiteral::ByteString(bytes) => HirLiteral::ByteString(bytes.clone()),
        AstLiteral::RawByteString { value, .. } => HirLiteral::ByteString(value.clone()),
        AstLiteral::Bool(b) => HirLiteral::Bool(*b),
        AstLiteral::Unit => HirLiteral::Unit,
    }
}

fn lower_unary_op(op: UnaryOp) -> HirUnaryOp {
    match op {
        UnaryOp::Neg => HirUnaryOp::Neg,
        UnaryOp::Not => HirUnaryOp::Not,
        UnaryOp::RefShared => HirUnaryOp::RefShared,
        UnaryOp::RefMut => HirUnaryOp::RefMut,
        UnaryOp::Deref => HirUnaryOp::Deref,
    }
}

/// Maps concrete binary operators to their HIR form. `PipeGt` is
/// lowered separately via [`Lowerer::lower_pipe`] before this helper is
/// called, so the mapping never sees it.
fn lower_binary_op(op: AstBinOp) -> HirBinaryOp {
    match op {
        AstBinOp::Add | AstBinOp::PipeGt => HirBinaryOp::Add,
        AstBinOp::Sub => HirBinaryOp::Sub,
        AstBinOp::Mul => HirBinaryOp::Mul,
        AstBinOp::Div => HirBinaryOp::Div,
        AstBinOp::Rem => HirBinaryOp::Rem,
        AstBinOp::BitAnd => HirBinaryOp::BitAnd,
        AstBinOp::BitOr => HirBinaryOp::BitOr,
        AstBinOp::BitXor => HirBinaryOp::BitXor,
        AstBinOp::Shl => HirBinaryOp::Shl,
        AstBinOp::Shr => HirBinaryOp::Shr,
        AstBinOp::Eq => HirBinaryOp::Eq,
        AstBinOp::Ne => HirBinaryOp::Ne,
        AstBinOp::Lt => HirBinaryOp::Lt,
        AstBinOp::Le => HirBinaryOp::Le,
        AstBinOp::Gt => HirBinaryOp::Gt,
        AstBinOp::Ge => HirBinaryOp::Ge,
        AstBinOp::And => HirBinaryOp::And,
        AstBinOp::Or => HirBinaryOp::Or,
    }
}

fn compound_assign_to_binary(op: AssignOp) -> HirBinaryOp {
    match op {
        AssignOp::Assign | AssignOp::AddAssign => HirBinaryOp::Add,
        AssignOp::SubAssign => HirBinaryOp::Sub,
        AssignOp::MulAssign => HirBinaryOp::Mul,
        AssignOp::DivAssign => HirBinaryOp::Div,
        AssignOp::RemAssign => HirBinaryOp::Rem,
        AssignOp::BitAndAssign => HirBinaryOp::BitAnd,
        AssignOp::BitOrAssign => HirBinaryOp::BitOr,
        AssignOp::BitXorAssign => HirBinaryOp::BitXor,
        AssignOp::ShlAssign => HirBinaryOp::Shl,
        AssignOp::ShrAssign => HirBinaryOp::Shr,
    }
}
