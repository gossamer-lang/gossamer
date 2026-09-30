//! Regression matrix for binding and reference mutability.
//!
//! These are language-soundness tests, not merely diagnostic snapshots. Every
//! rejected program describes a path that could otherwise modify storage whose
//! source binding or intervening reference is immutable. The accepted cases
//! pin the distinction between a mutable binding and a mutable reference.

use gossamer_lex::SourceMap;
use gossamer_parse::parse_source_file;
use gossamer_resolve::resolve_source_file;
use gossamer_types::{TyCtxt, TypeDiagnostic, TypeError, typecheck_source_file};

#[derive(Clone, Copy, Debug)]
enum ExpectedError {
    ImmutableBinding,
    SharedReference,
    MutableReferenceToImmutable,
    ExplicitMutableArgument,
    MutableReferenceConflict,
    ReferenceEscape,
    BorrowedPlaceConflict,
    ConcurrentInlineAggregate,
}

impl ExpectedError {
    fn matches(self, error: &TypeError) -> bool {
        match self {
            Self::ImmutableBinding => matches!(error, TypeError::AssignToImmutable { .. }),
            Self::SharedReference => {
                matches!(error, TypeError::AssignThroughSharedReference { .. })
            }
            Self::MutableReferenceToImmutable => {
                matches!(error, TypeError::MutableReferenceToImmutable { .. })
            }
            Self::ExplicitMutableArgument => {
                matches!(error, TypeError::MutableArgumentRequiresReference { .. })
            }
            Self::MutableReferenceConflict => {
                matches!(error, TypeError::MutableReferenceConflict { .. })
            }
            Self::ReferenceEscape => {
                matches!(error, TypeError::ReferenceEscapeUnsupported { .. })
            }
            Self::BorrowedPlaceConflict => {
                matches!(error, TypeError::BorrowedPlaceConflict { .. })
            }
            Self::ConcurrentInlineAggregate => {
                matches!(error, TypeError::ConcurrentAggregateUnsupported { .. })
            }
        }
    }

    const fn code(self) -> &'static str {
        match self {
            Self::ImmutableBinding => "GT0030",
            Self::SharedReference => "GT0031",
            Self::MutableReferenceToImmutable => "GT0032",
            Self::ExplicitMutableArgument => "GT0046",
            Self::MutableReferenceConflict => "GT0043",
            Self::ReferenceEscape => "GT0052",
            Self::BorrowedPlaceConflict => "GT0053",
            Self::ConcurrentInlineAggregate => "GT0055",
        }
    }
}

#[test]
fn mutable_reference_parameters_require_visible_mutable_arguments() {
    let cases = [
        (
            "immutable fixed array",
            "fn change(v: &mut [i64]) { v[0] = 0 }\nfn main() { let a = [1, 2]\n change(a) }",
        ),
        (
            "mutable scalar",
            "fn change(v: &mut i64) { *v = 0 }\nfn main() { let mut a = 1\n change(a) }",
        ),
        (
            "mutable fixed array",
            "fn change(v: &mut [i64]) { v[0] = 0 }\nfn main() { let mut a = [1, 2]\n change(a) }",
        ),
        (
            "mutable vector",
            "fn change(v: &mut Vec<i64>) { v[0] = 0 }\nfn main() { let mut a: Vec<i64> = [1, 2]\n change(a) }",
        ),
        (
            "mutable field",
            "struct S { value: i64 }\nfn change(v: &mut i64) { *v = 0 }\nfn main() { let mut s = S { value: 1 }\n change(s.value) }",
        ),
        (
            "mutable index",
            "fn change(v: &mut i64) { *v = 0 }\nfn main() { let mut a = [1, 2]\n change(a[0]) }",
        ),
        (
            "generic mutable parameter",
            "fn identity<T>(v: &mut T) -> &mut T { v }\nfn main() { let mut a = 1\n let _ = identity(a) }",
        ),
        (
            "closure mutable parameter",
            "fn main() { let change = |v: &mut i64| { *v = 0 }\n let mut a = 1\n change(a) }",
        ),
        (
            "first-class mutable function",
            "fn change(v: &mut i64) { *v = 0 }\nfn main() { let f = change\n let mut a = 1\n f(a) }",
        ),
        (
            "qualified mutable receiver",
            "struct S { value: i64 }\nimpl S { fn change(&mut self) { self.value = 0 } }\nfn main() { let mut s = S { value: 1 }\n S::change(s) }",
        ),
        (
            "pipeline into mutable parameter",
            "fn change(v: &mut i64) { *v = 0 }\nfn main() { let mut a = 1\n a |> change }",
        ),
        (
            "goroutine bare mutable argument",
            "fn change(v: &mut Vec<i64>) { v[0] = 0 }\nfn main() { let mut a: Vec<i64> = [1]\n spawn(|| change(a)) }",
        ),
    ];

    for (name, source) in cases {
        assert_rejected(name, source, ExpectedError::ExplicitMutableArgument);
    }
}

