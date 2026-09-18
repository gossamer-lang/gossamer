//! Purity is computed bottom-up over the call graph: a function is pure when
//! its body performs no effect and every function it calls is pure.

use gossamer_lex::SourceMap;
use gossamer_parse::parse_source_file;
use gossamer_resolve::resolve_source_file;
use gossamer_types::purity::{IMPURE_STDLIB_SUBMODULES, PURE_STDLIB_MODULES, PurityFacts, analyze};
use gossamer_types::{TyCtxt, check_parallel_adapters, typecheck_source_file};

fn purity_for(source: &str) -> PurityFacts {
    let mut map = SourceMap::new();
    let file = map.add_file("purity.gos".to_string(), source.to_string());
    let (mut parsed, parse_errors) = parse_source_file(source, file);
    assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
    let (resolutions, _) = resolve_source_file(&parsed);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut parsed, &resolutions);
    let mut tcx = TyCtxt::new();
    let (table, _) = typecheck_source_file(&parsed, &resolutions, &mut tcx);
    analyze(&parsed, &resolutions, &table, &tcx)
}

fn adapter_codes(source: &str) -> Vec<String> {
    let mut map = SourceMap::new();
    let file = map.add_file("adapters.gos".to_string(), source.to_string());
    let (mut parsed, parse_errors) = parse_source_file(source, file);
    assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
    let (resolutions, _) = resolve_source_file(&parsed);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut parsed, &resolutions);
    let mut tcx = TyCtxt::new();
    let (table, _) = typecheck_source_file(&parsed, &resolutions, &mut tcx);
    check_parallel_adapters(&parsed, &resolutions, &table, &tcx)
        .iter()
        .map(|d| d.to_diagnostic().code.0.to_string())
        .collect()
}

#[test]
fn arithmetic_is_pure() {
    assert!(purity_for("fn add(a: i64, b: i64) -> i64 { a + b }").is_pure_named("add"));
}

#[test]
fn printing_is_not_pure() {
    assert!(!purity_for("fn shout(s: String) { println(\"{}\", s) }").is_pure_named("shout"));
}

#[test]
fn purity_is_transitive_through_calls() {
    let facts =
        purity_for("fn shout(s: String) { println(\"{}\", s) }\nfn caller() { shout(\"hi\") }");
    assert!(!facts.is_pure_named("caller"));
}

#[test]
fn the_path_to_the_first_effect_is_recorded() {
    let facts = purity_for(
        "fn shout(s: String) { println(\"{}\", s) }\n\
         fn middle() { shout(\"hi\") }\nfn caller() { middle() }",
    );
    let path = facts.first_effect_path_named("caller").expect("a path");
    assert_eq!(path, vec!["caller", "middle", "shout"]);
}

#[test]
fn local_mutation_stays_pure() {
    let facts = purity_for(
        "fn total(xs: Vec<i64>) -> i64 { let mut acc = 0\n for x in xs { acc += x }\n acc }",
    );
    assert!(facts.is_pure_named("total"));
}

#[test]
fn a_mut_ref_parameter_is_not_pure() {
    assert!(!purity_for("fn push(xs: &mut Vec<i64>) { xs.push(1) }").is_pure_named("push"));
}

#[test]
fn a_closure_parameter_leaves_the_fragment() {
    let facts = purity_for("fn apply(f: Fn(i64) -> i64, x: i64) -> i64 { f(x) }");
    assert!(!facts.is_pure_named("apply"));
}

#[test]
fn recursion_does_not_make_a_function_impure() {
    let facts = purity_for("fn down(n: i64) -> i64 { if n <= 0 { 0 } else { down(n - 1) } }");
    assert!(facts.is_pure_named("down"));
}

#[test]
fn a_diverging_function_is_still_pure() {
    // Termination is not checked and is not required: an adapter callback
    // that never returns hangs exactly as the same call would sequentially.
    assert!(purity_for("fn spin(n: i64) -> i64 { spin(n) }").is_pure_named("spin"));
}

#[test]
fn a_closure_writing_a_captured_container_is_not_pure() {
    let codes = adapter_codes(
        "fn main() { let xs = #[1, 2]\n let mut seen = #[]\n \
         let ys = xs.par_map(|v| { seen.push(v)\n v })\n \
         println(\"{} {}\", ys.len(), seen.len()) }",
    );
    assert_eq!(codes, vec!["GT0090"]);
}

#[test]
fn a_callee_writing_only_through_a_local_mut_ref_keeps_the_caller_pure() {
    let facts = purity_for(
        "fn push(xs: &mut Vec<i64>) { xs.push(1) }\n\
         fn build() -> i64 { let mut t = #[]\n push(&mut t)\n t.len() }",
    );
    assert!(facts.is_pure_named("build"));
}

#[test]
fn a_mut_self_method_on_a_local_keeps_the_caller_pure() {
    let facts = purity_for(
        "struct Counter { n: i64 }\n\
         impl Counter { fn bump(&mut self) { self.n += 1 } }\n\
         fn count() -> i64 { let mut c = Counter { n: 0 }\n c.bump()\n c.n }",
    );
    assert!(facts.is_pure_named("count"));
    assert!(!facts.is_pure_named("Counter::bump"));
}

#[test]
fn the_math_module_is_pure_and_its_random_numbers_are_not() {
    let facts = purity_for(
        "use std::math\nuse std::math::rand\n\
         fn magnitude(x: f64) -> f64 { math::sqrt(x * x) }\n\
         fn noisy() -> i64 { rand::int(0, 10) }",
    );
    assert!(facts.is_pure_named("magnitude"));
    assert!(!facts.is_pure_named("noisy"));
}

#[test]
fn a_pure_callback_raises_no_diagnostic() {
    assert!(
        adapter_codes(
            "fn main() { let xs = #[1, 2]\n let ys = xs.par_map(|v| v * 2)\n println(\"{}\", ys) }"
        )
        .is_empty()
    );
}

#[test]
fn an_impure_user_ordering_is_refused_for_par_min() {
    let codes = adapter_codes(
        "struct Job { pri: i64 }\n\
         impl Ord for Job { fn cmp(&self, other: Job) -> i64 { println(\"cmp\")\n self.pri - other.pri } }\n\
         fn main() { let jobs = #[Job { pri: 1 }, Job { pri: 2 }]\n println(\"{}\", jobs.par_min().unwrap().pri) }",
    );
    assert_eq!(codes, vec!["GT0090"]);
}

#[test]
fn every_stdlib_purity_entry_names_a_real_module() {
    // A renamed module must not silently widen or narrow what counts as pure.
    for module in PURE_STDLIB_MODULES.iter().chain(IMPURE_STDLIB_SUBMODULES) {
        assert!(
            gossamer_resolve::STDLIB_MODULE_PATHS.contains(module),
            "`{module}` is not a stdlib module"
        );
    }
}
