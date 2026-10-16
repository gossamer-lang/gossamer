// Associated constants are ordinary constants reached through a type or a
// trait bound. Each one hoists to a `const` with a mangled name beside the
// `impl` or `trait` that declares it, in the same module, and every
// `Type::NAME` / `Self::NAME` / `T::NAME` read is rewritten to name that
// constant. The result is plain Gossamer source, so it compiles
// through every tier with no dedicated runtime or lowering support.

use gossamer_ast::assoc::type_head_name as assoc_type_head_name;
use gossamer_ast::{
    AssocIndex, AssocResolution, ConstDecl, Expr, ExprKind, FnDecl, GenericParam, Generics, Ident,
    ImplItem, PathExpr, PathSegment, TraitItem, Visibility, WhereClause,
};

/// Name prefix of the top-level constant an associated constant hoists to.
/// The `__gos_` prefix is reserved for compiler-generated items, so the
/// name can never shadow one the program declares.
const ASSOC_CONST_PREFIX: &str = "__gos_assoc_const_";

/// Name of the top-level constant holding `owner`'s associated constant
/// `name`, where `owner` is the impl's self type or the declaring trait.
fn hoisted_name(owner: &str, name: &str) -> String {
    format!("{ASSOC_CONST_PREFIX}{owner}_{name}")
}

/// Rewrites every associated-constant read to the hoisted constant it
/// names, then hoists each associated constant's value into that constant.
pub fn hoist_associated_consts(sf: &mut SourceFile) {
    let index = AssocIndex::build(sf);
    if index.is_assoc_const_free() {
        return;
    }
    let mut homes = ConstHomes::new();
    collect_homes(&sf.items, &[], &mut homes);
    let consts = HoistedConsts {
        index: &index,
        homes: &homes,
        imports: module_imports(&sf.uses),
    };
    let empty = HashMap::new();
    rewrite_items(&mut sf.items, &consts, &[], None, None, &empty);
    let mut ids = AssocIds {
        next: sf.next_node_id,
    };
    hoist_items(&mut sf.items, &mut ids);
    sf.next_node_id = ids.next;
}

/// The inline-module paths of the items declaring each associated
/// constant, by owner (the impl's self type or the trait) and constant
/// name. Two modules may each declare an owner of the same name.
type ConstHomes = HashMap<(String, String), Vec<Vec<String>>>;

/// The names each module's `use` declarations bind, by module path, with
/// the path each one imports as written.
type ModuleImports = HashMap<Vec<String>, HashMap<String, Vec<String>>>;

/// What a read is rewritten against: which owner a `Base::NAME` reaches,
/// and the modules declaring that owner's constant.
struct HoistedConsts<'a> {
    index: &'a AssocIndex,
    homes: &'a ConstHomes,
    imports: ModuleImports,
}

impl HoistedConsts<'_> {
    /// The path naming `owner`'s hoisted constant `name` from any module, for
    /// a read in `module` whose base was qualified by `qualifier`: a
    /// `crate::` path when the constant sits in a module, else its bare name.
    /// `None` when several modules declare it and the read does not say
    /// which, so the read stays as written and is reported where it is.
    fn path_to(
        &self,
        owner: &str,
        name: &str,
        module: &[String],
        base: &str,
        qualifier: &[String],
    ) -> Option<PathExpr> {
        let homes = self.homes.get(&(owner.to_string(), name.to_string()))?;
        let home = match homes.as_slice() {
            [only] => only,
            _ => self.home_of(homes, module, base, qualifier)?,
        };
        let hoisted = hoisted_name(owner, name);
        if home.is_empty() {
            return Some(PathExpr::single(hoisted));
        }
        Some(PathExpr {
            segments: std::iter::once("crate")
                .chain(home.iter().map(String::as_str))
                .chain(std::iter::once(hoisted.as_str()))
                .map(PathSegment::new)
                .collect(),
        })
    }

    /// Which of `homes` the read means: the module its qualifier names, else
    /// the reading module's own declaration, else the module a `use` of the
    /// base name in the reading module imports it from.
    fn home_of<'h>(
        &self,
        homes: &'h [Vec<String>],
        module: &[String],
        base: &str,
        qualifier: &[String],
    ) -> Option<&'h Vec<String>> {
        let find = |candidates: Vec<Vec<String>>| {
            candidates
                .into_iter()
                .find_map(|candidate| homes.iter().find(|home| **home == candidate))
        };
        if !qualifier.is_empty() {
            return find(anchored(module, qualifier));
        }
        if let Some(own) = homes.iter().find(|home| home.as_slice() == module) {
            return Some(own);
        }
        let imported = self.imports.get(module)?.get(base)?;
        let (_, from) = imported.split_last()?;
        find(anchored(module, from))
    }
}

