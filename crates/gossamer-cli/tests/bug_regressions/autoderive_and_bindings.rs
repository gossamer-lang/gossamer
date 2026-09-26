#[test]
fn autoderive_synthesizes_for_narrow_integer_fields() {
    // Structs whose fields use `i32` / `u8` / `i16` etc. must
    // still get `from_json` / `to_json` synthesized. Before the
    // fix, the FieldKind table only covered `i64`, so any narrow
    // integer caused the entire struct to be skipped and the
    // user's `from_json::<Type>(text)?` call surfaced as
    // `field access on non-struct ()` at runtime.
    let src = r#"
use std::errors

struct Counts {
    small: u8,
    medium: i32,
    big: i64,
}

fn main() -> Result<(), errors::Error> {
    let text = "{\"small\":255,\"medium\":-1,\"big\":9000000000}"
    let c = from_json::<Counts>(text)?
    println("small={} medium={} big={}", c.small, c.medium, c.big)
    Ok(())
}
"#;
    let dir = fresh_dir("autoderive_narrow_int");
    let path = write_source(&dir, "narrow", src);
    let run = run_vm(&path);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    assert_eq!(
        run.0.trim_end(),
        "small=255 medium=-1 big=9000000000",
        "narrow-int autoderive mismatch (vm); stdout: {:?}",
        run.0
    );

    let scratch = dir.join("bin");
    fs::create_dir_all(&scratch).unwrap();
    let bin = build_native_release(&path, &scratch).expect("build release");
    let out = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(out.2, Some(0), "release stderr: {}", out.1);
    assert_eq!(
        out.0.trim_end(),
        "small=255 medium=-1 big=9000000000",
        "narrow-int autoderive mismatch (llvm release); stdout: {:?}",
        out.0
    );
}

#[test]
fn write_file_with_vec_u8_preserves_embedded_nul() {
    // `fs::write(path, &Vec<u8>)` must route through the
    // bytes-shaped runtime helper; the c-string helper would
    // truncate at the first NUL and silently corrupt binary
    // writes. Reads the file back to confirm every byte
    // survived round-trip on each tier.
    let dir = fresh_dir("write_bytes_nul");
    let tmp_path = dir.join("payload.bin");
    let tmp_str = tmp_path.display().to_string();
    let src = format!(
        r#"
use std::errors
use std::fs

fn main() -> Result<(), errors::Error> {{
    let payload: Vec<u8> = [72, 105, 0, 65, 66, 67, 10].to_vec()
    fs::write("{tmp}", payload)?
    let back = fs::read("{tmp}")?
    println("len={{}}", back.len())
    println("byte2={{}}", back[2])
    println("byte3={{}}", back[3])
    println("byte6={{}}", back[6])
    Ok(())
}}
"#,
        tmp = tmp_str.replace('\\', "\\\\"),
    );
    let path = write_source(&dir, "write_nul", &src);
    let run = run_vm(&path);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    let expected = "len=7\nbyte2=0\nbyte3=65\nbyte6=10";
    assert_eq!(
        run.0.trim_end(),
        expected,
        "binary write round-trip mismatch (vm); stdout: {:?}",
        run.0
    );

    let _ = fs::remove_file(&tmp_path);
    let scratch = dir.join("bin");
    fs::create_dir_all(&scratch).unwrap();
    let bin = build_native_release(&path, &scratch).expect("build release");
    let out = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(out.2, Some(0), "release stderr: {}", out.1);
    assert_eq!(
        out.0.trim_end(),
        expected,
        "binary write round-trip mismatch (llvm release); stdout: {:?}",
        out.0
    );
}

#[test]
fn yaml_autoderive_round_trips_struct_via_yaml() {
    // Every named struct also gets `from_yaml` / `to_yaml`
    // alongside the JSON pair. The methods route through
    // `yaml::to_json` / `yaml::from_json` and reuse the
    // JSON decoder's strict field-type checks.
    let src = r#"
use std::errors

struct AppCfg {
    name: String,
    port: i64,
    debug: bool,
}

fn main() -> Result<(), errors::Error> {
    let yaml = "name: gossamer\nport: 8080\ndebug: true\n"
    let cfg = from_yaml::<AppCfg>(yaml)?
    println("{} {} {}", cfg.name, cfg.port, cfg.debug)

    let back = to_yaml::<AppCfg>(cfg)?
    let again = from_yaml::<AppCfg>(back)?
    println("{} {}", again.name, again.port)
    Ok(())
}
"#;
    let dir = fresh_dir("yaml_autoderive");
    let path = write_source(&dir, "yaml_derive", src);
    let run = run_vm(&path);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    let expected = "gossamer 8080 true\ngossamer 8080";
    assert_eq!(
        run.0.trim_end(),
        expected,
        "yaml round-trip mismatch (vm); stdout: {:?}",
        run.0
    );

    let scratch = dir.join("bin");
    fs::create_dir_all(&scratch).unwrap();
    let bin = build_native_release(&path, &scratch).expect("build release");
    let out = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(out.2, Some(0), "release stderr: {}", out.1);
    assert_eq!(
        out.0.trim_end(),
        expected,
        "yaml round-trip mismatch (llvm release); stdout: {:?}",
        out.0
    );
}