#[test]
fn explicit_and_forwarded_mutable_references_remain_usable() {
    let cases = [
        (
            "explicit scalar reference",
            "fn change(v: &mut i64) { *v = 0 }\nfn main() { let mut a = 1\n change(&mut a) }",
        ),
        (
            "explicit field and index references",
            "struct S { values: Vec<i64> }\nfn change(v: &mut i64) { *v = 0 }\nfn main() { let mut s = S { values: Vec::from([1, 2]) }\n change(&mut s.values[0]) }",
        ),
        (
            "forward existing mutable reference",
            "fn change(v: &mut i64) { *v = 0 }\nfn forward(v: &mut i64) { change(v) }\nfn main() { let mut a = 1\n forward(&mut a) }",
        ),
        (
            "pipe existing mutable reference",
            "fn change(v: &mut i64) { *v = 0 }\nfn main() { let mut a = 1\n let r = &mut a\n r |> change }",
        ),
        (
            "closure accepts call-scoped mutable reference",
            "fn main() { let mut values: Vec<i64> = Vec::from([1])\n let change = |v: &mut Vec<i64>| { v[0] = 7 }\n change(&mut values) }",
        ),
    ];

    for (name, source) in cases {
        assert_accepted(name, source);
    }
}

#[test]
fn references_cannot_escape_through_function_returns() {
    assert_rejected(
        "returned mutable reference",
        "fn change(v: &mut [i64]) -> &mut [i64] { v[0] = 0\n v }\nfn main() { let mut a = [1, 2]\n let b = change(&mut a)\n b[0] = 2 }",
        ExpectedError::ReferenceEscape,
    );
}

#[test]
fn lexical_references_protect_their_source_places() {
    for (name, source) in [
        (
            "shared reference blocks source mutation",
            "fn main() { let mut values = [1, 2]\n let view = &values\n values[0] = 9\n println(\"{}\", view[0]) }",
        ),
        (
            "mutable reference blocks source reads",
            "fn main() { let mut values = [1, 2]\n let view = &mut values\n println(\"{}\", values[0])\n view[0] = 9 }",
        ),
    ] {
        assert_rejected(name, source, ExpectedError::BorrowedPlaceConflict);
    }

    assert_rejected(
        "shared reference blocks mutable reference",
        "fn main() { let mut values = [1, 2]\n let view = &values\n let writable = &mut values\n println(\"{} {}\", view[0], writable[0]) }",
        ExpectedError::MutableReferenceConflict,
    );

    assert_accepted(
        "source access resumes after the lexical reference scope",
        "fn main() { let mut values = [1, 2]\n { let view = &mut values\n view[0] = 7 }\n values[1] = 9 }",
    );
}

#[test]
fn references_are_call_scoped_and_cannot_enter_owned_or_concurrent_storage() {
    for (name, source) in [
        (
            "reference field",
            "struct Bad { value: &i64 }\nfn main() {}",
        ),
        (
            "reference nested in Vec parameter",
            "fn bad(values: Vec<&i64>) {}\nfn main() {}",
        ),
        (
            "reference nested in tuple local",
            "fn main() { let value = 1\n let bad = (&value,) }",
        ),
        (
            "reference inferred into channel storage",
            "fn main() { let tx, rx = channel()\n let value = 1\n let view = &value\n tx.send(view)\n let _ = rx }",
        ),
        (
            "reference crosses a spawn boundary",
            "fn see(value: i64) {}\nfn main() { let value = 1\n let view = &value\n spawn(|| see(view)) }",
        ),
        (
            "closure captures reference binding",
            "fn main() { let value = 1\n let view = &value\n let bad = || *view\n let _ = bad() }",
        ),
        (
            "closure returns reference",
            "fn main() { let value = 1\n let bad = || &value\n let _ = bad() }",
        ),
        (
            "named reference borrows temporary",
            "fn main() { let bad = &[1, 2, 3]\n println(\"{}\", bad.len()) }",
        ),
    ] {
        assert_rejected(name, source, ExpectedError::ReferenceEscape);
    }

    assert_accepted(
        "temporary and named-place slices are valid for one call",
        "fn sum(values: [i64]) -> i64 { values[0] + values[1] }\nfn main() { let values = [3, 4]\n let a = sum(values)\n let b = sum([5, 6])\n println(\"{} {}\", a, b) }",
    );
}

