//! Callbacks a pointer-crossing combinator calls with the address of a
//! two-word carrier element.
//!
//! A combinator over a sequence of wide elements hands its callback the
//! address of each element's storage. A struct or tuple parameter is that
//! address already, but an `Option` / `Result` / inline-enum parameter is two
//! words held by value, so the lifted closure that takes one is marked to take
//! it by address instead, and each backend loads it into the frame at entry.

use std::collections::HashMap;

use gossamer_types::{TyCtxt, TyKind};

use crate::ir::{Body, ConstValue, Local, Operand, Rvalue, StatementKind, Terminator};

/// The local a `gos_store(env, 0, code)` statement writes into slot 0 of, with
/// the function `code` names.
fn env_code_stores(body: &Body) -> HashMap<Local, String> {
    let mut fn_names: HashMap<Local, String> = HashMap::new();
    for stmt in body.blocks.iter().flat_map(|b| &b.stmts) {
        if let StatementKind::Assign {
            place,
            rvalue:
                Rvalue::CallIntrinsic {
                    name: "gos_fn_addr",
                    args,
                },
        } = &stmt.kind
            && place.projection.is_empty()
            && let [Operand::Const(ConstValue::Str(callee))] = args.as_slice()
        {
            fn_names.insert(place.local, callee.clone());
        }
    }
    let mut out = HashMap::new();
    for stmt in body.blocks.iter().flat_map(|b| &b.stmts) {
        if let StatementKind::Assign {
            rvalue:
                Rvalue::CallIntrinsic {
                    name: "gos_store",
                    args,
                },
            ..
        } = &stmt.kind
            && let [Operand::Copy(env), offset, Operand::Copy(code)] = args.as_slice()
            && env.projection.is_empty()
            && code.projection.is_empty()
            && is_zero(body, offset)
            && let Some(callee) = fn_names.get(&code.local)
        {
            out.insert(env.local, callee.clone());
        }
    }
    out
}

/// Whether `op` is the constant `value`, written in place or held by a local
/// whose only non-null value it is.
fn is_int(body: &Body, op: &Operand, value: i128) -> bool {
    match op {
        Operand::Const(ConstValue::Int(v)) => *v == value,
        Operand::Copy(p) if p.projection.is_empty() => {
            let mut seen = false;
            for stmt in body.blocks.iter().flat_map(|b| &b.stmts) {
                if let StatementKind::Assign { place, rvalue } = &stmt.kind
                    && place.local == p.local
                {
                    match rvalue {
                        Rvalue::Use(Operand::Const(ConstValue::Int(v))) if *v == value => {
                            seen = true
                        }
                        Rvalue::Use(Operand::Const(ConstValue::Int(0))) => {}
                        _ => return false,
                    }
                }
            }
            (seen || value == 0)
                && !body.blocks.iter().any(|b| {
                    matches!(&b.terminator, Terminator::Call { destination, .. } if destination.local == p.local)
                })
        }
        _ => false,
    }
}

fn is_zero(body: &Body, op: &Operand) -> bool {
    is_int(body, op, 0)
}

/// The source a local copies, when its one write is a bare copy of a local.
fn copy_source(body: &Body, local: Local) -> Option<Local> {
    let mut source = None;
    for stmt in body.blocks.iter().flat_map(|b| &b.stmts) {
        if let StatementKind::Assign { place, rvalue } = &stmt.kind
            && place.local == local
            && place.projection.is_empty()
        {
            match rvalue {
                Rvalue::Use(Operand::Const(ConstValue::Int(0))) => {}
                Rvalue::Use(Operand::Copy(src))
                    if src.projection.is_empty() && source.is_none() =>
                {
                    source = Some(src.local);
                }
                _ => return None,
            }
        }
    }
    source
}