#[test]
fn sync_map_round_trips_set_get_delete_across_tiers() {
    // `sync::Map` is a concurrent string-keyed map. insert / get /
    // contains_key / remove / len must dispatch correctly on every
    // tier. The Option<String> returned by `.get` was previously
    // pinned to `i64` in the kind_dispatch fallback, surfacing
    // as `bar=<raw-pointer-as-number>` for the Some arm and
    // `Some(_)` being taken even for the None case.
    let src = r#"
use std::sync

fn main() {
    let mut m = sync::Map::new()
    sync::Map::insert(m, "alpha", "1")
    sync::Map::insert(m, "beta", "2")
    println("len={}", sync::Map::len(m))
    match sync::Map::get(m, "beta") {
        Some(v) => println("beta={}", v),
        None => println("beta missing"),
    }
    match sync::Map::get(m, "nope") {
        Some(_) => println("nope unexpected"),
        None => println("nope=None"),
    }
    sync::Map::remove(m, "alpha")
    println("contains alpha: {}", sync::Map::contains_key(m, "alpha"))
    println("after-delete len={}", sync::Map::len(m))
}
"#;
    let dir = fresh_dir("sync_map");
    let path = write_source(&dir, "sync_map", src);
    let expected = "len=2\nbeta=2\nnope=None\ncontains alpha: false\nafter-delete len=1";
    let run = run_vm(&path);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    assert_eq!(
        run.0.trim_end(),
        expected,
        "sync::Map mismatch (vm); stdout: {:?}",
        run.0
    );

    let scratch = dir.join("bin");
    fs::create_dir_all(&scratch).unwrap();
    let bin = build_native_release(&path, &scratch).expect("build release");
    let out = run_native(&bin);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(out.2, Some(0), "release stderr: {}", out.1);
    assert_eq!(
        out.0.trim_end(),
        expected,
        "sync::Map mismatch (llvm release); stdout: {:?}",
        out.0
    );
}

#[test]
fn deref_assign_through_mut_i64_runs_under_llvm() {
    // Bug fixed in 0.10.0: `*s = expr` through `&mut i64`
    // segfaulted in the LLVM AOT tier because `&mut state` was
    // lowered as the i64 value instead of the slot address.
    // Three coordinated MIR + LLVM + cranelift changes close the
    // class. The reproducer is the LCG step the bench-game LCRNG
    // benches use.
    let src = r#"
fn lcg(s: &mut i64) -> i64 {
    *s = *s * 6364136223846793005 + 1442695040888963407
    (*s >> 33) & 0x7fffffff
}
fn main() {
    let mut state: i64 = 42
    let n = lcg(&mut state)
    println("{}", n)
}
"#;
    let dir = fresh_dir("deref_assign_mut_i64");
    let path = write_source(&dir, "deref_assign_mut_i64", src);
    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();
    let bin = build_native_release(&path, &scratch).expect("release build");
    let out = run_native(&bin);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.2, Some(0), "stderr: {}", out.1);
    // First LCG step on state=42: returns 1220265334.
    assert!(
        out.0.contains("1220265334"),
        "expected LCG output 1220265334, got: {:?}",
        out.0
    );
}

#[test]
fn returned_mut_vec_reference_is_rejected_before_vm_execution() {
    let src = r#"
fn change(v: &mut Vec<i64>) -> &mut Vec<i64> {
    v[0] = 0
    v
}

fn main() {
    let mut a: Vec<i64> = Vec::from([1, 2])
    let b = change(&mut a)
    println("a: {:?}", a)
    println("b: {:?}", b)
    b[0] = 2
    println("a: {:?}", a)
    println("b: {:?}", b)
}
"#;
    let dir = fresh_dir("returned_mut_vec_ref");
    let path = write_source(&dir, "returned_mut_vec_ref", src);
    let (stdout, stderr, code) = run_vm(&path);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, Some(1), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stderr.contains("GT0052"), "stderr: {stderr}");
    assert!(
        stderr.contains("reference cannot escape through a function return"),
        "stderr: {stderr}"
    );
}

