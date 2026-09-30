//! AST → HIR lowering.

#![forbid(unsafe_code)]

use gossamer_ast::{
    AssignOp, BINARY_SEARCH_PREFIX, BinaryOp as AstBinOp, Block as AstBlock,
    ClosureParam as AstClosureParam, Expr as AstExpr, ExprKind as AstExprKind, Ident,
    Item as AstItem, ItemKind as AstItemKind, Literal as AstLiteral, MatchArm, NodeId,
    PARTITION_POINT_PREFIX, STRUCTURAL_COMPARATOR_PREFIX, SourceFile, Stmt as AstStmt,
    StmtKind as AstStmtKind, Type as AstType, USER_COMPARATOR_PREFIX, UnaryOp,
};
use gossamer_lex::Span;
use gossamer_resolve::{Resolution, Resolutions};
use gossamer_types::{Ty, TyCtxt, TypeTable};

use crate::ids::{HirId, HirIdGenerator};
use crate::tree::{
    HirBinaryOp, HirBlock, HirExpr, HirExprKind, HirItem, HirItemKind, HirLiteral, HirMatchArm,
    HirParam, HirPat, HirPatKind, HirProgram, HirStmt, HirStmtKind, HirUnaryOp,
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
        current_generic_names: Vec::new(),
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
    // Parallel adapters lower to plain index loops first, so their leaves
    // meet the fuser like any hand-written loop would.
    crate::par::desugar_parallel_adapters(&mut program, &mut *lowerer.tcx, &mut lowerer.ids);
    crate::fuse::fuse_iter_pipelines(&mut program, &mut *lowerer.tcx, &mut lowerer.ids);
    // After the desugars above, so the arithmetic they generate rounds too.
    crate::f32_round::round_f32_values(&mut program, lowerer.tcx, &mut lowerer.ids);
    crate::place_refs::inline_place_references(&mut program);
    program
}

