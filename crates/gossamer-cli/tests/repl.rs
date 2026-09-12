//! End-to-end tests for the interactive REPL (`gos repl`).
//!
//! Drives the binary's stdin with a fixed input script and asserts
//! against captured stdout / stderr. The REPL prints the banner and
//! per-input results to stdout; runtime errors and parse-error
//! summaries go to stderr. With stdin piped (not a TTY) rustyline
//! falls back to its dumb-terminal reader, so the prompt itself
//! never lands in captured output.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

fn gos_bin() -> PathBuf {
    PathBuf::from(env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

struct ReplOutput {
    stdout: String,
    stderr: String,
    success: bool,
}

/// Spawns `gos repl`, writes `input` to its stdin, waits for the
/// process to terminate, and returns the captured streams. EOF on
/// stdin terminates the loop cleanly via rustyline's `ReadlineError::Eof`
/// branch, so explicit `%quit` is optional.
fn run_repl(input: &str) -> ReplOutput {
    run_repl_args(input, &[])
}

fn run_repl_args(input: &str, args: &[&str]) -> ReplOutput {
    // REPL history is normally persistent. Every test gets a private path so
    // history assertions never read or mutate a developer's real session.
    // Test threads share this process, and two of them can read the clock in
    // the same tick, so the counter is what makes each history path its own.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let history_path = env::temp_dir().join(format!(
        "gos-repl-history-test-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos()),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ));
    let mut child = Command::new(gos_bin())
        .arg("repl")
        .args(args)
        .env("GOSSAMER_HISTORY", &history_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn gos repl");
    {
        let stdin = child.stdin.as_mut().expect("stdin handle");
        stdin.write_all(input.as_bytes()).expect("write stdin");
    }
    // Drop stdin by taking it out - closes the pipe so the REPL sees EOF.
    drop(child.stdin.take());

    // Bounded wait so a hung REPL fails fast rather than blocking CI.
    let start = std::time::Instant::now();
    loop {
        if child.try_wait().expect("try_wait").is_some() {
            break;
        }
        if start.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            panic!("gos repl did not terminate within 30s");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = child.wait_with_output().expect("wait_with_output");
    let output = ReplOutput {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        success: out.status.success(),
    };
    let _ = fs::remove_file(history_path);
    output
}

/// A file's imports precede its items, and the prompt reaches a session's
/// declarations in whatever order the user types them, so an import written
/// after a struct still lands in the session it belongs to.
#[test]
fn repl_accepts_an_import_after_another_declaration() {
    let out = run_repl(
        "struct Point { x: i64 }\nuse std::time\ntime::Duration::from_millis(5).as_millis()\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        !out.stderr.contains("GP0001"),
        "an import after a declaration was rejected; stderr: {}",
        out.stderr
    );
    assert!(
        out.stdout.contains('5'),
        "expected the imported call's result; stdout: {}",
        out.stdout
    );
}

/// A REPL result reads in the spelling its type is written in, the same one
/// `%bindings` shows for a binding of that type.
#[test]
fn repl_prints_a_sequence_in_its_own_literal_spelling() {
    let out = run_repl("#[1, 2, 3]\n[1, 2]\n#{7}\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("#[1, 2, 3]"),
        "a Vec prints in the Vec spelling; stdout: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("[1, 2]") && !out.stdout.contains("#[1, 2]\n"),
        "a fixed array prints in the bare bracket spelling; stdout: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("#{7}"),
        "a Set prints in the Set spelling; stdout: {}",
        out.stdout
    );
}

#[test]
fn repl_evaluates_simple_expression() {
    let out = run_repl("1 + 2\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains('3'),
        "expected `3` in stdout; got: {}",
        out.stdout
    );
}

#[test]
fn repl_prints_integer_division_result() {
    let out = run_repl("9600 / 60\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("160"),
        "integer division result was not printed; stdout: {}",
        out.stdout
    );
}

#[test]
fn repl_reports_mixed_numeric_types_without_vm_register_details() {
    let out = run_repl("9 * 9.0\n9.0 * 9\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stderr
            .contains("incompatible types: `i64` (`9`) and `f64` (`9.0`)")
            && out
                .stderr
                .contains("incompatible types: `f64` (`9.0`) and `i64` (`9`)"),
        "both operand orders should preserve the source expressions and order; stderr: {}",
        out.stderr
    );
    assert!(
        !out.stderr.contains("register"),
        "runtime diagnostics must not expose VM registers; stderr: {}",
        out.stderr
    );
}

#[test]
fn vec_insert_results_do_not_corrupt_persistent_repl_bindings() {
    let out = run_repl(
        "let mut values: Vec<i64> = Vec::from([1, 2, 3])\nlet ok = values.insert(1, 9)\nlet failure = values.insert(99, 8)\n%b\n",
    );
    assert!(out.success, "stderr: {}", out.stderr);
    assert!(out.stderr.is_empty(), "stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("mut values: Vec<i64> = #[1, 9, 2, 3]"),
        "stdout: {}",
        out.stdout
    );
    assert!(
        out.stdout
            .contains("ok: Result<(), errors::Error> = Ok(())")
    );
    assert!(
        out.stdout
            .contains("failure: Result<(), errors::Error> = Err(")
    );
}

#[test]
fn direct_vec_insert_results_replay_without_poisoning_the_repl() {
    let out = run_repl(
        "let mut first = Vec::new()\n\
         first.push(0)\n\
         let bound = first.insert(0, 0)\n\
         let mut valid = Vec::new()\n\
         valid.insert(0, 0)\n\
         valid.insert(0, 1)\n\
         let mut invalid = Vec::new()\n\
         invalid.insert(10, 10)\n\
         let after = 42\n\
         %b\n",
    );
    assert!(out.success, "stderr: {}", out.stderr);
    assert!(out.stderr.is_empty(), "stderr: {}", out.stderr);
    for expected in [
        "mut first: Vec<i64> = #[0, 0]",
        "bound: Result<(), errors::Error> = Ok(())",
        "mut valid: Vec<i64> = #[1, 0]",
        "mut invalid: Vec<i64> = #[]",
        "after: i64 = 42",
    ] {
        assert!(
            out.stdout.contains(expected),
            "missing `{expected}`: {}",
            out.stdout
        );
    }
    assert!(out.stdout.contains("Ok(())"), "stdout: {}", out.stdout);
    assert!(
        out.stdout.contains(
            "Err(errors::Error { message: \"insert: index 10 out of bounds for length 0\""
        ),
        "stdout: {}",
        out.stdout
    );
    assert!(!out.stdout.contains("<unknown>"), "stdout: {}", out.stdout);
    assert!(!out.stdout.contains("<error:"), "stdout: {}", out.stdout);
}

#[test]
fn repl_binding_and_declaration_listing_do_not_replay_program_output() {
    let out = run_repl(
        "fn id(x: i64) -> i64 { x }\n\
         let v = Vec::from([1, 2])\n\
         let a = for e in v { println(e) }\n\
         a\n\
         %b\n\
         %d\n",
    );
    assert!(out.success, "stderr: {}", out.stderr);
    assert!(out.stderr.is_empty(), "stderr: {}", out.stderr);

    let one_count = out.stdout.lines().filter(|line| *line == "1").count();
    let two_count = out.stdout.lines().filter(|line| *line == "2").count();
    assert_eq!(one_count, 1, "stdout: {}", out.stdout);
    assert_eq!(two_count, 1, "stdout: {}", out.stdout);
    assert!(
        out.stdout.contains("v: Vec<i64> = #[1, 2]"),
        "stdout: {}",
        out.stdout
    );
    assert!(out.stdout.contains("a:"), "stdout: {}", out.stdout);
    assert!(
        out.stdout.contains("fn id(x: i64) -> i64 { x }"),
        "stdout: {}",
        out.stdout
    );
}

#[test]
fn repl_reports_the_computed_operand_in_chained_numeric_mismatches() {
    let out = run_repl("0.38 * 40.0 * 50\n0.48 * 40.0 * 40\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stderr
            .contains("incompatible types: `f64` (`0.38 * 40.0`) and `i64` (`50`)")
            && out
                .stderr
                .contains("incompatible types: `f64` (`0.48 * 40.0`) and `i64` (`40`)"),
        "chained numeric errors must identify the source expressions involved: stderr={}",
        out.stderr
    );
}

#[test]
fn repl_persists_bindings_across_lines() {
    let out = run_repl("let x = 5\nx * 2\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        !out.stdout.contains("binding added"),
        "default REPL output should be quiet; stdout: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("10"),
        "binding `x` did not persist; stdout: {}",
        out.stdout
    );
}

#[test]
fn repl_redefines_a_name_whose_range_a_pipeline_consumed() {
    let out =
        run_repl("let r = 0..10\nlet m = r.map(|x| x * 2)\nlet r = 0..20\n%b\nlet r = 10\nr\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        !out.stdout.contains("GT0042") && !out.stderr.contains("GT0042"),
        "rebinding `r` must start a fresh binding; stdout: {} stderr: {}",
        out.stdout,
        out.stderr
    );
    assert!(
        out.stdout.contains("r: Range<i64> = 0..20"),
        "rebound range was not listed with its type; stdout: {}",
        out.stdout
    );
    assert!(
        out.stdout.lines().any(|line| line == "10"),
        "rebinding `r` to a scalar did not take effect; stdout: {}",
        out.stdout
    );
}

/// `%b` observes a binding to report it; observing is not a traversal, so a
/// consumed iterator still lists its type and value. A written read of the
/// same binding remains a linearity violation.
#[test]
fn repl_lists_a_consumed_iterator_with_its_type_and_value() {
    let out = run_repl("let r = 0..10\nlet m = r.map(|x| x * 2)\n%b\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("r: Range<i64> = 0..10"),
        "consumed range must still list its type and value; stdout: {}",
        out.stdout
    );
    assert!(
        !out.stdout.contains("GT0042"),
        "binding listing must not report a linearity error; stdout: {}",
        out.stdout
    );

    let read = run_repl("let r = 0..10\nlet m = r.map(|x| x * 2)\nr\n");
    assert!(
        read.stderr.contains("GT0042") || read.stdout.contains("GT0042"),
        "a written read of the consumed binding must still be rejected; stdout: {} stderr: {}",
        read.stdout,
        read.stderr
    );
}

#[test]
fn repl_accepts_trailing_line_comments() {
    let out = run_repl("let a = 1 // comment\na\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stderr.is_empty(),
        "a trailing comment must not cause a parse error: {}",
        out.stderr
    );
    assert!(
        out.stdout.lines().any(|line| line == "1"),
        "binding after a trailing comment was not retained: {}",
        out.stdout
    );
}

#[test]
fn repl_accepts_semicolon_between_binding_and_multiline_loop() {
    let out = run_repl(
        "let mut pos = 1; while pos < 10 { match pos { 7 => break, _ => pos += 3 } }\npos\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stderr.is_empty(),
        "valid mixed input must not produce an error; stderr: {}",
        out.stderr
    );
    assert!(
        out.stdout.lines().any(|line| line == "7"),
        "loop should break with the persisted binding at 7; stdout: {}",
        out.stdout
    );
}

#[test]
fn repl_accepts_newline_between_binding_and_multiline_loop() {
    let out = run_repl(
        "let mut pos = 1\nwhile pos < 10 {\n    match pos {\n        7 => break\n        _ => pos += 3\n    }\n}\npos\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stderr.is_empty(),
        "valid multiline input must not produce an error; stderr: {}",
        out.stderr
    );
    assert!(
        out.stdout.lines().any(|line| line == "7"),
        "newline-separated loop should break with pos at 7; stdout: {}",
        out.stdout
    );
}

#[test]
fn repl_history_outputs_copyable_unnumbered_inputs() {
    let out = run_repl("let answer = 42\nanswer\n%history\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        ["let answer = 42", "answer"]
            .iter()
            .all(|entry| out.stdout.lines().any(|line| line == *entry)),
        "history should reproduce each input without decoration: {}",
        out.stdout
    );
    assert!(
        !out.stdout.lines().any(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("1:") || trimmed.starts_with("2:") || trimmed.starts_with("3:")
        }),
        "history output should not contain numeric prefixes: {}",
        out.stdout
    );
}

#[test]
fn repl_history_shortcut_filters_with_a_regex() {
    let out = run_repl("let answer = 42\nanswer\n%h ^let\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.lines().any(|line| line == "let answer = 42"),
        "%h should retain matching entries: {}",
        out.stdout
    );
    assert!(
        !out.stdout.lines().any(|line| line == "answer"),
        "%h should omit non-matching entries: {}",
        out.stdout
    );
}

#[test]
fn repl_history_search_does_not_match_itself() {
    let query = format!("repl_history_self_match_{}", std::process::id());
    let command = format!("%h {query}");
    let out = run_repl(&format!("{command}\n"));
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        !out.stdout.lines().any(|line| line == command),
        "%h should search the history from before the current command: {}",
        out.stdout
    );
}

#[test]
fn repl_clear_history_removes_prior_entries_and_is_not_recorded() {
    let marker = format!("repl_clear_history_marker_{}", std::process::id());
    let out = run_repl(&format!("{marker}\n%clear-history\n%h {marker}\n"));
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("history cleared"),
        "%clear-history should confirm success: {}",
        out.stdout
    );
    assert!(
        !out.stdout.lines().any(|line| line == marker),
        "%h after %clear-history must not return an earlier input: {}",
        out.stdout
    );
    assert!(
        !out.stdout.lines().any(|line| line == "%clear-history"),
        "%clear-history must not add itself back to history: {}",
        out.stdout
    );
}

#[test]
fn repl_info_is_limited_to_language_and_standard_library_catalogs() {
    let out = run_repl("%i 42\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("nothing found for `42`"),
        "%info should not evaluate arbitrary REPL expressions: {}",
        out.stdout
    );
}

#[test]
fn repl_reports_a_missing_enum_body_once() {
    let out = run_repl("enum Nothing\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stderr.matches("error[").count() == 1
            && out.stderr.contains("expected `{` to open enum body")
            && !out.stderr.contains("unexpected keyword `fn`"),
        "missing enum body should have one clear diagnostic: {}",
        out.stderr
    );
}

#[test]
fn repl_indexes_with_a_computed_usize_struct_field() {
    let out = run_repl(
        "struct Mem { mem: Vec<i64> pos: usize }\n\
         impl Mem {\n\
             fn set_memory(&mut self, offset: usize, value: i64) {\n\
                 let pos = self.mem[self.pos + offset] as usize\n\
                 self.mem[pos] = value\n\
             }\n\
             fn get_memory(self, index: usize) -> i64 { self.mem[index] }\n\
         }\n\
         let mut mem = Mem { mem: Vec::from([1, 2, 3, 4]) pos: 0 }\n\
         mem.set_memory(1, 9)\n\
         println(mem.get_memory(1))\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        !out.stderr.contains("index must be integer"),
        "computed usize struct-field index should not reach the VM as a non-integer: {}",
        out.stderr
    );
}

#[test]
fn repl_bindings_show_the_concrete_type_of_a_clone() {
    let out = run_repl("let mut a = Vec::from([1, 2, 3])\nlet mut c = a.clone()\n%b\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("mut c: Vec<i64> = #[1, 2, 3]"),
        "a clone binding should retain its concrete type: {}",
        out.stdout
    );
}

#[test]
fn repl_supports_reference_let_patterns_and_explains_invalid_syntax() {
    let out = run_repl(
        "let mut &d = m\n\
         let mut m = [1, 2, 3, 4]\n\
         let &mut d = m\n\
         let value = 7\n\
         let &shared = &value\n\
         println(shared)\n\
         let mut value = 9\n\
         let &mut exclusive = &mut value\n\
         println(exclusive)\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stderr
            .contains("a reference pattern is written `&mut name`, not `mut &name`")
            && out
                .stderr
                .contains("`let &mut name = value` requires an `&mut` initializer")
            && out.stderr.contains("write `let name = &mut value`")
            && !out.stderr.contains("does not yet support")
            && !out.stderr.contains("let-pattern shape"),
        "only invalid reference-pattern syntax should be diagnosed: {}",
        out.stderr
    );
    assert!(
        out.stdout.contains("7\n9\n"),
        "shared and mutable reference patterns should bind their inner values: {}",
        out.stdout
    );
}

#[test]
fn repl_supports_at_let_patterns() {
    let out = run_repl(
        "let mut whole @ inner = 7\n\
         println(whole)\n\
         println(inner)\n\
         whole = 8\n\
         println(whole)\n\
         println(inner)\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("7\n7\n8\n7\n"),
        "at-pattern bindings should be independent values: {}",
        out.stdout
    );
}

#[test]
fn repl_info_does_not_inspect_session_state() {
    let out = run_repl(
        "let x = 9\n\
         fn wow() { \"heyoooo\" }\n\
         %i whatever\n\
         %i wow\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("nothing found for `whatever`"),
        "%info should identify an undefined name after catalog search: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("nothing found for `wow`"),
        "%info should leave declarations to %declarations: {}",
        out.stdout
    );
}

#[test]
fn repl_explain_owns_persistent_binding_inspection() {
    let catalog = run_repl("let x = 9\n%i x\n");
    assert!(
        catalog.success,
        "repl should exit zero; stderr: {}",
        catalog.stderr
    );
    assert!(
        !catalog.stdout.contains("x [binding]") && !catalog.stdout.contains("x: i64 = 9"),
        "%i must remain catalog-only: {}",
        catalog.stdout
    );
    let explained = run_repl("let x = 9\n%e x\n%explain x -d\n");
    assert!(
        explained.success,
        "repl should exit zero; stderr: {}",
        explained.stderr
    );
    assert!(
        explained.stdout.contains("x: i64 [binding]")
            && explained.stdout.contains("x [binding]")
            && explained.stdout.contains("type: i64"),
        "%e must inspect bindings: {}",
        explained.stdout
    );

    let stdlib = run_repl("%e Map\n%e Vec::from -d\n");
    assert!(stdlib.success, "stderr: {}", stdlib.stderr);
    assert!(
        stdlib
            .stderr
            .contains("no persistent binding or declaration named `Map`")
            && stdlib
                .stderr
                .contains("no persistent binding or declaration named `Vec::from`")
            && !stdlib.stdout.contains("Map [type]")
            && !stdlib.stdout.contains("Vec::from"),
        "%e must not fall through to language or stdlib catalog entries: stdout: {}; stderr: {}",
        stdlib.stdout,
        stdlib.stderr
    );
}

#[test]
fn repl_explain_owns_persistent_declaration_inspection() {
    let out = run_repl(
        "fn wow() -> String { \"heyoooo\" }\n\
         %i wow\n\
         %e wow\n",
    );
    assert!(out.success, "stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("nothing found for `wow`")
            && out.stdout.contains("wow [declaration]")
            && out.stdout.contains("fn wow() -> String { \"heyoooo\" }"),
        "%e should inspect declarations while %i remains catalog-only: {}",
        out.stdout
    );
}

#[test]
fn repl_explain_lists_all_available_methods() {
    let out = run_repl("let mut values = Vec::from([1, 2])\n%e values\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("mut values: Vec<i64> [binding]")
            && out
                .stdout
                .contains("values.push<T>(self: &mut Vec<T>, value: T) -> () [method]")
            && out
                .stdout
                .contains("values.len<T>(self: Vec<T>) -> i64 [method]"),
        "%e should list every method available to the binding: {}",
        out.stdout
    );
}

/// Asserts a `%e` listing offers every name in `present` and none in `absent`.
fn assert_surface(stdout: &str, owner: &str, present: &[&str], absent: &[&str]) {
    for name in present {
        assert!(stdout.contains(name), "{owner} is missing {name}: {stdout}");
    }
    for name in absent {
        assert!(!stdout.contains(name), "{owner} exposed {name}: {stdout}");
    }
}

#[test]
fn repl_sequence_help_respects_type_and_receiver_capability() {
    let array = run_repl("let a: [i64; 3] = #[1, 2, 3]\n%e a\n");
    assert!(array.success, "stderr: {}", array.stderr);
    assert!(array.stdout.contains("a.len<T>(self: &[T; N])"));
    assert!(array.stdout.contains("a.clone<T, const N: i64>"));
    // A traversal reads values a fixed array already holds, so `map` is on it;
    // resizing and in-place mutation are not.
    assert_surface(
        &array.stdout,
        "immutable fixed array",
        &["a.map"],
        &["a.push", "a.capacity", "a.sort", "a.swap"],
    );

    let shared_slice =
        run_repl("let storage: [i64; 3] = #[1, 2, 3]\nlet values: &[i64] = &storage\n%e values\n");
    assert!(shared_slice.success, "stderr: {}", shared_slice.stderr);
    assert!(shared_slice.stdout.contains("values.len<T>(self: &[T])"));
    assert_surface(
        &shared_slice.stdout,
        "shared slice",
        &["values.map"],
        &[
            "values.clone",
            "values.push",
            "values.capacity",
            "values.sort",
            "values.swap",
        ],
    );

    let mutable_slice = run_repl(
        "let mut storage: [i64; 3] = #[1, 2, 3]\nlet values: &mut [i64] = &mut storage\n%e values\n",
    );
    assert!(mutable_slice.success, "stderr: {}", mutable_slice.stderr);
    assert!(
        mutable_slice
            .stdout
            .contains("values.sort<T>(self: &mut [T])")
    );
    assert!(
        mutable_slice
            .stdout
            .contains("values.swap<T>(self: &mut [T]")
    );
    assert_surface(
        &mutable_slice.stdout,
        "mutable slice",
        &["values.map"],
        &["values.push", "values.capacity", "values.clone"],
    );

    let immutable_vec = run_repl("let values: Vec<i64> = Vec::from([1, 2, 3])\n%e values\n");
    assert!(immutable_vec.success, "stderr: {}", immutable_vec.stderr);
    assert!(
        immutable_vec
            .stdout
            .contains("values.capacity<T>(self: Vec<T>)")
    );
    assert!(
        immutable_vec
            .stdout
            .contains("values.map<T, U>(self: Vec<T>")
    );
    for unavailable in [
        "values.push",
        "values.reserve",
        "values.sort",
        "values.swap",
    ] {
        assert!(
            !immutable_vec.stdout.contains(unavailable),
            "immutable Vec exposed {unavailable}: {}",
            immutable_vec.stdout
        );
    }

    let mutable_vec = run_repl("let mut values: Vec<i64> = Vec::from([1, 2, 3])\n%e values\n");
    assert!(mutable_vec.success, "stderr: {}", mutable_vec.stderr);
    for available in [
        "values.push",
        "values.reserve",
        "values.sort",
        "values.swap",
    ] {
        assert!(
            mutable_vec.stdout.contains(available),
            "mutable Vec omitted {available}: {}",
            mutable_vec.stdout
        );
    }
}

#[test]
fn repl_info_lists_matches_unless_details_are_requested() {
    let out = run_repl("%i strings::trim\n%i strings::trim -d\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout
            .contains("std::strings::trim(text: String) -> String [fn]")
            && out
                .stdout
                .contains("Removes leading and trailing whitespace.\n    Defined in: std::strings"),
        "%i should show signatures by default and documentation with -d: {}",
        out.stdout
    );
}

#[test]
fn repl_info_and_explain_details_always_follow_descriptions_with_examples() {
    let info = run_repl("%i Map::from -d\n");
    assert!(info.success, "stderr: {}", info.stderr);
    let compact_info = info.stdout.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        compact_info.contains(
            "Map::from<K, V, const N: usize>(entries: [(K, V); N]) -> Map<K, V> [associated function]"
        ) && info.stdout.contains("Creates a hash map from a fixed array of key-value tuples.")
            && info.stdout.contains("    Builtin")
            && compact_info.contains("Example: let empty: Map<String, i64> = Map::from([]);")
            && compact_info.contains("let map = {\"one\": 1, \"two\": 2};")
            && compact_info.contains("let also = Map::from([(\"one\", 1), (\"two\", 2)])"),
        "Map::from help is incomplete: {}",
        info.stdout
    );

    let explained = run_repl("let text = \"hello\"\n%e text -d\n");
    assert!(explained.success, "stderr: {}", explained.stderr);
    assert!(
        explained.stdout.contains(
            "Returns whether the string starts with a prefix.\n    Builtin\n    Example: text.starts_with(\"te\")"
        ),
        "binding method example did not use the binding: {}",
        explained.stdout
    );
}

#[test]
fn repl_hashmap_literals_and_from_tuple_arrays_work() {
    let out = run_repl(
        "let m = {\"a\": 1, \"b\": 2}\n\
         let n = Map::from([(\"a\", 1), (\"b\", 2)])\n\
         m.len()\n\
         n.len()\n\
         m.get(\"a\")\n\
         n.get(\"a\")\n",
    );
    assert!(out.success, "stderr: {}", out.stderr);
    let lines = out.stdout.lines().map(str::trim).collect::<Vec<_>>();
    assert!(
        lines.iter().filter(|line| **line == "2").count() >= 2
            && lines.iter().filter(|line| **line == "Some(1)").count() >= 2,
        "Map::from forms did not expose matching entries: {}",
        out.stdout
    );
}

#[test]
fn repl_hashmap_from_rejects_map_literal_argument() {
    let out = run_repl(
        "let m1 = {1: 2, 2: 3}\n\
         let s1 = #{1, 2}\n\
         let v4 = Vec::from(m1)\n\
         let v5 = Vec::from(s1)\n\
         let s4 = Set::from(m1)\n\
         let s5 = Set::from(s1)\n\
         let h: Map<String, i64> = Map::from({\"a\": 1})\n",
    );
    assert!(out.success, "stderr: {}", out.stderr);
    assert!(
        out.stderr
            .contains("expected `fixed array of key-value tuples`, found `Map<String, i64>`"),
        "Map::from accepted a map literal argument: {}",
        out.stderr
    );
    assert!(
        out.stderr
            .matches("expected `array, slice, or Vec`, found `Map<i64, i64>`")
            .count()
            >= 2
            && out
                .stderr
                .matches("expected `array, slice, or Vec`, found `Set<i64>`")
                .count()
                >= 2,
        "collection ::from accepted incompatible source arguments: {}",
        out.stderr
    );
}

#[test]
fn repl_btreemap_from_tuple_arrays_work() {
    let out = run_repl(
        "let mut m: BTreeMap<String, i64> = BTreeMap::from([(\"b\", 2), (\"a\", 1)])\n\
         println(m.get(\"a\"))\n\
         println(m.len())\n\
         m.clear()\n\
         println(m.is_empty())\n",
    );
    assert!(out.success, "stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("Some(1)") && out.stdout.contains('2') && out.stdout.contains("true"),
        "BTreeMap::from/method surface failed: {}",
        out.stdout
    );
}

#[test]
fn repl_vecdeque_canonical_methods_and_clear_work() {
    let out = run_repl(
        "let mut d: Deque<i64> = Deque::new()\n\
         d.push_front(2)\n\
         d.push_back(3)\n\
         println(d.pop_front())\n\
         println(d.pop_back())\n\
         d.clear()\n\
         println(d.is_empty())\n",
    );
    assert!(out.success, "stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("Some(2)")
            && out.stdout.contains("Some(3)")
            && out.stdout.contains("true"),
        "Deque canonical methods or clear failed: {}",
        out.stdout
    );
}

#[test]
fn repl_info_string_from_has_display_bound_contract() {
    let out = run_repl("%i String::from\n%i String::from -d\n");
    assert!(out.success, "stderr: {}", out.stderr);
    assert!(
        out.stdout
            .contains("String::from<T: Display>(value: T) -> String [associated function]"),
        "String::from signature fell back to an untyped value parameter: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains(
            "Converts a Display value into a string.\n    Builtin\n    Example: String::from(1)"
        ),
        "String::from details are incomplete: {}",
        out.stdout
    );
    assert!(
        !out.stdout.contains("String::from(value) -> String"),
        "String::from should not expose an untyped value parameter: {}",
        out.stdout
    );
}