#[test]
fn mut_self_field_compound_assign_writes_back() {
    // Bug fixed in 0.10.0: `self.field += 1` in an `&mut self`
    // method silently dropped the mutation in the LLVM AOT tier.
    let src = r#"
struct Counter { n: i64 }
impl Counter {
    fn bump(&mut self) { self.n += 1 }
}
fn main() {
    let mut c = Counter { n: 0 }
    c.bump()
    c.bump()
    c.bump()
    println("{}", c.n)
}
"#;
    let dir = fresh_dir("mut_self_compound_assign");
    let path = write_source(&dir, "mut_self_compound_assign", src);
    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();
    let bin = build_native_release(&path, &scratch).expect("release build");
    let out = run_native(&bin);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.2, Some(0), "stderr: {}", out.1);
    assert_eq!(out.0.trim_end(), "3", "got {:?}", out.0);
}

#[test]
fn multi_dim_fixed_array_index_walks_inner_strides() {
    // Bug fixed in 0.10.0: `lower_place_address` did not advance
    // `current_ty` after a `Projection::Index`, so `arr[i][j]` over
    // `[[T; A]; B]` used the OUTER array's bounds for the inner
    // index. Iron Knight's 3D zobrist write hit this.
    let src = r#"
struct Z { pieces: [[[i64; 64]; 6]; 2] }
fn main() {
    let mut z = Z { pieces: [[[0; 64]; 6], [[0; 64]; 6]] }
    let mut s: i64 = 0
    while s < 2 {
        let mut p: i64 = 0
        while p < 6 {
            let mut sq: i64 = 0
            while sq < 64 {
                z.pieces[s][p][sq] = s * 1000 + p * 100 + sq
                sq += 1
            }
            p += 1
        }
        s += 1
    }
    println("z[1][5][63]={}", z.pieces[1][5][63])
}
"#;
    let dir = fresh_dir("multi_dim_array");
    let path = write_source(&dir, "multi_dim_array", src);
    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();
    let bin = build_native_release(&path, &scratch).expect("release build");
    let out = run_native(&bin);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.2, Some(0), "stderr: {}", out.1);
    assert!(
        out.0.contains("z[1][5][63]=1563"),
        "expected z[1][5][63]=1563 (1*1000+5*100+63), got: {:?}",
        out.0
    );
}

#[test]
fn env_args_empty_iter_does_not_segfault() {
    // Bug fixed in 0.10.0: `gos_rt_set_args` stored a null GosVec
    // pointer when `argc <= 1`, so iterating `env::args()` with
    // no user args segfaulted on the iterator's null-header walk.
    let src = r#"
use std::env
fn main() {
    let args = env::args()
    println("len={}", args.len())
    for a in args {
        println("{}", a)
    }
}
"#;
    let dir = fresh_dir("env_args_empty");
    let path = write_source(&dir, "env_args_empty", src);
    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();
    let bin = build_native_release(&path, &scratch).expect("release build");
    let out = run_native(&bin);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        out.2,
        Some(0),
        "no-arg run must exit cleanly; stderr: {}",
        out.1
    );
    assert!(
        out.0.contains("len=0"),
        "expected empty args to report len=0, got: {:?}",
        out.0
    );
}

#[test]
fn vec_pop_on_typed_storage_shrinks_by_one() {
    // Bug fixed in 0.10.0: VM `builtin_pop` fell into the
    // `_ => empty_array` catch-all for `Value::IntArray` /
    // `Value::FloatVec` receivers, and the writeback then moved
    // the empty result into the receiver - clobbering every
    // element instead of removing only the last one.
    let src = r#"
fn main() {
    let mut xs: Vec<i64> = Vec::from([10, 20, 30, 40])
    let _ = xs.pop()
    println("len={}", xs.len())
    println("xs[0]={}", xs[0])
    println("xs[2]={}", xs[2])
}
"#;
    let dir = fresh_dir("vec_pop_typed");
    let path = write_source(&dir, "vec_pop_typed", src);
    let run = run_vm(&path);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    assert!(
        run.0.contains("len=3") && run.0.contains("xs[0]=10") && run.0.contains("xs[2]=30"),
        "expected len=3 + xs[0]=10 + xs[2]=30 after a single pop, got: {:?}",
        run.0
    );
}

