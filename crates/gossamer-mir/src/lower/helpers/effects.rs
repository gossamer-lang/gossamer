//! Per-function, per-parameter effect summaries.
//!
//! One walk over each function body records what the function may do to the
//! values it is handed: change a container's length, write a heap value into
//! storage reached through a parameter, or keep a share of a parameter in a
//! value it builds, returns, or stores. A fixpoint over the call graph folds
//! each callee's answer into its callers.
//!
//! Two clients read the summary. The automatic arena analysis asks whether a
//! call made inside a region can let a region value outlive it or leave an
//! outer value's share inside a region object. The bounds-check proofs ask
//! whether a call can change the length of a vector a loop indexes.
//!
//! Every answer over-approximates: a mention the walk cannot classify counts
//! as every effect at once.

use std::collections::{HashMap, HashSet};

use gossamer_hir::{
    HirArrayExpr, HirBlock, HirExpr, HirExprKind, HirFn, HirItemKind, HirProgram, HirStmtKind,
    for_each_child_expr,
};
use gossamer_resolve::DefId;
use gossamer_types::{Ty, TyCtxt, TyKind, is_mutating_method_name};

use super::escape::{cannot_carry, is_copy_ty, pat_binding_names};

/// What a function may do with one of its parameters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ParamEffects {
    /// The parameter's container may change length or be replaced whole.
    pub resizes: bool,
    /// The function may write a heap value into storage reached through the
    /// parameter, or grow that storage.
    pub stores_into: bool,
    /// The function may keep a share of the parameter, or of a value reached
    /// through it, inside a value it builds, returns, or stores.
    pub retains: bool,
}

impl ParamEffects {
    const ALL: Self = Self {
        resizes: true,
        stores_into: true,
        retains: true,
    };

    fn join(&mut self, other: Self) -> bool {
        let before = *self;
        self.resizes |= other.resizes;
        self.stores_into |= other.stores_into;
        self.retains |= other.retains;
        *self != before
    }

    /// Whether handing the function a value it does not own leaves that value
    /// untouched and unshared once the call returns.
    #[must_use]
    pub fn leaves_untouched(self) -> bool {
        !self.stores_into && !self.retains
    }
}

/// What a function may do, summarised over its whole body and every callee.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FnEffects {
    /// The function may let a value outlive its call through a goroutine, a
    /// channel, a static, a closure, or a callee it cannot vet.
    pub escapes: bool,
    /// The function may build a value whose region-allocatable parts hold a
    /// heap-only object (see [`strands_heap_child`]): run inside an arena
    /// region, the region's bulk free would drop that object unreleased.
    pub strands: bool,
    /// One entry per parameter; a method's receiver is the first.
    pub params: Vec<ParamEffects>,
}

impl FnEffects {
    fn unknown(arity: usize) -> Self {
        Self {
            escapes: true,
            strands: true,
            params: vec![ParamEffects::ALL; arity],
        }
    }

    /// The effects on parameter `pos`, every effect when there is no such
    /// parameter.
    #[must_use]
    pub fn param(&self, pos: usize) -> ParamEffects {
        self.params.get(pos).copied().unwrap_or(ParamEffects::ALL)
    }

    fn join(&mut self, other: &Self) {
        self.escapes |= other.escapes;
        self.strands |= other.strands;
        if self.params.len() < other.params.len() {
            self.params
                .resize(other.params.len(), ParamEffects::default());
        }
        for (mine, theirs) in self.params.iter_mut().zip(&other.params) {
            mine.join(*theirs);
        }
    }
}

/// The function a call reaches, as the summary keys it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Callee {
    Fn(DefId),
    /// A method of the `impl` blocks written for one type, by name and
    /// argument count (receiver included).
    Method(MethodKey),
}

/// A method by the type its `impl` was written for, its name, and its
/// argument count with the receiver. `owner` is `None` for an `impl` whose
/// self type is not a nominal type.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MethodKey {
    owner: Option<DefId>,
    name: String,
    arity: usize,
}

