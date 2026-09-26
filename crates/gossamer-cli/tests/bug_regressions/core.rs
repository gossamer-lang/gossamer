// Regression tests covering shapes that previously crashed,
// mis-dispatched, or surfaced wrong values. Each `#[test]` runs
// a small Gossamer program through `gos` (or `gos build`)
// and asserts the user-visible output. A regression in any of
// the underlying fixes turns the test red. Tests are named
// after the property under test, not after a bug number - the
// file location is the regression-guard context.


use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const PER_RUN_TIMEOUT: Duration = Duration::from_mins(1);

fn gos_bin() -> PathBuf {
    PathBuf::from(env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

fn fresh_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!(
        "gos-bugreg-{pid}-{n}-{tag}",
        pid = std::process::id(),
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn run_with_timeout(mut child: std::process::Child) -> (String, String, Option<i32>) {
    let deadline = Instant::now() + PER_RUN_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                std::thread::sleep(Duration::from_millis(40));
            }
            Err(_) => break,
        }
    }
    let out = child.wait_with_output().expect("wait_with_output");
    (
        String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n"),
        String::from_utf8_lossy(&out.stderr).replace("\r\n", "\n"),
        out.status.code(),
    )
}

fn run_vm(src: &Path) -> (String, String, Option<i32>) {
    let child = Command::new(gos_bin())
        .arg("run")
        .arg(src)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn gos");
    run_with_timeout(child)
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.extension()
        .and_then(|s| s.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
}

fn build_native(src: &Path, scratch: &Path) -> Result<PathBuf, String> {
    build_native_with_flag(src, scratch, /*release=*/ false)
}

fn build_native_release(src: &Path, scratch: &Path) -> Result<PathBuf, String> {
    build_native_with_flag(src, scratch, /*release=*/ true)
}

fn build_native_with_flag(src: &Path, scratch: &Path, release: bool) -> Result<PathBuf, String> {
    let mut cmd = Command::new(gos_bin());
    cmd.arg("build");
    if release {
        cmd.arg("--release");
    }
    cmd.arg("--out-dir").arg(scratch).arg(src);
    let out = cmd.output().expect("spawn gos build");
    if !out.status.success() {
        return Err(format!(
            "gos build{} failed:\n  stderr: {}",
            if release { " --release" } else { "" },
            String::from_utf8_lossy(&out.stderr),
        ));
    }
    for entry in fs::read_dir(scratch)
        .map_err(|e| format!("read_dir: {e}"))?
        .flatten()
    {
        let p = entry.path();
        if p.is_file() && is_executable(&p) {
            return Ok(p);
        }
    }
    Err(format!("no binary in {}", scratch.display()))
}

fn run_native(bin: &Path) -> (String, String, Option<i32>) {
    let child = Command::new(bin)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn binary");
    run_with_timeout(child)
}

fn write_source(dir: &Path, tag: &str, source: &str) -> PathBuf {
    let path = dir.join(format!("{tag}.gos"));
    let mut f = fs::File::create(&path).expect("create source");
    let mut map = gossamer_lex::SourceMap::new();
    let file = map.add_file(path.display().to_string(), source.to_string());
    let source = gossamer_parse::autoderive::migrate_braced_struct_constructors(source, file)
        .expect("bug-regression source must parse for constructor migration");
    f.write_all(source.as_bytes()).unwrap();
    path
}

#[test]
fn nested_consts_run_in_vm_and_native() {
    let src = r#"
const OUTER: i64 = 5

fn main() {
    println("top={}", OUTER)
    const OUTER: i64 = 40
    const STEP: i64 = OUTER + 2
    {
        const OUTER: String = "answer"
        println("{}={}", OUTER, STEP)
    }
    println("again={}", OUTER)
}
"#;
    let dir = fresh_dir("nested_consts");
    let path = write_source(&dir, "nested_consts", src);
    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();
    let release_scratch = dir.join("bin-release");
    std::fs::create_dir_all(&release_scratch).unwrap();

    let vm = run_vm(&path);
    let bin = build_native(&path, &scratch).expect("native build");
    let native = run_native(&bin);
    let release_bin = build_native_release(&path, &release_scratch).expect("release build");
    let release_native = run_native(&release_bin);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert_eq!(
        release_native.2,
        Some(0),
        "release native stderr: {}",
        release_native.1
    );
    assert_eq!(vm.0, "top=5\nanswer=42\nagain=40\n");
    assert_eq!(native.0, vm.0);
    assert_eq!(release_native.0, vm.0);
}

#[test]
fn generic_struct_f64_field_prints_as_float() {
    // Bug fixed in 0.10.0: a generic struct field whose concrete type
    // is `f64` (`Triple<A, B, C> { third: C }` with `C = f64`) printed
    // the value's IEEE-754 bit pattern as an integer under
    // `gos build --release`. Two root causes: (1) unsuffixed float
    // literals (`3.0`) were never defaulted to `f64` so the field's
    // inference var leaked, and (2) `deep_resolve` didn't recurse into
    // `Adt` substs so the receiver's type args stayed unresolved. Both
    // are fixed; the field now prints as the float `3`.
    let src = r#"
struct Triple<A, B, C> { first: A, second: B, third: C }
fn main() {
    let r = Triple { first: 1, second: "two", third: 3.0 }
    println("{} {} {}", r.first, r.second, r.third)
}
"#;
    let dir = fresh_dir("generic_struct_f64");
    let path = write_source(&dir, "generic_struct_f64", src);
    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();
    let bin = build_native_release(&path, &scratch).expect("release build");
    let out = run_native(&bin);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.2, Some(0), "stderr: {}", out.1);
    assert!(out.0.contains("1 two 3"), "got: {:?}", out.0);
}

#[test]
fn impl_method_self_field_iteration_binds_element_type() {
    // Bug fixed in 0.10.0: inside an `impl` method the `self` receiver
    // was bound to a fresh inference var instead of the impl's `Self`
    // type, so `for x in self.items` over a `Vec<String>` field bound `x`
    // at the i64 default - printing element pointers as integers (the
    // auto-derived `to_json` serialised a `Vec<String>` field as numbers).
    // `self` now binds to the concrete `Self` type.
    let src = r#"
struct U { tags: Vec<String> }
impl U {
    fn dump(self) {
        for item in self.tags { println("item={}", item) }
    }
}
fn main() {
    let mut t: Vec<String> = Vec::from([])
    t.push("a")
    t.push("b")
    let u = U { tags: t }
    U::dump(u)
}
"#;
    let dir = fresh_dir("impl_self_field_iter");
    let path = write_source(&dir, "impl_self_field_iter", src);
    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();
    let bin = build_native_release(&path, &scratch).expect("release build");
    let out = run_native(&bin);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.2, Some(0), "stderr: {}", out.1);
    assert!(
        out.0.contains("item=a") && out.0.contains("item=b"),
        "got: {:?}",
        out.0
    );
}

