//! SIGQUIT goroutine stack dump.
//!
//! Pressing Ctrl-\ on a Gossamer process (or sending it SIGQUIT)
//! prints a Go-format dump of every live goroutine's stack to
//! stderr, then exits non-zero. This is the single most useful
//! production-incident diagnostic - without it, a hung service
//! is opaque.
//!
//! The handler runs on a dedicated relay thread (signal-hook's
//! safe abstraction over `sigaction`), so the printing logic is
//! free to allocate / take locks. Ordinary signal-handler async-
//! safety constraints don't apply.
//!
//! Output format mirrors Go's runtime stack dump closely enough
//! that existing tools (`stackparse`, `goroutine-stack-summarizer`,
//! grep) read it without modification:
//!
//! ```text
//! goroutine 17 [running]:
//!   handle_request()
//!         main.gos:128
//!   main()
//!         main.gos:18
//!
//! goroutine 18 [chan receive]:
//!   ...
//! ```
//!
//! A goroutine's frames are the ones its code registered: a debug build
//! pushes one per call with its file and line, a release build records
//! only the function the goroutine entered. The panic trace of a release
//! build walks the machine stack instead ([`render_native_panic_trace`]).

use std::io::Write;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

/// A Gossamer string constant in the program image, decoded on demand.
///
/// A codegen prologue names its frame with pointers into the module's
/// read-only string pool, which outlive every frame that records them.
#[derive(Clone, Copy)]
pub struct ImageStr(*const std::ffi::c_char);

// SAFETY: the pointee is a read-only string constant in the program
// image, so it is valid on every thread for the process lifetime.
unsafe impl Send for ImageStr {}
// SAFETY: as above - the pointee is immutable for the process lifetime.
unsafe impl Sync for ImageStr {}

impl ImageStr {
    /// Wraps a pointer to a Gossamer string constant in the program image.
    ///
    /// # Safety
    /// `ptr` is null, or points at a string constant in the program image
    /// that stays mapped and unmodified for the process lifetime.
    #[must_use]
    pub const unsafe fn new(ptr: *const std::ffi::c_char) -> Self {
        Self(ptr)
    }

    /// Whether the constant is absent or empty.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.as_str().is_empty()
    }

    /// Decodes the constant's text. Invalid UTF-8 reads as `""`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        // SAFETY: the constructor's contract makes the pointee a live
        // program-image string constant for the process lifetime.
        unsafe { crate::c_abi::gos_str_arg_text(self.0) }
    }
}

impl std::fmt::Display for ImageStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::fmt::Debug for ImageStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.as_str(), f)
    }
}

/// One frame on a goroutine's call stack. Pushed on every function
/// entry by [`stack_push`] and popped on return by [`stack_pop`].
#[derive(Debug, Clone, Copy)]
pub struct Frame {
    /// Symbolicated function name (e.g. `main::handle_request`).
    pub function: ImageStr,
    /// Source file path. Empty if no debug info is available.
    pub file: ImageStr,
    /// 1-based source line of the most recent statement executed
    /// in this frame. Updated by [`set_position`] at MIR-statement
    /// granularity.
    pub line: u32,
    /// 1-based character column of that statement, or 0 when unknown.
    pub column: u32,
}

/// Per-goroutine record published into the runtime's introspection
/// table. The scheduler updates this on park / unpark / spawn /
/// finish; SIGQUIT handler walks the table to render the dump.
#[derive(Debug, Clone)]
pub struct GoroutineInfo {
    /// Stable goroutine identifier.
    pub gid: u32,
    /// Last-known wait reason (`"running"`, `"chan receive"`, ...).
    pub state: &'static str,
    /// Symbolicated function name the goroutine was last running in.
    /// Empty when no frame has been recorded yet.
    /// Mirrors `frames.last().function` for backward-compatible
    /// consumers that only need the topmost name.
    pub function: String,
    /// Source file path captured from DWARF, when available.
    /// Mirrors `frames.last().file`.
    pub file: String,
    /// 1-based line number captured from DWARF.
    /// Mirrors `frames.last().line`.
    pub line: u32,
    /// Full call stack (outermost frame first). Empty until the
    /// codegen-emitted prologue runs `gos_rt_stack_push`.
    pub frames: Vec<Frame>,
}

#[derive(Default)]
struct Registry {
    infos: Mutex<std::collections::BTreeMap<u32, GoroutineInfo>>,
    next_id: AtomicU64,
}

