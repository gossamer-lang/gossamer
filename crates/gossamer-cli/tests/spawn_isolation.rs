#![allow(missing_docs)]

//! A goroutine never reaches the state of the code that spawned it: a
//! spawned closure's writes go to its own snapshots, and a callable that
//! carries its captures into a goroutine is accepted only where nothing
//! writes them.

mod common;

use common::{TIERS, gos_check_str, gos_run_on, stderr, stdout};

fn everywhere(src: &str, expected: &str) {
    for tier in TIERS {
        let out = gos_run_on(tier, src, None, &[]);
        assert!(out.status.success(), "{tier:?} failed:\n{}", stderr(&out));
        assert_eq!(stdout(&out), expected, "{tier:?} printed otherwise");
    }
}

fn refused(src: &str, code: &str, names: &[&str]) {
    let out = gos_check_str(src);
    assert!(!out.status.success(), "accepted:\n{src}");
    let err = stderr(&out);
    assert!(err.contains(code), "expected {code}:\n{err}");
    for name in names {
        assert!(
            err.contains(&format!("`{name}`")),
            "expected `{name}`:\n{err}"
        );
    }
}

const RUN: &str = "fn run(job: Fn() -> i64) -> i64 {\n    let mut out = 0\n    let _ = cohort {\n        let h = spawn(job)\n        out = h.join().unwrap()\n    }\n    out\n}\n";

#[test]
fn a_bound_closure_that_shares_a_written_binding_is_refused() {
    refused(
        "fn main() {\n    let mut n = 0\n    let f = || { n += 1; n }\n    let h = spawn(f)\n    println(f\"{h.join()} {n}\")\n}\n",
        "GT0118",
        &["f", "n"],
    );
}

#[test]
fn a_spawned_closure_calling_a_sharing_closure_is_refused() {
    refused(
        "fn main() {\n    let mut n = 0\n    let f = || { n += 1; n }\n    let h = spawn(|| f())\n    println(f\"{h.join()} {n}\")\n}\n",
        "GT0118",
        &["f", "n"],
    );
}

#[test]
fn a_closure_read_by_a_goroutine_while_the_parent_writes_is_refused() {
    refused(
        "fn main() {\n    let mut n = 0\n    let f = || n * 2\n    let h = spawn(f)\n    n += 5\n    println(f\"{h.join()} {n}\")\n}\n",
        "GT0118",
        &["f", "n"],
    );
}

#[test]
fn a_closure_captured_through_another_closure_is_followed() {
    refused(
        "fn main() {\n    let mut n = 0\n    let f = || { n += 1; n }\n    let g = || f() + 1\n    let h = spawn(g)\n    println(f\"{h.join()} {n}\")\n}\n",
        "GT0118",
        &["f", "n"],
    );
}

#[test]
fn a_callable_whose_captures_are_not_visible_is_refused() {
    refused(
        "fn make() -> Fn() -> i64 {\n    let mut n = 0\n    || { n += 1; n }\n}\nfn main() {\n    let f = make()\n    let a = spawn(f)\n    let b = spawn(make())\n    println(f\"{a.join()} {b.join()}\")\n}\n",
        "GT0118",
        &["f", "make(..)"],
    );
}

#[test]
fn a_closure_reached_through_a_struct_field_is_refused() {
    refused(
        "struct Job { run: Fn() -> i64 }\nfn main() {\n    let mut n = 0\n    let job = Job { run: || { n += 1; n } }\n    let h = spawn(|| (job.run)())\n    println(f\"{h.join()} {n}\")\n}\n",
        "GT0118",
        &["job"],
    );
}

#[test]
fn a_function_spawning_its_parameter_passes_the_check_to_its_callers() {
    refused(
        &format!(
            "{RUN}fn twice(job: Fn() -> i64) -> i64 {{ run(job) * 2 }}\nfn main() {{\n    let mut n = 0\n    println(f\"{{twice(|| {{ n += 1; n }})}} {{n}}\")\n}}\n"
        ),
        "GT0118",
        &["n"],
    );
    refused(
        "use std::sync\nfn main() {\n    let mut n = 0\n    println(\"{:?} {}\", sync::with_timeout(|| { n += 1; n }, 1000), n)\n}\n",
        "GT0118",
        &["n"],
    );
}

#[test]
fn a_spawned_mut_self_call_is_a_snapshot_write_whatever_its_name() {
    refused(
        "struct Counter { n: i64 }\nimpl Counter { fn bump(&mut self) { self.n += 1 } }\nfn main() {\n    let mut c = Counter { n: 0 }\n    let h = spawn(|| { c.bump(); c.n })\n    println(f\"{h.join()} {c.n}\")\n}\n",
        "GT0114",
        &["c"],
    );
    refused(
        "trait Tick { fn tick(&mut self) }\nstruct Clock { t: i64 }\nimpl Tick for Clock { fn tick(&mut self) { self.t += 1 } }\nfn main() {\n    let mut c = Clock { t: 0 }\n    let h = spawn(|| { c.tick(); c.t })\n    println(f\"{h.join()} {c.t}\")\n}\n",
        "GT0114",
        &["c"],
    );
    refused(
        "struct Counter { n: i64 }\nimpl Counter { fn bump(&mut self) { self.n += 1 } }\nstruct Pair { a: Counter, b: Counter }\nfn main() {\n    let mut p = Pair { a: Counter { n: 0 }, b: Counter { n: 0 } }\n    let h = spawn(|| { p.b.bump(); p.b.n })\n    println(f\"{h.join()} {p.b.n}\")\n}\n",
        "GT0114",
        &["p"],
    );
}

#[test]
fn callables_that_share_nothing_cross_to_goroutines_on_every_tier() {
    everywhere(
        &format!(
            "use std::sync\nstruct Tally {{ n: i64 }}\nimpl Tally {{ fn push(&self, x: i64) -> i64 {{ self.n + x }} }}\nfn work() -> i64 {{ 41 }}\n{RUN}fn twice(job: Fn() -> i64) -> i64 {{ run(job) * 2 }}\nfn main() {{\n    let base = 10\n    let t = Tally {{ n: 5 }}\n    let f = || base + 1\n    let g = || f() * 2\n    let h1 = spawn(f)\n    let h2 = spawn(g)\n    let h3 = spawn(|| g() + t.push(1))\n    let h4 = spawn(work)\n    let h5 = spawn(f)\n    println(f\"{{h1.join()}} {{h2.join()}} {{h3.join()}} {{h4.join()}} {{h5.join()}}\")\n    println(f\"{{run(|| base * 2)}} {{twice(|| base)}}\")\n    println(\"{{:?}}\", sync::with_timeout(|| base + 1, 1000))\n}}\n"
        ),
        "Ok(11) Ok(22) Ok(28) Ok(41) Ok(11)\n20 20\nOk(11)\n",
    );
}