/// The absolute module paths `written`, written in `module`, can mean: a
/// `crate::`, `self::`, or `super::` path means one; any other means a child
/// of `module` or a module at the root.
fn anchored(module: &[String], written: &[String]) -> Vec<Vec<String>> {
    match written.first().map(String::as_str) {
        Some("crate" | "root") => vec![written[1..].to_vec()],
        Some("self") => vec![[module, &written[1..]].concat()],
        Some("super") => {
            let ups = written.iter().take_while(|s| *s == "super").count();
            let Some(kept) = module.len().checked_sub(ups) else {
                return Vec::new();
            };
            vec![[&module[..kept], &written[ups..]].concat()]
        }
        _ => vec![[module, written].concat(), written.to_vec()],
    }
}

/// The names every module's `use` declarations bind, each with the path it
/// imports.
fn module_imports(uses: &[gossamer_ast::UseDecl]) -> ModuleImports {
    let mut out = ModuleImports::new();
    for decl in uses {
        let gossamer_ast::UseTarget::Module(target) = &decl.target else {
            continue;
        };
        let target: Vec<String> = target.segments.iter().map(|s| s.name.clone()).collect();
        let bound = out.entry(decl.module.clone()).or_default();
        match &decl.list {
            None => {
                let name = decl
                    .alias
                    .as_ref()
                    .map(|a| a.name.clone())
                    .or_else(|| target.last().cloned());
                if let Some(name) = name {
                    bound.insert(name, target.clone());
                }
            }
            Some(entries) => {
                for entry in entries {
                    let mut path = target.clone();
                    path.extend(entry.prefix.iter().map(|s| s.name.clone()));
                    path.push(entry.name.name.clone());
                    let name = entry.alias.as_ref().unwrap_or(&entry.name).name.clone();
                    bound.insert(name, path);
                }
            }
        }
    }
    out
}

/// Records `module` as declaring `owner`'s constant `name`, once: `#[cfg]`
/// alternatives of one constant share a module.
fn add_home(homes: &mut ConstHomes, owner: &str, name: &str, module: &[String]) {
    let modules = homes
        .entry((owner.to_string(), name.to_string()))
        .or_default();
    if !modules.iter().any(|known| known.as_slice() == module) {
        modules.push(module.to_vec());
    }
}

fn collect_homes(items: &[Item], module: &[String], homes: &mut ConstHomes) {
    for item in items {
        match &item.kind {
            ItemKind::Impl(decl) => {
                let Some(owner) = assoc_type_head_name(&decl.self_ty) else {
                    continue;
                };
                for impl_item in &decl.items {
                    if let ImplItem::Const { name, .. } = impl_item {
                        add_home(homes, owner, &name.name, module);
                    }
                }
            }
            ItemKind::Trait(decl) => {
                for trait_item in &decl.items {
                    if let TraitItem::Const {
                        name,
                        default: Some(_),
                        ..
                    } = trait_item
                    {
                        add_home(homes, &decl.name.name, &name.name, module);
                    }
                }
            }
            ItemKind::Mod(decl) => {
                if let ModBody::Inline(inner) = &decl.body {
                    let mut path = module.to_vec();
                    path.push(decl.name.name.clone());
                    collect_homes(inner, &path, homes);
                }
            }
            _ => {}
        }
    }
}

/// Node-id allocator for the constants and paths this pass synthesizes.
struct AssocIds {
    next: u32,
}