static REGISTRY: OnceLock<Registry> = OnceLock::new();
/// Set the first time any goroutine records a shadow-stack frame. Until then
/// the scheduler's per-step goroutine binding has no frames to migrate and
/// skips the registry entirely.
static FRAMES_RECORDED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn registry() -> &'static Registry {
    REGISTRY.get_or_init(Registry::default)
}

/// Allocates a fresh goroutine id for tracking purposes. Distinct
/// from the scheduler's `Gid` because that one wraps `u32` and we
/// want a wider counter for diagnostics in long-running processes.
#[must_use]
pub fn next_id() -> u32 {
    let raw = registry().next_id.fetch_add(1, Ordering::Relaxed);
    u32::try_from(raw & 0xFFFF_FFFF).unwrap_or(u32::MAX)
}

/// Publishes a fresh entry for `gid` (called when the scheduler
/// spawns a goroutine).
pub fn register(gid: u32, function: impl Into<String>) {
    let mut g = registry().infos.lock();
    g.insert(
        gid,
        GoroutineInfo {
            gid,
            state: "running",
            function: function.into(),
            file: String::new(),
            line: 0,
            frames: Vec::new(),
        },
    );
}

thread_local! {
    static ACTIVE_GID: std::cell::Cell<u32> = const { std::cell::Cell::new(u32::MAX) };
    /// The active goroutine's call stack while it runs on THIS
    /// thread. `stack_push` / `stack_pop` touch it lock-free on every
    /// call; it is checked out from / into the registry at
    /// [`set_active_gid`] (the scheduler's park / resume boundary) so
    /// the frames migrate with the goroutine across worker threads.
    /// The previous design pushed every frame into a process-global
    /// `Mutex<BTreeMap>`, serialising every function call in the
    /// program through one lock - this removes that lock from the
    /// per-call path entirely.
    static LOCAL_FRAMES: std::cell::RefCell<Vec<Frame>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Binds goroutine `gid` to the current OS thread. The scheduler
/// calls this at step entry (resume) and with `u32::MAX` at step
/// exit (park). `u32::MAX` is the sentinel "no goroutine - main
/// thread".
///
/// This is the migration boundary for the lock-free shadow stack:
/// the outgoing goroutine's frames are checked back into the registry
/// (so a SIGQUIT dump can render a parked goroutine), and the
/// incoming goroutine's saved frames are checked out to this thread's
/// `LOCAL_FRAMES`. The registry lock is taken at most twice here -
/// per step, never per call.
pub fn set_active_gid(gid: u32) {
    let old = ACTIVE_GID.with(std::cell::Cell::get);
    if old == gid {
        return;
    }
    if !FRAMES_RECORDED.load(Ordering::Relaxed) {
        // No shadow-stack frame exists process-wide, so there is nothing to
        // check in or out and the registry lock stays off the per-step path.
        // The compiled tier never pushes frames, so this is its steady state.
        ACTIVE_GID.with(|cell| cell.set(gid));
        return;
    }
    if old != u32::MAX {
        let frames = LOCAL_FRAMES.with(|f| std::mem::take(&mut *f.borrow_mut()));
        let mut g = registry().infos.lock();
        if let Some(info) = g.get_mut(&old) {
            if let Some(top) = frames.last() {
                info.function.clear();
                info.function.push_str(top.function.as_str());
                info.file.clear();
                info.file.push_str(top.file.as_str());
                info.line = top.line;
            }
            info.frames = frames;
        }
    }
    ACTIVE_GID.with(|cell| cell.set(gid));
    if gid == u32::MAX {
        LOCAL_FRAMES.with(|f| f.borrow_mut().clear());
    } else {
        let frames = {
            let mut g = registry().infos.lock();
            g.get_mut(&gid).map(|info| std::mem::take(&mut info.frames))
        };
        LOCAL_FRAMES.with(|f| *f.borrow_mut() = frames.unwrap_or_default());
    }
}

/// Returns the goroutine id currently bound to this thread, or
/// `None` if no goroutine is running (main thread).
#[must_use]
pub fn active_gid() -> Option<u32> {
    ACTIVE_GID.with(|cell| {
        let v = cell.get();
        if v == u32::MAX { None } else { Some(v) }
    })
}

/// Pushes a new frame onto the active goroutine's call stack.
/// Called from debug-profile compiled prologues through
/// `gos_rt_stack_push`. Lock-free: it touches only this thread's
/// `LOCAL_FRAMES`. A release build emits no such call and recovers
/// traces by unwinding the real machine stack
/// ([`render_native_panic_trace`]).
pub fn stack_push(function: ImageStr, file: ImageStr, line: u32, column: u32) {
    let frame = Frame {
        function,
        file,
        line,
        column,
    };
    FRAMES_RECORDED.store(true, Ordering::Relaxed);
    LOCAL_FRAMES.with(|f| f.borrow_mut().push(frame));
}

/// Pops the topmost frame from the active goroutine's call stack.
/// Lock-free. Tolerates over-pop (no-op when the stack is empty) so
/// unwinding past an aborted frame doesn't crash the runtime.
pub fn stack_pop() {
    LOCAL_FRAMES.with(|f| {
        f.borrow_mut().pop();
    });
}

/// Snapshots the active goroutine's call stack (outermost first).
/// Used by the panic helper to render the failing frame chain inline
/// with the diagnostic. Reads this thread's `LOCAL_FRAMES`, so it
/// reflects the goroutine that is panicking on the calling thread.
#[must_use]
pub fn active_frames() -> Vec<Frame> {
    LOCAL_FRAMES.with(|f| f.borrow().clone())
}

/// Updates the wait state of an already-registered goroutine.
pub fn set_state(gid: u32, state: &'static str) {
    let mut g = registry().infos.lock();
    if let Some(info) = g.get_mut(&gid) {
        info.state = state;
    }
}

/// Updates the line and column of the topmost call-stack frame for
/// the active goroutine. Lock-free (this thread's frames only); called
/// at MIR-statement granularity by codegen so panic traces carry
/// the precise failing position, not just the function-entry line.
pub fn set_active_line(line: u32, column: u32) {
    LOCAL_FRAMES.with(|f| {
        if let Some(top) = f.borrow_mut().last_mut() {
            top.line = line;
            top.column = column;
        }
    });
}

/// Updates the latest source position of a goroutine - called by
/// the codegen safepoint poll when DWARF info is available, or by
/// the interpreter on every step boundary. Also updates the
/// topmost call-stack frame's line so panic dumps show the line
/// of the most recent statement, not the function entry.
pub fn set_position(gid: u32, file: impl Into<String>, line: u32) {
    let mut g = registry().infos.lock();
    if let Some(info) = g.get_mut(&gid) {
        if let Some(top) = info.frames.last_mut() {
            top.line = line;
        }
        info.file = file.into();
        info.line = line;
    }
}

/// Removes the entry when the goroutine finishes.
pub fn unregister(gid: u32) {
    registry().infos.lock().remove(&gid);
}

/// Snapshots every live goroutine. Used by the SIGQUIT handler
/// and by `runtime::all_goroutines()`.
#[must_use]
pub fn snapshot() -> Vec<GoroutineInfo> {
    registry().infos.lock().values().cloned().collect()
}

/// Renders a Go-style stack dump into a writer. Returns the number
/// of bytes written.
///
/// # Errors
///
/// Returns an error if the underlying writer fails.
pub fn render_to(out: &mut impl Write) -> std::io::Result<usize> {
    let mut written = 0;
    let infos = snapshot();
    let _ = writeln!(out, "SIGQUIT: dumping {} goroutine(s)", infos.len()).map(|()| written += 1);
    for info in infos {
        let header = format!(
            "\ngoroutine {gid} [{state}]:\n",
            gid = info.gid,
            state = info.state,
        );
        out.write_all(header.as_bytes())?;
        written += header.len();
        if info.frames.is_empty() {
            let func_line = if info.function.is_empty() {
                "  <unknown>()\n".to_string()
            } else {
                format!("  {}()\n", info.function)
            };
            out.write_all(func_line.as_bytes())?;
            written += func_line.len();
            if !info.file.is_empty() {
                let pos = format!(
                    "        {file}:{line}\n",
                    file = info.file,
                    line = info.line
                );
                out.write_all(pos.as_bytes())?;
                written += pos.len();
            }
            // No per-call shadow frames (release compiled tier). The
            // dump carries this goroutine's identity, wait state, and
            // entry function from the spawn/park registry: the relay
            // thread cannot unwind another goroutine's suspended
            // coroutine stack, and its own stack names none of that
            // goroutine's frames.
        } else {
            // Most recent call first, as Go's stack dump prints it.
            for frame in info.frames.iter().rev() {
                let func_line = format!("  {}()\n", frame.function);
                out.write_all(func_line.as_bytes())?;
                written += func_line.len();
                if !frame.file.is_empty() {
                    let pos = format!(
                        "        {file}:{line}\n",
                        file = frame.file,
                        line = frame.line
                    );
                    out.write_all(pos.as_bytes())?;
                    written += pos.len();
                }
            }
        }
    }
    Ok(written)
}

/// Heading of every panic call-stack trailer, shared with the VM's report so
/// a trace reads the same whichever tier raised it.
pub const PANIC_TRACE_HEADER: &str = "  call stack (outermost first):\n";

/// Renders the active goroutine's call stack as a panic trailer, outermost
/// frame first. Used by `gos_rt_panic` to inline the trace with the
/// diagnostic.
#[must_use]
pub fn render_active_panic_trace() -> String {
    let frames = active_frames();
    if frames.is_empty() {
        return String::new();
    }
    let mut out = String::from(PANIC_TRACE_HEADER);
    for frame in &frames {
        out.push_str("    at ");
        out.push_str(frame.function.as_str());
        if !frame.file.is_empty() {
            out.push_str(" (");
            out.push_str(frame.file.as_str());
            out.push(':');
            out.push_str(&frame.line.to_string());
            if frame.column != 0 {
                out.push(':');
                out.push_str(&frame.column.to_string());
            }
            out.push(')');
        }
        out.push('\n');
    }
    out
}

/// Returns true if `symbol` names runtime / panic / std machinery
/// rather than a user Gossamer function. Used to trim the real-stack
/// backtrace down to the gos call chain. Conservative: anything that
/// is clearly host scaffolding is dropped; unknown bare names are
/// kept (they are almost certainly gos functions).
#[cfg(not(target_arch = "wasm32"))]
fn is_runtime_frame(symbol: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "gos_rt_",
        "gossamer",
        "std::",
        "std[",
        "core::",
        "alloc::",
        "backtrace",
        "corosensei",
        "stack_init_trampoline",
        "__rust",
        "rust_begin_unwind",
        "_start",
        "<",
    ];
    const CONTAINS: &[&str] = &["panic", "begin_unwind", "abort"];
    PREFIXES.iter().any(|p| symbol.starts_with(p)) || CONTAINS.iter().any(|c| symbol.contains(c))
}

