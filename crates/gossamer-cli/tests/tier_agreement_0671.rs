#![allow(missing_docs)]

//! Programs whose answer once differed between the bytecode VM, the JIT,
//! and a native build, or that one tier refused or lost: each prints the
//! same on all three.

mod common;

use common::{TIERS, gos_check_str, gos_run_on, stderr, stdout};

fn everywhere(src: &str, expected: &str) {
    for tier in TIERS {
        let out = gos_run_on(tier, src, None, &[]);
        assert!(out.status.success(), "{tier:?} failed:\n{}", stderr(&out));
        assert_eq!(stdout(&out), expected, "{tier:?} printed otherwise");
    }
}

/// A program that runs `work(1)` in a spawned goroutine, where `body` is
/// the body of `work(n: i64) -> i64`, and prints whether the join saw a
/// fault.
fn goroutine_fault(body: &str) -> String {
    format!(
        r#"use std::errors

struct Boom {{ n: i64 }}

impl Display for Boom {{
    fn fmt(&self) -> String {{ f"{{10 / self.n}}" }}
}}

fn work(n: i64) -> i64 {{
    {body}
}}

fn run() -> Result<(), errors::Error> {{
    cohort {{
        let h = spawn(|| work(1))
        println(f"join err: {{h.join().is_err()}}")
    }}
}}

fn main() {{
    println(f"cohort err: {{run().is_err()}}")
}}
"#
    )
}

#[test]
fn a_runtime_fault_in_a_goroutine_reaches_its_join() {
    for body in [
        "let xs = #[1]; xs[n + 4]",
        "let s = \"abc\"; s.repeat(n - 5).len()",
        "let m = {\"a\": 1}; m[\"b\"] + n",
        "let v = #[1, 2].map(|x| x / (x - n)); v.len()",
        "let v = (1..4).map(|x| x / (x - n)).sum(); v",
        "let v = #[1, 2, 3].iter().map(|x| x / (x - n)).collect::<Vec<i64>>(); v.len()",
        "let v = #[1, 2, 3].fold(0, |a, x| a + x / (x - n)); v",
        "let mut v = #[3, 2, 1]; v.sort_by_key(|x| 10 / (x - n)); v.len()",
        "let b = #[Boom { n: 0 }]; let s = f\"{b}\"; s.len() + n",
        "let v: Vec<i64> = Vec::with_capacity(-n); v.len()",
        "let s = \"abc\"; let c = s[n + 7]; 0",
        "let mut a = 0; for x in 1..3 { a += 10 / (x - n) }; a",
    ] {
        everywhere(&goroutine_fault(body), "join err: true\ncohort err: true\n");
    }
}

#[test]
fn a_goroutine_body_that_faults_runs_once() {
    everywhere(
        &goroutine_fault(
            "let mut a = 0; for x in 1..3 { println(f\"side {x}\"); a += 10 / (x - n) }; a",
        ),
        "side 1\njoin err: true\ncohort err: true\n",
    );
}