impl AssocIds {
    fn id(&mut self) -> NodeId {
        let id = NodeId::from_raw(self.next);
        self.next += 1;
        id
    }
}

/// Renumbers a cloned type's node ids so the copy never shares a table
/// entry with the item it was cloned from.
struct TypeRenum<'a> {
    ids: &'a mut AssocIds,
}

impl gossamer_ast::VisitorMut for TypeRenum<'_> {
    fn visit_type(&mut self, t: &mut gossamer_ast::Type) {
        t.id = self.ids.id();
        gossamer_ast::visitor::walk_type_mut(self, t);
    }
    fn visit_expr(&mut self, e: &mut Expr) {
        e.id = self.ids.id();
        gossamer_ast::visitor::walk_expr_mut(self, e);
    }
}

/// Trait names bounding each type parameter of a declaration, from the
/// angle brackets and the `where` clause alike.
fn param_bounds(generics: &Generics, where_clause: &WhereClause) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for param in &generics.params {
        let GenericParam::Type { name, bounds, .. } = param else {
            continue;
        };
        let entry = out.entry(name.name.clone()).or_default();
        entry.extend(
            bounds
                .iter()
                .filter_map(|b| b.trait_name().map(str::to_string)),
        );
    }
    for predicate in &where_clause.predicates {
        let Some(name) = assoc_type_head_name(&predicate.bounded) else {
            continue;
        };
        let entry = out.entry(name.to_string()).or_default();
        entry.extend(
            predicate
                .bounds
                .iter()
                .filter_map(|b| b.trait_name().map(str::to_string)),
        );
    }
    out
}

fn merged_bounds(
    outer: &HashMap<String, Vec<String>>,
    generics: &Generics,
    where_clause: &WhereClause,
) -> HashMap<String, Vec<String>> {
    let mut merged = outer.clone();
    for (name, bounds) in param_bounds(generics, where_clause) {
        merged.entry(name).or_default().extend(bounds);
    }
    merged
}

fn rewrite_items(
    items: &mut [Item],
    consts: &HoistedConsts<'_>,
    module: &[String],
    self_ty: Option<&str>,
    self_trait: Option<&str>,
    params: &HashMap<String, Vec<String>>,
) {
    for item in items {
        match &mut item.kind {
            ItemKind::Fn(decl) => rewrite_fn(decl, consts, module, self_ty, self_trait, params),
            ItemKind::Impl(decl) => {
                let impl_self = assoc_type_head_name(&decl.self_ty).map(ToString::to_string);
                let impl_trait = decl.trait_ref.as_ref().and_then(|b| b.trait_name());
                let impl_trait = impl_trait.map(ToString::to_string);
                let impl_params = merged_bounds(params, &decl.generics, &decl.where_clause);
                for impl_item in &mut decl.items {
                    match impl_item {
                        ImplItem::Fn(fn_decl) => rewrite_fn(
                            fn_decl,
                            consts,
                            module,
                            impl_self.as_deref(),
                            impl_trait.as_deref(),
                            &impl_params,
                        ),
                        ImplItem::Const { value, .. } => rewrite_expr(
                            value,
                            consts,
                            module,
                            impl_self.as_deref(),
                            impl_trait.as_deref(),
                            &impl_params,
                        ),
                        ImplItem::Type { .. } => {}
                    }
                }
            }
            ItemKind::Trait(decl) => {
                let trait_name = decl.name.name.clone();
                let trait_params = merged_bounds(params, &decl.generics, &decl.where_clause);
                for trait_item in &mut decl.items {
                    match trait_item {
                        TraitItem::Fn(fn_decl) => {
                            rewrite_fn(
                                fn_decl,
                                consts,
                                module,
                                None,
                                Some(&trait_name),
                                &trait_params,
                            );
                        }
                        TraitItem::Const {
                            default: Some(value),
                            ..
                        } => rewrite_expr(
                            value,
                            consts,
                            module,
                            None,
                            Some(&trait_name),
                            &trait_params,
                        ),
                        TraitItem::Const { .. } | TraitItem::Type { .. } => {}
                    }
                }
            }
            ItemKind::Const(decl) => {
                rewrite_expr(&mut decl.value, consts, module, None, None, params);
            }
            ItemKind::Static(decl) => {
                rewrite_expr(&mut decl.value, consts, module, None, None, params);
            }
            ItemKind::Mod(decl) => {
                if let ModBody::Inline(inner) = &mut decl.body {
                    let mut path = module.to_vec();
                    path.push(decl.name.name.clone());
                    rewrite_items(inner, consts, &path, self_ty, self_trait, params);
                }
            }
            _ => {}
        }
    }
}

