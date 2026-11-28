// Copies of a vector share its element buffer until one of them writes
// (the runtime's `vec_share` / `vec_unshare`). A write through an element of
// a vector - `rows[i].push(x)`, `grid[i][j] = v`, `bags[i].items[0] = 1` -
// reaches a nested vector whose header the buffer holds, and two vectors
// sharing that buffer hold the same nested header. Each vector on the path
// is therefore given a buffer of its own, outermost first, before the
// element read the write goes through, so the header the write reaches is
// the writer's own.
//
// The stores the backends emit inline (`gos_rt_vec_set_i64` and the other
// element writers below) do not pass through the runtime, so the vector they
// write is given its own buffer before the store; inside a loop nothing in
// which can share the vector again, once before the loop.

/// Runtime calls that write the vector at the argument index given.
const COW_VEC_WRITERS: &[(&str, usize)] = &[
    ("gos_rt_vec_assign", 0),
    ("gos_rt_vec_clear", 0),
    ("gos_rt_vec_compact_elems", 0),
    ("gos_rt_vec_copy_from_slice", 0),
    ("gos_rt_vec_copy_range_unchecked", 0),
    ("gos_rt_vec_copy_within", 0),
    ("gos_rt_vec_extend", 0),
    ("gos_rt_vec_fill_range_unchecked", 0),
    ("gos_rt_vec_insert_at", 0),
    ("gos_rt_vec_insert_safe", 0),
    ("gos_rt_vec_insert_slots_safe", 0),
    ("gos_rt_vec_pop", 0),
    ("gos_rt_vec_pop_into", 0),
    ("gos_rt_vec_pop_opt", 0),
    ("gos_rt_vec_push", 0),
    ("gos_rt_vec_push_i128", 0),
    ("gos_rt_vec_push_i64", 0),
    ("gos_rt_vec_remove_at", 0),
    ("gos_rt_vec_remove_safe", 0),
    ("gos_rt_vec_reserve_at_least", 0),
    ("gos_rt_vec_reserve_exact", 0),
    ("gos_rt_vec_reverse", 0),
    ("gos_rt_vec_set_i128", 0),
    ("gos_rt_vec_set_i64", 0),
    ("gos_rt_vec_set_i64_unchecked", 0),
    ("gos_rt_vec_set_slots", 0),
    ("gos_rt_vec_sort_by_aggr", 0),
    ("gos_rt_vec_sort_by_f64", 0),
    ("gos_rt_vec_sort_by_i64", 0),
    ("gos_rt_vec_sort_f64", 0),
    ("gos_rt_vec_sort_i64", 0),
    ("gos_rt_vec_sort_str", 0),
    ("gos_rt_vec_sort_tuple", 0),
    ("gos_rt_vec_swap_i64", 0),
    ("gos_rt_vec_swap_safe", 0),
    ("gos_rt_vec_swap_unchecked", 0),
    ("gos_rt_vec_truncate", 0),
    ("gos_rt_vec_window", 0),
    ("gos_rt_par_chunks", 1),
    ("gos_rt_map_entries_into", 1),
    ("gos_rt_map_entries_into_u64", 1),
];

/// The writers the backends may lower to inline stores, and the pushes, which
/// LLVM appends inline into spare capacity: each is given its own buffer
/// before the call, and a push then calls the entry that does not check again.
const COW_INLINE_WRITERS: &[&str] = &[
    "gos_rt_vec_push",
    "gos_rt_vec_set_i64",
    "gos_rt_vec_set_i64_unchecked",
    "gos_rt_vec_swap_i64",
    "gos_rt_vec_swap_safe",
    "gos_rt_vec_swap_unchecked",
    "gos_rt_vec_copy_range_unchecked",
    "gos_rt_vec_fill_range_unchecked",
];

/// Reads answering an element of the vector in argument 0: its word (a
/// nested vector's header) or its address.
const COW_ELEMENT_READS: &[&str] = &[
    "gos_rt_vec_get_i64",
    "gos_rt_vec_get_i64_unchecked",
    "gos_rt_vec_get_ptr",
    "gos_rt_vec_get_ptr_unchecked",
];

const COW_UNSHARE: &str = "gos_rt_vec_unshare";

/// Where a local's value came from, as far as the write path cares.
#[derive(Clone, Copy)]
enum CowOrigin {
    /// An element read of `container` at this site.
    Element { container: Local, site: CowSite },
    /// A copy of another local.
    Copy(Local),
    /// A field read through another local, a pointer to an element.
    Through(Local),
}

/// A call site: the terminator of `block`, or its statement `stmt`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct CowSite {
    block: usize,
    stmt: Option<usize>,
}

fn cow_call_name(callee: &Operand) -> Option<&str> {
    match callee {
        Operand::Const(ConstValue::Str(name)) => Some(name.as_str()),
        _ => None,
    }
}

fn cow_operand_local(op: &Operand) -> Option<Local> {
    match op {
        Operand::Copy(place) if place.projection.is_empty() => Some(place.local),
        _ => None,
    }
}

/// Gives every vector a write reaches through an element a buffer of its
/// own first (see the module comment).
fn unshare_written_vectors(body: &mut Body, tcx: &TyCtxt, effects: &ProgramEffects) {
    const OWNED_PUSH: &str = "gos_rt_vec_push_owned";
    let scan = cow_scan(body, tcx, effects);
    let (sites, hoists) = cow_unshare_sites(body, tcx, effects, scan);
    cow_insert_unshares(body, tcx, sites);
    cow_apply_hoists(body, tcx, hoists);
    // Every push onto a local was checked above, or known to need no check.
    let checked = |args: &[Operand]| args.first().and_then(cow_operand_local).is_some();
    for block in &mut body.blocks {
        for stmt in &mut block.stmts {
            if let StatementKind::Assign {
                rvalue: Rvalue::CallIntrinsic { name, args },
                ..
            } = &mut stmt.kind
                && *name == "gos_rt_vec_push"
                && checked(args)
            {
                *name = OWNED_PUSH;
            }
        }
        if let Terminator::Call { callee, args, .. } = &mut block.terminator
            && matches!(callee, Operand::Const(ConstValue::Str(name)) if name == "gos_rt_vec_push")
            && checked(args)
        {
            *callee = Operand::Const(ConstValue::Str(OWNED_PUSH.to_string()));
        }
    }
}