#[test]
fn native_bytecode_match_covers_pattern_shapes() {
    // The bytecode VM now lowers `match` directly (literals, ranges,
    // or-patterns, guards, enum-variant payloads, tuple/struct
    // destructure, `@`-bindings) instead of routing every arm
    // evaluation through the bundled tree-walker via
    // `Op::EvalDeferred`. This pins the VM output for a match that
    // exercises every natively-compiled pattern shape; a regression
    // in the test-and-branch lowering or the `VariantIs` /
    // `VariantField` / `StructIs` opcodes turns it red.
    let src = r#"
enum Shape { Circle(i64), Rect(i64, i64), Unit }
struct Point { x: i64, y: i64 }
fn classify(n: i64) -> String {
    match n {
        0 => "zero",
        1 | 2 | 3 => "small",
        4..=9 => "mid",
        x if x < 0 => "neg",
        big @ 100..=999 => format("big:{}", big),
        _ => "huge",
    }
}
fn area(s: Shape) -> i64 {
    match s {
        Shape::Circle(r) => 3 * r * r,
        Shape::Rect(w, h) => w * h,
        Shape::Unit => 0,
    }
}
fn main() {
    for n in [0, 2, 7, -4, 500, 100000] {
        println("{}={}", n, classify(n))
    }
    println("{}", area(Shape::Circle(2)))
    println("{}", area(Shape::Rect(3, 4)))
    println("{}", area(Shape::Unit))
    let p = Point { x: 5, y: 9 }
    match p { Point { x, y } => println("pt {} {}", x, y) }
    let pair = (11, 22)
    match pair { (a, b) => println("pair {} {}", a, b) }
}
"#;
    let dir = fresh_dir("native_match");
    let path = write_source(&dir, "native_match", src);
    let out = run_vm(&path);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.2, Some(0), "stderr: {}", out.1);
    let expect = "0=zero\n2=small\n7=mid\n-4=neg\n500=big:500\n100000=huge\n\
12\n12\n0\npt 5 9\npair 11 22\n";
    assert!(
        out.0.contains(expect),
        "native match output mismatch:\n{}",
        out.0
    );
}

#[test]
fn compiled_match_on_inferred_tuple_binds_element_types() {
    // Bug fixed in 0.10.0: a `match` on a tuple whose element types
    // were left unresolved by inference (`let pair = (10, "hi")`)
    // bound each element through a pointer-shaped local, so the
    // `println!` arg dispatcher routed the `i64` element through
    // `gos_rt_concat_str` and strlen'd the integer value → segfault.
    // The match tuple-binding now recovers each element type from
    // the sub-pattern when the tuple's recorded type is loose.
    let src = r#"
fn main() {
    let pair = (10, "hi")
    match pair { (n, s) => println("{} {}", n, s) }
}
"#;
    let dir = fresh_dir("compiled_match_tuple");
    let path = write_source(&dir, "compiled_match_tuple", src);
    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();
    let bin = build_native_release(&path, &scratch).expect("release build");
    let out = run_native(&bin);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.2, Some(0), "stderr: {}", out.1);
    assert!(out.0.contains("10 hi"), "got: {:?}", out.0);
}

#[test]
fn heterogeneous_tuple_binding_is_iterable() {
    let src = r#"
fn main() {
    let t = (1, 3.4, "a")
    for i in t { println(i) }
}
"#;
    let dir = fresh_dir("tuple_iter");
    let path = write_source(&dir, "tuple_iter", src);
    let run = run_vm(&path);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(run.2, Some(0), "stderr: {}", run.1);
    assert_eq!(run.0, "1\n3.4\na\n");
}

#[test]
fn eprintln_runs_without_aborting_via_jit() {
    // Cranelift JIT must have `gos_rt_eprint_str` /
    // `gos_rt_eprintln` in its symbol table; without them
    // `eprintln!` aborts at startup with "can't resolve
    // symbol gos_rt_eprint_str".
    let dir = fresh_dir("eprintln_jit");
    let src = write_source(
        &dir,
        "eprintln_jit",
        "fn main() { eprintln(\"diag-line\") }\n",
    );
    let run = run_vm(&src);
    assert_eq!(run.2, Some(0), "vm: expected clean exit, got {:?}", run.2);
    assert!(
        run.1.contains("diag-line"),
        "vm: expected eprintln output on stderr, got: {:?}",
        run.1,
    );
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&src, &cl_dir).expect("cranelift build");
    let native = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(
        native.2,
        Some(0),
        "native: expected clean exit, got {:?}",
        native.2
    );
    assert!(
        native.1.contains("diag-line"),
        "native: expected eprintln output on stderr, got: {:?}",
        native.1,
    );
}

#[test]
fn os_exit_flushes_stdout_in_native_binary() {
    // `process::exit(N)` after `println!` must flush the runtime's
    // stdout buffer before terminating; otherwise the buffered
    // output is dropped (`gos_rt_exit` previously skipped the
    // drain and called `std::process::exit` directly).
    let dir = fresh_dir("exit_flush");
    let src = write_source(
        &dir,
        "exit_flush",
        "use std::process\nfn main() {\n    println(\"before exit\")\n    process::exit(2)\n}\n",
    );
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&src, &cl_dir).expect("cranelift build");
    let run = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(run.2, Some(2), "expected exit code 2, got {:?}", run.2);
    assert!(
        run.0.contains("before exit"),
        "expected `before exit` on stdout, got: {:?}",
        run.0,
    );
}

#[test]
fn usize_index_works_for_string_arrays() {
    // `let mut i: usize = 0; arr[i]` must succeed even though
    // usize's runtime shape is `Value::U64`. The interp's
    // `index_get` previously only matched `Value::Int`, panicking
    // with "index must be integer".
    let src = r#"
fn try_it() {
    let mut args: Vec<String> = [].to_vec()
    args.push("zero")
    args.push("one")
    let mut i: usize = 0
    while i < args.len() {
        println("[{}] = {}", i, args[i].clone())
        i = i + 1
    }
}
fn main() { try_it() }
"#;
    let dir = fresh_dir("usize_index_string");
    let path = write_source(&dir, "usize_index_string", src);
    let run = run_vm(&path);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    assert!(
        run.0.contains("[0] = zero") && run.0.contains("[1] = one"),
        "vm: expected indexed output, got: {:?}",
        run.0,
    );
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&path, &cl_dir).expect("cranelift build");
    let native = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert!(
        native.0.contains("[0] = zero") && native.0.contains("[1] = one"),
        "native: expected indexed output, got: {:?}",
        native.0,
    );
}

#[test]
fn indexing_tuple_slices_with_usize_works() {
    // `&[(String, String)]` indexed with `i: usize` must use the
    // index helper that accepts both `Value::Int` and
    // `Value::U64` - the tuple-of-String case fell through the
    // same `Value::Int`-only match as the string-array case
    // above.
    let src = r#"
fn first_key(vars: [(String, String)]) -> String {
    if vars.len() == 0 { return "" }
    let mut i: usize = 0
    while i < vars.len() {
        let _ = vars[i].0.clone()
        i = i + 1
    }
    vars[0].0.clone()
}

fn main() {
    let pairs = [
        ("alpha", "1"),
        ("beta", "2"),
    ].to_vec()
    println("{}", first_key(pairs))
}
"#;
    let dir = fresh_dir("usize_index_tuple");
    let path = write_source(&dir, "usize_index_tuple", src);
    let run = run_vm(&path);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(run.2, Some(0), "stderr: {}", run.1);
    assert!(
        run.0.contains("alpha"),
        "expected first_key=alpha, got: {:?}",
        run.0,
    );
}

