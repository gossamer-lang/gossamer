//! Integer operands meet at one type: comparisons, integer methods, and
//! element queries unify like arithmetic, literals fit the type they take,
//! and the operators an integer alone answers reject other operands.

use gossamer_lex::SourceMap;
use gossamer_parse::parse_source_file;
use gossamer_resolve::resolve_source_file;
use gossamer_types::{TypeDiagnostic, TypeError, typecheck_source_file};

fn diagnostics(body: &str) -> Vec<TypeDiagnostic> {
    let source = format!(
        "fn main() {{\n    let a: u32 = 5\n    let b: i64 = 7\n    let c: u8 = 200\n    let x: u64 = 9\n    let f: f64 = 1.5\n    {body}\n    println(f\"{{a}} {{b}} {{c}} {{x}} {{f}}\")\n}}\n"
    );
    let mut map = SourceMap::new();
    let file = map.add_file("integer-strictness.gos", source.clone());
    let (mut parsed, parse_diags) = parse_source_file(&source, file);
    assert!(parse_diags.is_empty(), "parse errors: {parse_diags:?}");
    let (resolutions, resolve_diags) = resolve_source_file(&parsed);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut parsed, &resolutions);
    assert!(
        resolve_diags.is_empty(),
        "resolve errors: {resolve_diags:?}"
    );
    let mut tcx = gossamer_types::TyCtxt::new();
    typecheck_source_file(&parsed, &resolutions, &mut tcx).1
}

fn errors(body: &str) -> Vec<TypeError> {
    diagnostics(body)
        .into_iter()
        .filter(|diagnostic| !diagnostic.is_advisory())
        .map(|diagnostic| diagnostic.error)
        .collect()
}

fn accepted(body: &str) {
    let errors = errors(body);
    assert!(errors.is_empty(), "`{body}` produced {errors:#?}");
}

#[test]
fn a_comparison_of_two_integer_types_names_the_lossless_cast() {
    for (body, target) in [
        ("let r = a < b", "i64"),
        ("let r = a == b", "i64"),
        ("let r = b >= c", "i64"),
        ("let r = c != a", "u32"),
    ] {
        let errors = errors(body);
        assert!(
            matches!(
                errors.as_slice(),
                [TypeError::IntegerOperandMismatch { fix, .. }]
                    if fix.target.as_deref() == Some(target) && fix.casts.len() == 1
            ),
            "`{body}` produced {errors:#?}"
        );
    }
}

#[test]
fn a_comparison_with_no_lossless_common_type_offers_no_cast() {
    let errors = errors("let r = x < b");
    assert!(
        matches!(
            errors.as_slice(),
            [TypeError::IntegerOperandMismatch { fix, .. }]
                if fix.target.is_none() && fix.casts.is_empty()
        ),
        "{errors:#?}"
    );
}

#[test]
fn same_width_signed_and_unsigned_meet_at_the_next_signed_width() {
    let errors = errors("let i: i32 = -1\n    let r = a < i");
    assert!(
        matches!(
            errors.as_slice(),
            [TypeError::IntegerOperandMismatch { fix, .. }]
                if fix.target.as_deref() == Some("i64") && fix.casts.len() == 2
        ),
        "{errors:#?}"
    );
}

#[test]
fn integer_bound_methods_take_the_receivers_type() {
    for body in [
        "let r = c.min(b)",
        "let r = c.max(b)",
        "let r = c.clamp(0, b)",
    ] {
        let errors = errors(body);
        assert!(
            matches!(
                errors.as_slice(),
                [TypeError::IntegerOperandMismatch { .. }]
            ),
            "`{body}` produced {errors:#?}"
        );
    }
    accepted("let r = c.min(3)");
    accepted("let r = b.max(b + 1)");
}

#[test]
fn element_queries_take_the_element_type() {
    for body in [
        "let v: Vec<u8> = #[1]\n    let r = v.contains(b)",
        "let v: Vec<u8> = #[1]\n    let r = v.index_of(b)",
        "let v: Vec<u8> = #[1]\n    let r = v.count_of(b)",
        "let s: Set<u8> = #{1}\n    let r = s.contains(b)",
    ] {
        let errors = errors(body);
        assert!(!errors.is_empty(), "`{body}` was accepted");
    }
    accepted("let v: Vec<u8> = #[1]\n    let r = v.contains(1)");
}

#[test]
fn a_literal_compared_or_matched_must_fit_the_operands_type() {
    for body in [
        "let r = c == 300",
        "let r = a > -1",
        "match c {\n        300 => println(\"x\")\n        _ => println(\"y\")\n    }",
    ] {
        let errors = errors(body);
        assert!(
            matches!(errors.as_slice(), [TypeError::IntLiteralOverflow { .. }]),
            "`{body}` produced {errors:#?}"
        );
    }
    accepted("let r = c == 255");
    accepted("let i: i8 = -128\n    let r = i == -128");
}

#[test]
fn negating_an_unsigned_value_is_rejected() {
    for body in ["let r = -a", "let r = -(a + 1)"] {
        let errors = errors(body);
        assert!(
            matches!(errors.as_slice(), [TypeError::UnsignedNegation { .. }]),
            "`{body}` produced {errors:#?}"
        );
    }
    accepted("let r = -b");
    accepted("let r = 0 -% a");
}

#[test]
fn bitwise_operators_require_integers_and_shifts_reject_bool() {
    for body in [
        "let r = f & f",
        "let r = 1.5 | 2.5",
        "let r = true << 1",
        "let r = \"a\" >> 1",
        "let r = b << f",
    ] {
        let errors = errors(body);
        assert!(
            matches!(errors.as_slice(), [TypeError::UnresolvedOp { .. }]),
            "`{body}` produced {errors:#?}"
        );
    }
    accepted("let r = true & false");
    accepted("let r = a ^ 3");
}

#[test]
fn a_literal_shift_amount_must_be_below_the_types_width() {
    for body in ["let r = b << 64", "let r = c >> 8", "let r = b << -1"] {
        let errors = errors(body);
        assert!(
            matches!(errors.as_slice(), [TypeError::ShiftAmountOutOfRange { .. }]),
            "`{body}` produced {errors:#?}"
        );
    }
    accepted("let r = b << 63");
    accepted("let r = c >> 7");
    accepted("let r = b <<% 64");
}

#[test]
fn a_shift_amount_may_be_any_integer_type() {
    accepted("let r = c << b");
    accepted("let r = b >> c");
    accepted("let r = x <<% b");
    accepted("let mut m = x\n    m >>%= c");
    accepted("let mut m = x\n    m <<= c");
    let errors = errors("let mut m = c\n    m <<= 8");
    assert!(
        matches!(errors.as_slice(), [TypeError::ShiftAmountOutOfRange { .. }]),
        "{errors:#?}"
    );
}