/// The element reads, writes, and inline stores of a body.
struct CowScan {
    origins: HashMap<Local, Vec<CowOrigin>>,
    sinks: Vec<(Local, CowSite)>,
    inline_writes: Vec<(Local, CowSite)>,
}

fn cow_scan(body: &Body, tcx: &TyCtxt, effects: &ProgramEffects) -> CowScan {
    let mut origins: HashMap<Local, Vec<CowOrigin>> = HashMap::new();
    let mut sinks: Vec<(Local, CowSite)> = Vec::new();
    let mut inline_writes: Vec<(Local, CowSite)> = Vec::new();
    for (b, block) in body.blocks.iter().enumerate() {
        for (s, stmt) in block.stmts.iter().enumerate() {
            let site = CowSite { block: b, stmt: Some(s) };
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            if let Some(base) = (!place.projection.is_empty()).then_some(place.local) {
                // A store through a pointer writes the element it points at.
                sinks.push((base, site));
            }
            if !place.projection.is_empty() {
                continue;
            }
            match rvalue {
                Rvalue::Use(Operand::Copy(src)) | Rvalue::Ref { place: src, .. }
                    if src.projection.is_empty() =>
                {
                    origins.entry(place.local).or_default().push(CowOrigin::Copy(src.local));
                }
                // Any projection of a local - a field or element of an element
                // pointer, with or without a deref first - reads through it.
                Rvalue::Use(Operand::Copy(src)) | Rvalue::Ref { place: src, .. }
                    if !src.projection.is_empty() =>
                {
                    origins.entry(place.local).or_default().push(CowOrigin::Through(src.local));
                }
                Rvalue::CallIntrinsic { name, args } => {
                    let mut notes = CowNotes {
                        origins: &mut origins,
                        sinks: &mut sinks,
                        inline_writes: &mut inline_writes,
                    };
                    notes.call(name, args, site, Some(place.local));
                }
                _ => {}
            }
        }
        if let Terminator::Call { callee, args, destination, .. } = &block.terminator {
            let site = CowSite { block: b, stmt: None };
            if let Some(name) = cow_call_name(callee) {
                let dest = destination.projection.is_empty().then_some(destination.local);
                let mut notes = CowNotes {
                    origins: &mut origins,
                    sinks: &mut sinks,
                    inline_writes: &mut inline_writes,
                };
                notes.call(name, args, site, dest);
            }
            // Program code takes a vector `&mut` parameter as holding its
            // buffer alone and writes what it reaches. A vector argument
            // crosses as its header whether the parameter is `&mut` or not, so
            // a vector the callee's summary does not show taken by value holds
            // its buffer alone at the call, and is a write path.
            let summary = match callee {
                Operand::FnRef { def, .. } => Some(effects.of_symbol(Some(*def), "", args.len())),
                Operand::Const(ConstValue::Str(name)) if is_gossamer_function_symbol(name) => {
                    Some(effects.of_symbol(None, name, args.len()))
                }
                Operand::Const(_) => None,
                Operand::Copy(_) => Some(None),
            };
            if let Some(summary) = summary {
                for (i, arg) in args.iter().enumerate() {
                    let lent = summary.as_ref().is_none_or(|s| s.param(i).lends);
                    if lent
                        && let Some(l) = cow_operand_local(arg)
                        && cow_ty_is_vec(tcx, body.locals[l.0 as usize].ty)
                    {
                        sinks.push((l, site));
                        inline_writes.push((l, site));
                    }
                }
            }
        }
    }
    CowScan {
        origins,
        sinks,
        inline_writes,
    }
}

/// Each site and the vectors given a buffer of their own just before it.
fn cow_unshare_sites(
    body: &Body,
    tcx: &TyCtxt,
    effects: &ProgramEffects,
    scan: CowScan,
) -> (BTreeMap<CowSite, Vec<Local>>, Vec<CowHoist>) {
    let CowScan {
        origins,
        sinks,
        inline_writes,
    } = scan;
    let loops = crate::preempt::natural_loops(body);
    let mut unshare_at: BTreeMap<CowSite, Vec<Local>> = BTreeMap::new();
    let mut seen: HashSet<(Local, CowSite)> = HashSet::new();
    let mut work = sinks;
    while let Some((local, site)) = work.pop() {
        if !seen.insert((local, site))
            || !cow_ty_is_writable(tcx, body.locals[local.0 as usize].ty)
        {
            continue;
        }
        for origin in origins.get(&local).into_iter().flatten() {
            match *origin {
                CowOrigin::Element { container, site } => {
                    let at = cow_hoisted_site(body, effects, &loops, &origins, container, site).unwrap_or(site);
                    unshare_at.entry(at).or_default().push(container);
                    work.push((container, site));
                }
                CowOrigin::Copy(src) | CowOrigin::Through(src) => work.push((src, site)),
            }
        }
    }
    for (vec, site) in inline_writes {
        let at = cow_hoisted_site(body, effects, &loops, &origins, vec, site).unwrap_or(site);
        unshare_at.entry(at).or_default().push(vec);
    }
    let hoists = cow_drop_repeated_checks(body, tcx, effects, &origins, &mut unshare_at);
    (unshare_at, hoists)
}

/// Where a vector a check names was read from, so that two reads of the same
/// place - `self.cells[y]` on every turn of a loop over `x` - count as one
/// vector: a local, a field path below another, or an element of another.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum CowKey {
    Local(Local),
    Field(Box<CowKey>, Vec<Projection>),
    Elem(Box<CowKey>, CowIndex),
}

/// The index an element read takes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum CowIndex {
    Local(Local),
    Const(i128),
}

/// How a local is defined, when one statement or call defines it.
#[derive(Clone)]
enum CowPlace {
    Field(Local, Vec<Projection>),
    Elem(Local, CowIndex),
}

