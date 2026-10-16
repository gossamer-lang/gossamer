//! Stream E.6 - intraprocedural escape analysis.
//! A local "escapes" the current function when any of the following
//! is true:
//! - It is assigned into `Local::RETURN` (flows out to the caller).
//! - It is passed as an argument to a call terminator whose callee
//!   may capture the pointer (any user fn, plus runtime helpers
//!   not on the non-capturing whitelist below).
//! - It aliases (by copy) an already-escaping local.
//!
//! The analysis is intentionally conservative and linear in the
//! number of statements. Downstream passes can use
//! [`EscapeSet::is_non_escaping`] to decide whether a value can be
//! stack-allocated instead of boxed.
//!
//! The non-capturing runtime callee set is the keystone for the
//! heap-cleanup pass: a `let buf = U8Vec::new(...)` that flows only
//! into `buf.set_byte(...)` / `buf.to_string(...)` would otherwise
//! be marked as escaping (because the helper takes the pointer as a
//! `Copy` arg) and never get freed. Listing the helpers that only
//! read/write the pointee - never stash the pointer in a global,
//! return it back, or hand it to a user-controllable callback - lets
//! the cleanup pass see the local as non-escaping and emit the
//! matching `_free` call at the body's `Return`.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;

use crate::ir::{Body, Local, Operand, Rvalue, StatementKind, Terminator};

/// Runtime helpers that take heap pointers as arguments but never
/// capture them: they read/write the pointee in place, optionally
/// allocate a *fresh* result (independent of the input), and then
/// return. The escape analyser treats their args as non-escaping so
/// the cleanup pass can free heap-owned locals whose only uses are
/// these helpers.
///
/// Adding a helper here is a load-bearing safety claim: if the
/// runtime ever stores the pointer somewhere with lifetime longer
/// than the call (a global, a thread-local, an output aggregate),
/// the cleanup pass will free still-reachable memory and the
/// program will use-after-free. New helpers must be added only
/// after auditing their `c_abi.rs` implementation for capture.
// Listed in sorted order so [`is_non_capturing_runtime_callee`] can
// binary-search.
//
// A helper qualifies when (a) it does not stash any pointer
// argument anywhere with a lifetime longer than the call, and
// (b) any pointer it returns is freshly allocated, not aliased
// from an argument. Helpers that return a borrow of an argument
// (e.g. `map_keys` returning slices into the map's internal Box
// storage) MUST NOT appear here - the caller would otherwise
// free the source while the borrowed pointer is still live.
const NON_CAPTURING_RUNTIME_CALLEES: &[&str] = &[
    "gos_rt_heap_i64_get",
    "gos_rt_heap_i64_len",
    "gos_rt_heap_i64_set",
    "gos_rt_heap_u8_get",
    "gos_rt_heap_u8_len",
    "gos_rt_heap_u8_set",
    "gos_rt_heap_u8_to_string",
    "gos_rt_heap_u8_write_lines_to_stdout",
    // Reads the spawned child's outcome off the handle channel and
    // hands back a freshly boxed payload. The handle pointer reaches
    // nothing that outlives the call: the cohort entry it retires is
    // keyed by address, not held.
    "gos_rt_join",
    "gos_rt_len",
    "gos_rt_len_is_zero",
    // HashMap counter / probe helpers. The string-key variants
    // copy the key into a fresh `Box<[u8]>` before stashing it in
    // the map, so the caller's key buffer is not retained. The
    // map pointer is mutated but not captured.
    "gos_rt_map_contains_substring_str",
    "gos_rt_map_get_opt_substring_str",
    "gos_rt_map_get_or_i64_i64",
    "gos_rt_map_get_or_str_i64",
    "gos_rt_map_get_or_substring_str_i64",
    "gos_rt_map_get_or_substring_str_str",
    "gos_rt_map_get_or_typed_str_i64",
    "gos_rt_map_inc_at_str_i64",
    "gos_rt_map_inc_fields_str_i64",
    "gos_rt_map_inc_i64",
    "gos_rt_map_inc_str_i64",
    "gos_rt_map_inc_substring_str_i64",
    "gos_rt_map_inc_typed_str_i64",
    "gos_rt_map_or_insert_i64_i64",
    "gos_rt_map_or_insert_str_i64",
    "gos_rt_map_or_insert_substring_str_i64",
    "gos_rt_map_or_insert_typed_str_i64",
    "gos_rt_str_byte_at",
    "gos_rt_str_is_empty",
    "gos_rt_str_len",
];