#[test]
fn repl_explain_details_do_not_insert_blank_line_buffers() {
    let out = run_repl("let text = \"hello\"\n%e text -d\n");
    assert!(out.success, "stderr: {}", out.stderr);
    assert!(
        out.stdout
            .contains("capability: immutable binding\ntext.as_bytes"),
        "%e -d should start methods on the next line: {}",
        out.stdout
    );
    assert!(
        !out.stdout.contains("\n\ntext."),
        "%e -d inserted a blank line before a method entry: {}",
        out.stdout
    );
}

#[test]
fn repl_info_vec_from_has_the_array_conversion_contract() {
    let out = run_repl("%i Vec::from\n%i Vec::from -d\n%e Vec::from -d\n");
    assert!(out.success, "stderr: {}", out.stderr);
    assert!(
        out.stdout.contains(
            "Vec::from<T, const N: usize>(values: [T; N]) -> Vec<T> [associated function]"
        ),
        "Vec::from signature fell back to an opaque assoc entry: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains(
            "Creates a growable vector by moving values from a fixed-size array.\n    Builtin\n    Example: let values = Vec::from([1, 2, 3])"
        ),
        "Vec::from details are incomplete: {}",
        out.stdout
    );
    assert_eq!(
        out.stdout
            .matches("Vec::from<T, const N: usize>(values: [T; N]) -> Vec<T> [associated function]")
            .count(),
        2,
        "%i should resolve Vec::from, while %e should not use the catalog: {}",
        out.stdout
    );
    assert!(
        out.stderr
            .contains("no persistent binding or declaration named `Vec::from`"),
        "%e should reject stdlib-only Vec::from: {}",
        out.stderr
    );
}

