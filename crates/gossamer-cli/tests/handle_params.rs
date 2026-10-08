#![allow(missing_docs)]

//! A runtime handle passed to a function by parameter carries no
//! construction site, so its methods dispatch from its declared type: a
//! `bytes::Buffer`, a `bytes::Builder`, and a `regex::Pattern` read out of a
//! map each reach their own runtime calls on every tier, and the buffers are
//! freed with their last holder.

mod common;

use common::{TIERS, gos_run_on, stderr, stdout};

const PROGRAM: &str = r#"
use std::{bytes, regex}

fn fill(mut b: bytes::Buffer, n: i64) -> i64 {
    for i in 0..n { b.push(i as u8) }
    b.len()
}

fn grow(mut b: bytes::Builder) -> i64 {
    b.write("ab")
    b.len()
}

fn show(p: regex::Pattern) -> bool { p.is_match("abc") }

fn main() {
    let b = bytes::Buffer::new()
    let w = bytes::Builder::new()
    let mut pats = Map::new()
    pats.insert("p", regex::compile("b"))
    let p = pats.get("p").unwrap()
    println(f"{fill(b, 3)} {grow(w)} {show(p)}")
}
"#;

#[test]
fn handle_parameters_dispatch_from_their_declared_type() {
    for tier in TIERS {
        let out = gos_run_on(tier, PROGRAM, None, &[]);
        assert!(out.status.success(), "{tier:?}:\n{}", stderr(&out));
        assert_eq!(stdout(&out), "3 2 true\n", "{tier:?}");
    }
}