/// One callback a pointer-crossing combinator call is handed.
struct CallbackSite {
    body: usize,
    /// The block whose terminator, or whose statement `stmt`, hands the
    /// callback over.
    block: usize,
    stmt: Option<usize>,
    /// The operand local naming the callback at that site.
    operand: Local,
    env: Local,
    callee: String,
    elem: gossamer_types::Ty,
    skip_first: bool,
}

/// The two-word carrier element a lazy iterator operand streams, if any. A
/// lazy stream hands each element on as one word, which for a carrier is the
/// address of its two words.
fn streamed_carrier(body: &Body, tcx: &TyCtxt, op: &Operand) -> Option<gossamer_types::Ty> {
    let Operand::Copy(p) = op else { return None };
    if !p.projection.is_empty() {
        return None;
    }
    match tcx.kind_of(body.local_ty(p.local)) {
        TyKind::Iterator(item) if tcx.is_two_word_value(*item) => Some(*item),
        _ => None,
    }
}

/// Follows `local` back through the bare copies that bind it to the
/// environment a closure literal built, answering that environment and the
/// lifted body it calls.
fn resolve_env(
    body: &Body,
    stores: &HashMap<Local, String>,
    local: Local,
) -> Option<(Local, String)> {
    let mut env = local;
    for _ in 0..body.locals.len() {
        if let Some(callee) = stores.get(&env) {
            return (callee.starts_with(gossamer_hir::LIFTED_CLOSURE_PREFIX)
                || callee.starts_with(SHAPE_THUNK_PREFIX))
            .then(|| (env, callee.clone()));
        }
        env = copy_source(body, env)?;
    }
    None
}

/// The prefix of the thunks that call a function value through an
/// environment, named by the shapes of its inputs and answer.
const SHAPE_THUNK_PREFIX: &str = "__fn_thunk_";

/// The function a shape thunk's environment `env` calls: the name its word 1
/// is stored from.
fn thunk_target(body: &Body, env: Local) -> Option<String> {
    let mut fn_names: HashMap<Local, String> = HashMap::new();
    for stmt in body.blocks.iter().flat_map(|b| &b.stmts) {
        if let StatementKind::Assign {
            place,
            rvalue:
                Rvalue::CallIntrinsic {
                    name: "gos_fn_addr",
                    args,
                },
        } = &stmt.kind
            && let [Operand::Const(ConstValue::Str(callee))] = args.as_slice()
        {
            fn_names.insert(place.local, callee.clone());
        }
    }
    body.blocks
        .iter()
        .flat_map(|b| &b.stmts)
        .find_map(|stmt| match &stmt.kind {
            StatementKind::Assign {
                rvalue:
                    Rvalue::CallIntrinsic {
                        name: "gos_store",
                        args,
                    },
                ..
            } => match args.as_slice() {
                [Operand::Copy(e), offset, Operand::Copy(code)]
                    if e.local == env && is_int(body, offset, 8) =>
                {
                    fn_names.get(&code.local).cloned()
                }
                _ => None,
            },
            _ => None,
        })
}

