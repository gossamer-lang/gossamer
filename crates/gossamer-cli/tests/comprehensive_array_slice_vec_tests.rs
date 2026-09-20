//! Rust-oracle coverage for fixed arrays, unsized slices, and growable Vecs.

mod common;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use gossamer_lex::SourceMap;
use gossamer_parse::parse_source_file;
use gossamer_resolve::resolve_source_file;
use gossamer_types::{TyCtxt, typecheck_source_file};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

fn diagnostics(body: &str) -> Vec<String> {
    let source = format!("fn main() {{\n{body}\n}}\n");
    let mut map = SourceMap::new();
    let file = map.add_file("comprehensive_array_slice_vec_tests.gos", source.clone());
    let (mut parsed, parse_diagnostics) = parse_source_file(&source, file);
    if !parse_diagnostics.is_empty() {
        return parse_diagnostics
            .into_iter()
            .map(|diagnostic| diagnostic.error.to_string())
            .collect();
    }
    let (resolutions, resolve_diagnostics) = resolve_source_file(&parsed);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut parsed, &resolutions);
    if !resolve_diagnostics.is_empty() {
        return resolve_diagnostics
            .into_iter()
            .map(|diagnostic| diagnostic.error.to_string())
            .collect();
    }
    let mut tcx = TyCtxt::new();
    let (_, type_diagnostics) = typecheck_source_file(&parsed, &resolutions, &mut tcx);
    type_diagnostics
        .into_iter()
        .map(|diagnostic| format!("{}: {}", diagnostic.error.code(), diagnostic.error))
        .collect()
}

fn assert_accepted(label: &str, body: &str) {
    let found = diagnostics(body);
    assert!(found.is_empty(), "{label} should be accepted: {found:?}");
}

fn assert_rejected(label: &str, body: &str, expected: &str) {
    let found = diagnostics(body);
    assert!(
        found.iter().any(|diagnostic| diagnostic.contains(expected)),
        "{label} should contain {expected:?}: {found:?}"
    );
}