#[test]
fn hashmap_keys_router_does_not_get_shadowed_by_json() {
    // Bug fixed in 0.10.0: `install_module("json", …)` unconditionally
    // pushed `("keys", builtin_json_keys)` AFTER the Map surface
    // registered `("keys", builtin_map_keys)`. The later json push
    // overrode the bare-name registry, so every `m.keys()` on a
    // Map silently dispatched to the JSON helper which returns
    // `None` for non-Struct receivers - surfacing as `ks.len() == 0`
    // even with multiple inserts. Receiver-routing wrapper now
    // dispatches by Value shape.
    let src = r#"
use std::collections::Map
fn main() {
    let mut m: Map<i64, i64> = Map::new()
    m.insert(1, 10)
    m.insert(2, 20)
    m.insert(3, 30)
    let ks = m.keys()
    println("len={}", ks.len())
}
"#;
    let dir = fresh_dir("hashmap_keys_router");
    let path = write_source(&dir, "hashmap_keys_router", src);
    let run = run_vm(&path);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    assert!(run.0.contains("len=3"), "expected 3 keys, got: {:?}", run.0);
}

#[test]
fn explicit_vec_return_uses_vec_storage() {
    // A Vec return must be constructed explicitly from an array.
    // lowered the array literal as a flat `Array<String; 2>` and
    // returned the stack-aggregate bytes through the slot that the
    // caller dereferenced as a `*mut GosVec` - len read as garbage
    // bits, then `for s in xs` ran zero iterations. The Return
    // path now coerces `Array<T; N>` → `Vec<T>` via
    // `gos_rt_vec_from_arr` whenever the declared return type is
    // `Vec(elem)` or `Slice(elem)` with matching `elem`.
    let src = r#"
fn cols() -> Vec<String> {
    return Vec::from(["id", "name", "value"])
}
fn modes() -> Vec<i64> {
    let values = [7, 8, 9]
    values.to_vec()
}
fn main() {
    let xs = cols()
    println("len={}", xs.len())
    for s in xs { println("{}", s) }
    let ms = modes()
    println("modes={} sum={}", ms.len(), ms[0] + ms[1] + ms[2])
}
"#;
    let dir = fresh_dir("return_array_to_slice");
    let path = write_source(&dir, "return_array_to_slice", src);
    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();
    let bin = build_native_release(&path, &scratch).expect("release build");
    let out = run_native(&bin);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.2, Some(0), "stderr: {}", out.1);
    assert!(
        out.0.contains("len=3")
            && out.0.contains("id")
            && out.0.contains("value")
            && out.0.contains("modes=3 sum=24"),
        "expected both explicit and tail array returns to be Vec values, got: {:?}",
        out.0
    );
}

#[test]
fn typed_int_array_parameter_uses_generic_index_path() {
    // Function arguments use the general Value ABI. In particular, a
    // `[i64; N]` parameter may be boxed as `Value::Array`, so parameters must
    // stay on the generic indexing path rather than being incorrectly marked
    // as `Value::IntArray` fast-path storage.
    let src = r#"
fn slide(arr: [i64; 4]) -> i64 {
    let mut sum: i64 = 0
    for i in 0..4 { sum += arr[i] }
    sum
}
fn main() {
    for k in 0..3 {
        let r = slide([1, 2, 3, 4])
        println("k={} r={}", k, r)
    }
}
"#;
    let dir = fresh_dir("typed_int_array_get_fallback");
    let path = write_source(&dir, "typed_int_array_get_fallback", src);
    let run = run_vm(&path);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(run.2, Some(0), "vm stderr: {}", run.1);
    assert_eq!(
        run.0.matches("r=10").count(),
        3,
        "expected r=10 three times, got: {:?}",
        run.0
    );
}

#[test]
fn logical_and_or_short_circuit_in_compiled_tier() {
    // Bug fixed in 0.10.0: `lower_binary` evaluated both sides of
    // `&&` / `||` unconditionally in the MIR lowering, so a guarded
    // bounds check like `while j > 0 && arr[j - 1] < x` panicked
    // with `the index is -1` once j reached 0 - the RHS fired
    // even though the LHS was already false. The lowering now
    // branches on the LHS and evaluates the RHS only on the path
    // that needs it.
    let src = r#"
fn check_idx(arr: [i64; 4], j: i64) -> bool {
    arr[j - 1] < 100
}
fn main() {
    let arr: [i64; 4] = [1, 2, 3, 4]
    let mut j: i64 = 2
    while j > 0 && check_idx(arr, j) {
        j -= 1
    }
    println("done j={}", j)
    let mut k: i64 = 0
    while k < 5 || k > 100 {
        k += 1
        if k > 3 { break }
    }
    println("k={}", k)
}
"#;
    let dir = fresh_dir("logical_short_circuit");
    let path = write_source(&dir, "logical_short_circuit", src);
    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();
    let bin = build_native_release(&path, &scratch).expect("release build");
    let out = run_native(&bin);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.2, Some(0), "stderr: {}", out.1);
    assert!(
        out.0.contains("done j=0") && out.0.contains("k=4"),
        "expected short-circuit semantics, got: {:?}",
        out.0
    );
}