/// The comparator-taking spelling of an ordering call written bare, or
/// `None` for a call that names no order.
fn comparator_ordering_form(method: &str) -> Option<&'static str> {
    match method {
        "sort" => Some("sort_by"),
        "min" => Some("min_by"),
        "max" => Some("max_by"),
        "par_min" => Some("par_min_by"),
        "par_max" => Some("par_max_by"),
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
            // A module's `static` is reached by the same module-relative path
            // a function is, and the cell it names is registered under that
            // spelling. Two modules may each declare one of the same name, so
            // a bare reference has to carry the module that declared it.
            AstItemKind::Static(decl) if !module_path.is_empty() => {
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

mod for_loop;
mod items;
mod literals;
mod map_index;
mod patterns;
mod simd;
mod try_op;

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
    /// The generic parameter names of the function being lowered, at the
    /// positions the checker numbers them (an impl's first, a lifetime taking a
    /// position with an empty name), so a `ParamIdx` names its parameter.
    current_generic_names: Vec<String>,
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

    /// The method a type-qualified call on a runtime handle names, when its
    /// first argument is that type's handle: `sync::AtomicI64::load(a)` is
    /// `a.load()`, so every tier reaches it through the one method lowering.
    fn sync_qualified_method(&self, callee: &HirExpr, args: &[HirExpr]) -> Option<Ident> {
        use gossamer_types::TyKind;
        let HirExprKind::Path { segments, .. } = &callee.kind else {
            return None;
        };
        let [.., owner, method] = segments.as_slice() else {
            return None;
        };
        // A constructor may take a handle of its own type (`with_cancel(parent)`)
        // and still answer a new one, so it is never the receiver's method.
        if matches!(
            method.name.as_str(),
            "new" | "background" | "with_cancel" | "with_timeout"
        ) {
            return None;
        }
        let TyKind::Adt { def, .. } = self.tcx.kind_of(args.first()?.ty) else {
            return None;
        };
        if def.local < u32::MAX - 64 {
            return None;
        }
        let name = self.tcx.def_name(*def)?;
        let (module, tail) = name.rsplit_once("::")?;
        let typed_family = matches!(
            module,
            "sync" | "metrics" | "trace" | "rand" | "bufio" | "context"
        );
        (typed_family && tail == owner.name).then(|| method.clone())
    }

    /// Ends an inline `flat_map` callback whose body yields an `Iterator` in
    /// `.collect()`. The concatenation reads each callback result as a
    /// sequence, and lazy iterator state is not one until it is drained.
    fn drain_iterator_callback(&mut self, callback: &mut HirExpr) {
        use gossamer_types::{FnSig, TyKind};
        let HirExprKind::Closure { ret, body, .. } = &mut callback.kind else {
            return;
        };
        let TyKind::Iterator(elem) = self.tcx.kind_of(body.ty).clone() else {
            return;
        };
        let vec_ty = self.tcx.intern(TyKind::Vec(elem));
        let span = body.span;
        let inner = std::mem::replace(
            body.as_mut(),
            HirExpr {
                id: self.ids.next(),
                span,
                ty: vec_ty,
                kind: HirExprKind::Placeholder,
            },
        );
        body.kind = HirExprKind::MethodCall {
            receiver: Box::new(inner),
            name: Ident::new("collect"),
            args: Vec::new(),
            owner: None,
        };
        if ret.is_some() {
            *ret = Some(vec_ty);
        }
        let sig = match self.tcx.kind_of(callback.ty) {
            TyKind::FnTrait(sig) | TyKind::FnPtr(sig) => Some(sig.clone()),
            _ => None,
        };
        if let Some(sig) = sig {
            let rewritten = FnSig {
                inputs: sig.inputs,
                output: vec_ty,
            };
            callback.ty = match self.tcx.kind_of(callback.ty) {
                TyKind::FnPtr(_) => self.tcx.intern(TyKind::FnPtr(rewritten)),
                _ => self.tcx.intern(TyKind::FnTrait(rewritten)),
            };
        }
    }

    /// Appends the values a call hands its callee's const generic parameters,
    /// which the callee receives as trailing parameters.
    fn append_const_generic_args(&mut self, callee: NodeId, args: &mut Vec<HirExpr>, span: Span) {
        let Some(consts) = self.table.const_generic_args(callee) else {
            return;
        };
        let lowered: Vec<(HirExprKind, gossamer_types::Ty)> = consts
            .iter()
            .map(|arg| match arg {
                gossamer_types::ConstGenericArg::Value { value, ty } => (
                    HirExprKind::Literal(HirLiteral::Int(value.to_string())),
                    *ty,
                ),
                gossamer_types::ConstGenericArg::Param { name, ty } => (
                    HirExprKind::Path {
                        segments: vec![Ident::new(name)],
                        def: None,
                    },
                    *ty,
                ),
            })
            .collect();
        for (kind, ty) in lowered {
            args.push(HirExpr {
                id: self.fresh(),
                span,
                ty,
                kind,
            });
        }
    }

    /// The `impl` block a method call resolves to, as the checker recorded it.
    fn method_owner_of(&self, node: NodeId) -> Option<Ident> {
        self.table.method_owner(node).map(Ident::new)
    }

    /// The annotation's type when it resolved to a concrete one, else the
    /// initializer's.
    fn declared_or_init_ty(&mut self, annotation: NodeId, init: NodeId) -> gossamer_types::Ty {
        let declared = self.ty_of(annotation);
        if !ty_has_unresolved_var(self.tcx, declared) {
            return declared;
        }
        self.ty_of(init)
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

    /// `{:?}` of a `String` or `char` renders it in the spelling that builds
    /// it, so each such argument of the `__debug` channel is quoted first. A
    /// `DynValue` argument renders through the channel's own `DynValue`
    /// renderer on either channel.
    fn quote_debug_strings(&mut self, callee: &HirExpr, args: &mut [HirExpr]) {
        let HirExprKind::Path {
            segments,
            def: None,
        } = &callee.kind
        else {
            return;
        };
        let debug = match segments.as_slice() {
            [only] if only.name == "__debug" => true,
            [only]
                if matches!(
                    only.name.as_str(),
                    "__concat" | "println" | "print" | "eprintln" | "eprint" | "format" | "panic"
                ) =>
            {
                false
            }
            _ => return,
        };
        for arg in args.iter_mut() {
            // A `DynValue` holds a value whose text depends on the channel:
            // its string reads as itself under `{}` and quoted under `{:?}`.
            let renderer = match self.tcx.kind_of(arg.ty) {
                gossamer_types::TyKind::DynValue if debug => "__gos_dyn_debug",
                gossamer_types::TyKind::DynValue => "__gos_dyn_display",
                gossamer_types::TyKind::String | gossamer_types::TyKind::Char if debug => {
                    "__gos_debug_quote"
                }
                _ => continue,
            };
            let span = arg.span;
            let ty = self.tcx.string_ty();
            let quote = self.free_path(&[renderer], span);
            let inner = std::mem::replace(
                arg,
                HirExpr {
                    id: self.fresh(),
                    span,
                    ty,
                    kind: HirExprKind::Literal(HirLiteral::Unit),
                },
            );
            arg.kind = HirExprKind::Call {
                callee: Box::new(quote),
                args: vec![inner],
            };
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
                if let Some(lowered) = self.lower_simd_call(expr, callee, args) {
                    lowered
                } else if let Some(lowered) =
                    self.lower_sequence_order_call(callee, args, expr.span)
                {
                    lowered
                } else if let Some(lowered) = self.lower_reverse_call(callee, args, expr.span) {
                    lowered
                } else if let Some(lowered) = self.lower_tuple_struct_call(callee, args, expr.span)
                {
                    lowered
                } else {
                    let callee_node = callee.id;
                    let callee = Box::new(self.lower_expr(callee));
                    let mut args: Vec<HirExpr> = args.iter().map(|a| self.lower_expr(a)).collect();
                    if let Some(method) = self.sync_qualified_method(&callee, &args) {
                        let receiver = args.remove(0);
                        return HirExprKind::MethodCall {
                            receiver: Box::new(receiver),
                            name: method,
                            args,
                            owner: None,
                        };
                    }
                    // `{:+}` signs a number and leaves any other value as it
                    // rendered, which is decided by the value's type here.
                    if let HirExprKind::Path { segments, .. } = &callee.kind
                        && segments
                            .last()
                            .is_some_and(|segment| segment.name.as_str() == "__gos_fmt_sign")
                        && args.len() == 1
                        && !self.pad_value_is_numeric(&args[0])
                    {
                        return args.remove(0).kind;
                    }
                    self.resolve_format_pad_request(&callee, &mut args);
                    self.narrow_radix_operand(&callee, &mut args);
                    self.quote_debug_strings(&callee, &mut args);
                    if let HirExprKind::Path { segments, .. } = &callee.kind
                        && segments.len() == 2
                        && segments[0].name == "iter"
                        && segments[1].name == "flat_map"
                    {
                        for arg in &mut args {
                            self.drain_iterator_callback(arg);
                        }
                    }
                    self.append_const_generic_args(callee_node, &mut args, expr.span);
                    HirExprKind::Call { callee, args }
                }
            }
            AstExprKind::MethodCall {
                receiver,
                name,
                args,
                ..
            } => {
                if let Some(shape) = self.simd_shape(receiver.id)
                    && let Some(kind) = self.lower_simd_method(
                        receiver,
                        name.name.as_str(),
                        args,
                        shape,
                        expr.id,
                        expr.span,
                    )
                {
                    return kind;
                }
                if let Some(desugared) = self.desugar_or_insert_value(expr) {
                    return desugared.kind;
                }
                if name.name == "expect"
                    && args.len() == 1
                    && let Some(kind) = self.desugar_expect(receiver, Some(&args[0]), expr)
                {
                    return kind;
                }
                // `Result::unwrap` names the error it found, which only the
                // error's own type can render.
                if name.name == "unwrap"
                    && args.is_empty()
                    && self.is_result_expr(receiver)
                    && let Some(kind) = self.desugar_expect(receiver, None, expr)
                {
                    return kind;
                }
                if let Some(kind) =
                    self.desugar_btree_map_method(expr, receiver, name.name.as_str(), args)
                {
                    return kind;
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
                // A narrow signed integer's magnitude is taken as a word and
                // narrowed back, which wraps `i8::MIN` to itself as unary `-`
                // does, so the value stays in its type's range on every tier.
                if name.name == "abs"
                    && args.is_empty()
                    && let Some(narrow) = self.narrow_signed_int_of(receiver.id)
                {
                    let span = expr.span;
                    let i64_ty = self.tcx.int_ty(gossamer_types::IntTy::I64);
                    let widened = HirExpr {
                        id: self.fresh(),
                        span,
                        ty: i64_ty,
                        kind: HirExprKind::Cast {
                            value: Box::new(self.lower_expr(receiver)),
                            ty: i64_ty,
                        },
                    };
                    let magnitude = HirExpr {
                        id: self.fresh(),
                        span,
                        ty: i64_ty,
                        kind: HirExprKind::MethodCall {
                            receiver: Box::new(widened),
                            name: name.clone(),
                            args: Vec::new(),
                            owner: None,
                        },
                    };
                    return HirExprKind::Cast {
                        value: Box::new(magnitude),
                        ty: narrow,
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
                let mut args: Vec<HirExpr> = args.iter().map(|a| self.lower_expr(a)).collect();
                if name.name == "flat_map"
                    && let [callback] = args.as_mut_slice()
                {
                    self.drain_iterator_callback(callback);
                }
                self.append_const_generic_args(expr.id, &mut args, expr.span);
                HirExprKind::MethodCall {
                    receiver: Box::new(self.lower_expr(receiver)),
                    name: name.clone(),
                    args,
                    owner,
                }
            }
            AstExprKind::FieldAccess { receiver, field } => self.lower_field(receiver, field),
            AstExprKind::Index { base, index }
                if {
                    let base_ty = self.ty_of(base.id);
                    self.is_map_ty(base_ty)
                } =>
            {
                let map = self.lower_expr(base);
                let key = self.lower_expr(index);
                let value_ty = self.ty_of(expr.id);
                self.map_index_read(map, key, value_ty, expr.span).kind
            }
            AstExprKind::Index { base, index } => HirExprKind::Index {
                base: Box::new(self.lower_expr(base)),
                index: Box::new(self.lower_expr(index)),
            },
            AstExprKind::Unary {
                op: UnaryOp::Neg,
                operand,
            } if self.simd_shape(operand.id).is_some() => {
                let shape = self.simd_shape(operand.id);
                match shape {
                    Some(shape) => self.lower_simd_neg(operand, shape, expr.span),
                    None => HirExprKind::Placeholder,
                }
            }
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
        if let Some(shape) = self.simd_shape(lhs.id).or_else(|| self.simd_shape(rhs.id))
            && let Some(kind) = self.lower_simd_binary(op, lhs, rhs, shape, lhs.span)
        {
            return kind;
        }
        if let Some(method) = wrapping_binary_method(op) {
            return wrapping_call(method, self.lower_expr(lhs), self.lower_expr(rhs));
        }
        // An operator implemented for this right-hand type calls the method
        // the checker chose for it.
        if let Some(method) = self.table.operator_method(lhs.id).map(str::to_string) {
            return wrapping_call(&method, self.lower_expr(lhs), self.lower_expr(rhs));
        }
        let lhs = self.lower_expr(lhs);
        let rhs = self.lower_expr(rhs);
        HirExprKind::Binary {
            op: lower_binary_op(op),
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        }
    }

    /// Reads a lowered integer literal's value.
    fn literal_int_of(expr: &HirExpr) -> Option<i64> {
        match &expr.kind {
            HirExprKind::Literal(HirLiteral::Int(text)) => text.parse().ok(),
            _ => None,
        }
    }

    /// Renders a narrow signed integer in `{:x}` / `{:b}` / `{:o}` as the bits
    /// of its own width, as Rust does: `-1i8` is `ff`, not sixteen `f`s. The
    /// operand is read as the unsigned type of the same width.
    fn narrow_radix_operand(&mut self, callee: &HirExpr, args: &mut [HirExpr]) {
        use gossamer_types::{IntTy, TyKind};
        let HirExprKind::Path { segments, .. } = &callee.kind else {
            return;
        };
        if segments
            .last()
            .is_none_or(|segment| segment.name.as_str() != "__fmt_radix")
        {
            return;
        }
        let Some(value) = args.first_mut() else {
            return;
        };
        let unsigned = match self.tcx.kind_of(value.ty) {
            TyKind::Int(IntTy::I8) => IntTy::U8,
            TyKind::Int(IntTy::I16) => IntTy::U16,
            TyKind::Int(IntTy::I32) => IntTy::U32,
            _ => return,
        };
        let target = self.tcx.int_ty(unsigned);
        let span = value.span;
        let operand = std::mem::replace(
            value,
            HirExpr {
                id: self.fresh(),
                span,
                ty: target,
                kind: HirExprKind::Tuple(Vec::new()),
            },
        );
        value.kind = HirExprKind::Cast {
            value: Box::new(operand),
            ty: target,
        };
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
            // `{:+}` and `{:e}` wrap the number's own rendering.
            Some("__gos_fmt_sign" | "__gos_fmt_exp") => args
                .first()
                .is_some_and(|inner| self.pad_value_is_numeric(inner)),
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
                self.append_const_generic_args(callee.id, &mut new_args, rhs.span);
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
                self.append_const_generic_args(rhs.id, &mut new_args, rhs.span);
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
            AstExprKind::Path(_) | AstExprKind::Closure { .. } => {
                let mut args = vec![piped];
                self.append_const_generic_args(rhs.id, &mut args, rhs.span);
                HirExprKind::Call {
                    callee: Box::new(self.lower_expr(rhs)),
                    args,
                }
            }
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
                let chosen = self.table.operator_method(target.id).map(str::to_string);
                let kind = match chosen.as_deref().or_else(|| wrapping_assign_method(op)) {
                    Some(method) => wrapping_call(method, lowered_target.clone(), source),
                    None => HirExprKind::Binary {
                        op: compound_assign_to_binary(op),
                        lhs: Box::new(lowered_target.clone()),
                        rhs: Box::new(source),
                    },
                };
                HirExpr {
                    id: self.fresh(),
                    span: target.span,
                    ty,
                    kind,
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
        if let Some(kind) = self.lower_map_index_assign(op, place, value, outer.span) {
            return kind;
        }
        let chosen = self.table.operator_method(place.id).map(str::to_string);
        let lowered_place = self.lower_expr(place);
        let lowered_value = self.lower_expr(value);
        if let Some(method) = chosen
            && !matches!(op, AssignOp::Assign)
        {
            // A compound operator implemented for this right-hand type writes
            // back what the chosen method answers.
            let ty = lowered_place.ty;
            let combined = HirExpr {
                id: self.fresh(),
                span: outer.span,
                ty,
                kind: wrapping_call(&method, lowered_place.clone(), lowered_value),
            };
            return HirExprKind::Assign {
                place: Box::new(lowered_place),
                value: Box::new(combined),
            };
        }
        self.assign_kind(op, lowered_place, lowered_value, outer.span)
    }

    /// `place op value` over already-lowered operands; a compound operator
    /// reads the place, combines, and writes it back.
    fn assign_kind(
        &mut self,
        op: AssignOp,
        lowered_place: HirExpr,
        lowered_value: HirExpr,
        span: Span,
    ) -> HirExprKind {
        if matches!(op, AssignOp::Assign) {
            return HirExprKind::Assign {
                place: Box::new(lowered_place),
                value: Box::new(lowered_value),
            };
        }
        let bin_op = compound_assign_to_binary(op);
        let place_ty = lowered_place.ty;
        let value_ty = lowered_value.ty;
        let combined = match wrapping_assign_method(op) {
            Some(method) => wrapping_call(method, lowered_place.clone(), lowered_value),
            None => HirExprKind::Binary {
                op: bin_op,
                lhs: Box::new(lowered_place.clone()),
                rhs: Box::new(HirExpr {
                    ty: value_ty,
                    ..lowered_value
                }),
            },
        };
        let bin_expr = HirExpr {
            id: self.fresh(),
            span,
            ty: place_ty,
            kind: combined,
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
        // A numeric limit constant folds to its literal, so every tier
        // compiles the same value.
        if let [ty_name, name] = path.segments.as_slice()
            && matches!(self.resolutions.get(node), Some(Resolution::Primitive(_)))
            && let Some(constant) =
                gossamer_resolve::limit_constant(&ty_name.name.name, &name.name.name)
        {
            return HirExprKind::Literal(match constant.literal {
                gossamer_resolve::LimitLiteral::Int(text) => HirLiteral::Int(text),
                gossamer_resolve::LimitLiteral::Float(text) => HirLiteral::Float(text),
            });
        }
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
                .or_else(|| {
                    self.resolutions
                        .module_alias_in(&self.current_module, &segments[0].name)
                })
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
            // spelling the rewrite below needs. That definition is the
            // head's: a longer path names something inside it.
            Some(Resolution::Import { .. }) if segments.len() == 1 => {
                self.resolutions.import_def(node)
            }
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
                    .or_else(|| self.desugar_map_index_mutation(tail))
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
                match init {
                    Some(init) if pattern_holds_slice(&pattern) => {
                        self.let_through_match(pattern, init, stmt.span)
                    }
                    init => HirStmtKind::Let {
                        pattern,
                        ty: pattern_ty,
                        init,
                    },
                }
            }
            AstStmtKind::Expr { expr, has_semi } => {
                let expr = self
                    .desugar_or_insert_mutation(expr)
                    .or_else(|| self.desugar_map_index_mutation(expr))
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

    /// A `let` whose pattern takes a fixed array apart, written as the match
    /// `let ... else` desugars to: the pattern is an arm, and the arm answers
    /// its bindings for a plain `let` to receive. The checker has proved the
    /// pattern matches every value, so the one arm is the whole match.
    fn let_through_match(&mut self, pattern: HirPat, init: HirExpr, span: Span) -> HirStmtKind {
        let mut binds = Vec::new();
        collect_hir_bindings(&pattern, &mut binds);
        let unit = self.tcx.unit();
        let (outer, ty, body) = match binds.as_slice() {
            [] => (
                HirPatKind::Wildcard,
                unit,
                HirExprKind::Literal(HirLiteral::Unit),
            ),
            [(name, mutable, ty)] => (
                HirPatKind::Binding {
                    name: name.clone(),
                    mutable: *mutable,
                },
                *ty,
                HirExprKind::Path {
                    segments: vec![name.clone()],
                    def: None,
                },
            ),
            _ => {
                let tys: Vec<_> = binds.iter().map(|(_, _, ty)| *ty).collect();
                let pats = binds
                    .iter()
                    .map(|(name, mutable, ty)| HirPat {
                        id: self.fresh(),
                        span,
                        ty: *ty,
                        kind: HirPatKind::Binding {
                            name: name.clone(),
                            mutable: *mutable,
                        },
                    })
                    .collect();
                let reads = binds
                    .iter()
                    .map(|(name, _, ty)| HirExpr {
                        id: self.fresh(),
                        span,
                        ty: *ty,
                        kind: HirExprKind::Path {
                            segments: vec![name.clone()],
                            def: None,
                        },
                    })
                    .collect();
                (
                    HirPatKind::Tuple(pats),
                    self.tcx.intern(gossamer_types::TyKind::Tuple(tys)),
                    HirExprKind::Tuple(reads),
                )
            }
        };
        let body = HirExpr {
            id: self.fresh(),
            span,
            ty,
            kind: body,
        };
        let matched = HirExpr {
            id: self.fresh(),
            span,
            ty,
            kind: HirExprKind::Match {
                scrutinee: Box::new(init),
                arms: vec![HirMatchArm {
                    pattern,
                    guard: None,
                    body,
                }],
            },
        };
        HirStmtKind::Let {
            pattern: HirPat {
                id: self.fresh(),
                span,
                ty,
                kind: outer,
            },
            ty,
            init: Some(matched),
        }
    }

    /// One `let` of the entry desugars (`let [mut] name: ty = init`).
    /// Whether `ty` is a `Map` or `BTreeMap` once references are peeled.
    fn is_map_ty(&self, ty: gossamer_types::Ty) -> bool {
        let mut ty = ty;
        while let gossamer_types::TyKind::Ref { inner, .. } = self.tcx.kind_of(ty) {
            ty = *inner;
        }
        matches!(self.tcx.kind_of(ty), gossamer_types::TyKind::HashMap { .. })
    }

    /// `name(args)` for a prelude builtin, typed `ty`, with its arguments
    /// rendered as the written call's would be (`{:?}` quotes a string).
    fn builtin_call(&mut self, name: &str, mut args: Vec<HirExpr>, ty: Ty, span: Span) -> HirExpr {
        let callee = HirExpr {
            id: self.fresh(),
            span,
            ty,
            kind: HirExprKind::Path {
                segments: vec![Ident::new(name)],
                def: None,
            },
        };
        self.quote_debug_strings(&callee, &mut args);
        HirExpr {
            id: self.fresh(),
            span,
            ty,
            kind: HirExprKind::Call {
                callee: Box::new(callee),
                args,
            },
        }
    }

    /// `opt.expect(msg)` / `res.expect(msg)` (and `res.unwrap()` with no
    /// message of its own): a `match` answering the payload, or a panic with
    /// the message; a `Result` adds `": "` and the error's `{:?}`.
    fn desugar_expect(
        &mut self,
        receiver: &AstExpr,
        message: Option<&AstExpr>,
        expr: &AstExpr,
    ) -> Option<HirExprKind> {
        let mut carrier_ty = self.ty_of(receiver.id);
        while let gossamer_types::TyKind::Ref { inner, .. } = self.tcx.kind_of(carrier_ty) {
            carrier_ty = *inner;
        }
        let gossamer_types::TyKind::Adt { def, substs } = self.tcx.kind_of(carrier_ty).clone()
        else {
            return None;
        };
        let is_option = def.local == u32::MAX - 1;
        if !is_option && def.local != u32::MAX {
            return None;
        }
        let span = expr.span;
        let payload_ty = self.ty_of(expr.id);
        let string_ty = self.tcx.string_ty();
        let never = self.tcx.never();
        let scrutinee = self.lower_expr(receiver);
        let text = match message {
            Some(message) => self.lower_expr(message),
            None => HirExpr {
                id: self.fresh(),
                span,
                ty: string_ty,
                kind: HirExprKind::Literal(HirLiteral::String(
                    "called `Result::unwrap()` on an `Err` value".to_string(),
                )),
            },
        };
        let binding = |this: &mut Self, name: &str, ty: Ty| HirPat {
            id: this.fresh(),
            span,
            ty,
            kind: HirPatKind::Binding {
                name: Ident::new(name),
                mutable: false,
            },
        };
        let value_binding = binding(self, "__gos_expect_value", payload_ty);
        let found_pat = HirPat {
            id: self.fresh(),
            span,
            ty: carrier_ty,
            kind: HirPatKind::Variant {
                name: Ident::new(if is_option { "Some" } else { "Ok" }),
                fields: vec![value_binding],
            },
        };
        let found_body = self.entry_path(span, "__gos_expect_value", payload_ty);
        let (missing_pat, message) = if is_option {
            let pat = HirPat {
                id: self.fresh(),
                span,
                ty: carrier_ty,
                kind: HirPatKind::Variant {
                    name: Ident::new("None"),
                    fields: Vec::new(),
                },
            };
            (pat, text)
        } else {
            let err_ty = substs.types().get(1).copied()?;
            let err_binding = binding(self, "__gos_expect_err", err_ty);
            let pat = HirPat {
                id: self.fresh(),
                span,
                ty: carrier_ty,
                kind: HirPatKind::Variant {
                    name: Ident::new("Err"),
                    fields: vec![err_binding],
                },
            };
            let err = self.entry_path(span, "__gos_expect_err", err_ty);
            let rendered = self.builtin_call("__debug", vec![err], string_ty, span);
            let separator = HirExpr {
                id: self.fresh(),
                span,
                ty: string_ty,
                kind: HirExprKind::Literal(HirLiteral::String(": ".to_string())),
            };
            let message =
                self.builtin_call("__concat", vec![text, separator, rendered], string_ty, span);
            (pat, message)
        };
        let missing_body = self.builtin_call("panic", vec![message], never, span);
        Some(HirExprKind::Match {
            scrutinee: Box::new(scrutinee),
            arms: vec![
                HirMatchArm {
                    pattern: found_pat,
                    guard: None,
                    body: found_body,
                },
                HirMatchArm {
                    pattern: missing_pat,
                    guard: None,
                    body: missing_body,
                },
            ],
        })
    }

    /// Whether `expr` is a `Result` (through any references).
    fn is_result_expr(&mut self, expr: &AstExpr) -> bool {
        let mut ty = self.ty_of(expr.id);
        while let gossamer_types::TyKind::Ref { inner, .. } = self.tcx.kind_of(ty) {
            ty = *inner;
        }
        matches!(
            self.tcx.kind_of(ty),
            gossamer_types::TyKind::Adt { def, .. } if def.local == u32::MAX
        )
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
        AstBinOp::Add | AstBinOp::PipeGt | AstBinOp::WrappingAdd => HirBinaryOp::Add,
        AstBinOp::Sub | AstBinOp::WrappingSub => HirBinaryOp::Sub,
        AstBinOp::Mul | AstBinOp::WrappingMul => HirBinaryOp::Mul,
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

/// The integer method a wrapping arithmetic operator spells: `a +% b` is
/// `a.wrapping_add(b)` on every tier, so the operator reaches the one lowering
/// that already wraps at the operands' declared width. `lower_binary_op` never
/// sees these operators.
fn wrapping_binary_method(op: AstBinOp) -> Option<&'static str> {
    match op {
        AstBinOp::WrappingAdd => Some("__gos_wrapping_add"),
        AstBinOp::WrappingSub => Some("__gos_wrapping_sub"),
        AstBinOp::WrappingMul => Some("__gos_wrapping_mul"),
        _ => None,
    }
}

/// [`wrapping_binary_method`] for the compound forms `+%=`, `-%=`, `*%=`.
/// `compound_assign_to_binary` never sees these operators.
fn wrapping_assign_method(op: AssignOp) -> Option<&'static str> {
    match op {
        AssignOp::WrappingAddAssign => Some("__gos_wrapping_add"),
        AssignOp::WrappingSubAssign => Some("__gos_wrapping_sub"),
        AssignOp::WrappingMulAssign => Some("__gos_wrapping_mul"),
        _ => None,
    }
}

/// The `receiver.method(arg)` call a wrapping arithmetic operator lowers to.
fn wrapping_call(method: &str, receiver: HirExpr, arg: HirExpr) -> HirExprKind {
    HirExprKind::MethodCall {
        receiver: Box::new(receiver),
        name: Ident {
            name: method.to_string(),
        },
        args: vec![arg],
        owner: None,
    }
}

fn compound_assign_to_binary(op: AssignOp) -> HirBinaryOp {
    match op {
        AssignOp::Assign | AssignOp::AddAssign | AssignOp::WrappingAddAssign => HirBinaryOp::Add,
        AssignOp::SubAssign | AssignOp::WrappingSubAssign => HirBinaryOp::Sub,
        AssignOp::MulAssign | AssignOp::WrappingMulAssign => HirBinaryOp::Mul,
        AssignOp::DivAssign => HirBinaryOp::Div,
        AssignOp::RemAssign => HirBinaryOp::Rem,
        AssignOp::BitAndAssign => HirBinaryOp::BitAnd,
        AssignOp::BitOrAssign => HirBinaryOp::BitOr,
        AssignOp::BitXorAssign => HirBinaryOp::BitXor,
        AssignOp::ShlAssign => HirBinaryOp::Shl,
        AssignOp::ShrAssign => HirBinaryOp::Shr,
    }
}

/// Whether `ty` still names an inference variable, at its own head or inside
/// one of its arguments.
fn ty_has_unresolved_var(tcx: &gossamer_types::TyCtxt, ty: gossamer_types::Ty) -> bool {
    use gossamer_types::TyKind;
    match tcx.kind_of(ty) {
        TyKind::Var(_) | TyKind::Error => true,
        TyKind::Vec(inner)
        | TyKind::Slice(inner)
        | TyKind::Array { elem: inner, .. }
        | TyKind::Ref { inner, .. }
        | TyKind::Iterator(inner)
        | TyKind::Sender(inner)
        | TyKind::Receiver(inner)
        | TyKind::JoinHandle(inner) => ty_has_unresolved_var(tcx, *inner),
        TyKind::HashMap { key, value, .. } => {
            ty_has_unresolved_var(tcx, *key) || ty_has_unresolved_var(tcx, *value)
        }
        TyKind::Tuple(elems) => elems.iter().any(|e| ty_has_unresolved_var(tcx, *e)),
        TyKind::Adt { substs, .. } => substs
            .types()
            .into_iter()
            .any(|a| ty_has_unresolved_var(tcx, a)),
        _ => false,
    }
}

/// Whether a pattern takes a sequence apart anywhere inside it, with every
/// name it binds written as a pattern of its own. A struct field's shorthand
/// binding carries no type here, so a pattern holding one keeps the plain
/// `let` lowering.
fn pattern_holds_slice(pattern: &HirPat) -> bool {
    !holds_field_shorthand(pattern) && takes_sequence_apart(pattern)
}

fn holds_field_shorthand(pattern: &HirPat) -> bool {
    match &pattern.kind {
        HirPatKind::Struct { fields, .. } => fields
            .iter()
            .any(|field| field.pattern.as_ref().is_none_or(holds_field_shorthand)),
        HirPatKind::Tuple(parts) | HirPatKind::Or(parts) => parts.iter().any(holds_field_shorthand),
        HirPatKind::Variant { fields, .. } => fields.iter().any(holds_field_shorthand),
        HirPatKind::Slice {
            prefix,
            rest,
            suffix,
        } => prefix
            .iter()
            .chain(rest.as_deref())
            .chain(suffix)
            .any(holds_field_shorthand),
        HirPatKind::Ref { inner, .. } | HirPatKind::At { sub: inner, .. } => {
            holds_field_shorthand(inner)
        }
        HirPatKind::Binding { .. }
        | HirPatKind::Wildcard
        | HirPatKind::Literal(_)
        | HirPatKind::Rest
        | HirPatKind::Range { .. } => false,
    }
}

fn takes_sequence_apart(pattern: &HirPat) -> bool {
    match &pattern.kind {
        HirPatKind::Slice { .. } => true,
        HirPatKind::Tuple(parts) | HirPatKind::Or(parts) => parts.iter().any(takes_sequence_apart),
        HirPatKind::Variant { fields, .. } => fields.iter().any(takes_sequence_apart),
        HirPatKind::Struct { fields, .. } => fields
            .iter()
            .filter_map(|field| field.pattern.as_ref())
            .any(takes_sequence_apart),
        HirPatKind::Ref { inner, .. } | HirPatKind::At { sub: inner, .. } => {
            takes_sequence_apart(inner)
        }
        HirPatKind::Binding { .. }
        | HirPatKind::Wildcard
        | HirPatKind::Literal(_)
        | HirPatKind::Rest
        | HirPatKind::Range { .. } => false,
    }
}

/// The names a pattern binds, with their mutability and type, in source
/// order. An or-pattern binds the same names in each alternative.
fn collect_hir_bindings(pattern: &HirPat, out: &mut Vec<(Ident, bool, gossamer_types::Ty)>) {
    match &pattern.kind {
        HirPatKind::Binding { name, mutable } => out.push((name.clone(), *mutable, pattern.ty)),
        HirPatKind::Tuple(parts) | HirPatKind::Variant { fields: parts, .. } => {
            for part in parts {
                collect_hir_bindings(part, out);
            }
        }
        HirPatKind::Or(parts) => {
            if let Some(first) = parts.first() {
                collect_hir_bindings(first, out);
            }
        }
        HirPatKind::Slice {
            prefix,
            rest,
            suffix,
        } => {
            for part in prefix.iter().chain(rest.as_deref()).chain(suffix) {
                collect_hir_bindings(part, out);
            }
        }
        HirPatKind::Struct { fields, .. } => {
            for field in fields {
                if let Some(part) = &field.pattern {
                    collect_hir_bindings(part, out);
                }
            }
        }
        HirPatKind::At { name, mutable, sub } => {
            out.push((name.clone(), *mutable, pattern.ty));
            collect_hir_bindings(sub, out);
        }
        HirPatKind::Ref { inner, .. } => collect_hir_bindings(inner, out),
        HirPatKind::Wildcard
        | HirPatKind::Literal(_)
        | HirPatKind::Rest
        | HirPatKind::Range { .. } => {}
    }
}
