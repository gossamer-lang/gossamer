//! Reductions answer what the sequential fold answers, preserve index order,
//! and do not depend on the machine's worker count.

mod common;

use common::{TIERS, gos_run_on, stderr, stdout};

fn on_every_tier(src: &str) -> String {
    let mut answer: Option<String> = None;
    for tier in TIERS {
        let out = gos_run_on(tier, src, None, &[]);
        assert!(out.status.success(), "{tier:?} failed: {}", stderr(&out));
        let text = stdout(&out);
        if let Some(first) = &answer {
            assert_eq!(&text, first, "{tier:?} answered differently");
        } else {
            answer = Some(text);
        }
    }
    answer.unwrap_or_default().trim().to_string()
}

/// The same program's stdout on every tier at 1, 4, and 16 workers.
fn same_bits_everywhere(src: &str) {
    let reference = stdout(&gos_run_on(common::Tier::Bytecode, src, Some(1), &[]));
    assert!(!reference.trim().is_empty());
    for tier in TIERS {
        for workers in [1, 4, 16] {
            let out = gos_run_on(tier, src, Some(workers), &[]);
            assert_eq!(stdout(&out), reference, "{tier:?} at {workers} workers");
        }
    }
}

#[test]
fn par_sum_matches_the_sequential_sum_for_integers() {
    let out = on_every_tier(
        "fn main() { let xs = (0..10000).collect()\n \
         println(\"{}\", xs.par_sum() == xs.iter().sum()) }",
    );
    assert_eq!(out, "true");
}

#[test]
fn par_reduce_with_an_identity_answers_it_for_an_empty_input() {
    let out = on_every_tier(
        "fn main() { let xs: Vec<i64> = #[]\n println(\"{}\", xs.par_reduce(7, |a, b| a + b)) }",
    );
    assert_eq!(out, "7");
}

#[test]
fn par_reduce_preserves_index_order_for_a_non_commutative_combine() {
    // String concatenation is associative and not commutative. If the tree
    // reordered the leaves this would not spell the alphabet.
    let out = on_every_tier(
        "fn main() { let xs = #[\"a\", \"b\", \"c\", \"d\", \"e\"]\n \
         println(\"{}\", xs.par_reduce(\"\", |a, b| a + b)) }",
    );
    assert_eq!(out, "abcde");
}

#[test]
fn a_long_non_commutative_reduction_keeps_index_order() {
    let out = on_every_tier(
        "fn main() {\n\
             let xs = (0..5000).map(|i| format(\"{}\", i % 10)).collect()\n\
             let joined = xs.par_reduce(\"\", |a, b| a + b)\n\
             println(\"{} {}\", joined.len(), joined == xs.iter().fold(\"\", |a, b| a + b))\n\
         }",
    );
    assert_eq!(out, "5000 true");
}

#[test]
fn a_float_reduction_answers_the_same_bits_at_every_worker_count() {
    // The tree is a function of length and the grain. If it ever becomes a
    // function of the worker count, these differ.
    same_bits_everywhere(
        "fn main() { let xs = (0..100000).map(|i| 1.0 / ((i + 1) as f64)).collect()\n \
         println(\"{:?}\", xs.par_sum()) }",
    );
}

#[test]
fn par_reduce_is_deterministic_at_every_worker_count_too() {
    same_bits_everywhere(
        "fn main() { let xs = (0..100000).map(|i| 1.0 / ((i + 1) as f64)).collect()\n \
         println(\"{:?}\", xs.par_reduce(0.0, |a, b| a + b)) }",
    );
}

#[test]
fn par_min_and_par_max_answer_none_for_an_empty_input() {
    let out = on_every_tier(
        "fn main() { let xs: Vec<i64> = #[]\n println(\"{} {}\", xs.par_min(), xs.par_max()) }",
    );
    assert_eq!(out, "None None");
}

#[test]
fn par_min_and_par_max_follow_the_sequential_order() {
    let out = on_every_tier(
        "fn main() {\n\
             let fs = #[3.0, -0.0, 0.0, -2.5, 1e300]\n\
             let zs = #[0.0, -0.0]\n\
             let words = #[\"pear\", \"apple\", \"fig\", \"zoo\"]\n\
             let pairs = #[(2, \"b\"), (1, \"z\"), (1, \"a\"), (3, \"c\")]\n\
             println(\"{:?} {:?} {:?} {:?}\", fs.par_min(), fs.min(), fs.par_max(), fs.max())\n\
             println(\"{:?} {:?} {:?} {:?}\", zs.par_min(), zs.min(), zs.par_max(), zs.max())\n\
             println(\"{:?} {:?}\", words.par_min() == words.min(), words.par_max() == words.max())\n\
             println(\"{:?} {:?}\", pairs.par_min() == pairs.min(), pairs.par_max() == pairs.max())\n\
         }",
    );
    assert_eq!(
        out,
        "Some(-2.5) Some(-2.5) Some(1e300) Some(1e300)\n\
         Some(-0.0) Some(-0.0) Some(0.0) Some(0.0)\n\
         true true\n\
         true true"
    );
}

#[test]
fn a_user_ordering_keeps_the_first_of_equal_elements() {
    let out = on_every_tier(
        "struct Job { name: String, pri: i64 }\n\
         impl Ord for Job { fn cmp(&self, other: Job) -> i64 { self.pri - other.pri } }\n\
         fn main() {\n\
             let jobs = #[Job { name: \"first\", pri: 2 }, Job { name: \"low-a\", pri: 1 }, \
                          Job { name: \"low-b\", pri: 1 }, Job { name: \"top-a\", pri: 5 }, \
                          Job { name: \"top-b\", pri: 5 }]\n\
             println(\"{} {}\", jobs.par_min().unwrap().name, jobs.min().unwrap().name)\n\
             println(\"{} {}\", jobs.par_max().unwrap().name, jobs.max().unwrap().name)\n\
         }",
    );
    assert_eq!(out, "low-a low-a\ntop-a top-a");
}

#[test]
fn a_reduction_callback_obeys_the_admissibility_rule() {
    let out = common::gos_check_str(
        "fn main() { let xs = #[1, 2]\n \
         println(\"{}\", xs.par_reduce(0, |a, b| { println(\"{}\", a)\n a + b })) }",
    );
    assert!(stderr(&out).contains("GT0090"));
}