#[test]
fn vec_of_struct_index_field_reads_and_writes_through_data_buffer() {
    // Bug fixed in 0.10.0: indexing a `Vec<Body>` (multi-slot struct
    // elements) in a place expression - `bodies[i].x` for a read or
    // `bodies[i].vx = v` for a write - built a flat `Projection::Index`
    // that strode off the `*mut GosVec` *header* instead of the data
    // buffer, so every element past index 0 read/wrote garbage. The
    // place lowerer now routes Vec-with-multi-slot-element indexing
    // through `gos_rt_vec_get_ptr` and binds the element address to a
    // `&elem` local so the appended `Field` projection auto-derefs and
    // lands inside the Vec's storage for both reads and writes.
    let src = r#"
struct Body { x: f64, vx: f64, mass: f64 }
fn main() {
    let mut bs: Vec<Body> = Vec::from([])
    bs.push(Body { x: 1.0, vx: 2.0, mass: 10.0 })
    bs.push(Body { x: 4.0, vx: 5.0, mass: 20.0 })
    bs[1].x = 9.0
    println("{} {} {}", bs[0].x, bs[1].x, bs[1].mass)
}
"#;
    let dir = fresh_dir("vec_struct_index_field");
    let path = write_source(&dir, "vec_struct_index_field", src);
    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();
    let bin = build_native_release(&path, &scratch).expect("release build");
    let out = run_native(&bin);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.2, Some(0), "stderr: {}", out.1);
    assert!(
        out.0.contains("1 9 20"),
        "expected element-1 field write + correct strides, got: {:?}",
        out.0
    );
}

#[test]
fn mut_fixed_struct_array_not_promoted_keeps_layout_across_calls() {
    // Bug fixed in 0.10.0: `let mut bodies: [Body; N]` was
    // unconditionally promoted to a heap `Vec<Body>` because the
    // binding was `mut` with an array literal. Passing `&bodies` to a
    // function declared `fn energy(b: &[Body; N])` then desynchronised
    // the element stride (the callee strode the GosVec header as inline
    // data) and produced NaN. The promotion now fires only when the
    // binding actually receives a growth method (push / pop / sort /
    // …); a fixed array that is merely indexed, field-mutated, or
    // passed to a `[T; N]` parameter keeps its inline layout.
    let src = r#"
struct Body { x: f64, vx: f64, mass: f64 }
fn total_momentum(b: [Body; 2]) -> f64 {
    let mut p = 0.0
    for i in 0..2 { p += b[i].vx * b[i].mass }
    p
}
fn main() {
    let mut bodies: [Body; 2] = [
        Body { x: 1.0, vx: 0.1, mass: 10.0 },
        Body { x: 2.0, vx: 0.4, mass: 20.0 },
    ]
    bodies[0].vx = 0.5
    println("{:.4}", total_momentum(bodies))
}
"#;
    let dir = fresh_dir("mut_fixed_struct_array");
    let path = write_source(&dir, "mut_fixed_struct_array", src);
    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();
    let bin = build_native_release(&path, &scratch).expect("release build");
    let out = run_native(&bin);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.2, Some(0), "stderr: {}", out.1);
    // 0.5*10 + 0.4*20 = 5 + 8 = 13.0
    assert!(
        out.0.contains("13.0000"),
        "expected fixed-array field mutation + correct stride, got: {:?}",
        out.0
    );
}

#[test]
fn explicit_vec_from_array_supports_push_and_sort() {
    // Fixed arrays never grow. Convert explicitly when the value needs
    // length-changing Vec methods.
    let src = r#"
fn main() {
    let mut xs = Vec::from([3, 1, 2])
    xs.push(4)
    xs.sort()
    for x in &xs { print("{} ", x) }
    println("")
}
"#;
    let dir = fresh_dir("mut_scalar_array_push");
    let path = write_source(&dir, "mut_scalar_array_push", src);
    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();
    let bin = build_native_release(&path, &scratch).expect("release build");
    let out = run_native(&bin);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.2, Some(0), "stderr: {}", out.1);
    assert!(
        out.0.contains("1 2 3 4"),
        "expected push + sort to work on promoted Vec, got: {:?}",
        out.0
    );
}