#[test]
fn mutable_call_arguments_reject_obvious_overlapping_aliases() {
    let cases = [
        (
            "same root in two call arguments",
            "fn swap(a: &mut i64, b: &mut i64) { let t = *a\n *a = *b\n *b = t }\nfn main() { let mut value = 1\n swap(&mut value, &mut value) }",
        ),
        (
            "call borrow overlaps named mutable reference",
            "fn change(value: &mut i64) { *value = 0 }\nfn main() { let mut value = 1\n let reference = &mut value\n change(&mut value)\n println(\"{}\", reference) }",
        ),
        (
            "method arguments share one mutable root",
            "struct S {}\nimpl S { fn use_two(&self, a: &mut i64, b: &mut i64) {} }\nfn main() { let s = S {}\n let mut value = 1\n s.use_two(&mut value, &mut value) }",
        ),
    ];

    for (name, source) in cases {
        assert_rejected(name, source, ExpectedError::MutableReferenceConflict);
    }
}

fn check(source: &str) -> Vec<TypeDiagnostic> {
    let mut map = SourceMap::new();
    let file = map.add_file("mutability-regression.gos".to_string(), source.to_string());
    let (mut parsed, parse_diagnostics) = parse_source_file(source, file);
    assert!(
        parse_diagnostics.is_empty(),
        "test case must parse before mutability is checked: {parse_diagnostics:#?}\nsource:\n{source}"
    );
    let (resolutions, resolve_diagnostics) = resolve_source_file(&parsed);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut parsed, &resolutions);
    assert!(
        resolve_diagnostics.is_empty(),
        "test case must resolve before mutability is checked: {resolve_diagnostics:#?}\nsource:\n{source}"
    );
    let mut tcx = TyCtxt::new();
    let (_, diagnostics) = typecheck_source_file(&parsed, &resolutions, &mut tcx);
    diagnostics
}

fn assert_rejected(name: &str, source: &str, expected: ExpectedError) {
    let diagnostics = check(source);
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| expected.matches(&diagnostic.error)),
        "{name} must report {} ({expected:?}); diagnostics: {diagnostics:#?}\nsource:\n{source}",
        expected.code(),
    );
}

fn assert_accepted(name: &str, source: &str) {
    let diagnostics = check(source);
    assert!(
        diagnostics.is_empty(),
        "{name} must be accepted; diagnostics: {diagnostics:#?}\nsource:\n{source}"
    );
}

#[test]
fn immutable_bindings_reject_every_assignment_place_shape() {
    let cases = [
        (
            "direct scalar assignment",
            "fn main() { let value = 1\n value = 2 }",
        ),
        (
            "compound scalar assignment",
            "fn main() { let value = 1\n value += 2 }",
        ),
        (
            "compound array-element assignment",
            "fn main() { let values = [1, 2]\n values[0] += 2 }",
        ),
        (
            "compound struct-field assignment",
            "struct Point { x: i64 }\nfn main() { let point = Point { x: 1 }\n point.x += 2 }",
        ),
        (
            "fixed-array element assignment",
            "fn main() { let values = [1, 2]\n values[0] = 9 }",
        ),
        (
            "nested-array element assignment",
            "fn main() { let values = [[1, 2], [3, 4]]\n values[0][1] = 9 }",
        ),
        (
            "tuple-field assignment",
            "fn main() { let pair = (1, 2)\n pair.0 = 9 }",
        ),
        (
            "struct-field assignment",
            "struct Point { x: i64 }\nfn main() { let point = Point { x: 1 }\n point.x = 9 }",
        ),
        (
            "nested-struct-field assignment",
            "struct Inner { x: i64 }\nstruct Outer { inner: Inner }\nfn main() { let outer = Outer { inner: Inner { x: 1 } }\n outer.inner.x = 9 }",
        ),
        (
            "indexed-struct-field assignment",
            "struct Point { x: i64 }\nfn main() { let points = [Point { x: 1 }]\n points[0].x = 9 }",
        ),
        (
            "struct-field-index assignment",
            "struct Bag { values: Vec<i64> }\nfn main() { let bag = Bag { values: [1, 2] }\n bag.values[0] = 9 }",
        ),
        (
            "immutable static assignment",
            "static VALUE: i64 = 1\nfn main() { VALUE = 2 }",
        ),
        (
            "constant assignment",
            "const VALUE: i64 = 1\nfn main() { VALUE = 2 }",
        ),
    ];

    for (name, source) in cases {
        assert_rejected(name, source, ExpectedError::ImmutableBinding);
    }
}

