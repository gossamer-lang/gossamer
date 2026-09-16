//! Monomorphisation pass.
//!
//! Walks every [`Body`] and materialises one body per `(def, substs)` pair a
//! call site instantiates, and one per generic method a receiver's type
//! arguments instantiate. Each is lowered afresh from its HIR declaration with
//! the type parameters replaced, so every layout, comparison, and call the
//! builder chooses is the one the concrete program gets. Call sites are then
//! routed to the instantiation by its mangled name.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};

use gossamer_hir::{HirFn, HirItemKind, HirProgram};
use gossamer_lex::Span;
use gossamer_resolve::DefId;
use gossamer_types::{GenericArg, Mutbl, Substs, Ty, TyCtxt, TyKind};

use crate::ir::{
    Body, ConstValue, Local, Operand, Place, Projection, Rvalue, StatementKind, Terminator,
};
use crate::lower::instantiate::{Instantiation, map_fn_types};
use crate::lower::{ProgramTables, finish_lowered_bodies};

/// Cap on the number of fixed-point iterations the monomorphiser
/// will run before bailing. Real workloads converge in ≤ 5; the
/// generous cap guards against a runaway generic that recursively
/// produces fresh specialisations.
const MAX_MONOMORPHISE_ITERATIONS: u32 = 32;

/// The receiver convention each method body was lowered with, keyed by the
/// body's name.
///
/// A method states its own convention in its first local, and that is what
/// decides whether a call site hands over a value or an address. The call
/// site's own receiver type cannot answer it: below the checker a container,
/// a string, and a fieldless enum all travel in a slot the flat value model
/// types exactly the way it types an `i64`.
struct ReceiverConventions {
    /// Every method and associated function body, whatever its arity.
    declared: HashSet<String>,
    reference: HashMap<String, bool>,
    mut_reference: HashMap<String, bool>,
    scalar_reference: HashMap<String, bool>,
}

impl ReceiverConventions {
    /// Reads the convention off every method body in `bodies`.
    fn of(bodies: &[Body], tcx: &mut TyCtxt) -> Self {
        let mut reference = HashMap::new();
        let mut mut_reference = HashMap::new();
        let mut scalar_reference = HashMap::new();
        let declared: HashSet<String> = bodies
            .iter()
            .filter(|b| b.name.contains("::"))
            .map(|b| b.name.clone())
            .collect();
        for body in bodies
            .iter()
            .filter(|b| b.arity >= 1 && b.name.contains("::"))
        {
            let Some(recv) = body.locals.get(1) else {
                continue;
            };
            let kind = tcx.kind_of(recv.ty);
            let scalar_ref = match kind {
                TyKind::Ref { inner, .. } => matches!(
                    tcx.kind_of(*inner),
                    TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char
                ),
                _ => false,
            };
            reference.insert(body.name.clone(), matches!(kind, TyKind::Ref { .. }));
            mut_reference.insert(
                body.name.clone(),
                matches!(
                    kind,
                    TyKind::Ref {
                        mutability: Mutbl::Mut,
                        ..
                    }
                ),
            );
            scalar_reference.insert(body.name.clone(), scalar_ref);
        }
        Self {
            declared,
            reference,
            mut_reference,
            scalar_reference,
        }
    }

    /// Whether the program declares a method body under this name.
    fn declares(&self, name: &str) -> bool {
        self.declared.contains(name)
    }

    /// Whether the named method declares a reference receiver.
    fn takes_reference(&self, name: &str) -> bool {
        self.reference.get(name) == Some(&true)
    }

    /// Whether the named method declares a `&mut self` receiver.
    fn takes_mut_reference(&self, name: &str) -> bool {
        self.mut_reference.get(name) == Some(&true)
    }

    /// Whether the named method reads its receiver by loading through it,
    /// which is the case exactly when the reference names a scalar.
    fn loads_receiver(&self, name: &str) -> bool {
        self.scalar_reference.get(name) == Some(&true)
    }
}

/// Lowers a generic declaration once per instantiation.
///
/// The builder chooses how a value is compared, rendered, hashed, stored, and
/// passed from the value's type, so a body lowered against a type parameter
/// has made every one of those choices for an opaque slot. An instantiation is
/// therefore lowered from its declaration with the parameters already
/// replaced, and each choice is the one the concrete program gets.
struct Instantiator<'p> {
    program: &'p HirProgram,
    /// Every function and method declaration, under the name its body carries.
    decls: HashMap<String, (&'p HirFn, Span)>,
    /// Read off the program the first time an instantiation needs them.
    tables: Option<ProgramTables>,
}

impl<'p> Instantiator<'p> {
    fn new(program: &'p HirProgram) -> Self {
        let mut decls = HashMap::new();
        for item in &program.items {
            match &item.kind {
                HirItemKind::Fn(decl) => {
                    let name = if item.module_path.is_empty() {
                        decl.name.name.clone()
                    } else {
                        format!("{}::{}", item.module_path.join("::"), decl.name.name)
                    };
                    decls.insert(name, (decl, item.span));
                }
                HirItemKind::Impl(block) => {
                    for method in &block.methods {
                        let name = match &block.self_name {
                            Some(owner) => format!("{}::{}", owner.name, method.name.name),
                            None => method.name.name.clone(),
                        };
                        decls.insert(name, (method, item.span));
                    }
                }
                HirItemKind::Const(_)
                | HirItemKind::Static(_)
                | HirItemKind::Adt(_)
                | HirItemKind::Trait(_) => {}
            }
        }
        Self {
            program,
            decls,
            tables: None,
        }
    }

    /// Lowers the declaration behind the body `template` with each type
    /// parameter replaced by its `subst_tys` entry, naming the result `name`.
    ///
    /// The declaration keeps its own name while it is lowered, so every
    /// name-keyed table the builder reads about it answers as it did for the
    /// template.
    fn lower(
        &mut self,
        template: &str,
        name: String,
        subst_tys: &[Option<Ty>],
        tcx: &mut TyCtxt,
    ) -> Body {
        let Some(&(decl, span)) = self.decls.get(template) else {
            panic!("monomorphise: the generic body `{template}` has no declaration to instantiate");
        };
        let program = self.program;
        let tables = self
            .tables
            .get_or_insert_with(|| ProgramTables::collect(program, tcx));
        let mut decl = decl.clone();
        map_fn_types(
            &mut decl,
            &mut Substitution {
                tcx,
                subst_tys,
                tables,
            },
        );
        let Some(mut body) = tables.lower_fn(&decl, None, span, tcx) else {
            panic!("monomorphise: the generic body `{template}` has no body to instantiate");
        };
        body.name = name;
        body
    }
}

/// One instantiation's type arguments, applied to a declaration.
struct Substitution<'a> {
    tcx: &'a mut TyCtxt,
    subst_tys: &'a [Option<Ty>],
    tables: &'a ProgramTables,
}

impl Instantiation for Substitution<'_> {
    fn ty(&mut self, ty: Ty) -> Ty {
        subst_param_ty(self.tcx, ty, self.subst_tys)
    }

    fn method_owner(
        &mut self,
        template: Ty,
        concrete: Ty,
        method: &str,
    ) -> Option<gossamer_ast::Ident> {
        param_index(self.tcx, template)?;
        let owner = adt_name(self.tcx, peel_refs(self.tcx, concrete))?;
        self.tables
            .declares_method(&format!("{owner}::{method}"))
            .then(|| gossamer_ast::Ident::new(owner))
    }
}

/// `ty` with every reference layer removed.
fn peel_refs(tcx: &TyCtxt, ty: Ty) -> Ty {
    let mut ty = ty;
    while let TyKind::Ref { inner, .. } = tcx.kind_of(ty) {
        ty = *inner;
    }
    ty
}

/// What the specialisation steps build up as they go.
struct SpecialisationState<'p> {
    /// Name of every specialised body emitted so far.
    emitted: HashSet<String>,
    instantiator: Instantiator<'p>,
}

/// What a specialisation step reads besides the bodies it copies.
struct SpecialisationContext<'a> {
    /// Receiver convention of every method body.
    receivers: &'a ReceiverConventions,
    /// Lifted closure bodies typed against an enclosing body's parameters,
    /// by name, with their index in the body list.
    closures: &'a HashMap<String, usize>,
}