#[test]
fn writes_through_a_map_entry_land_in_the_map() {
    everywhere(
        r#"struct P { hp: i64, tags: Vec<i64> }

fn bump(x: &mut i64, by: i64) { *x += by }
fn grow(v: &mut Vec<i64>) { v.push(9) }

fn main() {
    let mut a = {"k": P { hp: 1, tags: #[] }}
    a["k"].hp += 10
    a["k"].tags.push(5)
    let mut b = {"k": #[1, 2]}
    b["k"][0] = 9
    b["k"][1] += 5
    let mut c = {"x": {"y": 1}}
    c["x"]["y"] = 3
    c["x"]["z"] = 4
    c["x"]["y"] += c["x"]["y"]
    let mut d = {"k": #[P { hp: 5, tags: #[] }]}
    d["k"][0].hp = 9
    d["k"][0].tags.push(1)
    bump(&mut d["k"][0].hp, d["k"][0].hp)
    let mut e = {"v": #[1]}
    grow(&mut e["v"])
    e["v"][0] = e["v"][1]
    println(f"{a} | {b} | {c} | {d} | {e}")
}
"#,
        "{\"k\": P { hp: 11, tags: #[5] }} | {\"k\": #[9, 7]} | \
         {\"x\": {\"y\": 6, \"z\": 4}} | {\"k\": #[P { hp: 18, tags: #[1] }]} | \
         {\"v\": #[9, 9]}\n",
    );
}

#[test]
fn a_write_through_a_missing_map_key_panics() {
    for tier in TIERS {
        let out = gos_run_on(
            tier,
            "fn main() {\n    let mut m = {\"a\": #[1]}\n    m[\"b\"][0] = 2\n}\n",
            None,
            &[],
        );
        assert_eq!(out.status.code(), Some(101), "{tier:?}:\n{}", stderr(&out));
        assert!(
            stderr(&out).contains("is not in the map"),
            "{tier:?}:\n{}",
            stderr(&out)
        );
    }
}

#[test]
fn two_enums_share_a_variant_name() {
    everywhere(
        r#"enum Conn { Ready(String), Error, Idle }
enum Job { Pending, Ready(i64), Error { code: i64 } }

fn d(c: Conn) -> String {
    match c {
        Conn::Ready(s) => f"conn {s}",
        Conn::Error => "conn err",
        Idle => "idle",
    }
}

fn j(x: Job) -> String {
    match x {
        Job::Pending => "pending",
        Job::Ready(n) => f"job {n}",
        Job::Error { code } => f"job err {code}",
    }
}

fn main() {
    for c in #[Conn::Ready("a"), Conn::Error, Conn::Idle] { println(d(c)) }
    for x in #[Job::Ready(3), Job::Error { code: 7 }, Job::Pending] { println(j(x)) }
    println(f"{Job::Ready(1) == Job::Ready(1)} {Conn::Error == Conn::Error}")
}
"#,
        "conn a\nconn err\nidle\njob 3\njob err 7\npending\ntrue true\n",
    );
}

#[test]
fn method_arguments_bind_against_the_receivers_declaration() {
    everywhere(
        r#"struct Socket { h: String }
struct Database { u: String }

impl Socket {
    fn connect(&self, host: String, timeout: i64 = 30) -> String { f"{self.h} {host} {timeout}" }
}

impl Database {
    fn connect(&self, url: String, retries: i64 = 3) -> String { f"{self.u} {url} {retries}" }
}

fn main() {
    let s = Socket { h: "s" }
    let d = Database { u: "d" }
    println(s.connect("a"))
    println(s.connect(host: "b", timeout: 5))
    println(d.connect(url: "c"))
    println(d.connect("e", retries: 1))
}
"#,
        "s a 30\ns b 5\nd c 3\nd e 1\n",
    );
}

#[test]
fn a_contended_mutex_serialises_its_goroutines() {
    everywhere(
        r#"use std::sync::Mutex

fn main() {
    let m = Mutex::new()
    let mut total = #[0]
    let _ = cohort {
        for _ in 0..4 {
            spawn(|| {
                for _ in 0..5000 {
                    m.lock()
                    let v = total[0]
                    total[0] = v + 1
                    m.unlock()
                }
            })
        }
    }
    println(f"{total[0]}")
}
"#,
        "20000\n",
    );
}

#[test]
fn a_lock_nothing_can_release_is_a_deadlock() {
    let src = r#"use std::errors
use std::sync::Mutex

fn holds_and_faults(m: Mutex) -> i64 {
    m.lock()
    panic("boom")
}

fn run() -> Result<(), errors::Error> {
    let m = Mutex::new()
    let _ = cohort {
        let h = spawn(|| holds_and_faults(m))
        println(f"first: {h.join().is_err()}")
    }
    cohort {
        let h = spawn(|| { m.lock(); m.unlock(); 7 })
        println(f"second: {h.join()}")
    }
}

fn main() { println(f"{run()}") }
"#;
    for tier in TIERS {
        let out = gos_run_on(tier, src, None, &[]);
        assert_eq!(out.status.code(), Some(101), "{tier:?}:\n{}", stderr(&out));
        assert!(
            stderr(&out).contains("all goroutines are asleep - deadlock!"),
            "{tier:?}:\n{}",
            stderr(&out)
        );
    }
}

#[test]
fn a_closure_cannot_write_a_value_it_captured_by_copy() {
    for (src, name) in [
        (
            "fn main() {\n    let mut count = 0\n    #[1, 2].for_each(|x| count += x)\n    println(f\"{count}\")\n}\n",
            "count",
        ),
        (
            "struct Bag { items: Vec<i64> }\nfn call(f: Fn()) { f() }\nfn main() {\n    let mut b = Bag { items: #[1] }\n    call(|| b.items.push(2))\n    println(f\"{b.items}\")\n}\n",
            "b",
        ),
        (
            "fn main() {\n    let mut s = \"a\"\n    let f = || s.push_str(\"b\")\n    f()\n}\n",
            "s",
        ),
    ] {
        let out = gos_check_str(src);
        assert!(!out.status.success(), "accepted:\n{src}");
        let err = stderr(&out);
        assert!(
            err.contains("GT0114") && err.contains(&format!("`{name}`")),
            "{err}"
        );
    }
}

#[test]
fn a_closure_still_writes_a_container_it_captured() {
    everywhere(
        "fn main() {\n    let mut seen = #[]\n    #[3, 1].for_each(|x| seen.push(x * 10))\n    println(f\"{seen}\")\n}\n",
        "#[30, 10]\n",
    );
}

#[test]
fn an_entry_mutation_keeps_its_key_when_the_key_is_read_once() {
    everywhere(
        r#"fn main() {
    let mut records: Map<String, Vec<i64>> = Map::new()
    for line in #["a 1", "b 2", "a 3"] {
        let parts = line.split(" ")
        let label = parts[0]
        let ns = parts[1].to_i64().unwrap_or(0)
        records.or_insert(label, #[]).push(ns)
    }
    let mut xs = #[#[0], #[0]]
    for i in 0..2 {
        let k = i
        xs[k].push(k)
    }
    println(f"{records} {xs}")
}
"#,
        "{\"a\": #[1, 3], \"b\": #[2]} #[#[0, 0], #[0, 1]]\n",
    );
}