#[test]
fn mutable_reference_creation_requires_a_writable_place() {
    let cases = [
        (
            "immutable scalar",
            "fn main() { let value = 1\n let reference = &mut value }",
            ExpectedError::MutableReferenceToImmutable,
        ),
        (
            "immutable array element",
            "fn main() { let values = [1, 2]\n let reference = &mut values[0] }",
            ExpectedError::MutableReferenceToImmutable,
        ),
        (
            "immutable struct field",
            "struct Point { x: i64 }\nfn main() { let point = Point { x: 1 }\n let reference = &mut point.x }",
            ExpectedError::MutableReferenceToImmutable,
        ),
        (
            "shared-reference dereference",
            "fn main() { let mut value = 1\n let shared = &value\n let reference = &mut *shared }",
            ExpectedError::SharedReference,
        ),
        (
            "shared-reference index",
            "fn main() { let mut values = [1, 2]\n let shared = &values\n let reference = &mut shared[0] }",
            ExpectedError::SharedReference,
        ),
        (
            "shared-reference field",
            "struct Point { x: i64 }\nfn main() { let mut point = Point { x: 1 }\n let shared = &point\n let reference = &mut shared.x }",
            ExpectedError::SharedReference,
        ),
        (
            "mutable outer reference cannot cross shared inner reference",
            "fn main() { let values = [1, 2]\n let mut shared = &values\n let outer = &mut shared\n let reference = &mut outer[0] }",
            ExpectedError::SharedReference,
        ),
    ];

    for (name, source, expected) in cases {
        assert_rejected(name, source, expected);
    }
}

#[test]
fn shared_reference_in_any_auto_deref_layer_blocks_writes() {
    let cases = [
        (
            "direct shared dereference",
            "fn main() { let mut value = 1\n let shared = &value\n *shared = 2 }",
        ),
        (
            "direct shared index",
            "fn main() { let mut values = [1, 2]\n let shared = &values\n shared[0] = 9 }",
        ),
        (
            "direct shared field",
            "struct Point { x: i64 }\nfn main() { let mut point = Point { x: 1 }\n let shared = &point\n shared.x = 9 }",
        ),
        (
            "mutable reference around shared array reference",
            "fn main() { let values = [1, 2]\n let mut shared = &values\n let outer = &mut shared\n outer[0] = 9 }",
        ),
        (
            "parenthesized shared-reference projection",
            "fn main() { let values = [1, 2]\n let mut shared = &values\n let outer = &mut shared\n (*outer)[0] = 9 }",
        ),
        (
            "explicit double dereference",
            "fn main() { let values = [1, 2]\n let mut shared = &values\n let outer = &mut shared\n **outer = [9, 2] }",
        ),
        (
            "three-layer mixed reference chain",
            "fn main() { let values = [1, 2]\n let mut shared = &values\n let mut outer = &mut shared\n let third = &mut outer\n third[0] = 9 }",
        ),
        (
            "mutable outer reference around shared struct reference",
            "struct Point { x: i64 }\nfn main() { let point = Point { x: 1 }\n let mut shared = &point\n let outer = &mut shared\n outer.x = 9 }",
        ),
        (
            "mutable outer reference around shared nested reference",
            "struct Point { x: i64 }\nfn main() { let point = Point { x: 1 }\n let mut shared = &point\n let mut outer = &mut shared\n let third = &mut outer\n third.x = 9 }",
        ),
    ];

    for (name, source) in cases {
        assert_rejected(name, source, ExpectedError::SharedReference);
    }
}

