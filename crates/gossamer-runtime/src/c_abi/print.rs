#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::similar_names)]
#![allow(clippy::many_single_char_names)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::same_length_and_capacity)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::cast_ptr_alignment)]
#![allow(clippy::ptr_as_ptr)]
#![allow(static_mut_refs)]
#![allow(unused_unsafe)]
#![allow(clippy::wildcard_imports)]

use std::os::raw::c_char;

// ---------------------------------------------------------------
// Print helpers (variadic-printf workaround - Cranelift 0.123
// has no variadic-call ABI support, so every formatted print
// routes through a fixed-signature wrapper.)
// ---------------------------------------------------------------

// Process-global 64 KiB stdout buffer. The buffer's lifetime is
// the whole process, but every entry into the inline byte-write
// fast path takes the buffer mutex (`STDOUT_LOCK` below) so two
// goroutines on different OS threads cannot race on
// `GOS_RT_STDOUT_LEN`. The previous design (no lock) tore the
// length under any multi-thread output and is the C3 finding in
// `~/dev/contexts/lang/adversarial_analysis.md`.
//
// Performance: parking_lot's uncontended acquire/release is ~10 ns
// total. The LLVM lowerer takes the lock once per inline write
// region (a single byte, or a contiguous range - the array
// writer in `lower_stream_write_byte_array_inline` packs up to
// 65 K bytes per acquire). For fasta's 60-byte lines that is
// ~4 M acquires across 250 MB of output → ~40 ms of total mutex
// overhead, lost in the noise against the ~2 s of I/O.
//
// `STDOUT_LOCK` is exposed to the codegen via
// `gos_rt_stdout_acquire` / `gos_rt_stdout_release`. The codegen
// pairs them around any inline access; the runtime helpers
// (`gos_rt_print_*`) acquire it via the safe `lock()` path.
/// Hot-path stdout buffer capacity. Codegen inlines a buffer
/// length check against this value, so it must stay in sync
/// with `GOS_RT_STDOUT_BYTES`'s length declared in
/// `gossamer-codegen-llvm::emit` (see the `@GOS_RT_STDOUT_BYTES`
/// extern there) and with the inline `icmp ... 8192` checks
/// emitted by `lower::Lowerer`.
///
/// Sized for the coalescing job it actually does: a formatted line
/// arrives as several writes (literal segments, an integer, the
/// newline) and leaves as one syscall, since `write_stdout_locked`
/// drains on the newline. What accumulates across many writes is the
/// byte-at-a-time and byte-range output the codegen inlines against
/// this same buffer. The previous 64 KiB cost 56 KiB of BSS in every
/// Gossamer binary for what is almost always wasted slack.
pub const STDOUT_BUF_SIZE: usize = 8 * 1024;

/// Process-global mutex protecting [`GOS_RT_STDOUT_BYTES`] and
/// [`GOS_RT_STDOUT_LEN`]. Held for the duration of any inline
/// byte-write region (codegen-emitted) or any
/// `gos_rt_print_*` / `gos_rt_println` runtime helper. The
/// underlying lock is non-recursive; reentrant nesting on the
/// same OS thread routes through the per-thread depth counter
/// below so `gos_rt_println("foo")` (which acquires inside the
/// helper) can be called from inside an inline write region
/// (which already acquired) without deadlocking.
static STDOUT_LOCK: parking_lot::RawMutex = {
    use parking_lot::lock_api::RawMutex;
    parking_lot::RawMutex::INIT
};

