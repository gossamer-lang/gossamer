fn install_sigint_handler(flag: Arc<AtomicBool>) {
    if SIGINT_HOOKED.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::Builder::new()
        .name("gossamer-http-sigint".to_string())
        .spawn(move || {
            let _ = flag;
        })
        .ok();
}

/// Extracts a borrowed string slice from a Gossamer value, returning
/// `None` when the value is not a string.
pub(crate) fn as_str(value: &Value) -> Option<&str> {
    match value {
        Value::String(s) => Some(s.as_str()),
        _ => None,
    }
}

/// Builds a `Result::Ok(value)` Gossamer variant.
pub(crate) fn ok_variant(value: Value) -> Value {
    Value::variant("Ok", vec![value])
}

/// Builds a `Result::Err(message)` Gossamer variant carrying a string.
pub(crate) fn err_variant(message: impl Into<String>) -> Value {
    Value::variant("Err", vec![errors_struct(message.into(), Value::Unit)])
}

/// Builds a `Option::Some(value)` Gossamer variant.
pub(crate) fn some_variant(value: Value) -> Value {
    Value::variant("Some", vec![value])
}

/// Builds a `Option::None` Gossamer variant.
pub(crate) fn none_variant() -> Value {
    Value::variant("None", Vec::new())
}

fn builtin_os_args(_args: &[Value]) -> RuntimeResult<Value> {
    let argv: Vec<Value> = program_args()
        .into_iter()
        .map(|s| Value::String(s.into()))
        .collect();
    Ok(Value::Array(Arc::new(argv)))
}

fn builtin_os_program_name(_args: &[Value]) -> RuntimeResult<Value> {
    let name = program_name();
    Ok(Value::String(name.into()))
}

fn builtin_os_env(args: &[Value]) -> RuntimeResult<Value> {
    let name = args.first().and_then(as_str).unwrap_or("");
    match os_std::env(name) {
        Some(value) => Ok(some_variant(Value::String(value.into()))),
        None => Ok(none_variant()),
    }
}

fn builtin_env_set_var(args: &[Value]) -> RuntimeResult<Value> {
    let name = args.first().and_then(as_str).unwrap_or("");
    let value = args.get(1).and_then(as_str).unwrap_or("");
    match env_std::set_var(name, value) {
        Ok(()) => Ok(ok_variant(Value::Unit)),
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

fn builtin_env_unset_var(args: &[Value]) -> RuntimeResult<Value> {
    let name = args.first().and_then(as_str).unwrap_or("");
    env_std::unset_var(name);
    Ok(Value::Unit)
}

fn builtin_env_set_current_dir(args: &[Value]) -> RuntimeResult<Value> {
    let path = args.first().and_then(as_str).unwrap_or("");
    match env_std::set_current_dir(path) {
        Ok(()) => Ok(ok_variant(Value::Unit)),
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

fn builtin_env_home_dir(_args: &[Value]) -> RuntimeResult<Value> {
    match env_std::home_dir() {
        Some(p) => Ok(some_variant(Value::String(p.into()))),
        None => Ok(none_variant()),
    }
}

fn builtin_env_temp_dir(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::String(env_std::temp_dir().into()))
}

fn builtin_env_vars(_args: &[Value]) -> RuntimeResult<Value> {
    let pairs = env_std::vars();
    let mut storage = crate::value::dense_map_with_capacity(pairs.len());
    for (name, value) in pairs {
        storage.insert(
            crate::value::MapKey::Str(SmolStr::from(name)),
            Value::String(SmolStr::from(value)),
        );
    }
    Ok(Value::Map(std::sync::Arc::new(parking_lot::Mutex::new(storage.into()))))
}

fn builtin_process_id(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Int(i64::from(gossamer_runtime::platform::process_id())))
}

fn builtin_process_abort(_args: &[Value]) -> RuntimeResult<Value> {
    std::process::abort();
}

fn builtin_os_family(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::String(os_std::family().into()))
}

fn builtin_os_arch(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::String(os_std::arch().into()))
}

fn builtin_os_exit(args: &[Value]) -> RuntimeResult<Value> {
    let code = i32::try_from(args.first().and_then(value_to_int).unwrap_or(0)).unwrap_or(0);
    if gossamer_runtime::platform::CAN_END_PROCESS {
        std::process::exit(code);
    }
    Err(RuntimeError::Exit(code))
}

fn builtin_os_read_file(args: &[Value]) -> RuntimeResult<Value> {
    let Some(path) = args.first().and_then(as_str) else {
        return Ok(err_variant("read_file: path argument must be a string"));
    };
    // A relative path inside a `comptime` region reads the file beside
    // the source that embeds it; at run time it means what it always did.
    let path = gossamer_runtime::comptime_paths::resolve(path);
    crate::comptime_gate::guard_read("fs::read", &path)?;
    match gossamer_runtime::sched_global::run_blocking("os-read-file", move || {
        os_std::read_file(&path)
    }) {
        Ok(Ok(bytes)) => {
            let values: Vec<Value> = bytes
                .into_iter()
                .map(|b| Value::Int(i64::from(b)))
                .collect();
            Ok(ok_variant(Value::Array(Arc::new(values))))
        }
        Ok(Err(e)) => Ok(err_variant(e.to_string())),
        Err(e) => Ok(err_variant(e)),
    }
}

fn builtin_os_read_file_to_string(args: &[Value]) -> RuntimeResult<Value> {
    let Some(path) = args.first().and_then(as_str) else {
        return Ok(err_variant(
            "read_file_to_string: path argument must be a string",
        ));
    };
    let path = gossamer_runtime::comptime_paths::resolve(path);
    crate::comptime_gate::guard_read("fs::read_to_string", &path)?;
    match gossamer_runtime::sched_global::run_blocking("os-read-file-string", move || {
        os_std::read_file_to_string(&path)
    }) {
        Ok(Ok(text)) => Ok(ok_variant(Value::String(text.into()))),
        Ok(Err(e)) => Ok(err_variant(e.to_string())),
        Err(e) => Ok(err_variant(e)),
    }
}

fn builtin_os_write_file(args: &[Value]) -> RuntimeResult<Value> {
    let Some(path) = args.first().and_then(as_str) else {
        return Ok(err_variant("write_file: path argument must be a string"));
    };
    let Some(bytes) = args.get(1).and_then(Value::byte_slice) else {
        return Ok(err_variant("write_file: contents must be string or byte array"));
    };
    let bytes = bytes.into_owned();
    let path = path.to_string();
    match gossamer_runtime::sched_global::run_blocking("os-write-file", move || {
        os_std::write_file(&path, &bytes)
    }) {
        Ok(Ok(())) => Ok(ok_variant(Value::Unit)),
        Ok(Err(e)) => Ok(err_variant(e.to_string())),
        Err(e) => Ok(err_variant(e)),
    }
}

fn builtin_os_remove_file(args: &[Value]) -> RuntimeResult<Value> {
    let Some(path) = args.first().and_then(as_str) else {
        return Ok(err_variant("remove_file: path argument must be a string"));
    };
    let path = path.to_string();
    match gossamer_runtime::sched_global::run_blocking("os-remove-file", move || {
        os_std::remove_file(&path)
    }) {
        Ok(Ok(())) => Ok(ok_variant(Value::Unit)),
        Ok(Err(e)) => Ok(err_variant(e.to_string())),
        Err(e) => Ok(err_variant(e)),
    }
}

fn builtin_fs_remove_dir_all(args: &[Value]) -> RuntimeResult<Value> {
    let Some(path) = args.first().and_then(as_str) else {
        return Ok(err_variant(
            "remove_dir_all: path argument must be a string",
        ));
    };
    let path = path.to_string();
    match gossamer_runtime::sched_global::run_blocking("fs-remove-dir-all", move || {
        std::fs::remove_dir_all(path)
    }) {
        Ok(Ok(())) => Ok(ok_variant(Value::Unit)),
        Ok(Err(e)) => Ok(err_variant(e.to_string())),
        Err(e) => Ok(err_variant(e)),
    }
}