#[test]
fn immutable_bindings_cannot_reach_mutation_through_calls() {
    let cases = [
        (
            "mutable-reference argument",
            "fn replace(value: &mut i64) { *value = 2 }\nfn main() { let value = 1\n replace(&mut value) }",
            ExpectedError::MutableReferenceToImmutable,
        ),
        (
            "user mutable method on immutable value",
            "struct Counter { value: i64 }\nimpl Counter { fn bump(&mut self) { self.value += 1 } }\nfn main() { let counter = Counter { value: 0 }\n counter.bump() }",
            ExpectedError::ImmutableBinding,
        ),
        (
            "user mutable method through shared reference",
            "struct Counter { value: i64 }\nimpl Counter { fn bump(&mut self) { self.value += 1 } }\nfn main() { let mut counter = Counter { value: 0 }\n let shared = &counter\n shared.bump() }",
            ExpectedError::SharedReference,
        ),
        (
            "user mutable method through mutable-then-shared chain",
            "struct Counter { value: i64 }\nimpl Counter { fn bump(&mut self) { self.value += 1 } }\nfn main() { let counter = Counter { value: 0 }\n let mut shared = &counter\n let outer = &mut shared\n outer.bump() }",
            ExpectedError::SharedReference,
        ),
        (
            "qualified user mutable method without a mutable argument",
            "struct Counter { value: i64 }\nimpl Counter { fn bump(&mut self) { self.value += 1 } }\nfn main() { let counter = Counter { value: 0 }\n Counter::bump(counter) }",
            ExpectedError::ExplicitMutableArgument,
        ),
        (
            "Vec push on immutable binding",
            "fn main() { let values: Vec<i64> = [1, 2]\n values.push(3) }",
            ExpectedError::ImmutableBinding,
        ),
        (
            "Vec push through shared reference",
            "fn main() { let mut values: Vec<i64> = [1, 2]\n let shared = &values\n shared.push(3) }",
            ExpectedError::SharedReference,
        ),
        (
            "String push_str on immutable binding",
            "fn main() { let text = \"a\"\n text.push_str(\"b\") }",
            ExpectedError::ImmutableBinding,
        ),
        (
            "built-in mutation on a field of an immutable struct",
            "struct Bag { values: Vec<i64> }\nfn main() { let bag = Bag { values: [1, 2] }\n bag.values.push(3) }",
            ExpectedError::ImmutableBinding,
        ),
        (
            "built-in mutation through shared-reference chain",
            "fn main() { let values: Vec<i64> = [1, 2]\n let mut shared = &values\n let outer = &mut shared\n outer.push(3) }",
            ExpectedError::SharedReference,
        ),
        (
            "qualified Map mutation on immutable binding",
            "fn main() { let map: Map<i64, i64> = Map::new()\n Map::insert(map, 1, 2) }",
            ExpectedError::ImmutableBinding,
        ),
        (
            "qualified Map mutation through shared reference",
            "fn main() { let mut map: Map<i64, i64> = Map::new()\n let shared = &map\n Map::remove(shared, 1) }",
            ExpectedError::SharedReference,
        ),
        (
            "Map inc on immutable binding",
            "fn main() { let map: Map<i64, i64> = Map::new()\n map.inc(1, 2) }",
            ExpectedError::ImmutableBinding,
        ),
        (
            "qualified Set mutation on immutable binding",
            "fn main() { let set: Set<i64> = Set::new()\n Set::insert(set, 1) }",
            ExpectedError::ImmutableBinding,
        ),
        (
            "user mutable method on an indexed immutable projection",
            "struct Counter { value: i64 }\nimpl Counter { fn bump(&mut self) { self.value += 1 } }\nfn main() { let counters = [Counter { value: 0 }]\n counters[0].bump() }",
            ExpectedError::ImmutableBinding,
        ),
    ];

    for (name, source, expected) in cases {
        assert_rejected(name, source, expected);
    }
}