/// Effect summaries for every function and method of a program.
#[derive(Debug, Clone, Default)]
pub struct ProgramEffects {
    fns: HashMap<DefId, FnEffects>,
    methods: HashMap<MethodKey, FnEffects>,
    /// Every method key by name and arity, for a receiver whose type does not
    /// say which `impl` answers.
    by_name: HashMap<(String, usize), Vec<MethodKey>>,
    /// The nominal type each `impl` names, by its written name, for a call
    /// compiled code spells as `Type::method`.
    owners_by_name: HashMap<String, Vec<Option<DefId>>>,
}

impl ProgramEffects {
    /// The summary of the free function `def`, if the program defines it.
    #[must_use]
    pub fn of_fn(&self, def: DefId) -> Option<&FnEffects> {
        self.fns.get(&def)
    }

    /// The summary of the user method a call of `name` with `arity` arguments
    /// (receiver included) reaches on a receiver of type `recv`: the one
    /// `impl` of a nominal receiver, or every candidate joined when the type
    /// does not decide it. `None` when no user `impl` defines one.
    #[must_use]
    pub fn of_method_call(
        &self,
        tcx: &TyCtxt,
        recv: Ty,
        name: &str,
        arity: usize,
    ) -> Option<FnEffects> {
        let targets = method_targets(&self.by_name, tcx, recv, name, arity);
        if targets.is_empty() {
            return None;
        }
        self.join_keys(targets.iter())
    }

    /// The summary for a compiled call target: a free function by `DefId`,
    /// or a method by its `Type::method` symbol and argument count.
    #[must_use]
    pub fn of_symbol(&self, def: Option<DefId>, symbol: &str, arity: usize) -> Option<FnEffects> {
        if let Some(def) = def {
            return self.of_fn(def).cloned();
        }
        let mut parts = symbol.rsplit("::");
        let method = parts.next()?;
        let candidates = self.by_name.get(&(method.to_string(), arity))?;
        match parts.next().and_then(|ty| self.owners_by_name.get(ty)) {
            Some(owners) => {
                self.join_keys(candidates.iter().filter(|key| owners.contains(&key.owner)))
            }
            None => self.join_keys(candidates.iter()),
        }
    }

    fn join_keys<'k>(&self, keys: impl Iterator<Item = &'k MethodKey>) -> Option<FnEffects> {
        let mut joined: Option<FnEffects> = None;
        for key in keys {
            let summary = self.methods.get(key)?;
            match &mut joined {
                Some(acc) => acc.join(summary),
                None => joined = Some(summary.clone()),
            }
        }
        joined
    }
}

/// The user methods a call of `name` with `arity` arguments (receiver
/// included) may reach on a receiver of type `recv`: the receiver's own
/// `impl` when its type names one, every candidate when the type does not
/// decide it.
fn method_targets(
    by_name: &HashMap<(String, usize), Vec<MethodKey>>,
    tcx: &TyCtxt,
    recv: Ty,
    name: &str,
    arity: usize,
) -> Vec<MethodKey> {
    let Some(candidates) = by_name.get(&(name.to_string(), arity)) else {
        return Vec::new();
    };
    if let Some(owner) = nominal_owner(tcx, recv) {
        // The type's own `impl`, or else the trait's default body.
        let own = candidates
            .iter()
            .find(|k| k.owner == Some(owner))
            .or_else(|| candidates.iter().find(|k| k.owner.is_none()));
        return own.cloned().into_iter().collect();
    }
    if is_builtin_data(tcx, recv) && !is_automatic_method(name) {
        // A builtin receiver reaches a user method only through a trait
        // `impl` written for a builtin type or a trait's default body, neither
        // of which has a nominal owner.
        return candidates
            .iter()
            .filter(|k| k.owner.is_none())
            .cloned()
            .collect();
    }
    candidates.clone()
}

/// The key of a trait method's default body.
fn default_method_key(m: &HirFn) -> MethodKey {
    MethodKey {
        owner: None,
        name: m.name.name.clone(),
        arity: m.params.len(),
    }
}

/// The nominal type a receiver of type `ty` names, through references.
fn nominal_owner(tcx: &TyCtxt, ty: Ty) -> Option<DefId> {
    match tcx.kind_of(ty) {
        TyKind::Ref { inner, .. } => nominal_owner(tcx, *inner),
        TyKind::Adt { def, .. } if !is_sentinel_adt(*def) => Some(*def),
        _ => None,
    }
}