fn rust_accepts(body: &str) -> bool {
    let unique = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
    let root = env::temp_dir().join(format!(
        "gossamer-rust-sequence-oracle-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&root).expect("create Rust oracle directory");
    let source = root.join("oracle.rs");
    let binary = root.join("oracle-bin");
    fs::write(&source, format!("fn main() {{\n{body}\n}}\n")).expect("write Rust oracle");
    let output = Command::new("rustc")
        .args(["--edition=2024", "--crate-name", "sequence_oracle"])
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .output()
        .expect("rustc is required for the Rust sequence oracle");
    let _ = fs::remove_dir_all(root);
    output.status.success()
}

fn gos_bin() -> PathBuf {
    PathBuf::from(env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

#[test]
fn rust_and_gossamer_agree_on_owned_sequence_assignments() {
    let accepted = [
        ("fixed array", "let a: [i64; 3] = [1, 2, 3]; let _b = a;"),
        (
            "explicit Vec construction",
            "let a: Vec<i64> = Vec::from([1, 2, 3]); let _b = a;",
        ),
    ];
    for (label, body) in accepted {
        assert!(rust_accepts(body), "Rust rejected oracle case {label}");
        assert_accepted(label, body);
    }

    assert_accepted(
        "hash-bracket literal creates Vec by default",
        "let _a: Vec<i64> = #[1, 2, 3]",
    );
    assert_accepted(
        "bracket literal creates a fixed array by default",
        "let _a: [i64; 3] = [1, 2, 3]",
    );
    assert_rejected(
        "an unshaped fixed array does not become Vec",
        "let a = [1, 2, 3]\nlet _v: Vec<i64> = a",
        "expected `Vec<i64>`, found `[i64; 3]`",
    );
    assert_rejected("owned slice local", "let _a: [i64] = #[1, 2, 3]", "GT0049");
}

#[test]
fn hash_bracket_literals_create_vecs_in_assignment_contexts() {
    let accepted = [
        (
            "named Vec local initializer",
            "let a = #[1, 2, 3]\nlet _v: Vec<i64> = a",
        ),
        (
            "Vec literal local initializer",
            "let _v: Vec<i64> = #[1, 2, 3]",
        ),
        (
            "whole-local reassignment",
            "let a = #[1, 2, 3]\nlet mut v: Vec<i64> = #[0]\nv = a",
        ),
        (
            "function argument",
            "fn consume(_v: Vec<i64>) {}\nlet a = #[1, 2, 3]\nconsume(a)",
        ),
        (
            "function return",
            "fn make() -> Vec<i64> { let a = #[1, 2, 3]\na }\nlet _v = make()",
        ),
        (
            "branch under Vec expectation",
            "let _v: Vec<i64> = if true { #[1, 2, 3] } else { #[4, 5, 6] }",
        ),
        (
            "nested tuple element",
            "let a = #[1, 2, 3]\nlet _pair: (Vec<i64>, i64) = (a, 4)",
        ),
    ];
    for (label, body) in accepted {
        assert_accepted(label, body);
    }
}

#[test]
fn explicit_array_to_vec_conversions_are_accepted_and_execute() {
    let body = "let first = [1, 2, 3]\n\
        let mut via_into: Vec<i64> = first.into()\n\
        via_into.push(4)\n\
        let second = [5, 6]\n\
        let via_from: Vec<i64> = Vec::from(second)\n\
        println(via_into)\n\
        println(via_from)";
    assert_accepted("explicit array to Vec conversions", body);

    let output = Command::new(gos_bin())
        .args(["-e", body])
        .output()
        .expect("execute explicit array conversions");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "#[1, 2, 3, 4]\n#[5, 6]\n"
    );
}

#[test]
fn all_four_rust_unsizing_coercions_are_accepted() {
    let gossamer = "    let array: [i64; 3] = [1, 2, 3];\n\
        let array_slice: &[i64] = &array;\n\
        let vector: Vec<i64> = Vec::from([4, 5, 6]);\n\
        let vec_slice: &[i64] = &vector;\n\
        let mut mutable_array: [i64; 3] = [7, 8, 9];\n\
        let mutable_array_slice: &mut [i64] = &mut mutable_array;\n\
        mutable_array_slice[0] = 10;\n\
        let mut mutable_vector: Vec<i64> = Vec::from([11, 12, 13]);\n\
        let mutable_vec_slice: &mut [i64] = &mut mutable_vector;\n\
        mutable_vec_slice[0] = 14;\n\
        let _sum = array_slice[0] + vec_slice[0] + mutable_array_slice[0] + mutable_vec_slice[0];";
    assert!(rust_accepts(gossamer), "Rust rejected the coercion oracle");
    assert_accepted("all four slice-reference coercions", gossamer);
}

#[test]
fn parameters_returns_and_owned_assignments_keep_each_sequence_identity() {
    // The two spellings differ in exactly one place, and deliberately: a
    // Gossamer parameter names the sequence view as `[T]`, where Rust
    // needs the `&` because the view is unsized by value there.
    let body = "let fixed = array_id([1, 2, 3]);\n\
        let mut fixed_mut = fixed;\n\
        slice_set(&mut fixed_mut);\n\
        let grown = vec_id(Vec::from([4, 5, 6]));\n\
        let _total = slice_len(&fixed_mut) + slice_len(&grown);";
    let oracle = format!(
        "fn array_id(value: [i64; 3]) -> [i64; 3] {{ value }}\n\
         fn slice_len(value: &[i64]) -> usize {{ value.len() }}\n\
         fn slice_set(value: &mut [i64]) {{ value[0] = 9 }}\n\
         fn vec_id(value: Vec<i64>) -> Vec<i64> {{ value }}\n\
         {body}"
    );
    let accepted = "fn array_id(value: [i64; 3]) -> [i64; 3] { value }\n\
        fn slice_len(value: [i64]) -> usize { value.len() as usize }\n\
        fn slice_set(value: &mut [i64]) { value[0] = 9 }\n\
        fn vec_id(value: Vec<i64>) -> Vec<i64> { value }\n\
        let fixed = array_id([1, 2, 3]);\n\
        let mut fixed_mut = fixed;\n\
        slice_set(&mut fixed_mut);\n\
        let grown = vec_id(Vec::from([4, 5, 6]));\n\
        let _total = slice_len(fixed_mut) + slice_len(grown);";
    assert!(rust_accepts(&oracle), "Rust rejected the parameter oracle");
    assert_accepted("array, slice, and Vec parameters and returns", accepted);

    assert!(
        !rust_accepts("fn bad() -> [i64] { [1, 2, 3] }"),
        "Rust unexpectedly accepted an unsized slice return"
    );
    assert_rejected(
        "unsized slice return",
        "fn bad() -> [i64] { [1, 2, 3] }",
        "GT0049",
    );
    assert_accepted(
        "Vec return accepts a Vec literal",
        "fn ok() -> Vec<i64> { #[1, 2, 3] }\nlet _v = ok()",
    );
}

#[test]
fn slices_and_arrays_reject_every_vec_only_operation() {
    let methods = [
        "push(4)",
        "pop()",
        "insert(0, 4)",
        "remove(0)",
        "clear()",
        "truncate(1)",
        "extend(Vec::from([4]))",
        "reserve(8)",
        "capacity()",
        "shrink_to_fit()",
        "drain()",
    ];
    for method in methods {
        assert_rejected(
            &format!("array {method}"),
            &format!("let mut values = [1, 2, 3]\nvalues.{method}"),
            "GT0050",
        );
        assert_rejected(
            &format!("slice {method}"),
            &format!(
                "let mut storage = #[1, 2, 3]\nlet values: &mut [i64] = &mut storage\nvalues.{method}"
            ),
            "GT0050",
        );
    }
}

#[test]
fn every_sequence_traverses_its_own_values() {
    // A sequence already holds its values, so a traversal answers on it
    // directly; `iter()` is the lazy walk, not a precondition for traversing.
    for method in [
        "sum()",
        "take(2)",
        "filter(|value| value > 1)",
        "fold(0, |a, b| a + b)",
    ] {
        assert_accepted(
            &format!("array {method}"),
            &format!("let values = [1, 2, 3]\nlet _ = values.{method}"),
        );
        assert_accepted(
            &format!("slice {method}"),
            &format!(
                "let storage = #[1, 2, 3]\nlet values: &[i64] = &storage\nlet _ = values.{method}"
            ),
        );
        assert_accepted(
            &format!("vec {method}"),
            &format!("let values = #[1, 2, 3]\nlet _ = values.{method}"),
        );
    }

    assert_accepted(
        "the lazy walk stays available through iter()",
        "let values = #[1, 2, 3]\nlet _sum = values.iter().sum()\nlet _head = values.iter().take(2)",
    );
    assert_accepted(
        "fixed array clone preserves its fixed type",
        "let values = [1, 2, 3]\nlet copy: [i64; 3] = values.clone()",
    );
}

#[test]
fn every_cataloged_shared_sequence_method_typechecks_on_its_valid_receiver() {
    let immutable_calls = [
        "values.len()",
        "values.is_empty()",
        "values.slice(0, 2)",
        "values.first()",
        "values.last()",
        "values.get(1)",
        "values.contains(2)",
        "values.index_of(2)",
        "values.count_of(2)",
        "values.windows(2)",
        "values.chunks(2)",
        "values.join(\",\")",
        "values.to_vec()",
        "values.iter()",
    ];
    for call in immutable_calls {
        assert_accepted(
            &format!("fixed array shared method {call}"),
            &format!("let values = [1, 2, 3]\nlet _result = {call}"),
        );
        assert_accepted(
            &format!("slice shared method {call}"),
            &format!(
                "let storage = #[1, 2, 3]\nlet values: &[i64] = &storage\nlet _result = {call}"
            ),
        );
    }

    let mutable_calls = [
        "values.sort()",
        "values.sort_by(|a, b| a - b)",
        "values.sort_by_key(|value| value)",
        "values.reverse()",
        "values.swap(0, 2)",
        "values.fill(7)",
    ];
    for call in mutable_calls {
        assert_accepted(
            &format!("mutable array shared method {call}"),
            &format!("let mut values = #[3, 1, 2]\nlet _result = {call}"),
        );
        assert_accepted(
            &format!("mutable slice shared method {call}"),
            &format!(
                "let mut storage = #[3, 1, 2]\nlet values: &mut [i64] = &mut storage\nlet _result = {call}"
            ),
        );
    }
}

#[test]
fn indexing_iteration_and_non_resizing_mutation_execute() {
    let source = "let mut array = #[3, 1, 2]; let mut total = 0; { let slice: &mut [i64] = &mut array; let _ = slice.swap(0, 1); slice.sort(); slice.reverse(); slice.fill(4); for value in slice { total += *value } }; println(array); println(total)";
    let output = Command::new(gos_bin())
        .args(["-e", source])
        .output()
        .expect("execute sequence program");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "#[4, 4, 4]\n12\n");
}

#[test]
fn map_literal_constructs_hashmap_and_from_accepts_pairs() {
    let source = "use std::collections::Map; let map = {\"one\": 1, \"two\": 2}; println(map.get(\"one\")); println(map.len()); let also: Map<String, i64> = Map::from([(\"three\", 3)]); println(also.get(\"three\"))";
    let output = Command::new(gos_bin())
        .args(["-e", source])
        .output()
        .expect("execute map literal program");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "Some(1)\n2\nSome(3)\n"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn array_slice_and_vec_execution_matches_vm_forced_jit_and_llvm_release() {
    let root = env::temp_dir().join(format!(
        "gossamer-sequence-tier-parity-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("create tier parity directory");
    let source = root.join("sequence_parity.gos");
    fs::write(
        &source,
        "fn sequence_total() -> i64 {\n\
            let mut fixed = [3, 1, 2]\n\
            {\n\
                let view: &mut [i64] = &mut fixed\n\
                view.sort()\n\
                view.reverse()\n\
                view.fill(2)\n\
            }\n\
            let grown_source = [4, 5]\n\
            let mut grown: Vec<i64> = grown_source.into()\n\
            grown.push(6)\n\
            grown.fill(3)\n\
            let mut total = fixed[0] + fixed[1]\n\
            for value in &fixed { total += *value }\n\
            {\n\
                let shared: &[i64] = &grown\n\
                total += shared[2]\n\
                for value in shared { total += *value }\n\
            }\n\
            let mut fixed_words = [\"a\", \"b\"]\n\
            {\n\
                let word_view: &mut [String] = &mut fixed_words\n\
                word_view.fill(\"x\")\n\
            }\n\
            println(fixed_words)\n\
            let mut grown_words = #[\"c\", \"d\"]\n\
            grown_words.fill(\"y\")\n\
            println(grown_words)\n\
            total\n\
        }\n\
        println(sequence_total())\n",
    )
    .expect("write sequence tier fixture");

    let vm = Command::new(gos_bin())
        .arg("run")
        .arg(&source)
        .env("GOS_JIT", "0")
        .output()
        .expect("run VM sequence fixture");
    assert!(
        vm.status.success(),
        "VM stderr: {}",
        String::from_utf8_lossy(&vm.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&vm.stdout),
        "[\"x\", \"x\"]\n#[\"y\", \"y\"]\n22\n"
    );

    let jit = Command::new(gos_bin())
        .arg("run")
        .arg(&source)
        .env("GOS_JIT_ONLY", "sequence_total")
        .env("GOS_JIT_TRACE", "1")
        .output()
        .expect("run JIT sequence fixture");
    assert!(
        jit.status.success(),
        "JIT stderr: {}",
        String::from_utf8_lossy(&jit.stderr)
    );
    assert_eq!(jit.stdout, vm.stdout);
    assert!(
        String::from_utf8_lossy(&jit.stderr).contains("jit: native hit sequence_total"),
        "fixture did not execute sequence_total natively: {}",
        String::from_utf8_lossy(&jit.stderr)
    );

    let build = Command::new(gos_bin())
        .args(["build", "--release", "--out-dir"])
        .arg(&root)
        .arg(&source)
        .output()
        .expect("build LLVM sequence fixture");
    assert!(
        build.status.success(),
        "LLVM build stderr: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let binary = common::native_executable(&root, "sequence_parity");
    assert!(
        binary.is_file(),
        "LLVM build did not produce {}",
        binary.display()
    );
    let llvm = Command::new(binary)
        .output()
        .expect("run LLVM sequence fixture");
    assert!(
        llvm.status.success(),
        "LLVM stderr: {}",
        String::from_utf8_lossy(&llvm.stderr)
    );
    assert_eq!(llvm.stdout, vm.stdout);
    let _ = fs::remove_dir_all(root);
}

fn write_owned_sequence_parameter_fixture(source: &Path) {
    fs::write(
        source,
        "struct Wrapped {\n\
             values: Vec<i64>,\n\
         }\n\
         fn mutate_array(mut value: [i64; 3]) -> i64 {\n\
             value[0] = 9\n\
             value[0]\n\
         }\n\
         fn mutate_vec(mut value: Vec<i64>) -> i64 {\n\
             value[0] = 8\n\
             value.push(4)\n\
             value.len()\n\
         }\n\
         fn mutate_nested(mut value: Vec<Vec<i64>>) -> i64 {\n\
             value[0].push(7)\n\
             value[0].len()\n\
         }\n\
         fn mutate_wrapped(mut value: Wrapped) -> i64 {\n\
             value.values.push(6)\n\
             value.values.len()\n\
         }\n\
         fn mutate_array_of_vec(mut value: [Vec<i64>; 1]) -> i64 {\n\
             value[0].push(6)\n\
             value[0].len()\n\
         }\n\
         fn publish_words(value: Vec<String>, done: Sender<i64>) {\n\
             done.send(value.len())\n\
         }\n\
         let array = [1, 2, 3]\n\
         let vector = #[1, 2, 3]\n\
         println(\"{} {} {:?} {:?}\", mutate_array(array), mutate_vec(vector), array, vector)\n\
         let nested = #[#[1, 2]]\n\
         println(\"{} {:?}\", mutate_nested(nested), nested)\n\
         let wrapped = Wrapped { values: #[1, 2] }\n\
         println(\"{} {:?}\", mutate_wrapped(wrapped), wrapped.values)\n\
         let array_of_vec = [#[1, 2]]\n\
         println(\"{} {}\", mutate_array_of_vec(array_of_vec), array_of_vec[0].len())\n\
         let tx, rx = channel::<Vec<i64>>(1)\n\
         tx.send(vector)\n\
         let mut received = rx.recv().unwrap()\n\
         received.push(5)\n\
         println(\"{:?} {:?}\", vector, received)\n\
         let words = #[\"alpha\", \"beta\"]\n\
         let word_tx, word_rx = channel::<Vec<String>>(1)\n\
         word_tx.send(words)\n\
         let mut received_words = word_rx.recv().unwrap()\n\
         received_words.push(\"gamma\")\n\
         println(\"{:?} {:?}\", words, received_words)\n\
         let done_tx, done_rx = channel::<i64>(1)\n\
         spawn(|| publish_words(words, done_tx))\n\
         println(\"{}\", done_rx.recv().unwrap())\n\
         println(\"{}\", wrapped.values.len())\n",
    )
    .expect("write owned parameter fixture");
}

#[test]
fn owned_sequence_parameters_do_not_alias_the_caller_on_any_tier() {
    let root = env::temp_dir().join(format!(
        "gossamer-owned-sequence-parameter-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("create owned parameter directory");
    let source = root.join("owned_sequence_parameter.gos");
    write_owned_sequence_parameter_fixture(&source);

    let vm = Command::new(gos_bin())
        .arg("run")
        .arg(&source)
        .env("GOS_JIT", "0")
        .output()
        .expect("run owned parameter fixture in VM");
    assert!(
        vm.status.success(),
        "VM stderr: {}",
        String::from_utf8_lossy(&vm.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&vm.stdout),
        "9 4 [1, 2, 3] #[1, 2, 3]\n3 #[#[1, 2]]\n3 #[1, 2]\n3 2\n#[1, 2, 3] #[1, 2, 3, 5]\n#[\"alpha\", \"beta\"] #[\"alpha\", \"beta\", \"gamma\"]\n2\n2\n"
    );

    for function in ["mutate_wrapped", "mutate_array_of_vec", "main"] {
        let jit = Command::new(gos_bin())
            .arg("run")
            .arg(&source)
            .env("GOS_JIT_ONLY", function)
            .output()
            .expect("run owned parameter fixture in JIT");
        assert!(
            jit.status.success(),
            "JIT {function} stderr: {}",
            String::from_utf8_lossy(&jit.stderr)
        );
        assert_eq!(jit.stdout, vm.stdout, "JIT mismatch for {function}");
    }

    let build = Command::new(gos_bin())
        .args(["build", "--release", "--out-dir"])
        .arg(&root)
        .arg(&source)
        .output()
        .expect("build owned parameter fixture");
    assert!(
        build.status.success(),
        "LLVM build stderr: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let native = Command::new(common::native_executable(&root, "owned_sequence_parameter"))
        .output()
        .expect("run owned parameter LLVM fixture");
    assert!(
        native.status.success(),
        "LLVM stderr: {}",
        String::from_utf8_lossy(&native.stderr)
    );
    assert_eq!(native.stdout, vm.stdout);
    let _ = fs::remove_dir_all(root);
}