fn rewrite_fn(
    decl: &mut FnDecl,
    consts: &HoistedConsts<'_>,
    module: &[String],
    self_ty: Option<&str>,
    self_trait: Option<&str>,
    params: &HashMap<String, Vec<String>>,
) {
    let fn_params = merged_bounds(params, &decl.generics, &decl.where_clause);
    for param in &mut decl.params {
        if let gossamer_ast::FnParam::Typed {
            default: Some(default),
            ..
        } = param
        {
            rewrite_expr(default, consts, module, self_ty, self_trait, &fn_params);
        }
    }
    if let Some(body) = &mut decl.body {
        rewrite_expr(body, consts, module, self_ty, self_trait, &fn_params);
    }
}

fn rewrite_expr(
    expr: &mut Expr,
    consts: &HoistedConsts<'_>,
    module: &[String],
    self_ty: Option<&str>,
    self_trait: Option<&str>,
    params: &HashMap<String, Vec<String>>,
) {
    let mut rewriter = ConstReadRewriter {
        consts,
        module,
        self_ty,
        self_trait,
        params,
    };
    gossamer_ast::VisitorMut::visit_expr(&mut rewriter, expr);
}

struct ConstReadRewriter<'a> {
    consts: &'a HoistedConsts<'a>,
    /// The inline module the read is written in.
    module: &'a [String],
    self_ty: Option<&'a str>,
    self_trait: Option<&'a str>,
    params: &'a HashMap<String, Vec<String>>,
}

impl ConstReadRewriter<'_> {
    /// Top-level constant a `Base::NAME` read resolves to, or `None` when
    /// the path names no associated constant reachable from here.
    fn owner_of(&self, base: &str, name: &str) -> Option<String> {
        if base == "Self" {
            if let Some(self_ty) = self.self_ty
                && let Some(owner) = self.consts.index.assoc_const_owner_for_self(self_ty, name)
            {
                return Some(owner);
            }
            let traits = self.consts.index.with_supertraits(
                self.self_trait
                    .into_iter()
                    .map(ToString::to_string)
                    .collect(),
            );
            return self.owner_through(&traits, name);
        }
        if let Some(bounds) = self.params.get(base) {
            let traits = self.consts.index.with_supertraits(bounds.clone());
            return self.owner_through(&traits, name);
        }
        self.consts.index.assoc_const_owner_for_self(base, name)
    }

    /// First of `traits` that resolves `name` to a single owner.
    fn owner_through(&self, traits: &[String], name: &str) -> Option<String> {
        traits.iter().find_map(
            |t| match self.consts.index.assoc_const_owner_for_trait(t, name) {
                AssocResolution::Found(owner) => Some(owner),
                AssocResolution::Ambiguous | AssocResolution::Unknown => None,
            },
        )
    }
}

impl gossamer_ast::VisitorMut for ConstReadRewriter<'_> {
    fn visit_expr(&mut self, e: &mut Expr) {
        gossamer_ast::visitor::walk_expr_mut(self, e);
        let ExprKind::Path(path) = &mut e.kind else {
            return;
        };
        // The last two segments carry the read, and any before them say
        // which module's type it is: `Gauge::MAX`, `inner::Gauge::MAX`.
        let Some((base, name)) = assoc_read_segments(path) else {
            return;
        };
        let Some(owner) = self.owner_of(&base, &name) else {
            return;
        };
        let qualifier: Vec<String> = path.segments[..path.segments.len() - 2]
            .iter()
            .map(|s| s.name.name.clone())
            .collect();
        if let Some(read) = self
            .consts
            .path_to(&owner, &name, self.module, &base, &qualifier)
        {
            *path = read;
        }
    }
}

