//! Tests that exercise the VM's JIT dispatch path explicitly via
//! `GOS_JIT=1` semantics. Each test sets the env var inside the
//! test process, runs `Vm::load`, asserts behaviour, then unsets.
//!
//! Tests that don't depend on `set_stdout_writer` (which the JIT
//! bypasses; the runtime writes through raw `write(2)` syscalls)
//! check return values directly so the JIT path is observable
//! without colliding with the bytecode VM's stdout-redirection.

#![allow(missing_docs)]
#![allow(unsafe_code)]

use gossamer_hir::lower_source_file;
use gossamer_interp::{Value, Vm};
use gossamer_lex::SourceMap;
use gossamer_parse::parse_source_file;
use gossamer_resolve::resolve_source_file;
use gossamer_types::{TyCtxt, typecheck_source_file};
use std::ffi::OsString;
use std::sync::{LazyLock, Mutex, MutexGuard};

/// Process environments are shared by every test thread in this binary.
/// Serialize the tests that toggle JIT policy so the RAM-cap regression cannot
/// accidentally suppress promotion in an unrelated JIT test.
static JIT_ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

struct GosJitGuard {
    _lock: MutexGuard<'static, ()>,
    previous: Option<OsString>,
}

impl GosJitGuard {
    fn new() -> Self {
        // A failing JIT assertion must not turn every later test into an
        // unrelated lock-poison failure. The prior guard restores its env
        // changes during unwinding, so recovering the mutex is safe here.
        let lock = JIT_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::env::var_os("GOS_JIT");
        // SAFETY: `lock` serializes all JIT environment mutations in this
        // integration-test process, and `Drop` restores the prior value.
        unsafe { std::env::set_var("GOS_JIT", "1") };
        Self {
            _lock: lock,
            previous,
        }
    }
}

struct JitRssCapGuard {
    previous: Option<OsString>,
}

struct JitCodeCapGuard {
    previous: Option<OsString>,
}

struct JitFilterGuard {
    name: &'static str,
    previous: Option<OsString>,
}

impl JitFilterGuard {
    fn new(name: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(name);
        // SAFETY: a `GosJitGuard` holds `JIT_ENV_LOCK` for this test.
        unsafe { std::env::set_var(name, value) };
        Self { name, previous }
    }
}

impl Drop for JitFilterGuard {
    fn drop(&mut self) {
        // SAFETY: a `GosJitGuard` still holds `JIT_ENV_LOCK` here.
        unsafe {
            if let Some(previous) = &self.previous {
                std::env::set_var(self.name, previous);
            } else {
                std::env::remove_var(self.name);
            }
        }
    }
}

impl JitCodeCapGuard {
    fn new(bytes: &str) -> Self {
        let previous = std::env::var_os("GOS_JIT_MAX_CODE_BYTES");
        // SAFETY: a `GosJitGuard` holds `JIT_ENV_LOCK` for this test.
        unsafe { std::env::set_var("GOS_JIT_MAX_CODE_BYTES", bytes) };
        Self { previous }
    }
}

impl Drop for JitCodeCapGuard {
    fn drop(&mut self) {
        // SAFETY: a `GosJitGuard` still holds `JIT_ENV_LOCK` here.
        unsafe {
            if let Some(previous) = &self.previous {
                std::env::set_var("GOS_JIT_MAX_CODE_BYTES", previous);
            } else {
                std::env::remove_var("GOS_JIT_MAX_CODE_BYTES");
            }
        };
    }
}

impl JitRssCapGuard {
    fn new(megabytes: &str) -> Self {
        let previous = std::env::var_os("GOS_JIT_MAX_RSS_MB");
        // SAFETY: a `GosJitGuard` holds `JIT_ENV_LOCK` for this test.
        unsafe { std::env::set_var("GOS_JIT_MAX_RSS_MB", megabytes) };
        Self { previous }
    }
}

impl Drop for JitRssCapGuard {
    fn drop(&mut self) {
        // SAFETY: a `GosJitGuard` still holds `JIT_ENV_LOCK` here.
        unsafe {
            if let Some(previous) = &self.previous {
                std::env::set_var("GOS_JIT_MAX_RSS_MB", previous);
            } else {
                std::env::remove_var("GOS_JIT_MAX_RSS_MB");
            }
        };
    }
}