#[test]
fn repl_resolution_errors_suggest_prelude_type_case() {
    let out = run_repl("let map = Btreemap::from({\"example\": 1})\n");
    assert!(out.success, "stderr: {}", out.stderr);
    assert!(
        out.stderr.contains("cannot find `Btreemap` in this scope")
            && out.stderr.contains("did you mean `BTreeMap`?"),
        "missing BTreeMap suggestion: {}",
        out.stderr
    );
}

#[test]
fn iterator_parameter_for_loop_preserves_single_pass_state() {
    let out = run_repl(
        "use Iterator\nfn list_range(v: Vec<i64>, r: Iterator<i64>) { for i in r { println(v[i]) } }\nlet values = Vec::from([1, 2, 3])\nlet fresh = 0..2\nlist_range(values, fresh)\n",
    );
    assert!(out.success, "stderr: {}", out.stderr);
    let values: Vec<&str> = out
        .stdout
        .lines()
        .filter(|line| line.parse::<i64>().is_ok())
        .collect();
    assert_eq!(
        values,
        ["1", "2"],
        "a fresh Iterator parameter must be driven through its single-pass state"
    );
}

#[test]
fn repl_range_binding_inspection_does_not_poison_iterator_reuse() {
    let out = run_repl(
        "use Iterator\n\
         fn list_range(v: Vec<i64>, r: Iterator<i64>) { for i in r { println(v[i]) } }\n\
         let v = Vec::from([1, 2, 3])\n\
         let r = 0..2\n\
         %b\n\
         for i in r { println(v[i]) }\n\
         list_range(v, r)\n",
    );
    assert!(out.success, "stderr: {}", out.stderr);
    assert!(
        out.stdout
            .lines()
            .any(|line| line == "r: Range<i64> = 0..2"),
        "%b must render the range binding without an inspection error: {}",
        out.stdout
    );
    let values: Vec<&str> = out
        .stdout
        .lines()
        .filter(|line| line.parse::<i64>().is_ok())
        .collect();
    assert_eq!(
        values,
        ["1", "2", "1", "2"],
        "range binding should be usable after %b and as an Iterator parameter"
    );
    assert!(
        !out.stderr.contains("already consumed"),
        "iterator consumption leaked across scopes: {}",
        out.stderr
    );
}

#[test]
fn iterator_parameter_count_method_does_not_drain_range_binding() {
    let out = run_repl(
        "use Iterator\n\
         let r = 0..2\n\
         fn count_many_range(r: Iterator<i64>) { for i in 0..3 { println(r.count()) } }\n\
         count_many_range(r)\n",
    );
    assert!(out.success, "stderr: {}", out.stderr);
    let values: Vec<&str> = out
        .stdout
        .lines()
        .filter(|line| line.parse::<i64>().is_ok())
        .collect();
    assert_eq!(
        values,
        ["2", "2", "2"],
        "Iterator method calls must fork range state instead of draining the caller binding"
    );
}

#[test]
fn repl_info_default_listing_has_no_blank_lines_between_entries() {
    let out = run_repl("%i *starts_with\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains(
            "String::starts_with(self: String, needle: String | char) -> bool [method]\nstd::path::starts_with"
        ),
        "%i should render default entries on consecutive lines: {}",
        out.stdout
    );
    assert!(
        !out.stdout.contains("[method]\n\nstd::path::starts_with"),
        "%i inserted a blank line between default entries: {}",
        out.stdout
    );
}

#[test]
fn repl_meta_info_empty_shows_only_the_catalog_directory() {
    let out = run_repl("%i\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("std::archive::tar [module]")
            && !out.stdout.contains("String::len [method]"),
        "blank %i should not emit every help entry: {}",
        out.stdout
    );
}

#[test]
fn repl_meta_info_does_not_render_keyword_documentation() {
    let out = run_repl("%i async_await\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("nothing found for `async_await`"),
        "%i should not render language keyword documentation: {}",
        out.stdout
    );
}

#[test]
fn repl_info_lists_stdlib_namespaces_and_catalog_types() {
    let out = run_repl("%i std::bufio\n%i std::archive\n%i database\n%i String -d\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("std::bufio [module]")
            && out.stdout.contains("std::database::sql [module]")
            && out.stdout.contains("std::archive::tar [module]")
            && out.stdout.contains("std::archive::zip [module]")
            && out.stdout.contains("String [type]")
            && out.stdout.contains("String::as_bytes(")
            && out
                .stdout
                .contains("Returns the UTF-8 bytes of the string."),
        "%info should expose standard-library namespaces and language types: {}",
        out.stdout
    );
}

#[test]
fn repl_info_resolves_qualified_http_constructor_with_a_real_signature() {
    let out = run_repl("%i http::Client::new -d\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout
            .contains("http::Client::new() -> http::Client [associated function]"),
        "%info should resolve the exact catalog path with its contract: {}",
        out.stdout
    );
    assert!(
        !out.stdout.contains("cannot find `http`") && !out.stdout.contains("..."),
        "%info must not evaluate a catalog path or show placeholder arguments: {}",
        out.stdout
    );
}

#[test]
fn repl_bindings_hide_inference_ids_for_empty_vec() {
    let out = run_repl(
        "let v = Vec::new()\n\
         let reserved = Vec::with_capacity(4)\n\
         let map = Map::with_capacity(4)\n\
         %b\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("v: Vec<_> = #[]"),
        "empty Vec binding should have a public generic type: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("reserved: Vec<_> = #[]") && out.stdout.contains("map: Map<_, _> = {}"),
        "capacity constructors should preserve container identity: {}",
        out.stdout
    );
    assert!(
        !out.stdout.contains('?'),
        "REPL binding output leaked an inference variable: {}",
        out.stdout
    );
}

#[test]
fn repl_bindings_keep_integral_float_decimal_points() {
    let out = run_repl(
        "struct Point<T, U> { x: T, y: U }\n\
         let point = Point { x: 1.0, y: 4.0 }\n\
         let integral = 2.0\n\
         let fractional = 2.3\n\
         %b\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout
            .contains("point: Point<f64, f64> = Point { x: 1.0, y: 4.0 }"),
        "{}",
        out.stdout
    );
    assert!(out.stdout.contains("integral: f64 = 2.0"), "{}", out.stdout);
    assert!(
        out.stdout.contains("fractional: f64 = 2.3"),
        "{}",
        out.stdout
    );
}

#[test]
fn repl_meta_commands_accept_leading_whitespace() {
    let out = run_repl("let x = 1\n  %b\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("x: i64 = 1"),
        "leading whitespace prevented meta-command dispatch: {}",
        out.stdout
    );
    assert!(
        !out.stderr.contains("cannot find"),
        "meta-command was parsed as source: {}",
        out.stderr
    );
}

#[test]
fn repl_vec_from_fixed_array_creates_growable_vec() {
    let out = run_repl(
        "let mut v = Vec::from([1, 2])\n\
         %b\n\
         v.push(3)\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("mut v: Vec<i64> = #[1, 2]"),
        "Vec::from should infer its element type: {}",
        out.stdout
    );
    assert!(
        !out.stderr.contains("unresolved") && !out.stderr.contains("type mismatch"),
        "Vec::from or Vec::push failed: {}",
        out.stderr
    );
}

#[test]
fn repl_rejects_out_of_range_vec_elements() {
    let out = run_repl("let mut v: [i8; 2] = #[1, 567]\n%b\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stderr
            .contains("integer literal `567` does not fit in `i8`"),
        "out-of-range Vec element was accepted: {}",
        out.stderr
    );
    assert!(
        out.stdout.contains("no `let` bindings yet"),
        "rejected binding was retained: {}",
        out.stdout
    );
}

#[test]
fn repl_later_assignment_cannot_retype_an_immutable_binding() {
    let out = run_repl(
        "let a = 256\n\
         let mut b: i8 = 1\n\
         let mut v: Vec<i8> = Vec::from([1, 2])\n\
         b = a\n\
         v[0] = a\n\
         %b\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    let diagnostics = format!("{}\n{}", out.stdout, out.stderr);
    assert!(
        diagnostics.matches("expected `i8`, found `i64`").count() >= 2,
        "both invalid assignments must preserve a's type: {diagnostics}"
    );
    assert!(
        out.stdout.contains("a: i64 = 256"),
        "immutable source binding was retyped: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("mut b: i8 = 1") && out.stdout.contains("mut v: Vec<i8> = #[1, 2]"),
        "failed assignments must not mutate destinations: {}",
        out.stdout
    );
}

#[test]
fn repl_byte_buffer_has_a_public_type_and_strict_mutation_contract() {
    let out = run_repl(
        "use Buffer\n\
         let mut b = Buffer::new()\n\
         b.push(65)\n\
         b.push(\"A\")\n\
         b.push(Vec::from([1, 2]))\n\
         b.push(256)\n\
         b.len()\n\
         b.to_string()\n\
         %b\n\
         %info push -d\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    let diagnostics = format!("{}\n{}", out.stdout, out.stderr);
    assert!(
        diagnostics.matches("type mismatch").count() >= 2,
        "Buffer::push accepted non-byte values: {diagnostics}"
    );
    assert!(
        diagnostics.contains("integer literal `256` does not fit in `u8`"),
        "Buffer::push did not range-check its byte argument: {diagnostics}"
    );
    assert!(
        out.stdout.contains('1') && out.stdout.contains("\"A\""),
        "rejected pushes changed the buffer: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("mut b: bytes::Buffer")
            && !out.stdout.contains("__buffer")
            && !out.stdout.contains("mut b: _"),
        "Buffer leaked an internal or inferred representation: {}",
        out.stdout
    );
    assert!(
        out.stdout
            .contains("Buffer::push(&mut self, byte: u8) -> () [method]")
            && !out.stdout.contains("push(self, ...) -> ..."),
        "Buffer help lacks its public method contract: {}",
        out.stdout
    );
}

#[test]
fn repl_uses_repr_for_results_and_display_for_explicit_printing() {
    let out = run_repl(
        "let x = \"wow\"\n\
         x\n\
         println(x)\n\
         #[x, \"ok\"]\n\
         \"ab\".chars().collect()\n\
         \"ab\".bytes()\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("\"wow\""),
        "bare string must be quoted: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("wow\n"),
        "println must use unquoted display text: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("[\"wow\", \"ok\"]"),
        "nested string repr is wrong: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("['a', 'b']"),
        "char vectors must use char literals: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("[97, 98]"),
        "String::bytes method must execute: {}",
        out.stdout
    );
}

#[test]
fn repl_decodes_byte_string_literals_without_prefix_or_quotes() {
    let out = run_repl("b'b'\nb\"b\"\nb\"a\\n\"\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("98"),
        "byte literal should render as its u8 value; stdout: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("[98]"),
        "byte string should contain only body bytes; stdout: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("[97, 10]"),
        "byte string escapes should decode before vector output; stdout: {}",
        out.stdout
    );
}

#[test]
fn repl_mutable_assignment_persists_across_lines() {
    // Reassigning a `let mut` binding from an earlier input must update the
    // persisted frame so a later read sees the new value.
    let out = run_repl("let mut name = \"Steven\"\nname = \"Mark\"\nname\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("\"Mark\""),
        "reassignment to `name` did not persist; stdout: {}",
        out.stdout
    );
    assert!(
        !out.stdout.contains("Steven"),
        "stale value returned after reassignment; stdout: {}",
        out.stdout
    );
}

#[test]
fn repl_rejects_rebinding_a_reference_with_its_referent() {
    // `let mut` permits assigning a new `&[i64; 2]`, not replacing the
    // reference binding with a bare `[i64; 2]` value.
    let out = run_repl("let source = [1, 2]\nlet mut x = &source\nx = [2, 3]\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stderr.contains("type mismatch"),
        "reference-rebinding mismatch was not reported: {}",
        out.stderr
    );
    assert!(
        out.stderr
            .contains("expected `&[i64; 2]`, found `[i64; 2]`"),
        "reference-rebinding mismatch did not preserve both types: {}",
        out.stderr
    );
}

#[test]
fn repl_reference_rebind_rejects_temporaries_without_mutating_named_referents() {
    let out = run_repl(
        "let a = [1, 2]\n\
         let b = [3, 4]\n\
         let mut c = &a\n\
         c = &b\n\
         c = &[5, 6]\n\
         let mut x = [10, 20]\n\
         let mut y = [30, 40]\n\
         let mut r = &mut x\n\
         r = &mut y\n\
         r = &mut [50, 60]\n\
         %bindings\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("a: [i64; 2] = [1, 2]"),
        "immutable original binding changed or disappeared: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("b: [i64; 2] = [3, 4]"),
        "immutable referent binding was overwritten by reference rebind: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("mut c: &[i64; 2] = &[3, 4]"),
        "rejected temporary rebind did not preserve the named referent: {}",
        out.stdout
    );
    assert!(
        !out.stdout.contains("b: [i64; 2] = [5, 6]"),
        "reference rebind leaked through and mutated immutable `b`: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("mut x: [i64; 2] = [10, 20]"),
        "mutable reference rebind changed the old named referent: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("mut r: &mut [i64; 2] = &mut [30, 40]"),
        "rejected mutable temporary rebind did not preserve the named referent: {}",
        out.stdout
    );
    assert!(
        !out.stdout.contains("mut y: [i64; 2] = [50, 60]"),
        "mutable reference rebind leaked through and mutated old `y`: {}",
        out.stdout
    );
    assert!(
        out.stderr
            .matches("reference cannot be rebound through an alias or from a temporary")
            .count()
            >= 2,
        "temporary-backed reference rebinds were not rejected: {}",
        out.stderr
    );
}

#[test]
fn repl_cannot_mutate_immutable_value_through_mutable_reference_chain() {
    let out = run_repl(
        "let a = [1, 2]\n\
         let mut b = &a\n\
         let mut c = &mut b\n\
         c[0] = 0\n\
         %bindings\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stderr
            .contains("cannot assign through shared reference `c`"),
        "write must be rejected as a shared-reference violation: {}",
        out.stderr
    );
    assert!(
        out.stdout.contains("a: [i64; 2] = [1, 2]"),
        "immutable source changed through the rejected alias chain: {}",
        out.stdout
    );
    assert!(
        !out.stdout.contains("a: [i64; 2] = [0, 2]"),
        "immutable source was modified despite GT0031: {}",
        out.stdout
    );
}

#[test]
fn repl_reference_assignment_error_shows_concrete_type() {
    let out = run_repl("let mut a = 12\nlet mut b = &mut a\nb = 16\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stderr
            .contains("type mismatch: expected `&mut i64`, found `i64`"),
        "reference mismatch must show the resolved type: {}",
        out.stderr
    );
    assert!(
        !out.stderr.contains("&mut ?"),
        "inference variable leaked into REPL diagnostic: {}",
        out.stderr
    );
}

