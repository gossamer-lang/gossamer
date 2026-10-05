//! Stack-overflow guard.
//!
//! Recursive Gossamer code can blow the OS stack. Without a guard
//! the kernel delivers SIGSEGV on the bottom guard page and the
//! process dies silently - there is no chance to print a useful
//! diagnostic because the signal handler itself would run on the
//! already-exhausted stack and trigger a second fault.
//!
//! [`install_stack_guard`] fixes that on Unix by installing an
//! alternate signal stack via `sigaltstack(2)` and a
//! `SA_ONSTACK | SA_SIGINFO` SIGSEGV handler. The handler checks
//! the faulting address against the calling thread's known stack
//! bounds; if the fault is within a guard-page-sized neighbourhood
//! of the stack bottom it prints a structured stack-overflow
//! message via a raw `write(2)` syscall and calls
//! [`std::process::abort`]. Otherwise it restores the default
//! disposition and re-raises so the original SIGSEGV is observed
//! by the parent process / debugger.
//!
//! On Windows the same job is done by `SetThreadStackGuarantee` +
//! `SetUnhandledExceptionFilter`, which fires on
//! `EXCEPTION_STACK_OVERFLOW` from within a 64 KiB reserved tail
//! that the kernel keeps available for the filter to execute on.
//! Hard CPU faults (access violations) inside JIT-compiled code are
//! named by a first-chance vectored exception handler instead: JIT
//! code carries no Windows unwind metadata, so the SEH stack walk
//! fails before the unhandled filter is reached, but a vectored
//! handler runs before any dispatch and reports the faulting body,
//! fault address, and instruction pointer.
//!
//! Every entry point in this module is async-signal-safe: the
//! Unix handler touches only the `libc::write` syscall, `itoa` on
//! a stack-resident scratch buffer, and `std::process::abort`. No
//! allocations, no locks, no Rust-side I/O.

#![allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]

/// Bytes reserved for the alternate signal stack. `SIGSTKSZ` on
/// glibc is 8 KiB which is too tight once symbolication is added;
/// 64 KiB matches Go's `sigaltstack` size and leaves room for the
/// signal-safe write path even on architectures with deep ABI
/// frames.
pub const ALT_STACK_BYTES: usize = 64 * 1024;

/// Distance from the bottom of the thread's stack within which a
/// fault is attributed to stack overflow. Linux's default guard is
/// one page (4 KiB); we widen to 16 KiB to absorb compiler-emitted
/// red-zone manipulation and to be robust against architectures
/// with larger pages.
pub const STACK_GUARD_PROXIMITY: usize = 16 * 1024;

/// How far *above* the recorded low bound a fault still counts as an
/// overflow. The main-thread bound derived from `RLIMIT_STACK` can sit a
/// few pages below where the kernel actually stops the growable stack, so
/// the faulting access lands just inside `[lo, hi)` rather than below `lo`.
/// 64 KiB comfortably covers that slop while staying deep enough that a
/// genuine wild write elsewhere in the live stack is not misattributed.
pub const STACK_GUARD_UPPER_SLOP: usize = 64 * 1024;

/// Installs the per-thread stack-overflow guard. Idempotent on a
/// single thread; the scheduler calls this once at the start of
/// every worker. The main thread should also call it from program
/// entry.
pub fn install_stack_guard() {
    // Miri models no signal delivery or guard pages and cannot execute the
    // `sigaltstack` / `sigaction` / `pthread_getattr_np` foreign calls the
    // guard installs. The guard only converts a native stack-overflow
    // SIGSEGV into a clean diagnostic, which is inert under the interpreter,
    // so installing it there is both impossible and pointless.
    if cfg!(miri) {
        return;
    }

    #[cfg(unix)]
    unix::install();

    #[cfg(windows)]
    windows::install();

    #[cfg(not(any(unix, windows)))]
    {
        // Other targets (notably wasm32) cannot deliver SIGSEGV;
        // stack exhaustion there manifests as a trap that the host
        // surfaces without our help.
    }
}

use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

/// Name pointer + length of the in-process-JIT-compiled body currently
/// executing on this thread's call stack, or null when control is not
/// inside one. A hard fault (Windows access violation / a non-stack-
/// overflow `SIGSEGV`) inside JIT-emitted machine code carries no Rust
/// frame, so the OS reports only an opaque exit code; the fault handlers
/// below read this breadcrumb to name the body that was running. Set and
/// cleared by the JIT dispatch trampoline ([`set_jit_breadcrumb`] /
/// [`clear_jit_breadcrumb`]).
static JIT_BODY_PTR: AtomicPtr<u8> = AtomicPtr::new(std::ptr::null_mut());
static JIT_BODY_LEN: AtomicUsize = AtomicUsize::new(0);

