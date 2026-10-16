//! Comprehensive Rust-guided assignment, borrowing, dereference, and binding-pattern matrix.
//!
//! Gossamer uses reference counting rather than Rust's ownership moves, but the
//! shared syntax must retain Rust's distinction between values, mutable
//! bindings, shared references, mutable references, and reference patterns.

use gossamer_lex::SourceMap;
use gossamer_parse::parse_source_file;
use gossamer_resolve::resolve_source_file;
use gossamer_types::{TyCtxt, typecheck_source_file};
use std::io::Write as _;
use std::process::{Command, Stdio};

fn diagnostics(source: &str) -> Vec<String> {
    let mut map = SourceMap::new();
    let file = map.add_file("comprehensive_assignment_tests.gos", source.to_string());
    let (mut parsed, parse_diagnostics) = parse_source_file(source, file);
    if !parse_diagnostics.is_empty() {
        // Guidance lives in the rendered help, so a case is matched on the
        // whole diagnostic rather than its summary line alone.
        return parse_diagnostics
            .iter()
            .map(|diagnostic| {
                let rendered = diagnostic.to_diagnostic();
                let mut text = rendered.title.clone();
                for help in &rendered.helps {
                    text.push_str("; ");
                    text.push_str(help);
                }
                text
            })
            .collect();
    }
    let (mut resolutions, resolve_diagnostics) = resolve_source_file(&parsed);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut parsed, &mut resolutions);
    if !resolve_diagnostics.is_empty() {
        return resolve_diagnostics
            .into_iter()
            .map(|diagnostic| diagnostic.error.to_string())
            .collect();
    }
    let mut tcx = TyCtxt::new();
    let (_, type_diagnostics) = typecheck_source_file(&parsed, &resolutions, &mut tcx);
    type_diagnostics
        .into_iter()
        .map(|diagnostic| format!("{}: {}", diagnostic.error.code(), diagnostic.error))
        .collect()
}

fn assert_accepted(label: &str, body: &str) {
    let source = format!("fn main() {{\n{body}\n}}\n");
    let found = diagnostics(&source);
    assert!(
        found.is_empty(),
        "{label} should be accepted, got {found:?}"
    );
}

fn assert_rejected(label: &str, body: &str, expected: &str) {
    let source = format!("fn main() {{\n{body}\n}}\n");
    let found = diagnostics(&source);
    assert!(
        found.iter().any(|diagnostic| diagnostic.contains(expected)),
        "{label} should be rejected with `{expected}`, got {found:?}"
    );
}

const PRELUDE: &str = "    let plain = 7\n    let mut mutable = 8\n    let mut referenced_mutable = 9\n    let shared = &plain\n    let exclusive = &mut referenced_mutable";

#[test]
fn plain_and_mutable_bindings_accept_every_well_typed_rhs_form() {
    let rhs_cases = [
        ("literal", "9"),
        ("immutable value", "plain"),
        ("mutable value", "mutable"),
        ("shared borrow", "&plain"),
        ("mutable borrow", "&mut mutable"),
        ("shared dereference", "*shared"),
        ("mutable dereference", "*exclusive"),
    ];
    for (rhs_label, rhs) in rhs_cases {
        for (pattern_label, pattern) in [("plain", "value"), ("mutable", "mut value")] {
            assert_accepted(
                &format!("{pattern_label} binding from {rhs_label}"),
                &format!("{PRELUDE}\n    let {pattern} = {rhs}"),
            );
        }
    }
}

#[test]
fn reference_patterns_require_the_exact_reference_mutability() {
    for (label, rhs) in [("shared borrow", "&plain"), ("shared binding", "shared")] {
        assert_accepted(
            &format!("shared reference pattern from {label}"),
            &format!("{PRELUDE}\n    let &value = {rhs}\n    let copy = value"),
        );
    }
    for (label, rhs) in [
        ("literal", "9"),
        ("plain value", "plain"),
        ("mutable value", "mutable"),
        ("mutable borrow", "&mut mutable"),
        ("mutable reference", "exclusive"),
        ("shared dereference", "*shared"),
        ("mutable dereference", "*exclusive"),
    ] {
        assert_rejected(
            &format!("shared reference pattern from {label}"),
            &format!("{PRELUDE}\n    let &value = {rhs}"),
            "GT0048",
        );
    }

    for (label, rhs) in [
        ("mutable borrow", "&mut mutable"),
        ("mutable binding", "exclusive"),
    ] {
        assert_accepted(
            &format!("mutable reference pattern from {label}"),
            &format!("{PRELUDE}\n    let &mut value = {rhs}\n    let copy = value"),
        );
    }
    for (label, rhs) in [
        ("literal", "9"),
        ("plain value", "plain"),
        ("mutable value", "mutable"),
        ("shared borrow", "&plain"),
        ("shared reference", "shared"),
        ("shared dereference", "*shared"),
        ("mutable dereference", "*exclusive"),
    ] {
        assert_rejected(
            &format!("mutable reference pattern from {label}"),
            &format!("{PRELUDE}\n    let &mut value = {rhs}"),
            "GT0048",
        );
    }
}