#[test]
fn sort_by_on_tuple_vec_orders_by_comparator() {
    // Bug fixed in 0.10.0: `xs.sort_by(|a, b| ...)` on a
    // `Vec<(String, i64)>` was a no-op / wrong-order because the
    // closure params `a` / `b` were left `Var` by inference and the
    // lift pass blanket-pinned every unresolved closure param to
    // i64. The lifted comparator body then computed `a.1`'s field
    // offset off a junk integer instead of the element pointer the
    // runtime sort hands it. The lift pass now skips the i64 pin for
    // params used through `TupleIndex` / `Field` / method-call
    // receivers - those are aggregates passed by pointer.
    let src = r#"
fn main() {
    let mut xs: Vec<(String, i64)> = Vec::from([])
    xs.push(("c", 3))
    xs.push(("a", 1))
    xs.push(("b", 2))
    xs.sort_by(|a, b| {
        if a.1 < b.1 { -1 }
        else if a.1 > b.1 { 1 }
        else { 0 }
    })
    for x in &xs {
        println("{}={}", x.0.clone(), x.1)
    }
}
"#;
    let dir = fresh_dir("sort_by_tuple_vec");
    let path = write_source(&dir, "sort_by_tuple_vec", src);
    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();
    let bin = build_native_release(&path, &scratch).expect("release build");
    let out = run_native(&bin);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.2, Some(0), "stderr: {}", out.1);
    let want = "a=1\nb=2\nc=3\n";
    assert!(
        out.0.contains(want),
        "expected ascending order by .1, got: {:?}",
        out.0
    );
}

#[test]
fn vec_of_enum_for_loop_dereferences_slot_pointer() {
    // Bug fixed in 0.10.0: `lower_for_vec` flagged any
    // `TyKind::Adt` element as "inline aggregate" and bound the loop
    // variable to the slot address directly. That's correct for
    // multi-slot user structs (whose inline storage starts at the
    // slot address), but enums and sentinel-handle structs occupy
    // exactly one 8-byte slot that *holds* a heap pointer. The loop
    // body needs the pointer value (one `gos_load` away), not the
    // slot address. Without the load, every `match e { … }` saw the
    // first 8 bytes of the heap allocation interpreted as the
    // pattern scrutinee - and fell through every variant arm.
    let src = r#"
enum Sv {
    SvInt(i64),
    SvText(String),
    SvNull,
}
enum Expr {
    EColumn(String, String),
    ELit(Sv),
}
fn show(e: Expr) {
    match e {
        Expr::EColumn(t, c) => println("Col({}, {})", t.clone(), c.clone()),
        Expr::ELit(v) => match v {
            Sv::SvInt(n) => println("Lit(Int({}))", *n),
            Sv::SvText(s) => println("Lit(Text({}))", s.clone()),
            Sv::SvNull => println("Lit(Null)"),
        },
    }
}
fn main() {
    let mut xs: Vec<Expr> = Vec::from([])
    xs.push(Expr::EColumn("t", "id"))
    xs.push(Expr::ELit(Sv::SvInt(42)))
    xs.push(Expr::ELit(Sv::SvText("hello")))
    for e in &xs { show(e) }
}
"#;
    let dir = fresh_dir("vec_enum_for_loop");
    let path = write_source(&dir, "vec_enum_for_loop", src);
    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();
    let bin = build_native_release(&path, &scratch).expect("release build");
    let out = run_native(&bin);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.2, Some(0), "stderr: {}", out.1);
    assert!(
        out.0.contains("Col(t, id)")
            && out.0.contains("Lit(Int(42))")
            && out.0.contains("Lit(Text(hello))"),
        "expected all three variants printed, got: {:?}",
        out.0
    );
}

#[test]
fn vec_of_multi_slot_struct_round_trips_all_fields() {
    // Bug fixed in 0.10.0: `type_slot_bytes` in MIR returned a flat
    // 8 bytes for every user-defined `Adt`, including multi-field
    // structs. `let xs: Vec<Projection> = []` then created a Vec with
    // `elem_bytes = 8`, so a `push(Projection { a, b })` writing 16
    // bytes of inline storage truncated to the first field. The
    // first iteration of `for p in &xs` re-read garbage for `p.b`
    // and downstream `p.alias.len()` strlen'd a bogus pointer →
    // segfault (atlas_db's exec_project crash).
    let src = r#"
struct Projection {
    a: i64,
    b: i64,
}
fn main() {
    let mut xs: Vec<Projection> = Vec::from([])
    xs.push(Projection { a: 1, b: 2 })
    xs.push(Projection { a: 3, b: 4 })
    for p in &xs {
        println("a={} b={}", p.a, p.b)
    }
}
"#;
    let dir = fresh_dir("vec_multi_slot_struct");
    let path = write_source(&dir, "vec_multi_slot_struct", src);
    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();
    let bin = build_native_release(&path, &scratch).expect("release build");
    let out = run_native(&bin);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.2, Some(0), "stderr: {}", out.1);
    assert!(
        out.0.contains("a=1 b=2") && out.0.contains("a=3 b=4"),
        "expected both fields per element, got: {:?}",
        out.0
    );
}