/// Renders the active thread's real machine-stack backtrace as a
/// gos-focused panic trace. Used by `gos_rt_panic` on a release build,
/// which keeps no per-call shadow stack: frames are recovered by
/// unwinding the live stack and symbolicating through the binary's
/// symbol table (`gos build --release` keeps `.symtab`), and through its
/// DWARF when `-g` kept that. Returns empty when capture or
/// symbolication yields nothing (e.g. a fully `--strip-all` binary).
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub fn render_native_panic_trace() -> String {
    // std's own capture and symbolizer are linked into every binary for
    // its panic hook; walking the stack through them keeps a second
    // symbolizer out of the binary.
    let captured = std::backtrace::Backtrace::force_capture();
    let symbols = backtrace_symbols(&format!("{captured:#}"));
    let mut lines: Vec<String> = Vec::new();
    for (sym, location) in &symbols {
        // Runtime / unwinder machinery is filtered everywhere, not just
        // at the top: a goroutine stack bottoms out in coroutine
        // trampoline frames rather than `gos_main`, and printing those
        // leaks runtime internals into a user-facing panic report.
        if is_runtime_frame(sym) {
            continue;
        }
        let entry = sym == "gos_main" || sym == "main";
        // The program's `main` is emitted as `gos_main`; the report names
        // the function the source declares.
        let mut line = format!("    at {}", if entry { "main" } else { sym.as_str() });
        if let Some(location) = location {
            line.push_str(" (");
            line.push_str(location);
            line.push(')');
        }
        line.push('\n');
        lines.push(line);
        // The program entry frame is the natural bottom of the gos
        // chain; everything below is libc / rt startup.
        if entry {
            break;
        }
    }
    if lines.is_empty() {
        return String::new();
    }
    let mut out = String::from(PANIC_TRACE_HEADER);
    for line in lines.iter().rev() {
        out.push_str(line);
    }
    out
}

