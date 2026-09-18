//! The elementwise adapters answer exactly what their sequential twins answer,
//! on every tier.

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

#[test]
fn par_map_keeps_input_order() {
    let out = on_every_tier(
        "fn main() { let xs = #[1, 2, 3, 4]\n println(\"{}\", xs.par_map(|v| v * 10)) }",
    );
    assert_eq!(out, "#[10, 20, 30, 40]");
}

#[test]
fn par_filter_keeps_input_order() {
    let out = on_every_tier(
        "fn main() { let xs = #[1, 2, 3, 4, 5, 6]\n \
         println(\"{}\", xs.par_filter(|v| v % 2 == 0)) }",
    );
    assert_eq!(out, "#[2, 4, 6]");
}

#[test]
fn par_map_over_a_range_works() {
    let out = on_every_tier("fn main() { println(\"{}\", (0..4).par_map(|i| i * i)) }");
    assert_eq!(out, "#[0, 1, 4, 9]");
}

#[test]
fn par_map_answers_what_map_answers_for_a_large_input() {
    let out = on_every_tier(
        "fn main() { let xs = (0..100000).collect()\n \
         println(\"{}\", xs.par_map(|v| v * 3) == xs.map(|v| v * 3)) }",
    );
    assert_eq!(out, "true");
}

#[test]
fn an_empty_input_answers_an_empty_vector() {
    let out = on_every_tier(
        "fn main() { let xs: Vec<i64> = #[]\n println(\"{}\", xs.par_map(|v| v * 2)) }",
    );
    assert_eq!(out, "#[]");
}

#[test]
fn elements_with_owned_fields_keep_their_values() {
    let out = on_every_tier(
        "struct Named { name: String, score: i64 }\n\
         fn label(n: i64) -> String { format(\"item-{}\", n) }\n\
         fn main() {\n\
             let xs = (0..3000).collect()\n\
             let people = xs.par_map(|v| Named { name: label(v), score: v % 13 })\n\
             let top = people.par_filter(|p| p.score == 12)\n\
             println(\"{} {} {}\", top.len(), top[0].name, top[top.len() - 1].name)\n\
         }",
    );
    assert_eq!(out, "230 item-12 item-2989");
}

#[test]
fn a_captured_container_is_read_by_every_worker() {
    let out = on_every_tier(
        "fn main() {\n\
             let table = #[10, 20, 30]\n\
             let xs = (0..10000).collect()\n\
             println(\"{}\", xs.par_map(|v| table[v % 3]).par_sum())\n\
         }",
    );
    assert_eq!(out, "199990");
}

#[test]
fn a_callback_may_call_an_adapter_of_its_own() {
    let out = on_every_tier(
        "fn main() {\n\
             let grid = (0..40).par_map(|r| (0..50).par_map(|c| r * c).par_sum())\n\
             println(\"{}\", grid.par_sum())\n\
         }",
    );
    assert_eq!(out, "955500");
}

#[test]
fn an_adapter_inside_an_arena_block_answers_the_same() {
    let out = on_every_tier(
        "fn main() {\n\
             let xs = (0..5000).collect()\n\
             let mut total = 0\n\
             arena {\n\
                 let lens = xs.par_map(|v| format(\"item-{}\", v).len())\n\
                 total = lens.par_sum()\n\
             }\n\
             println(\"{}\", total)\n\
         }",
    );
    assert_eq!(out, "43890");
}

#[test]
fn the_lowest_failing_element_is_the_one_reported() {
    let src = "fn check(v: i64) -> i64 {\n\
                   if v == 700 || v == 90000 || v == 45000 { panic(format(\"bad element {}\", v)) }\n\
                   v * 2\n\
               }\n\
               fn main() { let xs = (0..100000).collect()\n println(\"{}\", xs.par_map(|v| check(v)).len()) }";
    for tier in TIERS {
        for workers in [1, 4, 16] {
            let out = gos_run_on(tier, src, Some(workers), &[]);
            assert_eq!(out.status.code(), Some(101), "{tier:?} at {workers}");
            let err = stderr(&out);
            assert!(
                err.contains("bad element 700"),
                "{tier:?} at {workers}: {err}"
            );
            assert!(
                !err.contains("bad element 45000"),
                "{tier:?} at {workers}: {err}"
            );
        }
    }
}