#[test]
fn repl_compound_assignment_accumulates_across_lines() {
    // `+=` on a persisted binding must fold across inputs, in order.
    let out = run_repl("let mut c = 0\nc += 5\nc += 3\nc\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains('8'),
        "compound assignment did not accumulate; stdout: {}",
        out.stdout
    );
}

#[test]
fn repl_bindings_show_current_shadowed_lets_only() {
    let out = run_repl_args(
        "let i = 1\nlet mut i = 2\n%bindings\ni = 3\n%bindings\ni\n",
        &["-v"],
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("binding added (1 total)"),
        "first binding count should be one; stdout: {}",
        out.stdout
    );
    assert!(
        !out.stdout.contains("binding added (2 total)"),
        "shadowing let should replace the visible binding count; stdout: {}",
        out.stdout
    );
    assert!(
        out.stdout.lines().any(|line| line == "mut i: i64 = 2"),
        "visible binding should show the current shadowing value; stdout: {}",
        out.stdout
    );
    assert!(
        out.stdout.lines().any(|line| line == "mut i: i64 = 3"),
        "assignment should update the displayed current value; stdout: {}",
        out.stdout
    );
    assert!(
        !out.stdout.contains("let i = 1") && !out.stdout.contains("let mut i = 2"),
        "`%bindings` must not show replay source lines; stdout: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains('3'),
        "assignment must still apply to the active shadowing binding; stdout: {}",
        out.stdout
    );
}

#[test]
fn repl_bindings_show_immutable_values_without_let_prefix() {
    let out = run_repl("let i = 3\n%bindings\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.lines().any(|line| line == "i: i64 = 3"),
        "immutable binding should render as `name = value`; stdout: {}",
        out.stdout
    );
    assert!(
        !out.stdout.contains("let i = 3"),
        "`%bindings` must not show the original let source; stdout: {}",
        out.stdout
    );
}

#[test]
fn repl_bindings_show_full_inferred_types() {
    let out = run_repl("let x = #[1,2,3]\nlet words = #[\"a\", \"b\"]\n%bindings\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout
            .lines()
            .any(|line| line == "x: Vec<i64> = #[1, 2, 3]"),
        "Vec binding should show full inferred type; stdout: {}",
        out.stdout
    );
    assert!(
        out.stdout
            .lines()
            .any(|line| line == "words: Vec<String> = #[\"a\", \"b\"]"),
        "Vec<String> binding should show full inferred type; stdout: {}",
        out.stdout
    );
}

#[test]
fn repl_bindings_render_collection_literal_spelling() {
    let out = run_repl(
        "let fixed = [1, 3]\n\
         let map = {\"a\": 1}\n\
         let set = #{1, 2}\n\
         let ordered: BTreeSet<i64> = #{2, 1}\n\
         %bindings\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    for expected in [
        "fixed: [i64; 2] = [1, 3]",
        "map: Map<String, i64> = {\"a\": 1}",
        "set: Set<i64> = #{1, 2}",
        "ordered: BTreeSet<i64> = #{1, 2}",
    ] {
        assert!(
            out.stdout.lines().any(|line| line == expected),
            "%bindings omitted literal spelling `{expected}`: {}",
            out.stdout
        );
    }
}

#[test]
fn repl_bindings_preserve_reference_and_destructured_types() {
    let out = run_repl(
        "struct Pair { left: i64, right: String }\n\
         let mut m = [1, 2, 3, 4]\n\
         let shared_source = [5, 6]\n\
         let shared = &shared_source\n\
         let mut n = [7, 8]\n\
         let exclusive = &mut n\n\
         let a, b = (9, \"ten\")\n\
         let p = Pair { left: 11, right: \"twelve\" }\n\
         let Pair { left, right } = p\n\
         %b\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    for expected in [
        "mut m: [i64; 4] = [1, 2, 3, 4]",
        "shared: &[i64; 2] = &[5, 6]",
        "mut n: [i64; 2] = [7, 8]",
        "exclusive: &mut [i64; 2] = &mut [7, 8]",
        "a: i64 = 9",
        "b: String = \"ten\"",
        "left: i64 = 11",
        "right: String = \"twelve\"",
    ] {
        assert!(
            out.stdout.lines().any(|line| line == expected),
            "missing exact binding `{expected}`; stdout: {}",
            out.stdout
        );
    }
    assert!(
        !out.stdout.contains(": _ =") && !out.stdout.contains("<void>"),
        "valid reference bindings must not lose type or value information: {}",
        out.stdout
    );
}

#[test]
fn repl_bindings_show_owner_after_mut_ref_and_deref_copy() {
    let out = run_repl(
        "let mut a = [1,2,3]\n\
         let b = &mut a\n\
         let c = *b\n\
         %b\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    for expected in [
        "mut a: [i64; 3] = [1, 2, 3]",
        "b: &mut [i64; 3] = &mut [1, 2, 3]",
        "c: [i64; 3] = [1, 2, 3]",
    ] {
        assert!(
            out.stdout.lines().any(|line| line == expected),
            "missing exact binding `{expected}`; stdout: {}",
            out.stdout
        );
    }
    assert!(
        !out.stdout.contains("<unknown>") && !out.stdout.contains("<error:"),
        "valid owner and deref bindings must render cleanly: {}",
        out.stdout
    );
}

#[test]
fn repl_replays_user_mut_self_methods() {
    let out = run_repl(
        "struct Mutable {\n\
             content: Vec<i64>\n\
         }\n\
         impl Mutable {\n\
             fn change(&mut self, index: i64, value: i64) {\n\
                 self.content[index] = value\n\
             }\n\
         }\n\
         let mut m = Mutable { content: #[1, 2, 3] }\n\
         m.change(1, 5)\n\
         println(m)\n\
         %b\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout
            .lines()
            .any(|line| line == "Mutable { content: #[1, 5, 3] }"),
        "println should see the mutation; stdout: {}",
        out.stdout
    );
    assert!(
        out.stdout
            .lines()
            .any(|line| line == "mut m: Mutable = Mutable { content: #[1, 5, 3] }"),
        "%b should replay the user mutator; stdout: {}",
        out.stdout
    );
}

#[test]
fn repl_explain_resolves_binding_types_and_callable_method_capabilities() {
    let out = run_repl(
        "let mut m = [1, 2, 3, 4]\n\
         %e m -d\n\
         let r = &mut m\n\
         %e r -d\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    for expected in [
        "m [binding]\n  type: [i64; 4]\n  capability: mutable binding",
        "r [binding]\n  type: &mut [i64; 4]\n  capability: mutable referent",
        "m.len<T>(self: &[T; N]) -> i64 [method]",
        "m.reverse<T>(self: &mut [T; N]) -> () [method]",
        "r.reverse<T>(self: &mut [T; N]) -> () [method]",
    ] {
        assert!(
            out.stdout.contains(expected),
            "missing binding-aware info `{expected}`; stdout: {}",
            out.stdout
        );
    }
    assert!(
        !out.stdout.contains("m.push(") && !out.stdout.contains("r.push("),
        "info exposed methods unavailable to the binding or fixed array: {}",
        out.stdout
    );
}

#[test]
fn repl_rejects_malformed_let_without_phantom_bindings() {
    let out = run_repl(
        "struct Point { x: i64, y: i64 }\n\
         let a = 1\n\
         let b = 3\n\
         let Point {a, b}\n\
         %bindings\n\
         let p Point {a, b}\n\
         p\n\
         %bindings\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stderr
            .contains("malformed `let` input: missing `=` initializer; write `let PAT = EXPR`"),
        "malformed let inputs should be rejected clearly; stderr: {}",
        out.stderr
    );
    assert!(
        out.stderr.contains("cannot find `p` in this scope"),
        "`p` should not be registered after malformed let input; stderr: {}",
        out.stderr
    );
    assert!(
        out.stdout.lines().any(|line| line == "a: i64 = 1")
            && out.stdout.lines().any(|line| line == "b: i64 = 3"),
        "valid bindings should remain visible; stdout: {}",
        out.stdout
    );
    assert!(
        !out.stdout.contains("p:") && !out.stdout.contains("<void>"),
        "malformed let inputs must not create phantom bindings; stdout: {}",
        out.stdout
    );
}

#[test]
fn repl_rejects_literal_let_patterns_and_remains_live() {
    let out = run_repl("let 9 = 8\n9\n");
    assert!(
        out.success,
        "REPL should remain live; stderr: {}",
        out.stderr
    );
    assert!(
        out.stderr.contains("cannot assign to a literal"),
        "literal let should report the type-system error; stderr: {}",
        out.stderr
    );
    assert!(
        out.stdout.lines().any(|line| line.trim() == "9"),
        "REPL should evaluate the next input; stdout: {}",
        out.stdout
    );
}

#[test]
fn repl_rejects_push_on_fixed_array_binding() {
    let out = run_repl("let mut a = [1;3]\na.push(3)\n%bindings\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stderr
            .contains("method `push` changes sequence length or capacity and requires `Vec<T>`"),
        "fixed array push should be rejected; stderr: {}",
        out.stderr
    );
    assert!(
        out.stdout
            .lines()
            .any(|line| line == "mut a: [i64; 3] = [1, 1, 1]"),
        "fixed array binding should remain fixed-size; stdout: {}",
        out.stdout
    );
    assert!(
        !out.stdout.contains("[1, 1, 1, 3]"),
        "fixed array push should not produce a grown array; stdout: {}",
        out.stdout
    );
}

#[test]
fn repl_bindings_and_declarations_accept_regex_filters() {
    let out = run_repl(
        "let alpha_value = 11\n\
         let beta_value = 22\n\
         fn AlphaFn(value: i64) -> i64 { value }\n\
         fn BetaFn(value: String) -> String { value }\n\
         %bindings ^alpha_value$\n\
         %declarations ^AlphaFn$\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout
            .lines()
            .any(|line| line == "alpha_value: i64 = 11"),
        "binding regex should match binding names: {}",
        out.stdout
    );
    assert!(
        !out.stdout.contains("beta_value = 22"),
        "binding regex should exclude non-matches: {}",
        out.stdout
    );
    assert!(
        out.stdout
            .lines()
            .any(|line| line == "fn AlphaFn(value: i64) -> i64 { value }"),
        "declaration regex should match declaration names: {}",
        out.stdout
    );
    assert!(
        !out.stdout
            .contains("fn BetaFn(value: String) -> String { value }"),
        "declaration regex should exclude non-matches: {}",
        out.stdout
    );
}

#[test]
fn repl_bindings_and_declarations_report_invalid_regex() {
    let out = run_repl("let value = 1\nfn value_of() -> i64 { 1 }\n%b [\n%d [\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stderr.contains("invalid bindings regex `[`"),
        "invalid binding regex should produce a useful error: {}",
        out.stderr
    );
    assert!(
        out.stderr.contains("invalid declarations regex `[`"),
        "invalid declaration regex should produce a useful error: {}",
        out.stderr
    );
}

#[test]
fn repl_struct_construction_and_display_match_source_shapes() {
    let out = run_repl(
        "struct Pair { x: i64, y: i64 }\n\
         struct Tup(String, i64)\n\
         let p = Pair { x: 0, y: 0 }\n\
         p\n\
         let t = Tup(\"row\", 7)\n\
         t\n\
         %declarations\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        !out.stdout.contains("binding added"),
        "default REPL output should omit binding chatter; stdout: {}; stderr: {}",
        out.stdout,
        out.stderr
    );
    assert!(
        out.stdout.contains("Pair { x: 0, y: 0 }"),
        "named struct value should render with fields; stdout: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("Tup(\"row\", 7)"),
        "tuple struct value should render with parentheses; stdout: {}",
        out.stdout
    );
    assert!(
        out.stdout
            .lines()
            .any(|line| line == "struct Pair { x: i64, y: i64 }"),
        "%declarations should list accumulated declarations; stdout: {}",
        out.stdout
    );
    assert!(
        out.stdout
            .lines()
            .any(|line| line == "struct Tup(String, i64)"),
        "%declarations should list tuple struct declarations; stdout: {}",
        out.stdout
    );

    let legacy = run_repl_args("struct Marker\n", &["-v"]);
    assert!(
        legacy.stdout.contains("added 1 declarations"),
        "bare unit declarations should be accepted; stdout: {}; stderr: {}",
        legacy.stdout,
        legacy.stderr
    );
}

#[test]
fn repl_constructs_unit_and_empty_named_structs_with_unit_like_syntax() {
    let out = run_repl(
        "struct Unit\n\
         struct Empty {}\n\
         let unit = Unit\n\
         unit\n\
         let also_unit = Unit {}\n\
         also_unit\n\
         let empty = Empty {}\n\
         empty\n\
         let bad_unit = Unit()\n\
         let bare_empty = Empty\n\
         bare_empty\n",
    );
    assert!(out.success, "REPL should remain live: {}", out.stderr);
    assert!(out.stdout.contains("Unit {  }"), "{}", out.stdout);
    assert!(out.stdout.contains("Unit {  }"), "{}", out.stdout);
    assert!(out.stdout.contains("Empty {  }"), "{}", out.stdout);
    assert!(
        out.stderr
            .contains("struct `Unit` must be constructed with braces"),
        "{}",
        out.stderr
    );
    assert!(!out.stderr.contains("struct `Empty`"), "{}", out.stderr);
}

#[test]
fn repl_rejects_plain_tuple_for_tuple_struct_function_parameter() {
    let out = run_repl(
        "struct RGB(i64, i64, i64)\n\
         let color = RGB(0, 100, 100)\n\
         let three = (1, 500, -200)\n\
         fn print_color(color: RGB) { println(\"{}\", color) }\n\
         print_color(color)\n\
         print_color(three)\n",
    );
    assert!(
        out.success,
        "repl should remain live; stderr: {}",
        out.stderr
    );
    assert!(
        out.stdout.contains("RGB(0, 100, 100)"),
        "valid nominal argument did not execute: {}",
        out.stdout
    );
    assert!(
        out.stderr
            .contains("expected `RGB`, found `(i64, i64, i64)`"),
        "nominal mismatch was not reported: {}",
        out.stderr
    );
    assert!(
        !out.stdout.contains("(1, 500, -200)"),
        "rejected call still executed: {}",
        out.stdout
    );
}

#[test]
fn repl_open_ranges_are_lazy_and_printable() {
    let out =
        run_repl("use std::iter\n10..\n..10\n..=10\n(10..).take(5) |> iter::collect()\n10..=\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    for expected in ["10..", "..10", "..=10", "[10, 11, 12, 13, 14]"] {
        assert!(
            out.stdout.contains(expected),
            "missing `{expected}`; stdout: {}",
            out.stdout
        );
    }
    assert!(
        out.stderr
            .contains("inclusive range operator `..=` requires an upper bound"),
        "missing precise inclusive-range diagnostic; stderr: {}",
        out.stderr
    );
}