#[test]
fn iter_next_is_bound_and_returns_some_for_first() {
    // `xs.iter().next()` must dispatch through a `next` builtin
    // that returns `Some(first)` for non-empty collections and
    // `None` for empty ones (instead of erroring with "name
    // `next` is not bound in this scope" or silently returning
    // a non-matching value).
    let src = r#"
fn main() {
    let xs = ["a", "b"].to_vec()
    let mut it = xs.iter()
    match it.next() {
        Some(s) => println("first={}", s),
        None => println("none"),
    }
    let empty: Vec<i64> = [].to_vec()
    let mut it2 = empty.iter()
    match it2.next() {
        Some(_) => println("unexpected some"),
        None => println("empty-none"),
    }
}
"#;
    let dir = fresh_dir("iter_next");
    let path = write_source(&dir, "iter_next", src);
    let run = run_vm(&path);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    assert!(
        run.0.contains("first=a") && run.0.contains("empty-none"),
        "vm got: {:?}",
        run.0,
    );
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&path, &cl_dir).expect("cranelift build");
    let native = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert!(
        native.0.contains("first=a") && native.0.contains("empty-none"),
        "native got: {:?}",
        native.0,
    );
}

#[test]
fn type_from_json_round_trips_struct_with_nested_address_and_tags() {
    // Every named struct in the program auto-derives the generic
    // `from_json::<Type>(text)` / `to_json::<Type>(value)` free
    // functions. The decoder must (a) accept a clean payload
    // and produce a typed struct readable via dot-access, (b) reject
    // type mismatches with a path-qualified error, and (c) reject
    // missing required fields. Tests all three.
    let src = r#"
use std::errors

struct Address {
    city: String,
    zip: String,
}

struct User {
    name: String,
    age: i64,
    active: bool,
    tags: Vec<String>,
    address: Address,
}

fn main() -> Result<(), errors::Error> {
    let mut tags: Vec<String> = Vec::from([])
    tags.push("admin")
    let original = User { name: "alice", age: 30, active: true, tags: tags, address: Address { city: "denver", zip: "80205" } }
    let text = to_json::<User>(original)?
    let back: User = from_json::<User>(text)?
    println("name={}", back.name)
    println("age={}", back.age)
    println("city={}", back.address.city)
    println("tag0={}", back.tags[0])

    let bad = "{\"name\":\"bob\",\"age\":\"oops\",\"active\":false,\"tags\":[],\"address\":{\"city\":\"x\",\"zip\":\"0\"}}"
    match from_json::<User>(bad) {
        Ok(_)  => println("bad-passed"),
        Err(e) => println("bad-rejected: {}", e),
    }

    let missing = "{\"name\":\"carol\",\"age\":40,\"active\":true,\"tags\":[]}"
    match from_json::<User>(missing) {
        Ok(_)  => println("missing-passed"),
        Err(e) => println("missing-rejected: {}", e),
    }
    Ok(())
}
"#;
    let dir = fresh_dir("type_from_json_round_trip");
    let path = write_source(&dir, "from_json", src);
    let run = run_vm(&path);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    assert!(run.0.contains("name=alice"), "missing name: {:?}", run.0);
    assert!(run.0.contains("age=30"), "missing age: {:?}", run.0);
    assert!(run.0.contains("city=denver"), "missing city: {:?}", run.0);
    assert!(run.0.contains("tag0=admin"), "missing tag: {:?}", run.0);
    assert!(
        run.0.contains("bad-rejected: ") && run.0.contains("field `age`"),
        "expected age-mismatch rejection: {:?}",
        run.0,
    );
    assert!(
        run.0.contains("missing-rejected: ") && run.0.contains("missing field `address`"),
        "expected missing-address rejection: {:?}",
        run.0,
    );
}

#[test]
fn json_set_appends_and_replaces_fields() {
    // `json::set(obj, key, value)` must append a new field when
    // the key is missing and replace it when present, returning
    // the updated `json::Value::object()`. The free function
    // was previously unbound, surfacing as "name `json::set`
    // is not bound in this scope".
    let src = r#"
use std::encoding::json
fn main() {
    let obj = json::Value::object()
    let with_a = json::set(obj, "a", json::Value::Int(1))
    let with_b = json::set(with_a, "b", json::Value::String("hello"))
    let replaced = json::set(with_b, "a", json::Value::Int(99))
    println("{}", json::render(replaced))
}
"#;
    let dir = fresh_dir("json_set");
    let path = write_source(&dir, "json_set", src);
    let run = run_vm(&path);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    assert!(
        run.0.contains("\"a\":99"),
        "vm: expected replaced a=99 in output, got: {:?}",
        run.0,
    );
    assert!(
        run.0.contains("\"b\":\"hello\""),
        "vm: expected b=hello in output, got: {:?}",
        run.0,
    );
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&path, &cl_dir).expect("cranelift build");
    let native = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert!(
        native.0.contains("\"a\":99"),
        "native: expected replaced a=99 in output, got: {:?}",
        native.0,
    );
    assert!(
        native.0.contains("\"b\":\"hello\""),
        "native: expected b=hello in output, got: {:?}",
        native.0,
    );
}

#[test]
fn errors_wrap_chain_walks_through_cause() {
    // `errors::wrap(cause, msg)` must keep that argument order
    // (the SKILL-card-documented shape, used by every in-tree
    // example): the `msg` becomes the wrapped error's message
    // and the `cause` is reachable via `.cause()`. Build a
    // chain (`new` → `wrap`) and walk it.
    let src = r#"
use std::errors
fn main() {
    let inner = errors::new("inner failure")
    let wrapped = errors::wrap(inner, "outer context")
    println("top: {}", wrapped.message())
    match wrapped.cause() {
        Some(c) => println("cause: {}", c.message()),
        None => println("no cause"),
    }
}
"#;
    let dir = fresh_dir("errors_wrap_chain");
    let path = write_source(&dir, "errors_wrap_chain", src);
    let run = run_vm(&path);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    assert!(
        run.0.contains("top: outer context"),
        "vm: expected wrapped message, got: {:?}",
        run.0,
    );
    assert!(
        run.0.contains("cause: inner failure"),
        "vm: expected inner cause, got: {:?}",
        run.0,
    );
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&path, &cl_dir).expect("cranelift build");
    let native = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert!(
        native.0.contains("top: outer context"),
        "native: expected wrapped message, got: {:?}",
        native.0,
    );
    assert!(
        native.0.contains("cause: inner failure"),
        "native: expected inner cause, got: {:?}",
        native.0,
    );
}

#[test]
fn regex_captures_indexing_works() {
    // `caps[0]` / `caps[i]` must succeed via the same
    // `index_to_usize` helper that handles `Value::U64` and
    // `Value::Int` uniformly - the regex captures shape
    // previously panicked with "index must be integer".
    let src = r#"
use std::regex
fn main() {
    let re = regex::compile("(\\w+)=(\\d+)").unwrap()
    let caps = regex::captures_all(re, "a=1 b=22")
    if caps.len() == 0 {
        println("no match")
    } else {
        let row = caps[0].clone()
        if row.len() < 2 {
            println("too few groups")
        } else {
            match row[1].clone() {
                Some(s) => println("first={}", s),
                None => println("missing-group"),
            }
        }
    }
}
"#;
    let dir = fresh_dir("regex_captures_index");
    let path = write_source(&dir, "regex_captures_index", src);
    let run = run_vm(&path);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    assert!(
        run.0.contains("first=a"),
        "vm: expected first=a from regex captures, got: {:?}",
        run.0,
    );
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&path, &cl_dir).expect("cranelift build");
    let native = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert!(
        native.0.contains("first=a"),
        "native: expected first=a from regex captures, got: {:?}",
        native.0,
    );
}

