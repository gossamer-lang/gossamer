//! Item-level dead-code elimination: drop the bodies no root can reach.
//!
//! Everything after lowering is paid per body - the optimisation passes,
//! the RC insertion, the IR text, and LLVM's own work on it - so a
//! program that defines a library's worth of functions and calls three
//! of them pays for the whole library on every build.
//!
//! Reachability is computed over MIR rather than HIR because MIR states
//! every edge as data: a call names its callee, a function value names
//! the function, and a name-keyed dispatch carries the name as a string
//! constant. The last is the one with no call edge to follow - a handler
//! registered by name is reached through `gos_fn_addr("handler")` - so
//! any string constant that spells a body's name is an edge.

use std::collections::{HashMap, HashSet};

use crate::ir::{Body, ConstValue, Operand, Rvalue, StatementKind, Terminator};

/// Environment variable that turns the pass off, so a suspected
/// miscompile can be bisected against a build that lowered everything.
const DISABLE: &str = "GOS_MIR_NO_DCE";

/// How many bodies a prune kept and dropped, for `gos build -v`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PruneReport {
    /// Bodies the roots reach, which are the ones still in the vector.
    pub kept: usize,
    /// Bodies removed.
    pub pruned: usize,
}

/// Removes every body no root reaches, in place.
///
/// `roots` names the bodies that are reachable by definition - the entry
/// function, the test and benchmark items a test build runs, and a
/// library's exported surface. A name in `roots` that no body carries is
/// ignored: the caller states an intent, not a fact about this program.
pub fn prune_unreachable(bodies: &mut Vec<Body>, roots: &[String]) -> PruneReport {
    prune_scoped(bodies, roots, Scope::Exact)
}

/// How much of the graph is trusted, which depends on whether
/// specialisation has run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    /// Every edge in the program is present, so a body no root reaches
    /// is unreachable.
    Exact,
    /// Specialisation has not run yet, so a trait call through a type
    /// parameter has not become a call to a concrete impl method. Those
    /// edges do not exist to be followed, and every method body is kept
    /// whatever the graph says. A free function is still pruned: a
    /// specialisation rewrites trait-method calls, which are named with
    /// `::`, and cannot conjure a reference to a free function no live
    /// body already named.
    BeforeSpecialisation,
}

/// [`prune_unreachable`] with the trust in the graph stated.
pub fn prune_scoped(bodies: &mut Vec<Body>, roots: &[String], scope: Scope) -> PruneReport {
    prune_unreachable_enabled(bodies, roots, scope, std::env::var_os(DISABLE).is_none())
}

/// [`prune_unreachable`] with the switch supplied rather than read, so
/// the pass is a pure function of its inputs.
fn prune_unreachable_enabled(
    bodies: &mut Vec<Body>,
    roots: &[String],
    scope: Scope,
    enabled: bool,
) -> PruneReport {
    if !enabled {
        return PruneReport {
            kept: bodies.len(),
            pruned: 0,
        };
    }
    let live = reachable(bodies, roots, scope);
    // A program in which no root resolves to a body is not one whose
    // reachable set is empty - it is one whose roots cannot be named
    // here, which is what a library unit or a lowering harness with no
    // entry looks like. Dropping everything would be answering a
    // question that was not asked.
    if !roots
        .iter()
        .any(|root| bodies.iter().any(|b| b.name == *root))
    {
        return PruneReport {
            kept: bodies.len(),
            pruned: 0,
        };
    }
    let before = bodies.len();
    bodies.retain(|body| live.contains(&body.name));
    PruneReport {
        kept: bodies.len(),
        pruned: before - bodies.len(),
    }
}

