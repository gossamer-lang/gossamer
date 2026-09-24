#![allow(missing_docs)]

// A value whose last use hands it to a new holder pays for one share, not
// two, and a copy of a value nothing else observes is a handoff. The tests
// read the lowered body rather than a benchmark number, so a regression
// names itself.

mod common;

use common::lower;
use gossamer_mir::{Body, ConstValue, Operand, Rvalue, StatementKind, Terminator};

fn body<'a>(bodies: &'a [Body], name: &str) -> &'a Body {
    bodies
        .iter()
        .find(|body| body.name == name)
        .unwrap_or_else(|| panic!("no body {name}"))
}

/// How many times `body` calls the runtime helper `name`, as an intrinsic
/// statement or a call terminator.
fn count_calls(body: &Body, name: &str) -> usize {
    let statements = body
        .blocks
        .iter()
        .flat_map(|block| &block.stmts)
        .filter(|stmt| {
            matches!(
                &stmt.kind,
                StatementKind::Assign { rvalue: Rvalue::CallIntrinsic { name: n, .. }, .. }
                    if *n == name
            )
        })
        .count();
    let calls = body
        .blocks
        .iter()
        .filter(|block| {
            matches!(
                &block.terminator,
                Terminator::Call { callee: Operand::Const(ConstValue::Str(n)), .. } if n == name
            )
        })
        .count();
    statements + calls
}

#[test]
fn a_unique_local_pays_no_retain_and_one_release() {
    let (bodies, _) = lower("fn f() -> i64 {\n let xs = #[1, 2, 3]\n xs.len()\n}\n");
    let f = body(&bodies, "f");
    assert_eq!(count_calls(f, "gos_rt_vec_retain"), 0);
    assert_eq!(
        count_calls(f, "gos_rt_vec_free"),
        1,
        "one release at the end of scope"
    );
}

#[test]
fn a_value_read_after_it_is_stored_stores_a_copy() {
    let (bodies, _) =
        lower("fn f() -> i64 {\n let xs = #[1]\n let both = #[xs]\n both.len() + xs.len()\n}\n");
    let f = body(&bodies, "f");
    assert!(count_calls(f, "gos_rt_vec_clone") >= 1);
    assert_eq!(count_calls(f, "gos_rt_vec_retain"), 0);
}

#[test]
fn a_value_stored_by_its_last_use_hands_over_its_share() {
    let (bodies, _) = lower("fn f() -> i64 {\n let xs = #[1]\n let both = #[xs]\n both.len()\n}\n");
    assert_eq!(count_calls(body(&bodies, "f"), "gos_rt_vec_retain"), 0);
}

#[test]
fn a_copy_of_a_unique_vector_nothing_reads_again_is_a_handoff() {
    let (bodies, _) = lower(
        "fn build() -> Vec<i64> { #[1, 2] }\nfn f() -> i64 {\n let xs = build()\n let ys = xs\n ys.len()\n}\n",
    );
    assert_eq!(count_calls(body(&bodies, "f"), "gos_rt_vec_clone"), 0);
}

#[test]
fn a_row_of_aggregates_stored_by_its_last_use_is_handed_over() {
    let (bodies, _) = lower(
        "struct Cell { alive: bool, ns: Vec<i64> }\nfn f(h: i64, w: i64) -> i64 {\n let mut cells: Vec<Vec<Cell>> = #[]\n for _ in 0..h {\n let mut row: Vec<Cell> = #[]\n for _ in 0..w { row.push(Cell { alive: false, ns: #[] }) }\n cells.push(row)\n }\n cells.len()\n}\n",
    );
    assert_eq!(count_calls(body(&bodies, "f"), "gos_rt_vec_clone"), 0);
}

#[test]
fn a_row_of_constructed_values_stored_by_its_last_use_is_handed_over() {
    let (bodies, _) = lower(
        "struct Cell { alive: bool, ns: Vec<i64> }\nimpl Cell {\n fn new(a: bool) -> Cell { Cell { alive: a, ns: Vec::with_capacity(8) } }\n}\nfn f(h: i64, w: i64) -> i64 {\n let mut cells: Vec<Vec<Cell>> = #[]\n for _ in 0..h {\n let mut row: Vec<Cell> = #[]\n for _ in 0..w { row.push(Cell::new(false)) }\n cells.push(row)\n }\n cells.len()\n}\n",
    );
    assert_eq!(count_calls(body(&bodies, "f"), "gos_rt_vec_clone"), 0);
}

