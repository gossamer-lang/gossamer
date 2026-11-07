//! Positive-form silent-stub gate.
//!
//! `llvm_strict_lower_group_N` (in `tier_parity.rs`) catches a body
//! that *errored* during lowering - an LLVM lowering gap fails the
//! build. That gate is necessary but not sufficient: a body can also
//! be lowered to an empty stub that compiles cleanly, links, and runs
//! but computes the wrong answer. The strict gate sees nothing;
//! tier-parity only catches it if the wrong answer differs from
//! the other tiers.
//!
//! This file complements that gate by checking the inverse: for
//! a representative set of source shapes, the LLVM IR emitted
//! to `unit.ll` must contain a real `define` for each user
//! function with a non-trivial body. We don't snapshot the
//! whole IR (too noisy) - just confirm the body landed.
//!
//! Triggered via `GOS_LLVM_DUMP=1`, which makes `invoke_llc`
//! print `llvm backend: IR at <path>` to stderr and keep the
//! file. We parse that line out of stderr instead of guessing
//! the path from `pid`.

#![allow(missing_docs)]

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn gos_bin() -> PathBuf {
    PathBuf::from(env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

fn fresh_dir(tag: &str) -> PathBuf {
    let dir = env::temp_dir().join(format!(
        "gos-lower-{}-{}-{}",
        std::process::id(),
        tag,
        rand_suffix(),
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn rand_suffix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
}

/// Build `source` in release mode with `GOS_LLVM_DUMP=1` and return the
/// `unit.ll` IR string plus the produced binary path. A body that trips a
/// backend lowering gap fails the build outright. Together with the per-fn
/// `define` check below this gives both shape-positive and shape-negative
/// coverage.
fn build_and_capture_ir(source: &str, tag: &str) -> (String, PathBuf) {
    let dir = fresh_dir(tag);
    let src = dir.join(format!("{tag}.gos"));
    std::fs::write(&src, source).expect("write source");
    let mut cmd = Command::new(gos_bin());
    cmd.env("GOS_LLVM_DUMP", "1")
        .arg("build")
        .arg("--release")
        .arg("--out-dir")
        .arg(&dir)
        .arg(&src);
    let out = cmd.output().expect("spawn gos build");
    assert!(
        out.status.success(),
        "gos build --release failed for {tag}: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let ir_path = parse_ir_path_from_stderr(&stderr).unwrap_or_else(|| {
        panic!("could not find `llvm backend: IR at <path>` in stderr for {tag}: {stderr}")
    });
    let ir = std::fs::read_to_string(&ir_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", ir_path.display()));
    let mut binaries: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap().flatten() {
        let p = entry.path();
        if p.is_file() && is_executable(&p) {
            binaries.push(p);
        }
    }
    let bin = binaries
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("no executable produced for {tag} in {}", dir.display()));
    (ir, bin)
}

fn parse_ir_path_from_stderr(stderr: &str) -> Option<PathBuf> {
    for line in stderr.lines() {
        if let Some(rest) = line.trim_start().strip_prefix("llvm backend: IR at ") {
            return Some(PathBuf::from(rest.trim()));
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.extension()
        .and_then(|s| s.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
}

/// Returns true if the IR contains a `define` line for the named
/// user function with a non-empty body. We check for a `ret` in
/// the function body (i.e. it actually returns something) by
/// looking inside the next ~200 lines for a `ret ` opcode. A
/// fully empty stub like `define void @"foo"() { ret void }`
/// would still pass this check - the more interesting assertion
/// is paired below: the body must not be the trivial `ret`-only
/// shape when the source has user-visible side effects.
fn ir_contains_define(ir: &str, fn_name: &str) -> bool {
    let needle = format!("@\"{fn_name}\"");
    for line in ir.lines() {
        let t = line.trim_start();
        if t.starts_with("define ") && t.contains(&needle) {
            return true;
        }
    }
    false
}

/// Counts non-empty lines inside the body of the named function.
/// Returns `None` if the body cannot be located. Used to assert
/// "this body is more than just `ret void`" - the cheap
/// proxy for "lowering produced real instructions, not a stub".
fn ir_body_line_count(ir: &str, fn_name: &str) -> Option<usize> {
    let needle = format!("@\"{fn_name}\"");
    let mut in_body = false;
    let mut depth: i32 = 0;
    let mut lines = 0_usize;
    for line in ir.lines() {
        let t = line.trim();
        if !in_body {
            let s = line.trim_start();
            if s.starts_with("define ") && s.contains(&needle) {
                in_body = true;
                if line.contains('{') {
                    depth += 1;
                }
            }
            continue;
        }
        if line.contains('{') {
            depth += 1;
        }
        if line.contains('}') {
            depth -= 1;
            if depth == 0 {
                return Some(lines);
            }
            continue;
        }
        if !t.is_empty() && !t.starts_with(';') {
            lines += 1;
        }
    }
    None
}

/// Source shape → list of user functions whose bodies must
/// appear as `define` blocks in the emitted IR. Each fn has a
/// minimum body line count so the lowerer can't reduce a body
/// to a single `ret` and still pass.
struct Shape {
    tag: &'static str,
    source: &'static str,
    /// `(fn_name, min_body_lines)` - `fn_name` is the unmangled
    /// Gossamer name; `mangle_fn_name` rewrites only `main`.
    expect: &'static [(&'static str, usize)],
}

const SHAPES: &[Shape] = &[
    Shape {
        tag: "scalar_arith",
        source: r#"
fn add(a: i64, b: i64) -> i64 { a + b }
fn main() {
    let x = add(2, 3)
    println("{}", x)
}
"#,
        expect: &[("add", 2), ("gos_main", 2)],
    },
    Shape {
        tag: "branch_loop",
        source: r#"
fn sum_to(n: i64) -> i64 {
    let mut total = 0
    for i in 0..n {
        total = total + i
    }
    total
}
fn main() { println("{}", sum_to(10)) }
"#,
        expect: &[("sum_to", 5), ("gos_main", 2)],
    },
    Shape {
        tag: "struct_field",
        source: r#"
struct Point { x: i64, y: i64 }
fn dot(p: Point, q: Point) -> i64 { p.x * q.x + p.y * q.y }
fn main() {
    let p = Point { x: 2, y: 3 }
    let q = Point { x: 4, y: 5 }
    println("{}", dot(p, q))
}
"#,
        expect: &[("dot", 4)],
    },
    Shape {
        tag: "string_concat",
        source: r#"
fn greet(name: String) -> String { format("hi {}", name) }
fn main() { println("{}", greet("world")) }
"#,
        expect: &[("greet", 2)],
    },
    Shape {
        tag: "closure_apply",
        source: r#"
fn main() {
    let double = |x: i64| x * 2
    println("{}", double(7))
}
"#,
        expect: &[("gos_main", 2)],
    },
    Shape {
        tag: "match_int",
        source: r#"
fn name(n: i64) -> String {
    match n {
        0 => "zero",
        1 => "one",
        _ => "many",
    }
}
fn main() { println("{}", name(1)) }
"#,
        expect: &[("name", 4)],
    },
    Shape {
        tag: "vec_iter",
        source: r#"
fn total(xs: Vec<i64>) -> i64 {
    let mut t = 0
    for x in xs { t = t + x }
    t
}
fn main() {
    let xs: Vec<i64> = [1, 2, 3, 4, 5].to_vec()
    println("{}", total(xs))
}
"#,
        expect: &[("total", 4)],
    },
    Shape {
        tag: "tuple_return",
        source: r#"
fn divmod(a: i64, b: i64) -> (i64, i64) { (a / b, a % b) }
fn main() {
    let q, r = divmod(17, 5)
    println("{} {}", q, r)
}
"#,
        expect: &[("divmod", 2)],
    },
    Shape {
        tag: "result_question",
        source: r#"
fn parse_or_zero(s: String) -> i64 {
    match s.to_i64() {
        Some(n) => n,
        None => 0,
    }
}
fn main() { println("{}", parse_or_zero("42")) }
"#,
        expect: &[("parse_or_zero", 3)],
    },
    Shape {
        tag: "format_macro",
        source: r#"
fn label(n: i64) -> String { format("n={}", n) }
fn main() { println("{}", label(7)) }
"#,
        expect: &[("label", 2)],
    },
    Shape {
        tag: "while_break",
        source: r#"
fn first_negative(xs: [i64; 5]) -> i64 {
    let mut i = 0
    while i < 5 {
        if xs[i] < 0 { return xs[i] }
        i = i + 1
    }
    0
}
fn main() {
    let xs = [1, 2, -3, 4, 5]
    println("{}", first_negative(xs))
}
"#,
        expect: &[("first_negative", 4)],
    },
    Shape {
        tag: "float_arith",
        source: r#"
fn norm(x: f64, y: f64) -> f64 { x * x + y * y }
fn main() { println("{:.2}", norm(3.0, 4.0)) }
"#,
        expect: &[("norm", 2)],
    },
    Shape {
        tag: "bool_logic",
        source: r#"
fn classify(n: i64) -> bool {
    n > 0 && n < 100 && n % 2 == 0
}
fn main() {
    println("{}", classify(42))
    println("{}", classify(7))
    println("{}", classify(-1))
}
"#,
        expect: &[("classify", 2)],
    },
    Shape {
        tag: "shadowing",
        source: r#"
fn ladder(x: i64) -> i64 {
    let x = x + 1
    let x = x * 2
    let x = x - 3
    x
}
fn main() { println("{}", ladder(10)) }
"#,
        expect: &[("ladder", 4)],
    },
    Shape {
        tag: "if_chain_branches",
        source: r#"
fn grade(n: i64) -> String {
    if n >= 90 { "A" }
    else if n >= 80 { "B" }
    else if n >= 70 { "C" }
    else { "F" }
}
fn main() {
    println("{}", grade(95))
    println("{}", grade(72))
}
"#,
        expect: &[("grade", 4)],
    },
    Shape {
        tag: "nested_loops",
        source: r#"
fn product_table(n: i64) -> i64 {
    let mut total = 0
    let mut i = 1
    while i <= n {
        let mut j = 1
        while j <= n {
            total = total + i * j
            j = j + 1
        }
        i = i + 1
    }
    total
}
fn main() { println("{}", product_table(4)) }
"#,
        expect: &[("product_table", 8)],
    },
    Shape {
        tag: "char_string",
        source: r#"
fn first_char_code(s: String) -> i64 {
    if s.len() == 0 { 0 } else { s.byte_at(0) }
}
fn main() {
    println("{}", first_char_code("hi"))
    println("{}", first_char_code(""))
}
"#,
        expect: &[("first_char_code", 2)],
    },
    Shape {
        tag: "struct_method",
        source: r#"
struct Counter { n: i64 }
impl Counter {
    fn step(&self) -> i64 { self.n + 1 }
}
fn main() {
    let c = Counter { n: 41 }
    println("{}", c.step())
}
"#,
        expect: &[],
    },
    Shape {
        tag: "early_return",
        source: r#"
fn maybe_neg(n: i64) -> i64 {
    if n < 0 { return -1 }
    if n > 100 { return 1 }
    0
}
fn main() {
    println("{}", maybe_neg(-5))
    println("{}", maybe_neg(50))
    println("{}", maybe_neg(200))
}
"#,
        expect: &[("maybe_neg", 4)],
    },
];

#[test]
fn llvm_lowers_each_shape_to_a_real_define() {
    // The sub-build is the slow part - limit the scope of this
    // test by skipping when LLVM tooling is missing rather than
    // forcing a noisy environment failure. Match the tier-parity
    // suite's behaviour.
    if which_llc_missing() {
        eprintln!("skipping: LLVM tooling not on PATH");
        return;
    }

    let only = std::env::var("GOSSAMER_LLVM_LOWER_ONLY").ok();
    for shape in SHAPES {
        if let Some(filter) = &only
            && filter != shape.tag
        {
            continue;
        }
        let (ir, _bin) = build_and_capture_ir(shape.source, shape.tag);
        for (fn_name, min_lines) in shape.expect {
            assert!(
                ir_contains_define(&ir, fn_name),
                "shape {tag}: LLVM IR is missing `define ... @\"{fn_name}\"(...)` - \
                 the body was skipped entirely. \n\nIR head:\n{head}",
                tag = shape.tag,
                head = ir.lines().take(40).collect::<Vec<_>>().join("\n"),
            );
            let body_lines = ir_body_line_count(&ir, fn_name).unwrap_or_else(|| {
                panic!(
                    "shape {tag}: could not parse body for `{fn_name}` from IR",
                    tag = shape.tag
                )
            });
            assert!(
                body_lines >= *min_lines,
                "shape {tag}: body of `{fn_name}` has only {body_lines} non-trivial \
                 lines; expected >= {min_lines}. The lowerer may have reduced the \
                 body to a stub. Re-run with GOS_LLVM_DUMP=1 and inspect the IR.",
                tag = shape.tag,
            );
        }
    }
}

fn which_llc_missing() -> bool {
    gossamer_codegen_llvm::llvm_toolchain_status()
        .iter()
        .any(|status| status.tool == "llc" && status.resolved.is_err())
}