thread_local! {
    /// Reentrancy counter for [`STDOUT_LOCK`] on the current
    /// thread. Bumped on each `acquire`, dropped on each
    /// `release`. The mutex is taken on the 0→1 transition and
    /// released on the 1→0 transition; intermediate transitions
    /// are no-ops at the lock layer. This makes
    /// `gos_rt_stdout_acquire` / `_release` recursion-safe even
    /// though `parking_lot::RawMutex` itself is not.
    static STDOUT_LOCK_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Internal entry point: increments the per-thread reentrancy
/// counter, taking the mutex on the outermost acquire. Called by
/// every code path that touches the stdout buffer.
fn stdout_lock_acquire() {
    STDOUT_LOCK_DEPTH.with(|depth| {
        let n = depth.get();
        if n == 0 {
            use parking_lot::lock_api::RawMutex;
            STDOUT_LOCK.lock();
        }
        depth.set(n + 1);
    });
}

/// Internal entry point: decrements the per-thread reentrancy
/// counter, releasing the mutex when the counter returns to zero.
/// Calling this without a matching `stdout_lock_acquire` is a
/// programming error; debug builds assert.
fn stdout_lock_release() {
    STDOUT_LOCK_DEPTH.with(|depth| {
        let n = depth.get();
        debug_assert!(n > 0, "stdout_lock_release without acquire");
        if n == 1 {
            use parking_lot::lock_api::RawMutex;
            // SAFETY: invariant - `stdout_lock_acquire` ran on
            // the same thread when `n` was 0, taking the lock.
            unsafe { STDOUT_LOCK.unlock() };
        }
        depth.set(n.saturating_sub(1));
    });
}

/// Whether this OS thread currently owns the stdout-buffer lock. A caller
/// holding that lock cannot park for a scheduler handoff: peer goroutines may
/// be occupying every worker while waiting for the same lock.
fn stdout_lock_is_held() -> bool {
    STDOUT_LOCK_DEPTH.with(|depth| depth.get() > 0)
}

/// Sync-Sealed `[u8; STDOUT_BUF_SIZE]` newtype used as the
/// storage cell of [`GOS_RT_STDOUT_BYTES`]. `repr(transparent)`
/// keeps the linker symbol's size and alignment identical to a
/// bare `[u8; STDOUT_BUF_SIZE]`, so the LLVM lowerer's
/// `@GOS_RT_STDOUT_BYTES = external local_unnamed_addr global
/// [8192 x i8]` reference resolves at link time exactly as
/// before. `UnsafeCell` carries the documented interior-mutability
/// contract; the manual `Sync` impl declares that all access is
/// serialised by `STDOUT_LOCK` / `STDOUT_LOCK_DEPTH`.
#[repr(transparent)]
pub struct GosRtStdoutBytes(pub core::cell::UnsafeCell<[u8; STDOUT_BUF_SIZE]>);

// SAFETY: every `&self` use of this static reaches into the
// `UnsafeCell` via raw pointers under one of the access paths
// audited below. Mutation is gated by `STDOUT_LOCK`'s per-thread
// depth counter; reads from the inline LLVM fast path acquire the
// same lock via `gos_rt_stdout_acquire` before dereferencing.
unsafe impl Sync for GosRtStdoutBytes {}

/// Sync-Sealed `usize` newtype used as the storage cell of
/// [`GOS_RT_STDOUT_LEN`]. Same rationale as
/// [`GosRtStdoutBytes`]: `repr(transparent)` preserves the
/// linker symbol shape so the inline LLVM fast path's
/// `load i64, ptr @GOS_RT_STDOUT_LEN` and matching `store`
/// resolve unchanged.
#[repr(transparent)]
pub struct GosRtStdoutLen(pub core::cell::UnsafeCell<usize>);

// SAFETY: same contract as `GosRtStdoutBytes` - all access
// serialised by `STDOUT_LOCK`. The inline LLVM path holds
// `gos_rt_stdout_acquire` before reaching this symbol.
unsafe impl Sync for GosRtStdoutLen {}

/// Process-global stdout buffer storage. The LLVM backend
/// emits inline fast-path code that loads
/// `GOS_RT_STDOUT_LEN`, stores the new byte at offset
/// `bytes[len]`, and bumps the length - bypassing the FFI
/// call and saving the per-call overhead that dominates
/// character-at-a-time output (fasta hot loop). Access from any
/// thread requires the `STDOUT_LOCK` mutex be held.
#[unsafe(no_mangle)]
pub static GOS_RT_STDOUT_BYTES: GosRtStdoutBytes =
    GosRtStdoutBytes(core::cell::UnsafeCell::new([0; STDOUT_BUF_SIZE]));

/// Current write offset in `GOS_RT_STDOUT_BYTES`. The inline
/// fast path reads this, stores the byte, and writes it back.
/// Access from any thread requires the `STDOUT_LOCK` mutex be
/// held.
#[unsafe(no_mangle)]
pub static GOS_RT_STDOUT_LEN: GosRtStdoutLen = GosRtStdoutLen(core::cell::UnsafeCell::new(0));

/// Acquires the process-wide stdout buffer lock. Codegen wraps
/// every inline byte-write region in matched
/// [`gos_rt_stdout_acquire`] / [`gos_rt_stdout_release`] calls so
/// concurrent goroutines on different OS threads serialise their
/// writes against the buffer. Re-entry on the same thread is
/// supported via the per-thread `STDOUT_LOCK_DEPTH` counter so
/// the runtime FFI helpers (`gos_rt_print_*`, `gos_rt_println`,
/// `gos_rt_flush_stdout`) remain safe to call from inside an
/// outer acquire.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_stdout_acquire() {
    ffi_entry!((), {
        stdout_lock_acquire();
    });
}