#[test]
fn a_copy_of_a_vector_read_again_stays_a_copy() {
    let (bodies, _) = lower(
        "fn build() -> Vec<i64> { #[1, 2] }\nfn f() -> i64 {\n let xs = build()\n let mut ys = xs\n ys.push(3)\n ys.len() + xs.len()\n}\n",
    );
    assert_eq!(count_calls(body(&bodies, "f"), "gos_rt_vec_clone"), 1);
}

#[test]
fn a_constructor_hands_its_fields_to_the_value_it_answers() {
    let (bodies, _) = lower(
        "struct Prog { ops: Vec<i64>, name: String }\nimpl Prog {\n fn new(n: i64) -> Prog {\n let mut ops = #[]\n for i in 0..n { ops.push(i) }\n let name = \"p\".repeat(2)\n Prog { ops: ops, name: name }\n }\n}\n",
    );
    let new = body(&bodies, "Prog::new");
    assert_eq!(count_calls(new, "gos_rt_vec_retain"), 0);
    assert_eq!(count_calls(new, "gos_rt_rc_retain"), 0);
    assert_eq!(count_calls(new, "gos_rt_str_retain_typed"), 0);
}

#[test]
fn a_block_with_both_kinds_of_handoff_keeps_the_value_it_builds() {
    // The consuming call's null lands at the start of the next block, which
    // also holds an aggregate whose operands hand over their shares. Both
    // rewrites must name the statements they were found at.
    let (bodies, _) = lower(
        "struct P { name: String, tags: Vec<String> }\nfn e() -> i64 {\n let p = P { name: \"a\", tags: #[\"x\", \"y\"] }\n let q = p\n q.tags.len() + q.name.len()\n}\n",
    );
    let e = body(&bodies, "e");
    let aggregates = e
        .blocks
        .iter()
        .flat_map(|block| &block.stmts)
        .filter(|stmt| {
            matches!(
                &stmt.kind,
                StatementKind::Assign { rvalue: Rvalue::Aggregate { operands, .. }, .. }
                    if operands.len() == 2
            )
        })
        .count();
    assert_eq!(
        aggregates, 1,
        "the struct literal must survive the rewrite: {e:#?}"
    );
}

#[test]
fn an_element_read_out_of_a_container_has_no_share_to_hand_over() {
    // `row` reads an element `rows` still owns. The retain before the push is
    // a new share for `c`, not one `row` could give up, so it must stay.
    let (bodies, _) = lower(
        "fn f(rows: Vec<Vec<i64>>) -> i64 {\n let mut c: Vec<Vec<i64>> = #[]\n for row in rows { c.push(row) }\n c.len()\n}\n",
    );
    assert!(count_calls(body(&bodies, "f"), "gos_rt_vec_retain") >= 1);
}

#[test]
fn a_local_read_out_of_a_container_keeps_its_retain() {
    let (bodies, _) = lower(
        "fn f() -> i64 {\n let rows = #[#[1], #[2]]\n let mut c: Vec<Vec<i64>> = #[]\n for row in rows { c.push(row) }\n c.len()\n}\n",
    );
    assert!(count_calls(body(&bodies, "f"), "gos_rt_vec_retain") >= 1);
}

#[test]
fn a_store_paid_for_twice_keeps_its_accounting() {
    // The field store is followed by a retain of the stored string and one of
    // the field. Which of them is the field's share cannot be told apart, so
    // neither moves and the string is released once per store.
    let (bodies, _) = lower(
        "struct Doc { rendered: String, total: i64 }\nfn f(n: i64) -> i64 {\n let mut d = Doc { rendered: \"\", total: 0 }\n let mut i = 0\n while i < n {\n let mut r = String::with_capacity(16)\n r.push_str(\"row \")\n d.rendered = r\n d.total += d.rendered.byte_len()\n i += 1\n }\n d.total\n}\n",
    );
    assert!(count_calls(body(&bodies, "f"), "gos_rt_str_retain_typed") >= 1);
}
