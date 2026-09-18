//! A parallel adapter costs what its sequential twin costs whenever it cannot
//! help. Asserted on the mechanism of that overhead - work handed to the pool -
//! rather than on wall-clock time, so the gate means the same on every machine.
//! `rc_call_counts.rs` pins the other mechanism, reference counts per element.

mod common;

use common::{TIERS, gos_run_on, stderr};

const STATS: &[(&str, &str)] = &[("GOS_PAR_STATS", "1")];

#[test]
fn a_call_below_the_grain_submits_nothing_to_the_pool() {
    let src = "fn main() { let xs = #[1, 2, 3]\n println(\"{}\", xs.par_map(|v| v * 2)) }";
    for tier in TIERS {
        let out = gos_run_on(tier, src, None, STATS);
        assert!(
            stderr(&out).contains("par submitted: 0"),
            "{tier:?}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn a_single_worker_runs_every_chunk_inline() {
    let src = "fn main() { let xs = (0..100000).collect()\n \
               println(\"{} {}\", xs.par_sum(), xs.par_map(|v| v + 1).len()) }";
    for tier in TIERS {
        let out = gos_run_on(tier, src, Some(1), STATS);
        assert!(
            stderr(&out).contains("par submitted: 0"),
            "{tier:?}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn a_large_call_on_several_workers_reaches_the_pool() {
    // The counterpart of the two above: the stat counts real submissions, so
    // a zero there is evidence rather than a counter that never moves.
    let src = "fn main() { let xs = (0..100000).collect()\n println(\"{}\", xs.par_sum()) }";
    for tier in TIERS {
        let out = gos_run_on(tier, src, Some(4), STATS);
        let err = stderr(&out);
        assert!(
            err.contains("par submitted: ") && !err.contains("par submitted: 0"),
            "{tier:?}: {err}"
        );
    }
}