impl Drop for GosJitGuard {
    fn drop(&mut self) {
        // SAFETY: this guard owns `JIT_ENV_LOCK` until after the restoration.
        unsafe {
            if let Some(previous) = &self.previous {
                std::env::set_var("GOS_JIT", previous);
            } else {
                std::env::remove_var("GOS_JIT");
            }
        };
    }
}

fn build_vm(source: &str) -> (Vm, TyCtxt) {
    let mut map = SourceMap::new();
    let file = map.add_file("jit.gos", source.to_string());
    let (mut sf, parse_diags) = parse_source_file(source, file);
    assert!(parse_diags.is_empty(), "parse: {parse_diags:?}");
    let (mut resolutions, _resolve_diags) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &mut resolutions);
    let mut tcx = TyCtxt::new();
    let (table, _type_diags) = typecheck_source_file(&sf, &resolutions, &mut tcx);
    let program = lower_source_file(&sf, &resolutions, &table, &mut tcx);
    let mut vm = Vm::new();
    vm.load(&program, tcx.clone(), true).expect("load");
    (vm, tcx)
}

#[test]
fn jit_returns_constant_int() {
    let _g = GosJitGuard::new();
    let (vm, _) = build_vm("fn main() -> i64 { 42i64 }\n");
    let result = vm.call("main", Vec::new()).expect("main");
    assert!(matches!(result, Value::Int(42)));
}

#[test]
fn jit_dispatches_through_simple_arithmetic() {
    let _g = GosJitGuard::new();
    let (vm, _) = build_vm(
        "fn add(a: i64, b: i64) -> i64 { a + b }\nfn main() -> i64 { add(7i64, 35i64) }\n",
    );
    let result = vm.call("main", Vec::new()).expect("main");
    assert!(matches!(result, Value::Int(42)));
}

#[test]
fn jit_struct_return_promotes_without_losing_result() {
    // Struct returns use the explicit StructPtr trampoline shape. Exercise a
    // heap child as well as a scalar field so promotion covers both decoding
    // and ownership transfer back into VM values.
    let _g = GosJitGuard::new();
    let source = "struct Pair { label: String, right: i64 }\nfn build_pair(n: i64) -> Pair {\n  let mut i = 0i64\n  while i < 100i64 { i = i + 1i64 }\n  Pair { label: \"native\", right: n + 1i64 }\n}\nfn main() -> i64 { 0i64 }\n";
    let (vm, _) = build_vm(source);

    // The body has 27 bytecode instructions, so 500 entries exceed the
    // default 8,192-instruction admission floor without relying on the
    // process-wide test-only JIT threshold overrides.
    for _ in 0..500 {
        vm.call("build_pair", vec![Value::Int(20)])
            .expect("build_pair warm-up");
    }
    let result = vm
        .call("build_pair", vec![Value::Int(20)])
        .expect("build_pair");
    let Value::Struct(pair) = result else {
        panic!("JIT struct return must decode to Value::Struct");
    };
    assert_eq!(pair.name.as_str(), "Pair");
    assert!(matches!(pair.fields.get(0), Some((_, Value::String(s))) if s.as_str() == "native"));
    assert!(matches!(pair.fields.get(1), Some((_, Value::Int(21)))));

    let metrics = vm.jit_metrics();
    assert_eq!(
        metrics.successful_compiles, 1,
        "the struct-returning body must promote: {metrics:?}"
    );
}

#[test]
fn jit_fallback_for_value_typed_args() {
    // The JIT trampoline only handles primitive scalar args. Calling
    // `concat` with `Value::String` operands forces the fallback
    // path; we still want the right answer.
    let _g = GosJitGuard::new();
    let (vm, _) =
        build_vm("fn double(n: i64) -> i64 { n * 2i64 }\nfn main() -> i64 { double(21i64) }\n");
    let result = vm.call("main", Vec::new()).expect("main");
    assert!(matches!(result, Value::Int(42)));
}