impl CowKey {
    /// The locals this place is read through: reassigning one changes it.
    fn leaves(&self, out: &mut Vec<Local>) {
        match self {
            Self::Local(l) => out.push(*l),
            Self::Field(base, proj) => {
                base.leaves(out);
                // An index into a place is read through its local too.
                out.extend(proj.iter().filter_map(|p| match p {
                    Projection::Index(l) => Some(*l),
                    _ => None,
                }));
            }
            Self::Elem(base, index) => {
                base.leaves(out);
                if let CowIndex::Local(l) = index {
                    out.push(*l);
                }
            }
        }
    }

    /// Whether `inner` is this key or a place it is read through.
    fn reaches(&self, inner: &Self) -> bool {
        self == inner
            || match self {
                Self::Local(_) => false,
                Self::Field(base, _) | Self::Elem(base, _) => base.reaches(inner),
            }
    }
}

/// The single definition of each local that is a field or element read.
fn cow_places(body: &Body) -> HashMap<Local, CowPlace> {
    let mut defs: HashMap<Local, Option<CowPlace>> = HashMap::new();
    let mut note = |local: Local, place: Option<CowPlace>| {
        defs.entry(local)
            .and_modify(|d| *d = None)
            .or_insert(place);
    };
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign { place, rvalue } = &stmt.kind
                && place.projection.is_empty()
            {
                let def = match rvalue {
                    // The zero a local starts at holds no vector a check names.
                    Rvalue::Use(Operand::Const(_)) => continue,
                    Rvalue::Use(Operand::Copy(src)) if !src.projection.is_empty() => {
                        Some(CowPlace::Field(src.local, src.projection.clone()))
                    }
                    _ => None,
                };
                note(place.local, def);
            }
        }
        if let Terminator::Call { callee, args, destination, .. } = &block.terminator
            && destination.projection.is_empty()
        {
            let def = match (cow_call_name(callee), args.as_slice()) {
                (Some(name), [container, index, ..]) if COW_ELEMENT_READS.contains(&name) => {
                    let index = match index {
                        Operand::Copy(p) if p.projection.is_empty() => Some(CowIndex::Local(p.local)),
                        Operand::Const(ConstValue::Int(n)) => Some(CowIndex::Const(*n)),
                        _ => None,
                    };
                    cow_operand_local(container)
                        .zip(index)
                        .map(|(c, i)| CowPlace::Elem(c, i))
                }
                _ => None,
            };
            note(destination.local, def);
        }
    }
    defs.into_iter().filter_map(|(l, d)| d.map(|d| (l, d))).collect()
}

fn cow_key(places: &HashMap<Local, CowPlace>, local: Local, depth: u32) -> CowKey {
    if depth > 8 {
        return CowKey::Local(local);
    }
    match places.get(&local) {
        Some(CowPlace::Field(base, proj)) => {
            CowKey::Field(Box::new(cow_key(places, *base, depth + 1)), proj.clone())
        }
        Some(CowPlace::Elem(container, index)) => {
            CowKey::Elem(Box::new(cow_key(places, *container, depth + 1)), index.clone())
        }
        None => CowKey::Local(local),
    }
}

/// Calls that read a vector without changing what any of its places holds.
fn cow_call_reads_places(name: &str) -> bool {
    COW_ELEMENT_READS.contains(&name)
        || matches!(name, "gos_rt_vec_len" | "gos_rt_len" | "gos_rt_vec_capacity")
}

/// The vector a runtime writer changes in place, when the call leaves it
/// holding its buffer alone: every such writer separates its target first,
/// and none hands the target's buffer to another header. A target the call
/// also takes as another argument is not one.
fn cow_in_place_target(name: &str, args: &[Operand]) -> Option<Local> {
    let writes_in_place = COW_VEC_WRITERS
        .iter()
        .any(|&(n, at)| n == name && at == 0)
        && !matches!(name, "gos_rt_vec_assign" | "gos_rt_vec_window");
    if !writes_in_place {
        return None;
    }
    let target = args.first().and_then(cow_operand_local)?;
    let again = args[1..].iter().any(|a| match a {
        Operand::Copy(place) => place.local == target,
        Operand::Const(_) | Operand::FnRef { .. } => false,
    });
    (!again).then_some(target)
}

/// Drops every check of a vector that already holds its buffer alone where
/// the check stands, and moves checks of places a loop leaves alone ahead of
/// the loop (see [`CowFlow`]).
fn cow_drop_repeated_checks(
    body: &Body,
    tcx: &TyCtxt,
    effects: &ProgramEffects,
    origins: &HashMap<Local, Vec<CowOrigin>>,
    unshare_at: &mut BTreeMap<CowSite, Vec<Local>>,
) -> Vec<CowHoist> {
    if unshare_at.is_empty() || body.blocks.is_empty() {
        return Vec::new();
    }
    let flow = CowFlow::new(body, tcx, effects, origins);
    flow.drop_known(unshare_at);
    flow.hoist(unshare_at)
}

/// A forward must-analysis over a body of which places hold a vector whose
/// buffer it holds alone (see [`CowKey`]).
///
/// A check makes its vector hold its buffer alone, and gives the vectors its
/// elements hold headers of their own, so it ends what was known of places
/// below it. Reassigning a local a place is read through ends what was known
/// of the place; a store ends what was known at or below the place it
/// writes. A runtime call that does more than read a vector ends what was
/// known of every place at, above, or below its arguments, and of any place a
/// call could share. A program function shares only what its summary says it
/// keeps, and replaces what lies below a vector it takes `&mut`. A `&mut`
/// vector parameter of a named function holds its buffer alone on entry,
/// which every caller's check before the call establishes.
struct CowFlow<'a> {
    body: &'a Body,
    effects: &'a ProgramEffects,
    origins: &'a HashMap<Local, Vec<CowOrigin>>,
    places: HashMap<Local, CowPlace>,
    keys: std::cell::RefCell<HashMap<Local, CowKey>>,
    derived: std::cell::RefCell<HashMap<Local, HashSet<Local>>>,
    entry: HashSet<CowKey>,
    preds: Vec<Vec<usize>>,
}

