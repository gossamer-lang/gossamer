#![allow(missing_docs)]

// A freshly constructed value with one holder is unique; anything that can
// reach it a second way is not.

mod common;

use common::lower;
use gossamer_mir::uniqueness::{CallSummaries, Point, Uniqueness, analyze};
use gossamer_mir::{Body, ConstValue, Local, Operand, Terminator};

fn local_named(body: &Body, name: &str) -> Local {
    let index = body
        .locals
        .iter()
        .position(|decl| decl.debug_name.as_ref().is_some_and(|id| id.name == name))
        .unwrap_or_else(|| panic!("no local named {name} in {}", body.name));
    Local(u32::try_from(index).expect("local index"))
}

fn return_point(body: &Body) -> Point {
    let block = body
        .blocks
        .iter()
        .find(|block| matches!(block.terminator, Terminator::Return))
        .expect("a return");
    Point {
        block: block.id,
        stmt: block.stmts.len(),
    }
}

fn call_point(body: &Body, callee: &str) -> Point {
    let block = body
        .blocks
        .iter()
        .find(|block| {
            matches!(
                &block.terminator,
                Terminator::Call { callee: Operand::Const(ConstValue::Str(name)), .. }
                    if name == callee
            )
        })
        .unwrap_or_else(|| panic!("no call to {callee} in {}", body.name));
    Point {
        block: block.id,
        stmt: block.stmts.len(),
    }
}

/// The uniqueness of `name` in function `func` at the point `point` picks.
fn uniqueness_of(
    source: &str,
    func: &str,
    name: &str,
    point: impl Fn(&Body) -> Point,
) -> Uniqueness {
    let (bodies, tcx) = lower(source);
    let summaries = CallSummaries::compute(&bodies, &tcx);
    let body = bodies
        .iter()
        .find(|body| body.name == func)
        .unwrap_or_else(|| panic!("no body {func}"));
    let facts = analyze(body, &tcx, &summaries);
    facts.at(point(body), local_named(body, name))
}

#[test]
fn a_freshly_built_vector_is_unique() {
    let u = uniqueness_of(
        "fn f() -> i64 {\n let xs = #[1, 2, 3]\n xs.len()\n}\n",
        "f",
        "xs",
        return_point,
    );
    assert_eq!(u, Uniqueness::Unique);
}

#[test]
fn a_vector_stored_in_a_container_it_is_read_after_stays_unique() {
    let u = uniqueness_of(
        "fn f() -> i64 {\n let xs = #[1, 2, 3]\n let outer = #[xs]\n outer.len() + xs.len()\n}\n",
        "f",
        "xs",
        return_point,
    );
    assert_eq!(u, Uniqueness::Unique);
}

#[test]
fn a_container_holding_a_copy_of_a_vector_read_after_it_stays_unique() {
    let u = uniqueness_of(
        "fn f() -> i64 {\n let xs = #[1, 2, 3]\n let outer = #[xs]\n outer.len() + xs.len()\n}\n",
        "f",
        "outer",
        |body| call_point(body, "gos_rt_len"),
    );
    assert_eq!(u, Uniqueness::Unique);
}

#[test]
fn a_vector_captured_by_a_closure_is_shared() {
    let u = uniqueness_of(
        "fn f() -> i64 {\n let xs = #[1]\n let add = || xs.len()\n add() + xs.len()\n}\n",
        "f",
        "xs",
        return_point,
    );
    assert_eq!(u, Uniqueness::Shared);
}

#[test]
fn a_vector_sent_to_a_goroutine_is_shared() {
    let u = uniqueness_of(
        "use std::errors\nfn f() -> Result<(), errors::Error> {\n cohort {\n let xs = #[1]\n spawn(|| xs.len())\n println(\"{}\", xs.len())\n }\n}\n",
        "f",
        "xs",
        return_point,
    );
    assert_eq!(u, Uniqueness::Shared);
}