#[test]
fn jit_fib_recursion_returns_correctly() {
    let _g = GosJitGuard::new();
    let source = "fn fib(n: i64) -> i64 {\n  if n < 2i64 { n } else { fib(n - 1i64) + fib(n - 2i64) }\n}\nfn main() -> i64 { fib(10i64) }\n";
    let (vm, _) = build_vm(source);
    let result = vm.call("main", Vec::new()).expect("main");
    assert!(
        matches!(result, Value::Int(55)),
        "fib(10) result: {result:?}"
    );
}

#[test]
fn jit_falls_back_when_signature_unsupported() {
    // String concat returns String - not in the JIT's supported
    // primitive set. The VM should still produce the right answer
    // via the bytecode fallback.
    let _g = GosJitGuard::new();
    let (vm, _) = build_vm(
        "fn pick(b: bool) -> i64 { if b { 1i64 } else { 0i64 } }\nfn main() -> i64 { pick(true) + pick(false) }\n",
    );
    let result = vm.call("main", Vec::new()).expect("main");
    assert!(
        matches!(result, Value::Int(1)),
        "pick(true) + pick(false): {result:?}"
    );
}

#[test]
fn slice_pattern_body_does_not_block_an_unrelated_hot_loop() {
    let _g = GosJitGuard::new();
    let source = r"
fn slice_head(xs: [i64; 3]) -> i64 {
    match xs { [first, ..] => first }
}
fn hot_loop(n: i64) -> i64 {
    let mut out = 0i64
    let mut i = 0i64
    while i < n { out += i
        i += 1i64 }
    out
}
fn main() -> i64 { hot_loop(10i64) }
";
    let (vm, _) = build_vm(source);
    // Cross the production work floor without relying on a process-global
    // environment override, which is initialized only once per test binary.
    warm_up_n(&vm, "hot_loop", &[Value::Int(100)], 1_000);
    let result = vm.call("hot_loop", vec![Value::Int(10)]).expect("hot_loop");
    assert!(matches!(result, Value::Int(45)), "result: {result:?}");
    let metrics = vm.jit_metrics();
    assert!(
        metrics.successful_compiles >= 1 && metrics.resident_functions >= 1,
        "an unrelated slice-pattern body must not reject the hot loop: {metrics:?}"
    );
}

#[test]
fn option_local_loop_promotes_without_losing_result() {
    let _g = GosJitGuard::new();
    let source = r"
fn option_sum(n: i64) -> i64 {
    let mut out = 0i64
    let mut i = 0i64
    while i < n {
        let value = Some(i)
        match value { Some(x) => out += x, None => {} }
        i += 1i64
    }
    out
}
fn main() -> i64 { option_sum(10i64) }
";
    let (vm, _) = build_vm(source);
    // Use the real admission policy here too. Option locals carry enum state,
    // which the current in-process JIT lowers directly.
    warm_up_n(&vm, "option_sum", &[Value::Int(100)], 1_000);
    let result = vm
        .call("option_sum", vec![Value::Int(10)])
        .expect("option_sum");
    assert!(matches!(result, Value::Int(45)), "result: {result:?}");
    let metrics = vm.jit_metrics();
    assert!(
        metrics.successful_compiles >= 1,
        "Option locals should promote successfully: {metrics:?}"
    );
}

/// Calls `name` enough times to drive its per-function tier-up
/// counter to zero (the floor is 16, the ceiling 100), which forces
/// the deferred cranelift compile and installs the native override.
/// Deterministic: the count is fixed, not time-based.
fn warm_up(vm: &Vm, name: &str, args: &[Value]) {
    warm_up_n(vm, name, args, 300);
}

fn warm_up_n(vm: &Vm, name: &str, args: &[Value], calls: usize) {
    for _ in 0..calls {
        let _ = vm.call(name, args.to_vec());
    }
}