type CowState = HashSet<CowKey>;

impl<'a> CowFlow<'a> {
    fn new(
        body: &'a Body,
        tcx: &TyCtxt,
        effects: &'a ProgramEffects,
        origins: &'a HashMap<Local, Vec<CowOrigin>>,
    ) -> Self {
        let n = body.blocks.len();
        let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (b, block) in body.blocks.iter().enumerate() {
            for s in crate::preempt::successors(&block.terminator) {
                if let Some(into) = preds.get_mut(s) {
                    into.push(b);
                }
            }
        }
        // Program code is the only caller of a named function, and checks
        // every vector it hands one. A closure may be called by the runtime
        // with a vector stored where copies share it, so it starts knowing
        // nothing.
        let named = !body.name.starts_with("__closure");
        let entry = (1..=body.arity)
            .map(Local)
            .filter(|l| named && cow_ty_is_mut_vec(tcx, body.locals[l.0 as usize].ty))
            .map(CowKey::Local)
            .collect();
        Self {
            body,
            effects,
            origins,
            places: cow_places(body),
            keys: std::cell::RefCell::default(),
            derived: std::cell::RefCell::default(),
            entry,
            preds,
        }
    }

    fn key(&self, l: Local) -> CowKey {
        self.keys
            .borrow_mut()
            .entry(l)
            .or_insert_with(|| cow_key(&self.places, l, 0))
            .clone()
    }

    /// The locals holding any vector a place is read through, or a value read
    /// out of one.
    fn derived_for(&self, k: &CowKey) -> HashSet<Local> {
        let mut leaves = Vec::new();
        k.leaves(&mut leaves);
        let mut memo = self.derived.borrow_mut();
        let mut out = HashSet::new();
        for l in leaves {
            out.extend(
                memo.entry(l)
                    .or_insert_with(|| cow_derived_locals(self.origins, l))
                    .iter()
                    .copied(),
            );
        }
        out
    }

    /// Ends what was known of every place read through `local`.
    fn kill_leaf(state: &mut CowState, local: Local) {
        state.retain(|k| {
            let mut leaves = Vec::new();
            k.leaves(&mut leaves);
            !leaves.contains(&local)
        });
    }

    /// Ends what was known of every place at, above, or below `locals`, or
    /// read through one.
    fn kill_mentions(&self, state: &mut CowState, locals: &[Local]) {
        let mentioned: Vec<CowKey> = locals.iter().map(|&l| self.key(l)).collect();
        state.retain(|k| {
            let mut leaves = Vec::new();
            k.leaves(&mut leaves);
            !locals.iter().any(|l| leaves.contains(l))
                && !mentioned.iter().any(|m| k.reaches(m) || m.reaches(k))
        });
    }

    /// What a runtime call that does more than read ends of what is known:
    /// every place at, above, or below its arguments. A writer changing a
    /// vector in place changes only what lies below it: separating its target
    /// gives the target a buffer of its own, which leaves the target holding
    /// its buffer alone if it did and the buffer holding the target as it was.
    fn step_runtime_call(&self, state: &mut CowState, name: &str, args: &[Operand]) {
        let Some(target) = cow_in_place_target(name, args) else {
            let locals: Vec<Local> = args.iter().filter_map(cow_operand_local).collect();
            self.kill_mentions(state, &locals);
            return;
        };
        let others: Vec<Local> = args[1..].iter().filter_map(cow_operand_local).collect();
        self.kill_mentions(state, &others);
        let target_key = self.key(target);
        state.retain(|k| k == &target_key || !k.reaches(&target_key));
    }

    fn step_stmt(&self, state: &mut CowState, stmt: &Statement) {
        match &stmt.kind {
            StatementKind::Assign { place, rvalue } => {
                if place.projection.is_empty() {
                    // A local whose one definition reads a place leaves that
                    // place as it was; any other local is keyed by itself.
                    Self::kill_leaf(state, place.local);
                } else {
                    // A store through a local changes what lies at or below
                    // the place it writes; the vector holding that place keeps
                    // its buffer as it was.
                    let written = self.key(place.local);
                    Self::kill_leaf(state, place.local);
                    state.retain(|k| !k.reaches(&written));
                }
                if let Rvalue::CallIntrinsic { name, args } = rvalue
                    && !cow_call_reads_places(name)
                {
                    self.step_runtime_call(state, name, args);
                    state.retain(|k| !cow_intrinsic_may_share(name, args, &self.derived_for(k)));
                }
            }
            StatementKind::Nop | StatementKind::StorageLive(_) | StatementKind::StorageDead(_) => {}
            _ => state.clear(),
        }
    }

    fn step_term(&self, state: &mut CowState, terminator: &Terminator) {
        let Terminator::Call {
            callee,
            args,
            destination,
            ..
        } = terminator
        else {
            return;
        };
        // The answer's local, as a plain assignment's.
        Self::kill_leaf(state, destination.local);
        if cow_call_name(callee).is_some_and(cow_call_reads_places) {
            return;
        }
        let program = match callee {
            Operand::FnRef { .. } => true,
            Operand::Const(ConstValue::Str(name)) => is_gossamer_function_symbol(name),
            Operand::Const(_) | Operand::Copy(_) => false,
        };
        if !program {
            self.step_runtime_call(state, cow_call_name(callee).unwrap_or(""), args);
            return;
        }
        state.retain(|k| !cow_call_may_share(self.effects, callee, args, &self.derived_for(k)));
        // What a lent vector holds the callee may replace.
        let summary = match callee {
            Operand::FnRef { def, .. } => self.effects.of_symbol(Some(*def), "", args.len()),
            Operand::Const(ConstValue::Str(name)) => {
                self.effects.of_symbol(None, name, args.len())
            }
            Operand::Const(_) | Operand::Copy(_) => None,
        };
        for (i, arg) in args.iter().enumerate() {
            let lent = summary.as_ref().is_none_or(|s| s.param(i).lends);
            if lent && let Some(a) = cow_operand_local(arg) {
                let lent_key = self.key(a);
                state.retain(|k| k == &lent_key || !k.reaches(&lent_key));
            }
        }
    }