/// Releases the process-wide stdout buffer lock acquired by a
/// matching [`gos_rt_stdout_acquire`]. Calling this without a
/// prior acquire is a programming error; the codegen always
/// emits matched pairs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_stdout_release() {
    ffi_entry!((), {
        stdout_lock_release();
    });
}

/// Convenience RAII guard: acquires `STDOUT_LOCK` for the duration
/// of the current scope. Reentrant via the per-thread depth
/// counter so a runtime helper that holds a guard can call
/// another runtime helper that also acquires.
pub struct StdoutGuard;

impl StdoutGuard {
    pub fn acquire() -> Self {
        stdout_lock_acquire();
        Self
    }
}

impl Drop for StdoutGuard {
    fn drop(&mut self) {
        stdout_lock_release();
    }
}

/// Writes terminal bytes without pinning a scheduler worker. Unlocked calls
/// hand off the OS write and park the goroutine. A caller holding a
/// [`StdoutGuard`] writes inline instead: parking while it owns the global
/// buffer lock can deadlock when peer goroutines have occupied every worker
/// waiting for that same lock. Copying for the handoff is intentional because
/// the caller's buffer can be reused immediately after it starts.
pub fn write_terminal(fd: i32, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let label = match fd {
        1 => "stdout-write",
        2 => "stderr-write",
        _ => return,
    };
    if stdout_lock_is_held() {
        let _ = write_terminal_direct(fd, bytes);
        return;
    }
    let bytes = bytes.to_vec();
    let _ = crate::sched_global::run_blocking(label, move || write_terminal_direct(fd, &bytes));
}

fn write_terminal_direct(fd: i32, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let written = match fd {
        1 => std::io::stdout().lock().write_all(bytes),
        2 => std::io::stderr().lock().write_all(bytes),
        _ => unreachable!("terminal fd was validated before dispatch"),
    };
    if let Err(err) = &written {
        end_on_closed_stdio(err);
    }
    written
}

/// Ends the process when a write to its stdout or stderr found the reader
/// gone, the way the same write ends a Unix tool whose output was piped into
/// `head`.
///
/// The process ignores `SIGPIPE` so that a closed socket or child pipe is an
/// error the program can handle; the standard streams are the exception,
/// since nothing is left to read what the program goes on to write.
pub fn end_on_closed_stdio(err: &std::io::Error) {
    if err.kind() != std::io::ErrorKind::BrokenPipe {
        return;
    }
    // SAFETY: restoring a signal's default disposition and raising it are
    // async-signal-safe libc calls with no memory arguments.
    #[cfg(all(unix, not(miri)))]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        libc::raise(libc::SIGPIPE);
    }
    // No signal carries this on other hosts, so the process ends with a
    // failing status of its own.
    #[cfg(all(not(unix), not(target_arch = "wasm32")))]
    std::process::exit(1);
}

