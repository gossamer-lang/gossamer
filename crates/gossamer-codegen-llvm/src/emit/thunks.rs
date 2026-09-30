//! C-ABI and shape thunks: the adapters between Gossamer calling conventions and runtime callbacks.

use std::fmt::Write;

use anyhow::{Result, anyhow};
use gossamer_mir::Body;
use gossamer_types::TyCtxt;

use super::target_is_windows;

/// Renders an `extern declare` for a body outside the current
/// chunk. The signature must match what its defining LLVM chunk
/// emits so the linker can hook them up.
/// Verifies a single module-level global declaration string has
/// the structural shape LLVM IR expects. We don't parse the full
/// grammar - we only check the prefix tokens an entry must lead
/// with. The check is cheap (string scan, no allocation) and
/// catches the realistic regression mode: a *bare* identifier
/// (e.g. `"my_const"` instead of `"@my_const = constant ..."`)
/// being inserted via `runtime_refs.insert(...)`. That class of
/// bug previously corrupted the IR module silently and forced
/// `llc` to error which then triggered the per-fn Cranelift
/// fallback for unrelated bodies.
/// Walks `body`'s MIR statements + terminators looking for
/// `gos_fn_addr("__fn_thunk_*")` references. The names matter
/// because each unique shape needs a synthesised LLVM thunk;
/// see [`render_shape_thunk`].
pub(super) fn collect_thunk_names_in_body(
    body: &Body,
    out: &mut std::collections::BTreeSet<String>,
) {
    use gossamer_mir::{ConstValue, Operand, Rvalue, StatementKind, Terminator};
    let mut visit_args = |args: &[Operand], name: &str| {
        if name == "gos_fn_addr"
            && let Some(Operand::Const(ConstValue::Str(s))) = args.first()
            && s.starts_with("__fn_thunk_")
        {
            out.insert(s.clone());
        }
    };
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign { rvalue, .. } = &stmt.kind
                && let Rvalue::CallIntrinsic { name, args } = rvalue
            {
                visit_args(args, name);
            }
        }
        if let Terminator::Call { callee, args, .. } = &block.terminator
            && let Operand::Const(ConstValue::Str(name)) = callee
        {
            visit_args(args, name);
        }
    }
}

/// `spawn(f)`'s shim. Its callable crosses as `i128` only when it answers
/// a two-word `Result` / `Option`; the runtime reads the `ret_words`
/// argument (index 2) to decide, and calls a one-word callable as
/// `-> i64`, which already agrees on the GP register.
pub(crate) const CABI_SPAWN_SHIM: &str = "gos_rt_spawn_ex";
pub(crate) const CABI_SPAWN_RET_WORDS_ARG: usize = 2;
pub(crate) const SPAWN_RET_TWO_WORDS: i128 = 2;

/// Win64 entry the runtime is handed in place of a two-word spawn
/// callable's own address: it forwards to the callable (which returns the
/// `i128` in the GP-register pair) and re-emits the value as `<16 x i8>`,
/// the vector register rustc's `extern "C" fn(..) -> i128` reads.
pub(crate) const SPAWN_WIDE_CABI_THUNK: &str = "__gos_spawn_wide$cabi";

/// True when `body` spawns a callable that answers a two-word value.
pub(super) fn body_has_wide_spawn(body: &Body) -> bool {
    use gossamer_mir::{ConstValue, Operand, Terminator};
    body.blocks.iter().any(|block| {
        let Terminator::Call { callee, args, .. } = &block.terminator else {
            return false;
        };
        matches!(callee, Operand::Const(ConstValue::Str(sym)) if sym == CABI_SPAWN_SHIM)
            && args
                .get(CABI_SPAWN_RET_WORDS_ARG)
                .and_then(|w| operand_const_int(body, w))
                == Some(SPAWN_RET_TWO_WORDS)
    })
}

/// Renders [`SPAWN_WIDE_CABI_THUNK`]. The spawn lowering hands the runtime
/// the env blob and the address at its slot 0; this thunk takes that
/// argument pair's place, so it reads slot 0 itself and calls it the way
/// gossamer code does.
pub(super) fn render_spawn_wide_cabi_thunk() -> String {
    let mut out = String::new();
    let linkage = "linkonce_odr ";
    let comdat = linkonce_comdat(&mut out, linkage, SPAWN_WIDE_CABI_THUNK);
    let _ = writeln!(
        out,
        "define {linkage}<16 x i8> @\"{SPAWN_WIDE_CABI_THUNK}\"(ptr %env){comdat} {{"
    );
    writeln!(out, "entry:").unwrap();
    writeln!(out, "  %fn_ptr = load ptr, ptr %env").unwrap();
    writeln!(out, "  %r = call i128 %fn_ptr(ptr %env)").unwrap();
    writeln!(out, "  %v = bitcast i128 %r to <16 x i8>").unwrap();
    writeln!(out, "  ret <16 x i8> %v").unwrap();
    writeln!(out, "}}").unwrap();
    out
}

