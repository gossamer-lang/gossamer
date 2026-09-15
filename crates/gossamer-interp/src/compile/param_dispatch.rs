use std::collections::{HashMap, HashSet};

use gossamer_hir::{
    HirExpr, HirExprKind, HirFn, HirId, HirItemKind, HirProgram, for_each_child_expr,
    for_each_child_expr_in_block,
};
use gossamer_resolve::DefId;
use gossamer_types::{GenericArg, Ty, TyCtxt, TyKind};

/// Per-instantiation dispatch for generic functions that reach a trait
/// function through a type parameter (`T::zero()`).
///
/// The bytecode VM runs one chunk per function and a value carries no type
/// arguments, so nothing at run time says which impl `T::zero()` names. Each
/// such function is compiled once per instantiation the program reaches, and
/// every call that reaches one is pointed at that chunk. The answers are worked
/// out while the interner can still grow, so compiling a chunk only reads them.
#[derive(Debug, Default)]
pub(crate) struct ParamDispatch {
    /// Names of the free functions compiled per instantiation.
    pub(crate) dependent_names: HashSet<String>,
    /// Every instantiation the program reaches.
    pub(crate) instances: Vec<Instance>,
    /// The global an expression names in the chunk keyed by the first field:
    /// the empty key for a chunk compiled once, an instance's global name for
    /// an instance chunk.
    targets: HashMap<(String, HirId), String>,
    /// The instantiated type of every expression whose checked type names a
    /// type parameter, in the instance chunk keyed by the first field.
    instance_tys: HashMap<(String, HirId), Ty>,
}

/// A function compiled per instantiation: a free function by its definition,
/// or an impl method by the `Type::method` name its impl registers.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Callable {
    Free(DefId),
    Method(String),
}

/// One instantiation of a function compiled per instantiation.
#[derive(Debug)]
pub(crate) struct Instance {
    /// Global name the chunk registers under.
    pub(crate) global: String,
    /// The function the chunk instantiates.
    pub(crate) callable: Callable,
    /// Name the function's own call sites spell, for its parameter types.
    pub(crate) base_name: String,
}

/// How a call site states the instantiation it reaches.
enum SiteTypes {
    /// A free function call: the type arguments its callee carries.
    Free(Vec<Ty>),
    /// A method call: the receiver, argument, and result types, which are
    /// matched against the method's own signature to read its parameters.
    Method {
        receiver: Ty,
        args: Vec<Ty>,
        result: Ty,
    },
}

/// A call into something that may be compiled per instantiation.
struct Site {
    /// The expression the dispatch target is keyed on.
    id: HirId,
    callable: Callable,
    types: SiteTypes,
}

impl SiteTypes {
    fn mentions_param(&self, tcx: &TyCtxt) -> bool {
        match self {
            Self::Free(args) => args.iter().any(|t| mentions_param(tcx, *t)),
            Self::Method {
                receiver,
                args,
                result,
            } => {
                mentions_param(tcx, *receiver)
                    || mentions_param(tcx, *result)
                    || args.iter().any(|t| mentions_param(tcx, *t))
            }
        }
    }

    /// The type arguments this site instantiates `template` with, after
    /// substituting the enclosing instantiation `outer`. `None` when a
    /// position stays unknown or names no concrete type.
    fn resolve(self, template: &HirFn, outer: &[Option<Ty>], tcx: &mut TyCtxt) -> Option<Vec<Ty>> {
        let subst = |tcx: &mut TyCtxt, ty: Ty| gossamer_mir::subst_param_ty(tcx, ty, outer);
        let resolved: Vec<Option<Ty>> = match self {
            Self::Free(args) => args.into_iter().map(|t| Some(subst(tcx, t))).collect(),
            Self::Method {
                receiver,
                args,
                result,
            } => {
                let mut resolved = Vec::new();
                let mut pairs: Vec<(Ty, Ty)> = Vec::new();
                let unit = tcx.unit();
                if let Some(first) = template.params.first() {
                    pairs.push((first.ty, subst(tcx, receiver)));
                }
                for (param, arg) in template.params.iter().skip(1).zip(args) {
                    pairs.push((param.ty, subst(tcx, arg)));
                }
                pairs.push((template.ret.unwrap_or(unit), subst(tcx, result)));
                for (param, actual) in pairs {
                    gossamer_mir::bind_template_params(tcx, param, actual, &mut resolved);
                }
                let needed = param_positions(template, tcx);
                if resolved.len() < needed {
                    resolved.resize(needed, None);
                }
                resolved
            }
        };
        let types: Vec<Ty> = resolved.into_iter().collect::<Option<_>>()?;
        types.iter().all(|t| is_concrete(tcx, *t)).then_some(types)
    }
}

