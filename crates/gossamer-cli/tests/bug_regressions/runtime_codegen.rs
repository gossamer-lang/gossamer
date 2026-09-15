#[test]
fn format_macro_result_is_typed_as_string() {
    // `format!("{}{}", a, b).len()` returned a multi-trillion
    // garbage value in the cranelift compiled tier because the
    // typechecker had no signature for the parser-emitted
    // `__concat` intrinsic. The result local was typed as
    // `Var(_)` and the `.len()` dispatch picked `gos_rt_len`
    // (the generic Vec/HashMap length helper) instead of
    // `gos_rt_str_len`. The generic helper read a Vec
    // header from the *c_char pointer and printed garbage.
    // Fix: the typechecker now pins `__concat` / `__fmt_prec`
    // / `format` to `String` and `println` / `print` /
    // `eprintln` / `eprint` to `Unit` in its fallback table.
    let src = r#"
fn main() {
    let a = "foo"
    let b = "bar"
    let combined = format("{}{}", a, b)
    println("len = {}", combined.len())
}
"#;
    let dir = fresh_dir("format_len_typed");
    let path = write_source(&dir, "format_len_typed", src);
    let run = run_vm(&path);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    assert!(
        run.0.contains("len = 6"),
        "vm: expected len=6, got: {:?}",
        run.0
    );
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&path, &cl_dir).expect("cranelift build");
    let native = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert!(
        native.0.contains("len = 6"),
        "native: expected len=6, got: {:?}",
        native.0
    );
}

#[test]
fn immutable_static_path_resolves_to_typed_constant() {
    // `static N: i64 = 5; println!("{}", N)` previously
    // segfaulted in the cranelift compiled tier (and emitted
    // empty output). The typechecker left the path expression
    // typed as `Var(_)`, and `consts.get(def)` returned None
    // for static items (only `const` items were folded), so
    // `N` lowered as a `FnRef` whose pointer was then handed
    // to `gos_rt_concat_str` and caused a strlen segfault.
    // Fix: extend `collect_const_values` to fold immutable
    // `static` items too, and pin the local's MIR type from
    // the const value's shape when the typechecker leaves it
    // as `Var(_)` so format-arg dispatch picks the right
    // helper (concat_i64 / concat_f64 / concat_str etc.).
    let src = r#"
static MAX_RETRIES: i64 = 5
static THRESHOLD: f64 = 0.75
static GREETING: &str = "hello"

fn above_threshold(v: f64) -> bool {
    v > THRESHOLD
}

fn main() {
    println("MAX_RETRIES = {}", MAX_RETRIES)
    println("THRESHOLD = {}", THRESHOLD)
    println("GREETING = {}", GREETING)
    println("above(0.5) = {}", above_threshold(0.5))
    println("above(0.8) = {}", above_threshold(0.8))
}
"#;
    let dir = fresh_dir("static_items_typed");
    let path = write_source(&dir, "static_items_typed", src);
    let run = run_vm(&path);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    assert!(run.0.contains("MAX_RETRIES = 5"), "vm got: {:?}", run.0);
    assert!(run.0.contains("THRESHOLD = 0.75"), "vm got: {:?}", run.0);
    assert!(run.0.contains("GREETING = hello"), "vm got: {:?}", run.0);
    assert!(run.0.contains("above(0.5) = false"), "vm got: {:?}", run.0);
    assert!(run.0.contains("above(0.8) = true"), "vm got: {:?}", run.0);
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&path, &cl_dir).expect("cranelift build");
    let native = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert!(
        native.0.contains("MAX_RETRIES = 5"),
        "native got: {:?}",
        native.0
    );
    assert!(
        native.0.contains("THRESHOLD = 0.75"),
        "native got: {:?}",
        native.0
    );
    assert!(
        native.0.contains("GREETING = hello"),
        "native got: {:?}",
        native.0
    );
    assert!(
        native.0.contains("above(0.5) = false"),
        "native got: {:?}",
        native.0
    );
    assert!(
        native.0.contains("above(0.8) = true"),
        "native got: {:?}",
        native.0
    );
}