#[test]
fn every_builtin_writeback_method_requires_a_writable_receiver() {
    let vec_cases = [
        ("push", "values.push(3)"),
        ("pop", "let _ = values.pop()"),
        ("insert", "values.insert(0, 3)"),
        ("remove", "let _ = values.remove(0)"),
        ("clear", "values.clear()"),
        ("extend", "values.extend([3, 4])"),
        ("extend_from_slice", "values.extend_from_slice([3, 4])"),
        ("truncate", "values.truncate(1)"),
        ("sort", "values.sort()"),
        ("sort_by", "values.sort_by(|a, b| a - b)"),
        ("sort_by_key", "values.sort_by_key(|a| a)"),
        ("reverse", "values.reverse()"),
        ("swap", "values.swap(0, 1)"),
    ];
    for (method, call) in vec_cases {
        let source = format!("fn main() {{ let values: Vec<i64> = [1, 2]\n {call} }}");
        assert_rejected(
            &format!("immutable Vec::{method} receiver"),
            &source,
            ExpectedError::ImmutableBinding,
        );
    }

    let string_cases = [
        ("push", "text.push('b')"),
        ("push_str", "text.push_str(\"b\")"),
        ("push_char", "text.push_char('b')"),
        ("push_byte", "text.push_byte(98)"),
        ("clear", "text.clear()"),
        ("truncate", "text.truncate(1)"),
    ];
    for (method, call) in string_cases {
        let source = format!("fn main() {{ let text = \"a\"\n {call} }}");
        assert_rejected(
            &format!("immutable String::{method} receiver"),
            &source,
            ExpectedError::ImmutableBinding,
        );
    }
}

#[test]
fn trait_mutable_receivers_follow_the_same_capability_rules() {
    let cases = [
        (
            "concrete trait method on immutable value",
            "trait Advance { fn advance(&mut self) }\nstruct Counter { value: i64 }\nimpl Advance for Counter { fn advance(&mut self) { self.value += 1 } }\nfn main() { let counter = Counter { value: 0 }\n counter.advance() }",
            ExpectedError::ImmutableBinding,
        ),
        (
            "trait default mutable method on immutable value",
            "trait Reset { fn reset(&mut self) {} }\nstruct Counter { value: i64 }\nimpl Reset for Counter {}\nfn main() { let counter = Counter { value: 0 }\n counter.reset() }",
            ExpectedError::ImmutableBinding,
        ),
        (
            "inherent mutable method takes precedence over shared trait method",
            "trait Read { fn access(&self) -> i64 }\nstruct Counter { value: i64 }\nimpl Counter { fn access(&mut self) -> i64 { self.value } }\nimpl Read for Counter { fn access(&self) -> i64 { self.value } }\nfn main() { let counter = Counter { value: 0 }\n let value = counter.access() }",
            ExpectedError::ImmutableBinding,
        ),
        (
            "generic mutable trait method on an immutable binding",
            "trait Advance { fn advance(&mut self) }\nfn run<T: Advance>(value: T) { value.advance() }",
            ExpectedError::ImmutableBinding,
        ),
        (
            "generic mutable trait method through immutable value parameter",
            "trait Advance { fn advance(&mut self) }\nfn run<T: Advance>(value: T) { value.advance() }",
            ExpectedError::ImmutableBinding,
        ),
        (
            "mutable trait method on immutable primitive receiver",
            "trait Advance { fn advance(&mut self) }\nimpl Advance for i64 { fn advance(&mut self) { *self += 1 } }\nfn main() { let value = 0\n value.advance() }",
            ExpectedError::ImmutableBinding,
        ),
    ];
    for (name, source, expected) in cases {
        assert_rejected(name, source, expected);
    }
}

#[test]
fn implicit_bindings_and_receivers_are_immutable_by_default() {
    let cases = [
        (
            "function parameter",
            "fn replace(value: i64) { value = 2 }\nfn main() {}",
            ExpectedError::ImmutableBinding,
        ),
        (
            "closure parameter",
            "fn main() { let closure = |value: i64| { value = 2 } }",
            ExpectedError::ImmutableBinding,
        ),
        (
            "captured immutable binding",
            "fn main() { let value = 1\n let closure = || { value = 2 } }",
            ExpectedError::ImmutableBinding,
        ),
        (
            "for-loop binding",
            "fn main() { for value in [1, 2] { value = 3 } }",
            ExpectedError::ImmutableBinding,
        ),
        (
            "match binding",
            "fn main() { match Some(1) { Some(value) => { value = 2 }, None => () } }",
            ExpectedError::ImmutableBinding,
        ),
        (
            "tuple destructuring binding",
            "fn main() { let left, right = (1, 2)\n left = right }",
            ExpectedError::ImmutableBinding,
        ),
        (
            "struct-pattern shorthand binding",
            "struct Point { x: i64 }\nfn main() { let point = Point { x: 1 }\n match point { Point { x } => { x = 2 } } }",
            ExpectedError::ImmutableBinding,
        ),
        (
            "or-pattern with inconsistent binding mutability",
            "fn main() { match 1 { mut value | value => { value = 2 } } }",
            ExpectedError::ImmutableBinding,
        ),
        (
            "owned self receiver",
            "struct Counter { value: i64 }\nimpl Counter { fn consume(self) { self.value = 2 } }\nfn main() {}",
            ExpectedError::ImmutableBinding,
        ),
        (
            "shared self receiver",
            "struct Counter { value: i64 }\nimpl Counter { fn inspect(&self) { self.value = 2 } }\nfn main() {}",
            ExpectedError::ImmutableBinding,
        ),
    ];

    for (name, source, expected) in cases {
        assert_rejected(name, source, expected);
    }
}