impl ParamDispatch {
    /// The global `id` names inside the chunk keyed `key`, when dispatch
    /// resolved one.
    pub(crate) fn target(&self, key: &str, id: HirId) -> Option<&str> {
        self.targets.get(&(key.to_string(), id)).map(String::as_str)
    }

    /// The type `id` has in the instance chunk keyed `key`, where its checked
    /// type names a type parameter the instantiation fixes.
    pub(crate) fn instance_ty(&self, key: &str, id: HirId) -> Option<Ty> {
        if key.is_empty() {
            return None;
        }
        self.instance_tys.get(&(key.to_string(), id)).copied()
    }

    /// Resolves every instantiation `program` reaches.
    pub(crate) fn build(program: &HirProgram, tcx: &mut TyCtxt) -> Self {
        let mut fns: HashMap<Callable, (String, &HirFn)> = HashMap::new();
        for item in &program.items {
            match &item.kind {
                HirItemKind::Fn(decl) => {
                    if let Some(def) = item.def {
                        let name = if item.module_path.is_empty() {
                            decl.name.name.clone()
                        } else {
                            format!("{}::{}", item.module_path.join("::"), decl.name.name)
                        };
                        fns.insert(Callable::Free(def), (name, decl));
                    }
                }
                HirItemKind::Impl(decl) => {
                    if let Some(self_name) = &decl.self_name {
                        for method in &decl.methods {
                            let name = format!("{}::{}", self_name.name, method.name.name);
                            fns.insert(Callable::Method(name.clone()), (name, method));
                        }
                    }
                }
                HirItemKind::Trait(_)
                | HirItemKind::Const(_)
                | HirItemKind::Static(_)
                | HirItemKind::Adt(_) => {}
            }
        }
        let mut dependent: HashSet<Callable> = fns
            .iter()
            .filter(|(_, (_, decl))| {
                !param_function_paths(decl, tcx).is_empty() || renders_param_value(decl, tcx)
            })
            .map(|(callable, _)| callable.clone())
            .collect();
        if dependent.is_empty() {
            return Self::default();
        }
        loop {
            let reached: Vec<Callable> = fns
                .iter()
                .filter(|(callable, (_, decl))| {
                    !dependent.contains(*callable)
                        && sites(decl, tcx).iter().any(|site| {
                            dependent.contains(&site.callable) && site.types.mentions_param(tcx)
                        })
                })
                .map(|(callable, _)| callable.clone())
                .collect();
            if reached.is_empty() {
                break;
            }
            dependent.extend(reached);
        }
        let mut dispatch = Self {
            dependent_names: dependent
                .iter()
                .filter(|callable| matches!(callable, Callable::Free(_)))
                .filter_map(|callable| fns.get(callable).map(|(name, _)| name.clone()))
                .collect(),
            ..Self::default()
        };
        let mut queued: HashSet<String> = HashSet::new();
        let mut work: Vec<(String, Callable, Vec<Ty>)> = Vec::new();
        let mut roots: Vec<Site> = Vec::new();
        for item in &program.items {
            match &item.kind {
                HirItemKind::Fn(decl) => {
                    if item
                        .def
                        .is_none_or(|def| !dependent.contains(&Callable::Free(def)))
                    {
                        roots.extend(sites(decl, tcx));
                    }
                }
                HirItemKind::Impl(decl) => {
                    for method in &decl.methods {
                        let key = decl.self_name.as_ref().map(|self_name| {
                            Callable::Method(format!("{}::{}", self_name.name, method.name.name))
                        });
                        if key.is_none_or(|key| !dependent.contains(&key)) {
                            roots.extend(sites(method, tcx));
                        }
                    }
                }
                HirItemKind::Trait(decl) => {
                    roots.extend(decl.methods.iter().flat_map(|method| sites(method, tcx)));
                }
                HirItemKind::Const(_) | HirItemKind::Static(_) | HirItemKind::Adt(_) => {}
            }
        }
        for site in roots {
            dispatch.route(&fns, &dependent, site, "", &[], tcx, &mut queued, &mut work);
        }
        while let Some((global, callable, types)) = work.pop() {
            let Some((base_name, decl)) = fns.get(&callable) else {
                continue;
            };
            let base_name = base_name.clone();
            let outer: Vec<Option<Ty>> = types.iter().copied().map(Some).collect();
            let mut generic_exprs: Vec<(HirId, Ty)> = Vec::new();
            visit_body(decl, &mut |expr| {
                if mentions_param(tcx, expr.ty) {
                    generic_exprs.push((expr.id, expr.ty));
                }
            });
            for (id, ty) in generic_exprs {
                let instantiated = gossamer_mir::subst_param_ty(tcx, ty, &outer);
                if is_concrete(tcx, instantiated) {
                    dispatch
                        .instance_tys
                        .insert((global.clone(), id), instantiated);
                }
            }
            for (id, param, function) in param_function_paths(decl, tcx) {
                let TyKind::Param { idx, .. } = tcx.kind_of(param) else {
                    continue;
                };
                let Some(concrete) = types.get(idx.0 as usize).copied() else {
                    continue;
                };
                if let Some(owner) = gossamer_types::printer::impl_owner_name(tcx, concrete) {
                    dispatch
                        .targets
                        .insert((global.clone(), id), format!("{owner}::{function}"));
                }
            }
            for site in sites(decl, tcx) {
                dispatch.route(
                    &fns,
                    &dependent,
                    site,
                    &global,
                    &outer,
                    tcx,
                    &mut queued,
                    &mut work,
                );
            }
            dispatch.instances.push(Instance {
                global,
                callable,
                base_name,
            });
        }
        dispatch
    }