/// Resolves a fn-address local to the name its defining `gos_fn_addr("name")`
/// references, within `body`. The lowering assigns the address directly, so a
/// single pass over the body's statements suffices. A runtime symbol is not
/// one of ours to rewire and answers `None`.
fn resolve_fn_addr_target(body: &Body, target: gossamer_mir::Local) -> Option<String> {
    use gossamer_mir::{ConstValue, Operand, Rvalue, StatementKind};
    for block in &body.blocks {
        for stmt in &block.stmts {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            if place.local != target || !place.projection.is_empty() {
                continue;
            }
            let Rvalue::CallIntrinsic { name, args } = rvalue else {
                continue;
            };
            if *name != "gos_fn_addr" {
                continue;
            }
            if let Some(Operand::Const(ConstValue::Str(hname))) = args.first()
                && !hname.starts_with("gos_rt_")
            {
                return Some(hname.clone());
            }
        }
    }
    None
}

/// As [`resolve_fn_addr_target`], restricted to a body of this unit.
///
/// A `__fn_thunk_*` shape thunk is linkonce-synthesized rather than lowered
/// from MIR, and one thunk serves every call site of its shape, so a
/// name-keyed answer would reach uses that need no rewiring. Which sites do is
/// [`collect_cabi_thunk_sites`]'s question.
fn resolve_fn_addr_name(body: &Body, target: gossamer_mir::Local) -> Option<String> {
    resolve_fn_addr_target(body, target).filter(|name| !name.starts_with("__fn_thunk_"))
}