#[test]
fn mutable_places_and_reference_capabilities_remain_usable() {
    let cases = [
        (
            "direct mutable places",
            "struct Point { x: i64 }\nfn main() { let mut value = 1\n value = 2\n let mut values = [1, 2]\n values[0] = 9\n let mut point = Point { x: 1 }\n point.x = 9 }",
        ),
        (
            "explicitly mutable implicit bindings",
            "struct Point { x: i64 }\nfn update(mut value: i64) { value = 2 }\nfn main() { let mut left, right = (1, 2)\n left = right\n let point = Point { x: 1 }\n match point { Point { x: mut value } => { value = 2 } } }",
        ),
        (
            "immutable binding holding mutable reference",
            "fn main() { let mut values = [1, 2]\n let reference = &mut values\n reference[0] = 9 }",
        ),
        (
            "all-mutable nested reference chain",
            "fn main() { let mut values = [1, 2]\n let mut first = &mut values\n let second = &mut first\n second[0] = 9 }",
        ),
        (
            "reborrow mutable referent through immutable reference binding",
            "fn main() { let mut value = 1\n let first = &mut value\n let second = &mut *first\n *second = 9 }",
        ),
        (
            "mutable user method on mutable value",
            "struct Counter { value: i64 }\nimpl Counter { fn bump(&mut self) { self.value += 1 } }\nfn main() { let mut counter = Counter { value: 0 }\n counter.bump() }",
        ),
        (
            "mutable user method through mutable reference",
            "struct Counter { value: i64 }\nimpl Counter { fn bump(&mut self) { self.value += 1 } }\nfn main() { let mut counter = Counter { value: 0 }\n let reference = &mut counter\n reference.bump() }",
        ),
        (
            "qualified mutable user method through mutable reference",
            "struct Counter { value: i64 }\nimpl Counter { fn bump(&mut self) { self.value += 1 } }\nfn main() { let mut counter = Counter { value: 0 }\n Counter::bump(&mut counter) }",
        ),
        (
            "shared user method on immutable value",
            "struct Counter { value: i64 }\nimpl Counter { fn get(&self) -> i64 { self.value } }\nfn main() { let counter = Counter { value: 0 }\n let value = counter.get() }",
        ),
        (
            "shared user method may use a built-in mutator name",
            "struct Reader { value: i64 }\nimpl Reader { fn pop(&self) -> i64 { self.value } }\nfn main() { let reader = Reader { value: 7 }\n let value = reader.pop() }",
        ),
        (
            "metrics counters use interior mutability",
            "use std::metrics\nfn main() { let counter = metrics::Counter::new(\"hits\", \"hits\")\n counter.inc()\n let gauge = metrics::Gauge::new(\"depth\", \"depth\")\n gauge.inc() }",
        ),
        (
            "shared inherent method on an immutable binding beside a mutable trait method",
            "trait Advance { fn advance(&mut self) -> i64 }\nstruct Reader { value: i64 }\nimpl Reader { fn access(&self) -> i64 { self.value } }\nimpl Advance for Reader { fn advance(&mut self) -> i64 { self.value } }\nfn main() { let reader = Reader { value: 7 }\n let value = reader.access() }",
        ),
        (
            "mutable trait method through mutable reference",
            "trait Advance { fn advance(&mut self) }\nstruct Counter { value: i64 }\nimpl Advance for Counter { fn advance(&mut self) { self.value += 1 } }\nfn run<T: Advance>(value: &mut T) { value.advance() }\nfn main() { let mut counter = Counter { value: 0 }\n run(&mut counter) }",
        ),
        (
            "mutable builtin method on mutable value",
            "fn main() { let mut values: Vec<i64> = Vec::from([1, 2])\n values.push(3)\n let mut text = \"a\"\n text.push_str(\"b\") }",
        ),
        (
            "qualified map and set mutations on mutable values",
            "fn main() { let mut map: Map<i64, i64> = Map::new()\n Map::insert(map, 1, 2)\n let mut set: Set<i64> = Set::new()\n Set::insert(set, 1) }",
        ),
        (
            "mutable method through mutable-reference parameter",
            "struct Counter { value: i64 }\nimpl Counter { fn bump(&mut self) { self.value += 1 } }\nfn run(counter: &mut Counter) { counter.bump() }\nfn main() { let mut counter = Counter { value: 0 }\n run(&mut counter) }",
        ),
        (
            "value alias mutation does not require the source binding to be mutable",
            "fn main() { let original = [1, 2]\n let mut copy = original\n copy[0] = 9 }",
        ),
        (
            "scalar aggregate channel transfer remains available",
            "struct Pair { left: i64, right: i64 }\nfn main() { let value = Pair { left: 1, right: 2 }\n let tx, rx = channel::<Pair>(1)\n tx.send(value) }",
        ),
        (
            "reference cursor advances through matched recursive child",
            "enum Node { Link(Node), End }\nfn walk(head: Node) { let mut cursor = head\n loop { match cursor { Node::Link(next) => cursor = next, Node::End => break } } }",
        ),
    ];

    for (name, source) in cases {
        assert_accepted(name, source);
    }
}