#[test]
fn regex_captures_all_option_string_match_reads_real_discriminant() {
    // Bug fixed in 0.10.0: `gos_rt_regex_captures_all` / `captures`
    // pushed a bare c-string pointer (or 0) per capture group, but the
    // source type of each group is `Option<String>`. When the element
    // typed as a concrete `Option<String>` (e.g. through a function
    // whose declared return is `Vec<Vec<Option<String>>>`), the compiled-tier
    // `match group { Some(k) => ..., None => ... }` reads the tagged-
    // union discriminant via `gos_rt_result_disc` off the pointer - a
    // raw c-string's first bytes are not a valid discriminant, so the
    // match fell through and printed nothing. Fix: the runtime now
    // pushes canonical `gos_rt_result_new(disc, payload)` Options and
    // the MIR pins the result element to `Option<String>`.
    let src = r#"
use std::regex
fn parse_pairs(line: String) -> Vec<Vec<Option<String>>> {
    let re = match regex::compile("(\\w+)=(\\w+)") { Ok(r) => r, Err(_) => { return Vec::from([]) } }
    regex::captures_all(re, line)
}
fn main() {
    for row in parse_pairs("addr=localhost port=8080") {
        match row[1] {
            Some(k) => match row[2] {
                Some(v) => println("{} = {}", k, v),
                None => {}
            },
            None => {}
        }
    }
}
"#;
    let dir = fresh_dir("regex_captures_option");
    let path = write_source(&dir, "regex_captures_option", src);
    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();
    let bin = build_native_release(&path, &scratch).expect("release build");
    let out = run_native(&bin);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.2, Some(0), "stderr: {}", out.1);
    assert!(
        out.0.contains("addr = localhost") && out.0.contains("port = 8080"),
        "expected both decoded pairs, got: {:?}",
        out.0
    );
}

#[test]
fn hashmap_set_is_an_error_not_a_silent_drop() {
    // `set` is json's field-update helper; a `Map` receiver has
    // `insert`. Routing a Map receiver into the json helper returned
    // the receiver unchanged, so the write vanished without a sound.
    let src = r#"
use std::collections::Map
fn main() {
    let mut m = Map::new()
    m.insert("a", 1)
    m.set("a", 7)
    println("{:?}", m.get("a"))
}
"#;
    let dir = fresh_dir("hashmap_set_rejected");
    let path = write_source(&dir, "hashmap_set_rejected", src);
    let out = run_vm(&path);
    let _ = std::fs::remove_dir_all(&dir);
    assert_ne!(out.2, Some(0), "expected failure, stdout: {:?}", out.0);
    assert!(
        out.1.contains("insert"),
        "error should point at `insert`, got: {}",
        out.1
    );
}

#[test]
fn json_value_set_updates_objects_and_passes_leaves_through() {
    let src = r#"
use std::encoding::json
fn main() -> Result<(), String> {
    let v = json::parse("{\"a\": 1}").map_err(|e| format("{e}"))?
    let v2 = v.set("b", 2)
    println("{}", json::render(v2))
    let leaf = json::parse("3").map_err(|e| format("{e}"))?
    println("{}", json::render(leaf.set("x", 1)))
    Ok(())
}
"#;
    let dir = fresh_dir("json_set_objects");
    let path = write_source(&dir, "json_set_objects", src);
    let out = run_vm(&path);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.2, Some(0), "stderr: {}", out.1);
    assert!(
        out.0.contains("\"b\":2") || out.0.contains("\"b\": 2"),
        "object update lost: {:?}",
        out.0
    );
    assert!(
        out.0.lines().nth(1) == Some("3"),
        "leaf pass-through: {:?}",
        out.0
    );
}

#[test]
fn sync_qualified_waitgroup_constructor_resolves() {
    // The native tiers accept both `WaitGroup::new()` and
    // `sync::WaitGroup::new()`; the VM must bind the qualified
    // spelling too.
    let src = r#"
use std::sync
fn main() {
    let wg = sync::WaitGroup::new()
    wg.add(1)
    spawn(|| finish(wg))
    wg.wait()
    println("done")
}
fn finish(wg: sync::WaitGroup) { wg.done() }
"#;
    let dir = fresh_dir("sync_waitgroup_qualified");
    let path = write_source(&dir, "sync_waitgroup_qualified", src);
    let out = run_vm(&path);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.2, Some(0), "stderr: {}", out.1);
    assert_eq!(out.0.trim(), "done", "stdout: {:?}", out.0);
}

