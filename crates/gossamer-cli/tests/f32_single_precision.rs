//! `f32` arithmetic answers IEEE single-precision results on every tier: the
//! native tier computes `+ - * /` of two `f32` values in `float`, which must
//! give the same bits as the `double` operation rounded to `f32`.

mod common;

use common::Tier;

const SOURCE: &str = r#"fn main() {
    let mut acc: f32 = 0.0
    let mut prod: f32 = 1.0
    let mut q: f32 = 1.0
    for i in 1..2000 {
        let x = i as f32 * 0.1
        acc += x * 1.0001 - x / 3.0
        prod = prod * 1.0003 + 0.000001
        q = (q + x) / (1.0 + x)
    }
    let r: f32 = 16777216.0 + 1.0 - 1.0
    println(f"{acc.to_bits()} {prod.to_bits()} {q.to_bits()} {r.to_bits()} {(0.1f32 + 0.2).to_bits()}")
}
"#;

#[test]
fn f32_arithmetic_agrees_bit_for_bit_across_tiers() {
    let reference = common::stdout(&common::gos_run_on(Tier::Bytecode, SOURCE, None, &[]));
    assert!(!reference.is_empty());
    for tier in [Tier::Vm, Tier::Llvm] {
        let out = common::gos_run_on(tier, SOURCE, None, &[]);
        assert_eq!(
            common::stdout(&out),
            reference,
            "{tier:?}: {}",
            common::stderr(&out)
        );
    }
}