#[test]
fn continue_in_for_range_advances_counter() {
    // `for i in 0..n { if cond { continue } body }` previously
    // livelocked the bytecode VM: the for-range fast path set
    // `continue` to jump straight to the bounds-check header,
    // bypassing the fused increment-and-test op at the bottom of
    // the body. Hard timeout below would fire if the
    // `continue_patches` rewiring regressed.
    let src = r#"
fn main() {
    let mut acc: i64 = 0
    for i in 0..10 {
        if i % 4 == 0 {
            continue
        }
        acc = acc + i
    }
    println("acc={}", acc)
}

"#;
    let dir = fresh_dir("continue_for_range");
    let path = write_source(&dir, "continue_for_range", src);
    let run = run_vm(&path);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    assert!(
        run.0.contains("acc=33"),
        "vm: expected acc=33, got: {:?}",
        run.0
    );
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&path, &cl_dir).expect("cranelift build");
    let native = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert!(
        native.0.contains("acc=33"),
        "native: expected acc=33, got: {:?}",
        native.0
    );
}

#[test]
fn open_range_breaks_and_bounds_checks_match_across_tiers() {
    let break_src = r#"
fn main() {
    let mut total = 0
    for i in 0.. {
        total += i
        if i == 2 { break }
        if i > 20 { panic("break failed") }
    }
    let mut total2 = 0
    for i in .. {
        total2 += i
        if i == 2 { break }
        if i > 20 { panic("break failed") }
    }
    let mut finite = 0
    for i in ..3 { finite += i }
    println("{} {} {}", total, total2, finite)
}
"#;
    let dir = fresh_dir("open_range_break");
    let path = write_source(&dir, "open_range_break", break_src);
    let vm = run_vm(&path);
    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(vm.0, "3 3 3\n");
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&path, &cl_dir).expect("cranelift build");
    let native = run_native(&bin);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert_eq!(native.0, vm.0);

    let bounds_src =
        "fn main() {\n    let a = [1, 2, 3]\n    for i in .. { println(a[i]) }\n}\n";
    let bounds_path = write_source(&dir, "open_range_bounds", bounds_src);
    let vm_bounds = run_vm(&bounds_path);
    assert_ne!(vm_bounds.2, Some(0), "VM accepted index 3");
    assert!(vm_bounds.1.contains("out of bounds"), "{}", vm_bounds.1);
    let bounds_dir = dir.join("bounds-cl");
    fs::create_dir_all(&bounds_dir).unwrap();
    let bounds_bin = build_native(&bounds_path, &bounds_dir).expect("cranelift build");
    let native_bounds = run_native(&bounds_bin);
    let _ = fs::remove_dir_all(&dir);
    assert_ne!(native_bounds.2, Some(0), "native accepted index 3");
    assert!(
        native_bounds.1.contains("out of bounds"),
        "{}",
        native_bounds.1
    );
}

#[test]
fn variant_constructors_pin_call_result_to_adt() {
    // `let first = Some(10)` previously typed `first` as
    // `Int(I64)` because the typechecker had no signature for
    // the `Some` constructor - the call expression fell back to
    // a fresh `Var`, and the `let` binding's type promotion only
    // kicked in for primitives, dropping the Adt wrapper. Match
    // dispatch later treated the 8-byte `*mut GosResult` pointer
    // as a raw i64 and read garbage from nested `if let` chains.
    // Fix: extended `check_call`'s fallback table to recognise
    // the four standard variant constructors (`Some` / `None` /
    // `Ok` / `Err`) and synthesise the matching Adt with the
    // payload's type.
    let src = r#"
use std::errors

fn maybe_double(n: i64) -> Option<i64> {
    if n > 0 { Some(n * 2) } else { None }
}

fn safe_divide(a: i64, b: i64) -> Result<i64, errors::Error> {
    if b == 0 { return Err(errors::new("zero")) }
    Ok(a / b)
}

fn main() {
    let first = Some(10)
    if let Some(n) = first {
        if let Some(m) = maybe_double(n) {
            if let Ok(d) = safe_divide(m, 4) {
                println("d = {}", d)
            }
        }
    }
}
"#;
    let dir = fresh_dir("variant_ctor_typed");
    let path = write_source(&dir, "variant_ctor_typed", src);
    let run = run_vm(&path);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    assert!(
        run.0.contains("d = 5"),
        "vm: expected `d = 5`, got: {:?}",
        run.0
    );
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&path, &cl_dir).expect("cranelift build");
    let native = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert!(
        native.0.contains("d = 5"),
        "native: expected `d = 5`, got: {:?}",
        native.0
    );
}

#[test]
fn iterator_sum_preserves_usize_result_type() {
    let src = r#"
fn main() {
    let values: Vec<i64> = Vec::from([9, 18, 27])
    let total: usize = values.iter().map(|value| value as usize).sum()
    println("total={}", total)
}
"#;
    let dir = fresh_dir("iterator_sum_usize");
    let path = write_source(&dir, "iterator_sum_usize", src);
    let vm = run_vm(&path);
    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(vm.0, "total=54\n");
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&path, &cl_dir).expect("native build");
    let native = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert_eq!(native.0, "total=54\n");
}

#[test]
fn str_find_returns_option_in_compiled_mode() {
    // `s.find("missing")` previously returned Some(_) instead
    // of None in cranelift - the dispatch routed straight to
    // `gos_rt_str_find` (returns raw i64 with -1 sentinel)
    // and the SwitchInt on the Option discriminant defaulted
    // to the Some arm because -1 didn't match either disc=0
    // (Some) or disc=1 (None). Fix: introduced
    // `gos_rt_str_find_opt` which wraps the i64 in a
    // `*mut GosResult` (Some(idx) / None) and routed the
    // String `find` dispatch through it.
    let src = r#"
fn main() {
    let s = "foo bar"
    match s.find("bar") {
        Some(i) => println("found at {}", i),
        None => println("missing"),
    }
    match s.find("qux") {
        Some(i) => println("unexpected at {}", i),
        None => println("not found"),
    }
}
"#;
    let dir = fresh_dir("str_find_opt");
    let path = write_source(&dir, "str_find_opt", src);
    let run = run_vm(&path);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    assert!(run.0.contains("found at 4"), "vm got: {:?}", run.0);
    assert!(run.0.contains("not found"), "vm got: {:?}", run.0);
    assert!(!run.0.contains("unexpected"), "vm got: {:?}", run.0);
    let cl_dir = dir.join("cl");
    fs::create_dir_all(&cl_dir).unwrap();
    let bin = build_native(&path, &cl_dir).expect("cranelift build");
    let native = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert!(
        native.0.contains("found at 4"),
        "native got: {:?}",
        native.0
    );
    assert!(native.0.contains("not found"), "native got: {:?}", native.0);
    assert!(
        !native.0.contains("unexpected"),
        "native got: {:?}",
        native.0
    );
}

#[test]
fn assoc_missing_impl_item_is_rejected() {
    // `impl Holder for Label` without the trait's `type Item`: every
    // projection through the trait needs a concrete item to land on, so
    // `gos check` names the omission instead of accepting it and faulting
    // later.
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../feature-testing-examples/assoc_missing_impl_item.gos");
    let out = Command::new(gos_bin())
        .arg("check")
        .arg(&fixture)
        .output()
        .expect("spawn gos check");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "check unexpectedly passed: {stderr}");
    assert!(
        stderr.contains("GT0059"),
        "expected GT0059, got: {stderr}"
    );
    assert!(
        stderr.contains("is missing associated item: `type Item`"),
        "diagnostic must name the omitted item: {stderr}"
    );
    assert!(
        stderr.contains("add `type Item = ...`"),
        "help must name the fix: {stderr}"
    );
}