/// The symbols of a backtrace in std's `{:#}` rendering, innermost first,
/// each with the `file:line[:column]` it was resolved to (the file named by
/// its last path component). An inlined call is a symbol of its own, as
/// std lists it; an unresolved frame, which names no symbol, is skipped.
#[cfg(not(target_arch = "wasm32"))]
fn backtrace_symbols(rendered: &str) -> Vec<(String, Option<String>)> {
    let mut symbols: Vec<(String, Option<String>)> = Vec::new();
    for line in rendered.lines() {
        let trimmed = line.trim_start();
        if let Some(location) = trimmed.strip_prefix("at ") {
            if let Some(last) = symbols.last_mut()
                && last.1.is_none()
            {
                let file_start = location.rfind(['/', '\\']).map_or(0, |at| at + 1);
                last.1 = Some(location[file_start..].to_string());
            }
            continue;
        }
        let Some((index, name)) = trimmed.split_once(": ") else {
            continue;
        };
        if index.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        // The full rendering leads each frame with its address:
        // `0x55d1c0 - name`, or the address alone when nothing resolved.
        let name = name.trim();
        let name = match name.split_once(" - ") {
            Some((address, symbol)) if address.starts_with("0x") => symbol.trim(),
            _ if name.starts_with("0x") => continue,
            _ => name,
        };
        if name.is_empty() || name == "<unknown>" {
            continue;
        }
        symbols.push((name.to_string(), None));
    }
    symbols
}