#[test]
fn a_live_reference_makes_the_root_borrowed_not_unique() {
    let u = uniqueness_of(
        "fn edit(s: &mut String) { *s = \"b\" }\nfn f() -> i64 {\n let mut s = \"a\".repeat(2)\n edit(&mut s)\n s.len()\n}\n",
        "f",
        "s",
        |body| {
            let block = body
                .blocks
                .iter()
                .find(|block| {
                    matches!(
                        &block.terminator,
                        Terminator::Call {
                            callee: Operand::FnRef { .. },
                            ..
                        }
                    )
                })
                .expect("a call to edit");
            Point {
                block: block.id,
                stmt: block.stmts.len(),
            }
        },
    );
    assert_eq!(u, Uniqueness::Borrowed);
}

#[test]
fn a_value_returned_from_a_function_that_keeps_no_handle_stays_unique() {
    let u = uniqueness_of(
        "fn build() -> Vec<i64> { #[1, 2] }\nfn f() -> i64 {\n let xs = build()\n xs.len()\n}\n",
        "f",
        "xs",
        return_point,
    );
    assert_eq!(u, Uniqueness::Unique);
}

#[test]
fn a_value_a_function_also_stores_elsewhere_stays_unique() {
    let u = uniqueness_of(
        "struct Keep { items: Vec<Vec<i64>> }\nfn build(k: &mut Keep) -> Vec<i64> {\n let v = #[1, 2]\n k.items.push(v)\n v\n}\nfn f() -> i64 {\n let mut k = Keep { items: #[] }\n let xs = build(&mut k)\n xs.len() + k.items.len()\n}\n",
        "f",
        "xs",
        return_point,
    );
    assert_eq!(u, Uniqueness::Unique);
}

#[test]
fn a_parameter_is_shared_unless_the_caller_proved_otherwise() {
    let u = uniqueness_of(
        "fn f(xs: Vec<i64>) -> i64 { xs.len() }\n",
        "f",
        "xs",
        return_point,
    );
    assert_eq!(u, Uniqueness::Shared);
}

#[test]
fn a_recursive_function_that_returns_its_argument_answers_a_shared_value() {
    let source = "fn pass(xs: Vec<i64>, n: i64) -> Vec<i64> {\n if n == 0 { xs } else { pass(xs, n - 1) }\n}\nfn f() -> i64 {\n let a = #[1]\n let b = pass(a, 3)\n b.len() + a.len()\n}\n";
    let u = uniqueness_of(source, "f", "b", return_point);
    assert_eq!(u, Uniqueness::Shared);
}

/// The names of every runtime entry `func`'s final MIR calls.
fn called_names(source: &str, func: &str) -> Vec<String> {
    let (bodies, _) = lower(source);
    let body = bodies
        .iter()
        .find(|body| body.name == func)
        .unwrap_or_else(|| panic!("no body {func}"));
    body.blocks
        .iter()
        .filter_map(|block| match &block.terminator {
            Terminator::Call {
                callee: Operand::Const(ConstValue::Str(name)),
                ..
            } => Some(name.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_map_filled_by_inserts_stays_unique() {
    let u = uniqueness_of(
        "fn f() -> i64 {\n let mut m: Map<i64, i64> = Map::new()\n m.insert(1, 2)\n m.len()\n}\n",
        "f",
        "m",
        return_point,
    );
    assert_eq!(u, Uniqueness::Unique);
}

#[test]
fn a_map_stored_on_its_last_use_moves_instead_of_copying() {
    let names = called_names(
        "fn f(q: &mut Vec<Map<i64, i64>>) {\n let mut m: Map<i64, i64> = Map::new()\n m.insert(1, 2)\n q.push(m)\n}\n",
        "f",
    );
    assert!(
        !names.iter().any(|n| n == "gos_rt_map_clone"),
        "a map pushed on its last use is handed over, not copied: {names:?}"
    );
}

#[test]
fn a_deque_binding_takes_the_table_its_constructor_built() {
    let names = called_names(
        "use std::collections::Deque\nfn f() -> i64 {\n let mut d: Deque<i64> = Deque::new()\n d.push_back(1)\n d.len()\n}\n",
        "f",
    );
    assert!(
        !names.iter().any(|n| n == "gos_rt_deque_clone"),
        "a freshly built deque is bound without a copy: {names:?}"
    );
}