/// Drives the process's own stdout buffer out to the descriptor.
///
/// A native Gossamer binary enters at `main` and returns from it, so
/// nothing runs the flush a Rust program's entry shim performs on the
/// way out. Text that ends in a newline leaves the line buffer on its
/// own; a `print` with no terminator would otherwise sit there until
/// the process was gone.
fn flush_terminal_stdout() {
    use std::io::Write;

    let flush = || {
        let flushed = std::io::stdout().lock().flush();
        if let Err(err) = &flushed {
            end_on_closed_stdio(err);
        }
        flushed
    };
    if stdout_lock_is_held() {
        let _ = flush();
        return;
    }
    let _ = crate::sched_global::run_blocking("stdout-flush", flush);
}

pub fn raw_write_stdout(bytes: &[u8]) {
    write_terminal(1, bytes);
}

/// Inner mechanic shared by `write_stdout` and any internal
/// caller that already holds `STDOUT_LOCK`. Splitting the lock
/// acquisition from the buffer manipulation lets us avoid
/// re-entering the (non-recursive) `RawMutex` from helpers that
/// already entered through the safe guard.
///
/// A write whose last byte is a newline leaves the buffer before
/// returning, so a program that announces a line and then blocks is
/// visible to whatever is reading its stdout - a pipe, a file, or a
/// terminal alike. The check is on the final byte only: the
/// byte-at-a-time and byte-range writers are the throughput path and
/// never reach here, so no hot loop grows a scan.
pub unsafe fn write_stdout_locked(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let bytes_ptr = GOS_RT_STDOUT_BYTES.0.get();
    let len_ptr = GOS_RT_STDOUT_LEN.0.get();
    let len = unsafe { *len_ptr };
    // Flush and bypass the buffer entirely for chunks that
    // don't fit - a single large chunk costs one syscall
    // either way.
    if bytes.len() >= STDOUT_BUF_SIZE {
        if len > 0 {
            unsafe {
                raw_write_stdout(std::slice::from_raw_parts((*bytes_ptr).as_ptr(), len));
                *len_ptr = 0;
            }
        }
        raw_write_stdout(bytes);
        return;
    }
    if len + bytes.len() > STDOUT_BUF_SIZE {
        unsafe {
            raw_write_stdout(std::slice::from_raw_parts((*bytes_ptr).as_ptr(), len));
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), (*bytes_ptr).as_mut_ptr(), bytes.len());
            *len_ptr = bytes.len();
        }
    } else {
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                (*bytes_ptr).as_mut_ptr().add(len),
                bytes.len(),
            );
            *len_ptr = len + bytes.len();
        }
    }
    if bytes.last() == Some(&b'\n') {
        let pending = unsafe { *len_ptr };
        if pending > 0 {
            unsafe {
                raw_write_stdout(std::slice::from_raw_parts((*bytes_ptr).as_ptr(), pending));
                *len_ptr = 0;
            }
        }
    }
}

pub unsafe fn write_stdout(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let _guard = StdoutGuard::acquire();
    unsafe { write_stdout_locked(bytes) };
}

/// Drains the process-global stdout buffer to FD 1. Pure: carries no
/// diagnostics, so it is safe to call before every interpreter-side
/// write that must observe program order against JIT-buffered output.
pub fn flush_stdout_buffer() {
    let _guard = StdoutGuard::acquire();
    let bytes_ptr = GOS_RT_STDOUT_BYTES.0.get();
    let len_ptr = GOS_RT_STDOUT_LEN.0.get();
    let len = unsafe { *len_ptr };
    if len > 0 {
        unsafe {
            raw_write_stdout(std::slice::from_raw_parts((*bytes_ptr).as_ptr(), len));
            *len_ptr = 0;
        }
    }
}