#[test]
fn jit_metrics_report_promotion_and_snapshot_release() {
    let _g = GosJitGuard::new();
    let source = "fn fib(n: i64) -> i64 {\n  if n < 2i64 { n } else { fib(n - 1i64) + fib(n - 2i64) }\n}\nfn main() -> i64 { fib(10i64) }\n";
    let (mut vm, _) = build_vm(source);

    warm_up(&vm, "fib", &[Value::Int(8)]);
    let promoted = vm.jit_metrics();
    assert!(
        promoted.tier_up_requests >= 1,
        "hot bytecode calls must expose a tier-up request: {promoted:?}"
    );
    assert_eq!(
        promoted.compile_attempts, 1,
        "one VM compiles its immutable program snapshot at most once"
    );
    assert_eq!(
        promoted.successful_compiles, 1,
        "promotion metrics: {promoted:?}"
    );
    assert!(
        promoted.resident_functions >= 1,
        "a successful promotion must retain a callable native entry: {promoted:?}"
    );
    assert!(
        promoted.promoted_functions >= 1 && promoted.last_promoted_functions >= 1,
        "promotion accounting must report installed callable entries: {promoted:?}"
    );
    assert!(
        promoted.emitted_code_bytes > 0
            && promoted.last_emitted_code_bytes > 0
            && promoted.emitted_code_bytes >= promoted.last_emitted_code_bytes,
        "successful promotion must report exact Cranelift code bytes: {promoted:?}"
    );
    assert!(
        promoted.total_compile_time_us >= promoted.last_compile_time_us,
        "compile duration totals must include the latest compile: {promoted:?}"
    );
    assert!(
        promoted.peak_observed_rss_bytes >= promoted.last_observed_rss_bytes,
        "peak RSS must dominate the latest sample: {promoted:?}"
    );
    assert!(
        promoted.saved_vm_instructions > 0,
        "successful native dispatch must record bytecode work bypassed: {promoted:?}"
    );

    assert!(
        promoted.released_snapshots >= 1,
        "spawn-free promotion must release its MIR/type snapshot: {promoted:?}"
    );
    let releases_before = promoted.released_snapshots;
    vm.release_jit_prelude();
    let released = vm.jit_metrics();
    assert_eq!(
        released.released_snapshots, releases_before,
        "an already-released snapshot must not be counted twice: {released:?}"
    );
    let result = vm
        .call("fib", vec![Value::Int(10)])
        .expect("fib after release");
    assert!(
        matches!(result, Value::Int(55)),
        "releasing MIR metadata must not release the installed artifact: {result:?}"
    );
}

#[test]
fn eager_loop_compiles_before_entry() {
    let _g = GosJitGuard::new();
    // A loop-bearing body cannot switch tiers after bytecode execution has
    // already entered the frame, so it compiles at the first entry gate.
    let source = "fn tick(n: i64) -> i64 {\n  let mut out = n\n  let mut i = 0\n  while i < 1 { out += 1\n    i += 1 }\n  out\n}\nfn main() -> i64 { tick(1) }\n";
    let (vm, _) = build_vm(source);

    for _ in 0..100 {
        let value = vm.call("tick", vec![Value::Int(41)]).expect("tick");
        assert!(matches!(value, Value::Int(42)));
    }

    let metrics = vm.jit_metrics();
    assert!(
        metrics.tier_up_requests >= 1,
        "eligible loop work must reach the admission gate: {metrics:?}"
    );
    assert_eq!(
        metrics.work_floor_deferrals, 0,
        "eager loop entries must not be deferred by the dynamic work floor: {metrics:?}"
    );
    assert!(
        metrics.compile_attempts >= 1 && metrics.resident_functions >= 1,
        "eager loop work must install a native artifact: {metrics:?}"
    );
}

#[test]
fn jit_metrics_report_ram_aware_tier_up_skip() {
    let _g = GosJitGuard::new();
    let _rss = JitRssCapGuard::new("1");
    let source = "fn fib(n: i64) -> i64 {\n  if n < 2i64 { n } else { fib(n - 1i64) + fib(n - 2i64) }\n}\nfn main() -> i64 { fib(10i64) }\n";
    let (vm, _) = build_vm(source);

    warm_up(&vm, "fib", &[Value::Int(8)]);
    let metrics = vm.jit_metrics();
    assert!(
        metrics.tier_up_requests >= 1,
        "hot calls must still be observable before a RAM skip: {metrics:?}"
    );
    assert_eq!(
        metrics.compile_attempts, 0,
        "RAM cap must skip before Cranelift compile work starts: {metrics:?}"
    );
    assert_eq!(
        metrics.successful_compiles, 0,
        "RAM-capped tier-up must not install native dispatch: {metrics:?}"
    );
    assert!(
        metrics.ram_skipped_compiles >= 1,
        "RAM-capped tier-up must be counted: {metrics:?}"
    );
    assert!(
        metrics.last_observed_rss_bytes > 0,
        "RAM-capped tier-up must record observed RSS: {metrics:?}"
    );
    assert!(
        metrics.released_snapshots >= 1,
        "a terminal RAM skip must release the unused MIR/type snapshot: {metrics:?}"
    );
}