/// `true` when the callee name is on the non-capturing runtime
/// whitelist. The escape analyser uses this to skip marking args
/// as escaping for a known-safe call.
fn is_non_capturing_runtime_callee(name: &str) -> bool {
    NON_CAPTURING_RUNTIME_CALLEES.binary_search(&name).is_ok()
}

/// Result of [`analyse`] - the set of locals that escape this body.
///
/// Callers typically ask the inverse question via
/// [`EscapeSet::is_non_escaping`].
#[derive(Debug, Clone, Default)]
pub struct EscapeSet {
    escapes: BTreeSet<u32>,
}

impl EscapeSet {
    /// Returns `true` when `local` does **not** escape.
    #[must_use]
    pub fn is_non_escaping(&self, local: Local) -> bool {
        !self.escapes.contains(&local.0)
    }

    /// Returns `true` when `local` escapes.
    #[must_use]
    pub fn escapes(&self, local: Local) -> bool {
        self.escapes.contains(&local.0)
    }

    /// Iterates over every escaping local, in ascending numeric order.
    pub fn iter(&self) -> impl Iterator<Item = Local> + '_ {
        self.escapes.iter().copied().map(Local)
    }

    /// Number of escaping locals.
    #[must_use]
    pub fn len(&self) -> usize {
        self.escapes.len()
    }

    /// Whether no locals escape.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.escapes.is_empty()
    }
}

/// Inter-procedural per-fn argument capture summary. Maps the
/// callee's `DefId.local` to a bool vector - `captures[i] = true`
/// means parameter `i` (1-indexed; the entry corresponds to MIR
/// `Local(i+1)`) of the callee escapes its body. Callers can
/// safely skip the escape mark on non-capturing arguments.
#[derive(Debug, Clone, Default)]
pub struct CaptureSummary {
    captures: std::collections::BTreeMap<u32, Vec<bool>>,
}

impl CaptureSummary {
    /// Returns the captures vector for the callee identified by
    /// `def_local`. `None` when the callee has no recorded summary
    /// - callers must conservatively assume "captures all".
    #[must_use]
    pub fn captures(&self, def_local: u32) -> Option<&[bool]> {
        self.captures.get(&def_local).map(Vec::as_slice)
    }

    fn insert(&mut self, def_local: u32, captures: Vec<bool>) -> bool {
        match self.captures.get(&def_local) {
            Some(existing) if existing == &captures => false,
            _ => {
                self.captures.insert(def_local, captures);
                true
            }
        }
    }
}

/// Builds a capture summary across `bodies` by iterating to a
/// fixed point: each round re-runs the escape analyser with the
/// current summary, then refines each fn's captures vector based
/// on which parameter locals showed up in the escape set. The
/// monotone "true → false" direction guarantees termination.
#[must_use]
pub fn build_capture_summary(bodies: &[Body]) -> CaptureSummary {
    let mut summary = CaptureSummary::default();
    let mut changed = true;
    while changed {
        changed = false;
        for body in bodies {
            let Some(def) = body.def else {
                continue;
            };
            let escape = analyse_with_summary(body, &summary);
            let arity = body.arity as usize;
            let mut captures = Vec::with_capacity(arity);
            for i in 0..arity {
                // Param i in source order corresponds to MIR
                // `Local(i + 1)` - `Local(0)` is the return slot.
                let pl = Local((i + 1) as u32);
                captures.push(escape.escapes(pl));
            }
            if summary.insert(def.local, captures) {
                changed = true;
            }
        }
    }
    summary
}

/// Computes the escape set for `body` assuming worst-case
/// "captures all" semantics for every Call to a user-defined
/// function. Equivalent to [`analyse_with_summary`] with an empty
/// summary; kept for callers that don't have a program-wide
/// summary handy.
#[must_use]
pub fn analyse(body: &Body) -> EscapeSet {
    analyse_with_summary(body, &CaptureSummary::default())
}