/// Monomorphises `bodies` by emitting one specialised copy per
/// distinct `(def, substs)` pair observed at a call site whose
/// substitution is non-empty. Monomorphic calls are untouched.
///
/// the pass is now **fixed-point**. The
/// previous implementation walked the original bodies once and
/// emitted copies after; specialisations that themselves called
/// other generics never had their inner calls specialised. We
/// loop until a pass produces no new copies - `fn map<T,U>(f:
/// fn(T)->U, xs)` calling `fn each<T>(f, xs)` now produces both
/// `map_i64_str` and `each_i64`. Cap at
/// `MAX_MONOMORPHISE_ITERATIONS` as a runaway guard.
pub fn monomorphise(program: &HirProgram, bodies: &mut Vec<Body>, tcx: &mut TyCtxt) {
    let receivers = ReceiverConventions::of(bodies, tcx);
    let first_instantiation = bodies.len();
    let mut state = SpecialisationState {
        emitted: HashSet::new(),
        instantiator: Instantiator::new(program),
    };
    let sources: HashMap<u32, usize> = bodies
        .iter()
        .enumerate()
        .filter_map(|(i, b)| b.def.map(|d| (d.local, i)))
        .collect();
    let closure_templates: HashMap<String, usize> = bodies
        .iter()
        .enumerate()
        .filter(|(_, b)| {
            b.def.is_none()
                && b.name.starts_with(gossamer_hir::LIFTED_CLOSURE_PREFIX)
                && body_has_param(b, tcx)
        })
        .map(|(i, b)| (b.name.clone(), i))
        .collect();
    let ctx = SpecialisationContext {
        receivers: &receivers,
        closures: &closure_templates,
    };
    let method_bases: HashMap<String, usize> = bodies
        .iter()
        .enumerate()
        .filter(|(_, b)| b.def.is_none() && b.name.contains("::") && body_has_param(b, tcx))
        .map(|(i, b)| (b.name.clone(), i))
        .collect();
    // Source defs whose specialisation rewrote a trait-method call on a
    // type-parameter receiver. Their templates keep an unresolved callee, so
    // once every call site routes to a copy the template is dropped below.
    let mut trait_specialised_defs: HashSet<u32> = HashSet::new();
    // Method templates whose specialisation resolved a trait call through a
    // type parameter. The template keeps the unresolved bare callee, so once
    // every call site routes to a copy the template has to go with it.
    let mut trait_specialised_methods: HashSet<String> = HashSet::new();
    let mut function_scan_start = 0;
    let mut method_scan_start = 0;
    for iteration in 0..MAX_MONOMORPHISE_ITERATIONS {
        let function_scan_end = bodies.len();
        let specialised = specialise_functions_step(
            bodies,
            &sources,
            &mut state,
            &mut trait_specialised_defs,
            &ctx,
            tcx,
            function_scan_start,
        );
        let fn_progress = !specialised.is_empty();
        bodies.extend(specialised);
        let (method_progress, method_scan_end) = specialise_methods_step(
            bodies,
            &method_bases,
            &mut state,
            &mut trait_specialised_methods,
            &ctx,
            tcx,
            method_scan_start,
        );
        if !fn_progress && !method_progress {
            // No new copies - fixed point reached.
            break;
        }
        function_scan_start = function_scan_end;
        method_scan_start = method_scan_end;
        assert!(
            iteration + 1 != MAX_MONOMORPHISE_ITERATIONS,
            "monomorphise: did not reach a fixed point in {MAX_MONOMORPHISE_ITERATIONS} iterations \
            - either there's a runaway generic that depends on its own specialisation, \
            or the cap needs to be raised after auditing the offending bodies"
        );
    }
    // Each instantiation was lowered as a new body, so it takes the ownership
    // and canonicalisation passes every body lowered with the program took.
    if bodies.len() > first_instantiation {
        finish_lowered_bodies(bodies, first_instantiation, tcx);
    }
    // Route a generic call to its specialised concrete copy. The copy's
    // locals carry the instantiation's real types, which is what lets the
    // backends pick the per-type element read, the per-type register class,
    // and the typed callable ABI; a call left pointing at the template runs
    // a body whose every local is an opaque `Param` slot, so each element
    // read is an out-of-line runtime call and each callable goes through the
    // pointer-shaped thunk. Const-only instantiations have no copy.
    for body in bodies.iter_mut() {
        route_to_specialisations(body, &state.emitted, tcx);
    }
    // Trait-specialised templates carry an unresolved trait-method call in
    // their body; every caller now routes to a copy, so drop them.
    if !trait_specialised_defs.is_empty() || !trait_specialised_methods.is_empty() {
        bodies.retain(|b| {
            b.def
                .is_none_or(|d| !trait_specialised_defs.contains(&d.local))
                && !trait_specialised_methods.contains(&b.name)
        });
    }
    // A generic method template serves no call once every call names an
    // instantiation, so it goes. One still named is a call nothing could
    // instantiate, which the reachable-template gate reports.
    let referenced = names_referenced_elsewhere(bodies);
    bodies.retain(|b| {
        b.def.is_some()
            || !b.name.contains("::")
            || referenced.contains(&b.name)
            || !body_has_param(b, tcx)
    });
    // A `&self` method reads its receiver as an address on every tier, so
    // every call to one has to hand it an address. A generic template keeps
    // serving scalar instantiations directly, and its receiver travelled as
    // the opaque slot value the parameter had; settle the convention here,
    // where every body - template and copy alike - is in its final form.
    for body in bodies.iter_mut() {
        borrow_scalar_receivers_for_ref_methods(body, &receivers, tcx);
    }
    // Resolve every local's type one last time so specialised
    // copies + originals share the resolved (no-Var) state.
    for body in bodies.iter_mut() {
        for local in &mut body.locals {
            local.ty = resolve(tcx, local.ty);
        }
    }
    // Register per-instantiation field-type tables for every generic
    // struct instantiation reachable from a (resolved) local type, so the
    // compiled tiers lay out `Wrapper<Point>` by its concrete field
    // (`Point`) instead of the declared `Param` slot.
    register_struct_instantiations(bodies, tcx);
}

/// Materialises free-function specialisations requested by newly discovered
/// bodies. Method calls use a separate name-keyed path below.
fn specialise_functions_step(
    bodies: &[Body],
    sources: &HashMap<u32, usize>,
    state: &mut SpecialisationState<'_>,
    trait_specialised_defs: &mut HashSet<u32>,
    ctx: &SpecialisationContext<'_>,
    tcx: &mut TyCtxt,
    scan_start: usize,
) -> Vec<Body> {
    let receivers = ctx.receivers;
    let mut needs: HashMap<DefId, Vec<Substs>> = HashMap::new();
    for body in &bodies[scan_start..] {
        for_each_operand(body, &mut |operand| {
            collect_from_operand(operand, tcx, &mut needs);
        });
    }
    // Sorted rather than in the map's own order: the specialisations are
    // appended to the body list, so an iteration order that varies per
    // process varies the order bodies reach the backend, and with it the
    // emitted IR and the per-body object cache keyed on it. `local` is
    // unique within a crate and is the key `sources` is already keyed on.
    let mut needed: Vec<(&DefId, &Vec<Substs>)> = needs.iter().collect();
    needed.sort_by_key(|(def, _)| def.local);
    let mut specialised = Vec::new();
    for (def, subst_list) in needed {
        let Some(src_idx) = sources.get(&def.local) else {
            continue;
        };
        for substs in subst_list {
            if substs.is_empty() || substs_are_const_only(substs) {
                continue;
            }
            let name = mangled_name(*def, substs);
            if !state.emitted.insert(name.clone()) {
                continue;
            }
            let template = &bodies[*src_idx];
            if !trait_specialised_defs.contains(&def.local)
                && calls_trait_through_parameter(template, substs, receivers, tcx)
            {
                trait_specialised_defs.insert(def.local);
            }
            let subst_tys = subst_type_arguments(substs);
            let mut copy = state
                .instantiator
                .lower(&template.name, name, &subst_tys, tcx);
            rewrite_trait_method_calls(&mut copy, substs, receivers, tcx);
            let mut closures = Vec::new();
            specialise_lifted_closures(&mut copy, substs, ctx, state, tcx, &mut closures);
            specialised.push(copy);
            specialised.extend(closures);
        }
    }
    specialised
}