/// The transitive closure of `roots` over the bodies' call graph.
fn reachable(bodies: &[Body], roots: &[String], scope: Scope) -> HashSet<String> {
    let by_name: HashMap<&str, &Body> = bodies.iter().map(|b| (b.name.as_str(), b)).collect();
    // A `FnRef` names its callee by `DefId`; the body it refers to is the
    // one that carries that `DefId`. A monomorphised copy carries none,
    // and is reached by the mangled name its call sites were rewritten
    // to, which is an ordinary string edge.
    let by_def: HashMap<u32, &str> = bodies
        .iter()
        .filter_map(|b| b.def.map(|d| (d.local, b.name.as_str())))
        .collect();

    let mut live: HashSet<String> = HashSet::new();
    let mut queue: Vec<&str> = Vec::new();
    let keep_methods = scope == Scope::BeforeSpecialisation;
    for name in roots.iter().map(String::as_str).chain(
        bodies
            .iter()
            .map(|b| b.name.as_str())
            .filter(|name| is_rendering(name) || (keep_methods && name.contains("::"))),
    ) {
        if let Some(body) = by_name.get(name)
            && live.insert(body.name.clone())
        {
            queue.push(body.name.as_str());
        }
    }
    while let Some(name) = queue.pop() {
        let Some(body) = by_name.get(name) else {
            continue;
        };
        let mut reached: Vec<&str> = Vec::new();
        for_each_operand(body, &mut |operand| match operand {
            Operand::FnRef { def, .. } => {
                if let Some(target) = by_def.get(&def.local) {
                    reached.push(target);
                }
            }
            Operand::Const(ConstValue::Str(text)) => {
                // Every string that spells a body's name is an edge,
                // whether it is a callee spelled by name, a handler
                // registered through `gos_fn_addr`, or a message that
                // happens to read like one. The last costs a body that
                // did not have to be kept; missing one of the first two
                // costs a call into nothing.
                if let Some(target) = by_name.get(text.as_str()) {
                    reached.push(target.name.as_str());
                }
            }
            Operand::Copy(_) | Operand::Const(_) => {}
        });
        for name in reached {
            if live.insert(name.to_string()) {
                queue.push(name);
            }
        }
    }
    live
}

/// Whether `name` is a rendering method, which is a root whatever calls
/// it.
///
/// `{}` and `{:?}` reach a type's own `to_string` and `fmt` through a
/// symbol the backend builds from the *type* being rendered, not from
/// anything the MIR names: the descriptor a container's format shim
/// travels with carries the method by index into a table looked up that
/// way. There is no edge to follow, and pruning one turns a rendering
/// the program does perform into a build failure.
fn is_rendering(name: &str) -> bool {
    // A formatter reaches an instantiation of a rendering method by the name
    // monomorphisation gave it, so the instantiation is a root as well.
    let base = name.split("$mono$").next().unwrap_or(name);
    base.ends_with("::fmt") || base.ends_with("::to_string")
}

/// Calls `f` on every operand `body` names.
///
/// Exhaustive on purpose: a `_` arm here would silently stop following
/// an edge the day a new MIR shape carries an operand, and a missed edge
/// is a call into a body that was pruned.
fn for_each_operand(body: &Body, f: &mut impl FnMut(&Operand)) {
    for block in &body.blocks {
        for statement in &block.stmts {
            match &statement.kind {
                StatementKind::Assign { rvalue, .. } => for_each_rvalue_operand(rvalue, f),
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
                for arg in args {
                    f(arg);
                }
            }
            Terminator::SwitchInt { discriminant, .. } => f(discriminant),
            Terminator::Assert { cond, .. } => f(cond),
            Terminator::Goto { .. }
            | Terminator::Return
            | Terminator::Unreachable
            | Terminator::Panic { .. }
            | Terminator::Drop { .. } => {}
        }
    }
}