/// Marks, or clones and marks, every lifted closure a pointer-crossing
/// combinator calls with two-word carrier elements. A closure whose code is
/// named anywhere else keeps its own calling convention, and the combinator's
/// environment is pointed at a copy that takes the elements by address.
pub(crate) fn mark_carrier_address_params(bodies: &mut Vec<Body>, start: usize, tcx: &mut TyCtxt) {
    let mut sites: Vec<CallbackSite> = Vec::new();
    for (bi, body) in bodies.iter().enumerate().skip(start) {
        let mut stores: Option<HashMap<Local, String>> = None;
        // A lazy `map` / `filter` adapter over a stream of carriers.
        for (blk, si, stmt) in body.blocks.iter().enumerate().flat_map(|(blk, b)| {
            b.stmts
                .iter()
                .enumerate()
                .map(move |(si, st)| (blk, si, st))
        }) {
            if let StatementKind::IterAdapter {
                upstream,
                closure_or_arg: Some(Operand::Copy(callback)),
                ..
            } = &stmt.kind
                && callback.projection.is_empty()
                && let Some(elem) = streamed_carrier(body, tcx, &Operand::Copy(upstream.clone()))
            {
                let stores = stores.get_or_insert_with(|| env_code_stores(body));
                if let Some((env, callee)) = resolve_env(body, stores, callback.local) {
                    sites.push(CallbackSite {
                        body: bi,
                        block: blk,
                        stmt: Some(si),
                        operand: callback.local,
                        env,
                        callee,
                        elem,
                        skip_first: false,
                    });
                }
            }
        }
        for (blk, block) in body.blocks.iter().enumerate() {
            let Terminator::Call {
                callee: Operand::Const(ConstValue::Str(name)),
                args,
                ..
            } = &block.terminator
            else {
                continue;
            };
            // A lazy terminal or adapter call over a stream of carriers.
            if name.starts_with("gos_rt_lazy_iter_")
                && let Some(elem) = args.iter().find_map(|op| streamed_carrier(body, tcx, op))
            {
                let stores = stores.get_or_insert_with(|| env_code_stores(body));
                for op in args {
                    let Operand::Copy(p) = op else { continue };
                    if !p.projection.is_empty() {
                        continue;
                    }
                    if let Some((env, callee)) = resolve_env(body, stores, p.local) {
                        sites.push(CallbackSite {
                            body: bi,
                            block: blk,
                            stmt: None,
                            operand: p.local,
                            env,
                            callee,
                            elem,
                            skip_first: name.contains("fold") || name.contains("scan"),
                        });
                    }
                }
                continue;
            }
            let Some(abi) = gossamer_abi::combinator_abi_of(name) else {
                continue;
            };
            if abi.elem != gossamer_abi::ElemClass::Ptr {
                continue;
            }
            let elem = args.iter().find_map(|op| match op {
                Operand::Copy(p) if p.projection.is_empty() => {
                    match tcx.kind_of(body.local_ty(p.local)) {
                        TyKind::Vec(e) | TyKind::Slice(e) => Some(*e),
                        _ => None,
                    }
                }
                _ => None,
            });
            let Some(elem) =
                elem.filter(|e| crate::lower::carrier_ref::is_two_word_carrier(tcx, *e))
            else {
                continue;
            };
            let stores = stores.get_or_insert_with(|| env_code_stores(body));
            for op in args {
                let Operand::Copy(p) = op else { continue };
                if !p.projection.is_empty() {
                    continue;
                }
                let mut env = p.local;
                for _ in 0..body.locals.len() {
                    if stores.contains_key(&env) {
                        break;
                    }
                    match copy_source(body, env) {
                        Some(src) => env = src,
                        None => break,
                    }
                }
                let Some(callee) = stores.get(&env) else {
                    continue;
                };
                if !callee.starts_with(gossamer_hir::LIFTED_CLOSURE_PREFIX)
                    && !callee.starts_with(SHAPE_THUNK_PREFIX)
                {
                    continue;
                }
                sites.push(CallbackSite {
                    body: bi,
                    block: blk,
                    stmt: None,
                    operand: p.local,
                    env,
                    callee: callee.clone(),
                    elem,
                    skip_first: matches!(abi.combinator, "fold" | "scan" | "try_fold"),
                });
            }
        }
    }
    if sites.is_empty() {
        return;
    }
    // Later sites first, so the statements inserted at one never move the
    // index of another.
    sites.sort_by_key(|site| std::cmp::Reverse((site.body, site.block, site.stmt)));
    // How many places take each closure's code address.
    let mut addr_uses: HashMap<String, usize> = HashMap::new();
    for stmt in bodies.iter().flat_map(|b| &b.blocks).flat_map(|b| &b.stmts) {
        if let StatementKind::Assign {
            rvalue:
                Rvalue::CallIntrinsic {
                    name: "gos_fn_addr",
                    args,
                },
            ..
        } = &stmt.kind
            && let [Operand::Const(ConstValue::Str(callee))] = args.as_slice()
        {
            *addr_uses.entry(callee.clone()).or_default() += 1;
        }
    }
    let mut by_name: HashMap<String, usize> = bodies
        .iter()
        .enumerate()
        .map(|(i, b)| (b.name.clone(), i))
        .collect();
    for site in sites {
        if let Some(shape) = site.callee.strip_prefix(SHAPE_THUNK_PREFIX) {
            let Some(real) = thunk_target(&bodies[site.body], site.env) else {
                continue;
            };
            let Some(&real_index) = by_name.get(&real) else {
                continue;
            };
            let real_body = &bodies[real_index];
            let Some((inputs, answer)) = shape.split_once('_') else {
                continue;
            };
            // The function's own parameters may lead with an environment the
            // thunk's inputs do not name.
            let lead = (real_body.arity as usize).saturating_sub(inputs.len());
            let by_address: String = inputs
                .chars()
                .enumerate()
                .map(|(k, c)| {
                    let param = Local(u32::try_from(lead + k + 1).unwrap_or(0));
                    let element = !(site.skip_first && k == 0)
                        && real_body.arity as usize > lead + k
                        && real_body.local_ty(param) == site.elem;
                    if c == 'r' && element { 'q' } else { c }
                })
                .collect();
            if by_address != inputs {
                let thunk = format!("{SHAPE_THUNK_PREFIX}{by_address}_{answer}");
                hand_thunk_env(&mut bodies[site.body], &site, &thunk, &real, tcx);
            }
            continue;
        }
        let Some(&target) = by_name.get(&site.callee) else {
            continue;
        };
        let target_body = &bodies[target];
        let params: Vec<Local> = (2..=target_body.arity)
            .skip(usize::from(site.skip_first))
            .map(Local)
            .filter(|&l| target_body.local_ty(l) == site.elem)
            .collect();
        if params.is_empty() {
            continue;
        }
        let shared = addr_uses.get(&site.callee).copied().unwrap_or(0) > 1
            || !env_only_feeds_pointer_crossings(&bodies[site.body], site.env, tcx);
        let marked = if shared {
            let adapter_name = format!("{}$by_address", site.callee);
            let index = match by_name.get(&adapter_name) {
                Some(&i) => i,
                None => {
                    bodies.push(adapter_body(&bodies[target], &adapter_name, tcx));
                    by_name.insert(adapter_name.clone(), bodies.len() - 1);
                    bodies.len() - 1
                }
            };
            hand_adapter_env(&mut bodies[site.body], &site, &adapter_name, tcx);
            index
        } else {
            target
        };
        let body = &mut bodies[marked];
        for param in params {
            if !body.address_params.contains(&param) {
                body.address_params.push(param);
            }
        }
    }
}