/// Flushes the process-global stdout buffer. Called by explicit stream flushes,
/// stderr writers that must preserve output order, and process exit.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_flush_stdout() {
    ffi_entry!((), {
        if std::env::var_os("GOS_RC_DEBUG").is_some() {
            let live = crate::c_abi::rc::rc_live_count();
            let shared = crate::c_abi::rc::rc_shared_live_count();
            let reused = crate::c_abi::rc::rc_reuse_count();
            eprintln!("RC_LIVE_AT_EXIT={live} shared_live={shared} reused={reused}");
            if live > 0 && shared > 0 {
                // Cross-goroutine objects are excluded from the per-thread cycle
                // collector, so a shared reference cycle leaks. This is the only
                // leak class the collector cannot reach; break a back-edge with
                // `Weak` to fix it.
                eprintln!(
                    "RC_HINT: {shared} live cross-goroutine object(s) at exit; a shared \
                     reference cycle is not collected - break a back-edge with Weak<T>"
                );
            }
        }
        flush_stdout_buffer();
        flush_terminal_stdout();
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_print_str(s: *const c_char) {
    ffi_entry!((), {
        let bytes = if s.is_null() {
            b"" as &[u8]
        } else {
            unsafe { crate::c_abi::gos_str_arg_bytes(s) }
        };
        unsafe { write_stdout(bytes) };
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_print_i64(n: i64) {
    ffi_entry!((), {
        // Format on the stack - avoid the per-call heap allocation
        // that `n.to_string()` would incur.
        let mut buf = itoa::Buffer::new();
        let text = buf.format(n);
        unsafe { write_stdout(text.as_bytes()) };
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_println_fn_i64(n: i64) -> i64 {
    ffi_entry!(0, {
        unsafe { gos_rt_print_i64(n) };
        unsafe { write_stdout(b"\n") };
        0
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_println_fn_f64(x: f64) -> i64 {
    ffi_entry!(0, {
        unsafe { gos_rt_print_f64(x) };
        unsafe { write_stdout(b"\n") };
        0
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_println_fn_str_word(s: i64) -> i64 {
    ffi_entry!(0, {
        unsafe { gos_rt_print_str(s as usize as *const c_char) };
        unsafe { write_stdout(b"\n") };
        0
    })
}

/// Prints an unsigned 64-bit integer through the buffered
/// stdout path. Distinct from `gos_rt_print_i64` so values
/// `>= 2^63` print without a leading `-` (the sign-extension
/// bug a single shared printer would have).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_print_u64(n: u64) {
    ffi_entry!((), {
        let mut buf = itoa::Buffer::new();
        let text = buf.format(n);
        unsafe { write_stdout(text.as_bytes()) };
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_print_f64(x: f64) {
    ffi_entry!((), {
        // Match the interpreter's `{}` Display output.
        let text = format!("{x}");
        unsafe { write_stdout(text.as_bytes()) };
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_print_bool(b: i32) {
    ffi_entry!((), {
        unsafe { write_stdout(if b != 0 { b"true" } else { b"false" }) };
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_print_char(c: i32) {
    ffi_entry!((), {
        if let Some(ch) = char::from_u32(c as u32) {
            let mut buf = [0u8; 4];
            let s = ch.encode_utf8(&mut buf);
            unsafe { write_stdout(s.as_bytes()) };
        }
    });
}

/// Direct stderr writer used by `eprint`/`eprintln` lowering.
/// Bypasses the stdout buffer. Flushes stdout first so prior
/// `println` output isn't reordered with diagnostic output -
/// matches the language semantics where stderr appears in the
/// expected place relative to stdout.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_eprint_str(s: *const c_char) {
    ffi_entry!((), {
        let bytes = if s.is_null() {
            b"" as &[u8]
        } else {
            unsafe { crate::c_abi::gos_str_arg_bytes(s) }
        };
        unsafe { gos_rt_flush_stdout() };
        write_terminal(2, bytes);
    });
}

/// `eprint_str` followed by a newline. Mirrors `gos_rt_println`
/// for the stderr path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_eprintln() {
    ffi_entry!((), {
        write_terminal(2, b"\n");
    });
}
