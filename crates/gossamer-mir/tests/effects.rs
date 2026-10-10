#![allow(missing_docs)]

// Effect summaries and the bounds proofs that read them.

use gossamer_hir::{HirItemKind, HirProgram, lift_closures, lower_source_file};
use gossamer_lex::SourceMap;
use gossamer_mir::{
    Body, ConstValue, FnEffects, Operand, ProgramEffects, Terminator, collect_program_effects,
    lower_program, optimise, optimise_with_effects,
};
use gossamer_parse::autoderive::parse_with_autoderive;
use gossamer_resolve::resolve_source_file;
use gossamer_types::{TyCtxt, typecheck_source_file};

fn front_end(source: &str) -> (HirProgram, Vec<Body>, TyCtxt) {
    let mut map = SourceMap::new();
    let file = map.add_file("test.gos", source.to_string());
    let (mut sf, parse_diags) = parse_with_autoderive(source, file);
    assert!(parse_diags.is_empty(), "parse: {parse_diags:?}");
    let (resolutions, _) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &resolutions);
    let mut tcx = TyCtxt::new();
    let (table, diagnostics) = typecheck_source_file(&sf, &resolutions, &mut tcx);
    assert!(diagnostics.is_empty(), "typecheck: {diagnostics:?}");
    let hir = lower_source_file(&sf, &resolutions, &table, &mut tcx);
    let hir = lift_closures(hir, &mut tcx);
    let bodies = lower_program(&hir, &mut tcx);
    (hir, bodies, tcx)
}

fn summary_of(hir: &HirProgram, effects: &ProgramEffects, name: &str) -> FnEffects {
    let def = hir
        .items
        .iter()
        .find_map(|item| match &item.kind {
            HirItemKind::Fn(f) if f.name.name == name => item.def,
            _ => None,
        })
        .unwrap_or_else(|| panic!("no fn `{name}`"));
    effects
        .of_fn(def)
        .cloned()
        .unwrap_or_else(|| panic!("no summary for `{name}`"))
}

/// Index accesses of `body` as (checked, unchecked) counts.
fn access_counts(body: &Body) -> (usize, usize) {
    let mut counts = (0, 0);
    for block in &body.blocks {
        if let Terminator::Call {
            callee: Operand::Const(ConstValue::Str(name)),
            ..
        } = &block.terminator
        {
            match name.as_str() {
                "gos_rt_vec_get_i64" | "gos_rt_vec_set_i64" => counts.0 += 1,
                "gos_rt_vec_get_i64_unchecked" | "gos_rt_vec_set_i64_unchecked" => counts.1 += 1,
                _ => {}
            }
        }
    }
    counts
}

/// Optimises `name` with and without the program's summaries.
fn optimised(source: &str, name: &str) -> (Body, Body) {
    let (hir, bodies, tcx) = front_end(source);
    let effects = collect_program_effects(&hir, &tcx);
    let original = bodies
        .iter()
        .find(|b| b.name == name)
        .unwrap_or_else(|| panic!("no body `{name}`"))
        .clone();
    let mut with = original.clone();
    optimise_with_effects(&mut with, &tcx, &effects);
    let mut without = original;
    optimise(&mut without, &tcx);
    (with, without)
}

const HELPERS: &str = r#"
fn reads(xs: Vec<i64>, k: i64) -> i64 {
    let mut acc = 0
    for x in xs { acc += x * k }
    acc
}

fn grows(xs: &mut Vec<i64>, k: i64) { xs.push(k) }

fn writes(xs: &mut Vec<i64>, k: i64) { xs[0] = k }

fn forwards(xs: &mut Vec<i64>, k: i64) { writes(xs, k) }

fn forwards_growth(xs: &mut Vec<i64>, k: i64) { grows(xs, k) }