    /// Points `site`, found in the chunk keyed `key` whose instantiation is
    /// `outer`, at the instance it reaches, queueing that instance the first
    /// time it is reached.
    #[allow(clippy::too_many_arguments)]
    fn route(
        &mut self,
        fns: &HashMap<Callable, (String, &HirFn)>,
        dependent: &HashSet<Callable>,
        site: Site,
        key: &str,
        outer: &[Option<Ty>],
        tcx: &mut TyCtxt,
        queued: &mut HashSet<String>,
        work: &mut Vec<(String, Callable, Vec<Ty>)>,
    ) {
        if !dependent.contains(&site.callable) {
            return;
        }
        let Some((base, template)) = fns.get(&site.callable) else {
            return;
        };
        let Some(types) = site.types.resolve(template, outer, tcx) else {
            return;
        };
        let ids: Vec<String> = types.iter().map(|t| format!("t{}", t.as_u32())).collect();
        let global = format!("{base}$inst${}", ids.join("_"));
        if queued.insert(global.clone()) {
            work.push((global.clone(), site.callable, types));
        }
        self.targets.insert((key.to_string(), site.id), global);
    }
}

/// Visits `expr` and every expression nested inside it.
fn visit<'a>(expr: &'a HirExpr, f: &mut impl FnMut(&'a HirExpr)) {
    f(expr);
    for_each_child_expr(expr, &mut |child| visit(child, f));
}

/// Visits every expression in `decl`'s body.
fn visit_body<'a>(decl: &'a HirFn, f: &mut impl FnMut(&'a HirExpr)) {
    if let Some(body) = &decl.body {
        for_each_child_expr_in_block(&body.block, &mut |child| visit(child, f));
    }
}

/// Every call in `decl` that may reach a function compiled per
/// instantiation: a call to a function item, keyed on its callee, and a method
/// call the checker resolved to an impl, keyed on the call. A call whose
/// instantiation carries a const argument names no dispatch target: its type
/// arguments would not line up with the callee's parameter positions.
fn sites(decl: &HirFn, tcx: &TyCtxt) -> Vec<Site> {
    let mut out = Vec::new();
    visit_body(decl, &mut |expr| match &expr.kind {
        HirExprKind::Call { callee, .. } => {
            if let HirExprKind::Path { def: Some(def), .. } = &callee.kind
                && let Some(args) = fn_def_type_args(tcx, callee.ty)
            {
                out.push(Site {
                    id: callee.id,
                    callable: Callable::Free(*def),
                    types: SiteTypes::Free(args),
                });
            }
        }
        HirExprKind::MethodCall {
            receiver,
            name,
            args,
            owner,
        } => {
            // The impl the checker resolved the call to, or else the one the
            // receiver's own type names, which is the impl a method call on
            // that value is dispatched to.
            let owner_name = owner.as_ref().map(|owner| owner.name.clone()).or_else(|| {
                let mut receiver_ty = receiver.ty;
                while let TyKind::Ref { inner, .. } = tcx.kind_of(receiver_ty) {
                    receiver_ty = *inner;
                }
                match tcx.kind_of(receiver_ty) {
                    TyKind::Adt { .. } => {
                        gossamer_types::printer::impl_owner_name(tcx, receiver_ty)
                    }
                    _ => None,
                }
            });
            if let Some(owner_name) = owner_name {
                out.push(Site {
                    id: expr.id,
                    callable: Callable::Method(format!("{owner_name}::{}", name.name)),
                    types: SiteTypes::Method {
                        receiver: receiver.ty,
                        args: args.iter().map(|arg| arg.ty).collect(),
                        result: expr.ty,
                    },
                });
            }
        }
        _ => {}
    });
    out
}

