//! Subcommand implementations for `gos`.
//!
//! `main.rs` parses the `crate::cli::Cli` enum and dispatches each
//! variant into the matching module here. Keeping the per-command
//! logic in dedicated files makes `main.rs` a routing table - the
//! place to look when a flag stops landing where you expect.

pub(crate) mod attr_walk;
pub(crate) mod audit_cmd;
pub(crate) mod bench;
pub(crate) mod bindgen;
pub(crate) mod build;
pub(crate) mod cache;
pub(crate) mod check;
pub(crate) mod clean;
pub(crate) mod env_cmd;
pub(crate) mod explain;
pub mod feature_status;
pub(crate) mod fix_cmd;
pub(crate) mod fmt_cmd;
pub(crate) mod fuzz;
pub(crate) mod lint_cmd;
pub(crate) mod lsp_cmd;
pub(crate) mod mcp_cmd;
pub(crate) mod parse;
pub(crate) mod pkg;
pub(crate) mod run;
pub(crate) mod scaffold;
pub(crate) mod skill_prompt;
pub(crate) mod test;
pub(crate) mod traceback;
pub(crate) mod watch;

pub(crate) use test::TestOpts;

/// Native stack reserved for the main thread that runs the bytecode VM.
///
/// Shared with the goroutine worker threads in `gossamer-interp` so
/// every VM-executing thread has the same explicit reserve: the OS
/// main-thread stack is platform-dependent (8 MiB on Linux, roughly
/// 1 MiB on Windows), the VM's native dispatch and in-process JIT both
/// grow the real machine stack, and `MAX_CALL_DEPTH` only yields a
/// clean error if its frames fit before the OS guard page.
const VM_STACK_BYTES: usize = gossamer_interp::VM_THREAD_STACK_BYTES;

/// Runs `f` on a dedicated thread with the VM native stack reserve
/// ([`VM_STACK_BYTES`]) and returns its result. A panic inside `f` is
/// propagated to the caller unchanged. Used by `gos test`, `gos bench`, the
/// REPL, and the comptime fold, whose bytecode-VM evaluation would otherwise
/// run on the `build` / `check` main thread below the compiler's own frames.
pub(crate) fn with_vm_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .name("gos-vm".to_string())
        .stack_size(VM_STACK_BYTES)
        .spawn(move || {
            // Install the native fault handler on the VM thread itself.
            // Only the goroutine scheduler's worker threads install it
            // otherwise, so a program that spawns no goroutines runs the
            // bytecode VM and in-process JIT with no handler at all - a hard
            // fault inside JIT-compiled code (or a native stack overflow) then
            // exits opaquely. Installing here gives every `gos` / `test`
            // / `bench` / REPL execution the stack-overflow backstop and the
            // JIT fault breadcrumb. Idempotent and process-wide-safe.
            gossamer_runtime::stack_guard::install_stack_guard();
            // Arm the byte-budget recursion guard at this thread's shallowest
            // point, budgeted for its native stack. `apply()` consults it
            // before the frame-count cap so a hot JIT-compiled recursive body
            // - which grows the real OS stack rather than the heap frame pool
            // MAX_CALL_DEPTH bounds - raises a clean GX0008 before the guard
            // page instead of faulting opaquely. The goroutine workers arm
            // their own; this covers the main `gos` / `test` / `bench`
            // execution thread.
            gossamer_coro::arm_stack_guard(VM_STACK_BYTES - gossamer_coro::STACK_GUARD_MARGIN);
            f()
        })
        .expect("spawn VM execution thread")
        .join()
        .unwrap_or_else(|payload| std::panic::resume_unwind(payload))
}

/// Runs `f` directly on the calling (process main) thread, installing the
/// native fault handler and arming the recursion guard for the stack left
/// below this point. A program's `main` runs here on the bytecode VM as it
/// does in a native build, so native libraries that mandate the process main
/// thread (GLFW, Cocoa, Metal on macOS) behave the same on every tier, with
/// the same main-thread stack a compiled program gets.
pub(crate) fn on_main_thread<T>(f: impl FnOnce() -> T) -> T {
    gossamer_runtime::stack_guard::install_stack_guard();
    crate::arm_main_thread_guard();
    f()
}