fn keeps(xs: Vec<i64>) -> Vec<Vec<i64>> { #[xs] }

fn answers(xs: Vec<i64>) -> Vec<i64> { xs }

fn spins(n: i64, xs: &mut Vec<i64>) -> i64 {
    if n == 0 { return 0 }
    xs[0] = n
    spins(n - 1, xs)
}

fn spins_and_grows(n: i64, xs: &mut Vec<i64>) -> i64 {
    if n == 0 {
        xs.push(1)
        return 0
    }
    spins_and_grows(n - 1, xs)
}

fn hands_off(xs: Vec<i64>) -> i64 {
    let f = |k: i64| k + xs.len()
    f(1)
}

struct Counter { hits: i64, log: Vec<String> }

impl Counter {
    fn bump(&mut self, by: i64) -> i64 {
        self.hits += by
        self.hits
    }

    fn record(&mut self, line: String) { self.log.push(line) }
}

fn main() {
    let mut xs = #[1, 2, 3]
    let mut c = Counter { hits: 0, log: Vec::new() }
    println(reads(xs, 2))
    grows(&mut xs, 4)
    writes(&mut xs, 5)
    forwards(&mut xs, 6)
    forwards_growth(&mut xs, 7)
    println(keeps(xs).len())
    println(answers(xs).len())
    println(spins(3, &mut xs))
    println(spins_and_grows(3, &mut xs))
    println(hands_off(xs))
    println(c.bump(2))
    c.record("x")
}
"#;

#[test]
fn a_reading_helper_leaves_its_parameter_untouched() {
    let (hir, _, tcx) = front_end(HELPERS);
    let effects = collect_program_effects(&hir, &tcx);
    let reads = summary_of(&hir, &effects, "reads");
    assert!(!reads.escapes);
    assert!(reads.param(0).leaves_untouched(), "{reads:?}");
    assert!(!reads.param(0).resizes);
}

#[test]
fn growth_and_element_writes_are_told_apart_through_forwarding() {
    let (hir, _, tcx) = front_end(HELPERS);
    let effects = collect_program_effects(&hir, &tcx);
    for name in ["grows", "forwards_growth"] {
        let s = summary_of(&hir, &effects, name);
        assert!(s.param(0).resizes, "{name}: {s:?}");
        assert!(s.param(0).stores_into, "{name}: {s:?}");
    }
    for name in ["writes", "forwards"] {
        let s = summary_of(&hir, &effects, name);
        assert!(!s.param(0).resizes, "{name}: {s:?}");
    }
}

#[test]
fn keeping_or_answering_a_parameter_retains_it() {
    let (hir, _, tcx) = front_end(HELPERS);
    let effects = collect_program_effects(&hir, &tcx);
    for name in ["keeps", "answers"] {
        let s = summary_of(&hir, &effects, name);
        assert!(s.param(0).retains, "{name}: {s:?}");
    }
}

#[test]
fn recursion_settles_at_its_own_fixed_point() {
    let (hir, _, tcx) = front_end(HELPERS);
    let effects = collect_program_effects(&hir, &tcx);
    let spins = summary_of(&hir, &effects, "spins");
    assert!(!spins.param(1).resizes, "{spins:?}");
    let grows = summary_of(&hir, &effects, "spins_and_grows");
    assert!(grows.param(1).resizes, "{grows:?}");
}

#[test]
fn a_closure_makes_a_function_escape() {
    let (hir, _, tcx) = front_end(HELPERS);
    let effects = collect_program_effects(&hir, &tcx);
    assert!(summary_of(&hir, &effects, "hands_off").escapes);
}

#[test]
fn methods_are_summarised_per_receiver_parameter() {
    let (hir, _, tcx) = front_end(HELPERS);
    let effects = collect_program_effects(&hir, &tcx);
    let bump = effects
        .of_symbol(None, "Counter::bump", 2)
        .expect("Counter::bump");
    assert!(bump.param(0).leaves_untouched(), "{bump:?}");
    let record = effects
        .of_symbol(None, "Counter::record", 2)
        .expect("Counter::record");
    assert!(record.param(0).stores_into, "{record:?}");
    assert!(record.param(1).retains, "{record:?}");
}

#[test]
fn same_named_methods_of_other_types_do_not_leak_into_each_other() {
    let source = r"
struct Lexer { pos: i64 }
struct Tape { cells: Vec<i64>, pos: i64 }

impl Lexer {
    fn advance(&mut self) { self.pos += 1 }
}

impl Tape {
    fn advance(&mut self) {
        self.pos += 1
        if self.pos >= self.cells.len() { self.cells.push(0) }
    }
}

fn main() {
    let mut l = Lexer { pos: 0 }
    let mut t = Tape { cells: #[0], pos: 0 }
    l.advance()
    t.advance()
    println(l.pos + t.pos)
}
";
    let (hir, _, tcx) = front_end(source);
    let effects = collect_program_effects(&hir, &tcx);
    let lexer = effects.of_symbol(None, "Lexer::advance", 1).expect("Lexer");
    assert!(lexer.param(0).leaves_untouched(), "{lexer:?}");
    let tape = effects.of_symbol(None, "Tape::advance", 1).expect("Tape");
    assert!(tape.param(0).stores_into, "{tape:?}");
}

const KERNELS: &str = r"
fn weigh(xs: Vec<i64>, k: i64) -> i64 {
    let mut acc = 0
    let mut j = 0
    while j < 8 {
        if k % 3 == 0 { acc += j * k } else { acc += k ^ j }
        j += 1
    }
    acc + xs.len()
}

fn bump(mut ys: Vec<i64>, k: i64) -> i64 {
    ys.push(k)
    ys.len()
}

fn touch(ys: &mut Vec<i64>, k: i64) -> i64 {
    ys[0] = k
    k
}

fn stretch(ys: &mut Vec<i64>, k: i64) -> i64 {
    if k == 0 { ys.push(1) }
    k
}

fn by_value(xs: Vec<i64>) -> i64 {
    let mut total = 0
    for i in 0..xs.len() { total +%= xs[i] + weigh(xs, i) }
    total
}

fn by_value_mutating_callee(xs: Vec<i64>) -> i64 {
    let mut total = 0
    for i in 0..xs.len() { total +%= xs[i] + bump(xs, i) }
    total
}

fn by_reference(xs: &mut Vec<i64>) -> i64 {
    let mut total = 0
    for i in 0..xs.len() { total +%= xs[i] + touch(xs, i) }
    total
}

fn by_reference_growing(xs: &mut Vec<i64>) -> i64 {
    let mut total = 0
    for i in 0..xs.len() { total +%= xs[i] + stretch(xs, i) }
    total
}

fn main() {
    let mut xs = #[1, 2, 3]
    println(by_value(xs))
    println(by_value_mutating_callee(xs))
    println(by_reference(&mut xs))
    println(by_reference_growing(&mut xs))
}
";

#[test]
fn a_by_value_call_keeps_the_counted_loop_proof() {
    let (with, without) = optimised(KERNELS, "by_value");
    assert_eq!(access_counts(&with), (0, 1), "with summaries");
    assert_eq!(
        access_counts(&without),
        (1, 0),
        "a by-value argument and a `&mut` one cross as the same handle"
    );
}

#[test]
fn a_call_site_copy_for_a_mutating_callee_keeps_the_proof() {
    let (with, _) = optimised(KERNELS, "by_value_mutating_callee");
    assert_eq!(access_counts(&with), (0, 1));
}

#[test]
fn a_reference_handed_to_a_length_preserving_callee_keeps_the_proof() {
    let (with, without) = optimised(KERNELS, "by_reference");
    assert_eq!(access_counts(&with), (0, 1), "the summary proves `touch`");
    assert_eq!(
        access_counts(&without),
        (1, 0),
        "nothing vets `touch` without it"
    );
}

#[test]
fn a_reference_handed_to_a_growing_callee_keeps_its_check() {
    let (with, _) = optimised(KERNELS, "by_reference_growing");
    assert_eq!(access_counts(&with).1, 0, "`stretch` may push");
}

/// Runs the release per-body pipeline over every body, then the program-level
/// entry-range pass, answering the finished bodies.
fn finished(source: &str) -> Vec<Body> {
    let (hir, mut bodies, tcx) = front_end(source);
    let effects = collect_program_effects(&hir, &tcx);
    for body in &mut bodies {
        optimise_with_effects(body, &tcx, &effects);
    }
    gossamer_mir::propagate_entry_bounds(&mut bodies, &tcx, &effects);
    bodies
}

fn body<'a>(bodies: &'a [Body], name: &str) -> &'a Body {
    bodies
        .iter()
        .find(|b| b.name == name)
        .unwrap_or_else(|| panic!("no body `{name}`"))
}