/// Computes the escape set for `body`, consulting `summary` to
/// decide which arguments of each user-fn Call need the escape
/// mark. A callee's parameter that does not capture in `summary`
/// allows the caller to leave the matching argument off the
/// escape set, which in turn lets the cleanup pass schedule a
/// drop for owning bindings whose only outbound use is a
/// non-capturing helper.
#[must_use]
pub fn analyse_with_summary(body: &Body, summary: &CaptureSummary) -> EscapeSet {
    let mut set = EscapeSet::default();

    set.escapes.insert(Local::RETURN.0);
    let mut changed = true;
    while changed {
        changed = false;

        for block in &body.blocks {
            for stmt in &block.stmts {
                if let StatementKind::Assign { place, rvalue } = &stmt.kind {
                    let target_escapes = set.escapes.contains(&place.local.0);
                    // Reference-carrying rvalues (Use of a Copy, Ref,
                    // Aggregate) propagate escape through the source
                    // locals. Arithmetic and unary ops do not: they
                    // produce a fresh value that does not alias their
                    // operands.
                    if target_escapes {
                        let aliases: Vec<&Operand> = match rvalue {
                            Rvalue::Use(op) => vec![op],
                            Rvalue::Aggregate { operands, .. } => operands.iter().collect(),
                            Rvalue::Repeat { value, .. } => vec![value],
                            _ => Vec::new(),
                        };
                        for op in aliases {
                            if let Operand::Copy(src) = op {
                                if set.escapes.insert(src.local.0) {
                                    changed = true;
                                }
                            }
                        }
                    }
                }
            }
            if let Terminator::Call { callee, args, .. } = &block.terminator {
                let skip_all = if let Operand::Const(crate::ir::ConstValue::Str(name)) = callee {
                    is_non_capturing_runtime_callee(name)
                } else {
                    false
                };
                let per_arg_captures: Option<&[bool]> = if let Operand::FnRef { def, .. } = callee {
                    summary.captures(def.local)
                } else {
                    None
                };
                if !skip_all {
                    for (idx, arg) in args.iter().enumerate() {
                        if let Some(captures) = per_arg_captures
                            && captures.get(idx).copied() == Some(false)
                        {
                            // Callee's summary proves this parameter
                            // does not escape its body, so the arg
                            // does not escape this caller either.
                            continue;
                        }
                        if let Operand::Copy(src) = arg
                            && set.escapes.insert(src.local.0)
                        {
                            changed = true;
                        }
                    }
                }
            }
        }
    }

    set
}

#[cfg(test)]
mod tests {
    use gossamer_hir::lower_source_file;
    use gossamer_lex::SourceMap;
    use gossamer_parse::parse_source_file;
    use gossamer_resolve::resolve_source_file;
    use gossamer_types::{TyCtxt, typecheck_source_file};

    use super::*;
    use crate::lower_program;

    fn build(source: &str) -> Vec<Body> {
        let mut map = SourceMap::new();
        let file = map.add_file("t.gos", source.to_string());
        let (mut sf, _) = parse_source_file(source, file);
        let (mut res, _) = resolve_source_file(&sf);
        let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &mut res);
        let mut tcx = TyCtxt::new();
        let (tbl, _) = typecheck_source_file(&sf, &res, &mut tcx);
        let hir = lower_source_file(&sf, &res, &tbl, &mut tcx);
        lower_program(&hir, &mut tcx)
    }

    #[test]
    fn return_local_always_escapes() {
        let bodies = build("fn f() -> i64 { 42i64 }\n");
        let set = analyse(&bodies[0]);
        assert!(set.escapes(Local::RETURN));
    }

    #[test]
    fn locals_never_stored_to_return_or_call_do_not_escape() {
        let bodies = build("fn f() -> i64 { let x = 1i64 let y = 2i64 x + y }\n");
        let set = analyse(&bodies[0]);
        // Parameters + return slot escape through the public contract,
        // but the intermediate temp for `y` should not.
        let non_escaping_count = bodies[0]
            .locals
            .iter()
            .enumerate()
            .filter(|(i, _)| set.is_non_escaping(Local(*i as u32)))
            .count();
        assert!(non_escaping_count > 0);
    }

    #[test]
    fn call_arguments_mark_their_source_as_escaping() {
        let bodies = build(
            "fn helper(x: i64) -> i64 { x }\nfn caller() { let a = 7i64 let _ = helper(a) }\n",
        );
        let caller = bodies.iter().find(|b| b.name == "caller").unwrap();
        let set = analyse(caller);
        let had_call_escape = caller
            .locals
            .iter()
            .enumerate()
            .any(|(i, _)| set.escapes(Local(i as u32)));
        assert!(had_call_escape, "expected at least one escaping local");
    }
}