/// The integer literal `op` names, either directly or through a local bound
/// to `Use(Const(Int(n)))` (a lowering that needs the value in a local writes
/// it as a separate `let n = <literal>` statement first).
pub(crate) fn operand_const_int(body: &Body, op: &gossamer_mir::Operand) -> Option<i128> {
    use gossamer_mir::{ConstValue, Operand, Rvalue, StatementKind};
    match op {
        Operand::Const(ConstValue::Int(n)) => Some(*n),
        Operand::Copy(p) if p.projection.is_empty() => {
            for block in &body.blocks {
                for stmt in &block.stmts {
                    if let StatementKind::Assign { place, rvalue } = &stmt.kind
                        && place.local == p.local
                        && place.projection.is_empty()
                        && let Rvalue::Use(Operand::Const(ConstValue::Int(n))) = rvalue
                    {
                        return Some(*n);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// True when `op` is the integer literal 0 (the closure-env builder writes
/// the callable offset as a separate `let zero = 0` local before the
/// `gos_store`).
fn operand_is_zero_offset(body: &Body, op: &gossamer_mir::Operand) -> bool {
    operand_const_int(body, op) == Some(0)
}

/// The locals a closure env reaches through plain copies, starting with the
/// local itself. A call hands over the binding the closure was bound to, while
/// the `gos_store` that placed the callable names the local the env blob was
/// built in, so the two are the same env under different names.
fn env_copy_aliases(body: &Body, target: gossamer_mir::Local) -> Vec<gossamer_mir::Local> {
    use gossamer_mir::{Operand, Rvalue, StatementKind};
    let mut aliases = vec![target];
    let mut next = 0;
    while next < aliases.len() {
        let current = aliases[next];
        next += 1;
        for block in &body.blocks {
            for stmt in &block.stmts {
                let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                    continue;
                };
                if place.local != current || !place.projection.is_empty() {
                    continue;
                }
                let Rvalue::Use(Operand::Copy(src)) = rvalue else {
                    continue;
                };
                if !src.projection.is_empty() || aliases.contains(&src.local) {
                    continue;
                }
                aliases.push(src.local);
            }
        }
    }
    aliases
}

/// For a closure env-blob local, resolves the callable stored at offset 0 -
/// the `gos_store(env, 0, gos_fn_addr("name"))` the lowering emits when it
/// builds the env. Answers the local the address lands in and the name it
/// references, so a caller can key on the site as well as the name.
fn resolve_env_slot0_addr(
    body: &Body,
    env_local: gossamer_mir::Local,
) -> Option<(gossamer_mir::Local, String)> {
    use gossamer_mir::{Operand, Rvalue, StatementKind};
    let envs = env_copy_aliases(body, env_local);
    for block in &body.blocks {
        for stmt in &block.stmts {
            let StatementKind::Assign { rvalue, .. } = &stmt.kind else {
                continue;
            };
            let Rvalue::CallIntrinsic { name, args } = rvalue else {
                continue;
            };
            if *name != "gos_store" {
                continue;
            }
            let [Operand::Copy(env), off, Operand::Copy(fn_addr)] = args.as_slice() else {
                continue;
            };
            if !envs.contains(&env.local) || !env.projection.is_empty() {
                continue;
            }
            if !operand_is_zero_offset(body, off) {
                continue;
            }
            if let Some(hname) = resolve_fn_addr_target(body, fn_addr.local) {
                return Some((fn_addr.local, hname));
            }
        }
    }
    None
}

/// Runtime registration shims that store a gossamer handler's
/// `gos_fn_addr` and later invoke it as `extern "C" fn(..) -> i128`,
/// mapped to the fn-addr argument's position in the shim's signature.
/// Every stored callback crosses the rustc/LLVM i128-return boundary,
/// so on Win64 it must be registered through its `<16 x i8>` `$cabi`
/// thunk. On every target the collected names also identify functions
/// entered directly from the Rust runtime, whose opaque request params
/// arrive as raw pointers.
const CABI_HANDLER_REGISTRATIONS: &[(&str, usize)] = gossamer_abi::I128_HANDLER_REGISTRATIONS;

/// Bodies that answer a multi-slot inline aggregate through storage the caller
/// names, rather than a heap block of their own.
///
/// `GOS_NO_SRET` disables the convention for differential measurement and as a
/// safety escape hatch, the way `GOS_RC_NO_ELIDE` does for the elider.
///
/// The convention only holds where every call to the body is one this module
/// emits. A body whose address is taken can be reached through a function
/// pointer, by another Gossamer body or by a runtime shim holding a callback,
/// and such a call site writes the argument list from the pointer's type,
/// which says nothing about a trailing slot. Those keep the heap return.
pub(super) fn collect_sret_bodies(
    all_bodies: &[Body],
    tcx: &TyCtxt,
) -> std::collections::BTreeSet<String> {
    use gossamer_mir::{ConstValue, Operand, Rvalue, StatementKind, Terminator};

    let name_of_def = |def: gossamer_resolve::DefId| -> Option<&str> {
        all_bodies
            .iter()
            .find(|b| b.def == Some(def))
            .map(|b| b.name.as_str())
    };
    let mut address_taken: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let note = |op: &Operand, taken: &mut std::collections::BTreeSet<String>| {
        if let Operand::FnRef { def, .. } = op
            && let Some(name) = name_of_def(*def)
        {
            taken.insert(name.to_string());
        }
    };
    for body in all_bodies {
        for block in &body.blocks {
            for stmt in &block.stmts {
                let StatementKind::Assign { rvalue, .. } = &stmt.kind else {
                    continue;
                };
                match rvalue {
                    Rvalue::Use(op) => note(op, &mut address_taken),
                    Rvalue::Aggregate { operands, .. } => {
                        for op in operands {
                            note(op, &mut address_taken);
                        }
                    }
                    Rvalue::CallIntrinsic { name, args } => {
                        // `gos_fn_addr("f")` is the spelling that puts a body's
                        // address in a value.
                        if *name == "gos_fn_addr"
                            && let Some(Operand::Const(ConstValue::Str(fname))) = args.first()
                        {
                            address_taken.insert(fname.clone());
                        }
                        for op in args {
                            note(op, &mut address_taken);
                        }
                    }
                    _ => {}
                }
            }
            // A call's own callee is a direct one; every other operand of the
            // call puts the body it names into a value.
            if let Terminator::Call { args, .. } = &block.terminator {
                for op in args {
                    note(op, &mut address_taken);
                }
            }
        }
    }

    if std::env::var_os("GOS_NO_SRET").is_some() {
        return std::collections::BTreeSet::new();
    }
    all_bodies
        .iter()
        .filter(|body| {
            body.name != "main"
                && !address_taken.contains(&body.name)
                && crate::lower::sret_return_bytes(tcx, body.local_ty(gossamer_mir::Local::RETURN))
                    .is_some()
        })
        .map(|body| body.name.clone())
        .collect()
}

/// Collects the gossamer functions invoked by the rustc-compiled runtime
/// through `extern "C" fn(..) -> i128`, mapped to their parameter arity:
/// handler registrations (the [`CABI_HANDLER_REGISTRATIONS`] table, keyed
/// by the fn-addr argument position) and the closure callbacks of the
/// i128-returning std combinators (whose address sits at offset 0 of the env
/// blob passed to the helper). The Win64 ABI returns the 2-word `i128` in xmm0,
/// but a gossamer `define i128`/`ret i128` returns it in the GP-register pair,
/// so each collected function needs a `<16 x i8>` return thunk taken in place
/// of its raw address on that target.
pub(super) fn collect_cabi_handlers(
    all_bodies: &[Body],
) -> std::collections::BTreeMap<String, usize> {
    use gossamer_mir::{ConstValue, Operand, Terminator};
    let mut handlers = std::collections::BTreeMap::new();
    let arity_of = |name: &str| -> usize {
        all_bodies
            .iter()
            .find(|b| b.name == name)
            .map_or(2, |b| b.arity as usize)
    };
    for body in all_bodies {
        for block in &body.blocks {
            let Terminator::Call { callee, args, .. } = &block.terminator else {
                continue;
            };
            let Operand::Const(ConstValue::Str(sym)) = callee else {
                continue;
            };
            if let Some((_, addr_idx)) = CABI_HANDLER_REGISTRATIONS
                .iter()
                .find(|(shim, _)| *shim == sym.as_str())
            {
                if let Some(Operand::Copy(addr_place)) = args.get(*addr_idx)
                    && let Some(hname) = resolve_fn_addr_name(body, addr_place.local)
                {
                    let arity = arity_of(&hname);
                    handlers.insert(hname, arity);
                }
            } else if gossamer_abi::reads_carrier_from_callback(sym.as_str()) {
                for arg in args {
                    let Operand::Copy(env_place) = arg else {
                        continue;
                    };
                    if let Some((_, hname)) = resolve_env_slot0_addr(body, env_place.local)
                        && !hname.starts_with("__fn_thunk_")
                    {
                        let arity = arity_of(&hname);
                        handlers.insert(hname, arity);
                    }
                }
            }
        }
    }
    handlers
}

/// Where a `gos_fn_addr` hands a shared shape thunk's address to a runtime
/// shim that reads a two-word carrier back from it: for each body, the locals
/// the address lands in, mapped to the thunk they name.
///
/// A bare fn or a non-capturing closure reaches a callback slot through a
/// `__fn_thunk_<inputs>_<ret>` shape thunk, which is synthesized once per
/// shape and serves every call site of it. The Win64 vector return is
/// therefore not a property the thunk's name can carry - a Gossamer-invoked
/// use of the same shape reads the carrier from the register pair. The site
/// is what knows, so the site is what the redirect keys on.
type CabiThunkSites =
    std::collections::BTreeMap<String, std::collections::BTreeMap<gossamer_mir::Local, String>>;

/// Collects the [`CabiThunkSites`] of `all_bodies`.
pub(super) fn collect_cabi_thunk_sites(all_bodies: &[Body]) -> CabiThunkSites {
    use gossamer_mir::{ConstValue, Operand, Terminator};
    let mut sites = CabiThunkSites::new();
    for body in all_bodies {
        for block in &body.blocks {
            let Terminator::Call { callee, args, .. } = &block.terminator else {
                continue;
            };
            let Operand::Const(ConstValue::Str(sym)) = callee else {
                continue;
            };
            if !gossamer_abi::reads_carrier_from_callback(sym.as_str()) {
                continue;
            }
            for arg in args {
                let Operand::Copy(env_place) = arg else {
                    continue;
                };
                if let Some((local, hname)) = resolve_env_slot0_addr(body, env_place.local)
                    && hname.starts_with("__fn_thunk_")
                    && shape_thunk_answers_carrier(&hname)
                {
                    sites
                        .entry(body.name.clone())
                        .or_default()
                        .insert(local, hname);
                }
            }
        }
    }
    sites
}

/// Whether a shape thunk answers the two-word carrier - shape character `r`,
/// the one return shape whose register differs between the two ABIs.
fn shape_thunk_answers_carrier(name: &str) -> bool {
    name.strip_prefix("__fn_thunk_")
        .and_then(|suffix| suffix.rsplit_once('_'))
        .is_some_and(|(_, ret)| ret == "r")
}

/// The LLVM parameter types a shape thunk declares: the env pointer, then one
/// per input shape character. `None` when the name is not a shape a thunk is
/// rendered for.
pub(super) fn shape_thunk_param_tys(name: &str) -> Option<Vec<String>> {
    let suffix = name.strip_prefix("__fn_thunk_")?;
    let (inputs, _) = suffix.rsplit_once('_')?;
    let mut tys = vec!["ptr".to_string()];
    for c in inputs.chars() {
        tys.push(shape_char_to_llvm_ty(c)?.to_string());
    }
    Some(tys)
}

/// The LLVM parameter types a `$cabi` thunk forwards: the handler's own, so
/// each argument keeps the register class the handler reads it from - an
/// `f64` element reaches its callback in an SSE register, which a `ptr` slot
/// would move to the GP file. A handler with no body in this unit falls back
/// to `arity` pointer words.
pub(super) fn cabi_thunk_param_tys(
    all_bodies: &[Body],
    tcx: &TyCtxt,
    name: &str,
    arity: usize,
) -> Vec<String> {
    let Some(body) = all_bodies.iter().find(|b| b.name == name) else {
        return vec!["ptr".to_string(); arity];
    };
    (0..body.arity)
        .map(|i| crate::ty::param_llvm_ty(tcx, body.local_ty(gossamer_mir::Local(i + 1))))
        .collect()
}

/// Renders the Win64 handler-return thunk `define <16 x i8> @"name$cabi"` -
/// it forwards every argument to the real handler `@"name"` (which returns
/// the 2-word `i128` in the GP-register pair) and re-emits the value as
/// `<16 x i8>` so the rustc runtime reads it from xmm0.
/// Emitted in exactly the one chunk that owns the handler body (never duplicated).
pub(super) fn render_cabi_handler_thunk(name: &str, param_tys: &[String]) -> String {
    render_cabi_thunk_with_linkage(name, param_tys, "")
}

/// Body shared by both `$cabi` thunk renderers; `linkage` is the empty string
/// for a handler's own thunk, emitted once in the chunk that owns the body,
/// and `"linkonce_odr "` for a shape thunk's, which every chunk reaching that
/// shape renders and the linker deduplicates.
pub(super) fn render_cabi_thunk_with_linkage(
    name: &str,
    param_tys: &[String],
    linkage: &str,
) -> String {
    let params: Vec<String> = param_tys
        .iter()
        .enumerate()
        .map(|(i, ty)| format!("{ty} %a{i}"))
        .collect();
    let mut out = String::new();
    let comdat = linkonce_comdat(&mut out, linkage, &format!("{name}$cabi"));
    let _ = writeln!(
        out,
        "define {linkage}<16 x i8> @\"{name}$cabi\"({}){comdat} {{",
        params.join(", ")
    );
    writeln!(out, "entry:").unwrap();
    let _ = writeln!(out, "  %r = call i128 @\"{name}\"({})", params.join(", "));
    writeln!(out, "  %v = bitcast i128 %r to <16 x i8>").unwrap();
    writeln!(out, "  ret <16 x i8> %v").unwrap();
    writeln!(out, "}}").unwrap();
    out
}

/// Writes the `comdat` declaration a definition of `symbol` with `linkage`
/// needs and answers the suffix its `define` carries. COFF coalesces a
/// `linkonce_odr` definition emitted by several objects only through a COMDAT
/// section of its own; ELF and Mach-O coalesce it from the linkage alone, and
/// Mach-O has no COMDATs.
fn linkonce_comdat(out: &mut String, linkage: &str, symbol: &str) -> &'static str {
    match coff_comdat_decl(linkage, symbol) {
        Some(decl) => {
            let _ = writeln!(out, "{decl}");
            " comdat"
        }
        None => "",
    }
}

/// The module-level `$"symbol" = comdat any` a `linkonce_odr` definition of
/// `symbol` pairs with on a COFF target, where it is the only way the linker
/// keeps one copy; `None` for any other linkage or target.
pub(crate) fn coff_comdat_decl(linkage: &str, symbol: &str) -> Option<String> {
    comdat_decl_for(linkage, symbol, target_is_windows())
}

fn comdat_decl_for(linkage: &str, symbol: &str, coff: bool) -> Option<String> {
    (coff && linkage.trim_end() == "linkonce_odr").then(|| format!("$\"{symbol}\" = comdat any"))
}

/// Synthesises an LLVM `define` for a per-shape callable thunk
/// named `__fn_thunk_<inputs>_<ret>`. The thunk loads the real
/// fn pointer from `env+8` and forwards the typed arguments
/// with the matching calling convention. Mirrors the Cranelift
/// backend's `define_shape_thunk` so capturing closures and
/// fn-item refs flow through identical lowering.
pub(super) fn render_shape_thunk(name: &str) -> Option<String> {
    render_shape_thunk_with_linkage(name, "")
}

/// Body shared by both shape-thunk renderers; `linkage` is the empty string
/// for a plain `define` and `"linkonce_odr "` for the per-body modules.
pub(super) fn render_shape_thunk_with_linkage(name: &str, linkage: &str) -> Option<String> {
    let suffix = name.strip_prefix("__fn_thunk_")?;
    let (inputs_str, ret_str) = suffix.rsplit_once('_')?;
    let ret_char = ret_str.chars().next()?;
    let ret_ty = shape_char_to_llvm_ty(ret_char)?;
    let mut input_tys: Vec<&'static str> = Vec::with_capacity(inputs_str.len());
    for c in inputs_str.chars() {
        input_tys.push(shape_char_to_llvm_ty(c)?);
    }
    let unit_ret = ret_char == 'u';
    // A narrow integer, `bool`, or `char` result is answered as a whole word:
    // a caller reading the word the runtime's callback types declare would
    // otherwise see whatever the narrow return left in the upper bits.
    let widened = shape_char_widens(ret_char);
    let mut out = String::new();
    let header_ret = if unit_ret {
        "void"
    } else if widened {
        "i64"
    } else {
        ret_ty
    };
    let mut params = String::from("ptr %env");
    for (i, t) in input_tys.iter().enumerate() {
        let _ = write!(params, ", {t} %a{i}");
    }
    let comdat = linkonce_comdat(&mut out, linkage, name);
    let _ = writeln!(
        out,
        "define {linkage}{header_ret} @\"{name}\"({params}){comdat} {{"
    );
    writeln!(out, "entry:").unwrap();
    writeln!(out, "  %fn_ptr_addr = getelementptr i8, ptr %env, i64 8").unwrap();
    writeln!(out, "  %fn_ptr = load ptr, ptr %fn_ptr_addr").unwrap();
    // A `bool` crosses the ABI as a whole byte holding the canonical `0` or
    // `1` that callers store and compare against, while the body LLVM defines
    // takes and returns the `i1` its branches are built on. The thunk is where
    // the two meet: narrow on the way in, zero-extend on the way out.
    let mut call_args = String::new();
    for (i, (t, c)) in input_tys.iter().zip(inputs_str.chars()).enumerate() {
        if i > 0 {
            call_args.push_str(", ");
        }
        if c == 'b' {
            let _ = writeln!(out, "  %b{i} = trunc i8 %a{i} to i1");
            let _ = write!(call_args, "i1 %b{i}");
        } else if c == 'q' {
            // The caller hands the address of a two-word carrier's storage;
            // the callee's parameter is the carrier itself.
            let _ = writeln!(out, "  %c{i} = load i128, ptr %a{i}");
            let _ = write!(call_args, "i128 %c{i}");
        } else {
            let _ = write!(call_args, "{t} %a{i}");
        }
    }
    if unit_ret {
        let _ = writeln!(out, "  call void %fn_ptr({call_args})");
        writeln!(out, "  ret void").unwrap();
    } else if ret_char == 'b' {
        let _ = writeln!(out, "  %r = call i1 %fn_ptr({call_args})");
        let _ = writeln!(out, "  %rw = zext i1 %r to i64");
        let _ = writeln!(out, "  ret i64 %rw");
    } else if widened {
        let extension = if matches!(ret_char, 'y' | 'k' | 'j') {
            "sext"
        } else {
            "zext"
        };
        let _ = writeln!(out, "  %r = call {ret_ty} %fn_ptr({call_args})");
        let _ = writeln!(out, "  %rw = {extension} {ret_ty} %r to i64");
        let _ = writeln!(out, "  ret i64 %rw");
    } else {
        let _ = writeln!(out, "  %r = call {ret_ty} %fn_ptr({call_args})");
        let _ = writeln!(out, "  ret {ret_ty} %r");
    }
    writeln!(out, "}}").unwrap();
    Some(out)
}

/// Whether a thunk with this return shape answers a word widened from a
/// narrower integer.
fn shape_char_widens(c: char) -> bool {
    matches!(c, 'b' | 'c' | 'y' | 'Y' | 'k' | 'K' | 'j' | 'J')
}

/// Maps a shape character produced by
/// `gossamer_mir::mangle_callable_shape` to its LLVM IR type
/// name. Mirrors `shape_char_to_cl_type` on the Cranelift side.
fn shape_char_to_llvm_ty(c: char) -> Option<&'static str> {
    Some(match c {
        'q' => "ptr",
        'b' | 'y' | 'Y' => "i8",
        'k' | 'K' => "i16",
        'c' | 'j' | 'J' => "i32",
        'i' => "i64",
        'f' => "double",
        'g' => "float",
        'u' => "i64",
        // 2-word packed Result/Option.
        'r' => "i128",
        _ => return None,
    })
}

pub(super) fn validate_global_decl_shape(g: &str) -> Result<()> {
    let trimmed = g.trim_start();
    let valid = trimmed.starts_with('@')
        || trimmed.starts_with('$')
        || trimmed.starts_with("declare ")
        || trimmed.starts_with("define internal ");
    if !valid {
        return Err(anyhow!(
            "llvm backend: malformed module-level entry (expected `@symbol = ...`, \
             `$comdat = ...`, `declare ...`, or `define internal ...`, got: {snippet:?}). This is the same shape regression that \
             caused the 2026-04-28 / 2026-04-30 silent Cranelift-fallback incidents.",
            snippet = if trimmed.len() > 80 {
                &trimmed[..80]
            } else {
                trimmed
            }
        ));
    }
    Ok(())
}

pub(super) fn extern_declare_with(
    body: &Body,
    tcx: &TyCtxt,
    sret_bodies: &std::collections::BTreeSet<String>,
) -> String {
    let mut decl = extern_declare(body, tcx);
    if sret_bodies.contains(&body.name) {
        let close = decl.rfind(')').expect("declare ends with a parameter list");
        let open = decl.find('(').expect("declare has a parameter list");
        let sep = if close == open + 1 { "" } else { ", " };
        decl.insert_str(close, &format!("{sep}ptr"));
    }
    decl
}

fn extern_declare(body: &Body, tcx: &TyCtxt) -> String {
    let ret_ty = crate::ty::render_ty(tcx, body.local_ty(gossamer_mir::Local::RETURN));
    let mut params = String::new();
    for i in 0..body.arity {
        if i > 0 {
            params.push_str(", ");
        }
        let local = gossamer_mir::Local(i + 1);
        let p_ty = crate::ty::param_llvm_ty(tcx, body.local_ty(local));
        let _ = write!(params, "{p_ty}");
    }
    format!(
        "declare {ret_ty} @\"{name}\"({params})\n",
        name = crate::lower::mangle_fn_name(&body.name)
    )
}

#[cfg(test)]
mod shape_validation_tests {
    use super::validate_global_decl_shape;

    #[test]
    fn accepts_constant_definition() {
        let g = "@.str_0 = private unnamed_addr constant [6 x i8] c\"hello\\00\"";
        assert!(validate_global_decl_shape(g).is_ok());
    }

    #[test]
    fn accepts_extern_global() {
        let g = "@GOS_RT_STDOUT_LEN = external local_unnamed_addr global i64";
        assert!(validate_global_decl_shape(g).is_ok());
    }

    #[test]
    fn accepts_function_declaration() {
        let g = "declare void @gos_rt_print_str(ptr)";
        assert!(validate_global_decl_shape(g).is_ok());
    }

    #[test]
    fn rejects_bare_identifier() {
        // The exact regression shape: a runtime symbol name
        // accidentally inserted as a bare string instead of a
        // full `@name = constant ...` declaration.
        let g = "gos_rt_arena_save";
        let err = validate_global_decl_shape(g).unwrap_err();
        assert!(
            err.to_string().contains("malformed module-level entry"),
            "expected shape diagnostic, got: {err}"
        );
    }

    #[test]
    fn rejects_random_text() {
        let g = "this is not LLVM IR";
        assert!(validate_global_decl_shape(g).is_err());
    }
}

#[cfg(test)]
mod cabi_thunk_tests {
    use super::render_cabi_handler_thunk;

    #[test]
    fn coff_linkonce_definition_pairs_with_an_any_comdat() {
        assert_eq!(
            super::comdat_decl_for("linkonce_odr ", "__fn_thunk_i_r$cabi", true).as_deref(),
            Some("$\"__fn_thunk_i_r$cabi\" = comdat any"),
        );
    }

    #[test]
    fn non_coff_or_strong_definition_takes_no_comdat() {
        assert_eq!(
            super::comdat_decl_for("linkonce_odr ", "__fn_thunk_i_i", false),
            None
        );
        assert_eq!(super::comdat_decl_for("", "App::serve", true), None);
    }

    /// `render_cabi_handler_thunk` must emit a plain `define`, not
    /// `define linkonce_odr`. On ELF, `linkonce_odr` deduplicates across
    /// translation units implicitly, but lld-link (Windows COFF) requires an
    /// explicit COMDAT section for dedup and treats bare `linkonce_odr` as a
    /// duplicate strong symbol when the same thunk appears in multiple chunks.
    /// The fix emits the thunk once (in the chunk that owns the handler body),
    /// so `linkonce_odr` is no longer needed - and no longer safe on COFF.
    #[test]
    fn cabi_thunk_uses_plain_define_not_linkonce_odr() {
        let ir = render_cabi_handler_thunk("App::serve", &["ptr".to_string(), "ptr".to_string()]);
        assert!(
            ir.contains("define <16 x i8>"),
            "expected plain `define`, got:\n{ir}"
        );
        assert!(
            !ir.contains("linkonce_odr"),
            "must not use linkonce_odr (causes duplicate-symbol on Windows COFF lld-link):\n{ir}"
        );
    }

    /// The thunk stands between the runtime and the handler, so each argument
    /// must keep the register class the handler reads it from: an `f64`
    /// element arrives in an SSE register, which a `ptr` slot would move to
    /// the GP file and leave the handler reading an unset register.
    #[test]
    fn cabi_thunk_keeps_each_argument_in_its_own_register_class() {
        let ir =
            render_cabi_handler_thunk("__closure_0", &["i64".to_string(), "double".to_string()]);
        assert!(
            ir.contains("define <16 x i8> @\"__closure_0$cabi\"(i64 %a0, double %a1)"),
            "the thunk declares the handler's own parameter types:\n{ir}"
        );
        assert!(
            ir.contains("call i128 @\"__closure_0\"(i64 %a0, double %a1)"),
            "and forwards them unchanged:\n{ir}"
        );
    }

    #[test]
    fn cabi_thunk_calls_the_real_handler_and_bitcasts() {
        let ir = render_cabi_handler_thunk("Proxy::serve", &["ptr".to_string(), "ptr".to_string()]);
        assert!(
            ir.contains("call i128 @\"Proxy::serve\""),
            "must call real handler"
        );
        assert!(
            ir.contains("bitcast i128"),
            "must bitcast i128 to <16 x i8>"
        );
        assert!(ir.contains("ret <16 x i8>"), "must return <16 x i8>");
    }

    /// Every runtime shim that invokes a gossamer callback as
    /// `extern "C" fn(..) -> i128` must be collected, so the callback is
    /// reached through its `<16 x i8>` thunk on Win64. `gos_rt_fs_walk_dir_raw`
    /// takes its visitor as an env blob whose slot 0 holds the callable,
    /// the same shape as the i128 combinators.
    #[test]
    fn walk_dir_visitor_is_collected_as_a_cabi_handler() {
        let handlers = super::collect_cabi_handlers(&[env_callback_body("gos_rt_fs_walk_dir_raw")]);
        assert!(
            handlers.contains_key("visit"),
            "walk_dir visitor must be collected, got: {handlers:?}"
        );
    }

    /// A bare fn or a non-capturing closure reaches a carrier-reading shim
    /// through a shared shape thunk, which is not a body of this unit: it is
    /// collected as a site, so the redirect reaches that use and no other.
    #[test]
    fn a_shape_thunk_callback_is_collected_as_a_site_not_a_handler() {
        let body = named_env_callback_body("gos_rt_option_and_then", "__fn_thunk_i_r");
        assert!(
            super::collect_cabi_handlers(std::slice::from_ref(&body)).is_empty(),
            "a shape thunk is not a handler body"
        );
        let sites = super::collect_cabi_thunk_sites(std::slice::from_ref(&body));
        assert_eq!(
            sites.get("main").and_then(|s| s.values().next()),
            Some(&"__fn_thunk_i_r".to_string()),
            "the site that hands the thunk over must be collected: {sites:?}"
        );
    }

    /// Only a carrier answer crosses in a different register, so a shape that
    /// answers a word is left with its own address.
    #[test]
    fn a_word_answering_shape_thunk_is_not_a_site() {
        let body = named_env_callback_body("gos_rt_option_and_then", "__fn_thunk_i_i");
        assert!(super::collect_cabi_thunk_sites(std::slice::from_ref(&body)).is_empty());
    }

    /// The wrapper's parameters are the shape thunk's own: the env pointer,
    /// then one per input character.
    #[test]
    fn a_shape_thunk_wrapper_declares_the_thunks_parameters() {
        assert_eq!(
            super::shape_thunk_param_tys("__fn_thunk_if_r"),
            Some(vec![
                "ptr".to_string(),
                "i64".to_string(),
                "double".to_string()
            ])
        );
    }

    /// A closure bound to a name reaches its shim through that binding, which
    /// holds a copy of the local the env blob was built in - and the
    /// `gos_store` that placed the callable names the latter. Both spell the
    /// same env, so the callable resolves through the copy.
    #[test]
    fn an_env_reached_through_a_binding_is_collected_as_a_cabi_handler() {
        let handlers =
            super::collect_cabi_handlers(&[copied_env_callback_body("gos_rt_fs_walk_dir_raw")]);
        assert!(
            handlers.contains_key("visit"),
            "an env handed over through a binding must still resolve its callable, got: {handlers:?}"
        );
    }

    /// `spawn(f)` hands the runtime a callable the runtime invokes as
    /// `extern "C-unwind" fn(usize) -> i128` - the same crossing the
    /// combinators make, so a two-word spawn needs the Win64 forwarding
    /// thunk.
    #[test]
    fn two_word_spawn_needs_the_win64_forwarding_thunk() {
        assert!(super::body_has_wide_spawn(&spawn_body(2)));
    }

    /// A one-word spawn callable is invoked as `-> i64`, which already
    /// agrees on the GP register: routing it through the `<16 x i8>` thunk
    /// would hand the runtime a vector register it never reads.
    #[test]
    fn one_word_spawn_stays_on_the_gp_register() {
        assert!(!super::body_has_wide_spawn(&spawn_body(1)));
    }

    /// The thunk reads the callable at slot 0 of the env blob - the same
    /// address the spawn lowering passes - and re-emits its `i128` in the
    /// vector register.
    #[test]
    fn spawn_wide_thunk_forwards_slot_zero_and_bitcasts() {
        let ir = super::render_spawn_wide_cabi_thunk();
        assert!(ir.contains("load ptr, ptr %env"), "{ir}");
        assert!(ir.contains("call i128 %fn_ptr(ptr %env)"), "{ir}");
        assert!(ir.contains("ret <16 x i8>"), "{ir}");
    }

    /// Builds a body shaped like the `spawn(f)` lowering: the callable's
    /// address sits at offset 0 of the env blob, and `ret_words` reaches
    /// the shim through a local bound to a literal.
    fn spawn_body(ret_words: i128) -> gossamer_mir::Body {
        use gossamer_mir::{ConstValue, Operand, Place, Rvalue, StatementKind, Terminator};
        let mut body = env_callback_body("gos_rt_spawn_ex");
        let block = &mut body.blocks[0];
        let words = gossamer_mir::Local(5);
        let span = block.span;
        block.stmts.push(gossamer_mir::Statement {
            kind: StatementKind::Assign {
                place: Place::local(words),
                rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(ret_words))),
            },
            span,
            inlined: None,
        });
        if let Terminator::Call { args, .. } = &mut block.terminator {
            args.push(Operand::Copy(Place::local(words)));
            args.push(Operand::Const(ConstValue::Int(0)));
        }
        body
    }

    /// The env-blob callback body with the env handed over through a binding
    /// of its own: the blob is built in one local and copied into the local
    /// the call names, the shape a closure bound to a name lowers to.
    fn copied_env_callback_body(shim: &str) -> gossamer_mir::Body {
        use gossamer_mir::{Local, Operand, Place, Rvalue, Statement, StatementKind, Terminator};
        let mut body = env_callback_body(shim);
        let block = &mut body.blocks[0];
        let span = block.span;
        let (env, binding) = (Local(2), Local(6));
        block.stmts.push(Statement {
            kind: StatementKind::Assign {
                place: Place::local(binding),
                rvalue: Rvalue::Use(Operand::Copy(Place::local(env))),
            },
            span,
            inlined: None,
        });
        if let Terminator::Call { args, .. } = &mut block.terminator {
            args[1] = Operand::Copy(Place::local(binding));
        }
        body
    }

    /// Builds a body shaped like the env-blob callback lowering: the
    /// callable's address is stored at offset 0 of the env, and the env is
    /// handed to `shim` as its second argument.
    fn env_callback_body(shim: &str) -> gossamer_mir::Body {
        named_env_callback_body(shim, "visit")
    }

    fn named_env_callback_body(shim: &str, callable: &str) -> gossamer_mir::Body {
        use gossamer_lex::{SourceMap, Span};
        use gossamer_mir::{
            BasicBlock, BlockId, Body, ConstValue, Local, Operand, Place, Rvalue, Statement,
            StatementKind, Terminator,
        };

        let mut map = SourceMap::new();
        let span = Span::new(map.add_file("walk.gos", ""), 0, 0);
        let (root, env, addr) = (Local(1), Local(2), Local(3));
        let assign = |place: Local, rvalue: Rvalue| Statement {
            kind: StatementKind::Assign {
                place: Place::local(place),
                rvalue,
            },
            span,
            inlined: None,
        };
        Body {
            name: "main".to_string(),
            def: None,
            arity: 0,
            locals: Vec::new(),
            blocks: vec![BasicBlock {
                id: BlockId(0),
                stmts: vec![
                    assign(
                        addr,
                        Rvalue::CallIntrinsic {
                            name: "gos_fn_addr",
                            args: vec![Operand::Const(ConstValue::Str(callable.to_string()))],
                        },
                    ),
                    assign(
                        Local(4),
                        Rvalue::CallIntrinsic {
                            name: "gos_store",
                            args: vec![
                                Operand::Copy(Place::local(env)),
                                Operand::Const(ConstValue::Int(0)),
                                Operand::Copy(Place::local(addr)),
                            ],
                        },
                    ),
                ],
                terminator: Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(shim.to_string())),
                    args: vec![
                        Operand::Copy(Place::local(root)),
                        Operand::Copy(Place::local(env)),
                    ],
                    destination: Place::local(Local(0)),
                    target: None,
                },
                span,
                terminator_span: None,
                terminator_inlined: None,
            }],
            span,
        }
    }
}
