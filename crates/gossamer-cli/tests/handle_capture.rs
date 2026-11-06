//! A handle the checker types as its contents - a `std::flag` cell or set - reads
//! as the handle inside a closure that captures it, however deeply closures
//! nest, on every tier. A lifted closure reads each capture back from its
//! environment, where only the runtime kind its creator recorded marks the
//! word as a handle.

mod common;

use common::Tier;

const SOURCE: &str = r#"use std::{env, flag}

fn main() {
    let mut flags = flag::Set::new("t")
    let samples = flags.int("samples", 3, "n")
    let verbose = flags.bool("verbose", true, "v")
    let name = flags.string("name", "ab", "n")
    let ratio = flags.float("ratio", 0.5, "r")
    let parse = || flags.parse(env::args()).is_ok()
    let parsed = parse()
    let mut px = #[0; 4]
    px.par_chunks_mut(2, |row, chunk| { chunk[0] = *samples })
    let read = || if *verbose { *samples * 10 } else { 0 }
    let nested = || { let inner = || f"{*name} {*ratio} {*verbose}"; inner() }
    println(f"{parsed} {px} {read()} {nested()}")
}
"#;

#[test]
fn a_captured_flag_handle_reads_as_its_value_on_every_tier() {
    for tier in [Tier::Bytecode, Tier::Vm, Tier::Llvm] {
        let out = common::gos_run_on(tier, SOURCE, None, &[]);
        assert_eq!(
            common::stdout(&out),
            "true #[3, 0, 3, 0] 30 ab 0.5 true\n",
            "{tier:?}: {}",
            common::stderr(&out)
        );
    }
}