#[test]
fn borrowing_and_dereferencing_follow_the_referent_capability() {
    assert_accepted(
        "shared and mutable borrows can be bound",
        "    let plain = 1\n    let mut mutable = 2\n    let shared = &plain\n    let exclusive = &mut mutable\n    let a = *shared\n    let b = *exclusive",
    );
    assert_rejected(
        "mutable borrow of immutable binding",
        "    let plain = 1\n    let bad = &mut plain",
        "GT0032",
    );
    assert_rejected(
        "write through shared reference",
        "    let plain = 1\n    let shared = &plain\n    *shared = 2",
        "GT0031",
    );
    assert_accepted(
        "write through mutable reference",
        "    let mut mutable = 1\n    let exclusive = &mut mutable\n    *exclusive = 2",
    );
}

#[test]
fn assignment_places_distinguish_binding_mutability_from_reference_mutability() {
    assert_rejected(
        "immutable value binding assignment",
        "    let value = 1\n    value = 2",
        "GT0030",
    );
    assert_accepted(
        "mutable value binding assignment",
        "    let mut value = 1\n    value = 2",
    );
    assert_rejected(
        "immutable shared-reference binding cannot be rebound",
        "    let a = 1\n    let b = 2\n    let reference = &a\n    reference = &b",
        "GT0030",
    );
    assert_accepted(
        "mutable shared-reference binding can be rebound",
        "    let a = 1\n    let b = 2\n    let mut reference = &a\n    reference = &b",
    );
    assert_accepted(
        "immutable binding of mutable reference can write its referent",
        "    let mut value = 1\n    let reference = &mut value\n    *reference = 2",
    );
}

#[test]
fn destructuring_assignment_targets_follow_the_scalar_assignment_rules() {
    assert_accepted(
        "mutable bindings",
        "    let mut a = 1\n    let mut b = 2\n    a, b = (b, a)",
    );
    assert_accepted(
        "field, index, and tuple-position targets",
        "    let mut xs = #[0, 0]\n    let mut t = (0, 0)\n    xs[0], t.1 = (1, 2)",
    );
    assert_accepted(
        "nested tuple targets",
        "    let mut a = 0\n    let mut b = 0\n    let mut c = 0\n    a, (b, c) = 1, (2, 3)",
    );
    assert_accepted(
        "`_` discards an element",
        "    let mut a = 0\n    _, a = (1, 2)",
    );
    assert_rejected(
        "immutable element",
        "    let a = 1\n    let mut b = 2\n    a, b = (3, 4)",
        "GT0030",
    );
    assert_rejected(
        "write through a shared reference element",
        "    let plain = 1\n    let mut b = 2\n    let shared = &plain\n    *shared, b = (3, 4)",
        "GT0031",
    );
    assert_rejected(
        "arity mismatch",
        "    let mut a = 1\n    let mut b = 2\n    a, b = (1, 2, 3)",
        "GT0001",
    );
    assert_rejected(
        "element type mismatch",
        "    let mut a = 1\n    let mut b = \"s\"\n    a, b = (1, 2)",
        "GT0001",
    );
}

#[test]
fn an_assignment_target_that_names_no_place_is_rejected() {
    assert_rejected("literal target", "    5 = 1", "GT0078");
    assert_rejected(
        "literal element in a destructuring target",
        "    let mut a = 0\n    a, 5 = 1, 2",
        "GT0078",
    );
    assert_rejected(
        "call result target",
        "    fn f() -> i64 { 1 }\n    f() = 2",
        "GT0078",
    );
}

#[test]
fn invalid_mut_before_reference_pattern_spelling_is_rejected() {
    assert_rejected(
        "mut before shared reference pattern",
        "    let value = 1\n    let mut &copy = &value",
        "a reference pattern is written `&mut name`, not `mut &name`",
    );
}

#[test]
fn aggregate_reference_pattern_is_rejected() {
    assert_rejected(
        "mutable reference pattern cannot copy a fixed array referent",
        "    let mut values = [1, 2, 3]\n    let &mut copy = &mut values\n    let first = copy[0]",
        "GT0054",
    );
}

#[test]
fn repl_rejects_aggregate_reference_pattern() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_gos"))
        .arg("repl")
        .env(
            "GOSSAMER_HISTORY",
            std::env::temp_dir().join(format!(
                "gossamer-comprehensive-assignment-history-{}",
                std::process::id()
            )),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn repl");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"let mut m = [1, 2, 3]\nlet &mut d = &mut m\nlet b = d\n%b\n")
        .expect("write repl input");
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait for repl");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let transcript = format!("{stdout}{stderr}");
    assert!(
        transcript.contains("reference pattern cannot bind aggregate value `[i64; 3]` by value"),
        "{transcript}"
    );
    assert!(!stdout.contains("d: [i64; 3]"), "{stdout}");
    assert!(!stdout.contains("b: [i64; 3]"), "{stdout}");
}