const ENTRY: &str = r"
fn process(xs: Vec<i64>, i: i64) -> i64 {
    let mut acc = xs[i]
    let mut j = 0
    while j < 4 {
        acc = (acc * 31 + j) % 1000003
        j += 1
    }
    acc + xs[i]
}

fn guarded(xs: Vec<i64>, i: i64) -> i64 {
    xs[i] * 2
}

fn named(xs: Vec<i64>, i: i64) -> i64 {
    xs[i] + 1
}

fn apply(f: Fn(Vec<i64>, i64) -> i64, xs: Vec<i64>) -> i64 { f(xs, 0) }

fn main() {
    let xs = #[3, 1, 4, 1, 5]
    let mut total = 0
    for i in 0..xs.len() {
        total += process(xs, i)
        total += guarded(xs, i)
        total += named(xs, i)
    }
    total += guarded(xs, 7)
    total += apply(named, xs)
    println(total)
}
";

#[test]
fn a_range_every_caller_proves_reaches_the_callee() {
    let bodies = finished(ENTRY);
    assert_eq!(access_counts(body(&bodies, "process")), (0, 2));
}

#[test]
fn one_unproven_call_site_keeps_the_callee_checked() {
    let bodies = finished(ENTRY);
    assert_eq!(access_counts(body(&bodies, "guarded")), (1, 0));
}