fn builtin_fs_remove_dir(args: &[Value]) -> RuntimeResult<Value> {
    let Some(path) = args.first().and_then(as_str) else {
        return Ok(err_variant("remove_dir: path argument must be a string"));
    };
    let path = path.to_string();
    match gossamer_runtime::sched_global::run_blocking("fs-remove-dir", move || {
        std::fs::remove_dir(path)
    }) {
        Ok(Ok(())) => Ok(ok_variant(Value::Unit)),
        Ok(Err(e)) => Ok(err_variant(e.to_string())),
        Err(e) => Ok(err_variant(e)),
    }
}

fn builtin_fs_is_file(args: &[Value]) -> RuntimeResult<Value> {
    let path = args.first().and_then(as_str).unwrap_or("");
    let path = gossamer_runtime::comptime_paths::resolve(path);
    crate::comptime_gate::guard_read("fs::is_file", &path)?;
    Ok(Value::Bool(fs_std::is_file(&path)))
}

fn builtin_fs_is_dir(args: &[Value]) -> RuntimeResult<Value> {
    let path = args.first().and_then(as_str).unwrap_or("");
    let path = gossamer_runtime::comptime_paths::resolve(path);
    crate::comptime_gate::guard_read("fs::is_dir", &path)?;
    Ok(Value::Bool(fs_std::is_dir(&path)))
}

fn builtin_fs_is_symlink(args: &[Value]) -> RuntimeResult<Value> {
    let path = args.first().and_then(as_str).unwrap_or("");
    let path = gossamer_runtime::comptime_paths::resolve(path);
    crate::comptime_gate::guard_read("fs::is_symlink", &path)?;
    Ok(Value::Bool(fs_std::is_symlink(&path)))
}

fn builtin_fs_file_size(args: &[Value]) -> RuntimeResult<Value> {
    let path = args.first().and_then(as_str).unwrap_or("");
    let path = gossamer_runtime::comptime_paths::resolve(path);
    crate::comptime_gate::guard_read("fs::file_size", &path)?;
    let size = fs_std::file_size(&path);
    Ok(Value::Int(i64::try_from(size).unwrap_or(i64::MAX)))
}

fn builtin_fs_canonicalize(args: &[Value]) -> RuntimeResult<Value> {
    let Some(path) = args.first().and_then(as_str) else {
        return Ok(err_variant("canonicalize: path argument must be a string"));
    };
    let path = path.to_string();
    crate::comptime_gate::guard_read("fs::canonicalize", &path)?;
    match gossamer_runtime::sched_global::run_blocking("fs-canonicalize", move || {
        fs_std::canonicalize(&path)
    }) {
        Ok(Ok(p)) => Ok(ok_variant(Value::String(p.into()))),
        Ok(Err(e)) => Ok(err_variant(e.to_string())),
        Err(e) => Ok(err_variant(e)),
    }
}

fn builtin_os_rename(args: &[Value]) -> RuntimeResult<Value> {
    let Some(from) = args.first().and_then(as_str) else {
        return Ok(err_variant("rename: source path must be a string"));
    };
    let Some(to) = args.get(1).and_then(as_str) else {
        return Ok(err_variant("rename: destination path must be a string"));
    };
    let from = from.to_string();
    let to = to.to_string();
    match gossamer_runtime::sched_global::run_blocking("os-rename", move || {
        os_std::rename(&from, &to)
    }) {
        Ok(Ok(())) => Ok(ok_variant(Value::Unit)),
        Ok(Err(e)) => Ok(err_variant(e.to_string())),
        Err(e) => Ok(err_variant(e)),
    }
}

fn builtin_os_exists(args: &[Value]) -> RuntimeResult<Value> {
    let path = gossamer_runtime::comptime_paths::resolve(
        args.first().and_then(as_str).unwrap_or(""),
    );
    crate::comptime_gate::guard_read("fs::exists", &path)?;
    Ok(Value::Bool(os_std::exists(&path)))
}

