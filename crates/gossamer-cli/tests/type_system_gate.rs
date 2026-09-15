//! Adversarial gate on the strength of the type system.
//!
//! Every case here is a program that a strongly-typed language must reject,
//! paired with the diagnostic code it must be rejected by, plus a control
//! that must still be accepted so the gate cannot be satisfied by rejecting
//! everything. The suite runs the same authoritative front-end `gos check`
//! runs, in-process, so it stays fast enough to gate every commit.
//!
//! Adding a case is the way to record a type-system guarantee. A case that
//! starts passing for the wrong reason (a rejection that moves to a
//! different code) fails loudly rather than silently weakening the gate.

use gossamer_driver::check_frontend;
use gossamer_lex::SourceMap;

/// What the front-end must do with a program.
#[derive(Debug, Clone, Copy)]
enum Expect {
    /// The program is well-typed.
    Accept,
    /// The program is rejected, carrying this diagnostic code.
    Reject(&'static str),
}

/// Runs the authoritative front-end and returns the codes it reported.
///
/// `check_frontend` takes augmented source, so the program is augmented first
/// exactly as `gos check` augments it; the stdlib wrappers a spelling is
/// rewritten to exist only in the augmented text.
fn codes(source: &str) -> Vec<String> {
    let augmented = gossamer_parse::autoderive::augment_source(source);
    let mut map = SourceMap::new();
    let file = map.add_file("type_system_gate.gos".to_string(), augmented);
    check_frontend(map.source(file), file)
        .diagnostics
        .iter()
        .map(|d| d.code.as_str().to_string())
        .collect()
}

/// Asserts every case in a dimension, reporting all failures at once so a
/// regression shows its whole blast radius rather than the first case only.
fn gate(dimension: &str, cases: &[(&str, &str, Expect)]) {
    let mut failures = Vec::new();
    for (name, source, expect) in cases {
        let got = codes(source);
        let ok = match expect {
            Expect::Accept => got.is_empty(),
            Expect::Reject(code) => got.iter().any(|c| c == code),
        };
        if !ok {
            failures.push(format!("  {name}: expected {expect:?}, got {got:?}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{dimension} guarantees regressed:\n{}",
        failures.join("\n")
    );
}

#[test]
fn function_arguments_are_checked_by_type_and_arity() {
    gate(
        "function argument",
        &[
            (
                "wrong argument type",
                "fn f(x: i64) -> i64 { x }\nfn main() { println(\"{}\", f(\"s\")) }\n",
                Expect::Reject("GT0001"),
            ),
            (
                "too few arguments",
                "fn f(a: i64, b: i64) -> i64 { a + b }\nfn main() { println(\"{}\", f(1)) }\n",
                Expect::Reject("GT0018"),
            ),
            (
                "wrong return type",
                "fn f() -> i64 { \"s\" }\nfn main() { println(\"{}\", f()) }\n",
                Expect::Reject("GT0001"),
            ),
            (
                "calling a non-callable",
                "fn main() { let a = 5\n println(\"{}\", a(1)) }\n",
                Expect::Reject("GT0022"),
            ),
            (
                "matching argument is accepted",
                "fn f(x: i64) -> i64 { x }\nfn main() { println(\"{}\", f(1)) }\n",
                Expect::Accept,
            ),
        ],
    );
}

#[test]
fn numeric_conversions_are_never_implicit() {
    // Width and representation changes are written, never inferred: an
    // implicit widening here is what lets a value silently change meaning.
    gate(
        "numeric conversion",
        &[
            (
                "int is not a float",
                "fn f(x: f64) -> f64 { x }\nfn main() { println(\"{}\", f(1)) }\n",
                Expect::Reject("GT0001"),
            ),
            (
                "float is not an int",
                "fn f(x: i64) -> i64 { x }\nfn main() { println(\"{}\", f(1.5)) }\n",
                Expect::Reject("GT0001"),
            ),
            (
                "narrower int does not widen",
                "fn f(x: i64) -> i64 { x }\nfn main() { let a: i32 = 1\n println(\"{}\", f(a)) }\n",
                Expect::Reject("GT0001"),
            ),
            (
                "int is not a bool",
                "fn main() { let b: bool = 1\n println(\"{}\", b) }\n",
                Expect::Reject("GT0001"),
            ),
            (
                "int is not a String",
                "fn main() { let s: String = 5\n println(\"{}\", s) }\n",
                Expect::Reject("GT0001"),
            ),
            (
                "a written cast is accepted",
                "fn f(x: f64) -> f64 { x }\nfn main() { println(\"{}\", f(1 as f64)) }\n",
                Expect::Accept,
            ),
        ],
    );
}

#[test]
fn a_trait_impl_names_a_contract_the_language_dispatches_through() {
    gate(
        "trait impl",
        &[
            (
                "an operator impl supplies the operator",
                "struct M { b: i64 }\nimpl Not for M { fn not(&self) -> M { M { b: 0 - self.b } } }\nfn main() { println(\"{}\", (!M { b: 1 }).b) }\n",
                Expect::Accept,
            ),
            (
                "hashing is the language's, not the type's",
                "struct P { x: i64 }\nimpl Hash for P { fn hash(&self) -> i64 { self.x } }\nfn main() { println(\"{}\", P { x: 1 }) }\n",
                Expect::Reject("GT0084"),
            ),
            (
                "there is no destructor hook to implement",
                "struct P { x: i64 }\nimpl Drop for P { fn drop(&mut self) { println(\"gone\") } }\nfn main() { println(\"{}\", P { x: 1 }) }\n",
                Expect::Reject("GT0084"),
            ),
            (
                "the conversion is written on the target's `From`",
                "struct P { x: i64 }\nimpl Into<i64> for P { fn into(&self) -> i64 { self.x } }\nfn main() { println(\"{}\", P { x: 1 }) }\n",
                Expect::Reject("GT0084"),
            ),
            (
                "a trait nothing declares is still unknown",
                "struct P { x: i64 }\nimpl Bogus for P { fn go(&self) -> i64 { self.x } }\nfn main() { println(\"{}\", P { x: 1 }) }\n",
                Expect::Reject("GT0070"),
            ),
        ],
    );
}

#[test]
fn a_written_cmp_reaches_the_sites_that_can_read_it() {
    let ordered = "struct P { x: i64 }\nimpl Ord for P { fn cmp(&self, other: P) -> i64 { other.x - self.x } }\n";
    gate(
        "user ordering",
        &[
            (
                "a sequence orders on demand, through the type's own cmp",
                &format!(
                    "{ordered}fn main() {{ let mut v = #[P {{ x: 1 }}]\n v.sort()\n println(\"{{}}\", v.min()) }}\n"
                ),
                Expect::Accept,
            ),
            (
                "a heap orders as it stores, with no comparator to call",
                &format!(
                    "use std::collections::MinHeap\n{ordered}fn main() {{ let h = MinHeap::from(#[P {{ x: 1 }}])\n println(\"{{}}\", h.len()) }}\n"
                ),
                Expect::Reject("GT0085"),
            ),
            (
                "a sorted set orders as it stores",
                &format!(
                    "use std::collections::BTreeSet\n{ordered}fn main() {{ let s: BTreeSet<P> = BTreeSet::from([P {{ x: 1 }}])\n println(\"{{}}\", s.len()) }}\n"
                ),
                Expect::Reject("GT0085"),
            ),
            (
                "a type with no written cmp keeps every container",
                "use std::collections::MinHeap\nstruct Q { x: i64 }\nfn main() { let h = MinHeap::from(#[Q { x: 1 }])\n println(\"{}\", h.len()) }\n",
                Expect::Accept,
            ),
        ],
    );
}

/// A `spawn` attaches its child to the cohort the goroutine is inside at that
/// moment. A function is not a cohort, so one that spawns without opening a
/// `cohort { }` hands its children to whatever cohort its caller happens to be
/// in - and to the root cohort, whose extent is the process, when the program
/// has none. `main` is the one exemption: the root cohort's extent IS main's.
#[test]
fn a_spawn_belongs_to_a_cohort_written_around_it() {
    gate(
        "spawn scope",
        &[
            (
                "a function that spawns and opens no cohort strands its children",
                "use std::errors\nfn handle(id: i64) -> Result<i64, errors::Error> {\n let h = spawn(|| id)\n let _ = h.join()\n Err(errors::new(\"failed\")) }\nfn main() { let _ = handle(1) }\n",
                Expect::Reject("GT0086"),
            ),
            (
                "a method in an impl block is a function for this purpose",
                "struct Pool { n: i64 }\nimpl Pool { fn start(&self) -> i64 { let h = spawn(|| 1)\n h.join().unwrap_or(0) } }\nfn main() { let p = Pool { n: 1 }\n println(\"{}\", p.start()) }\n",
                Expect::Reject("GT0086"),
            ),
            (
                "a method named main is still a method, not the program's main",
                "struct T { n: i64 }\nimpl T { fn main(&self) -> i64 { let h = spawn(|| 5)\n h.join().unwrap_or(0) } }\nfn main() { let t = T { n: 1 }\n println(\"{}\", t.main()) }\n",
                Expect::Reject("GT0086"),
            ),
            (
                "a closure is not a cohort: it runs where it is written",
                "fn run() -> i64 { let f = || { let h = spawn(|| 7)\n h.join().unwrap_or(0) }\n f() }\nfn main() { println(\"{}\", run()) }\n",
                Expect::Reject("GT0086"),
            ),
            (
                "a cohort in the same body owns the spawn",
                "use std::errors\nfn gather() -> Result<(), errors::Error> {\n cohort {\n let a = spawn(|| 1)\n println(\"{}\", a.join()?)\n } }\nfn main() { let _ = gather() }\n",
                Expect::Accept,
            ),
            (
                "a cohort header changes nothing about the containment",
                "use std::errors\nfn timed() -> Result<(), errors::Error> {\n cohort(timeout: 500) {\n let a = spawn(|| 4, reason: \"unit\")\n println(\"{}\", a.join()?)\n } }\nfn main() { let _ = timed() }\n",
                Expect::Accept,
            ),
            (
                "a closure inside a cohort block is inside that block",
                "use std::errors\nfn work() -> Result<(), errors::Error> {\n cohort {\n let start = || spawn(|| 3)\n let h = start()\n println(\"{}\", h.join()?)\n } }\nfn main() { let _ = work() }\n",
                Expect::Accept,
            ),
            (
                "a nested cohort contains its own spawn and the outer one's",
                "use std::errors\nfn outer() -> Result<(), errors::Error> {\n cohort {\n let a = spawn(|| 1)\n cohort {\n let b = spawn(|| 2)\n println(\"{}\", b.join()?)\n }?\n println(\"{}\", a.join()?)\n } }\nfn main() { let _ = outer() }\n",
                Expect::Accept,
            ),
            (
                "main is the exemption: the root cohort's extent is its own",
                "fn main() { let h = spawn(|| 9)\n println(\"{}\", h.join().unwrap_or(0)) }\n",
                Expect::Accept,
            ),
            (
                "an entry file's top-level statements are that same main",
                "let h = spawn(|| 11)\nprintln(\"{}\", h.join().unwrap_or(0))\n",
                Expect::Accept,
            ),
            (
                "a nested fn inside main is its own body, not main's",
                "fn main() { fn helper() -> i64 { let h = spawn(|| 3)\n h.join().unwrap_or(0) }\n println(\"{}\", helper()) }\n",
                Expect::Reject("GT0086"),
            ),
            (
                "a trait default body is a function too",
                "trait Starter { fn go(&self) -> i64 { let h = spawn(|| 2)\n h.join().unwrap_or(0) } }\nstruct S { n: i64 }\nimpl Starter for S {}\nfn main() { let s = S { n: 1 }\n println(\"{}\", s.go()) }\n",
                Expect::Reject("GT0086"),
            ),
            (
                "a module's own spawn is a different function",
                "use std::process\nfn run_it() -> i64 { match process::run(\"echo\", #[\"hi\"]) { Ok(r) => r.code\n Err(_) => -1 } }\nfn main() { println(\"{}\", run_it()) }\n",
                Expect::Accept,
            ),
            (
                "an arena inside a cohort does not break containment",
                "use std::errors\nfn work() -> Result<(), errors::Error> {\n cohort {\n arena {\n let h = spawn(|| 5)\n println(\"{}\", h.join()?)\n }\n } }\nfn main() { let _ = work() }\n",
                Expect::Accept,
            ),
            (
                "a function that spawns nothing needs no cohort",
                "fn plain(n: i64) -> i64 { n * 2 }\nfn main() { println(\"{}\", plain(4)) }\n",
                Expect::Accept,
            ),
        ],
    );
}

#[test]
fn references_distinguish_shared_from_mutable() {
    gate(
        "reference",
        &[
            (
                "a value argument does not satisfy a mutable parameter",
                "fn bump(x: &mut i64) { *x += 1 }\nfn main() { let mut a = 1\n bump(a)\n println(\"{}\", a) }\n",
                Expect::Reject("GT0046"),
            ),
            (
                "no write through a value",
                "fn w(x: i64) { *x = 5 }\nfn main() { let mut a = 1\n w(a)\n println(\"{}\", a) }\n",
                Expect::Reject("GT0083"),
            ),
            (
                "no assignment to an immutable binding",
                "fn main() { let a = 1\n a = 2\n println(\"{}\", a) }\n",
                Expect::Reject("GT0030"),
            ),
            (
                "no field write through an immutable binding",
                "struct P { x: i64 }\nfn main() { let p = P { x: 1 }\n p.x = 2\n println(\"{}\", p.x) }\n",
                Expect::Reject("GT0030"),
            ),
            (
                "a mutable reference is accepted",
                "fn bump(x: &mut i64) { *x += 1 }\nfn main() { let mut a = 1\n bump(&mut a)\n println(\"{}\", a) }\n",
                Expect::Accept,
            ),
        ],
    );
}

#[test]
fn collections_keep_their_element_and_key_types() {
    gate(
        "collection",
        &[
            (
                "Vec element type is enforced",
                "fn main() { let v: Vec<i64> = #[\"a\"]\n println(\"{}\", v.len()) }\n",
                Expect::Reject("GT0001"),
            ),
            (
                "Vec element type is enforced across a call",
                "fn f(v: Vec<String>) -> i64 { v.len() }\nfn main() { println(\"{}\", f(#[1,2])) }\n",
                Expect::Reject("GT0001"),
            ),
            (
                "Map value type is enforced",
                "fn main() { let m: Map<String, i64> = {\"a\": \"b\"}\n println(\"{}\", m.len()) }\n",
                Expect::Reject("GT0001"),
            ),
            (
                "a Set is not a Vec",
                "fn f(v: Vec<i64>) -> i64 { v.len() }\nfn main() { println(\"{}\", f(#{1,2})) }\n",
                Expect::Reject("GT0001"),
            ),
            (
                "indexing a non-indexable value",
                "fn main() { let a = 5\n println(\"{}\", a[0]) }\n",
                Expect::Reject("GT0021"),
            ),
            (
                "a matching Vec is accepted",
                "fn f(v: Vec<i64>) -> i64 { v.len() }\nfn main() { println(\"{}\", f(#[1,2])) }\n",
                Expect::Accept,
            ),
        ],
    );
}

#[test]
fn aggregates_keep_their_declared_shape() {
    gate(
        "aggregate",
        &[
            (
                "tuple element types are enforced",
                "fn main() { let t: (i64, String) = (1, 2)\n println(\"{}\", t.0) }\n",
                Expect::Reject("GT0001"),
            ),
            (
                "enum payload type is enforced",
                "enum E { A(i64) }\nfn main() { let e = E::A(\"s\")\n println(\"ok\") }\n",
                Expect::Reject("GT0001"),
            ),
            (
                "an Option is not its payload",
                "fn f() -> Option<i64> { Some(1) }\nfn main() { let x: i64 = f()\n println(\"{}\", x) }\n",
                Expect::Reject("GT0001"),
            ),
            (
                "unknown struct field",
                "struct P { x: i64 }\nfn main() { let p = P { x: 1 }\n println(\"{}\", p.z) }\n",
                Expect::Reject("GT0006"),
            ),
            (
                "a matching payload is accepted",
                "enum E { A(i64) }\nfn main() { let e = E::A(1)\n println(\"ok\") }\n",
                Expect::Accept,
            ),
        ],
    );
}

#[test]
fn trait_bounds_are_authoritative() {
    // A type parameter stands for every type a caller may supply, so its
    // bounds are the whole of what it can do. Anything looser lets a method
    // bind an unrelated type's body and read the receiver at that layout.
    gate(
        "trait bound",
        &[
            (
                "unsatisfied bound is rejected at the call site",
                "trait Sh { fn a(&self) -> i64 }\nstruct R {}\nfn apply<T: Sh>(x: T) -> i64 { x.a() }\nfn main() { println(\"{}\", apply(R{})) }\n",
                Expect::Reject("GT0017"),
            ),
            (
                "a method no bound declares is rejected",
                "trait Sh { fn a(&self) -> i64 }\nstruct R {}\nimpl Sh for R { fn a(&self) -> i64 { 1 } }\nfn apply<T: Sh>(x: T) -> i64 { x.zzz() }\nfn main() { println(\"{}\", apply(R{})) }\n",
                Expect::Reject("GT0056"),
            ),
            (
                "an unbounded parameter has no methods",
                "struct R {}\nimpl R { fn a(&self) -> i64 { 1 } }\nfn apply<T>(x: T) -> i64 { x.a() }\nfn main() { println(\"{}\", apply(R{})) }\n",
                Expect::Reject("GT0056"),
            ),
            (
                "a bound naming no trait is rejected",
                "fn apply<T: Nope>(x: T) -> i64 { 1 }\nfn main() { println(\"{}\", apply(1)) }\n",
                Expect::Reject("GT0011"),
            ),
            (
                "iteration requires a bound that provides it",
                "fn total<T: Clone>(it: T) -> i64 { let mut s = 0\n for x in it { s += x }\n s }\nfn main() { println(\"{}\", total(0..5)) }\n",
                Expect::Reject("GT0056"),
            ),
            (
                "a built-in iterator cannot instantiate an iteration bound",
                "fn total<T: Iterator>(it: T) -> i64 { let mut s = 0\n for x in it { s += x }\n s }\nfn main() { println(\"{}\", total(0..5)) }\n",
                Expect::Reject("GT0057"),
            ),
            (
                "naming the iterator on the parameter is accepted",
                "fn total(it: Iterator<i64>) -> i64 { let mut s = 0\n for x in it { s += x }\n s }\nfn main() { println(\"{}\", total(0..5)) }\n",
                Expect::Accept,
            ),
            (
                "a bound-provided method is accepted",
                "trait Sh { fn a(&self) -> i64 }\nstruct R {}\nimpl Sh for R { fn a(&self) -> i64 { 1 } }\nfn apply<T: Sh>(x: T) -> i64 { x.a() }\nfn main() { println(\"{}\", apply(R{})) }\n",
                Expect::Accept,
            ),
        ],
    );
}

#[test]
fn declared_traits_are_checked_even_when_named_like_a_builtin() {
    // Bound checking keys on the declaration, not the spelling, so a trait
    // that happens to share a built-in name keeps its guarantee.
    gate(
        "builtin-named trait",
        &[
            (
                "a user trait named Ord still constrains",
                "trait Ord { fn cmpx(&self) -> i64 }\nstruct D {}\nimpl Ord for D { fn cmpx(&self) -> i64 { 1 } }\nstruct R {}\nfn apply<T: Ord>(x: T) -> i64 { x.cmpx() }\nfn main() { println(\"{}\", apply(R{})) }\n",
                Expect::Reject("GT0017"),
            ),
            (
                "the implementing type is accepted",
                "trait Ord { fn cmpx(&self) -> i64 }\nstruct D {}\nimpl Ord for D { fn cmpx(&self) -> i64 { 1 } }\nfn apply<T: Ord>(x: T) -> i64 { x.cmpx() }\nfn main() { println(\"{}\", apply(D{})) }\n",
                Expect::Accept,
            ),
        ],
    );
}

#[test]
fn names_without_a_type_behind_them_are_rejected() {
    // A name in scope that binds to no type would accept any value and
    // defer the failure to run time.
    // A scalar primitive's associated surface is entirely the standard
    // library's, so a member it does not declare is definitively unresolved
    // - reported at check time, never as a runtime GX0002.
    gate(
        "phantom associated path on a primitive",
        &[
            (
                "an undeclared f64 associated function is rejected",
                "fn main() { println(\"{}\", f64::nonexistent(1.0)) }\n",
                Expect::Reject("GR0001"),
            ),
            (
                "an undeclared bool associated function is rejected",
                "fn main() { println(\"{}\", bool::nonexistent(1)) }\n",
                Expect::Reject("GR0001"),
            ),
            (
                "an undeclared char associated function is rejected",
                "fn main() { println(\"{}\", char::nonexistent(1)) }\n",
                Expect::Reject("GR0001"),
            ),
            (
                "the IEEE-754 reinterpretations resolve",
                "fn main() { println(\"{}\", f64::from_bits(f64::to_bits(1.5))) }\n",
                Expect::Accept,
            ),
        ],
    );
    gate(
        "phantom type name",
        &[
            (
                "an undeclared type name is rejected",
                "fn main() { let x: Nonexistent = 5\n println(\"{}\", x) }\n",
                Expect::Reject("GR0001"),
            ),
            (
                "a range converts to the iterator it advances through",
                "fn mk() -> Range<i64> { 0..5 }\nfn take(r: Iterator<i64>) -> i64 { let mut s = 0\n for x in r { s += x }\n s }\nfn main() { println(\"{}\", take(mk())) }\n",
                Expect::Accept,
            ),
            (
                "an iterator does not convert back to a range",
                "fn mk() -> Iterator<i64> { 0..5 }\nfn take(r: Range<i64>) -> i64 { let mut s = 0\n for x in r { s += x }\n s }\nfn main() { println(\"{}\", take(mk())) }\n",
                Expect::Reject("GT0001"),
            ),
            (
                "a range names a real type",
                "fn total(it: Range<i64>) -> i64 { let mut s = 0\n for x in it { s += x }\n s }\nfn main() { println(\"{}\", total(0..5)) }\n",
                Expect::Accept,
            ),
        ],
    );
}

/// Keyword arguments bind with `=`, defaults fill the gaps, and a
/// parameter with neither is named rather than counted.
#[test]
fn keyword_arguments_and_defaults() {
    const DECL: &str = "fn wow(a: i64, b: i64 = 100) -> i64 { a * 2 + b * 10 }\n";
    gate(
        "keyword arguments",
        &[
            (
                "default filled",
                &format!("{DECL}fn main() {{ let _ = wow(9) }}"),
                Expect::Accept,
            ),
            (
                "named argument",
                &format!("{DECL}fn main() {{ let _ = wow(a: 9) }}"),
                Expect::Accept,
            ),
            (
                "named out of order",
                &format!("{DECL}fn main() {{ let _ = wow(b: 1, a: 9) }}"),
                Expect::Accept,
            ),
            (
                "parameter with no default omitted",
                &format!("{DECL}fn main() {{ let _ = wow(b: 0) }}"),
                Expect::Reject("GR0015"),
            ),
            (
                "equals is not a label",
                &format!("{DECL}fn main() {{ let _ = wow(a = 9, b = 1) }}"),
                Expect::Reject("GP0045"),
            ),
            (
                "equality is not a label",
                &format!("{DECL}fn main() {{ let _ = wow(9, 1) == 28 }}"),
                Expect::Accept,
            ),
        ],
    );
}

/// A `for` over a wrapper binds nothing and runs zero times, so accepting
/// it is a program that silently does nothing.
#[test]
fn for_over_a_wrapper_is_rejected() {
    gate(
        "wrapper iteration",
        &[
            (
                "result",
                "use std::fs\nfn main() { for e in fs::read_dir(\".\") { println(\"{}\", e.name) } }",
                Expect::Reject("GT0067"),
            ),
            (
                "option",
                "fn first_of(xs: [i64]) -> Option<i64> { xs.first() }\n\
                 fn main() { for v in first_of(#[1, 2]) { println(\"{}\", v) } }",
                Expect::Reject("GT0067"),
            ),
            (
                "taken first",
                "use std::fs\n\
                 fn main() { for e in fs::read_dir(\".\").unwrap_or(#[]) { println(\"{}\", e.name) } }",
                Expect::Accept,
            ),
        ],
    );
}

/// A synthesized name is not the user's to act on. Every shape that makes
/// the autoderive stage decline must report in the user's own vocabulary,
/// and none may leave a `__gos_` symbol in a message.
#[test]
fn refused_serde_targets_report_without_leaking_a_synthesized_name() {
    let cases = [
        (
            "generic struct",
            "struct W<T> { v: T }\nfn main() { let _ = to_json::<W<i64>>(W { v: 1 }) }",
            "GP0039",
        ),
        (
            "enum",
            "enum E { A(i64), B }\nfn main() { let _ = to_json::<E>(E::A(1)) }",
            "GP0039",
        ),
        (
            "not a struct",
            "fn main() { let _ = to_json::<Nope>(1) }",
            "GP0039",
        ),
        (
            "unserializable field",
            "struct H { cb: Fn(i64) -> i64 }\n\
             fn main() { let _ = to_json::<H>(H { cb: |x: i64| x }) }",
            "GP0022",
        ),
    ];
    let mut failures = Vec::new();
    for (name, source, expected) in cases {
        let augmented = gossamer_parse::autoderive::augment_source(source);
        let mut map = SourceMap::new();
        let file = map.add_file("serde_refusal.gos".to_string(), augmented);
        let result = check_frontend(map.source(file), file);
        let codes: Vec<String> = result
            .diagnostics
            .iter()
            .map(|d| d.code.as_str().to_string())
            .collect();
        if !codes.iter().any(|code| code == expected) {
            failures.push(format!("{name}: expected {expected}, got {codes:?}"));
        }
        for diagnostic in &result.diagnostics {
            let rendered = format!(
                "{} {} {} {}",
                diagnostic.code.as_str(),
                diagnostic.title,
                diagnostic.notes.join(" "),
                diagnostic.helps.join(" "),
            );
            if rendered.contains("__gos_") {
                failures.push(format!("{name}: leaked a synthesized name: {rendered}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