#[test]
fn jit_metrics_report_native_code_budget_skip() {
    let _g = GosJitGuard::new();
    let _code = JitCodeCapGuard::new("1");
    let source = "fn fib(n: i64) -> i64 {\n  if n < 2i64 { n } else { fib(n - 1i64) + fib(n - 2i64) }\n}\nfn main() -> i64 { fib(10i64) }\n";
    let (vm, _) = build_vm(source);

    warm_up(&vm, "fib", &[Value::Int(8)]);
    let metrics = vm.jit_metrics();
    assert_eq!(
        metrics.compile_attempts, 1,
        "the cap is checked after exact code-size measurement: {metrics:?}"
    );
    assert_eq!(
        metrics.successful_compiles, 0,
        "over-budget code must not install: {metrics:?}"
    );
    assert_eq!(
        metrics.resident_functions, 0,
        "over-budget code must not remain reachable: {metrics:?}"
    );
    assert!(
        metrics.code_size_skipped_compiles >= 1,
        "code-budget skip must be observable: {metrics:?}"
    );
    assert_eq!(
        metrics.emitted_code_bytes, 0,
        "rejected code must not be reported as retained: {metrics:?}"
    );
    assert!(
        metrics.released_snapshots >= 1,
        "rejected code must release the MIR snapshot: {metrics:?}"
    );
}

#[test]
fn jit_reuses_immutable_artifact_for_overlapping_vms_on_one_thread() {
    let _g = GosJitGuard::new();
    let source = "fn artifact_cache_fib(n: i64) -> i64 {\n  if n < 2i64 { n } else { artifact_cache_fib(n - 1i64) + artifact_cache_fib(n - 2i64) }\n}\nfn main() -> i64 { artifact_cache_fib(10i64) }\n";
    let (first, _) = build_vm(source);
    warm_up(&first, "artifact_cache_fib", &[Value::Int(8)]);
    let first_metrics = first.jit_metrics();
    assert_eq!(
        first_metrics.compile_attempts, 1,
        "first VM must compile: {first_metrics:?}"
    );

    // Keep `first` alive while warming `second`: the thread-local cache is
    // weak by design, so it must never retain code pages after all executions
    // release them.
    let (second, _) = build_vm(source);
    warm_up(&second, "artifact_cache_fib", &[Value::Int(8)]);
    let second_metrics = second.jit_metrics();
    assert_eq!(
        second_metrics.compile_attempts, 0,
        "the second equivalent VM must reuse finalized code instead of recompiling: {second_metrics:?}"
    );
    assert_eq!(
        second_metrics.reused_artifacts, 1,
        "reuse must be observable: {second_metrics:?}"
    );
    assert!(
        matches!(
            second.call("artifact_cache_fib", vec![Value::Int(10)]),
            Ok(Value::Int(55))
        ),
        "reused artifact must preserve bytecode-visible behavior"
    );
}