/// Gives a specialised copy its own copy of every lifted closure it names.
///
/// A closure written inside a generic body is lifted to a top-level body
/// before MIR lowering, typed against the enclosing body's own parameters, so
/// one lifted body would otherwise serve every instantiation with its
/// captured values and locals left as opaque parameter slots. Each
/// instantiation of the enclosing body instead names a copy of the closure
/// under the same substitution, and a closure nested in that closure is
/// reached through the copy in turn. `out` receives every closure copy made.
fn specialise_lifted_closures(
    copy: &mut Body,
    substs: &Substs,
    ctx: &SpecialisationContext<'_>,
    state: &mut SpecialisationState<'_>,
    tcx: &mut TyCtxt,
    out: &mut Vec<Body>,
) {
    let mut renames: Vec<(String, String)> = Vec::new();
    for_each_operand(copy, &mut |operand| {
        if let Operand::Const(ConstValue::Str(name)) = operand
            && ctx.closures.contains_key(name)
            && !renames.iter().any(|(from, _)| from == name)
        {
            renames.push((name.clone(), method_mangled_name(name, substs)));
        }
    });
    if renames.is_empty() {
        return;
    }
    for_each_operand_mut(copy, &mut |operand| {
        if let Operand::Const(ConstValue::Str(name)) = operand
            && let Some((_, to)) = renames.iter().find(|(from, _)| from == name)
        {
            name.clone_from(to);
        }
    });
    let subst_tys = subst_type_arguments(substs);
    for (from, to) in renames {
        if !state.emitted.insert(to.clone()) {
            continue;
        }
        let mut closure = state.instantiator.lower(&from, to, &subst_tys, tcx);
        rewrite_trait_method_calls(&mut closure, substs, ctx.receivers, tcx);
        specialise_lifted_closures(&mut closure, substs, ctx, state, tcx, out);
        out.push(closure);
    }
}