#[test]
fn repl_prints_runtime_error_without_crashing() {
    let out = run_repl("let answer = 41\npanic(\"boom\")\nanswer + 1\n");
    assert!(
        out.success,
        "repl should keep running after a runtime panic; stderr: {}",
        out.stderr
    );
    // The error line goes to stderr ("error: ..."); the recovery
    // expression's result lands on stdout. Both must be present.
    assert!(
        out.stderr.contains("boom") || out.stderr.contains("GX0005"),
        "panic message missing from stderr: {}",
        out.stderr
    );
    assert!(
        out.stdout.contains('2'),
        "REPL did not recover after the panic; stdout: {}",
        out.stdout
    );
}

#[test]
fn repl_constructs_empty_named_struct_from_its_bare_name() {
    let out = run_repl("struct Unit {}\nlet u = Unit\nu\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(out.stderr.is_empty(), "stderr: {}", out.stderr);
    assert!(out.stdout.contains("Unit"), "stdout: {}", out.stdout);
}

#[test]
fn repl_handles_empty_input() {
    let out = run_repl("");
    assert!(
        out.success,
        "empty stdin should close cleanly with exit 0; stderr: {}",
        out.stderr
    );
    assert!(
        !out.stdout.lines().any(|line| line == "1"),
        "no expression should have been evaluated; stdout: {}",
        out.stdout
    );
}

#[test]
fn repl_handles_syntax_error_recovery() {
    let out = run_repl("let z = @@@\n1 + 2\n");
    assert!(
        out.success,
        "repl must survive a syntax error and exit zero; stderr: {}",
        out.stderr
    );
    // The bad line should not prevent the following successful value.
    assert!(
        out.stdout.contains('3'),
        "good input after a syntax error did not evaluate; stdout: {}\nstderr: {}",
        out.stdout,
        out.stderr
    );
}

#[test]
fn repl_accepts_optional_line_ending_semicolons() {
    let out = run_repl("let x = 9;\n%b\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stderr.is_empty(),
        "optional semicolon must not produce a diagnostic:\n{}",
        out.stderr
    );
    assert!(
        out.stdout.lines().any(|line| line == "x: i64 = 9"),
        "semicolon-terminated binding was not persisted:\n{}",
        out.stdout
    );
}

#[test]
fn repl_accepts_semicolons_between_same_line_statements() {
    let out = run_repl("println(1); println(2)\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(out.stderr.is_empty(), "stderr: {}", out.stderr);
    assert!(out.stdout.contains("\n1\n2\n"), "stdout: {}", out.stdout);
}

#[test]
fn repl_persists_same_line_semicolon_separated_lets() {
    let out = run_repl("let x = 2;let y = 1\nx + y\n%bindings\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(out.stderr.is_empty(), "stderr: {}", out.stderr);
    assert!(out.stdout.contains("\n3\n"), "stdout: {}", out.stdout);
    assert!(out.stdout.contains("x: i64 = 2"), "stdout: {}", out.stdout);
    assert!(out.stdout.contains("y: i64 = 1"), "stdout: {}", out.stdout);
}

#[test]
fn repl_evaluates_function_definition() {
    let out = run_repl_args(
        "fn add(a: i64, b: i64) -> i64 { a + b }\nadd(1, 2)\n",
        &["-v"],
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("added 1 declarations"),
        "expected declaration confirmation; stdout: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains('3'),
        "user-defined fn was not callable from the next input; stdout: {}",
        out.stdout
    );
}

#[test]
fn repl_accepts_derived_struct_declaration() {
    let out = run_repl_args(
        "#[derive(PartialEq)] struct Point { x: i64, y: i64 }\n\
         let p1 = Point { x: 1, y: 2 }\n\
         let p2 = Point { x: 1, y: 2 }\n\
         println(p1 == p2)\n",
        &["-v"],
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("added 1 declarations"),
        "attributed struct should be stored as a declaration; stdout: {}; stderr: {}",
        out.stdout,
        out.stderr
    );
    assert!(
        out.stdout.contains("true"),
        "derived equality should be callable in the REPL; stdout: {}; stderr: {}",
        out.stdout,
        out.stderr
    );
    assert!(
        !out.stderr.contains("nested items"),
        "attributed struct was interpreted as a nested item: {}",
        out.stderr
    );
}

#[test]
fn repl_accepts_impl_block_declaration() {
    let out = run_repl_args(
        "struct Point { x: i64, y: i64 }\n\
         impl Point { fn total(self) -> i64 { self.x + self.y } }\n\
         let point = Point { x: 20, y: 22 }\n\
         point.total()\n",
        &["-v"],
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("added 2 declarations"),
        "impl block should be stored as a declaration; stdout: {}; stderr: {}",
        out.stdout,
        out.stderr
    );
    assert!(
        out.stdout.contains("42"),
        "impl method should be callable from later input; stdout: {}; stderr: {}",
        out.stdout,
        out.stderr
    );
    assert!(
        !out.stderr.contains("nested items"),
        "impl block was interpreted as a nested item: {}",
        out.stderr
    );
}

#[test]
fn repl_hash_set_bindings_show_and_iterate_stored_structs() {
    let out = run_repl(
        "#[derive(Debug, PartialEq, Eq)] struct Point { x: i64, y: i64 }\n\
         let mut set = Set::new()\n\
         let p1 = Point { x: 1, y: 2 }\n\
         let p2 = Point { x: 3, y: 4 }\n\
         impl Point { fn total(self) -> i64 { self.x + self.y } }\n\
         set.insert(p1)\n\
         set.insert(p2)\n\
         %bindings\n\
         for point in set { println(point) }\n\
         %explain set\n\
         set.map(|point| point.x)\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        !out.stderr.contains("nested items"),
        "impl block was interpreted as a nested item: {}",
        out.stderr
    );
    assert!(
        !out.stdout.contains("__set"),
        "internal set handle leaked into user output: {}",
        out.stdout
    );
    for point in ["Point { x: 1, y: 2 }", "Point { x: 3, y: 4 }"] {
        assert!(
            out.stdout.matches(point).count() >= 2,
            "`%bindings` and iteration should both show {point}: {}",
            out.stdout
        );
    }
    assert!(
        out.stdout.contains("mut set: Set<Point> ="),
        "`%explain set` should render the binding as `%bindings` does, with the \
         element type the stored values gave it: {}",
        out.stdout
    );
    assert!(
        out.stderr.contains("iterator operation"),
        "a set has no sequence surface, so `map` names the `iter()` spelling: {}",
        out.stderr
    );
}

#[test]
fn repl_executes_nested_function_items() {
    let out = run_repl(
        "fn outer(n: i64) -> i64 { fn double(x: i64) -> i64 { x * 2 } double(n) }\n\
         outer(21)\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("42"),
        "nested function did not execute in the REPL; stdout: {}; stderr: {}",
        out.stdout,
        out.stderr
    );
}

#[test]
fn repl_constructs_nested_struct_items() {
    let out = run_repl(
        "fn answer() -> i64 { struct Pair { left: i64, right: i64 } let p = Pair { left: 20, right: 22 }; println(p); p.left + p.right }\n\
         answer()\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("42"),
        "nested struct did not execute in the REPL; stdout: {}; stderr: {}",
        out.stdout,
        out.stderr
    );
    assert!(
        out.stdout.contains("Pair { left: 20, right: 22 }")
            && !out.stdout.contains("__gos_nested_"),
        "nested struct leaked its backend symbol; stdout: {}",
        out.stdout
    );
}

#[test]
fn repl_meta_quit_terminates_with_exit_zero() {
    let out = run_repl("%quit\n");
    assert!(
        out.success,
        "%quit should exit zero; stderr: {}",
        out.stderr
    );
    assert!(
        !out.stdout.lines().any(|line| line == "2"),
        "no expression should evaluate before %quit; stdout: {}",
        out.stdout
    );
}

#[test]
fn repl_drop_ends_reference_lifetime_and_preserves_mutation() {
    let out = run_repl(
        "let mut v = Vec::from([1, 2])\n\
         let a = &mut v\n\
         a[0] = 9\n\
         let answer = 42\n\
         v[0]\n\
         %drop a\n\
         v[0]\n\
         answer\n\
         %bindings\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stderr
            .contains("cannot read `v` while reference `a` is active"),
        "the reference should remain exclusive before `%drop`: {}",
        out.stderr
    );
    assert!(
        out.stdout.contains("dropped `a`"),
        "`%drop` should confirm the ended binding: {}",
        out.stdout
    );
    assert!(
        out.stdout.lines().any(|line| line == "9"),
        "mutation through the dropped reference should remain visible: {}",
        out.stdout
    );
    assert!(
        out.stdout.lines().any(|line| line == "42") && out.stdout.contains("answer: i64 = 42"),
        "bindings created after the reference should survive `%drop`: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("mut v: Vec<i64> = #[9, 2]"),
        "the source should remain available with its final value: {}",
        out.stdout
    );
    assert!(
        !out.stdout.lines().any(|line| line.starts_with("a:")),
        "the dropped binding should not remain in `%bindings`: {}",
        out.stdout
    );
}

#[test]
fn repl_drop_cascades_dependent_references_without_internal_errors() {
    let out = run_repl(
        "let mut v = Vec::from([1, 2])\n\
         let a = &mut v\n\
         let b = &a\n\
         v[0]\n\
         %drop a\n\
         v[0]\n\
         %bindings\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stderr
            .contains("cannot read `v` while reference `a` is active"),
        "the source should be locked before `%drop a`: {}",
        out.stderr
    );
    assert!(
        out.stdout.contains("dropped `a` and dependent `b`"),
        "`%drop a` should report the dependent binding it removed: {}",
        out.stdout
    );
    assert!(
        out.stdout.lines().any(|line| line == "1"),
        "`v` should be readable after dropping `a`: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("mut v: Vec<i64> = #[1, 2]")
            && !out.stdout.lines().any(|line| line.starts_with("a:"))
            && !out.stdout.lines().any(|line| line.starts_with("b:")),
        "`%bindings` should keep only the source binding: {}",
        out.stdout
    );
    assert!(
        !out.stderr.contains("reference cannot be copied")
            && !out.stderr.contains("parse error")
            && !out.stderr.contains("unexpected keyword"),
        "`%drop a` should not expose internal replay diagnostics: {}",
        out.stderr
    );
}

#[test]
fn repl_drop_leaf_reference_keeps_source_locked_until_owner_drop() {
    let out = run_repl(
        "let mut v = Vec::from([1, 2])\n\
         let a = &mut v\n\
         let b = &a\n\
         %drop b\n\
         v[0]\n\
         %drop a\n\
         v[0]\n\
         %bindings\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("dropped `b`") && out.stdout.contains("dropped `a`"),
        "`%drop` should report leaf and owner drops clearly: {}",
        out.stdout
    );
    assert!(
        out.stderr
            .contains("cannot read `v` while reference `a` is active"),
        "dropping `b` should not implicitly drop `a`: {}",
        out.stderr
    );
    assert!(
        out.stdout.lines().any(|line| line == "1")
            && out.stdout.contains("mut v: Vec<i64> = #[1, 2]")
            && !out.stdout.lines().any(|line| line.starts_with("a:"))
            && !out.stdout.lines().any(|line| line.starts_with("b:")),
        "`v` should become readable only after dropping `a`: {}",
        out.stdout
    );
    assert!(
        !out.stderr.contains("parse error") && !out.stderr.contains("unexpected keyword"),
        "`%drop b` should not expose parser diagnostics: {}",
        out.stderr
    );
}

#[test]
fn repl_drop_validates_its_binding_name() {
    let out = run_repl("%drop\n%drop missing\n%drop one two\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert_eq!(
        out.stderr.matches("usage: %drop NAME").count(),
        2,
        "empty and extra arguments should show usage: {}",
        out.stderr
    );
    assert!(
        out.stderr
            .contains("no persistent binding or declaration named `missing`"),
        "an unknown name should be diagnosed: {}",
        out.stderr
    );
}

#[test]
fn repl_drop_ends_a_declaration_so_its_name_is_free() {
    let out = run_repl(
        "fn f() -> i64 { 1 }\n\
         f()\n\
         %drop f\n\
         fn f() -> i64 { 2 }\n\
         f()\n\
         %declarations\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("dropped `f`"),
        "`%drop f` should confirm the ended declaration: {}",
        out.stdout
    );
    assert!(
        !out.stderr.contains("defined multiple times"),
        "redeclaring after `%drop` should not collide: {}",
        out.stderr
    );
    assert!(
        out.stdout.lines().any(|line| line == "2"),
        "the redeclared `f` should answer: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("fn f() -> i64 { 2 }") && !out.stdout.contains("fn f() -> i64 { 1 }"),
        "`%declarations` should list only the redeclared `f`: {}",
        out.stdout
    );
}

#[test]
fn repl_drop_declaration_cascades_to_declarations_that_need_it() {
    let out = run_repl(
        "struct Point { x: i64, y: i64 }\n\
         impl Point { fn sum(&self) -> i64 { self.x + self.y } }\n\
         Point { x: 1, y: 2 }.sum()\n\
         %drop Point\n\
         %declarations\n\
         struct Point { x: i64 }\n\
         Point { x: 9 }.x\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("dropped `Point`"),
        "`%drop Point` should confirm the ended declaration: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("no declarations yet"),
        "the orphaned `impl Point` should go with its type: {}",
        out.stdout
    );
    assert!(
        out.stdout.lines().any(|line| line == "9"),
        "the redeclared `Point` should be usable: {}",
        out.stdout
    );
}

#[test]
fn repl_drop_declaration_reports_the_dependents_it_removed() {
    let out = run_repl(
        "fn base() -> i64 { 1 }\n\
         fn uses() -> i64 { base() + 1 }\n\
         uses()\n\
         %drop base\n\
         %declarations\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("dropped `base` and dependent `uses`"),
        "`%drop base` should name the dependent declaration: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("no declarations yet"),
        "both declarations should be gone: {}",
        out.stdout
    );
}

#[test]
fn repl_drop_declaration_keeps_a_binding_that_still_needs_it() {
    let out = run_repl(
        "struct P { x: i64 }\n\
         let p = P { x: 1 }\n\
         %drop P\n\
         p.x\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stderr.contains("cannot drop `P`"),
        "a binding still using the type should block the drop: {}",
        out.stderr
    );
    assert!(
        out.stdout.lines().any(|line| line == "1"),
        "the binding should survive the refused drop: {}",
        out.stdout
    );
}

#[test]
fn repl_only_accepts_documented_quit_commands() {
    let out = run_repl("%exit\n1 + 2\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stderr.contains("unknown meta-command: %exit"),
        "%exit should not terminate the REPL; stderr: {}",
        out.stderr
    );
    assert!(
        out.stdout.contains('3'),
        "the REPL did not continue after %exit; stdout: {}",
        out.stdout
    );
}

#[test]
fn repl_meta_help_rejects_symbol_queries() {
    let out = run_repl("%help strings::trim\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stderr.contains("usage: %help"),
        "%help arguments should be rejected: {}",
        out.stderr
    );
    assert!(
        !out.stdout.contains("std::strings::trim [fn]"),
        "%help should not perform symbol lookup: {}",
        out.stdout
    );
}

#[test]
fn repl_meta_command_shortcuts_match_their_long_forms() {
    let out = run_repl(
        "let answer = 42\n\
         fn identity(value: i64) -> i64 { value }\n\
         %b\n\
         %d\n\
         %i strings\n\
         %info strings::trim\n\
         %r\n\
         %b\n\
         %d\n\
         %q\n\
         1 + 1\n",
    );
    assert!(out.success, "shortcut session failed: {}", out.stderr);
    assert!(
        out.stdout.contains("answer: i64 = 42"),
        "%b did not render bindings: {}",
        out.stdout
    );
    assert!(
        out.stdout
            .contains("fn identity(value: i64) -> i64 { value }"),
        "%d did not render declarations: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("std::strings::trim"),
        "%i or %help did not reach stdlib discovery: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("session cleared")
            && out.stdout.contains("no `let` bindings yet")
            && out.stdout.contains("no declarations yet"),
        "%r did not clear session state: {}",
        out.stdout
    );
    assert!(
        !out.stdout.lines().any(|line| line == "2"),
        "%q did not stop before the trailing expression: {}",
        out.stdout
    );
}

#[test]
fn repl_meta_find_is_removed() {
    let out = run_repl("%find String::parse\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stderr.contains("unknown meta-command: %find"),
        "%find should no longer be a REPL command: {}",
        out.stderr
    );
}

#[test]
fn repl_iter_receiver_methods_pipe_dotdot_and_range_index_work() {
    let out = run_repl(
        "use std::iter\n\
         let a: Vec<i64> = Vec::from([1, 2, 3, 4, 5])\n\
         a.iter().skip(2).collect()\n\
         a.iter().enumerate().collect()\n\
         a.iter().zip(0..).collect()\n\
         a |> |v| iter::zip(.., v).collect()\n\
         a[..2]\n\
         Vec::from([1, 1, 2, 2]).iter().dedup()\n\
         a.iter().windows(2)\n\
         a.iter().chunks(2)\n\
         a.iter().pairwise()\n\
         Vec::from([Vec::from([1, 2]), Vec::from([3])]).iter().flatten()\n\
         a.iter().rev().collect()\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    for expected in [
        "#[3, 4, 5]",
        "#[(0, 1), (1, 2), (2, 3), (3, 4), (4, 5)]",
        "#[(1, 0), (2, 1), (3, 2), (4, 3), (5, 4)]",
        "#[(0, 1), (1, 2), (2, 3), (3, 4), (4, 5)]",
        "#[1, 2]",
        "#[1, 2]",
        "#[#[1, 2], #[2, 3], #[3, 4], #[4, 5]]",
        "#[#[1, 2], #[3, 4], #[5]]",
        "#[(1, 2), (2, 3), (3, 4), (4, 5)]",
        "#[1, 2, 3]",
        "#[5, 4, 3, 2, 1]",
    ] {
        assert!(
            out.stdout.contains(expected),
            "missing `{expected}` from iterator regression output: {}",
            out.stdout
        );
    }
}

#[test]
fn repl_iter_take_rejects_negative_counts() {
    let out = run_repl("let a: Vec<i64> = Vec::from([1, 2, 3])\na.take(-2)\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stderr.contains("count must be non-negative"),
        "negative take should be rejected instead of clamped: {}",
        out.stderr
    );
    assert!(
        !out.stdout.contains("[]"),
        "negative take must not silently return an empty Vec: {}",
        out.stdout
    );
}

#[test]
fn repl_rejects_negative_size_arguments_across_stdlib() {
    let out = run_repl(
        "use std::{image, iter, strings, time}\n\
         strings::repeat(\"x\", -1)\n\
         strings::splitn(\"a,b\", -1, \",\")\n\
         strings::pad_left(\"x\", -1, ' ')\n\
         strings::replacen(\"aaa\", \"a\", \"b\", -1)\n\
         let xs: Vec<i64> = Vec::from([1, 2, 3])\n\
         xs.take(-1)\n\
         xs.step_by(-1)\n\
         xs.windows(-1)\n\
         iter::repeat(1, -1)\n\
         let v: Vec<i64> = Vec::with_capacity(-1)\n\
         String::with_capacity(-1)\n\
         let m: Map<String, i64> = Map::with_capacity(-1)\n\
         image::new(-1, 1)\n\
         time::sleep(-1)\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    for expected in [
        "strings::repeat: count must be non-negative",
        "strings::splitn: count must be non-negative",
        "strings::pad_left: width must be non-negative",
        "strings::replacen: count must be non-negative",
        "Vec::take: count must be non-negative",
        "Vec::step_by: count must be non-negative",
        "iter::windows: count must be non-negative",
        "iter::repeat: count must be non-negative",
        "Vec::with_capacity: capacity must be non-negative",
        "String::with_capacity: capacity must be non-negative",
        "Map::with_capacity: capacity must be non-negative",
        "image::new: width must be non-negative",
        "time::sleep: duration_ms must be non-negative",
    ] {
        assert!(
            out.stderr.contains(expected),
            "missing `{expected}` from negative-size regression stderr: {}",
            out.stderr
        );
    }
    for forbidden in ["\"\"", "[]", "[]", "[]", "[]", "\"\""] {
        assert!(
            !out.stdout.contains(forbidden),
            "negative size argument was silently accepted: {}",
            out.stdout
        );
    }
}

#[test]
fn repl_vec_slice_rejects_bad_arity_and_argument_types() {
    let out = run_repl(
        "let a = #[1, 2, 3, 4, 5]\n\
         a.slice(1, 3)\n\
         a.slice(1..3)\n\
         a.slice(..3)\n\
         a.slice(..)\n\
         a.slice(2)\n\
         a.slice(\"two\")\n\
         a.slice(\"two\", 3)\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("Ok(#[2, 3])"),
        "valid Vec::slice call should still work: {}",
        out.stdout
    );
    assert!(
        out.stderr.contains("type mismatch"),
        "non-integer slice bounds should be rejected by typing: {}",
        out.stderr
    );
    assert!(
        out.stderr.contains("Vec::slice") && out.stderr.contains("takes 2 argument"),
        "wrong Vec::slice arity should be diagnosed: {}",
        out.stderr
    );
    assert!(
        !out.stdout.contains("Ok([])"),
        "invalid Vec::slice calls must not silently return Ok([]): {}",
        out.stdout
    );
}

#[test]
fn repl_meta_info_finds_stdlib_symbol() {
    let out = run_repl("%info strings::trim -d\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout
            .contains("std::strings::trim(text: String) -> String [fn]"),
        "expected qualified stdlib item help; stdout: {}",
        out.stdout
    );
    assert!(
        out.stdout
            .contains("Removes leading and trailing whitespace."),
        "expected manifest doc text; stdout: {}",
        out.stdout
    );
}

#[test]
fn repl_meta_info_shows_complete_signatures_for_string_methods() {
    let out = run_repl("%i *starts_with -d\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout
            .contains("String::starts_with(self: String, needle: String | char) -> bool [method]"),
        "String method signature should name every parameter and return type: {}",
        out.stdout
    );
    assert!(
        !out.stdout.contains("fn starts_with(self, ...) -> ..."),
        "String method signature must not use placeholders: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains(
            "std::strings::starts_with(text: String, needle: String | char) -> bool [fn]"
        ) && out
            .stdout
            .contains("std::path::starts_with(path: String, prefix: String) -> bool [fn]"),
        "stdlib overloads should retain their complete signatures: {}",
        out.stdout
    );
}

#[test]
fn repl_meta_info_plain_text_searches_public_symbol_paths() {
    let out = run_repl("%i *start*\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    for expected in [
        "std::strings::starts_with(",
        "String::starts_with(",
        "std::strings::trim_start(",
    ] {
        assert!(
            out.stdout.contains(expected),
            "%i start omitted `{expected}`: {}",
            out.stdout
        );
    }
}

#[test]
fn repl_meta_info_renders_matching_modules_once() {
    let out = run_repl("%i gzip\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert_eq!(
        out.stdout.matches("std::compress::gzip [module]").count(),
        1,
        "%i should not duplicate matching module entries: {}",
        out.stdout
    );
    for expected in [
        "std::compress::gzip::encode(",
        "std::compress::gzip::decode(",
        "std::compress::gzip::Level [type]",
    ] {
        assert!(
            out.stdout.contains(expected),
            "%i should list members of a matching module, omitting `{expected}`: {}",
            out.stdout
        );
    }
}

#[test]
fn repl_info_lists_all_matches_without_pagination_prompt() {
    let out = run_repl("%i String\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    let total = out
        .stdout
        .lines()
        .filter(|line| line.ends_with(']') && line.contains(" ["))
        .count();
    assert!(total > 20, "String should list every match: {}", out.stdout);
    assert!(
        !out.stdout.contains("Use `%i String -p"),
        "pagination prompt should be gone: {}",
        out.stdout
    );
}

#[test]
fn repl_listing_commands_reject_removed_pagination_options() {
    let out = run_repl("let answer = 42\n%i iter --all\n%b --all\n%d --all\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stderr
            .matches("pagination options were removed; use a pattern to filter results")
            .count()
            >= 3,
        "removed pagination options should be rejected: {}",
        out.stderr
    );
}

/// The strict parse family is the one parse surface, and `%info` answers
/// for each spelling: the method form and the free form both.
#[test]
fn repl_meta_help_covers_the_strict_parse_family() {
    let out = run_repl(
        "%info String::to_i64 -d\n\
         %info to_f64 -d\n\
         \"123\".to_i64()\n\
         \"1.5\".to_f64()\n\
         \"true\".to_bool()\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("String::to_i64"),
        "expected the method help; stdout: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("Some(123)")
            && out.stdout.contains("Some(1.5)")
            && out.stdout.contains("Some(true)"),
        "the strict parses answer an Option; stdout: {}",
        out.stdout
    );
}

/// `String::parse` is retired: it reports GT0080 with the `to_T` rewrite
/// the expected type picks, and still types as it did so the rest of the
/// line is diagnosed on its own terms.
#[test]
fn repl_string_parse_reports_the_strict_rewrite() {
    let out = run_repl("let a = \"12\".parse<i64>()\n");
    assert!(
        out.stderr.contains("GT0080"),
        "expected the retired-parse diagnostic; stderr: {}",
        out.stderr
    );
    assert!(
        out.stderr.contains("to_i64"),
        "expected the rewrite to name `to_i64`; stderr: {}",
        out.stderr
    );
}

#[test]
fn repl_meta_help_covers_builtin_receiver_types() {
    let out = run_repl(
        "%info Option::map -d\n\
         %info Result::map_err -d\n\
         %info AtomicI64::new -d\n\
         %info validate::Errors::new -d\n\
         %info Option -d\n\
         %info http::Response -d\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    for expected in [
        "Option::map<",
        "Result::map_err<",
        "AtomicI64::new(",
        "validate::Errors::new(",
    ] {
        assert!(
            out.stdout.contains(expected),
            "missing `{expected}` from builtin receiver metadata output: {}",
            out.stdout
        );
    }
    assert!(
        out.stderr.is_empty(),
        "builtin receiver metadata lookups should not fail: {}",
        out.stderr
    );
}

#[test]
fn repl_meta_info_covers_payload_and_sequence_method_gaps() {
    let out = run_repl(
        "%i unwrap -d\n\
         %i Result::unwrap_or -d\n\
         %i Option::ok_or -d\n\
         %i Vec::take_while -d\n\
         %i Vec::iter -d\n\
         %i Vec::partition -d\n\
         %i Vec::unzip -d\n\
         %i Vec::scan -d\n\
         %i Vec::chunk_by -d\n\
         %i Vec::filter_map -d\n\
         %i Map::partition -d\n\
         %i Map::count_by -d\n\
         %i BTreeMap::filter_map -d\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    for expected in [
        "Option::unwrap<T>(self: Option<T>) -> T [method]",
        "Result::unwrap<T, E>(self: Result<T, E>) -> T [method]",
        "Result::unwrap_or<T, E>(self: Result<T, E>, fallback: T) -> T [method]",
        "Option::ok_or<T, E>(self: Option<T>, err: E) -> Result<T, E> [method]",
        "Vec::take_while<T>(self: Vec<T>, f: fn(T) -> bool) -> Vec<T> [method]",
        "Vec::iter<T>(self: Vec<T>) -> Iterator<T> [method]",
        "Vec::partition<T>(self: Vec<T>, predicate: Fn(T) -> bool) -> (Vec<T>, Vec<T>) [method]",
        "Vec::unzip<A, B>(self: Vec<(A, B)>) -> (Vec<A>, Vec<B>) [method]",
        "Vec::scan<T, S>(self: Vec<T>, init: S, f: Fn(S, T) -> S) -> Vec<S> [method]",
        "Vec::chunk_by<T, K: Eq>(self: Vec<T>, key: Fn(T) -> K) -> Map<K, Vec<T>> [method]",
        "Vec::filter_map<T, U>(self: Vec<T>, f: Fn(T) -> Option<U>) -> Vec<U> [method]",
        "Map::partition<K, V>(self: Map<K, V>, predicate: Fn((K, V)) -> bool) -> (Vec<(K, V)>, Vec<(K, V)>) [method]",
        "Map::count_by<K, V, B: Eq>(self: Map<K, V>, key: Fn((K, V)) -> B) -> Map<B, i64> [method]",
        "BTreeMap::filter_map<K, V, U>(self: BTreeMap<K, V>, f: Fn((K, V)) -> Option<U>) -> Vec<U> [method]",
        "Example: values.partition(|value| value > 0)",
        "Example: map.partition(|(key, value)| value > 0)",
    ] {
        assert!(
            out.stdout.contains(expected),
            "missing `{expected}` from method metadata output: {}",
            out.stdout
        );
    }
    assert!(
        !out.stdout.contains("Result::unwrap_or [method]")
            && !out.stdout.contains("Vec::take_while [method]")
            && !out.stdout.contains("Vec::partition [method]")
            && !out.stdout.contains("Vec::chunk_by [method]")
            && !out.stdout.contains("Map::partition [method]")
            && !out.stdout.contains("BTreeMap::filter_map [method]"),
        "method metadata must not fall back to bare names: {}",
        out.stdout
    );
    assert!(
        out.stderr.is_empty(),
        "method metadata lookups should not fail: {}",
        out.stderr
    );
}

#[test]
fn repl_question_mark_rejects_invalid_contexts_before_execution() {
    let out = run_repl(
        "let bad_operand = \"12\"?.to_i64()\n\
         let bad_context: u8 = \"12\".to_i64()?\n\
         let malformed u8 = \"12\".to_i64()?\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stderr
            .matches("the `?` operator cannot be used with")
            .count()
            >= 2,
        "invalid question-mark uses must be type errors; stderr: {}",
        out.stderr
    );
    assert!(
        !out.stderr.contains("GX0005") && !out.stderr.contains("GX0007"),
        "invalid question-mark uses must not reach runtime errors; stderr: {}",
        out.stderr
    );
}

#[test]
fn repl_lists_a_builtin_by_the_form_source_calls_it_by() {
    let out = run_repl("%info fmt\n%info println\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("std::fmt::println [builtin]"),
        "a builtin is listed as it is called; stdout: {}",
        out.stdout
    );
    assert!(
        !out.stdout.contains("println!"),
        "no discovery surface spells a builtin with a sigil; stdout: {}",
        out.stdout
    );
    assert!(
        out.stdout.matches("std::fmt::println [builtin]").count() >= 2,
        "the calling form searches back to the same item; stdout: {}",
        out.stdout
    );
}

#[test]
fn repl_meta_help_covers_every_builtin_macro_and_prelude_assertion() {
    let mut input = String::new();
    for builtin in gossamer_parse::builtin_macros::BUILTIN_MACROS {
        writeln!(&mut input, "%info {} -d", builtin.name).expect("write macro-info input");
    }
    input.push_str("%info assert -d\n%info assert_eq -d\n");
    let out = run_repl(&input);
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    for builtin in gossamer_parse::builtin_macros::BUILTIN_MACROS {
        assert!(
            out.stdout.contains(builtin.signature),
            "missing help for {}: {}",
            builtin.name,
            out.stdout
        );
        assert!(
            out.stdout.contains(builtin.signature),
            "missing signature for {}: {}",
            builtin.name,
            out.stdout
        );
    }
    assert!(
        out.stdout
            .contains("assert(condition: bool, message: String) [builtin]")
            && out
                .stdout
                .contains("assert_eq(left, right, message: String) [builtin]"),
        "missing prelude assertion help: {}",
        out.stdout
    );
}

#[test]
fn repl_meta_info_shows_a_function_signature_and_docs() {
    let out = run_repl("%info strings::slice -d\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout
            .contains("std::strings::slice(text: String, start: i64, end: i64)"),
        "function help must include the complete public signature: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("Safe byte-range slice"),
        "function help must retain documentation: {}",
        out.stdout
    );
}

#[test]
fn repl_meta_info_uses_checker_exposed_stdlib_signatures() {
    let out = run_repl("%info fs::read -d\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout
            .contains("std::fs::read(path: String) -> Result<Vec<u8>, io::Error> [fn]"),
        "expected generated catalog signature for fs::read: {}",
        out.stdout
    );
}

#[test]
fn repl_meta_info_distinguishes_same_leaf_function_names_by_type() {
    let out = run_repl("%info *count -d\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout
            .contains("std::strings::count(text: String, needle: String | char) -> i64 [fn]"),
        "strings count signature missing: {}",
        out.stdout
    );
    assert!(
        out.stdout
            .contains("std::iter::count<T>(items: Vec<T>) -> i64 [fn]"),
        "iter count signature missing: {}",
        out.stdout
    );
}

#[test]
fn repl_meta_info_regex_does_not_search_keyword_documentation() {
    let out = run_repl("%info /question_mark/\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("nothing found for `/question_mark/`"),
        "%info regex should not search keyword documentation; stdout: {}",
        out.stdout
    );
}

#[test]
fn repl_meta_ls_lists_the_complete_io_namespace() {
    let out = run_repl("%info io -d\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("std::io [module]")
            && out.stdout.contains("Stream-oriented I/O abstractions"),
        "%info io should show the matching module and its documentation: {}",
        out.stdout
    );
}

#[test]
fn repl_meta_info_combines_regex_help_and_listing() {
    let out = run_repl("%info /std::regex/\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("std::regex"),
        "expected regex-filtered module; stdout: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("replace_all"),
        "%info should combine module help with its listing; stdout: {}",
        out.stdout
    );
}

#[test]
fn repl_meta_info_for_qualified_function_is_focused() {
    let out = run_repl("%i strings::trim_start_matches -d\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stderr.is_empty(),
        "%i should accept qualified functions: {}",
        out.stderr
    );
    assert!(
        out.stdout
            .contains("std::strings::trim_start_matches(text: String, cutset: String | char)"),
        "%i should render help for the qualified function: {}",
        out.stdout
    );
    assert!(
        !out.stdout.contains("std::strings::bytes("),
        "%i should not dump the function's module listing: {}",
        out.stdout
    );
}

#[test]
fn repl_meta_info_for_shared_method_name_does_not_append_an_owner_listing() {
    let out = run_repl("%i *contains_key\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    for expected in ["BTreeMap::contains_key<", "Map::contains_key<"] {
        assert!(
            out.stdout.contains(expected),
            "%i omitted `{expected}`: {}",
            out.stdout
        );
    }
    assert!(
        !out.stdout.contains("BTreeMap::get<"),
        "%i should not append the arbitrary BTreeMap listing: {}",
        out.stdout
    );
}

#[test]
fn repl_meta_info_exact_short_name_still_matches_full_paths() {
    let out = run_repl("%i *scan\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    for expected in [
        "Scanner::scan(self: bufio::Scanner) -> bool [method]",
        "bufio::Scanner::scan(self: bufio::Scanner) -> bool [method]",
        "std::iter::scan<",
    ] {
        assert!(
            out.stdout.contains(expected),
            "%i scan omitted `{expected}`: {}",
            out.stdout
        );
    }
}

#[test]
fn repl_info_shows_every_sync_map_method_signature() {
    for query in ["sync::Map", "*Map"] {
        let out = run_repl(&format!("%i {query}\n"));
        assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
        for expected in [
            "contains_key(self: sync::Map, key: String) -> bool [method]",
            "get(self: sync::Map, key: String) -> Option<String> [method]",
            "insert(self: sync::Map, key: String, value: String) -> () [method]",
            "keys(self: sync::Map) -> Vec<String> [method]",
            "len(self: sync::Map) -> i64 [method]",
            "new() -> sync::Map [associated function]",
            "remove(self: sync::Map, key: String) -> () [method]",
        ] {
            assert!(
                out.stdout.contains(expected),
                "%i {query} omitted `{expected}`: {}",
                out.stdout
            );
        }
    }
}

#[test]
fn repl_rejects_invalid_string_call_arguments_before_execution() {
    let out = run_repl(
        "let s = \"abcde\"\n\
         s.slice(1)\n\
         s.slice(1..3)\n\
         s.slice(\"a\")\n\
         s.slice(1, 3)\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stderr
            .contains("strings::slice` takes 2 argument(s) but 1 were supplied"),
        "missing slice-end argument was not rejected: {}",
        out.stderr
    );
    assert!(
        out.stderr.contains("expected `i64`"),
        "non-integer slice argument was not rejected: {}",
        out.stderr
    );
    assert!(
        out.stdout.contains("Ok(\"bc\")"),
        "valid slice call should still run: {}",
        out.stdout
    );
}

#[test]
fn repl_reports_each_invalid_string_argument_once_with_its_value() {
    let out = run_repl("use std::strings\nstrings::count(1, \"a\")\nstrings::count(\"ab\", 1)\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert_eq!(
        out.stderr
            .matches("parameter `text` of `strings::count`")
            .count(),
        1,
        "the first invalid argument must be reported exactly once: {}",
        out.stderr
    );
    assert_eq!(
        out.stderr
            .matches("parameter `needle` of `strings::count`")
            .count(),
        1,
        "the second invalid argument must be reported exactly once: {}",
        out.stderr
    );
    assert!(
        out.stderr.contains("found `i64` (value `1`)"),
        "the diagnostic must include the supplied literal: {}",
        out.stderr
    );
}

#[test]
fn repl_reports_array_arguments_without_inference_variable_types() {
    let out = run_repl("use std::strings\nstrings::slice([1, 2, 3], 1, 2)\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stderr.contains("found `array`"),
        "array type should be user-facing: {}",
        out.stderr
    );
    assert!(
        !out.stderr.contains('?'),
        "diagnostic must not expose inference variables: {}",
        out.stderr
    );
}

#[test]
fn repl_rejects_unqualified_std_functions() {
    let out = run_repl("use std::strings\ncount(\"abc\", 'a')\nstrings::count(\"abc\")\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stderr.contains("cannot find `count` in this scope"),
        "unqualified std function must not dispatch ambiguously: {}",
        out.stderr
    );
    assert!(
        out.stderr
            .contains("strings::count` takes 2 argument(s) but 1 were supplied"),
        "qualified std function must still enforce arity: {}",
        out.stderr
    );
}

#[test]
fn repl_persists_rust_style_string_and_vec_mutations() {
    let out = run_repl(
        "let mut s = \"abc\"\n\
         let mut v = Vec::from([1, 2])\n\
         s.push('d')\n\
         s\n\
         v.push(3)\n\
         v\n\
         String::push(&mut s, 'e')\n\
         s\n\
         Vec::push(&mut v, 4)\n\
         v\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(out.stdout.contains("\"abcd\""), "{}", out.stdout);
    assert!(out.stdout.contains("[1, 2, 3]"), "{}", out.stdout);
    assert!(out.stdout.contains("\"abcde\""), "{}", out.stdout);
    assert!(out.stdout.contains("[1, 2, 3, 4]"), "{}", out.stdout);
}

#[test]
fn repl_mut_vec_for_loop_and_tuple_for_loop_work() {
    let out = run_repl(
        "let mut v = Vec::from([1, 2])\n\
         for i in &mut v { *i += 1 }\n\
         println(v)\n\
         for i in (0, 1) { println(i) }\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(out.stdout.contains("\n#[2, 3]\n"), "{}", out.stdout);
    assert!(out.stdout.contains("\n0\n1\n"), "{}", out.stdout);
}

#[test]
fn repl_iterates_open_start_ranges_from_zero() {
    let out = run_repl("for i in ..3 { println(i) }\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("\n0\n1\n2\n"),
        "open-start range silently produced no values:\n{}",
        out.stdout
    );
}

#[test]
fn repl_iterates_a_range_stored_in_a_binding() {
    let out = run_repl("let a = 0..3\nfor i in a { println(i) }\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("\n0\n1\n2\n"),
        "stored range silently produced no values:\n{}",
        out.stdout
    );
}

#[test]
fn repl_stores_and_iterates_an_open_end_range() {
    let out = run_repl(
        "use std::iter\n\
         let b = 3..\n\
         b.take(3) |> iter::collect()\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stderr.is_empty(),
        "open-ended range produced diagnostics:\n{}",
        out.stderr
    );
    assert!(out.stdout.contains("[3, 4, 5]"), "{}", out.stdout);
}

#[test]
fn repl_iterates_bare_strings_as_unicode_chars() {
    let out = run_repl("for c in \"ąčęėšž\" { println(c) }\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("\ną\nč\nę\nė\nš\nž\n"),
        "bare String iteration did not yield Unicode characters:\n{}",
        out.stdout
    );
}

#[test]
fn repl_preserves_vec_bindings_across_all_supported_mutating_loops() {
    let out = run_repl(
        "let mut by_ref = Vec::from([1, 2])\n\
         for value in &mut by_ref { *value += 1 }\n\
         let mut by_enumerate = Vec::from([1, 2])\n\
         for (i, _) in by_enumerate.iter().enumerate() { by_enumerate[i] += 1 }\n\
         let mut by_range = Vec::from([1, 2])\n\
         for i in 0..by_range.len() { by_range[i] += 1 }\n\
         let mut by_array = Vec::from([1, 2])\n\
         for i in [0, 1] { by_array[i] += 1 }\n\
         %b\n\
         let mut by_ref = Vec::from([4, 5])\n\
         by_ref\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    for name in ["by_ref", "by_enumerate", "by_range", "by_array"] {
        assert!(
            out.stdout
                .contains(&format!("mut {name}: Vec<i64> = #[2, 3]")),
            "{name} was not preserved after its loop:\n{}",
            out.stdout
        );
    }
    assert!(
        out.stdout.contains("[4, 5]"),
        "redefining a loop-mutated binding did not replace it:\n{}",
        out.stdout
    );
}

#[test]
fn repl_rejects_invalid_qualified_string_push_without_mutating() {
    let out = run_repl(
        "let mut s = \"abc\"\n\
         String::push(&mut s, \"d\")\n\
         s\n\
         String::push(&mut s, 'd')\n\
         s\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stderr.contains("expected `char`, found `String`"),
        "{}",
        out.stderr
    );
    assert!(out.stdout.contains("\"abc\""), "{}", out.stdout);
    assert!(out.stdout.contains("\"abcd\""), "{}", out.stdout);
}

#[test]
fn repl_rejects_invalid_qualified_vec_push_without_mutating() {
    let out = run_repl(
        "let mut v = Vec::from([1, 2])\n\
         Vec::push(&mut v, \"x\")\n\
         v\n\
         Vec::push(&mut v, 3)\n\
         v\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stderr.contains("expected `i64`, found `String`"),
        "{}",
        out.stderr
    );
    assert!(out.stdout.contains("[1, 2]"), "{}", out.stdout);
    assert!(out.stdout.contains("[1, 2, 3]"), "{}", out.stdout);
}

#[test]
fn repl_checks_string_and_vec_push_contracts() {
    let out = run_repl(
        "let mut s = \"abc\"\n\
         let mut v = Vec::from([1, 2])\n\
         s.push(\"x\")\n\
         v.push(3, 4, 5)\n\
         v.push(\"x\")\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stderr.contains("expected `char`, found `String`"),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr
            .contains("`Vec::push` takes 1 argument(s) but 3 were supplied"),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("expected `i64`, found `String`"),
        "{}",
        out.stderr
    );
}

#[test]
fn repl_reports_collection_argument_mismatches_in_expected_found_order() {
    let out = run_repl(
        "let mut v = Vec::from([1, 2])\n\
         v.push(\"x\")\n\
         v.push('a')\n\
         v.push([3, 4, 5])\n\
         let mut strings = Vec::from([\"a\"])\n\
         strings.push(1)\n\
         let mut floats = Vec::from([1.0])\n\
         floats.push('b')\n\
         let mut chars = Vec::from(['a'])\n\
         chars.push(2.0)\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stderr.contains("expected `i64`, found `String`"),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("expected `i64`, found `char`"),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("expected `i64`, found `[i64; 3]`"),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("expected `String`, found `i64`"),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("expected `f64`, found `char`"),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("expected `char`, found `f64`"),
        "{}",
        out.stderr
    );
}

#[test]
fn repl_persists_map_set_and_deque_mutations() {
    let out = run_repl(
        "let mut m: Map<String, i64> = Map::new()\n\
         m.insert(\"a\", 1)\n\
         m.len()\n\
         let mut set: Set<i64> = Set::new()\n\
         set.insert(7)\n\
         set.insert(7)\n\
         set.remove(7)\n\
         set.contains(7)\n\
         let mut deque: Deque<i64> = Deque::new()\n\
         deque.push_back(1)\n\
         deque.push_front(2)\n\
         deque.pop_back()\n\
         deque.pop_front()\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    for expected in ["1", "true", "false", "true", "false", "Some(1)", "Some(2)"] {
        assert!(
            out.stdout.contains(expected),
            "missing {expected}: {}",
            out.stdout
        );
    }
    assert!(
        !out.stdout.contains("Set {"),
        "set mutators leaked the internal handle: {}",
        out.stdout
    );
}

#[test]
fn repl_renders_queue_and_heap_bindings() {
    let out = run_repl(
        "let q = Queue::from([1, 2, 3])\nlet s = Stack::from([1, 2, 3])\nlet max = MaxHeap::from([1, 2, 3])\nlet min = MinHeap::from([1, 2, 3])\n%b\nmax.peek()\nmin.peek()\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    for expected in [
        "q: Queue<i64> = Queue [1, 2, 3]",
        "s: Stack<i64> = Stack [1, 2, 3]",
        "max: MaxHeap<i64> = MaxHeap [",
        "min: MinHeap<i64> = MinHeap [",
    ] {
        assert!(
            out.stdout.contains(expected),
            "binding did not render through %b: {expected}: {}",
            out.stdout
        );
    }
    assert!(
        out.stdout.contains("Some(3)") && out.stdout.contains("Some(1)"),
        "heap peeks should surface the extremes: {}",
        out.stdout
    );
}

#[test]
fn repl_or_insert_persists_and_cannot_retype_the_map() {
    let out = run_repl(
        "let mut h = Map::new()\n\
         h.insert(\"a\", 1)\n\
         h.or_insert(\"c\", 0)\n\
         h.get_or(\"c\", 5)\n\
         h = h.or_insert(\"d\", 2)\n\
         h\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains('0'),
        "or_insert mutation did not persist: {}",
        out.stdout
    );
    assert!(
        out.stderr
            .contains("expected `Map<String, i64>`, found `i64`"),
        "invalid map assignment was not rejected: {}",
        out.stderr
    );
    assert!(
        out.stdout.contains(r#"{"a": 1, "c": 0}"#),
        "failed assignment corrupted the map: {}",
        out.stdout
    );
}

#[test]
fn repl_bare_map_iteration_formats_keys_and_returns_unit() {
    let out = run_repl(
        "let mut h = Map::new()\n\
         h.insert(\"a\", 1)\n\
         h.insert(\"b\", 2)\n\
         %b\n\
         for (k, v) in h { println(\"{}: {}\", k, v) }\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout
            .contains(r#"h: Map<String, i64> = {"a": 1, "b": 2}"#),
        "map binding did not quote string keys: {}",
        out.stdout
    );
    assert!(out.stdout.contains("a: 1\nb: 2\n"), "{}", out.stdout);
    assert!(
        !out.stderr.contains("not indexable"),
        "bare map iteration attempted numeric map indexing: {}",
        out.stderr
    );
}

/// An unterminated triple-quoted literal keeps the REPL reading lines
/// until the closing delimiter arrives.
#[test]
fn repl_continues_reading_an_open_triple_quoted_string() {
    let out = run_repl("let text = \"\"\"\n    a\n    b\n    \"\"\"\nprintln(\"{}\", text)\n");
    assert!(
        out.stdout.contains("a\nb"),
        "stdout: {}\nstderr: {}",
        out.stdout,
        out.stderr
    );
}

/// `%info` answers for a type the session declares, not only for the
/// stdlib catalog, and reports its fields, the traits implemented for
/// it, and where each method came from.
#[test]
fn repl_info_reports_fields_traits_and_methods_for_a_session_struct() {
    let out = run_repl(concat!(
        "struct Point { x: f64, y: f64 }\n",
        "trait Area { fn area(&self) -> f64 }\n",
        "impl Area for Point { fn area(&self) -> f64 { self.x * self.y } }\n",
        "impl Point { fn mag(&self) -> f64 { self.x } }\n",
        "%info Point\n",
    ));
    assert!(
        out.stdout.contains("Point [struct]"),
        "stdout: {}",
        out.stdout
    );
    assert!(out.stdout.contains("  fields"), "stdout: {}", out.stdout);
    assert!(out.stdout.contains("    x: f64"), "stdout: {}", out.stdout);
    assert!(
        out.stdout.contains("  implements"),
        "stdout: {}",
        out.stdout
    );
    assert!(out.stdout.contains("    Area"), "stdout: {}", out.stdout);
    assert!(
        out.stdout.contains("Point::area(&self) -> f64 [Area]"),
        "stdout: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("Point::mag(&self) -> f64 [inherent]"),
        "stdout: {}",
        out.stdout
    );
}

/// `%info` on a trait names its methods and the types implementing it.
#[test]
fn repl_info_reports_a_traits_methods_and_implementors() {
    let out = run_repl(concat!(
        "struct Point { x: f64, y: f64 }\n",
        "trait Area { fn area(&self) -> f64 }\n",
        "impl Area for Point { fn area(&self) -> f64 { self.x * self.y } }\n",
        "%info Area\n",
    ));
    assert!(
        out.stdout.contains("Area [trait]"),
        "stdout: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("fn area(&self) -> f64"),
        "stdout: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("  implemented by"),
        "stdout: {}",
        out.stdout
    );
}

/// `%explain` on a binding renders the session's facts in receiver form.
#[test]
fn repl_explain_binding_shows_fields_and_receiver_form_methods() {
    let out = run_repl(concat!(
        "struct Point { x: f64, y: f64 }\n",
        "trait Area { fn area(&self) -> f64 }\n",
        "impl Area for Point { fn area(&self) -> f64 { self.x * self.y } }\n",
        "let p = Point { x: 1.0, y: 2.0 }\n",
        "%explain p\n",
    ));
    assert!(
        out.stdout.contains("p: Point [binding]"),
        "stdout: {}",
        out.stdout
    );
    assert!(out.stdout.contains("    y: f64"), "stdout: {}", out.stdout);
    assert!(
        out.stdout.contains("p.area() -> f64 [Area]"),
        "stdout: {}",
        out.stdout
    );
}

/// A session `impl` on a builtin type adds to the catalog's entry for
/// it rather than replacing it.
#[test]
fn repl_info_merges_a_session_impl_into_a_builtin_types_entry() {
    let out = run_repl(concat!(
        "trait Area { fn area(&self) -> f64 }\n",
        "impl Area for String { fn area(&self) -> f64 { 1.0 } }\n",
        "%info String\n",
    ));
    assert!(
        out.stdout.contains("String::area(&self) -> f64 [Area]"),
        "session impl missing: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("String::to_uppercase"),
        "catalog methods lost: {}",
        out.stdout
    );
    assert_eq!(
        out.stdout.matches("String [type]").count(),
        1,
        "header printed more than once: {}",
        out.stdout
    );
}

/// A builtin type's listing line carries what the type is, not just its
/// name and tag.
#[test]
fn repl_info_describes_a_builtin_type_without_details() {
    let out = run_repl("%info String\n");
    assert!(
        out.stdout.contains("String [type]"),
        "stdout: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("Unicode scalars"),
        "no description on the type line: {}",
        out.stdout
    );
}

/// Every builtin type is named once, whether it reaches the listing as
/// a catalogued type or as the owner of its methods.
#[test]
fn repl_info_names_a_builtin_type_once() {
    let out = run_repl("%info Tuple\n");
    assert_eq!(
        out.stdout.matches("Tuple [type]").count(),
        1,
        "stdout: {}",
        out.stdout
    );
}

/// The REPL runs the front end phase by phase, so it has to perform
/// the same caller-side normalisation a file gets: a std function named
/// in value position stands for the closure that calls it.
#[test]
fn repl_accepts_the_callback_shorthand() {
    let out = run_repl(
        "use std::math\n\
         #[1.0, -2.0].map(math::abs)\n\
         #[\" a \"].map(|v| v.trim())\n",
    );
    assert!(
        out.stdout.contains("[1.0, 2.0]"),
        "a std fn in value position should answer the mapped Vec: {}\n{}",
        out.stdout,
        out.stderr
    );
    assert!(
        out.stdout.contains("[\"a\"]"),
        "a written closure should run: {}\n{}",
        out.stdout,
        out.stderr
    );
}

/// The REPL runs its own front end, so it holds the same contract the
/// compiled tiers do: `$` and an argument-taking step report rather
/// than desugar.
#[test]
fn repl_rejects_the_retired_placeholder_forms() {
    let out = run_repl(
        "#[1.0, -2.0].map($.abs)\n\
         \"a,b\" |> strings::split(\",\")\n",
    );
    let combined = format!("{}{}", out.stdout, out.stderr);
    for code in ["GP0027", "GP0041"] {
        assert!(
            combined.contains(code),
            "expected {code} from the REPL front end: {combined}"
        );
    }
}

/// A traversal on a reference to a map walks the map the reference names, so
/// `mr.find(..)` answers what `(*mr).find(..)` does. The REPL runs its own
/// front end, so it checks the same contract the compiled tiers hold to.
#[test]
fn repl_traverses_a_keyed_collection_through_a_reference() {
    let out = run_repl(
        "let m = {\"a\": 1, \"b\": 2}\n\
         let mr = &m\n\
         mr.find(|(_, n)| n > 1)\n\
         (*mr).find(|(_, n)| n > 1)\n\
         let s = #{1, 2, 3}\n\
         let sr = &s\n\
         sr.iter().find(|v| v > 1)\n\
         sr.len()\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert_eq!(
        out.stdout.matches("Some((\"b\", 2))").count(),
        2,
        "a `&Map` walk answers what the dereferenced walk does: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("Some(2)"),
        "a `&Set` walks through its iterator: {}",
        out.stdout
    );
}

#[test]
fn repl_meta_info_names_one_type_exactly() {
    let out = run_repl("%i Set\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("Set [type]") && out.stdout.contains("Set::union<"),
        "%i Set should answer the set type and its surface: {}",
        out.stdout
    );
    for other in ["BTreeSet", "flag::Set"] {
        assert!(
            !out.stdout.contains(other),
            "%i Set should not answer `{other}`: {}",
            out.stdout
        );
    }
}

#[test]
fn repl_meta_info_names_one_method_exactly() {
    let out = run_repl("%i Set::new\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("Set::new<T>() -> Set<T>"),
        "%i Set::new should answer that method: {}",
        out.stdout
    );
    assert!(
        !out.stdout.contains("BTreeSet::new") && !out.stdout.contains("Set::insert"),
        "%i Set::new should answer nothing else: {}",
        out.stdout
    );
}

#[test]
fn repl_meta_info_leading_wildcard_matches_a_suffix() {
    let out = run_repl("%i *Set\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    for expected in ["Set [type]", "BTreeSet [type]", "flag::Set [type]"] {
        assert!(
            out.stdout.contains(expected),
            "%i *Set omitted `{expected}`: {}",
            out.stdout
        );
    }
}

#[test]
fn repl_meta_info_trailing_wildcard_matches_a_prefix() {
    let out = run_repl("%i Set*\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("Set [type]") && out.stdout.contains("Set::union<"),
        "%i Set* should answer the set type and its surface: {}",
        out.stdout
    );
    assert!(
        !out.stdout.contains("BTreeSet"),
        "%i Set* should not answer a name it is not a prefix of: {}",
        out.stdout
    );
}

#[test]
fn repl_meta_explain_names_one_binding_exactly_and_widens_with_a_wildcard() {
    let out = run_repl("let total = 1\nlet tot = 2\n%e total\n%e tot*\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    let exact = out.stdout.matches("total: i64 [binding]").count();
    assert_eq!(
        exact, 2,
        "`%e total` names one binding and `%e tot*` names it again: {}",
        out.stdout
    );
    assert_eq!(
        out.stdout.matches("tot: i64 [binding]").count(),
        1,
        "`%e tot*` should also answer `tot`: {}",
        out.stdout
    );
}

#[test]
fn repl_info_names_a_bare_stdlib_trait_and_the_method_to_implement() {
    let out = run_repl("%i Display\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("std::fmt::Display") && out.stdout.contains("[trait]"),
        "a bare trait name should reach its manifest entry: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("fn fmt(&self) -> String"),
        "the entry should name the method an impl supplies: {}",
        out.stdout
    );
}

#[test]
fn repl_info_answers_every_language_supplied_trait() {
    for (name, expected) in [
        ("PartialEq", "fn eq(&self, other: Self) -> bool"),
        ("Eq", "fn eq(&self, other: Self) -> bool"),
        ("Ord", "fn cmp(&self, other: Self) -> i64"),
        ("Clone", "fn clone(&self) -> Self"),
        ("Default", "fn default() -> Self"),
        ("Iterator", "fn next(&mut self) -> Option<T>"),
        ("From", "fn from(value: T) -> Self"),
        ("Add", "fn add(&self, other: Self) -> Self"),
        ("Not", "fn not(&self) -> Self"),
        ("Index", "fn index(&self, index: I) -> T"),
    ] {
        let out = run_repl(&format!("%i {name}\n"));
        assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
        assert!(
            out.stdout
                .contains(&format!("{name} {{ {expected} }} [trait]")),
            "`%i {name}` should name the method an impl supplies: {}",
            out.stdout
        );
    }
}

#[test]
fn repl_info_says_what_to_write_instead_of_a_language_supplied_trait() {
    let out = run_repl("%i Hash\n%i Drop\n%i Into\n");
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    for (name, guidance) in [
        ("Hash", "Hashing is structural and automatic"),
        ("Drop", "defer"),
        ("Into", "impl From<Source> for"),
    ] {
        assert!(
            out.stdout.contains(&format!("{name} [trait]")),
            "`%i {name}` should answer: {}",
            out.stdout
        );
        assert!(
            out.stdout.contains(guidance),
            "`%i {name}` should say what the language does instead: {}",
            out.stdout
        );
    }
}

#[test]
fn repl_info_calls_an_implemented_trait_a_trait_not_a_type() {
    let out = run_repl(
        "struct P { a: i64 }\n\
         impl Display for P { fn fmt(&self) -> String { \"p\" } }\n\
         %i Display\n",
    );
    assert!(out.success, "repl should exit zero; stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("Display [trait]"),
        "a name known through an `impl X for Y` header is a trait: {}",
        out.stdout
    );
    assert!(
        !out.stdout.contains("Display [type]"),
        "`Display` is a trait, never a type: {}",
        out.stdout
    );
}