/// Whether a pointer-crossing combinator call is the only thing that calls
/// through `env`: every mention of it, or of a copy of it, is its own
/// construction, a copy, a reference-count operation, or an argument of such
/// a call.
fn env_only_feeds_pointer_crossings(body: &Body, env: Local, tcx: &TyCtxt) -> bool {
    let mut held = vec![env];
    let mut changed = true;
    while changed {
        changed = false;
        for stmt in body.blocks.iter().flat_map(|b| &b.stmts) {
            if let StatementKind::Assign {
                place,
                rvalue: Rvalue::Use(Operand::Copy(src)),
            } = &stmt.kind
                && place.projection.is_empty()
                && src.projection.is_empty()
                && held.contains(&src.local)
                && !held.contains(&place.local)
            {
                held.push(place.local);
                changed = true;
            }
        }
    }
    let names_held = |op: &Operand| matches!(op, Operand::Copy(p) if held.contains(&p.local));
    for stmt in body.blocks.iter().flat_map(|b| &b.stmts) {
        let StatementKind::Assign { place, rvalue } = &stmt.kind else {
            continue;
        };
        match rvalue {
            Rvalue::Use(Operand::Copy(src)) if held.contains(&src.local) => {
                if !(src.projection.is_empty() && place.projection.is_empty()) {
                    return false;
                }
            }
            Rvalue::CallIntrinsic { name, args } if args.iter().any(names_held) => {
                let construction = *name == "gos_store"
                    && args.first().is_some_and(names_held)
                    && !args[1..].iter().any(names_held);
                let counted = crate::uniqueness::is_release_intrinsic(name)
                    || matches!(*name, "gos_rt_rc_retain" | "gos_rt_rc_mark_shared");
                if !(construction || counted) {
                    return false;
                }
            }
            other => {
                if held
                    .iter()
                    .any(|&l| crate::opt::rvalue_mentions_local(other, l))
                {
                    return false;
                }
            }
        }
    }
    for block in &body.blocks {
        match &block.terminator {
            Terminator::Call { callee, args, .. } => {
                if !args.iter().any(names_held) && !names_held(callee) {
                    continue;
                }
                let Operand::Const(ConstValue::Str(name)) = callee else {
                    return false;
                };
                let pointer_crossing = gossamer_abi::combinator_abi_of(name)
                    .is_some_and(|abi| abi.elem == gossamer_abi::ElemClass::Ptr)
                    && args.iter().any(|op| {
                        matches!(op, Operand::Copy(p)
                        if p.projection.is_empty()
                            && matches!(tcx.kind_of(body.local_ty(p.local)),
                                TyKind::Vec(e) | TyKind::Slice(e)
                                    if crate::lower::carrier_ref::is_two_word_carrier(tcx, *e)))
                    });
                let lazy_stream = name.starts_with("gos_rt_lazy_iter_")
                    && args
                        .iter()
                        .any(|op| streamed_carrier(body, tcx, op).is_some());
                if !(pointer_crossing || lazy_stream) {
                    return false;
                }
            }
            other => {
                if held
                    .iter()
                    .any(|&l| crate::opt::term_mentions_local(other, l))
                {
                    return false;
                }
            }
        }
    }
    true
}