fn builtin_os_mkdir(args: &[Value]) -> RuntimeResult<Value> {
    let Some(path) = args.first().and_then(as_str) else {
        return Ok(err_variant("mkdir: path argument must be a string"));
    };
    match os_std::mkdir(path) {
        Ok(()) => Ok(ok_variant(Value::Unit)),
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

fn builtin_os_mkdir_all(args: &[Value]) -> RuntimeResult<Value> {
    let Some(path) = args.first().and_then(as_str) else {
        return Ok(err_variant("mkdir_all: path argument must be a string"));
    };
    match os_std::mkdir_all(path) {
        Ok(()) => Ok(ok_variant(Value::Unit)),
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

fn builtin_os_cwd(_args: &[Value]) -> RuntimeResult<Value> {
    match std::env::current_dir() {
        Ok(p) => Ok(ok_variant(Value::String(SmolStr::from(
            p.to_string_lossy().into_owned(),
        )))),
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

fn builtin_os_read_dir(args: &[Value]) -> RuntimeResult<Value> {
    let Some(path) = args.first().and_then(as_str) else {
        return Ok(err_variant("read_dir: path argument must be a string"));
    };
    crate::comptime_gate::guard_read("fs::read_dir", path)?;
    match os_std::read_dir(path) {
        Ok(names) => {
            let values: Vec<Value> = names.into_iter().map(|s| Value::String(s.into())).collect();
            Ok(ok_variant(Value::Array(Arc::new(values))))
        }
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

fn builtin_time_now(_args: &[Value]) -> RuntimeResult<Value> {
    let ms = time_std::SystemTime::now().unix_millis();
    Ok(Value::Int(ms))
}

fn builtin_time_now_ms(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Int(gossamer_runtime::clock::wall_ms()))
}

/// `time::freeze(ms)` - pin the wall clock at `ms` since the epoch.
fn builtin_time_freeze(args: &[Value]) -> RuntimeResult<Value> {
    gossamer_runtime::clock::freeze(args.first().and_then(value_to_int).unwrap_or(0));
    Ok(Value::Unit)
}

/// `time::advance(ms)` - move a frozen wall clock forward.
fn builtin_time_advance(args: &[Value]) -> RuntimeResult<Value> {
    let ms = args.first().and_then(value_to_int).unwrap_or(0);
    Ok(Value::Int(gossamer_runtime::clock::advance(ms)))
}

/// `time::unfreeze()` - return to the real wall clock.
fn builtin_time_unfreeze(_args: &[Value]) -> RuntimeResult<Value> {
    gossamer_runtime::clock::unfreeze();
    Ok(Value::Unit)
}

/// `time::is_frozen() -> bool`.
fn builtin_time_is_frozen(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Bool(gossamer_runtime::clock::is_frozen()))
}

/// `time::sleep_ctx(ctx, ms)` - sleeps unless the context fires first,
/// answering whether the full duration elapsed.
fn builtin_time_sleep_ctx(args: &[Value]) -> RuntimeResult<Value> {
    let ms = args.get(1).and_then(value_to_int).unwrap_or(0);
    if ms < 0 {
        return Err(RuntimeError::Type(
            "time::sleep_ctx: duration_ms must be non-negative".to_string(),
        ));
    }
    let ms = u64::try_from(ms)
        .map_err(|_| RuntimeError::Type("time::sleep_ctx: duration_ms is too large".to_string()))?;
    Ok(sleep_ctx_for(args.first(), std::time::Duration::from_millis(ms)))
}

/// `time::sleep_ctx(ctx, d)` for a wait given as a `time::Duration`, a count
/// of nanoseconds. A negative duration waits not at all.
pub(crate) fn builtin_time_sleep_ns_ctx(args: &[Value]) -> RuntimeResult<Value> {
    let ns = args.get(1).and_then(value_to_int).unwrap_or(0).max(0);
    Ok(sleep_ctx_for(
        args.first(),
        std::time::Duration::from_nanos(u64::try_from(ns).unwrap_or(0)),
    ))
}

/// Waits for `wait` unless `ctx` is cancelled first, answering whether the
/// whole wait elapsed. The wait is split into short steps so a cancellation
/// raised while sleeping is observed promptly.
fn sleep_ctx_for(ctx: Option<&Value>, wait: std::time::Duration) -> Value {
    let Some(ctx) = ctx else {
        return Value::Bool(true);
    };
    let cancelled = || crate::stdlib_builtins::context::value_is_cancelled(ctx);
    if cancelled() {
        return Value::Bool(false);
    }
    let deadline = gossamer_runtime::platform::Instant::now() + wait;
    while gossamer_runtime::platform::Instant::now() < deadline {
        if cancelled() {
            return Value::Bool(false);
        }
        let remaining = deadline.saturating_duration_since(gossamer_runtime::platform::Instant::now());
        gossamer_runtime::platform::sleep(remaining.min(std::time::Duration::from_millis(5)));
    }
    Value::Bool(!cancelled())
}

fn builtin_pprof_cpu_profile(args: &[Value]) -> RuntimeResult<Value> {
    let ms = args.first().and_then(value_to_int).unwrap_or(0);
    if ms < 0 {
        return Err(RuntimeError::Type(
            "pprof::cpu_profile: millis must be non-negative".to_string(),
        ));
    }
    let window = std::time::Duration::from_millis(u64::try_from(ms).unwrap_or(0));
    Ok(Value::String(
        gossamer_runtime::pprof::cpu_profile(window).into(),
    ))
}

fn builtin_pprof_heap_profile(args: &[Value]) -> RuntimeResult<Value> {
    let ms = args.first().and_then(value_to_int).unwrap_or(0);
    if ms < 0 {
        return Err(RuntimeError::Type(
            "pprof::heap_profile: millis must be non-negative".to_string(),
        ));
    }
    let window = std::time::Duration::from_millis(u64::try_from(ms).unwrap_or(0));
    Ok(Value::String(
        gossamer_runtime::pprof::heap_profile(window).into(),
    ))
}

fn builtin_pprof_goroutine_profile(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::String(
        gossamer_runtime::pprof::goroutine_profile().into(),
    ))
}

fn builtin_pprof_mutex_profile(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::String(
        gossamer_runtime::pprof::mutex_profile().into(),
    ))
}

fn builtin_pprof_block_profile(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::String(
        gossamer_runtime::pprof::block_profile().into(),
    ))
}

fn builtin_pprof_execution_trace(args: &[Value]) -> RuntimeResult<Value> {
    let ms = args.first().and_then(value_to_int).unwrap_or(0);
    if ms < 0 {
        return Err(RuntimeError::Type(
            "pprof::execution_trace: millis must be non-negative".to_string(),
        ));
    }
    let window = std::time::Duration::from_millis(u64::try_from(ms).unwrap_or(0));
    Ok(Value::String(
        gossamer_runtime::pprof::execution_trace(window).into(),
    ))
}

fn builtin_pprof_route(args: &[Value]) -> RuntimeResult<Value> {
    let path = args.first().and_then(as_str).unwrap_or("");
    let query = args.get(1).and_then(as_str).unwrap_or("");
    match gossamer_runtime::pprof::route(path, query) {
        Some(body) => Ok(some_variant(Value::String(body.into()))),
        None => Ok(none_variant()),
    }
}

/// `runtime::collect_cycles()`. The interpreter models heap values with
/// `Arc`, so reclamation timing is not observable in program output. Once
/// those refs have dropped, force a process allocator collection/purge so
/// phase-oriented workloads can return pages before starting the next phase,
/// matching the compiled tier's explicit collection boundary.
/// `runtime::set_panic_hook(f)`. Stores the function value; the panic
/// report paths invoke it with the rendered message instead of the
/// default report. Mirrors the compiled tier's `gos_rt_set_panic_hook`.
fn builtin_runtime_set_panic_hook(args: &[Value]) -> RuntimeResult<Value> {
    crate::set_panic_hook_value(args.first().cloned());
    Ok(Value::Unit)
}

fn builtin_runtime_collect_cycles(_args: &[Value]) -> RuntimeResult<Value> {
    gossamer_runtime::collect_process_allocator(true);
    Ok(Value::Unit)
}

/// The VM deliberately uses `Arc`-backed values and has no tracing cycle
/// collector. Exposing that fact lets portable programs select an explicit
/// weak-reference cleanup path instead of inferring it from the execution tier.
fn builtin_runtime_cycle_collection_supported(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Bool(false))
}

/// `runtime::scheduler_stats_json()`. The counters describe the scheduler
/// the caller's goroutines are actually running on, which under the
/// interpreter is its own worker pool rather than the `MultiScheduler` the
/// compiled tiers use: that one never sees a VM goroutine, so reading it
/// here reported a program's own children as zero.
///
/// `steps`, `yields`, `steals`, `parks` and `unparks` name work-stealing
/// mechanics the pool does not have - it takes each task from one shared
/// queue and runs it to completion on a worker thread, with no step loop and
/// no per-worker deques to steal between - so they read as zero.
fn builtin_runtime_scheduler_stats_json(_args: &[Value]) -> RuntimeResult<Value> {
    let stats = crate::vm::goroutine::pool_stats();
    Ok(Value::String(format!(
        "{{\"spawned\":{},\"finished\":{},\"steps\":0,\"yields\":0,\"steals\":0,\"injects\":{},\"parks\":0,\"unparks\":0,\"live_goroutines\":{},\"worker_count\":{},\"worker_count_cap\":{}}}",
        stats.spawned, stats.finished, stats.injects, stats.live, stats.worker_count,
        stats.worker_count_cap,
    )
    .into()))
}

/// `runtime::arena_push()` / `runtime::arena_pop()`. Arena regions are a
/// compiled-tier allocation optimization (bump-allocate, free wholesale).
/// The interpreter models heap values with `Arc` and reclaims them by
/// refcount as the region's values leave scope; with the process
/// allocator's prompt purge (`purge_delay = 0`) those pages return to the
/// OS at block exit, so an explicit region needs no extra reclamation
/// here and stays a semantic no-op, preserving tier parity.
fn builtin_runtime_region_noop(_args: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Unit)
}

/// `time::format_rfc3339(unix_ms: i64) -> Result<String, String>`.
/// RFC 3339 rendering for the given wall-clock instant.
fn builtin_time_format_rfc3339(args: &[Value]) -> RuntimeResult<Value> {
    let ms = args.first().and_then(value_to_int).unwrap_or(0);
    let when = time_std::SystemTime::from_unix_millis(ms);
    match time_std::format_rfc3339(when) {
        Ok(s) => Ok(ok_variant(Value::String(SmolStr::from(s)))),
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

/// `time::parse_rfc3339(s: String) -> Result<i64, String>`. Returns
/// unix milliseconds; the inverse of `format_rfc3339`.
fn builtin_time_parse_rfc3339(args: &[Value]) -> RuntimeResult<Value> {
    let Some(s) = args.first().and_then(as_str) else {
        return Ok(err_variant(
            "time::parse_rfc3339: argument must be a string",
        ));
    };
    match time_std::parse_rfc3339(s) {
        Ok(when) => {
            Ok(ok_variant(Value::Int(when.unix_millis())))
        }
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}

/// `exec::run(prog: String, args: [String]) -> Result<{stdout, stderr, code}, String>`.
/// One-shot subprocess: spawns `prog` with `args`, captures stdout
/// and stderr, waits for completion, and returns the trio. The
/// Command builder pattern remains available through the
/// gossamer-std Rust API for callers that want stdin piping or
/// streamed output; this entry point covers the dominant
/// "run a command and read its output" use case.
fn builtin_exec_run(args: &[Value]) -> RuntimeResult<Value> {
    exec_run_with("exec::run", args, None, None, OutputShape::Struct)
}

/// `process::run` leaf for the injected `Output` wrapper: the same run,
/// answered as the `(stdout, stderr, code)` tuple the wrapper folds.
pub(crate) fn builtin_process_run_raw(args: &[Value]) -> RuntimeResult<Value> {
    exec_run_with("process::run", args, None, None, OutputShape::Tuple)
}

/// How a finished child's output is answered: the `ExecOutput` struct the
/// module builtins give, or the tuple an injected wrapper folds.
#[derive(Clone, Copy)]
enum OutputShape {
    Struct,
    Tuple,
}

/// The finished child's `(stdout, stderr, code)` in `shape`.
fn output_value(stdout: String, stderr: String, code: i64, shape: OutputShape) -> Value {
    match shape {
        OutputShape::Struct => Value::struct_(
            "ExecOutput",
            vec![
                ("stdout", Value::String(SmolStr::from(stdout))),
                ("stderr", Value::String(SmolStr::from(stderr))),
                ("code", Value::Int(code)),
            ],
        ),
        OutputShape::Tuple => Value::Tuple(Arc::from(vec![
            Value::String(SmolStr::from(stdout)),
            Value::String(SmolStr::from(stderr)),
            Value::Int(code),
        ])),
    }
}

/// `process::run_in(prog, args, dir, env) -> Result<Output, errors::Error>`.
///
/// The cwd-and-environment shape of `process::run`: an empty `dir`
/// inherits the caller's working directory, and each `env` pair
/// overrides the inherited environment rather than replacing it.
fn builtin_exec_run_in(args: &[Value]) -> RuntimeResult<Value> {
    exec_run_in_with(args, OutputShape::Struct)
}

/// `process::run_in` leaf for the injected `Output` wrapper.
pub(crate) fn builtin_process_run_in_raw(args: &[Value]) -> RuntimeResult<Value> {
    exec_run_in_with(args, OutputShape::Tuple)
}

fn exec_run_in_with(args: &[Value], shape: OutputShape) -> RuntimeResult<Value> {
    let dir = args.get(2).and_then(as_str).unwrap_or("").to_owned();
    let mut environment: Vec<(String, String)> = Vec::new();
    if let Some(Value::Array(pairs)) = args.get(3) {
        for pair in pairs.iter() {
            if let Value::Tuple(fields) = pair
                && let (Some(name), Some(value)) = (
                    fields.first().and_then(as_str),
                    fields.get(1).and_then(as_str),
                )
            {
                environment.push((name.to_owned(), value.to_owned()));
            }
        }
    }
    exec_run_with("process::run_in", args, Some(dir), Some(environment), shape)
}

/// Runs a child and answers the `Output` struct, for whichever entry
/// point asked.
fn exec_run_with(
    operation: &str,
    args: &[Value],
    dir: Option<String>,
    environment: Option<Vec<(String, String)>>,
    shape: OutputShape,
) -> RuntimeResult<Value> {
    let Some(prog) = args.first().and_then(as_str) else {
        return Ok(err_variant(format!(
            "{operation}: program argument must be a string"
        )));
    };
    let prog = prog.to_owned();
    let mut cmd_args = Vec::new();
    if let Some(Value::Array(arr)) = args.get(1) {
        for arg in arr.iter() {
            if let Some(s) = as_str(arg) {
                cmd_args.push(s.to_owned());
            }
        }
    }
    let working_directory = dir.unwrap_or_default();
    let environment = environment.unwrap_or_default();
    match gossamer_runtime::sched_global::run_blocking("exec-run", move || {
        let mut cmd = std::process::Command::new(prog);
        cmd.args(cmd_args);
        if !working_directory.is_empty() {
            cmd.current_dir(&working_directory);
        }
        for (name, value) in &environment {
            cmd.env(name, value);
        }
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.output()
    }) {
        Ok(Ok(out)) => {
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            let code = i64::from(out.status.code().unwrap_or(-1));
            Ok(ok_variant(output_value(stdout, stderr, code, shape)))
        }
        Ok(Err(e)) => Ok(err_variant(format!("{e}"))),
        Err(e) => Ok(err_variant(e)),
    }
}

/// `exec::spawn(prog, args) -> Result<i64, errors::Error>`.
///
/// Non-blocking sibling of `exec::run`: launches a child
/// process with stdin/stdout/stderr connected to `/dev/null`
/// and returns the PID immediately. Pairs with `exec::kill`
/// for teardown.
fn builtin_exec_spawn(args: &[Value]) -> RuntimeResult<Value> {
    use std::process::{Command as StdCommand, Stdio as StdStdio};
    let Some(prog) = args.first().and_then(as_str) else {
        return Ok(err_variant(
            "exec::spawn: program argument must be a string",
        ));
    };
    let prog = prog.to_owned();
    let mut cmd_args = Vec::new();
    if let Some(Value::Array(arr)) = args.get(1) {
        for arg in arr.iter() {
            if let Some(s) = as_str(arg) {
                cmd_args.push(s.to_owned());
            }
        }
    }
    let display_prog = prog.clone();
    match gossamer_runtime::sched_global::run_blocking("exec-spawn", move || {
        let mut cmd = StdCommand::new(&prog);
        cmd.args(cmd_args);
        cmd.stdin(StdStdio::null());
        cmd.stdout(StdStdio::null());
        cmd.stderr(StdStdio::null());
        cmd.spawn()
    }) {
        Ok(Ok(child)) => {
            let pid = i64::from(child.id());
            // Detach: forget the Child so its Drop doesn't wait.
            #[cfg_attr(target_arch = "wasm32", allow(clippy::forget_non_drop))]
            std::mem::forget(child);
            Ok(ok_variant(Value::Int(pid)))
        }
        Ok(Err(e)) => Ok(err_variant(format!("exec::spawn({display_prog}): {e}"))),
        Err(e) => Ok(err_variant(e)),
    }
}

/// `exec::kill(pid: i64) -> bool` - best-effort SIGTERM. Mirrors
/// the runtime helper `gos_rt_exec_kill` so the VM and compiled
/// tiers behave identically for the daemon-launch teardown path.
/// Shells out to `/bin/kill` instead of pulling in a libc
/// dep just for `kill(2)` - the dispatch path is rare (only the
/// `stop_server` pattern hits it) so an extra fork/exec is fine.
/// `process::spawn_piped(prog, args) -> Result<Child, errors::Error>`.
/// The child's stdin/stdout stay piped; the returned handle struct
/// dispatches the `Child::*` methods. The registry lives in the
/// runtime crate so VM builtins and JIT-compiled code share it.
fn builtin_exec_spawn_piped(args: &[Value]) -> RuntimeResult<Value> {
    let Some(prog) = args.first().and_then(as_str) else {
        return Ok(err_variant(
            "process::spawn_piped: program argument must be a string",
        ));
    };
    let mut cmd_args: Vec<String> = Vec::new();
    if let Some(Value::Array(arr)) = args.get(1) {
        for arg in arr.iter() {
            if let Some(s) = as_str(arg) {
                cmd_args.push(s.to_owned());
            }
        }
    }
    match gossamer_runtime::c_abi::piped_child_spawn(prog, &cmd_args) {
        Ok(handle) => Ok(ok_variant(make_handle_struct("Child", handle))),
        Err(msg) => Ok(err_variant(msg)),
    }
}

/// `child.write_stdin(s) -> bool`.
fn builtin_child_write_stdin(args: &[Value]) -> RuntimeResult<Value> {
    let Some(handle) = args.first().and_then(|v| struct_handle(v, "Child")) else {
        return Ok(Value::Bool(false));
    };
    let Some(s) = args.get(1).and_then(as_str) else {
        return Ok(Value::Bool(false));
    };
    Ok(Value::Bool(
        gossamer_runtime::c_abi::piped_child_write_stdin(handle, s.as_bytes()),
    ))
}

/// `child.close_stdin()`.
fn builtin_child_close_stdin(args: &[Value]) -> RuntimeResult<Value> {
    if let Some(handle) = args.first().and_then(|v| struct_handle(v, "Child")) {
        gossamer_runtime::c_abi::piped_child_close_stdin(handle);
    }
    Ok(Value::Unit)
}

/// `child.read_line() -> Option<String>`.
fn builtin_child_read_line(args: &[Value]) -> RuntimeResult<Value> {
    let Some(handle) = args.first().and_then(|v| struct_handle(v, "Child")) else {
        return Ok(none_variant());
    };
    match gossamer_runtime::c_abi::piped_child_read_line(handle) {
        Some(line) => Ok(some_variant(Value::String(SmolStr::from(line)))),
        None => Ok(none_variant()),
    }
}

/// `child.read_stdout() -> String`.
fn builtin_child_read_stdout(args: &[Value]) -> RuntimeResult<Value> {
    let Some(handle) = args.first().and_then(|v| struct_handle(v, "Child")) else {
        return Ok(Value::String(SmolStr::default()));
    };
    let text = gossamer_runtime::c_abi::piped_child_read_stdout(handle).unwrap_or_default();
    Ok(Value::String(SmolStr::from(text)))
}

/// `child.wait() -> Result<i64, errors::Error>`.
fn builtin_child_wait(args: &[Value]) -> RuntimeResult<Value> {
    let Some(handle) = args.first().and_then(|v| struct_handle(v, "Child")) else {
        return Ok(err_variant("process::Child::wait: not a Child handle"));
    };
    match gossamer_runtime::c_abi::piped_child_wait(handle) {
        Ok(code) => Ok(ok_variant(Value::Int(code))),
        Err(msg) => Ok(err_variant(msg)),
    }
}

/// `child.kill() -> bool`.
fn builtin_child_kill(args: &[Value]) -> RuntimeResult<Value> {
    let Some(handle) = args.first().and_then(|v| struct_handle(v, "Child")) else {
        return Ok(Value::Bool(false));
    };
    Ok(Value::Bool(gossamer_runtime::c_abi::piped_child_kill(
        handle,
    )))
}

fn builtin_exec_kill(args: &[Value]) -> RuntimeResult<Value> {
    #[cfg(unix)]
    use std::process::{Command as StdCommand, Stdio as StdStdio};
    let Some(Value::Int(pid)) = args.first() else {
        return Ok(Value::Bool(false));
    };
    if *pid <= 0 {
        return Ok(Value::Bool(false));
    }
    #[cfg(unix)]
    {
        let pid = *pid;
        let status = gossamer_runtime::sched_global::run_blocking("exec-kill", move || {
            StdCommand::new("/bin/kill")
                .arg("-TERM")
                .arg(format!("{pid}"))
                .stdout(StdStdio::null())
                .stderr(StdStdio::null())
                .status()
        });
        Ok(Value::Bool(matches!(status, Ok(Ok(s)) if s.success())))
    }
    #[cfg(windows)]
    {
        use std::process::{Command as StdCommand, Stdio as StdStdio};
        let pid = *pid;
        let status = gossamer_runtime::sched_global::run_blocking("exec-kill", move || {
            StdCommand::new("taskkill")
                .args(["/F", "/PID", &format!("{pid}")])
                .stdout(StdStdio::null())
                .stderr(StdStdio::null())
                .status()
        });
        Ok(Value::Bool(matches!(status, Ok(Ok(s)) if s.success())))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        Ok(Value::Bool(false))
    }
}

/// `exec::signal(pid: i64, signum: i64) -> bool`. Sends an
/// arbitrary signal number to the target pid. Mirrors
/// `gos_rt_exec_signal` so the VM matches the compiled tier.
fn builtin_exec_signal(args: &[Value]) -> RuntimeResult<Value> {
    let Some(Value::Int(pid)) = args.first() else {
        return Ok(Value::Bool(false));
    };
    let Some(Value::Int(signum)) = args.get(1) else {
        return Ok(Value::Bool(false));
    };
    Ok(Value::Bool(exec_std::send_raw_signal(*pid, *signum)))
}

/// `exec::kill_group(pid: i64) -> bool`. SIGTERMs the entire group
/// led by `pid` on Unix; best-effort `TerminateProcess` on Windows.
fn builtin_exec_kill_group(args: &[Value]) -> RuntimeResult<Value> {
    let Some(Value::Int(pid)) = args.first() else {
        return Ok(Value::Bool(false));
    };
    Ok(Value::Bool(exec_std::send_group_term(*pid)))
}

/// `exec::wait_timeout(pid: i64, ms: i64) -> i64`. Polls the pid
/// with WNOHANG until it exits or `ms` elapses. Returns the exit
/// code on success, -1 on timeout, -2 on error.
fn builtin_exec_wait_timeout(args: &[Value]) -> RuntimeResult<Value> {
    let Some(Value::Int(pid)) = args.first() else {
        return Ok(Value::Int(-2));
    };
    let Some(Value::Int(ms)) = args.get(1) else {
        return Ok(Value::Int(-2));
    };
    if *ms < 0 {
        return Ok(Value::Int(-2));
    }
    Ok(Value::Int(exec_std::wait_pid_timeout(*pid, *ms)))
}

/// `exec::pipeline_run(commands: [String]) -> Result<Output, errors::Error>`.
/// Mirrors `gos_rt_exec_pipeline_run_raw`. Each entry is a
/// whitespace-split shell command; stdout of stage N feeds stdin
/// of stage N+1.
fn builtin_exec_pipeline_run(args: &[Value]) -> RuntimeResult<Value> {
    exec_pipeline_run_with(args, OutputShape::Struct)
}

/// `process::pipeline_run` leaf for the injected `Output` wrapper.
pub(crate) fn builtin_process_pipeline_run_raw(args: &[Value]) -> RuntimeResult<Value> {
    exec_pipeline_run_with(args, OutputShape::Tuple)
}

fn exec_pipeline_run_with(args: &[Value], shape: OutputShape) -> RuntimeResult<Value> {
    let Some(Value::Array(arr)) = args.first() else {
        return Ok(err_variant(
            "exec::pipeline_run: commands must be Vec<String>",
        ));
    };
    let stages: Vec<Vec<String>> = arr
        .iter()
        .filter_map(as_str)
        .map(tokenize_pipeline_shell)
        .filter(|s: &Vec<String>| !s.is_empty())
        .collect();
    if stages.is_empty() {
        return Ok(err_variant("exec::pipeline_run: empty pipeline"));
    }
    match gossamer_runtime::sched_global::run_blocking("exec-pipeline", move || {
        run_pipeline_stages(stages)
    }) {
        Ok(Ok((stdout, stderr, code))) => Ok(ok_variant(output_value(stdout, stderr, code, shape))),
        Ok(Err(e)) => Ok(err_variant(e)),
        Err(e) => Ok(err_variant(e)),
    }
}

fn tokenize_pipeline_shell(line: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    for ch in line.chars() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            c if c.is_whitespace() && !in_single && !in_double => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn run_pipeline_stages(stages: Vec<Vec<String>>) -> Result<(String, String, i64), String> {
    use std::io::Read;
    use std::process::{Command as StdCommand, Stdio as StdStdio};
    let last = stages.len() - 1;
    let mut children: Vec<std::process::Child> = Vec::with_capacity(stages.len());
    for (i, parts) in stages.iter().enumerate() {
        let mut cmd = StdCommand::new(&parts[0]);
        if parts.len() > 1 {
            cmd.args(&parts[1..]);
        }
        if i > 0 {
            let Some(prev_stdout) = children.last_mut().and_then(|c| c.stdout.take()) else {
                return Err(format!("pipeline stage {i}: predecessor stdout missing"));
            };
            cmd.stdin(prev_stdout);
        }
        cmd.stdout(StdStdio::piped());
        if i == last {
            cmd.stderr(StdStdio::piped());
        }
        match cmd.spawn() {
            Ok(c) => children.push(c),
            Err(e) => return Err(format!("pipeline stage {i} ({}): {e}", parts[0])),
        }
    }
    let mut tail = children.pop().expect("nonempty");
    let mut stdout = Vec::new();
    if let Some(mut s) = tail.stdout.take() {
        let _ = s.read_to_end(&mut stdout);
    }
    let mut stderr = Vec::new();
    if let Some(mut e) = tail.stderr.take() {
        let _ = e.read_to_end(&mut stderr);
    }
    let tail_status = tail.wait().map_err(|e| format!("tail wait: {e}"))?;
    for (i, mut c) in children.into_iter().enumerate() {
        let _ = c.wait().map_err(|e| format!("stage {i} wait: {e}"))?;
    }
    Ok((
        String::from_utf8_lossy(&stdout).into_owned(),
        String::from_utf8_lossy(&stderr).into_owned(),
        i64::from(tail_status.code().unwrap_or(-1)),
    ))
}

// ---------------------------------------------------------------
// signal::on / Notifier::wait / Notifier::try_wait
// ---------------------------------------------------------------

fn signal_notifier_table() -> &'static parking_lot::Mutex<Vec<signal_std::Notifier>> {
    static TABLE: std::sync::OnceLock<parking_lot::Mutex<Vec<signal_std::Notifier>>> =
        std::sync::OnceLock::new();
    TABLE.get_or_init(|| parking_lot::Mutex::new(Vec::new()))
}

fn raw_to_signal(raw: i64) -> signal_std::Signal {
    match raw {
        2 => signal_std::sigs::SIGINT,
        15 => signal_std::sigs::SIGTERM,
        1 => signal_std::sigs::SIGHUP,
        10 => signal_std::sigs::SIGUSR1,
        12 => signal_std::sigs::SIGUSR2,
        3 => signal_std::sigs::SIGQUIT,
        _ => signal_std::Signal("SIGOTHER"),
    }
}

/// `signal::on(sig_raw) -> i64` - registers a notifier and returns
/// an opaque handle for use with `signal_wait` / `signal_try_wait`.
fn builtin_signal_on(args: &[Value]) -> RuntimeResult<Value> {
    let raw = match args.first() {
        Some(Value::Int(n)) => *n,
        _ => return Ok(Value::Int(-1)),
    };
    let sig = raw_to_signal(raw);
    let notifier = signal_std::on(sig);
    let mut table = signal_notifier_table().lock();
    let handle = i64::try_from(table.len()).unwrap_or(-1);
    table.push(notifier);
    Ok(Value::Int(handle))
}

/// `signal_wait(handle)` - blocks until the registered signal fires.
fn builtin_signal_wait(args: &[Value]) -> RuntimeResult<Value> {
    let Some(Value::Int(handle)) = args.first() else {
        return Ok(Value::Unit);
    };
    let table = signal_notifier_table().lock();
    let Some(notifier) = table.get(*handle as usize) else {
        return Ok(Value::Unit);
    };
    let notifier = notifier.clone();
    drop(table);
    notifier.wait();
    Ok(Value::Unit)
}

/// `signal_try_wait(handle) -> bool` - non-blocking check.
fn builtin_signal_try_wait(args: &[Value]) -> RuntimeResult<Value> {
    let Some(Value::Int(handle)) = args.first() else {
        return Ok(Value::Bool(false));
    };
    let table = signal_notifier_table().lock();
    let Some(notifier) = table.get(*handle as usize) else {
        return Ok(Value::Bool(false));
    };
    let notifier = notifier.clone();
    drop(table);
    Ok(Value::Bool(notifier.try_wait()))
}

/// `fs::read_dir(path: String) -> Result<[DirInfo], String>` - direct-children
/// listing with metadata. `DirInfo` is a struct carrying the entry's
/// name, full path, type predicates, byte size (`0` for directories),
/// and modification time as unix milliseconds. The result is sorted
/// by name. Pairs with `fs::walk_dir` for recursive traversal.
fn builtin_fs_list_dir(args: &[Value]) -> RuntimeResult<Value> {
    let Some(path) = args.first().and_then(as_str) else {
        return Ok(err_variant("fs::read_dir: path argument must be a string"));
    };
    let path = gossamer_runtime::comptime_paths::resolve(path);
    crate::comptime_gate::guard_read("fs::read_dir", &path)?;
    let entries = match fs_std::read_dir(fs_std::decode_path(&path)) {
        Ok(es) => es,
        Err(e) => return Ok(err_variant(format!("{e}"))),
    };
    let items: Vec<Value> = entries.iter().map(dir_info_value).collect();
    Ok(ok_variant(Value::Array(Arc::new(items))))
}

/// Builds the `DirInfo` struct value shared by `fs::read_dir` and
/// `fs::walk_dir`; field order matches the compiled tier's blob.
fn dir_info_fields(entry: &fs_std::DirEntry) -> Vec<(&'static str, Value)> {
    // Every entry a compile-time listing hands back is an input of the
    // fold: its name, size, and modification time are all values the
    // region can compile into the program. A walk reaches each
    // directory it descends into through here too, so recording the
    // entry covers the whole tree rather than only the root.
    gossamer_runtime::comptime_inputs::record(&fs_std::encode_path(&entry.path));
    let (size, modified_ms) = std::fs::metadata(&entry.path).map_or((0_i64, 0_i64), |m| {
        let size = i64::try_from(m.len()).unwrap_or(i64::MAX);
        let modified_ms = m
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        (size, modified_ms)
    });
    let path_str = fs_std::encode_path(&entry.path);
    vec![
        ("name", Value::String(SmolStr::from(entry.name.clone()))),
        ("path", Value::String(SmolStr::from(path_str))),
        ("is_file", Value::Bool(entry.is_file)),
        ("is_dir", Value::Bool(entry.is_dir)),
        ("is_symlink", Value::Bool(entry.is_symlink)),
        ("size", Value::Int(size)),
        ("modified_ms", Value::Int(modified_ms)),
    ]
}

/// The `DirInfo` struct value `fs::walk_dir` hands its visitor.
fn dir_info_value(entry: &fs_std::DirEntry) -> Value {
    Value::struct_("DirInfo", dir_info_fields(entry))
}

/// `fs::read_dir` leaf for the injected `DirInfo` wrapper: each entry as the
/// `(name, path, is_file, is_dir, is_symlink, size, modified_ms)` tuple the
/// wrapper folds.
pub(crate) fn builtin_fs_read_dir_raw(args: &[Value]) -> RuntimeResult<Value> {
    let Some(path) = args.first().and_then(as_str) else {
        return Ok(err_variant("fs::read_dir: path argument must be a string"));
    };
    let path = gossamer_runtime::comptime_paths::resolve(path);
    crate::comptime_gate::guard_read("fs::read_dir", &path)?;
    let entries = match fs_std::read_dir(fs_std::decode_path(&path)) {
        Ok(es) => es,
        Err(e) => return Ok(err_variant(format!("{e}"))),
    };
    let items: Vec<Value> = entries
        .iter()
        .map(|entry| {
            let values: Vec<Value> = dir_info_fields(entry).into_iter().map(|(_, v)| v).collect();
            Value::Tuple(Arc::from(values))
        })
        .collect();
    Ok(ok_variant(Value::Array(Arc::new(items))))
}

/// `fs::walk_dir(root: String, visit: Fn(fs::DirInfo) -> Result<(),
/// errors::Error>) -> Result<(), errors::Error>`. Recursively visits every
/// descendant, invoking `visit` with the same `DirInfo` shape as
/// `fs::read_dir`. Stops as soon as `visit` returns `Err`, propagating that
/// value as the walk's own result. Aliased as `path::walk` for Go-shaped
/// spelling.
/// `fs::walk_dir` leaf for the injected `DirInfo` wrapper: the same walk,
/// handing the visitor each entry as the tuple the wrapper folds.
pub(crate) fn native_fs_walk_dir_raw(
    dispatch: &mut dyn NativeDispatch,
    args: &[Value],
) -> RuntimeResult<Value> {
    walk_dir_with(dispatch, args, |entry| {
        let values: Vec<Value> = dir_info_fields(entry).into_iter().map(|(_, v)| v).collect();
        Value::Tuple(Arc::from(values))
    })
}

fn native_fs_walk_dir(dispatch: &mut dyn NativeDispatch, args: &[Value]) -> RuntimeResult<Value> {
    walk_dir_with(dispatch, args, dir_info_value)
}

/// Walks `args[0]`, calling `args[1]` with each entry `entry_value` builds.
fn walk_dir_with(
    dispatch: &mut dyn NativeDispatch,
    args: &[Value],
    entry_value: impl Fn(&fs_std::DirEntry) -> Value,
) -> RuntimeResult<Value> {
    let Some(root) = args.first().and_then(as_str) else {
        return Ok(err_variant("fs::walk_dir: root argument must be a string"));
    };
    let visit = args.get(1).cloned().unwrap_or(Value::Unit);
    let mut stop_err: Option<Value> = None;
    let mut fault: Option<RuntimeError> = None;
    let root = gossamer_runtime::comptime_paths::resolve(root);
    crate::comptime_gate::guard_read("fs::walk_dir", &root)?;
    let visit_result = fs_std::walk_dir(fs_std::decode_path(&root), |entry| {
        match dispatch.call_value(&visit, vec![entry_value(entry)]) {
            Ok(Value::Variant(v)) if v.name == "Err" => {
                stop_err = Some(v.fields.first().cloned().unwrap_or(Value::Unit));
                Err(std::io::Error::other("gossamer visitor stop"))
            }
            Ok(_) => Ok(()),
            Err(e) => {
                fault = Some(e);
                Err(std::io::Error::other("gossamer visitor fault"))
            }
        }
    });
    if let Some(e) = fault {
        return Err(e);
    }
    if let Some(payload) = stop_err {
        return Ok(Value::variant("Err", vec![payload]));
    }
    match visit_result {
        Ok(()) => Ok(ok_variant(Value::Unit)),
        Err(e) => Ok(err_variant(format!("{e}"))),
    }
}



/// `slog::info(msg: String)` - emits a JSON-line record at INFO
/// level on stderr. The full structured-fields API stays in
/// `gossamer-std::slog`; this entry point covers the common
/// "log this message" call shape from .gos source.
fn builtin_slog_info(args: &[Value]) -> RuntimeResult<Value> {
    slog_emit(slog_std::Level::Info, args)
}
fn builtin_slog_warn(args: &[Value]) -> RuntimeResult<Value> {
    slog_emit(slog_std::Level::Warn, args)
}
fn builtin_slog_error(args: &[Value]) -> RuntimeResult<Value> {
    slog_emit(slog_std::Level::Error, args)
}
fn builtin_slog_debug(args: &[Value]) -> RuntimeResult<Value> {
    slog_emit(slog_std::Level::Debug, args)
}

fn slog_emit(level: slog_std::Level, args: &[Value]) -> RuntimeResult<Value> {
    let msg = args.first().and_then(as_str).unwrap_or("");
    // Trailing args after the message are key/value pairs:
    // `slog::info("served", "status", 200i64, "path", "/")`.
    let mut fields = Vec::new();
    let mut iter = args.iter().skip(1);
    while let Some(key) = iter.next() {
        let Some(k) = as_str(key) else { break };
        let Some(value) = iter.next() else { break };
        fields.push((k.to_string(), format!("{value}")));
    }
    slog_record(level, msg, &fields);
    Ok(Value::Unit)
}

/// Writes one `slog`-shaped JSON record to stderr.
///
/// Formats directly to the interp's `STDERR_WRITER` so the gossamer-cli
/// test harness's stderr capture observes it (the gossamer-std
/// `JsonHandler` writes to `std::io::stderr()`, which the cli's writer
/// redirect does not see). The line shape is the compiled runtime's
/// `slog_emit`, so a record reads identically on every tier.
fn slog_record(level: slog_std::Level, msg: &str, fields: &[(String, String)]) {
    let mut line = String::with_capacity(64 + msg.len());
    line.push('{');
    let _ = write!(line, "\"level\":\"{}\"", level.tag());
    let _ = write!(line, ",\"msg\":\"{}\"", json_escape_str(msg));
    for (key, value) in fields {
        let _ = write!(
            line,
            ",\"{}\":\"{}\"",
            json_escape_str(key),
            json_escape_str(value),
        );
    }
    line.push_str("}\n");
    STDERR_WRITER.with(|cell| (cell.get())(&line));
}

/// Minimal JSON-string escaper for the slog builtin. Mirrors
/// `gossamer-std::slog::json_string`'s escape rules but writes
/// into a `String` directly so we can format the line in a
/// single allocation. Skipping the wrapping `"` since callers
/// own the surrounding quotes.
fn json_escape_str(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 => {
                use std::fmt::Write;
                let _ = write!(out, "\\u{:04x}", ch as u32);
            }
            ch => out.push(ch),
        }
    }
    out
}

/// `bufio::Scanner::new(stream)` - constructs a scanner.
///
/// State is kept in a Map (`Arc<Mutex>`) so `scan()`/`text()` can mutate
/// the cursor without requiring the immutable-struct writeback path.
fn builtin_bufio_scanner_new(args: &[Value]) -> RuntimeResult<Value> {
    use std::io::Read;
    let is_stdin = args.first().is_some_and(|v| stream_fd(v) == 0);
    let lines: Vec<Value> = if is_stdin {
        let read = gossamer_runtime::sched_global::run_blocking("stdin-scanner-read", || {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf).map(|_| buf)
        });
        let mut buf = match read {
            Ok(Ok(buf)) => buf,
            Ok(Err(_)) | Err(_) => String::new(),
        };
        buf.shrink_to_fit();
        buf.lines()
            .map(|s| Value::String(SmolStr::from(s.to_string())))
            .collect()
    } else {
        Vec::new()
    };
    let mut state = dense_map_with_capacity(4);
    state.insert(
        MapKey::Str(SmolStr::from("lines")),
        Value::Array(Arc::new(lines)),
    );
    state.insert(MapKey::Str(SmolStr::from("cursor")), Value::Int(-1));
    state.insert(
        MapKey::Str(SmolStr::from("current")),
        Value::String(SmolStr::from("")),
    );
    let state_map = Value::Map(Arc::new(parking_lot::Mutex::new(state.into())));
    let fields: Vec<(&'static str, Value)> = vec![("__state", state_map)];
    Ok(Value::struct_(
        "Scanner",
        Arc::unwrap_or_clone(Arc::new(fields)),
    ))
}

/// Extracts the mutable state Map from a Scanner struct.
fn scanner_state(args: &[Value]) -> Option<Arc<parking_lot::Mutex<crate::vm_map::VmMap>>> {
    let first = args.first()?;
    let guard;
    let value = if let Value::MutCell(cell) = first {
        guard = cell.lock();
        &*guard
    } else {
        first
    };
    if let Value::Struct(inner) = value {
        if inner.name == "Scanner" {
            for (name, val) in &inner.fields {
                if *name == "__state" {
                    if let Value::Map(m) = val {
                        return Some(Arc::clone(m));
                    }
                }
            }
        }
    }
    None
}

/// `scanner.scan() -> bool`. Advances to the next line; returns `true` if one exists.
fn builtin_bufio_scanner_scan(args: &[Value]) -> RuntimeResult<Value> {
    let Some(state) = scanner_state(args) else {
        return Ok(Value::Bool(false));
    };
    let mut map = state.lock();
    let cursor = match map.get(&MapKey::Str(SmolStr::from("cursor"))) {
        Some(Value::Int(n)) => *n,
        _ => -1,
    };
    let new_cursor = cursor + 1;
    // Clone the next line while the immutable borrow is still live; then drop it
    // before calling map.insert (which needs a mutable borrow).
    let next_line = match map.get(&MapKey::Str(SmolStr::from("lines"))) {
        Some(Value::Array(arr)) if (new_cursor as usize) < arr.len() => {
            Some(arr[new_cursor as usize].clone())
        }
        _ => None,
    };
    if let Some(line) = next_line {
        map.insert(MapKey::Str(SmolStr::from("cursor")), Value::Int(new_cursor));
        map.insert(MapKey::Str(SmolStr::from("current")), line);
        Ok(Value::Bool(true))
    } else {
        Ok(Value::Bool(false))
    }
}

/// `scanner.text() -> String`. Returns the last line read by `scan()`.
fn builtin_bufio_scanner_text(args: &[Value]) -> RuntimeResult<Value> {
    let Some(state) = scanner_state(args) else {
        return Ok(Value::String(SmolStr::from("")));
    };
    let map = state.lock();
    match map.get(&MapKey::Str(SmolStr::from("current"))) {
        Some(v) => Ok(v.clone()),
        None => Ok(Value::String(SmolStr::from(""))),
    }
}

/// `scanner.next() -> Option<String>`. Returns the next line and advances, `None` at EOF.
fn builtin_bufio_scanner_next(args: &[Value]) -> RuntimeResult<Value> {
    let Some(state) = scanner_state(args) else {
        return Ok(none_variant());
    };
    let mut map = state.lock();
    let cursor = match map.get(&MapKey::Str(SmolStr::from("cursor"))) {
        Some(Value::Int(n)) => *n,
        _ => -1,
    };
    let new_cursor = cursor + 1;
    let next_line = match map.get(&MapKey::Str(SmolStr::from("lines"))) {
        Some(Value::Array(arr)) if (new_cursor as usize) < arr.len() => {
            Some(arr[new_cursor as usize].clone())
        }
        _ => None,
    };
    if let Some(line) = next_line {
        map.insert(MapKey::Str(SmolStr::from("cursor")), Value::Int(new_cursor));
        map.insert(MapKey::Str(SmolStr::from("current")), line.clone());
        Ok(some_variant(line))
    } else {
        Ok(none_variant())
    }
}

/// `bufio::read_lines(path: String) -> Result<[String], String>`.
/// One-shot read of every line from the file at `path`. The full
/// streaming `Scanner` API stays available via gossamer-std for
/// callers that need backpressure or partial reads; this is the
/// 95% case where you just want the lines.
fn builtin_bufio_read_lines(args: &[Value]) -> RuntimeResult<Value> {
    let Some(path) = args.first().and_then(as_str) else {
        return Ok(err_variant(
            "bufio::read_lines: path argument must be a string",
        ));
    };
    let path = path.to_string();
    match gossamer_runtime::sched_global::run_blocking("bufio-read-lines", move || {
        std::fs::read_to_string(path)
    }) {
        Ok(Ok(mut contents)) => {
            contents.shrink_to_fit();
            let lines: Vec<Value> = contents
                .lines()
                .map(|s| Value::String(SmolStr::from(s.to_string())))
                .collect();
            Ok(ok_variant(Value::Array(Arc::new(lines))))
        }
        Ok(Err(e)) => Ok(err_variant(e.to_string())),
        Err(e) => Ok(err_variant(e)),
    }
}

#[cfg(test)]
mod blocking_file_tests {
    use super::*;

    fn ok_payload(value: Value) -> Value {
        match value {
            Value::Variant(inner) if inner.name == "Ok" => inner.fields[0].clone(),
            other => panic!("expected Ok result, got {other:?}"),
        }
    }

    #[test]
    fn whole_file_builtins_preserve_text_and_lines() {
        let path = gossamer_runtime::platform::temp_dir().join(format!(
            "gossamer-blocking-file-{}",
            gossamer_runtime::platform::process_id()
        ));
        let path_value = Value::String(path.to_string_lossy().into_owned().into());
        ok_payload(
            builtin_os_write_file(&[path_value.clone(), Value::String("one\ntwo\n".into())])
                .expect("write file"),
        );
        assert!(matches!(
            ok_payload(
                builtin_os_read_file_to_string(std::slice::from_ref(&path_value))
                    .expect("read file")
            ),
            Value::String(text) if text.as_str() == "one\ntwo\n"
        ));
        let lines = ok_payload(
            builtin_bufio_read_lines(std::slice::from_ref(&path_value)).expect("read lines"),
        );
        assert!(matches!(lines, Value::Array(lines) if lines.len() == 2));

        let renamed = path.with_extension("renamed");
        let renamed_value = Value::String(renamed.to_string_lossy().into_owned().into());
        ok_payload(
            builtin_os_rename(&[path_value, renamed_value.clone()]).expect("rename file"),
        );
        assert!(matches!(
            ok_payload(
                builtin_fs_canonicalize(std::slice::from_ref(&renamed_value))
                    .expect("canonicalize file")
            ),
            Value::String(path) if path.as_str().ends_with(".renamed")
        ));
        ok_payload(builtin_os_remove_file(&[renamed_value]).expect("remove file"));

        let dir = gossamer_runtime::platform::temp_dir().join(format!(
            "gossamer-blocking-dir-{}",
            gossamer_runtime::platform::process_id()
        ));
        std::fs::create_dir(&dir).expect("create fixture dir");
        ok_payload(
            builtin_fs_remove_dir(&[Value::String(dir.to_string_lossy().into_owned().into())])
                .expect("remove dir"),
        );
        assert!(!dir.exists());

        let nested = gossamer_runtime::platform::temp_dir().join(format!(
            "gossamer-blocking-tree-{}",
            gossamer_runtime::platform::process_id()
        ));
        std::fs::create_dir_all(nested.join("child")).expect("create nested fixture");
        ok_payload(
            builtin_fs_remove_dir_all(&[Value::String(
                nested.to_string_lossy().into_owned().into(),
            )])
            .expect("remove dir all"),
        );
        assert!(!nested.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn read_dir_path_field_reopens_non_utf8_directory() {
        use std::os::unix::ffi::OsStringExt;

        let root = gossamer_runtime::platform::temp_dir().join(format!(
            "gossamer-interp-non-utf8-dir-{}",
            gossamer_runtime::platform::process_id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let child = root.join(std::ffi::OsString::from_vec(b"x\xa0y".to_vec()));
        std::fs::create_dir(&child).unwrap();
        std::fs::write(child.join("payload"), b"x").unwrap();

        let root_value = Value::String(root.to_string_lossy().into_owned().into());
        let listed = ok_payload(builtin_fs_list_dir(&[root_value]).unwrap());
        let Value::Array(entries) = listed else {
            panic!("expected directory entries");
        };
        let Value::Struct(entry) = &entries[0] else {
            panic!("expected DirInfo");
        };
        let path = entry
            .fields
            .iter()
            .find_map(|(name, value)| (*name == "path").then_some(value.clone()))
            .expect("DirInfo.path");
        let nested = ok_payload(builtin_fs_list_dir(&[path]).unwrap());
        assert!(matches!(nested, Value::Array(entries) if entries.len() == 1));

        std::fs::remove_dir_all(&root).unwrap();
    }
}
