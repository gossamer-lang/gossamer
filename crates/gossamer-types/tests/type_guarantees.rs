//! Language-level type stability and mutability guarantees.

use gossamer_lex::SourceMap;
use gossamer_parse::parse_source_file;
use gossamer_resolve::resolve_source_file;
use gossamer_types::{TypeDiagnostic, TypeError, typecheck_source_file};

fn diagnostics(source: &str) -> Vec<TypeDiagnostic> {
    let mut map = SourceMap::new();
    let file = map.add_file("type-guarantees.gos", source.to_string());
    let (mut parsed, parse_diags) = parse_source_file(source, file);
    assert!(parse_diags.is_empty(), "parse errors: {parse_diags:?}");
    let (mut resolutions, resolve_diags) = resolve_source_file(&parsed);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut parsed, &mut resolutions);
    assert!(
        resolve_diags.is_empty(),
        "resolve errors: {resolve_diags:?}"
    );
    let mut tcx = gossamer_types::TyCtxt::new();
    typecheck_source_file(&parsed, &resolutions, &mut tcx).1
}

fn assert_type_checks(source: &str) {
    let diagnostics = diagnostics(source);
    assert!(diagnostics.is_empty(), "{diagnostics:#?}");
}

fn mismatch_pairs(diagnostics: &[TypeDiagnostic]) -> Vec<(&str, &str)> {
    diagnostics
        .iter()
        .filter_map(|diagnostic| match &diagnostic.error {
            TypeError::TypeMismatch { expected, found } => {
                Some((expected.as_str(), found.as_str()))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn mutable_immutable_and_constant_scalars_keep_declared_types() {
    assert_type_checks(
        "const LIMIT: i64 = 256
         fn main() {
             let immutable = LIMIT
             let mut mutable: i64 = immutable
             mutable = LIMIT
         }\n",
    );

    let diagnostics = diagnostics(
        "const LIMIT: i64 = 256
         fn main() {
             let immutable = 256
             let mut narrow: i8 = 1
             narrow = immutable
             narrow = LIMIT
             immutable = 1
         }\n",
    );
    assert_eq!(
        mismatch_pairs(&diagnostics),
        vec![("i8", "i64"), ("i8", "i64")]
    );
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| matches!(diagnostic.error, TypeError::AssignToImmutable { .. })),
        "immutable assignment must be rejected: {diagnostics:#?}"
    );
}

#[test]
fn nominal_structs_never_unify_from_matching_layouts() {
    let diagnostics = diagnostics(
        "struct Left { value: i64 }
         struct Right { value: i64 }
         fn take_left(value: Left) {}
         fn main() {
             let right = Right { value: 1 }
             let mut left = Left { value: 2 }
             left = right
             take_left(right)
         }\n",
    );
    let mismatches = mismatch_pairs(&diagnostics);
    assert_eq!(mismatches, vec![("Left", "Right"), ("Left", "Right")]);
}

#[test]
fn nested_struct_fields_enforce_leaf_and_nominal_types() {
    let diagnostics = diagnostics(
        "struct Inner { count: i8 }
         struct OtherInner { count: i8 }
         struct Outer { inner: Inner }
         fn main() {
             let immutable = Outer { inner: Inner { count: 1 } }
             let mut mutable = Outer { inner: Inner { count: 2 } }
             mutable.inner.count = 3
             mutable.inner.count = 256
             mutable.inner = OtherInner { count: 4 }
             immutable.inner.count = 5
         }\n",
    );
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| matches!(diagnostic.error, TypeError::IntLiteralOverflow { .. })),
        "narrow nested field must range-check: {diagnostics:#?}"
    );
    assert!(
        mismatch_pairs(&diagnostics).contains(&("Inner", "OtherInner")),
        "nested nominal field must reject a layout-compatible type: {diagnostics:#?}"
    );
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| matches!(diagnostic.error, TypeError::AssignToImmutable { .. })),
        "nested write through an immutable root must fail: {diagnostics:#?}"
    );
}

#[test]
fn enum_identity_and_payload_types_are_stable() {
    let diagnostics = diagnostics(
        "enum Small { Value(i8), Empty }
         enum Other { OtherValue(i8), OtherEmpty }
         fn take_small(value: Small) {}
         fn main() {
             let immutable = Small::Value(1)
             let mut mutable = Small::Empty
             mutable = immutable
             mutable = Small::Value(256)
             mutable = Other::OtherEmpty
             take_small(Other::OtherValue(1))
             immutable = Small::Empty
         }\n",
    );
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| matches!(diagnostic.error, TypeError::IntLiteralOverflow { .. })),
        "enum payload must range-check: {diagnostics:#?}"
    );
    let mismatches = mismatch_pairs(&diagnostics);
    assert!(
        mismatches.contains(&("Small", "Other")),
        "distinct enums must remain nominal: {diagnostics:#?}"
    );
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| matches!(diagnostic.error, TypeError::AssignToImmutable { .. })),
        "immutable enum binding must reject reassignment: {diagnostics:#?}"
    );
}

#[test]
fn nested_collection_and_option_inference_cannot_be_narrowed_later() {
    let diagnostics = diagnostics(
        "fn main() {
             let matrix = [[256, 257], [258, 259]]
             let optional = Some([256, 257])
             let narrowed_matrix: Vec<Vec<i8>> = matrix
             let narrowed_option: Option<Vec<i8>> = optional
         }\n",
    );
    let mismatches = mismatch_pairs(&diagnostics);
    assert_eq!(mismatches.len(), 2, "{diagnostics:#?}");
    assert!(
        mismatches
            .iter()
            .all(|(expected, found)| expected.contains("i8") && found.contains("i64")),
        "nested inferred types must retain their defaulted element type: {mismatches:?}"
    );
}

#[test]
fn byte_builder_and_buffer_have_complete_public_type_contracts() {
    assert_type_checks(
        "use std::bytes
         fn main() {
             let mut text = bytes::Builder::with_capacity(8)
             text.write(\"ab\")
             text.write_char('c')
             let _: i64 = text.len()
             let _: String = text.as_str()
             let _: String = text.build()

             let mut data = bytes::Buffer::with_capacity(8)
             data.write_str(\"ab\")
             data.push(255)
             let _: i64 = data.len()
             let _: bool = data.is_empty()
             let _: String = data.to_string()
             data.clear()
         }\n",
    );

    let diagnostics = diagnostics(
        "use std::bytes
         fn main() {
             let immutable = bytes::Buffer::new()
             let mut data = bytes::Buffer::new()
             data.push(\"A\")
             data.push([1, 2])
             data.push(-1)
             data.push(256)
             data.push(1, 2)
             data.write_str(1)
             immutable.push(1)

             let mut text = bytes::Builder::new()
             text.write(1)
             text.write_char(\"x\")
             text.write_char('x', 'y')
         }\n",
    );
    assert!(
        diagnostics.len() >= 10,
        "invalid byte and text operations must all be rejected: {diagnostics:#?}"
    );
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| matches!(diagnostic.error, TypeError::AssignToImmutable { .. })),
        "mutation through an immutable buffer must be rejected: {diagnostics:#?}"
    );
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| matches!(diagnostic.error, TypeError::IntLiteralOverflow { .. })),
        "Buffer::push must range-check u8 literals: {diagnostics:#?}"
    );
}