/// Borrows a receiver whose concrete type turned out to be a scalar for a
/// method whose impl declares `&self`.
///
/// A type parameter is one opaque slot to the template, so the receiver
/// travels by value. When the parameter resolves to a struct the slot already
/// holds the address; when it resolves to a scalar the slot holds the value,
/// and the impl - which declares a reference - reads it as an address. The
/// declared convention is read off the callee's own body, which states the
/// receiver type it was lowered with.
fn borrow_scalar_receivers_for_ref_methods(
    copy: &mut Body,
    receivers: &ReceiverConventions,
    tcx: &mut TyCtxt,
) {
    let local_tys: Vec<Ty> = copy.locals.iter().map(|l| l.ty).collect();
    let mut work: Vec<(usize, Local, Ty, bool)> = Vec::new();
    for (block_index, block) in copy.blocks.iter().enumerate() {
        let Terminator::Call { callee, args, .. } = &block.terminator else {
            continue;
        };
        let Operand::Const(ConstValue::Str(name)) = callee else {
            continue;
        };
        if !receivers.takes_reference(name) {
            continue;
        }
        let Some(Operand::Copy(recv)) = args.first() else {
            continue;
        };
        if !recv.projection.is_empty() {
            continue;
        }
        let Some(recv_ty) = local_tys.get(recv.local.0 as usize).copied() else {
            continue;
        };
        // A receiver that reaches the call as a value, where the callee reads
        // one through a reference. A type parameter is one: the lowering could
        // not choose a convention for its slot because the concrete type was
        // not yet known. A scalar is the other, but only where the callee
        // itself declares a reference to a scalar - the callee's own body is
        // what decides whether it loads, and an enum's discriminant travels in
        // a slot the flat model types the way it types a scalar.
        // A receiver already borrowed at the call site needs nothing more.
        if matches!(tcx.kind_of(recv_ty), TyKind::Ref { .. }) {
            continue;
        }
        let param_receiver = matches!(tcx.kind_of(recv_ty), TyKind::Param { .. });
        let callee_loads = receivers.loads_receiver(name);
        // A `&mut self` callee writes through the reference, and the
        // specialised body's own drop schedule reads the borrow as the
        // ownership it is. Handing the value over instead leaves the frame
        // with no claim on what its argument's heap fields name.
        let callee_writes = receivers.takes_mut_reference(name);
        if !param_receiver && !callee_loads && !callee_writes {
            continue;
        }
        work.push((block_index, recv.local, recv_ty, callee_writes));
    }
    for (block_index, recv_local, recv_ty, mutable) in work {
        let ref_ty = tcx.intern(TyKind::Ref {
            mutability: if mutable { Mutbl::Mut } else { Mutbl::Not },
            inner: recv_ty,
        });
        let tmp = Local(u32::try_from(copy.locals.len()).expect("local index fits"));
        copy.locals.push(crate::ir::LocalDecl {
            ty: ref_ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
        let span = copy.span;
        let block = &mut copy.blocks[block_index];
        block.stmts.push(crate::ir::Statement {
            kind: StatementKind::Assign {
                place: Place::local(tmp),
                rvalue: Rvalue::Ref {
                    mutable,
                    place: Place::local(recv_local),
                },
            },
            span,
            inlined: None,
        });
        if let Terminator::Call { args, .. } = &mut block.terminator
            && let Some(first) = args.first_mut()
        {
            *first = Operand::Copy(Place::local(tmp));
        }
    }
}

fn substs_are_const_only(substs: &Substs) -> bool {
    // A const-generic array parameter is lowered to a runtime-length sequence,
    // so one body serves every const value and a specialised copy is wasted.
    substs
        .as_slice()
        .iter()
        .all(|arg| matches!(arg, GenericArg::Const(_) | GenericArg::ConstParam(_)))
}

pub(crate) fn subst_type_arguments(substs: &Substs) -> Vec<Option<Ty>> {
    substs
        .as_slice()
        .iter()
        .map(|arg| match arg {
            GenericArg::Type(ty) => Some(*ty),
            GenericArg::Const(_) | GenericArg::ConstParam(_) => None,
        })
        .collect()
}

/// Every call to a generic method template in `bodies[scan]`, with the
/// substitution the call instantiates it with: the block it sits in, the
/// template's index, and the method's name.
///
/// A receiver whose type is a concrete instantiation supplies the leading
/// parameters, and a method's own parameters are read off the arguments and
/// the call's destination. A call whose receiver says nothing - a method with
/// its own type parameters on a type that has none, `impl Cmd { fn arg<T:
/// Arg>(self, v: T) }` - is instantiated from the arguments and destination
/// alone, exactly as a generic free function's call is.
fn method_call_instantiations(
    bodies: &[Body],
    method_bases: &HashMap<String, usize>,
    scan: std::ops::Range<usize>,
    tcx: &TyCtxt,
) -> Vec<(usize, usize, usize, Substs, String)> {
    let mut found = Vec::new();
    for (bi, body) in bodies.iter().enumerate().take(scan.end).skip(scan.start) {
        for (blk, block) in body.blocks.iter().enumerate() {
            let Terminator::Call {
                callee: Operand::Const(ConstValue::Str(name)),
                args,
                destination,
                ..
            } = &block.terminator
            else {
                continue;
            };
            let Some(&base_idx) = method_bases.get(name) else {
                continue;
            };
            let template = &bodies[base_idx];
            let receiver_substs = args
                .first()
                .and_then(|arg| match arg {
                    Operand::Copy(place) => body.locals.get(place.local.0 as usize),
                    Operand::Const(_) | Operand::FnRef { .. } => None,
                })
                .and_then(|decl| match tcx.kind_of(peel_ref(tcx, decl.ty)) {
                    TyKind::Adt { substs, .. }
                        if !substs.is_empty()
                            && !substs.types().iter().any(|t| ty_contains_param(tcx, *t)) =>
                    {
                        Some(substs.clone())
                    }
                    _ => None,
                });
            let substs = match receiver_substs {
                Some(base) => complete_method_substs(template, body, args, destination, &base, tcx)
                    .or_else(|| {
                        call_site_method_substs(template, body, args, destination, &[], tcx)
                    }),
                None => call_site_method_substs(template, body, args, destination, &[], tcx),
            };
            if let Some(substs) = substs {
                found.push((bi, blk, base_idx, substs, name.clone()));
            }
        }
    }
    found
}

/// One fixed-point round of generic-method specialisation. Methods are
/// dispatched by name (`Const(Str("Wrapper::get"))`, no `DefId`), so they
/// never enter the `FnRef`-keyed function path and their `self: &Wrapper<T>`
/// / `-> T` stay `Param` - which codegen renders as an opaque `ptr` slot,
/// mismatching the caller for non-pointer / aggregate `T`. For each call to a
/// generic method whose receiver (the `self` argument's local type) is a
/// concrete struct instantiation, materialise a per-instantiation copy with
/// the concrete types substituted in and route the call to it - the same
/// shape as free-function monomorphisation, keyed by name. Returns `true`
/// when at least one new copy was created.
fn specialise_methods_step(
    bodies: &mut Vec<Body>,
    method_bases: &HashMap<String, usize>,
    state: &mut SpecialisationState<'_>,
    trait_specialised_methods: &mut HashSet<String>,
    ctx: &SpecialisationContext<'_>,
    tcx: &mut TyCtxt,
    scan_start: usize,
) -> (bool, usize) {
    let receivers = ctx.receivers;
    if method_bases.is_empty() {
        return (false, bodies.len());
    }
    let mut rewrites: Vec<(usize, usize, String)> = Vec::new();
    let mut to_create: Vec<(usize, Substs, String, String)> = Vec::new();
    let scan_end = bodies.len();
    for (bi, blk, base_idx, substs, name) in
        method_call_instantiations(bodies, method_bases, scan_start..scan_end, tcx)
    {
        let spec_name = method_mangled_name(&name, &substs);
        rewrites.push((bi, blk, spec_name.clone()));
        if state.emitted.insert(spec_name.clone()) {
            to_create.push((base_idx, substs, spec_name, name));
        }
    }
    // A generic type's rendering methods are reached by a formatter that
    // walks a container's elements, not by a call, so every instance a body
    // holds gets its own.
    for body in &bodies[scan_start..scan_end] {
        for (base_name, substs) in rendering_instantiations(body, method_bases, tcx) {
            let spec_name = method_mangled_name(&base_name, &substs);
            if let Some(&base_idx) = method_bases.get(&base_name)
                && state.emitted.insert(spec_name.clone())
            {
                to_create.push((base_idx, substs, spec_name, base_name));
            }
        }
    }
    let made = !to_create.is_empty();
    for (base_idx, substs, spec_name, base_name) in to_create {
        // A method on a bounded `impl<T: Trait>` block calls the trait method
        // through its type parameter, which leaves the template with a callee
        // only an instantiation resolves.
        if !trait_specialised_methods.contains(&base_name)
            && calls_trait_through_parameter(&bodies[base_idx], &substs, receivers, tcx)
        {
            trait_specialised_methods.insert(base_name.clone());
        }
        let subst_tys = subst_type_arguments(&substs);
        let mut copy = state
            .instantiator
            .lower(&base_name, spec_name, &subst_tys, tcx);
        rewrite_trait_method_calls(&mut copy, &substs, receivers, tcx);
        let mut closures = Vec::new();
        specialise_lifted_closures(&mut copy, &substs, ctx, state, tcx, &mut closures);
        bodies.push(copy);
        bodies.extend(closures);
    }
    for (bi, blk, spec_name) in rewrites {
        if let Terminator::Call { callee, .. } = &mut bodies[bi].blocks[blk].terminator {
            *callee = Operand::Const(ConstValue::Str(spec_name));
        }
    }
    (made, scan_end)
}

/// Walks every body's local types and registers a per-instantiation field
/// table for each generic struct instantiation `Adt { def, substs }` whose
/// `substs` are concrete (no rigid `Param`). Recurses through the
/// substituted field types so a nested instantiation (`Outer<Inner<T>>`)
/// is registered too.
pub(crate) fn register_struct_instantiations(bodies: &[Body], tcx: &mut TyCtxt) {
    let mut done: HashSet<(DefId, Substs)> = HashSet::new();
    let mut stack: Vec<Ty> = Vec::new();
    for body in bodies {
        for local in &body.locals {
            stack.push(local.ty);
        }
    }
    while let Some(ty) = stack.pop() {
        match tcx.kind_of(ty).clone() {
            TyKind::Adt { def, substs } if !substs.is_empty() => {
                for t in substs.types() {
                    stack.push(t);
                }
                if substs.types().iter().any(|t| ty_contains_param(tcx, *t)) {
                    continue;
                }
                if !done.insert((def, substs.clone())) {
                    continue;
                }
                let Some(decl) = tcx.struct_field_tys(def).map(<[Ty]>::to_vec) else {
                    continue;
                };
                let subst_tys: Vec<Option<Ty>> = substs
                    .as_slice()
                    .iter()
                    .map(|a| match a {
                        GenericArg::Type(t) => Some(*t),
                        GenericArg::Const(_) | GenericArg::ConstParam(_) => None,
                    })
                    .collect();
                let inst: Vec<Ty> = decl
                    .iter()
                    .map(|&f| subst_param_ty(tcx, f, &subst_tys))
                    .collect();
                for f in &inst {
                    stack.push(*f);
                }
                tcx.register_struct_fields_inst(def, substs, inst);
            }
            TyKind::Ref { inner, .. }
            | TyKind::Vec(inner)
            | TyKind::Slice(inner)
            | TyKind::Iterator(inner)
            | TyKind::Range(inner)
            | TyKind::Sender(inner)
            | TyKind::Receiver(inner)
            | TyKind::JoinHandle(inner) => stack.push(inner),
            TyKind::Array { elem, .. } => stack.push(elem),
            TyKind::Tuple(elems) => stack.extend(elems),
            TyKind::HashMap { key, value, .. } => {
                stack.push(key);
                stack.push(value);
            }
            TyKind::FnPtr(sig) | TyKind::FnTrait(sig) => {
                stack.extend(sig.inputs);
                stack.push(sig.output);
            }
            _ => {}
        }
    }
}

/// `true` if `ty` mentions a generic `Param` anywhere in its structure.
fn ty_contains_param(tcx: &TyCtxt, ty: Ty) -> bool {
    match tcx.kind_of(ty) {
        TyKind::Param { .. } => true,
        TyKind::Ref { inner, .. }
        | TyKind::Vec(inner)
        | TyKind::Slice(inner)
        | TyKind::Sender(inner)
        | TyKind::Receiver(inner)
        | TyKind::JoinHandle(inner) => ty_contains_param(tcx, *inner),
        TyKind::Array { elem, .. } => ty_contains_param(tcx, *elem),
        TyKind::Tuple(elems) => elems.iter().any(|t| ty_contains_param(tcx, *t)),
        TyKind::HashMap { key, value, .. } => {
            ty_contains_param(tcx, *key) || ty_contains_param(tcx, *value)
        }
        TyKind::Adt { substs, .. } | TyKind::Alias { substs, .. } => {
            substs.types().iter().any(|t| ty_contains_param(tcx, *t))
        }
        TyKind::Iterator(inner) => ty_contains_param(tcx, *inner),
        TyKind::FnPtr(sig) | TyKind::FnTrait(sig) => {
            sig.inputs.iter().any(|t| ty_contains_param(tcx, *t))
                || ty_contains_param(tcx, sig.output)
        }
        _ => false,
    }
}

/// Binds every type parameter `template` names to the type `actual` holds in
/// the same position, keeping the first binding each parameter receives.
///
/// A parameter can sit anywhere inside a signature - `Fn() -> T`, `Vec<T>`,
/// `Option<T>` - so the call site's instantiation is read structurally rather
/// than only off a parameter declared as a bare `T`. A position whose actual
/// type is not concrete binds nothing.
pub fn bind_template_params(
    tcx: &TyCtxt,
    template: Ty,
    actual: Ty,
    resolved: &mut Vec<Option<Ty>>,
) {
    let template_kind = tcx.kind_of(template);
    let actual_kind = tcx.kind_of(actual);
    match (template_kind, actual_kind) {
        (TyKind::Param { idx, .. }, _) => {
            if ty_contains_param(tcx, actual)
                || matches!(actual_kind, TyKind::Var(_) | TyKind::Error)
            {
                return;
            }
            let slot = idx.0 as usize;
            if resolved.len() <= slot {
                resolved.resize(slot + 1, None);
            }
            resolved[slot].get_or_insert(actual);
        }
        (TyKind::Ref { inner: t, .. }, TyKind::Ref { inner: a, .. }) => {
            bind_template_params(tcx, *t, *a, resolved);
        }
        (TyKind::Ref { inner: t, .. }, _) => bind_template_params(tcx, *t, actual, resolved),
        (_, TyKind::Ref { inner: a, .. }) => bind_template_params(tcx, template, *a, resolved),
        (
            TyKind::Vec(t)
            | TyKind::Slice(t)
            | TyKind::Iterator(t)
            | TyKind::Sender(t)
            | TyKind::Receiver(t)
            | TyKind::JoinHandle(t),
            TyKind::Vec(a)
            | TyKind::Slice(a)
            | TyKind::Iterator(a)
            | TyKind::Sender(a)
            | TyKind::Receiver(a)
            | TyKind::JoinHandle(a),
        )
        | (TyKind::Array { elem: t, .. }, TyKind::Array { elem: a, .. }) => {
            bind_template_params(tcx, *t, *a, resolved);
        }
        (TyKind::Tuple(ts), TyKind::Tuple(actuals)) if ts.len() == actuals.len() => {
            for (t, a) in ts.iter().zip(actuals.iter()) {
                bind_template_params(tcx, *t, *a, resolved);
            }
        }
        (
            TyKind::HashMap {
                key: tk, value: tv, ..
            },
            TyKind::HashMap {
                key: ak, value: av, ..
            },
        ) => {
            bind_template_params(tcx, *tk, *ak, resolved);
            bind_template_params(tcx, *tv, *av, resolved);
        }
        (
            TyKind::Adt {
                def: td,
                substs: ts,
            },
            TyKind::Adt {
                def: ad,
                substs: actuals,
            },
        ) if td == ad => {
            for (t, a) in ts.types().iter().zip(actuals.types().iter()) {
                bind_template_params(tcx, *t, *a, resolved);
            }
        }
        (
            TyKind::FnPtr(ts) | TyKind::FnTrait(ts),
            TyKind::FnPtr(actuals) | TyKind::FnTrait(actuals),
        ) if ts.inputs.len() == actuals.inputs.len() => {
            for (t, a) in ts.inputs.iter().zip(actuals.inputs.iter()) {
                bind_template_params(tcx, *t, *a, resolved);
            }
            bind_template_params(tcx, ts.output, actuals.output, resolved);
        }
        _ => {}
    }
}

/// One past the highest type-parameter index any local of `body` names.
fn param_count(tcx: &TyCtxt, body: &Body) -> usize {
    fn visit(tcx: &TyCtxt, ty: Ty, highest: &mut usize) {
        match tcx.kind_of(ty) {
            TyKind::Param { idx, .. } => *highest = (*highest).max(idx.0 as usize + 1),
            TyKind::Ref { inner, .. }
            | TyKind::Vec(inner)
            | TyKind::Slice(inner)
            | TyKind::Iterator(inner)
            | TyKind::Sender(inner)
            | TyKind::Receiver(inner)
            | TyKind::JoinHandle(inner)
            | TyKind::Array { elem: inner, .. } => visit(tcx, *inner, highest),
            TyKind::Tuple(elems) => elems.iter().for_each(|t| visit(tcx, *t, highest)),
            TyKind::HashMap { key, value, .. } => {
                visit(tcx, *key, highest);
                visit(tcx, *value, highest);
            }
            TyKind::Adt { substs, .. } | TyKind::Alias { substs, .. } => {
                substs.types().iter().for_each(|t| visit(tcx, *t, highest));
            }
            TyKind::FnPtr(sig) | TyKind::FnTrait(sig) => {
                sig.inputs.iter().for_each(|t| visit(tcx, *t, highest));
                visit(tcx, sig.output, highest);
            }
            _ => {}
        }
    }
    let mut highest = 0;
    for local in &body.locals {
        visit(tcx, local.ty, &mut highest);
    }
    highest
}

/// Reads a method's type parameters off one call site: the receiver's own
/// substitution fills the leading positions, and each argument and the call's
/// destination are matched structurally against the template's parameter and
/// return locals. `None` when some parameter the body names stays unbound, so
/// the call keeps the template.
fn call_site_method_substs(
    template: &Body,
    caller: &Body,
    args: &[Operand],
    destination: &Place,
    base: &[Ty],
    tcx: &TyCtxt,
) -> Option<Substs> {
    let mut resolved: Vec<Option<Ty>> = base.iter().map(|t| Some(*t)).collect();
    let caller_tys: Vec<Ty> = caller.locals.iter().map(|l| l.ty).collect();
    for (index, arg) in args.iter().enumerate() {
        let Some(decl) = template.locals.get(index + 1) else {
            break;
        };
        if !ty_contains_param(tcx, decl.ty) {
            continue;
        }
        // A constant or a function item carries no local type to read; the
        // other positions and the destination still can.
        let Operand::Copy(place) = arg else {
            continue;
        };
        let Some(actual) = place_ty(tcx, &caller_tys, place) else {
            continue;
        };
        bind_template_params(tcx, decl.ty, actual, &mut resolved);
    }
    if let (Some(ret), Some(dest_ty)) = (
        template.locals.first(),
        place_ty(tcx, &caller_tys, destination),
    ) {
        bind_template_params(tcx, ret.ty, dest_ty, &mut resolved);
    }
    let count = param_count(tcx, template).max(base.len());
    if count == 0 {
        return None;
    }
    resolved.resize(count, None);
    let types: Option<Vec<Ty>> = resolved.into_iter().collect();
    Some(Substs::from_types(types?))
}

/// `true` if any of `body`'s locals carry a generic `Param`, marking it a
/// generic template (a method on a generic struct, or a generic function)
/// that needs a per-instantiation copy before codegen.
fn body_has_param(body: &Body, tcx: &TyCtxt) -> bool {
    body.locals.iter().any(|l| ty_contains_param(tcx, l.ty))
}

/// Peels a single layer of `&T` / `&mut T`, returning the pointee. A method
/// receiver is `&self`, so the receiver's struct type sits one reference
/// deep; values passed by value (a small aggregate) are returned unchanged.
fn peel_ref(tcx: &TyCtxt, ty: Ty) -> Ty {
    match tcx.kind_of(ty) {
        TyKind::Ref { inner, .. } => *inner,
        _ => ty,
    }
}

/// Extends `base` (the receiver's own substitution) with the method's own
/// type parameters, read off the call site by [`call_site_method_substs`].
/// Answers `base` unchanged when the method declares none, and `None` when a
/// parameter the body uses cannot be resolved, so the call keeps the template.
fn complete_method_substs(
    template: &Body,
    caller: &Body,
    args: &[Operand],
    destination: &Place,
    base: &Substs,
    tcx: &TyCtxt,
) -> Option<Substs> {
    // A method that declares no parameters of its own is instantiated by the
    // receiver alone, const arguments included.
    if param_count(tcx, template) <= base.len() {
        return Some(base.clone());
    }
    call_site_method_substs(template, caller, args, destination, &base.types(), tcx)
}

/// Mangled name of a generic method instantiation. Methods carry no `DefId`,
/// so the name keys the specialisation: the base `Type::method` name plus the
/// interned id of each concrete type argument (equal types share an id, so a
/// call site and the materialised copy agree).
#[must_use]
pub fn method_mangled_name(base: &str, substs: &Substs) -> String {
    let mut out = format!("{base}$mono$");
    for (i, arg) in substs.as_slice().iter().enumerate() {
        if i > 0 {
            out.push('_');
        }
        match arg {
            GenericArg::Type(ty) => {
                out.push('t');
                out.push_str(&ty.as_u32().to_string());
            }
            GenericArg::Const(c) => {
                out.push('c');
                out.push_str(&c.to_string());
            }
            GenericArg::ConstParam(idx) => {
                out.push('p');
                out.push_str(&idx.0.to_string());
            }
        }
    }
    out
}

/// Name prefix of a callee that reaches a trait function through a type
/// parameter. The `__gos_` prefix is reserved for compiler-generated names, so
/// no program item can spell it.
const PARAM_ASSOC_PREFIX: &str = "__gos_param_assoc#";

/// Callee spelling of the trait function `function` reached through the type
/// parameter `param` (`T::zero`), which monomorphisation resolves per
/// instantiation. `None` when `param` is not a type parameter.
pub(crate) fn param_assoc_callee(tcx: &TyCtxt, param: Ty, function: &str) -> Option<String> {
    let TyKind::Param { idx, .. } = tcx.kind_of(param) else {
        return None;
    };
    Some(format!("{PARAM_ASSOC_PREFIX}{}::{function}", idx.0))
}

/// The parameter position and function name a [`param_assoc_callee`] names.
fn parse_param_assoc_callee(name: &str) -> Option<(usize, &str)> {
    let (index, function) = name.strip_prefix(PARAM_ASSOC_PREFIX)?.split_once("::")?;
    Some((index.parse().ok()?, function))
}

/// Whether `template` reaches a trait function through a type parameter, which
/// only an instantiation can resolve.
fn calls_trait_through_parameter(
    template: &Body,
    substs: &Substs,
    receivers: &ReceiverConventions,
    tcx: &TyCtxt,
) -> bool {
    rewrite_trait_method_calls(&mut template.clone(), substs, receivers, tcx)
}

/// Static trait dispatch for a monomorphised generic body: a method
/// call on a type-parameter receiver (`x.describe()` where `x: &T`)
/// lowers to a bare `describe` callee the compiled tiers cannot link.
/// For this instantiation the receiver's parameter resolves to a concrete
/// type via `substs`, so rewrite the callee to that type's impl symbol
/// (`Dog::describe`), which already exists as a real function. The trait
/// bound checked at the call site guarantees the impl is present.
fn rewrite_trait_method_calls(
    copy: &mut Body,
    substs: &Substs,
    receivers: &ReceiverConventions,
    tcx: &TyCtxt,
) -> bool {
    let subst_tys: Vec<Option<Ty>> = substs
        .as_slice()
        .iter()
        .map(|a| match a {
            GenericArg::Type(t) => Some(*t),
            GenericArg::Const(_) | GenericArg::ConstParam(_) => None,
        })
        .collect();
    let local_tys: Vec<Ty> = copy.locals.iter().map(|l| l.ty).collect();
    let mut rewrote = false;
    // A trait function reached through a type parameter (`T::zero()`) names
    // the parameter's position; this instantiation says which impl that is.
    // It is resolved wherever it appears, as a callee or as a function value.
    for_each_operand_mut(copy, &mut |operand| {
        if let Operand::Const(ConstValue::Str(name)) = operand
            && let Some((index, function)) = parse_param_assoc_callee(name)
            && let Some(Some(concrete)) = subst_tys.get(index)
            && let Some(owner) = adt_name(tcx, *concrete)
        {
            *name = format!("{owner}::{function}");
            rewrote = true;
        }
    });
    for block in &mut copy.blocks {
        let Terminator::Call { callee, args, .. } = &mut block.terminator else {
            continue;
        };
        let Operand::Const(ConstValue::Str(name)) = callee else {
            continue;
        };
        if name.contains("::") {
            continue;
        }
        let Some(Operand::Copy(recv)) = args.first() else {
            continue;
        };
        let Some(recv_ty) = place_ty(tcx, &local_tys, recv) else {
            continue;
        };
        let Some(idx) = param_index(tcx, recv_ty) else {
            continue;
        };
        let Some(Some(concrete)) = subst_tys.get(idx) else {
            continue;
        };
        if let Some(cname) = adt_name(tcx, *concrete) {
            let resolved = format!("{cname}::{name}");
            // A primitive's trait surface is mostly builtin - `__debug` and
            // friends have no body of their own - so name one only when the
            // program actually declares it. A declared type keeps resolving
            // by name, which is how its derived methods are reached.
            let primitive_target = !matches!(tcx.kind_of(*concrete), TyKind::Adt { .. });
            if !primitive_target || receivers.declares(&resolved) {
                *callee = Operand::Const(ConstValue::Str(resolved));
                rewrote = true;
            }
        }
    }
    rewrote
}

/// Type of the value `place` denotes, walking its projection chain from the
/// root local's declared type. A receiver reached through a field - the shape
/// `self.value.method()` produces inside a generic `impl` block - carries its
/// type parameter on the projected field rather than on the local, so the
/// trait-dispatch rewrite has to resolve the whole chain to find it.
/// Returns `None` for any step whose type is not statically resolvable here.
fn place_ty(tcx: &TyCtxt, local_tys: &[Ty], place: &Place) -> Option<Ty> {
    let mut ty = local_tys.get(place.local.0 as usize).copied()?;
    for step in &place.projection {
        ty = match step {
            Projection::Deref => match tcx.kind_of(ty) {
                TyKind::Ref { inner, .. } => *inner,
                _ => return None,
            },
            Projection::Field(index) => {
                let mut base = ty;
                while let TyKind::Ref { inner, .. } = tcx.kind_of(base) {
                    base = *inner;
                }
                match tcx.kind_of(base) {
                    TyKind::Adt { def, substs } => *tcx
                        .adt_field_tys(*def, substs)
                        .and_then(|fields| fields.get(*index as usize))?,
                    TyKind::Tuple(elems) => *elems.get(*index as usize)?,
                    _ => return None,
                }
            }
            Projection::Index(_) | Projection::Downcast(_) | Projection::Discriminant => {
                return None;
            }
        };
    }
    Some(ty)
}

/// Generic-parameter index of a receiver type (`&T` / `T`), or `None`.
fn param_index(tcx: &TyCtxt, ty: Ty) -> Option<usize> {
    let mut t = ty;
    while let TyKind::Ref { inner, .. } = tcx.kind_of(t).clone() {
        t = inner;
    }
    match tcx.kind_of(t) {
        TyKind::Param { idx, .. } => Some(idx.0 as usize),
        _ => None,
    }
}

/// The name an `impl` block for `ty` registers its methods under; see
/// [`gossamer_types::printer::impl_owner_name`].
fn adt_name(tcx: &TyCtxt, ty: Ty) -> Option<String> {
    gossamer_types::printer::impl_owner_name(tcx, ty)
}

/// The rendering methods (`fmt`, `to_string`) of every generic type instance a
/// local of `body` holds, at any depth, that the program declares, with the
/// instance's type arguments.
fn rendering_instantiations(
    body: &Body,
    method_bases: &HashMap<String, usize>,
    tcx: &TyCtxt,
) -> Vec<(String, Substs)> {
    fn visit(
        tcx: &TyCtxt,
        ty: Ty,
        method_bases: &HashMap<String, usize>,
        seen: &mut HashSet<Ty>,
        out: &mut Vec<(String, Substs)>,
    ) {
        if !seen.insert(ty) {
            return;
        }
        match tcx.kind_of(ty) {
            TyKind::Ref { inner, .. }
            | TyKind::Vec(inner)
            | TyKind::Slice(inner)
            | TyKind::Iterator(inner)
            | TyKind::Sender(inner)
            | TyKind::Receiver(inner)
            | TyKind::JoinHandle(inner)
            | TyKind::Array { elem: inner, .. } => visit(tcx, *inner, method_bases, seen, out),
            TyKind::Tuple(elems) => {
                for elem in elems {
                    visit(tcx, *elem, method_bases, seen, out);
                }
            }
            TyKind::HashMap { key, value, .. } => {
                visit(tcx, *key, method_bases, seen, out);
                visit(tcx, *value, method_bases, seen, out);
            }
            TyKind::Adt { substs, .. } => {
                for arg in substs.types() {
                    visit(tcx, arg, method_bases, seen, out);
                }
                if substs.types().is_empty()
                    || substs.types().iter().any(|t| ty_contains_param(tcx, *t))
                {
                    return;
                }
                let Some(owner) = adt_name(tcx, ty) else {
                    return;
                };
                for method in ["fmt", "to_string"] {
                    let base = format!("{owner}::{method}");
                    if method_bases.contains_key(&base) {
                        out.push((base, substs.clone()));
                    }
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for local in &body.locals {
        visit(tcx, local.ty, method_bases, &mut seen, &mut out);
    }
    out
}

/// Every body name some other body spells as an operand: a callee, a function
/// address, or a value handed along.
fn names_referenced_elsewhere(bodies: &[Body]) -> HashSet<String> {
    let mut names = HashSet::new();
    for body in bodies {
        for_each_operand(body, &mut |operand| {
            if let Operand::Const(ConstValue::Str(name)) = operand
                && *name != body.name
            {
                names.insert(name.clone());
            }
        });
    }
    names
}

/// Visits every operand `body` holds, in statements and terminators alike.
///
/// Exhaustive on purpose: a function value can reach any operand position -
/// a call argument, an aggregate field, an intrinsic argument - and one this
/// walk skipped would keep naming the template.
fn for_each_operand(body: &Body, f: &mut impl FnMut(&Operand)) {
    for block in &body.blocks {
        for statement in &block.stmts {
            match &statement.kind {
                StatementKind::Assign { rvalue, .. } => match rvalue {
                    Rvalue::Use(operand)
                    | Rvalue::UnaryOp { operand, .. }
                    | Rvalue::Cast { operand, .. }
                    | Rvalue::Repeat { value: operand, .. } => f(operand),
                    Rvalue::BinaryOp { lhs, rhs, .. } => {
                        f(lhs);
                        f(rhs);
                    }
                    Rvalue::Aggregate { operands, .. }
                    | Rvalue::CallIntrinsic { args: operands, .. } => {
                        operands.iter().for_each(&mut *f);
                    }
                    Rvalue::Len(_) | Rvalue::Ref { .. } | Rvalue::StaticLoad(_) => {}
                },
                StatementKind::StaticStore { value, .. } => f(value),
                StatementKind::IterSource { source, .. } => f(source),
                StatementKind::IterAdapter {
                    closure_or_arg: Some(operand),
                    ..
                } => f(operand),
                StatementKind::IterAdapter { .. }
                | StatementKind::StorageLive(_)
                | StatementKind::StorageDead(_)
                | StatementKind::SetDiscriminant { .. }
                | StatementKind::IterNext { .. }
                | StatementKind::Nop => {}
            }
        }
        match &block.terminator {
            Terminator::Call { callee, args, .. } => {
                f(callee);
                args.iter().for_each(&mut *f);
            }
            Terminator::SwitchInt { discriminant, .. } => f(discriminant),
            Terminator::Assert { cond, msg, .. } => {
                f(cond);
                msg.operands().for_each(&mut *f);
            }
            Terminator::Goto { .. }
            | Terminator::Return
            | Terminator::Unreachable
            | Terminator::Panic { .. }
            | Terminator::Drop { .. } => {}
        }
    }
}

/// Mutable counterpart of [`for_each_operand`], over the same positions.
fn for_each_operand_mut(body: &mut Body, f: &mut impl FnMut(&mut Operand)) {
    for block in &mut body.blocks {
        for statement in &mut block.stmts {
            match &mut statement.kind {
                StatementKind::Assign { rvalue, .. } => match rvalue {
                    Rvalue::Use(operand)
                    | Rvalue::UnaryOp { operand, .. }
                    | Rvalue::Cast { operand, .. }
                    | Rvalue::Repeat { value: operand, .. } => f(operand),
                    Rvalue::BinaryOp { lhs, rhs, .. } => {
                        f(lhs);
                        f(rhs);
                    }
                    Rvalue::Aggregate { operands, .. }
                    | Rvalue::CallIntrinsic { args: operands, .. } => {
                        operands.iter_mut().for_each(&mut *f);
                    }
                    Rvalue::Len(_) | Rvalue::Ref { .. } | Rvalue::StaticLoad(_) => {}
                },
                StatementKind::StaticStore { value, .. } => f(value),
                StatementKind::IterSource { source, .. } => f(source),
                StatementKind::IterAdapter {
                    closure_or_arg: Some(operand),
                    ..
                } => f(operand),
                StatementKind::IterAdapter { .. }
                | StatementKind::StorageLive(_)
                | StatementKind::StorageDead(_)
                | StatementKind::SetDiscriminant { .. }
                | StatementKind::IterNext { .. }
                | StatementKind::Nop => {}
            }
        }
        match &mut block.terminator {
            Terminator::Call { callee, args, .. } => {
                f(callee);
                args.iter_mut().for_each(&mut *f);
            }
            Terminator::SwitchInt { discriminant, .. } => f(discriminant),
            Terminator::Assert { cond, msg, .. } => {
                f(cond);
                msg.operands_mut().for_each(&mut *f);
            }
            Terminator::Goto { .. }
            | Terminator::Return
            | Terminator::Unreachable
            | Terminator::Panic { .. }
            | Terminator::Drop { .. } => {}
        }
    }
}

/// Records the instantiation a function reference names. A substitution the
/// checker left with an unsolved position names no instantiation at all, so
/// the reference keeps the template.
fn collect_from_operand(operand: &Operand, tcx: &TyCtxt, out: &mut HashMap<DefId, Vec<Substs>>) {
    if let Operand::FnRef { def, substs } = operand {
        // A type argument that is still a parameter names the template
        // itself, reached from another template's body, and is no
        // instantiation of it.
        if substs.is_empty()
            || substs.types().iter().any(|t| {
                matches!(tcx.kind_of(*t), TyKind::Var(_) | TyKind::Error)
                    || ty_contains_param(tcx, *t)
            })
        {
            return;
        }
        let list = out.entry(*def).or_default();
        if !list.iter().any(|existing| existing == substs) {
            list.push(substs.clone());
        }
    }
}

/// Points every reference to an instantiated generic function at the copy
/// monomorphisation emitted for it.
///
/// A callee becomes the copy's name. A function used as a value becomes the
/// copy's address, because a copy has no `DefId` for a value operand to name:
/// an assignment of the value takes the address directly, and a value in any
/// other position is first bound to a local holding that address.
fn route_to_specialisations(body: &mut Body, emitted: &HashSet<String>, tcx: &mut TyCtxt) {
    let emitted_name = |operand: &Operand| match operand {
        Operand::FnRef { def, substs } if !substs.is_empty() => {
            let name = mangled_name(*def, substs);
            emitted.contains(&name).then_some(name)
        }
        _ => None,
    };
    for block in &mut body.blocks {
        if let Terminator::Call { callee, .. } = &mut block.terminator
            && let Some(name) = emitted_name(callee)
        {
            *callee = Operand::Const(ConstValue::Str(name));
        }
    }
    for block in &mut body.blocks {
        for stmt in &mut block.stmts {
            if let StatementKind::Assign { rvalue, .. } = &mut stmt.kind
                && let Rvalue::Use(operand) = rvalue
                && let Some(name) = emitted_name(operand)
            {
                *rvalue = Rvalue::CallIntrinsic {
                    name: "gos_fn_addr",
                    args: vec![Operand::Const(ConstValue::Str(name))],
                };
            }
        }
    }
    // Every remaining value reference is hoisted into a fresh local, typed as
    // the function item it names so the backends keep it pointer-shaped.
    let mut hoisted: Vec<(Operand, String)> = Vec::new();
    for_each_operand_mut(body, &mut |operand| {
        if let Some(name) = emitted_name(operand) {
            hoisted.push((operand.clone(), name));
        }
    });
    if hoisted.is_empty() {
        return;
    }
    let mut addresses: HashMap<String, Local> = HashMap::new();
    let mut fresh: Vec<(Local, Ty, String)> = Vec::new();
    for (operand, name) in hoisted {
        if addresses.contains_key(&name) {
            continue;
        }
        let Operand::FnRef { def, substs } = operand else {
            continue;
        };
        let ty = tcx.intern(TyKind::FnDef { def, substs });
        // A body's local count is bounded by what lowering could index with a
        // `u32`, so the next index fits.
        let local = Local(u32::try_from(body.locals.len() + fresh.len()).unwrap_or(u32::MAX));
        addresses.insert(name.clone(), local);
        fresh.push((local, ty, name));
    }
    for (_, ty, _) in &fresh {
        body.locals.push(crate::ir::LocalDecl {
            ty: *ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
    }
    for_each_operand_mut(body, &mut |operand| {
        if let Some(name) = emitted_name(operand)
            && let Some(local) = addresses.get(&name)
        {
            *operand = Operand::Copy(Place::local(*local));
        }
    });
    // Bind each address at entry. A function address is a link-time constant,
    // so taking it once before any use is equivalent to taking it at each.
    let span = body.span;
    let entry: Vec<crate::ir::Statement> = fresh
        .into_iter()
        .map(|(local, _, name)| crate::ir::Statement {
            kind: StatementKind::Assign {
                place: Place::local(local),
                rvalue: Rvalue::CallIntrinsic {
                    name: "gos_fn_addr",
                    args: vec![Operand::Const(ConstValue::Str(name))],
                },
            },
            span,
            inlined: None,
        })
        .collect();
    if let Some(first) = body.blocks.first_mut() {
        first.stmts.splice(0..0, entry);
    }
}

fn resolve(tcx: &mut TyCtxt, ty: Ty) -> Ty {
    let _ = tcx.kind(ty);
    ty
}

/// Substitutes a specialisation's concrete types for the template's type
/// parameters within `ty`, recursing through composite types. A `Param` whose
/// position holds a const argument (`subst_tys[i] == None`) is left unchanged.
pub fn subst_param_ty(tcx: &mut TyCtxt, ty: Ty, subst_tys: &[Option<Ty>]) -> Ty {
    let kind = tcx.kind_of(ty).clone();
    match kind {
        TyKind::Param { idx, .. } => subst_tys
            .get(idx.0 as usize)
            .copied()
            .flatten()
            .unwrap_or(ty),
        TyKind::Ref { inner, mutability } => {
            let inner = subst_param_ty(tcx, inner, subst_tys);
            tcx.intern(TyKind::Ref { inner, mutability })
        }
        TyKind::Vec(elem) => {
            let elem = subst_param_ty(tcx, elem, subst_tys);
            tcx.intern(TyKind::Vec(elem))
        }
        TyKind::Slice(elem) => {
            let elem = subst_param_ty(tcx, elem, subst_tys);
            tcx.intern(TyKind::Slice(elem))
        }
        TyKind::Array { elem, len } => {
            let elem = subst_param_ty(tcx, elem, subst_tys);
            tcx.intern(TyKind::Array { elem, len })
        }
        TyKind::Tuple(elems) => {
            let elems = elems
                .iter()
                .map(|&e| subst_param_ty(tcx, e, subst_tys))
                .collect();
            tcx.intern(TyKind::Tuple(elems))
        }
        TyKind::HashMap {
            key,
            value,
            ordered,
        } => {
            let key = subst_param_ty(tcx, key, subst_tys);
            let value = subst_param_ty(tcx, value, subst_tys);
            tcx.intern(TyKind::HashMap {
                key,
                value,
                ordered,
            })
        }
        TyKind::Iterator(elem) => {
            let elem = subst_param_ty(tcx, elem, subst_tys);
            tcx.intern(TyKind::Iterator(elem))
        }
        TyKind::Range(elem) => {
            let elem = subst_param_ty(tcx, elem, subst_tys);
            tcx.intern(TyKind::Range(elem))
        }
        // A channel endpoint and a join handle name the payload they carry,
        // and the send, receive, and join lowering picks its value
        // representation from that payload type.
        TyKind::Sender(elem) => {
            let elem = subst_param_ty(tcx, elem, subst_tys);
            tcx.intern(TyKind::Sender(elem))
        }
        TyKind::Receiver(elem) => {
            let elem = subst_param_ty(tcx, elem, subst_tys);
            tcx.intern(TyKind::Receiver(elem))
        }
        TyKind::JoinHandle(elem) => {
            let elem = subst_param_ty(tcx, elem, subst_tys);
            tcx.intern(TyKind::JoinHandle(elem))
        }
        // A callable parameter carries the template's parameters inside
        // its signature, and the compiled tiers build the call from that
        // signature: an unsubstituted `Fn(T) -> T` on an `f64`
        // instantiation puts the argument in an integer register and
        // reads the result out of one.
        TyKind::FnPtr(sig) => {
            let sig = subst_param_sig(tcx, &sig, subst_tys);
            tcx.intern(TyKind::FnPtr(sig))
        }
        TyKind::FnTrait(sig) => {
            let sig = subst_param_sig(tcx, &sig, subst_tys);
            tcx.intern(TyKind::FnTrait(sig))
        }
        TyKind::Adt { def, substs } => {
            let substs = subst_param_substs(tcx, &substs, subst_tys);
            tcx.intern(TyKind::Adt { def, substs })
        }
        TyKind::Alias { def, substs } => {
            let substs = subst_param_substs(tcx, &substs, subst_tys);
            tcx.intern(TyKind::Alias { def, substs })
        }
        // A generic function named inside a generic body carries the body's
        // parameters as its own type arguments, and those say which
        // instantiation the call reaches.
        TyKind::FnDef { def, substs } => {
            let substs = subst_param_substs(tcx, &substs, subst_tys);
            tcx.intern(TyKind::FnDef { def, substs })
        }
        _ => ty,
    }
}

/// [`subst_param_ty`] over every type argument of `substs`; const arguments
/// are carried unchanged.
fn subst_param_substs(tcx: &mut TyCtxt, substs: &Substs, subst_tys: &[Option<Ty>]) -> Substs {
    let new_args = substs
        .as_slice()
        .iter()
        .map(|a| match a {
            GenericArg::Type(t) => GenericArg::Type(subst_param_ty(tcx, *t, subst_tys)),
            other @ (GenericArg::Const(_) | GenericArg::ConstParam(_)) => other.clone(),
        })
        .collect();
    Substs::from_args(new_args)
}

/// [`subst_param_ty`] over every type a callable signature names.
fn subst_param_sig(
    tcx: &mut TyCtxt,
    sig: &gossamer_types::FnSig,
    subst_tys: &[Option<Ty>],
) -> gossamer_types::FnSig {
    gossamer_types::FnSig {
        inputs: sig
            .inputs
            .iter()
            .map(|input| subst_param_ty(tcx, *input, subst_tys))
            .collect(),
        output: subst_param_ty(tcx, sig.output, subst_tys),
    }
}

/// Rejects a body that reaches code generation with a type parameter still
/// in its locals. Every call site of a generic body is routed to an
/// instantiation lowered with concrete types, so a template that is still
/// reachable was called with type arguments nothing could name, and its
/// layouts, comparisons, and calls were chosen for an opaque slot. Returns
/// one message per such body; empty when every reachable body is concrete.
#[must_use]
pub fn check_generic_layouts(bodies: &[Body], tcx: &TyCtxt) -> Vec<String> {
    bodies
        .iter()
        .filter(|body| body_has_param(body, tcx))
        .map(|body| {
            let locals: Vec<String> = body
                .locals
                .iter()
                .enumerate()
                .filter(|(_, local)| ty_contains_param(tcx, local.ty))
                .map(|(index, local)| {
                    format!(
                        "_{index}: {}",
                        gossamer_types::printer::render_ty(tcx, local.ty)
                    )
                })
                .collect();
            format!(
                "internal compiler error: the generic body `{}` reached code \
                 generation without an instantiation for its type parameters ({})",
                body.name,
                locals.join(", ")
            )
        })
        .collect()
}

/// Returns the stable mangled name for a specialised copy of
/// function `def` at substitution `substs`. Callers (MIR codegen,
/// native backend) use this name as the symbol the specialised body
/// is registered under.
#[must_use]
pub fn mangled_name(def: DefId, substs: &Substs) -> String {
    let mut out = format!("fn#{}__mono__", def.local);
    for (i, arg) in substs.as_slice().iter().enumerate() {
        if i > 0 {
            out.push('_');
        }
        match arg {
            GenericArg::Type(ty) => {
                out.push('t');
                out.push_str(&ty.as_u32().to_string());
            }
            GenericArg::Const(c) => {
                out.push('c');
                out.push_str(&c.to_string());
            }
            GenericArg::ConstParam(idx) => {
                out.push('p');
                out.push_str(&idx.0.to_string());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monomorphise_is_idempotent_on_a_concrete_body() {
        // Smoke test: running the pass twice over the same body must
        // produce identical structural output - the pass is
        // deliberately a fixpoint.
        let mut tcx = TyCtxt::new();
        let unit = tcx.unit();
        let recorded = unit;
        let body = Body {
            name: "f".to_string(),
            def: None,
            arity: 0,
            locals: vec![
                crate::ir::LocalDecl {
                    ty: unit,
                    debug_name: None,
                    mutable: false,
                    region: false,
                },
                crate::ir::LocalDecl {
                    ty: recorded,
                    debug_name: None,
                    mutable: false,
                    region: false,
                },
            ],
            blocks: Vec::new(),
            span: gossamer_lex::Span::new(
                {
                    let mut map = gossamer_lex::SourceMap::new();
                    map.add_file("t.gos", "")
                },
                0,
                0,
            ),
        };
        let before = body.locals[1].ty;
        let mut bodies = vec![body];
        monomorphise(&gossamer_hir::HirProgram::default(), &mut bodies, &mut tcx);
        assert_eq!(bodies[0].locals[1].ty, before);
        monomorphise(&gossamer_hir::HirProgram::default(), &mut bodies, &mut tcx);
        assert_eq!(bodies[0].locals[1].ty, before);
    }
}