    /// Applies the checks at `site`: with `drop`, first strips those of a
    /// vector already alone.
    fn apply_checks(
        &self,
        state: &mut CowState,
        sites: &mut BTreeMap<CowSite, Vec<Local>>,
        site: CowSite,
        drop: bool,
    ) {
        let Some(vecs) = sites.get_mut(&site) else {
            return;
        };
        if drop {
            vecs.retain(|&v| !state.contains(&self.key(v)));
        }
        for &v in vecs.iter() {
            let k = self.key(v);
            state.retain(|other| other == &k || !other.reaches(&k));
            state.insert(k);
        }
    }

    /// Walks block `b` from `state`, answering the state at its end.
    fn walk(
        &self,
        b: usize,
        mut state: CowState,
        sites: &mut BTreeMap<CowSite, Vec<Local>>,
        drop: bool,
    ) -> CowState {
        let block = &self.body.blocks[b];
        for (i, stmt) in block.stmts.iter().enumerate() {
            self.apply_checks(&mut state, sites, CowSite { block: b, stmt: Some(i) }, drop);
            self.step_stmt(&mut state, stmt);
        }
        self.apply_checks(&mut state, sites, CowSite { block: b, stmt: None }, drop);
        self.step_term(&mut state, &block.terminator);
        state
    }

    /// The state entering block `b`: what every predecessor's end agrees on,
    /// treating a predecessor not yet reached as agreeing with anything when
    /// `optimistic`, and as knowing nothing otherwise.
    fn entering(
        &self,
        b: usize,
        out: &[Option<CowState>],
        top: &CowState,
        optimistic: bool,
    ) -> CowState {
        let mut state = if b == 0 {
            self.entry.clone()
        } else if self.preds[b].is_empty() {
            CowState::new()
        } else {
            top.clone()
        };
        for &p in &self.preds[b] {
            match &out[p] {
                Some(from) => state.retain(|k| from.contains(k)),
                None if optimistic => {}
                None => state.clear(),
            }
        }
        state
    }

    /// Strips every check of a vector known alone where it stands.
    fn drop_known(&self, sites: &mut BTreeMap<CowSite, Vec<Local>>) {
        let top: CowState = sites
            .values()
            .flatten()
            .map(|&l| self.key(l))
            .chain(self.entry.iter().cloned())
            .collect();
        let n = self.body.blocks.len();
        let mut out: Vec<Option<CowState>> = vec![None; n];
        let mut scratch = sites.clone();
        let mut changed = true;
        while changed {
            changed = false;
            for b in 0..n {
                let reached = b == 0
                    || self.preds[b].is_empty()
                    || self.preds[b].iter().any(|&p| out[p].is_some());
                if !reached {
                    continue;
                }
                let state = self.entering(b, &out, &top, true);
                let end = self.walk(b, state, &mut scratch, false);
                if out[b].as_ref() != Some(&end) {
                    out[b] = Some(end);
                    changed = true;
                }
            }
        }
        for b in 0..n {
            let state = self.entering(b, &out, &top, false);
            let _ = self.walk(b, state, sites, true);
        }
        sites.retain(|_, vecs| !vecs.is_empty());
    }

    /// Whether nothing in the loop of `blocks` changes or could share the
    /// vector at `k`.
    fn invariant(&self, blocks: &BTreeSet<usize>, k: &CowKey) -> bool {
        blocks.iter().all(|&b| {
            let mut probe: CowState = HashSet::from([k.clone()]);
            for stmt in &self.body.blocks[b].stmts {
                self.step_stmt(&mut probe, stmt);
            }
            self.step_term(&mut probe, &self.body.blocks[b].terminator);
            probe.contains(k)
        })
    }

    /// The type of a local holding the vector at `want`, for a hoisted read
    /// of it.
    fn ty_of(&self, want: &CowKey) -> Option<Ty> {
        (0..self.body.locals.len())
            .map(|i| Local(u32::try_from(i).unwrap_or(u32::MAX)))
            .find(|&l| &self.key(l) == want)
            .map(|l| self.body.locals[l.0 as usize].ty)
    }

    /// Moves each check inside a loop of a place nothing in the loop changes
    /// or could share ahead of the loop, as the reads of the place would be:
    /// ahead of the outermost loop around it that leaves the place alone.
    /// Outer places go first, so a container is checked before the elements
    /// read out of it.
    fn hoist(&self, sites: &mut BTreeMap<CowSite, Vec<Local>>) -> Vec<CowHoist> {
        let loops = crate::preempt::natural_loops(self.body);
        let mut hoists: Vec<CowHoist> = Vec::new();
        let mut candidates: Vec<(CowSite, Local, CowKey)> = sites
            .iter()
            .flat_map(|(&site, vecs)| vecs.iter().map(move |&v| (site, v)))
            .map(|(site, v)| (site, v, self.key(v)))
            .collect();
        candidates.sort_by_key(|(_, _, k)| cow_key_depth(k));
        for (site, v, k) in candidates {
            let Some(actions) = cow_hoist_actions(&k, &|want| self.ty_of(want)) else {
                continue;
            };
            let Some(entry) = self.hoist_target(&loops, sites, site, &k) else {
                continue;
            };
            if let Some(vecs) = sites.get_mut(&site) {
                vecs.retain(|&w| w != v);
            }
            match hoists.iter_mut().find(|h| h.block == entry) {
                Some(h) => {
                    for a in actions {
                        if !h.actions.contains(&a) {
                            h.actions.push(a);
                        }
                    }
                }
                None => hoists.push(CowHoist {
                    block: entry,
                    actions,
                }),
            }
        }
        sites.retain(|_, vecs| !vecs.is_empty());
        hoists
    }