fn for_each_rvalue_operand(rvalue: &Rvalue, f: &mut impl FnMut(&Operand)) {
    match rvalue {
        Rvalue::Use(operand)
        | Rvalue::UnaryOp { operand, .. }
        | Rvalue::Cast { operand, .. }
        | Rvalue::Repeat { value: operand, .. } => f(operand),
        Rvalue::BinaryOp { lhs, rhs, .. } => {
            f(lhs);
            f(rhs);
        }
        Rvalue::Aggregate { operands, .. } | Rvalue::CallIntrinsic { args: operands, .. } => {
            for operand in operands {
                f(operand);
            }
        }
        Rvalue::Len(_) | Rvalue::Ref { .. } | Rvalue::StaticLoad(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{BasicBlock, BlockId, Local, LocalDecl, Place, Statement};
    use gossamer_types::TyCtxt;

    fn body(name: &str, calls: &[&str]) -> Body {
        let mut tcx = TyCtxt::new();
        let unit = tcx.unit();
        let blocks = vec![BasicBlock {
            id: BlockId::ENTRY,
            span: gossamer_lex::Span::default(),
            stmts: calls
                .iter()
                .map(|callee| Statement {
                    kind: StatementKind::Assign {
                        place: Place::local(Local(0)),
                        rvalue: Rvalue::Use(Operand::Const(ConstValue::Str((*callee).to_string()))),
                    },
                    span: gossamer_lex::Span::default(),
                    inlined: None,
                })
                .collect(),
            terminator: Terminator::Return,
            terminator_span: None,
            terminator_inlined: None,
        }];
        Body {
            name: name.to_string(),
            arity: 0,
            locals: vec![LocalDecl {
                ty: unit,
                debug_name: None,
                mutable: false,
                region: false,
            }],
            blocks,
            def: None,
            span: gossamer_lex::Span::default(),
        }
    }

    #[test]
    fn a_body_no_root_reaches_is_dropped() {
        let mut bodies = vec![
            body("main", &["used"]),
            body("used", &[]),
            body("dead", &[]),
        ];
        let report = prune_unreachable(&mut bodies, &["main".to_string()]);
        assert_eq!(report.kept, 2);
        assert_eq!(report.pruned, 1);
        let names: Vec<&str> = bodies.iter().map(|b| b.name.as_str()).collect();
        assert!(names.contains(&"main") && names.contains(&"used"));
        assert!(!names.contains(&"dead"));
    }

    /// A name that only ever appears as a string is exactly the shape a
    /// handler registered through `gos_fn_addr` has: no call edge, and a
    /// null call if it is pruned.
    #[test]
    fn a_body_named_only_by_a_string_constant_survives() {
        let mut bodies = vec![body("main", &["handler"]), body("handler", &[])];
        let report = prune_unreachable(&mut bodies, &["main".to_string()]);
        assert_eq!(report.pruned, 0);
    }

    #[test]
    fn reachability_is_transitive_and_survives_a_cycle() {
        let mut bodies = vec![
            body("main", &["a"]),
            body("a", &["b"]),
            body("b", &["a"]),
            body("dead", &["also_dead"]),
            body("also_dead", &[]),
        ];
        let report = prune_unreachable(&mut bodies, &["main".to_string()]);
        assert_eq!(report.kept, 3);
        assert_eq!(report.pruned, 2);
    }

    /// `{}` and `{:?}` reach a type's own rendering through a symbol
    /// derived from the type, so there is no edge to follow and pruning
    /// one turns a rendering the program performs into a build failure.
    #[test]
    fn a_rendering_method_is_a_root_without_being_called() {
        let mut bodies = vec![
            body("main", &[]),
            body("Part::fmt", &["Inner::to_string"]),
            body("Inner::to_string", &[]),
            body("Part::area", &[]),
        ];
        let report = prune_unreachable(&mut bodies, &["main".to_string()]);
        let names: Vec<&str> = bodies.iter().map(|b| b.name.as_str()).collect();
        assert!(names.contains(&"Part::fmt"), "{names:?}");
        assert!(names.contains(&"Inner::to_string"), "{names:?}");
        assert!(!names.contains(&"Part::area"), "{names:?}");
        assert_eq!(report.pruned, 1);
    }

    /// A unit with no entry at all - a library, or a lowering harness -
    /// keeps every body: nothing here can say what reaches what.
    #[test]
    fn a_program_with_no_root_at_all_keeps_every_body() {
        let mut bodies = vec![body("step", &[]), body("helper", &[])];
        let report = prune_unreachable(&mut bodies, &["main".to_string()]);
        assert_eq!(report.kept, 2);
        assert_eq!(report.pruned, 0);
    }

    #[test]
    fn a_root_no_body_carries_is_ignored() {
        let mut bodies = vec![body("main", &[])];
        let report = prune_unreachable(&mut bodies, &["main".to_string(), "absent".to_string()]);
        assert_eq!(report.kept, 1);
        assert_eq!(report.pruned, 0);
    }

    /// `GOS_MIR_NO_DCE=1` reaches the pass as this flag, so a suspected
    /// miscompile is bisected against a build that lowered everything.
    #[test]
    fn the_disable_switch_keeps_every_body() {
        let mut bodies = vec![body("main", &[]), body("dead", &[])];
        let report =
            prune_unreachable_enabled(&mut bodies, &["main".to_string()], Scope::Exact, false);
        assert_eq!(report.kept, 2);
        assert_eq!(report.pruned, 0);
    }
}
