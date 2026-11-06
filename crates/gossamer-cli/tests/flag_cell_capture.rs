//! A `std::flag` cell captured by a closure reads as its value inside the
//! closure, on every tier. The lifted closure body reads the capture back
//! from its environment, where the native lowering used to see a plain word
//! and answer the cell's address.

mod common;

use common::Tier;

const SOURCE: &str = r#"use std::{env, flag}

fn main() {
    let mut flags = flag::Set::new("t")
    let samples = flags.int("samples", 3, "n")
    let verbose = flags.bool("verbose", true, "v")
    flags.parse(env::args()).unwrap()
    let mut px = #[0; 4]
    px.par_chunks_mut(2, |row, chunk| { chunk[0] = *samples })
    let read = || if *verbose { *samples * 10 } else { 0 }
    println(f"{px} {read()}")
}
"#;

#[test]
fn a_captured_flag_cell_reads_as_its_value_on_every_tier() {
    for tier in [Tier::Bytecode, Tier::Vm, Tier::Llvm] {
        let out = common::gos_run_on(tier, SOURCE, None, &[]);
        assert_eq!(
            common::stdout(&out),
            "#[3, 0, 3, 0] 30\n",
            "{tier:?}: {}",
            common::stderr(&out)
        );
    }
}