/// Records the JIT-compiled body whose native code (and result
/// marshalling) is about to run. `name` must stay live for the dispatch
/// window - the JIT artifact owns it for the process. Pair every call
/// with [`clear_jit_breadcrumb`] on the return path.
pub fn set_jit_breadcrumb(name: &str) {
    JIT_BODY_LEN.store(name.len(), Ordering::Relaxed);
    JIT_BODY_PTR.store(name.as_ptr().cast_mut(), Ordering::Release);
}

/// Clears the breadcrumb set by [`set_jit_breadcrumb`] once control
/// returns from the JIT body to interpreter code.
pub fn clear_jit_breadcrumb() {
    JIT_BODY_PTR.store(std::ptr::null_mut(), Ordering::Release);
}

/// Bytes between the caller's stack pointer and the low end of the calling
/// thread's stack, or `None` when the platform cannot report the bounds. The
/// byte-budget recursion guard sizes itself from this when the VM runs on the
/// process main thread, whose stack is whatever the OS and the `gos` binary
/// reserve rather than a size the toolchain chose.
#[must_use]
pub fn remaining_stack_bytes() -> Option<usize> {
    gossamer_coro::current_stack_ptr().checked_sub(stack_low_limit()?)
}

/// The lowest address the calling thread's stack can grow to.
fn stack_low_limit() -> Option<usize> {
    #[cfg(unix)]
    {
        let (lo, hi) = unix::thread_stack_bounds();
        (hi > lo).then_some(lo)
    }
    #[cfg(windows)]
    {
        windows::thread_stack_low_limit()
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// `(lo, hi)` byte bounds of the calling OS thread's stack, or `None`
/// when the platform cannot report them. A frame-pointer walk checks
/// every link against this window before dereferencing it.
#[must_use]
pub fn current_thread_stack_bounds() -> Option<(usize, usize)> {
    #[cfg(unix)]
    {
        let (lo, hi) = unix::thread_stack_bounds();
        (hi > lo).then_some((lo, hi))
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Composes the fault note into `scratch`, returning its byte length.
/// Always produces a line on a non-stack-overflow fault: it names the
/// JIT-compiled body that was running, or states the fault was outside
/// any JIT body (so an empty stderr means the handler never fired, not
/// that the breadcrumb was simply unset). Signal-safe: two atomic loads
/// and bounded `copy_from_slice`s, no allocation or locks.
#[cfg(not(target_arch = "wasm32"))]
fn compose_jit_breadcrumb(scratch: &mut [u8]) -> usize {
    let ptr = JIT_BODY_PTR.load(Ordering::Acquire);
    if ptr.is_null() {
        return guard_copy(
            scratch,
            b"gossamer: hard fault (not a stack overflow) outside any JIT-compiled body\n",
        );
    }
    let name_len = JIT_BODY_LEN.load(Ordering::Relaxed).min(160);
    let prefix = b"gossamer: fault inside JIT-compiled body '";
    let suffix =
        b"'; isolate with GOS_JIT_ONLY=<fn> / GOS_JIT_SKIP=<fn>, or GOS_JIT=0 to disable\n";
    let mut len = 0;
    len += guard_copy(&mut scratch[len..], prefix);
    // SAFETY: `ptr`/`name_len` describe a live `&str` owned by the JIT
    // artifact (alive for the process); at most 160 bytes are read.
    let name = unsafe { std::slice::from_raw_parts(ptr, name_len) };
    len += guard_copy(&mut scratch[len..], name);
    len += guard_copy(&mut scratch[len..], suffix);
    len
}

#[cfg(not(target_arch = "wasm32"))]
fn guard_copy(dst: &mut [u8], src: &[u8]) -> usize {
    let n = src.len().min(dst.len());
    dst[..n].copy_from_slice(&src[..n]);
    n
}

/// The one text every tier reports when recursion runs past the stack. The
/// bytecode tier appends its own frame count; the platform guards below write
/// exactly this, from a signal context where formatting is not available.
#[cfg(any(unix, windows))]
pub(crate) const STACK_OVERFLOW_MESSAGE: &[u8] =
    b"error[GX0008]: stack overflow - recursion exceeded the available stack\n";

#[cfg(unix)]
mod unix {
    use std::cell::Cell;
    use std::mem::MaybeUninit;
    use std::sync::Once;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::{ALT_STACK_BYTES, STACK_GUARD_PROXIMITY, STACK_GUARD_UPPER_SLOP};

    // Per-thread alternate-signal stack. Kept thread-local so the
    // kernel always has a valid `ss_sp` for the calling thread -
    // `sigaltstack` is a per-thread setting, not process-wide.
    thread_local! {
        static ALT_STACK: Cell<*mut u8> = const { Cell::new(std::ptr::null_mut()) };
        static STACK_LO: Cell<usize> = const { Cell::new(0) };
        static STACK_HI: Cell<usize> = const { Cell::new(0) };
        static INSTALLED: Cell<bool> = const { Cell::new(false) };
    }

    // The `sigaction(SIGSEGV, ...)` installation is process-wide;
    // we only want to do it once. `sigaltstack` separately needs
    // running on every thread.
    static ACTION_INSTALLED: AtomicBool = AtomicBool::new(false);
    static ACTION_ONCE: Once = Once::new();

    pub(super) fn install() {
        // AddressSanitizer installs its own SIGSEGV handler + alt
        // signal stack to diagnose stack overflow itself. Installing
        // ours on top conflicts with ASan's runtime - when the test
        // thread exits, ASan reads the current alt-stack state via
        // `sigaltstack(NULL, &old)` and tries to `munmap` whatever
        // it sees. Our heap-allocated `Box<[u8]>` alt stack isn't
        // an mmap allocation, so the munmap aborts with the
        // "Failed to munmap" CHECK in libasan. Detect ASan via its
        // standard option-variable contract (any ASan-instrumented
        // program reads `ASAN_OPTIONS` at startup; CI sets it
        // explicitly) and skip - ASan's diagnostics are strictly
        // better than ours for this case.
        if std::env::var_os("ASAN_OPTIONS").is_some() {
            return;
        }
        INSTALLED.with(|cell| {
            if cell.get() {
                return;
            }
            install_alt_stack();
            install_sigaction();
            record_stack_bounds();
            cell.set(true);
        });
    }

    fn install_alt_stack() {
        // `Box::into_raw` returns a valid heap allocation
        // of `ALT_STACK_BYTES`. The pointer is parked thread-local
        // and never freed - the thread either exits (kernel reaps
        // the alt stack first via SS_DISABLE on thread exit) or
        // lives until process abort.
        let buf: Box<[u8]> = vec![0_u8; ALT_STACK_BYTES].into_boxed_slice();
        let raw = Box::into_raw(buf).cast::<u8>();
        ALT_STACK.with(|cell| cell.set(raw));

        let ss = libc::stack_t {
            ss_sp: raw.cast::<libc::c_void>(),
            ss_flags: 0,
            ss_size: ALT_STACK_BYTES,
        };
        // SAFETY: `ss` is a valid `stack_t` describing an owned
        // buffer; the second argument is null to discard the old
        // alternate stack (we never need to restore it).
        let _ = unsafe { libc::sigaltstack(&raw const ss, std::ptr::null_mut()) };
    }

    fn install_sigaction() {
        ACTION_ONCE.call_once(|| {
            // SAFETY: zero-init `sigaction` is a defined initial
            // state across every supported libc; we set the fields
            // we care about below.
            let mut action: libc::sigaction = unsafe { MaybeUninit::zeroed().assume_init() };
            action.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK | libc::SA_RESTART;
            action.sa_sigaction = sigsegv_handler as *const () as usize;
            // SAFETY: `action.sa_mask` is a field of the local `action`, a writable `sigset_t`.
            unsafe { libc::sigemptyset(&raw mut action.sa_mask) };
            // SAFETY: `action` is fully initialised; passing null
            // for the old action is the standard "don't care"
            // call.
            let rc =
                unsafe { libc::sigaction(libc::SIGSEGV, &raw const action, std::ptr::null_mut()) };
            if rc == 0 {
                ACTION_INSTALLED.store(true, Ordering::Release);
            }
        });
    }

    // Records [lo, hi) byte range of the calling thread's stack
    // into thread-local storage. Best effort: if discovery fails
    // the handler simply propagates the signal instead of
    // diagnosing overflow.
    fn record_stack_bounds() {
        let (lo, hi) = discover_stack_bounds();
        STACK_LO.with(|c| c.set(lo));
        STACK_HI.with(|c| c.set(hi));
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn discover_stack_bounds() -> (usize, usize) {
        // SAFETY: pthread_getattr_np / pthread_attr_getstack /
        // pthread_attr_destroy are documented async-signal-safe
        // wrappers over the current thread's stack metadata. The
        // attr is zero-initialised before the get call; the get
        // call populates sp/size; destroy releases attr resources.
        // All raw pointers we dereference (&attr, &sp, &size) are
        // stack-locals owned by this fn.
        unsafe {
            let mut attr: libc::pthread_attr_t = MaybeUninit::zeroed().assume_init();
            if libc::pthread_getattr_np(libc::pthread_self(), &raw mut attr) != 0 {
                return (0, 0);
            }
            let mut sp: *mut libc::c_void = std::ptr::null_mut();
            let mut size: libc::size_t = 0;
            let rc = libc::pthread_attr_getstack(&raw const attr, &raw mut sp, &raw mut size);
            libc::pthread_attr_destroy(&raw mut attr);
            if rc != 0 || sp.is_null() {
                return (0, 0);
            }
            let lo = sp as usize;
            let hi = lo + size;
            // The growable main-thread stack is a special case: some C
            // libraries (musl) report a small fixed window for it rather than
            // the region the kernel will actually let it grow into, which is
            // bounded by `RLIMIT_STACK` below the top. Trusting the small
            // reported size would place the guard-page proximity window far
            // above the real fault, so a genuine overflow reads as an
            // unrelated hard fault. When the reported size is implausibly
            // small for the main thread, derive the low bound from the
            // rlimit. Spawned threads (the VM / goroutine workers) get their
            // exact requested size from the same call and are left untouched.
            if is_main_thread()
                && let Some(rlimit) = stack_rlimit()
                && size < rlimit / 2
            {
                return (hi.saturating_sub(rlimit), hi);
            }
            (lo, hi)
        }
    }

    /// Stack `(lo, hi)` bounds of the calling thread, or `(0, 0)` when the
    /// platform cannot report them. Resolves to the active per-target
    /// `discover_stack_bounds`.
    pub(super) fn thread_stack_bounds() -> (usize, usize) {
        discover_stack_bounds()
    }

    /// Whether the calling thread is the process main thread. On Linux the
    /// main thread's TID equals the process PID.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn is_main_thread() -> bool {
        // SAFETY: `gettid` / `getpid` are async-signal-safe syscalls that
        // take no arguments and only read kernel-maintained ids.
        unsafe { libc::gettid() == libc::getpid() }
    }

    /// The soft `RLIMIT_STACK` in bytes, or `None` when it is unlimited or
    /// unreadable - in which case no rlimit-derived bound can be computed.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn stack_rlimit() -> Option<usize> {
        // SAFETY: `getrlimit` fills the caller-owned `rlim` with the current
        // limits; the struct is zero-initialised before the call.
        unsafe {
            let mut rlim: libc::rlimit = MaybeUninit::zeroed().assume_init();
            if libc::getrlimit(libc::RLIMIT_STACK, &raw mut rlim) != 0 {
                return None;
            }
            if rlim.rlim_cur == libc::RLIM_INFINITY || rlim.rlim_cur == 0 {
                return None;
            }
            usize::try_from(rlim.rlim_cur).ok()
        }
    }

    #[cfg(target_os = "macos")]
    fn discover_stack_bounds() -> (usize, usize) {
        // Darwin uses pthread_get_stackaddr_np / _get_stacksize_np.
        // `stackaddr` is the high end of the stack (grows down).
        // SAFETY: both calls are documented as safe for the
        // current thread; they read libpthread-internal state and
        // return scalars.
        unsafe {
            let me = libc::pthread_self();
            let hi = libc::pthread_get_stackaddr_np(me) as usize;
            let size = libc::pthread_get_stacksize_np(me) as usize;
            if hi == 0 || size == 0 {
                return (0, 0);
            }
            (hi.saturating_sub(size), hi)
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    fn discover_stack_bounds() -> (usize, usize) {
        // Other Unix (FreeBSD, illumos, etc.): no portable bounds
        // discovery. The handler still installs but every fault is
        // forwarded unmodified - equivalent to the pre-guard
        // behaviour but with the alt stack in place so the kernel
        // doesn't double-fault.
        (0, 0)
    }

    /// # Safety
    ///
    /// Called only by the kernel as the SIGSEGV handler: `info` is null or the
    /// `siginfo_t` it delivers.
    unsafe extern "C" fn sigsegv_handler(
        sig: libc::c_int,
        info: *mut libc::siginfo_t,
        _ctx: *mut libc::c_void,
    ) {
        // The handler runs on the alternate signal stack
        // (`SA_ONSTACK`). Only async-signal-safe operations are
        // permitted: raw syscalls, atomics, and stack-only state.
        let fault_addr = if info.is_null() {
            0
        } else {
            // SAFETY: kernel guarantees `info` is non-null when
            // SA_SIGINFO is set. `si_addr` is a populated field
            // across every supported libc.
            unsafe { (*info).si_addr() as usize }
        };
        if is_stack_overflow(fault_addr) {
            report_overflow_and_abort(fault_addr);
        }
        // Not a stack overflow. If the fault landed inside a JIT-compiled
        // body, name it on stderr before the default handler turns the
        // crash into an opaque exit code.
        let mut scratch = [0_u8; 256];
        let n = super::compose_jit_breadcrumb(&mut scratch);
        if n > 0 {
            // SAFETY: `write(2)` is async-signal-safe; `scratch` outlives
            // the call and `n` is within its bounds.
            let _ = unsafe {
                libc::write(
                    libc::STDERR_FILENO,
                    scratch.as_ptr().cast::<libc::c_void>(),
                    n,
                )
            };
        }
        // Restore the default disposition and re-raise so the original
        // SIGSEGV is observed.
        propagate_signal(sig);
    }

    fn is_stack_overflow(addr: usize) -> bool {
        if addr == 0 {
            return false;
        }
        let lo = STACK_LO.with(Cell::get);
        let hi = STACK_HI.with(Cell::get);
        if lo == 0 || hi == 0 || hi <= lo {
            return false;
        }
        // A fault within a proximity window of the stack's low bound is an
        // overflow; a fault deeper inside the live stack is an unrelated
        // wild write. The window is two-sided: below `lo` for the guard page
        // proper, and a wider band above it for the RLIMIT-derived
        // main-thread bound, which sits a few pages under the kernel's real
        // growable limit so the faulting access lands just inside `[lo, hi)`.
        let guard_bottom = lo.saturating_sub(STACK_GUARD_PROXIMITY);
        let guard_top = lo.saturating_add(STACK_GUARD_UPPER_SLOP);
        addr >= guard_bottom && addr < guard_top
    }

    fn report_overflow_and_abort(addr: usize) -> ! {
        // Compose the message on a stack scratch buffer. We can't
        // use `format!` or `eprintln!` - both allocate / take
        // locks. `itoa::Buffer` writes into a stack-resident
        // array.
        //
        // The text, the code, and the exit status match what the bytecode
        // VM reports for the same program, so a stack overflow reads the same
        // whichever tier ran it. The faulting address is deliberately left
        // out: it varies run to run, and a report that differs between two
        // runs of one binary is not one a test or a reader can rely on.
        let _ = addr;
        let mut scratch = [0_u8; 96];
        let mut len = 0;
        len += copy_into(&mut scratch[len..], super::STACK_OVERFLOW_MESSAGE);
        // SAFETY: `write(2)` is async-signal-safe. fd 2 is
        // stderr; `scratch` outlives the call.
        let _ = unsafe {
            libc::write(
                libc::STDERR_FILENO,
                scratch.as_ptr().cast::<libc::c_void>(),
                len,
            )
        };
        // Exit with the fault status the other tiers report rather than
        // aborting: a diagnosed overflow is not a crash, and a core dump
        // for it is noise. `_exit` is async-signal-safe; `std::process::exit`
        // would run atexit handlers from a signal handler.
        // SAFETY: `_exit(2)` is async-signal-safe and does not return.
        unsafe { libc::_exit(101) }
    }

    fn propagate_signal(sig: libc::c_int) {
        // Restore SIG_DFL and re-raise so the parent / debugger
        // sees the original signal.
        // SAFETY: zero-initialising a POSIX sigaction is valid; the
        // struct's only invariant is sa_mask being a well-formed
        // sigset, which sigemptyset establishes immediately after.
        let mut action: libc::sigaction = unsafe { MaybeUninit::zeroed().assume_init() };
        action.sa_sigaction = libc::SIG_DFL;
        // SAFETY: action is a stack-local sigaction we own; the
        // pointer is well-aligned and exclusive.
        unsafe { libc::sigemptyset(&raw mut action.sa_mask) };
        action.sa_flags = 0;
        // SAFETY: sigaction is async-signal-safe (POSIX guarantee);
        // action is a fully-initialised sigaction we own.
        unsafe {
            let _ = libc::sigaction(sig, &raw const action, std::ptr::null_mut());
            let _ = libc::raise(sig);
        }
    }

    fn copy_into(dst: &mut [u8], src: &[u8]) -> usize {
        let n = src.len().min(dst.len());
        dst[..n].copy_from_slice(&src[..n]);
        n
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn copy_truncates() {
            let mut buf = [0_u8; 3];
            let n = copy_into(&mut buf, b"hello");
            assert_eq!(n, 3);
            assert_eq!(&buf, b"hel");
        }

        #[test]
        fn proximity_check_inside_stack_is_not_overflow() {
            STACK_LO.with(|c| c.set(0x1_0000_0000));
            STACK_HI.with(|c| c.set(0x1_0010_0000));
            // Address inside the live stack - not an overflow.
            assert!(!is_stack_overflow(0x1_0005_0000));
            // Address just below the bottom - the overflow case.
            assert!(is_stack_overflow(0x0_FFFF_F000));
            // Address just above the bottom - the overflow case when the
            // low bound was derived from RLIMIT_STACK a few pages too low.
            assert!(is_stack_overflow(0x1_0000_2000));
            // Address far below the bottom - propagate.
            assert!(!is_stack_overflow(0x0_0000_1000));
            // Address above the top - propagate.
            assert!(!is_stack_overflow(0x1_0020_0000));
            STACK_LO.with(|c| c.set(0));
            STACK_HI.with(|c| c.set(0));
        }

        #[test]
        #[cfg_attr(miri, ignore)] // install() calls sigaltstack; Miri has no signals
        fn install_is_idempotent() {
            // Under AddressSanitizer `install` is a no-op (see the
            // doc on `install` for why our sigaltstack would
            // collide with libasan's). Idempotency still holds -
            // two no-ops are equivalent to one - but the
            // `INSTALLED` assertion below would spuriously fail, so
            // skip the body when ASan owns the signal stack.
            if std::env::var_os("ASAN_OPTIONS").is_some() {
                return;
            }
            install();
            install();
            assert!(INSTALLED.with(Cell::get));
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::sync::Once;
    use std::sync::atomic::{AtomicBool, Ordering};

    use windows_sys::Win32::Foundation::{
        EXCEPTION_ACCESS_VIOLATION, EXCEPTION_ILLEGAL_INSTRUCTION, EXCEPTION_IN_PAGE_ERROR,
        EXCEPTION_INT_DIVIDE_BY_ZERO, EXCEPTION_STACK_OVERFLOW,
    };
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS,
        SetUnhandledExceptionFilter,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentThreadStackLimits, SetThreadStackGuarantee,
    };

    /// The lowest address the calling thread's stack can grow to.
    pub(super) fn thread_stack_low_limit() -> Option<usize> {
        let mut low = 0usize;
        let mut high = 0usize;
        // SAFETY: both out-pointers address live locals the call fills.
        unsafe { GetCurrentThreadStackLimits(&raw mut low, &raw mut high) };
        (high > low).then_some(low)
    }

    /// 64 KiB reserved tail kept available so the unhandled
    /// exception filter can execute even when the user stack is
    /// almost exhausted.
    const STACK_GUARANTEE_BYTES: u32 = 64 * 1024;

    static FILTER_ONCE: Once = Once::new();
    static VEH_ONCE: Once = Once::new();
    /// Set once any handler has rendered the fault report, so the
    /// first-chance vectored handler and the last-resort unhandled
    /// filter never double-print the same crash.
    static FAULT_REPORTED: AtomicBool = AtomicBool::new(false);

    pub(super) fn install() {
        let mut guarantee = STACK_GUARANTEE_BYTES;
        // SAFETY: SetThreadStackGuarantee writes through the
        // pointer; `guarantee` is a stack-resident u32.
        unsafe {
            let _ = SetThreadStackGuarantee(&raw mut guarantee);
        }
        FILTER_ONCE.call_once(|| {
            // SAFETY: installing a process-wide filter pointer is
            // documented safe to call from any thread.
            unsafe {
                SetUnhandledExceptionFilter(Some(handler));
            }
        });
        VEH_ONCE.call_once(|| {
            // A vectored handler is invoked first-chance, before any
            // frame-based SEH dispatch and stack unwind. JIT-compiled
            // code carries no Windows unwind metadata (no
            // `RUNTIME_FUNCTION` registration), so a fault inside it
            // makes `RtlDispatchException`'s stack walk fail before the
            // unhandled-exception filter is ever reached - which is why
            // a hard fault in a JIT body otherwise terminates with an
            // opaque exit code and no diagnostic. The vectored handler
            // runs regardless of unwind data and cannot be displaced by
            // a later `SetUnhandledExceptionFilter` caller, so it is the
            // reliable place to name the faulting body.
            // SAFETY: registering a process-wide vectored handler is
            // documented safe from any thread; `first = 1` runs it
            // ahead of any previously registered vectored handler.
            unsafe {
                AddVectoredExceptionHandler(1, Some(vectored_handler));
            }
        });
    }

    /// First-chance vectored handler. Names the JIT body (and the fault
    /// code / address / instruction pointer) for a hard CPU fault, then
    /// lets the exception continue so the process still terminates with
    /// its original code. Stack overflow is deferred to the unhandled
    /// filter, which runs in the reserved tail; a vectored handler would
    /// execute on the already-exhausted stack.
    unsafe extern "system" fn vectored_handler(info: *mut EXCEPTION_POINTERS) -> i32 {
        if info.is_null() {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        // SAFETY: the kernel populates `ExceptionRecord` for a delivered
        // exception; null is guarded above.
        let record = unsafe { (*info).ExceptionRecord };
        if record.is_null() {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        // SAFETY: `record` is non-null (checked above), the exception record the OS passes for
        // the fault.
        let code = unsafe { (*record).ExceptionCode };
        // Only hard CPU faults are reported. Rust panics, C++ exceptions
        // and breakpoints are also first-chance exceptions but are
        // handled by the runtime, so naming them would be noise. Stack
        // overflow is left to the reserved-tail unhandled filter.
        let report = code == EXCEPTION_ACCESS_VIOLATION
            || code == EXCEPTION_ILLEGAL_INSTRUCTION
            || code == EXCEPTION_IN_PAGE_ERROR
            || code == EXCEPTION_INT_DIVIDE_BY_ZERO;
        if !report || code == EXCEPTION_STACK_OVERFLOW {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        // SAFETY: `record` is non-null (guarded); its fields are valid
        // for the lifetime of the exception. `ExceptionInformation[0]`
        // is the access kind and `[1]` the faulting address for an
        // access violation / in-page error (`NumberParameters >= 2`).
        let (acc, addr) = unsafe {
            if (*record).NumberParameters >= 2
                && (code == EXCEPTION_ACCESS_VIOLATION || code == EXCEPTION_IN_PAGE_ERROR)
            {
                (
                    Some((*record).ExceptionInformation[0]),
                    (*record).ExceptionInformation[1],
                )
            } else {
                (None, (*record).ExceptionAddress as usize)
            }
        };
        // SAFETY: `ContextRecord` is populated alongside `ExceptionRecord`;
        // `Rip` is the faulting instruction pointer on x86-64.
        let rip = unsafe {
            let ctx = (*info).ContextRecord;
            if ctx.is_null() {
                0
            } else {
                (*ctx).Rip as usize
            }
        };
        report_fault(code as u32, acc, addr, rip);
        EXCEPTION_CONTINUE_SEARCH
    }

    unsafe extern "system" fn handler(info: *const EXCEPTION_POINTERS) -> i32 {
        if info.is_null() {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        // SAFETY: the kernel guarantees `ExceptionRecord` is
        // populated when this filter fires.
        let record = unsafe { (*info).ExceptionRecord };
        if record.is_null() {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        // SAFETY: `record` is non-null (checked above), the exception record the OS passes for
        // the fault.
        let code = unsafe { (*record).ExceptionCode };
        if code != EXCEPTION_STACK_OVERFLOW {
            // A non-stack-overflow fault (most often an access violation).
            // The first-chance vectored handler normally names it already;
            // this is a fallback for the rare case the filter is reached
            // first. `FAULT_REPORTED` keeps the report single.
            if !FAULT_REPORTED.swap(true, Ordering::SeqCst) {
                let mut scratch = [0_u8; 256];
                let n = super::compose_jit_breadcrumb(&mut scratch);
                if n > 0 {
                    write_bytes(&scratch[..n]);
                }
            }
            return EXCEPTION_CONTINUE_SEARCH;
        }
        // Write a single short line via the C runtime's stderr
        // handle. Using `eprintln!` here is unsafe (panic crossing
        // an FFI boundary inside an SEH handler); the message
        // length is fixed so a raw WriteFile call is enough.
        write_message();
        // Exit with the fault status the other tiers report rather than
        // aborting: a diagnosed overflow is not a crash. `ExitProcess`
        // is the SEH-safe counterpart of `_exit`.
        // SAFETY: ExitProcess is callable from any thread and does not
        // return.
        unsafe { windows_sys::Win32::System::Threading::ExitProcess(101) }
    }

    /// Renders the fault report into a stack scratch buffer and writes
    /// it to stderr once. SEH-safe: no allocation or locks, just atomic
    /// loads, bounded copies, and a raw `WriteFile`.
    fn report_fault(code: u32, acc: Option<usize>, addr: usize, rip: usize) {
        if FAULT_REPORTED.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut scratch = [0_u8; 448];
        let mut n = 0;
        n += super::guard_copy(&mut scratch[n..], b"gossamer: hard fault code=0x");
        n += hex_into(&mut scratch[n..], code as usize);
        if let Some(a) = acc {
            let kind: &[u8] = match a {
                0 => b" read",
                1 => b" write",
                8 => b" exec",
                _ => b" access",
            };
            n += super::guard_copy(&mut scratch[n..], kind);
            n += super::guard_copy(&mut scratch[n..], b" addr=0x");
        } else {
            n += super::guard_copy(&mut scratch[n..], b" addr=0x");
        }
        n += hex_into(&mut scratch[n..], addr);
        n += super::guard_copy(&mut scratch[n..], b" rip=0x");
        n += hex_into(&mut scratch[n..], rip);
        let ptr = super::JIT_BODY_PTR.load(Ordering::Acquire);
        if ptr.is_null() {
            n += super::guard_copy(
                &mut scratch[n..],
                b"; fault outside any JIT-compiled body\n",
            );
        } else {
            n += super::guard_copy(&mut scratch[n..], b"; fault inside JIT-compiled body '");
            let name_len = super::JIT_BODY_LEN.load(Ordering::Relaxed).min(160);
            // SAFETY: `ptr`/`name_len` describe a live `&str` owned by the
            // JIT artifact (alive for the process); at most 160 bytes read.
            let name = unsafe { std::slice::from_raw_parts(ptr, name_len) };
            n += super::guard_copy(&mut scratch[n..], name);
            n += super::guard_copy(
                &mut scratch[n..],
                b"'; isolate with GOS_JIT_ONLY=<fn> / GOS_JIT_SKIP=<fn>, or GOS_JIT=0 to disable\n",
            );
        }
        write_bytes(&scratch[..n]);
    }

    /// Renders `value` as lowercase hex (no `0x`, no leading zeros) into
    /// `dst`, returning the bytes written. SEH-safe scratch-only formatter.
    fn hex_into(dst: &mut [u8], mut value: usize) -> usize {
        if value == 0 {
            if !dst.is_empty() {
                dst[0] = b'0';
                return 1;
            }
            return 0;
        }
        let mut tmp = [0_u8; 16];
        let mut idx = tmp.len();
        while value != 0 {
            idx -= 1;
            let nibble = (value & 0xF) as u8;
            tmp[idx] = if nibble < 10 {
                b'0' + nibble
            } else {
                b'a' + (nibble - 10)
            };
            value >>= 4;
        }
        super::guard_copy(dst, &tmp[idx..])
    }

    fn write_message() {
        const MSG: &[u8] = super::STACK_OVERFLOW_MESSAGE;
        write_bytes(MSG);
    }

    fn write_bytes(bytes: &[u8]) {
        use windows_sys::Win32::Storage::FileSystem::WriteFile;
        use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE};
        // SAFETY: GetStdHandle returns a process-owned handle.
        // WriteFile against an inheritable stderr handle is safe
        // from any thread, including SEH context.
        unsafe {
            let h = GetStdHandle(STD_ERROR_HANDLE);
            if !h.is_null() {
                let mut written: u32 = 0;
                let _ = WriteFile(
                    h,
                    bytes.as_ptr(),
                    bytes.len() as u32,
                    &raw mut written,
                    std::ptr::null_mut(),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg_attr(miri, ignore)] // install_stack_guard calls sigaltstack; Miri has no signals
    fn install_does_not_panic() {
        install_stack_guard();
        // Installing twice on the same thread is a no-op on Unix
        // and a redundant ThreadStackGuarantee call on Windows;
        // neither path should panic.
        install_stack_guard();
    }
}