/// The adapter a combinator calls in place of the lifted closure `inner`
/// when that closure is called by value elsewhere too: it takes the elements
/// by address, reads `inner`'s own environment out of word 1 of its own, and
/// calls `inner` with the values.
fn adapter_body(inner: &Body, name: &str, tcx: &mut TyCtxt) -> Body {
    let span = inner.span;
    let mut locals: Vec<crate::ir::LocalDecl> = (0..=inner.arity as usize)
        .map(|i| crate::ir::LocalDecl {
            ty: inner.locals[i].ty,
            debug_name: None,
            mutable: false,
            region: false,
        })
        .collect();
    let mut fresh = |ty| {
        locals.push(crate::ir::LocalDecl {
            ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
        Local(u32::try_from(locals.len() - 1).expect("local overflow"))
    };
    let env_ty = inner.locals[1].ty;
    let word_ty = tcx.int_ty(gossamer_types::IntTy::I64);
    let inner_env = fresh(env_ty);
    let offset = fresh(word_ty);
    let block = |id: u32, stmts, terminator| crate::ir::BasicBlock {
        id: crate::ir::BlockId(id),
        stmts,
        terminator,
        span,
        terminator_span: None,
        terminator_inlined: None,
    };
    let blocks = vec![
        block(
            0,
            vec![crate::ir::Statement {
                kind: StatementKind::Assign {
                    place: crate::ir::Place::local(offset),
                    rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(8))),
                },
                span,
                inlined: None,
            }],
            Terminator::Call {
                callee: Operand::Const(ConstValue::Str("gos_load".to_string())),
                args: vec![
                    Operand::Copy(crate::ir::Place::local(Local(1))),
                    Operand::Copy(crate::ir::Place::local(offset)),
                ],
                destination: crate::ir::Place::local(inner_env),
                target: Some(crate::ir::BlockId(1)),
            },
        ),
        block(
            1,
            Vec::new(),
            Terminator::Call {
                callee: Operand::Const(ConstValue::Str(inner.name.clone())),
                args: std::iter::once(Operand::Copy(crate::ir::Place::local(inner_env)))
                    .chain(
                        (2..=inner.arity).map(|l| Operand::Copy(crate::ir::Place::local(Local(l)))),
                    )
                    .collect(),
                destination: crate::ir::Place::local(Local::RETURN),
                target: Some(crate::ir::BlockId(2)),
            },
        ),
        block(2, Vec::new(), Terminator::Return),
    ];
    Body {
        name: name.to_string(),
        def: None,
        arity: inner.arity,
        locals,
        blocks,
        span,
        shared_stores: Vec::new(),
        lent_slots: Vec::new(),
        address_params: Vec::new(),
    }
}