/// The key of a method declared in `imp`.
fn method_key(tcx: &TyCtxt, imp: &gossamer_hir::HirImpl, m: &HirFn) -> MethodKey {
    MethodKey {
        owner: nominal_owner(tcx, imp.self_ty),
        name: m.name.name.clone(),
        arity: m.params.len(),
    }
}

/// Builds the effect summary of every function and method in `program`.
#[must_use]
pub fn collect_program_effects(program: &HirProgram, tcx: &TyCtxt) -> ProgramEffects {
    let mut statics: HashSet<DefId> = HashSet::new();
    let mut fn_defs: HashSet<DefId> = HashSet::new();
    let mut by_name: HashMap<(String, usize), Vec<MethodKey>> = HashMap::new();
    let mut owners_by_name: HashMap<String, Vec<Option<DefId>>> = HashMap::new();
    for item in &program.items {
        match &item.kind {
            HirItemKind::Static(s) if !is_copy_ty(tcx, s.ty) => {
                if let Some(def) = item.def {
                    statics.insert(def);
                }
            }
            HirItemKind::Fn(f) if f.body.is_some() => {
                if let Some(def) = item.def {
                    fn_defs.insert(def);
                }
            }
            HirItemKind::Impl(imp) => {
                if let Some(name) = &imp.self_name {
                    let owners = owners_by_name.entry(name.name.clone()).or_default();
                    let owner = nominal_owner(tcx, imp.self_ty);
                    if !owners.contains(&owner) {
                        owners.push(owner);
                    }
                }
                for m in imp.methods.iter().filter(|m| m.body.is_some()) {
                    let key = method_key(tcx, imp, m);
                    let keys = by_name.entry((key.name.clone(), key.arity)).or_default();
                    if !keys.contains(&key) {
                        keys.push(key);
                    }
                }
            }
            HirItemKind::Trait(t) => {
                for m in t.methods.iter().filter(|m| m.body.is_some()) {
                    let key = default_method_key(m);
                    let keys = by_name.entry((key.name.clone(), key.arity)).or_default();
                    if !keys.contains(&key) {
                        keys.push(key);
                    }
                }
            }
            _ => {}
        }
    }
    let ctx = ScanCtx {
        tcx,
        statics: &statics,
        fn_defs: &fn_defs,
        by_name: &by_name,
    };

    let mut summaries: HashMap<Callee, FnEffects> = HashMap::new();
    let mut edges: Vec<(Callee, Vec<Edge>)> = Vec::new();
    let mut record = |key: Callee, f: &HirFn| {
        let (effects, fn_edges) = scan_fn(&ctx, f);
        match summaries.get_mut(&key) {
            Some(existing) => existing.join(&effects),
            None => {
                summaries.insert(key.clone(), effects);
            }
        }
        edges.push((key, fn_edges));
    };
    for item in &program.items {
        match &item.kind {
            HirItemKind::Fn(f) if f.body.is_some() => {
                if let Some(def) = item.def {
                    record(Callee::Fn(def), f);
                }
            }
            HirItemKind::Impl(imp) => {
                for m in imp.methods.iter().filter(|m| m.body.is_some()) {
                    record(Callee::Method(method_key(tcx, imp, m)), m);
                }
            }
            HirItemKind::Trait(t) => {
                for m in t.methods.iter().filter(|m| m.body.is_some()) {
                    record(Callee::Method(default_method_key(m)), m);
                }
            }
            _ => {}
        }
    }

    // A caller takes on whatever the callee does with each position it fills,
    // for the parameters that argument shares storage with. Effects only
    // grow, so the loop settles; recursion settles at its own fixed point.
    loop {
        let mut changed = false;
        for (caller, caller_edges) in &edges {
            for edge in caller_edges {
                let effect = summaries.get(&edge.callee).map_or_else(
                    || (true, true, ParamEffects::ALL),
                    |callee| (callee.escapes, callee.strands, callee.param(edge.pos)),
                );
                let Some(summary) = summaries.get_mut(caller) else {
                    continue;
                };
                if effect.0 && !summary.escapes {
                    summary.escapes = true;
                    changed = true;
                }
                if effect.1 && !summary.strands {
                    summary.strands = true;
                    changed = true;
                }
                for &p in &edge.params {
                    if let Some(slot) = summary.params.get_mut(p) {
                        changed |= slot.join(effect.2);
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }

    let mut out = ProgramEffects {
        by_name,
        owners_by_name,
        ..ProgramEffects::default()
    };
    for (key, effects) in summaries {
        match key {
            Callee::Fn(def) => {
                out.fns.insert(def, effects);
            }
            Callee::Method(key) => {
                out.methods.insert(key, effects);
            }
        }
    }
    out
}

/// A call made with an argument that shares storage with some parameters.
#[derive(Debug, Clone)]
struct Edge {
    callee: Callee,
    pos: usize,
    params: Vec<usize>,
}

struct ScanCtx<'a> {
    tcx: &'a TyCtxt,
    statics: &'a HashSet<DefId>,
    fn_defs: &'a HashSet<DefId>,
    by_name: &'a HashMap<(String, usize), Vec<MethodKey>>,
}

/// How a call to a standard-library function or compiler intrinsic treats
/// its arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StdCall {
    /// Reads its arguments and answers a fresh value; keeps nothing.
    Reads,
    /// Reads its arguments and may answer a value sharing one of them; keeps
    /// nothing else.
    Projects,
    /// Builds a value that holds its arguments.
    Builds,
    /// Not vetted: may keep its arguments anywhere.
    Unknown,
}

/// Standard-library modules whose functions keep nothing they are handed:
/// each answers from its arguments alone and stores none of them.
pub(crate) const STATELESS_STD_MODULES: &[&str] = &[
    "adler32", "ascii85", "base32", "base64", "binary", "bits", "blake3", "crc32", "crc32c", "fnv",
    "hex", "hmac", "math", "sha256", "sha512", "strconv", "strings", "subtle", "unicode", "utf8",
];

/// Classifies a call whose callee path has no user definition.
pub(crate) fn std_call(segments: &[gossamer_ast::Ident]) -> StdCall {
    let names: Vec<&str> = segments.iter().map(|s| s.name.as_str()).collect();
    let names = names.strip_prefix(&["std"]).unwrap_or(&names);
    match names {
        ["__struct" | "__gos_map_literal" | "Some" | "Ok" | "Err"] => StdCall::Builds,
        [name] if is_formatting_intrinsic(name) => StdCall::Reads,
        [
            "__range" | "__range_bound" | "__gos_wrapping_add" | "__gos_wrapping_sub"
            | "__gos_wrapping_mul" | "println" | "print" | "eprintln" | "eprint" | "format"
            | "panic" | "assert" | "assert_eq" | "assert_ne",
        ] => StdCall::Reads,
        [
            "__try_value" | "__try_err" | "__gos_expect_value" | "__gos_expect_err" | "__window"
            | "__entry_k" | "__entry_v" | "__gos_map_key" | "__gos_map_value",
        ] => StdCall::Projects,
        [
            "Vec" | "String" | "Map" | "Set" | "Queue" | "Stack" | "Deque" | "MinHeap" | "MaxHeap"
            | "BTreeMap" | "BTreeSet",
            "new" | "with_capacity",
        ] => StdCall::Reads,
        [module, _] if STATELESS_STD_MODULES.contains(module) => StdCall::Projects,
        ["runtime", "collect_cycles"] => StdCall::Reads,
        _ => StdCall::Unknown,
    }
}

/// The intrinsics string interpolation and `{}` rendering lower to. Each
/// reads its operands and answers a fresh `String`.
fn is_formatting_intrinsic(name: &str) -> bool {
    name == "__concat"
        || name == "__debug"
        || name.starts_with("__fmt_")
        || name.starts_with("__gos_fmt_")
        || matches!(
            name,
            "__gos_dyn_display" | "__gos_dyn_debug" | "__gos_debug_quote"
        )
}

/// Builtin receiver types whose method surface the checker fixes: a method
/// outside `is_mutating_method_name` cannot change such a receiver.
pub(crate) fn is_builtin_data(tcx: &TyCtxt, ty: Ty) -> bool {
    match tcx.kind_of(ty) {
        TyKind::Int(_)
        | TyKind::Float(_)
        | TyKind::Bool
        | TyKind::Char
        | TyKind::Unit
        | TyKind::String
        | TyKind::Vec(_)
        | TyKind::Slice(_)
        | TyKind::Array { .. }
        | TyKind::HashMap { .. }
        | TyKind::Iterator(_)
        | TyKind::Range(_)
        | TyKind::Duration
        | TyKind::Instant => true,
        TyKind::Tuple(elems) => elems.iter().all(|t| is_builtin_data(tcx, *t)),
        TyKind::Ref { inner, .. } => is_builtin_data(tcx, *inner),
        TyKind::Adt { def, substs } if is_sentinel_adt(*def) => {
            substs.types().iter().all(|t| is_builtin_data(tcx, *t))
        }
        _ => false,
    }
}

/// `Option` and `Result`, which the checker models as Adts with reserved ids.
fn is_sentinel_adt(def: DefId) -> bool {
    def.local == u32::MAX || def.local == u32::MAX - 1
}

/// The `next()` a `for` loop over a builtin collection desugars to. The
/// language gives these receivers no `next` method of their own, and the
/// loop lowers to element reads that allocate nothing.
pub(crate) fn is_for_loop_step(tcx: &TyCtxt, recv: Ty, name: &str, args: &[HirExpr]) -> bool {
    let recv = match tcx.kind_of(recv) {
        TyKind::Ref { inner, .. } => *inner,
        _ => recv,
    };
    name == "next"
        && args.is_empty()
        && matches!(
            tcx.kind_of(recv),
            TyKind::Vec(_)
                | TyKind::Slice(_)
                | TyKind::Array { .. }
                | TyKind::Range(_)
                | TyKind::HashMap { .. }
                | TyKind::String
        )
}

/// Whether a value of `ty` built inside an arena region keeps a heap-only
/// object - a runtime handle such as a channel, lock, or file, which is
/// always allocated outside the region - inside storage the region does
/// allocate: a vector's elements or a user type's fields. The region's bulk
/// free reclaims that storage without releasing what it holds, so such an
/// object would never be freed. A map, a JSON value, and a lazy iterator made
/// in a region belong to it and are finalized at its pop. A heap-only object
/// held directly by a local, a tuple, or an `Option` / `Result` carrier is
/// released with its holder.
#[must_use]
pub fn strands_heap_child(tcx: &TyCtxt, ty: Ty) -> bool {
    strands_within(tcx, ty, false, &mut Vec::new())
}

fn strands_within(tcx: &TyCtxt, ty: Ty, in_region: bool, seen: &mut Vec<DefId>) -> bool {
    match tcx.kind_of(ty) {
        TyKind::Int(_)
        | TyKind::Float(_)
        | TyKind::Bool
        | TyKind::Char
        | TyKind::Unit
        | TyKind::Never
        | TyKind::String
        | TyKind::Duration
        | TyKind::Instant
        | TyKind::Range(_)
        | TyKind::Simd { .. } => false,
        TyKind::HashMap { key, value, .. } => {
            strands_within(tcx, *key, true, seen) || strands_within(tcx, *value, true, seen)
        }
        TyKind::JsonValue => false,
        TyKind::Iterator(elem) => strands_within(tcx, *elem, true, seen),
        TyKind::Vec(elem) | TyKind::Slice(elem) => strands_within(tcx, *elem, true, seen),
        TyKind::Array { elem, .. } => strands_within(tcx, *elem, in_region, seen),
        TyKind::Tuple(elems) => elems
            .iter()
            .any(|e| strands_within(tcx, *e, in_region, seen)),
        TyKind::Ref { inner, .. } => strands_within(tcx, *inner, in_region, seen),
        TyKind::Adt { def, substs } if is_sentinel_adt(*def) => substs
            .types()
            .iter()
            .any(|t| strands_within(tcx, *t, in_region, seen)),
        // The other reserved ids are the opaque runtime handles.
        TyKind::Adt { def, .. } if def.local >= u32::MAX - 32 => in_region,
        TyKind::Adt { def, substs } => {
            if seen.contains(def) {
                return false;
            }
            seen.push(*def);
            let fields: Vec<Ty> = match tcx.adt_field_tys(*def, substs) {
                Some(fields) => fields.to_vec(),
                None => tcx
                    .enum_variant_tys(*def)
                    .map(|variants| variants.iter().flatten().copied().collect())
                    .unwrap_or_default(),
            };
            let strands = fields.iter().any(|f| strands_within(tcx, *f, true, seen));
            seen.pop();
            strands
        }
        // Maps, iterators, channels, callables, dynamic values, and anything
        // not yet resolved live outside the region.
        _ => in_region,
    }
}

/// Builtin mutators that rearrange or overwrite elements in place without
/// changing how many there are.
pub(crate) fn keeps_length(method: &str) -> bool {
    matches!(
        method,
        "sort"
            | "sort_by"
            | "sort_by_key"
            | "reverse"
            | "swap"
            | "fill"
            | "copy_within"
            | "copy_from_slice"
            | "reserve"
            | "reserve_exact"
            | "shrink_to_fit"
            | "par_chunks_mut"
    )
}

/// Methods every type answers without an `impl`: comparison, hashing,
/// rendering, and copying. Each reads its receiver and arguments.
pub(crate) fn is_automatic_method(name: &str) -> bool {
    matches!(
        name,
        "eq" | "ne" | "cmp" | "partial_cmp" | "fmt" | "hash" | "clone" | "to_string"
    )
}

/// Walks one function body.
struct FnScan<'a, 'c> {
    ctx: &'a ScanCtx<'c>,
    /// Binding name to the parameters its value may share storage with.
    /// Rebinding a name unions, so a shadowed binding is never forgotten.
    names: HashMap<String, Vec<usize>>,
    effects: FnEffects,
    edges: Vec<Edge>,
}

fn scan_fn(ctx: &ScanCtx<'_>, f: &HirFn) -> (FnEffects, Vec<Edge>) {
    let mut scan = FnScan {
        ctx,
        names: HashMap::new(),
        effects: FnEffects {
            escapes: false,
            strands: false,
            params: vec![ParamEffects::default(); f.params.len()],
        },
        edges: Vec::new(),
    };
    for (i, p) in f.params.iter().enumerate() {
        let mut names = Vec::new();
        pat_binding_names(&p.pattern, &mut names);
        for name in names {
            scan.names.entry(name).or_default().push(i);
        }
    }
    let Some(body) = &f.body else {
        return (FnEffects::unknown(f.params.len()), Vec::new());
    };
    scan.block(&body.block);
    // The body's value is the function's answer.
    if let Some(tail) = &body.block.tail {
        let shared = scan.shares(tail);
        scan.mark(&shared, |e| e.retains = true);
    }
    (scan.effects, scan.edges)
}

impl FnScan<'_, '_> {
    fn tcx(&self) -> &TyCtxt {
        self.ctx.tcx
    }

    fn mark(&mut self, params: &[usize], f: impl Fn(&mut ParamEffects)) {
        for &p in params {
            if let Some(slot) = self.effects.params.get_mut(p) {
                f(slot);
            }
        }
    }

    /// The parameters whose storage `e`'s value may share. A `Copy` value
    /// shares nothing; anything else may share whatever it mentions.
    fn shares(&self, e: &HirExpr) -> Vec<usize> {
        if is_copy_ty(self.tcx(), e.ty) {
            return Vec::new();
        }
        let mut out = Vec::new();
        self.collect_mentions(e, &mut out);
        out.sort_unstable();
        out.dedup();
        out
    }

    fn collect_mentions(&self, e: &HirExpr, out: &mut Vec<usize>) {
        if let HirExprKind::Path { segments, .. } = &e.kind
            && let [only] = segments.as_slice()
            && let Some(params) = self.names.get(&only.name)
        {
            out.extend(params);
        }
        for_each_child_expr(e, &mut |child| self.collect_mentions(child, out));
    }

    fn bind(&mut self, pattern: &gossamer_hir::HirPat, shared: &[usize]) {
        let mut names = Vec::new();
        pat_binding_names(pattern, &mut names);
        for name in names {
            let entry = self.names.entry(name).or_default();
            entry.extend(shared);
            entry.sort_unstable();
            entry.dedup();
        }
    }

    fn block(&mut self, b: &HirBlock) {
        for s in &b.stmts {
            match &s.kind {
                HirStmtKind::Let { pattern, init, .. } => {
                    if let Some(init) = init {
                        self.expr(init);
                        let shared = self.shares(init);
                        self.bind(pattern, &shared);
                    }
                }
                HirStmtKind::Expr { expr, .. } | HirStmtKind::Defer(expr) => self.expr(expr),
                HirStmtKind::Item(_) => {}
            }
        }
        if let Some(t) = &b.tail {
            self.expr(t);
        }
    }

    fn expr(&mut self, e: &HirExpr) {
        if matches!(
            e.kind,
            HirExprKind::Call { .. } | HirExprKind::MethodCall { .. } | HirExprKind::Array(_)
        ) && strands_heap_child(self.tcx(), e.ty)
        {
            self.effects.strands = true;
        }
        match &e.kind {
            HirExprKind::Select { .. }
            | HirExprKind::Closure { .. }
            | HirExprKind::LiftedClosure { .. } => {
                // A closure value or a select arm can carry anything anywhere.
                self.effects.escapes = true;
            }
            HirExprKind::Path { def: Some(d), .. } if self.ctx.statics.contains(d) => {
                // A heap static is shared storage the summary cannot follow.
                self.effects.escapes = true;
            }
            HirExprKind::Return(Some(value)) => {
                let shared = self.shares(value);
                self.mark(&shared, |p| p.retains = true);
            }
            HirExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                let shared = self.shares(scrutinee);
                for arm in arms {
                    self.bind(&arm.pattern, &shared);
                    if let Some(g) = &arm.guard {
                        self.expr(g);
                    }
                    self.expr(&arm.body);
                }
                return;
            }
            HirExprKind::Block(b) => {
                self.block(b);
                return;
            }
            HirExprKind::Assign { place, value } => self.assign(place, value),
            HirExprKind::Call { callee, args } => self.call(e, callee, args),
            HirExprKind::MethodCall {
                receiver,
                name,
                args,
                ..
            } => self.method_call(e, receiver, &name.name, args),
            HirExprKind::Array(HirArrayExpr::List(items)) => {
                for item in items {
                    let shared = self.shares(item);
                    self.mark(&shared, |p| p.retains = true);
                }
            }
            HirExprKind::Array(HirArrayExpr::Repeat { value, .. }) => {
                let shared = self.shares(value);
                self.mark(&shared, |p| p.retains = true);
            }
            _ => {}
        }
        for_each_child_expr(e, &mut |child| self.expr(child));
    }

    fn assign(&mut self, place: &HirExpr, value: &HirExpr) {
        let root = place_root(place);
        if let Some(HirExprKind::Path { def: Some(d), .. }) = root.map(|r| &r.kind)
            && self.ctx.statics.contains(d)
        {
            self.effects.escapes = true;
        }
        let place_params: Vec<usize> = root
            .and_then(|r| match &r.kind {
                HirExprKind::Path { segments, .. } => segments.first(),
                _ => None,
            })
            .and_then(|seg| self.names.get(&seg.name))
            .cloned()
            .unwrap_or_default();
        let whole = matches!(peel_unary(place).kind, HirExprKind::Path { .. });
        if whole {
            // Replacing a parameter's whole value.
            self.mark(&place_params, |p| {
                p.resizes = true;
                p.stores_into = true;
            });
        } else if !is_copy_ty(self.tcx(), value.ty) {
            self.mark(&place_params, |p| p.stores_into = true);
        }
        let shared = self.shares(value);
        let local_rebind =
            whole && place_params.is_empty() && matches!(place.kind, HirExprKind::Path { .. });
        if local_rebind {
            // A plain local rebinding: the local now shares what the value
            // shares, and its own drop gives the share back.
            if let HirExprKind::Path { segments, .. } = &place.kind
                && let [only] = segments.as_slice()
            {
                let entry = self.names.entry(only.name.clone()).or_default();
                entry.extend(&shared);
                entry.sort_unstable();
                entry.dedup();
            }
        } else {
            self.mark(&shared, |p| p.retains = true);
        }
    }

    fn call(&mut self, e: &HirExpr, callee: &HirExpr, args: &[HirExpr]) {
        match &callee.kind {
            HirExprKind::Path { def: Some(d), .. } if self.ctx.fn_defs.contains(d) => {
                for (pos, arg) in args.iter().enumerate() {
                    let params = self.shares(arg);
                    self.edges.push(Edge {
                        callee: Callee::Fn(*d),
                        pos,
                        params,
                    });
                }
            }
            // A variant or tuple-struct constructor holds its arguments.
            HirExprKind::Path { def: Some(_), .. } => self.retain_args(args),
            HirExprKind::Path {
                def: None,
                segments,
            } => match std_call(segments) {
                StdCall::Reads | StdCall::Projects => {
                    // A share the answer may hold is recorded by the binding
                    // the answer lands in, not by the call.
                }
                StdCall::Builds => self.retain_args(args),
                StdCall::Unknown => {
                    self.retain_args(args);
                    if args.iter().any(|a| !is_copy_ty(self.tcx(), a.ty))
                        || !cannot_carry(self.tcx(), e.ty)
                    {
                        self.effects.escapes = true;
                    }
                }
            },
            // A call through a local callable can do anything.
            _ => {
                self.effects.escapes = true;
                self.retain_args(args);
            }
        }
    }

    fn retain_args(&mut self, args: &[HirExpr]) {
        for a in args {
            let shared = self.shares(a);
            self.mark(&shared, |p| p.retains = true);
        }
    }

    fn method_call(&mut self, e: &HirExpr, receiver: &HirExpr, name: &str, args: &[HirExpr]) {
        let recv_params = self.shares_place(receiver);
        let arity = args.len() + 1;
        let builtin = is_builtin_data(self.tcx(), receiver.ty) || is_automatic_method(name);
        let targets = method_targets(self.ctx.by_name, self.tcx(), receiver.ty, name, arity);
        if !targets.is_empty() {
            // A user `impl` may answer the call, whatever the receiver.
            let operands: Vec<Vec<usize>> = std::iter::once(recv_params.clone())
                .chain(args.iter().map(|a| self.shares(a)))
                .collect();
            for key in targets {
                for (pos, params) in operands.iter().enumerate() {
                    self.edges.push(Edge {
                        callee: Callee::Method(key.clone()),
                        pos,
                        params: params.clone(),
                    });
                }
            }
        } else if !builtin {
            // A trait method on a type parameter, or a handle's method: nothing
            // vets it.
            self.effects.escapes = true;
            self.mark(&recv_params, |p| *p = ParamEffects::ALL);
            self.retain_args(args);
            return;
        }
        if !builtin {
            return;
        }
        if is_mutating_method_name(name) {
            self.mark(&recv_params, |p| {
                p.stores_into = true;
                if !keeps_length(name) {
                    p.resizes = true;
                }
            });
            self.retain_args(args);
        } else if is_for_loop_step(self.tcx(), receiver.ty, name, args) {
            // The element a `for` loop binds is a read of the container,
            // which the binding records.
        } else if !cannot_carry(self.tcx(), e.ty) {
            // A reading method that answers a container, an iterator, or a
            // view may hold the receiver or an argument inside it.
            self.mark(&recv_params, |p| p.retains = true);
            self.retain_args(args);
        }
    }

    /// The parameters a place expression's storage belongs to: the receiver
    /// of a method reaches its root binding however it is projected.
    fn shares_place(&self, e: &HirExpr) -> Vec<usize> {
        let mut out = Vec::new();
        self.collect_mentions(e, &mut out);
        out.sort_unstable();
        out.dedup();
        out
    }
}

/// The root path of a place expression (`x`, `*x`, `x.f`, `x[i]`).
fn place_root(expr: &HirExpr) -> Option<&HirExpr> {
    match &expr.kind {
        HirExprKind::Path { .. } => Some(expr),
        HirExprKind::Unary { operand, .. } => place_root(operand),
        HirExprKind::Field { receiver, .. } | HirExprKind::TupleIndex { receiver, .. } => {
            place_root(receiver)
        }
        HirExprKind::Index { base, .. } => place_root(base),
        _ => None,
    }
}

fn peel_unary(expr: &HirExpr) -> &HirExpr {
    match &expr.kind {
        HirExprKind::Unary { operand, .. } => peel_unary(operand),
        _ => expr,
    }
}