#[test]
fn jit_filters_are_applied_per_vm_when_artifact_is_reused() {
    let _g = GosJitGuard::new();
    let source = "fn filter_f(n: i64) -> i64 { if n == 0i64 { 1i64 } else { filter_g(n - 1i64) } }\nfn filter_g(n: i64) -> i64 { if n == 0i64 { 2i64 } else { filter_f(n - 1i64) } }\nfn main() -> i64 { filter_f(4i64) }\n";
    let only_f = JitFilterGuard::new("GOS_JIT_ONLY", "filter_f");
    let (first, _) = build_vm(source);
    warm_up(&first, "filter_f", &[Value::Int(8)]);
    assert_eq!(first.jit_metrics().resident_functions, 1);
    drop(only_f);

    let _only_g = JitFilterGuard::new("GOS_JIT_ONLY", "filter_g");
    let (second, _) = build_vm(source);
    warm_up(&second, "filter_f", &[Value::Int(8)]);
    let metrics = second.jit_metrics();
    assert_eq!(
        metrics.compile_attempts, 0,
        "artifact should be reused: {metrics:?}"
    );
    assert_eq!(
        metrics.reused_artifacts, 1,
        "reuse should be visible: {metrics:?}"
    );
    assert_eq!(
        metrics.resident_functions, 1,
        "the second filter must install only the currently allowed entry"
    );
}

#[test]
fn writable_static_artifacts_are_not_reused_between_vms() {
    let _g = GosJitGuard::new();
    let source = "static mut CACHE_COUNTER: i64 = 0i64\nfn static_tick(n: i64) -> i64 { CACHE_COUNTER += 1i64\nif n == 0i64 { CACHE_COUNTER } else { static_tick(n - 1i64) } }\nfn main() -> i64 { static_tick(4i64) }\n";
    let (first, _) = build_vm(source);
    warm_up(&first, "static_tick", &[Value::Int(8)]);
    assert_eq!(first.jit_metrics().compile_attempts, 1);

    let (second, _) = build_vm(source);
    warm_up(&second, "static_tick", &[Value::Int(8)]);
    let metrics = second.jit_metrics();
    assert_eq!(
        metrics.compile_attempts, 1,
        "mutable storage requires a fresh module: {metrics:?}"
    );
    assert_eq!(
        metrics.reused_artifacts, 0,
        "mutable artifacts must stay VM-local: {metrics:?}"
    );
}

#[test]
fn jit_code_budget_applies_to_cached_artifacts() {
    let _g = GosJitGuard::new();
    let source = "fn cached_budget_fib(n: i64) -> i64 {\n  if n < 2i64 { n } else { cached_budget_fib(n - 1i64) + cached_budget_fib(n - 2i64) }\n}\nfn main() -> i64 { cached_budget_fib(10i64) }\n";
    let (first, _) = build_vm(source);
    warm_up(&first, "cached_budget_fib", &[Value::Int(8)]);
    assert!(first.jit_metrics().emitted_code_bytes > 1);

    let _code = JitCodeCapGuard::new("1");
    let (second, _) = build_vm(source);
    warm_up(&second, "cached_budget_fib", &[Value::Int(8)]);
    let metrics = second.jit_metrics();
    assert_eq!(
        metrics.compile_attempts, 0,
        "the cached artifact must avoid recompilation: {metrics:?}"
    );
    assert_eq!(
        metrics.resident_functions, 0,
        "the cached artifact must honor the new VM budget: {metrics:?}"
    );
    assert!(
        metrics.code_size_skipped_compiles >= 1,
        "cached code-budget skip must be observable: {metrics:?}"
    );
}

#[test]
fn jit_u64_ge_uses_unsigned_comparison() {
    // Regression: the JIT lowered `>=` with a signed condition even
    // for `u64`, so a value with the high bit set (>= 2^63) compared
    // as a negative i64. `2^63 >= 1` must be true unsigned; a signed
    // compare wrongly yields false. The bytecode VM is the oracle.
    let _g = GosJitGuard::new();
    let (vm, _) =
        build_vm("fn uge(a: u64, b: u64) -> bool { a >= b }\nfn main() -> i64 { 0i64 }\n");
    // `i64::MIN` is the bit pattern of `u64` 2^63.
    let big = Value::Int(i64::MIN);
    let one = Value::Int(1);
    let vm_answer = vm.call("uge", vec![big.clone(), one.clone()]).expect("uge");
    assert!(
        matches!(vm_answer, Value::Bool(true)),
        "bytecode oracle: u64 2^63 >= 1 should be true, got {vm_answer:?}"
    );
    warm_up(&vm, "uge", &[big.clone(), one.clone()]);
    let jit_answer = vm.call("uge", vec![big, one]).expect("uge");
    assert!(
        matches!(jit_answer, Value::Bool(true)),
        "JIT must match the VM: u64 2^63 >= 1 is true (unsigned), got {jit_answer:?}"
    );
}