    /// The block ahead of the outermost loop around `site` that leaves the
    /// place at `k` alone, which a check of it there can run at the end of.
    fn hoist_target(
        &self,
        loops: &[(usize, BTreeSet<usize>)],
        sites: &BTreeMap<CowSite, Vec<Local>>,
        site: CowSite,
        k: &CowKey,
    ) -> Option<usize> {
        let mut around: Vec<&(usize, BTreeSet<usize>)> =
            loops.iter().filter(|(_, blocks)| blocks.contains(&site.block)).collect();
        // Loops around one block nest, so the smaller lies inside the larger.
        around.sort_by_key(|(_, blocks)| blocks.len());
        let mut target = None;
        for (header, blocks) in around {
            // A check still in the loop of a place this one is read through
            // gives its elements new headers on every turn.
            let below_a_check = sites.iter().any(|(s, vecs)| {
                blocks.contains(&s.block)
                    && vecs.iter().any(|&w| {
                        let wk = self.key(w);
                        &wk != k && k.reaches(&wk)
                    })
            });
            if below_a_check || !self.invariant(blocks, k) {
                break;
            }
            if let Some(entry) = cow_loop_entry(self.body, *header, blocks) {
                target = Some(entry);
            }
        }
        target
    }
}

/// The one block outside the loop of `blocks` that enters it at `header`,
/// when it hands straight to the loop: a call ending it could share a vector
/// again after a check joins its end.
fn cow_loop_entry(body: &Body, header: usize, blocks: &BTreeSet<usize>) -> Option<usize> {
    let mut entries = body.blocks.iter().enumerate().filter(|(b, block)| {
        !blocks.contains(b) && crate::preempt::successors(&block.terminator).contains(&header)
    });
    let (entry, entering) = entries.next()?;
    if entries.next().is_some() || !matches!(entering.terminator, Terminator::Goto { .. }) {
        return None;
    }
    Some(entry)
}

/// Checks run ahead of a loop: each its own place read, then the check.
struct CowHoist {
    block: usize,
    actions: Vec<CowHoistAction>,
}

#[derive(Clone, PartialEq, Eq)]
enum CowHoistAction {
    /// Check the vector at a place, of the type given: a local, or a field
    /// path below one.
    Unshare(Place, Ty),
    /// Check element `index` of the vector at a place, a nested vector the
    /// runtime reads in bounds.
    UnshareAt(Place, Operand),
}

fn cow_key_depth(k: &CowKey) -> usize {
    match k {
        CowKey::Local(_) => 0,
        CowKey::Field(base, _) | CowKey::Elem(base, _) => 1 + cow_key_depth(base),
    }
}

/// The place reads and checks that give the vector a key names a buffer of
/// its own ahead of a loop, outermost first, when every read is one that
/// cannot fault: a field path below a local, and an element read the
/// runtime bounds-checks.
fn cow_hoist_actions(
    k: &CowKey,
    ty_of: &dyn Fn(&CowKey) -> Option<Ty>,
) -> Option<Vec<CowHoistAction>> {
    let fields_only = |proj: &[Projection]| {
        proj.iter().all(|p| matches!(p, Projection::Field(_) | Projection::Deref))
    };
    match k {
        CowKey::Local(l) => Some(vec![CowHoistAction::Unshare(Place::local(*l), ty_of(k)?)]),
        CowKey::Field(base, proj) => {
            let CowKey::Local(b) = **base else { return None };
            match proj.split_last() {
                Some((Projection::Index(i), prefix)) if fields_only(prefix) => {
                    let container = Place { local: b, projection: prefix.to_vec() };
                    let container_ty = ty_of(&CowKey::Field(base.clone(), prefix.to_vec()))?;
                    Some(vec![
                        CowHoistAction::Unshare(container.clone(), container_ty),
                        CowHoistAction::UnshareAt(container, Operand::Copy(Place::local(*i))),
                    ])
                }
                _ if fields_only(proj) => Some(vec![CowHoistAction::Unshare(
                    Place {
                        local: b,
                        projection: proj.clone(),
                    },
                    ty_of(k)?,
                )]),
                _ => None,
            }
        }
        CowKey::Elem(base, index) => {
            let container = match &**base {
                CowKey::Local(b) => Place::local(*b),
                CowKey::Field(inner, proj) => {
                    let CowKey::Local(b) = **inner else { return None };
                    if !fields_only(proj) {
                        return None;
                    }
                    Place { local: b, projection: proj.clone() }
                }
                CowKey::Elem(..) => return None,
            };
            let index = match index {
                CowIndex::Local(l) => Operand::Copy(Place::local(*l)),
                CowIndex::Const(n) => Operand::Const(ConstValue::Int(*n)),
            };
            Some(vec![
                CowHoistAction::Unshare(container.clone(), ty_of(base)?),
                CowHoistAction::UnshareAt(container, index),
            ])
        }
    }
}

fn cow_insert_unshares(body: &mut Body, tcx: &TyCtxt, sites: BTreeMap<CowSite, Vec<Local>>) {
    let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
    // Later sites first, so the indices of earlier ones stay put.
    for (site, vecs) in sites.into_iter().rev() {
        let mut uniq: Vec<Local> = Vec::new();
        for v in vecs {
            if !uniq.contains(&v) {
                uniq.push(v);
            }
        }
        let span = body.span;
        let calls: Vec<Statement> = uniq
            .into_iter()
            .map(|v| {
                let dest = Local(u32::try_from(body.locals.len()).expect("local overflow"));
                body.locals.push(LocalDecl {
                    ty: unit_ty,
                    debug_name: None,
                    mutable: false,
                    region: false,
                });
                Statement {
                    kind: StatementKind::Assign {
                        place: Place::local(dest),
                        rvalue: Rvalue::CallIntrinsic {
                            name: COW_UNSHARE,
                            args: vec![Operand::Copy(Place::local(v))],
                        },
                    },
                    span,
                    inlined: crate::ir::InlineChain::default(),
                }
            })
            .collect();
        let stmts = &mut body.blocks[site.block].stmts;
        let at = site.stmt.unwrap_or(stmts.len());
        stmts.splice(at..at, calls);
    }
}