#[test]
fn reference_slots_cannot_be_rebound_through_an_alias() {
    assert_rejected(
        "mutable outer reference cannot rebind shared-reference slot",
        "fn main() { let first = [1, 2]\n let second = [3, 4]\n let mut shared = &first\n let outer = &mut shared\n *outer = &second }",
        ExpectedError::ReferenceEscape,
    );
}

#[test]
fn reference_cursor_cannot_rebind_to_a_shorter_lived_alias() {
    assert_rejected(
        "outer cursor cannot retain an inner local through its reference alias",
        "fn main() { let outer = [1, 2]\n let mut cursor = &outer\n if true { let inner = [3, 4]\n let alias = &inner\n cursor = alias }\n let value = cursor[0] }",
        ExpectedError::ReferenceEscape,
    );
}

#[test]
fn concurrency_rejects_unmarshalable_inline_aggregates() {
    for (name, source) in [
        (
            "struct argument",
            "struct Wrapped { values: Vec<i64> }\nfn work(value: Wrapped) {}\nfn main() { let value = Wrapped { values: Vec::from([1, 2]) }\n spawn(|| work(value)) }",
        ),
        (
            "scalar struct goroutine argument",
            "struct Pair { left: i64, right: i64 }\nfn work(value: Pair) {}\nfn main() { let value = Pair { left: 1, right: 2 }\n spawn(|| work(value)) }",
        ),
        (
            "fixed array goroutine argument",
            "fn work(value: [i64; 2]) {}\nfn main() { let value = [1, 2]\n spawn(|| work(value)) }",
        ),
        (
            "tuple containing Vec argument",
            "fn work(value: (Vec<i64>, i64)) {}\nfn main() { let value = (Vec::from([1, 2]), 2)\n spawn(|| work(value)) }",
        ),
    ] {
        assert_rejected(name, source, ExpectedError::ConcurrentInlineAggregate);
    }
}

#[test]
fn a_channel_takes_an_aggregate_carrying_growable_storage() {
    // The send mints the channel's share of the element's growable storage, a
    // receiver gives it back, and a value nobody receives is reclaimed at
    // teardown through the element's ownership descriptor - so the element
    // needs no restriction on its shape.
    for (name, source) in [
        (
            "struct containing Vec",
            "struct Wrapped { values: Vec<i64> }\nfn main() { let value = Wrapped { values: Vec::from([1, 2]) }\n let tx, rx = channel::<Wrapped>(1)\n tx.send(value) }",
        ),
        (
            "struct containing String",
            "struct Named { name: String }\nfn main() { let value = Named { name: \"a\" }\n let tx, rx = channel::<Named>(1)\n tx.send(value) }",
        ),
    ] {
        assert_accepted(name, source);
    }
}