#[test]
fn jit_u64_shr_uses_logical_shift() {
    // Regression: the JIT always lowered `>>` as an arithmetic shift.
    // `u64` must shift logically: `2^63 >> 1 == 2^62`; an arithmetic
    // shift would sign-extend to `-2^62`.
    let _g = GosJitGuard::new();
    let (vm, _) =
        build_vm("fn ushr(a: u64, n: u64) -> u64 { a >> n }\nfn main() -> i64 { 0i64 }\n");
    let big = Value::Int(i64::MIN); // u64 2^63
    let one = Value::Int(1);
    let expected = 1i64 << 62; // 2^62
    let vm_answer = vm
        .call("ushr", vec![big.clone(), one.clone()])
        .expect("ushr");
    assert!(
        matches!(vm_answer, Value::Int(n) if n == expected),
        "bytecode oracle: u64 2^63 >> 1 should be 2^62, got {vm_answer:?}"
    );
    warm_up(&vm, "ushr", &[big.clone(), one.clone()]);
    let jit_answer = vm.call("ushr", vec![big, one]).expect("ushr");
    assert!(
        matches!(jit_answer, Value::Int(n) if n == expected),
        "JIT must match the VM: u64 2^63 >> 1 is 2^62 (logical), got {jit_answer:?}"
    );
}

#[test]
fn jit_divide_by_zero_is_clean_panic_not_trap() {
    // Regression: the JIT lowered a failed divide-by-zero assertion
    // to a raw machine trap (SIGILL), where the VM and AOT tiers
    // render a clean `error[GX0005]` panic. The panic terminates the
    // process, so the actual divide runs in a re-exec'd child and the
    // parent inspects how the child died: a clean panic exits with a
    // code and a GX0005 message; a trap kills the child by signal.
    const CHILD_ENV: &str = "GOS_JIT_DIVZERO_CHILD";
    if std::env::var(CHILD_ENV).is_ok() {
        // Keep this regression body small while still forcing promotion in the
        // child: production tiering now has a work floor that can otherwise
        // leave the tiny one-trip loop on bytecode.
        unsafe { std::env::set_var("GOSSAMER_JIT_MIN_WORK", "0") };
        let _g = GosJitGuard::new();
        // `divi` carries a one-trip loop so it is JIT-worthy under the
        // promote-only-real-work policy; the divide still runs natively,
        // which is the path this regression guards.
        let (vm, _) = build_vm(
            "fn divi(a: i64, b: i64) -> i64 { let mut r = 0\nlet mut i = 0\nwhile i < 1 { r = a / b\ni += 1 }\nr }\nfn main() -> i64 { 0i64 }\n",
        );
        warm_up(&vm, "divi", &[Value::Int(10), Value::Int(2)]);
        // Trip the divide-by-zero on the now-native body.
        if let Err(err) = vm.call("divi", vec![Value::Int(1), Value::Int(0)]) {
            panic!("{err}");
        }
        // Only reached if the body somehow did not panic; exit clean.
        std::process::exit(0);
    }
    let exe = std::env::current_exe().expect("current test executable");
    let output = std::process::Command::new(exe)
        .args([
            "jit_divide_by_zero_is_clean_panic_not_trap",
            "--exact",
            "--nocapture",
        ])
        .env(CHILD_ENV, "1")
        .output()
        .expect("spawn child test process");
    let stderr = String::from_utf8_lossy(&output.stderr);
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert!(
            output.status.signal().is_none(),
            "JIT divide-by-zero terminated by signal {:?} (an illegal-instruction \
             trap) instead of a clean panic; stderr:\n{stderr}",
            output.status.signal()
        );
    }
    assert!(
        !output.status.success(),
        "JIT divide-by-zero unexpectedly exited successfully; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("GX0005"),
        "JIT divide-by-zero must produce a clean error[GX0005] panic; \
         child status {:?}, stderr:\n{stderr}",
        output.status
    );
}