#[test]
fn break_unknown_label_is_rejected() {
    // `break 'wrong` inside a loop labelled `'search`: the label names no
    // enclosing loop, so `gos check` reports it and names the labels that
    // are in scope, rather than leaving the program to fault at run time.
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../feature-testing-examples/break_unknown_label.gos");
    let out = Command::new(gos_bin())
        .arg("check")
        .arg(&fixture)
        .output()
        .expect("spawn gos check");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "check unexpectedly passed: {stderr}");
    assert!(
        stderr.contains("GR0017"),
        "expected GR0017, got: {stderr}"
    );
    assert!(
        stderr.contains("no enclosing loop is labelled `'wrong`"),
        "diagnostic must name the label: {stderr}"
    );
    assert!(
        stderr.contains("`'search`"),
        "diagnostic must list the labels in scope: {stderr}"
    );
}

#[test]
fn break_unknown_label_is_rejected_by_build_too() {
    // The compiled path took the same program and produced a binary that
    // trapped; refusing at check time covers `gos build` as well.
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../feature-testing-examples/break_unknown_label.gos");
    let out = Command::new(gos_bin())
        .arg("build")
        .arg(&fixture)
        .output()
        .expect("spawn gos build");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stderr.contains("GR0017"),
        "build must refuse the program: stdout={stdout} stderr={stderr}"
    );
    assert!(
        !stdout.contains("native executable"),
        "build must not emit a binary: {stdout}"
    );
}

#[test]
fn break_and_continue_outside_a_loop_are_rejected() {
    for (source, keyword) in [
        ("fn main() {\n    break\n}\n", "break"),
        ("fn main() {\n    continue\n}\n", "continue"),
        // A closure is a separate function, so a loop around it is not a
        // target for a `break` written inside it.
        (
            "fn main() {\n    for i in 0..3 {\n        let f = || { break }\n        f()\n    }\n}\n",
            "break",
        ),
    ] {
        let path = std::env::temp_dir().join(format!(
            "gos-loop-control-{}-{}.gos",
            keyword,
            std::process::id()
        ));
        std::fs::write(&path, source).expect("write fixture");
        let out = Command::new(gos_bin())
            .arg("check")
            .arg(&path)
            .output()
            .expect("spawn gos check");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success(),
            "check unexpectedly passed for {source:?}: {stderr}"
        );
        assert!(
            stderr.contains("GR0017") && stderr.contains(&format!("`{keyword}` outside of a loop")),
            "expected an outside-of-a-loop GR0017 for {source:?}, got: {stderr}"
        );
        let _ = std::fs::remove_file(&path);
    }
}