/// One past the highest type-parameter position `template`'s signature or
/// its `T::name` paths name.
fn param_positions(template: &HirFn, tcx: &TyCtxt) -> usize {
    fn visit_ty(tcx: &TyCtxt, ty: Ty, highest: &mut usize) {
        match tcx.kind_of(ty) {
            TyKind::Param { idx, .. } => *highest = (*highest).max(idx.0 as usize + 1),
            TyKind::Ref { inner, .. }
            | TyKind::Vec(inner)
            | TyKind::Slice(inner)
            | TyKind::Iterator(inner)
            | TyKind::Sender(inner)
            | TyKind::Receiver(inner)
            | TyKind::JoinHandle(inner)
            | TyKind::Array { elem: inner, .. } => visit_ty(tcx, *inner, highest),
            TyKind::Tuple(elems) => elems.iter().for_each(|t| visit_ty(tcx, *t, highest)),
            TyKind::HashMap { key, value, .. } => {
                visit_ty(tcx, *key, highest);
                visit_ty(tcx, *value, highest);
            }
            TyKind::Adt { substs, .. } | TyKind::Alias { substs, .. } => {
                substs
                    .types()
                    .iter()
                    .for_each(|t| visit_ty(tcx, *t, highest));
            }
            TyKind::FnPtr(sig) | TyKind::FnTrait(sig) => {
                sig.inputs.iter().for_each(|t| visit_ty(tcx, *t, highest));
                visit_ty(tcx, sig.output, highest);
            }
            _ => {}
        }
    }
    let mut highest = 0;
    for param in &template.params {
        visit_ty(tcx, param.ty, &mut highest);
    }
    if let Some(ret) = template.ret {
        visit_ty(tcx, ret, &mut highest);
    }
    for (_, param, _) in param_function_paths(template, tcx) {
        visit_ty(tcx, param, &mut highest);
    }
    highest
}

/// Every `T::name` path in `decl` whose head the checker recorded as a type
/// parameter, with that parameter and the function name.
fn param_function_paths(decl: &HirFn, tcx: &TyCtxt) -> Vec<(HirId, Ty, String)> {
    let mut out = Vec::new();
    visit_body(decl, &mut |expr| {
        if let HirExprKind::Path {
            segments,
            def: Some(def),
        } = &expr.kind
            && let [_, function] = segments.as_slice()
            && let Some(param) = tcx.type_param_of_def(*def)
        {
            out.push((expr.id, param, function.name.clone()));
        }
    });
    out
}

/// Whether `decl` renders a value whose type names a type parameter. A `Vec`
/// and a fixed array share one runtime representation, and a `u64` shares a
/// slot with an `i64`, so the spelling such a value renders in follows the
/// instantiation - which is what compiling the function per instantiation
/// gives each of its format sites.
fn renders_param_value(decl: &HirFn, tcx: &TyCtxt) -> bool {
    let mut renders = false;
    visit_body(decl, &mut |expr| match &expr.kind {
        HirExprKind::Call { callee, args } => {
            if let HirExprKind::Path { segments, .. } = &callee.kind
                && segments.last().is_some_and(|segment| {
                    matches!(
                        segment.name.as_str(),
                        "__concat" | "__debug" | "println" | "print" | "eprintln" | "format"
                    )
                })
                && args.iter().any(|arg| mentions_param(tcx, arg.ty))
            {
                renders = true;
            }
        }
        HirExprKind::MethodCall { receiver, name, .. }
            if matches!(name.name.as_str(), "to_string" | "fmt" | "join")
                && mentions_param(tcx, receiver.ty) =>
        {
            renders = true;
        }
        _ => {}
    });
    renders
}