/// Runs each hoisted check at the end of the block that enters its loop:
/// the place read into a local, then the check; an element check is a call
/// the block ends with, before it goes on to the loop.
fn cow_apply_hoists(body: &mut Body, tcx: &TyCtxt, hoists: Vec<CowHoist>) {
    let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
    let span = body.span;
    let fresh = |body: &mut Body, ty: Ty| {
        let l = Local(u32::try_from(body.locals.len()).expect("local overflow"));
        body.locals.push(LocalDecl {
            ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
        l
    };
    let stmt = |kind: StatementKind| Statement {
        kind,
        span,
        inlined: crate::ir::InlineChain::default(),
    };
    for hoist in hoists {
        let mut read: Vec<(Place, Local)> = Vec::new();
        let mut current = hoist.block;
        for action in hoist.actions {
            match action {
                CowHoistAction::Unshare(place, ty) => {
                    let v = if place.projection.is_empty() {
                        place.local
                    } else {
                        let v = fresh(body, ty);
                        body.blocks[current].stmts.push(stmt(StatementKind::Assign {
                            place: Place::local(v),
                            rvalue: Rvalue::Use(Operand::Copy(place.clone())),
                        }));
                        v
                    };
                    read.push((place, v));
                    let dest = fresh(body, unit_ty);
                    body.blocks[current].stmts.push(stmt(StatementKind::Assign {
                        place: Place::local(dest),
                        rvalue: Rvalue::CallIntrinsic {
                            name: COW_UNSHARE,
                            args: vec![Operand::Copy(Place::local(v))],
                        },
                    }));
                }
                CowHoistAction::UnshareAt(place, index) => {
                    let Some(&(_, container)) = read.iter().find(|(p, _)| *p == place) else {
                        continue;
                    };
                    let next = BlockId(u32::try_from(body.blocks.len()).expect("block overflow"));
                    let goto = std::mem::replace(
                        &mut body.blocks[current].terminator,
                        Terminator::Goto { target: next },
                    );
                    let dest = fresh(body, unit_ty);
                    body.blocks[current].terminator = Terminator::Call {
                        callee: Operand::Const(ConstValue::Str("gos_rt_vec_unshare_at".to_string())),
                        args: vec![Operand::Copy(Place::local(container)), index],
                        destination: Place::local(dest),
                        target: Some(next),
                    };
                    let block_span = body.blocks[current].span;
                    body.blocks.push(BasicBlock {
                        id: next,
                        stmts: Vec::new(),
                        terminator: goto,
                        span: block_span,
                        terminator_span: None,
                        terminator_inlined: crate::ir::InlineChain::default(),
                    });
                    current = next.0 as usize;
                }
            }
        }
    }
}

/// What a scan of a body's calls records.
struct CowNotes<'a> {
    origins: &'a mut HashMap<Local, Vec<CowOrigin>>,
    sinks: &'a mut Vec<(Local, CowSite)>,
    inline_writes: &'a mut Vec<(Local, CowSite)>,
}

/// Whether the runtime call `name` only reads what its arguments reach: a
/// query, a rendering, a copy, or a count change on the value itself. A call
/// not known to be one of these is taken to write, which costs a check and
/// never a wrong answer.
fn cow_call_only_reads(name: &str) -> bool {
    const READ_SUFFIXES: &[&str] = &[
        "_len", "_capacity", "_is_empty", "_contains", "_eq", "_first", "_last", "_peek", "_free",
        "_retain", "_release", "_clone", "_clone_field", "_keys", "_values",
    ];
    const READ_PARTS: &[&str] = &[
        "_get", "_contains_", "_index_of", "_count_of", "_binary_search", "_format", "_join",
        "_to_string", "_iter", "_slice", "_hash", "_cmp", "_mark_shared", "_desc_cmp",
    ];
    COW_ELEMENT_READS.contains(&name)
        || name == COW_UNSHARE
        || name.starts_with("gos_rt_str_")
        || name.starts_with("gos_rt_fmt")
        || name.starts_with("gos_rt_print")
        || matches!(
            name,
            "__concat" | "println" | "print" | "eprintln" | "eprint" | "format" | "gos_rt_panic_oob"
        )
        || READ_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
        || READ_PARTS.iter().any(|part| name.contains(part))
}

/// Whether `ty` is a `&mut` vector or slice.
fn cow_ty_is_mut_vec(tcx: &TyCtxt, ty: Ty) -> bool {
    matches!(
        tcx.kind_of(ty),
        TyKind::Ref { mutability: gossamer_types::Mutbl::Mut, inner }
            if matches!(tcx.kind_of(*inner), TyKind::Vec(_) | TyKind::Slice(_))
    )
}

/// Whether `ty` is a vector or slice, or a reference to one.
fn cow_ty_is_vec(tcx: &TyCtxt, ty: Ty) -> bool {
    match tcx.kind_of(ty) {
        TyKind::Vec(_) | TyKind::Slice(_) => true,
        TyKind::Ref { inner, .. } => {
            matches!(tcx.kind_of(*inner), TyKind::Vec(_) | TyKind::Slice(_))
        }
        _ => false,
    }
}

/// Whether a value of `ty` can be written in place: a container or an
/// aggregate that may hold one. Scalars and `String`s cannot.
fn cow_ty_is_writable(tcx: &TyCtxt, ty: Ty) -> bool {
    !matches!(
        tcx.kind_of(ty),
        TyKind::Int(_)
            | TyKind::Float(_)
            | TyKind::Bool
            | TyKind::Char
            | TyKind::String
            | TyKind::Never
            | TyKind::Error
    )
}

impl CowNotes<'_> {
    fn call(&mut self, name: &str, args: &[Operand], site: CowSite, dest: Option<Local>) {
        // A call not known to only read may write what any argument reaches.
        // A retain takes a nested vector out of its parent for an owner of its
        // own, which needs the header the parent's buffer holds alone, as a
        // write does.
        // A program function is judged by its summary (see `cow_scan`).
        if (!cow_call_only_reads(name) && !is_gossamer_function_symbol(name))
            || name == "gos_rt_vec_retain"
        {
            self.sinks
                .extend(args.iter().filter_map(cow_operand_local).map(|l| (l, site)));
        }
        if COW_ELEMENT_READS.contains(&name)
            && let (Some(dest), Some(container)) = (dest, args.first().and_then(cow_operand_local))
        {
            self.origins
                .entry(dest)
                .or_default()
                .push(CowOrigin::Element { container, site });
        }
        if let Some(&(_, at)) = COW_VEC_WRITERS.iter().find(|(n, _)| *n == name)
            && let Some(vec) = args.get(at).and_then(cow_operand_local)
        {
            self.sinks.push((vec, site));
        }
        if COW_INLINE_WRITERS.contains(&name)
            && let Some(vec) = args.first().and_then(cow_operand_local)
        {
            self.inline_writes.push((vec, site));
        }
    }
}

