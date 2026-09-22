//! End-to-end type-checker tests driven by parser + resolver output.

use gossamer_ast::{ExprKind, ItemKind, SourceFile, StmtKind};
use gossamer_lex::SourceMap;
use gossamer_parse::parse_source_file;
use gossamer_resolve::resolve_source_file;
use gossamer_types::{IntTy, TyCtxt, TyKind, TypeError, TypeTable, typecheck_source_file};

struct Checked {
    source: SourceFile,
    table: TypeTable,
    diagnostics: Vec<gossamer_types::TypeDiagnostic>,
    tcx: TyCtxt,
}

fn run(source: &str) -> Checked {
    let mut map = SourceMap::new();
    let file = map.add_file("test.gos", source.to_string());
    let (mut sf, parse_diags) = parse_source_file(source, file);
    assert!(parse_diags.is_empty(), "parse errors: {parse_diags:?}");
    let (resolutions, resolve_diags) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &resolutions);
    let unresolved: Vec<_> = resolve_diags
        .iter()
        .filter(|d| {
            matches!(
                d.error,
                gossamer_resolve::ResolveError::UnresolvedName { .. }
            )
        })
        .collect();
    assert!(unresolved.is_empty(), "resolve errors: {unresolved:?}");
    let mut tcx = TyCtxt::new();
    let (table, diagnostics) = typecheck_source_file(&sf, &resolutions, &mut tcx);
    Checked {
        source: sf,
        table,
        diagnostics,
        tcx,
    }
}

#[test]
fn suffixed_integer_literal_receives_declared_type() {
    let checked = run("fn main() { let x = 42i32 }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let ItemKind::Fn(decl) = &checked.source.items[0].kind else {
        panic!("expected fn");
    };
    let body = decl.body.as_ref().unwrap();
    let ExprKind::Block(block) = &body.kind else {
        panic!("expected block");
    };
    let stmt = &block.stmts[0];
    let StmtKind::Let { init, .. } = &stmt.kind else {
        panic!("expected let");
    };
    let init = init.as_ref().unwrap();
    let ty = checked.table.get(init.id).expect("init typed");
    assert!(matches!(
        checked.tcx.kind(ty),
        Some(TyKind::Int(gossamer_types::IntTy::I32))
    ));
}

#[test]
fn string_literal_has_string_type() {
    let checked = run("fn main() { let s = \"hi\" }\n");
    assert!(checked.diagnostics.is_empty());
    let ItemKind::Fn(decl) = &checked.source.items[0].kind else {
        panic!("expected fn");
    };
    let body = decl.body.as_ref().unwrap();
    let ExprKind::Block(block) = &body.kind else {
        panic!("expected block");
    };
    let stmt = &block.stmts[0];
    let StmtKind::Let { init, .. } = &stmt.kind else {
        panic!("expected let");
    };
    let init = init.as_ref().unwrap();
    let ty = checked.table.get(init.id).unwrap();
    assert!(matches!(checked.tcx.kind(ty), Some(TyKind::String)));
}

#[test]
fn plain_let_rejects_literal_patterns_even_when_values_match() {
    for source in [
        "fn main() { let 9 = 8 }\n",
        "fn main() { let 9 = 9 }\n",
        "fn main() { let true = true }\n",
    ] {
        let checked = run(source);
        assert!(
            checked
                .diagnostics
                .iter()
                .any(|diag| matches!(diag.error, TypeError::CannotAssignToLiteral)),
            "expected a literal-assignment diagnostic for {source:?}: {:?}",
            checked.diagnostics
        );
    }
}

#[test]
fn literal_patterns_remain_valid_in_pattern_testing_constructs() {
    let checked = run("fn main() {\n\
             if let 9 = 8 { }\n\
             match 8 { 9 => (), _ => () }\n\
             let 9 = 8 else { return }\n\
         }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

#[test]
fn basic_invalid_programs_report_their_specific_type_error() {
    for (name, source, expected) in basic_binding_error_cases()
        .iter()
        .chain(basic_expression_error_cases())
    {
        let checked = run(source);
        assert!(
            checked.diagnostics.iter().any(|diag| expected(&diag.error)),
            "{name} did not report its expected type error: {:?}",
            checked.diagnostics
        );
    }
}

type BasicErrorCase = (&'static str, &'static str, fn(&TypeError) -> bool);

fn basic_binding_error_cases() -> &'static [BasicErrorCase] {
    &[
        (
            "annotated binding",
            "fn main() { let value: bool = 1 }\n",
            |error| matches!(error, TypeError::TypeMismatch { .. }),
        ),
        (
            "assignment",
            "fn main() { let mut value = 1\nvalue = false }\n",
            |error| matches!(error, TypeError::TypeMismatch { .. }),
        ),
        (
            "immutable assignment",
            "fn main() { let value = 1\nvalue = 2 }\n",
            |error| matches!(error, TypeError::AssignToImmutable { .. }),
        ),
        (
            "function argument",
            "fn takes_bool(value: bool) {}\nfn main() { takes_bool(1) }\n",
            |error| {
                matches!(
                    error,
                    TypeError::TypeMismatch { .. } | TypeError::ArgumentTypeMismatch { .. }
                )
            },
        ),
        (
            "return value",
            "fn value() -> i64 { return false }\n",
            |error| matches!(error, TypeError::TypeMismatch { .. }),
        ),
        (
            "missing return value",
            "fn value() -> i64 { return }\n",
            |error| matches!(error, TypeError::TypeMismatch { .. }),
        ),
        ("if condition", "fn main() { if 1 { } }\n", |error| {
            matches!(error, TypeError::TypeMismatch { .. })
        }),
        ("while condition", "fn main() { while 1 { } }\n", |error| {
            matches!(error, TypeError::TypeMismatch { .. })
        }),
        (
            "match guard",
            "fn main() { match 1 { value if 1 => (), _ => () } }\n",
            |error| matches!(error, TypeError::TypeMismatch { .. }),
        ),
        ("literal assignment", "fn main() { let 9 = 8 }\n", |error| {
            matches!(error, TypeError::CannotAssignToLiteral)
        }),
    ]
}

fn basic_expression_error_cases() -> &'static [BasicErrorCase] {
    &[
        (
            "operator operands",
            "fn main() { let _ = true + 1 }\n",
            |error| {
                matches!(
                    error,
                    TypeError::UnresolvedOp { .. } | TypeError::TypeMismatch { .. }
                )
            },
        ),
        ("scalar indexing", "fn main() { let _ = 1[0] }\n", |error| {
            matches!(error, TypeError::NotIndexable { .. })
        }),
        (
            "struct field initializer",
            "struct Point { x: i64 }\nfn main() { let _ = Point { x: false } }\n",
            |error| matches!(error, TypeError::TypeMismatch { .. }),
        ),
        (
            "collection element",
            "fn main() { let values: Vec<bool> = [true, 1] }\n",
            |error| matches!(error, TypeError::TypeMismatch { .. }),
        ),
        (
            "call arity",
            "fn takes_one(value: i64) {}\nfn main() { takes_one() }\n",
            |error| matches!(error, TypeError::CallArityMismatch { .. }),
        ),
        (
            "non-callable value",
            "fn main() { let value = 1\nvalue() }\n",
            |error| matches!(error, TypeError::NotCallable { .. }),
        ),
        (
            "invalid cast",
            "fn main() { let _ = \"text\" as i64 }\n",
            |error| matches!(error, TypeError::InvalidCast { .. }),
        ),
        (
            "unknown field",
            "struct Point { x: i64 }\nfn main() { let p = Point { x: 1 }\np.y }\n",
            |error| matches!(error, TypeError::UnknownField { .. }),
        ),
        (
            "tuple field bounds",
            "fn main() { let pair = (1, true)\npair.2 }\n",
            |error| matches!(error, TypeError::NoTupleField { .. }),
        ),
    ]
}