/// Whether `ty` names a type parameter anywhere in its structure.
pub(crate) fn mentions_param(tcx: &TyCtxt, ty: Ty) -> bool {
    match tcx.kind_of(ty) {
        TyKind::Param { .. } => true,
        TyKind::Ref { inner, .. }
        | TyKind::Vec(inner)
        | TyKind::Slice(inner)
        | TyKind::Iterator(inner)
        | TyKind::Sender(inner)
        | TyKind::Receiver(inner)
        | TyKind::JoinHandle(inner)
        | TyKind::Array { elem: inner, .. } => mentions_param(tcx, *inner),
        TyKind::Tuple(elems) => elems.iter().any(|t| mentions_param(tcx, *t)),
        TyKind::HashMap { key, value, .. } => {
            mentions_param(tcx, *key) || mentions_param(tcx, *value)
        }
        TyKind::Adt { substs, .. } | TyKind::Alias { substs, .. } => {
            substs.types().iter().any(|t| mentions_param(tcx, *t))
        }
        TyKind::FnPtr(sig) | TyKind::FnTrait(sig) => {
            sig.inputs.iter().any(|t| mentions_param(tcx, *t)) || mentions_param(tcx, sig.output)
        }
        _ => false,
    }
}

/// Whether `ty` is a type an instantiation can be named by: no parameter, and
/// no position the checker left unsolved.
fn is_concrete(tcx: &TyCtxt, ty: Ty) -> bool {
    !mentions_param(tcx, ty) && !matches!(tcx.kind_of(ty), TyKind::Var(_) | TyKind::Error)
}

/// The type arguments a callee's function-item type carries, in parameter
/// order. `None` when any position holds a const argument.
fn fn_def_type_args(tcx: &TyCtxt, ty: Ty) -> Option<Vec<Ty>> {
    let TyKind::FnDef { substs, .. } = tcx.kind_of(ty) else {
        return Some(Vec::new());
    };
    substs
        .as_slice()
        .iter()
        .map(|arg| match arg {
            GenericArg::Type(t) => Some(*t),
            GenericArg::Const(_) | GenericArg::ConstParam(_) => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use gossamer_hir::lower_source_file;
    use gossamer_lex::SourceMap;
    use gossamer_parse::parse_source_file;
    use gossamer_resolve::resolve_source_file;
    use gossamer_types::{TyCtxt, typecheck_source_file};

    use super::*;

    fn dispatch_for(source: &str) -> ParamDispatch {
        let mut map = SourceMap::new();
        let file = map.add_file("test.gos", source.to_string());
        let (sf, parse_diags) = parse_source_file(source, file);
        assert!(parse_diags.is_empty(), "parse: {parse_diags:?}");
        let (resolutions, _) = resolve_source_file(&sf);
        let mut tcx = TyCtxt::new();
        let (table, _) = typecheck_source_file(&sf, &resolutions, &mut tcx);
        let program = lower_source_file(&sf, &resolutions, &table, &mut tcx);
        ParamDispatch::build(&program, &mut tcx)
    }

    #[test]
    fn static_trait_function_through_parameter_gets_one_instance_per_type() {
        let dispatch = dispatch_for(
            "trait Zero {\n    fn zero() -> Self\n}\n\nimpl Zero for f64 {\n    fn zero() -> f64 {\n        0.125\n    }\n}\n\nimpl Zero for i64 {\n    fn zero() -> i64 {\n        7\n    }\n}\n\nfn make<T: Zero>() -> T {\n    T::zero()\n}\n\nfn main() {\n    let f: f64 = make()\n    let n: i64 = make()\n    println(\"{} {}\", f, n)\n}\n",
        );
        assert!(
            dispatch.dependent_names.contains("make"),
            "dependent: {:?}",
            dispatch.dependent_names
        );
        assert_eq!(
            dispatch.instances.len(),
            2,
            "instances: {:?}",
            dispatch.instances
        );
        let resolved: HashSet<&str> = dispatch.targets.values().map(String::as_str).collect();
        assert!(
            resolved.contains("f64::zero"),
            "targets: {:?}",
            dispatch.targets
        );
        assert!(
            resolved.contains("i64::zero"),
            "targets: {:?}",
            dispatch.targets
        );
    }

    #[test]
    fn generic_impl_method_through_parameter_gets_an_instance_per_receiver() {
        let dispatch = dispatch_for(
            "trait Zero {\n    fn zero() -> Self\n}\n\nimpl Zero for f64 {\n    fn zero() -> f64 {\n        0.125\n    }\n}\n\nstruct Wrap<T> {\n    v: T\n}\n\nimpl<T: Zero> Wrap<T> {\n    fn fresh(&self) -> T {\n        T::zero()\n    }\n}\n\nfn main() {\n    let wf = Wrap { v: 1.5 }\n    println(\"{}\", wf.fresh())\n}\n",
        );
        assert_eq!(
            dispatch.instances.len(),
            1,
            "instances: {:?}",
            dispatch.instances
        );
        let resolved: HashSet<&str> = dispatch.targets.values().map(String::as_str).collect();
        assert!(
            resolved.contains("f64::zero"),
            "targets: {:?}",
            dispatch.targets
        );
    }
}