#[test]
fn option_result_chain_methods_match_across_tiers() {
    // and_then / or_else / filter / ok_or / ok_or_else in method form
    // on Option and Result receivers, VM output == native output.
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../feature-testing-examples/option_result_chain_methods.gos"),
    )
    .expect("read fixture");
    let dir = fresh_dir("option_result_chain_methods");
    let path = write_source(&dir, "option_result_chain_methods", &src);
    let vm = run_vm(&path);
    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    let expected = "Some(10)\nNone\nNone\nSome(5)\nNone\nSome(9)\nSome(1)\nOk(7)\nErr(\"computed\")\nOk(5)\nErr(\"boom\")\nOk(4)\n";
    assert_eq!(vm.0, expected, "vm output drift");
    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();
    let bin = build_native(&path, &scratch).expect("native build");
    let native = run_native(&bin);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert_eq!(native.0, expected, "native output drift");
}

#[test]
fn process_spawn_piped_round_trips_across_tiers() {
    // spawn_piped + write_stdin + close_stdin + read_line + wait,
    // VM output == native output.
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../feature-testing-examples/process_spawn_piped.gos"),
    )
    .expect("read fixture");
    let dir = fresh_dir("process_spawn_piped");
    let path = write_source(&dir, "process_spawn_piped", &src);
    let expected = "line: apple\nline: mango\nline: pear\nexit: 0\ntrue #[\"a\", \"b\"] true\n";
    let vm = run_vm(&path);
    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(vm.0, expected, "vm output drift");
    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();
    let bin = build_native(&path, &scratch).expect("native build");
    let native = run_native(&bin);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert_eq!(native.0, expected, "native output drift");
}

#[test]
fn vec_insert_result_and_receiver_match_across_tiers() {
    let src = r#"
fn main() {
    let mut values: Vec<i64> = Vec::from([1, 2, 3])
    println("{}", values.insert(1, 9).is_ok())
    println("{} {} {} {}", values[0], values[1], values[2], values[3])
    println("{}", values.insert(99, 8).is_err())
    println("{} {} {} {}", values[0], values[1], values[2], values[3])
    values.swap(0, 3)
    println("{} {} {} {}", values[0], values[1], values[2], values[3])
    println("{}", values.remove(1).unwrap_or(-1))
    println("{}", values.remove(99).is_err())
}
"#;
    let dir = fresh_dir("vec_insert_result");
    let path = write_source(&dir, "vec_insert_result", src);
    let expected = "true\n1 9 2 3\ntrue\n1 9 2 3\n3 9 2 1\n9\ntrue\n";

    let vm = run_vm(&path);
    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(vm.0, expected, "vm output drift");

    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();
    let bin = build_native(&path, &scratch).expect("native build");
    let native = run_native(&bin);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert_eq!(native.0, expected, "native output drift");
}

#[test]
fn hashmap_insert_option_matches_across_tiers() {
    let src = r#"
fn main() {
	    let mut values: Map<i64, i64> = Map::new()
    println("{}", values.insert(4, 10).is_none())
    println("{}", values.insert(4, 12).unwrap_or(-1))
    println("{}", values.get(4).unwrap_or(-1))
    println("{}", values.remove(4).unwrap_or(-1))
    println("{}", values.remove(4).is_none())
    let mut words: Map<String, String> = Map::new()
    println("{}", words.insert("key", "first").is_none())
    println("{}", words.insert("key", "second").unwrap_or("missing"))
    println("{}", words.get("key").unwrap_or("missing"))
}
"#;
    let dir = fresh_dir("hashmap_insert_option");
    let path = write_source(&dir, "hashmap_insert_option", src);
    let expected = "true\n10\n12\n12\ntrue\ntrue\nfirst\nsecond\n";

    let vm = run_vm(&path);
    assert_eq!(vm.2, Some(0), "vm stderr: {}", vm.1);
    assert_eq!(vm.0, expected, "vm output drift");

    let scratch = dir.join("bin");
    std::fs::create_dir_all(&scratch).unwrap();
    let bin = build_native(&path, &scratch).expect("native build");
    let native = run_native(&bin);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(native.2, Some(0), "native stderr: {}", native.1);
    assert_eq!(native.0, expected, "native output drift");
}