/// Gives the combinator at `site` an environment of its own for the adapter
/// `adapter`: word 0 the adapter's code, word 1 a counted share of the
/// closure's environment, released once the combinator has it.
fn hand_adapter_env(body: &mut Body, site: &CallbackSite, adapter: &str, tcx: &mut TyCtxt) {
    let meta = "gos_rc_meta_closure_adapter".to_string();
    tcx.register_rc_meta(
        meta.clone(),
        vec![gossamer_abi::rc::RC_KIND_STRUCT, 1, 0, 1, 1],
    );
    let span = body.blocks[site.block].span;
    let env_ty = body.local_ty(site.operand);
    let word_ty = tcx.int_ty(gossamer_types::IntTy::I64);
    let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
    let fresh = |body: &mut Body, ty| {
        body.locals.push(crate::ir::LocalDecl {
            ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
        Local(u32::try_from(body.locals.len() - 1).expect("local overflow"))
    };
    let own = fresh(body, env_ty);
    let code = fresh(body, word_ty);
    let sinks: Vec<Local> = (0..4).map(|_| fresh(body, unit_ty)).collect();
    let assign = |place: Local, rvalue: Rvalue| crate::ir::Statement {
        kind: StatementKind::Assign {
            place: crate::ir::Place::local(place),
            rvalue,
        },
        span,
        inlined: None,
    };
    let copy = |l: Local| Operand::Copy(crate::ir::Place::local(l));
    let intrinsic = |name: &'static str, args: Vec<Operand>| Rvalue::CallIntrinsic { name, args };
    let build = vec![
        assign(
            own,
            intrinsic(
                "gos_rc_alloc",
                vec![
                    Operand::Const(ConstValue::Int(16)),
                    Operand::Const(ConstValue::Str(meta)),
                ],
            ),
        ),
        assign(
            code,
            intrinsic(
                "gos_fn_addr",
                vec![Operand::Const(ConstValue::Str(adapter.to_string()))],
            ),
        ),
        assign(
            sinks[0],
            intrinsic(
                "gos_store",
                vec![copy(own), Operand::Const(ConstValue::Int(0)), copy(code)],
            ),
        ),
        assign(
            sinks[1],
            intrinsic("gos_rt_rc_retain", vec![copy(site.operand)]),
        ),
        assign(
            sinks[2],
            intrinsic(
                "gos_store",
                vec![
                    copy(own),
                    Operand::Const(ConstValue::Int(8)),
                    copy(site.operand),
                ],
            ),
        ),
    ];
    let release = assign(sinks[3], intrinsic("gos_rt_rc_release", vec![copy(own)]));
    place_own_env(body, site, own, build, release);
}

/// Gives the combinator at `site` an environment of its own that calls `real`
/// through the shape thunk `thunk`, which reads the carrier elements through
/// the addresses the combinator passes. It holds no counted value and is
/// released once the combinator has it.
fn hand_thunk_env(body: &mut Body, site: &CallbackSite, thunk: &str, real: &str, tcx: &mut TyCtxt) {
    let span = body.blocks[site.block].span;
    let env_ty = body.local_ty(site.operand);
    let word_ty = tcx.int_ty(gossamer_types::IntTy::I64);
    let unit_ty = tcx.unit_interned().unwrap_or(body.locals[0].ty);
    let fresh = |body: &mut Body, ty| {
        body.locals.push(crate::ir::LocalDecl {
            ty,
            debug_name: None,
            mutable: false,
            region: false,
        });
        Local(u32::try_from(body.locals.len() - 1).expect("local overflow"))
    };
    let own = fresh(body, env_ty);
    let code = fresh(body, word_ty);
    let target = fresh(body, word_ty);
    let sinks: Vec<Local> = (0..3).map(|_| fresh(body, unit_ty)).collect();
    let assign = |place: Local, rvalue: Rvalue| crate::ir::Statement {
        kind: StatementKind::Assign {
            place: crate::ir::Place::local(place),
            rvalue,
        },
        span,
        inlined: None,
    };
    let copy = |l: Local| Operand::Copy(crate::ir::Place::local(l));
    let intrinsic = |name: &'static str, args: Vec<Operand>| Rvalue::CallIntrinsic { name, args };
    let build = vec![
        assign(
            own,
            intrinsic(
                "gos_rc_alloc",
                vec![
                    Operand::Const(ConstValue::Int(16)),
                    Operand::Const(ConstValue::Str(String::new())),
                ],
            ),
        ),
        assign(
            code,
            intrinsic(
                "gos_fn_addr",
                vec![Operand::Const(ConstValue::Str(thunk.to_string()))],
            ),
        ),
        assign(
            target,
            intrinsic(
                "gos_fn_addr",
                vec![Operand::Const(ConstValue::Str(real.to_string()))],
            ),
        ),
        assign(
            sinks[0],
            intrinsic(
                "gos_store",
                vec![copy(own), Operand::Const(ConstValue::Int(0)), copy(code)],
            ),
        ),
        assign(
            sinks[1],
            intrinsic(
                "gos_store",
                vec![copy(own), Operand::Const(ConstValue::Int(8)), copy(target)],
            ),
        ),
    ];
    let release = assign(sinks[2], intrinsic("gos_rt_rc_release", vec![copy(own)]));
    place_own_env(body, site, own, build, release);
}

/// Hands the combinator at `site` the environment `own`, built by `build`
/// just before it and given back by `release` right after.
fn place_own_env(
    body: &mut Body,
    site: &CallbackSite,
    own: Local,
    build: Vec<crate::ir::Statement>,
    release: crate::ir::Statement,
) {
    let span = body.blocks[site.block].span;
    let renamed = |op: &mut Operand| {
        if let Operand::Copy(p) = op
            && p.local == site.operand
            && p.projection.is_empty()
        {
            p.local = own;
        }
    };
    match site.stmt {
        Some(si) => {
            let block = &mut body.blocks[site.block];
            if let StatementKind::IterAdapter {
                closure_or_arg: Some(op),
                ..
            } = &mut block.stmts[si].kind
            {
                renamed(op);
            }
            block.stmts.insert(si + 1, release);
            for (k, stmt) in build.into_iter().enumerate() {
                block.stmts.insert(si + k, stmt);
            }
        }
        None => {
            let next_id =
                crate::ir::BlockId(u32::try_from(body.blocks.len()).expect("block overflow"));
            let block = &mut body.blocks[site.block];
            block.stmts.extend(build);
            let Terminator::Call { args, target, .. } = &mut block.terminator else {
                return;
            };
            args.iter_mut().for_each(renamed);
            if let Some(after) = target.replace(next_id) {
                body.blocks.push(crate::ir::BasicBlock {
                    id: next_id,
                    stmts: vec![release],
                    terminator: Terminator::Goto { target: after },
                    span,
                    terminator_span: None,
                    terminator_inlined: None,
                });
            }
        }
    }
}
