//! A single name in a pattern means what it resolves to, a pattern that may
//! fail to match is refused where every value must match, and variants are
//! imported by `use`.

#![allow(missing_docs)]

use std::path::PathBuf;
use std::process::{Command, Output};

fn gos(args: &[&str], source: &str, tag: &str) -> Output {
    let dir = std::env::temp_dir().join(format!("gos-pattern-names-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let file: PathBuf = dir.join("main.gos");
    std::fs::write(&file, source).expect("write source");
    let out = Command::new(env!("CARGO_BIN_EXE_gos"))
        .args(args)
        .arg(&file)
        .output()
        .expect("run gos");
    let _ = std::fs::remove_dir_all(&dir);
    out
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn names_match_or_bind_by_what_they_resolve_to() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../feature-testing-examples/pattern_names_by_resolution.gos");
    let out = Command::new(env!("CARGO_BIN_EXE_gos"))
        .arg("run")
        .arg(&fixture)
        .output()
        .expect("run gos");
    assert!(out.status.success(), "{}", stderr(&out));
    let round = "green\nlimit\nother 9\n12\n3\n24\nother up\n";
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("round 1\n{round}round 2\n{round}")
    );
}

#[test]
fn patterns_that_may_not_match_are_refused_where_every_value_must() {
    let source = "enum Shape { Circle(i64), Square(i64) }\n\
                  const LIMIT: i64 = 5\n\
                  fn g(Shape::Circle(r): Shape) -> i64 { r }\n\
                  fn main() {\n\
                  \x20   let s = Shape::Square(3)\n\
                  \x20   let Shape::Circle(r) = s\n\
                  \x20   let o: Option<i64> = None\n\
                  \x20   let Some(x) = o\n\
                  \x20   let LIMIT = 5\n\
                  \x20   for Shape::Circle(c) in #[Shape::Square(1)] { println(c) }\n\
                  \x20   println(f\"{r} {x} {g(Shape::Square(1))}\")\n\
                  }\n";
    let out = gos(&["check"], source, "refutable");
    let text = stderr(&out);
    assert!(!out.status.success());
    assert_eq!(text.matches("error[GT0047]").count(), 5, "{text}");
}

#[test]
fn a_select_receive_binds_the_option_and_refuses_a_pattern_it_could_not_fill() {
    let ok = "use std::sync::channel_unbounded\n\
              enum Cmd { Go(i64), Stop }\n\
              fn main() {\n\
              \x20   let tx, rx = channel_unbounded()\n\
              \x20   tx.send(Cmd::Go(1))\n\
              \x20   tx.close()\n\
              \x20   for _ in 0..3 {\n\
              \x20       select {\n\
              \x20           Some(c) = rx.recv() => match c { Cmd::Go(n) => println(f\"go {n}\"), Cmd::Stop => println(\"stop\") }\n\
              \x20           None = rx.recv() => println(\"closed\")\n\
              \x20       }\n\
              \x20   }\n\
              }\n";
    let out = gos(&["run"], ok, "select_option");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "go 1\nclosed\nclosed\n"
    );
    let refused = "use std::sync::channel_unbounded\n\
                   fn main() {\n\
                   \x20   let tx, rx = channel_unbounded()\n\
                   \x20   tx.send(3)\n\
                   \x20   select { Some(3) = rx.recv() => println(\"three\"), default => () }\n\
                   }\n";
    let out = gos(&["check"], refused, "select_refutable");
    let text = stderr(&out);
    assert!(!out.status.success());
    assert_eq!(text.matches("error[GT0047]").count(), 1, "{text}");
}

#[test]
fn a_match_in_a_select_arm_is_checked_for_exhaustiveness() {
    let source = "use std::sync::channel_unbounded\n\
                  enum Cmd { Go(i64), Stop }\n\
                  fn main() {\n\
                  \x20   let tx, rx = channel_unbounded()\n\
                  \x20   tx.send(Cmd::Stop)\n\
                  \x20   select {\n\
                  \x20       Some(c) = rx.recv() => match c { Cmd::Go(n) => println(f\"go {n}\") }\n\
                  \x20       None = rx.recv() => println(\"closed\")\n\
                  \x20   }\n\
                  }\n";
    let out = gos(&["check"], source, "select_exhaustive");
    let text = stderr(&out);
    assert!(!out.status.success());
    assert!(text.contains("error[GM0001]"), "{text}");
}

#[test]
fn a_match_arm_that_misspells_a_variant_names_it() {
    let source = "enum Shape { Circle, Square }\n\
                  fn main() {\n\
                  \x20   match Shape::Square {\n\
                  \x20       Circel => println(\"circle\")\n\
                  \x20       Square => println(\"square\")\n\
                  \x20   }\n\
                  }\n";
    let text = stderr(&gos(&["check"], source, "misspelled"));
    assert!(text.contains("error[GR0022]"), "{text}");
    assert!(text.contains("did you mean `Circle`?"), "{text}");
}

#[test]
fn a_glob_of_a_module_is_refused() {
    let source =
        "use shapes::*\n\nmod shapes {\n    pub fn area() -> i64 { 1 }\n}\n\nfn main() {}\n";
    let text = stderr(&gos(&["check"], source, "glob"));
    assert!(text.contains("error[GR0024]"), "{text}");
}

#[test]
fn names_that_depart_from_rust_conventions_are_linted() {
    let source = "struct point { x: i64 }\n\
                  const limit: i64 = 2\n\
                  fn Compute(Value: i64) -> i64 { Value + limit }\n\
                  fn main() { println(Compute(point { x: 1 }.x)) }\n";
    let text = stderr(&gos(&["check"], source, "naming"));
    for code in ["GL0057", "GL0058", "GL0059"] {
        assert!(text.contains(&format!("warning[{code}]")), "{code}: {text}");
    }
}