/// Base type or trait name and item name of an associated-item read. The
/// last two segments carry it, so a module-qualified path resolves the
/// same way as a bare one.
fn assoc_read_segments(path: &PathExpr) -> Option<(String, String)> {
    let count = path.segments.len();
    if count < 2 {
        return None;
    }
    Some((
        path.segments[count - 2].name.name.clone(),
        path.segments[count - 1].name.name.clone(),
    ))
}

/// Moves each associated constant's value into a top-level constant that
/// sits immediately before the item it came from, so a later constant can
/// read it exactly as it could read any earlier constant.
fn hoist_items(items: &mut Vec<Item>, ids: &mut AssocIds) {
    let mut out: Vec<Item> = Vec::with_capacity(items.len());
    for mut item in std::mem::take(items) {
        collect_hoists(&mut item, &mut out, ids);
        out.push(item);
    }
    *items = out;
}

fn collect_hoists(item: &mut Item, out: &mut Vec<Item>, ids: &mut AssocIds) {
    let span = item.span;
    let owner_attrs = &item.attrs;
    match &mut item.kind {
        ItemKind::Impl(decl) => {
            let Some(owner) = assoc_type_head_name(&decl.self_ty).map(ToString::to_string) else {
                return;
            };
            for impl_item in &mut decl.items {
                if let ImplItem::Const {
                    attrs,
                    name,
                    ty,
                    value,
                } = impl_item
                {
                    let cfg = cfg_attrs(owner_attrs, attrs);
                    out.push(lift_const(&owner, name, ty, value, cfg, span, ids));
                }
            }
        }
        ItemKind::Trait(decl) => {
            let owner = decl.name.name.clone();
            for trait_item in &mut decl.items {
                if let TraitItem::Const {
                    attrs,
                    name,
                    ty,
                    default: Some(value),
                } = trait_item
                {
                    let cfg = cfg_attrs(owner_attrs, attrs);
                    out.push(lift_const(&owner, name, ty, value, cfg, span, ids));
                }
            }
        }
        // A module's constants hoist into the module itself, where the names
        // their types and values use are in scope.
        ItemKind::Mod(decl) => {
            if let ModBody::Inline(inner) = &mut decl.body {
                hoist_items(inner, ids);
            }
        }
        _ => {}
    }
}

/// The `#[cfg]` attributes of an associated constant and of the `impl` or
/// `trait` holding it, which together decide whether its hoisted constant
/// exists: a `#[cfg]` choosing between two declarations of one name keeps
/// exactly one of them.
fn cfg_attrs(owner: &gossamer_ast::Attrs, member: &gossamer_ast::Attrs) -> gossamer_ast::Attrs {
    let is_cfg = |attr: &&gossamer_ast::Attribute| {
        attr.path
            .segments
            .last()
            .is_some_and(|segment| segment.name.name == "cfg")
    };
    gossamer_ast::Attrs {
        outer: owner
            .outer
            .iter()
            .chain(&member.outer)
            .filter(is_cfg)
            .cloned()
            .collect(),
        inner: Vec::new(),
    }
}

fn lift_const(
    owner: &str,
    name: &Ident,
    ty: &gossamer_ast::Type,
    value: &mut Expr,
    attrs: gossamer_ast::Attrs,
    span: gossamer_lex::Span,
    ids: &mut AssocIds,
) -> Item {
    let hoisted = hoisted_name(owner, &name.name);
    let read = Expr::new(
        ids.id(),
        value.span,
        ExprKind::Path(PathExpr::single(hoisted.clone())),
    );
    let init = std::mem::replace(value, read);
    let mut const_ty = ty.clone();
    gossamer_ast::VisitorMut::visit_type(&mut TypeRenum { ids }, &mut const_ty);
    Item::new(
        ids.id(),
        span,
        attrs,
        Visibility::Public,
        ItemKind::Const(ConstDecl {
            name: Ident::new(hoisted),
            ty: const_ty,
            value: init,
        }),
    )
}
