/// Pointer-sized function type used by the builtin installer.
type BuiltinFn = fn(&[Value]) -> RuntimeResult<Value>;

/// Re-export of [`BuiltinFn`] for sibling builtin modules.
pub(crate) type BuiltinFnPub = fn(&[Value]) -> RuntimeResult<Value>;

fn install_module(
    prefix: &'static str,
    entries: &[(&'static str, BuiltinFn)],
    globals: &mut Vec<(&'static str, Value)>,
) {
    for (short, call) in entries {
        globals.push((*short, builtin(short, *call)));
        let joined: &'static str = Box::leak(format!("{prefix}::{short}").into_boxed_str());
        globals.push((joined, builtin(joined, *call)));
    }
}

/// Same as [`install_module`] but exposed for sibling stdlib-builtin
/// modules that build on the same shape (qualified + bare-name
/// registration).
pub(crate) fn install_module_pub(
    prefix: &'static str,
    entries: &[(&'static str, BuiltinFnPub)],
    globals: &mut Vec<(&'static str, Value)>,
) {
    install_module(prefix, entries, globals);
}

/// Public-crate wrapper around [`builtin`] so sibling builtin
/// modules can construct callable values without re-implementing
/// the boxing.
pub(crate) fn builtin_pub(name: &'static str, call: BuiltinFnPub) -> Value {
    builtin(name, call)
}

fn builtin_variant_one<const TAG: char>(args: &[Value]) -> RuntimeResult<Value> {
    let name = match TAG {
        'O' => "Ok",
        'E' => "Err",
        'S' => "Some",
        _ => "Variant",
    };
    let payload = args.first().cloned().unwrap_or(Value::Unit);
    Ok(Value::variant(name, vec![payload]))
}

fn builtin_field<const TAG: char>(args: &[Value]) -> RuntimeResult<Value> {
    let field_name = match TAG {
        'p' => "path",
        'm' => "method",
        _ => return Ok(Value::Unit),
    };
    match args.first() {
        Some(Value::Struct(inner)) => {
            for (ident, value) in &inner.fields {
                if (*ident) == field_name {
                    return Ok(value.clone());
                }
            }
            Ok(Value::Unit)
        }
        _ => Ok(Value::Unit),
    }
}

fn builtin(name: &'static str, call: fn(&[Value]) -> RuntimeResult<Value>) -> Value {
    Value::builtin(name, call)
}

fn native(
    name: &'static str,
    call: fn(&mut dyn NativeDispatch, &[Value]) -> RuntimeResult<Value>,
) -> Value {
    Value::native(name, call)
}

/// Captured stdout used by `println` and friends. The test harness
/// swaps this out via [`set_stdout_writer`] and reads back the buffer.
///
/// The pointer is `'static` only because the value it points at is a
/// per-thread static; no cross-thread access is possible.
type Writer = fn(&str);

thread_local! {
    static STDOUT_WRITER: std::cell::Cell<Writer> = const { std::cell::Cell::new(default_stdout) };
    static STDERR_WRITER: std::cell::Cell<Writer> = const { std::cell::Cell::new(default_stderr) };
}

fn default_stdout(text: &str) {
    // JIT-promoted bodies write directly into the runtime's stdout
    // buffer; the bytecode VM writes here through Rust's `stdout`.
    // Both funnel into the same `std::io::stdout()` sink, so draining
    // the runtime buffer before this write keeps hybrid (bytecode +
    // JIT-native) output in program order. No-op when nothing was
    // buffered, so pure-bytecode programs pay only an uncontended lock.
    gossamer_runtime::c_abi::flush_stdout_buffer();
    write_stdio(&mut std::io::stdout().lock(), text);
}

/// Writes `text` to a standard stream, ending the process as a compiled
/// program's write would when the stream's reader has gone.
fn write_stdio(stream: &mut impl std::io::Write, text: &str) {
    if let Err(err) = stream.write_all(text.as_bytes()) {
        gossamer_runtime::c_abi::print::end_on_closed_stdio(&err);
    }
}

fn default_stderr(text: &str) {
    // Drain any JIT-buffered stdout first so interleaved stdout/stderr
    // reaches the terminal in program order (see `default_stdout`).
    gossamer_runtime::c_abi::flush_stdout_buffer();
    write_stdio(&mut std::io::stderr().lock(), text);
}

/// Installs a custom stdout writer for the current thread. Returns the
/// previously-installed writer so the caller can restore it.
///
/// Side effect: also disables the JIT process-wide. The runtime's
/// `gos_rt_print_*` family writes to a separate buffer and flushes
/// directly to fd 1 - there's no per-call hook for that path, so a
/// JIT-promoted body's output bypasses the writer the test set up.
/// Disabling the JIT routes everything through the bytecode VM's
/// `STDOUT_WRITER`, which the redirect actually catches. Test
/// suites that wrap their writer with `set_stdout_writer` therefore
/// see every byte the program emits, JIT-eligible function or not.
///
/// The disable is reversible: callers in long-lived processes (REPL,
/// test runners that swap writers between cases) should pair the
/// teardown that restores the previous writer with a call to
/// [`crate::set_jit_enabled`] so subsequent runs regain JIT
/// promotion. Prior versions left the JIT permanently disabled after
/// any `set_stdout_writer` call.
pub fn set_stdout_writer(writer: Writer) -> Writer {
    crate::set_jit_disabled();
    STDOUT_WRITER.with(|cell| cell.replace(writer))
}

/// Installs a custom stderr writer for the current thread. Returns the
/// previously-installed writer so the caller can restore it.
pub fn set_stderr_writer(writer: Writer) -> Writer {
    STDERR_WRITER.with(|cell| cell.replace(writer))
}

/// Process-wide cap on the number of HTTP requests the interpreter-
/// hosted `http::serve` accepts before returning. Set by tests to
/// force the server loop to terminate; production callers leave it
/// at zero and rely on the `GOSSAMER_HTTP_MAX_REQUESTS` env var or a
/// shutdown signal.
///
/// A value of `0` means unset; any positive value wins over the env
/// var so tests that configure the override remain deterministic.
static HTTP_MAX_REQUESTS_OVERRIDE: AtomicU64 = AtomicU64::new(0);

/// Installs a programmatic cap on the number of HTTP requests the
/// interpreter's `http::serve` accepts before returning. Primarily a
/// test hook so that the server thread exits cleanly after the test
/// drives its one fixture request.
pub fn set_http_max_requests(n: u64) {
    HTTP_MAX_REQUESTS_OVERRIDE.store(n, Ordering::SeqCst);
}

thread_local! {
    /// Per-thread counters and messages tracked by `testing::check*`
    /// builtins; `gos test` resets them around each `#[test]` call
    /// so assertions that fire without being `?`-propagated still
    /// register as failures.
    static TEST_TALLY: std::cell::RefCell<TestTally> =
        const { std::cell::RefCell::new(TestTally::new()) };
}

/// Snapshot of `testing::check*` outcomes for the current test.
#[derive(Debug, Clone, Default)]
pub struct TestTally {
    /// Total `check*` calls observed since the last reset.
    pub assertions: u32,
    /// Subset of those that returned `Err`.
    pub failures: u32,
    /// First failure message, if any; later failures are still
    /// counted but not recorded, on the assumption that the first is
    /// usually the root cause.
    pub first_failure: Option<String>,
}

impl TestTally {
    /// Returns an empty tally.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            assertions: 0,
            failures: 0,
            first_failure: None,
        }
    }

    fn observe(&mut self, ok: bool, message: impl Into<String>) {
        self.assertions += 1;
        if !ok {
            self.failures += 1;
            if self.first_failure.is_none() {
                self.first_failure = Some(message.into());
            }
        }
    }
}

/// Resets the current thread's test tally. Call this immediately
/// before invoking a test function.
pub fn reset_test_tally() {
    TEST_TALLY.with(|cell| *cell.borrow_mut() = TestTally::new());
}

/// Returns a snapshot of the tally accumulated since the last reset.
#[must_use]
pub fn take_test_tally() -> TestTally {
    TEST_TALLY.with(|cell| cell.replace(TestTally::new()))
}

fn observe_assertion(ok: bool, message: String) {
    TEST_TALLY.with(|cell| cell.borrow_mut().observe(ok, message));
}

thread_local! {
    /// Most-recent `<file>:<line>:<col>` of an assertion-shaped
    /// builtin call. Set by the interpreter just before each
    /// `check*` invocation; read by the assertion builtins to
    /// stamp the location into the failure message.
    static ASSERTION_LOCATION: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// Records the source location of the assertion currently being
/// evaluated. The interpreter calls this before dispatching to
/// `testing::check*`.
pub fn set_assertion_location(location: Option<String>) {
    ASSERTION_LOCATION.with(|cell| *cell.borrow_mut() = location);
}

fn current_assertion_location() -> Option<String> {
    ASSERTION_LOCATION.with(|cell| cell.borrow().clone())
}

fn write_stdout(text: &str) {
    STDOUT_WRITER.with(|cell| (cell.get())(text));
}

fn write_stderr(text: &str) {
    STDERR_WRITER.with(|cell| (cell.get())(text));
}

fn builtin_repl_discard(_: &[Value]) -> RuntimeResult<Value> {
    Ok(Value::Unit)
}

fn builtin_println(args: &[Value]) -> RuntimeResult<Value> {
    // The line and its terminator reach the writer as one call, so two
    // goroutines printing at once produce two lines rather than one
    // spliced pair.
    let mut rendered = render_args(args);
    rendered.push('\n');
    write_stdout(&rendered);
    Ok(Value::Unit)
}

fn builtin_print(args: &[Value]) -> RuntimeResult<Value> {
    write_stdout(&render_args(args));
    Ok(Value::Unit)
}