#[test]
fn a_function_used_as_a_value_keeps_its_checks() {
    let bodies = finished(ENTRY);
    assert_eq!(access_counts(body(&bodies, "named")), (1, 0));
}

#[test]
fn an_inlined_helper_indexing_its_own_parameter_keeps_the_proof() {
    let source = r"
fn pick(xs: Vec<i64>, i: i64) -> i64 { xs[i] * 3 + xs[i] }

fn main() {
    let xs = #[3, 1, 4, 1, 5]
    let mut total = 0
    for i in 0..xs.len() { total += pick(xs, i) }
    println(total)
}
";
    let (hir, mut bodies, tcx) = front_end(source);
    let effects = collect_program_effects(&hir, &tcx);
    gossamer_mir::inline_trivial_wrappers(&mut bodies);
    gossamer_mir::inline_small_callees(&mut bodies, &tcx);
    gossamer_mir::inline_general(&mut bodies, &tcx);
    let main = bodies.iter_mut().find(|b| b.name == "main").expect("main");
    optimise_with_effects(main, &tcx, &effects);
    let (checked, unchecked) = access_counts(main);
    assert_eq!(
        checked, 0,
        "the inlined parameter is a copy of the loop's vector"
    );
    assert!(unchecked >= 2, "{unchecked}");
}

#[test]
fn a_copy_taken_before_the_loop_is_not_the_loops_vector() {
    let source = r"
fn main() {
    let mut xs = #[3, 1, 4]
    let before = xs
    xs = #[9, 9, 9, 9, 9, 9]
    let mut total = 0
    for i in 0..xs.len() { total += before[i] }
    println(total)
}
";
    let (hir, mut bodies, tcx) = front_end(source);
    let effects = collect_program_effects(&hir, &tcx);
    let main = bodies.iter_mut().find(|b| b.name == "main").expect("main");
    optimise_with_effects(main, &tcx, &effects);
    // Versioning may add an unchecked copy behind a length check at loop
    // entry; the checked access must remain as its fallback.
    assert_eq!(access_counts(main).0, 1);
}
