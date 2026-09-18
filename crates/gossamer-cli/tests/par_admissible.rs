//! A parallel adapter's callback must be one whose purity the compiler can
//! decide, and must never be one that writes a container it captured.

mod common;

#[test]
fn a_pure_closure_literal_is_accepted() {
    let out = common::gos_check_str(
        "fn main() { let xs = #[1, 2]\n println(\"{}\", xs.par_map(|v| v * 2)) }",
    );
    assert_eq!(out.status.code(), Some(0), "{}", common::stderr(&out));
}

#[test]
fn the_callback_shorthand_is_accepted() {
    let out = common::gos_check_str(
        "use std::math\nfn main() { let xs = #[-1, 2]\n println(\"{}\", xs.par_map(math::abs)) }",
    );
    assert_eq!(out.status.code(), Some(0), "{}", common::stderr(&out));
}

#[test]
fn an_effectful_closure_reports_gt0090() {
    let out = common::gos_check_str(
        "fn main() { let xs = #[1, 2]\n \
         println(\"{}\", xs.par_map(|v| { println(\"{}\", v)\n v })) }",
    );
    assert!(
        common::stderr(&out).contains("GT0090"),
        "{}",
        common::stderr(&out)
    );
}

#[test]
fn a_closure_that_writes_a_captured_container_reports_gt0090() {
    // The capture is by managed reference, so every worker would push into one
    // container at once. This test must never be relaxed.
    let out = common::gos_check_str(
        "fn main() { let xs = #[1, 2]\n let mut seen = #[]\n \
         let ys = xs.par_map(|v| { seen.push(v)\n v })\n \
         println(\"{} {}\", ys.len(), seen.len()) }",
    );
    let stderr = common::stderr(&out);
    assert!(stderr.contains("GT0090"), "{stderr}");
    assert!(
        stderr.contains("captured"),
        "the message must name the capture: {stderr}"
    );
}

#[test]
fn a_callable_binding_is_rejected_with_the_spelling_that_works() {
    let out = common::gos_check_str(
        "fn main() { let f = |v: i64| v * 2\n let xs = #[1, 2]\n \
         println(\"{}\", xs.par_map(f)) }",
    );
    let stderr = common::stderr(&out);
    assert!(stderr.contains("GT0090"), "{stderr}");
    assert!(stderr.contains("closure literal"), "{stderr}");
}

#[test]
fn the_diagnostic_prints_the_call_path_to_the_effect() {
    let out = common::gos_check_str(
        "fn log(n: i64) -> i64 { println(\"{}\", n)\n n }\n\
         fn mid(n: i64) -> i64 { log(n) }\n\
         fn main() { let xs = #[1, 2]\n println(\"{}\", xs.par_map(|v| mid(v))) }",
    );
    let stderr = common::stderr(&out);
    assert!(stderr.contains("mid") && stderr.contains("log"), "{stderr}");
}

#[test]
fn a_diverging_callback_is_accepted_because_purity_is_the_rule() {
    // Not admissible-by-termination: a callback that never returns hangs
    // exactly as the same call would sequentially. Nothing new is unsound.
    let out = common::gos_check_str(
        "fn spin(n: i64) -> i64 { spin(n) }\n\
         fn main() { let xs = #[1, 2]\n println(\"{}\", xs.par_map(|v| spin(v))) }",
    );
    assert_eq!(out.status.code(), Some(0), "{}", common::stderr(&out));
}

#[test]
fn a_reduction_callback_that_writes_a_captured_scalar_reports_gt0090() {
    // Every worker shares the closure's environment, so a captured scalar is
    // one slot written from every worker at once.
    let out = common::gos_check_str(
        "fn main() { let xs = #[1, 2]\n let mut calls = 0\n \
         println(\"{}\", xs.par_reduce(0, |a, b| { calls += 1\n a + b })) }",
    );
    assert!(
        common::stderr(&out).contains("GT0090"),
        "{}",
        common::stderr(&out)
    );
}

#[test]
fn an_adapter_on_a_lazy_iterator_is_not_part_of_its_surface() {
    let out = common::gos_check_str(
        "fn main() { let xs = #[1, 2]\n println(\"{}\", xs.iter().par_map(|v| v * 2)) }",
    );
    assert_ne!(out.status.code(), Some(0));
    assert!(
        common::stderr(&out).contains("par_map"),
        "{}",
        common::stderr(&out)
    );
}