/// wasm32 has no machine-stack unwinder (`backtrace` does not build for
/// wasm32-unknown-unknown), and the VM keeps its own shadow call stack
/// anyway, so the native-trace path renders nothing in the playground.
#[cfg(target_arch = "wasm32")]
#[must_use]
pub fn render_native_panic_trace() -> String {
    String::new()
}

/// Installs the SIGQUIT handler. Idempotent.
///
/// SIGQUIT delivery itself is owned by `gossamer_std::signal`'s
/// single blocking dispatcher thread - when it sees SIGQUIT, it
/// calls [`render_to`] directly. This entry point stays as a
/// no-op to preserve the `install_handler()` call sites that the
/// scheduler boot path uses.
#[cfg(unix)]
pub fn install_handler() {}

/// No-op on Windows - the `SetConsoleCtrlHandler` dispatcher in
/// `gossamer_std::signal` already calls [`render_to`] directly on
/// `CTRL_BREAK_EVENT`. Other non-unix targets have no equivalent.
#[cfg(not(unix))]
pub fn install_handler() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_round_trips_a_goroutine() {
        let gid = next_id();
        register(gid, "test::handle");
        set_state(gid, "chan receive");
        set_position(gid, "main.gos", 42);
        let snap = snapshot();
        let entry = snap
            .iter()
            .find(|info| info.gid == gid)
            .expect("registered entry");
        assert_eq!(entry.state, "chan receive");
        assert_eq!(entry.function, "test::handle");
        assert_eq!(entry.line, 42);
        unregister(gid);
        assert!(!snapshot().iter().any(|info| info.gid == gid));
    }

    #[test]
    fn render_to_writes_some_output() {
        let gid = next_id();
        register(gid, "test::handle");
        let mut buf = Vec::new();
        let n = render_to(&mut buf).unwrap();
        assert!(n > 0);
        let s = String::from_utf8_lossy(&buf);
        assert!(s.contains("goroutine"));
        assert!(s.contains("test::handle"));
        unregister(gid);
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod backtrace_text_tests {
    use super::backtrace_symbols;

    #[test]
    fn symbols_carry_the_file_they_resolved_to() {
        let rendered = "   0:     0x7085eb - gossamer_runtime::c_abi::panic::gos_rt_panic\n             at /src/panic.rs:143:5\n   1:     0x7085ec - inner\n             at /work/p.gos:2:5\n   2: gos_main\n   3:     0x7085ed - <unknown>\n   4:     0x55d1c0\n";
        assert_eq!(
            backtrace_symbols(rendered),
            vec![
                (
                    "gossamer_runtime::c_abi::panic::gos_rt_panic".to_string(),
                    Some("panic.rs:143:5".to_string())
                ),
                ("inner".to_string(), Some("p.gos:2:5".to_string())),
                ("gos_main".to_string(), None),
            ]
        );
    }
}