#[test]
fn integer_comparisons_accept_different_declared_widths() {
    let checked = run("fn main() {\n\
         let i: usize = 0usize\n\
         let n: i64 = 1i64\n\
         let _ = i < n\n\
         let _ = n == i\n\
         }\n");

    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

/// `fs::File::write` is text-typed, so a byte vector is a type error at
/// check time. Answering it as an `Err` at run time left a storage engine's
/// write silently producing nothing.
#[test]
fn a_byte_vector_is_rejected_by_the_text_write() {
    let d = diagnostics_for(
        "use std::fs\n\
         fn main() { match fs::File::create(\"x.dat\") {\n\
         Ok(f) => { let data: Vec<u8> = #[1]\n\
         let _ = f.write(data) }\n\
         Err(e) => println(\"{}\", e)\n\
         } }\n",
    );
    assert!(has_code(&d, "GT0001"), "{d:?}");
}

/// A method the handle does not answer is reported where it is written,
/// not as an unresolved name at run time.
#[test]
fn a_method_the_file_handle_lacks_is_rejected() {
    let d = diagnostics_for(
        "use std::fs\n\
         fn main() { match fs::File::create(\"x.dat\") {\n\
         Ok(f) => { let _ = f.zonk() }\n\
         Err(e) => println(\"{}\", e)\n\
         } }\n",
    );
    assert!(has_code(&d, "GT0002"), "{d:?}");
}

/// Opening a file can fail, so the constructors answer a `Result` that `?`
/// propagates rather than the bare handle.
#[test]
fn the_file_constructors_answer_a_result() {
    let d = diagnostics_for(
        "use std::{errors, fs}\n\
         fn main() -> Result<(), errors::Error> { let f = fs::File::create(\"x.dat\")?\n\
         f.close()\n\
         Ok(()) }\n",
    );
    assert!(d.is_empty(), "{d:?}");
}

#[test]
fn a_range_is_a_lazy_iterator() {
    let checked = run("fn main() { let r = 10.. }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let ItemKind::Fn(decl) = &checked.source.items[0].kind else {
        panic!("expected fn");
    };
    let ExprKind::Block(block) = &decl.body.as_ref().expect("body").kind else {
        panic!("expected block");
    };
    let StmtKind::Let {
        init: Some(init), ..
    } = &block.stmts[0].kind
    else {
        panic!("expected initialized let");
    };
    let ty = checked.table.get(init.id).expect("range typed");
    let Some(TyKind::Range(elem)) = checked.tcx.kind(ty) else {
        panic!("expected Range, got {:?}", checked.tcx.kind(ty));
    };
    assert!(matches!(
        checked.tcx.kind(*elem),
        Some(TyKind::Int(IntTy::I64))
    ));

    let consumed = run("use Iterator\n\
         fn total(it: Iterator<i64>) -> i64 { let mut sum = 0\n\
         for value in it { sum += value }\n\
         sum }\n\
         fn main() { let r = 10..12\n\
         let _ = total(r) }\n");
    assert!(
        consumed.diagnostics.is_empty(),
        "{:?}",
        consumed.diagnostics
    );
}

#[test]
fn an_iterator_parameter_accepts_an_iterator_argument() {
    let checked = run("use Iterator\n\
         fn take(a: Iterator<i64>) -> Option<i64> { a.next() }\n\
         fn main() { let v = #[1, 2, 3]\n\
         let _ = take(v.iter()) }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

#[test]
fn an_iterator_parameter_rejects_a_differently_typed_iterator() {
    let checked = run("use Iterator\n\
         fn take(a: Iterator<i64>) -> Option<i64> { a.next() }\n\
         fn main() { let v = #[\"a\", \"b\"]\n\
         let _ = take(v.iter()) }\n");
    assert!(
        checked.diagnostics.iter().any(|diagnostic| matches!(
            &diagnostic.error,
            TypeError::TypeMismatch { expected, found }
                if expected == "Iterator<i64>" && found == "Iterator<String>"
        )),
        "{:?}",
        checked.diagnostics
    );
}

#[test]
fn formatting_a_range_reports_the_iterator_remedy() {
    let checked = run("fn main() { println(\"{}\", 10..) }\n");
    let diagnostic = checked
        .diagnostics
        .iter()
        .find(|diagnostic| matches!(diagnostic.error, TypeError::IteratorStateFormatted))
        .expect("iterator formatting diagnostic")
        .to_diagnostic();
    assert_eq!(diagnostic.code.as_str(), "GT0041");
    assert!(
        diagnostic
            .helps
            .iter()
            .any(|help| help.contains("iter::collect"))
    );
}

#[test]
fn formatting_a_runtime_handle_names_the_handle() {
    let checked = run("use std::sync\nfn main() { println(\"{}\", sync::Map::new()) }\n");
    let diagnostic = checked
        .diagnostics
        .iter()
        .find(|diagnostic| matches!(diagnostic.error, TypeError::ValueNotDisplayable { .. }))
        .expect("handle formatting diagnostic")
        .to_diagnostic();
    assert_eq!(diagnostic.code.as_str(), "GT0062");
    assert!(
        diagnostic
            .helps
            .iter()
            .any(|help| help.contains("sync::Map")),
        "{diagnostic:?}"
    );
}

#[test]
fn formatting_a_function_value_is_rejected() {
    let checked = run("fn f(x: i64) -> i64 { x }\nfn main() { println(\"{}\", f) }\n");
    assert!(
        checked.diagnostics.iter().any(|diagnostic| matches!(
            &diagnostic.error,
            TypeError::ValueNotDisplayable { class, .. }
                if *class == gossamer_types::NotDisplayableClass::Callable
        )),
        "{:?}",
        checked.diagnostics
    );
}

#[test]
fn a_qualified_container_annotation_resolves_to_the_container() {
    let checked = run(
        "use std::collections\nfn f(d: collections::Deque<i64>) -> i64 { d.len() }\nfn main() { let _ = f(collections::Deque::new()) }\n",
    );
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let checked = run(
        "use std::collections\nfn f(d: collections::Deque<i64>) -> i64 { d.len() }\nfn main() { let _ = f(\"nope\") }\n",
    );
    assert!(
        checked.diagnostics.iter().any(|diagnostic| matches!(
            &diagnostic.error,
            TypeError::TypeMismatch { expected, .. } if expected.starts_with("Deque")
        )),
        "{:?}",
        checked.diagnostics
    );
}

#[test]
fn let_annotation_forces_concrete_type() {
    let checked = run("fn main() { let x: i32 = 1i32 }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

#[test]
fn obvious_concrete_mismatch_is_reported() {
    let checked = run("fn main() { let x: bool = 42i32 }\n");
    assert!(!checked.diagnostics.is_empty());
    assert!(
        checked
            .diagnostics
            .iter()
            .any(|d| matches!(d.error, TypeError::TypeMismatch { .. })),
        "expected type mismatch diagnostic: {:?}",
        checked.diagnostics
    );
}

#[test]
fn function_argument_rejects_plain_tuple_for_tuple_struct() {
    let checked = run("struct RGB(i64, i64, i64)\n\
         fn print_color(color: RGB) { println(\"{}\", color) }\n\
         fn main() { let three = (1, 500, -200)\n print_color(three) }\n");
    assert!(
        checked.diagnostics.iter().any(|d| matches!(
            &d.error,
            TypeError::TypeMismatch { expected, found }
                if expected == "RGB" && found == "(i64, i64, i64)"
        )),
        "plain tuple must not satisfy a nominal tuple-struct parameter: {:?}",
        checked.diagnostics
    );

    let checked = run("struct RGB(i64, i64, i64)\n\
         struct Triple(i64, i64, i64)\n\
         fn print_color(color: RGB) {}\n\
         fn main() { print_color(Triple(1, 500, -200)) }\n");
    assert!(
        checked.diagnostics.iter().any(|d| matches!(
            &d.error,
            TypeError::TypeMismatch { expected, found }
                if expected == "RGB" && found == "Triple"
        )),
        "distinct tuple structs should be named concisely: {:?}",
        checked.diagnostics
    );
}

#[test]
fn function_boundaries_preserve_nominal_struct_identity() {
    let rejected = [
        "struct A { value: i64 }\nstruct B { value: i64 }\nfn take(v: A) {}\nfn main() { take(B { value: 1 }) }\n",
        "struct A(i64, i64)\nstruct B(i64, i64)\nfn take(v: A) {}\nfn main() { take(B(1, 2)) }\n",
        "struct A(i64, i64)\nfn make() -> A { (1, 2) }\nfn main() {}\n",
        "struct A { value: i64 }\nstruct B { value: i64 }\nfn id<T>(v: T) -> T { v }\nfn main() { let _ = id::<A>(B { value: 1 }) }\n",
        "struct A(i64)\nstruct B(i64)\nfn id<T>(v: T) -> T { v }\nfn main() { let _ = id::<A, B>(A(1)) }\n",
    ];
    for source in rejected {
        let checked = run(source);
        assert!(
            checked.diagnostics.iter().any(|d| matches!(
                d.error,
                TypeError::TypeMismatch { .. } | TypeError::CallArityMismatch { .. }
            )),
            "nominal mismatch crossed a function boundary: {source}\n{:?}",
            checked.diagnostics
        );
    }

    let accepted = run("struct A(i64, i64)\n\
         fn take_struct(v: A) {}\n\
         fn take_tuple(v: (i64, i64)) {}\n\
         fn id<T>(v: T) -> T { v }\n\
         fn take_fn(f: Fn(i64) -> i64) { let _ = f(1) }\n\
         fn main() { take_struct(A(1, 2))\n take_tuple((1, 2))\n take_fn(id) }\n");
    assert!(
        accepted.diagnostics.is_empty(),
        "{:?}",
        accepted.diagnostics
    );
}

#[test]
fn string_values_coerce_to_borrowed_str_only_at_non_escaping_boundaries() {
    let checked = run("static GREETING: &str = \"hello\"\n\
         fn take(value: str) {}\n\
         fn main() { take(\"text\") }\n");
    assert!(
        checked.diagnostics.is_empty(),
        "String to &str coercions must remain valid: {:?}",
        checked.diagnostics
    );

    let static_return =
        run("fn classify(value: bool) -> &str { if value { \"yes\" } else { \"no\" } }\n");
    assert!(static_return.diagnostics.is_empty());

    let escaped = run("fn bad() -> &str { let local = \"temporary\"\n &local }\n");
    assert!(escaped.diagnostics.iter().any(|diagnostic| matches!(
        diagnostic.error,
        TypeError::ReferenceEscapeUnsupported { .. }
    )));
}

#[test]
fn function_boundaries_reject_wrong_float_callable_and_pipeline_types() {
    let rejected = [
        "fn take(v: i64) {}\nfn main() { take(1.5) }\n",
        "fn wrong(v: String) -> bool { true }\nfn take(f: Fn(i64) -> bool) {}\nfn main() { take(wrong) }\n",
        "fn invoke(f: Fn(i64) -> bool) { let _ = f(\"wrong\") }\nfn main() {}\n",
        "struct A(i64)\nstruct B(i64)\nfn take(v: A, n: i64) {}\nfn main() { 1 |> |v| take(B(2), v) }\n",
        "fn pair(a: i64, b: i64) -> i64 { a + b }\nfn main() { let _ = 1 |> pair }\n",
    ];
    for source in rejected {
        let checked = run(source);
        assert!(
            checked.diagnostics.iter().any(|d| matches!(
                d.error,
                TypeError::TypeMismatch { .. } | TypeError::CallArityMismatch { .. }
            )),
            "invalid function call was accepted: {source}\n{:?}",
            checked.diagnostics
        );
    }
}

#[test]
fn methods_and_enum_constructors_check_declared_payload_types() {
    let method = run("struct A(i64)\n\
         struct B(i64)\n\
         impl A { fn take(&self, value: A) {} }\n\
         fn main() { let a = A(1)\n a.take(B(2)) }\n");
    assert!(
        method
            .diagnostics
            .iter()
            .any(|d| matches!(d.error, TypeError::TypeMismatch { .. })),
        "method parameter mismatch was accepted: {:?}",
        method.diagnostics
    );

    let variant = run("struct A(i64)\n\
         struct B(i64)\n\
         enum E { Value(A) }\n\
         fn main() { let _ = E::Value(B(2)) }\n");
    assert!(
        variant
            .diagnostics
            .iter()
            .any(|d| matches!(d.error, TypeError::TypeMismatch { .. })),
        "enum payload mismatch was accepted: {:?}",
        variant.diagnostics
    );

    let generic_method = run("struct A(i64)\n\
         struct B(i64)\n\
         struct Boxed<T> { value: T }\n\
         impl<T> Boxed<T> { fn take(&self, value: T) {} }\n\
         fn main() { let boxed = Boxed { value: A(1) }\n boxed.take(B(2)) }\n");
    assert!(
        generic_method
            .diagnostics
            .iter()
            .any(|d| matches!(d.error, TypeError::TypeMismatch { .. })),
        "generic method parameter mismatch was accepted: {:?}",
        generic_method.diagnostics
    );

    let trait_method = run("struct A(i64)\n\
         struct B(i64)\n\
         trait Takes { fn take(&self, value: A)\n }\n\
         impl Takes for A { fn take(&self, value: A) {} }\n\
         fn call<T: Takes>(value: T) { value.take(B(2)) }\n\
         fn main() {}\n");
    assert!(
        trait_method
            .diagnostics
            .iter()
            .any(|d| matches!(d.error, TypeError::TypeMismatch { .. })),
        "trait method parameter mismatch was accepted: {:?}",
        trait_method.diagnostics
    );

    let variant_arity = run("enum E { Pair(i64, i64) }\nfn main() { let _ = E::Pair(1) }\n");
    assert!(
        variant_arity
            .diagnostics
            .iter()
            .any(|d| matches!(d.error, TypeError::CallArityMismatch { .. })),
        "enum constructor arity mismatch was accepted: {:?}",
        variant_arity.diagnostics
    );
}

#[test]
fn if_branch_mismatch_is_reported() {
    let checked = run("fn main() { let y = if true { 1i32 } else { false } }\n");
    assert!(
        checked
            .diagnostics
            .iter()
            .any(|d| matches!(d.error, TypeError::TypeMismatch { .. })),
        "expected branch-mismatch diagnostic: {:?}",
        checked.diagnostics
    );
}

#[test]
fn if_branches_with_matching_types_pass() {
    let checked = run("fn main() { let y = if true { 1i32 } else { 2i32 } }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

fn has_immutable_assign(checked: &Checked) -> bool {
    checked
        .diagnostics
        .iter()
        .any(|d| matches!(d.error, TypeError::AssignToImmutable { .. }))
}

fn has_shared_reference_assign(checked: &Checked) -> bool {
    checked
        .diagnostics
        .iter()
        .any(|d| matches!(d.error, TypeError::AssignThroughSharedReference { .. }))
}

fn has_mutable_reference_to_immutable(checked: &Checked) -> bool {
    checked
        .diagnostics
        .iter()
        .any(|d| matches!(d.error, TypeError::MutableReferenceToImmutable { .. }))
}

#[test]
fn compound_assign_to_immutable_let_is_rejected() {
    let checked = run("fn main() { let total: i64 = 0\n total += 5 }\n");
    assert!(
        has_immutable_assign(&checked),
        "expected GT0030: {:?}",
        checked.diagnostics
    );
}

#[test]
fn plain_assign_to_immutable_let_is_rejected() {
    let checked = run("fn main() { let x: i64 = 0\n x = 5 }\n");
    assert!(has_immutable_assign(&checked), "{:?}", checked.diagnostics);
}

#[test]
fn assign_to_mut_let_is_accepted() {
    let checked = run("fn main() { let mut total: i64 = 0\n total += 5 }\n");
    assert!(
        !has_immutable_assign(&checked),
        "unexpected GT0030: {:?}",
        checked.diagnostics
    );
}

#[test]
fn assign_to_immutable_parameter_is_rejected() {
    let checked = run("fn f(x: i64) -> i64 { x += 1\n x }\n");
    assert!(has_immutable_assign(&checked), "{:?}", checked.diagnostics);
}

#[test]
fn assign_to_mut_parameter_is_accepted() {
    let checked = run("fn f(mut x: i64) -> i64 { x += 1\n x }\n");
    assert!(
        !has_immutable_assign(&checked),
        "unexpected GT0030: {:?}",
        checked.diagnostics
    );
}

#[test]
fn field_assign_through_immutable_binding_is_rejected() {
    let checked = run("struct P { x: i64 }\nfn main() { let p = P { x: 1 }\n p.x = 5 }\n");
    assert!(has_immutable_assign(&checked), "{:?}", checked.diagnostics);
}

#[test]
fn field_assign_through_mut_binding_is_accepted() {
    let checked = run("struct P { x: i64 }\nfn main() { let mut p = P { x: 1 }\n p.x = 5 }\n");
    assert!(
        !has_immutable_assign(&checked),
        "unexpected GT0030: {:?}",
        checked.diagnostics
    );
}

#[test]
fn write_through_mut_reference_parameter_is_accepted() {
    // The `p` binding is not itself `mut`, but a `&mut P` reference makes
    // the pointed-to place writable - matching Rust and not a false GT0030.
    let checked = run("struct P { x: i64 }\nfn bump(p: &mut P) { p.x = 9 }\n");
    assert!(
        !has_immutable_assign(&checked),
        "unexpected GT0030: {:?}",
        checked.diagnostics
    );
}

#[test]
fn mutable_reference_to_immutable_binding_is_rejected() {
    let checked = run("fn main() { let a = [1, 2]\n let c = &mut a }\n");
    assert!(
        has_mutable_reference_to_immutable(&checked),
        "expected GT0032: {:?}",
        checked.diagnostics
    );
}

#[test]
fn mutable_reference_to_mutable_binding_is_accepted() {
    let checked = run("fn main() { let mut a = [1, 2]\n let c = &mut a\n c[0] = 0 }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

#[test]
fn mutable_reference_respects_static_mutability() {
    let immutable = run("static X: i64 = 1\nfn main() { let p = &mut X }\n");
    assert!(
        has_mutable_reference_to_immutable(&immutable),
        "expected GT0032: {:?}",
        immutable.diagnostics
    );

    let mutable = run("static mut X: i64 = 1\nfn main() { let p = &mut X }\n");
    assert!(mutable.diagnostics.is_empty(), "{:?}", mutable.diagnostics);
}

#[test]
fn assignment_through_shared_reference_is_rejected_precisely() {
    let checked = run("fn main() { let a = [1, 2]\n let mut d = &a\n d[0] = 0 }\n");
    assert!(
        has_shared_reference_assign(&checked),
        "expected GT0031: {:?}",
        checked.diagnostics
    );
    assert!(
        !has_immutable_assign(&checked),
        "shared-reference write must not be reported as GT0030: {:?}",
        checked.diagnostics
    );
}

#[test]
fn mutable_reference_binding_cannot_be_rebound_to_its_referent() {
    // `mut` permits rebinding `x`, but it does not erase the `&[i64; 2]`
    // type inferred for that binding. A new reference is required.
    let checked = run("fn main() { let mut x = &[1, 2]\n x = [2, 3] }\n");
    let ItemKind::Fn(decl) = &checked.source.items[0].kind else {
        panic!("expected fn");
    };
    let ExprKind::Block(block) = &decl.body.as_ref().expect("body").kind else {
        panic!("expected block");
    };
    let ExprKind::Assign { place, value, .. } = &block.tail.as_ref().expect("assignment").kind
    else {
        panic!("expected assignment");
    };
    let place_ty = checked.table.get(place.id).expect("place typed");
    let value_ty = checked.table.get(value.id).expect("value typed");
    assert!(
        checked
            .diagnostics
            .iter()
            .any(|d| matches!(d.error, TypeError::TypeMismatch { .. })),
        "expected reference-rebinding type mismatch: {:?}; place={}, value={}",
        checked.diagnostics,
        gossamer_types::render_ty(&checked.tcx, place_ty),
        gossamer_types::render_ty(&checked.tcx, value_ty),
    );
}

#[test]
fn mutable_reference_mismatch_renders_resolved_referent_type() {
    let checked = run("fn main() { let mut a = 12\n let mut b = &mut a\n b = 16 }\n");
    assert!(
        checked.diagnostics.iter().any(|diagnostic| matches!(
            &diagnostic.error,
            TypeError::TypeMismatch { expected, found }
                if expected == "&mut i64" && found == "i64"
        )),
        "expected concrete reference mismatch, got {:?}",
        checked.diagnostics
    );
    assert!(
        checked
            .diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.to_string().contains('?')),
        "inference variables leaked into diagnostics: {:?}",
        checked.diagnostics
    );
}

#[test]
fn return_reference_mismatch_renders_public_referent_type() {
    let checked = run("fn main() {\n\
         let value = no_dangle()\n\
         println(\"{}\", value)\n\
         }\n\
         fn no_dangle() -> String {\n\
         let s = String::from(\"hello\")\n\
         &s\n\
         }\n");
    assert!(
        checked.diagnostics.iter().any(|diagnostic| matches!(
            &diagnostic.error,
            TypeError::TypeMismatch { expected, found }
                if expected == "String" && found == "&String"
        )),
        "expected String/&String mismatch, got {:?}",
        checked.diagnostics
    );
    assert!(
        checked
            .diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.to_string().contains('?')),
        "inference variables leaked into diagnostics: {:?}",
        checked.diagnostics
    );
}

#[test]
fn vec_constructors_have_public_container_types() {
    let checked = run("fn main() {\n\
             let empty = Vec::new()\n\
             let reserved = Vec::with_capacity(4)\n\
             let values = Vec::from([1, 2])\n\
             let map = Map::with_capacity(4)\n\
         }\n");
    assert!(checked.diagnostics.is_empty(), "{:#?}", checked.diagnostics);

    let gossamer_ast::ItemKind::Fn(main) = &checked.source.items[0].kind else {
        panic!("expected main function");
    };
    let body = main.body.as_ref().expect("main body");
    let gossamer_ast::ExprKind::Block(body) = &body.kind else {
        panic!("expected main block");
    };
    let types = body
        .stmts
        .iter()
        .filter_map(|stmt| match &stmt.kind {
            gossamer_ast::StmtKind::Let {
                init: Some(expr), ..
            } => checked.table.get(expr.id),
            _ => None,
        })
        .map(|ty| gossamer_types::render_public_ty(&checked.tcx, ty))
        .collect::<Vec<_>>();
    assert_eq!(types, ["Vec<_>", "Vec<_>", "Vec<i64>", "Map<_, _>"]);
}

#[test]
fn contextual_integer_literals_must_fit_their_declared_width() {
    let checked = run("struct ByteHolder { value: i8 }\n\
         fn takes_byte(value: i8) {}\n\
         fn byte() -> i8 { 567 }\n\
         fn main() {\n\
             let scalar: i8 = 567\n\
             let values: [i8; 2] = [1, 567]\n\
             let holder = ByteHolder { value: 567 }\n\
             takes_byte(567)\n\
             let negative: i8 = -129\n\
             let unsigned: u8 = 256\n\
             let signed_minimum: i8 = -128\n\
         }\n");
    let overflows = checked
        .diagnostics
        .iter()
        .filter(|diagnostic| {
            matches!(
                &diagnostic.error,
                TypeError::IntLiteralOverflow { literal, ty }
                    if literal == "567" && ty == "i8"
            )
        })
        .count();
    assert_eq!(
        overflows, 5,
        "every contextual i8 literal should be checked: {:#?}",
        checked.diagnostics
    );
    assert!(checked.diagnostics.iter().any(|diagnostic| matches!(
        &diagnostic.error,
        TypeError::IntLiteralOverflow { literal, ty }
            if literal == "-129" && ty == "i8"
    )));
    assert!(checked.diagnostics.iter().any(|diagnostic| matches!(
        &diagnostic.error,
        TypeError::IntLiteralOverflow { literal, ty }
            if literal == "256" && ty == "u8"
    )));
}

#[test]
fn mutable_reference_binding_cannot_borrow_temporary_arrays() {
    let checked = run("fn main() { let mut x = &[1, 2]\n x = &[2, 3] }\n");
    assert!(checked.diagnostics.iter().any(|diagnostic| matches!(
        diagnostic.error,
        TypeError::ReferenceEscapeUnsupported { .. }
    )));
}

#[test]
fn overlapping_mutable_references_are_rejected() {
    let checked = run(
        "fn main() { let mut a = [1, 2]\n let x = &mut a\n let y = &mut a\n x[0] = 0\n y[1] = 3 }\n",
    );
    assert!(
        checked
            .diagnostics
            .iter()
            .any(|d| matches!(d.error, TypeError::MutableReferenceConflict { .. })),
        "expected overlapping mutable-reference diagnostic: {:?}",
        checked.diagnostics
    );
}

#[test]
fn mutable_reference_rebinding_releases_the_previous_root() {
    let checked = run(
        "fn main() { let mut a = [1, 2]\n let mut b = [3, 4]\n let mut r = &mut a\n r = &mut b\n let s = &mut a\n s[0] = 0\n r[0] = 5 }\n",
    );
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

#[test]
fn comparison_produces_bool() {
    let checked = run("fn main() { let b = 1i32 < 2i32 }\n");
    assert!(checked.diagnostics.is_empty());
    let ItemKind::Fn(decl) = &checked.source.items[0].kind else {
        panic!("expected fn");
    };
    let body = decl.body.as_ref().unwrap();
    let ExprKind::Block(block) = &body.kind else {
        panic!("expected block");
    };
    let stmt = &block.stmts[0];
    let StmtKind::Let { init, .. } = &stmt.kind else {
        panic!("expected let");
    };
    let init = init.as_ref().unwrap();
    let ty = checked.table.get(init.id).unwrap();
    assert!(matches!(checked.tcx.kind(ty), Some(TyKind::Bool)));
}

#[test]
fn every_expr_node_is_typed() {
    let checked = run("fn add(a: i32, b: i32) -> i32 { a + b }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let ItemKind::Fn(decl) = &checked.source.items[0].kind else {
        panic!("expected fn");
    };
    let body = decl.body.as_ref().unwrap();
    assert!(checked.table.get(body.id).is_some());
}

#[test]
fn example_programs_typecheck_without_false_positives() {
    for name in ["hello_world.gos", "line_count.gos", "web_server.gos"] {
        let path = format!("{}/../../examples/{name}", env!("CARGO_MANIFEST_DIR"));
        let source = std::fs::read_to_string(&path).expect("read example");
        let mut map = SourceMap::new();
        let file = map.add_file(&path, source.clone());
        let (mut sf, parse_diags) = parse_source_file(&source, file);
        assert!(parse_diags.is_empty(), "{path}: {parse_diags:?}");
        let (resolutions, _resolve_diags) = resolve_source_file(&sf);
        let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &resolutions);
        let mut tcx = TyCtxt::new();
        let (_table, diagnostics) = typecheck_source_file(&sf, &resolutions, &mut tcx);
        assert!(
            diagnostics.is_empty(),
            "{path}: type diagnostics: {diagnostics:?}"
        );
    }
}

#[test]
fn cast_allows_numeric_to_numeric() {
    let src = "fn main() { let i: i32 = 1i32\n let _ = i as i64\n let _ = i as f64 }\n";
    let checked = run(src);
    assert!(
        checked.diagnostics.is_empty(),
        "expected no diagnostics: {:?}",
        checked.diagnostics,
    );
}

#[test]
fn cast_allows_bool_and_char_to_integer_but_rejects_string() {
    let src = "fn main() { let b: bool = true\n let _ = b as i64\n let s: String = \"x\"\n let _ = s as i64 }\n";
    let checked = run(src);
    assert_eq!(checked.diagnostics.len(), 1);
    assert!(
        matches!(&checked.diagnostics[0].error, TypeError::InvalidCast { from, to } if from == "String" && to == "i64"),
        "expected InvalidCast, got {:?}",
        checked.diagnostics[0].error,
    );
}

#[test]
fn cast_fails_soft_on_inference_variable_source() {
    // An unannotated closure parameter stays an unresolved inference
    // variable, so the cast check must stay soft on it. (A concrete
    // `String` source is correctly rejected - see
    // `cast_allows_bool_and_char_to_integer_but_rejects_string`.)
    let src = "fn main() { let f = |x| x as i64\n let _ = f }\n";
    let checked = run(src);
    assert!(
        checked.diagnostics.is_empty(),
        "inference-var source should not trip the cast check: {:?}",
        checked.diagnostics,
    );
}

#[test]
fn cast_same_type_is_a_noop_and_passes() {
    let src = "fn main() { let i: i64 = 1i64\n let _ = i as i64 }\n";
    let checked = run(src);
    assert!(
        checked.diagnostics.is_empty(),
        "same-type cast should be allowed: {:?}",
        checked.diagnostics,
    );
}

#[test]
fn int_to_char_casts_allowed_float_rejected() {
    // Every int width casts to char by reading its low byte. String indexing
    // already has type char, and a same-type cast remains a no-op.
    for src in [
        "fn main() { let b: u8 = 65u8\n let _: char = b as char }\n",
        "fn main() { let i: i32 = 65i32\n let _: char = i as char }\n",
        "fn main() { let s = \"hi\"\n let _: char = s[0] as char }\n",
    ] {
        let ok = run(src);
        assert!(
            ok.diagnostics.is_empty(),
            "int -> char should pass for {src:?}: {:?}",
            ok.diagnostics,
        );
    }
    let src = "fn main() { let f: f64 = 65.0\n let _: char = f as char }\n";
    let bad = run(src);
    assert_eq!(bad.diagnostics.len(), 1);
    assert!(
        matches!(&bad.diagnostics[0].error, TypeError::InvalidCast { from, to } if from == "f64" && to == "char"),
        "expected f64 -> char rejection: {:?}",
        bad.diagnostics[0].error,
    );
}

#[test]
fn unsuffixed_integer_literal_takes_let_annotation_width() {
    let checked = run("fn main() { let x: u32 = 42 }\n");
    assert!(
        checked.diagnostics.is_empty(),
        "u32 annotation should soak up the literal: {:?}",
        checked.diagnostics,
    );
}

#[test]
fn unsuffixed_integer_literal_defaults_to_i64_when_unconstrained() {
    let checked = run("fn main() { let x = 42 }\n");
    assert!(
        checked.diagnostics.is_empty(),
        "orphan literal should default cleanly: {:?}",
        checked.diagnostics,
    );
    // Walk the AST and find the binding's type entry; it must
    // have resolved to a concrete i64 by the end of typecheck.
    let main = checked
        .source
        .items
        .iter()
        .find_map(|item| {
            if let ItemKind::Fn(f) = &item.kind {
                if f.name.name == "main" {
                    return Some(f);
                }
            }
            None
        })
        .expect("main fn");
    let body = main.body.as_ref().expect("main body");
    let ExprKind::Block(block) = &body.kind else {
        panic!("expected block body");
    };
    let StmtKind::Let { init, .. } = &block.stmts[0].kind else {
        panic!("expected let statement");
    };
    let init = init.as_ref().expect("let initializer");
    let init_id = match &init.kind {
        ExprKind::Literal(_) => init.id,
        other => panic!("expected literal initializer, got {other:?}"),
    };
    let ty = checked.table.get(init_id).expect("literal type");
    let kind = checked.tcx.kind(ty).expect("kind");
    assert!(
        matches!(kind, TyKind::Int(gossamer_types::IntTy::I64)),
        "unconstrained literal should default to i64, got {kind:?}",
    );
}

#[test]
fn unsuffixed_integer_literal_rejected_in_string_position() {
    let checked = run("fn main() { let x: String = 42 }\n");
    assert_eq!(
        checked.diagnostics.len(),
        1,
        "expected one mismatch diagnostic: {:?}",
        checked.diagnostics,
    );
    let TypeError::TypeMismatch { expected, found } = &checked.diagnostics[0].error else {
        panic!(
            "expected TypeMismatch, got {:?}",
            checked.diagnostics[0].error
        );
    };
    assert_eq!(expected, "String");
    assert_eq!(found, "i64");
}

#[test]
fn array_literal_does_not_coerce_to_vec_annotation() {
    let checked = run("fn main() { let xs: Vec<String> = [\"a\", \"b\"] }\n");
    assert!(checked.diagnostics.iter().any(|diagnostic| matches!(
        &diagnostic.error,
        TypeError::TypeMismatch { expected, found }
            if expected == "Vec<String>" && found == "[String; 2]"
    )));
}

#[test]
fn named_array_does_not_coerce_to_vec_annotation() {
    let checked = run("fn main() { let a = [1, 2, 3]\n let mut v: Vec<i64> = a\n v.push(4) }\n");
    assert!(checked.diagnostics.iter().any(|diagnostic| matches!(
        &diagnostic.error,
        TypeError::TypeMismatch { expected, found }
            if expected == "Vec<i64>" && found == "[i64; 3]"
    )));
}

#[test]
fn owned_slice_annotation_is_rejected_as_unsized() {
    let checked = run("fn main() { let xs: [String] = [\"a\", \"b\"] }\n");
    assert!(
        checked
            .diagnostics
            .iter()
            .any(|diagnostic| { matches!(diagnostic.error, TypeError::UnsizedSliceValue { .. }) })
    );
}

#[test]
fn a_signature_diagnostic_is_reported_once_per_source_position() {
    // A signature's types are converted while collecting signatures and
    // again while checking the item, so an editor showed the same message
    // stacked on one span.
    let checked =
        run("fn parse(path: String) -> [i64] { [1] }\nfn main() { let _ = parse(\"x\") }\n");
    let unsized_at: Vec<_> = checked
        .diagnostics
        .iter()
        .filter(|diagnostic| matches!(diagnostic.error, TypeError::UnsizedSliceValue { .. }))
        .collect();
    assert_eq!(
        unsized_at.len(),
        1,
        "one span must yield one diagnostic; got {unsized_at:?}"
    );
}

#[test]
fn distinct_spans_each_keep_their_own_diagnostic() {
    // A parameter names the view directly, so the unsized-by-value error
    // is raised where a slice would have to be stored: a struct field.
    let checked = run("struct A { xs: [i64] }\nstruct B { ys: [f64] }\nfn main() { let _ = 1 }\n");
    let unsized_count = checked
        .diagnostics
        .iter()
        .filter(|diagnostic| matches!(diagnostic.error, TypeError::UnsizedSliceValue { .. }))
        .count();
    assert_eq!(
        unsized_count, 2,
        "deduplication must not collapse separate positions; got {:?}",
        checked.diagnostics
    );
}

#[test]
fn vec_from_repeat_array_is_explicit_and_accepted() {
    let checked = run("fn main() { let xs: Vec<i64> = Vec::from([0; 4]) }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

#[test]
fn vec_return_requires_explicit_construction() {
    let checked = run(
        "fn make() -> Vec<String> { Vec::from([\"x\", \"y\"]) }\nfn main() { let _ = make()\n }\n",
    );
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

#[test]
fn vec_literal_rejects_wrong_element_type() {
    let checked = run("fn main() { let xs: Vec<String> = [1, 2] }\n");
    assert!(
        !checked.diagnostics.is_empty(),
        "assigning integer literals to Vec<String> must error",
    );
}

#[test]
fn if_branches_of_differing_array_length_are_rejected() {
    // Differing lengths can only co-type as a Vec; this must check for any
    // element type, not only integer literals.
    let checked =
        run("fn main() { let v: Vec<String> = if true { [\"a\", \"b\"] } else { [\"c\"] } }\n");
    assert!(!checked.diagnostics.is_empty());
}

#[test]
fn nested_vec_construction_with_differing_inner_lengths_checks() {
    let checked = run(
        "fn main() { let g: Vec<Vec<i64>> = Vec::from([Vec::from([1, 2]), Vec::from([3])]) }\n",
    );
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

#[test]
fn assignment_value_uses_explicit_vec_construction() {
    // Explicit construction must record the expression as a heap Vec. A fixed
    // array in the Vec-typed slot would desynchronize compiled-tier layouts.
    let checked =
        run("fn main() { let mut v: Vec<i64> = Vec::from([1])\n v = Vec::from([2, 3]) }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let ItemKind::Fn(decl) = &checked.source.items[0].kind else {
        panic!("expected fn");
    };
    let body = decl.body.as_ref().unwrap();
    let ExprKind::Block(block) = &body.kind else {
        panic!("expected block");
    };
    let assign = block.tail.as_ref().expect("assign tail");
    let ExprKind::Assign { value, .. } = &assign.kind else {
        panic!("expected assign");
    };
    let ty = checked.table.get(value.id).expect("assign value typed");
    assert!(
        matches!(checked.tcx.kind(ty), Some(TyKind::Vec(_))),
        "assign-value literal must adopt the place's Vec shape, got {:?}",
        checked.tcx.kind(ty)
    );
}

#[test]
fn later_narrow_assignments_do_not_retype_an_inferred_source_binding() {
    let checked = run("fn main() {
            let a = 256
            let mut b: i8 = 1
            let mut v: Vec<i8> = Vec::from([1, 2])
            b = a
            v[0] = a
        }\n");

    let mismatches: Vec<_> = checked
        .diagnostics
        .iter()
        .filter_map(|diag| match &diag.error {
            TypeError::TypeMismatch { expected, found } => {
                Some((expected.as_str(), found.as_str()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(mismatches, vec![("i8", "i64"), ("i8", "i64")]);

    let ItemKind::Fn(decl) = &checked.source.items[0].kind else {
        panic!("expected fn");
    };
    let ExprKind::Block(block) = &decl.body.as_ref().expect("body").kind else {
        panic!("expected block");
    };
    let StmtKind::Let { pattern, .. } = &block.stmts[0].kind else {
        panic!("expected source binding");
    };
    let source_ty = checked.table.get(pattern.id).expect("source binding typed");
    assert_eq!(checked.tcx.kind(source_ty), Some(&TyKind::Int(IntTy::I64)));
}

#[test]
fn later_use_sites_cannot_narrow_established_numeric_bindings() {
    let checked = run("use std::collections::Map
        fn takes_i8(value: i8) {}
        fn bad_return() -> i8 {
            let value = 256
            value
        }
        fn main() {
            let value = 256
            let values = [256, 257]
            let optional = Some(256)
            let mut bytes: Vec<i8> = Vec::from([])
            let mut map: Map<String, i8> = Map::new()
            takes_i8(value)
            bytes.push(value)
            map.insert(\"key\", value)
            let narrowed: [i8; 2] = values
            let narrowed_option: Option<i8> = optional
        }\n");

    let mismatches: Vec<_> = checked
        .diagnostics
        .iter()
        .filter_map(|diag| match &diag.error {
            TypeError::TypeMismatch { expected, found } => {
                Some((expected.as_str(), found.as_str()))
            }
            _ => None,
        })
        .collect();
    assert!(
        mismatches.len() >= 6,
        "every later narrowing use must fail: {:?}",
        checked.diagnostics
    );
    assert!(
        mismatches
            .iter()
            .all(|(expected, found)| expected.contains("i8") && found.contains("i64")),
        "narrowing diagnostics must preserve established types: {mismatches:?}"
    );
}

#[test]
fn some_payload_explicit_vec_construction_has_vec_shape() {
    // `Some([1, 2])` bound to `Option<Vec<i64>>` must record the
    // payload literal as a Vec, not a fixed `[i64; 2]`.
    let checked = run("fn main() { let x: Option<Vec<i64>> = Some(Vec::from([1, 2])) }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let ItemKind::Fn(decl) = &checked.source.items[0].kind else {
        panic!("expected fn");
    };
    let body = decl.body.as_ref().unwrap();
    let ExprKind::Block(block) = &body.kind else {
        panic!("expected block");
    };
    let StmtKind::Let { init, .. } = &block.stmts[0].kind else {
        panic!("expected let");
    };
    let init = init.as_ref().unwrap();
    let ExprKind::Call { args, .. } = &init.kind else {
        panic!("expected Some(..) call");
    };
    let ty = checked.table.get(args[0].id).expect("payload typed");
    assert!(
        matches!(checked.tcx.kind(ty), Some(TyKind::Vec(_))),
        "Some(..) payload literal must adopt the expected Vec shape, got {:?}",
        checked.tcx.kind(ty)
    );
}

/// Recursively finds the first closure expression nested in `expr`.
fn find_closure(expr: &gossamer_ast::Expr) -> Option<&gossamer_ast::Expr> {
    match &expr.kind {
        ExprKind::Closure { .. } => Some(expr),
        ExprKind::Call { callee, args } => {
            find_closure(callee).or_else(|| args.iter().find_map(find_closure))
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            find_closure(receiver).or_else(|| args.iter().find_map(find_closure))
        }
        ExprKind::Binary { lhs, rhs, .. } => find_closure(lhs).or_else(|| find_closure(rhs)),
        _ => None,
    }
}

/// Init expression of the `stmt_idx`-th statement (a `let`) in `fn_name`.
fn let_init<'a>(checked: &'a Checked, fn_name: &str, stmt_idx: usize) -> &'a gossamer_ast::Expr {
    for item in &checked.source.items {
        if let ItemKind::Fn(decl) = &item.kind {
            if decl.name.name == fn_name {
                let body = decl.body.as_ref().expect("fn body");
                let ExprKind::Block(block) = &body.kind else {
                    panic!("expected block body");
                };
                let StmtKind::Let { init, .. } = &block.stmts[stmt_idx].kind else {
                    panic!("expected let statement");
                };
                return init.as_ref().expect("let init");
            }
        }
    }
    panic!("fn `{fn_name}` not found");
}

/// Resolved first-parameter type of the first closure nested in `root`.
fn closure_param_kind(checked: &Checked, root: &gossamer_ast::Expr) -> TyKind {
    closure_param_kind_of(checked, find_closure(root).expect("closure expr"))
}

/// [`closure_param_kind`] for the callback inside a `|>` closure step, whose
/// own parameter is the outermost closure.
fn step_callback_param_kind(checked: &Checked, root: &gossamer_ast::Expr) -> TyKind {
    let step = find_closure(root).expect("step closure");
    let ExprKind::Closure { body, .. } = &step.kind else {
        panic!("expected a closure step");
    };
    closure_param_kind_of(checked, find_closure(body).expect("callback closure"))
}

fn closure_param_kind_of(checked: &Checked, closure: &gossamer_ast::Expr) -> TyKind {
    let ty = checked.table.get(closure.id).expect("closure typed");
    match checked.tcx.kind(ty).expect("closure ty kind") {
        TyKind::FnPtr(sig) | TyKind::FnTrait(sig) => {
            let input = *sig.inputs.first().expect("closure has a param");
            checked.tcx.kind(input).expect("param ty kind").clone()
        }
        other => panic!("closure typed as {other:?}, expected a fn type"),
    }
}

#[test]
fn map_err_method_closure_param_pins_to_err_payload_type() {
    let checked = run("fn fail() -> Result<i64, String> { Err(\"boom\") }\n\
         fn main() { let r = fail().map_err(|e| format(\"w: {e}\")) }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let init = let_init(&checked, "main", 0);
    assert!(
        matches!(closure_param_kind(&checked, init), TyKind::String),
        "map_err closure param must pin to the Err payload String"
    );
    // The call itself types as Result<i64, String> (format! output).
    let call_ty = checked.table.get(init.id).expect("call typed");
    let Some(TyKind::Adt { def, substs }) = checked.tcx.kind(call_ty) else {
        panic!("map_err call must type as a Result Adt");
    };
    assert_eq!(checked.tcx.def_name(*def), Some("Result"));
    let payloads = substs.types();
    assert!(matches!(
        checked.tcx.kind(payloads[0]),
        Some(TyKind::Int(gossamer_types::IntTy::I64))
    ));
    assert!(matches!(
        checked.tcx.kind(payloads[1]),
        Some(TyKind::String)
    ));
}

#[test]
fn option_map_method_closure_param_pins_to_payload_type() {
    let checked = run("fn main() { let o: Option<String> = Some(\"x\")\n\
         let m = o.map(|s| format(\"<{s}>\")) }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let init = let_init(&checked, "main", 1);
    assert!(
        matches!(closure_param_kind(&checked, init), TyKind::String),
        "Option::map closure param must pin to the payload String"
    );
    let call_ty = checked.table.get(init.id).expect("call typed");
    let Some(TyKind::Adt { def, substs }) = checked.tcx.kind(call_ty) else {
        panic!("Option::map call must type as an Option Adt");
    };
    assert_eq!(checked.tcx.def_name(*def), Some("Option"));
    assert!(matches!(
        checked.tcx.kind(substs.types()[0]),
        Some(TyKind::String)
    ));
}

#[test]
fn vec_get_returns_option_of_element_type() {
    let checked = run("fn main() { let got: Option<i64> = Vec::from([1, 2]).get(0) }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

#[test]
fn vec_rejects_methods_without_lowering() {
    for method in ["retain", "drain"] {
        let source = format!("fn main() {{ let mut xs = Vec::from([1, 2])\n xs.{method}(0) }}\n");
        let checked = run(&source);
        assert!(
            checked.diagnostics.iter().any(|diag| matches!(
                &diag.error,
                TypeError::UnresolvedMethod { name, .. } if name == method
            )),
            "Vec::{method} should be rejected until it has VM and compiled lowering: {:?}",
            checked.diagnostics
        );
    }
}

#[test]
fn iter_map_free_fn_closure_param_pins_to_elem_type() {
    let checked = run("use std::iter\n\
         fn main() { let xs: Vec<String> = Vec::from([\"a\"])\n\
         let ys = iter::map(|s| format(\"[{s}]\"), xs) }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let init = let_init(&checked, "main", 1);
    assert!(
        matches!(closure_param_kind(&checked, init), TyKind::String),
        "iter::map closure param must pin to the Vec element String"
    );
    let call_ty = checked.table.get(init.id).expect("call typed");
    let Some(TyKind::Vec(elem)) = checked.tcx.kind(call_ty) else {
        panic!("iter::map call must type as Vec");
    };
    assert!(matches!(checked.tcx.kind(*elem), Some(TyKind::String)));
}

#[test]
fn piped_iter_map_closure_param_pins_to_elem_type() {
    let checked = run("use std::iter\n\
         fn main() { let xs: Vec<String> = Vec::from([\"a\"])\n\
         let ys = xs |> |v| iter::map(|s| format(\"({s})\"), v) }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let init = let_init(&checked, "main", 1);
    assert!(
        matches!(step_callback_param_kind(&checked, init), TyKind::String),
        "piped iter::map closure param must pin to the Vec element String"
    );
    let pipe_ty = checked.table.get(init.id).expect("pipe typed");
    let Some(TyKind::Vec(elem)) = checked.tcx.kind(pipe_ty) else {
        panic!("piped iter::map must type as Vec");
    };
    assert!(matches!(checked.tcx.kind(*elem), Some(TyKind::String)));
}

#[test]
fn piped_result_default_with_closure_param_pins_to_err_type() {
    let checked = run("use std::result\n\
         fn fail() -> Result<i64, String> { Err(\"boom\") }\n\
         fn main() { let v = fail() |> |v| result::unwrap_or_else(|e| println(\"{e}\"), v) }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let init = let_init(&checked, "main", 0);
    assert!(
        matches!(step_callback_param_kind(&checked, init), TyKind::String),
        "result::unwrap_or_else closure param must pin to the Err payload String"
    );
}

#[test]
fn result_rejects_option_only_methods() {
    let checked = run("fn main() { let v = \"12\".parse::<i64>().ok_or(\"missing\") }\n");
    assert!(
        checked.diagnostics.iter().any(|d| matches!(
            d.error,
            TypeError::UnresolvedMethod { ref ty, ref name, .. }
                if ty == "Result" && name == "ok_or"
        )),
        "{:?}",
        checked.diagnostics
    );
}

#[test]
fn raw_stdlib_result_helpers_support_question_mark() {
    let checked = run("use std::errors\n\
         fn load(path: String) -> Result<i64, errors::Error> {\n\
             let size, is_file, is_dir, is_symlink, readonly, modified = __gos_fs_metadata_raw(path)?\n\
             Ok(size)\n\
         }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

#[test]
fn unknown_std_combinator_with_closure_errors_loudly() {
    // `iter::mystery` has no checker signature row. A closure passed
    // there is uninferrable, which the compiled tiers would render as
    // a pointer-formatting bug - the checker must reject loudly.
    // (The resolver flags the unknown name separately; this test
    // bypasses the resolve assertion on purpose.)
    let source = "use std::iter\n\
                  fn main() { let xs: Vec<String> = Vec::from([\"a\"])\n\
                  let ys = iter::mystery(|x| x, xs) }\n";
    let mut map = SourceMap::new();
    let file = map.add_file("test.gos", source.to_string());
    let (mut sf, parse_diags) = parse_source_file(source, file);
    assert!(parse_diags.is_empty(), "parse errors: {parse_diags:?}");
    let (resolutions, _) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &resolutions);
    let mut tcx = TyCtxt::new();
    let (_, diagnostics) = typecheck_source_file(&sf, &resolutions, &mut tcx);
    assert!(
        diagnostics.iter().any(|d| matches!(
            &d.error,
            TypeError::ClosureParamUninferred { combinator } if combinator == "iter::mystery"
        )),
        "expected ClosureParamUninferred for iter::mystery, got {diagnostics:?}"
    );
}

#[test]
fn i128_type_annotation_rejected_with_gt0014() {
    let checked = run("fn main() { let x: i128 = 1 }\n");
    assert!(
        checked
            .diagnostics
            .iter()
            .any(|d| matches!(&d.error, TypeError::Int128Unsupported { ty } if ty == "i128")),
        "expected Int128Unsupported for `i128`, got {:?}",
        checked.diagnostics,
    );
}

#[test]
fn u128_param_and_return_rejected_with_gt0014() {
    let checked = run("fn f(x: u128) -> u128 { x }\nfn main() { }\n");
    assert!(
        checked
            .diagnostics
            .iter()
            .any(|d| matches!(&d.error, TypeError::Int128Unsupported { ty } if ty == "u128")),
        "expected Int128Unsupported for `u128`, got {:?}",
        checked.diagnostics,
    );
}

#[test]
fn i128_literal_suffix_rejected_with_gt0014() {
    let checked = run("fn main() { let y = 1i128 }\n");
    assert!(
        checked
            .diagnostics
            .iter()
            .any(|d| matches!(&d.error, TypeError::Int128Unsupported { ty } if ty == "i128")),
        "expected Int128Unsupported for the `1i128` suffix, got {:?}",
        checked.diagnostics,
    );
}

#[test]
fn cast_target_i128_rejected_with_gt0014() {
    let checked = run("fn main() { let z = 1 as i128 }\n");
    assert!(
        checked
            .diagnostics
            .iter()
            .any(|d| matches!(&d.error, TypeError::Int128Unsupported { ty } if ty == "i128")),
        "expected Int128Unsupported for `as i128`, got {:?}",
        checked.diagnostics,
    );
}

#[test]
fn bool_and_char_casts_pass_the_whitelist() {
    let checked = run("fn main() {\n\
         let a = true as i64\n\
         let b = 'a' as u8\n\
         let c = 65 as u8 as char\n\
         let d = false as u64\n\
         }\n");
    assert!(
        checked.diagnostics.is_empty(),
        "whitelisted bool/char casts must typecheck: {:?}",
        checked.diagnostics,
    );
}

// ---------------------------------------------------------------
// Task 22 - std fns as first-class values (GT0015 + tabled set).
// ---------------------------------------------------------------

fn diagnostics_for(source: &str) -> Vec<gossamer_types::TypeDiagnostic> {
    let mut map = SourceMap::new();
    let file = map.add_file("test.gos", source.to_string());
    let (mut sf, parse_diags) = parse_source_file(source, file);
    assert!(parse_diags.is_empty(), "parse errors: {parse_diags:?}");
    let (resolutions, _) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &resolutions);
    let mut tcx = TyCtxt::new();
    let (_, diagnostics) = typecheck_source_file(&sf, &resolutions, &mut tcx);
    diagnostics
}

#[test]
fn a_macro_path_as_value_is_left_to_the_resolver() {
    // Every std function with a fixed parameter list is rewritten into
    // the closure that calls it before the checker runs. A macro path is
    // not a function at all, and the resolver already reports it as one
    // (GR0018) - a second report about parameter lists would describe the
    // wrong thing.
    let source = "use std::fmt\n\
                  fn main() { let out = #[\"ab\"].map(fmt::format)\n\
                  let _ = out }\n";
    let diagnostics = diagnostics_for(source);
    assert!(
        !diagnostics
            .iter()
            .any(|d| matches!(&d.error, TypeError::StdFnValueUnsupported { .. })),
        "a macro path is the resolver's to report, got {diagnostics:?}"
    );
}

#[test]
fn tabled_std_fn_as_value_typechecks_clean() {
    let source = "use std::errors\n\
                  fn main() { let r: Result<i64, String> = Err(\"boom\")\n\
                  let m = r.map_err(errors::new)\nlet _ = m }\n";
    let diagnostics = diagnostics_for(source);
    assert!(
        diagnostics.is_empty(),
        "tabled std fn value must typecheck clean, got {diagnostics:?}"
    );
}

#[test]
fn json_encode_of_an_enum_errors_with_gt0016() {
    // The classic missing-`?`: `json::parse` returns a Result, so
    // encoding it (instead of the unwrapped Value) is rejected.
    let source = "use std::encoding::json\n\
                  fn main() { let v = json::parse(\"{}\")\n\
                  let _ = json::encode(v) }\n";
    let diagnostics = diagnostics_for(source);
    assert!(
        diagnostics.iter().any(
            |d| matches!(&d.error, TypeError::JsonNotSerializable { op, .. } if op == "encode")
        ),
        "expected JsonNotSerializable for json::encode of a Result, got {diagnostics:?}"
    );
}

#[test]
fn json_encode_of_value_scalar_and_struct_typechecks_clean() {
    // json::Value, scalars, and structs are all valid encode inputs.
    let source = "use std::encoding::json\n\
                  struct P { x: i64 }\n\
                  fn main() {\n\
                  let _ = json::encode(42)\n\
                  let _ = json::encode(P { x: 1 })\n\
                  let v = json::parse(\"{}\").unwrap()\n\
                  let _ = json::encode(v) }\n";
    let diagnostics = diagnostics_for(source);
    assert!(
        !diagnostics
            .iter()
            .any(|d| matches!(&d.error, TypeError::JsonNotSerializable { .. })),
        "valid json::encode inputs must not trip GT0016, got {diagnostics:?}"
    );
}

#[test]
fn std_fn_in_callee_position_stays_legal() {
    // Call and pipe-rhs positions are the normal stdlib call shapes;
    // GT0015 only fires on genuine value positions.
    let source = "use std::strings\n\
                  fn main() { let a = strings::repeat(\"ab\", 2)\n\
                  let b = \"x\" |> |v| strings::repeat(v, 2)\nlet _ = a\nlet _ = b }\n";
    let diagnostics = diagnostics_for(source);
    assert!(
        !diagnostics
            .iter()
            .any(|d| matches!(&d.error, TypeError::StdFnValueUnsupported { .. })),
        "callee positions must not trip GT0015, got {diagnostics:?}"
    );
}

#[test]
fn bare_std_path_as_pipe_rhs_stays_legal() {
    let source = "use std::option\n\
                  fn main() { let o: Option<i64> = Some(1)\n\
                  let v = o |> option::is_some\nlet _ = v }\n";
    let diagnostics = diagnostics_for(source);
    assert!(
        !diagnostics
            .iter()
            .any(|d| matches!(&d.error, TypeError::StdFnValueUnsupported { .. })),
        "pipe-rhs bare std paths must not trip GT0015, got {diagnostics:?}"
    );
}

fn has_code(diags: &[gossamer_types::TypeDiagnostic], code: &str) -> bool {
    diags.iter().any(|d| d.error.code() == code)
}

// 0.18.1 authoritativeness fixes: each program below passed `gos check`
// on 0.18.0 and then SIGSEGV'd, printed garbage, or failed to build on
// the compiled tier. The checker now rejects them so "if it builds it
// runs" holds. Each test also has a valid sibling that must NOT trip.

#[test]
fn index_on_scalar_is_rejected() {
    // 0.18.0: compiled tier read through the i64 as a pointer (SIGSEGV).
    let d = diagnostics_for("fn main() { let x = 5\n let y = x[0]\n println(\"{}\", y) }\n");
    assert!(has_code(&d, "GT0021"), "{d:?}");
}

#[test]
fn index_on_a_lazy_iterator_is_rejected() {
    let d = diagnostics_for("fn main() { let xs = 0..3\n let y = xs[0]\n let _ = y }\n");
    assert!(has_code(&d, "GT0021"), "{d:?}");
}

#[test]
fn formatting_a_lazy_iterator_is_rejected() {
    let d = diagnostics_for("fn main() { let xs = 0..3\n println(\"{}\", xs) }\n");
    assert!(has_code(&d, "GT0041"), "{d:?}");
}

#[test]
fn lazy_iterator_step_by_is_accepted() {
    let d = diagnostics_for(
        "use std::iter\nfn main() { let xs = iter::range(0, 9) |> |v| iter::step_by(v, 2)\n let _ = xs }\n",
    );
    assert!(d.is_empty(), "{d:?}");
}

#[test]
fn reusing_consumed_lazy_iterator_is_rejected() {
    let d = diagnostics_for(
        "use std::iter\nfn main() { let xs = 0..3\n let out = iter::collect(xs)\n let _ = xs\n let _ = out }\n",
    );
    assert!(has_code(&d, "GT0042"), "{d:?}");
}

#[test]
fn iterator_parameters_cannot_be_reused_after_consuming_methods_or_for_loops() {
    for source in [
        "use Iterator\nfn consume(r: Iterator<i64>) { let n = r.count()\n let _ = r\n let _ = n }\n",
        "use Iterator\nfn consume(r: Iterator<i64>) { for i in r { let _ = i }\n let _ = r }\n",
    ] {
        let d = diagnostics_for(source);
        assert!(has_code(&d, "GT0042"), "{source}: {d:?}");
    }
}

#[test]
fn reusing_pipe_consumed_lazy_iterator_is_rejected() {
    let d = diagnostics_for(
        "use std::iter\nfn main() { let xs = 0..3\n let out = xs |> |v| iter::take(v, 1)\n let _ = xs\n let _ = out }\n",
    );
    assert!(has_code(&d, "GT0042"), "{d:?}");
}

/// A collection already holds its values, so it traverses them eagerly.
/// `iter()` is how a caller asks for the lazy walk instead.
#[test]
fn a_collection_traverses_its_own_values() {
    for source in [
        "fn main() { let xs = #[1, 2, 3]\n let _ = xs.map(|x| x * 2) }\n",
        "fn main() { let xs = #[1, 2, 3]\n let _ = xs.filter(|x| x > 1) }\n",
        "fn main() { let xs = #[1, 2, 3]\n let _ = xs.sum() }\n",
        "fn main() { let xs = #[1, 2, 3]\n let _ = xs.fold(0, |a, x| a + x) }\n",
        "fn main() { let xs = #[1, 2, 3]\n let _ = xs.rev() }\n",
        "fn main() { let a: [i64; 3] = [1, 2, 3]\n let _ = a.map(|x| x) }\n",
    ] {
        let d = diagnostics_for(source);
        assert!(d.is_empty(), "{source}: {d:?}");
    }
}

/// A set has no element order, so a traversal on one is the iterator's and is
/// written through `iter()`. The set itself answers membership, cardinality,
/// and set algebra.
#[test]
fn a_set_traverses_through_its_iterator() {
    for source in [
        "fn main() { let s = #{1, 2, 3}\n let _ = s.iter().map(|x| x * 2).collect() }\n",
        "fn main() { let s = #{1, 2, 3}\n let _ = s.iter().filter(|x| x > 1).collect() }\n",
        "fn main() { let s = #{1, 2, 3}\n let _ = s.iter().take(2).collect() }\n",
        "fn main() { let s = #{1, 2, 3}\n let _ = s.len() + s.to_vec().len() }\n",
        "fn main() { let s = #{1, 2, 3}\n let _ = s.union(#{4}).contains(4) }\n",
    ] {
        let d = diagnostics_for(source);
        assert!(d.is_empty(), "{source}: {d:?}");
    }
    for source in [
        "fn main() { let s = #{1, 2, 3}\n let _ = s.map(|x| x * 2) }\n",
        "fn main() { let s = #{1, 2, 3}\n let _ = s.take(2) }\n",
        "fn main() { let s = #{1, 2, 3}\n let _ = s.enumerate() }\n",
    ] {
        let d = diagnostics_for(source);
        assert!(!d.is_empty(), "{source}: a set has no sequence surface");
    }
}

/// A traversal answers eagerly on the collection and lazily through `iter()`,
/// so the two spellings differ in when the work runs, not in what they mean.
#[test]
fn a_traversal_is_eager_on_a_collection_and_lazy_through_iter() {
    let eager =
        "fn main() { let xs = #[1, 2, 3]\n let v: Vec<i64> = xs.map(|x| x * 2)\n let _ = v }\n";
    assert!(diagnostics_for(eager).is_empty());
    let lazy = "fn main() { let xs = #[1, 2, 3]\n let it: Iterator<i64> = xs.iter().map(|x| x * 2)\n let _ = it }\n";
    assert!(diagnostics_for(lazy).is_empty());
}

/// The same operations through `.iter()`, and the collection surface that
/// describes or mutates rather than traverses, all stay accepted.
#[test]
fn the_iterator_surface_and_the_collection_surface_both_still_work() {
    for source in [
        "fn main() { let xs = #[1, 2, 3]\n let _ = xs.iter().map(|x| x * 2).collect() }\n",
        "fn main() { let xs = #[1, 2, 3]\n let _ = xs.iter().sum() }\n",
        "fn main() { let xs = #[1, 2, 3]\n let _ = xs.iter().rev().collect() }\n",
        // Collection operations: length, membership, ordering, copying.
        "fn main() { let xs = #[1, 2, 3]\n let _ = xs.len() }\n",
        "fn main() { let xs = #[1, 2, 3]\n let _ = xs.contains(2) }\n",
        // A Vec is already owned, so `to_vec` belongs to the borrowed and
        // fixed-length sequences; a slice of one still converts.
        "fn main() { let xs = #[1, 2, 3]\n let _ = xs.slice(0, 2) }\n",
        "fn main() { let mut xs = #[1, 2, 3]\n xs.sort()\n let _ = xs }\n",
        // `for` still iterates a collection directly.
        "fn main() { let xs = #[1, 2, 3]\n let mut t = 0\n for x in xs { t += x }\n let _ = t }\n",
    ] {
        let d = diagnostics_for(source);
        assert!(d.is_empty(), "{source}: {d:?}");
    }
}

#[test]
fn rebinding_a_consumed_iterator_name_starts_a_fresh_binding() {
    for source in [
        "use std::iter\nfn main() { let xs = iter::range(0, 3)\n let out = iter::collect(xs)\n let xs = iter::range(0, 4)\n let _ = iter::collect(xs)\n let _ = out }\n",
        "use std::iter\nfn main() { let xs = iter::range(0, 3)\n let out = iter::collect(xs)\n let xs = 10\n let _ = xs\n let _ = out }\n",
    ] {
        let d = diagnostics_for(source);
        assert!(!has_code(&d, "GT0042"), "{source}: {d:?}");
    }
}

#[test]
fn index_on_vec_and_string_is_accepted() {
    let d = diagnostics_for(
        "fn main() { let xs = [1, 2, 3]\n let s = \"hi\"\n println(\"{} {}\", xs[0], s.byte_at(0)) }\n",
    );
    assert!(!has_code(&d, "GT0021"), "{d:?}");
}

#[test]
fn reasonable_fixed_array_is_accepted() {
    let d = diagnostics_for("fn main() { let a: [i64; 16] = [0; 16]\n println(\"{}\", a[0]) }\n");
    assert!(d.is_empty(), "{d:?}");
}

#[test]
fn benchmark_sized_fixed_array_is_accepted() {
    let d = diagnostics_for(
        "fn main() { let a: [f64; 40000] = [0.0; 40000]\n println(\"{}\", a[0]) }\n",
    );
    assert!(d.is_empty(), "{d:?}");
}

#[test]
fn very_large_fixed_array_is_accepted() {
    let d = diagnostics_for("fn main() { let a: [i64; 100000000] = [0; 100000000]\n let _ = a }\n");
    assert!(d.is_empty(), "{d:?}");
}

#[test]
fn owned_slice_repeat_is_rejected_as_unsized() {
    let d = diagnostics_for("fn main() { let v: [i64] = [0; 100000000]\n let _ = v.len() }\n");
    assert!(
        d.iter()
            .any(|diagnostic| { matches!(diagnostic.error, TypeError::UnsizedSliceValue { .. }) }),
        "{d:?}"
    );
}

#[test]
fn call_of_scalar_value_is_rejected() {
    // 0.18.0: compiled tier emitted a call through a non-function symbol.
    let d = diagnostics_for("fn main() { let x = 5\n let y = x(3)\n println(\"{}\", y) }\n");
    assert!(has_code(&d, "GT0022"), "{d:?}");
}

#[test]
fn qualified_associated_call_is_not_flagged_as_non_callable() {
    // `String::new()` types its callee as `String`; it must not trip GT0022.
    let d = diagnostics_for("fn main() { let s = String::new()\n println(\"{}\", s) }\n");
    assert!(!has_code(&d, "GT0022"), "{d:?}");
}

#[test]
fn constructor_calls_are_not_flagged_as_non_callable() {
    let d = diagnostics_for("fn main() { let o = Some(5)\n let r = Ok(1)\n println(\"ok\") }\n");
    assert!(!has_code(&d, "GT0022"), "{d:?}");
}

#[test]
fn empty_named_struct_accepts_bare_construction() {
    let checked = run("struct Unit {}\nfn main() { let u = Unit\n let _ = u }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

#[test]
fn named_struct_associated_function_is_not_checked_as_constructor() {
    let checked = run(
        "struct Pt { x: i64, y: i64 }\nimpl Pt { fn origin() -> Pt { Pt { x: 0, y: 0 } } }\nfn main() { let p = Pt::origin() }\n",
    );
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

#[test]
fn struct_destructuring_requires_the_nominal_name() {
    let d = diagnostics_for(
        "struct Point { x: i64, y: i64 }\nfn main() { let p = Point { x: 1, y: 2 }\n let x, y = p }\n",
    );
    assert!(has_code(&d, "GT0033"), "{d:?}");

    let d = diagnostics_for("fn main() { let pair = (1, 2)\n let x, y = pair }\n");
    assert!(!has_code(&d, "GT0033"), "{d:?}");
}

#[test]
fn named_struct_construction_requires_braces() {
    let d = diagnostics_for("struct Point { x: i64, y: i64 }\nfn main() { let _ = Point(1, 2) }\n");
    assert!(has_code(&d, "GT0034"), "{d:?}");

    let d = diagnostics_for(
        "struct Point { x: i64, y: i64 }\nfn main() { let _ = Point { x: 1, y: 2 } }\n",
    );
    assert!(!has_code(&d, "GT0034"), "{d:?}");

    let d =
        diagnostics_for("struct Point { x: i64, y: i64 }\nfn main() { let _ = Point { 1, 2 } }\n");
    assert!(has_code(&d, "GT0034"), "{d:?}");

    let d = diagnostics_for(
        "struct Point { x: i64, y: i64 }\nfn main() { let _ = Point { y: 2, 1 } }\n",
    );
    assert!(has_code(&d, "GT0034"), "{d:?}");
}

#[test]
fn unit_and_empty_named_struct_construction_are_distinct() {
    let d = diagnostics_for(
        "struct Unit\nstruct Empty {}\nfn main() { let _ = Unit\nlet _ = Unit {}\nlet _ = Empty {} }\n",
    );
    assert!(!has_code(&d, "GT0034"), "{d:?}");

    let d = diagnostics_for(
        "struct Unit\nstruct Empty {}\nfn main() { let _ = Unit()\nlet _ = Empty }\n",
    );
    assert!(has_code(&d, "GT0034"), "{d:?}");
}

#[test]
fn tuple_struct_construction_requires_parentheses() {
    let checked = run("struct Point(i64, i64)\nfn main() { let _ = Point }\n");
    let diagnostic = checked
        .diagnostics
        .iter()
        .find(|diagnostic| {
            matches!(
                diagnostic.error,
                TypeError::TupleStructConstructorParenthesesRequired { .. }
            )
        })
        .expect("bare tuple struct should require parentheses")
        .to_diagnostic();
    assert!(
        diagnostic.title.contains("constructed with parentheses"),
        "{}",
        diagnostic.title
    );
    assert!(!diagnostic.title.contains("braces"), "{}", diagnostic.title);

    let d = diagnostics_for("struct Point(i64, i64)\nfn main() { let _ = Point { x: 1, y: 2 } }\n");
    assert!(has_code(&d, "GT0034"), "{d:?}");

    let d = diagnostics_for("struct Point(i64, i64)\nfn main() { let _ = Point(1, 2) }\n");
    assert!(!has_code(&d, "GT0034"), "{d:?}");
}

#[test]
fn named_struct_literal_fields_are_checked() {
    let d =
        diagnostics_for("struct Point { x: i64, y: i64 }\nfn main() { let _ = Point { x: 1 } }\n");
    assert!(has_code(&d, "GT0035"), "{d:?}");

    let d = diagnostics_for(
        "struct Point { x: i64, y: i64 }\nfn main() { let _ = Point { x: 1, x: 2, y: 3 } }\n",
    );
    assert!(has_code(&d, "GT0036"), "{d:?}");

    let d = diagnostics_for(
        "struct Point { x: i64, y: i64 }\nfn main() { let _ = Point { x: 1, y: 2, z: 3 } }\n",
    );
    assert!(has_code(&d, "GT0006"), "{d:?}");

    let d = diagnostics_for("struct Point { x: i64, y: i64 }\nfn main() { let _ = Point { 1 } }\n");
    assert!(has_code(&d, "GT0035"), "{d:?}");

    let d = diagnostics_for(
        "struct Point { x: i64, y: i64 }\nfn main() { let _ = Point { 1, 2, 3 } }\n",
    );
    assert!(has_code(&d, "GT0037"), "{d:?}");

    let d = diagnostics_for(
        "struct Point { x: i64, y: i64 }\nfn main() { let _ = Point { x: 1, y: 2, 3 } }\n",
    );
    assert!(has_code(&d, "GT0037"), "{d:?}");
}

#[test]
fn out_of_range_tuple_index_is_rejected() {
    // 0.18.0: compiled tier read out-of-object memory (garbage / leak).
    let d = diagnostics_for("fn main() { let t = (1, 2)\n let x = t.5\n println(\"{}\", x) }\n");
    assert!(has_code(&d, "GT0023"), "{d:?}");
}

#[test]
fn positional_index_on_struct_is_rejected() {
    let d = diagnostics_for(
        "struct P { x: i64, y: i64 }\nfn main() { let p = P { x: 1, y: 2 }\n let v = p.0\n println(\"{}\", v) }\n",
    );
    assert!(has_code(&d, "GT0023"), "{d:?}");
}

#[test]
fn in_range_tuple_index_is_accepted() {
    let d = diagnostics_for("fn main() { let t = (1, 2, 3)\n println(\"{} {}\", t.0, t.2) }\n");
    assert!(!has_code(&d, "GT0023"), "{d:?}");
}

#[test]
fn method_call_with_wrong_arity_is_rejected() {
    // 0.18.0: VM aborted (GX0003) but the compiled tier zero-filled the
    // missing argument and returned a wrong result (tier divergence).
    let d = diagnostics_for(
        "struct A { x: i64 }\nimpl A { fn add(&self, a: i64, b: i64) -> i64 { self.x + a + b } }\nfn main() { let a = A { x: 1 }\n println(\"{}\", a.add(2)) }\n",
    );
    assert!(has_code(&d, "GT0018"), "{d:?}");
}

#[test]
fn method_call_with_correct_arity_is_accepted() {
    let d = diagnostics_for(
        "struct A { x: i64 }\nimpl A { fn add(&self, a: i64, b: i64) -> i64 { self.x + a + b } }\nfn main() { let a = A { x: 1 }\n println(\"{}\", a.add(2, 3)) }\n",
    );
    assert!(!has_code(&d, "GT0018"), "{d:?}");
}

#[test]
fn piped_method_call_counts_the_implicit_argument() {
    // `5 |> a.add(2)` desugars to `a.add(2, 5)`: arity is satisfied.
    let d = diagnostics_for(
        "struct A { x: i64 }\nimpl A { fn add(&self, a: i64, b: i64) -> i64 { self.x + a + b } }\nfn main() { let a = A { x: 1 }\n println(\"{}\", 5 |> |v| a.add(2, v)) }\n",
    );
    assert!(!has_code(&d, "GT0018"), "{d:?}");
}

#[test]
fn nonexistent_method_on_user_struct_is_rejected() {
    // 0.18.0: a typo passed check; the compiled build failed on an
    // undefined `@A::bogus` symbol.
    let d = diagnostics_for(
        "struct A { x: i64 }\nfn main() { let a = A { x: 1 }\n let y = a.bogus()\n println(\"{}\", y) }\n",
    );
    assert!(has_code(&d, "GT0002"), "{d:?}");
}

#[test]
fn real_method_on_user_struct_is_accepted() {
    let d = diagnostics_for(
        "struct A { x: i64 }\nimpl A { fn get(&self) -> i64 { self.x } }\nfn main() { let a = A { x: 1 }\n println(\"{}\", a.get()) }\n",
    );
    assert!(!has_code(&d, "GT0002"), "{d:?}");
}

#[test]
fn hashmap_keys_with_aggregate_key_yields_the_key_type() {
    // An aggregate key snapshots back as the value the program wrote, so
    // `keys()` types as `Vec<K>` for a struct key just as it does for a
    // scalar one.
    let d = diagnostics_for(
        "use std::collections::Map\nstruct Point { x: i64, y: i64 }\nfn main() { let m: Map<Point, i64> = Map::new()\n let _ = m.keys()\n }\n",
    );
    assert!(!has_code(&d, "GT0002"), "{d:?}");
}

#[test]
fn hashmap_keys_with_scalar_key_remains_available() {
    let d = diagnostics_for(
        "use std::collections::Map\nfn main() { let m: Map<i64, i64> = Map::new()\n let _ = m.keys()\n }\n",
    );
    assert!(d.is_empty(), "scalar Map keys should typecheck: {d:?}");
}

#[test]
fn strings_free_fn_rejects_integer_in_string_slot() {
    // 0.18.x: an integer in a `String` parameter of a `strings::` free
    // function passed check, then the compiled string shim dereferenced
    // it as a pointer (SIGSEGV the VM masked).
    let d = diagnostics_for(
        "use std::strings\nfn main() { let r = strings::contains(\"hello\", 5)\nprintln(\"{}\", r) }\n",
    );
    assert!(
        d.iter().any(
            |x| matches!(&x.error, TypeError::ArgumentTypeMismatch { callee, parameter, expected, found, .. }
                if callee == "strings::contains" && parameter == "needle"
                    && expected == "String | char" && found == "i64")
        ),
        "expected String/i64 mismatch, got {d:?}"
    );
}

#[test]
fn strings_free_fn_rejects_misordered_integer_argument() {
    // `splitn(text, n, sep)`: an integer landing in the `sep` slot is a
    // mis-ordered call that the compiled tier would crash on.
    let d = diagnostics_for(
        "use std::strings\nfn main() { let p = strings::splitn(\"a,b\", 2, 5)\nprintln(\"{}\", p.len()) }\n",
    );
    assert!(
        d.iter()
            .any(|x| matches!(&x.error, TypeError::ArgumentTypeMismatch { .. })),
        "expected a type mismatch for the integer separator, got {d:?}"
    );
}

#[test]
fn strings_free_fn_rejects_float_in_string_slot() {
    // An unsuffixed float literal uses the standard `f64` diagnostic spelling.
    let d = diagnostics_for(
        "use std::strings\nfn main() { let r = strings::contains(\"hi\", 1.5)\nprintln(\"{}\", r) }\n",
    );
    assert!(
        d.iter().any(
            |x| matches!(&x.error, TypeError::ArgumentTypeMismatch { callee, parameter, expected, found, .. }
                if callee == "strings::contains" && parameter == "needle"
                    && expected == "String | char" && found == "f64")
        ),
        "expected String/f64 mismatch, got {d:?}"
    );
}

#[test]
fn user_fn_rejects_float_in_string_parameter() {
    let d = diagnostics_for(
        "fn f(s: String) -> i64 { s.len() }\nfn main() { println(\"{}\", f(1.5)) }\n",
    );
    assert!(
        d.iter().any(
            |x| matches!(&x.error, TypeError::TypeMismatch { expected, found }
                if expected == "String" && found == "f64")
        ),
        "expected String/f64 mismatch, got {d:?}"
    );
}

#[test]
fn string_method_rejects_integer_in_string_slot() {
    // The same crash via method form: `s.contains(5)` dispatches to the
    // string shim with the receiver as the implicit first argument.
    let d = diagnostics_for(
        "fn main() { let s = \"hi\"\nlet r = s.contains(5)\nprintln(\"{}\", r) }\n",
    );
    assert!(
        d.iter().any(
            |x| matches!(&x.error, TypeError::ArgumentTypeMismatch { callee, parameter, expected, found, .. }
                if callee == "String::contains" && parameter == "needle"
                    && expected == "String | char" && found == "i64")
        ),
        "expected String/i64 mismatch, got {d:?}"
    );
}

#[test]
fn string_method_accepts_string_and_char_patterns() {
    let d = diagnostics_for(
        "fn main() {\n\
         let s = \"hello world\"\n\
         let _ = s.contains(\"world\")\n\
         let _ = s.contains('w')\n\
         let _ = s.replace(\"o\", \"0\")\n\
         let _ = s.splitn(2, \" \")\n\
         }\n",
    );
    assert!(
        !d.iter()
            .any(|x| matches!(&x.error, TypeError::TypeMismatch { .. })),
        "valid string-method calls must type clean, got {d:?}"
    );
}

#[test]
fn string_method_surface_covers_receiver_shaped_strings_functions() {
    let d = diagnostics_for(
        "fn main() {\n\
         let s = \" hello world \"\n\
         let _ = s.bytes()\n\
         let _ = s.center(15, ' ')\n\
         let _ = s.chars()\n\
         let _ = s.contains(\"world\")\n\
         let _ = s.contains_any(\"aeiou\")\n\
         let _ = s.count('l')\n\
         let _ = s.ends_with(\" \")\n\
         let _ = s.equal_fold(\" HELLO WORLD \")\n\
         let _ = s.find(\"world\")\n\
         let _ = s.find_any(\"od\")\n\
         let _ = s.lines()\n\
         let _ = s.pad_left(16, '.')\n\
         let _ = s.pad_right(16, '.')\n\
         let _ = s.repeat(2)\n\
         let _ = s.replace(\"l\", \"L\")\n\
         let _ = s.replacen(\"l\", \"L\", 1)\n\
         let _ = s.rfind(\"l\")\n\
         let _ = s.rfind_any(\"le\")\n\
         let _ = s.rsplit_once(\" \")\n\
         let _ = s.slice(1, 5)\n\
         let _ = s.split(\" \")\n\
         let _ = s.split_once(\" \")\n\
         let _ = s.split_whitespace()\n\
         let _ = s.splitn(2, \" \")\n\
         let _ = s.starts_with(\" \")\n\
         let _ = s.strip_prefix(\" \")\n\
         let _ = s.strip_suffix(\" \")\n\
         let _ = s.to_bool()\n\
         let _ = s.to_f64()\n\
         let _ = s.to_i64()\n\
         let _ = s.to_lowercase()\n\
         let _ = s.to_title()\n\
         let _ = s.to_uppercase()\n\
         let _ = s.trim()\n\
         let _ = s.trim_end()\n\
         let _ = s.trim_end_matches(\" \")\n\
         let _ = s.trim_matches(\" \")\n\
         let _ = s.trim_start()\n\
         let _ = s.trim_start_matches(\" \")\n\
         }\n",
    );
    assert!(
        !d.iter().any(|x| matches!(
            &x.error,
            TypeError::UnresolvedMethod { ty, .. } if ty == "String"
        )),
        "all receiver-shaped strings functions must work as String methods: {d:?}"
    );
    assert!(
        d.is_empty(),
        "valid string method surface must type clean: {d:?}"
    );
}

#[test]
fn strings_join_is_not_a_string_method() {
    let d = diagnostics_for("fn main() { let _ = \"a\".join(\"-\") }\n");
    assert!(
        d.iter().any(|x| matches!(
            &x.error,
            TypeError::UnresolvedMethod { ty, name, .. } if ty == "String" && name == "join"
        )),
        "`strings::join(parts, sep)` belongs to Vec, not String: {d:?}"
    );
}

#[test]
fn sequence_combinator_with_no_arguments_reports_its_parameter_count() {
    // The receiver does have `map`; what it does not have is a nullary one.
    let d = diagnostics_for("fn main() { let xs = #[1, 2]\n let _ = xs.map() }\n");
    assert!(
        d.iter().any(|x| matches!(
            &x.error,
            TypeError::CallArityMismatch { callee, expected, found }
                if callee == "map" && *expected == 1 && *found == 0
        )),
        "a combinator called with the wrong count reports the count: {d:?}"
    );
}

#[test]
fn sequence_method_the_receiver_lacks_still_reports_as_unknown() {
    let d = diagnostics_for("fn main() { let xs = #[1, 2]\n let _ = xs.nope() }\n");
    assert!(
        d.iter().any(|x| matches!(
            &x.error,
            TypeError::UnresolvedMethod { name, .. } if name == "nope"
        )),
        "a name the receiver does not declare is still unknown: {d:?}"
    );
}

#[test]
fn with_capacity_on_a_collection_that_does_not_declare_it_is_unknown() {
    let d = diagnostics_for(
        "fn main() { let _: Vec<i64> = Vec::with_capacity(4)\n let _: Map<i64, i64> = Map::with_capacity(4)\n let _: BTreeMap<i64, i64> = BTreeMap::with_capacity(4) }\n",
    );
    let unknown: Vec<_> = d
        .iter()
        .filter(|x| matches!(&x.error, TypeError::UnresolvedMethod { .. }))
        .collect();
    assert!(
        matches!(
            unknown.as_slice(),
            [x] if matches!(&x.error, TypeError::UnresolvedMethod { ty, name, .. } if ty == "BTreeMap" && name == "with_capacity")
        ),
        "only BTreeMap lacks with_capacity: {d:?}"
    );
}

#[test]
fn strings_free_fn_accepts_string_and_char_patterns() {
    // A real string needle, a `char` needle, and a `char` pad all type
    // cleanly - the validation must not reject the legitimate shapes.
    let d = diagnostics_for(
        "use std::strings\nfn main() {\n\
         let s = \"hello\"\n\
         let _ = strings::contains(s, \"ell\")\n\
         let _ = strings::contains(s, 'e')\n\
         let _ = strings::replace(s, \"l\", \"L\")\n\
         let _ = strings::pad_left(\"7\", 4, '0')\n\
         let _ = strings::repeat(\"ab\", 3)\n\
         }\n",
    );
    assert!(
        !d.iter()
            .any(|x| matches!(&x.error, TypeError::TypeMismatch { .. })),
        "valid string-function calls must type clean, got {d:?}"
    );
}

#[test]
fn strings_free_fn_accepts_inferred_borrowed_text_values() {
    let d = diagnostics_for(
        "use std::{metrics, strings, trace}\n\
         fn main() {\n\
         let c = metrics::Counter::new(\"requests_total\", \"total requests\")\n\
         let r = metrics::Registry::new()\n\
         r.register(c)\n\
         let text = r.render()\n\
         let _ = strings::contains(text, \"requests_total\")\n\
         let tracer = trace::Tracer::new()\n\
         let span = tracer.start_span(\"checkout\")\n\
         let ended = span.end()\n\
         let json = ended.to_otlp_json()\n\
         let _ = strings::contains(json, \"checkout\")\n\
         }\n",
    );
    assert!(
        !d.iter().any(|x| matches!(
            &x.error,
            TypeError::ArgumentTypeMismatch { callee, parameter, found, .. }
                if callee == "strings::contains" && parameter == "text" && found.starts_with('?')
        )),
        "borrowed inferred String values must not be reported as unresolved variables: {d:?}"
    );
    assert!(
        d.is_empty(),
        "valid inferred string calls must type clean: {d:?}"
    );
}

#[test]
fn string_slice_rejects_missing_or_non_integer_bounds() {
    let d = diagnostics_for(
        "fn main() {\n\
         let s = \"abcde\"\n\
         let _ = s.slice(1)\n\
         let _ = s.slice(1..3)\n\
         let _ = s.slice(\"a\")\n\
         }\n",
    );
    assert!(
        d.iter().any(|x| matches!(
            x.error,
            TypeError::CallArityMismatch { ref callee, expected: 2, found: 1 }
                if callee == "strings::slice"
        )),
        "missing string-method argument must be rejected: {d:?}"
    );
    assert_eq!(
        d.iter()
            .filter(|x| matches!(x.error, TypeError::CallArityMismatch { .. }))
            .count(),
        3,
        "every one-argument slice spelling must be rejected: {d:?}"
    );
    assert!(
        d.iter()
            .any(|x| matches!(x.error, TypeError::TypeMismatch { .. })),
        "a string bound must be rejected as non-integer: {d:?}"
    );
}

#[test]
fn string_range_index_has_string_type() {
    let checked = run("fn main() { let piece = \"abcd\"[1..3] }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let init = let_init(&checked, "main", 0);
    let ty = checked.table.get(init.id).expect("range index typed");
    assert!(
        matches!(checked.tcx.kind(ty), Some(TyKind::String)),
        "String range index should produce String, got {:?}",
        checked.tcx.kind(ty)
    );
}

#[test]
fn fixed_array_range_index_has_vec_type() {
    let checked = run("fn main() { let piece = [1, 2, 3, 4][1..3] }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let init = let_init(&checked, "main", 0);
    let ty = checked.table.get(init.id).expect("range index typed");
    let Some(TyKind::Vec(elem)) = checked.tcx.kind(ty) else {
        panic!(
            "fixed array range index should produce Vec, got {:?}",
            checked.tcx.kind(ty)
        );
    };
    assert!(
        matches!(
            checked.tcx.kind(*elem),
            Some(TyKind::Int(gossamer_types::IntTy::I64))
        ),
        "fixed array range element should be i64, got {:?}",
        checked.tcx.kind(*elem)
    );
}

#[test]
fn fixed_array_rejects_vec_only_methods() {
    let d = diagnostics_for(
        "fn main() {\n\
         let mut a = [1; 3]\n\
         a.push(4)\n\
         let _ = a.pop()\n\
         a.insert(1, 9)\n\
         a.truncate(1)\n\
         a.reserve(8)\n\
         }\n",
    );
    for name in ["push", "pop", "insert", "truncate", "reserve"] {
        assert!(
            d.iter().any(|diagnostic| matches!(
                &diagnostic.error,
                TypeError::SequenceResizeRequiresVec { ty, method }
                    if ty == "[i64; 3]" && method == name
            )),
            "expected fixed-array `{name}` rejection, got {d:?}"
        );
    }
}

#[test]
fn string_slice_rejects_a_duplicate_receiver_argument() {
    let d = diagnostics_for(
        "fn main() {\n\
         let s = \"world\"\n\
         let _ = s.slice(s, 1, 3)\n\
         let _ = s |> |s| s.slice(s, 1, 3)\n\
         }\n",
    );
    assert!(
        d.iter().any(|x| matches!(
            x.error,
            TypeError::CallArityMismatch { ref callee, expected: 2, found: 3 }
                if callee == "strings::slice"
        )),
        "a repeated String receiver must be an arity error, not silently ignored: {d:?}"
    );
    assert_eq!(
        d.iter()
            .filter(|x| matches!(
                x.error,
                TypeError::CallArityMismatch { ref callee, expected: 2, found: 3 }
                    if callee == "strings::slice"
            ))
            .count(),
        2,
        "the closure form must reject the duplicate receiver too: {d:?}"
    );
}

#[test]
fn strings_free_calls_enforce_complete_arity() {
    let d = diagnostics_for(
        "use std::strings\n\
         fn main() {\n\
         let _ = strings::count(\"abc\")\n\
         let _ = strings::slice(\"abc\", 0, 2)\n\
         }\n",
    );
    assert!(
        d.iter().any(|x| matches!(
            x.error,
            TypeError::CallArityMismatch { ref callee, expected: 2, found: 1 }
                if callee == "strings::count"
        )),
        "missing free-function argument must be rejected: {d:?}"
    );
    assert!(
        !d.iter().any(|x| matches!(
            x.error,
            TypeError::CallArityMismatch { ref callee, .. } if callee == "strings::slice"
        )),
        "valid string slice must retain its three-argument contract: {d:?}"
    );
}

#[test]
fn string_parse_requires_payload_type_when_result_is_unexpected() {
    let d = diagnostics_for(
        "use std::strings\n\
         use std::errors\n\
         fn main() {\n\
         let good_a: Result<i64, errors::Error> = \"12\".parse()\n\
         let good_b: Result<i64, errors::Error> = strings::parse(\"34\")\n\
         let missing_a = \"56\".parse()\n\
         let missing_b = strings::parse(\"78\")\n\
         }\n",
    );
    let uninferred = d
        .iter()
        .filter(|diag| matches!(diag.error, TypeError::GenericReturnTypeUninferred { .. }))
        .count();
    assert_eq!(
        uninferred, 2,
        "untyped parse calls must require a concrete payload type: {d:?}"
    );
}

#[test]
fn string_parse_missing_question_mark_uses_assignment_type_as_payload_hint() {
    let d = diagnostics_for(
        "use std::strings\n\
         fn main() {\n\
         let bad_a: u8 = \"12\".parse()\n\
         let bad_b: u8 = strings::parse(\"34\")\n\
         }\n",
    );
    let mismatches = d
        .iter()
        .filter(|diag| {
            matches!(
                &diag.error,
                TypeError::TypeMismatch { expected, found }
                    if expected == "u8" && found == "Result<u8, errors::Error>"
            )
        })
        .count();
    assert_eq!(
        mismatches, 2,
        "missing-question-mark parse diagnostics must use the concrete target payload: {d:?}"
    );
}

#[test]
fn question_mark_requires_result_or_option_context() {
    let invalid_operand = diagnostics_for(
        "use std::errors\n\
         fn main() -> Result<i64, errors::Error> {\n\
         \"12\"?\n\
         }\n",
    );
    assert!(
        invalid_operand
            .iter()
            .any(|diag| matches!(diag.error, TypeError::QuestionMarkUnsupported { .. })),
        "question mark on a string must be rejected before lowering: {invalid_operand:?}"
    );

    let unit_function = diagnostics_for(
        "use std::errors\n\
         fn main() {\n\
         let bad: u8 = \"12\".parse()?\n\
         }\n",
    );
    assert!(
        unit_function
            .iter()
            .any(|diag| matches!(diag.error, TypeError::QuestionMarkUnsupported { .. })),
        "question mark in a unit-returning function must be rejected: {unit_function:?}"
    );

    let valid = diagnostics_for(
        "use std::{errors, fs}\n\
         fn read_one() -> Result<String, errors::Error> {\n\
         let value = fs::read_to_string(\"input\")?\n\
         Ok(value)\n\
         }\n",
    );
    assert!(
        valid.is_empty(),
        "valid result propagation failed: {valid:?}"
    );
}

#[test]
fn strings_count_rejects_every_non_string_or_char_argument_with_parameter_names() {
    let d = diagnostics_for(
        "use std::strings\n\
         fn main() {\n\
         let _ = strings::count(1, \"a\")\n\
         let _ = strings::count(\"abc\", [1, 2])\n\
         let _ = strings::count((1, 2), \"a\")\n\
         let _ = strings::count(1..2, \"a\")\n\
         let _ = \"abc\".count([1, 2])\n\
         }\n",
    );
    let named: Vec<_> = d
        .iter()
        .filter_map(|diag| match &diag.error {
            TypeError::ArgumentTypeMismatch {
                callee, parameter, ..
            } => Some((callee.as_str(), parameter.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(
        named,
        vec![
            ("strings::count", "text"),
            ("strings::count", "needle"),
            ("strings::count", "text"),
            ("strings::count", "text"),
            ("String::count", "needle"),
        ],
        "every invalid count parameter must be rejected and identified: {d:?}"
    );
}

#[test]
fn named_string_argument_mismatch_includes_the_actual_literal_value() {
    let d = diagnostics_for("use std::strings\nfn main() { let _ = strings::count(\"ab\", 1) }\n");
    let Some(error) = d.iter().find_map(|diag| match &diag.error {
        TypeError::ArgumentTypeMismatch {
            callee,
            parameter,
            found,
            actual,
            ..
        } if callee == "strings::count" && parameter == "needle" => Some((found, actual)),
        _ => None,
    }) else {
        panic!("missing named argument mismatch: {d:?}");
    };
    assert_eq!(error.0, "i64");
    assert_eq!(error.1, "1");
    assert_eq!(
        d.len(),
        1,
        "one invalid parameter must produce exactly one error: {d:?}"
    );
}

#[test]
fn strings_count_reports_exact_parameter_types_and_names() {
    let d = diagnostics_for(
        "use std::strings\n\
         fn main() {\n\
         let _ = strings::count(#[1, 2], \"a\")\n\
         let _ = strings::count('a', \"a\")\n\
         let _ = strings::count(\"a\", 1)\n\
         }\n",
    );
    let mismatches: Vec<_> = d
        .iter()
        .filter_map(|diag| match &diag.error {
            TypeError::ArgumentTypeMismatch {
                callee,
                parameter,
                expected,
                found,
                actual,
            } => Some((
                callee.as_str(),
                parameter.as_str(),
                expected.as_str(),
                found.as_str(),
                actual.as_str(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        mismatches,
        vec![
            ("strings::count", "text", "String", "Vec", "#[1, 2]"),
            ("strings::count", "text", "String", "char", "'a'"),
            ("strings::count", "needle", "String | char", "i64", "1"),
        ],
        "diagnostics must match the source-facing signature: {d:?}"
    );
}

#[test]
fn named_string_argument_mismatch_uses_a_user_facing_container_type() {
    let d = diagnostics_for(
        "use std::strings\nfn main() { let _ = strings::slice([1, 2, 3], 1, 2) }\n",
    );
    assert!(
        d.iter().any(|diag| matches!(
            &diag.error,
            TypeError::ArgumentTypeMismatch { parameter, found, actual, .. }
                if parameter == "text" && found == "array" && actual == "[1, 2, 3]"
        )),
        "array mismatch must not expose an inference variable: {d:?}"
    );
}

#[test]
fn string_bytes_method_is_typed_as_byte_vector() {
    let d =
        diagnostics_for("fn main() { let bytes: Vec<u8> = \"ab\".bytes()\n let _ = bytes[1] }\n");
    assert!(
        d.is_empty(),
        "String::bytes must typecheck as Vec<u8>: {d:?}"
    );
}

#[test]
fn stdlib_signature_catalog_rejects_non_string_arity_mismatch() {
    let d = diagnostics_for(
        "use std::math\n\
         fn main() { let _ = math::sqrt() }\n",
    );
    assert!(
        d.iter().any(|x| matches!(
            x.error,
            TypeError::CallArityMismatch { ref callee, expected: 1, found: 0 }
                if callee == "math::sqrt"
        )),
        "stdlib signature arity must reject missing math::sqrt arg: {d:?}"
    );
}

#[test]
fn stdlib_signature_catalog_shapes_non_string_argument_types() {
    let d = diagnostics_for(
        "use std::math\n\
         fn main() { let _ = math::pow(\"x\", 2.0) }\n",
    );
    assert!(
        d.iter().any(|x| matches!(
            &x.error,
            TypeError::TypeMismatch { expected, found }
                if expected == "f64" && found == "String"
        )),
        "stdlib signature argument expectations must reject math::pow(String, f64): {d:?}"
    );
}

#[test]
fn stdlib_signature_catalog_supplies_non_specialized_return_type() {
    let d = diagnostics_for(
        "use std::math\n\
         fn f() -> String { math::sqrt(4.0) }\n",
    );
    assert!(
        d.iter().any(|x| matches!(
            &x.error,
            TypeError::TypeMismatch { expected, found }
                if expected == "String" && found == "f64"
        )),
        "stdlib signature return type must reject returning math::sqrt as String: {d:?}"
    );
}

#[test]
fn json_value_variant_pattern_is_rejected() {
    // `json::Value` is an opaque dynamic-document handle with no matchable
    // discriminant; matching its variants silently falls through on the VM
    // and faults on the compiled tiers, so it is rejected at check.
    let d = diagnostics_for(
        "use std::encoding::json\n\
         fn f(v: json::Value) -> i64 {\n\
         match v {\n\
         json::Value::Int(n) => n,\n\
         json::Value::Object(pairs) => 1,\n\
         _ => 0,\n\
         }\n\
         }\n",
    );
    let hits = d
        .iter()
        .filter(|x| matches!(&x.error, TypeError::JsonValuePatternUnsupported { .. }))
        .count();
    assert_eq!(
        hits, 2,
        "both json variant patterns must be rejected, got {d:?}"
    );
}

#[test]
fn json_value_if_let_pattern_is_rejected() {
    let d = diagnostics_for(
        "use std::encoding::json\n\
         fn f(v: json::Value) {\n\
         if let json::Value::Object(pairs) = v { let _ = pairs }\n\
         }\n",
    );
    assert!(
        d.iter()
            .any(|x| matches!(&x.error, TypeError::JsonValuePatternUnsupported { .. })),
        "if-let json variant pattern must be rejected, got {d:?}"
    );
}

#[test]
fn json_value_constructor_expression_is_not_rejected() {
    // Constructing a `json::Value` (path/call position, not a pattern) is
    // the supported programmatic-build API and must stay clean.
    let d = diagnostics_for(
        "use std::encoding::json\n\
         fn f() -> json::Value { json::Value::Int(7) }\n",
    );
    assert!(
        !d.iter()
            .any(|x| matches!(&x.error, TypeError::JsonValuePatternUnsupported { .. })),
        "json::Value constructor must not be rejected, got {d:?}"
    );
}

#[test]
fn downgrade_on_scalar_is_rejected() {
    // `.downgrade()` needs a runtime RC pointer; a by-value scalar has no
    // header, so `Weak` of it faults on the compiled tiers. Rejected at check.
    let d = diagnostics_for("fn main() { let x: i64 = 5\nlet w = x.downgrade() }\n");
    assert!(
        d.iter()
            .any(|x| matches!(&x.error, TypeError::WeakDowngradeNonRc { .. })),
        "downgrade on a scalar must be rejected, got {d:?}"
    );
}

#[test]
fn downgrade_on_struct_is_accepted() {
    // A struct is a reference-counted aggregate with a real header, so
    // `.downgrade()` on it is valid and must type clean.
    let d = diagnostics_for(
        "struct Node { x: i64 }\n\
         fn main() { let n = Node { x: 1 }\nlet w = n.downgrade()\nlet _ = w }\n",
    );
    assert!(
        !d.iter()
            .any(|x| matches!(&x.error, TypeError::WeakDowngradeNonRc { .. })),
        "downgrade on a struct must be accepted, got {d:?}"
    );
}

#[test]
fn unary_neg_without_impl_is_rejected() {
    // `-a` on a struct with no `impl Neg` previously passed check and
    // faulted GX0002 at runtime; it must be a clean GT0003 error.
    let d = diagnostics_for(
        "struct P { x: i64 }\n\
         fn main() { let a = P { x: 1 }\nlet n = -a\nlet _ = n }\n",
    );
    assert!(has_code(&d, "GT0003"), "{d:?}");
}

#[test]
fn unary_neg_with_impl_is_accepted() {
    let d = diagnostics_for(
        "struct P { x: i64 }\n\
         impl Neg for P { fn neg(self) -> P { P { x: 0 - self.x } } }\n\
         fn main() { let a = P { x: 1 }\nlet n = -a\nprintln(\"{}\", n.x) }\n",
    );
    assert!(!has_code(&d, "GT0003"), "{d:?}");
}

#[test]
fn unary_neg_on_enum_with_impl_is_accepted() {
    let d = diagnostics_for(
        "enum Sign { Pos(i64), Neg(i64) }\n\
         impl Neg for Sign { fn neg(self) -> Sign {\n\
         match self { Sign::Pos(n) => Sign::Neg(n), Sign::Neg(n) => Sign::Pos(n) } } }\n\
         fn main() { let p = Sign::Pos(7)\nlet s = -p\nlet _ = s }\n",
    );
    assert!(!has_code(&d, "GT0003"), "{d:?}");
}

#[test]
fn compound_assign_without_impl_is_rejected() {
    // `a += b` desugars through `Add`; a struct place with no impl
    // previously passed check and faulted at runtime.
    let d = diagnostics_for(
        "struct P { x: i64 }\n\
         fn main() { let mut a = P { x: 1 }\na += P { x: 2 }\nlet _ = a }\n",
    );
    assert!(has_code(&d, "GT0003"), "{d:?}");
}

#[test]
fn compound_assign_with_impl_is_accepted() {
    // Includes the heterogeneous shape `v *= 2.0` (impl Mul takes `f64`).
    let d = diagnostics_for(
        "struct V { x: f64 }\n\
         impl Add for V { fn add(self, o: V) -> V { V { x: self.x + o.x } } }\n\
         impl Mul for V { fn mul(self, s: f64) -> V { V { x: self.x * s } } }\n\
         fn main() { let mut v = V { x: 1.0 }\nv += V { x: 2.0 }\nv *= 2.0\nprintln(\"{}\", v.x) }\n",
    );
    assert!(!has_code(&d, "GT0003"), "{d:?}");
}

#[test]
fn adt_on_rhs_of_scalar_operator_is_rejected() {
    // Operator dispatch is receiver-first: `2.0 * v` would call
    // `V::mul(2.0, v)` with a scalar `self`, so it is rejected rather
    // than miscompiled with swapped operands.
    let d = diagnostics_for(
        "struct V { x: f64 }\n\
         impl Mul for V { fn mul(self, s: f64) -> V { V { x: self.x * s } } }\n\
         fn main() { let v = V { x: 1.0 }\nlet w = 2.0 * v\nlet _ = w }\n",
    );
    assert!(has_code(&d, "GT0003"), "{d:?}");
}

#[test]
fn generic_impl_operator_is_accepted() {
    // `impl<T> Add for Wrap<T>` types `a + b` per instantiation via the
    // generic impl's declared return with substituted arguments.
    let d = diagnostics_for(
        "struct Wrap<T> { v: T }\n\
         impl<T> Add for Wrap<T> { fn add(self, o: Wrap<T>) -> Wrap<T> { Wrap { v: self.v + o.v } } }\n\
         fn main() { let a = Wrap { v: 3 }\nlet b = Wrap { v: 4 }\nprintln(\"{}\", (a + b).v) }\n",
    );
    assert!(!has_code(&d, "GT0003"), "{d:?}");
}

#[test]
fn index_on_struct_without_impl_is_rejected() {
    let d = diagnostics_for(
        "struct P { x: i64 }\n\
         fn main() { let a = P { x: 1 }\nprintln(\"{}\", a[0]) }\n",
    );
    assert!(has_code(&d, "GT0021"), "{d:?}");
}

#[test]
fn index_on_struct_with_impl_is_accepted() {
    let d = diagnostics_for(
        "struct P { x: i64 }\n\
         impl Index for P { fn index(self, i: i64) -> i64 { self.x + i } }\n\
         fn main() { let a = P { x: 1 }\nprintln(\"{}\", a[0]) }\n",
    );
    assert!(!has_code(&d, "GT0021"), "{d:?}");
}

#[test]
fn flag_set_parse_supports_question_mark() {
    let d = diagnostics_for(
        "use std::env\n\
         use std::errors\n\
         use std::flag\n\
         fn main() -> Result<(), errors::Error> {\n\
         let mut fs = flag::Set::new(\"demo\")\n\
         let rest = fs.parse(env::args())?\n\
         println(\"{}\", rest.len())\n\
         Ok(())\n\
         }\n",
    );
    assert!(!has_code(&d, "GT0045"), "{d:?}");
}

#[test]
fn validate_handles_keep_methods_across_function_return() {
    let d = diagnostics_for(
        "use std::validate\n\
         fn errors() -> validate::Errors {\n\
         let errs = validate::Errors::new()\n\
         errs.add(\"name\", validate::FieldError::new(\"name\", \"missing\", \"required\"))\n\
         errs\n\
         }\n\
         fn field_error() -> validate::FieldError {\n\
         validate::FieldError::new(\"email\", \"bad\", \"format\")\n\
         }\n\
         fn main() {\n\
         let errs = errors()\n\
         println(\"{} {}\", errs.len(), errs.collect())\n\
         let fe = field_error()\n\
         println(\"{} {} {}\", fe.path(), fe.message(), fe.code())\n\
         }\n",
    );
    assert!(!has_code(&d, "GT0002"), "{d:?}");
}

#[test]
fn map_binding_cannot_be_retyped_to_or_insert_result() {
    let diagnostics = diagnostics_for(
        "fn main() {\n\
         let mut h = Map::new()\n\
         h.insert(\"a\", 1)\n\
         h = h.or_insert(\"c\", 0)\n\
         }\n",
    );
    assert!(
        diagnostics.iter().any(|diagnostic| {
            matches!(
                &diagnostic.error,
                TypeError::TypeMismatch { expected, found }
                    if expected == "Map<String, i64>" && found == "i64"
            )
        }),
        "map assignment should preserve the established map type: {diagnostics:?}"
    );
}

#[test]
fn constant_repeat_literal_can_flow_into_vec_of_fixed_arrays() {
    let checked = run("fn main() {\n\
         let mut rows: Vec<[i64; 6]> = Vec::new()\n\
         let mut row = [0; 6]\n\
         row[3] = 9\n\
         rows.push(row)\n\
         let shaped: Vec<i64> = Vec::from([0; 4])\n\
         println(\"{} {}\", rows[0][3], shaped.len())\n\
         }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

#[test]
fn std_iter_skip_while_full_import_and_methods_typecheck() {
    let checked = run("use std::iter::skip_while\n\
         fn main() {\n\
         let xs = #[1, 2, 3, 1]\n\
         let a = skip_while(|x: i64| x < 3, xs)\n\
         let b = xs.iter().skip_while(|x: i64| x < 3)\n\
         let c = (1..5).skip_while(|x: i64| x < 3).collect()\n\
         println(\"{} {} {}\", a.count(), b.count(), c.len())\n\
         }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

#[test]
fn fast_string_and_path_std_apis_typecheck() {
    let diagnostics = diagnostics_for(
        "use std::strings::{byte_at, byte_len, substring}\n\
         use std::path::components\n\
         fn main() {\n\
         let text = \"a/b//c\"\n\
         let n: i64 = byte_len(text)\n\
         let slash: i64 = byte_at(text, 1)\n\
         let part: String = substring(text, 2, n)\n\
         let parts: Vec<String> = components(text)\n\
         println(\"{} {} {} {}\", n, slash, part, parts.len())\n\
         }\n",
    );
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn phase1_runtime_collection_shapes_accept_i64_paths() {
    let diagnostics = diagnostics_for(
        "use std::collections::{BTreeMap, MaxHeap, MinHeap, Deque}\n\
         fn main() {\n\
         let mut q: Deque<i64> = Deque::new()\n\
         q.push_back(1)\n\
         let front: Option<i64> = q.pop_front()\n\
         let mut max: MaxHeap<i64> = MaxHeap::from([1, 2])\n\
         max.push(3)\n\
         let top: Option<i64> = max.peek()\n\
         let mut min: MinHeap<i64> = MinHeap::from([3, 1])\n\
         min.push(0)\n\
         let low: Option<i64> = min.pop()\n\
         let mut sorted: BTreeMap<String, i64> = BTreeMap::new()\n\
         sorted.insert(\"a\", 1)\n\
         let mut int_sorted: BTreeMap<i64, i64> = BTreeMap::from([(2, 20), (1, 10)])\n\
         int_sorted.insert(3, 30)\n\
         let mut mixed_sorted: BTreeMap<i64, String> = BTreeMap::new()\n\
         mixed_sorted.insert(1, \"one\")\n\
         let mut string_sorted: BTreeMap<String, String> = BTreeMap::new()\n\
         string_sorted.insert(\"a\", \"b\")\n\
         println(\"{} {} {} {} {} {} {}\", front, top, low, sorted.len(), int_sorted.len(), mixed_sorted.len(), string_sorted.len())\n\
         }\n",
    );
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

/// A `Deque` / `Queue` / `Stack` stores and hands back, so it holds an element
/// of any type. A heap also orders its elements, so an element with no
/// ordering - a `Map`, a `Set` - is reported as such, as is a `u64`, whose
/// range runs past the signed comparison a heap slot orders by.
#[test]
fn slot_backed_collections_reject_elements_they_cannot_hold() {
    for (name, source, owner, found) in [
        (
            "max heap unsigned annotation",
            "use std::collections::MaxHeap\n\
             fn main() { let mut h: MaxHeap<u64> = MaxHeap::new()\n\
             println(\"{}\", h.len()) }\n",
            "MaxHeap",
            "u64",
        ),
        (
            "max heap map annotation",
            "use std::collections::{MaxHeap, Map}\n\
             fn main() { let mut h: MaxHeap<Map<String, i64>> = MaxHeap::new()\n\
             println(\"{}\", h.len()) }\n",
            "MaxHeap",
            "Map<String, i64>",
        ),
        (
            "min heap set annotation",
            "use std::collections::{MinHeap, Set}\n\
             fn main() { let mut h: MinHeap<Set<i64>> = MinHeap::new()\n\
             println(\"{}\", h.len()) }\n",
            "MinHeap",
            "Set<i64>",
        ),
    ] {
        let diagnostics = diagnostics_for(source);
        assert!(
            diagnostics.iter().any(|diagnostic| matches!(
                &diagnostic.error,
                TypeError::SlotCollectionElement { owner: got_owner, found: got_found }
                    if got_owner == owner && got_found == found
            )),
            "{name} should name the element the container cannot hold: {diagnostics:?}"
        );
    }

    // The sequence-shaped containers hold whatever a `Vec` holds.
    for source in [
        "use std::collections::Deque\n\
         fn main() { let mut q: Deque<String> = Deque::new()\n\
         q.push_back(\"a\") }\n",
        "use std::collections::Queue\n\
         fn main() { let mut q = Queue::from([\"a\"])\n\
         println(\"{}\", q.len()) }\n",
        "use std::collections::Stack\n\
         fn main() { let mut s: Stack<(i64, i64)> = Stack::new()\n\
         println(\"{}\", s.len()) }\n",
        "use std::collections::MaxHeap\n\
         fn main() { let mut h: MaxHeap<String> = MaxHeap::new()\n\
         println(\"{}\", h.len()) }\n",
    ] {
        let diagnostics = diagnostics_for(source);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }
}

/// Every scalar is one word, so a slot-backed container holds it as written
/// and answers `Option<T>` in that element type.
#[test]
fn slot_backed_collections_hold_any_scalar_element() {
    let diagnostics = diagnostics_for(
        "use std::collections::{Deque, MaxHeap, MinHeap, Queue, Stack}\n\
         fn main() {\n\
         let mut q: Queue<u32> = Queue::new()\n\
         q.push(7 as u32)\n\
         let head: Option<u32> = q.pop()\n\
         let mut s: Stack<char> = Stack::new()\n\
         s.push('a')\n\
         let top: Option<char> = s.pop()\n\
         let mut d: Deque<bool> = Deque::new()\n\
         d.push_back(true)\n\
         let flag: Option<bool> = d.pop_front()\n\
         let mut hi: MaxHeap<f64> = MaxHeap::new()\n\
         hi.push(1.5)\n\
         let most: Option<f64> = hi.pop()\n\
         let mut lo: MinHeap<i16> = MinHeap::new()\n\
         lo.push(3 as i16)\n\
         let least: Option<i16> = lo.pop()\n\
         println(\"{:?} {:?} {:?} {:?} {:?}\", head, top, flag, most, least)\n\
         }\n",
    );
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn array_literal_never_satisfies_a_vec_parameter() {
    // Rust's direction: a growable sequence coerces to a borrowed view, but a
    // fixed array is not a Vec. Every call shape has to agree, including a
    // method whose receiver came from a `-> Self` constructor - that path
    // silently accepted the array and the callee then mis-read it.
    let cases = [
        "fn take(v: Vec<i64>) -> i64 { v.len() }\n\
         fn main() { let _ = take([1, 2]) }\n",
        "struct M { xs: Vec<i64> }\n\
         impl M {\n\
             fn take(&mut self, v: Vec<i64>) -> i64 { self.xs.extend(v)\n\
                 self.xs.len() }\n\
         }\n\
         fn main() { let mut m = M { xs: #[] }\n\
             let _ = m.take([1, 2]) }\n",
        "struct M { xs: Vec<i64> }\n\
         impl M {\n\
             fn new() -> Self { M { xs: #[] } }\n\
             fn take(&mut self, v: Vec<i64>) -> i64 { self.xs.extend(v)\n\
                 self.xs.len() }\n\
         }\n\
         fn main() { let mut m = M::new()\n\
             let _ = m.take([1, 2]) }\n",
        "fn main() { let _v: Vec<i64> = [1, 2] }\n",
    ];
    for source in cases {
        let checked = run(source);
        assert!(
            checked
                .diagnostics
                .iter()
                .any(|d| matches!(d.error, TypeError::TypeMismatch { .. })),
            "array literal was accepted for a Vec slot:\n{source}"
        );
    }
}

#[test]
fn vec_literal_still_satisfies_an_array_parameter() {
    let checked = run("fn take(a: [i64; 2]) -> i64 { a.len() }\n\
         fn main() { let _ = take(#[1, 2]) }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

#[test]
fn self_returning_constructor_types_its_result() {
    let checked = run("struct M { x: i64 }\n\
         impl M {\n\
             fn new() -> Self { M { x: 1 } }\n\
         }\n\
         fn main() { let _bad: i64 = M::new() }\n");
    assert!(
        checked
            .diagnostics
            .iter()
            .any(|d| matches!(d.error, TypeError::TypeMismatch { .. })),
        "`-> Self` result went unchecked: {:?}",
        checked.diagnostics
    );
}

#[test]
fn where_clause_bound_resolves_a_trait_method_on_a_parameter() {
    let checked = run("trait Shape { fn area(&self) -> f64 }\n\
         struct Sq { s: f64 }\n\
         impl Shape for Sq { fn area(&self) -> f64 { self.s * self.s } }\n\
         fn total<T>(x: T) -> f64 where T: Shape { x.area() }\n\
         fn main() { let _ = total(Sq { s: 3.0 }) }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

#[test]
fn where_clause_carries_several_predicates_and_several_bounds() {
    let checked = run("trait A { fn a(&self) -> i64 }\n\
         trait B { fn b(&self) -> i64 }\n\
         struct P { v: i64 }\n\
         impl A for P { fn a(&self) -> i64 { self.v } }\n\
         impl B for P { fn b(&self) -> i64 { self.v } }\n\
         fn both<T, U>(x: T, y: U) -> i64 where T: A + B, U: A { x.a() + x.b() + y.a() }\n\
         fn main() { let _ = both(P { v: 1 }, P { v: 2 }) }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

#[test]
fn impl_level_and_method_level_bounds_index_their_own_parameters() {
    let checked = run("trait A { fn a(&self) -> i64 }\n\
         trait B { fn b(&self) -> i64 }\n\
         struct P { v: i64 }\n\
         impl A for P { fn a(&self) -> i64 { self.v } }\n\
         impl B for P { fn b(&self) -> i64 { self.v } }\n\
         struct W<T> { value: T }\n\
         impl<T: A> W<T> {\n\
             fn mixed<U: B>(&self, other: U) -> i64 { self.value.a() + other.b() }\n\
         }\n\
         fn main() { let w = W { value: P { v: 1 } }; let _ = w.mixed(P { v: 2 }) }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

#[test]
fn impl_level_bound_alone_still_resolves_its_method() {
    let checked = run("trait Shape { fn area(&self) -> f64 }\n\
         struct Sq { s: f64 }\n\
         impl Shape for Sq { fn area(&self) -> f64 { self.s } }\n\
         struct Wrapper<T> { value: T }\n\
         impl<T: Shape> Wrapper<T> { fn run_it(&self) -> f64 { self.value.area() } }\n\
         fn main() { let w = Wrapper { value: Sq { s: 1.0 } }; let _ = w.run_it() }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

#[test]
fn method_off_every_bound_is_still_reported_under_a_where_clause() {
    let checked = run("trait A { fn a(&self) -> i64 }\n\
         fn f<T>(x: T) -> i64 where T: A { x.zzz() }\n");
    assert!(
        checked
            .diagnostics
            .iter()
            .any(|d| matches!(d.error, TypeError::MethodNotOnBound { .. })),
        "{:?}",
        checked.diagnostics
    );
}

#[test]
fn multi_bound_parameter_resolves_methods_from_both_traits() {
    let checked = run("trait Named { fn name(&self) -> String }\n\
         trait Sized2 { fn size(&self) -> i64 }\n\
         struct Item { n: String, s: i64 }\n\
         impl Named for Item { fn name(&self) -> String { self.n } }\n\
         impl Sized2 for Item { fn size(&self) -> i64 { self.s } }\n\
         fn describe<T: Named + Sized2>(x: T) -> String { format(\"{} {}\", x.name(), x.size()) }\n\
         fn main() { let _ = describe(Item { n: \"a\", s: 1 }) }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

#[test]
fn multi_bound_reports_the_violated_bound_at_the_call_site() {
    let checked = run("trait Named { fn name(&self) -> String }\n\
         trait Weighed { fn size(&self) -> i64 }\n\
         struct Item { n: String }\n\
         impl Named for Item { fn name(&self) -> String { self.n } }\n\
         fn describe<T: Named + Weighed>(x: T) -> String { x.name() }\n\
         fn main() { let _ = describe(Item { n: \"a\" }) }\n");
    assert!(
        checked.diagnostics.iter().any(|d| matches!(
            &d.error,
            TypeError::TraitBoundNotSatisfied { ty, bound } if ty == "Item" && bound == "Weighed"
        )),
        "{:?}",
        checked.diagnostics
    );
}

#[test]
fn unknown_trait_in_a_struct_bound_is_reported() {
    let checked = run("struct S<T: Hashabel> { v: T }\n\
         fn main() { let _ = S { v: 1 } }\n");
    assert!(
        checked.diagnostics.iter().any(|d| matches!(
            &d.error,
            TypeError::UnknownTraitBound { name, .. } if name == "Hashabel"
        )),
        "{:?}",
        checked.diagnostics
    );
}

#[test]
fn unknown_trait_in_a_where_clause_is_reported() {
    let checked = run("fn f<T>(x: T) -> i64 where T: Hashabel { 0 }\n");
    assert!(
        checked.diagnostics.iter().any(|d| matches!(
            &d.error,
            TypeError::UnknownTraitBound { name, .. } if name == "Hashabel"
        )),
        "{:?}",
        checked.diagnostics
    );
}

#[test]
fn struct_generic_bound_is_enforced_at_construction() {
    let checked = run("trait Shape { fn area(&self) -> f64 }\n\
         struct Sq { s: f64 }\n\
         struct Holder<T: Shape> { v: T }\n\
         fn main() { let _ = Holder { v: Sq { s: 1.0 } } }\n");
    assert!(
        checked.diagnostics.iter().any(|d| matches!(
            &d.error,
            TypeError::TraitBoundNotSatisfied { ty, bound } if ty == "Sq" && bound == "Shape"
        )),
        "{:?}",
        checked.diagnostics
    );
}

#[test]
fn struct_generic_bound_is_satisfied_by_an_impl() {
    let checked = run("trait Shape { fn area(&self) -> f64 }\n\
         struct Sq { s: f64 }\n\
         impl Shape for Sq { fn area(&self) -> f64 { self.s } }\n\
         struct Holder<T: Shape> { v: T }\n\
         fn main() { let _ = Holder { v: Sq { s: 1.0 } } }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

#[test]
fn operator_on_an_unbounded_parameter_is_rejected() {
    let checked = run("struct Wrap<T> { v: T }\n\
         impl<T> Add for Wrap<T> {\n\
             fn add(self, o: Wrap<T>) -> Wrap<T> { Wrap { v: self.v + o.v } }\n\
         }\n");
    assert!(
        checked.diagnostics.iter().any(|d| matches!(
            &d.error,
            TypeError::OperatorNotOnBound { op, trait_name, .. } if op == "+" && trait_name == "Add"
        )),
        "{:?}",
        checked.diagnostics
    );
}

#[test]
fn operator_on_a_bounded_parameter_is_accepted() {
    let checked = run("struct Wrap<T> { v: T }\n\
         impl<T: Add> Add for Wrap<T> {\n\
             fn add(self, o: Wrap<T>) -> Wrap<T> { Wrap { v: self.v + o.v } }\n\
         }\n\
         fn main() { let a = Wrap { v: 1 }; let b = Wrap { v: 2 }; let _ = (a + b).v }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

#[test]
fn builtin_operator_bound_is_enforced_against_the_impl_table() {
    let checked = run("struct Point { x: i64 }\n\
         fn twice<T: Add>(a: T, b: T) -> T { a + b }\n\
         fn main() { let _ = twice(Point { x: 1 }, Point { x: 2 }) }\n");
    assert!(
        checked.diagnostics.iter().any(|d| matches!(
            &d.error,
            TypeError::TraitBoundNotSatisfied { ty, bound } if ty == "Point" && bound == "Add"
        )),
        "{:?}",
        checked.diagnostics
    );
}

#[test]
fn automatic_builtin_bound_stays_satisfied_without_an_impl_block() {
    let checked = run("struct Point { x: i64 }\n\
         fn show<T: Debug>(a: T) -> T { a }\n\
         fn main() { let _ = show(Point { x: 1 }) }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

#[test]
fn trait_impl_missing_a_required_method_is_reported() {
    let checked = run("trait Shape { fn area(&self) -> f64 }\n\
         struct Sq { s: f64 }\n\
         impl Shape for Sq {}\n");
    assert!(
        checked.diagnostics.iter().any(|d| matches!(
            &d.error,
            TypeError::MissingTraitImplMethods { trait_name, ty, missing }
                if trait_name == "Shape" && ty == "Sq" && missing == &vec!["area".to_string()]
        )),
        "{:?}",
        checked.diagnostics
    );
}

#[test]
fn trait_impl_may_omit_a_method_that_has_a_default_body() {
    let checked = run("trait Shape { fn area(&self) -> f64 { 0.0 } }\n\
         struct Sq { s: f64 }\n\
         impl Shape for Sq {}\n");
    assert!(
        !checked
            .diagnostics
            .iter()
            .any(|d| matches!(d.error, TypeError::MissingTraitImplMethods { .. })),
        "{:?}",
        checked.diagnostics
    );
}

#[test]
fn explicit_const_generic_argument_overrides_the_inferred_length() {
    let checked = run(
        "fn sum_arr<const N: usize>(xs: [i64; N]) -> i64 { xs.len() }\n\
         fn main() { let _ = sum_arr::<3,>([1, 2, 3, 4]) }\n",
    );
    assert!(
        checked.diagnostics.iter().any(|d| matches!(
            &d.error,
            TypeError::TypeMismatch { expected, found }
                if expected == "[i64; 3]" && found == "[i64; 4]"
        )),
        "{:?}",
        checked.diagnostics
    );
}

#[test]
fn explicit_const_generic_argument_matching_the_argument_checks_clean() {
    let checked = run(
        "fn sum_arr<const N: usize>(xs: [i64; N]) -> i64 { xs.len() }\n\
         fn main() { let _ = sum_arr::<3,>([1, 2, 3]) }\n",
    );
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

#[test]
fn struct_variant_fields_bind_by_value_through_a_reference_scrutinee() {
    let checked = run("enum Shape { Circle(f64), Rect { w: f64, h: f64 } }\n\
         fn area(s: Shape) -> f64 {\n\
             match s {\n\
                 Shape::Circle(r) => 3.14 * r * r,\n\
                 Shape::Rect { w, h } => w * h,\n\
             }\n\
         }\n\
         fn main() { let _ = area(Shape::Circle(1.0)) }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

#[test]
fn bound_method_in_a_generic_impl_records_its_string_return() {
    let checked = run("trait Shape { fn name(&self) -> String }\n\
         struct Sq { s: f64 }\n\
         impl Shape for Sq { fn name(&self) -> String { \"sq\" } }\n\
         struct Wrapper<T> { value: T }\n\
         impl<T: Shape> Wrapper<T> { fn label(&self) -> i64 { self.value.name() } }\n");
    assert!(
        checked.diagnostics.iter().any(|d| matches!(
            &d.error,
            TypeError::TypeMismatch { expected, found } if expected == "i64" && found == "String"
        )),
        "bound method return went unrecorded: {:?}",
        checked.diagnostics
    );
}

#[test]
fn bound_method_in_a_generic_impl_records_its_float_return() {
    let checked = run("trait Shape { fn area(&self) -> f64 }\n\
         struct Sq { s: f64 }\n\
         impl Shape for Sq { fn area(&self) -> f64 { self.s } }\n\
         struct Wrapper<T> { value: T }\n\
         impl<T: Shape> Wrapper<T> { fn size(&self) -> String { self.value.area() } }\n");
    assert!(
        checked.diagnostics.iter().any(|d| matches!(
            &d.error,
            TypeError::TypeMismatch { expected, found } if expected == "String" && found == "f64"
        )),
        "bound method return went unrecorded: {:?}",
        checked.diagnostics
    );
}

#[test]
fn bound_method_in_a_generic_impl_records_a_struct_return() {
    let checked = run("struct Point { x: i64 }\n\
         trait Located { fn at(&self) -> Point }\n\
         struct Sq { s: i64 }\n\
         impl Located for Sq { fn at(&self) -> Point { Point { x: self.s } } }\n\
         struct Wrapper<T> { value: T }\n\
         impl<T: Located> Wrapper<T> { fn spot(&self) -> i64 { self.value.at() } }\n");
    assert!(
        checked.diagnostics.iter().any(|d| matches!(
            &d.error,
            TypeError::TypeMismatch { expected, found } if expected == "i64" && found == "Point"
        )),
        "bound method return went unrecorded: {:?}",
        checked.diagnostics
    );
}

#[test]
fn bound_method_in_a_generic_impl_checks_clean_at_its_declared_return() {
    let checked = run(
        "trait Shape { fn area(&self) -> f64\n fn name(&self) -> String }\n\
         struct Sq { s: f64 }\n\
         impl Shape for Sq { fn area(&self) -> f64 { self.s }\n fn name(&self) -> String { \"sq\" } }\n\
         struct Wrapper<T> { value: T }\n\
         impl<T: Shape> Wrapper<T> {\n\
             fn report(&self) -> String { format(\"{}={}\", self.value.name(), self.value.area()) }\n\
             fn doubled(&self) -> f64 { self.value.area() * 2.0 }\n\
         }\n\
         fn main() { let w = Wrapper { value: Sq { s: 3.0 } }; println(\"{} {}\", w.report(), w.doubled()) }\n",
    );
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

#[test]
fn tuple_rest_pattern_binds_its_suffix_from_the_end() {
    let checked = run("fn rest_pat(t: (i64, String, bool, i64)) -> i64 {\n\
             match t { (a, .., d) => a + d }\n\
         }\n\
         fn main() { let _ = rest_pat((1, \"x\", true, 2)) }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

#[test]
fn struct_pattern_scalar_fields_bind_by_value_through_a_reference() {
    let checked = run("struct P { x: i64, y: i64 }\n\
         fn f(p: P) -> i64 { match p { P { x, y } => x + y } }\n\
         fn main() { let _ = f(P { x: 1, y: 2 }) }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

#[test]
fn unary_not_on_an_unbounded_parameter_is_rejected() {
    let checked = run("fn flip<T>(x: T) -> T { !x }\n");
    assert!(
        checked.diagnostics.iter().any(|d| matches!(
            &d.error,
            TypeError::OperatorNotOnBound { op, trait_name, .. } if op == "!" && trait_name == "Not"
        )),
        "{:?}",
        checked.diagnostics
    );
}

#[test]
fn unary_neg_on_an_unbounded_parameter_is_rejected() {
    let checked = run("fn flip<T>(x: T) -> T { -x }\n");
    assert!(
        checked.diagnostics.iter().any(|d| matches!(
            &d.error,
            TypeError::OperatorNotOnBound { op, trait_name, .. } if op == "-" && trait_name == "Neg"
        )),
        "{:?}",
        checked.diagnostics
    );
}

#[test]
fn unary_neg_on_a_bounded_parameter_is_accepted() {
    let checked = run("trait Flip { fn neg(self) -> Self }\n\
         fn flip<T: Flip>(x: T) -> T { -x }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

#[test]
fn assoc_type_projects_through_a_bound() {
    let checked = run(
        "trait Holder { type Item\n    fn get(&self) -> Self::Item }\n\
         struct Label { text: String }\n\
         impl Holder for Label { type Item = String\n\
             fn get(&self) -> Self::Item { self.text } }\n\
         fn shout<T: Holder>(h: T) -> T::Item { h.get() }\n\
         fn main() { println(\"{}\", shout(Label { text: \"x\" }).to_uppercase()) }\n",
    );
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

#[test]
fn assoc_type_default_applies_when_the_impl_omits_it() {
    let checked = run(
        "trait Counted { type Count = i64\n    fn amount(&self) -> Self::Count }\n\
         struct Tally { hits: i64 }\n\
         impl Counted for Tally { fn amount(&self) -> Self::Count { self.hits } }\n\
         fn total<T: Counted>(c: T) -> T::Count { c.amount() }\n\
         fn main() { let n: i64 = total(Tally { hits: 1 })\n    println(\"{}\", n) }\n",
    );
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

#[test]
fn assoc_type_equality_constraint_pins_an_ambiguous_projection() {
    let source = "trait Source { type Item\n    fn take(&self) -> Self::Item }\n\
         struct A { v: i64 }\n\
         struct B { s: String }\n\
         impl Source for A { type Item = i64\n    fn take(&self) -> Self::Item { self.v } }\n\
         impl Source for B { type Item = String\n    fn take(&self) -> Self::Item { self.s } }\n";
    let ambiguous = run(&format!(
        "{source}fn pick<T: Source>(x: T) -> T::Item {{ x.take() }}\n"
    ));
    assert!(
        ambiguous.diagnostics.iter().any(
            |d| matches!(&d.error, TypeError::AmbiguousAssocItem { name, .. } if name == "Item")
        ),
        "{:?}",
        ambiguous.diagnostics
    );
    let pinned = run(&format!(
        "{source}fn pick<T: Source<Item = i64>>(x: T) -> T::Item {{ x.take() + 1 }}\n"
    ));
    assert!(pinned.diagnostics.is_empty(), "{:?}", pinned.diagnostics);
}

#[test]
fn impl_omitting_a_required_assoc_item_is_reported() {
    let checked = run(
        "trait Holder { type Item\n    const MAX: i64\n    fn get(&self) -> Self::Item }\n\
         struct Label { text: String }\n\
         impl Holder for Label { fn get(&self) -> Self::Item { self.text } }\n",
    );
    assert!(
        checked.diagnostics.iter().any(|d| matches!(
            &d.error,
            TypeError::MissingTraitImplAssocItems { missing, .. }
                if missing == &["type Item".to_string(), "const MAX".to_string()]
        )),
        "{:?}",
        checked.diagnostics
    );
}

#[test]
fn impl_defining_an_item_outside_the_trait_is_reported() {
    let checked = run("struct Point { id: i64 }\n\
         impl Display for Point { fn fmt(&self) -> String { \"p\" }\n\
             fn show(&self) -> String { \"s\" } }\n");
    assert!(
        checked.diagnostics.iter().any(|d| matches!(
            &d.error,
            TypeError::ImplItemNotInTrait { trait_name, item, .. }
                if trait_name == "Display" && item == "show"
        )),
        "{:?}",
        checked.diagnostics
    );
}

#[test]
fn impl_defining_only_the_traits_own_items_is_accepted() {
    let checked = run(
        "trait Area { fn area(&self) -> i64\n    fn name(&self) -> String }\n\
         struct Square { side: i64 }\n\
         impl Area for Square { fn area(&self) -> i64 { self.side * self.side }\n\
             fn name(&self) -> String { \"square\" } }\n",
    );
    assert!(
        !checked
            .diagnostics
            .iter()
            .any(|d| matches!(&d.error, TypeError::ImplItemNotInTrait { .. })),
        "{:?}",
        checked.diagnostics
    );
}

#[test]
fn a_trait_implemented_twice_for_one_type_is_reported() {
    let checked = run("struct Point { id: i64 }\n\
         impl Display for Point { fn fmt(&self) -> String { \"a\" } }\n\
         impl Display for Point { fn fmt(&self) -> String { \"b\" } }\n");
    assert!(
        checked.diagnostics.iter().any(|d| matches!(
            &d.error,
            TypeError::ConflictingTraitImpl { trait_name, derived, .. }
                if trait_name == "Display" && !*derived
        )),
        "{:?}",
        checked.diagnostics
    );
}

#[test]
fn an_impl_competing_with_a_derive_is_reported() {
    let checked = run("#[derive(Debug)]\n\
         struct Point { id: i64 }\n\
         impl Debug for Point { fn fmt(&self) -> String { \"a\" } }\n");
    assert!(
        checked.diagnostics.iter().any(|d| matches!(
            &d.error,
            TypeError::ConflictingTraitImpl { trait_name, derived, .. }
                if trait_name == "Debug" && *derived
        )),
        "{:?}",
        checked.diagnostics
    );
}

/// A `(trait, type)` pair is the type's own identity, so two modules each
/// declaring a `Point` implement two distinct pairs.
#[test]
fn two_modules_declaring_one_name_implement_distinct_pairs() {
    let checked = run("mod a {\n\
             pub struct Point { pub x: i64 }\n\
             impl Display for Point { pub fn fmt(&self) -> String { \"a\" } }\n\
         }\n\
         mod b {\n\
             #[derive(Debug)]\n\
             pub struct Point { pub x: i64 }\n\
         }\n\
         mod c {\n\
             pub struct Point { pub x: i64 }\n\
             impl Debug for Point { pub fn fmt(&self) -> String { \"c\" } }\n\
         }\n");
    assert!(
        !checked
            .diagnostics
            .iter()
            .any(|d| matches!(&d.error, TypeError::ConflictingTraitImpl { .. })),
        "{:?}",
        checked.diagnostics
    );
}

#[test]
fn a_body_answering_a_value_without_a_declared_return_is_reported() {
    let checked = run("fn add(a: i64, b: i64) { a + b }\n");
    assert!(
        checked.diagnostics.iter().any(|d| matches!(
            &d.error,
            TypeError::UndeclaredReturnValue { name, found } if name == "add" && found == "i64"
        )),
        "{:?}",
        checked.diagnostics
    );
}

#[test]
fn a_body_answering_a_unit_needs_no_declared_return() {
    let checked = run("fn shout(a: i64) { println(\"{}\", a) }\n");
    assert!(
        !checked
            .diagnostics
            .iter()
            .any(|d| matches!(&d.error, TypeError::UndeclaredReturnValue { .. })),
        "{:?}",
        checked.diagnostics
    );
}

#[test]
fn unknown_assoc_item_names_what_the_bound_declares() {
    let checked = run("trait Holder { type Item }\n\
         struct A {}\n\
         impl Holder for A { type Item = i64 }\n\
         fn pick<T: Holder>(x: T) -> T::Nope { 1 }\n");
    assert!(
        checked.diagnostics.iter().any(|d| matches!(
            &d.error,
            TypeError::UnknownAssocItem { name, declared, .. }
                if name == "Nope" && declared == &["Item".to_string()]
        )),
        "{:?}",
        checked.diagnostics
    );
}

#[test]
fn assoc_item_is_reachable_through_a_supertrait() {
    let checked = run("trait Base { type Item\n    const MAX: i64 }\n\
         trait Ext: Base { fn get(&self) -> Self::Item }\n\
         struct S { v: i64 }\n\
         impl Base for S { type Item = i64\n    const MAX: i64 = 6 }\n\
         impl Ext for S { fn get(&self) -> Self::Item { self.v } }\n\
         fn top<T: Ext>(x: T) -> i64 { T::MAX }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

/// A type declared inside a module registers under its module-qualified
/// identity, so an `impl` block on it has to record its methods under
/// that same identity - otherwise every call on a receiver of that type
/// reports the method as missing.
#[test]
fn a_module_local_types_methods_resolve_on_its_own_receivers() {
    let checked = run("mod lib {\n\
             pub struct Point { pub x: i64, pub y: i64 }\n\
             impl Point {\n\
                 pub fn new(x: i64, y: i64) -> Self { Point { x: x, y: y } }\n\
                 pub fn public_dist(self) -> i64 { self.internal_dist() }\n\
                 fn internal_dist(self) -> i64 { self.x + self.y }\n\
             }\n\
         }\n\
         fn main() { let p = lib::Point::new(1, 2)\n    let _ = p.public_dist() }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

/// A trait implemented for a module-local type satisfies a `T: Trait`
/// bound, which requires the impl to be recorded against the receiver's
/// module-qualified identity.
#[test]
fn a_trait_impl_on_a_module_local_type_satisfies_a_bound() {
    let checked = run("trait Speak { fn speak(&self) -> i64 }\n\
         mod animals {\n\
             pub struct Dog { pub n: i64 }\n\
             impl super::Speak for Dog { fn speak(&self) -> i64 { self.n } }\n\
         }\n\
         fn announce<T: Speak>(x: T) -> i64 { x.speak() }\n\
         fn main() { let d = animals::Dog { n: 1 }\n    let _ = announce(d) }\n");
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
}

/// A method a module-local type genuinely does not have is still
/// rejected - qualifying the owner keys must not blanket-accept.
#[test]
fn an_unknown_method_on_a_module_local_type_is_still_rejected() {
    let checked = run("mod lib {\n\
             pub struct Point { pub x: i64 }\n\
             impl Point { pub fn get(self) -> i64 { self.x } }\n\
         }\n\
         fn main() { let p = lib::Point { x: 1 }\n    let _ = p.nope() }\n");
    assert!(
        checked.diagnostics.iter().any(|d| matches!(
            &d.error,
            TypeError::UnresolvedMethod { name, .. } if name == "nope"
        )),
        "{:?}",
        checked.diagnostics
    );
}

/// A combinator on a built-in iterator receiver takes exactly the
/// arguments its surface declares. Without the count the call reaches
/// the runtime with an unconstrained result and reads as a no-op.
#[test]
fn iterator_combinator_rejects_wrong_argument_count() {
    for (src, callee, expected, found) in [
        ("let _ = (0..9).map(|n| n * 2).collect(1)", "collect", 0, 1),
        ("let _ = (0..9).map()", "map", 1, 0),
        ("let _ = (0..9).sum(1)", "sum", 0, 1),
        ("let _ = (0..9).take()", "take", 1, 0),
        ("let _ = (0..9).filter(|n| n > 1, 2)", "filter", 1, 2),
        ("let _ = (0..9).next(1)", "next", 0, 1),
        ("let _ = (0..9).fold(0, |a, b| a + b, 3)", "fold", 2, 3),
    ] {
        let d = diagnostics_for(&format!("fn main() {{\n{src}\n}}\n"));
        assert!(
            d.iter().any(|x| matches!(
                &x.error,
                TypeError::CallArityMismatch { callee: got, expected: e, found: f }
                    if got == callee && *e == expected && *f == found
            )),
            "`{src}` must be rejected: {d:?}"
        );
    }
}

/// The argument counts the iterator surface does declare stay accepted,
/// including `count`'s predicate form.
#[test]
fn iterator_combinator_accepts_declared_argument_counts() {
    for src in [
        "let _ = (0..9).map(|n| n * 2).collect()",
        "let _ = (0..9).filter(|n| n > 1).count()",
        "let _ = (0..9).count(|n| n > 1)",
        "let _ = (0..9).fold(0, |a, b| a + b)",
        "let _ = (0..9).take(3).sum()",
        "let _ = (0..9).next()",
    ] {
        let d = diagnostics_for(&format!("fn main() {{\n{src}\n}}\n"));
        assert!(d.is_empty(), "`{src}` must type clean: {d:?}");
    }
}

/// `x |> recv.m(a)` places the piped value in the method's last
/// argument slot, so a built-in receiver counts it toward the arity and
/// checks its type against that slot.
#[test]
fn piped_builtin_method_argument_is_counted_and_typed() {
    let clean = [
        "let mut xs = #[1, 2, 3]\nlet _ = 9 |> xs.push()",
        "fn dbl(n: i64) -> i64 { n * 2 }\nfn main() { let xs = #[1, 2, 3]\n    let _ = dbl |> xs.map() }",
        "let s = \"a-b\"\nlet _ = \"-\" |> s.split()",
    ];
    for src in clean {
        let body = if src.contains("fn main") {
            src.to_string()
        } else {
            format!("fn main() {{\n{src}\n}}\n")
        };
        let d = diagnostics_for(&body);
        assert!(d.is_empty(), "`{src}` must type clean: {d:?}");
    }
    let d =
        diagnostics_for("fn main() {\nlet mut xs = #[1, 2, 3]\nlet _ = \"x\" |> xs.push()\n}\n");
    assert!(
        d.iter()
            .any(|x| matches!(x.error, TypeError::TypeMismatch { .. })),
        "a piped value must match the slot it lands in: {d:?}"
    );
}

/// The built-in String and tuple surfaces declare one argument count
/// each; a call that supplies another reaches a shim that would ignore
/// the extra value, so it is rejected at check.
#[test]
fn builtin_receiver_methods_reject_wrong_argument_count() {
    for (recv, src) in [
        ("let s = \"a\"", "s.len(1)"),
        ("let s = \"a\"", "s.is_empty(1)"),
        ("let s = \"a\"", "s.as_bytes(1)"),
        ("let s = \"a\"", "s.index_rune()"),
        ("let s = \"a\"", "s.contains_rune('a', 2)"),
        ("let t = (1, 2)", "t.get()"),
        ("let t = (1, 2)", "t.len(1)"),
        ("let t = (1, 2)", "t.clone(1)"),
        ("let v = #[1, 2]", "v.len(1)"),
        ("let v = #[1, 2]", "v.first(1)"),
    ] {
        let d = diagnostics_for(&format!("fn main() {{\n{recv}\nlet _ = {src}\n}}\n"));
        assert!(
            d.iter()
                .any(|x| matches!(x.error, TypeError::CallArityMismatch { .. })),
            "`{src}` must be rejected as an arity mismatch: {d:?}"
        );
    }
    for (recv, src) in [
        ("let s = \"a\"", "s.len()"),
        ("let s = \"a\"", "s.as_bytes()"),
        ("let s = \"a\"", "s.index_rune('a')"),
        ("let t = (1, 2)", "t.get(0)"),
        ("let t = (1, 2)", "t.len()"),
        ("let v = #[1, 2]", "v.first()"),
    ] {
        let d = diagnostics_for(&format!("fn main() {{\n{recv}\nlet _ = {src}\n}}\n"));
        assert!(d.is_empty(), "`{src}` must type clean: {d:?}");
    }
}

#[test]
fn vec_eager_combinator_methods_accept_documented_arguments() {
    let source = "fn main() {\n\
        let xs: Vec<i64> = #[1, 2, 3, 4]\n\
        let evens, odds = xs.partition(|value: i64| value % 2 == 0)\n\
        let _ = evens.len() + odds.len()\n\
        let _ = xs.filter_map(|value: i64| if value > 1 { Some(value) } else { None })\n\
        let _ = xs.find_map(|value: i64| if value > 2 { Some(value) } else { None })\n\
        let _ = xs.flat_map(|value: i64| #[value, value])\n\
        let _ = xs.chunk_by(|value: i64| value)\n\
        let _ = xs.count_by(|value: i64| value)\n\
        let _ = xs.min_by(|left: i64, right: i64| left - right)\n\
        let _ = xs.max_by(|left: i64, right: i64| left - right)\n\
        let _ = xs.product_by(|value: i64| value)\n\
        let _ = xs.reduce(|left: i64, right: i64| left + right)\n\
        let _ = xs.sum_by(|value: i64| value)\n\
        let _ = xs.scan(0, |acc: i64, value: i64| acc + value)\n\
        let _ = xs.count() + xs.count(|value: i64| value > 1)\n\
        let pairs: Vec<(i64, i64)> = #[(1, 2), (3, 4)]\n\
        let _ = pairs.unzip()\n\
    }\n";
    let d = diagnostics_for(source);
    assert!(
        d.is_empty(),
        "Vec combinator method calls must type clean: {d:?}"
    );
}

/// A built-in handle receiver dispatches by name to a runtime shim that
/// reads a fixed number of slots, so a call supplying another count is
/// rejected rather than silently dropping or zero-filling a slot.
#[test]
fn handle_receiver_methods_reject_wrong_argument_count() {
    for (setup, src) in [
        ("let tx, rx = channel()", "tx.send()"),
        ("let tx, rx = channel()", "tx.close(1)"),
        ("let tx, rx = channel()", "rx.recv(1)"),
        ("let e = errors::new(\"x\")", "e.message(1)"),
        ("let e = errors::new(\"x\")", "e.is()"),
    ] {
        let d = diagnostics_for(&format!(
            "use std::errors\nuse std::sync::channel\nfn main() {{\n{setup}\nlet _ = {src}\n}}\n"
        ));
        assert!(
            d.iter()
                .any(|x| matches!(x.error, TypeError::CallArityMismatch { .. })),
            "`{src}` must be rejected as an arity mismatch: {d:?}"
        );
    }
    let clean = "use std::errors\nuse std::sync::channel\n\
         fn main() {\n\
         let tx, rx = channel()\n\
         tx.send(1)\n\
         tx.close()\n\
         let _ = rx.recv()\n\
         let e = errors::new(\"x\")\n\
         let _ = e.message()\n\
         let _ = e.is(\"x\")\n\
         }\n";
    let d = diagnostics_for(clean);
    assert!(d.is_empty(), "the declared counts must type clean: {d:?}");
}

/// `collect` ends an iterator chain; a collection that already holds its
/// values has no use for it, and neither does a `Vec` for `to_vec` nor a
/// `String` for `to_string` - each would convert a type into itself.
#[test]
fn the_redundant_self_conversions_are_not_on_the_surface() {
    for (source, ty, method) in [
        (
            "fn main() { let xs = #[1, 2, 3]\n let _ = xs.to_vec() }\n",
            "Vec<i64>",
            "to_vec",
        ),
        (
            "fn main() { let s = \"a\"\n let _ = s.to_string() }\n",
            "String",
            "to_string",
        ),
        (
            "use std::collections::Set\nfn main() { let s = #{1, 2}\n let _ = s.collect() }\n",
            "Set<i64>",
            "collect",
        ),
        (
            "fn main() { let m = {\"a\": 1}\n let _ = m.collect() }\n",
            "Map<String, i64>",
            "collect",
        ),
    ] {
        let d = diagnostics_for(source);
        assert!(
            d.iter().any(|diag| matches!(
                &diag.error,
                TypeError::UnresolvedMethod { ty: t, name, .. } if t == ty && name == method
            )),
            "{method} on {ty} should not resolve: {d:?}"
        );
    }
}

/// The conversions that do change a type stay: a borrowed or fixed-length
/// sequence into an owned one, and an iterator chain into a Vec.
#[test]
fn the_real_conversions_still_resolve() {
    for source in [
        "fn main() { let a = [1, 2, 3]\n let _ = a.to_vec() }\n",
        "use std::collections::Set\nfn main() { let s = #{1, 2}\n let _ = s.to_vec() }\n",
        "fn main() { let _ = (0..3).collect() }\n",
        "fn main() { let _ = #[1, 2].iter().collect() }\n",
        "fn main() { let s = \"a\"\n let _ = s.clone() }\n",
        "fn main() { let _ = 5.to_string() }\n",
    ] {
        let d = diagnostics_for(source);
        assert!(d.is_empty(), "{source}: {d:?}");
    }
}

/// `From<[T; N]> for Vec<T>` is built in, so `.into()` on the array itself
/// carries the conversion. A reference to the array is not the array, and
/// the audit says so rather than letting the call reach a run-time
/// unbound `into`.
#[test]
fn a_fixed_array_converts_into_a_vec_but_a_reference_to_one_does_not() {
    let d = diagnostics_for("fn main() { let a = [1, 2, 3]\n let _v: Vec<i64> = a.into() }\n");
    assert!(d.is_empty(), "array into Vec must type clean: {d:?}");

    let d = diagnostics_for(
        "fn main() { let a = [1, 2, 3]\n let r = &a\n let _v: Vec<i64> = r.into() }\n",
    );
    assert!(
        d.iter().any(|diag| matches!(
            &diag.error,
            TypeError::NoConversion {
                from,
                to,
                borrowed_sequence: true,
            } if from == "&[i64; 3]" && to == "Vec<i64>"
        )),
        "a reference to the array should be pointed at `to_vec()`: {d:?}"
    );
}

// ---------------------------------------------------------------
// Numeric receivers: the `math` surface reached in method position,
// and the field reads a scalar cannot answer.
// ---------------------------------------------------------------

/// The target of a conversion comes from the use site, never from the
/// receiver, so a call nothing constrains has nothing to convert to. Such
/// a call reached an unbound `into` at run time before it was named here.
#[test]
fn a_conversion_with_no_target_is_rejected() {
    for (source, method) in [
        ("fn main() { let _ = (1, 2).into() }\n", "into"),
        ("fn main() { let _ = (1, 2).try_into() }\n", "try_into"),
        ("fn main() { let _ = \"hi\".into() }\n", "into"),
        ("fn main() { println((1, 2).into()) }\n", "into"),
    ] {
        let d = diagnostics_for(source);
        assert!(
            d.iter().any(|diag| matches!(
                &diag.error,
                TypeError::ConversionTargetUnknown { method: m } if m == method
            )),
            "{source} should report an unknown conversion target: {d:?}"
        );
    }
}

/// A use site that does fix the target keeps the conversion, so the
/// report cannot fire on a working `From` impl.
#[test]
fn a_conversion_the_use_site_targets_still_resolves() {
    for source in [
        "newtype Id = i64\nfn main() { let a: Id = 5.into()\n let _b: i64 = a.into() }\n",
        "struct P { a: i64 }\n         impl From<(i64, i64)> for P { fn from(t: (i64, i64)) -> P { P { a: t.0 } } }\n         fn main() { let p: P = (1, 2).into()\n let _ = p.a }\n",
        "fn take(v: Vec<i64>) -> i64 { v[0] }\nfn main() { let _ = take([1, 2].into()) }\n",
    ] {
        let d = diagnostics_for(source);
        assert!(d.is_empty(), "{source}: {d:?}");
    }
}

/// A number answers the `math` surface and the conversions, and nothing
/// else. An unsuffixed literal is an inference variable while its call is
/// checked, so its report waits for defaulting; without that wait the call
/// typed as a fresh variable and ran.
#[test]
fn a_collection_method_on_a_numeric_literal_is_rejected() {
    for (source, ty, method) in [
        ("fn main() { let _ = 12.len() }\n", "i64", "len"),
        ("fn main() { let _ = (12).len() }\n", "i64", "len"),
        ("fn main() { let _ = 1.2.len() }\n", "f64", "len"),
        ("fn main() { let _ = 12.is_empty() }\n", "i64", "is_empty"),
        ("fn main() { let _ = 12.get(0) }\n", "i64", "get"),
        ("fn main() { let _ = 12u8.len() }\n", "u8", "len"),
    ] {
        let d = diagnostics_for(source);
        assert!(
            d.iter().any(|diag| matches!(
                &diag.error,
                TypeError::UnresolvedMethod { ty: t, name, .. } if t == ty && name == method
            )),
            "{method} on {ty} should not resolve: {d:?}"
        );
    }
}

/// The surface a numeric literal does answer stays reachable, so the
/// deferred report cannot fire on a `math` row or a conversion.
#[test]
fn the_numeric_literal_surface_still_resolves() {
    for source in [
        "fn main() { let _ = (-1.5).abs() }\n",
        "fn main() { let _ = 9.sqrt() }\n",
        "fn main() { let _ = 2.pow(3) }\n",
        "fn main() { let _ = 3.max(4) }\n",
        "fn main() { let _ = 5.to_string() }\n",
        "fn main() { let _ = 12.clone() }\n",
    ] {
        let d = diagnostics_for(source);
        assert!(d.is_empty(), "{source}: {d:?}");
    }
}

#[test]
fn a_math_method_on_a_numeric_receiver_answers_concretely() {
    // A concrete answer is one an annotation can agree or disagree
    // with; a free inference variable would accept both spellings.
    for (accepted, rejected) in [
        (
            "fn main() { let a: f64 = (-1.5).abs()\n let _ = a }\n",
            "fn main() { let a: String = (-1.5).abs()\n let _ = a }\n",
        ),
        (
            "fn main() { let a: f64 = 9.sqrt()\n let _ = a }\n",
            "fn main() { let a: i64 = 9.sqrt()\n let _ = a }\n",
        ),
        (
            "fn main() { let a: i64 = (-3).abs()\n let _ = a }\n",
            "fn main() { let a: f64 = (-3).abs()\n let _ = a }\n",
        ),
        (
            "fn main() { let a: bool = 1.5.is_nan()\n let _ = a }\n",
            "fn main() { let a: f64 = 1.5.is_nan()\n let _ = a }\n",
        ),
        (
            "fn main() { let a: Vec<f64> = #[1.0, -2.0].map(|x| x.abs())\n let _ = a }\n",
            "fn main() { let a: Vec<String> = #[1.0, -2.0].map(|x| x.abs())\n let _ = a }\n",
        ),
        (
            "fn main() { let a: Vec<i64> = #[1, -2].map(|x| x.abs())\n let _ = a }\n",
            "fn main() { let a: Vec<String> = #[1, -2].map(|x| x.abs())\n let _ = a }\n",
        ),
        (
            "fn main() { let a: Vec<f64> = #[1.0, -2.0].map(|v| v.abs())\n let _ = a }\n",
            "fn main() { let a: Vec<String> = #[1.0, -2.0].map(|v| v.abs())\n let _ = a }\n",
        ),
    ] {
        let d = diagnostics_for(accepted);
        assert!(d.is_empty(), "{accepted}: {d:?}");
        let d = diagnostics_for(rejected);
        assert!(!d.is_empty(), "{rejected} should not typecheck");
    }
}

#[test]
fn a_field_read_on_a_fieldless_receiver_is_rejected() {
    for source in [
        "fn main() { let x = 1\n let _ = x.bogus }\n",
        "fn main() { let s = \"hi\"\n let _ = s.bogus }\n",
        "fn main() { let v = #[1, 2]\n let _ = v.bogus }\n",
        "fn main() { let _ = #[1.0].map(|x| x.abs) }\n",
    ] {
        let d = diagnostics_for(source);
        assert!(
            d.iter()
                .any(|diag| matches!(&diag.error, TypeError::UnknownField { .. })),
            "{source} should report GT0006: {d:?}"
        );
    }
}

#[test]
fn a_missing_call_on_a_method_names_the_method() {
    let d = diagnostics_for("fn main() { let _ = #[1.0].map(|x| x.abs) }\n");
    assert!(
        d.iter().any(|diag| matches!(
            &diag.error,
            TypeError::UnknownField {
                method_of_same_name: true,
                ..
            }
        )),
        "the report should name `abs` as a method: {d:?}"
    );
}

#[test]
fn a_call_of_a_field_names_the_field() {
    let d = diagnostics_for(
        "struct P { name: String }\nfn main() { let p = P { name: \"a\" }\n let _ = p.name() }\n",
    );
    assert!(
        d.iter().any(|diag| matches!(
            &diag.error,
            TypeError::UnresolvedMethod {
                field_of_same_name: true,
                ..
            }
        )),
        "the report should name `name` as a field: {d:?}"
    );
}

#[test]
fn a_reference_pattern_parameter_over_a_value_type_names_the_type_spelling() {
    let d = diagnostics_for(
        "fn total(&m: Map<String, i64>) -> i64 { m.len() }\nfn main() { let _ = total({\"a\": 1}) }\n",
    );
    let found = d.iter().find_map(|diag| match &diag.error {
        TypeError::ReferenceParameterPatternPosition {
            binding,
            reference_ty,
            ..
        } => Some((binding.clone(), reference_ty.clone())),
        _ => None,
    });
    assert_eq!(
        found,
        Some(("m".to_string(), "&Map<String, i64>".to_string())),
        "the report should name `m: &Map<String, i64>`: {d:?}"
    );
}

#[test]
fn a_mutable_reference_pattern_parameter_names_the_mutable_type_spelling() {
    let d = diagnostics_for("fn bump(&mut n: i64) { let _ = n }\nfn main() { }\n");
    let found = d.iter().find_map(|diag| match &diag.error {
        TypeError::ReferenceParameterPatternPosition { reference_ty, .. } => {
            Some(reference_ty.clone())
        }
        _ => None,
    });
    assert_eq!(
        found,
        Some("&mut i64".to_string()),
        "the report should name `n: &mut i64`: {d:?}"
    );
}

#[test]
fn a_closure_reference_pattern_parameter_over_a_value_type_is_reported() {
    let d = diagnostics_for("fn main() { let _ = #[1, 2].map(|&v: i64| v + 1) }\n");
    assert!(
        d.iter().any(|diag| matches!(
            &diag.error,
            TypeError::ReferenceParameterPatternPosition { .. }
        )),
        "a closure parameter should report GT0069 too: {d:?}"
    );
}

#[test]
fn a_reference_typed_parameter_keeps_its_referent() {
    let d = diagnostics_for(
        "fn total(m: Map<String, i64>) -> i64 { m.len() }\nfn main() { let m = {\"a\": 1}\n let _ = total(m) }\n",
    );
    assert!(d.is_empty(), "a reference parameter is the spelling: {d:?}");
}

#[test]
fn json_value_method_form_types_from_the_same_table_as_the_free_form() {
    let d = diagnostics_for(
        "use std::encoding::json\nfn main() { let v = json::parse(\"{}\").unwrap()\n let b: i64 = v.get(\"a\") }\n",
    );
    assert!(
        d.iter()
            .any(|diag| matches!(&diag.error, TypeError::TypeMismatch { .. })),
        "`v.get(k)` is `Option<json::Value>`, not whatever the caller annotates: {d:?}"
    );
}

#[test]
fn json_value_accessor_methods_typecheck_clean() {
    let d = diagnostics_for(
        "use std::encoding::json\nfn main() { let v = json::parse(\"{}\").unwrap()\n let n: Option<i64> = v.as_i64()\n let s: Option<String> = v.as_str()\n let k: Option<json::Value> = v.get(\"a\")\n let l: i64 = v.len()\n let z: bool = v.is_null()\n println(\"{:?} {:?} {:?} {} {}\", n, s, k, l, z) }\n",
    );
    assert!(d.is_empty(), "the json accessor surface types: {d:?}");
}

/// A `json::Value` and a `DynValue` answer their own tables and nothing else,
/// so a name neither declares is unresolved at the call site.
#[test]
fn a_method_neither_dynamic_value_declares_is_unresolved() {
    for source in [
        "use std::encoding::json\nfn main() { let v = json::parse(\"[1]\").unwrap()\n let _ = json::at(v, 0).and_then(json::as_str) }\n",
        "use std::encoding::json\nfn main() { let v = json::parse(\"{}\").unwrap()\n let _ = v.frobnicate() }\n",
        "fn main() { let d = DynValue::int(7)\n let _ = d.frobnicate() }\n",
    ] {
        let d = diagnostics_for(source);
        assert!(has_code(&d, "GT0002"), "{source} -> {d:?}");
    }
    let d = diagnostics_for(
        "use std::encoding::json\nfn main() { let v = json::parse(\"{}\").unwrap()\n let _ = v.clone()\n let _ = v.to_string() }\n",
    );
    assert!(
        d.is_empty(),
        "every value answers clone and to_string: {d:?}"
    );
}

/// A callable declares no methods, so a method reached on a named function,
/// a closure binding, or an `Fn(..)` parameter is unresolved rather than
/// silently answering against the function's own name.
#[test]
fn a_method_on_a_callable_is_unresolved() {
    for source in [
        "fn wrap(s: String) -> String { s }\n\
         fn main() { let _ = wrap.len() }\n",
        "fn main() { let f = |v: i64| v + 1\n let _ = f.len() }\n",
        "fn call(f: Fn(i64) -> i64) -> i64 { f.len() }\n",
        "fn wrap(s: String) -> String { s }\n\
         fn main() { let _ = wrap.to_string() }\n",
    ] {
        let d = diagnostics_for(source);
        assert!(has_code(&d, "GT0002"), "{source} -> {d:?}");
    }
}

/// A callable in value position stays a callback: the receiver rejection
/// covers method calls only, not the eta-expansion that feeds combinators.
#[test]
fn a_callable_in_value_position_still_feeds_a_combinator() {
    let d = diagnostics_for(
        "fn dbl(v: i64) -> i64 { v * 2 }\n\
         fn main() { let xs = #[1, 2]\n let _ = xs.map(dbl) }\n",
    );
    assert!(!has_code(&d, "GT0002"), "{d:?}");
}

// A receiver that holds no ordered buffer cannot sort: on 0.55.0 these
// typechecked and then reordered nothing, so a top-N report printed its
// first N entries in the source's own traversal order.

#[test]
fn sorting_a_lazy_iterator_is_rejected() {
    let d = diagnostics_for(
        "fn main() { let mut m = {}\n m.inc(\"a\")\n \
         let mut pairs = m.iter()\n pairs.sort_by_key(|p| p.1)\n let _ = pairs }\n",
    );
    assert!(has_code(&d, "GT0002"), "{d:?}");
}

#[test]
fn sorting_a_range_is_rejected() {
    let d = diagnostics_for("fn main() { let xs = (1..6).sort_by(|a, b| a - b)\n let _ = xs }\n");
    assert!(has_code(&d, "GT0002"), "{d:?}");
}

#[test]
fn sorting_a_vec_in_place_is_accepted() {
    let d = diagnostics_for(
        "fn main() { let mut xs = #[3, 1, 2]\n xs.sort_by_key(|v| v)\n let _ = xs }\n",
    );
    assert!(d.is_empty(), "{d:?}");
}

// A scalar orders with `<`, compares with `==`, and renders through `{}`.
// The method spellings typechecked on 0.55.0 and then failed at run time
// as an unbound name on every tier.

#[test]
fn derived_trait_methods_on_a_scalar_are_rejected() {
    for call in ["a.cmp(b)", "a.eq(b)", "a.fmt()", "a.hash()"] {
        let source = format!("fn main() {{ let a = 3\n let b = 5\n let _ = {call} }}\n");
        let d = diagnostics_for(&source);
        assert!(has_code(&d, "GT0002"), "{call}: {d:?}");
    }
}

#[test]
fn conversions_on_a_scalar_are_accepted() {
    let d =
        diagnostics_for("fn main() { let a = 3\n let _ = a.clone()\n let _ = a.to_string() }\n");
    assert!(d.is_empty(), "{d:?}");
}

#[test]
fn a_free_function_in_method_position_on_a_scalar_is_rejected() {
    let d = diagnostics_for(
        "fn double(v: i64) -> i64 { v * 2 }\nfn main() { let a = 3\n let _ = a.double() }\n",
    );
    assert!(has_code(&d, "GT0002"), "{d:?}");
}

#[test]
fn an_item_imported_free_function_reaches_a_scalar_receiver() {
    let d = diagnostics_for("use std::math::abs\nfn main() { let a = -3.0\n let _ = a.abs() }\n");
    assert!(d.is_empty(), "{d:?}");
}

/// A parse-time desugar renames the call it builds, and a diagnostic that
/// named the rewritten method sent the reader hunting for a word their file
/// does not contain. Each sort spelling reports itself.
#[test]
fn a_rejected_sort_reports_the_spelling_the_source_wrote() {
    for (source, written) in [
        (
            "fn main() { let xs = (1..6).sort_by_key(|n: i64| n)\n let _ = xs }\n",
            "sort_by_key",
        ),
        (
            "fn main() { let xs = (1..6).sort_by_key_desc(|n: i64| n)\n let _ = xs }\n",
            "sort_by_key_desc",
        ),
        (
            "fn main() { let xs = (1..6).sort_by(|a: i64, b: i64| a - b)\n let _ = xs }\n",
            "sort_by",
        ),
    ] {
        let diagnostics = diagnostics_for(source);
        let Some(found) = diagnostics.iter().find(|d| d.error.code() == "GT0002") else {
            panic!("{written}: expected GT0002, got {diagnostics:?}");
        };
        let reported = format!("{}", found.error);
        assert!(
            reported.contains(&format!("`{written}`")),
            "expected the diagnostic to name `{written}`, got: {reported}"
        );
    }
}

/// The sort family keeps its receiver form on the types that hold an ordered
/// buffer, so the rejection names what this receiver lacks rather than
/// claiming the method exists nowhere.
#[test]
fn a_rejected_sort_names_a_free_call_that_exists() {
    let diagnostics = diagnostics_for(
        "fn main() { let xs = (1..6).sort_by_key_desc(|n: i64| n)\n let _ = xs }\n",
    );
    let rendered = diagnostics
        .iter()
        .find(|d| d.error.code() == "GT0002")
        .map(|d| format!("{:?}", d.to_diagnostic()))
        .expect("expected GT0002");
    assert!(
        rendered.contains("iter::sort_by_key"),
        "the help names the free call that sorts a sequence: {rendered}"
    );
}

/// An `else if` chain with no final `else` answers no value, so its branches
/// keep their own types the way an else-less `if` does.
#[test]
fn an_else_if_chain_without_a_final_else_discards_branch_values() {
    let diagnostics = diagnostics_for(
        "fn main() {\n    let mut m: Map<String, i64> = Map::new()\n    for i in 0..3 {\n        if i == 0 {\n            m.insert(\"a\", i)\n        } else if i == 1 {\n            m.insert(\"b\", i)\n        }\n    }\n    let _ = m\n}\n",
    );
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

/// A loop body discards its tail, so an `if` chain there may end its
/// branches in values of different types.
#[test]
fn a_loop_body_tail_chain_may_answer_different_branch_types() {
    let diagnostics = diagnostics_for(
        "fn main() {\n    let mut m: Map<String, i64> = Map::new()\n    let mut s: Set<i64> = #{}\n    for i in 0..3 {\n        if i == 0 {\n            m.insert(\"a\", i)\n        } else {\n            s.insert(i)\n        }\n    }\n    let _ = m\n    let _ = s\n}\n",
    );
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

/// A chain with no final `else` has no value to bind, so asking for one is
/// still a type mismatch.
#[test]
fn an_else_if_chain_without_a_final_else_has_no_value_to_bind() {
    let diagnostics = diagnostics_for(
        "fn pick(i: i64) -> i64 {\n    let v: i64 = if i == 0 { 1 } else if i == 1 { 2 }\n    v\n}\nfn main() { let _ = pick(0) }\n",
    );
    assert!(
        diagnostics.iter().any(|d| d.error.code() == "GT0001"),
        "{diagnostics:?}"
    );
}

/// Discarding a chain's branch value still reports a discarded `Result`.
#[test]
fn an_else_if_chain_still_reports_a_discarded_result() {
    let diagnostics = diagnostics_for(
        "fn fallible(i: i64) -> Result<i64, String> { Ok(i) }\nfn main() {\n    for i in 0..3 {\n        if i == 0 {\n            fallible(i)\n        } else if i == 1 {\n            fallible(i)\n        }\n    }\n}\n",
    );
    assert!(
        diagnostics.iter().any(|d| d.error.code() == "GT0007"),
        "{diagnostics:?}"
    );
}

/// Wrapping arithmetic keeps the integer type its operands share.
#[test]
fn wrapping_arithmetic_keeps_the_operand_integer_type() {
    let diagnostics = diagnostics_for(
        "struct Acc { total: u16 }\nfn step(h: u32, b: u8) -> u32 { (h << 5) +% h +% b as u32 }\nfn main() {\n    let mut acc = Acc { total: 1 }\n    acc.total +%= 7\n    acc.total *%= 3\n    let x: u8 = 250\n    let y: u8 = x -% b'z'\n    let _ = step(y as u32, x)\n}\n",
    );
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

/// Wrapping names a width, so a float or string operand is GT0003.
#[test]
fn wrapping_arithmetic_rejects_operands_without_a_width() {
    let floats = diagnostics_for("fn main() { let _ = 1.5 +% 2.0 }\n");
    assert!(has_code(&floats, "GT0003"), "{floats:?}");
    let strings = diagnostics_for("fn main() {\n    let mut s = \"a\"\n    s +%= \"b\"\n}\n");
    assert!(has_code(&strings, "GT0003"), "{strings:?}");
}

/// Wrapping arithmetic has one spelling: the method forms report GT0087.
#[test]
fn wrapping_arithmetic_methods_point_to_the_operator() {
    for call in [
        "a.wrapping_add(1)",
        "a.wrapping_mul(2)",
        "a.wrapping_sub(3)",
        "12.wrapping_add(1)",
    ] {
        let source = format!("fn main() {{ let a: u32 = 3\n let _ = {call} }}\n");
        let d = diagnostics_for(&source);
        assert!(has_code(&d, "GT0087"), "{call}: {d:?}");
    }
}

#[test]
fn a_struct_with_a_const_generic_length_checks_its_literal_and_methods() {
    let source = "struct Ring<const N: usize> {\n    items: [i64; N],\n    head: i64,\n}\n\
        impl<const N: usize> Ring<N> {\n    fn capacity(&self) -> i64 { N as i64 }\n    \
        fn first(&self) -> i64 { self.items[0] }\n}\n\
        fn main() {\n    let r: Ring<3> = Ring { items: [0; 3], head: 0 }\n    \
        let c: i64 = r.capacity()\n    let f: i64 = r.first()\n    let _ = c + f\n}\n";
    let d = diagnostics_for(source);
    assert!(d.is_empty(), "{d:?}");
}

#[test]
fn a_const_generic_fn_takes_its_value_from_a_generic_type_argument() {
    let source = "enum Grid<const N: usize> {\n    Filled([i64; N]),\n    Empty,\n}\n\
        fn size<const N: usize>(grid: Grid<N>) -> i64 {\n    N as i64\n}\n\
        fn main() {\n    let g: Grid<2> = Grid::Filled([7, 8])\n    let n: i64 = size(g)\n    let _ = n\n}\n";
    let d = diagnostics_for(source);
    assert!(d.is_empty(), "{d:?}");
}

#[test]
fn a_struct_literal_that_never_names_its_const_generic_reports_gt0088() {
    let source = "struct Tag<const N: usize> {\n    id: i64,\n}\n\
        fn main() {\n    let t = Tag { id: 1 }\n    let _ = t.id\n}\n";
    let d = diagnostics_for(source);
    assert!(has_code(&d, "GT0088"), "{d:?}");
}

#[test]
fn a_simd_lane_type_outside_the_supported_set_reports_gt0089() {
    let source = "fn main() {\n    let v: Simd<i16, 4> = Simd::splat(1)\n    let _ = v\n}\n";
    let d = diagnostics_for(source);
    assert!(has_code(&d, "GT0089"), "{d:?}");
}

#[test]
fn a_simd_lane_count_outside_the_supported_set_reports_gt0089() {
    let d = diagnostics_for(
        "fn main() {\n    let v: Simd<f64, 3> = Simd::splat(1.0)\n    let _ = v\n}\n",
    );
    assert!(has_code(&d, "GT0089"), "{d:?}");
    let d = diagnostics_for(
        "fn main() {\n    let v: Simd<f64, 16> = Simd::splat(1.0)\n    let _ = v\n}\n",
    );
    assert!(has_code(&d, "GT0089"), "{d:?}");
}

#[test]
fn simd_splat_without_an_annotated_lane_count_reports_gt0089() {
    let d = diagnostics_for("fn main() {\n    let v = Simd::splat(1.0)\n    let _ = v\n}\n");
    assert!(has_code(&d, "GT0089"), "{d:?}");
}

#[test]
fn simd_lane_arithmetic_and_methods_check() {
    let source = "fn main() {\n    let a = Simd::from_array([1.0, 2.0, 3.0, 4.0])\n    \
        let b: Simd<f64, 4> = Simd::splat(0.5)\n    let s: f64 = (a * b + a).reduce_sum()\n    \
        let m: Mask<4> = a.lanes_lt(b)\n    let c: Simd<f64, 4> = m.select(a, b)\n    \
        let x: f64 = c[2]\n    let _ = s + x\n}\n";
    let d = diagnostics_for(source);
    assert!(d.is_empty(), "{d:?}");
}

#[test]
fn a_simd_operation_its_lanes_do_not_define_reports_gt0089() {
    let d = diagnostics_for(
        "fn main() {\n    let a: Simd<f64, 4> = Simd::splat(1.0)\n    let b = a << a\n    let _ = b\n}\n",
    );
    assert!(has_code(&d, "GT0089"), "{d:?}");
}

#[test]
fn a_struct_literal_mismatching_its_annotated_const_generic_is_rejected() {
    let source = "struct Ring<const N: usize> {\n    items: [i64; N],\n}\n\
        fn main() {\n    let r: Ring<2> = Ring { items: [0; 3] }\n    let _ = r.items\n}\n";
    let d = diagnostics_for(source);
    assert!(has_code(&d, "GT0001"), "{d:?}");
}

#[test]
fn a_wrapping_method_chain_is_rewritten_whole_by_its_outermost_call() {
    let source = "fn main() { let a: u64 = 3\n let _ = a.wrapping_mul(5).wrapping_add(7) }\n";
    let replacements: Vec<String> = diagnostics_for(source)
        .into_iter()
        .filter_map(|d| match d.error {
            gossamer_types::TypeError::WrappingMethodRetired { replacement, .. } => replacement,
            _ => None,
        })
        .collect();
    assert!(
        replacements.iter().any(|r| r == "((a *% 5) +% 7)"),
        "{replacements:?}"
    );
}

#[test]
fn a_wrapping_method_with_a_prefixed_operand_is_rewritten() {
    for (call, rewrite) in [
        ("bb.wrapping_add(-1)", "(bb +% -1)"),
        ("bb.wrapping_sub(-d)", "(bb -% -d)"),
        ("(!bb).wrapping_mul(2)", "(!bb *% 2)"),
    ] {
        let source =
            format!("fn main() {{ let bb: i64 = 12\n let d: i64 = 3\n let _ = {call} }}\n");
        let replacements: Vec<String> = diagnostics_for(&source)
            .into_iter()
            .filter_map(|d| match d.error {
                gossamer_types::TypeError::WrappingMethodRetired { replacement, .. } => replacement,
                _ => None,
            })
            .collect();
        assert!(
            replacements.iter().any(|r| r == rewrite),
            "{call}: {replacements:?}"
        );
    }
}