#[test]
fn a_label_on_an_enclosing_loop_still_resolves() {
    let source = concat!(
        "fn main() {\n",
        "    let mut found = (0, 0)\n",
        "    'search: for row in 0..5 {\n",
        "        'inner: for col in 0..5 {\n",
        "            if row * col == 6 {\n",
        "                found = (row, col)\n",
        "                break 'search\n",
        "            }\n",
        "            continue 'inner\n",
        "        }\n",
        "    }\n",
        "    println(\"{:?}\", found)\n",
        "}\n"
    );
    let path = std::env::temp_dir().join(format!("gos-loop-ok-{}.gos", std::process::id()));
    std::fs::write(&path, source).expect("write fixture");
    let out = Command::new(gos_bin())
        .arg("run")
        .arg(&path)
        .output()
        .expect("spawn gos run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "run failed: {stderr}");
    assert!(stdout.contains("(2, 3)"), "stdout: {stdout}");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn builtin_receiver_keeps_its_method_over_a_same_named_user_method() {
    // A user `impl` declaring a method name a built-in container also
    // uses (`push`) must not capture calls whose receiver is that
    // container. The receiver's type owns the method surface, so
    // `xs.push(v)` on a `Vec` field, a `&mut Vec` parameter, and a
    // field read from outside the impl all reach `Vec::push`.
    let src = r#"
struct Other {}
impl Other { fn push(&mut self, v: i64) -> i64 { v * 100 } }

struct Holder { items: Vec<i64> }

impl Holder {
    fn add(&mut self, v: i64) { self.items.push(v) }
}

fn free_push(xs: &mut Vec<i64>, v: i64) { xs.push(v) }

fn main() {
    let mut h = Holder { items: #[] }
    h.add(1)
    println("field={:?}", h.items)

    let mut direct = Holder { items: #[] }
    direct.items.push(2)
    println("outside={:?}", direct.items)

    let mut xs: Vec<i64> = #[]
    free_push(&mut xs, 3)
    println("param={:?}", xs)

    let mut o = Other {}
    println("user={}", o.push(4))
}
"#;
    let dir = fresh_dir("builtin_receiver_dispatch");
    let path = write_source(&dir, "builtin_receiver_dispatch", src);
    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();

    let vm = run_vm(&path);
    let bin = build_native(&path, &scratch).expect("native build");
    let native = run_native(&bin);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert_eq!(
        vm.0, "field=#[1]\noutside=#[2]\nparam=#[3]\nuser=400\n",
        "vm stdout"
    );
    assert_eq!(native.0, vm.0, "tier parity");
}

#[test]
fn vec_u8_mutators_agree_across_tiers() {
    // A `Vec<u8>` packs its elements one byte wide rather than into the
    // boxed slots the generic sequence path assumes, so every mutator has
    // to read and write at that width: the VM answered its receiver
    // unchanged for `extend` / `truncate` / `sort` / `reverse` / `clear`,
    // and the compiled tiers sorted eight elements at a time.
    let src = r#"
fn main() {
    let mut a: Vec<u8> = #[1, 2, 3]
    a.extend(#[4, 5])
    println("extend {:?}", a)

    let mut b: Vec<u8> = #[1, 2, 3]
    b.extend_from_slice(#[9])
    println("extend_from_slice {:?}", b)

    let mut c: Vec<u8> = #[1, 2, 3, 4]
    c.truncate(2)
    println("truncate {:?}", c)

    let mut d: Vec<u8> = #[3, 1, 2]
    d.sort()
    println("sort {:?}", d)

    let mut e: Vec<u8> = #[1, 2, 3]
    e.reverse()
    println("reverse {:?}", e)

    let mut f: Vec<u8> = #[1, 2, 3]
    f.clear()
    println("clear {:?}", f)

    let mut g: Vec<u8> = #[]
    g.extend("hi".as_bytes())
    println("extend_str {:?}", g)
}
"#;
    let dir = fresh_dir("vec_u8_mutators");
    let path = write_source(&dir, "vec_u8_mutators", src);
    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();

    let vm = run_vm(&path);
    let bin = build_native(&path, &scratch).expect("native build");
    let native = run_native(&bin);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert_eq!(
        vm.0,
        "extend #[1, 2, 3, 4, 5]\n\
         extend_from_slice #[1, 2, 3, 9]\n\
         truncate #[1, 2]\n\
         sort #[1, 2, 3]\n\
         reverse #[3, 2, 1]\n\
         clear #[]\n\
         extend_str #[104, 105]\n",
        "vm stdout"
    );
    assert_eq!(native.0, vm.0, "tier parity");
}

#[test]
fn a_socket_constructor_propagates_with_the_question_mark() {
    // `TcpStream::connect` answers a `Result`, so `?` carries its error
    // like any other fallible call; the checker had no signature for the
    // socket handles and reported the operand was not a `Result`. A
    // `Vec<u8>` also reaches a socket write as packed bytes rather than a
    // boxed array, so the payload has to be read at that width.
    let src = r#"
use std::errors
use std::net::{TcpListener, TcpStream}

fn serve(listener: TcpListener) -> Result<String, errors::Error> {
    let client, _addr = listener.accept()?
    let payload = client.read(64)?
    client.close()
    String::from_utf8(payload)
}

fn main() -> Result<(), errors::Error> {
    let listener = TcpListener::bind("127.0.0.1:0")?
    let addr = listener.local_addr()?
    let sender = spawn(|| serve(listener))
    let stream = TcpStream::connect(addr)?
    let mut payload: Vec<u8> = #[]
    payload.extend("ping".as_bytes())
    stream.write_all(payload)?
    stream.close()
    println("{}", sender.join()??)
    Ok(())
}
"#;
    let dir = fresh_dir("socket_question_mark");
    let path = write_source(&dir, "socket_question_mark", src);
    let vm = run_vm(&path);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(vm.0, "ping\n");
}

#[test]
fn a_const_named_in_a_pattern_matches_its_value() {
    // A pattern must be a compile-time constant, so a `const` named in one
    // stands for its value. It parsed as a nominal pattern instead, which
    // no scalar matches, so every arm fell through to `_`. A unit variant
    // or unit struct of the same shape keeps its own nominal meaning.
    let src = r#"
const LOW: i64 = 10
const HIGH: i64 = 20
const NAME: String = "pg"
const RATIO: f64 = 1.5

enum Color { Red, Green }
struct Marker

fn band(v: i64) -> String {
    match v {
        LOW => "low",
        HIGH => "high",
        _ => "other",
    }
}

fn label(s: String) -> String {
    match s {
        NAME => "known",
        _ => "unknown",
    }
}

fn scale(f: f64) -> String {
    match f {
        RATIO => "exact",
        _ => "off",
    }
}

fn hue(c: Color) -> String {
    match c {
        Color::Red => "red",
        Color::Green => "green",
    }
}

fn marked(m: Marker) -> String {
    match m {
        Marker => "marker",
    }
}

fn main() {
    println("{} {} {}", band(10), band(20), band(30))
    println("{} {}", label("pg"), label("other"))
    println("{} {}", scale(1.5), scale(2.5))
    println("{} {}", hue(Color::Red), hue(Color::Green))
    println("{}", marked(Marker))
}
"#;
    let dir = fresh_dir("const_pattern");
    let path = write_source(&dir, "const_pattern", src);
    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();

    let vm = run_vm(&path);
    let bin = build_native(&path, &scratch).expect("native build");
    let native = run_native(&bin);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert_eq!(
        vm.0,
        "low high other\nknown unknown\nexact off\nred green\nmarker\n",
        "vm stdout"
    );
    assert_eq!(native.0, vm.0, "tier parity");
}

#[test]
fn string_reference_iterates_unicode_scalars() {
    // Iterating a `&String` handed the body each scalar as an integer:
    // the loop chose its cursor from the reference's own type rather
    // than the text it names, so `c.to_string()` rendered the code
    // point. The by-value spelling was already correct.
    let src = r#"
fn escape(text: String) -> String {
    let mut out = ""
    for c in text {
        out += match c { 'b' => "B", _ => c.to_string() }
    }
    out
}

fn count_upper(text: String) -> i64 {
    let mut n = 0
    for c in text { if c >= 'A' && c <= 'Z' { n += 1 } }
    n
}

fn main() {
    println("{}", escape("abc"))
    let owned = "aXbY"
    let borrowed = &owned
    let mut joined = ""
    for c in borrowed { joined += c.to_string() }
    println("{}", joined)
    println("{}", count_upper("aXbY"))
}
"#;
    let dir = fresh_dir("string_ref_iter");
    let path = write_source(&dir, "string_ref_iter", src);
    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();

    let vm = run_vm(&path);
    let bin = build_native(&path, &scratch).expect("native build");
    let native = run_native(&bin);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert_eq!(vm.0, "aBc\naXbY\n2\n", "vm stdout");
    assert_eq!(native.0, vm.0, "tier parity");
}

#[test]
fn for_over_sequence_literal_survives_a_body_that_moves_the_element() {
    // The `for` desugar binds an iterable with no home of its own to
    // its own `__for_iter` state local and takes `&mut` of it. The VM's
    // indexed loop read that `&mut` as a request to write each element
    // back into the source, so a body that moved the element into a
    // container stored the emptied register over the snapshot - a type
    // error against the literal's integer storage. A `&mut` the user
    // wrote still writes through.
    let src = r#"
enum V { Null, Int(i64), Text(String) }

fn main() {
    for n in #[1, 2, 3] {
        let params = #[V::Int(n)]
        let _ = params.len()
    }
    println("literal enum ok")
    for s in #["a", "b"] {
        let held = #[s]
        let _ = held.len()
    }
    println("literal string ok")
    let mut v = Vec::from([1, 2])
    for i in &mut v {
        *i += 1
    }
    println("write-back {} {}", v[0], v[1])
    let mut inline = #[#[1]]
    for row in &mut inline { row.push(5) }
    println("inline {}", inline[0].len())
}
"#;
    let dir = fresh_dir("for_literal_move");
    let path = write_source(&dir, "for_literal_move", src);
    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();

    let vm = run_vm(&path);
    let bin = build_native(&path, &scratch).expect("native build");
    let native = run_native(&bin);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert_eq!(
        vm.0,
        "literal enum ok\nliteral string ok\nwrite-back 2 3\ninline 2\n",
        "vm stdout"
    );
    assert_eq!(native.0, vm.0, "tier parity");
}

#[test]
fn vec_remove_hands_back_a_whole_struct_element() {
    // `remove` read one word of the element and then shifted the tail
    // over the slot it read from, so a struct or tuple element came
    // back as its first field's word - a wrong value, and a fault once
    // that word was read as the element's address.
    let src = r#"
struct N { channel: String, pid: i64 }

struct Conn { pending: Vec<N>, count: i64 }

impl Conn {
    fn take(&mut self) -> Option<N> {
        match Vec::remove(&mut self.pending, 0) {
            Ok(n) => Some(n)
            Err(_) => None
        }
    }
}

fn main() {
    let mut xs: Vec<N> = #[N { channel: "a", pid: 1 }, N { channel: "b", pid: 2 }]
    match Vec::remove(&mut xs, 0) {
        Ok(n) => println("free {} {}", n.channel, n.pid)
        Err(_) => println("none")
    }
    println("left {}", xs.len())

    let mut c = Conn {
        pending: #[N { channel: "c", pid: 3 }, N { channel: "d", pid: 4 }]
        count: 0
    }
    while let Some(n) = c.take() {
        println("field {} {}", n.channel, n.pid)
    }
    println("drained {}", c.pending.len())

    let mut ints: Vec<i64> = #[7, 8]
    match Vec::remove(&mut ints, 0) {
        Ok(v) => println("int {}", v)
        Err(_) => println("none")
    }
    match Vec::remove(&mut ints, 5) {
        Ok(v) => println("int {}", v)
        Err(_) => println("out of range")
    }
}
"#;
    let dir = fresh_dir("vec_remove_struct");
    let path = write_source(&dir, "vec_remove_struct", src);
    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();

    let vm = run_vm(&path);
    let bin = build_native(&path, &scratch).expect("native build");
    let native = run_native(&bin);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert_eq!(
        vm.0,
        "free a 1\nleft 1\nfield c 3\nfield d 4\ndrained 0\nint 7\nout of range\n",
        "vm stdout"
    );
    assert_eq!(native.0, vm.0, "tier parity");
}

#[test]
fn reference_to_a_vec_element_names_the_element() {
    // `&xs[i]` walked the element address off the `GosVec` header
    // rather than its data buffer, so the callee received the header's
    // own words. Binding the element first happened to load it
    // correctly, which is why the two spellings disagreed.
    let src = r#"
enum Value { Null, Int(i64), Text(String) }

fn row_len(row: Vec<Value>) -> i64 { row.len() }

fn first_text(row: Vec<Value>) -> String {
    match row[0] {
        Value::Text(s) => s
        _ => "?"
    }
}

fn walk(params: Vec<Vec<Value>>) -> i64 {
    let mut total = 0
    let mut i = 0
    while i < params.len() {
        total += row_len(params[i])
        i += 1
    }
    total
}

fn sum_words(words: Vec<String>) -> i64 {
    let mut total = 0
    let mut i = 0
    while i < words.len() {
        total += words[i].len()
        i += 1
    }
    total
}

fn main() {
    let params: Vec<Vec<Value>> = #[
        #[Value::Text("a"), Value::Int(1)]
        #[Value::Null]
    ]
    println("total {}", walk(params))
    println("first {}", first_text(params[0]))
    let words: Vec<String> = #["ab", "cde"]
    println("words {}", sum_words(words))
}
"#;
    let dir = fresh_dir("vec_elem_ref");
    let path = write_source(&dir, "vec_elem_ref", src);
    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();
    let release_scratch = dir.join("bin-release");
    std::fs::create_dir_all(&release_scratch).unwrap();

    let vm = run_vm(&path);
    let bin = build_native(&path, &scratch).expect("native build");
    let native = run_native(&bin);
    let release_bin = build_native_release(&path, &release_scratch).expect("release build");
    let release_native = run_native(&release_bin);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert_eq!(vm.0, "total 3\nfirst a\nwords 5\n", "vm stdout");
    assert_eq!(native.0, vm.0, "tier parity");
    assert_eq!(release_native.0, vm.0, "release tier parity");
}

#[test]
fn result_ok_and_err_answer_a_matchable_option() {
    // `.ok()` / `.err()` reached the payload-reading helper of the
    // `unwrap` family, so a compiled build handed the `match` a bare
    // word where an `Option` carrier was expected: `Err` read back as
    // `Some(0)`, and a String payload as a `Some` holding the error.
    let src = r#"
use std::errors

fn parse(text: String) -> Result<i64, errors::Error> {
    match text.to_i64() {
        Some(v) => Ok(v)
        None => Err(errors::new("bad"))
    }
}

fn read(text: String) -> Result<String, errors::Error> {
    if text.len() > 0 { Ok(text.clone()) } else { Err(errors::new("empty")) }
}

fn main() {
    println("{}", match parse("42").ok() { Some(v) => v, None => -1 })
    println("{}", match parse("x").ok() { Some(v) => v, None => -1 })
    println("{}", match read("hi").ok() { Some(v) => v, None => "none" })
    println("{}", match read("").ok() { Some(v) => v, None => "none" })
    println("{}", parse("7").ok().is_some())
    println("{}", parse("x").err().is_some())
    println("{}", parse("7").err().is_some())
}
"#;
    let dir = fresh_dir("result_ok_option");
    let path = write_source(&dir, "result_ok_option", src);
    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();
    let release_scratch = dir.join("bin-release");
    std::fs::create_dir_all(&release_scratch).unwrap();

    let vm = run_vm(&path);
    let bin = build_native(&path, &scratch).expect("native build");
    let native = run_native(&bin);
    let release_bin = build_native_release(&path, &release_scratch).expect("release build");
    let release_native = run_native(&release_bin);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert_eq!(vm.0, "42\n-1\nhi\nnone\ntrue\ntrue\nfalse\n", "vm stdout");
    assert_eq!(native.0, vm.0, "tier parity");
    assert_eq!(release_native.0, vm.0, "release tier parity");
}

#[test]
fn recursive_parser_returning_a_vec_of_enums_keeps_every_element() {
    // A recursive function whose result is `Result<Vec<Enum>, _>`
    // must answer the same list on every tier, nesting included.
    let src = r#"
use std::errors

enum Value {
    Null
    Bool(bool)
    Int(i64)
    Float(f64)
    Text(String)
    Bytes(Vec<u8>)
    Array(Vec<Value>)
}

fn matching_brace(text: String, open: i64) -> Result<i64, errors::Error> {
    let n = text.byte_len()
    let mut depth = 0
    let mut i = open
    while i < n {
        let c = text.byte_at(i)
        if c == 123 { depth += 1 }
        if c == 125 {
            depth -= 1
            if depth == 0 { return Ok(i) }
        }
        i += 1
    }
    Err(errors::new("unterminated"))
}

fn parse_body(text: String) -> Result<Vec<Value>, errors::Error> {
    let n = text.byte_len()
    let mut items: Vec<Value> = #[]
    let mut i = 1
    if i < n && text.byte_at(i) == 125 {
        return Ok(items)
    }
    while i < n {
        let c = text.byte_at(i)
        if c == 123 {
            let close = matching_brace(text, i)?
            let inner = parse_body(text.substring(i, close + 1))?
            items.push(Value::Array(inner))
            i = close + 1
        } else {
            let start = i
            while i < n && text.byte_at(i) != 44 && text.byte_at(i) != 125 { i += 1 }
            items.push(Value::Text(text.substring(start, i)))
        }
        if i >= n { return Err(errors::new("unterminated")) }
        let sep = text.byte_at(i)
        if sep == 125 { return Ok(items) }
        if sep != 44 { return Err(errors::new("unexpected byte")) }
        i += 1
    }
    Err(errors::new("unterminated"))
}

fn describe(vs: Vec<Value>) -> String {
    let mut out = ""
    for v in vs {
        out += match v {
            Value::Text(s) => format("{} ", s)
            Value::Array(inner) => format("[{}] ", describe(inner))
            _ => "? "
        }
    }
    out
}

fn report(text: String) {
    match parse_body(text) {
        Ok(vs) => println("{} :: {}", vs.len(), describe(vs))
        Err(e) => println("err {}", e)
    }
}

fn main() {
    report("{a,b}")
    report("{a,{b,c},d}")
    report("{{a,b},{c,d},{e,f}}")
    report("{}")
}
"#;
    let dir = fresh_dir("recursive_vec_enum");
    let path = write_source(&dir, "recursive_vec_enum", src);
    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();
    let release_scratch = dir.join("bin-release");
    std::fs::create_dir_all(&release_scratch).unwrap();

    let vm = run_vm(&path);
    let bin = build_native(&path, &scratch).expect("native build");
    let native = run_native(&bin);
    let release_bin = build_native_release(&path, &release_scratch).expect("release build");
    let release_native = run_native(&release_bin);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert_eq!(
        vm.0,
        "2 :: a b \n3 :: a [b c ] d \n3 :: [a b ] [c d ] [e f ] \n0 :: \n",
        "vm stdout"
    );
    assert_eq!(native.0, vm.0, "tier parity");
    assert_eq!(release_native.0, vm.0, "release tier parity");
}

#[test]
fn a_struct_reaching_an_opaque_handle_skips_serde_synthesis() {
    // The serde synthesizer decided per type, so a struct whose field
    // named a type it had refused still had a serializer emitted - one
    // that called the refused type's missing function. The refusal is
    // transitive now, and a type that still qualifies keeps its
    // serializer.
    let src = r#"
use std::net::UnixStream

enum Socket { Unix(UnixStream) }

struct Conn { sock: Socket, counter: i64 }

struct Client { pg: Conn, label: String }

struct Inner { a: i64, b: String }

struct Outer { name: String, inner: Inner, xs: Vec<Inner> }

fn main() {
    let o = Outer { name: "n", inner: Inner { a: 1, b: "x" }, xs: #[Inner { a: 2, b: "y" }] }
    let text = to_json::<Outer>(o).unwrap_or("ERR")
    println("{}", text)
    let back = from_json::<Outer>(text).unwrap_or(Outer {
        name: "?"
        inner: Inner { a: 0, b: "?" }
        xs: #[]
    })
    println("{} {} {}", back.name, back.inner.a, back.xs[0].b)
    match UnixStream::connect("/nonexistent-gossamer-regression.sock") {
        Ok(s) => {
            let c = Client { pg: Conn { sock: Socket::Unix(s), counter: 3 }, label: "pg" }
            println("{} {}", c.label, c.pg.counter)
        }
        Err(_) => println("no socket")
    }
}
"#;
    let dir = fresh_dir("serde_opaque_handle");
    let path = write_source(&dir, "serde_opaque_handle", src);
    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();

    let vm = run_vm(&path);
    let bin = build_native(&path, &scratch).expect("native build");
    let native = run_native(&bin);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert_eq!(
        vm.0,
        "{\"name\":\"n\",\"inner\":{\"a\":1,\"b\":\"x\"},\"xs\":[{\"a\":2,\"b\":\"y\"}]}\nn 1 y\nno socket\n",
        "vm stdout"
    );
    assert_eq!(native.0, vm.0, "tier parity");
}

/// A `for` over a sequence literal whose body binds a tuple from a call with a
/// diverging arm reads the literal's elements, rather than storing the call's
/// tuple back into the literal's own storage.
#[test]
fn a_for_over_a_literal_keeps_the_literal_as_the_sequence_it_walks() {
    let src = r#"
fn main() {
    let mut total = 0
    for cmd in #[1, 2, 3, 4] {
        let dx, dy = dir(cmd)
        total += dx + dy
    }
    println("{}", total)
    let mut seen = 0
    for cmd in #[2, 4] {
        let dx, _ = dir(cmd)
        seen += dx
    }
    println("{}", seen)
}

fn dir(cmd: i64) -> (i64, i64) {
    match cmd {
        1 => (0, -1)
        2 => (0, 1)
        3 => (-1, 0)
        4 => (1, 0)
        _ => panic("bad")
    }
}
"#;
    let dir = fresh_dir("for_literal_tuple_call");
    let path = write_source(&dir, "for_literal_tuple_call", src);
    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();

    let vm = run_vm(&path);
    let bin = build_native(&path, &scratch).expect("native build");
    let native = run_native(&bin);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(vm.0, "0\n1\n", "vm stdout");
    assert_eq!(native.0, vm.0, "tier parity");
}

/// A `&mut Option<T>` names the caller's slot, so a `*o = Some(..)` through
/// it publishes into that slot rather than writing through the carrier read
/// as a pointer.
#[test]
fn a_deref_write_through_a_mut_option_reference_publishes_to_the_caller() {
    let src = r#"
fn set_pair(o: &mut Option<(i64, i64)>) {
    *o = Some((1, 2))
}

fn set_scalar(o: &mut Option<i64>) {
    *o = Some(7)
}

fn set_result(r: &mut Result<i64, String>) {
    *r = Ok(9)
}

fn main() {
    let mut pair = Some((0, 0))
    set_pair(&mut pair)
    println("{}", pair)
    let mut scalar = Some(0)
    set_scalar(&mut scalar)
    println("{}", scalar)
    let mut answer: Result<i64, String> = Ok(0)
    set_result(&mut answer)
    println("{}", answer)
}
"#;
    let dir = fresh_dir("option_deref_write");
    let path = write_source(&dir, "option_deref_write", src);
    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();

    let vm = run_vm(&path);
    let bin = build_native(&path, &scratch).expect("native build");
    let native = run_native(&bin);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(vm.0, "Some((1, 2))\nSome(7)\nOk(9)\n", "vm stdout");
    assert_eq!(native.0, vm.0, "tier parity");
}

/// `n.to_string().chars()` answers the same cursor a `String` receiver's
/// `chars()` does, so every consumer of it reads iterator state rather than
/// the formatted scalars as one.
#[test]
fn numeric_chars_answers_a_cursor_every_consumer_reads() {
    let src = r#"
fn main() {
    let digits = 123_456.to_string().chars().collect()
    println("{}", digits.len())
    println("{}", 123_456.to_string().chars().count())
    let mut seen = 0
    for c in 123.to_string().chars() {
        seen += 1
    }
    println("{}", seen)
}
"#;
    let dir = fresh_dir("numeric_chars_cursor");
    let path = write_source(&dir, "numeric_chars_cursor", src);
    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();

    let vm = run_vm(&path);
    let bin = build_native(&path, &scratch).expect("native build");
    let native = run_native(&bin);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(vm.0, "6\n6\n3\n", "vm stdout");
    assert_eq!(native.0, vm.0, "tier parity");
}

#[test]
fn question_mark_without_conversion_is_rejected() {
    // `?` on a `Result<_, B>` inside a function answering `Result<_, C>`,
    // where `C` converts only from `A`: the checker names both error types
    // rather than letting the lowering call `C::from` with a `B`.
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../feature-testing-examples/question_mark_no_conversion.gos");
    let out = Command::new(gos_bin())
        .arg("check")
        .arg(&fixture)
        .output()
        .expect("spawn gos check");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "check unexpectedly passed: {stderr}");
    assert!(stderr.contains("GT0093"), "expected GT0093, got: {stderr}");
    assert!(
        stderr.contains("`B`") && stderr.contains("`C`"),
        "diagnostic must name both error types: {stderr}"
    );
}

#[test]
fn stream_and_child_methods_answer_their_declared_types() {
    // A standard stream's `read_line` / `flush` and a child's `wait` answer
    // `Result<i64, _>`, `()`, and `Result<i64, _>`; a `bool` annotation on
    // any of them is a mismatch, and a method the stream lacks is reported.
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../feature-testing-examples/stream_method_types_rejected.gos");
    let out = Command::new(gos_bin())
        .arg("check")
        .arg(&fixture)
        .output()
        .expect("spawn gos check");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "check unexpectedly passed: {stderr}");
    assert_eq!(
        stderr.matches("GT0001").count(),
        3,
        "expected three type mismatches: {stderr}"
    );
    assert!(stderr.contains("write_all"), "expected the unknown method: {stderr}");
}

#[test]
fn scalar_bounds_reject_non_numeric_operands() {
    // `max("x", "y")`, a tuple, a `bool`, and a mixed pair each report a type
    // mismatch where they printed `0` on the VM and a heap address compiled.
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../feature-testing-examples/scalar_bound_operand_types.gos");
    let out = Command::new(gos_bin())
        .arg("check")
        .arg(&fixture)
        .output()
        .expect("spawn gos check");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "check unexpectedly passed: {stderr}");
    assert_eq!(
        stderr.matches("error[GT0001]").count(),
        4,
        "each bad operand set is reported once: {stderr}"
    );
    assert!(stderr.contains("a number or char"), "names what it expected: {stderr}");
}