/// Calls a loop may make without sharing a vector it writes: element reads
/// and writes, lengths, and the checks the bounds proofs left.
fn cow_call_keeps_buffers(name: &str) -> bool {
    // A call that only reads shares nothing, unless what it answers is a copy
    // of a vector or a walk over one, which may share the buffer.
    let copies = name.contains("_clone")
        || name.contains("_slice")
        || name.contains("_iter")
        || name.contains("_share");
    (cow_call_only_reads(name) && !copies)
        || name == "gos_rt_len"
        || COW_INLINE_WRITERS.contains(&name)
        || COW_ELEMENT_READS.contains(&name)
        || name == COW_UNSHARE
        || matches!(
            name,
            "gos_rt_vec_push_owned"
                | "gos_rt_vec_len"
                | "gos_rt_vec_capacity"
                | "gos_rt_heap_i64_get"
                | "gos_rt_heap_i64_set"
                | "gos_rt_heap_u8_get"
                | "gos_rt_heap_u8_set"
                | "gos_rt_panic_oob"
        )
}

/// The locals holding `vec` or a value read out of it.
fn cow_derived_locals(origins: &HashMap<Local, Vec<CowOrigin>>, vec: Local) -> HashSet<Local> {
    let mut derived: HashSet<Local> = HashSet::from([vec]);
    loop {
        let before = derived.len();
        for (&local, from) in origins {
            let reaches = from.iter().any(|origin| match *origin {
                CowOrigin::Element { container, .. } => derived.contains(&container),
                CowOrigin::Copy(src) | CowOrigin::Through(src) => derived.contains(&src),
            });
            if reaches {
                derived.insert(local);
            }
        }
        if derived.len() == before {
            return derived;
        }
    }
}

/// Whether a call can share `vec`'s buffer: one through a closure value,
/// which may hold the vector, or one handed the vector or a value read out
/// of it, unless the callee is known to keep buffers as they are - a runtime
/// call that does, or a program function whose summary says it keeps no
/// share of those arguments once it returns.
fn cow_call_may_share(
    effects: &ProgramEffects,
    callee: &Operand,
    args: &[Operand],
    derived: &HashSet<Local>,
) -> bool {
    let positions: Vec<usize> = args
        .iter()
        .enumerate()
        .filter(|(_, a)| cow_operand_local(a).is_some_and(|l| derived.contains(&l)))
        .map(|(i, _)| i)
        .collect();
    let (def, symbol) = match callee {
        Operand::FnRef { def, .. } => (Some(*def), String::new()),
        Operand::Const(ConstValue::Str(name)) if cow_call_keeps_buffers(name) => return false,
        Operand::Const(ConstValue::Str(name)) if is_gossamer_function_symbol(name) => {
            (None, name.clone())
        }
        Operand::Const(_) => return !positions.is_empty(),
        Operand::Copy(_) => return true,
    };
    if positions.is_empty() {
        return false;
    }
    match effects.of_symbol(def, &symbol, args.len()) {
        Some(summary) => {
            summary.escapes || positions.iter().any(|&i| summary.param(i).retains)
        }
        None => true,
    }
}

/// [`cow_call_may_share`] for a statement's runtime call.
fn cow_intrinsic_may_share(name: &str, args: &[Operand], derived: &HashSet<Local>) -> bool {
    !cow_call_keeps_buffers(name)
        && args
            .iter()
            .filter_map(cow_operand_local)
            .any(|l| derived.contains(&l))
}

/// The site before the innermost loop around `site` where `vec` can be given
/// its own buffer once: the end of the loop's one entering block, when
/// nothing in the loop assigns `vec` or makes a call that could share it.
fn cow_hoisted_site(
    body: &Body,
    effects: &ProgramEffects,
    loops: &[crate::preempt::NaturalLoop],
    origins: &HashMap<Local, Vec<CowOrigin>>,
    vec: Local,
    site: CowSite,
) -> Option<CowSite> {
    let (header, blocks) = loops
        .iter()
        .filter(|(_, blocks)| blocks.contains(&site.block))
        .min_by_key(|(_, blocks)| blocks.len())?;
    let derived = cow_derived_locals(origins, vec);
    for &b in blocks {
        let block = &body.blocks[b];
        for stmt in &block.stmts {
            match &stmt.kind {
                StatementKind::Assign { place, rvalue } => {
                    if place.local == vec {
                        return None;
                    }
                    if let Rvalue::CallIntrinsic { name, args } = rvalue
                        && cow_intrinsic_may_share(name, args, &derived)
                    {
                        return None;
                    }
                }
                StatementKind::Nop
                | StatementKind::StorageLive(_)
                | StatementKind::StorageDead(_)
                | StatementKind::SetDiscriminant { .. } => {}
                _ => return None,
            }
        }
        if let Terminator::Call { callee, args, destination, .. } = &block.terminator
            && (destination.local == vec || cow_call_may_share(effects, callee, args, &derived))
        {
            return None;
        }
    }
    // The one block outside the loop that enters it.
    let mut entries = body.blocks.iter().enumerate().filter(|(b, block)| {
        !blocks.contains(b) && crate::preempt::successors(&block.terminator).contains(header)
    });
    let (entry, entering) = entries.next()?;
    // The call joins the end of that block, so the block must hand straight
    // to the loop: a call ending it could share the vector again.
    if entries.next().is_some() || !matches!(entering.terminator, Terminator::Goto { .. }) {
        return None;
    }
    Some(CowSite { block: entry, stmt: None })
}