#[test]
fn const_vec_string_contains_reads_the_real_vector_across_tiers() {
    // A top-level `const` whose initializer is heap-typed (`Vec<String>`
    // here) has no `ConstValue` representation, so `consts.get(def)` found
    // nothing and every reference fell through to the `FnRef` fallback -
    // treating the const like a named function and handing its bogus
    // pointer to `gos_rt_vec_contains_str` as the receiver. `contains`
    // silently returned `false` for a value that was present, only in the
    // compiled tiers (the VM resolves globals through a separate path).
    // Fix: a path resolving to such a const now re-lowers its declaration
    // expression at the reference site instead of falling to `FnRef`.
    let src = r#"
const GOOS: Vec<String> = Vec::from(["linux", "darwin"])

fn is_goos(s: String) -> bool {
    GOOS.contains(s)
}

fn check() -> bool {
    let v = Vec::from(["bandwidth", "linux"])
    let last = &v[v.len() - 1]
    let found = is_goos(*last)
    let mut sum = 0
    for i in 0..v.len() {
        sum = sum + i
    }
    found
}

fn main() {
    println("detected: {}", check())
}
"#;
    let expected = "detected: true\n";
    let dir = fresh_dir("const_vec_string_contains");
    let path = write_source(&dir, "const_vec_string_contains", src);

    let vm = run_vm(&path);
    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(vm.0, expected);

    // `check` statically contains a loop, so the in-process JIT eagerly
    // promotes it on its first call without needing `GOS_JIT_ONLY`.
    let jit = Command::new(gos_bin())
        .arg("run")
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn jit run");
    let jit = run_with_timeout(jit);
    assert_eq!(jit.2, Some(0), "jit stderr: {}", jit.1);
    assert_eq!(jit.0, expected);

    for release in [false, true] {
        let scratch = dir.join(if release { "release" } else { "debug" });
        fs::create_dir_all(&scratch).unwrap();
        let bin = build_native_with_flag(&path, &scratch, release).expect("native build");
        let native = run_native(&bin);
        assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
        assert_eq!(native.0, expected);
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn wrapping_arithmetic_operators_match_across_tiers() {
    let src = r"
fn main() {
    let max: i64 = 9223372036854775807
    println(max +% 1)
    println(max *% 2)
    println(127i8 +% 1i8)
    println(200u8 +% 100u8)
}
";
    let expected = "-9223372036854775808\n-2\n-128\n44\n";
    let dir = fresh_dir("wrapping_arithmetic_operators");
    let path = write_source(&dir, "wrapping_arithmetic_operators", src);

    let vm = run_vm(&path);
    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(vm.0, expected);

    let jit = Command::new(gos_bin())
        .arg("run")
        .env("GOS_JIT_ONLY", "main")
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn forced JIT");
    let jit = run_with_timeout(jit);
    assert_eq!(jit.2, Some(0), "jit stderr: {}", jit.1);
    assert_eq!(jit.0, expected);

    for release in [false, true] {
        let scratch = dir.join(if release { "release" } else { "debug" });
        fs::create_dir_all(&scratch).unwrap();
        let bin = build_native_with_flag(&path, &scratch, release).expect("native build");
        let native = run_native(&bin);
        assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
        assert_eq!(native.0, expected);
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn fixed_u8_array_to_vec_preserves_packed_positions_across_tiers() {
    let src = r"
fn main() {
    let mut bytes = [0u8; 10]
    bytes[1] = 11u8
    bytes[7] = 77u8
    let values: Vec<u8> = bytes.to_vec()
    println(values[1])
    println(values[7])
    println(values.len())
}
";
    let expected = "11\n77\n10\n";
    let dir = fresh_dir("fixed_u8_array_to_vec");
    let path = write_source(&dir, "fixed_u8_array_to_vec", src);

    let vm = run_vm(&path);
    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(vm.0, expected);

    let jit = Command::new(gos_bin())
        .arg("run")
        .env("GOS_JIT_ONLY", "main")
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn forced JIT");
    let jit = run_with_timeout(jit);
    assert_eq!(jit.2, Some(0), "jit stderr: {}", jit.1);
    assert_eq!(jit.0, expected);

    for release in [false, true] {
        let scratch = dir.join(if release { "release" } else { "debug" });
        fs::create_dir_all(&scratch).unwrap();
        let bin = build_native_with_flag(&path, &scratch, release).expect("native build");
        let native = run_native(&bin);
        assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
        assert_eq!(native.0, expected);
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn shared_borrow_of_indexed_string_passes_the_string_value() {
    let src = r"
use std::env
use std::strconv

fn main() {
    let args = env::args()
    println(strconv::parse_i64(args[0]).unwrap_or(9))
}
";
    let dir = fresh_dir("indexed_string_shared_borrow");
    let path = write_source(&dir, "indexed_string_shared_borrow", src);

    let vm = Command::new(gos_bin())
        .arg("run")
        .env("GOS_JIT", "0")
        .arg(&path)
        .arg("123")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn VM");
    let vm = run_with_timeout(vm);
    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(vm.0, "123\n");

    for release in [false, true] {
        let scratch = dir.join(if release { "release" } else { "debug" });
        fs::create_dir_all(&scratch).unwrap();
        let bin = build_native_with_flag(&path, &scratch, release).expect("native build");
        let native = Command::new(bin)
            .arg("123")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn native");
        let native = run_with_timeout(native);
        assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
        assert_eq!(native.0, "123\n");
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn static_mut_assignment_does_not_error_at_runtime() {
    // `static mut COUNTER: i64 = 0; COUNTER = 100` previously
    // failed in the VM with "name `COUNTER` is not bound in
    // this scope" because `eval_assign` only consulted the
    // goroutine-local `Env`, not the tree-walker's globals
    // table where statics live. The tree-walker's `eval_path`
    // already resolves the read against globals, so the
    // asymmetry was invisible until a write hit. This test
    // pins the no-error contract; it does *not* yet assert
    // that the read sees the written value, because the
    // bytecode VM's globals are a separate `Arc<HashMap>` and
    // sharing storage with the tree-walker is an open
    // follow-up. The contract today: writes accept, reads
    // return the initial value (consistent with cranelift on
    // this build).
    let src = r#"
static mut N: i64 = 7

fn main() {
    println("start = {}", N)
    N = 42
    println("after = {}", N)
}
"#;
    let dir = fresh_dir("static_mut_assign");
    let path = write_source(&dir, "static_mut_assign", src);
    let run = run_vm(&path);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    assert!(
        run.0.contains("start = 7"),
        "vm: expected static-mut initial value visible, got: {:?}",
        run.0,
    );
    assert!(
        run.0.contains("after ="),
        "vm: expected the post-assign println to run instead of erroring, got: {:?}",
        run.0,
    );
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&path, &cl_dir).expect("cranelift build");
    let native = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert!(
        native.0.contains("start = 7"),
        "native: expected static-mut initial value visible, got: {:?}",
        native.0,
    );
    assert!(
        native.0.contains("after ="),
        "native: expected the post-assign println to run instead of erroring, got: {:?}",
        native.0,
    );
}

#[test]
fn at_binding_subpattern_actually_filters_match_arms() {
    // `x @ literal` and `x @ lo..=hi` previously dropped the
    // subpattern at the AST→HIR boundary: `lower_pat_kind`
    // destructured `AstPatKind::Ident { name, mutability, .. }`
    // with `..` swallowing the `subpattern` field. Both VM and
    // cranelift always picked the first arm with a stale binding;
    // cranelift additionally bound `x` to a heap pointer instead
    // of the integer value (a representation-drift symptom).
    // The fix introduces `HirPatKind::At { name, mutable, sub }`
    // and threads it through every consumer (HIR walker, MIR
    // match lowering, exhaustiveness check, tree-walker
    // pattern matchers).
    let src = r#"
fn classify(n: i64) -> String {
    match n {
        x @ 0 => format("zero ({})", x),
        x @ 1..=3 => format("small {}", x),
        x @ 4..=10 => format("medium {}", x),
        x => format("other {}", x),
    }
}

fn main() {
    let inputs = [0, 1, 2, 3, 4, 7, 10, 11, -1]
    for n in inputs {
        println("{}", classify(n))
    }
}
"#;
    let dir = fresh_dir("at_binding_filter");
    let path = write_source(&dir, "at_binding_filter", src);
    let run = run_vm(&path);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    let expected =
        "zero (0)\nsmall 1\nsmall 2\nsmall 3\nmedium 4\nmedium 7\nmedium 10\nother 11\nother -1\n";
    assert_eq!(
        run.0, expected,
        "vm: at-binding subpattern; got: {:?}",
        run.0
    );
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&path, &cl_dir).expect("cranelift build");
    let native = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert_eq!(
        native.0, expected,
        "native: at-binding subpattern; got: {:?}",
        native.0
    );
}

#[test]
fn continue_in_for_vec_iter_advances_index() {
    // `for x in xs.iter() { if cond { continue } body }`
    // previously livelocked the bytecode VM for the same reason
    // as the for-range case: the vec-iter fast path's `continue`
    // skipped the index increment that lives between the body
    // and the back-edge.
    let src = r#"
fn main() {
    let xs: Vec<i64> = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10].to_vec()
    let mut acc: i64 = 0
    for x in xs.iter() {
        if *x % 3 == 0 {
            continue
        }
        acc = acc + *x
    }
    println("acc={}", acc)
}
"#;
    let dir = fresh_dir("continue_for_vec_iter");
    let path = write_source(&dir, "continue_for_vec_iter", src);
    let run = run_vm(&path);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    assert!(
        run.0.contains("acc=37"),
        "vm: expected acc=37, got: {:?}",
        run.0
    );
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&path, &cl_dir).expect("cranelift build");
    let native = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert!(
        native.0.contains("acc=37"),
        "native: expected acc=37, got: {:?}",
        native.0
    );
}

#[test]
fn result_option_payload_literal_matches_correct_arm() {
    // `Ok(1)` / `Ok(2)` must route to different arms in both VM and compiled.
    let src = r#"
fn classify(r: Result<i64, i64>) -> &str {
    match r {
        Ok(1) => "one",
        Ok(2) => "two",
        Ok(_) => "other-ok",
        Err(_) => "err",
    }
}

fn pick(o: Option<i64>) -> &str {
    match o {
        Some(10) => "ten",
        Some(20) => "twenty",
        None => "none",
        _ => "other",
    }
}

fn main() {
    println("{}", classify(Ok(1)))
    println("{}", classify(Ok(2)))
    println("{}", classify(Ok(99)))
    println("{}", classify(Err(0)))
    println("{}", pick(Some(10)))
    println("{}", pick(Some(20)))
    println("{}", pick(None))
}
"#;
    let expected = "one\ntwo\nother-ok\nerr\nten\ntwenty\nnone\n";
    let dir = fresh_dir("payload_literal_match");
    let path = write_source(&dir, "payload_literal_match", src);
    let run = run_vm(&path);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    assert_eq!(run.0, expected, "vm: got {:?}", run.0);
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&path, &cl_dir).expect("cranelift build");
    let native = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert_eq!(native.0, expected, "native: got {:?}", native.0);
}

#[test]
fn unary_prefix_at_line_start_breaks_statement() {
    // `&s`, `*p`, `-n` at the start of a line after a
    // semicolonless statement must parse as a new statement, not
    // as a binary continuation of the prior expression. Before the
    // fix, `let s = "hi"\n&s |> ...` was glued into `let s = "hi" &
    // s |> ...` and resolution failed with "cannot find `s` in
    // this scope".
    let src = r#"
use std::{iter, strings}

fn main() {
    let s = "alpha\nbeta"
    &s |> strings::lines |> |v| iter::for_each(v, |l| println("{}", l))

    let n = 5
    -n
    println("post-neg")

    let v = 42
    let p = &v
    *p
    println("post-deref={}", *p)
}
"#;
    let expected = "alpha\nbeta\npost-neg\npost-deref=42\n";
    let dir = fresh_dir("unary_line_start");
    let path = write_source(&dir, "unary_line_start", src);
    let run = run_vm(&path);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    assert_eq!(run.0, expected, "vm: got {:?}", run.0);
}

#[test]
fn try_operator_in_macro_arg_propagates_early_return() {
    // `?` inside a macro argument - e.g. `print!("{}", expr?)` -
    // must propagate the early-return from the enclosing function,
    // not silently pass the `Err(...)` value through to the macro.
    // The bug: eval_expr_to_value was converting Flow::Return(v)
    // to Ok(v), so the Err value was passed to __concat / print
    // instead of returning early from `cat`.
    let src = r#"
use std::{errors, fs}

fn cat(f: String) -> Result<(), errors::Error> {
    Ok(print("{}", fs::read_to_string(f)?))
}

fn main() {
    if let Err(e) = cat("/nonexistent-regression") {
        println("caught: {e}")
    }
}
"#;
    let expected = "caught: not found: /nonexistent-regression\n";
    let dir = fresh_dir("try_in_macro_arg");
    let path = write_source(&dir, "try_in_macro_arg", src);
    let run = run_vm(&path);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    assert_eq!(run.0, expected, "vm: got {:?}", run.0);
}

#[test]
fn llvm_named_fn_passed_to_sort_by_emits_typed_store() {
    // `Operand::FnRef` in the LLVM lowerer used to always emit
    // `ptrtoint ptr @"name" to i64`, but the destination slot is
    // ptr-typed when `FnDef → ptr` (e.g. when a named fn is passed
    // as a `Fn(i64,i64)->i64` arg to `sort_by`). The emitted
    // `store ptr %i64_value, ptr %slot` then fails opt validation
    // and the whole module silently falls back to Cranelift.
    let src = r#"
fn cmp(a: i64, b: i64) -> i64 { a - b }
fn main() {
    let mut xs = [5, 2, 4, 1, 3].to_vec()
    xs.sort_by(cmp)
    for x in xs.iter() { println("{}", *x) }
}
"#;
    let dir = fresh_dir("llvm_sortby_named_fn");
    let path = write_source(&dir, "llvm_sortby_named_fn", src);
    let scratch = dir.join("rel");
    fs::create_dir_all(&scratch).unwrap();
    let bin = build_native_release(&path, &scratch).expect("llvm release build");
    let out = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(out.2, Some(0), "native: stderr: {}", out.1);
    assert_eq!(out.0, "1\n2\n3\n4\n5\n");
}

#[test]
fn llvm_vec_of_tuples_index_returns_both_fields() {
    // `Vec<(i64, f64)>`-style tuple element types used to leave
    // the operand of `xs.push((1, 1.5))` typed as
    // `(Var, Var)` in MIR. The LLVM lowerer's `slot_count` for
    // a tuple with `Var` elements returned `None`, the alloca
    // shrank to 1 slot, the second-slot store overflowed, and
    // the subsequent `gos_rt_vec_get_ptr → memcpy` round-trip
    // surfaced garbage in the f64 field.
    let src = r#"
fn main() {
    let mut xs: Vec<(i64, f64)> = [].to_vec()
    xs.push((1, 1.5))
    xs.push((2, 2.5))
    let i: i64 = 1
    let p = xs[i]
    println("{} {}", p.0, p.1)
}
"#;
    let dir = fresh_dir("llvm_vec_tuple_index");
    let path = write_source(&dir, "llvm_vec_tuple_index", src);
    let scratch = dir.join("rel");
    fs::create_dir_all(&scratch).unwrap();
    let bin = build_native_release(&path, &scratch).expect("llvm release build");
    let out = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(out.2, Some(0), "native: stderr: {}", out.1);
    assert_eq!(out.0, "2 2.5\n");
}

#[test]
fn llvm_tuple_return_array_then_scalar_preserves_both() {
    // Returning an `([f64; N], f64)` tuple used to corrupt every
    // slot past the first: the temporary tuple local was typed
    // `([Var; 4], Var)`, `slot_count` collapsed to `None`, and
    // the alloca undersized to 1 slot. The aggregate-store then
    // overflowed and the subsequent memcpy into the return slot
    // copied stack garbage.
    let src = r#"
fn make() -> ([f64; 4], f64) {
    ([1.5, 2.5, 3.5, 4.5], 99.0)
}
fn main() {
    let pair = make()
    println("{} {} {} {} | {}", pair.0[0], pair.0[1], pair.0[2], pair.0[3], pair.1)
}
"#;
    let dir = fresh_dir("llvm_tuple_arr_scalar");
    let path = write_source(&dir, "llvm_tuple_arr_scalar", src);
    let scratch = dir.join("rel");
    fs::create_dir_all(&scratch).unwrap();
    let bin = build_native_release(&path, &scratch).expect("llvm release build");
    let out = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(out.2, Some(0), "native: stderr: {}", out.1);
    assert_eq!(out.0, "1.5 2.5 3.5 4.5 | 99\n");
}

#[test]
fn llvm_projected_destinations_accept_calls_aggregates_and_collection_literals() {
    let src = r#"
use std::collections::{Map, Set, BTreeSet}

struct Inner {
    xs: [i64; 2],
    name: String,
}

struct Boxy {
    inner: Inner,
    map: Map<String, i64>,
    set: Set<i64>,
    tree: BTreeSet<i64>,
}

fn make_name(n: i64) -> String {
    format("n={}", n)
}

fn make_pair(a: i64) -> [i64; 2] {
    #[a, a + 1]
}

fn main() {
    let mut b = Boxy {
        inner: Inner { xs: #[0, 0], name: "" },
        map: {},
        set: #{},
        tree: BTreeSet::new(),
    }
    b.inner.name = make_name(7)
    b.inner.xs = make_pair(3)
    b.inner.xs[1] = 9
    b.map.insert("a", b.inner.xs[0])
    b.set.insert(b.inner.xs[1])
    b.tree.insert(4)
    println("{} {} {} {}", b.inner.name, b.map.get("a").unwrap(), b.set.contains(9), b.tree.contains(4))
}
"#;
    let expected = "n=7 3 true true\n";
    let dir = fresh_dir("llvm_projected_dest_literals");
    let path = write_source(&dir, "llvm_projected_dest_literals", src);
    let vm = run_vm(&path);
    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(vm.0, expected);
    let scratch = dir.join("rel");
    fs::create_dir_all(&scratch).unwrap();
    let bin = build_native_release(&path, &scratch).expect("llvm release build");
    let native = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert_eq!(native.0, expected);
}

#[test]
fn native_vec_swap_and_pop_preserve_multiword_elements() {
    let src = r#"
fn main() {
    let mut xs = #[(1, 2), (3, 4)]
    let _ = xs.swap(0, 1)
    match xs.pop() {
        Some(v) => println("{} {}", v.0, v.1),
        None => println("none"),
    }
}
"#;
    let expected = "1 2\n";
    let dir = fresh_dir("native_vec_swap_pop_multiword");
    let path = write_source(&dir, "native_vec_swap_pop_multiword", src);
    let vm = run_vm(&path);
    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(vm.0, expected);
    let scratch = dir.join("native");
    fs::create_dir_all(&scratch).unwrap();
    let bin = build_native(&path, &scratch).expect("native build");
    let native = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert_eq!(native.0, expected);
}

#[test]
fn llvm_tuple_return_from_nested_loop_keeps_second_slot() {
    // `return (a, b)` from inside a nested loop used to drop the
    // second slot - the temporary `(Var, Var)` tuple's alloca
    // sized to one slot, so the aggregate-store overflowed and
    // the memcpy into the return slot only carried 8 valid bytes.
    // fannkuch-shaped programs lost the checksum value (always 0).
    let src = r#"
fn fannkuch(_n: i64) -> (i64, i64) {
    let mut perm = [0, 1, 2, 3, 4]
    let mut max_flips = 0
    let mut checksum = 0
    let mut sign = true
    let mut nperm = 0
    loop {
        let mut flips = 0
        let mut k = perm[0]
        while k != 0 {
            let mut i = 0
            let mut j = k
            while i < j {
                let t = perm[i]
                perm[i] = perm[j]
                perm[j] = t
                i += 1
                j -= 1
            }
            k = perm[0]
            flips += 1
        }
        if flips > max_flips { max_flips = flips }
        checksum += if sign { flips } else { -flips }
        if nperm >= 30 {
            return (max_flips, checksum)
        }
        nperm += 1
        if sign {
            let t = perm[0]
            perm[0] = perm[1]
            perm[1] = t
            sign = false
        } else {
            let t = perm[1]
            perm[1] = perm[2]
            perm[2] = t
            sign = true
        }
    }
}
fn main() {
    let r = fannkuch(5)
    println("max={} checksum={}", r.0, r.1)
}
"#;
    let dir = fresh_dir("llvm_nested_loop_tuple_ret");
    let path = write_source(&dir, "llvm_nested_loop_tuple_ret", src);
    let scratch = dir.join("rel");
    fs::create_dir_all(&scratch).unwrap();
    let bin = build_native_release(&path, &scratch).expect("llvm release build");
    let out = run_native(&bin);
    let vm = run_vm(&path);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(out.2, Some(0), "native: stderr: {}", out.1);
    assert_eq!(out.0, vm.0, "native diverged from VM");
    // The exact value can't be 0 - that's the bug shape we're
    // guarding against.
    assert!(!out.0.contains("checksum=0\n"), "second slot dropped");
}

#[test]
fn json_render_adt_text_branch_does_not_free_uninit_pairs_vec() {
    // json::render(&adt) builds a temporary GosVec (pairs_vec) inside
    // lower_json_render_adt.  The insert_drops_at_returns pass used to
    // emit a gos_rt_vec_free for pairs_vec at every Return block -
    // including the text-mode arm where pairs_vec was never initialised.
    // That produced gos_rt_vec_free(stack_garbage) → segfault in
    // __GI___libc_free.  The fix: emit the free immediately in the JSON
    // arm and re-assign pairs_vec to 0 so the global drop pass skips it.
    let src = r#"
use std::encoding::json

struct Info { num: i64, label: String }

fn show(item: Info, as_json: bool) {
    if as_json {
        println("{}", json::render(item))
    } else {
        println("num={} label={}", item.num, item.label)
    }
}

fn main() {
    let it = Info { num: 42, label: "hello" }
    show(it, false)
}
"#;
    let dir = fresh_dir("json_render_text_branch");
    let path = write_source(&dir, "json_render_text_branch", src);
    let scratch = dir.join("bin");
    fs::create_dir_all(&scratch).unwrap();
    let bin = build_native(&path, &scratch).expect("build");
    let out = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(out.2, Some(0), "segfault in text branch; stderr: {}", out.1);
    assert_eq!(out.0, "num=42 label=hello\n");
}

#[test]
fn jit_pre_interns_array_index_label_string() {
    // Regression: a program that hits the bounds-check helper
    // path (any `arr[i]` with i64 index) would route through the
    // codegen helper that interns "array index" as the diagnostic
    // label. The pre-pass that pre-interns strings before the
    // parallel codegen phase missed this literal, so the first
    // bounds-checked array access in any body panicked with
    // `OfflineModule: declare_data called in parallel phase`.
    //
    // The fix: pre-intern "array index" alongside `""`, `" "`,
    // and `"<value>"` in the codegen prelude. spectral-norm's
    // `src[j]` access is the canonical trigger.
    let src = "fn main() {\n\
                   let xs: [i64; 4] = [1, 2, 3, 4]\n\
                   let mut sum: i64 = 0\n\
                   let n: i64 = 4\n\
                   let mut i: i64 = 0\n\
                   while i < n {\n\
                       sum += xs[i]\n\
                       i += 1\n\
                   }\n\
                   println(\"{}\", sum)\n\
               }\n";
    let dir = fresh_dir("jit_array_index_pre_intern");
    let path = write_source(&dir, "jit_array_index_pre_intern", src);
    let run = run_vm(&path);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(
        run.2,
        Some(0),
        "vm: expected clean exit, got {:?}; stderr: {}",
        run.2,
        run.1,
    );
    assert!(
        !run.1.contains("declare_data called in parallel phase"),
        "vm: regressed parallel-phase declare_data panic; stderr: {}",
        run.1
    );
    assert_eq!(run.0.trim_end(), "10");
}

#[test]
fn local_var_shadowing_module_does_not_capture_qualified_path() {
    // Regression: a local binding whose name matches an imported
    // module silently captured every `mod_name::item(...)` call
    // through the VM-tier's tree-walker fallback. `eval_path`
    // looked up the head segment in the env first, returning the
    // local's value (a String), and `apply()` of a non-callable
    // degraded to Unit. The LLVM AOT tier resolved correctly; the
    // VM tier did not - a parity gap that broke askq's
    // `provider::provider_endpoint_and_auth(&cfg, &provider)` call
    // (the local `provider: String` captured the call).
    //
    // The fix: multi-segment paths bypass the env-first lookup.
    // A path's head can only resolve to a module / type / trait -
    // never a local binding.
    let dir = fresh_dir("local_shadow_mod_path");
    fs::write(
        dir.join("project.toml"),
        "[project]\nid = \"example.com/shadow\"\nversion = \"0.0.1\"\n",
    )
    .expect("write project.toml");
    let src_dir = dir.join("src");
    fs::create_dir_all(&src_dir).expect("mk src dir");
    fs::write(
        src_dir.join("main.gos"),
        "mod prov;\n\
         fn main() {\n\
             let prov = \"local-string\"\n\
             let s = prov::greet(prov)\n\
             println(\"{}\", s)\n\
         }\n",
    )
    .expect("write main.gos");
    fs::write(
        src_dir.join("prov.gos"),
        "pub fn greet(who: String) -> String {\n\
             format(\"hello, {}\", who)\n\
         }\n",
    )
    .expect("write prov.gos");
    let main_path = src_dir.join("main.gos");
    let run = run_vm(&main_path);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(
        run.2,
        Some(0),
        "vm: expected clean exit, got {:?}; stderr: {}",
        run.2,
        run.1
    );
    assert_eq!(
        run.0.trim_end(),
        "hello, local-string",
        "vm: expected greet to run, got stdout: {:?}",
        run.0,
    );
}

#[test]
fn aggregate_alloc_loop_reclaims_deterministically() {
    // Stress: a tight loop that allocates a heap aggregate every
    // iteration and discards it. The MIR drop pass must emit a
    // matching `gos_rt_aggr_free` per iteration. The test verifies:
    //   - the loop produces the correct numeric result (the drop
    //     pass does not free values still held in locals);
    //   - the process exits with status 0 (no segfault, no
    //     double-free in the drop pass);
    //   - all three tiers agree.
    let src = "struct Pair { a: i64, b: i64 }\n\
               fn make(i: i64) -> Pair { Pair { a: i, b: i * 2 } }\n\
               fn main() {\n\
                   let mut total: i64 = 0\n\
                   let mut i: i64 = 0\n\
                   while i < 10000 {\n\
                       let p = make(i)\n\
                       total += p.a + p.b\n\
                       i += 1\n\
                   }\n\
                   println(\"{}\", total)\n\
               }\n";
    let expected = (0i64..10000).map(|i| i + i * 2).sum::<i64>();
    let dir = fresh_dir("tracing_gc_loop");
    let path = write_source(&dir, "tracing_gc_loop", src);

    // VM tier
    let run = {
        let child = Command::new(gos_bin())
            .arg("run")
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn gos");
        run_with_timeout(child)
    };
    assert_eq!(
        run.2,
        Some(0),
        "vm: expected clean exit, got {:?}; stderr: {}",
        run.2,
        run.1
    );
    assert_eq!(run.0.trim_end(), expected.to_string(), "vm output mismatch");

    // Debug LLVM tier
    let scratch = dir.join("bin");
    fs::create_dir_all(&scratch).unwrap();
    let bin = build_native(&path, &scratch).expect("build debug");
    let out = {
        let child = Command::new(&bin)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn debug binary");
        run_with_timeout(child)
    };
    assert_eq!(
        out.2,
        Some(0),
        "debug: expected clean exit, got {:?}; stderr: {}",
        out.2,
        out.1
    );
    assert_eq!(
        out.0.trim_end(),
        expected.to_string(),
        "debug output mismatch"
    );

    // Release LLVM tier
    let bin = build_native_release(&path, &scratch).expect("build release");
    let out = {
        let child = Command::new(&bin)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn release binary");
        run_with_timeout(child)
    };
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(
        out.2,
        Some(0),
        "release: expected clean exit, got {:?}; stderr: {}",
        out.2,
        out.1
    );
    assert_eq!(
        out.0.trim_end(),
        expected.to_string(),
        "release output mismatch"
    );
}

#[test]
fn named_struct_constructor_is_available_on_vm_and_native_tiers() {
    let src = r#"
struct Pair { left: i64, right: i64 }

fn sum(p: Pair) -> i64 {
    p.left + p.right
}

fn main() {
    let p = Pair { left: 20i64, right: 22i64 }
    println("{}", sum(p))
}
"#;
    let dir = fresh_dir("named_struct_ctor");
    let path = write_source(&dir, "named_struct_ctor", src);

    let run = run_vm(&path);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    assert_eq!(run.0.trim_end(), "42", "vm output mismatch");

    let debug_dir = dir.join("debug");
    fs::create_dir_all(&debug_dir).unwrap();
    let debug_bin = build_native(&path, &debug_dir).expect("debug build");
    let debug = run_native(&debug_bin);
    assert_eq!(debug.2, Some(0), "debug stderr: {}", debug.1);
    assert_eq!(debug.0.trim_end(), "42", "debug output mismatch");

    let release_dir = dir.join("release");
    fs::create_dir_all(&release_dir).unwrap();
    let release_bin = build_native_release(&path, &release_dir).expect("release build");
    let release = run_native(&release_bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(release.2, Some(0), "release stderr: {}", release.1);
    assert_eq!(release.0.trim_end(), "42", "release output mismatch");
}

#[test]
fn collection_display_literals_match_vm_and_native_tiers() {
    let src = r#"
use std::collections::BTreeSet

#[derive(Debug)]
struct Rec { name: String, nums: Vec<i64> }

fn main() {
    println("{}", (1, "x", true))
    let mut m = {"a": 1, "b": 2}
    println("{}", (m, #[1, 2, 3]))
    println("{}", #{3, 1, 2})
    println("{}", BTreeSet::from([3, 1, 2]))
    println("{:?}", Rec { name: "r", nums: #[1, 2] })
}
"#;
    let expected = "\
(1, x, true)
({\"a\": 1, \"b\": 2}, #[1, 2, 3])
#{1, 2, 3}
#{1, 2, 3}
Rec { name: \"r\", nums: #[1, 2] }
";
    let dir = fresh_dir("collection_display_literals");
    let path = write_source(&dir, "collection_display_literals", src);

    let vm = run_vm(&path);
    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(vm.0, expected, "vm output mismatch");

    for release in [false, true] {
        let scratch = dir.join(if release { "release" } else { "debug" });
        fs::create_dir_all(&scratch).unwrap();
        let bin = build_native_with_flag(&path, &scratch, release).expect("native build");
        let native = run_native(&bin);
        assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
        assert_eq!(native.0, expected, "native output mismatch");
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn aggregate_return_chain_outlives_callee_frame() {
    // Stresses the aggregate-return heap-copy discipline: every
    // iteration calls a function that builds an aggregate on the
    // callee's frame; codegen copies it to the heap at return so
    // the pointer outlives the popped frame, and the caller uses
    // both fields of the returned tuple. The just-returned
    // aggregate must stay intact until the caller consumes it.
    let src = "fn pair_of(i: i64) -> (i64, i64) {\n\
                   (i, i * 7)\n\
               }\n\
               fn main() {\n\
                   let mut sum: i64 = 0\n\
                   let mut i: i64 = 0\n\
                   while i < 5000 {\n\
                       let p = pair_of(i)\n\
                       sum += p.0 + p.1\n\
                       i += 1\n\
                   }\n\
                   println(\"{}\", sum)\n\
               }\n";
    let expected = (0i64..5000).map(|i| i + i * 7).sum::<i64>();
    let dir = fresh_dir("tracing_gc_return_chain");
    let path = write_source(&dir, "tracing_gc_return_chain", src);

    let run = {
        let child = Command::new(gos_bin())
            .arg("run")
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn gos");
        run_with_timeout(child)
    };
    assert_eq!(
        run.2,
        Some(0),
        "vm: expected clean exit, got {:?}; stderr: {}",
        run.2,
        run.1
    );
    assert_eq!(
        run.0.trim_end(),
        expected.to_string(),
        "vm aggregate-return chain mismatch (rooted-return discipline broken?)"
    );

    let scratch = dir.join("bin");
    fs::create_dir_all(&scratch).unwrap();
    let bin = build_native_release(&path, &scratch).expect("build release");
    let out = {
        let child = Command::new(&bin)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn release binary");
        run_with_timeout(child)
    };
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(
        out.2,
        Some(0),
        "release: expected clean exit, got {:?}; stderr: {}",
        out.2,
        out.1
    );
    assert_eq!(
        out.0.trim_end(),
        expected.to_string(),
        "release aggregate-return chain mismatch"
    );
}

#[test]
fn a_ref_self_method_on_a_primitive_receives_an_address() {
    // A `&self` impl on a primitive read its receiver as an address while the
    // call site passed the value, so `*self` dereferenced the value itself.
    // The receiver is borrowed now, from a local, an element, a loop binding,
    // and a type parameter that resolved to a primitive alike; `&mut self`
    // writes back through that borrow.
    let src = r#"
trait Doubler { fn twice(&self) -> i64 }
impl Doubler for i64 { fn twice(&self) -> i64 { *self * 2 } }

trait Bump { fn bump(&mut self) }
impl Bump for i64 { fn bump(&mut self) { *self += 1 } }

trait Area { fn area(&self) -> i64 }
struct Square { side: i64 }
impl Area for Square { fn area(&self) -> i64 { self.side * self.side } }

fn sum_twice<T: Doubler>(items: Vec<T>) -> i64 {
    let mut total = 0
    for item in items { total += item.twice() }
    total
}

fn first_area<T: Area>(items: Vec<T>) -> i64 { items[0].area() }

fn main() {
    let x = 5
    let xs = #[7, 8]
    let mut m = 1
    m.bump()
    let mut loop_total = 0
    for v in xs { loop_total += v.twice() }
    println("local {}", x.twice())
    println("element {}", xs[0].twice())
    println("loop {}", loop_total)
    println("mut {}", m)
    println("generic-scalar {}", sum_twice(#[1, 2, 3]))
    println("generic-aggregate {}", first_area(#[Square { side: 4 }]))
}
"#;
    let dir = fresh_dir("ref_self_primitive");
    let path = write_source(&dir, "ref_self_primitive", src);
    let expected = [
        "local 10",
        "element 14",
        "loop 30",
        "mut 2",
        "generic-scalar 12",
        "generic-aggregate 16",
    ];
    let run = run_vm(&path);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    for line in expected {
        assert!(run.0.contains(line), "vm: missing `{line}` in {:?}", run.0);
    }
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&path, &cl_dir).expect("native build");
    let native = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    for line in expected {
        assert!(
            native.0.contains(line),
            "native: missing `{line}` in {:?}",
            native.0
        );
    }
}

/// Goroutines stranded on several channels must not hold the process at
/// exit. The root cohort reports what it abandons and the pool drain shares
/// its deadline, so the run ends on both tiers instead of waiting forever.
///
/// The spawns are `main`'s own: it is the one function whose children may
/// attach to the root cohort, everywhere else `GT0086` requires a block.
#[test]
fn stranded_goroutines_on_several_channels_still_exit() {
    let src = r#"
use std::sync::channel

fn main() {
    for r in 1..=3 {
        let tx, rx = channel()
        for i in 0..2 {
            let tx = tx
            spawn(|| { tx.send(r * 10 + i) }, reason: "w")
        }
        let first = rx.recv()
        println("round {r}: {first}")
    }
    println("main done")
}
"#;
    let dir = fresh_dir("stranded_channels_exit");
    let path = write_source(&dir, "stranded_channels_exit", src);

    let run = run_vm(&path);
    assert_eq!(run.2, Some(0), "vm did not exit: stderr={}", run.1);
    assert!(run.0.contains("main done"), "vm stdout: {:?}", run.0);
    assert!(
        run.1.contains("had not finished"),
        "the drain must name what it left running: {:?}",
        run.1
    );

    let native_dir = dir.join("native");
    fs::create_dir_all(&native_dir).unwrap();
    let bin = build_native(&path, &native_dir).expect("native build");
    let native = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(native.2, Some(0), "native did not exit: stderr={}", native.1);
    assert!(
        native.1.contains("had not finished"),
        "native drain report: {:?}",
        native.1
    );
}

/// A goroutine profile taken while children are running names them on
/// both tiers, rather than answering the format header alone.
#[test]
fn goroutine_profile_names_live_goroutines() {
    let src = r#"
use std::pprof
use std::time

fn main() {
    let _ = cohort {
        for i in 0..3 {
            spawn(|| { time::sleep(400) }, reason: "sleeper")
        }
        time::sleep(120)
        let p = pprof::goroutine_profile()
        println("{p}")
    }
    println("done")
}
"#;
    let dir = fresh_dir("goroutine_profile_live");
    let path = write_source(&dir, "goroutine_profile_live", src);

    let run = run_vm(&path);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    assert_eq!(
        run.0.matches("samples=1").count(),
        3,
        "vm profile must carry one sample per live goroutine: {:?}",
        run.0
    );

    let native_dir = dir.join("native");
    fs::create_dir_all(&native_dir).unwrap();
    let bin = build_native(&path, &native_dir).expect("native build");
    let native = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert_eq!(
        native.0.matches("samples=1").count(),
        3,
        "native profile: {:?}",
        native.0
    );
}

/// An `errors::Error` shows its message through a `Result` whose `Ok` type
/// holds a user struct, which routes the operand through the walk that
/// renders types supplying their own rendering.
#[test]
fn error_displays_its_message_through_a_struct_carrying_result() {
    let src = r#"
use std::errors
struct S { value: i64 }

fn direct() -> Result<Vec<S>, errors::Error> { Err(errors::new("boom")) }

fn wrapped() -> Result<S, errors::Error> {
    Err(errors::wrap(errors::new("root"), "outer"))
}

fn main() {
    let a = direct()
    let b = wrapped()
    println("display {a}")
    println("debug {a:?}")
    println("chain {b}")
}
"#;
    let dir = fresh_dir("error_display_struct_result");
    let path = write_source(&dir, "error_display_struct_result", src);
    let expected = ["display Err(boom)", "debug Err(boom)", "chain Err(outer: root)"];

    let run = run_vm(&path);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    for line in expected {
        assert!(run.0.contains(line), "vm: missing `{line}` in {:?}", run.0);
    }

    let native_dir = dir.join("native");
    fs::create_dir_all(&native_dir).unwrap();
    let bin = build_native(&path, &native_dir).expect("native build");
    let native = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    for line in expected {
        assert!(
            native.0.contains(line),
            "native: missing `{line}` in {:?}",
            native.0
        );
    }
}

/// A struct read out of a `Map` and handed back through `Ok(..)` keeps its
/// container fields alive for the caller, so the entry the map still holds
/// stays readable on every later lookup.
///
/// The payload wrap's field retains are keyed on the payload's type, not on
/// any local the drop pass owns, so a body owning nothing else still needs
/// them. Reading the map a second time is what makes a missing retain visible:
/// the first read's release frees the entry's own `Vec`, and its length then
/// reads as whatever the allocator handed out next.
#[test]
fn a_map_entry_survives_being_handed_back_through_a_result() {
    let src = r#"
struct Def { name: String, cols: Vec<String> }
struct Eng { tables: Map<String, Def> }

fn get_def(eng: Eng, table: String) -> Result<Def, String> {
    match eng.tables.get(table) {
        Some(d) => return Ok(d),
        None => return Err("no such table: " + table),
    }
}

fn probe(eng: &mut Eng, table: String) -> i64 {
    match get_def(*eng, table) {
        Ok(d) => d.cols.len(),
        Err(_) => -1,
    }
}

fn create(eng: &mut Eng, name: String, cols: Vec<String>) {
    let mut def = Def { name: name, cols: #[] }
    for col in cols {
        def.cols.push(col)
    }
    eng.tables.insert(name, def)
}

fn main() {
    let mut eng = Eng { tables: Map::new() }
    create(&mut eng, "users", #["username", "email", "password", "role", "created_at"])
    println("{} {} {}", probe(&mut eng, "users"), probe(&mut eng, "users"), probe(&mut eng, "users"))
}
"#;
    let dir = fresh_dir("map_entry_through_result");
    let path = write_source(&dir, "map_entry_through_result", src);
    let expected = "5 5 5";

    let run = run_vm(&path);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    assert!(run.0.contains(expected), "vm: {:?}", run.0);

    let native_dir = dir.join("native");
    fs::create_dir_all(&native_dir).unwrap();
    let bin = build_native(&path, &native_dir).expect("native build");
    let native = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert!(native.0.contains(expected), "native: {:?}", native.0);
}

/// A `Vec` element aggregate carrying a `Map` field keeps one table per
/// holder across every copy the vec takes: the array-to-Vec conversion, the
/// struct field the vec lands in, and the promoted body that reads the
/// element's table in a loop.
#[test]
fn vec_of_map_field_structs_survives_copies_across_tiers() {
    let src = r"
struct Cell {
    m: Map<i64, i64>
}

struct Network {
    computers: Vec<Cell>
}

fn from_array() -> i64 {
    let mut best = 0
    for _ in 0..1000 {
        let cells = Vec::from([Cell { m: {0: 1} }; 2])
        let r = cells[0].m.len() + cells[1].m.len()
        if r > best {
            best = r
        }
    }
    best
}

fn through_field() -> i64 {
    let mut computers = #[]
    for _ in 0..5 {
        computers.push(Cell { m: {0: 1} })
    }
    let network = Network { computers: computers }
    let mut signal = 0
    for _ in 0..1000 {
        for i in 0..5 {
            signal += network.computers[i].m.len()
        }
    }
    signal
}

fn main() {
    println(from_array())
    println(through_field())
    let source = #[Cell { m: {0: 1} }]
    let mut copy = source.clone()
    copy[0].m.insert(9, 90)
    println(source[0].m.len())
    println(copy[0].m.len())

    let sets = #[#{1}]
    let mut set_copy = sets.clone()
    set_copy[0].insert(9)
    println(sets[0].len())
    println(set_copy[0].len())
}
";
    let expected = "2\n5000\n1\n2\n1\n2\n";
    let dir = fresh_dir("vec_of_map_field_structs");
    let path = write_source(&dir, "vec_of_map_field_structs", src);

    let vm = run_vm(&path);
    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(vm.0, expected);

    let jit = Command::new(gos_bin())
        .arg("run")
        .env("GOS_JIT_ONLY", "main")
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn forced JIT");
    let jit = run_with_timeout(jit);
    assert_eq!(jit.2, Some(0), "jit stderr: {}", jit.1);
    assert_eq!(jit.0, expected);

    for release in [false, true] {
        let scratch = dir.join(if release { "release" } else { "debug" });
        fs::create_dir_all(&scratch).unwrap();
        let bin = build_native_with_flag(&path, &scratch, release).expect("native build");
        let native = run_native(&bin);
        assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
        assert_eq!(native.0, expected);
    }
    let _ = fs::remove_dir_all(&dir);
}
